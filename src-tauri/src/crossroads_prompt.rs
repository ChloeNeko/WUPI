//! System prompt + parser for the **Crossroads** option generator.
//!
//! Crossroads is the Fable "Choose a Director" tool: the player opens a popover
//! anchored above the narrator input, picks one of five directorial lenses, and
//! the model emits exactly 6 options through that lens. Each option is an
//! `[EMOJI] TITLE\nDESCRIPTION` block separated by `---`. The frontend renders
//! them as cards with Insert / Send / Copy actions.
//!
//! ## Output contract (load-bearing)
//!
//! Six option blocks separated by `---` on its own line. Each block opens with
//! `[EMOJI] TITLE` (single emoji + one-line title) and the rest of the block is
//! the 2–3 sentence description. This is Pathweaver's wire format — the cheapest
//! to parse defensively and the cheapest to stream-reveal (the frontend can fill
//! cards as `---` boundaries arrive).
//!
//! ```text
//! [⚡] Strike the ward-stone while the runes still smolder
//! The barkeeper's hand drifts toward the crossbow under the bar. A spark
//! leaping from the ward-stone would startle her long enough to dive for the
//! cellar door — if it works.
//!
//! ---
//!
//! [🤝] Buy a round for the dice game
//! ...
//! ```
//!
//! ## Memoryless by construction
//!
//! Nothing this prompt produces is ever archived. The category + seed are
//! supplied by the caller (the frontend holds them client-side); the generation
//! is a single one-shot call (`crossroads_generate`, lib.rs).

use std::fmt;

/// The five directorial lenses. Always all visible (no card-activity gating).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossroadsCategory {
    /// Things the player's character could do next.
    Action,
    /// Narrative curveballs (revelation / betrayal / reversal / arrival / discovery).
    Plot,
    /// NPCs who could plausibly enter the scene.
    Character,
    /// NSFW-flavored beats. Always available — no gating.
    Explicit,
    /// Director-level world nudges (weather / time skip / faction move / off-screen event).
    World,
}

impl CrossroadsCategory {
    /// Stable lowercase wire id — passed across the IPC boundary and used in
    /// logs. The frontend sends one of these strings; `from_id` parses it back.
    pub fn id(self) -> &'static str {
        match self {
            CrossroadsCategory::Action => "action",
            CrossroadsCategory::Plot => "plot",
            CrossroadsCategory::Character => "character",
            CrossroadsCategory::Explicit => "explicit",
            CrossroadsCategory::World => "world",
        }
    }

    /// One-line UI label for the popover (the frontend also has these but having
    /// the canonical label in Rust keeps the two in sync for tests + logs).
    pub fn label(self) -> &'static str {
        match self {
            CrossroadsCategory::Action => "Action",
            CrossroadsCategory::Plot => "Plot",
            CrossroadsCategory::Character => "Character",
            CrossroadsCategory::Explicit => "Explicit",
            CrossroadsCategory::World => "World",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "action" => CrossroadsCategory::Action,
            "plot" => CrossroadsCategory::Plot,
            "character" => CrossroadsCategory::Character,
            "explicit" => CrossroadsCategory::Explicit,
            "world" => CrossroadsCategory::World,
            _ => return None,
        })
    }
}

impl fmt::Display for CrossroadsCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Caller-supplied parameters for one generation. `player_seed` is the optional
/// free-text nudge from the FAB input (empty string if the player skipped it).
/// `count` is how many options to emit (default 6, range 1..=12 — the IPC
/// boundary and `GenerateOptions::validate_args` enforce the clamp; this struct
/// trusts the caller). The §11.24 hardcode of "6" became parameterizable in
/// the drawer-NL-trigger refactor so the player can name any count in chat.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrossroadsRequest {
    pub category: Option<CrossroadsCategory>,
    pub player_seed: String,
    #[serde(default = "default_count")]
    pub count: u8,
}

fn default_count() -> u8 {
    6
}

impl Default for CrossroadsRequest {
    fn default() -> Self {
        Self {
            category: None,
            player_seed: String::new(),
            count: default_count(),
        }
    }
}

