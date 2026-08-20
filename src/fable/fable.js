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
//   onPause  → (alt-tab / focus-loss) freeze theme music + CSS FX particles.
//              Title particle RAF already self-pauses on document.hidden
//              (see particles.js).
//   onResume → (focus return) unfreeze all of the above smoothly.
//   onClose  → (EXIT only) full teardown: stage torn down, particles
//              destroyed, music removed, chrome restored, OS aurora resumed,
//              fable_end IPC sent. Nothing survives the close.
//
// MENU STATE: all 3 title flows are wired.
//   New Game → the cinematic creator flow: Player pair → SIM pair → Codex
//              pair → launchGame (see revealNewGameShell). Each picker
//              reuses the newgame-split tile language; the Codex picker is
//              skipped when the card already has a codex (advanceFromSim).
//              The Load menu's per-card NEW shortcut-circuits the chain: the
//              card is preset, so the player step launches straight into the
//              world (flowAfterPlayer — no SIM pair, no Codex pair).
//   Continue → resume the freshest save (resumeSave).
//   Load     → two-level picker: worlds.js → saves.js → resume.
// The working stage + gameplay engine (stage.js, engine/*, fx/*, panels/*)
// are the destination of every flow.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { AppLifecycle } from '../app-lifecycle.js';
import { withShellBusy } from '../shell-guard.js';
import './fable.css';
import './flow-cinematic.css';

import { buildTitle } from './screens/title.js';
import { buildStage, wireStage, teardownStage, toast, bottomWarning } from './screens/stage.js';
import { buildNewGameSplit } from './screens/newgame-split.js';
import { buildPlayerPicker, renderPlayerPicker } from './screens/player-picker.js';
import { buildCreatorChat, renderCreatorChat, abortCreatorTurn } from './screens/creator-chat.js';
import { createEmbers } from './screens/embers.js';
import { parseImportFile } from './screens/st-import.js';
import { extractLorebookEntries } from './engine/creator-engine.js';
import { playBurnTransition, playReverseSpawn } from './engine/burn-transition.js';
import { tileCaptionHTML } from './engine/tile-caption.js';
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
import { buildOnlinePanel } from './screens/online.js';
import { pauseFX, resumeFX } from './fx/effects.js';
import { activateChrome, deactivateChrome } from './engine/chrome.js';
import { playMagicalTransition } from './engine/transition.js';
import { mountFlowChrome } from './engine/flow-chrome.js';

let fableRoot = null;       // the #fable app-window element
let screens = {};           // name → screen element
// The persistent New Game / Load flow ambiance layer (deep void + hearth
// glow + rising embers) mounted once on #fable. Stays put across screen
// swaps so the background never jumps; only the foreground UI changes.
// startFlowAmbiance / stopFlowAmbiance (defined in initFable) toggle it.
let flowAmbiance = null;
// The persistent flow-chrome controller (‹ / ⌂). Mounted once into
// fableRoot when the New Game flow begins; owns all nav for the flow
// so the screens themselves carry no header bars.
let flowChrome = null;
// The New Game flow state machine. `step` tracks the current screen so the
// flow-chrome ‹ can route back correctly; `selectedPlayerId` carries the
// chosen SavedPlayer forward to the SIM/codex steps; `selectedCardId`
// is the SIM card established by the SIM pair (rides to codex/launch).
// The GLM-driven player/sim/codex wizards all hang off this same state.
let flowState = {
  step: null,
  slideOneHasBack: false, // whether slide 1 (Player pair) shows ‹ (Load-menu entry only)
  selectedCardId: null,   // the sim card built by the sim wizard (rides to codex/launch)
  selectedPlayerId: null, // the chosen SavedPlayer (rides to fable_start)
  playerDraft: null,      // the player wizard's draft (starting conditions for fable_start)
  simDraft: null,         // the sim wizard's draft (context for the launch)
  pendingImport: null,    // a SillyTavern import { charData, portraitDataUrl?, portraitExt? } seeded by the IMPORT tile → the Player Wizard
};
// The blank flowState every full reset stamps (exitFlowToTitle + closeFable).
// A surviving state would resume a stale step on the next flow entry.
// (2026-08-18) pendingSimIntro is GONE: an import's greetings now ride into a
// SIM wizard ONLY via that wizard's own IMPORT tile (flowCreateSim's direct
// presetIntro param) — a player-side import can never leak its greeting into
// an unrelated fresh world's <intro>.
function freshFlowState() {
  return { step: null, slideOneHasBack: false, selectedCardId: null, selectedPlayerId: null, playerDraft: null, simDraft: null, pendingImport: null };
}

// Start/stop the persistent New Game / Load flow ambiance (deep void +
// hearth glow + rising embers on the #fable root). Called at flow entry
// (revealNewGameShell / onLoadClicked) + flow exit (exitFlowToTitle /
// exitLoadToTitle). The embers canvas is created on first start + destroyed
// on stop so no RAF leaks across flow entries/exits. Idempotent.
let flowEmbers = null;
function startFlowAmbiance() {
  if (!flowAmbiance) return;
  flowAmbiance.classList.add('is-active');
  if (!flowEmbers) {
    const host = flowAmbiance.querySelector('.fable-flow-ambiance-embers');
    if (host) flowEmbers = createEmbers(host);
  }
}
function stopFlowAmbiance() {
  if (!flowAmbiance) return;
  flowAmbiance.classList.remove('is-active');
  if (flowEmbers) { flowEmbers.destroy(); flowEmbers = null; }
}

