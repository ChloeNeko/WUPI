// =============================================================
// FABLE APP ENTRY — composition root for the whole subsystem.
//
// Fable is a REGISTERED APP under the WUPI app-lifecycle framework
// (src/app-lifecycle.js): onOpen / onClose / onPause / onResume. This is
// what makes WUPI feel like a real OS — Fable is a dedicated full-screen
// experience with NO minimize button / window bar. The only way back to
// the OS desktop is the title-screen EXIT button, which calls
// AppLifecycle.closeApp('fable') → full resource teardown.
//
// Lifecycle contract (the GUARANTEE: zero memory leaks, zero audio leaks,
// zero background CPU/GPU waste):
//   onOpen   → show #fable, activate chrome, pause OS aurora, ignite title,
//              start theme music + title particles.
//   onPause  → (alt-tab / focus-loss) freeze theme music + stage parallax +
//              CSS FX particles. Title particle RAF already self-pauses on
//              document.hidden (see particles.js).
//   onResume → (focus return) unfreeze all of the above smoothly.
//   onClose  → (EXIT only) full teardown: stage torn down, particles
//              destroyed, music removed, chrome restored, OS aurora resumed,
//              fable_end IPC sent. Nothing survives the close.
//
// MENU STATE: all 3 title flows are wired.
//   New Game → card picker (screens/picker.js): pick a shipped .sim → straight
//              into the stage at a fresh game (no interview).
//   Continue → resume the freshest save (resumeSave).
//   Load     → two-level picker: worlds.js → saves.js → resume.
// The working stage + gameplay engine (stage.js, engine/*, fx/*, panels/*)
// are the destination of every flow.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { AppLifecycle } from '../app-lifecycle.js';
import './fable.css';
import './flow-cinematic.css';

import { buildTitle } from './screens/title.js';
import { buildStage, wireStage, teardownStage, toast } from './screens/stage.js';
import { buildPicker, renderPicker } from './screens/picker.js';
import { buildNewGameSplit } from './screens/newgame-split.js';
import { buildCreator, renderCreator } from './screens/creator.js';
import { buildPlayerCreator, renderPlayerCreator } from './screens/player-creator.js';
import { buildPlayerPicker, renderPlayerPicker } from './screens/player-picker.js';
import { buildWorlds, renderWorlds } from './screens/worlds.js';
import { buildSaves, renderSaves } from './screens/saves.js';
import {
  startThemeMusic, stopThemeMusic, fadeOutThemeMusic,
  pauseThemeMusic, resumeThemeMusic,
} from './screens/reveal.js';
import {
  startNewGameMusic, stopNewGameMusic,
  pauseNewGameMusic, resumeNewGameMusic,
} from './screens/newgame-music.js';
import { playBootTransition } from './screens/boot.js';
import { playFogIntro } from './screens/fog.js';
import { pauseFX, resumeFX } from './fx/effects.js';
import { detachParallax, attachParallax } from './fx/atmosphere.js';
import { activateChrome, deactivateChrome } from './engine/chrome.js';
import { playMagicalTransition } from './engine/transition.js';
import { mountFlowChrome } from './engine/flow-chrome.js';
import { playBurnTransition, playReverseSpawn } from './engine/burn-transition.js';

let fableRoot = null;       // the #fable app-window element
let screens = {};           // name → screen element
// The persistent flow-chrome controller (‹ / ⌂). Mounted once into
// fableRoot when the New Game flow begins; owns all nav for the flow
// so the screens themselves carry no header bars.
let flowChrome = null;
// The New Game flow state machine. Tracks where we are so ‹ can route
// back correctly + the selected sim/player ids carry forward.
//   step: 'sim' (Pair 1) | 'player' (Pair 2) | 'sim-creator' | 'sim-picker' | 'player-creator' | 'player-picker'
//   pair1Choice: 'create' | 'existing'  (which Pair 1 tile was clicked —
//                routed to after the player step so the Sim Creator or
//                Picker opens with the player already in hand)
let flowState = { step: null, pair1Choice: null, selectedCardId: null, selectedPlayerId: null };

