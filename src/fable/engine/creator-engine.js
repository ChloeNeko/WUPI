// =============================================================
// CREATOR ENGINE — the DOM-free pure logic behind the GLM-driven
// creation wizards (Phase 6 extraction, mirroring the drawer-logic.js
// precedent). No Tauri, no DOM — safe to unit-test under plain Node.
//
// Three concerns live here:
//   1. Envelope parsing + draft accumulation (the ask/ready contract the
//      creator_assistant_turn IPC returns).
//   2. The review-section model (kind-aware field → [label,value] rows).
//   3. The SillyTavern import parse primitives (PNG chunk walk + JSON
//      normalize + lorebook conversion), extracted from st-import.js so
//      they're testable + so the importer + the codex-import path share
//      one implementation.
// =============================================================

// --- Envelope parsing -----------------------------------------------------

// Robustly extract the JSON envelope from GLM's reply. GLM is told to emit ONE
// JSON object + nothing else, but defensively strip a ```json fence and, on a
// full-document parse failure, fall back to the first `{` … last `}` slice.
// Returns { action, message, questions, draft } or null.
export function parseEnvelope(text) {
  if (!text) return null;
  let s = text.trim();
  const fence = s.match(/```(?:json)?\s*([\s\S]*?)```/i);
  if (fence) s = fence[1].trim();
  const tryParse = (str) => {
    try { return JSON.parse(str); } catch (_) { return null; }
  };
  let obj = tryParse(s);
  if (!obj) {
    const a = s.indexOf('{');
    const b = s.lastIndexOf('}');
    if (a !== -1 && b > a) obj = tryParse(s.slice(a, b + 1));
  }
  if (!obj || typeof obj !== 'object') return null;
  return obj;
}

