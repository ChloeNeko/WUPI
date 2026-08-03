//! The FableEngine: the Narrator's dedicated generation thread (Games app Seam 2).
//!
//! A dedicated `std::thread` ("wupi-fable") owning an ISOLATED
//! `LlamaContext<'static>` on the same `WUPI.gguf` model the chat engine
//! uses. The narrator's roleplay turns run here, fully isolated from the
//! Wupi-assistant chat context (and from the schema/embedder contexts).
//!
//! # Why a fourth context (the load-bearing isolation)
//!
//! The Games app design (docs/games-app-design.md §1.1) is built on
//! DUAL-CONTEXT: Wupi-as-game-manager (her chat context) must be available
//! *while* the Narrator is mid-scene. The two cannot share a context: they
//! are different personas with different system prompts and different KV
//! state. A fourth isolated `LlamaContext` on the same leaked model
//! accomplishes this. Mirrors the schema engine pattern (§2J) and the
//! embedder pattern (§3B). Under the v0.6.4 VRAM swap-lock (§2B), only ONE
//! of {chat, schema, fable} is resident at a time; the embedder is exempt.
//! **chat (4000) + embedder (512) + schema (2048) + game (3072)** share one
//! leaked `&'static LlamaModel` + one `shared_backend()`. Weights (~9.8 GB)
//! + embedder (~36 MB) + one resident context (~75-150 MB Q8_0 KV) ≈ ~10 GB
//! → ~2 GB headroom on 12 GB (game context cut 4000 → 3072 on 2026-07-26).
//!
//! # Streaming, not one-shot
//!
//! Unlike the schema engine (which returns a single JSON blob), the game
//! engine streams tokens to a Tauri Channel via the same `ChunkFn` callback
//! type the chat engine uses (`llm::ChunkFn`). The caller (`game_send` IPC)
//! wraps the Channel's `send` into the chunk callback, the same way
//! `chat_send` does. Bracket commands (`[CHARACTER_TURN:...]`, `[OBJECT ...]`,
//! `[FX ...]`) ride alongside prose as `type: "scene_event"` Channel messages
//! (parsed by the `BracketCommand` extractor in `stream_filter.rs`).
//!
//! # Lifecycle
//!
//! NOT eager-spawned at boot. Spawns on `game_start` (when the user picks a
//! roleplay card), shuts down on `game_end`. Costs VRAM only while a game is
//! actually running. Mirrors `SchemaEngine`'s handle shape (`mpsc::Sender` +
//! `Mutex<Option<JoinHandle>>` + `unsafe impl Send+Sync`).

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use llama_cpp_2::context::params::{KvCacheType, LlamaContextParams};
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::logit_bias::LlamaLogitBias;
use llama_cpp_2::token::LlamaToken;

use crate::llm::{shared_backend, shared_model, CancelToken, ChunkFn};

/// The game context's token budget.
///
/// **History:** originally 4000; cut to 3072 on 2026-07-26 (§2C) to free VRAM
/// headroom on 12 GB GPUs (~300-400 MiB saved). At the time the narrator
/// system prompt was ~1000 tokens, so 3072 fit comfortably.
///
/// **Held at 4096 (2026-07-29).** Originally 4000, cut to 3072 on 2026-07-26
/// (§2C) for VRAM headroom, then raised to 4096 on 2026-07-28 after the
/// Phase 3 wiring playtest found the narrator system prompt had grown to
/// ~5000 tokens (narrator_core 7.7KB + BRACKET_PROTOCOL 4.2KB after the
/// Phase 3 anti-Oblivion + threat-tiers + RP-conventions + 3-new-bracket-
/// command additions). At FABLE_CTX=3072 the front-truncation guard had
/// chopped ~2900 tokens off the front of the system prompt, breaking the
/// model's attention and producing the "hyphen-for-space" prose degradation
/// documented in §11.26.
///
/// **2026-07-29 regression + correct fix (EXECUTED).** Phase 4's Component
/// 2/3/4 prompt additions re-bloated BRACKET_PROTOCOL alone from ~4.2KB to
/// ~13.6KB (~3400 tokens); combined with narrator_core (~1900 tok) the
/// rendered LOCAL narrator prompt measured ~5770 tokens vs the 3072 prompt
/// budget. The front-truncation guard then `drain(0..2698)`'d the ENTIRE
/// system prompt, so the local 12B emitted raw JSON as narration (the
/// §11.38 Bug A recurrence). The unit tests missed this because they verify
/// prompt CONSTRUCTION, not generation under the ctx budget. **The correct
/// fix — executed 2026-07-29 in the prompt-distillation scrub — was to
/// SHRINK the prompt, not raise FABLE_CTX.** narrator_core + BRACKET_PROTOCOL
/// were distilled to lean declarative laws (the 200-word anti-bias lectures
/// → one line each; full bracket semantics + COMMON MISTAKES offloaded to
/// the unified fable.codex, retrieved on-demand via search_fable_visible),
/// recovering ~3000 tokens. The full narrator prompt is now ~2050 tok;
/// FABLE_MAX_TOKENS restored 512→1024 and the LOCAL window 6→8 (the
/// shortcut amputations reversed). FABLE_CTX stays at 4096.
///
/// **VRAM cost:** the Q8_0 KV cache at n_ctx=4096 is ~680 MiB (40 layers,
/// K+V q8_0, measured live on the RTX 5070 Ti Laptop 12 GB). Under the §2B
/// swap-lock only ONE of {chat, schema, fable} is resident during a turn,
/// so worst case is weights (~9.8 GB) + fable KV (680 MiB) + embed
/// (34 MiB, always resident) + compute buffer (~530 MiB) ≈ 11.0 GB of
/// 12 GB → ~1 GB headroom. Stable.
///
/// The API path uses a wider 16-message window (the cloud model has the
/// budget). The front-truncation guard below protects against overflow on
/// the rare turn where the prompt exceeds `FABLE_CTX - FABLE_MAX_TOKENS`.
const FABLE_CTX: u32 = crate::settings::CTX_FABLE;
const FABLE_BATCH: u32 = 512;
/// Cap on generated tokens for a single narrator turn.
///
/// **1024 (a CEILING, not a quota — 2026-07-29).** A narrative beat is 2-4
/// paragraphs (~300-450 tokens of prose + bracket commands); the lean
/// declarative-law narrator prompt (post the 2026-07-29 distillation scrub)
/// produces punchy beats naturally and stops when the beat is done. The
/// §11.41 DRY sampler + post-gen `truncate_repetition` truncator are the
/// mechanical backstop against token-sequence loops. The clamp (engine.rs
/// pattern) further bounds this by `n_ctx - n_cur` at decode time.
///
/// History: was 1024 from v1 through 2026-07-28. Cut in half 2026-07-29
/// after the Phase 4 consolidated playtest proved 1024 enabled the loop
/// failure mode (a symptom of the prompt bloat — the 3,364-token system
/// prompt left the model filling space). RESTORED to 1024 on 2026-07-29
/// after the prompt-distillation scrub recovered ~3,000 tokens. The cut
/// was a shortcut that amputated generation budget to make room for bloat;
/// the scrub fixed the bloat instead.
const FABLE_MAX_TOKENS: i32 = 1024;

