//! Player State engine — the Rust Referee (Fable Seam #7, brought forward).
//!
//! The LLM does ZERO math. This module is the sole authority over the
//! protagonist's body, stamina, wealth, and reputation. It rolls the dice,
//! computes the entropy, and renders SEMANTIC FACTS ("left arm: Medium
//! Injury; stamina: Winded") that the narrator reads as hard truth and
//! writes prose to match. The narrator cannot mutate this state — it can
//! only read the injected `<player_state>` block.
//!
//! # Architecture
//!
//! - **Canonical state lives in [`PlayerState`]**, nested inside
//!   `schema::WorldSchema` (NOT a separate AppState field or file). This
//!   gives free per-card persistence via the existing `WorldSchema::save`
//!   + `SaveFile` autosave/explicit-save paths — zero new plumbing.
//! - **The Referee ([`referee_evaluate`])** is a pure fn over the player's
//!   turn text. Heuristic keyword match → mocked dice roll → outcome. It
//!   fires once per `fable_send` turn, BEFORE the world-state render, so
//!   the new injury lands in the same `<world_state>` injection.
//! - **Mocked RNG** ([`Roller`]) is a tiny std-only xorshift seeded from
//!   the turn text. Deterministic per turn (testable); swapping in a real
//!   CSPRNG later is a one-line change to the seed source.
//!
//! # Why enums, not stringly-typed entity keys
//!
//! `WorldSchema::entities` is `HashMap<String, String>` — perfect for
//! LLM-driven, free-form world detail, but the wrong shape for compile-
//! checked player math. A 16-part body × 6-state mannequin + 5-state
//! stamina wants real types so dice rolls + state transitions are
//! verified at compile time, and so the LLM delta path can NEVER corrupt
//! canonical player state (it doesn't flow through `SchemaDelta`).

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Body part state (the mannequin color states)
// ---------------------------------------------------------------------------

/// The injury/health state of a single body part. Maps 1:1 to the mannequin
/// color states in the spec:
///
/// | Variant     | Color        | Meaning                          |
/// |-------------|--------------|----------------------------------|
/// | `Transparent` | transparent  | Healthy (the default)          |
/// | `Yellow`     | yellow       | Minor Injury                    |
/// | `Orange`     | orange       | Medium Injury                   |
/// | `Red`        | red          | Heavy Injury                    |
/// | `Purple`     | purple       | Critical Condition              |
/// | `Black`      | black        | Amputated / gone / decapitated  |
///
/// Serialization is lowercased kebab (serde default for `Transparent`).
/// The frontend's mannequin renderer reads these strings directly as the
/// CSS color class.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
pub enum BodyPartState {
    Transparent,
    Yellow,
    Orange,
    Red,
    Purple,
    Black,
}

impl Default for BodyPartState {
    fn default() -> Self {
        BodyPartState::Transparent
    }
}

impl BodyPartState {
    /// Human-readable label for prompt injection + UI tooltips.
    /// "Healthy" is the prose form of `Transparent` (the user-facing word).
    pub fn semantic(&self) -> &'static str {
        match self {
            BodyPartState::Transparent => "Healthy",
            BodyPartState::Yellow => "Minor Injury",
            BodyPartState::Orange => "Medium Injury",
            BodyPartState::Red => "Heavy Injury",
            BodyPartState::Purple => "Critical Condition",
            BodyPartState::Black => "Amputated",
        }
    }

    /// True when this part can still take a new injury. Amputated (`Black`)
    /// parts are off the table — you can't re-injure a missing limb. Healthy
    /// parts always can; injured parts can be worsened.
    pub fn can_be_injured(&self) -> bool {
        !matches!(self, BodyPartState::Black)
    }

    /// Severity rank 0..=5, used by the Referee to refuse "downgrades"
    /// (a Heavy blow shouldn't randomly become Minor). Higher = worse.
    fn rank(&self) -> u8 {
        match self {
            BodyPartState::Transparent => 0,
            BodyPartState::Yellow => 1,
            BodyPartState::Orange => 2,
            BodyPartState::Red => 3,
            BodyPartState::Purple => 4,
            BodyPartState::Black => 5,
        }
    }
}

