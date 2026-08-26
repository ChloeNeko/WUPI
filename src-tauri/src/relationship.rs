//! Relationship state machine (Fable Phase 3 Slice 5, 2026-07-28): the
//! diegetic-truth replacement for the legacy relationship integer.
//!
//! # Why a state machine, not a -100..+100 number
//!
//! The integer was permissive-by-default: the LLM's schema-delta could shift
//! "Marcus +10" arbitrarily, and the sycophancy bias in narration would push
//! relationships toward escalation ("Marcus is now your fast friend after
//! one drink"). The state machine forces every transition through a
//! Rust-checkable gate against diegetic truth: BOTH a time floor AND a
//! milestone event must clear before the tier advances. The narrator sees
//! only the resulting categorical label (Stranger / Acquaintance / Friendly /
//! Trusted / Bonded on the positive track; Rival / Hostile / Nemesis on the
//! negative track).
//!
//! # The bidirectional design
//!
//! Relationships are NOT monotonic. The integer handled deterioration
//! naturally (-90 = kill on sight); a forward-only state machine can't
//! represent "Marcus now hates you" at all. The design uses two parallel
//! tracks:
//!
//! - **Affinity track** (Stranger → Acquaintance → Friendly → Trusted →
//!   Bonded): earned slowly, time-locked + milestone-locked.
//! - **Hostility track** (Rival → Hostile → Nemesis): can fire on a single
//!   betrayal, no time floor (the gravity of betrayal is asymmetric).
//!
//! Both tracks are mutually exclusive — an NPC is on exactly one. The
//! `RelationshipTier` enum encodes them in a single ladder worst→best so
//! `as u8` comparisons work for "better than" / "worse than" reasoning.
//!
//! # Dual-keyed gates
//!
//! To advance on the affinity track, BOTH conditions must hold:
//! 1. **Time floor**: at least N in-world days have passed since the previous
//!    transition. Prevents "give a shiny sword on Day 1, BFF by Day 2."
//! 2. **Milestone event**: the player has accumulated enough qualifying
//!    events (saved_life, defended_in_combat, shared_downtime, …) to
//!    justify the new tier. Prevents "idling the calendar = ripening
//!    relationships in the player's absence."
//!
//! The Hostility track ignores both gates: a betrayal is a betrayal on Day 1
//! or Day 100.
//!
//! # Wupi-authorable milestones
//!
//! The milestone registry is NOT hardcoded. Wupi (the Wupi-as-game-manager
//! path) authors entries via codex: each entry is (keyword, point_value,
//! applies_to_tier). The `MilestoneRegistry` struct loads these from a
//! config blob the Wupi tool emits. Today the registry ships with a small
//! default set; a future Wupi-authored codex entry can extend it.
//!
//! # Silent-drop policy
//!
//! When the schema validator detects a relationship-tier escalation attempt
//! that hasn't cleared both gates, the delta is SILENTLY DROPPED — it does
//! NOT enter the failed_delta_queue (re-running the same prompt N more times
//! won't clear a still-closed time gate). The narrator instead receives a
//! `[DIRECTIVE: <npc> views you as <tier>. ...]` line explaining the NPC's
//! current stance, so it stops trying to escalate and writes prose that
//! matches the actual tier.
//!
//! # Per-NPC volatility
//!
//! Each NPC carries a `volatility` coefficient (default 1.0; 0.4 patient →
//! 3.0 volatile) that scales the time floor: volatile NPCs befriend faster
//! AND turn hostile faster. Reuses the same coefficient concept as the Quest
//! Frustration Curve (Slice 4) — one abstraction, two consumers. (The
//! Frustration Curve uses volatility on the *deadline math*; the
//! Relationship machine uses it on the *time floor math* — same knob,
//! different applications.)

use std::collections::HashMap;

// ===========================================================================
// RelationshipTier: the categorical state
// ===========================================================================

/// The categorical relationship tier an NPC holds toward the player.
/// STRICT ORDERING worst → best by enum-variant order, so `as u8`
/// comparisons give "better than" / "worse than" for free.
///
/// The ladder has three zones:
/// - **Hostility zone** (Nemesis, Hostile, Rival): negative tiers, fire on
///   betrayal/injury, no time floor.
/// - **Neutral zone** (Stranger, Acquaintance): the starting default. Most
///   new NPCs land here. Acquaintance is the "we've met and remember each
///   other" tier.
/// - **Affinity zone** (Friendly, Trusted, Bonded): earned slowly via
///   dual-keyed gates (time floor + milestone events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipTier {
    /// The NPC wants the player dead. Active hostility, will attack on sight,
    /// hire assassins, denounce publicly. Achieved via a severe betrayal
    /// (killed their family, razed their home) — NEVER the result of drift.
    Nemesis,
    /// The NPC is actively hostile: refuses to deal, may attack if provoked,
    /// works against the player behind the scenes. Fire on a serious
    /// betrayal or sustained harm.
    Hostile,
    /// The NPC dislikes the player: cold, uncooperative, may refuse service
    /// or charge inflated prices. Mild betrayal territory.
    Rival,
    /// Default for a brand-new NPC. The NPC has no opinion; interactions are
    /// transactional.
    Stranger,
    /// The NPC knows the player by name and remembers past interactions.
    /// Polite, willing to engage, but no real trust yet. Reached
    /// automatically after any positive first interaction.
    Acquaintance,
    /// The NPC likes the player. Warm, helpful, may extend small favors.
    /// Time-locked + milestone-locked past Acquaintance.
    Friendly,
    /// The NPC trusts the player deeply. Will extend significant aid, share
    /// secrets, vouch for the player to others. Dual-keyed gate past Friendly.
    Trusted,
    /// The NPC is bonded to the player — chosen family, sworn liege,
    /// lifelong companion. Will risk death for the player. The far end of
    /// the affinity track; requires multiple major milestones.
    Bonded,
}

impl Default for RelationshipTier {
    fn default() -> Self {
        RelationshipTier::Stranger
    }
}

