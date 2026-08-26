// Wupi-drawer send-loop regression tests (2026-08-17 E4B verification).
//
// The `.finally` backstop in sendWupiTurn settled the drawer synchronously
// when `chat_send` resolved — but the invoke resolve can beat the last
// QUEUED channel message to the page (observed live: the synchronous
// manager paths — QueryWorldState/MutateWorldState — send chunk+done then
// return, and the events land ~1ms AFTER the resolve). The synchronous
// settle nulled activeBubble before the chunk landed, rendering the
// manager reply as an EMPTY bubble. The fix settles on a short grace timer.
//
// These tests drive the REAL module (no reimplementation): a minimal DOM +
// Tauri-internals stub, the exact event ordering observed in the live app,
// and the public init/submit path (Enter in the input → form submit →
// sendWupiTurn).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { registerHooks } from 'node:module';

// The drawer's import chain pulls fx.css (and friends) — Node can't load
// CSS; stub every .css as an empty module.
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
    __set: () => set,
    __clear: () => set.clear(),
  };
}
function makeEl(tag) {
  const listeners = new Map();
  const el = {
    tagName: (tag || 'div').toUpperCase(),
    children: [],
    style: {},
    dataset: {},
    placeholder: '',
    scrollTop: 0,
    scrollHeight: 100,
    listeners,
    classList: null,
    __body: null,
    __val: '',
    appendChild(c) { el.children.push(c); return c; },
    addEventListener(t, fn) { listeners.set(t, fn); },
    removeEventListener(t) { listeners.delete(t); },
    dispatch(t, ev) { const fn = listeners.get(t); if (fn) fn(ev); return !!fn; },
    // Real-DOM shims for the 2026-08-24 line-lock wiring: onFormSubmit resets
    // the grower via dispatchEvent(new Event('input')), and input-lines'
    // unbind removes its measurement mirror.
    dispatchEvent(ev) { return el.dispatch(ev.type, ev); },
    remove() {},
    focus() {},
    requestSubmit() { el.dispatch('submit', { preventDefault() {} }); },
    getBoundingClientRect: () => ({ x: 0, y: 0, width: 10, height: 10, left: 0, top: 0 }),
    querySelector(sel) {
      if (sel === '.fable-wupi-msg-body') return el.__body || (el.__body = makeEl('div'));
      // The tab-rail build path (needed so closeDrawer's resetTabRail has a
      // dropdown element to hide — see the stuck-screen tests below).
      if (sel === '.fable-tab-rail') return el.__rail || (el.__rail = makeEl('div'));
      if (sel === '[data-tab-dropdown]') return el.__drop || (el.__drop = makeEl('div'));
      return null;
    },
    querySelectorAll() { return []; },
  };
  el.classList = makeClassList(el);
  // className is a live view of classList (like the real DOM).
  Object.defineProperty(el, 'className', {
    get() { return [...el.classList.__set()].join(' '); },
    set(v) {
      el.classList.__clear();
      String(v).split(/\s+/).filter(Boolean).forEach((c) => el.classList.add(c));
    },
  });
  Object.defineProperty(el, 'value', {
    get() { return el.__val; },
    set(v) { el.__val = v; },
  });
  // innerHTML as a plain property (some paths set it to clear children).
  let inner = '';
  Object.defineProperty(el, 'innerHTML', {
    get() { return inner; },
    set(v) { inner = v; if (v === '') el.children.length = 0; },
  });
  return el;
}

