// =============================================================
// SCREEN: STAGE — the narrator surface (the heart of immersion).
//
// Owns the full-screen stage: dialogue feed + input + Wupi drawer +
// panel overlay + toast. Wires engine/* modules together.
//
// This module is the COMPOSITION ROOT for an active game session:
//   - beats.js        → the card-style dialogue feed
//   - narrator.js     → fable_send streaming
//   - wupi-drawer.js  → chat_send (game master) + panel summoning
//   - fx/effects.js   → FX rendering (hooked by narrator)
//   - panels/manager  → read-view overlays summoned by Wupi
//
// Wupi trigger (Phase 1, decision 1): hover the right screen edge
// for 300ms — NO visible button. The pause overlay + Save-As modal
// are gone; Save/Load/Home live in the drawer footer.
// (The LEFT-side vitals/stats panel + mannequin were removed 2026-07-25 —
// shelved pending the rest of the UI settling. See stats-panel.js +
// mannequin.js in git history when the left side is revisited.)
//
// BACKGROUND + WEATHER STRIPPED: the bg <img>, the tone-keyword bg picker,
// and (2026-08-03) the entire atmosphere system (time-of-day background
// filter, keyword-scan weather particles, and mouse parallax) were all
// removed. The stage is a pure black void for ALL games now — no card
// sources a background. The `.fable-stage-bg`
// element stays in the DOM as a no-op over the black .fable-stage so a
// future bg re-add is a one-line structural change. Explicit narrator
// [FX ...] brackets still drive fx/effects.js (a separate, opt-in path).
// =============================================================

import * as beats from '../engine/beats.js';
import * as narrator from '../engine/narrator.js';
import * as wupiDrawer from '../engine/wupi-drawer.js';
import * as leftDrawer from '../engine/left-drawer.js';
// VN INTERACTIONS (2026-08-11): the behavior layer over the .fable-mes feed
// — hysteresis history mask, portrait corner snaps, flank dblclick portrait-
// hide, dblclick-to-edit. Decoupled from narrator.js (knows the DOM, not the
// IPC) — it calls back into stage.js's onEditBeat hook, which mirrors the
// routing the ✎ control-button handler below uses. Returns { teardown };
// stage.js stores the handle + tears it down in teardownStage so the reused
// stage DOM doesn't accumulate listeners/observer across entries.
import * as vn from '../engine/vn-interactions.js';
// SLICE REGENERATE (golden pencil, 2026-08-11): highlights a span of an AI
// message → floating brass pencil → click regenerates only that span in
// place. Self-contained + removable (mirrors vn: returns { teardown }).
import { initSliceRegen } from '../engine/slice-regen.js';
// isActionPopupOpen feeds the left drawer's mouseleave guard: the action popup
// is appended to document.body, so reaching for CONSUME/EQUIP/etc. would
// otherwise fire the drawer's mouseleave + yank it in mid-click. See
// left-drawer.js setActionPopupProbe.
import { isActionPopupOpen } from '../engine/inventory-panel.js';
import { buildTabRail, renderActive, resetTabRail } from '../engine/tab-rail.js';
import { buildRawEditor, onEsc as rawEditorEsc, resetRawEditor, isOpen as rawEditorOpen } from '../engine/raw-editor.js';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
// playFX + clearFX were the weather-render hooks pre-stripping; weather is
// gone now (file header), so only initFX + clearAllFX remain used. The two
// named exports stay imported here so re-adding weather later is a one-line
// restore (the FX registry itself is unchanged).
import { initFX, playFX, clearFX, clearAllFX } from '../fx/effects.js';
// Touch the weather hooks so a strict linter doesn't flag them as unused
// (they're reserved for the weather re-add). No-op at runtime.
void playFX; void clearFX;
import { saveNow } from '../engine/saves-io.js';
// Background Library (2026-08-11): the 4th WUPI-drawer foot button. Owns the
// gallery modal + the dormant .fable-stage-bg paint layer. Global to Fable
// (one marker file, not per-card). Self-contained: builds + caches its own
// overlay appended to the stage (mirrors the save-overlay pattern).
import * as backgrounds from '../engine/backgrounds.js';
import * as soulGems from '../engine/soul-gem.js';
// ONLINE PANEL (2026-08-14): the 5th WUPI-drawer foot button (API). Opens the
// same in-Fable API connection window the title's ONLINE button uses
// (buildOnlinePanel — body-mounted, self-contained close) so the player can
// swap provider/model mid-roleplay without leaving the stage.
import { buildOnlinePanel } from './online.js';
import { initPanelManager, summon as summonPanel, dismissPanel, isActive as panelActive } from '../panels/manager.js';
import { setMapTheme } from '../panels/map.js';
// DEV PREVIEW (?dev=preview): pure-frontend layout preview with no backend.
// Portraits render empty in preview — production resolves them via
// fable_active_card_get / the saved player. (The dev-only sample art that
// used to live here was removed.)