// FABLE ENTRY (#fable / fable.exe): mirrors script.js's FABLE_ENTRY. When
// active, openFable skips the fog gate + the 2s boot transition and instead
// shows the title with buttons already visible + theme music started
// immediately. fable.exe loads `wupi.html#fable` so this is TRUE in the
// shipped launcher (lands on Fable's title in <1s); the legacy #dev=fable /
// ?dev=fable forms are kept for `npm run dev` iteration. Accepts the bare hash
// (#fable), the query (?fable), and the legacy dev forms — see script.js.
const FABLE_ENTRY = (() => {
  try {
    const has = (p) => p && (p.get('fable') !== null || p.get('dev') === 'fable');
    if (has(new URLSearchParams(window.location.search))) return true;
    const h = window.location.hash.replace(/^#/, '');
    if (h === 'fable') return true;          // bare #fable
    return has(new URLSearchParams(h));      // #fable=… / #dev=fable
  } catch (_) { return false; }
})();
// The floating-F splash hold (ms). MUST match script.js's
// `setTimeout(fadeSplash, 2000)` — the splash (#fable-entry-splash, owned by
// script.js) holds this long before crossfading out, and openFable's
// FABLE_ENTRY branch delays the title-screen reveal + theme music by the same
// amount so the menu appears as the F logo dissolves (not before — the splash
// background is transparent, so an earlier reveal shows the menu THROUGH the
// splash). Kept as a named constant here so the two sites stay synchronized.
const FABLE_SPLASH_HOLD_MS = 2000;
// The entry reveal's top-to-bottom wipe duration (ms) — shared by the
// fable.exe title reveal AND the .lnk direct-launch stage reveal. MUST match
// the .fable-entry-wipe animation duration in fable.css (1000ms) — the entry
// hold (.fable-entry-hold + html.fable-wipe) stays up for exactly this long
// after the reveal, so no backdrop ever out-paces the wipe's coverage.
const FABLE_ENTRY_WIPE_MS = 1000;
// Shared entry-wipe machinery — used by BOTH reveal paths (the fable.exe
// title reveal AND the .lnk direct-launch stage reveal): stamps
// html.fable-wipe, runs the 1s top-to-bottom mask sweep (.fable-entry-wipe,
// fable.css) on `screenEl`, and drops the whole entry hold at wipe end.
//
// Backdrop discipline: NOTHING dark paints until the wipe has covered it.
// The old choreography dropped .fable-entry-hold first so the fade ran over
// the solid void — fine for a fast uniform fade, but with the slow wipe the
// instant void read as a blank dark window popping onto the desktop. So:
//   - #fable's void stays suppressed for the WHOLE wipe (.fable-entry-hold
//     stays on): the sweep reveals the DESKTOP behind the content, and the
//     void only paints once the screen fully covers it (an invisible swap).
//   - html.fable-wipe (styles.css) extends the body-transparent hold past
//     script.js's html.fable-entry teardown (~2.6s, when the splash node
//     goes) — otherwise the body's #02040a base would pop in behind the
//     still-sweeping bottom half of the screen.
// A stranded hold would leave the app permanently transparent +
// click-through, so a timer backs up the animationend hook — idempotent,
// whichever lands first wins.
const runEntryWipe = (fableRoot, screenEl) => {
  document.documentElement.classList.add('fable-wipe');
  const dropEntryHold = () => {
    fableRoot.classList.remove('fable-entry-hold');
    document.documentElement.classList.remove('fable-wipe');
  };
  if (screenEl) {
    screenEl.classList.remove('fable-entry-wipe');
    void screenEl.offsetWidth; // reflow so re-adding restarts the animation
    screenEl.classList.add('fable-entry-wipe');
    // animationend BUBBLES — the screen's children (staggered buttons,
    // particles, feed beats…) fire their own. Only the wipe itself may end
    // the hold.
    const onWipeEnd = (ev) => {
      if (ev.animationName !== 'fableEntryWipe') return;
      screenEl.removeEventListener('animationend', onWipeEnd);
      screenEl.classList.remove('fable-entry-wipe');
      dropEntryHold();
    };
    screenEl.addEventListener('animationend', onWipeEnd);
  }
  // Fallback: animationend can be missed (element swapped mid-wipe, animations
  // throttled while backgrounded). +120ms so the timer never beats the
  // compositor's final animated frame. Also strips the wipe class — a missed
  // event would otherwise leave the mask layer mounted all session.
  setTimeout(() => {
    if (screenEl) screenEl.classList.remove('fable-entry-wipe');
    dropEntryHold();
  }, FABLE_ENTRY_WIPE_MS + 120);
};
// Title-reveal choreography shared by the FABLE_ENTRY + DIRECT_LAUNCH title
// paths: showScreen('title') + the shared entry wipe + theme-music fade-in.
// Defined ONCE so the direct-launch fallbacks can't drift out of sync with
// the plain-entry reveal again — the 2026-08-14 "menu spawns beside the F on
// .lnk launches" bug was exactly that drift: the fallbacks called
// revealHold()+showScreen('title') bare, skipping the hold window + dissolve
// entirely (the async IPC handoff resolves in tens of ms, so the title
// appeared while the F splash was still in its 2s hold).
const revealTitleUnderSplash = (fableRoot) => {
  showScreen('title');
  const titleEl = screens.title;
  // Drop the title's 2s transparency hold FIRST (fable.css): the title was
  // shown at t=0 behind the F splash, held at opacity 0 while it warmed up —
  // the wipe then rides the full 1s top-to-bottom sweep from a clean start.
  if (titleEl) titleEl.classList.remove('fable-title-held');
  runEntryWipe(fableRoot, titleEl);
  // Theme music fires HERE — i.e. exactly at FABLE_SPLASH_HOLD_MS (2s after
  // entry), never during the splash hold.
  try { startThemeMusic(fableRoot, { fadeIn: true }); } catch (_) {}
};
// DEV SHORTCUT (?dev=preview or #dev=preview): pure-frontend layout preview.
// Skips title + void + ALL backend (no model, no API, no fable_send) and drops
// straight into the chat stage with placeholder messages (devPreviewEnter).
// Purpose: iterate on the VN chat UI + test scroll behavior without launching
// a real game. stage.js injects the placeholder portraits via its own
// DEV_PREVIEW flag. False in production.
const DEV_PREVIEW_SHORTCUT = (() => {
  try {
    if (new URLSearchParams(window.location.search).get('dev') === 'preview') return true;
    const h = window.location.hash.replace(/^#/, '');
    return new URLSearchParams(h).get('dev') === 'preview';
  } catch (_) { return false; }
})();
// DIRECT LAUNCH (?direct=1): appended to the window URL by Rust (lib.rs setup)
// ONLY on the fable.exe --card <slug> [--save <id>] path. Means a desktop
// shortcut / direct exe launch wants to boot straight into a specific card+save,
// skipping the title. The actual { cardSlug, saveId } comes from the
// get_launch_context IPC (Rust stashed it from argv). Checked AFTER
// DEV_PREVIEW + BEFORE the plain FABLE_ENTRY title branch (mutually exclusive).
const DIRECT_LAUNCH = (() => {
  try {
    return new URLSearchParams(window.location.search).get('direct') === '1';
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

// ── Double-click / rapid-tap guard (2026-08-05) ───────────────────────
// Chloe: "it's really easy to double click and glitch your game, prevent
// double clicking all through FABLE." A single busy flag + a capture-phase
// listener on fableRoot that swallows clicks/pointerdowns while a transition
// / burn / load is in flight. Every transition path wraps itself in
// withFlowBusy(...) below; a second click during the wrap is killed before it
// reaches any handler. The flag is cleared on completion AND on a safety
// timeout so a forgotten clear can never dead-lock the UI.
let flowBusy = false;
let flowBusyTimer = null;
// Hard ceiling: no transition should ever run this long. If the flag is still
// set after this, force-clear it (a forgotten clear is a recoverable bug, a
// permanent dead-lock is not). 12s covers the longest burn+spawn+seed chains.
const FLOW_BUSY_SAFETY_MS = 12000;
function setFlowBusy(on) {
  flowBusy = on;
  if (flowBusyTimer) { clearTimeout(flowBusyTimer); flowBusyTimer = null; }
  if (on) {
    flowBusyTimer = setTimeout(() => {
      flowBusy = false;
      flowBusyTimer = null;
    }, FLOW_BUSY_SAFETY_MS);
  }
}
// Wrap an async/completion-based transition in the busy flag. `task` may
// return a Promise (resolved → clear) or nothing (caller clears manually via
// setFlowBusy(false)). Used by every burn/transition/load path so they share
// one guard instead of per-button ad-hoc flags.
function withFlowBusy(task) {
  if (flowBusy) return;        // already mid-transition: drop the second click
  setFlowBusy(true);
  let ret;
  try {
    ret = task();
  } catch (e) {
    setFlowBusy(false);
    throw e;
  }
  if (ret && typeof ret.then === 'function') {
    ret.then(() => setFlowBusy(false), () => setFlowBusy(false));
  }
  return ret;
}

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
  // (2026-08-16 audit M8a) Ambient systems only run while #fable is actually
  // shown: initFable's boot-time showScreen('title') used to start the
  // title's particle/grass/leaf/cloud/sparkle RAFs while the app window was
  // still hidden — on wupi.exe (OS boot, Fable never opened) they then ran
  // invisibly for the WHOLE desktop session. Every real open path adds
  // .show BEFORE its showScreen call (fog onSwap, FABLE_ENTRY, dev preview),
  // so gating here covers them all; hidden screens still get _stopAmbient.
  const rootShown = !!(fableRoot && fableRoot.classList.contains('show'));
  for (const key of Object.keys(screens)) {
    const showing = (key === name);
    const scr = screens[key];
    scr.hidden = !showing;
    if (showing && rootShown && scr._startAmbient) scr._startAmbient();
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

// === Continue / Load / New Game flows ====================================
//
// CONTINUE: resume the freshest save for a NEW GAME world (the title's
// _refreshTitleGate stashes the target from fable_continue_target). Autosaves
// are included (the per-turn checkpoint is "where you left off"). The stashed
// target carries both card_id + save_id, so this is a one-shot resume.
//
// LOAD: a two-level picker — choose a world (screens/worlds.js), then choose
// a save in that world (screens/saves.js). Both feed into resumeSave.
//
// NEW GAME: reveals the cinematic creator flow shell — background + music +
// the ‹ / ⌂ flow-chrome buttons (see onNewGameClicked / revealNewGameShell),
// then slide 1 (the Player pair). The Load menu's per-card "NEW" action
// (beginNewGameFromCard) lands on the same shell with ‹ routing back to the
// worlds grid.
//
// resumeSave mirrors the shared stage-entry tail (stop ambient/music →
// fable_end → load → enterStageViaTransition) and drives a world via
// fable_start(cardId, saveId), which re-reads the .sim from disk + resumes
// the named slot.

// Resume a named save for a world. `cardId` resolves the .sim card; `saveId`
// is the slot to resume. `opts.underSplash` (direct launch only) routes the
// stage entry through the splash-aligned wipe reveal instead of showing the
// stage the moment the load finishes. Cold-resume from the title (no game
// running yet), so fable_start is the entry — NOT fable_load_save (that
// requires an already-running game).
async function resumeSave(cardId, saveId, opts = {}) {
  // Guarded: a rapid double-click on a save row could fire two fable_start
  // calls. The withFlowBusy wrapper drops the second; the first clears the
  // flag via enterStageViaTransition (the final step below).
  if (flowBusy) return;
  setFlowBusy(true);
  // The Load-menu music (shared with New Game) stops here — the stage owns its
  // own ambience. Stopped before the IPC so the fade overlaps the load.
  stopNewGameMusic(fableRoot);
  // NOTE: the title ambient (grass/particles) is NOT stopped here — it's
  // stopped inside enterStageViaTransition right before the stage shows, so
  // the grass keeps animating until the moment the stage appears.
  fadeOutThemeMusic(fableRoot);
  // Same funnel as launchGame: clear any prior session (stopping any live
  // generation first — a no-op on the normal cold path).
  await endFableSession();

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', { cardId, saveId });
    engineStarted = true;
    if (result && result.intro) openingScene = result.intro;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_start (resume) failed — returning to the title', err);
    engineStarted = false;
    // (P2 fix) Direct-launch contract: "any failure falls back to the title
    // so a broken shortcut never strands the window". Rethrow under the
    // splash so the DIRECT_LAUNCH catch routes to revealTitleAfterSplash
    // instead.
    if (opts && opts.underSplash) throw err;
    // (2026-08-16 audit fix #26) The non-direct path used to fall through
    // into enterStageViaTransition anyway — a dead, engine-less stage the
    // player could only stare at. Unwind to the title instead: disarm the
    // busy latch immediately (not the 12s safety timer) + exit the Load
    // flow's chrome/ambience the same way its ⌂ does.
    setFlowBusy(false);
    exitLoadToTitle();
    return;
  }

  enterStageViaTransition(openingScene, loadMessages, opts);
}

// CONTINUE button handler: resume the stashed continue target. The target is
// refreshed on every title show via _refreshTitleGate (title.js); if it's null
// (no save exists) the button is disabled, so a null target here is a
// race/state bug — guard + log rather than crash.
function onContinueClicked() {
  const target = (screens.title && screens.title._continueTarget) || null;
  if (!target || !target.card_id || !target.save_id) {
    console.warn('[fable] Continue clicked with no resume target — ignoring');
    return;
  }
  // resumeSave sets the busy flag itself (it's also called from the saves
  // screen); no separate wrap needed here.
  resumeSave(target.card_id, target.save_id);
}

// LOAD button handler: show the world picker (the "Load Game" grid) +
// populate it. The worlds screen mirrors the Player Picker exactly — same
// embers + grid + mini-cards, no ‹ Back header — and uses the flow-chrome ⌂
// home button (top-right) to return to the title. Selecting a world card
// opens a modal (NEW / LOAD / EDIT / DELETE) via worldHandlers().
//
// Transition: wrap the title → worlds swap in the black magical transition +
// stop the title ambient at click (matches New Game). The title
// theme fades out at click; the SAME newgame.mp3 + fire.mp3 ambience as New
// Game blooms in at the transition midpoint. On exit (⌂ → exitLoadToTitle)
// the ambience stops + the title theme restarts. NB: this owns its OWN
// transition, so title.js must call it directly (NOT wrapped in a second
// playMagicalTransition — that double-fades; see title.js).
function onLoadClicked() {
  withFlowBusy(() => {
    // Stop the title ambient (grass + particles) at click so the dim falls over
    // a static frame (matches New Game).
    if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
    // Fade the title theme out — the Load menu gets the SAME newgame.mp3 +
    // fire.mp3 ambience as New Game (Chloe 2026-08-05). The pair fades in at
    // the transition midpoint, mirroring onNewGameClicked.
    fadeOutThemeMusic(fableRoot);
    // The reveal is factored out so the transition's catch fallback (below)
    // mounts the home button + music too, not just the happy path.
    const revealWorlds = () => {
      startFlowAmbiance();
      showScreen('worlds');
      renderWorlds(screens.worlds, worldHandlers());
      // Mount the flow chrome (⌂ home, top-right) — mirrors the Player Picker.
      // No ‹ Back here (the worlds grid is the top of the Load flow); ⌂ returns
      // to the title. Hidden for 2.5s so it doesn't spawn the instant the
      // screen reveals (matches the New Game flow's delayHome feel).
      if (!flowChrome) flowChrome = mountFlowChrome(fableRoot);
      if (flowChrome) {
        flowChrome.setVariant('newgame');
        flowChrome.hideBack();
        flowChrome.delayHome(2500);
        flowChrome.onHome(() => exitLoadToTitle());
      }
      // Bloom the music + fire as the worlds screen reveals (same hand-off
      // feel as New Game).
      startNewGameMusic(fableRoot, { fadeIn: true });
    };
    return playMagicalTransition({
      blackHoldMs: 1150,
      onMidpoint: revealWorlds,
    }).catch((e) => {
      console.error('[fable] Load transition failed, jumping to worlds', e);
      revealWorlds();
    });
  });
}

// The Load menu's card-modal handlers (2026-08-05). The modal surfaces four
// actions per card; these are the wired behaviors:
//   • onNewGame → fade out → the Player pair (slide 1) with reverse-spawn,
//     with THIS card preset into the flow — once a player is chosen/created
//     the game launches straight into this world (flowAfterPlayer; no SIM
//     pair, no Codex pair). The
//     music keeps playing (it's the same New Game ambience the flow uses).
//   • onResume  → the saves list for this card (most-recent first; backend
//     sorts by timestamp desc). ‹ Back from saves returns to the worlds grid.
//   • onEdit    → a centered raw-XML editor over the worlds screen, loaded
//     via fable_card_raw_get_by_id, saved via fable_card_raw_set_by_id (the
//     _by_id variants take an explicit card_id — no active game required,
//     which the Load menu needs since no game is active).
function worldHandlers() {
  return {
    onNewGame: (card) => beginNewGameFromCard(card),
    onResume: (card) => openWorldSaves(card),
    onEdit: (card) => openCardRawEditor(card),
  };
}

// NEW GAME from the Load menu's per-card "NEW" action. The card IS the
// world — it's preset into the flow (revealNewGameShell's presetCard), so
// after the player step the flow launches straight into that world: NO SIM
// pair (the card was already chosen here — asking again was the double-pick
// bug), NO Codex pair (2026-08-19, Chloe: skip it). The New Game ambience is
// already playing (it's the same track the worlds/Load screen uses), so it
// is NOT restarted here. ‹ routes back to the worlds grid; ⌂ routes to the
// title (via exitLoadToTitle, which also tears the ambience down).
function beginNewGameFromCard(card) {
  withFlowBusy(() => {
    return revealNewGameShell({
      presetCard: card,
      onBack: () => {
        // Back to the worlds grid (where this entry came from). The Load-menu
        // ambience keeps playing — it's the same track the shell was using.
        if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
        showScreen('worlds');
        renderWorlds(screens.worlds, worldHandlers());
      },
      onHome: () => exitLoadToTitle(),
    });
  });
}

// RESUME from the Load menu → the saves list for this card.
function openWorldSaves(card) {
  showScreen('saves');
  renderSaves(screens.saves, card.id, (save) => resumeSave(card.id, save.save_id), card.name);
}

// EDIT from the Load menu → a centered raw-XML editor modal over the worlds
// screen (2026-08-05). The card creators have no reverse-parse (their
// <persona> block is a lossy merge), so the faithful edit surface is the XML
// itself — zero data loss. Loads via fable_card_raw_get_by_id, saves via
// fable_card_raw_set_by_id (the same validation gate the tab-rail raw editor
// uses: parse_from_xml_str before any disk touch). Self-contained (doesn't
// touch the stage's raw-editor.js singleton, which is active-card-keyed) so
// it works from the title with no game active.
//
// (2026-08-20, Chloe) The SAME editor now serves PLAYER cards: editing a
// loaded player from the Player Picker opens the raw `.player` XML (Rust
// owns the format — parse_player_xml / render_player_xml) instead of the
// retired wizard-chat edit run. The modal core is shared via openXmlEditorModal.
function openCardRawEditor(card) {
  const worlds = screens.worlds;
  if (!worlds) return;
  openXmlEditorModal(worlds, {
    title: `Edit ${card.name} — Sim Card (.sim)`,
    rootTag: 'sim_card',
    load: () => invoke('fable_card_raw_get_by_id', { cardId: card.id }),
    save: (text) => invoke('fable_card_raw_set_by_id', { cardId: card.id, xml: text }),
  });
}

// The PLAYER twin (2026-08-20): a centered raw-XML editor over the Player
// Picker screen. Saves round-trip through the server-side parse gate
// (parse_player_xml before any disk touch); on a successful save the picker
// re-renders so a renamed player's tile reflects the edit.
function openPlayerRawEditor(player) {
  const picker = screens['player-picker'];
  if (!picker) return;
  openXmlEditorModal(picker, {
    title: `Edit ${player.name} — Player Card (.player)`,
    rootTag: 'player',
    load: () => invoke('fable_player_raw_get', { id: player.id }),
    save: (text) => invoke('fable_player_raw_set', { id: player.id, xml: text }),
    onSaved: () => renderPlayerPickerStep(),
  });
}

// The shared centered raw-XML editor modal (self-contained by design — it
// never touches the stage's active-card-keyed raw-editor singleton, so it
// works from the title with no game active). `opts`:
//   title   — the header text (already plain; escaped here)
//   rootTag — the expected root element ('sim_card' | 'player')
//   load()  — Promise<string> of the current file text
//   save(text) — Promise persisting it (server-side validation gate)
//   onSaved() — optional post-save hook (e.g. re-render the picker)
function openXmlEditorModal(host, opts) {
  // If one's already open, close it first (defensive against a double-open).
  const existing = host.querySelector('.fable-world-raw-overlay');
  if (existing) existing.remove();

  const overlay = document.createElement('div');
  overlay.className = 'fable-raw-editor-overlay fable-world-raw-overlay';
  overlay.innerHTML = `
    <div class="fable-raw-editor-backdrop" aria-hidden="true"></div>
    <div class="fable-raw-editor-modal" role="dialog" aria-modal="true">
      <div class="fable-raw-editor-head">
        <span class="fable-raw-editor-title">${escHtml(opts.title)}</span>
        <div class="fable-raw-editor-controls">
          <button type="button" class="fable-raw-btn save" data-raw-save>✓</button>
          <button type="button" class="fable-raw-btn revert" data-raw-revert>↻</button>
          <button type="button" class="fable-raw-btn close" data-raw-close>✕</button>
        </div>
      </div>
      <textarea class="fable-raw-editor-text" data-raw-text spellcheck="false"></textarea>
    </div>`;
  host.appendChild(overlay);

  const textarea = overlay.querySelector('[data-raw-text]');
  const saveBtn = overlay.querySelector('[data-raw-save]');
  const revertBtn = overlay.querySelector('[data-raw-revert]');
  const closeBtn = overlay.querySelector('[data-raw-close]');
  let lastGood = '';
  let isValid = true;

  // Load the current XML.
  opts.load()
    .then((xml) => { lastGood = xml || ''; textarea.value = lastGood; validate(); })
    .catch((err) => { console.warn('[fable] raw editor load failed', err); });

  // Client-side XML well-formedness sniff (mirrors raw-editor.js). Cheap
  // pre-check; the authoritative gate is the server-side parse on save.
  function sniffXmlWellFormed(s) {
    const trimmed = String(s || '').trim();
    if (!trimmed) return 'Empty file';
    const rootRe = new RegExp(`^<\\?xml|<${opts.rootTag}[\\s>]`, 'i');
    if (!rootRe.test(trimmed)) return `Missing <${opts.rootTag}> root`;
    // Basic tag-balance sniff: walk a stack of opening/closing tags.
    const stack = [];
    const re = /<\/?([a-zA-Z_][\w.-]*)([^>]*?)(\/?)>|<!--[\s\S]*?-->|<!\[CDATA\[[\s\S]*?\]\]>/g;
    let m;
    while ((m = re.exec(trimmed)) !== null) {
      if (!m[1]) continue;                 // comment / CDATA
      const isClose = m[0].charAt(1) === '/';
      const isSelf = m[3] === '/';
      if (isClose) {
        if (!stack.length || stack[stack.length - 1] !== m[1]) return `Mismatched </${m[1]}>`;
        stack.pop();
      } else if (!isSelf) {
        stack.push(m[1]);
      }
    }
    if (stack.length) return `Unclosed <${stack[stack.length - 1]}>`;
    return null;
  }
  function validate() {
    const err = sniffXmlWellFormed(textarea.value);
    isValid = !err;
    saveBtn.disabled = !isValid;
    textarea.classList.toggle('invalid', !isValid);
  }
  textarea.addEventListener('input', validate);

  function close() { overlay.remove(); }
  async function save() {
    if (!isValid) return;
    try {
      await opts.save(textarea.value);
      lastGood = textarea.value;
      validate();
      if (opts.onSaved) opts.onSaved();
    } catch (err) {
      // Status bar removed 2026-08-12 per Chloe — save failures log silently.
      console.warn('[fable] raw editor save failed', err);
    }
  }

  saveBtn.addEventListener('click', save);
  revertBtn.addEventListener('click', () => { textarea.value = lastGood; validate(); });
  closeBtn.addEventListener('click', close);
  // Esc closes (only when the text == last-good, mirroring the raw-editor's
  // "✕ refuses on invalid" discipline — unsaved/invalid changes must be
  // reverted or fixed first). Ctrl+Enter saves.
  overlay.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      if (textarea.value === lastGood || isValid) close();
    } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault(); save();
    }
  });
  // Click on the backdrop (outside the modal) closes — same rule as Esc.
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay && (textarea.value === lastGood || isValid)) close();
  });

  setTimeout(() => textarea.focus(), 50);
}

