// =============================================================
// FABLE SELECTION — highlight-to-regenerate popup.
//
// When the player highlights a passage inside the LAST assistant beat (the
// most recent narrator/character card), a small "Regenerate" chip pops up
// flush against the right edge of the selection. Clicking it invokes the
// `regenerate_slice` IPC, which asks the model to rewrite ONLY the
// highlighted passage so the new text flows into the surrounding prose; the
// replacement is spliced back into the same beat in-place (no new turn, no
// schema mutation — same line as `edit_message`, the 4th UX chat control).
//
// GATING (mirrors the existing edit/reroll UX chat controls, beats.js):
//   - last assistant beat only (refreshControls pins reroll there too).
//     Highlighting in mid-history beats does nothing — editing mid-history
//     prose without branching the timeline would desync (same v1 limit).
//   - beat is not .streaming (mid-generation) or .editing (inline editor).
//   - selection is non-empty + fully contained in the beat's body.
//
// POPUP PLACEMENT: the popup is a stage-root child (NOT a beat descendant)
// because `.fable-dialogue-feed` has `pointer-events: none` — only `.fable-
// beat` descendants re-enable it. A floating popup needs to be reachable
// even when the selection starts to collapse, so it lives one level up next
// to `.fable-toast`. Position is computed from `selection.getClientRects()`
// against the stage's bounding rect.
//
// STREAMING: the IPC streams real text chunks (unlike Crossroads/Ghostwriter
// heartbeats) so the user watches the gap refill. We swap the beat body into
// a "regenerating" state — replace the body innerHTML with the in-flight
// replacement bracketed by the before/after anchors (read-only), then on
// `done` the backend returns the full new `messages[]` and we hand off to
// the standard `beats.rebuildFromMessages` path (single source of truth).
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import * as beats from './beats.js';

const ICON_REROLL = '<svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true">' +
  '<path fill="none" stroke="currentColor" stroke-width="1.6" ' +
  'd="M3 8a5 5 0 1 1 1.5 3.6M3 11.5V8h3.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';

let stageRoot = null;
let feedEl = null;        // the .fable-dialogue-feed element
let popup = null;         // the floating chip
let onGenerating = null;  // hook: reflect generation state on the input
let onComplete = null;    // hook: re-stamp controls after a successful splice
let active = false;       // a regenerate is in flight (suppress new popups)
let listeners = [];       // tracked for teardown (mirrors stage.js's pattern)
// Chloe 2026-07-27 — lag fix: cache the last assistant beat per popup-show
// session. `selectionchange` fires continuously during a drag (dozens of
// times/sec) and re-querying `.fable-beat[data-role="assistant"]` on every
// event is pure waste — the beat never changes mid-drag. Cached on first
// use after initSelection / feed rebuild; invalidated when the feed is
// known to have changed (we re-query lazily when the cached node is
// detached or missing).
let cachedLastBeat = null;
// rAF coalescer for onSelectionChange — at most ONE execution per animation
// frame regardless of how many selectionchange events fire between frames.
let pendingFrame = null;

// Escape user text for safe HTML injection. Local copy so this module has no
// dependency on beats.js's private `esc`/`prose` helpers.
function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
function prose(s) {
  return esc(s).replace(/\n/g, '<br>');
}

export function initSelection(root, hooks = {}) {
  stageRoot = root;
  feedEl = root.querySelector('[data-feed]');
  onGenerating = hooks.onGenerating || null;
  onComplete = hooks.onComplete || null;
  cachedLastBeat = null;  // invalidate cache on (re)init
  pendingFrame = null;

  // The popup is a single stage-root child, re-positioned per selection.
  // Built once here, hidden by default via the [hidden] attr (CSS toggles).
  popup = document.createElement('div');
  popup.className = 'fable-selection-popup';
  popup.setAttribute('role', 'button');
  popup.setAttribute('aria-label', 'Regenerate selected passage');
  popup.title = 'Regenerate this passage';
  popup.hidden = true;
  popup.innerHTML =
    `<span class="fable-selection-popup-icon">${ICON_REROLL}</span>` +
    `<span class="fable-selection-popup-label">Regenerate</span>`;
  stageRoot.appendChild(popup);

  on(popup, 'mousedown', (e) => {
    // Stop the mousedown from collapsing the selection before click fires —
    // we read the selection off the captured rect at click time, so we need
    // it intact. preventDefault on mousedown is the standard fix.
    e.preventDefault();
  });
  on(popup, 'click', () => onRegenerateClick());

  // `selectionchange` fires on the document (not the selection target) per
  // spec; mouseup is the user-visible "done selecting" signal on most
  // platforms. We listen to both: selectionchange updates position live as
  // the user drags, mouseup is the canonical "show the popup" beat.
  on(document, 'selectionchange', onSelectionChange);
  on(document, 'mouseup', onSelectionChange);
}