// DEV SHORTCUT (?dev=fable or #dev=fable): mirrors script.js's
// DEV_FABLE_SHORTCUT. When active, openFable skips the fog gate + the 2s boot
// transition and instead shows the title with buttons already visible + theme
// music started immediately. Used together with script.js skipping the OS boot
// ceremony, a dev refresh lands on Fable's title in well under a second. False
// in production (Tauri loads the page with no query/hash). Accepts both the
// query (?dev=fable) and hash (#dev=fable) forms — see script.js for why.
const DEV_FABLE_SHORTCUT = (() => {
  try {
    if (new URLSearchParams(window.location.search).get('dev') === 'fable') return true;
    const h = window.location.hash.replace(/^#/, '');
    return new URLSearchParams(h).get('dev') === 'fable';
  } catch (_) { return false; }
})();
// Whether the stage screen is currently the active screen. Drives the
// pause/resume split: title freezes music, stage freezes parallax + FX.
let stageActive = false;
// The active boot-transition controller (null when no transition is
// running). closeFable cancels it FIRST so an EXIT during the cinematic
// can't leak a fog node, orphan audio, or strand invisible buttons.
let currentBoot = null;
// The active fog-intro controller (null when no fog intro is running). The
// fog gate plays before the boot transition; closeFable cancels it FIRST so
// an EXIT during the 3s hold tears down the overlay + wind audio.
let currentFog = null;

// External hooks set by script.js (the OS chrome integration).
// closeWindow: ref to the OS closeWindow() so Fable's own close paths
// (Exit button) keep the openWindows set in sync.
let hooks = { pauseAurora: null, resumeAurora: null, openHooks: null, closeHooks: null, closeWindow: null };

// Screen router: hide all, show one. Also drives each screen's ambient
// lifecycle: any screen exposing _startAmbient / _stopAmbient (the title's
// floating motes + the New Game flow's rising fire embers) runs its ambient
// system ONLY while it is the visible screen (created on show, destroyed on
// hide) so no canvas RAF ever leaks across screen changes or exit/relaunch.
// This is screen-agnostic: a screen opts in by defining the hooks, no
// special-casing here.
function showScreen(name) {
  for (const key of Object.keys(screens)) {
    const showing = (key === name);
    const scr = screens[key];
    scr.hidden = !showing;
    if (showing && scr._startAmbient) scr._startAmbient();
    else if (!showing && scr._stopAmbient) scr._stopAmbient();
  }
  // Track whether the stage owns the screen (pause/resume + onClose branch
  // on this to know whether stage-specific teardown is needed).
  stageActive = (name === 'stage' && !!screens.stage);
}

// Track whether the engine started for the current stage session, so
// returnToTitle knows whether to call fable_end (no-op call is wasteful +
// noisy in logs). Reset to false whenever we leave the stage.
let engineStarted = false;
// Whether the current stage session is a Quick Play run. Set on entry via
// the Quick Play paths + read by enterStageViaTransition to pass isQuickPlay
// to wireStage (which hides the manual Save/Load footer buttons — Quick Play
// auto-saves its single quicksave slot on fable_end). Reset on returnToTitle.
let isQuickPlaySession = false;

// === Continue / Load / New Game / Quick Play flows =======================
//
// CONTINUE: resume the freshest save for a NEW GAME world (the title's
// _refreshContinue stashes the target from fable_continue_target). Autosaves
// are included (the per-turn checkpoint is "where you left off"). Quick Play
// quicksaves are EXCLUDED (Quick Play owns its own Resume). The stashed
// target carries both card_id + save_id, so this is a one-shot resume.
//
// LOAD: a two-level picker — choose a world (screens/worlds.js), then choose
// a save in that world (screens/saves.js). Both feed into resumeSave.
//
// NEW GAME: a split screen (Use Existing vs Create New). "Use Existing" opens
// the card picker (screens/picker.js) — pick a shipped .sim → fresh game.
// "Create New" opens the Creator (screens/creator.js) — author a card via the
// 3-tab form, save it, → fresh game from the new card. No interview, no draft.
//
// QUICK PLAY: throw the player straight into the placeless Narrative
// Simulator (data/fable.sim). If a quicksave exists (title._quickPlaySave),
// an inline Start-New / Resume-Last choice appears; otherwise a fresh run
// starts immediately. ONE quicksave slot, auto-written on fable_end — no
// manual saves, no load list.
//
// resumeSave mirrors the shared stage-entry tail (stop ambient/music →
// fable_end → load → enterStageViaTransition) and drives a world via
// fable_start(cardId, saveId), which re-reads the .sim from disk + resumes
// the named slot.

// Resume a named save for a world. `cardId` resolves the .sim card; `saveId`
// is the slot to resume. Cold-resume from the title (no game running yet),
// so fable_start is the entry — NOT fable_load_save (that requires an
// already-running game).
async function resumeSave(cardId, saveId) {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  fadeOutThemeMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', { cardId, saveId });
    engineStarted = true;
    if (result && result.opening_scene) openingScene = result.opening_scene;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_start (resume) failed — entering stage without engine', err);
    engineStarted = false;
  }

  enterStageViaTransition(openingScene, loadMessages);
}

// CONTINUE button handler: resume the stashed continue target. The target is
// refreshed on every title show via _refreshContinue (title.js); if it's null
// (no save exists) the button is disabled, so a null target here is a
// race/state bug — guard + log rather than crash.
function onContinueClicked() {
  const target = (screens.title && screens.title._continueTarget) || null;
  if (!target || !target.card_id || !target.save_id) {
    console.warn('[fable] Continue clicked with no resume target — ignoring');
    return;
  }
  resumeSave(target.card_id, target.save_id);
}

// LOAD button handler: show the world picker + populate it. The worlds screen
// lists only worlds that have saves (a world with no saves is a New Game
// target). Selecting one routes to the saves list for that world.
function onLoadClicked() {
  showScreen('worlds');
  renderWorlds(screens.worlds, (card) => openWorldSaves(card));
}

// World-picker select → switch to the saves screen for the chosen world +
// render its save list. onSelect(save) resumes that slot.
function openWorldSaves(card) {
  showScreen('saves');
  renderSaves(screens.saves, card.id, (save) => resumeSave(card.id, save.save_id), card.name);
}

// === THE CINEMATIC NEW GAME FLOW (4-step state machine) ================
//
// Title "New Game" click
//   → [black transition; embers + music + buttons fade IN after black clears]
//   → Pair 1: [Create Sim] [Load Sim]   (‹ hidden, ⌂ present)
//       Create Sim → burn → Sim Card Creator (3-button) → Save → advance to Pair 2
//       Load Sim   → burn → Sim Card Picker → select → advance to Pair 2
//   → Pair 2: [Create Player] [Load Player]   (‹ APPEARS, ⌂ present)
//       Create Player → burn → Player Creator → Save → start game (sim + player)
//       Load Player   → burn → Player Picker → select → start game
//       ‹ from Pair 2 → reverse to Pair 1
//
// AUDIO (Chloe's spec, 2026-08-02): the music + buttons fade in ONLY after
// the black fully clears — NOT at the midpoint. So onNewGameClicked runs
// the black transition with an empty midpoint (just swaps the DOM at
// opacity:0), then AFTER the undim resolves it starts the music fade-in
// AND reverse-spawns the Pair 1 buttons together.
//
// The flow chrome (‹ / ⌂) is mounted once into fableRoot on entry; it
// persists across every step (never burns, never moves). Only ‹'s
// visibility toggles (hidden on Pair 1; shown from Pair 2 onward).

