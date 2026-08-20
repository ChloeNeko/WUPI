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
                aria-selected="false" aria-label="${t.label}">
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
  // Header (label · centered NAME · ✎). Built once; the body is filled by
  // the tab renderer. (2026-08-20) The center slot shows the character's
  // NAME for the Player / Card tabs (bold white, set by the renderer once
  // its IPC resolves); other tabs leave it empty.
  el.innerHTML = `
    <div class="fable-tab-drop__head">
      <span class="fable-tab-drop__title" data-drop-title>${esc(meta.label)}</span>
      <span class="fable-tab-drop__name" data-drop-name></span>
      <button class="fable-tab-drop__edit" data-raw-edit="${meta.file.kind}"
              aria-label="Edit raw file">✎</button>
    </div>
    <div class="fable-tab-drop__body" data-drop-body></div>
  `;
  el.querySelector('[data-raw-edit]').addEventListener('click', () => {
    // onSaved: re-render this dropdown so the read-only view reflects the
    // just-saved raw edit immediately (no stale display).
    openRawEditor(meta.file.kind, renderActive);
  });
  const body = el.querySelector('[data-drop-body]');
  // Renderer hooks into the head: `setTitle` (a fallback label when the tab
  // has NO character name — the World/Codex/NPC tabs keep theirs) and
  // `setName` (the center bold-white name). (2026-08-20) When a NAME is set
  // the head carries it ALONE — the "Player" / "NPC Card" label hides (the
  // name IS the header); without a name the label stands as the fallback.
  const head = {
    setTitle(t) {
      const node = el.querySelector('[data-drop-title]');
      if (node && nonEmpty(t)) node.textContent = String(t);
    },
    setName(n) {
      const node = el.querySelector('[data-drop-name]');
      const headEl = el.querySelector('.fable-tab-drop__head');
      const has = nonEmpty(n);
      if (node) node.textContent = has ? String(n) : '';
      if (headEl) headEl.classList.toggle('has-name', has);
    },
  };
  try {
    if (key === 'player') await renderPlayer(body, head);
    else if (key === 'card') await renderCard(body, head);
    else if (key === 'codex') await renderCodex(body);
    else if (key === 'world') await renderWorld(body);
    else if (key === 'npc') await renderNpc(body);
  } catch (err) {
    body.innerHTML = `<div class="fable-tab-drop__error">Couldn't load: ${esc(err)}</div>`;
  }
}

// ── Player tab (character sheet, READ-ONLY except the World-tab style
// date/time rows — none here) ────────────────────────────────────────────
// Pulls from THREE sources + renders each section only when it has content:
//   • fable_active_player_get → the attached SavedPlayer CARD: full identity
//     trait set, opt-in persona, custom tags. None for a playerless game.
//   • player_state_get        → appearance deltas, vitals, injuries, inventory.
//   • fable_schema_get        → relationships (keyed by npc id). Best-effort.
// (2026-08-20 rebuild, Chloe) The tab MIRRORS the SIM card tab: the header
// carries the NAME alone (the old in-body name row was the duplicate),
// VITALS lead (live state above identity), then Identity → Persona →
// Appearance → Injuries → Effects → custom tags (NO divider, directly above
// inventory) → Inventory → Relationships. The slot-row "Equipped" section
// is GONE — worn/held items render as stacked lists ("Clothing / Weapons /
// Belt / Pack") like the card tab.
async function renderPlayer(bodyEl, head) {
  let ps;
  try {
    ps = await invoke('player_state_get');
  } catch (err) {
    bodyEl.innerHTML = emptyBox('No active game.');
    return;
  }
  const [player, schema] = await Promise.all([
    invoke('fable_active_player_get').then((p) => p || null).catch(() => null),
    invoke('fable_schema_get').catch(() => null),
  ]);

  // The header names the character being played — NAME ONLY (the tab label
  // hides while a name is set).
  head.setName(player && player.name);

  const parts = [];
  parts.push(renderVitals(ps));
  const identity = renderIdentity(player);
  if (identity) parts.push(identity);
  const persona = renderPlayerPersona(player);
  if (persona) parts.push(persona);
  const appearance = renderAppearance(ps);
  if (appearance) parts.push(appearance);
  const injuries = renderInjuries(ps);
  if (injuries) parts.push(injuries);
  const effects = renderEffects(schema);
  if (effects) parts.push(effects);
  const tags = renderCustomTags(player && player.custom_tags);
  if (tags) parts.push(tags);
  const inventory = renderInventory(ps);
  if (inventory) parts.push(inventory);
  const relationships = renderRelationships(schema);
  if (relationships) parts.push(relationships);
  bodyEl.innerHTML = parts.join('');
}

