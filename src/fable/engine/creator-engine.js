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
    dst[k] = v;
  }
  return dst;
}

// --- Review-section model -------------------------------------------------

// Build the review card's [title, [[label, value], ...]] section list for a
// kind/draft. Values are RAW (escaping is the renderer's job). Empty rows +
// empty sections are filtered out. `row` returns null for absent values so the
// filter chain drops them.
export function buildReviewSections(kind, d) {
  const row = (label, v) => {
    if (v == null) return null;
    if (Array.isArray(v)) {
      if (!v.length) return null;
      v = v.join(', ');
    }
    const s = String(v).trim();
    return s ? [label, s] : null;
  };
  if (kind === 'player') {
    const customRows = d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)
      ? Object.entries(d.custom_tags)
          .map(([k, v]) => row(String(k), v))
          .filter(Boolean)
      : [];
    return [
      ['Identity', [row('Name', d.name), row('Gender', d.gender), row('Race', d.race), row('Age', d.age)].filter(Boolean)],
      ['Appearance', [row('Height', d.height), row('Weight', d.weight), row('Build', d.body_type), row('Skin', d.skin_complexion), row('Eyes', d.eye_color)].filter(Boolean)],
      ['Hair', [row('Color', d.hair_color), row('Length', d.hair_length), row('Style', d.hair_style)].filter(Boolean)],
      ['Distinctive', [row('Breast', d.breast_size), row('Ears', d.ears), row('Tail', d.tail), row('Horn', d.horn)].filter(Boolean)],
      ['Clothing', [row('Outfit', d.clothing)].filter(Boolean)],
      ['Inventory', [row('Gear', d.gear), row('Tools', d.tools), row('Weapons', d.weapons)].filter(Boolean)],
      ['Background', [row('Job', d.job), row('Personality', d.personality), row('Weakness', d.weakness), row('Distinguishing marks', d.distinguishing_marks), row('History', d.backstory)].filter(Boolean)],
      // Starting conditions are TRANSIENT (seed PlayerState at attach, never
      // persisted on the SavedPlayer) but surface here so the player sees
      // what was captured. Absent on edited/loaded players → section dropped.
      ['Starting conditions', [row('Wealth', d.wealth), row('Reputation', d.reputation || d.fame)].filter(Boolean)],
      ['Custom tags', customRows],
    ].filter(([, rows]) => rows.length);
  }
  if (kind === 'sim') {
    const subtype = (d.card_type || '').trim();
    const customRows = d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)
      ? Object.entries(d.custom_tags).map(([k, v]) => row(String(k), v)).filter(Boolean)
      : [];
    const locSrc = Array.isArray(d.locations) && d.locations.length
      ? d.locations
      : (d.location ? [{ name: d.location }] : []);
    const locRows = locSrc
      .map((l) => row(l.name || l.id, Array.isArray(l.neighbors) ? l.neighbors.join(', ') : ''))
      .filter(Boolean);
    const castRows = (Array.isArray(d.cast) ? d.cast : [])
      .map((c) => row(c.name || c.id, c.identity || c.role))
      .filter(Boolean);
    const sections = [
      ['Card', [row('Type', subtype), row('Name', d.name)].filter(Boolean)],
      ['World anchors', [row('Date', d.date), row('Time', d.time || d.start_time), row('Weather', d.weather || d.start_weather), row('Location', d.location)].filter(Boolean)],
    ];
    // Branch-specific sections (per the SIM Wizard Type Router spec).
    if (subtype === 'npc') {
      sections.push(['Identity', [row('Gender', d.gender), row('Race', d.race), row('Age', d.age), row('Height', d.height), row('Weight', d.weight), row('Build', d.body_type), row('Skin', d.skin_complexion), row('Eyes', d.eye_color)].filter(Boolean)]);
      sections.push(['Hair', [row('Color', d.hair_color), row('Length', d.hair_length), row('Style', d.hair_style)].filter(Boolean)]);
      sections.push(['Distinctive', [row('Breast', d.breast_size), row('Ears', d.ears), row('Tail', d.tail), row('Horn', d.horn)].filter(Boolean)]);
      sections.push(['Clothing', [row('Outfit', d.clothing)].filter(Boolean)]);
      sections.push(['Persona', [row('Personality', d.personality), row('Flaws', d.flaws), row('Job', d.job), row('Backstory', d.backstory), row('Dialogue style', d.dialogue_style), row('Tone', d.tone)].filter(Boolean)]);
    } else if (subtype === 'scenario') {
      sections.push(['Scenario', [row('Directive', d.directive), row('Trigger', d.trigger_condition), row('Objective', d.primary_objective), row('Actors', d.participating_actors), row('Hazards', d.environmental_hazards), row('Outcomes', d.outcomes), row('Tone', d.tone)].filter(Boolean)]);
    } else {
      // world, or pre-router/unknown: show the world-style fields that exist.
      sections.push(['World', [row('Directive', d.directive), row('Setting', d.setting), row('Tone', d.tone)].filter(Boolean)]);
    }
    if (locRows.length) sections.push(['Locations', locRows]);
    if (castRows.length) sections.push(['Cast', castRows]);
    if (customRows.length) sections.push(['Custom tags', customRows]);
    // The intro the SIM Wizard gathered (its mandatory question — 2026-08-15
    // the intro is the wizard's job, not a post-card step). Absent → dropped.
    if ((d.intro || '').trim()) sections.push(['Intro', [['Text', String(d.intro).trim()]]]);
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