// Begin the New Game flow: black transition → embers + music + Pair 1.
function onNewGameClicked() {
  // Theme FADES OUT at click time (Chloe 2026-08-02: "let the entire mp3
  // play out, don't cut it"). The theme ramps to silence over ~1.5s + the
  // node removes itself when silent — no hard cut. The short button SFX
  // (fableButtonSFX.mp3) plays alongside it and finishes naturally.
  fadeOutThemeMusic(fableRoot);
  // Reset the flow state + mount the chrome (‹ hidden initially).
  flowState = { step: 'sim', pair1Choice: null, selectedCardId: null, selectedPlayerId: null };
  if (!flowChrome) flowChrome = mountFlowChrome(fableRoot);
  if (flowChrome) {
    flowChrome.hideBack();  // Pair 1: ‹ hidden
    // ⌂ home is hidden for 2.5s on every New Game entry so it doesn't appear
    // the instant the flow mounts (Chloe: "the house icon spawns immediately
    // when pressing new game, add a 2.5s delay"). Re-entrant: re-pressing New
    // Game cancels any prior pending reveal + restarts the delay.
    flowChrome.delayHome(2500);
    flowChrome.onHome(() => exitFlowToTitle());
    flowChrome.onBack(() => flowBack());
  }
  // The split tiles ship at opacity:0 (the reverse-spawn reveals them
  // after the black clears). Show the screen at the midpoint (DOM ready,
  // still invisible), then after the undim: start music + spawn buttons.
  playMagicalTransition({
    blackHoldMs: 1150,
    onMidpoint: () => {
      // Swap the screen at peak darkness (invisible — content is at 0).
      // Do NOT start music here; it blooms AFTER the black clears.
      // Rebuild the Pair 1 tiles fresh (the split may hold Pair 2 labels
      // from a prior run) + wire their click handlers.
      const tiles = rebuildSplitTiles([
        { glyphHtml: `<span class="fable-newgame-tile-glyph" aria-hidden="true">${CREATE_SIM_GLYPH_SVG}</span>`, caption: 'CREATE SIM CARD', act: 'create' },
        { glyphHtml: `<span class="fable-newgame-tile-glyph" aria-hidden="true">${LOAD_SIM_GLYPH_SVG}</span>`, caption: 'LOAD SIM CARD', act: 'existing' },
      ]);
      tiles[0].addEventListener('click', (e) => flowCreateSim(e.currentTarget));
      tiles[1].addEventListener('click', (e) => flowLoadSim(e.currentTarget));
      showScreen('newgame-split');
      // Ensure the tiles are at opacity:0 for the reverse-spawn.
      tiles.forEach((t) => { t.style.opacity = '0'; });
    },
  }).then(() => {
    // AFTER the black fully clears: fade music in + reverse-spawn the
    // Pair 1 buttons together. This is the load-bearing timing fix.
    startNewGameMusic(fableRoot, { fadeIn: true });
    const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
    return playReverseSpawn(Array.from(tiles));
  }).catch((e) => {
    console.error('[fable] New Game transition failed, jumping to split', e);
    startNewGameMusic(fableRoot, { fadeIn: true });
    showScreen('newgame-split');
  });
}

// Exit the New Game flow back to the title (⌂ click). Fades the new-game
// tracks out + restarts the title theme (mirrors the old split back()).
function exitFlowToTitle() {
  stopNewGameMusic(fableRoot);
  startThemeMusic(fableRoot);
  if (flowChrome) {
    flowChrome.hideBack();
    // Hide ⌂ home too so it doesn't linger over the title/main menu after
    // exiting the flow (the chrome overlay persists; both buttons go dark).
    flowChrome.hideHome();
  }
  flowState = { step: null, pair1Choice: null, selectedCardId: null, selectedPlayerId: null };
  showScreen('title');
}

// === FLOW STEP TRANSITIONS =============================================
//
// THE BURN CONTRACT (Chloe's spec): when a button is clicked, the CLICKED
// button POPS (scale burst) then SLOWLY FADES OUT; the OTHER button(s)
// BURN bottom→top. The clicked button is NEVER burned. So every step
// transition receives `selectedBtn` (the clicked one) + builds the
// rejected list as "all siblings except the clicked one."
//
// THE ORDER (Chloe's spec): Player FIRST, then build the Sim Card.
//   Pair 1 (Create Sim / Load Sim)
//     → click either → burn →
//   Pair 2 (Create Player / Load Player)
//     → click Create Player → burn → Player Creator → Save → Sim Creator
//     → click Load Player   → burn → Player Picker → select → Sim Creator
//   Sim Creator / Picker → start game with [sim + player]
//
// (The Sim Creator is reached AFTER the player is chosen, so the player
// identity is in hand when the card is authored/selected. Both Create-Sim
// and Load-Sim converge on the Player step before the Sim work begins.)

// Rebuild the split screen's two tiles with the given labels + return
// the fresh tile elements (clone-detached so no handler stacks). Used by
// every Pair render so the burn/spawn engine always sees clean buttons.
// `labels[].glyphHtml` is the inner glyph markup — either a Pair-1 SVG
// (quill-in-inkwell for CREATE / unrolled scroll for LOAD) or the player
// person+badge SVG structure (Pair 2).
function rebuildSplitTiles(labels) {
  const split = screens['newgame-split'];
  const host = split.querySelector('.fable-newgame-tiles');
  host.innerHTML = labels.map((l) => `
    <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="${l.act}">
      ${l.glyphHtml}
      <span class="fable-newgame-tile-caption">${l.caption}</span>
    </button>
  `).join('');
  return Array.from(host.querySelectorAll('.fable-newgame-tile'));
}

// --- Pair 1 glyphs: CREATE = pencil, LOAD = 4-pointed star --------------------
// Two inline SVGs in a matched brass line-art style. Sized in em via
// .fable-create-glyph / .fable-scroll-glyph so they track the slot's font-size
// clamp. The newgame-split.js copies use stroke=currentColor to inherit the
// slot's brass hover-bright swap; these constant copies use the hardcoded
// #D4AF37 brass for the non-tile uses.
//
// CREATE — a writer's pencil drawn diagonally: wood-cradle ferrule → sharpened
// triangle nib → long shaft → eraser cap with a metal band.
const CREATE_SIM_GLYPH_SVG = `<svg viewBox="0 0 24 24" width="64" height="64" stroke="#D4AF37" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round" class="fable-icon-create"><!-- Pencil Shaft (diagonal: eraser top-right → nib bottom-left) --><line x1="16" y1="4" x2="5" y2="15" /><!-- Wood Cradle (the sharpened cone behind the nib) --><path d="M5 15 L7 9 L13 3 L19 9 L13 15 Z" /><!-- Graphite Nib Tip --><path d="M5 15 L3 19 L7 15" /><!-- Metal Ferrule Band on the Shaft --><line x1="11.5" y1="6.5" x2="15.5" y2="10.5" /><!-- Eraser Cap (top end) --><line x1="15" y1="3" x2="19" y2="7" /></svg>`;

