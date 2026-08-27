//! Player State engine — the Rust Referee (Fable Seam #7, brought forward).
//!
//! The LLM does ZERO math. This module is the sole authority over the
//! player's body, stamina, wealth, and reputation. It rolls the dice,
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

use std::collections::{BTreeMap, HashMap};

use crate::equipment;

// ---------------------------------------------------------------------------
// Body part state (the mannequin color states)
// ---------------------------------------------------------------------------

/// The injury/health state of a single body part. Maps 1:1 to the mannequin
/// color states in the spec:
///
/// | Variant     | Color        | Meaning                          |
/// |-------------|--------------|----------------------------------|
/// | `Healthy`   | transparent  | Healthy (the default)            |
/// | `Yellow`    | yellow       | Minor Injury                    |
/// | `Orange`    | orange       | Medium Injury                   |
/// | `Red`       | red          | Heavy Injury                    |
/// | `Purple`    | purple       | Critical Condition              |
/// | `Black`     | black        | Amputated / gone / decapitated  |
///
/// Serialization is PascalCase variant names verbatim (`"Healthy"`,
/// `"Yellow"`, … — serde's default unit-variant form; no rename_all).
/// (2026-08-22 Chloe ruling) The healthy variant was renamed from
/// `Transparent` to `Healthy` so every surface reads healthy → wounded
/// semantics; the WIRE keeps both spellings loadable — new writes say
/// `"Healthy"`, pre-rename saves carrying `"Transparent"` deserialize
/// through the serde `alias` (deserialize-only; the serializer always
/// emits the new name). The wire is consumed by the frontend's injury
/// heatmap, which owns the only PascalCase→snake_case seam
/// (injury-heatmap.js) to map them onto the CSS color classes — a healthy
/// part renders nothing there, under either spelling.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
pub enum BodyPartState {
    #[serde(rename = "Healthy", alias = "Transparent")]
    Healthy,
    Yellow,
    Orange,
    Red,
    Purple,
    Black,
}

impl Default for BodyPartState {
    fn default() -> Self {
        BodyPartState::Healthy
    }
}

impl BodyPartState {
    /// Human-readable label for prompt injection + UI tooltips.
    /// "Healthy" is the prose form of the default state (the user-facing
    /// word).
    pub fn semantic(&self) -> &'static str {
        match self {
            BodyPartState::Healthy => "Healthy",
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
            BodyPartState::Healthy => 0,
            BodyPartState::Yellow => 1,
            BodyPartState::Orange => 2,
            BodyPartState::Red => 3,
            BodyPartState::Purple => 4,
            BodyPartState::Black => 5,
        }
    }

    /// (2026-08-15 recovery seam) One healing step DOWN the severity
    /// ladder: Purple→Red→Orange→Yellow→healthy. Returns `None` when the
    /// part cannot heal — healthy (nothing to heal) or Amputated (a lost
    /// limb is gone; it never regrows). `rank` is private + load-bearing,
    /// so this stays inside the impl rather than reconstructing the ladder
    /// at the call site.
    pub fn heal_step(&self) -> Option<BodyPartState> {
        match self {
            BodyPartState::Healthy | BodyPartState::Black => None,
            BodyPartState::Yellow => Some(BodyPartState::Healthy),
            BodyPartState::Orange => Some(BodyPartState::Yellow),
            BodyPartState::Red => Some(BodyPartState::Orange),
            BodyPartState::Purple => Some(BodyPartState::Red),
        }
    }
}

// ---------------------------------------------------------------------------
// Stamina
// ---------------------------------------------------------------------------

/// The player's energy level. A 5-step ordinal, NOT a number — the UI
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
    /// player collapses rather than dying of stamina).
    pub fn drain(&mut self) {
        *self = match self {
            Stamina::Fresh => Stamina::Active,
            Stamina::Active => Stamina::Winded,
            Stamina::Winded => Stamina::Exhausted,
            Stamina::Exhausted | Stamina::Depleted => Stamina::Depleted,
        };
    }

    /// (2026-08-15 recovery seam) Recover one step toward `Fresh` — the
    /// inverse of `drain`, fired by the recovery Referee on a
    /// Downtime-classified rest turn. Without this stamina was strictly
    /// monotonic downward: every long campaign converged on a permanently
    /// `Depleted` player.
    pub fn recover(&mut self) {
        *self = match self {
            Stamina::Depleted => Stamina::Exhausted,
            Stamina::Exhausted => Stamina::Winded,
            Stamina::Winded => Stamina::Active,
            Stamina::Active | Stamina::Fresh => Stamina::Fresh,
        };
    }

    /// (2026-08-22 living-world) The player-side skill-check bonus this
    /// grade contributes (the 2026-08-22 alignment table: Fresh +4, Active
    /// +2, Winded 0, Exhausted −2, Depleted −4). The skill-check Referee
    /// consumes it NEGATED (its DC args are additive-harder-positive) via
    /// [`vigor_dc_mod`].
    pub fn dc_bonus(&self) -> i32 {
        match self {
            Stamina::Fresh => 4,
            Stamina::Active => 2,
            Stamina::Winded => 0,
            Stamina::Exhausted => -2,
            Stamina::Depleted => -4,
        }
    }
}

/// (2026-08-22 living-world, the Auto-Harvest Dormancy ruling) The ARCANE
/// pool — a dormant twin of [`Stamina`] that stays 100% hidden (zero
/// tokens, no render, no clamps, no recovery) until a card's fiction
/// actually names an arcane resource ("mana", "biotics", "rage", "ki").
/// Activation paths: the `[ARCANA <label>]` tracker bracket, or the
/// cold-start bootstrap harvesting the resource name from the intro. Once
/// active it rides the SAME rest-recovery curve + fatigue clamps + grade
/// DC table as stamina:
///
/// | Stamina | Mana    | player roll bonus |
/// |---------|---------|-------------------|
/// | Fresh   | Surging | +4                |
/// | Active  | Steady  | +2                |
/// | Winded  | Strained| 0                 |
/// | Exhausted| Drained| −2                |
/// | Depleted| Spent   | −4                |
///
/// Ordering mirrors Stamina (worst→best, `as u8` comparisons work).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize, Debug)]
pub enum Mana {
    Spent,
    Drained,
    Strained,
    Steady,
    Surging,
}

impl Mana {
    pub fn semantic(&self) -> &'static str {
        match self {
            Mana::Surging => "Surging",
            Mana::Steady => "Steady",
            Mana::Strained => "Strained",
            Mana::Drained => "Drained",
            Mana::Spent => "Spent",
        }
    }

    /// Drain one step toward `Spent` (a casting/channeling cost), never
    /// wrapping past the floor. `[ARCANA -1]` steps this.
    pub fn drain(&mut self) {
        *self = match self {
            Mana::Surging => Mana::Steady,
            Mana::Steady => Mana::Strained,
            Mana::Strained => Mana::Drained,
            Mana::Drained | Mana::Spent => Mana::Spent,
        };
    }

    /// Recover one step toward `Surging` — the same rest curve stamina
    /// rides (`[REST]` + the recovery referee).
    pub fn recover(&mut self) {
        *self = match self {
            Mana::Spent => Mana::Drained,
            Mana::Drained => Mana::Strained,
            Mana::Strained => Mana::Steady,
            Mana::Steady | Mana::Surging => Mana::Surging,
        };
    }

    /// The player-side skill-check bonus (the alignment table above).
    pub fn dc_bonus(&self) -> i32 {
        match self {
            Mana::Surging => 4,
            Mana::Steady => 2,
            Mana::Strained => 0,
            Mana::Drained => -2,
            Mana::Spent => -4,
        }
    }
}

/// (2026-08-22 living-world) The additive DC modifier the skill-check
/// Referee consumes for the body's current vigor: the WORSE of the stamina
/// grade and the ACTIVE mana grade (a drained channel drags every skilled
/// attempt; a dormant pool contributes nothing — gritty westerns pay zero
/// mechanics, not a penalty). Player-side bonuses NEGATED into
/// harder-positive DC units, matching `pacing_dc_mod`/`health_dc_mod`.
pub fn vigor_dc_mod(stamina: Stamina, mana: Option<Mana>) -> i32 {
    let bonus = match mana {
        Some(m) => stamina.dc_bonus().min(m.dc_bonus()),
        None => stamina.dc_bonus(),
    };
    -bonus
}

// ---------------------------------------------------------------------------
// Body parts (the 22 mannequin zones — LOCKED to the frontend hitbox layer)
// ---------------------------------------------------------------------------

/// The 22 mannequin body parts. This set is LOCKED 1:1 with the frontend
/// paperdoll hitbox layer (`src/fable/engine/body-parts.js` PARTS +
/// `src/fable/data/paperdoll-hitboxes.json`) — the paperdoll heatmap renders
/// an injury on the exact zone the Referee injures here, so the two MUST
/// never drift. The old 16-part set (Torso / LeftBicep / LeftThigh / LeftAnkle
/// + mirrors) was DELETED on 2026-08-07 — not renamed, not remapped.
///
/// `id()` is the stable snake_case key (`"left_upper_arm"`) shared verbatim
/// with the frontend's PARTS ids (the source of truth lives there); serde
/// serializes the PascalCase variant name (`"LeftUpperArm"`) on the wire.
/// `display()` is the human UI label (`"Left Upper Arm"`). Order is
/// anatomical, head→foot, left before right within each pair — matches the
/// frontend PARTS order so iteration lands in the same sequence on both
/// sides.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Debug)]
pub enum BodyPart {
    Head,
    Neck,
    UpperTorso,
    LowerTorso,
    LeftShoulder,
    RightShoulder,
    LeftUpperArm,
    RightUpperArm,
    LeftElbow,
    RightElbow,
    LeftLowerArm,
    RightLowerArm,
    LeftHand,
    RightHand,
    LeftUpperLeg,
    RightUpperLeg,
    LeftKnee,
    RightKnee,
    LeftLowerLeg,
    RightLowerLeg,
    LeftFoot,
    RightFoot,
}

impl BodyPart {
    /// All 22 parts in canonical (anatomical) order — head→foot, left before
    /// right within each pair. This is the single iteration order the Default
    /// seeding, the prompt injury-list, and the Referee candidate pool use.
    pub fn all() -> &'static [BodyPart] {
        &[
            BodyPart::Head,
            BodyPart::Neck,
            BodyPart::UpperTorso,
            BodyPart::LowerTorso,
            BodyPart::LeftShoulder,
            BodyPart::RightShoulder,
            BodyPart::LeftUpperArm,
            BodyPart::RightUpperArm,
            BodyPart::LeftElbow,
            BodyPart::RightElbow,
            BodyPart::LeftLowerArm,
            BodyPart::RightLowerArm,
            BodyPart::LeftHand,
            BodyPart::RightHand,
            BodyPart::LeftUpperLeg,
            BodyPart::RightUpperLeg,
            BodyPart::LeftKnee,
            BodyPart::RightKnee,
            BodyPart::LeftLowerLeg,
            BodyPart::RightLowerLeg,
            BodyPart::LeftFoot,
            BodyPart::RightFoot,
        ]
    }

    /// Stable snake_case id (`"left_upper_arm"`). Identical to the frontend
    /// `body-parts.js` PARTS ids — the single shared key space. Used in the
    /// prompt's injury list + as the Rust-internal identifier. Note the serde
    /// WIRE format is the PascalCase variant name, not this id (kept distinct
    /// so the frontend seam resolves the two at one place).
    pub fn id(&self) -> &'static str {
        match self {
            BodyPart::Head => "head",
            BodyPart::Neck => "neck",
            BodyPart::UpperTorso => "upper_torso",
            BodyPart::LowerTorso => "lower_torso",
            BodyPart::LeftShoulder => "left_shoulder",
            BodyPart::RightShoulder => "right_shoulder",
            BodyPart::LeftUpperArm => "left_upper_arm",
            BodyPart::RightUpperArm => "right_upper_arm",
            BodyPart::LeftElbow => "left_elbow",
            BodyPart::RightElbow => "right_elbow",
            BodyPart::LeftLowerArm => "left_lower_arm",
            BodyPart::RightLowerArm => "right_lower_arm",
            BodyPart::LeftHand => "left_hand",
            BodyPart::RightHand => "right_hand",
            BodyPart::LeftUpperLeg => "left_upper_leg",
            BodyPart::RightUpperLeg => "right_upper_leg",
            BodyPart::LeftKnee => "left_knee",
            BodyPart::RightKnee => "right_knee",
            BodyPart::LeftLowerLeg => "left_lower_leg",
            BodyPart::RightLowerLeg => "right_lower_leg",
            BodyPart::LeftFoot => "left_foot",
            BodyPart::RightFoot => "right_foot",
        }
    }

    /// UI label (`"Left Upper Arm"`). Title-case with spaces.
    pub fn display(&self) -> &'static str {
        match self {
            BodyPart::Head => "Head",
            BodyPart::Neck => "Neck",
            BodyPart::UpperTorso => "Upper Torso",
            BodyPart::LowerTorso => "Lower Torso",
            BodyPart::LeftShoulder => "Left Shoulder",
            BodyPart::RightShoulder => "Right Shoulder",
            BodyPart::LeftUpperArm => "Left Upper Arm",
            BodyPart::RightUpperArm => "Right Upper Arm",
            BodyPart::LeftElbow => "Left Elbow",
            BodyPart::RightElbow => "Right Elbow",
            BodyPart::LeftLowerArm => "Left Lower Arm",
            BodyPart::RightLowerArm => "Right Lower Arm",
            BodyPart::LeftHand => "Left Hand",
            BodyPart::RightHand => "Right Hand",
            BodyPart::LeftUpperLeg => "Left Upper Leg",
            BodyPart::RightUpperLeg => "Right Upper Leg",
            BodyPart::LeftKnee => "Left Knee",
            BodyPart::RightKnee => "Right Knee",
            BodyPart::LeftLowerLeg => "Left Lower Leg",
            BodyPart::RightLowerLeg => "Right Lower Leg",
            BodyPart::LeftFoot => "Left Foot",
            BodyPart::RightFoot => "Right Foot",
        }
    }

}

// ---------------------------------------------------------------------------
// PlayerState (the persisted canonical state)
// ---------------------------------------------------------------------------

/// The player's canonical state. Rust is the SOLE authority — the
/// narrator LLM never writes here, only reads the rendered `<player_state>`
/// block. Nested inside `WorldSchema` so it persists for free per-card.
///
/// `body` defaults to all-`Healthy`; `stamina` defaults to
/// `Fresh`. Wealth + reputation are numeric, Rust-owned, and never shown
/// raw to the user (the UI renders them via semantic formatting later).
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct PlayerState {
    #[serde(default)]
    pub body: HashMap<BodyPart, BodyPartState>,

    /// Per-zone injury descriptors, parallel to `body` (same `BodyPart` keys).
    /// Each entry is a list of terse noun-phrases ("Deep gash", "Minor scrape")
    /// appended by the Combat Referee each time that zone is wounded — so a zone
    /// hit across multiple turns accumulates a real history of what happened to
    /// it, surfaced in the paperdoll injury tooltip + rendered inline in the
    /// narrator's `injuries:` line. A zone heals/clears when it goes amputated
    /// (`Black`) — the limb is gone, its wound list is no longer meaningful.
    /// `#[serde(default)]` keeps pre-field saves loadable (empty map → no list).
    #[serde(default)]
    pub injury_details: HashMap<BodyPart, Vec<String>>,

    #[serde(default)]
    pub stamina: Stamina,

    /// (2026-08-22 living-world, the Auto-Harvest Dormancy ruling) The
    /// arcane pool. `None` = DORMANT: the card never named an arcane
    /// resource, and the field costs zero tokens forever (no render, no
    /// clamp, no recovery — the economy-dormancy pattern). `Some` once
    /// `[ARCANA <label>]` or the cold-start bootstrap activates it; rides
    /// the same rest curves + fatigue floors as stamina thereafter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mana: Option<Mana>,

    /// The diegetic resource name ("mana", "biotics", "rage") — the
    /// activated pool's render label. Empty while dormant.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mana_label: String,

    /// Coin / gold / credits. Numeric; the UI formats it. Default 0.
    #[serde(default)]
    pub wealth: u32,

    /// Standing in the world. Signed: negative = infamy, positive = renown.
    /// Default 0.
    #[serde(default)]
    pub reputation: i32,

    /// (2026-08-20 Economy) The player's upkeep tier. Default (and dormant)
    /// is Squatter — free, renders nothing, settles nothing. Mutated only
    /// by the `[LEDGER lifestyle]` applier; the daily settlement charges
    /// the inverse-curve cost at the current node's prosperity (fallback
    /// chain: pocket → player-owned tills → Starving).
    #[serde(default)]
    pub lifestyle: crate::economy::Lifestyle,

    /// (2026-08-20 Economy) The lifestyle settlement's day-boundary stamp
    /// (epoch-minutes — the same per-entity discipline as
    /// `economy::Property::last_settled_minutes`). Re-stamped on every
    /// `[LEDGER lifestyle]` change so a fresh tier never back-charges.
    #[serde(default)]
    pub lifestyle_settled_minutes: i64,

    /// (2026-08-20 Economy) The player's paid work (capped at
    /// `economy::MAX_JOBS` by the applier). Wages land in `wealth` at the
    /// daily settlement (presence-free); `JOB_LAPSE_ABSENT_DAYS`
    /// consecutive away-days end the contract. Mutated only by the
    /// `[LEDGER job]` applier + the settlement's lapse pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<crate::economy::Job>,

    /// Live appearance deltas applied ON TOP of the SavedPlayer's authored
    /// identity during play (2026-08-04 overhaul). A stable-keyed map so the
    /// `[APPEARANCE key=value]` bracket pipeline can mutate individual traits
    /// on the fly — cut hair, fresh scars, a disguise donned. Empty value
    /// (`""`) is the clear sentinel for that key. Clothing is NOT here
    /// (2026-08-18): garments are typed inventory items (`equipment` below) —
    /// the `outfit` key is retired.
    ///
    /// Seeded once from the SavedPlayer's structured traits at game attach
    /// (lib.rs `enter_fable_session`); every subsequent `[APPEARANCE]` bracket
    /// mutates this map — the SavedPlayer identity is never touched (it's the
    /// reusable cross-card baseline; this is the per-run live layer).
    /// Rides `save_split` → `<card_id>.player.json` for free (nested in
    /// `player_state`).
    #[serde(default)]
    pub current_appearance_deltas: HashMap<String, String>,

    /// Worn equipment — six slots (Head/Chest/MainHand/OffHand/Legs/Feet),
    /// each a two-layer stack (Outer narrator-visible, Inner hidden). Mutated
    /// by the `[EQUIP]` bracket; only present slots are keyed. Clothing lives
    /// HERE (2026-08-18): garments are items, and a change of clothes is an
    /// equip that displaces the prior garment into the pack. Empty by
    /// default. See `equipment.rs`. Rides `save_split` →
    /// `<card_id>.player.json`.
    #[serde(default)]
    pub equipment: equipment::Equipment,

    /// Quick-access belt — a fixed 4-slot rack (`BELT_MAX`) for potions,
    /// lockpicks, throwables. Mutated by the `[BELT]` bracket. Never
    /// appearance-visible (carried, not worn).
    #[serde(default)]
    pub belt: Vec<equipment::StackItem>,

    /// (2026-08-23 pouch ruling) The POUCH — the player's wallet. Currency,
    /// coins, keys, ID papers, and small valuables auto-route here at every
    /// acquisition path (`equipment::pouch_fit` via `stash_target`); the
    /// retired "pocketing" concept (small items riding pants/belt pockets)
    /// is replaced by it. UNBOUNDED like the pack. Mutated by the same
    /// brackets as the pack (`[PACK]`/`[NPC_ITEM player]` route wallet cargo
    /// here mechanically) + the Soul Gem panel's POUCH action. Rides
    /// `save_split` → `<card_id>.player.json` for free.
    #[serde(default)]
    pub pouch: Vec<equipment::StackItem>,

    /// Deep-storage pack — UNBOUNDED bagged inventory (the encumbrance/weight
    /// system was PERMANENTLY REMOVED 2026-08-09: no capacity enforcement, no
    /// fill bar, ever). Mutated by the `[PACK]` bracket OR by the Soul Gem
    /// inspection panel's STORE action. Never appearance-visible (carried, not
    /// worn). (`StackItem.weight` survives only for the narrator-summary text
    /// readout — it enforces nothing.)
    #[serde(default)]
    pub pack: Vec<equipment::StackItem>,
}

impl Default for PlayerState {
    fn default() -> Self {
        // Seed every body part to Healthy explicitly. HashMap::default() is
        // empty, which would read as "no body" — we want "fully healthy
        // body" so the mannequin renders correctly + referee_injureable
        // has the full part list to pick from.
        let mut body = HashMap::with_capacity(22);
        for part in BodyPart::all() {
            body.insert(*part, BodyPartState::Healthy);
        }
        PlayerState {
            body,
            injury_details: HashMap::new(),
            stamina: Stamina::Fresh,
            mana: None,
            mana_label: String::new(),
            wealth: 0,
            reputation: 0,
            lifestyle: crate::economy::Lifestyle::Squatter,
            lifestyle_settled_minutes: 0,
            jobs: Vec::new(),
            current_appearance_deltas: HashMap::new(),
            equipment: HashMap::new(),
            belt: Vec::new(),
            pouch: Vec::new(),
            pack: Vec::new(),
        }
    }
}

impl PlayerState {
    /// True when the state is the fresh-default (no injuries, full stamina,
    /// zero wealth/reputation, no live appearance deltas). Used to OMIT the
    /// `<player_state>` block entirely on a brand-new game with no attached
    /// player — same empty-skip pattern as `WorldSchema::render_for_prompt`.
    /// A seeded appearance (from a SavedPlayer attach) makes this false so
    /// the block renders even at full health.
    pub fn is_default(&self) -> bool {
        self.stamina == Stamina::Fresh
            && self.mana.is_none()
            && self.wealth == 0
            && self.reputation == 0
            && self.lifestyle == crate::economy::Lifestyle::Squatter
            && self.jobs.is_empty()
            && self.body.values().all(|s| *s == BodyPartState::Healthy)
            && self.injury_details.values().all(|v| v.is_empty())
            && self.current_appearance_deltas.is_empty()
            && self.equipment.is_empty()
            && self.belt.is_empty()
            && self.pouch.is_empty()
            && self.pack.is_empty()
    }

    /// Render the semantic block injected into the narrator prompt. Returns
    /// `None` when fully default (so the caller emits no block). Tight +
    /// line-oriented: every token is prefill cost.
    ///
    /// Format (when non-default):
    /// ```text
    /// stamina: Winded
    /// injuries: Left Upper Arm (Medium Injury), Right Upper Leg (Heavy Injury)
    /// amputated: Left Hand
    /// wealth: 12
    /// reputation: -3
    /// appearance:
    ///   hair_color: raven black
    ///   scars: brand on the shoulder
    /// equipped:
    ///   Main Hand: Iron Sword (+2 ATK)
    ///   Chest: Heavy Cloak
    /// ```
    /// Lines are omitted when empty (no injuries → no `injuries:` line). The
    /// `appearance:` block is emitted LAST so the model reads the character's
    /// current look right before generating prose — the diegetic ground truth
    /// that must stay consistent turn to turn. This is the fact block the
    /// narrator reads as hard truth. The `equipped:` block (observer-visible
    /// items — outer garments always, plus an inner item only where it
    /// physically peeks, e.g. socks under boots with bare/short-covered legs)
    /// follows appearance so the visible garments + readied weapons read as
    /// one cohesive look (2026-08-18: clothing IS this block — garments are
    /// equipped items; 2026-08-19: visibility, not blanket outer-only).
    /// `currency` — the world's money-unit label (`WorldSchema::
    /// currency_label`, 2026-08-21 economy addendum). Empty = naked
    /// integers (`wealth: 0`, `+8/day`); set = `wealth: 150 dollars`.
    /// Always the FLAT label (never tier-split): this block is
    /// model-facing, and the tracker must read the base unit to do
    /// `[LEDGER]` arithmetic.
    pub fn render_for_prompt(&self, currency: &str) -> Option<String> {
        self.render_for_prompt_with_beneath(false, currency)
    }

