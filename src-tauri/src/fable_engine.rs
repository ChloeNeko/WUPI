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
//! leaked `&'static LlamaModel` + one `shared_backend()`. Weights (~5.8 GB,
//! Gemma 4 E4B Q6_K since 2026-08-17)
//! + embedder (~36 MB) + one resident context (~50-100 MB Q8_0 KV) ≈ ~6.5 GB
//! → ~5.5 GB headroom on 12 GB (the 12B's 4000→3072 cut of 2026-07-26 was a
//! VRAM measure of the pre-E4B era).
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
/// shortcut amputations reversed).
///
/// **2026-08-08 override: CTX_FABLE set to 3072. 2026-08-21 Chloe ruling —
/// FINAL: 8192 (E4B; same-day interims at 4096).** The local model's Fable
/// role is now TRACKING ONLY (bracket commands + schema state) — the API
/// narrates
/// exclusively (§3A override). The tracker window is 2 messages (1 turn)
/// — it relies on the schema delta + Rust state, not re-read
/// history. 3072 fit the 2026-08-08 teaching set, but the 2026-08-18→21 verb
/// growth (NPC interior + site + economy) pushed real campaigns against the
/// derived tracker char budget from turn ~22 (the Cinderfen economy playtest).
/// 8192 + the raised world-state visibility caps are one ruling: track MORE
/// stuff; extremely long roleplays must never approach the ceiling.
///
/// **VRAM cost (2026-08-17 E4B figures, extrapolated 2026-08-21):** the Q8_0
/// KV cache at n_ctx=3072
/// is ~70 MiB (the E4B runs 2 KV-heads, a 512-token sliding window on 5 of
/// every 6 layers, + 18 shared-KV layers — read the exact MiB off the boot
/// telemetry); at 8192 it scales worst-case linearly to ~190 MiB (sublinear
/// in practice — the SWA layers don't grow). Under the §2B
/// swap-lock + the 2026-08-08 local-model turn lock, only ONE of {chat, schema,
/// fable} is resident + decoding at a time, so worst case is weights (~5.8 GB)
/// + one KV (~70-190 MiB) + embed (34 MiB, always resident) + compute buffer
/// (~530 MiB) ≈ ~6.6 GB of 12 GB → ~5.4 GB headroom. Stable.
///
/// The front-truncation guard below protects against overflow on the rare
/// turn where the prompt exceeds `FABLE_CTX - reserve` (reserve is mode-aware:
/// TRACKER_MAX_TOKENS for the tracker, FABLE_MAX_TOKENS for the narrator).
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
/// The hard ceiling for a TRACKER pass (`FableTurnMode::Tracker`). The tracker's
/// contract is brackets-only — a typical bracket set is 20-100 tokens, but a
/// multi-bracket turn (4-item inventory purchase + time + disguise) can reach
/// 150-200. Raised 150→256 on 2026-08-10 after the T52 playtest showed the 150
/// wall decapitating mid-bracket on multi-state-change turns (the tracker
/// emitted `[EQUIP ...]` then started `[BELT name` and was cut — the knife/rope
/// never landed). The Rust Sniper (§below) is the PRIMARY stop; it proved
/// itself 5× in T52, killing runaway prose inside ~100ms before KV damage. The
/// sniper is the true "cut Gemma off" lever; this wall is the backstop behind
/// it, sized to give the model enough room to finish a full bracket set without
/// artificial mid-word decapitation. pub(crate): settings.rs derives
/// TRACKER_PROMPT_CHAR_BUDGET from this + CTX_FABLE.
pub(crate) const TRACKER_MAX_TOKENS: i32 = 256;