// LOAD — a 4-pointed star (concave diamond, the classic compass/tarot star):
// "open / discover an existing card."
const LOAD_SIM_GLYPH_SVG = `<svg viewBox="0 0 24 24" width="64" height="64" stroke="#D4AF37" stroke-width="1.5" fill="none" stroke-linecap="round" stroke-linejoin="round" class="fable-icon-load"><!-- 4-Pointed Star: outer points N/E/S/W, concave arcs into the center --><path d="M12 2 L14 10 L22 12 L14 14 L12 22 L10 14 L2 12 L10 10 Z" /><!-- Center Spark Dot --><circle cx="12" cy="12" r="1" fill="#D4AF37" /></svg>`;

// --- Player tile glyphs (Pair 2): person silhouette + small badge ----
// Both player tiles share the SAME person silhouette; a small bottom-
// right badge distinguishes Create (+) from Load (folder). NOT the
// reused create/load glyphs from Pair 1 (Chloe: "that's lazy").
const PERSON_SVG = `<svg class="fable-person-icon" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">
  <circle cx="12" cy="7" r="3.4"/>
  <path d="M5 20c0-3.6 3.1-6.2 7-6.2s7 2.6 7 6.2"/>
</svg>`;
const PLUS_BADGE_SVG = `<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>`;
const FOLDER_BADGE_SVG = `<svg viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7a1 1 0 0 1 1-1h4l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7z"/></svg>`;
function playerTileGlyphHtml(badgeSvg) {
  return `<span class="fable-player-tile-glyph">${PERSON_SVG}<span class="fable-player-tile-badge" aria-hidden="true">${badgeSvg}</span></span>`;
}

// Pair 1 → Create Sim: clicked pops+fades, the other burns, → Pair 2.
function flowCreateSim(selectedBtn) {
  flowState.pair1Choice = 'create';
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => advanceToPlayerPair(),
  });
}

// Pair 1 → Load Sim: clicked pops+fades, the other burns, → Pair 2.
function flowLoadSim(selectedBtn) {
  flowState.pair1Choice = 'existing';
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => advanceToPlayerPair(),
  });
}

// Helper: all sibling tiles except the clicked one (the burn targets).
function siblingTilesExcept(selectedBtn) {
  return Array.from(selectedBtn.parentElement.querySelectorAll('.fable-newgame-tile'))
    .filter((b) => b !== selectedBtn);
}

// Advance from Pair 1 to Pair 2 (Create/Load Player). Rebuilds the split
// tiles with Pair 2 labels, shows the screen, and reverse-spawns the
// new tiles. The ‹ chrome becomes visible here.
function advanceToPlayerPair() {
  const tiles = rebuildSplitTiles([
    { glyphHtml: playerTileGlyphHtml(PLUS_BADGE_SVG), caption: 'CREATE PLAYER', act: 'create-player' },
    { glyphHtml: playerTileGlyphHtml(FOLDER_BADGE_SVG), caption: 'LOAD PLAYER', act: 'load-player' },
  ]);
  // Wire Pair 2 handlers (pass the clicked button for the burn).
  tiles[0].addEventListener('click', (e) => flowCreatePlayer(e.currentTarget));
  tiles[1].addEventListener('click', (e) => flowLoadPlayer(e.currentTarget));
  flowState.step = 'player';
  if (flowChrome) flowChrome.showBack();
  showScreen('newgame-split');
  // Hide + then ignite the new tiles into place.
  tiles.forEach((t) => { t.style.opacity = '0'; });
  playReverseSpawn(tiles);
}

// Pair 2 → Create Player: clicked pops+fades, the other burns, → Player Creator.
function flowCreatePlayer(selectedBtn) {
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      showScreen('player-creator');
      flowState.step = 'player-creator';
      renderPlayerCreator(screens['player-creator'], {
        onSave: (playerId) => { flowState.selectedPlayerId = playerId; advanceToSimStep(); },
      });
    },
  });
}

// Pair 2 → Load Player: clicked pops+fades, the other burns, → Player Picker.
function flowLoadPlayer(selectedBtn) {
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      showScreen('player-picker');
      flowState.step = 'player-picker';
      renderPlayerPicker(screens['player-picker'], (player) => {
        flowState.selectedPlayerId = player.id;
        advanceToSimStep();
      });
    },
  });
}

// Advance from a completed Pair 2 (player chosen) to the Sim step.
// The player chose Create-Sim or Load-Sim back at Pair 1 — route to the
// matching Sim screen now, carrying the selected player id forward.
function advanceToSimStep() {
  if (flowState.pair1Choice === 'create') {
    // → Sim Card Creator. On save, start the game with [new sim + player].
    showScreen('creator');
    flowState.step = 'sim-creator';
    renderCreator(screens.creator, {
      onSave: (cardId) => startFreshGame(cardId, flowState.selectedPlayerId),
    });
  } else {
    // → Sim Card Picker. On select, start the game with [picked sim + player].
    showScreen('picker');
    flowState.step = 'sim-picker';
    renderPicker(screens.picker, (card) => startFreshGame(card.id, flowState.selectedPlayerId));
  }
}

// ‹ (back) routing: depends on the current step.
function flowBack() {
  switch (flowState.step) {
    case 'player':            // Pair 2 → Pair 1
      returnToPair1();
      break;
    case 'player-creator':    // Player Creator → Pair 2
    case 'player-picker':
      returnToPair2();
      break;
    case 'sim-creator':       // Sim Creator → Pair 2 (player already chosen)
    case 'sim-picker':
      returnToPair2();
      break;
    default:
      exitFlowToTitle();
  }
}

