//! The single source of truth for sampler/context/window configuration.
//!
//! Prompt prose lives in `.prompt` files (authored, loaded by `prompts.rs`);
//! sim-card identity lives in `.sim` files. THIS module holds the mechanical
//! numbers that govern HOW the engines run — context budgets, sampler
//! profiles, visible-history windows, the API timeout. Constants only: zero
//! logic, zero authored voice. Renaming/repointing a value here changes every
//! call site at once.
//!
//! ## The Prime Directive (§1B of AGENTS.md)
//!
//! `dist(0)` is the ONLY permitted terminal sampler stage. `greedy()` is
//! PERMANENTLY BANNED — pure argmax collapsed Gemma 12B into degenerate output.
//! Do not reintroduce it under any sampler profile.
//!
//! ## Re-lock note (2026-07-31)
//!
//! These values supersede the inline literals previously scattered across
//! `engine.rs`, `fable_engine.rs`, `schema_engine.rs`, `llm.rs`, and `lib.rs`.
//! AGENTS.md §1C + §10 point here as the authoritative location.

// ---------------------------------------------------------------------------
// Thinking (the Gemma4 <|think|> control token)
// ---------------------------------------------------------------------------

/// Whether the LOCAL 12B emits its Gemma4 thought channel (`<|think|>`) on
/// every local pass (Wupi chat, the Fable tracker, + the schema passes).
///
/// `false` (default, 2026-08-09): `<|think|>` is NOT injected. The thought
/// channel was a precautionary coherence measure, but live measurement on the
/// Gemma 4 12B showed it either (a) roughly 5×'d per-turn wall-clock (the model
/// reasons before every reply) or, worse, (b) could wedge into a thought
/// channel that never closed → generation ran to `max_tokens` → a 4-minute+
/// apparent hang with zero streamed events. Gemma 12B tracked brackets + schema
/// state cleanly WITHOUT thinking in prior sessions, so the cost/benefit was
/// upside-down. Flipping this to `false` keeps all the thought-handling
/// machinery (ThoughtGate, the StreamFilter `<|think|>` strip, the
/// `extract_reasoning_channel` capture) intact but DORMANT — they're no-ops
/// when the model emits no thought channel. Re-enable by setting `true`.
///
/// Gating the 4 injection sites (chat_format system turn, build_narrator_prompt
/// tracker, fable_command translation + seed prompts) here means flipping one
/// constant re-enables thinking everywhere consistently.
pub const THINKING_ENABLED: bool = false;

// ---------------------------------------------------------------------------
// API failure handling
// ---------------------------------------------------------------------------

/// Hard deadline for the API to deliver its FIRST token (TTFT), in
/// milliseconds. If no first byte arrives within this window, the request is
/// treated as dead: the stream is dropped, an `api_timeout` event fires to the
/// frontend (top-center error bubble), and the turn falls back to the local
/// model at `CTX_FALLBACK_TURN`. Once the first token arrives the deadline is
/// released — a working stream is never killed mid-flight, even if slow.
pub const API_FIRST_TOKEN_TIMEOUT_MS: u64 = 10_000;

// ---------------------------------------------------------------------------
// Context sizes (tokens)
// ---------------------------------------------------------------------------

/// API provider max_context (was 8192; raised 2026-07-31). Used by the API
/// path's `truncate_to_budget` safety net in `llm.rs`.
pub const CTX_API: u32 = 16384;

/// Local model context when NO API is connected — the user's full local
/// experience (narrator + agent both run locally).
pub const CTX_LOCAL_SOLO: u32 = 4096;

/// Local model context when an API IS connected. The local 12B is demoted to
/// the silent agent (schema/memory tracking + the tool/tracker pass); the API
/// carries narration. 2048 fits the short, mechanical agent turns.
pub const CTX_LOCAL_WITH_API: u32 = 2048;

/// Fable engine context (the tracker pass). **2026-08-08 override: 3072.** The
/// local 12B's Fable role is TRACKING ONLY (bracket commands + schema state) —
/// the API narrates. The tracker window is 2 messages (1 turn) — it
/// relies on the schema delta + Rust state, not re-read history.
/// 3072 fits the AGENT section (~386 tok) + bracket protocol (~477 tok) +
/// world_state + the 1-turn window + the 256-token tracker generation
/// reserve (TRACKER_MAX_TOKENS; raised 150→256 post-T52; the sniper is the
/// primary stop). The prior
/// 4096 was sized for the deleted local-narrator path; 2048 was too tight
/// (the fixed overhead alone is ~860 tok, nearly half the budget before any
/// card/world_state enters).
pub const CTX_FABLE: u32 = 3072;

/// Schema-delta engine context. The micro-delta pass only needs system
/// instruction + current schema JSON + one exchange (see schema_engine.rs).
pub const CTX_SCHEMA: u32 = 2048;

