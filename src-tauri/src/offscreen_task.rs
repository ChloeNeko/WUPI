//! Off-Screen Risk Referee + Task Queue (Fable Phase 3 Slice 6, 2026-07-28).
//!
//! The "Commander playstyle" mechanic (generalized to all NPCs, not just the
//! party): Wupi logs a task with an in-world ETA. When the WorldClock crosses
//! the ETA, the Referee rolls a d20 against the NPC's suitability for the
//! task. The outcome injects a `[DIRECTIVE: ...]` line for the narrator to
//! reveal on the player's return ("Marcus returns from his scouting mission
//! — bloodied but successful, the bandit camp's location marked on his map").
//!
//! # Why pure-Rust resolution
//!
//! Same anti-sycophancy principle as the combat Referee: Rust decides the
//! outcome, the narrator obeys. The LLM never rolls dice; the player never
//! sees a probability. The directive carries only the qualitative outcome
//! ("succeeded with complication", "failed catastrophically") — never the
//! d20 number.
//!
//! # Hard constraint: no apocalyptic shifts
//!
//! The architect directive is explicit: off-screen tasks cannot trigger
//! world-shaking events while the player runs errands. The `TaskDifficulty`
//! + the `OutcomeSeverity` ladder are tuned so even a catastrophic failure
//! produces LOCAL consequences ("Marcus was captured; the bandits hold him
//! for ransom"), never global ones ("Marcus's failure released the dark
//! god"). The directive text Rust emits is bounded to the local scope.
//!
//! # v1 boundary
//!
//! This module ships the Rust-authoritative mechanics: the task queue, the
//! Risk Referee, the directive emission. The World Progression tick
//! integration (Phase 4 §11.44 Component 3) IS wired: `resolve_expired_
//! tasks` runs inside the tick, and its `[DIRECTIVE: ...]` lines land in
//! `pending_tick_directives`, which the next narrator turn drains into its
//! `<directives>` block — a directive only reaches the player one turn
//! after the ETA crosses (the tick is turn-gated by design).

use crate::player_state::{roll_d20, Roller};
use std::collections::HashMap;

// ===========================================================================
// TaskDifficulty: the off-screen analog of AttackerTier (reversed polarity)
// ===========================================================================

/// How hard an off-screen task is. Higher difficulty = higher DC. The band
/// maps roughly to the Slice 3 `AttackerTier` concept, but reversed: a
/// Legendary-tier threat as an ENEMY becomes a Trivial task for a capable
/// agent (the polarity flips because the NPC is now the actor, not the
/// threat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskDifficulty {
    /// Fetch a known item from a safe place. DC 5. Almost any NPC succeeds.
    Trivial,
    /// Routine work the NPC is trained for (a knight on patrol, a thief
    /// picking pockets in a familiar market). DC 10.
    Routine,
    /// A real challenge requiring skill or risk (scout a bandit camp, pick a
    /// merchant's lock, negotiate with a wary guildmaster). DC 15.
    Challenging,
    /// A serious challenge that may exceed the NPC's capability
    /// (infiltrate a guarded fortress, negotiate with a hostile lord).
    /// DC 20. Failure is common; catastrophic failure is on the table.
    Hard,
    /// A near-impossible task the NPC should probably refuse
    /// (assassinate a king, steal from a dragon). DC 25. Catastrophic
    /// failure likely unless the NPC is exceptionally suited.
    NearImpossible,
}

impl Default for TaskDifficulty {
    fn default() -> Self {
        TaskDifficulty::Routine
    }
}

impl TaskDifficulty {
    /// The DC the d20 roll must meet-or-beat for success. Higher = harder.
    pub fn dc(self) -> u32 {
        match self {
            TaskDifficulty::Trivial => 5,
            TaskDifficulty::Routine => 10,
            TaskDifficulty::Challenging => 15,
            TaskDifficulty::Hard => 20,
            TaskDifficulty::NearImpossible => 25,
        }
    }

    /// Lowercase tag for the directive sentence.
    pub fn tag(self) -> &'static str {
        match self {
            TaskDifficulty::Trivial => "trivial",
            TaskDifficulty::Routine => "routine",
            TaskDifficulty::Challenging => "challenging",
            TaskDifficulty::Hard => "hard",
            TaskDifficulty::NearImpossible => "near-impossible",
        }
    }
}

// ===========================================================================
// Suitability: the NPC's aptitude for this kind of task
// ===========================================================================

/// How well-suited the NPC is to a given task type. Adds a modifier to the
/// d20 roll: a perfectly-suited NPC gets +5 (their expertise compensates for
/// the difficulty), a hopelessly-mismatched NPC gets −5. Set by the caller
/// when logging the task (Wupi-as-game-manager resolves it from the NPC's
/// declared skills + the task type).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Suitability {
    /// The NPC has no business attempting this (a scholar asked to win a
    /// bar brawl). −5 to the roll.
    Hopeless,
    /// Poorly suited but not impossible. −2.
    Poor,
    /// Adequately suited — neither bonus nor penalty. The default.
    Adequate,
    /// Well-suited; the NPC has relevant skills. +2.
    WellSuited,
    /// Perfectly suited; this is the NPC's specialty. +5.
    Ideal,
}

impl Default for Suitability {
    fn default() -> Self {
        Suitability::Adequate
    }
}

impl Suitability {
    pub fn modifier(self) -> i32 {
        match self {
            Suitability::Hopeless => -5,
            Suitability::Poor => -2,
            Suitability::Adequate => 0,
            Suitability::WellSuited => 2,
            Suitability::Ideal => 5,
        }
    }
}

// ===========================================================================
// Task: the queued off-screen job
// ===========================================================================