// --- ID-card model (compact core + bronze-arrow extra disclosure) ----------

// The ID card splits a draft into a COMPACT always-visible "core" face (a flat
// label/value grid, NO section headers — a real-life state-license look) and an
// "extra" disclosure of every remaining tag, revealed by the bronze arrow
// (§6C/§6D, 2026-08-13). Player/NPC share the player layout (8 core fields);
// world/scenario get a TYPE banner + a 4-field core. codex is NOT an ID
// card → returns null (it keeps the generic review-section rendering, so
// buildReviewSections + its tests stay intact).

// idRow mirrors buildReviewSections' `row`: arrays join to a comma string;
// null/undefined/empty/blank → null so the caller's filter chain drops them.
// Values are RAW (escaping is the renderer's job).
function idRow(label, v) {
  if (v == null) return null;
  if (Array.isArray(v)) {
    if (!v.length) return null;
    v = v.join(', ');
  }
  const s = String(v).trim();
  return s ? [label, s] : null;
}

function customTagRows(d) {
  return d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)
    ? Object.entries(d.custom_tags).map(([k, v]) => idRow(String(k), v)).filter(Boolean)
    : [];
}

// Returns { variant, banner, tag, core, extra } or null for non-ID kinds.
//   variant: 'player' | 'world'
//   banner:  'WORLD CARD' | 'SCENARIO CARD' | null  (world/scenario only)
//   tag:     'NPC CARD' | null                        (npc only — subtle corner tag)
//   core:    [[label, value], ...]   always-visible compact grid (no headers)
//   extra:   [[title, [[label, value], ...]], ...]   revealed by the bronze arrow
export function buildIdCard(kind, d) {
  const draft = d || {};
  if (kind === 'player') return playerIdCard(draft, false);
  if (kind === 'sim') {
    const subtype = (draft.card_type || '').trim().toLowerCase();
    if (subtype === 'npc') return playerIdCard(draft, true);
    if (subtype === 'scenario') return worldIdCard(draft, 'SCENARIO CARD');
    return worldIdCard(draft, 'WORLD CARD'); // world, pre-router, or unknown
  }
  return null; // codex is not an ID card
}