    /// The exposure-gated variant (2026-08-19, the upskirt ruling): when
    /// `reveal_beneath` is set — the turn's narrative window tripped
    /// `equipment::narrative_trips_exposure` ("someone looked up her skirt",
    /// undressing, the prose naming the smallclothes) — the CONCEALED inner
    /// layers render as one `beneath:` line, so the narrator narrates the
    /// real tracked garment instead of improvising a contradiction. Every
    /// other turn is byte-identical to the ungated render (zero tokens —
    /// Prime Mandate: concealed wear earns its place only in the 100% of
    /// gated turns where the scene actually exposes it).
    pub fn render_for_prompt_with_beneath(&self, reveal_beneath: bool, currency: &str) -> Option<String> {
        if self.is_default() {
            return None;
        }

        let mut lines: Vec<String> = Vec::with_capacity(8);

        // Stamina always (when non-default state); the model needs to know
        // fatigue even at full health if injured.
        lines.push(format!("stamina: {}", self.stamina.semantic()));

        // (2026-08-22 living-world) The arcane pool renders DIRECTLY under
        // stamina — only when ACTIVATED (the Auto-Harvest Dormancy ruling:
        // a dormant pool renders nothing, ever). One lean line, the
        // stamina: discipline; the label falls back to "mana" for a
        // hand-edited Some-with-empty-label save. The label is CLEANED at
        // the render (the same `clean_free_text` + cap the `[ARCANA]`
        // parse applies): a hand-edited save must never inject newlines or
        // oversized prose into `<world_state>`.
        if let Some(mana) = self.mana {
            let cleaned = crate::bracket_parser::clean_free_text(
                self.mana_label.trim(),
                crate::bracket_parser::ARCANA_LABEL_MAX,
            );
            let label: &str = if cleaned.is_empty() { "mana" } else { cleaned.as_str() };
            lines.push(format!("{label}: {}", mana.semantic()));
        }

        // Injuries: any part not Healthy AND not Amputated, in anatomical order.
        // Each entry carries its per-zone wound history (from injury_details)
        // so the narrator reads the descriptors as hard fact, not just the
        // severity tier — "Left Upper Arm (Medium Injury): Deep cut; Puncture".
        let injuries: Vec<String> = BodyPart::all()
            .iter()
            .filter_map(|p| {
                let state = self.body.get(p).copied().unwrap_or_default();
                match state {
                    BodyPartState::Healthy
                    | BodyPartState::Black => None,
                    _ => {
                        let base = format!("{} ({})", p.display(), state.semantic());
                        // Append the wound list if the zone has any. Joined by
                        // "; " so a multi-wound zone reads as a clean sublist.
                        match self.injury_details.get(p) {
                            Some(details) if !details.is_empty() => {
                                let joined = details
                                    .iter()
                                    .filter(|d| !d.is_empty())
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join("; ");
                                if joined.is_empty() {
                                    Some(base)
                                } else {
                                    Some(format!("{base}: {joined}"))
                                }
                            }
                            _ => Some(base),
                        }
                    }
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
        // facts; the narrator weaves them in, doesn't dwell. (2026-08-21
        // addendum) wealth renders through `money_plain` — the naked
        // base-unit integer when no currency is known, `{n} {label}` when
        // the tracker has set one. NEVER a hardcoded unit.
        if self.wealth != 0 {
            lines.push(format!(
                "wealth: {}",
                crate::economy::money_plain(self.wealth as i64, currency)
            ));
        }
        if self.reputation != 0 {
            lines.push(format!("reputation: {}", self.reputation));
        }

        // (2026-08-20 Economy) Lifestyle + jobs — dormant at the defaults
        // (Squatter renders nothing; zero prompt bytes for the common case).
        // A lifestyle line reads as an upkeep tier the narrator flavors;
        // each job carries its node (where the wage comes from). Wages are
        // base units (`money_plain`, same 2026-08-21 discipline).
        if self.lifestyle != crate::economy::Lifestyle::Squatter {
            lines.push(format!("lifestyle: {}", self.lifestyle.word()));
        }
        for j in &self.jobs {
            lines.push(format!(
                "job: {} @{} +{}/day",
                j.title,
                j.node_id,
                crate::economy::money_plain(j.daily_wage as i64, currency)
            ));
        }

        // Live appearance deltas: emitted LAST (loudest signal) so the model
        // reads the character's current look right before it writes prose.
        // Stable, alphabetically-sorted keys for token-deterministic output
        // (matches the entities-render discipline in schema.rs). The two-space
        // indent nests cleanly under the caller's `player_state:` wrapper.
        if !self.current_appearance_deltas.is_empty() {
            let mut entries: Vec<(&String, &String)> = self.current_appearance_deltas.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let body = entries
                .iter()
                .map(|(k, v)| format!("  {}: {}", k, v))
                .collect::<Vec<_>>()
                .join("\n");
            lines.push(format!("appearance:\n{}", body));
        }

        // Equipped items — what an OBSERVER sees (2026-08-19 NPC-perception
        // upgrade: outer garments always; an Inner item only where it
        // physically peeks — socks under boots show when the legs are bare or
        // short-hemmed, stay hidden under trousers or a full-length gown; an
        // Inner-only slot renders its item as the slot's visible wearer). A
        // Heavy Cloak (Outer) over a Linen Shirt (Inner) still reads as just
        // the cloak. Iterated in canonical slot order (Head→Feet) so the
        // readied weapon + visible garments read head-to-foot as one look.
        // Belt + pack are NEVER here — they're carried, not worn.
        if !self.equipment.is_empty() {
            let equipped_lines = equipment::visible_equipment_lines(&self.equipment);
            if !equipped_lines.is_empty() {
                lines.push(format!("equipped:\n{}", equipped_lines.join("\n")));
            }
            // The exposure-gated reveal: concealed wear, named only on turns
            // whose scene exposes it. Lives INSIDE the equipped block's
            // guard (nothing worn → nothing to reveal) but renders as its own
            // top-level line so the lean surgery passes it through verbatim.
            if reveal_beneath {
                let concealed = equipment::concealed_beneath_names(&self.equipment);
                if !concealed.is_empty() {
                    lines.push(format!(
                        "beneath (visible this moment): {}",
                        concealed.join(", ")
                    ));
                }
            }
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
/// `lethal` + `directive` (Slice 3, 2026-07-28): when the blow is judged
/// lethal (the dice + attacker tier + defender condition crossed the
/// threshold), `lethal` flips true and `directive` carries a hard
/// `[DIRECTIVE: ...]` line the narrator MUST obey. The lethality judgment
/// is pure Rust (the anti-Oblivion principle: NPC threat does not scale
/// with the player — enforced mechanically here, not by prompt).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefereeOutcome {
    pub part: BodyPart,
    pub new_state: BodyPartState,
    pub stamina_after: Stamina,
    /// Terse noun-phrase descriptor for THIS wound, picked from a static table
    /// keyed by `(attacker_tier, new_state)` + rolled by the outcome's own
    /// Roller so back-to-back identical blows differ (e.g. "Deep gash",
    /// "Minor scrape", "Shattered"). Applied by `apply_outcome` as a new entry
    /// in `PlayerState::injury_details[part]` — the paperdoll injury tooltip +
    /// the narrator's `injuries:` line both surface it. Empty only when the
    /// outcome was stamina-only (no wound descriptor to record).
    pub injury_desc: String,
    /// True when the Referee judged this blow lethal — the body is Downed
    /// (unconscious, dying). The narrator must obey: the player character
    /// cannot continue to fight, run, or act this turn. False for ordinary
    /// injuries that merely hurt.
    pub lethal: bool,
    /// Hard narrator directive, populated only when `lethal == true`. The
    /// caller wraps this as `[DIRECTIVE: {directive}]` inside `<world_state>`
    /// (same injection path as the skill-check Referee). Empty string
    /// otherwise. Reads as a single imperative sentence.
    pub directive: String,
}

/// The attacker's resilience/danger tier (Slice 3, 2026-07-28 — the
/// anti-Oblivion mechanic). Tier bands are pure qualitative descriptors of
/// the threat the player is engaging. The Referee uses the tier to weight
/// the severity roll: a Minion's blows rarely crit, a Legendary's rarely
/// don't.
///
/// The bands map 1:1 to the Slice 1 prompt clause — they're the Rust side
/// of the same thesis ("physics don't scale with the player"). The narrator
/// sees the tier only through the `[DIRECTIVE: ...]` lines it produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttackerTier {
    /// A trivial threat: rat, commoner, small dog. Blows land rarely, and
    /// when they do they're scrapes at worst. The player can mop these up.
    Minion,
    /// A competent combatant: trained guard, bandit, soldier, goblin warrior.
    /// The median threat — the default when the player attacks anything
    /// without an explicit tier. Blows weight toward Minor/Medium.
    Soldier,
    /// A serious combatant: veteran, knight, orc champion. Blows weight
    /// toward Medium/Heavy. A player engaging one of these solo is taking
    /// real risk.
    Elite,
    /// A boss-tier threat: warlord, troll, dire wolf. Blows weight toward
    /// Heavy/Critical. Engaging solo is suicide for an unprepared player.
    Boss,
    /// An apex threat: dragon, lich, demon prince. Blows weight heavily
    /// toward Critical; lethality is on the table every turn. The Slice 1
    /// "dragon" example incarnate.
    Legendary,
}

impl Default for AttackerTier {
    fn default() -> Self {
        AttackerTier::Soldier
    }
}

impl AttackerTier {
    /// Severity-roll weights for the five-tier BodyPartState ladder
    /// (Yellow / Orange / Red / Purple / Black — Black added 2026-08-20,
    /// Chloe tuning). Higher tiers weight toward severe outcomes.
    /// Index 0 = Yellow (Minor), 3 = Purple (Critical), 4 = Black
    /// (Amputated / destroyed — on a core part, death).
    fn severity_weights(self) -> [u32; 5] {
        match self {
            // Minion: almost always Minor, occasionally Medium, rarely worse;
            // a 1% amputation/kill ceiling.
            AttackerTier::Minion => [75, 18, 4, 2, 1],
            // Soldier: the v1 baseline shape (the pre-Black [50, 30, 15, 5]
            // distribution, rescaled with a 2% Black tail).
            AttackerTier::Soldier => [55, 30, 8, 5, 2],
            // Elite: weights shift toward Medium/Heavy.
            AttackerTier::Elite => [32, 40, 15, 10, 3],
            // Boss: Red becomes the modal outcome; Orange holds at 30.
            AttackerTier::Boss => [20, 30, 26, 20, 4],
            // Legendary: Red+Purple dominate; lethality is the lived reality.
            AttackerTier::Legendary => [10, 15, 40, 30, 5],
        }
    }

    /// Lethality DC modifier — added to the base DC for the lethal-blow SAVE.
    /// Higher tiers LOWER the save DC, making lethal outcomes more likely
    /// (the player needs a lower roll to fail the save and drop). Pure Rust
    /// math: a d20 is rolled, compared against `BASE_LETHAL_DC + tier_modifier
    /// + condition_penalty`. If the roll meets or beats the DC, the blow is
    /// lethal.
    fn lethality_dc_mod(self) -> i32 {
        match self {
            AttackerTier::Minion => 8,    // almost never lethal
            AttackerTier::Soldier => 4,
            AttackerTier::Elite => 0,     // baseline
            AttackerTier::Boss => -4,
            AttackerTier::Legendary => -8, // very likely lethal on a good hit
        }
    }

    /// (2026-08-23 dynamic DCs) Skill-check STAKES modifier — added to every
    /// skill DC, scaled by the strongest hostile presence in the scene (the
    /// `combined` tier: on-camera NPC tier max the ACTIVE site map's
    /// `present_mob_tier`). Persuading a king is not persuading a barkeep;
    /// sneaking past a legendary is a different universe from slipping a
    /// minion patrol — and now the DC says so WITHOUT any LLM discretion
    /// (the tier is Rust-derived, the ladder is a constant). The DELIBERATE
    /// MIRROR of [`Self::lethality_dc_mod`]: lethality lowers the save DC
    /// (the player drops easier); stakes RAISE the check DC (the player
    /// succeeds harder). Same 4-step spacing, opposite sign of effect.
    pub fn skill_dc_mod(self) -> i32 {
        match self {
            AttackerTier::Minion => 0,    // mooks add no pressure
            AttackerTier::Soldier => 2,
            AttackerTier::Elite => 4,
            AttackerTier::Boss => 6,
            AttackerTier::Legendary => 8, // persuading a legend is near-impossible
        }
    }

    /// Human-readable label for the lethality directive. The narrator sees
    /// this in the `[DIRECTIVE: Lethal blow (<tier> tier, DC N)...]` line.
    pub fn tag_for_directive(self) -> &'static str {
        match self {
            AttackerTier::Minion => "minion",
            AttackerTier::Soldier => "soldier",
            AttackerTier::Elite => "elite",
            AttackerTier::Boss => "boss",
            AttackerTier::Legendary => "legendary",
        }
    }
}

/// The base DC for the lethality SAVE. A roll >= this (modified) means the
/// blow is lethal. Tuned so a Legendary's full hit on a Battered defender
/// is almost always lethal, and a Minion's is almost never.
/// `pub(crate)` so the scene-pacing tests can pin the Soldier one-shot
/// guard against the real value.
pub(crate) const BASE_LETHAL_DC: i32 = 18;


/// Combat / exertion keywords that trigger a Referee roll. TWO-TIER split
/// (2026-08-17 E4B shakedown P1e): the playtest showed single soft words
/// ("hunting", "raids", "arrests" — verbs ABOUT others, asked in gossip)
/// flipping stew-and-chat turns into Combat mode with real injury rolls.
/// Matched through `keyword_present` (BOTH-side word boundaries,
/// case-insensitive) over dialogue-STRIPPED text (see `strip_dialogue`).
///
/// - **HARD** (below): direct first-person violence/action verbs — ONE fires
///   alone. This is the full pre-P1e combat list (attack + swing + strike +
///   …) PLUS the plan's additions (shove, duck, lunge families).
/// - **SOFT** ([`COMBAT_SOFT_KEYWORDS`]): violence-adjacent but commonly
///   figurative or about-others (hunt / raid / arrest / fight / chase) —
///   they corroborate: ≥2 DISTINCT soft keywords, or any hard keyword.
///   DEVIATION FROM THE PLAN (flagged for Chloe): the plan soft-listed
///   "attack" too, but "I attack the goblin" / "The goblin is attacking me"
///   are pinned combat controls (this file's sync test + scene_pacing's
///   suite) and the game's core loop — a soft-single "attack" would break
///   both. The evidence's false positives (arrests / hunting / raided) are
///   fully covered by the other five families.
///   both. Keep this file's TEST_COMBAT_KEYWORDS copy in lockstep (the sync
///   test pins the hard list fires BOTH consumers through the shared gate).
const COMBAT_HARD_KEYWORDS: &[&str] = &[
    // base verbs
    "attack", "swing", "strike", "slash", "stab", "punch", "kick", "block", "dodge",
    "parry", "shoot", "fire", "cast", "throw", "tackle", "grapple", "charge",
    "run", "sprint", "climb", "jump", "leap", "swim",
    // (P1e) the plan's first-person violence additions
    "shove", "shoves", "shoved", "shoving",
    "duck", "ducks", "ducked", "ducking",
    "lunge", "lunges", "lunged", "lunging",
    // inflected forms (P2b)
    "attacks", "attacked", "attacking",
    "swings", "swung", "swinging",
    "strikes", "struck", "striking",
    "slashes", "slashed", "slashing",
    "stabs", "stabbed", "stabbing",
    "punches", "punched", "punching",
    "kicks", "kicked", "kicking",
    "blocks", "blocked", "blocking",
    "dodges", "dodged", "dodging",
    "parries", "parried", "parrying",
    "shoots", "shot", "shooting",
    "fires", "fired", "firing",
    "casts", "casting",
    "throws", "threw", "thrown", "throwing",
    "tackles", "tackled", "tackling",
    "grapples", "grappled", "grappling",
    "charges", "charged", "charging",
    "runs", "running", "ran",
    "sprints", "sprinted", "sprinting",
    "climbs", "climbed", "climbing",
    "jumps", "jumped", "jumping",
    "leaps", "leapt", "leaping", "leaped",
    "swims", "swam", "swimming",
];

/// (P1e) Soft combat triggers: violence-adjacent verbs that commonly appear
/// in ABOUT-OTHERS / reported / gossip phrasing ("Is Harsk still hunting…",
/// "Have there been any arrests?", "the watch raided the docks"). A single
/// soft word fires NOTHING; two DISTINCT soft words (or any hard word) fire.
const COMBAT_SOFT_KEYWORDS: &[&str] = &[
    "hunt", "hunts", "hunted", "hunting",
    "raid", "raids", "raided", "raiding",
    "arrest", "arrests", "arrested", "arresting",
    "fight", "fights", "fought", "fighting",
    "chase", "chases", "chased", "chasing",
];

/// (P1e) The two-tier combat gate: any hard keyword alone, or ≥2 DISTINCT
/// soft keywords. Shared verbatim by the combat Referee + scene-pacing's
/// kinetic pillar so the two can never disagree.
pub(crate) fn combat_triggered(lower_dialogue_stripped: &str) -> bool {
    let hard = COMBAT_HARD_KEYWORDS
        .iter()
        .any(|kw| keyword_present(lower_dialogue_stripped, kw));
    if hard {
        return true;
    }
    COMBAT_SOFT_KEYWORDS
        .iter()
        .filter(|kw| keyword_present(lower_dialogue_stripped, kw))
        .count()
        >= 2
}

/// (2026-08-22 Chloe ruling — traversal is exertion, damage is
/// consequence-only) The movement/effort verb families that ride
/// [`COMBAT_HARD_KEYWORDS`] (scene pacing's kinetic pillar + the shared gate
/// stay intact — a chase still classifies Combat) but must NEVER mint a
/// limb injury on their own. Basic traversal drains stamina; damage needs
/// an explicit hazard ([`HAZARD_KEYWORDS`]) or a violence verb alongside.
pub(crate) const TRANSIT_VERBS: &[&str] = &[
    "run", "runs", "running", "ran",
    "sprint", "sprints", "sprinted", "sprinting",
    "climb", "climbs", "climbed", "climbing",
    "jump", "jumps", "jumped", "jumping",
    "leap", "leaps", "leapt", "leaping", "leaped",
    "swim", "swims", "swam", "swimming",
    "vault", "vaults", "vaulted", "vaulting",
    "scramble", "scrambles", "scrambled", "scrambling",
    "march", "marches", "marched", "marching",
];

/// (2026-08-22 Chloe ruling) Hazard markers that RE-ARM the injury roll on
/// a transit turn — the "failed roll / explicit hazard" half: falls,
/// slips, traps, heavy impacts, collapses. Paired with any transit verb
/// these are consequence-worthy (the climb that ends in a fall rolls).
const HAZARD_KEYWORDS: &[&str] = &[
    "fall", "falls", "fell", "falling",
    "slip", "slips", "slipped", "slipping",
    "trip", "trips", "tripped",
    "trap", "traps", "trapped",
    "impact", "impacts", "impacted",
    "crash", "crashes", "crashed", "crashing",
    "slam", "slams", "slammed", "slamming",
    "collapse", "collapses", "collapsed", "collapsing",
    "tumble", "tumbles", "tumbled", "tumbling",
];

/// (2026-08-22 Chloe ruling) TRUE when the ONLY hard trigger is traversal:
/// a transit verb with NO violence verb and NO hazard marker. Such a turn
/// pays the stamina tax (applied by the caller — one `drain()` step) and
/// rolls no injury, flips no lethality. Soft keywords (hunt/raid gossip)
/// never re-arm the roll — they are about-others phrasing, not hazards.
/// Pure fn over the SAME dialogue-stripped, lowercased text the referee
/// gate consumes.
pub(crate) fn transit_only_exertion(lower_dialogue_stripped: &str) -> bool {
    let transit = TRANSIT_VERBS
        .iter()
        .any(|kw| keyword_present(lower_dialogue_stripped, kw));
    if !transit {
        return false;
    }
    let violence = COMBAT_HARD_KEYWORDS
        .iter()
        .filter(|kw| !TRANSIT_VERBS.contains(kw))
        .any(|kw| keyword_present(lower_dialogue_stripped, kw));
    if violence {
        return false;
    }
    !HAZARD_KEYWORDS
        .iter()
        .any(|kw| keyword_present(lower_dialogue_stripped, kw))
}

/// (2026-08-15 recovery seam) Rest keywords that trigger the recovery
/// Referee. Same whole-word, case-insensitive matcher as COMBAT_KEYWORDS.
/// Deliberately rest-specific: "rest"/"sleep"/"camp" families — the verbs a
/// player types when their character actually rests. "watch" (a common
/// camp activity) is intentionally ABSENT: it is already a TENSE pillar
/// keyword in scene_pacing, and a watchkeeping sentry is not resting.
/// (2026-08-16 P2b) Inflections explicit (trailing boundary).
const REST_KEYWORDS: &[&str] = &[
    "rest", "rests", "rested", "resting",
    "sleep", "sleeps", "slept", "sleeping",
    "nap", "naps", "napped", "napping",
    "camp", "camps", "camped", "camping",
    "recuperate", "recuperates", "recuperating",
    "convalesce", "convalesces", "convalescing",
    "bandage", "bandages", "bandaged", "bandaging",
    "binds", "mend", "mends", "mending",
    "recovers", "recover", "recovered", "recovering",
];

/// (2026-08-15 recovery seam) The recovery Referee's verdict for one turn.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryOutcome {
    /// Stamina moved one tier toward `Fresh`.
    pub stamina_recovered: bool,
    /// The worst healable injury improved one grade. `None` = no injury was
    /// healable this turn (all healthy, or only amputations). The part maps
    /// to its NEW state; `Healthy` means fully healed (entry removed).
    pub healed: Option<(BodyPart, BodyPartState)>,
}

/// (2026-08-22 living-world) The rest-fatigue clamp floors: a `weary`
/// rested-band clamps stamina/mana down to Winded/Strained, the deeper
/// band to Exhausted/Drained. The clamp ONLY lowers — callers apply
/// `if current > floor` — so a state already at-or-below its floor
/// survives untouched.
pub fn fatigue_floors(band: &str) -> (Stamina, Mana) {
    match band {
        "weary" => (Stamina::Winded, Mana::Strained),
        _ => (Stamina::Exhausted, Mana::Drained),
    }
}

/// (2026-08-15 recovery seam) The recovery Referee — the exit from the
/// otherwise-monotonic wound/stamina economy. Fires ONLY when the scene is
/// classified Downtime AND the player's action carries a rest keyword: one
/// stamina tier up, plus the WORST healable injury down one grade. Worst =
/// highest severity; ties resolve to the first part in canonical
/// `BodyPart::all()` order (deterministic, no RNG — healing is not a gamble).
/// Amputated parts never heal; a fully-healed (`Healthy`) part's entry
/// + injury history are removed from the maps.
///
/// Pure evaluation: reads `state`, returns the outcome; the caller applies
/// it (same contract as `referee_evaluate` + `apply_outcome`).
pub fn referee_evaluate_recovery(
    text: &str,
    state: &PlayerState,
    is_downtime: bool,
) -> Option<RecoveryOutcome> {
    // (2026-08-20) The dead do not recuperate: a Black core part
    // (HealthTier::Deceased) refuses recovery outright — no stamina tier,
    // no healing.
    if crate::consequence::CORE_PARTS
        .iter()
        .any(|p| state.body.get(p).copied() == Some(BodyPartState::Black))
    {
        return None;
    }
    if !is_downtime {
        return None;
    }
    // (P1e) Dialogue-stripped: a quoted "The rest when the heat dies down"
    // (T22) must not heal real injuries across a negotiation.
    let lower = strip_dialogue(text).to_lowercase();
    if !REST_KEYWORDS.iter().any(|kw| keyword_present(&lower, kw)) {
        return None;
    }
    let stamina_recovered = state.stamina != Stamina::Fresh;
    // Worst healable injury: max severity rank among non-Black, non-Healthy
    // entries; canonical-order tie-break (first wins).
    // (2026-08-16 audit LOW) The tracked rank is the CURRENT injury rank —
    // the old code stored the HEALED (one-grade-down) rank, so ties resolved
    // to the LAST part (docs promise the first) and a later one-grade-less
    // injury could steal "worst" from an equal-grade earlier one.
    let mut worst: Option<(BodyPart, BodyPartState, u8)> = None; // (part, healed, current rank)
    for part in BodyPart::all() {
        if let Some(st) = state.body.get(part) {
            if let Some(healed) = st.heal_step() {
                let better = match worst {
                    None => true,
                    Some((.., cur_rank)) => st.rank() > cur_rank,
                };
                if better {
                    worst = Some((*part, healed, st.rank()));
                }
            }
        }
    }
    if !stamina_recovered && worst.is_none() {
        return None; // resting while fully healthy: nothing to recover
    }
    Some(RecoveryOutcome {
        stamina_recovered,
        healed: worst.map(|(part, healed, _)| (part, healed)),
    })
}

/// (2026-08-15 recovery seam) Apply a recovery outcome to the canonical
/// state. A part healed to `Healthy` is REMOVED from `body` (the map
/// only carries non-healthy zones — same clean-delete the split round-trip
/// uses) and its `injury_details` history is dropped with it.
pub fn apply_recovery(state: &mut PlayerState, outcome: &RecoveryOutcome) {
    if outcome.stamina_recovered {
        state.stamina.recover();
    }
    if let Some((part, new_state)) = outcome.healed {
        if new_state == BodyPartState::Healthy {
            state.body.remove(&part);
            state.injury_details.remove(&part);
        } else {
            state.body.insert(part, new_state);
        }
    }
}

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
    ///
    /// Rejection sampling (#82): plain `% n` weights the first
    /// `2^32 mod n` values one count heavier (for a d20 that's 16 values in
    /// ~4.3e9 — negligible, but the unbiased draw costs one comparison and
    /// the dice are the system's contract with the player). Draws landing in
    /// the incomplete tail zone are redrawn.
    pub fn range(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let n64 = n as u64;
        let zone = (1u64 << 32) / n64 * n64;
        loop {
            let x = u64::from(self.next_u32());
            if x < zone {
                return (x % n64) as usize;
            }
        }
    }

    /// Roll against a weighted table. `weights[i]` is the relative weight
    /// of outcome `i`. Returns the index of the chosen outcome. Sums the
    /// weights internally; panics on empty weights (caller bug). Same
    /// rejection-sampling discipline as [`Self::range`].
    pub fn weighted(&mut self, weights: &[u32]) -> usize {
        let total: u64 = weights.iter().map(|&w| w as u64).sum();
        assert!(total > 0, "weighted(): empty weights");
        let zone = (1u64 << 32) / total * total;
        let mut roll = loop {
            let x = u64::from(self.next_u32());
            if x < zone {
                break x % total;
            }
        };
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

/// Roll a terse wound descriptor for a fresh injury. Indexed by the resolved
/// `BodyPartState` tier (the wound's severity) with vocabulary that escalates
/// in weight alongside the attacker's tier — a Minion's Minor hit is a
/// "Scratch" or "Bruise", a Legendary's Heavy hit is a "Ruptured wound" or
/// "Caved-in fracture". `roller` picks one variant so back-to-back identical
/// blows differ. Returns Title Case so it reads cleanly in the tooltip + the
/// narrator's `injuries:` line. Pure (no I/O).
///
/// The vocabulary deliberately stays noun-phrase (no verb, no full sentence) so
/// the descriptor composes as a list item under the tooltip header and slots
/// into the existing `injuries: <part> (<severity>): <desc>; <desc>` render
/// without grammar surgery.
fn roll_injury_descriptor(
    roller: &mut Roller,
    tier: AttackerTier,
    state: BodyPartState,
    exertion: bool,
) -> String {
    // The base vocabulary per severity tier. Each row escalates: Minor is
    // surface damage, Medium cuts tissue, Heavy breaks structure, Critical
    // ruins it. Healthy never reaches here (no outcome for Healthy); Black
    // arrives from the severity roll's tuned tail (2026-08-20) and maps to
    // the "Severed" marker below.
    // (2026-08-27 playtest M3) EXERTION rows: a turn whose trigger text
    // carries a transit verb (climb/sprint/leap…) rolls CONSEQUENCE
    // vocabulary — sprains, wrenches, fractures — never violence wounds.
    // The playtest's T14 climb ("she pushed a door") minted "Puncture,
    // right upper leg": a descriptor class with zero narrative setup.
    let table: &[&str] = match (state, exertion) {
        (BodyPartState::Yellow, true) => &["Bruise", "Scrape", "Strain", "Twinge", "Abrasion"],
        (BodyPartState::Orange, true) => &["Sprain", "Bad bruise", "Pulled muscle", "Torn ligament", "Wrench"],
        (BodyPartState::Red, true) => &["Torn muscle", "Fracture", "Ruptured tendon", "Dislocation", "Severe sprain"],
        (BodyPartState::Purple, true) => &["Shattered joint", "Torn ligament mass", "Crushed bone", "Ruptured tissue", "Gruesome tear"],
        (BodyPartState::Yellow, false) => &["Scratch", "Bruise", "Minor scrape", "Abrasion", "Splinter"],
        (BodyPartState::Orange, false) => &["Deep cut", "Gash", "Bad bruise", "Laceration", "Puncture"],
        (BodyPartState::Red, false) => &["Deep gash", "Fracture", "Torn muscle", "Shattered bone", "Severe wound"],
        (BodyPartState::Purple, false) => &["Mangled wound", "Shattered", "Ruptured tissue", "Crushed bone", "Gruesome gash"],
        // Healthy never produces a descriptor; Black carries the single
        // amputation marker (apply_outcome REPLACES the zone's wound list
        // with it). Fall back to the semantic label so the field is never
        // empty if an unexpected path reaches here.
        (BodyPartState::Healthy, _) | (BodyPartState::Black, _) => &[],
    };
    if table.is_empty() {
        return match state {
            BodyPartState::Black => "Severed".to_string(),
            BodyPartState::Healthy => String::new(),
            _ => state.semantic().to_string(),
        };
    }
    let base = table[roller.range(table.len())];

    // Attacker-tier prefix: only the heaviest tiers prepend a qualifier — a
    // Minion's hit is just the base word ("Bruise"), while a Legendary's
    // becomes "Brutal Bruise" so the tooltip conveys the weight class at a
    // glance. Keeps the common case (Soldier, the default) unqualified.
    let prefix: &str = match tier {
        AttackerTier::Minion | AttackerTier::Soldier => "",
        AttackerTier::Elite => "Nasty ",
        AttackerTier::Boss => "Brutal ",
        AttackerTier::Legendary => "Devastating ",
    };
    format!("{prefix}{base}")
}


/// The Referee entry point. Pure fn — no I/O, no locks, no side effects.
/// Scans `text` for combat/exertion keywords; if matched, rolls the dice
/// against the current player state and returns the outcome.
///
/// Returns `None` when:
/// - no keyword matched (the turn was social/exploratory), OR
/// - the player is already `Depleted` AND fully amputated (no body
///   part left to injure — the dice have nothing left to say).
///
/// The caller (`fable_send`) applies the outcome via [`PlayerState`]'s
/// mutation helpers and then renders. This fn does NOT mutate.
///
/// Defaults to `AttackerTier::Soldier` (the median threat) — preserves the
/// v1 severity distribution exactly. Callers that know the attacker's tier
/// (Wupi-as-game-manager resolution, NPC stat declaration, the [COMBAT]
/// block's tier field) should call [`referee_evaluate_with_tier`] instead.
pub fn referee_evaluate(
    text: &str,
    state: &PlayerState,
) -> Option<RefereeOutcome> {
    referee_evaluate_with_tier(text, state, AttackerTier::Soldier, 0, 0, 0)
}

/// Select the attacker tier for a combat turn from the schema's entity map
/// (Fable Phase 3 Slice 5 wiring, 2026-07-28 — unblocks the tier-scaling the
/// `referee_evaluate_with_tier` integration test verified).
///
/// The narrator can declare an NPC's combat tier via entity keys shaped
/// `npc.<id>.tier` with one of these values (case-insensitive, plus common
/// synonyms): `minion`, `soldier` (default), `elite`, `boss`, `legendary`
/// (also `dragon`, `apex`, `arch`, `ancient` → Legendary; `giant`, `warlord`
/// → Boss; `veteran`, `knight`, `guard` → Elite; `grunt`, `thug`, `bandit`
/// → Soldier; `rat`, `wolf`, `spider`, `goblin` → Minion).
///
/// This fn scans the entity map for any `npc.*.tier` keys + picks the
/// HIGHEST threat found (if the player fights two foes, the dangerous one
/// dominates the severity distribution). If no tier keys exist, returns
/// `AttackerTier::Soldier` (the safe default — preserves the v1
/// distribution). The caller then passes the result to
/// `referee_evaluate_with_tier`.
///
/// Pure fn — no I/O. The entity map is `&HashMap<String, String>` (the
/// WorldSchema's `entities` field shape).
/// (2026-08-16 P2b) BOTH-side word-boundary keyword matcher: the keyword
/// must start at a word edge (the char before it is non-alphanumeric or
/// start-of-string) AND end at one (the char after is non-alphanumeric or
/// end-of-string). The old matcher checked only the leading edge, so it did
/// unpaid inflection duty — "attacks"/"swings" rode "attack"/"swing", but
/// "runner" also matched "run", "firelight" matched "fire", and a player in
/// a "restaurant" triggered the recovery Referee's "rest". With both
/// boundaries enforced, inflections must be explicit entries in the keyword
/// lists (every list was extended in the same change); compounds
/// ("campfire", "firelight") can never match their embedded keywords.
///
/// Case-insensitive by contract: callers pass `text.to_lowercase()` and
/// ASCII keywords. `pub(crate)`: also the matcher for `scene_pacing::score_
/// pillar` + `calm_signal` (#35) and `has_suspicious_action` — every
/// keyword referee in the codebase shares this one fn.
pub(crate) fn keyword_present(lower: &str, kw: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = lower[from..].find(kw) {
        let at = from + pos;
        let end = at + kw.len();
        // ASCII keywords can only match at char boundaries, so `at`/`end`
        // are boundaries.
        let leading_ok = lower[..at]
            .chars()
            .next_back()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        let trailing_ok = lower[end..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        if leading_ok && trailing_ok {
            return true;
        }
        from = at + kw.len().max(1);
    }
    false
}

/// (2026-08-17 E4B shakedown P1e) Strip double-quoted dialogue spans from
/// the player's action text before ANY keyword matching — dialogue is SPEECH,
/// not action. The playtest false-positives all matched inside quoted lines:
/// T22's recovery Referee healed 3 injury grades across a tavern negotiation
/// because Mara's quoted "The rest when the heat dies down" contained the
/// rest keyword, and T46/T47's gossip questions ("Is Harsk still hunting…")
/// flipped Combat mode. Straight `"` AND typographic `“”` quotes delimit a
/// span; each span collapses to a single space (word separation). An
/// UNTERMINATED quote swallows the rest of the text — the conservative
/// direction (speech tail can't trigger referees). Every keyword referee
/// (combat / recovery / skill checks / suspicious-action) + scene-pacing
/// score on the STRIPPED text.
///
/// (2026-08-20 P3) ANY quote flavor closes an open span. The old
/// same-flavor (or `”`-only) closer left MIXED pairs (`“sure"`) running to
/// end-of-text as "unterminated" — routine LLM/player output — silently
/// swallowing every action keyword after the quote. Over-closing a nested
/// different-flavor quote (`“She said "run" loudly”`) only re-leaks a
/// speech fragment, the milder failure either way.
pub(crate) fn strip_dialogue(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_quote = false;
    for c in text.chars() {
        match c {
            '"' | '“' | '”' => {
                if in_quote {
                    // Closing quote — any flavor (see the doc above).
                    in_quote = false;
                    out.push(' ');
                } else {
                    in_quote = true;
                }
            }
            _ => {
                if !in_quote {
                    out.push(c);
                }
            }
        }
    }
    out
}

pub fn select_attacker_tier_from_entities(
    entities: &std::collections::BTreeMap<String, serde_json::Value>,
    present_npc_ids: &[String],
) -> AttackerTier {
    // Single-pass scan: collect every npc.<id>.tier value for an NPC that is
    // ON-CAMERA this turn, parse each, keep the max. (P2 fix: the old scan
    // took the max over the WHOLE world — one npc.dragon.tier entity key
    // (which nothing ever removes) permanently escalated every subsequent
    // fight's severity + lethality DC, including bar brawls on the other
    // side of the map.) Tier keys are conventionally bare strings ("Elite",
    // "Boss"); a structured value at a .tier key is unrecognized noise —
    // skip it. Empty presence list → the Soldier default.
    let mut best: Option<AttackerTier> = None;
    if present_npc_ids.is_empty() {
        return AttackerTier::Soldier;
    }
    for id in present_npc_ids {
        let Some(value) = entities.get(&format!("npc.{id}.tier")) else {
            continue;
        };
        let Some(s) = value.as_str() else { continue };
        if let Some(tier) = parse_attacker_tier(s) {
            best = Some(match best {
                Some(prev) if prev as u8 >= tier as u8 => prev,
                _ => tier,
            });
        }
    }
    best.unwrap_or(AttackerTier::Soldier)
}

/// Parse a free-form tier string into an AttackerTier. Case-insensitive,
/// tolerant of common synonyms. Returns None for unparseable input. Mirrors
/// `relationship::parse_tier`'s tolerant-parse style.
pub fn parse_attacker_tier(s: &str) -> Option<AttackerTier> {
    let lower = s.trim().to_lowercase();
    // Direct enum names first (the canonical form).
    let direct = match lower.as_str() {
        "minion" | "trash" | "mob" => Some(AttackerTier::Minion),
        "soldier" | "regular" | "standard" | "normal" => Some(AttackerTier::Soldier),
        "elite" | "veteran" | "knight" | "guard" | "champion" => Some(AttackerTier::Elite),
        "boss" | "giant" | "warlord" | "ogre" | "troll" => Some(AttackerTier::Boss),
        "legendary" | "dragon" | "apex" | "arch" | "ancient" | "lich" | "demon" | "demigod" => {
            Some(AttackerTier::Legendary)
        }
        _ => None,
    };
    if direct.is_some() {
        return direct;
    }
    // Synonym fall-through: common low-tier creatures.
    match lower.as_str() {
        "rat" | "wolf" | "spider" | "goblin" | "bat" | "rat_king" | "kobold" => {
            Some(AttackerTier::Minion)
        }
        "grunt" | "thug" | "bandit" | "brigand" | "cultist" => Some(AttackerTier::Soldier),
        _ => None,
    }
}

/// The tier-aware Referee entry point (Slice 3, 2026-07-28). Identical to
/// [`referee_evaluate`] except the severity-roll weights + the lethality
/// threshold are scaled by the attacker's tier. The default `referee_evaluate`
/// is a thin wrapper that passes `AttackerTier::Soldier` here.
///
/// Lethality judgment: after the severity roll resolves, the Referee rolls
/// a second d20 against `BASE_LETHAL_DC + tier_mod + condition_penalty +
/// pacing_dc_mod`. `condition_penalty` is derived from the player's existing
/// wound load (a Battered defender is easier to drop than an Unscathed one).
/// `pacing_dc_mod` is ScenePacing's LETHALITY modifier
/// ([`crate::schema::SceneMode::lethality_dc_mod`]: Combat −1, Exploration 0,
/// Downtime +2 — the mirrored sign of the skill-check `dc_modifier`, so
/// combat raises the stakes instead of being the safest place to take a
/// hit; see that method's doc for the tuning rationale). On a failed save,
/// the outcome is flagged `lethal: true` + a hard directive is
/// emitted the narrator MUST obey ("the player is Downed — they cannot act
/// this turn"). This is the mechanical enforcement of the Slice 1
/// anti-Oblivion clause: a Legendary's full hit on a wounded body is lethal,
/// period.
pub fn referee_evaluate_with_tier(
    text: &str,
    state: &PlayerState,
    attacker_tier: AttackerTier,
    buff_tag_count: usize,
    debuff_tag_count: usize,
    pacing_dc_mod: i32,
) -> Option<RefereeOutcome> {
    // (P1e) Dialogue-stripped + two-tier combat gate: quoted speech never
    // triggers, and soft about-others verbs corroborate instead of firing
    // alone.
    let lower = strip_dialogue(text).to_lowercase();
    let triggered = combat_triggered(&lower);
    if !triggered {
        return None;
    }
    // (2026-08-22 Chloe ruling — traversal is exertion, damage is
    // consequence-only) Transit-only text (climb/sprint/leap… with no
    // violence verb, no hazard) rolls NO injury here — the caller routes
    // the turn to the one-step stamina tax instead. Scene pacing still
    // classifies these turns through the shared `combat_triggered` gate.
    if transit_only_exertion(&lower) {
        tracing::debug!(
            "referee: transit-only exertion — no injury roll (the stamina tax routes at the caller)"
        );
        return None;
    }

    // Seed from the text + current injury count so back-to-back identical
    // turns roll differently (the count changes after the first applies).
    let injury_count = state
        .body
        .values()
        .filter(|s| s.can_be_injured() && **s != BodyPartState::Healthy)
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

    // Roll severity on a weighted table. Weights are tier-driven (Slice 3):
    // a Minion weights toward Minor; a Legendary weights toward Red/Purple.
    // Index maps to: 0=Yellow, 1=Orange, 2=Red, 3=Purple, 4=Black.
    const SEVERITY_TABLE: [BodyPartState; 5] = [
        BodyPartState::Yellow,
        BodyPartState::Orange,
        BodyPartState::Red,
        BodyPartState::Purple,
        BodyPartState::Black,
    ];
    let roll_idx = roller.weighted(&attacker_tier.severity_weights());
    let mut new_state = SEVERITY_TABLE[roll_idx];

    // (2026-08-20 Chloe ruling — replaces the Slice 3 same-part escalation
    // rule) A re-hit never escalates AND never downgrades: wound tiers
    // progress only when the severity roll actually lands there (or worse)
    // — each tier is progressively harder to roll. A lighter roll on an
    // already-wounded part leaves the part AT its current tier; the fresh
    // blow still drains stamina and appends its (rolled-tier) descriptor to
    // the zone's history. Black comes only from the roll itself (the tuned
    // 1-5% tail) — never as a free escalation: a re-hit on a Purple part
    // stays Purple unless the dice actually land Black. A Black CORE part
    // (Head/Neck/UpperTorso) is death (see `apply_outcome`).
    let rolled_state = new_state;
    if new_state.rank() < current_state.rank() {
        new_state = current_state;
    }

    // Stamina always drains on a combat turn. The caller applies this too.
    let mut stamina_after = state.stamina;
    stamina_after.drain();

    // Lethality judgment (Slice 3, 2026-07-28). Roll a second d20 against
    // `BASE_LETHAL_DC + tier_mod + condition_penalty`. The condition penalty
    // is derived from the player's current wound load: a body that's already
    // badly hurt is far easier to drop. We use the consequence module's
    // derive_condition to get the holistic label, then map to a penalty.
    //
    // Cross-module read: pure fns on both sides, no mutation, no schema
    // coupling. The Referee stays pure-Rust; the consequence module owns
    // the label taxonomy.
    let derived =
        crate::consequence::derive_condition(&state.body, buff_tag_count, debuff_tag_count);
    let condition_penalty = match derived {
        crate::consequence::Condition::Downed => -20, // already down → any hit finishes
        crate::consequence::Condition::Critical => -10,
        crate::consequence::Condition::Battered => -4,
        crate::consequence::Condition::Wounded => -2,
        crate::consequence::Condition::Haggard => -1,
        crate::consequence::Condition::Unscathed => 0,
    };
    let lethality_dc =
        BASE_LETHAL_DC + attacker_tier.lethality_dc_mod() + condition_penalty + pacing_dc_mod;
    let lethality_roll = roll_d20(&mut roller);
    // `>=` (P0 fix): the roll meeting or beating the (modified) DC is a
    // LETHAL blow; a roll below it is survivable. The modifiers make sense
    // on that axis — dangerous attackers + wounded bodies LOWER the DC
    // (easier to clear), scrub attackers + fresh bodies raise it. The prior
    // `<` inverted every tier (default Soldier DC 22 > max d20 → EVERY hit
    // was lethal; Minions always lethal, Legendary 45%, a Downed player
    // unfinishable).
    let lethal = (lethality_roll as i32) >= lethality_dc;

    // Wound descriptor: a terse noun-phrase rolled from the (tier, severity)
    // table via the outcome's own Roller (so identical blows still differ).
    // This is what `apply_outcome` appends to injury_details[part] — the
    // paperdoll tooltip lists it + the narrator's injuries: line carries it.
    // Rolled from the ROLLED severity (the blow that actually landed), not
    // the kept tier — a light graze on a shattered arm records a Bruise, not
    // another "Shattered".
    // (2026-08-27 playtest M3) A transit verb anywhere in the trigger picks
    // the EXERTION vocabulary class — a climb/jump/leap turn's consequence
    // reads as a strain or wrench, never "Puncture" with zero narrative
    // setup.
    let exertion_class = TRANSIT_VERBS
        .iter()
        .any(|kw| keyword_present(&lower, kw));
    let injury_desc =
        roll_injury_descriptor(&mut roller, attacker_tier, rolled_state, exertion_class);

    // Directive: only populated when lethal. The caller wraps as
    // `[DIRECTIVE: {directive}]` in `<world_state>`.
    let directive = if lethal {
        format!(
            "Lethal blow ({} tier, DC {}): the player is DOWNED — \
             unconscious, unable to act, the fight is over. Narrate the \
             drop and its immediate aftermath; the player cannot continue \
             to fight, run, or resist this turn.",
            attacker_tier.tag_for_directive(),
            lethality_dc,
        )
    } else {
        String::new()
    };

    Some(RefereeOutcome {
        part,
        new_state,
        stamina_after,
        injury_desc,
        lethal,
        directive,
    })
}

/// (2026-08-22 playtest tuning — Chloe-flagged, tables untouched) The
/// LIVE-turn combat Referee entry point: `referee_evaluate_with_tier` wrapped
/// with an AVOIDANCE roll + a lethality-coherence gate. The 2026-08-22
/// Vaskar playtest showed the raw entry point's two failure shapes:
///
/// 1. **Every combat keyword was a guaranteed wound.** "I jump over his
///    pole-arm attack" (a purely defensive beat) rolled a full Purple foot
///    injury — six turns of skirmish produced six injuries ("gathering
///    injuries like Pokémon"), and the wound load then lowered the lethality
///    DC until two "DOWNED" directives fired back-to-back vs Soldier-tier
///    goblins. The avoidance roll (d20 + stamina mod vs
///    [`REFEREE_AVOID_DC`]) lets a competent, fresh fighter slip the
///    exchange entirely — no wound, no stamina drain. A Depleted fighter
///    still eats most exchanges (the death spiral for an exhausted body is
///    the designed law; the fix is that reaching it now takes deliberate
///    attrition, not three verbs).
/// 2. **A lethal save could fire on a landed graze.** The save's condition
///    penalty is wound-load-derived, so a battered body cleared a DC 17 with
///    a Yellow-grade blow — "lethal blow (soldier, DC 17): the player is
///    DOWNED" over a Minor Injury read incoherent and ended runs without
///    mercy. The coherence gate clears the lethal flag when the landed
///    severity is below Red — EXCEPT on an already-Downed body (any hit
///    finishes a Downed player, the existing law, which the −20 condition
///    penalty already makes near-certain).
///
/// The severity WEIGHTS + the lethality save math are the 2026-08-20 Chloe
/// rulings and pass through untouched — a blow that lands rolls exactly as
/// ruled.
pub fn referee_evaluate_live(
    text: &str,
    state: &PlayerState,
    attacker_tier: AttackerTier,
    buff_tag_count: usize,
    debuff_tag_count: usize,
    pacing_dc_mod: i32,
) -> Option<RefereeOutcome> {
    // Cheap gate first — no combat keyword, no rolls at all (the wrapper must
    // stay trigger-compatible with the raw entry point + scene pacing).
    let lower = strip_dialogue(text).to_lowercase();
    if !combat_triggered(&lower) {
        return None;
    }
    // Avoidance roll. Seeded off the same text + injury-count discipline as
    // the inner referee (identical text on an unchanged body = identical
    // verdict, so regenerate re-rolls stay deterministic), offset so the
    // defense draw is never the inner severity stream's first draw.
    let injury_count = state
        .body
        .values()
        .filter(|s| s.can_be_injured() && **s != BodyPartState::Healthy)
        .count();
    let seed = hash_text(text)
        .wrapping_add(injury_count as u64)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut roller = Roller::new(seed);
    let stamina_mod = match state.stamina {
        Stamina::Fresh => 4,
        Stamina::Active => 2,
        Stamina::Winded => 0,
        Stamina::Exhausted => -2,
        Stamina::Depleted => -4,
    };
    let defense = roll_d20(&mut roller) as i32 + stamina_mod;
    if defense >= REFEREE_AVOID_DC {
        // Dodged/parried clean: no wound, no drain, no directive. The turn
        // still narrates — the narrator just owes no injury this exchange.
        // (2026-08-27 playtest M3) VISIBLE now: the playtest's T21 ("I
        // attack… slash… stab" → no injury, no log line) was indistinguishable
        // from the referee not firing at all.
        tracing::info!(
            defense,
            stamina_mod,
            "combat referee: avoidance slip — clean dodge, no wound this exchange"
        );
        return None;
    }
    let mut outcome = referee_evaluate_with_tier(
        text,
        state,
        attacker_tier,
        buff_tag_count,
        debuff_tag_count,
        pacing_dc_mod,
    )?;
    if outcome.lethal && outcome.new_state.rank() < BodyPartState::Red.rank() {
        let derived =
            crate::consequence::derive_condition(&state.body, buff_tag_count, debuff_tag_count);
        if derived != crate::consequence::Condition::Downed {
            outcome.lethal = false;
            outcome.directive.clear();
        }
    }
    Some(outcome)
}

/// The avoidance DC: d20 + stamina modifier must meet or beat this to slip a
/// triggered exchange. **9 (2026-08-22 Chloe ruling — exact targets: Fresh
/// ~80% / Winded ~60% / Depleted ~40% clean; the ladder steps 10 points per
/// stamina tier: Fresh 80 / Active 70 / Winded 60 / Exhausted 50 / Depleted
/// 40 via mods +4/+2/0/−2/−4).** The severity tables stay Chloe-ruled.
const REFEREE_AVOID_DC: i32 = 9;

/// (2026-08-22) Player-facing bubble text for a landed combat outcome — the
/// turn-notice channel renders it top-left the moment the dice land (the
/// playtest's player "never found out he was even injured"). Wording per
/// Chloe's spec: severity as an adverbial phrase, amputation as the flat
/// fact, a lethal save as the DOWNED callout.
pub fn referee_notice_text(outcome: &RefereeOutcome) -> String {
    if outcome.lethal {
        return format!(
            "{} took a lethal blow — you are DOWNED.",
            outcome.part.display()
        );
    }
    let phrase = match outcome.new_state {
        // (2026-08-22 Chloe) The healthy arm is unreachable in practice —
        // `referee_evaluate_with_tier` only ever selects Yellow..Black and
        // never lowers a rank — but it now reads the RIGHT word if a future
        // path ever lands here (the old arm said "wounded" for the healthy
        // baseline).
        BodyPartState::Healthy => "healthy",
        BodyPartState::Yellow => "mildly injured",
        BodyPartState::Orange => "moderately injured",
        BodyPartState::Red => "heavily injured",
        BodyPartState::Purple => "in critical condition",
        BodyPartState::Black => "amputated",
    };
    format!("{} is now {}.", outcome.part.display(), phrase)
}

/// Apply a Referee outcome to a PlayerState. Mutates in place. Separate from
/// `referee_evaluate` (which is pure) so the caller controls WHEN state
/// mutates — typically right before the prompt render, inside the schema
/// lock, so the persisted state + the injected state are the same.
pub fn apply_outcome(state: &mut PlayerState, outcome: &RefereeOutcome) {
    state.body.insert(outcome.part, outcome.new_state);
    state.stamina = outcome.stamina_after;
    // (2026-08-20) A destroyed CORE part is death: the stamina well drops
    // to the Depleted floor (Chloe ruling: no separate Empty variant —
    // Depleted IS the empty-pips read) and `consequence::derive_health_tier`
    // reports Deceased off the same wound. Limb amputations never touch
    // this — Black limbs score zero points, that is all.
    if outcome.new_state == BodyPartState::Black
        && crate::consequence::CORE_PARTS.contains(&outcome.part)
    {
        state.stamina = Stamina::Depleted;
    }

    // Record the wound descriptor. A non-empty `injury_desc` appends to the
    // zone's detail history (so a zone hit across turns accumulates a list).
    // Amputation (Black) is the clear sentinel: the limb is gone, its prior
    // wound list is no longer meaningful, so drop it + leave just the
    // "Severed" marker the descriptor table produces for Black.
    if outcome.new_state == BodyPartState::Black {
        state.injury_details.insert(outcome.part, vec![outcome.injury_desc.clone()]);
    } else if !outcome.injury_desc.is_empty() {
        let v = state
            .injury_details
            .entry(outcome.part)
            .or_default();
        v.push(outcome.injury_desc.clone());
        // (P3 fix) Bound the per-zone wound history: a zone hit 50 times
        // over a long campaign otherwise contributes 50 noun phrases of
        // permanent prompt prefill + save bloat. Keep the most recent 5.
        let overflow = v.len().saturating_sub(5);
        v.drain(..overflow);
    }
}

// ---------------------------------------------------------------------------
// The Skill-Check Referee: silent Rust-authoritative dice for non-combat
// risky actions (2026-07-27, anti-sycophancy core).
// ---------------------------------------------------------------------------

/// The outcome of a non-combat skill check. The combat Referee above rolls
/// injuries; THIS sibling rolls the social/utility/movement checks (lockpick,
/// sneak, persuade, deceive, intimidate, etc.). Pure value type: turn-scoped,
/// never persisted on `WorldSchema` (computed fresh each `fable_send` and
/// discarded after the prompt is built).
///
/// `roll` is the d20 result kept for tracing/tests; it is NEVER shown to the
/// narrator. The narrator sees only `directive` (e.g. "[DIRECTIVE: Lockpick
/// (DC 12): FAIL. The lock resists.]") as a hard fact it must obey — the
/// sycophancy-killer, because Rust decided the outcome and the LLM has no
/// choice but to write prose that matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillCheckOutcome {
    /// Canonical skill name ("lockpick", "persuade", ...). Matches the
    /// `SkillSpec.name` that fired.
    pub skill: &'static str,
    /// The effective Difficulty Class the roll was compared against
    /// (`base_dc` + the ScenePacing DC modifier). 1..=30 in practice.
    pub dc: u32,
    /// The d20 result (1..=20). Tracing/tests only — NOT shown to narrator.
    pub roll: u32,
    /// `roll >= dc`. The authoritative success/fail decision.
    pub success: bool,
    /// Pre-formatted narrator directive, e.g.
    /// "Lockpick (DC 12): FAIL. The lock resists; your picks slip."
    /// The caller wraps this as `[DIRECTIVE: {directive}]` inside the
    /// `<world_state>` block.
    pub directive: String,
}

/// A skill-check specification: trigger keywords + base DC + outcome seeds.
/// The seeds are short, second-person prose the directive wraps — they are
/// NOT creative license; the canonical fact is the SUCCESS/FAIL outcome
/// itself (the seed just gives the narrator a starting verb).
struct SkillSpec {
    name: &'static str,
    /// Whole-word, case-insensitive substring matches against the player's
    /// turn text. Conservative: false-negative cost (missed roll) is a free
    /// success; false-positive cost (rolled on "I tell the truth about
    /// persuasion") is a spurious check. Mirror COMBAT_KEYWORDS' bar.
    keywords: &'static [&'static str],
    /// (2026-08-16 audit LOW; folded into P2b) Keywords that were promoted
    /// to whole-word matching because their lenient form false-positived on
    /// everyday prose ("lie" fired on "liege" + "lie down"). Since the
    /// trailing-boundary fix, EVERY keyword matches whole-word — this field
    /// survives as documentation of WHICH entries earned the distinction;
    /// its matcher (`keyword_whole_word`) is now a delegate of
    /// `keyword_present`.
    whole_word: &'static [&'static str],
    /// Base DC before the ScenePacing modifier (Combat +2, Exploration +0,
    /// Downtime −2). Tuned for d20 (1..=20): 12 = coin-flip for an untrained
    /// player, 14 = slight disadvantage.
    base_dc: u32,
    /// Narrator seed when the check succeeds. "{skill}" placeholder NOT used
    /// here — the seed is bespoke per skill so it reads naturally.
    success_seed: &'static str,
    /// Narrator seed when the check fails.
    fail_seed: &'static str,
    /// (2026-08-23 hazard referees) Entity stems: lowercase substrings
    /// matched against `skill_*` entity keys (after the `skill_`/`skill.`
    /// prefix strip) to find the player's declared proficiency — a
    /// declared rank lowers THIS skill's DC only (the one-directional
    /// prof bonus; an undeclared skill changes nothing). The panel
    /// (`skills.js`) reads the same entities, so the bar the player sees
    /// and the DC mod the referee applies are one source.
    entity_stems: &'static [&'static str],
}

