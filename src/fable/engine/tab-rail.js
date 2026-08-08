// =============================================================
// FABLE TAB RAIL — the tracked-stat icon rail under the WUPI brand.
//
// Sits in the RIGHT Wupi drawer between the brand header and the chat
// messages. Five tabs: Player · Sim Card · Codex · World · NPC. Clicking an
// icon toggles a prose dropdown (JSON/XML-free, friendly labels). One tab
// active at a time (glowing); re-click or pick another closes/swaps.
//
// TAB STATE + DRAWER CLOSE (2026-08-06):
//   The dropdown state (`activeTab`) is SEPARATE from the drawer-open state so
//   the rail owns its own state. When the Wupi drawer closes (mouseleave / close
//   button), `closeDrawer()` calls `resetTabRail()` → `setActiveTab(null)`,
//   collapsing the dropdown + deactivating the icon so nothing persists behind
//   a closed drawer. (The prior persist-on-close behavior was retired to match
//   the left drawer.) `renderActive()` remains for the reopen-after-edit path.
//
// Each dropdown has a ✎ icon (top-right) that opens the raw-file editor
// (engine/raw-editor.js) loaded with that tab's file. The prose fields save
// through the existing live-edit IPCs (fable_card_save, fable_schema_set) or
// the new raw-slice IPCs (fable_json_raw_set).
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { openRawEditor } from './raw-editor.js';

// The five tabs in rail order. `icon` is an inline SVG (minimalist brass).
// `file` is the raw-editor target (`kind` for fable_json_raw_* / special for
// card + codex).
const TABS = [
  { key: 'player',  label: 'Player',   file: { kind: 'player' } },
  { key: 'card',    label: 'Sim Card', file: { kind: 'card' } },
  { key: 'codex',   label: 'Codex',    file: { kind: 'codex' } },
  { key: 'world',   label: 'World',    file: { kind: 'world' } },
  { key: 'npc',     label: 'NPC',      file: { kind: 'npc' } },
];

const ICONS = {
  // A simple person silhouette.
  player: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="7" r="3.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M5 20c0-3.9 3.1-7 7-7s7 3.1 7 7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>',
  // A card / document.
  card: '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="4" y="3" width="16" height="18" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8 8h8M8 12h8M8 16h5" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>',
  // An open book (codex / lore).
  codex: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 6c-1.5-1.2-3.8-2-6-2v13c2.2 0 4.5.8 6 2 1.5-1.2 3.8-2 6-2V4c-2.2 0-4.5.8-6 2z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M12 6v13" fill="none" stroke="currentColor" stroke-width="1.4"/></svg>',
  // A globe (world).
  world: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M3 12h18M12 3c2.5 2.5 2.5 15 0 18M12 3c-2.5 2.5-2.5 15 0 18" fill="none" stroke="currentColor" stroke-width="1.4"/></svg>',
  // Two figures (NPC / cast).
  npc: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="8" cy="8" r="2.8" fill="none" stroke="currentColor" stroke-width="1.5"/><circle cx="16" cy="8" r="2.8" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M3 19c0-3 2.2-5 5-5s5 2 5 5M11 19c0-3 2.2-5 5-5s5 2 5 5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>',
};

let railEl = null;       // the .fable-tab-rail root
let dropdownEl = null;   // the .fable-tab-dropdown (the prose panel)
let tabBtns = {};        // key → button element
let activeTab = null;    // the currently-active tab key (null = none)

