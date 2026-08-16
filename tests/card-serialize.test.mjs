// Unit tests for the creator serializers (screens/card-serialize.js).
// Plain Node ESM — no test runner. Run: `node tests/card-serialize.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
import { strict as assert } from 'node:assert';
import {
  cdata,
  serializePlayer,
  serializeSimCard,
  codexEntriesToCompound,
  slugify,
  escapeXml,
} from '../src/fable/screens/card-serialize.js';

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

// ── slugify ────────────────────────────────────────────────────────────────
test('slugify: lowercases + hyphenates', () => {
  assert.equal(slugify('The Rusty Tavern'), 'the-rusty-tavern');
  assert.equal(slugify("Mage's  Tower!!"), 'mage-s-tower');
  assert.equal(slugify('   '), '');
});

test('slugify: keeps Unicode letters/digits like Rust (#60)', () => {
  // Rust's slug derivation keeps Unicode alphanumerics; the JS slug must
  // agree or the duplicate-name guard + client id math miss.
  assert.equal(slugify('Café'), 'café');
  // No ASCII folding on either side — lowercase 'Ü' stays 'ü' in Rust too.
  assert.equal(slugify('Ünter der Brücke'), 'ünter-der-brücke');
  assert.equal(slugify('北の王国'), '北の王国');
  // Non-letter symbols still hyphenate.
  assert.equal(slugify('Café ✦ Corner'), 'café-corner');
});

test('slugify: Windows reserved names get a suffix', () => {
  // create_dir_all on Windows fails opaquely for reserved base names — the
  // slug must never land on one.
  assert.equal(slugify('Nul'), 'nul-card');
  assert.equal(slugify('CON'), 'con-card');
  assert.equal(slugify('Com7'), 'com7-card');
  assert.equal(slugify('LPT1'), 'lpt1-card');
  // Only the EXACT base name is reserved — a hyphenated slug is a legal name.
  assert.equal(slugify('LPT1 Device'), 'lpt1-device');
  assert.equal(slugify('Null Void'), 'null-void');
});

// ── escapeXml ──────────────────────────────────────────────────────────────
test('escapeXml: escapes & < >', () => {
  assert.equal(escapeXml('a & b < c > d'), 'a &amp; b &lt; c &gt; d');
});

test('escapeXml: escapes double quotes (attribute contexts)', () => {
  assert.equal(escapeXml('say "hi"'), 'say &quot;hi&quot;');
});

// ── serializePlayer ────────────────────────────────────────────────────────
test('serializePlayer: id from name, gender preserved as-typed', () => {
  // 2026-08-13: gender is free-form identity text — no longer force-lowercased.
  const { id, player } = serializePlayer({ name: 'Kael Brightwood', gender: 'Nonbinary' });
  assert.equal(id, 'kael-brightwood');
  assert.equal(player.id, 'kael-brightwood');
  assert.equal(player.name, 'Kael Brightwood');
  assert.equal(player.gender, 'Nonbinary');
});

test('serializePlayer: conditional traits only when present', () => {
  const { player } = serializePlayer({ name: 'Nyx', tail: 'long + prehensile' });
  assert.equal(player.tail, 'long + prehensile');
  assert.ok(!('breast_size' in player));   // absent → omitted entirely
  assert.ok(!('ears' in player));
});

test('serializePlayer: clothing array carried, blanks dropped', () => {
  const { player } = serializePlayer({ name: 'A', clothing: ['cloak', '  ', 'boots'] });
  assert.deepEqual(player.clothing, ['cloak', 'boots']);
});

test('serializePlayer: new fields (horn/custom_tags/chip lists) emitted when present', () => {
  const { player } = serializePlayer({
    name: 'Nyx', race: 'tiefling', horn: 'curled red', job: 'thief',
    gear: ['compass', '  ', 'rope'],
    custom_tags: { starting_currency: '200 gold', guard_reputation: '-20', blank: '' },
  });
  assert.equal(player.horn, 'curled red');
  assert.equal(player.job, 'thief');
  assert.deepEqual(player.gear, ['compass', 'rope']);              // blanks dropped
  assert.deepEqual(player.custom_tags, { starting_currency: '200 gold', guard_reputation: '-20' }); // blank value dropped
});

test('serializePlayer: new optional fields omitted when absent', () => {
  const { player } = serializePlayer({ name: 'A' });
  for (const k of ['horn', 'job', 'weakness', 'distinguishing_marks', 'gear', 'tools', 'weapons', 'custom_tags']) {
    assert.ok(!(k in player), `${k} should be omitted when absent`);
  }
});