// ---------------------------------------------------------------------------
// Stamina
// ---------------------------------------------------------------------------

/// The protagonist's energy level. A 5-step ordinal, NOT a number — the UI
/// renders pips, the prompt gets the semantic word. Drains on exertion
/// (combat, running, climbing); recovers on rest (future: a `rest` keyword).
///
/// Ordering is load-bearing: variants are declared worst→best so
/// `as u8` comparisons work for the drain cap (see [`Stamina::drain`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize, Debug)]
pub enum Stamina {
    Depleted,
    Exhausted,
    Winded,
    Active,
    Fresh,
}

impl Default for Stamina {
    fn default() -> Self {
        Stamina::Fresh
    }
}

impl Stamina {
    pub fn semantic(&self) -> &'static str {
        match self {
            Stamina::Fresh => "Fresh",
            Stamina::Active => "Active",
            Stamina::Winded => "Winded",
            Stamina::Exhausted => "Exhausted",
            Stamina::Depleted => "Depleted",
        }
    }

    /// Drain one step toward `Depleted`, never wrapping past the floor.
    /// Combat/exertion costs one step; the Referee calls this on every
    /// fired outcome. Stops at `Depleted` (the absolute floor — the
    /// protagonist collapses rather than dying of stamina).
    pub fn drain(&mut self) {
        *self = match self {
            Stamina::Fresh => Stamina::Active,
            Stamina::Active => Stamina::Winded,
            Stamina::Winded => Stamina::Exhausted,
            Stamina::Exhausted | Stamina::Depleted => Stamina::Depleted,
        };
    }
}

// ---------------------------------------------------------------------------
// Body parts (the 16 from the spec)
// ---------------------------------------------------------------------------

/// The 16 mannequin body parts. The `id()` is the stable wire format
/// (`"left_bicep"`) used in JSON + prompt; `display()` is the UI label
/// (`"Left Bicep"`). Order is anatomical, head→foot, left before right
/// within each pair — this is the iteration order the mannequin renderer
/// + the prompt's injury list use.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Debug)]
pub enum BodyPart {
    Head,
    Torso,
    LeftBicep,
    LeftForearm,
    LeftHand,
    RightBicep,
    RightForearm,
    RightHand,
    LeftThigh,
    LeftCalf,
    LeftAnkle,
    LeftFoot,
    RightThigh,
    RightCalf,
    RightAnkle,
    RightFoot,
}

impl BodyPart {
    /// All 16 parts in canonical (anatomical) order.
    pub fn all() -> &'static [BodyPart] {
        &[
            BodyPart::Head,
            BodyPart::Torso,
            BodyPart::LeftBicep,
            BodyPart::LeftForearm,
            BodyPart::LeftHand,
            BodyPart::RightBicep,
            BodyPart::RightForearm,
            BodyPart::RightHand,
            BodyPart::LeftThigh,
            BodyPart::LeftCalf,
            BodyPart::LeftAnkle,
            BodyPart::LeftFoot,
            BodyPart::RightThigh,
            BodyPart::RightCalf,
            BodyPart::RightAnkle,
            BodyPart::RightFoot,
        ]
    }

    /// Stable wire id (`"left_bicep"`). Used in JSON + the prompt's injury
    /// list. Lowercase snake_case so it survives any case-folding.
    pub fn id(&self) -> &'static str {
        match self {
            BodyPart::Head => "head",
            BodyPart::Torso => "torso",
            BodyPart::LeftBicep => "left_bicep",
            BodyPart::LeftForearm => "left_forearm",
            BodyPart::LeftHand => "left_hand",
            BodyPart::RightBicep => "right_bicep",
            BodyPart::RightForearm => "right_forearm",
            BodyPart::RightHand => "right_hand",
            BodyPart::LeftThigh => "left_thigh",
            BodyPart::LeftCalf => "left_calf",
            BodyPart::LeftAnkle => "left_ankle",
            BodyPart::LeftFoot => "left_foot",
            BodyPart::RightThigh => "right_thigh",
            BodyPart::RightCalf => "right_calf",
            BodyPart::RightAnkle => "right_ankle",
            BodyPart::RightFoot => "right_foot",
        }
    }

    /// UI label (`"Left Bicep"`). Title-case with spaces.
    pub fn display(&self) -> &'static str {
        match self {
            BodyPart::Head => "Head",
            BodyPart::Torso => "Torso",
            BodyPart::LeftBicep => "Left Bicep",
            BodyPart::LeftForearm => "Left Forearm",
            BodyPart::LeftHand => "Left Hand",
            BodyPart::RightBicep => "Right Bicep",
            BodyPart::RightForearm => "Right Forearm",
            BodyPart::RightHand => "Right Hand",
            BodyPart::LeftThigh => "Left Thigh",
            BodyPart::LeftCalf => "Left Calf",
            BodyPart::LeftAnkle => "Left Ankle",
            BodyPart::LeftFoot => "Left Foot",
            BodyPart::RightThigh => "Right Thigh",
            BodyPart::RightCalf => "Right Calf",
            BodyPart::RightAnkle => "Right Ankle",
            BodyPart::RightFoot => "Right Foot",
        }
    }
}

