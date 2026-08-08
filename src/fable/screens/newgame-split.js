// =============================================================
// SCREEN: NEW GAME SPLIT — Pair 1 of the cinematic New Game flow.
//
// Two caption-only dark panels: CREATE SIM CARD (left) + LOAD SIM CARD
// (right). Every tile icon was removed from the New Game flow (Chloe
// 2026-08-03) — the only chrome that survives is the flow controller's
// ‹ / ⌂ (engine/flow-chrome.js), NOT on this screen. There is no header
// bar / home button here (Chloe 2026-08-02: "no header and back button
// it looks terrible"). ‹ is HIDDEN on Pair 1 (Home is the only exit at
// this depth); the flow controller reveals ‹ once the user advances to
// Pair 2.
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
import { tileCaptionHTML } from '../engine/tile-caption.js';

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
        <span class="fable-newgame-tile-caption">${tileCaptionHTML('CREATE SIM CARD')}</span>
      </button>
      <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="existing">
        <span class="fable-newgame-tile-caption">${tileCaptionHTML('LOAD SIM CARD')}</span>
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
