// Tab-rail Codex link-manager regression tests (2026-08-22 bug fix).
//
// The Codex tab (right Wupi drawer) reads the active card from
// `fable_card_get`, whose hand-built DTO (lib.rs) keys the slug as
// `card_id` — there is NO `id` field. The link manager invoked
// fable_codex_link_{get,set} with `cardId: card.id`, i.e. undefined:
// JSON serialization DROPS undefined keys, so Tauri rejected the call
// ("command fable_codex_link_get missing required cardid"), and the catch
// block replaced the whole tab with the error box — hiding every per-row
// ✎ edit + "+ Link" button with it.
//
// These tests drive the REAL module (no reimplementation): a minimal DOM +
// Tauri-internals stub (same pattern as wupi-drawer.test.mjs), the public
// buildTabRail → codex-tab click path, and pin the wire pair (a defined
// `cardId` == the DTO slug) + the rendered link manager + a link mutation.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';

// The rail's import chain pulls CSS — Node can't load it; stub every .css
// as an empty module.
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
    toggle: (c, on) => (on === undefined ? (set.has(c) ? set.delete(c) : set.add(c)) : on ? set.add(c) : set.delete(c)),
    contains: (c) => set.has(c),
  };
}
function makeEl(extra = {}) {
  const listeners = new Map();
  const el = {
    tagName: 'DIV',
    dataset: {},
    style: {},
    hidden: false,
    classList: makeClassList(),
    appendChild(c) { return c; },
    addEventListener(t, fn) { listeners.set(t, fn); },
    removeEventListener(t) { listeners.delete(t); },
    dispatch(t, ev) { const fn = listeners.get(t); if (fn) fn(ev || {}); return !!fn; },
    setAttribute() {},
    querySelector: () => null,
    querySelectorAll: () => [],
    ...extra,
  };
  let inner = '';
  Object.defineProperty(el, 'innerHTML', {
    get: () => inner,
    set(v) { inner = String(v); },
  });
  Object.defineProperty(el, 'className', {
    get: () => '',
    set() {},
  });
  return el;
}

// ---- Tauri internals stub ---------------------------------------------------
const calls = [];
let invokeImpl = async () => ({});
globalThis.window = globalThis;
globalThis.document = { createElement: () => makeEl(), querySelector: () => null };
window.__TAURI_INTERNALS__ = {
  transformCallback: () => 0,
  unregisterCallback() {},
  invoke: (cmd, args) => { calls.push({ cmd, args }); return invokeImpl(cmd, args); },
};

const settle = () => new Promise((r) => setTimeout(r, 5));

const rail = await import('../src/fable/engine/tab-rail.js');

// The full rail path: buildTabRail wires the five tab buttons; clicking the
// codex one runs renderTab → renderCodex against the stubbed invoke.
// (The shared `calls` log is reset per build so a later test's find() can
// never match an earlier test's invocation.)
function buildCodexTab({ card, library, links }) {
  calls.length = 0;
  // Canned link-manager rows renderCodex's wiring loop will walk. These
  // mirror the two library files the stub reports: one linked, one not.
  const rows = [
    makeLinkedRow('World Lore', ['up', 'down', 'edit', 'unlink']),
    makeLinkedRow('Monsters', ['link']),
  ];
  const body = makeEl({
    querySelectorAll: (sel) => (sel === '.fable-codex-link-manage-row' ? rows : []),
  });
  const headEditBtn = makeEl();
  const dropdown = makeEl({
    querySelector: (sel) => {
      if (sel === '[data-raw-edit]') return headEditBtn;
      if (sel === '[data-drop-body]') return body;
      return null;
    },
  });
  const tabButtons = ['player', 'card', 'codex', 'world', 'npc'].map((key) => {
    const btn = makeEl();
    btn.dataset.tab = key;
    return btn;
  });
  const railEl = makeEl({ querySelectorAll: () => tabButtons });
  const wrap = makeEl({
    querySelector: (sel) => {
      if (sel === '.fable-tab-rail') return railEl;
      if (sel === '[data-tab-dropdown]') return dropdown;
      return null;
    },
  });
  globalThis.document.createElement = () => wrap;
  rail.buildTabRail();
  // The rail keeps `activeTab` at module level — a codex click left over
  // from a prior test would TOGGLE the tab off instead of opening it.
  rail.resetTabRail();
  invokeImpl = async (cmd) => {
    if (cmd === 'fable_card_get') return card;
    if (cmd === 'fable_codex_library_list') return library;
    if (cmd === 'fable_codex_link_get') return links;
    return undefined;
  };
  return { body, headEditBtn, rows, tabButtons };
}

// One link-manager row: a name + its action buttons (act names match the
// rendered markup's data-act values).
function makeLinkedRow(name, acts) {
  const buttons = acts.map((act) => {
    const b = makeEl();
    b.dataset.act = act;
    return b;
  });
  const row = makeEl({
    querySelectorAll: (sel) => (sel === 'button[data-act]' ? buttons : []),
  });
  row.dataset.name = name;
  row.__buttons = buttons;
  return row;
}

test('codex tab: link_get carries the card DTO slug and renders the link manager', async () => {
  const card = { name: 'Liam', subtype: 'npc', card_id: 'liam' };
  const library = [
    { name: 'World Lore', entry_count: 3 },
    { name: 'Monsters', entry_count: 5 },
  ];
  const { body, headEditBtn, tabButtons } = buildCodexTab({ card, library, links: ['World Lore'] });

  tabButtons.find((b) => b.dataset.tab === 'codex').dispatch('click');
  await settle();

  // THE regression pin: `cardId` must arrive defined, equal to the DTO's
  // `card_id` (the old `card.id` was undefined → the key dropped → the
  // whole tab replaced by the error box).
  const linkGet = calls.find((c) => c.cmd === 'fable_codex_link_get');
  assert.ok(linkGet, 'fable_codex_link_get invoked');
  assert.equal(linkGet.args.cardId, 'liam');

  // The tab renders the link manager, not the error box — with the per-row
  // edit + link buttons present.
  assert.ok(body.innerHTML.includes('fable-codex-link-manage'), 'link manager rendered');
  assert.ok(!body.innerHTML.includes("Couldn't load the codex library"), 'no error box');
  assert.ok(body.innerHTML.includes('data-act="edit"'), 'per-row ✎ edit button rendered');
  assert.ok(body.innerHTML.includes('data-act="link"'), '+ Link button rendered');
  // The generic head ✎ stays hidden for the codex tab (per-row edits only).
  assert.equal(headEditBtn.hidden, true);
});

test('codex tab: linking a library file writes the full ordered list back', async () => {
  const card = { name: 'Liam', subtype: 'npc', card_id: 'liam' };
  const library = [
    { name: 'World Lore', entry_count: 3 },
    { name: 'Monsters', entry_count: 5 },
  ];
  const { rows, tabButtons } = buildCodexTab({ card, library, links: ['World Lore'] });

  tabButtons.find((b) => b.dataset.tab === 'codex').dispatch('click');
  await settle();

  const monsters = rows.find((r) => r.dataset.name === 'Monsters');
  monsters.__buttons.find((b) => b.dataset.act === 'link').dispatch('click');
  await settle();

  const set = calls.find((c) => c.cmd === 'fable_codex_link_set');
  assert.ok(set, 'fable_codex_link_set invoked');
  assert.equal(set.args.cardId, 'liam');
  assert.deepEqual(set.args.codices, ['World Lore', 'Monsters']);
});