// ---------------------------------------------------------------------------
// PlayerState (the persisted canonical state)
// ---------------------------------------------------------------------------

/// The protagonist's canonical state. Rust is the SOLE authority — the
/// narrator LLM never writes here, only reads the rendered `<player_state>`
/// block. Nested inside `WorldSchema` so it persists for free per-card.
///
/// `body` defaults to all-`Transparent` (Healthy); `stamina` defaults to
/// `Fresh`. Wealth + reputation are numeric, Rust-owned, and never shown
/// raw to the user (the UI renders them via semantic formatting later).
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct PlayerState {
    #[serde(default)]
    pub body: HashMap<BodyPart, BodyPartState>,

    #[serde(default)]
    pub stamina: Stamina,

    /// Coin / gold / credits. Numeric; the UI formats it. Default 0.
    #[serde(default)]
    pub wealth: u32,

    /// Standing in the world. Signed: negative = infamy, positive = renown.
    /// Default 0.
    #[serde(default)]
    pub reputation: i32,
}

impl Default for PlayerState {
    fn default() -> Self {
        // Seed every body part to Healthy explicitly. HashMap::default() is
        // empty, which would read as "no body" — we want "fully healthy
        // body" so the mannequin renders correctly + referee_injureable
        // has the full part list to pick from.
        let mut body = HashMap::with_capacity(16);
        for part in BodyPart::all() {
            body.insert(*part, BodyPartState::Transparent);
        }
        PlayerState {
            body,
            stamina: Stamina::Fresh,
            wealth: 0,
            reputation: 0,
        }
    }
}

impl PlayerState {
    /// True when the state is the fresh-default (no injuries, full stamina,
    /// zero wealth/reputation). Used to OMIT the `<player_state>` block
    /// entirely on a brand-new game — same empty-skip pattern as
    /// `WorldSchema::render_for_prompt`.
    pub fn is_default(&self) -> bool {
        self.stamina == Stamina::Fresh
            && self.wealth == 0
            && self.reputation == 0
            && self.body.values().all(|s| *s == BodyPartState::Transparent)
    }