// ---- Tauri internals stub --------------------------------------------------
const callbacks = new Map();
let nextCbId = 0;
globalThis.window = globalThis;
globalThis.document = { createElement: (tag) => makeEl(tag), querySelector: () => null };
// input-lines.js (the 2026-08-24 line-locked grower, wired by initWupiDrawer)
// measures the textarea via getComputedStyle + parks an off-screen mirror on
// document.body at wire time — Node has neither. Flat metrics + a body stub
// keep the wiring alive without exercising grow math (these tests drive the
// send loop, not the grower).
globalThis.getComputedStyle = () => ({
  lineHeight: '22px',
  fontSize: '15px',
  font: '15px system-ui',
  letterSpacing: 'normal',
  paddingLeft: '0px',
  paddingRight: '0px',
  getPropertyValue: () => '',
});
globalThis.document.body = makeEl('body');
window.__TAURI_INTERNALS__ = {
  transformCallback(cb) { const id = ++nextCbId; callbacks.set(id, cb); return id; },
  unregisterCallback(id) { callbacks.delete(id); },
  invoke: async () => ({}), // replaced per test
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Deliver one ordered channel message to a live Channel instance.
function channelSend(channel, message, index) {
  const cb = callbacks.get(channel.id);
  assert.ok(cb, 'channel callback registered');
  cb({ index, message });
}

const drawer = await import('../src/fable/engine/wupi-drawer.js');
const tabRail = await import('../src/fable/engine/tab-rail.js');
// closeDrawer resets the tab rail (hide dropdown, clear active tab); give the
// rail module its elements once so that chain is exercisable under the stub.
tabRail.buildTabRail();

function freshDrawer(panelManager) {
  const messagesEl = makeEl('div');
  const inputEl = makeEl('textarea');
  const form = makeEl('form');
  const drawerEl = makeEl('aside');
  const closeBtn = makeEl('button');
  drawer.initWupiDrawer({ drawerEl, messagesEl, inputEl, form, closeBtn, panelManager: panelManager || { summon() {} } });
  return { messagesEl, inputEl, form };
}

function submitTurn(inputEl, text) {
  inputEl.value = text;
  inputEl.dispatch('keydown', { key: 'Enter', shiftKey: false, preventDefault() {} });
}

function bubbles(messagesEl) {
  return messagesEl.children.map((c) => ({
    cls: c.__cls || '',
    body: c.querySelector('.fable-wupi-msg-body').innerHTML,
  }));
}

// The synchronous manager path ordering observed live (2026-08-17): the
// command resolves, THEN the queued chunk/done events land ~1ms later.
test('manager-path race: reply renders when events land just after the resolve', async () => {
  const REPLY = 'Here is the pack summary.';
  window.__TAURI_INTERNALS__.invoke = async (cmd, args) => {
    if (cmd !== 'chat_send') return {};
    const ch = args.onEvent;
    let idx = 0;
    setTimeout(() => {
      channelSend(ch, { type: 'fable_state_query', focus: 'inventory', state: '{}' }, idx++);
      channelSend(ch, { type: 'chunk', text: REPLY }, idx++);
      channelSend(ch, { type: 'done', final_text: REPLY, reasoning: '' }, idx++);
    }, 1);
  };
  const { messagesEl, inputEl } = freshDrawer();
  submitTurn(inputEl, 'show me my inventory');
  await sleep(400); // grace (150ms) + margin
  const bs = bubbles(messagesEl);
  assert.equal(bs.length, 2, 'user + assistant bubble');
  assert.ok(bs[1].body.includes('Here is the pack summary'), `assistant bubble carries the reply, got: ${bs[1].body}`);
  assert.ok(!bs[1].cls.includes('streaming'), 'assistant bubble finalized');
  assert.equal(drawer.isGenerating(), false, 'generating unlatched');
  assert.ok(!bs.some((b) => b.cls.includes('error')), 'no error bubble');
});

// A genuinely event-less resolve must STILL settle (the backstop exists for
// this) — the grace timer must not leave `generating` latched forever.
test('event-less resolve: grace backstop still settles the drawer', async () => {
  window.__TAURI_INTERNALS__.invoke = async () => ({});
  const { messagesEl, inputEl } = freshDrawer();
  submitTurn(inputEl, 'hello?');
  await sleep(400);
  assert.equal(drawer.isGenerating(), false, 'generating unlatched by the backstop');
  const bs = bubbles(messagesEl);
  assert.equal(bs.length, 2, 'user + assistant bubble');
  assert.ok(!bs[1].cls.includes('streaming'), 'bubble finalized (no stuck caret)');
});

// Non-regression: a normal streaming turn (chunks + done all BEFORE the
// resolve, like the local-model path) still renders in full.
test('normal streaming turn still renders in full', async () => {
  window.__TAURI_INTERNALS__.invoke = async (cmd, args) => {
    if (cmd !== 'chat_send') return {};
    const ch = args.onEvent;
    await sleep(30); channelSend(ch, { type: 'chunk', text: 'Part one. ' }, 0);
    await sleep(30); channelSend(ch, { type: 'chunk', text: 'Part two.' }, 1);
    await sleep(30); channelSend(ch, { type: 'done', final_text: 'Part one. Part two.', reasoning: '' }, 2);
  };
  const { messagesEl, inputEl } = freshDrawer();
  submitTurn(inputEl, 'tell me something');
  await sleep(500);
  const bs = bubbles(messagesEl);
  assert.equal(bs.length, 2, 'user + assistant bubble');
  assert.ok(bs[1].body.includes('Part one.') && bs[1].body.includes('Part two.'), 'streamed text rendered');
  assert.equal(drawer.isGenerating(), false, 'generating unlatched');
});

// ---- The stuck-screen bug (2026-08-25) -------------------------------------
// openDrawer used to fire a bare 320ms setTimeout(inputEl.focus()). Any close
// landing inside that window (a fast swipe-off, or the blur-path close —
// stage.js's closeUnlockedDrawers, then named dismissStaleEdgeLocks) let the focus fire on the PARKED
// drawer's input — off the right edge — and the engine's focus-scroll-into-
// view scrolled the app root right to reveal it, permanently shifting the
// whole stage. Regression tests: the close cancels the pending focus, and the
// focus that DOES land carries preventScroll so it can never scroll anything.
test('close inside the 320ms window cancels the pending input focus', async () => {
  window.__TAURI_INTERNALS__.invoke = async () => ({});
  const { inputEl } = freshDrawer();
  let focused = 0;
  inputEl.focus = () => { focused += 1; };
  drawer.openDrawer();
  drawer.closeDrawer();              // the fast swipe-off / alt-tab blur close
  await sleep(420);                  // past the whole focus window
  assert.equal(focused, 0, 'the orphaned focus never fired on the parked drawer');
  assert.equal(drawer.isOpen(), false, 'drawer still closed');
});

test('the post-open focus lands with preventScroll (can never scroll-reveal the drawer)', async () => {
  window.__TAURI_INTERNALS__.invoke = async () => ({});
  const { inputEl } = freshDrawer();
  let focusOpts = null;
  inputEl.focus = (opts) => { focusOpts = opts; };
  drawer.openDrawer();
  await sleep(380);                  // let the post-slide focus fire
  assert.ok(focusOpts, 'focus fired after the slide-in');
  assert.equal(focusOpts.preventScroll, true, 'focus must not scroll anything to reveal the input');
  drawer.closeDrawer();              // leave the module closed for later tests
});

// (2026-08-25 v2) Lock means "stay open": an EXPLICIT close (Esc / the ✕
// button) must drop the lock so the drawer tab never wears its padlock on a
// closed drawer.
test('an explicit close resets the lock', async () => {
  window.__TAURI_INTERNALS__.invoke = async () => ({});
  const { inputEl } = freshDrawer();
  inputEl.focus = () => {};
  drawer.openDrawer();
  drawer.toggleLock();
  assert.equal(drawer.isLocked(), true, 'pinned');
  drawer.closeDrawer();
  assert.equal(drawer.isOpen(), false, 'closed');
  assert.equal(drawer.isLocked(), false, 'lock dropped with the explicit close');
});