// Build the rail DOM. Called once from stage.js buildStage; injected into the
// Wupi drawer between the brand header and the messages list. Returns the
// rail element (stage.js mounts it).
export function buildTabRail() {
  const wrap = document.createElement('div');
  wrap.className = 'fable-tab-rail-wrap';
  wrap.innerHTML = `
    <div class="fable-tab-rail-divider"></div>
    <div class="fable-tab-rail" role="tablist" aria-label="Tracked stats">
      ${TABS.map((t) => `
        <button class="fable-tab-rail__btn" data-tab="${t.key}" role="tab"
                aria-selected="false" title="${t.label}" aria-label="${t.label}">
          ${ICONS[t.key] || ''}
        </button>
      `).join('')}
    </div>
    <div class="fable-tab-dropdown" data-tab-dropdown hidden></div>
  `;
  railEl = wrap.querySelector('.fable-tab-rail');
  dropdownEl = wrap.querySelector('[data-tab-dropdown]');
  for (const btn of railEl.querySelectorAll('[data-tab]')) {
    const key = btn.dataset.tab;
    tabBtns[key] = btn;
    btn.addEventListener('click', () => onTabClick(key));
  }
  return wrap;
}

// Single-active toggle. Clicking the active tab closes it; clicking another
// swaps (old un-toggles, new toggles). No two active at once.
function onTabClick(key) {
  if (activeTab === key) {
    setActiveTab(null);
    return;
  }
  setActiveTab(key);
}

function setActiveTab(key) {
  activeTab = key;
  for (const [k, btn] of Object.entries(tabBtns)) {
    const on = k === key;
    btn.classList.toggle('is-active', on);
    btn.setAttribute('aria-selected', on ? 'true' : 'false');
  }
  if (key) {
    renderActive();
  } else {
    dropdownEl.hidden = true;
    dropdownEl.innerHTML = '';
  }
}

// Render the active tab's dropdown. Called on tab-select AND on drawer
// reopen (so the dropdown reappears after a mouseleave auto-close — the
// hover-reopen-keeps-active mechanic). Async (fetches the tab's data).
export function renderActive() {
  if (!activeTab || !dropdownEl) return;
  renderTab(activeTab, dropdownEl);
}

export function activeTabKey() { return activeTab; }

// ── Per-tab renderers ───────────────────────────────────────────────────
// Each builds the prose dropdown (friendly, no JSON/XML): a header row with
// the tab label + a ✎ raw-edit icon, then the tab's content. The ✎ opens the
// raw editor (engine/raw-editor.js) for that tab's file.

async function renderTab(key, el) {
  el.hidden = false;
  const meta = TABS.find((t) => t.key === key);
  // Header (label + ✎). Built once; the body is filled by the tab renderer.
  el.innerHTML = `
    <div class="fable-tab-drop__head">
      <span class="fable-tab-drop__title">${meta.label}</span>
      <button class="fable-tab-drop__edit" data-raw-edit="${meta.file.kind}"
              title="Edit raw file" aria-label="Edit raw file">✎</button>
    </div>
    <div class="fable-tab-drop__body" data-drop-body></div>
  `;
  el.querySelector('[data-raw-edit]').addEventListener('click', () => {
    openRawEditor(meta.file.kind);
  });
  const body = el.querySelector('[data-drop-body]');
  try {
    if (key === 'player') await renderPlayer(body);
    else if (key === 'card') await renderCard(body);
    else if (key === 'codex') await renderCodex(body);
    else if (key === 'world') await renderWorld(body);
    else if (key === 'npc') await renderNpc(body);
  } catch (err) {
    body.innerHTML = `<div class="fable-tab-drop__error">Couldn't load: ${esc(err)}</div>`;
  }
}

