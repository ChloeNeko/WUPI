// =============================================================
// PANEL: MAP — atlas PNG + location pins from loc_ entities.
// The card's tone picks the atlas (fantasy/futuristic/modern).
// Pins are loc_* entities; the state string is the location's
// status (e.g. "explored", "current", "locked").
// =============================================================

// Atlas PNG per theme. Restored from v0.6.5 (the Phase 0a asset wipe had
// stubbed these to ''). The 3 atlas PNGs ship in public/ and are picked by
// the card's tone via mapThemeForTone() in stage.js → setMapTheme(theme).
// Relative URLs resolve under Tauri's custom protocol (base: "./").
const ATLASES = {
  fantasy: './map-fantasy-atlas.png',
  futuristic: './map-futuristic-atlas.png',
  modern: './map-modern-atlas.png',
};
let atlas = ATLASES.fantasy; // default; overridable via setMapTheme on game start

export function setMapTheme(theme) {
  // Resolve the theme string to one of the 3 known atlases. Unknown themes
  // fall back to fantasy (the most common card tone). Empty/null stays at
  // the current atlas (defensive — stage.js always passes a real theme).
  if (theme && ATLASES[theme]) atlas = ATLASES[theme];
}

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function renderMap(entities, schema) {
  const locs = Object.entries(entities || {})
    .filter(([id]) => id.startsWith('loc_'));
  const head = `<div class="panel-head">
      <h2>World Map</h2>
      <p class="panel-hint">Where you've been.</p>
    </div>`;
  if (!locs.length) {
    return head +
      `<div class="panel-map-empty">
         <img src="${atlas}" alt="" class="panel-map-bg" />
         <div class="panel-empty">
           <p>No locations charted yet.</p>
           <p class="panel-empty-hint">Travel somewhere and ask Wupi to chart it.</p>
         </div>
       </div>`;
  }
  const pins = locs.map(([id, state], i) => pin(id, state, i, locs.length)).join('');
  return head +
    `<div class="panel-map">
       <img src="${atlas}" alt="" class="panel-map-bg" />
       <div class="panel-map-pins">${pins}</div>
     </div>`;
}

// Deterministic pseudo-random pin position from the id hash, spread
// across the atlas. No real geography here — a read view of state.
function pin(id, state, i, total) {
  const name = prettify(id);
  const x = (hash(id) % 80) + 10;       // 10–90%
  const y = ((hash(id) >> 8) % 70) + 15; // 15–85%
  const status = statusClass(state);
  const marker = status === 'current' ? '★' : status === 'explored' ? '●' : '◯';
  return `<div class="panel-pin pin-${status}" style="left:${x}%;top:${y}%">
    <span class="panel-pin-marker">${marker}</span>
    <span class="panel-pin-label">${esc(name)}</span>
  </div>`;
}

function statusClass(state) {
  const s = (state || '').toLowerCase();
  if (s.includes('current') || s.includes('here')) return 'current';
  if (s.includes('explored') || s.includes('visited')) return 'explored';
  if (s.includes('locked') || s.includes('unknown') || s.includes('sealed')) return 'locked';
  return 'known';
}

function prettify(id) {
  return id.replace(/^loc_/, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

// FNV-1a 32-bit → stable spread from a string.
function hash(s) {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = (h + ((h << 1) + (h << 4) + (h << 7) + (h << 8) + (h << 24))) >>> 0;
  }
  return h;
}
