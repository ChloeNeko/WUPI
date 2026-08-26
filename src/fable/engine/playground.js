// =============================================================
// THE PLAYGROUND (2026-08-23) — the Sand Table turned player-facing UI.
//
// A god-mode control surface in the RIGHT Wupi drawer, revealed by the WAND
// icon in the brand header. When on, the `[ PLAYGROUND ACTIVE ]` strip
// slides down OVER the tab-rail zone (the rail itself is inert + aria-hidden
// — never destroyed — and its open dropdown collapses non-destructively so
// the covered tab survives wand-off intact). Below the strip, the domain
// panel (Player / NPC / World) carries the controls; every control hits a
// `playground_*` IPC and renders its result as a small inline report line;
// backend guard errors surface VERBATIM.
//
// State discipline: module state `{ wandOn, domain }`; `resetPlayground()`
// (drawer close + stage teardown) collapses the strip + unpresses the wand
// AND clears the domain — opening the Playground NEVER pre-selects one
// (2026-08-25); the panel appears only after a domain button is clicked.
// Backend god flags are NEVER touched by any UI collapse —
// `playground_flags_set` is the only writer.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { ICONS, collapseTabDropdown, renderActive } from './tab-rail.js';

// The three domains, strip order. The icons reuse the tab-rail vocabulary.
const DOMAINS = [
  { key: 'player', label: 'Player' },
  { key: 'npc', label: 'NPC' },
  { key: 'world', label: 'World' },
];

// The relationship ladder, worst → best (Rust enum order). Slider 0-7.
const TIER_LADDER = ['nemesis', 'hostile', 'rival', 'stranger', 'acquaintance', 'friendly', 'trusted', 'bonded'];

// AssetKind words the spawner accepts (the backend's set; building refuses
// on hosted room maps — the depth-2 law).
const ASSET_KINDS = ['creature', 'group', 'object', 'trap', 'hazard', 'loot', 'building'];
const SPAWN_TIERS = ['', 'minion', 'soldier', 'elite', 'boss', 'legendary'];
const THREATS = ['low', 'moderate', 'high', 'deadly'];
const ITEM_TAGS = ['consumable', 'equippable', 'pouchable'];

let wrapEl = null;       // the .fable-playground-wrap root
let stripEl = null;      // the covering strip
let panelEl = null;      // the domain content area
let domainBtns = {};     // key → strip domain button
let wandBtn = null;      // the header wand button (stage.js hands it over)
let railWrapEl = null;   // the tab-rail wrap that gets covered
let wandOn = false;
let domain = null;     // NO domain auto-selected on open (2026-08-25) — the
                       // panel stays collapsed until a domain button is clicked
let spawnPrefill = null; // one-shot AMBUSH handoff → the World spawner
let ticking = false;     // FORCE WORLD TICK busy state (the ~1-3s local pass)

// Build the Playground DOM (once per stage; stage.js mounts it inside
// [data-tab-rail-mount] AFTER the rail wrap). Returns the wrap element.
export function buildPlayground() {
  wrapEl = document.createElement('div');
  wrapEl.className = 'fable-playground-wrap';

  stripEl = document.createElement('div');
  stripEl.className = 'fable-playground-strip';
  const label = document.createElement('span');
  label.className = 'fable-playground-strip__label';
  label.textContent = 'PLAYGROUND ACTIVE';
  // The fade divider under the label — same gradient shape as the rail's
  // own divider (transparent → line → transparent), kept as an element so
  // it spans the strip width rather than the label's shrink-to-fit box.
  const divider = document.createElement('div');
  divider.className = 'fable-playground-strip__divider';
  const domains = document.createElement('div');
  domains.className = 'fable-playground-strip__domains';
  domainBtns = {};
  for (const d of DOMAINS) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'fable-playground-strip__btn';
    btn.dataset.pgDomain = d.key;
    btn.setAttribute('aria-label', d.label);
    btn.innerHTML = ICONS[d.key] || '';
    btn.addEventListener('click', () => switchDomain(d.key));
    domainBtns[d.key] = btn;
    domains.appendChild(btn);
  }
  stripEl.appendChild(label);
  stripEl.appendChild(divider);
  stripEl.appendChild(domains);

  panelEl = document.createElement('div');
  panelEl.className = 'fable-playground-panel';
  panelEl.hidden = true;

  wrapEl.appendChild(stripEl);
  wrapEl.appendChild(panelEl);
  return wrapEl;
}