impl RelationshipTier {
    /// Human-readable label for prompt injection. Reads as a phrase the
    /// narrator can weave into prose ("Marcus views you as a Trusted
    /// companion…").
    pub fn label(self) -> &'static str {
        match self {
            RelationshipTier::Nemesis => "a nemesis who wants you dead",
            RelationshipTier::Hostile => "openly hostile, working against you",
            RelationshipTier::Rival => "cold and uncooperative",
            RelationshipTier::Stranger => "a stranger with no opinion of you",
            RelationshipTier::Acquaintance => "an acquaintance who knows your name",
            RelationshipTier::Friendly => "a friendly presence, warm and helpful",
            RelationshipTier::Trusted => "a trusted confidant, willing to vouch for you",
            RelationshipTier::Bonded => "bonded to you — chosen family, sworn to your side",
        }
    }

    /// The lowercase tag for serialization + prompt attribute.
    pub fn tag(self) -> &'static str {
        match self {
            RelationshipTier::Nemesis => "nemesis",
            RelationshipTier::Hostile => "hostile",
            RelationshipTier::Rival => "rival",
            RelationshipTier::Stranger => "stranger",
            RelationshipTier::Acquaintance => "acquaintance",
            RelationshipTier::Friendly => "friendly",
            RelationshipTier::Trusted => "trusted",
            RelationshipTier::Bonded => "bonded",
        }
    }

    /// True if this tier is on the affinity track (Friendly or above).
    /// Used by the gate logic to decide whether time/milestone gates apply.
    pub fn is_affinity(self) -> bool {
        matches!(
            self,
            RelationshipTier::Friendly | RelationshipTier::Trusted | RelationshipTier::Bonded
        )
    }

    /// True if this tier is on the hostility track (Rival or below).
    pub fn is_hostility(self) -> bool {
        matches!(
            self,
            RelationshipTier::Nemesis | RelationshipTier::Hostile | RelationshipTier::Rival
        )
    }

    /// The next-higher affinity tier, if any. Used by the gate logic to
    /// compute what a transition would land on.
    pub fn next_affinity(self) -> Option<RelationshipTier> {
        match self {
            RelationshipTier::Stranger => Some(RelationshipTier::Acquaintance),
            RelationshipTier::Acquaintance => Some(RelationshipTier::Friendly),
            RelationshipTier::Friendly => Some(RelationshipTier::Trusted),
            RelationshipTier::Trusted => Some(RelationshipTier::Bonded),
            RelationshipTier::Bonded => None, // ceiling
            // Hostility-track tiers transition to Neutral first (reconciliation).
            RelationshipTier::Rival | RelationshipTier::Hostile | RelationshipTier::Nemesis => {
                Some(RelationshipTier::Stranger)
            }
        }
    }
}

// ===========================================================================
// Per-NPC relationship state (DriverEvent + DriverStatic)
// ===========================================================================

/// The persisted relationship state for ONE NPC toward the player. Lives on
/// `WorldSchema` (Slice 5 will nest a `HashMap<npc_id, RelationshipState>`).
///
/// The state is Rust-authoritative: the schema-delta LLM pass CANNOT write
/// here directly. Transitions happen through `evaluate_transition`, which
/// checks the dual-keyed gates against diegetic truth (time elapsed +
/// milestone events accumulated). The narrator sees only the resulting
/// `tier` label.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RelationshipState {
    /// The current categorical tier. The only field the narrator reads.
    pub tier: RelationshipTier,

    /// The in-world minute (same epoch units as `WorldClock::current_minutes`)
    /// at which the CURRENT tier was entered. Used by the time-floor gate to
    /// decide whether enough in-world time has passed for the next
    /// transition. 0 means "unset" (Stranger by default).
    #[serde(default)]
    pub tier_entered_at_minutes: i64,

    /// The set of milestone event IDs the player has accumulated with this
    /// NPC. Each ID is a short string key (e.g. "saved_life",
    /// "betrayed_trust", "shared_downtime"). Append-only — events never
    /// leave (you can't un-save a life). The registry (below) maps IDs to
    /// point values + applicable tiers.
    #[serde(default)]
    pub events: Vec<String>,

    /// Per-NPC volatility coefficient (default 1.0; 0.4 patient → 3.0
    /// volatile). Scales the time floor: volatile NPCs befriend AND turn
    /// hostile faster. Reuses the same abstraction as the Quest Frustration
    /// Curve (Slice 4).
    #[serde(default = "default_volatility")]
    pub volatility: f64,
}

fn default_volatility() -> f64 {
    1.0
}

/// Clamp a volatility coefficient to the design band [0.1, 3.0] (invalid
/// input — 0, negative, NaN, infinity — folds to the 1.0 default; the
/// serde default only covers ABSENCE, not a hand-installed bad value).
/// Shared by every consumer that scales time by volatility — the
/// relationship time floor, the promise-frustration curve
/// (`offscreen_task`), and the quest-frustration score (`consequence`) —
/// so one authority owns the band (2026-08-20 audit M4).
pub fn clamp_volatility(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        v.clamp(0.1, 3.0)
    } else {
        1.0
    }
}

impl Default for RelationshipState {
    fn default() -> Self {
        RelationshipState {
            tier: RelationshipTier::Stranger,
            tier_entered_at_minutes: 0,
            events: Vec::new(),
            volatility: 1.0,
        }
    }
}

impl RelationshipState {
    /// True if this state is the fresh default (Stranger, no events). Used
    /// to omit the relationship block from the prompt on a brand-new NPC.
    pub fn is_default(&self) -> bool {
        self.tier == RelationshipTier::Stranger
            && self.events.is_empty()
            && self.tier_entered_at_minutes == 0
            && (self.volatility - 1.0).abs() < 0.001
    }

    /// Record a milestone event. Append-only: events never leave. Returns
    /// true if the event was new (not already recorded).
    pub fn record_event(&mut self, event_id: &str) -> bool {
        if self.events.iter().any(|e| e == event_id) {
            return false;
        }
        self.events.push(event_id.to_owned());
        true
    }

    /// Has the player accumulated the given milestone event?
    pub fn has_event(&self, event_id: &str) -> bool {
        self.events.iter().any(|e| e == event_id)
    }
}

// ===========================================================================
// MilestoneRegistry: Wupi-authorable event definitions
// ===========================================================================

/// One milestone event definition. Wupi authors these via codex (the
/// Wupi-as-game-manager path emits a config blob the registry loads). Each
/// entry tells the gate logic: "this event is worth N points toward an
/// affinity advance, and it applies when transitioning FROM this source tier
/// (or `Any` to apply from any tier)."
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MilestoneSpec {
    /// The event ID. Short stable string key ("saved_life", "betrayed_trust",
    /// "shared_downtime"). The Referee/event-detector emits these.
    pub id: String,
    /// Point value toward the next affinity tier. Higher = bigger impact.
    /// A typical milestone is 1-3 points; "saved their life" might be 3,
    /// "shared a drink" might be 1.
    pub points: u32,
    /// The affinity advance this event counts toward. `Any` = counts toward
    /// any transition; a specific tier = counts only when transitioning
    /// INTO that tier. Most events are `Any`.
    pub applies_to: MilestoneApplicability,
    /// If true, this event is a HOSTILITY trigger: recording it drops the
    /// NPC's tier into the hostility zone regardless of the current affinity.
    /// Betrayal/murder-of-kin type events. These bypass the time floor.
    pub hostility_trigger: bool,
    /// If `hostility_trigger`, which hostility tier does this drop to?
    /// Required when `hostility_trigger == true`.
    pub hostility_drop: Option<RelationshipTier>,
}

/// Which affinity advance a milestone counts toward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneApplicability {
    /// Counts toward any affinity transition.
    Any,
    /// Counts only when transitioning INTO the Friendly tier.
    IntoFriendly,
    /// Counts only when transitioning INTO the Trusted tier.
    IntoTrusted,
    /// Counts only when transitioning INTO the Bonded tier.
    IntoBonded,
}

