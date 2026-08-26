// Unit tests for the POUCH wallet classifier's pure surface (2026-08-23
// pouch ruling). Plain Node ESM — no test runner. Run:
// `node tests/pouch.test.mjs`. Exits non-zero on any failure. Mirrors
// tests/spellcheck.test.mjs style.
//
// engine/pouch.js is the JS TWIN of Rust `equipment::pouch_fit` /
// `stash_target` (src-tauri/src/equipment.rs — pinned there by
// `pouch_fit_wallet_cargo_matches` / `pouch_fit_worn_and_household_vetoed` /
// `stash_target_routes_wallet_cargo_to_pouch`). These pins mirror the Rust
// cases so a vocabulary drift on either side trips a test. The panel/DOM
// pieces (the dock icon, the POUCH inspection view) are browser-only +
// exercised manually.
import { strict as assert } from 'node:assert';
import { pouchFits, POUCH_FIT_WORDS, POUCH_GUARD_WORDS, POUCH_METAL_WORDS } from '../src/fable/engine/pouch.js';

let passed = 0;
let failed = 0;
function test(name, fn) {
  try {
    fn();
    passed += 1;
    console.log(`  ✓ ${name}`);
  } catch (e) {
    failed += 1;
    console.error(`  ✗ ${name}`);
    console.error(`    ${e.message}`);
  }
}

console.log('pouch.test.mjs — the wallet classifier');

test('currency + coin names are wallet cargo', () => {
  for (const name of [
    '3 gold', 'Gold', 'silver', 'Copper Coins', 'gold pieces',
    'assorted coppers', 'Electrum', 'small silver',
    'Silver Coins', 'foreign currency',
  ]) {
    assert.ok(pouchFits(name), `"${name}" should be pouch cargo`);
  }
});

test('keys, ID papers, + small valuables are wallet cargo', () => {
  for (const name of [
    'Brass Key', 'keys', 'Rusted Keyring', 'ID Card', 'identity papers',
    'Passport', 'Tavern Permit', 'Deed to the Mill', 'Ruby', 'loose pearls',
    'Emerald Gemstone', 'Gold Ingot', 'Leather Wallet', 'Coin Purse',
  ]) {
    assert.ok(pouchFits(name), `"${name}" should be pouch cargo`);
  }
});

test('worn jewelry + household goods are vetoed by the guard list', () => {
  for (const name of [
    'Gold Ring', 'Pearl Necklace', 'Ruby Pendant', 'Silver Earrings',
    'Crown Jewels',            // the crown guard beats the jewels fit word
    'Silver Tray', 'Copper Pot', 'Golden Goblet', 'Jade Statue',
    'Healing Potion', 'Glass Vial', 'Iron Sword', 'Copper Wire',
    'Linen Shirt', 'Bedroll', 'Rations', 'Lockpick Set',
    'Change of Clothes',       // "change" the money ≠ a change of clothes
  ]) {
    assert.ok(!pouchFits(name), `"${name}" must NOT be pouch cargo`);
  }
});

test('bare metals read as money only in short names', () => {
  assert.ok(pouchFits('3 gold'), 'a qty + metal is money');
  assert.ok(pouchFits('silver'), 'a bare metal is money');
  assert.ok(!pouchFits('Silver-Trimmed Saddle'), 'long metal compounds are material descriptors');
  assert.ok(!pouchFits('copper fitting for the still'), '3+ words never hit the metal rule');
});

test('degenerate inputs never fit', () => {
  assert.ok(!pouchFits(''), 'empty name');
  assert.ok(!pouchFits(null), 'null name');
  assert.ok(!pouchFits(undefined), 'undefined name');
  assert.ok(!pouchFits('   '), 'whitespace-only name');
});

// (2026-08-24 review P2) Tokenizer parity with Rust `phrase_words`: accented
// letters are WORD characters, not split points — Rust's
// `char::is_alphanumeric()` keeps "keyé" one token (≠ "key", so it packs),
// and the JS twin must agree. The old ASCII-only split broke "keyé" into
// ["key"] — a fit word — so the panel pouched what the backend packed.
test('accented names tokenize like the Rust twin (Unicode word chars)', () => {
  // The review's exact divergence case: "keyé" is ONE token, not "key".
  assert.ok(!pouchFits('keyé'), '"keyé" is one Unicode token — not the fit word "key"');
  // A guard word still vetoes through accented neighbors.
  assert.ok(!pouchFits('Clé Ring'), 'guard word "ring" still vetoes');
  // And a genuine fit word in accented company still fits (both twins agree).
  assert.ok(pouchFits('Étui of tokens'), '"tokens" fits through accented context');
});

test('the vocab lists are the twin discipline (non-empty, lowercase, no dups)', () => {
  for (const set of [POUCH_FIT_WORDS, POUCH_GUARD_WORDS, POUCH_METAL_WORDS]) {
    assert.ok(set.size > 0, 'non-empty');
    for (const w of set) {
      assert.ok(w === w.toLowerCase(), `"${w}" is lowercase`);
      assert.ok(/^[a-z]+$/.test(w), `"${w}" is a bare word`);
    }
  }
  // A word in BOTH lists would make the guard silently eat a fit word.
  for (const w of POUCH_FIT_WORDS) {
    assert.ok(!POUCH_GUARD_WORDS.has(w), `"${w}" must not be both FIT and GUARD`);
  }
});

console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) process.exit(1);
