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
// MENU STATE: the 4 menu flows (New Game / Continue / Load / Quick Play)
// are currently NO-OPS — their buttons stay in the DOM but do nothing. The
// entire title-screen UI for those flows is being redone; the working stage
// + gameplay engine (stage.js, engine/*, fx/*, panels/*) stay on disk,
// fully functional, just temporarily unreachable from this menu.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { AppLifecycle } from '../app-lifecycle.js';
import './fable.css';

import { buildTitle } from './screens/title.js';
import { buildStage, wireStage, teardownStage, toast } from './screens/stage.js';
import { buildVoid, wireVoid, teardownVoid } from './screens/void.js';
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

// === Quick Play flow (the live authoring path, 2026-07-26) ================
//
// Clicking Quick Play on the title screen kicks off the interview-based
// authoring flow:
//   - No quicksave exists  → straight into the void interview.
//   - Quicksave exists     → inline Resume / Start New choice on the title.
//       Resume    → load the quicksave, enter the stage (magical transition).
//       Start New → wipe the old quicksave (fable_quick_reset), then interview.
//
// The void interview (screens/void.js) is a pure-black infinite-space
// surface where the user is greeted by fading large text, then asked four
// fixed questions one at a time (character / setting / plot / extra). The
// questions fade in/out like magic; the user types + hits Enter (no send
// button — Enter only, per Chloe's directive). After the last answer the
// UI disappears and the user sits in the void while the backend runs a
// SINGLE interview_generate call that produces three tagged blocks: a
// <sim_card>, a <world_schema>, and a <player_state>. On done, fable_quick_start
// seats the card + seeds the schema/player state (no .sim on disk; the
// card is bundled inside the quicksave) → the void fades to black → the
// stage is revealed with the card's opening scene as the first narrator beat.
//
// The interview is MEMORYLESS server-side (interview_generate archives
// nothing), so the conversation disappears the moment the user begins — by
// construction, not by erasure. Quick Play is single-slot quicksave only:
// the manual Save + Load footer buttons are disabled in the stage (driven
// by isQuickPlay), and starting a new Quick Play wipes the old quicksave +
// memory entirely.
//
// New Game is DISABLED on the title (its dedicated interview is future
// work). Continue + Load are still no-ops (their flows are also future work,
// separate from Quick Play).

// Track whether the engine started for the current stage session, so
// returnToTitle knows whether to call fable_end (no-op call is wasteful +
// noisy in logs). Reset to false whenever we leave the stage.
let engineStarted = false;

// Track whether the current stage session is a Quick Play session, so
// returnToTitle + closeFable know whether to call teardownVoid (only the
// Quick Play path enters the void; the manual-card path doesn't). Also
// drives the wireStage isQuickPlay flag for Save/Load disabling.
let inQuickPlay = false;
// Track whether we're currently in the void interview, so closeFable can
// tear it down cleanly on EXIT mid-interview.
let inVoid = false;

// The Quick Play button on the title screen. Branches on whether a quicksave
// exists (inline Resume/Start-New choice) or not (straight into the interview).
async function onQuickPlayClicked() {
  let exists = false;
  try {
    exists = await invoke('fable_quick_exists');
  } catch (err) {
    console.error('[fable] fable_quick_exists failed — assuming no quicksave', err);
    exists = false;
  }
  if (exists) {
    showQuickPlayChoiceOverlay({
      onResume: quickPlayResume,
      onStartNew: async () => {
        try { await invoke('fable_quick_reset'); } catch (err) {
          console.error('[fable] fable_quick_reset failed (continuing to interview)', err);
        }
        enterVoidInterview();
      },
    });
  } else {
    enterVoidInterview();
  }
}

// Resume the existing quicksave: load it server-side, then enter the stage
// via the magical transition. Mirrors the stage-entry tail (enterStageVia-
// Transition) but uses fable_quick_resume + sets isQuickPlay.
async function quickPlayResume() {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  stopThemeMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_quick_resume');
    engineStarted = true;
    inQuickPlay = true;
    if (result && result.opening_scene) openingScene = result.opening_scene;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_quick_resume failed — entering stage without engine', err);
    engineStarted = false;
  }

  enterStageViaTransition(openingScene, loadMessages, /* isQuickPlay */ true);
}

