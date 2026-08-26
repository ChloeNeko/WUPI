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
test('buildIdCard: player = NAME header + six license rows (race/gender, age/eye color, height/weight)', () => {
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
  assert.equal(m.tag, 'PLAYER CARD');     // the type subheader (2026-08-20)
  // The face is ONLY the six license rows (2026-08-20 Chloe) — skin, body,
  // and hair moved behind the details button.
  assert.deepEqual(m.core.map((c) => c.label), [
    'Race', 'Gender', 'Age', 'Eye Color', 'Height', 'Weight',
  ]);
  assert.deepEqual(m.core.map((c) => c.value), [
    'human', 'male', '28', 'green', "6'1\"", '180 lb',
  ]);
  assert.ok(m.core.every((c) => !c.third), 'no thirds cells on the face anymore');
  // Skin/Body/Hair live in the leading Appearance extra — hair as BARE
  // stacked values (newline-joined), no Color/Length/Style sub-labels.
  const e = byTitle(m);
  assert.deepEqual(e.Appearance, [
    ['Skin', 'tan'],
    ['Body', 'lean'],
    ['Hair', 'black\nshort\nmessy'],
  ]);
  assert.ok(!('Physique' in e));
  // v2: clothing rides the Inventory extra (the mutable sibling seed).
  assert.deepEqual(e.Inventory, [['Clothing', 'tunic, boots']]);
  // (2026-08-22 Chloe ruling) wealth never renders on the card — money is
  // inventory-only, standings are live tracker state.
  assert.ok(!('Starting conditions' in e));
});

test('buildIdCard: player face holds the six license rows only — skin/body land in the Appearance extra', () => {
  const m = buildIdCard('player', { name: 'Nyx', age: '120', skin_complexion: 'pale', body_type: 'wiry' });
  assert.deepEqual(m.core.map((c) => c.label), ['Age']);
  assert.deepEqual(byTitle(m).Appearance, [['Skin', 'pale'], ['Body', 'wiry']]);
});

test('buildIdCard: player drops empty cells + empty extra sections', () => {
  const m = buildIdCard('player', { name: 'Nyx', gender: 'female', hair_color: 'red' });
  // Only the present cells survive; missing ones are dropped (not null).
  assert.deepEqual(m.core.map((c) => c.label), ['Gender']);
  // Hair with only a color → one bare stacked line in the Appearance extra.
  assert.deepEqual(byTitle(m).Appearance, [['Hair', 'red']]);
  // No Distinctive/Inventory/etc. → extra holds just the Appearance section.
  assert.deepEqual(m.extra, [['Appearance', [['Hair', 'red']]]]);
});

test('buildIdCard: player surfaces distinctive, inventory, custom tags', () => {
  const m = buildIdCard('player', {
    name: 'Nyx', race: 'tiefling', horn: 'curled', ears: 'pointed',
    stored: ['compass', 'rope'], equipped: ['dagger'], accessories: 'silver locket',
    custom_tags: { curse: 'moon-bound', faction: 'thieves guild' },
  });
  const e = byTitle(m);
  assert.deepEqual(e.Distinctive, [['Ears', 'pointed'], ['Horn', 'curled']]);
  // v2: one Inventory extra — Equipped/Accessories/Stored rows (flat draft
  // folds to Stored).
  assert.deepEqual(e.Inventory, [
    ['Equipped', 'dagger'],
    ['Accessories', 'silver locket'],
    ['Stored', 'compass, rope'],
  ]);
  assert.ok(!('Accessories' in e), 'no separate Accessories extra in v2');
  // (2026-08-20) Custom-tag KEYS render prettified — no underscores on any
  // visual surface.
  const ct = Object.fromEntries(e['Custom tags']);
  assert.deepEqual(ct, { Curse: 'moon-bound', Faction: 'thieves guild' });
});

// ── NPC ────────────────────────────────────────────────────────────────────
test('buildIdCard: sim npc = player layout (NAME header — no NPC chip, 2026-08-20 Chloe)', () => {
  const m = buildIdCard('sim', {
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', age: '40',
    hair_color: 'auburn', eye_color: 'brown', height: "5'6\"", weight: '130 lb',
    personality: 'warm', flaws: 'curious', likes: 'songs', dislikes: 'nobles',
    dialogue_style: 'cheery', backstory: 'exile', goal: 'buy the tavern', tone: 'cozy',
    stored: ['compass'], accessories: ['silver locket'],
    date: 'Day 1', time: '09:00', weather: 'clear', location: 'Tavern',
  });
  assert.equal(m.variant, 'player');
  assert.equal(m.title, 'Mara');
  assert.equal(m.tag, 'NPC CARD'); // small subheader under the name rule (2026-08-20)
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
  assert.ok(e['World'] && e['World'].find(([l]) => l === 'Weather'));
  assert.ok(e['World'].find(([l]) => l === 'Location'));
  assert.ok(e['World'].find(([l]) => l === 'Tone'));
  // The inventory answers surface as the Inventory extra (flat draft
  // Stored, accessories their own row).
  assert.ok(e.Inventory && e.Inventory.find(([l]) => l === 'Stored'));
  assert.ok(e.Inventory.find(([l]) => l === 'Accessories'));
});