/// The skill table. Order matters only for tracing (first match in iteration
/// order wins the lower skill-index seed for the Roller — but every match
/// fires, see `referee_evaluate_skill_checks`). Add new skills by appending.
///
/// EXCLUDES combat actions: those are owned by the combat Referee above
/// (`referee_evaluate` + COMBAT_KEYWORDS). The two Referees are disjoint by
/// keyword set, so a single turn never triggers both.
const SKILL_TABLE: &[SkillSpec] = &[
    SkillSpec {
        name: "lockpick",
        keywords: &[
            "pick the lock", "pick lock", "pick a lock", "picks the lock", "picked the lock",
            "picking the lock", "lockpick", "lockpicks", "lockpicked", "lockpicking",
            "pickpocket", "pickpockets", "pickpocketed", "pickpocketing",
        ],
        whole_word: &[],
        base_dc: 12,
        success_seed: "the lock clicks open",
        fail_seed: "the lock resists; your picks slip",
        entity_stems: &["lockpick", "locks"],
    },
    SkillSpec {
        name: "sneak",
        keywords: &[
            "sneak", "sneaks", "sneaked", "snuck", "sneaking",
            "sneak past", "sneaking past",
            "stealth", "stealthy", "stealthily",
            "hide", "hides", "hid", "hidden", "hiding",
            "slip past", "slipping past",
            "creep", "creeps", "crept", "creeping",
        ],
        whole_word: &[],
        base_dc: 12,
        success_seed: "you move unseen",
        fail_seed: "you are noticed",
        entity_stems: &["sneak", "stealth"],
    },
    SkillSpec {
        name: "persuade",
        keywords: &[
            "persuade", "persuades", "persuaded", "persuading", "persuasion",
            "convince", "convinces", "convinced", "convincing",
            "talk into", "talks into", "talked into", "talking into",
            "talk him into", "talk her into", "talking them into",
        ],
        whole_word: &[],
        base_dc: 14,
        success_seed: "your words land",
        fail_seed: "your words fall flat",
        entity_stems: &["persuade", "persuasion", "diplomacy", "negotiat"],
    },
    SkillSpec {
        name: "deceive",
        keywords: &[
            "bluff", "bluffs", "bluffed", "bluffing",
            "deceive", "deceives", "deceived", "deceiving", "deception",
            "fast-talk", "fast talk", "fast-talking", "fast talking",
            // "con" as a complete word ("I con the guard"); the old
            // trailing-space form "con " breaks under the boundary matcher
            // (the char after the space is a letter). Compounds like
            // "convince"/"connection" can't match — the boundary blocks.
            "con",
        ],
        // "lie" moved here: "lie down" (rest) + "liege" (a noun) used to roll
        // a deceive check — a complete-word match only fires on the verb.
        whole_word: &["lie"],
        base_dc: 14,
        success_seed: "the lie holds",
        fail_seed: "the lie unravels",
        entity_stems: &["deceive", "deception", "lie"],
    },
    SkillSpec {
        name: "intimidate",
        keywords: &[
            "intimidate", "intimidates", "intimidated", "intimidating", "intimidation",
            "threaten", "threatens", "threatened", "threatening", "threats",
            "scare", "scares", "scared", "scaring",
            "menace", "menaces", "menacing",
        ],
        whole_word: &[],
        base_dc: 13,
        success_seed: "they flinch",
        fail_seed: "they stand firm",
        entity_stems: &["intimidat"],
    },
];

