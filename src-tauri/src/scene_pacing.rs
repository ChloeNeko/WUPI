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
// `player_state::keyword_present` (BOTH-side word boundaries — the SAME
// matcher convention as `player_state::COMBAT_KEYWORDS`, #35 + 2026-08-16
// P2b). Conservative: false-negative cost is a "neutral" classification
// (default Exploration); false-positive cost is a mis-paced scene. Both are
// recoverable on the next turn, so the bar is "good default + obvious
// matches." Inflections are explicit (the trailing boundary killed the old
// free suffix ride: "streets" no longer contains "street" as a whole word).
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
    "street", "streets", "market", "markets", "square", "squares", "plaza",
    "plazas", "alley", "alleys", "road", "roads", "path", "paths", "bridge",
    "bridges", "harbor", "docks", "dock", "wharf", "gate", "gates", "wall",
    "walls", "tower", "towers", "courtyard", "courtyards", "yard", "yards",
    "village", "villages", "town", "towns", "city",
];

const SPATIAL_WILDERNESS: &[&str] = &[
    "forest", "forests", "woods", "jungle", "jungles", "desert", "deserts",
    "mountain", "mountains", "hill", "hills", "valley", "valleys",
    "ocean", "sea", "seas", "river", "rivers", "lake", "lakes", "stream",
    "streams", "field", "fields", "meadow", "meadows", "plain", "plains",
    "swamp", "swamps", "marsh", "marshes", "wastes", "wilderlands", "trail",
    "trails", "wilderness", "dungeon", "dungeons", "ruin", "ruins", "wilds",
];

/// Emotional vector: the affective register of the scene.
const EMOTIONAL_CALM: &[&str] = &[
    "rest", "rests", "rested", "resting",
    "sleep", "sleeps", "slept", "sleeping",
    "relax", "relaxes", "relaxed", "relaxing",
    "wait", "waits", "waited", "waiting",
    "sit", "sits", "sat", "sitting",
    "drink", "drinks", "drank", "drinking",
    "eat", "eats", "ate", "eating",
    "dine", "dines", "dined", "dining",
    "chat", "chats", "chatted", "chatting",
    "talk", "talks", "talked", "talking",
    "listen", "listens", "listened", "listening",
    "watch", "watches", "watched", "watching",
    "trade", "trades", "traded", "trading",
    "barter", "barters", "bartered", "bartering",
    "buy", "buys", "buying", "bought",
    "sell", "sells", "sold", "selling",
    "shop", "shops", "shopping",
    "browse", "browses", "browsing",
    "hum", "hums", "humming",
    "sing softly", "singing softly",
    "think", "thinks", "thinking",
    // (2026-08-16 audit fix #7) The recovery Referee needs Downtime AND a
    // REST_KEYWORD; these rest verbs were in REST_KEYWORDS but NOT here, so
    // "we camp for the night" classified Exploration → zero recovery — the
    // monotonic-decline economy the recovery seam exists to exit persisted
    // for exactly the verbs its own doc comment lists. "watch" stays OUT of
    // the tense pillar (see below) but IN here — a watching sentry camps.
    "camp", "camps", "camped", "camping",
    "nap", "naps", "napped", "napping",
    "recuperate", "recuperating", "convalesce", "convalescing",
    "bandage", "bandages", "bandaging", "mend", "mends", "mending",
    // (P1e, 2026-08-17 E4B shakedown) The T43 gap: "exhaustion wins — I
    // sleep, hard, for several hours" classified Exploration, so the
    // Recovery Referee's Downtime gate never opened. sleep/nap were already
    // here — the missing rest phrasings are the lie-down + doze families.
    "lie down", "lies down", "lay down", "laid down", "lying down",
    "doze", "dozes", "dozed", "dozing",
    "bed down", "beds down", "bedded down",
    "turn in", "turns in", "turned in",
];

