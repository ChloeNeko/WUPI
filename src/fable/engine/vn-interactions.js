// =============================================================
// VN INTERACTIONS — the Visual-Novel behavior layer for the feed.
//
// A SELF-CONTAINED interaction module layered ON TOP of the existing
// card-style dialogue feed (engine/beats.js → the .fable-mes-* DOM).
// No DOM is restructured + no .fable-mes-* class is renamed: this module
// only attaches small `vn-*` utility classes to the existing elements +
// binds delegated listeners on the stage root. The structural contract
// (the beat HTML shape, the .fable-feed container) stays owned by
// beats.js. This module owns these behaviors:
//
//   1. History tagging — refreshHistory() marks the last 2 dialogue beats
//      `vn-recent` and everything older `vn-history`. The CLASSES drive
//      the archive styling (no nameplate, no portrait, tight margins,
//      centered to the screen — see fable.css §1/§3). (2026-08-17) The
//      wheel-driven reveal animation is REMOVED: the feed is a plain
//      natively-scrollable column + history beats are always rendered —
//      this module only keeps the tagging fresh.
//   2. Portrait corner snaps — a single click on a beat portrait toggles
//      a .vn-snapped class on the <img>, which detaches it to a fixed
//      bottom-left (AI) / bottom-right (user) VN character sprite. A
//      second click re-docks it flush with its bubble.
//   3. Flank double-click — dblclick within the leftmost/rightmost
//      edge band toggles visibility of the docked (in-bubble) portraits
//      for AI / user beats respectively, WITHOUT touching snapped ones.
//   4. Click-to-edit — dblclick directly on a beat's prose opens the
//      inline editor (routes through the same onSave hook the ✎ button
//      uses in stage.js). Single-click + native text selection untouched.
//   5. Hover toolrail visibility — the per-beat `.fable-mes-drawer` rail
//      (built by beats.buildDrawerHTML, routed by stage.js) is revealed by
//      CSS on hover of the 2 latest beats (`vn-recent`); this module owns
//      nothing here beyond keeping the `vn-recent` tagging (item 1) fresh.
//
// Wiring (stage.js):
//   import * as vn from '../engine/vn-interactions.js';
//   const vnApi = vn.init({ stageRoot, feedEl, onEditBeat });
//   // ... on exit: vnApi.teardown();
//
// `onEditBeat(beat)` is called when a dblclick opens the editor on a
// beat; the caller (stage.js) wires the same enterEditMode + commit
// routing the ✎ control uses (user → editMessage, assistant → rewind).
// The module is deliberately DECOUPLED from narrator.js — it knows the
// DOM, not the IPC, mirroring the hooks pattern beats.js already uses.
// =============================================================

// Tuning constants. The flank band mirrors stage.js's EDGE_HIT_PX (14px)
// so the drawer-trigger shrink + the flank-toggle coexist.
const RECENT_BEATS = 2;             // last N dialogue beats tagged vn-recent
const FLANK_BAND_PX = 14;           // dblclick-within-X-px-of-edge toggles portraits

// Module state — all listeners + observers are owned here so teardown()
// can release every one. The stage DOM is reused across entries (see
// stage.js's stageListeners comment), so a clean teardown is required
// or the next wireStage would double-bind + double-observe.
let state = null;

// ── History re-tagging ──────────────────────────────────────────
// Walk the feed's dialogue beats + tag the last RECENT_BEATS as
// `vn-recent` (full treatment) and everything older as `vn-history`
// (centered text-only archive styling). System/error beats are excluded
// — they're narrow centered cards, not dialogue. Idempotent (strips both
// classes before re-applying) so it's safe to call on every insertion.
// Exported so beats.js / stage.js can force a refresh after a feed
// rebuild.
export function refreshHistory() {
  if (!state || !state.feedEl) return;
  const beats = state.feedEl.querySelectorAll(
    '.fable-mes:not(.system):not(.error)'
  );
  const total = beats.length;
  beats.forEach((el, i) => {
    el.classList.remove('vn-recent', 'vn-history');
    // The last RECENT_BEATS read with the full card treatment. If there
    // are fewer than RECENT_BEATS+1 beats total, nothing is old enough to
    // de-style — tagging everything vn-recent avoids stripping the sole
    // beat in a fresh conversation down to the archive look.
    const recentFrom = Math.max(0, total - RECENT_BEATS);
    if (total <= RECENT_BEATS || i >= recentFrom) {
      el.classList.add('vn-recent');
    } else {
      el.classList.add('vn-history');
    }
  });
}

// ── Portrait corner snaps (delegated click) ─────────────────────
// Clicking a beat portrait toggles .vn-snapped on the <img>. CSS does
// the detach-to-fixed-sprite + the dissolve-mask clear. We skip clicks
// that land on an actual control button (the ‹/›/↻/✎ bar) so a tap on
// a control never accidentally snaps. No-op on .no-avatar beats (the
// closest('.fable-mes-avatar-img') resolves null).
function onFeedClick(e) {
  // Don't snap when the user is interacting with the side-drawer or the
  // history-beat iron tools column.
  if (e.target.closest('.fable-mes-drawer, .fable-mes-histools')) return;
  const img = e.target.closest('.fable-mes-avatar-img');
  if (!img) return;
  e.preventDefault();
  e.stopPropagation();
  img.classList.toggle('vn-snapped');
  // A newly-snapped portrait must NOT be masked even if its beat is
  // history — the snapped sprite is the live focus, not background.
  // The CSS handles this via the :not(.vn-snapped) carve-out in the
  // hide-portraits rules; for the history mask we add an explicit
  // exemption class so the snapped <img>'s parent row still dims but
  // the fixed sprite reads at full opacity (position:fixed takes it out
  // of the masked row's paint box anyway, so no extra JS needed).
}