// DEV PREVIEW flag: pure-frontend layout preview (no backend). Same query/hash
// parsing as script.js's DEV_PREVIEW_SHORTCUT. False in production.
const DEV_PREVIEW = (() => {
  try {
    if (new URLSearchParams(window.location.search).get('dev') === 'preview') return true;
    const h = window.location.hash.replace(/^#/, '');
    return new URLSearchParams(h).get('dev') === 'preview';
  } catch (_) { return false; }
})();

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// Background + atmosphere were stripped (see file header). The bg layer
// (.fable-stage-bg) is empty; mapTheme is a static 'fantasy' default so the
// optional map panel still gets a usable atlas theme without depending on
// a card tone.

let stageRoot = null;
let toastTimer = null;
// API-lost composer lock (2026-08-07 override): when the API narrator dies
// mid-session there's no local fallback. The composer greys out, shows a red
// "API LOST CONNECTION" message, + Enter becomes a retry probe. The pending
// turn text is stashed so a successful retry re-sends it.
let composerLocked = false;
let pendingTurnText = '';
// (2026-08-15 audit fix) The text of the turn currently in flight. The
// composer is CLEARED at submit, so when api_lost fires mid-turn the box is
// empty — the old lockComposer stash read input.value and got nothing, killing
// the Enter-retry affordance (the player's typed action was lost to a
// disconnect). Stash the in-flight text here at submit; a delivered turn
// clears it.
let lastSentTurnText = '';
// (2026-08-16 audit fix #26) Timestamp of the last composer submit — the
// double-Enter debounce anchor (a second Enter <350ms after the send is a
// duplicate keypress, not the stop gesture).
let lastSendAt = 0;
let cardContext = null;  // { card, saveId } for the active session
let activeCardName = '';  // display name of the seated card (the typing indicator)
// The seated card's wizard subtype ("npc" | "scenario" | "world" | ''). Drives
// the typing indicator's voice: an npc card reads as the character themselves
// typing; a scenario/world card narrates through the unseen "Narrator".
let activeCardSubtype = '';
let activePlayerName = '';  // protagonist name (card.player_name) → user-beat headers
// Portrait identity for the VN chat (Phase 2 portrait bridge). Resolved in
// refreshActiveCardName from fable_active_card_get's extended return shape.
// Each is already a convertFileSrc-ready asset:// URL (or '' when absent).
let activeCardPortrait = '';  // the card/narrator portrait (NPC fallback too)
let activePlayerPortrait = '';  // the active saved player's portrait
let npcNameMap = new Map();  // npc_id → display name (from the card's <cast>)
let cornerTrigger = null;   // the right-edge hover zone (Wupi drawer)
let cornerDwellTimer = null; // 300ms arm-before-open timer (right edge)
let leftCornerTrigger = null;   // the left-edge hover zone (Card / Tracker drawer)
let leftCornerDwellTimer = null; // 300ms arm-before-open timer (left edge)
let saveModalClose = null;  // fn: close the save modal (set in wireStage, called by Esc)
// (2026-08-19) Cascade-delete confirm: the doomed run's target index while
// the confirm modal is open (-1 = closed). The Confirm button reads this —
// the click that opened the modal may belong to a beat a rebuild already
// replaced, so the index is captured at OPEN time, never re-read from the
// DOM at CONFIRM time.
let deleteConfirmIndex = -1;
let deleteModalClose = null;  // fn: close the delete modal (set in wireStage, called by Esc)
const CORNER_DWELL_MS = 300;

// STORED stage-element listeners (Chloe 2026-07-23: the resource-isolation
// audit). The stage DOM is REUSED across entries (the screen element is
// built once in buildStage), so addEventListener on its child elements
// during wireStage would DOUBLE-BIND on every re-entry (anonymous arrows
// aren't deduped). We store each handler + element ref here and remove
// them in teardownStage so the next wireStage binds exactly once. Each
// entry is [element, type, handler, useCapture?].
let stageListeners = [];
// VN-interactions handle ({ teardown } from vn.init). Stored here so
// teardownStage can release it; null when no live session. The vn module
// owns its own listeners + MutationObserver internally (NOT routed through
// stageListeners, because the observer + the feed-scoped click/dblclick are
// module-internal details stage.js shouldn't enumerate) — its teardown() is
// the single release point.
let vnApi = null;
// Slice-regen handle ({ teardown } from initSliceRegen). Same discipline as
// vnApi: stored here so teardownStage releases it; null when no live session.
let sliceApi = null;

export function buildStage() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-stage';
  root.dataset.fableScreen = 'stage';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-stage-bg"></div>
    <div class="fable-fx-layer" data-fx></div>
    <!-- The card-style dialogue feed. Each narrator/user turn renders as a
         rounded glass card with a flush-left 2:3 portrait (dissolved inner
         edge) + a name header + the prose body. engine/beats.js owns the
         DOM; this is its mount point. Sits above the bg/FX, below the input
         row + drawers.
         (2026-08-19) FEED VIEWPORT WRAPPER: the feed + the top blur strip
         share this wrapper. The wrapper's opacity (0.9999 — visually a
         no-op) establishes a BACKDROP ROOT, which is the load-bearing fence
         for the strip: its backdrop-filter can only sample content painted
         INSIDE this wrapper (the chat bubbles) — never the stage
         background, the FX layer, or the OS top bar (which also sits z
         9999 above the whole stage). The strip itself is pointer-events:
         none, so nothing here can ever intercept the chrome-peek top bar
         or a bubble click. -->
    <div class="fable-feed-viewport">
      <div class="fable-feed" data-feed></div>
      <!-- 12px bubbles-only frosted band at the screen's top edge: the
           transcript reads as always a tiny bit faded at the very top, so
           bubbles appear to fade in as they scroll up under it. -->
      <div class="fable-feed-topblur" aria-hidden="true"></div>
    </div>
    <!-- Typing indicator (re-instated 2026-08-19, subtype-aware): a small
         label pinned just above the input row while a turn is in flight.
         npc-subtype cards read "(Name) is currently typing..."; scenario/world
         cards read "Narrator is currently thinking...". Toggled via the
         .is-visible class from setGenerating (the onTurnStart/onTurnEnd
         hooks) — it covers the WHOLE turn, including the silent tracker
         stage before the first chunk streams. -->
    <div class="fable-typing-indicator" data-typing-indicator aria-hidden="true">
      <span class="fable-typing-indicator__text" data-typing-text></span>
    </div>
    <form class="fable-input-row" data-input-form>
      <!-- The input is a single centered, max-width text box. The send button
           is GONE (2026-07-27): generation is fired by pressing Enter on a
           non-empty field, and stopped by pressing Enter on an EMPTY field
           while generation is in flight. One key, two intents — seamless. -->
      <div class="fable-input-group">
        <div class="fable-input-box">
          <textarea class="fable-input" data-input rows="1" placeholder="Type a message..."></textarea>
        </div>
      </div>
    </form>

    <!-- Invisible right-edge hover zone (decision 1: hover the right edge,
         no visible button). Wider strip (~2 inches); a 300ms dwell arms on
         mouseenter → opens the Wupi drawer; mouseleave cancels. Pattern
         mirrors script.js dock hover-reveal (mouse grace timer) + chrome.js
         edge peek. Wired in wireStage, removed in teardownStage (global
         listener MUST be unwired or it leaks across game restarts). -->
    <div class="fable-corner-trigger" data-corner-trigger aria-hidden="true"></div>

    <!-- RIGHT-edge LOCK BAR — standalone super-thin strip at the absolute
         right edge. INVISIBLE by default; only visible when the Wupi drawer
         is open AND the mouse touches the complete edge. Click toggles the
         lock (color change only). Mirrors the left-edge lock bar. -->
    <div class="fable-edge-lock fable-edge-lock--right" data-wupi-edge-lock aria-hidden="true"></div>

    <!-- LEFT-edge LOCK BAR — standalone super-thin strip at the absolute
         left edge. INVISIBLE by default; only visible when the left drawer
         is open AND the mouse touches the complete edge. Click toggles the
         lock (color change only). Exact mirror of the right-edge lock bar. -->
    <div class="fable-edge-lock fable-edge-lock--left" data-left-edge-lock aria-hidden="true"></div>

    <!-- Invisible LEFT-edge hover zone — the mirror of the right-edge
         fable-corner-trigger. A 300ms dwell arms on mouseenter → opens the
         left (Card / Tracker) drawer; mouseleave cancels. -->
    <div class="fable-corner-trigger fable-corner-trigger--left" data-left-corner-trigger aria-hidden="true"></div>

    <!-- LEFT drawer mount point (Card / Tracker tabs). The element is built
         by engine/left-drawer.js and injected here in buildStage. Slides in
         from the left (exact mirror of the right Wupi drawer: hover-to-open
         + edge-lock, no visible handle). -->
    <div class="fable-left-drawer-mount" data-left-mount></div>

    <aside class="fable-wupi-drawer" data-wupi-drawer>
      <!-- Chloe 2026-07-26: replaced the 🐾 avatar + "Wupi / Game Master"
           sublabel with a single centered, large, bold, glowy "WUPI"
           wordmark. The drawer's identity IS the brand. -->
      <header class="fable-wupi-header">
        <div class="fable-wupi-brand">WUPI</div>
      </header>
      <!-- The tracked-stat tab rail (Player / Sim Card / Codex / World / NPC).
           Built by engine/tab-rail.js + injected here in buildStage. Sits
           between the brand + the chat messages. A toggled tab drops down its
           read-only prose panel; the rail RESETS on drawer close
           (resetTabRail) + re-selects per open. -->
      <div class="fable-tab-rail-mount" data-tab-rail-mount></div>
      <div class="fable-wupi-messages" data-wupi-messages></div>
      <form class="fable-wupi-input-row" data-wupi-form>
        <textarea class="fable-wupi-input" data-wupi-input rows="1" placeholder="Ask WUPI anything…"></textarea>
      </form>
      <!-- Drawer footer: the stage's save/load/home controls as ICON
           buttons. Phase 2: the native emoji glyphs (💾 📂 🏠) were replaced
           with sleek minimalist brass SVGs (see .fable-foot-icon svg in
           fable.css) — the OS emojis read as a dev stub, not the game's UI.
           Save opens a center modal for the name (quick or named); Load
           fires the onLoad hook (save picker, future); Home returns to the
           title via the onExit hook. aria-label + title preserved for a11y. -->
      <footer class="fable-wupi-footer" data-wupi-footer>
        <div class="fable-wupi-foot-actions">
          <!-- Background Library (2026-08-11): the 4th foot tool, FIRST child
               so it lands far-left (the flex row is DOM-ordered LTR, centered).
               Opens the gallery modal over the stage. Visible in ALL modes —
               backgrounds are global. -->
          <button class="fable-foot-icon" data-foot-bg aria-label="Background" title="Background">
            <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="5" width="18" height="14" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="8.5" cy="10" r="1.6" fill="currentColor"/><path d="M3 16l4.5-4 3.5 3 4-5 6 6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round" stroke-linecap="round"/></svg>
          </button>
          <button class="fable-foot-icon" data-foot-save aria-label="Save" title="Save">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 3h11l3 3v15a0 0 0 0 1 0 0H5a0 0 0 0 1 0 0V3z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><rect x="8" y="3" width="7" height="5" rx="0.8" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="8" y="12" width="8" height="6" rx="0.6" fill="none" stroke="currentColor" stroke-width="1.4"/></svg>
          </button>
          <button class="fable-foot-icon" data-foot-load aria-label="Load" title="Load">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M12 17V9M12 9l-3 3M12 9l3 3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>
          </button>
          <!-- API (2026-08-14): the 5th foot tool, 2nd-to-last (between Load
               and Home). Opens the in-Fable API connection window (the SAME
               buildOnlinePanel the title's ONLINE button opens) so the player
               can swap provider/model mid-roleplay without leaving the stage.
               Also the reconnect path after a mid-session api_lost. -->
          <button class="fable-foot-icon" data-foot-api aria-label="API" title="API">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M9 8V3M15 8V3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><path d="M6 8h12v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M12 17v4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>
          </button>
          <button class="fable-foot-icon" data-foot-home aria-label="Home" title="Home">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 11l8-7 8 7" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/><path d="M6 10v9a1 1 0 0 0 1 1h10a1 1 0 0 0 1-1v-9" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M10 20v-5h4v5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/></svg>
          </button>
        </div>
      </footer>
    </aside>

    <!-- Save modal: a centered card for naming a save (or quick-saving).
         Hidden by default; the Save icon opens it. Mirrors the panel-overlay
         modal pattern (backdrop + centered card + Esc/backdrop dismiss).
         Two actions: Quick Save (autosave slot, no name) or Save (named slot
         using the typed name). -->
    <div class="fable-save-overlay" data-save-overlay hidden>
      <div class="fable-save-backdrop" data-save-backdrop></div>
      <div class="fable-save-modal">
        <h2 class="fable-save-title">Save Your Progress</h2>
        <input class="fable-save-name-input" data-save-name-input type="text" placeholder="Name this save (or leave blank)" />
        <div class="fable-save-actions">
          <button class="fable-save-btn ghost" data-save-quick>Quick Save</button>
          <button class="fable-save-btn primary" data-save-named>Save</button>
        </div>
        <button class="fable-save-close" data-save-close aria-label="Close">✕</button>
      </div>
    </div>

    <!-- Cascade-delete confirm (2026-08-19, Chloe): deleting a beat removes
         it AND every message below it (a transcript gap is meaningless), so
         any doomed run of 2+ messages gets an explicit count-first confirm —
         "You are going to delete N messages, are you sure?" The SOLE
         trailing beat never opens this (the two-click arm already served as
         its confirmation). Mirrors the save modal's overlay pattern; Esc +
         backdrop + Cancel all dismiss. -->
    <div class="fable-delete-overlay" data-delete-overlay hidden>
      <div class="fable-save-backdrop" data-delete-backdrop></div>
      <div class="fable-save-modal fable-delete-modal">
        <h2 class="fable-save-title">Delete Messages</h2>
        <p class="fable-delete-text" data-delete-text></p>
        <div class="fable-save-actions">
          <button class="fable-save-btn ghost" data-delete-cancel>Cancel</button>
          <button class="fable-save-btn primary" data-delete-confirm>Delete</button>
        </div>
      </div>
    </div>

    <div class="fable-panel-overlay" data-panel-overlay hidden>
      <div class="fable-panel-backdrop"></div>
      <div class="fable-panel" data-panel-host></div>
    </div>

    <div class="fable-toast" data-toast hidden></div>
  `;
  // Inject the left drawer (now an empty shell) into the mount point. Built by
  // engine/left-drawer.js; reused across entries. Opens via the left hover
  // strip (mirrors the right Wupi drawer).
  const leftMount = root.querySelector('[data-left-mount]');
  if (leftMount) {
    leftMount.appendChild(leftDrawer.buildLeftDrawer());
  }
  // Inject the tab rail into the Wupi drawer (between brand + messages) + the
  // raw-editor overlay onto the stage. Both built once, reused across entries.
  const railMount = root.querySelector('[data-tab-rail-mount]');
  if (railMount) railMount.appendChild(buildTabRail());
  root.appendChild(buildRawEditor());
  return root;
}

// Wire the stage after it's in the DOM. cardContext: { card, saveId }.
// Async because it awaits the active-card identity fetch (so the narrator
// builders have the card/player names in hand before the first beat could
// render). Callers fire-and-forget; the return value (a Promise) is unused.
export async function wireStage(root, hooks) {
  // (2026-08-15 audit fix) Epoch capture: if this wiring is superseded (a
  // re-entry) or torn down (fast Home) across any await below, stop wiring.
  const myEpoch = ++wireEpoch;
  stageRoot = root;
  cardContext = hooks.cardContext || null;

  const fxLayer = root.querySelector('[data-fx]');

  // Background stripped (file header). The bg layer (.fable-stage-bg) is
  // empty — it paints nothing over the pure black .fable-stage. Map theme
  // is a static 'fantasy' default so the optional map panel still works
  // without a card tone.
  setMapTheme('fantasy');

  // Engine init (composition root). beats owns the [data-feed] dialogue
  // surface; hand it the element so its builders can append cards.
  const feedEl = root.querySelector('[data-feed]');
  beats.initBeats(feedEl);
  // HOVER TOOLRAIL click routing (2026-08-14): one delegated listener on
  // the feed routes [data-drawer-act] button clicks to the narrator.js
  // mutation wrappers. vn-interactions already short-circuits its
  // snap/dblclick handlers for .fable-mes-drawer, so these clicks are
  // isolated. Gated on isGenerating/isRerolling so no mid-stream mutation —
  // EXCEPT › during an in-flight REROLL (the interrupt-and-restart affordance).
  //   edit   → enterEditMode (user → rewindAndEditUser, AI → editMessage);
  //            a SECOND press while the editor is open SAVES the edit
  //            (commit — the ✎ toggles, mirroring Enter; Esc cancels)
  //   delete → deleteMessage (with a confirm — destructive)
  //   prev   → swipeVariant(index, active-1)
  //   next   → swipeNextAction: swipe to the next variant, OR reroll at the
  //            last variant. There is NO dedicated Regenerate button
  //            (permanently removed 2026-08-14) — the ▼ arrow IS the
  //            regenerate feature (cut the local tracker + switch back and
  //            forth); engine/drawer-logic.js pins the fold branch.
  //
  // SINGLE-EDITOR DISCIPLINE (2026-08-14): the feed allows ONE inline editor.
  // Every action below first commits any OTHER open editor + waits out its
  // save — a second editor's text would otherwise be silently lost when the
  // first save's feed rebuild lands (the rebuild replaces every node, and a
  // user-beat save REWINDS, which truncates beats + shifts indexes).
  // (P1 fix) Routed through `on()` like every other stage-element listener:
  // the stage DOM persists across entries and wireStage re-runs each entry —
  // a raw addEventListener double-binds, making every ✎/🗑/‹› click fire N
  // times (a doubled ✎ opened-then-saved instantly, breaking inline editing
  // from session 2 onward; on a user beat that fired rewind+regen).
  // (#78) Two-click inline delete confirm — helpers. Native window.confirm
  // is DEAD in the webview (wry disables default script dialogs: confirm()
  // always returns false → the delete button was a silent NO-OP). First
  // click ARMS the button (red heat + 5s auto-disarm); the second click
  // deletes. The armed state lives on the button node itself, so any feed
  // rebuild (which replaces the node) disarms for free. One armed button at
  // a time.
  const DELETE_ARM_TIMEOUT_MS = 5000;
  function armDeleteButton(btn) {
    disarmDeleteButton(btn);
    btn.dataset.armed = '1';
    btn.dataset.origTitle = btn.title || '';
    btn.title = 'Click again to delete';
    btn._disarmTimer = setTimeout(() => disarmDeleteButton(btn), DELETE_ARM_TIMEOUT_MS);
  }
  function disarmDeleteButton(btn) {
    if (btn._disarmTimer) { clearTimeout(btn._disarmTimer); btn._disarmTimer = null; }
    if (btn.dataset.armed === '1') {
      btn.dataset.armed = '';
      if (btn.dataset.origTitle !== undefined) btn.title = btn.dataset.origTitle;
      delete btn.dataset.origTitle;
    }
  }

  on(feedEl, 'click', async (e) => {
    // Both control surfaces route here: the .vn-recent hover toolrail AND
    // the .vn-history iron tools column (same data-drawer-act hooks).
    const btn = e.target.closest(
      '.fable-mes-drawer [data-drawer-act], .fable-mes-histools [data-drawer-act]');
    if (!btn || btn.disabled) return;
    let beat = btn.closest('.fable-mes');
    if (!beat) return;
    const index = Number.parseInt(beat.dataset.index || '-1', 10);
    if (index < 0) return; // unindexed beat (e.g. system/error) — not a message
    const role = beat.dataset.role;
    const act = btn.dataset.drawerAct;
    if (narrator.isGenerating() || narrator.isRerolling()) {
      // Mid-turn, exactly ONE control stays live (Stage 3): › during an
      // in-flight REROLL aborts the roll (discard the partial + revert the
      // schema to base) and immediately starts a fresh one. Everything else
      // waits for the turn to finish.
      if (act === 'next' && narrator.isRerolling()) narrator.interruptAndReroll();
      return;
    }
    if (act === 'edit' && beats.exitEditMode(beat, true)) {
      // Same-beat ✎ toggle: the edit just SAVED. Its save (tracker
      // re-track) is now in flight — every other action waits it out via
      // the isGenerating gate above.
      return;
    }
    if (act === 'delete' && beats.isEditing(beat)) {
      // Deleting the beat being edited: DISCARD the editor (no save — the
      // beat is about to vanish; saving + re-tracking it first would be
      // pure waste).
      beats.exitEditMode(beat, false);
    }
    // A DIFFERENT beat's editor may still be open — commit it + let the
    // save settle before mutating anything else.
    const openBeat = beats.openEditingBeat();
    const pendingSave = beats.commitOpenEditor();
    if (pendingSave) {
      await pendingSave;
      if (narrator.isGenerating() || narrator.isRerolling()) return;
      // A user-beat save rewound the timeline (truncation + regen): indexes
      // shifted — bail + let the user re-orient. An AI-beat save
      // (editMessage) rebuilds the feed WITHOUT shifting indexes →
      // re-resolve the target node (the rebuild replaced it) + continue.
      if (openBeat && openBeat.dataset.role === 'user') return;
      const fresh = feedEl.querySelector(`.fable-mes[data-index="${index}"]`);
      if (!fresh) return;
      beat = fresh;
    }
    if (act === 'edit') {
      // (P2b) Click-time authority, mirroring the › gate: the ✎ must respect
      // the backend edit_message contract — any USER beat, or the TRAILING
      // assistant beat. A mid-history AI edit is refused server-side; the old
      // optimistic editor left the beat blank on the failed save.
      if (!beats.canEditMessage({
        role,
        isLastAssistant: beats.isTrailingAssistant(beat),
      })) return;
      beats.enterEditMode(beat, {
        onSave: (text) => {
          // Player → rewind + branch + regen ("I changed what I did");
          // AI beat → edit + schema re-track ("the beat now says something
          // else — undo its last track + re-track the new information").
          // Mirrors the onEditBeat hook above (the ✎/dblclick routing).
          // The returned promise propagates through exitEditMode/
          // commitOpenEditor so the single-editor handoff can await it.
          if (role === 'user') return narrator.rewindAndEditUser(index, text);
          return narrator.editMessage(index, text);
        },
      });
    } else if (act === 'delete') {
      // (#78) Two-click inline confirm (the #24 API-profile-delete pattern —
      // see the helpers above): first click arms, second click deletes.
      // (2026-08-19 cascade, Chloe) A delete now takes every message BELOW
      // the target too — a transcript gap between surviving beats is
      // meaningless. The arm stays the first gate; the second click either
      // deletes the SOLE trailing beat outright (no further warning — the
      // arm WAS the confirmation) or opens the count-first confirm modal
      // for a doomed run of 2+ ("You are going to delete N messages, are
      // you sure?"). The minimum the modal ever shows is therefore 2.
      if (btn.dataset.armed !== '1') {
        feedEl.querySelectorAll('[data-drawer-act="delete"][data-armed="1"]')
          .forEach(disarmDeleteButton);
        armDeleteButton(btn);
        return;
      }
      disarmDeleteButton(btn);
      const doomed = beats.sessionMessageCount() - index;
      if (doomed > 1) {
        openDeleteConfirm(index, doomed);
      } else {
        narrator.deleteMessage(index);
      }
    } else if (act === 'prev') {
      const active = Number.parseInt(beat.dataset.variantActive || '0', 10);
      if (active > 0) narrator.swipeVariant(index, active - 1);
    } else if (act === 'next') {
      const count = Number.parseInt(beat.dataset.variantCount || '1', 10);
      const active = Number.parseInt(beat.dataset.variantActive || '0', 10);
      // (#84 2026-08-15) Click-time authority: the stamped disabled-state is
      // advisory and can be stale (a live beat's › is refreshed post-append
      // now, but a rebuild/append race can still leave it a beat behind).
      // Re-derive canNext HERE — a non-trailing single-variant beat must
      // no-op instead of rerolling the trailing turn.
      const { canNext } = beats.computeDrawerState({
        role,
        count,
        active,
        isLastAssistant: beats.isTrailingAssistant(beat),
      });
      if (!canNext) return;
      const action = beats.swipeNextAction({ count, active });
      if (action.kind === 'swipe') narrator.swipeVariant(index, action.variantIdx);
      else narrator.rerollLastTurn();
    }
  });
  // VN INTERACTIONS: attach the behavior layer (history tagging, snaps,
  // flank dblclick, dblclick-to-edit) to the same feed + stage. Initialized
  // AFTER beats.initBeats so the feedEl ref is live + any seed beats
  // (opening scene / loaded history) are present for the first
  // refreshHistory pass. The
  // onEditBeat hook mirrors the ✎ control-button routing at the feed click
  // handler below (user beat → editMessage, assistant → rewind-and-edit) so
  // dblclick-on-prose opens the SAME editor the ✎ button does.
  vnApi = vn.init({
    stageRoot: root,
    feedEl: root.querySelector('[data-feed]'),
    onEditBeat: async (beat) => {
      if (narrator.isGenerating() || narrator.isRerolling()) return;
      // Same-beat dblclick → SAVE (same as the ✎ toggle + Enter; Esc is
      // the cancel gesture), never a re-enter (which would nest editors +
      // drop the in-progress edits).
      if (beats.exitEditMode(beat, true)) return;
      // Another beat's editor is open → commit it + wait out its save
      // (single-editor discipline — see the feed click handler above).
      const openBeat = beats.openEditingBeat();
      const pendingSave = beats.commitOpenEditor();
      if (pendingSave) {
        await pendingSave;
        if (narrator.isGenerating() || narrator.isRerolling()) return;
        // A user-beat save rewound the timeline — bail; otherwise the
        // editMessage rebuild replaced the nodes → re-resolve by index.
        if (openBeat && openBeat.dataset.role === 'user') return;
        const fresh = feedEl.querySelector(`.fable-mes[data-index="${beat.dataset.index}"]`);
        if (!fresh) return;
        beat = fresh;
      }
      const index = Number.parseInt(beat.dataset.index || '-1', 10);
      if (index < 0) return; // unindexed beat — not a backend message
      const isUser = beat.dataset.role === 'user';
      // (P2b) The ✎-button gate applies to the dblclick path too — a
      // mid-history AI beat must never open a doomed editor.
      if (!beats.canEditMessage({
        role: isUser ? 'user' : 'assistant',
        isLastAssistant: beats.isTrailingAssistant(beat),
      })) return;
      beats.enterEditMode(beat, {
        onSave: (text) => {
          // Player → rewind + branch + regen ("I changed what I did"); AI
          // beat → edit + schema re-track (the backend reverts to the
          // message's base_schema + re-runs the local tracker). Inverted
          // from the prior wiring (which called rewind_and_edit_user on
          // assistant beats — that errors: the command requires a user
          // target). (P1 fix) RETURN the promise — without it a second
          // dblclick treated the save as not-pending and reopened a doomed
          // editor whose text the rebuild vaporized.
          if (isUser) return narrator.rewindAndEditUser(index, text);
          return narrator.editMessage(index, text);
        },
      });
    },
  });
  initFX(fxLayer, root, { onTransient: () => {} });
  // The message-header identity: fetch the active card's name + player_name
  // ONCE (best-effort) so the narrator builders can stamp the headers.
  // Awaited so the names are in hand before initNarrator forwards them (a
  // fetch failure falls back to generic labels).
  await refreshActiveCardName(root);
  // (2026-08-15 audit fix) The card-name IPC awaited — a fast Home may have
  // torn the stage down (or a re-entry superseded us) while suspended. Any
  // listener registered now would be untracked forever. Stop wiring.
  if (myEpoch !== wireEpoch) return;
  narrator.initNarrator({
    onTurnStart: () => {
      setGenerating(true);
    },
    onTurnEnd: (info) => {
      setGenerating(false);
      // (2026-08-16 audit fix #5) A REVERTED turn (soft cancel / api_lost /
      // dev-narrator error) hands its typed text back — the user bubble was
      // removed in lockstep with the backend's pop, so this restore is the
      // only thing keeping the player's action from vanishing. Skipped while
      // the composer is API-locked (the pendingTurnText stash owns the retry
      // there; refilling the box would fight the lock's cleared state).
      if (info && typeof info.revertedText === 'string' && info.revertedText && !composerLocked) {
        const input = stageRoot && stageRoot.querySelector('[data-input]');
        if (input && !input.value.trim()) {
          input.value = info.revertedText;
          autoGrow(input);
          input.focus();
        }
      }
      // (2026-08-15) The turn delivered — its text was consumed; the api_lost
      // stash must not resurrect a stale action on some FUTURE disconnect.
      lastSentTurnText = '';
      // §11.30 Left-Drawer HUD: a turn may have mutated player body (Combat
      // Referee), entities (item-grant brackets), clock/weather (TIME/
      // WEATHER commands). Refresh the paperdoll + ambient + inventory so
      // the HUD reflects the new ground truth. Best-effort (no await — the
      // UI re-enable above must not block on IPC latency).
      leftDrawer.refreshAll();
    },
    npcPretty: hooks.npcPretty || ((id) => npcNameMap.get(id) || null),
    cardName: activeCardName,
    playerName: activePlayerName,
    // Phase 2 portrait bridge: pass the resolved asset:// URLs + the npc name
    // map into the narrator, which forwards them to beats.setIdentity for the
    // VN renderer. npcPortraits is deferred (empty) — NPCs fall back to the
    // card portrait until per-NPC portrait storage exists.
    cardPortrait: activeCardPortrait,
    playerPortrait: activePlayerPortrait,
    npcNames: npcNameMap,
    // Chat-side schema patch hook (2026-08-11): when the operator asks WUPI
    // via chat to fable_schema_patch the active session, the backend emits
    // `fable-session-changed` kind=schema; narrator forwards the merged_keys
    // here. Same refresh as onTurnEnd — the paperdoll/inventory may have moved.
    onSchemaPatch: (_mergedKeys) => { leftDrawer.refreshAll(); },
    // (D5 2026-08-16) Editor-restore hook: a chat-side messages rebuild that
    // lands while a narrator turn streams cancels the open inline editor
    // (its save would be dropped by the generating guard after the body
    // swap); narrator captures the in-progress edit + fires this after the
    // rebuild. Re-open the editor SEEDED with the typed text, wiring the
    // SAME role-shaped onSave the ✎/dblclick paths use (mirror the
    // delegated [data-drawer-act="edit"] handler). Single-editor discipline:
    // never open over an existing editor; a vanished beat no-ops (narrator
    // pre-checks the index, this re-resolves defensively).
    onRestoreEditor: ({ index, text, role }) => {
      if (beats.openEditingBeat()) return;
      if (typeof text !== 'string') return;
      const beat = beats.beatByIndex(index);
      if (!beat) return;
      // Never restore onto a beat the rebind just claimed as the streaming
      // target (a reroll/slice re-bind): appendChunk/beginSliceRegen write
      // the body wholesale — the textarea (and the typed text with it) would
      // be clobbered by the very next chunk.
      if (beat.classList.contains('streaming')
          || beat.classList.contains('slice-regenerating')) return;
      beats.enterEditMode(beat, {
        seed: text,
        onSave: (newText) => {
          if (role === 'user') return narrator.rewindAndEditUser(index, newText);
          return narrator.editMessage(index, newText);
        },
      });
    },
    // Schema-ring-buffer handoff: when a mutation command (reroll / rewind)
    // returns `schema_pop_count`, invoke `fable_rollback` that many times to
    // restore the matching world-state snapshots. `fable_rollback` is the
    // parallel Rust command (lib.rs) that pops `AppState::fable_schema_
    // history`. Each pop emits a `fable_rollback` event the world-state UI
    // can subscribe to; we ignore the returned diff here (the feed rebuild
    // is the visible signal). Best-effort: a rollback failure is logged,
    // not surfaced — the message-history mutation already succeeded.
    onSchemaPop: async (count) => {
      for (let i = 0; i < count; i++) {
        try {
          await invoke('fable_rollback');
        } catch (err) {
          console.warn('[fable] schema rollback failed:', err);
          break;
        }
      }
    },
    // 2026-08-07 override: the API narrator died mid-session (no local fallback).
    // Lock the composer with the red "API LOST CONNECTION" state + surface a
    // retry affordance. The player reconnects via the OS Settings AI panel.
    onApiLost: (message) => {
      lockComposer(message);
    },
  });

  // Golden-pencil slice regen: highlight a span of an AI message → a brass
  // pencil floats at the selection's right edge → click regenerates only that
  // span in place. isGenerating gates it off during any turn; the regen routes
  // through narrator.regenerateSlice (own Channel + slice_done protocol).
  // Torn down in teardownStage alongside vnApi.
  sliceApi = initSliceRegen({
    isGenerating: narrator.isGenerating,
    onRegenerate: narrator.regenerateSlice,
  });

  // Panel manager: overlay + host. onDismiss hides the overlay element.
  const panelOverlay = root.querySelector('[data-panel-overlay]');
  const panelHost = root.querySelector('[data-panel-host]');
  initPanelManager(panelOverlay, panelHost, {
    onDismiss: () => { panelOverlay.hidden = true; },
  });

  // Wupi drawer.
  wupiDrawer.initWupiDrawer({
    drawerEl: root.querySelector('[data-wupi-drawer]'),
    messagesEl: root.querySelector('[data-wupi-messages]'),
    inputEl: root.querySelector('[data-wupi-input]'),
    form: root.querySelector('[data-wupi-form]'),
    closeBtn: root.querySelector('[data-wupi-close]'),
    panelManager: {
      summon: (focus, entities, schema) => {
        // Inject activities into schema if we have them from the card.
        const fullSchema = Object.assign({}, schema, {
          activities: (cardContext && cardContext.card && cardContext.card.activities) || [],
        });
        summonPanel(focus, entities, fullSchema);
        panelOverlay.hidden = false;
      },
    },
  });

  // Input form (narrator turn). All stage-element listeners go through
  // on() so teardownStage removes them — the stage DOM is reused across
  // entries, so a raw addEventListener would double-bind on re-wireStage.
  //
  // ENTER does double duty (2026-07-27, no send button): on a non-empty field
  // it submits the turn; on an EMPTY field while generation is in flight it
  // stops the generation. Shift+Enter is a literal newline. The input stays
  // focusable + enabled during generation so the empty-Enter-to-stop works.
  const inputForm = root.querySelector('[data-input-form]');
  const input = root.querySelector('[data-input]');
  on(inputForm, 'submit', async (e) => {
    e.preventDefault();
    const text = input.value.trim();
    // Block new turns while a narrator turn is in flight.
    if (!text || narrator.isGenerating()) return;
    // Single-editor discipline: settle any open inline editor BEFORE the
    // turn starts — a save fired mid-generation would be dropped by
    // narrator's generating guard while its optimistic DOM update had
    // already landed (silent desync). Committing a USER-beat editor here
    // runs its rewind + regen first; the composer text then sends as the
    // follow-up turn (correct order, nothing lost).
    const pendingSave = beats.commitOpenEditor();
    if (pendingSave) {
      await pendingSave;
      if (narrator.isGenerating() || narrator.isRerolling()) return;
    }
    // (2026-08-16 audit LOW) Clear ONLY if the field still holds the
    // submitted text — the editor save above can take seconds (an assistant
    // re-track is a local decode), and clearing unconditionally destroyed
    // whatever the player typed into the composer during that wait.
    if (input.value.trim() === text) {
      input.value = '';
      input.style.height = 'auto';
    }
    lastSendAt = Date.now(); // (audit #26) the double-Enter debounce anchor
    lastSentTurnText = text; // (2026-08-15) the api_lost retry affordance's source
    narrator.sendFableTurn(text);
  });
  on(input, 'keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      // API-locked composer (2026-08-07): Enter re-checks the connection. If
      // the API is back, unlock + re-send the pending turn; otherwise re-toast.
      if (composerLocked) {
        retryIfApiReady();
        return;
      }
      if (narrator.isGenerating() && !input.value.trim()) {
        // (2026-08-16 audit fix #26) Double-Enter debounce: the composer is
        // cleared synchronously at submit, so a second Enter ~100ms after the
        // first read as "empty Enter mid-generation" and instantly STOPPED
        // the just-started turn. The stop affordance stays — it just needs
        // the user to have SEEN the turn start (350ms).
        if (Date.now() - lastSendAt < 350) return;
        // Empty Enter mid-generation → stop. Route to the slice cancel slot
        // if a golden-pencil regen is in flight (distinct from the full-turn
        // fable_stop slot — Bug #7 cross-wire lesson); otherwise the normal
        // narrator stop. The seamless stop affordance.
        if (narrator.isSliceRegenerating()) narrator.stopSliceRegen();
        else narrator.stopFableTurn();
        return;
      }
      inputForm.requestSubmit();
    }
  });
  on(input, 'input', () => autoGrow(input));
  setGenerating(false);

  // Per-beat UX controls (edit / delete / ‹ › variant nav) live in the
  // HOVER TOOLRAIL (`.fable-mes-drawer` v2, engine/beats.js) — the gold
  // corner pair + the role-shaped variant capsule on the 2 latest beats.
  // This module routes the rail's `[data-drawer-act]` clicks to the
  // narrator.js mutation API (editMessage / deleteMessage / swipeVariant /
  // rerollLastTurn / rewindAndEditUser); the pure decision logic lives in
  // engine/drawer-logic.js.

  // Wupi trigger: invisible right-edge strip with a 300ms dwell.
  // No visible button (decision 1). The strip element is sized/positioned
  // in CSS (tall, thin, right:0); here we just arm the dwell timer on
  // enter + cancel on leave. Pattern mirrors script.js dock hover-reveal
  // (grace timer) + chrome.js edge peek. Tracked via on() so re-wireStage
  // doesn't double-bind.
  cornerTrigger = root.querySelector('[data-corner-trigger]');
  on(cornerTrigger, 'mouseenter', armCornerDwell);
  on(cornerTrigger, 'mouseleave', cancelCornerDwell);

  // Auto-pull-in: when the mouse fully exits the Wupi drawer, it slides
  // back in UNLESS locked. The lock is toggled by a separate super-thin
  // EDGE bar (see below), NOT a button on the drawer — the drawer itself
  // has no visible lock UI.
  const wupiDrawerEl = root.querySelector('[data-wupi-drawer]');
  on(wupiDrawerEl, 'mouseleave', () => wupiDrawer.onDrawerMouseLeave());

  // EDGE LOCK BAR — a super-thin strip at the absolute right screen edge.
  // INVISIBLE by default. It becomes visible ONLY when:
  //   (a) the Wupi drawer is open, AND
  //   (b) the mouse is touching the complete right edge (within EDGE_HIT_PX).
  // Moving away from the edge hides the bar; the drawer stays if locked.
  // Click toggles the lock — color change only (brass = unlocked, magenta =
  // locked), no glyph. The pop-out zone (wider, ~2 inches) is separate;
  // the UI always pops out FIRST, the edge lock only appears at the very
  // edge afterward.
  // How close to the absolute edge counts as "touching" the lock bar. Widened
  // 2026-08-10 (6 → 14) alongside the bar itself (2px → 10px) so the wider bar
  // is easy to summon + click without pixel-precise aiming. Both edges share it.
  const EDGE_HIT_PX = 14;
  const wupiEdgeLock = root.querySelector('[data-wupi-edge-lock]');

  // Wire the edge-lock visibility probe into the wupi drawer module. Its
  // onDrawerMouseLeave checks this probe: if the edge lock is visible, the
  // auto-close is suppressed so the user can move onto the lock to click it
  // without the drawer phasing out (the lock is a separate element on top of
  // the drawer — reaching for it fires mouseleave on the drawer).
  wupiDrawer.setEdgeLockProbe(() => wupiEdgeLock && wupiEdgeLock.classList.contains('visible'));

  // Reflect the current lock state onto the edge bar's color. Called after
  // every toggle + on initial show.
  function syncWupiEdgeLockColor() {
    if (wupiEdgeLock) wupiEdgeLock.classList.toggle('locked', wupiDrawer.isLocked());
  }

  // The mousemove handler watches the pointer's distance from the right edge.
  // When within EDGE_HIT_PX of the right edge AND the wupi drawer is open,
  // show the edge bar; otherwise hide it. One listener so it stays cheap +
  // tear-down is one removeEventListener.
  // (The left-edge branch was removed 2026-07-25 with the left UI.)
  function onStageMouseMove(e) {
    const rect = root.getBoundingClientRect();
    const fromRight = rect.right - e.clientX;
    if (wupiEdgeLock) {
      const show = fromRight <= EDGE_HIT_PX && wupiDrawer.isOpen();
      wupiEdgeLock.classList.toggle('visible', show);
    }
  }
  on(root, 'mousemove', onStageMouseMove);

  // Click toggles the wupi lock. The color sync runs after toggle so the bar
  // reflects the new state immediately (stays visible until the mouse leaves
  // the edge).
  on(wupiEdgeLock, 'click', (e) => {
    e.stopPropagation();
    wupiDrawer.toggleLock();
    syncWupiEdgeLockColor();
  });

  // ── LEFT DRAWER (Card / Tracker) — exact mirror of the right Wupi drawer.
  // Invisible left-edge hover strip with a 300ms dwell, a left edge-lock bar,
  // and mouseleave auto-close (suppressed when locked). No visible handle.
  leftCornerTrigger = root.querySelector('[data-left-corner-trigger]');
  on(leftCornerTrigger, 'mouseenter', armLeftCornerDwell);
  on(leftCornerTrigger, 'mouseleave', cancelLeftCornerDwell);

  const leftDrawerEl = root.querySelector('[data-left-drawer]');
  on(leftDrawerEl, 'mouseleave', () => leftDrawer.onDrawerMouseLeave());

  const leftEdgeLock = root.querySelector('[data-left-edge-lock]');
  leftDrawer.setEdgeLockProbe(() => leftEdgeLock && leftEdgeLock.classList.contains('visible'));
  // While the inventory action popup is open, the unlocked drawer must NOT
  // auto-close on mouseleave (the popup lives on document.body, so the mouse
  // crosses the drawer boundary reaching for it). Mirrors the edge-lock probe.
  leftDrawer.setActionPopupProbe(() => isActionPopupOpen());

  function syncLeftEdgeLockColor() {
    if (leftEdgeLock) leftEdgeLock.classList.toggle('locked', leftDrawer.isLocked());
  }

  // Extend the stage mousemove handler to also drive the LEFT edge-lock bar.
  // (The existing onStageMouseMove above handles the right edge; this branch
  // mirrors it for the left edge.)
  function onStageMouseMoveLeft(e) {
    const rect = root.getBoundingClientRect();
    const fromLeft = e.clientX - rect.left;
    if (leftEdgeLock) {
      const show = fromLeft <= EDGE_HIT_PX && leftDrawer.isOpenState();
      leftEdgeLock.classList.toggle('visible', show);
    }
  }
  on(root, 'mousemove', onStageMouseMoveLeft);

  on(leftEdgeLock, 'click', (e) => {
    e.stopPropagation();
    leftDrawer.toggleLock();
    syncLeftEdgeLockColor();
  });

  // ── Edge-lock "stuck visible" guard (Chloe 2026-08-06) ────────────────
  // The edge-lock bars are shown/hidden ONLY by onStageMouseMove (it toggles
  // `.visible` based on the pointer's distance from the screen edge). That
  // handler only runs while the mouse is MOVING over the stage. Three cases
  // leave a bar stuck `.visible` with no further mousemove to clear it:
  //   1. The pointer leaves the window entirely (alt-tab, clicking another
  //      monitor, a fullscreen hand-off). The last mousemove armed the bar;
  //      nothing fires to disarm it.
  //   2. A viewport RESIZE — fullscreen toggle, display-resolution change,
  //      DPR shift, dock auto-hide. The stage rect changes size, so the
  //      pointer's stored screen position is suddenly "at the edge" relative
  //      to the NEW (smaller) rect, but no mousemove arrives to recompute.
  //   3. The window loses focus (blur) mid-hover.
  // A stuck `.visible` edge lock permanently suppresses the drawer's
  // mouseleave auto-close (onDrawerMouseLeave's `edgeLockVisible()` probe
  // returns true forever) → the drawer is stuck OPEN, which the user
  // experiences as "the right drawer stays infinitely open / the left drawer
  // breaks / the text box looks pushed left" (the open drawer overlaps the
  // centered input). Toggling the lock doesn't help because the bar is still
  // `.visible`; only a hard refresh cleared it.
  // FIX: dismiss BOTH edge-lock bars whenever the pointer leaves the window,
  // the window blurs, or the viewport resizes — AND close any un-locked
  // drawer that was held open only by the now-cleared lock (its mouseleave
  // auto-close was suppressed by the stale `.visible` state, so without this
  // it would stay open until the next manual interaction). A resize while
  // genuinely hovering the edge re-arms the bar on the very next mousemove
  // (the mousemove handler is authoritative for the visible state), so this
  // only ever clears STALE state — never a live hover. Tracked via on() so it
  // tears down with the stage.
  function dismissStaleEdgeLocks() {
    if (wupiEdgeLock) wupiEdgeLock.classList.remove('visible');
    if (leftEdgeLock) leftEdgeLock.classList.remove('visible');
    // Close any drawer held open only by the stale lock. Locked drawers stay
    // (the user pinned them deliberately); generating drawers stay (don't
    // yank mid-stream). This mirrors onDrawerMouseLeave's own guards.
    if (wupiDrawer.isOpen() && !wupiDrawer.isLocked() && !wupiDrawer.isGenerating()) {
      wupiDrawer.closeDrawer();
    }
    if (leftDrawer.isOpenState() && !leftDrawer.isLocked()) {
      // (2026-08-15 audit fix) An open inventory action popup (mid-EQUIP
      // destination sub-menu) survives the alt-tab/blur: force-closing the
      // drawer here killed the in-progress item action. Mirrors
      // onDrawerMouseLeave's own actionPopupOpen guard.
      if (!isActionPopupOpen()) leftDrawer.closeDrawer();
    }
  }
  // mouseout on document with no relatedTarget = pointer left the viewport
  // (the robust cross-browser "mouse exited window" signal). window blur
  // covers the focus-loss case.
  on(document, 'mouseout', (e) => {
    if (!e.relatedTarget && !e.toElement) dismissStaleEdgeLocks();
  });
  on(window, 'blur', dismissStaleEdgeLocks);
  on(window, 'resize', dismissStaleEdgeLocks);
  // visibilitychange (tab hidden / window minimized via Win+D) covers cases
  // resize+blur miss on some Windows builds.
  on(document, 'visibilitychange', () => {
    if (document.hidden) dismissStaleEdgeLocks();
  });

  // Drawer footer actions. Three ICON buttons (no worded labels):
  //   💾 Save → opens the center save modal (name input + Quick Save / Save)
  //   📂 Load → fires the onLoad hook (save picker, future); closes drawer
  //   🏠 Home → fires the onExit hook (return to title); closes drawer
  // The onExit/onLoad hooks come from wireStage's `hooks` arg (set by
  // fable.js), NOT cardContext — cardContext is null today (no card loaded),
  // so the prior cardContext.onExit path was a no-op (the Home bug).
  const onExitHook = hooks.onExit || null;
  const onLoadHook = hooks.onLoad || (cardContext && cardContext.onLoad) || null;

  const saveOverlay = root.querySelector('[data-save-overlay]');
  const saveNameInput = root.querySelector('[data-save-name-input]');
  const footBg = root.querySelector('[data-foot-bg]');
  const footSave = root.querySelector('[data-foot-save]');
  const footLoad = root.querySelector('[data-foot-load]');
  const footApi = root.querySelector('[data-foot-api]');
  const footHome = root.querySelector('[data-foot-home]');

  // (P2c, 2026-08-17 E4B shakedown) Foot-button cooldown: every foot action
  // closes the Wupi drawer, and the close ANIMATION shifts the footer's
  // layout — a click landing at STALE coordinates mid-slide fired the WRONG
  // foot action (the playtest's save-modal → ✕ → stale-coordinate Load click
  // ended in a screenless limbo). Disable the whole footer for the close
  // window — the gems' 350ms `animating` pattern; disabled buttons swallow
  // clicks natively.
  const FOOT_COOLDOWN_MS = 350;
  let footCooldownTimer = null;
  function armFootCooldown() {
    for (const b of [footBg, footSave, footLoad, footApi, footHome]) {
      if (b) b.disabled = true;
    }
    if (footCooldownTimer) clearTimeout(footCooldownTimer);
    footCooldownTimer = setTimeout(() => {
      for (const b of [footBg, footSave, footLoad, footApi, footHome]) {
        if (b) b.disabled = false;
      }
      footCooldownTimer = null;
    }, FOOT_COOLDOWN_MS);
  }

  // Save icon → open the modal + focus the name input.
  on(footSave, 'click', () => {
    if (footSave.disabled) return;
    armFootCooldown();
    if (!saveOverlay) return;
    wupiDrawer.closeDrawer();
    saveOverlay.hidden = false;
    // Focus the input after the un-hide frame so the user can type immediately.
    setTimeout(() => saveNameInput && saveNameInput.focus(), 30);
  });
  // Quick Save (autosave slot, no name). The closed-modal guard eats
  // programmatic .click()s on a hidden overlay (Enter auto-repeat in the
  // name field re-fires the handler); doSave's in-flight latch coalesces
  // any remaining double-fire into one write.
  on(root.querySelector('[data-save-quick]'), 'click', () => {
    if (saveOverlay && saveOverlay.hidden) return;
    doSave('autosave', 'Autosave', 'Quick saved.');
    closeSaveModal();
  });
  // Named Save (timestamped slot with the typed name; falls back to autosave
  // when the name is blank — same behavior as Quick Save in that case).
  on(root.querySelector('[data-save-named]'), 'click', () => {
    if (saveOverlay && saveOverlay.hidden) return;
    const name = saveNameInput.value.trim();
    if (name) {
      doSave(String(Date.now()), name, 'Saved "' + name + '".');
    } else {
      doSave('autosave', 'Autosave', 'Quick saved.');
    }
    closeSaveModal();
  });
  // Enter in the name field submits a named save.
  on(saveNameInput, 'keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); root.querySelector('[data-save-named]').click(); }
  });
  // Close the modal: ✕ button, backdrop click, or Esc (Esc handled in onKeyDown).
  on(root.querySelector('[data-save-close]'), 'click', closeSaveModal);
  on(root.querySelector('[data-save-backdrop]'), 'click', closeSaveModal);

  function closeSaveModal() {
    if (!saveOverlay) return;
    saveOverlay.hidden = true;
    if (saveNameInput) saveNameInput.value = '';
  }
  // Expose closeSaveModal to onKeyDown's Esc handler via a module ref.
  saveModalClose = closeSaveModal;

  // ── Cascade-delete confirm modal (2026-08-19, Chloe) ──────────────
  // Opened by the feed delete handler when the doomed run is 2+ messages.
  // Confirm fires narrator.deleteMessage(index, { cascade: true }); every
  // dismiss path (Cancel / backdrop / Esc) just closes. The cancel button
  // takes focus on open so an accidental Enter can never confirm a delete.
  const deleteOverlay = root.querySelector('[data-delete-overlay]');
  function closeDeleteModal() {
    if (deleteOverlay) deleteOverlay.hidden = true;
    deleteConfirmIndex = -1;
  }
  function openDeleteConfirm(index, count) {
    if (!deleteOverlay) {
      // Modal missing from the template (defensive) — delete directly
      // rather than stranding the armed click as a no-op.
      narrator.deleteMessage(index, { cascade: true });
      return;
    }
    deleteConfirmIndex = index;
    const textEl = deleteOverlay.querySelector('[data-delete-text]');
    if (textEl) {
      textEl.textContent =
        `You are going to delete ${count} messages, are you sure?`;
    }
    deleteOverlay.hidden = false;
    const cancelBtn = deleteOverlay.querySelector('[data-delete-cancel]');
    setTimeout(() => cancelBtn && cancelBtn.focus(), 30);
  }
  deleteModalClose = closeDeleteModal;
  on(root.querySelector('[data-delete-cancel]'), 'click', closeDeleteModal);
  on(root.querySelector('[data-delete-backdrop]'), 'click', closeDeleteModal);
  on(root.querySelector('[data-delete-confirm]'), 'click', () => {
    if (deleteOverlay && deleteOverlay.hidden) return;
    const idx = deleteConfirmIndex;
    closeDeleteModal();
    if (idx >= 0) narrator.deleteMessage(idx, { cascade: true });
  });

  on(footLoad, 'click', () => {
    if (footLoad.disabled) return;
    armFootCooldown();
    wupiDrawer.closeDrawer();
    if (onLoadHook) onLoadHook();
  });
  // Home double-click latch: the exit path is async (the title transition) —
  // a second click mid-transition must not re-fire it. Closure-scoped so a
  // fresh session's wireStage resets it.
  let homeFired = false;
  on(footHome, 'click', () => {
    if (footHome.disabled || homeFired) return;
    homeFired = true;
    armFootCooldown();
    wupiDrawer.closeDrawer();
    if (onExitHook) onExitHook();
  });
  // Background Library (2026-08-11): open the gallery modal. Close the drawer
  // first so the modal isn't half-covered by the drawer's slide-out. The modal
  // is stage-appended (z:46, above the drawer's z:40) + Esc/backdrop-dismiss.
  on(footBg, 'click', () => {
    if (footBg.disabled) return;
    armFootCooldown();
    wupiDrawer.closeDrawer();
    backgrounds.openBackgroundsPanel(root);
  });
  // API foot button (2026-08-14): open the in-Fable API connection window (the
  // same panel the title's ONLINE button opens). Close the drawer first (the
  // popup is body-mounted above everything; an open drawer would sit under
  // its backdrop). Connecting from here is ALSO the recovery path for a
  // composer locked by a mid-session api_lost — see onStageApiChanged.
  on(footApi, 'click', () => {
    if (footApi.disabled) return;
    armFootCooldown();
    wupiDrawer.closeDrawer();
    openStageOnlinePanel();
  });

  // Esc: dismiss panel → close wupi.
  // (2026-08-15 audit fix) Routed through on() (capture preserved) — the raw
  // document.addEventListener re-ran on every stage entry and NEVER tore
  // down, so N sessions meant N Esc handlers firing per keypress.
  on(document, 'keydown', onKeyDown, { capture: true });

  // Paint the dialogue feed on entry. Two paths feed it:
  //   - hooks.loadMessages: the session's full history (the backend already
  //     holds it; we mirror it into the DOM). This is authoritative — when
  //     present it IS the feed. (2026-08-16) the opening beat rides here too:
  //     a fresh game's `<intro>` is seeded as session message 0 backend-side,
  //     so index 0 in the feed is index 0 in the conversation and the beat
  //     survives rebuilds/resumes like any other message.
  //   - hooks.openingScene: legacy fallback for a backend that still surfaces
  //     `FableLoadResult.intro` (rendered DOM-only). Dead under the current
  //     backend — kept as a one-shot guard.
  // A cold start has neither — the first fable_send turn
  // streams the opening beat live.
  beats.clearFeed();
  if (Array.isArray(hooks.loadMessages) && hooks.loadMessages.length) {
    beats.rebuildFromMessages(hooks.loadMessages);
  } else if (typeof hooks.openingScene === 'string' && hooks.openingScene.trim()) {
    const opening = beats.startNarratorBeat({ name: activeCardName });
    beats.finalizeBeat(opening, hooks.openingScene);
  }
  beats.scrollDown();

  // §11.30 Left-Drawer HUD: hydrate immediately on stage entry. Without this
  // the HUD (clock / weather / location / paperdoll heatsink) stays stale
  // until the first narrator turn finalizes (onTurnEnd at line ~267) or the
  // user opens the left drawer (armLeftCornerDwell at line ~864). On a
  // Continue/Load resume this meant a freshly-loaded save showed default
  // values for a beat even though the persisted schema carried real state.
  // Best-effort + non-blocking: refreshAll swallows IPC failures internally.
  leftDrawer.refreshAll();
  // (2026-08-15 audit fix) Re-mount the Soul Gems on every stage entry: the
  // prior exit's resetLeftDrawer → clearSoulGems removed the overlay + panel
  // slot (the anti-bleed guarantee) and nulled soul-gem.js's refs, so without
  // a rebuild here toggleSoulGems() no-ops from game session 2 on. Idempotent:
  // an existing overlay returns early (leftDrawerEl is resolved above, line
  // ~744, the same element refreshAll/resetLeftDrawer act on).
  leftDrawer.mountSoulGems(leftDrawerEl);
  // (P2a, 2026-08-17 E4B shakedown) Post-layout recompute on EVERY stage
  // entry path (they all funnel through wireStage: title Continue resume,
  // Load, New Game, the direct-launch --card/--save boot). The immediate
  // measure inside buildSoulGems can land mid-layout — the save→title→
  // Continue resume stamped the whole gem cluster at negative x. The ladder
  // re-measures at settled frames, degenerate-guarded with last-good
  // fallbacks inside soul-gem.js.
  try { soulGems.scheduleSoulGemReposition(); } catch (e) {
    console.warn('[stage] soul-gem entry recompute failed:', e);
  }

  // Background Library (2026-08-11): paint the saved active background (if any)
  // onto .fable-stage-bg on entry. Best-effort + non-blocking — a fetch failure
  // leaves the stage at its default black void. Mirrors refreshAll's tolerance.
  void backgrounds.applyBackground(root);

  // Ambient music removed in Fable asset wipe (Phase 0a). Music module deleted;
  // §2A "ambient title music" will be re-sourced when audio assets are re-added.
}

