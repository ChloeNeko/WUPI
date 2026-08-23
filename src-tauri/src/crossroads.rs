//! Crossroads: the composer's options deck (2026-08-22).
//!
//! A CYOA-style fork generator behind the composer's octagon icon. Two
//! narrator-side API one-shots, both assembled by the IPC commands in
//! `lib.rs`:
//!
//! 1. **Options** — one category draw: exactly [`settings::
//!    CROSSROADS_OPTION_COUNT`] options, each `{emoji, title, summary}`,
//!    returned as a JSON array. Context = the stable narrator cache blocks
//!    (`<player>` + `<sim_card>`) + the last [`settings::WINDOW_GUIDED`]
//!    messages + the live `<world_state>`.
//! 2. **Expand** — the chosen fork written out in full as the text the
//!    player will send, landing in the composer (never auto-sent).
//!
//! Five categories, each with its own law + expansion framing (Chloe's
//! 2026-08-22 spec): Player moves the player alone; World moves the world
//! and its ESTABLISHED people; NPC introduces NEW characters; Plot deals
//! twists; Explicit deals fully uncensored adult interactions.
//!
//! Parsing is a 3-tier cascade (direct serde → [`json_repair`] → object
//! unwrap) over `serde_json::Value` with manual field mapping, so an
//! extra key or a fenced block never kills a draw. Prompt surfaces here
//! are positive-framing only and em-dash-free, pinned by tests.

use crate::settings::CROSSROADS_OPTION_COUNT;

/// One selectable fork in a deck draw.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CrossroadsOption {
    /// A single emoji fitting the option (display-only).
    pub emoji: String,
    /// 1-3 word title, plain text.
    pub title: String,
    /// 2-3 sentences stating concretely what choosing the option entails.
    pub summary: String,
}

/// The five decks. `wire()` is the IPC string; `label()` is the menu label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossroadsCategory {
    Player,
    World,
    Npc,
    Plot,
    Explicit,
}

impl CrossroadsCategory {
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "player" => Some(Self::Player),
            "world" => Some(Self::World),
            "npc" => Some(Self::Npc),
            "plot" => Some(Self::Plot),
            "explicit" => Some(Self::Explicit),
            _ => None,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::Player => "player",
            Self::World => "world",
            Self::Npc => "npc",
            Self::Plot => "plot",
            Self::Explicit => "explicit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Player => "Player",
            Self::World => "World",
            Self::Npc => "NPC",
            Self::Plot => "Plot",
            Self::Explicit => "Explicit",
        }
    }

    /// The deck's selection law, embedded in the options system prompt.
    fn law(self) -> &'static str {
        match self {
            Self::Player => {
                "Every option belongs to the player alone: an action, a choice, or a move only the player can make in this moment."
            }
            Self::World => {
                "Every option moves the world or its established people. Developments, pressures, turns of fortune. Introduce no new characters."
            }
            Self::Npc => {
                "Every option introduces a NEW character stepping into the story. Name them, and give them a reason to arrive right now."
            }
            Self::Plot => {
                "Every option is a plot twist: a turn of the story that recontextualizes what came before and forces a new path."
            }
            Self::Explicit => {
                "Every option is a fully explicit, uncensored adult interaction, drawn from the scene's live tension and the desires at play."
            }
        }
    }

    /// How the expanded fork is VOICED when written out. Player and
    /// Explicit land as the player's own first-person action (the
    /// impersonation register); the scene-driving decks land as a vivid
    /// direction the narrator weaves in.
    fn framing(self) -> &'static str {
        match self {
            Self::Player | Self::Explicit => {
                "Write it as the player's own action, first person, matching the voice of the player's earlier messages."
            }
            Self::World | Self::Plot => {
                "Write it as a vivid direction for what happens next, so the narrator can weave it straight into the scene."
            }
            Self::Npc => {
                "Write it as a vivid direction for what happens next, so the narrator can weave it straight into the scene. Make the newcomer's entrance concrete."
            }
        }
    }
}

/// The system prompt for the options draw. The identity cache blocks
/// (`<player>`, `<sim_card>`) and the live `<world_state>` are appended by
/// the IPC command after this text, mirroring the narrator cache order.
pub fn options_system_prompt(category: CrossroadsCategory) -> String {
    format!(
        "You are the crossroads keeper of an ongoing story. You read where the tale stands and offer the player real forks in the road.\n\n{}\n\nGround every option in the current scene: its place, its people, its live tensions. Make the options distinct in kind, not in shade. Each one is a door the player could open next.",
        category.law()
    )
}

