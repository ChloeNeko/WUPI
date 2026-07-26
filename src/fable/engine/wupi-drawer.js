// =============================================================
// GAMES WUPI DRAWER — the in-world game master (Direction 3).
//
// A slide-out drawer (right edge, 420px) wired to chat_send. Because
// a game is active, the backend auto-routes through fable_command::
// classify (lib.rs). So the player asks Wupi in natural language:
//   "show me my inventory"   → QueryWorldState → panel opens
//   "make the barkeeper angry" → MutateWorldState → schema delta
//   "tell me a joke"         → NotACommand → normal Wupi chat
//
// THE PANEL SUMMONING SEAM (the innovation):
//   On a fable_state_query event, we route the returned WorldSchema
//   entities + the query focus to panels.manager.summon(), which
//   opens the matching read-view as a full-stage overlay. Wupi
//   confirms in her drawer ("Here's your inventory."). The player
//   never leaves immersion; panels are summoned, never persistent.
//
// Channel event shapes (from chat_send, the manager-routing variants):
//   { type: 'chunk',            text }
//   { type: 'fable_state_query', focus, state }   → opens a panel
//   { type: 'error',            message }
//   { type: 'done',             final_text, fable_manager?, reasoning? }
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import * as savesIo from './saves-io.js';

let drawerEl = null;
let messagesEl = null;
let inputEl = null;
let toggleBtn = null;      // ▶ send / ■ stop toggle
let activeBubble = null;   // streaming wupi bubble
let generating = false;    // tracks whether a chat_send turn is in flight
let open = false;
let locked = false;        // LOCK state: when true, mouseleave does NOT auto-close
let panelManager = null;   // injected: { summon(focus, entities) }
let greetingSeeded = false;
// Edge-lock visibility probe: set by stage.js via setEdgeLockProbe. When the
// edge lock is visible, auto-close on mouseleave is SUPPRESSED — the user is
// at the absolute edge trying to click the lock, and moving onto the lock
// (a separate element on top of the drawer) would otherwise fire mouseleave
// and yank the drawer + lock away before the click lands (the "phasing" bug).
let edgeLockVisible = () => false;

const WUPI_INTROS = [
  "Hi! I'm Wupi, your game master. Ask me anything — want me to show your inventory, change the weather, or nudge an NPC?",
  "Hey hey! Need a hand? I can edit your inventory, tweak the world, or do anything else you want me to.",
  "Wupi here! Want to see your skills, edit a memory, or make something happen? Just say the word.",
];

export function initWupiDrawer(opts) {
  drawerEl = opts.drawerEl;
  messagesEl = opts.messagesEl;
  inputEl = opts.inputEl;
  toggleBtn = opts.toggleBtn || null;
  panelManager = opts.panelManager || null;

  // Idempotent binding (Chloe 2026-07-23: the resource-isolation audit).
  // The drawer elements are REUSED across stage entries (built once in
  // buildStage), and wireStage calls initWupiDrawer every entry. A raw
  // addEventListener here would double-bind closeBtn/form/toggle/input
  // on every re-entry (anonymous arrows aren't deduped). Track the bound
  // elements: if they're the SAME instances as last time, skip re-binding
  // (the listeners are still attached from the first wireStage). Only
  // re-bind if the elements genuinely changed (they don't in this app).
  const sameEls =
    boundCloseBtn === opts.closeBtn &&
    boundForm === opts.form &&
    boundToggle === toggleBtn &&
    boundInput === inputEl;
  if (!sameEls) {
    // Strip old listeners if elements changed (defensive — shouldn't happen).
    unbindDrawer();
    opts.closeBtn && opts.closeBtn.addEventListener('click', closeDrawer);
    boundCloseBtn = opts.closeBtn;
    opts.form && opts.form.addEventListener('submit', onFormSubmit);
    boundForm = opts.form;
    toggleBtn && toggleBtn.addEventListener('click', onToggleClick);
    boundToggle = toggleBtn;
    inputEl && inputEl.addEventListener('input', onInputGrow);
    boundInput = inputEl;
  }
}

// Named handlers (so they can be removed if the elements ever change) +
// refs to the currently-bound elements (so initWupiDrawer can no-op on a
// repeat call with the same reused elements).
let boundCloseBtn = null;
let boundForm = null;
let boundToggle = null;
let boundInput = null;