// ---------------------------------------------------------------------------
// Control plane: channel types
// ---------------------------------------------------------------------------

/// A request to the game thread: stream a narrator turn for `prompt`.
struct FableRequest {
    /// Fully-rendered prompt (system + visible history + new user turn +
    /// generation prompt). The engine tokenizes + prefills + decodes it.
    prompt: String,
    /// Streaming callback: invoked once per decoded token piece. Wraps the
    /// Tauri Channel's `send` (mirrors `chat_send`'s `on_chunk`).
    on_chunk: ChunkFn,
    /// Per-request cancellation token. The decode loop checks
    /// `cancel.load(Relaxed)` between tokens (same pattern as the chat
    /// engine, §2C). Distinct slot from `active_cancel` so game/chat cancels
    /// never cross-wire.
    cancel: CancelToken,
    /// §11.43.B (2026-07-28): when true, the engine uses the DETERMINISTIC
    /// "tracker" sampler chain (temp 0.2, top_p 0.9, DRY allowed_length=1)
    /// instead of the creative narrator chain (temp 0.85, top_p 0.95, DRY
    /// allowed_length=2). The tracker is an AGENT — it emits rigid
    /// bracket/JSON state deltas, never prose — so it gets no stylistic
    /// leeway and a tighter DRY to kill single-token loops like the
    /// `(player)(player)` pathology (which slips past the narrator's
    /// allowed_length=2 because the loop spans only 1-2 tokens per repeat).
    /// The §11.42 API-mode tracker call site sets this true; every other
    /// caller (LOCAL-mode narrator, API-mode fallback narrator) leaves it
    /// false → existing behavior unchanged.
    tracker_mode: bool,
    /// One-shot reply channel. Sent exactly once when the turn completes
    /// (success, cancel, or error).
    reply: mpsc::Sender<FableReply>,
}

/// What the game thread sends back when a narrator turn completes. Carries
/// the full cleaned text + raw model output + any bracket commands the
/// parser extracted. On error, `error` is populated and the others are empty.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FableReply {
    /// The verbatim model output (post generation, pre-cleanup). Empty on
    /// generation failure.
    pub raw_output: String,
    /// Human-readable error if the turn failed. Empty on success.
    pub error: String,
    /// True if the turn was cancelled mid-generation (`game_stop`). The
    /// caller decides whether to persist a partial reply.
    pub cancelled: bool,
}

enum FableMsg {
    Request(Box<FableRequest>),
    Shutdown,
}

// ---------------------------------------------------------------------------
// Handle (held by callers; fully Send + Sync)
// ---------------------------------------------------------------------------

