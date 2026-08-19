// =============================================================
// FABLE TAB RAIL — the tracked-stat icon rail under the WUPI brand.
//
// Sits in the RIGHT Wupi drawer between the brand header and the chat
// messages. Five tabs: Player · Sim Card · Codex · World · NPC. Clicking an
// icon toggles a read-only PROSE dropdown (JSON/XML-free, friendly labels)
// that shows ONLY what the simulation is actively tracking — empty/untracked
// fields are hidden, never shown as blank inputs. One tab active at a time
// (glowing); re-click or pick another closes/swaps.
//
// READ-ONLY BY DESIGN (2026-08-11): the dropdowns are VISUAL AID references.
// You cannot edit anything inline. To change tracked state, either talk to
// WUPI (she mutates it through the simulation) or press the ✎ icon (top-right
// of each dropdown) to open the raw-file editor (engine/raw-editor.js). The
// four prose tabs (Card/Codex/World/NPC) used to render live-editable forms +
// Save buttons; those were retired because (a) the dropdowns are meant to be
// read-only references, and (b) the NPC inline-save wrote a partial npc slice
// that risked wiping npc_registry/relationships/presences. Editing now flows
// exclusively through the ✎ raw-editor path or WUPI chat.
//
// TAB STATE + DRAWER CLOSE (2026-08-06):
//   The dropdown state (`activeTab`) is SEPARATE from the drawer-open state so
//   the rail owns its own state. When the Wupi drawer closes (mouseleave / close
//   button), `closeDrawer()` calls `resetTabRail()` → `setActiveTab(null)`,
//   collapsing the dropdown + deactivating the icon so nothing persists behind
//   a closed drawer. (The prior persist-on-close behavior was retired to match
//   the left drawer.) `renderActive()` remains for the reopen-after-edit path:
//   it's also the onSaved callback handed to the raw editor so the dropdown
//   re-reads immediately after a ✎ save.
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
// reopen (the rail resets on close — resetTabRail nulls activeTab — so the
// reopen path re-selects a tab before this runs). Async (fetches the tab's
// data).
export function renderActive() {
  if (!activeTab || !dropdownEl) return;
  renderTab(activeTab, dropdownEl);
}

// ── Per-tab renderers ───────────────────────────────────────────────────
// Each builds the read-only prose dropdown (friendly, no JSON/XML): a header
// row with the tab label + a ✎ raw-edit icon, then the tab's content. The ✎
// opens the raw editor (engine/raw-editor.js) for that tab's file; on a
// successful save the editor calls back into renderActive so this dropdown
// re-reads immediately.

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
    // onSaved: re-render this dropdown so the read-only view reflects the
    // just-saved raw edit immediately (no stale display).
    openRawEditor(meta.file.kind, renderActive);
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

// ── Player tab (character sheet, READ-ONLY) ─────────────────────────────
// Pulls from THREE sources + renders each section only when it has content:
//   • fable_active_player_get → identity (name/race/gender/age/height/weight)
//     + backstory. None for a playerless game → section hidden.
//   • player_state_get        → appearance deltas, vitals, injuries, inventory.
//   • fable_schema_get        → relationships (keyed by npc id). Best-effort.
// Editing happens via the ✎ raw editor or by talking to WUPI; nothing here
// is editable inline.
async function renderPlayer(bodyEl) {
  let ps;
  try {
    ps = await invoke('player_state_get');
  } catch (err) {
    bodyEl.innerHTML = emptyBox('No active game.');
    return;
  }
  // Identity + relationships schema are best-effort: a playerless game has no
  // attached player (identity stays hidden) + a schema read failure just hides
  // relationships. Vitals/injuries/inventory always come from player_state.
  const [player, schema] = await Promise.all([
    invoke('fable_active_player_get').then((p) => p || null).catch(() => null),
    invoke('fable_schema_get').catch(() => null),
  ]);

  const parts = [];
  const identity = renderIdentity(player);
  if (identity) parts.push(identity);
  const appearance = renderAppearance(ps);
  if (appearance) parts.push(appearance);
  parts.push(renderVitals(ps));
  const injuries = renderInjuries(ps);
  if (injuries) parts.push(injuries);
  const inventory = renderInventory(ps);
  if (inventory) parts.push(inventory);
  const relationships = renderRelationships(schema);
  if (relationships) parts.push(relationships);
  bodyEl.innerHTML = parts.join('');
}

