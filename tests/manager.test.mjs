// Unit tests for the manager panel's pure focus router (classifyFocus —
// exported for these tests 2026-08-25). Plain Node ESM — no test runner.
// Run: `node tests/manager.test.mjs`. Exits non-zero on any failure.
// Pins the chronicle route's word-boundary fix: the old bare-alternatives
// regex matched "turns" inside "returns"/"overturns" and summoned the
// chronicle panel off unrelated foci.
import { strict as assert } from 'node:assert';
import { classifyFocus } from '../src/fable/panels/manager.js';

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

test('memory foci route to the chronicle', () => {
  assert.equal(classifyFocus('chronicle', {}), 'chronicle');
  assert.equal(classifyFocus('chronicles', {}), 'chronicle');
  assert.equal(classifyFocus('my memories', {}), 'chronicle');
  assert.equal(classifyFocus('memory', {}), 'chronicle');
  assert.equal(classifyFocus('recent turns', {}), 'chronicle');
  assert.equal(classifyFocus('rollback a turn', {}), 'chronicle');
});

test('substring hits do NOT route to the chronicle (word boundaries)', () => {
  assert.notEqual(classifyFocus('returns', {}), 'chronicle');
  assert.notEqual(classifyFocus('overturns', {}), 'chronicle');
  assert.notEqual(classifyFocus('turnstone', {}), 'chronicle');
});

test('sibling routes keep their keywords', () => {
  assert.equal(classifyFocus('where am I', {}), 'map');
  assert.equal(classifyFocus('my skills', {}), 'skills');
  assert.equal(classifyFocus('the party', {}), 'party');
  assert.equal(classifyFocus('craft', {}), 'craft');
  assert.equal(classifyFocus('world lore', {}), 'codex');
});

test('entity-prefix fallbacks + the codex default', () => {
  assert.equal(classifyFocus('', { npc_mara: 'present' }), 'party');
  assert.equal(classifyFocus('', { loc_tavern: 'known' }), 'map');
  assert.equal(classifyFocus('nothing special', {}), 'codex');
});

console.log(failed === 0 ? `\nAll ${passed} manager tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
