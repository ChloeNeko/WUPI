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
//      centered to the screen — see fable.css §1/§3). The archive is
//      FULLY off-screen at rest (height 0, visibility hidden — no peek
//      sliver, no fade mask). It's a BINARY wheel gesture: scrolling up
//      at the top opens the whole archive in one smooth drop (recent 2
//      stay pinned); once open it's read with NATIVE scroll (up past the
//      recent beats); scrolling back down to the very bottom edge snaps
//      it shut again. This module TAGS + runs the open/close tweens.
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
//   5. Micro-controls fade — the ‹ n/N › ↻ ✎ bar (already rendered by
//      beats.renderControls) is invisible by default + fades in on the
//      beat's hover. The CSS carries this; this module owns nothing here
//      beyond ensuring the bar keeps existing.
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
//
// History reveal is a binary OPEN/CLOSE driven by an opacity FADE (no drop,
// no slide, no height animation): wheel up at the top fades the archive in,
// wheel down at the bottom edge fades it out. The recent 2 are sticky-pinned
// (fable.css) so they never move; scrollTop is never touched so the page
// never scrolls. In between, the open archive is read with NATIVE scroll.
const RECENT_BEATS = 2;             // last N dialogue beats tagged vn-recent
const FLANK_BAND_PX = 14;           // dblclick-within-X-px-of-edge toggles portraits
const FADE_DURATION_MS = 260;       // matches the .vn-history opacity transition in fable.css — close collapses height only after this fades out

// Module state — all listeners + observers are owned here so teardown()
// can release every one. The stage DOM is reused across entries (see
// stage.js's stageListeners comment), so a clean teardown is required
// or the next wireStage would double-bind + double-observe.
let state = null;

// ── History re-tagging ──────────────────────────────────────────
// Walk the feed's dialogue beats + tag the last RECENT_BEATS as
// `vn-recent` (crisp) and everything older as `vn-history` (masked).
// System/error beats are excluded — they're narrow centered cards, not
// dialogue, and masking them would just look like a flicker. Idempotent
// (strips both classes before re-applying) so it's safe to call on
// every insertion. Exported so beats.js / stage.js can force a refresh
// after a feed rebuild.
export function refreshHistory() {
  if (!state || !state.feedEl) return;
  const beats = state.feedEl.querySelectorAll(
    '.fable-mes:not(.system):not(.error)'
  );
  const total = beats.length;
  beats.forEach((el, i) => {
    el.classList.remove('vn-recent', 'vn-history', 'vn-peek');
    // The last RECENT_BEATS read crisp. If there are fewer than
    // RECENT_BEATS+1 beats total, nothing is old enough to mask —
    // tagging everything vn-recent (a no-op style-wise) avoids masking
    // the sole beat in a fresh conversation.
    const recentFrom = Math.max(0, total - RECENT_BEATS);
    if (total <= RECENT_BEATS || i >= recentFrom) {
      el.classList.add('vn-recent');
    } else {
      el.classList.add('vn-history');
    }
  });

  // Sync the binary reveal state. When the beat SET changes (a new beat
  // landed, or a load/rebuild rewrote the feed), snap the archive shut
  // instantly so the new layout settles before the user re-opens it.
  const historyCount = Math.max(0, total - RECENT_BEATS);
  if (historyCount !== state.lastHistoryCount) {
    state.lastHistoryCount = historyCount;
    if (state.revealed) {
      state.revealed = false;
      closeHistory();
    }
  }
}

// ── Binary wheel-driven history reveal (opacity FADE) ───────────
// The archive is fully off-screen at rest (height 0, opacity 0). The reveal
// is a SMOOTH OPACITY FADE — no drop, no slide, no height animation:
//   • wheel UP at the top (collapsed)   → open: history snaps to its natural
//     height (invisibly, at opacity 0) and FADES IN. preventDefault so the
//     page never scrolls. The sticky recent 2 don't move.
//   • wheel DOWN at the bottom edge (open) → close: history FADES OUT (height
//     held during the fade so it doesn't vanish instantly), then collapses
//     once invisible. preventDefault; recent don't move.
//   • everything else → NATIVE scroll to read the open archive.
//
// Driven entirely by CSS classes on the feed (.vn-revealed / .vn-fading) +
// the opacity transition on .vn-history in fable.css. No rAF, no scrollTop
// manipulation, no per-beat inline styles.

