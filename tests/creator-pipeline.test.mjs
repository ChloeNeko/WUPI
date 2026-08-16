// Integration test: the full creator pipeline with a REALISTIC synthetic GLM
// `ready` envelope — the exact shape GLM is prompted to emit. Exercises
// parseEnvelope → mergeDraft → serializeSimCard + validates the emitted XML's
// structure (the part no unit test covered end-to-end). No app, no GLM, no key.
// Run: `node tests/creator-pipeline.test.mjs`.
import { strict as assert } from 'node:assert';
import { parseEnvelope, mergeDraft } from '../src/fable/engine/creator-engine.js';
import { serializeSimCard, serializePlayer, slugify } from '../src/fable/screens/card-serialize.js';

let passed = 0;
let failed = 0;
function test(name, fn) {
  try { fn(); console.log('  ok   %s', name); passed++; }
  catch (e) { console.error('  FAIL %s\n       %s', name, e.message); failed++; }
}

// A realistic GLM envelope — fenced, with a leading acknowledgement line (the
// exact messy shape the parseEnvelope defensive path must handle).
const GLM_REPLY_SIM = `Sure! Here's the world:\n\`\`\`json\n${JSON.stringify({
  action: 'ready',
  draft: {
    name: 'Aldermoor',
    directive: 'Survive the encroaching bog and find out why the village is sinking.',
    setting: 'A dying moorland village slowly being swallowed by a sentient marsh.',
    tone: 'grim folk horror with flashes of warmth',
    start_time: 'Day 1, 07:30',
    start_weather: 'low fog rolling off the marsh',
    locations: [
      { name: 'Village Square', neighbors: ['Marsh Edge', 'Old Chapel'] },
      { name: 'Marsh Edge', neighbors: ['Village Square'] },
      { name: 'Old Chapel', neighbors: ['Village Square'] },
    ],
    cast: [
      { name: 'Mara', identity: 'the village innkeep, knows everyone' },
      { name: 'Old Perrin', identity: 'the half-blind sexton' },
    ],
  },
})}\n\`\`\``;

test('pipeline: sim envelope → valid XML with anchors + graph + cast', () => {
  // 1. Parse the envelope (defensive: fence + leading prose).
  const env = parseEnvelope(GLM_REPLY_SIM);
  assert.ok(env, 'envelope parsed');
  assert.equal(env.action, 'ready');

  // 2. Accumulate the draft (mergeDraft is the accumulator).
  const draft = {};
  mergeDraft(draft, env.draft);
  assert.equal(draft.name, 'Aldermoor');
  assert.equal(draft.locations.length, 3);

  // 3. Serialize.
  const { xml, intro } = serializeSimCard(draft);

  // 4. Structural validation of the emitted XML.
  //    a. roleplay metadata (so fable_cards_list surfaces it). The embedded
  //       <id> (2026-08-16 audit fix #3: the client slug rides in-metadata so
  //       the parsed id == the created folder by construction) is asserted
  //       too — the name "Aldermoor" slugs to "aldermoor". This draft carries
  //       no card_type, so no <subtype> appears.
  assert.ok(xml.includes('<metadata><type>roleplay</type>'));
  assert.ok(/<id>aldermoor<\/id>/.test(xml));
  //    b. anchors block with both children (weather prose is CDATA-wrapped).
  assert.ok(/<start>\s*<time>/.test(xml));
  assert.ok(/<weather>/.test(xml) && xml.includes('low fog rolling off the marsh'));
  //    c. the travel graph is internally consistent: every <neighbor> ref is a
  //       known node id. (serializeSimCard slugs both, so this should hold.)
  const nodeIds = [...xml.matchAll(/<node id="([^"]+)">/g)].map((m) => m[1]);
  const neighborIds = [...xml.matchAll(/<neighbor>([^<]+)<\/neighbor>/g)].map((m) => m[1]);
  assert.ok(nodeIds.includes('village-square'));
  assert.ok(nodeIds.includes('marsh-edge'));
  assert.ok(nodeIds.includes('old-chapel'));
  for (const nb of neighborIds) {
    assert.ok(nodeIds.includes(nb), `neighbor "${nb}" has no matching <node id>`);
  }
  //    d. cast: every npc has an id + a <name>.
  const npcIds = [...xml.matchAll(/<npc id="([^"]+)">/g)].map((m) => m[1]);
  assert.ok(npcIds.includes('mara'));
  assert.ok(npcIds.includes('old-perrin'));
  //    e. well-formed-ish: balanced tag pairs we care about.
  for (const tag of ['sim_card', 'metadata', 'identity', 'start', 'locations', 'cast']) {
    const open = xml.split(`<${tag}`).length - 1;
    const close = xml.split(`</${tag}>`).length - 1;
    assert.equal(open, close, `<${tag}> tags balanced (${open} open / ${close} close)`);
  }
  assert.equal(intro, '', 'no intro in this draft');
});

