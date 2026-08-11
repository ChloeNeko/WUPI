//! The background state-delta schema engine.
//!
//! A dedicated `std::thread` ("wupi-schema") owning an ISOLATED
//! `LlamaContext<'static>` on `WUPI.gguf`. After each chat turn, `chat_send`
//! posts a [`SchemaRequest`] here; the thread generates a micro-delta JSON
//! (only the changed keys), parses it, and replies. The chat KV cache is
//! never touched: true context isolation.
//!
//! # Why a separate context (the load-bearing isolation requirement)
//!
//! The schema pass MUST NOT pollute the chat engine's rolling KV cache. A
//! second `LlamaContext` — borrowing the SHARED `&'static LlamaModel` (the
//! same weights the chat engine uses, via `shared_model()`) — achieves this:
//! independent KV state, no cross-contamination, with zero duplicate weight
//! allocation. Same isolation pattern as the embedder (§3B). The schema
//! engine shares weights but owns its own context (2026-07-23 dedupe: it
//! previously loaded its own 9.8GB copy, a redundant ~9.8GB VRAM cost).
//!
//! # The micro-delta contract
//!
//! Emits ONLY changed keys, not a full schema rewrite. A typical delta is
//! 20-100 tokens for sub-second generation. See `schema.rs` for the merge
//! semantics (`null` = delete key).
//!
//! # The fail-proof contract (3-pass + Rust validator + failure queue)
//!
//! Replaces the earlier "two-pass, drop on second fail" behavior. The new
//! invariant (locked 2026-07-20, §5): **no world-state evolution is ever
//! silently dropped.** Three layers, cheapest-first:
//!
//! 1. **Pure-Rust shape validator** (`schema_validator::validate`, ~0 cost).
//!    Enforces structural integrity (key/value length, no control chars,
//!    per-delta count caps). Defense by *structure*: a delta that fails
//!    validation gets fed its specific error back via the repair prompt so
//!    the model can correct the *issue*, not just regenerate blindly.
//! 2. **3-pass repair loop with accumulating context.** Initial generation →
//!    if parse OR validation fails, repair pass 1 (shows pass 1's raw output
//!    + the specific error) → repair pass 2 (shows BOTH prior errors + both
//!    raw outputs). Cap is 3 (empirically the LLM-JSON-repair cliff; passes
//!    4+ mostly produce different versions of the same failure). Worst case
//!    ~15-24s vs 35-56s for the rejected 7-pass proposal.
//! 3. **Failure queue (`failed_delta_queue` on AppState).** A delta that
//!    still fails all 3 passes is NOT dropped: it's queued. The next turn's
//!    delta prompt folds in the failed attempt as "previously deferred state
//!    change — re-attempt with new context." The new conversational context
//!    is a strictly better retry signal than re-running the same failed
//!    prompt. `SchemaReply::failed_attempt` carries the data the caller
//!    needs to enqueue.

use std::sync::mpsc;

use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use crate::llm::{shared_backend, shared_model};
use crate::schema::{SchemaDelta, WorldSchema};
use crate::schema_validator;

/// The schema context's token budget. Smaller than chat's 4000: the delta
/// pass only needs: system instruction (~150 tokens) + current schema JSON
/// (~200-800) + last exchange (~100-400) + generation room. 2048 is generous
/// headroom; the KV cost at Q8_0 is ~75MB.
const SCHEMA_CTX: u32 = crate::settings::CTX_SCHEMA;
const SCHEMA_BATCH: u32 = 512;
/// Cap on generated tokens for a delta pass. A compliant micro-delta is
/// 20-100 tokens; 256 is hard headroom before truncation forces the model to
/// stop rambling. If it hits this cap the output likely isn't valid JSON
/// anyway and the repair path or error path handles it.
const SCHEMA_MAX_TOKENS: i32 = 256;
/// Schema-engine sampler temperature. TASK-based, not source-based: schema
/// tracking is a NON-creative JSON task whether the API or the local model is
/// the active narrator. 0.2 strongly flattens the distribution so the model
/// commits to the structurally-correct token instead of a "creative"
/// alternative (which would corrupt the delta and force the 3-pass repair
/// loop to fire). The chat/narrator engines use their own NARRATIVE_TEMP
/// (0.85) for prose — same local model, different task, different temp.
/// Combined with top_p(0.9) + min_p(0.1) in the sampler chain, this keeps
/// JSON output near-deterministic without the pure-argmax collapse that
/// strict greedy produced (the prior `temp + greedy` config made temp a
/// no-op — greedy after temp scaling always picks the top-1 logit, so the
/// "0.2 lets it consider both" intent was defeated). See the chain
/// construction in `generate_with_repair`.
const SCHEMA_TEMP: f32 = crate::settings::TEMP_TRACKER;

/// Maximum number of generation passes per delta attempt (initial + 2
/// repairs = 3 total). The 4th-and-beyond cliff is empirically steep for
/// LLM-JSON-repair; the failure queue (fold-into-next-turn) is the strictly
/// better retry strategy past this cap. See module doc "The fail-proof
/// contract" layer 2.
const MAX_DELTA_PASSES: u8 = 3;

// ---------------------------------------------------------------------------
// Control plane: channel types
// ---------------------------------------------------------------------------

/// A request to the schema thread: diff `last_exchange` against
/// `current_schema` and emit the changed keys.
struct SchemaRequest {
    /// (user_message, assistant_message) from the turn that just completed.
    last_exchange: (String, String),
    /// The current schema serialized as pretty JSON, so the model knows what
    /// to diff against.
    current_schema_json: String,
    /// Deferred attempts from prior turns that the engine couldn't commit
    /// (all 3 passes failed). Folded into this turn's prompt as
    /// "previously deferred state changes — re-attempt with the new exchange
    /// as context." Empty in the common case (no prior failures).
    deferred_attempts: Vec<FailedAttempt>,
    /// Entity keys flagged immutable in the current schema (the `[CORE]`-style
    /// lock, 2026-07-27). Threaded through so the validator's repair loop can
    /// reject overwrite/delete of canon. Empty in the common case (no keys
    /// flagged immutable yet — the model isn't instructed to emit flags).
    immutable_keys: std::collections::HashSet<String>,
    /// The set of entity keys currently in the schema (i.e. the keys of
    /// `WorldSchema::entities`). Required for the immutability check to
    /// distinguish first-set (allowed) from overwrite (rejected).
    existing_keys: std::collections::HashSet<String>,
    /// One-shot reply channel.
    reply: mpsc::Sender<SchemaReply>,
}

/// What the schema thread sends back when a delta pass completes. Carries the
/// RAW model output alongside the parsed delta so callers (the debug IPC, and
/// Component D's queue) can see exactly what the model emitted: essential for
/// diagnosing JSON malformedness. On parse failure, `delta` is `None` and
/// `error` explains why. `raw_output` is always populated on a completed pass.
///
/// `failed_attempt` is `Some` ONLY when all 3 passes failed AND the failure
/// looks retryable (parse failures, validation failures). Generation errors
/// (tokenize/prefill/decode infrastructure failures) leave it `None` — those
/// aren't going to fix themselves on the next turn. The caller (lib.rs's
/// delta-fire spawn) pushes the `FailedAttempt` onto the failure queue; the
/// next turn's delta prompt folds it in. See module doc layer 3.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaReply {
    /// The verbatim model output (post generation). Empty only if generation
    /// itself failed before producing tokens.
    pub raw_output: String,
    /// The parsed delta, if JSON was valid AND passed validation. `None` on
    /// parse failure, validation failure, or generation error.
    pub delta: Option<SchemaDelta>,
    /// Human-readable error if the pass failed (tokenize/prefill/decode, or
    /// JSON parse failure after all passes, or validation failure after all
    /// passes). Empty string on success.
    pub error: String,
    /// Populated when all 3 passes failed AND the failure is retryable
    /// (parse/validation errors). The caller enqueues this; the next turn
    /// re-attempts with fresh conversational context. `None` on success, on
    /// infrastructure errors, or on generation panics (those don't benefit
    /// from a retry).
    #[serde(default)]
    pub failed_attempt: Option<FailedAttempt>,
}