/// One parsed option. `icon` is a single emoji (or `"✦"` fallback when the
/// model emits something unparseable). `title` is a one-line label; `description`
/// is the 2–3 sentence body the player reads to decide.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct CrossroadsOption {
    pub icon: String,
    pub title: String,
    pub description: String,
}

/// Build the generation system prompt for one lens. Sterile + structural voice
/// (NOT the catgirl persona — these are generation prompts, like the Quick Play
/// `interview_prompt` builder). Each lens gets its own persona / task /
/// anti-clichque block; the OUTPUT_FORMAT + GUIDELINES tail is shared.
pub fn build_crossroads_system_prompt(req: &CrossroadsRequest) -> String {
    let category = req.category.unwrap_or(CrossroadsCategory::Action);
    // The count is interpolated into four sites (output_format, guidelines,
    // final_reminder, the lens block). The static consts below all hardcode
    // the word "six"; we substitute the spelled-out count at the call site so
    // the prose stays grammatical ("Emit EXACTLY six options" / "...twelve
    // options"). One-shot generation, called once per click — the heap allocs
    // here are noise.
    let count_word = count_word_for(req.count);
    let lens_raw = match category {
        CrossroadsCategory::Action => LENS_ACTION,
        CrossroadsCategory::Plot => LENS_PLOT,
        CrossroadsCategory::Character => LENS_CHARACTER,
        CrossroadsCategory::Explicit => LENS_EXPLICIT,
        CrossroadsCategory::World => LENS_WORLD,
    };
    let lens = lens_raw.replace("six", &count_word);
    let output_format = OUTPUT_FORMAT.replace("six", &count_word);
    let guidelines = GUIDELINES.replace("six", &count_word);
    let final_reminder = FINAL_REMINDER.replace("six", &count_word);

    let mut out = String::with_capacity(3000);

    out.push_str("<crossroads_role>\n");
    out.push_str(CROSSROADS_ROLE);
    out.push_str("\n</crossroads_role>\n\n");

    out.push_str("<lens>\n");
    out.push_str(&lens);
    out.push_str("\n</lens>\n\n");

    out.push_str("<output_format>\n");
    out.push_str(&output_format);
    out.push_str("\n</output_format>\n\n");

    out.push_str("<guidelines>\n");
    out.push_str(&guidelines);
    out.push_str("\n</guidelines>\n\n");

    // Player seed (optional free-text nudge). Empty → invent context from the
    // scene; non-empty → bias the options toward what the player asked for,
    // but never collapse to N variations on one idea.
    out.push_str("<player_seed>\n");
    if req.player_seed.trim().is_empty() {
        out.push_str("(no specific seed — invent options grounded in the live scene.)\n");
    } else {
        out.push_str(req.player_seed.trim());
        out.push('\n');
    }
    out.push_str("</player_seed>\n\n");

    out.push_str("<final_reminder>\n");
    out.push_str(&final_reminder);
    out.push_str("\n</final_reminder>\n");

    out
}

/// Spell out a count for prompt prose. Falls back to the decimal form for
/// values outside the small expected range (1..=12) so we never emit something
/// ungrammatical like "Emit EXACTLY 13 options" mid-sentence. Public so the
/// `crossroads_generate` IPC can build the matching user-message line.
pub fn count_word_for(n: u8) -> String {
    match n {
        1 => "one".into(),
        2 => "two".into(),
        3 => "three".into(),
        4 => "four".into(),
        5 => "five".into(),
        6 => "six".into(),
        7 => "seven".into(),
        8 => "eight".into(),
        9 => "nine".into(),
        10 => "ten".into(),
        11 => "eleven".into(),
        12 => "twelve".into(),
        _ => n.to_string(),
    }
}

// ── Prompt text blocks ───────────────────────────────────────────────────

const CROSSROADS_ROLE: &str = "\
You are the SIMULATION OPTION ENGINE for an in-progress roleplay. You emit \
concrete, grounded, player-pickable options for what could happen next. You are \
not a character, not a player, not the narrator. You do not narrate. You produce \
ONLY a numbered set of option blocks — nothing else. No preamble, no closing \
remark, no markdown fence, no apology.";