test('serializePlayer: wealth/reputation/fame are NEVER serialized (transient)', () => {
  // These seed PlayerState at attach, never the SavedPlayer identity (§6C).
  const { player } = serializePlayer({ name: 'A', wealth: '200 gold', reputation: '-20', fame: 'known' });
  assert.ok(!('wealth' in player));
  assert.ok(!('reputation' in player));
  assert.ok(!('fame' in player));
});

test('serializePlayer: empty name → fallback id + Unnamed', () => {
  const { id, player } = serializePlayer({});
  assert.equal(id, 'player');
  assert.equal(player.name, 'Unnamed');
});

// ── serializeSimCard ───────────────────────────────────────────────────────
test('serializeSimCard: emits anchors + locations + cast + intro', () => {
  const { xml, intro } = serializeSimCard({
    name: 'Aldermoor', directive: 'survive the bog', setting: 'a dying village',
    tone: 'grim folk horror', start_time: 'Day 1, 09:00', start_weather: 'fog',
    locations: [{ name: 'Village Square', neighbors: ['Marsh Edge'] }],
    cast: [{ name: 'Mara', identity: 'the innkeep' }],
    intro: 'You arrive at dusk.',
  });
  // Roleplay metadata so fable_cards_list surfaces it.
  assert.ok(xml.includes('<type>roleplay</type>'));
  // Anchors.
  assert.ok(xml.includes('<start>'));
  assert.ok(xml.includes('<time>Day 1, 09:00</time>'));
  assert.ok(xml.includes('<weather>'));
  // Travel graph: neighbor ref is the SLUG of the neighbor's name.
  assert.ok(xml.includes('<node id="village-square">'));
  assert.ok(xml.includes('<name>Village Square</name>'));
  assert.ok(xml.includes('<neighbor>marsh-edge</neighbor>'));
  // Cast: npc id is the slug.
  assert.ok(xml.includes('<npc id="mara">'));
  assert.ok(xml.includes('<role>'));
  assert.equal(intro, 'You arrive at dusk.');
});

test('serializeSimCard: omits <start> when no time/weather', () => {
  const { xml } = serializeSimCard({ name: 'X', directive: 'd', setting: 's', tone: 't' });
  assert.ok(!xml.includes('<start>'));
  assert.ok(!xml.includes('<locations>'));
  assert.ok(!xml.includes('<cast>'));
});

test('serializeSimCard: CDATA-wraps prose (directive with special chars)', () => {
  const { xml } = serializeSimCard({ name: 'X', directive: 'a < b & c' });
  assert.ok(xml.includes('<![CDATA[a < b & c]]>'));
});

test('serializeSimCard: embeds <intro> as a sibling AFTER </sim_card>', () => {
  // 2026-08-13: the opening beat lives in-file as <intro> after </sim_card>
  // (kept out of <sim_card> so it never inflates the cached prompt).
  const { xml } = serializeSimCard({
    name: 'Aldermoor', directive: 'survive', intro: 'You arrive at dusk.',
  });
  const closeIdx = xml.indexOf('</sim_card>');
  const introIdx = xml.indexOf('<intro>');
  assert.ok(closeIdx > -1, '</sim_card> present');
  assert.ok(introIdx > closeIdx, '<intro> sits AFTER </sim_card>');
  assert.ok(xml.includes('You arrive at dusk.'));
  // No <intro> emitted when the draft carries none.
  const { xml: xml2 } = serializeSimCard({ name: 'X', directive: 'd' });
  assert.ok(!xml2.includes('<intro>'));
});

test('serializeSimCard: world branch emits subtype + setting + date anchor', () => {
  const { xml } = serializeSimCard({
    card_type: 'world', name: 'Cinderfen', directive: 'a cursed fen', setting: 'a dying village',
    tone: 'grim', date: '3rd of Harvest, Year 1247', time: 'dusk', weather: 'fog', location: 'Cinderfen',
  });
  assert.ok(xml.includes('<type>roleplay</type><subtype>world</subtype>'), 'subtype carried, type stays roleplay');
  assert.ok(xml.includes('<setting>'), 'world → <setting>');
  assert.ok(xml.includes('<date><![CDATA[3rd of Harvest, Year 1247]]></date>'), 'date anchor (CDATA-wrapped)');
  // location synthesized as a single node (no locations graph supplied).
  assert.ok(xml.includes('<node id="cinderfen">'));
});

test('serializeSimCard: npc branch emits appearance + conversational_style + single cast', () => {
  const { xml } = serializeSimCard({
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', hair_color: 'auburn',
    clothing: ['apron'], personality: 'warm', flaws: 'curious', job: 'innkeep',
    dialogue_style: 'cheery', tone: 'cozy', date: 'Day 1', time: '09:00', weather: 'clear', location: 'Tavern',
  });
  assert.ok(xml.includes('<subtype>npc</subtype>'));
  assert.ok(xml.includes('Gender: female'), 'identity → <appearance>');
  assert.ok(xml.includes('Clothing: apron'));
  assert.ok(xml.includes('<conversational_style>'), 'dialogue_style → conversational_style');
  assert.ok(xml.includes('cheery'));
  // Single cast entry = the NPC, role = job.
  assert.ok(xml.includes('<npc id="mara">'));
  assert.ok(xml.includes('innkeep'));
});

