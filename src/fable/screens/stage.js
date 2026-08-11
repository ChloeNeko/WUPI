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
// removed. The stage is a pure black void for ALL games now — Quick Play
// lands here straight from the void interview, and the manual-card path no
// longer has a tavern card to source a bg from. The `.fable-stage-bg`
// element stays in the DOM as a no-op over the black .fable-stage so a
// future bg re-add is a one-line structural change. Explicit narrator
// [FX ...] brackets still drive fx/effects.js (a separate, opt-in path).
// =============================================================

import * as beats from '../engine/beats.js';
import * as narrator from '../engine/narrator.js';
import { swipeNextAction } from '../engine/drawer-logic.js';
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
import { initPanelManager, summon as summonPanel, dismissPanel, isActive as panelActive } from '../panels/manager.js';
import { setMapTheme } from '../panels/map.js';
// DEV PREVIEW placeholders (?dev=preview): static bundled portraits so the
// VN layout renders with real images without a backend session. Vite hashes
// these into dist/assets/ like every other static image in the project.
// (Dev-only sample art: mage.jpg for the narrator, player.jpg for the user.
// Production uses portraits resolved via fable_active_card_get / the saved
// player — these imports only flow into the DEV_PREVIEW branch below.)
import PLACEHOLDER_AI_PORTRAIT from '../assets/mage.jpg';
import PLACEHOLDER_PLAYER_PORTRAIT from '../assets/player.jpg';

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
let cardContext = null;  // { card, saveId } for the active session
let activeCardName = '';  // display name of the seated card (the typing indicator)
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
         row + drawers. -->
    <div class="fable-feed" data-feed></div>
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
           read-only prose panel; the dropdown collapses on drawer close +
           re-renders on reopen (the hover-reopen-keeps-active mechanic). -->
      <div class="fable-tab-rail-mount" data-tab-rail-mount></div>
      <div class="fable-wupi-messages" data-wupi-messages></div>
      <form class="fable-wupi-input-row" data-wupi-form>
        <textarea class="fable-wupi-input" data-wupi-input rows="1" placeholder="Ask Wupi anything…"></textarea>
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
               Opens the gallery modal over the stage. Visible in ALL modes
               (including Quick Play — backgrounds are global, unlike
               Save/Load which Quick Play hides). -->
          <button class="fable-foot-icon" data-foot-bg aria-label="Background" title="Background">
            <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="5" width="18" height="14" rx="2" fill="none" stroke="currentColor" stroke-width="1.6"/><circle cx="8.5" cy="10" r="1.6" fill="currentColor"/><path d="M3 16l4.5-4 3.5 3 4-5 6 6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round" stroke-linecap="round"/></svg>
          </button>
          <button class="fable-foot-icon" data-foot-save aria-label="Save" title="Save">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 3h11l3 3v15a0 0 0 0 1 0 0H5a0 0 0 0 1 0 0V3z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><rect x="8" y="3" width="7" height="5" rx="0.8" fill="none" stroke="currentColor" stroke-width="1.6"/><rect x="8" y="12" width="8" height="6" rx="0.6" fill="none" stroke="currentColor" stroke-width="1.4"/></svg>
          </button>
          <button class="fable-foot-icon" data-foot-load aria-label="Load" title="Load">
            <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M3 7a2 2 0 0 1 2-2h5l2 2h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M12 17V9M12 9l-3 3M12 9l3 3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></svg>
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
  beats.initBeats(root.querySelector('[data-feed]'));
  // VN INTERACTIONS: attach the behavior layer (history mask, snaps, flank
  // dblclick, dblclick-to-edit) to the same feed + stage. Initialized AFTER
  // beats.initBeats so the feedEl ref is live + any seed beats (opening scene
  // / loaded history) are present for the first refreshHistory pass. The
  // onEditBeat hook mirrors the ✎ control-button routing at the feed click
  // handler below (user beat → editMessage, assistant → rewind-and-edit) so
  // dblclick-on-prose opens the SAME editor the ✎ button does.
  vnApi = vn.init({
    stageRoot: root,
    feedEl: root.querySelector('[data-feed]'),
    onEditBeat: (beat) => {
      if (narrator.isGenerating()) return;
      const index = Number.parseInt(beat.dataset.index || '-1', 10);
      const isUser = beat.dataset.role === 'user';
      beats.enterEditMode(beat, {
        onSave: (text) => {
          // Player → rewind + branch + regen ("I changed what I did"); AI
          // beat → in-place prose tweak (no inference). Inverted from the
          // prior wiring (which called rewind_and_edit_user on assistant
          // beats — that errors: the command requires a user target).
          if (isUser) narrator.rewindAndEditUser(index, text);
          else narrator.editMessage(index, text);
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
  narrator.initNarrator({
    onTurnStart: () => {
      setGenerating(true);
    },
    onTurnEnd: () => {
      setGenerating(false);
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
  on(inputForm, 'submit', (e) => {
    e.preventDefault();
    const text = input.value.trim();
    // Block new turns while a narrator turn is in flight.
    if (!text || narrator.isGenerating()) return;
    input.value = '';
    input.style.height = 'auto';
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
        // Empty Enter mid-generation → stop. The seamless stop affordance.
        narrator.stopFableTurn();
        return;
      }
      inputForm.requestSubmit();
    }
  });
  on(input, 'input', () => autoGrow(input));
  setGenerating(false);

  // Per-beat UX controls: a single delegated click handler on the feed
  // routes the drawer's data-action buttons to the narrator API. The
  // backend (edit_message / rewind_and_edit_user / reroll_last_turn /
  // swipe_variant / delete_message) is the source of truth — the DOM is
  // regenerated from the returned messages[] where applicable. The drawer
  // (built by beats.renderMessageDrawer) lives in beats.js; this handler
  // only routes clicks. Regenerate is FOLDED INTO › (there is no separate
  // ↻ button): › at the last variant of the trailing assistant beat rerolls.
  // All actions except the two-step delete-confirm arm are blocked while a
  // turn is in flight (the ›-during-generation interrupt is wired later).
  // Tracked via on() so teardown removes it.
  const feed = root.querySelector('[data-feed]');
  on(feed, 'click', (e) => {
    const btn = e.target.closest('[data-action]');
    if (!btn) return;
    const beat = btn.closest('.fable-mes');
    if (!beat) return;
    const action = btn.dataset.action;
    const index = Number.parseInt(beat.dataset.index || '-1', 10);

    // Two-step inline delete confirm: first click arms a 3.5s window + morphs
    // the button to a check; a second click within it confirms. A timeout
    // (or any re-render) restores the default delete button. No native dialog.
    if (action === 'delete') {
      if (narrator.isGenerating()) return;
      btn.dataset.action = 'delete-confirm';
      btn.classList.add('is-confirming');
      btn.setAttribute('title', 'Click again to confirm delete');
      btn.innerHTML = '<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>';
      if (btn._deleteTimer) clearTimeout(btn._deleteTimer);
      btn._deleteTimer = setTimeout(() => { beats.renderMessageDrawer(beat); }, 3500);
      return;
    }
    if (action === 'delete-confirm') {
      if (btn._deleteTimer) { clearTimeout(btn._deleteTimer); btn._deleteTimer = null; }
      if (narrator.isGenerating()) return;
      narrator.deleteMessage(index);
      return;
    }

    // › during an in-flight REROLL: interrupt it + auto-reroll (Stage 3,
    // 2026-08-11). The drawer only enables › at the trailing assistant beat's
    // last variant; mid-reroll that's the beat being re-streamed, so re-pressing
    // › abandons this roll + starts a fresh one (cancel → revert schema to base
    // → reroll). A normal turn in flight is left alone (its streaming beat has
    // no drawer; › on an older beat mid-turn would reroll the wrong turn).
    if (action === 'swipe-next' && narrator.isGenerating()) {
      if (narrator.isRerolling()) narrator.interruptAndReroll();
      return;
    }

    // Everything below is blocked mid-generation.
    if (narrator.isGenerating()) return;

    if (action === 'swipe-prev') {
      const active = Number.parseInt(beat.dataset.variantActive || '0', 10);
      if (active <= 0) return;
      narrator.swipeVariant(index, active - 1);
    } else if (action === 'swipe-next') {
      const count = Number.parseInt(beat.dataset.variantCount || '1', 10);
      const active = Number.parseInt(beat.dataset.variantActive || '0', 10);
      const next = swipeNextAction({ count, active });
      if (next.kind === 'swipe') {
        narrator.swipeVariant(index, next.variantIdx);
      } else {
        // At the last variant → fold into Regenerate. The drawer only enables ›
        // here on the trailing assistant beat, so this is the reroll trigger.
        narrator.rerollLastTurn();
      }
    } else if (action === 'edit') {
      // Player message → rewind + branch + regen; AI beat → in-place edit.
      const isUser = beat.dataset.role === 'user';
      beats.enterEditMode(beat, {
        onSave: (text) => {
          if (isUser) narrator.rewindAndEditUser(index, text);
          else narrator.editMessage(index, text);
        },
      });
    }
  });

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
      leftDrawer.closeDrawer();
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
  const footHome = root.querySelector('[data-foot-home]');

  // Quick Play mode: the session auto-saves its single quicksave slot on
  // fable_end (Home/Exit) — there is no manual save list + no load list. Hide
  // the Save + Load footer buttons so the only affordance is Home (which
  // triggers the auto-quicksave). The narrator feed, Wupi drawer, and all UX
  // chat controls stay fully active.
  if (hooks.isQuickPlay) {
    if (footSave) footSave.hidden = true;
    if (footLoad) footLoad.hidden = true;
  }

  // Save icon → open the modal + focus the name input.
  on(footSave, 'click', () => {
    if (footSave.disabled) return;
    if (!saveOverlay) return;
    wupiDrawer.closeDrawer();
    saveOverlay.hidden = false;
    // Focus the input after the un-hide frame so the user can type immediately.
    setTimeout(() => saveNameInput && saveNameInput.focus(), 30);
  });
  // Quick Save (autosave slot, no name).
  on(root.querySelector('[data-save-quick]'), 'click', () => {
    doSave('autosave', 'Autosave', 'Quick saved.');
    closeSaveModal();
  });
  // Named Save (timestamped slot with the typed name; falls back to autosave
  // when the name is blank — same behavior as Quick Save in that case).
  on(root.querySelector('[data-save-named]'), 'click', () => {
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

  on(footLoad, 'click', () => {
    if (footLoad.disabled) return;
    wupiDrawer.closeDrawer();
    if (onLoadHook) onLoadHook();
  });
  on(footHome, 'click', () => {
    wupiDrawer.closeDrawer();
    if (onExitHook) onExitHook();
  });
  // Background Library (2026-08-11): open the gallery modal. Close the drawer
  // first so the modal isn't half-covered by the drawer's slide-out. The modal
  // is stage-appended (z:46, above the drawer's z:40) + Esc/backdrop-dismiss.
  on(footBg, 'click', () => {
    if (footBg.disabled) return;
    wupiDrawer.closeDrawer();
    backgrounds.openBackgroundsPanel(root);
  });

  // Esc: dismiss panel → close wupi.
  document.addEventListener('keydown', onKeyDown, true);

  // Paint the dialogue feed on entry. Two paths feed it:
  //   - hooks.loadMessages: a resumed/loaded session's full history
  //     (the backend already holds it; we mirror it into the DOM). This
  //     is authoritative — when present it IS the feed, including any
  //     opening beat as its first entry.
  //   - hooks.openingScene: a fresh game's one-shot narrator beat (the
  //     card's .intro), rendered as the sole assistant card. Used only
  //     when there's no history to rebuild from.
  // A cold Quick Play start has neither — the first fable_send turn
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
// 4 lines × line-height 1.5 × 17px = 102px + 26px vertical padding = 128px.
const INPUT_MAX_HEIGHT = 128;
function autoGrow(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, INPUT_MAX_HEIGHT) + 'px';
}

// Track a stage-element listener so teardownStage can remove it (prevents
// the double-bind on re-wireStage — see stageListeners comment above).
// Use for any element whose DOM persists across stage entries.
function on(el, type, handler, opts) {
  el.addEventListener(type, handler, opts);
  stageListeners.push([el, type, handler, opts && opts.capture]);
}

// Generation state reflection (2026-07-27, no send button): the send/stop
// toggle button + its SVG icons are GONE. The only UI feedback for a turn in
// flight is now the in-card streaming caret (the .streaming class on the
// narrator beat, see fable.css). The input stays ENABLED so the empty-Enter-
// to-stop affordance (wired above in wireStage) works: pressing Enter on an
// empty field while generating stops the turn. The placeholder flips to hint
// the stop affordance so the user learns the gesture.
function setGenerating(on) {
  const input = stageRoot && stageRoot.querySelector('[data-input]');
  if (!input) return;
  if (on) {
    input.dataset.idlePlaceholder = input.placeholder;
    input.placeholder = 'Press Enter to stop…';
  } else {
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
  // Stash the text the player was trying to send so a retry re-sends it. The
  // backend already aborted the turn WITHOUT consuming the user message (the
  // api_lost path returns before add_message on the assistant side, but the
  // user turn WAS pushed at the top of fable_send — so on a successful retry
  // we send a fresh copy; the duplicate user turn is benign because the
  // retried narration reads the window, not a strict turn-count).
  pendingTurnText = input.value && input.value.trim();
  input.value = '';
  input.style.height = 'auto';
  input.disabled = true;
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
  input.disabled = false;
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
    narrator.sendFableTurn(text);
  }
}

function onKeyDown(e) {
  if (e.key !== 'Escape') return;
  // Priority order: raw editor → save modal → modal panel → wupi drawer →
  // left drawer. Innermost surface dismisses first so a player with stacked
  // surfaces can Esc them one at a time. The raw editor is highest (it darkens
  // the full stage + its validation lock means Esc refuses on invalid — only
  // ↻ or ✕ escape, so Esc on invalid just flashes the hint, not a close).
  if (rawEditorOpen() && rawEditorEsc()) { e.preventDefault(); return; }
  if (saveModalClose) {
    const overlay = stageRoot && stageRoot.querySelector('[data-save-overlay]');
    if (overlay && !overlay.hidden) { saveModalClose(); e.preventDefault(); return; }
  }
  // Backgrounds gallery modal — same z-tier as the save modal, so it dismisses
  // right after it (before the drawer/panel surfaces below).
  if (backgrounds.isOpen()) { backgrounds.closeBackgroundsPanel(); e.preventDefault(); return; }
  if (panelActive()) { dismissPanel(); e.preventDefault(); return; }
  if (wupiDrawer.isOpen()) { wupiDrawer.closeDrawer(); e.preventDefault(); return; }
  if (leftDrawer.isOpenState()) { leftDrawer.closeDrawer(); e.preventDefault(); return; }
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
    // Hover-reopen-keeps-active: if a tab was active before the drawer
    // auto-closed on mouseleave, re-render its dropdown now so it reappears
    // with the tab still glowing. No-op when no tab is active.
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

async function doSave(saveId, name, msg) {
  try {
    await saveNow(saveId, name);
    toast(msg || 'Saved.');
  } catch (err) {
    toast('Save failed: ' + err);
  }
}

export function toast(msg) {
  const t = stageRoot.querySelector('[data-toast]');
  if (!t) return;
  t.textContent = msg;
  t.hidden = false;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 2200);
}

// Populate the feed from a load result (messages: [{role, content, variants?,
// active_idx?, timestamp?}]). Forwards straight to beats.rebuildFromMessages,
// which is the single source of truth for the feed DOM.
export function loadHistory(messages) {
  beats.rebuildFromMessages(messages);
  beats.scrollDown();
}

// ── Typing indicator + card identity ────────────────────────────────────
// "(name) is currently typing.." pinned above the input row while a beat
// streams. The card identity (name + player_name) is fetched once on stage
// entry (best-effort) + cached in `activeCardName` (the display name for the
// typing label) + stashed in `activePlayerName` so the initNarrator call
// (which runs right after this in wireStage) can forward both into the
// beats builders for the message headers. Generic fallback when no name.
//
// DEFENSIVE DUAL-SHAPE: `fable_active_card_get` returns a plain card-name
// string today; a parallel Rust change widens it to `{name, player_name}`.
// We accept BOTH shapes so this UI works whether or not that Rust edit has
// landed yet (a string → name only, player_name stays ''; an object → both).
async function refreshActiveCardName(root) {
  // DEV PREVIEW: skip the IPC entirely — inject the placeholder identities +
  // portraits directly. The preview has no backend, so fable_active_card_get
  // would fail/return empty. The AI/narrator identity is "Game Master" (the
  // default narrator), the player is "Wanderer". NPCs fall back to the AI
  // portrait (per the deferred-NPC-portrait rule).
  if (DEV_PREVIEW) {
    activeCardName = 'Game Master';
    activePlayerName = 'Wanderer';
    activeCardPortrait = PLACEHOLDER_AI_PORTRAIT;
    activePlayerPortrait = PLACEHOLDER_PLAYER_PORTRAIT;
    npcNameMap = new Map();
    return;
  }
  try {
    const res = await invoke('fable_active_card_get');
    if (typeof res === 'string') {
      // Legacy plain-string shape (older backend): just the card name.
      activeCardName = res;
      activePlayerName = '';
      activeCardPortrait = '';
      activePlayerPortrait = '';
      npcNameMap = new Map();
    } else if (res && typeof res === 'object') {
      activeCardName = typeof res.name === 'string' ? res.name : '';
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
      activePlayerName = '';
      activeCardPortrait = '';
      activePlayerPortrait = '';
      npcNameMap = new Map();
    }
  } catch (_) {
    activeCardName = '';
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
  cancelCornerDwell();
  cornerTrigger = null;
  saveModalClose = null;
  // Reset the API-lost composer lock so a re-entry isn't stuck greyed out.
  composerLocked = false;
  pendingTurnText = '';
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
  // Wipe the dialogue feed so a prior session's cards don't leak into the
  // next entry (the stage DOM is reused across games).
  beats.clearFeed();
  activeCardName = '';
  // Reset the engine module state so a close mid-turn can't leave a stuck
  // `generating` (would no-op the next session's first send) or a dangling
  // activeBeat, and so the Wupi drawer starts fresh next entry.
  narrator.resetNarrator();
  wupiDrawer.resetWupiDrawer();
  leftDrawer.resetLeftDrawer();
  // Reset the tab rail (clears the active tab so no stale dropdown leaks into
  // the next session) + the raw editor (closes it if open — a mid-edit exit
  // discards unsaved textarea edits to the last-good file, same protection as
  // Alt+F4). The raw editor's atomic-write backend means the file on disk is
  // always the last successfully-saved state regardless.
  resetTabRail();
  resetRawEditor();
  // Selection popup teardown removed (the selection module + its regenerate_slice
  // IPC are dead). The stageListeners loop above already removes every tracked
  // stage-element listener, so nothing leaks.
}
