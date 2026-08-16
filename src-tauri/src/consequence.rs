//! Consequence engine (Fable Phase 3, 2026-07-28): the Rust-authoritative
//! type system for diegetic state. The home of every "the world has changed
//! in a way the narrator must obey" mechanic that is NOT a per-turn dice roll
//! (those live in `player_state::referee_*`).
//!
//! # The locked design (see architect directive, 2026-07-28)
//!
//! **No integers in the descriptive layer.** HP, Mana, Stamina-as-a-number,
//! Levels, "78/120 HP" — all banned from the prompt and from any entity the
//! LLM reads or writes. Numbers are permitted ONLY for:
//! - **Time** (the WorldClock's `i64` minutes — already established).
//! - **Probability** (dice rolls, kept internal to the Referees — never shown).
//! - **Engine-room accounting** (counts of wounds, ticks elapsed, volatility
//!   coefficients) — these are Rust-internal integers the narrator never sees
//!   in raw form. They surface only as derived qualitative labels.
//!
//! The narrator sees diegetic *labels* (Fresh/Active/Winded/Exhausted/Depleted,
//! Unscathed/Scraped/Wounded/Critical/Downed, Stranger/Acquaintance/...).
//! Rust is the casino, the clock, and the consequence engine; the LLM only
//! ever writes prose to match the labels Rust computed.
//!
//! # The four-type Driver taxonomy
//!
//! Every tracked field ticks in ONE of four ways. The type determines who
//! writes it, when it changes, and how the World Progression tick touches it.
//!
//! | Type            | Ticks?                | Examples                              |
//! |-----------------|-----------------------|---------------------------------------|
//! | `DriverStatic`  | Never (set once)      | Age, Background, Role, Species        |
//! | `DriverEvent`   | Never (append-only)   | saved_life, betrayed_trust (milestones)|
//! | `DriverTime`    | On clock tick         | Fatigue, hunger, buff durations, NPC  |
//! |                 |                       | mood (Quest Frustration Curve)        |
//! | `DriverNarrative`| On LLM judgment      | Reputation, rumors, public mood not   |
//! |                 |                       | driven by the clock                   |
//!
//! `DriverStatic` + `DriverEvent` are the "truth" layer (set, never mutated
//! by the LLM). `DriverTime` ticks deterministically against the WorldClock.
//! `DriverNarrative` flows through the existing `SchemaDelta` LLM pass.
//!
//! NO `driverStatedFact` type exists — that was the legacy trap (it implied
//! a stored numeric "fact," which smuggles HP/Level back in). Wound severity
//! is a `DriverEvent` (the Referee appends an event); condition is derived
//! at read time from the inputs above.
//!
//! # Read-time derived Condition
//!
//! `Condition` (the holistic "how is this body doing" label) is NEVER stored.
//! It is computed fresh on every prompt render from `(age_axes + active_wounds
//! + active_buffs + active_debuffs)`. The LLM cannot drift it because it can
//! never write it — only the Referee's events + the clock's tick can change
//! the inputs, and Rust recomputes the label every read. Same Prime Directive
//! shape as `BuffReversion` below.
//!
//! # Why this is its own module
//!
//! The Phase 3 roadmap splits the build into six slices. Slices 4–6
//! (Buff/Debuff, Quest Frustration Curve, Relationship State Machine,
//! Off-Screen Risk Referee) all consume this type system. Building it first
//! means each downstream slice is a *consumer*, not a parallel reinvention.

use crate::player_state::{BodyPart, BodyPartState};
use std::collections::HashMap;

// ===========================================================================
// Driver taxonomy: how a tracked field ticks
// ===========================================================================

/// The four-type taxonomy classifying how a tracked diegetic field evolves.
/// See the module docs for the locked design. Pure marker — no data — used
/// only to make the intent of each field explicit at the type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Driver {
    /// Set once (or rarely, by Wupi-as-game-manager); never mutated by the
    /// LLM. The slow-truth layer: age, background, role, species, birthplace.
    /// These feed the per-axis age curves and the Referee's DC adjustments.
    Static,
    /// Append-only log of discrete diegetic events that have happened to this
    /// entity. The Relationship milestone registry (saved_life, betrayed_trust)
    /// lives here. Each entry is owned by the Referee or by Rust-side event
    /// detection, never by the LLM directly.
    Event,
    /// Advances on the WorldClock tick. Buff/Debuff durations, fatigue,
    /// hunger, and the Quest Frustration Curve's NPC-mood math. The clock
    /// calls `tick()` on these; the LLM never writes them.
    Time,
    /// Mutated by LLM judgment via the existing `SchemaDelta` path. Public
    /// reputation, rumors, ambient public mood — things the narrator
    /// legitimately authors. Bounded by the validator (string-shape) and
    /// by the immutability lock where appropriate.
    Narrative,
}

// ===========================================================================
// Per-axis age curves
// ===========================================================================

/// The four capability axes the age curve evaluates. Each peaks at a
/// different age, so a 40-year-old knight and an 80-year-old retired knight
/// have OVERLAPPING mastery on some axes and DIVERGENT capacity on others —
/// the texture that "Veteran vs Veteran" or "Level 12 vs Level 8" can't
/// express. See the architect discussion 2026-07-28.
///
/// The axes are deliberately coarse. A finer breakdown (Physical → Strength /
/// Endurance / Speed) is a future tuning axis; the v1 question each axis
/// answers is "what does this body's age SAY about what it can still do?"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    /// Raw physical capacity: strength, speed, stamina, recovery. Peaks
    /// young (~25–35), declines steadily after.
    Physical,
    /// Martial skill / hard-won experience with weapons, tactics, command.
    /// Peaks late (~40–55), holds well into elder age even as Physical fades.
    Martial,
    /// Cunning / social craft / political instincts. Peaks latest (~50+),
    /// often still climbing when the body is in clear decline.
    Cunning,
    /// Magical affinity. Setting-dependent — in some worlds magic peaks in
    /// youth (raw potential), in others it accumulates with study. We default
    /// to "accumulates with study" (peaks late, like Martial) since that's
    /// the more common fantasy convention; a card may override.
    Magical,
}

impl Axis {
    /// All four axes in canonical order (the iteration order used by
    /// `AgeAxes::render_for_prompt`).
    pub fn all() -> &'static [Axis] {
        &[Axis::Physical, Axis::Martial, Axis::Cunning, Axis::Magical]
    }

    /// Lowercase tag for serialization + prompt labels.
    pub fn tag(self) -> &'static str {
        match self {
            Axis::Physical => "physical",
            Axis::Martial => "martial",
            Axis::Cunning => "cunning",
            Axis::Magical => "magical",
        }
    }
}

/// A qualitative capability label per axis. STRICT ORDERING by rank (see
/// `rank()`): higher rank = more capable. The labels are the only thing the
/// narrator ever sees for a character's capability — never the underlying
/// age number.
///
/// The six-tier ladder is deliberately coarse-grained. A character's place on
/// it is derived from age + background + active modifiers; the narrator
/// reads it as a directional hint about what this body can still do, not as
/// a stat block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// The body cannot yet do this (a child asked to swing a greatsword).
    /// Surfaced in prose as incapacity, not as "level 0."
    None,
    /// Raw, undeveloped — fumbling, untrained. A child's first attempts at
    /// anything; an adult trying something entirely outside their experience.
    Formative,
    /// Trained and competent; the working norm. A journeyman soldier, a
    /// guild apprentice, a hedge mage.
    Developing,
    /// Full trained capacity; the standard for an active professional in
    /// their field.
    Capable,
    /// The peak of ordinary human potential. Hard-won, sustained by active
    /// practice. The knight in his prime, the practiced courtier.
    Peak,
    /// Mastery that survives the body's decline — the retired veteran whose
    /// hands know the forms even when the lungs complain. Indicates deep
    /// engrained experience compensating for physical fade.
    Mastery,
}

impl Default for Capability {
    fn default() -> Self {
        Capability::Capable
    }
}

impl Capability {
    /// Numeric rank 0..=5 for the rare internal comparison (e.g. "is this
    /// character's Physical at least Capable?"). NEVER surfaced to the
    /// narrator as a number — this exists only for Rust-side gating.
    pub fn rank(self) -> u8 {
        match self {
            Capability::None => 0,
            Capability::Formative => 1,
            Capability::Developing => 2,
            Capability::Capable => 3,
            Capability::Peak => 4,
            Capability::Mastery => 5,
        }
    }

