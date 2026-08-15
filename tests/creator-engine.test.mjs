// Unit tests for the pure creator-wizard logic (engine/creator-engine.js).
// Plain Node ESM — no test runner. Run: `node tests/creator-engine.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
import { strict as assert } from 'node:assert';
import {
  parseEnvelope,
  stripToJsonFallback,
  mergeDraft,
  buildReviewSections,
  findCharaChunk,
  base64ToUtf8,
  normalizeCharJson,
  normalizeLorebookJson,
  lorebookToCodexEntries,
} from '../src/fable/engine/creator-engine.js';

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

// ── parseEnvelope ──────────────────────────────────────────────────────────
test('parseEnvelope: raw ready JSON', () => {
  const env = parseEnvelope('{"action":"ready","draft":{"name":"Kael"}}');
  assert.equal(env.action, 'ready');
  assert.equal(env.draft.name, 'Kael');
});

test('parseEnvelope: ```json fenced', () => {
  const env = parseEnvelope('```json\n{"action":"ask","message":"hi","questions":["q1"],"draft":{"name":"K"}}\n```');
  assert.equal(env.action, 'ask');
  assert.equal(env.message, 'hi');
  assert.deepEqual(env.questions, ['q1']);
  assert.equal(env.draft.name, 'K');
});

test('parseEnvelope: prose around JSON → {…} fallback', () => {
  const env = parseEnvelope('Sure! Here you go:\n{"action":"ready","draft":{"name":"V"}}\nHope that helps.');
  assert.equal(env.action, 'ready');
  assert.equal(env.draft.name, 'V');
});

test('parseEnvelope: empty / garbage → null', () => {
  assert.equal(parseEnvelope(''), null);
  assert.equal(parseEnvelope(null), null);
  assert.equal(parseEnvelope('no json here at all'), null);
});

test('parseEnvelope: bare ``` fence (no json lang)', () => {
  const env = parseEnvelope('```\n{"action":"ready","draft":{}}\n```');
  assert.equal(env.action, 'ready');
});

// ── stripToJsonFallback ────────────────────────────────────────────────────
test('stripToJsonFallback: prefers nested message', () => {
  assert.equal(stripToJsonFallback('{"message":"hello"}'), 'hello');
});
test('stripToJsonFallback: strips fences when no message', () => {
  assert.equal(stripToJsonFallback('```json\nnot an object\n```'), 'not an object');
});

// ── mergeDraft ─────────────────────────────────────────────────────────────
test('mergeDraft: merges non-empty fields, skips blanks', () => {
  const dst = { name: 'A', age: '30' };
  mergeDraft(dst, { name: '', age: null, race: 'elf', eyes: undefined });
  assert.equal(dst.name, 'A');       // blank did not clobber
  assert.equal(dst.age, '30');       // null did not clobber
  assert.equal(dst.race, 'elf');     // new field merged
  assert.ok(!('eyes' in dst));       // undefined skipped entirely
});

test('mergeDraft: arrays replace, not concatenate', () => {
  const dst = { clothing: ['a'] };
  mergeDraft(dst, { clothing: ['x', 'y'] });
  assert.deepEqual(dst.clothing, ['x', 'y']);
});

// ── buildReviewSections ────────────────────────────────────────────────────
test('buildReviewSections: player drops empty rows', () => {
  const secs = buildReviewSections('player', { name: 'Kael', gender: 'male', hair_color: 'black' });
  const labels = secs.map(([t]) => t);
  assert.ok(labels.includes('Identity'));
  assert.ok(labels.includes('Hair'));
  // Appearance is all-empty (no height/weight/...) → filtered out.
  assert.ok(!labels.includes('Appearance'));
  const identity = secs.find(([t]) => t === 'Identity');
  assert.deepEqual(identity[1], [['Name', 'Kael'], ['Gender', 'male']]);
});

test('buildReviewSections: player surfaces horn, inventory, custom tags, starting conditions', () => {
  const secs = buildReviewSections('player', {
    name: 'Nyx', race: 'tiefling', horn: 'curled',
    gear: ['compass', 'rope'], weapons: ['dagger'],
    job: 'thief', backstory: 'orphan',
    wealth: '200 gold', reputation: '-20',
    custom_tags: { curse: 'moon-bound' },
  });
  const byName = Object.fromEntries(secs);
  assert.deepEqual(byName.Distinctive, [['Horn', 'curled']]);
  assert.ok(byName.Inventory.find(([l]) => l === 'Gear'));      // chip list joined
  assert.ok(byName['Custom tags'].find(([l]) => l === 'curse'));
  assert.deepEqual(byName['Starting conditions'], [['Wealth', '200 gold'], ['Reputation', '-20']]);
});

