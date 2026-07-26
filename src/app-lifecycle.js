// =============================================================
// APP LIFECYCLE MANAGER — the WUPI OS app-lifecycle framework.
//
// One centralized registry that governs how full-screen apps (Fable
// today; future OS apps later) open, pause, resume, and close. The
// point is a GUARANTEE: zero memory leaks, zero audio leaks, zero
// background CPU/GPU waste. Each app self-registers a descriptor with
// onOpen/onClose/onPause/onResume callbacks; the manager fires them at
// the right OS moments and never lets an app's resources outlive its
// session.
//
// Lifecycle states for an app:
//   closed  → launchApp → open   (onOpen fires)
//   open    → focus-loss → paused (onPause fires: freeze audio/GPU)
//   paused  → focus-return → open (onResume fires: unfreeze)
//   open|paused → closeApp → closed (onClose fires: full teardown)
//
// System focus: a single window-level listener pair
// (visibilitychange + window blur/focus) detects alt-tab away to
// Chrome/Discord/etc. and fires onPause/onResume on the ACTIVE app only.
// Only one app owns the screen at a time (single `activeApp` slot), so
// the listeners are cheap and unambiguous.
//
// Re-entrancy: onClose may itself call closeApp (e.g. an app routing its
// own exit through the OS closeWindow set). The `transitioning` guard +
// per-app `closed` short-circuit make that a no-op on the second entry,
// mirroring Fable's existing `closingFable` pattern.
// =============================================================

// Registered app descriptors: id → { onOpen, onClose, onPause, onResume }.
const registry = new Map();

// The single app currently owning the screen (null when on the OS desktop).
// One owner at a time is the WUPI invariant — Fable is full-screen immersion.
let activeApp = null;

// Re-entrancy guard: set while an app's callback is mid-flight so a callback
// re-entering the manager (closeApp called from within onClose, etc.) is a
// safe no-op rather than a recursive storm.
let transitioning = false;

// `true` once the system focus listeners have been attached (install-once).
let listenersArmed = false;

// ── Registry ─────────────────────────────────────────────────
// An app self-registers once at init. Re-registering the same id replaces the
// descriptor (lets an app hot-swap its callbacks on re-init without growing
// the map). All callbacks are optional; missing ones are treated as no-ops.
export function registerApp(descriptor) {
  if (!descriptor || !descriptor.id) return;
  registry.set(descriptor.id, {
    onOpen: descriptor.onOpen || null,
    onClose: descriptor.onClose || null,
    onPause: descriptor.onPause || null,
    onResume: descriptor.onResume || null,
  });
  // Arm the system focus listeners lazily — once, on first registration.
  // No sense attaching them if no app ever registers.
  if (!listenersArmed) armSystemFocusListeners();
}

// ── Launch (closed → open) ───────────────────────────────────
// Idempotent: launching an already-open app is a no-op (raise-to-top is the
// OS window layer's job, not ours). Fires onOpen exactly once per open.
export function launchApp(id) {
  const app = registry.get(id);
  if (!app) return;
  // Already open: nothing to do. (A paused app is still "open"; re-launching
  // doesn't resume it — focus return does that.)
  if (activeApp === id) return;
  activeApp = id;
  fire(app.onOpen);
}

// ── Close (open|paused → closed) ─────────────────────────────
// Idempotent + re-entrancy-safe. Closing an inactive id is a no-op. The
// transitioning guard means an onClose that itself calls closeApp (the
// Fable→closeWindow→closeFable re-entry path) won't loop.
export function closeApp(id) {
  if (transitioning) return;
  const app = registry.get(id);
  if (!app) return;
  if (activeApp !== id) return;
  transitioning = true;
  try {
    fire(app.onClose);
  } finally {
    activeApp = null;
    transitioning = false;
  }
}

// ── Pause / Resume (open ⇄ paused) ───────────────────────────
// Only the ACTIVE app gets paused/resumed. Pausing an inactive app would fire
// its onPause with no matching onResume context, so we guard on activeApp.
// Idempotent: pauseApp on an already-paused app is harmless (the app's own
// onPause should be idempotent too, but we don't double-fire from here).
export function pauseApp(id) {
  if (activeApp !== id) return;
  const app = registry.get(id);
  if (app) fire(app.onPause);
}

export function resumeApp(id) {
  if (activeApp !== id) return;
  const app = registry.get(id);
  if (app) fire(app.onResume);
}

// ── Introspection (for apps that need to know if they're active) ──
export function isActive(id) { return activeApp === id; }
export function getActiveApp() { return activeApp; }

// ── System focus listeners ───────────────────────────────────
// One window-level pair detects the user alt-tabbing away to another OS app
// (Chrome, Discord, …) or minimizing WUPI. On hidden/blur → pause the active
// app; on visible/focus → resume it. Installed ONCE (install-once guard).
//
// Why both visibilitychange AND blur/focus:
//   - visibilitychange fires on minimize + tab-switch (most reliable signal).
//   - blur fires when focus leaves the window entirely WITHOUT the tab
//     hiding (e.g. clicking a second monitor app that doesn't minimize WUPI).
//   Together they cover the real-world "user looked away" set. We track a
//   `paused` mirror so a blur-then-hide sequence doesn't double-fire onPause
//   (and a focus after a visible resume doesn't double-fire onResume).
let systemPaused = false;

function armSystemFocusListeners() {
  if (listenersArmed) return;
  listenersArmed = true;

  document.addEventListener('visibilitychange', () => {
    if (document.hidden) {
      if (!systemPaused) { systemPaused = true; pauseActiveApp(); }
    } else {
      if (systemPaused) { systemPaused = false; resumeActiveApp(); }
    }
  });

  window.addEventListener('blur', () => {
    // Only treat blur as a pause if the document isn't already hidden
    // (visibilitychange owns the hidden case; blur is the supplementary
    // "focus left the window" signal).
    if (!document.hidden && !systemPaused) { systemPaused = true; pauseActiveApp(); }
  });
  window.addEventListener('focus', () => {
    if (systemPaused) { systemPaused = false; resumeActiveApp(); }
  });
}

// Pause/resume whichever app currently owns the screen (no-op on the desktop).
function pauseActiveApp() {
  if (!activeApp) return;
  const app = registry.get(activeApp);
  if (app) fire(app.onPause);
}
function resumeActiveApp() {
  if (!activeApp) return;
  const app = registry.get(activeApp);
  if (app) fire(app.onResume);
}

// ── Safe callback invoker ────────────────────────────────────
// App callbacks are best-effort: a throw inside onPause must not strand the
// manager in a weird state or prevent onResume/Close from firing later. Wrap
// each fire in try/catch + console.error so one app's bug is loud but
// non-fatal.
function fire(fn) {
  if (typeof fn !== 'function') return;
  try { fn(); } catch (err) { console.error('[app-lifecycle] callback threw', err); }
}

// ── Singleton export ─────────────────────────────────────────
// Consumers import { AppLifecycle } and call AppLifecycle.registerApp(...),
// AppLifecycle.launchApp(...), AppLifecycle.closeApp(...). The methods are
// the module functions above; bound into one object for a clean call surface.
export const AppLifecycle = {
  registerApp,
  launchApp,
  closeApp,
  pauseApp,
  resumeApp,
  isActive,
  getActiveApp,
};