// Enter the void interview: stop the title ambient + music, play the magical
// transition, swap to the void screen at midpoint, wire it with the begin
// callback that hands off to quickPlayBegin.
function enterVoidInterview() {
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  stopThemeMusic(fableRoot);
  try { invoke('fable_end').catch(() => {}); } catch (_) {}

  inVoid = true;
  playMagicalTransition({
    onMidpoint: () => {
      showScreen('void');
      try {
        if (screens.void) {
          wireVoid(screens.void, { onBegin: quickPlayBegin });
        }
      } catch (e) {
        console.error('[fable] wireVoid threw', e);
      }
    },
  }).catch((e) => {
    console.error('[fable] magical transition to void failed, jumping', e);
    showScreen('void');
    try {
      if (screens.void) wireVoid(screens.void, { onBegin: quickPlayBegin });
    } catch (e) { console.error('[fable] wireVoid threw on fallback', e); }
  });
}

// The void's onBegin callback. By the time this fires, the void has already
// faded to black + interview_generate has produced the card + the seeded
// world/player state. fable_quick_start seats the card server-side
// (overriding its id to the quickplay sentinel + wiping the prior quick
// play's state + seeding the world schema + player state from the
// generation output), then we swap to the stage + wire it (the swap is
// invisible — the void's overlay is still up). The stage is revealed as
// the void overlay undims.
async function quickPlayBegin(card, worldSchema, playerState) {
  let result = null;
  try {
    result = await invoke('fable_quick_start', {
      card,
      worldSchema: worldSchema || null,
      playerState: playerState || null,
    });
    engineStarted = true;
    inQuickPlay = true;
  } catch (err) {
    console.error('[fable] fable_quick_start failed', err);
    engineStarted = false;
  }
  // Tear down the void (cancel particles, drop interview history). The void
  // DOM stays mounted (reused next Quick Play); only its runtime state goes.
  teardownVoid();
  inVoid = false;

  const openingScene = (result && result.opening_scene) || null;
  const loadMessages = (result && Array.isArray(result.messages) && result.messages.length)
    ? result.messages : null;

  // Swap to the stage INVISIBLY (the void's overlay is still dimming). No
  // second transition here — the void's fade-to-black IS the handoff.
  showScreen('stage');
  try {
    if (screens.stage) {
      wireStage(screens.stage, {
        cardContext: null,
        onExit: returnToTitle,
        isQuickPlay: true,
        openingScene,
        loadMessages,
      });
    }
  } catch (e) {
    console.error('[fable] wireStage threw after quick-start', e);
  }
  if (!engineStarted) {
    try { toast('Simulation engine unavailable — chat will not respond.'); } catch (_) {}
  }
}

