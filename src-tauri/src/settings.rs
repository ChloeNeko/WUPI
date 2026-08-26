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
//! PERMANENTLY BANNED — pure argmax collapsed the predecessor Gemma 12B into
//! degenerate output. Do not reintroduce it under any sampler profile.
//!
//! ## Re-lock note (2026-07-31)
//!
//! These values supersede the inline literals previously scattered across
//! `engine.rs`, `fable_engine.rs`, `schema_engine.rs`, `llm.rs`, and `lib.rs`.
//! AGENTS.md §1C + §10 point here as the authoritative location.

// ---------------------------------------------------------------------------
// Thinking (the Gemma4 <|think|> control token)
// ---------------------------------------------------------------------------

/// Whether the LOCAL model (Gemma 4 E4B) emits its Gemma4 thought channel (`<|think|>`) on
/// every local pass (Wupi chat, the Fable tracker, + the schema passes).
///
/// `false` (default, 2026-08-09): `<|think|>` is NOT injected. The thought
/// channel was a precautionary coherence measure, but live measurement on the
/// Gemma 4 12B (predecessor model) showed it either (a) roughly 5×'d per-turn
/// wall-clock (the model
/// reasons before every reply) or, worse, (b) could wedge into a thought
/// channel that never closed → generation ran to `max_tokens` → a 4-minute+
/// apparent hang with zero streamed events. Gemma 12B tracked brackets + schema
/// state cleanly WITHOUT thinking in prior sessions, so the cost/benefit was
/// upside-down. Flipping this to `false` keeps all the thought-handling
/// machinery (ThoughtGate, the StreamFilter `<|think|>` strip, the
/// `extract_reasoning_channel` capture) intact but DORMANT — they're no-ops
/// when the model emits no thought channel. Re-enable by setting `true`
/// (2026-08-17: model swapped 12B→E4B — re-verify on the E4B first).
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
/// treated as dead: the stream is dropped and the caller surfaces the loss
/// (Fable: `api_lost` + autosave + early return; there is NO local narration
/// fallback). Once the first token arrives the deadline is released — a
/// working stream is never killed mid-flight, even if slow.
/// (2026-08-21 Chloe ruling, 10s → 30s) NanoGPT's glm-latest TTFT tail
/// regularly rides 9-10s on healthy turns; the Liam session dropped two
/// narrations at exactly the 10s line — provider queue variance, not dead
/// connections. 30s keeps the drop for genuinely hung providers while
/// clearing their normal congestion band.
pub const API_FIRST_TOKEN_TIMEOUT_MS: u64 = 30_000;

/// (2026-08-15 audit fix) Idle guard BETWEEN chunks once the stream is alive
/// (post-first-token), in milliseconds. Replaces the old reqwest 300s TOTAL
/// request timeout — that killed legit 5+ minute narrations mid-flight. With
/// no total limit, a stream that stalls forever post-first-token needs its
/// own liveness bound: any working stream beats 120s between chunks by
/// orders of magnitude, so this only fires on a genuinely dead connection.
pub const API_CHUNK_IDLE_TIMEOUT_MS: u64 = 120_000;

// ---------------------------------------------------------------------------
// Context sizes (tokens)
// ---------------------------------------------------------------------------

/// API provider max_context (was 8192; raised 2026-07-31). Used by the API
/// path's `truncate_to_budget` safety net in `llm.rs`.
pub const CTX_API: u32 = 16384;

/// Local model context when an API IS connected. The local model is demoted to
/// the silent agent (schema/memory tracking + the tool/tracker pass); the API
/// carries narration. **2026-08-17 E4B shakedown P0: 3072** (Chloe-signed
/// override of the locked constant). The E4B swap bloated the Wupi-chat system
/// prefix to ~1541 tokens — ALONE over the 2048−512=1536 prompt budget, so
/// every chat turn (even "Hello Wupi") failed `truncate_to_fit` (truncation
/// can only drop conversation turns, never the system prefix). 3072 gives a
/// 2304-token prompt budget (the engine's reserve is `n_ctx/4` = 768 at
/// 3072 — see the generate() truncation math in engine.rs — not the 512
/// floor): absorbs
/// the prefix + the manager-path world-state
/// slice + memory block with headroom, without doubling the persistent chat
/// KV (~50% growth). Matches CTX_FABLE.
/// (2026-08-21 Chloe ruling — FINAL: 8192; was 3072 since the 2026-08-17 P0,
/// with a same-day interim at 4096) The E4B swap made context cheap; the
/// copilot prompt budget gets durable slack so the 2026-08-17 P0 (E4B prefix
/// ~1541 tokens locking the copilot out of every turn at 2048) can never
/// recur on any prefix growth. Engine reserve at 8192 is `n_ctx/4` = 2048,
/// leaving 6144 prompt tokens. KV cost: ~187 MiB Q8_0 worst-case linear
/// (sublinear in practice — the E4B's 512-token SWA layers don't grow; read
/// the exact MiB off the boot telemetry); the chat slot is the default
/// resident under the swap-lock. Matches CTX_FABLE.
pub const CTX_LOCAL_WITH_API: u32 = 8192;