/// A deferred delta: the schema engine's claim check for a turn's
/// world-state evolution that it couldn't commit. The caller (lib.rs) holds
/// these in `failed_delta_queue`; the next turn's `request_delta` /
/// `request_translation` call passes them in via `deferred_attempts` so the
/// prompt can fold them in ("previously deferred state change — re-attempt
/// with new context").
///
/// Carries the *triggering context*, not the failed model output: re-running
/// the same broken output through the model rarely helps. What helps is
/// giving the model a fresh generation pass with the *exchange* that
/// produced the broken delta, alongside the new turn's exchange. The model
/// gets two shots worth of conversational signal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedAttempt {
    /// The (user, assistant) exchange that produced the failed delta. Empty
    /// for translation attempts (which carry `trigger` instead).
    pub exchange: Option<(String, String)>,
    /// The player request that produced the failed translation. `None` for
    /// auto-summarizer attempts (which carry `exchange` instead).
    pub trigger: Option<String>,
    /// The accumulated errors from all 3 passes, joined. The next attempt's
    /// prompt can include this so the model knows what went wrong last time.
    pub errors: String,
    /// How many times this attempt has been retried (always 3 on first
    /// enqueue; the caller bumps it if a deferred re-attempt ALSO fails and
    /// re-enqueues). The queue caps total retries to avoid pathological
    /// loops — see lib.rs's `failed_delta_queue` cap.
    pub passes_used: u8,
}

/// Type alias distinguishing the three kinds of triggering context an attempt
/// carries. Internal to the engine; `FailedAttempt` exposes them as
/// `Option<(exchange)>` / `Option<request>` / `Option<interval>` for the IPC
/// boundary.
#[derive(Clone)]
enum AttemptSource {
    /// Auto-summarizer: triggered by a chat exchange.
    Auto { exchange: (String, String) },
    /// Game-manager translation: triggered by an explicit player request.
    Translation { request: String },
    /// World progression tick (Seam #4): triggered by in-world clock advance.
    /// Carries the elapsed interval hours so a failed tick can be re-attempted
    /// with the same magnitude on the next tick.
    WorldProgression { interval_hours: u32 },
}

/// The outcome of a delta-or-translation attempt. The engine's internal
/// return type; the message handler maps this to `SchemaReply` for the IPC
/// boundary. Distinguishes "committed cleanly" from "retryable failure" (the
/// carrier lets the caller enqueue for next turn).
enum AttemptOutcome {
    /// The delta parsed + validated cleanly. Ready to apply.
    Committed { raw_output: String, delta: SchemaDelta },
    /// All passes failed. `last_raw_output` is for the debug panel; `errors`
    /// is the joined accumulated diagnostics; `carrier` is what the caller
    /// pushes onto `failed_delta_queue` so the next turn re-attempts.
    Failed {
        last_raw_output: String,
        errors: String,
        carrier: FailedAttempt,
    },
}

enum SchemaMsg {
    Request(Box<SchemaRequest>),
    /// Translate a player's natural-language game-management request into a
    /// `SchemaDelta` (Phase E, 2026-07-18). Distinct from `Request` (the auto-
    /// summarizer's per-turn delta): the translation takes an explicit player
    /// command, not a just-finished chat exchange. Reuses the same JSON-delta
    /// parser + the schema engine's isolated context, no new infrastructure.
    RequestTranslation(Box<TranslationRequest>),
    /// Advance the off-screen world (Fable Seam #4, 2026-07-27). Fires when
    /// the in-world clock advances past the interval. Reuses the same engine
    /// + parser + validator; only the prompt differs. The returned delta is
    /// applied to `game_schema` by the caller.
    RequestWorldProgression(Box<WorldProgressionRequest>),
    /// Derive the starting clock + weather + opening location from a card's
    /// `.intro` text (2026-08-10). The cold-start anchor bootstrap for cards
    /// with no `<start>` block. Single raw generation (NOT a SchemaDelta — the
    /// reply's raw_output is parsed by the caller via
    /// `schema::BootstrapAnchors::from_model_output`). Bypasses the 3-pass
    /// repair loop entirely (the bootstrap is best-effort; a parse failure
    /// falls through to sensible defaults in the caller).
    RequestBootstrap(Box<BootstrapRequest>),
    /// Shut the schema thread down cleanly (drop its `LlamaContext`, freeing
    /// its KV cache, then join). Required by the VRAM-hibernate path so the
    /// schema context's ~75MB is reclaimable without process restart. Mirrors
    /// `ChatEngine::shutdown` / `FableEngine::shutdown`. (2026-07-23.)
    Shutdown,
}

/// A request to translate a player's natural-language request ("make it
/// stormy") into a `SchemaDelta` against the current game-world schema.
/// Carries the raw player text + the current schema JSON. The handler uses
/// `fable_command::render_translation_prompt` to build the LLM prompt, then
/// parses the reply via the same `SchemaDelta::from_model_output` the
/// auto-summarizer uses.
struct TranslationRequest {
    /// The player's verbatim request to Wupi (e.g. "make it stormy").
    player_request: String,
    /// The current game-world schema as pretty JSON (what to diff against).
    current_schema_json: String,
    /// Deferred translation attempts from prior player requests that the
    /// engine couldn't commit. Folded into this request's prompt so the
    /// model gets another shot with new context. Empty in the common case.
    deferred_attempts: Vec<FailedAttempt>,
    /// Immutable + existing key sets (same role as in `SchemaRequest`).
    immutable_keys: std::collections::HashSet<String>,
    existing_keys: std::collections::HashSet<String>,
    /// One-shot reply channel.
    reply: mpsc::Sender<SchemaReply>,
}

/// A request to advance the off-screen world (Fable Seam #4, 2026-07-27).
/// Fires when the in-world clock (`WorldSchema::world_clock`) advances past
/// the configured interval. Reuses the schema engine's isolated context +
/// the same 3-pass repair loop + the immutability validator — no new
/// infrastructure, just a different prompt. The result is a `SchemaDelta`
/// the caller applies to `game_schema`, so the next narrator turn sees a
/// world that has moved off-screen.
struct WorldProgressionRequest {
    /// The current game-world schema as pretty JSON. The model reads the
    /// entities to know what off-screen state exists.
    current_schema_json: String,
    /// The interval (in hours) that just elapsed. Surfaces to the model as
    /// "advance the off-screen state by ~N hours of activity."
    interval_hours: u32,
    /// Deferred progression attempts from prior ticks. Same fail-proof
    /// contract as the delta + translation paths.
    deferred_attempts: Vec<FailedAttempt>,
    /// Immutable + existing key sets (same role as in `SchemaRequest`).
    immutable_keys: std::collections::HashSet<String>,
    existing_keys: std::collections::HashSet<String>,
    /// One-shot reply channel.
    reply: mpsc::Sender<SchemaReply>,
}

/// A bootstrap request (2026-08-10): derive the starting clock + weather +
/// opening location from a card's `.intro`. The pre-built prompt is passed in
/// (the caller renders it via `fable_command::render_bootstrap_prompt`). The
/// reply carries ONLY `raw_output` — `delta` is always `None`, `error` is set
/// only on an infrastructure failure (tokenize/prefill/decode). The caller
/// parses the raw output via `schema::BootstrapAnchors::from_model_output`.
/// No deferred-attempts / immutability context: the bootstrap is a single-shot
/// best-effort extraction, NOT a 3-pass-repair SchemaDelta mutation.
struct BootstrapRequest {
    /// The fully-rendered bootstrap prompt (built by the caller).
    prompt: String,
    /// One-shot reply channel.
    reply: mpsc::Sender<SchemaReply>,
}

// ---------------------------------------------------------------------------
// Handle (held by callers; fully Send + Sync)
// ---------------------------------------------------------------------------