// Auto-grow the composer to fit content, capped at 4 lines (Chloe
// 2026-08-02: "automatically size the box as you type in more lines with
// a max limit of 4 lines"). Beyond 4 lines the field scrolls internally
// (the scrollbar is hidden via CSS — still scrollable, no visual handle).
// Cap the auto-grow at exactly 2 rendered lines. Computed from the textarea's
// LIVE computed style (font-size × line-height) + its vertical padding so the
// cap tracks the real metrics instead of drifting on a magic pixel constant.
// (Was 4 lines; capped at 2 lines 2026-08-14 per Chloe.)
const INPUT_MAX_LINES = 2;
function inputMaxHeightPx(el) {
  const cs = getComputedStyle(el);
  const lineHeight = parseFloat(cs.lineHeight) || (parseFloat(cs.fontSize) * 1.5);
  const padTop = parseFloat(cs.paddingTop) || 0;
  const padBottom = parseFloat(cs.paddingBottom) || 0;
  return Math.ceil(lineHeight * INPUT_MAX_LINES + padTop + padBottom);
}
function autoGrow(el) {
  el.style.height = 'auto';
  const max = inputMaxHeightPx(el);
  // Cap at the 2-line height; if content exceeds it, CLAMP to the cap and let
  // the textarea scroll internally (overflow flipped to auto in CSS once over).
  // Setting exactly `max` (not scrollHeight) means the box never grows to show
  // a 3rd line peeking — the 3rd line lives below the fold, scrollable but
  // invisible until the user scrolls.
  const clamped = Math.min(el.scrollHeight, max);
  el.style.height = clamped + 'px';
  // Once content overflows the 2-line cap, switch to auto overflow so the
  // cursor can still navigate the hidden 3rd+ lines; otherwise hidden so no
  // half-line peeks at the bottom edge.
  el.style.overflow = el.scrollHeight > max + 1 ? 'auto' : 'hidden';
}