/// Fable engine context (the tracker pass). **2026-08-08: 3072. 2026-08-21
/// Chloe ruling — FINAL: 8192 (E4B; same-day interims at 4096).** The local
/// model's Fable role is TRACKING ONLY (bracket commands + schema state) —
/// the API narrates. The tracker window is 2 messages (1 turn) — it relies
/// on the schema delta + Rust state, not re-read history. 3072 fit the
/// 2026-08-08 teaching set, but the 2026-08-18→21 verb growth (NPC interior
/// + site + economy) pushed real campaigns against the derived char budget
/// from turn ~22 (the Cinderfen economy playtest). 8192 + the raised
/// world-state visibility caps (STAGE0 + WS_BUDGET, lib.rs) are one ruling:
/// track MORE stuff, and extremely long roleplays must never approach the
/// ceiling — the worst realistic composition (fixed teaching ~7.6k + WS
/// ≤17k + maxed window) lands ~90% of the derived budget, and a REAL
/// campaign world state (~1-3k) never approaches it. KV cost:
/// ~120-190 MiB Q8_0 in the swap-locked fable slot (sublinear in practice —
/// the E4B's 512-token SWA layers don't grow; read the exact MiB off the
/// boot telemetry). `TRACKER_PROMPT_CHAR_BUDGET` is DERIVED from this (see
/// below): raising this constant automatically raises the budget.
pub const CTX_FABLE: u32 = 8192;

/// Schema-delta engine context. The micro-delta pass only needs system
/// instruction + current schema JSON + one exchange (see schema_engine.rs).
/// **2026-08-24: 2048 → 8192** (Chloe-authorized review, matching the
/// tracker/chat contexts — the standing "all local contexts 8192" ladder).
/// Measured composition of the WORLD-PROGRESSION tick prompt — the fattest
/// schema-engine path: framing ~80 + system instruction 3,557 + schema JSON
/// ≤4,000 (`SCHEMA_JSON_PROMPT_BUDGET_CHARS`) + interval paragraph ~500 ≈
/// **8,137 chars ≈ 2,260 tokens at 3.6 chars/token — over the old
/// 1,792-token prompt ceiling (2048 − 256) even with NO designated/evolution
/// sections**; the realistic full composition approaches ~11k chars ≈ 3,080
/// tokens, and the ALL-CAPS pathological composition (8 deferred attempts ×
/// max-capped errors + both site sections saturated) reaches ~16.8k chars ≈
/// 4,670 tokens. The middle-drop was therefore firing on healthy long
/// campaigns, slicing into the system instruction and splicing the schema
/// JSON the model must diff against — the exact duplicate-key spiral the
/// piecewise caps exist to prevent. Every piece is already hard-capped (JSON
/// 4k, evolution 2.3k, exchange 1.5k/side, deferred errors ≤400); the TOTAL
/// outgrew 2048 — shrinking further would cut teaching or schema visibility,
/// against the 2026-08-21 "never truncate" direction. 8192 gives a
/// 7,936-token prompt ceiling ≈ 28.5k chars — even the all-caps composition
/// fits with ~40% headroom, and the growth trajectory (the evolution
/// section's budget rose 1400→1700→1900→2300 across 2026-08-22→24) has room
/// before the composed-prompt pin trips again. Cost: KV worst-case linear
/// ~120-190 MB (the same figure as the fable slot at 8192; sublinear in
/// practice — the E4B's 512-token SWA layers don't grow), one-shot
/// (clear+prefill per call) and swap-locked to single residency; prefill
/// scales with ACTUAL prompt length (~2.3k realistic tokens), not n_ctx, so
/// small prompts pay nothing for the headroom. The composed whole-prompt pin
/// (`world_progression_prompt_worst_case_fits_schema_context`) is the
/// authoritative guard on this path now.
pub const CTX_SCHEMA: u32 = 8192;

