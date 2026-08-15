// =============================================================
// FABLE SLICE REGENERATE — the golden pencil (2026-08-11).
//
// When the player drag-highlights a span of text inside an AI narrator
// message, a sleek brass pencil floats in at the right edge of the
// selection. Click → the API rewrites ONLY that span in place, splicing
// cleanly against the surrounding prose (see narrator.regenerateSlice +
// the fable_regenerate_slice IPC for the backend half).
//
// Scope: a non-collapsed selection whose start AND end anchors lie in
// the SAME `.fable-mes.assistant:not(.editing):not(.streaming)` beat's
// `.fable-mes-text`. User beats, cross-beat, and streaming/editing
// beats show no pencil. While a turn is generating, no pencil.
//
// Coexists with the dblclick-to-edit handler (vn-interactions.onFeedDbl
// Click): a dblclick selects a word then opens the textarea + `.editing`
// class, which dissolves the selection → the `.editing` guard below
// suppresses the pencil for that beat. A rAF defer on show avoids the
// dblclick flash.
//
// This module is self-contained + removable (mirrors drawer-logic.js /
// shell-guard.js): initSliceRegen returns { teardown } and touches only
// `document` + the feed root.
// =============================================================

// The inline pencil glyph (a sleek nib-forward pencil). Kept inline so it
// inherits the brass `currentColor` + scales with the button.
const PENCIL_SVG =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
  '<path d="M12 20h9"/>' +
  '<path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4 12.5-12.5z"/>' +
  '</svg>';

// The single lazily-created pencil element + the beat it's currently
// anchored to. Reused across selections (never recreated).
let pencilEl = null;
let currentBeat = null;
let pendingFrame = 0;
let listeners = [];
let teardownDone = false;

// Lazy-build the pencil button. Appended to <body> + positioned fixed so it
// can float over any beat regardless of each beat's transform/overflow. A
// child of <body> (not the beat) avoids being swept by feed rebuilds.
function pencil() {
  if (pencilEl) return pencilEl;
  pencilEl = document.createElement('button');
  pencilEl.type = 'button';
  pencilEl.className = 'fable-slice-pencil';
  pencilEl.setAttribute('aria-label', 'Regenerate highlighted text');
  pencilEl.innerHTML = PENCIL_SVG;
  pencilEl.addEventListener('mousedown', (e) => {
    // A click-drag that starts on the pencil must not reseed a prose
    // selection; stop it before the browser does anything clever.
    e.preventDefault();
    e.stopPropagation();
  });
  pencilEl.addEventListener('click', (e) => {
    e.preventDefault();
    e.stopPropagation();
    onPencilClick();
  });
  document.body.appendChild(pencilEl);
  return pencilEl;
}

// Determine whether a selection is a slice-regen candidate + resolve the
// target beat. Returns { beat, textEl, range } or null.
function resolveSelection() {
  const sel = window.getSelection();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) return null;
  const range = sel.getRangeAt(0);
  // startContainer/endContainer are usually text nodes inside .fable-mes-text.
  const startText = closestTextEl(range.startContainer);
  const endText = closestTextEl(range.endContainer);
  if (!startText || startText !== endText) return null;
  const beat = startText.closest('.fable-mes.assistant');
  if (!beat) return null;
  if (beat.classList.contains('editing')) return null;
  if (beat.classList.contains('streaming')) return null;
  if (beat.classList.contains('slice-regenerating')) return null;
  // Empty-string selection guard (a click can leave a non-collapsed range
  // whose toString is whitespace-only).
  if (!range.toString().trim()) return null;
  // Detached-DOM guard (P2 fix): after a feed rebuild (chat-side messages
  // event, an edit/rewind elsewhere) the Range still resolves on the
  // DETACHED tree — closest() works on detached nodes. Clicking the pencil
  // then splices stale pre/selection/post into whatever message now holds
  // the stale dataset.index. Only live beats are eligible.
  if (!beat.isConnected) return null;
  return { beat, textEl: startText, range };
}

function closestTextEl(node) {
  if (!node) return null;
  // Text nodes have no closest(); walk up to the element.
  const el = node.nodeType === Node.TEXT_NODE ? node.parentElement : node;
  if (!el) return null;
  const textEl = el.closest ? el.closest('.fable-mes-text') : null;
  return textEl || null;
}

// Reposition the pencil at the right edge of the selection's last line,
// vertically centered on that line. Falls back to the range's bounding rect.
function positionPencil(beat, range) {
  const p = pencil();
  const beatRect = beat.getBoundingClientRect();
  const rect = range.getBoundingClientRect();
  // The selection's end is the right edge of its last line. getBoundingClient
  // Rect returns the union of all selected lines; use its right edge + the
  // vertical midpoint of its bottom line (approximated by the rect's lower
  // third) so a multi-line selection still pins the pencil sensibly.
  const lineH = Math.max(16, rect.height || 24);
  const cy = rect.bottom - lineH / 2;
  p.style.left = `${rect.right + 10}px`;
  p.style.top = `${cy}px`;
  p.classList.add('visible');
  currentBeat = beat;
}