const OUTPUT_FORMAT: &str = "\
Emit EXACTLY six option blocks, separated by a single line containing only three \
dashes (---). Each block has exactly this shape:\n\
\n\
[EMOJI] Title\n\
2–3 sentence description. Concrete and grounded in the live scene.\n\
\n\
Rules:\n\
- The opening line begins with a single emoji in square brackets, then a space, \
  then a one-line title (no trailing period).\n\
- The body is 2–3 sentences. Specific, sensory, and tied to what's actually in \
  the scene right now. Never a generic template.\n\
- Separate blocks with a line containing only ---. Nothing before the first \
  block or after the last.\n\
- Do NOT number the blocks. Do NOT wrap the whole response in a code fence. Do \
  NOT emit any other tags, headers, or commentary.";

const GUIDELINES: &str = "\
GROUNDING: every option must be plausible given the live scene provided in the \
user message. An option that contradicts established canon, ignores the present \
NPCs, or invents items the player doesn't have is a failure.\n\
\n\
DIVERSITY: the six options must explore genuinely different directions. Do not \
emit six variations on one idea. If you find three options all leading to a \
fight, replace two of them with something else.\n\
\n\
SPECIFICITY: name the actual object, NPC, or location. \"Ask the innkeep about \
the merchant's cargo\" beats \"Talk to someone.\" Concrete detail signals that \
the option was authored for THIS scene, not a template.\n\
\n\
CONSEQUENCE: each option should imply a real cost, risk, or tradeoff. Options \
that are free wins or pure flavor dilute the choice. The player should feel the \
weight of picking one over another.\n\
\n\
PROSE VOICE: second person (\"You…\"), present tense, immersive. Match the tone \
of the surrounding scene. Never address the player out of character.";

const FINAL_REMINDER: &str = "\
Emit six blocks now. No preamble, no commentary, no closing line. The first \
character of your response is the [ of the first option's emoji. The last \
character is the final period of the sixth option's description.";

// ── Per-lens blocks ──────────────────────────────────────────────────────

const LENS_ACTION: &str = "\
LENS: ACTION — options for what the PLAYER'S CHARACTER could do next.\n\
\n\
TASK: Generate six distinct, concrete actions the player could take right \
now, grounded in the live scene. Each must be something the player would type as \
their next move.\n\
\n\
APPROACH: read the scene. What is the player's immediate situation? What \
are the salient objects, exits, NPCs, and tensions in play? What WOULDN'T they \
default to? Only then author six options.\n\
\n\
ANTI-TEMPLATE: avoid generic \"attack / flee / talk\" unless they genuinely fit \
this moment. If the scene has a ward-stone, a cellar door, a half-finished dice \
game, an unattended crossbow — those specifics are the well. Draw from them.\n\
\n\
The player's body is what it is: a Heavy Injury to the right arm rules out \
swinging a sword effectively; Exhausted means running is desperate, not casual. \
Honor the limits the scene reports.";

const LENS_PLOT: &str = "\
LENS: PLOT — narrative curveballs the world throws at the player.\n\
\n\
TASK: Generate six distinct plot twists that would meaningfully complicate, \
redirect, or escalate the current situation. These are WORLD events that happen \
TO or AROUND the player, not actions the player takes.\n\
\n\
TWIST TYPES (use as a taxonomy, mix across them):\n\
- The Revelation: a hidden truth about a character or the world surfaces.\n\
- The Betrayal: an ally's true allegiance or motive becomes visible (must fit \
  their established motivation — never invent a betrayal from nothing).\n\
- The Reversal: a power dynamic flips (predator becomes prey, hunter hunted).\n\
- The Arrival: an unexpected faction, character, or force enters the fray.\n\
- The Discovery: an item, location, or piece of information changes the stakes.\n\
\n\
ANTI-TEMPLATE: avoid deus ex machina. Every twist must be seeded by something \
already in the scene or in the established world — a hinted motive pays off, a \
mentioned threat arrives, a prior choice has a delayed consequence. The player \
should be able to look back and see how this was coming.\n\
\n\
Each option describes the twist itself, not how the player reacts — the \
player's response is theirs to choose.";

