// =============================================================
// GAMES WUPI DRAWER — the in-world game master (Direction 3).
//
// A slide-out drawer (right edge, 420px) wired to chat_send. Because
// a game is active, the backend auto-routes through fable_command::
// classify (lib.rs). So the player asks Wupi in natural language:
//   "show me my inventory"   → QueryWorldState → Wupi narrates the typed
//                              inventory (equipment/belt/pack); the paperdoll
//                              HUD (left drawer) is the visual source of truth
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
//   NOTE (2026-08-07): the inventory panel was RETIRED — items now live
//   in the typed player_state.{equipment,belt,pack} model + are surfaced
//   via the paperdoll HUD. An inventory focus no longer opens a modal;
//   route_to_fable_query (lib.rs) renders a typed-model summary for Wupi
//   to narrate. The panel seam still serves the other foci (map/skills/
//   party/craft/codex).
//
// Channel event shapes (from chat_send, the manager-routing variants):
//   { type: 'chunk',            text }
//   { type: 'fable_state_query', focus, state }   → opens a panel (non-inventory)
//   { type: 'error',            message }
//   { type: 'done',             final_text, fable_manager?, reasoning? }
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import * as savesIo from './saves-io.js';
import { showApiTimeout, dismissApiTimeout } from '../../api-timeout-bubble.js';
import { resetTabRail } from './tab-rail.js';

let drawerEl = null;
let messagesEl = null;
let inputEl = null;
let activeBubble = null;   // streaming wupi bubble
let activeToolChip = null; // tool-call status chip (Phase 5), null between turns
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

// Chloe 2026-07-27: the WUPI_INTROS greeting array was REMOVED. The drawer
// now opens EMPTY — the user's first message is the first thing in the
// panel. No intro text explaining "edit NPCs / world" or similar; the
// panel is a clean slate until the user types.
const IDLE_PLACEHOLDER = 'Ask Wupi anything…';
const STOP_PLACEHOLDER = 'Press Enter to stop…';

export function initWupiDrawer(opts) {
  drawerEl = opts.drawerEl;
  messagesEl = opts.messagesEl;
  inputEl = opts.inputEl;
  panelManager = opts.panelManager || null;

  // Idempotent binding (Chloe 2026-07-23: the resource-isolation audit).
  // The drawer elements are REUSED across stage entries (built once in
  // buildStage), and wireStage calls initWupiDrawer every entry. A raw
  // addEventListener here would double-bind closeBtn/form/input on every
  // re-entry (anonymous arrows aren't deduped). Track the bound elements: if
  // they're the SAME instances as last time, skip re-binding (the listeners
  // are still attached from the first wireStage). Only re-bind if the
  // elements genuinely changed (they don't in this app). The send/stop
  // toggle button is GONE (2026-07-27) — Enter handles both intents now.
  const sameEls =
    boundCloseBtn === opts.closeBtn &&
    boundForm === opts.form &&
    boundInput === inputEl;
  if (!sameEls) {
    // Strip old listeners if elements changed (defensive — shouldn't happen).
    unbindDrawer();
    opts.closeBtn && opts.closeBtn.addEventListener('click', closeDrawer);
    boundCloseBtn = opts.closeBtn;
    opts.form && opts.form.addEventListener('submit', onFormSubmit);
    boundForm = opts.form;
    inputEl && inputEl.addEventListener('input', onInputGrow);
    inputEl && inputEl.addEventListener('keydown', onInputKeydown);
    boundInput = inputEl;
  }
}

// Named handlers (so they can be removed if the elements ever change) +
// refs to the currently-bound elements (so initWupiDrawer can no-op on a
// repeat call with the same reused elements).
let boundCloseBtn = null;
let boundForm = null;
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
// Enter does double duty (2026-07-27, no send button): on a non-empty field
// it submits the turn; on an EMPTY field while a turn streams it stops the
// generation. Shift+Enter is a literal newline. The input stays focusable +
// enabled during generation so the empty-Enter-to-stop works.
function onInputKeydown(e) {
  if (e.key !== 'Enter' || e.shiftKey) return;
  e.preventDefault();
  if (generating && !inputEl.value.trim()) {
    invoke('chat_stop').catch((err) => console.warn('[fable] wupi drawer stop failed', err));
    return;
  }
  boundForm && boundForm.requestSubmit();
}
function onInputGrow() { autoGrow(inputEl); }

