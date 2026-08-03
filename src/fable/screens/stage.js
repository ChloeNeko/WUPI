// =============================================================
// SCREEN: STAGE — the narrator surface (the heart of immersion).
//
// Owns the full-screen stage: dialogue feed + input + Wupi drawer +
// panel overlay + toast. Wires engine/* modules together.
//
// This module is the COMPOSITION ROOT for an active game session:
//   - beats.js        → dialogue feed
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
// BACKGROUND + WEATHER STRIPPED (2026-07-26, Quick Play rewrite): the bg
// <img>, the tone-keyword bg picker, the atmosphere layer (time-of-day
// filter + weather), and the mouse parallax were all removed. The stage is
// a pure black void for ALL games now — Quick Play lands here straight from
// the void interview, and the manual-card path no longer has a tavern card
// to source a bg from. Backgrounds + weather will be re-added in a later
// pass once the new art direction settles. The `.fable-stage-bg` element
// stays in the DOM as a no-op over the black .fable-stage so the re-add is a
// one-line structural change.
// =============================================================

import * as beats from '../engine/beats.js';
import * as narrator from '../engine/narrator.js';
import * as wupiDrawer from '../engine/wupi-drawer.js';
import * as leftDrawer from '../engine/left-drawer.js';
import { buildTabRail, renderActive, resetTabRail } from '../engine/tab-rail.js';
import { buildRawEditor, onEsc as rawEditorEsc, resetRawEditor, isOpen as rawEditorOpen } from '../engine/raw-editor.js';
import { invoke } from '@tauri-apps/api/core';
// playFX + clearFX were the weather-render hooks pre-stripping; weather is
// gone now (file header), so only initFX + clearAllFX remain used. The two
// named exports stay imported here so re-adding weather later is a one-line
// restore (the FX registry itself is unchanged).
import { initFX, playFX, clearFX, clearAllFX } from '../fx/effects.js';
// Touch the weather hooks so a strict linter doesn't flag them as unused
// (they're reserved for the weather re-add). No-op at runtime.
void playFX; void clearFX;
import { saveNow } from '../engine/saves-io.js';
import { initPanelManager, summon as summonPanel, dismissPanel, isActive as panelActive } from '../panels/manager.js';
import { setMapTheme } from '../panels/map.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// Background + atmosphere were stripped (see file header). The bg layer
// (.fable-stage-bg) is empty; mapTheme is a static 'fantasy' default so the
// optional map panel still gets a usable atlas theme without depending on a
// card tone.

let stageRoot = null;
let toastTimer = null;
let cardContext = null;  // { card, saveId } for the active session
let activeCardName = '';  // display name of the seated card (the typing indicator)
let activePlayerName = '';  // protagonist name (card.player_name) → user-beat headers
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