impl MilestoneApplicability {
    /// True if this applicability matches a transition into the given tier.
    pub fn matches(self, target_tier: RelationshipTier) -> bool {
        match self {
            MilestoneApplicability::Any => true,
            MilestoneApplicability::IntoFriendly => target_tier == RelationshipTier::Friendly,
            MilestoneApplicability::IntoTrusted => target_tier == RelationshipTier::Trusted,
            MilestoneApplicability::IntoBonded => target_tier == RelationshipTier::Bonded,
        }
    }
}

/// The registry of known milestone events. Wupi-authored via codex; today
/// ships with a small default set. The gate logic queries this to compute
/// whether the player has accumulated enough points to advance.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MilestoneRegistry {
    /// The known specs, keyed by event ID.
    pub specs: HashMap<String, MilestoneSpec>,
}

impl MilestoneRegistry {
    /// The shipped default registry. A small, conservative set covering the
    /// common diegetic events. Wupi can extend this at runtime via codex.
    pub fn defaults() -> Self {
        let mut specs = HashMap::new();
        let mk = |id: &str, points: u32, applies_to: MilestoneApplicability| MilestoneSpec {
            id: id.to_owned(),
            points,
            applies_to,
            hostility_trigger: false,
            hostility_drop: None,
        };
        let mk_hostility = |id: &str, drop: RelationshipTier| MilestoneSpec {
            id: id.to_owned(),
            points: 0,
            applies_to: MilestoneApplicability::Any,
            hostility_trigger: true,
            hostility_drop: Some(drop),
        };

        // Affinity milestones.
        specs.insert("first_positive_interaction".into(), mk("first_positive_interaction", 1, MilestoneApplicability::Any));
        specs.insert("shared_drink".into(), mk("shared_drink", 1, MilestoneApplicability::Any));
        specs.insert("shared_downtime".into(), mk("shared_downtime", 2, MilestoneApplicability::Any));
        specs.insert("helped_with_task".into(), mk("helped_with_task", 2, MilestoneApplicability::Any));
        specs.insert("defended_in_combat".into(), mk("defended_in_combat", 3, MilestoneApplicability::IntoTrusted));
        specs.insert("saved_life".into(), mk("saved_life", 3, MilestoneApplicability::IntoTrusted));
        specs.insert("shared_secret".into(), mk("shared_secret", 3, MilestoneApplicability::IntoTrusted));
        specs.insert("long_loyalty".into(), mk("long_loyalty", 3, MilestoneApplicability::IntoBonded));
        specs.insert("sworn_oath".into(), mk("sworn_oath", 3, MilestoneApplicability::IntoBonded));
        specs.insert("risked_death_for".into(), mk("risked_death_for", 3, MilestoneApplicability::IntoBonded));

        // Hostility triggers (bypass time floor + milestone gates).
        specs.insert("betrayed_trust".into(), mk_hostility("betrayed_trust", RelationshipTier::Hostile));
        specs.insert("stole_from".into(), mk_hostility("stole_from", RelationshipTier::Rival));
        specs.insert("inspected_hostile".into(), mk_hostility("inspected_hostile", RelationshipTier::Rival));
        specs.insert("killed_ally".into(), mk_hostility("killed_ally", RelationshipTier::Nemesis));
        specs.insert("killed_family".into(), mk_hostility("killed_family", RelationshipTier::Nemesis));
        specs.insert("razed_home".into(), mk_hostility("razed_home", RelationshipTier::Nemesis));

        MilestoneRegistry { specs }
    }

    /// Look up a spec by event ID.
    pub fn get(&self, event_id: &str) -> Option<&MilestoneSpec> {
        self.specs.get(event_id)
    }

    /// Total accumulated points the player has with this NPC, for events
    /// whose applicability matches a transition into `target_tier`.
    pub fn points_toward(
        &self,
        state: &RelationshipState,
        target_tier: RelationshipTier,
    ) -> u32 {
        state
            .events
            .iter()
            .filter_map(|eid| self.specs.get(eid))
            .filter(|spec| !spec.hostility_trigger)
            .filter(|spec| spec.applies_to.matches(target_tier))
            .map(|spec| spec.points)
            .sum()
    }

    /// If any recorded event is a hostility trigger, return the WORST
    /// (lowest) tier it would drop to. Used by `evaluate_transition` to
    /// short-circuit the gates on a betrayal.
    pub fn worst_hostility_drop(&self, state: &RelationshipState) -> Option<RelationshipTier> {
        state
            .events
            .iter()
            .filter_map(|eid| self.specs.get(eid))
            .filter(|spec| spec.hostility_trigger)
            .filter_map(|spec| spec.hostility_drop)
            .min() // min by enum ordering = worst (most hostile)
    }
}

// ===========================================================================
// The gate logic: time floor + milestone events + hostility short-circuit
// ===========================================================================

/// The required affinity-point threshold to advance INTO each tier. These
/// are the v1 defaults; Wupi-authored card overrides can adjust per-NPC.
const POINTS_THRESHOLD_INTO_FRIENDLY: u32 = 3;
const POINTS_THRESHOLD_INTO_TRUSTED: u32 = 6;
const POINTS_THRESHOLD_INTO_BONDED: u32 = 10;

/// The base time floor (in real-time days, scaled by volatility) before the
/// next affinity transition is allowed. The actual floor for a given NPC is
/// `BASE_TIME_FLOOR_DAYS / volatility` — volatile NPCs befriend faster,
/// patient ones slower. These are deliberately non-trivial so relationships
/// can't be rushed.
const BASE_TIME_FLOOR_DAYS_INTO_FRIENDLY: f64 = 7.0;
const BASE_TIME_FLOOR_DAYS_INTO_TRUSTED: f64 = 21.0;
const BASE_TIME_FLOOR_DAYS_INTO_BONDED: f64 = 60.0;

/// The outcome of a transition evaluation. Tells the caller what (if
/// anything) changed, and why. Pure data — the caller decides whether to
/// apply + how to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// No transition occurred. The NPC remains at its current tier. Carries
    /// the reason so the caller can render the appropriate directive
    /// ("Marcus remains an acquaintance — trust must be earned over time").
    NoTransition { reason: TransitionReason },

    /// A transition occurred. The new tier + the reason it fired. The caller
    /// updates `RelationshipState::tier` + `tier_entered_at_minutes`.
    Transition {
        new_tier: RelationshipTier,
        reason: TransitionReason,
    },
}

/// Why a transition did or didn't occur. Used for narrator directives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionReason {
    /// A hostility trigger fired (betrayal/murder). Bypassed the gates.
    HostilityTriggered,
    /// Both gates cleared: enough time AND enough milestones.
    GatesCleared,
    /// Time floor not yet met — the relationship is too new.
    TimeFloorNotMet,
    /// Milestone threshold not met — not enough diegetic events accumulated.
    MilestoneThresholdNotMet,
    /// The current tier is the ceiling (Bonded on affinity; Nemesis on
    /// hostility). Nothing higher to advance to.
    AtCeiling,
    /// The current tier is on the opposite track from the requested advance
    /// (e.g. trying to advance affinity while on the hostility track).
    /// Reconciliation requires its own milestone path (future work).
    WrongTrackForAdvance,
}