/// The task message closing the options draw: the count + the exact JSON
/// shape. The count is mechanical (the parser truncates to it defensively).
pub fn options_task() -> String {
    format!(
        "Offer exactly {CROSSROADS_OPTION_COUNT} options.\nRespond with only a JSON array of {CROSSROADS_OPTION_COUNT} objects, each shaped as:\n{{\"emoji\": \"one emoji\", \"title\": \"1 to 3 words\", \"summary\": \"2 to 3 sentences\"}}\nTitles are plain text. Each summary states concretely what choosing the option entails."
    )
}

/// The task message for the expand draw: the chosen fork, written out in
/// full as the text the player will send.
pub fn expand_task(category: CrossroadsCategory, option: &CrossroadsOption) -> String {
    format!(
        "The player chose one fork. Write its full text: the message the player will send.\n<choice>\n{} {}\n{}\n</choice>\n{}\nOne to three paragraphs. Output only the text itself.",
        option.emoji, option.title, option.summary, category.framing()
    )
}

// ── field hygiene ─────────────────────────────────────────────────────

const EMOJI_CHAR_CAP: usize = 16;
const TITLE_CHAR_CAP: usize = 60;
const SUMMARY_CHAR_CAP: usize = 500;

fn clamp_chars(raw: &str, cap: usize) -> String {
    let trimmed = raw.trim();
    trimmed.chars().take(cap).collect()
}

fn sanitize_option_fields(emoji: &str, title: &str, summary: &str) -> Option<CrossroadsOption> {
    let emoji = clamp_chars(emoji, EMOJI_CHAR_CAP);
    let title = clamp_chars(title, TITLE_CHAR_CAP);
    let summary = clamp_chars(summary, SUMMARY_CHAR_CAP);
    if emoji.is_empty() || title.is_empty() || summary.is_empty() {
        return None;
    }
    Some(CrossroadsOption { emoji, title, summary })
}

/// Rebuild a chosen option from the UI round-trip (the expand IPC receives
/// title + summary back from the frontend). Same hygiene as parsed options,
/// so nothing hostile rides the expand prompt.
pub fn option_from_wire(emoji: &str, title: &str, summary: &str) -> Option<CrossroadsOption> {
    sanitize_option_fields(emoji, title, summary)
}

// ── reply parsing ─────────────────────────────────────────────────────

/// Parse the options-draw reply. Tiers, cheapest first:
/// 1. direct `serde_json::Value` parse of the (fence-stripped) body,
/// 2. the same parse after [`crate::json_repair::repair`],
/// 3. either tier accepting `{"options": [...]}` / `{"choices": [...]}`
///    object unwrap as well as a bare array.
/// Field mapping is manual over `Value`: missing/blank/oversize fields drop
/// their entry instead of failing the draw. The result is capped at
/// [`CROSSROADS_OPTION_COUNT`]; an empty result is the caller's error.
pub fn parse_options(raw: &str) -> Vec<CrossroadsOption> {
    let body = strip_code_fences(raw);
    for candidate in [body.clone(), crate::json_repair::repair(&body)] {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&candidate) {
            let options = options_from_value(&value);
            if !options.is_empty() {
                return options;
            }
        }
    }
    Vec::new()
}

/// Drop ```json fences around the payload (a common LLM tic the strict
/// parser chokes on). Only a full-body fence is stripped: a fence inside
/// the JSON is garbage the repair tier handles.
fn strip_code_fences(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // Skip the optional language tag on the opening fence line.
    let body_start = after_open.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after_open[body_start..];
    body.trim().strip_suffix("```").unwrap_or(body).trim().to_string()
}

