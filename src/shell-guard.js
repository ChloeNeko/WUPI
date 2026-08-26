// =============================================================
// SHELL GUARD — the OS-wide rapid-click / double-launch guard.
//
// A generalization of Fable's `flowBusy` capture-phase pattern
// (fable.js:141-183, 1697-1712) to cover the ENTIRE WUPI shell —
// the top-bar, dock, home grid, boot overlays, and both full-screen
// apps (Fable + PRISM). Fable's flag only guards its own in-app
// transitions (burn/seed/resume); nothing guarded the shell-chrome
// entry points (home-grid tile launch, dock toggle, paw-menu restart)
// — a rapid double-click there could fire the launch/restart path
// twice before the first one's async work landed. This closes that
// gap with the same proven technique, lifted one level higher.
//
// DESIGN (mirrors Fable's flowBusy exactly, renamed to avoid
// confusion with Fable's flag):
//   • A single module-level `shellBusy` flag.
//   • A capture-phase listener on `document` for BOTH `click` and
//     `pointerdown`. Capture phase is load-bearing: it intercepts
//     the event BEFORE any per-button listener can run, so no
//     handler needs to know about the guard. `pointerdown` is covered
//     too so a fast double-tap doesn't even start a press animation
//     on the second click. While `shellBusy` is true the listener
//     calls stopImmediatePropagation() + preventDefault(); normal
//     clicks (the vast majority) pass through completely untouched.
//   • `withShellBusy(task)` wraps an async/completion-based shell
//     transition: sets the flag, runs the task, clears on
//     resolve/reject. `setShellBusy(false)` for completion-based paths.
//   • A 12s safety-timeout auto-clear so a forgotten clear can never
//     dead-lock the UI (a recoverable bug, not a permanent freeze —
//     no transition should ever run this long).
//
// WHY `document`, not a root div: the OS shell renders directly into
// `<body>` — there is no single root container (wupi.html:20). The
// top-bar, dock, home grid, boot overlays, and the `.app-window`
// siblings (#chat/#api/#profile/#apps/#fable/#prism) are all direct
// children of <body>. `document` is the only target that uniformly
// covers all of them in one listener. The existing bubble-phase
// `document` listeners in script.js (dropdown-dismiss click, Esc-
// keydown) fire AFTER this capture listener, and only get swallowed
// when shellBusy is true — so normal dropdown/keyboard behavior is
// unaffected.
//
// RELATIONSHIP TO FABLE'S flowBusy: the two flags are independent
// and complementary. shellBusy covers the shell-launch phase (tile
// click → app open, dock toggle, restart); Fable's flowBusy covers
// in-Fable transitions (burn/seed/resume). A click is in one phase
// or the other, not both, so there's no conflict.
// =============================================================

// Hard ceiling: no shell transition should ever run this long. If the
// flag is still set after this, force-clear it (a forgotten clear is a
// recoverable bug, a permanent dead-lock is not). 12s mirrors Fable's
// FLOW_BUSY_SAFETY_MS — covers the longest app-launch + fog-transition
// chains.
const SHELL_BUSY_SAFETY_MS = 12000;

let shellBusy = false;
let shellBusyTimer = null;

export function setShellBusy(on) {
  shellBusy = on;
  if (shellBusyTimer) { clearTimeout(shellBusyTimer); shellBusyTimer = null; }
  if (on) {
    shellBusyTimer = setTimeout(() => {
      shellBusy = false;
      shellBusyTimer = null;
    }, SHELL_BUSY_SAFETY_MS);
  }
}

export function isShellBusy() {
  return shellBusy;
}

// Wrap an async/completion-based shell transition in the busy flag.
// `task` may return a Promise (resolved/rejected → clear) or nothing
// (caller clears manually via setShellBusy(false)). Drops the second
// click during the wrap before it reaches any handler.
export function withShellBusy(task) {
  if (shellBusy) return;        // already mid-transition: drop the second click
  setShellBusy(true);
  let ret;
  try {
    ret = task();
  } catch (e) {
    setShellBusy(false);
    throw e;
  }
  if (ret && typeof ret.then === 'function') {
    ret.then(() => setShellBusy(false), () => setShellBusy(false));
  }
  return ret;
}