/// Evaluate whether a transition should occur for this NPC's relationship
/// state, given the current world time + the milestone registry.
///
/// The logic:
/// 1. **Hostility short-circuit**: if any recorded event is a hostility
///    trigger, drop to the worst triggered tier (bypassing all gates). This
///    is the asymmetric gravity-of-betrayal design: a murder is a murder on
///    Day 1.
/// 2. **Affinity advance**: if the current tier has a `next_affinity()`,
///    check both gates (time floor + milestone threshold). If BOTH clear,
///    advance. If only one clears, return the relevant `NoTransition` reason
///    so the caller can render the appropriate directive.
/// 3. **Ceiling**: if the current tier is the ceiling (Bonded or Nemesis),
///    return `AtCeiling`.
///
/// `now_minutes` is the WorldClock's current value (the caller threads it
/// in). The time-floor math compares `now − tier_entered_at_minutes` against
/// the threshold scaled by the NPC's volatility.
///
/// PURE FN — no I/O, no locks, no side effects. The caller applies the
/// outcome to `RelationshipState` after deciding to.
pub fn evaluate_transition(
    state: &RelationshipState,
    registry: &MilestoneRegistry,
    now_minutes: i64,
) -> TransitionOutcome {
    // (1) Hostility short-circuit. If a betrayal event has been recorded,
    // drop to the worst triggered tier — no gates apply.
    if let Some(worst_drop) = registry.worst_hostility_drop(state) {
        // Only fire if the worst drop is WORSE than the current tier.
        if worst_drop < state.tier {
            return TransitionOutcome::Transition {
                new_tier: worst_drop,
                reason: TransitionReason::HostilityTriggered,
            };
        }
        // If the current tier is already at or below the worst drop, no
        // further hostility transition fires (you can't double-betray).
    }

    // (2) Hostility track: no upward advance without reconciliation (future
    // work). Return WrongTrackForAdvance so the caller doesn't try.
    if state.tier.is_hostility() {
        return TransitionOutcome::NoTransition {
            reason: TransitionReason::WrongTrackForAdvance,
        };
    }

    // (3) Affinity advance. Find the next tier + check both gates.
    let Some(target_tier) = state.tier.next_affinity() else {
        return TransitionOutcome::NoTransition {
            reason: TransitionReason::AtCeiling,
        };
    };

    // Neutral-target advance (Stranger → Acquaintance — the only neutral
    // hop; (2) above already parked hostility-track states, so the target
    // can never be a hostility tier). Gated on the documented "first
    // positive interaction": at least one recorded non-hostility milestone
    // (2026-08-20 audit M3: the gate was documented but never enforced, so
    // a raw-editor relationship install auto-advanced with zero events).
    if !target_tier.is_affinity() {
        let has_positive = state
            .events
            .iter()
            .filter_map(|eid| registry.get(eid))
            .any(|spec| !spec.hostility_trigger);
        if !has_positive {
            return TransitionOutcome::NoTransition {
                reason: TransitionReason::MilestoneThresholdNotMet,
            };
        }
        return TransitionOutcome::Transition {
            new_tier: target_tier,
            reason: TransitionReason::GatesCleared,
        };
    }

    // Affinity advance (target_tier is Friendly/Trusted/Bonded). Check both gates.
    let (threshold_points, base_floor_days) = match target_tier {
        RelationshipTier::Friendly => (POINTS_THRESHOLD_INTO_FRIENDLY, BASE_TIME_FLOOR_DAYS_INTO_FRIENDLY),
        RelationshipTier::Trusted => (POINTS_THRESHOLD_INTO_TRUSTED, BASE_TIME_FLOOR_DAYS_INTO_TRUSTED),
        RelationshipTier::Bonded => (POINTS_THRESHOLD_INTO_BONDED, BASE_TIME_FLOOR_DAYS_INTO_BONDED),
        _ => unreachable!("target_tier is affinity-track by the check above"),
    };

    // Time floor: scaled by volatility. Volatile NPCs (volatility > 1.0)
    // have a SHORTER floor; patient ones (volatility < 1.0) a longer one.
    // Clamped to the shared [0.1, 3.0] band — an unclamped hand-installed
    // value (e.g. 1000) collapsed the 60-day Bonded floor to ~1.4 hours
    // (2026-08-20 audit M4; the frustration-curve consumers already
    // clamped — unified on one helper).
    let vol = clamp_volatility(state.volatility);
    let floor_days = base_floor_days / vol;
    let floor_minutes = (floor_days * 1440.0) as i64;
    let elapsed = now_minutes - state.tier_entered_at_minutes;
    if elapsed < floor_minutes {
        return TransitionOutcome::NoTransition {
            reason: TransitionReason::TimeFloorNotMet,
        };
    }

    // Milestone threshold: total accumulated points toward this tier.
    let points = registry.points_toward(state, target_tier);
    if points < threshold_points {
        return TransitionOutcome::NoTransition {
            reason: TransitionReason::MilestoneThresholdNotMet,
        };
    }

    // Both gates cleared.
    TransitionOutcome::Transition {
        new_tier: target_tier,
        reason: TransitionReason::GatesCleared,
    }
}

/// Apply a transition outcome to a relationship state. Mutates in place.
/// Sets `tier` to the new value + stamps `tier_entered_at_minutes` to
/// `now_minutes`. No-op for `NoTransition`.
pub fn apply_transition(state: &mut RelationshipState, outcome: &TransitionOutcome, now_minutes: i64) {
    if let TransitionOutcome::Transition { new_tier, .. } = outcome {
        state.tier = *new_tier;
        state.tier_entered_at_minutes = now_minutes;
    }
}

/// (2026-08-23 Playground) The MILESTONE INJECTOR's forced re-evaluation —
/// the god-tool variant of [`evaluate_transition`]: the TIME FLOORS are
/// ignored, while the milestone POINTS threshold still gates (injecting the
/// right events advances, injecting junk doesn't) and both figures are
/// reported so the tool can show "points 4 / threshold 3". The hostility
/// short-circuit, the wrong-track rule, the ceiling, and the
/// Stranger→Acquaintance positive-event gate all apply unchanged. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedTransition {
    pub outcome: TransitionOutcome,
    /// Points accumulated toward the evaluated target tier (0 when no
    /// affinity target was evaluated — hostility drop, wrong track,
    /// ceiling, or the neutral hop).
    pub points: u32,
    /// The threshold the target tier demands (same 0 convention).
    pub threshold: u32,
}