// Track a stage-element listener so teardownStage can remove it (prevents
// the double-bind on re-wireStage — see stageListeners comment above).
// Use for any element whose DOM persists across stage entries.
function on(el, type, handler, opts) {
  el.addEventListener(type, handler, opts);
  stageListeners.push([el, type, handler, opts && opts.capture]);
}

// (2026-08-15 audit fix) wireStage epoch: bumped on every wireStage ENTRY and
// in teardownStage. After each internal await, wireStage checks its captured
// epoch — a fast Home during the card-name IPC tears the stage down (or a
// re-entry supersedes this wiring) while the async fn is suspended; without
// the guard, the suspended run CONTINUES registering listeners (the document
// keydown among them) onto a torn-down stage — an untracked leak that
// accumulated one document listener per aborted entry.
let wireEpoch = 0;

// The typing indicator's label (2026-08-19, subtype-aware): an npc-subtype
// card IS the character, so the card name reads as them typing; a scenario/
// world card narrates through an unseen voice → "Narrator is currently
// thinking...". Unknown/legacy subtypes (no <subtype> in the file, the
// legacy plain-string IPC shape, dev preview) fall into the npc lane — the
// named-persona reading. "Game Master" is the no-name fallback.
function typingLabel() {
  if (activeCardSubtype === 'scenario' || activeCardSubtype === 'world') {
    return 'Narrator is currently thinking...';
  }
  const who = activeCardName || 'Game Master';
  return `${who} is currently typing...`;
}