// Tiny HTML-escape for the editor title (card names are user-authored).
function escHtml(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// === THE NEW GAME SHELL ================================================
//
// The player + sim-card creation suite was removed ahead of a full UI
// overhaul. The New Game flow now reveals a single empty shell — just the
// ember background + hearth glow, the new-game ambience, + the ‹ / ⌂
// flow-chrome buttons (engine/flow-chrome.js). Two entry points share it:
//   • the title's "New Game" button  → onNewGameClicked (‹ + ⌂ → title)
//   • the Load menu's per-card "NEW" → beginNewGameFromCard (‹ → worlds)
//
// AUDIO: the music + fire fade-in KICKS OFF at the transition midpoint
// (screen black) so the slow 3s ramp blooms through the 2s undim — partly
// audible as the shell reveals (earlier + more gradual than starting cold
// after the undim). Fade duration lives in newgame-music.js (FADE_IN_MS).
//
// The flow chrome is mounted once into fableRoot on entry + persists; both
// ‹ + ⌂ are visible on the shell. ⌂ home is delayed 2.5s so it doesn't spawn
// the instant the flow mounts.

// Reveal the New Game shell with a black transition. Mounts the flow chrome,
// swaps to the 'newgame-split' screen at peak darkness + builds slide 1 (the
// Player pair), + (optionally) blooms the ambience. `startMusic` is false on
// the Load-menu entry path (the track is already playing from the worlds
// screen). `presetCard` (the Load-menu per-card NEW entry) establishes the
// SIM card up front so the player step routes straight into that world.
// `onBack`/`onHome`
// override the default ‹/⌂ routing (both default to exitFlowToTitle).
function revealNewGameShell({ startMusic = false, presetCard = null, onBack, onHome } = {}) {
  // Fresh state on EVERY entry (both entries pass through here). exitLoadToTitle
  // doesn't reset (unlike exitFlowToTitle), so without this a card established
  // on an earlier Load-menu NEW run could survive into a later title-entry
  // flow and hijack flowAfterPlayer into launching the stale world.
  flowState = freshFlowState();
  if (presetCard) {
    flowState.selectedCardId = presetCard.id;
    // Mirrors renderSimPickerStep's meta draft (context only — launch reads
    // selectedPlayerId/playerDraft, not simDraft).
    flowState.simDraft = {
      name: presetCard.name,
      tone: presetCard.tone || null,
      setting: presetCard.setting_preview || null,
    };
  }
  startFlowAmbiance();
  if (!flowChrome) flowChrome = mountFlowChrome(fableRoot);
  if (flowChrome) {
    flowChrome.setVariant('newgame');  // brass home glyph
    // Slide 1 (the Player pair) hides ‹ unless the entry has a meaningful
    // back destination (the Load-menu entry's worlds-grid return). The title
    // entry already has ⌂ home to exit, so a redundant ‹ is noise — "arrow
    // in the top left which shouldn't show" only on this FIRST slide. Record
    // the entry path + stamp slide 1 via setFlowStep, whose syncFlowBack
    // call enforces the rule. Every deeper slide (also set via setFlowStep)
    // re-shows ‹; returnToPlayerPair re-hides it here on slide 1.
    flowState.slideOneHasBack = !!onBack;
    setFlowStep('player');
    // ⌂ home is hidden for 2.5s on every entry so it doesn't appear the
    // instant the flow mounts. Re-entrant: re-entry cancels any prior pending
    // reveal + restarts the delay.
    flowChrome.delayHome(2500);
    // ‹ routes through flowBack so in-flow steps (e.g. the Player Picker)
    // return to the player pair instead of bailing to the title. `onBack`
    // (the Load-menu entry's worlds-grid return) is carried for the 'player'
    // step; the title entry leaves it null → exitFlowToTitle.
    flowChrome.onBack(() => flowBack(onBack));
    flowChrome.onHome(() => (onHome ? onHome() : exitFlowToTitle()));
  }
  return playMagicalTransition({
    blackHoldMs: 1150,
    onMidpoint: () => {
      // Swap the screen at peak darkness, then build the Player pair tiles
      // (Create Player / Load Player) — slide 1 of the flow — so they ship at
      // opacity:0 for the reverse-spawn after the undim. Bloom the ambience so
      // its fade-in overlaps the undim reveal.
      buildPlayerPairTiles();
      showScreen('newgame-split');
      if (startMusic) startNewGameMusic(fableRoot, { fadeIn: true });
    },
  }).then(() => {
    // AFTER the black fully clears: reverse-spawn the Player pair buttons
    // (visible scene first, then UI). The music fade-in already kicked off at
    // the midpoint above.
    const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
    if (tiles.length) return playReverseSpawn(Array.from(tiles));
  }).catch((e) => {
    console.error('[fable] New Game transition failed, jumping to shell', e);
    if (startMusic) startNewGameMusic(fableRoot, { fadeIn: true });
    buildPlayerPairTiles();
    showScreen('newgame-split');
    // (2026-08-16 audit M8d) The tiles are BUILT at opacity:0 (they ship for
    // the reverse-spawn) — without spawning them here the catch fallback
    // revealed a silent, empty picker. Run the same spawn the success path
    // runs; its own failure is swallowed (the tiles' CSS default still
    // allows interaction-less visibility recovery on the next rebuild).
    const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
    if (tiles.length) {
      try { playReverseSpawn(Array.from(tiles)).catch(() => {}); } catch (_) {}
    }
  });
}

// Title "New Game" click → black transition → embers + music + the shell.
function onNewGameClicked() {
  withFlowBusy(() => {
    // NOTE: the title ambient (grass/particles) is NOT stopped here — it's
    // stopped automatically by showScreen() when the title is hidden (which
    // happens at the black-midpoint swap below). Stopping at click killed the
    // grass instantly under a still-visible screen (Chloe 2026-08-03).
    // Theme FADES OUT at click (Chloe 2026-08-02: "let the entire mp3 play
    // out, don't cut it") — ramps to silence over ~1.5s, no hard cut.
    fadeOutThemeMusic(fableRoot);
    return revealNewGameShell({ startMusic: true });
  });
}

// Exit the New Game flow back to the title (⌂ click). Fades the new-game
// tracks out + restarts the title theme (mirrors the old split back()).
function exitFlowToTitle() {
  // (2026-08-19 Chloe) ⌂ kills ANY in-flight creator GLM turn (wizard chat /
  // import / lorebook batch / pencil edit). The creator screen is NOT
  // re-rendered on the exit, so without this the stale-turn firewall never
  // fired and the turn kept streaming over the title screen.
  abortCreatorTurn(screens['creator-chat']);
  stopNewGameMusic(fableRoot);
  startThemeMusic(fableRoot);
  stopFlowAmbiance();
  if (flowChrome) {
    flowChrome.hideBack();
    // Hide ⌂ home too so it doesn't linger over the title/main menu after
    // exiting the flow (the chrome overlay persists; both buttons go dark).
    flowChrome.hideHome();
  }
  flowState = freshFlowState();
  showScreen('title');
}

// Exit the LOAD flow (worlds/saves pickers) back to the title (⌂ home from the
// worlds screen). Fades the new-game ambience out + restarts the title theme —
// mirrors exitFlowToTitle. Also hides the flow-chrome buttons so ‹ + ⌂ don't
// linger over the title (the chrome overlay persists; both buttons go dark). The Load
// menu shares the New Game music + ember background, so it shares the teardown
// too. This is an instant screen swap (no transition), so no withFlowBusy wrap
// — the title buttons are immediately usable again.
function exitLoadToTitle() {
  // (2026-08-19) Same kill-any-creator-turn contract as exitFlowToTitle.
  abortCreatorTurn(screens['creator-chat']);
  stopNewGameMusic(fableRoot);
  startThemeMusic(fableRoot);
  stopFlowAmbiance();
  if (flowChrome) {
    flowChrome.hideBack();
    flowChrome.hideHome();
  }
  showScreen('title');
}

// === NEW GAME FLOW — the 4 picker chain (burn/reverse-spawn) ===============
//
// THE BURN CONTRACT: when a tile is clicked, the CLICKED tile POPS (scale
// burst) then FADES OUT; the OTHER tiles BURN bottom→top. The ignition whoosh
// (assets/Incinerate.mp3) fires on every burn. Each step transition receives
// `selectedBtn` (the clicked one) + builds the rejected list as "all sibling
// tiles except the clicked one" (siblingTilesExcept → burnPairTile).
//
// THE ORDER (player-first):
//   Slide 1: Player pair (NEW PLAYER / LOAD PLAYER / IMPORT)
//     → Create/Import Player → burn → GLM Player Wizard → SIM pair
//     → Load Player          → burn → Player Picker → select → SIM pair
//     (Load-menu per-card NEW entry: the card is preset, so every player
//      resolution routes through flowAfterPlayer → launchGame — slides 2 + 3
//      never show)
//   Slide 2: SIM pair (NEW / LOAD / IMPORT SIM CARD) → establish card (the
//            LOAD step shows the same review-card modal as the Load menu —
//            NEW on the modal selects the card; 2026-08-20) →
//            advanceFromSim (skips the Codex picker when a codex exists)
//   Slide 3: Codex pair (CREATE / CONTINUE-WITHOUT / IMPORT) — skipped if codex
//   → launchGame
// The INTRO step is GONE (2026-08-15, Chloe): the SIM Wizard itself must ask
// the mandatory intro question (yes → what should it say / no → confirmed
// none) before its draft can complete — serializeSimCard writes the agreed
// `<intro>` sibling in-file, so no post-card step exists. A card that already
// has a codex launches the instant it's established. Each GLM wizard
// (player/sim/codex) is driven by creator-chat.js.

// === Flow pair tiles (the shared picker language) ========================
// Every picker slide (Player / SIM / Codex) is a pair of caption slabs
// (+ an optional IMPORT mini tile) in the newgame-split host, revealed by the
// reverse-spawn + burned on click. buildFlowPairTiles generalizes the old
// Player-pair-only builders so all pickers share one code path.

// Build an arbitrary pair of flow tiles (+ optional IMPORT mini tile) into the
// newgame-split host. `pair` is [{caption, act, onClick} x2]; `importTile` is
// {caption, onClick} | null. Tiles ship opacity:0 for the reverse-spawn. Any
// prior IMPORT mini tile is stripped first (re-entry / picker switch), unless
// a fresh one is appended.
function buildFlowPairTiles({ pair, importTile = null }) {
  // Clear any leftover cinematic launch fade so a re-shown picker isn't invisible
  // (launchGame stamps .is-launching on the screen it faded; it persists on disk
  // across the stage swap + a later New Game entry re-shows this host).
  screens['newgame-split'].classList.remove('is-launching');
  const tiles = rebuildSplitTiles(pair.map((p) => ({ caption: p.caption, act: p.act })));
  pair.forEach((p, i) => {
    if (tiles[i]) tiles[i].addEventListener('click', (e) => p.onClick(e.currentTarget));
  });
  tiles.forEach((t) => { t.style.opacity = '0'; });
  if (importTile) {
    buildFlowImportTile(importTile);
  } else {
    const host = screens['newgame-split'].querySelector('.fable-newgame-tiles');
    if (host) host.querySelectorAll('.fable-newgame-tile-mini').forEach((el) => el.remove());
  }
  return tiles;
}

// Build the IMPORT mini tile — a smaller silver slab centered below the pair.
// Clicking it runs `onClick` (each picker wires its own import handler).
function buildFlowImportTile({ caption, onClick }) {
  const split = screens['newgame-split'];
  const host = split.querySelector('.fable-newgame-tiles');
  // Strip any prior mini tile (re-entry / picker switch rebuilds it).
  host.querySelectorAll('.fable-newgame-tile-mini').forEach((el) => el.remove());
  const tile = document.createElement('button');
  tile.className = 'fable-newgame-tile fable-newgame-tile-mini fable-flow-spawn';
  tile.type = 'button';
  tile.dataset.act = 'import';
  tile.innerHTML = `<span class="fable-newgame-tile-caption">${tileCaptionHTML(caption)}</span>`;
  tile.style.opacity = '0';
  tile.addEventListener('click', (e) => onClick(e.currentTarget));
  host.appendChild(tile);
  return tile;
}

// The Player pair (Create / Load) + IMPORT — slide 1 of the flow.
function buildPlayerPairTiles() {
  return buildFlowPairTiles({
    pair: [
      { caption: 'NEW PLAYER', act: 'create-player', onClick: (b) => flowCreatePlayer(b) },
      { caption: 'LOAD PLAYER', act: 'load-player', onClick: (b) => flowLoadPlayer(b) },
    ],
    importTile: { caption: 'IMPORT', onClick: (b) => flowImportPlayer(b) },
  });
}

// Rebuild the split screen's tiles with the given labels + return the fresh
// tile elements (clone-detached so no handler stacks). Used by every pair
// render so the burn/spawn engine always sees clean buttons. Each tile is a
// caption-only dark panel; `caption` is split into stacked lines per the
// word-count rule (engine/tile-caption.js). The pair is wrapped in a
// .fable-newgame-tile-row so the host's column layout keeps the pair on one
// row + lets the IMPORT mini tile sit centered BELOW it as a sibling.
function rebuildSplitTiles(labels) {
  const split = screens['newgame-split'];
  const host = split.querySelector('.fable-newgame-tiles');
  host.innerHTML = `<div class="fable-newgame-tile-row">${labels.map((l) => `
    <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="${l.act}">
      <span class="fable-newgame-tile-caption">${tileCaptionHTML(l.caption)}</span>
    </button>
  `).join('')}</div>`;
  return Array.from(host.querySelectorAll('.fable-newgame-tile'));
}

// All sibling tiles except the clicked one (the burn targets). Walks up to the
// `.fable-newgame-tiles` host first so the layout resolves all tiles as
// siblings regardless of nesting.
function siblingTilesExcept(selectedBtn) {
  const host = selectedBtn.closest('.fable-newgame-tiles') || selectedBtn.parentElement;
  return Array.from(host.querySelectorAll('.fable-newgame-tile'))
    .filter((b) => b !== selectedBtn);
}

// Create Player: clicked pops+fades, the other burns, → GLM Player Wizard.
// The wizard converses with the API, shows a review card (with a clickable
// portrait slot → cropper), then writes via fable_player_write. On CREATE it
// carries the new player id (+ draft, for the starting conditions) into the
// sim step.
function flowCreatePlayer(selectedBtn) {
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      showScreen('creator-chat');
      setFlowStep('creator-chat');
      renderCreatorChat(screens['creator-chat'], {
        creatorKind: 'player',
        onCreated: ({ playerId, draft }) => {
          flowState.selectedPlayerId = playerId;
          flowState.playerDraft = draft;
          flowAfterPlayer();
        },
        back: () => returnToPlayerPair(),
      });
    },
  });
}

