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
test('buildIdCard: player = NAME header + license rows (race/gender, age/skin/body, height/weight, hair/eyes)', () => {
  const m = buildIdCard('player', {
    name: 'Kael', gender: 'male', race: 'human', age: '28',
    hair_color: 'black', hair_length: 'short', hair_style: 'messy',
    eye_color: 'green', height: "6'1\"", weight: '180 lb',
    body_type: 'lean', skin_complexion: 'tan', clothing: ['tunic', 'boots'],
    job: 'ranger', backstory: 'orphan', personality: 'stoic', wealth: '50 gold',
  });
  assert.equal(m.variant, 'player');
  assert.equal(m.title, 'Kael');          // the NAME is the header (2026-08-15)
  assert.equal(m.banner, null);
  assert.equal(m.tag, null);
  // The license grid, in Chloe's specified row order. The age/skin/body trio
  // packs as narrow thirds; hair stacks Color/Length/Style sub-lines.
  assert.deepEqual(m.core.map((c) => c.label), [
    'Race', 'Gender', 'Age', 'Skin', 'Body', 'Height', 'Weight', 'Hair', 'Eye Color',
  ]);
  assert.deepEqual(m.core.map((c) => c.value || c.sub), [
    'human', 'male', '28', 'tan', 'lean', "6'1\"", '180 lb',
    [['Color', 'black'], ['Length', 'short'], ['Style', 'messy']],
    'green',
  ]);
  assert.deepEqual(m.core.filter((c) => c.third).map((c) => c.label), ['Age', 'Skin', 'Body']);
  // Hair + physique live ON the face now — no Hair/Physique sections in extra.
  const e = byTitle(m);
  assert.ok(!('Hair' in e));
  assert.ok(!('Physique' in e));
  // v2: clothing rides the Inventory extra (the mutable sibling seed).
  assert.deepEqual(e.Inventory, [['Clothing', 'tunic, boots']]);
  assert.deepEqual(e['Starting conditions'], [['Wealth', '50 gold']]);
});

test('buildIdCard: player missing body → age/skin fall back to half cells, no ragged gap', () => {
  const m = buildIdCard('player', { name: 'Nyx', age: '120', skin_complexion: 'pale' });
  assert.ok(m.core.every((c) => !c.third));
  assert.deepEqual(m.core.map((c) => c.label), ['Age', 'Skin']);
});

test('buildIdCard: player drops empty cells + empty extra sections', () => {
  const m = buildIdCard('player', { name: 'Nyx', gender: 'female', hair_color: 'red' });
  // Only the present cells survive; missing ones are dropped (not null).
  assert.deepEqual(m.core.map((c) => c.label), ['Gender', 'Hair']);
  assert.deepEqual(m.core[1].sub, [['Color', 'red']]);
  // No Distinctive/Clothing/etc. → extra is [].
  assert.deepEqual(m.extra, []);
});

test('buildIdCard: player surfaces distinctive, inventory, custom tags', () => {
  const m = buildIdCard('player', {
    name: 'Nyx', race: 'tiefling', horn: 'curled', ears: 'pointed',
    gear: ['compass', 'rope'], equipped: ['dagger'], accessories: 'silver locket',
    custom_tags: { curse: 'moon-bound', faction: 'thieves guild' },
  });
  const e = byTitle(m);
  assert.deepEqual(e.Distinctive, [['Ears', 'pointed'], ['Horn', 'curled']]);
  // v2: one Inventory extra — Equipped/Accessories/Stored rows (legacy gear
  // folds to Stored).
  assert.deepEqual(e.Inventory, [
    ['Equipped', 'dagger'],
    ['Accessories', 'silver locket'],
    ['Stored', 'compass, rope'],
  ]);
  assert.ok(!('Accessories' in e), 'no separate Accessories extra in v2');
  const ct = Object.fromEntries(e['Custom tags']);
  assert.deepEqual(ct, { curse: 'moon-bound', faction: 'thieves guild' });
});