const LENS_CHARACTER: &str = "\
LENS: CHARACTER — NPCs who could plausibly enter or step forward in this scene.\n\
\n\
TASK: Generate six distinct characters whose arrival the world would find \
plausible right now. For each, give the character a name, a one-line reason they \
are here (their errand), and the tension or opportunity their presence creates.\n\
\n\
APPROACH: derive each character from the setting and the present situation. A \
frontier tavern at midnight invites a road-worn messenger, a debt collector, a \
rival from someone's past, a lost traveler. A besieged city invites a deserter, \
a smuggler, a herald with terms. The world already contains the seeds of who \
might appear — water them.\n\
\n\
ANTI-TEMPLATE: do NOT default to \"mysterious stranger in a dark cloak,\" \"wise \
old mentor,\" \"quirky comic relief sidekick,\" or any other stock archetype. If \
your first idea is one of these, replace it. Every character should feel like \
they were already part of this world, waiting offstage.\n\
\n\
Each option is the character's entrance — name, what they want, why now. The \
player chooses how to react.";

const LENS_EXPLICIT: &str = "\
LENS: EXPLICIT — intimate, sensual, or sexual beats.\n\
\n\
TASK: Generate six distinct explicit options grounded in the player's \
established relationships and the live scene's tone. These are about what could \
happen, proposed with sensory specificity — the player picks the one that fits.\n\
\n\
APPROACH: read the scene for who is present, the established dynamics between \
them, and the room's mood. Explicit content lands hardest when it emerges from \
real relationship texture — existing tension, a prior glance, a slow build. \
Author options that respect the characters as people, not props.\n\
\n\
ANTI-TEMPLATE: avoid out-of-character escalation. An option that ignores the \
established dynamic (strangers suddenly intimate, allies suddenly cold) is a \
failure. Honor the present NPCs' established dispositions; if the scene hasn't \
earned an escalation, the option is a flirtation or a tension, not an act.\n\
\n\
Each option is the moment itself — sensory, specific, in the player's \
second-person POV. No clinical or instructional tone. Match the surrounding \
prose voice.";

const LENS_WORLD: &str = "\
LENS: WORLD — director-level changes to the world itself, not the player's \
actions.\n\
\n\
TASK: Generate six distinct world-shifts that would meaningfully change the \
scene's conditions. The player is NOT the actor here — the world moves on \
its own.\n\
\n\
KINDS (mix across them):\n\
- Weather + atmosphere: the storm breaks, fog rolls in, the lanterns gutter out.\n\
- Time passage: a meaningful skip (dawn arrives, hours pass, the night deepens).\n\
- Faction + off-screen moves: the guard rotates, the caravan departs, a rival \
  faction makes a move the player hears about secondhand.\n\
- Environmental change: the fire dies, the river rises, the door the player \
came in by is now blocked.\n\
- World-becomes-alive detail: an NPC who was background noise does something \
  with their own agenda that crosses the player's orbit.\n\
\n\
ANTI-TEMPLATE: every shift must NOT contradict established canon. The world \
moves coherently from where it is — weather doesn't reverse without cause, \
factions don't teleport. Lean on the LIVING WORLD discipline: NPCs and forces \
have their own errands; a good world-shift shows one of those errands colliding \
with the player's scene.\n\
\n\
Each option is the shift itself — what changes, why, and what it costs the \
player to deal with. The player's response is theirs to choose.";

// ── Parser ───────────────────────────────────────────────────────────────