/// Whole-word variant of [`keyword_present`]. (2026-08-16 P2b) now a pure
/// delegate: `keyword_present` enforces BOTH boundaries since the trailing-
/// boundary fix, so the two matchers are one semantics. Retained as the
/// named call site for `SkillSpec::whole_word` — the field documents WHICH
/// keywords were promoted for false-positive reasons ("lie" vs "liege"),
/// even though the matcher no longer differs.
pub(crate) fn keyword_whole_word(lower: &str, kw: &str) -> bool {
    keyword_present(lower, kw)
}

/// (2026-08-23 hazard referees) Map a `skill_*` entity's declared value to
/// the 0–5 proficiency rank, pinning parity with the panel's `toLevel`
/// ladder (`src/fable/panels/skills.js` — the bar the player sees and the
/// DC mod the referee applies must agree):
///
/// - Keywords (substring, first ladder match wins, mirroring toLevel's
///   ordered regexes): untrained|none→0, novice|beginner|apprentice→1,
///   adept|competent|junior→2, skilled|proficient→3,
///   expert|veteran|senior→4, master|mastery|legendary→5; an unmatched
///   non-empty value reads as the JS 50%-fallback → 2.
/// - Numbers: `v ≤ 1` is a fraction (rank `round(v×5)`); `v ≤ 10` is the
///   0–10 scale (rank `v`); `v > 10` is a percent (rank `round(v/20)`).
///   Clamped to [0, 5].
/// - Non-scalar values (objects/arrays/null) read as untrained (0).
///
/// Pure.
pub fn parse_skill_rank(value: &serde_json::Value) -> u8 {
    let text = match value {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return 0,
    };
    if text.is_empty() {
        return 0;
    }
    if let Ok(v) = text.parse::<f64>() {
        let rank = if v <= 1.0 {
            (v * 5.0).round()
        } else if v <= 10.0 {
            v.round()
        } else {
            (v / 20.0).round()
        };
        return rank.clamp(0.0, 5.0) as u8;
    }
    let lower = text.to_lowercase();
    const LADDER: &[(&str, u8)] = &[
        ("untrained", 0),
        ("none", 0),
        ("novice", 1),
        ("beginner", 1),
        ("apprentice", 1),
        ("adept", 2),
        ("competent", 2),
        ("junior", 2),
        ("skilled", 3),
        ("proficient", 3),
        ("expert", 4),
        ("veteran", 4),
        ("senior", 4),
        ("master", 5),
        ("mastery", 5),
        ("legendary", 5),
    ];
    for (stem, rank) in LADDER {
        if lower.contains(stem) {
            return *rank;
        }
    }
    2
}