export function teardownSelection() {
  for (const [el, type, handler, capture] of listeners) {
    el.removeEventListener(type, handler, capture);
  }
  listeners = [];
  if (pendingFrame !== null) {
    cancelAnimationFrame(pendingFrame);
    pendingFrame = null;
  }
  if (popup && popup.parentNode) popup.parentNode.removeChild(popup);
  popup = null;
  stageRoot = null;
  feedEl = null;
  onGenerating = null;
  onComplete = null;
  cachedLastBeat = null;
  active = false;
}

// Track a listener so teardown removes it (the stage DOM is reused across
// entries — same shape as stage.js's on()).
function on(el, type, handler, opts) {
  el.addEventListener(type, handler, opts);
  listeners.push([el, type, handler, opts && opts.capture]);
}

function hidePopup() {
  if (popup) popup.hidden = true;
  // Invalidate the cached popup size so the next show re-measures
  // (defensive: if the label or icon ever changes between shows, the
  // cached size would be wrong. The popup is content-fixed today, so this
  // is just future-proofing — cheap to reset).
  cachedPopupSize = null;
}

// Find the last assistant beat (the only beat eligible for selective
// regenerate). Returns the beat element or null. Chloe 2026-07-27: cached
// per popup-show session — `selectionchange` fires dozens of times during
// a drag and re-querying the DOM each time was wasteful. The cached node
// is validated via `isConnected` only (cheap, no DOM scan). If a new
// narrator turn finalizes mid-drag (rare — the user is dragging, not
// submitting), the cache may briefly point at the prior beat, but
// `eligibleSelection`'s containment check will catch it (the live
// selection won't be inside the stale beat's body) and the popup simply
// won't show — the user re-selects. Acceptable edge case vs the cost of
// re-querying on every selectionchange tick.
function lastAssistantBeat() {
  if (!feedEl) return null;
  if (cachedLastBeat && cachedLastBeat.isConnected) {
    return cachedLastBeat;
  }
  const beatsAll = feedEl.querySelectorAll('.fable-beat[data-role="assistant"]');
  if (!beatsAll.length) {
    cachedLastBeat = null;
    return null;
  }
  cachedLastBeat = beatsAll[beatsAll.length - 1];
  return cachedLastBeat;
}

// The currently-active selection, if it lives entirely inside the last
// assistant beat's body. Returns { beat, range, rect, text } or null.
function eligibleSelection() {
  const beat = lastAssistantBeat();
  if (!beat) return null;
  // Skip if the beat is streaming or in inline-edit mode.
  if (beat.classList.contains('streaming') || beat.classList.contains('editing')) return null;
  const body = beat.querySelector('.fable-beat-body');
  if (!body) return null;

  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) return null;
  const range = sel.getRangeAt(0);

  // Fully contained inside the body (not straddling into another beat).
  if (!body.contains(range.startContainer) || !body.contains(range.endContainer)) return null;

  const text = sel.toString();
  if (!text || !text.trim()) return null;

  // getClientRects returns per-line rects for multi-line selections; we want
  // a single bounding rect for popup placement. Use range.getBoundingClientRect()
  // — it spans the full selection box.
  const rect = range.getBoundingClientRect();
  if (!rect || (rect.width === 0 && rect.height === 0)) return null;

  return { beat, body, range, rect, text };
}

// Chloe 2026-07-27 — lag fix: coalesce selectionchange events through a
// single requestAnimationFrame. `selectionchange` fires continuously while
// the user drags to extend a selection (not just at mouseup), and the old
// handler ran the full pipeline (DOM query + getBoundingClientRect × 3) on
// every tick — dozens per second — starving the main thread. rAF batches
// all events that land between two frames into one execution, matching the
// browser's repaint cadence. The work is identical; it just happens ≤60×/s
// instead of unbounded. mouseup is NOT throttled (it's a single discrete
// event per drag — the canonical "show the popup" beat — so it runs
// immediately to avoid a 1-frame lag on release).
function onSelectionChange(evt) {
  if (active) return; // a regenerate is in flight — leave the popup hidden.
  // mouseup: run immediately (discrete event, no batching).
  if (evt && evt.type === 'mouseup') {
    if (pendingFrame !== null) {
      cancelAnimationFrame(pendingFrame);
      pendingFrame = null;
    }
    runSelectionUpdate();
    return;
  }
  // selectionchange during a drag: coalesce.
  if (pendingFrame !== null) return; // already scheduled — drop the new one.
  pendingFrame = requestAnimationFrame(() => {
    pendingFrame = null;
    runSelectionUpdate();
  });
}

