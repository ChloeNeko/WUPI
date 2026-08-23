// Unit tests for the Ghost Writer's pure surfaces (2026-08-22).
// Plain Node ESM — no test runner. Run: `node tests/ghost-writer.test.mjs`.
// Exits non-zero on any failure. Mirrors tests/drawer-logic.test.mjs style.
//
// The DOM pieces (menu open/close, notice chip, the impersonate invoke
// loop) are browser-only + exercised manually; this file pins the menu
// model + the empty-prompt contract (Swipe and Continue refuse an empty
// composer, Impersonate does not).
import { strict as assert } from 'node:assert';
import {
  GHOST_MODES,
  ghostModeRequiresPrompt,
  EMPTY_PROMPT_WARNING,
} from '../src/fable/engine/ghost-writer.js';

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

// ── the menu model ──────────────────────────────────────────────────────
test('menu offers exactly Swipe / Continue / Impersonate in order', () => {
  assert.deepEqual(GHOST_MODES.map((m) => m.label), ['Swipe', 'Continue', 'Impersonate']);
});

test('Swipe and Continue require a typed prompt; Impersonate does not', () => {
  assert.equal(ghostModeRequiresPrompt('swipe'), true);
  assert.equal(ghostModeRequiresPrompt('continue'), true);
  assert.equal(ghostModeRequiresPrompt('impersonate'), false);
});

test('unknown mode ids do not require a prompt (safe default)', () => {
  assert.equal(ghostModeRequiresPrompt('nope'), false);
  assert.equal(ghostModeRequiresPrompt(''), false);
});

test('the empty-prompt warning is the exact spec copy', () => {
  assert.equal(
    EMPTY_PROMPT_WARNING,
    'Please type a prompt for the Narrator to follow.'
  );
  assert.ok(!EMPTY_PROMPT_WARNING.includes('—'), 'no em dash in player-facing copy');
});

console.log(failed === 0 ? `\nAll ${passed} ghost-writer tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