// Generation state reflection (2026-07-27, no send button): the send/stop
// toggle button + its SVG icons are GONE. The UI feedback for a turn in
// flight is the typing indicator above the input row (subtype-aware label,
// 2026-08-19) + the in-card streaming caret (the .streaming class on the
// narrator beat, see fable.css). The input stays ENABLED so the empty-Enter-
// to-stop affordance (wired above in wireStage) works: pressing Enter on an
// empty field while generating stops the turn. The placeholder flips to hint
// the stop affordance so the user learns the gesture.
function setGenerating(on) {
  const input = stageRoot && stageRoot.querySelector('[data-input]');
  if (!input) return;
  // The typing indicator: label text is computed at SHOW time (the card
  // identity is cached by then; a mid-teardown call just hides). Shown for
  // the WHOLE turn — the tracker stage before the first chunk has no other
  // in-flight signal.
  const indicator = stageRoot.querySelector('[data-typing-indicator]');
  if (indicator) {
    if (on) {
      const label = stageRoot.querySelector('[data-typing-text]');
      if (label) label.textContent = typingLabel();
      indicator.classList.add('is-visible');
    } else {
      indicator.classList.remove('is-visible');
    }
  }
  if (on) {
    // Idempotent stash: reroll / rewind-edit fire onTurnStart, then
    // sendFableTurn fires it AGAIN — the second call must not stash the
    // BUSY placeholder over the idle one (else the composer forever reads
    // "Press Enter to stop…" after the turn).
    if (input.dataset.idlePlaceholder == null) {
      input.dataset.idlePlaceholder = input.placeholder;
    }
    input.placeholder = 'Press Enter to stop…';
  } else {
    // A locked composer (mid-turn api_lost) keeps its red lock message —
    // the stash restore must not clobber it; unlockComposer resets the
    // placeholder itself.
    if (composerLocked) {
      delete input.dataset.idlePlaceholder;
      return;
    }
    if (input.dataset.idlePlaceholder != null) {
      input.placeholder = input.dataset.idlePlaceholder;
      delete input.dataset.idlePlaceholder;
    }
    input.focus();
  }
}