function onFeedWheel(e) {
  if (!state || !state.feedEl) return;
  const feedEl = state.feedEl;
  const dy = e.deltaY;
  if (!state.revealed && dy < 0) {
    // Collapsed + ANY wheel up → fade the archive in. We preventDefault so the
    // page never scrolls on activation — the recent beats must not move. There's
    // no atTop gate: at rest the feed may be slightly scrolled (or a single
    // notch takes it off 0), and gating on scrollTop===0 made the first wheel-up
    // scroll the page instead of activating. The gesture is "wheel up = open",
    // unconditional while collapsed.
    e.preventDefault();
    openHistory();
  } else if (state.revealed && dy > 0) {
    const atBottom = feedEl.scrollTop + feedEl.clientHeight >= feedEl.scrollHeight - 1;
    if (atBottom) {
      // Open + wheel down at the very bottom edge → fade the archive out.
      e.preventDefault();
      closeHistory();
    }
    // Else: open but not at the bottom → let native scroll move through the
    // archive (don't preventDefault). Only the bottom edge closes it.
  }
  // Otherwise (revealed + wheel up, or no gesture matched) → native scroll.
}

// Open: add .vn-revealed (history beats take their natural height, fading in),
// then PIN scrollTop to the BOTTOM. The height change makes the feed tall;
// without pinning, scrollTop stays 0 and the view jumps to the TOP (oldest
// history) — the "teleport above the recent beats." Pinning to the bottom keeps
// the recent beats exactly where they were (at the viewport bottom), with the
// history fading in ABOVE them. The user then scrolls UP to read older history.
function openHistory() {
  const feedEl = state.feedEl;
  if (state.fadeTimer) { clearTimeout(state.fadeTimer); state.fadeTimer = null; }
  state.revealed = true;
  feedEl.classList.remove('vn-fading');
  feedEl.classList.add('vn-revealed');
  // Reading scrollHeight forces layout to commit the new (taller) height, then
  // we pin to the bottom so recent beats stay put.
  feedEl.scrollTop = feedEl.scrollHeight;
}

// Close: fade opacity OUT first (via .vn-fading, which holds height:auto so
// the beats keep their space while fading), then collapse the height AFTER
// the fade finishes (removing .vn-fading) — collapsing mid-fade would make
// them vanish instantly instead of fading. The duration matches the CSS
// transition (fable.css .vn-history transition: opacity 260ms).
function closeHistory() {
  const feedEl = state.feedEl;
  state.revealed = false;
  feedEl.classList.remove('vn-revealed');
  feedEl.classList.add('vn-fading');
  if (state.fadeTimer) clearTimeout(state.fadeTimer);
  state.fadeTimer = setTimeout(() => {
    state.fadeTimer = null;
    if (state && state.feedEl) state.feedEl.classList.remove('vn-fading');
  }, FADE_DURATION_MS);
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
    revealed: false,         // archive open/closed (binary, wheel-triggered)
    lastHistoryCount: -1,    // last seen history-beat count (change detection → close)
    fadeTimer: null,         // close collapses height only after the fade finishes
    observer: null,
  };

  // Listeners are bound to the stage root / feed (the reused elements)
  // + tracked so teardown can remove every one.
  state.boundWheel = onFeedWheel;
  state.boundDblClickStage = onStageDblClick;
  state.boundClickFeed = onFeedClick;
  state.boundDblClickFeed = onFeedDblClick;

  // wheel is passive:false so onFeedWheel can preventDefault on the
  // reveal/hide gestures (wheel up at top / wheel down at bottom).
  feedEl.addEventListener('wheel', state.boundWheel, { passive: false });
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
  // Cancel the pending close-collapse timer so it can't fire at a torn-down feed.
  if (state.fadeTimer) { clearTimeout(state.fadeTimer); }
  const { stageRoot, feedEl, boundWheel, boundDblClickStage, boundClickFeed, boundDblClickFeed, observer } = state;
  if (feedEl && boundWheel) feedEl.removeEventListener('wheel', boundWheel);
  if (stageRoot && boundDblClickStage) stageRoot.removeEventListener('dblclick', boundDblClickStage);
  if (feedEl && boundClickFeed) feedEl.removeEventListener('click', boundClickFeed);
  if (feedEl && boundDblClickFeed) feedEl.removeEventListener('dblclick', boundDblClickFeed);
  if (observer) observer.disconnect();
  // Clear the reveal/fade classes we put on the feed so a re-entry doesn't
  // inherit a stuck-open archive, plus the portrait-toggle classes.
  if (feedEl) {
    feedEl.classList.remove('vn-revealed', 'vn-fading', 'vn-hide-ai-portraits', 'vn-hide-user-portraits');
  }
  // Clear snapped portraits so a re-entry doesn't inherit orphan fixed sprites.
  if (feedEl) {
    feedEl.querySelectorAll('.fable-mes-avatar-img.vn-snapped')
      .forEach((img) => img.classList.remove('vn-snapped'));
  }
  state = null;
}
