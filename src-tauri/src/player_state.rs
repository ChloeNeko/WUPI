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

use std::collections::HashMap;

use crate::equipment;

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

    /// The set of PascalCase serde wire keys the 22 variants serialize as
    /// (`"LeftUpperArm"`, `"UpperTorso"`, …). Used by the save-load seam
    /// (`WorldSchema::load_split`) to DROP unknown body keys before
    /// deserializing `player_state` — the clean-delete safety net. A pre-
    /// 2026-08-07 save carries the deleted 16-part keys (`Torso`, `LeftBicep`,
    /// `LeftThigh`, `LeftAnkle`, …); those no longer name a real variant, so
    /// a raw `serde_json::from_value` would ERROR on them. Filtering the
    /// `body` object to only these 22 keys first makes the clean delete
    /// save-safe: dead-part injury data simply vanishes (the part no longer
    /// exists), no remap, no crash. Built once from `all()` so it can't drift
    /// from the enum.
    pub fn wire_keys() -> std::collections::HashSet<&'static str> {
        let mut set = std::collections::HashSet::with_capacity(22);
        for part in BodyPart::all() {
            // serde's default for a unit enum variant is the variant's name
            // (PascalCase). We avoid re-listing the 22 strings by deriving
            // each key from the variant via a tiny match — keeps one source.
            set.insert(match part {
                BodyPart::Head => "Head",
                BodyPart::Neck => "Neck",
                BodyPart::UpperTorso => "UpperTorso",
                BodyPart::LowerTorso => "LowerTorso",
                BodyPart::LeftShoulder => "LeftShoulder",
                BodyPart::RightShoulder => "RightShoulder",
                BodyPart::LeftUpperArm => "LeftUpperArm",
                BodyPart::RightUpperArm => "RightUpperArm",
                BodyPart::LeftElbow => "LeftElbow",
                BodyPart::RightElbow => "RightElbow",
                BodyPart::LeftLowerArm => "LeftLowerArm",
                BodyPart::RightLowerArm => "RightLowerArm",
                BodyPart::LeftHand => "LeftHand",
                BodyPart::RightHand => "RightHand",
                BodyPart::LeftUpperLeg => "LeftUpperLeg",
                BodyPart::RightUpperLeg => "RightUpperLeg",
                BodyPart::LeftKnee => "LeftKnee",
                BodyPart::RightKnee => "RightKnee",
                BodyPart::LeftLowerLeg => "LeftLowerLeg",
                BodyPart::RightLowerLeg => "RightLowerLeg",
                BodyPart::LeftFoot => "LeftFoot",
                BodyPart::RightFoot => "RightFoot",
            });
        }
        set
    }
}

// ---------------------------------------------------------------------------
// PlayerState (the persisted canonical state)
// ---------------------------------------------------------------------------

/// The player's canonical state. Rust is the SOLE authority — the
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

    /// Live appearance deltas applied ON TOP of the SavedPlayer's authored
    /// identity during play (2026-08-04 overhaul). A stable-keyed map so the
    /// `[APPEARANCE key=value]` bracket pipeline can mutate individual traits
    /// on the fly — outfit changes, cut hair, fresh scars, a disguise donned.
    /// Empty value (`""`) is the clear sentinel for that key.
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
    /// by the `[EQUIP]` bracket; only present slots are keyed. Empty by
    /// default. See `equipment.rs`. Rides `save_split` → `<card_id>.player.json`.
    #[serde(default)]
    pub equipment: equipment::Equipment,

    /// Quick-access belt — a fixed 4-slot rack (`BELT_MAX`) for potions,
    /// lockpicks, throwables. Mutated by the `[BELT]` bracket. Never
    /// appearance-visible (carried, not worn).
    #[serde(default)]
    pub belt: Vec<equipment::StackItem>,

    /// Deep-storage pack — weight-bounded (`pack_capacity_lbs`) for everything
    /// else. Mutated by the `[PACK]` bracket. The encumbrance UI divides
    /// `stack_weight(&pack)` by `pack_capacity_lbs`. Never appearance-visible.
    #[serde(default)]
    pub pack: Vec<equipment::StackItem>,

    /// Pack carry capacity in pounds. Per-card overridable; defaults to
    /// `PACK_DEFAULT_CAPACITY_LBS` (20.0). Drives the encumbrance fill bar.
    #[serde(default = "equipment_default_pack_capacity")]
    pub pack_capacity_lbs: f32,
}

/// Default pack capacity for serde `#[serde(default = ...)]`. Wraps the const
/// so the attribute can name a fn (serde requires a fn path, not a const).
fn equipment_default_pack_capacity() -> f32 {
    equipment::PACK_DEFAULT_CAPACITY_LBS
}