/// The handle callers hold. Fully `Send + Sync`: a channel sender to the
/// dedicated schema thread + the retained `JoinHandle` (needed for
/// `shutdown()`'s synchronous join — the VRAM-hibernate path). No
/// `LlamaContext` crosses out.
pub struct SchemaEngine {
    tx: mpsc::Sender<SchemaMsg>,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

// SAFETY: mpsc::Sender<SchemaMsg> is Send (SchemaMsg owns only Send data).
// Mutex<Option<JoinHandle<()>>> is Send+Sync. No `LlamaContext` crosses out.
unsafe impl Send for SchemaEngine {}
unsafe impl Sync for SchemaEngine {}

impl SchemaEngine {
    /// Spawn the schema thread. The chat backend MUST be loaded first: the
    /// schema engine borrows the leaked shared model (`shared_model()`) — it
    /// no longer loads its own copy (2026-07-23 schema-dedupe: the redundant
    /// second 9.8GB WUPI.gguf allocation is gone; chat+schema+fable now share
    /// ONE weight copy, matching AGENTS.md §2).
    ///
    /// Returns `Err` via the readiness receiver if `shared_model()` is `None`
    /// (model not loaded yet): callers should treat the schema engine as
    /// optional (chat proceeds without schema updates). The caller SHOULD
    /// `recv()` before treating the engine as ready, same contract as
    /// `ChatEngine::spawn` (Bug #6).
    pub fn spawn_load() -> (Self, mpsc::Receiver<Result<(), String>>) {
        let (tx, rx) = mpsc::channel::<SchemaMsg>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();

        let builder = std::thread::Builder::new().name("wupi-schema".into());
        let join = builder
            .spawn(move || {
                let mut runtime = match Self::init_runtime() {
                    Ok(rt) => {
                        let _ = init_tx.send(Ok(()));
                        rt
                    }
                    Err(e) => {
                        let msg = format!("schema engine init failed: {e}");
                        tracing::error!(error = %msg, "schema engine init failed; thread exiting");
                        let _ = init_tx.send(Err(msg.clone()));
                        Self::drain_failed(&rx, msg);
                        return;
                    }
                };
                tracing::info!("wupi-schema thread ready");

                loop {
                    // Both Request and RequestTranslation produce the same
                    // `(raw, Result<delta, err>)` outcome shape; we share the
                    // outcome → SchemaReply mapping (Phase E, 2026-07-18).
                    let parsed_msg = match rx.recv() {
                        Ok(SchemaMsg::Request(req)) => {
                            // Self-healing: isolate each delta pass so one
                            // panic doesn't kill the thread.
                            let outcome = std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| {
                                    runtime.generate_delta(&req)
                                }),
                            );
                            Some((outcome, req.reply))
                        }
                        Ok(SchemaMsg::RequestTranslation(req)) => {
                            // Phase E: same self-healing wrap, different runtime
                            // call. The translation prompt is built by
                            // `fable_command::render_translation_prompt`; the
                            // parser is the same `SchemaDelta::from_model_output`.
                            let outcome = std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| {
                                    runtime.generate_translation(&req)
                                }),
                            );
                            Some((outcome, req.reply))
                        }
                        Ok(SchemaMsg::RequestWorldProgression(req)) => {
                            // Seam #4 (2026-07-27): off-screen world simulation.
                            // Same self-healing wrap, the world-progression
                            // prompt is built by `render_world_progression_prompt`.
                            let outcome = std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| {
                                    runtime.generate_world_progression(&req)
                                }),
                            );
                            Some((outcome, req.reply))
                        }
                        Ok(SchemaMsg::RequestBootstrap(req)) => {
                            // Cold-start anchor bootstrap (2026-08-10). Single
                            // raw generation — no SchemaDelta parsing, no 3-pass
                            // repair. The reply carries raw_output for the
                            // caller to parse via BootstrapAnchors::from_model_
                            // output. Best-effort: a failure (panic or decode
                            // error) returns an empty raw_output + error string;
                            // the caller falls through to sensible defaults.
                            let outcome = std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| {
                                    runtime.generate_text(&req.prompt)
                                }),
                            );
                            // Map the raw-String outcome to the (outcome, reply)
                            // shape the shared reply-building block expects. We
                            // bypass the AttemptOutcome path (it's SchemaDelta-
                            // shaped) + build the SchemaReply inline.
                            let reply_msg = match outcome {
                                Ok(Ok(raw_output)) => SchemaReply {
                                    raw_output,
                                    delta: None,
                                    error: String::new(),
                                    failed_attempt: None,
                                },
                                Ok(Err(e)) => {
                                    tracing::warn!(error = %format!("{e:#}"), "bootstrap generation failed");
                                    runtime.ctx.clear_kv_cache();
                                    SchemaReply {
                                        raw_output: String::new(),
                                        delta: None,
                                        error: format!("{e:#}"),
                                        failed_attempt: None,
                                    }
                                }
                                Err(_) => {
                                    tracing::error!("bootstrap generation panicked; clearing KV");
                                    runtime.ctx.clear_kv_cache();
                                    SchemaReply {
                                        raw_output: String::new(),
                                        delta: None,
                                        error: "bootstrap generation panicked".to_string(),
                                        failed_attempt: None,
                                    }
                                }
                            };
                            // Best-effort reply: a closed channel just means the
                            // caller dropped the receiver (e.g. the game was
                            // cancelled mid-start). Logged, never fatal.
                            let _ = req.reply.send(reply_msg);
                            continue;
                        }
                        Err(mpsc::RecvError) => {
                            tracing::info!("wupi-schema: all senders dropped, exiting");
                            break;
                        }
                        Ok(SchemaMsg::Shutdown) => {
                            tracing::info!("wupi-schema shutting down");
                            break;
                        }
                    };
                    let Some((outcome, reply_tx)) = parsed_msg else { continue };
                    let reply_msg = match outcome {
                        // Generation succeeded; delta parsed + validated.
                        Ok(Ok(AttemptOutcome::Committed { raw_output, delta })) => SchemaReply {
                            raw_output,
                            delta: Some(delta),
                            error: String::new(),
                            failed_attempt: None,
                        },
                        // Generation succeeded but all 3 passes failed
                        // (parse/validation). Retryable: surface the carrier
                        // so the caller enqueues for next-turn re-attempt.
                        // The schema is unchanged for THIS turn.
                        Ok(Ok(AttemptOutcome::Failed { last_raw_output, errors, carrier })) => {
                            tracing::warn!(
                                error = %errors,
                                passes = carrier.passes_used,
                                "schema attempt failed all passes; queuing for re-attempt"
                            );
                            runtime.ctx.clear_kv_cache();
                            SchemaReply {
                                raw_output: last_raw_output,
                                delta: None,
                                error: errors,
                                failed_attempt: Some(carrier),
                            }
                        }
                        // Generation itself failed (tokenize/prefill/decode).
                        // Infrastructure failure: not retryable, no carrier.
                        Ok(Err(e)) => {
                            tracing::warn!(error = %format!("{e:#}"), "schema generation failed (infrastructure)");
                            runtime.ctx.clear_kv_cache();
                            SchemaReply {
                                raw_output: String::new(),
                                delta: None,
                                error: format!("{e:#}"),
                                failed_attempt: None,
                            }
                        }
                        // The catch_unwind caught a panic. Thread survives
                        // (KV cleared below); not retryable, no carrier.
                        Err(payload) => {
                            let msg = payload
                                .downcast_ref::<String>()
                                .map(|s| s.clone())
                                .or_else(|| {
                                    payload.downcast_ref::<&str>().map(|s| s.to_string())
                                })
                                .unwrap_or_else(|| {
                                    "schema delta panic (unknown cause)".to_string()
                                });
                            tracing::error!(panic = %msg, "schema delta panicked");
                            runtime.ctx.clear_kv_cache();
                            SchemaReply {
                                raw_output: String::new(),
                                delta: None,
                                error: format!("schema panic: {msg}"),
                                failed_attempt: None,
                            }
                        }
                    };
                    let _ = reply_tx.send(reply_msg);
                }
            })
            .expect("failed to spawn wupi-schema thread");

        (
            SchemaEngine {
                tx,
                join: std::sync::Mutex::new(Some(join)),
            },
            init_rx,
        )
    }

    /// Shut the schema thread down cleanly. Posts `Shutdown`, then joins the
    /// thread so the caller is guaranteed the `SchemaRuntime` (the
    /// `LlamaContext` + its KV cache) has been dropped — that's what frees the
    /// schema context's ~75MB on the VRAM-hibernate path. Idempotent across
    /// repeated calls (the `JoinHandle` is taken under the mutex). Mirrors
    /// `ChatEngine::shutdown` / `FableEngine::shutdown`. (2026-07-23.)
    pub fn shutdown(&self) {
        let _ = self.tx.send(SchemaMsg::Shutdown);
        if let Ok(mut guard) = self.join.lock() {
            if let Some(handle) = guard.take() {
                if let Err(e) = handle.join() {
                    tracing::warn!(error = ?e, "wupi-schema thread join failed during shutdown");
                }
            }
        }
    }

    /// Post a delta request. The caller awaits the reply via the receiver
    /// it created. Fire-and-forget is NOT the contract here: the caller
    /// (chat_send's queue) needs the result before proceeding.
    ///
    /// `deferred_attempts` carries failures from prior turns (folded into
    /// the prompt so the model gets another shot with fresh context). Pass
    /// an empty vec in the common case; the caller is responsible for
    /// draining the failure queue.
    ///
    /// `immutable_keys` + `existing_keys` thread the schema's `[CORE]`-style
    /// immutability set + its current entity keys into the validator. Pass
    /// empty sets in the common case (no keys locked yet). 2026-07-27.
    pub fn request_delta(
        &self,
        last_exchange: (String, String),
        current_schema: &WorldSchema,
        deferred_attempts: Vec<FailedAttempt>,
    ) -> anyhow::Result<mpsc::Receiver<SchemaReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<SchemaReply>();
        let req = SchemaRequest {
            last_exchange,
            current_schema_json: current_schema.to_json_pretty(),
            deferred_attempts,
            immutable_keys: current_schema.immutable_keys.clone(),
            existing_keys: current_schema.entities.keys().cloned().collect(),
            reply: reply_tx,
        };
        self.tx
            .send(SchemaMsg::Request(Box::new(req)))
            .map_err(|_| anyhow::anyhow!("schema engine thread closed"))?;
        Ok(reply_rx)
    }

    /// Post a TRANSLATION request (Phase E, 2026-07-18): translate a player's
    /// natural-language game-management request into a `SchemaDelta`. Used by
    /// `route_to_fable_manager` when Wupi intercepts a "make it stormy" /
    /// "give me a sword" / "travel to the dungeon" command. Same reply
    /// contract as `request_delta`: caller awaits via the returned receiver.
    ///
    /// `deferred_attempts` carries translation failures from prior player
    /// requests. Pass an empty vec in the common case.
    pub fn request_translation(
        &self,
        player_request: String,
        current_schema: &WorldSchema,
        deferred_attempts: Vec<FailedAttempt>,
    ) -> anyhow::Result<mpsc::Receiver<SchemaReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<SchemaReply>();
        let req = TranslationRequest {
            player_request,
            current_schema_json: current_schema.to_json_pretty(),
            deferred_attempts,
            immutable_keys: current_schema.immutable_keys.clone(),
            existing_keys: current_schema.entities.keys().cloned().collect(),
            reply: reply_tx,
        };
        self.tx
            .send(SchemaMsg::RequestTranslation(Box::new(req)))
            .map_err(|_| anyhow::anyhow!("schema engine thread closed"))?;
        Ok(reply_rx)
    }

    /// Post a WORLD PROGRESSION request (Fable Seam #4, 2026-07-27): advance
    /// the off-screen world by one interval of in-world time. Fires from
    /// `fable_send`'s tick gate when the clock crosses the configured
    /// interval. Reuses the schema engine's isolated context + the same
    /// fail-proof 3-pass contract; only the prompt differs. The returned
    /// delta is applied to `game_schema` by the caller, so the next narrator
    /// turn reflects the moved world.
    ///
    /// `interval_hours` surfaces to the model as the magnitude of time to
    /// advance ("~N hours of off-screen activity"). `deferred_attempts`
    /// carries failed progression attempts from prior ticks. Same shape +
    /// semantics as the delta/translation paths.
    pub fn request_world_progression(
        &self,
        current_schema: &WorldSchema,
        interval_hours: u32,
        deferred_attempts: Vec<FailedAttempt>,
    ) -> anyhow::Result<mpsc::Receiver<SchemaReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<SchemaReply>();
        let req = WorldProgressionRequest {
            current_schema_json: current_schema.to_json_pretty(),
            interval_hours,
            deferred_attempts,
            immutable_keys: current_schema.immutable_keys.clone(),
            existing_keys: current_schema.entities.keys().cloned().collect(),
            reply: reply_tx,
        };
        self.tx
            .send(SchemaMsg::RequestWorldProgression(Box::new(req)))
            .map_err(|_| anyhow::anyhow!("schema engine thread closed"))?;
        Ok(reply_rx)
    }

    /// Post a BOOTSTRAP request (2026-08-10): derive the starting clock +
    /// weather + opening location from a card's `.intro`. The caller passes a
    /// fully-rendered prompt (built via `fable_command::render_bootstrap_prompt`)
    /// + receives the raw model output on the returned receiver. The reply's
    /// `delta` is always `None` (the bootstrap is NOT a SchemaDelta — the
    /// caller parses the raw_output via `schema::BootstrapAnchors::from_model_
    /// output`); `error` is set only on an infrastructure failure. Single-shot
    /// best-effort: no 3-pass repair, no deferred-attempt queue. Fires once at
    /// `enter_fable_session` when the `<start>` block left an anchor dormant.
    pub fn request_bootstrap(
        &self,
        prompt: String,
    ) -> anyhow::Result<mpsc::Receiver<SchemaReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<SchemaReply>();
        let req = BootstrapRequest {
            prompt,
            reply: reply_tx,
        };
        self.tx
            .send(SchemaMsg::RequestBootstrap(Box::new(req)))
            .map_err(|_| anyhow::anyhow!("schema engine thread closed"))?;
        Ok(reply_rx)
    }

    fn drain_failed(rx: &mpsc::Receiver<SchemaMsg>, why: String) {
        while let Ok(msg) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
            // Both Request and RequestTranslation carry a reply sender that
            // needs an error response on init failure. The deferred_attempts
            // are dropped (the caller will re-enqueue them next turn from
            // its own failure queue — the schema thread's queue is separate
            // from the caller's AppState queue and is always empty between
            // turns).
            let reply_tx = match msg {
                SchemaMsg::Request(r) => r.reply,
                SchemaMsg::RequestTranslation(r) => r.reply,
                SchemaMsg::RequestWorldProgression(r) => r.reply,
                SchemaMsg::RequestBootstrap(r) => r.reply,
                // Shutdown during init-failure drain: nothing to reply to,
                // just drop it (the engine never came up).
                SchemaMsg::Shutdown => continue,
            };
            let _ = reply_tx.send(SchemaReply {
                raw_output: String::new(),
                delta: None,
                error: why.clone(),
                failed_attempt: None, // infrastructure failure, not retryable
            });
        }
    }

    /// Initialize the schema runtime: borrow the leaked shared chat model
    /// (`shared_model()`) + create an isolated context on it. The schema
    /// engine no longer loads its own copy (2026-07-23 schema-dedupe): the
    /// redundant second 9.8GB WUPI.gguf allocation is gone — chat+schema+
    /// fable now share ONE weight copy. The schema's isolated `LlamaContext`
    /// still provides the load-bearing KV isolation (schema deltas never
    /// pollute the chat cache); only the weights are shared.
    ///
    /// Returns `Err` if `shared_model()` is `None` (chat backend not loaded
    /// yet) — the caller treats the schema engine as optional in that case.
    /// Runs on the schema thread.
    fn init_runtime() -> anyhow::Result<SchemaRuntime> {
        let backend = shared_backend();
        let model_ref: &'static LlamaModel = shared_model().ok_or_else(|| {
            anyhow::anyhow!("schema engine: shared_model() is None (chat backend not loaded)")
        })?;
        tracing::info!("schema engine reuses shared chat model (VRAM-efficient, deduped)");

        // SCHEMA_CTX (2048) is fixed for both Local and API modes: under API
        // the schema engine only runs as a fallback / silent delta agent, and
        // 2048 is already the right size for delta work (system instruction +
        // current schema JSON + last exchange + generation room). See §5.
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(SCHEMA_CTX))
            .with_n_batch(SCHEMA_BATCH)
            .with_embeddings(false)
            // Match the chat engine's KV quantization for consistency.
            .with_type_k(KvCacheType::Q8_0)
            .with_type_v(KvCacheType::Q8_0);
        let ctx = model_ref
            .new_context(backend, ctx_params)
            .map_err(|e| anyhow::anyhow!("schema context init: {e:?}"))?;
        tracing::info!(n_ctx = SCHEMA_CTX, "schema context created (isolated, shared weights)");

        Ok(SchemaRuntime { ctx, model: model_ref })
    }
}