test('buildReviewSections: sim (no card_type) includes world anchors + locations + cast', () => {
  // Pre-router / world-style draft: anchors section renamed to 'World anchors'
  // (2026-08-13 Type Router), but Locations/Cast still surface.
  const secs = buildReviewSections('sim', {
    name: 'Aldermoor', directive: 'survive', setting: 'a bog', tone: 'grim',
    date: '3rd of Harvest, Year 1247', time: '09:00', weather: 'fog', location: 'Village',
    locations: [{ name: 'Village', neighbors: ['Marsh'] }],
    cast: [{ name: 'Mara', identity: 'innkeep' }],
  });
  const titles = secs.map(([t]) => t);
  assert.ok(titles.includes('World anchors'));
  assert.ok(titles.includes('Locations'));
  assert.ok(titles.includes('Cast'));
  const loc = secs.find(([t]) => t === 'Locations');
  assert.deepEqual(loc[1], [['Village', 'Marsh']]);
});

test('buildReviewSections: sim npc/scenario/world branch sections', () => {
  const npc = Object.fromEntries(buildReviewSections('sim', {
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', hair_color: 'auburn',
    personality: 'warm', flaws: 'curious', job: 'innkeep', backstory: 'exile', dialogue_style: 'cheery', tone: 'cozy',
  }));
  assert.ok(npc.Identity && npc.Identity.find(([l]) => l === 'Gender'));
  assert.ok(npc.Persona && npc.Persona.find(([l]) => l === 'Dialogue style'));

  const sce = Object.fromEntries(buildReviewSections('sim', {
    card_type: 'scenario', name: 'Ambush', directive: 'bandits strike',
    trigger_condition: 'leaving at night', primary_objective: 'survive',
    participating_actors: 'bandits, guards', tone: 'tense',
  }));
  assert.ok(sce.Scenario && sce.Scenario.find(([l]) => l === 'Trigger'));

  const wrld = Object.fromEntries(buildReviewSections('sim', {
    card_type: 'world', name: 'Cinderfen', directive: 'a cursed fen', setting: 'dying village', tone: 'grim',
  }));
  assert.ok(wrld.World && wrld.World.find(([l]) => l === 'Setting'));
});

test('buildReviewSections: codex → one section per entry', () => {
  const secs = buildReviewSections('codex', {
    entries: [{ title: 'Magic', tags: ['arcane'], body: 'Spells cost stamina.' }],
  });
  assert.equal(secs.length, 1);
  assert.equal(secs[0][0], 'Magic');
});

test('buildReviewSections: intro kind is gone → [] (removed with the intro wizard)', () => {
  // 2026-08-15: the intro wizard was deleted — the SIM Wizard gathers the
  // intro itself (its sections carry it under 'Intro' / 'Text').
  assert.deepEqual(buildReviewSections('intro', { intro: 'You wake in fog.' }), []);
});

test('buildReviewSections: sim draft surfaces the intro as Intro/Text', () => {
  const secs = buildReviewSections('sim', { name: 'Aldermoor', intro: 'You wake in fog.' });
  const introSec = secs.find(([t]) => t === 'Intro');
  assert.ok(introSec, 'Intro section present');
  assert.deepEqual(introSec[1], [['Text', 'You wake in fog.']]);
  // No intro agreed → section dropped entirely.
  const none = buildReviewSections('sim', { name: 'Aldermoor' });
  assert.ok(!none.some(([t]) => t === 'Intro'));
});

test('buildReviewSections: empty draft → []', () => {
  assert.deepEqual(buildReviewSections('player', {}), []);
});

// ── findCharaChunk (PNG walker) ────────────────────────────────────────────
// Build a minimal PNG: signature + one tEXt chunk keyed "chara" whose value is
// a base64 JSON blob. CRC is not validated by the walker (it's skipped), so a
// zero CRC is fine.
function buildPngWithChara(valueB64) {
  const sig = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  const type = [0x74, 0x45, 0x58, 0x74]; // 'tEXt'
  const keyword = [0x63, 0x68, 0x61, 0x72, 0x61]; // 'chara'
  const nul = [0x00];
  const valueBytes = Array.from(valueB64, (c) => c.charCodeAt(0));
  const data = [...keyword, ...nul, ...valueBytes];
  const len = [0, 0, 0, data.length]; // u32be
  const crc = [0, 0, 0, 0]; // dummy (walker doesn't check)
  const iend = [
    0, 0, 0, 0,             // length 0
    0x49, 0x45, 0x4e, 0x44, // 'IEND'
    0, 0, 0, 0,             // crc
  ];
  return new Uint8Array([...sig, ...len, ...type, ...data, ...crc, ...iend]);
}

test('findCharaChunk: extracts the chara value from a PNG', () => {
  const json = JSON.stringify({ name: 'Test', description: 'd' });
  const b64 = Buffer.from(json, 'utf-8').toString('base64');
  const u8 = buildPngWithChara(b64);
  assert.equal(findCharaChunk(u8), b64);
});