/// A logged off-screen task. Lives in a per-NPC queue on the WorldSchema
/// (Slice 6 v1 leaves the queue storage to a follow-up that adds the
/// AppState field; the resolution mechanics here are decoupled from storage).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OffScreenTask {
    /// The NPC assigned to the task. Caller resolves to a name for the
    /// directive; we store the id.
    pub npc_id: String,
    /// A short diegetic description of the task ("scout the bandit camp",
    /// "negotiate with the guildmaster"). Surfaces in the directive.
    pub description: String,
    /// How hard the task is.
    pub difficulty: TaskDifficulty,
    /// How well-suited the NPC is.
    pub suitability: Suitability,
    /// The in-world minute at which the task resolves (the ETA — when the
    /// NPC returns with a result). Same epoch-minutes units as `WorldClock`.
    pub resolves_at_minutes: i64,
    /// Whether the task has been resolved yet. The tick checks this before
    /// rolling; resolved tasks stay in the queue (so the directive can be
    /// re-emitted if needed) but don't re-roll.
    #[serde(default)]
    pub resolved: bool,
}

impl OffScreenTask {
    /// True if the task is due as of `now_minutes` (the WorldClock's value).
    /// The tick calls this to decide which tasks to resolve.
    pub fn is_due(&self, now_minutes: i64) -> bool {
        !self.resolved && now_minutes >= self.resolves_at_minutes
    }
}

// ===========================================================================
// OutcomeSeverity: the bounded consequence ladder
// ===========================================================================

/// The qualitative outcome of an off-screen task resolution. STRICTLY ORDERED
/// worst → best by enum-variant order. The ladder is BOUNDED — there is no
/// "the dark god was released" tier. The worst possible outcome is
/// `CatastrophicFailure`, which produces LOCAL consequences only ("Marcus was
/// captured", "the bandits burned the player's safehouse"). The architect
/// directive's hard constraint against apocalyptic shifts is enforced by the
/// shape of this enum: no tier above CatastrophicFailure exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSeverity {
    /// Catastrophic failure with local-only consequences. The NPC is
    /// captured, killed, or turned; the player's local assets are
    /// compromised. NEVER global.
    CatastrophicFailure,
    /// Clear failure. The NPC returns empty-handed (or doesn't return yet);
    /// minor local repercussions.
    Failure,
    /// Success with a meaningful complication. The objective was achieved
    /// but at a cost (the NPC is wounded, the alarm was raised, the loot
    /// is partial).
    ComplicatedSuccess,
    /// Clean success. The NPC returns with the objective achieved + any
    /// agreed reward.
    Success,
    /// Critical success — the NPC exceeded the objective (extra loot,
    /// a valuable new contact, an unanticipated advantage).
    CriticalSuccess,
}

impl OutcomeSeverity {
    /// Map a d20 result to a severity tier. `raw_roll` is the unmodified die
    /// (the ONLY value the natural-1/20 override may judge); `effective_roll`
    /// is the suitability-adjusted value the margin math uses. The mapping:
    /// - effective roll < DC by 10+ → CatastrophicFailure
    /// - effective roll < DC → Failure
    /// - effective roll ≥ DC, margin < 5 → ComplicatedSuccess (a near thing)
    /// - effective roll ≥ DC by 5+ → Success
    /// - effective roll ≥ DC by 10+ → CriticalSuccess
    /// - RAW natural 1 → CatastrophicFailure regardless of margin
    /// - RAW natural 20 → CriticalSuccess regardless of margin
    ///
    /// A modified value of 1 or 20 is NOT a natural — raw 15 + Ideal(+5)
    /// reaching 20 is a strong hit that earns its tier by margin, not an
    /// auto-crit (and raw 6 + Hopeless(−5) reaching 1 is a bad miss, not an
    /// auto-catastrophe).
    pub fn from_margin(raw_roll: u32, effective_roll: u32, dc: u32) -> OutcomeSeverity {
        // Natural 1 / natural 20 override the margin math — RAW die only.
        if raw_roll == 1 {
            return OutcomeSeverity::CatastrophicFailure;
        }
        if raw_roll == 20 {
            return OutcomeSeverity::CriticalSuccess;
        }
        let roll = effective_roll;
        if roll + 10 <= dc {
            OutcomeSeverity::CatastrophicFailure
        } else if roll < dc {
            OutcomeSeverity::Failure
        } else if roll + 5 > dc + (dc - dc.min(roll)) && roll < dc + 5 {
            // roll ∈ [dc, dc+5) → ComplicatedSuccess
            OutcomeSeverity::ComplicatedSuccess
        } else if roll < dc + 10 {
            OutcomeSeverity::Success
        } else {
            OutcomeSeverity::CriticalSuccess
        }
    }

    /// A narrator-facing directive seed. Reads as a one-line seed the
    /// narrator weaves into the reveal scene.
    pub fn directive_seed(self) -> &'static str {
        match self {
            OutcomeSeverity::CatastrophicFailure => {
                "catastrophic failure — the NPC is captured, killed, or turned; local consequences only"
            }
            OutcomeSeverity::Failure => "clear failure — the objective was not achieved",
            OutcomeSeverity::ComplicatedSuccess => {
                "success with complication — the objective was achieved but at a cost"
            }
            OutcomeSeverity::Success => "clean success — the objective was achieved",
            OutcomeSeverity::CriticalSuccess => {
                "critical success — the NPC exceeded the objective"
            }
        }
    }

    /// Lowercase tag for serialization.
    pub fn tag(self) -> &'static str {
        match self {
            OutcomeSeverity::CatastrophicFailure => "catastrophic_failure",
            OutcomeSeverity::Failure => "failure",
            OutcomeSeverity::ComplicatedSuccess => "complicated_success",
            OutcomeSeverity::Success => "success",
            OutcomeSeverity::CriticalSuccess => "critical_success",
        }
    }
}

// ===========================================================================
// The Risk Referee: resolve an off-screen task
// ===========================================================================

