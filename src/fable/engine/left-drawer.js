// =============================================================
// FABLE LEFT DRAWER — the Card / Tracker / Codex authoring panel
// (ECHO rewrite, 2026-07-31).
//
// Mirrors the right-side Wupi drawer's slide mechanism (translateX), but on
// the LEFT edge, with three tabs across the top:
//   • Card    — edit the active sim card's info + first message (session-only).
//   • Tracker — view + manually edit the live tracked world state.
//   • Codex   — add/edit/delete lore entries live as you roleplay.
//
// Opened by a left-edge hover strip (symmetric with the right drawer's corner
// trigger) OR a small always-visible handle (discoverability — the right
// drawer is invisible by design, but a brand-new left panel needs a hint).
// Esc closes it (handled in stage.js's keydown, after the wupi drawer).
//
// The drawer is a pure-DOM overlay; all state lives in the Rust backend
// (fable_card_get/save, fable_schema_get/set, codex_list/save/delete). Each
// tab fetches its data on open + writes back on save.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

let drawerEl = null;     // the .fable-left-drawer root
let handleEl = null;     // the always-visible left-edge tab handle
let tabBtns = [];        // the three tab buttons
let tabPanels = {};      // tab-key → panel element
let activeTab = 'card';  // the currently-shown tab
let isOpen = false;

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

// Build the drawer DOM. Called once from stage.js buildStage (the element is
// reused across stage entries). Returns the drawer element.
export function buildLeftDrawer() {
  drawerEl = document.createElement('aside');
  drawerEl.className = 'fable-left-drawer';
  drawerEl.dataset.leftDrawer = '';
  drawerEl.setAttribute('aria-hidden', 'true');
  drawerEl.innerHTML = `
    <header class="fable-left-drawer__header">
      <div class="fable-left-drawer__tabs" role="tablist">
        <button class="fable-left-drawer__tab is-active" data-tab="card" role="tab">Card</button>
        <button class="fable-left-drawer__tab" data-tab="tracker" role="tab">Tracker</button>
      </div>
      <button class="fable-left-drawer__close" data-left-close aria-label="Close panel">✕</button>
    </header>
    <div class="fable-left-drawer__body">
      <section class="fable-left-drawer__panel is-active" data-panel="card"></section>
      <section class="fable-left-drawer__panel" data-panel="tracker"></section>
    </div>
  `;
  tabBtns = Array.from(drawerEl.querySelectorAll('[data-tab]'));
  for (const btn of tabBtns) {
    btn.addEventListener('click', () => switchTab(btn.dataset.tab));
  }
  drawerEl.querySelector('[data-left-close]').addEventListener('click', closeDrawer);
  for (const panel of drawerEl.querySelectorAll('[data-panel]')) {
    tabPanels[panel.dataset.panel] = panel;
  }

  // The always-visible handle (a slim tab on the left edge). Clicking opens.
  handleEl = document.createElement('button');
  handleEl.className = 'fable-left-drawer__handle';
  handleEl.dataset.leftHandle = '';
  handleEl.setAttribute('aria-label', 'Open card panel');
  handleEl.innerHTML = '<span class="fable-left-drawer__handle-icon">›</span>';
  handleEl.addEventListener('click', () => (isOpen ? closeDrawer() : openDrawer()));

  return drawerEl;
}

// Return the handle element so stage.js can mount it.
export function buildLeftHandle() {
  return handleEl;
}

export function openDrawer() {
  if (!drawerEl) return;
  isOpen = true;
  drawerEl.classList.add('open');
  drawerEl.setAttribute('aria-hidden', 'false');
  if (handleEl) handleEl.classList.add('is-hidden');
  // Refresh the active tab's data on every open (state may have changed).
  renderTab(activeTab);
}

export function closeDrawer() {
  if (!drawerEl) return;
  isOpen = false;
  drawerEl.classList.remove('open');
  drawerEl.setAttribute('aria-hidden', 'true');
  if (handleEl) handleEl.classList.remove('is-hidden');
}

export function isOpenState() {
  return isOpen;
}

function switchTab(key) {
  activeTab = key;
  for (const btn of tabBtns) {
    btn.classList.toggle('is-active', btn.dataset.tab === key);
  }
  for (const [k, panel] of Object.entries(tabPanels)) {
    panel.classList.toggle('is-active', k === key);
  }
  renderTab(key);
}

