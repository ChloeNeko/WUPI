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
  shouldRejectDuplicateName,
  creatorRetryAllowed,
  MAX_CREATOR_RETRIES,
  MANDATORY_LABELS,
  findCharaChunk,
  readCharaChunk,
  base64ToUtf8,
  normalizeCharJson,
  extractLorebookEntries,
  extractStandaloneLorebook,
  charDataHasContent,
  batchLorebookEntries,
  padCodexEntryTags,
  normalizeLoreImportEntries,
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

test('buildReviewSections: player surfaces horn, inventory, persona, custom tags, starting conditions', () => {
  const secs = buildReviewSections('player', {
    name: 'Nyx', race: 'tiefling', horn: 'curled',
    gear: ['compass', 'rope'], equipped: ['dagger'],
    job: 'thief', backstory: 'orphan',
    wealth: '200 gold', reputation: '-20',
    custom_tags: { curse: 'moon-bound' },
  });
  const byName = Object.fromEntries(secs);
  assert.deepEqual(byName.Distinctive, [['Horn', 'curled']]);
  // v2: legacy gear folds to the Stored row; equipped is its own line.
  assert.ok(byName.Inventory.find(([l]) => l === 'Stored'));    // chip list joined
  assert.ok(byName.Inventory.find(([l]) => l === 'Equipped'));
  // job + backstory surface under the opt-in Persona group.
  assert.ok(byName.Persona.find(([l]) => l === 'Occupation'));
  assert.ok(byName.Persona.find(([l]) => l === 'History'));
  assert.ok(byName['Custom tags'].find(([l]) => l === 'curse'));
  assert.deepEqual(byName['Starting conditions'], [['Wealth', '200 gold'], ['Reputation', '-20']]);
});

test('buildReviewSections: sim (no card_type) includes world anchors (incl. tone), never locations/cast', () => {
  // v2 (2026-08-19): cast + the locations graph are GONE from the format —
  // they never surface. The anchors section carries the tone (a world
  // anchor, rendered beside date/time/weather).
  const secs = buildReviewSections('sim', {
    name: 'Aldermoor', directive: 'survive', setting: 'a bog', tone: 'grim',
    date: '3rd of Harvest, Year 1247', time: '09:00', weather: 'fog', location: 'Village',
    locations: [{ name: 'Village', neighbors: ['Marsh'] }],
    cast: [{ name: 'Mara', identity: 'innkeep' }],
  });
  const titles = secs.map(([t]) => t);
  assert.ok(titles.includes('World anchors'));
  assert.ok(!titles.includes('Locations'), 'no locations graph in v2');
  assert.ok(!titles.includes('Cast'), 'no cast in v2');
  const anchors = secs.find(([t]) => t === 'World anchors');
  assert.ok(anchors[1].find(([l]) => l === 'Tone'));
});