test('serializeSimCard: scenario branch composes plot + custom_tags (all branches)', () => {
  const { xml } = serializeSimCard({
    card_type: 'scenario', name: 'Ambush', directive: 'bandits strike',
    trigger_condition: 'leaving at night', primary_objective: 'survive',
    participating_actors: 'bandits, guards', tone: 'tense',
    date: 'Day 2', time: 'night', weather: 'rain', location: 'Road',
    custom_tags: { bandit_count: '8', reward: '50 gold' },
  });
  assert.ok(xml.includes('<subtype>scenario</subtype>'));
  assert.ok(xml.includes('<plot>'), 'scenario → <plot>');
  assert.ok(xml.includes('Trigger: leaving at night'));
  assert.ok(xml.includes('Objective: survive'));
  assert.ok(xml.includes('<custom_tags>'), 'custom_tags emitted');
  assert.ok(xml.includes('<entry key="bandit_count"><![CDATA[8]]></entry>'));
  assert.ok(xml.includes('<entry key="reward"><![CDATA[50 gold]]></entry>'));
});

// ── codexEntriesToCompound ─────────────────────────────────────────────────
test('codexEntriesToCompound: emits front-matter blocks', () => {
  const text = codexEntriesToCompound([
    { title: 'Magic', tags: ['arcane', 'cost'], body: 'Spells cost stamina.' },
  ]);
  assert.ok(text.startsWith('---\ntitle: Magic\ntags: arcane, cost\n---\n\nSpells cost stamina.'));
});

test('codexEntriesToCompound: joins multiple entries with blank line', () => {
  const text = codexEntriesToCompound([
    { title: 'A', body: 'aa' },
    { title: 'B', body: 'bb' },
  ]);
  assert.ok(text.includes('\n\n---\ntitle: B'));
});

test('codexEntriesToCompound: empty → ""', () => {
  assert.equal(codexEntriesToCompound([]), '');
  assert.equal(codexEntriesToCompound(null), '');
});

// ── hostile GLM draft shapes (2026-08-15: the "Create failed: (a || "").trim
//    is not a function" regression — GLM emits numbers/arrays/nulls/objects
//    where the schema says string; the serializers must COERCE, never throw) ──
test('serializeSimCard: scenario arrays (actors/hazards/outcomes) serialize, no crash', () => {
  // The exact crash shape: participating_actors etc. as ARRAYS hit
  // composeLabeled's (prose || '').trim().
  const { xml } = serializeSimCard({
    card_type: 'scenario', name: 'Ambush', directive: 'bandits strike',
    trigger_condition: 'leaving at night', primary_objective: 'survive',
    participating_actors: ['Bandits', 'Toll Guards'],
    environmental_hazards: ['mud', 'darkness'],
    outcomes: ['richer', 'dead'],
    tone: 'tense', date: 'D1', time: '21:00', weather: 'rain', location: 'Road',
    intro: 'You walk into it.',
  });
  assert.ok(xml.includes('Bandits, Toll Guards'));
  assert.ok(xml.includes('<plot>'));
});

test('serializeSimCard: numbers/nulls/objects across every scalar field never throw', () => {
  const { xml, intro } = serializeSimCard({
    card_type: 42, name: 7, directive: { bad: 1 }, setting: ['a', 'b'],
    tone: null, date: true, time: 900, weather: { x: 1 }, location: ['road'],
    intro: ['line one', 'line two'], custom_tags: { k: { v: 1 }, ok: 'yes' },
    cast: ['Mara the innkeep'], locations: ['Village Square'],
  });
  assert.ok(xml.includes('<sim_card>'));
  assert.equal(intro, 'line one, line two');
  // A bare-string cast/location entry is tolerated as {name}.
  assert.ok(xml.includes('village-square'));
  assert.ok(xml.includes('mara-the-innkeep'));
  // Object custom-tag VALUE drops (never "[object Object]"); the clean one stays.
  assert.ok(!xml.includes('[object Object]'));
  assert.ok(xml.includes('yes'));
});

