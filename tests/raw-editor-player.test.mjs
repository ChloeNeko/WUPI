// Raw-editor PLAYER tab combined-format tests (2026-08-22, Chloe).
//
// The Wupi drawer's Player ✎ editor carries BOTH halves of the attached
// player in ONE text field: the `.player` identity XML on top, the active
// session's player.json (`{ "player_state": … }`) just underneath, behind a
// `===== player.json =====` divider line. These pin:
//   • the split/combine pair (divider tolerance, no-divider = XML-only),
//   • the combined validity gate (json checker scoped to the half below the
//     divider — ✓ stays armed on good input, locks on broken JSON, broken
//     XML, an empty half, or a deleted divider),
//   • the open→edit→save flow against the REAL module (stubbed Tauri):
//     each half writes through its own IPC ONLY when it changed, and an
//     XML-only session (no player.json yet) keeps the legacy whole-text
//     save.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';

// The editor's import chain pulls CSS — Node can't load it; stub every .css
// as an empty module (same pattern as wupi-drawer / tab-rail-codex suites).
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
  return {
    tagName: 'DIV',
    dataset: {},
    style: {},
    hidden: false,
    disabled: false,
    value: '',
    placeholder: '',
    textContent: '',
    classList: makeClassList(),
    focus() {},
    appendChild(c) { return c; },
    addEventListener(t, fn) { listeners.set(t, fn); },
    removeEventListener(t) { listeners.delete(t); },
    dispatch(t, ev) { const fn = listeners.get(t); if (fn) fn(ev || {}); return !!fn; },
    setAttribute() {},
    ...extra,
  };
}

// ---- Tauri internals stub ---------------------------------------------------
const calls = [];
let invokeImpl = async () => ({});
globalThis.window = globalThis;
// buildRawEditor queries its controls off the created overlay — memoize one
// stub per selector so listeners land on the same elements the test drives.
const selMemos = new Map();
globalThis.document = {
  createElement: () => makeEl({
    querySelector: (sel) => {
      if (!selMemos.has(sel)) selMemos.set(sel, makeEl());
      return selMemos.get(sel);
    },
  }),
};
window.__TAURI_INTERNALS__ = {
  transformCallback: () => 0,
  unregisterCallback() {},
  invoke: (cmd, args) => { calls.push({ cmd, args }); return invokeImpl(cmd, args); },
};

const settle = () => new Promise((r) => setTimeout(r, 5));
const editor = await import('../src/fable/engine/raw-editor.js');

const XML = '<player>\n  <metadata>\n    <id>alex</id>\n  </metadata>\n</player>';
const JSON_TXT = '{\n  "player_state": {\n    "wealth": 12,\n    "stamina": 100\n  }\n}';
const DIVIDER = '===== player.json =====';

// ---- pure helpers ----------------------------------------------------------
test('splitPlayerRawText: canonical + tolerant dividers, no-divider passthrough', () => {
  const canon = editor.splitPlayerRawText(`${XML}\n\n${DIVIDER}\n\n${JSON_TXT}`);
  assert.equal(canon.xml, XML);
  assert.equal(canon.json, JSON_TXT);

  // A hand-typed equals count still splits (whole-line match, any = count).
  const sloppy = editor.splitPlayerRawText(`${XML}\n=== player.json ===\n${JSON_TXT}`);
  assert.equal(sloppy.xml, XML);
  assert.equal(sloppy.json, JSON_TXT);

  // No divider → XML-only mode (json null, text verbatim).
  const none = editor.splitPlayerRawText(XML);
  assert.equal(none.xml, XML);
  assert.equal(none.json, null);
});

test('combinePlayerRawText: empty JSON half yields the XML alone; round-trips', () => {
  assert.equal(editor.combinePlayerRawText(XML, ''), XML);
  assert.equal(editor.combinePlayerRawText(XML, '   \n'), XML);

  const combined = editor.combinePlayerRawText(XML, JSON_TXT);
  assert.ok(combined.includes(DIVIDER));
  assert.ok(combined.endsWith(JSON_TXT));
  assert.ok(combined.startsWith(XML));
  const back = editor.splitPlayerRawText(combined);
  assert.equal(back.xml, XML);
  assert.equal(back.json, JSON_TXT);
});

test('playerRawTextLooksValid: each half gates in its own format', () => {
  assert.equal(editor.playerRawTextLooksValid(XML), true, 'XML alone (legacy shape)');
  assert.equal(editor.playerRawTextLooksValid(editor.combinePlayerRawText(XML, JSON_TXT)), true, 'both halves good');
  assert.equal(
    editor.playerRawTextLooksValid(editor.combinePlayerRawText(XML, '{ "player_state": ')),
    false, 'broken JSON half locks',
  );
  assert.equal(
    editor.playerRawTextLooksValid(editor.combinePlayerRawText('<player><metadata>', JSON_TXT)),
    false, 'broken XML half locks',
  );
  assert.equal(
    editor.playerRawTextLooksValid(`${XML}\n\n${DIVIDER}\n\n`),
    false, 'empty half after the divider locks',
  );
  // Divider deleted but JSON kept: the tag-count sniff would balance through
  // the trailing JSON — the gate must catch it client-side.
  assert.equal(editor.playerRawTextLooksValid(`${XML}\n\n${JSON_TXT}`), false, 'JSON without a divider locks');
});

// ---- open / checker / revert / save flows (real module) --------------------
function build() {
  selMemos.clear();
  editor.buildRawEditor();
  return {
    title: selMemos.get('[data-raw-title]'),
    textarea: selMemos.get('[data-raw-text]'),
    save: selMemos.get('[data-raw-save]'),
    revert: selMemos.get('[data-raw-revert]'),
    backdrop: selMemos.get('.fable-raw-editor-backdrop'),
  };
}