const EMOTIONAL_TENSE: &[&str] = &[
    // (2026-08-16 P2b) the old prefix stub "argu" is dead under the boundary
    // matcher — explicit forms replace it.
    "argue", "argues", "argued", "arguing",
    "disagree", "disagrees", "disagreed", "disagreeing",
    "negotiate", "negotiates", "negotiated", "negotiating",
    "bargain hard", "bargaining hard",
    "suspect", "suspects", "suspected", "suspecting",
    "suspicious", "suspiciously",
    "wary", "warily", "wariness",
    "distrust", "distrusts", "distrusted", "distrusting",
    "tense", "tensely", "tensed",
    "uneasy", "uneasily",
    "guard", "guards", "guarded", "guarding",
    "study", "studies", "studied", "studying",
    "investigate", "investigates", "investigated", "investigating",
    "search", "searches", "searched", "searching",
    "examine", "examines", "examined", "examining",
    "interrogate", "interrogates", "interrogated", "interrogating",
    "question", "questions", "questioned", "questioning",
    "plead", "pleads", "pleaded", "pleading",
    "beg", "begs", "begged", "begging",
    "plea", "pleas",
    // (2026-08-15 audit fix) "watch" REMOVED — it also lives in CALM, and the
    // tier scoring let TENSE win, so "I sit and watch the fire" classified
    // Exploration instead of Downtime (also suppressing the recovery referee:
    // its Downtime gate never opened for a restful watching turn). Tense
    // watching is still covered by "suspicious(ly)" / "wary" / "guard".
];

const EMOTIONAL_ALARMED: &[&str] = &[
    "panic", "panics", "panicked", "panicking",
    "flee", "flees", "fled", "fleeing",
    "run away", "runs away", "ran away", "running away",
    "ambush", "ambushes", "ambushed", "ambushing",
    "trap", "traps", "trapped",
    "danger", "dangers", "dangerous",
    "terrify", "terrifies", "terrified", "terrifying",
    "horror", "horrors", "horrified",
    "scream", "screams", "screamed", "screaming",
    "shriek", "shrieks", "shrieked", "shrieking",
    "stampede", "stampedes", "stampeded",
    "alarm", "alarms", "alarmed", "alarming",
    "trap is sprung",
    "betrayal", "betrayals", "betrayed",
];

/// Kinetic scale: the action intensity. The Combat tier delegates to the
/// SHARED two-tier combat gate (`player_state::combat_triggered` over
/// `COMBAT_HARD_KEYWORDS` / `COMBAT_SOFT_KEYWORDS`, P1e 2026-08-17) so
/// scene-pacing and the combat Referee agree on what counts as "combat" BY
/// CONSTRUCTION — one source of truth, no mirror list to keep in sync (the
/// old re-declared `KINETIC_COMBAT` mirror is gone; the
/// `combat_keywords_match_scene_pacing_combat` test in `player_state.rs`
/// still pins the agreement through the shared gate).
/// Soft keywords (hunt/raid/arrest/fight/chase) corroborate — a gossip
/// question about "arrests" + "hunting" (T46) no longer mis-paces a
/// stew-and-chat turn as Combat.