// Return to Pair 1 (Create/Load Sim) — rebuild tiles + re-spawn.
function returnToPair1() {
  const tiles = rebuildSplitTiles([
    { glyphHtml: `<span class="fable-newgame-tile-glyph" aria-hidden="true">${CREATE_SIM_GLYPH_SVG}</span>`, caption: 'CREATE SIM CARD', act: 'create' },
    { glyphHtml: `<span class="fable-newgame-tile-glyph" aria-hidden="true">${LOAD_SIM_GLYPH_SVG}</span>`, caption: 'LOAD SIM CARD', act: 'existing' },
  ]);
  tiles[0].addEventListener('click', (e) => flowCreateSim(e.currentTarget));
  tiles[1].addEventListener('click', (e) => flowLoadSim(e.currentTarget));
  showScreen('newgame-split');
  flowState.step = 'sim';
  if (flowChrome) flowChrome.hideBack();
  tiles.forEach((t) => { t.style.opacity = '0'; });
  playReverseSpawn(tiles);
}

// Return to Pair 2 (Create/Load Player) — rebuild tiles + re-spawn.
function returnToPair2() {
  const tiles = rebuildSplitTiles([
    { glyphHtml: playerTileGlyphHtml(PLUS_BADGE_SVG), caption: 'CREATE PLAYER', act: 'create-player' },
    { glyphHtml: playerTileGlyphHtml(FOLDER_BADGE_SVG), caption: 'LOAD PLAYER', act: 'load-player' },
  ]);
  tiles[0].addEventListener('click', (e) => flowCreatePlayer(e.currentTarget));
  tiles[1].addEventListener('click', (e) => flowLoadPlayer(e.currentTarget));
  showScreen('newgame-split');
  flowState.step = 'player';
  if (flowChrome) flowChrome.showBack();
  tiles.forEach((t) => { t.style.opacity = '0'; });
  playReverseSpawn(tiles);
}

// Start a fresh game from a card (+ optional saved player): stop the
// title ambient + music, end any prior engine session, call fable_start
// with fresh:true (+ player_id), then enter the stage. The player_id
// attaches the saved player's identity onto the new game.
async function startFreshGame(cardId, playerId = null) {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  fadeOutThemeMusic(fableRoot);
  stopNewGameMusic(fableRoot);  // entering the stage — end the New Game ambience
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', { cardId, fresh: true, playerId });
    engineStarted = true;
    if (result && result.opening_scene) openingScene = result.opening_scene;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_start (new game) failed — entering stage without engine', err);
    engineStarted = false;
  }

  enterStageViaTransition(openingScene, loadMessages);
}

// === Quick Play =========================================================
//
// A single-button "throw me in" path. Runs the placeless Narrative Simulator
// (data/fable.sim) under a fixed __quickplay__ card id with ONE quicksave
// slot, auto-written on fable_end (Home/Exit). Invisible to New Game / Load /
// Continue. If a quicksave exists, the player chooses Start New (overwrites)
// vs Resume Last; otherwise a fresh run starts immediately.

// QUICK PLAY button handler. Reads the stashed quicksave state
// (title._quickPlaySave, refreshed on every title show via
// _refreshQuickPlay). No quicksave → straight into a fresh run. Quicksave
// present → an inline Start-New / Resume-Last choice overlay.
function onQuickPlayClicked() {
  const save = (screens.title && screens.title._quickPlaySave) || null;
  if (save) {
    showQuickPlayChoice(save);
  } else {
    startQuickPlayNew();
  }
}

// The inline Start-New / Resume-Last overlay. A centered card (reuses the
// save-modal visual language) offering two actions: Start New (overwrites the
// old quicksave) or Resume Last (loads it). Built transiently on the title
// screen + torn down on either choice or Esc/backdrop — no separate screen,
// no new file. The summary/timestamp give the player enough to recognize the
// saved run before overwriting it.
function showQuickPlayChoice(save) {
  // Tear down any prior choice overlay (idempotent re-entry).
  hideQuickPlayChoice();
  const overlay = document.createElement('div');
  overlay.className = 'fable-save-overlay fable-quickplay-choice';
  overlay.dataset.quickplayChoice = '';
  overlay.innerHTML = `
    <div class="fable-save-backdrop" data-qp-backdrop></div>
    <div class="fable-save-modal">
      <h2 class="fable-save-title">Quick Play</h2>
      <p class="fable-quickplay-summary"></p>
      <div class="fable-save-actions">
        <button class="fable-save-btn ghost" data-qp-new>Start New</button>
        <button class="fable-save-btn primary" data-qp-resume>Resume Last</button>
      </div>
      <button class="fable-save-close" data-qp-close aria-label="Close">✕</button>
    </div>
  `;
  // Surface the saved run's summary so overwriting it is an informed choice.
  const summaryEl = overlay.querySelector('.fable-quickplay-summary');
  if (summaryEl) {
    const text = save.summary && save.summary.trim()
      ? save.summary
      : 'A previous Quick Play run awaits.';
    summaryEl.textContent = text;
  }
  const dismiss = () => hideQuickPlayChoice();
  overlay.querySelector('[data-qp-new]').addEventListener('click', () => { dismiss(); startQuickPlayNew(); });
  overlay.querySelector('[data-qp-resume]').addEventListener('click', () => { dismiss(); resumeQuickPlay(); });
  overlay.querySelector('[data-qp-close]').addEventListener('click', dismiss);
  overlay.querySelector('[data-qp-backdrop]').addEventListener('click', dismiss);
  // Esc dismiss (one-shot listener — removed on hide).
  const onEsc = (e) => { if (e.key === 'Escape') { dismiss(); } };
  overlay.addEventListener('remove-choice', () => document.removeEventListener('keydown', onEsc, true), { once: true });
  document.addEventListener('keydown', onEsc, true);
  if (screens.title) screens.title.appendChild(overlay);
}

function hideQuickPlayChoice() {
  const existing = document.querySelector('[data-quickplay-choice]');
  if (existing) {
    existing.dispatchEvent(new CustomEvent('remove-choice'));
    existing.remove();
  }
}