// ---------------------------------------------------------------------------
// Runtime (owned by the schema thread; never crosses thread boundaries)
// ---------------------------------------------------------------------------

struct SchemaRuntime {
    ctx: llama_cpp_2::context::LlamaContext<'static>,
    model: &'static LlamaModel,
}

impl SchemaRuntime {
    /// Generate a micro-delta for the given exchange + current schema.
    ///
    /// Fail-proof contract (see module doc): 3-pass max with accumulating
    /// repair context, validator between parse and success, failure queue
    /// carrier on the returned `AttemptOutcome::Failed` variant.
    fn generate_delta(
        &mut self,
        req: &SchemaRequest,
    ) -> Result<AttemptOutcome, anyhow::Error> {
        let initial_prompt = render_delta_prompt(
            &req.current_schema_json,
            &req.last_exchange,
            &req.deferred_attempts,
        );
        self.generate_with_repair(
            &initial_prompt,
            AttemptSource::Auto {
                exchange: req.last_exchange.clone(),
            },
            &req.deferred_attempts,
            &req.immutable_keys,
            &req.existing_keys,
            "schema delta",
        )
    }

    /// Translate a player's natural-language game-management request into a
    /// `SchemaDelta` (Phase E, 2026-07-18). Same fail-proof contract as
    /// `generate_delta`: 3-pass + validator + failure queue. The initial
    /// prompt is built by `fable_command::render_translation_prompt` from the
    /// player's verbatim text + the current game-world schema. Used by
    /// Wupi-as-game-manager when she intercepts "make it stormy" / "give me
    /// a sword" via chat_send.
    fn generate_translation(
        &mut self,
        req: &TranslationRequest,
    ) -> Result<AttemptOutcome, anyhow::Error> {
        let initial_prompt = crate::fable_command::render_translation_prompt(
            &req.player_request,
            &req.current_schema_json,
            &req.deferred_attempts,
        );
        self.generate_with_repair(
            &initial_prompt,
            AttemptSource::Translation {
                request: req.player_request.clone(),
            },
            &req.deferred_attempts,
            &req.immutable_keys,
            &req.existing_keys,
            "schema translation",
        )
    }