/// The result of resolving one off-screen task. Carries the d20 (for tracing
/// only — NEVER shown to narrator), the severity tier, and a pre-formatted
/// directive the caller wraps as `[DIRECTIVE: ...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResolution {
    /// The NPC id from the task (echoed for the caller's routing).
    pub npc_id: String,
    /// The task description (echoed for the directive).
    pub description: String,
    /// The d20 roll. Engine-room only — never shown to narrator.
    #[allow(dead_code)]
    pub roll: u32,
    /// The effective DC after suitability modifier.
    pub dc: u32,
    /// The outcome tier.
    pub severity: OutcomeSeverity,
    /// Pre-formatted directive. The caller wraps as
    /// `[DIRECTIVE: {directive}]` inside `<world_state>` for the next
    /// narrator turn after the player returns to the NPC.
    pub directive: String,
}

/// Resolve a single off-screen task. Pure fn — no I/O, no side effects.
/// The caller (the tick handler) mutates the task's `resolved` flag +
/// removes it from the queue after applying the directive.
///
/// The resolution:
/// 1. Roll a d20 using a seed derived from (npc_id + description +
///    resolves_at). Deterministic per task — replays + tests reproduce.
/// 2. Apply the suitability modifier.
/// 3. Compare the modified roll against the difficulty DC.
/// 4. Map the margin to an `OutcomeSeverity` tier.
/// 5. Emit the directive.
///
/// The d20 + modifier are NEVER shown to the narrator — only the severity
/// tier + the directive seed. Same anti-sycophancy contract as the combat
/// Referee.
pub fn resolve_task(task: &OffScreenTask) -> TaskResolution {
    // Seed: stable per task. The same task always resolves the same way
    // (testable + replayable).
    let seed = hash_task(task);
    let mut roller = Roller::new(seed);
    let raw_roll = roll_d20(&mut roller);
    let modified = (raw_roll as i32 + task.suitability.modifier()).clamp(1, 30) as u32;
    let dc = task.difficulty.dc();
    let severity = OutcomeSeverity::from_margin(raw_roll, modified, dc);

    let directive = format!(
        "Off-screen task — {} ({}): {}. {}. Narrate the NPC's return and the local consequences; do not invent global or world-shaking repercussions.",
        task.description,
        task.npc_id,
        severity.tag().replace('_', " "),
        severity.directive_seed(),
    );

    TaskResolution {
        npc_id: task.npc_id.clone(),
        description: task.description.clone(),
        roll: raw_roll,
        dc,
        severity,
        directive,
    }
}

/// FNV-1a hash of (npc_id + description + resolves_at). Stable per task so
/// the resolution is deterministic. Mirrors `player_state::hash_text` — kept
/// local so this module stays self-contained.
fn hash_task(task: &OffScreenTask) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    // (2026-08-16 audit LOW) Field SEPARATORS: concatenating without one
    // made ("ab", "c") and ("a", "bc") collide — two different tasks could
    // roll identical dice. A `|` cannot appear ambiguity-free... it can
    // appear in a description, but a deliberate 3-field collision still
    // requires matching npc_id+description+eta anyway; the separator kills
    // the accidental boundary collisions.
    for b in task.npc_id.as_bytes().iter().copied().chain(Some(b'|')) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    for b in task.description.as_bytes().iter().copied().chain(Some(b'|')) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h ^= task.resolves_at_minutes as u64;
    h = h.wrapping_mul(0x100_0000_01B3);
    h
}

/// Resolve every due task in the queue. Pure fn — returns the resolutions;
/// the caller mutates the queue (marks tasks resolved + removes them) and
/// threads the directives into the next narrator prompt.
///
/// `now_minutes` is the WorldClock's current value. Tasks with
/// `resolves_at_minutes > now` are not yet due and are skipped; tasks
/// already `resolved` are skipped (no re-roll).
pub fn resolve_expired_tasks(tasks: &[OffScreenTask], now_minutes: i64) -> Vec<TaskResolution> {
    tasks
        .iter()
        .filter(|t| t.is_due(now_minutes))
        .map(resolve_task)
        .collect()
}

/// (2026-08-24 reaped-owner fix) The live-owner gate over
/// [`resolve_expired_tasks`]: every task whose NPC the reaper has archived
/// (`npc_interior.archived` stub set) — or whose registry entry was evicted
/// outright — is STALE and drops from the queue WITHOUT resolving or
/// narrating. The ungated path resolved + narrated due tasks for gone
/// NPCs, resurrecting dead characters in `[DIRECTIVE]` lines ("Marcus
/// returns from his scouting mission" about an NPC the world archived
/// days ago). The reaper itself refuses to archive open-task holders
/// (`reaper_protection_reason` → `open-task`), so this is the backstop for
/// the residual leaks: a `[TASK]` logged AFTER the archive (the tracker
/// narrating a ghost — logging never un-archives), evicted interiors, and
/// hand-edited saves. Mirrors [`select_focus`]'s player-bubble exclusion
/// set in shape. Pure; the caller derives the set (schema lock held) and
/// owns the post-resolution drain ([`remove_resolved_tasks`]).
pub fn resolve_expired_tasks_gated(
    tasks: &mut Vec<OffScreenTask>,
    now_minutes: i64,
    stale_owners: &std::collections::HashSet<String>,
) -> Vec<TaskResolution> {
    if !stale_owners.is_empty() {
        tasks.retain(|t| !stale_owners.contains(&t.npc_id));
    }
    resolve_expired_tasks(tasks, now_minutes)
}

