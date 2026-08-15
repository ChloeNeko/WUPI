// =============================================================
// CARD SERIALIZE — the flat-format <sim_card> XML builders for the
// GLM-conversational Creator. The single polymorphic `serializeSimCard`
// routes on `draft.card_type` (npc | scenario | world — the retired
// per-kind builders were deleted once it absorbed their schemas).
//
// It emits the canonical flat format (AGENTS.md §6):
//   <sim_card>
//     <metadata><type>roleplay</type></metadata>   ← required for the
//                                                    New Game picker
//     <identity><name>…</name>…</identity>
//     <setting>…</setting> <plot>…</plot> <tone>…</tone>
//     <cast>…</cast> (optional)
//   </sim_card>
//
// THE INTRO LIVES AFTER </sim_card>, NOT INSIDE IT — exactly the shape
// `data/wupi.sim` uses. The opening narrator beat (the one-shot first-turn
// seed) is appended as a SIBLING `<intro>` element AFTER `</sim_card>` in the
// same `.sim` file. Baking it INTO `<sim_card>` would inflate every turn's KV
// cache with text only relevant to turn 1 — a prime-directive violation — so
// it rides outside the cached root. `serializeSimCard` returns { xml, intro }
// where `xml` already carries the appended `<intro>` block when non-empty, so
// ONE `fable_write_card` write persists both. (The dedicated intro step +
// import path replace it post-create via the Rust `fable_card_set_intro`
// IPC. NO separate `.intro` files are written — the in-file sibling is the
// canonical home; Rust parses the two-root shape by slicing at the first
// `</sim_card>`, see sim_card.rs `parse`.)
//
// CDATA wraps all prose (smart quotes, literal <>, auto-merged by
// roxmltree). The Rust parser (sim_card.rs) is the validator of record;
// fable_write_card rejects malformed XML server-side. `card_type` is
// always "roleplay" so the card appears in fable_cards_list + is startable
// via fable_start unchanged (NPC/World/Scenario are distinguished only by
// which fields the wizard collected, not by a new card_type — per the
// user's directive that they're "just sim cards in card folders").
//
// The persona block composes the wizard's prose fields into a single
// <persona> CDATA the narrator reads as the character/world voice. The
// exact composition differs per schema (see each builder).
// =============================================================

export function cdata(text) {
  return `<![CDATA[${String(text || '').replace(/]]>/g, ']]]]><![CDATA[>')}]]>`;
}

// Coerce ANY GLM draft value into a serialize-safe string. GLM occasionally
// emits numbers ("age": 24), arrays ("participating_actors": [...]), or
// nulls/objects despite the string schema — calling (v || '').trim() on those
// crashes CREATE with "(a || \"\").trim is not a function". Arrays join with
// ', ' (a list-ish value stays readable); booleans + plain objects are
// unrenderable → '' so the field reads as absent instead of poisoning the
// card with "[object Object]".
export function text(v) {
  if (v == null || typeof v === 'boolean') return '';
  if (Array.isArray(v)) return v.map(text).filter(Boolean).join(', ');
  if (typeof v === 'object') return '';
  return String(v);
}