// ── world ──────────────────────────────────────────────────────────────────
test('buildIdCard: sim world = NAME header + WORLD CARD subheader + Setting/Purpose/Tone core', () => {
  const m = buildIdCard('sim', {
    card_type: 'world', name: 'Cinderfen', directive: 'a cursed fen',
    setting: 'dying village', tone: 'grim',
    date: '3rd of Harvest', time: '09:00', weather: 'fog', location: 'Village',
  });
  assert.equal(m.variant, 'world');
  assert.equal(m.title, 'Cinderfen');     // the NAME is the header (2026-08-20)
  assert.equal(m.banner, null);           // the type banner is now the subheader
  assert.equal(m.tag, 'WORLD CARD');
  // The Name cell moved into the header; directive shows as "Purpose".
  assert.deepEqual(m.core.map((c) => [c.label, c.value]), [
    ['Setting', 'dying village'],
    ['Purpose', 'a cursed fen'], ['Tone', 'grim'],
  ]);
  const e = byTitle(m);
  assert.ok(e['World']);
  // No Scenario section on a world card.
  assert.ok(!e.Scenario);
});

// ── scenario ───────────────────────────────────────────────────────────────
test('buildIdCard: sim scenario = NAME header + SCENARIO CARD subheader + Scenario extras', () => {
  const m = buildIdCard('sim', {
    card_type: 'scenario', name: 'Ambush', directive: 'bandits strike',
    setting: 'forest road', tone: 'tense',
    trigger_condition: 'leaving at night', primary_objective: 'survive',
    participating_actors: 'bandits, guards',
  });
  assert.equal(m.title, 'Ambush');
  assert.equal(m.tag, 'SCENARIO CARD');
  assert.deepEqual(m.core[0], { label: 'Setting', value: 'forest road' });
  const e = byTitle(m);
  assert.ok(e.Scenario && e.Scenario.find(([l]) => l === 'Trigger'));
  assert.ok(e.Scenario.find(([l]) => l === 'Objective'));
});

test('buildIdCard: sim with no card_type defaults to WORLD CARD', () => {
  const m = buildIdCard('sim', { name: 'Aldermoor', directive: 'survive', setting: 'a bog', tone: 'grim' });
  assert.equal(m.tag, 'WORLD CARD');
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
  assert.ok(!m.core.find((c) => c.label === 'Hair'));   // null → absent (hair never on the face)
  assert.ok(!m.core.find((c) => c.label === 'Eye Color')); // boolean → absent
  // None of them coerce into the Appearance extra either — the section drops.
  assert.ok(!('Appearance' in byTitle(m)));
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

// ── (2026-08-20 audit) authored holdings — the Holdings extra ───────────────
test('buildIdCard: authored holdings render as a Holdings section, dropped when empty', () => {
  const props = [
    { id: 'forge', node: 'iron-forge', kind: 'business', revenue: 8, upkeep: 3, owner: 'liam', price: 250 },
    { id: 'manor', node: 'hill', revenue: 2, upkeep: 9 },
  ];
  const player = buildIdCard('player', { name: 'Kael', properties: props });
  const sections = byTitle(player);
  assert.ok(sections.Holdings, 'player face carries Holdings');
  assert.equal(sections.Holdings[0][0], 'forge');
  assert.ok(sections.Holdings[0][1].includes('@ iron-forge'));
  assert.ok(sections.Holdings[0][1].includes('owner liam'));
  assert.equal(sections.Holdings[1][0], 'manor');
  const world = buildIdCard('sim', { card_type: 'world', name: 'Greywater', properties: props });
  assert.ok(byTitle(world).Holdings, 'world face carries Holdings');
  // No properties → no Holdings section at all (hide-when-untracked).
  const bare = buildIdCard('player', { name: 'Bare' });
  assert.ok(!byTitle(bare).Holdings);
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