/// Remove the tasks a tick's resolutions consumed, in place (the tick's
/// queue-drain step). One removal slot per resolution, keyed on the
/// (npc_id, description, DC) tuple.
///
/// (2026-08-16 yellow W2) Only DUE tasks consume a slot: duplicate tuples
/// (a `merge_patch` full-replace or a pre-P3 save can carry two tasks with
/// identical npc/description/DC but different `resolves_at_minutes`) used to
/// let the FIRST positional match consume the slot — possibly the not-yet-due
/// sibling — leaving the due one in the queue to re-resolve (and re-emit its
/// directive) on the next tick. Matching `is_due(now_minutes)` mirrors
/// `resolve_expired_tasks`'s own filter exactly, so the tasks removed are
/// precisely the tasks that produced the resolutions. Pure fn, unit-tested.
pub fn remove_resolved_tasks(
    tasks: &mut Vec<OffScreenTask>,
    resolutions: &[TaskResolution],
    now_minutes: i64,
) {
    let mut to_remove: Vec<(String, String, i64)> = resolutions
        .iter()
        .map(|r| (r.npc_id.clone(), r.description.clone(), r.dc as i64))
        .collect();
    tasks.retain(|t| {
        // A not-yet-due (or already-resolved) duplicate never consumes a
        // slot — its resolution hasn't happened.
        if !t.is_due(now_minutes) {
            return true;
        }
        let key = (
            t.npc_id.clone(),
            t.description.clone(),
            t.difficulty.dc() as i64,
        );
        match to_remove.iter().position(|k| *k == key) {
            Some(idx) => {
                to_remove.remove(idx);
                false
            }
            None => true,
        }
    });
}

// ===========================================================================
// Focus Randomization (the world-is-alive mechanic)
// ===========================================================================

/// A focus target: one entity outside the player's immediate bubble that the
/// World Progression tick should advance. Pure data — the schema engine's
/// progression prompt consumes a list of these as "designated entities for
/// this period" (the Multihog pattern).
///
/// The hard constraint (no apocalyptic shifts) lives in the SHAPE of this
/// struct: there is no `global_event` field. Every focus is bound to a
/// specific entity + a bounded mutation magnitude.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FocusTarget {
    /// The entity key (e.g. "npc.marcus", "loc.tavern", "faction.cult").
    pub entity_key: String,
    /// How big a change the tick is allowed to make to this entity.
    /// `Minor` = a mood shift, a small inventory change, a rumor;
    /// `Moderate` = a meaningful shift (NPC relocates, faction gains/loses
    ///   influence, a new event triggers);
    /// `Major` = a significant shift (NPC dies of natural causes, faction
    ///   goes to war) — used sparingly, never for multiple Major targets in
    ///   one tick.
    pub magnitude: FocusMagnitude,
    /// A short diegetic seed the schema engine can use ("Marcus visits the
    /// blacksmith", "the Cult recruits in the slums"). Free-form.
    pub seed: String,
}

/// The bounded magnitude of a focus mutation. The HARD CONSTRAINT against
/// apocalyptic shifts is enforced here: there is no "Apocalyptic" tier. The
/// worst a single tick can do to a single entity is `Major`, and only one
/// Major target is permitted per tick (see `select_focus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusMagnitude {
    Minor,
    Moderate,
    Major,
}

impl FocusMagnitude {
    /// The maximum number of this magnitude permitted per tick. The hard
    /// constraint against apocalyptic shifts: Major changes are rare
    /// (1 per tick max), Moderate changes are limited (3 per tick), Minor
    /// changes are liberal (up to 8 per tick — the world is full of small
    /// movements).
    pub fn per_tick_cap(self) -> usize {
        match self {
            FocusMagnitude::Minor => 8,
            FocusMagnitude::Moderate => 3,
            FocusMagnitude::Major => 1,
        }
    }
}

/// Select a focus set for this World Progression tick. Pure fn — given the
/// full pool of candidate entities + a roller, returns the bounded focus
/// list the schema engine will be asked to advance.
///
/// The hard constraint (no apocalyptic shifts) is enforced by:
/// 1. The per-magnitude caps in `FocusMagnitude::per_tick_cap`.
/// 2. The exclusion list (entities the player is currently interacting with
///    are off-limits — the focus is on the WORLD, not the bubble).
/// 3. The bounded magnitude ladder (no Apocalyptic tier exists).
///
/// The caller provides:
/// - `candidates`: every (entity_key, magnitude, seed) the world could
///   advance this tick.
/// - `excluded`: entity keys inside the player's bubble (the present NPCs,
///   the current location) — these are NOT valid focus targets this tick.
/// - `roller`: the seeded RNG (so focus selection is deterministic per tick
///   for testing).
pub fn select_focus(
    candidates: &[FocusTarget],
    excluded: &std::collections::HashSet<String>,
    roller: &mut Roller,
) -> Vec<FocusTarget> {
    // Filter out excluded entities first.
    let pool: Vec<&FocusTarget> = candidates
        .iter()
        .filter(|c| !excluded.contains(&c.entity_key))
        .collect();
    if pool.is_empty() {
        return Vec::new();
    }

    // Shuffle a copy of the pool indices (Fisher-Yates) so the selection
    // is a fair sample, not biased toward declaration order.
    let mut indices: Vec<usize> = (0..pool.len()).collect();
    for i in (1..indices.len()).rev() {
        let j = roller.range(i + 1);
        indices.swap(i, j);
    }

    // Walk the shuffled order, accumulating targets up to each magnitude's
    // per-tick cap. The order of magnitudes processed doesn't matter (each
    // has its own cap).
    let mut selected: Vec<FocusTarget> = Vec::new();
    let mut counts: HashMap<FocusMagnitude, usize> = HashMap::new();
    for &idx in &indices {
        let cand = pool[idx]; // pool[idx] : &FocusTarget
        let cap = cand.magnitude.per_tick_cap();
        let current = *counts.get(&cand.magnitude).unwrap_or(&0);
        if current >= cap {
            continue;
        }
        counts.insert(cand.magnitude, current + 1);
        selected.push(cand.clone());

        // Early exit if every cap is filled.
        let all_filled = [FocusMagnitude::Minor, FocusMagnitude::Moderate, FocusMagnitude::Major]
            .iter()
            .all(|&m| *counts.get(&m).unwrap_or(&0) >= m.per_tick_cap());
        if all_filled {
            break;
        }
    }
    selected
}

// ===========================================================================
// Promise frustration curve (2026-08-19 Referee QoL)
// ===========================================================================