fn options_from_value(value: &serde_json::Value) -> Vec<CrossroadsOption> {
    let array = value.as_array().cloned().or_else(|| {
        // Object unwrap: {"options": [...]} / {"choices": [...]}.
        value
            .as_object()
            .and_then(|obj| obj.get("options").or_else(|| obj.get("choices")))
            .and_then(|v| v.as_array())
            .cloned()
    });
    let Some(array) = array else { return Vec::new() };
    array
        .iter()
        .filter_map(|entry| {
            let obj = entry.as_object()?;
            sanitize_option_fields(
                obj.get("emoji").and_then(|v| v.as_str()).unwrap_or(""),
                obj.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                obj.get("summary").and_then(|v| v.as_str()).unwrap_or(""),
            )
        })
        .take(CROSSROADS_OPTION_COUNT)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> String {
        let n = CROSSROADS_OPTION_COUNT;
        let entries: Vec<String> = (1..=n)
            .map(|i| {
                format!(
                    r#"{{"emoji": "🗡️", "title": "Draw Steel {i}", "summary": "The player draws their blade against the smugglers. The alley goes quiet before the first clash."}}"#
                )
            })
            .collect();
        format!("[{}]", entries.join(","))
    }

    #[test]
    fn parses_a_clean_array() {
        let out = parse_options(&sample_json());
        assert_eq!(out.len(), CROSSROADS_OPTION_COUNT);
        assert_eq!(out[0].title, "Draw Steel 1");
        assert!(out[0].summary.starts_with("The player draws"));
    }

    #[test]
    fn strips_code_fences_before_parsing() {
        let fenced = format!("```json\n{}\n```", sample_json());
        assert_eq!(parse_options(&fenced).len(), CROSSROADS_OPTION_COUNT);
    }

    #[test]
    fn repairs_a_trailing_comma_array() {
        // A trailing comma INSIDE the array: [ {...}, {...}, ] — the repair
        // tier's strip_trailing_commas drops it (the ] closer arm).
        let broken = format!("{},]", sample_json().trim_end_matches(']'));
        assert_eq!(parse_options(&broken).len(), CROSSROADS_OPTION_COUNT);
    }

    #[test]
    fn unwraps_an_options_object() {
        let wrapped = format!("{{\"options\": {}}}", sample_json());
        assert_eq!(parse_options(&wrapped).len(), CROSSROADS_OPTION_COUNT);
    }

    #[test]
    fn drops_entries_with_blank_fields_and_keeps_the_rest() {
        let raw = r#"[
            {"emoji": "🔥", "title": "Torch It", "summary": "Burn the warehouse."},
            {"emoji": "", "title": "Ghost", "summary": "Blank emoji drops."},
            {"emoji": "❄️", "title": "", "summary": "Blank title drops."},
            {"emoji": "🌊", "title": "Flood", "summary": "  "}
        ]"#;
        let out = parse_options(raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Torch It");
    }

    #[test]
    fn caps_an_overlong_draw_to_the_deck_count() {
        let n = CROSSROADS_OPTION_COUNT + 3;
        let entries: Vec<String> = (1..=n)
            .map(|i| format!(r#"{{"emoji": "✨", "title": "T{i}", "summary": "S{i}."}}"#))
            .collect();
        let out = parse_options(&format!("[{}]", entries.join(",")));
        assert_eq!(out.len(), CROSSROADS_OPTION_COUNT);
    }

    #[test]
    fn garbage_yields_an_empty_deck() {
        assert!(parse_options("the narrator declines").is_empty());
        assert!(parse_options("[1, 2, 3]").is_empty());
    }

    #[test]
    fn prompt_surfaces_carry_no_em_dash() {
        let cats = [
            CrossroadsCategory::Player,
            CrossroadsCategory::World,
            CrossroadsCategory::Npc,
            CrossroadsCategory::Plot,
            CrossroadsCategory::Explicit,
        ];
        let surfaces: Vec<String> = cats
            .into_iter()
            .flat_map(|cat| {
                let opt = CrossroadsOption {
                    emoji: "🗝️".into(),
                    title: "The Locked Door".into(),
                    summary: "A door no key has opened.".into(),
                };
                vec![
                    options_system_prompt(cat),
                    options_task(),
                    expand_task(cat, &opt),
                ]
            })
            .collect();
        assert!(surfaces.iter().all(|s| !s.contains('—')));
    }

    #[test]
    fn every_category_round_trips_its_wire_key() {
        let cats = [
            CrossroadsCategory::Player,
            CrossroadsCategory::World,
            CrossroadsCategory::Npc,
            CrossroadsCategory::Plot,
            CrossroadsCategory::Explicit,
        ];
        for cat in cats {
            assert_eq!(CrossroadsCategory::from_wire(cat.wire()), Some(cat));
        }
        assert_eq!(CrossroadsCategory::from_wire("surprise"), None);
    }

    #[test]
    fn world_law_forbids_new_characters_and_npc_law_demands_them() {
        let world = options_system_prompt(CrossroadsCategory::World);
        assert!(world.contains("Introduce no new characters"));
        let npc = options_system_prompt(CrossroadsCategory::Npc);
        assert!(npc.contains("NEW character"));
    }
}