// Identity: name (bold header) + stable trait rows + optional backstory.
// Omitted entirely when there's no attached player.
function renderIdentity(player) {
  if (!player) return '';
  const out = [];
  if (nonEmpty(player.name)) out.push(`<div class="fable-drop-name">${esc(player.name)}</div>`);
  const traitRows = [
    ['Race', player.race], ['Gender', player.gender], ['Age', player.age],
    ['Height', player.height], ['Weight', player.weight],
  ].filter(([, v]) => nonEmpty(v));
  if (traitRows.length) out.push(traitRows.map(([k, v]) => row(k, v)).join(''));
  if (nonEmpty(player.backstory)) out.push(proseBlock('Backstory', player.backstory));
  return out.length ? out.join('') : '';
}

// Appearance: the live `current_appearance_deltas` (hair/body/skin/eyes/
// scars/wounds/etc.) — what the character's BODY currently looks like in
// this game. Clothing is NOT here (2026-08-18): garments are equipped
// items, rendered by the Equipment section. Hidden entirely when no deltas
// are tracked.
function renderAppearance(ps) {
  const deltas = ps.current_appearance_deltas || {};
  const rows = Object.entries(deltas)
    .filter(([, v]) => nonEmpty(v))
    .map(([k, v]) => row(prettify(k), v));
  return rows.length ? section('Appearance', rows.join('')) : '';
}

// Vitals: always present (stamina/gold/reputation have defaults).
function renderVitals(ps) {
  const stam = staminaInfo(ps.stamina);
  const wealth = ps.wealth ?? 0;
  const rep = ps.reputation ?? 0;
  return section('Vitals',
    row('Stamina', stam.label, stam.cls)
    + row('Gold', String(wealth))
    + row('Reputation', repLabel(rep), rep < 0 ? 'bad' : rep > 0 ? 'good' : ''));
}

// Injuries: only the 22 parts that are NOT Healthy, each with its severity
// label + any wound descriptors from `injury_details`. Returns '' when
// uninjured (no section rendered).
function renderInjuries(ps) {
  const bodyMap = ps.body || {};
  const details = ps.injury_details || {};
  const rows = [];
  for (const [part, state] of Object.entries(bodyMap)) {
    const info = injuryState(state);
    if (info.label === 'Healthy') continue;
    rows.push(row(pascalToWords(part), info.label, info.cls));
    const descs = (details[part] || []).filter((d) => nonEmpty(d));
    if (descs.length) rows.push(`<div class="fable-drop-injury-detail">${esc(descs.join('; '))}</div>`);
  }
  return rows.length ? section('Injuries', rows.join('')) : '';
}

// Inventory: equipped (6 slots, outer + inner layers) + belt (≤4) + pack
// (unbounded). Each subsection hidden when empty. Item tags render as chips.
function renderInventory(ps) {
  const out = [];
  // Equipped — slot keys are the snake_case EquipSlot wire ids.
  const eq = ps.equipment || {};
  const slotRows = Object.keys(eq)
    .map((slot) => {
      const layers = eq[slot] || {};
      const items = [];
      if (layers.outer && nonEmpty(layers.outer.name)) {
        items.push(`<span class="fable-inv-item">${esc(layers.outer.name)}${tagChips(layers.outer.tags)}</span>`);
      }
      if (layers.inner && nonEmpty(layers.inner.name)) {
        items.push(`<span class="fable-inv-item"><span class="fable-inv-layer">under:</span> ${esc(layers.inner.name)}${tagChips(layers.inner.tags)}</span>`);
      }
      if (!items.length) return '';
      return `<div class="fable-inv-slot"><span class="fable-inv-slot-label">${esc(EQUIP_SLOT_LABELS[slot] || prettify(slot))}</span><span class="fable-inv-slot-items">${items.join('')}</span></div>`;
    })
    .filter(Boolean);
  if (slotRows.length) out.push(section('Equipped', slotRows.join('')));

  const belt = Array.isArray(ps.belt) ? ps.belt : [];
  if (belt.length) out.push(section('Belt', stackList(belt)));
  const pack = Array.isArray(ps.pack) ? ps.pack : [];
  if (pack.length) out.push(section('Pack', stackList(pack)));
  return out.join('');
}

// Relationships: per-NPC tier (nemesis → bonded). Keyed by npc id on the
// WorldSchema. Hidden entirely when none are tracked.
function renderRelationships(schema) {
  const rels = (schema && schema.relationships) || {};
  const rows = Object.entries(rels).map(([k, v]) => {
    const tier = tierInfo((v && v.tier) || 'stranger');
    return `<div class="fable-drop-row"><span>${esc(prettifyNpcKey(k))}</span><span class="fable-drop-tier ${tier.cls}">${esc(tier.label)}</span></div>`;
  });
  return rows.length ? section('Relationships', rows.join('')) : '';
}