// ---------------------------------------------------------------------------
// Visible-history windows (message counts, NOT tokens)
// ---------------------------------------------------------------------------

/// Wupi-chat visible window. 16 messages = 8 user↔assistant turns (2026-08-24
/// Chloe ruling — parity with the Fable narrator's [`WINDOW_API_FABLE`]). Both
/// the local KV render and the hybrid API reply pass assemble the session at
/// this window; the 8192 chat context absorbs it comfortably. History RESETS
/// when the player exits the chat surface (`chat_reset` clears the
/// conversation), so the window spans one sitting, never the process
/// lifetime.
pub const WINDOW_LOCAL_CHAT: usize = 16;

/// API-connected Fable visible window (8 turns).
pub const WINDOW_API_FABLE: usize = 16;

/// (2026-08-22 Ghost Writer + Crossroads) History window for the composer's
/// narrator-side one-shots (impersonation + the options deck). 8 messages =
/// 4 full exchanges: enough to carry the player's voice + the live scene
/// without paying the full 16-message narrator window on a helper call.
/// The Ghost Writer continue path deliberately uses the FULL
/// [`WINDOW_API_FABLE`] instead — a continuation IS a narrator beat and
/// needs every line of context it can get.
pub const WINDOW_GUIDED: usize = 8;

/// (2026-08-22 Ghost Writer + Crossroads) Char cap on the composer nudge
/// (the typed steer Swipe / Continue / Impersonate / a Crossroads choice
/// rides into its directive). Chars, not bytes (anti-pattern #6). The
/// composer itself allows 4000; a nudge is a direction, not a turn.
pub const GUIDED_NUDGE_CHAR_CAP: usize = 500;

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
/// T52 overflow fix; RAISED 600 → 1,200 on the 2026-08-21 8192 ruling —
/// evening follow-up). The tracker sees the last 1 turn (the player's action +
/// the preceding narrator beat). The narrator beat can run 1500-4700 chars;
/// at the old 600 the tracker read only the beat's opening paragraph — the
/// consequences that drive [PRESENCE]/[MOOD]/[INTENT]/[NPC_ITEM] live deeper
/// in the prose. 1,200 chars (~300 tokens) covers a full 2-3 paragraph beat;
/// the window is still mathematically bounded (~1,950 chars worst case with
/// a realistic action, pinned by the budget test). User messages are NOT
/// capped (they're the player's action — the trigger the tracker keys
/// off). The narrator window is unaffected (it has the 16k API budget).
pub const TRACKER_ASSISTANT_CHAR_CAP: usize = 1_200;

/// (2026-08-16 bug 12) Per-side char cap for the exchange/request folded
/// into the schema-engine prompts (delta + translation). The deferred
/// re-attempt entries were already capped at 200 chars while the LIVE
/// exchange rode in verbatim — a long chat reply or player paste pushed the
/// prompt over budget and the middle-drop deleted a contiguous band of the
/// sorted entity JSON the model must diff against (re-minted keys → growth
/// spiral). 1500 chars/side (~375 tokens) keeps the anchor signal generously
/// intact while bounding the blowup; the truncation marker makes the cut
/// visible to the model.
pub const SCHEMA_EXCHANGE_CHAR_CAP: usize = 1500;

/// (2026-08-16 yellow S4) TOTAL char budget for the schema JSON rendered into
/// the schema-engine prompts (`WorldSchema::to_json_prompt`). The
/// per-field legal maxima (500 entities × 400-char values + summary + events)
/// compose to ~25× the context; when the whole document would overflow, the
/// renderer now trims entities (oldest first, `player.*` identity keys always
/// kept) instead of letting the prompt path's middle-drop splice a contiguous
/// band out of the sorted JSON the model must diff against. 4000 chars
/// (~1000 tokens) leaves the fixed instruction + capped exchange their share
/// of CTX_SCHEMA with headroom.
pub const SCHEMA_JSON_PROMPT_BUDGET_CHARS: usize = 4000;

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

/// (2026-08-21 narrator length cap) `max_tokens` for the FABLE API narrator
/// ONLY — the one lever that bounds beat length mechanically. The request
/// body previously shipped NO max_tokens at all, so GLM generated to its
/// provider default and beats ran long (2026-08-21 playtest: "most replies
/// got really long"). 800 tokens ≈ a full 3-5 paragraph beat — generous for
/// the authored one-beat-per-turn pacing, but a hard ceiling the provider
/// enforces server-side. The creator assistant + slice-regen paths stay
/// UNCAPPED (`None`): lorebook batch conversion legitimately needs long
/// outputs and slice spans are short by construction.
pub const API_NARRATOR_MAX_TOKENS: u32 = 800;