/// Defensive multi-strategy parser. Handles the three shapes the model tends to
/// produce despite the OUTPUT_FORMAT spec:
///   1. Spec-compliant: blocks separated by `---` on its own line.
///   2. Double-newline separated (model forgot the `---`).
///   3. Emoji-boundary: `[X] Title` lines mark new blocks (no separator at all).
///
/// `count` caps how many options are returned (the prompt emits exactly `count`
/// blocks; we truncate defensively in case the model rambles). Caller passes
/// `req.count as usize`.
///
/// Malformed input (no parseable option anywhere) returns an empty Vec — never
/// panics. The caller decides whether to surface an error or retry.
pub fn parse_options(raw: &str, count: usize) -> Vec<CrossroadsOption> {
    let cleaned = strip_markdown_fence(raw).trim();
    if cleaned.is_empty() {
        return Vec::new();
    }

    // Strategy 1: split on `---` separators.
    let mut blocks: Vec<String> = cleaned
        .split('\n')
        .map(|line| line.trim())
        .fold(Vec::new(), |mut acc, line| {
            if line == "---" || line == "—" || line == "***" {
                acc.push(String::new()); // sentinel: new block
            } else if line.starts_with("---") && line.chars().skip(3).all(|c| c == '-') {
                acc.push(String::new());
            } else if let Some(last) = acc.last_mut() {
                if !last.is_empty() {
                    last.push('\n');
                }
                last.push_str(line);
            } else {
                acc.push(line.to_string());
            }
            acc
        })
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Strategy 2 fallback: if `---` produced only one block but the text has
    // multiple `[X] Title` openers, split on the emoji-bracket boundary instead.
    if blocks.len() < 2 {
        let emoji_split = split_on_emoji_brackets(cleaned);
        if emoji_split.len() > blocks.len() {
            blocks = emoji_split;
        }
    }

    // Strategy 3 fallback: if still one block but double-newline gaps exist,
    // split on those.
    if blocks.len() < 2 {
        let paragraphs: Vec<String> = cleaned
            .split("\n\n")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if paragraphs.len() > blocks.len() {
            blocks = paragraphs;
        }
    }

    let cap = count.max(1);
    blocks.iter().take(cap).map(|b| parse_one_block(b)).collect()
}

/// Strip a wrapping ``` markdown fence if the model added one despite the spec.
fn strip_markdown_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip the optional language tag on the opening fence line.
        let after_lang = rest.trim_start_matches(|c: char| c.is_alphanumeric());
        let body = after_lang.trim_start_matches('\n');
        if let Some(body) = body.strip_suffix("```") {
            return body.trim();
        }
        return body.trim();
    }
    trimmed
}

/// Strategy 2: split on `[EMOJI] Title` openers. Returns the block bodies, each
/// INCLUDING its opener (so `parse_one_block` can pull the emoji + title off).
fn split_on_emoji_brackets(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if is_emoji_bracket_opener(trimmed) {
            out.push(trimmed.to_string());
        } else if let Some(last) = out.last_mut() {
            if !trimmed.is_empty() {
                last.push('\n');
                last.push_str(trimmed);
            }
        }
    }
    out
}

/// Detect `[X] Title` openers. Permissive: any line starting with `[` followed
/// by 1–7 emoji codepoints (up to ~32 bytes to cover ZWJ families, skin tones,
/// and variation selectors like 🗝️) then `]` then a space + non-empty title.
fn is_emoji_bracket_opener(line: &str) -> bool {
    let line = line.trim_start();
    if !line.starts_with('[') {
        return false;
    }
    let inner_end = match line.find(']') {
        // [X] = 1–32 bytes inside the brackets (1 byte for a single ASCII
        // letter up to ~32 for a ZWJ family emoji + variation selector).
        Some(i) if i > 1 && i <= 32 => i,
        _ => return false,
    };
    let after = &line[inner_end + 1..];
    after.starts_with(' ') && after.trim().len() >= 2
}