// The actual update work — factored out so both the rAF path and the
// immediate mouseup path share one implementation.
function runSelectionUpdate() {
  const info = eligibleSelection();
  if (!info) {
    hidePopup();
    return;
  }
  positionPopup(info.rect);
}

// Place the popup flush against the right edge of the selection rect,
// vertically centered on it, clamped to the stage viewport. Chloe
// 2026-07-27 — lag fix: avoid the layout-thrash pattern of "reveal →
// measure → write → re-measure" on every event. We measure the popup
// ONCE (lazily, the first time it's shown) and cache its size; on
// subsequent calls we only write left/top, never re-read layout. The
// popup's size is content-fixed (icon + label, never reflows), so this
// is safe.
let cachedPopupSize = null; // { w, h } — invalidated on hidePopup()
function positionPopup(selRect) {
  if (!popup || !stageRoot) return;
  const stageRect = stageRoot.getBoundingClientRect();
  // First show: reveal + measure once, cache. Subsequent shows reuse the
  // cached size — no forced reflow on the hot path.
  if (!cachedPopupSize) {
    popup.hidden = false;
    const popRect = popup.getBoundingClientRect();
    cachedPopupSize = { w: popRect.width, h: popRect.height };
  } else if (popup.hidden) {
    popup.hidden = false;
  }
  const popW = cachedPopupSize.w;
  const popH = cachedPopupSize.h;
  // Right edge of selection + a small gap so the chip doesn't kiss the text.
  const GAP = 6;
  let left = selRect.right + GAP - stageRect.left;
  let top = selRect.top + selRect.height / 2 - popH / 2 - stageRect.top;
  // Clamp horizontally: if the chip would overflow the right edge, flip it
  // to the LEFT of the selection instead (still vertically centered).
  if (left + popW > stageRect.width - 8) {
    left = selRect.left - GAP - popW - stageRect.left;
  }
  // Clamp horizontally on the left too: if it overflowed off-screen left,
  // fall back to sitting at the right with a smaller gap.
  if (left < 8) {
    left = Math.max(8, selRect.right + GAP - stageRect.left);
  }
  // Clamp vertically into the stage.
  top = Math.max(8, Math.min(stageRect.height - popH - 8, top));
  popup.style.left = `${Math.round(left)}px`;
  popup.style.top = `${Math.round(top)}px`;
}

// Click handler — capture the selection's text context, invoke the IPC, and
// stream the replacement live into the beat body.
function onRegenerateClick() {
  if (active) return;
  const info = eligibleSelection();
  if (!info) {
    hidePopup();
    return;
  }
  // Snapshot the selection's structural context BEFORE we mutate anything.
  // `beats.getBeatText` returns the body's innerText (BRs collapsed to
  // newlines) — that's the same shape the backend stores in `content`, so
  // indexOf on it lands at the same splice site the backend will validate.
  const beatText = beats.getBeatText(info.beat);
  if (!beatText || !info.text) {
    hidePopup();
    return;
  }
  // Find the highlight inside the beat text. indexOf (first occurrence) —
  // matches the backend's apply_regenerate_slice, which also uses the first
  // occurrence for the splice site. If the highlight appears multiple times
  // the player gets the first one rewritten, which is the predictable choice.
  const idx = beatText.indexOf(info.text);
  if (idx < 0) {
    // Should be near-impossible (the selection came FROM this text), but if
    // whitespace normalization disagrees between innerText and the DOM we
    // bail rather than send a mismatched snapshot to the backend.
    hidePopup();
    return;
  }
  const before = beatText.slice(0, idx);
  const highlight = info.text;
  const after = beatText.slice(idx + highlight.length);
  const beatIndex = Number.parseInt(info.beat.dataset.index || '', 10);
  if (!Number.isInteger(beatIndex)) {
    hidePopup();
    return;
  }

  // Clear the live selection so the user sees the regeneration as a distinct
  // phase. collapse at the beat body's start (no visible caret jump).
  const sel = window.getSelection();
  if (sel) sel.removeAllRanges();
  hidePopup();
  startRegenerate(info.beat, info.body, beatIndex, before, highlight, after);
}

