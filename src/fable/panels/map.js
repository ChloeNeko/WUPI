// =============================================================
// PANEL: MAP — location pins from loc_ entities, on a CSS backdrop.
// Pins are loc_* entities; the state string is the location's status
// (e.g. "explored", "current", "locked").
//
// ATLAS HISTORY: this panel used to render one of three atlas PNGs
// (map-{fantasy,futuristic,modern}-atlas.png) as the backdrop, picked by
// the card's tone. Those 3 PNGs were deleted 2026-07-27 (they were the
// only references to themselves and shipped ~7.5MB of dead weight). The
// panel now renders pins over a pure-CSS parchment backdrop (see
// .panel-map-bg in fable.css). setMapTheme() is retained as a no-op so
// stage.js's existing call site doesn't need to change — a future atlas
// (e.g. procedurally drawn, or a single themed PNG under assets/) can
// re-light it without touching the call site.
// =============================================================

// Retained for API compatibility (stage.js calls setMapTheme('fantasy') at
// init). Currently a no-op — no atlas PNG to switch. If a future atlas
// returns, store the theme here and consume it in renderMap.
let _theme = 'fantasy';
export function setMapTheme(theme) {
  if (typeof theme === 'string' && theme) _theme = theme;
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
         <div class="panel-map-bg" data-theme="${esc(_theme)}"></div>
         <div class="panel-empty">
           <p>No locations charted yet.</p>
           <p class="panel-empty-hint">Travel somewhere and ask Wupi to chart it.</p>
         </div>
       </div>`;
  }
  const pins = locs.map(([id, state], i) => pin(id, state, i, locs.length)).join('');
  return head +
    `<div class="panel-map">
       <div class="panel-map-bg" data-theme="${esc(_theme)}"></div>
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
