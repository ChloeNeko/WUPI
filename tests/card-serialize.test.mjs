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

test('slugify: caps at 64 chars without a trailing dash (2026-08-15 audit fix)', () => {
  // Mirrors the Rust-side cap; keeps <slug>/<slug>.sim well under MAX_PATH.
  // The truncation must not leave a trailing dash (the trim above guaranteed
  // a clean end — the cut must restore that invariant).
  // (2026-08-16 yellow J9) The cap counts CODE POINTS (Rust's
  // cap_slug_chars counts chars) — astral letters pass whole + a cut can
  // never split a surrogate pair, so the old unit-count assertions are
  // retired with the unit-based cut they pinned.
  const long = 'a'.repeat(30) + ' ' + 'b'.repeat(50); // → 30a + '-' + 50b = 81 chars
  const out = slugify(long);
  assert.equal([...out].length, 64);
  assert.ok(!out.endsWith('-'), `trailing dash after truncation: ${out}`);
  assert.ok(out.startsWith('a'.repeat(30) + '-'));
  // An exactly-64 slug passes through untouched.
  assert.equal(slugify('c'.repeat(64)), 'c'.repeat(64));
  // Astral LETTERS (U+1D400 𝐀, category Lu — emoji are symbols and slug to
  // '') count ONE each: 41 code points is under the cap + passes through
  // with pairs intact.
  const astral = 'a' + '𝐀'.repeat(40);
  const eout = slugify(astral);
  assert.equal([...eout].length, 41, `under the cap passes whole, got ${[...eout].length}`);
  assert.ok(!/[\ud800-\udbff]$/.test(eout), 'no dangling high surrogate');
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
  // v2: clothing is the <inventory> sibling seed (the mutable state), not an
  // identity field — it lands on player.inventory.clothing.
  const { player } = serializePlayer({ name: 'A', clothing: ['cloak', '  ', 'boots'] });
  assert.deepEqual(player.inventory.clothing, ['cloak', 'boots']);
});

test('serializePlayer: new fields (horn/custom_tags/inventory) emitted when present', () => {
  const { player } = serializePlayer({
    name: 'Nyx', race: 'tiefling', horn: 'curled red', job: 'thief',
    equipped: ['Notched Iron Broadsword'],
    gear: ['compass', '  ', 'rope'],
    custom_tags: { starting_currency: '200 gold', guard_reputation: '-20', blank: '' },
  });
  assert.equal(player.horn, 'curled red');
  // job is a persona member (the final-question offer) — Occupation line.
  assert.equal(player.persona.occupation, 'thief');
  // equipped rides the inventory sibling seed; legacy gear folds to stored.
  assert.deepEqual(player.inventory.equipped, ['Notched Iron Broadsword']);
  assert.deepEqual(player.inventory.stored, ['compass', 'rope']);
  assert.deepEqual(player.custom_tags, { starting_currency: '200 gold', guard_reputation: '-20' }); // blank value dropped
});

test('serializePlayer: new optional fields omitted when absent', () => {
  const { player } = serializePlayer({ name: 'A' });
  for (const k of ['horn', 'job', 'weakness', 'distinguishing_marks', 'gear', 'tools', 'weapons', 'custom_tags', 'persona', 'inventory']) {
    assert.ok(!(k in player), `${k} should be omitted when absent`);
  }
});