pub fn evaluate_transition_forced(
    state: &RelationshipState,
    registry: &MilestoneRegistry,
    _now_minutes: i64,
) -> ForcedTransition {
    // (1) Hostility short-circuit — identical to the gated path.
    if let Some(worst_drop) = registry.worst_hostility_drop(state) {
        if worst_drop < state.tier {
            return ForcedTransition {
                outcome: TransitionOutcome::Transition {
                    new_tier: worst_drop,
                    reason: TransitionReason::HostilityTriggered,
                },
                points: 0,
                threshold: 0,
            };
        }
    }
    // (2) Hostility track: no upward advance without reconciliation.
    if state.tier.is_hostility() {
        return ForcedTransition {
            outcome: TransitionOutcome::NoTransition {
                reason: TransitionReason::WrongTrackForAdvance,
            },
            points: 0,
            threshold: 0,
        };
    }
    // (3) Affinity advance — the ONLY divergence from the gated path: the
    // time floor never blocks; the points threshold still does.
    let Some(target_tier) = state.tier.next_affinity() else {
        return ForcedTransition {
            outcome: TransitionOutcome::NoTransition {
                reason: TransitionReason::AtCeiling,
            },
            points: 0,
            threshold: 0,
        };
    };
    if !target_tier.is_affinity() {
        // The neutral hop (Stranger → Acquaintance): the positive-event
        // gate applies (a forced re-evaluation is not a license to mint
        // Acquaintances from nothing — inject an event first).
        let has_positive = state
            .events
            .iter()
            .filter_map(|eid| registry.get(eid))
            .any(|spec| !spec.hostility_trigger);
        return ForcedTransition {
            outcome: if has_positive {
                TransitionOutcome::Transition {
                    new_tier: target_tier,
                    reason: TransitionReason::GatesCleared,
                }
            } else {
                TransitionOutcome::NoTransition {
                    reason: TransitionReason::MilestoneThresholdNotMet,
                }
            },
            points: 0,
            threshold: 0,
        };
    }
    let (threshold_points, _base_floor_days) = match target_tier {
        RelationshipTier::Friendly => (POINTS_THRESHOLD_INTO_FRIENDLY, BASE_TIME_FLOOR_DAYS_INTO_FRIENDLY),
        RelationshipTier::Trusted => (POINTS_THRESHOLD_INTO_TRUSTED, BASE_TIME_FLOOR_DAYS_INTO_TRUSTED),
        RelationshipTier::Bonded => (POINTS_THRESHOLD_INTO_BONDED, BASE_TIME_FLOOR_DAYS_INTO_BONDED),
        _ => unreachable!("target_tier is affinity-track by the check above"),
    };
    let points = registry.points_toward(state, target_tier);
    ForcedTransition {
        outcome: if points >= threshold_points {
            TransitionOutcome::Transition {
                new_tier: target_tier,
                reason: TransitionReason::GatesCleared,
            }
        } else {
            TransitionOutcome::NoTransition {
                reason: TransitionReason::MilestoneThresholdNotMet,
            }
        },
        points,
        threshold: threshold_points,
    }
}

/// Build a narrator directive explaining the current relationship stance.
/// The caller wraps this as `[DIRECTIVE: <npc_id> relationship: <directive>]`
/// inside `<world_state>`. The narrator obeys; the LLM never sees the
/// underlying time/points math.
pub fn render_relationship_directive(
    npc_id: &str,
    state: &RelationshipState,
) -> String {
    format!("{} views you as {} — {}", npc_id, state.tier.tag(), state.tier.label())
}

// ===========================================================================
// The silent-drop validator hook (called from the schema validator)
// ===========================================================================

/// The result of validating a relationship-tier mutation the LLM tried to
/// write via the schema-delta path. The validator calls this for any entity
/// key matching the relationship namespace (e.g. `rel.marcus.tier`).
///
/// `Accept` is returned ONLY for transitions the LLM is allowed to make
/// unilaterally — basically just Stranger → Acquaintance (the "first
/// positive interaction" auto-advance, no gates). Every other advance must
/// come through `evaluate_transition` after a Referee-detected milestone
/// event; an LLM attempt to skip the gates is REJECTED (silent-drop policy).
///
/// Rejections do NOT enter the failed_delta_queue (re-running the same
/// prompt won't clear a still-closed gate). The caller drops the entity
/// mutation silently and emits a `[DIRECTIVE: ...]` explaining the actual
/// tier, so the narrator stops trying to escalate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationshipValidation {
    /// The LLM's attempted tier mutation is allowed (rare — only the
    /// no-gate Stranger→Acquaintance auto-advance).
    Accept,
    /// The LLM attempted a gated transition unilaterally. REJECT — silent
    /// drop. Carries the actual current tier so the caller can render the
    /// directive.
    Reject { actual_tier: RelationshipTier },
    /// The LLM wrote a value the validator can't parse as a tier. REJECT —
    /// silent drop (different from a parse failure that WOULD enter the
    /// repair queue; this is a known-keyword rejection).
    Unparseable,
}

/// Validate an LLM-attempted tier write. `attempted_value` is the raw
/// string the schema-delta wrote (e.g. "friendly", "trusted"). `current`
/// is the actual current RelationshipState.
///
/// The accepted transition is Stranger → Acquaintance (the auto-advance on
/// first positive interaction, no gates). Everything else requires the
/// Referee → evaluate_transition path.
pub fn validate_llm_tier_write(
    attempted_value: &str,
    current: &RelationshipState,
) -> RelationshipValidation {
    let attempted = parse_tier(attempted_value);
    let Some(attempted) = attempted else {
        return RelationshipValidation::Unparseable;
    };
    // The one allowed LLM-initiated transition: Stranger → Acquaintance.
    if current.tier == RelationshipTier::Stranger
        && attempted == RelationshipTier::Acquaintance
    {
        return RelationshipValidation::Accept;
    }
    // Anything else is a gate-bypass attempt. Reject.
    RelationshipValidation::Reject {
        actual_tier: current.tier,
    }
}