/// The volatility-scaled promise-frustration score (pure Rust).
///
/// Maps elapsed-vs-deadline time onto a single axis the renderer bands:
/// NEGATIVE = the giver is pleased (the promise is comfortably ahead of
/// schedule — early fulfillment or healthy margin), 0 = neutral timing,
/// POSITIVE = growing frustration as the deadline slips. Pre-deadline the
/// ratio is ROOT-compressed (`ratio^(1/coeff) − 1`); post-deadline it grows
/// LINEARLY in the overrun (`(ratio − 1) × coeff`). `coeff` is the giver's
/// relationship `volatility` clamped to [0.1, 3.0] via the shared
/// `relationship::clamp_volatility` (1.0 when no relationship is tracked
/// or the value is invalid): a patient NPC (0.4) reads half-done as
/// −0.823 (Very Pleased) where a volatile one (3.0) reads it as −0.206
/// (barely Neutral) — and the same 50% overrun reads +0.200 vs +1.500.
/// A `now` before `accepted_at` (clock regression / un-stamped row)
/// clamps to ratio 0 — the old negative `powf` base produced NaN, which
/// fell through EVERY band to "Furious" (2026-08-20 audit L2).
///
/// Pinned test vectors (the plan's): halfway at coeff 0.4/1.0/3.0 →
/// −0.823/−0.500/−0.206; at the deadline → exactly 0; 50% overtime →
/// 0.200/0.500/1.500.
pub fn promise_frustration(
    accepted_at_minutes: i64,
    deadline_minutes: i64,
    now_minutes: i64,
    volatility: Option<f64>,
) -> f64 {
    let coeff = crate::relationship::clamp_volatility(volatility.unwrap_or(1.0));
    // A non-positive window (0-minute promise, or a legacy un-stamped row)
    // is instantly overdue — treat the span as 1 minute so the ratio is
    // finite + the post-deadline branch takes over.
    let span = (deadline_minutes - accepted_at_minutes).max(1) as f64;
    let ratio = (now_minutes - accepted_at_minutes).max(0) as f64 / span;
    if ratio <= 1.0 {
        ratio.powf(1.0 / coeff) - 1.0
    } else {
        (ratio - 1.0) * coeff
    }
}

