// =============================================================
// GAMES CHROME — auto-hiding OS chrome while a game is active.
//
// Per §7B "feels like launching an .exe": when a game opens, the OS
// top bar + dock slide away. A 4px top-edge mousemove peeks them
// back for 1.8s so the user can still reach the paw menu + clock.
//
// Body classes (styles.css):
//   body.fable-active       → chrome hidden (translated off-screen)
//   body.fable-chrome-peek  → chrome slides back temporarily
// =============================================================

let peekTimer = null;

export function activateChrome() {
  document.body.classList.add('fable-active');
  window.addEventListener('mousemove', onTopEdgeMove, { passive: true });
}

export function deactivateChrome() {
  document.body.classList.remove('fable-active', 'fable-chrome-peek');
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