// When no envelope parsed, surface something readable (strip fences; prefer a
// nested message if present).
export function stripToJsonFallback(text) {
  const env = parseEnvelope(text);
  if (env && env.message) return env.message;
  return (text || '').replace(/```(?:json)?/gi, '').trim();
}

// Merge a partial draft into the accumulating draft (shallow; arrays replace).
// Empty/null/undefined values are skipped so GLM can't clobber a decided field
// with a blank on a later turn.
export function mergeDraft(dst, src) {
  for (const [k, v] of Object.entries(src || {})) {
    if (v === null || v === undefined || v === '') continue;
    // (2026-08-16 audit M7) Prototype-pollution guard: JSON.parse makes
    // `__proto__` an own enumerable key, and `dst[k]=v` assignment (unlike
    // defineProperty) routes through the inherited setter — model-controlled
    // data swapping the draft's prototype. Skip the whole danger trio.
    if (k === '__proto__' || k === 'constructor' || k === 'prototype') continue;
    dst[k] = v;
  }
  return dst;
}

// toText: coerce ANY GLM draft value to a clean string (the same discipline as
// card-serialize.js::text — arrays join, numbers stringify, booleans/objects/
// null collapse to ''). Kept local so this DOM-free module has zero imports.
// Every value pulled out of a draft flows through this — GLM emits numbers,
// arrays, and nulls where the schema says string, and a raw (v || '').trim()
// on those throws.
function toText(v) {
  if (v == null || typeof v === 'boolean') return '';
  if (Array.isArray(v)) return v.map(toText).filter(Boolean).join(', ');
  if (typeof v === 'object') return '';
  return String(v).trim();
}

// prettyTagKey: custom-tag KEYS render as spaced, capitalized words on every
// visual surface (2026-08-20, Chloe — "custom tags still shows up with an
// underscore for the visual aspect"). DISPLAY ONLY: the draft/map keeps the
// raw snake_case key (it round-trips into the XML `entry key="…"`).
function prettyTagKey(k) {
  return String(k)
    .replace(/[-_]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/(^|\s)([a-z])/g, (m, a, b) => a + b.toUpperCase());
}

// --- Review-section model -------------------------------------------------

// Build the review card's [title, [[label, value], ...]] section list for a
// kind/draft. Values are RAW (escaping is the renderer's job). Empty rows +
// empty sections are filtered out. `row` returns null for absent values so the
// filter chain drops them.
export function buildReviewSections(kind, d) {
  const row = (label, v) => (toText(v) ? [label, toText(v)] : null);
  if (kind === 'player') {
    const customRows = d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)
      ? Object.entries(d.custom_tags)
          .map(([k, v]) => row(prettyTagKey(k), v))
          .filter(Boolean)
      : [];
    // The v2 inventory seed (the optional sibling) — accepts either the
    // structured inventory object or the legacy flat fields.
    const inv = d.inventory && typeof d.inventory === 'object' && !Array.isArray(d.inventory) ? d.inventory : {};
    const persona = d.persona && typeof d.persona === 'object' && !Array.isArray(d.persona) ? d.persona : {};
    return [
      ['Identity', [row('Name', d.name), row('Gender', d.gender), row('Race', d.race), row('Age', d.age)].filter(Boolean)],
      ['Appearance', [row('Height', d.height), row('Weight', d.weight), row('Build', d.body_type), row('Skin', d.skin_complexion), row('Eyes', d.eye_color)].filter(Boolean)],
      ['Hair', [row('Color', d.hair_color), row('Length', d.hair_length), row('Style', d.hair_style)].filter(Boolean)],
      ['Distinctive', [row('Breast', d.breast_size), row('Ears', d.ears), row('Tail', d.tail), row('Horn', d.horn)].filter(Boolean)],
      ['Inventory', [
        row('Clothing', inv.clothing != null ? inv.clothing : d.clothing),
        row('Equipped', inv.equipped != null ? inv.equipped : d.equipped),
        row('Accessories', inv.accessories != null ? inv.accessories : d.accessories),
        row('Stored', inv.stored != null ? inv.stored : d.gear),
      ].filter(Boolean)],
      // The OPT-IN persona block (2026-08-19: the wizard's final question —
      // absent entirely when the player declined).
      ['Persona', [
        row('Personality', persona.personality != null ? persona.personality : d.personality),
        row('Likes', persona.likes != null ? persona.likes : d.likes),
        row('Dislikes', persona.dislikes != null ? persona.dislikes : d.dislikes),
        row('Flaws', persona.flaws != null ? persona.flaws : d.flaws),
        row('Goals', persona.goals != null ? persona.goals : d.goal),
        row('Occupation', persona.occupation != null ? persona.occupation : d.job),
        row('History', d.backstory),
      ].filter(Boolean)],
      // Starting conditions are TRANSIENT (seed PlayerState at attach, never
      // persisted on the SavedPlayer) but surface here so the player sees
      // what was captured. Absent on edited/loaded players → section dropped.
      ['Starting conditions', [row('Wealth', d.wealth), row('Reputation', d.reputation || d.fame)].filter(Boolean)],
      ['Custom tags', customRows],
    ].filter(([, rows]) => rows.length);
  }
  if (kind === 'sim') {
    const subtype = toText(d.card_type).trim();
    const customRows = d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)
      ? Object.entries(d.custom_tags).map(([k, v]) => row(prettyTagKey(k), v)).filter(Boolean)
      : [];
    const sections = [
      ['Card', [row('Type', subtype), row('Name', d.name)].filter(Boolean)],
      // Tone is the story's tone — a world anchor like the date/weather (it
      // seeds the tracker, rendered beside them every turn).
      ['World anchors', [row('Date', d.date), row('Time', d.time || d.start_time), row('Weather', d.weather || d.start_weather), row('Tone', d.tone), row('Location', d.location)].filter(Boolean)],
    ];
    // Branch-specific sections (per the SIM Wizard Type Router spec).
    if (subtype === 'npc') {
      sections.push(['Identity', [row('Gender', d.gender), row('Race', d.race), row('Age', d.age), row('Height', d.height), row('Weight', d.weight), row('Build', d.body_type), row('Skin', d.skin_complexion), row('Eyes', d.eye_color)].filter(Boolean)]);
      sections.push(['Hair', [row('Color', d.hair_color), row('Length', d.hair_length), row('Style', d.hair_style)].filter(Boolean)]);
      sections.push(['Distinctive', [row('Breast', d.breast_size), row('Ears', d.ears), row('Tail', d.tail), row('Horn', d.horn)].filter(Boolean)]);
      // The full persona set (every label mandatory for npc cards — the
      // character IS the card).
      sections.push(['Persona', [row('Personality', d.personality), row('Dialogue style', d.dialogue_style), row('Likes', d.likes), row('Dislikes', d.dislikes), row('Flaws', d.flaws), row('Goals', d.goal), row('Occupation', d.job), row('Backstory', d.backstory)].filter(Boolean)]);
      // The npc `<inventory>` sibling (Clothing mandatory, the rest optional).
      sections.push(['Inventory', [
        row('Clothing', d.clothing),
        row('Equipped', d.equipped),
        row('Accessories', d.accessories),
        row('Stored', d.stored != null ? d.stored : d.gear),
      ].filter(Boolean)]);
    } else if (subtype === 'scenario') {
      sections.push(['Scenario', [row('Premise', d.directive), row('Trigger', d.trigger_condition), row('Objective', d.primary_objective), row('Actors', d.participating_actors), row('Hazards', d.environmental_hazards), row('Outcomes', d.outcomes)].filter(Boolean)]);
    } else {
      // world, or pre-router/unknown: show the world-style fields that exist.
      sections.push(['World', [row('Purpose', d.directive), row('Setting', d.setting)].filter(Boolean)]);
    }
    if (customRows.length) sections.push(['Custom tags', customRows]);
    // The intro the SIM Wizard gathered (its mandatory question — 2026-08-15
    // the intro is the wizard's job, not a post-card step). Absent → dropped.
    if (toText(d.intro)) sections.push(['Intro', [['Text', toText(d.intro)]]]);
    return sections.filter(([, rows]) => rows.length);
  }
  if (kind === 'codex') {
    const entries = Array.isArray(d.entries) ? d.entries : [];
    return entries.map((e, i) => {
      const bodyRow = row('Body', e.body);
      return [e.title || `Entry ${i + 1}`, [row('Tags', e.tags), bodyRow].filter(Boolean)];
    }).filter(([, rows]) => rows.length);
  }
  return [];
}

// --- ID-card model (NAME header + license grid + card-icon disclosure) -----

// The ID card splits a draft into a HEADER (the player/NPC NAME in caps, or
// the WORLD/SCENARIO CARD type banner — centered over the information space
// with a gold rule fitted to the text width), a COMPACT license grid ("core"),
// and an "extra" disclosure of every remaining tag held in an inert
// <template> + revealed by the corner card-icon button.
//
// Core layout (player/NPC, 2026-08-20 Chloe):
//   RACE | GENDER
//   AGE  | EYE COLOR
//   HEIGHT | WEIGHT
// The face carries ONLY those six license rows — skin, body, and hair moved
// behind the details button (the Appearance extra; hair renders as bare
// stacked lines, no Color/Length/Style labels). World/Scenario keep a
// 1-column core (Name/Setting/Purpose/Tone) under the TYPE banner. codex is
// NOT an ID card → returns null (it keeps the generic review-section
// rendering, so buildReviewSections + its tests stay intact).
//
// Core cells: { label, value }. Values are RAW (escaping is the renderer's
// job).

function idRow(label, v) {
  const s = toText(v);
  return s ? [label, s] : null;
}

function customTagRows(d) {
  return d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)
    ? Object.entries(d.custom_tags).map(([k, v]) => idRow(prettyTagKey(k), v)).filter(Boolean)
    : [];
}

// A plain core cell (dropped when the value is empty).
function idCell(label, v) {
  const s = toText(v);
  return s ? { label, value: s } : null;
}

// The HAIR row (2026-08-20 Chloe): the three values stacked WITHOUT their
// Color/Length/Style sub-labels — newline-joined; the details popup's dd
// renders pre-line so each value is its own line.
function hairValueRow(d) {
  const vals = [toText(d.hair_color), toText(d.hair_length), toText(d.hair_style)].filter(Boolean);
  return vals.length ? ['Hair', vals.join('\n')] : null;
}

// Returns { variant, title, banner, core, extra } or null for non-ID kinds.
//   variant: 'player' | 'world'
//   title:   the NAME header (player/NPC only — uppercase is a renderer concern)
//   banner:  null (retired — the TYPE banner is now the small subheader `tag`)
//   tag:     'PLAYER CARD' | 'NPC CARD' | 'WORLD CARD' | 'SCENARIO CARD'
//            (small darker-bronze subheader under the name rule, 2026-08-20)
//   core:    cell objects (above) in render order
//   extra:   [[title, [[label, value], ...]], ...]   revealed by the card-icon
export function buildIdCard(kind, d) {
  const draft = d || {};
  if (kind === 'player') return playerIdCard(draft, false);
  if (kind === 'sim') {
    const subtype = toText(draft.card_type).toLowerCase();
    if (subtype === 'npc') return playerIdCard(draft, true);
    if (subtype === 'scenario') return worldIdCard(draft, 'SCENARIO CARD');
    return worldIdCard(draft, 'WORLD CARD'); // world, pre-router, or unknown
  }
  return null; // codex is not an ID card
}

// (2026-08-20 audit) Authored holdings — draft.properties (the Creator's
// optional starting-holdings field; worlds.js parseSimDraft parses the
// `<properties>` sibling back into it on load). One row per property,
// label = the id.
function holdingsRows(d) {
  if (!Array.isArray(d.properties)) return [];
  const rows = [];
  for (const p of d.properties) {
    if (!p || typeof p !== 'object') continue;
    const id = toText(p.id).trim();
    const node = toText(p.node).trim();
    if (!id || !node) continue;
    const rev = Math.max(0, Math.floor(Number(p.revenue) || 0));
    const up = Math.max(0, Math.floor(Number(p.upkeep) || 0));
    const bits = [`@ ${node}`, toText(p.kind).trim() || 'business', `${rev}/day revenue`, `${up}/day upkeep`];
    const owner = toText(p.owner).trim();
    if (owner) bits.push(`owner ${owner}`);
    const price = Math.floor(Number(p.price));
    // (2026-08-21 currency addendum) No hardcoded unit — the world's
    // currency is learned in play, so the authored preview shows the naked
    // base-unit number.
    if (Number.isFinite(price) && price > 0) bits.push(`price ${price}`);
    rows.push([id, bits.join(', ')]);
  }
  return rows;
}

// Player + NPC share the license face; the only difference is the subheader
// tag (PLAYER CARD / NPC CARD) under the name rule. The face is the six
// license rows in paired rows; skin/body/hair moved into the extra
// disclosure as the leading Appearance section, so the popup carries:
// Appearance, Distinctive, the v2 Inventory seed, the opt-in Persona block,
// transient Starting conditions, Custom tags, and (NPC) the world anchors +
// intro.
function playerIdCard(d, isNpc) {
  const title = toText(d.name);
  const core = [
    idCell('Race', d.race),
    idCell('Gender', d.gender),
    idCell('Age', d.age),
    idCell('Eye Color', d.eye_color),
    idCell('Height', d.height),
    idCell('Weight', d.weight),
  ].filter(Boolean);
  const inv = d.inventory && typeof d.inventory === 'object' && !Array.isArray(d.inventory) ? d.inventory : {};
  const persona = d.persona && typeof d.persona === 'object' && !Array.isArray(d.persona) ? d.persona : {};
  const pGet = (k, fallback) => (persona[k] != null ? persona[k] : d[fallback]);
  const extra = [
    // The SIM anchors are mandatory for EVERY branch — and on sim cards the
    // World section leads the details popup (2026-08-20 Chloe). Absent on
    // plain players (no anchors) → section drops.
    // 2026-08-20 Chloe: titled "World" (was "World anchors") + the Intro row
    // carries NO label (was "Text") — bare paragraph rows.
    ['World', [idRow('Date', d.date), idRow('Time', d.time || d.start_time), idRow('Weather', d.weather || d.start_weather), idRow('Tone', d.tone), idRow('Location', d.location)].filter(Boolean)],
    ['Appearance', [
      idRow('Skin', d.skin_complexion),
      idRow('Body', d.body_type),
      hairValueRow(d),
    ].filter(Boolean)],
    ['Distinctive', [idRow('Breast', d.breast_size), idRow('Ears', d.ears), idRow('Tail', d.tail), idRow('Horn', d.horn)].filter(Boolean)],
    ['Inventory', [
      idRow('Clothing', inv.clothing != null ? inv.clothing : d.clothing),
      idRow('Equipped', inv.equipped != null ? inv.equipped : d.equipped),
      idRow('Accessories', inv.accessories != null ? inv.accessories : d.accessories),
      idRow('Stored', inv.stored != null ? inv.stored : d.gear),
    ].filter(Boolean)],
    ['Holdings', holdingsRows(d)],
    // The OPT-IN persona block (2026-08-19) — absent entirely when the
    // player declined the final wizard question.
    ['Persona', [
      idRow('Personality', pGet('personality', 'personality')),
      // NPC cards carry a Conversation Style persona line; players never get
      // the field, so the row self-omits for them.
      idRow('Dialogue style', d.dialogue_style),
      idRow('Likes', pGet('likes', 'likes')),
      idRow('Dislikes', pGet('dislikes', 'dislikes')),
      idRow('Flaws', pGet('flaws', 'flaws')),
      idRow('Goals', pGet('goals', 'goal')),
      idRow('Occupation', pGet('occupation', 'job')),
      idRow('Backstory', d.backstory),
    ].filter(Boolean)],
    // Starting conditions are TRANSIENT (seed PlayerState at attach, never
    // persisted on the SavedPlayer) but surface here so the player sees what
    // was captured. Absent on edited/loaded players → section dropped.
    ['Starting conditions', [idRow('Wealth', d.wealth), idRow('Reputation', d.reputation || d.fame)].filter(Boolean)],
    ['Custom tags', customTagRows(d)],
    // The intro the SIM Wizard gathered for an NPC card (mandatory question —
    // 2026-08-15). Player cards never carry one → dropped.
    ...(toText(d.intro) ? [['Intro', [['', toText(d.intro)]]]] : []),
  ].filter(([, rows]) => rows.length);
  return { variant: 'player', title, banner: null, tag: isNpc ? 'NPC CARD' : 'PLAYER CARD', core, extra };
}

// World + Scenario share the face: the NAME is the header (2026-08-20 Chloe —
// was the TYPE banner) with the card type as the small subheader tag beneath,
// over a 1-column core (Setting/Purpose/Tone — the Name cell moved into the
// header). directive is shown as "Purpose". The scenario specifics, world
// anchors, custom tags, and the intro live in the disclosure (no
// cast/locations in the v2 format — they emerge in play).
function worldIdCard(d, tag) {
  const core = [
    idCell('Setting', d.setting),
    idCell('Purpose', d.directive),   // directive shown as "Purpose"
    idCell('Tone', d.tone),
  ].filter(Boolean);
  const extra = [
    ['World', [idRow('Date', d.date), idRow('Time', d.time || d.start_time), idRow('Weather', d.weather || d.start_weather), idRow('Tone', d.tone), idRow('Location', d.location)].filter(Boolean)],
  ];
  if (tag === 'SCENARIO CARD') {
    extra.push(['Scenario', [idRow('Premise', d.directive), idRow('Trigger', d.trigger_condition), idRow('Objective', d.primary_objective), idRow('Actors', d.participating_actors), idRow('Hazards', d.environmental_hazards), idRow('Outcomes', d.outcomes)].filter(Boolean)]);
  }
  extra.push(['Holdings', holdingsRows(d)]);
  extra.push(['Custom tags', customTagRows(d)]);
  // The intro the SIM Wizard gathered (mandatory question). Absent → dropped.
  // 2026-08-20 Chloe: bare paragraph row (no "Text" label).
  if (toText(d.intro)) extra.push(['Intro', [['', toText(d.intro)]]]);
  return { variant: 'world', title: toText(d.name) || tag, banner: null, tag, core, extra: extra.filter(([, rows]) => rows.length) };
}

// --- Mandatory-field gate (2026-08-15 Chloe) --------------------------------
//
// A `ready` draft missing ANY mandatory field must NEVER reach the review
// screen — the review card is the human-in-the-loop contract that the card is
// complete. The lists mirror build_creator_assistant_system_prompt's schema
// exactly (player CORE FIELDS, sim per-branch mandatory sets, codex entries);
// body_type was promoted to mandatory 2026-08-15 after a player card was
// created with "body_type": null.

export const MANDATORY_LABELS = {
  name: 'name', gender: 'gender', age: 'age', race: 'race',
  skin_complexion: 'skin', height: 'height', weight: 'weight',
  body_type: 'body (build)', hair_color: 'hair color',
  hair_length: 'hair length', hair_style: 'hair style', eye_color: 'eye color',
  clothing: 'clothing',
  card_type: 'card type', directive: 'directive', setting: 'setting',
  trigger_condition: 'trigger', primary_objective: 'objective',
  participating_actors: 'actors',
  personality: 'personality', flaws: 'flaws', job: 'job/occupation',
  backstory: 'backstory', dialogue_style: 'dialogue style',
  likes: 'likes', dislikes: 'dislikes', goal: 'goal',
  date: 'date anchor', time: 'time anchor', weather: 'weather anchor',
  tone: 'tone anchor', location: 'location anchor',
  intro_answer: 'the intro question',
  items_answer: 'the inventory question',
  persona_answer: 'the persona question',
  entries: 'at least one lore entry (title + body)',
};

// The player identity set (2026-08-19 v2): the 11 `<identity>` trait lines —
// clothing is NOT mandatory for players (their whole `<inventory>` sibling is
// optional). The SIM npc branch adds it back (an npc card's inventory block
// always carries a Clothing line).
const MANDATORY_IDENTITY = [
  'name', 'gender', 'age', 'race', 'skin_complexion', 'height', 'weight',
  'body_type', 'hair_color', 'hair_length', 'hair_style', 'eye_color',
];
// The universal world anchors — every sim branch, ALL mandatory. tone joined
// the set 2026-08-19 (it seeds the world tracker + renders beside the
// time/weather every turn).
const SIM_ANCHORS = ['date', 'time', 'weather', 'tone', 'location'];

function mandatoryKeys(kind, d) {
  if (kind === 'player') return MANDATORY_IDENTITY;
  if (kind === 'sim') {
    const subtype = toText(d && d.card_type).toLowerCase();
    if (subtype === 'npc') {
      return [
        ...MANDATORY_IDENTITY,
        'clothing',
        // The FULL persona set — every label mandatory for npc cards (the
        // character IS the card; 2026-08-19 v2: goals/dialogue_style are
        // persona members, tone is a world anchor above). The draft key for
        // the Goals line is `goal`.
        'personality', 'dialogue_style', 'likes', 'dislikes', 'flaws',
        'goal', 'job', 'backstory',
        ...SIM_ANCHORS,
      ];
    }
    if (subtype === 'scenario') {
      return [
        'card_type', 'name', 'directive', 'trigger_condition',
        'primary_objective', 'participating_actors', ...SIM_ANCHORS,
      ];
    }
    // world, pre-router, or unknown — the router itself must complete first.
    return ['card_type', 'name', 'directive', 'setting', ...SIM_ANCHORS];
  }
  return []; // codex's only mandatory is the entries array (checked below)
}

// A mandatory value counts as filled when it coerces to content: non-empty
// arrays, non-blank strings, numbers (GLM emits "age": 24). Booleans, null,
// objects, blanks, and empty arrays do NOT count.
function mandatoryFilled(v) {
  if (Array.isArray(v)) return v.length > 0;
  if (v === null || v === undefined || v === '' || typeof v === 'boolean') return false;
  if (typeof v === 'object') return false;
  return String(v).trim() !== '';
}

// ── Pure CREATE/retry decisions (extracted 2026-08-15) ─────────────────────
// The DOM-coupled creator-chat screen kept these inline + untestable; the
// drawer-logic.js precedent: the DECISION is pure, the screen only wires it.

// Retry cap shared by the codex-embed validation loop + the mandatory-field
// gate. Attempts are counted AFTER increment (1st retry = 1): 1..2 retry,
// 3+ exhausts + surfaces the gap to the user.
export const MAX_CREATOR_RETRIES = 2;
export function creatorRetryAllowed(attempts) {
  return attempts <= MAX_CREATOR_RETRIES;
}

// Duplicate-name guard (both write IPCs are silent atomic overwrites — a
// CREATE reusing an existing slug replaces authored content). REJECT when
// the write target collides with an existing id, EXCEPT when the target IS
// the seeded edit-run entity's own id (re-saving itself). A pencil-edit that
// RENAMES onto a different existing slug must still be caught (M3's rename
// hole: the write target is re-derived from the possibly-renamed draft).
export function shouldRejectDuplicateName(target, seededId, existingIds) {
  if (target === seededId) return false;
  return existingIds.includes(target);
}

// Returns the list of MISSING mandatory field keys for kind/draft ([] = the
// draft may finalize). Three questions share the explicit-answer shape (an
// absence is incomplete — the wizard must have asked):
//   - the sim INTRO question (agreed text or intro_answered:false),
//   - the npc INVENTORY question (clothing is mandatory-fill; the optional
//     equipped/accessories/stored lines may be empty but only with the
//     items_answered:false marker),
//   - the PLAYER PERSONA question (2026-08-19, the final wizard question —
//     persona fields may be absent only with persona_answered:false).
// Codex requires ≥1 entry with a body.
export function missingMandatoryFields(kind, d) {
  const draft = d || {};
  const missing = mandatoryKeys(kind, draft).filter((k) => !mandatoryFilled(draft[k]));
  if (kind === 'sim' && missing.length === 0) {
    // The mandatory INTRO question: either a non-empty intro or an explicit
    // no-intro marker (intro_answered === false, set when the player declines).
    if (!toText(draft.intro) && draft.intro_answered !== false) missing.push('intro_answer');
    // The npc INVENTORY question: clothing is already mandatory-filled above;
    // the optional lines may all be empty ONLY with the explicit decline
    // marker (an absence with no marker means the wizard never asked).
    const subtype = toText(draft.card_type).toLowerCase();
    if (subtype === 'npc'
      && !['equipped', 'accessories', 'stored', 'gear'].some((k) => mandatoryFilled(draft[k]))
      && draft.items_answered !== false) {
      missing.push('items_answer');
    }
  }
  if (kind === 'player' && missing.length === 0) {
    // The mandatory FINAL persona question: any persona content counts as
    // answered; an absent persona needs the explicit decline marker.
    const hasPersona = ['personality', 'likes', 'dislikes', 'flaws', 'goals', 'goal', 'job', 'backstory']
      .some((k) => mandatoryFilled(
        draft.persona && typeof draft.persona === 'object' && draft.persona[k] != null
          ? draft.persona[k]
          : draft[k],
      ));
    if (!hasPersona && draft.persona_answered !== false) missing.push('persona_answer');
  }
  if (kind === 'codex') {
    const entries = Array.isArray(draft.entries) ? draft.entries : [];
    const ok = entries.some((e) => e && toText(e.body));
    if (!ok) missing.push('entries');
  }
  return missing;
}

// --- SillyTavern import primitives (pure) ---------------------------------

function decodeLatin1(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return s;
}

// PNG = 8-byte signature + (length:u32be, type:4, data, crc:u32be) chunks.
// Walk the tEXt/iTXt chunks keyed `chara` (SillyTavern's V2 convention) or
// `ccv3` (the V3 convention — some V3 cards carry ONLY ccv3). Returns the
// candidates in file order: {keyword, text} for values decodable in place,
// {keyword, compressed} for iTXt with the compression flag set (a zlib
// stream — needs async inflation, see readCharaChunk). Pure.
function walkCharaCandidates(u8) {
  const SIG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (u8.length < 8) return [];
  for (let i = 0; i < 8; i++) if (u8[i] !== SIG[i]) return [];
  let off = 8;
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  const out = [];
  while (off + 8 <= u8.length) {
    const len = dv.getUint32(off);
    const type = decodeLatin1(u8.subarray(off + 4, off + 8));
    const dataStart = off + 8;
    const dataEnd = dataStart + len;
    if (dataEnd > u8.length) return out; // truncated
    if (type === 'tEXt' || type === 'iTXt') {
      const chunk = u8.subarray(dataStart, dataEnd);
      let nul = -1;
      for (let i = 0; i < chunk.length; i++) {
        if (chunk[i] === 0) { nul = i; break; }
      }
      if (nul > 0) {
        const keyword = decodeLatin1(chunk.subarray(0, nul));
        if (keyword === 'chara' || keyword === 'ccv3') {
          if (type === 'tEXt') {
            out.push({ keyword, text: decodeLatin1(chunk.subarray(nul + 1)) });
          } else {
            // iTXt: keyword\0 compressionFlag(1) compressionMethod(1)
            // langTag\0 translatedKey\0 text(utf-8).
            const compressed = chunk[nul + 1] === 1;
            let p = nul + 3;
            while (p < chunk.length && chunk[p] !== 0) p++;
            p++;
            while (p < chunk.length && chunk[p] !== 0) p++;
            p++;
            const payload = chunk.subarray(p);
            if (compressed) out.push({ keyword, compressed: payload });
            else out.push({ keyword, text: new TextDecoder('utf-8').decode(payload) });
          }
        }
      }
    }
    off = dataEnd + 4; // skip data + crc
  }
  return out;
}

// The sync accessor (pure, unit-tested): the first uncompressed `chara`
// value, else the first uncompressed `ccv3`. Compressed iTXt candidates are
// skipped — only the async readCharaChunk can inflate them.
export function findCharaChunk(u8) {
  const cands = walkCharaCandidates(u8);
  const pick = (kw) => cands.find((c) => c.keyword === kw && c.text != null);
  const hit = pick('chara') || pick('ccv3');
  return hit ? hit.text : null;
}

// Inflate a zlib stream (PNG iTXt compressionMethod 0 = RFC-1950 zlib).
// DecompressionStream('deflate') IS the zlib-wrapped flavor ('deflate-raw'
// is raw). Shipped by WebView2 + Node ≥18 alike.
async function inflateZlib(bytes) {
  if (typeof DecompressionStream === 'undefined') {
    throw new Error('this PNG stores its character data compressed, which this runtime cannot inflate');
  }
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('deflate'));
  const buf = await new Response(stream).arrayBuffer();
  return new Uint8Array(buf);
}

// The full import path (async): `chara` first, then `ccv3`; inflates
// compressed iTXt payloads; a candidate that fails to inflate falls through
// to the next one rather than aborting the import. Returns null when no
// usable candidate exists.
export async function readCharaChunk(u8) {
  const cands = walkCharaCandidates(u8);
  const tryKeys = async (kw) => {
    for (const c of cands) {
      if (c.keyword !== kw) continue;
      try {
        return c.text != null ? c.text : new TextDecoder('utf-8').decode(await inflateZlib(c.compressed));
      } catch (_) { /* fall through to the next candidate */ }
    }
    return null;
  };
  const chara = await tryKeys('chara');
  if (chara != null) return chara;
  return tryKeys('ccv3');
}

export function base64ToUtf8(b64) {
  const bin = atob(b64.replace(/\s/g, ''));
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder('utf-8').decode(bytes);
}

// Normalize a parsed character JSON (V2/V3 wrapper OR plain) into a flat
// charData object. Returns null on a non-object. Carries the canonical ST
// fields + the V2 content fields (alternate_greetings / system_prompt /
// post_history_instructions / tags / creator / character_version) so the GLM
// import path sees the WHOLE card and can map every content-bearing field
// onto the wizard schema — the classic 8 alone dropped alternate greetings +
// behavior directives + tags, losing authored content on import.
export function normalizeCharJson(obj) {
  if (!obj || typeof obj !== 'object') return null;
  const data = (obj.spec && obj.data && typeof obj.data === 'object') ? obj.data : obj;
  const arr = (v) => (Array.isArray(v) ? v : []);
  return {
    spec: obj.spec || 'plain',
    name: data.name || obj.name || '',
    description: data.description || '',
    personality: data.personality || '',
    scenario: data.scenario || '',
    first_mes: data.first_mes || '',
    mes_example: data.mes_example || '',
    creator_notes: data.creator_notes || '',
    character_book: data.character_book || null,
    // V2 content fields (absent on plain/V1 cards → empty).
    alternate_greetings: arr(data.alternate_greetings || obj.alternate_greetings),
    system_prompt: data.system_prompt || '',
    post_history_instructions: data.post_history_instructions || '',
    tags: arr(data.tags || obj.tags),
    creator: data.creator || obj.creator || '',
    character_version: data.character_version || obj.character_version || '',
  };
}

// --- SillyTavern lorebook import (2026-08-19 Chloe ruling) ------------------
//
// A standalone lorebook JSON is a FIRST-CLASS import: recognized mechanically,
// converted to codex entries, and refined by the assistant in batches (a real
// book runs 70-140KB — far past the API payload budget, so a single one-shot
// turn would have its <import> message silently dropped and the assistant
// would truthfully report "no lorebook content").
//
// Hard rules (Chloe, 2026-08-19):
//   - entries with NO keywords are SKIPPED at parse time (no retrieval trigger
//     = dead weight; the assistant never even sees them);
//   - every surviving entry gets a clean title + AT LEAST 3 tags (the
//     assistant writes them; padCodexEntryTags is the mechanical floor);
//   - bodies over 1400 chars are split into parts (Rust Gate 1 rejects an
//     oversize `ready`; codex::expand_oversize_entries is the write backstop).

// Merge an ST entry's keyword arrays. ST-native books carry `key` (array), V2
// spec books carry `keys`, many exports carry BOTH — read the union, deduped,
// trimmed. Secondary keys never count (they are AND-conditions, not triggers).
function loreEntryKeys(e) {
  const out = [];
  const push = (v) => {
    if (v == null) return;
    if (Array.isArray(v)) { v.forEach(push); return; }
    const s = String(v).trim();
    if (s && !out.includes(s)) out.push(s);
  };
  push(e.key);
  push(e.keys);
  return out.slice(0, 12);
}

// Extract the usable entries from a lorebook container (a standalone book's
// top level, or a card's character_book). `entries` may be an array OR an
// object keyed by index (both are valid ST shapes — guard on truthiness, not
// Array.isArray). Returns [{title, keys, content}] — keyless and empty-body
// entries are already skipped.
export function extractLorebookEntries(book) {
  if (!book || typeof book !== 'object' || !book.entries || typeof book.entries !== 'object') return [];
  const raw = book.entries;
  const list = Array.isArray(raw) ? raw : Object.values(raw);
  const out = [];
  for (const e of list) {
    if (!e || typeof e !== 'object') continue;
    const content = (e.content || '').toString();
    if (!content.trim()) continue;
    const keys = loreEntryKeys(e);
    if (!keys.length) continue; // no keywords → skipped (2026-08-19 ruling)
    out.push({
      title: (e.comment || e.name || keys[0]).toString().slice(0, 128),
      keys,
      content,
    });
  }
  return out;
}

// Recognize a STANDALONE lorebook JSON: top-level `entries` yielding at least
// one usable entry, with no real character fields (a `name` alone is fine —
// Frieren-style exports name the book; any description/greeting/personality
// makes it a character card carrying its book, which the character path owns).
// Returns {name, entries} or null.
export function extractStandaloneLorebook(obj) {
  if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return null;
  if (!obj.entries || typeof obj.entries !== 'object') return null;
  const charFields = [obj.description, obj.first_mes, obj.personality, obj.scenario, obj.mes_example];
  if (charFields.some((v) => typeof v === 'string' && v.trim())) return null;
  const entries = extractLorebookEntries(obj);
  if (!entries.length) return null;
  return {
    name: (typeof obj.name === 'string' && obj.name.trim() ? obj.name : 'Imported lorebook').slice(0, 80),
    entries,
  };
}

// Does a normalized charData carry ANY content-bearing field? normalizeCharJson
// accepts every object, so an arbitrary JSON file (settings dumps, arrays of
// strings, …) produced an all-empty character card — the import "succeeded"
// and the wizards were fed a husk. This is the recognition gate at the import
// boundary: no content → unrecognized → bottom warning, never a wizard.
export function charDataHasContent(c) {
  if (!c || typeof c !== 'object') return false;
  return !!(
    toText(c.name) || toText(c.description) || toText(c.personality) || toText(c.scenario)
    || toText(c.first_mes) || toText(c.mes_example) || toText(c.creator_notes)
    || toText(c.system_prompt) || toText(c.post_history_instructions)
    || (Array.isArray(c.tags) && c.tags.length)
    || (Array.isArray(c.alternate_greetings) && c.alternate_greetings.length)
    || c.character_book
  );
}

// Chunk extracted entries into conversion batches by cumulative char size so
// each assistant turn (input + reply JSON) stays well inside the API payload
// budget. Whole entries only — a batch boundary never splits an entry, and a
// single oversized entry rides alone (its SIZE is the model's split problem,
// answered by Rust's 1400-char ready gate).
export const LORE_BATCH_CHAR_BUDGET = 8000;
export function batchLorebookEntries(entries, budget = LORE_BATCH_CHAR_BUDGET) {
  const list = Array.isArray(entries) ? entries : [];
  const batches = [];
  let cur = [];
  let curChars = 0;
  for (const e of list) {
    const size = e.content.length + e.title.length + e.keys.join(',').length + 48;
    if (cur.length && curChars + size > budget) {
      batches.push(cur);
      cur = [];
      curChars = 0;
    }
    cur.push(e);
    curChars += size;
  }
  if (cur.length) batches.push(cur);
  return batches;
}

// Mechanical tag floor (≥3 per entry, lowercase, deduped, capped at 8): the
// assistant's tags first, then the SOURCE entry's ST keywords (the authored
// retrieval triggers — the most semantically honest fallback), then the AI
// title's words, then the book name. Only a total drought hits the generic
// 'lore'/'world' floor.
export function padCodexEntryTags(tags, sourceKeys, title, bookName) {
  const out = [];
  const push = (v) => {
    const s = String(v == null ? '' : v).trim().toLowerCase();
    if (s && !out.includes(s)) out.push(s);
  };
  (Array.isArray(tags) ? tags : (tags != null ? [tags] : [])).forEach(push);
  (Array.isArray(sourceKeys) ? sourceKeys : (sourceKeys != null ? [sourceKeys] : [])).forEach(push);
  String(title || '')
    .toLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter((w) => w.length >= 3 && w !== 'part')
    .forEach(push);
  push(bookName);
  if (out.length < 3) { push('lore'); push('world'); push('imported'); }
  return out.slice(0, 8);
}

// Match a returned entry back to its source: normalized title equality first
// (strips "— Part N" so split parts find their origin), index clamp second
// (the assistant keeps input order, so a same-index guess is at worst adjacent).
function normalizeTitleForMatch(s) {
  return String(s || '')
    .toLowerCase()
    .replace(/[—–-]\s*part\s*\d+\s*$/i, '')
    .replace(/[^\p{L}\p{N}]+/gu, ' ')
    .trim();
}

// Normalize one batch's assistant entries into the final codex shape: drop
// body-less husks, fall missing titles back to the source title, and run every
// entry through the tag floor. `batchSources` is the batch's extracted
// [{title, keys, content}] in emission order.
export function normalizeLoreImportEntries(aiEntries, batchSources, bookName) {
  if (!Array.isArray(aiEntries)) return [];
  const sources = Array.isArray(batchSources) ? batchSources : [];
  return aiEntries
    .map((e, i) => {
      const src = e && typeof e === 'object' ? e : {};
      const body = typeof src.body === 'string' ? src.body.trim() : '';
      if (!body) return null;
      const nt = normalizeTitleForMatch(src.title);
      let source = nt ? sources.find((s) => normalizeTitleForMatch(s.title) === nt) : null;
      if (!source) source = sources[Math.min(i, sources.length - 1)];
      const title = toText(src.title) || (source ? source.title : '') || 'Imported entry';
      return {
        title,
        tags: padCodexEntryTags(src.tags, source ? source.keys : [], title, bookName),
        body,
      };
    })
    .filter(Boolean);
}