// ── Player tab ──────────────────────────────────────────────────────────
// player_state_get → { body: { head: "Transparent", ... }, stamina, wealth,
// reputation }. Body keys are the 16 stable ids; values are PascalCase states.
async function renderPlayer(body) {
  let ps;
  try {
    ps = await invoke('player_state_get');
  } catch (err) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  const bodyParts = ps.body || {};
  // Friendly labels for the body-part ids (the 16 stable ids from player_state.rs).
  const PART_LABELS = {
    head: 'Head', torso: 'Torso',
    left_bicep: 'Left Arm', left_forearm: 'Left Forearm', left_hand: 'Left Hand',
    right_bicep: 'Right Arm', right_forearm: 'Right Forearm', right_hand: 'Right Hand',
    left_thigh: 'Left Thigh', left_calf: 'Left Calf', left_ankle: 'Left Ankle', left_foot: 'Left Foot',
    right_thigh: 'Right Thigh', right_calf: 'Right Calf', right_ankle: 'Right Ankle', right_foot: 'Right Foot',
  };
  // Split body into status rows (only non-default/"Transparent" parts are
  // interesting; the healthy default is shown as a summary, not 16 rows).
  const injured = Object.entries(bodyParts)
    .filter(([_, v]) => v && v !== 'Transparent')
    .map(([k, v]) => `<div class="fable-drop-row"><span>${esc(PART_LABELS[k] || prettify(k))}</span><span class="fable-drop-val ${stateClass(v)}">${esc(prettifyState(v))}</span></div>`);
  const stamina = ps.stamina || 'Fresh';
  const wealth = ps.wealth ?? 0;
  const rep = ps.reputation ?? 0;
  body.innerHTML = `
    <div class="fable-drop-row"><span>Stamina</span><span class="fable-drop-val">${esc(prettifyState(stamina))}</span></div>
    <div class="fable-drop-row"><span>Gold</span><span class="fable-drop-val">${esc(String(wealth))}</span></div>
    <div class="fable-drop-row"><span>Reputation</span><span class="fable-drop-val ${rep < 0 ? 'bad' : rep > 0 ? 'good' : ''}">${esc(repLabel(rep))}</span></div>
    <div class="fable-drop-section">Body</div>
    ${injured.length
      ? injured.join('')
      : '<div class="fable-drop-row"><span class="fable-drop-muted">Uninjured.</span></div>'}
  `;
}