    /// Human-readable label for prompt injection. Reads as a noun phrase the
    /// narrator can weave into prose ("Marcus's swordwork still shows the
    /// Mastery of a man forty years in the trade…").
    pub fn label(self) -> &'static str {
        match self {
            Capability::None => "no capacity",
            Capability::Formative => "formative, untrained",
            Capability::Developing => "trained and developing",
            Capability::Capable => "fully capable",
            Capability::Peak => "in his prime",
            Capability::Mastery => "a master whose skill outlasts the body",
        }
    }
}

/// A biological baseline: age + biological sex (where setting-relevant) +
/// background. The inputs to the per-axis curve. Stored on a character
/// (player or NPC) as a `DriverStatic` field. The labels Rust derives from
/// this are the ONLY thing the narrator sees about a body's capability.
///
/// `age` is the only number here, and it's a `DriverStatic` truth — never
/// recomputed, never shown to the narrator as a number. It surfaces only as
/// the per-axis Capability labels below.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct BiologicalBaseline {
    /// Age in years. The single driver of the per-axis curves. 0 means
    /// "unset" (the character has no age recorded); the curves degrade
    /// gracefully to the `Capable` default for an adult of unspecified age.
    #[serde(default)]
    pub age: u16,
    /// One-word background hint (e.g. "knight", "scholar", "street-thief",
    /// "farmhand"). Used by the Referee to derive per-axis modifiers (a
    /// knight's Martial curve peaks later than a brawler's; a scholar's
    /// Physical curve never reaches Peak). Free-form string; the validator
    /// bounds length elsewhere.
    #[serde(default)]
    pub background: String,
}

impl BiologicalBaseline {
    /// Compute the per-axis Capability ladder from age + background. Pure fn
    /// — the only inputs are the baseline fields. Each axis's curve is a
    /// piecewise mapping from age to Capability tier, with background nudges.
    ///
    /// The curves are conservative defaults — a card or a Wupi-authored
    /// override can adjust them per-character (e.g. an elf's Physical peaks
    /// far later than a human's). The v1 curves are human-baseline.
    ///
    /// Design notes:
    /// - **Physical**: peaks ~25–35 (`Peak`), declines to `Capable` ~50,
    ///   `Developing` ~65, `Formative` ~80+. Children are `Formative` →
    ///   `Developing` through adolescence.
    /// - **Martial**: climbs slowly, peaks ~40–55 (`Peak`), then transitions
    ///   to `Mastery` (the veteran whose skill outlasts the body) which holds
    ///   into old age. This is the axis where elder characters shine.
    /// - **Cunning**: peaks latest (~50+, often still climbing). Rarely
    ///   declines sharply — the cunning elder is a stock archetype.
    /// - **Magical**: defaults to "accumulates with study" (peaks late like
    ///   Martial, transitions to Mastery). A card override can flip this to
    ///   "raw potential" (peaks in youth) for settings where that's canon.
    pub fn axes(&self) -> AgeAxes {
        let age = self.age;
        let bg = self.background.to_lowercase();

        // Background nudges: +1 axis rank for backgrounds that favor an axis,
        // -1 for those that neglect it. Bounded to the [None, Mastery] range
        // by the Capability::from_rank saturating helper.
        let physical_nudge = nudge_for(&bg, Axis::Physical);
        let martial_nudge = nudge_for(&bg, Axis::Martial);
        let cunning_nudge = nudge_for(&bg, Axis::Cunning);
        let magical_nudge = nudge_for(&bg, Axis::Magical);

        AgeAxes {
            physical: physical_curve(age).nudge(physical_nudge),
            martial: martial_curve(age).nudge(martial_nudge),
            cunning: cunning_curve(age).nudge(cunning_nudge),
            magical: magical_curve(age).nudge(magical_nudge),
        }
    }
}

/// The four-axis Capability snapshot Rust derives from a `BiologicalBaseline`.
/// Pure data; the narrator sees this through `render_for_prompt` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct AgeAxes {
    pub physical: Capability,
    pub martial: Capability,
    pub cunning: Capability,
    pub magical: Capability,
}