// Remove the drawer's element listeners. Only needed defensively (same-
// element re-init is already short-circuited); called if initWupiDrawer is
// ever handed DIFFERENT elements than last time.
function unbindDrawer() {
  if (boundCloseBtn) { boundCloseBtn.removeEventListener('click', closeDrawer); boundCloseBtn = null; }
  if (boundForm) { boundForm.removeEventListener('submit', onFormSubmit); boundForm = null; }
  if (boundInput) {
    boundInput.removeEventListener('input', onInputGrow);
    boundInput.removeEventListener('keydown', onInputKeydown);
    boundInput = null;
  }
}

// Reflect generation state. The send/stop button is gone (2026-07-27); the
// input stays ENABLED so the empty-Enter-to-stop affordance works. The only
// feedback for a turn in flight is now the in-bubble streaming caret + the
// placeholder flipping to hint the stop gesture.
function setGenerating(on) {
  generating = on;
  if (inputEl) inputEl.placeholder = on ? STOP_PLACEHOLDER : IDLE_PLACEHOLDER;
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
  // No greeting/intro seeded (2026-07-27): the drawer opens empty so the
  // user's first message is the first thing in the panel. `greetingSeeded`
  // is retained as a no-op flag for reset-bookkeeping only.
  setTimeout(() => inputEl && inputEl.focus(), 320);
}

export function closeDrawer() {
  if (!drawerEl) return;
  drawerEl.classList.remove('open');
  open = false;
  // Collapse any open tab dropdown + deactivate its icon so nothing persists
  // behind the closed drawer (Chloe 2026-08-06: match the left drawer's reset-
  // on-close). Touches ONLY the tab rail — the Wupi chat history (messagesEl)
  // is a separate element + is never cleared here.
  resetTabRail();
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
    inputEl.placeholder = IDLE_PLACEHOLDER;
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

function finalizeBubble(bubble, finalText, reasoning) {
  // `reasoning` is unused post-2026-08-07 override (the player-facing reasoning
  // UI was removed; the local model still thinks internally).
  void reasoning;
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

  activeToolChip = null;  // reset per-turn (Phase 5)
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
    case 'tool_call':
      // Tool-calling agent loop (Phase 5): show a chip above the active
      // bubble indicating Wupi is executing a tool. The chip morphs on
      // tool_result. Lazily created so non-tool turns see nothing.
      if (!activeToolChip && messagesEl && activeBubble) {
        activeToolChip = document.createElement('div');
        activeToolChip.className = 'drawer-tool-chip running';
        messagesEl.insertBefore(activeToolChip, activeBubble);
      }
      if (activeToolChip) {
        activeToolChip.className = 'drawer-tool-chip running';
        activeToolChip.textContent = `🔧 ${msg.name || 'tool'}…`;
        messagesEl.scrollTop = messagesEl.scrollHeight;
      }
      break;
    case 'tool_result':
      if (activeToolChip) {
        activeToolChip.className = 'drawer-tool-chip ' + (msg.ok ? 'ok' : 'fail');
        const out = String(msg.output || '').slice(0, 120);
        activeToolChip.textContent = msg.ok
          ? `✓ ${msg.name || 'tool'}${out ? ': ' + out : ''}`
          : `✗ ${msg.name || 'tool'}: ${msg.output || 'failed'}`;
        messagesEl.scrollTop = messagesEl.scrollHeight;
      }
      break;
    case 'fable_state_query':
      onFableStateQuery(msg.focus, msg.state);
      break;
    case 'error':
      finalizeBubble(activeBubble, null);
      addWupiError(msg.message || 'Something went wrong.');
      activeBubble = null;
      activeToolChip = null;
      setGenerating(false);
      break;
    case 'api_timeout':
      // API TTFT deadline elapsed (no first token in 10s) — the turn is being
      // handled by Local fallback. Show the persistent top-center error
      // bubble (shared with the home chat path). The Local reply still streams
      // normally; this just signals the user why the voice changed.
      showApiTimeout();
      break;
    case 'done':
      // Successful turn — clear any lingering timeout bubble.
      dismissApiTimeout();
      if (activeBubble) {
        finalizeBubble(activeBubble, msg.final_text);
        activeBubble = null;
      }
      activeToolChip = null;
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