/// (2026-08-24 hybrid chat) `max_tokens` for the WUPI-chat API reply pass.
/// The real verbosity fix lives in `data/wupi.prompt`'s pacing law (answer
/// first, no filler); this is the mechanical backstop so a runaway GLM reply
/// can't wall-of-text the chat. 512 tokens ≈ 2 short paragraphs plus a fenced
/// snippet — the pacing law asks for less; long file dumps belong in fenced
/// blocks the tool transcript already bounded. The local fallback path is
/// uncapped (it keeps its pre-hybrid decode shape).
pub const API_WUPI_MAX_TOKENS: u32 = 512;

/// (2026-08-22 Ghost Writer) `max_tokens` for the impersonation one-shot:
/// the model writes the player's NEXT message in the player's voice. A
/// player action runs a paragraph or three; 500 is generous headroom while
/// keeping a runaway short.
pub const GHOST_IMPERSONATE_MAX_TOKENS: u32 = 500;

/// (2026-08-22 Ghost Writer) `max_tokens` for the continue one-shot. The
/// continuation extends an existing beat, it never replaces one: it should
/// read as the back half of the same beat, so half a narrator ceiling.
pub const GHOST_CONTINUE_MAX_TOKENS: u32 = 400;

/// (2026-08-22 Ghost Writer) Lines of the trailing beat quoted into the
/// continue directive (Chloe's "2 line grab"): the closing anchor the
/// continuation picks up from, followed by the empty line that marks the
/// new paragraph. Beats keep a paragraph per line, so 2 lines is the
/// final paragraph plus its predecessor.
pub const GHOST_CONTINUE_TAIL_LINES: usize = 2;

/// (2026-08-22 Crossroads) `max_tokens` for the options-deck one-shot: 6
/// options, each an emoji + a 1-3 word title + a 2-3 sentence summary,
/// returned as one JSON array. ~120-150 tokens per option + syntax leaves
/// deep headroom under 1100.
pub const CROSSROADS_OPTIONS_MAX_TOKENS: u32 = 1100;

/// (2026-08-22 Crossroads) Options per deck draw. The task prompt asks for
/// exactly this many; `parse_options` truncates to it defensively.
pub const CROSSROADS_OPTION_COUNT: usize = 6;

/// (2026-08-22 Crossroads) `max_tokens` for the expand one-shot: the chosen
/// fork written out in full as the text the player will send. 1-3
/// paragraphs.
pub const CROSSROADS_EXPAND_MAX_TOKENS: u32 = 500;

// ---------------------------------------------------------------------------
// World-sim growth caps
// ---------------------------------------------------------------------------

/// (2026-08-15 audit fix) Hard cap on the player's concurrent status tags
/// ([EFFECT] upserts). Old behavior pushed unconditionally — a tracker loop
/// re-emitting the same effect stacked duplicates into `condition_penalty`
/// (lethality DC) and grew every save. TASK (20) and RUMOR (20) already had
/// their caps; this closes the third member of the family.
pub const FABLE_STATUS_TAG_CAP: usize = 16;

/// (2026-08-15 audit fix) Server-side cap on a `fable_send` player action,
/// in CHARS (not bytes — CJK/accented actions count fairly; anti-pattern #6
/// discipline). A giant paste front-drained the tracker prompt past its
/// 3072-token budget — the client composer cap is the first line of defense,
/// this is the authoritative backstop ("bounded only by client behavior" was
/// a documented P0 regression risk). 4000 chars ≈ 1000 tokens — generous for
/// a typed action, well under the tracker budget even before the window
/// truncation guard runs.
pub const FABLE_ACTION_CHAR_CAP: usize = 4000;

/// (2026-08-16 tracker-budget fix; 2026-08-21 DERIVED, economy-playtest
/// recalibration) Chars-per-token used to derive the tracker prompt budget
/// from the tracker context. Observed density on WUPI.gguf across the
/// 2026-08-16/21 playtests: 3.7–4.0 chars/token on real tracker prompts
/// (bracket-dense but English-prose-dominated). The constant uses 3.6 — a
/// half-step margin below the observed floor, reclaiming the headroom the
/// original conservative 3.5 left on the table (the 2026-08-21 Cinderfen
/// playtest: real campaigns hit the old 9,800 cap from turn ~22 and silently
/// lost the site/economy verb teaching to the core-tier degrade). If a
/// prompt class ever tokenizes below 3.6 the engine's tracker-mode backstop
/// hard-errors the decode (a loud skip, never a headless decode) — the
/// failure mode this margin trades against.
pub const TRACKER_PROMPT_CHARS_PER_TOKEN: f32 = 3.6;