// ── Sim Card tab (READ-ONLY) ────────────────────────────────────────────
// fable_card_get → the card's identity fields. Rendered as read-only prose —
// each field hidden when empty so the dropdown shows only what's authored.
// Editing happens via the ✎ raw editor (writes the .sim) or by talking to
// WUPI; there is no inline form.
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
  const parts = [];
  if (nonEmpty(card.name)) parts.push(row('Name', card.name));
  if (nonEmpty(card.subtype)) parts.push(row('Type', card.subtype));
  if (nonEmpty(card.player_name)) parts.push(row('Playing as', card.player_name));
  // The v2 <identity> line block (present for every card — the KV-cache
  // payload; missing traits are simply absent).
  const id = card.identity || {};
  const idRows = [
    ['Gender', id.gender], ['Race', id.race], ['Age', id.age],
    ['Height', id.height], ['Weight', id.weight], ['Body', id.body],
    ['Skin', id.skin], ['Eyes', id.eyes],
    ['Hair Color', id.hair_color], ['Hair Length', id.hair_length],
    ['Hair Style', id.hair_style],
  ].filter(([, v]) => nonEmpty(v)).map(([l, v]) => row(l, v)).join('');
  if (idRows) parts.push(section('Identity', idRows));
  // The <persona> line block (npc cards carry every label; absent otherwise).
  const p = card.persona || {};
  const personaRows = [
    ['Personality', p.personality], ['Conversation Style', p.conversation_style],
    ['Likes', p.likes], ['Dislikes', p.dislikes], ['Flaws', p.flaws],
    ['Goals', p.goals], ['Occupation', p.occupation], ['Backstory', p.backstory],
  ].filter(([, v]) => nonEmpty(v)).map(([l, v]) => row(l, v)).join('');
  if (personaRows) parts.push(section('Persona', personaRows));
  if (nonEmpty(card.setting)) parts.push(proseBlock('Setting', card.setting));
  if (nonEmpty(card.plot)) parts.push(proseBlock('Plot', card.plot));
  // The <world> sibling anchors (the mutable seeds — the live values live in
  // the World tab; this shows what the card shipped).
  const w = card.world || {};
  const worldRows = [
    ['Date', w.date], ['Time', w.time], ['Weather', w.weather], ['Tone', w.tone],
    ['Location', card.location],
  ].filter(([, v]) => nonEmpty(v)).map(([l, v]) => row(l, v)).join('');
  if (worldRows) parts.push(section('World Anchors', worldRows));
  // The <inventory> sibling (npc cards).
  const inv = card.inventory || {};
  const invRows = [
    ['Clothing', inv.clothing], ['Equipped', inv.equipped],
    ['Accessories', inv.accessories], ['Stored', inv.stored],
  ].filter(([, v]) => Array.isArray(v) && v.length).map(([l, v]) => row(l, v.join(', '))).join('');
  if (invRows) parts.push(section('Inventory', invRows));
  const tags = card.custom_tags || {};
  const tagRows = Object.entries(tags).filter(([, v]) => nonEmpty(v)).map(([k, v]) => row(k, v)).join('');
  if (tagRows) parts.push(section('Custom Tags', tagRows));
  body.innerHTML = parts.length ? parts.join('') : emptyBox('No active card.');
}

// ── Codex tab (READ-ONLY) ───────────────────────────────────────────────
// fable_codex_get → { raw, entries: [{title, tags, body}] }. The authored hard
// rules of the world, shown read-only. Add/edit/delete happens via the ✎ raw
// editor (writes the .codex) or by talking to WUPI; there is no inline form.
async function renderCodex(body) {
  let read;
  try {
    read = await invoke('fable_codex_get');
  } catch (err) {
    body.innerHTML = emptyBox('No active game.');
    return;
  }
  const entries = (read && read.entries) || [];
  if (!entries.length) {
    body.innerHTML = '<div class="fable-drop-empty">No codex entries for this world.</div>';
    return;
  }
  body.innerHTML = `<div class="fable-codex-list">${entries.map(codexReadCard).join('')}</div>`;
}
function codexReadCard(e) {
  const title = (e.title || '').trim();
  const tags = (e.tags || []).map((t) => String(t).trim()).filter(Boolean);
  const tagLine = tags.length ? `<div class="fable-drop-tags">${esc(tags.join(', '))}</div>` : '';
  const bodyText = (e.body || '').trim();
  // Skip a fully-blank entry rather than render an empty card.
  if (!title && !tags.length && !bodyText) return '';
  return `<div class="fable-codex-item">
    ${title ? `<div class="fable-drop-title">${esc(title)}</div>` : ''}
    ${tagLine}
    ${bodyText ? `<div class="fable-drop-prose">${esc(bodyText)}</div>` : ''}
  </div>`;
}

