// Unit tests for the pure drawer decision logic (Stage 1 frontend).
// Plain Node ESM — no test runner. Run: `node tests/drawer-logic.test.mjs`.
// Exits non-zero on any failure so it can gate CI / the Stage 3 go-ahead.
import { strict as assert } from 'node:assert';
import {
  variantCount,
  computeDrawerState,
  swipeNextAction,
  canEditMessage,
  centeredPopupOpen,
  isTrailingAssistantBeat,
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
test('AI 3 variants, active 2, NOT trailing: ‹ › both disabled (swipe lock)', () => {
  const s = computeDrawerState({ role: 'assistant', count: 3, active: 2, isLastAssistant: false });
  assert.equal(s.canPrev, false);
  assert.equal(s.canNext, false);
});
test('AI 3 variants, active 1, NOT trailing: mid-variants ‹ also disabled (backend locks swipes)', () => {
  // The swipe-lock contract: a non-trailing beat may never swipe, even with
  // earlier variants to step back to — the backend refuses once turns start.
  const s = computeDrawerState({ role: 'assistant', count: 3, active: 1, isLastAssistant: false });
  assert.equal(s.canPrev, false);
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

// ── canEditMessage (P2b 2026-08-17: the ✎ mirrors the edit_message contract) ─
test('canEdit: any USER beat is editable (mid-history included)', () => {
  assert.equal(canEditMessage({ role: 'user', isLastAssistant: false }), true);
  assert.equal(canEditMessage({ role: 'user', isLastAssistant: true }), true);
});
test('canEdit: the TRAILING assistant beat is editable', () => {
  assert.equal(canEditMessage({ role: 'assistant', isLastAssistant: true }), true);
});
test('canEdit: a mid-history assistant beat is NOT editable (backend refuses)', () => {
  // The playtest case: ✎ on beat 44 → edit_message refused "not the trailing
  // assistant message" → the beat rendered blank until a rebuild.
  assert.equal(canEditMessage({ role: 'assistant', isLastAssistant: false }), false);
});
test('canEdit: isLastAssistant is REQUIRED for assistant beats (no undefined ride)', () => {
  assert.equal(canEditMessage({ role: 'assistant', isLastAssistant: undefined }), false);
});

// ── centeredPopupOpen (the stage keyboard gate over the centered popups) ────
// A stub root whose querySelector matches exactly one selector string — the
// predicate is a pure read, so the selector list is the contract under test.
function rootMatching(sel) {
  return { querySelector: (q) => (q === sel ? { matched: true } : null) };
}
test('centeredPopupOpen: false when the stage root has no popup mounted', () => {
  assert.equal(centeredPopupOpen({ querySelector: () => null }), false);
});
test('centeredPopupOpen: false on a null/shapeless root (never blocks on junk)', () => {
  assert.equal(centeredPopupOpen(null), false);
  assert.equal(centeredPopupOpen(undefined), false);
  assert.equal(centeredPopupOpen({}), false);
});
test('centeredPopupOpen: each of the four centered popups trips the gate', () => {
  assert.equal(centeredPopupOpen(rootMatching('.fable-saves-popup-overlay, .fable-sessions-popup-overlay, [data-chronicle-overlay], [data-npc-dossier]')), true);
});
test('centeredPopupOpen: unrelated selectors do not trip the gate', () => {
  assert.equal(centeredPopupOpen(rootMatching('.fable-some-other-overlay')), false);
});

// ── isTrailingAssistantBeat (the DOM-side trailing derivation's pure core) ──
// The swipe-lock contract: a beat is swipeable/editable only when it is the
// feed's TRAILING MESSAGE — the last role-bearing (user/assistant) beat AND
// an assistant. The old last-ASSISTANT-only check stayed enabled when a user
// beat followed (the api_lost composer-restore / rewind shape) → a
// guaranteed backend "not the trailing beat" error.
const roleBeat = (role) => ({ dataset: { role } });
test('trailing: the last role beat when it is an assistant IS trailing', () => {
  const a = roleBeat('assistant');
  const feed = [roleBeat('user'), a];
  assert.equal(isTrailingAssistantBeat(a, feed), true);
});
test('trailing: a user beat AFTER the assistant retires it (not the trailing message)', () => {
  const a = roleBeat('assistant');
  const feed = [a, roleBeat('user')];
  assert.equal(isTrailingAssistantBeat(a, feed), false, 'api_lost restore / rewind shape');
});
test('trailing: a mid-feed assistant with later assistants is not trailing', () => {
  const a0 = roleBeat('assistant');
  const feed = [a0, roleBeat('assistant'), roleBeat('user')];
  assert.equal(isTrailingAssistantBeat(a0, feed), false);
});
test('trailing: a trailing USER beat is never the trailing assistant', () => {
  const u = roleBeat('user');
  assert.equal(isTrailingAssistantBeat(u, [roleBeat('assistant'), u]), false);
});
test('trailing: empty / junk feeds + beatless calls degrade to false', () => {
  assert.equal(isTrailingAssistantBeat(roleBeat('assistant'), []), false);
  assert.equal(isTrailingAssistantBeat(roleBeat('assistant'), null), false);
  assert.equal(isTrailingAssistantBeat(null, [roleBeat('assistant')]), false);
  assert.equal(isTrailingAssistantBeat({}, [{}]), false, 'a beat with no dataset never trips it');
});

console.log('\n%d passed, %d failed', passed, failed);
process.exit(failed ? 1 : 0);