// Lock the narrator composer when the API dies mid-session (2026-08-07). The
// input greys out + a red glowing "API LOST CONNECTION" message replaces the
// placeholder. The user is told to reconnect via Settings; pressing Enter
// probes the connection (retryIfApiReady) and re-sends the pending turn if the
// API is back. The Wupi-drawer chat (local model) is unaffected — only the
// narrator composer locks.
function lockComposer(message) {
  const inputRow = stageRoot && stageRoot.querySelector('[data-input-form]');
  const input = stageRoot && stageRoot.querySelector('[data-input]');
  if (!inputRow || !input) return;
  // Stash the text the player was trying to send so a retry re-sends it.
  // (2026-08-15 audit fix) The composer is CLEARED at submit, so mid-turn
  // input.value is EMPTY — fall back to the in-flight turn's text stashed at
  // submit (lastSentTurnText). The backend already aborted the turn WITHOUT
  // consuming the user message (the api_lost path pops the turn-start user
  // message), so the retry sends a fresh, unduplicated copy.
  // (2026-08-16 audit M4) PREFER the in-flight action over a half-typed
  // draft: lockComposer fires mid-turn, where input.value is whatever the
  // player started typing NEXT — the old `input.value ||` order let that
  // draft beat the lost action, which then existed nowhere. Keep any prior
  // stash as the last resort (never clobber with empty).
  pendingTurnText = lastSentTurnText || (input.value && input.value.trim()) || pendingTurnText;
  input.value = '';
  input.style.height = 'auto';
  // (2026-08-16 audit fix #25) readOnly, NOT disabled: a disabled textarea
  // receives NO keydown in WebView2, so the Enter-to-retry affordance the
  // toast advertises could never fire (the lock was a dead end until the API
  // panel reconnect path unlocked it). readOnly blocks typing but keeps the
  // keydown path alive — the composer's Enter handler routes composerLocked
  // to retryIfApiReady. The .is-api-lost class carries the visual lock.
  input.readOnly = true;
  // The red glowing placeholder IS the in-box error message.
  input.placeholder = 'API LOST CONNECTION';
  inputRow.classList.add('is-api-lost');
  composerLocked = true;
  toast(message || 'API connection lost — reconnect via Settings, then press Enter to retry.');
}

