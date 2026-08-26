// Unit tests for the Chronicle panel's pure surfaces (2026-08-24 Part II C3).
// Plain Node ESM — no test runner. Run: `node tests/chronicle.test.mjs`.
// Exits non-zero on any failure. The DOM pieces (mountChronicle's fetch +
// pin/rollback wiring, the overlays) are browser-only + exercised manually;
// this file pins the view model over memory_turns_list rows.
import { strict as assert } from 'node:assert';
import { buildChronicleModel } from '../src/fable/panels/chronicle.js';

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

const ROWS = [
  { turn_uuid: 't3', snippet: 'Mara slides the tankard across...', timestamp: 1724400000, pinned: true, chunks: 2 },
  { turn_uuid: 't2', snippet: 'The cellar door groans open.', timestamp: 1724313600, pinned: false, chunks: 4 },
  { turn_uuid: 't1', snippet: '', timestamp: 1724227200, pinned: false, chunks: 1 },
];

test('rows keep their newest-first order', () => {
  const m = buildChronicleModel(ROWS);
  assert.equal(m.turns.length, 3);
  assert.equal(m.turns[0].turn_uuid, 't3');
  assert.equal(m.turns[1].turn_uuid, 't2');
  assert.equal(m.turns[2].turn_uuid, 't1');
});

test('pins + part counts carry; empty snippets degrade to a placeholder', () => {
  const m = buildChronicleModel(ROWS);
  assert.equal(m.turns[0].pinned, true);
  assert.equal(m.turns[0].chunks, 2);
  assert.equal(m.turns[2].snippet, '(no text)');
});

test('the header names the saved memories; empty input stays honest', () => {
  assert.equal(buildChronicleModel(ROWS).header, 'Saved memories for this story');
  const empty = buildChronicleModel([]);
  assert.equal(empty.header, 'No saved memories yet');
  assert.deepEqual(empty.turns, []);
  assert.deepEqual(buildChronicleModel(null).turns, []);
  assert.deepEqual(buildChronicleModel('junk').turns, []);
});

console.log(failed === 0 ? `\nAll ${passed} chronicle tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