// Player + NPC share the 8-field core. The only difference is the subtle
// 'NPC CARD' corner tag (isNpc). Extra groups surface EVERY remaining tag,
// filter-emptied: Hair length/style, Physique, Distinctive features, Clothing,
// Accessories (legacy), Inventory, Persona, Background, transient Starting
// conditions, and Custom tags.
function playerIdCard(d, isNpc) {
  const core = [
    idRow('Name', d.name),
    idRow('Gender', d.gender),
    idRow('Race', d.race),
    idRow('Age', d.age),
    idRow('Hair Color', d.hair_color),
    idRow('Eye Color', d.eye_color),
    idRow('Height', d.height),
    idRow('Weight', d.weight),
  ].filter(Boolean);
  const extra = [
    ['Hair', [idRow('Length', d.hair_length), idRow('Style', d.hair_style)].filter(Boolean)],
    ['Physique', [idRow('Body', d.body_type), idRow('Skin', d.skin_complexion)].filter(Boolean)],
    ['Distinctive', [idRow('Breast', d.breast_size), idRow('Ears', d.ears), idRow('Tail', d.tail), idRow('Horn', d.horn)].filter(Boolean)],
    ['Clothing', [idRow('Outfit', d.clothing)].filter(Boolean)],
    ['Accessories', [idRow('Items', d.accessories)].filter(Boolean)],
    ['Inventory', [idRow('Gear', d.gear), idRow('Tools', d.tools), idRow('Weapons', d.weapons)].filter(Boolean)],
    ['Persona', [idRow('Personality', d.personality), idRow('Flaws', d.flaws), idRow('Dialogue style', d.dialogue_style), idRow('Tone', d.tone)].filter(Boolean)],
    ['Background', [idRow('Job', d.job), idRow('Weakness', d.weakness), idRow('Distinguishing marks', d.distinguishing_marks), idRow('Description', d.description), idRow('Appearance', d.appearance), idRow('History', d.backstory)].filter(Boolean)],
    // Starting conditions are TRANSIENT (seed PlayerState at attach, never
    // persisted on the SavedPlayer) but surface here so the player sees what
    // was captured. Absent on edited/loaded players → section dropped.
    ['Starting conditions', [idRow('Wealth', d.wealth), idRow('Reputation', d.reputation || d.fame)].filter(Boolean)],
    ['Custom tags', customTagRows(d)],
    // The intro the SIM Wizard gathered for an NPC card (mandatory question —
    // 2026-08-15). Player cards never carry one → dropped.
    ...((d.intro || '').trim() ? [['Intro', [['Text', String(d.intro).trim()]]]] : []),
  ].filter(([, rows]) => rows.length);
  return { variant: 'player', banner: null, tag: isNpc ? 'NPC CARD' : null, core, extra };
}

// World + Scenario share a banner + a 4-field core (Name/Setting/Purpose/Tone).
// directive is shown as "Purpose". Scenario-specific fields, world anchors,
// locations, cast, custom tags, and the intro all live in the disclosure.
function worldIdCard(d, banner) {
  const core = [
    idRow('Name', d.name),
    idRow('Setting', d.setting),
    idRow('Purpose', d.directive),   // directive shown as "Purpose"
    idRow('Tone', d.tone),
  ].filter(Boolean);
  const locSrc = Array.isArray(d.locations) && d.locations.length
    ? d.locations
    : (d.location ? [{ name: d.location }] : []);
  const locRows = locSrc
    .map((l) => idRow(l.name || l.id, Array.isArray(l.neighbors) ? l.neighbors.join(', ') : ''))
    .filter(Boolean);
  const castRows = (Array.isArray(d.cast) ? d.cast : [])
    .map((c) => idRow(c.name || c.id, c.identity || c.role))
    .filter(Boolean);
  const extra = [
    ['World anchors', [idRow('Date', d.date), idRow('Time', d.time || d.start_time), idRow('Weather', d.weather || d.start_weather), idRow('Location', d.location)].filter(Boolean)],
  ];
  if (banner === 'SCENARIO CARD') {
    extra.push(['Scenario', [idRow('Trigger', d.trigger_condition), idRow('Objective', d.primary_objective), idRow('Actors', d.participating_actors), idRow('Hazards', d.environmental_hazards), idRow('Outcomes', d.outcomes)].filter(Boolean)]);
  }
  if (locRows.length) extra.push(['Locations', locRows]);
  if (castRows.length) extra.push(['Cast', castRows]);
  extra.push(['Custom tags', customTagRows(d)]);
  // The intro the SIM Wizard gathered (mandatory question). Absent → dropped.
  if ((d.intro || '').trim()) extra.push(['Intro', [['Text', String(d.intro).trim()]]]);
  return { variant: 'world', banner, tag: null, core, extra: extra.filter(([, rows]) => rows.length) };
}

