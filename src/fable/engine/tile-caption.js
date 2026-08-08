// =============================================================
// TILE CAPTION — the shared word-stacking rule for New Game / Quick
// Play flow tiles (Chloe 2026-08-03).
//
// Rule (verbatim from the spec):
//   • 1 word  → one line.
//   • 2 words → stack VERTICALLY (one word per line).
//   • 3 words → first word on top, the other two together on the bottom.
//
// So every multi-word tile caption in the flow reads as a stacked block,
// not a single cramped line. Single-word captions (WORLD / CHARACTER /
// SCENARIO) stay one line. The split is pure (no DOM), returns an HTML
// string of <span class="fable-tile-line">…</span> rows; CSS makes each
// row display:block so they stack. Used by every screen that renders a
// .fable-newgame-tile-caption (newgame-split, quickplay-split, creator
// gates, + rebuildSplitTiles in fable.js) so the rule lives in ONE place.
// =============================================================

// Escape a caption word for safe HTML insertion. Captions are authored
// ASCII caps (CREATE / SIM / CARD / …), but belt-and-suspenders against
// any stray < > & in a user-typed label.
function esc(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

// Split a caption into display lines per the stacking rule, returning the
// inner HTML for a .fable-newgame-tile-caption (a sequence of line spans).
// `caption` is the raw text (case is preserved; callers .toUpperCase() if
// they want caps). Whitespace-only / empty input returns an empty string.
export function tileCaptionHTML(caption) {
  const words = String(caption || '').trim().split(/\s+/).filter(Boolean);
  if (words.length === 0) return '';
  let lines;
  if (words.length === 1) {
    lines = [words[0]];
  } else if (words.length === 2) {
    // 2 words → one per line (vertical stack).
    lines = [words[0], words[1]];
  } else {
    // 3+ words → first word on top, the rest joined on the bottom line.
    // (Spec is for exactly 3, but this generalizes gracefully: a 4-word
    // caption would show word 1 over "word2 word3 word4" — acceptable.)
    lines = [words[0], words.slice(1).join(' ')];
  }
  return lines.map((ln) => `<span class="fable-tile-line">${esc(ln)}</span>`).join('');
}
