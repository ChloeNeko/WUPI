// =============================================================
// SCREEN: STAGE — the narrator surface (the heart of immersion).
//
// Owns the full-screen stage: bg image + atmosphere overlay + FX
// layer + dialogue feed + input + Wupi drawer + panel overlay + toast.
// Wires engine/* modules together.
//
// This module is the COMPOSITION ROOT for an active game session:
//   - beats.js        → dialogue feed
//   - narrator.js     → fable_send streaming
//   - wupi-drawer.js  → chat_send (game master) + panel summoning
//   - fx/effects.js   → FX rendering (hooked by narrator)
//   - fx/atmosphere   → time/weather + parallax
//   - panels/manager  → read-view overlays summoned by Wupi
//
// Wupi trigger (Phase 1, decision 1): hover the right screen edge
// for 300ms — NO visible button. The pause overlay + Save-As modal
// are gone; Save/Load/Home live in the drawer footer.
// (The LEFT-side vitals/stats panel + mannequin were removed 2026-07-25 —
// shelved pending the rest of the UI settling. See stats-panel.js +
// mannequin.js in git history when the left side is revisited.)
//
// Two background modes:
//   1. TONE-KEYWORD MATCH: pick a starter bg by card tone keywords.
//   2. ATMOSPHERE: layered time/weather filter + parallax over the bg.
// =============================================================

// Background PNG imports removed in the Phase 0a asset wipe (decision 12:
// delete all assets; decision 4: backgrounds/sprites deferred). pickBgForTone
// below now returns '' so the <img data-bg> stays empty until the bg/sprite
// layer returns. Atmosphere parallax still targets the element (harmless on
// an empty img).
//
// PHASE 2: the empty img left a flat black void. Rather than ship a new
// image asset (which would reopen decision 4), the void is now filled by a
// CSS-only world-space treatment on `.fable-stage` itself — layered warm
// torchlight gradients + a faint parchment-grain texture (inline SVG data
// URI, no file) + a vignette. It reads as a candlelit stone interior at
// dusk: immersive enough that the empty <img> over it is invisible. When
// decision 4 lifts and pickBgForTone() returns a real URL, the <img> will
// paint OVER this treatment with no conflict (it sits above .fable-stage).

import * as beats from '../engine/beats.js';
import * as narrator from '../engine/narrator.js';
import * as wupiDrawer from '../engine/wupi-drawer.js';
import { initFX, playFX, clearFX, clearAllFX } from '../fx/effects.js';
import { initAtmosphere, attachParallax, resetAtmosphere } from '../fx/atmosphere.js';
import { saveNow } from '../engine/saves-io.js';
import { initPanelManager, summon as summonPanel, dismissPanel, isActive as panelActive } from '../panels/manager.js';
import { setMapTheme } from '../panels/map.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// tone keyword → starter background. Pure. Returns '' while background
// assets are deferred (Phase 0a wipe; decision 4). The keyword matching is
// kept so re-adding backgrounds later is a one-line restore per tone.
function pickBgForTone(tone = '') {
  return '';
}

// tone keyword → map atlas theme. Pure.
function mapThemeForTone(tone = '') {
  const t = tone.toLowerCase();
  if (/cyber|neon|futur|sci/.test(t)) return 'futuristic';
  if (/school|academy|modern|apartment|contemporary/.test(t)) return 'modern';
  return 'fantasy';
}

