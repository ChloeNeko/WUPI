// =============================================================
// SPELLCHECK — the custom right-click surface for WUPI's written
// inputs (2026-08-23, Chloe; OS-chat zone added same day).
//
// The OS-wide right-click ban (shell-guard.js, 2026-08-20) stays in
// force: the WebView's NATIVE context menu is dead everywhere, always.
// This module is the ONE sanctioned right-click surface — a Fable-
// styled menu on every written input in TWO zones:
//
//   • FABLE — every textarea / text input inside #fable (the composer,
//     the Wupi-drawer chat, the raw editors, the creator chat, save-
//     name fields). Misspelled words underline in AGED BRASS.
//   • OS-HOME WUPI CHAT — the #chat window's input. Same menu, same
//     toggle; the underline reads the WUPI purple (#b534fa) instead of
//     brass (Chloe, same day).
//
//   • A dictionary spellchecker (SCOWL 80 — the curated spellchecker
//     word list, ~284k lowercase entries + a curated British-variant
//     pass — shipped unbundled at public/spellcheck/ + lazy-fetched on
//     first focus so boot pays nothing) underlines misspelled words
//     while you type. ON by default; per Chloe's spec the checker
//     judges WORDS ONLY — punctuation, capitalization, acronyms,
//     digit-glued tokens, and non-ASCII (accented) words are never
//     flagged.
//   • Right-click a misspelled word → ranked correction candidates +
//     a "Disable spellchecker" item pinned to the very bottom.
//     Right-click anywhere else in an input → just the toggle item.
//     Clicking a candidate replaces the word in place (matching the
//     original's first-letter capitalization) and fires a real `input`
//     event so every screen's own listeners (autoGrow, dirty-state…)
//     see the edit.
//   • The enable/disable toggle persists in localStorage, governs BOTH
//     zones at once, and is initialized once from script.js (app-wide
//     boot — the listeners are document-level delegation, so no
//     per-screen wiring anywhere).
//
// MECHANICS: the underline cannot style text inside a <textarea>, so
// each focused field gets a transparent-text mirror overlay (position:
// fixed, exact computed-metrics copy, rAF-synced to the field's rect +
// scroll — the same mirror discipline as stage.js's autoGrow measurer).
// The field itself is never touched: no reparenting, no class edits,
// fully native caret/selection/IME behavior. The word under the right-
// click is read from `selectionStart` (Chromium moves the caret on
// mousedown, so by contextmenu time it sits at the click point).
//
// The pure layer (tokenizer, suggestion search, replacement math) has
// NO DOM imports and is pinned by tests/spellcheck.test.mjs.
// =============================================================

import { setContextMenuPassThrough } from '../../shell-guard.js';

// ── Pure layer ───────────────────────────────────────────────────────

