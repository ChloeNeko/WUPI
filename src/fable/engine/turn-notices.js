// =============================================================
// TURN NOTICES — top-left slide-in bubbles for the silent state
// changes a turn makes (2026-08-22 playtest, Chloe spec).
//
// The Combat Referee assigns injuries and the tracker's bracket
// applies move inventory (auto-wear, equip swaps, belt spills)
// entirely inside fable_send — the playtest's player "never found
// out he was even injured" until the prose happened to mention a
// limp. The backend now emits `{ type: 'turn_notice', kind, text }`
// over the turn channel (narrator.js routes it here):
//   kind 'injury'    — "Right Foot is now heavily injured." /
//                      "… took a lethal blow — you are DOWNED."
//   kind 'inventory' — "Iron Dagger was equipped." /
//                      "Canteen was moved to the pack."
//
// UX per Chloe: bubbles pop in from the LEFT edge of the TOP-LEFT
// corner, a NEW bubble takes the top slot while the older ones GLIDE
// down to make room (FLIP on measured rects — no height math), fade
// out slowly once their lifetime lapses, and a click dismisses early.
// No hover tooltips (the app-wide ban) — textContent only.
// =============================================================

const MAX_STACK = 5;
const LIFETIME_MS = 5200;
const FADE_MS = 650;

function host() {
  // The bubbles live on the ACTIVE stage screen so they ride the feed
  // (and die with the screen swap — no orphans on the title screen).
  // Every Fable screen (title, pickers, stage…) is a `.fable-screen`
  // kept in the DOM + toggled via `hidden` — a bare `.fable-screen`
  // query would grab whichever sits FIRST in DOM order (the title
  // screen, display:none mid-game) and the bubbles would never show.
  // Scope to the visible stage screen itself.
  const screen = document.querySelector('.fable-screen.fable-stage:not([hidden])');
  if (!screen) return null;
  let root = screen.querySelector('.fable-turn-notices');
  if (!root) {
    root = document.createElement('div');
    root.className = 'fable-turn-notices';
    screen.appendChild(root);
  }
  return root;
}

// FLIP the `kids` from where they just were (`tops`, captured BEFORE
// the DOM mutation) to their new natural slots: pose each at its old
// position, then release — the CSS transform transition does the glide.
function flip(kids, tops) {
  kids.forEach((k, i) => {
    if (!k.isConnected) return;
    const delta = tops[i] - k.getBoundingClientRect().top;
    if (!delta) return;
    k.style.transition = 'none';
    k.style.transform = `translateY(${delta}px)`;
    void k.offsetHeight; // commit the old pose before releasing it
    k.style.transition = '';
    k.style.transform = '';
  });
}

// Fade one bubble out, then remove it — the bubbles below it glide up
// to close the gap (same FLIP, measured just before the removal so
// any stack churn during the fade is accounted for).
function dismiss(el) {
  if (!el.isConnected || el.classList.contains('is-out')) return;
  el.classList.add('is-out');
  setTimeout(() => {
    const parent = el.parentElement;
    const kids = parent
      ? [...parent.children].filter((k) => k !== el && k.isConnected)
      : [];
    const tops = kids.map((k) => k.getBoundingClientRect().top);
    el.remove();
    flip(kids, tops);
  }, FADE_MS);
}

/**
 * Show one turn-notice bubble. `kind` ∈ { 'injury', 'inventory' } —
 * anything else renders with the neutral inventory chrome.
 * Silently no-ops when no stage screen is mounted.
 */
export function showTurnNotice(kind, text) {
  if (!text) return;
  const root = host();
  if (!root) return;
  // Cap the LIVE stack (fading bubbles don't count — they're leaving).
  // Document order is newest-first (prepend), so the OLDEST bubble — the
  // one to evict — is the END of the list.
  const live = [...root.querySelectorAll('.fable-turn-notice:not(.is-out)')];
  while (live.length >= MAX_STACK) dismiss(live.pop());
  // The new bubble takes the TOP slot; the existing bubbles glide down.
  const kids = [...root.children];
  const tops = kids.map((k) => k.getBoundingClientRect().top);
  const el = document.createElement('div');
  el.className = 'fable-turn-notice' + (kind === 'injury' ? ' is-injury' : '');
  el.textContent = String(text); // textContent — never HTML
  root.insertBefore(el, root.firstChild);
  flip(kids, tops);
  const timer = setTimeout(() => dismiss(el), LIFETIME_MS);
  el.addEventListener('click', () => {
    clearTimeout(timer);
    dismiss(el);
  }, { once: true });
}
