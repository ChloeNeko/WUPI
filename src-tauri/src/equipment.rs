//! Inventory / equipment model — the typed inventory core (Fable §2026-08-07).
//!
//! Rust is the SOLE authority over what the player carries and wears, mirroring
//! the `player_state` discipline: the narrator LLM mutates this ONLY through
//! the bracket pipeline (`[EQUIP]`/`[BELT]`/`[PACK]`), never by writing the
//! rendered `<player_state>` block. The rendered block exposes ONLY each slot's
//! Outer-layer item to the narrator — Inner layers are invisible to the AI (the
//! "Heavy Cloak over Linen Shirt → AI only knows the Cloak" rule). Belt + pack
//! are never appearance-visible (carried, not worn).
//!
//! Lives nested inside `PlayerState` (NOT a separate AppState field), so it
//! rides `save_split` → `<card_id>.player.json` for free + round-trips through
//! `fable_json_raw_set(kind="player")` unchanged. All fields are `#[serde
//! (default)]` so existing saves load without migration.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// The six equipment slots — map 1:1 to body-part anchors on the paperdoll.
// ---------------------------------------------------------------------------

/// An equipment slot. Each maps to a body-part bbox anchor the frontend renders
/// a dashed-brass node against (see `equipment-overlay.js`):
///
/// | Slot      | Body-part anchor (frontend `getHitbox` id) |
/// |-----------|--------------------------------------------|
/// | `Head`    | `head`                                     |
/// | `Chest`   | `upper_torso`                              |
/// | `MainHand`| `right_hand`                               |
/// | `OffHand` | `left_hand`                                |
/// | `Legs`    | `lower_torso`                              |
/// | `Feet`    | midpoint of `left_foot` / `right_foot`     |
///
/// Serialization is snake_case (`"main_hand"`) so it round-trips cleanly through
/// JSON; the bracket parser lowercases + matches against the canonical form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum EquipSlot {
    Head,
    Chest,
    MainHand,
    OffHand,
    Legs,
    Feet,
}

impl EquipSlot {
    /// Canonical snake_case ids — the allowlist the `[EQUIP slot=...]` parser
    /// matches against (case-insensitive). Source of truth for both the parser
    /// + the frontend's slot→body-part mapping.
    pub fn all() -> &'static [EquipSlot] {
        &[
            EquipSlot::Head,
            EquipSlot::Chest,
            EquipSlot::MainHand,
            EquipSlot::OffHand,
            EquipSlot::Legs,
            EquipSlot::Feet,
        ]
    }

    /// Case-insensitive parse from a snake_case id (`"main_hand"`). Returns
    /// `None` for unknown ids — the parser drops these silently (same leniency
    /// as every other bracket parser).
    pub fn from_id(s: &str) -> Option<EquipSlot> {
        let lower = s.trim().to_lowercase();
        EquipSlot::all()
            .iter()
            .copied()
            .find(|slot| slot.id() == lower)
    }

    /// The canonical snake_case id (`"main_hand"`). Mirrors the serde wire form
    /// so the parser, the applier, and the frontend all share one vocabulary.
    pub fn id(self) -> &'static str {
        match self {
            EquipSlot::Head => "head",
            EquipSlot::Chest => "chest",
            EquipSlot::MainHand => "main_hand",
            EquipSlot::OffHand => "off_hand",
            EquipSlot::Legs => "legs",
            EquipSlot::Feet => "feet",
        }
    }

    /// Human label for the narrator-rendered block + tooltips (`"Main Hand"`).
    pub fn label(self) -> &'static str {
        match self {
            EquipSlot::Head => "Head",
            EquipSlot::Chest => "Chest",
            EquipSlot::MainHand => "Main Hand",
            EquipSlot::OffHand => "Off Hand",
            EquipSlot::Legs => "Legs",
            EquipSlot::Feet => "Feet",
        }
    }
}

// ---------------------------------------------------------------------------
// The layering model — Outer is narrator-visible, Inner is hidden.
// ---------------------------------------------------------------------------

/// Which layer an equipped item sits in. The Outer layer is what an observer
/// sees; the Inner layer is concealed beneath it. The narrator's
/// `render_for_prompt` exposes ONLY Outer — so a `Heavy Cloak` (Outer) over a
/// `Linen Shirt` (Inner) reads to the AI as just the cloak.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ItemLayer {
    Outer,
    Inner,
}

