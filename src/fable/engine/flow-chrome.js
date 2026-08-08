// =============================================================
// FLOW CHROME — the persistent ‹ (back) + ⌂ (home) nav for the New
// Game flow.
//
// A single overlay element mounted ONCE into #fable (above every New-
// Game-flow screen). It is NEVER part of any burned content area, so
// it never moves or flickers during the burn/reverse-spawn transitions.
// Only the ‹ button's visibility changes (hidden on Pair 1 / Create-
// Load Sim; visible from Pair 2 onward + on the creators/pickers);
// ⌂ is always present.
//
// CONTRACT:
//   mountFlowChrome(root)  — create + append the chrome to `root`.
//                            Returns a controller { showBack, hideBack,
//                            onBack, onHome, destroy }.
//   showBack()/hideBack()  — toggle ‹ with the amber fade (the .is-hidden
//                            class). Idempotent.
//   onBack(fn)/onHome(fn)  — set the click handlers (re-routed through
//                            cloneNode-detach so re-wiring never stacks).
//
// The chrome owns its OWN click routing; the screens don't reach in.
// =============================================================

const HOME_SVG = `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 11l8-7 8 7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/><path d="M6 10v9a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1v-9" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M10 20v-5h4v5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/></svg>`;
const BACK_SVG = `<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M15 5l-7 7 7 7" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>`;

// Mount the chrome into `root`. Returns a controller. The chrome is a
// direct child of root (positioned absolute over the screens), so it
// shares root's stacking context and sits above screen content (z:50).
export function mountFlowChrome(root) {
  if (!root) return null;
  // Idempotent: if a chrome already exists on this root, reuse it.
  let chrome = root.querySelector('.fable-flow-chrome');
  if (chrome) return bindController(chrome);

  chrome = document.createElement('div');
  chrome.className = 'fable-flow-chrome';
  chrome.setAttribute('aria-hidden', 'true');
  chrome.innerHTML = `
    <button class="fable-flow-chrome__btn fable-flow-chrome__btn--back is-hidden"
            type="button" aria-label="Back" title="Back">${BACK_SVG}</button>
    <button class="fable-flow-chrome__btn fable-flow-chrome__btn--home"
            type="button" aria-label="Back to title" title="Back to title">${HOME_SVG}</button>
  `;
  root.appendChild(chrome);
  return bindController(chrome);
}

// Wire the controller onto an existing chrome element. Handlers are
// attached via cloneNode-detach so re-wiring (re-entry into the flow)
// never stacks listeners (mirrors creator.js:205-213).
function bindController(chrome) {
  const backBtn = chrome.querySelector('.fable-flow-chrome__btn--back');
  const homeBtn = chrome.querySelector('.fable-flow-chrome__btn--home');

  const setHandler = (btn, fn) => {
    const fresh = btn.cloneNode(true);
    btn.replaceWith(fresh);
    if (fn) fresh.addEventListener('click', fn);
    return fresh;
  };

  let backRef = backBtn;
  let homeRef = homeBtn;

  // The pending home-reveal timer (so re-entering the flow before the previous
  // delay elapsed cancels the stale reveal — only the latest entry's timer
  // decides when home appears).
  let homeTimer = null;

  return {
    // Toggle ‹ with the amber fade. Idempotent.
    showBack() { backRef.classList.remove('is-hidden'); },
    hideBack() { backRef.classList.add('is-hidden'); },
    // Hide ⌂ home, then reveal it after `ms` (Chloe 2026-08-02: "the house
    // icon spawns immediately when pressing new game, add a 2.5s delay so it
    // doesn't appear on the main menu"). Re-entrant: a prior pending reveal is
    // cancelled first. The home button is NOT shown on the title/main menu —
    // flow-chrome only mounts inside the New Game flow.
    delayHome(ms = 2500) {
      if (homeTimer) { clearTimeout(homeTimer); homeTimer = null; }
      homeRef.classList.add('is-hidden');
      homeTimer = setTimeout(() => {
        homeTimer = null;
        homeRef.classList.remove('is-hidden');
      }, ms);
    },
    showHome() {
      if (homeTimer) { clearTimeout(homeTimer); homeTimer = null; }
      homeRef.classList.remove('is-hidden');
    },
    hideHome() {
      if (homeTimer) { clearTimeout(homeTimer); homeTimer = null; }
      homeRef.classList.add('is-hidden');
    },
    // Re-route click handlers (clone-detach prevents stacking).
    onBack(fn) { backRef = setHandler(backRef, fn); },
    onHome(fn) { homeRef = setHandler(homeRef, fn); },
    // Set the visual variant: 'newgame' (brass) or 'quickplay' (white). The
    // Quick Play home button reads as a bright white glyph over the void
    // (Chloe 2026-08-05) instead of the New Game flow's aged brass.
    setVariant(variant) {
      chrome.classList.remove('fable-flow-chrome--quickplay', 'fable-flow-chrome--newgame');
      if (variant) chrome.classList.add(`fable-flow-chrome--${variant}`);
    },
    // Full teardown (on Fable close).
    destroy() {
      if (homeTimer) { clearTimeout(homeTimer); homeTimer = null; }
      if (chrome.parentNode) chrome.parentNode.removeChild(chrome);
    },
  };
}
