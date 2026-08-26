// Playground module regression tests (2026-08-23).
//
// These drive the REAL module (no reimplementation): a minimal DOM +
// Tauri-internals stub (the wupi-drawer / tab-rail-codex pattern), the
// public buildPlayground → initPlayground → toggle/domain path, and the
// four pinned surfaces from the plan:
//   (1) wand on/off toggles the strip classes WITHOUT touching the
//       tab-rail's active tab state (the covered rail survives intact),
//   (2) a domain switch renders the right panel,
//   (3) the invoke payload shapes (flags_set, wealth_delta, force_tick,
//       asset_spawn — camelCase keys, the anti-pattern-#5-safe surface),
//   (4) a rejected invoke renders the backend error VERBATIM inline.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';

// The import chain pulls CSS (tab-rail → raw-editor); Node can't load CSS.
registerHooks({
  load(url, context, nextLoad) {
    if (url.endsWith('.css')) {
      return { format: 'module', shortCircuit: true, source: '' };
    }
    return nextLoad(url, context);
  },
});

// ---- DOM stub ------------------------------------------------------------
function makeClassList() {
  const set = new Set();
  return {
    add: (...cs) => cs.forEach((c) => set.add(c)),
    remove: (...cs) => cs.forEach((c) => set.delete(c)),
    toggle: (c, on) => (on === undefined
      ? (set.has(c) ? set.delete(c) : set.add(c))
      : (on ? set.add(c) : set.delete(c))),
    contains: (c) => set.has(c),
    __set: () => set,
    __clear: () => set.clear(),
  };
}

// A tiny class/attribute selector matcher over the stub tree. Supports
// '.class', '[data-x="y"]', and ':scope > .class' (direct children only).
function parseSel(sel) {
  const scope = sel.startsWith(':scope > ');
  if (scope) sel = sel.slice(9);
  const attr = sel.match(/^\[data-([a-z-]+)="(.*)"\]$/);
  return { scope, attr, cls: attr ? null : sel.replace(/^\./, '') };
}
function elMatches(el, m) {
  if (!el || typeof el !== 'object') return false;
  if (m.attr) {
    const key = m.attr[1].replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    return String(el.dataset[key] ?? '') === m.attr[2];
  }
  return el.classList && el.classList.contains(m.cls);
}
function queryAll(root, sel) {
  const m = parseSel(sel);
  const out = [];
  const walk = (el) => {
    for (const c of el.children || []) {
      if (elMatches(c, m)) out.push(c);
      if (!m.scope) walk(c);
    }
  };
  walk(root);
  return out;
}

function makeEl(tag) {
  const listeners = new Map();
  const attrs = new Map();
  const el = {
    tagName: (tag || 'div').toUpperCase(),
    children: [],
    dataset: {},
    style: {},
    hidden: false,
    disabled: false,
    type: '',
    value: '',
    placeholder: '',
    min: '',
    max: '',
    step: '',
    checked: false,
    textContent: '',
    parentNode: null,
    listeners,
    __attrs: attrs,
    classList: makeClassList(),
    appendChild(c) { el.children.push(c); c.parentNode = el; return c; },
    addEventListener(t, fn) { listeners.set(t, fn); },
    removeEventListener(t) { listeners.delete(t); },
    dispatch(t, ev) { const fn = listeners.get(t); if (fn) fn(ev || {}); return !!fn; },
    setAttribute(name, val) { attrs.set(name, String(val)); },
    removeAttribute(name) { attrs.delete(name); },
    getAttribute(name) { return attrs.has(name) ? attrs.get(name) : null; },
    querySelector(sel) { return queryAll(el, sel)[0] || null; },
    querySelectorAll(sel) { return queryAll(el, sel); },
    focus() {},
  };
  Object.defineProperty(el, 'className', {
    get() { return [...el.classList.__set()].join(' '); },
    set(v) {
      el.classList.__clear();
      String(v).split(/\s+/).filter(Boolean).forEach((c) => el.classList.add(c));
    },
  });
  let inner = '';
  Object.defineProperty(el, 'innerHTML', {
    get() { return inner; },
    set(v) {
      inner = String(v);
      el.children = []; // the module clears panels via innerHTML = ''
    },
  });
  return el;
}

