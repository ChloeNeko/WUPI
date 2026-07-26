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
import {
  startThemeMusic, stopThemeMusic, pauseThemeMusic, resumeThemeMusic,
} from './screens/reveal.js';
import { playBootTransition } from './screens/boot.js';
import { openFogGate } from './screens/fog-gate.js';
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

// Enter the simulation engine (the stage) via the magical transition.
// Both "New Game" and "Quick Play" route here today. The card-authoring flow
// (interview) was removed for a from-scratch rewrite, so both buttons enter the
// stage against the starter scenario card `rusty_tavern` — no picker, no
// interview. The starter card ships at `apps/fable/cards/rusty_tavern.sim`.
//
// THE AI CONNECTION (the load-bearing wiring, 2026-07-26): this function now
// actually STARTS the FableEngine. The prior version entered the stage with
// no engine spawned, so `fable_send` errored "no game running" and the chat
// input hung forever. The flow:
//   1. `fable_end` (defensive — clears any leaked engine from a prior session
//      that didn't tear down cleanly).
//   2. `fable_start` with `fresh: true` → spawns the FableEngine, loads the
//      card, swaps `active_card_id`, seeds the narrator prompt. Returns the
//      opening scene + any prior messages.
//   3. The transition's midpoint swap calls `wireStage` with `openingScene`
//      + `loadMessages` so the first narrator beat renders immediately and a
//      resumed session re-populates the feed.
// On any backend error we still enter the stage (graceful degrade) + show a
// toast so the user knows the engine didn't start, rather than a silent hang.
//
// The transition itself: a magical chime + a 2s dim-to-black, scene swap at
// peak darkness, then 2s undim to the new scene. The midpoint swap
// (showScreen) is invisible because the overlay is fully opaque at that moment.
//
// The starter card id. Both New Game + Quick Play enter against this card
// until the new authoring flow lands a card picker. Mirrors the card filename
// at apps/fable/cards/rusty_tavern.sim (id derives from <identity><name>
// lowercased only when <metadata><id> is absent; this card declares it
// explicitly so the contract is stable regardless of the display name).
const STARTER_CARD_ID = 'rusty_tavern';

// Track whether the engine started for the current stage session, so
// returnToTitle knows whether to call fable_end (no-op call is wasteful +
// noisy in logs). Reset to false whenever we leave the stage.
let engineStarted = false;

async function enterStageMagically() {
  // Stop the title ambient systems NOW so they don't keep painting under the
  // dim (wasted RAF during the 4s transition). showScreen('stage') would
  // normally stop them via the title's _stopAmbient hook, but we swap the
  // screen at midpoint — stopping here avoids 2s of churn under the overlay.
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  // Stop the theme music the INSTANT the button is clicked so the magical
  // chime (fired inside the transition) plays alone, not mixed with the
  // looping track. Cutting the music here also gives the click a clear
  // audible "this did something" before the dim even starts.
  stopThemeMusic(fableRoot);

  // Start the engine BEFORE the transition's midpoint so the swap reveals a
  // stage that's already wired + ready to stream. `fresh: true` → a brand-new
  // run (the prior behavior; New Game semantics). The opening scene + any
  // resumed messages come back in the result; we hand them to wireStage at
  // the midpoint swap.
  //
  // Defensive `fable_end` first: if a prior session crashed or the EXIT path
  // didn't run (e.g. the user killed the window mid-game), AppState could
  // still hold a spawned FableEngine → fable_start returns "a game is already
  // running". fable_end is idempotent (returns Ok(()) if nothing's running),
  // so this just clears any stale state. Best-effort: errors swallowed.
  try { await invoke('fable_end'); } catch (_) {}
  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', {
      cardId: STARTER_CARD_ID,
      saveId: null,
      fresh: true,
    });
    engineStarted = true;
    if (result && result.meta) {
      // The card's opening scene is surfaced as `opening_scene_preview` on
      // FableCardMeta, but fable_start returns FableLoadResult (meta + the
      // resumed messages). The opening-scene preview isn't on SaveMeta, so we
      // surface it via a one-off field if the backend ever adds it; for now
      // we read it from the card-authoring seam below.
    }
    // Pull the opening scene preview from the card list (cheap: one IPC,
    // returns the first ~240 chars of the card's <opening_scene>). This is
    // what renders as the first narrator beat on a fresh game.
    try {
      const cards = await invoke('fable_cards_list');
      const card = (cards || []).find((c) => c.id === STARTER_CARD_ID);
      if (card && card.opening_scene_preview) {
        openingScene = card.opening_scene_preview;
      }
    } catch (_) {}
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_start failed — entering stage without engine', err);
    engineStarted = false;
    // Surface the failure via a toast AFTER the stage shows (toast() needs
    // the stage mounted). The stage still enters so the user isn't stranded
    // on the title; the input will error-beat on send instead of hanging.
  }

  playMagicalTransition({
    onMidpoint: () => {
      // The overlay is fully opaque here — swap the screen invisibly.
      // ORDER MATTERS: showScreen runs FIRST so the user is never stranded
      // on the title even if wireStage throws. wireStage initializes the
      // engine modules (narrator, wupi-drawer, stats-panel, FX, panel-
      // manager); a throw there used to block showScreen (the cause of the
      // "transition plays but stays on title" bug — map.js's setMapTheme
      // threw on an undefined atlas var). Now the screen swaps unconditionally;
      // a wireStage throw degrades gracefully (the stage shows but some
      // panels/input may not work) + logs to the console for diagnosis.
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
      // If the engine failed to start, surface a toast now that the stage is
      // mounted (toast() looks up [data-toast] off the stage element).
      if (!engineStarted) {
        try { toast('Simulation engine unavailable — chat will not respond.'); } catch (_) {}
      }
    },
  }).catch((e) => {
    // Defensive: if the transition throws (e.g. DOM issue), still get the
    // user to the stage rather than stranding them on the title.
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
    } catch (e) {
      console.error('[fable] wireStage threw on fallback', e);
    }
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
  // restores the pre-game active_card_id. Idempotent + safe if the engine
  // never started (engineStarted gate avoids a needless IPC round-trip on
  // the no-engine degrade path).
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
// the cinematic boot transition (cloud part → ripple + button stagger).
//
// THE FOG GATE HANDOFF:
// The user clicks the Fable tile → fogGate builds up fog over 2s in the OS
// layer (over the desktop) while wind noise fades in. At 2s the gate calls
// back, Fable opens underneath, and the gate's fog node is handed to boot.js
// — which leaves it AT document.body for the HOLD period (so it stays above
// #fable while #fable fades in), then reparents + converts + parts it in one
// frame at Phase 2. See screens/boot.js for the flicker-fix rationale.
//
// openFable receives { fogNode, wind } from the gate (or null/null on a
// bare launch with no gate, e.g. dev shortcuts). The fog node is passed
// THROUGH to boot.js untouched — we do NOT adopt it into #fable here. The
// prior version did, and that reparent-then-convert-later split was the
// source of the handoff flicker.
function openFable() {
  if (!fableRoot) return;
  // Pull the fog handoff from launchFable's stash (set by the fog gate's
  // onReady callback). Cleared after consumption so a bare launchApp (no
  // gate) runs with nulls — boot.js handles that gracefully.
  const handoff = pendingFogHandoff;
  pendingFogHandoff = null;
  const fogNode = handoff ? handoff.fogNode : null;
  const wind = handoff ? handoff.wind : null;
  // Hide the title buttons BEFORE the screen shows so they never flash
  // visible for one frame before the boot transition hides them.
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

  // NOTE: the fog node is intentionally NOT adopted into #fable here. It
  // stays at document.body (z:5000, above #fable's z:4000) so it continues
  // to visually obscure the screen while #fable completes its 420ms opacity
  // transition underneath. boot.js reparents it into #fable at Phase 2
  // (after HOLD) in the same tick it converts + parts it — see boot.js's
  // convertToPartingFog for the flicker-fix rationale.

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
    fogNode,
    wind,
  });
}