    /// Advance the off-screen world (Fable Seam #4, 2026-07-27). Same fail-
    /// proof contract as the delta + translation paths. The prompt asks the
    /// model to advance a subset of entities by `interval_hours` of off-screen
    /// activity, emitting only the changed keys. The result delta is applied
    /// to `game_schema` by the caller. Fires from `fable_send`'s tick gate.
    ///
    /// The `interval_hours` field surfaces to the model as the magnitude of
    /// time that elapsed; the model decides which entities meaningfully
    /// advanced in that window (a faction relocated, an NPC's mood shifted,
    /// a deadline approached). The validator's immutability check protects
    /// against the progression pass trying to retcon canon entities.
    fn generate_world_progression(
        &mut self,
        req: &WorldProgressionRequest,
    ) -> Result<AttemptOutcome, anyhow::Error> {
        let initial_prompt = render_world_progression_prompt(
            &req.current_schema_json,
            req.interval_hours,
            &req.deferred_attempts,
        );
        self.generate_with_repair(
            &initial_prompt,
            AttemptSource::WorldProgression {
                interval_hours: req.interval_hours,
            },
            &req.deferred_attempts,
            &req.immutable_keys,
            &req.existing_keys,
            "world progression",
        )
    }

    /// The shared 3-pass repair loop. Runs the model up to `MAX_DELTA_PASSES`
    /// times. Each pass parses the output via `SchemaDelta::from_model_output`
    /// AND validates it via `schema_validator::validate`. A pass succeeds only
    /// if both parse and validation succeed. Repair prompts accumulate prior
    /// errors + prior raw outputs so the model sees what it got wrong, not
    /// just a generic "try again."
    ///
    /// Returns `AttemptOutcome` (success / parse-or-validation failure /
    /// retryable-failure-with-carrier) so the message handler can build the
    /// right `SchemaReply` including the failure-queue carrier.
    ///
    /// `label` is a short diagnostic ("schema delta" / "schema translation")
    /// used in tracing. `source` carries the trigger context (exchange or
    /// player request) so a failed attempt can be re-attempted on the next
    /// turn. `prior_deferred` is the failures folded in from previous turns;
    /// it does NOT count toward this attempt's pass budget.
    fn generate_with_repair(
        &mut self,
        initial_prompt: &str,
        source: AttemptSource,
        prior_deferred: &[FailedAttempt],
        immutable_keys: &std::collections::HashSet<String>,
        existing_keys: &std::collections::HashSet<String>,
        label: &'static str,
    ) -> Result<AttemptOutcome, anyhow::Error> {
        // The validation context now carries the immutability + existing-key
        // sets (2026-07-27). When the schema has flagged any keys immutable,
        // the validator rejects overwrite/delete of those keys — the cheap
        // structural defense against NPC drift. Empty sets in the common case
        // (no keys locked yet) → check is a no-op.
        let validation_ctx = schema_validator::ValidationContext {
            known_nodes: None,
            immutable_keys: Some(immutable_keys),
            existing_keys: Some(existing_keys),
        };

        // Track every failure across all passes so we can (a) accumulate them
        // into the repair prompt and (b) carry them on the FailedAttempt if
        // all passes fail.
        let mut errors: Vec<String> = Vec::with_capacity(MAX_DELTA_PASSES as usize);
        let mut raw_outputs: Vec<String> = Vec::with_capacity(MAX_DELTA_PASSES as usize);
        let mut last_raw = String::new();

        for pass in 1..=MAX_DELTA_PASSES {
            let prompt: String = if pass == 1 {
                // First pass: the caller-built initial prompt (delta or
                // translation), already includes deferred-attempts context
                // if any.
                initial_prompt.to_string()
            } else {
                // Repair pass: shows the accumulated raw outputs + errors
                // from every prior pass. The model sees what it got wrong
                // and why, so it can correct the specific issue.
                render_accumulated_repair_prompt(&raw_outputs, &errors)
            };
            let raw = self.generate_text(&prompt)?;
            // Reasoning debug: strip the thought channel BEFORE storing the
            // raw output into raw_outputs / last_raw. extract_reply_channel
            // (the same gate from_model_output uses) drops the
            // `<|channel>thought ... <channel|>` body, so (a) the repair
            // prompt's re-quoted prior outputs never show the model its own
            // thought as if it were payload, and (b) the forensic raw_output
            // we return is the clean JSON. The thought itself is captured
            // separately for debug logging. No-op when the model didn't emit
            // a thought channel.
            let reply = crate::schema::extract_reply_channel(&raw);
            let reasoning = crate::chat_format::extract_reasoning_channel(&raw);
            if !reasoning.is_empty() {
                tracing::debug!(
                    label,
                    pass,
                    reasoning_len = reasoning.len(),
                    "{label} pass {pass} reasoning: {}",
                    reasoning.chars().take(600).collect::<String>()
                );
            }
            last_raw = reply.clone();
            raw_outputs.push(reply.clone());

            // Parse the JSON (channel-protocol + fence strip happens inside
            // from_model_output; on a reply already stripped this is a no-op).
            let parsed = SchemaDelta::from_model_output(&reply);
            let delta = match parsed {
                Ok(d) => d,
                Err(e) => {
                    let msg = format!("pass {pass} JSON parse: {e}");
                    tracing::warn!(
                        label,
                        pass,
                        error = %e,
                        raw_preview = %reply.chars().take(200).collect::<String>(),
                        "{label} parse failed"
                    );
                    errors.push(msg);
                    continue; // next pass
                }
            };

            // Validate structure. This is the §1B defense layer: catches
            // parseable-but-corrupt deltas (control chars, runaway length,
            // count-cap violations) at zero LLM cost.
            if let Err(vfail) = schema_validator::validate(&delta, &validation_ctx) {
                let msg = format!("pass {pass} validation: {vfail}");
                tracing::warn!(label, pass, failure = %vfail, "{label} validation failed");
                errors.push(msg);
                continue; // next pass — repair prompt will show the failure
            }

            // Success: parse OK + validation OK. Trace + return.
            tracing::debug!(
                label,
                pass,
                tokens = reply.len(),
                deferred = prior_deferred.len(),
                "{label} committed on pass {pass}"
            );
            return Ok(AttemptOutcome::Committed { raw_output: reply, delta });
        }

        // All passes exhausted. Build the failure-queue carrier so the caller
        // can enqueue this for re-attempt on the next turn. The carrier
        // carries the SOURCE (exchange, request, or progression interval) +
        // the accumulated errors; it does NOT carry the broken raw outputs
        // (re-running those through the model rarely helps; fresh context
        // does). For WorldProgression the trigger is a synthetic string
        // carrying the interval so the next tick's prompt can re-attempt at
        // the same magnitude.
        let (exchange_opt, trigger_opt) = match &source {
            AttemptSource::Auto { exchange } => (Some(exchange.clone()), None),
            AttemptSource::Translation { request } => (None, Some(request.clone())),
            AttemptSource::WorldProgression { interval_hours } => (
                None,
                Some(format!("world progression (~{interval_hours}h elapsed)")),
            ),
        };
        tracing::warn!(
            label,
            passes = MAX_DELTA_PASSES,
            errors = errors.join(" | "),
            "{label} failed all {MAX_DELTA_PASSES} passes; carrying for re-attempt"
        );
        Ok(AttemptOutcome::Failed {
            last_raw_output: last_raw,
            errors: errors.join(" | "),
            carrier: FailedAttempt {
                exchange: exchange_opt,
                trigger: trigger_opt,
                errors: errors.join(" | "),
                passes_used: MAX_DELTA_PASSES,
            },
        })
    }

