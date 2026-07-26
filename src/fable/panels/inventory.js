// =============================================================
// PANEL: INVENTORY — read view over item_/inv_ entities.
// Entities with id starting item_ or inv_ are inventory slots.
// The state string is the item's status/description (e.g. "equipped",
// "3 in pack", "shiny"). Rendered as a grid of parchment slots.
// =============================================================

const PREFIXES = ['item_', 'inv_'];

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function renderInventory(entities, schema) {
  const items = Object.entries(entities || {})
    .filter(([id]) => PREFIXES.some((p) => id.startsWith(p)));
  const head = `<div class="panel-head">
      <h2>Inventory</h2>
      <p class="panel-hint">What you're carrying.</p>
    </div>`;
  if (!items.length) {
    return head + emptyState();
  }
  const slots = items.map(([id, state]) => slot(id, state)).join('');
  return head + `<div class="panel-grid inventory-grid">${slots}</div>`;
}

function slot(id, state) {
  const name = prettify(id);
  const hasState = state && state.trim() && state.toLowerCase() !== 'true';
  return `<div class="panel-slot">
    <div class="panel-slot-glyph">${glyphFor(id)}</div>
    <div class="panel-slot-name">${esc(name)}</div>
    ${hasState ? `<div class="panel-slot-state">${esc(state)}</div>` : ''}
  </div>`;
}

function emptyState() {
  return `<div class="panel-empty">
    <p>Your pack is light.</p>
    <p class="panel-empty-hint">Ask Wupi to add or find an item.</p>
  </div>`;
}

function prettify(id) {
  return id.replace(/^(item_|inv_)/, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

// Cheap glyph pick by keyword in the id — no asset dependency.
function glyphFor(id) {
  const s = id.toLowerCase();
  if (/sword|blade|axe|dagger|spear|mace/.test(s)) return '⚔';
  if (/bow|arrow|crossbow/.test(s)) return '➶';
  if (/shield|buckler/.test(s)) return '🛡';
  if (/potion|flask|vial|elixir/.test(s)) return '⚗';
  if (/coin|gold|gem|jewel|necklace|ring/.test(s)) return '✦';
  if (/key/.test(s)) return '⚷';
  if (/book|scroll|tome|map|letter|note/.test(s)) return '📜';
  if (/food|bread|meat|apple|ration/.test(s)) return '🍖';
  if (/torch|lantern|candle|lamp/.test(s)) return '🔥';
  if (/cloak|robe|armor|helmet|boot|glove/.test(s)) return '✦';
  return '◆';
}
