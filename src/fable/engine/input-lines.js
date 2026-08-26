// =============================================================
// INPUT-LINES — the shared line-locked composer grower (2026-08-24).
//
// Born from the Fable composer fix: a textarea whose vertical padding
// lives INSIDE its scroll viewport always frames fractional lines (the
// bottom-cut sliver). The contract this module implements, EXACTLY as
// the stage composer does it:
//
//   • The field itself is CHROMELESS — no vertical padding, no border;
//     its height is pure whole-line math (1·L, 2·L, … maxLines·L).
//     The visual chrome (inset, bg, frame) lives on a wrapper element
//     OUTSIDE the scroll area, so the scroll viewport is always a whole
//     number of lines and a mid-line cut is geometrically impossible.
//   • CSS must pin a WHOLE-PIXEL line-height on the field (the grid is
//     lineHeight multiples).
//   • Wheel = exactly ONE line per click, always landing on the grid.
//     Trackpad / keyboard / scrollbar scrolls snap onto the same grid.
//   • While typing past the cap, the view follows the caret line only
//     when that line leaves the window (stable view otherwise),
//     measured through an off-screen mirror (same discipline as
//     stage.js's autoGrow — the live field is only ever WRITTEN).
//
// Used by: the Fable Wupi-drawer chat input + the OS-home Wupi chat
// input. (The Fable composer keeps its own stage.js implementation —
// its mirror also feeds placeholder/metric decisions local to it.)
// =============================================================

function escText(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;');
}

export function wireLineLockedInput(el, maxLines = 3) {
  let mirror = null;

  const metrics = () => {
    const cs = getComputedStyle(el);
    const lineHeight = parseFloat(cs.lineHeight) || Math.round(parseFloat(cs.fontSize) * 1.5);
    return { cs, lineHeight };
  };

  // Off-screen measurement mirror: same font/letter-spacing/line-height +
  // horizontal padding + width, so its line count IS the field's.
  const ensureMirror = (m) => {
    if (!mirror) {
      mirror = document.createElement('div');
      mirror.style.cssText =
        'position:fixed;left:-99999px;top:0;visibility:hidden;' +
        'white-space:pre-wrap;overflow-wrap:break-word;word-break:normal;' +
        'box-sizing:border-box;border:0;margin:0;';
      document.body.appendChild(mirror);
    }
    const cs = m.cs;
    mirror.style.font = cs.font;
    mirror.style.lineHeight = cs.lineHeight;
    mirror.style.letterSpacing = cs.letterSpacing;
    mirror.style.paddingLeft = cs.paddingLeft;
    mirror.style.paddingRight = cs.paddingRight;
    mirror.style.width = el.getBoundingClientRect().width + 'px';
    return mirror;
  };

  // Quantize a scroll offset onto the whole-line grid (clamped to range).
  const snap = (lineHeight, target) => {
    const raw = Math.round(target / lineHeight) * lineHeight;
    const maxScroll = el.scrollHeight - el.clientHeight;
    return Math.max(0, Math.min(raw, maxScroll));
  };

  const grow = () => {
    const m = metrics();
    const L = m.lineHeight;
    const max = L * maxLines;
    const mir = ensureMirror(m);
    // Trailing '\u200b': a div collapses a block-final '\n' in pre-wrap —
    // a bare Shift+Enter must still count as a line.
    mir.textContent = el.value + '\u200b';
    const full = Math.ceil(mir.scrollHeight);
    mir.innerHTML = escText(el.value.slice(0, el.selectionStart)) + '<span data-mk>\u200b</span>';
    const marker = mir.querySelector('[data-mk]');
    const caretTop = marker ? marker.offsetTop : 0;
    el.style.height = Math.min(Math.max(full, L), max) + 'px';
    if (full > max) {
      el.style.overflow = 'auto';
      let st = el.scrollTop;
      if (caretTop + L > st + max) st = caretTop - (max - L);
      else if (caretTop < st) st = caretTop;
      el.scrollTop = snap(L, st);
    } else {
      el.style.overflow = 'hidden';
      el.scrollTop = 0;
    }
  };

  const onWheel = (e) => {
    if (el.style.overflow !== 'auto') return; // fits: nothing to scroll
    e.preventDefault();
    const { lineHeight: L } = metrics();
    const dir = e.deltaY > 0 ? 1 : e.deltaY < 0 ? -1 : 0;
    if (!dir) return;
    el.scrollTop = snap(L, el.scrollTop + dir * L);
  };

  const onScroll = () => {
    const { lineHeight: L } = metrics();
    const snapped = snap(L, el.scrollTop);
    if (snapped !== el.scrollTop) el.scrollTop = snapped;
  };

  el.addEventListener('input', grow);
  el.addEventListener('wheel', onWheel, { passive: false });
  el.addEventListener('scroll', onScroll);
  grow();
  return () => {
    el.removeEventListener('input', grow);
    el.removeEventListener('wheel', onWheel);
    el.removeEventListener('scroll', onScroll);
    if (mirror) { mirror.remove(); mirror = null; }
  };
}