// Start a brand-new Quick Play run: stop ambient/music, end any prior
// session, call fable_quick_play_start (loads data/fable.sim, wipes the old
// quicksave, seats the card at __quickplay__, fresh state), then enter the
// stage flagged as Quick Play (Save/Load footer suppressed).
async function startQuickPlayNew() {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  fadeOutThemeMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_quick_play_start');
    engineStarted = true;
    isQuickPlaySession = true;
    if (result && result.opening_scene) openingScene = result.opening_scene;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_quick_play_start failed — entering stage without engine', err);
    engineStarted = false;
    isQuickPlaySession = true;
    try { toast('Quick Play narrator card missing or malformed (data/fable.sim).'); } catch (_) {}
  }

  enterStageViaTransition(openingScene, loadMessages);
}

// Resume the last Quick Play quicksave: mirrors startQuickPlayNew but calls
// fable_quick_play_resume (loads the bundled card + session/schema from the
// single quicksave slot). The frontend only reaches here after
// title._quickPlaySave confirmed a quicksave exists.
async function resumeQuickPlay() {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  fadeOutThemeMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_quick_play_resume');
    engineStarted = true;
    isQuickPlaySession = true;
    // A resumed run has no fresh opening scene — its history is in messages.
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_quick_play_resume failed — entering stage without engine', err);
    engineStarted = false;
    isQuickPlaySession = true;
    try { toast('Could not resume Quick Play — the quicksave may be corrupt.'); } catch (_) {}
  }

  enterStageViaTransition(openingScene, loadMessages);
}

// The shared "play the magical transition + swap to stage + wire it" tail.
// Used by every start/resume path.
function enterStageViaTransition(openingScene, loadMessages) {
  // Leaving the New Game flow for the stage — hide the flow chrome entirely
  // so ‹ / ⌂ don't linger over the stage (the Wupi top bar owns stage nav).
  if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
  playMagicalTransition({
    onMidpoint: () => {
      showScreen('stage');
      try {
        if (screens.stage) {
          wireStage(screens.stage, {
            cardContext: null,
            onExit: returnToTitle,
            openingScene,
            loadMessages,
            isQuickPlay: isQuickPlaySession,
          });
        }
      } catch (e) {
        console.error('[fable] wireStage threw (stage shown, some features may degrade)', e);
      }
      if (!engineStarted) {
        try { toast('Simulation engine unavailable — chat will not respond.'); } catch (_) {}
      }
    },
  }).catch((e) => {
    console.error('[fable] magical transition failed, jumping to stage', e);
    showScreen('stage');
    try {
      if (screens.stage) {
        wireStage(screens.stage, {
          cardContext: null,
          onExit: returnToTitle,
          openingScene,
          loadMessages,
          isQuickPlay: isQuickPlaySession,
        });
      }
    } catch (e) { console.error('[fable] wireStage threw on fallback', e); }
  });
}

// Return from the stage to the Fable title screen. Wired as the stage's
// `onExit` hook (the Home button in the Wupi drawer footer calls it).
// Tears down the stage's engine modules + listeners so re-entry is clean,
// shuts down the FableEngine (the load-bearing fix: prior version leaked the
// engine — the next fable_start would error "a game is already running"),
// then swaps back to the title + restarts the title ambient + music.
// No magical transition here — the return is instant (the user is leaving
// a game, not entering one; the cinematic is for entry only).
async function returnToTitle() {
  // Shut down the FableEngine BEFORE teardown so the narrator thread is gone
  // by the time wireStage nulls its refs. fable_end persists the session +
  // schema per-card first (best-effort), then joins the engine thread +
  // restores the pre-game active_card_id server-side. Idempotent + safe if
  // the engine never started (engineStarted gate avoids a needless IPC
  // round-trip on the no-engine degrade path).
  if (engineStarted) {
    try { await invoke('fable_end'); } catch (e) {
      console.error('[fable] fable_end on return-to-title failed', e);
    }
    engineStarted = false;
  }
  isQuickPlaySession = false;  // reset so the next entry isn't mis-flagged
  teardownStage();
  showScreen('title');
}

// === App-lifecycle callbacks ==========================================
// These are registered with AppLifecycle in initFable(). The manager fires
// them at the right OS moments: onOpen on launch, onPause/onResume on
// alt-tab focus loss/return, onClose on EXIT.