    /// Render the semantic block injected into the narrator prompt. Returns
    /// `None` when fully default (so the caller emits no block). Tight +
    /// line-oriented: every token is prefill cost.
    ///
    /// Format (when non-default):
    /// ```text
    /// stamina: Winded
    /// injuries: Left Bicep (Medium Injury), Right Thigh (Heavy Injury)
    /// amputated: Left Hand
    /// wealth: 12
    /// reputation: -3
    /// ```
    /// Lines are omitted when empty (no injuries → no `injuries:` line).
    /// This is the fact block the narrator reads as hard truth.
    pub fn render_for_prompt(&self) -> Option<String> {
        if self.is_default() {
            return None;
        }

        let mut lines: Vec<String> = Vec::with_capacity(5);

        // Stamina always (when non-default state); the model needs to know
        // fatigue even at full health if injured.
        lines.push(format!("stamina: {}", self.stamina.semantic()));

        // Injuries: any part not Healthy AND not Amputated, in anatomical order.
        let injuries: Vec<String> = BodyPart::all()
            .iter()
            .filter_map(|p| {
                let state = self.body.get(p).copied().unwrap_or_default();
                match state {
                    BodyPartState::Transparent
                    | BodyPartState::Black => None,
                    _ => Some(format!("{} ({})", p.display(), state.semantic())),
                }
            })
            .collect();
        if !injuries.is_empty() {
            lines.push(format!("injuries: {}", injuries.join(", ")));
        }

        // Amputated parts get their own line — distinct semantic ("gone",
        // not "injured") the narrator must respect absolutely.
        let amputated: Vec<&str> = BodyPart::all()
            .iter()
            .filter_map(|p| {
                let state = self.body.get(p).copied().unwrap_or_default();
                if state == BodyPartState::Black {
                    Some(p.display())
                } else {
                    None
                }
            })
            .collect();
        if !amputated.is_empty() {
            lines.push(format!("amputated: {}", amputated.join(", ")));
        }

        // Wealth + reputation: only when non-zero. These are background
        // facts; the narrator weaves them in, doesn't dwell.
        if self.wealth != 0 {
            lines.push(format!("wealth: {}", self.wealth));
        }
        if self.reputation != 0 {
            lines.push(format!("reputation: {}", self.reputation));
        }

        Some(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// The Referee: heuristic keyword detection + dice roll
// ---------------------------------------------------------------------------

/// The outcome of a Referee evaluation. Returned when the player's turn
/// text triggered a combat/exertion event; `None`-equivalent (via
/// [`referee_evaluate`] returning `Option`) when no keyword matched.
///
/// `narrative_hint` is a short prose-seed the caller MAY inject alongside
/// the world-state block ("your left arm takes a heavy blow"). The narrator
/// is NOT required to use it — the canonical fact is the body-state change
/// itself; the hint is just a nudge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefereeOutcome {
    pub part: BodyPart,
    pub new_state: BodyPartState,
    pub stamina_after: Stamina,
    /// Short, second-person prose seed. Empty when the change was stamina-only.
    pub narrative_hint: String,
}

/// Combat / exertion keywords that trigger a Referee roll. Matched as
/// whole-word, case-insensitive substrings of the player's turn text.
/// Conservative: short, action-verb list. False-negative cost (missed roll)
/// is one less injury; false-positive cost (rolled on "I walk to the bar")
/// is a spurious wound. Walking/chatting/looking never triggers.
const COMBAT_KEYWORDS: &[&str] = &[
    "attack", "swing", "strike", "slash", "stab", "punch", "kick", "block", "dodge",
    "parry", "shoot", "fire", "cast", "throw", "tackle", "grapple", "charge",
    "run", "sprint", "climb", "jump", "leap", "swim",
];

/// A tiny xorshift RNG. std-only (no new crate). Seeded from the turn text
/// so each turn is deterministic for testing; swap for a real RNG later by
/// changing only the seed source + the `fn next_u32`.
pub struct Roller {
    state: u64,
}

impl Roller {
    /// Seed from any u64. The Referee seeds with a hash of the turn text +
    /// the current injury count so two "I attack" turns in a row produce
    /// different rolls (otherwise the same text → same roll every time).
    pub fn new(seed: u64) -> Self {
        // Avoid the all-zero state (xorshift collapses to 0).
        Roller {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    /// xorshift64. One step. Returns a uniformly-distributed u32 (top bits
    /// are higher-quality in xorshift, so we take them).
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        (x >> 32) as u32
    }

    /// Uniform index in `0..n`. Returns 0 when n == 0 (defensive).
    pub fn range(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u32() as usize) % n
    }

    /// Roll against a weighted table. `weights[i]` is the relative weight
    /// of outcome `i`. Returns the index of the chosen outcome. Sums the
    /// weights internally; panics on empty weights (caller bug).
    pub fn weighted(&mut self, weights: &[u32]) -> usize {
        let total: u64 = weights.iter().map(|&w| w as u64).sum();
        assert!(total > 0, "weighted(): empty weights");
        let mut roll = (self.next_u32() as u64) % total;
        for (i, &w) in weights.iter().enumerate() {
            if roll < w as u64 {
                return i;
            }
            roll -= w as u64;
        }
        0 // unreachable; defensive
    }
}

/// FNV-1a 64-bit hash of a string. Used to seed the Roller deterministically
/// from the turn text. Not cryptographic; just well-distributed.
fn hash_text(s: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// The Referee entry point. Pure fn — no I/O, no locks, no side effects.
/// Scans `text` for combat/exertion keywords; if matched, rolls the dice
/// against the current player state and returns the outcome.
///
/// Returns `None` when:
/// - no keyword matched (the turn was social/exploratory), OR
/// - the protagonist is already `Depleted` AND fully amputated (no body
///   part left to injure — the dice have nothing left to say).
///
/// The caller (`fable_send`) applies the outcome via [`PlayerState`]'s
/// mutation helpers and then renders. This fn does NOT mutate.
pub fn referee_evaluate(text: &str, state: &PlayerState) -> Option<RefereeOutcome> {
    let lower = text.to_lowercase();
    let triggered = COMBAT_KEYWORDS.iter().any(|kw| lower.contains(kw));
    if !triggered {
        return None;
    }

    // Seed from the text + current injury count so back-to-back identical
    // turns roll differently (the count changes after the first applies).
    let injury_count = state
        .body
        .values()
        .filter(|s| s.can_be_injured() && **s != BodyPartState::Transparent)
        .count();
    let seed = hash_text(text).wrapping_add(injury_count as u64);
    let mut roller = Roller::new(seed);

    // Pick a body part to injure. Pool = non-amputated parts (you can't
    // re-injure a missing limb). If everything's amputated, bail.
    let candidates: Vec<BodyPart> = BodyPart::all()
        .iter()
        .copied()
        .filter(|p| state.body.get(p).copied().unwrap_or_default().can_be_injured())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let part = candidates[roller.range(candidates.len())];
    let current_state = state.body.get(&part).copied().unwrap_or_default();

    // Roll severity on a weighted table. Weights favor Minor; crits are rare.
    // Index maps to: 0=Yellow, 1=Orange, 2=Red, 3=Purple.
    const SEVERITY_WEIGHTS: [u32; 4] = [50, 30, 15, 5];
    const SEVERITY_TABLE: [BodyPartState; 4] = [
        BodyPartState::Yellow,
        BodyPartState::Orange,
        BodyPartState::Red,
        BodyPartState::Purple,
    ];
    let roll_idx = roller.weighted(&SEVERITY_WEIGHTS);
    let mut new_state = SEVERITY_TABLE[roll_idx];

    // The new state must be at least as severe as the current one — a Heavy
    // blow to an already-Heavy part shouldn't randomly downgrade to Minor.
    // If the roll is lighter than current, escalate by one tier instead
    // (the blow still did *something*).
    if new_state.rank() < current_state.rank() {
        new_state = match current_state {
            BodyPartState::Transparent => BodyPartState::Yellow,
            BodyPartState::Yellow => BodyPartState::Orange,
            BodyPartState::Orange => BodyPartState::Red,
            BodyPartState::Red => BodyPartState::Purple,
            BodyPartState::Purple | BodyPartState::Black => BodyPartState::Black,
        };
    }

    // Stamina always drains on a combat turn. The caller applies this too.
    let mut stamina_after = state.stamina;
    stamina_after.drain();

    // Narrative hint: a short second-person seed. The narrator reads the
    // canonical body-state change as hard fact; this hint just nudges prose.
    let narrative_hint = format!(
        "your {} takes a {}",
        part.display().to_lowercase(),
        new_state.semantic().to_lowercase(),
    );

    Some(RefereeOutcome {
        part,
        new_state,
        stamina_after,
        narrative_hint,
    })
}

/// Apply a Referee outcome to a PlayerState. Mutates in place. Separate from
/// `referee_evaluate` (which is pure) so the caller controls WHEN state
/// mutates — typically right before the prompt render, inside the schema
/// lock, so the persisted state + the injected state are the same.
pub fn apply_outcome(state: &mut PlayerState, outcome: &RefereeOutcome) {
    state.body.insert(outcome.part, outcome.new_state);
    state.stamina = outcome.stamina_after;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_state() -> PlayerState {
        PlayerState::default()
    }

    // --- enum basics ---

    #[test]
    fn body_part_state_default_is_transparent() {
        assert_eq!(BodyPartState::default(), BodyPartState::Transparent);
    }

    #[test]
    fn body_part_state_semantic_covers_all_variants() {
        // Catches the "added a variant, forgot semantic()" bug.
        assert_eq!(BodyPartState::Transparent.semantic(), "Healthy");
        assert_eq!(BodyPartState::Yellow.semantic(), "Minor Injury");
        assert_eq!(BodyPartState::Orange.semantic(), "Medium Injury");
        assert_eq!(BodyPartState::Red.semantic(), "Heavy Injury");
        assert_eq!(BodyPartState::Purple.semantic(), "Critical Condition");
        assert_eq!(BodyPartState::Black.semantic(), "Amputated");
    }

    #[test]
    fn body_part_state_can_be_injured() {
        assert!(BodyPartState::Transparent.can_be_injured());
        assert!(BodyPartState::Yellow.can_be_injured());
        assert!(BodyPartState::Red.can_be_injured());
        assert!(BodyPartState::Purple.can_be_injured());
        assert!(!BodyPartState::Black.can_be_injured(), "amputated cannot be re-injured");
    }

    #[test]
    fn body_part_all_has_16_in_anatomical_order() {
        let all = BodyPart::all();
        assert_eq!(all.len(), 16, "spec mandates exactly 16 body parts");
        assert_eq!(all[0], BodyPart::Head, "head first");
        assert_eq!(all[1], BodyPart::Torso, "torso second");
        // Left before right within a pair (the spec order).
        assert_eq!(all[2], BodyPart::LeftBicep);
        assert_eq!(all[5], BodyPart::RightBicep);
        assert_eq!(all[15], BodyPart::RightFoot, "right foot last");
    }

    #[test]
    fn body_part_id_and_display_round_trip() {
        for part in BodyPart::all() {
            assert!(!part.id().is_empty());
            assert!(!part.display().is_empty());
            assert_ne!(part.id(), part.display(), "id and display must differ");
        }
    }

    // --- stamina ---

    #[test]
    fn stamina_default_is_fresh() {
        assert_eq!(Stamina::default(), Stamina::Fresh);
    }

    #[test]
    fn stamina_drain_steps_one_at_a_time_to_depleted() {
        let mut s = Stamina::Fresh;
        assert_eq!(s.semantic(), "Fresh");
        s.drain();
        assert_eq!(s, Stamina::Active);
        s.drain();
        assert_eq!(s, Stamina::Winded);
        s.drain();
        assert_eq!(s, Stamina::Exhausted);
        s.drain();
        assert_eq!(s, Stamina::Depleted);
        // Floor: never wraps past Depleted.
        s.drain();
        assert_eq!(s, Stamina::Depleted, "stamina never wraps past Depleted");
    }

    // --- PlayerState ---

    #[test]
    fn player_state_default_is_fully_healthy() {
        let s = fresh_state();
        assert!(s.is_default(), "fresh state must be default");
        assert_eq!(s.stamina, Stamina::Fresh);
        assert_eq!(s.body.len(), 16);
        for part in BodyPart::all() {
            assert_eq!(
                s.body.get(part).copied().unwrap_or_default(),
                BodyPartState::Transparent,
                "{} should be Healthy by default",
                part.display(),
            );
        }
    }

    #[test]
    fn player_state_render_none_when_default() {
        let s = fresh_state();
        assert_eq!(s.render_for_prompt(), None);
    }

    #[test]
    fn player_state_render_some_when_injured() {
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftBicep, BodyPartState::Orange);
        s.stamina = Stamina::Winded;
        let rendered = s.render_for_prompt().expect("non-default renders");
        assert!(rendered.contains("stamina: Winded"));
        assert!(rendered.contains("injuries: Left Bicep (Medium Injury)"));
        // No amputated line when none amputated.
        assert!(!rendered.contains("amputated:"));
    }

    #[test]
    fn player_state_render_lists_amputated_separately() {
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftHand, BodyPartState::Black);
        s.body.insert(BodyPart::RightThigh, BodyPartState::Red);
        let rendered = s.render_for_prompt().expect("non-default renders");
        // Injuries line excludes the amputated part.
        assert!(rendered.contains("injuries: Right Thigh (Heavy Injury)"));
        assert!(!rendered.contains("Left Hand (Amputated)"));
        // Amputated gets its own line.
        assert!(rendered.contains("amputated: Left Hand"));
    }

    #[test]
    fn player_state_render_omits_stamina_only_changes_correctly() {
        // Stamina change alone (no injuries) is still non-default → renders.
        let mut s = fresh_state();
        s.stamina = Stamina::Exhausted;
        let rendered = s.render_for_prompt().expect("non-default renders");
        assert_eq!(rendered, "stamina: Exhausted");
    }

    // --- serde ---

    #[test]
    fn player_state_serde_round_trip() {
        let mut s = fresh_state();
        s.body.insert(BodyPart::Head, BodyPartState::Red);
        s.body.insert(BodyPart::LeftFoot, BodyPartState::Black);
        s.stamina = Stamina::Winded;
        s.wealth = 42;
        s.reputation = -7;

        let json = serde_json::to_string(&s).unwrap();
        let back: PlayerState = serde_json::from_str(&json).unwrap();

        assert_eq!(back.stamina, Stamina::Winded);
        assert_eq!(back.wealth, 42);
        assert_eq!(back.reputation, -7);
        assert_eq!(back.body.get(&BodyPart::Head).copied().unwrap(), BodyPartState::Red);
        assert_eq!(back.body.get(&BodyPart::LeftFoot).copied().unwrap(), BodyPartState::Black);
    }

    #[test]
    fn player_state_serde_missing_fields_default() {
        // An old save (pre-PlayerState) loads as `{}.to_string()`-ish — every
        // field has #[serde(default)] so this must not fail.
        let json = r#"{}"#;
        let s: PlayerState = serde_json::from_str(json).expect("empty object must default");
        assert!(s.is_default());
    }

    #[test]
    fn player_state_serde_partial_body_defaults_missing_parts() {
        // A save that only persisted one injured part must load the other 15
        // as Healthy when accessed via the getter (the getter uses
        // unwrap_or_default).
        let json = r#"{"body":{"LeftBicep":"Orange"},"stamina":"Active"}"#;
        let s: PlayerState = serde_json::from_str(json).unwrap();
        assert_eq!(s.body.get(&BodyPart::LeftBicep).copied().unwrap(), BodyPartState::Orange);
        assert_eq!(
            s.body.get(&BodyPart::Head).copied().unwrap_or_default(),
            BodyPartState::Transparent,
        );
    }

    // --- Referee ---

    #[test]
    fn referee_no_keyword_returns_none() {
        let s = fresh_state();
        assert_eq!(referee_evaluate("I walk to the bar and order an ale.", &s), None);
        assert_eq!(referee_evaluate("Hello, nice weather.", &s), None);
        assert_eq!(referee_evaluate("", &s), None);
    }

    #[test]
    fn referee_combat_keyword_returns_some() {
        let s = fresh_state();
        // Every keyword should fire.
        for kw in COMBAT_KEYWORDS {
            let text = format!("I {} at the goblin", kw);
            assert!(
                referee_evaluate(&text, &s).is_some(),
                "keyword {:?} should trigger a roll",
                kw,
            );
        }
    }

    #[test]
    fn referee_keyword_match_is_case_insensitive() {
        let s = fresh_state();
        assert!(referee_evaluate("I ATTACK the dragon", &s).is_some());
        assert!(referee_evaluate("I Swing my sword", &s).is_some());
    }

    #[test]
    fn referee_outcome_target_is_not_amputated() {
        // Pre-amputate every part; the referee must find nothing to injure.
        let mut s = fresh_state();
        for part in BodyPart::all() {
            s.body.insert(*part, BodyPartState::Black);
        }
        assert_eq!(referee_evaluate("I attack the goblin", &s), None);
    }

    #[test]
    fn referee_skips_amputated_parts_when_picking() {
        // Amputate the left arm; the referee must never pick it.
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftBicep, BodyPartState::Black);
        // Run many turns with varied text to exercise the RNG across the
        // candidate pool.
        for i in 0..64 {
            let text = format!("I attack the goblin number {}", i);
            let outcome = referee_evaluate(&text, &s).expect("should fire");
            assert_ne!(
                outcome.part, BodyPart::LeftBicep,
                "referee must not pick an amputated part",
            );
            // The outcome should be a valid non-Black state.
            assert!(outcome.new_state.can_be_injured());
        }
    }

