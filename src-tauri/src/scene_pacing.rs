//! Scene Pacing Engine (Fable Seam #4 expansion, 2026-07-27): a pure-Rust,
//! keyword-driven classifier that scores the player's turn text on three
//! pillars and derives a [`SceneMode`] from them. The mode then drives:
//!
//! 1. The narrator prose cadence via a `<scene_pacing mode="...">` tag in
//!    `build_narrator_system_prompt` (lib.rs).
//! 2. The World Progression tick interval (`SceneMode::progression_interval_hours`).
//! 3. The skill-check DC modifier (`SceneMode::dc_modifier`).
//!
//! # The hybrid 3-pillar model
//!
//! Each pillar is scored 0..=2 (low/med/high) from keyword scans of the
//! turn text. The pillars are intentionally orthogonal: a combat scene in a
//! tavern scores Spatial=0 (enclosed) + Emotional=2 (alarmed) + Kinetic=2
//! (violent); a tense negotiation in the same tavern scores Spatial=0 +
//! Emotional=1 + Kinetic=0. The mode mapping uses Kinetic as the dominant
//! signal (combat overrides everything) and falls back to Emotional for the
//! Downtime-vs-Exploration split.
//!
//! # Why keyword-driven, not LLM-driven
//!
//! Pacing is a control signal, not a creative decision. Rust-classifying it
//! keeps the mode deterministic (the same turn text always classifies the
//! same way), testable, and zero-LLM-cost. The narrator still has full
//! creative control over the PROSE — it just gets a pacing hint to obey.
//!
//! # Why not a separate schema-delta pass
//!
//! The mode is per-TURN, not per-world-state. It re-classifies every turn
//! from the player's text. Persisting it on `WorldSchema` is only so the
//! narrator's NEXT turn inherits the prior mode if the player does something
//! neutral (e.g. "I think" or "I look around" — Exploration by default, but
//! if the prior turn was Combat, a single neutral line shouldn't snap the
//! rhythm back to Exploration instantly). The persistence is a soft memory;
//! the next non-neutral turn re-classifies.

use crate::schema::{SceneMode, ScenePacing};

// ---------------------------------------------------------------------------
// Pillar keyword tables. Matched case-insensitively through
// `player_state::keyword_present` (leading word boundary — the SAME matcher
// convention as `player_state::COMBAT_KEYWORDS`, #35). Conservative:
// false-negative cost is a "neutral" classification (default Exploration);
// false-positive cost is a mis-paced scene. Both are recoverable on the next
// turn, so the bar is "good default + obvious matches."
// ---------------------------------------------------------------------------

/// Spatial scale: how enclosed is the immediate environment?
/// Declared for documentation + future tuning (a v2 mapping could read it
/// to distinguish "enclosed combat" from "wilderness combat"). Today the
/// scoring treats enclosed as the implicit-0 tier (the default when neither
/// civil nor wilderness matched), so this list is never scanned — kept so
/// the keyword vocabulary is explicit + grep-able.
#[allow(dead_code)]
const SPATIAL_ENCLOSED: &[&str] = &[
    "room", "tavern", "inn", "house", "hall", "chamber", "cellar", "attic",
    "cabin", "hut", "tent", "cave", "cavern", "tunnel", "corridor", "hallway",
    "shop", "store", "market stall", "booth",
];

const SPATIAL_OPEN_CIVIL: &[&str] = &[
    "street", "market", "square", "plaza", "alley", "road", "path", "bridge",
    "harbor", "dock", "wharf", "gate", "wall", "tower", "courtyard", "yard",
    "village", "town", "city",
];

const SPATIAL_WILDERNESS: &[&str] = &[
    "forest", "woods", "jungle", "desert", "mountain", "hill", "valley",
    "ocean", "sea", "river", "lake", "stream", "field", "meadow", "plain",
    "swamp", "marsh", "wastes", "wilderlands", "trail", "wilderness",
    "dungeon", "ruin", "wilds",
];

/// Emotional vector: the affective register of the scene.
const EMOTIONAL_CALM: &[&str] = &[
    "rest", "sleep", "relax", "wait", "sit", "drink", "eat", "dine", "chat",
    "talk", "listen", "watch", "trade", "barter", "buy", "sell", "shop",
    "browse", "hum", "sing softly", "think",
];