// onOpen: show the app, hand the screen to Fable, ignite the title, run
// the boot transition (2s paused welcome → ripple + button stagger).
//
// NO FOG GATE (removed 2026-07-26): the prior design ran a 2s OS-layer
// fog buildup + wind noise over the desktop before opening Fable, then a
// 5.5s cloud-part handoff. That's all gone — Fable now opens immediately
// on click. The boot transition is responsible for the deliberate 2s
// pause (buttons hidden, title art visible) before the music + ripple +
// button reveal. See screens/boot.js for the timeline.
function openFable() {
  if (!fableRoot) return;
  // Hide the title buttons BEFORE the screen shows so they never flash
  // visible for one frame before the boot transition hides them. They stay
  // hidden through boot's REVEAL_DELAY pause.
  if (screens.title) {
    screens.title.querySelectorAll('.fable-title-btn').forEach((b) => {
      b.classList.add('fable-title-btn--hidden');
    });
  }
  fableRoot.classList.add('show');
  fableRoot.setAttribute('aria-hidden', 'false');
  activateChrome();
  if (hooks.pauseAurora) hooks.pauseAurora();
  showScreen('title');

  // ── DEV SHORTCUT (?dev=fable): skip the fog gate + 2s boot transition. ──
  // The buttons were left hidden above for the cinematic path; here we
  // force-reveal them immediately + start the theme music, then return BEFORE
  // the fog/boot setup. Title screen is ready to click in <1s. This mirrors
  // boot.js's own prefers-reduced-motion fast path (music on, buttons shown).
  // No fog node, no ripple aura, no boot timers → nothing for closeFable to
  // cancel; currentFable + currentBoot stay null.
  if (DEV_FABLE_SHORTCUT) {
    if (screens.title) {
      screens.title.querySelectorAll('.fable-title-btn--hidden').forEach((b) => {
        b.classList.remove('fable-title-btn--hidden');
      });
    }
    try { startThemeMusic(fableRoot); } catch (_) {}
    return;
  }

  // The ripple aura anchors on the QUICK PLAY button (the middle of the
  // stack), and the buttons reveal radiating outward from it: Quick Play
  // first, then its neighbors New Game + Load together, then Continue +
  // Exit (the outermost) last. The visual order top→bottom is: Continue,
  // New Game, Quick Play, Load, Exit — so Quick Play sits in the middle and
  // the groups are symmetric around it.
  const q = (act) => screens.title ? screens.title.querySelector(`[data-act="${act}"]`) : null;
  const allButtons = screens.title
    ? Array.from(screens.title.querySelectorAll('.fable-title-btn'))
    : [];
  // Each entry is a group revealed at one stagger beat. Order: center →
  // neighbors → outermost.
  const revealGroups = [
    [q('quickplay')],            // 1st: the ripple anchor itself
    [q('new'), q('load')],       // 2nd: its immediate neighbors
    [q('continue'), q('exit')],  // 3rd: the outermost pair
  ].map((group) => group.filter(Boolean))
   .filter((group) => group.length > 0);

  // FOG GATE (re-added 2026-08-01): the title screen is already shown behind
  // the fog; the fog overlay (z:7000, on document.body) covers it for a 2s
  // hold with wind looping + the fog drifting, then a soft left→right
  // feathered wipe reveals the menu. The boot transition (music + ripple +
  // button cascade from Quick Play) fires AFTER the fog clears so its beat
  // isn't hidden under the fog. The fog's `done` promise resolves on clear;
  // cancel() is available for teardown if the app closes mid-intro.
  const fog = playFogIntro();
  currentFog = fog;
  fog.done.then(() => {
    if (currentFog !== fog) return;  // a newer open / cancel superseded us
    currentFog = null;
    currentBoot = playBootTransition({
      fableRoot,
      titleScreen: screens.title,
      musicHost: fableRoot,
      rippleAnchorBtn: q('quickplay'),
      allButtons,
      revealOrder: revealGroups,
    });
  });
}

// PUBLIC: the entry point the OS calls when the user clicks the Fable tile.
// Launches Fable directly through the lifecycle manager (which fires
// onOpen = openFable). This is what script.js wires the home tile + dock
// click to (replacing the direct openWindow('fable')).
//
// Fog gate (re-added 2026-08-01): a fog intro now plays INSIDE openFable
// (after the title screen is shown), NOT as an OS-layer pre-launch gate.
// The fog overlay covers the title for 3s with wind looping, then blows off
// the right edge to reveal the menu; the boot transition fires after it
// clears. launchFable itself stays immediate.
export function launchFable() {
  AppLifecycle.launchApp('fable');
}

// onPause: the resource-freeze layer (alt-tab / focus-loss). Freezes the
// title theme music + (if the stage is active) the stage parallax + CSS FX
// particles. The title canvas particle RAF self-pauses on document.hidden
// (see particles.js's onVisibility), so nothing to do there. The narrator
// streaming path is event-driven (no JS RAF), so no loop to stop.
// Idempotent — safe to fire repeatedly.
function pauseFable() {
  // Title music: pause in place (node stays mounted for resume). On the
  // stage there's no theme music (ambient music was wiped in Phase 0a), so
  // pauseThemeMusic is a safe no-op there.
  pauseThemeMusic(fableRoot);
  // New Game tracks: same pause-in-place treatment so alt-tab doesn't leave
  // them playing while Fable is hidden.
  pauseNewGameMusic(fableRoot);
  if (stageActive) {
    detachParallax();   // stop the parallax RAF throttle
    pauseFX();          // freeze CSS particle animations
  }
}

// onResume: the focus-return mirror of onPause. Unfreezes music + stage
// parallax + FX. Best-effort (play() can reject under autoplay policy;
// pauseFX/resumeFX + attach/detachParallax are idempotent).
function resumeFable() {
  resumeThemeMusic(fableRoot);
  resumeNewGameMusic(fableRoot);
  if (stageActive) {
    resumeFX();
    attachParallax();
  }
}