// stage.js wires the wand button + the rail wrap it covers. Idempotent
// binding follows the wupi-drawer pattern (same elements → skip re-bind).
let boundWand = null;
export function initPlayground({ wandBtn: wand, railWrap } = {}) {
  if (wand && wand !== boundWand) {
    if (boundWand) boundWand.removeEventListener('click', onWandClick);
    wand.addEventListener('click', onWandClick);
    boundWand = wand;
  }
  wandBtn = wand || wandBtn;
  railWrapEl = railWrap || railWrapEl;
}

function onWandClick() {
  togglePlayground();
}

export function togglePlayground() {
  setWand(!wandOn);
  return wandOn;
}

export function isWandOn() {
  return wandOn;
}

function setWand(on) {
  wandOn = on;
  // Turning the wand off (wand re-click, off-screen click, teardown)
  // DESELECTS the active domain button — no silent pre-selection survives
  // a toggle cycle. The drawer's pull-in does NOT pass through here; the
  // Playground stays live behind the closed drawer (2026-08-25).
  if (!on) clearDomainSelection();
  if (stripEl) stripEl.classList.toggle('is-open', on);
  if (wandBtn) {
    wandBtn.setAttribute('aria-pressed', on ? 'true' : 'false');
    wandBtn.classList.toggle('is-active', on);
  }
  if (railWrapEl) {
    if (on) {
      // Cover: the rail stays in the DOM, untouched — inert + hidden from
      // a11y, its open dropdown collapsed NON-destructively (activeTab
      // survives; wand-off re-renders it).
      railWrapEl.setAttribute('inert', '');
      railWrapEl.setAttribute('aria-hidden', 'true');
      collapseTabDropdown();
    } else {
      railWrapEl.removeAttribute('inert');
      railWrapEl.removeAttribute('aria-hidden');
      renderActive();
    }
  }
  if (panelEl) {
    // Open shows the strip alone — no domain pre-selected, panel collapsed
    // until a domain button (or the AMBUSH handoff) picks one.
    panelEl.hidden = !on || domain == null;
    if (!on || domain == null) panelEl.innerHTML = '';
    else switchDomain(domain);
  }
}

// Unpress every domain button + forget the selection.
function clearDomainSelection() {
  domain = null;
  for (const btn of Object.values(domainBtns)) {
    btn.classList.remove('is-active');
    btn.setAttribute('aria-pressed', 'false');
  }
}

// Collapse for stage teardown ONLY (2026-08-25): strip in, wand unpressed,
// domain deselected. The right drawer's pull-in no longer calls this — the
// Playground stays active behind the closed drawer. Backend god flags are
// untouched by any UI collapse — `playground_flags_set` is the only writer.
export function resetPlayground() {
  setWand(false);
}

function switchDomain(key) {
  domain = key;
  for (const [k, btn] of Object.entries(domainBtns)) {
    const on = k === key;
    btn.classList.toggle('is-active', on);
    btn.setAttribute('aria-pressed', on ? 'true' : 'false');
  }
  if (!panelEl) return;
  panelEl.hidden = false;
  panelEl.innerHTML = '';
  if (key === 'player') void renderPlayerPanel(panelEl);
  else if (key === 'npc') void renderNpcPanel(panelEl);
  else if (key === 'world') void renderWorldPanel(panelEl);
}

