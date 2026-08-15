// Unit tests for the pure creator-wizard logic (engine/creator-engine.js).
// Plain Node ESM — no test runner. Run: `node tests/creator-engine.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
import { strict as assert } from 'node:assert';
import zlib from 'node:zlib';
import {
  parseEnvelope,
  stripToJsonFallback,
  mergeDraft,
  buildReviewSections,
  buildIdCard,
  missingMandatoryFields,
  MANDATORY_LABELS,
  findCharaChunk,
  readCharaChunk,
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

// Build a PNG carrying one iTXt chunk (compressed or not). Layout after the
// keyword NUL: compressionFlag(1) compressionMethod(1) langTag\0 transKey\0
// payload — both tags empty here.
function buildPngWithITXt(keyword, payloadBytes, compress = false) {
  const sig = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  const type = [0x69, 0x54, 0x58, 0x74]; // 'iTXt'
  const kw = Array.from(keyword, (c) => c.charCodeAt(0));
  const data = [...kw, 0, compress ? 1 : 0, 0, 0, 0, ...payloadBytes];
  const len = [0, 0, 0, data.length];
  const crc = [0, 0, 0, 0];
  const iend = [0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0, 0, 0, 0];
  return new Uint8Array([...sig, ...len, ...type, ...data, ...crc, ...iend]);
}

test('findCharaChunk: ccv3-only V3 card imports (uncompressed iTXt)', async () => {
  const json = JSON.stringify({ spec: 'chara_card_v3', data: { name: 'V3', description: 'x' } });
  const b64 = Buffer.from(json, 'utf-8').toString('base64');
  const payload = Array.from(Buffer.from(b64, 'latin1'));
  const u8 = buildPngWithITXt('ccv3', payload, false);
  assert.equal(findCharaChunk(u8), b64);
  assert.equal(await readCharaChunk(u8), b64);
});

test('findCharaChunk: chara wins over ccv3 when both are present', async () => {
  const a = Buffer.from(JSON.stringify({ name: 'A' }), 'utf-8').toString('base64');
  const b = Buffer.from(JSON.stringify({ name: 'B' }), 'utf-8').toString('base64');
  // Two tEXt chunks in one PNG: chara first, ccv3 second.
  const sig = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  const chunk = (kw, val) => {
    const data = [...Array.from(kw, (c) => c.charCodeAt(0)), 0, ...Array.from(Buffer.from(val, 'latin1'))];
    return [0, 0, 0, data.length, ...[0x74, 0x45, 0x58, 0x74], ...data, 0, 0, 0, 0];
  };
  const u8 = new Uint8Array([...sig, ...chunk('chara', a), ...chunk('ccv3', b), 0, 0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0, 0, 0, 0]);
  assert.equal(findCharaChunk(u8), a);
  assert.equal(await readCharaChunk(u8), a);
});

test('readCharaChunk: inflates zlib-compressed iTXt', async () => {
  const json = JSON.stringify({ name: 'Zipped' });
  const b64 = Buffer.from(json, 'utf-8').toString('base64');
  const zipped = Array.from(zlib.deflateSync(Buffer.from(b64, 'latin1')));
  const u8 = buildPngWithITXt('ccv3', zipped, true);
  // The sync walker can't inflate — it must skip the compressed candidate.
  assert.equal(findCharaChunk(u8), null);
  // The async reader inflates it.
  assert.equal(await readCharaChunk(u8), b64);
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

// ── the mandatory-field gate (2026-08-15 Chloe) ────────────────────────────
// A `ready` missing ANY mandatory field must never reach the review screen.
const FULL_PLAYER = {
  name: 'Kael', gender: 'male', age: '28', race: 'human',
  skin_complexion: 'tan', height: "6'1\"", weight: '180 lb', body_type: 'lean',
  hair_color: 'black', hair_length: 'short', hair_style: 'messy',
  eye_color: 'green', clothing: ['tunic'],
};
const SIM_BASE = { date: '3rd of Harvest', time: '09:00', weather: 'fog', location: 'Square' };

test('missingMandatoryFields: a complete player draft passes', () => {
  assert.deepEqual(missingMandatoryFields('player', FULL_PLAYER), []);
});

test('missingMandatoryFields: body_type:null is MISSING (the bug Chloe hit)', () => {
  const d = { ...FULL_PLAYER, body_type: null };
  assert.deepEqual(missingMandatoryFields('player', d), ['body_type']);
  // Blank string + absent are equally rejected.
  assert.deepEqual(missingMandatoryFields('player', { ...FULL_PLAYER, body_type: '  ' }), ['body_type']);
  const { body_type, ...noBody } = FULL_PLAYER;
  assert.deepEqual(missingMandatoryFields('player', noBody), ['body_type']);
});

test('missingMandatoryFields: numbers count as filled; booleans/objects do not', () => {
  assert.deepEqual(missingMandatoryFields('player', { ...FULL_PLAYER, age: 28 }), []);
  assert.deepEqual(missingMandatoryFields('player', { ...FULL_PLAYER, gender: true }), ['gender']);
  assert.deepEqual(missingMandatoryFields('player', { ...FULL_PLAYER, race: { bad: true } }), ['race']);
  assert.deepEqual(missingMandatoryFields('player', { ...FULL_PLAYER, clothing: [] }), ['clothing']);
});

test('missingMandatoryFields: sim npc requires the full identity set + persona + anchors', () => {
  const npc = { ...FULL_PLAYER, card_type: 'npc', personality: 'warm', flaws: 'curious',
    job: 'smith', backstory: 'b', dialogue_style: 'gruff', tone: 'cozy',
    ...SIM_BASE, intro: 'You wake.' };
  assert.deepEqual(missingMandatoryFields('sim', npc), []);
  const incomplete = { ...npc, body_type: null, personality: '' };
  delete incomplete.body_type;
  assert.deepEqual(missingMandatoryFields('sim', incomplete), ['body_type', 'personality']);
});

test('missingMandatoryFields: sim scenario/world branch sets + the router itself', () => {
  const scen = { card_type: 'scenario', name: 'Ambush', directive: 'd',
    trigger_condition: 't', primary_objective: 'o', participating_actors: ['bandits'],
    tone: 'tense', ...SIM_BASE, intro_answered: false };
  assert.deepEqual(missingMandatoryFields('sim', scen), []);
  const world = { card_type: 'world', name: 'W', directive: 'd', setting: 's', tone: 'grim', ...SIM_BASE, intro: 'x' };
  assert.deepEqual(missingMandatoryFields('sim', world), []);
  // No card_type at all → the router never ran → card_type + the world branch set.
  const pre = missingMandatoryFields('sim', {});
  assert.ok(pre.includes('card_type'));
  assert.ok(pre.includes('directive'));
});

test('missingMandatoryFields: sim requires an intro ANSWER (agreed text or explicit no)', () => {
  const world = { card_type: 'world', name: 'W', directive: 'd', setting: 's', tone: 'grim', ...SIM_BASE };
  // No intro + no marker → the question was never asked.
  assert.deepEqual(missingMandatoryFields('sim', world), ['intro_answer']);
  // Explicit decline marker → complete.
  assert.deepEqual(missingMandatoryFields('sim', { ...world, intro_answered: false }), []);
  // Agreed text → complete.
  assert.deepEqual(missingMandatoryFields('sim', { ...world, intro: 'You wake.' }), []);
});

test('missingMandatoryFields: codex requires ≥1 entry with a body', () => {
  assert.deepEqual(missingMandatoryFields('codex', { entries: [{ title: 'T', body: 'lore' }] }), []);
  assert.deepEqual(missingMandatoryFields('codex', { entries: [] }), ['entries']);
  assert.deepEqual(missingMandatoryFields('codex', {}), ['entries']);
  assert.deepEqual(missingMandatoryFields('codex', { entries: [{ title: 'T', body: '   ' }] }), ['entries']);
});

test('missingMandatoryFields: every missing key has a friendly label', () => {
  const missing = missingMandatoryFields('player', {});
  for (const k of missing) {
    assert.ok(MANDATORY_LABELS[k], `no label for ${k}`);
  }
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