// ── Flank double-click (toggle docked portraits) ────────────────
// dblclick within FLANK_BAND_PX of the left/right viewport edge toggles
// visibility of the docked AI / user portraits respectively. Snapped
// sprites are unaffected (CSS :has(.vn-snapped) carve-out). Attached to
// the stage root, NOT to invisible flank divs — no transparent rectangles
// eat stray clicks this way. Matches the hysteresis pattern.
function onStageDblClick(e) {
  if (!state || !state.feedEl) return;
  // Ignore dblclicks that originate on interactive feed content (the
  // editor textarea, control buttons) — those have their own dblclick
  // semantics + shouldn't toggle portrait visibility.
  if (e.target.closest('.fable-mes-editor, .fable-mes-drawer, .fable-mes-histools, .fable-mes-text')) return;
  const vw = window.innerWidth;
  if (e.clientX <= FLANK_BAND_PX) {
    state.feedEl.classList.toggle('vn-hide-ai-portraits');
  } else if (e.clientX >= vw - FLANK_BAND_PX) {
    state.feedEl.classList.toggle('vn-hide-user-portraits');
  }
}

// ── Click-to-edit (delegated dblclick on prose) ─────────────────
// dblclick on the .fable-mes-text node opens the inline editor via the
// caller-supplied onEditBeat hook. Single-click + drag stay native
// (only dblclick is bound) so the user can still highlight prose.
// Streaming beats are excluded — editing a live stream is nonsense.
function onFeedDblClick(e) {
  if (!state || typeof state.onEditBeat !== 'function') return;
  const text = e.target.closest('.fable-mes-text');
  if (!text) return;
  const beat = text.closest('.fable-mes');
  if (!beat) return;
  // Never edit a streaming beat (the prose is mid-flight) or one
  // already being edited.
  if (beat.classList.contains('streaming') || beat.classList.contains('editing')) return;
  state.onEditBeat(beat);
}

// ── MutationObserver: auto re-tag on feed mutations ─────────────
// Rather than coupling this module into beats.js's append sites, watch
// the feed for childList changes + re-run refreshHistory. This catches
// every path (live streaming, load/resume rebuild, future inserters we
// don't know about) without a single edit to beats.js. The observer is
// cheap: childList only (no subtree churn), + refreshHistory is a short
// NodeList walk over dialogue beats. Re-tagging is idempotent.
function onFeedMutation(_mutations, _observer) {
  refreshHistory();
}

// ── Lifecycle ───────────────────────────────────────────────────
// init({ stageRoot, feedEl, onEditBeat }) → { teardown }.
// Called from stage.js wireStage AFTER beats.initBeats (so feedEl is
// bound + any seed beats are present). Returns a handle whose teardown()
// releases every listener + the observer — stage.js calls it from
// teardownStage so the reused stage DOM doesn't accumulate bindings.
export function init({ stageRoot, feedEl, onEditBeat }) {
  // Defensive: if a prior init leaked (no teardown), tear it down first.
  if (state) teardown();

  state = {
    stageRoot,
    feedEl,
    onEditBeat,
    observer: null,
  };

  // Listeners are bound to the stage root / feed (the reused elements)
  // + tracked so teardown can remove every one.
  state.boundDblClickStage = onStageDblClick;
  state.boundClickFeed = onFeedClick;
  state.boundDblClickFeed = onFeedDblClick;

  stageRoot.addEventListener('dblclick', state.boundDblClickStage);
  feedEl.addEventListener('click', state.boundClickFeed);
  feedEl.addEventListener('dblclick', state.boundDblClickFeed);

  // Observer: re-tag on any beat insertion/removal. childList on the
  // feed catches appendMes + rebuildFromMessages + clearFeed.
  state.observer = new MutationObserver(onFeedMutation);
  state.observer.observe(feedEl, { childList: true, subtree: false });

  // Initial tag pass for any beats already in the feed (load/resume path
  // — beats.rebuildFromMessages may have run before this init).
  refreshHistory();

  return { teardown };
}

export function teardown() {
  if (!state) return;
  const { stageRoot, feedEl, boundDblClickStage, boundClickFeed, boundDblClickFeed, observer } = state;
  if (stageRoot && boundDblClickStage) stageRoot.removeEventListener('dblclick', boundDblClickStage);
  if (feedEl && boundClickFeed) feedEl.removeEventListener('click', boundClickFeed);
  if (feedEl && boundDblClickFeed) feedEl.removeEventListener('dblclick', boundDblClickFeed);
  if (observer) observer.disconnect();
  // Clear the portrait-toggle classes we put on the feed so a re-entry
  // doesn't inherit them, plus any snapped portraits (orphan fixed sprites).
  if (feedEl) {
    feedEl.classList.remove('vn-hide-ai-portraits', 'vn-hide-user-portraits');
    feedEl.querySelectorAll('.fable-mes-avatar-img.vn-snapped')
      .forEach((img) => img.classList.remove('vn-snapped'));
  }
  state = null;
}
