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
import { resetTabRail } from './tab-rail.js';
import { wireLineLockedInput } from './input-lines.js';

let drawerEl = null;
let messagesEl = null;
let inputEl = null;
let activeBubble = null;   // streaming wupi bubble
let activeToolChip = null; // tool-call status chip (Phase 5), null between turns
let generating = false;    // tracks whether a chat_send turn is in flight
let open = false;
let locked = false;        // LOCK state: when true, mouseleave does NOT auto-close
// (2026-08-25 stuck-screen fix) The post-open input focus timer, OWNED so
// closeDrawer/resetWupiDrawer can cancel it. The bare setTimeout used to
// OUTLIVE any close that landed inside its 320ms window — a fast swipe-off
// right after the drawer opens, or the blur-path close stage.js's
// dismissStaleEdgeLocks runs on alt-tab. The focus then fired on the
// PARKED drawer's input (the closed drawer sits at translateX(100%), fully
// past the right edge), and the engine's focus-scroll-into-view scrolled
// the nearest scrollable ancestor — #fable, whose overflow:hidden still
// makes it a PROGRAMMATIC scroll container — hard right to reveal it.
// Nothing ever scrolled back: the whole stage sat shifted left with the
// parked drawer posing as "stuck open" while its JS state said CLOSED
// (so Esc + mouseleave had nothing to close), and the left-edge hover
// strip — the only way to summon the left drawer — sat off-screen. Only
// leaving the game rebuilt the DOM + reset the scroll. The LEFT drawer
// can never trigger this: it parks at translateX(-100%), and an LTR page
// cannot scroll to a negative scrollLeft.
let openFocusTimer = null;
let panelManager = null;   // injected: { summon(focus, entities) }
let greetingSeeded = false;
// (2026-08-25 lock redesign) The edgeLockVisible probe + onDrawerMouseLeave
// are GONE — stage.js now owns auto-close entirely, via a distance check on
// the stage mousemove (the pointer must clear the drawer's inner edge by
// DRAWER_CLOSE_GRACE_PX before an unlocked, non-generating drawer closes).
// The drawer modules expose only isOpen/isLocked/isGenerating for that.

// Chloe 2026-07-27: the WUPI_INTROS greeting array was REMOVED. The drawer
// now opens EMPTY — the user's first message is the first thing in the
// panel. No intro text explaining "edit NPCs / world" or similar; the
// panel is a clean slate until the user types.
const IDLE_PLACEHOLDER = 'Ask WUPI anything…';
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
    inputEl && inputEl.addEventListener('keydown', onInputKeydown);
    // Line-locked grow/scroll (2026-08-24): replaces the old scrollHeight
    // dance — 3 whole 22px lines max, one line per wheel click, no slivers.
    if (inputEl) unwireLineLock = wireLineLockedInput(inputEl, 3) || null;
    boundInput = inputEl;
  }
}