// ── tiny DOM builders ────────────────────────────────────────────────────
// Everything is createElement + textContent (no innerHTML for data — the
// esc-free path; report lines render backend text verbatim and safely).
function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text != null) node.textContent = String(text);
  return node;
}
function section(parent, title) {
  const box = el('div', 'fable-playground-section');
  box.appendChild(el('div', 'fable-playground-section__title', title));
  parent.appendChild(box);
  return box;
}
function row(parent) {
  const r = el('div', 'fable-playground-row');
  parent.appendChild(r);
  return r;
}
function button(parent, label, act, data) {
  const btn = el('button', 'fable-playground-btn', label);
  btn.type = 'button';
  btn.dataset.pgAct = act;
  if (data) btn.dataset.pgData = data;
  parent.appendChild(btn);
  return btn;
}
function select(parent, options, value) {
  const sel = document.createElement('select');
  sel.className = 'fable-playground-select';
  for (const opt of options) {
    const o = document.createElement('option');
    const val = typeof opt === 'string' ? opt : opt.value;
    const lbl = typeof opt === 'string' ? opt : opt.label;
    o.value = val;
    o.textContent = lbl;
    sel.appendChild(o);
  }
  if (value != null) sel.value = value;
  parent.appendChild(sel);
  return sel;
}
function input(parent, value, placeholder) {
  const inp = document.createElement('input');
  inp.type = 'text';
  inp.className = 'fable-playground-input';
  if (value != null) inp.value = value;
  if (placeholder) inp.placeholder = placeholder;
  parent.appendChild(inp);
  return inp;
}
function numberInput(parent, value) {
  const inp = document.createElement('input');
  inp.type = 'number';
  inp.className = 'fable-playground-input fable-playground-input--num';
  if (value != null) inp.value = String(value);
  parent.appendChild(inp);
  return inp;
}
// One inline report line under a control group. `kind` colors the line
// (ok | error | info); replaces the previous line in the same slot.
function report(parent, text, kind) {
  let slot = parent.querySelector(':scope > .fable-playground-report');
  if (!slot) {
    slot = el('div', 'fable-playground-report');
    parent.appendChild(slot);
  }
  slot.className = 'fable-playground-report' + (kind ? ' is-' + kind : '');
  slot.textContent = String(text);
  return slot;
}
// Run one IPC + render its report. `fmt` maps the resolved payload to a
// line; a rejection renders the backend error VERBATIM (the plan's rule).
async function run(parent, cmd, args, fmt) {
  try {
    const out = await invoke(cmd, args);
    report(parent, fmt ? fmt(out) : 'Done.', 'ok');
    return out;
  } catch (err) {
    report(parent, String(err), 'error');
    return null;
  }
}

