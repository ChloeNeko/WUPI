// Unit tests for the ID-card model (engine/creator-engine.js::buildIdCard).
// Plain Node ESM — no test runner. Run: `node tests/id-card.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
import { strict as assert } from 'node:assert';
import { buildIdCard } from '../src/fable/engine/creator-engine.js';

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

const byTitle = (model) => Object.fromEntries(model.extra);

// ── player ─────────────────────────────────────────────────────────────────
test('buildIdCard: player core = the 8 license fields, in order, no extras in core', () => {
  const m = buildIdCard('player', {
    name: 'Kael', gender: 'male', race: 'human', age: '28',
    hair_color: 'black', eye_color: 'green', height: "6'1\"", weight: '180 lb',
    // extra-only fields:
    hair_length: 'short', hair_style: 'messy', body_type: 'lean',
    skin_complexion: 'tan', clothing: ['tunic', 'boots'], job: 'ranger',
    backstory: 'orphan', personality: 'stoic', wealth: '50 gold',
  });
  assert.equal(m.variant, 'player');
  assert.equal(m.banner, null);
  assert.equal(m.tag, null);
  assert.deepEqual(m.core, [
    ['Name', 'Kael'], ['Gender', 'male'], ['Race', 'human'], ['Age', '28'],
    ['Hair Color', 'black'], ['Eye Color', 'green'], ["Height", "6'1\""], ['Weight', '180 lb'],
  ]);
  // None of the 8 core labels leak into extra, and the extra-only fields land
  // in their groups.
  const e = byTitle(m);
  assert.deepEqual(e.Hair, [['Length', 'short'], ['Style', 'messy']]);
  assert.deepEqual(e.Physique, [['Body', 'lean'], ['Skin', 'tan']]);
  assert.deepEqual(e.Clothing, [['Outfit', 'tunic, boots']]);
  assert.deepEqual(e['Starting conditions'], [['Wealth', '50 gold']]);
});

test('buildIdCard: player drops empty core rows + empty extra sections', () => {
  const m = buildIdCard('player', { name: 'Nyx', gender: 'female', hair_color: 'red' });
  // Only the 3 present core fields survive; missing ones are dropped (not null).
  assert.deepEqual(m.core, [['Name', 'Nyx'], ['Gender', 'female'], ['Hair Color', 'red']]);
  // No Hair section (length/style both empty), no Clothing, etc. → extra is [].
  assert.deepEqual(m.extra, []);
});

test('buildIdCard: player surfaces distinctive, inventory, accessories, custom tags', () => {
  const m = buildIdCard('player', {
    name: 'Nyx', race: 'tiefling', horn: 'curled', ears: 'pointed',
    gear: ['compass', 'rope'], weapons: ['dagger'], accessories: 'silver locket',
    custom_tags: { curse: 'moon-bound', faction: 'thieves guild' },
  });
  const e = byTitle(m);
  assert.deepEqual(e.Distinctive, [['Ears', 'pointed'], ['Horn', 'curled']]);
  assert.deepEqual(e.Inventory, [['Gear', 'compass, rope'], ['Weapons', 'dagger']]);
  assert.deepEqual(e.Accessories, [['Items', 'silver locket']]);
  const ct = Object.fromEntries(e['Custom tags']);
  assert.deepEqual(ct, { curse: 'moon-bound', faction: 'thieves guild' });
});

// ── NPC ────────────────────────────────────────────────────────────────────
test('buildIdCard: sim npc = player layout + subtle NPC CARD tag', () => {
  const m = buildIdCard('sim', {
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', age: '40',
    hair_color: 'auburn', eye_color: 'brown', height: "5'6\"", weight: '130 lb',
    personality: 'warm', flaws: 'curious', dialogue_style: 'cheery', tone: 'cozy',
  });
  assert.equal(m.variant, 'player');
  assert.equal(m.tag, 'NPC CARD');
  assert.equal(m.banner, null);
  assert.equal(m.core.length, 8);
  assert.equal(m.core[0][0], 'Name');
  const e = byTitle(m);
  // NPC-only persona fields surface.
  assert.ok(e.Persona && e.Persona.find(([l]) => l === 'Dialogue style'));
});

// ── world ──────────────────────────────────────────────────────────────────
test('buildIdCard: sim world = WORLD CARD banner + Name/Setting/Purpose/Tone core', () => {
  const m = buildIdCard('sim', {
    card_type: 'world', name: 'Cinderfen', directive: 'a cursed fen',
    setting: 'dying village', tone: 'grim',
    date: '3rd of Harvest', time: '09:00', weather: 'fog', location: 'Village',
  });
  assert.equal(m.variant, 'world');
  assert.equal(m.banner, 'WORLD CARD');
  assert.equal(m.tag, null);
  // directive shows as "Purpose".
  assert.deepEqual(m.core, [
    ['Name', 'Cinderfen'], ['Setting', 'dying village'],
    ['Purpose', 'a cursed fen'], ['Tone', 'grim'],
  ]);
  const e = byTitle(m);
  assert.ok(e['World anchors']);
  // No Scenario section on a world card.
  assert.ok(!e.Scenario);
});

// ── scenario ───────────────────────────────────────────────────────────────
test('buildIdCard: sim scenario = SCENARIO CARD banner + Scenario extras', () => {
  const m = buildIdCard('sim', {
    card_type: 'scenario', name: 'Ambush', directive: 'bandits strike',
    setting: 'forest road', tone: 'tense',
    trigger_condition: 'leaving at night', primary_objective: 'survive',
    participating_actors: 'bandits, guards',
  });
  assert.equal(m.banner, 'SCENARIO CARD');
  assert.deepEqual(m.core[0], ['Name', 'Ambush']);
  const e = byTitle(m);
  assert.ok(e.Scenario && e.Scenario.find(([l]) => l === 'Trigger'));
  assert.ok(e.Scenario.find(([l]) => l === 'Objective'));
});

test('buildIdCard: sim with no card_type defaults to WORLD CARD', () => {
  const m = buildIdCard('sim', { name: 'Aldermoor', directive: 'survive', setting: 'a bog', tone: 'grim' });
  assert.equal(m.banner, 'WORLD CARD');
  assert.equal(m.variant, 'world');
});

// ── non-ID kinds ───────────────────────────────────────────────────────────
test('buildIdCard: codex + intro are not ID cards → null', () => {
  assert.equal(buildIdCard('codex', { entries: [] }), null);
  assert.equal(buildIdCard('intro', { intro: 'You wake.' }), null);
  assert.equal(buildIdCard('codex', {}), null);
});

test('buildIdCard: empty/unknown draft is safe', () => {
  const m = buildIdCard('player', {});
  assert.equal(m.variant, 'player');
  assert.deepEqual(m.core, []);
  assert.deepEqual(m.extra, []);
  assert.equal(buildIdCard('unknown', { name: 'x' }), null);
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
