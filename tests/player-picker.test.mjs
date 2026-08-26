// Player Picker regression tests (2026-08-24 delete-confirm teardown).
//
// The 2026-08-24 fix called closeConfirm(root) on re-entry + on every modal
// open, but the function was never defined — renderPlayerPicker THREW
// "closeConfirm is not defined" on every entry, leaving the grid permanently
// empty. These pin, against the REAL module (stubbed Tauri + DOM, same
// pattern as tab-rail-codex / raw-editor-player suites):
//   • renderPlayerPicker resolves + renders the grid tiles (the
//     ReferenceError regression),
//   • a delete-confirm left open is torn down on re-entry: the element
//     hides AND its stale Yes listener is released — a click after
//     re-entry can never reach fable_player_delete against the dead target,
//   • a fresh modal open (refreshPlayerModal, the raw-editor save path)
//     tears down a leftover confirm too.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';

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
    disabled: false,
    value: '',
    textContent: '',
    offsetWidth: 0,
    classList: makeClassList(),
    focus() {},
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
  return el;
}

// ---- Tauri internals stub ---------------------------------------------------
const calls = [];
let invokeImpl = async () => ({});
globalThis.window = globalThis;
globalThis.document = {
  createElement: () => makeEl(),
  querySelector: () => null,
  addEventListener() {},
  removeEventListener() {},
};
window.__TAURI_INTERNALS__ = {
  transformCallback: () => 0,
  unregisterCallback() {},
  invoke: (cmd, args) => { calls.push({ cmd, args }); return invokeImpl(cmd, args); },
};

const settle = () => new Promise((r) => setTimeout(r, 5));
const picker = await import('../src/fable/screens/player-picker.js');

// The picker screen root, mirroring buildPlayerPicker's static DOM. The modal
// card memoizes per-selector stubs so openModal's button wiring lands on the
// elements the test drives; the host records appended grid tiles.
function buildRoot() {
  const cardMemos = new Map();
  const card = makeEl({
    querySelector: (sel) => {
      if (!cardMemos.has(sel)) cardMemos.set(sel, makeEl());
      return cardMemos.get(sel);
    },
  });
  const appended = [];
  const host = makeEl({ appendChild(c) { appended.push(c); return c; } });
  const map = {
    '[data-host]': host,
    '[data-modal]': makeEl(),
    '[data-modal-card]': card,
    '[data-confirm]': makeEl(),
    '[data-confirm-msg]': makeEl(),
    '[data-confirm-yes]': makeEl(),
    '[data-confirm-no]': makeEl(),
  };
  return {
    root: { querySelector: (sel) => map[sel] || null },
    map,
    cardMemos,
    appended,
  };
}

const PLAYER_LIST = [{ id: 'p1', name: 'Alex', has_portrait: false }];
const PLAYER_FULL = { id: 'p1', name: 'Alex' };

invokeImpl = async (cmd) => {
  if (cmd === 'fable_players_list') return PLAYER_LIST;
  if (cmd === 'fable_player_get') return PLAYER_FULL;
  return undefined;
};

test('renderPlayerPicker resolves and renders the grid tiles (closeConfirm is defined)', async () => {
  const { root, appended } = buildRoot();
  // The dangling closeConfirm call site (2026-08-24) used to throw
  // ReferenceError before the grid could populate — resolving + appending
  // the tile IS the regression pin.
  await picker.renderPlayerPicker(root, {});
  await settle();
  assert.equal(appended.length, 1, 'one tile appended per listed player');
});

test('a delete-confirm left open is torn down on re-entry (stale Yes can never fire)', async () => {
  const { root, map, cardMemos, appended } = buildRoot();
  await picker.renderPlayerPicker(root, {});
  await settle();

  // Open the player modal via its grid tile click, then arm the confirm via
  // the modal's DELETE button.
  appended[0].dispatch('click');
  await settle();
  cardMemos.get('[data-modal-delete]').dispatch('click');
  await settle();
  const confirmEl = map['[data-confirm]'];
  assert.equal(confirmEl.hidden, false, 'confirm visible after DELETE');
  assert.ok(confirmEl.classList.contains('is-open'), 'confirm open class stamped');

  // Re-enter the picker (the abandonment): the confirm must hide + release
  // its Yes listener.
  calls.length = 0;
  await picker.renderPlayerPicker(root, {});
  await settle();
  assert.equal(confirmEl.hidden, true, 'confirm torn down on re-entry');

  // The stale Yes click must be a dead listener — no delete IPC fires.
  map['[data-confirm-yes]'].dispatch('click');
  await settle();
  assert.ok(!calls.some((c) => c.cmd === 'fable_player_delete'),
    'a stale Yes after re-entry never reaches fable_player_delete');
});

test('a fresh modal open also tears down a leftover confirm', async () => {
  const { root, map, cardMemos, appended } = buildRoot();
  await picker.renderPlayerPicker(root, {});
  await settle();

  appended[0].dispatch('click');
  await settle();
  // Arm the confirm, then re-open the modal through the exported refresh
  // path (the raw-XML editor save flow): openModal runs closeConfirm on
  // every fresh open.
  cardMemos.get('[data-modal-delete]').dispatch('click');
  await settle();
  const confirmEl = map['[data-confirm]'];
  assert.equal(confirmEl.hidden, false, 'confirm armed');

  picker.refreshPlayerModal(root, PLAYER_LIST[0]);
  await settle();
  assert.equal(confirmEl.hidden, true, 'confirm torn down by the fresh modal open');
});