// ---------------------------------------------------------------------------
// The Rust Sniper — early-stop for tracker rambling (2026-08-10)
// ---------------------------------------------------------------------------
//
// The tracker's contract is BRACKETS ONLY — prose belongs to the Stage-2 API
// narrator. When the local model has emitted its bracket set and then starts
// generating narrative prose (the failure mode: "ghostlyly quiet...", 460-1024
// tokens of rambling that never carries a bracket), the sniper detects the
// bracket→prose transition and BREAKS the decode loop within ~100ms — before
// the garbage tokens commit to KV or a repetition loop can start.
//
// The detection runs on the accumulated REPLY text (post-thought-channel),
// tracking: have we seen at least one closed bracket `]`, and is the text
// AFTER the last `]` sustained prose (no new `[` opening)? If a bracket has
// closed and ≥ SNIPER_PROSE_GRACE_CHARS of non-whitespace non-bracket text has
// accumulated since, the model has transitioned from tracking to narrating →
// snipe.
//
// This correctly permits multi-bracket turns ([PACK …] [TIME …] — while
// inside a bracket body, chars never count as prose) and only fires on the
// genuine failure (a bracket set followed by newline + prose). Cheap: one
// byte scan per token over the tail since the last `]` (the typical tail is
// < 30 chars).
//
// (2026-08-15 audit C1 fix) The original counter reset on `[`/`]` but still
// ACCUMULATED inside bracket bodies — any bracket after the first whose
// body reached 8 cumulative non-whitespace chars (`[TIME Day 2, 14:00]`,
// `[PRESENCE mara …]` — i.e. nearly all of them) tripped the sniper
// mid-body, decapitating the bracket before its `]` arrived. The parser
// then silently dropped the unterminated remnant: multi-bracket turns lost
// their 2nd+ state mutations with zero errors (the T52 "mid-bracket
// decapitation" previously blamed on the 150-token wall was this). The fix
// is an `inside_bracket` toggle: prose counts ONLY outside brackets.
//
// The sniper is the PRIMARY stop. TRACKER_MAX_TOKENS (256) is the wall behind
// it. Together they guarantee a tracker turn ends in seconds, not minutes.

/// How many non-whitespace, non-bracket chars of prose may follow a closed
/// bracket before the sniper fires. Tuned for: a short grace window so a `[`
/// mid-stream (the model opening its next bracket) isn't misread as prose, but
/// any genuine sentence fragment ("The fog is...") trips well inside it.
const SNIPER_PROSE_GRACE_CHARS: usize = 8;

struct TrackerSniper {
    /// True once at least one `]` (a closed bracket) has been seen in the
    /// reply text. The sniper only fires AFTER a bracket has closed — turns
    /// that emit no brackets are left to run to EOG/max_tokens (a bracket-less
    /// tracker turn is a valid "nothing changed" outcome, not rambling), and
    /// prose BEFORE the first close is tolerated (a tracker may emit a short
    /// preamble before its first bracket — rare, but tolerated).
    seen_closed_bracket: bool,
    /// True between a `[` and its closing `]`. While inside, chars never
    /// count as prose — later bracket bodies routinely exceed the grace
    /// window (`[TIME Day 2, 14:00]` is 8+ non-space chars) and must not
    /// trip the sniper mid-body (the 2026-08-15 C1 fix).
    inside_bracket: bool,
    /// (2026-08-16 yellow B12) True inside a ```-fence. A fenced-JSON command
    /// emitted AFTER the brackets used to count its opener + body as
    /// post-bracket prose (the grace window trips at the `{` of
    /// `{"kind":...`) — the sniper decapitated the fence mid-body and the
    /// command was silently dropped via the repair path. Fence interiors
    /// never count, and `[`/`]` inside a JSON body don't touch the bracket
    /// state machine.
    inside_fence: bool,
    /// Length of the current backtick run (a complete run of 3 toggles
    /// `inside_fence`; 1-2 stray backticks were prose).
    backtick_run: usize,
    /// The number of non-whitespace chars accumulated since the last `]`
    /// (or `[`) while OUTSIDE brackets.
    prose_since_close: usize,
}