test('serializePlayer: hostile shapes (numeric age, null traits, object custom tag) never throw', () => {
  const { player } = serializePlayer({
    name: 42, age: 28, race: ['human'], body_type: null,
    clothing: 'cloak, boots', custom_tags: { curse: { deep: true }, mood: 'grim' },
  });
  assert.equal(player.name, '42');
  assert.equal(player.age, '28');
  assert.equal(player.race, 'human');
  assert.equal(player.body_type, null);        // null → JSON null (Rust Option::None)
  assert.deepEqual(player.clothing, ['cloak', 'boots']);  // string → chip list
  assert.deepEqual(player.custom_tags, { mood: 'grim' }); // object value dropped
});

test('slugify: numbers/arrays/nulls never throw', () => {
  assert.equal(slugify(42), '42');
  assert.equal(slugify(['A', 'B']), 'a-b');
  assert.equal(slugify(null), '');
  assert.equal(slugify({}), '');
});


// ── cdata() `]]>` segmentation (2026-08-15 audit: zero coverage on the
// only escaping behavior that actually breaks XML) ─────────────────────────

test('cdata: plain prose wraps verbatim', () => {
  assert.equal(cdata('hello world'), '<![CDATA[hello world]]>');
  assert.equal(cdata('smart “quotes” + <literal> & amps'), '<![CDATA[smart “quotes” + <literal> & amps]]>');
});

test('cdata: a `]]>` in prose is segmented, never emitted raw', () => {
  // A raw `]]>` inside a CDATA block terminates it early → broken XML that
  // the server-side parser rejects. The segmenter splits it across two
  // blocks: `]]` + `>` — the ONLY escaping behavior that matters here.
  assert.equal(cdata('a]]>b'), '<![CDATA[a]]]]><![CDATA[>b]]>');
  // Multiple occurrences each get segmented.
  assert.equal(
    cdata('x]]>y]]>z'),
    '<![CDATA[x]]]]><![CDATA[>y]]]]><![CDATA[>z]]>'
  );
  // Round-trip: unwrap the outer block, then un-segment every
  // `]]]]><![CDATA[>` glue point back into the original `]]>`.
  const unwrap = /^<!\[CDATA\[((?:.|\n)*)\]\]>$/;
  const unsegment = (wrapped) => wrapped.replace(unwrap, '$1').replace(/\]\]\]\]><!\[CDATA\[>/g, ']]>');
  assert.equal(unsegment(cdata('a]]>b')), 'a]]>b');
  assert.equal(unsegment(cdata('x]]>y]]>z')), 'x]]>y]]>z');
});

test('cdata: numbers/arrays/nulls coerce safely', () => {
  assert.equal(cdata(42), '<![CDATA[42]]>');
  assert.equal(cdata(null), '<![CDATA[]]>');
});

// ── codexEntriesToCompound front-matter hygiene (2026-08-15 audit fix) ────

test('codexEntriesToCompound: newlines/fences in titles cannot forge entries', () => {
  // A GLM title carrying a `---` fence + `title:` line used to be emitted
  // verbatim into the front-matter — on reconcile the Rust parser split it
  // into MULTIPLE codex entries (a forged lorebook). The forge REQUIRES a
  // second `---` fence line; the sanitizer must make that impossible.
  const hostile = [{
    title: 'Lore\n\n---\ntitle: FORGED\ntags: x',
    tags: ['real tag', 'evil\n---\ntitle: FORGED2'],
    body: 'legitimate body text',
  }];
  const out = codexEntriesToCompound(hostile);
  // Exactly ONE front-matter block (one `---` fence pair) — nothing the
  // parser could split into a forged entry.
  assert.equal((out.match(/^---$/gm) || []).length, 2, `exactly one entry fence pair, got:\n${out}`);
  assert.equal((out.match(/^title: /gm) || []).length, 1, 'exactly one title line');
  assert.equal((out.match(/^tags: /gm) || []).length, 1, 'exactly one tags line');
  // Fence runs in the sanitized title/tags lines are neutralized (an
  // em-dash, not `---`) — the only `---` lines left are the entry's own
  // fences (asserted above).
  assert.ok(!/^title:.*-{3}/m.test(out), `no fence-run survives in the title line: ${out}`);
  assert.ok(!/^tags:.*-{3}/m.test(out), `no fence-run survives in the tags line: ${out}`);
  // The body rides verbatim.
  assert.ok(out.includes('legitimate body text'));
});

test('codexEntriesToCompound: clean titles pass through unchanged', () => {
  const out = codexEntriesToCompound([
    { title: 'The Sunken Temple', tags: ['geography', 'ruins'], body: 'Drowned long ago.' },
  ]);
  assert.equal(
    out,
    '---\ntitle: The Sunken Temple\ntags: geography, ruins\n---\n\nDrowned long ago.'
  );
});

// ── summary ────────────────────────────────────────────────────────────────
console.log('\n%s passed, %s failed', passed, failed);
process.exit(failed === 0 ? 0 : 1);