// A "word" is a maximal run of Unicode letters, allowing internal
// apostrophes (don't / Liam's) so they tokenize as ONE token and can be
// skipped whole — splitting on the apostrophe would flag "shouldn".
const TOKEN_RE = /\p{L}+(?:['’]\p{L}+)*/gu;

const ASCII_ONLY = /^[\x00-\x7F]*$/;
const HAS_APOSTROPHE = /['’]/;
const IS_UPPER = /^[A-Z]+$/;

// Is this token one the dictionary can judge? Everything the spec says
// NOT to check filters out here (pure — tested):
//   • length < 2                — single letters ("I", "a") are always fine
//   • non-ASCII letters         — "café": the shipped list is English/ASCII;
//                                 accented words are never flagged
//   • internal apostrophe       — contractions/possessives are punctuation
//                                 territory, not spelling territory
//   • ALL-CAPS                  — acronyms + shouts (OOC, NPC, NO)
//   • a capital after char 0    — mixed case reads as a proper noun
//                                 (McDonald, iPhone) — left alone
//   • digit-adjacent            — "2nd", "hello123": alphanumeric soup
//                                 is not prose spelling
// First-letter capitals ARE checked (lowercased before lookup), so a
// typo opening a sentence still flags — case itself is never the error.
function isCheckableToken(word, text, start, end) {
  if (word.length < 2) return false;
  if (!ASCII_ONLY.test(word) || HAS_APOSTROPHE.test(word)) return false;
  if (IS_UPPER.test(word)) return false;
  if (word.slice(1) !== word.slice(1).toLowerCase()) return false;
  const prev = start > 0 ? text[start - 1] : '';
  const next = end < text.length ? text[end] : '';
  if (prev >= '0' && prev <= '9') return false;
  if (next >= '0' && next <= '9') return false;
  return true;
}

// Tokenize `text` into letter runs with their [start, end) offsets.
// Pure — tested.
export function tokenizeWords(text) {
  const out = [];
  for (const m of String(text).matchAll(TOKEN_RE)) {
    const word = m[0];
    const start = m.index;
    const end = start + word.length;
    out.push({ word, start, end, checkable: isCheckableToken(word, text, start, end) });
  }
  return out;
}

// Dictionary lookup is case-insensitive by design.
export function isMisspelledWord(word, dictSet) {
  return !dictSet.has(word.toLowerCase());
}

// All checkable tokens the dictionary rejects, in order. Pure — tested.
export function findMisspellings(text, dictSet) {
  return tokenizeWords(text).filter((t) => t.checkable && isMisspelledWord(t.word, dictSet));
}

// The token a character index lands in (inclusive of both edge
// boundaries, earliest token wins — forgiving aim at word edges).
// Pure — tested.
export function tokenAtChar(text, charIndex) {
  const idx = Math.max(0, Math.min(charIndex | 0, String(text).length));
  for (const t of tokenizeWords(text)) {
    if (t.start <= idx && idx <= t.end) return t;
  }
  return null;
}

// All single-edit variants of a lowercase ASCII word: deletion,
// adjacent transpose, replacement, insertion. The building block of
// the candidate search — a Set because many edit paths collide.
function edits1(w) {
  const out = new Set();
  for (let i = 0; i < w.length; i++) {
    out.add(w.slice(0, i) + w.slice(i + 1));
    if (i + 1 < w.length) {
      out.add(w.slice(0, i) + w[i + 1] + w[i] + w.slice(i + 2));
    }
    for (let c = 97; c <= 122; c++) {
      const ch = String.fromCharCode(c);
      out.add(w.slice(0, i) + ch + w.slice(i + 1));
      out.add(w.slice(0, i) + ch + w.slice(i));
    }
  }
  for (let c = 97; c <= 122; c++) out.add(w + String.fromCharCode(c));
  out.delete(w);
  return out;
}

// Ranked correction candidates for a misspelled word: generate-and-
// test against the dictionary (dist 1 always; dist 2 for words of 5+
// letters — a 2-edit radius on a 3-letter word buries the fix in
// noise). Ranking within a distance tier: common-word rank first (the
// 10k frequency list), then smallest length delta, then alphabetical —
// deterministic. Pure — tested.
export function suggestWords(word, dictSet, commonRank, limit = 5) {
  const lower = word.toLowerCase();
  if (dictSet.has(lower)) return [];
  const rank = (w) => (commonRank && commonRank.has(w) ? commonRank.get(w) : Infinity);
  const hits = [];
  const seen = new Set();
  const d1 = edits1(lower);
  for (const cand of d1) {
    if (!seen.has(cand) && dictSet.has(cand)) {
      seen.add(cand);
      hits.push({ w: cand, dist: 1 });
    }
  }
  if (lower.length >= 5) {
    for (const e1 of d1) {
      if (Math.abs(e1.length - lower.length) > 1) continue; // a 2nd edit can only shift length by 1 more
      for (const cand of edits1(e1)) {
        if (!seen.has(cand) && dictSet.has(cand)) {
          seen.add(cand);
          hits.push({ w: cand, dist: 2 });
        }
      }
    }
  }
  hits.sort((a, b) =>
    a.dist - b.dist ||
    rank(a.w) - rank(b.w) ||
    Math.abs(a.w.length - lower.length) - Math.abs(b.w.length - lower.length) ||
    (a.w < b.w ? -1 : a.w > b.w ? 1 : 0)
  );
  return hits.slice(0, limit).map((h) => h.w);
}

// Give a replacement the original's first-letter casing ("teh"→"the"
// stays lower, "Teh"→"The"). All-caps tokens never reach this path.
export function matchFirstLetterCase(original, replacement) {
  if (!original || !replacement) return replacement;
  const first = original[0];
  if (first === first.toUpperCase() && first !== first.toLowerCase()) {
    return replacement[0].toUpperCase() + replacement.slice(1);
  }
  return replacement;
}

// Splice a replacement over [start, end) and report the caret position
// just after it. Pure — tested.
export function applySuggestion(value, start, end, replacement) {
  const text = String(value);
  const s = Math.max(0, Math.min(start, text.length));
  const e = Math.max(s, Math.min(end, text.length));
  return {
    text: text.slice(0, s) + replacement + text.slice(e),
    caret: s + replacement.length,
  };
}

// ── Settings (browser) ───────────────────────────────────────────────

const LS_KEY = 'fable.spellcheck.enabled';

export function isSpellcheckEnabled() {
  try {
    return localStorage.getItem(LS_KEY) !== '0'; // ON by default
  } catch {
    return true;
  }
}

function persistEnabled(on) {
  try {
    localStorage.setItem(LS_KEY, on ? '1' : '0');
  } catch {
    /* storage unavailable — the session-scoped toggle still works */
  }
}

// ── Dictionary (browser, lazy) ───────────────────────────────────────

// The word list ships UNBUNDLED in public/spellcheck/ (Vite copies
// public/ verbatim to dist/), so the 4MB list never enters the JS
// bundle and boot never touches it: the fetch fires on the first input
// focus inside Fable, once per process. CRLF-split tolerant.
let dictPromise = null;
let dictState = null;   // the resolved { set, commonRank } (null until loaded)

function fetchWordList(url) {
  return fetch(url)
    .then((r) => {
      if (!r.ok) throw new Error(`${url} → HTTP ${r.status}`);
      return r.text();
    })
    .then((t) => t.split(/\r?\n/));
}

// The legitimate two-letter English words (the Scrabble/OSPD set).
// words_alpha's 2-letter tail is OCR debris ("th", "aq") and the common
// list carries subtitle fragments ("th" again) — a 2-letter entry is
// only a word if this set vouches for it.
const TWO_LETTER_WORDS = new Set((
  'aa ab ad ae ag ah ai al am an ar as at aw ax ay ba be bi bo by da de do ed ef eh el em en er es et ex fa ' +
  'fe gi go ha he hi hm ho id if in is it jo ka ki la li lo ma me mi mm mo mu my na ne no nu od oe of oh oi ' +
  'om on op or os ow ox oy pa pe pi po qi re sh si so ta ti to ug uh um un up us ut we wo xi xu ya ye yo za'
).split(' '));

// ── British spelling variants ────────────────────────────────────────
// The shipped SCOWL build is US-spelling; fantasy prose freely mixes
// British forms (colour, realise, centre, travelled). This curated
// transform derives the UK twin of each listed US stem — explicit
// families only, never a blanket suffix rule (blanket -ize→-ise would
// mint "sise" from "size"). Pure — tested: every rule maps a real US
// word to its real UK counterpart and touches nothing else.
const UK_PREFIX_PAIRS = [
  // -our family
  ['color', 'colour'], ['favor', 'favour'], ['honor', 'honour'],
  ['labor', 'labour'], ['neighbor', 'neighbour'], ['harbor', 'harbour'],
  ['humor', 'humour'], ['rumor', 'rumour'], ['endeavor', 'endeavour'],
  ['savior', 'saviour'], ['behavior', 'behaviour'], ['splendor', 'splendour'],
  ['candor', 'candour'], ['valor', 'valour'], ['odour', 'odour'],
  ['armor', 'armour'], ['parlor', 'parlour'], ['demeanor', 'demeanour'],
  ['vapor', 'vapour'], ['rigor', 'rigour'], ['tumor', 'tumour'],
  // -re family
  ['center', 'centre'], ['meter', 'metre'], ['liter', 'litre'],
  ['fiber', 'fibre'], ['caliber', 'calibre'], ['theater', 'theatre'],
  ['somber', 'sombre'], ['meager', 'meagre'],
  // -ce/-se nouns
  ['defense', 'defence'], ['offense', 'offence'], ['license', 'licence'],
  ['pretense', 'pretence'], ['practice', 'practise'],
  // doubled-l family
  ['traveled', 'travelled'], ['traveling', 'travelling'],
  ['canceled', 'cancelled'], ['canceling', 'cancelling'],
  ['modeled', 'modelled'], ['modeling', 'modelling'],
  ['labeled', 'labelled'], ['labeling', 'labelling'],
  ['marvelous', 'marvellous'], ['fueled', 'fuelled'], ['fueling', 'fuelling'],
  ['signaled', 'signalled'], ['signaling', 'signalling'],
  ['totaled', 'totalled'], ['totaling', 'totalling'],
  ['counselor', 'counsellor'], ['counseling', 'counselling'],
  ['jeweled', 'jewelled'], ['jewelry', 'jewellery'],
  ['enrollment', 'enrolment'], ['fulfill', 'fulfil'], ['skillful', 'skilful'],
  ['willful', 'wilful'], ['instill', 'instil'], ['installment', 'instalment'],
  // one-offs
  ['aluminum', 'aluminium'], ['airplane', 'aeroplane'], ['mold', 'mould'],
  ['molt', 'moult'], ['smolder', 'smoulder'], ['plow', 'plough'],
  ['spelled', 'spelt'], ['learned', 'learnt'], ['dreamed', 'dreamt'],
  ['gray', 'grey'],
];
// -ize/-ization words take -ise/-isation in British English, with a
// short exception family where -ize is not the suffix (size, seize…).
const IZE_EXCEPTIONS = new Set(['size', 'seize', 'capsize', 'prize', 'upsize', 'downsize']);

function britishVariants(word) {
  const out = [];
  // Prefix semantics: the stem AND its inflections/derivations
  // (colors→colours, centered→centred, molder→moulder) all cross over.
  for (const [us, uk] of UK_PREFIX_PAIRS) {
    if (word.startsWith(us)) out.push(uk + word.slice(us.length));
  }
  for (const [suffix, ukSuffix] of [
    ['ization', 'isation'], ['izations', 'isations'],
    ['ized', 'ised'], ['izes', 'ises'],
    ['izing', 'ising'], ['izer', 'iser'], ['izers', 'isers'],
    ['ize', 'ise'],
  ]) {
    if (word.endsWith(suffix) && !IZE_EXCEPTIONS.has(word)) {
      out.push(word.slice(0, -suffix.length) + ukSuffix);
      break; // one suffix family per word — the first match is the most specific
    }
  }
  return out;
}

export function addBritishVariants(set) {
  const additions = [];
  for (const word of set) {
    for (const uk of britishVariants(word)) {
      if (!set.has(uk)) additions.push(uk);
    }
  }
  for (const w of additions) set.add(w);
  return set;
}

// Build the lookup structures from the two shipped word lists. Pure —
// tested. The COMMON list (10k frequency-ranked words) is folded INTO
// the dictionary (every frequency-listed word is valid English), junk
// guards keep out single letters + non-whitelisted 2-letter entries,
// and the curated British transform adds the UK twins the US-spelling
// SCOWL build lacks.
export function buildDictionary(dictLines, commonLines) {
  const commonRank = new Map();
  for (const raw of commonLines) {
    const t = String(raw).trim().toLowerCase();
    if (t && !commonRank.has(t)) commonRank.set(t, commonRank.size);
  }
  const set = new Set();
  const admit = (t) => {
    if (t.length < 2) return;
    if (t.length === 2 && !TWO_LETTER_WORDS.has(t)) return;
    set.add(t);
  };
  for (const w of commonRank.keys()) admit(w);
  for (const raw of dictLines) admit(String(raw).trim().toLowerCase());
  addBritishVariants(set);
  return { set, commonRank };
}

export function ensureDictionary() {
  if (dictPromise) return dictPromise;
  const base = typeof document !== 'undefined' ? document.baseURI : '.';
  dictPromise = Promise.all([
    fetchWordList(new URL('spellcheck/dict-en.txt', base)),
    fetchWordList(new URL('spellcheck/common-en.txt', base)).catch(() => []),
  ]).then(([dictLines, commonLines]) => {
    dictState = buildDictionary(dictLines, commonLines);
    return dictState;
  });
  // A failed load (dev server without public/, a stripped dist) must not
  // poison the process: clear the cached promise so the next right-click
  // retries, and surface the reason in the console exactly once.
  dictPromise.catch((e) => {
    dictPromise = null;
    console.warn('[spellcheck] dictionary load failed:', e && e.message);
  });
  return dictPromise;
}

// ── UI layer (browser only; everything below runs post-init) ────────

const INPUT_SELECTOR =
  'textarea, input[type="text"], input[type="search"], input:not([type])';

// What counts as a spellcheck surface: a text-entry element that isn't
// disabled, inside one of the two zones — FABLE (every written input in
// the #fable app window; bronze marks) or the OS-HOME WUPI CHAT (the
// #chat window's input; purple marks — 2026-08-23 follow-up). Also the
// predicate handed to shell-guard so the guard lets OUR listener see
// the event (the native menu stays preventDefault-ed everywhere — the
// pass-through only re-allows propagation, never the browser menu).
function spellZone(target) {
  if (!target || !target.closest || !target.matches(INPUT_SELECTOR)) return null;
  if (target.disabled) return null;
  if (target.closest('#fable')) return 'fable';
  if (target.closest('#chat')) return 'os';
  return null;
}

function isSpellSurface(target) {
  return spellZone(target) !== null;
}

// Text metrics that must match the field EXACTLY or the mirror's line
// breaks drift from the real wrap. Same list stage.js's autoGrow
// mirror syncs, plus alignment/indent properties.
const SYNC_STYLE_PROPS = [
  'font', 'letterSpacing', 'lineHeight', 'textIndent', 'textTransform',
  'textAlign', 'textJustify', 'tabSize', 'direction', 'unicodeBidi',
  'fontKerning', 'fontVariantLigatures', 'fontFeatureSettings',
];

// el → overlay controller. One overlay per live field.
const overlays = new Map();

let menuEl = null;        // the open context menu (null when closed)
let menuCloser = null;    // transient dismiss wiring while the menu is open
let initialized = false;

function initSpellcheck() {
  if (initialized) return;
  initialized = true;

  // The sanctioned exception to the right-click ban: written inputs in
  // the two spellcheck zones. The guard still preventDefaults (native
  // menu dead); it just stops swallowing propagation so our contextmenu
  // handler below runs.
  setContextMenuPassThrough((e) => isSpellSurface(e.target));

  document.addEventListener(
    'focusin',
    (e) => {
      if (!isSpellcheckEnabled()) return;
      // (2026-08-25 fix) Sweep BEFORE the zone check: fields destroyed
      // while their overlay was HIDDEN (screen teardowns remove inputs
      // without firing focusout) leave dead map entries + parked overlay
      // divs that only a LIVE rAF loop would reap — and with every
      // overlay hidden, no loop is running. Any focusin (spell surface or
      // not) is free user activity to sweep on: the map holds a handful of
      // entries and the scan is a non-cost.
      sweepDisconnected();
      const zone = spellZone(e.target);
      if (zone) attachOverlay(e.target, zone);
    },
    true
  );
  document.addEventListener(
    'focusout',
    (e) => {
      const o = overlays.get(e.target);
      if (o) o.hide();
    },
    true
  );
  document.addEventListener('contextmenu', (e) => {
    if (!isSpellSurface(e.target)) return;
    e.preventDefault();
    openMenuFor(e.target, e.clientX, e.clientY);
  });
}

// ── The underline overlay ────────────────────────────────────────────

function attachOverlay(el, zone) {
  let o = overlays.get(el);
  if (o) {
    o.show();
    return;
  }
  // Kill the NATIVE checker on this field (2026-08-24, Chloe): Chromium's
  // own red squiggle was drawing under the same words as our bronze rule.
  // The attribute only gates the browser's built-in spellcheck — caret /
  // selection / IME behavior and every screen listener are untouched, and
  // OUR checker (dictionary + mirror overlay) is unaffected.
  el.setAttribute('spellcheck', 'false');
  const overlay = document.createElement('div');
  overlay.className = 'fable-spell-overlay' + (zone === 'os' ? ' is-os' : '');
  overlay.setAttribute('aria-hidden', 'true');
  const content = document.createElement('div');
  content.className = 'fable-spell-overlay-content';
  overlay.appendChild(content);
  document.body.appendChild(overlay);

  o = {
    el, overlay, content,
    raf: 0,
    hidden: false,
    destroyed: false,
    lastValue: null,

    syncStyles() {
      const cs = getComputedStyle(el);
      for (const prop of SYNC_STYLE_PROPS) content.style[prop] = cs[prop];
      // (2026-08-24 review P2) A single-line <input> SCROLLS its value
      // horizontally — but the copied white-space makes the mirror WRAP at
      // the field width, so the underline drifted off its word on any line
      // longer than the field. Force `pre` (one long line) so the
      // scrollLeft transform in place() tracks the field exactly.
      // Textareas copy verbatim — their multi-line wrap IS the exact
      // behavior.
      if (el.tagName === 'INPUT') content.style.whiteSpace = 'pre';
      // Content origin = the field's padding AND border (the overlay
      // covers the field's full border-box; it draws no border itself).
      const pad = (side) =>
        `calc(${cs['padding' + side]} + ${cs['border' + side + 'Width']})`;
      content.style.paddingLeft = pad('Left');
      content.style.paddingRight = pad('Right');
      content.style.paddingTop = pad('Top');
      content.style.paddingBottom = pad('Bottom');
    },

    place() {
      const r = el.getBoundingClientRect();
      overlay.style.left = r.left + 'px';
      overlay.style.top = r.top + 'px';
      overlay.style.width = r.width + 'px';
      overlay.style.height = r.height + 'px';
      // Mirror internal scroll (auto-grown composer, tall raw editors).
      content.style.transform = `translate(${-el.scrollLeft}px, ${-el.scrollTop}px)`;
    },

    // Re-render the mirror text with misspelled tokens wrapped in
    // underline spans. Runs on every input — the composer-sized fields
    // this is trivial for; on the largest raw editor it is still a
    // linear tokenize + one pass of DOM nodes.
    rebuild() {
      if (this.destroyed) return;
      this.syncStyles();
      const value = el.value;
      this.lastValue = value;
      const marks = dictState ? findMisspellings(value, dictState.set) : [];
      const frag = document.createDocumentFragment();
      let pos = 0;
      for (const m of marks) {
        if (m.start > pos) frag.appendChild(document.createTextNode(value.slice(pos, m.start)));
        const span = document.createElement('span');
        span.className = 'fable-spell-miss';
        span.textContent = value.slice(m.start, m.end);
        frag.appendChild(span);
        pos = m.end;
      }
      if (pos < value.length) frag.appendChild(document.createTextNode(value.slice(pos)));
      // Trailing '\u200b': a div collapses a block-final '\n' in
      // pre-wrap (same trick as stage.js's autoGrow mirror).
      frag.appendChild(document.createTextNode('\u200b'));
      content.replaceChildren(frag);
      this.place();
    },

    loop() {
      if (this.destroyed || this.hidden) return;
      if (!el.isConnected) {
        this.destroy();
        return;
      }
      sweepDisconnected();
      // Self-heal on programmatic value writes (e.g. Ghost Writer's
      // impersonate drops text into the composer without an input
      // event) — a per-frame string compare is effectively free.
      if (el.value !== this.lastValue) this.rebuild();
      this.place();
      this.raf = requestAnimationFrame(() => this.loop());
    },

    show() {
      if (this.destroyed || !this.hidden) return;
      this.hidden = false;
      overlay.style.display = '';
      this.rebuild();
      this.loop();
    },

    hide() {
      if (this.destroyed || this.hidden) return;
      this.hidden = true;
      cancelAnimationFrame(this.raf);
      overlay.style.display = 'none';
    },

    destroy() {
      if (this.destroyed) return;
      this.destroyed = true;
      cancelAnimationFrame(this.raf);
      overlay.remove();
      overlays.delete(el);
    },
  };

  overlays.set(el, o);
  o.rebuild();
  o.loop();
  el.addEventListener('input', () => o.rebuild());

  // First focus is what triggers the dictionary fetch; when it lands,
  // every live overlay re-renders so the underlines appear.
  ensureDictionary().then(() => {
    if (!o.destroyed && !o.hidden) o.rebuild();
  });
}

function destroyAllOverlays() {
  for (const o of overlays.values()) o.destroy();
  overlays.clear();
}

// Screen teardowns remove inputs without firing focusout, so a HIDDEN
// overlay's rAF loop is already stopped and can never notice its field
// died. The live loops sweep the map for dead entries — overlays are
// few, and the Map scan is a non-cost at frame rate.
function sweepDisconnected() {
  for (const o of overlays.values()) {
    if (!o.el.isConnected) o.destroy();
  }
}

// ── The context menu ─────────────────────────────────────────────────

function closeMenu() {
  if (!menuEl) return;
  menuEl.remove();
  menuEl = null;
  if (menuCloser) {
    menuCloser();
    menuCloser = null;
  }
}

// Measure the on-screen rect of a token through the live mirror overlay
// (2026-08-24, Chloe). The mirror's content is a structural copy of the
// field's value (text nodes + miss spans, offsets aligned), so a DOM Range
// over the token's slice yields the word's TRUE viewport rect — transforms
// (the scroll-sync translate) included. First rect = the word's first
// line (a wrap-spanning word anchors its menu above where it starts).
function measureTokenRect(field, token) {
  const o = overlays.get(field);
  if (!o || o.hidden || !o.content.isConnected) return null;
  let pos = 0;
  for (const node of Array.from(o.content.childNodes)) {
    const len = node.textContent.length;
    if (token.start >= pos + len) { pos += len; continue; }
    const s = Math.max(0, token.start - pos);
    const e = Math.min(len, token.end - pos);
    if (e <= s) return null;
    // Range offsets on an ELEMENT count CHILD NODES, not characters —
    // a miss span (one text child) with setEnd(span, 4) throws
    // IndexSizeError and killed the whole contextmenu handler (the
    // "menu never opens on an underlined word" bug). Measure against
    // the element's single text child instead.
    const target = node.nodeType === Node.TEXT_NODE ? node : node.firstChild;
    if (!target || target.nodeType !== Node.TEXT_NODE) return null;
    const range = document.createRange();
    range.setStart(target, s);
    range.setEnd(target, e);
    const rects = range.getClientRects();
    return rects.length ? rects[0] : null;
  }
  return null;
}

function openMenu(x, y, items, field, wordRect) {
  closeMenu();
  menuEl = document.createElement('div');
  menuEl.className = 'fable-spell-menu';
  menuEl.setAttribute('role', 'menu');
  // Mousedown inside the menu must not steal focus from the field (the
  // replacement's caret math + the underline overlay both ride on it).
  menuEl.addEventListener('mousedown', (e) => e.preventDefault());
  menuEl.style.left = x + 'px';
  menuEl.style.top = y + 'px';
  document.body.appendChild(menuEl);

  for (const item of items) {
    if (item.sep) {
      const sep = document.createElement('div');
      sep.className = 'fable-spell-menu-sep';
      menuEl.appendChild(sep);
      continue;
    }
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'fable-spell-menu-item' + (item.footer ? ' is-footer' : '');
    btn.setAttribute('role', 'menuitem');
    btn.textContent = item.label;
    btn.addEventListener('click', () => item.run(field));
    menuEl.appendChild(btn);
  }

  // Position after the items have laid out. wordRect (2026-08-24): the
  // right-clicked word's measured rect — the menu centers on the word and
  // sits directly ABOVE it. No word (empty field / clean word): the same
  // treatment around the CURSOR point itself (centered on it, above it),
  // viewport-clamped either way.
  requestAnimationFrame(() => {
    if (!menuEl) return;
    const mw = menuEl.offsetWidth;
    const mh = menuEl.offsetHeight;
    const cx = wordRect ? wordRect.left + wordRect.width / 2 : x;
    const topEdge = wordRect ? wordRect.top : y;
    const left = cx - mw / 2;
    const top = topEdge - mh - 8;
    menuEl.style.left = Math.max(8, Math.min(left, window.innerWidth - mw - 8)) + 'px';
    menuEl.style.top = Math.max(8, Math.min(top, window.innerHeight - mh - 8)) + 'px';
  });

  const onPointerDown = (e) => {
    if (menuEl && !menuEl.contains(e.target)) closeMenu();
  };
  const onKeyDown = (e) => {
    if (e.key !== 'Escape') return;
    e.stopPropagation();
    closeMenu();
  };
  const onScrollOrResize = () => closeMenu();
  document.addEventListener('pointerdown', onPointerDown, true);
  document.addEventListener('keydown', onKeyDown, true);
  window.addEventListener('scroll', onScrollOrResize, true);
  window.addEventListener('resize', onScrollOrResize);
  window.addEventListener('blur', onScrollOrResize);
  menuCloser = () => {
    document.removeEventListener('pointerdown', onPointerDown, true);
    document.removeEventListener('keydown', onKeyDown, true);
    window.removeEventListener('scroll', onScrollOrResize, true);
    window.removeEventListener('resize', onScrollOrResize);
    window.removeEventListener('blur', onScrollOrResize);
  };
}

function toggleSpellcheck(field) {
  const next = !isSpellcheckEnabled();
  persistEnabled(next);
  closeMenu();
  if (!next) {
    destroyAllOverlays();
  } else if (field && document.activeElement === field) {
    const zone = spellZone(field);
    if (zone) attachOverlay(field, zone);
  }
}

function openMenuFor(field, x, y) {
  const enabled = isSpellcheckEnabled();
  const toggleItem = {
    label: enabled ? 'Disable' : 'Enable',
    footer: true,
    run: (f) => toggleSpellcheck(f),
  };

  if (!enabled) {
    openMenu(x, y, [toggleItem], field);
    return;
  }

  // Word under the cursor: Chromium moves the caret on mousedown, so
  // by contextmenu time selectionStart sits at the click point.
  const token = tokenAtChar(field.value, field.selectionStart ?? field.value.length);

  // Dictionary not resident yet (first right-click racing the first
  // focus's fetch): open the menu now with just the toggle — the list
  // will be there on the next click; don't block the menu on the load.
  if (!token || !token.checkable || !dictState || !isMisspelledWord(token.word, dictState.set)) {
    openMenu(x, y, [toggleItem], field);
    ensureDictionary().then(() => {
      const o = overlays.get(field);
      if (o && !o.hidden) o.rebuild();
    });
    return;
  }

  const suggestions = suggestWords(token.word, dictState.set, dictState.commonRank);
  const items = suggestions.map((s) => ({
    label: matchFirstLetterCase(token.word, s),
    run: (f) => {
      const fixed = matchFirstLetterCase(token.word, s);
      const { text, caret } = applySuggestion(f.value, token.start, token.end, fixed);
      f.value = text;
      f.focus();
      f.setSelectionRange(caret, caret);
      // Real input event so every screen's own listeners (autoGrow,
      // dirty-state, wizard draft capture) see the correction.
      f.dispatchEvent(new Event('input', { bubbles: true }));
      closeMenu();
    },
  }));
  // Centered above the clicked word (2026-08-24): measure the token's
  // rect through the mirror. A null rect (no live overlay — e.g. the
  // menu raced a blur) falls back to the cursor position.
  openMenu(x, y, [...items, ...(items.length ? [{ sep: true }] : []), toggleItem], field, measureTokenRect(field, token));
}

export { initSpellcheck, INPUT_SELECTOR, isSpellSurface };