// Teardown for the line-lock wiring handed back by wireLineLockedInput.
let unwireLineLock = null;

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
  // Drive the line-lock grower's reset (height → 1 line, scroll → 0).
  inputEl.dispatchEvent(new Event('input', { bubbles: true }));
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
// Remove the drawer's element listeners. Only needed defensively (same-
// element re-init is already short-circuited); called if initWupiDrawer is
// ever handed DIFFERENT elements than last time.
function unbindDrawer() {
  if (boundCloseBtn) { boundCloseBtn.removeEventListener('click', closeDrawer); boundCloseBtn = null; }
  if (boundForm) { boundForm.removeEventListener('submit', onFormSubmit); boundForm = null; }
  if (unwireLineLock) { unwireLineLock(); unwireLineLock = null; }
  if (boundInput) {
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

// (2026-08-16 audit M8b) Stop an in-flight drawer chat turn. Called from
// teardownStage: a decode surviving the stage exit keeps the process-wide
// local-model turn lock, stalling the next game's first tracker turn by
// seconds. Safe no-op when nothing streams (chat_stop's own contract).
export function stopWupiTurn() {
  if (!generating) return;
  invoke('chat_stop').catch((err) => console.warn('[fable] wupi drawer stop failed', err));
}

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
  // (2026-08-25) The delayed focus is OWNED (closeDrawer cancels it) and
  // lands with preventScroll — focusing the drawer's input must NEVER let
  // the engine scroll anything to "reveal" it (see openFocusTimer's decl
  // for the stuck-screen bug this closes).
  if (openFocusTimer) clearTimeout(openFocusTimer);
  openFocusTimer = setTimeout(() => {
    openFocusTimer = null;
    inputEl && inputEl.focus({ preventScroll: true });
  }, 320);
}

export function closeDrawer() {
  if (!drawerEl) return;
  drawerEl.classList.remove('open');
  open = false;
  // (2026-08-25 v2) An explicit close resets the lock — lock means "stay
  // open", so once ANY path closes the drawer (Esc, the ✕ button), the tab
  // must fall back to its resting chevron instead of wearing a padlock on a
  // closed drawer. Auto-close never fires while locked, so this only
  // affects deliberate closes.
  if (locked) { locked = false; drawerEl.classList.remove('locked'); }
  // (2026-08-25) A close landing inside the 320ms focus window must kill
  // the pending focus: firing it on the parking drawer's off-screen input
  // is what scroll-stranded the whole stage (see openFocusTimer's decl).
  if (openFocusTimer) { clearTimeout(openFocusTimer); openFocusTimer = null; }
  // Collapse any open tab dropdown + deactivate its icon so nothing persists
  // behind the closed drawer (Chloe 2026-08-06: match the left drawer's reset-
  // on-close). Touches ONLY the tab rail — the Wupi chat history (messagesEl)
  // is a separate element + is never cleared here.
  resetTabRail();
  // (2026-08-25 Chloe) The Playground STAYS ACTIVE behind the closed drawer
  // — no resetPlayground() here anymore. The wand stays pressed, the strip +
  // domain panel persist; reopening the drawer shows them exactly as left.
  // Only the stage teardown (stage.js) collapses the Playground.
}

// (2026-08-25 lock redesign) setEdgeLockProbe + onDrawerMouseLeave were
// removed here: stage.js's distance-based auto-close (see the module-state
// note above) replaced the mouseleave trigger, and the probe it consulted
// only existed to keep the now-deleted invisible edge-lock bars from
// "phasing" the drawer away mid-reach. The visible side bars need no
// suppression — hovering one keeps the pointer inside the drawer's span.

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
  // (2026-08-25) A stage exit inside the 320ms focus window must not leave
  // the timer armed to focus a stale input after teardown.
  if (openFocusTimer) { clearTimeout(openFocusTimer); openFocusTimer = null; }
  if (drawerEl) drawerEl.classList.remove('locked');
  if (open && drawerEl) drawerEl.classList.remove('open');
  open = false;
  if (messagesEl) messagesEl.innerHTML = '';
  if (inputEl) {
    inputEl.value = '';
    // Drive the line-lock grower's reset instead of poking height directly.
    inputEl.dispatchEvent(new Event('input', { bubbles: true }));
    inputEl.placeholder = IDLE_PLACEHOLDER;
  }
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
    // (.finally backstop, 2026-08-16 audit fix — same contract as the stage
    // composer + narrator) A backend resolve WITHOUT a terminal event would
    // leave the drawer's `generating` latched: the input stays disabled
    // until stage exit. Every terminal path runs setGenerating(false), so
    // still-generating here means no terminal arrived — settle defensively.
    // (2026-08-17 E4B verification) Settle on a short grace timer, NOT
    // synchronously: the invoke resolve can beat the last queued channel
    // message to the page (the synchronous manager paths —
    // QueryWorldState/MutateWorldState — send chunk+done then return
    // immediately), and a synchronous settle nulled activeBubble before the
    // chunk landed, rendering the manager reply as an EMPTY bubble. A real
    // terminal event arrives within the grace window and clears generating
    // itself; only a genuinely event-less resolve runs the settle.
    setTimeout(() => {
      if (!generating) return;
      finalizeBubble(activeBubble, null);
      activeBubble = null;
      setGenerating(false);
    }, 150);
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
    case 'api_fallback':
      // (2026-08-24 hybrid chat) The API handoff failed and the local model
      // answered instead — note it on the turn (the voice change is audible).
      // One-shot chip, left in place like a tool chip; the fallback reply
      // streams into the bubble right after.
      if (activeBubble && messagesEl) {
        const chip = document.createElement('div');
        chip.className = 'drawer-tool-chip fail';
        chip.textContent = `⚠ ${msg.message || 'api unreachable — answered locally'}`;
        messagesEl.insertBefore(chip, activeBubble);
        messagesEl.scrollTop = messagesEl.scrollHeight;
      }
      break;
    case 'error':
      finalizeBubble(activeBubble, null);
      addWupiError(msg.message || 'Something went wrong.');
      activeBubble = null;
      activeToolChip = null;
      setGenerating(false);
      break;
    case 'done':
      if (activeBubble) {
        // (2026-08-24) A cancelled-before-any-text turn reverts fully on the
        // backend (done{cancelled:true}) — drop the empty bubble rather than
        // finalize it. Partial stops still finalize below.
        if (msg.cancelled && !activeBubble._raw) {
          activeBubble.remove();
        } else {
          finalizeBubble(activeBubble, msg.final_text);
        }
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