test('findCharaChunk: non-PNG → null', () => {
  assert.equal(findCharaChunk(new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8])), null);
});

test('findCharaChunk: too short → null', () => {
  assert.equal(findCharaChunk(new Uint8Array([0x89, 0x50])), null);
});

// ── base64ToUtf8 ───────────────────────────────────────────────────────────
test('base64ToUtf8: round-trips UTF-8', () => {
  const orig = 'héllo 世界 — "quotes"';
  const b64 = Buffer.from(orig, 'utf-8').toString('base64');
  assert.equal(base64ToUtf8(b64), orig);
});

// ── normalizeCharJson ──────────────────────────────────────────────────────
test('normalizeCharJson: V2 wrapper unwrapped', () => {
  const c = normalizeCharJson({ spec: 'chara_card_v2', data: { name: 'N', description: 'D', character_book: { entries: [] } } });
  assert.equal(c.name, 'N');
  assert.equal(c.description, 'D');
  assert.ok(c.character_book);
  assert.equal(c.spec, 'chara_card_v2');
});

test('normalizeCharJson: plain shape', () => {
  const c = normalizeCharJson({ name: 'P', first_mes: 'hi' });
  assert.equal(c.name, 'P');
  assert.equal(c.first_mes, 'hi');
});

test('normalizeCharJson: null → null', () => {
  assert.equal(normalizeCharJson(null), null);
});

test('normalizeCharJson: V2 content fields captured (alternate greetings, prompts, tags)', () => {
  // The classic 8 fields alone dropped these authored-content fields on import.
  const c = normalizeCharJson({
    spec: 'chara_card_v2', data: {
      name: 'N', description: 'd',
      alternate_greetings: ['hi', 'yo'],
      system_prompt: 'be spooky', post_history_instructions: 'stay in world',
      tags: ['dark', 'fantasy'], creator: 'Chloe', character_version: '1.2',
    },
  });
  assert.deepEqual(c.alternate_greetings, ['hi', 'yo']);
  assert.equal(c.system_prompt, 'be spooky');
  assert.equal(c.post_history_instructions, 'stay in world');
  assert.deepEqual(c.tags, ['dark', 'fantasy']);
  assert.equal(c.creator, 'Chloe');
  assert.equal(c.character_version, '1.2');
});

test('normalizeCharJson: missing V2 fields → empty defaults (not undefined)', () => {
  const c = normalizeCharJson({ name: 'P' });
  assert.deepEqual(c.alternate_greetings, []);
  assert.deepEqual(c.tags, []);
  assert.equal(c.system_prompt, '');
  assert.equal(c.post_history_instructions, '');
  assert.equal(c.creator, '');
  // A non-array tags value (some cards ship a string) is normalized to [].
  const c2 = normalizeCharJson({ name: 'P', tags: 'oops' });
  assert.deepEqual(c2.tags, []);
});

// ── normalizeLorebookJson ──────────────────────────────────────────────────
test('normalizeLorebookJson: standalone lorebook → charData with character_book', () => {
  const c = normalizeLorebookJson({ entries: [{ key: ['k'], content: 'body', comment: 'C' }] });
  assert.ok(c.character_book);
  assert.ok(Array.isArray(c.character_book.entries));
  assert.equal(c.spec, 'lorebook');
});

test('normalizeLorebookJson: character-shaped object (has name+desc) → null (defer)', () => {
  assert.equal(normalizeLorebookJson({ name: 'X', description: 'Y', entries: [] }), null);
});

test('normalizeLorebookJson: no entries → null', () => {
  assert.equal(normalizeLorebookJson({ name: 'X' }), null);
});

// ── lorebookToCodexEntries ─────────────────────────────────────────────────
test('lorebookToCodexEntries: array + object-map shapes', () => {
  const arr = lorebookToCodexEntries({ entries: [{ comment: 'T1', content: 'B1', key: ['a', 'b'] }] });
  assert.equal(arr.length, 1);
  assert.equal(arr[0].title, 'T1');
  assert.deepEqual(arr[0].tags, ['a', 'b']);
  const map = lorebookToCodexEntries({ entries: { 0: { comment: 'M', content: 'B' } } });
  assert.equal(map.length, 1);
  assert.equal(map[0].title, 'M');
});

test('lorebookToCodexEntries: drops empty-body entries', () => {
  const r = lorebookToCodexEntries({ entries: [{ comment: 'empty', content: '   ' }] });
  assert.equal(r.length, 0);
});

test('lorebookToCodexEntries: no book → []', () => {
  assert.deepEqual(lorebookToCodexEntries(null), []);
  assert.deepEqual(lorebookToCodexEntries({}), []);
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