export function buildStage() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-stage';
  root.dataset.fableScreen = 'stage';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-stage-bg"></div>
    <div class="fable-atmo-layer" data-atmo></div>
    <div class="fable-fx-layer" data-fx></div>
    <div class="fable-dialogue-feed" data-feed></div>
    <!-- Typing indicator: "(name) is currently typing.." pinned just above the
         input row while a narrator/character beat streams. Toggled via the
         .is-visible class from onTurnStart/onTurnEnd hooks. -->
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
           prose panel; the dropdown persists across drawer close/reopen (the
           hover-reopen-keeps-active mechanic). -->
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

  const feed = root.querySelector('[data-feed]');
  const fxLayer = root.querySelector('[data-fx]');

  // Background + atmosphere stripped (file header). The bg layer
  // (.fable-stage-bg) is empty — it paints nothing over the pure black
  // .fable-stage. Map theme is a static 'fantasy' default so the optional
  // map panel still works without a card tone.
  setMapTheme('fantasy');

  // Engine init (composition root).
  beats.initBeats(feed);
  initFX(fxLayer, root, { onTransient: () => {} });
  // The typing indicator + the message-header identity: fetch the active
  // card's name + player_name ONCE (best-effort) so the narrator/beats
  // builders can stamp the headers. Awaited so the names are in hand before
  // initNarrator forwards them (a fetch failure falls back to generic labels).
  await refreshActiveCardName(root);
  narrator.initNarrator({
    onTurnStart: () => {
      setGenerating(true);
      showTypingIndicator(root);
    },
    onTurnEnd: () => {
      setGenerating(false);
      hideTypingIndicator(root);
      // A narrator turn just finalized → a new assistant beat is now the
      // last in the feed. Re-stamp controls so the swipe/regenerate affordance
      // moves onto it (and the prior last assistant beat loses its bar).
      refreshControls();
      // §11.30 Left-Drawer HUD: a turn may have mutated player body (Combat
      // Referee), entities (item-grant brackets), clock/weather (TIME/
      // WEATHER commands). Refresh the paperdoll + ambient + inventory so
      // the HUD reflects the new ground truth. Best-effort (no await — the
      // UI re-enable above must not block on IPC latency).
      leftDrawer.refreshAll();
    },
    npcPretty: hooks.npcPretty || null,
    cardName: activeCardName,
    playerName: activePlayerName,
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

  // Crossroads (the option-picker modal) + Ghostwriter (the Impersonate ✎
  // button) + Selective Regenerate (highlight-to-rewrite) were removed
  // 2026-07-31: their backend IPCs (crossroads_generate / ghostwriter_generate
  // / regenerate_slice) are dead stubs after the narrator/codex layer strip.
  // The generate_options / set_directive Director tools are still live in
  // Rust and now render only the generic tool chip in the Wupi drawer.

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

  // Delegated click handler for the per-beat UX controls (edit / reroll).
  // Delegation (one listener on the feed container) beats per-beat binding
  // because `beats.rebuildFromMessages` wipes + recreates the whole feed on
  // every mutation — per-beat listeners would need re-binding each time.
  // The handler reads `data-action` off the clicked button (set in
  // `beats.renderControls`) and the `data-index` off the enclosing beat.
  //
  // Edit has two flows:
  //   - edit on the LAST beat (regardless of role) → `rewindAndEditUser`
  //     if it's a user beat (branch the timeline + regen) OR `editMessage`
  //     if it's the last assistant beat (in-place typo fix, no regen). The
  //     distinction: editing a user message changes what the AI replies TO,
  //     so we must regenerate; editing an assistant message is cosmetic.
  //   - edit on a NON-last user beat → `rewindAndEditUser` (this is the
  //     "edit 3 turns ago" case the spec describes — branch the timeline).
  // Reroll is only ever on the last assistant beat (refreshControls pins
  // it there) → `rerollLastTurn`.
  // Shared edit entry-point used by BOTH the legacy pencil (removed) and the
  // new double-click-to-edit (2026-07-29). Routes by role: user beats branch
  // the timeline (rewind + regen); assistant beats get an in-place typo fix.
  function beginEdit(beat) {
    const role = beat.dataset.role;
    const idx = Number.parseInt(beat.dataset.index || '', 10);
    if (!Number.isInteger(idx)) return;
    beats.enterEditMode(beat, {
      onSave: (newText) => {
        if (!newText) return;
        if (role === 'user') {
          // Editing a user message always branches the timeline (the AI
          // would reply differently to the new text) → rewind + regen.
          narrator.rewindAndEditUser(idx, newText);
        } else {
          // Assistant message edit → in-place typo fix, no regen.
          narrator.editMessage(idx, newText);
        }
      },
      onCancel: () => { /* no-op — editor already torn down */ },
    });
  }

  // Delegated click handler for the feed controls (the swipe arrows + the edit
  // ✎ button in the hover header). The › arrow on the last variant of the last
  // beat carries `data-action="regenerate"` (a new variant is generated +
  // appended — unlimited); ‹ / non-last › carry swipe-left/right (navigate
  // existing variants); `data-action="edit"` enters inline edit mode (same
  // routing as the dblclick shortcut below). One delegated listener because
  // `beats.renderControls` rebuilds the actions slot on every mutation.
  on(feed, 'click', (e) => {
    const btn = e.target.closest('[data-action]');
    if (!btn) return;
    const beat = e.target.closest('.fable-beat');
    if (!beat) return;
    const action = btn.dataset.action;
    const idx = Number.parseInt(beat.dataset.index || '', 10);
    if (!Number.isInteger(idx)) return;
    if (narrator.isGenerating()) return;
    if (action === 'edit') {
      // Header ✎ button → inline edit (same routing as dblclick).
      if (beat.classList.contains('streaming') || beat.classList.contains('editing')) return;
      beginEdit(beat);
      return;
    }
    if (action === 'regenerate') {
      // › on the last variant → generate a fresh sibling variant (uncapped).
      narrator.rerollLastTurn();
      return;
    }
    if (action === 'swipe-left' || action === 'swipe-right') {
      const target = Number.parseInt(btn.dataset.targetVariant || '0', 10);
      if (Number.isInteger(target)) narrator.swipeVariant(idx, target);
      return;
    }
  });

  // Double-click anywhere on a beat → edit mode (2026-07-29, replaces the
  // pencil button). Same routing as the old pencil click: user beats branch,
  // assistant beats get an in-place fix. Ignored while generating so a
  // mid-stream dblclick can't collide with the active beat.
  on(feed, 'dblclick', (e) => {
    const beat = e.target.closest('.fable-beat');
    if (!beat) return;
    if (beat.classList.contains('streaming') || beat.classList.contains('editing')) return;
    if (narrator.isGenerating()) return;
    const role = beat.dataset.role;
    if (role !== 'user' && role !== 'assistant') return; // no editing system beats
    beginEdit(beat);
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
  const EDGE_HIT_PX = 6;  // how close to the absolute edge counts as "touching"
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

  // Esc: dismiss panel → close wupi.
  document.addEventListener('keydown', onKeyDown, true);

  // If we have an opening scene (New Game on a fresh card), render it
  // as the first narrator beat.
  if (hooks.openingScene) {
    const b = beats.startNarratorBeat();
    beats.appendChunk(b, hooks.openingScene);
    beats.finalizeBeat(b, hooks.openingScene);
  }
  // Resumed session (Continue / Load): re-populate the feed from the loaded
  // messages in one pass so the user sees their prior conversation. Comes
  // AFTER the opening-scene render so a fresh game shows the scene first;
  // for a resumed game openingScene is null (the card's opening beat is
  // already in the message history).
  if (Array.isArray(hooks.loadMessages) && hooks.loadMessages.length) {
    beats.rebuildFromMessages(hooks.loadMessages);
  }
  // Stamp the UX chat controls (edit on user beats + last assistant beat,
  // reroll on the last assistant beat). Called after every populate path
  // and after every narrator turn finalizes (see onTurnEnd wiring below).
  refreshControls();

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

// Populate the feed from a load result (messages: [{role, content}]).
// Delegates to `beats.rebuildFromMessages` (single source of truth — the
// mutation wrappers in narrator.js use the same path) then stamps the UX
// chat controls via `refreshControls`.
export function loadHistory(messages) {
  beats.rebuildFromMessages(messages);
  refreshControls();
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
  try {
    const res = await invoke('fable_active_card_get');
    if (typeof res === 'string') {
      activeCardName = res;
      activePlayerName = '';
    } else if (res && typeof res === 'object') {
      activeCardName = typeof res.name === 'string' ? res.name : '';
      activePlayerName = typeof res.player_name === 'string' ? res.player_name : '';
    } else {
      activeCardName = '';
      activePlayerName = '';
    }
  } catch (_) {
    activeCardName = '';
    activePlayerName = '';
  }
  updateTypingText(root);
}
function typingLabel() {
  // "Game Master" is the neutral fallback (the seated card IS the GM persona
  // — data/fable.sim). A named card (e.g. "Rusty Tavern") reads as that card.
  const who = activeCardName || 'Game Master';
  return `${who} is currently typing..`;
}
function updateTypingText(root) {
  const el = root.querySelector('[data-typing-text]');
  if (el) el.textContent = typingLabel();
}
function showTypingIndicator(root) {
  updateTypingText(root);
  const ind = root.querySelector('[data-typing-indicator]');
  if (ind) ind.classList.add('is-visible');
}
function hideTypingIndicator(root) {
  const ind = root.querySelector('[data-typing-indicator]');
  if (ind) ind.classList.remove('is-visible');
}

// Walk the feed and stamp UX chat controls (edit / reroll) on each beat
// based on its role + position:
//   - every USER beat: edit (in-place typo fix via `edit_message`).
//   - the LAST ASSISTANT beat: edit (in-place) + reroll (regen via
//     `reroll_last_turn`).
// Assistant beats that aren't last get nothing — you can't reroll a
// non-final turn, and editing a mid-history assistant message without
// branching the timeline would desync the conversation. Mid-history edits
// of assistant prose are intentionally not supported at v1.
//
// Idempotent: `beats.renderControls` removes any prior controls block
// before injecting, so calling this after every turn finalize is cheap.
function refreshControls() {
  const feedEl = stageRoot && stageRoot.querySelector('[data-feed]');
  if (!feedEl) return;
  const allBeats = feedEl.querySelectorAll('.fable-beat');
  if (!allBeats.length) return;
  const lastIdx = allBeats.length - 1;
  allBeats.forEach((beat, i) => {
    const role = beat.dataset.role;
    // Variant state is stamped on the beat dataset (beats.stampVariants) so
    // we can render the ‹ n/N › swipe bar without a message cache. Default
    // count 1.
    const variantCount = Number.parseInt(beat.dataset.variantCount || '1', 10);
    const activeVariant = Number.parseInt(beat.dataset.activeVariant || '0', 10);
    if (role === 'assistant' && i === lastIdx) {
      // Last assistant beat: the swipe bar (hover-gated via the header) + the
      // edit ✎ button. › on the last variant is the regenerate trigger
      // (variants are uncapped).
      beats.renderControls(beat, {
        variantCount,
        activeVariant,
        isLastBeat: true,
        canRegenerate: true,
        canEdit: true,
      });
    } else if (role === 'assistant') {
      // Earlier assistant beats: hover-gated swipe bar (navigation only, no
      // regenerate, no edit — mid-history assistant edits would desync).
      beats.renderControls(beat, { variantCount, activeVariant, isLastBeat: false });
    } else if (role === 'user') {
      // User beats: the edit ✎ button only (rewind-and-edit branches the
      // timeline). No swipe bar (variants are an assistant-only affordance).
      beats.renderControls(beat, { variantCount: 1, activeVariant: 0, isLastBeat: false, canEdit: true });
    }
    // System beats: no controls.
  });
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
//   - tears down the FX timers + atmosphere (RAF + window listener) +
//     clears the dialogue feed.
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
  // Clear the toast timer so it can't fire into a torn-down element after
  // close (was a residual-state gap — harmless but not clean).
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }
  // Dismiss any open panel so panel-manager's `active` flag + the overlay
  // content don't persist into the next session.
  if (panelActive()) dismissPanel();
  clearAllFX();
  // (resetAtmosphere was here pre-stripping — atmosphere is gone now, see
  // file header. beats.clearFeed is the only feed reset needed.)
  beats.clearFeed();
  // Hide the typing indicator (a close mid-stream could leave it visible).
  if (stageRoot) hideTypingIndicator(stageRoot);
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