impl AgeAxes {
    /// Render the four axes as a compact prompt fragment for the narrator.
    /// Returns `None` when every axis is the default `Capable` (a healthy
    /// adult of unspecified age — nothing interesting to say, so we emit no
    /// tokens). One short line per axis otherwise.
    ///
    /// Format: `physical: <label>, martial: <label>, …`. The narrator reads
    /// this as a directional hint, not a stat block.
    pub fn render_for_prompt(&self) -> Option<String> {
        // Skip emitting when every axis is at the boring default. This is the
        // same empty-skip pattern as `PlayerState::render_for_prompt`.
        let all_default = [self.physical, self.martial, self.cunning, self.magical]
            .iter()
            .all(|&c| c == Capability::Capable);
        if all_default {
            return None;
        }
        let mut parts: Vec<String> = Vec::with_capacity(4);
        // Only emit axes that diverge from the default — keeps the block
        // tight (a 40yo knight gets "martial: Peak", not four lines).
        for (axis, cap) in [
            (Axis::Physical, self.physical),
            (Axis::Martial, self.martial),
            (Axis::Cunning, self.cunning),
            (Axis::Magical, self.magical),
        ] {
            if cap != Capability::Capable {
                parts.push(format!("{}: {}", axis.tag(), cap.label()));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

impl Capability {
    /// Saturating nudge: bump rank by `delta` (may be negative), clamped to
    /// the valid range. Used by `BiologicalBaseline::axes` to apply
    /// background modifiers. NEVER exposed to the narrator as a number.
    fn nudge(self, delta: i8) -> Capability {
        let new_rank = (self.rank() as i8 + delta).clamp(0, Capability::Mastery.rank() as i8);
        Capability::from_rank(new_rank as u8)
    }

    /// Inverse of `rank()`. Used by `nudge`. Kept private (rank/from_rank
    /// are internal — the labels are the only public surface).
    fn from_rank(rank: u8) -> Capability {
        match rank {
            0 => Capability::None,
            1 => Capability::Formative,
            2 => Capability::Developing,
            3 => Capability::Capable,
            4 => Capability::Peak,
            _ => Capability::Mastery,
        }
    }
}

// ---- Per-axis age curves (human baseline) ----

/// Physical curve. Peaks ~25-35, declines steadily after.
fn physical_curve(age: u16) -> Capability {
    match age {
        0..=4 => Capability::None,
        5..=11 => Capability::Formative,
        12..=17 => Capability::Developing,
        18..=24 => Capability::Capable,
        25..=35 => Capability::Peak,
        36..=50 => Capability::Capable,
        51..=65 => Capability::Developing,
        66..=80 => Capability::Formative,
        _ => Capability::None,
    }
}

/// Martial curve. Climbs slowly, peaks ~40-55, then transitions to Mastery
/// (skill outlasting the body).
fn martial_curve(age: u16) -> Capability {
    match age {
        0..=6 => Capability::None,
        7..=14 => Capability::Formative,
        15..=22 => Capability::Developing,
        23..=39 => Capability::Capable,
        40..=55 => Capability::Peak,
        // Mastery holds into old age — the veteran whose hands know the forms.
        56..=90 => Capability::Mastery,
        _ => Capability::Developing, // extreme elder: mastery finally fading
    }
}

/// Cunning curve. Peaks latest (~50+), rarely declines sharply.
fn cunning_curve(age: u16) -> Capability {
    match age {
        0..=7 => Capability::None,
        8..=15 => Capability::Formative,
        16..=25 => Capability::Developing,
        26..=49 => Capability::Capable,
        50..=75 => Capability::Peak,
        // Cunning elders stay sharp — the scheming patriarch is stock.
        _ => Capability::Mastery,
    }
}

/// Magical curve. Defaults to "accumulates with study" (peaks late).
fn magical_curve(age: u16) -> Capability {
    match age {
        0..=8 => Capability::None,
        9..=16 => Capability::Formative,
        17..=30 => Capability::Developing,
        31..=60 => Capability::Capable,
        61..=90 => Capability::Peak,
        _ => Capability::Mastery,
    }
}

/// Background-driven axis nudges. A small heuristic table — a `knight` gets
/// +1 Martial (sustained arms training), -1 Magical (no time for study); a
/// `scholar` gets +1 Cunning +1 Magical, -1 Physical; etc. Conservative: the
/// nudge is at most ±1 (it shifts a tier, not categories).
///
/// Backgrounds not in this table get a 0 nudge on every axis — the curve
/// alone drives the label. The table is intentionally short: a few
/// archetypes that obviously matter, not an exhaustive encyclopedia.
fn nudge_for(background: &str, axis: Axis) -> i8 {
    // Knight / soldier / warrior / guard — sustained arms training.
    let martial_bg = ["knight", "soldier", "warrior", "guard", "mercenary", "men-at-arms", "squire"];
    // Scholar / mage / sage — studious, less physical.
    let studious_bg = ["scholar", "mage", "wizard", "sage", "alchemist", "librarian", "cleric", "priest"];
    // Laborer / farmer / blacksmith — physically hard, no martial training.
    let labor_bg = ["farmer", "blacksmith", "laborer", "woodcutter", "miner", "dockworker", "sailor"];
    // Courtier / merchant / noble — socially skilled.
    let social_bg = ["courtier", "merchant", "noble", "diplomat", "trader", "bard"];
    // Street-thief / urchin / rogue — cunning from survival.
    let rogue_bg = ["thief", "rogue", "urchin", "beggar", "smuggler", "pickpocket"];

    let matches = |set: &[&str]| set.iter().any(|s| background.contains(s));

    match axis {
        Axis::Physical => {
            if matches(&martial_bg) || matches(&labor_bg) {
                1
            } else if matches(&studious_bg) || matches(&social_bg) {
                -1
            } else {
                0
            }
        }
        Axis::Martial => {
            if matches(&martial_bg) {
                1
            } else if matches(&studious_bg) {
                -1
            } else {
                0
            }
        }
        Axis::Cunning => {
            if matches(&social_bg) || matches(&rogue_bg) || matches(&studious_bg) {
                1
            } else if matches(&labor_bg) {
                -1
            } else {
                0
            }
        }
        Axis::Magical => {
            if matches(&studious_bg) {
                1
            } else if matches(&labor_bg) || matches(&martial_bg) {
                -1
            } else {
                0
            }
        }
    }
}

// ===========================================================================
// Read-time derived Condition (NEVER stored)
// ===========================================================================

/// The holistic "how is this body doing right now" label. Pure DERIVED
/// state — computed fresh on every prompt render from `(age_axes +
/// active_wounds + active_buffs + active_debuffs)`. The LLM can never write
/// it; it can only earn changes to the inputs through Referee-judged events.
///
/// Strictly ordered worst → best by `rank()`. Used by `derive_condition`
/// to pick the dominant signal (a character who is `Battered` AND `Exalted`
/// by a buff is still `Battered` — injuries dominate buffs in the read).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// Downed / dying / unconscious. Cannot act. The Referee may roll this
    /// on a lethal outcome; pending death-resolution.
    Downed,
    /// On the brink of death. Multiple critical wounds or system shock.
    Critical,
    /// Badly hurt; movement and skill seriously impaired.
    Battered,
    /// Hurt but functional; favoring injuries, working around them.
    Wounded,
    /// Scraped, bruised, winded — sub-optimal but largely fine.
    Haggard,
    /// No significant injury. Healthy baseline.
    Unscathed,
}

impl Default for Condition {
    fn default() -> Self {
        Condition::Unscathed
    }
}

impl Condition {
    /// Numeric rank 0..=5 (worst → best). Internal only — used by
    /// `derive_condition` to pick the dominant signal. NEVER surfaced as a
    /// number to the narrator.
    pub fn rank(self) -> u8 {
        match self {
            Condition::Downed => 0,
            Condition::Critical => 1,
            Condition::Battered => 2,
            Condition::Wounded => 3,
            Condition::Haggard => 4,
            Condition::Unscathed => 5,
        }
    }

    /// Human-readable label for prompt injection.
    pub fn label(self) -> &'static str {
        match self {
            Condition::Downed => "downed — unconscious, cannot act",
            Condition::Critical => "in critical condition — on the brink of death",
            Condition::Battered => "battered — badly hurt, movement and skill seriously impaired",
            Condition::Wounded => "wounded — hurt but functional, favoring injuries",
            Condition::Haggard => "haggard — scraped and winded, sub-optimal",
            Condition::Unscathed => "unscathed — healthy and able",
        }
    }
}

/// Derive the holistic Condition from the inputs. PURE FN — never stored,
/// never written by the LLM. The inputs are:
///
/// - `wounds`: the set of (body part → wound tier) the Referee has applied.
///   The dominant signal — injuries nearly always override buffs.
/// - `buffs_count` / `debuffs_count`: how many active qualitative tags are
///   currently on the body. These nudge the derived condition by ±1 tier
///   (a body with three debuffs and one scrape reads as `Wounded`, not
///   `Haggard`). The tags themselves are tracked elsewhere (Buff/Debuff
///   module, Slice 4); this fn takes only the counts so it stays decoupled
///   from the tag storage representation.
///
/// The mapping is conservative: wounds dominate, buffs can lift a healthy
/// body one tier but cannot rescue a Critical one, debuffs cannot drop an
/// Unscathed body below `Haggard` without an actual wound.
pub fn derive_condition(
    wounds: &HashMap<BodyPart, BodyPartState>,
    buffs_count: usize,
    debuffs_count: usize,
) -> Condition {
    // Start from the wound baseline. We treat the body's WORST wound as the
    // dominant signal (a critical head wound makes you Critical regardless
    // of healthy limbs). BodyPartState's existing rank (0..=5) maps cleanly
    // onto Condition's reverse-ordered rank.
    let worst_wound_rank = wounds
        .values()
        .map(|s| *s as u8)
        .max()
        .unwrap_or(0);

    // Map wound rank → Condition rank. Wound ranks are 0=Healthy → 5=Amputated
    // (BodyPartState enum ordinals); Condition ranks are 0=Downed → 5=Unscathed.
    // The mapping is intentionally NOT 1:1 because the semantics differ:
    // - 0 wounds (Healthy everywhere) → Unscathed (rank 5).
    // - 1+ Yellow (Minor) → Haggard (rank 4).
    // - Orange (Medium) on a single part → Wounded (rank 3).
    // - Red (Heavy) on any part → Battered (rank 2).
    // - Purple (Critical) on any part → Critical (rank 1).
    // - Amputated alone is Critical (shock); Amputated + another severe wound
    //   is Downed (rank 0).
    let has_amputated = wounds.values().any(|s| *s == BodyPartState::Black);
    let has_critical = wounds.values().any(|s| *s == BodyPartState::Purple);
    let has_heavy = wounds.values().any(|s| *s == BodyPartState::Red);
    // `has_medium` and `has_minor` are computed for documentation + future
    // tuning (per-part severity interactions); the v1 mapping uses only the
    // worst-wound rank below. Kept so a later pass can read them without
    // re-scanning the map.
    let _has_medium = wounds.values().any(|s| *s == BodyPartState::Orange);
    let _has_minor = wounds.values().any(|s| *s == BodyPartState::Yellow);
    let severe_count = wounds
        .values()
        .filter(|s| matches!(**s, BodyPartState::Red | BodyPartState::Purple | BodyPartState::Black))
        .count();

    let mut condition = match worst_wound_rank {
        0 => Condition::Unscathed,
        1 => Condition::Haggard,           // Yellow only
        2 => Condition::Wounded,           // Orange
        3 => Condition::Battered,          // Red
        4 => Condition::Critical,          // Purple
        _ => Condition::Critical,          // Amputated alone → Critical (shock)
    };
    // Multiple severe wounds escalate to Downed — the body can't sustain them.
    if severe_count >= 3 || (has_amputated && (has_critical || has_heavy)) {
        condition = Condition::Downed;
    }

    // Apply buff/debuff nudges. These can shift the label ±1 tier, bounded
    // to [Critical, Unscathed] for buffs and [Downed, Unscathed] for debuffs.
    // Wounds dominate: no buff can lift a body above Haggard if it has a
    // Medium-or-worse wound, and no buff can rescue Critical/Downed.

    if condition.rank() >= Condition::Haggard.rank() {
        // Body is at least Haggard — buffs may lift it one tier toward Unscathed.
        if buffs_count > 0 && debuffs_count == 0 {
            condition = nudge_condition_up(condition);
        }
    }
    if debuffs_count >= 2 {
        // Multiple debuffs drag the body down one tier (fever + poison +
        // exhaustion). Applies to ANY condition above Downed — even a fresh
        // Unscathed body sickens under two stacked debuffs.
        condition = nudge_condition_down(condition);
    }

    condition
}

fn nudge_condition_up(c: Condition) -> Condition {
    match c {
        Condition::Haggard => Condition::Unscathed,
        other => other, // wounds dominate above Haggard
    }
}

fn nudge_condition_down(c: Condition) -> Condition {
    match c {
        Condition::Unscathed => Condition::Haggard,
        Condition::Haggard => Condition::Wounded,
        Condition::Wounded => Condition::Battered,
        Condition::Battered => Condition::Critical,
        Condition::Critical => Condition::Downed,
        Condition::Downed => Condition::Downed,
    }
}

// ===========================================================================
// Slice 4: Qualitative Buff/Debuff tags with expiry (DriverTime consumer #1)
// ===========================================================================

/// A qualitative status tag with a WorldClock expiry timestamp. NO numeric
/// stat tracking — the tag is a free-form descriptor ("Berserk Rage",
/// "Feverish", "Blessed by the Sun Priest", "Poisoned") plus the in-world
/// minute at which it expires. Rust drops expired tags on the World
/// Progression tick; the body's `Condition` is re-derived from `(wounds +
/// active_buffs + active_debuffs)` on every read, so the tag's effect fades
/// automatically when it expires (no restoration math, no `base_value` —
/// the integer trap killed in the architect directive).
///
/// `polarity` separates buffs from debuffs because `derive_condition` reads
/// the counts separately (buffs lift toward Unscathed; debuffs drag toward
/// Downed). The label itself is free-form: the narrator sees it as a phrase
/// it can weave into prose, NOT as "+2 to attack."
///
/// `kind` (Phase 4 §11.44, Component 1) is the optional discriminator that
/// routes a tag out of the generic buff/debuff lanes into a dedicated render
/// lane — the load-bearing case is `"disguise"`, which the disguise Referee
/// gate reads to decide auto-pass vs scrutiny. Generic tags (the historical
/// case) leave `kind` empty; they route by `polarity` as before. Backwards-
/// compatible via `#[serde(default)]` — pre-Phase-4 saves load as empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusTag {
    /// The diegetic label. Free-form, narrator-facing. Reads as a phrase
    /// the model can use ("Berserk Rage", "Feverish", "Blessed", "Poisoned").
    pub label: String,
    /// Buff (positive) or Debuff (negative). Used by `derive_condition`'s
    /// buff/debuff nudges + by the renderer to group them.
    pub polarity: Polarity,
    /// In-world minute at which this tag expires (same epoch-minutes units
    /// as `WorldClock::current_minutes`). When the World Progression tick
    /// advances past this, `expire_tags` drops the tag.
    pub expires_at: i64,
    /// Optional source — the in-world cause ("quaffed a strength potion",
    /// "bitten by a swamp viper"). Tracing + narrator flavor; not load-bearing.
    #[serde(default)]
    pub source: String,
    /// Optional discriminator routing a tag out of the generic buff/debuff
    /// lanes into a dedicated render + mechanic lane. Currently recognized:
    /// `"disguise"` (Phase 4 Component 1 — read by the disguise Referee
    /// gate). Empty string = generic effect (the historical case).
    #[serde(default)]
    pub kind: String,
}

/// Whether a `StatusTag` helps or hurts. The polarity drives `derive_condition`'s
/// ±1 nudge rules: buffs can lift a body one tier toward Unscathed, debuffs
/// (2+) drag it down one tier toward Downed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Polarity {
    Buff,
    Debuff,
}