// PUBLIC: the entry point the OS calls when the user clicks the Fable tile.
// Opens the fog gate (2s buildup over the desktop + wind fade-in), then on
// ready launches Fable through the lifecycle manager (which fires onOpen =
// openFable) with the fog handoff. This is what script.js wires the home
// tile + dock click to (replacing the direct openWindow('fable')).
//
// The fog node + wind are stashed module-level so openFable (called by the
// lifecycle manager's launchApp → onOpen) can pick them up. This indirection
// is necessary because launchApp calls onOpen with no args.
let pendingFogHandoff = null;
export function launchFable() {
  openFogGate((fogNode, wind) => {
    pendingFogHandoff = { fogNode, wind };
    AppLifecycle.launchApp('fable');
  });
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
    // ~3.4s cinematic (e.g. mid-fog at t=1s), this stops the whoosh/ripple
    // SFX, removes the fog + ripple nodes, and force-reveals the title
    // buttons — so the next open isn't stranded with invisible buttons or
    // a duplicate fog layer from a half-finished prior transition.
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

  // Build the screens we still use. The 4 menu flows (picker / saves /
  // interview / loading) are no-ops for now — their UI is being redone, so
  // we don't build them. The stage stays built (teardownStage needs it on
  // close even though the menu can't reach it yet).
  screens.title = buildTitle({
    // NEW GAME + QUICK PLAY both enter the simulation engine via the magical
    // transition today. There are no roleplay .sim cards on disk yet + the
    // card-authoring flow was removed for a from-scratch rewrite, so both
    // buttons enter the stage UI directly (no card, no engine). The
    // distinction between them will return when the new authoring flow
    // lands — for now both are "enter the simulation."
    new: () => enterStageMagically(),
    quickplay: () => enterStageMagically(),
    // CONTINUE + LOAD stay no-ops: their flows (resume target / save picker)
    // are being rebuilt alongside the new authoring flow.
    continue: () => {},
    load: () => {},
    // EXIT is the ONLY real close path — routes through the lifecycle
    // manager for full teardown.
    exit: () => AppLifecycle.closeApp('fable'),
  });
  screens.stage = buildStage();
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

  // Bridge the OS window system to the fog-gate launch. The home-grid
  // Fable tile + the dev #fable shortcut call openWindow('fable'), which
  // fires the openHook. We redirect it to launchFable() — which runs the
  // 2s fog-gate buildup over the desktop FIRST, then calls
  // AppLifecycle.launchApp('fable') → onOpen=openFable with the fog handoff.
  // This is the load-bearing "2 second delay before launching" gate.
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