    #[test]
    fn referee_new_state_never_downgrades_current() {
        // A part already Heavy (Red) shouldn't roll down to Yellow.
        let mut s = fresh_state();
        s.body.insert(BodyPart::Torso, BodyPartState::Red);
        for i in 0..32 {
            let text = format!("I strike the ogre {}", i);
            let outcome = referee_evaluate(&text, &s).expect("should fire");
            if outcome.part == BodyPart::Torso {
                assert!(
                    outcome.new_state.rank() >= BodyPartState::Red.rank(),
                    "Torso roll ({:?}) must not downgrade from Red",
                    outcome.new_state,
                );
            }
        }
    }

    #[test]
    fn referee_stamina_always_drains() {
        let s = fresh_state();
        let outcome = referee_evaluate("I attack", &s).expect("should fire");
        assert_eq!(outcome.stamina_after, Stamina::Active, "Fresh → Active on combat");
    }

    #[test]
    fn referee_stamina_drains_from_any_level() {
        let mut s = fresh_state();
        s.stamina = Stamina::Exhausted;
        let outcome = referee_evaluate("I attack", &s).expect("should fire");
        assert_eq!(outcome.stamina_after, Stamina::Depleted);
    }

    #[test]
    fn referee_deterministic_for_same_text_and_state() {
        // Same text + same state → same outcome (the xorshift seed is
        // derived from text + injury count). This is the testability
        // contract; a real RNG would break this and we'd swap the assertion.
        let s = fresh_state();
        let a = referee_evaluate("I swing my longsword at the goblin chieftain", &s);
        let b = referee_evaluate("I swing my longsword at the goblin chieftain", &s);
        assert_eq!(a, b);
    }

