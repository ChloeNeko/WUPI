// =============================================================
// SCREEN: NEW GAME SPLIT — Pair 1 of the cinematic New Game flow.
//
// Two giant tiles: CREATE SIM CARD (left) + LOAD SIM CARD (right). The
// chrome (‹ / ⌂) is owned by the flow controller (engine/flow-chrome.js),
// NOT this screen — there is no header bar / home button here anymore
// (Chloe 2026-08-02: "no header and back button it looks terrible").
// ‹ is HIDDEN on Pair 1 (Home is the only exit at this depth); the flow
// controller reveals ‹ once the user advances to Pair 2.
//
// The tiles ship at opacity:0 so the flow controller can reverse-spawn
// them after the black transition clears (the music + buttons fade in
// TOGETHER, post-black — per Chloe's spec). Each click triggers the
// burn-transition engine (engine/burn-transition.js).
//
// AMBIENCE: deep black void + rising fire embers (screens/embers.js),
// framed by a faint hearth-glow. Fresh on show, destroyed on hide.
// =============================================================

import { createEmbers } from './embers.js';

export function buildNewGameSplit(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-newgame-split-screen';
  root.dataset.fableScreen = 'newgame-split';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-glow" aria-hidden="true"></div>
    <div class="fable-ember-host" aria-hidden="true"></div>
    <div class="fable-newgame-tiles">
      <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="create">
        <span class="fable-newgame-tile-glyph" aria-hidden="true"><svg class="fable-create-glyph" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><line x1="16" y1="4" x2="5" y2="15" /><path d="M5 15 L7 9 L13 3 L19 9 L13 15 Z" /><path d="M5 15 L3 19 L7 15" /><line x1="11.5" y1="6.5" x2="15.5" y2="10.5" /><line x1="15" y1="3" x2="19" y2="7" /></svg></span>
        <span class="fable-newgame-tile-caption">CREATE SIM CARD</span>
      </button>
      <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="existing">
        <span class="fable-newgame-tile-glyph" aria-hidden="true"><svg class="fable-scroll-glyph" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2 L14 10 L22 12 L14 14 L12 22 L10 14 L2 12 L10 10 Z" /><circle cx="12" cy="12" r="1" /></svg></span>
        <span class="fable-newgame-tile-caption">LOAD SIM CARD</span>
      </button>
    </div>
  `;
  // Create (left) + Existing (right). Each click passes the CLICKED
  // button to its handler — the flow controller (fable.js) uses it as
  // the `selectedBtn` for the burn (it pops + fades; the OTHER burns).
  root.querySelector('[data-act="create"]').addEventListener('click', (e) => handlers.createNew && handlers.createNew(e.currentTarget));
  root.querySelector('[data-act="existing"]').addEventListener('click', (e) => handlers.useExisting && handlers.useExisting(e.currentTarget));

  // Ambient ember lifecycle — mirrors title.js's particle wiring. Fresh
  // on show, destroyed on hide so no RAF/listener leaks.
  const emberHost = root.querySelector('.fable-ember-host');
  let embers = null;
  root._startAmbient = () => { if (!embers) embers = createEmbers(emberHost); };
  root._stopAmbient = () => { if (embers) { embers.destroy(); embers = null; } };

  return root;
}