    /// Tokenize → prefill → sample-and-decode a single response. One-shot
    /// generation with a max-tokens cap and near-greedy probabilistic
    /// sampling (the delta is deterministic JSON; no creativity needed —
    /// see the sampler-chain comment below for the temp/top_p/min_p config).
    /// Returns the decoded text.
    ///
    /// The context is fully reset each call (clear_kv_cache + re-prefill from
    /// zero). Unlike the chat engine, there's no delta-prefill optimization
    /// here: each prompt is a different schema + exchange, and the prompt is
    /// small (~1-2KB), so a full prefill each call is cheap and correct.
    ///
    /// The sample/detokenize/batch pattern mirrors `engine.rs::decode_loop`
    /// exactly: `sample(&ctx, -1)` reads from the last logits position,
    /// `accept` advances sampler state, `token_to_piece` with an encoding_rs
    /// decoder handles multibyte boundaries, and the sampled token is fed
    /// back at position `n_cur - 1`.
    fn generate_text(&mut self, prompt: &str) -> anyhow::Result<String> {
        let mut tokens = self
            .model
            .str_to_token(prompt, AddBos::Always)
            .map_err(|e| anyhow::anyhow!("schema tokenize: {e:?}"))?;
        if tokens.is_empty() {
            anyhow::bail!("schema tokenized prompt is empty");
        }
        // Guard: if the prompt alone exceeds the context, truncate from the
        // FRONT (keep the generation prompt + last exchange). Losing the
        // oldest schema detail beats failing entirely.
        let max_prompt = (SCHEMA_CTX as usize).saturating_sub(SCHEMA_MAX_TOKENS as usize);
        if tokens.len() > max_prompt {
            let drop = tokens.len() - max_prompt;
            tokens.drain(0..drop);
            tracing::warn!(dropped = drop, "schema prompt exceeded context; truncated from front");
        }

        // Fresh cache each call: the schema context is one-shot, no reuse.
        self.ctx.clear_kv_cache();

        // Prefill in batches (mirrors engine.rs::prefill).
        let n_prompt = tokens.len() as i32;
        let mut batch = LlamaBatch::new(SCHEMA_BATCH as usize, 1);
        let mut consumed = 0usize;
        while consumed < tokens.len() {
            let take = std::cmp::min(SCHEMA_BATCH as usize, tokens.len() - consumed);
            let is_last_chunk = consumed + take == tokens.len();
            batch.clear();
            for (i, tok) in tokens[consumed..consumed + take].iter().enumerate() {
                let is_final = is_last_chunk && i == take - 1;
                batch
                    .add(*tok, (consumed + i) as i32, &[0], is_final)
                    .map_err(|e| anyhow::anyhow!("schema batch add: {e:?}"))?;
            }
            self.ctx
                .decode(&mut batch)
                .map_err(|e| anyhow::anyhow!("schema prefill decode: {e:?}"))?;
            consumed += take;
        }

        // Sample-and-decode loop. Near-greedy probabilistic sampling.
        //
        // Sampler chain (2026-07-28): temp(SCHEMA_TEMP=0.2) + top_p(0.9) +
        // min_p(0.1) + dist(0). The temp/top_p/min_p trio filters the
        // distribution tightly (low temp + tight top_p + min_p floor) so
        // the JSON output stays near-deterministic; `dist` does the final
        // multinomial sample from what remains.
        //
        // HISTORY: previously ended in `greedy()` (pure argmax after temp
        // scaling, making temp a no-op). Replaced with `dist(0)` 2026-07-28
        // — the correct probabilistic terminal sampler. NOT bare: leaving
        // the chain without a terminal sampler triggers
        // `GGML_ASSERT(cur_p.selected >= 0)` in llama-sampler.cpp on the
        // first decode (the chain's `selected` stays at -1).
        //
        // Schema deltas are strict JSON — a NON-creative task — so we want
        // near-deterministic token selection: the model commits to the
        // structurally-correct token instead of a "creative" alternative
        // (which corrupts the delta and forces the 3-pass repair loop).
        // SCHEMA_TEMP is TASK-based: schema tracking is non-creative JSON
        // whether the API or local model is the active narrator. The chat
        // + fable engines use temp 0.85 + top_p 0.95 + min_p 0.1 for prose
        // (same chain shape, looser values for creative narrative).
        //
        // No ThoughtGate/StreamFilter here (output is JSON, not the Gemma4
        // channel protocol). n_cur = next position to decode.
        let mut sampler = LlamaSampler::chain_simple([
            LlamaSampler::temp(SCHEMA_TEMP),
            LlamaSampler::top_p(crate::settings::TOP_P_TRACKER, 1),
            LlamaSampler::min_p(crate::settings::MIN_P, 1),
            LlamaSampler::dist(0),
        ]);
        let eos = self.model.token_eos();
        let mut n_cur = n_prompt;
        let mut step_batch = LlamaBatch::new(1, 1);
        let mut out = String::new();

        for _ in 0..SCHEMA_MAX_TOKENS {
            // sample(&ctx, -1) reads logits from the last decoded position.
            let new_token: LlamaToken = sampler.sample(&self.ctx, -1);
            sampler.accept(new_token);

            if self.model.is_eog_token(new_token) || new_token == eos {
                break;
            }

            // Detokenize with an encoding_rs decoder for multibyte safety
            // (mirrors engine.rs:750-754).
            let mut decoder = encoding_rs::UTF_8.new_decoder();
            let piece = self
                .model
                .token_to_piece(new_token, &mut decoder, true, None)
                .map_err(|e| anyhow::anyhow!("schema token to piece: {e:?}"))?;
            if !piece.is_empty() {
                out.push_str(&piece);
            }

            // Feed the sampled token back at position n_cur (one past the
            // last prefilled/decoded), then decode to produce the next
            // position's logits. Mirrors engine.rs:770-776.
            step_batch.clear();
            step_batch
                .add(new_token, n_cur, &[0], true)
                .map_err(|e| anyhow::anyhow!("schema decode batch: {e:?}"))?;
            self.ctx
                .decode(&mut step_batch)
                .map_err(|e| anyhow::anyhow!("schema decode: {e:?}"))?;
            n_cur += 1;
        }

        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering (Component C)
// ---------------------------------------------------------------------------

/// Render the schema-delta generation prompt. Uses the Gemma4 turn markers so
/// the model sees familiar structure, but the content is schema-specific.
/// NOT routed through `ChatFormat::render_prompt`: this is a dedicated
/// renderer (the schema pass isn't a chat turn).
///
/// `deferred_attempts` carries failures from prior turns (fail-proof contract
/// §5 layer 3). Folded in as "previously deferred state changes — re-attempt
/// with this turn's exchange as anchor." Empty in the common case.
fn render_delta_prompt(
    current_schema_json: &str,
    last_exchange: &(String, String),
    deferred_attempts: &[FailedAttempt],
) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("<|turn>system\n");
    out.push_str(DELTA_SYSTEM_INSTRUCTION);
    // Always-on thinking: inject the Gemma4 `<|think|>` control token so the
    // delta pass always reasons before the JSON. The thought body is stripped
    // before parsing by SchemaDelta::from_model_output → extract_reply_channel
    // (the single load-bearing gate; see schema.rs).
    // DISABLED 2026-08-09 (`THINKING_ENABLED`) — see settings.rs.
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Current schema:\n");
    out.push_str(current_schema_json);
    out.push_str("\n\nLast exchange:\n[user]: ");
    out.push_str(&last_exchange.0);
    out.push_str("\n[model]: ");
    out.push_str(&last_exchange.1);
    // Deferred re-attempt context. When the previous turn's delta failed all
    // 3 passes, fold its triggering exchange + accumulated errors in here so
    // the model gets another shot with the new exchange as anchor.
    if !deferred_attempts.is_empty() {
        out.push_str(
            "\n\n[Previously deferred state changes — re-attempt with the above exchange as the primary context:]\n",
        );
        for (i, attempt) in deferred_attempts.iter().enumerate() {
            let (u, a) = attempt
                .exchange
                .clone()
                .unwrap_or(("".to_string(), "".to_string()));
            out.push_str(&format!(
                "  {}. prior [user]: {:?}\n      prior [model]: {:?}\n      prior errors: {}\n",
                i + 1,
                u.chars().take(200).collect::<String>(),
                a.chars().take(200).collect::<String>(),
                attempt.errors
            ));
        }
    }
    out.push_str("\n<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// Render the World Progression prompt (Fable Seam #4, 2026-07-27). Fires
/// when the in-world clock advances past the configured interval — the model
/// is asked to advance a subset of the off-screen entities by ~N hours of
/// activity, emitting only the changed keys as a delta.
///
/// The prompt deliberately does NOT show recent narrator output (unlike the
/// delta pass): World Progression is OFF-SCREEN simulation, decoupled from
/// the player's immediate bubble. The model reads the current entities +
/// advances whichever ones would plausibly move in that window (a faction
/// relocates, an NPC's mood shifts, a deadline approaches). The result delta
/// is applied to `game_schema`; the next narrator turn reflects the moved
/// world via the existing `<world_state>` injection.
///
/// The fail-proof contract applies: `deferred_attempts` from prior failed
/// ticks fold in so the model gets a fresh shot at the same interval. The
/// validator's immutability check protects against the progression pass
/// trying to retcon canon entities (locked identity keys).
fn render_world_progression_prompt(
    current_schema_json: &str,
    interval_hours: u32,
    deferred_attempts: &[FailedAttempt],
) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("<|turn>system\n");
    out.push_str(WORLD_PROGRESSION_SYSTEM_INSTRUCTION);
    // Always-on thinking (delta prompt above documents the strip pipeline).
    // DISABLED 2026-08-09 (`THINKING_ENABLED`) — see settings.rs.
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Current world state:\n");
    out.push_str(current_schema_json);
    out.push_str(&format!(
        "\n\nApproximately {interval_hours} hours of in-world time have elapsed off-screen. \
         Advance the simulation: pick a SUBSET of entities that would plausibly have moved \
         in that window (a faction relocates, an NPC's mood shifts, a deadline approaches, \
         a rumor spreads, a rival makes a move) and emit ONLY their changed keys."
    ));
    if !deferred_attempts.is_empty() {
        out.push_str(
            "\n\n[Previously deferred progression attempts — re-attempt with the above as context:]\n",
        );
        for (i, attempt) in deferred_attempts.iter().enumerate() {
            let trigger = attempt
                .trigger
                .as_deref()
                .or_else(|| attempt.exchange.as_ref().map(|(u, _)| u.as_str()))
                .unwrap_or("(no trigger recorded)");
            out.push_str(&format!(
                "  {}. prior: {:?}\n      prior errors: {}\n",
                i + 1,
                trigger.chars().take(200).collect::<String>(),
                attempt.errors
            ));
        }
    }
    out.push_str("\n<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// Accumulating repair prompt. Shows the model EVERY prior pass's raw output
/// + every prior error, so it can correct the *specific* issue rather than
/// regenerate blindly. This is the §1B-aligned repair: structured feedback
/// at the cost of one LLM pass, not 7 blind retries.
///
/// The accumulated-context shape is load-bearing: pass 3 sees both pass 1
/// and pass 2's outputs + errors, giving the model maximum signal on its
/// final attempt before the failure queue takes over.
fn render_accumulated_repair_prompt(prior_raw: &[String], prior_errors: &[String]) -> String {
    let mut out = String::with_capacity(1024 + prior_raw.len() * 256);
    out.push_str("<|turn>system\n");
    out.push_str(
        "Your previous output(s) were invalid. Emit ONLY the JSON delta object: no prose, no markdown fences, no commentary. Address EACH error below. If nothing actually changed, emit {}.",
    );
    // Always-on thinking (delta prompt above documents the strip pipeline).
    // DISABLED 2026-08-09 (`THINKING_ENABLED`) — see settings.rs.
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str(&format!("{} prior attempt(s) failed:\n", prior_raw.len()));
    for (i, raw) in prior_raw.iter().enumerate() {
        let err = prior_errors.get(i).map(|s| s.as_str()).unwrap_or("(no error recorded)");
        out.push_str(&format!(
            "\n--- Attempt {} ---\nError: {}\nYour output was:\n{}\n",
            i + 1,
            err,
            raw.chars().take(500).collect::<String>(),
        ));
    }
    out.push_str("\n---\nNow emit the corrected JSON delta:\n<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// Cheap content gate for whether the schema delta pass should fire this turn.
///
/// The delta pass is a FULL 12B forward pass (tokenize + prefill + greedy
/// decode up to 256 tokens). Firing it unconditionally on every turn -
/// including "ok", "thanks", "lol", "yes": burns ~1-4s of dedicated GPU time
/// for a turn that changed nothing in the world. This gate skips those.
///
/// **Conservative by design.** The cost of a false skip (missing a real world-
/// state change) is far higher than the cost of a false fire (one wasted pass),
/// so the bar to skip is HIGH: only short, clearly-non-substantive user turns
/// with a short assistant reply. Anything ambiguous fires the pass.
///
/// # What skips
///
/// - User message ≤ 4 words AND ≤ 32 chars (covers "ok", "thanks", "lol",
///   "yes", "no", "sure", "k", "yep", "continue", "ok cool", etc.).
/// - No assistant content (empty/error reply: nothing to record).
///
/// # What does NOT skip (deliberately)
///
/// - Short roleplay actions ("I nod", "I draw": 2 words but world-moving).
///   These are 2 words but contain a verb in first person, so the word-count
///   gate alone is wrong for them. We can't distinguish "I nod" from "ok"
///   cheaply without a model call, so we FIRE on anything that looks like it
///   could be an action. The signal we use: presence of a pronoun ("i", "you",
///   "he", "she", "they", "we") or a verb-shape. Cheaper and safer to just
///   fire on anything that isn't obviously filler.
/// - Long assistant replies (a meaty reply likely reflects a meaty exchange).
///
/// Pure + allocation-light so it's testable in isolation (Prime Directive §3A:
/// retrieval/control logic stays decoupled from the model backend).
pub fn should_fire_delta(user_text: &str, assistant_text: &str) -> bool {
    // No assistant content → nothing to record (error turn, empty reply).
    if assistant_text.trim().is_empty() {
        return false;
    }
    let user = user_text.trim();
    // Word count via split_whitespace (handles runs of spaces/tabs/newlines).
    let word_count = user.split_whitespace().count();
    // Short AND compact → almost certainly filler. The char ceiling catches
    // 4 "words" that are actually one long token blob; the word ceiling catches
    // long rambling filler. Both must hold to skip.
    if word_count <= 4 && user.len() <= 32 {
        // Final guard: if the short message contains a first/second-person
        // pronoun, it might be a roleplay action ("I nod", "you see"). Fire
        // rather than risk skipping world state. Pronoun check is case-
        // insensitive on a small set; cheaper than a verb lookup.
        let lower = user.to_lowercase();
        const PRONOUNS: &[&str] = &[
            "i ", "i'", "i’m", "i'm", "i’ll", "i'll", "i’ve", "i've",
            "you ", "you'", "u ", "he ", "she ", "they ", "we ",
        ];
        let looks_like_action = PRONOUNS.iter().any(|p| lower.starts_with(p));
        if looks_like_action {
            return true; // ambiguous: fire to be safe
        }
        return false; // short, compact, no pronoun → filler, skip
    }
    // Everything else: fire. Long or substantive exchanges always get a pass.
    true
}

const DELTA_SYSTEM_INSTRUCTION: &str = "\
You are a world-state tracker. Given the current schema and the last exchange, emit ONLY the keys that changed as a JSON delta. Do NOT rewrite unchanged keys.

Output format (raw JSON only: no markdown fences, no prose):
{
  \"summary\": \"<updated summary string, or omit if unchanged>\",
  \"recent_events\": [\"<new event>\", ...],
  \"entities\": {\"<key>\": \"<new value>\", \"<key_to_delete>\": null}
}

Rules:
- Emit ONLY changed keys. Omit unchanged sections entirely. If nothing tracked changed this turn, emit {}.
- entities: a null value means DELETE the key. A non-null string means SET/overwrite.
- Keep the delta minimal: a few keys at most per turn.
- summary: only emit when the narrative arc meaningfully shifts, not every turn.
- recent_events: append only genuinely new salient events.\n";

/// System instruction for the World Progression pass (Seam #4, 2026-07-27).
/// Distinct from the delta pass: this fires on a TIME tick, not a chat
/// exchange, so the instruction is about advancing OFF-SCREEN state rather
/// than recording the just-completed turn. The output shape is identical
/// (a `SchemaDelta`) so the same parser + validator + apply path is reused.
const WORLD_PROGRESSION_SYSTEM_INSTRUCTION: &str = "\
You are a world simulation engine. In-world time has elapsed off-screen, and \
you advance the simulation: pick a small subset of entities that would \
plausibly have moved in the elapsed window (a faction relocates, an NPC's \
mood shifts, a deadline approaches, a rumor spreads, a rival makes a move) \
and emit ONLY their changed keys as a JSON delta. The world moves \
independently of the player's bubble.

Output format (raw JSON only: no markdown fences, no prose):
{
  \"summary\": \"<one-line updated arc summary, or omit if unchanged>\",
  \"recent_events\": [\"<new off-screen event>\", ...],
  \"entities\": {\"<key>\": \"<new value>\", \"<key_to_delete>\": null}
}

Rules:
- Emit ONLY changed keys. Omit unchanged sections entirely. If nothing \
plausibly moved, emit {}.
- entities: a null value means DELETE the key. A non-null string means SET.
- Pick 1-4 entities to advance per tick — the world moves in small ripples, \
not wholesale rewrites. Avoid touching the player's direct possessions \
or immediate scene state (those are the player's bubble).
- Some entity keys may be flagged immutable (the canonical identity of an NPC, \
the foundational facts of a location). NEVER overwrite or delete those — \
record changes under NEW keys instead (e.g. append to a chronicle field).
- summary: only update when the macro state of the world meaningfully shifts.
- recent_events: append a brief note about each off-screen development.\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_prompt_contains_system_instruction_and_exchange() {
        let prompt = render_delta_prompt(
            "{\"summary\":\"\"}",
            &("I pick up the sword".to_string(), "You grab it.".to_string()),
            &[], // no deferred attempts in the common case
        );
        assert!(prompt.contains("world-state tracker"));
        assert!(prompt.contains("I pick up the sword"));
        assert!(prompt.contains("You grab it."));
        assert!(prompt.starts_with("<|turn>system\n"));
        assert!(prompt.ends_with("<|turn>model\n"));
    }

    #[test]
    fn delta_prompt_folds_deferred_attempts_when_present() {
        // The fail-proof contract layer 3: a prior turn's failed delta must
        // surface in the next turn's prompt so the model gets a fresh shot.
        let deferred = vec![FailedAttempt {
            exchange: Some(("prior user text".to_string(), "prior model text".to_string())),
            trigger: None,
            errors: "pass 1 JSON parse: ... | pass 2 validation: ...".to_string(),
            passes_used: MAX_DELTA_PASSES,
        }];
        let prompt = render_delta_prompt(
            "{\"summary\":\"\"}",
            &("new user text".to_string(), "new model text".to_string()),
            &deferred,
        );
        assert!(prompt.contains("Previously deferred"));
        assert!(prompt.contains("prior user text"));
        assert!(prompt.contains("prior model text"));
        assert!(prompt.contains("pass 1 JSON parse"));
        // The new exchange is still the primary anchor.
        assert!(prompt.contains("new user text"));
    }

    // ---------- World Progression prompt (Seam #4) ----------

    #[test]
    fn world_progression_prompt_contains_interval_and_state() {
        // The model needs to see (a) the current entities, (b) the elapsed
        // interval, (c) the system instruction framing the off-screen task.
        let prompt = render_world_progression_prompt(
            "{\"entities\":{\"faction.cult.position\":\"east_ridge\"}}",
            24,
            &[],
        );
        assert!(prompt.contains("world simulation engine"));
        assert!(prompt.contains("24 hours"));
        assert!(prompt.contains("faction.cult.position"));
        assert!(prompt.contains("east_ridge"));
        assert!(prompt.starts_with("<|turn>system\n"));
        assert!(prompt.ends_with("<|turn>model\n"));
    }

    #[test]
    fn world_progression_prompt_instructs_off_screen_subset() {
        // Critical framing: the model must advance a SUBSET, not rewrite the
        // world wholesale, and must NOT touch the player's bubble.
        let prompt = render_world_progression_prompt("{}", 12, &[]);
        assert!(prompt.contains("SUBSET"));
        assert!(prompt.contains("off-screen"));
        assert!(prompt.contains("the player's direct possessions"));
    }

    #[test]
    fn world_progression_prompt_mentions_immutability_rule() {
        // The system instruction must warn about immutable keys so the model
        // doesn't try to overwrite canon (the validator catches it, but the
        // instruction prevents wasted passes).
        let prompt = render_world_progression_prompt("{}", 24, &[]);
        assert!(prompt.contains("immutable"));
        assert!(prompt.contains("NEW keys"));
    }

    #[test]
    fn world_progression_prompt_folds_deferred_attempts() {
        // Fail-proof contract: a prior failed tick must surface in the next
        // tick's prompt so the model gets a fresh shot at the same interval.
        let deferred = vec![FailedAttempt {
            exchange: None,
            trigger: Some("world progression (~24h elapsed)".to_string()),
            errors: "pass 1 JSON parse: ... | pass 2 validation: ImmutableKeyOverwrite ...".to_string(),
            passes_used: MAX_DELTA_PASSES,
        }];
        let prompt = render_world_progression_prompt("{}", 24, &deferred);
        assert!(prompt.contains("Previously deferred"));
        assert!(prompt.contains("world progression"));
        assert!(prompt.contains("ImmutableKeyOverwrite"));
    }

    #[test]
    fn accumulated_repair_prompt_shows_every_prior_pass() {
        // Pass 3 sees both pass 1 and pass 2's outputs + errors (load-bearing
        // for the accumulating-context shape — vs the old single-shot repair).
        let prior_raw = vec![
            "first bad output".to_string(),
            "second bad output".to_string(),
        ];
        let prior_errors = vec![
            "pass 1 JSON parse: unexpected token".to_string(),
            "pass 2 validation: invalid entity key".to_string(),
        ];
        let prompt = render_accumulated_repair_prompt(&prior_raw, &prior_errors);
        assert!(prompt.contains("2 prior attempt(s) failed"));
        assert!(prompt.contains("first bad output"));
        assert!(prompt.contains("second bad output"));
        assert!(prompt.contains("pass 1 JSON parse"));
        assert!(prompt.contains("pass 2 validation"));
    }

    #[test]
    fn accumulated_repair_prompt_truncates_long_raw_outputs() {
        // Defense against prompt bloat: a 10KB garbage raw output shouldn't
        // eat the entire context. Capped at 500 chars per pass.
        let huge = "x".repeat(10_000);
        let prompt = render_accumulated_repair_prompt(&[huge.clone()], &["err".to_string()]);
        // The full 10KB should not appear; only a 500-char preview.
        assert!(!prompt.contains(&huge));
        assert!(prompt.contains(&"x".repeat(500)));
    }

    // The gate is the M2 overhead fix: skip the full 12B forward pass on
    // clearly non-substantive turns. The contract is conservative: when in
    // doubt, fire (the cost of a missed world-state change > one wasted pass).

    #[test]
    fn gate_skips_short_filler_user_messages() {
        // The canonical skip cases: 1-4 word filler with a real assistant reply.
        let reply = "Sure thing, here's the info you asked for.";
        for filler in &[
            "ok", "thanks", "lol", "yes", "no", "sure", "k", "yep",
            "ok cool", "got it", "sounds good", "will do",
        ] {
            assert!(
                !should_fire_delta(filler, reply),
                "filler {filler:?} should skip the delta pass"
            );
        }
    }

    #[test]
    fn gate_skips_when_assistant_reply_is_empty() {
        // Empty/error reply → nothing to record, regardless of user message.
        assert!(!should_fire_delta("Tell me about the dungeon", ""));
        assert!(!should_fire_delta("Tell me about the dungeon", "   "));
    }

    #[test]
    fn gate_fires_on_normal_substantive_exchange() {
        // A real question + real reply → always fire.
        assert!(should_fire_delta(
            "What's in the iron chest?",
            "You open it and find a glowing amulet inside."
        ));
    }

    #[test]
    fn gate_fires_on_long_user_message_even_if_filler_sounding() {
        // 5+ words clears the word ceiling regardless of content: fires.
        assert!(should_fire_delta(
            "ok so anyway let me think about that for a second",
            "Take your time."
        ));
    }

    #[test]
    fn gate_fires_on_short_roleplay_action_with_pronoun() {
        // The critical false-negative guard: "I nod" is 2 words (would skip by
        // count alone) but it's a world-moving roleplay action. The pronoun
        // check catches it and fires.
        assert!(should_fire_delta("I nod", "She acknowledges you."));
        assert!(should_fire_delta("I draw my sword", "Roll for initiative."));
        assert!(should_fire_delta("you see a goblin", "It snarls."));
    }

    #[test]
    fn gate_fires_on_first_person_contraction() {
        // "I'm" / "I'll": pronoun check covers contractions too.
        assert!(should_fire_delta("I'm going north", "The path narrows."));
        assert!(should_fire_delta("I'll attack", "You strike."));
    }

    #[test]
    fn gate_skips_short_message_without_pronoun_or_verb_shape() {
        // 3 words, no pronoun, not action-shaped: filler, skip.
        assert!(!should_fire_delta("lol that's funny", "Glad you enjoyed it."));
    }
}
