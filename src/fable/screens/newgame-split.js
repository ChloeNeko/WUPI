// =============================================================
// SCREEN: NEW GAME SPLIT — the host for every picker slide of the
// cinematic New Game flow (Player / SIM / Codex / Intro pairs).
//
// This screen ships ONLY the empty `.fable-newgame-tiles` host — the actual
// pair tiles (+ optional IMPORT mini tile) are injected + reverse-spawned by
// `buildFlowPairTiles` in fable.js on every picker show, so no static labels
// live here (the prior CREATE/LOAD placeholders were always overwritten on the
// first paint). Every tile icon was removed from the New Game flow (Chloe
// 2026-08-03) — the only chrome that survives is the flow controller's
// ‹ / ⌂ (engine/flow-chrome.js), NOT on this screen. There is no header bar /
// home button here (Chloe 2026-08-02: "no header and back button it looks
// terrible"). ‹ is HIDDEN on the Player pair (slide 1, Home is the only exit
// at that depth); the flow controller reveals ‹ once the user advances.
//
// The tiles ship at opacity:0 so the flow controller can reverse-spawn them
// after the black transition clears (the music + buttons fade in TOGETHER,
// post-black). Each click triggers the burn-transition engine
// (engine/burn-transition.js).
//
// AMBIENCE: deep black void + rising fire embers (screens/embers.js), framed
// by a faint hearth-glow. Fresh on show, destroyed on hide.
// =============================================================

export function buildNewGameSplit() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-newgame-split-screen';
  root.dataset.fableScreen = 'newgame-split';
  root.hidden = true;
  // Empty host — buildFlowPairTiles (fable.js) populates the pair row + the
  // optional IMPORT mini tile on every picker show, so nothing is baked in.
  root.innerHTML = `<div class="fable-newgame-tiles"></div>`;
  // NOTE: the deep-void background + hearth glow + rising embers NO LONGER
  // live here — they were hoisted to a persistent .fable-flow-ambiance layer
  // on #fable (fable.js) so the background stays consistent across screen
  // swaps. This screen now carries ONLY the foreground UI (the tile host).
  return root;
}
