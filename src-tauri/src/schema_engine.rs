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
//! previously loaded its own duplicate weight copy — ~9.8GB in the 12B era,
//! ~5.8GB today — a redundant full-model VRAM cost).
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
//! 2. **2-pass delta repair loop + full-emit fallback (2026-08-22 multihog
//!    WS5).** Initial generation → on failure, one accumulated repair pass
//!    (shows pass 1's raw output + the specific error). A third delta pass
//!    rarely recovered what the second missed (the empirical repair cliff),
//!    so the budget funds a FULL-EMIT fallback instead: one lenient pass
//!    asks for a flat key → COMPLETE-new-value map, and Rust normalizes +
//!    diffs it (`full_emit_to_delta` — omission = unchanged, null = delete).
//!    Entities only; summary/events failures keep the deferred-retry path.
//! 3. **Failure queue (`failed_delta_queue` on AppState).** A delta that
//!    still fails all passes (incl. the full-emit fallback) is NOT
//!    dropped: it's queued. The next turn's
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

/// The schema context's token budget. See `settings::CTX_SCHEMA` for the
/// 2026-08-24 2048 → 8192 raise: the measured world-progression tick
/// composition (~8.1k chars base, ~16.8k all-caps worst) overflowed the old
/// 1,792-token prompt ceiling and the middle-drop spliced the schema JSON.
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

/// (2026-08-22 multihog WS5) Maximum DELTA-shape passes per attempt
/// (initial + 1 repair = 2 total), down from 3: pass 3 rarely recovered
/// what pass 2 missed (the empirical LLM-JSON-repair cliff), and the freed
/// decode budget funds the FULL-EMIT FALLBACK — after the delta passes
/// fail, one lenient pass asks for a flat key → COMPLETE-new-value map and
/// Rust-side normalizes + diffs it into a delta (see
/// [`full_emit_to_delta`]). Summary/events failures keep the
/// deferred-retry path (the fallback recovers ENTITIES only). A failure
/// that survives everything enqueues with `passes_used = MAX_DELTA_PASSES
/// + 1` (2 delta + 1 full-emit).
const MAX_DELTA_PASSES: u8 = 2;

// ---------------------------------------------------------------------------
// Control plane: channel types
// ---------------------------------------------------------------------------