impl TrackerSniper {
    fn new() -> Self {
        Self {
            seen_closed_bracket: false,
            inside_bracket: false,
            inside_fence: false,
            backtick_run: 0,
            prose_since_close: 0,
        }
    }

    /// Feed one decoded piece (already appended to the reply stream). Returns
    /// true if the sniper should fire (early-stop the decode).
    fn feed(&mut self, piece: &str) -> bool {
        // Per-char state machine (the old per-piece pre-close free-pass
        // lumped a whole piece together; per-char keeps bracket-open state
        // correct when a `[` and its text share a piece with the close).
        for ch in piece.chars() {
            if ch == '`' {
                self.backtick_run += 1;
                if self.backtick_run >= 3 {
                    // A complete fence delimiter toggles fence state
                    // (well-formed output pairs them: ```json … ```).
                    self.inside_fence = !self.inside_fence;
                    self.backtick_run = 0;
                    self.prose_since_close = 0;
                }
                continue;
            }
            if self.backtick_run > 0 {
                // A short run (1-2 backticks) wasn't a delimiter — those
                // chars were prose after all.
                let stray_run = self.backtick_run;
                self.backtick_run = 0;
                if self.seen_closed_bracket && !self.inside_fence {
                    self.prose_since_close += stray_run;
                }
            }
            if self.inside_fence {
                continue;
            }
            match ch {
                '[' => {
                    self.inside_bracket = true;
                    self.prose_since_close = 0;
                }
                ']' => {
                    self.inside_bracket = false;
                    self.seen_closed_bracket = true;
                    self.prose_since_close = 0;
                }
                c if c.is_whitespace() => { /* whitespace never counts */ }
                _ => {
                    // Prose: counted ONLY after the first close AND outside
                    // any bracket. Pre-first-close prose (a preamble) and
                    // bracket-body chars are tolerated by design.
                    if self.seen_closed_bracket && !self.inside_bracket {
                        self.prose_since_close += 1;
                    }
                }
            }
            if self.seen_closed_bracket
                && !self.inside_bracket
                && self.prose_since_close >= SNIPER_PROSE_GRACE_CHARS
            {
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Control plane: channel types
// ---------------------------------------------------------------------------

/// Which kind of pass a `FableRequest` is (2026-08-19: replaces the old
/// `tracker_mode: bool` — the JIT Site Architect is a third consumer).
///
/// - **Tracker** — the Stage-1 bracket pass: deterministic sampler, hard
///   over-budget REFUSAL, sniper armed, `TRACKER_MAX_TOKENS` reserve.
/// - **Architect** — the JIT site-map generator (`maybe_run_site_architect`):
///   the SAME deterministic profile + refusal + sniper as the tracker (it
///   emits one fenced JSON object, and the sniper is fence-aware so the
///   fence body can't be decapitated), with `SITE_ARCHITECT_MAX_TOKENS`
///   (512) as its reserve.
/// - **Narrator** — the dev-only local-narrator path: creative sampler,
///   front-truncate overflow valve, no sniper, `FABLE_MAX_TOKENS` reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FableTurnMode {
    Tracker,
    Narrator,
    Architect,
}

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
    /// §11.43.B (2026-07-28): the pass's mode — see [`FableTurnMode`].
    mode: FableTurnMode,
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
/// Tracker + Architect share the DETERMINISTIC profile (both are agents
/// emitting rigid state — brackets or one fenced JSON object).
fn sampler_config(mode: FableTurnMode) -> SamplerConfig {
    if matches!(mode, FableTurnMode::Tracker | FableTurnMode::Architect) {
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
    /// `mode` (§11.43.B + 2026-08-19): selects the sampler profile, the
    /// generation reserve, the sniper arming, and the over-budget behavior —
    /// see [`FableTurnMode`]. Callers that don't care should pass
    /// `FableTurnMode::Narrator` to get the default narrator behavior.
    pub fn request_turn(
        &self,
        prompt: String,
        on_chunk: ChunkFn,
        cancel: CancelToken,
        mode: FableTurnMode,
    ) -> anyhow::Result<mpsc::Receiver<FableReply>> {
        let (reply_tx, reply_rx) = mpsc::channel::<FableReply>();
        let req = FableRequest {
            prompt,
            on_chunk,
            cancel,
            mode,
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
    /// game 4000) fit on a 12GB GPU. Loading a second model copy would OOM
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
        let tokenize_start = std::time::Instant::now();
        let mut tokens = self
            .model
            .str_to_token(&req.prompt, AddBos::Always)
            .map_err(|e| GenerationOutcome::GenerationErr(anyhow::anyhow!("game tokenize: {e:?}")))?;
        let tokenize_ms = tokenize_start.elapsed().as_millis();
        tracing::info!(
            prompt_tokens = tokens.len(),
            tokenize_ms,
            "FABLE DECODE: tokenized prompt"
        );
        if tokens.is_empty() {
            return Err(GenerationOutcome::GenerationErr(anyhow::anyhow!(
                "game tokenized prompt is empty"
            )));
        }
        // Overflow guard: if the prompt alone exceeds context minus the
        // generation reserve, tracker mode REFUSES the decode (hard error —
        // see the in-block comment) and narrator mode (dev-only) front-
        // truncates. The fix per the Prime Directive is always to SHRINK the
        // prompt so this guard never fires; lib.rs's
        // `build_tracker_prompt_bounded` is the first line of defense.
        //
        // HISTORY (2026-08-10, 3rd recurrence): a front-drain chops the SYSTEM
        // PROMPT (the AGENT directive + bracket protocol), which is the exact
        // bug that silently killed all bracket emission: the tracker ran but
        // never saw the bracket syntax → zero brackets every turn → frozen
        // schema. Any overflow (either mode) is a P0 prompt-bloat regression.
        //
        // Reserve is MODE-AWARE (2026-08-10 fix): the tracker needs only
        // TRACKER_MAX_TOKENS (256 — raised from 150 post-T52) of generation
        // reserve, the narrator needs FABLE_MAX_TOKENS (1024). The prior bug
        // reserved 1024 for BOTH → max_prompt = 3072-1024 = 2048 → a
        // ~2500-token tracker prompt front-truncated 454-1022 tokens EVERY
        // turn, chopping the bracket protocol → tracker narrated instead of
        // tracking → zero brackets. With the tracker reserve, max_prompt =
        // 3072-256 = 2816 — the prompt fits, the bracket protocol survives,
        // the tracker sees its syntax.
        // Reserve is MODE-AWARE (2026-08-10 fix + 2026-08-19 Architect): the
        // tracker needs only TRACKER_MAX_TOKENS (256) of generation reserve,
        // the architect SITE_ARCHITECT_MAX_TOKENS (512 — one fenced site
        // JSON object), the narrator FABLE_MAX_TOKENS (1024).
        let reserve = match req.mode {
            FableTurnMode::Tracker => TRACKER_MAX_TOKENS,
            FableTurnMode::Architect => crate::site_map::SITE_ARCHITECT_MAX_TOKENS,
            FableTurnMode::Narrator => FABLE_MAX_TOKENS,
        };
        let max_prompt = (FABLE_CTX as usize).saturating_sub(reserve as usize);
        if tokens.len() > max_prompt {
            // (2026-08-16, 4th recurrence KILLED) Tracker + Architect modes
            // REFUSE to decode an over-budget prompt. The old front-drain
            // chopped the SYSTEM PROMPT (AGENT directive + bracket protocol)
            // — the exact mechanism that silently killed bracket emission
            // three times (2026-08-09, 2026-08-10 T52, and the 2026-08-16
            // playtest where episodic `<retrieved_knowledge>` growth made the
            // drain progressive: 5 → 687 tokens dropped over 16 turns). A
            // refused pass fails loudly (the caller skips brackets + logs)
            // and the narrator still runs on pre-tracker state — strictly
            // better than a confident decode of a headless prompt. The
            // lib.rs char-budget guard (`build_tracker_prompt_bounded`)
            // should catch this first; reaching this arm means that guard
            // was bypassed or the tokenizer defied the chars/token ratio.
            if matches!(req.mode, FableTurnMode::Tracker | FableTurnMode::Architect) {
                return Err(GenerationOutcome::GenerationErr(anyhow::anyhow!(
                    "TRACKER/ARCHITECT PROMPT OVERFLOW: {} tokens > {} max — refusing to decode a \
                                         headless prompt (the bracket protocol must never be front-chopped). \
                                         Fix the prompt budget, not the context.",
                    tokens.len(),
                    max_prompt
                )));
            }
            // Narrator mode (dev-only local-narrator path): keep the
            // front-truncate safety valve — the narrator's 16-message window
            // routinely exceeds its 2048-token slice in dev, and degraded
            // prose is acceptable there (the shipped build is API-only).
            let drop = tokens.len() - max_prompt;
            tokens.drain(0..drop);
            tracing::warn!(
                dropped = drop,
                total = tokens.len(),
                max = max_prompt,
                "⚠ PROMPT OVERFLOW: game prompt exceeded context; front-truncated (narrator/dev path)."
            );
        }

        // One-shot full prefill each turn (no KV reuse for v1).
        self.ctx.clear_kv_cache();

        let n_prompt = tokens.len() as i32;
        let mut batch = LlamaBatch::new(FABLE_BATCH as usize, 1);
        let mut consumed = 0usize;
        let prefill_start = std::time::Instant::now();
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
        let prefill_ms = prefill_start.elapsed().as_millis();
        tracing::info!(
            n_prompt,
            prefill_ms,
            prefill_tok_s = if prefill_ms > 0 { (n_prompt as u128 * 1000 / prefill_ms) as u64 } else { 0 },
            "FABLE DECODE: prefill complete"
        );

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
        // Gemma 12B (pre-E4B), repetition_penalty 1.1 suppresses common tokens (`the`,
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
        let cfg = sampler_config(req.mode);
        // (2026-08-19) The deterministic flag covers Tracker + Architect —
        // both are agent passes and share the profile; only the reserve
        // differs (see above).
        let deterministic = matches!(req.mode, FableTurnMode::Tracker | FableTurnMode::Architect);
        let mut sampler = if deterministic {
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
        // The tracker/architect get a tight failsafe ceiling (the sniper is
        // the primary stop; this is the wall behind it). The narrator keeps
        // the full 1024 budget. Both clamp to the remaining cache space.
        let base_cap = match req.mode {
            FableTurnMode::Tracker => TRACKER_MAX_TOKENS,
            FableTurnMode::Architect => crate::site_map::SITE_ARCHITECT_MAX_TOKENS,
            FableTurnMode::Narrator => FABLE_MAX_TOKENS,
        };
        let max_tokens = base_cap
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

        // ThoughtGate: holds the variable-length thought channel
        // (`<|channel>thought ... <channel|>`) out of the streamed prose so the
        // reasoning body never leaks to the UI (the marker_filter above strips
        // the MARKERS but not the body between them). Same pattern as the chat
        // engine (engine.rs). No-op when the model doesn't think: Detecting→Reply
        // passes through with only a tiny first-token peek (the length of the
        // opening marker). The raw `out` still accumulates the FULL verbatim
        // output (incl. thought) so chat_format::extract_reasoning_channel can
        // pull the reasoning out end-of-turn + bracket parsing sees the reply.
        let mut thought_gate = crate::chat_format::ThoughtGate::new();

        let decode_start = std::time::Instant::now();
        let mut gen_count: i32 = 0;
        // The sniper only arms for tracker + architect passes (the narrator
        // MUST be allowed to generate prose — that's its whole job; the
        // architect's fenced JSON is exempt via the fence-aware state
        // machine). One instance per turn.
        let mut sniper = if deterministic { Some(TrackerSniper::new()) } else { None };
        for _ in 0..max_tokens {
            // Cancellation check at the TOP of the loop (between tokens,
            // never mid-decode: same KV-consistency contract as the chat
            // engine, §2C).
            if req.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::debug!("game turn cancelled by request");
                // Flush both filters so the partial reply is complete up to
                // the cancel point (mirrors the chat engine's flush order:
                // thought_gate first, then marker_filter).
                let gate_tail = thought_gate.flush();
                if !gate_tail.is_empty() {
                    let cleaned = marker_filter.feed(&gate_tail);
                    if !cleaned.is_empty() {
                        (req.on_chunk)(&cleaned);
                    }
                }
                let tail = marker_filter.flush();
                if !tail.is_empty() {
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
                // Pipeline mirrors the chat engine (engine.rs:884-892):
                // thought_gate holds the thought body + emits clean reply,
                // then marker_filter strips the remaining protocol/bracket
                // markers. Only the safe slice reaches the UI. The raw `out`
                // keeps the full verbatim piece for end-of-turn reasoning
                // extraction + bracket parsing.
                let (gate_output, _is_thinking) = thought_gate.feed(&piece);
                if !gate_output.is_empty() {
                    let cleaned = marker_filter.feed(&gate_output);
                    if !cleaned.is_empty() {
                        (req.on_chunk)(&cleaned);
                    }
                }
                // The Rust Sniper (tracker only): feed the piece + check whether
                // the model transitioned from tracking (closed bracket) into
                // sustained prose. If so, break NOW — the bracket set is
                // already in `out`, the rambling isn't needed, and stopping
                // here keeps KV pristine + ends the turn in ~100ms.
                if let Some(sniper) = sniper.as_mut() {
                    if sniper.feed(&piece) {
                        tracing::info!(
                            gen_count,
                            out_len_chars = out.len(),
                            "🎯 SNIPER: tracker bracket→prose transition detected; early-stopping decode"
                        );
                        break;
                    }
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
            gen_count += 1;
            // Progress pulse every 100 tokens (the live-engine health signal).
            // Keeps the log alive during a long decode + gives tok/s for tuning.
            if gen_count % 100 == 0 {
                let elapsed_ms = decode_start.elapsed().as_millis();
                tracing::info!(
                    gen_count,
                    elapsed_ms,
                    tok_s = if elapsed_ms > 0 { (gen_count as u128 * 1000 / elapsed_ms) as u64 } else { 0 },
                    "FABLE DECODE: progress"
                );
            }
        }
        // Final decode telemetry (mirrors the chat engine's performance block).
        // Includes head/tail output samples so a non-terminating or repetitive
        // decode is diagnosable from the log alone.
        {
            let elapsed_ms = decode_start.elapsed().as_millis();
            let head: String = out.chars().take(200).collect();
            let tail: String = out.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
            tracing::info!(
                gen_count,
                max_tokens,
                elapsed_ms,
                tok_s = if elapsed_ms > 0 { (gen_count as u128 * 1000 / elapsed_ms) as u64 } else { 0 },
                out_len_chars = out.len(),
                hit_max_tokens = gen_count >= max_tokens,
                head_200 = %head,
                tail_200 = %tail,
                "FABLE DECODE: complete"
            );
        }

        // Flush both filters (thought_gate first, then marker_filter) — same
        // order + contract as the chat engine's post-loop flush
        // (engine.rs:940-950). Emits any held-back reply tail at EOG.
        let gate_tail = thought_gate.flush();
        if !gate_tail.is_empty() {
            let cleaned = marker_filter.feed(&gate_tail);
            if !cleaned.is_empty() {
                (req.on_chunk)(&cleaned);
            }
        }
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

    // ─────────────────────────────────────────────────────────────────────
    // TrackerSniper (2026-08-15 audit C1 regression tests). The old counter
    // accumulated whitespace-free prose INSIDE bracket bodies, so every
    // bracket after the first got decapitated mid-body. These pin the fixed
    // contract: prose counts ONLY outside brackets, after the first close.
    // ─────────────────────────────────────────────────────────────────────

    /// THE C1 invariant: a multi-bracket turn (the T52 shape — inventory +
    /// time + presence in one turn) must complete WITHOUT a snipe, even
    /// though the 2nd+ bracket bodies each exceed the grace window in
    /// non-whitespace chars.
    #[test]
    fn sniper_permits_multi_bracket_turns() {
        let mut s = TrackerSniper::new();
        for piece in [
            "[PACK name=\"Iron Ingot\" qty=2]",
            "\n",
            "[TIME Day 2, 14:00]",
            " ",
            "[PRESENCE mara \"behind the bar\"]",
        ] {
            assert!(!s.feed(piece), "sniper must not fire on bracket body: {piece}");
        }
    }

    /// A bracket arriving complete in ONE token piece must also survive (the
    /// per-char loop must reach the `]` before the 8th body char counts —
    /// inside-bracket chars never count).
    #[test]
    fn sniper_permits_single_piece_second_bracket() {
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[BELT name=Knife]"));
        assert!(!s.feed(" "));
        assert!(!s.feed("[TIME 09:00]"), "8 body chars inside the bracket must not snipe");
    }

    /// The genuine failure mode still fires: sustained prose AFTER a closed
    /// bracket (the bracket→prose transition the sniper exists to kill).
    #[test]
    fn sniper_fires_on_sustained_prose_after_brackets() {
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[TIME 09:00]"));
        assert!(!s.feed("\nThe "));
        assert!(s.feed("fog settles"), "8+ outside-bracket prose chars must snipe");
    }

    /// Prose BEFORE the first close is tolerated (the documented preamble
    /// allowance — pre-first-close prose never counts).
    #[test]
    fn sniper_tolerates_preamble_before_first_bracket() {
        let mut s = TrackerSniper::new();
        assert!(!s.feed("Tracking this turn now: "));
        assert!(!s.feed("[EFFECT Berserk buff 60]"));
        assert!(!s.feed(" short"));
        assert!(s.feed(" tail prose"));
    }

    /// An UNTERMINATED bracket never counts its body as prose — the decode
    /// runs to the TRACKER_MAX_TOKENS wall instead (the wall is the correct
    /// stop for that shape; sniping mid-bracket would decapitate it).
    #[test]
    fn sniper_waits_on_unterminated_bracket() {
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[EFFECT Rage buff 60] "));
        assert!(!s.feed("[BELT name=Very Long Knife Name Indeed"));
    }

    /// (2026-08-16 yellow B12) A fenced-JSON command emitted AFTER the
    /// brackets must complete without a snipe — the old counter read the
    /// fence opener + body as post-bracket prose and decapitated the command
    /// mid-body (silently dropped via the repair path).
    #[test]
    fn sniper_permits_fenced_json_after_brackets() {
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[TIME 09:00] "));
        assert!(
            !s.feed("```json\n{\"kind\":\"pack\",\"name\":\"Gold\",\"qty\":5}\n```"),
            "a complete fenced command after brackets must not snipe"
        );
        // Chunks split at arbitrary boundaries (the stream arrives in token
        // pieces — the backtick run can straddle pieces).
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[BELT name=Knife] "));
        for piece in ["`", "``json\n{", "\"kind\":\"task\",\"npc_id\":\"mara\"", ",\"description\":\"scout\",\"eta_minutes\":90}\n", "`", "``"] {
            assert!(!s.feed(piece), "split fence pieces must not snipe: {piece}");
        }
        // Sustained PROSE after the fence closes still fires (the fence is a
        // command, not a prose amnesty).
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[TIME 09:00] "));
        assert!(!s.feed("```json\n{\"kind\":\"fx\",\"effect\":\"rain\"}\n```"));
        assert!(s.feed(" then the fog settles"), "post-fence prose still snipes");
    }

    /// (yellow B12 corollary) A short backtick run (1-2) is prose, not a
    /// fence delimiter — it counts toward the grace window like any char.
    #[test]
    fn sniper_counts_stray_backticks_as_prose() {
        let mut s = TrackerSniper::new();
        assert!(!s.feed("[TIME 09:00] `a` `b` "));
        // ``a`` ``b`` = 2+1+2 + 2+1+2 = 10 prose chars past the close.
        assert!(s.feed(" tail"), "stray backticks count as prose");
    }

    /// Constants are sane (compile-time sanity check).
    #[test]
    fn constants_are_sane() {
        // 2026-08-21: CTX_FABLE = 8192 (Chloe ruling — FINAL, E4B; was 3072
        // since 2026-08-08 with a same-day interim at 4096). The tracker
        // window is 2 messages (1 turn) — it relies on
        // the schema delta + Rust state, not re-read history. 8192 + the
        // raised world-state visibility caps give tracking durable headroom
        // (the 2026-08-21 Cinderfen playtest
        // hit the derived char budget from turn ~22 at 3072) + the 256-token
        // tracker generation reserve (TRACKER_MAX_TOKENS, raised
        // 150→256 post-T52 to end mid-bracket decapitation on multi-item turns).
        assert!(FABLE_CTX >= 8192, "tracker context must fit system + window + gen reserve + thinking");
        assert!(FABLE_BATCH >= 256, "batch must fit a chunk");
        assert!(FABLE_MAX_TOKENS >= 256, "max tokens must allow a meaty beat");
    }

    // ─────────────────────────────────────────────────────────────────────
    // §11.43.B — Sampler-config tests. These pin the "LOCAL mode is
    // unchanged" contract + the tracker-profile selection. They test the
    // PURE helper (no LlamaSampler construction, no model loading) so they
    // run in milliseconds and don't depend on the CUDA backend.
    // ─────────────────────────────────────────────────────────────────────

    /// THE §11.43.B INVARIANT: the narrator profile (`FableTurnMode::Narrator`)
    /// must be byte-identical to the pre-§11.43.B values. This is the
    /// "LOCAL mode is unchanged" contract — LOCAL mode is the `false`
    /// branch. If this test fails, a change to the narrator sampler has
    /// either regressed LOCAL's behavior or shifted its profile without
    /// updating AGENTS.md §11.41 + §11.43.B.
    #[test]
    fn sampler_config_returns_narrator_defaults_for_local_mode() {
        let cfg = sampler_config(FableTurnMode::Narrator);
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
        let cfg = sampler_config(FableTurnMode::Tracker);
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
        let tracker = sampler_config(FableTurnMode::Tracker);
        let narrator = sampler_config(FableTurnMode::Narrator);
        assert_ne!(tracker, narrator, "tracker and narrator profiles must differ");
        // The temp + top_p + dry_allowed_length must all be tighter for the
        // tracker. (Multiplier/base are intentionally shared — only the
        // sequence-length threshold differs.)
        assert!(tracker.temp < narrator.temp);
        assert!(tracker.top_p < narrator.top_p);
        assert!(tracker.dry_allowed_length < narrator.dry_allowed_length);
    }

    /// (2026-08-19) The Architect shares the Tracker's DETERMINISTIC profile
    /// exactly — it emits one fenced JSON object, the same agent discipline
    /// as the bracket pass.
    #[test]
    fn sampler_config_architect_shares_tracker_profile() {
        assert_eq!(
            sampler_config(FableTurnMode::Architect),
            sampler_config(FableTurnMode::Tracker),
            "architect == tracker profile (only the reserve differs)"
        );
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