// ---- Tauri internals stub ---------------------------------------------------
const calls = [];
let invokeImpl = async () => ({});
globalThis.window = globalThis;
globalThis.document = { createElement: (tag) => makeEl(tag), querySelector: () => null };
window.__TAURI_INTERNALS__ = {
  transformCallback: () => 0,
  unregisterCallback() {},
  invoke: (cmd, args) => { calls.push({ cmd, args }); return invokeImpl(cmd, args); },
};

const settle = () => new Promise((r) => setTimeout(r, 10));
const find = (cmd) => calls.filter((c) => c.cmd === cmd);

const pg = await import('../src/fable/engine/playground.js');
const rail = await import('../src/fable/engine/tab-rail.js');

// Build the playground against fresh stubs. The tab rail's MODULE state
// (activeTab) is reset first so a prior test's covered tab never re-renders
// into a torn-down stub on wand-off — but the rail may never have been
// built in an isolated run (dropdownEl null), hence the guard.
function buildPlaygroundStub() {
  calls.length = 0;
  invokeImpl = async () => ({});
  try { rail.resetTabRail(); } catch { /* rail not built in this process yet */ }
  pg.resetPlayground(); // force wandOn=false before each test (leak guard)
  const wrap = pg.buildPlayground();
  const wand = makeEl('button');
  const railWrap = makeEl('div');
  pg.initPlayground({ wandBtn: wand, railWrap });
  return { wrap, wand, railWrap };
}

// Find the strip + panel among the wrap's children by class.
function childWithClass(root, cls) {
  return (root.children || []).find((c) => c.classList && c.classList.contains(cls)) || null;
}
// The strip's domain button for a key (buttons live inside the strip's
// __domains container — a grandchild of the strip).
function domainBtn(wrap, key) {
  const strip = childWithClass(wrap, 'fable-playground-strip');
  return queryAll(strip, `[data-pg-domain="${key}"]`)[0] || null;
}
// All section titles currently rendered in the panel.
function panelTitles(wrap) {
  const panel = childWithClass(wrap, 'fable-playground-panel');
  const titles = [];
  for (const sec of panel.children) {
    if (sec.classList.contains('fable-playground-section')) {
      const t = (sec.children || []).find((c) => c.classList.contains('fable-playground-section__title'));
      if (t) titles.push(t.textContent);
    }
  }
  return titles;
}
// A rendered report line anywhere under the panel (optionally by kind).
function findReport(wrap, kind) {
  const panel = childWithClass(wrap, 'fable-playground-panel');
  const all = queryAll({ children: panel.children }, '.fable-playground-report');
  return kind ? all.find((r) => r.classList.contains('is-' + kind)) : all[0];
}
function panelControls(wrap, act) {
  const panel = childWithClass(wrap, 'fable-playground-panel');
  return queryAll({ children: panel.children }, `[data-pg-act="${act}"]`);
}

// The canned PLAYGROUND panel payload every test hydrates from.
const PLAYER_GET = {
  flags: { auto_pass: false, freeze_clamps: false },
  wealth: { amount: 100, display: '100', currency_label: 'gold' },
  status: {},
  containers: {
    belt: [],
    pouch: [{ name: 'Gold', qty: 5, tags: ['pouchable'] }],
    pack: [],
  },
};
const NPC_GET = {
  cast: [{ id: 'kira', name: 'Kira', role: 'the herbalist', prominence: 'named',
           aliases: ['kira'], mood: null, intent: null, archived: null,
           relationship_tier: 'stranger', needs_revive: false }],
  milestones: [{ id: 'saved_life', points: 3, hostility: false }],
};
const WORLD_GET = {
  clock: { minutes: 540, label: 'Day 1, 09:00' },
  node: { id: 'tavern', name: 'Tavern' },
  map: {
    key: 'tavern', threat: 'low', current_area: 'hall',
    areas: [{ id: 'hall', name: 'Hall' }, { id: 'cellar', name: 'Cellar' }],
    assets: [],
  },
  history: [{ index: 0, turn_tag: 3, clock: 'Day 1, 09:00', node: 'tavern', wealth: 12 }],
};