let stageRoot = null;
let toastTimer = null;
let cardContext = null;  // { card, saveId } for the active session
let cornerTrigger = null;   // the right-edge hover zone (Wupi drawer)
let cornerDwellTimer = null; // 300ms arm-before-open timer (right edge)
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
    <div class="fable-stage-bg"><img data-bg alt="" /></div>
    <div class="fable-atmo-layer" data-atmo></div>
    <div class="fable-fx-layer" data-fx></div>
    <div class="fable-dialogue-feed" data-feed></div>
    <form class="fable-input-row" data-input-form>
      <!-- The input group: a centered, max-width container holding the text
           box (which centers itself) + the send arrow absolutely positioned
           at the box's right edge. Decoupling the button from the flex flow
           means the box centers on its OWN (not as part of a box+button
           group), and the glyph can hug the textarea's right border
           tightly without button-internal padding creating a visible gap. -->
      <div class="fable-input-group">
        <div class="fable-input-box">
          <textarea class="fable-input" data-input rows="1" placeholder="Type a message..."></textarea>
        </div>
        <button type="button" class="send-toggle-btn fable-input-send" data-send-toggle aria-label="Send"></button>
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

    <aside class="fable-wupi-drawer" data-wupi-drawer>
      <!-- Chloe 2026-07-26: replaced the 🐾 avatar + "Wupi / Game Master"
           sublabel with a single centered, large, bold, glowy "WUPI"
           wordmark. The drawer's identity IS the brand. -->
      <header class="fable-wupi-header">
        <div class="fable-wupi-brand">WUPI</div>
      </header>
      <div class="fable-wupi-messages" data-wupi-messages></div>
      <form class="fable-wupi-input-row" data-wupi-form>
        <textarea class="fable-wupi-input" data-wupi-input rows="1" placeholder="Ask Wupi anything…"></textarea>
        <button type="button" class="send-toggle-btn" data-wupi-send-toggle aria-label="Send">▶</button>
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
  return root;
}

// Wire the stage after it's in the DOM. cardContext: { card, saveId }.
export function wireStage(root, hooks) {
  stageRoot = root;
  cardContext = hooks.cardContext || null;

  const feed = root.querySelector('[data-feed]');
  const bg = root.querySelector('[data-bg]');
  const atmo = root.querySelector('[data-atmo]');
  const fxLayer = root.querySelector('[data-fx]');

  // Background: tone-keyword match + map theme sync.
  const tone = (cardContext && cardContext.card && cardContext.card.tone) || '';
  bg.src = pickBgForTone(tone);
  setMapTheme(mapThemeForTone(tone));

  // Engine init (composition root).
  beats.initBeats(feed);
  initFX(fxLayer, root, { onTransient: () => {} });
  initAtmosphere(atmo, bg, {
    onWeatherStart: playFX,
    onWeatherStop: (name) => { if (name) clearFX(name); },
  });
  attachParallax();
  narrator.initNarrator({
    onTurnStart: () => setGenerating(true),
    onTurnEnd: () => {
      setGenerating(false);
      // (The left-side stats/mannequin panel refresh lived here — removed
      // 2026-07-25 when the left UI was shelved. Restore alongside the
      // mannequin when the left side is revisited.)
    },
    npcPretty: hooks.npcPretty || null,
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
    toggleBtn: root.querySelector('[data-wupi-send-toggle]'),
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
  const inputForm = root.querySelector('[data-input-form]');
  const input = root.querySelector('[data-input]');
  const toggleBtn = root.querySelector('[data-send-toggle]');
  on(inputForm, 'submit', (e) => {
    e.preventDefault();
    const text = input.value.trim();
    if (!text || narrator.isGenerating()) return;
    input.value = '';
    input.style.height = 'auto';
    narrator.sendFableTurn(text);
  });
  on(input, 'keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      inputForm.requestSubmit();
    }
  });
  on(input, 'input', () => autoGrow(input));
  // Single toggle button: the SVG send icon submits the turn, the SVG stop
  // icon stops generation. The narrator hooks (onTurnStart/onTurnEnd →
  // setGenerating) flip the icon + .is-stop class. Render the send icon now
  // so the button isn't empty before the first turn.
  on(toggleBtn, 'click', () => {
    if (narrator.isGenerating()) narrator.stopFableTurn();
    else inputForm.requestSubmit();
  });
  setGenerating(false);

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

  // Save icon → open the modal + focus the name input.
  on(footSave, 'click', () => {
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
    for (const m of hooks.loadMessages) {
      if (m.role === 'user') beats.addUserBeat(m.content);
      else if (m.role === 'assistant') {
        const b = beats.startNarratorBeat();
        beats.finalizeBeat(b, m.content);
      } else if (m.content) {
        beats.addSystemBeat(m.content);
      }
    }
  }

  // Ambient music removed in Fable asset wipe (Phase 0a). Music module deleted;
  // §2A "ambient title music" will be re-sourced when audio assets are re-added.
}

function autoGrow(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, 160) + 'px';
}