// ── World tab (READ-ONLY) ───────────────────────────────────────────────
// fable_schema_get → the full WorldSchema. Rendered as read-only prose, each
// block shown ONLY when populated: dormant clock/weather/location are hidden
// (zero tracked state → zero rows), so a fresh game shows nothing misleading.
// Editing happens via the ✎ raw editor (writes world.json) or by talking to
// WUPI; there is no inline form.
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
  const mins = Number(clock.current_minutes || 0);
  const weather = (schema.weather && schema.weather.condition) || '';
  const node = (schema.travel_graph && schema.travel_graph.current_node) || '';
  const rumors = (Array.isArray(schema.rumors) ? schema.rumors : [])
    .map((r) => (r && r.label ? r.label : null))
    .filter(Boolean);
  const events = (Array.isArray(schema.recent_events) ? schema.recent_events : [])
    .filter((e) => nonEmpty(e));
  const worldEnts = Object.entries(schema.entities || {})
    .filter(([k]) => !k.startsWith('npc.'))
    .filter(([, v]) => nonEmpty(v));

  const parts = [];
  if (mins > 0) parts.push(row('Time', clockLabel(mins)));
  if (weather.trim()) parts.push(row('Weather', weather.trim()));
  if (node.trim()) parts.push(row('Location', prettify(node)));
  if (nonEmpty(schema.summary)) parts.push(proseBlock('Summary', schema.summary));
  if (rumors.length) parts.push(listBlock('Rumors', rumors));
  if (events.length) parts.push(listBlock('Recent events', events.slice(-5)));
  if (worldEnts.length) parts.push(listBlock('Tracked details', worldEnts.map(([k, v]) => `${prettify(k)}: ${v}`)));
  body.innerHTML = parts.length ? parts.join('') : '<div class="fable-drop-empty">World state not yet established.</div>';
}