function onFormSubmit(e) {
  e.preventDefault();
  if (generating) return;
  const text = inputEl.value.trim();
  if (!text) return;
  inputEl.value = '';
  inputEl.style.height = 'auto';
  sendWupiTurn(text);
}
function onToggleClick() {
  if (generating) {
    invoke('chat_stop').catch((e) => console.warn('[fable] wupi drawer stop failed', e));
  } else {
    boundForm && boundForm.requestSubmit();
  }
}
function onInputGrow() { autoGrow(inputEl); }

// Remove the drawer's element listeners. Only needed defensively (same-
// element re-init is already short-circuited); called if initWupiDrawer is
// ever handed DIFFERENT elements than last time.
function unbindDrawer() {
  if (boundCloseBtn) { boundCloseBtn.removeEventListener('click', closeDrawer); boundCloseBtn = null; }
  if (boundForm) { boundForm.removeEventListener('submit', onFormSubmit); boundForm = null; }
  if (boundToggle) { boundToggle.removeEventListener('click', onToggleClick); boundToggle = null; }
  if (boundInput) { boundInput.removeEventListener('input', onInputGrow); boundInput = null; }
}

// Flip the toggle button icon + disable the input while a turn streams.
function setGenerating(on) {
  generating = on;
  if (inputEl) inputEl.disabled = on;
  if (toggleBtn) {
    toggleBtn.textContent = on ? '■' : '▶';
    toggleBtn.classList.toggle('is-stop', on);
    toggleBtn.setAttribute('aria-label', on ? 'Stop' : 'Send');
  }
}

export function isGenerating() { return generating; }

export function isOpen() { return open; }

export function isLocked() { return locked; }

// Toggle the lock. When locked, the drawer stays open even when the mouse
// leaves (the auto-pull-in on mouseleave is suppressed). The lock bar's
// visual state (a filled vs hollow glyph) is driven by the .locked class
// on the drawer element. Returns the new locked state so the caller can
// update the bar glyph if needed.
export function toggleLock() {
  locked = !locked;
  if (drawerEl) drawerEl.classList.toggle('locked', locked);
  return locked;
}

export function openDrawer() {
  if (!drawerEl) return;
  drawerEl.classList.add('open');
  open = true;
  if (!greetingSeeded) {
    addWupiMsg(WUPI_INTROS[Math.floor(Math.random() * WUPI_INTROS.length)], 'wupi');
    greetingSeeded = true;
  }
  setTimeout(() => inputEl && inputEl.focus(), 320);
}

export function closeDrawer() {
  if (!drawerEl) return;
  drawerEl.classList.remove('open');
  open = false;
}

// Set the edge-lock visibility probe. stage.js calls this with a fn that
// returns true when THIS drawer's edge lock is currently visible (mouse at
// the absolute edge). See onDrawerMouseLeave for why this matters.
export function setEdgeLockProbe(probe) {
  edgeLockVisible = typeof probe === 'function' ? probe : () => false;
}

// Auto-pull-in: when the mouse fully exits the drawer, close it UNLESS:
//   - locked (the user clicked the lock to pin it open), OR
//   - generating (don't yank mid-stream), OR
//   - the edge lock is visible (the user is at the absolute edge, about to
//     click the lock — moving onto the lock fires this mouseleave, so we
//     suppress to let the click land. This is the fix for the "phasing"
//     bug where the drawer + lock vanished the moment you reached for it).
export function onDrawerMouseLeave() {
  if (locked) return;
  if (generating) return;
  if (edgeLockVisible()) return;
  closeDrawer();
}

