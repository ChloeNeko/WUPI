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
// History reveal is a binary OPEN/CLOSE: wheel up at the top opens the
// whole archive in one smooth drop (recent 2 stay pinned); wheel down at
// the bottom edge snaps it shut again. In between, the open archive is
// explored with NATIVE scroll (scroll up past the recent beats to read
// older history). See onFeedWheel + openHistory/closeHistory below.
const RECENT_BEATS = 2;             // last N dialogue beats tagged vn-recent
const FLANK_BAND_PX = 14;           // dblclick-within-X-px-of-edge toggles portraits
const REVEAL_DURATION_MS = 340;     // open animation length (one smooth drop)
const COLLAPSE_DURATION_MS = 130;   // close animation length (fast "pull back up")
const HISTORY_MARGIN_PX = 10;       // per-beat margin on revealed archive beats (+2 2026-08-11: old beats spaced further apart). Visible gap between adjacent archive beats = 2× this (top+bottom margins sum — flex doesn't collapse).

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
  // INSTANTLY (no animation) so the new layout settles before the user
  // re-opens it — chasing a moving beat set mid-open is janky.
  const historyCount = Math.max(0, total - RECENT_BEATS);
  if (historyCount !== state.lastHistoryCount) {
    state.lastHistoryCount = historyCount;
    if (state.revealed) {
      cancelHistoryAnim();
      state.revealed = false;
      settleClosed(state.feedEl.querySelectorAll('.fable-mes.vn-history'));
    }
  }
}

// ── Binary wheel-driven history reveal ──────────────────────────
// The archive is FULLY off-screen at rest (height 0, visibility hidden).
// Revealing it is a single binary gesture, NOT a per-beat crawl:
//   • wheel UP at the top (collapsed)   → open the WHOLE archive in one
//     smooth drop. The drop is bottom-anchored, so the recent 2 stay
//     pinned at the bottom while the history fills the space above them.
//   • wheel DOWN at the bottom edge (open) → snap the archive shut fast
//     (a quick "pull back up" — one block, not a per-message animation).
//   • everything else → NATIVE scroll. Once open you read the archive by
//     scrolling normally — scroll up to move past the recent beats into
//     the older history, scroll back down to return. Only when you reach
//     the very bottom edge again does it collapse.
//
// The open/close are JS height tweens (height:auto isn't interpolable):
// every beat animates from its current height to its target (natural on
// open, 0 on close) as one block. Both directions bottom-anchor scrollTop
// each frame so the recent 2 stay pinned and never jump.

function onFeedWheel(e) {
  if (!state || !state.feedEl) return;
  const feedEl = state.feedEl;
  const atTop = feedEl.scrollTop <= 0;
  const atBottom = feedEl.scrollTop + feedEl.clientHeight >= feedEl.scrollHeight - 1;
  const dy = e.deltaY;
  if (!state.revealed && atTop && dy < 0) {
    // Collapsed + wheel up at the top → open the archive in one smooth drop.
    e.preventDefault();
    openHistory();
  } else if (state.revealed && atBottom && dy > 0) {
    // Open + wheel down at the very bottom edge → snap it shut immediately.
    e.preventDefault();
    closeHistory();
  }
  // Otherwise: native scroll — explore the open archive in either direction,
  // or scroll within tall recent beats.
}

// Open: every history beat tweens 0 → natural height as one block, over
// REVEAL_DURATION_MS. Bottom-anchored so the recent 2 stay pinned at the
// bottom and the archive drops into the space above them.
function openHistory() {
  const feedEl = state.feedEl;
  const beats = Array.from(feedEl.querySelectorAll('.fable-mes.vn-history'));
  if (beats.length === 0) { state.revealed = true; return; }
  cancelHistoryAnim();
  state.revealed = true;
  const targets = captureAndMeasure(beats);
  const startTime = performance.now();
  function frame(now) {
    const p = Math.min(1, (now - startTime) / REVEAL_DURATION_MS);
    const e = 1 - Math.pow(1 - p, 3);                 // easeOutCubic — settles like a drop
    for (const t of targets) tweenBeat(t, t.natH, HISTORY_MARGIN_PX, HISTORY_MARGIN_PX, e);
    feedEl.scrollTop = feedEl.scrollHeight - feedEl.clientHeight;   // bottom-anchor
    if (p < 1) state.raf = requestAnimationFrame(frame);
    else settleOpen(beats);
  }
  state.raf = requestAnimationFrame(frame);
}

// Close: every history beat tweens natural → 0 as one block, fast
// (COLLAPSE_DURATION_MS). Bottom-anchored so the recent 2 stay visible as
// the archive pulls back up. Called from the bottom-edge wheel-down AND
// from refreshHistory when the beat set changes (a layout reset).
function closeHistory() {
  const feedEl = state.feedEl;
  const beats = Array.from(feedEl.querySelectorAll('.fable-mes.vn-history'));
  state.revealed = false;
  if (beats.length === 0) return;
  cancelHistoryAnim();
  const targets = captureAndMeasure(beats);
  const startTime = performance.now();
  function frame(now) {
    const p = Math.min(1, (now - startTime) / COLLAPSE_DURATION_MS);
    const e = 1 - Math.pow(1 - p, 3);                 // easeOutCubic — quick pull-up
    for (const t of targets) tweenBeat(t, 0, 0, 0, e);
    feedEl.scrollTop = feedEl.scrollHeight - feedEl.clientHeight;   // bottom-anchor
    if (p < 1) state.raf = requestAnimationFrame(frame);
    else settleClosed(beats);
  }
  state.raf = requestAnimationFrame(frame);
}

