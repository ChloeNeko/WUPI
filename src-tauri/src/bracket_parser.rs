//! Bracket-command extractor for narrator output (Games app Seam 3).
//!
//! The narrator emits bracket commands alongside its prose to drive the UI
//! deterministically. This module parses those out of the *final raw output*
//! (post-generation), NOT from the token stream: brackets are scene-level
//! events, not token-level concerns, so they're best extracted once from the
//! complete text rather than incrementally during streaming.
//!
//! # Supported commands (mirror `narrator_prompt::BRACKET_PROTOCOL`)
//!
//! - `[CHARACTER_TURN:npc_id]` ... `[CHARACTER_TURN:end]`: an NPC spoke.
//! - `[OBJECT id=iron_chest state=open]`: an object's state changed.
//! - `[FX rain]`: a scene effect should activate.
//! - `[TIME Day 3, 14:00]`: advance the in-world clock (Seam #4, 2026-07-27).
//!   Parsed to minutes-since-epoch via [`parse_in_world_time`] below; the
//!   resulting `i64` is the authoritative clock value (Rust owns it, never
//!   the LLM). Drives the World Progression tick gate in `fable_send`.
//!
//! # Design
//!
//! Pure string parsing: no regex backtracking, no re-tokenizing (Prime
//! Directive §1B.2). One linear scan over the text, extracting bracketed
//! regions. The prose left over after extraction is the cleaned narrator
//! output the UI renders.
//!
//! Robustness: malformed brackets (`[OBJECT id=x]` missing `state=`,
//! `[CHARACTER_TURN:` unterminated) are silently dropped, not fatal. The
//! narrator is a 12B model; we tolerate noisy output.

use crate::consequence::Polarity;
use serde::Serialize;

/// One bracket command extracted from narrator output.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BracketCommand {
    /// An NPC spoke. `npc_id` matches a card's `start_npc_ids`. `line` is
    /// the prose between the open and close tags.
    CharacterTurn { npc_id: String, line: String },
    /// An object's state changed.
    Object { id: String, state: String },
    /// A scene effect should activate.
    Fx { effect: String },
    /// The global atmospheric condition changed (Fable Phase 4 Component 2,
    /// 2026-07-28). Single-region like FX/TIME/OBJECT. `condition` is the
    /// diegetic phrase the narrator weaves into prose ("heavy rain", "clear",
    /// "thick fog", "snowfall"). The Rust side owns the weather field — this
    /// is the ONLY bracket path that writes `WorldSchema::weather` (the World
    /// Progression tick drift is the other writer, but it's pure Rust, not a
    /// bracket). Named `condition` (not `kind`) to avoid colliding with this
    /// enum's `#[serde(tag = "kind")]` external discriminator (same reason
    /// `Effect.tag_kind` isn't named `kind`, see above).
    Weather { condition: String },
    /// The player moved to a new node (Fable Phase 4 Component 3, 2026-07-28).
    /// Single-region like WEATHER/TIME. `destination` is a bare node id
    /// ("cellar", "market_square") — NOT the diegetic name, NOT "node.cellar"
    /// (the parser strips an optional `node.` prefix the narrator may emit).
    /// Rust is the SOLE authority on whether the move is legal: the applier
    /// validates the destination exists + is adjacent to the current node
    /// (anti-sycophancy gate — non-neighbor moves are rejected with a
    /// [DIRECTIVE]). The first `[TRAVEL]` from `current_node: None` is allowed
    /// (seeds initial location without scenario-card wiring). Named
    /// `destination` (not `kind` / `node`) to avoid colliding with this
    /// enum's `#[serde(tag = "kind")]` external discriminator.
    Travel { destination: String },
    /// A rumor is seeded at the current node (Fable Phase 4 Component 4,
    /// 2026-07-28). Single-region like WEATHER/TRAVEL. `label` is the
    /// free-form diegetic phrase the narrator weaves into ambient gossip
    /// ("the stranger paid in gold coins", "the captain is looking for
    /// someone", "a bandit scout was seen at the ridge"). The applier roots
    /// the rumor at the current node (`known_nodes` initialized to
    /// `[origin_node]`); the World Progression tick then propagates it to
    /// adjacent nodes via `rumor::propagate_rumors`. Propagation-only by
    /// design — no polarity / truth field, no stored reputation score. Named
    /// `label` (not `kind`) to avoid colliding with this enum's
    /// `#[serde(tag = "kind")]` external discriminator (same reason
    /// `Effect.tag_kind` / `Weather.condition` / `Travel.destination` aren't
    /// named `kind`).
    Rumor { label: String },
    /// The in-world clock advanced. `minutes` is the authoritative value
    /// (minutes since 0001-01-01, parsed by [`parse_in_world_time`]); `raw`
    /// is the verbatim string the narrator emitted (kept for diagnostics +
    /// the debug panel). The Rust side owns the clock — this is the ONLY
    /// path that writes `WorldSchema::world_clock`.
    Time { minutes: i64, raw: String },
    /// A buff/debuff status tag is created with a timed WorldClock expiry
    /// (Fable Phase 3 Slice 4 wiring, 2026-07-28). `label` is the diegetic
    /// phrase the narrator weaves into prose ("Berserk Rage", "Feverish",
    /// "Blessed by the Sun Priest"). `polarity` is buff (positive) or
    /// debuff (negative) — drives `consequence::derive_condition`'s ±1
    /// nudge rules. `duration_minutes` is how long the tag lasts before
    /// the World Progression tick drops it (the tag's `expires_at` =
    /// current clock + duration). `0` duration means permanent (the
    /// sentinel; only removed by an explicit event, not time).
    ///
    /// `tag_kind` (Phase 4 §11.44, Component 1) is the optional
    /// discriminator routed into the resulting `StatusTag.kind`. Named
    /// `tag_kind` (not `kind`) to avoid colliding with this enum's
    /// `#[serde(tag = "kind")]` external discriminator. Currently
    /// recognized non-empty value: `"disguise"` (the disguise Referee gate
    /// reads it). Empty string = generic effect (the historical case).
    Effect {
        label: String,
        polarity: Polarity,
        duration_minutes: i64,
        tag_kind: String,
    },
    /// A relationship milestone event was recorded (Fable Phase 3 Slice 5
    /// wiring, 2026-07-28). `npc_id` matches the entity-map convention
    /// (e.g. "npc.marcus"). `event_id` is one of the known milestones in
    /// `relationship::MilestoneRegistry::defaults()` (e.g. "saved_life",
    /// "betrayed_trust", "shared_drink"). Rust records the event on the
    /// NPC's `RelationshipState`; the next render evaluates any transition
    /// the event triggers (hostility drops fire instantly; affinity
    /// advances respect the dual gates).
    Milestone { npc_id: String, event_id: String },
    /// An off-screen task was queued (Fable Phase 3 Slice 6 wiring,
    /// 2026-07-28). `npc_id` is the assigned NPC, `description` is the
    /// short diegetic task ("scout the bandit camp"). `difficulty` +
    /// `suitability` are enum-stringified (e.g. "challenging",
    /// "adequate"). `eta_minutes` is how many in-world minutes until the
    /// task resolves (added to the current clock to compute
    /// `resolves_at_minutes`). The World Progression tick resolves due
    /// tasks via `offscreen_task::resolve_expired_tasks` + emits
    /// directives.
    Task {
        npc_id: String,
        description: String,
        difficulty: String,
        suitability: String,
        eta_minutes: i64,
    },
}

/// The result of parsing narrator output: the bracket commands found + the
/// prose with brackets removed (for UI rendering).
#[derive(Debug, Clone, Default, Serialize)]
pub struct ParsedNarration {
    /// Bracket commands in the order they appeared.
    pub commands: Vec<BracketCommand>,
    /// The narrator prose with all bracket regions stripped out. What the
    /// UI renders as the dialogue box.
    pub prose: String,
}

/// Parse a narrator's complete raw output into commands + cleaned prose.
///
/// The output is the verbatim text the model emitted (Gemma4 channel
/// protocol is stripped upstream by `chat_format::extract_reply_channel` or
/// equivalent; this function sees pure narrator text).
///
/// Strategy: walk the text, when we see `[`, attempt to match a known
/// command pattern. On match, push a command + skip past the bracket. On
/// no match, copy the `[` into prose and continue (graceful: better to
/// leak a literal bracket than misparse).
///
/// `CHARACTER_TURN` is the only multi-region command (open + body + close).
/// `OBJECT` and `FX` are single-region. This keeps the parser linear and
/// the brackets-plus-prose invariant simple.
pub fn parse(raw: &str) -> ParsedNarration {
    // Bug A fix (2026-07-28): pre-extract fenced JSON blocks BEFORE the
    // bracket scan. Modern instruct-tuned models reach for JSON when they
    // see structured schema fields; we accept both shapes. Fences are
    // lexically disjoint from brackets (no `[`/`]` in the opener/closer),
    // so the bracket loop's `text_after` slicing contract for
    // CHARACTER_TURN is preserved — it runs over the fence-stripped
    // string and never sees fence bytes.
    let (raw, json_bodies) = extract_fenced_json(raw);

    let bytes = raw.as_bytes();
    let mut commands = Vec::new();
    let mut prose = String::with_capacity(raw.len());
    let mut i = 0;

    while i < bytes.len() {
        // Find the next `[` from the current position.
        let Some(rel) = bytes[i..].iter().position(|&b| b == b'[') else {
            prose.push_str(&raw[i..]);
            break;
        };
        let start = i + rel;

        // Emit any prose before the bracket.
        prose.push_str(&raw[i..start]);

        // Find the closing `]`.
        let Some(end_rel) = bytes[start..].iter().position(|&b| b == b']') else {
            // Unterminated bracket: emit the `[` literally and advance one
            // byte (so we don't loop forever on a stray `[`).
            prose.push('[');
            i = start + 1;
            continue;
        };
        let end = start + end_rel;
        let bracket = &raw[start + 1..end]; // contents between [ and ]

        // Try to match a command. On match, push it; on miss, the bracket
        // content is emitted as literal prose (preserves original text).
        // `text_after` is the raw text starting just past the closing `]` -
        // used by CHARACTER_TURN to find its `[CHARACTER_TURN:end]` body
        // terminator. Indices returned by `parse_one` are relative to this
        // slice (not the full `raw`), so the caller adds `end + 1`.
        let text_after = &raw[end + 1..];
        match parse_one(bracket, text_after) {
            Some((cmd, consumed_after_bracket)) => {
                commands.push(cmd);
                // For CHARACTER_TURN we also consumed the body + close tag;
                // advance past them.
                i = end + 1 + consumed_after_bracket;
            }
            None => {
                // Not a recognized command. Emit the bracket verbatim.
                prose.push('[');
                prose.push_str(bracket);
                prose.push(']');
                i = end + 1;
            }
        }
    }

    // Bug A fix (2026-07-28): parse each JSON body extracted in the pre-pass
    // into a BracketCommand. Failed parses are dropped silently — same
    // contract as a malformed bracket (the fence was a machine-channel; a
    // body that doesn't yield a valid command is just noise). The bodies
    // were already removed from `prose` by `extract_fenced_json`, so we
    // never re-inject them on failure.
    for body in &json_bodies {
        if let Some(cmd) = parse_json_command(body) {
            commands.push(cmd);
        }
    }

    // Chloe 2026-07-27 — extra-spaces fix. When a bracket command is
    // stripped, the spaces immediately before and after it survive in the
    // prose: `"Mara nods. [OBJECT id=door state=open] The fire crackles."`
    // becomes `"Mara nods.  The fire crackles."` (double space) because the
    // trailing space of the lead-in AND the leading space of the follow-on
    // both remain. The model often emits brackets inline despite the prompt
    // asking for them on their own line, so this is common. HTML collapses
    // adjacent whitespace in rendering, but the double spaces persist
    // verbatim in stored `content` (archived to session, re-rendered on
    // every feed rebuild) — and they're visible in the live stream too
    // (stream_filter strips brackets the same way, leaving the same gaps).
    //
    // The fence stripping above leaves the SAME gap pattern (a fence on its
    // own line, removed, leaves the surrounding newlines + any inline
    // spaces), so this normalize covers JSON removal too — no separate
    // fence-whitespace pass needed.
    //
    // Normalize: collapse runs of 2+ spaces to one, and trim trailing
    // whitespace per line (preserves newlines as paragraph breaks). Pure
    // string work, single pass, no allocation beyond the rebuilt string.
    let prose = normalize_whitespace(&prose);

    ParsedNarration { commands, prose }
}

