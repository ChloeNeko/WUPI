// Unit tests for the Progress Clocks' pure surfaces (2026-08-24 Part II C4).
// Plain Node ESM — no test runner. Run: `node tests/quest-clocks.test.mjs`.
// Exits non-zero on any failure. Mirrors tests/site-map.test.mjs style.
import { strict as assert } from 'node:assert';
import {
  clockFraction,
  clockFracVar,
  buildQuestClocksModel,
} from '../src/fable/engine/quest-clocks.js';

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

// ── clockFraction ────────────────────────────────────────────────────────
test('halfway through the window is 0.5, clamped at both ends', () => {
  assert.equal(clockFraction(100, 200, 150), 0.5);
  assert.equal(clockFraction(100, 200, 250), 1); // past the deadline: full
  assert.equal(clockFraction(100, 200, 50), 0); // before the start: empty
});

test('a missing deadline is 0 (no clock renders); a missing start reads full', () => {
  assert.equal(clockFraction(100, 0, 150), 0, 'no deadline → no window');
  assert.equal(clockFraction(0, 100, 50), 1, 'unstamped start → fully elapsed ring');
  assert.equal(clockFraction(100, 100, 150), 1, 'zero-length window → full');
});

test('junk input never NaNs', () => {
  assert.equal(clockFraction(null, 'x', {}), 0);
  assert.equal(clockFracVar(NaN), '0.000');
});

// ── clockFracVar (the CSS custom property string) ────────────────────────
test('fracVar serializes three-decimal clamped strings', () => {
  assert.equal(clockFracVar(0.5), '0.500');
  assert.equal(clockFracVar(1.7), '1.000');
  assert.equal(clockFracVar(-0.2), '0.000');
});

// ── buildQuestClocksModel ────────────────────────────────────────────────
const SCHEMA = {
  world_clock: { current_minutes: 1500 },
  quests: [
    {
      id: 'q1',
      giver: 'mara',
      title: 'Recover the ledger',
      reward: 'a warm bed',
      deadline_minutes: 2000,
      accepted_at_minutes: 1000,
      objectives: [
        { text: 'Find the vault', done: true, cur: 0, total: 0 },
        { text: 'Copy the pages', done: false, cur: 2, total: 5 },
      ],
    },
    {
      id: 'q2',
      giver: 'player',
      title: 'Learn the tunnels',
      deadline_minutes: 0,
      accepted_at_minutes: 900,
      objectives: [],
    },
  ],
  promises: [
    { npc_id: 'corin', description: 'Meet him at the docks', deadline_minutes: 1400, accepted_at_minutes: 1000 },
  ],
};

test('quests map with objective progress + counters', () => {
  const { rows } = buildQuestClocksModel(SCHEMA);
  assert.equal(rows.length, 3);
  const q1 = rows[0];
  assert.equal(q1.kind, 'quest');
  assert.equal(q1.done, 1);
  assert.equal(q1.objectiveCount, 2);
  assert.equal(q1.counter, '2/5');
  assert.equal(q1.frac, 0.5);
  assert.equal(q1.overdue, false);
});

test('overdue flags when the clock passes the deadline', () => {
  const { rows } = buildQuestClocksModel(SCHEMA);
  const promise = rows[2];
  assert.equal(promise.kind, 'promise');
  assert.equal(promise.overdue, true, 'deadline 1400 < now 1500');
  assert.equal(promise.frac, 1);
});

test('deadline-less quests carry no clock (deadline 0, frac 0)', () => {
  const { rows } = buildQuestClocksModel(SCHEMA);
  const q2 = rows[1];
  assert.equal(q2.deadline, 0);
  assert.equal(q2.frac, 0);
  assert.equal(q2.overdue, false);
});

test('missing quests/promises keys degrade to empty rows', () => {
  assert.deepEqual(buildQuestClocksModel({}).rows, []);
  assert.deepEqual(buildQuestClocksModel(null).rows, []);
});

console.log(failed === 0 ? `\nAll ${passed} quest-clock tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