/// (2026-08-24 Part II B8) Diff two entity maps' `skill_*`/`skill.` ranks and
/// return every skill whose parsed rank STRICTLY deepened (`old < new`; a
/// key absent from `before` counts as rank 0 — a first-time knack is the
/// biggest jump there is). Demotions/losses and non-skill keys never report.
/// The display name prettifies the key stem (`skill_lockpicking` →
/// `Lockpicking`). Pure — the whole surviving surface of the killed
/// level-up system (ONE toast per advance, no counters, no menus).
pub fn detect_skill_advances(
    before: &BTreeMap<String, serde_json::Value>,
    after: &BTreeMap<String, serde_json::Value>,
) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    for (key, value) in after {
        if !(key.starts_with("skill_") || key.starts_with("skill.")) {
            continue;
        }
        let new_rank = parse_skill_rank(value);
        let old_rank = before.get(key).map(parse_skill_rank).unwrap_or(0);
        if new_rank > old_rank {
            let stem = key
                .strip_prefix("skill_")
                .or_else(|| key.strip_prefix("skill."))
                .unwrap_or(key);
            let display: String = stem
                .split(['_', ' ', '-'])
                .map(|w| {
                    let mut cs = w.chars();
                    match cs.next() {
                        Some(c) => c.to_uppercase().collect::<String>() + cs.as_str(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            out.push((display, old_rank as u32, new_rank as u32));
        }
    }
    out
}

/// (2026-08-23 hazard referees) The one-directional proficiency DC mod for
/// one skill: the BEST (highest) declared rank among `skill_*` entities
/// whose key (post `skill_`/`skill.` prefix strip) contains one of the
/// spec's stems, mapped 1→0, 2→−1, 3→−2, 4→−3, 5→−4. Rank 0 (explicitly
/// untrained) and an undeclared skill both change nothing — proficiency
/// only ever HELPS (the anti-inflation law: the tracker declaring
/// "skill_sneak: terrible" must not make sneaking harder than never
/// having said anything). Pure.
fn skill_prof_dc_mod(
    spec: &SkillSpec,
    entities: &BTreeMap<String, serde_json::Value>,
) -> i32 {
    let mut best: Option<u8> = None;
    for (key, value) in entities {
        // The panel convention: `skill_<name>` keys (a tolerant `skill.`
        // alias rides along).
        let Some(rest) = key
            .strip_prefix("skill_")
            .or_else(|| key.strip_prefix("skill."))
        else {
            continue;
        };
        let rest_lower = rest.to_lowercase();
        if rest_lower.is_empty()
            || !spec.entity_stems.iter().any(|stem| rest_lower.contains(stem))
        {
            continue;
        }
        let rank = parse_skill_rank(value);
        best = Some(match best {
            Some(b) => b.max(rank),
            None => rank,
        });
    }
    match best {
        Some(r) if r >= 2 => -(i32::from(r.min(5)) - 1),
        _ => 0,
    }
}

/// Roll a single d20 (1..=20) using the provided Roller. Exposed so tests can
/// construct a Roller with a known seed and assert the roll value directly.
/// `pub(crate)` so sibling modules (offscreen_task) can roll the same d20
/// without duplicating the primitive (Slice 6, 2026-07-28).
pub(crate) fn roll_d20(roller: &mut Roller) -> u32 {
    // `range(20)` returns 0..=19; shift to 1..=20 (the canonical d20 range).
    roller.range(20) as u32 + 1
}

/// The skill-check Referee entry point. Pure fn — no I/O, no locks, no side
/// effects, mirrors `referee_evaluate`'s contract. Scans `text` for
/// skill-check keywords; for EACH match, rolls a d20 against the skill's
/// base DC (modified by `pacing_dc_mod` from ScenePacing) and returns the
/// outcome. Returns a Vec because a single turn can attempt multiple skills
/// ("I pick the lock, then sneak past the guard").
///
/// `pacing_dc_mod`: additive DC modifier from the current ScenePacing mode
/// (Combat: +2, Exploration: +0, Downtime: −2). Pass 0 when ScenePacing is
/// not yet computed (the Phase 2 default; Phase 3 threads the real value).
///
/// `health_dc_mod`: additive DC modifier from the derived overall health
/// tier (2026-08-20 — `consequence::derive_health_tier` /
/// `HealthTier::skill_dc_mod`). A Fair-or-worse body, or an active
/// illness, makes every skilled attempt harder. Pass 0 for a neutral body.
///
/// `vigor_dc_mod`: additive DC modifier from the body's current vigor
/// (2026-08-22 living-world — [`vigor_dc_mod`], the WORSE of the stamina
/// grade and the ACTIVE mana grade, player-side bonus negated into DC
/// units). A fresh body + surging channel lifts every skilled attempt; a
/// depleted one hardens them. Pass 0 for a neutral Fresh body with a
/// dormant pool.
///
/// `stakes_dc_mod`: additive DC modifier from the scene's STAKES
/// (2026-08-23 dynamic DCs — [`AttackerTier::skill_dc_mod`] over the
/// `combined` tier: on-camera NPC tier max the ACTIVE site map's
/// `present_mob_tier`). Picking a lock under a legend's gaze is a different
/// DC than picking one in an empty alley — pure Rust, no LLM discretion.
/// Pass 0 when nothing hostile is present.
///
/// `entities` (2026-08-23 hazard referees): the live schema's entity map —
/// a declared `skill_*` rank lowers ITS skill's DC by the one-directional
/// proficiency ladder ([`parse_skill_rank`] /
/// [`skill_prof_dc_mod`]). Pass an empty map when no schema is in scope.
/// The DICE are unaffected — the proficiency shifts the DC, never the seed
/// (the modifier-must-not-reseed-the-dice invariant).
///
/// Combat keywords are EXCLUDED here — `referee_evaluate` owns those. The
/// two Referees are disjoint by keyword set; the same turn may fire one
/// combat roll AND multiple skill rolls (e.g. "I attack the guard then
/// pickpocket the body"), but never the same keyword twice.
///
/// Determinism: each skill rolls with a distinct seed
/// (`hash_text(text) + skill_index`), so back-to-back identical turns produce
/// different rolls (the skill_index offset + the text hash compound). Same
/// text + same pacing → same outcome (testable).
///
/// (2026-08-23 Playground) `auto_pass` is an explicit god-mode INPUT, not a
/// seed change: when true every triggered skill forces `success: true` and
/// the directive renders the success seed — the dice + DC still roll + still
/// report, so the Playground's report line shows exactly what WOULD have
/// happened. Determinism is untouched (same text + same flags → same
/// outcome).
/// (2026-08-24 Part II B3) Display-name aliases for pinned-DC matching: the
/// Crossroads law declares natural skill names ("— [Lockpicking DC 18]"),
/// the SKILL_TABLE carries terse ids (`lockpick`). Both directions are
/// matched case-insensitively + trimmed: exact, alias-equal, or a ≥5-char
/// contains (short stems like "lie" would false-positive inside unrelated
/// words, so only the long forms participate in contains).
const SKILL_PIN_ALIASES: &[(&str, &[&str])] = &[
    ("lockpick", &["lockpicking", "lock-picking", "locksmithing"]),
    ("sneak", &["stealth", "sneaking", "skulking"]),
    ("persuade", &["persuasion", "convince", "convincing", "diplomacy"]),
    ("deceive", &["deception", "lying", "bluffing", "bluff"]),
    ("intimidate", &["intimidation", "menace", "threaten", "threatening"]),
];

fn pinned_skill_matches(pin: &str, spec_name: &str) -> bool {
    let pin = pin.trim().to_lowercase();
    if pin.is_empty() {
        return false;
    }
    let spec = spec_name.trim().to_lowercase();
    let aliases: Vec<&str> = SKILL_PIN_ALIASES
        .iter()
        .find(|(name, _)| *name == spec)
        .map(|(_, a)| a.to_vec())
        .unwrap_or_default();
    let mut candidates: Vec<&str> = vec![spec.as_str()];
    candidates.extend(aliases.iter().copied());
    candidates.retain(|c| !c.is_empty());
    if candidates.iter().any(|c| *c == pin) {
        return true;
    }
    // (2026-08-25 fix) Word-level matching — the old bidirectional raw
    // `contains` let "flying".contains("lying") price the DECEIVE check
    // with a Crossroads pin declared for a different skill. Words split on
    // non-alphanumerics; a candidate word matches a pin word on equality or
    // a ≥5-char PREFIX extension (inflections: "lockpicking" ← "lockpick",
    // "stealthy" ← "stealth"). Mid-word embeddings never match.
    let pin_words: Vec<&str> = pin
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .collect();
    let words_match = |a: &str, b: &str| {
        a == b
            || (b.chars().count() >= 5 && a.starts_with(b))
            || (a.chars().count() >= 5 && b.starts_with(a))
    };
    candidates.iter().any(|cand| {
        cand.split(|ch: char| !ch.is_alphanumeric())
            .filter(|w| w.chars().count() >= 3)
            .any(|w| pin_words.iter().any(|pw| words_match(*pw, w)))
    })
}

pub fn referee_evaluate_skill_checks(
    text: &str,
    pacing_dc_mod: i32,
    health_dc_mod: i32,
    vigor_dc_mod: i32,
    stakes_dc_mod: i32,
    entities: &BTreeMap<String, serde_json::Value>,
    auto_pass: bool,
    pinned_dc: Option<(&str, u32)>,
) -> Vec<SkillCheckOutcome> {
    // (P1e) Dialogue-stripped matching: spoken words don't attempt checks.
    let lower = strip_dialogue(text).to_lowercase();
    let text_hash = hash_text(text);
    let mut out = Vec::new();
    for (idx, spec) in SKILL_TABLE.iter().enumerate() {
        let triggered = spec.keywords.iter().any(|kw| keyword_present(&lower, kw))
            || spec
                .whole_word
                .iter()
                .any(|kw| keyword_whole_word(&lower, kw));
        if !triggered {
            continue;
        }
        // Distinct seed per skill: text hash + skill index. The index offset
        // guarantees "I pick the lock and sneak past" rolls lockpick and
        // sneak with different dice (otherwise the same hash → same roll).
        // The prof modifier deliberately does NOT enter the seed.
        let seed = text_hash.wrapping_add(idx as u64);
        let mut roller = Roller::new(seed);
        let roll = roll_d20(&mut roller);
        // (2026-08-24 Part II B3) PINNED DC: a Crossroads option that
        // declared "— [Skill DC N]" commits its difficulty the moment it is
        // offered — when the player picks it, the frontend arms this slot
        // and the NEXT turn's referee uses the declared DC for the matching
        // skill INSTEAD of the computed one (dice + seeds untouched). This
        // closes the last sycophancy door: the model cannot soften a check
        // it already priced. Matching runs through [`pinned_skill_matches`]
        // (natural names alias onto the terse table ids).
        let dc = match pinned_dc {
            Some((pin_name, pin_dc)) if pinned_skill_matches(pin_name, spec.name) => {
                pin_dc.clamp(1, 30)
            }
            _ => {
                // Effective DC = base + pacing + health + vigor + stakes +
                // declared proficiency modifiers, clamped to [1, 30]. A d20
                // roll is 1..=20, so DC ≤ 1 is always-success and DC ≥ 21 is
                // always-fail; the clamp keeps the math honest without
                // panicking.
                (spec.base_dc as i32
                    + pacing_dc_mod
                    + health_dc_mod
                    + vigor_dc_mod
                    + stakes_dc_mod
                    + skill_prof_dc_mod(spec, entities))
                .clamp(1, 30) as u32
            }
        };
        let success = auto_pass || roll >= dc;
        let seed_text = if success { spec.success_seed } else { spec.fail_seed };
        let directive = format!(
            "{} (DC {}): {}. {}.",
            // Capitalize the skill name for the directive sentence (cosmetic;
            // the narrator reads it as prose, so sentence case reads natural).
            capitalize_first(spec.name),
            dc,
            if success { "SUCCESS" } else { "FAIL" },
            seed_text,
        );
        out.push(SkillCheckOutcome {
            skill: spec.name,
            dc,
            roll,
            success,
            directive,
        });
    }
    out
}

/// Capitalize the first ASCII letter of `s`, lowercase the rest. Used only
/// for the directive sentence's leading word (cosmetic, narrator-facing).
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// ===========================================================================
// Phase 4 §11.44 (Component 1): Disguise Referee — the Rust-side gate
// ===========================================================================
//
// Design (locked with Chloe, 2026-07-28):
//
//   A disguise is a StatusTag with `kind == "disguise"` (the Tracker emits
//   `[EFFECT <label> buff 0 kind=disguise]`). When the player is disguised
//   and walks past low-tier NPCs (Minion / Soldier), Rust AUTO-PASSES — no
//   Deception roll, the disguise simply holds. The narrator is handed the
//   hard fact ("your disguise holds; the guard waves you through") and
//   writes prose to match.
//
//   Two overrides revoke the auto-pass and force a real Deception check:
//     (1) NPC tier > Soldier. Elite/Boss/Legendary NPCs scrutinize — a
//         captain knows his garrison's faces. The gate returns None and the
//         normal skill-check Referee (§11.21) handles the Deception roll.
//     (2) The player ACTS SUSPICIOUSLY while disguised. Rust keyword-detects
//         nervous tells / furtive movement / protocol mistakes. Even a
//         tired rank-and-file guard will challenge a sweating, stammering
//         stranger in uniform. The auto-pass is revoked; a Deception roll
//         fires here (so the directive carries the disguise context, not a
//         bare skill-check line).
//
//   This is the anti-bloat design Chloe locked: NO "Perception" stat, NO
//   "Disguise Level," NO "Guard Alertness Meter." Disguise is a binary tag
//   + a suspicious-action keyword scan + the existing tier ladder. Rust
//   owns the dice; the AI just renders the outcome.
//
//   Pure fn, no I/O, no schema mutation (mirrors referee_evaluate_skill_
//   checks' contract). The caller threads the result into turn_directives.

/// Base DC for the scrutinized-disguise Deception roll. Mirrors the
/// SKILL_TABLE `deceive` entry so there's one source of truth for "a lie
/// is DC 14" — a disguise under scrutiny IS a deception.
const DECEPTION_BASE_DC: u32 = 14;

/// Keywords that revoke the disguise auto-pass when present in the player's
/// turn text. Three families:
///   - nervous tells: sweat, tremble, stammer, hesitate, flinch, etc.
///   - furtive movement: sneak, creep, lurk, slink, tiptoe, etc. (intentional
///     overlap with SKILL_TABLE `sneak` — sneaking while disguised is itself
///     suspicious)
///   - protocol mistakes: wrong name, forget, confuse, salute wrong, etc.
///
/// Matched via `keyword_present` (BOTH-side word boundaries). Kept
/// conservative: only flags behavior a guard would actually notice.
/// (2026-08-16 P2b) Inflections explicit — the old prefix stubs ("hesitat")
/// and free suffix rides died with the trailing boundary.
const SUSPICIOUS_ACTIONS: &[&str] = &[
    // nervous tells — visible distress
    "sweat", "sweats", "sweating", "sweaty", "nervous", "nervously",
    "tense", "tensed", "tensely",
    "tremble", "trembles", "trembled", "trembling",
    "stutter", "stutters", "stuttered", "stuttering",
    "stammer", "stammers", "stammered", "stammering",
    "hesitate", "hesitates", "hesitated", "hesitating",
    "flinch", "flinches", "flinched", "flinching",
    "mumble", "mumbles", "mumbled", "mumbling",
    "mutter", "mutters", "muttered", "muttering",
    "fidget", "fidgets", "fidgeting",
    "fumble", "fumbles", "fumbled", "fumbling",
    "falter", "falters", "faltered", "faltering",
    "stiffen", "stiffens", "stiffened", "stiffening",
    "rigid", "rigidly",
    // eye behavior — the classic tell
    "avoid eye contact", "avoids eye contact", "avoiding eye contact",
    "look away", "looks away", "looking away",
    "avert eyes", "averts eyes", "averting eyes",
    "avert gaze", "averts gaze", "averting gaze",
    "stare at the ground", "stares at the ground", "staring at the ground",
    "eyes dart", "eyes darting",
    "glance around", "glances around", "glancing around",
    // furtive movement — trying not to be noticed IS suspicious in uniform
    "sneak", "sneaks", "snuck", "sneaking",
    "creep", "creeps", "crept", "creeping",
    "lurk", "lurks", "lurking",
    "slink", "slinks", "slinking",
    "tiptoe", "tiptoes", "tiptoeing",
    "slip past", "slipping past",
    "edge away", "edges away", "edging away",
    "skulk", "skulks", "skulking",
    // protocol mistakes — the disguise breaks down
    "wrong name", "forget", "forgets", "forgot", "forgetting",
    "confuse", "confuses", "confused", "confusing",
    "salute wrong", "wrong salute",
    "don't know", "do not know",
    "blunder", "blunders", "blundering",
    "stumble over", "stumbles over", "stumbling over",
    "misspell", "misspells", "misspelled", "misspelling",
    "wrong badge", "no badge", "wrong uniform", "wrong color", "wrong rank",
];

/// Find the active disguise tag, if any. Returns the first LIVE (unexpired)
/// tag with `kind == "disguise"` — the gate's clock (2026-08-20) skips
/// expired disguises at read time, the same filter the prompt renderer and
/// the polarity counts run (the tick's expiry sweep is suspended in Combat,
/// so an expired disguise must not keep auto-passing through a long fight).
/// A player can technically hold multiple disguise tags (e.g. swapped
/// mid-scene); we evaluate against the first live one — the others are
/// stale and the gate cares about presence, not multiplicity.
pub fn find_disguise_tag(
    tags: &[crate::consequence::StatusTag],
    now_minutes: i64,
) -> Option<&crate::consequence::StatusTag> {
    tags.iter()
        .find(|t| t.kind == "disguise" && !t.is_expired(now_minutes))
}

/// True if the player's turn text contains any suspicious-action keyword.
/// Pure keyword scan; case-insensitive. Used by the gate to decide whether
/// to revoke the auto-pass. (P1e) Scored on dialogue-stripped text.
pub fn has_suspicious_action(text: &str) -> bool {
    let lower = strip_dialogue(text).to_lowercase();
    SUSPICIOUS_ACTIONS.iter().any(|kw| keyword_present(&lower, kw))
}

/// The outcome of the disguise gate for one turn. Carries everything the
/// narrator needs to render the moment — Rust has already rolled the dice;
/// the AI just writes prose to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisguiseDirective {
    /// The auto-pass held: a disguised player walked past low-tier
    /// rank-and-file without drawing scrutiny. `tier_tag` is the
    /// AttackerTier::tag_for_directive() of the NPCs present (e.g.
    /// "soldier", "minion") — narratively, who was fooled.
    AutoPass {
        label: String,
        tier_tag: &'static str,
    },
    /// The auto-pass was revoked by suspicious behavior; Rust rolled a
    /// Deception check. `dc`/`roll`/`success` are the dice facts; `seed`
    /// is the narrator-flavor lead-in (same shape as SkillCheckOutcome).
    Scrutinized {
        label: String,
        dc: u32,
        roll: u32,
        success: bool,
        seed: &'static str,
    },
}

impl DisguiseDirective {
    /// Format the directive line for the `<directives>` block. Reads as a
    /// hard fact the narrator obeys. Two registers:
    ///   - AutoPass: "Disguise (city guard uniform): ACCEPTED — soldiers and
    ///     lesser rank-and-file do not challenge the player."
    ///   - Scrutinized: "Disguise (city guard uniform): SCRUTINIZED —
    ///     Deception (DC 14): FAIL. the act cracks under scrutiny."
    pub fn render(&self) -> String {
        match self {
            DisguiseDirective::AutoPass { label, tier_tag } => format!(
                "Disguise ({}): ACCEPTED — {} and lesser rank-and-file do not challenge the player.",
                label, tier_tag,
            ),
            DisguiseDirective::Scrutinized { label, dc, roll, success, seed } => format!(
                "Disguise ({}): SCRUTINIZED — Deception (DC {}): {}. {} (roll {})",
                label, dc, if *success { "SUCCESS" } else { "FAIL" }, seed, roll,
            ),
        }
    }

    /// True when the gate's dice say the disguise is BLOWN and the caller
    /// (which holds the schema lock) must mechanically remove the disguise
    /// StatusTag. (P2 fix: a failed scrutiny used to emit prose only — the
    /// tag survived until its expiry and the auto-pass resumed next turn.)
    pub fn should_revoke(&self) -> bool {
        matches!(self, DisguiseDirective::Scrutinized { success: false, .. })
    }
}

/// Seeds for the Scrutinized outcome. Mirrors SkillSpec.success_seed /
/// fail_seed — short narrator-flavor phrases.
const SCRUTINIZED_SUCCESS_SEED: &str = "the player's nerve holds; the disguise buys passage";
const SCRUTINIZED_FAIL_SEED: &str = "the act cracks under scrutiny; the disguise is challenged";

/// The gate. Pure fn — no I/O, no schema mutation.
///
/// Returns:
///   - `None` when there's no active disguise tag (nothing to gate).
///   - `None` when NOBODY is on camera: no observers means no scrutiny — the
///     tag simply persists unchallenged (an AutoPass here would narrate
///     invisible soldiers waving the player through; #55).
///   - `Some(Scrutinized)` when an Elite+ NPC is present: they scrutinize by
///     default, so a Deception check is rolled HERE (P2 fix — the old
///     `return None` assumed the §11.21 skill referee would roll it, but
///     that referee only fires on deceive-keywords: a disguised player
///     walking past a captain with neutral text got NO check at all).
///   - `Some(AutoPass)` when disguised + low-tier NPCs + no suspicious action.
///   - `Some(Scrutinized)` when disguised + low-tier NPCs + suspicious action
///     (the auto-pass is revoked; a Deception roll fires here).
///
/// On any Scrutinized FAIL the caller should honor `should_revoke()` and
/// mechanically remove the disguise tag.
///
/// `entities` + `present_npc_ids` scope the tier selection to the NPCs
/// actually on-camera this turn. `pacing_dc_mod` is the ScenePacing DC
/// modifier (Combat +2, Exploration 0, Downtime −2) and `health_dc_mod` the
/// derived-body modifier (`consequence::health_dc_mod`, 2026-08-20 Chloe
/// ruling: deception under scrutiny is a skilled act — a feverish or
/// battered body fumbles it exactly like a lockpick) — both threaded into
/// the Scrutinized DC exactly as the skill-check Referee does.
/// `now_minutes` is the WorldClock (the gate's clock): expired disguise
/// tags read as no disguise at all.
pub fn evaluate_disguise_gate(
    text: &str,
    tags: &[crate::consequence::StatusTag],
    entities: &BTreeMap<String, serde_json::Value>,
    present_npc_ids: &[String],
    pacing_dc_mod: i32,
    health_dc_mod: i32,
    vigor_dc_mod: i32,
    now_minutes: i64,
) -> Option<DisguiseDirective> {
    let disguise = find_disguise_tag(tags, now_minutes)?;
    // Empty scene: no gate outcome at all. Falling through would compare
    // against the Soldier default and emit an AutoPass crediting soldiers
    // who are not there (and could even roll scrutiny with no observer).
    if present_npc_ids.is_empty() {
        return None;
    }
    let label = disguise.label.clone();
    let tier = select_attacker_tier_from_entities(entities, present_npc_ids);
    // Elite+ (captains, bosses, legendary creatures) scrutinize by default —
    // they know their people. Roll the Deception check NOW, at a harder DC
    // (they are harder to fool than a doorway guard).
    if tier > AttackerTier::Soldier {
        // Distinct dice from the low-tier scrutinized roll (different offset).
        let seed = hash_text(text).wrapping_add(0xE117E5);
        let mut roller = Roller::new(seed);
        let roll = roll_d20(&mut roller);
        let dc = (DECEPTION_BASE_DC as i32 + 3 + pacing_dc_mod + health_dc_mod + vigor_dc_mod)
            .clamp(1, 30) as u32;
        let success = roll >= dc;
        let s = if success {
            "the player's composure withstands a captain's eye; the disguise holds — for now"
        } else {
            "an Elite's scrutiny pierces the disguise; it is challenged and blown"
        };
        return Some(DisguiseDirective::Scrutinized {
            label,
            dc,
            roll,
            success,
            seed: s,
        });
    }
    // Low-tier rank-and-file. Auto-pass UNLESS the player acts suspiciously.
    if !has_suspicious_action(text) {
        return Some(DisguiseDirective::AutoPass {
            label,
            tier_tag: tier.tag_for_directive(),
        });
    }
    // Suspicious behavior revokes the auto-pass. Roll a Deception check
    // (seeded from text + a fixed offset so it diverges from the §11.21
    // deceive roll that may ALSO fire on the same turn — distinct dice).
    let seed = hash_text(text).wrapping_add(0xC0FFEE);
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);
    let dc =
        (DECEPTION_BASE_DC as i32 + pacing_dc_mod + health_dc_mod + vigor_dc_mod).clamp(1, 30) as u32;
    let success = roll >= dc;
    let s = if success { SCRUTINIZED_SUCCESS_SEED } else { SCRUTINIZED_FAIL_SEED };
    Some(DisguiseDirective::Scrutinized {
        label,
        dc,
        roll,
        success,
        seed: s,
    })
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

    // --- pack_capacity_lbs removal (2026-08-11) ---
    //
    // The field was deleted from `PlayerState` after being permanently retired
    // (2026-08-09). Old saves with a stray `pack_capacity_lbs` key must still
    // load — serde's default unknown-field tolerance ignores it (the struct
    // doesn't carry `#[serde(deny_unknown_fields)]`). This is the regression
    // guard.

    #[test]
    fn player_state_loads_with_legacy_pack_capacity_lbs_field() {
        // A pre-removal save would have included "pack_capacity_lbs": 20.0.
        let legacy_json = r#"{
            "body": {},
            "injury_details": {},
            "stamina": "Fresh",
            "wealth": 0,
            "reputation": 0,
            "current_appearance_deltas": {},
            "equipment": {},
            "belt": [],
            "pack": [],
            "pack_capacity_lbs": 20.0
        }"#;
        let parsed: PlayerState = serde_json::from_str(legacy_json)
            .expect("legacy save with pack_capacity_lbs must still load");
        // The struct no longer has the field — the assertion is purely "didn't
        // error during deserialize". Sanity-check a sibling field landed.
        assert_eq!(parsed.stamina, Stamina::Fresh);
    }

    #[test]
    fn player_state_serializes_without_pack_capacity_lbs_key() {
        // Post-removal saves must NOT carry the field — confirm it's gone from
        // the wire shape (a future reload won't see it).
        let s = PlayerState::default();
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            !json.contains("pack_capacity_lbs"),
            "post-removal save must not include pack_capacity_lbs: {json}"
        );
    }

    // --- enum basics ---

    #[test]
    fn body_part_state_default_is_transparent() {
        assert_eq!(BodyPartState::default(), BodyPartState::Healthy);
    }

    #[test]
    fn body_part_state_wire_renames_healthy_but_loads_legacy() {
        // (2026-08-22 rename pin) New writes serialize "Healthy"; pre-rename
        // saves carrying "Transparent" must keep loading (the serde alias).
        let ser = serde_json::to_string(&BodyPartState::Healthy).unwrap();
        assert_eq!(ser, "\"Healthy\"");
        let legacy: BodyPartState = serde_json::from_str("\"Transparent\"").unwrap();
        assert_eq!(legacy, BodyPartState::Healthy);
        let modern: BodyPartState = serde_json::from_str("\"Healthy\"").unwrap();
        assert_eq!(modern, BodyPartState::Healthy);
    }

    #[test]
    fn body_part_state_semantic_covers_all_variants() {
        // Catches the "added a variant, forgot semantic()" bug.
        assert_eq!(BodyPartState::Healthy.semantic(), "Healthy");
        assert_eq!(BodyPartState::Yellow.semantic(), "Minor Injury");
        assert_eq!(BodyPartState::Orange.semantic(), "Medium Injury");
        assert_eq!(BodyPartState::Red.semantic(), "Heavy Injury");
        assert_eq!(BodyPartState::Purple.semantic(), "Critical Condition");
        assert_eq!(BodyPartState::Black.semantic(), "Amputated");
    }

    #[test]
    fn body_part_state_can_be_injured() {
        assert!(BodyPartState::Healthy.can_be_injured());
        assert!(BodyPartState::Yellow.can_be_injured());
        assert!(BodyPartState::Red.can_be_injured());
        assert!(BodyPartState::Purple.can_be_injured());
        assert!(!BodyPartState::Black.can_be_injured(), "amputated cannot be re-injured");
    }

    #[test]
    fn body_part_all_has_22_in_anatomical_order() {
        let all = BodyPart::all();
        assert_eq!(all.len(), 22, "spec mandates exactly 22 body parts");
        assert_eq!(all[0], BodyPart::Head, "head first");
        assert_eq!(all[1], BodyPart::Neck, "neck second");
        assert_eq!(all[2], BodyPart::UpperTorso, "upper torso third");
        // Left before right within a pair (the spec order).
        assert_eq!(all[4], BodyPart::LeftShoulder);
        assert_eq!(all[5], BodyPart::RightShoulder);
        assert_eq!(all[21], BodyPart::RightFoot, "right foot last");
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

    #[test]
    fn apply_outcome_core_black_drops_stamina_to_depleted() {
        // Death (a destroyed core part) empties the well — the Depleted
        // floor (2026-08-20: no separate Empty variant).
        let mut s = fresh_state();
        let out = RefereeOutcome {
            part: BodyPart::Neck,
            new_state: BodyPartState::Black,
            stamina_after: Stamina::Winded,
            injury_desc: "Severed".into(),
            lethal: true,
            directive: String::new(),
        };
        apply_outcome(&mut s, &out);
        assert_eq!(s.stamina, Stamina::Depleted);
        assert_eq!(s.body.get(&BodyPart::Neck), Some(&BodyPartState::Black));
        // …but a black LIMB (amputation) only drains as usual.
        let mut s = fresh_state();
        let out = RefereeOutcome {
            part: BodyPart::LeftHand,
            new_state: BodyPartState::Black,
            stamina_after: Stamina::Winded,
            injury_desc: "Severed".into(),
            lethal: false,
            directive: String::new(),
        };
        apply_outcome(&mut s, &out);
        assert_eq!(s.stamina, Stamina::Winded);
    }

    #[test]
    fn recovery_refuses_the_deceased() {
        let mut s = fresh_state();
        s.body.insert(BodyPart::Head, BodyPartState::Black);
        s.stamina = Stamina::Depleted;
        assert!(
            referee_evaluate_recovery("I rest and sleep by the campfire", &s, true).is_none(),
            "a destroyed core part refuses recovery entirely"
        );
    }

    // --- PlayerState ---

    #[test]
    fn player_state_default_is_fully_healthy() {
        let s = fresh_state();
        assert!(s.is_default(), "fresh state must be default");
        assert_eq!(s.stamina, Stamina::Fresh);
        assert_eq!(s.body.len(), 22);
        for part in BodyPart::all() {
            assert_eq!(
                s.body.get(part).copied().unwrap_or_default(),
                BodyPartState::Healthy,
                "{} should be Healthy by default",
                part.display(),
            );
        }
    }

    #[test]
    fn player_state_render_none_when_default() {
        let s = fresh_state();
        assert_eq!(s.render_for_prompt(""), None);
    }

    #[test]
    fn player_state_render_some_when_injured() {
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftUpperArm, BodyPartState::Orange);
        s.stamina = Stamina::Winded;
        let rendered = s.render_for_prompt("").expect("non-default renders");
        assert!(rendered.contains("stamina: Winded"));
        assert!(rendered.contains("injuries: Left Upper Arm (Medium Injury)"));
        // No amputated line when none amputated.
        assert!(!rendered.contains("amputated:"));
    }

    #[test]
    fn player_state_render_lists_amputated_separately() {
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftHand, BodyPartState::Black);
        s.body.insert(BodyPart::RightUpperLeg, BodyPartState::Red);
        let rendered = s.render_for_prompt("").expect("non-default renders");
        // Injuries line excludes the amputated part.
        assert!(rendered.contains("injuries: Right Upper Leg (Heavy Injury)"));
        assert!(!rendered.contains("Left Hand (Amputated)"));
        // Amputated gets its own line.
        assert!(rendered.contains("amputated: Left Hand"));
    }

    #[test]
    fn player_state_render_omits_stamina_only_changes_correctly() {
        // Stamina change alone (no injuries) is still non-default → renders.
        let mut s = fresh_state();
        s.stamina = Stamina::Exhausted;
        let rendered = s.render_for_prompt("").expect("non-default renders");
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
        // A save that only persisted one injured part must load the other 21
        // as Healthy when accessed via the getter (the getter uses
        // unwrap_or_default).
        let json = r#"{"body":{"LeftUpperArm":"Orange"},"stamina":"Active"}"#;
        let s: PlayerState = serde_json::from_str(json).unwrap();
        assert_eq!(s.body.get(&BodyPart::LeftUpperArm).copied().unwrap(), BodyPartState::Orange);
        assert_eq!(
            s.body.get(&BodyPart::Head).copied().unwrap_or_default(),
            BodyPartState::Healthy,
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
        // Every HARD keyword should fire alone (the two-tier gate).
        for kw in COMBAT_HARD_KEYWORDS {
            let text = format!("I {} at the goblin", kw);
            assert!(
                referee_evaluate(&text, &s).is_some(),
                "keyword {:?} should trigger a roll",
                kw,
            );
        }
        // Soft keywords corroborate: one alone fires nothing, two distinct do.
        assert_eq!(
            referee_evaluate("Is Harsk still hunting for the stranger?", &s),
            None,
            "a single soft keyword must not roll injuries (T46)"
        );
        assert!(
            referee_evaluate("They are hunting the stranger and chasing him down the lane.", &s)
                .is_some(),
            "two distinct soft keywords corroborate into a roll"
        );
    }

    #[test]
    fn referee_keyword_match_is_case_insensitive() {
        let s = fresh_state();
        assert!(referee_evaluate("I ATTACK the dragon", &s).is_some());
        assert!(referee_evaluate("I Swing my sword", &s).is_some());
    }

    /// (2026-08-22 playtest) The LIVE referee's avoidance roll: a triggered
    /// exchange is sometimes slipped entirely (no wound, no drain) and
    /// sometimes lands — the raw entry point wounded on EVERY trigger, which
    /// turned six skirmish verbs into six injuries ("gathering injuries like
    /// Pokémon"). Over 200 distinct seeded actions both branches are a
    /// statistical certainty at the 40-80% avoid rates.
    #[test]
    fn referee_live_avoidance_rolls_both_ways() {
        let s = fresh_state();
        assert_eq!(
            referee_evaluate_live("I walk to the bar and order an ale.", &s,
                AttackerTier::Soldier, 0, 0, 0),
            None,
            "the live wrapper keeps the raw gate: no keyword, no roll"
        );
        let mut avoided = false;
        let mut landed = false;
        for i in 0..200u32 {
            let text = format!("I attack the goblin with my sword, sweep #{i}");
            if referee_evaluate_live(&text, &s, AttackerTier::Soldier, 0, 0, 0).is_some() {
                landed = true;
            } else {
                avoided = true;
            }
        }
        assert!(avoided, "some triggered exchanges must be dodged clean");
        assert!(landed, "some triggered exchanges must still land");
    }

    /// (2026-08-22 playtest) Lethality coherence: whatever the save rolled,
    /// a returned lethal outcome only ever rides a Red+ wound on a
    /// non-Downed body (the gate's exemption for already-Downed bodies is
    /// unreachable from a fresh state by construction). Swept over 400
    /// seeded actions at the deadliest tuning (Legendary, Combat pacing).
    #[test]
    fn referee_live_lethal_only_on_red_plus() {
        let s = fresh_state();
        for i in 0..400u32 {
            let text = format!("I lunge at the bugbear and stab, trading blows #{i}");
            if let Some(o) =
                referee_evaluate_live(&text, &s, AttackerTier::Legendary, 0, 0, -1)
            {
                if o.lethal {
                    assert!(
                        o.new_state.rank() >= BodyPartState::Red.rank(),
                        "lethal on a sub-Red wound is incoherent (got {:?})",
                        o.new_state
                    );
                    assert!(!o.directive.is_empty(), "lethal keeps its directive");
                }
            }
        }
    }

    /// (2026-08-22) The player-facing bubble text wording, per Chloe's spec.
    #[test]
    fn referee_notice_text_wording() {
        let mk = |part: BodyPart, state: BodyPartState| {
            let mut o = referee_evaluate("I attack the goblin", &fresh_state())
                .expect("seeded control text fires");
            o.part = part;
            o.new_state = state;
            o
        };
        assert_eq!(
            referee_notice_text(&mk(BodyPart::LeftHand, BodyPartState::Black)),
            "Left Hand is now amputated."
        );
        assert_eq!(
            referee_notice_text(&mk(BodyPart::Head, BodyPartState::Orange)),
            "Head is now moderately injured."
        );
        assert_eq!(
            referee_notice_text(&mk(BodyPart::RightFoot, BodyPartState::Red)),
            "Right Foot is now heavily injured."
        );
        let mut lethal = mk(BodyPart::UpperTorso, BodyPartState::Purple);
        lethal.lethal = true;
        assert_eq!(
            referee_notice_text(&lethal),
            "Upper Torso took a lethal blow — you are DOWNED."
        );
    }

    /// (2026-08-16 P2b) The trailing word boundary, pinned in both
    /// directions: compounds + agent-nouns can never fire their embedded
    /// keyword, while explicitly-listed inflections still do.
    #[test]
    fn keyword_present_enforces_trailing_boundary() {
        // Compounds and derived words must NOT match the embedded keyword.
        assert!(!keyword_present("a runner arrives with a message", "run"),
            "'runner' must not match 'run'");
        assert!(!keyword_present("the firelight flickers", "fire"),
            "'firelight' must not match 'fire'");
        assert!(!keyword_present("we sit by the campfire and eat", "camp"),
            "'campfire' must not match 'camp'");
        assert!(!keyword_present("we sit by the campfire and eat", "fire"),
            "'campfire' must not match 'fire' either side");
        assert!(!keyword_present("I dine at the restaurant", "rest"),
            "'restaurant' must not match 'rest'");
        assert!(!keyword_present("the blockage holds", "block"),
            "'blockage' must not match 'block'");
        // Explicit inflection entries still match (whole words).
        assert!(keyword_present("I am running late", "running"));
        assert!(keyword_present("the goblin is attacking", "attacking"));
        assert!(keyword_present("I was attacked from behind", "attacked"));
        assert!(keyword_present("we swam the river", "swam"));
        // Base forms still match standalone.
        assert!(keyword_present("I attack", "attack"));
        assert!(keyword_present("run!", "run"));
        // Punctuation + end-of-string are valid trailing edges.
        assert!(keyword_present("I run, then rest.", "run"));
        assert!(keyword_present("I run, then rest.", "rest"));
        // Case-insensitivity is the CALLER's contract (text arrives
        // lowercased) — the matcher itself is byte-exact.
        assert!(keyword_present("running", "running"));
        assert!(!keyword_present("RUNNING", "running"));
    }

    /// (2026-08-16 P2b; amended 2026-08-22 Chloe ruling — traversal is
    /// exertion, damage is consequence-only) A narrator beat describing
    /// "a runner" stays silent everywhere (the P2b compound guard). Explicit
    /// "running" still fires scene-pacing's Combat classification (the
    /// shared `combat_triggered` gate is UNTOUCHED) but the combat Referee
    /// itself rolls no injury on transit-only text — the stamina tax routes
    /// at the caller. A hazard or a violence verb alongside re-arms the
    /// roll.
    #[test]
    fn runner_does_not_drain_but_running_does() {
        let s = fresh_state();
        assert_eq!(
            referee_evaluate("A runner arrives at the gate with a message.", &s),
            None,
            "'runner' must not fire the combat Referee"
        );
        assert_ne!(
            crate::scene_pacing::evaluate("A runner arrives at the gate with a message.").mode,
            crate::schema::SceneMode::Combat,
            "'runner' must not classify Combat"
        );
        // (2026-08-22 ruling) Transit-only: no injury roll — but the scene
        // still classifies Combat through the shared gate (the chase is
        // kinetic; world ticks suspend, the DC mods apply).
        assert_eq!(
            referee_evaluate("I am running from the guards.", &s),
            None,
            "'running' alone is traversal — exertion, never a limb injury (2026-08-22 Chloe ruling)"
        );
        assert_ne!(
            crate::scene_pacing::evaluate("I am running from the guards.").mode,
            crate::schema::SceneMode::Combat,
            "'running' must still classify Combat (shared gate untouched)"
        );
        // The playtest's exact case: climbing out of a pit rolled a Yellow
        // elbow + Depleted — pure traversal now rolls nothing.
        assert_eq!(
            referee_evaluate("I start climbing back out of the pit.", &s),
            None,
            "a plain climb rolls no injury"
        );
        // A hazard re-arms the roll (the failed climb that ends in a fall).
        assert!(
            referee_evaluate("I climb out but slip, falling hard back into the pit.", &s)
                .is_some(),
            "a fall alongside transit re-arms the injury roll"
        );
        // A violence verb alongside re-arms the roll.
        assert!(
            referee_evaluate("I run at the goblin and swing my sword.", &s).is_some(),
            "a violence verb alongside transit re-arms the injury roll"
        );
    }

    /// (2026-08-22 Chloe ruling) The pure transit gate: movement alone is
    /// exertion; violence or hazard markers flip it off. Dialogue-stripped,
    /// lowercased input (the referee-gate contract).
    #[test]
    fn transit_only_exertion_gate() {
        let t = |s: &str| {
            let lower = strip_dialogue(s).to_lowercase();
            transit_only_exertion(&lower)
        };
        // Pure traversal.
        assert!(t("I climb out of the pit"));
        assert!(t("I sprint across the field and leap the fence"));
        assert!(t("swimming back to the shore"));
        // Violence re-arms.
        assert!(!t("I climb out and punch the goblin"));
        assert!(!t("I leap at the bugbear, swinging the polearm"));
        // Hazards re-arm.
        assert!(!t("I climb out but slip and fall back down"));
        assert!(!t("the ledge collapses under me mid-climb"));
        assert!(!t("I jump the gap and crash into the far wall"));
        // No transit verb at all → false (not this gate's business).
        assert!(!t("I attack the goblin"));
        assert!(!t("I offer the elf some water"));
        // Dialogue is stripped before the gate: quoted speech never routes.
        assert!(t("*I climb* \"You should see me running up there\" — I keep climbing"));
    }

    /// (2026-08-16 P2b) The recovery Referee's rest keywords under the
    /// boundary matcher: explicit inflections fire, compounds don't.
    #[test]
    fn rest_inflections_fire_recovery_but_restaurant_does_not() {
        // The keyword GATE is what matters: a rest-inflected text must clear
        // it as a whole-word match, not via substring luck.
        for text in [
            "I rest for the night.",
            "We rested until dawn.",
            "I am resting by the hearth.",
            "We camp for the night.",
            "I slept soundly.",
        ] {
            let lower = text.to_lowercase();
            assert!(
                REST_KEYWORDS.iter().any(|kw| keyword_present(&lower, kw)),
                "{text:?} must match a REST_KEYWORD"
            );
        }
        let lower = "I dine at the restaurant".to_lowercase();
        assert!(
            !REST_KEYWORDS.iter().any(|kw| keyword_present(&lower, kw)),
            "'restaurant' must not match any REST_KEYWORD"
        );
        // With something to recover, an inflected rest text under Downtime
        // produces an outcome; the restaurant text does not.
        let mut tired = fresh_state();
        tired.stamina = Stamina::Winded;
        assert!(
            referee_evaluate_recovery("I am resting by the hearth.", &tired, true).is_some(),
            "resting must recover a winded player"
        );
        assert!(
            referee_evaluate_recovery("I dine at the restaurant.", &tired, true).is_none(),
            "'restaurant' must not trigger recovery"
        );
    }

    /// (P1e, 2026-08-17 E4B shakedown) The T22 regression: a QUOTED rest
    /// phrase inside a tavern negotiation healed 3 injury grades across
    /// three turns. Dialogue is speech, not action — the recovery gate
    /// scores on dialogue-stripped text.
    #[test]
    fn quoted_rest_phrase_never_triggers_recovery() {
        let mut tired = fresh_state();
        tired.stamina = Stamina::Winded;
        // The verbatim quoted line from the playtest, embedded in the
        // player's turn.
        assert!(
            referee_evaluate_recovery(
                "I haggle over the pouch. Mara smiles. \"The rest when the heat dies down,\" she says. I slide another coin across the wood.",
                &tired,
                true,
            )
            .is_none(),
            "a quoted 'rest' must not heal anyone (T22)"
        );
        // The same rest spoken by the PLAYER (their own declared action,
        // also quoted) is still speech, not a camp.
        assert!(
            referee_evaluate_recovery("\"I'll rest once this is settled,\" I tell her.", &tired, true)
                .is_none(),
            "spoken intent to rest is not resting"
        );
        // An unquoted actual rest still recovers.
        assert!(
            referee_evaluate_recovery("I rest for the night by the hearth.", &tired, true).is_some(),
            "positive control: an actual rest recovers"
        );
    }

    /// (P1e) The T46/T47 regression on the REFEREE side: gossip about
    /// arrests/hunts/raids rolled real injuries. Single soft verbs never
    /// fire; quoted questions never fire.
    #[test]
    fn gossip_about_violence_never_rolls_injuries() {
        let s = fresh_state();
        assert_eq!(
            referee_evaluate(
                "I eat slowly and keep my voice low. \"Have there been any arrests since the docks? Is Harsk still hunting for the one who cut the moorings?\"",
                &s
            ),
            None,
            "quoted gossip questions must not roll injuries (T46)"
        );
        assert_eq!(
            referee_evaluate("They say the watch raided the eel-shed last night.", &s),
            None,
            "a reported raid is news, not a blow (T47)"
        );
        // Positive controls: direct violence fires.
        assert!(
            referee_evaluate("I stab the thug who grabs my arm.", &s).is_some(),
            "positive control: direct violence rolls"
        );
        assert!(
            referee_evaluate("I shove him back and lunge for the door.", &s).is_some(),
            "positive control: the new hard verbs fire alone"
        );
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
        s.body.insert(BodyPart::LeftUpperArm, BodyPartState::Black);
        // Run many turns with varied text to exercise the RNG across the
        // candidate pool.
        for i in 0..64 {
            let text = format!("I attack the goblin number {}", i);
            let outcome = referee_evaluate(&text, &s).expect("should fire");
            assert_ne!(
                outcome.part, BodyPart::LeftUpperArm,
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
        s.body.insert(BodyPart::UpperTorso, BodyPartState::Red);
        for i in 0..32 {
            let text = format!("I strike the ogre {}", i);
            let outcome = referee_evaluate(&text, &s).expect("should fire");
            if outcome.part == BodyPart::UpperTorso {
                assert!(
                    outcome.new_state.rank() >= BodyPartState::Red.rank(),
                    "Upper Torso roll ({:?}) must not downgrade from Red",
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
            part: BodyPart::RightUpperLeg,
            new_state: BodyPartState::Orange,
            stamina_after: Stamina::Winded,
            injury_desc: "Deep cut".into(),
            lethal: false,
            directive: String::new(),
        };
        apply_outcome(&mut s, &outcome);
        assert_eq!(s.body.get(&BodyPart::RightUpperLeg).copied().unwrap(), BodyPartState::Orange);
        assert_eq!(s.stamina, Stamina::Winded);
        // The wound descriptor is appended to the zone's detail history.
        let details = s.injury_details.get(&BodyPart::RightUpperLeg).unwrap();
        assert_eq!(details, &vec!["Deep cut".to_string()]);
        assert!(!s.is_default());
    }

    #[test]
    fn apply_outcome_appends_to_existing_detail_history() {
        // A zone hit across multiple turns accumulates a real list, not a
        // single overwrite. This is what makes the tooltip's detail list
        // meaningful.
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftUpperArm, BodyPartState::Yellow);
        s.injury_details.insert(
            BodyPart::LeftUpperArm,
            vec!["Scratch".to_string()],
        );
        let outcome = RefereeOutcome {
            part: BodyPart::LeftUpperArm,
            new_state: BodyPartState::Orange,
            stamina_after: Stamina::Active,
            injury_desc: "Gash".into(),
            lethal: false,
            directive: String::new(),
        };
        apply_outcome(&mut s, &outcome);
        let details = s.injury_details.get(&BodyPart::LeftUpperArm).unwrap();
        assert_eq!(
            details,
            &vec!["Scratch".to_string(), "Gash".to_string()]
        );
    }

    #[test]
    fn apply_outcome_amputate_clears_detail_history() {
        // Amputation replaces the wound list with the single "Severed" marker —
        // the prior wound history is no longer meaningful on a missing limb.
        let mut s = fresh_state();
        s.body.insert(BodyPart::LeftHand, BodyPartState::Red);
        s.injury_details.insert(
            BodyPart::LeftHand,
            vec!["Deep gash".to_string(), "Fracture".to_string()],
        );
        let outcome = RefereeOutcome {
            part: BodyPart::LeftHand,
            new_state: BodyPartState::Black,
            stamina_after: Stamina::Exhausted,
            injury_desc: "Severed".into(),
            lethal: true,
            directive: "down".into(),
        };
        apply_outcome(&mut s, &outcome);
        // The body reflects amputation; the detail history is replaced.
        assert_eq!(s.body.get(&BodyPart::LeftHand).copied().unwrap(), BodyPartState::Black);
        let details = s.injury_details.get(&BodyPart::LeftHand).unwrap();
        assert_eq!(details, &vec!["Severed".to_string()]);
    }

    #[test]
    fn render_for_prompt_emits_injury_detail() {
        // The narrator's injuries: line carries the per-zone descriptors so the
        // API narrator reads them as hard fact (closes the schema→narrator loop).
        let mut s = fresh_state();
        s.body.insert(BodyPart::Neck, BodyPartState::Yellow);
        s.injury_details.insert(BodyPart::Neck, vec!["Bruise".to_string()]);
        let rendered = s.render_for_prompt("").unwrap();
        assert!(
            rendered.contains("Neck (Minor Injury): Bruise"),
            "expected the descriptor in the injuries line, got: {rendered}"
        );
    }

    // --- economy fields (2026-08-20) ---

    #[test]
    fn economy_defaults_are_dormant_and_back_compatible() {
        // Fresh default: Squatter + no jobs → still is_default (the empty
        // world-state render contract holds for pre-economy games).
        assert!(PlayerState::default().is_default());
        // A pre-economy save JSON (no lifestyle/jobs keys) loads at the
        // dormant defaults.
        let legacy = r#"{
            "body": {}, "injury_details": {}, "stamina": "Fresh",
            "wealth": 0, "reputation": 0,
            "current_appearance_deltas": {}, "equipment": {}, "belt": [], "pack": []
        }"#;
        let parsed: PlayerState = serde_json::from_str(legacy).expect("legacy save loads");
        assert_eq!(parsed.lifestyle, crate::economy::Lifestyle::Squatter);
        assert!(parsed.jobs.is_empty());
        assert!(parsed.is_default());
    }

    #[test]
    fn lifestyle_and_jobs_render_when_set() {
        let mut s = fresh_state();
        s.wealth = 12;
        s.lifestyle = crate::economy::Lifestyle::Comfortable;
        s.jobs.push(crate::economy::Job {
            title: "Apprentice".into(),
            node_id: "iron-forge".into(),
            daily_wage: 8,
            last_settled_minutes: 0,
            absent_days: 0,
        });
        let rendered = s.render_for_prompt("").unwrap();
        assert!(rendered.contains("wealth: 12"), "naked integer when no currency known: {rendered}");
        assert!(rendered.contains("lifestyle: comfortable"), "tier renders when ≠ Squatter: {rendered}");
        assert!(rendered.contains("job: Apprentice @iron-forge +8/day"), "naked wage, no hardcoded unit: {rendered}");
        // (2026-08-21 addendum) A known currency labels wealth + wages.
        let rendered = s.render_for_prompt("dollars").unwrap();
        assert!(rendered.contains("wealth: 12 dollars"), "{rendered}");
        assert!(rendered.contains("job: Apprentice @iron-forge +8 dollars/day"), "{rendered}");
        // Squatter stays silent.
        s.lifestyle = crate::economy::Lifestyle::Squatter;
        let rendered = s.render_for_prompt("").unwrap();
        assert!(!rendered.contains("lifestyle:"), "Squatter renders nothing");
    }

    #[test]
    fn injury_descriptor_table_covers_injureable_tiers() {
        // Every severity the Referee can produce (Yellow..Purple) yields a
        // non-empty descriptor. Sanity-check the table + the roller path.
        let mut r = Roller::new(42);
        for state in [
            BodyPartState::Yellow,
            BodyPartState::Orange,
            BodyPartState::Red,
            BodyPartState::Purple,
        ] {
            let d = roll_injury_descriptor(&mut r, AttackerTier::Soldier, state);
            assert!(!d.is_empty(), "descriptor for {:?} was empty", state);
        }
        // Black yields the "Severed" marker; Healthy yields empty (no wound).
        assert_eq!(roll_injury_descriptor(&mut r, AttackerTier::Soldier, BodyPartState::Black), "Severed");
        assert_eq!(roll_injury_descriptor(&mut r, AttackerTier::Soldier, BodyPartState::Healthy), "");
    }

    #[test]
    fn injury_descriptor_tier_prefix_escalates() {
        // Heavier attacker tiers prepend a qualifier so the weight class reads
        // at a glance; Minion/Soldier stay unqualified (the common case).
        let mut r = Roller::new(7);
        let minion = roll_injury_descriptor(&mut r, AttackerTier::Minion, BodyPartState::Orange);
        let legendary = roll_injury_descriptor(&mut r, AttackerTier::Legendary, BodyPartState::Orange);
        assert!(!minion.starts_with("Devastating"), "Minion should be unqualified: {minion}");
        assert!(legendary.starts_with("Devastating"), "Legendary should be qualified: {legendary}");
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

    // --- Skill-Check Referee (Phase 2, 2026-07-27) ---

    #[test]
    fn skill_check_lockpick_triggers_on_keyword() {
        let outcomes = referee_evaluate_skill_checks("I pick the lock on the chest.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(outcomes.len(), 1, "lockpick keyword must fire exactly one check");
        assert_eq!(outcomes[0].skill, "lockpick");
        assert_eq!(outcomes[0].dc, 12, "base DC for lockpick with no pacing mod");
        // roll must be in canonical d20 range.
        assert!((1..=20).contains(&outcomes[0].roll), "roll must be 1..=20");
        // success flag must agree with roll vs dc.
        assert_eq!(outcomes[0].success, outcomes[0].roll >= outcomes[0].dc);
        // directive must contain the skill name (capitalized) + DC + outcome.
        let d = &outcomes[0].directive;
        assert!(d.contains("Lockpick"), "directive must name the skill: {d}");
        assert!(d.contains("DC 12"), "directive must include DC: {d}");
        assert!(d.contains("SUCCESS") || d.contains("FAIL"), "directive must state outcome: {d}");
    }

    #[test]
    fn skill_check_no_trigger_on_neutral_text() {
        // The canonical false-positive guard: walking/chatting/looking never
        // triggers a skill check (mirrors referee_combat_keyword behavior).
        assert!(
            referee_evaluate_skill_checks("I walk to the bar and order an ale.", 0, 0, 0, 0, &BTreeMap::new(), false, None).is_empty(),
            "neutral text must not trigger any skill check"
        );
        assert!(
            referee_evaluate_skill_checks("Hello, nice weather today.", 0, 0, 0, 0, &BTreeMap::new(), false, None).is_empty(),
            "smalltalk must not trigger any skill check"
        );
        assert!(
            referee_evaluate_skill_checks("I look at the painting.", 0, 0, 0, 0, &BTreeMap::new(), false, None).is_empty(),
            "looking must not trigger any skill check"
        );
    }

    #[test]
    fn skill_check_keyword_match_is_case_insensitive() {
        // Mixed case must still trigger (the evaluator lowercases the text).
        let upper = referee_evaluate_skill_checks("I PICK THE LOCK.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let mixed = referee_evaluate_skill_checks("I Persuade the guard.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(upper.len(), 1);
        assert_eq!(mixed.len(), 1);
    }

    #[test]
    fn skill_check_deterministic_for_same_text_and_pacing() {
        // Same text + same pacing modifier → same outcomes (RNG is seeded
        // from the text + skill index, so the result is reproducible). This
        // is what makes the Referee testable AND what makes replays stable.
        let a = referee_evaluate_skill_checks("I try to pick the lock.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let b = referee_evaluate_skill_checks("I try to pick the lock.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(a, b, "same text + pacing must produce identical outcomes");
        // Different text → different roll (almost certainly; the hash shifts).
        let c = referee_evaluate_skill_checks("I try to pick the lock again.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_ne!(a[0].roll, c[0].roll, "different text must produce different rolls");
    }

    #[test]
    fn skill_check_multiple_skills_one_turn() {
        // A turn can attempt multiple skills in one breath: each must fire.
        let outcomes = referee_evaluate_skill_checks(
            "I pick the lock, then sneak past the guard.",
            0,
            0,
            0,
            0,
            &BTreeMap::new(),
            false,
            None,
        );
        let skills: Vec<&str> = outcomes.iter().map(|o| o.skill).collect();
        assert!(skills.contains(&"lockpick"), "lockpick must fire: {skills:?}");
        assert!(skills.contains(&"sneak"), "sneak must fire: {skills:?}");
        assert_eq!(outcomes.len(), 2, "exactly two checks expected");
        // The two skills must roll with DIFFERENT dice (per-skill seed offset).
        // We can't assert specific values, but they should differ most of the
        // time — verify the seed offset changes the roll by checking a few
        // texts (statistical sanity, not a hard guarantee for one sample).
        let _ = outcomes; // already checked
    }

    /// (2026-08-23 dynamic DCs) The STAKES ladder: every tier step adds +2
    /// to the skill DC, mirroring `lethality_dc_mod`'s spacing (opposite
    /// sign of effect), and the ladder composes into the DC sum + clamp
    /// exactly like pacing/health/vigor.
    #[test]
    fn skill_dc_mod_ladder_and_stakes_composition() {
        use AttackerTier::*;
        assert_eq!(Minion.skill_dc_mod(), 0);
        assert_eq!(Soldier.skill_dc_mod(), 2);
        assert_eq!(Elite.skill_dc_mod(), 4);
        assert_eq!(Boss.skill_dc_mod(), 6);
        assert_eq!(Legendary.skill_dc_mod(), 8);
        // Composition: persuade (base 14) under a Boss's gaze (+6) → 20;
        // a Legendary (+8) in Combat pacing (+2) clamps toward 30 territory
        // but a plain +8 → 22 stays un-clamped.
        let neutral = referee_evaluate_skill_checks("I persuade the guard.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let boss = referee_evaluate_skill_checks("I persuade the guard.", 0, 0, 0, Boss.skill_dc_mod(), &BTreeMap::new(), false, None);
        let legend_combat =
            referee_evaluate_skill_checks("I persuade the guard.", 2, 0, 0, Legendary.skill_dc_mod(), &BTreeMap::new(), false, None);
        assert_eq!(neutral[0].dc, 14, "persuade base DC");
        assert_eq!(boss[0].dc, 20, "boss stakes +6");
        assert_eq!(legend_combat[0].dc, 24, "legendary +8 stacks with combat +2");
        // The roll never moves — the ladder shifts the DC, not the die
        // (the model never sees a number to nudge).
        assert_eq!(neutral[0].roll, boss[0].roll);
        assert_eq!(neutral[0].roll, legend_combat[0].roll);
    }

    /// (2026-08-23 hazard referees) `parse_skill_rank` parity pins — every
    /// value the panel's `toLevel` ladder produces must map to the same
    /// rank the referee applies (the bar the player sees and the DC mod
    /// are one source).
    #[test]
    fn parse_skill_rank_ladder_parity_with_skills_js() {
        use serde_json::Value;
        let v = |s: &str| Value::String(s.to_string());
        // Keyword ladder.
        assert_eq!(parse_skill_rank(&v("untrained")), 0);
        assert_eq!(parse_skill_rank(&v("none")), 0);
        assert_eq!(parse_skill_rank(&v("novice")), 1);
        assert_eq!(parse_skill_rank(&v("beginner")), 1);
        assert_eq!(parse_skill_rank(&v("apprentice")), 1);
        assert_eq!(parse_skill_rank(&v("adept")), 2);
        assert_eq!(parse_skill_rank(&v("competent")), 2);
        assert_eq!(parse_skill_rank(&v("junior")), 2);
        assert_eq!(parse_skill_rank(&v("skilled")), 3);
        assert_eq!(parse_skill_rank(&v("proficient")), 3);
        assert_eq!(parse_skill_rank(&v("expert")), 4);
        assert_eq!(parse_skill_rank(&v("veteran")), 4);
        assert_eq!(parse_skill_rank(&v("senior")), 4);
        assert_eq!(parse_skill_rank(&v("master")), 5);
        assert_eq!(parse_skill_rank(&v("mastery")), 5);
        assert_eq!(parse_skill_rank(&v("legendary")), 5);
        // Unmatched non-empty reads as the JS 50% fallback → 2.
        assert_eq!(parse_skill_rank(&v("quite good")), 2);
        // Numbers: fraction / 0-10 scale / percent.
        assert_eq!(parse_skill_rank(&Value::Number(serde_json::Number::from(3))), 3);
        assert_eq!(parse_skill_rank(&v("3")), 3);
        assert_eq!(parse_skill_rank(&v("0.8")), 4); // 80% → round(4)
        assert_eq!(parse_skill_rank(&v("10")), 5); // scale top, clamped
        assert_eq!(parse_skill_rank(&v("80")), 4); // percent → round(4)
        assert_eq!(parse_skill_rank(&v("100")), 5);
        assert_eq!(parse_skill_rank(&v("0")), 0);
        // Non-scalars read as untrained.
        assert_eq!(parse_skill_rank(&Value::Null), 0);
        assert_eq!(parse_skill_rank(&v("  ")), 0);
    }

    /// (2026-08-23 hazard referees) The prof mod: stem→entity matching +
    /// the one-directional ladder + the dice-never-reseed invariant.
    #[test]
    fn skill_prof_dc_mod_stem_matching_and_dc_threading() {
        use serde_json::Value;
        // A declared master lockpick lowers the lockpick DC by 4…
        let mut entities = BTreeMap::new();
        entities.insert("skill_lockpick".to_string(), Value::String("master".into()));
        let base = referee_evaluate_skill_checks("I pick the lock.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let prof = referee_evaluate_skill_checks("I pick the lock.", 0, 0, 0, 0, &entities, false);
        assert_eq!(prof[0].dc, base[0].dc - 4, "master → −4");
        assert_eq!(prof[0].roll, base[0].roll, "the prof mod must NOT reseed the dice");
        // …and leaves OTHER skills untouched (stem matching is per-skill).
        let sneak = referee_evaluate_skill_checks("I sneak past the guard.", 0, 0, 0, 0, &entities, false);
        let sneak_base =
            referee_evaluate_skill_checks("I sneak past the guard.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(sneak[0].dc, sneak_base[0].dc, "a lockpick rank must not touch sneak");
        // Synonym keys match through the stems: skill_stealth lowers sneak.
        let mut stealth_entities = BTreeMap::new();
        stealth_entities.insert("skill_stealth".to_string(), Value::String("expert".into()));
        let stealth = referee_evaluate_skill_checks("I sneak past the guard.", 0, 0, 0, 0, &stealth_entities, false);
        assert_eq!(stealth[0].dc, sneak_base[0].dc - 3, "skill_stealth expert → −3");
        // Explicitly untrained changes NOTHING (one-directional — a hostile
        // "skill_sneak: terrible" must not raise the DC above undeclared).
        let mut untrained = BTreeMap::new();
        untrained.insert("skill_sneak".to_string(), Value::String("untrained".into()));
        let same = referee_evaluate_skill_checks("I sneak past the guard.", 0, 0, 0, 0, &untrained, false);
        assert_eq!(same[0].dc, sneak_base[0].dc, "untrained → no mod");
        // The BEST rank wins among multiple matching entities.
        let mut two = BTreeMap::new();
        two.insert("skill_locks".to_string(), Value::String("apprentice".into()));
        two.insert("skill_lockpicking".to_string(), Value::String("veteran".into()));
        let best = referee_evaluate_skill_checks("I pick the lock.", 0, 0, 0, 0, &two, false);
        assert_eq!(best[0].dc, base[0].dc - 3, "best of {1, 4} → −3");
        // Unrelated skill_* entities never match.
        let mut other = BTreeMap::new();
        other.insert("skill_cooking".to_string(), Value::String("master".into()));
        let untouched = referee_evaluate_skill_checks("I pick the lock.", 0, 0, 0, 0, &other, false);
        assert_eq!(untouched[0].dc, base[0].dc);
        // Non-skill entities never match.
        let mut npc = BTreeMap::new();
        npc.insert("npc.guard.tier".to_string(), Value::String("legendary".into()));
        let clean = referee_evaluate_skill_checks("I pick the lock.", 0, 0, 0, 0, &npc, false);
        assert_eq!(clean[0].dc, base[0].dc);
    }

    #[test]
    fn skill_check_pacing_dc_mod_applies() {
        // +2 pacing mod raises the effective DC by 2; -2 lowers it by 2.
        let neutral = referee_evaluate_skill_checks("I intimidate the thug.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let combat = referee_evaluate_skill_checks("I intimidate the thug.", 2, 0, 0, 0, &BTreeMap::new(), false, None);
        let downtime = referee_evaluate_skill_checks("I intimidate the thug.", -2, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(neutral.len(), 1);
        assert_eq!(combat.len(), 1);
        assert_eq!(downtime.len(), 1);
        assert_eq!(neutral[0].dc, 13, "intimidate base DC");
        assert_eq!(combat[0].dc, 15, "combat pacing +2");
        assert_eq!(downtime[0].dc, 11, "downtime pacing -2");
        // roll must be identical across the three (same text → same seed,
        // the dc modifier doesn't reseed).
        assert_eq!(neutral[0].roll, combat[0].roll);
        assert_eq!(neutral[0].roll, downtime[0].roll);
        // The directive must include the EFFECTIVE DC, not the base.
        assert!(combat[0].directive.contains("DC 15"), "directive must show effective DC");
        assert!(downtime[0].directive.contains("DC 11"), "directive must show effective DC");
    }

    #[test]
    fn skill_check_health_dc_mod_applies() {
        // (2026-08-20) The derived health tier feeds an additive DC modifier
        // exactly like pacing: a Poor body (+2) hardens every attempt, and
        // the modifier must not reseed the dice.
        let neutral = referee_evaluate_skill_checks("I intimidate the thug.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let hurt = referee_evaluate_skill_checks("I intimidate the thug.", 0, 2, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(neutral[0].dc, 13, "intimidate base DC");
        assert_eq!(hurt[0].dc, 15, "health +2 raises DC like pacing +2");
        assert_eq!(neutral[0].roll, hurt[0].roll, "modifier must not reseed the dice");
    }

    #[test]
    fn skill_check_dc_clamps_at_1_and_30() {
        // A pathological pacing modifier can't push DC out of [1, 30].
        let low = referee_evaluate_skill_checks("I pick the lock.", -100, 0, 0, 0, &BTreeMap::new(), false, None);
        let high = referee_evaluate_skill_checks("I pick the lock.", 100, 0, 0, 0, &BTreeMap::new(), false, None);
        assert_eq!(low[0].dc, 1, "DC must clamp at 1");
        assert_eq!(high[0].dc, 30, "DC must clamp at 30");
        // DC 1 with d20 (1..=20) → only a natural 1 fails. Almost always succeeds.
        // DC 30 → always fails (max roll is 20).
        assert!(high[0].success == false, "DC 30 must always fail");
    }

    #[test]
    fn skill_check_excludes_combat_keywords() {
        // "I attack" is a combat keyword — the skill Referee must NOT fire on it.
        // The combat Referee (referee_evaluate) owns that action.
        let skill_outcomes = referee_evaluate_skill_checks("I attack the goblin with my sword.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        assert!(
            skill_outcomes.is_empty(),
            "skill Referee must not fire on combat keywords (combat Referee owns them): {skill_outcomes:?}"
        );
        // Sanity: the combat Referee DOES fire on the same text.
        let combat = referee_evaluate("I attack the goblin with my sword.", &fresh_state());
        assert!(combat.is_some(), "combat Referee must fire on combat keyword");
    }

    #[test]
    fn skill_check_auto_pass_forces_success_with_success_seed() {
        // (2026-08-23 Playground) God-mode auto-pass: every triggered skill
        // forces SUCCESS + the success seed, while the dice + DC still roll
        // (the report shows what WOULD have happened). A DC-30 clamp (an
        // always-fail without the flag) pins the override: forced SUCCESS
        // despite the unreachable DC. Determinism doc-pin: the flag is an
        // explicit input, not a seed change — same text + same flag repeats.
        let forced = referee_evaluate_skill_checks(
            "I pick the lock.",
            100, // DC clamp → 30 (always-fail without god mode)
            0,
            0,
            0,
            &BTreeMap::new(),
            true,
            None,
        );
        assert!(!forced.is_empty(), "the check must still fire under auto-pass");
        assert_eq!(forced[0].dc, 30);
        assert!(forced[0].success, "auto-pass forces success even past DC 30");
        assert!(
            forced[0].directive.contains("SUCCESS"),
            "directive renders the forced outcome: {}",
            forced[0].directive
        );
        assert!(
            forced[0].directive.contains("the lock clicks open"),
            "directive carries the SUCCESS seed under auto-pass: {}",
            forced[0].directive
        );
        let repeat = referee_evaluate_skill_checks("I pick the lock.", 100, 0, 0, 0, &BTreeMap::new(), true, None);
        assert_eq!(forced[0].roll, repeat[0].roll, "auto-pass is an input, not a seed change");
        // Without the flag the same text + DC stays an honest fail.
        let honest = referee_evaluate_skill_checks("I pick the lock.", 100, 0, 0, 0, &BTreeMap::new(), false, None);
        assert!(!honest[0].success, "DC 30 still fails without the god flag");
    }

    /// (2026-08-24 Part II B3) A Crossroads-declared DC ("— [Lockpicking
    /// DC 18]") is COMMITTED at offer time — the next turn's referee uses it
    /// verbatim for the matching skill (dice + seeds untouched), and a
    /// different skill's check computes its DC normally.
    #[test]
    fn pinned_dc_overrides_matching_skill_only() {
        let computed = referee_evaluate_skill_checks(
            "I pick the lock on the chest.",
            0, 0, 0, 0,
            &BTreeMap::new(),
            false,
            None,
        );
        let pinned = referee_evaluate_skill_checks(
            "I pick the lock on the chest.",
            0, 0, 0, 0,
            &BTreeMap::new(),
            false,
            Some(("Lockpicking", 18)),
        );
        assert_eq!(pinned[0].dc, 18, "the declared DC wins verbatim");
        assert_ne!(computed[0].dc, 18, "fixture sanity: computed DC differs");
        assert_eq!(pinned[0].roll, computed[0].roll, "dice + seeds untouched");
        // A pin for a DIFFERENT skill never touches this check's DC.
        let bystander = referee_evaluate_skill_checks(
            "I pick the lock on the chest.",
            0, 0, 0, 0,
            &BTreeMap::new(),
            false,
            Some(("Persuasion", 5)),
        );
        assert_eq!(bystander[0].dc, computed[0].dc);
        // The directive line reports the pinned DC.
        assert!(pinned[0].directive.contains("(DC 18)"), "{}", pinned[0].directive);
    }

    /// (2026-08-24 Part II B8) The rank-advance diff: strictly-deepened
    /// skills report with prettified names; demotions, unchanged ranks, and
    /// non-skill keys never do. A new key counts as 0 → n (a knack roots).
    #[test]
    fn detect_skill_advances_reports_only_deepened_ranks() {
        use serde_json::Value;
        let mut before = BTreeMap::new();
        before.insert("skill_lockpicking".to_string(), Value::String("novice".into()));
        before.insert("skill_sneak".to_string(), Value::String("expert".into()));
        before.insert("char.mira.trust".to_string(), Value::String("wary".into()));
        let mut after = before.clone();
        after.insert("skill_lockpicking".to_string(), Value::String("adept".into()));
        after.insert("skill_sneak".to_string(), Value::String("skilled".into())); // demotion
        after.insert("skill_intimidation".to_string(), Value::String("apprentice".into())); // new
        let advances = detect_skill_advances(&before, &after);
        assert_eq!(advances.len(), 2, "{advances:?}");
        assert!(advances.contains(&("Lockpicking".to_string(), 1, 2)));
        assert!(advances.contains(&("Intimidation".to_string(), 0, 1)));
        // No non-skill key ever reports.
        assert!(!advances.iter().any(|(s, _, _)| s.contains("mira")));
    }

    #[test]
    fn capitalize_first_handles_edge_cases() {
        assert_eq!(capitalize_first("lockpick"), "Lockpick");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
    }

    // --- Phase 3: combat-keyword / scene-pacing sync ---

    /// The combat-keyword gate `player_state::combat_triggered`
    /// (`COMBAT_HARD_KEYWORDS` / `COMBAT_SOFT_KEYWORDS`) and scene-pacing's
    /// kinetic pillar now share ONE implementation — the scene_pacing
    /// `KINETIC_COMBAT` mirror list was deleted in the 2026-08-17 P1e
    /// two-tier split, so the two can no longer drift apart. If they ever
    /// disagree again, the combat Referee could fire on a turn the
    /// scene-pacing classifies as non-Combat (or vice versa), which would
    /// break the anti-sycophancy contract (Referee injury with no combat
    /// pacing directive, or combat pacing with no injury).
    ///
    /// This test re-declares the canonical HARD combat list here and asserts
    /// (a) every keyword fires `referee_evaluate` AND (b) every keyword
    /// triggers `SceneMode::Combat` via `scene_pacing::evaluate` — the
    /// re-declaration is the load-bearing check that the shared gate is
    /// wired into BOTH consumers.
    const TEST_COMBAT_KEYWORDS: &[&str] = &[
        "attack", "swing", "strike", "slash", "stab", "punch", "kick", "block", "dodge",
        "parry", "shoot", "fire", "cast", "throw", "tackle", "grapple", "charge",
        "run", "sprint", "climb", "jump", "leap", "swim",
        // (P1e) first-person violence additions
        "shove", "duck", "lunge",
        // inflected forms (2026-08-16 P2b) — the trailing boundary requires
        // these as explicit entries; the sync test pins that they ALL fire.
        "attacks", "attacked", "attacking",
        "swings", "swung", "swinging",
        "strikes", "struck", "striking",
        "slashes", "slashed", "slashing",
        "stabs", "stabbed", "stabbing",
        "punches", "punched", "punching",
        "kicks", "kicked", "kicking",
        "blocks", "blocked", "blocking",
        "dodges", "dodged", "dodging",
        "parries", "parried", "parrying",
        "shoots", "shot", "shooting",
        "fires", "fired", "firing",
        "casts", "casting",
        "throws", "threw", "thrown", "throwing",
        "tackles", "tackled", "tackling",
        "grapples", "grappled", "grappling",
        "charges", "charged", "charging",
        "runs", "running", "ran",
        "sprints", "sprinted", "sprinting",
        "climbs", "climbed", "climbing",
        "jumps", "jumped", "jumping",
        "leaps", "leapt", "leaping", "leaped",
        "swims", "swam", "swimming",
        "shoves", "lunges", "ducks",
    ];

    #[test]
    fn combat_keywords_match_scene_pacing_combat() {
        for kw in TEST_COMBAT_KEYWORDS {
            // Embed the keyword in a sentence so substring matching is
            // exercised (not just bare-word equality).
            let text = format!("I {kw} now.");
            let referee = referee_evaluate(&text, &fresh_state());
            let pacing = crate::scene_pacing::evaluate(&text);
            assert!(
                referee.is_some(),
                "referee_evaluate must fire on combat keyword {kw:?}"
            );
            assert_eq!(
                pacing.mode,
                crate::schema::SceneMode::Combat,
                "scene_pacing must classify combat keyword {kw:?} as Combat (got {:?})",
                pacing.mode
            );
        }
    }

    // --- Slice 3 (2026-07-28): tier-aware Referee + lethality judgment ---

    #[test]
    fn attacker_tier_default_is_soldier() {
        // The default tier is Soldier; the 2026-08-20 five-tier rescale
        // (Black tail added) keeps the shipped shape: Yellow-dominant, the
        // old Orange 30 preserved, severe outcomes the minority.
        assert_eq!(AttackerTier::default(), AttackerTier::Soldier);
        assert_eq!(AttackerTier::Soldier.severity_weights(), [55, 30, 8, 5, 2]);
    }

    #[test]
    fn attacker_tier_severity_weights_shift_with_tier() {
        // Minion weights toward Minor (index 0); Legendary toward Red/Purple
        // (indices 2-3). The shift must be monotonic per index.
        let minion = AttackerTier::Minion.severity_weights();
        let legendary = AttackerTier::Legendary.severity_weights();
        assert!(
            minion[0] > legendary[0],
            "Minion should weight Minor more heavily than Legendary"
        );
        assert!(
            legendary[3] > minion[3],
            "Legendary should weight Critical more heavily than Minion"
        );
        // (2026-08-20) Black exists on every tier's table (no free zeros):
        // the tail grows with tier and never sums past 100.
        for tier in [
            AttackerTier::Minion,
            AttackerTier::Soldier,
            AttackerTier::Elite,
            AttackerTier::Boss,
            AttackerTier::Legendary,
        ] {
            let w = tier.severity_weights();
            assert_eq!(w.iter().sum::<u32>(), 100, "{tier:?} weights must sum to 100");
            assert!(*w.last().unwrap() > 0, "{tier:?} must carry a Black tail");
        }
        assert!(
            legendary[4] > minion[4],
            "Legendary should weight Black more heavily than Minion"
        );
        // Sanity: the five-tier shift ladder is ordered Minion→Soldier→Elite→
        // Boss→Legendary, with each step increasing severe-outcome weight.
        let tiers = [
            AttackerTier::Minion,
            AttackerTier::Soldier,
            AttackerTier::Elite,
            AttackerTier::Boss,
            AttackerTier::Legendary,
        ];
        for window in tiers.windows(2) {
            let lower = window[0].severity_weights();
            let higher = window[1].severity_weights();
            // Combined Red+Purple+Black weight must increase with tier.
            let lower_severe = lower[2] + lower[3] + lower[4];
            let higher_severe = higher[2] + higher[3] + higher[4];
            assert!(
                higher_severe >= lower_severe,
                "tier {:?} should have at least as much severe weight as {:?}",
                window[1],
                window[0]
            );
        }
    }

    #[test]
    fn referee_default_tier_preserves_v1_behavior() {
        // The default referee_evaluate (Soldier tier) must produce the SAME
        // outcomes the v1 referee did for the same text + state. This is the
        // backwards-compat contract: existing call sites see no behavior
        // change until they explicitly opt into tier-aware rolling.
        let s = fresh_state();
        let a = referee_evaluate("I swing my longsword at the goblin chieftain", &s);
        let b = referee_evaluate_with_tier(
            "I swing my longsword at the goblin chieftain",
            &s,
            AttackerTier::Soldier,
            0,
            0,
            0,
        );
        assert_eq!(a, b, "default and explicit-Soldier paths must agree");
    }

    #[test]
    fn referee_outcome_carries_lethality_fields() {
        // Every outcome now has the lethal flag + directive string, even
        // when non-lethal (the fields default to false / empty). This pins
        // the API contract so callers can rely on them. A fresh body vs the
        // default Soldier tier (save DC 22) is UNREACHABLE on a d20 — the
        // outcome must be non-lethal with an empty directive. Under the old
        // inverted comparison this failed on both counts.
        let s = fresh_state();
        let outcome = referee_evaluate("I attack the goblin.", &s).expect("should fire");
        assert!(
            !outcome.lethal,
            "Soldier vs fresh body: save DC 22 exceeds the d20 — never lethal"
        );
        assert!(
            outcome.directive.is_empty(),
            "non-lethal outcome must carry no directive (got: {})",
            outcome.directive
        );
    }

    #[test]
    fn referee_lethality_fires_for_legendary_on_wounded_body() {
        // The architect's defining example: a Legendary-tier attacker on a
        // badly-wounded body should be lethal most of the time. We can't
        // pin a specific roll (RNG), but across many trials lethality must
        // fire most of the time. The save math (BASE 18 + Legendary −8 +
        // Battered −4 = DC 6, so any roll 6..=20 fails the save = 75%
        // per trial) makes the half-bound below a polarity discriminator:
        // the old inverted comparison sat at 25% and cannot reach 32/64.
        let mut s = fresh_state();
        // Battered: one Heavy wound.
        s.body.insert(BodyPart::UpperTorso, BodyPartState::Red);
        let mut lethal_count = 0;
        for i in 0..64 {
            let text = format!("I attack the dragon again, turn {i}");
            if let Some(o) = referee_evaluate_with_tier(&text, &s, AttackerTier::Legendary, 0, 0, 0) {
                if o.lethal {
                    lethal_count += 1;
                    // Lethal outcomes MUST carry a non-empty directive.
                    assert!(
                        !o.directive.is_empty(),
                        "lethal outcome must carry a directive"
                    );
                    assert!(
                        o.directive.contains("DOWNED"),
                        "lethal directive must mention DOWNED: {}",
                        o.directive
                    );
                }
            }
        }
        assert!(
            lethal_count >= 32,
            "Legendary vs Battered must be lethal in at least half of 64 trials (got {lethal_count}/64); \
             less indicates the lethality save comparison is inverted again"
        );
    }

    #[test]
    fn referee_lethality_rarely_fires_for_minion_on_healthy_body() {
        // The opposite extreme: a Minion attacking a fresh body should
        // almost never be lethal. Save DC = 18 + 8 (Minion) + 0 (Unscathed)
        // = 26 — UNREACHABLE on a d20 (max 20), so a healthy player can
        // never be lethally dropped by a Minion. The text MUST contain a
        // combat keyword ("attacks", not "bites" — the old text never fired
        // the referee, making this test vacuous). Across 100 trials we
        // allow a small slack bound, but under the corrected comparison the
        // expected count is exactly 0; the old inverted comparison scored
        // 100/100.
        let s = fresh_state();
        let mut lethal_count = 0;
        let mut fired = 0;
        for i in 0..100 {
            let text = format!("the rat attacks me, turn {i}");
            if let Some(o) = referee_evaluate_with_tier(&text, &s, AttackerTier::Minion, 0, 0, 0) {
                fired += 1;
                if o.lethal {
                    lethal_count += 1;
                }
            }
        }
        assert!(
            fired > 0,
            "test text must actually trigger the combat referee (vacuous otherwise)"
        );
        assert!(
            lethal_count <= 15,
            "Minion vs Unscathed should be lethal ≤15/100 trials (got {lethal_count}/100); \
             higher indicates the lethality threshold is broken",
        );
    }

    #[test]
    fn referee_repeat_hit_never_downgrades_or_free_escalates() {
        // (2026-08-20 Chloe ruling — replaces the Slice 3 escalation rule)
        // A re-hit never escalates AND never downgrades: the wound tier only
        // moves when the severity roll actually lands there (or worse).
        // Pre-wound the Upper Torso to Orange; every repeat hit must keep
        // Orange (a lighter roll) or earn Red/Purple/Black (the roll landing
        // there) — never Yellow. Black is EARNED by the roll's tuned tail
        // only, never as a free escalation from a lighter blow.
        let mut s = fresh_state();
        s.body.insert(BodyPart::UpperTorso, BodyPartState::Orange);
        let mut torso_hits = 0;
        for i in 0..200 {
            // Vary the text to spread the RNG across parts.
            let text = format!("I strike the bandit, exchange {i}");
            if let Some(o) = referee_evaluate_with_tier(&text, &s, AttackerTier::Elite, 0, 0, 0) {
                if o.part == BodyPart::UpperTorso {
                    torso_hits += 1;
                    assert!(
                        !matches!(o.new_state, BodyPartState::Yellow),
                        "Upper Torso repeat-hit must not downgrade from Orange (got {:?})",
                        o.new_state
                    );
                }
            }
        }
        // Sanity: across 200 trials the Upper Torso should have been hit at
        // least a few times (1/22 parts = ~9 expected). If it's 0 the test is
        // meaningless; assert we actually exercised the path.
        assert!(torso_hits > 0, "Upper Torso must be hit at least once in 200 trials");
    }

    #[test]
    fn attacker_tier_tag_for_directive_is_lowercase_word() {
        // The directive sentence uses these tags; they must read as natural
        // English words (lowercase, no underscores).
        assert_eq!(AttackerTier::Minion.tag_for_directive(), "minion");
        assert_eq!(AttackerTier::Legendary.tag_for_directive(), "legendary");
        for tier in [
            AttackerTier::Minion,
            AttackerTier::Soldier,
            AttackerTier::Elite,
            AttackerTier::Boss,
            AttackerTier::Legendary,
        ] {
            let tag = tier.tag_for_directive();
            assert!(!tag.is_empty());
            assert!(!tag.contains('_'), "tag must be a natural word: {tag}");
        }
    }

    // ---- Phase 4 §11.44 (Component 1): Disguise Referee ----

    use crate::consequence::{Polarity, StatusTag};

    fn disguise_tag(label: &str) -> StatusTag {
        StatusTag {
            label: label.into(),
            // polarity is irrelevant when kind=disguise (the renderer routes
            // by kind, not polarity); use Buff as a sane default.
            polarity: Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: "disguise".into(),
        }
    }

    fn generic_tag(label: &str) -> StatusTag {
        StatusTag {
            label: label.into(),
            polarity: Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(),
        }
    }

    fn entities_with_tier(tier: &str) -> BTreeMap<String, serde_json::Value> {
        let mut m = BTreeMap::new();
        m.insert(
            "npc.guard1.tier".into(),
            serde_json::Value::String(tier.into()),
        );
        m
    }

    #[test]
    fn disguise_gate_none_when_no_disguise_tag() {
        // No disguise tag → nothing to gate, regardless of tier or behavior.
        let entities = entities_with_tier("soldier");
        let present = vec!["guard1".to_string()];
        assert!(evaluate_disguise_gate("I walk past the guard", &[generic_tag("Blessed")], &entities, &present, 0, 0, 0, 0).is_none());
    }

    #[test]
    fn disguise_gate_none_on_empty_scene() {
        // (#55) Nobody on camera → no gate outcome at all: the old path fell
        // through to the Soldier default and AutoPassed with invisible
        // soldiers vouching the player through — even on suspicious text.
        let entities = entities_with_tier("soldier");
        assert!(evaluate_disguise_gate(
            "I sneak along the corridor and pick the lock",
            &[disguise_tag("city guard uniform")],
            &entities,
            &[],
            0,
            0,
            0,
            0
        )
        .is_none());
    }

    #[test]
    fn disguise_gate_autopass_minion_confident_walkby() {
        let entities = entities_with_tier("minion");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I nod to the drunk guard and march into the keep.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("minion + confident → AutoPass");
        match out {
            DisguiseDirective::AutoPass { label, tier_tag } => {
                assert_eq!(label, "city guard uniform");
                assert_eq!(tier_tag, "minion");
            }
            _ => panic!("expected AutoPass, got {out:?}"),
        }
    }

    #[test]
    fn disguise_gate_autopass_soldier_confident_walkby() {
        // The goldilocks cutoff: Soldier is the v1 default tier, so most
        // NPCs auto-pass. This is the "confident walk-by" fantasy.
        let entities = entities_with_tier("soldier");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I flash my badge and stride through the gate.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("soldier + confident → AutoPass");
        assert!(matches!(out, DisguiseDirective::AutoPass { tier_tag: "soldier", .. }));
    }

    #[test]
    fn disguise_gate_elite_confident_rolls_scrutiny() {
        // (P2 contract) Elite+ scrutinize by default — the gate NOW rolls the
        // Deception check itself (the old None assumed the keyword-gated
        // skill referee would roll it, which it never does on neutral text).
        // Even a confident walk-by faces a harder DC (14 + 3).
        let entities = entities_with_tier("elite");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I nod to the captain and walk past.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("Elite+ + disguise -> Scrutinized (rolled here)");
        let dc_seen = match &out {
            DisguiseDirective::Scrutinized { dc, .. } => *dc,
            _ => panic!("expected Scrutinized, got {out:?}"),
        };
        assert_eq!(dc_seen, 17, "Elite DC = DECEPTION_BASE_DC + 3 scrutiny");
    }

    #[test]
    fn disguise_gate_legendary_confident_rolls_scrutiny() {
        let entities = entities_with_tier("legendary");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I salute the dragon and walk past.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("Legendary + disguise -> Scrutinized (rolled here)");
        assert!(matches!(out, DisguiseDirective::Scrutinized { .. }));
    }

    #[test]
    fn disguise_gate_scrutinized_minion_suspicious() {
        // Suspicious behavior revokes the auto-pass even for minions.
        let entities = entities_with_tier("minion");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I sweat nervously, avoid eye contact, and try to slip past.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("suspicious → Scrutinized");
        match out {
            DisguiseDirective::Scrutinized { label, dc, roll, success, .. } => {
                assert_eq!(label, "city guard uniform");
                assert_eq!(dc, 14, "DC = DECEPTION_BASE_DC + 0 (Downtime not applied here)");
                assert!(roll >= 1 && roll <= 20);
                assert_eq!(success, roll >= dc);
            }
            _ => panic!("expected Scrutinized, got {out:?}"),
        }
    }

    #[test]
    fn disguise_gate_scrutinized_soldier_suspicious() {
        let entities = entities_with_tier("soldier");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I stammer, fumble my badge, and mumble an excuse.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("soldier + suspicious → Scrutinized");
        assert!(matches!(out, DisguiseDirective::Scrutinized { .. }));
    }

    #[test]
    fn disguise_gate_elite_suspicious_rolls_scrutiny() {
        // (P2 contract) Elite+ suspicious -> the same harder-DC Scrutinized
        // roll (never a silent None).
        let entities = entities_with_tier("elite");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I sweat nervously and stammer at the captain.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            0,
            0,
            0,
        ).expect("Elite+ + suspicious → Scrutinized");
        match out {
            DisguiseDirective::Scrutinized { dc, .. } => assert_eq!(dc, 17),
            _ => panic!("expected Scrutinized, got {out:?}"),
        }
    }

    #[test]
    fn disguise_gate_scrutinized_dc_threads_pacing_modifier() {
        // Combat (+2) makes scrutiny harder; Downtime (−2) easier. Same shape
        // as the §11.21 skill-check DC threading.
        let entities = entities_with_tier("soldier");
        let present = vec!["guard1".to_string()];
        let combat = evaluate_disguise_gate(
            "I sweat and stammer.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            2,
            0,
            0,
            0,
        ).unwrap();
        let downtime = evaluate_disguise_gate(
            "I sweat and stammer.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            -2,
            0,
            0,
            0,
        ).unwrap();
        match (combat, downtime) {
            (DisguiseDirective::Scrutinized { dc: dc_c, .. },
             DisguiseDirective::Scrutinized { dc: dc_d, .. }) => {
                assert_eq!(dc_c, 16, "Combat DC = 14 + 2");
                assert_eq!(dc_d, 12, "Downtime DC = 14 - 2");
            }
            _ => panic!("both should be Scrutinized"),
        }
    }

    #[test]
    fn disguise_gate_dc_threads_health_modifier() {
        // (2026-08-20 Chloe ruling) Deception under scrutiny is a skilled
        // act — the derived-body health modifier hardens it exactly like a
        // skill-check DC: +4 turns the base 14 into 18, the Elite 17 into 21.
        let entities = entities_with_tier("soldier");
        let present = vec!["guard1".to_string()];
        let out = evaluate_disguise_gate(
            "I sweat and stammer.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            4,
            0,
            0,
        ).unwrap();
        match out {
            DisguiseDirective::Scrutinized { dc, .. } => {
                assert_eq!(dc, 18, "low-tier scrutiny DC = 14 + health 4");
            }
            _ => panic!("expected Scrutinized"),
        }
        let entities = entities_with_tier("elite");
        let out = evaluate_disguise_gate(
            "I nod to the captain and walk past.",
            &[disguise_tag("city guard uniform")],
            &entities,
            &present,
            0,
            4,
            0,
            0,
        ).unwrap();
        match out {
            DisguiseDirective::Scrutinized { dc, .. } => {
                assert_eq!(dc, 21, "Elite scrutiny DC = 14 + 3 + health 4");
            }
            _ => panic!("expected Scrutinized"),
        }
    }

    #[test]
    fn disguise_gate_ignores_expired_disguise() {
        // (2026-08-20) The gate runs on the clock: an expired disguise tag
        // reads as NO disguise — no auto-pass narrating a costume that timed
        // out, no scrutiny roll, no revoke.
        let entities = entities_with_tier("soldier");
        let present = vec!["guard1".to_string()];
        let expired = StatusTag {
            label: "city guard uniform".into(),
            polarity: Polarity::Buff,
            expires_at: 100,
            source: String::new(),
            kind: "disguise".into(),
        };
        assert!(
            evaluate_disguise_gate(
                "I nod to the guard and walk past confidently.",
                &[expired],
                &entities,
                &present,
                0,
                0,
                0,
                100
            )
            .is_none(),
            "an expired disguise gates nothing"
        );
        // One minute earlier it was live and would auto-pass.
        assert!(matches!(
            evaluate_disguise_gate(
                "I nod to the guard and walk past confidently.",
                &[expired],
                &entities,
                &present,
                0,
                0,
                0,
                99
            ),
            Some(DisguiseDirective::AutoPass { .. })
        ));
    }

    #[test]
    fn suspicious_action_detector_flags_nervous_tells() {
        assert!(has_suspicious_action("I sweat and tremble as I walk past."));
        assert!(has_suspicious_action("I avoid eye contact with the guard."));
        assert!(has_suspicious_action("I creep along the wall in uniform."));
        assert!(has_suspicious_action("I salute wrong and the guard frowns."));
    }

    #[test]
    fn suspicious_action_detector_clean_when_confident() {
        // Confident, casual behavior — no tells, no auto-pass revoke.
        assert!(!has_suspicious_action("I nod to the guard and walk past."));
        assert!(!has_suspicious_action("I flash my badge and stride through."));
        assert!(!has_suspicious_action("I greet the sentry by name and enter."));
    }

    #[test]
    fn disguise_directive_render_autopass_reads_as_hard_fact() {
        let d = DisguiseDirective::AutoPass {
            label: "city guard uniform".into(),
            tier_tag: "soldier",
        };
        let r = d.render();
        assert!(r.contains("ACCEPTED"), "AutoPass render: {r}");
        assert!(r.contains("city guard uniform"), "render: {r}");
        assert!(r.contains("soldier"), "render: {r}");
        assert!(r.contains("do not challenge"), "render: {r}");
    }

    #[test]
    fn disguise_directive_render_scrutinized_carries_dice_facts() {
        let d = DisguiseDirective::Scrutinized {
            label: "merchant robes".into(),
            dc: 14,
            roll: 7,
            success: false,
            seed: "the act cracks under scrutiny",
        };
        let r = d.render();
        assert!(r.contains("SCRUTINIZED"), "render: {r}");
        assert!(r.contains("DC 14"), "render: {r}");
        assert!(r.contains("FAIL"), "render: {r}");
        assert!(r.contains("roll 7"), "render: {r}");
    }

    #[test]
    fn find_disguise_tag_returns_first_live_disguise() {
        let tags = vec![
            generic_tag("Blessed"),
            disguise_tag("city guard uniform"),
            disguise_tag("merchant robes"), // second disguise ignored
        ];
        let found = find_disguise_tag(&tags, 0).expect("must find the disguise");
        assert_eq!(found.label, "city guard uniform");
        // (2026-08-20) The gate's clock: an expired disguise is skipped in
        // favor of the next live one — the tick's sweep is suspended in
        // Combat, so read-time filtering is the authority.
        let expired = StatusTag {
            label: "stale hood".into(),
            polarity: Polarity::Buff,
            expires_at: 50,
            source: String::new(),
            kind: "disguise".into(),
        };
        let tags = vec![expired, disguise_tag("merchant robes")];
        let found = find_disguise_tag(&tags, 60).expect("must find the LIVE disguise");
        assert_eq!(found.label, "merchant robes");
        assert!(
            find_disguise_tag(&tags[..1], 60).is_none(),
            "an all-expired disguise set gates nothing"
        );
    }

    #[test]
    fn attacker_tier_ord_ladder_is_threat_order() {
        // The derived Ord must give Minion < Soldier < Elite < Boss < Legendary.
        // This is load-bearing for the `tier > Soldier` gate comparison.
        assert!(AttackerTier::Minion < AttackerTier::Soldier);
        assert!(AttackerTier::Soldier < AttackerTier::Elite);
        assert!(AttackerTier::Elite < AttackerTier::Boss);
        assert!(AttackerTier::Boss < AttackerTier::Legendary);
    }

    // --- Recovery Referee (2026-08-15 recovery-seam pins) ---

    /// (2026-08-22 living-world) Pin the rest-fatigue clamp mapping: the
    /// floors per band + the only-lowers contract (each floor sits strictly
    /// below the fresh states it may clamp, and the deeper band floors
    /// strictly below the weary one — `if current > floor` at the call site
    /// then never raises a state).
    #[test]
    fn fatigue_floors_pin_the_rest_clamp_mapping() {
        let (weary_s, weary_m) = fatigue_floors("weary");
        assert_eq!(weary_s, Stamina::Winded);
        assert_eq!(weary_m, Mana::Strained);
        let (deep_s, deep_m) = fatigue_floors("exhausted");
        assert_eq!(deep_s, Stamina::Exhausted);
        assert_eq!(deep_m, Mana::Drained);
        assert!(Stamina::Fresh > weary_s, "only-lowers: Fresh sits above the weary floor");
        assert!(Mana::Steady > weary_m, "only-lowers: Steady sits above the weary floor");
        assert!(Stamina::Winded > deep_s, "the deeper band floors below the weary floor");
        assert!(Mana::Strained > deep_m, "the deeper band floors below the weary floor");
    }

    /// Downtime-gating: no rest keyword or no Downtime classification → the
    /// referee stays silent (healing is an active choice, not a default).
    #[test]
    fn referee_evaluate_recovery_gates_on_downtime_and_keyword() {
        let mut s = fresh_state();
        s.stamina = Stamina::Winded;
        s.body.insert(BodyPart::Head, BodyPartState::Orange);
        // No Downtime → never fires, even with the keyword.
        assert!(referee_evaluate_recovery("I rest by the hearth.", &s, false).is_none());
        // Downtime but no rest keyword → silent.
        assert!(referee_evaluate_recovery("I stare at the map.", &s, true).is_none());
        // Both gates open → fires.
        assert!(referee_evaluate_recovery("I rest by the hearth.", &s, true).is_some());
        // "watch" is deliberately NOT a rest keyword (a sentry is not resting).
        assert!(referee_evaluate_recovery("I watch the road.", &s, true).is_none());
    }

    /// Worst-injury selection: the highest-severity healable part improves one
    /// grade; amputations never heal; a fully-healthy body recovers stamina only.
    #[test]
    fn referee_evaluate_recovery_heals_worst_injury_and_skips_amputations() {
        let mut s = fresh_state();
        s.stamina = Stamina::Fresh; // fully rested: stamina won't move
        s.body.insert(BodyPart::LeftHand, BodyPartState::Yellow);
        s.body.insert(BodyPart::Head, BodyPartState::Purple);
        s.body.insert(BodyPart::RightFoot, BodyPartState::Black); // amputated
        let out = referee_evaluate_recovery("we camp for the night", &s, true)
            .expect("resting with injuries must fire");
        assert!(!out.stamina_recovered, "Fresh stamina has nothing to recover");
        // Purple Head outranks Yellow LeftHand; Black is never healable.
        assert_eq!(out.healed, Some((BodyPart::Head, BodyPartState::Red)));
        // Apply: head advances one grade, everything else untouched.
        apply_recovery(&mut s, &out);
        assert_eq!(s.body.get(&BodyPart::Head), Some(&BodyPartState::Red));
        assert_eq!(s.body.get(&BodyPart::LeftHand), Some(&BodyPartState::Yellow));
        assert_eq!(s.body.get(&BodyPart::RightFoot), Some(&BodyPartState::Black));
    }

    /// A Yellow (minor) wound heals to healthy in ONE rest: the part's entry
    /// AND its injury history leave the maps (the clean-delete contract).
    #[test]
    fn apply_recovery_removes_fully_healed_part_and_history() {
        let mut s = fresh_state();
        s.stamina = Stamina::Fresh;
        s.body.insert(BodyPart::Neck, BodyPartState::Yellow);
        s.injury_details.insert(BodyPart::Neck, vec!["bruised in a fall".into()]);
        let out = referee_evaluate_recovery("I sleep", &s, true)
            .expect("resting with a minor wound must fire");
        assert_eq!(out.healed, Some((BodyPart::Neck, BodyPartState::Healthy)));
        apply_recovery(&mut s, &out);
        assert!(!s.body.contains_key(&BodyPart::Neck), "a fully healed part leaves the body map");
        assert!(!s.injury_details.contains_key(&BodyPart::Neck), "its injury history goes with it");
        // Resting while fully healthy → nothing to recover → silent.
        assert!(referee_evaluate_recovery("I sleep", &s, true).is_none());
    }

    /// One rest = ONE stamina tier up (the monotonic economy's single exit).
    #[test]
    fn apply_recovery_advances_stamina_exactly_one_tier() {
        let mut s = fresh_state();
        s.stamina = Stamina::Depleted;
        let out = referee_evaluate_recovery("I sleep", &s, true).expect("must fire");
        assert!(out.stamina_recovered);
        apply_recovery(&mut s, &out);
        assert_eq!(s.stamina, Stamina::Exhausted, "Depleted recovers to Exhausted, never further");
    }

    // ---- (2026-08-22 living-world) the arcane pool + vigor DC -----------

    #[test]
    fn mana_mirrors_stamina_ladder() {
        let mut m = Mana::Surging;
        for _ in 0..6 {
            m.drain();
        }
        assert_eq!(m, Mana::Spent, "drain stops at the floor");
        for _ in 0..6 {
            m.recover();
        }
        assert_eq!(m, Mana::Surging, "recover stops at the ceiling");
        // The alignment table's bonuses.
        assert_eq!(Mana::Surging.dc_bonus(), 4);
        assert_eq!(Mana::Steady.dc_bonus(), 2);
        assert_eq!(Mana::Strained.dc_bonus(), 0);
        assert_eq!(Mana::Drained.dc_bonus(), -2);
        assert_eq!(Mana::Spent.dc_bonus(), -4);
        assert_eq!(Stamina::Fresh.dc_bonus(), 4);
        assert_eq!(Stamina::Winded.dc_bonus(), 0);
        assert_eq!(Stamina::Depleted.dc_bonus(), -4);
    }

    #[test]
    fn dormant_mana_renders_nothing_active_renders_under_stamina() {
        // Dormant: the default state renders no line even when the block is
        // up (an injury forces the block) — the zero-token invariant.
        let mut s = fresh_state();
        s.body.insert(BodyPart::Neck, BodyPartState::Yellow);
        let block = s.render_for_prompt("").expect("block renders");
        assert!(!block.contains("mana"), "dormant pool is invisible: {block}");

        // Activated: the label line renders directly under stamina.
        s.mana = Some(Mana::Strained);
        s.mana_label = "biotics".into();
        let block = s.render_for_prompt("").expect("block renders");
        let stamina_at = block.find("stamina: Fresh").expect("stamina line");
        let mana_at = block.find("biotics: Strained").expect("mana line");
        assert!(mana_at > stamina_at, "mana renders under stamina: {block}");
        // A Some-with-empty-label save falls back to "mana".
        s.mana_label = String::new();
        let block = s.render_for_prompt("").expect("block renders");
        assert!(block.contains("mana: Strained"), "label fallback: {block}");
        // An active pool makes the state non-default (the block renders
        // even at full health — the tracker needs to see the pool).
        let mut clean = fresh_state();
        assert!(clean.is_default());
        clean.mana = Some(Mana::Surging);
        assert!(!clean.is_default(), "active pool renders the block");
    }

    #[test]
    fn vigor_dc_mod_takes_the_worse_pool() {
        // Dormant pool: stamina alone.
        assert_eq!(vigor_dc_mod(Stamina::Fresh, None), -4);
        assert_eq!(vigor_dc_mod(Stamina::Winded, None), 0);
        assert_eq!(vigor_dc_mod(Stamina::Depleted, None), 4);
        // Active pool: the worse of the two grades (player bonus negated
        // into harder-positive DC units).
        assert_eq!(vigor_dc_mod(Stamina::Fresh, Some(Mana::Strained)), 0);
        assert_eq!(vigor_dc_mod(Stamina::Exhausted, Some(Mana::Surging)), 2);
        assert_eq!(vigor_dc_mod(Stamina::Fresh, Some(Mana::Spent)), 4);
    }

    #[test]
    fn skill_check_dc_threads_vigor_modifier() {
        // Same text + pacing: a Depleted body hardens the DC by 4 vs Fresh.
        let fresh = referee_evaluate_skill_checks("I intimidate the thug.", 0, 0, 0, 0, &BTreeMap::new(), false, None);
        let spent = referee_evaluate_skill_checks("I intimidate the thug.", 0, 0, 4, 0, &BTreeMap::new(), false, None);
        assert_eq!(fresh[0].dc + 4, spent[0].dc, "vigor threads like pacing/health");
        assert_eq!(fresh[0].roll, spent[0].roll, "the modifier never reseeds the dice");
    }
}
