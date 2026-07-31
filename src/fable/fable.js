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

import { buildTitle } from './screens/title.js';
import { buildStage, wireStage, teardownStage, toast } from './screens/stage.js';
import { buildPicker, renderPicker } from './screens/picker.js';
import { buildWorlds, renderWorlds } from './screens/worlds.js';
import { buildSaves, renderSaves } from './screens/saves.js';
import {
  startThemeMusic, stopThemeMusic, pauseThemeMusic, resumeThemeMusic,
} from './screens/reveal.js';
import { playBootTransition } from './screens/boot.js';
import { pauseFX, resumeFX } from './fx/effects.js';
import { detachParallax, attachParallax } from './fx/atmosphere.js';
import { activateChrome, deactivateChrome } from './engine/chrome.js';
import { playMagicalTransition } from './engine/transition.js';

let fableRoot = null;       // the #fable app-window element
let screens = {};           // name → screen element
// Whether the stage screen is currently the active screen. Drives the
// pause/resume split: title freezes music, stage freezes parallax + FX.
let stageActive = false;
// The active boot-transition controller (null when no transition is
// running). closeFable cancels it FIRST so an EXIT during the cinematic
// can't leak a fog node, orphan audio, or strand invisible buttons.
let currentBoot = null;

// External hooks set by script.js (the OS chrome integration).
// closeWindow: ref to the OS closeWindow() so Fable's own close paths
// (Exit button) keep the openWindows set in sync.
let hooks = { pauseAurora: null, resumeAurora: null, openHooks: null, closeHooks: null, closeWindow: null };

// Screen router: hide all, show one. Also drives the title's particle
// system lifecycle: the floating motes run ONLY while the title is the
// visible screen (created on show, destroyed on hide) so the canvas RAF
// never leaks across screen changes or exit/relaunch.
function showScreen(name) {
  for (const key of Object.keys(screens)) {
    const showing = (key === name);
    screens[key].hidden = !showing;
    if (key === 'title' && screens.title) {
      if (showing && screens.title._startAmbient) screens.title._startAmbient();
      else if (!showing && screens.title._stopAmbient) screens.title._stopAmbient();
    }
  }
  // Track whether the stage owns the screen (pause/resume + onClose branch
  // on this to know whether stage-specific teardown is needed).
  stageActive = (name === 'stage' && !!screens.stage);
}

// Track whether the engine started for the current stage session, so
// returnToTitle knows whether to call fable_end (no-op call is wasteful +
// noisy in logs). Reset to false whenever we leave the stage.
let engineStarted = false;

// === Continue / Load / New Game flows ====================================
//
// CONTINUE: resume the freshest save for ANY world (the title's
// _refreshContinue stashes the target from fable_continue_target). Autosaves
// are included (the per-turn checkpoint is "where you left off"). The stashed
// target carries both card_id + save_id, so this is a one-shot resume.
//
// LOAD: a two-level picker — choose a world (screens/worlds.js), then choose
// a save in that world (screens/saves.js). Both feed into resumeSave.
//
// NEW GAME: a one-level picker — choose a shipped .sim card
// (screens/picker.js) → start a FRESH game from it. No interview, no draft.
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
  stopThemeMusic(fableRoot);
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

// NEW GAME button handler: show the card picker + populate it. Selecting a
// card starts a FRESH game (no save slot) via startFreshGame.
function onNewGameClicked() {
  showScreen('picker');
  renderPicker(screens.picker, (card) => startFreshGame(card.id));
}

// Start a fresh game from a card: stop the title ambient + music, end any
// prior engine session, call fable_start with fresh:true (seats the card +
// installs the fresh-game default world/player state), then enter the stage.
async function startFreshGame(cardId) {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  stopThemeMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', { cardId, fresh: true });
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

// The shared "play the magical transition + swap to stage + wire it" tail.
// Used by every start/resume path.
function enterStageViaTransition(openingScene, loadMessages) {
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

  // The ripple aura anchors on the New Game button; buttons reveal in
  // order: New Game → Load → Continue → Exit.
  const q = (act) => screens.title ? screens.title.querySelector(`[data-act="${act}"]`) : null;
  const allButtons = screens.title
    ? Array.from(screens.title.querySelectorAll('.fable-title-btn'))
    : [];
  const revealOrder = ['new', 'load', 'continue', 'exit']
    .map(q)
    .filter(Boolean);
  currentBoot = playBootTransition({
    fableRoot,
    titleScreen: screens.title,
    musicHost: fableRoot,
    rippleAnchorBtn: q('new'),
    allButtons,
    revealOrder,
  });
}

// PUBLIC: the entry point the OS calls when the user clicks the Fable tile.
// Launches Fable directly through the lifecycle manager (which fires
// onOpen = openFable). This is what script.js wires the home tile + dock
// click to (replacing the direct openWindow('fable')).
//
// No fog gate anymore (removed 2026-07-26): the prior version ran a 2s
// OS-layer fog buildup + wind before calling launchApp. Now we launch
// immediately; the boot transition owns the 2s paused-welcome beat.
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
    // Cancel the boot transition FIRST. If the user clicks EXIT during the
    // ~4s welcome (e.g. mid-pause at t=1s), this stops the ripple SFX,
    // removes the aura node, and force-reveals the title buttons — so the
    // next open isn't stranded with invisible buttons or a leftover aura
    // from a half-finished prior transition.
    if (currentBoot) { try { currentBoot.cancel(); } catch (_) {} currentBoot = null; }
    teardownStage();
    // Stop the title ambient canvas systems (motes + grass) so their RAF +
    // listeners don't outlive the app (the load-bearing reset against the
    // relaunch bug).
    if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
    stopThemeMusic(fableRoot);
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
    // at a fresh game (no interview).
    new: () => onNewGameClicked(),
    // CONTINUE: resume the freshest save (autosave-inclusive). Target stashed
    // by title._refreshContinue.
    continue: () => onContinueClicked(),
    // LOAD: two-level picker — worlds screen → saves screen → resumeSave.
    load: () => onLoadClicked(),
    // EXIT is the ONLY real close path — routes through the lifecycle
    // manager for full teardown.
    exit: () => AppLifecycle.closeApp('fable'),
  });
  screens.stage = buildStage();
  // The New Game card picker + the two-level Load picker. All Back buttons
  // return to the title. Hidden until their button is clicked.
  screens.picker = buildPicker({ back: () => showScreen('title') });
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