// Import Player: opens the SillyTavern .png/.json picker, parses the file,
// stashes the result on flowState.pendingImport, then burns into the Player
// Wizard (same destination as Create Player) with the import pre-seeded. On
// cancel/no file, the tile stays put (no burn, no flow advance). The pair
// tiles are the burn rejects when this tile is clicked.
async function flowImportPlayer(selectedBtn) {
  if (flowBusy) return;
  let result;
  try {
    result = await parseImportFile(screens['newgame-split']);
  } catch (e) {
    bottomWarning(`Import failed: ${e.message || e}`);
    return;
  }
  if (!result) return; // picker cancelled
  // (2026-08-19) A standalone lorebook is CODEX material — it has no
  // character to build a player card from. Bottom warning, stay on the
  // picker (never burn into a wizard that cannot use the file).
  if (result.lorebook && !result.charData) {
    bottomWarning('That file is a lorebook (world lore entries), not a character — import it at the CODEX step.');
    return;
  }
  // (2026-08-15 audit fix) Re-check flowBusy AFTER the dialog await: two
  // rapid clicks both pass the pre-await guard, then both resolve past the
  // picker — the second would burn/render over the first's chain.
  if (flowBusy) return;
  flowState.pendingImport = result;
  // (2026-08-18) The import's greetings (first_mes + alternate_greetings) NO
  // LONGER ride toward the SIM step from a player-side import — that carry
  // seeded an unrelated fresh world's <intro> with this card's opening beat.
  // They survive only in charData (the wizard's import context); a SIM card
  // gets an import's greetings only via the SIM pair's own IMPORT tile.
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      // Pull + clear the import we just stashed, then render the Player
      // Wizard with it pre-seeded. Inlined here (not delegated to
      // flowCreatePlayer) so we don't double-burn — the burn already fired
      // above with the IMPORT tile as the selected button.
      const importSeed = flowState.pendingImport;
      flowState.pendingImport = null;
      showScreen('creator-chat');
      setFlowStep('creator-chat');
      renderCreatorChat(screens['creator-chat'], {
        creatorKind: 'player',
        presetImportData: importSeed && importSeed.charData,
        presetPortraitDataUrl: importSeed && importSeed.portraitDataUrl,
        presetPortraitExt: importSeed && importSeed.portraitExt,
        presetPortraitBytes: importSeed && importSeed.portraitBytes,
        onCreated: ({ playerId, draft }) => {
          flowState.selectedPlayerId = playerId;
          flowState.playerDraft = draft;
          flowAfterPlayer();
        },
        back: () => returnToPlayerPair(),
      });
    },
  });
}