// ── The global capture-phase click/pointerdown/keydown guard ─────────
// One listener at document, capture phase, swallows click + pointerdown
// while a shell transition is in flight (shellBusy). Self-registers on
// module load — `script.js` is loaded as type="module", so the DOM is
// already parsed by the time this runs and the listener is armed before
// any per-button handler can fire. Calling `initShellGuard()` is also
// exported for explicitness, but importing the module is sufficient.
//
// (#63) keydown is covered too: Esc (close topmost window) + Enter (send
// chat) fired while `shellBusy` — the click/pointerdown arms couldn't see
// keyboard activation, so Esc could dismiss the home grid mid-Fable-fog
// and Enter could double-send. Same capture + swallow discipline; the
// 12s safety timeout bounds any swallowed-key window.
//
// (2026-08-20, Chloe ruling) The WebView's native right-click menu
// (Refresh / Save As / Print / Send tab to your device) + the reload keys
// (Ctrl+R / Cmd+R / F5, incl. the Shift hard-reload variant) are disabled
// APP-WIDE, unconditionally — WUPI is a kiosk-style shell whose whole
// state lives in the SPA + the Rust core; a manual refresh drops the
// frontend state out from under a live engine. There is ONE document
// (wupi.html bundles all three surfaces — shell, Fable, PRISM), so one
// document-level listener covers everything. These run BEFORE the
// shellBusy gate: they block in every state, busy or not.
//
// (2026-08-23, Chloe ruling) THE ONE SANCTIONED RIGHT-CLICK SURFACE: the
// spellchecker's custom context menu (engine/spellcheck.js — correction
// candidates + the spellcheck toggle) on written inputs in TWO zones:
// every text entry inside #fable, and the OS-home Wupi chat input in
// #chat. The NATIVE menu stays dead everywhere, INCLUDING there — the
// guard still calls preventDefault() unconditionally. What changes is
// propagation only: a registered pass-through predicate may let the
// event reach the spellchecker's own listener (this guard's capture
// listener would otherwise stopImmediatePropagation it into the void).
// The predicate lives in the spellcheck module — this file stays
// surface-agnostic.
function isReloadKey(e) {
  if (e.key === 'F5') return true;
  return (e.ctrlKey || e.metaKey) && !e.altKey && (e.key === 'r' || e.key === 'R');
}

let contextMenuPassThrough = null;

// Register the single consumer allowed to see contextmenu events (the
// native menu remains blocked for it too). Pass null to revoke.
export function setContextMenuPassThrough(fn) {
  contextMenuPassThrough = typeof fn === 'function' ? fn : null;
}

export function initShellGuard() {
  document.addEventListener('contextmenu', (e) => {
    // Native menu: dead everywhere, no exceptions.
    e.preventDefault();
    // Propagation: swallowed unless the sanctioned surface claims it.
    if (contextMenuPassThrough && contextMenuPassThrough(e)) return;
    e.stopImmediatePropagation();
  }, { capture: true });
  ['click', 'pointerdown', 'keydown'].forEach((evt) => {
    document.addEventListener(evt, (e) => {
      if (evt === 'keydown' && isReloadKey(e)) {
        e.preventDefault();
        e.stopImmediatePropagation();
        return;
      }
      if (!shellBusy) return;
      e.stopImmediatePropagation();
      e.preventDefault();
    }, { capture: true });
  });
}

// Auto-register on import. The guard is a pure no-op when shellBusy is
// false (the common case), so there's no cost to mounting it eagerly.
// (The `document` probe keeps the module importable under plain Node —
// tests/spellcheck.test.mjs reaches this file through engine/spellcheck.js.
// In the browser the guard arms exactly as before.)
if (typeof document !== 'undefined') initShellGuard();