// ── Sim Card tab ────────────────────────────────────────────────────────
// fable_card_get → the editable scalar fields. Edits go through the existing
// session-only fable_card_save (live narrator pick-up next turn).
async function renderCard(body) {
  let card;
  try {
    card = await invoke('fable_card_get');
  } catch (err) {
    body.innerHTML = emptyBox('No active card.');
    return;
  }
  if (!card) {
    body.innerHTML = emptyBox('No active card.');
    return;
  }
  body.innerHTML = `
    <div class="fable-drop-form" data-card-form>
      ${field('Name', 'name', card.name || '')}
      ${field('Player name', 'player_name', card.player_name || '')}
      ${field('Setting', 'setting', card.setting || '', 3)}
      ${field('Tone', 'tone', card.tone || '')}
      ${field('Core persona', 'persona', card.core_persona || '', 4)}
      <div class="fable-drop-actions">
        <button class="fable-drop-btn primary" data-card-save>Save (live)</button>
        <span class="fable-drop-status"></span>
      </div>
    </div>
  `;
  const form = body.querySelector('[data-card-form]');
  const status = form.querySelector('.fable-drop-status');
  form.querySelector('[data-card-save]').addEventListener('click', async () => {
    const fields = {
      name: form.querySelector('[data-f="name"]').value,
      player_name: form.querySelector('[data-f="player_name"]').value,
      setting: form.querySelector('[data-f="setting"]').value,
      tone: form.querySelector('[data-f="tone"]').value,
      core_persona: form.querySelector('[data-f="persona"]').value,
    };
    status.textContent = 'Saving…';
    try {
      await invoke('fable_card_save', { fields });
      status.textContent = 'Saved — applies next turn.';
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
}

// ── Codex tab ───────────────────────────────────────────────────────────
// fable_codex_get → { raw, entries: [{title, tags, body}] }. The authored hard
// rules of the world. Add/edit/delete entries; save serializes back via
// fable_codex_raw_set. Editing the entries list rebuilds the raw compound text.
let codexEntries = [];  // working copy (edited in-place by the UI)
async function renderCodex(body) {
  let read;
  try {
    read = await invoke('fable_codex_get');
  } catch (err) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  codexEntries = (read.entries || []).map((e) => ({ title: e.title || '', tags: [...(e.tags || [])], body: e.body || '' }));
  paintCodex(body);
}
function paintCodex(body) {
  body.innerHTML = `
    <div class="fable-codex-add">
      <button class="fable-drop-btn" data-codex-add>+ Add entry</button>
    </div>
    <div class="fable-codex-list"></div>
    <div class="fable-drop-actions">
      <button class="fable-drop-btn primary" data-codex-save>Save</button>
      <span class="fable-drop-status"></span>
    </div>
  `;
  const list = body.querySelector('.fable-codex-list');
  if (!codexEntries.length) {
    list.innerHTML = '<div class="fable-drop-muted">No codex entries yet — the hard rules of this world.</div>';
  }
  codexEntries.forEach((entry, i) => list.appendChild(codexEntryCard(entry, i, body)));
  body.querySelector('[data-codex-add]').addEventListener('click', () => {
    codexEntries.push({ title: 'New Entry', tags: [], body: '' });
    paintCodex(body);
  });
  body.querySelector('[data-codex-save]').addEventListener('click', () => saveCodex(body));
}
function codexEntryCard(entry, i, body) {
  const card = document.createElement('div');
  card.className = 'fable-codex-item';
  card.innerHTML = `
    <input class="fable-codex-title" data-f="title" value="${esc(entry.title)}" placeholder="Title">
    <input class="fable-codex-tags" data-f="tags" value="${esc(entry.tags.join(', '))}" placeholder="tags, comma, separated">
    <textarea class="fable-codex-body" data-f="body" rows="4" placeholder="The rule / lore body…">${esc(entry.body)}</textarea>
    <button class="fable-codex-del" data-del>Remove</button>
  `;
  card.querySelector('[data-f="title"]').addEventListener('input', (e) => { codexEntries[i].title = e.target.value; });
  card.querySelector('[data-f="tags"]').addEventListener('input', (e) => {
    codexEntries[i].tags = e.target.value.split(',').map((t) => t.trim()).filter(Boolean);
  });
  card.querySelector('[data-f="body"]').addEventListener('input', (e) => { codexEntries[i].body = e.target.value; });
  card.querySelector('[data-del]').addEventListener('click', () => {
    codexEntries.splice(i, 1);
    paintCodex(body);
  });
  return card;
}
async function saveCodex(body) {
  const status = body.querySelector('.fable-drop-status');
  status.textContent = 'Saving…';
  // Serialize entries back to the compound .codex format (mirrors Rust's
  // codex::format_compound_text). Built client-side so the save is one IPC.
  let text = '';
  for (const e of codexEntries) {
    if (!e.body.trim() && !e.title.trim()) continue;
    if (text) text += '\n\n';
    text += '---\ntitle: ' + e.title + '\n';
    if (e.tags.length) text += 'tags: ' + e.tags.join(', ') + '\n';
    text += '---\n\n' + e.body.trim() + '\n';
  }
  try {
    await invoke('fable_codex_raw_set', { text });
    status.textContent = 'Saved.';
  } catch (err) {
    status.textContent = `Failed: ${err}`;
  }
}

// ── World tab ───────────────────────────────────────────────────────────
// fable_schema_get → the full WorldSchema. Edits the friendly fields, saves
// via the existing fable_schema_set (writes the live world state).
async function renderWorld(body) {
  let schema;
  try {
    schema = await invoke('fable_schema_get');
  } catch (err) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  if (!schema) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  const clock = schema.world_clock || {};
  const weather = (schema.weather && schema.weather.condition) || '';
  const currentNode = (schema.travel_graph && schema.travel_graph.current_node) || '';
  const rumors = Array.isArray(schema.rumors) ? schema.rumors : [];
  const rumorText = rumors.map((r) => (r && r.label) ? r.label : '').filter(Boolean).join('\n');
  const events = Array.isArray(schema.recent_events) ? schema.recent_events.join('\n') : '';
  const entities = schema.entities || {};
  const worldEntText = Object.entries(entities)
    .filter(([k]) => !k.startsWith('npc.'))
    .map(([k, v]) => `${k}: ${v}`).join('\n');
  body.innerHTML = `
    <div class="fable-drop-form" data-world-form>
      ${field('Summary', 'summary', schema.summary || '', 3)}
      ${field('Time — day', 'day', String(clock.day ?? 1))}
      ${field('Time — minutes (0–1439)', 'minutes', String(clock.current_minutes ?? 0))}
      ${field('Weather', 'weather', weather)}
      ${field('Current location', 'location', currentNode)}
      ${field('Rumors (one per line)', 'rumors', rumorText, 4)}
      ${field('Recent events (one per line)', 'events', events, 4)}
      ${field('Tracked details (key: value, one per line)', 'entities', worldEntText, 5)}
      <div class="fable-drop-actions">
        <button class="fable-drop-btn primary" data-world-save>Save</button>
        <span class="fable-drop-status"></span>
      </div>
    </div>
  `;
  const form = body.querySelector('[data-world-form]');
  const status = form.querySelector('.fable-drop-status');
  form.querySelector('[data-world-save]').addEventListener('click', async () => {
    status.textContent = 'Saving…';
    try {
      const next = JSON.parse(JSON.stringify(schema));
      next.summary = form.querySelector('[data-f="summary"]').value;
      next.recent_events = splitLines(form.querySelector('[data-f="events"]').value);
      // Rebuild non-npc entities from the textarea; preserve npc.* entries.
      const entText = form.querySelector('[data-f="entities"]').value;
      const ent = {};
      for (const [k, v] of Object.entries(next.entities || {})) {
        if (k.startsWith('npc.')) ent[k] = v;  // preserve
      }
      for (const line of splitLines(entText)) {
        const idx = line.indexOf(':');
        if (idx > 0) ent[line.slice(0, idx).trim()] = line.slice(idx + 1).trim();
      }
      next.entities = ent;
      if (!next.world_clock) next.world_clock = {};
      next.world_clock.day = numOr(form.querySelector('[data-f="day"]').value, 1);
      next.world_clock.current_minutes = numOr(form.querySelector('[data-f="minutes"]').value, 0);
      const w = form.querySelector('[data-f="weather"]').value.trim();
      if (!next.weather) next.weather = {};
      next.weather.condition = w;
      if (next.travel_graph) next.travel_graph.current_node = form.querySelector('[data-f="location"]').value.trim() || null;
      const newLabels = splitLines(form.querySelector('[data-f="rumors"]').value);
      const existingByLabel = new Map((rumors || []).filter((r) => r && r.label).map((r) => [r.label, r]));
      const root = next.travel_graph && next.travel_graph.current_node;
      next.rumors = newLabels.map((label) => existingByLabel.get(label) || {
        label, origin_node: root, known_nodes: root ? [root] : [],
        born_minutes: next.world_clock.current_minutes || 0,
      });
      await invoke('fable_schema_set', { schemaJson: next });
      status.textContent = 'Saved.';
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
}

// ── NPC tab ─────────────────────────────────────────────────────────────
// fable_schema_get → the npc.* entities + npc_registry. Each NPC shown as a
// name + disposition card. Edits the npc.* entity values; saves via
// fable_json_raw_set('npc', ...) which recomposes the npc slice live.
async function renderNpc(body) {
  let schema;
  try {
    schema = await invoke('fable_schema_get');
  } catch (err) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  if (!schema) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  const npcs = Object.entries(schema.entities || {}).filter(([k]) => k.startsWith('npc.'));
  if (!npcs.length) {
    body.innerHTML = emptyBox('No NPCs tracked yet.');
    return;
  }
  body.innerHTML = `
    <div class="fable-npc-list" data-npc-list>
      ${npcs.map(([k, v]) => `
        <div class="fable-npc-card" data-key="${esc(k)}">
          <input class="fable-npc-name" value="${esc(prettifyNpcKey(k))}" readonly>
          <textarea class="fable-npc-state" rows="2">${esc(v || '')}</textarea>
        </div>
      `).join('')}
    </div>
    <div class="fable-drop-actions">
      <button class="fable-drop-btn primary" data-npc-save>Save</button>
      <span class="fable-drop-status"></span>
    </div>
  `;
  const list = body.querySelector('[data-npc-list]');
  const status = body.querySelector('.fable-drop-status');
  body.querySelector('[data-npc-save]').addEventListener('click', async () => {
    status.textContent = 'Saving…';
    try {
      // Build the npc slice JSON: { entities: { npc.*: editedValue } }.
      const entities = {};
      for (const cardEl of list.querySelectorAll('.fable-npc-card')) {
        const key = cardEl.dataset.key;
        const val = cardEl.querySelector('.fable-npc-state').value;
        entities[key] = val;
      }
      const npcJson = JSON.stringify({ entities }, null, 2);
      await invoke('fable_json_raw_set', { kind: 'npc', json: npcJson });
      status.textContent = 'Saved.';
    } catch (err) {
      status.textContent = `Failed: ${err}`;
    }
  });
}

// Reset the rail to its beginning state: collapse the open dropdown + strip the
// glowing is-active state from every icon. Called from TWO places now: (1)
// teardownStage on stage exit (the original hard-reset), + (2) wupi-drawer's
// closeDrawer so an open tab never persists behind a closed drawer (Chloe
// 2026-08-06: match the left drawer's reset-on-close; the prior persist-on-
// close behavior is retired). Touches ONLY the tab rail — the Wupi chat
// history lives in a separate element + is never cleared here.
export function resetTabRail() {
  setActiveTab(null);
}

// ── helpers ─────────────────────────────────────────────────────────────
function esc(s) {
  return String(s ?? '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
function prettify(k) {
  return String(k).replace(/_/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}
function prettifyState(s) {
  // The PascalCase enum names → "Fresh", "Winded", etc. Already friendly;
  // just space-out camel boundaries defensively.
  return String(s || '').replace(/([a-z])([A-Z])/g, '$1 $2');
}
function prettifyNpcKey(k) {
  return String(k).replace(/^npc\./, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}
function stateClass(v) {
  const s = String(v || '').toLowerCase();
  if (/wound|injur|bleed|broken|crippled|sever|dead|dying|critical/.test(s)) return 'bad';
  if (/bruis|cut|scrap|sore|winded|tired|fatigue/.test(s)) return 'warn';
  return '';
}
function repLabel(rep) {
  if (rep > 5) return `Renowned (${rep})`;
  if (rep > 0) return `Liked (${rep})`;
  if (rep < -5) return `Infamous (${rep})`;
  if (rep < 0) return `Distrusted (${rep})`;
  return 'Neutral (0)';
}
function splitLines(s) {
  return String(s || '').split('\n').map((l) => l.trim()).filter(Boolean);
}
function numOr(s, fallback) {
  const n = Number.parseInt(String(s || ''), 10);
  return Number.isFinite(n) ? n : fallback;
}
// A labeled form field (reused by Card + World tabs). `rows` makes a textarea.
function field(label, name, value, rows) {
  const v = esc(value);
  if (rows) {
    return `<label class="fable-drop-field"><span class="fable-drop-label">${esc(label)}</span><textarea data-f="${name}" rows="${rows}">${v}</textarea></label>`;
  }
  return `<label class="fable-drop-field"><span class="fable-drop-label">${esc(label)}</span><input data-f="${name}" value="${v}"></label>`;
}
function emptyBox(msg) {
  return `<div class="fable-drop-empty">${esc(msg)}</div>`;
}