// --- SillyTavern import primitives (pure) ---------------------------------

function decodeLatin1(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return s;
}

// PNG = 8-byte signature + (length:u32be, type:4, data, crc:u32be) chunks.
// Walk until a tEXt/iTXt chunk keyed `chara` (SillyTavern's convention); return
// its base64 value, or null if not found / not a PNG.
export function findCharaChunk(u8) {
  const SIG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (u8.length < 8) return null;
  for (let i = 0; i < 8; i++) if (u8[i] !== SIG[i]) return null;
  let off = 8;
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  while (off + 8 <= u8.length) {
    const len = dv.getUint32(off);
    const type = decodeLatin1(u8.subarray(off + 4, off + 8));
    const dataStart = off + 8;
    const dataEnd = dataStart + len;
    if (dataEnd > u8.length) return null; // truncated
    if (type === 'tEXt' || type === 'iTXt') {
      const chunk = u8.subarray(dataStart, dataEnd);
      let nul = -1;
      for (let i = 0; i < chunk.length; i++) {
        if (chunk[i] === 0) { nul = i; break; }
      }
      if (nul > 0) {
        const keyword = decodeLatin1(chunk.subarray(0, nul));
        if (keyword === 'chara') {
          let value;
          if (type === 'tEXt') {
            value = decodeLatin1(chunk.subarray(nul + 1));
          } else {
            // iTXt: flag(1)+method(1)+langTag\0+translatedKey\0+text(utf-8).
            let p = nul + 1 + 1 + 1;
            while (p < chunk.length && chunk[p] !== 0) p++;
            p++;
            while (p < chunk.length && chunk[p] !== 0) p++;
            p++;
            value = new TextDecoder('utf-8').decode(chunk.subarray(p));
          }
          return value;
        }
      }
    }
    off = dataEnd + 4; // skip data + crc
  }
  return null;
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

// Normalize a STANDALONE lorebook JSON into the same charData shape so the
// codex-import path treats character-files + lorebooks uniformly. A standalone
// lorebook has `entries` (array or object map) but no character fields; we wrap
// it as { character_book: {entries}, name: 'Imported lorebook' }. Returns null
// if the object has no entries.
export function normalizeLorebookJson(obj) {
  if (!obj || typeof obj !== 'object') return null;
  const data = (obj.spec && obj.data && typeof obj.data === 'object') ? obj.data : obj;
  const entries = data.entries || obj.entries;
  if (!entries) return null;
  // If it's clearly a character (has name/description), defer to normalizeCharJson.
  if ((data.name || obj.name) && (data.description || obj.description)) return null;
  return {
    spec: 'lorebook',
    name: obj.name || data.name || 'Imported lorebook',
    description: '',
    personality: '',
    scenario: '',
    first_mes: '',
    mes_example: '',
    creator_notes: '',
    character_book: { entries },
  };
}

// Convert a character_book (SillyTavern lorebook) into the codex_entries shape
// { title, tags, body }. `entries` may be an array OR an object keyed by index.
export function lorebookToCodexEntries(book) {
  // `entries` may be an array OR an object keyed by index (both are valid ST
  // lorebook shapes). Guard on truthiness, not Array.isArray — the latter
  // silently rejected every object-map lorebook (a latent bug the unit test
  // caught).
  if (!book || !book.entries) return [];
  const raw = book.entries;
  const list = Array.isArray(raw) ? raw : Object.values(raw);
  return list
    .filter((e) => e && (e.content || e.comment))
    .map((e, i) => ({
      title: (e.comment || e.name || `Entry ${i + 1}`).toString().slice(0, 128),
      tags: Array.isArray(e.key)
        ? e.key.slice(0, 8).map((k) => String(k).slice(0, 64))
        : (e.key ? [String(e.key).slice(0, 64)] : []),
      body: (e.content || '').toString(),
    }))
    .filter((e) => e.body.trim());
}
