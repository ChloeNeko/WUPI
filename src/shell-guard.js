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

// ── The global capture-phase click/pointerdown guard ────────────────
// One listener at document, capture phase, swallows click + pointerdown
// while a shell transition is in flight (shellBusy). Self-registers on
// module load — `script.js` is loaded as type="module", so the DOM is
// already parsed by the time this runs and the listener is armed before
// any per-button handler can fire. Calling `initShellGuard()` is also
// exported for explicitness, but importing the module is sufficient.
export function initShellGuard() {
  ['click', 'pointerdown'].forEach((evt) => {
    document.addEventListener(evt, (e) => {
      if (!shellBusy) return;
      e.stopImmediatePropagation();
      e.preventDefault();
    }, { capture: true });
  });
}

// Auto-register on import. The guard is a pure no-op when shellBusy is
// false (the common case), so there's no cost to mounting it eagerly.
initShellGuard();