/// The handle callers hold. Fully `Send + Sync`: a channel sender + the
/// thread's JoinHandle so `shutdown()` can block until VRAM is actually freed
/// (same load-bearing concern as `SchemaEngine`: the next `game_start` must
/// not race the previous `game_end`'s VRAM teardown). Mirrors `SchemaEngine`
/// and `LlamaCppEmbedder`.
pub struct FableEngine {
    tx: mpsc::Sender<FableMsg>,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

// SAFETY: mpsc::Sender<FableMsg> is Send (FableMsg owns only Send data).
// Mutex<Option<JoinHandle<()>>> is Send+Sync. No `LlamaContext` crosses out.
unsafe impl Send for FableEngine {}
unsafe impl Sync for FableEngine {}

/// The per-turn sampler parameters that differ between the tracker and the
/// narrator. Extracted to a pure helper so the §11.43.B "LOCAL mode is
/// unchanged" contract can be unit-tested without inspecting `LlamaSampler`
/// (whose stage list the llama_cpp_2 crate doesn't expose).
///
/// The `false` branch MUST stay byte-identical to the pre-§11.43.B chain —
/// that's the "LOCAL mode unchanged" contract. The `sampler_config_returns_
/// narrator_defaults_for_local_mode` test pins it.
#[derive(Debug, PartialEq)]
struct SamplerConfig {
    temp: f32,
    top_p: f32,
    dry_multiplier: f32,
    dry_base: f32,
    /// `allowed_length` for the DRY stage. The narrator keeps the §11.41
    /// value of 2 (preserves rhetorical anaphora); the tracker tightens to
    /// 1 (kills single-token loops like `(player)(player)`).
    dry_allowed_length: i32,
}

/// The punctuation logit-bias table applied to the narrator + tracker sampler
/// chains (Prong 2 of the LOCAL Phase 4 fix, 2026-07-29). Each entry is a
/// (token-text, bias) pair resolved against the live model at sampler-build
/// time via `str_to_token`. Returns a `&'static` slice so it is trivially
/// unit-testable without a loaded model.
///
/// Rationale (per the blueprint):
/// - Hyphen `"-"` / `" -"`: -10.0 (the original §11.40 Bug B defense —
///   suppresses the hyphen-spam attractor).
/// - Comma `","`: -1.0 (mild — still available for natural sentence
///   structure, but kills the comma-splicing attractor from Finding B).
/// - Semicolon `";"`: -10.0 (heavy — semicolons invite run-on attention).
/// - En-dash `"\u{2013}"` / Em-dash `"\u{2014}"`: -100.0 (hard ban — these
///   are garbage tokens for this model that cause run-on sentences; nuke
///   them from the distribution).
///
/// NOTE on multi-token characters: `str_to_token` returns a `Vec`; the
/// caller resolves only the FIRST token of each entry. For ASCII punctuation
/// that is the whole token. For the non-ASCII en/em-dashes Gemma's tokenizer
/// may split them into 2-3 sub-tokens; biasing the lead token makes the
/// sequence near-unreachable (each subsequent sub-token carries its own
/// probability mass), which achieves the blueprint's "nuke them completely"
/// intent. A future pass can bias all sub-tokens if the lead-token bias
/// proves insufficient under live play.
fn punct_bias_table() -> &'static [(&'static str, f32)] {
    &[
        ("-", -10.0),         // §11.40 Bug B — hyphen-spam defense (preserved)
        (" -", -10.0),        // leading-space hyphen (same attractor)
        (",", -1.0),          // mild — kills comma-splicing, keeps natural commas
        (";", -10.0),         // heavy — semicolons invite run-ons
        ("\u{2013}", -100.0), // en-dash — hard ban
        ("\u{2014}", -100.0), // em-dash — hard ban
    ]
}

/// Resolve the punctuation bias table against the live model into concrete
/// `LlamaLogitBias` pairs. Each table entry maps to (at most) one bias on the
/// lead token; entries whose text doesn't tokenize are silently dropped (the
/// `filter_map` mirrors the legacy inline pattern). Pure given a model ref —
/// testable via `shared_model()` once a model is loaded.
fn resolve_punct_biases(model: &LlamaModel) -> Vec<LlamaLogitBias> {
    punct_bias_table()
        .iter()
        .filter_map(|(s, bias)| {
            model
                .str_to_token(s, AddBos::Never)
                .ok()
                .and_then(|v| v.first().copied())
                .map(|t| LlamaLogitBias::new(t, *bias))
        })
        .collect()
}

/// Returns the sampler profile for the requested turn mode. See
/// [`SamplerConfig`] + the `sampler_config_*` tests for the pinned values.
fn sampler_config(tracker_mode: bool) -> SamplerConfig {
    if tracker_mode {
        SamplerConfig {
            temp: crate::settings::TEMP_TRACKER,
            top_p: crate::settings::TOP_P_TRACKER,
            dry_multiplier: crate::settings::DRY_MULT,
            dry_base: crate::settings::DRY_BASE,
            dry_allowed_length: crate::settings::DRY_ALLOWED_LEN_TRACKER,
        }
    } else {
        // The narrator profile. Sourced from `settings.rs` (the single source
        // of truth); the `sampler_config_returns_narrator_defaults_for_local_mode`
        // test pins them so any drift is caught. If you change these, update
        // the test + AGENTS.md §11.41 + §11.43.B docs together.
        SamplerConfig {
            temp: crate::settings::TEMP_NARRATOR,
            top_p: crate::settings::TOP_P_NARRATOR,
            dry_multiplier: crate::settings::DRY_MULT,
            dry_base: crate::settings::DRY_BASE,
            dry_allowed_length: crate::settings::DRY_ALLOWED_LEN_NARRATOR,
        }
    }
}