test('open: player.json lands in the same field under the <player> XML; ✓ armed', async () => {
  const ui = build();
  invokeImpl = async (cmd) => {
    if (cmd === 'fable_active_player_get') return { id: 'alex' };
    if (cmd === 'fable_player_raw_get') return XML;
    if (cmd === 'fable_json_raw_get') return JSON_TXT;
    return undefined;
  };
  await editor.openRawEditor('player');
  await settle();

  assert.ok(ui.textarea.value.startsWith('<player>'), 'XML on top');
  assert.ok(ui.textarea.value.includes(DIVIDER), 'divider present');
  assert.ok(ui.textarea.value.includes('"player_state"'), 'JSON underneath');
  assert.equal(ui.save.disabled, false, 'green ✓ enabled on a good combined load');
  assert.ok(editor.isOpen());
});

test('json checker lock + ↻ revert work on the combined text', async () => {
  const ui = build();
  invokeImpl = async (cmd) => {
    if (cmd === 'fable_active_player_get') return { id: 'alex' };
    if (cmd === 'fable_player_raw_get') return XML;
    if (cmd === 'fable_json_raw_get') return JSON_TXT;
    return undefined;
  };
  await editor.openRawEditor('player');
  await settle();
  const loaded = ui.textarea.value;

  // Break ONLY the JSON half → the checker locks ✓ (input-driven revalidate).
  ui.textarea.value = loaded.replace('"wealth": 12', '"wealth": {{{');
  ui.textarea.dispatch('input');
  assert.equal(ui.save.disabled, true, '✓ disabled on a broken JSON half');
  assert.ok(ui.textarea.classList.contains('invalid'), 'red outline engaged');

  // ↻ (revert) restores the combined last-good and re-arms ✓.
  ui.revert.dispatch('click');
  assert.equal(ui.textarea.value, loaded, 'revert restores the combined text');
  assert.equal(ui.save.disabled, false, '✓ re-enabled after revert');
});

test('save: a JSON-half edit writes ONLY fable_json_raw_set (player kind)', async () => {
  const ui = build();
  invokeImpl = async (cmd) => {
    if (cmd === 'fable_active_player_get') return { id: 'alex' };
    if (cmd === 'fable_player_raw_get') return XML;
    if (cmd === 'fable_json_raw_get') return JSON_TXT;
    return undefined;
  };
  await editor.openRawEditor('player');
  await settle();
  calls.length = 0;

  ui.textarea.value = ui.textarea.value.replace('"wealth": 12', '"wealth": 20');
  ui.textarea.dispatch('input');
  ui.save.dispatch('click');
  await settle();

  const set = calls.find((c) => c.cmd === 'fable_json_raw_set');
  assert.ok(set, 'fable_json_raw_set invoked');
  assert.equal(set.args.kind, 'player');
  assert.equal(JSON.parse(set.args.json).player_state.wealth, 20);
  assert.ok(!set.args.json.includes('<player>'), 'the XML half never rides in the JSON save');
  assert.ok(!calls.some((c) => c.cmd === 'fable_player_raw_set'), 'unchanged XML half is not rewritten');
  assert.equal(editor.isOpen(), false, '✓ closes on success');
});

test('save: an XML-half edit writes ONLY the .player XML (divider + JSON stripped)', async () => {
  const ui = build();
  invokeImpl = async (cmd) => {
    if (cmd === 'fable_active_player_get') return { id: 'alex' };
    if (cmd === 'fable_player_raw_get') return XML;
    if (cmd === 'fable_json_raw_get') return JSON_TXT;
    return undefined;
  };
  await editor.openRawEditor('player');
  await settle();
  calls.length = 0;

  ui.textarea.value = ui.textarea.value.replace('</metadata>', '  <extra>x</extra>\n  </metadata>');
  ui.textarea.dispatch('input');
  ui.save.dispatch('click');
  await settle();

  const set = calls.find((c) => c.cmd === 'fable_player_raw_set');
  assert.ok(set, 'fable_player_raw_set invoked');
  assert.equal(set.args.id, 'alex');
  assert.ok(set.args.xml.startsWith('<player>'), 'the XML half is the payload');
  assert.ok(!set.args.xml.includes(DIVIDER), 'divider stripped from the XML save');
  assert.ok(!set.args.xml.includes('player_state'), 'JSON half stripped from the XML save');
  assert.ok(!calls.some((c) => c.cmd === 'fable_json_raw_set'), 'unchanged JSON half is not recomposed over the live schema');
});

test('XML-only session (no player.json yet): legacy shape end to end', async () => {
  const ui = build();
  invokeImpl = async (cmd) => {
    if (cmd === 'fable_active_player_get') return { id: 'alex' };
    if (cmd === 'fable_player_raw_get') return XML;
    if (cmd === 'fable_json_raw_get') return ''; // not-yet-written file
    return undefined;
  };
  await editor.openRawEditor('player');
  await settle();

  assert.equal(ui.textarea.value, XML, 'no divider when there is no JSON half');
  assert.equal(ui.save.disabled, false);

  calls.length = 0;
  ui.textarea.value = XML.replace('<id>alex</id>', '<id>alex</id>\n    <note>n</note>');
  ui.textarea.dispatch('input');
  ui.save.dispatch('click');
  await settle();

  const set = calls.find((c) => c.cmd === 'fable_player_raw_set');
  assert.ok(set, 'whole-text .player save (the pre-combined behavior)');
  assert.ok(set.args.xml.includes('<note>'));
  assert.ok(!calls.some((c) => c.cmd === 'fable_json_raw_set'), 'no session-state write without a JSON half');
});