// Load Player: clicked pops+fades, the other burns, → Player Picker.
function flowLoadPlayer(selectedBtn) {
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      renderPlayerPickerStep();
    },
  });
}

// Show the Player Picker with its LOAD/EDIT handlers wired.
function renderPlayerPickerStep() {
  showScreen('player-picker');
  setFlowStep('player-picker');
  renderPlayerPicker(screens['player-picker'], {
    onSelect: (player) => advanceAfterPlayer(player.id),
    onEdit: (player) => flowEditPlayer(player),
  });
}

// Edit a saved player (2026-08-20, Chloe): open the SAME raw-XML editor the
// SIM Load menu uses — the wizard-chat edit run is RETIRED. The `.player`
// XML is Rust-owned (render_player_xml / parse_player_xml); the edit
// round-trips through the server-side parse gate before any disk touch.
function flowEditPlayer(player) {
  openPlayerRawEditor(player);
}

// Route after the player step resolves (CREATE / IMPORT / EDIT / LOAD). When
// the flow entered from the Load menu's per-card NEW (beginNewGameFromCard),
// the SIM card is ALREADY established (flowState.selectedCardId) — skip the
// SIM pair + the Codex pair and launch straight into that world. Otherwise
// the SIM pair is the next step (the title-entry flow).
function flowAfterPlayer() {
  if (flowState.selectedCardId) launchGame(flowState.selectedCardId);
  else flowSimPair();
}

// Route after a player is chosen (Load). (Loaded players have no draft;
// flowState.playerDraft stays null → no transient starting conditions are
// seeded at launch.)
function advanceAfterPlayer(playerId) {
  flowState.selectedPlayerId = playerId;
  // (P2 fix) Clear any CREATEd player's draft: after CREATE → back → LOAD,
  // the stale draft made the LOADED player inherit the CREATED one's
  // wealth/reputation via buildStartingConditions.
  flowState.playerDraft = null;
  flowAfterPlayer();
}

// === Sim World Wizard ====================================================
// The GLM sim wizard gathers the MANDATORY world anchors (date/weather via
// <start>, the travel graph via <locations>, the opening cast via <cast>) so
// the tracker has them from turn 1. Reached from the SIM pair (NEW / IMPORT).
// `presetImport` seeds the wizard from a SillyTavern import; `presetIntro`
// carries THAT import's greetings into the card's <intro> (passed directly by
// flowImportSim — the only source since 2026-08-18). On CREATE →
// advanceFromSim (the content-aware skip matrix).
function flowCreateSim(presetImport = null, presetIntro = null) {
  showScreen('creator-chat');
  setFlowStep('creator-chat');
  renderCreatorChat(screens['creator-chat'], {
    creatorKind: 'sim',
    presetImportData: presetImport,
    presetIntro,
    onCreated: ({ cardId, draft }) => {
      flowState.selectedCardId = cardId;
      flowState.simDraft = draft;
      advanceFromSim(cardId);
    },
    back: () => flowSimPair(),
  });
}

// === SIM / Codex pickers + content-aware skip ============================
// The New Game flow past the player step is a chain of tile pickers (each
// reusing the newgame-split tile language) — SIM pair → Codex pair — ending
// in launchGame. `advanceFromSim` skips the Codex picker when the established
// card already has a codex (a loaded world with lore launches immediately).

// Burn the clicked pair tile + its siblings, then run `next` once the burn
// completes. Centralizes the per-picker burn boilerplate (every picker tile
// pops+fades itself + burns its siblings before advancing).
function burnPairTile(selectedBtn, next) {
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => { setFlowBusy(false); next(); },
  });
}

// Reverse-spawn whatever tiles currently sit in the newgame-split host (called
// after every buildFlowPairTiles so the picker enters with the spawn anim).
function spawnFlowTiles() {
  const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
  if (tiles.length) playReverseSpawn(Array.from(tiles));
}

// --- SIM pair: NEW SIM CARD / LOAD SIM CARD / IMPORT ---------------------
function flowSimPair() {
  buildFlowPairTiles({
    pair: [
      { caption: 'NEW SIM CARD', act: 'new-sim', onClick: (b) => burnPairTile(b, () => flowCreateSim()) },
      { caption: 'LOAD SIM CARD', act: 'load-sim', onClick: (b) => flowLoadSim(b) },
    ],
    importTile: { caption: 'IMPORT', onClick: (b) => flowImportSim(b) },
  });
  showScreen('newgame-split');
  setFlowStep('sim-pair');
  spawnFlowTiles();
}

// LOAD SIM CARD → the roleplay-card grid (worlds.js pick mode). Picking a card
// establishes it as the world + builds a best-effort simDraft from its meta,
// then advanceFromSim decides which pickers to skip.
function flowLoadSim(selectedBtn) {
  burnPairTile(selectedBtn, () => renderSimPickerStep());
}

function renderSimPickerStep() {
  showScreen('worlds');
  setFlowStep('sim-picker');
  // (2026-08-20 Chloe ruling) The sim picker shows the SAME review-card
  // modal the Load menu does — clicking a card must NEVER auto-launch
  // straight into the game (the old pickMode grid bypassed the review
  // card, so the EDIT/DELETE buttons were unreachable from this step).
  // The modal's NEW continues THIS flow (the player was already chosen at
  // slide 1 — no player re-pick, no double-ask): select the card +
  // advance. LOAD / EDIT / DELETE behave exactly as on the Load menu.
  renderWorlds(screens['worlds'], {
    onNewGame: (card) => {
      flowState.selectedCardId = card.id;
      flowState.simDraft = {
        name: card.name,
        tone: card.tone || null,
        setting: card.setting_preview || null,
      };
      advanceFromSim(card.id);
    },
    onResume: (card) => openWorldSaves(card),
    onEdit: (card) => openCardRawEditor(card),
  });
}

// IMPORT SIM CARD → SillyTavern png/json → seed the SIM wizard (same dest as
// NEW), carrying the import's greetings into the card's <intro>.
async function flowImportSim(selectedBtn) {
  if (flowBusy) return;
  let result;
  try {
    result = await parseImportFile(screens['newgame-split']);
  } catch (e) {
    bottomWarning(`Import failed: ${e.message || e}`);
    return;
  }
  if (!result) return; // picker cancelled
  // (2026-08-19) A standalone lorebook is CODEX material — a SIM card is a
  // character/scenario/world, not a pile of lore entries. Bottom warning,
  // stay on the picker.
  if (result.lorebook && !result.charData) {
    bottomWarning('That file is a lorebook (world lore entries), not a sim card — import it at the CODEX step.');
    return;
  }
  // (2026-08-15 audit fix) Re-check flowBusy AFTER the dialog await (see
  // flowImportPlayer): two rapid clicks both passed the pre-await guard.
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      flowCreateSim(result.charData, result.introText || null);
    },
  });
}

// --- Content detection + the skip matrix ---------------------------------
// Best-effort: a card "has" a codex when its .codex sibling is non-empty.
// Runs before any active game exists, so it uses the by-id variant.
async function detectHasCodex(cardId) {
  try {
    const r = await invoke('fable_codex_get_by_id', { cardId });
    return !!(r && r.raw && r.raw.trim());
  } catch (_) { return false; }
}

// Route after a sim card is established (NEW / LOAD / IMPORT). Skips the
// Codex picker when a codex already exists — a loaded world with lore
// launches immediately. The intro question is the SIM Wizard's job now (asked
// in-chat before its draft can complete), so there is no intro picker to skip.
async function advanceFromSim(cardId) {
  const hasCodex = await detectHasCodex(cardId);
  if (hasCodex) {
    launchGame(cardId);
  } else {
    flowCodexPair(cardId, { afterCodex: () => launchGame(cardId) });
  }
}

// --- Codex pair: CREATE SIM CODEX / CONTINUE WITHOUT CODEX / IMPORT ------
// The LAST picker slide: any codex choice (create / skip / import) ends in
// `afterCodex` — normally launchGame.
function flowCodexPair(cardId, { afterCodex } = {}) {
  const done = afterCodex || (() => launchGame(cardId));
  buildFlowPairTiles({
    pair: [
      { caption: 'CREATE SIM CODEX', act: 'create-codex', onClick: (b) => burnPairTile(b, () => flowCreateCodex(cardId, null, done)) },
      { caption: 'CONTINUE WITHOUT CODEX', act: 'no-codex', onClick: (b) => burnPairTile(b, done) },
    ],
    importTile: { caption: 'IMPORT', onClick: (b) => flowImportCodexPair(b, cardId, done) },
  });
  showScreen('newgame-split');
  setFlowStep('codex-pair');
  spawnFlowTiles();
}

function flowCreateCodex(cardId, presetImport, afterCodex, presetLorebook = null) {
  const done = afterCodex || (() => launchGame(cardId));
  showScreen('creator-chat');
  setFlowStep('creator-chat');
  renderCreatorChat(screens['creator-chat'], {
    creatorKind: 'codex',
    cardId,
    // With a lorebook payload the batched lore conversion owns the run — no
    // character import block rides along (the book IS the payload).
    presetImportData: presetLorebook ? null : presetImport,
    presetLorebook,
    onCreated: () => done(),
    back: () => flowCodexPair(cardId, { afterCodex: done }),
  });
}

async function flowImportCodexPair(selectedBtn, cardId, afterCodex) {
  if (flowBusy) return;
  let result;
  try {
    result = await parseImportFile(screens['newgame-split']);
  } catch (e) {
    bottomWarning(`Codex import failed: ${e.message || e}`);
    return;
  }
  if (!result) return; // picker cancelled
  // (2026-08-19 Chloe) The CODEX import is a LOREBOOK import. The payload is
  // the mechanically-extracted entries: a standalone world book directly, or
  // the character_book embedded in an imported character card. Anything else
  // is unrecognized → bottom warning, never a wizard/chat window.
  const lorebook = result.lorebook
    || (result.charData && result.charData.character_book
      ? {
        name: (result.charData.name || result.charData.character_book.name || 'Imported lorebook').slice(0, 80),
        entries: extractLorebookEntries(result.charData.character_book),
      }
      : null);
  if (!lorebook || !lorebook.entries.length) {
    bottomWarning('No lorebook entries found in that file — expected a SillyTavern world book JSON (or a card with an embedded lorebook).');
    return;
  }
  // (2026-08-15 audit fix) Re-check flowBusy AFTER the dialog await (see
  // flowImportPlayer): two rapid clicks both passed the pre-await guard.
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      flowCreateCodex(cardId, result.charData, afterCodex, lorebook);
    },
  });
}

// (The intro pair + nudge collector were REMOVED 2026-08-15, Chloe: the SIM
// Wizard asks the mandatory intro question itself + serializeSimCard embeds
// the agreed `<intro>` sibling — see creator-chat.js + card-serialize.js.)

// Sync the flow-chrome ‹ button to the current step. Slide 1 (the Player
// pair, step === 'player') hides ‹ unless the entry had a meaningful back
// destination (flowState.slideOneHasBack — the Load-menu entry's worlds-grid
// return); every deeper slide shows ‹. Called by setFlowStep at every
// transition + by revealNewGameShell on entry.
function syncFlowBack() {
  if (!flowChrome) return;
  if (flowState.step === 'player') {
    if (flowState.slideOneHasBack) flowChrome.showBack(); else flowChrome.hideBack();
  } else {
    flowChrome.showBack();
  }
}

// Set flowState.step + sync the ‹ button in one step so every transition
// keeps the back button's visibility honest (slide 1 hidden for the title
// entry, visible everywhere else). Use this instead of bare `flowState.step =`.
function setFlowStep(step) {
  flowState.step = step;
  syncFlowBack();
}