// ---- (1) wand on/off: strip classes, rail untouched ------------------------

test('wand toggle covers the strip without touching tab-rail active state', async () => {
  calls.length = 0;
  // Wire the REAL tab rail with a structured dropdown stub so the wand-off
  // re-render (renderActive → renderTab → renderPlayer) resolves against
  // stubbed head/body elements (the tab-rail-codex pattern).
  const dropBody = makeEl('div');
  const dropEdit = makeEl('button');
  const dropName = makeEl('span');
  const dropTitle = makeEl('span');
  const dropHead = makeEl('div');
  const dropdown = makeEl('div');
  dropdown.querySelector = (sel) => {
    if (sel === '[data-raw-edit]') return dropEdit;
    if (sel === '[data-drop-body]') return dropBody;
    if (sel === '[data-drop-title]') return dropTitle;
    if (sel === '[data-drop-name]') return dropName;
    if (sel === '.fable-tab-drop__head') return dropHead;
    return null;
  };
  const tabButtons = ['player', 'card', 'codex', 'world', 'npc'].map((key) => {
    const b = makeEl('button');
    b.dataset.tab = key;
    return b;
  });
  const railEl = makeEl('div');
  railEl.querySelectorAll = () => tabButtons;
  const railWrapEl = makeEl('div');
  railWrapEl.querySelector = (sel) => {
    if (sel === '.fable-tab-rail') return railEl;
    if (sel === '[data-tab-dropdown]') return dropdown;
    return null;
  };
  const savedCreate = globalThis.document.createElement;
  globalThis.document.createElement = () => railWrapEl;
  rail.buildTabRail();
  globalThis.document.createElement = savedCreate;
  // The player tab's dropdown render invokes these — stub benign payloads.
  invokeImpl = async () => ({});

  // Make the player tab ACTIVE (click it through the rail's own wiring).
  tabButtons[0].dispatch('click');
  await settle();
  assert.ok(tabButtons[0].classList.contains('is-active'), 'rail tab active before the cover');
  assert.equal(dropdown.hidden, false, 'dropdown open before the cover');

  // Now the playground over that rail — initPlayground gets the RAIL'S OWN
  // wrap (the structured stub above), exactly as stage.js passes the real
  // .fable-tab-rail-wrap.
  const wrap = pg.buildPlayground();
  const wand = makeEl('button');
  pg.initPlayground({ wandBtn: wand, railWrap: railWrapEl });
  const strip = childWithClass(wrap, 'fable-playground-strip');
  const panel = childWithClass(wrap, 'fable-playground-panel');

  // Wand ON: strip opens, wand presses, rail is aria-hidden + inert, the
  // dropdown collapsed — but the tab's is-active state is UNTOUCHED.
  pg.togglePlayground();
  await settle();
  assert.ok(strip.classList.contains('is-open'), 'strip is-open on wand-on');
  assert.equal(wand.getAttribute('aria-pressed'), 'true', 'wand pressed');
  assert.equal(pg.isWandOn(), true);
  assert.equal(railWrapEl.getAttribute('aria-hidden'), 'true', 'rail aria-hidden while covered');
  assert.equal(railWrapEl.getAttribute('inert'), '', 'rail inert while covered');
  assert.equal(dropdown.hidden, true, 'open dropdown collapsed non-destructively');
  assert.ok(tabButtons[0].classList.contains('is-active'), 'rail tab STAYS active under the cover');

  // Wand OFF: strip closes, rail restored, tab still active (wand-off
  // re-renders the covered tab through renderActive — activeTab survived
  // because the cover never reset it).
  pg.togglePlayground();
  await settle();
  assert.ok(!strip.classList.contains('is-open'), 'strip closed on wand-off');
  assert.equal(wand.getAttribute('aria-pressed'), 'false', 'wand unpressed');
  assert.equal(railWrapEl.getAttribute('aria-hidden'), null, 'rail visible again');
  assert.equal(railWrapEl.getAttribute('inert'), null, 'rail interactive again');
  assert.ok(tabButtons[0].classList.contains('is-active'), 'rail tab still active after uncover');

  // resetPlayground (stage teardown) collapses + unpresses AND clears the
  // domain (2026-08-25) — no domain survives a reset cycle.
  pg.togglePlayground();
  await settle();
  pg.resetPlayground();
  await settle();
  assert.ok(!strip.classList.contains('is-open'), 'reset collapses the strip');
  assert.equal(wand.getAttribute('aria-pressed'), 'false', 'reset unpresses the wand');
  assert.equal(panel.hidden, true, 'panel hidden after reset');
  assert.equal(pg.isWandOn(), false);
  // Re-open: the strip opens with NO domain auto-selected — the panel stays
  // collapsed until a domain button is clicked (the 2026-08-25 law).
  pg.togglePlayground();
  await settle();
  assert.ok(strip.classList.contains('is-open'), 're-open re-opens the strip');
  assert.equal(panel.hidden, true, 'no domain auto-selected on re-open');
  pg.resetPlayground();
  await settle();
  rail.resetTabRail();
});