function unlockComposer() {
  const inputRow = stageRoot && stageRoot.querySelector('[data-input-form]');
  const input = stageRoot && stageRoot.querySelector('[data-input]');
  if (!inputRow || !input) return;
  inputRow.classList.remove('is-api-lost');
  input.readOnly = false;
  // Restore the original idle placeholder (setGenerating stashes it, but the
  // lock overwrote it directly — reset to the default).
  input.placeholder = 'Type a message...';
  composerLocked = false;
  input.focus();
}

// Probes whether the API is back; if so, unlocks + re-sends the pending turn.
// Called when the player presses Enter on the locked composer. A failure or a
// still-down API re-toasts so the player knows to keep trying.
async function retryIfApiReady() {
  let extra;
  try {
    extra = await invoke('model_source_get');
  } catch (e) {
    toast('Still no API connection — check Settings.');
    return;
  }
  const apiReady = !!(extra && extra.source === 'api' && extra.apiReady);
  if (!apiReady) {
    toast('Still no API connection — reconnect in Settings, then press Enter.');
    return;
  }
  const text = pendingTurnText;
  pendingTurnText = '';
  unlockComposer();
  if (text && !narrator.isGenerating()) {
    // (2026-08-16 audit M4) The retry re-sends through sendFableTurn
    // DIRECTLY — the submit handler that sets lastSentTurnText is bypassed,
    // so a SECOND consecutive api_lost would find it cleared and lose the
    // action for good. Set it here like a normal submit would.
    lastSentTurnText = text;
    narrator.sendFableTurn(text);
  }
}

// ── In-stage API connection window (2026-08-14) ──────────────────────────
// The Wupi drawer's API foot button opens the SAME panel the title's ONLINE
// button opens (buildOnlinePanel — body-mounted above every stage surface,
// self-contained ✕/Esc/backdrop close). Mounted with the same ritual as
// fable.js's openOnlinePanel: singleton guard + append + reflow + .is-open.
// The title needs no refresh ping from here — _startAmbient re-runs
// _refreshTitleGate on every title show, so the gate reflects whatever API
// state the player left behind when they return Home.
function openStageOnlinePanel() {
  if (document.querySelector('.fable-online-popup')) return;  // already open
  const panel = buildOnlinePanel({ onChanged: onStageApiChanged });
  document.body.appendChild(panel);
  // Force a reflow so the opacity transition runs (mounts at 0).
  void panel.offsetWidth;
  panel.classList.add('is-open');
}

// The panel's onChanged (fires on connect/disconnect/profile edits + close).
// Stage-side concern only: if the narrator composer is locked by a mid-session
// api_lost and an API is (re)connected through the panel, unlock it + restore
// the stashed pending turn into the box so the player can resume with one
// Enter. No-op otherwise (probe-gated; profile edits while disconnected
// change nothing).
async function onStageApiChanged() {
  if (!composerLocked) return;
  let extra = null;
  try {
    extra = await invoke('model_source_get');
  } catch (_) {
    return;  // IPC failure: leave the lock alone (Enter-probe still works)
  }
  if (!(extra && extra.source === 'api' && extra.apiReady)) return;
  const text = pendingTurnText;
  pendingTurnText = '';
  unlockComposer();
  if (text) {
    const input = stageRoot && stageRoot.querySelector('[data-input]');
    if (input) {
      input.value = text;
      input.style.height = 'auto';
    }
  }
}

function onKeyDown(e) {
  if (e.key !== 'Escape') return;
  // Online/API popup FIRST: it's body-mounted above every stage surface
  // (z:100100), so while it's open it owns Esc — its own document-level
  // listener (bubble phase, after this capture one) closes it. Bail without
  // acting so the stage doesn't close a surface UNDERNEATH the popup (e.g.
  // the left drawer) on the same keypress.
  if (document.querySelector('.fable-online-popup')) return;
  // Priority order: raw editor → save modal → modal panel → wupi drawer →
  // left drawer. Innermost surface dismisses first so a player with stacked
  // surfaces can Esc them one at a time. The raw editor is highest (it darkens
  // the full stage + its validation lock means Esc refuses on invalid — only
  // ↻ or ✕ escape, so Esc on invalid just flashes the hint, not a close).
  // (2026-08-15 audit fix) Every HANDLED branch also stopPropagation: this is
  // a capture-phase listener, so without it the same keypress reaches the
  // inline editor's own Esc handler (beats.js enterEditMode) on the bubble
  // and discards the typed edit — one Esc closed TWO surfaces. When no
  // surface was open the event propagates untouched (nothing handled it).
  if (rawEditorOpen() && rawEditorEsc()) { e.preventDefault(); e.stopPropagation(); return; }
  if (saveModalClose) {
    const overlay = stageRoot && stageRoot.querySelector('[data-save-overlay]');
    if (overlay && !overlay.hidden) { saveModalClose(); e.preventDefault(); e.stopPropagation(); return; }
  }
  // (2026-08-19) The cascade-delete confirm sits at the save modal's
  // z-tier — Esc dismisses it (as a cancel) right after the save modal.
  if (deleteModalClose) {
    const overlay = stageRoot && stageRoot.querySelector('[data-delete-overlay]');
    if (overlay && !overlay.hidden) { deleteModalClose(); e.preventDefault(); e.stopPropagation(); return; }
  }
  // Backgrounds gallery modal — same z-tier as the save modal, so it dismisses
  // right after it (before the drawer/panel surfaces below).
  if (backgrounds.isOpen()) { backgrounds.closeBackgroundsPanel(); e.preventDefault(); e.stopPropagation(); return; }
  if (panelActive()) { dismissPanel(); e.preventDefault(); e.stopPropagation(); return; }
  // (2026-08-16 audit fix #26) The inline beat editor + an in-flight slice
  // regen own Esc BEFORE any drawer: this listener runs CAPTURE-phase, so a
  // drawer branch's stopPropagation used to eat the keypress while the
  // editor stayed open — the drawer (an outer surface) dismissed first.
  // Both surfaces handle Esc on their own listeners (the editor's textarea
  // keydown; slice-regen's transient capture listener) — just don't act.
  if (beats.openEditingBeat()) return;
  if (narrator.isSliceRegenerating()) return;
  if (wupiDrawer.isOpen()) { wupiDrawer.closeDrawer(); e.preventDefault(); e.stopPropagation(); return; }
  if (leftDrawer.isOpenState()) { leftDrawer.closeDrawer(); e.preventDefault(); e.stopPropagation(); return; }
}

// Hover-corner dwell (decision 1: no visible button). Arm a 300ms timer
// on mouseenter; if the pointer is still in the zone when it fires, open
// the drawer. mouseleave cancels. This avoids accidental opens from a
// passing pointer while keeping the trigger invisible + low-friction.
function armCornerDwell() {
  if (cornerDwellTimer) clearTimeout(cornerDwellTimer);
  cornerDwellTimer = setTimeout(() => {
    cornerDwellTimer = null;
    wupiDrawer.openDrawer();
    // The rail RESETS on drawer close (resetTabRail nulls the active tab),
    // so there is no "keep the prior tab" on reopen — the open path
    // re-selects a tab itself. renderActive is the backstop for a tab that
    // re-selection leaves rendered-but-stale (e.g. data changed mid-close).
    renderActive();
  }, CORNER_DWELL_MS);
}
function cancelCornerDwell() {
  if (cornerDwellTimer) { clearTimeout(cornerDwellTimer); cornerDwellTimer = null; }
}

// Left-edge dwell — exact mirror of armCornerDwell / cancelCornerDwell above
// but for the left (Card / Tracker) drawer.
function armLeftCornerDwell() {
  if (leftCornerDwellTimer) clearTimeout(leftCornerDwellTimer);
  leftCornerDwellTimer = setTimeout(() => {
    leftCornerDwellTimer = null;
    leftDrawer.openDrawer();
    // §11.30: refresh the HUD on open so the paperdoll/ambient/inventory
    // reflect the latest state (mirrors the right rail's renderActive() on
    // reopen — the drawer's content may be stale from a prior session).
    leftDrawer.refreshAll();
  }, CORNER_DWELL_MS);
}
function cancelLeftCornerDwell() {
  if (leftCornerDwellTimer) { clearTimeout(leftCornerDwellTimer); leftCornerDwellTimer = null; }
}