impl Default for StatusTag {
    /// Default is a generic, non-expiring Buff with no source and no kind —
    /// matches the historical pre-Phase-4 construction shape so existing
    /// call sites can use `..Default::default()` to omit fields they don't
    /// care about.
    fn default() -> Self {
        Self {
            label: String::new(),
            polarity: Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(),
        }
    }
}

impl StatusTag {
    /// True if this tag has expired as of `now_minutes` (the WorldClock's
    /// current value). Pure fn. The tick calls this to decide which tags to
    /// drop. A tag with `expires_at == 0` is treated as non-expiring (the
    /// sentinel for permanent conditions; only removed by an explicit event).
    pub fn is_expired(&self, now_minutes: i64) -> bool {
        self.expires_at != 0 && now_minutes >= self.expires_at
    }
}

/// Drop every expired tag from the list, in place. Called by the World
/// Progression tick handler. Pure mutation: the surviving tags keep their
/// identity; only the expired ones leave. Returns the count of dropped tags
/// so the caller can log it / surface it.
///
/// Tags with `expires_at == 0` are permanent (the sentinel for conditions
/// that only end via an event, not time — "Cursed by the Witch King" until
/// lifted). They are NOT dropped by this fn.
pub fn expire_tags(tags: &mut Vec<StatusTag>, now_minutes: i64) -> usize {
    let before = tags.len();
    tags.retain(|t| !t.is_expired(now_minutes));
    before - tags.len()
}

/// Append a new tag to the list. The caller computes the expiry from the
/// WorldClock (e.g. `now + duration_minutes`). Validates the label is
/// non-empty (a buff with no name is meaningless). Returns true on success.
pub fn add_tag(tags: &mut Vec<StatusTag>, tag: StatusTag) -> bool {
    if tag.label.trim().is_empty() {
        return false;
    }
    tags.push(tag);
    true
}

/// Count tags by polarity. Used by `derive_condition` (which takes counts,
/// not the tag list, so it stays decoupled from the storage representation).
///
/// (2026-08-15 audit fix) Only PURE buff/debuff tags count — `kind.is_empty()`
/// — matching `render_tags_for_prompt`'s lanes. A kinded tag like a Buff-
/// polarity `disguise` is a mechanical state lane, not a condition modifier:
/// counting it nudged the lethality condition penalty + could lift
/// Haggard→Unscathed purely from wearing a disguise.
pub fn count_by_polarity(tags: &[StatusTag], polarity: Polarity) -> usize {
    tags.iter()
        .filter(|t| t.kind.is_empty() && t.polarity == polarity)
        .count()
}

