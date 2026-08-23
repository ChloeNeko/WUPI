// Unit tests for the Crossroads' pure surfaces (2026-08-22).
// Plain Node ESM — no test runner. Run: `node tests/crossroads.test.mjs`.
// Exits non-zero on any failure. Mirrors tests/drawer-logic.test.mjs style.
//
// The DOM pieces (category menu, deck overlay, the draw/expand invokes)
// are browser-only + exercised manually; this file pins the category set
// (the five decks, wire ids intact) + the defensive option shaping the UI
// applies before rendering a draw.
import { strict as assert } from 'node:assert';
import {
  CROSSROADS_CATEGORIES,
  categoryLabel,
  sanitizeOptions,
} from '../src/fable/engine/crossroads.js';

let passed = 0;
let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log('  ok   %s', name);
    passed++;
  } catch (e) {
    console.error('  FAIL %s\n       %s', name, e.message);
    failed++;
  }
}

// ── the five decks ──────────────────────────────────────────────────────
test('the deck menu offers Player / World / NPC / Plot / Explicit in order', () => {
  assert.deepEqual(
    CROSSROADS_CATEGORIES.map((c) => c.label),
    ['Player', 'World', 'NPC', 'Plot', 'Explicit']
  );
});

test('wire ids are the lowercase Rust category keys', () => {
  assert.deepEqual(
    CROSSROADS_CATEGORIES.map((c) => c.id),
    ['player', 'world', 'npc', 'plot', 'explicit']
  );
});

test('categoryLabel maps ids to labels and unknowns to empty', () => {
  assert.equal(categoryLabel('npc'), 'NPC');
  assert.equal(categoryLabel('explicit'), 'Explicit');
  assert.equal(categoryLabel('bogus'), '');
});

// ── sanitizeOptions (the UI trust boundary over the parsed payload) ─────
test('a clean payload passes through field-for-field', () => {
  const out = sanitizeOptions([
    { emoji: '🗡️', title: 'Draw Steel', summary: 'The player draws their blade.' },
  ]);
  assert.equal(out.length, 1);
  assert.equal(out[0].title, 'Draw Steel');
});

test('non-array payloads yield an empty deck', () => {
  assert.deepEqual(sanitizeOptions(null), []);
  assert.deepEqual(sanitizeOptions(undefined), []);
  assert.deepEqual(sanitizeOptions({ options: [] }), []);
});

test('entries with a missing or blank field are dropped, survivors kept', () => {
  const out = sanitizeOptions([
    { emoji: '🔥', title: 'Torch It', summary: 'Burn the warehouse down.' },
    { emoji: '', title: 'Ghost', summary: 'No emoji.' },
    { emoji: '❄️', title: '   ', summary: 'Blank title.' },
    { emoji: '🌊', title: 'Flood', summary: '' },
    'not an object',
  ]);
  assert.equal(out.length, 1);
  assert.equal(out[0].title, 'Torch It');
});

test('clamps are codepoint-aware (an emoji is never split mid-surrogate)', () => {
  const emoji = '🗡️'; // sword + VS16: two codepoints, would split under a raw slice
  const out = sanitizeOptions([{ emoji: `${emoji}${emoji}${emoji}`, title: 'T', summary: 'S.' }]);
  assert.equal(out[0].emoji, `${emoji}${emoji}${emoji}`); // under the cap, intact
});

test('oversize strings are clamped and the deck is capped at six', () => {
  const many = Array.from({ length: 9 }, (_, i) => ({
    emoji: '✨',
    title: `T${i}`,
    summary: 'S.',
  }));
  const out = sanitizeOptions(many);
  assert.equal(out.length, 6);
  const long = sanitizeOptions([
    { emoji: '✨', title: 'x'.repeat(200), summary: 's'.repeat(900) },
  ]);
  assert.ok(long[0].title.length <= 60);
  assert.ok(long[0].summary.length <= 500);
});

console.log(failed === 0 ? `\nAll ${passed} crossroads tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