const KINETIC_MOBILE: &[&str] = &[
    "walk", "walks", "walked", "walking",
    "go to", "goes to", "went to", "going to",
    "head to", "heads to", "headed to", "heading to",
    "travel", "travels", "traveled", "traveling", "travelling",
    "wander", "wanders", "wandered", "wandering",
    "stroll", "strolls", "strolled", "strolling",
    "march", "marches", "marched", "marching",
    "ride", "rides", "rode", "ridden", "riding",
    "sail", "sails", "sailed", "sailing",
    "fly", "flies", "flew", "flown", "flying",
    "teleport", "teleports", "teleported", "teleporting",
    "fast-travel", "fast travel", "fast-traveling", "fast traveling",
    "journey", "journeys", "journeyed", "journeying",
    "depart", "departs", "departed", "departing",
    "leave", "leaves", "left", "leaving",
    "enter", "enters", "entered", "entering",
    "arrive", "arrives", "arrived", "arriving",
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
/// Combat (Tier2) = the SHARED two-tier combat gate (hard-alone or ≥2 soft)
/// — see the module-level comment above.
fn score_kinetic(lower: &str) -> u8 {
    if crate::player_state::combat_triggered(lower) {
        2
    } else if KINETIC_MOBILE
        .iter()
        .any(|kw| crate::player_state::keyword_present(lower, kw))
    {
        1
    } else {
        0
    }
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
    // (P1e, 2026-08-17 E4B shakedown) Dialogue-stripped scoring: quoted
    // speech is not action. T46's "Is Harsk still hunting…" (spoken as a
    // question) must not pace the scene as Combat, and T22's quoted "The
    // rest when the heat dies down" must not heal anyone.
    let lower = crate::player_state::strip_dialogue(text).to_lowercase();
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

    /// (2026-08-16 P2b) Inflected forms classify identically to their base
    /// verbs — the keyword lists carry explicit inflection entries now that
    /// `keyword_present` enforces both word boundaries.
    #[test]
    fn inflected_forms_classify_like_base_verbs() {
        // Combat inflections.
        assert_eq!(mode_of("The goblin is attacking me."), SceneMode::Combat);
        assert_eq!(mode_of("I was attacked from the shadows."), SceneMode::Combat);
        assert_eq!(mode_of("We swam across the lake."), SceneMode::Combat);
        assert_eq!(mode_of("They charged the gate."), SceneMode::Combat);
        // Mobile inflections → Exploration.
        assert_eq!(mode_of("I am walking to the market."), SceneMode::Exploration);
        assert_eq!(mode_of("We traveled north for hours."), SceneMode::Exploration);
        // Calm/rest inflections → Downtime.
        assert_eq!(mode_of("I am resting by the hearth."), SceneMode::Downtime);
        assert_eq!(mode_of("We slept until dawn."), SceneMode::Downtime);
        assert_eq!(mode_of("I watched the dancers while I ate."), SceneMode::Downtime);
        // Tense inflections → Exploration (kinetic 0, emotional 1).
        assert_eq!(mode_of("We argued about the price."), SceneMode::Exploration);
        assert_eq!(mode_of("I studied the stranger suspiciously."), SceneMode::Exploration);
        // Alarmed inflections → emotional 2.
        let s = evaluate("The villagers panicked and fled.");
        assert_eq!(s.emotional, 2, "inflected alarmed verbs must score");
        // Spatial inflections still score.
        let s = evaluate("I walked from the streets into the mountains.");
        assert_eq!(s.spatial, 2, "wilderness beats civil with both inflected");
        // Compounds stay inert: derived nouns must never classify.
        assert_ne!(mode_of("A runner arrived with a message."), SceneMode::Combat);
        assert_eq!(mode_of("The firelight was warm."), SceneMode::Exploration,
            "'firelight' must not classify Combat");
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
        // Downtime because emotional != 0). NOTE: "watch the stranger
        // SUSPICIOUSLY" is tense via "suspicious(ly)" — "watch" alone is a
        // CALM keyword (the 2026-08-15 fix; it used to sit in both lists).
        assert_eq!(mode_of("I argue with the merchant."), SceneMode::Exploration);
        assert_eq!(mode_of("I watch the stranger suspiciously."), SceneMode::Exploration);
    }

    /// 2026-08-15 fix pin: "watch" is CALM-only. A restful watching turn
    /// classifies Downtime (the recovery referee's Downtime gate opens).
    /// ("hearth", not "fire" — a standalone "fire" is a combat keyword by
    /// the shared referee list; the campfire-compound case is pinned above.)
    #[test]
    fn watching_restfully_is_downtime() {
        assert_eq!(mode_of("I sit and watch the hearth."), SceneMode::Downtime);
        assert_eq!(mode_of("I watch the dancers while I eat."), SceneMode::Downtime);
    }

    #[test]
    fn default_for_neutral_text_is_exploration() {
        // Empty / gibberish → Exploration (the safe default).
        assert_eq!(mode_of(""), SceneMode::Exploration);
        assert_eq!(mode_of("   "), SceneMode::Exploration);
        // Text with no recognized keywords (proper noun prose) → Exploration.
        assert_eq!(mode_of("I quux the fnord."), SceneMode::Exploration);
    }

    // --- P1e (2026-08-17 E4B shakedown): the false-positive corpus ---
    // Built from the ACTUAL 51-turn playtest turns (T22/T43/T46/T47) — the
    // E4B's failure mode was keywords matching inside SPOKEN text and
    // single soft verbs matching ABOUT-OTHERS gossip. Dialogue is stripped
    // before scoring; soft combat verbs corroborate.

    #[test]
    fn p1e_t46_gossip_questions_are_not_combat() {
        // T46 verbatim fragments: a stew-and-gossip turn mis-classified
        // Combat (real injury rolls followed — one Purple).
        assert_eq!(
            mode_of("I eat slowly and keep my voice low. \"Have there been any arrests since the docks? Is Harsk still hunting for the one who cut the moorings?\""),
            SceneMode::Downtime,
            "quoted gossip questions must not pace Combat (T46)"
        );
        // Unquoted ABOUT-OTHERS gossip: a SINGLE soft verb corroborates
        // nothing — it must not fire alone.
        assert_ne!(
            mode_of("I ask whether there have been any arrests."),
            SceneMode::Combat,
            "a lone about-others 'arrests' must not pace Combat"
        );
        // (NB: "the watch raided…" is a poor fixture — noun "watch" is a
        // CALM keyword, correctly classifying Downtime. Use a watch-free
        // report; the P1e invariant under test is NOT-Combat either way.)
        assert_ne!(
            mode_of("They say the constables raided the eel-shed last night."),
            SceneMode::Combat,
            "a reported raid is news, not a fight (T47)"
        );
    }

    #[test]
    fn p1e_t43_sleep_classifies_downtime() {
        // T43 verbatim: "exhaustion wins — I sleep, hard, for several hours"
        // classified Exploration, so the Recovery Referee's Downtime gate
        // never opened. sleep was already a calm keyword — the classification
        // died on the long turn's other words; pin the canonical phrasings.
        assert_eq!(
            mode_of("exhaustion wins — I sleep, hard, for several hours"),
            SceneMode::Downtime
        );
        assert_eq!(mode_of("I lie down on the straw and doze off."), SceneMode::Downtime);
        assert_eq!(mode_of("We bed down for the night."), SceneMode::Downtime);
    }

    #[test]
    fn p1e_positive_combat_controls_still_fire() {
        // The referee's bar must survive the two-tier split: direct violence
        // fires ALONE, quoted or not.
        assert_eq!(mode_of("I attack the goblin."), SceneMode::Combat);
        assert_eq!(mode_of("I stab the bartender."), SceneMode::Combat);
        assert_eq!(mode_of("I shove past the guard and lunge for the door."), SceneMode::Combat);
        assert_eq!(mode_of("The goblin is attacking me!"), SceneMode::Combat);
        // Two soft verbs corroborate: an actual hunt + chase is kinetic.
        assert_eq!(
            mode_of("We are hunting the stag, chasing it through the brush."),
            SceneMode::Combat
        );
    }

    #[test]
    fn p1e_quoted_speech_never_paces_the_scene() {
        // The T22 family: quoted dialogue is speech, not action — a quoted
        // combat verb inside a negotiation must not pace Combat.
        assert_eq!(
            mode_of("I raise my hands. \"I'm not here to fight, I only want to talk.\""),
            SceneMode::Exploration,
            "a quoted 'fight' inside a de-escalation line must not pace Combat"
        );
        assert_eq!(
            mode_of("I drink while Mara laughs. \"The rest when the heat dies down,\" she says."),
            SceneMode::Downtime,
            "unquoted calm actions pace the scene; the quoted 'rest' must not do referee work (T22)"
        );
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