const EMOTIONAL_TENSE: &[&str] = &[
    "argue", "argu", "disagree", "negotiate", "bargain hard", "suspect",
    "suspicious", "wary", "distrust", "tense", "uneasy", "guard", "watch",
    "study", "investigate", "search", "examine", "interrogate", "question",
    "plead", "beg", "plea",
];

const EMOTIONAL_ALARMED: &[&str] = &[
    "panic", "flee", "run away", "ambush", "trap", "danger", "terrified",
    "horror", "scream", "shriek", "stampede", "alarm", "trap is sprung",
    "betrayal", "betrayed",
];

/// Kinetic scale: the action intensity. The Combat tier reuses the EXACT
/// `player_state::COMBAT_KEYWORDS` list so scene-pacing and the combat
/// Referee agree on what counts as "combat" (single source of truth: if the
/// Referee fires, the mode is Combat).
///
/// We re-declare here (rather than `use`) because the combat list is private
/// to `player_state`. The lists MUST stay in sync — the
/// `combat_keywords_match_scene_pacing_combat` test in `player_state.rs`
/// pins this (it asserts every combat keyword triggers Combat classification).
/// If you add a combat keyword to `player_state::COMBAT_KEYWORDS`, add it
/// here too AND verify the test passes.
const KINETIC_COMBAT: &[&str] = &[
    "attack", "swing", "strike", "slash", "stab", "punch", "kick", "block",
    "dodge", "parry", "shoot", "fire", "cast", "throw", "tackle", "grapple",
    "charge", "run", "sprint", "climb", "jump", "leap", "swim",
];

const KINETIC_MOBILE: &[&str] = &[
    "walk", "go to", "head to", "travel", "wander", "stroll", "march",
    "ride", "sail", "fly", "teleport", "fast-travel", "fast travel",
    "journey", "depart", "leave", "enter", "arrive",
];

// ---------------------------------------------------------------------------
// Pillar scorers: each returns 0..=2 (low/med/high). Pure fns.
// ---------------------------------------------------------------------------

/// Count how many keywords from each tier appear in `lower` (lowercased text).
/// Returns the highest tier that matched (0 = none / low, 1 = civil / tense,
/// 2 = wilderness / alarmed). Tie-breaks toward the higher tier — a turn that
/// mentions both "street" and "ocean" classifies as wilderness (Spatial=2),
/// not civil. This is the load-bearing call: pillar scoring is "did this
/// tier's keyword appear at all, and at what highest tier."
///
/// (#35 2026-08-15) Matches through `player_state::keyword_present` — the
/// SAME leading word-boundary matcher the combat referee uses (a hit must
/// start at a word edge; "campfire"/"fireplace" ≠ "fire"). The old raw
/// `contains` made "I sit by the campfire and rest" classify as Combat
/// (kinetic=2) while the referee correctly didn't fire.
fn score_pillar(lower: &str, tier1: &[&str], tier2: &[&str]) -> u8 {
    let has_t2 = tier2.iter().any(|kw| crate::player_state::keyword_present(lower, kw));
    let has_t1 = tier1.iter().any(|kw| crate::player_state::keyword_present(lower, kw));
    if has_t2 {
        2
    } else if has_t1 {
        1
    } else {
        0
    }
}

/// Spatial pillar. 0 = enclosed, 1 = open-but-civilized, 2 = wilderness.
/// Tier1 = civil (street/market), Tier2 = wilderness (forest/ocean).
fn score_spatial(lower: &str) -> u8 {
    score_pillar(lower, SPATIAL_OPEN_CIVIL, SPATIAL_WILDERNESS)
    // Note: SPATIAL_ENCLOSED is the implicit "0" tier — if neither civil nor
    // wilderness matched AND an enclosed keyword matched, the score is still
    // 0 (which is correct: enclosed is the low tier). If NOTHING matched,
    // the score is also 0 (treated as enclosed/neutral). We don't need to
    // check ENCLOSED explicitly because 0 is the default.
}

/// Emotional pillar. 0 = calm, 1 = tense, 2 = alarmed.
fn score_emotional(lower: &str) -> u8 {
    score_pillar(lower, EMOTIONAL_TENSE, EMOTIONAL_ALARMED)
    // Same pattern as spatial: calm is the implicit-0 tier.
}