// ---- (2) domain switch renders the right panel ------------------------------

test('domain switch renders the right panel', async () => {
  const { wrap } = buildPlaygroundStub();
  invokeImpl = async (cmd) => {
    if (cmd === 'playground_player_get') return PLAYER_GET;
    if (cmd === 'playground_npc_get') return NPC_GET;
    if (cmd === 'playground_world_get') return WORLD_GET;
    return {};
  };

  // Wand on opens the strip with NO domain auto-selected (2026-08-25) —
  // the panel hydrates only after a domain button click.
  pg.togglePlayground();
  await settle();
  domainBtn(wrap, 'player').dispatch('click');
  await settle();
  assert.ok(panelTitles(wrap).includes('God Mode & Sculpting'), `player panel: ${panelTitles(wrap)}`);
  assert.ok(panelTitles(wrap).includes('Hazard & Rest Testing'));

  // NPC domain.
  domainBtn(wrap, 'npc').dispatch('click');
  await settle();
  assert.ok(panelTitles(wrap).includes('State & Bond Overrides'), `npc panel: ${panelTitles(wrap)}`);
  assert.ok(panelTitles(wrap).includes('Registry Management'));

  // World domain.
  domainBtn(wrap, 'world').dispatch('click');
  await settle();
  assert.ok(panelTitles(wrap).includes('Time Machine & Evolution'), `world panel: ${panelTitles(wrap)}`);
  assert.ok(panelTitles(wrap).includes('Asset & Spawn Controls'));
  assert.ok(panelTitles(wrap).includes('Snapshot Ring Inspector'));
  // Leave the playground collapsed on the PLAYER domain for the next test.
  domainBtn(wrap, 'player').dispatch('click');
  await settle();
  pg.resetPlayground();
  await settle();
});

// ---- (3) invoke payload shapes ----------------------------------------------