// ── PLAYER — The Avatar & Referees ───────────────────────────────────────
async function renderPlayerPanel(panel) {
  let data;
  try {
    data = await invoke('playground_player_get');
  } catch (err) {
    panel.appendChild(el('div', 'fable-playground-error', String(err)));
    return;
  }

  // God Mode & Sculpting.
  const god = section(panel, 'God Mode & Sculpting');
  const flags = data.flags || {};
  const chipRow = row(god);
  const autoChip = button(chipRow, 'AUTO-PASS CHECKS', 'flag-auto-pass');
  autoChip.classList.toggle('is-on', !!flags.auto_pass);
  autoChip.setAttribute('aria-pressed', flags.auto_pass ? 'true' : 'false');
  autoChip.addEventListener('click', async () => {
    const next = !(autoChip.classList.contains('is-on'));
    await run(god, 'playground_flags_set', { autoPass: next },
      (o) => `AUTO-PASS ${o.auto_pass ? 'ON' : 'OFF'}`);
    autoChip.classList.toggle('is-on', next);
    autoChip.setAttribute('aria-pressed', next ? 'true' : 'false');
  });
  const freezeChip = button(chipRow, 'FREEZE CLAMPS', 'flag-freeze');
  freezeChip.classList.toggle('is-on', !!flags.freeze_clamps);
  freezeChip.setAttribute('aria-pressed', flags.freeze_clamps ? 'true' : 'false');
  freezeChip.addEventListener('click', async () => {
    const next = !(freezeChip.classList.contains('is-on'));
    await run(god, 'playground_flags_set', { freezeClamps: next },
      (o) => `FREEZE CLAMPS ${o.freeze_clamps ? 'ON' : 'OFF'}`);
    freezeChip.classList.toggle('is-on', next);
    freezeChip.setAttribute('aria-pressed', next ? 'true' : 'false');
  });

  // Pouch gold stepper.
  const wealth = data.wealth || {};
  const moneyRow = row(god);
  const goldLbl = el('span', 'fable-playground-lbl', `Gold: ${wealth.display || 0}`);
  moneyRow.appendChild(goldLbl);
  const step = (delta) => button(moneyRow, (delta > 0 ? '+' : '') + delta, 'wealth', String(delta));
  step(-100); step(-10); step(10); step(100);
  const customGold = numberInput(moneyRow, '');
  customGold.placeholder = '±';
  button(moneyRow, 'APPLY', 'wealth-custom');
  // The label self-updates after a successful op (the chip pattern) — it
  // rendered once at panel build, so every delta left a stale total.
  for (const btn of moneyRow.querySelectorAll('[data-pg-act="wealth"]')) {
    btn.addEventListener('click', async () => {
      const o = await run(god, 'playground_wealth_delta', { delta: Number(btn.dataset.pgData) },
        (r) => `${r.before} → ${r.after} (${r.display})`);
      if (o && o.display != null) goldLbl.textContent = `Gold: ${o.display}`;
    });
  }
  moneyRow.querySelector('[data-pg-act="wealth-custom"]').addEventListener('click', async () => {
    const d = Number(customGold.value);
    if (!Number.isFinite(d) || d === 0) { report(god, 'Enter a non-zero delta.', 'error'); return; }
    const o = await run(god, 'playground_wealth_delta', { delta: d },
      (r) => `${r.before} → ${r.after} (${r.display})`);
    if (o && o.display != null) goldLbl.textContent = `Gold: ${o.display}`;
  });

  // Inventory tag editor: container → item → tag chips.
  const containers = data.containers || {};
  const tagRow = row(god);
  const containerSel = select(tagRow, ['belt', 'pouch', 'pack'], 'pouch');
  const itemSel = document.createElement('select');
  itemSel.className = 'fable-playground-select';
  tagRow.appendChild(itemSel);
  const refillItems = () => {
    itemSel.innerHTML = '';
    const items = containers[containerSel.value] || [];
    if (!items.length) {
      const o = document.createElement('option');
      o.value = '';
      o.textContent = '(empty)';
      itemSel.appendChild(o);
      return;
    }
    for (const it of items) {
      const o = document.createElement('option');
      o.value = it.name;
      o.textContent = it.qty > 1 ? `${it.name} ×${it.qty}` : it.name;
      itemSel.appendChild(o);
    }
  };
  refillItems();
  containerSel.addEventListener('change', refillItems);
  const checks = {};
  for (const tag of ITEM_TAGS) {
    const chk = document.createElement('input');
    chk.type = 'checkbox';
    chk.dataset.pgTag = tag;
    tagRow.appendChild(chk);
    tagRow.appendChild(el('span', 'fable-playground-taglbl', tag));
    checks[tag] = chk;
  }
  const syncChecks = () => {
    const items = containers[containerSel.value] || [];
    const cur = items.find((it) => it.name === itemSel.value);
    for (const tag of ITEM_TAGS) {
      checks[tag].checked = !!(cur && (cur.tags || []).includes(tag));
    }
  };
  syncChecks();
  itemSel.addEventListener('change', syncChecks);
  button(tagRow, 'SET TAGS', 'item-tags').addEventListener('click', () => {
    if (!itemSel.value) { report(god, 'No item selected.', 'error'); return; }
    const tags = ITEM_TAGS.filter((t) => checks[t].checked);
    void run(god, 'playground_item_tag_set',
      { container: containerSel.value, name: itemSel.value, tags },
      (o) => `${o.name} (${o.container}): ${o.tags.length ? o.tags.join(', ') : 'no tags'}`);
  });

  // Hazard & Rest Testing.
  const hazard = section(panel, 'Hazard & Rest Testing');
  const diceRow = row(hazard);
  button(diceRow, 'TRAVEL D20', 'roll-travel').addEventListener('click', () => {
    void run(hazard, 'playground_hazard_roll', { kind: 'travel' }, (o) => o.detail);
  });
  button(diceRow, 'REST D20', 'roll-rest').addEventListener('click', () => {
    void run(hazard, 'playground_hazard_roll', { kind: 'rest' }, (o) => o.detail);
  });
  const restRow = row(hazard);
  button(restRow, 'FORCE REST', 'force-rest').addEventListener('click', () => {
    void run(restRow, 'playground_force_rest', {},
      (o) => `Rested — ${o.steps} steps, stamina ${o.stamina}, anchored ${o.anchored_at}`);
  });
  button(restRow, 'AMBUSH: IMPAIRED NOW', 'apply-impaired').addEventListener('click', () => {
    void run(restRow, 'playground_apply_interruption', {},
      (o) => `Impaired until ${o.expires_label}`);
  });
  const ambushRow = row(hazard);
  button(ambushRow, 'SPAWN HOSTILES HERE', 'spawn-hostiles').addEventListener('click', () => {
    // Hand off to the World tab's spawner, prefilled (creature-class group
    // of soldiers at the current area).
    spawnPrefill = { kind: 'group', tier: 'soldier', count: 3 };
    switchDomain('world');
  });
}