/// Collapse runs of 2+ ASCII spaces into one, and trim trailing whitespace
/// from each line (preserves the newline as a paragraph break). Leading
/// whitespace per line is left intact (the model sometimes indents
/// intentionally for stylization; we don't want to flatten that). The
/// overall string is NOT trimmed — the caller may rely on leading/trailing
/// space semantics (rare, but cheap to leave alone).
///
/// Rationale (2026-07-27): the bracket-stripping in `parse` above leaves
/// adjacent spaces un-collapsed around each removed bracket. This helper
/// is the single normalization pass that fixes the resulting "double
/// space" artifacts in stored + streamed narrator prose.
fn normalize_whitespace(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_was_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if prev_was_space {
                // Collapse: skip this space (we already emitted one).
                continue;
            }
            out.push(' ');
            prev_was_space = true;
        } else if ch == '\n' {
            // Newline: always emit (paragraph break). Reset the space flag
            // so a leading space on the next line isn't treated as a run
            // continuation — but ALSO strip a trailing space we may have
            // just emitted before this newline (avoids " \n" line-end
            // artifacts that read as odd whitespace when rendered).
            if out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
            prev_was_space = false;
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    // Strip a trailing space before EOF (same line-end logic as above).
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Attempt to parse one bracket's contents into a `BracketCommand`.
/// Returns `(command, bytes_consumed_after_the_closing_bracket)`: the
/// after-bracket consumption is nonzero only for `CHARACTER_TURN`, which
/// swallows its body + close tag.
///
/// `text_after` is the raw text starting just after the closing `]` (used
/// to find the `CHARACTER_TURN:end` terminator). Indices returned are
/// relative to this slice.

/// Infer a status tag's polarity (Buff vs Debuff) from its label when the
/// narrator omits the explicit `buff`/`debuff` token (the model frequently
/// does — see the 2026-07-28 playtest). The heuristic is conservative:
/// only well-known debuff keywords map to Debuff; anything ambiguous
/// defaults to Buff (the safer default — a false-Buff just means a tag
/// that lifts condition rather than drags it, vs a false-Debuff that would
/// penalize the player for what should have been a help).
fn infer_polarity(label: &str) -> Polarity {
    const DEBUFF_KEYWORDS: &[&str] = &[
        "poison", "poisoned", "venom", "toxin", "toxic",
        "curse", "cursed", "hex", "hexed", "bane",
        "fever", "feverish", "sick", "illness", "nausea", "nauseous",
        "bleed", "bleeding", "wound", "wounded",
        "stun", "stunned", "daze", "dazed", "paralyze", "paralyzed",
        "fear", "frightened", "terrified", "panic",
        "exhaust", "exhausted", "fatigue", "fatigued", "tired",
        "drunk", "intoxicated", "hangover",
        "burn", "burning", "frostbite", "hypothermia",
        "disease", "diseased", "plague", "infection", "infected",
        "rage", "berserk", "frenzy",  // ambiguous but typically debuff in RPGs (loss of control)
        "slow", "slowed", "weakness", "weakened", "vulnerable",
        "blind", "blinded", "deaf", "deafened",
        "corruption", "corrupted", "taint", "tainted",
    ];
    let lower = label.to_lowercase();
    if DEBUFF_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        Polarity::Debuff
    } else {
        Polarity::Buff
    }
}

// ============================================================================
// Fenced-JSON dual parser (Bug A fix, 2026-07-28).
//
// Modern instruct-tuned models (Gemma 12B in particular) reach for JSON when
// they see structured schema-ish fields in the system prompt, ignoring the
// bracket protocol and emitting `{"effect_name": "..."}` blocks instead.
// Rather than fight the training (the rejected "Iron Fist" logit-bias plan),
// we accept BOTH shapes: brackets remain the legacy path, fenced JSON is the
// new canonical path. Both map to the same `BracketCommand` enum, so every
// downstream consumer (apply_phase3_bracket_commands, scene_event emission,
// the World Progression tick) is unchanged.
//
// The two-path-sync contract (stream_filter::with_brackets regex MUST mirror
// parse_one's recognized prefixes — the documented 2026-07-28 drift-leak
// lesson) extends to JSON too: a `fence_re` was added alongside the bracket
// regex in the same commit. Both paths strip in stream_filter AND parse here.
// ============================================================================

/// The fenced-JSON opener the model emits (Markdown code fence + language
/// tag). Kept as a single constant so the streaming-side regex + this
/// extraction stay in sync by construction.
const JSON_FENCE_OPENER: &str = "```json";
const JSON_FENCE_CLOSER: &str = "```";

/// Pre-pass: extract every ```` ```json ... ``` ```` fenced block from the
/// raw narrator text. Returns `(prose_with_fences_stripped, json_bodies)`.
///
/// The fences (opener + body + closer + any single surrounding newline) are
/// removed from the prose; the body of each fence is collected for parsing.
/// Defensive on every malformed shape:
///   - No opener in the input → returned unchanged, empty bodies vec.
///   - Unterminated fence (opener but no closer) → body up to EOF is taken,
///     opener + body removed from prose (treats the rest of the generation
///     as the JSON body the model was mid-way through emitting).
///   - Empty body (`opener` immediately followed by `closer`) → skipped
///     (no command can come from `{}` or empty).
///
/// Pure byte-scanning + `find`, mirroring the discipline of `parse()` below
/// (Prime Directive §1B.2: no regex backtracking, linear scan).
pub(crate) fn extract_fenced_json(raw: &str) -> (String, Vec<String>) {
    let bytes = raw.as_bytes();
    let mut prose = String::with_capacity(raw.len());
    let mut bodies = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        // Find the next ```json opener from the current position.
        let Some(rel) = raw[i..].find(JSON_FENCE_OPENER) else {
            prose.push_str(&raw[i..]);
            break;
        };
        let opener_start = i + rel;

        // Emit any prose before the opener.
        prose.push_str(&raw[i..opener_start]);

        // Body starts after the opener. Skip a single trailing newline if
        // present (the model always puts the JSON on its own line).
        let body_start = opener_start + JSON_FENCE_OPENER.len();
        let body_start = if bytes.get(body_start) == Some(&b'\r') {
            body_start + 1
        } else {
            body_start
        };
        let body_start = if bytes.get(body_start) == Some(&b'\n') {
            body_start + 1
        } else {
            body_start
        };

        // Find the closing fence. The closer is just ``` (no `json`), so we
        // search the remainder for it.
        let after_opener = &raw[body_start..];
        match after_opener.find(JSON_FENCE_CLOSER) {
            Some(closer_rel) => {
                let body_end = body_start + closer_rel;
                let body = &raw[body_start..body_end];
                let closer_end = body_end + JSON_FENCE_CLOSER.len();
                // Skip a single trailing newline after the closer (keeps the
                // prose clean — otherwise the fence leaves a blank line).
                let after_closer = if bytes.get(closer_end) == Some(&b'\r') {
                    closer_end + 1
                } else {
                    closer_end
                };
                let after_closer = if bytes.get(after_closer) == Some(&b'\n') {
                    after_closer + 1
                } else {
                    after_closer
                };
                if !body.trim().is_empty() {
                    bodies.push(body.to_string());
                }
                i = after_closer;
            }
            None => {
                // Unterminated fence: the model was mid-generation when the
                // stream ended (cancel, max-tokens, or a stutter). Take the
                // rest of the text as the body — best-effort, will likely
                // fail JSON parse and be dropped, but a partial body that
                // happens to be valid JSON still works.
                let body = &raw[body_start..];
                if !body.trim().is_empty() {
                    bodies.push(body.to_string());
                }
                // Nothing left to scan.
                break;
            }
        }
    }

    (prose, bodies)
}

/// Parse one JSON object body into a `BracketCommand`. Returns `None` on any
/// failure (malformed JSON, unknown shape, missing required fields) — the
/// caller drops silently, same contract as a malformed bracket.
///
/// Two-pass: try `serde_json::from_str` first (the fast path for well-formed
/// output). On failure, run `json_repair::repair` (the same module the schema
/// engine's 3-pass contract uses) and retry once. This reuses the existing
/// failure-recovery pattern rather than inventing a new one.
///
/// Dispatch: prefers an explicit discriminator field (`"type"` / `"kind"` /
/// `"command"`), else infers the variant from field-name prefixes
/// (`effect_*` → Effect, `milestone_*` / `event_id` → Milestone, `task_*` /
/// `eta_minutes` → Task, `clock` / `timestamp` → Time). Field-name aliases
/// are accepted liberally — the model invents names like `effect_name` and
/// `effect_duration_minutes`; we accept those alongside the canonical
/// `label` / `duration_minutes`.
pub(crate) fn parse_json_command(body: &str) -> Option<BracketCommand> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    // Try strict parse, then repaired parse. Both go through the same
    // downstream dispatcher so the repair path is just a recovery wrapper.
    let parsed = serde_json::from_str::<serde_json::Value>(body).ok().or_else(|| {
        let repaired = crate::json_repair::repair(body);
        serde_json::from_str::<serde_json::Value>(&repaired).ok()
    })?;

    let obj = parsed.as_object()?;
    json_value_to_command(obj)
}