/// The one-shot context rebound for a fallback turn. When an API call fails
/// (timeout or error) and the local model handles that single turn, it runs at
/// 4096 so the user-visible fallback reply has full context. The next turn's
/// normal path drops it back to `CTX_LOCAL_WITH_API` (2048) — no sticky state.
pub const CTX_FALLBACK_TURN: u32 = 4096;

// ---------------------------------------------------------------------------
// Visible-history windows (message counts, NOT tokens)
// ---------------------------------------------------------------------------

/// Local-only chat visible window (4 user↔assistant turns).
pub const WINDOW_LOCAL_CHAT: usize = 8;

/// API-connected chat visible window (8 turns).
pub const WINDOW_API_CHAT: usize = 16;

/// Local-only Fable visible window (4 turns).
pub const WINDOW_LOCAL_FABLE: usize = 8;

/// API-connected Fable visible window (8 turns).
pub const WINDOW_API_FABLE: usize = 16;

/// The Fable TRACKER window (2026-08-10). The tracker scans ONE turn: the
/// player's just-typed action + the immediately preceding narrator response
/// (2 messages = 1 turn). Its job per the AGENT directive is "read what
/// happened THIS turn" — singular — and emit a state delta. It does NOT re-
/// read narrative history; the schema delta + Rust state are the authority,
/// and the API narrator (16 messages) carries full history. The prior value
/// of 4 (2 full turns of prose) was the bulk of the prompt bloat: 4 messages
/// at ~400 tokens each = ~1600 tokens just for the window, pushing the prompt
/// past the truncation guard and chopping the bracket protocol. With 1 turn
/// the prompt stays lean (~1200-1500 tokens), well under the 2922 budget, and
/// the bracket protocol survives intact.
pub const WINDOW_TRACKER: usize = 2;

/// Char cap on each ASSISTANT message fed into the tracker window (2026-08-10,
/// T52 overflow fix). The tracker sees the last 1 turn (the player's action +
/// the preceding narrator beat). The narrator beat can run 1500-4700 chars
/// (~400-1200 tokens) — feeding it raw pushed the tracker prompt to 3331 tokens
/// (over the 2922 budget) 7 times in T52, front-truncating the bracket protocol
/// on the worst turns. The tracker doesn't need the full prose — it needs the
/// gist (what happened) to decide whether brackets fire. Capping at 600 chars
/// (~150 tokens) per assistant message mathematically bounds the window to
/// ~300 tokens, keeping the total prompt under 2922 even with a maxed-out
/// world_state block. User messages are NOT capped (they're the player's action
/// — typically short, and truncating them would lose the trigger the tracker
/// keys off). The narrator window is unaffected (it has the 16k API budget).
pub const TRACKER_ASSISTANT_CHAR_CAP: usize = 600;

/// Cache-coherent fallback window. When an API call fails and the turn falls
/// back to local, the message window re-assembles at 6 — sized to stay inside
/// `CTX_FALLBACK_TURN` minus the generation reserve. See `chat_send`.
pub const WINDOW_API_FALLBACK: usize = 6;

// ---------------------------------------------------------------------------
// Sampler profiles — dist(0) terminal, greedy() BANNED
// ---------------------------------------------------------------------------

/// Narrator / creative temperature. Chat prose + Fable narrator pass.
pub const TEMP_NARRATOR: f32 = 0.85;

/// Tracker / agent / schema temperature. Low for deterministic JSON, bracket
/// emission, and tool-call structure.
pub const TEMP_TRACKER: f32 = 0.2;

/// Narrator / creative top_p.
pub const TOP_P_NARRATOR: f32 = 0.95;

/// Tracker / agent / schema top_p. Tighter — focus on high-probability tokens.
pub const TOP_P_TRACKER: f32 = 0.9;

/// min_p floor. Shared by both narrator and tracker profiles.
pub const MIN_P: f32 = 0.1;

/// DRY (repetition) multiplier. Shared by narrator + tracker.
pub const DRY_MULT: f32 = 0.8;

/// DRY base. Shared by narrator + tracker.
pub const DRY_BASE: f32 = 1.75;

/// DRY allowed_length for the NARRATOR. 2 preserves rhetorical anaphora
/// (deliberate paragraph-level repetition is not penalized).
pub const DRY_ALLOWED_LEN_NARRATOR: i32 = 2;

/// DRY allowed_length for the TRACKER. 1 kills single-token loops
/// (e.g. `(player)(player)` attractors in bracket output).
pub const DRY_ALLOWED_LEN_TRACKER: i32 = 1;

/// API temperature. Provider controls the rest (min_p/top_k/DRY are
/// llama.cpp-native and rejected by at least one provider with HTTP 400).
pub const API_TEMP: f32 = 0.85;

/// API top_p.
pub const API_TOP_P: f32 = 0.95;
