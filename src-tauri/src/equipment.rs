//! Inventory / equipment model — the typed inventory core (Fable §2026-08-07).
//!
//! Rust is the SOLE authority over what the player carries and wears, mirroring
//! the `player_state` discipline: the narrator LLM mutates this ONLY through
//! the bracket pipeline (`[EQUIP]`/`[BELT]`/`[PACK]`), never by writing the
//! rendered `<player_state>` block. The rendered block exposes what an
//! OBSERVER sees (2026-08-19 NPC-perception upgrade): each slot's Outer-layer
//! item, plus an Inner item only where it physically peeks — socks under boots
//! show when the legs are bare or short-hemmed, stay hidden under trousers or
//! a full-length gown (the "Heavy Cloak over Linen Shirt → AI only knows the
//! Cloak; shorts + shoes → the socks show" rule). Belt + pack are never
//! appearance-visible (carried, not worn).
//!
//! Lives nested inside `PlayerState` (NOT a separate AppState field), so it
//! rides `save_split` → `<card_id>.player.json` for free + round-trips through
//! `fable_json_raw_set(kind="player")` unchanged. All fields are `#[serde
//! (default)]` so existing saves load without migration.

use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------------
// The six equipment slots — map 1:1 to body-part anchors on the paperdoll.
// ---------------------------------------------------------------------------

/// An equipment slot. Each maps to a Soul Gem inspection-panel category
/// (see `inventory-panel.js` `CATEGORY_MAP` — Head/TOP/HAND/BOTTOM/FEET gems;
/// the hands share one gem, the BOTTOM gem also carries the belt):
///
/// | Slot      | Panel category (`CATEGORY_MAP` key)          |
/// |-----------|----------------------------------------------|
/// | `Head`    | `head` (shares the gem with `Neck` jewelry)  |
/// | `Neck`    | `head` gem (2026-08-19 zone sweep)           |
/// | `Chest`   | `chest` (TOP gem)                            |
/// | `Arms`    | `hand` gem (2026-08-19 zone sweep)           |
/// | `Hands`   | `hand` gem (2026-08-19 zone sweep)           |
/// | `MainHand`| `hand` (shared with OffHand)                 |
/// | `OffHand` | `hand` (shared with MainHand)                |
/// | `Legs`    | `leg` (BOTTOM gem, shared with the belt)     |
/// | `Feet`    | `foot`                                       |
///
/// Serialization is snake_case (`"main_hand"`) so it round-trips cleanly through
/// JSON; the bracket parser lowercases + matches against the canonical form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum EquipSlot {
    Head,
    Neck,
    Chest,
    Arms,
    Hands,
    MainHand,
    OffHand,
    Legs,
    Feet,
}

