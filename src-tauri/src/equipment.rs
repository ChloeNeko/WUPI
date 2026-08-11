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

/// A behavior tag on an inventory item. Drives which actions the Soul Gem
/// inspection popup offers (CONSUME / EQUIP / POCKET). The closed three-value
/// domain keeps the tracker prompt + the frontend UI in lockstep via the
/// snake_case serde form (`"consumable"` / `"equippable"` / `"pocketable"`):
/// the local model emits these in `[EQUIP/BELT/PACK ... tags=...]`, Rust parses
/// them into the enum, and the frontend reads `item.tags` as an array of these
/// strings to conditionally render actions (no client-side name heuristics).
///
/// `Equippable` is the natural tag for items the player can wear/wield;
/// `Consumable` for food/drink/potions/scrolls; `Pocketable` for small items
/// that fit a belt pouch (rings, keys, vials, coins). An item may carry several
/// (a "Healing Potion" is both Consumable + Pocketable).
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ItemTag {
    Consumable,
    Equippable,
    Pocketable,
}

impl ItemTag {
    /// Case-insensitive parse from a serde string (`"consumable"`). Returns
    /// `None` for unknown tags — the parser drops these silently (same
    /// leniency as every other bracket field).
    pub fn from_id(s: &str) -> Option<ItemTag> {
        match s.trim().to_lowercase().as_str() {
            "consumable" => Some(ItemTag::Consumable),
            "equippable" | "equipable" => Some(ItemTag::Equippable),
            "pocketable" | "pocket" => Some(ItemTag::Pocketable),
            _ => None,
        }
    }

    /// The canonical snake_case id (`"consumable"`). Mirrors the serde wire
    /// form so the parser, the applier, and the frontend share one vocabulary.
    pub fn id(self) -> &'static str {
        match self {
            ItemTag::Consumable => "consumable",
            ItemTag::Equippable => "equippable",
            ItemTag::Pocketable => "pocketable",
        }
    }
}

/// Parse a comma-separated tag string (`"consumable, pocketable"`) into a
/// deduped `Vec<ItemTag>` in canonical order. Unknown tags are dropped (no
/// error). Empty/whitespace input → empty vec. Used by the bracket text parser
/// (`tags=consumable,equippable`).
pub fn parse_tag_list(s: &str) -> Vec<ItemTag> {
    let mut out: Vec<ItemTag> = Vec::new();
    for raw in s.split(',') {
        // Strip JSON array + quote punctuation the model sometimes wraps tags
        // in (`tags=["pocketable"]` → the text path sees `["pocketable"]`).
        // Trimming `[]{}"' ` recovers the bare tag id for `from_id`.
        let cleaned = raw.trim_matches(|c| matches!(c, '[' | ']' | '"' | '\'' | ' '));
        if let Some(t) = ItemTag::from_id(cleaned) {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    out
}

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
/// (e.g. `"+2 ATK"`, `"lined with wolf fur"`) + a behavior-tag set driving the
/// Soul Gem inspection popup actions. Pure identity — no weight (worn items
/// don't count against carry capacity), no quantity (a slot holds one item per
/// layer).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct EquippedItem {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<String>,
    /// Behavior tags (`consumable`/`equippable`/`pocketable`). `#[serde
    /// (default)]` so existing saves without tags load as empty; empty vecs
    /// are skipped on serialize to keep the JSON tight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ItemTag>,
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
/// stats + a behavior-tag set. Used by both belt and pack — they differ only in
/// capacity semantics (belt is a fixed 4-slot count; pack is UNBOUNDED deep
/// storage — the encumbrance/weight system was PERMANENTLY REMOVED 2026-08-09:
/// no weight limits, no fill bar, no capacity enforcement, ever). `weight`
/// survives on the struct purely for the narrator-summary text readout (it
/// carries no live semantics + no code path enforces it). The belt is the
/// only true capacity cap (`BELT_MAX = 4`).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize, Debug)]
pub struct StackItem {
    pub name: String,
    #[serde(default = "default_qty")]
    pub qty: u32,
    #[serde(default = "default_weight")]
    pub weight: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<String>,
    /// Behavior tags (`consumable`/`equippable`/`pocketable`). `#[serde
    /// (default)]` so existing saves without tags load as empty; empty vecs
    /// are skipped on serialize to keep the JSON tight. When `stack_upsert`
    /// merges into an existing same-name entry, the union of both tag sets is
    /// kept (tags are monotonic — once known, never forgotten).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<ItemTag>,
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
            tags: Vec::new(),
        }
    }
}