impl FableEngine {
    /// Spawn the game thread. Loads `WUPI.gguf` (or whatever path resolves)
    /// as this engine's OWN model: freshly leaked `&'static`, independent
    /// KV state. The readiness receiver yields `Ok(())` once the context is
    /// live (or `Err` if init failed: the caller should treat the engine as
    /// unavailable, same contract as `SchemaEngine::spawn_load`).
    pub fn spawn_load(
        path: PathBuf,
        n_gpu_layers: u32,
    ) -> (Self, mpsc::Receiver<Result<(), String>>) {
        let (tx, rx) = mpsc::channel::<FableMsg>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();

        let builder = std::thread::Builder::new().name("wupi-fable".into());
        let join = builder
            .spawn(move || {
                let mut runtime = match Self::init_runtime(&path, n_gpu_layers) {
                    Ok(rt) => {
                        let _ = init_tx.send(Ok(()));
                        rt
                    }
                    Err(e) => {
                        let msg = format!("game engine init failed: {e}");
                        tracing::error!(error = %msg, "game engine init failed; thread exiting");
                        let _ = init_tx.send(Err(msg.clone()));
                        Self::drain_failed(&rx, msg);
                        return;
                    }
                };
                tracing::info!("wupi-fable thread ready");

                loop {
                    match rx.recv() {
                        Ok(FableMsg::Request(req)) => {
                            // Self-healing: isolate each turn so one panic
                            // doesn't kill the thread.
                            let outcome = std::panic::catch_unwind(
                                std::panic::AssertUnwindSafe(|| {
                                    runtime.generate_turn(&req)
                                }),
                            );
                            let reply_msg = match outcome {
                                Ok(Ok(raw)) => FableReply {
                                    raw_output: raw,
                                    error: String::new(),
                                    cancelled: false,
                                },
                                Ok(Err(GenerationOutcome::Cancelled(raw))) => FableReply {
                                    raw_output: raw,
                                    error: String::new(),
                                    cancelled: true,
                                },
                                Ok(Err(GenerationOutcome::GenerationErr(e))) => {
                                    tracing::warn!(error = %format!("{e:#}"), "game turn failed");
                                    runtime.ctx.clear_kv_cache();
                                    FableReply {
                                        raw_output: String::new(),
                                        error: format!("{e:#}"),
                                        cancelled: false,
                                    }
                                }
                                Err(payload) => {
                                    let msg = payload
                                        .downcast_ref::<String>()
                                        .map(|s| s.clone())
                                        .or_else(|| {
                                            payload.downcast_ref::<&str>().map(|s| s.to_string())
                                        })
                                        .unwrap_or_else(|| {
                                            "game turn panic (unknown cause)".to_string()
                                        });
                                    tracing::error!(panic = %msg, "game turn panicked");
                                    runtime.ctx.clear_kv_cache();
                                    FableReply {
                                        raw_output: String::new(),
                                        error: format!("game panic: {msg}"),
                                        cancelled: false,
                                    }
                                }
                            };
                            let _ = req.reply.send(reply_msg);
                        }
                        Ok(FableMsg::Shutdown) => {
                            tracing::info!("wupi-fable shutting down");
                            break;
                        }
                        Err(mpsc::RecvError) => {
                            tracing::info!("wupi-fable: all senders dropped, exiting");
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn wupi-fable thread");

        (
            FableEngine {
                tx,
                join: std::sync::Mutex::new(Some(join)),
            },
            init_rx,
        )
    }

    /// Shut down the game thread and block until VRAM is freed. Same
    /// load-bearing concern as `SchemaEngine::shutdown`: required so the
    /// next `game_start` doesn't race the teardown.
    pub fn shutdown(&self) {
        let _ = self.tx.send(FableMsg::Shutdown);
        if let Ok(mut guard) = self.join.lock() {
            if let Some(handle) = guard.take() {
                if let Err(e) = handle.join() {
                    tracing::warn!(error = ?e, "wupi-fable thread join failed during shutdown");
                }
            }
        }
    }

    /// Post a narrator turn request. The caller awaits the reply via the
    /// receiver it created. The streaming chunks arrive via `on_chunk` *as
    /// they decode*: the reply comes once when generation completes.
    ///
    /// `tracker_mode` (§11.43.B): when true, selects the deterministic
    /// tracker sampler chain. See [`FableRequest::tracker_mode`] for the
    /// full rationale. Callers that don't care should pass `false` to get
    /// the default narrator behavior.
    pub fn request_turn(
        &self,
        prompt: String,
        on_chunk: ChunkFn,
        cancel: CancelToken,
        tracker_mode: bool,
    ) -> anyhow::Result<mpsc::Receiver<FableReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<FableReply>();
        let req = FableRequest {
            prompt,
            on_chunk,
            cancel,
            tracker_mode,
            reply: reply_tx,
        };
        self.tx
            .send(FableMsg::Request(Box::new(req)))
            .map_err(|_| anyhow::anyhow!("game engine thread closed"))?;
        Ok(reply_rx)
    }

    /// Drain any queued requests after a failed init so callers don't block
    /// forever waiting on a reply from a dead thread. Mirrors
    /// `SchemaEngine::drain_failed`.
    fn drain_failed(rx: &mpsc::Receiver<FableMsg>, why: String) {
        while let Ok(msg) = rx.recv_timeout(std::time::Duration::from_millis(50)) {
            if let FableMsg::Request(req) = msg {
                let _ = req.reply.send(FableReply {
                    raw_output: String::new(),
                    error: why.clone(),
                    cancelled: false,
                });
            }
        }
    }

    /// Initialize the game runtime. Prefers the chat engine's already-loaded
    /// `&'static LlamaModel` via `shared_model()`: sharing weights is the
    /// ONLY way four contexts (chat 4000 + embedder 512 + schema 2048 +
    /// game 4000) fit on a 12GB GPU. Loading a second 12B copy would OOM
    /// (the 2026-07-18 `NullResult` lesson). The `path` arg is kept for
    /// forward-compat (a future dedicated narrator model); it's only used
    /// if `shared_model()` returns `None`.
    fn init_runtime(path: &Path, n_gpu_layers: u32) -> anyhow::Result<FableRuntime> {
        let backend = shared_backend();

        // Prefer the shared model (the load-bearing path: avoids VRAM OOM).
        // Only load a separate copy if there's no shared model to reuse
        // (e.g. API mode where the chat engine's local model is torn down).
        let model_ref: &'static LlamaModel = match shared_model() {
            Some(m) => {
                tracing::info!("game engine reusing shared chat model (VRAM-efficient)");
                m
            }
            None => {
                tracing::warn!(
                    "no shared model available; game engine loading its own copy (may OOM)"
                );
                let params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
                let model = LlamaModel::load_from_file(backend, path, &params)
                    .map_err(|e| anyhow::anyhow!("game model load {}: {e:?}", path.display()))?;
                tracing::info!(path = %path.display(), "game model loaded (own copy)");
                Box::leak(Box::new(model))
            }
        };

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(std::num::NonZeroU32::new(FABLE_CTX))
            .with_n_batch(FABLE_BATCH)
            .with_embeddings(false)
            // Match the chat engine's KV quantization exactly: the narrator
            // context is the same shape as a chat context.
            .with_type_k(KvCacheType::Q8_0)
            .with_type_v(KvCacheType::Q8_0);
        let ctx = model_ref
            .new_context(backend, ctx_params)
            .map_err(|e| anyhow::anyhow!("game context init: {e:?}"))?;
        tracing::info!(n_ctx = FABLE_CTX, "game context created (isolated)");

        Ok(FableRuntime { ctx, model: model_ref })
    }
}

/// Distinguishes a mid-generation cancel from a real error so the reply can
/// set `cancelled: true` appropriately.
enum GenerationOutcome {
    Cancelled(String),
    GenerationErr(anyhow::Error),
}

// ---------------------------------------------------------------------------
// Runtime (owned by the game thread; never crosses thread boundaries)
// ---------------------------------------------------------------------------

struct FableRuntime {
    ctx: llama_cpp_2::context::LlamaContext<'static>,
    model: &'static LlamaModel,
}

impl FableRuntime {
    /// Generate one narrator turn: tokenize → prefill → sample-and-decode,
    /// streaming chunks via `req.on_chunk`. Checks `req.cancel` between
    /// tokens (Relaxed ordering, same correctness argument as the chat
    /// engine, §2B). Returns the full raw model output (Gemma4 channel
    /// protocol included: the caller parses/extracts).
    ///
    /// Uses the locked sampler config (temp 0.85 + top_p 0.95 + min_p 0.1 +
    /// dist(0), probabilistic multinomial sampling): same as the chat engine
    /// (AGENTS.md "Sampler config LOCKED"). Creative but not unhinged.
    ///
    /// No delta-prefill optimization for v1: each turn does a full
    /// prefill. The accepted §2F cold-reset tax on memory-injected turns
    /// applies here too. Optimize later if TTFT becomes a constraint.
    fn generate_turn(&mut self, req: &FableRequest) -> Result<String, GenerationOutcome> {
        let mut tokens = self
            .model
            .str_to_token(&req.prompt, AddBos::Always)
            .map_err(|e| GenerationOutcome::GenerationErr(anyhow::anyhow!("game tokenize: {e:?}")))?;
        if tokens.is_empty() {
            return Err(GenerationOutcome::GenerationErr(anyhow::anyhow!(
                "game tokenized prompt is empty"
            )));
        }
        // Truncate from the front if the prompt alone exceeds context (keep
        // the system prompt's tail + recent turns + generation cue). Mirror
        // of the schema engine's guard.
        let max_prompt = (FABLE_CTX as usize).saturating_sub(FABLE_MAX_TOKENS as usize);
        if tokens.len() > max_prompt {
            let drop = tokens.len() - max_prompt;
            tokens.drain(0..drop);
            tracing::warn!(dropped = drop, "game prompt exceeded context; truncated from front");
        }

        // One-shot full prefill each turn (no KV reuse for v1).
        self.ctx.clear_kv_cache();

        let n_prompt = tokens.len() as i32;
        let mut batch = LlamaBatch::new(FABLE_BATCH as usize, 1);
        let mut consumed = 0usize;
        while consumed < tokens.len() {
            let take = std::cmp::min(FABLE_BATCH as usize, tokens.len() - consumed);
            let is_last_chunk = consumed + take == tokens.len();
            batch.clear();
            for (i, tok) in tokens[consumed..consumed + take].iter().enumerate() {
                let is_final = is_last_chunk && i == take - 1;
                batch
                    .add(*tok, (consumed + i) as i32, &[0], is_final)
                    .map_err(|e| {
                        GenerationOutcome::GenerationErr(anyhow::anyhow!("game batch add: {e:?}"))
                    })?;
            }
            self.ctx
                .decode(&mut batch)
                .map_err(|e| {
                    GenerationOutcome::GenerationErr(anyhow::anyhow!("game prefill decode: {e:?}"))
                })?;
            consumed += take;
        }

        // Locked sampler config (see module doc + AGENTS.md).
        //
        // Chain order (load-bearing — see comments per stage):
        //   1. temp 0.85  — scales logits for creativity.
        //   2. top_p 0.95 + min_p 0.1 — truncate the candidate set.
        //   3. dry(...)   — DRY sampler (§11.40.E follow-up fix 2026-07-28).
        //      Purpose-built for sequence repetition: penalizes repeated
        //      multi-token sequences (the smuggler-loop / tail-repetition
        //      failure mode). DOES NOT suffer the penalties() failure mode
        //      (repetition_penalty suppresses common tokens `the`/`is`/
        //      `player` progressively, causing the `(player) is (player)`
        //      common-token loop — REVERTED 2026-07-28); DRY only fires on
        //      a sequence ≥ allowed_length tokens, so common tokens alone
        //      are never penalized. Fires after min_p (sees the truncated
        //      candidate set), before logit_bias (general shaping first,
        //      token-specific hyphen override second, sample last —
        //      llama.cpp convention). Conservative starting values:
        //      multiplier 0.8 / base 1.75 (upstream default) /
        //      allowed_length 2 (lets short bigrams repeat naturally) /
        //      penalty_last_n -1 (whole-context lookback for sequence
        //      detection). seq_breakers ["\n"] is the load-bearing
        //      false-positive guard: resets the DRY window at every newline
        //      so deliberate paragraph-level rhetorical anaphora /
        //      parallelism that crosses a paragraph break is NEVER
        //      penalized — DRY operates only within a single paragraph,
        //      exactly where mechanical looping happens. A second
        //      firewall (`stream_filter::truncate_repetition`) runs
        //      post-generation on `parsed.prose` in lib.rs::fable_send as
        //      the deterministic backstop for longer 4+-word loops that
        //      slip past the sampler; the two layers are complementary.
        //   4. logit_bias — negative bias on the hyphen token(s) (Bug B fix
        //      2026-07-28: the model natively generates `local-brewed-ale`-
        //      style hyphenated tokens; a -10.0 bias on the bare `-` token
        //      before sampling makes the model reach for a space instead).
        //      Gemma's SentencePiece tokenizer may encode `-` as multiple
        //      surface forms (bare `-`, space-prefixed ` -`); we resolve
        //      and bias ALL of them. Resolution failure → empty bias slice
        //      → no-op stage (the chain shape stays stable).
        //   5. dist(0)    — terminal multinomial sample. NOT bare (leaving
        //      the chain without a terminal sampler triggers
        //      `GGML_ASSERT(cur_p.selected >= 0)` in llama-sampler.cpp on
        //      the first decode).
        //
        // ANTI-REPETITION (smuggler-loop fix): TWO LAYERS. (1) DRY sampler
        // above (handles short token-sequence repetition at generation
        // time). (2) PROMPT-LEVEL "CRITICAL — DO NOT REPEAT" clause in
        // BRACKET_PROTOCOL (§11.40.C). (3) Deterministic post-generation
        // `stream_filter::truncate_repetition` firewall on finalized prose.
        // A prior attempt added `penalties(-1, 1.1, 0.0, 0.0)` as the first
        // chain stage; it was REVERTED the same day after live testing — on
        // Gemma 12B, repetition_penalty 1.1 suppresses common tokens (`the`,
        // `is`, `player`) progressively, causing the model to loop on
        // whatever high-logit token remains (the `(player) is (player)`
        // regression). DRY replaced it because DRY structurally can't cause
        // that failure mode (it only penalizes repeated *sequences*, not
        // individual common tokens).
        //
        // HISTORY: this chain previously ended in `greedy()` — pure argmax
        // after temp scaling, which made temp/top_p/min_p all no-ops (greedy
        // always picks the highest logit regardless of the filtered
        // distribution). Removed 2026-07-28 after the Phase 3 playtest: the
        // silent greedy collapse was producing low-creativity deterministic
        // prose. The seed is fixed at 0; a future pass can use a per-turn
        // time-based seed for nondeterminism across re-rolls of the same input.
        // Prong 2 (2026-07-29): the punctuation logit-bias table — comma
        // (mild), semicolon (heavy), en/em-dash (hard ban), plus the legacy
        // hyphen bias. Resolved against the live model at sampler-build time
        // via the shared `resolve_punct_biases` helper. See `punct_bias_table`
        // for the per-token rationale + the multi-token-resolution caveat.
        let punct_biases: Vec<LlamaLogitBias> = resolve_punct_biases(self.model);
        let n_vocab = self.model.n_vocab();
        // §11.43.B (2026-07-28): two sampler profiles sharing the same chain
        // shape (temp → top_p → min_p → dry → logit_bias → dist). The tracker
        // is an AGENT emitting rigid JSON/brackets — it gets a deterministic
        // profile (low temp, tight top_p, aggressive DRY). The narrator keeps
        // the existing creative profile (high temp, wide top_p, lenient DRY).
        // The punctuation logit bias + dist(0) terminal are shared (the Bug B
        // hyphen defense + the Prong 2 comma/semicolon/dash biases apply to
        // both passes; the chain needs a terminal sampler either way).
        //
        // Why DRY allowed_length=1 for the tracker: the `(player)(player)`
        // loop we saw in the §11.42 playtest repeated a 1-2 token sequence
        // that slipped under the narrator's allowed_length=2. Tightening to
        // 1 penalizes any 2-token-sequence repeat, which kills the loop
        // without affecting legitimate JSON field repetition (each JSON
        // field name appears at most a few times per turn, well under the
        // 3-repeat threshold the post-gen truncator would catch anyway).
        // Not 0 — DRY at allowed_length=0 would penalize ANY 2-token
        // sequence, mangling legitimate repetitions like `{ "type": "task",
        // "task": ... }`.
        let cfg = sampler_config(req.tracker_mode);
        let mut sampler = if req.tracker_mode {
            tracing::info!("sampler: tracker profile (temp=0.2, top_p=0.9, DRY allowed_length=1)");
            LlamaSampler::chain_simple([
                LlamaSampler::temp(cfg.temp),
                LlamaSampler::top_p(cfg.top_p, 1),
                LlamaSampler::min_p(crate::settings::MIN_P, 1),
                LlamaSampler::dry(self.model, cfg.dry_multiplier, cfg.dry_base, cfg.dry_allowed_length, -1, ["\n"]),
                LlamaSampler::logit_bias(n_vocab, &punct_biases),
                LlamaSampler::dist(0),
            ])
        } else {
            LlamaSampler::chain_simple([
                LlamaSampler::temp(cfg.temp),
                LlamaSampler::top_p(cfg.top_p, 1),
                LlamaSampler::min_p(crate::settings::MIN_P, 1),
                LlamaSampler::dry(self.model, cfg.dry_multiplier, cfg.dry_base, cfg.dry_allowed_length, -1, ["\n"]),
                LlamaSampler::logit_bias(n_vocab, &punct_biases),
                LlamaSampler::dist(0),
            ])
        };
        if !punct_biases.is_empty() {
            tracing::info!(
                biased = punct_biases.len(),
                "sampler: punctuation logit biases active (comma/semicolon/en-dash/em-dash + hyphen)"
            );
        } else {
            tracing::warn!("sampler: could not resolve any punctuation tokens — logit bias disabled");
        }
        let eos = self.model.token_eos();
        let mut n_cur = n_prompt;
        let mut step_batch = LlamaBatch::new(1, 1);
        let mut out = String::new();
        let max_tokens = FABLE_MAX_TOKENS
            .min((FABLE_CTX as i32 - n_prompt).max(64));

        // Protocol marker filter + bracket-command stripper. Same literal
        // marker set as the chat engine (engine.rs): strips `<|turn>`,
        // `<|channel>thought`, `<channel|>`, `<audio|>`, and the tool markers
        // so they never stream to the UI. PLUS `.with_brackets()` enables
        // stripping of the narrator's bracket commands (`[OBJECT...]`,
        // `[CHARACTER_TURN:...]`, `[FX...]`) during streaming, so they don't
        // render as raw text in the live UI feed (the 2026-07-26 leakage fix).
        // The bracket parser in lib.rs reads `raw_output` (captured on this
        // thread, never filtered), so stripping here doesn't break scene_event
        // extraction — the two paths are independent.
        //
        // Chloe 2026-07-28 — bare `<|channel>` added AFTER `<|channel>thought`.
        // The Gemma 4 protocol has many channel openers (`<|channel>reply`,
        // `<|channel>analysis`, bare `<|channel>`, etc.); the original list
        // only had `<|channel>thought`, so any non-thought opener leaked to
        // the UI during streaming (runtime-discovered on a local-only RP
        // session: `<channel>` markers flashing in the live feed). The bare
        // opener catches every variant. ORDER IS LOAD-BEARING: the regex is
        // built by joining markers with `|` in array order, and Rust's regex
        // alternation is first-match-wins (NOT longest), so `<|channel>thought`
        // MUST come before `<|channel>` — otherwise `<|channel>` (10 bytes)
        // would match first on a thought-channel opener and leave the
        // `thought` suffix as literal prose. The partial-prefix holdback in
        // stream_filter still works either way (it holds on the longer marker
        // when the chunk cuts the boundary), but the flush/strip pass relies
        // on the regex order to take the longest match.
        let mut marker_filter = crate::stream_filter::StreamFilter::new(&[
            "<|turn>",
            "<turn|>",
            "<|think|>",
            "<|channel>thought",
            "<|channel>",
            "<channel|>",
            "<audio|>",
            "<|tool_call>",
            "<tool_call|>",
            "<|tool_response>",
            "<tool_response|>",
            "<|tool>",
            "<tool|>",
        ])
        .with_brackets();

        for _ in 0..max_tokens {
            // Cancellation check at the TOP of the loop (between tokens,
            // never mid-decode: same KV-consistency contract as the chat
            // engine, §2C).
            if req.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::debug!("game turn cancelled by request");
                // Flush any held-back text so the partial reply is complete
                // up to the cancel point (mirrors the chat engine's flush).
                let tail = marker_filter.flush();
                if !tail.is_empty() {
                    out.push_str(&tail);
                    (req.on_chunk)(&tail);
                }
                return Err(GenerationOutcome::Cancelled(out));
            }

            // sample(&ctx, -1) reads logits from the last decoded position.
            // Same direct API the chat engine uses (engine.rs:773).
            let new_token: LlamaToken = sampler.sample(&self.ctx, -1);
            sampler.accept(new_token);

            if self.model.is_eog_token(new_token) || new_token == eos {
                break;
            }

            // Detokenize + stream the piece (encoding_rs decoder for
            // multibyte safety, mirrors engine.rs:750-754).
            let mut decoder = encoding_rs::UTF_8.new_decoder();
            let piece = self
                .model
                .token_to_piece(new_token, &mut decoder, true, None)
                .map_err(|e| {
                    GenerationOutcome::GenerationErr(anyhow::anyhow!("game token to piece: {e:?}"))
                })?;
            if !piece.is_empty() {
                out.push_str(&piece);
                // Feed through the marker filter: only the safe-to-emit
                // slice reaches the UI. The filter holds back any partial
                // marker prefix straddling the chunk boundary.
                let safe = marker_filter.feed(&piece);
                if !safe.is_empty() {
                    (req.on_chunk)(&safe);
                }
            }

            // Feed the token back at position n_cur.
            step_batch.clear();
            step_batch
                .add(new_token, n_cur, &[0], true)
                .map_err(|e| {
                    GenerationOutcome::GenerationErr(anyhow::anyhow!("game decode batch: {e:?}"))
                })?;
            self.ctx
                .decode(&mut step_batch)
                .map_err(|e| {
                    GenerationOutcome::GenerationErr(anyhow::anyhow!("game decode: {e:?}"))
                })?;
            n_cur += 1;
        }

        // Flush any held-back tail (partial marker prefix, or text inside
        // the trailing window at EOG). Same contract as the chat engine's
        // post-loop flush.
        let tail = marker_filter.flush();
        if !tail.is_empty() {
            out.push_str(&tail);
            (req.on_chunk)(&tail);
        }

        // Sampler drops implicitly on scope exit: no explicit free() needed.
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle is Send+Sync (manually asserted via unsafe impl). This test
    /// just confirms the type compiles with the right trait bounds: it
    /// doesn't construct one (that requires a real model load).
    #[test]
    fn fable_engine_traits_compile() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FableEngine>();
    }

    /// The reply struct serializes (it crosses the IPC boundary as JSON).
    #[test]
    fn game_reply_serializes() {
        let reply = FableReply {
            raw_output: "scene text".into(),
            error: String::new(),
            cancelled: false,
        };
        let json = serde_json::to_string(&reply).expect("serializes");
        assert!(json.contains("scene text"));
        assert!(json.contains("\"cancelled\":false"));
    }

    /// Constants are sane (compile-time sanity check).
    #[test]
    fn constants_are_sane() {
        assert!(FABLE_CTX >= 3072, "game context must fit system + 4-beat window + gen reserve");
        assert!(FABLE_BATCH >= 256, "batch must fit a chunk");
        assert!(FABLE_MAX_TOKENS >= 256, "max tokens must allow a meaty beat");
    }

    // ─────────────────────────────────────────────────────────────────────
    // §11.43.B — Sampler-config tests. These pin the "LOCAL mode is
    // unchanged" contract + the tracker-profile selection. They test the
    // PURE helper (no LlamaSampler construction, no model loading) so they
    // run in milliseconds and don't depend on the CUDA backend.
    // ─────────────────────────────────────────────────────────────────────

    /// THE §11.43.B INVARIANT: the narrator profile (tracker_mode=false)
    /// must be byte-identical to the pre-§11.43.B values. This is the
    /// "LOCAL mode is unchanged" contract — LOCAL mode is the `false`
    /// branch. If this test fails, a change to the narrator sampler has
    /// either regressed LOCAL's behavior or shifted its profile without
    /// updating AGENTS.md §11.41 + §11.43.B.
    #[test]
    fn sampler_config_returns_narrator_defaults_for_local_mode() {
        let cfg = sampler_config(false);
        // The pre-§11.43.B values from §11.41. Pinned here so any drift
        // breaks the test, not silently ships.
        assert_eq!(cfg.temp, 0.85, "narrator temp is the §11.41 value (0.85)");
        assert_eq!(cfg.top_p, 0.95, "narrator top_p is the §11.41 value (0.95)");
        assert_eq!(cfg.dry_multiplier, 0.8, "DRY multiplier from §11.41");
        assert_eq!(cfg.dry_base, 1.75, "DRY base from §11.41 (upstream default)");
        assert_eq!(
            cfg.dry_allowed_length, 2,
            "narrator DRY allowed_length=2 (the §11.41 value — preserves \
             rhetorical anaphora). NOT 1 (that's the tracker's tighter value)."
        );
    }

    /// The tracker profile (§11.43.B): deterministic + tighter DRY.
    #[test]
    fn sampler_config_returns_deterministic_tracker_profile() {
        let cfg = sampler_config(true);
        // The §11.43.B tracker values — chosen for deterministic JSON/bracket
        // emission + tighter repetition penalty than the narrator.
        assert_eq!(cfg.temp, 0.2, "tracker temp is LOW for deterministic output");
        assert_eq!(cfg.top_p, 0.9, "tracker top_p is TIGHT (focus on high-prob tokens)");
        assert!(cfg.temp < 0.5, "tracker temp must be well below the narrator's 0.85");
        assert!(cfg.top_p < 0.95, "tracker top_p must be tighter than the narrator's 0.95");
        assert_eq!(cfg.dry_multiplier, 0.8, "DRY multiplier same as narrator");
        assert_eq!(cfg.dry_base, 1.75, "DRY base same as narrator");
        assert_eq!(
            cfg.dry_allowed_length, 1,
            "tracker DRY allowed_length=1 — tighter than narrator's 2 to kill \
             single-token loops like `(player)(player)`. NOT 0 (would penalize \
             any 2-token sequence, mangling legitimate JSON field repetition)."
        );
    }

    /// The two profiles must be DISTINCT. If they ever collapse to the same
    /// values, the §11.43.B sampler split is a no-op (a regression in
    /// intent even if not in behavior).
    #[test]
    fn sampler_config_tracker_and_narrator_are_distinct() {
        let tracker = sampler_config(true);
        let narrator = sampler_config(false);
        assert_ne!(tracker, narrator, "tracker and narrator profiles must differ");
        // The temp + top_p + dry_allowed_length must all be tighter for the
        // tracker. (Multiplier/base are intentionally shared — only the
        // sequence-length threshold differs.)
        assert!(tracker.temp < narrator.temp);
        assert!(tracker.top_p < narrator.top_p);
        assert!(tracker.dry_allowed_length < narrator.dry_allowed_length);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Prong 2 (2026-07-29): punctuation logit-bias table tests. The table is
    // a pure &'static slice — testable without a loaded model. The model
    // resolution (`resolve_punct_biases`) needs a real LlamaModel, so it's
    // verified via the live build (the tracing log confirms the bias count)
    // rather than a unit test.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn punct_bias_table_has_the_locked_entries() {
        let table = punct_bias_table();
        // The blueprint's exact bias values. Pinned so any drift breaks the
        // test, not silently ships.
        let as_map: std::collections::HashMap<&str, f32> = table.iter().copied().collect();
        // Hyphen defenses (Bug B) — preserved from the pre-Prong-2 inline form.
        assert_eq!(as_map.get("-"), Some(&-10.0), "hyphen bias preserved (Bug B)");
        assert_eq!(as_map.get(" -"), Some(&-10.0), "leading-space hyphen bias preserved");
        // New Prong 2 entries.
        assert_eq!(as_map.get(","), Some(&-1.0), "comma bias is mild (-1.0)");
        assert_eq!(as_map.get(";"), Some(&-10.0), "semicolon bias is heavy (-10.0)");
        assert_eq!(
            as_map.get("\u{2013}"),
            Some(&-100.0),
            "en-dash is hard-banned (-100.0)"
        );
        assert_eq!(
            as_map.get("\u{2014}"),
            Some(&-100.0),
            "em-dash is hard-banned (-100.0)"
        );
    }

    #[test]
    fn punct_bias_table_ordering_is_hyphen_first() {
        // The hyphen entry MUST stay first so the legacy "Bug B" identity is
        // obvious in the table source (a future reader scanning for the
        // historical defense finds it immediately). Pinned.
        let table = punct_bias_table();
        assert_eq!(table[0].0, "-", "hyphen is the first entry (legacy Bug B defense)");
        assert_eq!(table[1].0, " -", "leading-space hyphen is second");
    }
}