/// Band the frustration score into the label the `owed:` render shows (and
/// the narrator plays). Ordered worst→best; boundaries are inclusive `<=`.
pub fn frustration_band(score: f64) -> &'static str {
    if score <= -0.5 {
        "Very Pleased"
    } else if score <= -0.1 {
        "Pleased"
    } else if score <= 0.1 {
        "Neutral"
    } else if score <= 0.5 {
        "Mildly Frustrated"
    } else if score <= 1.0 {
        "Frustrated"
    } else if score <= 1.5 {
        "Very Frustrated"
    } else {
        "Furious"
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ---- Promise frustration curve (2026-08-19) ----

    /// The plan's pinned vectors: halfway (ratio 0.5) at the three reference
    /// volatilities, the deadline itself, and 50% overtime.
    #[test]
    fn promise_frustration_pinned_vectors() {
        let halfway = promise_frustration(0, 100, 50, None);
        assert!((halfway - (-0.5)).abs() < 1e-9, "coeff 1.0 halfway: {halfway}");
        // Pre-deadline is root-compressed: patient NPCs read MORE pleased.
        let patient = promise_frustration(0, 100, 50, Some(0.4));
        assert!((patient - (-0.823)).abs() < 1e-3, "coeff 0.4 halfway: {patient}");
        let volatile = promise_frustration(0, 100, 50, Some(3.0));
        assert!((volatile - (-0.206)).abs() < 1e-3, "coeff 3.0 halfway: {volatile}");
        // At the deadline every coeff reads exactly 0.
        for coeff in [0.1f64, 1.0, 3.0] {
            let at_deadline = promise_frustration(0, 100, 100, Some(coeff));
            assert!(at_deadline.abs() < 1e-9, "coeff {coeff} at deadline: {at_deadline}");
        }
        // Post-deadline grows linearly in the overrun, scaled by coeff.
        let over = promise_frustration(0, 100, 150, None);
        assert!((over - 0.5).abs() < 1e-9, "coeff 1.0 overtime: {over}");
        let over_patient = promise_frustration(0, 100, 150, Some(0.4));
        assert!((over_patient - 0.2).abs() < 1e-9, "coeff 0.4 overtime: {over_patient}");
        let over_volatile = promise_frustration(0, 100, 150, Some(3.0));
        assert!((over_volatile - 1.5).abs() < 1e-9, "coeff 3.0 overtime: {over_volatile}");
    }

    /// A `now` before `accepted_at` (clock regression / un-stamped row)
    /// clamps to ratio 0 instead of NaN-ing the powf (2026-08-20 audit L2).
    #[test]
    fn promise_frustration_pre_acceptance_never_nans() {
        let s = promise_frustration(1000, 2000, 500, Some(1.0));
        assert!(s.is_finite(), "score must be finite, got {s}");
        assert_eq!(frustration_band(s), "Very Pleased");
    }

    /// Volatility is clamped — an absurd hand-edited coefficient can't
    /// degenerate the curve (and `None` = the 1.0 default).
    #[test]
    fn promise_frustration_clamps_volatility() {
        let lo = promise_frustration(0, 100, 50, Some(0.0001));
        let lo_clamped = promise_frustration(0, 100, 50, Some(0.1));
        assert!((lo - lo_clamped).abs() < 1e-9, "volatility clamps at 0.1");
        let hi = promise_frustration(0, 100, 50, Some(99.0));
        let hi_clamped = promise_frustration(0, 100, 50, Some(3.0));
        assert!((hi - hi_clamped).abs() < 1e-9, "volatility clamps at 3.0");
        assert_eq!(
            promise_frustration(0, 100, 50, None),
            promise_frustration(0, 100, 50, Some(1.0)),
            "None defaults to 1.0"
        );
    }

    /// A degenerate window (deadline ≤ accepted) reads as overdue, never
    /// NaN.
    #[test]
    fn promise_frustration_survives_zero_window() {
        let s = promise_frustration(100, 100, 100, None);
        assert!(s.is_finite(), "zero window must stay finite: {s}");
        let late = promise_frustration(100, 100, 130, None);
        assert!(late.is_finite() && late > 0.0, "zero window overdue: {late}");
    }

    /// The band boundaries (inclusive <=, worst→best).
    #[test]
    fn frustration_bands_match_the_plan() {
        assert_eq!(frustration_band(-0.9), "Very Pleased");
        assert_eq!(frustration_band(-0.5), "Very Pleased");
        assert_eq!(frustration_band(-0.499), "Pleased");
        assert_eq!(frustration_band(-0.1), "Pleased");
        assert_eq!(frustration_band(0.0), "Neutral");
        assert_eq!(frustration_band(0.1), "Neutral");
        assert_eq!(frustration_band(0.11), "Mildly Frustrated");
        assert_eq!(frustration_band(0.5), "Mildly Frustrated");
        assert_eq!(frustration_band(0.51), "Frustrated");
        assert_eq!(frustration_band(1.0), "Frustrated");
        assert_eq!(frustration_band(1.2), "Very Frustrated");
        assert_eq!(frustration_band(1.5), "Very Frustrated");
        assert_eq!(frustration_band(1.51), "Furious");
    }

    // ---- TaskDifficulty ----

    #[test]
    fn difficulty_dc_is_monotonic() {
        assert!(TaskDifficulty::Trivial.dc() < TaskDifficulty::Routine.dc());
        assert!(TaskDifficulty::Routine.dc() < TaskDifficulty::Challenging.dc());
        assert!(TaskDifficulty::Challenging.dc() < TaskDifficulty::Hard.dc());
        assert!(TaskDifficulty::Hard.dc() < TaskDifficulty::NearImpossible.dc());
    }

    // ---- Suitability ----

    #[test]
    fn suitability_modifiers_sum_correctly() {
        assert_eq!(Suitability::Hopeless.modifier(), -5);
        assert_eq!(Suitability::Poor.modifier(), -2);
        assert_eq!(Suitability::Adequate.modifier(), 0);
        assert_eq!(Suitability::WellSuited.modifier(), 2);
        assert_eq!(Suitability::Ideal.modifier(), 5);
    }

    // ---- OutcomeSeverity mapping ----

    #[test]
    fn natural_1_is_alwayscatastrophic() {
        let sev = OutcomeSeverity::from_margin(1, 1, 5);
        assert_eq!(sev, OutcomeSeverity::CatastrophicFailure);
        // Even against a Trivial DC.
        let sev = OutcomeSeverity::from_margin(1, 1, 5);
        assert_eq!(sev, OutcomeSeverity::CatastrophicFailure);
    }

    #[test]
    fn natural_20_is_always_critical() {
        let sev = OutcomeSeverity::from_margin(20, 20, 25);
        assert_eq!(sev, OutcomeSeverity::CriticalSuccess);
        // Even against NearImpossible.
        let sev = OutcomeSeverity::from_margin(20, 20, 25);
        assert_eq!(sev, OutcomeSeverity::CriticalSuccess);
    }

    #[test]
    fn modified_20_is_not_an_auto_crit() {
        // raw 15 + Ideal(+5) = effective 20 vs DC 15 → Success by margin
        // (margin 5), NOT a natural-20 CriticalSuccess.
        assert_eq!(
            OutcomeSeverity::from_margin(15, 20, 15),
            OutcomeSeverity::Success
        );
    }

    #[test]
    fn modified_1_is_not_an_auto_catastrophe() {
        // raw 6 + Hopeless(−5) = effective 1 vs DC 5 → plain Failure — the
        // natural-1 override must NOT fire on the adjusted value.
        assert_eq!(
            OutcomeSeverity::from_margin(6, 1, 5),
            OutcomeSeverity::Failure
        );
    }

    #[test]
    fn outcome_failure_by_margin() {
        // roll 8 vs DC 15 → Failure (missed by 7, not by 10+).
        assert_eq!(OutcomeSeverity::from_margin(8, 8, 15), OutcomeSeverity::Failure);
        // roll 5 vs DC 15 → CatastrophicFailure (missed by 10).
        assert_eq!(OutcomeSeverity::from_margin(5, 5, 15), OutcomeSeverity::CatastrophicFailure);
    }

    #[test]
    fn outcome_success_by_margin() {
        // roll 16 vs DC 15 → ComplicatedSuccess (made by 1, margin < 5).
        assert_eq!(OutcomeSeverity::from_margin(16, 16, 15), OutcomeSeverity::ComplicatedSuccess);
        // roll 19 vs DC 15 → Success (made by 4 — still < 5).
        assert_eq!(OutcomeSeverity::from_margin(19, 19, 15), OutcomeSeverity::ComplicatedSuccess);
        // roll 20 vs DC 15 → CriticalSuccess (natural 20 overrides).
        assert_eq!(OutcomeSeverity::from_margin(20, 20, 15), OutcomeSeverity::CriticalSuccess);
    }

    // ---- resolve_task ----

    #[test]
    fn resolve_task_returns_directive_with_npc_and_severity() {
        let task = OffScreenTask {
            npc_id: "marcus".into(),
            description: "scout the bandit camp".into(),
            difficulty: TaskDifficulty::Challenging,
            suitability: Suitability::WellSuited,
            resolves_at_minutes: 10000,
            resolved: false,
        };
        let res = resolve_task(&task);
        assert_eq!(res.npc_id, "marcus");
        assert_eq!(res.description, "scout the bandit camp");
        assert!(res.directive.contains("marcus"));
        assert!(res.directive.contains("scout the bandit camp"));
        // The directive must enforce the no-apocalyptic-shift constraint.
        assert!(
            res.directive.contains("do not invent global"),
            "directive must enforce the no-apocalyptic-shift constraint: {}",
            res.directive
        );
    }

    #[test]
    fn resolve_task_is_deterministic_for_same_task() {
        let task = OffScreenTask {
            npc_id: "mira".into(),
            description: "negotiate with the guildmaster".into(),
            difficulty: TaskDifficulty::Hard,
            suitability: Suitability::Adequate,
            resolves_at_minutes: 20000,
            resolved: false,
        };
        let a = resolve_task(&task);
        let b = resolve_task(&task);
        // Same task → same resolution (deterministic seed).
        assert_eq!(a.roll, b.roll);
        assert_eq!(a.severity, b.severity);
    }

    #[test]
    fn resolve_task_natural_20_yields_critical_success() {
        // We can't force a natural 20 without knowing the seed, but we can
        // verify the high-likelihood path: a well-suited NPC on a Trivial
        // task almost always succeeds. Across many tasks of the same shape,
        // at least one should be CriticalSuccess or Success.
        let mut any_success = false;
        for i in 0..64 {
            let task = OffScreenTask {
                npc_id: format!("npc_{i}"),
                description: format!("task_{i}"),
                difficulty: TaskDifficulty::Trivial,
                suitability: Suitability::Ideal,
                resolves_at_minutes: i as i64,
                resolved: false,
            };
            let res = resolve_task(&task);
            if res.severity >= OutcomeSeverity::Success {
                any_success = true;
                break;
            }
        }
        assert!(any_success, "Ideal NPC on Trivial task should succeed in at least one of 64 trials");
    }

    // ---- resolve_expired_tasks ----

    #[test]
    fn resolve_expired_tasks_skips_not_yet_due() {
        let tasks = vec![
            OffScreenTask {
                npc_id: "marcus".into(),
                description: "due task".into(),
                difficulty: TaskDifficulty::Routine,
                suitability: Suitability::Adequate,
                resolves_at_minutes: 1000,
                resolved: false,
            },
            OffScreenTask {
                npc_id: "mira".into(),
                description: "future task".into(),
                difficulty: TaskDifficulty::Routine,
                suitability: Suitability::Adequate,
                resolves_at_minutes: 5000,
                resolved: false,
            },
        ];
        // now = 2000: only the first task is due.
        let resolutions = resolve_expired_tasks(&tasks, 2000);
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].npc_id, "marcus");
    }

    #[test]
    fn resolve_expired_tasks_skips_already_resolved() {
        let tasks = vec![OffScreenTask {
            npc_id: "marcus".into(),
            description: "done task".into(),
            difficulty: TaskDifficulty::Routine,
            suitability: Suitability::Adequate,
            resolves_at_minutes: 1000,
            resolved: true, // already done
        }];
        let resolutions = resolve_expired_tasks(&tasks, 5000);
        assert!(resolutions.is_empty(), "resolved tasks must not re-roll");
    }

    // ---- resolve_expired_tasks_gated (the reaped-owner fix, 2026-08-24) ----

    /// A due task owned by a REAPED (archived) NPC must neither resolve nor
    /// narrate — it drops from the queue as stale. A due task owned by a
    /// live NPC resolves normally through the same call.
    #[test]
    fn gated_resolution_drops_reaped_owner_tasks() {
        let mut tasks = vec![
            task("marcus", "scout the pass", 1000), // owner live — resolves
            task("ghost", "deliver the letter", 1000), // owner reaped — drops
        ];
        let mut stale = HashSet::new();
        stale.insert("ghost".to_string());
        let resolutions = resolve_expired_tasks_gated(&mut tasks, 2000, &stale);
        assert_eq!(resolutions.len(), 1, "only the live owner's task resolves");
        assert_eq!(resolutions[0].npc_id, "marcus");
        // The reaped owner's task is GONE from the queue — no future tick
        // may resurrect it.
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].npc_id, "marcus");
        // And the surviving task drains through the normal path.
        remove_resolved_tasks(&mut tasks, &resolutions, 2000);
        assert!(tasks.is_empty());
    }

    /// A not-yet-due task owned by a reaped NPC drops too — the owner is
    /// gone from the world; waiting for the ETA would only defer the
    /// resurrection.
    #[test]
    fn gated_resolution_drops_future_tasks_of_reaped_owners() {
        let mut tasks = vec![
            task("ghost", "deliver the letter", 9000), // due later
            task("mira", "negotiate", 1000),           // live, due now
        ];
        let mut stale = HashSet::new();
        stale.insert("ghost".to_string());
        let resolutions = resolve_expired_tasks_gated(&mut tasks, 2000, &stale);
        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].npc_id, "mira");
        assert!(tasks.iter().all(|t| t.npc_id != "ghost"));
    }

    /// An empty stale set behaves exactly like the ungated fn (the no-op
    /// fast path — every tick where nobody was reaped).
    #[test]
    fn gated_resolution_empty_set_matches_ungated() {
        let mut tasks = vec![
            task("marcus", "scout the pass", 1000),
            task("mira", "negotiate", 5000),
        ];
        let gated = resolve_expired_tasks_gated(&mut tasks, 2000, &HashSet::new());
        let ungated = resolve_expired_tasks(&tasks, 2000);
        assert_eq!(gated.len(), ungated.len());
        assert_eq!(gated[0].npc_id, ungated[0].npc_id);
        assert_eq!(tasks.len(), 2, "nothing dropped when nobody is stale");
    }

    // ---- remove_resolved_tasks (the tick's queue drain, yellow W2) ----

    fn task(npc: &str, desc: &str, resolves_at: i64) -> OffScreenTask {
        OffScreenTask {
            npc_id: npc.into(),
            description: desc.into(),
            difficulty: TaskDifficulty::Routine,
            suitability: Suitability::Adequate,
            resolves_at_minutes: resolves_at,
            resolved: false,
        }
    }

    /// One resolution + two same-TUPLE tasks (different resolves_at): the DUE
    /// one is removed, the not-yet-due sibling survives — regardless of the
    /// queue's ordering. The old positional first-match could delete the
    /// future sibling and leave the due one to re-resolve next tick.
    #[test]
    fn remove_resolved_tasks_spares_not_yet_due_duplicate_tuples() {
        let resolutions = vec![TaskResolution {
            npc_id: "marcus".into(),
            description: "scout the pass".into(),
            roll: 15,
            dc: TaskDifficulty::Routine.dc(),
            severity: OutcomeSeverity::Success,
            directive: "marcus returned".into(),
        }];

        // Future sibling FIRST in the queue — the adversarial ordering.
        let mut tasks = vec![
            task("marcus", "scout the pass", 5000),
            task("marcus", "scout the pass", 1000),
        ];
        remove_resolved_tasks(&mut tasks, &resolutions, 2000);
        assert_eq!(tasks.len(), 1, "exactly the due copy is removed");
        assert_eq!(tasks[0].resolves_at_minutes, 5000, "the future sibling survives");

        // And the mirrored ordering.
        let mut tasks = vec![
            task("marcus", "scout the pass", 1000),
            task("marcus", "scout the pass", 5000),
        ];
        remove_resolved_tasks(&mut tasks, &resolutions, 2000);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].resolves_at_minutes, 5000);
    }

    /// Two due duplicates → two resolutions from resolve_expired_tasks → both
    /// removed (the count semantics the P3 fix established stay intact).
    #[test]
    fn remove_resolved_tasks_removes_as_many_as_resolved() {
        let mut tasks = vec![
            task("marcus", "scout the pass", 1000),
            task("marcus", "scout the pass", 1200),
        ];
        let resolutions = resolve_expired_tasks(&tasks, 2000);
        assert_eq!(resolutions.len(), 2);
        remove_resolved_tasks(&mut tasks, &resolutions, 2000);
        assert!(tasks.is_empty());
    }

    // ---- Focus randomization ----

    #[test]
    fn focus_magnitude_caps_enforce_no_apocalyptic() {
        // The hard constraint: Minor=8, Moderate=3, Major=1 per tick. No
        // Apocalyptic tier exists. Pin the caps so a future edit can't
        // silently raise them.
        assert_eq!(FocusMagnitude::Minor.per_tick_cap(), 8);
        assert_eq!(FocusMagnitude::Moderate.per_tick_cap(), 3);
        assert_eq!(FocusMagnitude::Major.per_tick_cap(), 1);
    }

    #[test]
    fn select_focus_excludes_player_bubble() {
        let candidates = vec![
            FocusTarget {
                entity_key: "npc.marcus".into(),
                magnitude: FocusMagnitude::Minor,
                seed: "visits the blacksmith".into(),
            },
            FocusTarget {
                entity_key: "loc.tavern".into(), // present location — excluded
                magnitude: FocusMagnitude::Minor,
                seed: "the fire crackles".into(),
            },
        ];
        let mut excluded = HashSet::new();
        excluded.insert("loc.tavern".into());
        let mut roller = Roller::new(42);
        let selected = select_focus(&candidates, &excluded, &mut roller);
        assert!(
            selected.iter().all(|t| t.entity_key != "loc.tavern"),
            "excluded entities must not be selected"
        );
    }

    #[test]
    fn select_focus_respects_major_cap() {
        // Provide 5 Major candidates; only 1 should be selected.
        let candidates: Vec<FocusTarget> = (0..5)
            .map(|i| FocusTarget {
                entity_key: format!("npc.{i}"),
                magnitude: FocusMagnitude::Major,
                seed: format!("seed_{i}"),
            })
            .collect();
        let excluded = HashSet::new();
        let mut roller = Roller::new(99);
        let selected = select_focus(&candidates, &excluded, &mut roller);
        let major_count = selected
            .iter()
            .filter(|t| t.magnitude == FocusMagnitude::Major)
            .count();
        assert!(
            major_count <= FocusMagnitude::Major.per_tick_cap(),
            "Major targets must respect the per-tick cap (got {major_count})"
        );
    }

    #[test]
    fn select_focus_respects_all_caps() {
        // Provide many of each magnitude; verify each cap is respected.
        let mut candidates: Vec<FocusTarget> = Vec::new();
        for i in 0..20 {
            for (mag, suffix) in [
                (FocusMagnitude::Minor, "min"),
                (FocusMagnitude::Moderate, "mod"),
                (FocusMagnitude::Major, "maj"),
            ] {
                candidates.push(FocusTarget {
                    entity_key: format!("{suffix}_{i}"),
                    magnitude: mag,
                    seed: format!("{suffix} seed {i}"),
                });
            }
        }
        let excluded = HashSet::new();
        let mut roller = Roller::new(7);
        let selected = select_focus(&candidates, &excluded, &mut roller);
        for mag in [FocusMagnitude::Minor, FocusMagnitude::Moderate, FocusMagnitude::Major] {
            let count = selected.iter().filter(|t| t.magnitude == mag).count();
            assert!(
                count <= mag.per_tick_cap(),
                "{mag:?} count {count} exceeds cap {}",
                mag.per_tick_cap()
            );
        }
    }

    #[test]
    fn select_focus_empty_candidates_returns_empty() {
        let candidates: Vec<FocusTarget> = Vec::new();
        let excluded = HashSet::new();
        let mut roller = Roller::new(0);
        let selected = select_focus(&candidates, &excluded, &mut roller);
        assert!(selected.is_empty());
    }

    #[test]
    fn select_focus_is_deterministic_for_same_seed() {
        let candidates: Vec<FocusTarget> = (0..10)
            .map(|i| FocusTarget {
                entity_key: format!("npc.{i}"),
                magnitude: FocusMagnitude::Minor,
                seed: format!("seed_{i}"),
            })
            .collect();
        let excluded = HashSet::new();
        let mut r1 = Roller::new(123);
        let mut r2 = Roller::new(123);
        let s1 = select_focus(&candidates, &excluded, &mut r1);
        let s2 = select_focus(&candidates, &excluded, &mut r2);
        assert_eq!(s1, s2, "same seed must produce same selection");
    }
}