// Identity: the attached player CARD's full stable trait set — the same
// field family the SIM tab renders for a card (plus the player-only
// conditional + flavor lines). NO name row: the header owns the name (the
// old in-body name was the duplicate Chloe flagged). Omitted entirely when
// there's no attached player.
function renderIdentity(player) {
  if (!player) return '';
  const traitRows = [
    ['Gender', player.gender], ['Race', player.race], ['Age', player.age],
    ['Height', player.height], ['Weight', player.weight],
    ['Body', player.body_type], ['Skin', player.skin_complexion],
    ['Eyes', player.eye_color],
    ['Hair Color', player.hair_color], ['Hair Length', player.hair_length],
    ['Hair Style', player.hair_style],
    ['Breast Size', player.breast_size], ['Ears', player.ears],
    ['Tail', player.tail], ['Horn', player.horn],
    ['Weakness', player.weakness],
    ['Distinguishing Marks', player.distinguishing_marks],
  ].filter(([, v]) => nonEmpty(v));
  if (!traitRows.length) return '';
  return section('Identity', traitRows.map(([k, v]) => row(k, v)).join(''));
}

// Persona: the player card's OPT-IN persona block (players never carry a
// conversation style) + the top-level backstory. STACKED (long-prose
// fields), mirroring the SIM tab's Persona section. Hidden when empty.
function renderPlayerPersona(player) {
  if (!player) return '';
  const p = (player.persona && typeof player.persona === 'object') ? player.persona : {};
  const rows = [
    ['Personality', p.personality != null ? p.personality : player.personality],
    ['Likes', p.likes != null ? p.likes : player.likes],
    ['Dislikes', p.dislikes != null ? p.dislikes : player.dislikes],
    ['Flaws', p.flaws != null ? p.flaws : player.flaws],
    ['Goals', p.goals != null ? p.goals : player.goal],
    ['Occupation', p.occupation != null ? p.occupation : null],
    ['Backstory', player.backstory],
  ].filter(([, v]) => nonEmpty(v)).map(([l, v]) => stackRow(l, v)).join('');
  return rows ? section('Persona', rows) : '';
}

// Appearance: the live `current_appearance_deltas` (hair/body/skin/eyes/
// scars/wounds/etc.) — what the character's BODY currently looks like in
// this game. Clothing is NOT here (2026-08-18): garments are typed
// inventory, rendered by the Inventory section's Clothing list. Hidden
// entirely when no deltas are tracked.
function renderAppearance(ps) {
  const deltas = ps.current_appearance_deltas || {};
  const rows = Object.entries(deltas)
    .filter(([, v]) => nonEmpty(v))
    .map(([k, v]) => row(prettify(k), v));
  return rows.length ? section('Appearance', rows.join('')) : '';
}

// Vitals: always present. (2026-08-20) Health sits ABOVE stamina — the
// derived overall tier from the backend (`ps.health`, injected by
// player_state_get). Gold + Reputation are GONE: reputation is per-faction
// world-tracker state, and currency is inventory (belt/pack items).
function renderVitals(ps) {
  const hp = healthInfo(ps);
  const stam = staminaInfo(ps.stamina);
  return section('Vitals',
    row('Health', hp.label, hp.cls)
    + row('Stamina', stam.label, stam.cls));
}