/// Kinetic pillar. 0 = static, 1 = mobile, 2 = violent.
/// Combat (Tier2) reuses the combat Referee's keyword list.
fn score_kinetic(lower: &str) -> u8 {
    score_pillar(lower, KINETIC_MOBILE, KINETIC_COMBAT)
}

/// Calm-signal flag: did any calm/rest/chat/trade keyword appear? Used by
/// `classify` to gate Downtime — Downtime is an ACTIVE choice (resting/
/// chatting), not the absence of other signals. Without this gate, neutral
/// or unrecognized text ("I quux the fnord") would mis-classify as Downtime
/// (kinetic=0, emotional=0) when it should be Exploration (the safe default).
fn calm_signal(lower: &str) -> bool {
    EMOTIONAL_CALM
        .iter()
        .any(|kw| crate::player_state::keyword_present(lower, kw))
}

/// Map the three pillar scores (+ the calm-signal flag) to a [`SceneMode`].
/// Pure fn, deterministic.
///
/// The mapping (locked 2026-07-27):
/// - **Kinetic == 2 → Combat** (override). The combat Referee will fire on
///   the same keyword, so the narrator MUST be paced for combat. This is the
///   dominant signal — even a "calm" scene that turns violent is combat.
/// - **Else Kinetic == 0 AND Emotional == 0 AND `calm_signal` → Downtime.**
///   Downtime is an ACTIVE choice (resting/chatting/trading), not the absence
///   of other signals. Requiring a calm-keyword match prevents neutral/
///   unrecognized text ("I quux the fnord") from mis-classifying as Downtime.
///   The narrator should slow down; the world should sim faster (off-screen
///   NPCs move while you recover).
/// - **Else → Exploration.** The balanced default: walking around, talking
///   with mild tension, investigating, OR neutral/unrecognized text. Covers
///   the long tail.
///
/// Spatial is NOT used in the v1 mapping — it's scored + persisted for
/// future tuning (e.g. wilderness exploration could pace differently from
/// urban exploration in a v2). The pillar is recorded so a future revision
/// can read it without re-scoring.
fn classify(kinetic: u8, emotional: u8, _spatial: u8, calm_signal: bool) -> SceneMode {
    if kinetic >= 2 {
        SceneMode::Combat
    } else if kinetic == 0 && emotional == 0 && calm_signal {
        SceneMode::Downtime
    } else {
        SceneMode::Exploration
    }
}