// Return to the Player pair (slide 1) — rebuild the tiles + reverse-spawn them.
function returnToPlayerPair() {
  buildPlayerPairTiles();
  showScreen('newgame-split');
  setFlowStep('player');
  const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
  if (tiles.length) playReverseSpawn(Array.from(tiles));
}

// ‹ back routing — depends on the current step. `onBack` is the Load-menu
// entry's worlds-grid return (null for the title entry → title).
function flowBack(onBack) {
  switch (flowState.step) {
    case 'creator-chat': {      // A GLM wizard → its configured back step
      const cb = screens['creator-chat'] && screens['creator-chat']._creatorBack;
      if (cb) cb(); else exitFlowToTitle();
      break;
    }
    case 'player-picker':       // Player picker → Player pair (slide 1)
      returnToPlayerPair();
      break;
    case 'sim-picker':          // LOAD SIM CARD grid → SIM pair
      flowSimPair();
      break;
    case 'sim-pair':            // SIM pair → Player pair (slide 1)
      returnToPlayerPair();
      break;
    case 'codex-pair':          // Codex pair → SIM pair
      flowSimPair();
      break;
    case 'player':              // Player pair → prior screen or title
      if (onBack) onBack(); else exitFlowToTitle();
      break;
    default:
      if (onBack) onBack(); else exitFlowToTitle();
  }
}

// Start a fresh game from a card (+ optional saved player): stop the
// Extract TRANSIENT starting gameplay conditions (wealth/reputation/fame)
// from the Player Wizard draft. These seed `PlayerState` at attach — they
// are NOT persisted on the SavedPlayer identity (§6C identity-only lock).
// Leading-integer parse: "200 gold" → 200, "-20" → -20, "famous" → null.
// Returns null when neither is present (the common case → fable_start gets
// no arg → PlayerStartingConditions defaults to None server-side).
function buildStartingConditions(draft) {
  if (!draft) return null;
  const leadInt = (v) => {
    if (v == null) return null;
    const m = String(v).trim().match(/-?\d+/);
    return m ? parseInt(m[0], 10) : null;
  };
  const wealth = leadInt(draft.wealth);
  let reputation = leadInt(draft.reputation);
  if (reputation == null) reputation = leadInt(draft.fame);
  if (wealth == null && reputation == null) return null;
  const conds = {};
  if (wealth != null) conds.wealth = Math.max(0, wealth);                       // u32
  if (reputation != null) conds.reputation = Math.max(-2147483648, Math.min(2147483647, reputation)); // i32
  return conds;
}

// === The cinematic launch ===============================================
// The terminal step for every New Game path: fade the flow UI to leave only
// the background + music, run fable_start (the "schema captured" wait), then
// stop the music + fade to black + enter the stage. (The behind-the-fade
// intro generation is GONE with the intro step — the SIM Wizard's draft
// already carries the agreed `<intro>` in-file.)

// Fade whatever screen is currently visible (the picker tiles or the chat
// shell) to opacity 0 + hide the flow chrome, leaving the .fable-flow-ambiance
// background + the new-game music playing while the backend works.
function fadeFlowToLoading() {
  if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
  const current = fableRoot.querySelector('.fable-screen:not([hidden])');
  if (current) current.classList.add('is-launching');
}

// The terminal step: fade UI (background + music hold) → fable_start (schema
// capture) → stop music + fade to black + stage.
async function launchGame(cardId) {
  // Guarded: a picker tile's burn-onComplete or the intro Enter could
  // double-fire. enterStageViaTransition clears the flag.
  if (flowBusy) return;
  setFlowBusy(true);
  fadeFlowToLoading();
  // fable_start: seat the card, bootstrap the schema anchors (clock/weather/
  // location), seed the tracker. The new-game music keeps playing during this
  // wait (the "background + music" hold); it stops further below.
  // NOTE: the title ambient is stopped inside enterStageViaTransition (right
  // before the stage shows), not here — so the grass doesn't vanish early.
  // endFableSession also signals every cancel slot first — a prior mid-turn
  // exit that outran its unwind window is cleared here instead of erroring
  // "a game is already running".
  await endFableSession();
  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', {
      cardId,
      fresh: true,
      playerId: flowState.selectedPlayerId,
      // Transient starting wealth/reputation from the Player Wizard draft
      // (null when the AI never asked → omitted → defaults). See buildStartingConditions.
      playerStartingConditions: buildStartingConditions(flowState.playerDraft),
    });
    engineStarted = true;
    if (result && result.intro) openingScene = result.intro;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] fable_start (new game) failed — returning to title', err);
    // (2026-08-15 audit fix) NEVER enter the stage without a seated engine —
    // the old path showed a dead stage (no card, first turn guaranteed to
    // fail). Surface the error + return to the title instead.
    engineStarted = false;
    fadeOutThemeMusic(fableRoot);
    stopNewGameMusic(fableRoot);
    // (2026-08-16 audit M8c) Tear the flow ambiance down too — the success
    // path does it inside enterStageViaTransition; without this the embers
    // RAF kept running invisibly behind the silent title.
    stopFlowAmbiance();
    showScreen('title');
    setFlowBusy(false);
    toast(`Could not start the game: ${err?.message || err}`);
    return;
  }
  // Fade-to-black + stage swap. Stop the new-game music + fade the title theme
  // HERE (after the schema-capture wait) so the stage opens in silence.
  fadeOutThemeMusic(fableRoot);
  stopNewGameMusic(fableRoot);
  enterStageViaTransition(openingScene, loadMessages);
}


// The shared "play the magical transition + swap to stage + wire it" tail.
// Used by every start/resume path.
function enterStageViaTransition(openingScene, loadMessages, opts = {}) {
  // Leaving the New Game flow for the stage — hide the flow chrome entirely
  // so ‹ / ⌂ don't linger over the stage (the Wupi top bar owns stage nav).
  if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
  // The persistent flow ambiance (embers/glow) is a flow-only backdrop; the
  // stage has its own scene background, so tear it down here.
  stopFlowAmbiance();
  // The fade-through-black "magical transition" was removed — the stage now
  // opens immediately with no overlay. Stop the title ambient (grass/particles)
  // + show + wire the stage directly.
  if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  const revealStage = () => {
    showScreen('stage');
    try {
      if (screens.stage) {
        wireStage(screens.stage, {
          cardContext: null,
          onExit: returnToTitle,
          // (2026-08-16 audit fix #26) The drawer's Load foot button: exit to
          // the title + open the SAME worlds→saves picker the title's LOAD
          // opens (mid-session save-swaps go through the standard flow; the
          // in-flight-turn guards live on the backend).
          // (P2c, 2026-08-17 E4B shakedown) The load-flow entry is a
          // multi-step async chain (stage teardown → title → transition →
          // worlds grid). A failure anywhere in the middle used to leave the
          // app SCREENLESS (0×0 drawer, no visible screen, flow-ambience
          // stuck — only a webview reload recovered). withShellBusy drops
          // overlapping input during the chain + the catch guarantees SOME
          // screen is shown.
          onLoad: () => {
            void withShellBusy(async () => {
              try {
                await returnToTitle();
                await onLoadClicked();
              } catch (e) {
                console.error('[fable] Load flow failed mid-chain — recovering to a visible screen', e);
                try {
                  // If the worlds grid never rendered, land on the title (the
                  // pre-flow state) + kill any half-started flow ambience.
                  if (!(screens.worlds && !screens.worlds.hidden)) {
                    stopFlowAmbiance();
                    showScreen('title');
                  }
                } catch (_) { /* last resort: leave whatever is visible */ }
              }
            });
          },
          openingScene,
          loadMessages,
        });
      }
    } catch (e) {
      console.error('[fable] wireStage threw (stage shown, some features may degrade)', e);
    }
    // Stage entry is the terminal step of every start/resume path — clear the
    // double-click guard here so the flag set by resumeSave / launchGame is
    // always released.
    setFlowBusy(false);
    // Direct launch (.lnk): reveal EXACTLY like the fable.exe title — the 1s
    // top-to-bottom wipe over the desktop, the entry hold dropping only at
    // wipe end (runEntryWipe owns both). Normal entries (title Continue /
    // Load / New Game) reveal instantly, as before.
    if (opts.underSplash) runEntryWipe(fableRoot, screens.stage);
  };
  // Direct launch: the stage must not spawn beside the F splash — wait out
  // the remainder of the 2s splash window (0ms if the save load already
  // outlasted it, e.g. a slow fable_start). While it waits, everything stays
  // transparent: only the F floats over the desktop, then the stage wipes in.
  const wait = opts.underSplash && opts.launchT0
    ? Math.max(0, FABLE_SPLASH_HOLD_MS - (performance.now() - opts.launchT0))
    : 0;
  if (wait > 0) setTimeout(revealStage, wait);
  else revealStage();
}


// (2026-08-19 Chloe) The universal game-exit funnel: leaving a live roleplay
// (the drawer's Home button, or any launch path that must clear a prior
// session) STOPS ALL GENERATION, not just navigating away. Signal every
// reserved cancel slot — the narrator turn + edit retrack (`fable_stop`: the
// API stream AND the local tracker decode), the golden-pencil slice regen
// (`fable_slice_stop`), and the Wupi drawer's local chat decode (`chat_stop`)
// — then end the session, RETRYING while a cancelled turn is still unwinding:
// `fable_stop` is signal-only and `fable_end` refuses mid-turn by design, so
// the unwind takes a moment and the bare stop→end pair could error out and
// leave the engine alive server-side. All signals are safe no-ops when idle.
const FABLE_END_UNWIND_RETRIES = 10;
const FABLE_END_UNWIND_DELAY_MS = 150;
async function endFableSession() {
  try { await invoke('fable_stop'); } catch (_) {}
  try { await invoke('fable_slice_stop'); } catch (_) {}
  try { await invoke('chat_stop'); } catch (_) {}
  for (let i = 0; ; i++) {
    try {
      await invoke('fable_end');
      return;
    } catch (e) {
      const msg = String((e && e.message) || e);
      if (i < FABLE_END_UNWIND_RETRIES && msg.includes('still in flight')) {
        await new Promise((res) => setTimeout(res, FABLE_END_UNWIND_DELAY_MS));
        continue;
      }
      console.error('[fable] fable_end failed', e);
      return;
    }
  }
}