// ── NPC ────────────────────────────────────────────────────────────────────
test('buildIdCard: sim npc = player layout (NAME header) + subtle NPC CARD tag', () => {
  const m = buildIdCard('sim', {
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', age: '40',
    hair_color: 'auburn', eye_color: 'brown', height: "5'6\"", weight: '130 lb',
    personality: 'warm', flaws: 'curious', likes: 'songs', dislikes: 'nobles',
    dialogue_style: 'cheery', backstory: 'exile', goal: 'buy the tavern', tone: 'cozy',
    gear: ['compass'], accessories: ['silver locket'],
    date: 'Day 1', time: '09:00', weather: 'clear', location: 'Tavern',
  });
  assert.equal(m.variant, 'player');
  assert.equal(m.title, 'Mara');
  assert.equal(m.tag, 'NPC CARD');
  assert.equal(m.banner, null);
  assert.ok(m.core.find((c) => c.label === 'Race'));
  const e = byTitle(m);
  // v2: the FULL persona set surfaces (Goals + Backstory are persona
  // members now; the story tone is a world anchor, never persona).
  assert.ok(e.Persona && e.Persona.find(([l]) => l === 'Dialogue style'));
  assert.ok(e.Persona.find(([l]) => l === 'Likes'));
  assert.ok(e.Persona.find(([l]) => l === 'Dislikes'));
  assert.ok(e.Persona.find(([l]) => l === 'Goals'));
  assert.ok(e.Persona.find(([l]) => l === 'Backstory'));
  assert.ok(!e.Persona.find(([l]) => l === 'Tone'));
  assert.ok(!('Background' in e), 'the Background group is gone in v2');
  // The mandatory SIM anchors (incl. tone) surface in the details.
  assert.ok(e['World anchors'] && e['World anchors'].find(([l]) => l === 'Weather'));
  assert.ok(e['World anchors'].find(([l]) => l === 'Location'));
  assert.ok(e['World anchors'].find(([l]) => l === 'Tone'));
  // The inventory answers surface as the Inventory extra (legacy gear →
  // Stored, accessories their own row).
  assert.ok(e.Inventory && e.Inventory.find(([l]) => l === 'Stored'));
  assert.ok(e.Inventory.find(([l]) => l === 'Accessories'));
});

// ── world ──────────────────────────────────────────────────────────────────
test('buildIdCard: sim world = WORLD CARD banner header + Name/Setting/Purpose/Tone core', () => {
  const m = buildIdCard('sim', {
    card_type: 'world', name: 'Cinderfen', directive: 'a cursed fen',
    setting: 'dying village', tone: 'grim',
    date: '3rd of Harvest', time: '09:00', weather: 'fog', location: 'Village',
  });
  assert.equal(m.variant, 'world');
  assert.equal(m.banner, 'WORLD CARD');
  assert.equal(m.title, null);           // the banner is the header, not a name
  assert.equal(m.tag, null);
  // directive shows as "Purpose".
  assert.deepEqual(m.core.map((c) => [c.label, c.value]), [
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
  assert.deepEqual(m.core[0], { label: 'Name', value: 'Ambush' });
  const e = byTitle(m);
  assert.ok(e.Scenario && e.Scenario.find(([l]) => l === 'Trigger'));
  assert.ok(e.Scenario.find(([l]) => l === 'Objective'));
});

test('buildIdCard: sim with no card_type defaults to WORLD CARD', () => {
  const m = buildIdCard('sim', { name: 'Aldermoor', directive: 'survive', setting: 'a bog', tone: 'grim' });
  assert.equal(m.banner, 'WORLD CARD');
  assert.equal(m.variant, 'world');
});

// ── hostile GLM shapes never crash the model ───────────────────────────────
test('buildIdCard: numbers/arrays/nulls/objects in the draft coerce safely', () => {
  const m = buildIdCard('player', {
    name: 42, age: 28, race: ['human', 'elf'], hair_color: null,
    body_type: { bad: 'shape' }, eye_color: true,
  });
  assert.equal(m.title, '42');                 // numbers stringify
  assert.deepEqual(m.core.find((c) => c.label === 'Age').value, '28');
  assert.deepEqual(m.core.find((c) => c.label === 'Race').value, 'human, elf');
  assert.ok(!m.core.find((c) => c.label === 'Body'));   // object → absent
  assert.ok(!m.core.find((c) => c.label === 'Hair'));   // null → absent
  assert.ok(!m.core.find((c) => c.label === 'Eye Color')); // boolean → absent
});

// ── non-ID kinds ───────────────────────────────────────────────────────────
test('buildIdCard: codex is not an ID card → null (unknown kinds too)', () => {
  assert.equal(buildIdCard('codex', { entries: [] }), null);
  assert.equal(buildIdCard('codex', {}), null);
  // The intro wizard kind was removed 2026-08-15; any stray kind → null.
  assert.equal(buildIdCard('intro', { intro: 'You wake.' }), null);
});

test('buildIdCard: empty/unknown draft is safe', () => {
  const m = buildIdCard('player', {});
  assert.equal(m.variant, 'player');
  assert.equal(m.title, '');
  assert.deepEqual(m.core, []);
  assert.deepEqual(m.extra, []);
  assert.equal(buildIdCard('unknown', { name: 'x' }), null);
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