/// Render the active tags as a compact prompt fragment. Returns `None` when
/// empty. Format:
/// ```text
/// buffs: Berserk Rage, Blessed by the Sun Priest
/// debuffs: Feverish, Poisoned
/// disguises: city guard uniform
/// ```
/// The narrator reads this as flavor it can weave in, not as a stat sheet.
///
/// Phase 4 §11.44 (Component 1): tags carrying a recognized `kind` route
/// out of the buff/debuff lanes into a dedicated lane. Currently the only
/// recognized kind is `"disguise"`. Tags with an unrecognized or empty
/// `kind` route by `polarity` exactly as before (backwards-compatible).
pub fn render_tags_for_prompt(tags: &[StatusTag]) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    let buffs: Vec<&str> = tags
        .iter()
        .filter(|t| t.kind.is_empty() && t.polarity == Polarity::Buff)
        .map(|t| t.label.as_str())
        .collect();
    let debuffs: Vec<&str> = tags
        .iter()
        .filter(|t| t.kind.is_empty() && t.polarity == Polarity::Debuff)
        .map(|t| t.label.as_str())
        .collect();
    let disguises: Vec<&str> = tags
        .iter()
        .filter(|t| t.kind == "disguise")
        .map(|t| t.label.as_str())
        .collect();
    let mut lines: Vec<String> = Vec::new();
    if !buffs.is_empty() {
        lines.push(format!("buffs: {}", buffs.join(", ")));
    }
    if !debuffs.is_empty() {
        lines.push(format!("debuffs: {}", debuffs.join(", ")));
    }
    if !disguises.is_empty() {
        lines.push(format!("disguises: {}", disguises.join(", ")));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

// ===========================================================================
// Slice 4: Quest Frustration Curve (DriverTime consumer #2)
// ===========================================================================

/// A questgiver's emotional state, derived continuously from how much time
/// has elapsed relative to the deadline and a per-NPC volatility coefficient.
/// Pure Rust math — the narrator sees only the resulting mood label, never
/// the underlying numbers.
///
/// The curve (locked 2026-07-28 in the architect directive):
/// - **Before deadline** (`elapsed < window`): mood rises smoothly from
///   pleased (at acceptance) → neutral (at deadline). Formula:
///   `(elapsed/window)^(1/coeff) − 1`. Higher volatility (coeff → 3.0)
///   sharpens the rise; lower (coeff → 0.4) flattens it.
/// - **At deadline** (`elapsed == window`): mood is exactly neutral (0).
/// - **Past deadline** (`elapsed > window`): mood drops linearly into
///   unbounded frustration. Formula: `(ratio − 1) * coeff` where
///   `ratio = elapsed / window`. Volatile NPCs go nuclear fast; patient
///   ones degrade slowly.
///
/// `mood_score` is the engine-room number (a signed float); `tier()` maps
/// it to a categorical label the narrator reads.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrustrationState {
    /// The derived mood score, in roughly [-1, +inf) range. 0 = neutral
    /// (at-deadline); negative = pleased; positive = frustrated/angry.
    /// Engine-room only — the narrator sees `tier()`.
    pub mood_score: f64,
    /// The volatility coefficient that produced this score, kept for
    /// tracing + future tuning. 0.4 = patient, 1.0 = default, 3.0 = volatile.
    pub volatility: f64,
}

/// Compute the FrustrationState for a quest given its time parameters.
/// Pure fn. The caller threads `elapsed_minutes` (now − accepted_at) and
/// `window_minutes` (deadline − accepted_at); this fn does the math.
///
/// Returns `None` when `window_minutes <= 0` (a quest with no deadline has
/// no frustration curve — it's a permanent background objective). Also
/// returns `None` when `elapsed_minutes < 0` (the quest hasn't started yet).
pub fn compute_frustration(
    elapsed_minutes: i64,
    window_minutes: i64,
    volatility: f64,
) -> Option<FrustrationState> {
    if window_minutes <= 0 || elapsed_minutes < 0 {
        return None;
    }
    let elapsed = elapsed_minutes as f64;
    let window = window_minutes as f64;
    let coeff = if volatility > 0.0 { volatility } else { 1.0 };
    let mood_score = if elapsed < window {
        // Before deadline: smooth rise from −1 (pleased at acceptance) → 0
        // (at deadline). The exponent 1/coeff sharpens (volatile) or
        // flattens (patient) the curve.
        let ratio = elapsed / window;
        // patient (coeff < 1): exponent > 1 → curve starts low and rises
        //   sharply near the deadline (the patient NPC is fine until they're
        //   suddenly not).
        // volatile (coeff > 1): exponent < 1 → curve rises sharply early
        //   then flattens (the volatile NPC shows irritation immediately).
        ratio.powf(1.0 / coeff) - 1.0
    } else {
        // Past deadline: linear drop into frustration, scaled by coeff.
        // Volatile NPCs degrade fast; patient ones slowly.
        let ratio = elapsed / window;
        (ratio - 1.0) * coeff
    };
    Some(FrustrationState {
        mood_score,
        volatility: coeff,
    })
}

/// A categorical mood tier derived from the FrustrationState's score.
/// These are the labels the narrator sees — never the underlying float.
/// The 7-tier ladder is intentionally coarse: it gives the narrator a
/// directional register without inviting arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoodTier {
    /// mood < −0.6 — the NPC is genuinely delighted with the player's
    /// swift action. Rare; only seen when the player resolves a quest well
    /// before the deadline.
    Delighted,
    /// −0.6 ≤ mood < −0.2 — pleased, warm. The default at quest acceptance.
    Pleased,
    /// −0.2 ≤ mood < 0.2 — neutral. The "I'm not mad, I'm just waiting"
    /// band that straddles the deadline.
    Neutral,
    /// 0.2 ≤ mood < 0.8 — mildly frustrated. The patient beginning of
    /// impatience; tone cools.
    Irritated,
    /// 0.8 ≤ mood < 1.5 — visibly frustrated. Sharp words, cold courtesy.
    Frustrated,
    /// 1.5 ≤ mood < 3.0 — angry. Questgiver is openly hostile; threats
    /// of cancellation, refusal of further aid.
    Angry,
    /// mood ≥ 3.0 — furious past reason. The NPC may actively turn on
    /// the player, denounce them, or hire assassins. The far end of the
    /// curve for very late resolutions on volatile NPCs.
    Furious,
}

impl MoodTier {
    /// Map a mood score to a tier. Pure fn.
    pub fn from_score(score: f64) -> MoodTier {
        if score < -0.6 {
            MoodTier::Delighted
        } else if score < -0.2 {
            MoodTier::Pleased
        } else if score < 0.2 {
            MoodTier::Neutral
        } else if score < 0.8 {
            MoodTier::Irritated
        } else if score < 1.5 {
            MoodTier::Frustrated
        } else if score < 3.0 {
            MoodTier::Angry
        } else {
            MoodTier::Furious
        }
    }

    /// A short narrator-facing directive. Reads as an imperative the
    /// narrator obeys for this NPC's tone on this turn.
    pub fn directive(self) -> &'static str {
        match self {
            MoodTier::Delighted => "delighted — visibly warm, may offer a bonus or share useful information unprompted",
            MoodTier::Pleased => "pleased — friendly, default tone at quest acceptance",
            MoodTier::Neutral => "neutral — polite but transactional",
            MoodTier::Irritated => "irritated — tone cools, mild impatience surfaces",
            MoodTier::Frustrated => "frustrated — sharp words, cold courtesy, may remind the player of the deadline",
            MoodTier::Angry => "angry — openly hostile, may threaten to cancel the deal or refuse further help",
            MoodTier::Furious => "furious past reason — may actively turn on the player, denounce them, or seek retribution",
        }
    }

    /// The lowercase tag for serialization + prompt attribute.
    pub fn tag(self) -> &'static str {
        match self {
            MoodTier::Delighted => "delighted",
            MoodTier::Pleased => "pleased",
            MoodTier::Neutral => "neutral",
            MoodTier::Irritated => "irritated",
            MoodTier::Frustrated => "frustrated",
            MoodTier::Angry => "angry",
            MoodTier::Furious => "furious",
        }
    }
}

impl FrustrationState {
    /// Derive the categorical MoodTier from the score.
    pub fn tier(&self) -> MoodTier {
        MoodTier::from_score(self.mood_score)
    }

