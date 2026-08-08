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
//   New Game → card picker (screens/picker.js): pick a shipped .sim → straight
//              into the stage at a fresh game (no interview).
//   Continue → resume the freshest save (resumeSave).
//   Load     → two-level picker: worlds.js → saves.js → resume.
// The working stage + gameplay engine (stage.js, engine/*, fx/*, panels/*)
// are the destination of every flow.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { AppLifecycle } from '../app-lifecycle.js';
import './fable.css';
import './flow-cinematic.css';

import { buildTitle } from './screens/title.js';
import { buildStage, wireStage, teardownStage, toast } from './screens/stage.js';
import { buildPicker } from './screens/picker.js';
import { buildNewGameSplit } from './screens/newgame-split.js';
import { buildQuickPlayForm } from './screens/quickplay-form.js';
import {
  startQuickPlayMusic, stopQuickPlayMusic,
  pauseQuickPlayMusic, resumeQuickPlayMusic,
} from './screens/quickplay-music.js';
import { buildPlayerCreator, renderPlayerCreator } from './screens/player-creator.js';
import { buildNpcCreator, renderNpcCreator } from './screens/npc-creator.js';
import { buildWorldCreator, renderWorldCreator } from './screens/world-creator.js';
import { buildScenarioCreator, renderScenarioCreator } from './screens/scenario-creator.js';
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
import { activateChrome, deactivateChrome } from './engine/chrome.js';
import { playMagicalTransition } from './engine/transition.js';
import { mountFlowChrome } from './engine/flow-chrome.js';
import { playBurnTransition, playReverseSpawn } from './engine/burn-transition.js';
import { tileCaptionHTML } from './engine/tile-caption.js';