impl StackItem {
    /// Total weight of this stack (qty × per-unit). RETAINED for the narrator's
    /// inventory-summary text readout only — the encumbrance system was
    /// permanently removed 2026-08-09, so this drives no UI + enforces nothing.
    pub fn total_weight(&self) -> f32 {
        self.qty as f32 * self.weight
    }
}

/// Maximum belt slots — a hard cap. The applier evicts the OLDEST entry (FIFO)
/// when a fifth is added, so the belt is always a stable 4-slot quick-access
/// rack. Exposed as a `pub const` so the frontend can render exactly this many
/// slots + tests can pin the eviction order.
pub const BELT_MAX: usize = 4;

/// Total weight of a stack list (belt or pack). Pure fold; surfaced only in the
/// narrator's inventory-summary text. The encumbrance UI that divided this by
/// a pack-capacity field was permanently removed 2026-08-09 (the field itself
/// was deleted from `PlayerState` on 2026-08-11 — old saves with a stray
/// `pack_capacity_lbs` key are ignored by serde's default unknown-field
/// tolerance).
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
        // Union the tag sets (monotonic: once a tag is known it's never
        // forgotten on a restack — a "Healing Potion" re-added with only the
        // consumable tag keeps the pocketable tag it had before).
        for t in &item.tags {
            if !existing.tags.contains(t) {
                existing.tags.push(*t);
            }
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
    entities: &mut HashMap<String, serde_json::Value>,
    equipment: &mut Equipment,
    pack: &mut Vec<StackItem>,
) {
    // Collect first to avoid mutating while iterating (borrowck). Only
    // bare-string values are legacy-convention states ("equipped", "3 in
    // pack"); a widened structured value at an `item_*`/`inv_*` key is
    // unrecognized noise — skip it (leave it in the entity map untouched).
    let legacy: Vec<(String, String)> = entities
        .keys()
        .filter(|k| k.starts_with("item_") || k.starts_with("inv_"))
        .cloned()
        // pull the matching values via a second pass (can't borrow mut + immut together)
        .into_iter()
        .filter_map(|k| {
            entities
                .get(&k)
                .and_then(|v| v.as_str().map(|s| (k, s.to_string())))
        })
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
                // by pushing it to Inner — rare for a fresh migration). Legacy
                // items carry no tags (the old entity convention had none) →
                // default to empty; a future tracker turn may tag them.
                let layers = equipment.entry(slot).or_default();
                if layers.outer.is_none() {
                    layers.outer = Some(EquippedItem { name: name.clone(), stats: None, ..Default::default() });
                } else if layers.inner.is_none() {
                    layers.inner = Some(EquippedItem { name: name.clone(), stats: None, ..Default::default() });
                } else {
                    // Both layers full → fall back to pack.
                    stack_upsert(
                        pack,
                        StackItem { name: name.clone(), qty: 1, weight: 1.0, stats: None, ..Default::default() },
                    );
                }
            } else {
                // Not marked equipped → pack it.
                let qty = parse_qty_hint(&state);
                stack_upsert(
                    pack,
                    StackItem { name: name.clone(), qty, weight: 1.0, stats: None, ..Default::default() },
                );
            }
        } else {
            // No slot routing → pack with a qty hint if present.
            let qty = parse_qty_hint(&state);
            stack_upsert(
                pack,
                StackItem { name: name.clone(), qty, weight: 1.0, stats: None, ..Default::default() },
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
        let added = stack_upsert(&mut items, StackItem { name: "Lockpick".into(), qty: 2, weight: 0.5, stats: None, ..Default::default() });
        assert!(added, "first entry is a new add");
        let added2 = stack_upsert(&mut items, StackItem { name: "lockpick".into(), qty: 3, weight: 0.5, stats: None, ..Default::default() });
        assert!(!added2, "second is a stack onto existing (case-insensitive)");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].qty, 5);
        assert_eq!(items[0].total_weight(), 2.5);
    }

    #[test]
    fn stack_upsert_takes_heavier_weight() {
        let mut items = Vec::new();
        stack_upsert(&mut items, StackItem { name: "Iron Sword".into(), qty: 1, weight: 4.0, stats: None, ..Default::default() });
        stack_upsert(&mut items, StackItem { name: "iron sword".into(), qty: 1, weight: 6.0, stats: None, ..Default::default() });
        assert_eq!(items[0].weight, 6.0, "the heavier per-unit weight wins on restack");
    }

    #[test]
    fn stack_upsert_unions_tags_on_restack() {
        let mut items = Vec::new();
        stack_upsert(&mut items, StackItem { name: "Healing Potion".into(), qty: 1, weight: 0.5, stats: None, tags: vec![ItemTag::Consumable], ..Default::default() });
        stack_upsert(&mut items, StackItem { name: "healing potion".into(), qty: 1, weight: 0.5, stats: None, tags: vec![ItemTag::Pocketable], ..Default::default() });
        assert_eq!(items.len(), 1);
        assert!(items[0].tags.contains(&ItemTag::Consumable), "consumable tag kept on restack");
        assert!(items[0].tags.contains(&ItemTag::Pocketable), "pocketable tag unioned in");
    }

    #[test]
    fn stack_remove_drops_whole_entry_at_qty_zero() {
        let mut items = vec![StackItem { name: "Arrow".into(), qty: 10, weight: 0.1, stats: None, ..Default::default() }];
        assert!(stack_remove(&mut items, "arrow", 0));
        assert!(items.is_empty(), "qty=0 removes the whole entry");
    }

    #[test]
    fn stack_remove_partial_decrements() {
        let mut items = vec![StackItem { name: "Arrow".into(), qty: 10, weight: 0.1, stats: None, ..Default::default() }];
        assert!(stack_remove(&mut items, "Arrow", 3));
        assert_eq!(items[0].qty, 7);
    }

    // ── ItemTag parsing ──────────────────────────────────────────────────

    #[test]
    fn item_tag_from_id_canonical_and_aliases() {
        assert_eq!(ItemTag::from_id("consumable"), Some(ItemTag::Consumable));
        assert_eq!(ItemTag::from_id("Equippable"), Some(ItemTag::Equippable));
        assert_eq!(ItemTag::from_id("equipable"), Some(ItemTag::Equippable), "common misspelling tolerated");
        assert_eq!(ItemTag::from_id("POCKETABLE"), Some(ItemTag::Pocketable));
        assert_eq!(ItemTag::from_id("pocket"), Some(ItemTag::Pocketable));
        assert_eq!(ItemTag::from_id("cursed"), None, "unknown tag rejected");
        assert_eq!(ItemTag::from_id(""), None);
    }

    #[test]
    fn parse_tag_list_dedupes_and_drops_unknown() {
        let tags = parse_tag_list("consumable, pocketable, consumable, junk, equippable");
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0], ItemTag::Consumable);
        assert_eq!(tags[1], ItemTag::Pocketable);
        assert_eq!(tags[2], ItemTag::Equippable);
        assert!(parse_tag_list("   ").is_empty());
    }

    #[test]
    fn item_tag_serde_is_snake_case() {
        let json = serde_json::to_string(&vec![ItemTag::Consumable, ItemTag::Pocketable]).unwrap();
        assert_eq!(json, r#"["consumable","pocketable"]"#);
        let parsed: Vec<ItemTag> = serde_json::from_str(r#"["equippable"]"#).unwrap();
        assert_eq!(parsed, vec![ItemTag::Equippable]);
    }

    // ── render_for_prompt Outer-layer filter ──────────────────────────────

    #[test]
    fn render_equipped_shows_outer_only() {
        let mut ps = PlayerState::default();
        ps.equipment.insert(
            EquipSlot::Chest,
            SlotLayers {
                outer: Some(EquippedItem { name: "Heavy Cloak".into(), stats: None, ..Default::default() }),
                inner: Some(EquippedItem { name: "Linen Shirt".into(), stats: None, ..Default::default() }),
            },
        );
        ps.equipment.insert(
            EquipSlot::MainHand,
            SlotLayers {
                outer: Some(EquippedItem { name: "Iron Sword".into(), stats: Some("+2 ATK".into()), ..Default::default() }),
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
        let mut entities: HashMap<String, serde_json::Value> = HashMap::new();
        entities.insert("item_iron_sword".into(), serde_json::Value::String("equipped".into()));
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
        let mut entities: HashMap<String, serde_json::Value> = HashMap::new();
        entities.insert(
            "inv_health_potion".into(),
            serde_json::Value::String("3 in pack".into()),
        );
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
        let mut entities: HashMap<String, serde_json::Value> = HashMap::new();
        entities.insert("item_iron_sword".into(), serde_json::Value::String("equipped".into()));
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

    #[test]
    fn migrate_legacy_items_skips_structured_values() {
        // Widening guard (2026-08-11): a structured JSON value at an item_* key
        // is unrecognized noise — leave it in the entity map untouched.
        let mut entities: HashMap<String, serde_json::Value> = HashMap::new();
        entities.insert(
            "item_strange".into(),
            serde_json::json!({ "enchantment": "unknown" }),
        );
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        migrate_legacy_items(&mut entities, &mut equipment, &mut pack);
        assert!(equipment.is_empty(), "structured value not routed");
        assert!(pack.is_empty(), "structured value not packed");
        assert!(
            entities.contains_key("item_strange"),
            "structured value left in entity map"
        );
    }
}