/// (2026-08-21 DERIVED — no longer a hand-set number) TOTAL char budget for
/// the fully rendered TRACKER prompt (system prompt + window + generation
/// cue) — the lib.rs-side guard that keeps the fable engine's overflow
/// backstop from ever firing. Derivation: CTX_FABLE − TRACKER_MAX_TOKENS
/// (fable_engine.rs) = max prompt tokens ×
/// [`TRACKER_PROMPT_CHARS_PER_TOKEN`]. DERIVED so the coupling is explicit:
/// raising CTX_FABLE (a Foundation-Law constant, Chloe's call alone)
/// automatically raises this budget — no second number to forget. An
/// over-budget render takes exactly ONE fallback in
/// `build_tracker_prompt_bounded` (drop the preceding assistant beat — the
/// window tail-drop); still over = the tracker pass fails loudly instead of
/// decoding — the engine's old front-drain silently decapitated the system
/// prompt (bracket protocol) three separate times (2026-08-09, 2026-08-10
/// T52, 2026-08-16 playtest), which is why overflow is a hard error in
/// tracker mode.
pub const TRACKER_PROMPT_CHAR_BUDGET: usize = ((CTX_FABLE as f32
    - crate::fable_engine::TRACKER_MAX_TOKENS as f32)
    * TRACKER_PROMPT_CHARS_PER_TOKEN) as usize;

/// (2026-08-22 re-track hardening) The SAME derivation as
/// [`TRACKER_PROMPT_CHAR_BUDGET`], but for the edit/reroll RE-TRACK pass
/// (`FableTurnMode::TrackerRetrack`): CTX_FABLE − TRACKER_RETRACK_MAX_TOKENS
/// (512) × chars-per-token. The re-track's generation wall is DOUBLE the
/// live turn's (a re-track re-emits a full beat's bracket set), so its
/// prompt budget is correspondingly TIGHTER — the pair must move together
/// or the engine's over-budget REFUSAL fires on a prompt the lib.rs guard
/// just blessed.
pub const TRACKER_RETRACK_PROMPT_CHAR_BUDGET: usize = ((CTX_FABLE as f32
    - crate::fable_engine::TRACKER_RETRACK_MAX_TOKENS as f32)
    * TRACKER_PROMPT_CHARS_PER_TOKEN) as usize;

/// (2026-08-22 tracker feedback loop) Caps on the `<emit_errors>` block —
/// last turn's REJECTED bracket emissions, folded into the tracker's next
/// system prompt so the local model sees its own rejects (the Stage-2
/// narrator was the only consumer before; the tracker repeated the same
/// illegal emissions every turn of the 2026-08-22 playtest). The block is
/// retrieved-when-relevant by construction (empty slot → no block), but its
/// worst case must stay bounded: 6 lines × 140 chars ≈ 840 chars + frame —
/// counted inside the tracker budget pins.
pub const TRACKER_EMIT_ERROR_MAX_LINES: usize = 6;
/// Per-line char cap for the `<emit_errors>` block (UTF-8-safe truncation —
/// chars, not bytes).
pub const TRACKER_EMIT_ERROR_LINE_CHARS: usize = 140;

/// (2026-08-18 Dedicated-NPC reaper — Chloe's 3-tier / Garbage-Collector
/// ruling) In-world days a `named` (discovered, non-authored) NPC's interior
/// state survives without contact before the world-tick reaper archives it
/// (mood/intent/items compressed into a one-line stub; the registry entry +
/// relationship survive). Authored `core` NPCs are reaper-immune forever.
/// Measured on the WORLD CLOCK (`WorldSchema::world_clock.current_minutes`),
/// not wall-clock; `last_seen_minutes` is stamped by every `[PRESENCE]`
/// assert + interior mutation. 30 days = the "shopkeeper you stopped
/// visiting" horizon — long enough that an active recurring cast never
/// archives (any contact refreshes the stamp), short enough that a long
/// campaign's tail of one-meeting NPCs compresses instead of accumulating.
pub const NPC_REAP_NAMED_AFTER_DAYS: i64 = 30;
