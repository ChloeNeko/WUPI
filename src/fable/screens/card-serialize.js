// =============================================================
// CARD SERIALIZE — the v2 (2026-08-19) `<sim_card>` XML builder for the
// GLM-conversational Creator + the SavedPlayer DTO builder.
//
// THE V2 .sim FORMAT (Chloe ruling 2026-08-19 — byte-matches her authored
// reference cards, `liam.sim` / `example.sim`):
//   <sim_card>                      ← EVERYTHING inside is the KV-cache
//     <metadata>                      payload, read by the narrator EVERY
//     <type>simulation</type>         turn ("roleplay" is retired).
//     <subtype>npc</subtype>
//     <id>slug</id>
//     </metadata>
//     <identity><![CDATA[           ← line block: "Label: value" per line
//   Name: Liam
//   Gender: Male
//   ...
//     ]]></identity>
//     <persona><![CDATA[            ← line block (mandatory for npc cards;
//   Personality: ...                  optional + omitted entirely for
//   Conversation Style: ...           players on the .player side)
//     ]]></persona>
//     <setting><![CDATA[...]]></setting>   ← scenario/world dedicated prose
//     <plot><![CDATA[...]]></plot>         ← (kept per Chloe's ruling)
//     <custom_tags>
//       <entry key="scent"><![CDATA[...]]></entry>
//     </custom_tags>
//   </sim_card>
//
//   <world><![CDATA[               ← SIBLINGS: mutable world-state seeds,
//   Date: ...                        never part of the cached root. Tone
//   Time: ...                        lives HERE (seeded into the world
//   Weather: ...                     tracker, rendered with time+weather).
//   Tone: ...
//   ]]></world>
//   <location><![CDATA[
//   ...
//   ]]></location>
//   <intro><![CDATA[...]]></intro>
//   <inventory><![CDATA[            ← npc cards only; empty lines omitted.
//   Clothing: ...                      Players get the same sibling in the
//   Equipped: ...                      `.player` format.
//   ]]></inventory>
//
// Empty fields are omitted from the file entirely (the format rule).
// `<cast>` and the `<locations>` graph are GONE — an npc card IS the
// character (it self-registers at session start); places/NPCs beyond the
// opening location emerge in play.
//
// THE INTRO SIBLING: `serializeSimCard` returns { xml, intro } where `xml`
// already carries the appended `<intro>` block when non-empty, so ONE
// `fable_write_card` write persists both. The Rust parser
// (sim_card.rs, the validator of record) reads the two-root shape by slicing
// at the CDATA-aware `</sim_card>` scanner.
//
// Players are NOT XML here: `serializePlayer` builds the SavedPlayer DTO
// (the IPC shape); RUST renders + writes the `.player` XML — mechanical
// integrity stays server-side, one renderer, and the edit flow's
// merge-forward keeps working untouched.
// =============================================================

export function cdata(text) {
  return `<![CDATA[${String(text || '').replace(/]]>/g, ']]]]><![CDATA[>')}]]>`;
}

// ── (2026-08-20 audit) Authored starting holdings — draft.properties ─────
// The optional GLM field (player + sim schemas, taught in the Rust creator
// prompt). Normalized ONCE here into the AuthoredProperty shape; the sim
// path renders it to the pipe-kv `<properties>` sibling (Rust
// parse_property_lines reads it back), the player path carries the objects
// on the SavedPlayer DTO (Rust renders the `.player` XML twin + seeds them
// Player-owned on fresh runs). The Rust seed refuses past MAX_PROPERTIES
// (8) — cap here so a GLM overshoot doesn't seed-and-drop.
const PROPERTY_KINDS = new Set(['business', 'estate', 'settlement']);
const MAX_DRAFT_PROPERTIES = 8;

