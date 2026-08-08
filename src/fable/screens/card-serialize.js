// =============================================================
// CARD SERIALIZE — flat-format <sim_card> XML builders for the three
// new creators (NPC / World / Scenario). Shared helpers + one builder
// per schema so the XML shape is auditable in one place.
//
// All three emit the canonical flat format (AGENTS.md §6):
//   <sim_card>
//     <metadata><type>roleplay</type></metadata>   ← required for the
//                                                    New Game picker
//     <identity><name>…</name>…</identity>
//     <setting>…</setting> <plot>…</plot> <tone>…</tone>
//     <cast>…</cast> (optional)
//   </sim_card>
//
// THE INTRO IS NOT IN THE XML. The opening narrator beat (the one-shot
// first-turn seed) lives in a SIBLING `.intro` file, not inside the cached
// `<sim_card>` (2026-08-05). Baking it into the system prompt would inflate
// every turn's KV cache with text only relevant to turn 1 — a prime-directive
// violation. Each builder returns { xml, intro } where `intro` is the plain
// text for the `.intro` file (empty string when the wizard collected none);
// the write path writes them as two siblings.
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

export function escapeXml(s) {
  return String(s || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

// Compose an array of [label, prose] pairs into a single CDATA block,
// skipping empty pairs. Each non-empty pair becomes a labeled section so
// the narrator reads structured prose, not a wall of text. Used by the
// persona + appearance builders.
function composeLabeled(pairs) {
  const out = [];
  for (const [label, prose] of pairs) {
    const s = (prose || '').trim();
    if (s) out.push(`${label}: ${s}`);
  }
  return out.join('\n\n');
}

// --- NPC ------------------------------------------------------------------
// The NPC's persona is composed from Personality + Backstory + Likes/
// Dislikes + Mission. Occupation → a <traits> line. Conversation Style →
// folded into <tone>. Appearance traits → <appearance>. Intro → the sibling
// .intro file (NOT in the XML). The card is a roleplay card whose single
// <cast> entry IS this NPC (so fable_start seeds it as a present NPC).
//
// Returns { xml, intro }: xml is the <sim_card>, intro is the plain text for
// the .intro sibling (empty string when the wizard collected no intro).
export function serializeNpcCard(f) {
  const name = (f.name || '').trim();
  const persona = composeLabeled([
    ['Personality', f.personality],
    ['Backstory', f.backstory],
    ['Likes', f.likes],
    ['Dislikes', f.dislikes],
    ['Core Mission', f.core_mission],
    ['Miscellaneous', f.miscellaneous],
  ]);
  const occupation = (f.occupation || '').trim();
  const conversationStyle = (f.conversation_style || '').trim();
  const intro = (f.intro_message || '').trim();

  // Appearance from the structured traits (same vocabulary as the Player
  // Creator). Conditional traits only when enabled.
  const appearanceParts = [];
  const push = (label, v) => { const s = (v || '').trim(); if (s) appearanceParts.push(`${label}: ${s}`); };
  push('Gender', f.gender);
  push('Race', f.race);
  push('Age', f.age);
  push('Height', f.height);
  push('Weight', f.weight);
  push('Hair Color', f.hair_color);
  push('Hair Length', f.hair_length);
  push('Hair Style', f.hair_style);
  push('Body', f.body_type);
  push('Skin', f.skin_complexion);
  push('Eyes', f.eye_color);
  if (f.breast_size_enabled) push('Breast', f.breast_size);
  if (f.ears_enabled) push('Ears', f.ears);
  if (f.tail_enabled) push('Tail', f.tail);
  if (Array.isArray(f.clothing) && f.clothing.length) {
    appearanceParts.push(`Clothing: ${f.clothing.join(', ')}`);
  }
  const appearance = appearanceParts.join('\n');

  let xml = '<sim_card>\n';
  xml += '  <metadata><type>roleplay</type></metadata>\n';
  xml += '  <identity>\n';
  xml += `    <name>${escapeXml(name)}</name>\n`;
  if (persona) xml += `    <persona>${cdata(persona)}</persona>\n`;
  if (occupation) xml += `    <traits>${cdata(`Occupation: ${occupation}`)}</traits>\n`;
  xml += '  </identity>\n';
  if (appearance) xml += `  <appearance>${cdata(appearance)}</appearance>\n`;
  if (conversationStyle) xml += `  <tone>${cdata(conversationStyle)}</tone>\n`;
  // Cast: this card's NPC is present at scene start (slug id from name).
  xml += '  <cast>\n';
  const npcId = slugify(name) || 'npc';
  xml += `    <npc id="${escapeXml(npcId)}">\n      <name>${escapeXml(name)}</name>\n`;
  if (occupation) xml += `      <role>${cdata(occupation)}</role>\n`;
  xml += '    </npc>\n';
  xml += '  </cast>\n';
  xml += '</sim_card>\n';
  return { xml, intro };
}

// --- World ----------------------------------------------------------------
// A world card: the persona is the Directive (the world's driving
// principle). Setting + Tone are first-class. No cast, no intro (a world is
// a stage, not a character — it has no opening beat). Returns { xml, intro }
// with intro always '' for worlds.
export function serializeWorldCard(f) {
  const name = (f.name || '').trim();
  const directive = (f.directive || '').trim();
  const setting = (f.setting || '').trim();
  const tone = (f.tone || '').trim();

  let xml = '<sim_card>\n';
  xml += '  <metadata><type>roleplay</type></metadata>\n';
  xml += '  <identity>\n';
  xml += `    <name>${escapeXml(name)}</name>\n`;
  if (directive) xml += `    <persona>${cdata(directive)}</persona>\n`;
  xml += '  </identity>\n';
  if (setting) xml += `  <setting>${cdata(setting)}</setting>\n`;
  if (tone) xml += `  <tone>${cdata(tone)}</tone>\n`;
  xml += '</sim_card>\n';
  return { xml, intro: '' };
}

// --- Scenario -------------------------------------------------------------
// A scenario card: Directive → persona, Setting + Tone first-class, Intro →
// the sibling .intro file (NOT in the XML). No cast. Returns { xml, intro }.
export function serializeScenarioCard(f) {
  const name = (f.name || '').trim();
  const directive = (f.directive || '').trim();
  const setting = (f.setting || '').trim();
  const tone = (f.tone || '').trim();
  const intro = (f.intro_message || '').trim();

  let xml = '<sim_card>\n';
  xml += '  <metadata><type>roleplay</type></metadata>\n';
  xml += '  <identity>\n';
  xml += `    <name>${escapeXml(name)}</name>\n`;
  if (directive) xml += `    <persona>${cdata(directive)}</persona>\n`;
  xml += '  </identity>\n';
  if (setting) xml += `  <setting>${cdata(setting)}</setting>\n`;
  if (tone) xml += `  <tone>${cdata(tone)}</tone>\n`;
  xml += '</sim_card>\n';
  return { xml, intro };
}

export function slugify(s) {
  return (s || '').trim().toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
}

// Convert the codex_entries array (from the Attach Codex slide) into the
// compound `.codex` text format that codex::parse_compound_text reads.
// Each entry: { title, tags (array), body }. Used by all three creators
// on CREATE (written via fable_codex_raw_set after the card folder exists).
export function codexEntriesToCompound(entries) {
  if (!Array.isArray(entries) || !entries.length) return '';
  return entries.map((e) => {
    const title = (e.title || 'untitled').trim();
    const tags = Array.isArray(e.tags) ? e.tags.filter(Boolean) : [];
    const body = (e.body || '').trim();
    let block = `---\ntitle: ${title}\n`;
    if (tags.length) block += `tags: ${tags.join(', ')}\n`;
    block += `---\n\n${body}`;
    return block;
  }).join('\n\n');
}