// onClose: the ONE full teardown, fired by AppLifecycle.closeApp('fable')
// when the user clicks EXIT on the title screen. Leaves the host pristine
// for the next open — no stale audio nodes, no leaked RAF, no residual
// listeners. This is the load-bearing reset against the relaunch bug.
//
// This runs INSIDE AppLifecycle.closeApp's transitioning guard, so calling
// closeApp again from here (the closeWindow re-entry below) is a safe no-op
// at the manager level. We keep the OS set in sync by routing through the
// OS closeWindow if Fable was launched via the home tile.
function closeFable() {
  if (!fableRoot) return;
  // The chrome-restore (deactivateChrome + resumeAurora) MUST run on every
  // exit path, even if an earlier teardown step throws — otherwise
  // body.fable-active could stay applied (hiding the OS dock permanently)
  // and the aurora RAF could stay paused. So we restore the OS-side state
  // in a finally, and hide the window last. The stage/particles/music
  // teardown is best-effort in try; a throw there can't strand the OS.
  // (Boundary audit gap #2, 2026-07-23.)
  try {
    // Cancel the fog intro FIRST. If the user clicks EXIT during the 3s fog
    // hold, this tears down the fog overlay + stops the wind audio so the
    // next open isn't stranded with a leftover fog node or orphan audio.
    // currentFog is set null BEFORE cancel so the .done chain no-ops too.
    if (currentFog) { const f = currentFog; currentFog = null; try { f.cancel(); } catch (_) {} }
    // Cancel the boot transition. If the user clicks EXIT during the
    // ~4s welcome (e.g. mid-pause at t=1s), this stops the ripple SFX,
    // removes the aura node, and force-reveals the title buttons — so the
    // next open isn't stranded with invisible buttons or a leftover aura
    // from a half-finished prior transition.
    if (currentBoot) { try { currentBoot.cancel(); } catch (_) {} currentBoot = null; }
    teardownStage();
    // Stop every screen's ambient canvas system (title motes/grass + the
    // New Game flow's embers) so their RAF + listeners don't outlive the
    // app — the load-bearing reset against the relaunch leak. Sweeps all
    // screens so a teardown isn't missed regardless of which screen was
    // showing when EXIT was clicked.
    for (const scr of Object.values(screens)) {
      if (scr && scr._stopAmbient) scr._stopAmbient();
    }
    stopThemeMusic(fableRoot);
    // New Game tracks: immediate teardown (no fade — the app is closing, no
    // point in a 1.5s fade-out after the window is gone).
    stopNewGameMusic(fableRoot, { immediate: true });
  } catch (err) {
    console.error('[fable] teardown threw, continuing with OS restore', err);
  } finally {
    // OS-side restore — always runs. deactivateChrome removes
    // body.fable-active + the peek listener + peekTimer; resumeAurora
    // hands the canvas back to the OS. These must NOT be skipped.
    try { deactivateChrome(); } catch (_) {}
    if (hooks.resumeAurora) { try { hooks.resumeAurora(); } catch (_) {} }
  }
  invoke('fable_end').catch(() => {});
  engineStarted = false;  // mirror the Rust slot clear so returnToTitle's gate stays honest
  isQuickPlaySession = false;  // reset alongside the engine flag
  stageActive = false;
  fableRoot.classList.remove('show');
  fableRoot.setAttribute('aria-hidden', 'true');

  // Keep the OS openWindows set in sync. When launched via the home tile,
  // 'fable' is in the set; the EXIT button bypasses closeWindow(), so
  // without this the next tile click would hit openWindow()'s "already
  // open, just raise" early-return and the app wouldn't re-show. Routing
  // through closeWindow('fable') fires THIS closeFable again via the OS
  // close hook — but AppLifecycle.closeApp's transitioning guard makes
  // that a no-op (the descriptor's onClose isn't re-invoked). Safe.
  if (hooks.closeWindow) {
    try { hooks.closeWindow('fable'); } catch (_) {}
  }
}

// === Public init: called once from script.js boot ===
export function initFable(extHooks = {}) {
  hooks = Object.assign(hooks, extHooks);

  // Build the #fable app window. This is the minimal shell; all
  // inner structure is generated by the screen builders.
  fableRoot = document.createElement('div');
  fableRoot.className = 'app-window fable-app';
  fableRoot.id = 'fable';
  fableRoot.setAttribute('aria-hidden', 'true');
  document.body.appendChild(fableRoot);

  // Build the screens we use. New Game (picker) + Continue/Load
  // (worlds + saves pickers) are all wired; the stage stays built
  // (teardownStage needs it on close too).
  screens.title = buildTitle({
    // NEW GAME: the card picker — pick a shipped .sim → straight to the stage
    // at a fresh game (no interview). DISABLED on the button until Chloe wires
    // it up; this handler stays ready for when it's re-enabled.
    new: () => onNewGameClicked(),
    // QUICK PLAY: throw the player straight into the placeless Narrative
    // Simulator. If a quicksave exists (checked via title._quickPlaySave,
    // refreshed on every title show), an inline Start-New / Resume-Last choice
    // appears; otherwise it goes straight into a fresh run.
    quickplay: () => onQuickPlayClicked(),
    // CONTINUE: resume the freshest New Game save (autosave-inclusive).
    // Target stashed by title._refreshContinue. Quick Play is excluded.
    continue: () => onContinueClicked(),
    // LOAD: two-level picker — worlds screen → saves screen → resumeSave.
    load: () => onLoadClicked(),
    // EXIT is the ONLY real close path — routes through the lifecycle
    // manager for full teardown.
    exit: () => AppLifecycle.closeApp('fable'),
  });
  screens.stage = buildStage();
  // The New Game split (Pair 1: Create/Load Sim). The flow controller
  // (fable.js) rebuilds the split's tiles + wires clicks directly on
  // each entry (so it can pass the clicked button to the burn), so the
  // builder's own handlers are unused. Built once; tiles are rebuilt
  // per-step via rebuildSplitTiles().
  screens['newgame-split'] = buildNewGameSplit({
    useExisting: () => {},
    createNew: () => {},
  });
  screens.picker = buildPicker({});
  screens.creator = buildCreator();
  // Player Creator + Picker (Pair 2). Built once; rendered on entry.
  screens['player-creator'] = buildPlayerCreator();
  screens['player-picker'] = buildPlayerPicker();
  screens.worlds = buildWorlds({ back: () => showScreen('title') });
  screens.saves = buildSaves({ back: () => showScreen('worlds') });
  for (const s of Object.values(screens)) fableRoot.appendChild(s);
  showScreen('title');

  // Register Fable as a full-screen OS app under the lifecycle framework.
  // The manager owns onOpen/onPause/onResume/onClose; the OS window set is
  // kept in sync by routing closeFable through hooks.closeWindow above.
  AppLifecycle.registerApp({
    id: 'fable',
    onOpen: openFable,
    onClose: closeFable,
    onPause: pauseFable,
    onResume: resumeFable,
  });

  // Bridge the OS window system to launchFable. The home-grid Fable tile +
  // the dev #fable shortcut call openWindow('fable'), which fires the
  // openHook. We redirect it to launchFable() → AppLifecycle.launchApp
  // → onOpen=openFable → boot transition (the 2s paused welcome lives in
  // boot.js now, not in a pre-launch fog gate).
  if (hooks.openHooks) hooks.openHooks.set('fable', () => launchFable());
  if (hooks.closeHooks) {
    hooks.closeHooks.set('fable', () => {
      // Only route through the manager if we're not already mid-close
      // (closeFable itself calls closeWindow, which would re-enter here).
      // AppLifecycle.closeApp's transitioning guard already makes the
      // re-entry a no-op, so the direct call is safe.
      AppLifecycle.closeApp('fable');
    });
  }
}

// Exposed for the stage's pause menu + toast (dev/debug convenience).
export { toast };
