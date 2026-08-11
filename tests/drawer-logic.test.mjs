// Unit tests for the pure drawer decision logic (Stage 1 frontend).
// Plain Node ESM — no test runner. Run: `node tests/drawer-logic.test.mjs`.
// Exits non-zero on any failure so it can gate CI / the Stage 3 go-ahead.
import { strict as assert } from 'node:assert';
import {
  variantCount,
  computeDrawerState,
  swipeNextAction,
} from '../src/fable/engine/drawer-logic.js';

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

// ── variantCount (the off-by-one fix: was variants.length + 1) ──────────────
test('variantCount: empty array → 1 (implicit single variant)', () => {
  assert.equal(variantCount([]), 1);
});
test('variantCount: non-array → 1', () => {
  assert.equal(variantCount(null), 1);
  assert.equal(variantCount(undefined), 1);
});
test('variantCount: [a] → 1 (single, after normalize)', () => {
  assert.equal(variantCount(['a']), 1);
});
test('variantCount: [a,b] → 2 (after one reroll)', () => {
  assert.equal(variantCount(['a', 'b']), 2);
});
test('variantCount: [a,b,c] → 3 (after two rerolls)', () => {
  assert.equal(variantCount(['a', 'b', 'c']), 3);
});

// ── computeDrawerState — assistant beats ───────────────────────────────────
test('AI single variant, trailing: ‹ disabled, › enabled (reroll)', () => {
  const s = computeDrawerState({ role: 'assistant', count: 1, active: 0, isLastAssistant: true });
  assert.equal(s.canPrev, false);
  assert.equal(s.canNext, true);
  assert.equal(s.nextLabel, 'Regenerate');
});
test('AI single variant, mid-history: ‹ › both disabled (no reroll)', () => {
  const s = computeDrawerState({ role: 'assistant', count: 1, active: 0, isLastAssistant: false });
  assert.equal(s.canPrev, false);
  assert.equal(s.canNext, false);
});
test('AI 3 variants, active 1 (middle): both enabled, ›=Next', () => {
  const s = computeDrawerState({ role: 'assistant', count: 3, active: 1, isLastAssistant: true });
  assert.equal(s.canPrev, true);
  assert.equal(s.canNext, true);
  assert.equal(s.nextLabel, 'Next variant');
});
test('AI 3 variants, active 2, trailing: › enabled (= reroll)', () => {
  const s = computeDrawerState({ role: 'assistant', count: 3, active: 2, isLastAssistant: true });
  assert.equal(s.canPrev, true);
  assert.equal(s.canNext, true);
  assert.equal(s.nextLabel, 'Regenerate');
});
test('AI 3 variants, active 2, NOT trailing: › disabled (mid-history, no reroll)', () => {
  const s = computeDrawerState({ role: 'assistant', count: 3, active: 2, isLastAssistant: false });
  assert.equal(s.canPrev, true);
  assert.equal(s.canNext, false);
});
test('AI at active 0 with >1 variants: ‹ disabled, › enabled (=Next)', () => {
  const s = computeDrawerState({ role: 'assistant', count: 3, active: 0, isLastAssistant: true });
  assert.equal(s.canPrev, false);
  assert.equal(s.canNext, true);
  assert.equal(s.nextLabel, 'Next variant');
});

// ── computeDrawerState — user beats (never reroll, always 1/1) ─────────────
test('User single: ‹ › disabled (no variant nav on player text)', () => {
  const s = computeDrawerState({ role: 'user', count: 1, active: 0, isLastAssistant: false });
  assert.equal(s.canPrev, false);
  assert.equal(s.canNext, false);
});
test('User beat ignores isLastAssistant (never rerolls even if flagged)', () => {
  const s = computeDrawerState({ role: 'user', count: 1, active: 0, isLastAssistant: true });
  assert.equal(s.canNext, false);
});

// ── swipeNextAction (the ›-folds-into-reroll decision) ─────────────────────
test('swipeNext: middle variant → swipe to active+1', () => {
  assert.deepEqual(swipeNextAction({ count: 3, active: 1 }), { kind: 'swipe', variantIdx: 2 });
});
test('swipeNext: at last of 2 → reroll', () => {
  assert.deepEqual(swipeNextAction({ count: 2, active: 1 }), { kind: 'reroll' });
});
test('swipeNext: single variant at end → reroll (the fold)', () => {
  assert.deepEqual(swipeNextAction({ count: 1, active: 0 }), { kind: 'reroll' });
});
test('swipeNext: active 0 of 3 → swipe to 1', () => {
  assert.deepEqual(swipeNextAction({ count: 3, active: 0 }), { kind: 'swipe', variantIdx: 1 });
});

console.log('\n%d passed, %d failed', passed, failed);
process.exit(failed ? 1 : 0);