test('serializePlayer: the opt-in persona block (final wizard question)', () => {
  const { player } = serializePlayer({
    name: 'Mira',
    persona: { personality: 'Quiet.', likes: 'Maps, rain.' },
    backstory: 'Raised by cartographers.',
  });
  assert.equal(player.persona.personality, 'Quiet.');
  assert.equal(player.persona.likes, 'Maps, rain.');
  assert.ok(!('occupation' in player.persona), 'unanswered persona fields stay absent');
  assert.equal(player.backstory, 'Raised by cartographers.');
  // No persona + no backstory → the block is omitted ENTIRELY (file + cache).
  const bare = serializePlayer({ name: 'Bare' }).player;
  assert.ok(!('persona' in bare));
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

// ── serializeSimCard (v2: 2026-08-19 Chloe format) ─────────────────────────
test('serializeSimCard: emits metadata + world sibling + location + intro', () => {
  const { xml, intro } = serializeSimCard({
    card_type: 'world', name: 'Aldermoor', directive: 'survive the bog', setting: 'a dying village',
    tone: 'grim folk horror', start_time: 'Day 1, 09:00', start_weather: 'fog',
    location: 'Village Square',
    intro: 'You arrive at dusk.',
  });
  // Simulation metadata (the 2026-08-19 rename of "roleplay").
  assert.ok(xml.includes('<type>simulation</type>'));
  assert.ok(xml.includes('<subtype>world</subtype>'));
  assert.ok(xml.includes('<id>aldermoor</id>'));
  // The anchors are the <world> SIBLING (outside the cached root), never a
  // <start> block inside it.
  assert.ok(!xml.includes('<start>'));
  assert.ok(xml.includes('<world><![CDATA['));
  assert.ok(xml.includes('Time: Day 1, 09:00'));
  assert.ok(xml.includes('Weather: fog'));
  assert.ok(xml.includes('Tone: grim folk horror'));
  assert.ok(xml.includes('<location><![CDATA[\nVillage Square\n]]></location>'));
  // v2: no cast, no locations graph.
  assert.ok(!xml.includes('<cast>'));
  assert.ok(!xml.includes('<locations>'));
  assert.ok(!xml.includes('<node '));
  assert.equal(intro, 'You arrive at dusk.');
});

test('serializeSimCard: omits <world>/<location> when no anchors', () => {
  const { xml } = serializeSimCard({ name: 'X', card_type: 'world', directive: 'd', setting: 's' });
  assert.ok(!xml.includes('<world>'));
  assert.ok(!xml.includes('<location>'));
  assert.ok(!xml.includes('<cast>'));
});

// (2026-08-16 yellow J6) GLM drift: the draft may carry `clothing` as a
// comma STRING — the NPC inventory path splits it like the chip lists.
test('serializeSimCard: NPC clothing as comma string is split into the inventory', () => {
  const { xml } = serializeSimCard({
    name: 'Mara', card_type: 'npc', directive: 'd',
    clothing: 'leather armor, traveling cloak,boots',
  });
  assert.ok(
    xml.includes('Clothing: leather armor, traveling cloak, boots'),
    `clothing line present: ${xml}`
  );
  assert.ok(xml.includes('<inventory>'), 'the line rides the <inventory> sibling');
});

// (2026-08-16 yellow J9) The 64 cap counts CODE POINTS, matching Rust's
// cap_slug_chars — an astral-letter name must derive the SAME slug the
// server would (the old UTF-16-unit cut split surrogate pairs).
test('slugify: astral letters cap at 64 code points, pairs never split', () => {
  const astral = '𝕏'.repeat(70); // 70 chars, 140 UTF-16 units
  const slug = slugify(astral);
  assert.equal([...slug].length, 64, 'exactly 64 code points survive');
  assert.ok(!slug.includes('\uFFFD'), 'no replacement chars from split pairs');
  // A mixed name under the cap passes through untouched.
  assert.equal(slugify('Café Örn'), 'café-örn');
});

test('serializeSimCard: CDATA-wraps prose (setting with special chars)', () => {
  const { xml } = serializeSimCard({ name: 'X', card_type: 'world', setting: 'a < b & c' });
  // The v2 wrap indents the body (the reference-card layout): CDATA opens,
  // newline, the prose, then the indented close.
  assert.ok(xml.includes('<![CDATA[\na < b & c\n  ]]>'), `setting CDATA-wrapped: ${xml}`);
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

test('serializeSimCard: world branch emits subtype + setting + world sibling', () => {
  const { xml } = serializeSimCard({
    card_type: 'world', name: 'Cinderfen', directive: 'a cursed fen', setting: 'a dying village',
    tone: 'grim', date: '3rd of Harvest, Year 1247', time: 'dusk', weather: 'fog', location: 'Cinderfen',
  });
  assert.ok(xml.includes('<type>simulation</type>'), 'type is simulation (the rename)');
  assert.ok(xml.includes('<subtype>world</subtype>'), 'subtype carried');
  assert.ok(xml.includes('<setting>'), 'world → <setting>');
  // The date anchor rides the <world> sibling — NOT a <start> block.
  assert.ok(xml.includes('Date: 3rd of Harvest, Year 1247'));
  assert.ok(xml.includes('Tone: grim'));
  // No locations graph — the single <location> sibling is the opening place.
  assert.ok(xml.includes('<location><![CDATA[\nCinderfen\n]]></location>'));
  assert.ok(!xml.includes('<node '));
});

test('serializeSimCard: npc branch emits identity/persona line blocks + inventory sibling, NO cast', () => {
  const { xml } = serializeSimCard({
    card_type: 'npc', name: 'Mara', gender: 'female', race: 'human', hair_color: 'auburn',
    clothing: ['apron'], accessories: ['silver locket'], gear: ['compass'],
    personality: 'warm', flaws: 'curious', likes: 'river songs',
    dislikes: 'nobles', job: 'innkeep', backstory: 'exile', dialogue_style: 'cheery',
    goal: 'buy the tavern', tone: 'cozy', date: 'Day 1', time: '09:00', weather: 'clear', location: 'Tavern',
  });
  assert.ok(xml.includes('<subtype>npc</subtype>'));
  // v2: identity + persona are CDATA LINE BLOCKS inside the cached root.
  assert.ok(xml.includes('Name: Mara'));
  assert.ok(xml.includes('Gender: female'));
  assert.ok(xml.includes('Hair Color: auburn'));
  assert.ok(xml.includes('Conversation Style: cheery'));
  assert.ok(xml.includes('Likes: river songs'));
  assert.ok(xml.includes('Dislikes: nobles'));
  assert.ok(xml.includes('Occupation: innkeep'));
  assert.ok(xml.includes('Goals: buy the tavern'), 'goal → the Goals persona line');
  assert.ok(xml.includes('Backstory: exile'));
  // The inventory sibling: clothing + accessories + stored (legacy gear).
  assert.ok(xml.includes('Clothing: apron'));
  assert.ok(xml.includes('Accessories: silver locket'));
  assert.ok(xml.includes('Stored: compass'), 'legacy gear folds to Stored');
  // Tone rides the <world> sibling (a world anchor, never card-level).
  assert.ok(xml.includes('Tone: cozy'));
  assert.ok(!xml.includes('<tone>'));
  // NPC cards carry NO <cast> — the character IS the card.
  assert.ok(!xml.includes('<cast>'), 'npc cards emit no cast roster');
  assert.ok(!xml.includes('<npc id='), 'no synthesized cast entry');
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
  assert.ok(xml.includes('Premise: bandits strike'));
  assert.ok(xml.includes('Trigger: leaving at night'));
  assert.ok(xml.includes('Objective: survive'));
  assert.ok(xml.includes('<custom_tags>'), 'custom_tags emitted');
  assert.ok(xml.includes('<entry key="bandit_count"><![CDATA[8]]></entry>'));
  assert.ok(xml.includes('<entry key="reward"><![CDATA[50 gold]]></entry>'));
});

// v2: the npc conditional traits (breast/ears/tail/horn) have no identity
// line — they ride custom_tags (mirroring the Rust legacy mapping).
test('serializeSimCard: npc conditional traits ride custom_tags', () => {
  const { xml } = serializeSimCard({
    card_type: 'npc', name: 'Nyx', race: 'tiefling', horn: 'curled red', tail: 'whip-thin',
    custom_tags: { faction: 'guild' },
  });
  assert.ok(xml.includes('<entry key="horn">'), 'horn → custom tag');
  assert.ok(xml.includes('<entry key="tail">'), 'tail → custom tag');
  assert.ok(xml.includes('<entry key="faction">'), 'authored tags survive');
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

// (2026-08-16 audit follow-up) Body-fence guard: the Rust parser splits on a
// blank-preceded `---` + `title:` line, so that exact shape inside a lore
// body forges an extra entry at the next parse. Only the COLLIDING fence is
// neutralized (→ em-dash); ordinary `---` rules pass through verbatim.
test('codexEntriesToCompound: neutralizes a body fence that would forge an entry', () => {
  const text = codexEntriesToCompound([
    {
      title: 'The Schism',
      body: 'Ancient records quote the scroll:\n\n---\ntitle: Forged Treaty\n---\n\nTreaty prose.',
    },
  ]);
  assert.ok(!text.includes('---\ntitle: Forged Treaty'), 'colliding fence neutralized');
  assert.ok(text.includes('—\ntitle: Forged Treaty'), 'replaced with the em-dash glyph');
  assert.ok(text.includes('Treaty prose.'), 'tail prose survives');
  // The entry's own real front-matter fences are untouched.
  assert.ok(text.startsWith('---\ntitle: The Schism\n'));
});

test('codexEntriesToCompound: plain body --- rules pass through verbatim', () => {
  const body = 'Section one.\n\n---\n\nSection two after a rule.';
  const text = codexEntriesToCompound([{ title: 'Rule', body }]);
  assert.ok(text.includes(body), 'no following title: line → untouched');
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
  // v2: a hostile `name` coerces safely; cast/locations are IGNORED entirely
  // (no graph, no roster in the format).
  assert.ok(!xml.includes('[object Object]'));
  assert.ok(xml.includes('yes'));
  assert.ok(!xml.includes('<cast>'));
  assert.ok(!xml.includes('<node '));
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
  assert.deepEqual(player.inventory.clothing, ['cloak', 'boots']);  // string → chip list (v2 inventory seed)
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