export function normalizeProperties(props) {
  if (!Array.isArray(props)) return [];
  const clampAmount = (v) => {
    const n = Math.floor(Number(v));
    return Number.isFinite(n) && n > 0 ? Math.min(n, 100000) : 0;
  };
  const out = [];
  for (const src of props.slice(0, MAX_DRAFT_PROPERTIES)) {
    if (!src || typeof src !== 'object') continue;
    const id = slugify(text(src.id));
    const node = text(src.node).trim();
    if (!id || !node) continue; // the Rust parser skips these too
    const kindRaw = text(src.kind).trim().toLowerCase();
    const owner = text(src.owner).trim();
    const price = clampAmount(src.price);
    out.push({
      id,
      node,
      kind: PROPERTY_KINDS.has(kindRaw) ? kindRaw : 'business',
      revenue: clampAmount(src.revenue),
      upkeep: clampAmount(src.upkeep),
      ...(owner ? { owner } : {}),
      ...(price > 0 ? { price } : {}),
    });
  }
  return out;
}

// The pipe-kv line grammar (`id: forge | node: iron-forge | kind: business
// | revenue: 8 | upkeep: 3`) — the round-trip half of Rust's
// economy::parse_property_lines.
export function renderPropertyLines(props) {
  return normalizeProperties(props).map((p) => {
    const parts = [`id: ${p.id}`, `node: ${p.node}`, `kind: ${p.kind}`];
    if (p.owner) parts.push(`owner: ${p.owner}`);
    parts.push(`revenue: ${p.revenue}`, `upkeep: ${p.upkeep}`);
    if (p.price > 0) parts.push(`price: ${p.price}`);
    return parts.join(' | ');
  }).join('\n');
}

// Coerce ANY GLM draft value into a serialize-safe string. GLM occasionally
// emits numbers ("age": 24), arrays, or nulls/objects despite the string
// schema — calling (v || '').trim() on those crashes CREATE. Arrays join
// with ', '; booleans + plain objects are unrenderable → ''.
export function text(v) {
  if (v == null || typeof v === 'boolean') return '';
  if (Array.isArray(v)) return v.map(text).filter(Boolean).join(', ');
  if (typeof v === 'object') return '';
  return String(v);
}