impl Default for ItemLayer {
    fn default() -> Self {
        ItemLayer::Outer
    }
}

/// A single equipped item: a name + optional flavor stats for the tooltip
/// (e.g. `"+2 ATK"`, `"lined with wolf fur"`). Pure identity — no weight
/// (worn items don't count against carry capacity), no quantity (a slot holds
/// one item per layer).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct EquippedItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<String>,
}

/// The two-layer stack a single slot holds. A slot may carry an Outer item, an
/// Inner item, both (cloak over shirt), or neither (empty slot → omitted from
/// the equipment map entirely). `Option::is_none` skip-serializes absent layers
/// so an empty slot stays out of the JSON.
#[derive(Clone, PartialEq, Default, serde::Serialize, serde::Deserialize, Debug)]
pub struct SlotLayers {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outer: Option<EquippedItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inner: Option<EquippedItem>,
}

impl SlotLayers {
    /// True when neither layer holds an item — the slot is empty. The equipment
    /// map never stores empty slots (the applier removes them on unequip), so
    /// this is the invariant check on every mutation path.
    pub fn is_empty(&self) -> bool {
        self.outer.is_none() && self.inner.is_none()
    }

    /// The Outer-layer item, if any — the single narrator-visible item for this
    /// slot. Inner is deliberately hidden by the renderer.
    pub fn visible(&self) -> Option<&EquippedItem> {
        self.outer.as_ref()
    }
}

/// The equipment map: only slots that hold at least one item are keyed. Empty
/// slots are absent (not `SlotLayers::default()`) so iteration is cheap and the
/// serialized form stays tight.
pub type Equipment = HashMap<EquipSlot, SlotLayers>;

// ---------------------------------------------------------------------------
// Stackable items — belt (quick-access) + pack (deep storage).
// ---------------------------------------------------------------------------

/// A stackable item entry: name + quantity + per-unit weight (lbs) + optional
/// stats. Used by both belt and pack — they differ only in capacity semantics
/// (belt is a fixed 4-slot count; pack is weight-bounded for encumbrance).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize, Debug)]
pub struct StackItem {
    pub name: String,
    #[serde(default = "default_qty")]
    pub qty: u32,
    #[serde(default = "default_weight")]
    pub weight: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<String>,
}

fn default_qty() -> u32 {
    1
}
fn default_weight() -> f32 {
    1.0
}

impl Default for StackItem {
    fn default() -> Self {
        StackItem {
            name: String::new(),
            qty: default_qty(),
            weight: default_weight(),
            stats: None,
        }
    }
}

impl StackItem {
    /// Total weight of this stack (qty × per-unit). Drives the pack
    /// encumbrance bar.
    pub fn total_weight(&self) -> f32 {
        self.qty as f32 * self.weight
    }
}

/// Maximum belt slots — a hard cap. The applier evicts the OLDEST entry (FIFO)
/// when a fifth is added, so the belt is always a stable 4-slot quick-access
/// rack. Exposed as a `pub const` so the frontend can render exactly this many
/// slots + tests can pin the eviction order.
pub const BELT_MAX: usize = 4;

/// Default pack carry capacity in pounds. Per-card overridable via
/// `PlayerState.pack_capacity_lbs`; this is the seed for a fresh game.
pub const PACK_DEFAULT_CAPACITY_LBS: f32 = 20.0;

/// Total weight of a stack list (belt or pack). Pure fold; the pack
/// encumbrance UI divides this by `pack_capacity_lbs` for the fill bar.
pub fn stack_weight(items: &[StackItem]) -> f32 {
    items.iter().map(|i| i.total_weight()).sum()
}

/// Upsert a stack item into a list by name: if an entry with the same
/// (case-insensitive) name exists, add `qty` to it (taking the max of the two
/// per-unit weights so a heavier restack wins); otherwise push a new entry.
/// Returns true if a new entry was added (for the belt's FIFO eviction check).
pub fn stack_upsert(items: &mut Vec<StackItem>, item: StackItem) -> bool {
    let key = item.name.to_lowercase();
    if let Some(existing) = items.iter_mut().find(|i| i.name.to_lowercase() == key) {
        existing.qty = existing.qty.saturating_add(item.qty);
        if item.weight > existing.weight {
            existing.weight = item.weight;
        }
        if existing.stats.is_none() {
            existing.stats = item.stats;
        }
        false
    } else {
        items.push(item);
        true
    }
}