function show() {
  const res = resolveSelection();
  if (!res) {
    hide();
    return;
  }
  positionPencil(res.beat, res.range);
}

function hide() {
  if (pencilEl) pencilEl.classList.remove('visible');
  currentBeat = null;
}

// On pencil click: compute the 3-way split with consistent Range.toString()
// semantics (so <br>/quote/entity handling is uniform across all three +
// pre + selection + post is exactly the beat's visible text), collapse the
// selection, hide the pencil, hand off to the regen wrapper.
function onPencilClick() {
  const res = resolveSelection();
  if (!res) {
    hide();
    return;
  }
  const { beat, textEl, range } = res;
  const index = Number.parseInt(beat.dataset.index || '-1', 10);
  if (index < 0 || typeof onRegenerate !== 'function') {
    hide();
    return;
  }

  // pre = text in this beat before the selection. Build a Range spanning
  // from the start of .fable-mes-text to the selection start.
  let pre = '';
  try {
    const preRange = document.createRange();
    preRange.selectNodeContents(textEl);
    preRange.setEnd(range.startContainer, range.startOffset);
    pre = preRange.toString();
  } catch (_) {
    pre = '';
  }

  const selection = range.toString();

  // post = text in this beat after the selection.
  let post = '';
  try {
    const postRange = document.createRange();
    postRange.selectNodeContents(textEl);
    postRange.setStart(range.endContainer, range.endOffset);
    post = postRange.toString();
  } catch (_) {
    post = '';
  }

  // Collapse the selection + hide the pencil before the body swap dissolves
  // the selection (the beat's .fable-mes-text is rebuilt by beginSliceRegen).
  window.getSelection()?.removeAllRanges?.();
  hide();
  onRegenerate({ index, pre, selection, post });
}

// Module-bound callback set by initSliceRegen.
let onRegenerate = null;
let isGenerating = null;

function scheduleShow() {
  if (teardownDone) return;
  if (pendingFrame) cancelAnimationFrame(pendingFrame);
  // Defer one frame so a dblclick's transient word-selection doesn't flash
  // the pencil before enterEditMode swaps to a textarea + .editing (the
  // .editing guard inside resolveSelection then suppresses it).
  pendingFrame = requestAnimationFrame(() => {
    pendingFrame = 0;
    if (isGenerating && isGenerating()) {
      hide();
      return;
    }
    show();
  });
}

function onScrollOrResize() {
  // Reposition the pencil relative to the live selection rect on
  // scroll/resize (the rect is viewport-relative; `position: fixed` keeps
  // it put, but the selection's screen position moved). If the selection
  // was dissolved, hide.
  if (!pencilEl || !pencilEl.classList.contains('visible')) return;
  const res = resolveSelection();
  if (!res) {
    hide();
    return;
  }
  positionPencil(res.beat, res.range);
}

// ── Public API ──────────────────────────────────────────────────

// Wire the selection → pencil behavior. `feedEl` is the [data-feed]
// container (scoped for teardown in case future refinements need it).
// `isGenerating` returns true while any narrator turn / slice regen is
// streaming (gates the pencil off). `onRegenerate({ index, pre, selection,
// post })` is invoked on pencil click — narrator.regenerateSlice.
export function initSliceRegen({ isGenerating: genFn, onRegenerate: regenFn }) {
  isGenerating = genFn || (() => false);
  onRegenerate = regenFn || null;
  teardownDone = false;

  // selectionchange is document-level (cheap); the mouseup/keyup listeners
  // are the real flush points (selectionchange fires continuously during a
  // drag, which we debounce via rAF).
  const selHandler = () => scheduleShow();
  const flushHandler = () => scheduleShow();
  document.addEventListener('selectionchange', selHandler);
  document.addEventListener('mouseup', flushHandler);
  document.addEventListener('keyup', flushHandler);
  window.addEventListener('scroll', onScrollOrResize, { passive: true, capture: true });
  window.addEventListener('resize', onScrollOrResize);

  listeners = [
    ['document', 'selectionchange', selHandler],
    ['document', 'mouseup', flushHandler],
    ['document', 'keyup', flushHandler],
  ];

  return { teardown };
}

function teardown() {
  teardownDone = true;
  if (pendingFrame) {
    cancelAnimationFrame(pendingFrame);
    pendingFrame = 0;
  }
  for (const [, ev, fn] of listeners) {
    document.removeEventListener(ev, fn);
  }
  listeners = [];
  window.removeEventListener('scroll', onScrollOrResize, { capture: true });
  window.removeEventListener('resize', onScrollOrResize);
  hide();
  if (pencilEl) {
    pencilEl.remove();
    pencilEl = null;
  }
  currentBeat = null;
  onRegenerate = null;
  isGenerating = null;
}

// ── Pure helpers (exported for unit tests) ──────────────────────

// Predicate: does a resolved selection object point at a slice-regen-
// eligible beat? Exposed DOM-free so tests can pin the gate logic.
export function isSliceEligible({ role, editing, streaming, sliceRegenerating, collapsed, emptyText }) {
  if (role !== 'assistant') return false;
  if (editing || streaming || sliceRegenerating) return false;
  if (collapsed) return false;
  if (emptyText) return false;
  return true;
}