/// Dispatch a parsed JSON object to the right `BracketCommand` variant. Pure
/// shape matching; no validation beyond "the required fields are present and
/// the right type" — same leniency as the bracket parsers.
fn json_value_to_command(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    // 1. Explicit discriminator field wins if present.
    let disc = obj
        .get("type")
        .or_else(|| obj.get("kind"))
        .or_else(|| obj.get("command"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let kind = disc.or_else(|| infer_kind_from_fields(obj))?;

    match kind.as_str() {
        "effect" | "status" | "tag" | "buff" | "debuff" => json_to_effect(obj),
        "time" | "clock" => json_to_time(obj),
        "milestone" => json_to_milestone(obj),
        "task" => json_to_task(obj),
        "character_turn" | "character" | "dialogue" => json_to_character_turn(obj),
        "object" => json_to_object(obj),
        "fx" | "effect_fx" | "scene_fx" => json_to_fx(obj),
        "weather" => json_to_weather(obj),
        "travel" | "move" | "arrive" | "go" => json_to_travel(obj),
        "rumor" | "gossip" | "hearsay" => json_to_rumor(obj),
        _ => None,
    }
}

/// If no explicit discriminator, look at the field names to infer the kind.
/// Accepts the same aliases as the per-variant parsers (e.g. `event` matches
/// the same variant as `event_id`).
fn infer_kind_from_fields(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    if keys.iter().any(|k| {
        k.starts_with("effect_") || *k == "polarity" || *k == "buff_or_debuff"
    }) {
        return Some("effect".to_string());
    }
    // Milestone: event_id/event + npc_id/npc. Both aliases recognized so a
    // body with only the short forms (`{npc, event}`) still dispatches.
    if keys.iter().any(|k| k.starts_with("milestone_") || matches!(*k, "event_id" | "event")) {
        return Some("milestone".to_string());
    }
    if keys.iter().any(|k| k.starts_with("task_") || *k == "eta_minutes" || *k == "eta") {
        return Some("task".to_string());
    }
    if keys.iter().any(|k| matches!(*k, "clock" | "timestamp" | "minutes" | "day" | "hour")) {
        return Some("time".to_string());
    }
    if keys.iter().any(|k| matches!(*k, "npc_id" | "npc")) && keys.iter().any(|k| *k == "line") {
        return Some("character_turn".to_string());
    }
    if keys.iter().any(|k| *k == "state") {
        return Some("object".to_string());
    }
    // Travel: destination / to / node (Component 3, 2026-07-28). Placed before
    // the weather single-field rule so a `{"destination": ...}` body doesn't
    // fall through to `condition`-based weather inference. None of these keys
    // collide with the richer discriminators above (effect_*/event_id/eta/
    // clock/npc+line/state), so order relative to those is safe.
    if keys
        .iter()
        .any(|k| matches!(*k, "destination" | "to" | "node"))
    {
        return Some("travel".to_string());
    }
    // Rumor: label / text / gossip / hearsay (Component 4, 2026-07-28). Placed
    // before the weather single-`condition` rule so a `{"label": ...}` body
    // doesn't fall through. None of these keys collide with the richer
    // discriminators above (effect_*/event_id/eta/clock/npc+line/state/
    // destination), so order relative to those is safe.
    if keys
        .iter()
        .any(|k| matches!(*k, "label" | "text" | "gossip" | "hearsay"))
    {
        return Some("rumor".to_string());
    }
    // Weather: only `condition` (the single field). Placed last so it doesn't
    // shadow any richer discriminator above (Component 2, 2026-07-28).
    if keys.iter().any(|k| *k == "condition") {
        return Some("weather".to_string());
    }
    None
}

fn json_to_effect(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let label = obj
        .get("label")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("effect_name"))
        .or_else(|| obj.get("effect_label"))
        .and_then(|v| v.as_str())?
        .to_string();
    if label.trim().is_empty() {
        return None;
    }

    let duration_minutes = obj
        .get("duration_minutes")
        .or_else(|| obj.get("duration"))
        .or_else(|| obj.get("effect_duration_minutes"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if duration_minutes < 0 {
        return None;
    }

    let polarity = obj
        .get("polarity")
        .or_else(|| obj.get("buff_or_debuff"))
        .and_then(|v| v.as_str())
        .and_then(|s| match s.to_lowercase().as_str() {
            "buff" | "positive" | "help" | "helps" => Some(Polarity::Buff),
            "debuff" | "negative" | "hurt" | "hurts" => Some(Polarity::Debuff),
            _ => None,
        })
        .unwrap_or_else(|| infer_polarity(&label));

    // §11.44 (Component 1): optional kind discriminator. Mirrors the
    // key=value EFFECT path; the resulting StatusTag.kind routes it out of
    // the generic buff/debuff lanes when non-empty (e.g. "disguise").
    let kind = obj
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(BracketCommand::Effect {
        label,
        polarity,
        duration_minutes,
        tag_kind: kind,
    })
}

fn json_to_time(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    // Prefer an explicit numeric minutes field (Rust-authoritative form).
    if let Some(mins) = obj.get("minutes").and_then(|v| v.as_i64()) {
        if mins >= 0 {
            return Some(BracketCommand::Time {
                minutes: mins,
                raw: format!("{}", mins),
            });
        }
    }
    // Otherwise parse a raw timestamp string via the existing parser.
    let raw = obj
        .get("raw")
        .or_else(|| obj.get("time"))
        .or_else(|| obj.get("clock"))
        .or_else(|| obj.get("timestamp"))
        .and_then(|v| v.as_str())?;
    let minutes = parse_in_world_time(raw)?;
    Some(BracketCommand::Time {
        minutes,
        raw: raw.to_string(),
    })
}

fn json_to_milestone(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let npc_id = obj
        .get("npc_id")
        .or_else(|| obj.get("npc"))
        .and_then(|v| v.as_str())?
        .to_string();
    let event_id = obj
        .get("event_id")
        .or_else(|| obj.get("event"))
        .and_then(|v| v.as_str())?
        .to_string();
    if npc_id.trim().is_empty() || event_id.trim().is_empty() {
        return None;
    }
    Some(BracketCommand::Milestone { npc_id, event_id })
}

fn json_to_task(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let npc_id = obj
        .get("npc_id")
        .or_else(|| obj.get("npc"))
        .and_then(|v| v.as_str())?
        .to_string();
    let description = obj
        .get("description")
        .or_else(|| obj.get("desc"))
        .and_then(|v| v.as_str())?
        .to_string();
    let difficulty = obj
        .get("difficulty")
        .and_then(|v| v.as_str())
        .unwrap_or("routine")
        .to_string();
    let suitability = obj
        .get("suitability")
        .and_then(|v| v.as_str())
        .unwrap_or("adequate")
        .to_string();
    let eta_minutes = obj
        .get("eta_minutes")
        .or_else(|| obj.get("eta"))
        .and_then(|v| v.as_i64())?;
    if npc_id.trim().is_empty()
        || description.trim().is_empty()
        || eta_minutes <= 0
    {
        return None;
    }
    Some(BracketCommand::Task {
        npc_id,
        description,
        difficulty,
        suitability,
        eta_minutes,
    })
}

fn json_to_character_turn(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let npc_id = obj
        .get("npc_id")
        .or_else(|| obj.get("npc"))
        .and_then(|v| v.as_str())?
        .to_string();
    let line = obj
        .get("line")
        .or_else(|| obj.get("text"))
        .or_else(|| obj.get("speech"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if npc_id.trim().is_empty() {
        return None;
    }
    Some(BracketCommand::CharacterTurn { npc_id, line })
}

fn json_to_object(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let id = obj
        .get("id")
        .or_else(|| obj.get("object_id"))
        .and_then(|v| v.as_str())?
        .to_string();
    let state = obj
        .get("state")
        .or_else(|| obj.get("new_state"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.trim().is_empty() {
        return None;
    }
    Some(BracketCommand::Object { id, state })
}

fn json_to_fx(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let effect = obj
        .get("effect")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("effect_name"))
        .and_then(|v| v.as_str())?
        .to_string();
    if effect.trim().is_empty() {
        return None;
    }
    Some(BracketCommand::Fx { effect })
}

/// Parse a `{"kind": "weather", ...}` JSON body into `BracketCommand::Weather`
/// (Fable Phase 4 Component 2, 2026-07-28). Single field `condition` (aliased
/// `weather`); mirrors `json_to_fx`'s leniency. Empty condition → None.
fn json_to_weather(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let condition = obj
        .get("condition")
        .or_else(|| obj.get("weather"))
        .and_then(|v| v.as_str())?
        .to_string();
    if condition.trim().is_empty() {
        return None;
    }
    Some(BracketCommand::Weather { condition })
}

/// Parse a `{"kind": "travel", ...}` JSON body (Component 3, 2026-07-28).
/// Destination is read from `destination` / `to` / `node` (model flexibility —
/// the field name is unstable across models). An optional `node.` prefix the
/// narrator may emit is stripped ("node.cellar" → "cellar"). Empty → None.
fn json_to_travel(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let dest_raw = obj
        .get("destination")
        .or_else(|| obj.get("to"))
        .or_else(|| obj.get("node"))
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    if dest_raw.is_empty() {
        return None;
    }
    let dest = dest_raw
        .strip_prefix("node.")
        .map(|s| s.trim().to_string())
        .unwrap_or(dest_raw);
    if dest.is_empty() {
        return None;
    }
    Some(BracketCommand::Travel { destination: dest })
}

/// Parse a `{"kind": "rumor", ...}` JSON body (Component 4, 2026-07-28).
/// Label is read from `label` / `text` / `gossip` / `hearsay` / `rumor`
/// (model flexibility — the field name is unstable across models). Empty →
/// None. Mirrors `json_to_weather`'s single-field leniency.
fn json_to_rumor(obj: &serde_json::Map<String, serde_json::Value>) -> Option<BracketCommand> {
    let label = obj
        .get("label")
        .or_else(|| obj.get("text"))
        .or_else(|| obj.get("gossip"))
        .or_else(|| obj.get("hearsay"))
        .or_else(|| obj.get("rumor"))
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    if label.is_empty() {
        return None;
    }
    Some(BracketCommand::Rumor { label })
}

fn parse_one(bracket: &str, text_after: &str) -> Option<(BracketCommand, usize)> {
    let bracket = bracket.trim();

    if let Some(rest) = strip_prefix_ci(bracket, "CHARACTER_TURN:") {
        let npc_id = rest.trim().to_string();
        if npc_id == "end" || npc_id.is_empty() {
            // A stray close tag or empty open tag: drop it.
            return Some((BracketCommand::CharacterTurn {
                npc_id: String::new(),
                line: String::new(),
            }, 0));
        }
        // Find the matching [CHARACTER_TURN:end] in `text_after`. The body
        // between is the NPC's spoken line. Case-insensitive (§11.40.F fix):
        // the model sometimes emits `[CHARACTER_Turn:end]` with a capital T;
        // `find_ci` catches the variant the same as the canonical form. The
        // consumed-length is the matched-close-tag's actual byte length (not
        // a hardcoded const) so it's correct regardless of the case variant.
        let close = "[CHARACTER_TURN:end]";
        if let Some(end_idx) = find_ci(text_after, close) {
            // Measure the actual close-tag length at the match site (case-
            // insensitive match means the surface form may differ in length
            // only if non-ASCII, which won't happen here — but measure
            // defensively by scanning to the next `]`).
            let tag_end = text_after[end_idx..]
                .find(']')
                .map(|r| end_idx + r + 1)
                .unwrap_or(end_idx + close.len());
            let line = text_after[..end_idx].trim().to_string();
            return Some((
                BracketCommand::CharacterTurn { npc_id, line },
                tag_end,
            ));
        }
        // No close tag: treat the rest of the output as the line (graceful).
        let line = text_after.trim().to_string();
        return Some((
            BracketCommand::CharacterTurn { npc_id, line },
            text_after.len(),
        ));
    }

    if let Some(rest) = strip_prefix_ci(bracket, "OBJECT") {
        // Parse `id=x state=y` (whitespace-tolerant). This is the documented
        // contract format. Models usually follow it; the strict parse below
        // is the fast path.
        let mut id = None;
        let mut state = None;
        for tok in rest.split_whitespace() {
            if let Some(v) = tok.strip_prefix("id=") {
                id = Some(v.to_string());
            } else if let Some(v) = tok.strip_prefix("state=") {
                state = Some(v.to_string());
            }
        }
        if let (Some(id), Some(state)) = (id, state) {
            return Some((BracketCommand::Object { id, state }, 0));
        }
        // Fallback: accept the model's actual free-form attribute format,
        // e.g. `[OBJECT npc_mara relationship=amicable]` or
        // `[OBJECT player_gold 100]`. The 2026-07-27 stress test found that
        // GLM-5.2 (and likely other models) emit OBJECT commands with
        // arbitrary `key=value` attributes instead of the strict
        // `id=/state=` pair. Without this fallback, parse_one returns None
        // and the top-level parse() emits the bracket as literal prose
        // ("Not a recognized command" branch at line ~108), leaking the
        // raw bracket into the user-visible content.
        //
        // Strategy: treat the FIRST whitespace token as the entity id
        // (the model consistently puts the entity name first), and join
        // the remaining tokens into the state string verbatim
        // (`"relationship=amicable"` or just `"100"`). This preserves the
        // `BracketCommand::Object { id, state }` UI contract while
        // accepting the free-form shape. If there's only one token
        // (just an id, no state), skip — we can't construct a meaningful
        // command from id alone.
        let toks: Vec<&str> = rest.split_whitespace().collect();
        if toks.len() >= 2 {
            let id = toks[0].to_string();
            let state = toks[1..].join(" ");
            return Some((BracketCommand::Object { id, state }, 0));
        }
        return None;
    }

    if let Some(rest) = strip_prefix_ci(bracket, "FX") {
        let effect = rest.trim().to_string();
        if !effect.is_empty() {
            return Some((BracketCommand::Fx { effect }, 0));
        }
        return None;
    }

    // [WEATHER <condition>] — Fable Phase 4 Component 2 (2026-07-28). Single-
    // region like FX/TIME/OBJECT. The body is a free-form diegetic phrase
    // (spaces allowed: "heavy rain", "clearing skies", "thick morning fog").
    // Empty body → None (emitted as literal prose). Case-insensitive via the
    // §11.41 follow-up `strip_prefix_ci` helper (ASCII-safe — "WEATHER" is
    // ASCII). The JSON form `{"kind": "weather", "condition": "..."}` is
    // handled by the serde-tag routing in `parse_json_command` — zero parse
    // code here (the `#[serde(tag = "kind")]` discriminator does the work).
    if let Some(rest) = strip_prefix_ci(bracket, "WEATHER") {
        let condition = rest.trim().to_string();
        if !condition.is_empty() {
            return Some((BracketCommand::Weather { condition }, 0));
        }
        return None;
    }

    // [TRAVEL <destination>] — Fable Phase 4 Component 3 (2026-07-28). Single-
    // region like WEATHER/TIME. The body is a bare node id ("cellar",
    // "market_square") — NOT the diegetic name. An optional `node.` prefix the
    // narrator may emit is stripped for ergonomics ("node.cellar" → "cellar").
    // Empty body → None (literal prose). Case-insensitive via `strip_prefix_ci`
    // (ASCII-safe — "TRAVEL" is ASCII). The JSON form `{"kind": "travel",
    // "destination": "..."}` is handled by the manual per-variant dispatch in
    // `parse_json_command` (the `travel` arm + `json_to_travel` helper) — zero
    // parse code here beyond the prefix-form.
    if let Some(rest) = strip_prefix_ci(bracket, "TRAVEL") {
        let dest_raw = rest.trim().to_string();
        // Ergonomic: strip an optional `node.` prefix (narrator convention).
        let dest = dest_raw
            .strip_prefix("node.")
            .map(|s| s.trim().to_string())
            .unwrap_or(dest_raw);
        if !dest.is_empty() {
            return Some((BracketCommand::Travel {
                destination: dest,
            }, 0));
        }
        return None;
    }

    // [RUMOR <label>] — Fable Phase 4 Component 4 (2026-07-28). Single-region
    // like WEATHER/TRAVEL. The body is a free-form diegetic phrase (spaces
    // allowed: "the stranger paid in gold coins", "the captain is looking for
    // someone"). Empty body → None (emitted as literal prose). Case-insensitive
    // via `strip_prefix_ci` (ASCII-safe — "RUMOR" is ASCII). The JSON form
    // `{"kind": "rumor", "label": "..."}` is handled by the manual per-variant
    // dispatch in `parse_json_command` (the `rumor` arm + `json_to_rumor`
    // helper) — zero parse code here beyond the prefix-form.
    if let Some(rest) = strip_prefix_ci(bracket, "RUMOR") {
        let label = rest.trim().to_string();
        if !label.is_empty() {
            return Some((BracketCommand::Rumor { label }, 0));
        }
        return None;
    }

    // [TIME <in-world timestamp>] — Seam #4 clock advance. Single-region like
    // OBJECT/FX. The body is parsed by parse_in_world_time into minutes-since-
    // epoch; on failure the bracket is emitted as literal prose (better to
    // surface a malformed timestamp than silently drop it).
    if let Some(rest) = strip_prefix_ci(bracket, "TIME") {
        let raw = rest.trim().to_string();
        if let Some(minutes) = parse_in_world_time(&raw) {
            return Some((BracketCommand::Time { minutes, raw }, 0));
        }
        return None;
    }

    // [EFFECT ...] — Fable Phase 3 Slice 4 (2026-07-28). Single-region.
    // Tolerant parse: accepts BOTH positional and key=value syntax (the model
    // emits both shapes — see the 2026-07-28 playtest). Polarity is OPTIONAL:
    // if absent, infer from the label (debuff-leaning words → Debuff, else
    // Buff). Defensive: malformed → None (emitted as literal prose, no panic).
    //
    // Accepted shapes:
    //   [EFFECT <label> <buff|debuff> <duration>]              (positional)
    //   [EFFECT <label> <duration>]                            (positional, no polarity)
    //   [EFFECT label=<label> polarity=<buff|debuff> duration=<n>]  (key=value)
    //   [EFFECT label=<label> duration_minutes=<n>]            (key=value, no polarity)
    //
    // The label may contain spaces (positional) or be a quoted/escaped value
    // (key=value — we strip a trailing `=` and join the rest).
    //
    // Phase 4 §11.44 (Component 1): the key=value form accepts an optional
    // `kind=<value>` discriminator (e.g. `kind=disguise`) that routes the
    // resulting StatusTag into a dedicated render + mechanic lane. The
    // positional form does NOT support kind (backwards-compat — it stays
    // generic); kind defaults to empty string.
    if let Some(rest) = strip_prefix_ci(bracket, "EFFECT") {
        let rest = rest.trim();
        // Parse key=value pairs first (model's preferred shape).
        let mut kv: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        let mut positional: Vec<&str> = Vec::new();
        for tok in rest.split_whitespace() {
            if let Some(eq) = tok.find('=') {
                let k = tok[..eq].trim();
                let v = tok[eq + 1..].trim();
                if !k.is_empty() {
                    kv.insert(k, v);
                }
            } else {
                positional.push(tok);
            }
        }
        // §11.44: optional `kind` discriminator. Hoisted before the
        // key=value/positional split so a MIXED form — positional body +
        // `kind=disguise` trailing (the recommended disguise syntax in
        // BRACKET_PROTOCOL) — still threads kind through. The key=value
        // branch below does NOT re-read kind; it inherits this value.
        let kind = kv.get("kind").copied().unwrap_or("").to_string();
        // Try key=value extraction first.
        if !kv.is_empty() {
            let label = kv.get("label").copied().or_else(|| kv.get("name").copied());
            let duration = kv
                .get("duration_minutes")
                .or_else(|| kv.get("duration"))
                .copied()
                .and_then(|s| s.parse::<i64>().ok());
            let polarity = kv.get("polarity").copied().and_then(|s| match s.to_lowercase().as_str() {
                "buff" => Some(Polarity::Buff),
                "debuff" => Some(Polarity::Debuff),
                _ => None,
            });
            if let (Some(label), Some(duration)) = (label, duration) {
                if !label.is_empty() && duration >= 0 {
                    let polarity = polarity.unwrap_or_else(|| infer_polarity(label));
                    return Some((
                        BracketCommand::Effect {
                            label: label.to_string(),
                            polarity,
                            duration_minutes: duration,
                            tag_kind: kind,
                        },
                        0,
                    ));
                }
            }
        }
        // Fall back to positional.
        if positional.len() >= 2 {
            let duration_idx = positional.len() - 1;
            let duration_minutes = positional[duration_idx].parse::<i64>().ok();
            if let Some(duration_minutes) = duration_minutes {
                if duration_minutes >= 0 {
                    // Check if the second-to-last token is a polarity.
                    let polarity_idx = positional.len() - 2;
                    let explicit_polarity = match positional[polarity_idx].to_lowercase().as_str() {
                        "buff" => Some(Polarity::Buff),
                        "debuff" => Some(Polarity::Debuff),
                        _ => None,
                    };
                    let (label, polarity) = if let Some(p) = explicit_polarity {
                        (positional[..polarity_idx].join(" "), p)
                    } else {
                        let label = positional[..duration_idx].join(" ");
                        (label.clone(), infer_polarity(&label))
                    };
                    if !label.is_empty() {
                        return Some((
                            BracketCommand::Effect {
                                label,
                                polarity,
                                duration_minutes,
                                // §11.44: kind is hoisted above so a mixed
                                // positional-body + `kind=disguise` form threads
                                // it through; pure-positional forms carry "".
                                tag_kind: kind,
                            },
                            0,
                        ));
                    }
                }
            }
        }
        return None;
    }

    // [MILESTONE <npc_id> <event_id>] — Fable Phase 3 Slice 5 (2026-07-28).
    // Single-region. Records a relationship milestone event for an NPC.
    // Tolerant: the model sometimes emits a literal `event_id`/`npc_id`
    // placeholder token from the prompt doc (`[MILESTONE mara event_id
    // first_positive_interaction]`). Filter those placeholders out before
    // extracting the two real values. Also accepts key=value.
    if let Some(rest) = strip_prefix_ci(bracket, "MILESTONE") {
        let rest = rest.trim();
        // Try key=value first.
        let mut kv_npc = None;
        let mut kv_event = None;
        let mut positional: Vec<&str> = Vec::new();
        for tok in rest.split_whitespace() {
            if let Some(eq) = tok.find('=') {
                let k = tok[..eq].trim();
                let v = tok[eq + 1..].trim();
                match k {
                    "npc_id" | "npc" | "id" => kv_npc = Some(v),
                    "event_id" | "event" => kv_event = Some(v),
                    _ => {}
                }
            } else {
                positional.push(tok);
            }
        }
        // Placeholder words the model copies verbatim from the prompt doc.
        const PLACEHOLDERS: &[&str] = &["event_id", "npc_id", "npc", "event", "id"];
        let real_positional: Vec<&str> = positional
            .iter()
            .copied()
            .filter(|t| !PLACEHOLDERS.contains(t))
            .collect();
        let npc_id = kv_npc
            .or_else(|| real_positional.first().copied())
            .map(|s| s.to_string());
        let event_id = kv_event
            .or_else(|| real_positional.get(1).copied())
            .map(|s| s.to_string());
        if let (Some(npc_id), Some(event_id)) = (npc_id, event_id) {
            if !npc_id.is_empty() && !event_id.is_empty() {
                return Some((BracketCommand::Milestone { npc_id, event_id }, 0));
            }
        }
        return None;
    }

    // [TASK ...] — Fable Phase 3 Slice 6 (2026-07-28). Single-region. Queues
    // an off-screen task. Tolerant parse: accepts BOTH positional AND key=value
    // syntax (the model emits both — see the 2026-07-28 playtest). The `|`
    // separator splits the head (npc_id + description) from the tail
    // (difficulty suitability eta); the description may contain spaces.
    // Defensive: malformed → None.
    //
    // Accepted shapes:
    //   [TASK <npc_id> <description> | <difficulty> <suitability> <eta>]  (positional)
    //   [TASK npc_id=<id> description=<desc> | difficulty=<d> suitability=<s> eta_minutes=<n>]  (key=value)
    if let Some(rest) = strip_prefix_ci(bracket, "TASK") {
        let rest = rest.trim();
        // Split on the `|` separator.
        let (head, tail) = if let Some(pipe_idx) = rest.find('|') {
            (rest[..pipe_idx].trim(), rest[pipe_idx + 1..].trim())
        } else {
            // No pipe — fall back to splitting the whole thing by whitespace.
            // The model sometimes omits the pipe. Take the first token as
            // npc_id, last 3 as difficulty/suitability/eta, middle as desc.
            return parse_task_no_pipe(rest);
        };

        // Parse head: extract key=value pairs, collect positional tokens.
        let (kv_head, pos_head) = split_kv_positional(head);
        let npc_id = kv_head
            .get("npc_id")
            .or_else(|| kv_head.get("npc"))
            .or_else(|| kv_head.get("id"))
            .copied()
            .map(|s| s.to_string())
            .or_else(|| pos_head.first().copied().map(|s| s.to_string()));
        let description = kv_head
            .get("description")
            .or_else(|| kv_head.get("desc"))
            .copied()
            .map(|s| s.to_string())
            .or_else(|| {
                if pos_head.len() >= 2 {
                    // First positional was npc_id; rest joined is description.
                    Some(pos_head[1..].join(" "))
                } else {
                    None
                }
            });

        // Parse tail: extract key=value pairs, collect positional tokens.
        let (kv_tail, pos_tail) = split_kv_positional(tail);
        let difficulty = kv_tail
            .get("difficulty")
            .copied()
            .map(|s| s.to_string())
            .or_else(|| pos_tail.first().copied().map(|s| s.to_string()));
        let suitability = kv_tail
            .get("suitability")
            .copied()
            .map(|s| s.to_string())
            .or_else(|| pos_tail.get(1).copied().map(|s| s.to_string()));
        let eta_minutes = kv_tail
            .get("eta_minutes")
            .or_else(|| kv_tail.get("eta"))
            .copied()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| pos_tail.get(2).and_then(|s| s.parse::<i64>().ok()));

        if let (Some(npc_id), Some(description), Some(difficulty), Some(suitability), Some(eta_minutes)) =
            (npc_id, description, difficulty, suitability, eta_minutes)
        {
            if !npc_id.is_empty() && !description.is_empty() && eta_minutes > 0 {
                return Some((
                    BracketCommand::Task {
                        npc_id,
                        description,
                        difficulty,
                        suitability,
                        eta_minutes,
                    },
                    0,
                ));
            }
        }
        return None;
    }

    None
}

/// Case-insensitive `strip_prefix`. Returns the slice of `haystack` after the
/// matched `prefix` (compared ASCII-case-insensitively), or `None` if the
/// prefix doesn't match. The returned slice is from the ORIGINAL `haystack`
/// (not a lowercased copy) so any arguments following the verb retain their
/// original case — only the command verb is folded for matching.
///
/// Why this exists (§11.40.F follow-up fix 2026-07-28): the live narrator
/// model (Gemma 12B) sometimes emits `[CHARACTER_Turn:...]` with a capital T
/// instead of the documented `CHARACTER_TURN`. The parser was case-sensitive
/// on the prefix, so the variant leaked as literal prose. This helper makes
/// all 7 command verbs case-insensitive at the match site, while preserving
/// the original `bracket` string for the `None`-arm literal-prose emission
/// (so legitimate text like `[The old road]` still emits verbatim, not
/// lowercased) and the argument parsing (`rest` stays original-case).
fn strip_prefix_ci<'a>(haystack: &'a str, prefix: &str) -> Option<&'a str> {
    let hay_bytes = haystack.as_bytes();
    let pfx_bytes = prefix.as_bytes();
    if hay_bytes.len() < pfx_bytes.len() {
        return None;
    }
    if hay_bytes[..pfx_bytes.len()]
        .iter()
        .zip(pfx_bytes)
        .all(|(h, p)| h.eq_ignore_ascii_case(p))
    {
        // Safety: prefix matched ASCII-case-insensitively, so the first
        // pfx_bytes.len() bytes are all ASCII (ASCII byte boundaries are
        // always valid UTF-8 char boundaries). The slice at pfx_bytes.len()
        // is therefore a valid char boundary.
        Some(&haystack[pfx_bytes.len()..])
    } else {
        None
    }
}

/// Case-insensitive search for a tag (e.g. `[CHARACTER_TURN:end]`) in `text`.
/// Returns the byte index of the match start, or `None`. Used by the
/// CHARACTER_TURN close-tag lookup so `[CHARACTER_Turn:end]` (capital T) is
/// found the same as the canonical form — sibling of `strip_prefix_ci`.
fn find_ci(text: &str, needle: &str) -> Option<usize> {
    let n_bytes = needle.as_bytes();
    if n_bytes.is_empty() {
        return Some(0);
    }
    let t_bytes = text.as_bytes();
    if t_bytes.len() < n_bytes.len() {
        return None;
    }
    for i in 0..=(t_bytes.len() - n_bytes.len()) {
        if t_bytes[i..i + n_bytes.len()]
            .iter()
            .zip(n_bytes)
            .all(|(t, n)| t.eq_ignore_ascii_case(n))
        {
            // Verify we landed on a UTF-8 char boundary (ASCII needle bytes
            // match ASCII text bytes, but the surrounding context could be
            // multibyte — confirm the start index is a boundary).
            if text.is_char_boundary(i) {
                return Some(i);
            }
        }
    }
    None
}

/// Split a whitespace-separated string into (key=value pairs, positional tokens).
/// Helper for the bracket parsers' tolerant key=value + positional parse. A
/// token containing `=` (with a non-empty key) goes into the kv map; any other
/// token goes into the positional vec. Pure, no allocations beyond the returns.
fn split_kv_positional(s: &str) -> (std::collections::HashMap<&str, &str>, Vec<&str>) {
    let mut kv = std::collections::HashMap::new();
    let mut positional = Vec::new();
    for tok in s.split_whitespace() {
        if let Some(eq) = tok.find('=') {
            let k = tok[..eq].trim();
            let v = tok[eq + 1..].trim();
            if !k.is_empty() {
                kv.insert(k, v);
                continue;
            }
        }
        positional.push(tok);
    }
    (kv, positional)
}

/// Fallback for `[TASK ...]` without a `|` separator. Splits by whitespace:
/// first token = npc_id, last 3 = difficulty/suitability/eta, middle (joined)
/// = description. The model occasionally omits the pipe; this keeps the parse
/// resilient.
fn parse_task_no_pipe(rest: &str) -> Option<(BracketCommand, usize)> {
    let (kv, pos) = split_kv_positional(rest);
    // Try key=value first.
    let npc_id = kv.get("npc_id").or_else(|| kv.get("npc")).copied();
    let description = kv.get("description").or_else(|| kv.get("desc")).copied();
    let difficulty = kv.get("difficulty").copied();
    let suitability = kv.get("suitability").copied();
    let eta = kv
        .get("eta_minutes")
        .or_else(|| kv.get("eta"))
        .copied()
        .and_then(|s| s.parse::<i64>().ok());
    if let (Some(npc_id), Some(description), Some(difficulty), Some(suitability), Some(eta)) =
        (npc_id, description, difficulty, suitability, eta)
    {
        if !npc_id.is_empty() && !description.is_empty() && eta > 0 {
            return Some((
                BracketCommand::Task {
                    npc_id: npc_id.to_string(),
                    description: description.to_string(),
                    difficulty: difficulty.to_string(),
                    suitability: suitability.to_string(),
                    eta_minutes: eta,
                },
                0,
            ));
        }
    }
    // Positional: need at least 5 tokens (npc_id + ≥1 desc + 3 trailing).
    if pos.len() >= 5 {
        let npc_id = pos[0].to_string();
        let trailing = &pos[pos.len() - 3..];
        let description = pos[1..pos.len() - 3].join(" ");
        let difficulty = trailing[0].to_string();
        let suitability = trailing[1].to_string();
        let eta_minutes = trailing[2].parse::<i64>().ok();
        if let Some(eta_minutes) = eta_minutes {
            if !npc_id.is_empty() && !description.is_empty() && eta_minutes > 0 {
                return Some((
                    BracketCommand::Task {
                        npc_id,
                        description,
                        difficulty,
                        suitability,
                        eta_minutes,
                    },
                    0,
                ));
            }
        }
    }
    None
}

/// Parse an in-world timestamp string into minutes since a fixed ancient epoch
/// (0001-01-01, same trick Multihog's `parseInWorldTime` uses). Pure string
/// parsing — no `chrono`, no `regex` (Prime Directive §1B: cheapest path). One
/// linear tokenization pass over the input.
///
/// Accepts a deliberately permissive set of formats (any one must be present;
/// combinations merge additively):
/// - `"Day 3"` / `"day 3"` / `"D 3"` — day index from 1
/// - `"14:00"` / `"2:30 PM"` / `"08:00 AM"` — 12/24-hour clock
/// - `"01/01/2026"` — DD/MM/YYYY calendar date (converted to days-since-epoch)
/// - Comma-joined combinations: `"Day 3, 14:00"`, `"08:00 AM, Day 1"`,
///   `"22:00, 01/01/2026"`
///
/// Returns `None` when the string has no parseable date/time signal at all.
/// Malformed fragments (e.g. a clock with a non-numeric hour) cause that
/// fragment to be skipped without failing the whole parse: a string like
/// `"Day 5, lunch"` parses as Day 5 at 00:00.
///
/// The fixed ancient epoch keeps all reasonable calendar years (500–9999 AD)
/// mapping to large positive numbers, so subtraction always works without
/// sign juggling. `i64` has ~5.3 trillion years of headroom at minute
/// granularity — never overflows in practice.
///
/// This is the load-bearing primitive for the World Progression tick gate
/// (`fable_send` checks `current - last_fired >= interval`), exactly mirroring
/// Multihog's design but in Rust and on a typed field.
pub fn parse_in_world_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut day_from_date: Option<i64> = None;
    let mut day_from_word: Option<i64> = None;
    let mut hours: i64 = 0;
    let mut minutes: i64 = 0;
    let mut saw_clock = false;
    let mut saw_any_signal = false;

    // Tokenize on whitespace + commas. We need word-vs-clock-vs-date scans per
    // token; a single linear pass over whitespace- and comma-delimited chunks
    // is cheaper than regex and avoids the dep.
    for tok_raw in s.split(|c: char| c.is_whitespace() || c == ',') {
        let tok = tok_raw.trim();
        if tok.is_empty() {
            continue;
        }
        let lower = tok.to_lowercase();

        // "Day N" / "day N" / "D N" — but the day number may be glued
        // ("day3") or split ("day 3" → "day" then "3"). Handle the split case
        // by remembering we saw "day" and picking up the next numeric token.
        if let Some(rest) = lower
            .strip_prefix("day")
            .or_else(|| lower.strip_prefix("d"))
        {
            let rest = rest.trim_start_matches(|c: char| c == '-' || c == '_');
            if rest.is_empty() {
                continue;
            }
            if let Ok(n) = rest.parse::<i64>() {
                day_from_word = Some(n);
                saw_any_signal = true;
                continue;
            }
            continue;
        }

        // "AM" / "PM" — handled inline with the clock parse below; skip here.
        if lower == "am" || lower == "pm" {
            continue;
        }

        // DD/MM/YYYY calendar date (slashes present + 3 numeric parts).
        if tok.contains('/') {
            let parts: Vec<&str> = tok.split('/').collect();
            if parts.len() == 3 {
                if let (Ok(dd), Ok(mm), Ok(mut yy)) = (
                    parts[0].parse::<u32>(),
                    parts[1].parse::<u32>(),
                    parts[2].parse::<i32>(),
                ) {
                    // Only 1-2 digit years get the 2000 offset. A 3-4 digit
                    // year (e.g. "001", "0001", "999") is taken literally —
                    // the user wrote the digits they meant. This matches
                    // Multihog's behavior (which uses yy < 100 on the parsed
                    // integer, treating "26" as 2026 but "2026" as 2026).
                    if parts[2].len() <= 2 && yy < 100 {
                        yy += 2000;
                    }
                    if let Some(days) = days_from_civil(yy, mm, dd)
                        .checked_sub(days_from_civil(1, 1, 1))
                    {
                        // Calendar dates are ABSOLUTE (days since 0001-01-01);
                        // they override the relative "Day N" form.
                        day_from_date = Some(days + 1);
                        saw_any_signal = true;
                        continue;
                    }
                }
            }
            // Malformed date token (has slashes but didn't parse): skip.
            continue;
        }

        // Clock "HH:MM" with optional trailing AM/PM (the AM/PM may be a
        // separate token; we read it from the raw token if glued, else we
        // scan the following tokens below).
        if tok.contains(':') {
            let (clock_part, meridian) = match tok.find(|c: char| c.is_alphabetic()) {
                Some(idx) => (&tok[..idx], Some(&tok[idx..])),
                None => (tok, None),
            };
            let parts: Vec<&str> = clock_part.split(':').collect();
            if parts.len() == 2 {
                if let (Ok(h), Ok(m)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
                    let h = apply_meridian(h, meridian);
                    hours = h;
                    minutes = m;
                    saw_clock = true;
                    saw_any_signal = true;
                    continue;
                }
            }
            continue;
        }

        // A bare numeric token after we've seen "day" with no number yet:
        // treat as the day index. This handles "day 3" (split form).
        if day_from_word.is_none() && tok.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = tok.parse::<i64>() {
                // Only adopt as day if it's plausibly a day index (1..100000).
                // A bare huge number is more likely a year we failed to parse
                // as a date — skip those.
                if (1..=100_000).contains(&n) {
                    day_from_word = Some(n);
                    saw_any_signal = true;
                    continue;
                }
            }
        }
    }

    // Second pass for glued-or-split AM/PM: if we saw a clock but no meridian
    // was glued to it, scan the raw tokens for a standalone AM/PM and re-apply.
    if saw_clock {
        for tok in s.split(|c: char| c.is_whitespace() || c == ',') {
            let lower = tok.trim().to_lowercase();
            if lower == "am" || lower == "pm" {
                hours = apply_meridian(hours, Some(&lower));
                break;
            }
        }
    }

    if !saw_any_signal {
        return None;
    }

    // Resolve the day. Prefer the calendar date (absolute); fall back to the
    // "Day N" word form (relative). If neither, only a clock was given: treat
    // the day as 1 (a bare time with no day is meaningful for the FIRST turn
    // of a game where the day hasn't been established yet — the gate's first-
    // call baseline behavior handles that).
    let day_index: i64 = day_from_date.or(day_from_word).unwrap_or(1);

    // (day - 1) * 1440 + h * 60 + m. Day 1, 00:00 → 0; Day 2 → 1440, etc.
    Some((day_index - 1) * 1440 + hours * 60 + minutes)
}

/// Apply a 12-hour meridian to a clock hour. AM keeps 1-11, sets 12 → 0.
/// PM keeps 1-11 → 13-23, keeps 12. Hours outside 1..=12 are passed through
/// unchanged (a 24-hour clock with a stray "PM" token shouldn't be mangled).
fn apply_meridian(h: i64, meridian: Option<&str>) -> i64 {
    match meridian.map(|m| m.to_lowercase()).as_deref() {
        Some("am") if h == 12 => 0,
        Some("am") if (1..=11).contains(&h) => h,
        Some("pm") if (1..=11).contains(&h) => h + 12,
        Some("pm") if h == 12 => 12,
        _ => h,
    }
}

/// Convert a (year, month, day) civil date to a day count (Howard Hinnant's
/// `days_from_civil` algorithm — public domain, no overflow for any plausible
/// date). 1-based month + day, astronomical year numbering (year 0 = 1 BC).
/// Returns the count of days since 1970-01-01 (the Unix epoch) — we then
/// subtract `days_from_civil(1, 1, 1)` at the call site to anchor everything
/// to 0001-01-01.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + (d as i64 - 1); // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_object_command() {
        let raw = "Alex approaches the hearth. [OBJECT id=iron_chest state=open] The lock gives way.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Object {
                id: "iron_chest".into(),
                state: "open".into(),
            }
        );
        assert!(parsed.prose.contains("Alex approaches the hearth."));
        assert!(parsed.prose.contains("The lock gives way."));
        assert!(!parsed.prose.contains("[OBJECT"));
    }

    #[test]
    fn extracts_fx_command() {
        let raw = "The storm breaks. [FX rain] Water drums on the shutters.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Fx { effect: "rain".into() }
        );
        assert!(parsed.prose.contains("The storm breaks."));
        assert!(!parsed.prose.contains("[FX"));
    }

    // ---------- WEATHER (Fable Phase 4 Component 2, 2026-07-28) ----------

    #[test]
    fn extracts_weather_command_basic() {
        let raw = "The sky darkens. [WEATHER heavy rain] Drops hammer the roof.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Weather { condition: "heavy rain".into() }
        );
        // Prose: the bracket is stripped, surrounding text preserved.
        assert!(parsed.prose.contains("The sky darkens."));
        assert!(parsed.prose.contains("Drops hammer the roof."));
        assert!(!parsed.prose.contains("[WEATHER"));
    }

    #[test]
    fn extracts_weather_command_case_insensitive() {
        // The §11.41 follow-up convention: prefix matching is ASCII-case-
        // insensitive. The condition's casing is preserved (it's free-form
        // diegetic text the narrator chose).
        for (bracket, expected_cond) in [
            ("[weather heavy rain]", "heavy rain"),
            ("[Weather Heavy Rain]", "Heavy Rain"),
            ("[WEATHER thick morning fog]", "thick morning fog"),
        ] {
            let parsed = parse(bracket);
            assert_eq!(parsed.commands.len(), 1, "bracket: {}", bracket);
            assert_eq!(
                parsed.commands[0],
                BracketCommand::Weather { condition: expected_cond.into() },
                "bracket: {}",
                bracket
            );
        }
    }

    #[test]
    fn extracts_weather_preserves_spaces_in_condition() {
        // The condition may contain spaces (positional body, free-form).
        let raw = "[WEATHER clearing skies after dawn]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Weather { condition: "clearing skies after dawn".into() }
        );
    }

    #[test]
    fn weather_empty_body_emitted_as_literal_prose() {
        // No condition → not a valid command → the bracket leaks verbatim
        // (mirrors FX empty-body behavior). Better to surface a malformed
        // bracket than silently drop it.
        let raw = "Odd text [WEATHER] trailing.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
        assert!(parsed.prose.contains("[WEATHER]"));
    }

    #[test]
    fn weather_whitespace_only_body_emitted_as_literal_prose() {
        // `[WEATHER   ]` trims to empty → same as above.
        let raw = "[WEATHER   ]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
        assert!(parsed.prose.contains("[WEATHER"));
    }

    #[test]
    fn json_weather_kind_dispatches() {
        // The JSON form `{"kind": "weather", "condition": "..."}` dispatches
        // via the per-variant arm in parse_json_command.
        let raw = "```json\n{ \"type\": \"weather\", \"condition\": \"heavy rain\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Weather { condition: "heavy rain".into() }
        );
    }

    #[test]
    fn json_weather_kind_alias_works() {
        // `kind` discriminator + `weather` field alias (the json_to_weather
        // leniency — accepts both `condition` and `weather` keys).
        let raw = "```json\n{ \"kind\": \"weather\", \"weather\": \"thick fog\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Weather { condition: "thick fog".into() }
        );
    }

    #[test]
    fn json_weather_inferred_from_condition_field() {
        // No explicit type/kind discriminator → infer_kind_from_fields should
        // route a body with only `condition` to the weather variant.
        let raw = "```json\n{ \"condition\": \"snowfall\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert!(matches!(parsed.commands[0], BracketCommand::Weather { .. }));
    }

    #[test]
    fn json_weather_empty_condition_is_noop() {
        // Empty condition → not a valid command → dropped silently.
        let raw = "```json\n{ \"type\": \"weather\", \"condition\": \"\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
    }

    // ---------- [TRAVEL] (Fable Phase 4 Component 3, 2026-07-28) ----------

    #[test]
    fn extracts_travel_basic() {
        let raw = "[TRAVEL cellar]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "cellar".into() }
        );
    }

    #[test]
    fn extracts_travel_strips_node_prefix() {
        // The narrator may emit "node.cellar"; the parser strips the prefix
        // for ergonomics (the id is "cellar").
        let raw = "[TRAVEL node.cellar]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "cellar".into() }
        );
    }

    #[test]
    fn extracts_travel_case_insensitive() {
        // §11.41 follow-up: all command-verb prefixes are case-insensitive.
        for verb in ["travel", "Travel", "TrAvEl"] {
            let raw = format!("[{verb} market_square]");
            let parsed = parse(&raw);
            assert_eq!(parsed.commands.len(), 1, "verb={verb}");
            assert_eq!(
                parsed.commands[0],
                BracketCommand::Travel { destination: "market_square".into() }
            );
        }
    }

    #[test]
    fn travel_with_underscore_id_preserved() {
        // Node ids are bare slugs (snake_case allowed).
        let raw = "[TRAVEL market_square]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "market_square".into() }
        );
    }

    #[test]
    fn travel_empty_body_emitted_as_literal_prose() {
        // No destination → not a valid command → the bracket leaks verbatim.
        let raw = "Odd text [TRAVEL] trailing.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
        assert!(parsed.prose.contains("[TRAVEL]"));
    }

    #[test]
    fn travel_whitespace_only_body_emitted_as_literal_prose() {
        let raw = "[TRAVEL   ]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
        assert!(parsed.prose.contains("[TRAVEL"));
    }

    #[test]
    fn json_travel_kind_dispatches() {
        // The JSON form `{"kind": "travel", "destination": "..."}` dispatches
        // via the per-variant arm in parse_json_command.
        let raw = "```json\n{ \"type\": \"travel\", \"destination\": \"cellar\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "cellar".into() }
        );
    }

    #[test]
    fn json_travel_to_alias_works() {
        // `to` is one of the lenient aliases (destination / to / node).
        let raw = "```json\n{ \"kind\": \"travel\", \"to\": \"market_square\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "market_square".into() }
        );
    }

    #[test]
    fn json_travel_node_alias_works() {
        // `node` is the third alias.
        let raw = "```json\n{ \"kind\": \"travel\", \"node\": \"cellar\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "cellar".into() }
        );
    }

    #[test]
    fn json_travel_node_prefix_stripped_in_json_form() {
        // The node. prefix is stripped in the JSON form too (consistency with
        // the bracket form).
        let raw = "```json\n{ \"destination\": \"node.cellar\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "cellar".into() }
        );
    }

    #[test]
    fn json_travel_inferred_from_destination_field() {
        // No explicit discriminator → infer_kind_from_fields routes a body with
        // `destination` (or `to` / `node`) to the travel variant.
        let raw = "```json\n{ \"destination\": \"cellar\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert!(matches!(parsed.commands[0], BracketCommand::Travel { .. }));
    }

    #[test]
    fn json_travel_inferred_from_to_field() {
        let raw = "```json\n{ \"to\": \"cellar\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert!(matches!(parsed.commands[0], BracketCommand::Travel { .. }));
    }

    #[test]
    fn json_travel_empty_destination_is_noop() {
        // Empty destination → not a valid command → dropped silently.
        let raw = "```json\n{ \"type\": \"travel\", \"destination\": \"\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
    }

    #[test]
    fn json_travel_whitespace_only_destination_is_noop() {
        let raw = "```json\n{ \"destination\": \"   \" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0);
    }

    #[test]
    fn json_travel_does_not_shadow_weather_inference() {
        // infer_kind_from_fields ordering: travel's heuristic runs BEFORE
        // weather's `condition` single-field rule. A body with both
        // `destination` and `condition` should route to travel (the richer
        // signal), not weather.
        let raw = "```json\n{ \"destination\": \"cellar\", \"condition\": \"fog\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert!(matches!(parsed.commands[0], BracketCommand::Travel { .. }));
    }

    #[test]
    fn extracts_character_turn_with_body() {
        let raw = "[CHARACTER_TURN:gorm] Rain's bad tonight. [CHARACTER_TURN:end] Gorm dries a mug.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        match &parsed.commands[0] {
            BracketCommand::CharacterTurn { npc_id, line } => {
                assert_eq!(npc_id, "gorm");
                assert_eq!(line, "Rain's bad tonight.");
            }
            _ => panic!("expected CharacterTurn"),
        }
        // The body was consumed into the command; prose has only the trailing bit.
        assert!(parsed.prose.contains("Gorm dries a mug."));
        assert!(!parsed.prose.contains("Rain's bad tonight."));
    }

    #[test]
    fn extracts_multiple_commands_in_order() {
        let raw = "[FX thunder] [OBJECT id=door state=closed] A shape moves outside.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 2);
        assert!(matches!(parsed.commands[0], BracketCommand::Fx { .. }));
        assert!(matches!(parsed.commands[1], BracketCommand::Object { .. }));
    }

    #[test]
    fn no_brackets_passes_through_unchanged() {
        let raw = "The fire crackles. Rain falls steadily.";
        let parsed = parse(raw);
        assert!(parsed.commands.is_empty());
        assert_eq!(parsed.prose, raw);
    }

    #[test]
    fn unknown_bracket_emitted_as_literal() {
        // `[NOTE:foo]` isn't a recognized command: preserve it in prose.
        let raw = "Strange [NOTE:foo] marker.";
        let parsed = parse(raw);
        assert!(parsed.commands.is_empty());
        assert_eq!(parsed.prose, raw);
    }

    #[test]
    fn unterminated_bracket_emits_literal() {
        let raw = "Trailing [unterminated";
        let parsed = parse(raw);
        assert!(parsed.commands.is_empty());
        assert!(parsed.prose.contains("[unterminated"));
    }

    #[test]
    fn malformed_object_dropped() {
        // Missing state= → not a valid command → bracket emitted verbatim.
        let raw = "Alex looks. [OBJECT id=chest] Nothing happens.";
        let parsed = parse(raw);
        assert!(parsed.commands.is_empty());
        assert!(parsed.prose.contains("[OBJECT id=chest]"));
    }

    // Regression tests for the 2026-07-27 leakage fix: the model emits
    // OBJECT commands with free-form attributes (relationship=, awareness=,
    // location=, etc.) instead of the strict id=/state= pair. Without the
    // fallback in parse_one, these leaked into prose as literal text.
    #[test]
    fn object_with_free_form_attribute_is_parsed() {

        // GLM-5.2's actual format: entity_name first, then key=value attrs.
        let raw = "Mara watches. [OBJECT npc_mara relationship=amicable] She nods.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        match &parsed.commands[0] {
            BracketCommand::Object { id, state } => {
                assert_eq!(id, "npc_mara");
                assert_eq!(state, "relationship=amicable");
            }
            other => panic!("expected Object, got {other:?}"),
        }
        // Critical: the bracket must NOT survive into prose (this was the leak).
        assert!(!parsed.prose.contains("[OBJECT"));
        assert!(parsed.prose.contains("Mara watches."));
        assert!(parsed.prose.contains("She nods."));
    }

    #[test]
    fn object_with_bare_value_is_parsed() {
        // `[OBJECT player_gold 100]` — entity + bare scalar (no key=).
        let raw = "Gold clinks. [OBJECT player_gold 100] Pouch heavy.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Object { id, state } = &parsed.commands[0] {
            assert_eq!(id, "player_gold");
            assert_eq!(state, "100");
        } else {
            panic!("expected Object");
        }
        assert!(!parsed.prose.contains("[OBJECT"));
    }

    #[test]
    fn object_with_multiple_attributes_joins_state() {
        // Multiple key=value pairs: id = first token, state = space-joined rest.
        let raw = "[OBJECT npc_guard disposition=hostile weapon=drawn]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Object { id, state } = &parsed.commands[0] {
            assert_eq!(id, "npc_guard");
            assert_eq!(state, "disposition=hostile weapon=drawn");
        } else {
            panic!("expected Object");
        }
    }

    #[test]
    fn strict_id_state_format_still_works() {
        // The legacy strict format (id=X state=Y) must still parse to the
        // same shape — the fallback must not regress the documented contract.
        let raw = "Alex approaches. [OBJECT id=iron_chest state=open] Click.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Object { id, state } = &parsed.commands[0] {
            assert_eq!(id, "iron_chest");
            assert_eq!(state, "open");
        } else {
            panic!("expected Object");
        }
    }

    #[test]
    fn character_turn_without_close_consumes_rest() {
        // Graceful: no end tag → treat rest of output as the line.
        let raw = "Alex nods. [CHARACTER_TURN:gorm] Welcome, traveller.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::CharacterTurn { npc_id, line } = &parsed.commands[0] {
            assert_eq!(npc_id, "gorm");
            assert_eq!(line, "Welcome, traveller.");
        } else {
            panic!("expected CharacterTurn");
        }
    }

    // ---------- [TIME ...] clock command (Seam #4) ----------

    #[test]
    fn extracts_time_command_day_and_clock() {
        // The canonical form: day + clock in one bracket.
        let raw = "Night falls. [TIME Day 3, 14:00] The candles flicker.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        match &parsed.commands[0] {
            BracketCommand::Time { minutes, raw } => {
                // Day 3 → (3-1)*1440 = 2880; 14:00 → 14*60 = 840; total 3720.
                assert_eq!(*minutes, 3720);
                assert_eq!(raw, "Day 3, 14:00");
            }
            other => panic!("expected Time, got {other:?}"),
        }
        // The bracket must be stripped from prose (same invariant as OBJECT/FX).
        assert!(!parsed.prose.contains("[TIME"));
        assert!(parsed.prose.contains("Night falls."));
        assert!(parsed.prose.contains("The candles flicker."));
    }

    #[test]
    fn extracts_time_command_day_only() {
        let raw = "We travel. [TIME Day 5]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Time { minutes, .. } = &parsed.commands[0] {
            assert_eq!(*minutes, (5 - 1) * 1440);
        } else {
            panic!("expected Time");
        }
    }

    #[test]
    fn extracts_time_command_clock_only() {
        let raw = "[TIME 14:00] Noon arrives.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Time { minutes, .. } = &parsed.commands[0] {
            // Bare clock: day defaults to 1 → (1-1)*1440 + 14*60 = 840.
            assert_eq!(*minutes, 840);
        } else {
            panic!("expected Time");
        }
    }

    #[test]
    fn time_command_12h_am_pm() {
        let raw = "[TIME 08:00 AM, Day 1]";
        let parsed = parse(raw);
        if let BracketCommand::Time { minutes, .. } = &parsed.commands[0] {
            // Day 1, 08:00 AM → 0 + 8*60 = 480.
            assert_eq!(*minutes, 480);
        } else {
            panic!("expected Time");
        }

        let raw2 = "[TIME 8:00 PM, Day 1]";
        let parsed2 = parse(raw2);
        if let BracketCommand::Time { minutes, .. } = &parsed2.commands[0] {
            // 8 PM → 20:00 → 20*60 = 1200.
            assert_eq!(*minutes, 1200);
        } else {
            panic!("expected Time");
        }
    }

    #[test]
    fn malformed_time_bracket_emitted_as_literal() {
        // No parseable date/time signal → bracket survives as literal prose.
        let raw = "Strange. [TIME lunchtime] Hmm.";
        let parsed = parse(raw);
        assert!(parsed.commands.is_empty());
        assert!(parsed.prose.contains("[TIME lunchtime]"));
    }

    // ---------- parse_in_world_time (unit tests for the primitive) ----------

    #[test]
    fn parse_in_world_time_day_only() {
        assert_eq!(parse_in_world_time("Day 1"), Some(0));
        assert_eq!(parse_in_world_time("Day 2"), Some(1440));
        assert_eq!(parse_in_world_time("Day 3"), Some(2880));
        // Case-insensitive + "D" abbreviation.
        assert_eq!(parse_in_world_time("day 5"), Some(5760));
        assert_eq!(parse_in_world_time("D 4"), Some(4320));
    }

    #[test]
    fn parse_in_world_time_clock_only_defaults_day_one() {
        // Bare clock with no day → day defaults to 1 (campaign day 1).
        assert_eq!(parse_in_world_time("14:00"), Some(840));
        assert_eq!(parse_in_world_time("00:00"), Some(0));
        assert_eq!(parse_in_world_time("23:59"), Some(1439));
    }

    #[test]
    fn parse_in_world_time_day_and_clock_combined() {
        // The canonical narrator format.
        assert_eq!(parse_in_world_time("Day 3, 14:00"), Some(3720));
        // Reversed order works too.
        assert_eq!(parse_in_world_time("14:00, Day 3"), Some(3720));
    }

    #[test]
    fn parse_in_world_time_12h_with_meridian() {
        assert_eq!(parse_in_world_time("08:00 AM, Day 1"), Some(480));
        assert_eq!(parse_in_world_time("8:00 PM, Day 1"), Some(1200));
        // 12 AM → 0 hours; 12 PM → 12 hours.
        assert_eq!(parse_in_world_time("12:00 AM"), Some(0));
        assert_eq!(parse_in_world_time("12:00 PM"), Some(720));
    }

    #[test]
    fn parse_in_world_time_calendar_date() {
        // 01/01/0001 → the epoch itself → 0 minutes. (Calendar dates are
        // absolute: anchored to 0001-01-01.)
        assert_eq!(parse_in_world_time("01/01/0001"), Some(0));
        // 02/01/0001 → one day later → 1440 minutes.
        assert_eq!(parse_in_world_time("02/01/0001"), Some(1440));
        // 2-digit year → 2000-offset.
        let mins = parse_in_world_time("01/01/26").unwrap();
        // Year 2026 is well into positive territory; just sanity-check it's
        // a large positive number (the exact value depends on leap years).
        assert!(mins > 1_000_000_000);
    }

    #[test]
    fn parse_in_world_time_unparseable_returns_none() {
        assert_eq!(parse_in_world_time(""), None);
        assert_eq!(parse_in_world_time("   "), None);
        assert_eq!(parse_in_world_time("lunchtime"), None);
        assert_eq!(parse_in_world_time("garbage"), None);
    }

    #[test]
    fn parse_in_world_time_skips_malformed_fragments() {
        // A malformed clock fragment is skipped; the day still parses.
        assert_eq!(parse_in_world_time("Day 5, lunch"), Some(5760));
        // A malformed day token is skipped; the clock still parses.
        assert_eq!(parse_in_world_time("Dayz, 14:00"), Some(840));
    }

    // ── 2026-07-27 extra-spaces normalization tests ──────────────────────
    // When a bracket is stripped, the spaces immediately before + after it
    // survive in the prose as a double space. normalize_whitespace collapses
    // those runs. These tests pin the behavior.

    #[test]
    fn inline_bracket_does_not_leave_double_space() {
        // The classic shape: bracket emitted inline (despite the prompt
        // asking for own-line). Before the fix this produced
        // "Mara nods.  The fire crackles." (two spaces). After: single space.
        let raw = "Mara nods. [OBJECT id=door state=open] The fire crackles.";
        let parsed = parse(raw);
        assert!(!parsed.prose.contains("  "), "double space leaked: {:?}", parsed.prose);
        assert_eq!(parsed.prose, "Mara nods. The fire crackles.");
    }

    #[test]
    fn multiple_inline_brackets_collapse_cleanly() {
        let raw = "A [FX rain] B [FX thunder] C";
        let parsed = parse(raw);
        assert_eq!(parsed.prose, "A B C");
        assert!(!parsed.prose.contains("  "));
    }

    #[test]
    fn newline_preserved_as_paragraph_break() {
        // A bracket on its own line (the prompt's preferred shape) leaves
        // a blank line after stripping. normalize keeps the newline as a
        // paragraph break but trims trailing space before it.
        let raw = "Para one.\n[OBJECT id=x state=y]\nPara two.";
        let parsed = parse(raw);
        assert_eq!(parsed.prose, "Para one.\n\nPara two.");
    }

    #[test]
    fn trailing_space_before_eof_stripped() {
        let raw = "Text [FX rain] ";
        let parsed = parse(raw);
        assert_eq!(parsed.prose, "Text");
    }

    #[test]
    fn pre_existing_double_spaces_in_prose_are_also_collapsed() {
        // Defensive: even if the model itself emits double spaces (not just
        // bracket-stripping artifacts), normalize fixes them. The narrator
        // is a 12B model; prose hygiene is not guaranteed.
        let raw = "The  fire   crackles.";
        let parsed = parse(raw);
        assert_eq!(parsed.prose, "The fire crackles.");
    }

    // ========================================================================
    // Bug A fix tests (2026-07-28): fenced-JSON dual parser.
    // Every test mirrors an existing bracket test's intent, just with the
    // JSON input shape the model actually emits.
    // ========================================================================

    #[test]
    fn json_effect_basic_parses() {
        // The exact shape the model emitted in the 2026-07-28 playtest.
        let raw = "Prose before.\n```json\n{ \"effect_name\": \"exploration\", \"effect_label\": \"exploration\", \"effect_duration_minutes\": 15 }\n```\nProse after.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1, "one effect command");
        match &parsed.commands[0] {
            BracketCommand::Effect { label, polarity, duration_minutes, .. } => {
                assert_eq!(label, "exploration");
                assert_eq!(*duration_minutes, 15);
                // "exploration" has no debuff keyword → inferred Buff.
                assert_eq!(*polarity, Polarity::Buff);
            }
            other => panic!("expected Effect, got {:?}", other),
        }
        // The fence is gone from the prose; surrounding prose preserved.
        assert!(!parsed.prose.contains("```"));
        assert!(!parsed.prose.contains("effect_name"));
        assert!(parsed.prose.contains("Prose before."));
        assert!(parsed.prose.contains("Prose after."));
    }

    #[test]
    fn json_effect_with_polarity_and_aliases() {
        // Explicit polarity + the canonical field names + a debuff keyword.
        let raw = "```json\n{ \"type\": \"effect\", \"label\": \"Berserk Rage\", \"polarity\": \"buff\", \"duration_minutes\": 60 }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Effect { label, polarity, duration_minutes, .. } = &parsed.commands[0] {
            assert_eq!(label, "Berserk Rage");
            assert_eq!(*polarity, Polarity::Buff);
            assert_eq!(*duration_minutes, 60);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_effect_inferred_debuff_from_keyword() {
        // No polarity field → engine infers from label. "Poisoned" is a
        // debuff keyword → Debuff.
        let raw = "```json\n{ \"type\": \"effect\", \"name\": \"Poisoned\", \"duration\": 120 }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Effect { label, polarity, .. } = &parsed.commands[0] {
            assert_eq!(label, "Poisoned");
            assert_eq!(*polarity, Polarity::Debuff);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_effect_zero_duration_is_permanent() {
        let raw = "```json\n{ \"type\": \"effect\", \"label\": \"Cursed\", \"polarity\": \"debuff\", \"duration_minutes\": 0 }\n```";
        let parsed = parse(raw);
        if let BracketCommand::Effect { duration_minutes, .. } = &parsed.commands[0] {
            assert_eq!(*duration_minutes, 0, "zero = permanent sentinel");
        } else {
            panic!("wrong variant");
        }
    }

    // ---- Phase 4 §11.44 (Component 1): EFFECT `kind` discriminator ----

    #[test]
    fn effect_keyvalue_kind_disguise_parsed() {
        // key=value form carries the optional `kind` discriminator.
        let raw = "[EFFECT city guard uniform buff 0 kind=disguise]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        match &parsed.commands[0] {
            BracketCommand::Effect { label, polarity, duration_minutes, tag_kind } => {
                assert_eq!(label, "city guard uniform");
                assert_eq!(*polarity, Polarity::Buff);
                assert_eq!(*duration_minutes, 0);
                assert_eq!(tag_kind, "disguise", "kind must thread through");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn effect_keyvalue_omits_kind_defaults_empty() {
        // Existing key=value form (no kind) → empty string, NOT a parse failure.
        let raw = "[EFFECT label=Berserk duration=60]";
        let parsed = parse(raw);
        if let BracketCommand::Effect { tag_kind, .. } = &parsed.commands[0] {
            assert_eq!(tag_kind, "", "absent kind defaults to empty");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn effect_positional_form_never_carries_kind() {
        // Positional form has no kind channel — always empty (backwards-compat).
        let raw = "[EFFECT Berserk Rage buff 60]";
        let parsed = parse(raw);
        if let BracketCommand::Effect { tag_kind, .. } = &parsed.commands[0] {
            assert_eq!(tag_kind, "");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_effect_kind_disguise_parsed() {
        // JSON form carries kind alongside the bracket-form parity.
        let raw = "```json\n{ \"type\": \"effect\", \"label\": \"merchant robes\", \"polarity\": \"buff\", \"duration_minutes\": 0, \"kind\": \"disguise\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Effect { label, tag_kind, .. } = &parsed.commands[0] {
            assert_eq!(label, "merchant robes");
            assert_eq!(tag_kind, "disguise");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn effect_kind_case_sensitive_value_preserved() {
        // The kind value is preserved verbatim (not lowercased) so future
        // kinds aren't accidentally mangled. Only the discriminator logic
        // downstream reads it case-sensitively against recognized kinds.
        let raw = "[EFFECT novice robe buff 0 kind=Disguise]";
        let parsed = parse(raw);
        if let BracketCommand::Effect { tag_kind, .. } = &parsed.commands[0] {
            assert_eq!(tag_kind, "Disguise");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_milestone_parses() {
        let raw = "He pulls you from the river.\n```json\n{ \"type\": \"milestone\", \"npc_id\": \"npc.marcus\", \"event_id\": \"saved_life\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Milestone { npc_id, event_id } = &parsed.commands[0] {
            assert_eq!(npc_id, "npc.marcus");
            assert_eq!(event_id, "saved_life");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_milestone_infers_kind_from_fields() {
        // No `type` discriminator — infer from `event_id` presence.
        let raw = "```json\n{ \"npc\": \"npc.smuggler\", \"event\": \"betrayed_trust\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Milestone { npc_id, event_id } = &parsed.commands[0] {
            assert_eq!(npc_id, "npc.smuggler");
            assert_eq!(event_id, "betrayed_trust");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_task_parses() {
        let raw = "```json\n{ \"type\": \"task\", \"npc_id\": \"npc.marcus\", \"description\": \"scout the bandit camp\", \"difficulty\": \"challenging\", \"suitability\": \"adequate\", \"eta_minutes\": 240 }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Task { npc_id, description, difficulty, suitability, eta_minutes } =
            &parsed.commands[0]
        {
            assert_eq!(npc_id, "npc.marcus");
            assert_eq!(description, "scout the bandit camp");
            assert_eq!(difficulty, "challenging");
            assert_eq!(suitability, "adequate");
            assert_eq!(*eta_minutes, 240);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_task_rejects_zero_and_negative_eta() {
        for body in [
            r#"{ "type": "task", "npc_id": "x", "description": "d", "eta_minutes": 0 }"#,
            r#"{ "type": "task", "npc_id": "x", "description": "d", "eta_minutes": -5 }"#,
        ] {
            let raw = format!("```json\n{}\n```", body);
            let parsed = parse(&raw);
            assert_eq!(parsed.commands.len(), 0, "rejected: {}", body);
        }
    }

    #[test]
    fn json_time_parses_raw_string() {
        let raw = "```json\n{ \"type\": \"time\", \"raw\": \"Day 3, 14:00\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        if let BracketCommand::Time { raw: raw_str, .. } = &parsed.commands[0] {
            assert_eq!(raw_str, "Day 3, 14:00");
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_time_accepts_explicit_minutes() {
        let raw = "```json\n{ \"type\": \"time\", \"minutes\": 10080 }\n```";
        let parsed = parse(raw);
        if let BracketCommand::Time { minutes, .. } = &parsed.commands[0] {
            assert_eq!(*minutes, 10080);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn json_object_and_fx_and_character_turn() {
        // Three commands in three separate fences — covers the less-common
        // variants in one test.
        let raw = "```json\n{ \"type\": \"object\", \"id\": \"door_cellar\", \"state\": \"open\" }\n```\nMiddle.\n```json\n{ \"type\": \"fx\", \"effect\": \"rain\" }\n```\nMore.\n```json\n{ \"type\": \"character_turn\", \"npc_id\": \"npc.mara\", \"line\": \"Hello.\" }\n```";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 3);
        assert!(matches!(parsed.commands[0], BracketCommand::Object { .. }));
        assert!(matches!(parsed.commands[1], BracketCommand::Fx { .. }));
        assert!(matches!(parsed.commands[2], BracketCommand::CharacterTurn { .. }));
        assert!(parsed.prose.contains("Middle."));
        assert!(parsed.prose.contains("More."));
        assert!(!parsed.prose.contains("```"));
    }

    #[test]
    fn json_malformed_is_noop_not_panic() {
        // Garbage bodies: parser must drop silently, never panic.
        let bodies = [
            "```json\n{ this is not json }\n```",
            "```json\n\n```",                       // empty
            "```json\n[1, 2, 3]\n```",              // array, not object
            "```json\n\"just a string\"\n```",      // scalar
            "```json\n{ \"type\": \"unknown\" }\n```", // unknown kind
            "```json\n{ \"type\": \"effect\" }\n```",  // missing required label
        ];
        for raw in bodies {
            let parsed = parse(raw);
            assert_eq!(parsed.commands.len(), 0, "rejected: {}", raw);
            assert!(!parsed.prose.contains("```"), "fence stripped from prose: {}", raw);
        }
    }

    #[test]
    fn json_mixed_with_brackets_in_one_turn() {
        // Both formats coexist — brackets and JSON in the same turn.
        let raw = "Start.\n[FX thunder]\nMiddle.\n```json\n{ \"type\": \"effect\", \"label\": \"Shaken\", \"polarity\": \"debuff\", \"duration_minutes\": 30 }\n```\nEnd.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 2, "one bracket + one JSON command");
        assert!(matches!(parsed.commands[0], BracketCommand::Fx { .. }));
        assert!(matches!(parsed.commands[1], BracketCommand::Effect { .. }));
        assert!(parsed.prose.contains("Start."));
        assert!(parsed.prose.contains("Middle."));
        assert!(parsed.prose.contains("End."));
        assert!(!parsed.prose.contains("[FX"));
        assert!(!parsed.prose.contains("```"));
    }

    #[test]
    fn json_unterminated_fence_takes_rest_as_body() {
        // No closing fence — body up to EOF. If it's valid JSON, it parses.
        let raw = "Prose.\n```json\n{ \"type\": \"fx\", \"effect\": \"fog\" }";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1);
        assert!(matches!(parsed.commands[0], BracketCommand::Fx { .. }));
        assert!(parsed.prose.contains("Prose."));
        assert!(!parsed.prose.contains("```"));
    }

    #[test]
    fn json_fence_with_character_turn_bracket_mixed() {
        // The load-bearing CHARACTER_TURN text_after contract must still work
        // when a JSON fence precedes it — the fence is stripped from the
        // raw BEFORE the bracket loop runs, so bracket indices are correct.
        // Note: bracket commands are collected first (the while-loop), then
        // JSON commands appended after — so CHARACTER_TURN is commands[0]
        // and the JSON Fx is commands[1].
        let raw = "```json\n{ \"type\": \"fx\", \"effect\": \"rain\" }\n```\n[CHARACTER_TURN:npc.mara]Hello there.[CHARACTER_TURN:end]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 2);
        if let BracketCommand::CharacterTurn { npc_id, line } = &parsed.commands[0] {
            assert_eq!(npc_id, "npc.mara");
            assert_eq!(line, "Hello there.");
        } else {
            panic!("expected CharacterTurn at [0], got {:?}", parsed.commands[0]);
        }
        assert!(matches!(parsed.commands[1], BracketCommand::Fx { .. }));
    }

    #[test]
    fn json_fence_followed_by_inline_bracket_no_text_corruption() {
        // Regression guard: the fence-stripping must not corrupt bracket
        // positions. The prose between a fence and a bracket must survive.
        let raw = "```json\n{ \"type\": \"object\", \"id\": \"x\", \"state\": \"y\" }\n```\nThe door creaks. [FX vignette]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 2);
        assert!(parsed.prose.contains("The door creaks."));
    }

    #[test]
    fn extract_fenced_json_no_opener_returns_input_unchanged() {
        let (prose, bodies) = extract_fenced_json("just prose, no fences");
        assert_eq!(prose, "just prose, no fences");
        assert!(bodies.is_empty());
    }

    // ========================================================================
    // Case-insensitive command-verb matching (§11.40.F fix, 2026-07-28).
    // The live narrator model (Gemma 12B) sometimes emits bracket commands
    // with non-canonical casing — most observed: `[CHARACTER_Turn:...]`
    // (capital T). Before the fix, the parser was case-sensitive on the
    // verb prefix, so the variant leaked as literal prose. These tests pin
    // the case-insensitive contract for all 7 verbs + the CHARACTER_TURN
    // close-tag, AND guard that legitimate mixed-case prose (e.g. an aside
    // like `[The old road]`) still passes through verbatim (the case-fold
    // is for MATCHING ONLY; the None-arm emits the original bracket text).
    // ========================================================================

    #[test]
    fn character_turn_capital_t_variant_parsed() {
        // The canonical live bug: `[CHARACTER_Turn:...]` with a capital T.
        // Must parse to a CharacterTurn command, NOT leak as literal prose.
        let raw = "[CHARACTER_Turn:mara] Welcome, traveler. [CHARACTER_Turn:end] She smiles.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1, "got: {:?}", parsed);
        match &parsed.commands[0] {
            BracketCommand::CharacterTurn { npc_id, line } => {
                assert_eq!(npc_id, "mara", "npc_id preserved");
                assert_eq!(line, "Welcome, traveler.", "body extracted");
            }
            _ => panic!("expected CharacterTurn, got: {:?}", parsed.commands[0]),
        }
        // Trailing prose survives; the body was consumed.
        assert!(parsed.prose.contains("She smiles."));
        assert!(!parsed.prose.contains("CHARACTER_Turn"));
        assert!(!parsed.prose.contains("Welcome, traveler."));
    }

    #[test]
    fn character_turn_mixed_case_close_tag_too() {
        // The close tag is the same variant shape: `[CHARACTER_Turn:end]`.
        // find_ci must match it the same as the canonical form. If only the
        // open tag were case-insensitive, the close would leak as a second
        // (empty) CharacterTurn — this test catches that regression.
        let raw = "[CHARACTER_TURN:mara] Hello there. [CHARACTER_Turn:end] Bye.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 1, "expected exactly 1 command (close tag must not double-count): {:?}", parsed);
        match &parsed.commands[0] {
            BracketCommand::CharacterTurn { npc_id, line } => {
                assert_eq!(npc_id, "mara");
                assert_eq!(line, "Hello there.");
            }
            _ => panic!("expected CharacterTurn"),
        }
        assert!(parsed.prose.contains("Bye."));
    }

    #[test]
    fn all_lowercase_command_verbs_parsed() {
        // Defensive: all-lowercase variants of every command verb. The model
        // is unlikely to emit these but the case-insensitive contract covers
        // them; this test pins that coverage so a future "ASCII-only upper"
        // regression is caught.
        let raw = "[fx thunder] [object id=door state=open] [time Day 3, 14:00]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 3, "got: {:?}", parsed);
        assert!(matches!(parsed.commands[0], BracketCommand::Fx { .. }));
        assert!(matches!(parsed.commands[1], BracketCommand::Object { .. }));
        assert!(matches!(parsed.commands[2], BracketCommand::Time { .. }));
    }

    #[test]
    fn effect_milestone_task_case_variants_parsed() {
        // The Phase 3 commands (EFFECT/MILESTONE/TASK) also case-fold. Pins
        // all three so a future "only CHARACTER_TURN is ci" regression is
        // caught.
        let raw = "[Effect Berserk Rage buff 60][Milestone npc.mara shared_drink][Task npc.mara scout | challenging adequate 1440]";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 3, "got: {:?}", parsed);
        assert!(matches!(parsed.commands[0], BracketCommand::Effect { .. }));
        assert!(matches!(parsed.commands[1], BracketCommand::Milestone { .. }));
        assert!(matches!(parsed.commands[2], BracketCommand::Task { .. }));
    }

    #[test]
    fn stray_bracket_with_mixed_case_prose_emitted_verbatim() {
        // False-positive guard: a legitimate bracket in prose that is NOT a
        // command (e.g. an aside like `[The old road]`) must emit VERBATIM
        // with original casing — the case-fold is for MATCHING ONLY. If the
        // fix naively lowercased the whole bracket, this would corrupt the
        // text. The None-arm must use the original string.
        let raw = "He took the [The old road] home and rested.";
        let parsed = parse(raw);
        assert_eq!(parsed.commands.len(), 0, "stray bracket should not parse as command");
        assert!(
            parsed.prose.contains("[The old road]"),
            "stray bracket must emit verbatim with original casing, got: {:?}",
            parsed.prose
        );
    }

    #[test]
    fn strip_prefix_ci_returns_original_case_rest() {
        // Unit test for the helper itself: the returned slice must come from
        // the ORIGINAL haystack (preserving argument case), not a lowercased
        // copy. Only the prefix is folded for matching.
        assert_eq!(strip_prefix_ci("CHARACTER_Turn:mara", "CHARACTER_TURN:"), Some("mara"));
        assert_eq!(strip_prefix_ci("character_turn:mara", "CHARACTER_TURN:"), Some("mara"));
        assert_eq!(strip_prefix_ci("FX rainstorm", "FX"), Some(" rainstorm"));
        assert_eq!(strip_prefix_ci("fx rainstorm", "FX"), Some(" rainstorm"));
        assert_eq!(strip_prefix_ci("OBJECT id=Door", "OBJECT"), Some(" id=Door"));
        // Capital D in "Door" must survive (argument case preserved).
        let rest = strip_prefix_ci("OBJECT id=Door", "OBJECT").unwrap();
        assert!(rest.contains("Door"), "argument case must be preserved, got: {:?}", rest);
        // No match → None.
        assert_eq!(strip_prefix_ci("SOMETHING_ELSE", "OBJECT"), None);
        // Shorter than prefix → None (no panic).
        assert_eq!(strip_prefix_ci("FX", "CHARACTER_TURN:"), None);
    }

    #[test]
    fn find_ci_matches_case_variants_and_respects_boundaries() {
        // Unit test for the close-tag helper. Must find the canonical form
        // AND case variants; must not match across a multibyte char boundary.
        assert_eq!(find_ci("text [CHARACTER_TURN:end] more", "[CHARACTER_TURN:end]"), Some(5));
        assert_eq!(find_ci("text [CHARACTER_Turn:end] more", "[CHARACTER_TURN:end]"), Some(5));
        assert_eq!(find_ci("text [character_turn:end] more", "[CHARACTER_TURN:end]"), Some(5));
        assert_eq!(find_ci("no match here", "[CHARACTER_TURN:end]"), None);
        assert_eq!(find_ci("anything", ""), Some(0));
    }
}