impl EquipSlot {
    /// Canonical snake_case ids — the allowlist the `[EQUIP slot=...]` parser
    /// matches against (case-insensitive). Source of truth for both the parser
    /// + the frontend's slot→body-part mapping. Order is head-to-foot — the
    /// render order of the `equipped:` block.
    pub fn all() -> &'static [EquipSlot] {
        &[
            EquipSlot::Head,
            EquipSlot::Neck,
            EquipSlot::Chest,
            EquipSlot::Arms,
            EquipSlot::Hands,
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
            EquipSlot::Neck => "neck",
            EquipSlot::Chest => "chest",
            EquipSlot::Arms => "arms",
            EquipSlot::Hands => "hands",
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
            EquipSlot::Neck => "Neck",
            EquipSlot::Chest => "Chest",
            EquipSlot::Arms => "Arms",
            EquipSlot::Hands => "Hands",
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

    /// The Outer-layer item, if any. This is the slot's TOPMOST item — the
    /// default observer-visible one (the renderer adds an Inner only where it
    /// physically peeks, e.g. socks under boots with bare/short-covered legs:
    /// see `visible_equipment_lines`). Not a complete visibility answer on
    /// its own — cross-slot facts decide the peek.
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
/// (case-insensitive, whitespace-trimmed) name exists, add `qty` to it (taking
/// the max of the two per-unit weights so a heavier restack wins); otherwise
/// push a new entry. Returns true if a new entry was added (for the belt's
/// FIFO eviction check).
///
/// (2026-08-17 E4B shakedown P1a) The comparison normalizes BOTH sides — the
/// parse-time `clean_item_name` gate already strips mangled quoting upstream,
/// and the trim+lowercase match here is the merge-on-add backstop for entries
/// minted by other paths (the Soul Gem UI, legacy saves): a re-acquisition of
/// an existing item must stack, never append a twin the panel renders twice.
pub fn stack_upsert(items: &mut Vec<StackItem>, item: StackItem) -> bool {
    let key = item.name.trim().to_lowercase();
    if let Some(existing) = items
        .iter_mut()
        .find(|i| i.name.trim().to_lowercase() == key)
    {
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

/// (2026-08-22 playtest) Restate a stack item: the tracker's BARE form
/// (`[PACK name="Field Points" (qty=13)]`, no `+`) is a full-list
/// RESTATEMENT read back off the world-state's `pack: … ×12` line — the
/// model is asserting "the stack is now N", not "add N more". On an
/// EXISTING name with `replace_qty` this REPLACES the qty (taking the max
/// per-unit weight, filling absent stats, unioning tags — the same
/// monotonic merges as [`stack_upsert`]); with `replace_qty == false` (a
/// bare emission that carried NO qty token — the parser's defaulted 1 is
/// "no count given", not "the stack is now 1") the stored count is KEPT and
/// only the stat/weight/tag merges run. On a name the list doesn't know it
/// pushes a new entry (a first sighting is an acquisition regardless of
/// form). Without this, every restatement STACKED (`stack_upsert` sums),
/// and the read-back loop multiplied inventories ("Field Points ×12" →
/// ×25 → ×38 — the playtest's "it doubled my Dagger and quadrupled some
/// other items"); conversely a qty-less restatement with unconditional
/// replace collapsed a `Rope ×50` stack to ×1 (2026-08-22 review). Returns
/// true if the list changed.
pub fn stack_restate(items: &mut Vec<StackItem>, item: StackItem, replace_qty: bool) -> bool {
    let key = item.name.trim().to_lowercase();
    if let Some(existing) = items
        .iter_mut()
        .find(|i| i.name.trim().to_lowercase() == key)
    {
        let mut changed = replace_qty && existing.qty != item.qty;
        if replace_qty {
            existing.qty = item.qty;
        }
        if item.weight > existing.weight {
            existing.weight = item.weight;
            changed = true;
        }
        if existing.stats.is_none() && item.stats.is_some() {
            existing.stats = item.stats;
            changed = true;
        }
        for t in &item.tags {
            if !existing.tags.contains(t) {
                existing.tags.push(*t);
                changed = true;
            }
        }
        changed
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
        if items[idx].name.trim().to_lowercase() == key {
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
// (2026-08-17 E4B follow-up) Narrative phrase resolution for item fragments.
// ---------------------------------------------------------------------------

/// Tokens that terminate noun-phrase absorption. Deliberately broad: a false
/// stop keeps the fragment as-emitted (harmless), while absorbing a stopword
/// bakes "dark and" / "tin of" into a stored item name.
const PHRASE_STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "nor", "so", "yet", "of", "in", "on",
    "at", "to", "for", "with", "from", "by", "into", "onto", "over", "under",
    "up", "down", "out", "off", "near", "past", "than", "then", "as", "if",
    "while", "when", "before", "after", "is", "are", "was", "were", "be",
    "been", "being", "am", "do", "does", "did", "have", "has", "had", "i",
    "you", "he", "she", "it", "we", "they", "me", "him", "her", "us", "them",
    "my", "your", "his", "its", "our", "their", "this", "that", "these",
    "those", "here", "there", "all", "any", "both", "each", "few", "more",
    "most", "other", "some", "such", "no", "not", "only", "own", "same", "too",
    "very", "just", "like", "one",
    // (2026-08-21) Preposition gap — "he grabbed a cookie across from me"
    // absorbed "across" into the stored name ("cookie across"). The list
    // had "past"/"from" but none of these; all pure function words, never
    // name material.
    "across", "toward", "towards", "against", "around", "behind", "beside",
    "besides", "between", "beyond", "through", "throughout", "upon", "along",
    "among", "amid", "within", "without", "inside", "outside", "during",
    "despite", "except", "via", "per", "about",
];

/// How many narrative words may follow the fragment into the resolved name
/// ("hard" → "hard cheese" is 1; "rough" → "rough mooring rope" is 2). Caps
/// runaway chains before a stopword happens to appear.
const MAX_ABSORBED_WORDS: usize = 3;

fn phrase_words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn name_contains_word(name: &str, word_lower: &str) -> bool {
    phrase_words(name).iter().any(|w| w == word_lower)
}

/// Strip edge punctuation (incl. typographic quotes/dashes) from a narrative
/// token so "cheese," / "ale." / "“cold" absorb as clean words.
fn strip_token_edges(tok: &str) -> &str {
    tok.trim_matches(|c: char| {
        c.is_whitespace() || c.is_ascii_punctuation() || "\u{2018}\u{2019}\u{201c}\u{201d}\u{2014}\u{2013}\u{2026}".contains(c)
    })
}

/// Expand a ONE-WORD tracker item fragment ("dark", "tin", "rough") into the
/// full name the story actually used. The E4B tracker routinely grabs the
/// highest-attention modifier as shorthand for a multi-word purchase
/// (observed 2026-08-17: "a wedge of hard cheese" → `[PACK hard]`), which
/// lands as janky Soul-Gem entries and misses later merges/removals. This is
/// the mechanical fix per the Prime Mandate — no prompt tokens spent.
///
/// Resolution order (first hit wins, deterministic):
/// 1. **Narrative noun phrase** — scan `narrative` (the tracker's own window:
///    the player's action, then preceding narrator beats) for the fragment;
///    absorb up to [`MAX_ABSORBED_WORDS`] following name-y words up to a
///    stopword → "dark" + "…cold dark ale and a block…" → "dark ale".
///    - If the phrase matches an existing stored name (equal words, or the
///      stored name is a superset), return the STORED spelling so the add
///      merges into that stack.
///    - Else the phrase IS the item's full name (a new item).
/// 2. **Single unambiguous inventory match** — exactly one existing name
///    contains the fragment as a whole word → that stored name (re-links
///    shorthand to the stack it clearly means). Ambiguous (2+ candidates)
///    keeps the fragment — a wrong merge is worse than a terse name.
/// 3. `None` — keep the fragment verbatim.
///
/// Multi-word fragments are never resolved (`None`): they are specific
/// enough, and rewriting them risks fighting a correct emission. Pure fn.
pub fn resolve_item_fragment(
    fragment: &str,
    narrative: &[&str],
    existing_names: &[String],
) -> Option<String> {
    let frag_raw = fragment.trim();
    if frag_raw.is_empty() || frag_raw.chars().any(char::is_whitespace) {
        return None; // only one-word fragments (the observed shorthand)
    }
    let frag_lower = frag_raw.to_lowercase();
    if PHRASE_STOPWORDS.contains(&frag_lower.as_str()) {
        return None; // "and"/"the" is not an item
    }

    // ── Step 1: narrative noun-phrase expansion ──────────────────────────
    for text in narrative {
        for (tok_raw, nexts) in noun_phrase_windows(text) {
            if tok_raw.to_lowercase() != frag_lower {
                continue;
            }
            if nexts.is_empty() {
                continue; // fragment IS the head noun here; nothing to absorb
            }
            let words: Vec<String> = std::iter::once(tok_raw.clone())
                .chain(nexts.iter().cloned())
                .collect();
            let candidate: String = words
                .join(" ")
                .chars()
                .take(crate::bracket_parser::INV_NAME_MAX)
                .collect();
            let cand_words = phrase_words(&candidate);
            // Prefer the STORED spelling when the phrase is (or narrows to)
            // an existing stack — keeps merges exact + panel-consistent.
            for stored in existing_names {
                let stored_words = phrase_words(stored);
                let equal = stored_words == cand_words;
                let superset = stored_words.len() > cand_words.len()
                    && cand_words.iter().all(|w| stored_words.contains(w));
                if equal || superset {
                    return Some(stored.clone());
                }
            }
            return Some(candidate); // a genuinely new, fully-named item
        }
    }

    // ── Step 2: single unambiguous inventory match ───────────────────────
    let mut hits: Vec<&String> = existing_names
        .iter()
        .filter(|n| name_contains_word(n.as_str(), &frag_lower))
        .collect();
    hits.dedup_by(|a, b| a.trim().eq_ignore_ascii_case(b.trim()));
    if hits.len() == 1 {
        return Some(hits[0].clone());
    }

    None
}

/// Tokenize `text` into (token, up-to-`MAX_ABSORBED_WORDS` absorbable
/// following words) pairs. Tokens keep their narrative casing; absorbed
/// words are edge-stripped and stop at the first stopword / non-word token.
fn noun_phrase_windows(text: &str) -> Vec<(String, Vec<String>)> {
    let raw_toks: Vec<&str> = text.split_whitespace().collect();
    let mut out = Vec::with_capacity(raw_toks.len());
    for (i, tok) in raw_toks.iter().enumerate() {
        let tok_raw = strip_token_edges(tok).to_string();
        if tok_raw.is_empty() {
            continue;
        }
        let mut nexts: Vec<String> = Vec::new();
        for n in raw_toks.get(i + 1..).unwrap_or(&[]) {
            if nexts.len() >= MAX_ABSORBED_WORDS {
                break;
            }
            let clean = strip_token_edges(n);
            if clean.is_empty() || !clean.chars().next().is_some_and(char::is_alphanumeric) {
                break; // punctuation wall / ellipsis → stop
            }
            let lower = clean.to_lowercase();
            if PHRASE_STOPWORDS.contains(&lower.as_str()) {
                break;
            }
            let bare = clean.strip_suffix("'s").unwrap_or(clean);
            if bare.chars().count() < 2 {
                break;
            }
            nexts.push(bare.to_string());
        }
        out.push((tok_raw, nexts));
    }
    out
}

// ---------------------------------------------------------------------------
// Legacy migration: item_*/inv_* entity keys → typed inventory (one-shot).
// ---------------------------------------------------------------------------

/// Keyword→slot routing for the legacy `item_*`/`inv_*` entity migration, the
/// Player Creator's clothing chips (2026-08-18 clothing-as-inventory ruling —
/// one router, one vocabulary), AND the live garment auto-wear paths
/// (2026-08-19: the `[PACK]` applier + the Soul Gem AUTO-FIT — clothes must
/// land on the body, not in the bag). Pure heuristic on the lowercased name:
/// a sword/axe/mace routes to MainHand, a shield to OffHand, etc. Anything
/// that doesn't match returns `None` → the item lands in the pack instead.
/// Mirrors the (deleted) `panels/inventory.js` glyph-picker heuristic,
/// adapted to slot routing. `pub` because lib.rs consults it directly (the
/// `[PACK]` auto-wear gate + the equippable-tag ensure).
pub fn route_legacy_to_slot(name_lower: &str) -> Option<EquipSlot> {
    // Underwear claims FIRST (2026-08-19): the deepest layer's vocabulary is
    // word-boundary matched and shares no words with the weapon/garment
    // needles, so precedence here is about clarity, not collision-avoidance
    // (a "Lace Panties" chip must never fall through to the pack).
    if let Some(slot) = underwear_slot(name_lower) {
        return Some(slot);
    }
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
    // Chest runs BEFORE Head (2026-08-18): "cape" contains the Head needle
    // "cap", and a "hooded cloak" is a garment — body coverage outranks the
    // head mention in a compound name.
    if contains_any(
        name_lower,
        &[
            // Armor family.
            "armor", "armour", "chestplate", "breastplate", "cuirass", "vest",
            // (2026-08-19 zone sweep) the mail family — bare "mail" is a
            // word-route (below); these compounds are substring-safe.
            "chainmail", "chain mail", "hauberk",
            // Everyday garments (2026-08-18): clothing is inventory — a
            // cloak/tunic/dress/robe routes like any other worn thing. One-
            // piece garments (dress/robe/gown) anchor Chest: they dominate
            // the torso read + layer under a cloak exactly like the
            // "Heavy Cloak over Linen Shirt" example.
            // (2026-08-19 vocabulary expansion) coat/jacket/frock/poncho/
            // cardigan/sweater/garb/attire/outfit/shawl/chemise — the
            // Creator's GLM-authored chip lists routinely used these +
            // hit NO needle, dumping the whole wardrobe into the pack.
            // (2026-08-20) "crop top" joins here; bare "tee" is a WORD-route
            // below (a "Canteen" substring must never equip to Chest).
            "cloak", "cape", "mantle", "tunic", "shirt", "blouse", "dress", "robe",
            "gown", "bodice", "surcoat", "doublet", "jerkin", "tabard", "corset",
            "coat", "jacket", "frock", "poncho", "cardigan", "sweater", "garb",
            "attire", "outfit", "shawl", "chemise", "crop top",
        ],
    ) {
        return Some(EquipSlot::Chest);
    }
    // (2026-08-19 zone sweep) "earring" MUST run before the Hands word-route
    // "ring" could see it — substring here wins over the word table below.
    if contains_any(name_lower, &["helm", "helmet", "hat", "hood", "cap", "crown", "circlet", "bonnet", "bandana", "earring"]) {
        return Some(EquipSlot::Head);
    }
    // (2026-08-19 zone sweep) The neck: specific jewelry + neckwear. A neck
    // brace is neckwear; a plain "neck" word would be too generic (a "Neck
    // Key"?), so the phrase carries it.
    if contains_any(
        name_lower,
        &["necklace", "amulet", "pendant", "locket", "gorget", "choker", "torc", "torque", "scarf", "brooch", "cravat", "neck brace"],
    ) {
        return Some(EquipSlot::Neck);
    }
    // (2026-08-19 zone sweep) The arms: sleeves, bracers, elbow/shoulder
    // armor. "sleeve" is a WORD-route (below) — a substring would catch
    // "Sleeveless Gown" (which Chest already claimed above anyway, but the
    // word form is the correct discipline).
    if contains_any(
        name_lower,
        &["bracer", "vambrace", "armlet", "armband", "elbow", "pauldron", "spaulder", "arm guard", "shoulder pads"],
    ) {
        return Some(EquipSlot::Arms);
    }
    // (2026-08-19 zone sweep) The hands: gloves, gauntlets, mittens, wrist
    // jewelry. "ring" is a WORD-route (below) — an "earring" substring would
    // be caught by Head above, but "keyring" is one word the word-table
    // correctly ignores.
    if contains_any(name_lower, &["glove", "gauntlet", "mitten", "bracelet"]) {
        return Some(EquipSlot::Hands);
    }
    if contains_any(name_lower, &["legging", "pants", "trouser", "greave", "skirt", "kilt", "breeches", "hose"]) {
        return Some(EquipSlot::Legs);
    }
    if contains_any(
        name_lower,
        &["boot", "sabaton", "shoe", "sandal", "slipper", "stocking", "sock", "hosiery", "heels", "gaiter", "sneaker", "loafer"],
    ) {
        return Some(EquipSlot::Feet);
    }
    // (2026-08-19 zone sweep) Word-routed LAST — after every substring
    // needle, so "Knee-High Boots" hits the Feet needle before the "knee"
    // word can misroute it, and "Sleeveless Gown" stays Chest. These are
    // names whose SUBSTRING form would misroute: "shorts" (a "short"
    // substring would catch "Short Boots"), "sleeve" (would catch
    // "sleeveless"), "ring" (would catch "keyring"/"earring"), "spat"
    // (would catch "spatula"), "mail" (would catch "mailing").
    if let Some(slot) = word_routed_slot(name_lower) {
        return Some(slot);
    }
    None
}

/// Whole-word routing table for names whose substring form would misroute
/// (see `route_legacy_to_slot`'s tail). Checked AFTER every needle.
fn word_routed_slot(name_lower: &str) -> Option<EquipSlot> {
    const WORD_ROUTES: &[(&str, EquipSlot)] = &[
        ("shorts", EquipSlot::Legs),
        ("trunks", EquipSlot::Legs),
        ("speedo", EquipSlot::Legs),
        ("knee", EquipSlot::Legs),
        ("kneepads", EquipSlot::Legs),
        ("sleeve", EquipSlot::Arms),
        ("sleeves", EquipSlot::Arms),
        ("ring", EquipSlot::Hands),
        ("rings", EquipSlot::Hands),
        ("spat", EquipSlot::Feet),
        ("spats", EquipSlot::Feet),
        ("mail", EquipSlot::Chest),
        // (2026-08-20) "Cropped Tee" / "White Sneakers" packed instead of
        // equipping — single-word garment names route as WORDS so their
        // substrings can't catch unrelated items ("Canteen" ≠ "tee").
        ("tee", EquipSlot::Chest),
        ("tees", EquipSlot::Chest),
        ("trainer", EquipSlot::Feet),
        ("trainers", EquipSlot::Feet),
    ];
    for w in phrase_words(name_lower) {
        for (word, slot) in WORD_ROUTES {
            if w == *word {
                return Some(*slot);
            }
        }
    }
    None
}

/// True if `hay` contains any of the `needles` (case-insensitive on `hay`,
/// which callers pre-lowercase; needles are authored lowercase).
fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

// ---------------------------------------------------------------------------
// (2026-08-19 clothes-on-person fix) Under-clothing + auto-wear routing.
// ---------------------------------------------------------------------------

/// Under-clothing vocabulary: garments whose LOGICAL layer is Inner (worn
/// beneath whatever else holds the slot — a sock under a boot, a chemise
/// under a gown). Consulted ONLY as a layer PREFERENCE and only when it
/// AGREES with [`route_legacy_to_slot`] on the slot (the agreement gate keeps
/// a "Socketed Wand" — contains "sock" — routing MainHand/Outer, never Feet).
/// Pure vocabulary, no state.
fn under_garment_slot(name_lower: &str) -> Option<EquipSlot> {
    if contains_any(name_lower, &["sock", "stocking", "hosiery"]) {
        return Some(EquipSlot::Feet);
    }
    if contains_any(name_lower, &["undershirt", "chemise"]) {
        return Some(EquipSlot::Chest);
    }
    underwear_slot(name_lower)
}

// ---------------------------------------------------------------------------
// (2026-08-19 exposure upgrade, Chloe's upskirt question) Underwear — the
// deepest layer. Word-boundary matched (NOT substring): "bra" must not catch
// "bracelet", "briefs" must not catch "briefcase", and "Silk Slippers" must
// never route Chest. A "Slip of Paper" is stationery, not lingerie — which is
// why "slip"/"shift" are deliberately NOT in the vocabulary.
// ---------------------------------------------------------------------------

/// Underwear that covers the lower body (smallclothes family) — routes Legs,
/// claims the Inner layer beneath skirts/trousers.
/// Underwear and under-layers that anchor the LOWER body (smallclothes
/// family, thigh/waist foundations, below-the-hem hosiery) — routes Legs,
/// claims the Inner layer beneath skirts/trousers. The full zone sweep
/// (2026-08-19, Chloe ruling: leotards, panties, boxers, bras, swimsuits,
/// briefs, garter belts, "etc" — no body part overlooked):
/// - smallclothes: drawers, panties/panty, briefs, knickers, bloomers,
///   pantaloons, loincloth, fundoshi, boxers/boxer, thong, underwear,
///   undergarment(s)
/// - waist/thigh foundations: garter (belt), girdle, chastity (belt)
/// - below-the-hem hosiery that layers UNDER skirts (peeks — see
///   `extends_below_hem`): tights, petticoat, underskirt
/// - swim/athletic lower pieces: bottoms (bikini bottoms, swim bottoms)
///
/// ("Chest of Drawers" furniture would misroute here — accepted joke: nobody
/// packs a dresser.)
const UNDERWEAR_LEGS_WORDS: &[&str] = &[
    "drawers", "smallclothes", "underwear", "undergarment", "undergarments",
    "panties", "panty", "briefs", "knickers", "bloomers", "pantaloons",
    "loincloth", "fundoshi", "boxers", "boxer", "thong", "jock",
    "garter", "girdle", "chastity",
    "tights", "petticoat", "underskirt",
    "bottoms",
];

/// Underwear and under-layers that anchor the TORSO — routes Chest, claims
/// the Inner layer. Full-body garments (leotard, bodysuit, bodystocking,
/// swimsuit, union suit, long johns, sleepwear) anchor Chest too: the torso
/// is their dominant read, they render as the visible item when worn alone
/// (inner-only slots render), and they conceal naturally under outer layers.
/// Corsets/bustiers/basques join the under-family (worn over the chemise but
/// under the gown); their `garment_rank` stays mid so an EXPLICIT outer
/// placement still works.
const UNDERWEAR_CHEST_WORDS: &[&str] = &[
    "bra", "bralette", "brassiere", "bandeau", "camisole", "chemise", "undershirt",
    "corset", "bustier", "basque",
    "leotard", "bodysuit", "bodystocking",
    "swimsuit", "swimwear", "bikini",
    "negligee", "nightgown", "nightie", "nightdress", "lingerie",
];

/// Footwear vocabulary — when any of these appears, the name is FEET-family
/// and must never classify as underwear ("Thong Sandals" and "Thong Boots"
/// are footwear; the thong is a panty only when sandals are NOT involved).
/// Guard consulted at the top of [`underwear_slot`] only.
const FOOTWEAR_MARKERS: &[&str] = &["sandal", "slipper", "shoe", "boot", "heel", "wader"];

/// Whole-word underwear classification (lowercased name). `None` when no
/// underwear word appears. Shared by the router (slot), the under-garment
/// preference (Inner layer), and the exposure gate (prose mentions).
///
/// Two-pass (all Legs words scanned before any Chest word): a compound like
/// "Bikini Bottoms" names its zone with the LAST word — the legs pass must
/// win over the chest word "bikini" sitting earlier in the name.
fn underwear_slot(name_lower: &str) -> Option<EquipSlot> {
    if contains_any(name_lower, FOOTWEAR_MARKERS) {
        return None; // footwear names are never underwear, whatever else they contain
    }
    // Compound under-layer phrases whose individual words are too generic to
    // word-match ("johns", "suit").
    if contains_any(name_lower, &["long johns", "union suit"]) {
        return Some(EquipSlot::Chest);
    }
    let words = phrase_words(name_lower);
    if words.iter().any(|w| UNDERWEAR_LEGS_WORDS.contains(&w.as_str())) {
        return Some(EquipSlot::Legs);
    }
    if words.iter().any(|w| UNDERWEAR_CHEST_WORDS.contains(&w.as_str())) {
        return Some(EquipSlot::Chest);
    }
    None
}

/// Overness ranking within a slot — which garment sits on TOP when two share
/// it (2026-08-19 NPC-perception upgrade). Higher = more outer. Consulted by
/// every auto-placement path (the tracker's layer-less `[EQUIP]`, the `[PACK]`
/// auto-wear, the clothing seeds, the legacy migration) so a cloak OVER a
/// shirt never lands beneath it. Slot-local by design: rank only ever
/// compares two garments contending for the SAME slot, so the vocabulary is
/// per-slot and coarse. Pure heuristic on the lowercased name, mirroring
/// [`route_legacy_to_slot`]'s substring scan.
fn garment_rank(slot: EquipSlot, name_lower: &str) -> u8 {
    match slot {
        // Chest: cloaks/coats/surcoats drape over everything; armor and
        // mid-layers sit over base shirts/dresses/chemises.
        EquipSlot::Chest => {
            if contains_any(
                name_lower,
                &["cloak", "cape", "mantle", "coat", "jacket", "poncho", "shawl", "surcoat", "tabard"],
            ) {
                2
            } else if contains_any(
                name_lower,
                &[
                    "armor", "armour", "chestplate", "breastplate", "cuirass", "chainmail",
                    "chain mail", "hauberk", "sweater", "cardigan", "jerkin", "doublet",
                    "vest", "frock", "bodice", "corset",
                ],
            ) {
                1
            } else {
                0 // shirt/blouse/tunic/dress/gown/robe/chemise/undershirt
            }
        }
        // Feet: footwear (incl. spats + gaiters, worn OVER the shoe) sits
        // over socks/stockings/hosiery.
        EquipSlot::Feet => {
            if contains_any(name_lower, &["boot", "shoe", "sabaton", "sandal", "slipper", "heels", "spat", "gaiter"]) {
                2
            } else {
                0
            }
        }
        // Legs: greaves strap over trousers; trousers over hose/leggings.
        EquipSlot::Legs => {
            if contains_any(name_lower, &["greave"]) {
                2
            } else if contains_any(name_lower, &["trouser", "pants", "breeches", "skirt", "kilt"]) {
                1
            } else {
                0 // hose/leggings (the under-layer family)
            }
        }
        // Head: helmets/hoods/crowns sit over hats/caps/circlets.
        EquipSlot::Head => {
            if contains_any(name_lower, &["helm", "helmet", "hood", "crown"]) {
                2
            } else {
                1
            }
        }
        // Arms: rigid armor (pauldrons, bracers, vambraces) straps over
        // soft sleeves/armlets.
        EquipSlot::Arms => {
            if contains_any(name_lower, &["pauldron", "spaulder", "vambrace", "bracer", "elbow"]) {
                2
            } else {
                0 // sleeves, armlets, armbands
            }
        }
        // Hands: gauntlets over gloves; rings/bracelets are the base layer.
        EquipSlot::Hands => {
            if contains_any(name_lower, &["gauntlet"]) {
                2
            } else {
                0
            }
        }
        // Neck is single-purpose (one necklace, one gorget) — rank unused;
        // the exclusive-swap rule handles changes.
        EquipSlot::Neck => 1,
        // Hands (the READIED-weapon slots) are single-layer — rank unused.
        _ => 1,
    }
}

/// The outcome of a [`place_equipped`] call.
#[derive(Clone, Debug, PartialEq)]
pub enum Placement {
    /// The item was worn. `displaced` carries any prior occupants the caller
    /// must route to the pack (the never-vaporize contract) — empty when the
    /// placement layered cleanly (a free layer, or a demotion into a free
    /// Inner).
    Worn {
        layer: ItemLayer,
        displaced: Vec<EquippedItem>,
    },
    /// Auto placement declined (`force = false` only): both layers hold and
    /// the incoming garment ranks at or below the worn Outer — a genuine
    /// spare, the caller keeps it packed.
    Packed,
}

/// The common-sense wear placement (2026-08-19 clothes-on-person fix + the
/// NPC-perception upgrade, Chloe ruling: the system KNOWS where things
/// belong — there is no UI affordance for it). ONE placement authority shared
/// by the bracket `[EQUIP]` applier, the `[PACK]` auto-wear, the clothing
/// seeds, and the legacy migration, so the layer a garment lands in can never
/// disagree with the layer the narrator-render perceives it in.
///
/// - `explicit = Some(layer)` (the tracker emitted `layer=`): respected
///   verbatim — the prior occupant of THAT layer rides to `displaced`.
/// - Hands: single-layer, always Outer.
/// - Under-clothing (socks/stockings/chemise via [`under_garment_slot`]):
///   claims a free Inner FIRST (socks under boots, socks on bare feet — a
///   later boot takes the Outer above them); a second under-garment over a
///   held Inner is a spare (`Packed`, or an Inner swap when forced).
/// - Free Outer (bare or inner-only slot — boots onto socked feet): Outer.
/// - Incoming OUT-RANKS the worn Outer (cloak over shirt, boots over socks):
///   the incoming takes Outer and the incumbent is DEMOTED to a free Inner
///   (it stays worn — "she pulls a cloak over her shirt" no longer strips
///   the shirt off), else the incumbent rides to `displaced`.
/// - Equal rank on Feet/Head (footwear/headwear are exclusive — you don't
///   wear two pairs of boots): SWAP, the incumbent rides to `displaced`.
/// - Equal rank on Chest/Legs (a second shirt, armor over a vest): the free
///   Inner, else `Packed` (or an Inner swap when forced).
/// - Incoming UNDER-ranks the worn Outer (shirt under cloak): the free
///   Inner, else `Packed` (or an Inner swap when forced — "changing the
///   shirt under the cloak").
///
/// `force = true` (the bracket `[EQUIP]` contract: the model said the player
/// wears it, so it must end up worn) never returns `Packed`.
pub fn place_equipped(
    equipment: &mut Equipment,
    slot: EquipSlot,
    item: EquippedItem,
    explicit: Option<ItemLayer>,
    force: bool,
) -> Placement {
    if let Some(layer) = explicit {
        let layers = equipment.entry(slot).or_default();
        let old = write_layer(layers, layer, item);
        return Placement::Worn { layer, displaced: old.into_iter().collect() };
    }
    if matches!(slot, EquipSlot::MainHand | EquipSlot::OffHand) {
        let layers = equipment.entry(slot).or_default();
        let old = layers.outer.replace(item);
        return Placement::Worn { layer: ItemLayer::Outer, displaced: old.into_iter().collect() };
    }

    let lower = item.name.trim().to_lowercase();
    let rank = garment_rank(slot, &lower);
    let under = under_garment_slot(&lower) == Some(slot);

    // Same garment as an already-worn layer — a refresh emission (the tracker
    // re-emitted the worn item with new stats/tags): update IN PLACE, before
    // any layering logic (including the under-garment branch — a re-tagged
    // sock must not displace itself into the pack). Never a twin beneath it,
    // never a packed duplicate of the self-same object (the displaced vec
    // stays empty — the old version is replaced, not displaced into
    // co-existence).
    {
        let layers = equipment.entry(slot).or_default();
        if layers.outer.as_ref().is_some_and(|o| o.name.trim().to_lowercase() == lower) {
            layers.outer = Some(item);
            return Placement::Worn { layer: ItemLayer::Outer, displaced: Vec::new() };
        }
        if layers.inner.as_ref().is_some_and(|i| i.name.trim().to_lowercase() == lower) {
            layers.inner = Some(item);
            return Placement::Worn { layer: ItemLayer::Inner, displaced: Vec::new() };
        }
    }

    if under {
        let layers = equipment.entry(slot).or_default();
        if layers.inner.is_none() {
            layers.inner = Some(item);
            return Placement::Worn { layer: ItemLayer::Inner, displaced: Vec::new() };
        }
        if !force {
            return Placement::Packed;
        }
        let old = layers.inner.replace(item);
        return Placement::Worn { layer: ItemLayer::Inner, displaced: old.into_iter().collect() };
    }

    let layers = equipment.entry(slot).or_default();
    if layers.outer.is_none() {
        layers.outer = Some(item);
        return Placement::Worn { layer: ItemLayer::Outer, displaced: Vec::new() };
    }
    let outer_rank = layers
        .outer
        .as_ref()
        .map(|o| garment_rank(slot, &o.name.trim().to_lowercase()))
        .unwrap_or(0);

    if rank > outer_rank {
        // Over-garment: take the Outer, demote the incumbent beneath.
        let incumbent = layers.outer.take().expect("outer held checked above");
        layers.outer = Some(item);
        if layers.inner.is_none() {
            layers.inner = Some(incumbent);
            return Placement::Worn { layer: ItemLayer::Outer, displaced: Vec::new() };
        }
        return Placement::Worn { layer: ItemLayer::Outer, displaced: vec![incumbent] };
    }
    // Every zone EXCEPT Chest/Legs is exclusive at equal rank (one necklace,
    // one pair of gloves, one hat, one pair of boots — you don't wear two):
    // SWAP, the incumbent rides to `displaced`.
    if rank == outer_rank && !matches!(slot, EquipSlot::Chest | EquipSlot::Legs) {
        let old = layers.outer.replace(item);
        return Placement::Worn { layer: ItemLayer::Outer, displaced: old.into_iter().collect() };
    }
    if layers.inner.is_none() {
        layers.inner = Some(item);
        return Placement::Worn { layer: ItemLayer::Inner, displaced: Vec::new() };
    }
    if !force {
        return Placement::Packed;
    }
    let old = layers.inner.replace(item);
    Placement::Worn { layer: ItemLayer::Inner, displaced: old.into_iter().collect() }
}

/// Write `item` into one layer of a slot, returning the prior occupant.
fn write_layer(layers: &mut SlotLayers, layer: ItemLayer, item: EquippedItem) -> Option<EquippedItem> {
    match layer {
        ItemLayer::Outer => layers.outer.replace(item),
        ItemLayer::Inner => layers.inner.replace(item),
    }
}

/// True when this slot already wears an item with this (normalized) name —
/// the tracker-echo guard: a re-emitted `[EQUIP]` for the garment already on
/// that slot is a no-op, not a layer shuffle.
pub fn slot_holds_name(equipment: &Equipment, slot: EquipSlot, name: &str) -> bool {
    match equipment.get(&slot) {
        Some(layers) => [&layers.outer, &layers.inner]
            .into_iter()
            .flatten()
            .any(|it| it.name.trim().to_lowercase() == name.trim().to_lowercase()),
        None => false,
    }
}

// ---------------------------------------------------------------------------
// (2026-08-19 NPC-perception upgrade) Observer visibility — what renders.
// ---------------------------------------------------------------------------

/// Leg-garment vocabulary that covers the player down to the ANKLE — the
/// condition under which Feet inner layers (socks/stockings) stay hidden
/// inside the footwear. Long-form overrides run first ("long skirt" beats
/// the short-hem default for plain skirts/kilts); everything else in the
/// Legs routing vocabulary (trousers, pants, breeches, hose, leggings,
/// greaves) reaches the shoe. Names are matched lowercased.
fn legs_cover_ankles_name(name_lower: &str) -> bool {
    // Word-level overrides first — "Long Wool Skirt" must beat the short-hem
    // default for skirts even with words between "long" and "skirt".
    if contains_any(name_lower, &["maxi", "floor", "ankle", "long"]) {
        return true;
    }
    if contains_any(name_lower, &["skirt", "kilt", "short"]) {
        return false; // knee-or-higher hem — the ankle (and the sock) shows
    }
    true
}

/// True when the player's legs are covered down to the ankle — the cross-slot
/// fact that decides whether Feet inner layers peek. Consults the Legs slot
/// (outer, else inner — hose alone still covers the ankle), then falls back
/// to a one-piece Chest garment (dress/robe/gown drape to the ankle; a
/// cloak/coat/jacket does NOT — the legs hang bare beneath it). Bare legs →
/// false (the socks-between-shorts-and-shoes case).
pub fn legs_cover_ankles(equipment: &Equipment) -> bool {
    if let Some(layers) = equipment.get(&EquipSlot::Legs) {
        if let Some(worn) = layers.outer.as_ref().or(layers.inner.as_ref()) {
            return legs_cover_ankles_name(&worn.name.trim().to_lowercase());
        }
    }
    if let Some(chest_outer) = equipment
        .get(&EquipSlot::Chest)
        .and_then(|l| l.outer.as_ref())
    {
        let n = chest_outer.name.trim().to_lowercase();
        if contains_any(&n, &["mini", "short"]) {
            return false;
        }
        if contains_any(&n, &["dress", "robe", "gown"]) {
            return true;
        }
    }
    false
}

/// Legs inner garments that EXTEND BELOW a short hem — tights, stockings,
/// hose, leggings, petticoats, underskirts peek out from under a skirt.
/// Underwear (drawers, smallclothes, briefs) does NOT: it sits fully above
/// the hem and stays concealed until an exposure event
/// ([`narrative_trips_exposure`]). Substring match is safe here — the name
/// already routed to the Legs slot.
fn extends_below_hem(name_lower: &str) -> bool {
    contains_any(name_lower, &["stocking", "hose", "legging", "tight", "petticoat", "underskirt"])
}

/// True when this slot's Inner item physically peeks out from under its Outer
/// (an observer sees it). The peek channels: Feet inner shows when the legs
/// are NOT covered to the ankle (socks between shoe-top and hem); Legs inner
/// shows only when the hem is short AND the inner garment extends below it
/// (tights under a skirt — never underwear). Chest/Head inner under a worn
/// Outer never shows.
fn inner_peeks(slot: EquipSlot, equipment: &Equipment, ankles_covered: bool) -> bool {
    match slot {
        EquipSlot::Feet => !ankles_covered,
        EquipSlot::Legs => match equipment.get(&EquipSlot::Legs) {
            Some(layers) => match (&layers.outer, &layers.inner) {
                (Some(outer), Some(inner)) => {
                    !legs_cover_ankles_name(&outer.name.trim().to_lowercase())
                        && extends_below_hem(&inner.name.trim().to_lowercase())
                }
                _ => false,
            },
            None => false,
        },
        _ => false,
    }
}

/// Render the observer-visible equipment lines (the `equipped:` block body —
/// two-space-indented, canonical slot order Head→Feet). One line per slot:
/// the Outer item (with stats in parens when present), plus — when the Inner
/// peeks — a parenthetical naming it, so the narrator reads shorts + shoes as
/// "the socks show". An Inner-only slot (socks, no boots) renders the Inner
/// as the slot's visible item — it IS the topmost thing worn there. Belt +
/// pack are never here (carried, not worn).
pub fn visible_equipment_lines(equipment: &Equipment) -> Vec<String> {
    let ankles_covered = legs_cover_ankles(equipment);
    let mut out = Vec::new();
    for slot in EquipSlot::all() {
        let Some(layers) = equipment.get(slot) else { continue };
        let peek = layers.inner.is_some() && layers.outer.is_some() && inner_peeks(*slot, equipment, ankles_covered);
        match (&layers.outer, &layers.inner) {
            (Some(outer), Some(inner)) if peek => {
                let line = match outer.stats.as_deref() {
                    Some(s) if !s.trim().is_empty() => {
                        format!("  {}: {} ({})", slot.label(), outer.name, s)
                    }
                    _ => format!("  {}: {}", slot.label(), outer.name),
                };
                out.push(format!("{} ({} visible beneath)", line, inner.name));
            }
            (Some(outer), _) => {
                out.push(match outer.stats.as_deref() {
                    Some(s) if !s.trim().is_empty() => {
                        format!("  {}: {} ({})", slot.label(), outer.name, s)
                    }
                    _ => format!("  {}: {}", slot.label(), outer.name),
                });
            }
            (None, Some(inner)) => {
                out.push(match inner.stats.as_deref() {
                    Some(s) if !s.trim().is_empty() => {
                        format!("  {}: {} ({})", slot.label(), inner.name, s)
                    }
                    _ => format!("  {}: {}", slot.label(), inner.name),
                });
            }
            (None, None) => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// (2026-08-19 exposure upgrade) The event-driven reveal — "someone looked up
// her skirt". Concealed wear is hidden from the narrator BY DESIGN (the
// perception filter stops characters reacting to what they cannot see), so
// the narrator could otherwise improvise a garment that contradicts the
// tracked one. The gate below fires ONLY on turns whose narrative window
// involves exposure; those turns gain one `beneath:` line naming the real
// concealed items. Zero tokens on every other turn (Prime Mandate).
// ---------------------------------------------------------------------------

/// Names of the CONCEALED-but-worn items (hidden inner layers — the exact
/// complement of [`visible_equipment_lines`]), canonical slot order. These
/// are what an exposure event (looking up a skirt, undressing) would reveal;
/// they render ONLY behind [`narrative_trips_exposure`].
pub fn concealed_beneath_names(equipment: &Equipment) -> Vec<String> {
    let ankles_covered = legs_cover_ankles(equipment);
    let mut out = Vec::new();
    for slot in EquipSlot::all() {
        let Some(layers) = equipment.get(slot) else { continue };
        if let (Some(_), Some(inner)) = (&layers.outer, &layers.inner) {
            if !inner_peeks(*slot, equipment, ankles_covered) {
                out.push(inner.name.clone());
            }
        }
    }
    out
}

/// Exposure-event vocabulary (MAY-tune list, the stream-filter marker
/// discipline): hem-directed phrases + undressing words + any underwear word
/// appearing in the prose (a scene that names the smallclothes is obviously
/// about them). Scanned over the turn's narrative window — the player's
/// action + the preceding narrator beat (the tracker's OWN window, re-used at
/// the narrator tail). A false positive costs one `beneath:` line; a missed
/// reveal costs a narrator-invented garment that contradicts the tracked one
/// — bias toward firing. Player-initiated exposure trips the SAME turn; an
/// NPC-initiated look (written in the narrator's beat) trips the NEXT turn,
/// once the beat has entered the window.
pub fn narrative_trips_exposure(narrative: &[&str]) -> bool {
    const EXPOSURE_PHRASES: &[&str] = &[
        "up her skirt", "up my skirt", "up your skirt", "up his kilt",
        "under her skirt", "under my skirt", "under your skirt",
        "beneath her skirt", "beneath my skirt", "beneath your skirt",
        "beneath his kilt", "beneath the hem", "under the hem",
        "hikes her skirt", "hiked her skirt", "hiking her skirt",
        "hikes my skirt", "hiked my skirt",
        "lifts her skirt", "lifted her skirt", "lifting her skirt",
        "lifts my skirt", "lifted my skirt",
        "raises her skirt", "raised her skirt", "raising her skirt",
        "flashes her", "flashed her",
        "upskirt", "panty shot",
        "skirt up", "skirts up", "kilt up",
        "up her dress", "up my dress", "under her dress", "beneath her dress",
        "peeks up", "peeked up", "glancing up her", "glanced up her",
        "looks up her", "looked up her", "looking up her",
        "looks up my", "looked up my", "looking up my",
    ];
    const UNDRESSING_WORDS: &[&str] = &[
        "undress", "undresses", "undressed", "undressing",
        "disrobe", "disrobes", "disrobed", "disrobing",
        "strips", "stripped", "stripping",
        "naked", "nude", "nudity",
    ];
    for text in narrative {
        let lower = text.to_lowercase();
        if EXPOSURE_PHRASES.iter().any(|p| lower.contains(p)) {
            return true;
        }
        for w in phrase_words(&lower) {
            if UNDERWEAR_LEGS_WORDS.contains(&w.as_str())
                || UNDERWEAR_CHEST_WORDS.contains(&w.as_str())
                || UNDRESSING_WORDS.contains(&w.as_str())
            {
                return true;
            }
        }
    }
    false
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
    entities: &mut BTreeMap<String, serde_json::Value>,
    equipment: &mut Equipment,
    pack: &mut Vec<StackItem>,
) {
    // Collect first to avoid mutating while iterating (borrowck). Only
    // bare-string values are legacy-convention states ("equipped", "3 in
    // pack"); a widened structured value at an `item_*`/`inv_*` key is
    // unrecognized noise — skip it (leave it in the entity map untouched).
    // Sorted by key (2026-08-15 audit fix): HashMap iteration order is
    // random per process, and when two legacy items route to the same slot
    // the winner (outer vs inner vs pack fallback) was decided by hash
    // order — the same save could migrate differently every boot.
    let mut legacy: Vec<(String, String)> = entities
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
    legacy.sort_by(|a, b| a.0.cmp(&b.0));

    for (raw_key, state_raw) in legacy {
        // Strip the prefix → the item slug. Title-case it into a display name.
        let slug = raw_key
            .strip_prefix("item_")
            .or_else(|| raw_key.strip_prefix("inv_"))
            .unwrap_or(&raw_key);
        let mut name = prettify(slug);
        // (2026-08-16 audit LOW) Name hygiene before it enters the typed
        // model (it persists to player.json + renders into the carry-back):
        // drop empties entirely (an `item_` key with a bare slug migrates as
        // a nameless ghost stack) + clamp oversize slugs to the INV_NAME_MAX
        // discipline instead of storing a paragraph-long "name".
        name = name.trim().chars().take(crate::bracket_parser::INV_NAME_MAX).collect();
        if name.is_empty() {
            entities.remove(&raw_key);
            continue;
        }
        let name = name;
        let state = state_raw.trim().to_lowercase();

        // State hint: "equipped" → slot Outer; "N in pack" / "N" → pack qty N.
        // The panel convention was freeform, so we read defensively.
        let is_equipped = state == "equipped" || state == "worn" || state == "held";

        if let Some(slot) = route_legacy_to_slot(&name.to_lowercase()) {
            if is_equipped {
                // Route through the shared placement authority (2026-08-19):
                // under-clothing claims Inner, over-garments demote the
                // incumbent beneath them, anything displaced rides to the
                // pack — the same never-vaporize contract as before, now
                // rank-aware. Legacy items carry no tags (the old entity
                // convention had none) → default to empty; a future tracker
                // turn may tag them.
                let placement = place_equipped(
                    equipment,
                    slot,
                    EquippedItem { name: name.clone(), stats: None, ..Default::default() },
                    None,
                    false,
                );
                if let Placement::Worn { displaced, .. } = placement {
                    for d in displaced {
                        stack_upsert(
                            pack,
                            StackItem { name: d.name, qty: 1, weight: 1.0, stats: None, ..Default::default() },
                        );
                    }
                } else {
                    // Both layers hold and the item doesn't outrank them →
                    // pack.
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

// ---------------------------------------------------------------------------
// (2026-08-18 clothing-as-inventory ruling) Clothing seeds / migrates into
// the typed inventory — it is NOT an identity or appearance line.
// ---------------------------------------------------------------------------

/// Split a legacy `outfit` appearance-delta line ("bloodstained leather,
/// travel cloak, muddy boots") into per-garment chips. The historical seed
/// comma-joined the Player Creator's chip list into one delta value, so the
/// comma split is exact for authored data and a fine heuristic for tracker
/// output. Each chip is trimmed + capped at
/// `crate::bracket_parser::INV_NAME_MAX` (the same discipline
/// `clean_item_name` applies to bracket emissions — a 256-char TRAIT_MAX
/// chip must not become a paragraph-named inventory item). Empty chips drop.
pub fn split_outfit_line(line: &str) -> Vec<String> {
    line.split(',')
        .map(|c| {
            let capped: String = c.trim().chars().take(crate::bracket_parser::INV_NAME_MAX).collect();
            capped.trim().to_string()
        })
        .filter(|c| !c.is_empty())
        .collect()
}

/// Seed authored clothing chips into the typed inventory. Used at BOTH
/// chokepoints: `enter_fable_session` (a fresh New Game's SavedPlayer chip
/// list — the one-time starting kit) and `WorldSchema::load_split` (the
/// legacy `outfit` appearance-delta migration). Garments route to their body
/// slots via [`route_legacy_to_slot`] (cloak/dress/robe → Chest, trousers →
/// Legs, boots → Feet…); chips with no body-slot vocabulary (gloves, a
/// necklace, a sash) land in the pack — carried, still visible in the Soul
/// Gem panel. Contention routes through [`place_equipped`] (2026-08-19):
/// under-clothing claims Inner first, an over-garment DEMOTES the incumbent
/// beneath it (chip order can never invert the stack), anything that can't
/// layer falls to the pack — everything is preserved.
///
/// Deduped by normalized name (trim + lowercase) against the equipment
/// layers, the pack, AND earlier chips in the same batch — a mixed-era save
/// that already tracks "Travel Cloak" as an item while still carrying the
/// legacy outfit line must not mint a twin. Returns the number of chips
/// seeded (for the INV log line). Pure fn over the typed model.
pub fn seed_clothing_items(
    chips: &[String],
    equipment: &mut Equipment,
    pack: &mut Vec<StackItem>,
) -> usize {
    let mut known: Vec<String> = Vec::new();
    for layers in equipment.values() {
        for layer in [&layers.outer, &layers.inner] {
            if let Some(item) = layer {
                known.push(item.name.trim().to_lowercase());
            }
        }
    }
    for item in pack.iter() {
        known.push(item.name.trim().to_lowercase());
    }

    let mut seeded = 0;
    for chip in chips {
        let name = chip.trim();
        if name.is_empty() {
            continue;
        }
        let key = name.to_lowercase();
        if known.contains(&key) {
            continue; // already tracked — never mint a twin
        }
        // Garments are wearable: the equippable tag drives the Soul Gem
        // popup's EQUIP action once the item is in the pack.
        let tags = vec![ItemTag::Equippable];
        match route_legacy_to_slot(&key) {
            Some(slot) => {
                let item = EquippedItem {
                    name: name.to_string(),
                    stats: None,
                    tags: tags.clone(),
                };
                // (2026-08-19 clothes-on-person fix + NPC-perception upgrade)
                // ONE placement authority: under-clothing (socks, stockings,
                // a chemise) claims the INNER layer FIRST so chip order never
                // inverts the stack, and an over-garment seeded after its
                // base (Cloak after Shirt) DEMOTES the base to Inner instead
                // of burying beneath it — the worn layer always matches what
                // the narrator-render perceives.
                if let Placement::Packed = place_equipped(equipment, slot, item, None, false) {
                    // Both layers hold and the chip doesn't outrank them →
                    // pack (nothing is ever vaporized).
                    stack_upsert(
                        pack,
                        StackItem {
                            name: name.to_string(),
                            qty: 1,
                            stats: None,
                            tags,
                            ..StackItem::default()
                        },
                    );
                }
            }
            None => {
                stack_upsert(
                    pack,
                    StackItem {
                        name: name.to_string(),
                        qty: 1,
                        stats: None,
                        tags,
                        ..StackItem::default()
                    },
                );
            }
        }
        known.push(key);
        seeded += 1;
    }
    seeded
}

/// Seed a player card's `<inventory>` sibling (2026-08-19 v2 format):
/// Clothing routes through the shared garment router, weapon-ish Equipped
/// items claim the readied-hand slots (`main_hand` first, `off_hand` next),
/// and Accessories route too — SPECIFIC jewelry/wearables (a necklace, a
/// ring, gloves) auto-wear on their zone from turn 1 (2026-08-19 zone sweep,
/// Chloe ruling: "whatever accessories (that are obvious) equip themselves
/// upon the first run"); anything non-specific (a trinket, a charm) plus
/// Stored land in the pack. FRESH runs only — a resumed campaign's inventory
/// is authoritative. Returns the number of items seeded.
pub fn seed_player_inventory(
    inv: &crate::player::PlayerInventory,
    equipment: &mut Equipment,
    pack: &mut Vec<StackItem>,
) -> usize {
    let mut seeded = seed_clothing_items(&inv.clothing, equipment, pack);
    // Readied weapons: the Equipped line is for READIED items (the [EQUIP]
    // contract reserves main_hand/off_hand for weapons). WORD-boundary
    // matched (the `phrase_words` discipline the garment router uses):
    // substring matching let "bow" claim "Elbow Pads"/"Rainbow Scarf",
    // "wand" claim a "Wanderer's Cloak", "lance" a "Freelance Scribe"
    // (2026-08-20 audit) — so the one-word compounds that substring used
    // to catch ride as explicit terms.
    const WEAPON_TERMS: [&str; 22] = [
        "sword", "blade", "axe", "bow", "dagger", "staff", "spear", "hammer",
        "knife", "wand", "mace", "lance", "rapier", "scythe", "shortsword",
        "longsword", "broadsword", "greatsword", "longbow", "crossbow",
        "warhammer", "battleaxe",
    ];
    for item in inv.equipped.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let key = item.to_lowercase();
        let weaponish = WEAPON_TERMS.iter().any(|t| name_contains_word(&key, t));
        let tags = vec![ItemTag::Equippable];
        if weaponish {
            let main_held = equipment
                .get(&EquipSlot::MainHand)
                .map(|l| l.visible().is_some())
                .unwrap_or(false);
            let slot = if main_held { EquipSlot::OffHand } else { EquipSlot::MainHand };
            let layers = equipment.entry(slot).or_default();
            if layers.outer.is_none() {
                layers.outer = Some(EquippedItem {
                    name: item.to_string(),
                    stats: None,
                    tags,
                });
                seeded += 1;
                continue;
            }
        }
        // Not a readied weapon (or both hands full) → the pack (never
        // vaporized).
        stack_upsert(
            pack,
            StackItem {
                name: item.to_string(),
                qty: 1,
                stats: None,
                tags,
                ..StackItem::default()
            },
        );
        seeded += 1;
    }
    // Accessories: specific/jewelry pieces WEAR (the router knows them);
    // non-specific trinkets pack — "if something isn't specific then going
    // into inventory is perfectly fine" (Chloe, 2026-08-19).
    for item in inv.accessories.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let key = item.to_lowercase();
        let tags = vec![ItemTag::Equippable];
        let Some(slot) = route_legacy_to_slot(&key) else {
            // Non-specific trinket — the pack, no Equippable tag.
            stack_upsert(
                pack,
                StackItem {
                    name: item.to_string(),
                    qty: 1,
                    stats: None,
                    tags: Vec::new(),
                    ..StackItem::default()
                },
            );
            seeded += 1;
            continue;
        };
        // Dedup guard: an item the clothing pass already WEARS never mints
        // a twin (2026-08-20 audit M7: the old `!slot_holds_name`
        // predicate only skipped the WEAR and fell through to the pack,
        // duplicating the worn item).
        if slot_holds_name(equipment, slot, item) {
            continue;
        }
        match place_equipped(
            equipment,
            slot,
            EquippedItem {
                name: item.to_string(),
                stats: None,
                tags: tags.clone(),
            },
            None,
            false,
        ) {
            // Worn on its zone — never also packed.
            Placement::Worn { .. } => {}
            // Routable but the zone couldn't take it — the pack keeps it
            // (never vaporized).
            _ => {
                stack_upsert(
                    pack,
                    StackItem {
                        name: item.to_string(),
                        qty: 1,
                        stats: None,
                        tags,
                        ..StackItem::default()
                    },
                );
            }
        }
        seeded += 1;
    }
    for item in inv.stored.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        stack_upsert(
            pack,
            StackItem {
                name: item.to_string(),
                qty: 1,
                stats: None,
                tags: Vec::new(),
                ..StackItem::default()
            },
        );
        seeded += 1;
    }
    seeded
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
    fn stack_upsert_merges_despite_whitespace_and_case_noise() {
        // 2026-08-17 E4B shakedown P1a: the belt re-added existing items as
        // new entries in the playtest. The comparison now normalizes BOTH
        // sides — `Coin` merges with ` coin ` (Soul-Gem UI / legacy-save
        // entries that skipped the parser's clean gate).
        let mut items = vec![StackItem { name: "coin".into(), qty: 3, weight: 0.1, stats: None, ..Default::default() }];
        let added = stack_upsert(&mut items, StackItem { name: " Coin ".into(), qty: 2, weight: 0.1, stats: None, ..Default::default() });
        assert!(!added, "re-acquisition of an existing item must stack, not append");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].qty, 5);
        // The remove path normalizes the same way.
        assert!(stack_remove(&mut items, "  COIN ", 0));
        assert!(items.is_empty());
    }

    #[test]
    fn stack_remove_drops_whole_entry_at_qty_zero() {
        let mut items = vec![StackItem { name: "Arrow".into(), qty: 10, weight: 0.1, stats: None, ..Default::default() }];
        assert!(stack_remove(&mut items, "arrow", 0));
        assert!(items.is_empty(), "qty=0 removes the whole entry");
    }

    #[test]
    fn stack_restate_replaces_only_with_explicit_qty() {
        // (2026-08-22 review pin) The bare restatement contract: an explicit
        // qty REPLACES the count (the read-back loop's "the stack is now N");
        // a qty-LESS bare emission (parser-defaulted qty=1, replace_qty=false)
        // is existence-only — it must NOT collapse an existing stack to 1.
        let mut items = vec![StackItem { name: "Rope".into(), qty: 50, weight: 0.5, stats: None, ..Default::default() }];
        // Qty-less assertion: count preserved, no phantom change.
        assert!(!stack_restate(
            &mut items,
            StackItem { name: "rope".into(), qty: 1, weight: 0.5, stats: None, ..Default::default() },
            false,
        ));
        assert_eq!(items[0].qty, 50, "a qty-less bare restatement keeps the stored count");
        // Explicit qty: replace semantics.
        assert!(stack_restate(
            &mut items,
            StackItem { name: "Rope".into(), qty: 13, weight: 0.5, stats: None, ..Default::default() },
            true,
        ));
        assert_eq!(items[0].qty, 13);
        // First sighting is an acquisition regardless of form/flag.
        assert!(stack_restate(
            &mut items,
            StackItem { name: "Torch".into(), qty: 1, weight: 0.5, stats: None, ..Default::default() },
            false,
        ));
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].name, "Torch");
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
        let rendered = ps.render_for_prompt("").expect("non-default state renders");
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
        let rendered = ps.render_for_prompt("").expect("non-default renders");
        assert!(!rendered.contains("equipped:"), "empty equipment → no equipped block");
    }

    // ── Migration ────────────────────────────────────────────────────────

    #[test]
    fn migrate_legacy_items_routes_weapon_to_main_hand() {
        let mut entities: BTreeMap<String, serde_json::Value> = BTreeMap::new();
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
        let mut entities: BTreeMap<String, serde_json::Value> = BTreeMap::new();
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
        let mut entities: BTreeMap<String, serde_json::Value> = BTreeMap::new();
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

    /// 2026-08-15 audit fix: two legacy items contending for the SAME slot
    /// migrate deterministically (sorted by key) — HashMap iteration order
    /// used to decide outer-vs-inner-vs-pack at random per boot.
    #[test]
    fn migrate_legacy_items_slot_contention_is_deterministic() {
        let build = || {
            let mut entities: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            // Both route to chest ("armor"/"vest" keywords): alphabetically
            // "item_iron_vest" < "item_leather_armor", so the vest takes
            // Outer deterministically.
            entities.insert(
                "item_iron_vest".into(),
                serde_json::Value::String("equipped".into()),
            );
            entities.insert(
                "item_leather_armor".into(),
                serde_json::Value::String("equipped".into()),
            );
            let mut equipment = Equipment::new();
            let mut pack = Vec::new();
            migrate_legacy_items(&mut entities, &mut equipment, &mut pack);
            (equipment, pack)
        };
        // The SAME outcome on every run (the old HashMap order flipped it).
        for _ in 0..8 {
            let (equipment, pack) = build();
            let chest = equipment.get(&EquipSlot::Chest).expect("chest populated");
            assert_eq!(
                chest.outer.as_ref().expect("outer filled").name,
                "Iron Vest",
                "alphabetically-first key wins the Outer layer deterministically"
            );
            assert_eq!(
                chest.inner.as_ref().expect("inner filled").name,
                "Leather Armor"
            );
            assert!(pack.is_empty(), "both layers absorbed the contention");
        }
    }

    #[test]
    fn migrate_legacy_items_skips_structured_values() {
        // Widening guard (2026-08-11): a structured JSON value at an item_* key
        // is unrecognized noise — leave it in the entity map untouched.
        let mut entities: BTreeMap<String, serde_json::Value> = BTreeMap::new();
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

    // ── Clothing seeds (2026-08-18 clothing-as-inventory ruling) ─────────

    #[test]
    fn seed_clothing_routes_garments_to_body_slots() {
        let chips = vec![
            "Travel Cloak".to_string(),
            "Wool Trousers".to_string(),
            "Leather Boots".to_string(),
        ];
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        let seeded = seed_clothing_items(&chips, &mut equipment, &mut pack);
        assert_eq!(seeded, 3);
        assert!(pack.is_empty(), "routable garments never hit the pack");
        assert_eq!(
            equipment.get(&EquipSlot::Chest).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Travel Cloak"),
            "cloak routes to chest outer"
        );
        assert_eq!(
            equipment.get(&EquipSlot::Legs).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Wool Trousers")
        );
        assert_eq!(
            equipment.get(&EquipSlot::Feet).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Leather Boots")
        );
        // Seeded garments carry the equippable tag so the Soul Gem popup
        // offers EQUIP once they're in the pack.
        assert_eq!(
            equipment.get(&EquipSlot::Chest).and_then(|l| l.outer.as_ref()).map(|i| i.tags.clone()),
            Some(vec![ItemTag::Equippable])
        );
    }

    #[test]
    fn seed_clothing_one_piece_garments_route_chest() {
        let chips = vec!["Silk Gown".to_string(), "Wool Robe".to_string()];
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        seed_clothing_items(&chips, &mut equipment, &mut pack);
        let chest = equipment.get(&EquipSlot::Chest).expect("gown on chest");
        assert_eq!(chest.outer.as_ref().unwrap().name, "Silk Gown");
        // Contention: the robe layers Inner rather than displacing the gown.
        assert_eq!(chest.inner.as_ref().unwrap().name, "Wool Robe");
        assert!(pack.is_empty());
    }

    #[test]
    fn seed_clothing_unroutable_chip_lands_in_pack() {
        // Gloves/a necklace have no body-slot vocabulary (hands are READIED
        // weapons only) — carried, not vaporized.
        let chips = vec!["Silk Gloves".to_string()];
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        let seeded = seed_clothing_items(&chips, &mut equipment, &mut pack);
        assert_eq!(seeded, 1);
        assert!(equipment.is_empty());
        assert_eq!(pack.len(), 1);
        assert_eq!(pack[0].name, "Silk Gloves");
        assert!(pack[0].tags.contains(&ItemTag::Equippable));
    }

    #[test]
    fn seed_clothing_dedupes_against_existing_items_and_batch() {
        // Mixed-era save: the cloak is already tracked as an item while the
        // legacy outfit line still names it — no twin. Same-batch dupes
        // collapse too.
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers {
                outer: Some(EquippedItem { name: "Travel Cloak".into(), stats: None, ..Default::default() }),
                inner: None,
            },
        );
        let mut pack = Vec::new();
        let chips = vec![
            "travel cloak".to_string(),
            "Linen Dress".to_string(),
            "Linen Dress".to_string(),
        ];
        let seeded = seed_clothing_items(&chips, &mut equipment, &mut pack);
        assert_eq!(seeded, 1, "existing cloak + same-batch dupe both skip");
        let chest = equipment.get(&EquipSlot::Chest).expect("chest held the cloak");
        assert_eq!(chest.outer.as_ref().unwrap().name, "Travel Cloak");
        assert_eq!(chest.inner.as_ref().unwrap().name, "Linen Dress");
        assert!(pack.is_empty());
    }

    #[test]
    fn seed_clothing_both_layers_full_spills_to_pack() {
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers {
                outer: Some(EquippedItem { name: "Travel Cloak".into(), stats: None, ..Default::default() }),
                inner: Some(EquippedItem { name: "Padded Vest".into(), stats: None, ..Default::default() }),
            },
        );
        let mut pack = Vec::new();
        let seeded = seed_clothing_items(&["Silk Gown".to_string()], &mut equipment, &mut pack);
        assert_eq!(seeded, 1);
        assert_eq!(pack.len(), 1, "both layers held → pack (preserve, never vaporize)");
        assert_eq!(pack[0].name, "Silk Gown");
    }

    #[test]
    fn seed_cape_routes_chest_not_head() {
        // "cape" contains the Head needle "cap" — the chest block runs first,
        // so a cape (and a "hooded cloak") routes to Chest, never Head.
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        seed_clothing_items(
            &["Velvet Cape".to_string(), "Leather Cap".to_string()],
            &mut equipment,
            &mut pack,
        );
        assert!(equipment.contains_key(&EquipSlot::Chest), "cape → chest");
        assert_eq!(
            equipment.get(&EquipSlot::Chest).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Velvet Cape")
        );
        assert!(equipment.contains_key(&EquipSlot::Head), "a plain cap still routes to head");
    }

    #[test]
    fn split_outfit_line_splits_trims_and_caps() {
        let chips = split_outfit_line("bloodstained leather, travel cloak , muddy boots");
        assert_eq!(chips, vec!["bloodstained leather", "travel cloak", "muddy boots"]);
        // Oversize chip clamps to the INV_NAME_MAX discipline.
        let long = "x".repeat(crate::bracket_parser::INV_NAME_MAX + 50);
        let capped = split_outfit_line(&long);
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].chars().count(), crate::bracket_parser::INV_NAME_MAX);
        // Empty segments drop entirely.
        assert!(split_outfit_line("  ,, ").is_empty());
    }

    // ── Narrative phrase resolution (2026-08-17 E4B follow-up) ───────────
    // Every narrative string below is VERBATIM from the live playtest turns
    // (V5 shop scene + the innkeeper example) — the exact corpus that
    // produced the one-word shorthand emissions ("hard", "dark", "tin",
    // "rough").

    const V5_SHOP_TEXT: &str = "I find the general store and buy provisions for the road: a wedge of hard cheese, a loaf of dark bread, and a coil of rough mooring rope like my old one, plus a cheap tin lantern. I count out coin from my pouch — prices up here are fairer than Cinderfen's.";

    #[test]
    fn fragment_resolves_to_the_narrative_noun_phrase() {
        let none: Vec<String> = Vec::new();
        assert_eq!(
            resolve_item_fragment("hard", &[V5_SHOP_TEXT], &none),
            Some("hard cheese".to_string())
        );
        assert_eq!(
            resolve_item_fragment("dark", &[V5_SHOP_TEXT], &none),
            Some("dark bread".to_string())
        );
        assert_eq!(
            resolve_item_fragment("tin", &[V5_SHOP_TEXT], &none),
            Some("tin lantern".to_string())
        );
        // Absorption stops at the stopword "like".
        assert_eq!(
            resolve_item_fragment("rough", &[V5_SHOP_TEXT], &none),
            Some("rough mooring rope".to_string())
        );
        // The innkeeper example: the stopword "and" ends the phrase.
        let inn = "The innkeeper slides over a cold dark ale and a block of hard biscuit.";
        assert_eq!(
            resolve_item_fragment("dark", &[inn], &none),
            Some("dark ale".to_string())
        );
        assert_eq!(
            resolve_item_fragment("hard", &[inn], &none),
            Some("hard biscuit".to_string())
        );
    }

    #[test]
    fn fragment_never_absorbs_prepositions() {
        // (2026-08-21) "he grabbed a cookie across from me" used to absorb
        // "across" into the stored name ("cookie across") — prepositions
        // are function words, never name material. With "across" stopworded
        // the one-word fragment stays verbatim (None = no expansion).
        let none: Vec<String> = Vec::new();
        let text = "He grabbed a cookie across from me.";
        assert_eq!(resolve_item_fragment("cookie", &[text], &none), None);
    }

    #[test]
    fn fragment_prefers_the_stored_stack_spelling_and_merges() {
        let existing = vec!["Rough Mooring Rope".to_string()];
        assert_eq!(
            resolve_item_fragment("rough", &[V5_SHOP_TEXT], &existing),
            Some("Rough Mooring Rope".to_string()),
            "narrative phrase narrows to the stored stack — exact-merge spelling"
        );
        // And the follow-on stack_upsert merges into it (no twin entry).
        let mut pack = vec![StackItem {
            name: "Rough Mooring Rope".to_string(),
            qty: 2,
            ..Default::default()
        }];
        let added = stack_upsert(
            &mut pack,
            StackItem {
                name: resolve_item_fragment("rough", &[V5_SHOP_TEXT], &existing).unwrap(),
                qty: 1,
                ..Default::default()
            },
        );
        assert!(!added, "resolved fragment merged into the existing stack");
        assert_eq!(pack.len(), 1);
        assert_eq!(pack[0].qty, 3);
    }

    #[test]
    fn fragment_inventory_relink_and_ambiguity_rules() {
        let none: Vec<String> = Vec::new();
        // Unique containment relinks ("mire" → the stored mire-oil) — the
        // removal path's only resolution arm.
        let existing = vec!["mire-oil".to_string()];
        assert_eq!(
            resolve_item_fragment("mire", &[], &existing),
            Some("mire-oil".to_string())
        );
        // Exact stored name is a no-op relink ("coin" → "coin").
        assert_eq!(
            resolve_item_fragment("coin", &[], &["coin".to_string()]),
            Some("coin".to_string())
        );
        // Ambiguous containment declines (wrong merge beats terse name).
        let both = vec!["dark ale".to_string(), "dark bread".to_string()];
        assert_eq!(resolve_item_fragment("dark", &[], &both), None);
        // But the narrative phrase disambiguates against the stored names.
        assert_eq!(
            resolve_item_fragment("dark", &["I buy a loaf of dark bread from her."], &both),
            Some("dark bread".to_string())
        );
    }

    #[test]
    fn fragment_boundaries_keep_correct_emissions_verbatim() {
        let none: Vec<String> = Vec::new();
        // Multi-word fragments are NEVER rewritten.
        assert_eq!(resolve_item_fragment("rough rope", &[V5_SHOP_TEXT], &none), None);
        // Stopword fragments are not items.
        assert_eq!(resolve_item_fragment("and", &[V5_SHOP_TEXT], &none), None);
        // A head-noun fragment (nothing to absorb: next word is a stopword)
        // stays as-emitted — "food." followed by "The moment…" absorbs nothing.
        assert_eq!(
            resolve_item_fragment("food", &["eat the last crumbs of my food. The moment I sit down, exhaustion wins"], &none),
            None
        );
        // Unknown single word with no narrative hit + no inventory match.
        assert_eq!(resolve_item_fragment("lantern", &[], &none), None);
    }

    // ── (2026-08-19 clothes-on-person fix) auto-wear routing ─────────────

    #[test]
    fn seed_clothing_under_garments_layer_inner_regardless_of_order() {
        // Chip order must never invert the stack: socks authored BEFORE the
        // boots still land Inner (boots Outer, socks Inner) — and the
        // reversed order produces the identical layout.
        for chips in [
            vec!["Wool Socks".to_string(), "Leather Boots".to_string()],
            vec!["Leather Boots".to_string(), "Wool Socks".to_string()],
        ] {
            let mut equipment = Equipment::new();
            let mut pack = Vec::new();
            seed_clothing_items(&chips, &mut equipment, &mut pack);
            let feet = equipment.get(&EquipSlot::Feet).expect("feet populated");
            assert_eq!(feet.outer.as_ref().unwrap().name, "Leather Boots", "boots outer");
            assert_eq!(feet.inner.as_ref().unwrap().name, "Wool Socks", "socks inner");
            assert!(pack.is_empty());
        }
    }

    #[test]
    fn place_equipped_layers_with_common_sense() {
        let mut equipment = Equipment::new();
        // Bare equipment: a shirt claims the free chest Outer.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Linen Shirt"), None, true),
            ItemLayer::Outer,
            &[],
        );
        // THE over-garment fix: a cloak over the worn shirt DEMOTES the shirt
        // to Inner — it stays worn (the old default-Outer displacement
        // stripped it off to the pack, and the old auto-wear buried the cloak
        // UNDER the shirt).
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Travel Cloak"), None, true),
            ItemLayer::Outer,
            &[],
        );
        let chest = equipment.get(&EquipSlot::Chest).unwrap();
        assert_eq!(chest.outer.as_ref().unwrap().name, "Travel Cloak");
        assert_eq!(chest.inner.as_ref().unwrap().name, "Linen Shirt");
        // A mid-layer (padded doublet) under the cloak: free Inner.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Padded Doublet"), None, true),
            ItemLayer::Inner,
            &[],
        );
        assert_eq!(
            equipment.get(&EquipSlot::Chest).unwrap().inner.as_ref().unwrap().name,
            "Padded Doublet"
        );
    }

    #[test]
    fn place_equipped_over_garment_with_both_layers_full_displaces_outer() {
        // Cloak arrives over shirt + doublet (both layers held): the cloak
        // still takes Outer; the SHIRT (old outer) rides to the pack — the
        // doublet keeps the Inner.
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers {
                outer: Some(item("Linen Shirt")),
                inner: Some(item("Padded Doublet")),
            },
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Travel Cloak"), None, true),
            ItemLayer::Outer,
            &["Linen Shirt"],
        );
        let chest = equipment.get(&EquipSlot::Chest).unwrap();
        assert_eq!(chest.outer.as_ref().unwrap().name, "Travel Cloak");
        assert_eq!(chest.inner.as_ref().unwrap().name, "Padded Doublet");
    }

    #[test]
    fn place_equipped_footwear_swaps_at_equal_rank() {
        // Feet/Head are exclusive: a second pair of boots SWAPS (the worn pair
        // rides to the pack) — it never layers beneath.
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Feet,
            SlotLayers { outer: Some(item("Leather Boots")), inner: Some(item("Wool Socks")) },
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Feet, item("Soft Slippers"), None, true),
            ItemLayer::Outer,
            &["Leather Boots"],
        );
        let feet = equipment.get(&EquipSlot::Feet).unwrap();
        assert_eq!(feet.outer.as_ref().unwrap().name, "Soft Slippers");
        assert_eq!(feet.inner.as_ref().unwrap().name, "Wool Socks", "socks stay on");
    }

    #[test]
    fn place_equipped_under_clothing_prefers_inner_and_spares_stack() {
        let mut equipment = Equipment::new();
        // Socks onto bare feet: Inner (a later boot takes the Outer above).
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Feet, item("Wool Socks"), None, false),
            ItemLayer::Inner,
            &[],
        );
        assert!(equipment.get(&EquipSlot::Feet).unwrap().outer.is_none());
        // Boots over the socks: Outer, socks stay Inner.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Feet, item("Leather Boots"), None, false),
            ItemLayer::Outer,
            &[],
        );
        // A SECOND sock-layer garment over a held Inner is a genuine spare
        // (auto mode).
        assert_eq!(
            place_equipped(&mut equipment, EquipSlot::Feet, item("Silk Stockings"), None, false),
            Placement::Packed
        );
        // The agreement gate: "Socketed Wand" contains "sock" but routes
        // MainHand (weapon needles win) — hands are always Outer, never Feet.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::MainHand, item("Socketed Wand"), None, false),
            ItemLayer::Outer,
            &[],
        );
        // Unroutable names never reach placement (the caller routes to pack).
    }

    #[test]
    fn place_equipped_base_garment_under_worn_outer_or_packed() {
        // A shirt under a worn cloak: free Inner → worn beneath.
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers { outer: Some(item("Travel Cloak")), inner: None },
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Linen Shirt"), None, false),
            ItemLayer::Inner,
            &[],
        );
        // Both layers held + the incoming doesn't outrank → a genuine spare
        // (the [PACK] auto-wear rule). Forced (the bracket [EQUIP] contract:
        // the model said the player wears it) → swaps the Inner.
        assert_eq!(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Silk Chemise"), None, false),
            Placement::Packed
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Silk Chemise"), None, true),
            ItemLayer::Inner,
            &["Linen Shirt"],
        );
        assert_eq!(
            equipment.get(&EquipSlot::Chest).unwrap().inner.as_ref().unwrap().name,
            "Silk Chemise"
        );
    }

    #[test]
    fn place_equipped_explicit_layer_respected_verbatim() {
        // The tracker's explicit layer= wins: even a rank-2 cloak goes Inner
        // when the model says so, displacing whatever held that layer.
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers { outer: Some(item("Linen Shirt")), inner: Some(item("Padded Doublet")) },
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Travel Cloak"), Some(ItemLayer::Inner), true),
            ItemLayer::Inner,
            &["Padded Doublet"],
        );
        let chest = equipment.get(&EquipSlot::Chest).unwrap();
        assert_eq!(chest.outer.as_ref().unwrap().name, "Linen Shirt");
        assert_eq!(chest.inner.as_ref().unwrap().name, "Travel Cloak");
    }

    #[test]
    fn place_equipped_same_name_refreshes_in_place() {
        // A re-emission of the WORN garment (new stats) updates it in place —
        // no twin layered beneath, no phantom duplicate in the pack.
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers { outer: Some(item("Travel Cloak")), inner: Some(item("Linen Shirt")) },
        );
        let refreshed = EquippedItem {
            name: "Travel Cloak".into(),
            stats: Some("bloodstained".into()),
            ..Default::default()
        };
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, refreshed, None, true),
            ItemLayer::Outer,
            &[],
        );
        let chest = equipment.get(&EquipSlot::Chest).unwrap();
        assert_eq!(chest.outer.as_ref().unwrap().stats.as_deref(), Some("bloodstained"));
        assert_eq!(chest.inner.as_ref().unwrap().name, "Linen Shirt", "shirt untouched");
        // Same for a refresh of the worn INNER (socks re-tagged).
        let socks = EquippedItem {
            name: "Wool Socks".into(),
            tags: vec![ItemTag::Equippable],
            ..Default::default()
        };
        equipment.insert(
            EquipSlot::Feet,
            SlotLayers { outer: Some(item("Leather Boots")), inner: Some(item("Wool Socks")) },
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Feet, socks, None, true),
            ItemLayer::Inner,
            &[],
        );
        assert_eq!(
            equipment.get(&EquipSlot::Feet).unwrap().inner.as_ref().unwrap().tags,
            vec![ItemTag::Equippable]
        );
    }

    #[test]
    fn seed_clothing_later_overgarment_takes_outer() {
        // Chip order can no longer invert the stack either: Shirt then Cloak
        // seeds the cloak OVER the shirt (previously the cloak landed Inner).
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        seed_clothing_items(
            &["Linen Shirt".to_string(), "Travel Cloak".to_string()],
            &mut equipment,
            &mut pack,
        );
        let chest = equipment.get(&EquipSlot::Chest).expect("chest populated");
        assert_eq!(chest.outer.as_ref().unwrap().name, "Travel Cloak");
        assert_eq!(chest.inner.as_ref().unwrap().name, "Linen Shirt");
        assert!(pack.is_empty());
    }

    // ── (2026-08-19 NPC-perception upgrade) observer visibility ──────────

    /// Chloe's pinned scenario: an NPC will not see under a jacket, but shorts
    /// + shoes means they definitely see the socks.
    #[test]
    fn visible_render_socks_peek_with_shorts_but_not_trousers() {
        let shorts = |legs_garment: &str| {
            let mut equipment = Equipment::new();
            equipment.insert(
                EquipSlot::Legs,
                SlotLayers { outer: Some(item(legs_garment)), inner: None },
            );
            equipment.insert(
                EquipSlot::Feet,
                SlotLayers { outer: Some(item("Leather Boots")), inner: Some(item("Wool Socks")) },
            );
            equipment
        };
        let shorts_render = visible_equipment_lines(&shorts("Wool Shorts"));
        assert_eq!(
            shorts_render.iter().find(|l| l.contains("Feet")).map(|l| l.as_str()),
            Some("  Feet: Leather Boots (Wool Socks visible beneath)"),
            "shorts + boots → the socks peek"
        );
        let trousers_render = visible_equipment_lines(&shorts("Wool Trousers"));
        assert_eq!(
            trousers_render.iter().find(|l| l.contains("Feet")).map(|l| l.as_str()),
            Some("  Feet: Leather Boots"),
            "trousers cover the ankle → the socks stay hidden"
        );
        assert!(!trousers_render.iter().any(|l| l.contains("Wool Socks")));
    }

    #[test]
    fn visible_render_leg_coverage_sources() {
        let boots_and_socks = || {
            let mut equipment = Equipment::new();
            equipment.insert(
                EquipSlot::Feet,
                SlotLayers { outer: Some(item("Leather Boots")), inner: Some(item("Wool Socks")) },
            );
            equipment
        };
        // Bare legs → socks show.
        assert!(visible_equipment_lines(&boots_and_socks())
            .iter()
            .any(|l| l.contains("Wool Socks visible beneath")));
        // A full-length gown (one-piece Chest garment, no Legs) covers the ankle.
        let mut gown = boots_and_socks();
        gown.insert(EquipSlot::Chest, SlotLayers { outer: Some(item("Silk Gown")), inner: None });
        assert!(!visible_equipment_lines(&gown).iter().any(|l| l.contains("Wool Socks")));
        // A jacket does NOT cover the legs.
        let mut jacket = boots_and_socks();
        jacket.insert(EquipSlot::Chest, SlotLayers { outer: Some(item("Leather Jacket")), inner: None });
        assert!(visible_equipment_lines(&jacket).iter().any(|l| l.contains("Wool Socks visible beneath")));
        // Hose alone (Legs inner, no outer) still covers the ankle.
        let mut hose = boots_and_socks();
        hose.insert(EquipSlot::Legs, SlotLayers { outer: None, inner: Some(item("Wool Hose")) });
        assert!(!visible_equipment_lines(&hose).iter().any(|l| l.contains("Wool Socks")));
        // A long skirt covers; a plain skirt/kilt does not.
        let mut long_skirt = boots_and_socks();
        long_skirt.insert(EquipSlot::Legs, SlotLayers { outer: Some(item("Long Wool Skirt")), inner: None });
        assert!(!visible_equipment_lines(&long_skirt).iter().any(|l| l.contains("Wool Socks")));
        let mut kilt = boots_and_socks();
        kilt.insert(EquipSlot::Legs, SlotLayers { outer: Some(item("Tartan Kilt")), inner: None });
        assert!(visible_equipment_lines(&kilt).iter().any(|l| l.contains("Wool Socks visible beneath")));
    }

    #[test]
    fn visible_render_inner_only_slot_renders_its_item() {
        // Socks with no boots (acquired barefoot, or boots unequipped over
        // them): the Inner IS the topmost thing in the slot — it renders as
        // the slot's visible item (previously the slot rendered NOTHING and
        // the narrator saw bare feet).
        let mut equipment = Equipment::new();
        equipment.insert(EquipSlot::Feet, SlotLayers { outer: None, inner: Some(item("Wool Socks")) });
        assert_eq!(
            visible_equipment_lines(&equipment),
            vec!["  Feet: Wool Socks".to_string()]
        );
    }

    #[test]
    fn visible_render_chest_inner_stays_hidden_and_stats_ride() {
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Chest,
            SlotLayers {
                outer: Some(EquippedItem {
                    name: "Heavy Cloak".into(),
                    stats: Some("+1 DEF".into()),
                    ..Default::default()
                }),
                inner: Some(item("Linen Shirt")),
            },
        );
        equipment.insert(
            EquipSlot::MainHand,
            SlotLayers { outer: Some(item("Iron Sword")), inner: None },
        );
        let lines = visible_equipment_lines(&equipment);
        assert!(lines.contains(&"  Chest: Heavy Cloak (+1 DEF)".to_string()));
        assert!(lines.contains(&"  Main Hand: Iron Sword".to_string()));
        assert!(!lines.iter().any(|l| l.contains("Linen Shirt")), "chest inner never peeks");
    }

    #[test]
    fn visible_render_legs_inner_peeks_under_short_hems_only() {
        // Tights under a skirt show; under trousers they don't.
        let legs = |outer: &str| {
            let mut equipment = Equipment::new();
            equipment.insert(
                EquipSlot::Legs,
                SlotLayers { outer: Some(item(outer)), inner: Some(item("Cotton Tights")) },
            );
            visible_equipment_lines(&equipment)
        };
        assert_eq!(
            legs("Pleated Skirt").iter().find(|l| l.contains("Legs")).map(|l| l.as_str()),
            Some("  Legs: Pleated Skirt (Cotton Tights visible beneath)")
        );
        assert_eq!(
            legs("Wool Trousers").iter().find(|l| l.contains("Legs")).map(|l| l.as_str()),
            Some("  Legs: Wool Trousers")
        );
    }

    #[test]
    fn slot_holds_name_is_normalized() {
        let mut equipment = Equipment::new();
        equipment.insert(EquipSlot::Chest, SlotLayers { outer: Some(item("Travel Cloak")), inner: None });
        assert!(slot_holds_name(&equipment, EquipSlot::Chest, " travel cloak "));
        assert!(!slot_holds_name(&equipment, EquipSlot::Chest, "Wool Robe"));
        assert!(!slot_holds_name(&equipment, EquipSlot::Feet, "Travel Cloak"));
    }

    // ── (2026-08-19 exposure upgrade) underwear + the upskirt gate ────────

    #[test]
    fn underwear_routes_by_whole_word_and_lands_inner() {
        // Word-boundary routing: smallclothes family → Legs, torso family → Chest.
        assert_eq!(route_legacy_to_slot("cotton drawers"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("lace panties"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("linen petticoat"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("silk camisole"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("satin bra"), Some(EquipSlot::Chest));
        // The collision pins: the substring families must never misroute.
        assert_eq!(route_legacy_to_slot("leather bracelet"), None, "'bra' never catches 'bracelet'");
        assert_eq!(route_legacy_to_slot("oiled briefcase"), None, "'briefs' never catches 'briefcase'");
        assert_eq!(route_legacy_to_slot("silk slippers"), Some(EquipSlot::Feet), "slippers stay footwear");
        assert_eq!(route_legacy_to_slot("slip of paper"), None, "a slip of paper is stationery");
        // (2026-08-20) The GLM clothing-chip gaps: everyday modern garments
        // must EQUIP, not pack; the collision pins must not misroute.
        assert_eq!(route_legacy_to_slot("cropped tee"), Some(EquipSlot::Chest), "'tee' word-route claims the chest");
        assert_eq!(route_legacy_to_slot("graphic tee"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("crop top"), Some(EquipSlot::Chest), "'crop top' phrase needle");
        assert_eq!(route_legacy_to_slot("sneakers"), Some(EquipSlot::Feet), "'sneaker' claims the feet");
        assert_eq!(route_legacy_to_slot("white trainers"), Some(EquipSlot::Feet), "'trainer' word-route claims the feet");
        assert_eq!(route_legacy_to_slot("leather canteen"), None, "'tee' never catches 'canteen'");
        assert_eq!(route_legacy_to_slot("leather loafer"), Some(EquipSlot::Feet));
        // Underwear claims the Inner layer under a worn skirt (the
        // under-garment preference agrees with the router).
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Legs,
            SlotLayers { outer: Some(item("Pleated Skirt")), inner: None },
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Legs, item("Cotton Drawers"), None, false),
            ItemLayer::Inner,
            &[],
        );
    }

    #[test]
    fn underwear_under_skirt_stays_hidden_but_tights_peek() {
        // Chloe's upskirt ruling, static half: underwear ends ABOVE the hem —
        // concealed by the skirt in the every-turn render; tights/petticoats
        // extend below it and peek.
        let legs = |inner: &str| {
            let mut equipment = Equipment::new();
            equipment.insert(
                EquipSlot::Legs,
                SlotLayers { outer: Some(item("Pleated Skirt")), inner: Some(item(inner)) },
            );
            equipment
        };
        assert_eq!(
            visible_equipment_lines(&legs("Cotton Drawers")),
            vec!["  Legs: Pleated Skirt".to_string()],
            "drawers concealed — no peek, no line"
        );
        assert_eq!(
            visible_equipment_lines(&legs("Wool Tights")),
            vec!["  Legs: Pleated Skirt (Wool Tights visible beneath)".to_string()]
        );
        assert!(
            visible_equipment_lines(&legs("Linen Petticoat"))
                .iter()
                .any(|l| l.contains("Linen Petticoat visible beneath")),
            "a petticoat intentionally shows at the hem"
        );
    }

    #[test]
    fn exposure_gate_fires_on_relevant_windows_only() {
        assert!(narrative_trips_exposure(&["The guard crouches and looks up her skirt."]));
        assert!(narrative_trips_exposure(&["I hike my skirt up to cross the stream."]));
        assert!(narrative_trips_exposure(&["She flashes her smallclothes at the crowd."]));
        assert!(narrative_trips_exposure(&["He begins to undress her."]));
        assert!(narrative_trips_exposure(&["her cotton drawers show"]));
        assert!(!narrative_trips_exposure(&["She eats bread by the fire."]));
        assert!(
            !narrative_trips_exposure(&["I polish my silk slippers."]),
            "slippers are footwear, not underwear (word boundary)"
        );
        assert!(!narrative_trips_exposure(&[]));
    }

    #[test]
    fn concealed_names_are_the_visibility_complement() {
        let mut equipment = Equipment::new();
        equipment.insert(
            EquipSlot::Feet,
            SlotLayers { outer: Some(item("Leather Boots")), inner: Some(item("Wool Socks")) },
        );
        equipment.insert(
            EquipSlot::Legs,
            SlotLayers { outer: Some(item("Wool Shorts")), inner: Some(item("Cotton Drawers")) },
        );
        // The socks PEAK (shorts → bare ankles) → they're visible, not
        // concealed; the drawers are concealed.
        assert_eq!(
            concealed_beneath_names(&equipment),
            vec!["Cotton Drawers".to_string()]
        );
    }

    #[test]
    fn beneath_line_renders_only_when_gated() {
        let mut ps = PlayerState::default();
        ps.equipment.insert(
            EquipSlot::Legs,
            SlotLayers { outer: Some(item("Pleated Skirt")), inner: Some(item("Cotton Drawers")) },
        );
        ps.equipment.insert(
            EquipSlot::Chest,
            SlotLayers { outer: Some(item("Wool Shawl")), inner: Some(item("Silk Camisole")) },
        );
        let ungated = ps.render_for_prompt("").expect("renders");
        assert!(!ungated.contains("Cotton Drawers"), "concealed wear hidden by default");
        assert!(!ungated.contains("Silk Camisole"));
        assert!(ungated.contains("Legs: Pleated Skirt"));
        let gated = ps.render_for_prompt_with_beneath(true, "").expect("renders");
        assert!(gated.contains("beneath (visible this moment): "), "the gated line renders");
        assert!(gated.contains("Cotton Drawers"), "the REAL tracked garment is revealed");
        assert!(gated.contains("Silk Camisole"));
    }

    // ── (2026-08-19 zone sweep) every undergarment family, every body zone ──

    #[test]
    fn undergarment_zone_sweep_routes_every_family() {
        // Lower body: smallclothes, foundations, hosiery, swim lowers.
        for name in [
            "cotton drawers", "lace panties", "silk boxers", "boxer briefs",
            "linen knickers", "wool bloomers", "leather loincloth", "silk fundoshi",
            "garter belt", "silk girdle", "chastity belt", "lace thong",
            "wool tights", "linen petticoat", "silk underskirt", "bikini bottoms",
            "linen pantaloons",
        ] {
            assert_eq!(route_legacy_to_slot(name), Some(EquipSlot::Legs), "legs family: {name}");
        }
        // Torso: lingerie, shapewear, full-body + sleep + swim uppers.
        for name in [
            "satin bra", "lace bralette", "silk brassiere", "linen bandeau",
            "silk camisole", "linen chemise", "cotton undershirt", "leather corset",
            "silk bustier", "velvet basque", "black leotard", "silk bodysuit",
            "fishnet bodystocking", "one piece swimsuit", "swimwear", "string bikini",
            "silk negligee", "cotton nightgown", "flannel nightie", "satin nightdress",
            "lace lingerie", "thermal long johns", "cotton union suit",
        ] {
            assert_eq!(route_legacy_to_slot(name), Some(EquipSlot::Chest), "chest family: {name}");
        }
        // Feet stay feet — the footwear guard + needle order.
        for name in ["thong sandals", "short boots", "leather boots", "wool socks", "silk stockings"] {
            assert_eq!(route_legacy_to_slot(name), Some(EquipSlot::Feet), "feet family: {name}");
        }
        // Outer legwear the sweep discovered was unroutable (packed!).
        assert_eq!(route_legacy_to_slot("wool shorts"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("swim trunks"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("speedo"), Some(EquipSlot::Legs));
        // The non-collision pins hold.
        assert_eq!(route_legacy_to_slot("oiled briefcase"), None);
        assert_eq!(route_legacy_to_slot("leather bracelet"), None);
        assert_eq!(route_legacy_to_slot("slip of paper"), None);
        assert_eq!(route_legacy_to_slot("shortsword"), Some(EquipSlot::MainHand));
        assert_eq!(route_legacy_to_slot("short cloak"), Some(EquipSlot::Chest), "cloak outranks the word check");
    }

    #[test]
    fn full_body_and_swim_layers_place_inner_and_render_alone() {
        // A leotard/swimsuit claims the Chest Inner — worn ALONE it renders as
        // the slot's visible item (inner-only slots render); under a dress it
        // conceals silently until an exposure event.
        let mut equipment = Equipment::new();
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Black Leotard"), None, false),
            ItemLayer::Inner,
            &[],
        );
        assert_eq!(
            visible_equipment_lines(&equipment),
            vec!["  Chest: Black Leotard".to_string()],
            "a leotard worn alone IS the visible garment"
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Chest, item("Silk Gown"), None, false),
            ItemLayer::Outer,
            &[],
        );
        // Gown over leotard: the leotard conceals (chest inner never peeks)
        // and is revealable by the gate.
        assert_eq!(
            visible_equipment_lines(&equipment),
            vec!["  Chest: Silk Gown".to_string()]
        );
        assert_eq!(concealed_beneath_names(&equipment), vec!["Black Leotard".to_string()]);
        // Boxers under trousers: concealed, no peek.
        equipment.insert(
            EquipSlot::Legs,
            SlotLayers { outer: Some(item("Wool Trousers")), inner: Some(item("Silk Boxers")) },
        );
        assert!(!visible_equipment_lines(&equipment).iter().any(|l| l.contains("Boxers")));
        assert!(concealed_beneath_names(&equipment).contains(&"Silk Boxers".to_string()));
        // Garter belt under a skirt: concealed until exposed (it ends above
        // the hem — not an extends-below-hem garment).
        equipment.insert(
            EquipSlot::Legs,
            SlotLayers { outer: Some(item("Pleated Skirt")), inner: Some(item("Silk Garter Belt")) },
        );
        assert!(!visible_equipment_lines(&equipment).iter().any(|l| l.contains("Garter")));
        assert!(narrative_trips_exposure(&["Her skirt shifts; the garter belt shows."]));
    }

    #[test]
    fn exposure_gate_covers_the_full_undergarment_vocabulary() {
        // The gate scans the SAME word lists the router uses — prose naming
        // any undergarment family trips it.
        for prose in [
            "her leotard clings",
            "she adjusts her bikini top",
            "a glimpse of her negligee",
            "his boxers show above the waistband",
            "the garter belt snaps",
            "lace lingerie beneath",
        ] {
            assert!(narrative_trips_exposure(&[prose]), "gate fires on: {prose}");
        }
    }

    // ── (2026-08-19 zone sweep II) arms/hands/neck + armor/pads/jewelry ────

    #[test]
    fn zone_sweep_arms_hands_neck_jewelry_route() {
        // Neck: specific jewelry + neckwear (the neck brace included).
        for name in [
            "silver necklace", "wooden amulet", "ruby pendant", "golden locket",
            "steel gorget", "velvet choker", "bronze torc", "silk scarf",
            "silver brooch", "silk cravat", "leather neck brace",
        ] {
            assert_eq!(route_legacy_to_slot(name), Some(EquipSlot::Neck), "neck family: {name}");
        }
        // Arms: sleeves, bracers, elbow/shoulder armor.
        for name in [
            "leather sleeves", "steel bracers", "leather vambraces", "gold armlet",
            "red armband", "padded elbow pads", "steel pauldrons", "iron spaulders",
            "leather arm guards", "lined shoulder pads",
        ] {
            assert_eq!(route_legacy_to_slot(name), Some(EquipSlot::Arms), "arms family: {name}");
        }
        // Hands: gloves, gauntlets, mittens, rings, wrist jewelry.
        for name in [
            "leather gloves", "steel gauntlets", "wool mittens",
            "silver ring", "golden rings", "beaded bracelet",
        ] {
            assert_eq!(route_legacy_to_slot(name), Some(EquipSlot::Hands), "hands family: {name}");
        }
        // Armor, pads, straps, over-shoe wear.
        assert_eq!(route_legacy_to_slot("dwarven chainmail"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("chain mail hauberk"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("quilted vest"), Some(EquipSlot::Chest), "vests stay chest");
        assert_eq!(route_legacy_to_slot("padded knee pads"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("leather jock strap"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("wool leggings"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("polished spats"), Some(EquipSlot::Feet));
        assert_eq!(route_legacy_to_slot("canvas gaiters"), Some(EquipSlot::Feet));
        // The word-boundary pins: substring needles win FIRST, words are the
        // fallback — each of these would misroute on a substring form.
        assert_eq!(route_legacy_to_slot("knee-high boots"), Some(EquipSlot::Feet), "boots beat the knee word");
        assert_eq!(route_legacy_to_slot("sleeveless gown"), Some(EquipSlot::Chest), "gown beats the sleeve word");
        assert_eq!(route_legacy_to_slot("golden earring"), Some(EquipSlot::Head), "earrings are head jewelry");
        assert_eq!(route_legacy_to_slot("brass keyring"), None, "keyring is one word — not a ring");
        assert_eq!(route_legacy_to_slot("kitchen spatula"), None, "spatula is not spats");
    }

    #[test]
    fn new_zone_slots_parse_and_render_head_to_foot() {
        for (id, slot) in [
            ("neck", EquipSlot::Neck),
            ("arms", EquipSlot::Arms),
            ("hands", EquipSlot::Hands),
        ] {
            assert_eq!(EquipSlot::from_id(id), Some(slot));
        }
        // Canonical order is head-to-foot (the equipped block reads as a
        // head-to-toe look): Head < Neck < Chest < Arms < Hands < hands
        // (weapons) < Legs < Feet.
        let order: Vec<&str> = EquipSlot::all().iter().map(|s| s.id()).collect();
        assert_eq!(
            order,
            vec!["head", "neck", "chest", "arms", "hands", "main_hand", "off_hand", "legs", "feet"]
        );
    }

    #[test]
    fn new_zones_place_swap_and_layer() {
        let mut equipment = Equipment::new();
        // Necklace on a bare neck.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Neck, item("Silver Necklace"), None, false),
            ItemLayer::Outer,
            &[],
        );
        // A second necklace SWAPS (exclusive at equal rank), never layers.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Neck, item("Ruby Pendant"), None, true),
            ItemLayer::Outer,
            &["Silver Necklace"],
        );
        // Sleeves first, bracers over them: rank 0 under rank 2.
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Arms, item("Leather Sleeves"), None, false),
            ItemLayer::Outer,
            &[],
        );
        assert_worn(
            place_equipped(&mut equipment, EquipSlot::Arms, item("Steel Bracers"), None, false),
            ItemLayer::Outer,
            &[],
        );
        let arms = equipment.get(&EquipSlot::Arms).unwrap();
        assert_eq!(arms.outer.as_ref().unwrap().name, "Steel Bracers");
        assert_eq!(arms.inner.as_ref().unwrap().name, "Leather Sleeves");
        // Render carries the new zones head-to-foot; arms inner stays hidden.
        let lines = visible_equipment_lines(&equipment);
        assert!(lines.contains(&"  Neck: Ruby Pendant".to_string()));
        assert!(lines.contains(&"  Arms: Steel Bracers".to_string()));
        assert!(!lines.iter().any(|l| l.contains("Sleeves")), "arms inner never peeks");
    }

    #[test]
    fn player_seed_wears_specific_accessories() {
        // (2026-08-19 Chloe ruling: "whatever accessories (that are obvious)
        // equips themselves upon the first run") Specific jewelry/gear wears
        // its zone; the non-specific trinket packs.
        let inv = crate::player::PlayerInventory {
            clothing: vec!["Linen Shirt".into(), "Wool Trousers".into()],
            equipped: vec!["Iron Sword".into()],
            accessories: vec!["Silver Necklace".into(), "Leather Gloves".into(), "Lucky Trinket".into()],
            stored: vec!["Bedroll".into()],
        };
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        let seeded = seed_player_inventory(&inv, &mut equipment, &mut pack);
        assert_eq!(seeded, 7);
        assert_eq!(
            equipment.get(&EquipSlot::Neck).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Silver Necklace")
        );
        assert_eq!(
            equipment.get(&EquipSlot::Hands).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Leather Gloves")
        );
        assert_eq!(
            equipment.get(&EquipSlot::MainHand).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Iron Sword")
        );
        let names: Vec<&str> = pack.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Lucky Trinket"), "non-specific accessory packs");
        assert!(names.contains(&"Bedroll"));
    }

    #[test]
    fn player_seed_never_mints_pack_twins_of_worn_accessories() {
        // M7 (2026-08-20): an accessory the clothing pass already WEARS is a
        // no-op — the old path skipped the wear but still fell through to
        // the pack, duplicating the worn item.
        let inv = crate::player::PlayerInventory {
            clothing: vec!["Leather Gloves".into()],
            equipped: vec![],
            accessories: vec!["Leather Gloves".into()],
            stored: vec![],
        };
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        let seeded = seed_player_inventory(&inv, &mut equipment, &mut pack);
        assert!(pack.is_empty(), "the worn glove must not also pack, got {pack:?}");
        assert_eq!(
            equipment.get(&EquipSlot::Hands).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Leather Gloves")
        );
        assert_eq!(seeded, 1, "only the clothing pass seeds the item");
    }

    #[test]
    fn player_seed_weapon_terms_are_word_matched() {
        // L6 (2026-08-20): substring "bow" claimed "Elbow Pads" (and
        // "Rainbow Scarf") for the readied hands; word-boundary matching
        // keeps clothing in the pack while real weapons still claim slots.
        let inv = crate::player::PlayerInventory {
            clothing: vec![],
            equipped: vec!["Elbow Pads".into(), "Hunting Bow".into()],
            accessories: vec![],
            stored: vec![],
        };
        let mut equipment = Equipment::new();
        let mut pack = Vec::new();
        seed_player_inventory(&inv, &mut equipment, &mut pack);
        assert_eq!(
            equipment.get(&EquipSlot::MainHand).and_then(|l| l.outer.as_ref()).map(|i| i.name.as_str()),
            Some("Hunting Bow")
        );
        let names: Vec<&str> = pack.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Elbow Pads"), "pads are clothing, not a weapon: {names:?}");
    }

    /// Test helper: assert a `Worn` placement's layer + the NAMES of any
    /// displaced occupants (the caller-packs-them contract).
    fn assert_worn(p: Placement, layer: ItemLayer, displaced_names: &[&str]) {
        match p {
            Placement::Worn { layer: l, displaced } => {
                assert_eq!(l, layer, "worn layer");
                let names: Vec<String> = displaced.iter().map(|d| d.name.clone()).collect();
                let want: Vec<String> = displaced_names.iter().map(|s| s.to_string()).collect();
                assert_eq!(names, want, "displaced occupants (the caller must pack them)");
            }
            Placement::Packed => panic!("expected Worn, got Packed"),
        }
    }

    /// Test helper: a bare named item.
    fn item(name: &str) -> EquippedItem {
        EquippedItem { name: name.to_string(), ..Default::default() }
    }

    #[test]
    fn garment_vocabulary_expansion_routes_common_chips() {
        // The 2026-08-19 expansion: GLM-authored chip names that hit NO needle
        // (and dumped whole wardrobes into the pack) now route.
        assert_eq!(route_legacy_to_slot("wool coat"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("travel garb"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("elegant attire"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("silk chemise"), Some(EquipSlot::Chest));
        assert_eq!(route_legacy_to_slot("silver circlet"), Some(EquipSlot::Head));
        assert_eq!(route_legacy_to_slot("wool hose"), Some(EquipSlot::Legs));
        assert_eq!(route_legacy_to_slot("silk hosiery"), Some(EquipSlot::Feet));
        // Weapon vocabulary still wins over garment needles in compounds.
        assert_eq!(route_legacy_to_slot("shortsword"), Some(EquipSlot::MainHand));
    }
}
