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
//              pair → Intro pair → launchGame (see revealNewGameShell). Each
//              picker reuses the newgame-split tile language; any picker whose
//              content already exists on the card is skipped (advanceFromSim).
//   Continue → resume the freshest save (resumeSave).
//   Load     → two-level picker: worlds.js → saves.js → resume.
// The working stage + gameplay engine (stage.js, engine/*, fx/*, panels/*)
// are the destination of every flow.
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { AppLifecycle } from '../app-lifecycle.js';
import './fable.css';
import './flow-cinematic.css';

import { buildTitle } from './screens/title.js';
import { buildStage, wireStage, teardownStage, toast } from './screens/stage.js';
import { buildNewGameSplit } from './screens/newgame-split.js';
import { buildPlayerPicker, renderPlayerPicker } from './screens/player-picker.js';
import { buildCreatorChat, renderCreatorChat } from './screens/creator-chat.js';
import { createEmbers } from './screens/embers.js';
import { parseImportFile } from './screens/st-import.js';
import { playBurnTransition, playReverseSpawn } from './engine/burn-transition.js';
import { tileCaptionHTML } from './engine/tile-caption.js';
import { buildWorlds, renderWorlds } from './screens/worlds.js';
import { buildSaves, renderSaves } from './screens/saves.js';
import { parseEnvelope } from './engine/creator-engine.js';
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
// chosen SavedPlayer forward to the SIM/Codex/Intro steps; `selectedCardId`
// is the SIM card established by the SIM pair (rides to Codex/Intro/launch).
// The GLM-driven player/sim/codex/intro wizards all hang off this same state.
let flowState = {
  step: null,
  slideOneHasBack: false, // whether slide 1 (Player pair) shows ‹ (Load-menu entry only)
  selectedCardId: null,   // the sim card built by the sim wizard (rides to codex/intro)
  selectedPlayerId: null, // the chosen SavedPlayer (rides to fable_start)
  playerDraft: null,      // the player wizard's draft (context for the intro)
  simDraft: null,         // the sim wizard's draft (context for the intro)
  pendingImport: null,    // a SillyTavern import { charData, portraitDataUrl?, portraitExt? } seeded by the IMPORT tile → the Player Wizard
  pendingSimIntro: null,  // imported first_mes + alternate_greetings carried into the SIM card's <intro> (set by the IMPORT tile)
};

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
// Title-reveal choreography shared by the FABLE_ENTRY + DIRECT_LAUNCH title
// paths: drop the transparency hold FIRST (the void backdrop paints
// instantly, so the title's fade-in runs over the solid void, never over the
// desktop), then showScreen('title') + the .fable-title-enter dissolve +
// theme-music fade-in. Defined ONCE so the direct-launch fallbacks can't
// drift out of sync with the plain-entry reveal again — the 2026-08-14
// "menu spawns beside the F on .lnk launches" bug was exactly that drift:
// the fallbacks called revealHold()+showScreen('title') bare, skipping the
// hold window + dissolve entirely (the async IPC handoff resolves in tens
// of ms, so the title appeared while the F splash was still in its 2s hold).
const revealTitleUnderSplash = (fableRoot) => {
  fableRoot.classList.remove('fable-entry-hold');
  showScreen('title');
  const titleEl = screens.title;
  if (titleEl) {
    // Drop the 2s transparency hold FIRST (fable.css): the title was shown at
    // t=0 behind the F splash, held at opacity 0 while it warmed up — the
    // enter animation then rides the full 1s top-to-bottom mask sweep.
    titleEl.classList.remove('fable-title-held');
    titleEl.classList.remove('fable-title-enter');
    void titleEl.offsetWidth; // reflow so re-adding restarts the animation
    titleEl.classList.add('fable-title-enter');
    titleEl.addEventListener('animationend', () => {
      titleEl.classList.remove('fable-title-enter');
    }, { once: true });
  }
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
// is the slot to resume. Cold-resume from the title (no game running yet),
// so fable_start is the entry — NOT fable_load_save (that requires an
// already-running game).
async function resumeSave(cardId, saveId) {
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
  try { await invoke('fable_end'); } catch (_) {}

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
    console.error('[fable] fable_start (resume) failed — entering stage without engine', err);
    engineStarted = false;
  }

  enterStageViaTransition(openingScene, loadMessages);
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
//   • onNewGame → fade out → the Player pair (slide 1) with reverse-spawn, so
//     the New Game flow continues into a fresh game with a chosen player. The
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

// NEW GAME from the Load menu's per-card "NEW" action. The card argument is
// retained for the worldHandlers() call signature but is not consumed — the
// flow always starts fresh at the Player pair (the user picks/creates a
// player, then a SIM card, etc.). The New Game ambience is already playing
// (it's the same track the worlds/Load screen uses), so it is NOT restarted
// here. ‹ routes back to the worlds grid; ⌂ routes to the title (via
// exitLoadToTitle, which also tears the ambience down).
function beginNewGameFromCard(card) {
  void card;  // unused; kept for the handler signature
  withFlowBusy(() => {
    return revealNewGameShell({
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
function openCardRawEditor(card) {
  const worlds = screens.worlds;
  if (!worlds) return;
  // If one's already open, close it first (defensive against a double-open).
  const existing = worlds.querySelector('.fable-world-raw-overlay');
  if (existing) existing.remove();

  const overlay = document.createElement('div');
  overlay.className = 'fable-raw-editor-overlay fable-world-raw-overlay';
  overlay.innerHTML = `
    <div class="fable-raw-editor-backdrop" aria-hidden="true"></div>
    <div class="fable-raw-editor-modal" role="dialog" aria-modal="true">
      <div class="fable-raw-editor-head">
        <span class="fable-raw-editor-title">Edit ${escHtml(card.name)} — Sim Card (.sim)</span>
        <div class="fable-raw-editor-controls">
          <button type="button" class="fable-raw-btn save" data-raw-save title="Save (Ctrl+Enter)">✓</button>
          <button type="button" class="fable-raw-btn revert" data-raw-revert title="Revert to last saved">↻</button>
          <button type="button" class="fable-raw-btn close" data-raw-close title="Close (Esc)">✕</button>
        </div>
      </div>
      <textarea class="fable-raw-editor-text" data-raw-text spellcheck="false"></textarea>
    </div>`;
  worlds.appendChild(overlay);

  const textarea = overlay.querySelector('[data-raw-text]');
  const saveBtn = overlay.querySelector('[data-raw-save]');
  const revertBtn = overlay.querySelector('[data-raw-revert]');
  const closeBtn = overlay.querySelector('[data-raw-close]');
  let lastGood = '';
  let isValid = true;

  // Load the current XML.
  invoke('fable_card_raw_get_by_id', { cardId: card.id })
    .then((xml) => { lastGood = xml || ''; textarea.value = lastGood; validate(); })
    .catch((err) => { console.warn('[fable] card raw load failed', err); });

  // Client-side XML well-formedness sniff (mirrors raw-editor.js). Cheap
  // pre-check; the authoritative gate is the server-side parse on save.
  function sniffXmlWellFormed(s) {
    const trimmed = String(s || '').trim();
    if (!trimmed) return 'Empty card';
    if (!/^<\?xml|<sim_card[\s>]/i.test(trimmed)) return 'Missing <sim_card> root';
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
      await invoke('fable_card_raw_set_by_id', { cardId: card.id, xml: textarea.value });
      lastGood = textarea.value;
      validate();
    } catch (err) {
      // Status bar removed 2026-08-12 per Chloe — save failures log silently.
      console.warn('[fable] sim editor save failed', err);
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
// screen). `onBack`/`onHome`
// override the default ‹/⌂ routing (both default to exitFlowToTitle).
function revealNewGameShell({ startMusic = false, onBack, onHome } = {}) {
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
  stopNewGameMusic(fableRoot);
  startThemeMusic(fableRoot);
  stopFlowAmbiance();
  if (flowChrome) {
    flowChrome.hideBack();
    // Hide ⌂ home too so it doesn't linger over the title/main menu after
    // exiting the flow (the chrome overlay persists; both buttons go dark).
    flowChrome.hideHome();
  }
  flowState = { step: null, slideOneHasBack: false, selectedCardId: null, selectedPlayerId: null, playerDraft: null, simDraft: null, pendingImport: null, pendingSimIntro: null };
  showScreen('title');
}

// Exit the LOAD flow (worlds/saves pickers) back to the title (⌂ home from the
// worlds screen). Fades the new-game ambience out + restarts the title theme —
// mirrors exitFlowToTitle. Also hides the flow-chrome buttons so ‹ + ⌂ don't
// linger over the title (the chrome overlay persists; both go dark). The Load
// menu shares the New Game music + ember background, so it shares the teardown
// too. This is an instant screen swap (no transition), so no withFlowBusy wrap
// — the title buttons are immediately usable again.
function exitLoadToTitle() {
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
//   Slide 2: SIM pair (NEW / LOAD / IMPORT SIM CARD) → establish card →
//            advanceFromSim (skips Codex/Intro pickers whose content exists)
//   Slide 3: Codex pair (CREATE / CONTINUE-WITHOUT / IMPORT) — skipped if codex
//   Slide 4: Intro pair (ADD / NO INTRO) — skipped if intro; else launchGame
// A card that already has BOTH codex + intro launches the instant it's
// established. Each GLM wizard (player/sim/codex) is driven by creator-chat.js;
// the intro is a one-shot nudge collector → launchGame generates behind the fade.

// === Flow pair tiles (the shared picker language) ========================
// Every picker slide (Player / SIM / Codex / Intro) is a pair of caption slabs
// (+ an optional IMPORT mini tile) in the newgame-split host, revealed by the
// reverse-spawn + burned on click. buildFlowPairTiles generalizes the old
// Player-pair-only builders so all four pickers share one code path.

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
// carries the new player id ( + draft, for the intro's context) into the sim
// step.
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
        title: 'Player Wizard',
        onCreated: ({ playerId, draft }) => {
          flowState.selectedPlayerId = playerId;
          flowState.playerDraft = draft;
          flowSimPair();
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
    toast(`Import failed: ${e.message || e}`);
    return;
  }
  if (!result) return; // picker cancelled
  flowState.pendingImport = result;
  // Carry the imported greetings (first_mes + alternate_greetings) into the
  // SIM card's <intro> so the authored opening survives verbatim (2026-08-13).
  flowState.pendingSimIntro = result.introText || null;
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
        title: 'Player Wizard',
        presetImportData: importSeed && importSeed.charData,
        presetPortraitDataUrl: importSeed && importSeed.portraitDataUrl,
        presetPortraitExt: importSeed && importSeed.portraitExt,
        presetPortraitBytes: importSeed && importSeed.portraitBytes,
        onCreated: ({ playerId, draft }) => {
          flowState.selectedPlayerId = playerId;
          flowState.playerDraft = draft;
          flowSimPair();
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

// Edit a saved player: open the Player Wizard seeded with the loaded player
// (edit mode → straight to the review card). CREATE re-saves via
// fable_player_write (same id overwrites if the name is unchanged).
function flowEditPlayer(player) {
  showScreen('creator-chat');
  setFlowStep('creator-chat');
  renderCreatorChat(screens['creator-chat'], {
    creatorKind: 'player',
    title: 'Edit Player',
    seedDraft: player,
    onCreated: ({ playerId, draft }) => {
      flowState.selectedPlayerId = playerId;
      flowState.playerDraft = draft;
      flowSimPair();
    },
    back: () => renderPlayerPickerStep(),
  });
}

// Route after a player is chosen (Load). The SIM pair is the next step.
// (Loaded players have no draft; flowState.playerDraft stays null + the intro
// step works off the world context alone.)
function advanceAfterPlayer(playerId) {
  flowState.selectedPlayerId = playerId;
  flowSimPair();
}

// === Sim World Wizard ====================================================
// The GLM sim wizard gathers the MANDATORY world anchors (date/weather via
// <start>, the travel graph via <locations>, the opening cast via <cast>) so
// the tracker has them from turn 1. Reached from the SIM pair (NEW / IMPORT).
// `presetImport` seeds the wizard from a SillyTavern import; `presetIntro`
// carries the import's greetings into the card's <intro>. On CREATE →
// advanceFromSim (the content-aware skip matrix).
function flowCreateSim(presetImport = null, presetIntro = null) {
  showScreen('creator-chat');
  setFlowStep('creator-chat');
  // Consume any import-carried opening beat (first_mes + alternate_greetings
  // from the IMPORT tile) so it lands in the SIM card's <intro>. Cleared after
  // consumption so a later non-import run starts clean.
  const intro = presetIntro != null ? presetIntro : (flowState.pendingSimIntro || null);
  flowState.pendingSimIntro = null;
  renderCreatorChat(screens['creator-chat'], {
    creatorKind: 'sim',
    title: 'World Wizard',
    presetImportData: presetImport,
    presetIntro: intro,
    onCreated: ({ cardId, draft }) => {
      flowState.selectedCardId = cardId;
      flowState.simDraft = draft;
      advanceFromSim(cardId);
    },
    back: () => flowSimPair(),
  });
}

// === SIM / Codex / Intro pickers + content-aware skip ====================
// The New Game flow past the player step is a chain of tile pickers (each
// reusing the newgame-split tile language) — SIM pair → Codex pair → Intro
// pair — ending in launchGame. `advanceFromSim` skips any picker whose content
// already exists on the established card (a loaded world with a codex skips
// the Codex picker; one with both codex + intro launches immediately).

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
  renderWorlds(screens['worlds'], {
    pickMode: true,
    onSelect: (card) => {
      flowState.selectedCardId = card.id;
      flowState.simDraft = {
        name: card.name,
        tone: card.tone || null,
        setting: card.setting_preview || null,
      };
      advanceFromSim(card.id);
    },
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
    toast(`Import failed: ${e.message || e}`);
    return;
  }
  if (!result) return; // picker cancelled
  flowState.pendingSimIntro = result.introText || null;
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
// Best-effort: a card "has" a codex when its .codex sibling is non-empty, and
// "has" an intro when its .sim carries an <intro> root (the in-file sibling
// written by fable_card_set_intro / the SIM serializer). Both run before any
// active game exists, so they use the by-id variants.
async function detectHasCodex(cardId) {
  try {
    const r = await invoke('fable_codex_get_by_id', { cardId });
    return !!(r && r.raw && r.raw.trim());
  } catch (_) { return false; }
}
async function detectHasIntro(cardId) {
  try {
    const raw = await invoke('fable_card_raw_get_by_id', { cardId });
    return /<intro[\s>]/i.test(raw || '');
  } catch (_) { return false; }
}

// Route after a sim card is established (NEW / LOAD / IMPORT). Skips the Codex
// picker if a codex exists + the Intro picker if an intro exists — so a loaded
// world that already has both launches immediately.
async function advanceFromSim(cardId) {
  const [hasCodex, hasIntro] = await Promise.all([
    detectHasCodex(cardId),
    detectHasIntro(cardId),
  ]);
  if (hasCodex && hasIntro) {
    launchGame(cardId);
  } else if (hasCodex) {
    flowIntroPair(cardId);
  } else if (hasIntro) {
    // Intro already exists → the Codex picker is the LAST slide (no Intro
    // picker after it): on any codex choice, launch directly.
    flowCodexPair(cardId, { afterCodex: () => launchGame(cardId) });
  } else {
    flowCodexPair(cardId, { afterCodex: () => flowIntroPair(cardId) });
  }
}

// --- Codex pair: CREATE SIM CODEX / CONTINUE WITHOUT CODEX / IMPORT ------
function flowCodexPair(cardId, { afterCodex } = {}) {
  buildFlowPairTiles({
    pair: [
      { caption: 'CREATE SIM CODEX', act: 'create-codex', onClick: (b) => burnPairTile(b, () => flowCreateCodex(cardId, null, afterCodex)) },
      { caption: 'CONTINUE WITHOUT CODEX', act: 'no-codex', onClick: (b) => burnPairTile(b, () => (afterCodex ? afterCodex() : flowIntroPair(cardId))) },
    ],
    importTile: { caption: 'IMPORT', onClick: (b) => flowImportCodexPair(b, cardId, afterCodex) },
  });
  showScreen('newgame-split');
  setFlowStep('codex-pair');
  spawnFlowTiles();
}

function flowCreateCodex(cardId, presetImport, afterCodex) {
  showScreen('creator-chat');
  setFlowStep('creator-chat');
  renderCreatorChat(screens['creator-chat'], {
    creatorKind: 'codex',
    title: 'Codex Wizard',
    cardId,
    presetImportData: presetImport,
    onCreated: () => (afterCodex ? afterCodex() : flowIntroPair(cardId)),
    back: () => flowCodexPair(cardId, { afterCodex }),
  });
}

async function flowImportCodexPair(selectedBtn, cardId, afterCodex) {
  if (flowBusy) return;
  let result;
  try {
    result = await parseImportFile(screens['newgame-split']);
  } catch (e) {
    toast(`Codex import failed: ${e.message || e}`);
    return;
  }
  if (!result) return; // picker cancelled
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      flowCreateCodex(cardId, result.charData, afterCodex);
    },
  });
}

// --- Intro pair: ADD INTRO / NO INTRO  (no import) -----------------------
function flowIntroPair(cardId) {
  buildFlowPairTiles({
    pair: [
      { caption: 'ADD INTRO', act: 'add-intro', onClick: (b) => burnPairTile(b, () => flowCreateIntro(cardId)) },
      { caption: 'NO INTRO', act: 'no-intro', onClick: (b) => burnPairTile(b, () => launchGame(cardId)) },
    ],
    importTile: null,
  });
  showScreen('newgame-split');
  setFlowStep('intro-pair');
  spawnFlowTiles();
}

// ADD INTRO → the intro-nudge collector. A fixed static prompt asks what the
// intro should say/include; Enter (empty or a nudge) hands it to launchGame,
// which generates the opening beat behind the fade (codex + sim + player
// context, fitting the world <tone>, revolving around the nudge if given).
function flowCreateIntro(cardId) {
  showScreen('creator-chat');
  setFlowStep('creator-chat');
  renderCreatorChat(screens['creator-chat'], {
    creatorKind: 'intro',
    title: 'Opening Beat',
    cardId,
    introNudge: true,
    staticPrompt: "How would you like the narrator to start your story? If you're unsure just add something vague or just tap *ENTER* and I don't mind creating the intro for you.",
    onEnter: (nudge) => launchGame(cardId, nudge),
    back: () => flowIntroPair(cardId),
  });
}

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
    case 'intro-pair':          // Intro pair → Codex pair (re-offer codex)
      flowCodexPair(flowState.selectedCardId, { afterCodex: () => flowIntroPair(flowState.selectedCardId) });
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
// The terminal step for every New Game path (NO INTRO + ADD INTRO alike):
// fade the flow UI to leave only the background + music, [optionally generate
// the intro behind the fade], run fable_start (the "schema captured" wait),
// then stop the music + fade to black + enter the stage.

// Fade whatever screen is currently visible (the picker tiles or the chat
// shell) to opacity 0 + hide the flow chrome, leaving the .fable-flow-ambiance
// background + the new-game music playing while the backend works.
function fadeFlowToLoading() {
  if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
  const current = fableRoot.querySelector('.fable-screen:not([hidden])');
  if (current) current.classList.add('is-launching');
}

// Generate the opening beat behind the fade: gather codex + world + player +
// nudge into import_data, one creator_assistant_turn (intro kind, emits `ready`
// immediately), parse the envelope, write draft.intro via fable_card_set_intro.
// Empty nudge → the model crafts an intro from the codex + world tone + player;
// non-empty → revolves around the nudge. Best-effort: a failure logs + the
// launch continues (fable_start falls back to no intro).
async function generateIntroOneShot(cardId, nudge) {
  let codexEntries = [];
  try {
    const r = await invoke('fable_codex_get_by_id', { cardId });
    codexEntries = (r && r.entries) || [];
  } catch (_) { /* no codex — fine */ }
  const importData = {
    world: flowState.simDraft || null,
    player: flowState.playerDraft || null,
    codex: codexEntries,
    nudge: nudge || '',
  };
  const text = await new Promise((resolve, reject) => {
    const channel = new Channel();
    let settled = false;
    channel.onmessage = (msg) => {
      if (msg.type === 'done') {
        if (settled) return; settled = true; resolve(msg.text || '');
      } else if (msg.type === 'cancelled' || msg.type === 'api_lost') {
        if (settled) return; settled = true; reject(new Error(msg.message || msg.type));
      }
    };
    invoke('creator_assistant_turn', {
      creatorKind: 'intro',
      history: [{
        role: 'user',
        content: nudge ? `Opening beat nudge: ${nudge}` : 'Write the opening narrator beat that launches this world.',
      }],
      importData,
      onEvent: channel,
    }).catch((e) => { if (!settled) { settled = true; reject(e); } });
  });
  const env = parseEnvelope(text);
  const intro = env && env.draft ? (env.draft.intro || '').trim() : '';
  if (intro) {
    await invoke('fable_card_set_intro', { cardId, text: intro });
  }
}

// The terminal step: fade UI (background + music hold) → [intro gen if ADD]
// → fable_start (schema capture) → stop music + fade to black + stage.
// `nudge === undefined` → NO INTRO (skip generation); a string (possibly '')
// → ADD INTRO (generate the opening beat from the nudge).
async function launchGame(cardId, nudge = undefined) {
  // Guarded: a picker tile's burn-onComplete or the intro Enter could
  // double-fire. enterStageViaTransition clears the flag.
  if (flowBusy) return;
  setFlowBusy(true);
  fadeFlowToLoading();
  // ADD INTRO: generate the opening beat behind the fade, then persist it.
  if (nudge !== undefined) {
    try {
      await generateIntroOneShot(cardId, nudge);
    } catch (e) {
      console.error('[fable] intro generation failed — launching without intro', e);
    }
  }
  // fable_start: seat the card, bootstrap the schema anchors (clock/weather/
  // location), seed the tracker. The new-game music keeps playing during this
  // wait (the "background + music" hold); it stops further below.
  // NOTE: the title ambient is stopped inside enterStageViaTransition (right
  // before the stage shows), not here — so the grass doesn't vanish early.
  try { await invoke('fable_end'); } catch (_) {}
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
    console.error('[fable] fable_start (new game) failed — entering stage without engine', err);
    engineStarted = false;
  }
  // Fade-to-black + stage swap. Stop the new-game music + fade the title theme
  // HERE (after the schema-capture wait) so the stage opens in silence.
  fadeOutThemeMusic(fableRoot);
  stopNewGameMusic(fableRoot);
  enterStageViaTransition(openingScene, loadMessages);
}


// The shared "play the magical transition + swap to stage + wire it" tail.
// Used by every start/resume path.
function enterStageViaTransition(openingScene, loadMessages) {
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
  // Stage entry is the terminal step of every start/resume path — clear the
  // double-click guard here so the flag set by resumeSave / launchGame is
  // always released.
  setFlowBusy(false);
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
    // suppressed so nothing paints behind the F. Unlike FABLE_ENTRY there is
    // no fixed 2s reveal — the async handoff below drops the hold at whatever
    // point real content first appears (title fallback OR the stage entry).
    fableRoot.classList.add('fable-entry-hold');
    const revealHold = () => fableRoot.classList.remove('fable-entry-hold');
    // Title fallbacks honor the splash hold: reveal via the shared
    // choreography (revealTitleUnderSplash) DELAYED until the F splash's 2s
    // window has elapsed (0ms if the async handoff already outlasted it —
    // e.g. a slow IPC). Revealing bare made the menu spawn beside the F
    // (the handoff resolves in tens of ms; fixed 2026-08-14).
    const launchT0 = performance.now();
    const revealTitleAfterSplash = () => {
      const wait = Math.max(0, FABLE_SPLASH_HOLD_MS - (performance.now() - launchT0));
      setTimeout(() => revealTitleUnderSplash(fableRoot), wait);
    };
    (async () => {
      try {
        const ctx = await invoke('get_launch_context');
        if (!ctx || !ctx.cardSlug) { revealTitleAfterSplash(); return; }
        // API gate: fable_start refuses without a connected API. Falling back
        // to the title (instead of a dead stage) lets the player connect via
        // the ONLINE button + retry — a persisted API connection carries over.
        const src = await invoke('model_source_get').catch(() => null);
        if (!src || !src.apiReady) { revealTitleAfterSplash(); return; }
        revealHold();
        await resumeSave(ctx.cardSlug, ctx.saveId ?? null);
      } catch (e) {
        console.error('[fable] direct launch failed, falling back to title', e);
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
    // must stay off — only the F floats over the desktop. Dropped at the
    // reveal below, so the title's fade-in runs over the solid void, never
    // over the desktop.
    fableRoot.classList.add('fable-entry-hold');
    // Show the title NOW (initFable's closing showScreen('title') already
    // left it visible; this re-show is idempotent + re-fires the ambient
    // guards) + apply the transparency hold in the same synchronous block —
    // no paint can land between them. The reveal fires at SPLASH_HOLD_MS
    // (matching script.js's splash fade start) via the shared
    // revealTitleUnderSplash choreography: the held class drops, the
    // .fable-title-enter wipe fades in top-to-bottom (1000ms) as the F logo
    // crossfades out (600ms) → the F dissolves INTO the menu, and the theme
    // music starts its fade-in at that same 2s mark (never during the hold).
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