// ── NPC — The Cast Sculptor ──────────────────────────────────────────────
async function renderNpcPanel(panel) {
  let data;
  try {
    data = await invoke('playground_npc_get');
  } catch (err) {
    panel.appendChild(el('div', 'fable-playground-error', String(err)));
    return;
  }
  const cast = data.cast || [];
  const milestones = data.milestones || [];

  // State & Bond Overrides — one card per cast member.
  const overrides = section(panel, 'State & Bond Overrides');
  if (!cast.length) {
    overrides.appendChild(el('div', 'fable-playground-empty', 'No registered cast yet.'));
  }
  for (const npc of cast) {
    const card = el('div', 'fable-playground-npc');
    card.dataset.pgNpc = npc.id;
    overrides.appendChild(card);
    const head = el('div', 'fable-playground-npc__head');
    const name = npc.name || npc.id;
    head.appendChild(el('span', 'fable-playground-npc__name', name));
    head.appendChild(el('span', 'fable-playground-npc__id', npc.id));
    if (npc.prominence === 'core') head.appendChild(el('span', 'fable-playground-npc__core', 'CORE'));
    card.appendChild(head);

    // Relationship tier slider 0-7 (Nemesis → Bonded).
    const relRow = row(card);
    const curTier = TIER_LADDER.indexOf(String(npc.relationship_tier || 'stranger'));
    relRow.appendChild(el('span', 'fable-playground-lbl', 'Bond'));
    const slider = document.createElement('input');
    slider.type = 'range';
    slider.min = '0';
    slider.max = '7';
    slider.step = '1';
    slider.value = String(curTier < 0 ? 3 : curTier);
    slider.dataset.pgAct = 'tier-slider';
    relRow.appendChild(slider);
    const tierLbl = el('span', 'fable-playground-lbl fable-playground-tierlbl',
      TIER_LADDER[Number(slider.value)]);
    relRow.appendChild(tierLbl);
    slider.addEventListener('input', () => {
      tierLbl.textContent = TIER_LADDER[Number(slider.value)] || '';
    });
    button(relRow, 'SET', 'tier-set', npc.id).addEventListener('click', () => {
      const tier = TIER_LADDER[Number(slider.value)];
      void run(card, 'playground_relationship_set', { npcId: npc.id, tier },
        (o) => `${o.npc_id}: ${o.before} → ${o.tier}`);
    });
    // CLEAR HOSTILITY quick action (tier ≤ Rival → Stranger).
    if (curTier >= 0 && curTier <= 2) {
      button(relRow, 'CLEAR HOSTILITY', 'tier-clear', npc.id).addEventListener('click', () => {
        void run(card, 'playground_relationship_set', { npcId: npc.id, tier: 'stranger' },
          (o) => `${o.npc_id}: ${o.before} → ${o.tier}`);
      });
    }

    // Mood + intent.
    const moodRow = row(card);
    const moodIn = input(moodRow, npc.mood || '', 'mood');
    const intentIn = input(moodRow, npc.intent || '', 'intent');
    button(moodRow, 'SET INTERIOR', 'interior-set', npc.id).addEventListener('click', () => {
      void run(card, 'playground_npc_interior_set',
        { npcId: npc.id, mood: moodIn.value.trim() || null, intent: intentIn.value.trim() || null },
        (o) => `${o.npc_id}: ${o.mood || '—'} / ${o.intent || '—'}`);
    });
    if (npc.archived) {
      card.appendChild(el('div', 'fable-playground-npc__archived', `Archived: ${npc.archived}`));
    }
    // REVIVE when the interior is archived / dead-marked.
    if (npc.needs_revive) {
      const reviveRow = row(card);
      button(reviveRow, 'REVIVE', 'revive', npc.id).addEventListener('click', () => {
        void run(card, 'playground_npc_revive', { npcId: npc.id, name: npc.name || null, role: npc.role || null },
          (o) => `Revived ${npc.id}` +
            (o.registered ? ' (re-registered)' : '') +
            (o.interior_reset ? ', interior reset' : '') +
            (o.assets_removed && o.assets_removed.length
              ? `, ${o.assets_removed.length} corpse asset(s) removed` : ''));
      });
    }
  }

  // Registry Management.
  const registry = section(panel, 'Registry Management');
  const nearRow = row(registry);
  const nearInput = input(nearRow, '', 'near-name search (Kira…)');
  button(nearRow, 'SEARCH', 'near-search').addEventListener('click', async () => {
    const q = nearInput.value.trim();
    if (!q) { report(registry, 'Type a name to search.', 'error'); return; }
    try {
      const out = await invoke('playground_near_names', { query: q });
      nearResults(registry, q, out.candidates || []);
    } catch (err) {
      report(registry, String(err), 'error');
    }
  });
  // The ranked candidates + their merge actions.
  function nearResults(parent, query, candidates) {
    let list = parent.querySelector('.fable-playground-nearlist');
    if (list) list.innerHTML = '';
    else {
      list = el('div', 'fable-playground-nearlist');
      parent.appendChild(list);
    }
    if (!candidates.length) {
      list.appendChild(el('div', 'fable-playground-empty', `No near-names for “${query}”.`));
      return;
    }
    for (const c of candidates) {
      const r = row(list);
      r.appendChild(el('span', 'fable-playground-lbl', `${c.name} [${c.id}] · d=${c.distance}`));
      button(r, '+ ALIAS', 'near-alias', c.id).addEventListener('click', () => {
        void run(list, 'playground_registry_alias_add', { npcId: c.id, alias: query },
          (o) => `Alias added to ${o.npc_id}: ${query}`);
      });
      button(r, 'RENAME', 'near-rename', c.id).addEventListener('click', () => {
        void run(list, 'playground_registry_rename', { npcId: c.id, newName: query },
          (o) => `Renamed to ${o.name}` + (o.warning ? ` — ⚠ ${o.warning}` : ''));
      });
      button(r, 'REMOVE', 'near-remove', c.id).addEventListener('click', () => {
        void run(list, 'playground_registry_remove', { npcId: c.id },
          (o) => `Removed ${o.removed}`);
      });
    }
  }

  // Milestone injector: NPC select + milestone select → inject + report.
  const mileRow = row(registry);
  const npcSel = select(mileRow, cast.length
    ? cast.map((n) => ({ value: n.id, label: n.name || n.id }))
    : [{ value: '', label: '(no cast)' }]);
  const mileSel = select(mileRow, milestones.length
    ? milestones.map((m) => ({
        value: m.id,
        label: `${m.id}${m.hostility ? ' ⚡' : ''} (+${m.points})`,
      }))
    : [{ value: '', label: '(no milestones)' }]);
  button(mileRow, 'INJECT', 'milestone').addEventListener('click', () => {
    if (!npcSel.value || !mileSel.value) { report(registry, 'Pick an NPC and a milestone.', 'error'); return; }
    void run(registry, 'playground_milestone_inject', { npcId: npcSel.value, eventId: mileSel.value },
      (o) => `${o.npc_id}: ${o.tier_before} → ${o.tier_after} (${o.reason}; points ${o.points}/${o.threshold})`);
  });
}