/// A request to the schema thread: diff `last_exchange` against
/// `current_schema` and emit the changed keys.
struct SchemaRequest {
    /// (user_message, assistant_message) from the turn that just completed.
    last_exchange: (String, String),
    /// (2026-08-24 fix) Which surface is asking — tags the failure carrier
    /// so the shared `failed_delta_queue` drain sites never cross-feed a
    /// chat failure into a fable delta prompt (different schemas), or vice
    /// versa.
    surface: DeltaSurface,
    /// The current schema serialized as pretty JSON, so the model knows what
    /// to diff against.
    current_schema_json: String,
    /// Deferred attempts from prior turns that the engine couldn't commit
    /// (all passes failed). Folded into this turn's prompt as
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
/// `failed_attempt` is `Some` ONLY when all passes failed (2 delta + the
/// full-emit fallback) AND the failure looks retryable (parse failures,
/// validation failures). Generation errors (tokenize/prefill/decode
/// infrastructure failures) leave it `None` — those aren't going to fix
/// themselves on the next turn. The caller (lib.rs's delta-fire spawn)
/// pushes the `FailedAttempt` onto the failure queue; the next turn's
/// delta prompt folds it in. See module doc layer 3.
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
    /// Populated when all passes (delta + full-emit fallback) failed AND the
/// failure is retryable
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
/// (2026-08-24 fix) Which surface produced a delta attempt. The two delta
/// surfaces — Wupi chat (`state.schema`) and Fable turns
/// (`state.fable_schema`) — run against DIFFERENT schemas, so a failure
/// from one surface must only ever be re-fed into that surface's next
/// prompt; the drain sites filter on this tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum DeltaSurface {
    /// The Wupi chat auto-summarizer (chat_send's post-turn spawn) + the
    /// manager translation path.
    #[default]
    Chat,
    /// The Fable per-turn delta (fable_send's post-done spawn) + the world
    /// progression tick.
    Fable,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedAttempt {
    /// The (user, assistant) exchange that produced the failed delta. Empty
    /// for translation attempts (which carry `trigger` instead).
    pub exchange: Option<(String, String)>,
    /// The player request that produced the failed translation. `None` for
    /// auto-summarizer attempts (which carry `exchange` instead).
    pub trigger: Option<String>,
    /// The accumulated errors from all passes (2 delta + the full-emit
    /// fallback), joined. The next attempt's prompt can include this so the
    /// model knows what went wrong last time.
    pub errors: String,
    /// How many times this attempt has been retried (always 3 on first
    /// enqueue — 2 delta passes + 1 full-emit — the caller bumps it if a
    /// deferred re-attempt ALSO fails and re-enqueues). The queue caps
    /// total retries to avoid pathological loops — see lib.rs's
    /// `failed_delta_queue` cap.
    pub passes_used: u8,
    /// The surface that produced this attempt (2026-08-24 fix). The queue
    /// drain sites take only their own surface — a chat failure is never
    /// folded into a fable delta prompt (different schema), and vice versa.
    #[serde(default)]
    pub surface: DeltaSurface,
}

/// Type alias distinguishing the three kinds of triggering context an attempt
/// carries. Internal to the engine; `FailedAttempt` exposes them as
/// `Option<(exchange)>` / `Option<request>` / `Option<interval>` for the IPC
/// boundary.
#[derive(Clone)]
enum AttemptSource {
    /// Auto-summarizer: triggered by a chat or fable exchange. `surface`
    /// tags the failure carrier so the queue drain sites never cross-feed
    /// the two schemas.
    Auto {
        exchange: (String, String),
        surface: DeltaSurface,
    },
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
    /// schema context's ~50MB is reclaimable without process restart. Mirrors
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

/// (2026-08-19 Stale Roulette) One site designated to the world-progression
/// pass — the model is asked to (optionally) emit a `site_seeds` hook for
/// it. Pure prompt input; the apply lives in `fire_world_progression_tick`.
#[derive(Debug, Clone)]
pub struct DesignatedSite {
    /// The travel-node id.
    pub id: String,
    /// Diegetic name (prompt flavor).
    pub name: String,
    /// In-world days since the node's `last_evolved_minutes` watermark
    /// (0 = never designated).
    pub elapsed_days: i64,
    /// (2026-08-23 starvation fix) In-world days since the node's
    /// `last_material_minutes` watermark — the last time a seed ACTUALLY
    /// planted (the designation watermark above rotates on every offer).
    /// `None` = never materialized. The prompt renders BOTH so the model
    /// can distinguish "offered recently" from "unchanged for a month".
    pub idle_days: Option<i64>,
    /// The node's current un-germinated seed hooks (honor-them context).
    pub seeds: Vec<String>,
    /// (2026-08-22 multihog WS3) The node's pending pressure lines — the
    /// accumulated intent the pass is asked to honor or evolve past.
    pub pressure: Vec<String>,
}

/// (2026-08-22 living-world) One MAPPED, DEPARTED site designated to the
/// world-progression pass for SITE EVOLUTION — the model may emit
/// constrained `site_ops` for it (set/move/remove asset only). Pure prompt
/// input; the play-canon-locked apply lives in `fire_world_progression_tick`
/// step 7c. The player's CURRENT node never appears here (the bubble
/// freeze, enforced at designation AND re-checked at apply).
#[derive(Debug, Clone)]
pub struct EvolutionSite {
    /// The travel-node id (a key into `WorldSchema::site_maps`).
    pub id: String,
    /// Diegetic name (prompt flavor).
    pub name: String,
    /// In-world days since the player's last visit.
    pub elapsed_days: i64,
    /// The compact id-bearing site slice (`site_map::render_tracker_slice`)
    /// — the asset/area ids the ops must target.
    pub slice: String,
    /// (2026-08-22 multihog WS3) The node's pending pressure lines —
    /// context the evolution ops are asked to honor (an applied op
    /// CONSUMES the queue; a no-op tick retains it — the anti-starvation
    /// rule, enforced at the lib.rs apply).
    pub pressure: Vec<String>,
    /// (2026-08-23 WS5) The site's OPEN causal threads, pre-rendered
    /// bounded lines (`site_map::render_thread_lines`) — the live plots
    /// the evolution pass is asked to advance or resolve. Empty when the
    /// ledger is empty (zero prompt cost).
    pub threads: Vec<String>,
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
    /// (2026-08-19 Stale Roulette) The stale un-mapped sites designated this
    /// tick — the prompt's `## DESIGNATED SITES` section; the model may
    /// emit `site_seeds` hooks for them (Rust validates + plants).
    designated: Vec<DesignatedSite>,
    /// (2026-08-22 living-world) The mapped DEPARTED sites designated this
    /// tick — the prompt's `## DEPARTED SITES` section; the model may emit
    /// constrained `site_ops` for them (Rust applies under the play-canon
    /// locks + bubble freeze).
    evolution_sites: Vec<EvolutionSite>,
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
    /// second WUPI.gguf weight allocation is gone; chat+schema+fable now share
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
                        // Generation succeeded but all passes failed
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
    /// schema context's ~50MB on the VRAM-hibernate path. Idempotent across
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
    ///
    /// `surface` (2026-08-24 fix) tags the request + any failure carrier
    /// with the asking surface (chat vs fable) — the failure queues filter
    /// on it at drain time.
    pub fn request_delta(
        &self,
        last_exchange: (String, String),
        current_schema: &WorldSchema,
        deferred_attempts: Vec<FailedAttempt>,
        surface: DeltaSurface,
    ) -> anyhow::Result<mpsc::Receiver<SchemaReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<SchemaReply>();
        let req = SchemaRequest {
            last_exchange,
            surface,
            current_schema_json: current_schema.to_json_prompt(),
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
            current_schema_json: current_schema.to_json_prompt(),
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
    /// carries failed progression attempts from prior ticks. `designated`
    /// carries this tick's Stale-Roulette sites (2026-08-19). Same shape +
    /// semantics as the delta/translation paths.
    pub fn request_world_progression(
        &self,
        current_schema: &WorldSchema,
        interval_hours: u32,
        deferred_attempts: Vec<FailedAttempt>,
        designated: Vec<DesignatedSite>,
        evolution_sites: Vec<EvolutionSite>,
    ) -> anyhow::Result<mpsc::Receiver<SchemaReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<SchemaReply>();
        let req = WorldProgressionRequest {
            current_schema_json: current_schema.to_json_prompt(),
            interval_hours,
            deferred_attempts,
            designated,
            evolution_sites,
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
    /// redundant second WUPI.gguf weight allocation is gone — chat+schema+
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

        // SCHEMA_CTX is fixed for both Local and API modes: under API
        // the schema engine only runs as a fallback / silent delta agent.
        // 8192 (2026-08-24 raise, see settings::CTX_SCHEMA) fits the fattest
        // path — the world-progression tick — whole, so the middle-drop
        // never splices the schema JSON the model must diff against. See §5.
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
                surface: req.surface,
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
        // (2026-08-17 E4B shakedown P0) Manager-path prefix telemetry — the
        // sibling of engine.rs's [PREFIX] chat line. The schema prompts are
        // char-capped upstream, so this is an approx-token readout (chars÷3.7,
        // the observed density) whose only alarm condition is approaching the
        // middle-drop threshold, where the schema JSON the model must diff
        // against starts losing contiguous bands.
        {
            let approx_tokens = initial_prompt.len() / 4;
            let budget = (SCHEMA_CTX as usize).saturating_sub(SCHEMA_MAX_TOKENS as usize);
            eprintln!(
                "[DEBUG] [PREFIX] manager translation render: {} chars (~{} tokens of ~{}-token budget)",
                initial_prompt.len(),
                approx_tokens,
                budget
            );
        }
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
            &req.designated,
            &req.evolution_sites,
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

    /// The shared repair loop (2026-08-22 multihog WS5: 2 delta passes +
    /// full-emit fallback). Runs the model up to `MAX_DELTA_PASSES` times
    /// in the delta grammar; each pass parses the output via
    /// `SchemaDelta::from_model_output` AND validates it via
    /// `schema_validator::validate`. A pass succeeds only if both parse and
    /// validation succeed. The repair prompt shows the prior errors + raw
    /// outputs so the model sees what it got wrong. When both delta passes
    /// fail, the FULL-EMIT FALLBACK runs (see [`full_emit_to_delta`]).
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
        // (2026-08-23 audit fix) The failure-queue carrier, factored so an
        // INFRA failure (engine decode error) after retryable parse/
        // validation failures can still carry them — the old `?` returned
        // Err and silently discarded the accumulated retryable errors,
        // thinning the "no world-state evolution is ever silently dropped"
        // contract. passes_used stays MAX_DELTA_PASSES + 1 (the budget the
        // caller's MAX_TOTAL_PASSES was sized on).
        let build_failed_carrier = |raw_outputs: &[String], errors: &[String]| -> AttemptOutcome {
            let (exchange_opt, trigger_opt, surface) = match &source {
                AttemptSource::Auto { exchange, surface } => (Some(exchange.clone()), None, *surface),
                AttemptSource::Translation { request } => {
                    (None, Some(request.clone()), DeltaSurface::Chat)
                }
                AttemptSource::WorldProgression { interval_hours } => (
                    None,
                    Some(format!("world progression (~{interval_hours}h elapsed)")),
                    DeltaSurface::Fable,
                ),
            };
            let joined = errors.join(" | ");
            AttemptOutcome::Failed {
                last_raw_output: raw_outputs.last().cloned().unwrap_or_default(),
                errors: joined.clone(),
                carrier: FailedAttempt {
                    exchange: exchange_opt,
                    trigger: trigger_opt,
                    errors: joined,
                    passes_used: MAX_DELTA_PASSES + 1,
                    surface,
                },
            }
        };

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
            let raw = match self.generate_text(&prompt) {
                Ok(r) => r,
                // Pure infrastructure failure with nothing retryable
                // accumulated — propagate as infra (the caller restores
                // priors unbumped).
                Err(e) if errors.is_empty() => return Err(e),
                Err(e) => {
                    tracing::warn!(
                        label,
                        pass,
                        error = %format!("{e:#}"),
                        "{label} pass {pass} infra failure after retryable failures — carrying for re-attempt"
                    );
                    errors.push(format!("pass {pass} infra failure: {e}"));
                    return Ok(build_failed_carrier(&raw_outputs, &errors));
                }
            };
            // Reasoning debug: strip the thought channel BEFORE storing the
            // raw output into raw_outputs. extract_reply_channel
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

        // (2026-08-22 multihog WS5) FULL-EMIT FALLBACK — after the delta
        // passes fail, ONE lenient pass asks for a flat entity-key →
        // COMPLETE-new-value map ("any reasonable shape"), parsed with
        // maximum tolerance + normalized + diffed in Rust (omitted key =
        // unchanged, explicit null = delete). A truncated or shape-drifted
        // output can never mass-delete that way — the safe default is
        // "unchanged". Entities only: summary/events changes lost to the
        // failed passes ride the deferred-retry queue below when the
        // fallback also fails.
        let fallback_prompt = render_full_emit_prompt(&raw_outputs, &errors);
        let raw = match self.generate_text(&fallback_prompt) {
            Ok(r) => r,
            // Pure infra with no accumulated retryable errors → propagate.
            Err(e) if errors.is_empty() => return Err(e),
            Err(e) => {
                // (2026-08-23 audit fix) Same carry as the pass loop: the
                // two delta passes' retryable errors must reach the failure
                // queue even when the fallback decode itself dies.
                tracing::warn!(
                    label,
                    error = %format!("{e:#}"),
                    "{label} full-emit fallback infra failure — carrying for re-attempt"
                );
                errors.push(format!("full-emit fallback infra failure: {e}"));
                return Ok(build_failed_carrier(&raw_outputs, &errors));
            }
        };
        let reply = crate::schema::extract_reply_channel(&raw);
        match full_emit_to_delta(&reply, immutable_keys, existing_keys) {
            Ok(delta) => {
                tracing::info!(
                    label,
                    keys = delta.entities.as_ref().map(|m| m.len()).unwrap_or(0),
                    "{label} recovered via full-emit fallback after {MAX_DELTA_PASSES} delta passes"
                );
                return Ok(AttemptOutcome::Committed { raw_output: reply, delta });
            }
            Err(e) => {
                tracing::warn!(label, error = %e, "{label} full-emit fallback failed");
                errors.push(format!("full-emit fallback: {e}"));
                // The fallback's own output joins the record so the carrier
                // below reports it as the last raw output (the pre-closure
                // behavior).
                raw_outputs.push(reply.clone());
            }
        }

        // All passes exhausted (delta passes + the fallback). Build the
        // failure-queue carrier so the caller can enqueue this for
        // re-attempt on the next turn. The carrier carries the SOURCE
        // (exchange, request, or progression interval) + the accumulated
        // errors; it does NOT carry the broken raw outputs (re-running
        // those through the model rarely helps; fresh context does). For
        // WorldProgression the trigger is a synthetic string carrying the
        // interval so the next tick's prompt can re-attempt at the same
        // magnitude.
        tracing::warn!(
            label,
            passes = MAX_DELTA_PASSES + 1,
            errors = errors.join(" | "),
            "{label} failed all passes (incl. full-emit); carrying for re-attempt"
        );
        Ok(build_failed_carrier(&raw_outputs, &errors))
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
        // Guard: if the prompt alone exceeds the context, truncate the
        // MIDDLE (#30 2026-08-15) — keep the BOS token + the head slice (the
        // system instruction lives at the front) + the tail slice (the
        // exchange + generation prompt live at the end), dropping the schema
        // body's least-recent detail in between. The old front-drain deleted
        // the BOS token AND the system instruction, so the delta pass
        // degraded toward garbage exactly when long-campaign state (and thus
        // the prompt) was richest.
        let max_prompt = (SCHEMA_CTX as usize).saturating_sub(SCHEMA_MAX_TOKENS as usize);
        if tokens.len() > max_prompt {
            let head = std::cmp::min(std::cmp::max(max_prompt / 4, 64), tokens.len());
            let tail = max_prompt.saturating_sub(head).min(tokens.len() - head);
            let dropped = tokens.len() - head - tail;
            let mut kept: Vec<LlamaToken> = tokens[..head].to_vec();
            kept.extend_from_slice(&tokens[tokens.len() - tail..]);
            tokens = kept;
            tracing::warn!(dropped, head, tail, "schema prompt exceeded context; truncated the middle (kept BOS + system head + exchange tail)");
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

/// (2026-08-16 bug 12) Cap one side of the exchange folded into a schema
/// prompt, char-boundary-safe with a visible truncation marker. The deferred
/// re-attempt entries render with a plain `.take(200)`; the LIVE exchange
/// used to ride in verbatim + unbounded — this is the same discipline for
/// the primary anchor. `pub(crate)` so the translation prompt
/// (fable_command) shares it.
pub(crate) fn cap_exchange_chars(s: &str) -> String {
    const CAP: usize = crate::settings::SCHEMA_EXCHANGE_CHAR_CAP;
    if s.chars().count() <= CAP {
        return s.to_string();
    }
    let mut out: String = s.chars().take(CAP).collect();
    out.push_str("[…]");
    out
}

/// (2026-08-24 review) Cap the `FailedAttempt::errors` string folded into a
/// schema prompt. The errors join every prior pass's failure text and were
/// rendered UNBOUNDED at three sites (delta + world-progression + the
/// translation prompt) — with the queue cap of 8 deferred attempts, one
/// verbose failure family could add thousands of chars to a prompt path
/// already riding its context ceiling, amplifying the middle-drop risk the
/// piecewise caps exist to kill. 400 chars keeps the actionable signal (the
/// error class + key names); same `[…]` marker discipline as the exchange
/// cap. `pub(crate)` so `fable_command`'s translation prompt shares it.
pub(crate) fn cap_attempt_error_chars(s: &str) -> String {
    const CAP: usize = 400;
    if s.chars().count() <= CAP {
        return s.to_string();
    }
    let mut out: String = s.chars().take(CAP).collect();
    out.push_str("[…]");
    out
}

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
    // (2026-08-16 bug 12) Both exchange sides run through the cap — an
    // unbounded reply here blew the 2048-token budget + the middle-drop
    // spliced the entity JSON the model must diff against.
    out.push_str("\n\nLast exchange:\n[user]: ");
    out.push_str(&cap_exchange_chars(&last_exchange.0));
    out.push_str("\n[model]: ");
    out.push_str(&cap_exchange_chars(&last_exchange.1));
    // Deferred re-attempt context. When the previous turn's delta failed all
    // passes, fold its triggering exchange + accumulated errors in here so
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
                cap_attempt_error_chars(&attempt.errors)
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
/// (2026-08-22 living-world) HARD char budget for the `## DEPARTED SITES`
/// evolution section (site slices + op grammar + laws). The progression
/// prompt rides CTX_SCHEMA: it already carries the ~4k-char schema
/// JSON + instruction + deferred attempts; an unbounded site section would
/// push the total past the prompt ceiling and the middle-drop would splice a
/// contiguous band out of the schema JSON the model must diff against (the
/// exact failure the `SCHEMA_JSON_PROMPT_BUDGET_CHARS` pin exists to kill).
///
/// (2026-08-22 calibration) The grammar block alone measures ~740 chars and
/// each designated site line adds ~95 + its ≤300-char slice, so a realistic
/// two-site section lands at ~1,530 — the original 1,400 cap truncated the
/// SECOND site's slice mid-line (potentially through an asset id) on every
/// tick. 1,700 admitted the realistic composition whole.
///
/// (2026-08-23 WS5 recalibration) 1,700 → 1,900: the per-site lines now
/// build into `section` (the out-push bug fix above — the trim is finally
/// REAL) and each site may carry `Open threads:` ≤2×120 chars (WS5 causal
/// ledger), so the realistic two-site worst is ~1,530 + ~500 + pressure
/// ≈ 1,900. Only degenerate compositions hit the trim, and the whole
/// section still rides CTX_SCHEMA beside the ~4k-char schema JSON.
/// (2026-08-24 Part II A2) 1,900 → 2,300: the grammar block gained the
/// fourth op (add_asset) + the restock/carrier laws (~380 chars), so the
/// realistic two-site worst moves to ~2,280 — still well under the CTX_SCHEMA
/// headroom beside the schema JSON (the 2026-08-24 raise to 8192 tokens
/// gives ~28.5k chars of prompt budget; the composed whole-prompt pin below
/// is the authoritative guard).
const SITE_EVOLUTION_PROMPT_BUDGET_CHARS: usize = 2_300;
/// Per-site slice truncation (ids + doors + states, lean-truncated —
/// enough for the model to target real asset ids).
const SITE_EVOLUTION_SLICE_CHARS: usize = 300;
/// (2026-08-22 multihog WS3) Total `site_pressure` entries the tick apply
/// accepts per round (the "at most 6" in the instruction — the pair moves
/// together). Bounded so one enthusiastic pass cannot stuff every node's
/// pressure queue at once.
pub const SITE_PRESSURE_MAX_PER_TICK: usize = 6;

fn render_world_progression_prompt(
    current_schema_json: &str,
    interval_hours: u32,
    deferred_attempts: &[FailedAttempt],
    designated: &[DesignatedSite],
    evolution_sites: &[EvolutionSite],
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
         a rumor spreads, a rival makes a move) and emit ONLY their changed keys. \
         Trends are causal state, not escalation commands: a pressure may intensify, \
         persist, plateau, fragment, transform, backfire, resolve, or reverse when \
         causally plausible — reversals need an intelligible macro cause. Avoid both \
         automatic escalation and arbitrary oscillation."
    ));
    // (2026-08-19 Stale Roulette) The designated-site section — ONE decode,
    // folded into the existing tick pass (not three micro-prompts). The
    // model MAY answer with `site_seeds` hooks for these ids; "no change"
    // is a valid outcome (Rust stamps the watermark regardless — rotation).
    if !designated.is_empty() {
        out.push_str("\n\n## DESIGNATED SITES\n");
        out.push_str(
            "These unvisited places moved while the player was elsewhere. For any that \
             plausibly changed, add a \"site_seeds\" object to your JSON keyed by site id, \
             each value ONE short line (≤140 chars) describing what festered, moved in, \
             collapsed, or stirred there. Sites with existing seeds build on them, never \
             contradict. A site that stayed quiet simply gets no entry.\n",
        );
        for d in designated {
            out.push_str(&format!(
                "- {} (id: {}) — ~{} day(s) since last touched",
                d.name,
                d.id,
                d.elapsed_days
            ));
            // (2026-08-23 starvation fix) The material-change read: how long
            // since something LAST HAPPENED here, independent of the
            // designation rotation. A never-materialized site or a long-idle
            // one is OVERDUE — the model should prefer it over a
            // recently-touched neighbor.
            match d.idle_days {
                None => out.push_str("; NEVER materialized"),
                Some(days) if days > 0 => {
                    out.push_str(&format!("; last material change ~{days} day(s) ago"))
                }
                Some(_) => {} // changed within the day — nothing to flag
            }
            if !d.seeds.is_empty() {
                out.push_str(&format!("; known: {}", d.seeds.join(" / ")));
            }
            // (2026-08-22 multihog WS3) The pending-pressure read: the
            // accumulated intent this pass is asked to honor or evolve past.
            if !d.pressure.is_empty() {
                out.push_str(&format!(
                    "; pending pressure: {}",
                    d.pressure.join(" / ")
                ));
            }
            out.push('\n');
        }
    }
    // (2026-08-22 living-world) The departed-site EVOLUTION section — the
    // constrained mutation pass over mapped interiors the player has left.
    // HARD-capped at SITE_EVOLUTION_PROMPT_BUDGET_CHARS (see the const's
    // CTX_SCHEMA guard note).
    if !evolution_sites.is_empty() {
        let mut section = String::with_capacity(1_024);
        section.push_str("\n\n## DEPARTED SITES\n");
        section.push_str(
            "These mapped interiors kept moving while the player was elsewhere. You may add a \
             \"site_ops\" object to your JSON keyed by site id, each value a LIST of ops drawn \
             from exactly these four forms:\n\
             {\"op\":\"set_asset\",\"asset\":\"<id>\",\"state\":\"active|dead|taken|triggered|deactivated|fleeing\",\"count\":N,\"detail\":\"...\",\"cause\":\"...\",\"actor\":\"...\"}\n\
             {\"op\":\"move_asset\",\"asset\":\"<id>\",\"to\":\"<area_id>\",\"cause\":\"...\",\"actor\":\"...\"}\n\
             {\"op\":\"remove_asset\",\"asset\":\"<id>\",\"cause\":\"...\",\"actor\":\"...\"}\n\
             {\"op\":\"add_asset\",\"id\":\"<new-id>\",\"name\":\"<name>\",\"kind\":\"creature|group|object|trap|hazard|loot\",\"area\":\"<existing-area-id>\",\"count\":N,\"cause\":\"<why they came>\"}\n\
             Laws: terminal states are locked (dead stays dead, looted stays looted, disarmed \
             stays disarmed — carry the aftermath in cause/detail, or remove the remnant); \
             never invent areas; a new arrival enters only through add_asset. A quiet site \
             simply gets no entry. \
             add_asset restocks a vacated site with original dwellers, scavengers, wildlife, \
             rival delvers, or a cult moving in — whatever the site could plausibly attract \
             (a sealed tomb does not host a market; arrivals need a way in). \
             Living occupants act: a group fortifies, forages, buries its dead, \
             squabbles — carry it in set_asset detail/count or a move; a long \
             window with several living groups justifies several ops. An asset \
             moves only along open ways — never across locked or blocked routes. \
             Moving or removing a carrier moves or drops what it carries.\n",
        );
        for site in evolution_sites {
            let slice: String = site
                .slice
                .chars()
                .take(SITE_EVOLUTION_SLICE_CHARS)
                .collect();
            // (2026-08-23 WS5 FIX) Per-site lines build into `section`, NOT
            // `out` — the original code pushed them to `out`, so the bullets
            // rendered BEFORE the `## DEPARTED SITES` header and the
            // whole-section budget trim below could never fire (it only
            // bounded header+grammar). The trim is now real: the CTX_SCHEMA
            // middle-drop guard holds by construction.
            section.push_str(&format!(
                "- {} (id: {}) — ~{} day(s) since the player's last visit. Current truth: \
                 {slice}",
                site.name, site.id, site.elapsed_days
            ));
            // (2026-08-22 multihog WS3) Same pending-pressure read: the ops
            // for this site are asked to honor it (an applied op consumes
            // the queue at the lib.rs apply).
            if !site.pressure.is_empty() {
                section.push_str(&format!(
                    ". Pending pressure: {}",
                    site.pressure.join(" / ")
                ));
            }
            // (2026-08-23 WS5) The open causal threads — live plots the pass
            // is asked to advance or resolve (bounded ≤2×120 chars/site,
            // pre-rendered by `site_map::render_thread_lines`).
            if !site.threads.is_empty() {
                section.push_str(&format!(
                    ". Open threads: {}",
                    site.threads.join(" / ")
                ));
            }
            section.push('\n');
        }
        // Budget trim: the section is bounded whole (the grammar block is
        // irreducible; trailing SITES fall off first, oldest-last order
        // means the stalest — most evolved-worthy — site leads).
        if section.chars().count() > SITE_EVOLUTION_PROMPT_BUDGET_CHARS {
            section = section
                .chars()
                .take(SITE_EVOLUTION_PROMPT_BUDGET_CHARS)
                .collect();
        }
        out.push_str(&section);
    }
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
                cap_attempt_error_chars(&attempt.errors)
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

/// (2026-08-22 multihog WS5) The full-emit fallback's prompt: after the
/// delta passes fail, ask for ONE flat JSON object mapping each entity key
/// the model intends to change → the COMPLETE new value in any reasonable
/// shape. Deliberately NOT the delta grammar (that just failed twice) — the
/// simpler emit shape is the point, and Rust-side
/// [`full_emit_to_delta`] owns the diff semantics. Pure.
fn render_full_emit_prompt(prior_raw: &[String], prior_errors: &[String]) -> String {
    let mut out = String::with_capacity(1024 + prior_raw.len() * 256);
    out.push_str("<|turn>system\n");
    out.push_str(
        "Your delta attempts were invalid. Switch format: emit ONLY one flat JSON object \
         whose keys are the entity keys you intend to CHANGE and whose values are the \
         COMPLETE new value for each (a string, number, or small object — any reasonable \
         shape). To delete a key, make its value null. OMIT every unchanged key entirely. \
         If nothing actually changed, emit {}.",
    );
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Prior attempts (for reference only):\n");
    for (i, raw) in prior_raw.iter().enumerate() {
        let err = prior_errors.get(i).map(|s| s.as_str()).unwrap_or("(no error recorded)");
        out.push_str(&format!(
            "\n--- Attempt {} ({}): {}\n",
            i + 1,
            err,
            raw.chars().take(400).collect::<String>(),
        ));
    }
    out.push_str("\n---\nNow emit the flat key → new-value object:\n<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// (2026-08-22 multihog WS5) Parse + normalize + diff one full-emit output
/// into a valid [`SchemaDelta`]. Maximum tolerance on the parse (reply
/// channel → fenced-JSON extraction → `json_repair` → serde, the
/// `SiteMap::from_model_output` pipeline); STRICT Rust-side semantics on
/// the diff:
///
/// - **Omitted key = unchanged** (the safe default — a truncated output
///   can never mass-delete).
/// - **Explicit `null` = delete.**
/// - String values are control-char-stripped + clamped to the validator's
///   `MAX_VALUE_LEN` (chars, never bytes); keys clamp at `MAX_KEY_LEN`.
/// - The constructed delta runs through `schema_validator::validate` —
///   the immutability lock + count caps are enforced NATURALLY on this
///   path too, so a fallback can never do what the delta passes couldn't.
///
/// Returns `Err(reason)` when the lenient parse yields nothing usable;
/// the caller carries that into the deferred-retry queue with full-emit
/// provenance. Pure + unit-tested.
pub(crate) fn full_emit_to_delta(
    raw: &str,
    immutable_keys: &std::collections::HashSet<String>,
    existing_keys: &std::collections::HashSet<String>,
) -> Result<SchemaDelta, String> {
    let reply = crate::schema::extract_reply_channel(raw);
    let (_prose, bodies) = crate::bracket_parser::extract_fenced_json(&reply);
    let candidates: Vec<String> = if bodies.is_empty() {
        vec![reply.trim().to_string()]
    } else {
        bodies
    };
    let mut parsed: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut last_err = "no JSON object found in the full-emit output".to_string();
    for body in &candidates {
        let repaired = crate::json_repair::repair(body);
        match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&repaired) {
            Ok(map) => {
                parsed = Some(map);
                break;
            }
            Err(e) => last_err = format!("JSON parse: {e}"),
        }
    }
    let Some(map) = parsed else {
        return Err(last_err);
    };
    let flatten = |v: &str| -> String {
        v.chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                _ => c,
            })
            .collect()
    };
    // (2026-08-23 audit fix) The fallback fires only after the model failed
    // the delta grammar twice, and the fallback prompt re-quotes those
    // delta-shaped outputs "for reference" — a model that answers with the
    // ENVELOPE shape (`{"summary":…, "recent_events":[…], "entities":{…}}`)
    // used to mint grime entities literally named `summary`/`entities`.
    // An `entities` value holding an OBJECT is UNWRAPPED (its inner keys
    // are the flat map we asked for — works for the lone and the mixed
    // envelope shape); every other reserved envelope key is skipped (their
    // payloads have no slot in the fallback contract).
    const RESERVED_ENVELOPE_KEYS: &[&str] = &[
        "summary",
        "recent_events",
        "site_seeds",
        "site_ops",
        "site_pressure",
    ];
    let mut flat: Vec<(String, serde_json::Value)> = Vec::with_capacity(map.len());
    for (k, v) in map {
        if k == "entities" {
            if let serde_json::Value::Object(inner) = v {
                for (ik, iv) in inner {
                    flat.push((ik, iv));
                }
            }
            continue;
        }
        flat.push((k, v));
    }
    let mut entities: std::collections::HashMap<String, Option<serde_json::Value>> =
        std::collections::HashMap::new();
    for (k, v) in flat {
        let key: String = flatten(k.trim()).chars().take(schema_validator::MAX_KEY_LEN).collect();
        if key.is_empty() {
            continue;
        }
        if RESERVED_ENVELOPE_KEYS.contains(&key.as_str()) {
            // Envelope keys are never entities; their payloads (summary
            // prose, event arrays) have no slot in the fallback contract.
            continue;
        }
        let value = match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => {
                let cleaned = flatten(&s);
                let cleaned = cleaned.trim();
                Some(serde_json::Value::String(
                    cleaned.chars().take(schema_validator::MAX_VALUE_LEN).collect(),
                ))
            }
            other => {
                // Structured values pass through; `validate` remains the
                // authority on oversize serializations (an oversize object
                // fails validation → the deferred-retry path, never a
                // silent truncation that corrupts structure).
                let compact = serde_json::to_string(&other).unwrap_or_default();
                if compact.chars().count() > schema_validator::MAX_VALUE_LEN {
                    return Err(format!(
                        "entity {key:?} serializes to {} chars (max {})",
                        compact.chars().count(),
                        schema_validator::MAX_VALUE_LEN
                    ));
                }
                Some(other)
            }
        };
        entities.insert(key, value);
    }
    if entities.is_empty() {
        return Err("the full-emit object carried no entity keys".to_string());
    }
    let delta = SchemaDelta {
        entities: Some(entities),
        ..Default::default()
    };
    let ctx = schema_validator::ValidationContext {
        known_nodes: None,
        immutable_keys: Some(immutable_keys),
        existing_keys: Some(existing_keys),
    };
    schema_validator::validate(&delta, &ctx).map_err(|f| f.to_string())?;
    Ok(delta)
}

/// Cheap content gate for whether the schema delta pass should fire this turn.
///
/// The delta pass is a FULL local-model (E4B) forward pass (tokenize + prefill + greedy
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
    // (2026-08-16 audit LOW) CHARS, never bytes (anti-pattern #6's counting
    // discipline): byte length undercounts CJK/emoji short actions as
    // overlong... inverts here — bytes OVERCOUNT non-ASCII, so the 32-unit
    // ceiling skipped the delta pass for short non-English fillers while
    // never protecting the pronoun path for non-ASCII leads. Char count
    // keeps the ceiling's intent language-neutral.
    if word_count <= 4 && user.chars().count() <= 32 {
        // Final guard: if the short message LEADS with a first/second-person
        // pronoun, it might be a roleplay action ("I nod", "you see"). Fire
        // rather than risk skipping world state. Word-boundary match on the
        // first word (more precise than the old prefix list, and it can't
        // false-positive on "liege"/"lie down" style leads).
        let first = user.split_whitespace().next().unwrap_or("").to_lowercase();
        // "i'm"/"you're" → "i"/"you": the comparison word is the leading
        // alphanumeric run (the apostrophe + suffix drop off).
        let first_word: String =
            first.chars().take_while(|c| c.is_alphanumeric()).collect();
        const PRONOUNS: &[&str] = &[
            "i", "you", "u", "he", "she", "they", "we",
            // (2026-08-16 audit LOW) Apostrophe-LESS contractions — the chat
            // corpus drops them constantly ("Im tired", "Ill go"); without
            // these the gate misread real actions as filler.
            "im", "ill", "ive", "id", "ur", "youre", "youve", "youd", "lets",
        ];
        if PRONOUNS.contains(&first_word.as_str()) {
            return true; // ambiguous: fire to be safe
        }
        return false; // short, compact, no leading pronoun → filler, skip
    }
    // Everything else: fire. Long or substantive exchanges always get a pass.
    true
}

/// The FABLE-side delta gate (2026-08-24 fix — the fable path never fired a
/// schema delta at all; only the `[TIME]`-gated tick called `apply_delta`,
/// so `summary`/`recent_events` stayed empty forever on normal turns).
///
/// Thin wrapper over [`should_fire_delta`] with the two turn-shape facts the
/// fable tail knows and the chat gate doesn't:
/// - a CANCELLED turn never landed — no beat, nothing to record;
/// - the beat (`parsed.prose`) substitutes for the chat assistant reply.
///
/// The wrapper exists so the spawn site stays a one-line read + the gate is
/// unit-testable without a `fable_send` harness. Pure.
pub fn fable_delta_should_fire(cancelled: bool, user_action: &str, beat: &str) -> bool {
    if cancelled {
        return false;
    }
    should_fire_delta(user_action, beat)
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
- recent_events: append only genuinely new salient events.
- (2026-08-27 playtest M5) Strict JSON mechanics — ~20% of first-pass deltas were malformed: every key and string value is double-quoted on ONE line (escape inner quotes as \\\"), a colon separates each key from its value, commas separate entries, and the final entry carries NO trailing comma.\n";

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
  \"entities\": {\"<key>\": \"<new value>\", \"<key_to_delete>\": null},
  \"site_seeds\": {\"<designated site id>\": \"<one short line of what changed there>\"},
  \"site_ops\": {\"<departed site id>\": [{\"op\": \"set_asset|move_asset|remove_asset|add_asset\", ...}]},
  \"site_pressure\": {\"<any known site id>\": \"<one short line of where the site is headed>\"},
  \"wider_currents\": \"<=160 chars, one regional pattern beyond per-entity deltas\"
}

Rules:
- Emit ONLY changed keys. Omit unchanged sections entirely. If nothing \
plausibly moved, emit {}.
- site_seeds: ONLY for site ids listed under ## DESIGNATED SITES (if any), \
one short line each; omit the whole object when none moved or none were \
designated. \"Since last touched\" is the designation rotation (every offer \
resets it); \"last material change\" / \"NEVER materialized\" is how long since \
something actually happened there. A never-materialized or long-idle site is \
OVERDUE — prefer planting for it over a recently-changed neighbor.
- site_ops: ONLY for site ids listed under ## DEPARTED SITES (if any), each \
value a list of set_asset/move_asset/remove_asset/add_asset ops targeting the asset \
ids the section shows (add_asset mints a new id); terminal states are locked (dead stays dead, looted \
stays looted, disarmed stays disarmed); omit the whole object when none \
moved or none were listed.
- site_pressure: for any KNOWN site whose situation now points somewhere — \
designated, departed, or merely mentioned in the world state — one ≤140-char \
directional line of where it is heading next (a debt comes due, a camp \
swells, a feud cools). At most 6 entries total; omit the whole object when \
no site has real momentum. A line already carried as pending pressure is \
either fulfilled (site_seeds / site_ops answered it) or superseded (emit \
the newer line instead).
- entities: a null value means DELETE the key. A non-null string means SET.
- Pick 1-4 entities to advance per tick — the world moves in small ripples, \
not wholesale rewrites. Avoid touching the player's direct possessions \
or immediate scene state (those are the player's bubble).
- Advance trends causally, not linearly: an established shift may intensify, \
persist, plateau, fragment, transform, backfire, resolve, or reverse. A \
reversal needs an intelligible cause visible in the world state; escalation \
is never the default trajectory.
- Some entity keys may be flagged immutable (the canonical identity of an NPC, \
the foundational facts of a location). NEVER overwrite or delete those — \
record changes under NEW keys instead (e.g. append to a chronicle field).
- summary: only update when the macro state of the world meaningfully shifts.
- wider_currents: omit unless a regional pattern BEYOND any single entity's \
delta genuinely moved (a war's front shifts, a trade route collapses, a \
season turns); one line of at most 160 chars.
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
            surface: DeltaSurface::Chat,
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
            &[],
            &[],
        );
        assert!(prompt.contains("world simulation engine"));
        assert!(prompt.contains("24 hours"));
        assert!(prompt.contains("faction.cult.position"));
        assert!(prompt.contains("east_ridge"));
        // (2026-08-23) The trend-continuity law rides the always-on tick
        // instruction (escalation is never the default trajectory).
        assert!(prompt.contains("intensify"));
        assert!(prompt.starts_with("<|turn>system\n"));
        assert!(prompt.ends_with("<|turn>model\n"));
    }

    /// (2026-08-19 Stale Roulette) The designated-site section renders only
    /// when sites were designated, carries id + known seeds, and the output
    /// contract teaches `site_seeds`.
    #[test]
    fn world_progression_prompt_carries_designated_sites() {
        let designated = vec![DesignatedSite {
            id: "old-watchtower".into(),
            name: "The Old Watchtower".into(),
            elapsed_days: 30,
            idle_days: None,
            seeds: vec!["smoke seen on the horizon".into()],
            pressure: vec![],
        }];
        let prompt = render_world_progression_prompt("{}", 24, &[], &designated, &[]);
        assert!(prompt.contains("## DESIGNATED SITES"), "section missing");
        assert!(prompt.contains("old-watchtower"), "site id missing");
        assert!(prompt.contains("smoke seen on the horizon"), "seed context missing");
        assert!(prompt.contains("site_seeds"), "output contract missing");
        // Without designated sites the section is absent (zero tokens).
        let bare = render_world_progression_prompt("{}", 24, &[], &[], &[]);
        assert!(!bare.contains("## DESIGNATED SITES"), "section must be conditional");
    }

    /// (2026-08-23 starvation fix) The material-change read disambiguates the
    /// designation rotation from real change: a never-materialized site says
    /// so, a long-idle one carries its true idle age, and a recently-changed
    /// one stays quiet. The system instruction teaches the preference.
    #[test]
    fn world_progression_prompt_carries_material_idle() {
        let designated = vec![
            DesignatedSite {
                id: "old-watchtower".into(),
                name: "The Old Watchtower".into(),
                elapsed_days: 1,
                idle_days: None,
                seeds: vec![],
                pressure: vec![],
            },
            DesignatedSite {
                id: "fen-hollow".into(),
                name: "Fen Hollow".into(),
                elapsed_days: 1,
                idle_days: Some(30),
                seeds: vec![],
                pressure: vec![],
            },
            DesignatedSite {
                id: "mill-cross".into(),
                name: "Mill Cross".into(),
                elapsed_days: 1,
                idle_days: Some(0),
                seeds: vec![],
                pressure: vec![],
            },
        ];
        let prompt = render_world_progression_prompt("{}", 24, &[], &designated, &[]);
        assert!(
            prompt.contains("NEVER materialized"),
            "the never-materialized flag is the overdue signal: {prompt}"
        );
        assert!(
            prompt.contains("last material change ~30 day(s) ago"),
            "the idle age must survive the rotation reset: {prompt}"
        );
        assert_eq!(
            prompt.matches("last material change").count(),
            1,
            "only the 30-day-idle site carries the idle flag (changed-today stays quiet)"
        );
        assert!(
            prompt.contains("OVERDUE"),
            "the instruction teaches the preference: {prompt}"
        );
        assert!(
            prompt.contains("every offer resets it"),
            "the rotation-vs-material distinction is taught"
        );
    }

    /// (2026-08-22 multihog WS3) The pending-pressure read rides BOTH site
    /// sections (designated + departed), and the output contract teaches
    /// `site_pressure` for any known site.
    #[test]
    fn world_progression_prompt_carries_site_pressure() {
        let designated = vec![DesignatedSite {
            id: "old-watchtower".into(),
            name: "The Old Watchtower".into(),
            elapsed_days: 30,
            idle_days: None,
            seeds: vec![],
            pressure: vec!["the garrison's debt comes due".into()],
        }];
        let prompt = render_world_progression_prompt("{}", 24, &[], &designated, &[]);
        assert!(
            prompt.contains("pending pressure: the garrison's debt comes due"),
            "designated pressure missing: {prompt}"
        );
        let sites = vec![EvolutionSite {
            id: "goblin-camp".into(),
            name: "The Goblin Camp".into(),
            elapsed_days: 12,
            slice: "areas=gate:v".into(),
            pressure: vec!["the warband swells toward thirty".into()],
            threads: vec![],
        }];
        let prompt = render_world_progression_prompt("{}", 24, &[], &[], &sites);
        assert!(
            prompt.contains("Pending pressure: the warband swells toward thirty"),
            "evolution pressure missing: {prompt}"
        );
        // The output contract teaches the field (any site, bounded count).
        assert!(prompt.contains("site_pressure"));
        assert!(prompt.contains("at most 6 entries"), "the cap is taught");
        assert_eq!(SITE_PRESSURE_MAX_PER_TICK, 6, "the const matches the taught cap");
    }

    /// (2026-08-22 living-world) The departed-site EVOLUTION section: renders
    /// only when mapped departed sites were designated, carries the slice +
    /// the three-op grammar + the play-canon law, teaches `site_ops`, and
    /// stays inside the CTX_SCHEMA guard budget.
    #[test]
    fn world_progression_prompt_carries_departed_sites() {
        let sites = vec![EvolutionSite {
            id: "goblin-camp".into(),
            name: "The Goblin Camp".into(),
            elapsed_days: 12,
            slice: "areas=gate:v doors=pit:open,pit:v assets=shaman:active,warband:deadx4"
                .into(),
            pressure: vec![],
            threads: vec!["Bandit Chief (the player) — killed in the raid [day 2]".into()],
        }];
        let prompt = render_world_progression_prompt("{}", 24, &[], &[], &sites);
        assert!(prompt.contains("## DEPARTED SITES"), "section missing");
        assert!(prompt.contains("goblin-camp"), "site id missing");
        assert!(prompt.contains("shaman:active"), "slice (asset ids) missing");
        assert!(prompt.contains("set_asset"), "op grammar missing");
        assert!(prompt.contains("move_asset"));
        assert!(prompt.contains("remove_asset"));
        // (2026-08-24 Part II A2) The fourth op + its restock law.
        assert!(prompt.contains("add_asset"), "add_asset op grammar missing");
        assert!(prompt.contains("vacated site"), "restock law missing");
        assert!(
            prompt.contains("Moving or removing a carrier"),
            "carrier law missing"
        );
        assert!(prompt.contains("dead stays dead"), "play-canon law missing");
        assert!(prompt.contains("site_ops"), "output contract missing");
        // (2026-08-23) Civilizational activity + open-ways movement laws.
        assert!(prompt.contains("Living occupants act"), "activity law missing");
        assert!(prompt.contains("open ways"), "open-ways law missing");
        // (2026-08-23 WS5) The open causal threads render as live plots…
        assert!(
            prompt.contains("Open threads: Bandit Chief"),
            "thread ledger missing: {prompt}"
        );
        // …AFTER the section header (the 2026-08-23 out→section fix).
        let header_pos = prompt.find("## DEPARTED SITES").expect("header");
        let thread_pos = prompt.find("Open threads:").expect("threads");
        assert!(
            header_pos < thread_pos,
            "site bullets must follow the section header"
        );
        // Conditional: absent without evolution sites (zero tokens).
        let bare = render_world_progression_prompt("{}", 24, &[], &[], &[]);
        assert!(!bare.contains("## DEPARTED SITES"), "section must be conditional");
        // The budget guard: the whole section stays bounded even with
        // max-length slices + threads (the trim is real — every part of the
        // section, header, grammar, and site bullets, is inside it).
        let fat = vec![
            EvolutionSite {
                id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                name: "N".repeat(60),
                elapsed_days: 99,
                slice: "s".repeat(2_000),
                pressure: vec!["p".repeat(140)],
                threads: vec!["t".repeat(120), "u".repeat(120)],
            },
            EvolutionSite {
                id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                name: "M".repeat(60),
                elapsed_days: 99,
                slice: "t".repeat(2_000),
                pressure: vec![],
                threads: vec![],
            },
        ];
        let prompt = render_world_progression_prompt("{}", 24, &[], &[], &fat);
        let section = prompt
            .split("## DEPARTED SITES")
            .nth(1)
            .expect("section present");
        assert!(
            section.chars().count() <= SITE_EVOLUTION_PROMPT_BUDGET_CHARS + 4,
            "section must stay inside the CTX_SCHEMA guard budget ({} chars)",
            section.chars().count()
        );
    }

    /// (2026-08-24 review P1) The COMPOSED whole-prompt guard — the missing
    /// pin the section-level budget above couldn't provide. Every piece of
    /// the world-progression tick prompt is individually capped (schema JSON
    /// ≤ `SCHEMA_JSON_PROMPT_BUDGET_CHARS`, evolution section ≤ its own
    /// budget, deferred errors ≤ 400, triggers ≤ 200), but the 2026-08-24
    /// measurement showed the TOTAL (instruction 3,557 + JSON 4,000 + framing
    /// + interval ~500 ≈ 8.1k chars base) overflowed the OLD 2048-token
    /// context's 1,792-token prompt ceiling even with no optional sections —
    /// the middle-drop fired on healthy long campaigns. This pin composes
    /// the worst legal prompt AT the caps and asserts it fits the CURRENT
    /// context's prompt ceiling on the conservative 3.6 chars/token density
    /// (the engine's own middle-drop math is token-exact at decode time;
    /// this is the shipping gate that trips when any piece grows). If this
    /// fails: shrink a piece or raise CTX_SCHEMA (Chloe's call) — never let
    /// it ship red.
    #[test]
    fn world_progression_prompt_worst_case_fits_schema_context() {
        // Max-length schema JSON (at the renderer's own budget).
        let schema_json = "e".repeat(crate::settings::SCHEMA_JSON_PROMPT_BUDGET_CHARS);
        // Deferred attempts at the lib.rs queue cap, each with a
        // max-capped error string + max trigger.
        let deferred: Vec<FailedAttempt> = (0..crate::MAX_FAILED_DELTA_ATTEMPTS)
            .map(|i| FailedAttempt {
                exchange: Some((format!("u{i}"), format!("m{i}"))),
                trigger: Some("t".repeat(200)),
                errors: "e".repeat(400),
                passes_used: MAX_DELTA_PASSES,
                surface: DeltaSurface::Fable,
            })
            .collect();
        // Designated sites with seeds + pressure (the fat shapes).
        let designated = vec![
            DesignatedSite {
                id: "d".repeat(32),
                name: "N".repeat(60),
                elapsed_days: 99,
                idle_days: Some(99),
                seeds: vec!["s".repeat(140)],
                pressure: vec!["p".repeat(140)],
            },
            DesignatedSite {
                id: "x".repeat(32),
                name: "M".repeat(60),
                elapsed_days: 99,
                idle_days: None,
                seeds: vec!["s".repeat(140), "s2".repeat(70)],
                pressure: vec!["p".repeat(140)],
            },
        ];
        // Evolution sites sized to saturate the section budget (same fat
        // shape the section-level pin uses).
        let evolution = vec![
            EvolutionSite {
                id: "a".repeat(32),
                name: "N".repeat(60),
                elapsed_days: 99,
                slice: "s".repeat(2_000),
                pressure: vec!["p".repeat(140)],
                threads: vec!["t".repeat(120), "u".repeat(120)],
            },
            EvolutionSite {
                id: "b".repeat(32),
                name: "M".repeat(60),
                elapsed_days: 99,
                slice: "t".repeat(2_000),
                pressure: vec![],
                threads: vec![],
            },
        ];
        let prompt = render_world_progression_prompt(
            &schema_json,
            24,
            &deferred,
            &designated,
            &evolution,
        );
        let chars = prompt.chars().count();
        let max_prompt_tokens = (SCHEMA_CTX as usize).saturating_sub(SCHEMA_MAX_TOKENS as usize);
        let char_budget = (max_prompt_tokens as f32
            * crate::settings::TRACKER_PROMPT_CHARS_PER_TOKEN) as usize;
        assert!(
            chars <= char_budget,
            "composed world-progression prompt ({} chars) exceeds the {}-char \
             budget for CTX_SCHEMA={} − SCHEMA_MAX_TOKENS={} at {} chars/token — \
             shrink a capped piece or raise CTX_SCHEMA",
            chars,
            char_budget,
            SCHEMA_CTX,
            SCHEMA_MAX_TOKENS,
            crate::settings::TRACKER_PROMPT_CHARS_PER_TOKEN
        );
    }

    /// (2026-08-24 review) The deferred-error cap: a fat `errors` string must
    /// render capped with the visible truncation marker in BOTH prompt paths
    /// (delta + world-progression).
    #[test]
    fn deferred_attempt_errors_render_capped() {
        // 'û' appears nowhere in the prompt prose, so counting it isolates
        // the rendered error body exactly.
        let deferred = vec![FailedAttempt {
            exchange: None,
            trigger: Some("trigger".into()),
            errors: "û".repeat(2_000),
            passes_used: MAX_DELTA_PASSES,
            surface: DeltaSurface::Chat,
        }];
        let tick = render_world_progression_prompt("{}", 24, &deferred, &[], &[]);
        assert!(
            tick.matches('û').count() <= 400,
            "errors must be capped in the tick prompt (got {} chars)",
            tick.matches('û').count()
        );
        assert!(tick.contains("prior errors: ûûûû"), "error head renders");
        assert!(tick.contains("[…]"), "truncation marker present: {tick:?}");
        assert!(tick.contains("trigger"), "trigger renders");
    }

    #[test]
    fn world_progression_prompt_instructs_off_screen_subset() {
        // Critical framing: the model must advance a SUBSET, not rewrite the
        // world wholesale, and must NOT touch the player's bubble.
        let prompt = render_world_progression_prompt("{}", 12, &[], &[], &[]);
        assert!(prompt.contains("SUBSET"));
        assert!(prompt.contains("off-screen"));
        assert!(prompt.contains("the player's direct possessions"));
        // (2026-08-24 Part II A1) Trend discipline in the USER prompt — a
        // pressure may resolve or reverse, escalation is never automatic,
        // and oscillation is barred.
        assert!(prompt.contains("escalation"), "trend law missing");
        assert!(prompt.contains("reverse"), "trend law missing");
        assert!(prompt.contains("oscillation"), "trend law missing");
        // (2026-08-24 Part II A6) The wider-currents output contract.
        assert!(prompt.contains("wider_currents"), "output contract missing");
    }

    #[test]
    fn world_progression_prompt_mentions_immutability_rule() {
        // The system instruction must warn about immutable keys so the model
        // doesn't try to overwrite canon (the validator catches it, but the
        // instruction prevents wasted passes).
        let prompt = render_world_progression_prompt("{}", 24, &[], &[], &[]);
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
            surface: DeltaSurface::Fable,
        }];
        let prompt = render_world_progression_prompt("{}", 24, &deferred, &[], &[]);
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

    // ---- (2026-08-22 multihog WS5) full-emit fallback ---------------------

    /// The pass contract: exactly 2 delta passes before the fallback, and
    /// the failure carrier reports 3 (2 delta + 1 full-emit) — the number
    /// `bump_retry_budget`'s MAX_TOTAL_PASSES = 5 arithmetic was built on.
    #[test]
    fn full_emit_pass_contract_is_two_delta_plus_one_fallback() {
        assert_eq!(MAX_DELTA_PASSES, 2, "2 delta passes, then the fallback");
        assert_eq!(MAX_DELTA_PASSES + 1, 3, "the carrier's first-enqueue passes_used");
    }

    /// The fallback prompt asks for the flat key → COMPLETE-new-value shape
    /// and carries the prior attempts' outputs + errors for reference.
    #[test]
    fn full_emit_prompt_render() {
        let prompt = render_full_emit_prompt(
            &["{\"entities\": [broken".to_string()],
            &["pass 1 JSON parse: eof".to_string()],
        );
        assert!(prompt.contains("COMPLETE new value"));
        assert!(prompt.contains("null"));
        assert!(prompt.contains("OMIT every unchanged key"));
        assert!(prompt.contains("broken"));
        assert!(prompt.contains("pass 1 JSON parse"));
        assert!(prompt.starts_with("<|turn>system\n"));
        assert!(prompt.ends_with("<|turn>model\n"));
    }

    /// Diff semantics: omission = unchanged (only emitted keys reach the
    /// delta), explicit null = delete, string values are control-char
    /// stripped + clamped, structured values pass through whole.
    #[test]
    fn full_emit_diff_semantics() {
        let immutable = std::collections::HashSet::new();
        let existing: std::collections::HashSet<String> =
            ["known.key".to_string()].into_iter().collect();
        let raw = r#"```json
{"known.key": "the new truth",
 "gone.key": null,
 "structured.key": {"progress": 3, "target": 5},
 "dirty.key": "line one\nline two\ttabbed",
 "long.key": "y"}
```"#;
        let delta = full_emit_to_delta(raw, &immutable, &existing).expect("parses + validates");
        let ents = delta.entities.expect("entities present");
        assert_eq!(ents.len(), 5, "only emitted keys (omission = unchanged)");
        assert_eq!(
            ents.get("known.key").and_then(|o| o.as_ref().and_then(|v| v.as_str())),
            Some("the new truth")
        );
        assert_eq!(ents.get("gone.key"), Some(&None), "explicit null = delete");
        assert_eq!(
            ents.get("structured.key")
                .and_then(|o| o.as_ref())
                .and_then(|v| v.get("progress"))
                .and_then(|v| v.as_u64()),
            Some(3),
            "structured values pass through whole"
        );
        assert_eq!(
            ents.get("dirty.key").and_then(|o| o.as_ref().and_then(|v| v.as_str())),
            Some("line one line two tabbed"),
            "control chars flatten inside string values"
        );
        // No summary/events on the fallback path (entities only).
        assert!(delta.summary.is_none());
        assert!(delta.recent_events.is_none());

        // An oversize STRING clamps to the validator cap (normalization,
        // not rejection); an oversize STRUCTURED serialization errs so the
        // deferred-retry path takes it (never a silent structure corrupt).
        let huge_string = format!("\"{}\"", "v".repeat(schema_validator::MAX_VALUE_LEN + 500));
        let raw2 = format!("{{\"big.key\": {huge_string}}}");
        let delta2 = full_emit_to_delta(&raw2, &immutable, &existing).expect("string clamps");
        let clamped = delta2
            .entities
            .unwrap()
            .get("big.key")
            .and_then(|o| o.as_ref().and_then(|v| v.as_str()))
            .unwrap()
            .to_string();
        assert_eq!(clamped.chars().count(), schema_validator::MAX_VALUE_LEN);
    }

    /// The immutability lock is enforced NATURALLY on the fallback path: an
    /// overwrite of a locked existing key errs with the validator's
    /// wording, so the fallback can never do what the delta passes couldn't.
    #[test]
    fn full_emit_respects_immutability() {
        let immutable: std::collections::HashSet<String> =
            ["npc.marcus.core".to_string()].into_iter().collect();
        let existing: std::collections::HashSet<String> =
            ["npc.marcus.core".to_string()].into_iter().collect();
        let raw = r#"{"npc.marcus.core": "retconned"}"#;
        let err = full_emit_to_delta(raw, &immutable, &existing).expect_err("must refuse");
        assert!(err.to_lowercase().contains("immutable"), "error explains: {err}");
        // Unparseable output errs with full-emit provenance for the queue.
        let err2 = full_emit_to_delta("total garbage [ [ [", &immutable, &existing)
            .expect_err("must refuse");
        assert!(err2.contains("JSON parse") || err2.contains("no JSON object"), "{err2}");
        // An empty object carries nothing → err (the deferred path takes it).
        let err3 = full_emit_to_delta("{}", &immutable, &existing).expect_err("must refuse");
        assert!(err3.contains("no entity keys"), "{err3}");
    }

    /// (2026-08-23 audit fix) The reserved envelope keys are never minted as
    /// entities: a model answering the fallback in the delta-ENVELOPE shape
    /// (`{"summary":…, "entities":{…}}`) used to create grime keys literally
    /// named `summary`/`entities` that ride every later prompt. A lone
    /// `entities` object unwraps into the flat map we asked for.
    #[test]
    fn full_emit_skips_envelope_keys_and_unwraps_entities() {
        let immutable = std::collections::HashSet::new();
        let existing: std::collections::HashSet<String> =
            ["known.key".to_string()].into_iter().collect();
        // Envelope shape: inner entities survive, envelope keys don't.
        let raw = r#"{"summary": "the party rested", "recent_events": ["a"], "entities": {"known.key": "v2"}, "site_ops": {}}"#;
        let delta = full_emit_to_delta(raw, &immutable, &existing).expect("parses");
        let ents = delta.entities.expect("entities present");
        assert_eq!(ents.len(), 1, "only the inner entity key survives: {ents:?}");
        assert!(ents.contains_key("known.key"));
        // A lone `entities` envelope unwraps.
        let raw2 = r#"{"entities": {"known.key": "v3"}}"#;
        let delta2 = full_emit_to_delta(raw2, &immutable, &existing).expect("unwraps");
        let ents2 = delta2.entities.expect("entities present");
        assert_eq!(ents2.len(), 1, "{ents2:?}");
        assert!(ents2.contains_key("known.key"));
        // Envelope-only output carries NO entities → the deferred path.
        let raw3 = r#"{"summary": "nothing but prose"}"#;
        let err = full_emit_to_delta(raw3, &immutable, &existing).expect_err("must refuse");
        assert!(err.contains("no entity keys"), "{err}");
    }

    // The gate is the M2 overhead fix: skip the full local-model forward pass on
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

    // ---------- fable_delta_should_fire (2026-08-24 fix) ----------

    #[test]
    fn fable_gate_never_fires_on_cancelled_turn() {
        // A cancelled turn never landed — no beat exists, no matter what the
        // half-built prose strings say.
        assert!(!fable_delta_should_fire(
            true,
            "I walk to the harbor district",
            "The docks creak underfoot as gulls wheel overhead."
        ));
    }

    #[test]
    fn fable_gate_skips_empty_beat() {
        assert!(!fable_delta_should_fire(false, "I enter the tavern", ""));
        assert!(!fable_delta_should_fire(false, "I enter the tavern", "   "));
    }

    #[test]
    fn fable_gate_fires_on_substantive_turn() {
        assert!(fable_delta_should_fire(
            false,
            "I ask the barkeep about the missing courier",
            "He lowers his voice and glances at the door before answering."
        ));
    }

    #[test]
    fn fable_gate_inherits_filler_skip() {
        // The wrapper delegates to the chat gate verbatim: short filler
        // player actions with no beat substance skip exactly as chat does.
        assert!(!fable_delta_should_fire(false, "ok", "She waits, patient."));
    }
}