/// Parse one block into `CrossroadsOption`. Tolerates missing emoji, missing
/// title, or no description — falls back to `✦` + the first line as title.
fn parse_one_block(block: &str) -> CrossroadsOption {
    let block = block.trim();
    let mut lines = block.lines();

    let raw_first = lines.next().unwrap_or("").trim();
    // Strip a leading number/bullet the model sometimes adds DESPITE the spec
    // forbidding it. Done BEFORE the bracket-opener check so "1. [⚡] Title"
    // parses correctly into the bracket form.
    let first = strip_leading_number(raw_first);
    let (icon, title) = if is_emoji_bracket_opener(&first) {
        let close = first.find(']').unwrap_or(0);
        let inner = &first[1..close];
        let icon = if inner.is_empty() { "✦".to_string() } else { inner.to_string() };
        let title = first[close + 1..].trim().to_string();
        (icon, title)
    } else {
        // No bracket opener — treat the whole first line as the title.
        ("✦".to_string(), first.to_string())
    };

    let title = strip_markdown_asterisks(&title);
    let title = truncate_chars(&title, 100);

    let description: String = lines
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    CrossroadsOption {
        icon,
        title,
        description,
    }
}

/// Strip a leading "1." / "1)" / "-" numbering the model sometimes adds despite
/// the spec forbidding it.
fn strip_leading_number(s: &str) -> String {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    // "12. " or "12) "
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b')') {
        let rest = trimmed[i + 1..].trim_start();
        return rest.to_string();
    }
    // "- " or "* " bullet
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        return trimmed[2..].to_string();
    }
    trimmed.to_string()
}

/// Strip **bold** / *italic* markdown the model sometimes wraps titles in.
fn strip_markdown_asterisks(s: &str) -> String {
    s.trim_matches('*').trim().to_string()
}