test('pipeline: a two-turn ask→ready accumulation', () => {
  const draft = {};
  // Turn 1: GLM asks a question + gives a partial draft.
  const askEnv = parseEnvelope(JSON.stringify({
    action: 'ask', message: 'Tell me the tone.', questions: ['grim or hopeful?'],
    draft: { name: 'X', directive: 'd' },
  }));
  mergeDraft(draft, askEnv.draft);
  // Turn 2: GLM finalizes, repeating prior fields + adding the rest.
  const readyEnv = parseEnvelope(JSON.stringify({
    action: 'ready',
    draft: { card_type: 'world', name: 'X', directive: 'd', setting: 's', tone: 'grim', start_time: 'Day 1, 08:00', start_weather: 'clear', locations: [{ name: 'Town', neighbors: [] }], cast: [{ name: 'N', identity: 'i' }] },
  }));
  mergeDraft(draft, readyEnv.draft);
  // The accumulator carried name+directive from turn 1; turn 2 filled the rest.
  assert.equal(draft.name, 'X');
  assert.equal(draft.directive, 'd');
  const { xml } = serializeSimCard(draft);
  assert.ok(xml.includes('<name>X</name>'));
  assert.ok(xml.includes('<setting>'));
});

test('pipeline: player envelope → SavedPlayer shape + gender preserved as-typed', () => {
  // 2026-08-13: gender is free-form identity text — no longer force-lowercased.
  const env = parseEnvelope(JSON.stringify({
    action: 'ready',
    draft: { name: 'Kael', gender: 'Nonbinary', race: 'half-elf', clothing: ['cloak', 'boots'], tail: 'long' },
  }));
  const draft = {};
  mergeDraft(draft, env.draft);
  const { id, player } = serializePlayer(draft);
  assert.equal(id, 'kael');
  assert.equal(player.gender, 'Nonbinary');   // preserved, not lowercased
  assert.deepEqual(player.clothing, ['cloak', 'boots']);
  assert.equal(player.tail, 'long');
  assert.ok(!('breast_size' in player));
});

test('pipeline: a malformed GLM reply (no JSON) does not crash the chain', () => {
  const env = parseEnvelope("I'll do my best but here's no JSON at all.");
  assert.equal(env, null);
  // The frontend treats null as "stay in chat, show raw" — the chain simply
  // never serializes. Verify the serializer isn't reached with garbage: an
  // empty draft still serializes to a valid (if bare) card.
  const { xml } = serializeSimCard({});
  assert.ok(xml.includes('<sim_card>'));
});

// ── the 2026-08-15 regression pack ─────────────────────────────────────────
// 1. The exact "Create failed: (a || \"\").trim is not a function" report: GLM
//    emitted ARRAYS for scenario list fields (actors/hazards/outcomes) +
//    numbers elsewhere. parseEnvelope → mergeDraft → serializeSimCard must
//    all survive + produce valid XML.
test('pipeline: hostile-shape ready envelope (arrays/numbers) serializes without crashing', () => {
  const env = parseEnvelope(JSON.stringify({
    action: 'ready',
    draft: {
      card_type: 'scenario', name: 7, directive: 'bandits strike',
      trigger_condition: 'leaving at night', primary_objective: 'survive',
      participating_actors: ['Bandits', 'Toll Guards'],
      environmental_hazards: ['mud'], outcomes: ['richer', 'dead'],
      tone: 'tense', date: '3rd of Harvest', time: 2100,
      weather: 'rain', location: ['the Old Road'],
      intro: ['You walk the road.'],
    },
  }));
  assert.ok(env, 'envelope parsed');
  const draft = {};
  mergeDraft(draft, env.draft);
  const { xml, intro } = serializeSimCard(draft);
  assert.ok(xml.includes('<name>7</name>'));
  assert.ok(xml.includes('Bandits, Toll Guards'));
  assert.equal(intro, 'You walk the road.');
});

// 2. The mandatory gate: a ready missing body_type is REJECTED (never shows
//    the review screen), then GLM's corrective ready completes the draft.
//    This mirrors creator-chat.js handleDone's gate exactly.
import { missingMandatoryFields } from '../src/fable/engine/creator-engine.js';

test('pipeline: ready missing body_type is gated, then the corrective turn fills it', () => {
  const draft = {};
  // Turns 1..n accumulate a nearly-complete player.
  mergeDraft(draft, {
    name: 'Kael', gender: 'male', age: 28, race: 'human',
    skin_complexion: 'tan', height: "6'1\"", weight: '180 lb',
    hair_color: 'black', hair_length: 'short', hair_style: 'messy',
    eye_color: 'green', clothing: ['tunic'],
  });
  // The buggy ready GLM emitted (body_type: null — mergeDraft skips null, so
  // the key never lands; an explicit null passes through Object.assign on the
  // edit path — both must gate identically).
  const buggy = parseEnvelope(JSON.stringify({
    action: 'ready',
    draft: { body_type: null },
  }));
  mergeDraft(draft, buggy.draft);
  assert.deepEqual(missingMandatoryFields('player', draft), ['body_type']);
  // The corrective turn (the alert GLM receives) produces the fill.
  const fix = parseEnvelope(JSON.stringify({
    action: 'ready',
    draft: { body_type: 'lean' },
  }));
  mergeDraft(draft, fix.draft);
  assert.deepEqual(missingMandatoryFields('player', draft), []);
  // CREATE then serializes cleanly.
  const { player } = serializePlayer(draft);
  assert.equal(player.body_type, 'lean');
});

console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