// Return from the stage to the Fable title screen. Wired as the stage's
// `onExit` hook (the Home button in the Wupi drawer footer calls it).
// Tears down the stage's engine modules + listeners so re-entry is clean,
// shuts down the FableEngine (the load-bearing fix: prior version leaked the
// engine — the next fable_start would error "a game is already running"),
// then swaps back to the title + restarts the title ambient + music.
// No magical transition here — the return is instant (the user is leaving a
// game, not entering one; the cinematic is for entry only).
async function returnToTitle() {
  // Shut down the FableEngine BEFORE teardown so the narrator thread is gone
  // by the time wireStage nulls its refs. endFableSession stops EVERY
  // in-flight generation first (narrator/tracker turn, slice regen, drawer
  // chat) + waits out the unwind, then fable_end persists the session +
  // schema per-card (best-effort), joins the engine thread + restores the
  // pre-game active_card_id server-side. Idempotent + safe if the engine
  // never started (engineStarted gate avoids a needless IPC round-trip on
  // the no-engine degrade path).
  if (engineStarted) {
    await endFableSession();
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
// DEV PREVIEW ENTER: pure-frontend layout preview (?dev=preview). Drops
// straight into the chat stage with placeholder messages — no backend, no
// model, no IPC. The 10 messages exercise the VN prose renderer (dialogue
// quotes, *italics*, multi-line beats) + give enough scroll height to test
// the scroll wheel + long-conversation layout. Continues the tavern/Aldric
// scene so the prose reads naturally.
function devPreviewEnter() {
  const now = Date.now();
  const loadMessages = [
    {
      role: 'assistant',
      content: 'The tavern door creaks open, spilling rain across the warped floorboards. A hooded figure slips inside, shaking the wet from a travel-worn cloak. "The roads are flooded," the stranger mutters, *glancing at the empty tables*. "I need a room. Just for tonight."',
      timestamp: now - 1000 * 60 * 10,
    },
    {
      role: 'user',
      content: 'You step behind the bar and pull a copper key from the hook. "Third door on the left. Three silvers." Your eyes drift to the muddy blade at the stranger\'s hip — *a knight\'s arming sword, recently drawn*.',
      timestamp: now - 1000 * 60 * 9,
    },
    {
      role: 'assistant',
      content: 'The stranger drops three coins onto the bar without counting them. "You\'re kind. Most innkeepers turn me out on sight." *A tired smile flickers across a weathered face.* "The name\'s Aldric. If anyone comes asking — I was never here."',
      timestamp: now - 1000 * 60 * 8,
    },
    {
      role: 'user',
      content: '"Your secret\'s safe. I don\'t ask questions." You sweep the coins into your palm and turn back to the hearth, but the image of that blade lingers. *A knight\'s sword, in the hands of a fugitive.*',
      timestamp: now - 1000 * 60 * 7,
    },
    {
      role: 'assistant',
      content: 'Minutes pass. The fire pops and settles. Then — hoofbeats outside, slowing. Three riders, by the rhythm. Aldric\'s hand goes white-knuckled on the stairpost. "They found me faster than I thought." *His voice is barely a whisper.* "Is there a back way out? A cellar? Anything?"',
      timestamp: now - 1000 * 60 * 6,
    },
    {
      role: 'user',
      content: 'You jerk your chin toward the kitchen. "Through the pantry, past the flour sacks — root cellar door behind them. Tunnel comes up by the old mill." You start wiping down the bar, casual. "I never saw you come in. You were never here."',
      timestamp: now - 1000 * 60 * 5,
    },
    {
      role: 'assistant',
      content: 'The door bursts open before Aldric can move. Three figures in rain-darkened tabards — a falcon crest you don\'t recognize. The lead rider scans the empty room, water dripping from his beard. "We\'re looking for a man. Tall, dark-haired, travels armed." *His hand rests on a worn sword pommel.* "Seen anyone tonight?"',
      timestamp: now - 1000 * 60 * 4,
    },
    {
      role: 'user',
      content: '"Just me and the storm." You keep polishing a mug, not meeting his eyes. "Slow night. Roads are a mess — anyone with sense stopped miles back." You set the mug down. "Can I pour your men something? Ale\'s warm, but it\'s dry." *Buying time, every second Aldric gets closer to the mill.*',
      timestamp: now - 1000 * 60 * 3,
    },
    {
      role: 'assistant',
      content: 'The rider studies you a long moment — then nods, once. "Two ales. And keep the change." His men shake the rain from their cloaks and crowd the hearth. *None of them glance at the kitchen.* But the leader pauses at the stair, frowning at the muddy bootprint Aldric left on the first step.',
      timestamp: now - 1000 * 60 * 2,
    },
    {
      role: 'user',
      content: '"Roof leaked earlier," you offer, already moving to the barrels. "Whole place is a mess tonight." You fill two tankards, hands steady. *Don\'t look at the stair. Don\'t hurry. Just pour.* Somewhere beneath your feet, faint and muffled, you hear the cellar door groan — then silence.',
      timestamp: now - 1000 * 60,
    },
  ];
  enterStageViaTransition(null, loadMessages);
}

function openFable() {
  if (!fableRoot) return;

  // ── FABLE ENTRY (#fable / fable.exe): skip the fog gate + boot. ──
  // Shows #fable + chrome + pauses the OS aurora directly (the cinematic path
  // defers this to the fog hold — here we skip straight there), then returns
  // BEFORE the fog/boot setup. No fog node, no ripple aura, no boot timers →
  // nothing for closeFable to cancel; currentFog + currentBoot stay null.
  // The title screen + theme music are held for FABLE_SPLASH_HOLD_MS so the
  // floating-F splash (#fable-entry-splash) owns the first ~2s, then the menu
  // reveals + music fades in as the splash crossfades out. Mirrors boot.js's
  // prefers-reduced-motion fast path in shape (no fog/boot), not in timing.
  //
  // DEV PREVIEW: pure-frontend layout preview. Show #fable + chrome, hide
  // every screen (clean black wait), then drop straight into the stage with
  // placeholder messages. No backend, no model, no IPC — devPreviewEnter is
  // synchronous. Checked BEFORE the fable branch because the two shortcuts are
  // mutually exclusive.
  if (DEV_PREVIEW_SHORTCUT) {
    fableRoot.classList.add('show');
    fableRoot.setAttribute('aria-hidden', 'false');
    activateChrome();
    if (hooks.pauseAurora) hooks.pauseAurora();
    for (const s of Object.values(screens)) s.hidden = true;
    stageActive = false;
    try { devPreviewEnter(); } catch (e) {
      console.error('[fable] dev-preview enter failed', e);
    }
    return;
  }

  // ── DIRECT LAUNCH (?direct=1, fable.exe --card <slug> [--save <id>]). ──
  // A desktop shortcut / direct exe launch that wants to boot straight into a
  // specific card+save, skipping the title. Same #fable/chrome/pauseAurora +
  // hide-all-screens setup as dev preview (clean wait under the floating-F
  // splash), then an async handoff: read the launch target from Rust
  // (get_launch_context), gate on the API being connected (Fable narration is
  // API-only — without it the stage would be dead; fall back to the title
  // where the ONLINE panel lives), then reuse resumeSave() which already does
  // fable_end → fable_start → enterStageViaTransition. Any failure falls back
  // to the title so a broken shortcut never strands the window.
  if (DIRECT_LAUNCH) {
    fableRoot.classList.add('show');
    fableRoot.setAttribute('aria-hidden', 'false');
    activateChrome();
    if (hooks.pauseAurora) hooks.pauseAurora();
    for (const s of Object.values(screens)) s.hidden = true;
    stageActive = false;
    // Transparency hold (see the FABLE_ENTRY branch above): while the
    // floating-F splash owns the screen, #fable's void backdrop stays
    // suppressed so nothing paints behind the F. The hold persists through
    // the WHOLE entry: the save load runs invisibly behind the F, then the
    // stage reveal (enterStageViaTransition's underSplash path) wipes in over
    // the desktop at the splash's 2s mark — the void only paints once the
    // stage has fully covered it, never as a blank pop. Title fallbacks drop
    // it the same way via revealTitleUnderSplash. html.fable-wipe is stamped
    // HERE because the stage reveal is LOAD-GATED (a slow fable_start can
    // outlast script.js's html.fable-entry teardown at ~2.6s) — without it
    // the body's dark base would pop in mid-load; with it the transparent
    // hold is continuous from t=0 until runEntryWipe drops it at wipe end.
    fableRoot.classList.add('fable-entry-hold');
    document.documentElement.classList.add('fable-wipe');
    const launchT0 = performance.now();
    // Title fallbacks honor the splash hold: reveal via the shared
    // choreography (revealTitleUnderSplash) DELAYED until the F splash's 2s
    // window has elapsed (0ms if the async handoff already outlasted it —
    // e.g. a slow IPC). Revealing bare made the menu spawn beside the F
    // (the handoff resolves in tens of ms; fixed 2026-08-14).
    const revealTitleAfterSplash = () => {
      const wait = Math.max(0, FABLE_SPLASH_HOLD_MS - (performance.now() - launchT0));
      setTimeout(() => revealTitleUnderSplash(fableRoot), wait);
    };
    (async () => {
      // (#74) A HUNG — not rejected — IPC (get_launch_context, or the
      // fable_start inside resumeSave) used to strand the window transparent
      // + pointer-events:none forever: the Rust 5s reveal fallback only
      // calls win.show(), it cannot drop frontend classes. Race the chain
      // against a 6s frontend deadline; on timeout reveal the title via the
      // shared choreography (it owns the hold classes end-to-end — no
      // manual stripping). A late chain resolution is harmless: the
      // underSplash stage entry simply replaces the title afterward.
      const DIRECT_LAUNCH_DEADLINE_MS = 6000;
      let settled = false;
      const chain = (async () => {
        try {
          const ctx = await invoke('get_launch_context');
          if (!ctx || !ctx.cardSlug) { revealTitleAfterSplash(); return; }
          // API gate: fable_start refuses without a connected API. Falling back
          // to the title (instead of a dead stage) lets the player connect via
          // the ONLINE button + retry — a persisted API connection carries over.
          // (P2 fix) Check BOTH flags: apiReady alone stays true after an
          // api_disconnect (the profile persists), which booted a dead stage
          // on a stale .lnk. Every other gate (title, stage, Rust's
          // require_api_for_fable) requires source === 'api'.
          const src = await invoke('model_source_get').catch(() => null);
          if (!src || src.source !== 'api' || !src.apiReady) { revealTitleAfterSplash(); return; }
          // underSplash: resumeSave → enterStageViaTransition delays the stage
          // to the 2s splash window + runs the shared entry wipe (the reveal is
          // identical to the fable.exe title path).
          await resumeSave(ctx.cardSlug, ctx.saveId ?? null, { underSplash: true, launchT0 });
        } catch (e) {
          console.error('[fable] direct launch failed, falling back to title', e);
          // (2026-08-16 audit fix #26) resumeSave armed flowBusy before its
          // fable_start threw — without this the title's buttons stayed dead
          // for the full 12s safety window after the fallback reveal.
          setFlowBusy(false);
          revealTitleAfterSplash();
        }
      })();
      chain.then(() => { settled = true; }, () => { settled = true; });
      await Promise.race([
        chain,
        new Promise((resolve) => setTimeout(resolve, DIRECT_LAUNCH_DEADLINE_MS)),
      ]);
      if (!settled) {
        console.error('[fable] direct launch timed out (hung IPC); revealing title + dropping entry hold');
        // (2026-08-16 audit M8c) Same flowBusy release the catch path got
        // (fix #26) — the timeout branch missed it, leaving the title's
        // buttons dead for the 12s safety window after the fallback reveal.
        setFlowBusy(false);
        revealTitleAfterSplash();
      }
    })();
    return;
  }

  if (FABLE_ENTRY) {
    // Reveal #fable + chrome + pause the OS aurora IMMEDIATELY, and show the
    // title screen RIGHT AWAY — held 100% transparent (.fable-title-held,
    // fable.css) behind the floating-F splash (#fable-entry-splash, driven by
    // script.js + the inline head script in wupi.html). Loading the menu
    // during the splash hold means every layer (bg image, dim, grass,
    // particles, clouds, wordmark) composites + warms BEFORE the player sees
    // anything, so the 2s reveal is a pure opacity fade with zero pop-in —
    // and WUPI's purple boot base never shows, because by the time anything
    // becomes visible the (fading-in) menu owns the pixels.
    fableRoot.classList.add('show');
    fableRoot.setAttribute('aria-hidden', 'false');
    activateChrome();
    if (hooks.pauseAurora) hooks.pauseAurora();
    // Suppress #fable's own void backdrop for the hold too (.fable-entry-hold,
    // styles.css): while the title is held transparent the opaque #05040a void
    // (a blue-violet near-black that reads as purple behind the warm glow)
    // must stay off — only the F floats over the desktop. It stays on through
    // the WHOLE reveal wipe (dropped at wipe end by revealTitleUnderSplash) so
    // the sweep reveals the desktop behind the menu — the void never pops in
    // as a blank window, it only paints once the title fully covers it.
    // html.fable-wipe is stamped HERE (not just at reveal) so the body stay
    // transparent is CONTINUOUS across script.js's html.fable-entry teardown
    // (~2.6s) — no dark frame can slip in between the two holds.
    fableRoot.classList.add('fable-entry-hold');
    document.documentElement.classList.add('fable-wipe');
    // Show the title NOW (initFable's closing showScreen('title') already
    // left it visible; this re-show is idempotent + re-fires the ambient
    // guards) + apply the transparency hold in the same synchronous block —
    // no paint can land between them. The reveal fires at SPLASH_HOLD_MS
    // (matching script.js's splash fade start) via the shared
    // revealTitleUnderSplash choreography: the .fable-entry-wipe sweep fades
    // in top-to-bottom (1000ms) as the F logo crossfades out (600ms) → the F
    // dissolves INTO the menu sweeping in over the desktop (no backdrop pops
    // in — the entry hold persists until the wipe has covered the screen),
    // and the theme music starts its fade-in at that same 2s mark (never
    // during the hold).
    showScreen('title');
    if (screens.title) screens.title.classList.add('fable-title-held');
    setTimeout(() => revealTitleUnderSplash(fableRoot), FABLE_SPLASH_HOLD_MS);
    return;
  }

  // ── CINEMATIC: the fog gate (rewritten 2026-08-03). ──
  //
  // One full-screen overlay. Simple sequence:
  //   t=0s    Click. Overlay mounts over the OS desktop; .fog-in fades it to
  //           fully opaque over 2s (the fog-up). Wind audio loops. Fable's
  //           title buttons are hidden now (pre-flashed) so they never show
  //           through the fog.
  //   t=2–4s  HOLD: overlay at 100% opacity.
  //   t=3s    onSwap fires (mid-hold, fully foggy) — swap the OS desktop for
  //           Fable's title here, invisibly under the solid fog.
  //   t=4–6s  UNFOG: .fog-out fades the overlay back to 0 over 2s, revealing
  //           Fable's title.
  //   t=6s    Overlay removed. The boot transition (music + ripple + buttons)
  //           fires now so its beat isn't hidden under the fog.
  //
  // The whole thing is one CSS opacity transition on one element — no rAF, no
  // canvas, no per-frame JS. `cancel()` tears it down for closeFable's
  // EXIT-mid-fog path.

  // Hide the title buttons BEFORE the swap so they never flash visible for
  // one frame when #fable shows under the fog. They stay hidden through the
  // fog + boot's REVEAL_DELAY pause.
  if (screens.title) {
    screens.title.querySelectorAll('.fable-title-btn').forEach((b) => {
      b.classList.add('fable-title-btn--hidden');
    });
  }

  // The ripple aura anchors on the NEW GAME button (the primary action), and
  // the buttons reveal radiating outward from it: New Game first, then its
  // neighbors Continue + Load together, then Exit (the outermost) last. The
  // visual order top→bottom is: Continue, New Game, Load, Exit.
  const q = (act) => screens.title ? screens.title.querySelector(`[data-act="${act}"]`) : null;
  const allButtons = screens.title
    ? Array.from(screens.title.querySelectorAll('.fable-title-btn'))
    : [];
  // Each entry is a group revealed at one stagger beat. Order: anchor →
  // neighbors → outermost.
  const revealGroups = [
    [q('new')],                            // 1st: the ripple anchor itself
    [q('continue'), q('load')],            // 2nd: its immediate neighbors
    [q('exit'), q('online')],              // 3rd: the outermost pair
  ].map((group) => group.filter(Boolean))
   .filter((group) => group.length > 0);

  // The fog overlay covers the OS desktop + fades up over 2s. At mid-hold
  // (fully foggy) the swap fires — OS desktop → Fable title, hidden under the
  // solid fog. The boot transition fires AFTER the fog so its beat isn't hidden.
  const fog = playFogIntro({
    onSwap: () => {
      // The invisible hand-off. Everything here is hidden by the solid fog.
      fableRoot.classList.add('show');
      fableRoot.setAttribute('aria-hidden', 'false');
      activateChrome();
      if (hooks.pauseAurora) hooks.pauseAurora();
      showScreen('title');
      // Dismiss the OS home grid so it isn't stranded open behind the closed
      // app (the home tile click left 'apps' open). Routed through the OS
      // closeWindow hook the same way the EXIT path keeps openWindows in sync.
      if (hooks.closeWindow) { try { hooks.closeWindow('apps'); } catch (_) {} }
    },
  });
  currentFog = fog;

  fog.done.then(() => {
    if (currentFog !== fog) return;  // a newer open / cancel superseded us
    currentFog = null;
    currentBoot = playBootTransition({
      fableRoot,
      titleScreen: screens.title,
      musicHost: fableRoot,
      rippleAnchorBtn: q('new'),
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
export async function launchFable() {
  // Entry is no longer API-gated (2026-08-13). Fable opens to its title screen
  // regardless of API state, so the fable.exe launcher (and the home tile) can
  // land on the title with no API connected. The title screen itself handles
  // the UX: its Continue / New Game / Load buttons gray out + a bold brass
  // "API NOT CONNECTED" label shows when no API is connected, and an ONLINE
  // button (between Load and Exit) opens an in-Fable API connection window.
  // Narration is still API-only (the locked override) — only the ENTRY block
  // was removed. The backend require_api_for_fable guard on fable_start stays
  // as a backstop (unreachable while the buttons are disabled). Kept `async`
  // so existing `launchFable().catch(...)` callers keep working.
  AppLifecycle.launchApp('fable');
}

// Opens the in-Fable API connection window (the Fable-styled twin of the
// WUPI-home AI panel — same backend IPCs, Fable brass/glass aesthetic). Mounted
// on document.body over the title screen. The panel manages its own close (✕ /
// Esc) and calls `onChanged` whenever the API connection or profile list
// changes AND on close — so the title re-runs _refreshTitleGate (the grayed
// game buttons light up + "API NOT CONNECTED" hides the moment an API
// connects). Idempotent singleton (guards on an existing panel).
function openOnlinePanel() {
  if (document.querySelector('.fable-online-popup')) return;  // already open
  const titleScreen = screens.title;
  const onChanged = () => {
    if (titleScreen && titleScreen._refreshTitleGate) titleScreen._refreshTitleGate();
  };
  const panel = buildOnlinePanel({ onChanged });
  document.body.appendChild(panel);
  // Force a reflow so the opacity transition runs (mounts at 0).
  void panel.offsetWidth;
  panel.classList.add('is-open');
}


// title theme music + (if the stage is active) the CSS FX particles. The
// title canvas particle RAF self-pauses on document.hidden (see
// particles.js's onVisibility), so nothing to do there. The narrator
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
    pauseFX();          // freeze CSS particle animations
  }
}

// onResume: the focus-return mirror of onPause. Unfreezes music + FX.
// Best-effort (play() can reject under autoplay policy; pauseFX/resumeFX
// are idempotent).
function resumeFable() {
  resumeThemeMusic(fableRoot);
  resumeNewGameMusic(fableRoot);
  if (stageActive) {
    resumeFX();
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
    // (2026-08-20 audit) Same kill-any-creator-turn contract as
    // exitFlowToTitle/exitLoadToTitle: closeFable is the LAST teardown every
    // exit funnels through, so no future path through it can leave a creator
    // GLM turn streaming over the OS home. Currently unreachable from a
    // live creator run (the flow's own ⌂ sinks fire first) — belt +
    // suspenders. No-op when idle (abortCreatorTurn guards !root itself).
    abortCreatorTurn(screens['creator-chat']);
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
    // Reset the New Game flow machine + chrome (mirrors exitFlowToTitle):
    // closeFable can fire mid-flow, and a surviving flowState would resume a
    // stale step on reopen.
    stopFlowAmbiance();
    if (flowChrome) {
      flowChrome.hideBack();
      flowChrome.hideHome();
    }
    flowState = freshFlowState();
  } catch (err) {
    console.error('[fable] teardown threw, continuing with OS restore', err);
  } finally {
    // OS-side restore — always runs. deactivateChrome removes
    // body.fable-active + the peek listener + peekTimer; resumeAurora
    // hands the canvas back to the OS. These must NOT be skipped.
    try { deactivateChrome(); } catch (_) {}
    if (hooks.resumeAurora) { try { hooks.resumeAurora(); } catch (_) {} }
  }
  // (2026-08-15 audit fix) Stop any in-flight turn BEFORE fable_end (the app
  // can close mid-generation; fable_stop is a safe no-op otherwise). Chained
  // so the end IPC always follows the stop.
  invoke('fable_stop').catch(() => {}).finally(() => {
    invoke('fable_end').catch(() => {});
  });
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

  // Build the screens we use. New Game (the shell) + Continue/Load
  // (worlds + saves pickers) are wired; the stage stays built
  // (teardownStage needs it on close too).
  screens.title = buildTitle({
    // NEW GAME: reveals the cinematic creator flow — background + music + the
    // ‹ / ⌂ flow-chrome buttons, then the Player pair (slide 1).
    new: () => onNewGameClicked(),
    // CONTINUE: resume the freshest New Game save (autosave-inclusive).
    // Target stashed by title._refreshTitleGate.
    continue: () => onContinueClicked(),
    // LOAD: two-level picker — worlds screen → saves screen → resumeSave.
    load: () => onLoadClicked(),
    // ONLINE: opens the in-Fable API connection window (Fable-styled twin of
    // the WUPI-home AI panel). Never disabled — it's the path to connect an
    // API so the grayed-out game buttons light back up.
    online: () => openOnlinePanel(),
    // EXIT is the ONLY real close path — routes through the lifecycle
    // manager for full teardown.
    exit: () => AppLifecycle.closeApp('fable'),
  });
  screens.stage = buildStage();
  // The New Game split host. The flow controller (fable.js) rebuilds the
  // split's tiles + wires clicks on each entry (to pass the clicked button to
  // the burn engine), so the builder's own handlers are unused no-op stubs.
  screens['newgame-split'] = buildNewGameSplit();
  // The Player Picker (LOAD PLAYER) — built once; rendered on entry from
  // flowLoadPlayer. Surfaces LOAD/EDIT/DELETE per player via its modal.
  screens['player-picker'] = buildPlayerPicker();
  // The GLM-driven Creator Chat — the reusable conversational screen for the
  // player/sim/codex/intro wizards (Phases 3-5). Built once; rendered on entry.
  screens['creator-chat'] = buildCreatorChat();
  screens.worlds = buildWorlds({ back: () => exitLoadToTitle() });
  screens.saves = buildSaves({ back: () => showScreen('worlds') });
  for (const s of Object.values(screens)) fableRoot.appendChild(s);

  // ── The persistent New Game flow ambiance ─────────────────────────
  // A single background layer (deep void + hearth glow + rising embers) that
  // lives at the #fable root for the ENTIRE New Game / Load flow. This is
  // what keeps the background consistent across screen swaps — only the
  // foreground UI swaps, the embers/glow never tear down + rebuild. Started
  // on flow entry (startFlowAmbiance), stopped on exit back to title
  // (stopFlowAmbiance). Both are module-level so the flow functions can call
  // them; the element is module-level (flowAmbiance) for the same reason.
  flowAmbiance = document.createElement('div');
  flowAmbiance.className = 'fable-flow-ambiance';
  flowAmbiance.setAttribute('aria-hidden', 'true');
  flowAmbiance.innerHTML = `
    <div class="fable-flow-ambiance-glow"></div>
    <div class="fable-flow-ambiance-embers"></div>
  `;
  fableRoot.insertBefore(flowAmbiance, fableRoot.firstChild);

  showScreen('title');

  // ── The global double-click guard (capture phase) ────────────────
  // One listener at the root, capture phase, swallows click + pointerdown
  // while a transition is in flight (flowBusy). Capture phase is load-bearing:
  // it intercepts the event BEFORE any per-button listener can run, so no
  // handler needs to know about the guard. pointerdown is covered too so a
  // fast double-tap doesn't even start a press animation on the second click.
  // The title's disabled-button guard (title.js) already short-circuits
  // disabled buttons; this is the belt-and-suspenders for the rapid-click
  // case on ENABLED buttons mid-transition.
  ['click', 'pointerdown'].forEach((evt) => {
    fableRoot.addEventListener(evt, (e) => {
      if (!flowBusy) return;
      e.stopImmediatePropagation();
      e.preventDefault();
    }, { capture: true });
  });

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