// Track a stage-element listener so teardownStage can remove it (prevents
// the double-bind on re-wireStage — see stageListeners comment above).
// Use for any element whose DOM persists across stage entries.
function on(el, type, handler, opts) {
  el.addEventListener(type, handler, opts);
  stageListeners.push([el, type, handler, opts && opts.capture]);
}

// The send/stop button SVG icons (inline — no asset files). The shared
// .send-toggle-btn color (brass, scoped to .fable-app) drives the fill via
// currentColor; the .is-stop class on the button flips currentColor to the
// muted crimson defined in fable.css. Replaces the prior raw ■ / ▶ text
// glyphs which read as dev placeholders.
const SEND_ICON_SVG = `
<svg viewBox="0 0 24 24" aria-hidden="true">
  <path d="M4 5.5c0-0.9 1-1.5 1.8-1.1l13.5 6.5c0.9 0.4 0.9 1.7 0 2.2L5.8 19.6C5 20 4 19.4 4 18.5V5.5z" fill="currentColor"/>
</svg>`;
const STOP_ICON_SVG = `
<svg viewBox="0 0 24 24" aria-hidden="true">
  <circle cx="12" cy="12" r="10" fill="none" stroke="currentColor" stroke-width="1.6" opacity="0.55"/>
  <rect x="8.5" y="8.5" width="7" height="7" rx="1.2" fill="currentColor"/>
</svg>`;

function setGenerating(on) {
  const toggleBtn = stageRoot.querySelector('[data-send-toggle]');
  const input = stageRoot.querySelector('[data-input]');
  if (!toggleBtn) return;
  // One button: send (▶) ↔ stop (■). The raw text glyphs are GONE — replaced
  // with inline SVG icons (a brass paper-plane-style send arrow + a proper
  // stop-square-in-circle for stop). The .is-stop class flips currentColor
  // to the muted crimson (scoped in .fable-app .send-toggle-btn.is-stop).
  // innerHTML swap (not textContent) so the SVG renders. The button's own
  // size + the .fable-input-send sizing are unchanged (per the locked
  // directive: don't resize the field or button).
  toggleBtn.innerHTML = on ? STOP_ICON_SVG : SEND_ICON_SVG;
  toggleBtn.classList.toggle('is-stop', on);
  toggleBtn.setAttribute('aria-label', on ? 'Stop' : 'Send');
  input.disabled = on;
  if (!on) input.focus();
}

function onKeyDown(e) {
  if (e.key !== 'Escape') return;
  // Priority order: save modal → modal panel → wupi drawer.
  // Innermost surface dismisses first so a player with stacked surfaces can
  // Esc them one at a time. The save modal is highest (it's a centered card
  // over everything when open).
  if (saveModalClose) {
    const overlay = stageRoot && stageRoot.querySelector('[data-save-overlay]');
    if (overlay && !overlay.hidden) { saveModalClose(); e.preventDefault(); return; }
  }
  if (panelActive()) { dismissPanel(); e.preventDefault(); return; }
  if (wupiDrawer.isOpen()) { wupiDrawer.closeDrawer(); e.preventDefault(); return; }
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
  }, CORNER_DWELL_MS);
}
function cancelCornerDwell() {
  if (cornerDwellTimer) { clearTimeout(cornerDwellTimer); cornerDwellTimer = null; }
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
export function loadHistory(messages) {
  beats.clearFeed();
  for (const m of messages || []) {
    if (m.role === 'user') beats.addUserBeat(m.content);
    else if (m.role === 'assistant') {
      const b = beats.startNarratorBeat();
      beats.finalizeBeat(b, m.content);
    } else beats.addSystemBeat(m.content);
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
  resetAtmosphere();
  beats.clearFeed();
  // Reset the engine module state so a close mid-turn can't leave a stuck
  // `generating` (would no-op the next session's first send) or a dangling
  // activeBeat, and so the Wupi drawer starts fresh next entry.
  narrator.resetNarrator();
  wupiDrawer.resetWupiDrawer();
}