export function escapeXml(s) {
  // `"` is escaped too so the fn is safe in ATTRIBUTE contexts (<entry
  // key="…">) as well as element text. XML-invalid control chars are
  // stripped (no valid escape exists for most in XML 1.0): a GLM string
  // carrying a stray vertical tab made the whole card unloadable at the
  // next parse. \t \n \r are the only legal controls + are preserved.
  return String(s || '')
    // eslint-disable-next-line no-control-regex
    .replace(/[\u0000-\u0008\u000B\u000C\u000E-\u001F\u007F]/g, '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// --- Player (SavedPlayer DTO — Rust renders the .player XML) ---------------
// Build the DTO from the GLM draft. Core identity traits are the v2
// `<identity>` line set; `persona` (the FINAL wizard question's opt-in
// block — Conversation Style is NEVER offered to players) + `backstory`
// ride the `<persona>` block; `inventory` is the mutable sibling seed.
// Transient gameplay fields (wealth/reputation/fame) are deliberately NOT
// serialized here — they seed PlayerState at attach, never the identity
// file. Returns { id, player } for fable_player_write({ id, player }).
export function serializePlayer(draft) {
  const d = draft || {};
  const name = text(d.name).trim();
  const opt = (v) => {
    const s = text(v).trim();
    return s || null;
  };
  // Chip-list normalizer: trim + drop blanks; null when nothing remains.
  // Accepts an array OR a comma-separated string (GLM sometimes emits
  // "tunic, boots" for an array field).
  const chipList = (v) => {
    const arr = typeof v === 'string' ? v.split(',') : v;
    if (!Array.isArray(arr) || !arr.length) return null;
    const items = arr.map((s) => text(s).trim()).filter(Boolean);
    return items.length ? items : null;
  };
  const player = {
    id: slugify(name) || 'player',
    name: name || 'Unnamed',
    gender: opt(d.gender),
    race: opt(d.race),
    age: opt(d.age),
    height: opt(d.height),
    weight: opt(d.weight),
    hair_color: opt(d.hair_color),
    hair_length: opt(d.hair_length),
    hair_style: opt(d.hair_style),
    body_type: opt(d.body_type),
    skin_complexion: opt(d.skin_complexion),
    eye_color: opt(d.eye_color),
  };
  // Conditional traits — include only when GLM supplied them.
  const breast = opt(d.breast_size); if (breast) player.breast_size = breast;
  const ears = opt(d.ears); if (ears) player.ears = ears;
  const tail = opt(d.tail); if (tail) player.tail = tail;
  const horn = opt(d.horn); if (horn) player.horn = horn;
  // The opt-in persona block (2026-08-19): any of the offered fields, plus
  // the standalone backstory the wizard collects — all serialize as
  // `<persona>` lines server-side, omitted entirely when all absent.
  const src = d.persona && typeof d.persona === 'object' && !Array.isArray(d.persona) ? d.persona : {};
  const persona = {};
  const pOpt = (k, v) => {
    const s = text(v).trim();
    if (s) persona[k] = s;
  };
  pOpt('personality', src.personality != null ? src.personality : d.personality);
  pOpt('likes', src.likes != null ? src.likes : d.likes);
  pOpt('dislikes', src.dislikes != null ? src.dislikes : d.dislikes);
  pOpt('flaws', src.flaws != null ? src.flaws : d.flaws);
  pOpt('goals', src.goals != null ? src.goals : d.goals);
  pOpt('occupation', src.occupation != null ? src.occupation : d.job);
  if (Object.keys(persona).length) player.persona = persona;
  const backstory = opt(d.backstory);
  if (backstory) player.backstory = backstory;
  // (2026-08-20 audit) Authored starting holdings — the `.player`
  // `<properties>` twin (Player-owned seeds on fresh runs; Rust renders
  // the XML). Previously the entire Rust-side pathway was unreachable
  // except by hand-editing XML.
  const authored = normalizeProperties(d.properties);
  if (authored.length) player.properties = authored;
  // The optional `<inventory>` sibling seed.
  const srcInv = d.inventory && typeof d.inventory === 'object' && !Array.isArray(d.inventory)
    ? d.inventory
    : {};
  const inv = {};
  const clothing = chipList(srcInv.clothing != null ? srcInv.clothing : d.clothing);
  if (clothing) inv.clothing = clothing;
  const equipped = chipList(srcInv.equipped != null ? srcInv.equipped : d.equipped);
  if (equipped) inv.equipped = equipped;
  const accessories = chipList(srcInv.accessories != null ? srcInv.accessories : d.accessories);
  if (accessories) inv.accessories = accessories;
  const stored = chipList(srcInv.stored != null ? srcInv.stored : d.stored != null ? d.stored : d.gear);
  if (stored) inv.stored = stored;
  if (Object.keys(inv).length) player.inventory = inv;
  // Custom extensions: flat string→string map (drop blank keys/values).
  if (d.custom_tags && typeof d.custom_tags === 'object' && !Array.isArray(d.custom_tags)) {
    const tags = {};
    for (const [k, v] of Object.entries(d.custom_tags)) {
      const kk = String(k || '').trim();
      const vv = opt(v);
      if (kk && vv) tags[kk] = vv;
    }
    if (Object.keys(tags).length) player.custom_tags = tags;
  }
  return { id: player.id, player };
}

// --- Sim Card (the GLM sim wizard's polymorphic Type-Router card) ---------
// The v2 emitter. `<type>` is ALWAYS "simulation" (the 2026-08-19 rename —
// the Rust parser normalizes legacy "roleplay" onto it); the discriminator
// rides in `<subtype>`. UNVERAL anchors (date/time/weather/tone) + location
// serialize to the `<world>`/`<location>` SIBLINGS; npc inventory rides the
// `<inventory>` sibling; everything static (identity traits, persona,
// scenario/world prose, custom tags) lives inside the cached root.
// Returns { xml, intro }.
//
// `opts.pinnedId` (edit runs): the loaded card's existing id. A rename
// changes ONLY the display name — the id (and everything keyed off it:
// folder, saves/, .codex, memory partition) survives. Fresh creates omit
// it and derive the id from the name as before (2026-08-20 rename ruling).
export function serializeSimCard(f, opts = {}) {
  const d = f || {};
  const subtype = text(d.card_type).trim().toLowerCase(); // npc | scenario | world
  const name = text(d.name).trim();
  const tone = text(d.tone).trim();
  const date = text(d.date).trim();
  const time = text(d.time || d.start_time).trim();
  const weather = text(d.weather || d.start_weather).trim();
  const location = text(d.location).trim();
  const intro = text(d.intro).trim();

  // ── identity line block (npc: the full trait set; scenario/world: Name
  // only — their premise lives in the dedicated setting/plot elements). ──
  const identityLines = [];
  if (name) identityLines.push(`Name: ${name}`);
  if (subtype === 'npc') {
    const push = (label, v) => {
      const s = text(v).trim();
      if (s) identityLines.push(`${label}: ${s}`);
    };
    push('Gender', d.gender);
    push('Race', d.race);
    push('Age', d.age);
    push('Height', d.height);
    push('Weight', d.weight);
    push('Body', d.body_type);
    push('Skin', d.skin_complexion);
    push('Eyes', d.eye_color);
    push('Hair Color', d.hair_color);
    push('Hair Length', d.hair_length);
    push('Hair Style', d.hair_style);
  }

  // ── persona line block (npc: every label mandatory in the wizard). ──
  const personaLines = [];
  if (subtype === 'npc') {
    const push = (label, v) => {
      const s = text(v).trim();
      if (s) personaLines.push(`${label}: ${s}`);
    };
    push('Personality', d.personality);
    push('Conversation Style', d.dialogue_style);
    push('Likes', d.likes);
    push('Dislikes', d.dislikes);
    push('Flaws', d.flaws);
    push('Goals', d.goal);
    push('Occupation', d.job);
    push('Backstory', d.backstory);
  }

  let xml = '<sim_card>\n';
  xml += '  <metadata>\n';
  xml += '  <type>simulation</type>\n';
  if (subtype) xml += `  <subtype>${escapeXml(subtype)}</subtype>\n`;
  // The client slug rides in-file. Fresh creates: the parsed id and the
  // on-disk folder agree BY CONSTRUCTION (the folder is the display name,
  // the id the slug — both derive from `name` on the Rust side). Edit
  // runs: the PINNED id wins over the name-derived slug so a rename never
  // mints a new identity (the backend's `renamingId` contract).
  const cardId = (typeof opts.pinnedId === 'string' && opts.pinnedId.trim())
    ? opts.pinnedId.trim()
    : slugify(name);
  xml += `  <id>${escapeXml(cardId)}</id>\n`;
  xml += '  </metadata>\n';

  xml += `  <identity><![CDATA[\n${identityLines.join('\n')}\n  ]]></identity>\n`;

  if (personaLines.length) {
    xml += `  <persona><![CDATA[\n${personaLines.join('\n')}\n  ]]></persona>\n`;
  }

  // scenario/world dedicated prose (Chloe's 2026-08-19 ruling: kept).
  // (2026-08-20) The PURPOSE is persisted for BOTH branches: the world branch
  // used to fold directive into <setting> (`setting || directive` — setting
  // wins, the purpose was LOST on disk and the load modal's Purpose row went
  // empty for every world card). <setting> now carries the setting alone
  // (either branch, when present); <plot> carries the directive as a
  // 'Premise:' line for world + the labeled scenario block — parseSimDraft
  // (worlds.js) + Rust's render_cache_block both read it back.
  const setting = text(d.setting).trim();
  if ((subtype === 'world' || subtype === 'scenario') && setting) {
    xml += `  <setting><![CDATA[\n${setting}\n  ]]></setting>\n`;
  }
  if (subtype === 'world') {
    const plot = composeLabeled([['Premise', d.directive]]);
    if (plot) xml += `  <plot><![CDATA[\n${plot.replace(/\n\n+/g, '\n')}\n  ]]></plot>\n`;
  }
  if (subtype === 'scenario') {
    const plot = composeLabeled([
      ['Premise', d.directive],
      ['Trigger', d.trigger_condition],
      ['Objective', d.primary_objective],
      ['Actors', d.participating_actors],
      ['Hazards', d.environmental_hazards],
      ['Outcomes', d.outcomes],
    ]);
    if (plot) xml += `  <plot><![CDATA[\n${plot.replace(/\n\n+/g, '\n')}\n  ]]></plot>\n`;
  }

  // Custom extensions: flat string→string map.
  const tagsXml = buildCustomTagsXml(d.custom_tags, d, subtype);
  if (tagsXml) xml += tagsXml;

  xml += '</sim_card>\n';

  // ── siblings (mutable world-state seeds — outside the cached root) ──
  const worldLines = [];
  if (date) worldLines.push(`Date: ${date}`);
  if (time) worldLines.push(`Time: ${time}`);
  if (weather) worldLines.push(`Weather: ${weather}`);
  if (tone) worldLines.push(`Tone: ${tone}`);
  if (worldLines.length) {
    xml += `\n<world><![CDATA[\n${worldLines.join('\n')}\n]]></world>\n`;
  }

  if (location) {
    xml += `\n<location><![CDATA[\n${location}\n]]></location>\n`;
  }

  // The opening beat lives as the SIBLING `<intro>` AFTER `</sim_card>` —
  // kept out of the cached root (prime directive). ONE write persists both.
  if (intro) {
    xml += `\n<intro><![CDATA[\n${intro}\n]]></intro>\n`;
  }

  // The npc `<inventory>` sibling: Clothing mandatory, the other lines
  // omitted when empty (the format rule). Equipped spelled correctly; the
  // Rust parser also tolerates the example.sim "Equppied" typo.
  if (subtype === 'npc') {
    const itemList = (v) => {
      const arr = typeof v === 'string' ? v.split(',') : v;
      if (!Array.isArray(arr)) return [];
      return arr.map((s) => text(s).trim()).filter(Boolean);
    };
    const clothing = itemList(d.clothing);
    const equipped = itemList(d.equipped);
    const accessories = itemList(d.accessories);
    const stored = itemList(d.stored != null ? d.stored : d.gear);
    const invLines = [];
    if (clothing.length) invLines.push(`Clothing: ${clothing.join(', ')}`);
    if (equipped.length) invLines.push(`Equipped: ${equipped.join(', ')}`);
    if (accessories.length) invLines.push(`Accessories: ${accessories.join(', ')}`);
    if (stored.length) invLines.push(`Stored: ${stored.join(', ')}`);
    if (invLines.length) {
      xml += `\n<inventory><![CDATA[\n${invLines.join('\n')}\n]]></inventory>\n`;
    }
  }

  // (2026-08-20 audit) The authored `<properties>` sibling — starting
  // holdings that seed the world economy at play start (ALL subtypes: an
  // npc card's property belongs to that character; scenario/world keep the
  // authored owner). Rust reads it via economy::parse_property_lines +
  // seeds at enter_fable_session. Omitted entirely when empty (the format
  // rule) — before this, the whole Rust-side seed pathway was unreachable
  // except by hand-editing XML in the raw editor.
  const propLines = renderPropertyLines(d.properties);
  if (propLines) {
    xml += `\n<properties><![CDATA[\n${propLines}\n]]></properties>\n`;
  }

  return { xml, intro };
}

// Compose an array of [label, prose] pairs into labeled lines (one per
// line — the v2 persona/plot line-block grammar), skipping empty pairs.
function composeLabeled(pairs) {
  const out = [];
  for (const [label, prose] of pairs) {
    const s = text(prose).trim();
    if (s) out.push(`${label}: ${s}`);
  }
  return out.join('\n');
}

// <custom_tags><entry key="…">value</entry>…</custom_tags> from a flat
// object. For npc drafts the conditional traits (breast/ears/tail/horn) —
// which have no v2 identity line — ride here when present. Empty string
// when no non-blank entries.
function buildCustomTagsXml(tags, draft, subtype) {
  const merged = {};
  if (tags && typeof tags === 'object' && !Array.isArray(tags)) {
    for (const [k, v] of Object.entries(tags)) {
      const kk = text(k).trim();
      const vv = text(v).trim();
      if (kk && vv) merged[kk] = vv;
    }
  }
  if (subtype === 'npc') {
    const conditional = [
      ['breast_size', draft.breast_size],
      ['ears', draft.ears],
      ['tail', draft.tail],
      ['horn', draft.horn],
    ];
    for (const [key, v] of conditional) {
      const s = text(v).trim();
      if (s && !merged[key]) merged[key] = s;
    }
  }
  const entries = Object.entries(merged);
  if (!entries.length) return '';
  const lines = entries.map(([k, v]) => `    <entry key="${escapeXml(k)}">${cdata(v)}</entry>`);
  return `  <custom_tags>\n${lines.join('\n')}\n  </custom_tags>\n`;
}

// Windows reserved base names (reserved with ANY extension — a folder or
// file stem hitting these makes create_dir_all / File::create fail
// opaquely).
const WINDOWS_RESERVED = new Set([
  'con', 'prn', 'aux', 'nul', 'conin$', 'conout$',
  ...Array.from({ length: 9 }, (_, i) => `com${i + 1}`),
  ...Array.from({ length: 9 }, (_, i) => `lpt${i + 1}`),
]);

export function slugify(s) {
  // The ID derivation (lowercase, unicode alnum kept, dash-run collapse,
  // 64 code-point cap, reserved suffix). The v2 identity split: this is the
  // `<metadata><id>` + memory-partition key; the FOLDER/FILE stem is the
  // display name (Rust's `safe_display_stem` mirrors it server-side).
  const slug = text(s).trim().toLowerCase()
    .replace(/[^\p{L}\p{N}_-]+/gu, '-')
    .replace(/-{2,}/g, '-')
    .replace(/^-+|-+$/g, '');
  const slugChars = [...slug];
  const capped = slugChars.length > 64
    ? slugChars.slice(0, 64).join('').replace(/-+$/g, '')
    : slug;
  if (WINDOWS_RESERVED.has(capped)) return `${capped}-card`;
  return capped;
}

// Convert the codex_entries array (from the Attach Codex slide) into the
// compound `.codex` text format that codex::parse_compound_text reads.
// Each entry: { title, tags (array), body }. Used by all three creators
// on CREATE (written via fable_codex_raw_set after the card folder exists).
export function codexEntriesToCompound(entries) {
  if (!Array.isArray(entries) || !entries.length) return '';
  // Front-matter hygiene: titles + tags are emitted into `title:`/`tags:`
  // lines, and the Rust parser splits entries on a blank-line-preceded
  // `---` fence — a GLM title containing newlines + `---\ntitle:` would
  // FORGE extra codex entries on reconcile. Collapse titles/tags to
  // single-line text + neutralize `---`-fence runs (bodies stay verbatim —
  // the parser owns body splitting).
  const fmInline = (s) => text(s)
    .replace(/[\r\n]+/g, ' ')
    .replace(/\s+/g, ' ')
    .replace(/-{3,}/g, '—')
    .trim();
  // Body-fence guard — the Rust parser splits entries on a
  // blank-line-preceded `---` followed by a `title:` line, so a lore body
  // containing that exact shape would FORGE an extra entry at the next
  // parse. Neutralize ONLY the colliding fences (`---` → `—`): ordinary
  // `---` rules and code fences (no following `title:` line) pass through
  // untouched. Mirrors the Rust round-trip guard in
  // `codex::format_compound_text`.
  const neutralizeBodyFences = (s) => text(s).split('\n').map((line, i, arr) => {
    if (line.trim() !== '---') return line;
    const prevBlank = i === 0 || arr[i - 1].trim() === '';
    const next = arr.slice(i + 1).find((l) => l.trim() !== '');
    const nextTitle = !!next && next.trim().toLowerCase().startsWith('title:');
    return (prevBlank && nextTitle) ? '—' : line;
  }).join('\n');
  // Normalize + drop entries with no body (an empty body would emit a junk
  // front-matter-only block the codex parser reads as an empty entry).
  const clean = entries.map((e) => {
    const src = e && typeof e === 'object' ? e : {};
    const rawTags = Array.isArray(src.tags)
      ? src.tags
      : (typeof src.tags === 'string' && src.tags.trim() ? [src.tags] : []);
    return {
      title: fmInline(src.title) || 'untitled',
      tags: rawTags.map((t) => fmInline(t)).filter(Boolean),
      body: neutralizeBodyFences(text(src.body).trim()),
    };
  }).filter((e) => e.body);
  return clean.map((e) => {
    let block = `---\ntitle: ${e.title}\n`;
    if (e.tags.length) block += `tags: ${e.tags.join(', ')}\n`;
    block += `---\n\n${e.body}`;
    return block;
  }).join('\n\n');
}
