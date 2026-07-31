// =============================================================
// PRISM CHROME — auto-hiding OS chrome while Prism is active.
//
// Mirrors fable/engine/chrome.js but namespaced to `body.prism-active`
// (Prism's own theme flag, not Fable's). When Prism opens, the OS top bar
// + dock slide away; a 4px top-edge mousemove peeks them back for 1.8s so
// the user can still reach the paw menu + clock.
//
// Body classes (styles.css — the OS chrome rules key off `fable-active`,
// so Prism reuses the SAME class to drive the same hide/show. This is
// intentional: the chrome-hide is an OS-level "a full-screen app owns the
// screen" signal, shared by Fable + Prism; only the per-app theming
// classes differ.):
//   body.fable-active       → chrome hidden (translated off-screen)
//   body.fable-chrome-peek  → chrome slides back temporarily
//   body.prism-active       → Prism's own theme flag (CSS namespacing)
// =============================================================

let peekTimer = null;

export function activateChrome() {
  // Both classes: `fable-active` drives the shared chrome-hide in styles.css;
  // `prism-active` is Prism's own namespacing flag for prism.css rules.
  document.body.classList.add('fable-active', 'prism-active');
  window.addEventListener('mousemove', onTopEdgeMove, { passive: true });
}

export function deactivateChrome() {
  document.body.classList.remove('fable-active', 'prism-active', 'fable-chrome-peek');
  window.removeEventListener('mousemove', onTopEdgeMove);
  if (peekTimer) { clearTimeout(peekTimer); peekTimer = null; }
}

function onTopEdgeMove(e) {
  if (e.clientY > 4) return;
  document.body.classList.add('fable-chrome-peek');
  if (peekTimer) clearTimeout(peekTimer);
  peekTimer = setTimeout(() => {
    document.body.classList.remove('fable-chrome-peek');
  }, 1800);
}
