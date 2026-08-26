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
  parseDeclaredDc,
  pinMatchesSentText,
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

// ── parseDeclaredDc (the B3 committed-DC declaration parse) ─────────────
test('an em-dash bracketed DC declaration parses to {skill, dc}', () => {
  assert.deepEqual(parseDeclaredDc('Slip the wards before dawn — [Lockpicking DC 18]'), {
    skill: 'Lockpicking',
    dc: 18,
  });
  assert.deepEqual(parseDeclaredDc("Talk the quartermaster down — [Persuasion DC 12]."), {
    skill: 'Persuasion',
    dc: 12,
  });
});

test('hyphen-dash, spacing, and multi-word skill names are tolerated', () => {
  assert.deepEqual(parseDeclaredDc('Climb the wall - [Athletics DC 15]'), {
    skill: 'Athletics',
    dc: 15,
  });
  assert.deepEqual(parseDeclaredDc('— [Sleight of Hand DC 20]'), {
    skill: 'Sleight of Hand',
    dc: 20,
  });
});

test('no declaration, plain brackets, or out-of-range DC yield null', () => {
  assert.equal(parseDeclaredDc('Just walk through the open door.'), null);
  assert.equal(parseDeclaredDc('[the guard is alert]'), null);
  assert.equal(parseDeclaredDc('— [Lockpicking DC 99]'), null);
  assert.equal(parseDeclaredDc('— [Lockpicking DC 0]'), null);
  assert.equal(parseDeclaredDc(null), null);
});

// ── pinMatchesSentText (the B3 timing fix's send gate) ───────────────────
// The declared DC is stashed when an expand LANDS and committed only when
// the fork is actually SENT — the sent text must still carry the expanded
// fork's opening line, so an abandoned expand (never armed) and a replaced
// composer text (no lineage match) can never poison the next skill roll.
test('a sent fork (verbatim or edited tail) matches its expanded text', () => {
  const fork = 'Slip past the sleeping wardens.\nAnd vanish into the cellar dark.';
  assert.equal(pinMatchesSentText(fork, fork), true, 'verbatim send');
  assert.equal(
    pinMatchesSentText(`${fork}\n(added flourish)`, fork),
    true,
    'kept the fork, appended a tail',
  );
  assert.equal(
    pinMatchesSentText('Sure — first: slip past the sleeping wardens.', fork),
    true,
    'the opening line embedded in a rewritten send',
  );
});
test('a replaced composer text or empty send never inherits the pin', () => {
  const fork = 'Slip past the sleeping wardens.\nAnd vanish into the cellar dark.';
  assert.equal(pinMatchesSentText('I persuade the quartermaster instead.', fork), false);
  assert.equal(pinMatchesSentText('', fork), false);
  assert.equal(pinMatchesSentText('   ', fork), false);
});
test('shapeless inputs degrade to no-match (never commit on junk)', () => {
  assert.equal(pinMatchesSentText('anything', ''), false);
  assert.equal(pinMatchesSentText('anything', null), false);
  assert.equal(pinMatchesSentText(null, 'A fork line.'), false);
  assert.equal(pinMatchesSentText(undefined, undefined), false);
});

console.log(failed === 0 ? `\nAll ${passed} crossroads tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
