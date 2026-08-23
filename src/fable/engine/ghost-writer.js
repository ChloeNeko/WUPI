// =============================================================
// GHOST WRITER — the composer's ghost icon (2026-08-22).
//
// Three turn aids behind one tiny ghost at the composer's right edge:
//   Swipe       → a GUIDED reroll of the trailing beat (the typed text is
//                 the narrator's <direction>; the existing reroll machinery
//                 streams the fresh variant over the old one)
//   Continue    → the trailing beat extended from where it ends, landed
//                 through the edit + re-track path (narrator.ghostContinue)
//   Impersonate → the player's NEXT message written in their own voice,
//                 dropped into the composer for review (never auto-sent)
//
// Swipe + Continue REQUIRE typed text (the warning chip fires otherwise,
// per Chloe's spec); Impersonate runs with or without a steer. All three
// need an idle composer: the icon is inert while a turn is in flight or
// the API lock holds.
//
// The module also owns the COMPOSER NOTICE chip (the small popup directly
// above the input bar) — the warning + busy surface the Crossroads module
// reuses.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

// The menu model, in display order. Pure data — pinned by tests.
export const GHOST_MODES = [
  { id: 'swipe', label: 'Swipe', requiresPrompt: true },
  { id: 'continue', label: 'Continue', requiresPrompt: true },
  { id: 'impersonate', label: 'Impersonate', requiresPrompt: false },
];

// Does the given mode refuse to run on an empty composer? (Pure, tested.)
export function ghostModeRequiresPrompt(id) {
  const mode = GHOST_MODES.find((m) => m.id === id);
  return mode ? mode.requiresPrompt : false;
}

// The exact warning copy when Swipe/Continue fire on an empty composer.
export const EMPTY_PROMPT_WARNING = 'Please type a prompt for the Narrator to follow.';

let deps = null;          // wiring context (set in wireGhostWriter)
let menuEl = null;        // the open menu element (null when closed)
let noticeEl = null;      // the composer notice chip (null when hidden)
let noticeTimer = null;   // auto-hide timer for warning notices
let outsideCloser = null; // transient document listener while the menu is open

export function wireGhostWriter(next) {
  deps = next;
  hideComposerNotice(true);
  closeMenu();
}

export function teardownGhostWriter() {
  deps = null;
  hideComposerNotice(true);
  closeMenu();
}

export function isGhostMenuOpen() {
  return menuEl !== null;
}

export function closeMenu() {
  if (outsideCloser) {
    document.removeEventListener('pointerdown', outsideCloser, true);
    outsideCloser = null;
  }
  if (menuEl) {
    menuEl.remove();
    menuEl = null;
  }
}

function openMenu() {
  if (!deps || menuEl) return;
  menuEl = document.createElement('div');
  menuEl.className = 'fable-aid-menu fable-ghost-menu';
  menuEl.setAttribute('role', 'menu');
  for (const mode of GHOST_MODES) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'fable-aid-menu-item';
    btn.textContent = mode.label;
    btn.dataset.ghostMode = mode.id;
    btn.setAttribute('role', 'menuitem');
    menuEl.appendChild(btn);
  }
  menuEl.addEventListener('click', (e) => {
    const btn = e.target.closest('[data-ghost-mode]');
    if (!btn) return;
    e.stopPropagation();
    runMode(btn.dataset.ghostMode);
  });
  deps.anchor.appendChild(menuEl);
  // Close on any pointerdown outside the menu + its icon (the icon's own
  // click toggles closed). Transient document listener, removed on close.
  outsideCloser = (ev) => {
    if (menuEl && !menuEl.contains(ev.target) && !deps.icon.contains(ev.target)) {
      closeMenu();
    }
  };
  document.addEventListener('pointerdown', outsideCloser, true);
}

function runMode(id) {
  if (!deps) return;
  // A turn may have started while the menu was open — refuse with the
  // notice rather than silently dropping the action.
  if (deps.isBusy()) {
    closeMenu();
    showComposerNotice('The narrator is busy right now.');
    return;
  }
  const input = deps.getInput();
  const text = input ? input.value.trim() : '';
  if (ghostModeRequiresPrompt(id) && !text) {
    closeMenu();
    showComposerNotice(EMPTY_PROMPT_WARNING);
    return;
  }
  closeMenu();
  if (id === 'swipe' || id === 'continue') {
    // The composer is cleared the moment the nudge is claimed (Chloe's
    // contract) — the steer lives in the directive from here on.
    if (input) { input.value = ''; deps.onInputChanged(); }
    if (id === 'swipe') deps.onSwipe(text);
    else deps.onContinue(text);
    return;
  }
  if (id === 'impersonate') {
    void impersonate(text);
    return;
  }
}

async function impersonate(text) {
  if (!deps || deps.isBusy()) return;
  showComposerNotice('Ghostwriting your next message...', { busy: true });
  try {
    const result = await invoke('ghostwriter_impersonate', { nudge: text || null });
    if (!deps) return;
    const input = deps.getInput();
    if (input && typeof result === 'string' && result.trim()) {
      input.value = result.trim();
      deps.onInputChanged();
      input.focus();
    }
    hideComposerNotice();
  } catch (err) {
    if (!deps) return;
    showComposerNotice(String(err));
  }
}

// ---- The composer notice chip -----------------------------------
// A single small popup directly above the input bar (anchored to the
// input row, like the typing indicator). `warn` (default) is amber-red
// with an auto-hide; `{ busy: true }` carries the spinner ring + stays
// until explicitly hidden. One chip at a time — a new call replaces the
// current one.

export function showComposerNotice(text, opts = {}) {
  const row = deps && deps.inputRow;
  if (!row) return;
  clearNoticeTimer();
  if (!noticeEl) {
    noticeEl = document.createElement('div');
    noticeEl.className = 'fable-composer-notice';
    row.appendChild(noticeEl);
  }
  noticeEl.classList.toggle('is-busy', !!opts.busy);
  noticeEl.classList.add('is-visible');
  const label = document.createElement('span');
  label.className = 'fable-composer-notice__text';
  label.textContent = text;
  noticeEl.replaceChildren(label);
  if (!opts.busy) {
    noticeTimer = setTimeout(() => hideComposerNotice(), 2600);
  }
}

export function hideComposerNotice(immediate = false) {
  clearNoticeTimer();
  if (!noticeEl) return;
  if (immediate) {
    noticeEl.remove();
    noticeEl = null;
    return;
  }
  noticeEl.classList.remove('is-visible');
  const el = noticeEl;
  noticeEl = null;
  // Let the fade-out finish before dropping the element.
  setTimeout(() => el.remove(), 240);
}

function clearNoticeTimer() {
  if (noticeTimer) {
    clearTimeout(noticeTimer);
    noticeTimer = null;
  }
}

// Stage wiring entry for the icon itself. `deps`:
//   icon      the ghost button element
//   anchor    the element menus position against (the aids cluster)
//   inputRow  the .fable-input-row form (the notice chip mounts inside)
//   getInput  () => the composer textarea
//   isBusy    () => true while a turn is in flight or the API lock holds
//   onSwipe / onContinue / onInputChanged  stage-provided flows
export function handleGhostIconClick() {
  if (!deps) return;
  if (deps.isBusy()) {
    // Never a silent dead button: the aids need an idle narrator.
    showComposerNotice('The narrator is busy right now.');
    return;
  }
  if (menuEl) closeMenu();
  else openMenu();
}