test('buildReviewSections: sim npc/scenario/world branch sections', () => {
  const npc = Object.fromEntries(buildReviewSections('sim', {
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', hair_color: 'auburn',
    personality: 'warm', flaws: 'curious', likes: 'river songs', dislikes: 'nobles',
    job: 'innkeep', backstory: 'exile', dialogue_style: 'cheery', goal: 'buy the tavern', tone: 'cozy',
  }));
  assert.ok(npc.Identity && npc.Identity.find(([l]) => l === 'Gender'));
  assert.ok(npc.Persona && npc.Persona.find(([l]) => l === 'Dialogue style'));
  // 2026-08-19 v2: the FULL persona set — likes/dislikes/goals/backstory are
  // persona members; the story tone is a world anchor, never persona.
  assert.ok(npc.Persona.find(([l]) => l === 'Likes'));
  assert.ok(npc.Persona.find(([l]) => l === 'Dislikes'));
  assert.ok(npc.Persona.find(([l]) => l === 'Goals'));
  assert.ok(npc.Persona.find(([l]) => l === 'Backstory'));
  assert.ok(!npc.Persona.find(([l]) => l === 'Tone'));
  assert.ok(!npc.Background, 'the Background group is gone (all persona now)');
  const anchors = Object.fromEntries(npc['World anchors'] || []);
  assert.ok(anchors.Tone === 'cozy', 'tone renders as a world anchor');

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

// ── extractLorebookEntries (the mechanical lorebook→codex extraction) ──────
// (2026-08-19 Chloe rules: keyword-less entries are SKIPPED at parse time;
// content-less entries are skipped; title falls back comment → name → first
// key; keys read the `key` ∪ `keys` union.)
test('extractLorebookEntries: array + object-map shapes, title/keys/body', () => {
  const arr = extractLorebookEntries({ entries: [{ comment: 'T1', content: 'B1', key: ['a', 'b'] }] });
  assert.equal(arr.length, 1);
  assert.equal(arr[0].title, 'T1');
  assert.deepEqual(arr[0].keys, ['a', 'b']);
  assert.equal(arr[0].content, 'B1');
  const map = extractLorebookEntries({ entries: { 0: { comment: 'M', content: 'B', keys: ['k1', 'k2', 'k1'] } } });
  assert.equal(map.length, 1);
  assert.equal(map[0].title, 'M');
  // key + keys union, deduped (real ST exports carry both fields).
  assert.deepEqual(map[0].keys, ['k1', 'k2']);
});

test('extractLorebookEntries: SKIPS entries with no keywords (2026-08-19 ruling)', () => {
  const r = extractLorebookEntries({
    entries: [
      { comment: 'READ THIS legend', content: 'symbol legend', key: [] },
      { comment: 'constant header', content: 'always-on text' },
      { comment: 'real', content: 'kept', key: ['x'] },
      { comment: 'empty body', content: '   ', key: ['y'] },
    ],
  });
  assert.equal(r.length, 1);
  assert.equal(r[0].title, 'real');
});

test('extractLorebookEntries: no book / junk → []', () => {
  assert.deepEqual(extractLorebookEntries(null), []);
  assert.deepEqual(extractLorebookEntries({}), []);
  assert.deepEqual(extractLorebookEntries({ entries: 'nope' }), []);
});

// ── extractStandaloneLorebook (recognition at the import boundary) ─────────
test('extractStandaloneLorebook: Frieren-style book (name, EMPTY description, entries map) → recognized', () => {
  const lore = extractStandaloneLorebook({
    name: 'Frieren',
    description: '',
    entries: { 1: { uid: 1, comment: '═══════[Setting]═══════', content: 'A world.', key: [], keys: [] }, 2: { uid: 2, comment: 'Mana', content: 'Mana is everywhere.', key: ['mana', 'magic'] } },
  });
  assert.ok(lore);
  assert.equal(lore.name, 'Frieren');
  // The keyless decorative header is skipped — only the keyed entry survives.
  assert.equal(lore.entries.length, 1);
  assert.equal(lore.entries[0].title, 'Mana');
});

test('extractStandaloneLorebook: character-shaped object → null (character path owns it)', () => {
  assert.equal(extractStandaloneLorebook({ name: 'X', description: 'Y', entries: [{ key: ['k'], content: 'b' }] }), null);
  assert.equal(extractStandaloneLorebook({ name: 'X', first_mes: 'hi', entries: [{ key: ['k'], content: 'b' }] }), null);
});

test('extractStandaloneLorebook: no usable entries / arrays / non-objects → null', () => {
  assert.equal(extractStandaloneLorebook({ name: 'X' }), null);
  assert.equal(extractStandaloneLorebook({ entries: [{ comment: 'keyless', content: 'b' }] }), null);
  assert.equal(extractStandaloneLorebook([1, 2, 3]), null);
  assert.equal(extractStandaloneLorebook('nope'), null);
});

// ── charDataHasContent (the unrecognized-JSON gate) ─────────────────────────
test('charDataHasContent: empty husk → false, any content field → true', () => {
  assert.equal(charDataHasContent(normalizeCharJson({ random: 'settings dump' })), false);
  assert.equal(charDataHasContent(normalizeCharJson({ name: 'Kael' })), true);
  assert.equal(charDataHasContent(normalizeCharJson({ description: 'tall' })), true);
  assert.equal(charDataHasContent(normalizeCharJson({ tags: ['a'] })), true);
  assert.equal(charDataHasContent(normalizeCharJson({ character_book: { entries: [] } })), true);
  assert.equal(charDataHasContent(null), false);
});

// ── batchLorebookEntries (the API-budget chunker) ───────────────────────────
test('batchLorebookEntries: whole entries only, budget respected, order kept', () => {
  const mk = (i, size) => ({ title: `T${i}`, keys: ['k'], content: 'x'.repeat(size) });
  const entries = [mk(1, 3000), mk(2, 3000), mk(3, 3000), mk(4, 100)];
  const batches = batchLorebookEntries(entries, 8000);
  // 3000+3000+3000 > 8000 → the third entry opens a fresh batch.
  assert.equal(batches.length, 2);
  assert.deepEqual(batches[0].map((e) => e.title), ['T1', 'T2']);
  assert.deepEqual(batches[1].map((e) => e.title), ['T3', 'T4']);
  // Flattened batches preserve the original order exactly.
  assert.deepEqual(batches.flat().map((e) => e.title), ['T1', 'T2', 'T3', 'T4']);
});

test('batchLorebookEntries: a single oversized entry rides alone', () => {
  const entries = [{ title: 'Big', keys: ['k'], content: 'x'.repeat(20000) }, { title: 'Small', keys: ['k'], content: 'y' }];
  const batches = batchLorebookEntries(entries, 8000);
  assert.equal(batches.length, 2);
  assert.equal(batches[0][0].title, 'Big');
  assert.deepEqual(batches[1].map((e) => e.title), ['Small']);
});

test('batchLorebookEntries: empty / non-array → []', () => {
  assert.deepEqual(batchLorebookEntries([]), []);
  assert.deepEqual(batchLorebookEntries(null), []);
});

// ── padCodexEntryTags (the ≥3-tag mechanical floor) ─────────────────────────
test('padCodexEntryTags: pads to ≥3 from source keys → title words → book name', () => {
  // Assistant gave 1 tag; source keys top it up.
  assert.deepEqual(
    padCodexEntryTags(['demons'], ['demon', 'abyss'], 'Demon Lords', 'Frieren'),
    ['demons', 'demon', 'abyss', 'lords', 'frieren'],
  );
  // No assistant tags, no source keys — title words + book name carry it.
  const t = padCodexEntryTags([], [], 'Continental Magic', 'Frieren');
  assert.ok(t.length >= 3);
  assert.ok(t.includes('continental'));
  assert.ok(t.includes('frieren'));
  // Total drought → the generic floor guarantees 3.
  assert.equal(padCodexEntryTags([], [], '??', '').length >= 3, true);
  // Dedupe is case-insensitive + capped at 8.
  assert.equal(padCodexEntryTags(['A', 'a', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I'], [], 'T', 'N').length, 8);
});

// ── normalizeLoreImportEntries (the batch result normalizer) ────────────────
test('normalizeLoreImportEntries: title fallback, tag floor, part-suffix source match, husk drop', () => {
  const sources = [
    { title: 'Demon Lords', keys: ['demon', 'abyss'], content: 'raw' },
    { title: 'Mana', keys: ['mana'], content: 'raw2' },
  ];
  const merged = normalizeLoreImportEntries([
    { title: '', tags: [], body: 'Refined body one.' },               // husk title → source title; tags floored
    { title: 'Mana — Part 1', tags: ['mana'], body: 'Refined body two.' }, // part suffix matches its source
    { tags: ['x'], body: '' },                                        // body-less husk dropped
  ], sources, 'Frieren');
  assert.equal(merged.length, 2);
  assert.equal(merged[0].title, 'Demon Lords');
  assert.ok(merged[0].tags.length >= 3);
  assert.ok(merged[0].tags.includes('demon'));
  assert.ok(merged[1].tags.includes('mana'));
  assert.ok(merged[1].tags.length >= 3);
  // Non-array input → [].
  assert.deepEqual(normalizeLoreImportEntries(null, sources, 'Frieren'), []);
});

// ── the mandatory-field gate (2026-08-15 Chloe) ────────────────────────────
// A `ready` missing ANY mandatory field must never reach the review screen.
const FULL_PLAYER = {
  name: 'Kael', gender: 'male', age: '28', race: 'human',
  skin_complexion: 'tan', height: "6'1\"", weight: '180 lb', body_type: 'lean',
  hair_color: 'black', hair_length: 'short', hair_style: 'messy',
  eye_color: 'green', clothing: ['tunic'],
  // v2 (2026-08-19): clothing is no longer a mandatory identity field, but
  // the FINAL persona question must be answered — this fixture answered it.
  persona: { personality: 'Steady.' },
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
  // clothing is OPTIONAL in v2 — an empty list no longer blocks.
  assert.deepEqual(missingMandatoryFields('player', { ...FULL_PLAYER, clothing: [] }), []);
  // The persona question: absent with no marker blocks; the explicit decline
  // marker passes; a declined-then-declined run re-checking stays clean.
  const declined = { ...FULL_PLAYER, persona: undefined, persona_answered: false };
  assert.deepEqual(missingMandatoryFields('player', declined), []);
  const neverAsked = { ...FULL_PLAYER, persona: undefined, personality: undefined };
  delete neverAsked.personality;
  assert.deepEqual(missingMandatoryFields('player', neverAsked), ['persona_answer']);
});

test('missingMandatoryFields: sim npc requires the full identity set + persona + background + anchors', () => {
  const npc = { ...FULL_PLAYER, card_type: 'npc', personality: 'warm', flaws: 'curious',
    likes: 'songs', dislikes: 'nobles', job: 'smith', backstory: 'b', dialogue_style: 'gruff',
    goal: 'reopen the forge', tone: 'cozy', items_answered: false,
    ...SIM_BASE, intro: 'You wake.' };
  assert.deepEqual(missingMandatoryFields('sim', npc), []);
  const incomplete = { ...npc, body_type: null, personality: '', goal: '' };
  delete incomplete.body_type;
  assert.deepEqual(missingMandatoryFields('sim', incomplete), ['body_type', 'personality', 'goal']);
  // The 2026-08-19 additions are each independently gated.
  const noLikes = { ...npc, likes: ' ' };
  assert.deepEqual(missingMandatoryFields('sim', noLikes), ['likes']);
  const noDislikes = { ...npc, dislikes: [] };
  assert.deepEqual(missingMandatoryFields('sim', noDislikes), ['dislikes']);
});

test('missingMandatoryFields: npc items questions — must be asked, may be declined', () => {
  const base = { ...FULL_PLAYER, card_type: 'npc', personality: 'warm', flaws: 'curious',
    likes: 'songs', dislikes: 'nobles', job: 'smith', backstory: 'b', dialogue_style: 'gruff',
    goal: 'reopen the forge', tone: 'cozy', ...SIM_BASE, intro: 'You wake.' };
  // All-empty item fields with NO marker → the wizard never asked.
  assert.deepEqual(missingMandatoryFields('sim', base), ['items_answer']);
  // The explicit decline marker completes the draft.
  assert.deepEqual(missingMandatoryFields('sim', { ...base, items_answered: false }), []);
  // ANY filled item field counts as asked — no marker needed.
  assert.deepEqual(missingMandatoryFields('sim', { ...base, accessories: ['silver locket'] }), []);
  assert.deepEqual(missingMandatoryFields('sim', { ...base, gear: ['compass'] }), []);
  // The gate is npc-only: scenario/world drafts never carry it.
  const scen = { card_type: 'scenario', name: 'A', directive: 'd', trigger_condition: 't',
    primary_objective: 'o', participating_actors: ['b'], tone: 't', ...SIM_BASE, intro_answered: false };
  assert.deepEqual(missingMandatoryFields('sim', scen), []);
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

// ── shouldRejectDuplicateName (2026-08-15: the rename hole M3 hid) ────────

test('shouldRejectDuplicateName: fresh CREATE colliding with an existing id rejects', () => {
  assert.equal(shouldRejectDuplicateName('nyx', undefined, ['kael', 'nyx']), true);
  assert.equal(shouldRejectDuplicateName('kael', null, ['kael', 'nyx']), true);
});

test('shouldRejectDuplicateName: a genuinely new name passes', () => {
  assert.equal(shouldRejectDuplicateName('mira', undefined, ['kael', 'nyx']), false);
  assert.equal(shouldRejectDuplicateName('mira', undefined, []), false);
});

test('shouldRejectDuplicateName: edit run re-saving its own id is exempt', () => {
  // Editing "Kael" without renaming: the write target IS the seeded id.
  assert.equal(shouldRejectDuplicateName('kael', 'kael', ['kael', 'nyx']), false);
});

test('shouldRejectDuplicateName: an edit run RENAMING onto another player rejects', () => {
  // THE rename hole: editing "Kael", GLM renames her to "Nyx", a different
  // saved player "Nyx" exists — fable_player_write is a silent atomic
  // overwrite, so the guard must fire despite seedDraft being present.
  assert.equal(shouldRejectDuplicateName('nyx', 'kael', ['kael', 'nyx']), true);
});

// ── creatorRetryAllowed (2026-08-15: the retry caps were DOM-coupled) ─────

test('creatorRetryAllowed: attempts 1..MAX retry, beyond exhausts', () => {
  assert.equal(MAX_CREATOR_RETRIES, 2);
  assert.equal(creatorRetryAllowed(1), true);
  assert.equal(creatorRetryAllowed(2), true);
  assert.equal(creatorRetryAllowed(3), false, 'the third attempt surfaces the gap to the user');
  assert.equal(creatorRetryAllowed(99), false);
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
