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
    /// The in-world clock advanced. `minutes` is the authoritative value
    /// (minutes since 0001-01-01, parsed by [`parse_in_world_time`]); `raw`
    /// is the verbatim string the narrator emitted (kept for diagnostics +
    /// the debug panel). The Rust side owns the clock — this is the ONLY
    /// path that writes `WorldSchema::world_clock`.
    Time { minutes: i64, raw: String },
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
fn parse_one(bracket: &str, text_after: &str) -> Option<(BracketCommand, usize)> {
    let bracket = bracket.trim();

    if let Some(rest) = bracket.strip_prefix("CHARACTER_TURN:") {
        let npc_id = rest.trim().to_string();
        if npc_id == "end" || npc_id.is_empty() {
            // A stray close tag or empty open tag: drop it.
            return Some((BracketCommand::CharacterTurn {
                npc_id: String::new(),
                line: String::new(),
            }, 0));
        }
        // Find the matching [CHARACTER_TURN:end] in `text_after`. The body
        // between is the NPC's spoken line.
        let close = "[CHARACTER_TURN:end]";
        if let Some(end_idx) = text_after.find(close) {
            let line = text_after[..end_idx].trim().to_string();
            return Some((
                BracketCommand::CharacterTurn { npc_id, line },
                end_idx + close.len(),
            ));
        }
        // No close tag: treat the rest of the output as the line (graceful).
        let line = text_after.trim().to_string();
        return Some((
            BracketCommand::CharacterTurn { npc_id, line },
            text_after.len(),
        ));
    }

    if let Some(rest) = bracket.strip_prefix("OBJECT") {
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

    if let Some(rest) = bracket.strip_prefix("FX") {
        let effect = rest.trim().to_string();
        if !effect.is_empty() {
            return Some((BracketCommand::Fx { effect }, 0));
        }
        return None;
    }

    // [TIME <in-world timestamp>] — Seam #4 clock advance. Single-region like
    // OBJECT/FX. The body is parsed by parse_in_world_time into minutes-since-
    // epoch; on failure the bracket is emitted as literal prose (better to
    // surface a malformed timestamp than silently drop it).
    if let Some(rest) = bracket.strip_prefix("TIME") {
        let raw = rest.trim().to_string();
        if let Some(minutes) = parse_in_world_time(&raw) {
            return Some((BracketCommand::Time { minutes, raw }, 0));
        }
        return None;
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
}
