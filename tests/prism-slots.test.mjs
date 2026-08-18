// Unit tests for the pure Guided Slot Pipeline (PRISM composer, 2026-08-18).
// Plain Node ESM — no test runner. Run: `node tests/prism-slots.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
import { strict as assert } from 'node:assert';
import {
  SLOT_SETS, createSlots, slotsSatisfied, compilePrompt, splitIntoSlots,
} from '../src/prism/engine/slots.js';

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

// ── slotsSatisfied (the freeform search unlock + Generate gate) ──────────

test('slotsSatisfied: fresh state → false', () => {
  assert.equal(slotsSatisfied(createSlots()), false);
});

test('slotsSatisfied: subject alone → false (slot 2 mandatory)', () => {
  const s = createSlots();
  s.subject = '1girl';
  assert.equal(slotsSatisfied(s), false);
});

test('slotsSatisfied: framing alone (no subject) → false', () => {
  const s = createSlots();
  s.framing = 'full body';
  assert.equal(slotsSatisfied(s), false);
});

test('slotsSatisfied: subject + framing → true', () => {
  const s = createSlots();
  s.subject = '1girl';
  s.framing = 'cowboy shot';
  assert.equal(slotsSatisfied(s), true);
});

test('slotsSatisfied: subject + pose (no framing) → true', () => {
  const s = createSlots();
  s.subject = '1boy';
  s.pose = ['sitting'];
  assert.equal(slotsSatisfied(s), true);
});

test('slotsSatisfied: null/undefined state → false (never throws)', () => {
  assert.equal(slotsSatisfied(null), false);
  assert.equal(slotsSatisfied(undefined), false);
});

// ── compilePrompt (the expert ordering — the whole point) ────────────────

test('compilePrompt: empty state → empty string', () => {
  assert.equal(compilePrompt(createSlots()), '');
});

test('compilePrompt: ALWAYS slot-ordered regardless of pick order', () => {
  // Environment + freeform picked FIRST, subject last — the compiled
  // prompt still leads with the subject, then framing, pose, env, free.
  const s = createSlots();
  s.env = ['tavern', 'night'];
  s.free = ['sword', 'glowing eyes'];
  s.pose = ['sitting'];
  s.framing = 'cowboy shot';
  s.subject = '1girl';
  assert.equal(
    compilePrompt(s),
    '1girl, cowboy shot, sitting, tavern, night, sword, glowing eyes'
  );
});

test('compilePrompt: skips absent slots cleanly', () => {
  const s = createSlots();
  s.subject = 'no humans';
  s.env = ['dungeon'];
  assert.equal(compilePrompt(s), 'no humans, dungeon');
});

// ── splitIntoSlots (Send to Composer / legacy rows) ──────────────────────

test('splitIntoSlots: empty prompt → fresh state', () => {
  const s = splitIntoSlots('');
  assert.deepEqual(s, createSlots());
});

test('splitIntoSlots: round-trips compilePrompt output exactly', () => {
  const s = createSlots();
  s.subject = '2girls';
  s.framing = 'full body';
  s.pose = ['standing', 'fighting stance'];
  s.env = ['outdoors', 'forest'];
  s.free = ['magic', 'sunset', 'detailed background'];
  const out = splitIntoSlots(compilePrompt(s));
  assert.deepEqual(out, s);
});

test('splitIntoSlots: legacy interleaved row regroups into slots', () => {
  // A pre-slot-era row with tags in random order still lands grouped.
  const s = splitIntoSlots('dungeon, 1girl, sitting, torch light, full body');
  assert.equal(s.subject, '1girl');
  assert.equal(s.framing, 'full body');
  assert.deepEqual(s.pose, ['sitting']);
  assert.deepEqual(s.env, ['dungeon']);
  assert.deepEqual(s.free, ['torch light']);
});

test('splitIntoSlots: dedupes repeated slot tags', () => {
  const s = splitIntoSlots('1girl, sitting, sitting, tavern, tavern, extra');
  assert.deepEqual(s.pose, ['sitting']);
  assert.deepEqual(s.env, ['tavern']);
  assert.deepEqual(s.free, ['extra']);
});

test('splitIntoSlots: underscore spelling matches slot sets', () => {
  // Hand-edited payloads may carry the danbooru underscore form.
  const s = splitIntoSlots('1girl, cowboy_shot, no_humans_placeholder');
  assert.equal(s.subject, '1girl');
  assert.equal(s.framing, 'cowboy_shot');
});

test('splitIntoSlots: second subject tag falls to freeform (never clobbers)', () => {
  const s = splitIntoSlots('1girl, 1boy, tavern');
  assert.equal(s.subject, '1girl');
  assert.deepEqual(s.free, ['1boy']);
});

// ── The chip vocabulary sanity ────────────────────────────────────────────

test('SLOT_SETS: every quick-pick set is non-empty + duplicate-free', () => {
  for (const [slot, tags] of Object.entries(SLOT_SETS)) {
    assert.ok(Array.isArray(tags) && tags.length > 0, `${slot} non-empty`);
    assert.equal(new Set(tags.map((t) => t.toLowerCase())).size, tags.length, `${slot} no dupes`);
  }
});

test('SLOT_SETS: no tag appears in two slots (routing is unambiguous)', () => {
  const seen = new Set();
  for (const tags of Object.values(SLOT_SETS)) {
    for (const t of tags) {
      const key = t.toLowerCase();
      assert.ok(!seen.has(key), `tag "${t}" lives in two slots`);
      seen.add(key);
    }
  }
});

// ── Report ────────────────────────────────────────────────────────────────

console.log(`\nprism-slots: ${passed} passed, ${failed} failed`);
process.exit(failed > 0 ? 1 : 0);