    #[test]
    fn apply_outcome_mutates_state() {
        let mut s = fresh_state();
        let outcome = RefereeOutcome {
            part: BodyPart::RightThigh,
            new_state: BodyPartState::Orange,
            stamina_after: Stamina::Winded,
            narrative_hint: "test".into(),
        };
        apply_outcome(&mut s, &outcome);
        assert_eq!(s.body.get(&BodyPart::RightThigh).copied().unwrap(), BodyPartState::Orange);
        assert_eq!(s.stamina, Stamina::Winded);
        assert!(!s.is_default());
    }

    // --- Roller (the mocked RNG) ---

    #[test]
    fn roller_range_stays_in_bounds() {
        let mut r = Roller::new(12345);
        for _ in 0..1000 {
            let i = r.range(7);
            assert!(i < 7);
        }
    }

    #[test]
    fn roller_range_zero_returns_zero() {
        let mut r = Roller::new(1);
        assert_eq!(r.range(0), 0);
    }

    #[test]
    fn roller_weighted_picks_valid_index() {
        let mut r = Roller::new(99);
        for _ in 0..100 {
            let i = r.weighted(&[10, 20, 5]);
            assert!(i < 3);
        }
    }

    #[test]
    fn roller_weighted_favors_heavier_weights() {
        // With weights [1, 99], index 1 should dominate. Sanity check the
        // distribution isn't broken.
        let mut r = Roller::new(7);
        let mut ones = 0;
        for _ in 0..200 {
            if r.weighted(&[1, 99]) == 1 {
                ones += 1;
            }
        }
        assert!(ones > 180, "weighted() should favor the heavy bucket; got {}/200", ones);
    }

    #[test]
    fn roller_zero_seed_does_not_collapse() {
        // The all-zero state would make xorshift return 0 forever. The ctor
        // remaps it; verify.
        let mut r = Roller::new(0);
        let a = r.next_u32();
        let b = r.next_u32();
        assert_ne!(a, b, "zero-seeded roller must not be stuck at 0");
    }

    #[test]
    fn hash_text_distributes() {
        // Different texts → different seeds (sanity).
        assert_ne!(hash_text("attack"), hash_text("defend"));
        assert_ne!(hash_text("attack"), hash_text("attack!"));
    }
}