let fableRoot = null;       // the #fable app-window element
let screens = {};           // name → screen element
// The persistent flow-chrome controller (‹ / ⌂). Mounted once into
// fableRoot when the New Game flow begins; owns all nav for the flow
// so the screens themselves carry no header bars.
let flowChrome = null;
// The New Game / Quick Play flow state machine. Tracks where we are so ‹
// can route back correctly + the selected sim/player ids carry forward.
//   mode: 'newgame' | 'quickplay'  (which flow is active — New Game routes
//         through the Sim Creator chooser after the player step; Quick Play
//         SKIPS the sim step entirely and starts the game with the chosen
//         player attached to data/fable.sim. Added 2026-08-03.)
//   step: 'player' (slide 1) | 'sim-chooser' | 'player-creator' | 'player-picker' | 'npc-creator' | 'world-creator' | 'scenario-creator'
//   pair1Choice: vestigial (always null) — the Pair-1 sim choice was removed
//                2026-08-05; kept on the object so every reset site stays
//                shape-compatible. Loading an existing card now happens only
//                via the title LOAD menu, not inside New Game.
let flowState = { mode: null, step: null, pair1Choice: null, selectedCardId: null, selectedPlayerId: null };

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
// DEV SHORTCUT (?dev=quickplay or #dev=quickplay): a sibling of the fable
// shortcut that goes ONE step further — skips Fable's title screen AND the
// Quick Play void-form and drops you straight into the roleplay chat stage.
// openFable's DEV branch routes to devQuickPlayEnter() instead of showing the
// title. devQuickPlayEnter auto-resumes an existing Quick Play quicksave if
// one exists; otherwise it seeds a fresh world from DEFAULT_QUICKPLAY_VALUES
// (below) — so refresh lands in a real roleplay in well under a second. False
// in production (same query/hash absence story as DEV_FABLE_SHORTCUT).
const DEV_QUICKPLAY_SHORTCUT = (() => {
  try {
    if (new URLSearchParams(window.location.search).get('dev') === 'quickplay') return true;
    const h = window.location.hash.replace(/^#/, '');
    return new URLSearchParams(h).get('dev') === 'quickplay';
  } catch (_) { return false; }
})();

// The baked-in default Quick Play descriptions used by the dev shortcut's
// fresh-seed path (no quicksave exists yet). Reused verbatim each time the
// dev run starts cold. Edit these to taste — they're only read by
// devQuickPlayEnter, never by the real Quick Play void-form.
const DEFAULT_QUICKPLAY_VALUES = Object.freeze({
  player: 'A curious wanderer named Alex — quick-witted, kind-hearted, and cursed with a knack for stumbling into trouble.',
  scenario: 'The fog-shrouded port city of Mirehaven, where smugglers whisper of a cursed relic hidden somewhere in the drowned quarter beneath the old piers.',
  desire: 'Unravel the relic\'s mystery, forge uneasy alliances with the city\'s rival factions, and survive the gangs that hunt for the same prize.',
});
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
// "Create New" opens the Sim Creator chooser (NPC/WORLD/SCENARIO tiles) →
// the matching wizard (screens/npc-creator.js / world-creator.js /
// scenario-creator.js) — author a card via the slide wizard, save it, → fresh
// game from the new card. No interview, no draft.
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
  // Guarded: a rapid double-click on a save row could fire two fable_start
  // calls. The withFlowBusy wrapper drops the second; the first clears the
  // flag via enterStageViaTransition's transition completion (which is the
  // final async step below).
  if (flowBusy) return;
  setFlowBusy(true);
  // The Load-menu music (shared with New Game) stops here — the stage owns its
  // own ambience. Stopped before the IPC so the fade overlaps the load.
  stopNewGameMusic(fableRoot);
  // NOTE: the title ambient (grass/particles) is NOT stopped here — it's
  // stopped inside enterStageViaTransition's onMidpoint (when the screen is
  // black), so the grass keeps animating until the transition covers it
  // instead of vanishing instantly at click.
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
// refreshed on every title show via _refreshContinue (title.js); if it's null
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

// LOAD button handler: show the world picker + populate it. The worlds screen
// lists only worlds that have saves (a world with no saves is a New Game
// target). Selecting one routes to the saves list for that world.
// LOAD button handler: show the world picker + populate it. The worlds screen
// lists only worlds that have saves (a world with no saves is a New Game
// target). Selecting one routes to the saves list for that world.
//
// Transition (Chloe 2026-08-03): wrap the title → worlds swap in the black
// magical transition + stop the title ambient at click, matching Quick Play /
// New Game. Previously this was an instant showScreen, so the grass stayed
// animated through an abrupt swap — inconsistent with the other title buttons.
// The title theme is NOT faded here — the worlds picker returns to the title
// (via ‹), so the theme keeps playing through the picker rather than being
// cut + restarted on back.
function onLoadClicked() {
  withFlowBusy(() => {
    // Stop the title ambient (grass + particles) at click so the dim falls over
    // a static frame (matches Quick Play / New Game).
    if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
    // Fade the title theme out — the Load menu gets the SAME newgame.mp3 +
    // fire.mp3 ambience as New Game (Chloe 2026-08-05). The pair fades in at
    // the transition midpoint, mirroring onNewGameClicked.
    fadeOutThemeMusic(fableRoot);
    return playMagicalTransition({
      blackHoldMs: 1150,
      onMidpoint: () => {
        showScreen('worlds');
        renderWorlds(screens.worlds, worldHandlers());
        // Bloom the music + fire as the worlds screen reveals (same hand-off
        // feel as New Game).
        startNewGameMusic(fableRoot, { fadeIn: true });
      },
    }).catch((e) => {
      console.error('[fable] Load transition failed, jumping to worlds', e);
      showScreen('worlds');
      renderWorlds(screens.worlds, worldHandlers());
      startNewGameMusic(fableRoot, { fadeIn: true });
    });
  });
}

// The Load menu's card-modal handlers (2026-08-05). The modal surfaces four
// actions per card; these are the wired behaviors:
//   • onNewGame → fade out → Pair 2 (Create/Load Player) with reverse-spawn,
//     the card pre-selected so the New Game flow continues into a fresh game
//     with [card + chosen player]. The music keeps playing (it's the same
//     New Game ambience the flow already uses).
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

// NEW GAME from the Load menu: pre-select the card, then land on Pair 2
// (Create/Load Player) with the fade + reverse-spawn animation. The New Game
// flow's state is set up exactly as if the user had walked Pair 1 → Load Sim
// → picked this card, so the existing advanceAfterPlayer → advanceToSimStep
// machinery carries the run forward.
function beginNewGameFromCard(card) {
  withFlowBusy(() => {
    // Set up the New Game flow state: card pre-selected, pair1 = existing
    // (Load Sim), so flowBack() + the ‹ chrome route correctly from Pair 2.
    flowState = {
      mode: 'newgame',
      step: 'player',
      pair1Choice: 'existing',
      selectedCardId: card.id,
      selectedPlayerId: null,
    };
    if (!flowChrome) flowChrome = mountFlowChrome(fableRoot);
    if (flowChrome) {
      flowChrome.showBack();
      flowChrome.delayHome(2500);
      flowChrome.onHome(() => exitFlowToTitle());
      flowChrome.onBack(() => flowBack());
    }
    return playMagicalTransition({
      blackHoldMs: 600,
      onMidpoint: () => {
        // Land on Pair 2 with the two tiles reverse-spawning.
        const tiles = rebuildSplitTiles([
          { caption: 'CREATE PLAYER', act: 'create-player' },
          { caption: 'LOAD PLAYER', act: 'load-player' },
        ]);
        tiles[0].addEventListener('click', (e) => flowCreatePlayer(e.currentTarget));
        tiles[1].addEventListener('click', (e) => flowLoadPlayer(e.currentTarget));
        showScreen('newgame-split');
        tiles.forEach((t) => { t.style.opacity = '0'; });
      },
    }).then(() => {
      const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
      return playReverseSpawn(Array.from(tiles));
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
      <div class="fable-raw-editor-status" data-raw-status></div>
    </div>`;
  worlds.appendChild(overlay);

  const textarea = overlay.querySelector('[data-raw-text]');
  const status = overlay.querySelector('[data-raw-status]');
  const saveBtn = overlay.querySelector('[data-raw-save]');
  const revertBtn = overlay.querySelector('[data-raw-revert]');
  const closeBtn = overlay.querySelector('[data-raw-close]');
  let lastGood = '';
  let isValid = true;

  // Load the current XML.
  invoke('fable_card_raw_get_by_id', { cardId: card.id })
    .then((xml) => { lastGood = xml || ''; textarea.value = lastGood; validate(); })
    .catch((err) => { status.textContent = `Load failed: ${err}`; status.classList.add('bad'); });

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
    status.textContent = err ? `⚠ ${err}` : (textarea.value === lastGood ? '' : 'unsaved changes');
    status.classList.toggle('bad', !!err);
  }
  textarea.addEventListener('input', validate);

  function close() { overlay.remove(); }
  async function save() {
    if (!isValid) return;
    status.textContent = 'Saving…';
    status.classList.remove('bad');
    try {
      await invoke('fable_card_raw_set_by_id', { cardId: card.id, xml: textarea.value });
      lastGood = textarea.value;
      status.textContent = 'Saved.';
      validate();
    } catch (err) {
      status.textContent = `Save failed: ${err}`;
      status.classList.add('bad');
      // Keep the modal open so the user sees the server's validation message.
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
      else { status.textContent = 'Fix errors or ↻ revert before closing.'; status.classList.add('bad'); }
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

// === THE CINEMATIC NEW GAME FLOW (2-step, 2026-08-05 rework) ============
//
// The Pair-1 sim choice (Create Sim / Load Sim) was REMOVED 2026-08-05 — the
// player pair is now slide 1. Loading an existing card happens only via the
// title LOAD menu; New Game always authors a fresh card.
//
// Title "New Game" click
//   → [black transition; embers + music + buttons fade IN after black clears]
//   → Slide 1: [Create Player] [Load Player]   (‹ hidden, ⌂ present)
//       Create Player → burn → Player Creator → Save → Slide 2
//       Load Player   → burn → Player Picker → select → Slide 2
//   → Slide 2: Sim Creator chooser [NPC / WORLD / SCENE pyramid]
//       pick one → burn → that wizard → Save → start game (new sim + player)
//
// AUDIO (Chloe's spec, 2026-08-03): the music + fire fade-in KICKS OFF at
// the transition midpoint (screen black) so its slow 3s ramp blooms THROUGH
// the 2s undim — the pair is partly audible as the New Game scene reveals
// (earlier + more gradual than starting it cold after the undim). The
// reverse-spawn of the Pair 1 buttons still happens AFTER the undim resolves
// (visible scene first, then UI). The fade-in duration lives in
// newgame-music.js (FADE_IN_MS).
//
// The flow chrome (‹ / ⌂) is mounted once into fableRoot on entry; it
// persists across every step (never burns, never moves). Only ‹'s
// visibility toggles (hidden on Pair 1; shown from Pair 2 onward).

// Begin the New Game flow: black transition → embers + music + Pair 1.
function onNewGameClicked() {
  withFlowBusy(() => {
  // NOTE: the title ambient (grass/particles) is NOT stopped here. It's stopped
  // automatically by showScreen() when the title is hidden (fable.js:121) — and
  // the title isn't hidden until the black-midpoint swap to 'newgame-split'
  // below. Stopping it at click (the prior behavior) killed the grass instantly
  // the moment New Game was pressed, vanishing under a still-visible screen.
  // Leaving it alone keeps the grass animating through the 2s fade-out + stops
  // it exactly when the screen is fully black (Chloe 2026-08-03).
  // Theme FADES OUT at click time (Chloe 2026-08-02: "let the entire mp3
  // play out, don't cut it"). The theme ramps to silence over ~1.5s + the
  // node removes itself when silent — no hard cut. The short button SFX
  // (fableButtonSFX.mp3) plays alongside it and finishes naturally.
  fadeOutThemeMusic(fableRoot);
  // Reset the flow state + mount the chrome. Player choice is now slide 1
  // (2026-08-05: "completely remove the 'create sim' and 'load sim' from new
  // game so choosing player is the very first slide"). The Pair-1 sim choice
  // is gone — loading an existing card happens only via the title LOAD menu.
  flowState = { mode: 'newgame', step: 'player', pair1Choice: null, selectedCardId: null, selectedPlayerId: null };
  if (!flowChrome) flowChrome = mountFlowChrome(fableRoot);
  if (flowChrome) {
    flowChrome.setVariant('newgame');  // brass home glyph (default)
    flowChrome.hideBack();  // first slide — no ‹ (matches the prior Pair 1 rule)
    // ⌂ home is hidden for 2.5s on every New Game entry so it doesn't appear
    // the instant the flow mounts (Chloe: "the house icon spawns immediately
    // when pressing new game, add a 2.5s delay"). Re-entrant: re-pressing New
    // Game cancels any prior pending reveal + restarts the delay.
    flowChrome.delayHome(2500);
    flowChrome.onHome(() => exitFlowToTitle());
    flowChrome.onBack(() => flowBack());
  }
  // The split tiles ship at opacity:0 (the reverse-spawn reveals them
  // after the black clears). AUDIO + DOM split (Chloe 2026-08-03: "have
  // both the fire pit and music playing a tiny bit earlier and add more of a
  // fade in"): the music+fire fade-in KICKS OFF at the midpoint (screen
  // black) so its slow 3s ramp blooms THROUGH the 2s undim and the pair is
  // partly audible as the New Game scene reveals — earlier + more gradual
  // than starting it cold after the undim. The buttons still spawn AFTER
  // the undim resolves (visible scene first, then UI).
  return playMagicalTransition({
    blackHoldMs: 1150,
    onMidpoint: () => {
      // Swap the screen at peak darkness (invisible — content is at 0).
      // Start the music+fire fade-in HERE so it overlaps the undim reveal.
      // Build the Player pair tiles (Create Player / Load Player) fresh +
      // wire their click handlers. This is now slide 1 of the flow.
      const tiles = rebuildSplitTiles([
        { caption: 'CREATE PLAYER', act: 'create-player' },
        { caption: 'LOAD PLAYER', act: 'load-player' },
      ]);
      tiles[0].addEventListener('click', (e) => flowCreatePlayer(e.currentTarget));
      tiles[1].addEventListener('click', (e) => flowLoadPlayer(e.currentTarget));
      showScreen('newgame-split');
      // Ensure the tiles are at opacity:0 for the reverse-spawn.
      tiles.forEach((t) => { t.style.opacity = '0'; });
      // Bloom the music+fire as the screen begins to undim (earlier + the
      // longer fade-in lives in newgame-music.js FADE_IN_MS).
      startNewGameMusic(fableRoot, { fadeIn: true });
    },
  }).then(() => {
    // AFTER the black fully clears: reverse-spawn the Player pair buttons.
    // (The music fade-in was already kicked off at the midpoint above.)
    const tiles = screens['newgame-split'].querySelectorAll('.fable-flow-spawn');
    return playReverseSpawn(Array.from(tiles));
  }).catch((e) => {
    console.error('[fable] New Game transition failed, jumping to split', e);
    // Fallback: the midpoint may not have run, so ensure the music starts.
    startNewGameMusic(fableRoot, { fadeIn: true });
    showScreen('newgame-split');
  });
  }); // withFlowBusy
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
  flowState = { mode: null, step: null, pair1Choice: null, selectedCardId: null, selectedPlayerId: null };
  showScreen('title');
}

// Exit the LOAD flow (worlds/saves pickers) back to the title (‹ Back from the
// worlds screen). Fades the new-game ambience out + restarts the title theme —
// mirrors exitFlowToTitle. The Load menu shares the New Game music + ember
// background, so it shares the teardown too. (2026-08-05) This is an instant
// screen swap (no transition), so no withFlowBusy wrap — the title buttons
// are immediately usable again.
function exitLoadToTitle() {
  stopNewGameMusic(fableRoot);
  startThemeMusic(fableRoot);
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
// THE ORDER (2026-08-05 rework — Pair 1 sim choice REMOVED):
//   Slide 1: Player pair (Create Player / Load Player)
//     → click Create Player → burn → Player Creator → Save → Sim Creator
//     → click Load Player   → burn → Player Picker → select → Sim Creator
//   Slide 2: Sim Creator chooser (NPC/WORLD/SCENE pyramid) → wizard → Save
//            → start game with [new sim + player]
//   (Loading an EXISTING card now happens only via the title LOAD menu, not
//   inside New Game. New Game always authors a fresh card.)

// Rebuild the split screen's two tiles with the given labels + return
// the fresh tile elements (clone-detached so no handler stacks). Used by
// every Pair render so the burn/spawn engine always sees clean buttons.
// Each tile is a caption-only dark panel (Chloe 2026-08-03: every icon was
// removed from the New Game flow — only ‹ / ⌂ in the flow chrome survive).
// `labels[].caption` is the always-visible label text, split into stacked
// lines per the word-count rule (engine/tile-caption.js).
function rebuildSplitTiles(labels) {
  const split = screens['newgame-split'];
  const host = split.querySelector('.fable-newgame-tiles');
  host.innerHTML = labels.map((l) => `
    <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="${l.act}">
      <span class="fable-newgame-tile-caption">${tileCaptionHTML(l.caption)}</span>
    </button>
  `).join('');
  return Array.from(host.querySelectorAll('.fable-newgame-tile'));
}

// Helper: all sibling tiles except the clicked one (the burn targets).
// Walks up to the `.fable-newgame-tiles` host first so the pyramid layout
// (NPC in the column, WORLD/SCENE in a nested row) resolves ALL three tiles
// as siblings regardless of which row the clicked tile sits in. For the flat
// player-pair layout the host IS the parent, so this is a strict generalization.
function siblingTilesExcept(selectedBtn) {
  const host = selectedBtn.closest('.fable-newgame-tiles') || selectedBtn.parentElement;
  return Array.from(host.querySelectorAll('.fable-newgame-tile'))
    .filter((b) => b !== selectedBtn);
}

// Route after a player is chosen (Create or Load) in the New Game flow.
// Quick Play no longer routes through here — it dropped the player step
// entirely (2026-08-05) and seeds the run from three free-text descriptions
// instead. So this is now New-Game-only: the player is in hand, now pick/
// author the sim card, then start a fresh game with both.
//
// EXCEPTION (2026-08-05): when the run entered via the Load menu's NEW GAME
// action, the card is ALREADY selected (`flowState.selectedCardId` is set by
// beginNewGameFromCard). In that case skip the sim step entirely and start
// the game with [pre-selected card + chosen player].
function advanceAfterPlayer(playerId) {
  flowState.selectedPlayerId = playerId;
  if (flowState.selectedCardId) {
    const cardId = flowState.selectedCardId;
    flowState.selectedCardId = null;   // one-shot: consume the pre-selection
    startFreshGame(cardId, playerId);
    return;
  }
  advanceToSimStep();
}

// Pair 2 → Create Player: clicked pops+fades, the other burns, → Player Creator.
function flowCreatePlayer(selectedBtn) {
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      showScreen('player-creator');
      flowState.step = 'player-creator';
      renderPlayerCreator(screens['player-creator'], {
        onSave: (playerId) => advanceAfterPlayer(playerId),
      });
    },
  });
}

// Pair 2 → Load Player: clicked pops+fades, the other burns, → Player Picker.
function flowLoadPlayer(selectedBtn) {
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      showScreen('player-picker');
      flowState.step = 'player-picker';
      // The picker now surfaces three actions per player (LOAD/EDIT/DELETE)
      // via its expand-to-center modal (2026-08-04 overhaul). onSelect = LOAD
      // (the existing handoff); onEdit pushes the loaded JSON into the Player
      // Creator's slide wizard seeded with editFrom.
      renderPlayerPicker(screens['player-picker'], {
        onSelect: (player) => advanceAfterPlayer(player.id),
        onEdit: (player) => {
          showScreen('player-creator');
          flowState.step = 'player-creator';
          renderPlayerCreator(screens['player-creator'], {
            onSave: (playerId) => advanceAfterPlayer(playerId),
            editFrom: player,
          });
        },
      });
    },
  });
}

// Advance from a completed Pair 2 (player chosen) to the Sim step.
// The player chose Create-Sim or Load-Sim back at Pair 1 — route to the
// matching Sim screen now, carrying the selected player id forward.
//
// CREATE path (2026-08-05): the old single 3-gate creator (creator.js) is
// replaced by a Sim Creator CHOOSER — three tiles (NPC / WORLD / SCENARIO)
// on the split screen. Each tile burns into its matching wizard
// (npc-creator / world-creator / scenario-creator). All three author a sim
// card (distinguished only by which fields the wizard collects); on save the
// game starts with [new sim + player]. Reuses the burn/whoosh engine + the
// rebuildSplitTiles pattern (no new transition code).
function advanceToSimStep() {
  // → Sim Creator chooser: a PYRAMID of three tiles (NPC on top, WORLD +
  // SCENE on the bottom row). Each tile burns into its matching wizard.
  // The pyramid lives inside the same `.fable-newgame-tiles` host but is a
  // distinct `.fable-sim-chooser-pyramid` container so its CSS (column +
  // generous gap) doesn't collide with the flat player-pair row layout. The
  // tiles themselves reuse `.fable-newgame-tile` (same giant-square sizing
  // as the player pair) so the burn/reverse-spawn engine + the star hover
  // glow work unchanged.
  //
  // (2026-08-05 rework: the Pair-1 sim choice is gone — New Game always
  // authors a fresh card. Loading an existing card happens only via the
  // title LOAD menu. So this is no longer conditional on pair1Choice.)
  const split = screens['newgame-split'];
  const host = split.querySelector('.fable-newgame-tiles');
  host.innerHTML = `
    <div class="fable-sim-chooser-pyramid">
      <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="create-npc">
        <span class="fable-newgame-tile-caption">${tileCaptionHTML('NPC')}</span>
      </button>
      <div class="fable-sim-chooser-row">
        <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="create-world">
          <span class="fable-newgame-tile-caption">${tileCaptionHTML('WORLD')}</span>
        </button>
        <button class="fable-newgame-tile fable-flow-spawn" type="button" data-act="create-scenario">
          <span class="fable-newgame-tile-caption">${tileCaptionHTML('SCENE')}</span>
        </button>
      </div>
    </div>`;
  const tiles = Array.from(host.querySelectorAll('.fable-newgame-tile'));
  tiles.forEach((t) => {
    const act = t.dataset.act;
    t.addEventListener('click', (e) => flowSimCreatePick(e.currentTarget, act));
  });
  flowState.step = 'sim-chooser';
  showScreen('newgame-split');
  tiles.forEach((t) => { t.style.opacity = '0'; });
  playReverseSpawn(tiles);
}

// A Sim Creator chooser tile was clicked. Burn the other two + route to the
// matching wizard. `act` is 'create-npc' | 'create-world' | 'create-scenario'.
function flowSimCreatePick(selectedBtn, act) {
  if (flowBusy) return;
  setFlowBusy(true);
  const rejected = siblingTilesExcept(selectedBtn);
  playBurnTransition({
    selectedBtn,
    rejectedBtns: rejected,
    onComplete: () => {
      setFlowBusy(false);
      const onSave = (cardId) => startFreshGame(cardId, flowState.selectedPlayerId);
      if (act === 'create-npc') {
        showScreen('npc-creator');
        flowState.step = 'npc-creator';
        renderNpcCreator(screens['npc-creator'], { onSave });
      } else if (act === 'create-world') {
        showScreen('world-creator');
        flowState.step = 'world-creator';
        renderWorldCreator(screens['world-creator'], { onSave });
      } else if (act === 'create-scenario') {
        showScreen('scenario-creator');
        flowState.step = 'scenario-creator';
        renderScenarioCreator(screens['scenario-creator'], { onSave });
      }
    },
  });
}

// ‹ (back) routing: depends on the current step + flow mode.
function flowBack() {
  // Quick Play's only screen is the void-form, which has ‹ hidden (it's the
  // first slide). This branch is only reached via an edge case — bail to the
  // title rather than stranding the user.
  if (flowState.mode === 'quickplay') {
    exitQuickPlayToTitle();
    return;
  }
  switch (flowState.step) {
    case 'player':            // Player pair (slide 1). ‹ is only visible here
                             // when entered via the Load menu's NEW GAME (a
                             // pre-selected card is in hand) — go back to the
                             // worlds grid. From the normal New Game entry ‹ is
                             // hidden on slide 1, so this case is unreachable
                             // there.
      if (flowState.selectedCardId) {
        returnToWorldsFromFlow();
      } else {
        exitFlowToTitle();
      }
      break;
    case 'player-creator':    // Player Creator → Player pair (slide 1)
    case 'player-picker':
      returnToPlayerPair();
      break;
    case 'sim-chooser':       // Sim Creator chooser → Player pair (slide 1)
      returnToPlayerPair();
      break;
    case 'npc-creator':       // a Sim wizard → back to the Sim Creator chooser
    case 'world-creator':
    case 'scenario-creator':
      returnToSimChooser();
      break;
    default:
      exitFlowToTitle();
  }
}

// Return to the Sim Creator chooser (NPC/WORLD/SCENARIO) from one of the
// three wizards. Re-renders the three tiles + reverse-spawns them.
function returnToSimChooser() {
  advanceToSimStep();
}

// Return from the New Game flow's Pair 2 back to the Load menu's worlds grid
// (the Load-menu NEW GAME entry path, 2026-08-05). Clears the pre-selected
// card + the flow chrome, shows the worlds screen, re-renders its grid. The
// Load-menu music keeps playing (it's the same New Game ambience).
function returnToWorldsFromFlow() {
  flowState.selectedCardId = null;
  flowState.step = null;
  if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
  showScreen('worlds');
  renderWorlds(screens.worlds, worldHandlers());
}

// Return to the Player pair (slide 1: Create Player / Load Player) — rebuild
// the tiles + reverse-spawn them. Replaces the old returnToPair1/2 split
// (2026-08-05 rework: the sim Pair 1 is gone, so the player pair IS slide 1).
// ‹ is HIDDEN here (slide 1 has no prior step in the New Game flow); ⌂ home
// remains the only exit. If the run entered via the Load menu's NEW GAME
// (selectedCardId set), ‹ from the player-pair goes back to the worlds grid
// via returnToWorldsFromFlow — but that path keeps ‹ visible, so it doesn't
// route through here.
function returnToPlayerPair() {
  const tiles = rebuildSplitTiles([
    { caption: 'CREATE PLAYER', act: 'create-player' },
    { caption: 'LOAD PLAYER', act: 'load-player' },
  ]);
  tiles[0].addEventListener('click', (e) => flowCreatePlayer(e.currentTarget));
  tiles[1].addEventListener('click', (e) => flowLoadPlayer(e.currentTarget));
  showScreen('newgame-split');
  flowState.step = 'player';
  if (flowChrome) flowChrome.hideBack();   // slide 1 — no ‹
  tiles.forEach((t) => { t.style.opacity = '0'; });
  playReverseSpawn(tiles);
}

// Start a fresh game from a card (+ optional saved player): stop the
// title ambient + music, end any prior engine session, call fable_start
// with fresh:true (+ player_id), then enter the stage. The player_id
// attaches the saved player's identity onto the new game.
async function startFreshGame(cardId, playerId = null) {
  // Guarded: the final wizard slide's CREATE or the picker's select could
  // double-fire. enterStageViaTransition's .finally clears the flag.
  if (flowBusy) return;
  setFlowBusy(true);
  // NOTE: the title ambient is stopped inside enterStageViaTransition's
  // onMidpoint (screen black), not here — so the grass doesn't vanish
  // instantly at click.
  fadeOutThemeMusic(fableRoot);
  stopNewGameMusic(fableRoot);  // entering the stage — end the New Game ambience
  try { await invoke('fable_end'); } catch (_) {}

  let openingScene = null;
  let loadMessages = null;
  try {
    const result = await invoke('fable_start', { cardId, fresh: true, playerId });
    engineStarted = true;
    if (result && result.intro) openingScene = result.intro;
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
// A "throw me in" path. Runs the placeless Narrative Simulator
// (data/fable.sim) under a fixed __quickplay__ card id with ONE quicksave
// slot, auto-written on fable_end (Home/Exit). Invisible to New Game / Load /
// Continue. Player creation lives ONLY in New Game now — Quick Play drops the
// player step entirely and seeds the run from three free-text descriptions
// instead (Chloe 2026-08-05).
//
// VOID-FORM SCREEN (2026-08-05): Quick Play is now a FULL SCREEN ominous
// void-form — a near-black void with slow dark void-particles drifting in
// the background, three sleek dark charcoal-gray text fields stacked
// vertically (DESCRIBE YOUR PLAYER / DESCRIBE THE SCENARIO / DESCRIBE WHAT
// YOU DESIRE), and a large black-and-white CREATE button at the bottom that
// emits subtle dark particles + a dark glow. QuickPlay.mp3 plays at 0.3.
// CREATE stays disabled until ALL THREE fields have words; on enable it
// turns white + becomes clickable.
//
// RESUME: if a quicksave exists when the user enters Quick Play, a centered
// popup asks "start fresh (deletes the old save) or resume last?". Fresh →
// the form (interactive); resume → resumeQuickPlay(). No quicksave → straight
// to the interactive form.
//
// DRIFT: on CREATE the form fades out (everything except the void + particles
// → opacity 0), leaving the user adrift in the pure void with the particles
// still drifting while the backend seeds the world from the three
// descriptions (fable_quick_play_seed). A cinematic minimum hold keeps the
// drift on screen long enough to feel intentional even if the seed resolves
// fast; once it resolves, the chat stage loads (no opening beat — cold open).

// QUICK PLAY button handler. Runs the black transition → swaps to the
// Quick Play void-form → blooms the QuickPlay.mp3 bed on reveal. If a
// quicksave exists, a centered popup offers Start-New (deletes the old
// save) vs Resume-Last; otherwise the form is immediately interactive.
function onQuickPlayClicked() {
  withFlowBusy(() => {
  // NOTE: the title ambient (grass/particles) is NOT stopped here — it's
  // stopped automatically by showScreen() when the title hides, and the
  // title isn't hidden until the black-midpoint swap below. Leaving it
  // alone keeps the grass animating through the 2s fade-out.
  // Theme FADES OUT at click time — same hand-off as New Game.
  fadeOutThemeMusic(fableRoot);
  // Reset the flow state to Quick Play mode.
  flowState = { mode: 'quickplay', step: 'quickplay-form', pair1Choice: null, selectedCardId: null, selectedPlayerId: null };
  // Mount the flow chrome (‹ hidden on this first slide; ⌂ home delayed so
  // it doesn't appear on entry — mirrors New Game). onHome → back to title.
  if (!flowChrome) flowChrome = mountFlowChrome(fableRoot);
  if (flowChrome) {
    flowChrome.setVariant('quickplay');  // white home glyph over the void
    flowChrome.hideBack();        // first slide — no ‹
    flowChrome.delayHome(2500);   // ⌂ appears after 2.5s (matches New Game)
    flowChrome.onHome(() => exitQuickPlayToTitle());
    flowChrome.onBack(() => flowBack());  // ‹ routes via the shared flowBack
  }
  return playMagicalTransition({
    blackHoldMs: 1150,
    onMidpoint: () => {
      // Swap to the Quick Play void-form at peak darkness (invisible —
      // content ships at opacity:0; the form's own fade-in handles the
      // reveal after the black clears).
      showScreen('quickplay-form');
      // Start the QuickPlay.mp3 fade-in HERE so it overlaps the undim
      // reveal (same timing fix as New Game).
      startQuickPlayMusic(fableRoot, { fadeIn: true });
    },
  }).then(async () => {
    // AFTER the black fully clears: authoritatively check whether a quicksave
    // exists RIGHT NOW (the title's stashed _quickPlaySave can be stale if
    // its refresh IPC hasn't resolved). If one exists, show the Start-New vs
    // Resume-Last popup over the form; otherwise the form is already
    // interactive. The form's own fade-in runs regardless (handled by CSS on
    // the .fable-qp-stack once shown).
    let has = false;
    try {
      const save = await invoke('fable_quick_play_status');
      has = !!save;
    } catch (e) {
      console.error('[fable] quicksave status check failed, assuming none', e);
      has = false;
    }
    // Mirror the fresh result into the title stash so a later ‹/⌂ return
    // sees consistent state.
    if (screens.title) screens.title._quickPlaySave = has ? true : null;
    if (has) showQuickPlayResumePopup();
  }).catch((e) => {
    console.error('[fable] Quick Play transition failed, jumping to form', e);
    showScreen('quickplay-form');
    startQuickPlayMusic(fableRoot, { fadeIn: true });
  });
  }); // withFlowBusy
}

// The Start-New vs Resume-Last popup (shown over the void-form when a
// quicksave exists). Centered glass modal: a warning that starting fresh
// deletes the old save, plus START NEW / RESUME LAST / cancel (cancel =
// close popup, form stays interactive). START NEW just closes the popup —
// the form is the fresh-run path. RESUME LAST → resumeQuickPlay().
function showQuickPlayResumePopup() {
  const form = screens['quickplay-form'];
  if (!form || form.querySelector('.fable-qp-popup')) return;  // already shown
  const popup = document.createElement('div');
  popup.className = 'fable-qp-popup';
  popup.setAttribute('role', 'dialog');
  popup.setAttribute('aria-modal', 'true');
  popup.innerHTML = `
    <div class="fable-qp-popup-card">
      <p class="fable-qp-popup-title">A previous drift lingers here.</p>
      <p class="fable-qp-popup-warn">Starting fresh deletes the old save. Resume it, or let it go?</p>
      <div class="fable-qp-popup-actions">
        <button class="fable-qp-popup-btn" type="button" data-act="new">START NEW</button>
        <button class="fable-qp-popup-btn" type="button" data-act="resume">RESUME LAST</button>
        <button class="fable-qp-popup-btn fable-qp-popup-cancel" type="button" data-act="cancel">cancel</button>
      </div>
    </div>
  `;
  form.appendChild(popup);
  // Force a reflow so the CSS opacity transition runs (popup mounts at 0).
  void popup.offsetWidth;
  popup.classList.add('is-open');
  const close = () => {
    popup.classList.remove('is-open');
    setTimeout(() => { if (popup.parentNode) popup.remove(); }, 260);
  };
  popup.querySelector('[data-act="new"]').addEventListener('click', () => close());
  popup.querySelector('[data-act="resume"]').addEventListener('click', () => {
    close();
    resumeQuickPlay();
  });
  popup.querySelector('[data-act="cancel"]').addEventListener('click', () => close());
}

// Exit the Quick Play void-form back to the title (⌂ click). Fades the
// QuickPlay track out + restarts the title theme (mirrors exitFlowToTitle).
function exitQuickPlayToTitle() {
  stopQuickPlayMusic(fableRoot);
  startThemeMusic(fableRoot);
  if (flowChrome) {
    flowChrome.hideBack();
    flowChrome.hideHome();
  }
  // Reset the form so a re-entry doesn't show stale text / a leftover popup.
  const form = screens['quickplay-form'];
  if (form && form._reset) form._reset();
  const popup = form && form.querySelector('.fable-qp-popup');
  if (popup) popup.remove();
  flowState = { mode: null, step: null, pair1Choice: null, selectedCardId: null, selectedPlayerId: null };
  showScreen('title');
}

// === DEV SHORTCUT (?dev=quickplay) — straight-to-stage entry =============
//
// devQuickPlayEnter is the openFable hand-off for the ?dev=quickplay
// shortcut. It skips the title + the Quick Play void-form entirely and lands
// in the roleplay chat stage. Auto-resume if a quicksave exists; otherwise
// seed a fresh world from DEFAULT_QUICKPLAY_VALUES.
//
// The model race: the schema-engine seed path (fable_quick_play_seed) needs
// shared_model() loaded, which returns Err if the load hasn't finished yet.
// The dev boot path fires boot_load_model fire-and-forget, so on a cold
// refresh the model may still be loading when we get here. We therefore wait
// for the model-status:ready event before the fresh-seed branch (resume is
// unaffected — it only loads saved JSON, no model pass). The wait has a
// generous timeout that proceeds best-effort so a missing event can't strand
// the UI; the seed itself is already best-effort on the Rust side.
async function devQuickPlayEnter() {
  // Resume-vs-fresh: the authoritative quicksave-status check. If a quicksave
  // exists, resume it — the bundled card + session + schema load straight
  // into the stage (no model pass needed at entry). This is the path that
  // makes "refresh continues the roleplay" work.
  let hasSave = false;
  try {
    const save = await invoke('fable_quick_play_status');
    hasSave = !!save;
  } catch (e) {
    console.error('[fable] dev-quickplay: quicksave status check failed, assuming none', e);
    hasSave = false;
  }
  isQuickPlaySession = true;

  if (hasSave) {
    // ── DEV: skip the magical transition. resumeQuickPlay() normally ends
    //    with enterStageViaTransition (the ~4s dim/undim cinematic). For the
    //    straight-to-stage dev shortcut we inline its resume IPC + jump
    //    straight to the stage via enterStageDirect (no transition overlay,
    //    no title-ambient stop dance — no title was shown).
    setFlowBusy(true); // enterStageDirect's .finally clears it
    fadeOutThemeMusic(fableRoot);
    stopQuickPlayMusic(fableRoot);
    try { await invoke('fable_end'); } catch (_) {}
    let loadMessages = null;
    try {
      const result = await invoke('fable_quick_play_resume');
      engineStarted = true;
      if (result && Array.isArray(result.messages) && result.messages.length) {
        loadMessages = result.messages;
      }
    } catch (err) {
      console.error('[fable] dev-quickplay: fable_quick_play_resume failed — entering stage without engine', err);
      engineStarted = false;
      try { toast('Could not resume Quick Play — the quicksave may be corrupt.'); } catch (_) {}
    }
    enterStageDirect(null, loadMessages);
    return;
  }

  // Fresh seed — NO void drift. The normal path (beginVoidDrift) fades the
  // quickplay-form out + holds a 1.5s cinematic drift over the void while the
  // seed runs, then plays the magical transition into the stage. For the dev
  // straight-to-stage shortcut we skip ALL of that: wait for the model, run
  // the seed IPC directly, then jump straight to the stage. The screen is
  // already black (openFable hid every Fable screen for the wait), so the
  // hand-off is a seamless black → stage.
  setFlowBusy(true); // enterStageDirect's .finally clears it
  try {
    await waitForModelReady();
  } catch (e) {
    console.warn('[fable] dev-quickplay: model-ready wait failed, seeding best-effort', e);
  }
  fadeOutThemeMusic(fableRoot);
  stopQuickPlayMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let loadMessages = null;
  try {
    const result = await invoke('fable_quick_play_seed', {
      playerDesc: DEFAULT_QUICKPLAY_VALUES.player,
      scenarioDesc: DEFAULT_QUICKPLAY_VALUES.scenario,
      desireDesc: DEFAULT_QUICKPLAY_VALUES.desire,
    });
    engineStarted = true;
    if (result && Array.isArray(result.messages) && result.messages.length) {
      loadMessages = result.messages;
    }
  } catch (err) {
    console.error('[fable] dev-quickplay: fable_quick_play_seed failed — entering stage without seeded world', err);
    engineStarted = false;
    try { toast('Quick Play narrator card missing or malformed (data/fable.sim).'); } catch (_) {}
  }
  enterStageDirect(null, loadMessages);
}

// Wait for the chat 12B to report ready via Rust's model-status event. The
// fresh-seed path needs shared_model() loaded (the schema engine returns Err
// otherwise). On a cold dev refresh boot_load_model is fired fire-and-forget
// during the OS boot-skip, so the model is almost always still loading when
// we reach here — the ready event reliably fires after we subscribe. The 60s
// timeout covers the unlikely warm-cache race (load completes before we
// subscribe → no event arrives) by proceeding best-effort; the Rust seed path
// is best-effort anyway.
function waitForModelReady() {
  return new Promise((resolve) => {
    let done = false;
    const finish = () => { if (!done) { done = true; resolve(); } };
    // Safety timeout: don't strand the UI if the ready event never arrives.
    const timer = setTimeout(() => {
      finish();
    }, 60000);
    listen('model-status', (e) => {
      if (e?.payload?.status === 'ready') {
        clearTimeout(timer);
        finish();
      }
    }).catch(() => finish());
  });
}

// Quick Play → CREATE clicked (form valid). Fades the form out (everything
// except the void + particles → opacity 0), then runs the seed: the backend
// seeds the fresh empty world from the three descriptions
// (fable_quick_play_seed) while the user drifts in the pure void with the
// particles still drifting. A cinematic minimum hold keeps the drift on
// screen long enough to feel intentional even if the seed resolves fast.
// No opening beat — the chat opens cold.
async function beginVoidDrift(values) {
  // Guarded: a double-click on CREATE could fire two seeds. enterStageViaTransition's
  // .finally clears the flag at the end of the drift.
  if (flowBusy) return;
  setFlowBusy(true);
  const form = screens['quickplay-form'];
  // Fade the form (labels + fields + CREATE) to 0 — the void + particles
  // stay (the particle host is a sibling of the fading stack).
  const fadePromise = (form && form._fadeFormOut) ? form._fadeFormOut() : Promise.resolve();
  // Cinematic minimum: the drift should never feel like it blinked, even if
  // the seed IPC resolves in 200ms. Race the fade against a 1.5s floor.
  const minHold = new Promise((r) => setTimeout(r, 1500));

  // The seed + stage-entry prep. Runs in parallel with the fade + min-hold.
  // Stop the QuickPlay bed as the drift resolves (mirrors New Game stopping
  // newgame-music on stage entry). End any prior session first.
  fadeOutThemeMusic(fableRoot);
  stopQuickPlayMusic(fableRoot);
  try { await invoke('fable_end'); } catch (_) {}

  let loadMessages = null;
  const seedPromise = (async () => {
    try {
      const result = await invoke('fable_quick_play_seed', {
        playerDesc: values.player,
        scenarioDesc: values.scenario,
        desireDesc: values.desire,
      });
      engineStarted = true;
      isQuickPlaySession = true;
      // No opening beat — the chat opens cold. loadMessages is empty for a
      // fresh seed.
      if (result && Array.isArray(result.messages) && result.messages.length) {
        loadMessages = result.messages;
      }
    } catch (err) {
      console.error('[fable] fable_quick_play_seed failed — entering stage without seeded world', err);
      engineStarted = false;
      isQuickPlaySession = true;
      try { toast('Quick Play narrator card missing or malformed (data/fable.sim).'); } catch (_) {}
    }
  })();

  // Hold the drift until BOTH the form has faded AND the cinematic minimum
  // has elapsed. The seed itself doesn't gate the drift — it's awaited next
  // (the drift covers perceived latency, but a fast seed shouldn't rush the
  // cinematic; a slow seed shouldn't extend the drift past what feels right
  // — both are bounded independently).
  await Promise.all([fadePromise, minHold]);
  // Now wait for the seed to finish before loading the stage (the stage
  // reads the seeded schema on its first narrator turn).
  await seedPromise;

  enterStageViaTransition(null, loadMessages);
}

// Resume the last Quick Play quicksave: calls fable_quick_play_resume (loads
// the bundled card + session/schema from the single quicksave slot). Reached
// from the void-form's resume popup (shown only when a quicksave exists).
async function resumeQuickPlay() {
  // Guarded: double-click on RESUME LAST. enterStageViaTransition's .finally clears.
  if (flowBusy) return;
  setFlowBusy(true);
  // NOTE: the title ambient is stopped inside enterStageViaTransition's
  // onMidpoint (screen black), not here — so the grass doesn't vanish
  // instantly at click.
  fadeOutThemeMusic(fableRoot);
  stopQuickPlayMusic(fableRoot);
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
      // The screen is now fully black — safe to tear down the title ambient
      // (grass/particles) HERE, hidden, rather than at click time. Stopping
      // it at the midpoint (not before) means the grass keeps animating up
      // until the moment the black covers it, so it never vanishes abruptly
      // while the title is still visible. (Chloe 2026-08-03 fix — the prior
      // start/resume paths stopped it at the top, before the IPC await, so the
      // grass died instantly on click then a beat passed before the fade.)
      if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
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
  }).finally(() => {
    // Stage-entry transitions are the terminal step of every start/resume
    // path — clear the double-click guard here so the flag set by resumeSave
    // / startFreshGame / the Quick Play paths is always released.
    setFlowBusy(false);
  });
}

// === DEV SHORTCUT (?dev=quickplay) — cinema-free stage entry ================
//
// enterStageDirect is the straight-to-stage hand-off for the dev shortcut. It
// mirrors enterStageViaTransition's onMidpoint body EXACTLY (stop title
// ambient → showScreen('stage') → wireStage → engine-unavailable toast) but
// WITHOUT the ~4s magical dim/undim transition overlay, and WITHOUT the
// .finally flowBusy clear being conditional on the transition resolving. Used
// only by devQuickPlayEnter's resume + fresh-seed branches so the dev refresh
// lands in the roleplay stage instantly — no title, no void-form, no drift,
// no cinematic. The wait for the model/seed still happens (it must — the
// stage's first narrator turn needs the seeded schema), but it happens over a
// seamless black, not a staged screen.
function enterStageDirect(openingScene, loadMessages) {
  if (flowChrome) { flowChrome.hideBack(); flowChrome.hideHome(); }
  try {
    // No title was shown for the dev shortcut, but stop its ambient anyway in
    // case a re-entry left it running (idempotent + safe if it never started).
    if (screens.title && screens.title._stopAmbient) screens.title._stopAmbient();
  } catch (_) {}
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
    console.error('[fable] dev-quickplay: wireStage threw (stage shown, some features may degrade)', e);
  }
  if (!engineStarted) {
    try { toast('Simulation engine unavailable — chat will not respond.'); } catch (_) {}
  }
  // Terminal step of the dev shortcut's entry paths — release the guard.
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

  // ── DEV SHORTCUT (?dev=fable / ?dev=quickplay): skip the fog gate + boot. ──
  // Both shortcuts show #fable + chrome + pause the OS aurora directly (the
  // cinematic path defers this to the fog hold — here we skip straight there),
  // then return BEFORE the fog/boot setup. No fog node, no ripple aura, no
  // boot timers → nothing for closeFable to cancel; currentFog + currentBoot
  // stay null.
  //
  // They diverge on WHICH screen to land on:
  //   ?dev=fable     → show the title immediately (buttons visible + theme
  //                    music started). Ready to click in <1s. Mirrors boot.js's
  //                    prefers-reduced-motion fast path.
  //   ?dev=quickplay → skip the title too; hand off to devQuickPlayEnter,
  //                    which auto-resumes a quicksave or seeds a fresh world
  //                    from DEFAULT_QUICKPLAY_VALUES and drops straight into
  //                    the roleplay stage. No theme music (we're skipping the
  //                    title entirely).
  if (DEV_FABLE_SHORTCUT || DEV_QUICKPLAY_SHORTCUT) {
    fableRoot.classList.add('show');
    fableRoot.setAttribute('aria-hidden', 'false');
    activateChrome();
    if (hooks.pauseAurora) hooks.pauseAurora();
    if (DEV_QUICKPLAY_SHORTCUT) {
      // ── Straight-to-stage (Chloe 2026-08-05): skip title AND the void. ──
      // initFable() ends with showScreen('title'); devQuickPlayEnter() below is
      // async (IPC + model-ready wait) and only swaps to the stage AFTER it
      // resolves. During that gap the active screen is visible — and we don't
      // want the title OR the quickplay void to show. So hide every Fable screen
      // now: #fable's own --fable-void background (on .app-window.fable-app) is
      // a clean near-black, and the stage screen is pure black (#000), so the
      // wait reads as a seamless black with no title flash + no void-form.
      // devQuickPlayEnter swaps straight to the stage (no magical transition)
      // the instant the seed/resume is ready.
      for (const s of Object.values(screens)) s.hidden = true;
      stageActive = false; // no screen owns the stage yet
      // devQuickPlayEnter owns its own flowBusy guard + the (cinema-free)
      // stage-entry sequence; fire-and-forget here (errors are logged inside).
      try { devQuickPlayEnter(); } catch (e) {
        console.error('[fable] dev-quickplay enter failed', e);
      }
    } else {
      showScreen('title');
      try { startThemeMusic(fableRoot); } catch (_) {}
    }
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
export async function launchFable() {
  // Architectural override (2026-08-07): Fable narration is API-only — the
  // local 12B never narrates. Block opening Fable without an active API
  // connection and show a glass popup instead of triggering the fog gate /
  // title. The backend `fable_*` IPCs enforce the same rule (require_api_
  // for_fable), so this is the primary UX gate + the backend is the backstop.
  let extra;
  try {
    extra = await invoke('model_source_get');
  } catch (e) {
    console.warn('[Fable] model_source_get failed; blocking launch', e);
  }
  const apiReady = !!(extra && extra.source === 'api' && extra.apiReady);
  if (!apiReady) {
    showFableApiRequiredPopup();
    return;
  }
  AppLifecycle.launchApp('fable');
}

// API-required launch gate popup (2026-08-07). Centered glass modal mirroring
// the Quick Play resume popup (`.fable-qp-popup`), but mounts on document.body
// because Fable isn't open yet — the home grid is still the active surface.
// Idempotent singleton (guards on an existing popup). Dismiss = OK button only.
function showFableApiRequiredPopup() {
  if (document.querySelector('.fable-api-required-popup')) return;  // already shown
  const popup = document.createElement('div');
  popup.className = 'fable-api-required-popup';
  popup.setAttribute('role', 'dialog');
  popup.setAttribute('aria-modal', 'true');
  popup.innerHTML = `
    <div class="fable-qp-popup-card">
      <p class="fable-qp-popup-title">No API Connection</p>
      <p class="fable-qp-popup-warn">Fable narration requires an active API connection. The local model handles tracking only — it cannot narrate. Connect an API provider in Settings (the paw-menu AI panel), then try again.</p>
      <div class="fable-qp-popup-actions">
        <button class="fable-qp-popup-btn" type="button" data-act="ok">OK</button>
      </div>
    </div>
  `;
  document.body.appendChild(popup);
  // Force a reflow so the opacity transition runs (mounts at 0).
  void popup.offsetWidth;
  popup.classList.add('is-open');
  const close = () => {
    popup.classList.remove('is-open');
    setTimeout(() => { if (popup.parentNode) popup.remove(); }, 260);
  };
  popup.querySelector('[data-act="ok"]').addEventListener('click', close);
  // Esc also dismisses.
  popup.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
  // Focus the OK button for keyboard users.
  setTimeout(() => popup.querySelector('[data-act="ok"]').focus(), 50);
}

// onPause: the resource-freeze layer (alt-tab / focus-loss). Freezes the
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
  // Quick Play track: same pause-in-place treatment.
  pauseQuickPlayMusic(fableRoot);
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
  resumeQuickPlayMusic(fableRoot);
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
    // Quick Play track: same immediate teardown on close.
    stopQuickPlayMusic(fableRoot, { immediate: true });
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
  // The Quick Play void-form (2026-08-05): three free-text fields (player /
  // scenario / desire) + a CREATE button. CREATE is disabled until all three
  // fields have words; on enable it turns white + becomes clickable. The
  // form's onCreate → beginVoidDrift (fade out + seed the world + load the
  // stage). Resume-Last is handled by a popup shown over the form when a
  // quicksave exists (not a tile on this screen).
  screens['quickplay-form'] = buildQuickPlayForm({
    onCreate: (values) => { beginVoidDrift(values); },
  });
  screens.picker = buildPicker({});
  // Player Creator + Picker (Pair 2). Built once; rendered on entry.
  screens['player-creator'] = buildPlayerCreator();
  screens['player-picker'] = buildPlayerPicker();
  // The three Sim Card Creators (2026-08-05): NPC / World / Scenario, each a
  // thin config over the generic wizard-engine. All three author a sim card
  // (distinguished only by which fields the wizard collects). Built once;
  // rendered on entry from the Sim Creator chooser.
  screens['npc-creator'] = buildNpcCreator();
  screens['world-creator'] = buildWorldCreator();
  screens['scenario-creator'] = buildScenarioCreator();
  screens.worlds = buildWorlds({ back: () => exitLoadToTitle() });
  screens.saves = buildSaves({ back: () => showScreen('worlds') });
  for (const s of Object.values(screens)) fableRoot.appendChild(s);
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