// Hard reset of all module state (Chloe 2026-07-23: the resource-isolation
// audit, "reset every re-entry"). Called from teardownStage on stage exit so:
//   - a close mid-turn can't leave `generating` stuck true (would no-op the
//     next session's first send via the `if (generating) return` guard),
//   - a stale `activeBubble` reference can't leak into the next session,
//   - the drawer is forced closed (not left visually .open on the reused
//     element),
//   - the Wupi conversation + greeting are wiped so the next stage entry
//     shows a fresh greeting + empty transcript (true statelessness per
//     Chloe's "reset every re-entry" decision).
export function resetWupiDrawer() {
  generating = false;
  activeBubble = null;
  greetingSeeded = false;
  locked = false;
  if (drawerEl) drawerEl.classList.remove('locked');
  if (open && drawerEl) drawerEl.classList.remove('open');
  open = false;
  if (messagesEl) messagesEl.innerHTML = '';
  if (inputEl) {
    inputEl.value = '';
    inputEl.style.height = 'auto';
    inputEl.disabled = false;
  }
  if (toggleBtn) {
    toggleBtn.textContent = '▶';
    toggleBtn.classList.remove('is-stop');
    toggleBtn.setAttribute('aria-label', 'Send');
  }
}

function autoGrow(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, 120) + 'px';
}

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
function prose(s) { return esc(s).replace(/\n/g, '<br>'); }

function addWupiMsg(text, role) {
  const m = document.createElement('div');
  m.className = 'fable-wupi-msg ' + role;
  m.innerHTML = `<div class="fable-wupi-msg-body">${prose(text)}</div>`;
  messagesEl.appendChild(m);
  messagesEl.scrollTop = messagesEl.scrollHeight;
  return m;
}

function addUserMsg(text) { return addWupiMsg(text, 'user'); }

function startWupiBubble() {
  const m = document.createElement('div');
  m.className = 'fable-wupi-msg wupi streaming';
  m.innerHTML = `<div class="fable-wupi-msg-body"></div>`;
  messagesEl.appendChild(m);
  messagesEl.scrollTop = messagesEl.scrollHeight;
  return m;
}

function appendToBubble(bubble, text) {
  if (!bubble || !text) return;
  bubble._raw = (bubble._raw || '') + text;
  bubble.querySelector('.fable-wupi-msg-body').innerHTML = prose(bubble._raw);
  messagesEl.scrollTop = messagesEl.scrollHeight;
}

function finalizeBubble(bubble, finalText) {
  if (!bubble) return;
  bubble.classList.remove('streaming');
  if (finalText != null) bubble.querySelector('.fable-wupi-msg-body').innerHTML = prose(finalText);
  else if (bubble._raw != null) bubble.querySelector('.fable-wupi-msg-body').innerHTML = prose(bubble._raw);
}

function addWupiError(text) { return addWupiMsg(text, 'error'); }

// Send a message to Wupi (routes through chat_send → fable_command).
async function sendWupiTurn(text) {
  addUserMsg(text);

  const channel = new Channel();
  channel.onmessage = (msg) => handleEvent(msg, text);

  activeBubble = startWupiBubble();
  setGenerating(true);
  try {
    await invoke('chat_send', { text, onEvent: channel });
  } catch (err) {
    finalizeBubble(activeBubble, null);
    addWupiError(String(err));
    activeBubble = null;
    setGenerating(false);
  }
}

function handleEvent(msg, originalText) {
  if (!msg || typeof msg !== 'object') return;
  switch (msg.type) {
    case 'chunk':
      appendToBubble(activeBubble, msg.text);
      break;
    case 'fable_state_query':
      onFableStateQuery(msg.focus, msg.state);
      break;
    case 'error':
      finalizeBubble(activeBubble, null);
      addWupiError(msg.message || 'Something went wrong.');
      activeBubble = null;
      setGenerating(false);
      break;
    case 'done':
      if (activeBubble) {
        finalizeBubble(activeBubble, msg.final_text);
        activeBubble = null;
      }
      setGenerating(false);
      break;
  }
}

// THE PANEL SUMMONING SEAM.
// A fable_state_query carries the full WorldSchema JSON (state field) +
// a focus hint. Parse the schema + route by focus to open the right
// panel as a full-stage overlay.
function onFableStateQuery(focus, stateJson) {
  if (!panelManager) return;
  let schema = null;
  try {
    schema = typeof stateJson === 'string' ? JSON.parse(stateJson) : stateJson;
  } catch (_) {
    schema = null;
  }
  const entities = (schema && schema.entities) || {};
  panelManager.summon(focus || '', entities, schema || {});
}

// Expose save/load shortcuts the pause menu + drawer can call.
export { savesIo };
