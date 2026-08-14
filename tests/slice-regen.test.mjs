// Unit tests for the slice-regen eligibility predicate (golden pencil,
// 2026-08-11). Plain Node ESM — no test runner. Run:
//   `node tests/slice-regen.test.mjs`
// Exits non-zero on any failure. Mirrors tests/drawer-logic.test.mjs style.
//
// The DOM-dependent pieces (Selection/Range math, pencil positioning) are
// browser-only + exercised manually; this file pins the DOM-free eligibility
// gate that decides whether a resolved selection should show the pencil.
import { strict as assert } from 'node:assert';
import { isSliceEligible } from '../src/fable/engine/slice-regen.js';

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

// ── isSliceEligible (the gate) ─────────────────────────────────────────────
test('assistant beat, clean selection → eligible', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), true);
});

test('user beat → never eligible (AI messages only)', () => {
  assert.equal(isSliceEligible({
    role: 'user',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('editing beat → not eligible (dblclick-to-edit owns it)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: true,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('streaming beat → not eligible (still generating)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: true,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('already-slice-regenerating beat → not eligible (no nested slices)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: true,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('collapsed selection → not eligible (nothing highlighted)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: true,
    emptyText: false,
  }), false);
});

test('whitespace-only selection → not eligible (empty after trim)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: true,
  }), false);
});

test('multiple guards fail at once → still not eligible', () => {
  assert.equal(isSliceEligible({
    role: 'user',
    editing: true,
    streaming: true,
    sliceRegenerating: true,
    collapsed: true,
    emptyText: true,
  }), false);
});

console.log('\n%d passed, %d failed', passed, failed);
process.exit(failed ? 1 : 0);