/// Truncate to at most `max_chars` unicode scalar values, appending "…" if cut.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let mut out: String = chars.into_iter().take(max_chars).collect();
    out.push('…');
    out
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn req(category: CrossroadsCategory, seed: &str) -> CrossroadsRequest {
        CrossroadsRequest {
            category: Some(category),
            player_seed: seed.to_string(),
            count: 6,
        }
    }

    fn req_count(category: CrossroadsCategory, seed: &str, count: u8) -> CrossroadsRequest {
        CrossroadsRequest {
            category: Some(category),
            player_seed: seed.to_string(),
            count,
        }
    }

    // --- Prompt builder ---

    #[test]
    fn prompt_carries_role_and_format_blocks() {
        let p = build_crossroads_system_prompt(&req(CrossroadsCategory::Action, ""));
        for tag in ["<crossroads_role>", "<output_format>", "<guidelines>", "<player_seed>", "<final_reminder>"] {
            assert!(p.contains(tag), "prompt missing {tag}");
        }
    }

    #[test]
    fn prompt_for_each_lens_carries_its_lens_block() {
        for (cat, marker) in [
            (CrossroadsCategory::Action, "LENS: ACTION"),
            (CrossroadsCategory::Plot, "LENS: PLOT"),
            (CrossroadsCategory::Character, "LENS: CHARACTER"),
            (CrossroadsCategory::Explicit, "LENS: EXPLICIT"),
            (CrossroadsCategory::World, "LENS: WORLD"),
        ] {
            let p = build_crossroads_system_prompt(&req(cat, ""));
            assert!(p.contains(marker), "lens {cat:?} missing its LENS block");
            assert!(p.contains("<lens>"), "lens {cat:?} missing <lens> wrapper");
        }
    }

    #[test]
    fn prompt_forbids_preamble_and_fence() {
        let p = build_crossroads_system_prompt(&req(CrossroadsCategory::Action, ""));
        assert!(p.contains("No preamble"));
        assert!(p.contains("no markdown fence"));
    }

    #[test]
    fn prompt_carries_player_seed_when_present() {
        let p = build_crossroads_system_prompt(&req(CrossroadsCategory::Action, "make the barkeeper sweat"));
        assert!(p.contains("make the barkeeper sweat"));
    }

    #[test]
    fn prompt_marks_blank_seed() {
        let p = build_crossroads_system_prompt(&req(CrossroadsCategory::Action, "   "));
        assert!(p.contains("no specific seed"));
    }

    #[test]
    fn prompt_each_lens_carries_its_anti_template() {
        // Each lens must contain its ANTI-TEMPLATE anchor (the load-bearing
        // anti-clichque instruction). The "six" → count substitution must
        // NOT have clobbered these anchors (regression guard).
        let cases = [
            (CrossroadsCategory::Action, "ANTI-TEMPLATE: avoid generic"),
            (CrossroadsCategory::Plot, "avoid deus ex machina"),
            (CrossroadsCategory::Character, "mysterious stranger"),
            (CrossroadsCategory::Explicit, "out-of-character escalation"),
            (CrossroadsCategory::World, "contradict established canon"),
        ];
        for (cat, anchor) in cases {
            let p = build_crossroads_system_prompt(&req(cat, ""));
            assert!(p.contains(anchor), "lens {cat:?} missing anti-template anchor {anchor:?}");
        }
    }

    #[test]
    fn prompt_demands_exactly_six_at_default_count() {
        let p = build_crossroads_system_prompt(&req(CrossroadsCategory::Action, ""));
        assert!(p.contains("EXACTLY six"));
    }

    #[test]
    fn prompt_substitutes_count_word() {
        // count=1, 3, 12 must each produce the spelled-out word in the
        // output_format + final_reminder blocks (not the digit, not "six").
        for (n, word) in [(1u8, "one"), (3u8, "three"), (12u8, "twelve")] {
            let p = build_crossroads_system_prompt(&req_count(CrossroadsCategory::Action, "", n));
            assert!(
                p.contains(&format!("EXACTLY {word} option")),
                "count={n}: expected 'EXACTLY {word} option' in prompt"
            );
            assert!(
                p.contains(&format!("Emit {word} blocks now")),
                "count={n}: expected 'Emit {word} blocks now' in final_reminder"
            );
        }
    }

    #[test]
    fn prompt_count_word_falls_back_to_digit_outside_range() {
        // Defensive: if a caller somehow bypasses the clamp, the prompt should
        // still be grammatical (use the digit) rather than silently keep "six".
        let p = build_crossroads_system_prompt(&req_count(CrossroadsCategory::Action, "", 99));
        assert!(p.contains("EXACTLY 99 option"));
        assert!(!p.contains("EXACTLY six"));
    }

    // --- Category wire ids ---

    #[test]
    fn category_ids_roundtrip() {
        for cat in [
            CrossroadsCategory::Action,
            CrossroadsCategory::Plot,
            CrossroadsCategory::Character,
            CrossroadsCategory::Explicit,
            CrossroadsCategory::World,
        ] {
            assert_eq!(CrossroadsCategory::from_id(cat.id()), Some(cat));
        }
    }

    #[test]
    fn category_from_id_rejects_unknown() {
        assert!(CrossroadsCategory::from_id("nonsense").is_none());
        assert!(CrossroadsCategory::from_id("").is_none());
    }

    #[test]
    fn category_from_id_is_case_insensitive() {
        assert_eq!(
            CrossroadsCategory::from_id("ACTION"),
            Some(CrossroadsCategory::Action)
        );
    }

    // --- Parser: spec-compliant ---

    #[test]
    fn parse_spec_compliant_six_blocks() {
        let raw = "[⚡] Strike the ward-stone\nA spark leaps. The barkeep flinches.\n\n---\n\n[🤝] Buy a round\nThe dice game warms to you.\n\n---\n\n[🏃] Bolt for the cellar\nDarkness. Maybe a back door.\n\n---\n\n[🗡️] Draw steel\nLoud. Final. No retreat after.\n\n---\n\n[🗝️] Palm the key off the hook\nShe's not looking. Now.\n\n---\n\n[📜] Read the ward-runes aloud\nWords have weight. So does she.";
        let opts = parse_options(raw, 6);
        assert_eq!(opts.len(), 6);
        assert_eq!(opts[0].icon, "⚡");
        assert_eq!(opts[0].title, "Strike the ward-stone");
        assert!(opts[0].description.starts_with("A spark leaps."));
        assert_eq!(opts[5].icon, "📜");
    }

    #[test]
    fn parse_caps_at_requested_count_when_smaller_than_available() {
        // 10 blocks available, count=4 → only 4 returned.
        let mut raw = String::new();
        for i in 1..=10 {
            if i > 1 {
                raw.push_str("\n\n---\n\n");
            }
            raw.push_str(&format!("[✦] Option {}\nBody {}.", i, i));
        }
        let opts = parse_options(&raw, 4);
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].title, "Option 1");
        assert_eq!(opts[3].title, "Option 4");
    }

    #[test]
    fn parse_returns_all_available_when_count_exceeds_blocks() {
        // 3 blocks available, count=12 → all 3 returned (no padding).
        let mut raw = String::new();
        for i in 1..=3 {
            if i > 1 {
                raw.push_str("\n\n---\n\n");
            }
            raw.push_str(&format!("[✦] Option {}\nBody {}.", i, i));
        }
        let opts = parse_options(&raw, 12);
        assert_eq!(opts.len(), 3);
    }

    #[test]
    fn parse_count_one_returns_first_block_only() {
        let mut raw = String::new();
        for i in 1..=5 {
            if i > 1 {
                raw.push_str("\n\n---\n\n");
            }
            raw.push_str(&format!("[✦] Option {}\nBody {}.", i, i));
        }
        let opts = parse_options(&raw, 1);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].title, "Option 1");
    }

    #[test]
    fn parse_count_zero_is_treated_as_one_defensively() {
        // The cap floor is count.max(1) — zero should never return empty when
        // there's parseable content.
        let raw = "[⚡] Only\nBody.";
        let opts = parse_options(raw, 0);
        assert_eq!(opts.len(), 1);
    }

    // --- Parser: fallbacks ---

    #[test]
    fn parse_emoji_boundary_fallback_when_no_separators() {
        // Model forgot the `---` separators — all blocks share one text blob,
        // but each opens with `[X] Title`.
        let raw = "[⚡] First\nFirst body.\n[🤝] Second\nSecond body.\n[🗝️] Third\nThird body.";
        let opts = parse_options(raw, 6);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].title, "First");
        assert_eq!(opts[1].title, "Second");
        assert_eq!(opts[2].title, "Third");
    }

    #[test]
    fn parse_strips_markdown_fence() {
        let raw = "```\n[⚡] Title\nBody.\n\n---\n\n[✦] Two\nBody two.\n```";
        let opts = parse_options(raw, 6);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].title, "Title");
    }

    #[test]
    fn parse_strips_leading_numbers_from_titles() {
        let raw = "1. [⚡] Strike\nBody.\n\n---\n\n2) [🤝] Buy\nBody two.\n\n---\n\n* [🗝️] Palm\nBody three.";
        let opts = parse_options(raw, 6);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].title, "Strike");
        assert_eq!(opts[1].title, "Buy");
        assert_eq!(opts[2].title, "Palm");
    }

    #[test]
    fn parse_strips_markdown_asterisks_from_titles() {
        let raw = "[⚡] **Strike**\nBody.\n\n---\n\n[🤝] *Buy*\nBody two.";
        let opts = parse_options(raw, 6);
        assert_eq!(opts[0].title, "Strike");
        assert_eq!(opts[1].title, "Buy");
    }

    #[test]
    fn parse_truncates_long_titles() {
        let long_title: String = "word ".repeat(40);
        let raw = format!("[⚡] {}\nBody.", long_title.trim());
        let opts = parse_options(&raw, 6);
        assert!(opts[0].title.chars().count() <= 101); // 100 + ellipsis
        assert!(opts[0].title.ends_with('…'));
    }

    #[test]
    fn parse_block_without_bracket_opener_uses_first_line_as_title() {
        let raw = "Just a title line\nBody description.";
        let opts = parse_options(raw, 6);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].title, "Just a title line");
        assert_eq!(opts[0].icon, "✦");
    }

    #[test]
    fn parse_empty_input_returns_empty_vec() {
        assert!(parse_options("", 6).is_empty());
        assert!(parse_options("   \n  \n", 6).is_empty());
    }

    #[test]
    fn parse_malformed_returns_empty_or_partial_never_panics() {
        // Just noise — should not panic, may return empty or whatever fragments
        // the parser can salvage.
        let _ = parse_options("---\n---\n---", 6);
        let _ = parse_options("[][][]", 6);
        let _ = parse_options("no brackets no separators just prose", 6);
    }
}
