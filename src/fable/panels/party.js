// =============================================================
// PANEL: PARTY — read view over npc_ entities (the named cast).
// Each npc_ entity's state is their current disposition/relationship
// (e.g. "wary", "trusted ally", "hostile"). Rendered as character
// cards with a glyph + state chip.
// =============================================================

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function renderParty(entities, schema) {
  const npcs = Object.entries(entities || {})
    .filter(([id]) => id.startsWith('npc_'));
  const head = `<div class="panel-head">
      <h2>The Cast</h2>
      <p class="panel-hint">Who's in this with you.</p>
    </div>`;
  if (!npcs.length) {
    return head + `<div class="panel-empty">
      <p>No one notable tracked yet.</p>
      <p class="panel-empty-hint">Meet someone and ask Wupi to remember them.</p>
    </div>`;
  }
  const cards = npcs.map(([id, state]) => npcCard(id, state)).join('');
  return head + `<div class="party-grid">${cards}</div>`;
}

function npcCard(id, state) {
  const name = prettify(id);
  const rel = relationClass(state);
  return `<div class="party-card party-${rel}">
    <div class="party-card-glyph">${glyphFor(name)}</div>
    <div class="party-card-body">
      <div class="party-card-name">${esc(name)}</div>
      <div class="party-card-state">${esc(state || 'an unknown figure')}</div>
    </div>
  </div>`;
}

function relationClass(state) {
  const s = (state || '').toLowerCase();
  if (/hostile|enemy|hates?|angry|furious/.test(s)) return 'hostile';
  if (/wary|suspicious|distrust|unease/.test(s)) return 'wary';
  if (/neutral|indifferent|stranger/.test(s)) return 'neutral';
  if (/friendly|ally|trusted|devoted|loyal/.test(s)) return 'ally';
  return 'neutral';
}

function prettify(id) {
  return id.replace(/^npc_/, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

function glyphFor(name) {
  // Crude role guess from name keywords — no asset dependency.
  const n = name.toLowerCase();
  if (/keeper|barkeep|innkeeper|bartender/.test(n)) return '🍺';
  if (/guard|soldier|warrior|knight/.test(n)) return '⚔';
  if (/mage|wizard|witch|sorcer/.test(n)) return '✦';
  if (/stranger|hooded|cloaked/.test(n)) return '🕵';
  if (/merchant|trader|shopkeep/.test(n)) return '⚖';
  if (/child|kid|young/.test(n)) return '✿';
  return '☻';
}