// The shared "play the magical transition + swap to stage + wire it" tail.
// Used by the Quick Play resume path. The fresh-interview path
// (quickPlayBegin) skips this because the void's own fade-to-black is its
// handoff (no second transition).
function enterStageViaTransition(openingScene, loadMessages, isQuickPlay) {
  playMagicalTransition({
    onMidpoint: () => {
      showScreen('stage');
      try {
        if (screens.stage) {
          wireStage(screens.stage, {
            cardContext: null,
            onExit: returnToTitle,
            isQuickPlay: !!isQuickPlay,
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
          isQuickPlay: !!isQuickPlay,
          openingScene,
          loadMessages,
        });
      }
    } catch (e) { console.error('[fable] wireStage threw on fallback', e); }
  });
}

// ── Inline Quick Play choice overlay ───────────────────────────────────
// A small centered modal on the title screen: "Resume your Quick Play?" +
// two buttons (Resume / Start New). Esc + backdrop dismiss. Built transient
// (appended to the title screen, removed on choice/dismiss).
function showQuickPlayChoiceOverlay({ onResume, onStartNew }) {
  if (!screens.title) { onResume && onResume(); return; }
  // Remove any prior overlay (idempotent — defensive against a double-click).
  const prior = screens.title.querySelector('.fable-quick-choice');
  if (prior) prior.remove();

  const overlay = document.createElement('div');
  overlay.className = 'fable-quick-choice';
  overlay.innerHTML = `
    <div class="fable-quick-choice-backdrop"></div>
    <div class="fable-quick-choice-modal">
      <h2 class="fable-quick-choice-title">Resume your Quick Play?</h2>
      <p class="fable-quick-choice-sub">A Quick Play save exists. Continue it, or start fresh (the old save will be erased).</p>
      <div class="fable-quick-choice-actions">
        <button class="fable-quick-choice-btn primary" data-qp-resume>Resume</button>
        <button class="fable-quick-choice-btn ghost" data-qp-new>Start New</button>
      </div>
      <button class="fable-quick-choice-close" data-qp-close aria-label="Close">✕</button>
    </div>
  `;
  screens.title.appendChild(overlay);

  const close = () => overlay.remove();
  const onKey = (e) => {
    if (e.key === 'Escape') { close(); document.removeEventListener('keydown', onKey, true); }
  };
  document.addEventListener('keydown', onKey, true);
  overlay.querySelector('.fable-quick-choice-backdrop').addEventListener('click', close);
  overlay.querySelector('[data-qp-close]').addEventListener('click', () => {
    close(); document.removeEventListener('keydown', onKey, true);
  });
  overlay.querySelector('[data-qp-resume]').addEventListener('click', () => {
    document.removeEventListener('keydown', onKey, true);
    overlay.remove();
    onResume && onResume();
  });
  overlay.querySelector('[data-qp-new]').addEventListener('click', () => {
    document.removeEventListener('keydown', onKey, true);
    overlay.remove();
    onStartNew && onStartNew();
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
  // restores the pre-game active_card_id + resets is_quick_play server-side.
  // Idempotent + safe if the engine never started (engineStarted gate avoids
  // a needless IPC round-trip on the no-engine degrade path).
  if (engineStarted) {
    try { await invoke('fable_end'); } catch (e) {
      console.error('[fable] fable_end on return-to-title failed', e);
    }
    engineStarted = false;
  }
  // Reset the Quick Play flag on the frontend mirror too. The Rust side
  // resets it in fable_end; this keeps the two in sync so the next Quick
  // Play (or a future manual-card game) starts from a known-clean state.
  inQuickPlay = false;
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

  // The ripple aura anchors on the Quick Play button; buttons reveal in
  // order: Quick Play → New Game → Load → Continue → Exit.
  const q = (act) => screens.title ? screens.title.querySelector(`[data-act="${act}"]`) : null;
  const allButtons = screens.title
    ? Array.from(screens.title.querySelectorAll('.fable-title-btn'))
    : [];
  const revealOrder = ['quickplay', 'new', 'load', 'continue', 'exit']
    .map(q)
    .filter(Boolean);
  currentBoot = playBootTransition({
    fableRoot,
    titleScreen: screens.title,
    musicHost: fableRoot,
    rippleAnchorBtn: q('quickplay'),
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
    // Tear down the void if the user EXITs mid-interview (the void screen's
    // particles + listeners would leak otherwise). Safe no-op if not in void.
    if (inVoid) { teardownVoid(); inVoid = false; }
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
  inQuickPlay = false;    // reset the Quick Play flag on full close
  inVoid = false;
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

  // Build the screens we still use. The 4 menu flows (picker / saves /
  // interview / loading) are no-ops for now — their UI is being redone, so
  // we don't build them. The stage stays built (teardownStage needs it on
  // close even though the menu can't reach it yet).
  screens.title = buildTitle({
    // NEW GAME is disabled on the title (its dedicated interview is future
    // work, separate from Quick Play). The handler stays a no-op for
    // belt-and-suspenders (the button itself is `disabled` so the click
    // never fires; this is the backstop).
    new: () => {},
    // QUICK PLAY is the live authoring flow: void interview → GM sketch →
    // Begin → stage. Branches on whether a quicksave exists (inline choice)
    // or not (straight into the interview).
    quickplay: () => onQuickPlayClicked(),
    // CONTINUE + LOAD stay no-ops: their flows (resume target / save picker)
    // are being rebuilt separately from Quick Play.
    continue: () => {},
    load: () => {},
    // EXIT is the ONLY real close path — routes through the lifecycle
    // manager for full teardown.
    exit: () => AppLifecycle.closeApp('fable'),
  });
  screens.stage = buildStage();
  screens.void = buildVoid();
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
