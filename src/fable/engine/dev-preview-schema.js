// =============================================================
// DEV PREVIEW MOCK SCHEMA (?dev=preview)
//
// The dev preview is a PURE-FRONTEND layout preview (no Tauri backend):
// fable_schema_get / fable_active_card_get fail in the plain browser, so
// the paperdoll heatsink + the Soul Gem inventory panel + the left-drawer
// header have nothing to render. This module ships a frozen mock schema
// (mirroring the player_state shape documented in AGENTS.md §7 "Inventory"
// + the BodyPart/BodyPartState wire format) so those surfaces render real
// test data in the preview.
//
// The mock mirrors apps/fable/cards/rusty_tavern/rusty_tavern.player.json:
// 5 injuries across 5 severity tiers (every heatsink bloom color), all 6
// equipment slots filled (chest two-layer), a 4-slot belt at the cap, + an
// 8-item pack with varied tags. BodyPart + BodyPartState use PascalCase
// wire names (no serde rename_all); EquipSlot + ItemTag use snake_case.
//
// Consumers import { getDevSchema, getDevActiveCard, isDevPreview } and
// call them INSTEAD of invoke('fable_schema_get') / fable_active_card_get
// when isDevPreview() is true. The helpers return null when NOT in dev
// preview, so production code paths are untouched.
// =============================================================

export function isDevPreview() {
  try {
    if (new URLSearchParams(window.location.search).get('dev') === 'preview') return true;
    const h = window.location.hash.replace(/^#/, '');
    return new URLSearchParams(h).get('dev') === 'preview';
  } catch (_) {
    return false;
  }
}

// The mock active-card identity (mirrors stage.js's DEV_PREVIEW branch).
export function getDevActiveCard() {
  return {
    name: 'Game Master',
    player_name: 'Wanderer',
    card_portrait_url: '',
    player_portrait_url: '',
    npc_names: [],
  };
}

// The mock WorldSchema. Only the surfaces the preview exercises are
// populated: player_state (injuries + inventory + appearance) + a minimal
// world_clock / travel_graph / weather so the left-drawer header renders.
export function getDevSchema() {
  // Deep-clone on each call so a consumer that mutates the returned object
  // (inventory-panel's handleAction edits player_state in place before
  // fable_schema_set) doesn't poison the frozen source for the next caller.
  return structuredClone(DEV_SCHEMA);
}

const DEV_SCHEMA = {
  // (2026-08-20 P3) `day_label: 'Day 1'` REMOVED: a phantom field (WorldClock
  // carries only current_minutes/last_tick_minutes on the wire) holding the
  // app-wide-banned "Day N" string. The date lives in the top-level
  // `calendar` label, which the mock now carries so the calendar click-card
  // renders realistically in preview.
  calendar: 'March 15',
  world_clock: { current_minutes: 540, last_tick_minutes: 540 },
  weather: { condition: 'Rain, cold' },
  travel_graph: {
    current_node: 'rusty_tavern',
    nodes: [
      { id: 'rusty_tavern', name: 'The Rusty Tavern', setting: 'A drafty roadside inn' },
    ],
  },
  player_state: {
    body: {
      Head: 'Yellow',
      LeftShoulder: 'Orange',
      RightUpperArm: 'Red',
      LeftLowerLeg: 'Purple',
      RightHand: 'Black',
    },
    injury_details: {
      Head: ['Grazing wound across the temple'],
      LeftShoulder: ['Deep lance thrust, still bleeding'],
      RightUpperArm: ['Shattered bone from a mace blow'],
      LeftLowerLeg: ['Mangled by a bear trap'],
      RightHand: ['Severed'],
    },
    stamina: 'Winded',
    wealth: 47,
    reputation: -2,
    current_appearance_deltas: {
      hair_color: 'greasy black, tied back',
      scars: 'jagged line across the right cheek',
      wounds: 'limping badly; right arm strapped',
    },
    equipment: {
      head:      { outer: { name: 'Dented Iron Helm', stats: '+1 DEF', tags: ['equippable'] } },
      chest:     { outer: { name: 'Road-Worn Cloak', tags: ['equippable'] }, inner: { name: 'Boiled Leather Cuirass', stats: '+2 DEF', tags: ['equippable'] } },
      main_hand: { outer: { name: 'Notched Longsword', stats: '+3 ATK', tags: ['equippable'] } },
      off_hand:  { outer: { name: 'Cracked Buckler', stats: '+1 DEF', tags: ['equippable'] } },
      legs:      { outer: { name: 'Reinforced Greaves', tags: ['equippable'] } },
      feet:      { outer: { name: 'Worn Travel Boots', tags: ['equippable'] } },
    },
    belt: [
      { name: 'Health Potion', qty: 2, weight: 0.5, stats: 'restores 2d4 HP', tags: ['consumable', 'pouchable'] },
      { name: 'Lockpick Set',  qty: 1, weight: 0.2, tags: ['pouchable'] },
      { name: 'Throwing Knife', qty: 3, weight: 0.25, tags: ['equippable', 'pouchable'] },
      { name: 'Torch',         qty: 1, weight: 1.0 },
    ],
    // (2026-08-23 pouch ruling) The wallet stack — auto-routed coin/keys/ID
    // cargo; the POUCH dock icon renders it in the dev preview too.
    pouch: [
      { name: 'Silver Coins',  qty: 12, weight: 0.02 },
      { name: 'Brass Key',     qty: 1, weight: 0.1, stats: 'stamped with the mill mark' },
      { name: 'Identity Papers', qty: 1, weight: 0.05 },
      { name: 'Ruby',          qty: 1, weight: 0.1 },
    ],
    pack: [
      { name: 'Bedroll',           qty: 1, weight: 3.0 },
      { name: 'Rations',           qty: 4, weight: 1.0, tags: ['consumable'] },
      { name: 'Hempen Rope',       qty: 1, weight: 2.5, stats: '50 feet' },
      { name: 'Antidote',          qty: 1, weight: 0.2, tags: ['consumable', 'pouchable'] },
      { name: 'Sealed Letter',     qty: 1, weight: 0.05, stats: 'wax seal unbroken' },
      { name: 'Tinderbox',         qty: 1, weight: 0.5 },
      { name: 'Old Map',           qty: 1, weight: 0.1 },
    ],
  },
};
