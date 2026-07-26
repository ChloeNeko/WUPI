// =============================================================
// PANEL: CRAFT — generic crafting grid (forge/alchemy/kitchen).
// Reads forge_/alchemy_/kitchen_/craft_ entity prefixes. If empty,
// shows an inspirational empty state pointing the player at Wupi.
// =============================================================

const PREFIXES = ['forge_', 'alchemy_', 'kitchen_', 'craft_'];

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function renderCraft(entities, schema) {
  const entries = Object.entries(entities || {})
    .filter(([id]) => PREFIXES.some((p) => id.startsWith(p)));
  const head = `<div class="panel-head">
      <h2>Workshop</h2>
      <p class="panel-hint">Forge, alchemy, kitchen.</p>
    </div>`;
  if (!entries.length) {
    return head + `<div class="panel-empty">
      <p>The workbench is bare.</p>
      <p class="panel-empty-hint">Ask Wupi to set up a recipe or gather materials.</p>
    </div>`;
  }
  const slots = entries.map(([id, state]) => slot(id, state)).join('');
  return head + `<div class="panel-grid craft-grid">${slots}</div>`;
}

function slot(id, state) {
  const [kind, ...rest] = id.split(/_(.+)/);
  const name = (rest.join('') || id).replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
  return `<div class="panel-slot craft-slot craft-${kind}">
    <div class="panel-slot-glyph">${glyph(kind)}</div>
    <div class="panel-slot-name">${esc(name)}</div>
    ${state ? `<div class="panel-slot-state">${esc(state)}</div>` : ''}
  </div>`;
}

function glyph(kind) {
  if (kind === 'forge') return '⚒';
  if (kind === 'alchemy') return '⚗';
  if (kind === 'kitchen') return '🍲';
  return '✦';
}