// Dispatch the active tab's render. Each render is async (fetches from the
// backend); failures show an inline error rather than crashing the panel.
async function renderTab(key) {
  const panel = tabPanels[key];
  if (!panel) return;
  try {
    if (key === 'card') await renderCard(panel);
    else if (key === 'tracker') await renderTracker(panel);
    // Codex tab removed (codex_save / codex_delete are dead stubs after the
    // codex lore-RAG module was deleted; codex_list always returns empty).
  } catch (err) {
    panel.innerHTML = `<div class="fable-left-drawer__error">Couldn't load: ${esc(err)}</div>`;
  }
}

// ── Card tab ────────────────────────────────────────────────────────────
// Edit the active card's name / persona / setting / tone / first message /
// player name. Session-only (fable_card_save updates the in-memory card +
// re-seats the persona; the .sim file on disk is not modified).
async function renderCard(panel) {
  let card = null;
  try {
    card = await invoke('fable_card_get');
  } catch (err) {
    panel.innerHTML = `<div class="fable-left-drawer__error">${esc(err)}</div>`;
    return;
  }
  if (!card) {
    panel.innerHTML = `<div class="fable-left-drawer__empty">No active card. Start a game to edit its info.</div>`;
    return;
  }
  panel.innerHTML = `
    <div class="fable-left-drawer__form" data-card-form>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Name</span>
        <input type="text" data-card-name value="${esc(card.name || '')}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Player name</span>
        <input type="text" data-card-player value="${esc(card.player_name || '')}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Setting</span>
        <textarea data-card-setting rows="3">${esc(card.setting || '')}</textarea>
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Tone</span>
        <input type="text" data-card-tone value="${esc(card.tone || '')}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Core persona</span>
        <textarea data-card-persona rows="4">${esc(card.core_persona || '')}</textarea>
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">First message (opening scene)</span>
        <textarea data-card-opening rows="5">${esc(card.opening_scene || '')}</textarea>
      </label>
      <div class="fable-left-drawer__actions">
        <button class="fable-left-drawer__btn primary" data-card-save>Save (live)</button>
        <span class="fable-left-drawer__note">Session-only — applies to the next turn, not saved to disk.</span>
      </div>
    </div>
  `;
  const form = panel.querySelector('[data-card-form]');
  const status = document.createElement('span');
  status.className = 'fable-left-drawer__status';
  form.querySelector('[data-card-save]').addEventListener('click', async () => {
    const fields = {
      name: form.querySelector('[data-card-name]').value,
      player_name: form.querySelector('[data-card-player]').value,
      setting: form.querySelector('[data-card-setting]').value,
      tone: form.querySelector('[data-card-tone]').value,
      core_persona: form.querySelector('[data-card-persona]').value,
      opening_scene: form.querySelector('[data-card-opening]').value,
    };
    status.textContent = 'Saving…';
    try {
      await invoke('fable_card_save', { fields });
      status.textContent = 'Saved — applies to the next turn.';
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
  form.querySelector('.fable-left-drawer__actions').appendChild(status);
}

// ── Tracker tab ─────────────────────────────────────────────────────────
// View + edit the live WorldSchema. The full schema is large, so we surface
// the core human-meaningful fields as editable inputs: summary, the entities
// map (one textarea, `key: value` per line), the world clock, weather
// condition, current location, active rumors, and recent events. Saving
// rebuilds the full schema (preserving the unedited typed fields) + writes it
// back via fable_schema_set.
async function renderTracker(panel) {
  let schema = null;
  try {
    schema = await invoke('fable_schema_get');
  } catch (err) {
    panel.innerHTML = `<div class="fable-left-drawer__error">${esc(err)}</div>`;
    return;
  }
  if (!schema) {
    panel.innerHTML = `<div class="fable-left-drawer__empty">No active game.</div>`;
    return;
  }
  // Flatten the editable fields for the form.
  const summary = schema.summary || '';
  const entities = schema.entities || {};
  const entityText = Object.entries(entities).map(([k, v]) => `${k}: ${v}`).join('\n');
  const clock = schema.world_clock || {};
  const weather = (schema.weather && schema.weather.condition) || '';
  const graph = schema.travel_graph || {};
  const currentNode = graph.current_node || '';
  const rumors = Array.isArray(schema.rumors) ? schema.rumors : [];
  const rumorText = rumors.map((r) => (r && r.label) ? r.label : '').filter(Boolean).join('\n');
  const events = Array.isArray(schema.recent_events) ? schema.recent_events.join('\n') : '';

  panel.innerHTML = `
    <div class="fable-left-drawer__form" data-tracker-form>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Summary</span>
        <textarea data-trk-summary rows="3">${esc(summary)}</textarea>
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Entities (key: value, one per line)</span>
        <textarea data-trk-entities rows="6">${esc(entityText)}</textarea>
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Time — day</span>
        <input type="number" data-trk-day value="${esc(String(clock.day ?? 1))}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Time — minutes (0–1439)</span>
        <input type="number" data-trk-minutes value="${esc(String(clock.current_minutes ?? 0))}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Weather</span>
        <input type="text" data-trk-weather value="${esc(weather)}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Current location (node id)</span>
        <input type="text" data-trk-location value="${esc(currentNode)}">
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Rumors (one per line)</span>
        <textarea data-trk-rumors rows="4">${esc(rumorText)}</textarea>
      </label>
      <label class="fable-left-drawer__field">
        <span class="fable-left-drawer__label">Recent events (one per line)</span>
        <textarea data-trk-events rows="4">${esc(events)}</textarea>
      </label>
      <div class="fable-left-drawer__actions">
        <button class="fable-left-drawer__btn primary" data-trk-save>Save</button>
        <span class="fable-left-drawer__note">Edits the live world state. Undoable via the history ring buffer.</span>
      </div>
    </div>
  `;
  const form = panel.querySelector('[data-tracker-form]');
  const status = document.createElement('span');
  status.className = 'fable-left-drawer__status';
  form.querySelector('[data-trk-save]').addEventListener('click', async () => {
    status.textContent = 'Saving…';
    try {
      const next = JSON.parse(JSON.stringify(schema)); // preserve typed fields
      next.summary = form.querySelector('[data-trk-summary]').value;
      next.recent_events = splitLines(form.querySelector('[data-trk-events]').value);
      // Rebuild the entities map from the textarea.
      const entText = form.querySelector('[data-trk-entities]').value;
      const ent = {};
      for (const line of splitLines(entText)) {
        const i = line.indexOf(':');
        if (i > 0) {
          const k = line.slice(0, i).trim();
          const v = line.slice(i + 1).trim();
          if (k) ent[k] = v;
        }
      }
      next.entities = ent;
      // Clock.
      if (!next.world_clock) next.world_clock = {};
      next.world_clock.day = numOr(form.querySelector('[data-trk-day]').value, 1);
      next.world_clock.current_minutes = numOr(form.querySelector('[data-trk-minutes]').value, 0);
      // Weather.
      const w = form.querySelector('[data-trk-weather]').value.trim();
      if (w) {
        if (!next.weather) next.weather = {};
        next.weather.condition = w;
      } else if (next.weather) {
        next.weather.condition = '';
      }
      // Current location.
      if (next.travel_graph) {
        next.travel_graph.current_node = form.querySelector('[data-trk-location]').value.trim() || null;
      }
      // Rumors: rebuild as bare labels rooted at the current node (the simplest
      // faithful representation; the full Rumor struct is preserved for
      // existing rumors whose label is unchanged).
      const newLabels = splitLines(form.querySelector('[data-trk-rumors]').value);
      const existingByLabel = new Map((rumors || []).filter(r => r && r.label).map(r => [r.label, r]));
      const root = next.travel_graph && next.travel_graph.current_node;
      next.rumors = newLabels.map((label) => existingByLabel.get(label) || {
        label,
        origin_node: root,
        known_nodes: root ? [root] : [],
        born_minutes: next.world_clock.current_minutes || 0,
      });
      await invoke('fable_schema_set', { schemaJson: next });
      status.textContent = 'Saved.';
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
  form.querySelector('.fable-left-drawer__actions').appendChild(status);
}

// ── Codex tab REMOVED (2026-07-31) ───────────────────────────────────────
// The codex_save / codex_delete IPCs are dead stubs (the codex lore-RAG
// module was deleted), and codex_list always returns empty. The tab was
// removed with them. Live lore authoring will return when a new lore
// surface replaces the deleted RAG layer.

// ── helpers ─────────────────────────────────────────────────────────────
function splitLines(s) {
  return String(s || '').split('\n').map((l) => l.trim()).filter(Boolean);
}
function numOr(s, fallback) {
  const n = Number.parseInt(String(s || ''), 10);
  return Number.isFinite(n) ? n : fallback;
}

// Hard reset (called from teardownStage on stage exit so a close mid-edit
// can't leave the drawer open or stale on the next session).
export function resetLeftDrawer() {
  closeDrawer();
  activeTab = 'card';
}