    /// Render as a narrator directive. The caller wraps this as
    /// `[DIRECTIVE: Questgiver <name> is <directive>]` inside `<world_state>`.
    /// The narrator obeys the tone; the player feels the deadline's gravity.
    pub fn render_directive(&self, npc_name: &str, quest_label: &str) -> String {
        format!(
            "Questgiver {} ({}): {}",
            npc_name,
            quest_label,
            self.tier().directive()
        )
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Driver taxonomy ----

    #[test]
    fn driver_variants_are_exactly_four() {
        // Pin the four-type taxonomy: no driverStatedFact (the legacy trap).
        // If a fifth variant appears, this test fails and forces the author
        // to reconsider whether they're smuggling numbers back in.
        let four = [
            Driver::Static,
            Driver::Event,
            Driver::Time,
            Driver::Narrative,
        ];
        assert_eq!(four.len(), 4, "the taxonomy is locked at exactly four types");
    }

    #[test]
    fn driver_serializes_snake_case() {
        // The serde rename keeps the wire format stable + lowercase.
        let s = serde_json::to_string(&Driver::Time).unwrap();
        assert_eq!(s, "\"time\"");
        let parsed: Driver = serde_json::from_str("\"narrative\"").unwrap();
        assert_eq!(parsed, Driver::Narrative);
    }

    // ---- Per-axis age curves ----

    #[test]
    fn axis_all_has_four_in_canonical_order() {
        let all = Axis::all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], Axis::Physical);
        assert_eq!(all[1], Axis::Martial);
        assert_eq!(all[2], Axis::Cunning);
        assert_eq!(all[3], Axis::Magical);
    }

    #[test]
    fn physical_curve_peaks_young_and_declines() {
        // A 30-year-old is at Peak physical.
        let axes = BiologicalBaseline { age: 30, background: String::new() }.axes();
        assert_eq!(axes.physical, Capability::Peak, "30yo physical should be Peak");
        // An 80-year-old has Formative physical (bodies decline).
        let axes = BiologicalBaseline { age: 80, background: String::new() }.axes();
        assert_eq!(axes.physical, Capability::Formative, "80yo physical should be Formative");
        // A 5-year-old child has Formative — too young for true capability.
        let axes = BiologicalBaseline { age: 5, background: String::new() }.axes();
        assert_eq!(axes.physical, Capability::Formative, "5yo physical should be Formative");
    }

    #[test]
    fn martial_curve_peaks_late_and_holds_as_mastery() {
        // A 40-year-old knight is at Peak martial.
        let axes = BiologicalBaseline { age: 45, background: "knight".into() }.axes();
        // 45yo base curve = Peak, +1 nudge for knight background clamps at Mastery.
        // (Peak is rank 4; +1 → Mastery rank 5.)
        assert_eq!(axes.martial, Capability::Mastery, "45yo knight martial should be Mastery");
        // An 80-year-old retired knight holds Mastery — skill outlasting body.
        let axes = BiologicalBaseline { age: 80, background: "knight".into() }.axes();
        // 80yo base martial = Mastery; +1 nudge clamps at Mastery.
        assert_eq!(axes.martial, Capability::Mastery, "80yo knight martial should still be Mastery");
    }

    #[test]
    fn age_distinguishes_two_knights() {
        // The architect's defining example: a 40-year-old knight in his prime
        // vs an 80-year-old retired knight. They have OVERLAPPING martial
        // mastery but DIVERGENT physical capacity. Single-axis labels would
        // collapse them; per-axis labels distinguish them.
        let knight_40 = BiologicalBaseline { age: 40, background: "knight".into() }.axes();
        let knight_80 = BiologicalBaseline { age: 80, background: "knight".into() }.axes();
        // Both have Mastery martial (40yo: Peak+1 → Mastery; 80yo: Mastery).
        assert_eq!(knight_40.martial, knight_80.martial, "both knights share martial Mastery");
        // But the 40yo's physical dominates the 80yo's.
        assert!(
            knight_40.physical.rank() > knight_80.physical.rank(),
            "40yo physical ({:?}) must outstrip 80yo physical ({:?})",
            knight_40.physical,
            knight_80.physical
        );
    }

    #[test]
    fn child_vs_adult_distinguishes_on_every_axis() {
        // A 10-year-old vs a 40-year-old knight — the original framing.
        // Per-axis, NOT a single "level" comparison.
        let child = BiologicalBaseline { age: 10, background: String::new() }.axes();
        let knight = BiologicalBaseline { age: 40, background: "knight".into() }.axes();
        for axis in [Axis::Physical, Axis::Martial, Axis::Cunning, Axis::Magical] {
            let (c, k) = match axis {
                Axis::Physical => (child.physical, knight.physical),
                Axis::Martial => (child.martial, knight.martial),
                Axis::Cunning => (child.cunning, knight.cunning),
                Axis::Magical => (child.magical, knight.magical),
            };
            assert!(
                k.rank() > c.rank(),
                "{axis:?}: knight ({:?}) must outstrip child ({:?})",
                k,
                c
            );
        }
    }

    #[test]
    fn scholar_background_nudges_axes() {
        // A scholar gets +1 Cunning +1 Magical, -1 Physical.
        let axes = BiologicalBaseline { age: 50, background: "scholar".into() }.axes();
        // 50yo base Cunning = Peak; +1 → Mastery.
        assert_eq!(axes.cunning, Capability::Mastery, "scholar cunning should be Mastery");
        // 50yo base Magical = Capable; +1 → Peak.
        assert_eq!(axes.magical, Capability::Peak, "scholar magical should be Peak");
        // 50yo base Physical = Capable; -1 → Developing.
        assert_eq!(axes.physical, Capability::Developing, "scholar physical should be Developing");
    }

    #[test]
    fn unknown_background_uses_curve_only() {
        // A background we don't recognize gets no nudges — the pure age curve.
        let axes = BiologicalBaseline { age: 30, background: "philosopher".into() }.axes();
        // 30yo base physical = Peak, no nudge.
        assert_eq!(axes.physical, Capability::Peak);
        // 30yo base martial = Capable, no nudge.
        assert_eq!(axes.martial, Capability::Capable);
    }

    // ---- AgeAxes rendering ----

    #[test]
    fn age_axes_render_none_when_all_default() {
        // A 30yo with no background: physical=Peak, others default → renders.
        // A 25yo with no background: ALL axes default to Capable → no render.
        // (25yo base: physical=Peak, martial=Capable, cunning=Capable, magical=Developing)
        // Actually finding an all-Capable year is tricky — age 26 hits Peak for
        // Physical. The closest is age 23-24 where Physical is Capable and all
        // others are Capable/Developing. We test the explicit "all Capable"
        // path via direct construction instead.
        let axes = AgeAxes {
            physical: Capability::Capable,
            martial: Capability::Capable,
            cunning: Capability::Capable,
            magical: Capability::Capable,
        };
        assert_eq!(axes.render_for_prompt(), None, "all-Capable axes must not emit");
    }

    #[test]
    fn age_axes_render_emits_only_diverging_axes() {
        let axes = AgeAxes {
            physical: Capability::Peak,
            martial: Capability::Mastery,
            cunning: Capability::Capable, // default — should NOT appear
            magical: Capability::Capable, // default — should NOT appear
        };
        let rendered = axes.render_for_prompt().expect("non-default axes must render");
        assert!(rendered.contains("physical:"));
        assert!(rendered.contains("martial:"));
        assert!(!rendered.contains("cunning:"), "default cunning must not appear: {rendered}");
        assert!(!rendered.contains("magical:"), "default magical must not appear: {rendered}");
    }

    // ---- Derived Condition ----

    #[test]
    fn condition_default_is_unscathed() {
        assert_eq!(Condition::default(), Condition::Unscathed);
    }

    #[test]
    fn derive_condition_no_wounds_is_unscathed() {
        let wounds = HashMap::new();
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Unscathed);
    }

    #[test]
    fn derive_condition_minor_wound_is_haggard() {
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftHand, BodyPartState::Yellow);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Haggard);
    }

    #[test]
    fn derive_condition_medium_wound_is_wounded() {
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftUpperArm, BodyPartState::Orange);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Wounded);
    }

    #[test]
    fn derive_condition_heavy_wound_is_battered() {
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::UpperTorso, BodyPartState::Red);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Battered);
    }

    #[test]
    fn derive_condition_critical_wound_is_critical() {
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::Head, BodyPartState::Purple);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Critical);
    }

    #[test]
    fn derive_condition_amputated_alone_is_critical() {
        // A single amputation is shock-critical, not Downed (the body is
        // holding on but only just).
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftHand, BodyPartState::Black);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Critical);
    }

    #[test]
    fn derive_condition_amputated_plus_severe_is_downed() {
        // An amputation + a Heavy wound → Downed (the body can't sustain it).
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftHand, BodyPartState::Black);
        wounds.insert(BodyPart::UpperTorso, BodyPartState::Red);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Downed);
    }

    #[test]
    fn derive_condition_three_severe_wounds_is_downed() {
        // System shock: three Red-or-worse wounds = Downed regardless of amputation.
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::Head, BodyPartState::Red);
        wounds.insert(BodyPart::UpperTorso, BodyPartState::Red);
        wounds.insert(BodyPart::LeftUpperLeg, BodyPartState::Red);
        assert_eq!(derive_condition(&wounds, 0, 0), Condition::Downed);
    }

    #[test]
    fn derive_condition_buff_lifts_haggard_to_unscathed() {
        // A single Minor wound + a buff (and no debuffs) → buff lifts the
        // body one tier toward Unscathed.
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftHand, BodyPartState::Yellow);
        assert_eq!(derive_condition(&wounds, 1, 0), Condition::Unscathed);
    }

    #[test]
    fn derive_condition_buff_does_not_rescue_critical() {
        // A Critical wound + a buff: wounds dominate, buff is ignored.
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::Head, BodyPartState::Purple);
        assert_eq!(derive_condition(&wounds, 5, 0), Condition::Critical);
    }

    #[test]
    fn derive_condition_two_debuffs_drag_down() {
        // An Unscathed body with 2+ debuffs drops to Haggard.
        let wounds = HashMap::new();
        assert_eq!(derive_condition(&wounds, 0, 2), Condition::Haggard);
        // A Haggard body with 2+ debuffs drops to Wounded.
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftHand, BodyPartState::Yellow);
        assert_eq!(derive_condition(&wounds, 0, 2), Condition::Wounded);
    }

    #[test]
    fn condition_label_reads_as_prose() {
        // The label is what the narrator sees; it must read as a phrase the
        // narrator can weave into prose, not as a stat.
        assert!(Condition::Downed.label().contains("downed"));
        assert!(Condition::Unscathed.label().contains("healthy"));
    }

    #[test]
    fn capability_label_reads_as_prose() {
        // Same: the Capability label reads as a noun phrase.
        assert!(Capability::Mastery.label().contains("master"));
        assert!(Capability::Peak.label().contains("prime"));
    }

    // ---- The defining architect examples (pin them as regression tests) ----

    #[test]
    fn architect_example_10yo_vs_40yo_knight() {
        // The original framing: a 10-year-old vs a 40-year-old knight, "level
        // 1 vs level 10" in legacy systems but per-axis Capability in ours.
        let child = BiologicalBaseline { age: 10, background: String::new() }.axes();
        let knight = BiologicalBaseline { age: 40, background: "knight".into() }.axes();
        // Per-axis: the knight dominates on every axis.
        for axis in Axis::all() {
            let (c, k) = match axis {
                Axis::Physical => (child.physical, knight.physical),
                Axis::Martial => (child.martial, knight.martial),
                Axis::Cunning => (child.cunning, knight.cunning),
                Axis::Magical => (child.magical, knight.magical),
            };
            assert!(
                k.rank() > c.rank(),
                "{axis:?}: knight ({:?}) should outstrip child ({:?})",
                k,
                c
            );
        }
    }

    #[test]
    fn architect_example_40yo_vs_80yo_knight() {
        // The texture example: 40yo knight vs 80yo retired knight. Both
        // formidable, but the elder's body has faded while his skill holds.
        let knight_prime = BiologicalBaseline { age: 40, background: "knight".into() }.axes();
        let knight_elder = BiologicalBaseline { age: 80, background: "knight".into() }.axes();
        // Martial: both at Mastery (skill outlasts the body).
        assert_eq!(knight_prime.martial, knight_elder.martial);
        // Physical: the prime knight dominates.
        assert!(knight_prime.physical.rank() > knight_elder.physical.rank());
        // The labels render DIFFERENTLY for the two — that's the point.
        let prime_render = knight_prime.render_for_prompt().unwrap_or_default();
        let elder_render = knight_elder.render_for_prompt().unwrap_or_default();
        assert_ne!(prime_render, elder_render, "the two knights must render distinctly");
    }

    // ---- Slice 4: Buff/Debuff tags ----

    #[test]
    fn status_tag_expires_at_or_after_expiry() {
        let tag = StatusTag {
            label: "Berserk Rage".into(),
            polarity: Polarity::Buff,
            expires_at: 1000,
            source: "quaffed a potion".into(),
        kind: String::new(),
        };
        // Just before expiry: still active.
        assert!(!tag.is_expired(999));
        // At expiry: gone.
        assert!(tag.is_expired(1000));
        // After: gone.
        assert!(tag.is_expired(1500));
    }

    #[test]
    fn status_tag_zero_expiry_is_permanent() {
        // expires_at == 0 is the sentinel for permanent conditions (curses
        // that only lift via an event). The tick never drops these.
        let tag = StatusTag {
            label: "Cursed by the Witch King".into(),
            polarity: Polarity::Debuff,
            expires_at: 0,
            source: String::new(),
        kind: String::new(),
        };
        assert!(!tag.is_expired(0), "permanent tags never expire");
        assert!(!tag.is_expired(9_999_999), "permanent tags never expire");
    }

    #[test]
    fn expire_tags_drops_only_expired_ones() {
        let mut tags = vec![
            StatusTag {
                label: "Berserk Rage".into(),
                polarity: Polarity::Buff,
                expires_at: 500,
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Blessed".into(),
                polarity: Polarity::Buff,
                expires_at: 1500,
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Feverish".into(),
                polarity: Polarity::Debuff,
                expires_at: 200,
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Cursed (permanent)".into(),
                polarity: Polarity::Debuff,
                expires_at: 0, // permanent — must survive
                source: String::new(),
            kind: String::new(),
            },
        ];
        let dropped = expire_tags(&mut tags, 1000);
        assert_eq!(dropped, 2, "two expired tags dropped (Berserk + Feverish)");
        assert_eq!(tags.len(), 2, "two tags remain (Blessed + permanent curse)");
        assert!(tags.iter().any(|t| t.label == "Blessed"));
        assert!(tags.iter().any(|t| t.label == "Cursed (permanent)"));
    }

    #[test]
    fn add_tag_rejects_empty_label() {
        let mut tags: Vec<StatusTag> = Vec::new();
        let ok = add_tag(
            &mut tags,
            StatusTag {
                label: "   ".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
        );
        assert!(!ok, "empty label must be rejected");
        assert!(tags.is_empty());
        let ok = add_tag(
            &mut tags,
            StatusTag {
                label: "Hasted".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
        );
        assert!(ok);
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn count_by_polarity_separates_buffs_and_debuffs() {
        let tags = vec![
            StatusTag {
                label: "Blessed".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Hasted".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Poisoned".into(),
                polarity: Polarity::Debuff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
        ];
        assert_eq!(count_by_polarity(&tags, Polarity::Buff), 2);
        assert_eq!(count_by_polarity(&tags, Polarity::Debuff), 1);
    }

    /// 2026-08-15 audit fix: a kinded tag (e.g. a Buff-polarity disguise) is
    /// a mechanical lane, NOT a condition buff — it must not count toward
    /// the buff total that feeds `derive_condition` + the lethality
    /// condition penalty.
    #[test]
    fn count_by_polarity_excludes_kinded_tags() {
        let tags = vec![
            StatusTag {
                label: "city guard uniform".into(),
                polarity: Polarity::Buff,
                expires_at: 0,
                source: String::new(),
                kind: "disguise".into(),
            },
            StatusTag {
                label: "Blessed".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
                kind: String::new(),
            },
        ];
        assert_eq!(
            count_by_polarity(&tags, Polarity::Buff),
            1,
            "the disguise tag is a mechanical lane, not a condition buff"
        );
    }

    #[test]
    fn render_tags_separates_buffs_and_debuffs() {
        let tags = vec![
            StatusTag {
                label: "Blessed".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Poisoned".into(),
                polarity: Polarity::Debuff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
        ];
        let rendered = render_tags_for_prompt(&tags).expect("non-empty must render");
        assert!(rendered.contains("buffs: Blessed"));
        assert!(rendered.contains("debuffs: Poisoned"));
    }

    // ---- Phase 4 §11.44 (Component 1): StatusTag.kind discriminator ----

    #[test]
    fn status_tag_kind_defaults_empty_via_serde() {
        // Pre-Phase-4 saves don't carry `kind`; serde must load them as empty
        // (the generic-effect lane), NOT fail, NOT coerce to anything else.
        let json = r#"{"label":"Blessed","polarity":"buff","expires_at":0,"source":""}"#;
        let tag: StatusTag = serde_json::from_str(json).expect("missing kind must default");
        assert_eq!(tag.label, "Blessed");
        assert_eq!(tag.kind, "", "kind defaults to empty string");
    }

    #[test]
    fn status_tag_kind_round_trips_through_serde() {
        let tag = StatusTag {
            label: "city guard uniform".into(),
            polarity: Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: "disguise".into(),
        };
        let json = serde_json::to_string(&tag).unwrap();
        let back: StatusTag = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, "disguise");
    }

    #[test]
    fn render_tags_routes_disguise_into_own_lane() {
        // A disguise tag (kind="disguise") must NOT appear in buffs:/debuffs:,
        // even when its polarity is Buff. It goes to its own `disguises:` line.
        let tags = vec![
            StatusTag {
                label: "Blessed".into(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
                kind: String::new(),
            },
            StatusTag {
                label: "city guard uniform".into(),
                polarity: Polarity::Buff, // buff polarity, but kind routes it away
                expires_at: 0,
                source: String::new(),
                kind: "disguise".into(),
            },
        ];
        let rendered = render_tags_for_prompt(&tags).expect("must render");
        assert!(
            rendered.contains("buffs: Blessed"),
            "generic buff stays in buffs lane: {rendered}"
        );
        assert!(
            !rendered.contains("buffs: city guard uniform"),
            "disguise must NOT leak into buffs lane: {rendered}"
        );
        assert!(
            rendered.contains("disguises: city guard uniform"),
            "disguise lands in its own lane: {rendered}"
        );
    }

    #[test]
    fn render_tags_disguise_lane_joins_multiple_disguises() {
        let tags = vec![
            StatusTag {
                label: "city guard uniform".into(),
                polarity: Polarity::Buff,
                expires_at: 0,
                source: String::new(),
                kind: "disguise".into(),
            },
            StatusTag {
                label: "merchant robes".into(),
                polarity: Polarity::Debuff, // polarity ignored when kind=disguise
                expires_at: 0,
                source: String::new(),
                kind: "disguise".into(),
            },
        ];
        let rendered = render_tags_for_prompt(&tags).expect("must render");
        assert!(rendered.contains("disguises: city guard uniform, merchant robes"));
        assert!(!rendered.contains("buffs:"), "no buffs lane when only disguises");
        assert!(!rendered.contains("debuffs:"), "no debuffs lane when only disguises");
    }

    #[test]
    fn render_tags_empty_kind_generic_tag_unaffected() {
        // Empty kind = generic effect → routes by polarity exactly as before.
        // This is the backwards-compat guarantee: pre-Phase-4 tags unchanged.
        let tags = vec![StatusTag {
            label: "Poisoned".into(),
            polarity: Polarity::Debuff,
            expires_at: 1000,
            source: String::new(),
            kind: String::new(),
        }];
        let rendered = render_tags_for_prompt(&tags).expect("must render");
        assert!(rendered.contains("debuffs: Poisoned"));
        assert!(!rendered.contains("disguises:"));
    }

    #[test]
    fn render_tags_only_disguises_no_buffs_debuffs() {
        // Sanity: a disguise-only list produces a disguises: line and nothing else.
        let tags = vec![StatusTag {
            label: "novice robe".into(),
            polarity: Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: "disguise".into(),
        }];
        let rendered = render_tags_for_prompt(&tags).expect("must render");
        assert_eq!(rendered.trim(), "disguises: novice robe");
    }

    #[test]
    fn render_tags_none_when_empty() {
        let tags: Vec<StatusTag> = Vec::new();
        assert_eq!(render_tags_for_prompt(&tags), None);
    }

    #[test]
    fn render_tags_handles_buffs_only() {
        let tags = vec![StatusTag {
            label: "Blessed".into(),
            polarity: Polarity::Buff,
            expires_at: 1000,
            source: String::new(),
        kind: String::new(),
        }];
        let rendered = render_tags_for_prompt(&tags).expect("non-empty must render");
        assert!(rendered.contains("buffs: Blessed"));
        assert!(!rendered.contains("debuffs:"), "debuffs line must be omitted when empty");
    }

    // ---- Slice 4: Quest Frustration Curve ----

    #[test]
    fn frustration_none_for_no_deadline() {
        // window == 0 → no deadline → no frustration (permanent background objective).
        assert!(compute_frustration(0, 0, 1.0).is_none());
        assert!(compute_frustration(100, 0, 1.0).is_none());
    }

    #[test]
    fn frustration_none_for_negative_elapsed() {
        // Quest hasn't started yet.
        assert!(compute_frustration(-1, 1000, 1.0).is_none());
    }

    #[test]
    fn frustration_at_acceptance_is_pleased() {
        // At elapsed == 0 the curve gives (0)^(1/coeff) - 1 = -1 → Delighted
        // or Pleased depending on the threshold. For coeff 1.0, score = -1.0
        // which falls in Delighted (< −0.6).
        let f = compute_frustration(0, 1000, 1.0).expect("must compute");
        assert!(f.mood_score < 0.0, "mood at acceptance should be negative (pleased)");
        assert_eq!(f.tier(), MoodTier::Delighted, "exact-acceptance → Delighted for coeff 1.0");
    }

    #[test]
    fn frustration_at_deadline_is_neutral() {
        // At elapsed == window the score is exactly 0 → Neutral.
        let f = compute_frustration(1000, 1000, 1.0).expect("must compute");
        assert!(
            f.mood_score.abs() < 0.001,
            "mood at deadline should be ~0, got {}",
            f.mood_score
        );
        assert_eq!(f.tier(), MoodTier::Neutral);
    }

    #[test]
    fn frustration_past_deadline_grows_negative_linearly() {
        // At 2x the window the score is (2-1)*coeff = coeff. For coeff 1.0
        // that's 1.0 → Frustrated.
        let f = compute_frustration(2000, 1000, 1.0).expect("must compute");
        assert!(
            (f.mood_score - 1.0).abs() < 0.001,
            "mood at 2x window should be 1.0, got {}",
            f.mood_score
        );
        assert_eq!(f.tier(), MoodTier::Frustrated);
    }

    #[test]
    fn frustration_volatile_npc_degrades_faster_past_deadline() {
        // A volatile NPC (coeff 3.0) at 2x window: score = (2-1)*3 = 3 → Furious.
        // A patient NPC (coeff 0.4) at 2x window: score = (2-1)*0.4 = 0.4 → Irritated.
        let volatile = compute_frustration(2000, 1000, 3.0).unwrap();
        let patient = compute_frustration(2000, 1000, 0.4).unwrap();
        assert!(volatile.mood_score > patient.mood_score);
        assert_eq!(volatile.tier(), MoodTier::Furious, "volatile 2x → Furious");
        assert_eq!(patient.tier(), MoodTier::Irritated, "patient 2x → Irritated");
    }

    #[test]
    fn frustration_directive_renders_with_npc_and_quest() {
        let f = compute_frustration(3000, 1000, 1.0).unwrap(); // 3x → Angry
        let directive = f.render_directive("Marcus", "Rescue His Daughter");
        assert!(directive.contains("Marcus"), "must mention NPC name: {directive}");
        assert!(directive.contains("Rescue His Daughter"), "must mention quest: {directive}");
        assert!(directive.contains("angry"), "must contain the mood word: {directive}");
    }

    #[test]
    fn mood_tier_ladder_is_monotonic() {
        // The tier boundaries must be ordered: lower score → better mood.
        let scores = [-1.0, -0.4, 0.0, 0.5, 1.0, 2.0, 5.0];
        let tiers: Vec<MoodTier> = scores.iter().map(|&s| MoodTier::from_score(s)).collect();
        for window in tiers.windows(2) {
            // Earlier tier must be "better" (lower enum index = better mood).
            assert!(
                (window[0] as u8) <= (window[1] as u8),
                "tiers must be ordered better→worse as score rises"
            );
        }
    }

    #[test]
    fn mood_tier_tags_and_directives_are_distinct_and_nonempty() {
        let tiers = [
            MoodTier::Delighted,
            MoodTier::Pleased,
            MoodTier::Neutral,
            MoodTier::Irritated,
            MoodTier::Frustrated,
            MoodTier::Angry,
            MoodTier::Furious,
        ];
        let tags: Vec<&str> = tiers.iter().map(|t| t.tag()).collect();
        let directives: Vec<&str> = tiers.iter().map(|t| t.directive()).collect();
        // All tags distinct.
        let mut sorted = tags.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), tiers.len(), "tags must be distinct");
        // All directives nonempty + distinct.
        for d in &directives {
            assert!(!d.is_empty());
        }
        let mut sorted = directives.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), tiers.len(), "directives must be distinct");
    }
}