let saveInFlight = false;
async function doSave(saveId, name, msg) {
  if (saveInFlight) return; // a double-fired save coalesces into the first
  saveInFlight = true;
  try {
    await saveNow(saveId, name);
    toast(msg || 'Saved.');
  } catch (err) {
    toast('Save failed: ' + err);
  } finally {
    saveInFlight = false;
  }
}

export function toast(msg) {
  // (2026-08-15 audit fix) toast can fire before any stage entry (e.g. a New
  // Game import failure in fable.js calls it while stageRoot is still null —
  // wireStage hasn't run on a fresh boot). Guard instead of throwing.
  // (2026-08-16 audit fix #26) The stage must also be the VISIBLE screen:
  // showScreen toggles each screen's `hidden` property, so a stale stageRoot
  // from a prior entry painted the toast into the hidden stage DOM — the
  // flow-level failure the caller wanted to surface never showed anywhere.
  // Non-stage surfaces drop to console so the error at least lands somewhere
  // observable.
  if (!stageRoot || stageRoot.hidden) { console.warn('[stage] toast (stage not visible):', msg); return; }
  const t = stageRoot.querySelector('[data-toast]');
  if (!t) return;
  t.textContent = msg;
  t.hidden = false;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 2200);
}

// (2026-08-19 Chloe) Bottom warning popup for IMPORT failures: a fixed,
// document-level notice so it shows on ANY flow screen (the stage toast above
// is stage-gated and invisible during the New Game pickers — an unrecognized
// import previously failed silently or fell into a chat window). Auto-dismisses.
let importWarnTimer = null;
export function bottomWarning(msg) {
  let el = document.querySelector('.fable-import-warn');
  if (!el) {
    el = document.createElement('div');
    el.className = 'fable-import-warn';
    el.setAttribute('role', 'alert');
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add('is-open');
  if (importWarnTimer) clearTimeout(importWarnTimer);
  importWarnTimer = setTimeout(() => { el.classList.remove('is-open'); }, 3600);
}

// Populate the feed from a load result (messages: [{role, content, variants?,
// active_idx?, timestamp?}]). Forwards straight to beats.rebuildFromMessages,
// which is the single source of truth for the feed DOM.
export function loadHistory(messages) {
  beats.rebuildFromMessages(messages);
  beats.scrollDown();
}

// ── Typing indicator + card identity ────────────────────────────────────
// The subtype-aware typing label ("(Name) is currently typing..." for npc
// cards / "Narrator is currently thinking..." for scenario+world — see
// typingLabel) pinned above the input row while a turn is in flight. The
// card identity (name + subtype + player_name) is fetched once on stage
// entry (best-effort) + cached in `activeCardName`/`activeCardSubtype`
// (the display voice for the typing label) + stashed in `activePlayerName`
// so the initNarrator call (which runs right after this in wireStage) can
// forward both into the beats builders for the message headers. Generic
// fallback when no name.
//
// DEFENSIVE DUAL-SHAPE: `fable_active_card_get` returns a plain card-name
// string today; a parallel Rust change widens it to `{name, player_name}`.
// We accept BOTH shapes so this UI works whether or not that Rust edit has
// landed yet (a string → name only, player_name stays ''; an object → both).
async function refreshActiveCardName(root) {
  // DEV PREVIEW: skip the IPC entirely — inject the placeholder identities
  // directly. The preview has no backend, so fable_active_card_get would
  // fail/return empty. The AI/narrator identity is "Game Master" (the default
  // narrator), the player is "Wanderer". Portraits stay '' — the renderer
  // (beats.buildMes) now fills an empty portrait with a sleek silhouette
  // placeholder, so dev-preview shows silhouettes on both sides. NPCs fall
  // back to the AI portrait (per the deferred-NPC-portrait rule).
  if (DEV_PREVIEW) {
    activeCardName = 'Game Master';
    activeCardSubtype = '';
    activePlayerName = 'Wanderer';
    activeCardPortrait = '';
    activePlayerPortrait = '';
    npcNameMap = new Map();
    return;
  }
  try {
    const res = await invoke('fable_active_card_get');
    if (typeof res === 'string') {
      // Legacy plain-string shape (older backend): just the card name.
      activeCardName = res;
      activeCardSubtype = '';
      activePlayerName = '';
      activeCardPortrait = '';
      activePlayerPortrait = '';
      npcNameMap = new Map();
    } else if (res && typeof res === 'object') {
      activeCardName = typeof res.name === 'string' ? res.name : '';
      // The wizard subtype ("npc" | "scenario" | "world") — drives the typing
      // label's voice. Null/absent (legacy card) reads as the npc lane.
      activeCardSubtype = typeof res.subtype === 'string' ? res.subtype : '';
      activePlayerName = typeof res.player_name === 'string' ? res.player_name : '';
      // Portrait paths arrive as absolute filesystem paths; convertFileSrc
      // mints the asset:// URL the webview can <img src>. Empty when absent.
      activeCardPortrait = typeof res.card_portrait_url === 'string' && res.card_portrait_url
        ? convertFileSrc(res.card_portrait_url) : '';
      activePlayerPortrait = typeof res.player_portrait_url === 'string' && res.player_portrait_url
        ? convertFileSrc(res.player_portrait_url) : '';
      // Build the npc_id → display-name map from the cast summary. Real NPC
      // speaker labels on character beats (replaces the slug-title fallback).
      npcNameMap = new Map();
      if (Array.isArray(res.npc_names)) {
        for (const n of res.npc_names) {
          if (n && typeof n.id === 'string' && typeof n.name === 'string') {
            npcNameMap.set(n.id, n.name);
          }
        }
      }
    } else {
      activeCardName = '';
      activeCardSubtype = '';
      activePlayerName = '';
      activeCardPortrait = '';
      activePlayerPortrait = '';
      npcNameMap = new Map();
    }
  } catch (_) {
    activeCardName = '';
    activeCardSubtype = '';
    activePlayerName = '';
    activeCardPortrait = '';
    activePlayerPortrait = '';
    npcNameMap = new Map();
  }
}

// Tear down on exit to title / window close. HARDENED (Chloe 2026-07-23:
// the resource-isolation audit) so the stage leaves ZERO residual state —
// re-entering Fable after a close must be byte-for-byte brand new:
//   - removes the global document keydown + ALL tracked stage-element
//     listeners (prevents the double-bind on re-wireStage),
//   - cancels the corner dwell timer + the toast timer,
//   - dismisses any open panel overlay (so a leftover panel can't make
//     Esc behave as if one's open on the next session),
//   - resets narrator + wupi-drawer module state (clears a stuck
//     `generating` flag from a close mid-turn, nulls dangling beat refs,
//     wipes the Wupi transcript + reseeds the greeting for next entry),
//   - tears down the FX timers + clears the dialogue feed.
// No RAF, no interval, no listener, no flag survives this.
export function teardownStage() {
  document.removeEventListener('keydown', onKeyDown, true);
  // Remove every tracked stage-element listener so re-wireStage binds
  // exactly once (the stage DOM is reused across entries).
  for (const [el, type, handler, capture] of stageListeners) {
    el.removeEventListener(type, handler, capture);
  }
  stageListeners = [];
  // (2026-08-15 audit fix) Invalidate any wireStage still suspended across its
  // card-name IPC await — its post-await epoch check stops it from registering
  // listeners onto this torn-down stage.
  wireEpoch++;
  cancelCornerDwell();
  cornerTrigger = null;
  // The LEFT edge dwell timer too (2026-08-15 audit fix): a dwell armed
  // <300ms before exit fired into the torn-down stage, and the next entry
  // could start with the left drawer stuck open.
  cancelLeftCornerDwell();
  leftCornerTrigger = null;
  saveModalClose = null;
  // (2026-08-19) Cascade-delete confirm state: a close mid-confirm must not
  // leave the modal ref alive (Esc on the next session would call a stale
  // closer) or a stale doomed index for the next session's Confirm click.
  deleteModalClose = null;
  deleteConfirmIndex = -1;
  // Reset the API-lost composer lock so a re-entry isn't stuck greyed out.
  composerLocked = false;
  pendingTurnText = '';
  lastSentTurnText = '';
  // Clear the toast timer so it can't fire into a torn-down element after
  // close (was a residual-state gap — harmless but not clean).
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }
  // Dismiss any open panel so panel-manager's `active` flag + the overlay
  // content don't persist into the next session.
  if (panelActive()) dismissPanel();
  clearAllFX();
  // Release the VN-interactions layer (listeners + observer + any vn-* state
  // classes on the feed + any snapped portraits) BEFORE the feed is wiped.
  // Disconnecting the observer first means the imminent clearFeed's
  // childList mutation doesn't fire a now-orphaned refreshHistory pass, +
  // the snapped-portrait cleanup runs while the <img> nodes still exist.
  if (vnApi) { vnApi.teardown(); vnApi = null; }
  // Release the golden-pencil selection layer (selectionchange/mouseup/keyup
  // listeners + the floating pencil element) before the feed is wiped.
  if (sliceApi) { sliceApi.teardown(); sliceApi = null; }
  // Wipe the dialogue feed so a prior session's cards don't leak into the
  // next session (the stage DOM is reused across games).
  beats.clearFeed();
  activeCardName = '';
  activeCardSubtype = '';
  // Hide the typing indicator: the epoch guard can block a mid-exit turn's
  // late finishTurn (→ setGenerating(false)), and the reused stage DOM would
  // otherwise carry the visible label into the next session's first paint.
  const staleIndicator = stageRoot && stageRoot.querySelector('[data-typing-indicator]');
  if (staleIndicator) staleIndicator.classList.remove('is-visible');
  // Reset the engine module state so a close mid-turn can't leave a stuck
  // `generating` (would no-op the next session's first send) or a dangling
  // activeBeat, and so the Wupi drawer starts fresh next entry.
  narrator.resetNarrator();
  // (2026-08-16 audit M8b) Cut an in-flight DRAWER chat decode too — the
  // local-model turn lock it holds would stall the next session's first
  // tracker turn. resetWupiDrawer only clears DOM state; the decode is
  // backend-side. Fire-and-forget: chat_stop races nothing (fable_end on
  // the next entry is chained after the stop in the backend's own paths,
  // and a turn finalizing into the swapped session is blocked by the
  // session-identity guards).
  wupiDrawer.stopWupiTurn();
  wupiDrawer.resetWupiDrawer();
  leftDrawer.resetLeftDrawer();
  // Reset the tab rail (clears the active tab so no stale dropdown leaks into
  // the next session) + the raw editor (closes it if open — a mid-edit exit
  // discards unsaved textarea edits to the last-good file, same protection as
  // Alt+F4). The raw editor's atomic-write backend means the file on disk is
  // always the last successfully-saved state regardless.
  resetTabRail();
  resetRawEditor();
  // Selection popup teardown: the golden-pencil slice-regen layer (sliceApi
  // above) owns its own document-level listeners + the floating pencil element
  // + releases them in its teardown(). The stageListeners loop above already
  // removes every tracked stage-element listener, so nothing else leaks.
}