/// Parse a free-form string as a RelationshipTier. Case-insensitive,
/// tolerant of common synonyms ("friend", "ally"). Returns None for
/// unparseable input.
pub fn parse_tier(s: &str) -> Option<RelationshipTier> {
    let lower = s.trim().to_lowercase();
    match lower.as_str() {
        "nemesis" | "enemy_dead" | "sworn_enemy" => Some(RelationshipTier::Nemesis),
        "hostile" | "enemy" => Some(RelationshipTier::Hostile),
        "rival" | "dislike" | "cold" => Some(RelationshipTier::Rival),
        "stranger" | "neutral" | "unknown" => Some(RelationshipTier::Stranger),
        "acquaintance" | "acquiantance" | "known" => Some(RelationshipTier::Acquaintance),
        "friendly" | "friend" | "ally" | "warm" => Some(RelationshipTier::Friendly),
        "trusted" | "confidant" | "confidante" => Some(RelationshipTier::Trusted),
        "bonded" | "family" | "sworn" | "devoted" => Some(RelationshipTier::Bonded),
        _ => None,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> MilestoneRegistry {
        MilestoneRegistry::defaults()
    }

    fn state_at(tier: RelationshipTier, entered_at: i64) -> RelationshipState {
        RelationshipState {
            tier,
            tier_entered_at_minutes: entered_at,
            events: Vec::new(),
            volatility: 1.0,
        }
    }

    // ---- RelationshipTier basics ----

    #[test]
    fn tier_default_is_stranger() {
        assert_eq!(RelationshipTier::default(), RelationshipTier::Stranger);
    }

    #[test]
    fn tier_ordering_worst_to_best() {
        // Enum-variant order = worst → best. as u8 comparisons must agree.
        assert!(RelationshipTier::Nemesis < RelationshipTier::Hostile);
        assert!(RelationshipTier::Hostile < RelationshipTier::Rival);
        assert!(RelationshipTier::Rival < RelationshipTier::Stranger);
        assert!(RelationshipTier::Stranger < RelationshipTier::Acquaintance);
        assert!(RelationshipTier::Acquaintance < RelationshipTier::Friendly);
        assert!(RelationshipTier::Friendly < RelationshipTier::Trusted);
        assert!(RelationshipTier::Trusted < RelationshipTier::Bonded);
    }

    #[test]
    fn tier_track_classifications() {
        // Affinity track.
        for tier in [RelationshipTier::Friendly, RelationshipTier::Trusted, RelationshipTier::Bonded] {
            assert!(tier.is_affinity(), "{tier:?} should be affinity");
            assert!(!tier.is_hostility());
        }
        // Hostility track.
        for tier in [RelationshipTier::Nemesis, RelationshipTier::Hostile, RelationshipTier::Rival] {
            assert!(tier.is_hostility(), "{tier:?} should be hostility");
            assert!(!tier.is_affinity());
        }
        // Neutral tiers are neither.
        for tier in [RelationshipTier::Stranger, RelationshipTier::Acquaintance] {
            assert!(!tier.is_affinity());
            assert!(!tier.is_hostility());
        }
    }

    #[test]
    fn tier_next_affinity_ladder() {
        assert_eq!(RelationshipTier::Stranger.next_affinity(), Some(RelationshipTier::Acquaintance));
        assert_eq!(RelationshipTier::Acquaintance.next_affinity(), Some(RelationshipTier::Friendly));
        assert_eq!(RelationshipTier::Friendly.next_affinity(), Some(RelationshipTier::Trusted));
        assert_eq!(RelationshipTier::Trusted.next_affinity(), Some(RelationshipTier::Bonded));
        assert_eq!(RelationshipTier::Bonded.next_affinity(), None); // ceiling
        // Hostility → Stranger (reconciliation path).
        assert_eq!(RelationshipTier::Rival.next_affinity(), Some(RelationshipTier::Stranger));
    }

    #[test]
    fn tier_label_and_tag_are_nonempty_distinct() {
        let tiers = [
            RelationshipTier::Nemesis,
            RelationshipTier::Hostile,
            RelationshipTier::Rival,
            RelationshipTier::Stranger,
            RelationshipTier::Acquaintance,
            RelationshipTier::Friendly,
            RelationshipTier::Trusted,
            RelationshipTier::Bonded,
        ];
        let mut labels: Vec<&str> = tiers.iter().map(|t| t.label()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), tiers.len(), "labels must be distinct");
        let mut tags: Vec<&str> = tiers.iter().map(|t| t.tag()).collect();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), tiers.len(), "tags must be distinct");
    }

    // ---- RelationshipState ----

    #[test]
    fn state_default_is_stranger_no_events() {
        let s = RelationshipState::default();
        assert!(s.is_default());
        assert_eq!(s.tier, RelationshipTier::Stranger);
        assert!(s.events.is_empty());
    }

    #[test]
    fn record_event_is_idempotent() {
        let mut s = RelationshipState::default();
        assert!(s.record_event("saved_life"));
        assert!(!s.record_event("saved_life"), "duplicate event must be a no-op");
        assert_eq!(s.events.len(), 1);
        assert!(s.has_event("saved_life"));
    }

    // ---- MilestoneRegistry ----

    #[test]
    fn registry_defaults_loads_known_events() {
        let r = registry();
        assert!(r.get("saved_life").is_some());
        assert!(r.get("betrayed_trust").is_some());
        assert!(r.get("shared_drink").is_some());
        assert!(r.get("nonexistent_event").is_none());
    }

    #[test]
    fn registry_hostility_triggers_flagged() {
        let r = registry();
        assert!(r.get("betrayed_trust").unwrap().hostility_trigger);
        assert!(!r.get("saved_life").unwrap().hostility_trigger);
    }

    #[test]
    fn registry_points_toward_filters_by_applicability() {
        let r = registry();
        // saved_life applies IntoTrusted; helps_with_task applies Any.
        let mut s = state_at(RelationshipTier::Friendly, 0);
        s.record_event("saved_life");
        s.record_event("helped_with_task");
        // Points toward Trusted: both events count (Any + IntoTrusted match Trusted).
        let points_trusted = r.points_toward(&s, RelationshipTier::Trusted);
        assert_eq!(points_trusted, 3 + 2, "saved_life(3) + helped_with_task(2)");
        // Points toward Bonded: only Any-applicable counts (helped_with_task).
        let points_bonded = r.points_toward(&s, RelationshipTier::Bonded);
        assert_eq!(points_bonded, 2, "only helped_with_task counts toward Bonded");
    }

    #[test]
    fn registry_worst_hostility_drop_picks_minimum() {
        let r = registry();
        let mut s = state_at(RelationshipTier::Friendly, 0);
        // Two hostility triggers recorded: Hostile + Nemesis. Worst = Nemesis.
        s.record_event("betrayed_trust"); // Hostile
        s.record_event("killed_ally"); // Nemesis
        let worst = r.worst_hostility_drop(&s).expect("must find a drop");
        assert_eq!(worst, RelationshipTier::Nemesis);
    }

    // ---- evaluate_transition: the dual-keyed gate logic ----

    #[test]
    fn transition_betrayal_short_circuits_gates() {
        // Betrayal drops to Hostile regardless of time or current tier.
        let r = registry();
        let mut s = state_at(RelationshipTier::Friendly, 0);
        s.record_event("betrayed_trust");
        let outcome = evaluate_transition(&s, &r, 0); // now = 0, no time has passed
        assert!(matches!(
            outcome,
            TransitionOutcome::Transition {
                new_tier: RelationshipTier::Hostile,
                reason: TransitionReason::HostilityTriggered
            }
        ));
    }

    #[test]
    fn transition_murder_drops_to_nemesis() {
        let r = registry();
        let mut s = state_at(RelationshipTier::Trusted, 0);
        s.record_event("killed_family");
        let outcome = evaluate_transition(&s, &r, 0);
        assert_eq!(
            outcome,
            TransitionOutcome::Transition {
                new_tier: RelationshipTier::Nemesis,
                reason: TransitionReason::HostilityTriggered
            }
        );
    }

    #[test]
    fn transition_to_friendly_blocked_by_time_floor() {
        // Acquaintance → Friendly requires 7 days. Day 1 = blocked.
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.record_event("shared_drink"); // enough points
        s.record_event("helped_with_task");
        // now = Day 1 (1440 minutes) — way short of the 7-day floor.
        let outcome = evaluate_transition(&s, &r, 1440);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::TimeFloorNotMet
            }
        ));
    }

    #[test]
    fn transition_to_friendly_blocked_by_milestones() {
        // 7+ days elapsed, but only 1 point — needs 3 to clear Friendly.
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.record_event("shared_drink"); // 1 point
        // now = Day 10 (14400 minutes) — past the 7-day floor.
        let outcome = evaluate_transition(&s, &r, 14400);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::MilestoneThresholdNotMet
            }
        ));
    }

    #[test]
    fn transition_to_friendly_clears_both_gates() {
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.record_event("shared_drink"); // 1
        s.record_event("helped_with_task"); // 2
        // Total: 3 points, past the Into-Friendly threshold of 3.
        // now = Day 10 (14400 min) — past the 7-day floor.
        let outcome = evaluate_transition(&s, &r, 14400);
        assert_eq!(
            outcome,
            TransitionOutcome::Transition {
                new_tier: RelationshipTier::Friendly,
                reason: TransitionReason::GatesCleared
            }
        );
    }

    #[test]
    fn transition_volatile_npc_has_shorter_time_floor() {
        // A volatile NPC (coeff 3.0) has a 7/3 ≈ 2.3-day floor instead of 7.
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.volatility = 3.0;
        s.record_event("shared_drink");
        s.record_event("helped_with_task");
        // Day 3 (4320 min) — past the volatile's ~2.3-day floor but short of
        // the patient's 7-day floor.
        let outcome = evaluate_transition(&s, &r, 4320);
        assert!(matches!(
            outcome,
            TransitionOutcome::Transition { new_tier: RelationshipTier::Friendly, .. }
        ), "volatile NPC should clear the shortened time floor: {outcome:?}");
    }

    #[test]
    fn transition_patient_npc_has_longer_time_floor() {
        // A patient NPC (coeff 0.4) has a 7/0.4 = 17.5-day floor.
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.volatility = 0.4;
        s.record_event("shared_drink");
        s.record_event("helped_with_task");
        // Day 10 (14400 min) — short of the 17.5-day floor.
        let outcome = evaluate_transition(&s, &r, 14400);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::TimeFloorNotMet
            }
        ));
    }

    #[test]
    fn stranger_to_acquaintance_requires_positive_event() {
        // M3 (2026-08-20): the documented "first positive interaction" gate,
        // now enforced — a raw-editor install with zero recorded events
        // stays Stranger; any non-hostility milestone clears it.
        let r = registry();
        let s = state_at(RelationshipTier::Stranger, 0);
        let outcome = evaluate_transition(&s, &r, 10_000);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::MilestoneThresholdNotMet
            }
        ));
        let mut with_event = state_at(RelationshipTier::Stranger, 0);
        assert!(with_event.record_event("first_positive_interaction"));
        assert!(matches!(
            evaluate_transition(&with_event, &r, 10_000),
            TransitionOutcome::Transition { reason: TransitionReason::GatesCleared, .. }
        ));
        // A hostility-only record does NOT clear the positive gate.
        let mut betrayed = state_at(RelationshipTier::Stranger, 0);
        assert!(betrayed.record_event("stole_from"));
        assert!(matches!(
            evaluate_transition(&betrayed, &r, 10_000),
            TransitionOutcome::Transition { reason: TransitionReason::HostilityTriggered, .. }
        ));
    }

    #[test]
    fn volatility_is_clamped_to_design_band() {
        // M4 (2026-08-20): an unclamped 1000 collapsed the 60-day Bonded
        // floor to ~1.4 hours; the clamp caps it at 3.0 (~20 days).
        assert_eq!(clamp_volatility(1000.0), 3.0);
        assert_eq!(clamp_volatility(0.0), 1.0);
        assert_eq!(clamp_volatility(-5.0), 1.0);
        assert_eq!(clamp_volatility(f64::NAN), 1.0);
        assert_eq!(clamp_volatility(0.4), 0.4);
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.volatility = 1000.0;
        s.record_event("shared_drink");
        s.record_event("helped_with_task");
        // Day 3 — past the unclamped 1000× floor (~10 min) but short of the
        // clamped 3× floor (~2.3 days): still blocked.
        let outcome = evaluate_transition(&s, &r, 4320);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::TimeFloorNotMet
            }
        ));
    }

    #[test]
    fn transition_bonded_is_ceiling() {
        let r = registry();
        let s = state_at(RelationshipTier::Bonded, 0);
        let outcome = evaluate_transition(&s, &r, 9_999_999);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::AtCeiling
            }
        ));
    }

    #[test]
    fn transition_hostility_track_blocks_affinity_advance() {
        // A Rival NPC can't advance affinity without reconciliation.
        let r = registry();
        let s = state_at(RelationshipTier::Rival, 0);
        let outcome = evaluate_transition(&s, &r, 9_999_999);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::WrongTrackForAdvance
            }
        ));
    }

    // ---- apply_transition ----

    #[test]
    fn apply_transition_updates_tier_and_timestamp() {
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.record_event("shared_drink");
        s.record_event("helped_with_task");
        let outcome = evaluate_transition(&s, &r, 14400);
        apply_transition(&mut s, &outcome, 14400);
        assert_eq!(s.tier, RelationshipTier::Friendly);
        assert_eq!(s.tier_entered_at_minutes, 14400);
    }

    #[test]
    fn apply_transition_noop_on_no_transition() {
        let mut s = state_at(RelationshipTier::Stranger, 1000);
        let outcome = TransitionOutcome::NoTransition {
            reason: TransitionReason::TimeFloorNotMet,
        };
        apply_transition(&mut s, &outcome, 9999);
        // Tier unchanged, timestamp unchanged.
        assert_eq!(s.tier, RelationshipTier::Stranger);
        assert_eq!(s.tier_entered_at_minutes, 1000);
    }

    // ---- (2026-08-23 Playground) forced transition ----

    #[test]
    fn forced_transition_ignores_time_floor_keeps_points_gate() {
        // The gated path blocks Acquaintance → Friendly on Day 1 (the
        // 7-day floor). The FORCED path advances the same state at minute 0
        // — but only with enough points: 1 point still refuses, 3 clears.
        let r = registry();
        let mut one_point = state_at(RelationshipTier::Acquaintance, 0);
        one_point.record_event("shared_drink");
        let forced = evaluate_transition_forced(&one_point, &r, 0);
        assert!(matches!(
            forced.outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::MilestoneThresholdNotMet
            }
        ));
        assert_eq!((forced.points, forced.threshold), (1, 3), "points computed + reported");

        let mut three_points = one_point.clone();
        three_points.record_event("helped_with_task");
        let forced = evaluate_transition_forced(&three_points, &r, 0);
        assert!(matches!(
            forced.outcome,
            TransitionOutcome::Transition { new_tier: RelationshipTier::Friendly, .. }
        ));
        assert_eq!((forced.points, forced.threshold), (3, 3));
        // The apply path is the shared one.
        let mut applied = three_points.clone();
        apply_transition(&mut applied, &forced.outcome, 0);
        assert_eq!(applied.tier, RelationshipTier::Friendly);
    }

    #[test]
    fn forced_transition_keeps_hostility_and_track_rules() {
        let r = registry();
        // A betrayal still short-circuits instantly (no gates to force).
        let mut betrayed = state_at(RelationshipTier::Trusted, 0);
        betrayed.record_event("betrayed_trust");
        let forced = evaluate_transition_forced(&betrayed, &r, 0);
        assert!(matches!(
            forced.outcome,
            TransitionOutcome::Transition { new_tier: RelationshipTier::Hostile, .. }
        ));
        assert_eq!((forced.points, forced.threshold), (0, 0));
        // The hostility track still refuses affinity advances.
        let rival = state_at(RelationshipTier::Rival, 0);
        assert!(matches!(
            evaluate_transition_forced(&rival, &r, 9_999_999).outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::WrongTrackForAdvance
            }
        ));
        // Bonded is still the ceiling.
        let bonded = state_at(RelationshipTier::Bonded, 0);
        assert!(matches!(
            evaluate_transition_forced(&bonded, &r, 9_999_999).outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::AtCeiling
            }
        ));
        // The neutral hop still needs one positive event.
        let bare = state_at(RelationshipTier::Stranger, 0);
        assert!(matches!(
            evaluate_transition_forced(&bare, &r, 9_999_999).outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::MilestoneThresholdNotMet
            }
        ));
    }

    // ---- Silent-drop validator hook ----

    #[test]
    fn validate_accepts_stranger_to_acquaintance() {
        // The one allowed LLM-initiated transition: Stranger → Acquaintance
        // (the auto-advance on first positive interaction, no gates).
        let s = state_at(RelationshipTier::Stranger, 0);
        assert_eq!(
            validate_llm_tier_write("acquaintance", &s),
            RelationshipValidation::Accept
        );
    }

    #[test]
    fn validate_rejects_unearned_escalation_to_friendly() {
        // The LLM tries to write "friendly" while still at Acquaintance.
        // REJECT (silent drop) — Friendly requires both gates.
        let s = state_at(RelationshipTier::Acquaintance, 0);
        assert_eq!(
            validate_llm_tier_write("friendly", &s),
            RelationshipValidation::Reject {
                actual_tier: RelationshipTier::Acquaintance
            }
        );
    }

    #[test]
    fn validate_rejects_unearned_escalation_to_trusted() {
        // Even from Friendly, LLM can't write Trusted — needs the gates.
        let s = state_at(RelationshipTier::Friendly, 0);
        assert_eq!(
            validate_llm_tier_write("trusted", &s),
            RelationshipValidation::Reject {
                actual_tier: RelationshipTier::Friendly
            }
        );
    }

    #[test]
    fn validate_unparseable_value_rejected() {
        let s = state_at(RelationshipTier::Stranger, 0);
        assert_eq!(
            validate_llm_tier_write("besties_forever_xoxo", &s),
            RelationshipValidation::Unparseable
        );
    }

    #[test]
    fn validate_rejects_attempted_demotion_via_llm() {
        // The LLM tries to write "hostile" — hostility transitions must come
        // from a Referee-detected betrayal event, not an LLM write.
        let s = state_at(RelationshipTier::Friendly, 0);
        assert!(matches!(
            validate_llm_tier_write("hostile", &s),
            RelationshipValidation::Reject { .. }
        ));
    }

    // ---- parse_tier ----

    #[test]
    fn parse_tier_accepts_synonyms() {
        assert_eq!(parse_tier("friend"), Some(RelationshipTier::Friendly));
        assert_eq!(parse_tier("ALLY"), Some(RelationshipTier::Friendly));
        assert_eq!(parse_tier("enemy"), Some(RelationshipTier::Hostile));
        assert_eq!(parse_tier("family"), Some(RelationshipTier::Bonded));
        assert_eq!(parse_tier(" neutral "), Some(RelationshipTier::Stranger));
        assert_eq!(parse_tier("garbage"), None);
        assert_eq!(parse_tier(""), None);
    }

    // ---- render_relationship_directive ----

    #[test]
    fn render_directive_reads_as_prose() {
        let s = state_at(RelationshipTier::Friendly, 0);
        let d = render_relationship_directive("Marcus", &s);
        assert!(d.contains("Marcus"));
        assert!(d.contains("friendly"));
        // The label must read as a phrase, not a stat.
        assert!(d.contains("warm"));
    }

    // ---- Architect-defining scenarios (pin them as regression tests) ----

    #[test]
    fn architect_shiny_sword_does_not_skip_gates() {
        // The original Gemini-scenario: player gives Marcus a shiny sword on
        // Day 1. The LLM schema-delta tries {"rel.marcus.tier": "friendly"}.
        // Rust REJECTS — the time floor (7 days) hasn't cleared, and there's
        // only one milestone event. Marcus stays Acquaintance.
        let r = registry();
        let mut s = state_at(RelationshipTier::Acquaintance, 0);
        s.record_event("helped_with_task"); // the shiny-sword gift = 2 points
        // now = Day 1 (1440 min) — way short of the 7-day floor.
        let outcome = evaluate_transition(&s, &r, 1440);
        assert!(matches!(
            outcome,
            TransitionOutcome::NoTransition {
                reason: TransitionReason::TimeFloorNotMet
            }
        ));
        // The LLM write attempt would also be rejected:
        let llm_validation = validate_llm_tier_write("friendly", &s);
        assert!(matches!(llm_validation, RelationshipValidation::Reject { .. }));
        // Marcus stays Acquaintance; the narrator renders the directive.
        let d = render_relationship_directive("Marcus", &s);
        assert!(d.contains("acquaintance"));
    }

    #[test]
    fn architect_betrayal_drops_instantly() {
        // The gravity-of-betrayal asymmetry: even on Day 1, even at Trusted,
        // a single betrayed_trust event drops Marcus to Hostile — no time
        // floor, no milestone gate.
        let r = registry();
        let mut s = state_at(RelationshipTier::Trusted, 0);
        s.record_event("betrayed_trust");
        let outcome = evaluate_transition(&s, &r, 0); // Day 0
        assert_eq!(
            outcome,
            TransitionOutcome::Transition {
                new_tier: RelationshipTier::Hostile,
                reason: TransitionReason::HostilityTriggered
            }
        );
    }

    #[test]
    fn architect_murder_drops_to_nemesis_regardless_of_bond() {
        // Even a Bonded NPC turns Nemesis on a killed_family event.
        let r = registry();
        let mut s = state_at(RelationshipTier::Bonded, 0);
        s.record_event("killed_family");
        let outcome = evaluate_transition(&s, &r, 0);
        assert_eq!(
            outcome,
            TransitionOutcome::Transition {
                new_tier: RelationshipTier::Nemesis,
                reason: TransitionReason::HostilityTriggered
            }
        );
    }
}