/// Remove up to `qty` of a named item from a stack list. If `qty` covers the
/// whole stack (or the caller passes the removal form), the entry is dropped.
/// Returns true if anything was removed. qty=0 means "remove the whole entry"
/// (the `[BELT -name]` / `[PACK -name]` form).
pub fn stack_remove(items: &mut Vec<StackItem>, name: &str, qty: u32) -> bool {
    let key = name.trim().to_lowercase();
    for idx in 0..items.len() {
        if items[idx].name.to_lowercase() == key {
            if qty == 0 || items[idx].qty <= qty {
                items.remove(idx);
            } else {
                items[idx].qty -= qty;
            }
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Legacy migration: item_*/inv_* entity keys → typed inventory (one-shot).
// ---------------------------------------------------------------------------

/// Keyword→slot routing for the legacy `item_*`/`inv_*` entity migration. Pure
/// heuristic on the lowercased name: a sword/axe/mace routes to MainHand, a
/// shield to OffHand, etc. Anything that doesn't match returns `None` → the
/// item lands in the pack instead. Mirrors the (deleted) `panels/inventory.js`
/// glyph-picker heuristic, adapted to slot routing.
fn route_legacy_to_slot(name_lower: &str) -> Option<EquipSlot> {
    // Cheap substring scan (no regex). Order matters: the first match wins,
    // mirroring the deleted panel's glyph-picker heuristic.
    if contains_any(
        name_lower,
        &["sword", "blade", "axe", "dagger", "spear", "mace", "warhammer", "flail", "staff", "wand"],
    ) {
        return Some(EquipSlot::MainHand);
    }
    if contains_any(name_lower, &["shield", "buckler", "targe"]) {
        return Some(EquipSlot::OffHand);
    }
    if contains_any(name_lower, &["helm", "helmet", "hat", "hood", "cap", "crown"]) {
        return Some(EquipSlot::Head);
    }
    if contains_any(
        name_lower,
        &["armor", "armour", "chestplate", "breastplate", "cuirass", "vest"],
    ) {
        return Some(EquipSlot::Chest);
    }
    if contains_any(name_lower, &["legging", "pants", "trouser", "greave", "skirt"]) {
        return Some(EquipSlot::Legs);
    }
    if contains_any(name_lower, &["boot", "sabaton", "shoe", "sandal"]) {
        return Some(EquipSlot::Feet);
    }
    None
}

/// True if `hay` contains any of the `needles` (case-insensitive on `hay`,
/// which callers pre-lowercase; needles are authored lowercase).
fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

/// Migrate legacy `item_*`/`inv_*` entity keys into the typed inventory model.
/// Called once from `WorldSchema::load_split` after the body-key filter. For
/// each legacy item key: strips the prefix, title-cases the name, routes to a
/// slot by keyword (else pack), reads the entity's state string as a hint
/// (`"equipped"` → slot Outer layer, `"N in pack"` → pack with qty N), then
/// removes the entity key. Idempotent: a second run finds no legacy keys →
/// no-op. Mutates `entities`, `equipment`, `belt`, and `pack` in place.
///
/// This is the single chokepoint that retires the old freeform-item convention
/// (the deleted `panels/inventory.js` read-view) — once migrated, the typed
/// model is the sole source of truth.
pub fn migrate_legacy_items(
    entities: &mut HashMap<String, String>,
    equipment: &mut Equipment,
    pack: &mut Vec<StackItem>,
) {
    // Collect first to avoid mutating while iterating (borrowck).
    let legacy: Vec<(String, String)> = entities
        .keys()
        .filter(|k| k.starts_with("item_") || k.starts_with("inv_"))
        .cloned()
        // pull the matching values via a second pass (can't borrow mut + immut together)
        .into_iter()
        .filter_map(|k| entities.get(&k).map(|v| (k, v.clone())))
        .collect();

    for (raw_key, state_raw) in legacy {
        // Strip the prefix → the item slug. Title-case it into a display name.
        let slug = raw_key
            .strip_prefix("item_")
            .or_else(|| raw_key.strip_prefix("inv_"))
            .unwrap_or(&raw_key);
        let name = prettify(slug);
        let state = state_raw.trim().to_lowercase();

        // State hint: "equipped" → slot Outer; "N in pack" / "N" → pack qty N.
        // The panel convention was freeform, so we read defensively.
        let is_equipped = state == "equipped" || state == "worn" || state == "held";

        if let Some(slot) = route_legacy_to_slot(&name.to_lowercase()) {
            if is_equipped {
                // Route to the slot's Outer layer (preserving any existing item
                // by pushing it to Inner — rare for a fresh migration).
                let layers = equipment.entry(slot).or_default();
                if layers.outer.is_none() {
                    layers.outer = Some(EquippedItem { name: name.clone(), stats: None });
                } else if layers.inner.is_none() {
                    layers.inner = Some(EquippedItem { name: name.clone(), stats: None });
                } else {
                    // Both layers full → fall back to pack.
                    stack_upsert(
                        pack,
                        StackItem { name: name.clone(), qty: 1, weight: 1.0, stats: None },
                    );
                }
            } else {
                // Not marked equipped → pack it.
                let qty = parse_qty_hint(&state);
                stack_upsert(
                    pack,
                    StackItem { name: name.clone(), qty, weight: 1.0, stats: None },
                );
            }
        } else {
            // No slot routing → pack with a qty hint if present.
            let qty = parse_qty_hint(&state);
            stack_upsert(
                pack,
                StackItem { name: name.clone(), qty, weight: 1.0, stats: None },
            );
        }

        // Remove the legacy key either way — it now lives in the typed model.
        entities.remove(&raw_key);
    }
}

/// Parse a quantity hint from a legacy entity state string. Recognizes
/// `"3 in pack"`, `"3"`, `"x3"`, `"qty: 3"`; falls back to 1.
fn parse_qty_hint(state: &str) -> u32 {
    // Pull the first run of digits in the string.
    let digits: String = state.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        // Try "in pack" / "x N" shapes by scanning for any digit run.
        for token in state.split_whitespace() {
            let t: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
            if !t.is_empty() {
                if let Ok(n) = t.parse::<u32>() {
                    return n.max(1);
                }
            }
        }
        return 1;
    }
    digits.parse::<u32>().unwrap_or(1).max(1)
}

/// Title-case a slug: `"iron_sword"` → `"Iron Sword"`, `"health_potion"` →
/// `"Health Potion"`. Mirrors the deleted `panels/inventory.js::prettify`.
fn prettify(slug: &str) -> String {
    slug.split('_')
        .map(|seg| {
            let mut c = seg.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player_state::PlayerState;

    // ── EquipSlot parsing ────────────────────────────────────────────────

    #[test]
    fn equip_slot_from_id_canonical() {
        assert_eq!(EquipSlot::from_id("main_hand"), Some(EquipSlot::MainHand));
        assert_eq!(EquipSlot::from_id("off_hand"), Some(EquipSlot::OffHand));
        assert_eq!(EquipSlot::from_id("head"), Some(EquipSlot::Head));
        assert_eq!(EquipSlot::from_id("chest"), Some(EquipSlot::Chest));
        assert_eq!(EquipSlot::from_id("legs"), Some(EquipSlot::Legs));
        assert_eq!(EquipSlot::from_id("feet"), Some(EquipSlot::Feet));
    }

    #[test]
    fn equip_slot_from_id_case_insensitive_and_rejects_unknown() {
        assert_eq!(EquipSlot::from_id("MAIN_HAND"), Some(EquipSlot::MainHand));
        assert_eq!(EquipSlot::from_id("  Main_Hand  "), Some(EquipSlot::MainHand));
        assert_eq!(EquipSlot::from_id("weapon"), None, "unknown slot must reject");
        assert_eq!(EquipSlot::from_id(""), None);
    }

    // ── Stack helpers ────────────────────────────────────────────────────

    #[test]
    fn stack_upsert_adds_then_stacks_by_name() {
        let mut items = Vec::new();
        let added = stack_upsert(&mut items, StackItem { name: "Lockpick".into(), qty: 2, weight: 0.5, stats: None });
        assert!(added, "first entry is a new add");
        let added2 = stack_upsert(&mut items, StackItem { name: "lockpick".into(), qty: 3, weight: 0.5, stats: None });
        assert!(!added2, "second is a stack onto existing (case-insensitive)");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].qty, 5);
        assert_eq!(items[0].total_weight(), 2.5);
    }

    #[test]
    fn stack_upsert_takes_heavier_weight() {
        let mut items = Vec::new();
        stack_upsert(&mut items, StackItem { name: "Iron Sword".into(), qty: 1, weight: 4.0, stats: None });
        stack_upsert(&mut items, StackItem { name: "iron sword".into(), qty: 1, weight: 6.0, stats: None });
        assert_eq!(items[0].weight, 6.0, "the heavier per-unit weight wins on restack");
    }

    #[test]
    fn stack_remove_drops_whole_entry_at_qty_zero() {
        let mut items = vec![StackItem { name: "Arrow".into(), qty: 10, weight: 0.1, stats: None }];
        assert!(stack_remove(&mut items, "arrow", 0));
        assert!(items.is_empty(), "qty=0 removes the whole entry");
    }

    #[test]
    fn stack_remove_partial_decrements() {
        let mut items = vec![StackItem { name: "Arrow".into(), qty: 10, weight: 0.1, stats: None }];
        assert!(stack_remove(&mut items, "Arrow", 3));
        assert_eq!(items[0].qty, 7);
    }

    // ── render_for_prompt Outer-layer filter ──────────────────────────────

    #[test]
    fn render_equipped_shows_outer_only() {
        let mut ps = PlayerState::default();
        ps.equipment.insert(
            EquipSlot::Chest,
            SlotLayers {
                outer: Some(EquippedItem { name: "Heavy Cloak".into(), stats: None }),
                inner: Some(EquippedItem { name: "Linen Shirt".into(), stats: None }),
            },
        );
        ps.equipment.insert(
            EquipSlot::MainHand,
            SlotLayers {
                outer: Some(EquippedItem { name: "Iron Sword".into(), stats: Some("+2 ATK".into()) }),
                inner: None,
            },
        );
        let rendered = ps.render_for_prompt().expect("non-default state renders");
        assert!(rendered.contains("equipped:"), "equipped block emitted");
        assert!(rendered.contains("Chest: Heavy Cloak"), "outer layer shown");
        assert!(rendered.contains("Main Hand: Iron Sword (+2 ATK)"), "stats in parens");
        assert!(!rendered.contains("Linen Shirt"), "inner layer is HIDDEN from narrator");
    }

    #[test]
    fn render_equipped_omitted_when_empty() {
        // Only stamina differs from default → no equipped block at all.
        let mut ps = PlayerState::default();
        ps.stamina = crate::player_state::Stamina::Winded;
        let rendered = ps.render_for_prompt().expect("non-default renders");
        assert!(!rendered.contains("equipped:"), "empty equipment → no equipped block");
    }

    // ── Migration ────────────────────────────────────────────────────────

    #[test]
    fn migrate_legacy_items_routes_weapon_to_main_hand() {
        let mut entities: HashMap<String, String> = HashMap::new();
        entities.insert("item_iron_sword".into(), "equipped".into());
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        migrate_legacy_items(&mut entities, &mut equipment, &mut pack);
        assert!(entities.is_empty(), "legacy key removed");
        let layers = equipment.get(&EquipSlot::MainHand).expect("sword routed to main_hand");
        assert_eq!(layers.outer.as_ref().unwrap().name, "Iron Sword");
        assert!(pack.is_empty(), "equipped weapon does not also go to pack");
    }

    #[test]
    fn migrate_legacy_items_routes_unknown_to_pack() {
        let mut entities: HashMap<String, String> = HashMap::new();
        entities.insert("inv_health_potion".into(), "3 in pack".into());
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        migrate_legacy_items(&mut entities, &mut equipment, &mut pack);
        assert!(equipment.is_empty(), "potion has no slot → no equipment");
        assert_eq!(pack.len(), 1);
        assert_eq!(pack[0].name, "Health Potion");
        assert_eq!(pack[0].qty, 3, "qty hint parsed from '3 in pack'");
    }

    #[test]
    fn migrate_legacy_items_is_idempotent() {
        let mut entities: HashMap<String, String> = HashMap::new();
        entities.insert("item_iron_sword".into(), "equipped".into());
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        migrate_legacy_items(&mut entities, &mut equipment, &mut pack);
        // Second run: no legacy keys left.
        let equipment_before = equipment.clone();
        let pack_before = pack.clone();
        migrate_legacy_items(&mut entities, &mut equipment, &mut pack);
        assert_eq!(equipment, equipment_before, "idempotent — no change on second run");
        assert_eq!(pack, pack_before);
    }
}
