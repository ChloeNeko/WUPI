// =============================================================
// FABLE POUCH — the wallet classifier (2026-08-23 pouch ruling).
//
// "Pocketing" is RETIRED: small items no longer ride the pants/belt as
// pockets. The POUCH is the player's wallet — currency, coins, keys, ID
// papers, and small valuables ("anything that can fit in a wallet") auto-
// route into it at every acquisition path; everything else falls into the
// pack (inventory) by default.
//
// This module is the JS TWIN of Rust `equipment::pouch_fit` /
// `stash_target` (src-tauri/src/equipment.rs): the same three word lists,
// the same guard-first algorithm, byte-for-byte. The Rust side routes every
// acquisition ([PACK], [NPC_ITEM player], the starting-kit seed, belt spill);
// this twin gates the Soul Gem panel's manual POUCH action. Keep the
// vocabularies in sync — same discipline as GARMENT_OVER_VOCAB
// (inventory-panel.js ↔ equipment.rs).
// =============================================================

// Whole-word needles that make a name POUCH-FIT. Matched on the lowercased
// word tokens of the item name (mirrors POUCH_FIT_WORDS in equipment.rs).
export const POUCH_FIT_WORDS = new Set([
  // Currency + coin.
  'coin', 'coins', 'currency', 'money', 'cash', 'change', 'credits',
  'coppers', 'silvers', 'golds', 'shillings', 'pennies', 'pence',
  'farthings', 'banknote', 'banknotes', 'dollar', 'dollars', 'cent',
  'cents', 'nickel', 'nickels', 'dime', 'dimes', 'scrip', 'voucher',
  'vouchers', 'piece', 'pieces',
  // Keys.
  'key', 'keys', 'keycard', 'keyring',
  // ID + papers.
  'id', 'ids', 'identity', 'identification', 'passport', 'passports',
  'visa', 'visas', 'license', 'licence', 'licenses', 'licences',
  'permit', 'permits', 'papers', 'document', 'documents', 'deed',
  'deeds', 'certificate', 'certificates', 'credential', 'credentials',
  'diploma', 'diplomas',
  // Small valuables (loose stones — jewelry is GUARD-excluded).
  'gem', 'gems', 'gemstone', 'gemstones', 'jewel', 'jewels',
  'diamond', 'diamonds', 'ruby', 'rubies', 'sapphire', 'sapphires',
  'emerald', 'emeralds', 'opal', 'opals', 'pearl', 'pearls',
  'amethyst', 'amethysts', 'topaz', 'topazes', 'onyx', 'garnet',
  'garnets', 'tourmaline', 'tourmalines', 'aquamarine', 'peridot',
  'zircon', 'spinel', 'ingot', 'ingots', 'nugget', 'nuggets',
  'signet', 'token', 'tokens', 'wallet', 'wallets', 'purse',
  'coinpurse',
]);

// Whole-word needles that VETO pouch-fit even when a fit word also appears —
// worn jewelry, household goods, and containers are not wallet cargo
// (mirrors POUCH_GUARD_WORDS in equipment.rs).
export const POUCH_GUARD_WORDS = new Set([
  // Worn jewelry + adornments.
  'ring', 'rings', 'earring', 'earrings', 'necklace', 'necklaces',
  'pendant', 'pendants', 'amulet', 'amulets', 'bracelet', 'bracelets',
  'bangle', 'bangles', 'crown', 'crowns', 'circlet', 'tiara', 'anklet',
  'brooch', 'choker', 'torc', 'armband', 'armlet',
  // Tableware + household valuables (valuable, but not wallet-sized).
  'goblet', 'chalice', 'cup', 'cups', 'plate', 'plates', 'tray',
  'trays', 'platter', 'platters', 'bowl', 'bowls', 'candlestick',
  'candelabra', 'mirror', 'mirrors', 'brush', 'brushes', 'comb',
  'combs', 'spoon', 'spoons', 'fork', 'forks', 'statue', 'statuette',
  'figurine', 'idol', 'idols', 'mask', 'masks', 'lantern', 'lanterns',
  'pot', 'pans', 'kettle', 'teapot',
  // Consumables + gear that merely LOOK coin-adjacent.
  'bottle', 'bottles', 'vial', 'vials', 'flask', 'flasks', 'potion',
  'potions', 'knife', 'knives', 'dagger', 'daggers', 'sword', 'swords',
  'wire', 'wires', 'sheet', 'sheets',
  // Worn-carrier cargo — "change of CLOTHES" is not pocket change.
  'clothes', 'clothing', 'garment', 'garments', 'outfit', 'outfits',
]);

// Bare metal names that read as MONEY only in a short name ("3 gold",
// "gold pieces") — in a longer name they're material descriptors
// (mirrors POUCH_METAL_WORDS in equipment.rs).
export const POUCH_METAL_WORDS = new Set(['gold', 'silver', 'copper', 'platinum', 'electrum']);

// Tokenize an item name into lowercase word tokens — the JS twin of Rust
// `phrase_words` (split on non-alphanumerics, drop empties). (2026-08-24
// review P2) The class is Unicode-aware (`\p{L}\p{N}`, matching Rust's
// `char::is_alphanumeric`) — the old ASCII-only `[^a-z0-9]` SPLIT on every
// accented letter, so "Clé d'Or" tokenized as ["cl","d","or"] in JS while
// Rust saw ["clé","d","or"] — the twins diverged on any accented name and
// the panel pouched what the backend packed.
function nameWords(name) {
  return String(name || '')
    .toLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter(Boolean);
}

// True when a name is wallet cargo — currency, coins, keys, ID papers, or a
// small valuable — and belongs in the POUCH rather than the pack. Pure; the
// mirror of Rust `equipment::pouch_fit`. Guard words veto first (a "Gold
// Ring" is worn, a "Pearl Necklace" is worn), then fit words, then the
// ≤2-word metal rule ("3 gold" is money; "Silver-Trimmed Saddle" is not).
export function pouchFits(name) {
  const words = nameWords(name);
  if (!words.length) return false;
  if (words.some((w) => POUCH_GUARD_WORDS.has(w))) return false;
  if (words.some((w) => POUCH_FIT_WORDS.has(w))) return true;
  return words.length <= 2 && words.some((w) => POUCH_METAL_WORDS.has(w));
}
