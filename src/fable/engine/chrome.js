// =============================================================
// GAMES CHROME — auto-hiding OS chrome while a game is active.
//
// Per §7B "feels like launching an .exe": when a game opens, the OS
// top bar + dock slide away. The top bar re-peeks ONLY when the cursor
// touches the absolute top edge of the screen (CHROME_PEEK_OPEN_PX) so
// the user can still reach the paw menu + clock without the bar
// shadowing the game on every casual pass near the top.
//
// (2026-08-20 restored — the peek had been retired earlier that day and
// Chloe flagged its absence as a bug; this is again the exact twin of
// prism/engine/chrome.js.)
//
// Asymmetric hysteresis (Chloe 2026-08-14): OPEN and CLOSE use
// DIFFERENT thresholds — open demands the very top edge; once peeked,
// the bar stays up while clientY is within the top CHROME_PEEK_RATIO
// of the viewport height (default 20%) — the forgive zone ("leave 20%
// room before closing"). That comfortably covers the 48px bar + clock,
// and the paw dropdown (which drops from top:56px) as long as its
// content fits under the band. Computed against window.innerHeight so
// it scales to any display.
//
// Body classes (fable.css):
//   body.fable-active       → chrome hidden (translated off-screen)
//   body.fable-chrome-peek  → chrome slides back temporarily
// =============================================================

// Fraction of the viewport height, measured from the top, within which the
// chrome stays peeked. 0.2 == top 20% (216px on 1080p). Covers the 48px bar,
// the clock, and the paw start-menu dropdown (no max-height; content-sized).
// CLOSE-SIDE ONLY (the forgive zone): it keeps an ALREADY-PEEKED chrome up
// while the cursor lingers in the top band. It does NOT open the chrome —
// see CHROME_PEEK_OPEN_PX (Chloe 2026-08-14: one ratio used to drive BOTH
// directions, so merely entering the top 20% triggered the peek; the open
// side now demands the absolute top edge).
const CHROME_PEEK_RATIO = 0.2;
// How close (px) to the ABSOLUTE top edge the cursor must get for the
// HIDDEN chrome to peek at all. 4px == "all the way to the top edge" — a
// deliberate screen-edge poke, not a casual drift through the top band.
const CHROME_PEEK_OPEN_PX = 4;
// Grace period after the cursor leaves the peek zone before the chrome
// collapses. The timer is cleared on every move within the zone, so the bar
// only slides away once the cursor has left AND this delay has elapsed.
const CHROME_PEEK_GRACE_MS = 600;

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
  // Hysteresis (Chloe 2026-08-14): the OPEN and CLOSE thresholds are
  // DELIBERATELY different so a casual drift through the top 20% of the
  // screen no longer summons the chrome:
  //   HIDDEN  → peeks ONLY when the cursor touches the absolute top edge
  //             (clientY <= CHROME_PEEK_OPEN_PX) — "all the way to the top".
  //   PEEKED  → the wide CHROME_PEEK_RATIO forgive zone keeps it up ("leave
  //             20% room before closing"); leaving that band arms the
  //             CHROME_PEEK_GRACE_MS collapse as before.
  // The class check is instantaneous (not transition-dependent), so the
  // state flips the moment the peek class lands — no race with the slide.
  const peeked = document.body.classList.contains('fable-chrome-peek');
  if (!peeked) {
    if (e.clientY <= CHROME_PEEK_OPEN_PX) {
      document.body.classList.add('fable-chrome-peek');
      if (peekTimer) { clearTimeout(peekTimer); peekTimer = null; }
    }
    return;
  }
  // Forgive zone: stay peeked while the cursor is within the top
  // CHROME_PEEK_RATIO of the viewport. Only arm the collapse timer once it
  // leaves that band. (window.innerHeight is read live each move so display
  // changes / resizes are honored without a separate listener.)
  const threshold = Math.max(4, window.innerHeight * CHROME_PEEK_RATIO);
  if (e.clientY > threshold) {
    // Outside the zone: if no collapse is armed yet, start the grace timer.
    // Further moves outside the zone don't reset it (the cursor is clearly
    // leaving), so the bar collapses after a short, predictable delay.
    if (peekTimer) return;
    peekTimer = setTimeout(() => {
      document.body.classList.remove('fable-chrome-peek');
      peekTimer = null;
    }, CHROME_PEEK_GRACE_MS);
    return;
  }
  // Inside the zone: cancel any pending collapse.
  if (peekTimer) { clearTimeout(peekTimer); peekTimer = null; }
}
