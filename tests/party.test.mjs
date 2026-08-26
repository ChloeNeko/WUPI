// Unit tests for the PARTY panel's pure surfaces (2026-08-24 fix).
// Plain Node ESM — no test runner. Run: `node tests/party.test.mjs`.
// Pins collectCast (both entity-key conventions → one card per NPC) and
// the rendered HTML's data-npc surfaces — the cast-card → dossier chain
// the npc-dossier resolver consumes. The click wiring (wirePartyCards)
// is browser-only + exercised manually.
import { strict as assert } from 'node:assert';
import { collectCast, renderParty } from '../src/fable/panels/party.js';

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

test('collectCast: flat npc_ keys pass through with stripped surfaces', () => {
  const cast = collectCast({ npc_marcus: 'wary', npc_lia: 'trusted ally' });
  assert.deepEqual(cast, [
    { surface: 'marcus', state: 'wary' },
    { surface: 'lia', state: 'trusted ally' },
  ]);
});

test('collectCast: dotted npc.<id>.<field> keys group to one card per NPC', () => {
  const cast = collectCast({
    'npc.mara_the_innkeep.tier': 'Friendly',
    'npc.mara_the_innkeep.mood': 'warming',
    'npc.corin_bard.note': 'owes you a song',
    unrelated: 'ignored',
    'npc.': 'malformed — no id',
  });
  assert.equal(cast.length, 2, 'one card per NPC id');
  const mara = cast.find((c) => c.surface === 'mara_the_innkeep');
  assert.equal(mara.state, 'Friendly', 'the tier field wins the state label');
  const corin = cast.find((c) => c.surface === 'corin_bard');
  assert.equal(corin.state, 'owes you a song');
});

test('collectCast: empty + junk input', () => {
  assert.deepEqual(collectCast({}), []);
  assert.deepEqual(collectCast(null), []);
  assert.deepEqual(collectCast({ 'player.name': 'Alex' }), []);
});

test('renderParty: dotted-convention sessions render cards (not the empty panel)', () => {
  const html = renderParty(
    { 'npc.mara_the_innkeep.tier': 'Friendly', 'npc.corin_bard.tier': 'Rival' },
    {}
  );
  assert.ok(html.includes('party-grid'), 'cards render');
  assert.ok(!html.includes('panel-empty'), 'not the empty state');
  assert.ok(html.includes('data-npc="mara_the_innkeep"'), 'bare surface on the card');
  assert.ok(html.includes('data-npc="corin_bard"'));
});

test('renderParty: flat keys keep the stripped data-npc surface', () => {
  const html = renderParty({ npc_marcus: 'wary' }, {});
  assert.ok(html.includes('data-npc="marcus"'));
  assert.ok(html.includes('Marcus'));
});

test('renderParty: no cast entities renders the empty panel', () => {
  const html = renderParty({ 'world.fact': 'the mire is poisonous' }, {});
  assert.ok(html.includes('panel-empty'));
});

if (failed > 0) {
  console.error('\nparty: %d failed, %d passed', failed, passed);
  process.exit(1);
}
console.log('\nparty: all %d passed', passed);
