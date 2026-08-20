// =============================================================
// GAMES CHROME — auto-hiding OS chrome while a game is active.
//
// Per §7B "feels like launching an .exe": when a game opens, the OS
// top bar + dock slide away and STAY away.
//
// (2026-08-20, Chloe) The top-edge chrome PEEK is RETIRED — the blurred
// glass top bar is completely removed from Fable. There is no hover zone,
// no peek class, no mousemove listener: while `body.fable-active` is set,
// the bar is translated off-screen with pointer-events disabled and
// nothing in Fable can summon it back. (PRISM keeps its own peek system —
// this file is the Fable twin only.)
//
// Body classes (fable.css):
//   body.fable-active → chrome hidden (translated off-screen, unclickable)
// =============================================================

export function activateChrome() {
  document.body.classList.add('fable-active');
}

export function deactivateChrome() {
  document.body.classList.remove('fable-active');
}