export function escapeXml(s) {
  // `"` is escaped too so the fn is safe in ATTRIBUTE contexts (<node id="…">,
  // <entry key="…">) as well as element text — an unescaped quote in an
  // attribute emits broken XML caught server-side as a parse-error toast.
  return String(s || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// Compose an array of [label, prose] pairs into a single CDATA block,
// skipping empty pairs. Each non-empty pair becomes a labeled section so
// the narrator reads structured prose, not a wall of text. Used by the
// persona + appearance builders.
function composeLabeled(pairs) {
  const out = [];
  for (const [label, prose] of pairs) {
    const s = text(prose).trim();
    if (s) out.push(`${label}: ${s}`);
  }
  return out.join('\n\n');
}

// --- Player (SavedPlayer JSON, not a sim card) ---------------------------
// Build the SavedPlayer object (§6C) from the GLM draft. All structured
// traits are Option<String> on the Rust side; conditional + optional fields
// (breast/ears/tail/horn, clothing, job/weakness/distinguishing_marks,
// gear/tools/weapons, custom_tags) are omitted entirely when absent so the
// JSON stays clean. `gender` is free-form identity text (2026-08-13: no
// longer restricted to male/female, no longer force-lowercased — preserved
// as the user typed it). The transient gameplay fields (wealth/reputation/
// fame) are deliberately NOT serialized here — they seed PlayerState at
// attach, never the identity file (§6C identity-only lock). Returns
// { id, player } for fable_player_write({ id, player }).
export function serializePlayer(draft) {
  const d = draft || {};
  const name = text(d.name).trim();
  const opt = (v) => {
    const s = text(v).trim();
    return s || null;
  };
  // Chip-list normalizer: trim + drop blanks; null when nothing remains.
  // Accepts an array OR a comma-separated string (GLM sometimes emits
  // "tunic, boots" for an array field) — both become clean item lists.
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
    backstory: opt(d.backstory),
  };
  // Conditional + optional fields — include only when GLM supplied them.
  const breast = opt(d.breast_size); if (breast) player.breast_size = breast;
  const ears = opt(d.ears); if (ears) player.ears = ears;
  const tail = opt(d.tail); if (tail) player.tail = tail;
  const horn = opt(d.horn); if (horn) player.horn = horn;
  const job = opt(d.job); if (job) player.job = job;
  const weakness = opt(d.weakness); if (weakness) player.weakness = weakness;
  const marks = opt(d.distinguishing_marks); if (marks) player.distinguishing_marks = marks;
  const clothing = chipList(d.clothing); if (clothing) player.clothing = clothing;
  const gear = chipList(d.gear); if (gear) player.gear = gear;
  const tools = chipList(d.tools); if (tools) player.tools = tools;
  const weapons = chipList(d.weapons); if (weapons) player.weapons = weapons;
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
// The GLM sim wizard's Turn 1 is a TYPE ROUTER: it picks card_type ∈
// {npc, scenario, world} + name, then gathers only that branch's fields.
// `<type>` is ALWAYS "roleplay" (so fable_cards_list + fable_start match); the
// discriminator rides in `<subtype>` (2026-08-13). UNIVERSAL world anchors
// (date/time/weather/location) are gathered for every branch + never empty
// (GLM supplies defaults). Per-type fields compose into existing XML slots
// (per the SIM Wizard spec): NPC identity → <appearance>, dialogue_style →
// <conversational_style>, scenario specifics → <plot>, directive → <persona>.
// `custom_tags` (all branches) → <custom_tags><entry key="…">value</entry>.
// Cast ids are slugged bare; <name> carries the diegetic label. Returns
// { xml, intro }.
export function serializeSimCard(f) {
  const d = f || {};
  const subtype = text(d.card_type).trim().toLowerCase(); // npc | scenario | world
  const name = text(d.name).trim();
  const tone = text(d.tone).trim();
  const date = text(d.date).trim();
  const time = text(d.time || d.start_time).trim();
  const weather = text(d.weather || d.start_weather).trim();
  const location = text(d.location).trim();
  const locations = Array.isArray(d.locations) ? d.locations : [];
  const cast = Array.isArray(d.cast) ? d.cast : [];
  const intro = text(d.intro).trim();

  // Branch-specific <persona> composition.
  let persona = '';
  if (subtype === 'npc') {
    persona = composeLabeled([
      ['Personality', d.personality],
      ['Flaws', d.flaws],
      ['Occupation', d.job],
      ['Backstory', d.backstory],
    ]);
  } else {
    // scenario + world: the directive is the core premise / purpose.
    persona = text(d.directive).trim();
  }

  let xml = '<sim_card>\n';
  xml += '  <metadata><type>roleplay</type>';
  if (subtype) xml += `<subtype>${escapeXml(subtype)}</subtype>`;
  xml += '</metadata>\n';
  xml += '  <identity>\n';
  xml += `    <name>${escapeXml(name)}</name>\n`;
  if (persona) xml += `    <persona>${cdata(persona)}</persona>\n`;
  xml += '  </identity>\n';

  // NPC: physical identity → <appearance> (the 12 Player Wizard traits +
  // contextual breast/ears/tail/horn). Same vocabulary the tracker's
  // [APPEARANCE] allowlist speaks.
  if (subtype === 'npc') {
    const appearance = buildNpcAppearance(d);
    if (appearance) xml += `  <appearance>${cdata(appearance)}</appearance>\n`;
  }

  // World: the world's identity → <setting>.
  if (subtype === 'world') {
    const setting = text(d.setting).trim();
    if (setting) xml += `  <setting>${cdata(setting)}</setting>\n`;
  }

  // Scenario: the event spec → <plot> as labeled prose.
  if (subtype === 'scenario') {
    const plot = composeLabeled([
      ['Trigger', d.trigger_condition],
      ['Objective', d.primary_objective],
      ['Actors', d.participating_actors],
      ['Hazards', d.environmental_hazards],
      ['Outcomes', d.outcomes],
    ]);
    if (plot) xml += `  <plot>${cdata(plot)}</plot>\n`;
  }

  // NPC: dialogue_style → <conversational_style> (existing slot).
  if (subtype === 'npc') {
    const ds = text(d.dialogue_style).trim();
    if (ds) xml += `  <conversational_style>\n    <rules>${cdata(ds)}</rules>\n  </conversational_style>\n`;
  }

  if (tone) xml += `  <tone>${cdata(tone)}</tone>\n`;

  // Universal cold-start anchors: <start> seeds clock + weather + the calendar
  // label from turn 1 (the wizard guarantees date/time/weather non-empty).
  if (date || time || weather) {
    xml += '  <start>\n';
    if (date) xml += `    <date>${cdata(date)}</date>\n`;
    if (time) xml += `    <time>${escapeXml(time)}</time>\n`;
    if (weather) xml += `    <weather>${cdata(weather)}</weather>\n`;
    xml += '  </start>\n';
  }

  // Travel graph (flat-first; parser reads top-level <locations>). The
  // `location` anchor is the FIRST node; the graph grows in play. If the
  // wizard supplied a graph, use it; else synthesize one node from `location`.
  const nodes = locations.length
    ? locations
    : (location ? [{ name: location }] : []);
  if (nodes.length) {
    xml += '  <locations>\n';
    for (const raw of nodes) {
      // A bare string entry ("Village Square") is tolerated as {name}.
      const loc = typeof raw === 'string' ? { name: raw } : (raw || {});
      const lid = slugify(loc.name || loc.id);
      if (!lid) continue;
      const lname = text(loc.name).trim();
      const neigh = Array.isArray(loc.neighbors) ? loc.neighbors : [];
      xml += `    <node id="${escapeXml(lid)}">\n`;
      if (lname) xml += `      <name>${escapeXml(lname)}</name>\n`;
      for (const nb of neigh) {
        const nid = slugify(text(nb));
        if (nid) xml += `      <neighbor>${escapeXml(nid)}</neighbor>\n`;
      }
      xml += '    </node>\n';
    }
    xml += '  </locations>\n';
  }

  // Cast roster (flat-first; parser reads top-level <cast>). NPC branch: the
  // card's single NPC is the cast entry (role = job). scenario/world: any
  // starting cast the wizard declared (optional).
  const castList = subtype === 'npc'
    ? [{ name, identity: text(d.job).trim() }]
    : cast;
  if (castList.length) {
    xml += '  <cast>\n';
    for (const raw of castList) {
      // A bare string entry ("Mara the innkeep") is tolerated as {name}.
      const c = typeof raw === 'string' ? { name: raw } : (raw || {});
      const cid = slugify(c.name || c.id);
      if (!cid) continue;
      const cname = text(c.name).trim();
      const cidentity = text(c.identity || c.role).trim();
      xml += `    <npc id="${escapeXml(cid)}">\n`;
      if (cname) xml += `      <name>${escapeXml(cname)}</name>\n`;
      if (cidentity) xml += `      <role>${cdata(cidentity)}</role>\n`;
      xml += '    </npc>\n';
    }
    xml += '  </cast>\n';
  }

  // Custom extensions (all branches): flat string→string map.
  const tagsXml = buildCustomTagsXml(d.custom_tags);
  if (tagsXml) xml += tagsXml;

  xml += '</sim_card>\n';
  // The opening beat lives as a SIBLING `<intro>` AFTER </sim_card> (canonical,
  // 2026-08-13) — kept out of `<sim_card>` so it never inflates the cached
  // system prompt (prime directive). Rust reads it as the Fable opening beat.
  if (intro) {
    xml += `\n<intro>${cdata(intro)}</intro>\n`;
  }
  return { xml, intro };
}

// NPC <appearance> from the Player Wizard identity vocabulary. Each trait → a
// `tag: value` line (the parser reads <appearance> children as `tag: text`).
// Conditional traits (breast/ears/tail/horn) only when present.
function buildNpcAppearance(d) {
  const parts = [];
  const push = (tag, v) => {
    const s = text(v).trim();
    if (s) parts.push(`${tag}: ${s}`);
  };
  push('Gender', d.gender);
  push('Race', d.race);
  push('Age', d.age);
  push('Height', d.height);
  push('Weight', d.weight);
  push('Hair color', d.hair_color);
  push('Hair length', d.hair_length);
  push('Hair style', d.hair_style);
  push('Body', d.body_type);
  push('Skin', d.skin_complexion);
  push('Eyes', d.eye_color);
  if (Array.isArray(d.clothing) && d.clothing.length) {
    const clothes = d.clothing.map((s) => text(s).trim()).filter(Boolean);
    if (clothes.length) parts.push(`Clothing: ${clothes.join(', ')}`);
  }
  push('Breast', d.breast_size);
  push('Ears', d.ears);
  push('Tail', d.tail);
  push('Horn', d.horn);
  return parts.join('\n');
}

// <custom_tags><entry key="…">value</entry>…</custom_tags> from a flat object.
// Empty string when no non-blank entries. Keys are attribute-safe (quotes
// escaped); values are CDATA-wrapped (handles any prose safely).
function buildCustomTagsXml(tags) {
  if (!tags || typeof tags !== 'object' || Array.isArray(tags)) return '';
  const entries = [];
  for (const [k, v] of Object.entries(tags)) {
    const kk = text(k).trim();
    const vv = text(v).trim();
    if (kk && vv) {
      const keyAttr = escapeXml(kk);
      entries.push(`    <entry key="${keyAttr}">${cdata(vv)}</entry>`);
    }
  }
  if (!entries.length) return '';
  return `  <custom_tags>\n${entries.join('\n')}\n  </custom_tags>\n`;
}

// Windows reserved base names (reserved with ANY extension — a folder or file
// stem hitting these makes create_dir_all / File::create fail opaquely).
const WINDOWS_RESERVED = new Set([
  'con', 'prn', 'aux', 'nul', 'conin$', 'conout$',
  ...Array.from({ length: 9 }, (_, i) => `com${i + 1}`),
  ...Array.from({ length: 9 }, (_, i) => `lpt${i + 1}`),
]);

export function slugify(s) {
  const slug = text(s).trim().toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
  // A card named "Nul" or "Com1" must not slug onto a reserved name —
  // append a suffix so the folder create succeeds (the slug stays unique
  // per name; "nul" and "nul-card" can't collide).
  if (WINDOWS_RESERVED.has(slug)) return `${slug}-card`;
  return slug;
}

// Convert the codex_entries array (from the Attach Codex slide) into the
// compound `.codex` text format that codex::parse_compound_text reads.
// Each entry: { title, tags (array), body }. Used by all three creators
// on CREATE (written via fable_codex_raw_set after the card folder exists).
export function codexEntriesToCompound(entries) {
  if (!Array.isArray(entries) || !entries.length) return '';
  // Normalize + drop entries with no body (an empty body would emit a junk
  // front-matter-only block the codex parser reads as an empty entry).
  const clean = entries.map((e) => {
    const src = e && typeof e === 'object' ? e : {};
    const rawTags = Array.isArray(src.tags)
      ? src.tags
      : (typeof src.tags === 'string' && src.tags.trim() ? [src.tags] : []);
    return {
      title: text(src.title).trim() || 'untitled',
      tags: rawTags.map((t) => text(t).trim()).filter(Boolean),
      body: text(src.body).trim(),
    };
  }).filter((e) => e.body);
  return clean.map((e) => {
    let block = `---\ntitle: ${e.title}\n`;
    if (e.tags.length) block += `tags: ${e.tags.join(', ')}\n`;
    block += `---\n\n${e.body}`;
    return block;
  }).join('\n\n');
}