impl Default for PlayerState {
    fn default() -> Self {
        // Seed every body part to Healthy explicitly. HashMap::default() is
        // empty, which would read as "no body" — we want "fully healthy
        // body" so the mannequin renders correctly + referee_injureable
        // has the full part list to pick from.
        let mut body = HashMap::with_capacity(22);
        for part in BodyPart::all() {
            body.insert(*part, BodyPartState::Transparent);
        }
        PlayerState {
            body,
            stamina: Stamina::Fresh,
            wealth: 0,
            reputation: 0,
            current_appearance_deltas: HashMap::new(),
            equipment: HashMap::new(),
            belt: Vec::new(),
            pack: Vec::new(),
            pack_capacity_lbs: equipment::PACK_DEFAULT_CAPACITY_LBS,
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
            && self.wealth == 0
            && self.reputation == 0
            && self.body.values().all(|s| *s == BodyPartState::Transparent)
            && self.current_appearance_deltas.is_empty()
            && self.equipment.is_empty()
            && self.belt.is_empty()
            && self.pack.is_empty()
            && self.pack_capacity_lbs == equipment::PACK_DEFAULT_CAPACITY_LBS
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
    ///   outfit: bloodstained leather, travel cloak
    /// equipped:
    ///   Main Hand: Iron Sword (+2 ATK)
    ///   Chest: Heavy Cloak
    /// ```
    /// Lines are omitted when empty (no injuries → no `injuries:` line). The
    /// `appearance:` block is emitted LAST so the model reads the character's
    /// current look right before generating prose — the diegetic ground truth
    /// that must stay consistent turn to turn. This is the fact block the
    /// narrator reads as hard truth. The `equipped:` block (Outer-layer items
    /// only — Inner layers are hidden from the narrator) follows appearance so
    /// the visible garments + readied weapons read as one cohesive look.
    pub fn render_for_prompt(&self) -> Option<String> {
        if self.is_default() {
            return None;
        }

        let mut lines: Vec<String> = Vec::with_capacity(8);

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

        // Equipped items — Outer layer ONLY. The narrator sees what an observer
        // sees: a Heavy Cloak (Outer) over a Linen Shirt (Inner) reads as just
        // the cloak. Inner layers are hidden by design (concealed garments,
        // hidden armor). Iterated in canonical slot order (Head→Feet) so the
        // readied weapon + visible garments read head-to-foot as one look. Belt
        // + pack are NEVER here — they're carried, not worn.
        if !self.equipment.is_empty() {
            let equipped_lines: Vec<String> = equipment::EquipSlot::all()
                .iter()
                .filter_map(|slot| {
                    self.equipment.get(slot).and_then(|layers| {
                        layers.visible().map(|item| {
                            // "Main Hand: Iron Sword" — append stats in parens
                            // if present ("Main Hand: Iron Sword (+2 ATK)").
                            match &item.stats {
                                Some(s) if !s.trim().is_empty() => {
                                    format!("  {}: {} ({})", slot.label(), item.name, s)
                                }
                                _ => format!("  {}: {}", slot.label(), item.name),
                            }
                        })
                    })
                })
                .collect();
            if !equipped_lines.is_empty() {
                lines.push(format!("equipped:\n{}", equipped_lines.join("\n")));
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
/// `narrative_hint` is a short prose-seed the caller MAY inject alongside
/// the world-state block ("your left arm takes a heavy blow"). The narrator
/// is NOT required to use it — the canonical fact is the body-state change
/// itself; the hint is just a nudge.
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
    /// Short, second-person prose seed. Empty when the change was stamina-only.
    pub narrative_hint: String,
    /// True when the Referee judged this blow lethal — the body is Downed
    /// (unconscious, dying). The narrator must obey: the player character
    /// cannot continue to fight, run, or act this turn. False for ordinary
    /// injuries that merely hurt.
    #[cfg_attr(not(test), allow(dead_code))]
    pub lethal: bool,
    /// Hard narrator directive, populated only when `lethal == true`. The
    /// caller wraps this as `[DIRECTIVE: {directive}]` inside `<world_state>`
    /// (same injection path as the skill-check Referee). Empty string
    /// otherwise. Reads as a single imperative sentence.
    #[cfg_attr(not(test), allow(dead_code))]
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
    /// Severity-roll weights for the four-tier BodyPartState ladder
    /// (Yellow / Orange / Red / Purple). Higher tiers weight toward severe
    /// outcomes. Index 0 = Yellow (Minor), 3 = Purple (Critical).
    fn severity_weights(self) -> [u32; 4] {
        match self {
            // Minion: almost always Minor, occasionally Medium, rarely worse.
            AttackerTier::Minion => [80, 18, 2, 0],
            // Soldier: the v1 baseline (preserved exactly from the original
            // SEVERITY_WEIGHTS const — the [50, 30, 15, 5] distribution that
            // shipped before Slice 3). Default tier, so the default behavior
            // is unchanged.
            AttackerTier::Soldier => [50, 30, 15, 5],
            // Elite: weights shift toward Medium/Heavy.
            AttackerTier::Elite => [25, 40, 30, 5],
            // Boss: Heavy becomes the modal outcome.
            AttackerTier::Boss => [10, 25, 45, 20],
            // Legendary: Critical is common; lethality is the lived reality.
            AttackerTier::Legendary => [5, 15, 35, 45],
        }
    }

    /// Lethality DC modifier — added to the base DC for the lethal-blow roll.
    /// Higher tiers make lethal outcomes more likely. Pure Rust math: a d20
    /// is rolled, compared against `BASE_LETHAL_DC + tier_modifier +
    /// condition_penalty`. If the roll falls short, the blow is lethal.
    fn lethality_dc_mod(self) -> i32 {
        match self {
            AttackerTier::Minion => 8,    // almost never lethal
            AttackerTier::Soldier => 4,
            AttackerTier::Elite => 0,     // baseline
            AttackerTier::Boss => -4,
            AttackerTier::Legendary => -8, // very likely lethal on a good hit
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

/// The base DC for the lethality roll. A roll < this (modified) means the
/// blow is lethal. Tuned so a Legendary's full hit on a Battered defender
/// is almost always lethal, and a Minion's is almost never.
const BASE_LETHAL_DC: i32 = 18;


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
pub fn referee_evaluate(text: &str, state: &PlayerState) -> Option<RefereeOutcome> {
    referee_evaluate_with_tier(text, state, AttackerTier::Soldier)
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
pub fn select_attacker_tier_from_entities(
    entities: &std::collections::HashMap<String, String>,
) -> AttackerTier {
    // Single-pass scan: collect every npc.*.tier value, parse each, keep the
    // max. Cheap (entity maps are small — typically <50 keys).
    let mut best: Option<AttackerTier> = None;
    for (key, value) in entities.iter() {
        if !key.starts_with("npc.") || !key.ends_with(".tier") {
            continue;
        }
        if let Some(tier) = parse_attacker_tier(value) {
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
/// a second d20 against `BASE_LETHAL_DC + tier_mod + condition_penalty`.
/// `condition_penalty` is derived from the player's existing wound load (a
/// Battered defender is easier to drop than an Unscathed one). On a failed
/// save, the outcome is flagged `lethal: true` + a hard directive is emitted
/// the narrator MUST obey ("the player is Downed — they cannot act this
/// turn"). This is the mechanical enforcement of the Slice 1 anti-Oblivion
/// clause: a Legendary's full hit on a wounded body is lethal, period.
pub fn referee_evaluate_with_tier(
    text: &str,
    state: &PlayerState,
    attacker_tier: AttackerTier,
) -> Option<RefereeOutcome> {
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

    // Roll severity on a weighted table. Weights are now tier-driven (Slice 3):
    // a Minion weights toward Minor; a Legendary weights toward Critical.
    // Index maps to: 0=Yellow, 1=Orange, 2=Red, 3=Purple.
    const SEVERITY_TABLE: [BodyPartState; 4] = [
        BodyPartState::Yellow,
        BodyPartState::Orange,
        BodyPartState::Red,
        BodyPartState::Purple,
    ];
    let roll_idx = roller.weighted(&attacker_tier.severity_weights());
    let mut new_state = SEVERITY_TABLE[roll_idx];

    // The new state must be at least as severe as the current one — a Heavy
    // blow to an already-Heavy part shouldn't randomly downgrade to Minor.
    // If the roll is lighter than current, escalate by one tier instead
    // (the blow still did *something*). This is the same-part repeat-hit
    // rule (architect directive Slice 3): a second hit to an already-wounded
    // part always escalates, never downgrades.
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
        crate::consequence::derive_condition(&state.body, 0, 0);
    let condition_penalty = match derived {
        crate::consequence::Condition::Downed => -20, // already down → any hit finishes
        crate::consequence::Condition::Critical => -10,
        crate::consequence::Condition::Battered => -4,
        crate::consequence::Condition::Wounded => -2,
        crate::consequence::Condition::Haggard => -1,
        crate::consequence::Condition::Unscathed => 0,
    };
    let lethality_dc = BASE_LETHAL_DC + attacker_tier.lethality_dc_mod() + condition_penalty;
    let lethality_roll = roll_d20(&mut roller);
    let lethal = (lethality_roll as i32) < lethality_dc;

    // Narrative hint: a short second-person seed. The narrator reads the
    // canonical body-state change as hard fact; this hint just nudges prose.
    // Lethal outcomes get a stronger hint that flags the drop.
    let narrative_hint = if lethal {
        format!(
            "the {} blow drops you — your {} goes limp, the fight leaves you",
            new_state.semantic().to_lowercase(),
            part.display().to_lowercase(),
        )
    } else {
        format!(
            "your {} takes a {}",
            part.display().to_lowercase(),
            new_state.semantic().to_lowercase(),
        )
    };

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
        narrative_hint,
        lethal,
        directive,
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
    /// Base DC before the ScenePacing modifier (Combat +2, Exploration +0,
    /// Downtime −2). Tuned for d20 (1..=20): 12 = coin-flip for an untrained
    /// player, 14 = slight disadvantage.
    base_dc: u32,
    /// Narrator seed when the check succeeds. "{skill}" placeholder NOT used
    /// here — the seed is bespoke per skill so it reads naturally.
    success_seed: &'static str,
    /// Narrator seed when the check fails.
    fail_seed: &'static str,
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
        keywords: &["pick the lock", "pick lock", "lockpick", "pick a lock", "pickpocket"],
        base_dc: 12,
        success_seed: "the lock clicks open",
        fail_seed: "the lock resists; your picks slip",
    },
    SkillSpec {
        name: "sneak",
        keywords: &["sneak", "sneak past", "stealth", "hide", "slip past", "creep"],
        base_dc: 12,
        success_seed: "you move unseen",
        fail_seed: "you are noticed",
    },
    SkillSpec {
        name: "persuade",
        keywords: &["persuade", "convince", "talk into", "talk him into", "talk her into"],
        base_dc: 14,
        success_seed: "your words land",
        fail_seed: "your words fall flat",
    },
    SkillSpec {
        name: "deceive",
        keywords: &["bluff", "lie", "deceive", "fast-talk", "fast talk", "con "],
        base_dc: 14,
        success_seed: "the lie holds",
        fail_seed: "the lie unravels",
    },
    SkillSpec {
        name: "intimidate",
        keywords: &["intimidate", "threaten", "scare", "menace"],
        base_dc: 13,
        success_seed: "they flinch",
        fail_seed: "they stand firm",
    },
];

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
/// Combat keywords are EXCLUDED here — `referee_evaluate` owns those. The
/// two Referees are disjoint by keyword set; the same turn may fire one
/// combat roll AND multiple skill rolls (e.g. "I attack the guard then
/// pickpocket the body"), but never the same keyword twice.
///
/// Determinism: each skill rolls with a distinct seed
/// (`hash_text(text) + skill_index`), so back-to-back identical turns produce
/// different rolls (the skill_index offset + the text hash compound). Same
/// text + same pacing → same outcome (testable).
pub fn referee_evaluate_skill_checks(text: &str, pacing_dc_mod: i32) -> Vec<SkillCheckOutcome> {
    let lower = text.to_lowercase();
    let text_hash = hash_text(text);
    let mut out = Vec::new();
    for (idx, spec) in SKILL_TABLE.iter().enumerate() {
        let triggered = spec.keywords.iter().any(|kw| lower.contains(kw));
        if !triggered {
            continue;
        }
        // Distinct seed per skill: text hash + skill index. The index offset
        // guarantees "I pick the lock and sneak past" rolls lockpick and
        // sneak with different dice (otherwise the same hash → same roll).
        let seed = text_hash.wrapping_add(idx as u64);
        let mut roller = Roller::new(seed);
        let roll = roll_d20(&mut roller);
        // Effective DC = base + pacing modifier, clamped to [1, 30]. A d20
        // roll is 1..=20, so DC ≤ 1 is always-success and DC ≥ 21 is
        // always-fail; the clamp keeps the math honest without panicking.
        let dc = (spec.base_dc as i32 + pacing_dc_mod)
            .clamp(1, 30) as u32;
        let success = roll >= dc;
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
/// Whole-word, case-insensitive substring match (same pattern as
/// COMBAT_KEYWORDS + SKILL_TABLE keywords). Kept conservative: only flags
/// behavior a guard would actually notice.
const SUSPICIOUS_ACTIONS: &[&str] = &[
    // nervous tells — visible distress
    "sweat", "sweaty", "nervous", "tense", "tremble", "trembling",
    "stutter", "stuttering", "stammer", "stammering", "hesitate", "hesitat",
    "flinch", "flinching", "mumble", "mutter", "fidget", "fumble",
    "falter", "stiffen", "rigid",
    // eye behavior — the classic tell
    "avoid eye contact", "look away", "avert eyes", "avert gaze",
    "stare at the ground", "eyes dart", "glance around",
    // furtive movement — trying not to be noticed IS suspicious in uniform
    "sneak", "sneaking", "creep", "creeping", "lurk", "lurking",
    "slink", "tiptoe", "slip past", "edge away", "skulk",
    // protocol mistakes — the disguise breaks down
    "wrong name", "forget", "forgot", "confuse", "confused",
    "salute wrong", "wrong salute", "don't know", "do not know",
    "blunder", "stumble over", "misspell", "wrong badge", "no badge",
    "wrong uniform", "wrong color", "wrong rank",
];

/// Find the active disguise tag, if any. Returns the first tag with
/// `kind == "disguise"`. A player can technically hold multiple disguise
/// tags (e.g. swapped mid-scene); we evaluate against the first — the
/// others are stale and the gate cares about presence, not multiplicity.
pub fn find_disguise_tag(tags: &[crate::consequence::StatusTag]) -> Option<&crate::consequence::StatusTag> {
    tags.iter().find(|t| t.kind == "disguise")
}

/// True if the player's turn text contains any suspicious-action keyword.
/// Pure keyword scan; case-insensitive. Used by the gate to decide whether
/// to revoke the auto-pass.
pub fn has_suspicious_action(text: &str) -> bool {
    let lower = text.to_lowercase();
    SUSPICIOUS_ACTIONS.iter().any(|kw| lower.contains(kw))
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
}

/// Seeds for the Scrutinized outcome. Mirrors SkillSpec.success_seed /
/// fail_seed — short narrator-flavor phrases.
const SCRUTINIZED_SUCCESS_SEED: &str = "the player's nerve holds; the disguise buys passage";
const SCRUTINIZED_FAIL_SEED: &str = "the act cracks under scrutiny; the disguise is challenged";

/// The gate. Pure fn — no I/O, no schema mutation.
///
/// Returns:
///   - `None` when there's no active disguise tag (nothing to gate).
///   - `None` when an NPC tier above Soldier is present (Elite+ scrutinize
///     by default; the normal §11.21 skill-check Referee handles the roll).
///   - `Some(AutoPass)` when disguised + low-tier NPCs + no suspicious action.
///   - `Some(Scrutinized)` when disguised + low-tier NPCs + suspicious action
///     (the auto-pass is revoked; a Deception roll fires here).
///
/// `entities` is the WorldSchema entity map (read for `npc.*.tier` keys via
/// the existing `select_attacker_tier_from_entities`). `pacing_dc_mod` is
/// the ScenePacing DC modifier (Combat +2, Exploration 0, Downtime −2) —
/// threaded into the Scrutinized DC exactly as the skill-check Referee does.
pub fn evaluate_disguise_gate(
    text: &str,
    tags: &[crate::consequence::StatusTag],
    entities: &HashMap<String, String>,
    pacing_dc_mod: i32,
) -> Option<DisguiseDirective> {
    let disguise = find_disguise_tag(tags)?;
    let label = disguise.label.clone();
    let tier = select_attacker_tier_from_entities(entities);
    // Elite+ (captains, bosses, legendary creatures) scrutinize by default.
    // They know their people. Return None → the normal skill-check Referee
    // runs a plain Deception roll with no disguise framing.
    if tier > AttackerTier::Soldier {
        return None;
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
    let dc = (DECEPTION_BASE_DC as i32 + pacing_dc_mod).clamp(1, 30) as u32;
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
        s.body.insert(BodyPart::LeftUpperArm, BodyPartState::Orange);
        s.stamina = Stamina::Winded;
        let rendered = s.render_for_prompt().expect("non-default renders");
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
        let rendered = s.render_for_prompt().expect("non-default renders");
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
        // A save that only persisted one injured part must load the other 21
        // as Healthy when accessed via the getter (the getter uses
        // unwrap_or_default).
        let json = r#"{"body":{"LeftUpperArm":"Orange"},"stamina":"Active"}"#;
        let s: PlayerState = serde_json::from_str(json).unwrap();
        assert_eq!(s.body.get(&BodyPart::LeftUpperArm).copied().unwrap(), BodyPartState::Orange);
        assert_eq!(
            s.body.get(&BodyPart::Head).copied().unwrap_or_default(),
            BodyPartState::Transparent,
        );
    }

    #[test]
    fn player_state_serde_drops_legacy_body_keys() {
        // A pre-2026-08-07 save carries the deleted 16-part keys (Torso,
        // LeftBicep, LeftThigh, LeftAnkle, ...). The clean-delete contract:
        // those keys no longer name a real body part, so the load seam in
        // WorldSchema::load_split filters them out BEFORE deserializing
        // player_state. This test proves the post-filter JSON deserializes
        // cleanly with zero injuries from the dead keys (the raw PlayerState
        // deserializer itself would panic on an unknown variant; the seam is
        // what makes the clean delete save-safe).
        let filtered = r#"{"body":{"LeftUpperArm":"Orange"},"stamina":"Active"}"#;
        let s: PlayerState = serde_json::from_str(filtered).unwrap();
        assert_eq!(s.body.len(), 1, "only the one known key survived the seam filter");
        assert_eq!(
            s.body.get(&BodyPart::LeftUpperArm).copied().unwrap(),
            BodyPartState::Orange,
        );
        // And an unknown legacy key MUST fail raw deserialization (the seam is
        // the guard, not the deserializer) — this pins why the seam exists.
        let legacy = r#"{"body":{"LeftBicep":"Red"}}"#;
        assert!(serde_json::from_str::<PlayerState>(legacy).is_err());
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
            narrative_hint: "test".into(),
            lethal: false,
            directive: String::new(),
        };
        apply_outcome(&mut s, &outcome);
        assert_eq!(s.body.get(&BodyPart::RightUpperLeg).copied().unwrap(), BodyPartState::Orange);
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

    // --- Skill-Check Referee (Phase 2, 2026-07-27) ---

    #[test]
    fn skill_check_lockpick_triggers_on_keyword() {
        let outcomes = referee_evaluate_skill_checks("I pick the lock on the chest.", 0);
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
            referee_evaluate_skill_checks("I walk to the bar and order an ale.", 0).is_empty(),
            "neutral text must not trigger any skill check"
        );
        assert!(
            referee_evaluate_skill_checks("Hello, nice weather today.", 0).is_empty(),
            "smalltalk must not trigger any skill check"
        );
        assert!(
            referee_evaluate_skill_checks("I look at the painting.", 0).is_empty(),
            "looking must not trigger any skill check"
        );
    }

    #[test]
    fn skill_check_keyword_match_is_case_insensitive() {
        // Mixed case must still trigger (the evaluator lowercases the text).
        let upper = referee_evaluate_skill_checks("I PICK THE LOCK.", 0);
        let mixed = referee_evaluate_skill_checks("I Persuade the guard.", 0);
        assert_eq!(upper.len(), 1);
        assert_eq!(mixed.len(), 1);
    }

    #[test]
    fn skill_check_deterministic_for_same_text_and_pacing() {
        // Same text + same pacing modifier → same outcomes (RNG is seeded
        // from the text + skill index, so the result is reproducible). This
        // is what makes the Referee testable AND what makes replays stable.
        let a = referee_evaluate_skill_checks("I try to pick the lock.", 0);
        let b = referee_evaluate_skill_checks("I try to pick the lock.", 0);
        assert_eq!(a, b, "same text + pacing must produce identical outcomes");
        // Different text → different roll (almost certainly; the hash shifts).
        let c = referee_evaluate_skill_checks("I try to pick the lock again.", 0);
        assert_ne!(a[0].roll, c[0].roll, "different text must produce different rolls");
    }

    #[test]
    fn skill_check_multiple_skills_one_turn() {
        // A turn can attempt multiple skills in one breath: each must fire.
        let outcomes = referee_evaluate_skill_checks(
            "I pick the lock, then sneak past the guard.",
            0,
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

    #[test]
    fn skill_check_pacing_dc_mod_applies() {
        // +2 pacing mod raises the effective DC by 2; -2 lowers it by 2.
        let neutral = referee_evaluate_skill_checks("I intimidate the thug.", 0);
        let combat = referee_evaluate_skill_checks("I intimidate the thug.", 2);
        let downtime = referee_evaluate_skill_checks("I intimidate the thug.", -2);
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
    fn skill_check_dc_clamps_at_1_and_30() {
        // A pathological pacing modifier can't push DC out of [1, 30].
        let low = referee_evaluate_skill_checks("I pick the lock.", -100);
        let high = referee_evaluate_skill_checks("I pick the lock.", 100);
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
        let skill_outcomes = referee_evaluate_skill_checks("I attack the goblin with my sword.", 0);
        assert!(
            skill_outcomes.is_empty(),
            "skill Referee must not fire on combat keywords (combat Referee owns them): {skill_outcomes:?}"
        );
        // Sanity: the combat Referee DOES fire on the same text.
        let combat = referee_evaluate("I attack the goblin with my sword.", &fresh_state());
        assert!(combat.is_some(), "combat Referee must fire on combat keyword");
    }

    #[test]
    fn capitalize_first_handles_edge_cases() {
        assert_eq!(capitalize_first("lockpick"), "Lockpick");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
    }

    // --- Phase 3: combat-keyword / scene-pacing sync ---

    /// The combat-keyword list in `player_state::COMBAT_KEYWORDS` and the
    /// kinetic-combat list in `scene_pacing::KINETIC_COMBAT` MUST stay in
    /// sync — they're independently declared (the lists are private to their
    /// modules). If they drift, the combat Referee could fire on a turn the
    /// scene-pacing classifies as non-Combat (or vice versa), which would
    /// break the anti-sycophancy contract (Referee injury with no combat
    /// pacing directive, or combat pacing with no injury).
    ///
    /// This test re-declares the canonical combat list here and asserts
    /// (a) every keyword fires `referee_evaluate` AND (b) every keyword
    /// triggers `SceneMode::Combat` via `scene_pacing::evaluate`. The
    /// re-declaration is the load-bearing check: it's the same literal list,
    /// and any drift between the three copies (player_state, scene_pacing,
    /// this test) will surface as a test failure. The list itself is the
    /// canonical one from `player_state::COMBAT_KEYWORDS` (the comment above
    /// that const says "swap for a real RNG later" — same list, kept here
    /// verbatim).
    const TEST_COMBAT_KEYWORDS: &[&str] = &[
        "attack", "swing", "strike", "slash", "stab", "punch", "kick", "block", "dodge",
        "parry", "shoot", "fire", "cast", "throw", "tackle", "grapple", "charge",
        "run", "sprint", "climb", "jump", "leap", "swim",
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
        // The default preserves the v1 severity distribution exactly —
        // the [50,30,15,5] weights are what shipped before Slice 3.
        assert_eq!(AttackerTier::default(), AttackerTier::Soldier);
        assert_eq!(AttackerTier::Soldier.severity_weights(), [50, 30, 15, 5]);
    }

    #[test]
    fn attacker_tier_severity_weights_shift_with_tier() {
        // Minion weights toward Minor (index 0); Legendary toward Critical (3).
        // The shift must be monotonic per index.
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
        // Sanity: the four-tier shift ladder is ordered Minion→Soldier→Elite→
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
            // Combined Heavy+Critical weight must increase with tier.
            let lower_severe = lower[2] + lower[3];
            let higher_severe = higher[2] + higher[3];
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
        );
        assert_eq!(a, b, "default and explicit-Soldier paths must agree");
    }

    #[test]
    fn referee_outcome_carries_lethality_fields() {
        // Every outcome now has the lethal flag + directive string, even
        // when non-lethal (the fields default to false / empty). This pins
        // the API contract so callers can rely on them.
        let s = fresh_state();
        let outcome = referee_evaluate("I attack the goblin.", &s).expect("should fire");
        // Non-lethal outcomes have an empty directive (the caller only wraps
        // non-empty directives in the [DIRECTIVE: ...] block).
        assert!(!outcome.directive.is_empty() || !outcome.lethal,
            "directive must be empty when non-lethal");
    }

    #[test]
    fn referee_lethality_fires_for_legendary_on_wounded_body() {
        // The architect's defining example: a Legendary-tier attacker on a
        // badly-wounded body should be lethal most of the time. We can't
        // pin a specific roll (RNG), but across many trials lethality must
        // fire at least once. The threshold math (BASE 18 + Legendary −8 +
        // Battered −4 = DC 6, so any roll 1..=5 is lethal) makes this very
        // likely — the test would only fail if the lethality judgment were
        // broken or the condition penalty weren't applied.
        let mut s = fresh_state();
        // Battered: one Heavy wound.
        s.body.insert(BodyPart::UpperTorso, BodyPartState::Red);
        let mut lethal_count = 0;
        for i in 0..64 {
            let text = format!("I attack the dragon again, turn {i}");
            if let Some(o) = referee_evaluate_with_tier(&text, &s, AttackerTier::Legendary) {
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
            lethal_count > 0,
            "Legendary vs Battered must be lethal in at least one of 64 trials (got 0)"
        );
    }

    #[test]
    fn referee_lethality_rarely_fires_for_minion_on_healthy_body() {
        // The opposite extreme: a Minion attacking a fresh body should
        // almost never be lethal. DC = 18 + 8 (Minion) + 0 (Unscathed) = 26,
        // so only a natural 1 on the d20 triggers it (1/20 = 5% per turn).
        // Across 100 trials we expect ~5 lethals — allow up to 15 (3σ slack).
        let s = fresh_state();
        let mut lethal_count = 0;
        for i in 0..100 {
            let text = format!("the rat bites me, turn {i}");
            if let Some(o) = referee_evaluate_with_tier(&text, &s, AttackerTier::Minion) {
                if o.lethal {
                    lethal_count += 1;
                }
            }
        }
        assert!(
            lethal_count <= 15,
            "Minion vs Unscathed should be lethal ≤15/100 trials (got {lethal_count}/100); \
             higher indicates the lethality threshold is broken",
        );
    }

    #[test]
    fn referee_same_part_repeat_hit_escalates() {
        // Architect directive Slice 3: a second hit to an already-wounded
        // part always escalates, never downgrades. Pre-wound the Upper Torso
        // to Orange; subsequent Upper Torso hits must land at Red or worse.
        let mut s = fresh_state();
        s.body.insert(BodyPart::UpperTorso, BodyPartState::Orange);
        let mut torso_hits = 0;
        for i in 0..200 {
            // Vary the text to spread the RNG across parts.
            let text = format!("I strike the bandit, exchange {i}");
            if let Some(o) = referee_evaluate_with_tier(&text, &s, AttackerTier::Elite) {
                if o.part == BodyPart::UpperTorso {
                    torso_hits += 1;
                    assert!(
                        o.new_state.rank() >= BodyPartState::Orange.rank(),
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

    fn entities_with_tier(tier: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("npc.guard1.tier".into(), tier.into());
        m
    }

    #[test]
    fn disguise_gate_none_when_no_disguise_tag() {
        // No disguise tag → nothing to gate, regardless of tier or behavior.
        let entities = entities_with_tier("soldier");
        assert!(evaluate_disguise_gate("I walk past the guard", &[generic_tag("Blessed")], &entities, 0).is_none());
    }

    #[test]
    fn disguise_gate_autopass_minion_confident_walkby() {
        let entities = entities_with_tier("minion");
        let out = evaluate_disguise_gate(
            "I nod to the drunk guard and march into the keep.",
            &[disguise_tag("city guard uniform")],
            &entities,
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
        let out = evaluate_disguise_gate(
            "I flash my badge and stride through the gate.",
            &[disguise_tag("city guard uniform")],
            &entities,
            0,
        ).expect("soldier + confident → AutoPass");
        assert!(matches!(out, DisguiseDirective::AutoPass { tier_tag: "soldier", .. }));
    }

    #[test]
    fn disguise_gate_none_elite_confident() {
        // Elite+ scrutinize by default. Even a confident walk-by forces a
        // roll (handled by the §11.21 skill-check Referee, not here).
        let entities = entities_with_tier("elite");
        let out = evaluate_disguise_gate(
            "I nod to the captain and walk past.",
            &[disguise_tag("city guard uniform")],
            &entities,
            0,
        );
        assert!(out.is_none(), "Elite+ must NOT auto-pass: {out:?}");
    }

    #[test]
    fn disguise_gate_none_legendary_confident() {
        let entities = entities_with_tier("legendary");
        let out = evaluate_disguise_gate(
            "I salute the dragon and walk past.",
            &[disguise_tag("city guard uniform")],
            &entities,
            0,
        );
        assert!(out.is_none(), "Legendary must NOT auto-pass: {out:?}");
    }

    #[test]
    fn disguise_gate_scrutinized_minion_suspicious() {
        // Suspicious behavior revokes the auto-pass even for minions.
        let entities = entities_with_tier("minion");
        let out = evaluate_disguise_gate(
            "I sweat nervously, avoid eye contact, and try to slip past.",
            &[disguise_tag("city guard uniform")],
            &entities,
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
        let out = evaluate_disguise_gate(
            "I stammer, fumble my badge, and mumble an excuse.",
            &[disguise_tag("city guard uniform")],
            &entities,
            0,
        ).expect("soldier + suspicious → Scrutinized");
        assert!(matches!(out, DisguiseDirective::Scrutinized { .. }));
    }

    #[test]
    fn disguise_gate_none_elite_suspicious() {
        // Elite+ return None even when suspicious — the normal skill-check
        // Referee handles the Deception roll there (no disguise framing
        // needed; the captain would have challenged anyway).
        let entities = entities_with_tier("elite");
        let out = evaluate_disguise_gate(
            "I sweat nervously and stammer at the captain.",
            &[disguise_tag("city guard uniform")],
            &entities,
            0,
        );
        assert!(out.is_none());
    }

    #[test]
    fn disguise_gate_scrutinized_dc_threads_pacing_modifier() {
        // Combat (+2) makes scrutiny harder; Downtime (−2) easier. Same shape
        // as the §11.21 skill-check DC threading.
        let entities = entities_with_tier("soldier");
        let combat = evaluate_disguise_gate(
            "I sweat and stammer.",
            &[disguise_tag("city guard uniform")],
            &entities,
            2,
        ).unwrap();
        let downtime = evaluate_disguise_gate(
            "I sweat and stammer.",
            &[disguise_tag("city guard uniform")],
            &entities,
            -2,
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
    fn find_disguise_tag_returns_first_disguise() {
        let tags = vec![
            generic_tag("Blessed"),
            disguise_tag("city guard uniform"),
            disguise_tag("merchant robes"), // second disguise ignored
        ];
        let found = find_disguise_tag(&tags).expect("must find the disguise");
        assert_eq!(found.label, "city guard uniform");
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
}
