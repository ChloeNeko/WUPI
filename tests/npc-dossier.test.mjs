// Unit tests for the NPC dossier's pure surfaces (2026-08-24 Part II C1).
// Plain Node ESM — no test runner. Run: `node tests/npc-dossier.test.mjs`.
// Exits non-zero on any failure. The DOM popup (openNpcDossier) is
// browser-only + exercised manually; this file pins the registry resolver
// and the dossier model over a fixture schema (the fable_schema_get shape).
import { strict as assert } from 'node:assert';
import {
  resolveRegistryEntry,
  buildNpcDossierModel,
} from '../src/fable/engine/npc-dossier.js';

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

const REGISTRY = [
  { id: 'mara_the_innkeep', name: 'Mara', role: 'The innkeeper', prominence: 'core', aliases: ['mara', 'innkeep'] },
  { id: 'corin_bard', name: 'Corin', role: 'A wandering bard', prominence: 'named', aliases: [] },
  { id: 'maro_cook', name: 'Maro', role: '', prominence: 'named', aliases: [] },
];

// ── resolveRegistryEntry ─────────────────────────────────────────────────
test('resolution: exact id, alias, name, then unique fragment', () => {
  assert.equal(resolveRegistryEntry(REGISTRY, 'mara_the_innkeep').id, 'mara_the_innkeep');
  assert.equal(resolveRegistryEntry(REGISTRY, 'Mara').id, 'mara_the_innkeep');
  assert.equal(resolveRegistryEntry(REGISTRY, 'innkeep').id, 'mara_the_innkeep');
  assert.equal(resolveRegistryEntry(REGISTRY, 'corin').id, 'corin_bard');
});

test('resolution: ambiguous fragments and junk yield null', () => {
  assert.equal(resolveRegistryEntry(REGISTRY, 'mar'), null, 'mara + maro both match');
  assert.equal(resolveRegistryEntry(REGISTRY, ''), null);
  assert.equal(resolveRegistryEntry(REGISTRY, 'nobody'), null);
  assert.equal(resolveRegistryEntry(null, 'mara'), null);
});

// (2026-08-24 fix) The party panel passes raw ENTITY KEYS — both key
// conventions must resolve. The old resolver's fragment check ran
// backwards against prefixed surfaces (the surface was LONGER than the
// id), leaving the dossier unreachable from every cast card.
test('resolution: entity-key surfaces (npc_ flat + npc. dotted) resolve', () => {
  assert.equal(resolveRegistryEntry(REGISTRY, 'npc_mara_the_innkeep').id, 'mara_the_innkeep');
  assert.equal(resolveRegistryEntry(REGISTRY, 'npc.mara_the_innkeep.tier').id, 'mara_the_innkeep');
  assert.equal(resolveRegistryEntry(REGISTRY, 'npc.mara_the_innkeep').id, 'mara_the_innkeep');
  assert.equal(resolveRegistryEntry(REGISTRY, 'npc_corin_bard').id, 'corin_bard');
  // An id that legitimately starts with the prefix still exact-hits.
  const odd = [{ id: 'npc_jester', name: 'Jester', aliases: [] }];
  assert.equal(resolveRegistryEntry(odd, 'npc_jester').id, 'npc_jester');
  // Prefixed AMBIGUITY still refuses (mar matches mara + maro).
  assert.equal(resolveRegistryEntry(REGISTRY, 'npc.mar'), null);
});

// ── buildNpcDossierModel ────────────────────────────────────────────────
const SCHEMA = {
  world_clock: { current_minutes: 3000 },
  npc_registry: { entries: REGISTRY },
  relationships: {
    mara_the_innkeep: { tier: 'friendly', events: ['shared_drink', 'saved_life', 'custom_rite'], volatility: 1 },
  },
  offscreen_tasks: [
    { npc_id: 'mara_the_innkeep', description: 'scout the bandit camp', difficulty: 'hard', resolves_at_minutes: 2000, resolved: true },
    { npc_id: 'mara_the_innkeep', description: 'buy lamp oil', difficulty: 'easy', resolves_at_minutes: 3600, resolved: false },
    { npc_id: 'corin_bard', description: 'not hers', difficulty: 'easy', resolves_at_minutes: 100, resolved: false },
  ],
  npc_interior: {
    mara_the_innkeep: {
      mood: 'warming',
      intent: 'keep the player from the cellar',
      items: [{ name: 'Brass Key', qty: 1 }, { name: 'Copper', qty: 12 }],
      worn: [{ name: 'Wool Apron', qty: 1 }],
      interactions: 9,
      last_seen_minutes: 1560,
    },
  },
  presences: [],
};

test('dossier: identity + prettified fallback name', () => {
  const m = buildNpcDossierModel(SCHEMA, 'innkeep');
  assert.equal(m.id, 'mara_the_innkeep');
  assert.equal(m.name, 'Mara');
  assert.equal(m.role, 'The innkeeper');
  assert.deepEqual(m.aliases, ['mara', 'innkeep']);
  assert.equal(m.prominence, 'core');
});

test('dossier: the bond tier + milestone points (unknown events keep their seat)', () => {
  const m = buildNpcDossierModel(SCHEMA, 'mara');
  assert.equal(m.bond.tier, 'friendly');
  const shared = m.bond.events.find((e) => e.id === 'shared_drink');
  assert.equal(shared.points, 1);
  const saved = m.bond.events.find((e) => e.id === 'saved_life');
  assert.equal(saved.points, 3);
  const custom = m.bond.events.find((e) => e.id === 'custom_rite');
  assert.equal(custom.points, null, 'a codex-authored milestone renders unpointed');
});

test('dossier: tasks scoped to THIS npc with due/resolved flags', () => {
  const m = buildNpcDossierModel(SCHEMA, 'mara');
  assert.equal(m.tasks.length, 2, "corin's task never crosses");
  assert.equal(m.tasks[0].resolved, true);
  assert.equal(m.tasks[1].due, false, 'eta 3600 > now 3000');
});

test('dossier: interior carries/wears/mood + last-seen duration', () => {
  const m = buildNpcDossierModel(SCHEMA, 'mara');
  assert.equal(m.interior.mood, 'warming');
  assert.deepEqual(m.interior.carries, ['Brass Key', 'Copper ×12']);
  assert.deepEqual(m.interior.wears, ['Wool Apron']);
  // now 3000 − last_seen 1560 = 1440 = exactly one day.
  assert.equal(m.lastSeen, '1 day ago');
});

test('dossier: presence beats last-seen; unknown npc yields null', () => {
  const present = buildNpcDossierModel(
    { ...SCHEMA, presences: [{ npc_id: 'mara_the_innkeep', name: 'Mara', stance: 'polishing a tankard', ttl: 4 }] },
    'mara'
  );
  assert.equal(present.present.here, true);
  assert.equal(present.lastSeen, 'on camera now');
  assert.equal(buildNpcDossierModel(SCHEMA, 'nobody'), null);
  assert.equal(buildNpcDossierModel(null, 'mara'), null);
});

console.log(failed === 0 ? `\nAll ${passed} npc-dossier tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