async function startRegenerate(beat, body, beatIndex, before, highlight, after) {
  active = true;
  if (onGenerating) onGenerating(true);

  // Swap the body into a "regenerating" preview state: show before + an
  // inline live-streaming slot + after, all read-only. The slot refills as
  // chunks arrive. The .regenerating class on the beat lets CSS dim the
  // surrounding anchors slightly so the eye tracks the filling gap.
  beat.classList.add('regenerating');
  let streamed = '';
  const renderPreview = () => {
    body.innerHTML =
      prose(before) +
      `<span class="fable-slice-slot">${prose(streamed) || '<span class="fable-slice-caret"></span>'}</span>` +
      prose(after);
  };
  renderPreview();
  // Scroll the slot into view as it grows.
  const scrollSlotIntoView = () => {
    const slot = body.querySelector('.fable-slice-slot');
    if (slot && slot.scrollIntoView) slot.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
  };

  const channel = new Channel();
  channel.onmessage = (msg) => {
    if (!msg || typeof msg !== 'object') return;
    switch (msg.type) {
      case 'chunk': {
        if (typeof msg.text === 'string' && msg.text) {
          streamed += msg.text;
          renderPreview();
          scrollSlotIntoView();
        }
        break;
      }
      case 'fallback':
        // API dropped, local took over — no UI change needed (the chunks
        // keep flowing). Could be surfaced as a subtle toast later.
        break;
      case 'error':
        finishRegenerate(beat, /* restoreFull */ null, msg.message || 'Regenerate failed.');
        return;
      case 'done': {
        // Chloe 2026-07-27 — flicker fix: splice the replacement IN-PLACE
        // into the existing beat body instead of wiping + rebuilding the
        // whole feed. The old path called beats.rebuildFromMessages, which
        // nukes every beat (.innerHTML = '') and recreates them all as new
        // DOM nodes — that wholesale teardown was the flicker source (every
        // beat lost its layout, scroll jumped, full repaint). The streaming
        // preview above already does the right thing (mutates only this
        // beat's body), so on `done` we just finalize that same mutation
        // with the authoritative replacement + the unchanged anchors.
        // The backend has already persisted; the UI is already visually in
        // sync because the streamed preview IS the replacement. The `beat`
        // + `body` refs are still valid (we never wiped the feed).
        const replacement = typeof msg.replacement === 'string' ? msg.replacement : streamed;
        if (body) {
          body.innerHTML = prose(before) + prose(replacement) + prose(after);
        }
        finishRegenerate(beat, /* restoreFull */ null, null);
        // Invalidate the last-beat cache (the body content changed; a
        // subsequent selection must re-resolve).
        cachedLastBeat = null;
        // Re-stamp edit/reroll controls on the last beat (the in-place
        // splice didn't touch the controls block, but the regenerating
        // state may have interfered — cheap + idempotent to call).
        if (onComplete) onComplete();
        return;
        return;
      }
    }
  };

  try {
    await invoke('regenerate_slice', {
      index: beatIndex,
      before,
      highlight,
      after,
      onEvent: channel,
    });
  } catch (err) {
    // IPC-level error (the command rejected). Show an error beat so the
    // user knows it failed, then restore the beat to its pre-regenerate
    // state (the live content was NOT mutated server-side on this path).
    beats.addErrorBeat(String(err));
    finishRegenerate(beat, /* restoreFull */ before + highlight + after, null);
  }
}

// Tear down the "regenerating" preview state. If `restoreFull` is non-null,
// the body is reset to that verbatim (used on IPC failure where the backend
// didn't touch anything). On success the feed was already rebuilt by the
// caller, so the beat ref is stale and `restoreFull` is null.
function finishRegenerate(beat, restoreFull, errMsg) {
  if (errMsg) beats.addErrorBeat(errMsg);
  if (restoreFull != null && beat && beat.querySelector) {
    const body = beat.querySelector('.fable-beat-body');
    if (body) body.innerHTML = prose(restoreFull);
  }
  if (beat && beat.classList) beat.classList.remove('regenerating');
  active = false;
  if (onGenerating) onGenerating(false);
}

// Public so stage.js / external callers can check whether a regenerate is in
// flight (e.g. to gate the input's Enter-to-stop gesture during one).
export function isRegenerating() {
  return active;
}