// Active status tags (schema.status_tags): every LIVE effect with its
// polarity. (2026-08-20) Expired tags are filtered read-time against the
// same payload's world clock — the backend's expiry sweep only runs on the
// world progression tick (suspended in Combat), so a just-expired
// "Feverish" must not sit in this list while the derived Health line has
// already dropped Sick. `expires_at: 0` is the permanent sentinel (never
// expires); when the payload carries no clock the filter degrades to
// show-all. Hidden when no live tags remain.
function renderEffects(schema) {
  const now = schema && schema.world_clock && Number.isFinite(schema.world_clock.current_minutes)
    ? schema.world_clock.current_minutes
    : null;
  const tags = (schema && Array.isArray(schema.status_tags) ? schema.status_tags : [])
    .filter((t) => t && nonEmpty(t.label))
    .filter((t) => now === null || !t.expires_at || now < t.expires_at);
  if (!tags.length) return '';
  const rows = tags.map((t) =>
    row(t.label, t.polarity === 'buff' ? 'Buff' : 'Debuff', t.polarity === 'buff' ? 'good' : 'bad'));
  return section('Active Effects', rows.join(''));
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

// Inventory: stacked lists, NO slot rows (2026-08-20, Chloe — "completely
// removed equipped, it should just show clothing and stuff"). Clothing =
// everything WORN live (all equipment slots except the readied hands,
// outer + under layers folded together); Weapons = the main/off hands;
// Belt + Pack = the live stacks with quantities. Each subsection hidden
// when empty.
function renderInventory(ps) {
  const eq = ps.equipment || {};
  const worn = [];
  const weapons = [];
  for (const [slot, layers] of Object.entries(eq)) {
    const bucket = (slot === 'main_hand' || slot === 'off_hand') ? weapons : worn;
    for (const layerKey of ['outer', 'inner']) {
      const it = layers && layers[layerKey];
      if (it && nonEmpty(it.name) && !bucket.includes(it.name)) bucket.push(it.name);
    }
  }
  const belt = Array.isArray(ps.belt) ? ps.belt : [];
  const pack = Array.isArray(ps.pack) ? ps.pack : [];
  const rows = [];
  if (worn.length) rows.push(stackRow('Clothing', worn.join(', ')));
  if (weapons.length) rows.push(stackRow('Weapons', weapons.join(', ')));
  if (belt.length) rows.push(stackRow('Belt', stackNames(belt)));
  if (pack.length) rows.push(stackRow('Pack', stackNames(pack)));
  return rows.length ? section('Inventory', rows.join('')) : '';
}

// A belt/pack stack rendered as one comma-joined line: name (+ ×qty).
function stackNames(items) {
  return items.map((it) => {
    const name = nonEmpty(it.name) ? String(it.name) : 'Unknown';
    const qty = (typeof it.qty === 'number' && it.qty > 1) ? ` ×${it.qty}` : '';
    return `${name}${qty}`;
  }).join(', ');
}

// Custom tags (shared by the Player + SIM card tabs): the authored
// {key:value} map. (2026-08-20, Chloe) NO section divider — the rows flow
// directly ABOVE Inventory — and KEYS render prettified (snake_case →
// spaced, capitalized words) instead of showing raw underscores.
function renderCustomTags(tags) {
  if (!tags || typeof tags !== 'object') return '';
  return Object.entries(tags)
    .filter(([, v]) => nonEmpty(v))
    .map(([k, v]) => stackRow(prettify(k), v))
    .join('');
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
// (2026-08-20 rework) The dropdown header IS the card: the NAME alone in the
// center (bold white). The subtype label only stands as a fallback when the
// card carries no name — the "Player" / "NPC Card" style headers are gone
// (Chloe: "just have it show NAME only"). "Playing as" is deleted — the
// bound player is the PLAYER tab's business, never the card's. The <world>
// anchor seeds (date/time/weather/tone/location) no longer render here:
// they're consumed into the live World tab on start, so the card section
// carries persona only.
async function renderCard(body, head) {
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
  if (!nonEmpty(card.name)) head.setTitle(cardTabTitle(card.subtype));
  head.setName(card.name);
  const parts = [];
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
  // STACKED (label above value): these are long-prose fields — a side-by-side
  // row put a paragraph hard against its label.
  const p = card.persona || {};
  const personaRows = [
    ['Personality', p.personality], ['Conversation Style', p.conversation_style],
    ['Likes', p.likes], ['Dislikes', p.dislikes], ['Flaws', p.flaws],
    ['Goals', p.goals], ['Occupation', p.occupation], ['Backstory', p.backstory],
  ].filter(([, v]) => nonEmpty(v)).map(([l, v]) => stackRow(l, v)).join('');
  if (personaRows) parts.push(section('Persona', personaRows));
  if (nonEmpty(card.setting)) parts.push(proseBlock('Setting', card.setting));
  if (nonEmpty(card.plot)) parts.push(proseBlock('Plot', card.plot));
  // Custom tags FIRST (2026-08-20, Chloe): NO "Custom Tags" divider — the
  // prettified rows flow directly ABOVE the Inventory section.
  const tags = renderCustomTags(card.custom_tags);
  if (tags) parts.push(tags);
  // The <inventory> sibling (npc cards). STACKED — the clothing list runs long.
  const inv = card.inventory || {};
  const invRows = [
    ['Clothing', inv.clothing], ['Equipped', inv.equipped],
    ['Accessories', inv.accessories], ['Stored', inv.stored],
  ].filter(([, v]) => Array.isArray(v) && v.length).map(([l, v]) => stackRow(l, v.join(', '))).join('');
  if (invRows) parts.push(section('Inventory', invRows));
  body.innerHTML = parts.length ? parts.join('') : emptyBox('No active card.');
}

// Subtype → the card tab's header label. Mirrors the wizard discriminator
// ("npc" | "scenario" | "world"); unknown/absent falls back to plain "Card".
function cardTabTitle(subtype) {
  const s = String(subtype || '').toLowerCase();
  if (s === 'npc') return 'NPC Card';
  if (s === 'world') return 'World Card';
  if (s === 'scenario') return 'Scenario Card';
  return 'Card';
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
// WUPI; there is no inline form — EXCEPT the Date + Time rows (2026-08-20,
// Chloe): those two are click-to-edit inline (the calendar label + the
// time-of-day were previously impossible to change from the drawer).
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
  // (2026-08-20) DATE first (the authored/[DATE]-rewritten calendar label —
  // seeded from the card's <world> sibling on a fresh start), then the clock,
  // then TONE (world state since 2026-08-19 — it lives HERE, not on the card
  // section, which no longer renders the anchor seeds at all). Date + Time
  // are the two INLINE-EDITABLE rows: click the value to edit (Enter saves,
  // Esc cancels). Time renders as a 12-hour AM/PM readout — NEVER "Day N"
  // (the date label carries the day).
  const calLabel = nonEmpty(schema.calendar) ? String(schema.calendar).trim() : '';
  parts.push(rowEditable('Date', calLabel || '—'));
  parts.push(rowEditable('Time', mins > 0 ? clockLabel(mins) : '—'));
  if (weather.trim()) parts.push(row('Weather', weather.trim()));
  if (nonEmpty(schema.tone)) parts.push(row('Tone', String(schema.tone).trim()));
  if (node.trim()) parts.push(row('Location', prettify(node)));
  if (nonEmpty(schema.summary)) parts.push(proseBlock('Summary', schema.summary));
  if (rumors.length) parts.push(listBlock('Rumors', rumors));
  if (events.length) parts.push(listBlock('Recent events', events.slice(-5)));
  if (worldEnts.length) parts.push(listBlock('Tracked details', worldEnts.map(([k, v]) => `${prettify(k)}: ${v}`)));
  body.innerHTML = parts.length ? parts.join('') : '<div class="fable-drop-empty">World state not yet established.</div>';
  wireWorldEdits(body, schema);
}

// ── World-tab inline date/time editing ───────────────────────────────────
// The sanctioned write path is `fable_json_raw_set` (kind=world) — the same
// recompose-into-live-schema gate the ✎ raw editor uses. The patch is built
// from the LIVE schema snapshot (never a stale disk read) and touches ONLY
// the edited keys; every other world-slice key passes through untouched.

// An editable row: same stacked centered layout as `row()`, but the value
// swaps to a text input on click.
function rowEditable(label, val) {
  return `<div class="fable-drop-row fable-drop-row--edit" data-edit-row="${esc(label)}"><span>${esc(label)}</span><span class="fable-drop-val fable-drop-val--edit" data-edit-val>${esc(val)}</span></div>`;
}

function wireWorldEdits(bodyEl, schema) {
  const clock = schema.world_clock || {};
  const base = Number(clock.current_minutes) || 0;
  const handlers = {
    Date: (v) => {
      const label = String(v || '').replace(/\s+/g, ' ').trim().slice(0, 80);
      if (!label) return;
      // Re-stamp the sync marker so the label isn't "stale" against the clock.
      saveWorldSlice({ calendar: label, calendar_synced_minutes: base });
    },
    Time: (v) => {
      const tod = parseTimeInput(v);
      if (tod === null) return;
      // Keep the epoch DAY; replace the time-of-day. minute 0 is the dormant
      // sentinel, so a midnight set on day 0 nudges to 00:01 (is_set() gate).
      let next = Math.floor(base / 1440) * 1440 + tod;
      if (next <= 0) next = 1;
      const lastTick = Math.min(Number(clock.last_tick_minutes) || 0, next);
      saveWorldSlice({ world_clock: { current_minutes: next, last_tick_minutes: lastTick } });
    },
  };
  for (const valEl of bodyEl.querySelectorAll('[data-edit-val]')) {
    const rowEl = valEl.closest('[data-edit-row]');
    const commit = rowEl && handlers[rowEl.dataset.editRow];
    if (commit) valEl.addEventListener('click', () => beginInlineEdit(valEl, commit));
  }
}

// Swap a value span for a focused text input. Enter commits (calls back with
// the raw text), Esc/blur cancels. stopPropagation keeps Enter/Esc off the
// stage-level key handlers while the input is live.
function beginInlineEdit(valEl, commit) {
  if (valEl.parentNode.querySelector('.fable-drop-edit-input')) return;
  const input = document.createElement('input');
  input.type = 'text';
  input.className = 'fable-drop-edit-input';
  input.value = valEl.textContent === '—' ? '' : valEl.textContent;
  valEl.hidden = true;
  valEl.parentNode.appendChild(input);
  input.focus();
  input.select();
  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    const v = input.value;
    input.remove();
    valEl.hidden = false;
    if (save) commit(v);
  };
  input.addEventListener('keydown', (e) => {
    e.stopPropagation();
    if (e.key === 'Enter') { e.preventDefault(); finish(true); }
    else if (e.key === 'Escape') { e.preventDefault(); finish(false); }
  });
  input.addEventListener('blur', () => finish(false));
}

// "10:30", "10:30 AM", "10:30pm", "22:05", "9am" → minute-of-day. Null when
// unparseable (the edit silently no-ops — the row keeps its current value).
function parseTimeInput(v) {
  const m = String(v || '').trim().match(/^(\d{1,2})(?::(\d{2}))?\s*(am|pm)?$/i);
  if (!m) return null;
  let h = parseInt(m[1], 10);
  const min = m[2] ? parseInt(m[2], 10) : 0;
  if (min > 59) return null;
  const mer = m[3] ? m[3].toLowerCase() : null;
  if (mer) {
    if (h < 1 || h > 12) return null;
    h = (h % 12) + (mer === 'pm' ? 12 : 0);
  } else if (h > 23) {
    return null;
  }
  return h * 60 + min;
}

async function saveWorldSlice(patch) {
  try {
    await invoke('fable_json_raw_set', { kind: 'world', json: JSON.stringify(patch) });
  } catch (err) {
    console.warn('[tab-rail] world slice save failed', err);
    return;
  }
  renderActive();
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
// Slug → display text (2026-08-20). Ids arrive dash/underscore-slugged, and
// names with an apostrophe minted as "-s-" ("Liam-s-House"): the possessive
// is restored FIRST, then remaining separators become spaces, then words
// capitalize at string start / after a space ONLY (never after the
// apostrophe — "Liam's" must not become "Liam'S").
function prettySlug(k) {
  const spaced = String(k)
    .replace(/-s-(?=\S)/gi, "'s ")
    .replace(/[-_]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
  return spaced.replace(/(^|\s)([a-z])/g, (m, a, b) => a + b.toUpperCase());
}
function prettify(k) {
  return prettySlug(k);
}
function prettifyNpcKey(k) {
  return prettySlug(String(k).replace(/^npc\./, ''));
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
// Overall Health tier wire → { label, cls }. The backend injects the derived
// `health` string on player_state_get (Rust `consequence::derive_health_tier`
// over wounds + active illness tags); when absent (stale payload) the
// client-side points-system mirror below computes the same grade (no illness
// detection in the fallback — wounds only). (2026-08-20) Deceased — a Black
// core part (Head/Neck/UpperTorso) — is death, above every illness label.
const HEALTH_LABELS = {
  deceased:    { label: 'Deceased',  cls: 'hp-deceased' },
  sick:      { label: 'Sick',      cls: 'hp-sick' },
  infected:  { label: 'Infected',  cls: 'hp-sick' },
  excellent: { label: 'Excellent', cls: 'hp-excellent' },
  good:      { label: 'Good',      cls: 'hp-good' },
  fair:      { label: 'Fair',      cls: 'hp-fair' },
  poor:      { label: 'Poor',      cls: 'hp-poor' },
  critical:  { label: 'Critical',  cls: 'hp-critical' },
};
function healthInfo(ps) {
  const raw = (ps && nonEmpty(ps.health)) ? ps.health : derivedHealth(ps && ps.body);
  const key = String(raw || '').toLowerCase();
  return HEALTH_LABELS[key] || { label: pascalToWords(raw) || 'Excellent', cls: 'hp-excellent' };
}
// Client-side mirror of the backend's 2026-08-20 points system (the stale-
// payload fallback only): core floor from Head/Neck/UpperTorso worst color
// (yellow→Good, orange→Fair, red→Poor, purple→Critical, black core =
// Deceased — checked FIRST, death outranks everything), non-core points
// Yellow=1/Orange=2/Red=4/Purple=8 (Black = 0), banded 0-7/8-11/12-17/
// 18-23/24+ → Excellent/Good/Fair/Poor/Critical; the worse of the two
// halves wins. Body keys are the PascalCase serde wire names.
function derivedHealth(bodyMap) {
  const pts = { yellow: 1, orange: 2, red: 4, purple: 8 };
  const coreFloor = { yellow: 'Good', orange: 'Fair', red: 'Poor', purple: 'Critical' };
  const order = ['Critical', 'Poor', 'Fair', 'Good', 'Excellent'];
  const worse = (a, b) => (order.indexOf(a) <= order.indexOf(b) ? a : b);
  let floor = 'Excellent';
  let points = 0;
  for (const [part, v] of Object.entries(bodyMap || {})) {
    const key = String(v || '').toLowerCase();
    if (part === 'Head' || part === 'Neck' || part === 'UpperTorso') {
      if (key === 'black') return 'Deceased';
      if (coreFloor[key]) floor = worse(floor, coreFloor[key]);
    } else {
      points += pts[key] || 0;
    }
  }
  const band = points >= 24 ? 'Critical' : points >= 18 ? 'Poor'
    : points >= 12 ? 'Fair' : points >= 8 ? 'Good' : 'Excellent';
  return worse(floor, band);
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
// (2026-08-20) EQUIP_SLOT_LABELS + the belt/pack chip/list builders are
// RETIRED with the slot-row "Equipped" section — inventory renders as
// stacked name lists now (renderInventory / stackNames).
// Format epoch-minutes as a 12-hour AM/PM readout of the minute-of-day.
// (2026-08-20, Chloe) NEVER "Day N" — the calendar DATE carries the day; a
// day counter next to a real date label is noise. Mirrors the left drawer's
// formatTime12h.
function clockLabel(minutes) {
  const m = Number(minutes) || 0;
  const tod = ((m % 1440) + 1440) % 1440;
  let h = Math.floor(tod / 60);
  const min = tod % 60;
  const mer = h >= 12 ? 'PM' : 'AM';
  h = h % 12;
  if (h === 0) h = 12;
  return `${h}:${String(min).padStart(2, '0')} ${mer}`;
}
// ─ read-only block builders (reused by Card / World / Codex / NPC / Player) ─
// `cls` (optional) adds a severity class to the value (bad/warn/good/hp-*).
function row(label, val, cls) {
  return `<div class="fable-drop-row"><span>${esc(label)}</span><span class="fable-drop-val ${cls || ''}">${esc(val)}</span></div>`;
}
// STACKED row (label ABOVE value) for long-prose fields — persona lines,
// inventory lists, custom tags — where a side-by-side row put a paragraph
// hard against its label.
function stackRow(label, val) {
  return `<div class="fable-drop-stack"><span class="fable-drop-stack-label">${esc(label)}</span><span class="fable-drop-stack-val">${esc(val)}</span></div>`;
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