test('invoke payload shapes: flags_set, wealth_delta, force_tick, asset_spawn', async () => {
  const { wrap } = buildPlaygroundStub();
  invokeImpl = async (cmd) => {
    if (cmd === 'playground_player_get') return PLAYER_GET;
    if (cmd === 'playground_world_get') return WORLD_GET;
    if (cmd === 'playground_time_skip') {
      return { label: 'Day 1, 10:00', minutes: 60, economy_directives: [], fatigue_band: '' };
    }
    if (cmd === 'playground_force_tick') {
      return { entities_changed: 2, summary_changed: false, events_before: 3, events_after: 3, directives: ['x moved'] };
    }
    return {};
  };

  pg.togglePlayground(); // opens the strip (no domain auto-selected)
  await settle();
  // Defensive: force the player domain no matter what came before.
  domainBtn(wrap, 'player').dispatch('click');
  await settle();

  // flags_set: the AUTO-PASS chip sends the camelCase key.
  const autoChip = panelControls(wrap, 'flag-auto-pass')[0];
  assert.ok(autoChip, 'AUTO-PASS chip rendered');
  autoChip.dispatch('click');
  await settle();
  const flagCall = find('playground_flags_set').pop();
  assert.ok(flagCall, 'playground_flags_set invoked');
  assert.deepEqual(flagCall.args, { autoPass: true }, `flags payload: ${JSON.stringify(flagCall.args)}`);

  // wealth_delta: the +10 stepper (the [data-pg-data] attr carries it).
  const plus10 = panelControls(wrap, 'wealth').find((b) => b.dataset.pgData === '10');
  assert.ok(plus10, '+10 stepper rendered');
  plus10.dispatch('click');
  await settle();
  const wealthCall = find('playground_wealth_delta').pop();
  assert.ok(wealthCall, 'playground_wealth_delta invoked');
  assert.deepEqual(wealthCall.args, { delta: 10 }, `wealth payload: ${JSON.stringify(wealthCall.args)}`);

  // force_tick + asset_spawn on the World domain.
  domainBtn(wrap, 'world').dispatch('click');
  await settle();

  const tickBtn = panelControls(wrap, 'force-tick')[0];
  assert.ok(tickBtn, 'FORCE WORLD TICK button rendered');
  tickBtn.dispatch('click');
  await settle();
  await settle();
  const tickCall = find('playground_force_tick').pop();
  assert.ok(tickCall, 'playground_force_tick invoked');
  assert.deepEqual(tickCall.args, {}, 'force_tick sends no params');

  const spawnBtn = panelControls(wrap, 'spawn-asset')[0];
  assert.ok(spawnBtn, 'SPAWN button rendered');
  spawnBtn.dispatch('click');
  await settle();
  const spawnCall = find('playground_asset_spawn').pop();
  assert.ok(spawnCall, 'playground_asset_spawn invoked');
  assert.deepEqual(
    spawnCall.args,
    { kind: 'creature', label: null, tier: null, count: 1, area: 'hall' },
    `spawn payload: ${JSON.stringify(spawnCall.args)}`,
  );
  // Park back on player + collapse for the next test.
  domainBtn(wrap, 'player').dispatch('click');
  await settle();
  pg.resetPlayground();
  await settle();
});

// ---- (4) inline error rendering on a rejected invoke ------------------------

test('rejected invoke renders the backend error verbatim inline', async () => {
  const { wrap } = buildPlaygroundStub();
  const backendError = 'no fable game active: call fable_start first';
  invokeImpl = async (cmd) => {
    if (cmd === 'playground_player_get') return PLAYER_GET;
    if (cmd === 'playground_force_rest') throw backendError;
    return {};
  };

  pg.togglePlayground();
  await settle();
  domainBtn(wrap, 'player').dispatch('click');
  await settle();
  const forceRest = panelControls(wrap, 'force-rest')[0];
  assert.ok(forceRest, 'FORCE REST rendered');
  forceRest.dispatch('click');
  await settle();

  const errLine = findReport(wrap, 'error');
  assert.ok(errLine, 'an error report line rendered');
  assert.equal(errLine.textContent, backendError, 'backend error surfaces VERBATIM');
  pg.resetPlayground();
  await settle();
});