/// The entry point. Evaluate the player's turn text and return a
/// [`ScenePacing`] (mode + the three pillar scores). Pure fn — no I/O, no
/// locks, no side effects. Called from `fable_send` each turn, inside the
/// schema lock, BEFORE the prompt render. The caller persists the result
/// on `WorldSchema::scene_pacing` and threads the mode into the narrator
/// prompt + the skill-check Referee + the World Progression tick gate.
///
/// Empty / whitespace text → default `Exploration` (the neutral mode; the
/// narrator doesn't get a pacing tag override and the prior turn's mode
/// survives via the persisted `scene_pacing` field). A truly empty turn
/// (whitespace only, or text too short to be an action) should NOT classify
/// as Downtime — Downtime is "resting/chatting" which is an active choice,
/// not the absence of input.
pub fn evaluate(text: &str) -> ScenePacing {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // Empty / whitespace → neutral Exploration (all pillar scores 0).
        return ScenePacing::default();
    }
    let lower = text.to_lowercase();
    let spatial = score_spatial(&lower);
    let emotional = score_emotional(&lower);
    let kinetic = score_kinetic(&lower);
    let calm = calm_signal(&lower);
    let mode = classify(kinetic, emotional, spatial, calm);
    ScenePacing {
        mode,
        spatial,
        emotional,
        kinetic,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(text: &str) -> SceneMode {
        evaluate(text).mode
    }

    // --- Mode mapping ---

    #[test]
    fn combat_keyword_overrides_to_combat() {
        // Any combat keyword → Combat, regardless of other pillars.
        assert_eq!(mode_of("I attack the goblin."), SceneMode::Combat);
        assert_eq!(mode_of("I swing my sword at the dragon."), SceneMode::Combat);
        assert_eq!(mode_of("I punch the drunk in the tavern."), SceneMode::Combat);
        // Even in an enclosed calm scene, a combat keyword flips it.
        assert_eq!(
            mode_of("I sit in the tavern and stab the bartender."),
            SceneMode::Combat
        );
    }

    /// #35: the leading-boundary matcher (shared with the combat referee)
    /// must not see "fire" inside "campfire" — a restful scene stays restful
    /// instead of mis-classifying Combat while the referee correctly stays
    /// silent. (Word-INITIAL compounds like "fireplace" still match — that's
    /// #53's accepted referee semantics, unchanged here.)
    #[test]
    fn campfire_compound_does_not_classify_combat() {
        assert_eq!(mode_of("I sit by the campfire and rest."), SceneMode::Downtime);
        // A standalone "fire" remains a combat keyword (list semantics).
        assert_eq!(mode_of("I fire my bow at the wolf."), SceneMode::Combat);
    }

    #[test]
    fn no_movement_no_tension_is_downtime() {
        // Static + calm → Downtime.
        // NOTE: a standalone "fire" still matches the combat keyword "fire"
        // (the canonical list, shared with the combat Referee) — use "hearth"
        // for pure-rest fixtures. Mid-word compounds are covered by
        // `campfire_compound_does_not_classify_combat` below.
        assert_eq!(mode_of("I sit and rest by the hearth."), SceneMode::Downtime);
        assert_eq!(mode_of("I order a drink and chat with the barkeep."), SceneMode::Downtime);
        assert_eq!(mode_of("I trade for supplies."), SceneMode::Downtime);
    }

    #[test]
    fn mobile_without_combat_is_exploration() {
        // Walking/traveling → Exploration (kinetic=1, neither 0 nor 2).
        assert_eq!(mode_of("I walk to the market."), SceneMode::Exploration);
        assert_eq!(mode_of("I travel north along the road."), SceneMode::Exploration);
        assert_eq!(mode_of("I head to the harbor."), SceneMode::Exploration);
    }

    #[test]
    fn tension_without_combat_is_exploration() {
        // Tense but not moving → Exploration (kinetic=0, emotional=1 → not
        // Downtime because emotional != 0).
        assert_eq!(mode_of("I argue with the merchant."), SceneMode::Exploration);
        assert_eq!(mode_of("I watch the stranger suspiciously."), SceneMode::Exploration);
    }

    #[test]
    fn default_for_neutral_text_is_exploration() {
        // Empty / gibberish → Exploration (the safe default).
        assert_eq!(mode_of(""), SceneMode::Exploration);
        assert_eq!(mode_of("   "), SceneMode::Exploration);
        // Text with no recognized keywords (proper noun prose) → Exploration.
        assert_eq!(mode_of("I quux the fnord."), SceneMode::Exploration);
    }

    // --- Pillar scores (kept for tracing + future tuning) ---

    #[test]
    fn spatial_pillar_wilderness_beats_civil() {
        // Tier2 (wilderness) wins over Tier1 (civil) when both appear.
        let s = evaluate("I walk from the street into the forest.");
        assert_eq!(s.spatial, 2, "wilderness must beat civil: forest + street → 2");
    }

    #[test]
    fn spatial_pillar_civil_when_no_wilderness() {
        let s = evaluate("I cross the market square.");
        assert_eq!(s.spatial, 1);
    }

    #[test]
    fn spatial_pillar_zero_when_neither() {
        let s = evaluate("I sit in the corner.");
        assert_eq!(s.spatial, 0);
    }

    #[test]
    fn emotional_pillar_alarmed_beats_tense() {
        let s = evaluate("I argue with the merchant, then panic.");
        assert_eq!(s.emotional, 2, "alarmed must beat tense");
    }

    #[test]
    fn kinetic_pillar_combat_beats_mobile() {
        let s = evaluate("I walk over and attack the guard.");
        assert_eq!(s.kinetic, 2, "combat must beat mobile");
    }

    // --- Determinism + case-insensitivity ---

    #[test]
    fn evaluate_is_deterministic_for_same_text() {
        let a = evaluate("I sneak past the sleeping guard.");
        let b = evaluate("I sneak past the sleeping guard.");
        assert_eq!(a, b);
    }

    #[test]
    fn evaluate_is_case_insensitive() {
        let lower = evaluate("i attack the goblin.");
        let upper = evaluate("I ATTACK THE GOBLIN.");
        let mixed = evaluate("I aTtAcK the goblin.");
        assert_eq!(lower.mode, SceneMode::Combat);
        assert_eq!(lower.mode, upper.mode);
        assert_eq!(lower.mode, mixed.mode);
    }

    // --- SceneMode helpers (tag / prose / interval / dc_mod) ---

    #[test]
    fn scene_mode_tag_round_trips() {
        assert_eq!(SceneMode::Combat.tag(), "combat");
        assert_eq!(SceneMode::Exploration.tag(), "exploration");
        assert_eq!(SceneMode::Downtime.tag(), "downtime");
    }

    #[test]
    fn scene_mode_progression_intervals_make_sense() {
        // Combat = 0 (never fire mid-combat); Downtime (1h) > Exploration (4h)??
        // NO — Downtime should fire FASTER (world moves while you rest).
        // Exploration 4h, Downtime 1h, Combat 0. Verify the ordering.
        assert_eq!(SceneMode::Combat.progression_interval_hours(), 0);
        assert_eq!(SceneMode::Exploration.progression_interval_hours(), 4);
        assert_eq!(SceneMode::Downtime.progression_interval_hours(), 1);
        // Downtime fires more often than Exploration (smaller interval).
        assert!(
            SceneMode::Downtime.progression_interval_hours()
                < SceneMode::Exploration.progression_interval_hours()
        );
    }

    #[test]
    fn scene_mode_dc_modifier_ordering() {
        // Combat +2, Exploration 0, Downtime -2. Tension raises stakes.
        assert_eq!(SceneMode::Combat.dc_modifier(), 2);
        assert_eq!(SceneMode::Exploration.dc_modifier(), 0);
        assert_eq!(SceneMode::Downtime.dc_modifier(), -2);
        assert!(SceneMode::Combat.dc_modifier() > SceneMode::Exploration.dc_modifier());
        assert!(SceneMode::Exploration.dc_modifier() > SceneMode::Downtime.dc_modifier());
    }

    /// 2026-08-15 inversion fix: the lethality mod is the MIRRORED sign of
    /// the skill-check mod — combat makes killing blows land easier, not
    /// harder. Combat must never be the safest mode to take a hit.
    #[test]
    fn scene_mode_lethality_modifier_mirrors_dc_modifier() {
        assert_eq!(SceneMode::Combat.lethality_dc_mod(), -1);
        assert_eq!(SceneMode::Exploration.lethality_dc_mod(), 0);
        assert_eq!(SceneMode::Downtime.lethality_dc_mod(), 2);
        // Ordering: combat is the deadliest place to take a blow, downtime
        // the safest — the intuitive reading, pinned.
        assert!(SceneMode::Combat.lethality_dc_mod() < SceneMode::Exploration.lethality_dc_mod());
        assert!(SceneMode::Exploration.lethality_dc_mod() < SceneMode::Downtime.lethality_dc_mod());
        // Common-case guard (Chloe's "not too deadly"): a Soldier (+4 tier
        // mod) still cannot one-shot an Unscathed player even in Combat —
        // BASE 18 + 4 − 1 = 21 beats a max d20.
        assert!(crate::player_state::BASE_LETHAL_DC + 4
            + SceneMode::Combat.lethality_dc_mod()
            > 20);
    }

    #[test]
    fn scene_mode_prose_guidance_is_nonempty_and_distinct() {
        let c = SceneMode::Combat.prose_guidance();
        let e = SceneMode::Exploration.prose_guidance();
        let d = SceneMode::Downtime.prose_guidance();
        assert!(!c.is_empty() && !e.is_empty() && !d.is_empty());
        assert_ne!(c, e);
        assert_ne!(e, d);
        assert_ne!(c, d);
    }

    // --- Default ---

    #[test]
    fn scene_pacing_default_is_exploration_neutral() {
        let s = ScenePacing::default();
        assert_eq!(s.mode, SceneMode::Exploration);
        assert_eq!(s.spatial, 0);
        assert_eq!(s.emotional, 0);
        assert_eq!(s.kinetic, 0);
    }
}