// ── WORLD — The Engine & Environment ─────────────────────────────────────
async function renderWorldPanel(panel) {
  let data;
  try {
    data = await invoke('playground_world_get');
  } catch (err) {
    panel.appendChild(el('div', 'fable-playground-error', String(err)));
    return;
  }
  const clock = data.clock || {};
  const node = data.node || {};
  const map = data.map || {};

  // Time Machine & Evolution.
  const time = section(panel, 'Time Machine & Evolution');
  const clockRow = row(time);
  clockRow.appendChild(el('span', 'fable-playground-lbl', `${clock.label || '—'}${node.id ? ` · ${node.name || node.id}` : ''}`));
  const skipRow = row(time);
  const skipHour = button(skipRow, '+1h', 'skip', '60');
  const skipDay = button(skipRow, '+24h', 'skip', '1440');
  const skipCustom = numberInput(skipRow, '');
  skipCustom.placeholder = 'min';
  button(skipRow, 'SKIP', 'skip-custom');
  const doSkip = (minutes) => {
    void run(time, 'playground_time_skip', { minutes },
      (o) => `${o.label} (+${o.minutes} min)${o.fatigue_band ? ` · ${o.fatigue_band}` : ''}` +
        (o.economy_directives && o.economy_directives.length
          ? ` · ${o.economy_directives.length} economy directive(s)` : ''));
  };
  skipHour.addEventListener('click', () => doSkip(60));
  skipDay.addEventListener('click', () => doSkip(1440));
  skipRow.querySelector('[data-pg-act="skip-custom"]').addEventListener('click', () => {
    const m = Number(skipCustom.value);
    if (!Number.isFinite(m) || m <= 0) { report(time, 'Enter minutes (1-10080).', 'error'); return; }
    doSkip(m);
  });
  const tickRow = row(time);
  const tickBtn = button(tickRow, 'FORCE WORLD TICK', 'force-tick');
  tickBtn.addEventListener('click', () => {
    if (ticking) return;
    ticking = true;
    tickBtn.disabled = true;
    report(tickRow, 'Ticking… (the local pass takes a few seconds)', 'info');
    void (async () => {
      try {
        const out = await invoke('playground_force_tick');
        const dirs = out.directives || [];
        report(tickRow,
          `Tick done — ${out.entities_changed} entit${out.entities_changed === 1 ? 'y' : 'ies'} changed` +
          (out.summary_changed ? ', summary moved' : '') +
          (dirs.length ? `; ${dirs.length} directive(s):` : '; no directives'),
          'ok');
        for (const d of dirs.slice(0, 5)) {
          tickRow.appendChild(el('div', 'fable-playground-directive', d));
        }
      } catch (err) {
        report(tickRow, String(err), 'error');
      } finally {
        ticking = false;
        tickBtn.disabled = false;
      }
    })();
  });

  // Asset & Spawn Controls.
  const spawn = section(panel, 'Asset & Spawn Controls');
  if (!map.key) {
    spawn.appendChild(el('div', 'fable-playground-empty',
      'No site map at the current node — the spawner needs a mapped site.'));
  } else {
    const spawnRow = row(spawn);
    const kindSel = select(spawnRow, ASSET_KINDS, 'creature');
    const labelIn = input(spawnRow, '', 'label');
    const tierSel = select(spawnRow, SPAWN_TIERS, '');
    const countIn = numberInput(spawnRow, 1);
    const areaSel = select(spawnRow, (map.areas || []).length
      ? map.areas.map((a) => ({ value: a.id, label: a.name || a.id }))
      : [{ value: '', label: '(no areas)' }]);
    if (map.current_area) areaSel.value = map.current_area;
    // The AMBUSH handoff consumes its prefill ONCE.
    if (spawnPrefill) {
      kindSel.value = spawnPrefill.kind;
      tierSel.value = spawnPrefill.tier;
      countIn.value = String(spawnPrefill.count);
      spawnPrefill = null;
    }
    button(spawnRow, 'SPAWN', 'spawn-asset').addEventListener('click', () => {
      if (!areaSel.value) { report(spawn, 'The map has no areas.', 'error'); return; }
      void run(spawn, 'playground_asset_spawn', {
        kind: kindSel.value,
        label: labelIn.value.trim() || null,
        tier: tierSel.value || null,
        count: Number(countIn.value) || 1,
        area: areaSel.value,
      }, (o) => `Spawned ${o.asset.name} [${o.asset.id}] (${o.asset.kind}) in ${o.asset.area}`);
    });
    const threatRow = row(spawn);
    threatRow.appendChild(el('span', 'fable-playground-lbl', `Threat: ${map.threat || '—'}`));
    const threatSel = select(threatRow, THREATS, map.threat || 'moderate');
    button(threatRow, 'SET THREAT', 'set-threat').addEventListener('click', () => {
      void run(spawn, 'playground_site_threat_set', { threat: threatSel.value },
        (o) => `Threat set to ${o.threat} on ${o.map}`);
    });
  }

  // Loot & Timelines.
  const loot = section(panel, 'Loot & Timelines');
  const lootRow = row(loot);
  const lootTier = select(lootRow, SPAWN_TIERS.slice(1), 'soldier');
  const lootProsperity = numberInput(lootRow, 100);
  button(lootRow, 'ROLL LOOT', 'roll-loot').addEventListener('click', () => {
    void run(loot, 'playground_hazard_roll',
      { kind: 'loot', tier: lootTier.value, prosperity: Number(lootProsperity.value) || 100 },
      (o) => o.detail);
  });
  const ring = section(panel, 'Snapshot Ring Inspector');
  const history = data.history || [];
  ring.appendChild(el('span', 'fable-playground-depth', `depth ${history.length}`));
  if (!history.length) {
    ring.appendChild(el('div', 'fable-playground-empty', 'The undo ring is empty.'));
  }
  for (const h of history) {
    const r = row(ring);
    r.appendChild(el('span', 'fable-playground-lbl',
      `#${h.index} · ${h.clock} · turn ${h.turn_tag}${h.node ? ` · ${h.node}` : ''} · ${h.wealth}`));
    button(r, 'RESTORE', 'restore', String(h.index)).addEventListener('click', () => {
      void run(ring, 'playground_history_restore', { index: Number(h.index) },
        (o) => `Restored #${h.index} — ${o.entities_changed} entit${o.entities_changed === 1 ? 'y' : 'ies'} changed`);
    });
  }
}