// ── reveal helpers ──────────────────────────────────────────────

// Capture each beat's CURRENT height + margin as the tween start (pinning
// it inline so it holds still through the natural-height measurement), and
// measure its NATURAL height (height:auto + archive margin) in the same
// pass — restoring the pinned start before returning so the browser never
// paints at the auto height (no flash).
function captureAndMeasure(beats) {
  return beats.map((el) => {
    el.style.boxSizing = 'border-box';
    el.style.overflow = 'hidden';
    el.style.marginLeft = 'auto';
    el.style.marginRight = 'auto';
    const startH = el.getBoundingClientRect().height;
    const cs = getComputedStyle(el);
    const startMT = parseFloat(cs.marginTop) || 0;
    const startMB = parseFloat(cs.marginBottom) || 0;
    el.style.height = 'auto';
    el.style.marginTop = HISTORY_MARGIN_PX + 'px';
    el.style.marginBottom = HISTORY_MARGIN_PX + 'px';
    const natH = el.getBoundingClientRect().height;
    el.style.height = startH + 'px';
    el.style.marginTop = startMT + 'px';
    el.style.marginBottom = startMB + 'px';
    return { el, startH, startMT, startMB, natH };
  });
}

// Write one beat's interpolated height + margin for tween progress e.
function tweenBeat(t, endH, endMT, endMB, e) {
  const h = t.startH + (endH - t.startH) * e;
  const mt = t.startMT + (endMT - t.startMT) * e;
  const mb = t.startMB + (endMB - t.startMB) * e;
  t.el.style.height = h + 'px';
  t.el.style.marginTop = mt + 'px';
  t.el.style.marginBottom = mb + 'px';
  t.el.style.visibility = h < 1 ? 'hidden' : 'visible';
}

// Settle open: revealed beats keep an inline natural height of `auto`
// (responsive to resize/reflow, not a frozen px) + the archive margin.
function settleOpen(beats) {
  state.raf = null;
  for (const el of beats) {
    el.style.boxSizing = '';
    el.style.marginLeft = '';
    el.style.marginRight = '';
    el.style.height = 'auto';
    el.style.marginTop = HISTORY_MARGIN_PX + 'px';
    el.style.marginBottom = HISTORY_MARGIN_PX + 'px';
    el.style.overflow = '';
    el.style.visibility = 'visible';
  }
}

// Settle closed: clear every inline prop so the .vn-history CSS resting
// state (height:0, margin:0, hidden) governs again.
function settleClosed(beats) {
  state.raf = null;
  for (const el of beats) {
    el.style.boxSizing = '';
    el.style.height = '';
    el.style.marginTop = '';
    el.style.marginBottom = '';
    el.style.marginLeft = '';
    el.style.marginRight = '';
    el.style.overflow = '';
    el.style.visibility = '';
  }
}

function cancelHistoryAnim() {
  if (state && state.raf) {
    cancelAnimationFrame(state.raf);
    state.raf = null;
  }
}

// ── Portrait corner snaps (delegated click) ─────────────────────
// Clicking a beat portrait toggles .vn-snapped on the <img>. CSS does
// the detach-to-fixed-sprite + the dissolve-mask clear. We skip clicks
// that land on an actual control button (the ‹/›/↻/✎ bar) so a tap on
// a control never accidentally snaps. No-op on .no-avatar beats (the
// closest('.fable-mes-avatar-img') resolves null).
function onFeedClick(e) {
  // Don't snap when the user is interacting with the side-drawer.
  if (e.target.closest('.fable-mes-drawer')) return;
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
  if (e.target.closest('.fable-mes-editor, .fable-mes-drawer, .fable-mes-text')) return;
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
    lastHistoryCount: -1,    // last seen history-beat count (change detection → instant collapse)
    raf: null,               // in-flight open/close tween
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
  // Stop any in-flight reveal tween first so it doesn't fire frames at a
  // torn-down feed.
  cancelHistoryAnim();
  const { stageRoot, feedEl, boundWheel, boundDblClickStage, boundClickFeed, boundDblClickFeed, observer } = state;
  if (feedEl && boundWheel) feedEl.removeEventListener('wheel', boundWheel);
  if (stageRoot && boundDblClickStage) stageRoot.removeEventListener('dblclick', boundDblClickStage);
  if (feedEl && boundClickFeed) feedEl.removeEventListener('click', boundClickFeed);
  if (feedEl && boundDblClickFeed) feedEl.removeEventListener('dblclick', boundDblClickFeed);
  if (observer) observer.disconnect();
  // Clear any vn-* state classes we put on the feed so a re-entry isn't
  // stuck revealed / portrait-hidden.
  if (feedEl) {
    feedEl.classList.remove('vn-revealed', 'vn-hide-ai-portraits', 'vn-hide-user-portraits');
    // Clear any inline height/margin/visibility left on history beats by a
    // mid-tween teardown so a re-entry doesn't inherit stale overrides.
    feedEl.querySelectorAll('.fable-mes.vn-history').forEach((el) => {
      el.style.boxSizing = '';
      el.style.height = '';
      el.style.marginTop = '';
      el.style.marginBottom = '';
      el.style.marginLeft = '';
      el.style.marginRight = '';
      el.style.overflow = '';
      el.style.visibility = '';
    });
  }
  // Clear snapped portraits so a re-entry doesn't inherit orphan fixed sprites.
  if (feedEl) {
    feedEl.querySelectorAll('.fable-mes-avatar-img.vn-snapped')
      .forEach((img) => img.classList.remove('vn-snapped'));
  }
  state = null;
}