// ── NPC tab (READ-ONLY) ─────────────────────────────────────────────────
// fable_schema_get → the npc.* entities. Each NPC shown as a read-only name +
// state card. The old inline-save path wrote a partial npc slice that risked
// wiping npc_registry/relationships/presences — it's gone. Editing happens via
// the ✎ raw editor (writes npc.json) or by talking to WUPI.
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
  body.innerHTML = `<div class="fable-npc-list">${npcs.map(([k, v]) => npcReadCard(k, v)).join('')}</div>`;
}
function npcReadCard(key, val) {
  const name = prettifyNpcKey(key);
  const state = nonEmpty(val) ? String(val).trim() : '';
  return `<div class="fable-npc-card">
    <div class="fable-drop-title">${esc(name)}</div>
    ${state ? `<div class="fable-drop-prose">${esc(state)}</div>` : ''}
  </div>`;
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
// True when a value holds non-blank content (string or otherwise).
function nonEmpty(v) {
  return v !== null && v !== undefined && String(v).trim() !== '';
}
function prettify(k) {
  return String(k).replace(/_/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());
}
function prettifyNpcKey(k) {
  return String(k).replace(/^npc\./, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}
// BodyPart wire keys are PascalCase ("LeftUpperArm") → "Left Upper Arm".
function pascalToWords(k) {
  return String(k || '').replace(/([a-z0-9])([A-Z])/g, '$1 $2');
}
// BodyPartState wire (PascalCase or lowercase) → { label, cls }. The Healthy
// (Transparent) state is the sentinel renderInjuries skips.
const INJURY_STATES = {
  transparent: { label: 'Healthy', cls: '' },
  yellow:      { label: 'Minor Injury', cls: 'warn' },
  orange:      { label: 'Medium Injury', cls: 'warn' },
  red:         { label: 'Heavy Injury', cls: 'bad' },
  purple:      { label: 'Critical', cls: 'bad' },
  black:       { label: 'Amputated', cls: 'bad' },
};
function injuryState(v) {
  const info = INJURY_STATES[String(v || '').toLowerCase()];
  return info || { label: pascalToWords(v) || '—', cls: '' };
}
// Stamina enum wire → { label, cls }.
const STAMINA_LABELS = {
  depleted:  { label: 'Depleted', cls: 'bad' },
  exhausted: { label: 'Exhausted', cls: 'warn' },
  winded:    { label: 'Winded', cls: 'warn' },
  active:    { label: 'Active', cls: 'good' },
  fresh:     { label: 'Fresh', cls: 'good' },
};
function staminaInfo(s) {
  const key = String(s || '').toLowerCase();
  return STAMINA_LABELS[key] || { label: pascalToWords(s) || 'Fresh', cls: '' };
}
// RelationshipTier wire (snake_case) → { label, cls }. The label is left
// lowercase; the .fable-drop-tier CSS capitalizes it for display.
const TIER_INFO = {
  nemesis: { cls: 'bad' }, hostile: { cls: 'bad' }, rival: { cls: 'bad' },
  stranger: { cls: '' }, acquaintance: { cls: '' },
  friendly: { cls: 'good' }, trusted: { cls: 'good' }, bonded: { cls: 'good' },
};
function tierInfo(t) {
  const key = String(t || 'stranger').toLowerCase();
  return { label: key, cls: (TIER_INFO[key] || {}).cls || '' };
}
// EquipSlot wire ids → friendly slot labels.
const EQUIP_SLOT_LABELS = {
  head: 'Head', chest: 'Chest', main_hand: 'Main Hand',
  off_hand: 'Off Hand', legs: 'Legs', feet: 'Feet',
};
// Behavior-tag chips (consumable/equippable/pocketable) for inventory items.
function tagChips(tags) {
  if (!Array.isArray(tags) || !tags.length) return '';
  return `<span class="fable-drop-chips">${tags
    .map((t) => `<span class="fable-drop-chip">${esc(String(t))}</span>`).join('')}</span>`;
}
// A belt/pack entry: name + optional ×qty + tag chips.
function stackItemRow(it) {
  const name = nonEmpty(it.name) ? String(it.name) : 'Unknown';
  const qty = (typeof it.qty === 'number' && it.qty > 1) ? `<span class="fable-drop-qty">×${it.qty}</span>` : '';
  return `<div class="fable-drop-list-item">${esc(name)}${qty}${tagChips(it.tags)}</div>`;
}
function stackList(items) {
  return `<div class="fable-drop-list">${items.map(stackItemRow).join('')}</div>`;
}
function repLabel(rep) {
  if (rep > 5) return `Renowned (${rep})`;
  if (rep > 0) return `Liked (${rep})`;
  if (rep < -5) return `Infamous (${rep})`;
  if (rep < 0) return `Distrusted (${rep})`;
  return 'Neutral (0)';
}
// Format epoch-minutes as the narrator's "Day N, HH:MM" clock line. Mirrors
// Rust's WorldClock::render_clock_line exactly (schema.rs): 1 day = 1440 min,
// day index = minutes/1440 + 1, time-of-day = minutes % 1440.
function clockLabel(minutes) {
  const m = Number(minutes) || 0;
  const day = Math.floor(m / 1440) + 1;
  const rem = m % 1440;
  const h = Math.floor(rem / 60);
  const min = rem % 60;
  return `Day ${day}, ${String(h).padStart(2, '0')}:${String(min).padStart(2, '0')}`;
}
// ─ read-only block builders (reused by Card / World / Codex / NPC / Player) ─
// `cls` (optional) adds a severity class to the value (bad/warn/good).
function row(label, val, cls) {
  return `<div class="fable-drop-row"><span>${esc(label)}</span><span class="fable-drop-val ${cls || ''}">${esc(val)}</span></div>`;
}
// A section header + arbitrary inner HTML.
function section(label, inner) {
  return `<div class="fable-drop-section">${esc(label)}</div>${inner}`;
}
function proseBlock(label, text) {
  return `<div class="fable-drop-section">${esc(label)}</div><div class="fable-drop-prose">${esc(text)}</div>`;
}
function listBlock(label, items) {
  return `<div class="fable-drop-section">${esc(label)}</div><div class="fable-drop-list">${
    items.map((i) => `<div class="fable-drop-list-item">${esc(i)}</div>`).join('')
  }</div>`;
}
function emptyBox(msg) {
  return `<div class="fable-drop-empty">${esc(msg)}</div>`;
}
