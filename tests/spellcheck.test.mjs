// Unit tests for the Fable spellchecker's pure surfaces (2026-08-23).
// Plain Node ESM — no test runner. Run: `node tests/spellcheck.test.mjs`.
// Exits non-zero on any failure. Mirrors tests/ghost-writer.test.mjs style.
//
// The DOM pieces (the mirror overlay, the right-click menu, focus/
// scroll sync) are browser-only + exercised manually; this file pins
// the tokenizer's checkability rules (Chloe's spec: words only — no
// punctuation, no capitalization, no acronyms/digit-glue/accented
// words), the suggestion search + ranking, and the replacement math.
// It also verifies the SHIPPED dictionary files exist + parse, so a
// cleanup pass can't silently delete them from public/bin/.
import { strict as assert } from 'node:assert';
import { readFileSync } from 'node:fs';
import {
  tokenizeWords,
  isMisspelledWord,
  findMisspellings,
  tokenAtChar,
  suggestWords,
  matchFirstLetterCase,
  applySuggestion,
  buildDictionary,
  addBritishVariants,
} from '../src/fable/engine/spellcheck.js';

let passed = 0;
let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log('  ok   %s', name);
    passed++;
  } catch (e) {
    console.error('  FAIL %s\n       %s', name, e.message);
    failed++;
  }
}

// A tiny stand-in dictionary + common-rank map for the pure tests.
const DICT = new Set([
  'the', 'of', 'and', 'hello', 'world', 'work', 'fork', 'week', 'workout',
  'definitely', 'fine', 'deaf', 'define', 'delegate', 'tea',
  'ten', 'then', 'them', 'they', 'brazier', 'gilded', 'palisade', 'cafe',
  'quill',
]);
const COMMON = new Map([['the', 0], ['and', 2], ['they', 20], ['them', 30], ['then', 40]]);

// ── tokenizeWords ──────────────────────────────────────────────────────
test('tokenizes letter runs with exact offsets', () => {
  const toks = tokenizeWords('Hello, wrold!');
  assert.deepEqual(toks.map((t) => t.word), ['Hello', 'wrold']);
  assert.equal(toks[0].start, 0);
  assert.equal(toks[0].end, 5);
  assert.equal(toks[1].start, 7);
  assert.equal(toks[1].end, 12);
});

test('contractions + possessives tokenize as ONE token (not flagged)', () => {
  const toks = tokenizeWords("shouldn't Liam's");
  assert.deepEqual(toks.map((t) => t.word), ["shouldn't", "Liam's"]);
  assert.ok(toks.every((t) => !t.checkable));
});

test('checkable: plain words, incl. first-letter capitals', () => {
  const toks = tokenizeWords('hello Teh wrold');
  assert.deepEqual(toks.map((t) => t.checkable), [true, true, true]);
});

test('NOT checkable: single letters', () => {
  const toks = tokenizeWords('I am a tester');
  assert.deepEqual(toks.map((t) => t.checkable), [false, true, false, true]);
});

test('NOT checkable: ALL-CAPS (acronyms + shouts)', () => {
  const toks = tokenizeWords('OOC NPC NO HELLO');
  assert.ok(toks.every((t) => !t.checkable));
});

test('NOT checkable: internal capitals (proper-noun shapes)', () => {
  const toks = tokenizeWords('McDonald iPhone iPad');
  assert.ok(toks.every((t) => !t.checkable));
});

test('NOT checkable: accented words (dictionary is English/ASCII)', () => {
  const toks = tokenizeWords('café naïve façade');
  assert.ok(toks.every((t) => !t.checkable));
});

test('NOT checkable: digit-adjacent runs (2nd, hello123world)', () => {
  const toks = tokenizeWords('the 2nd hello123world end');
  // "the" ✓, "nd" glued to a digit ✗, "hello"/"world" glued to digits ✗,
  // "end" ✓ — digit-glue never flags, punctuation-adjacent digits are not words.
  assert.deepEqual(toks.map((t) => t.checkable), [true, false, false, false, true]);
});

test('punctuation is never a token + never blocks a word', () => {
  const toks = tokenizeWords('"Quote," (parenthetical) -- dash; end.');
  assert.ok(toks.every((t) => /^[A-Za-z]+$/.test(t.word)));
  assert.ok(toks.some((t) => t.word === 'Quote' && t.checkable));
});

// ── isMisspelledWord / findMisspellings ────────────────────────────────
test('dictionary lookup is case-insensitive (case is never the error)', () => {
  assert.equal(isMisspelledWord('The', DICT), false);
  assert.equal(isMisspelledWord('the', DICT), false);
  assert.equal(isMisspelledWord('teh', DICT), true);
});

test('findMisspellings returns in-order ranges over a sentence', () => {
  const marks = findMisspellings('The wrold of teh quill', DICT);
  assert.deepEqual(marks.map((m) => m.word), ['wrold', 'teh']);
  assert.equal(marks[0].start, 4);
  assert.equal(marks[0].end, 9);
  assert.equal(marks[1].start, 13);
  assert.equal(marks[1].end, 16);
});

// ── tokenAtChar ────────────────────────────────────────────────────────
test('tokenAtChar resolves the word under a click index', () => {
  const text = 'the wrold';
  assert.equal(tokenAtChar(text, 5).word, 'wrold');   // mid-word
  assert.equal(tokenAtChar(text, 4).word, 'wrold');   // left edge
  assert.equal(tokenAtChar(text, 9).word, 'wrold');   // right edge (inclusive)
  assert.equal(tokenAtChar(text, 0).word, 'the');     // very start
  assert.equal(tokenAtChar(text, 99).word, 'wrold');  // clamps to the end → last word
  assert.equal(tokenAtChar('', 0), null);             // empty field → none
  assert.equal(tokenAtChar('wrold!', 6), null);       // on trailing punctuation → none
});

// ── suggestWords ───────────────────────────────────────────────────────
test('suggests the common-word fix first ("teh" → "the")', () => {
  const s = suggestWords('teh', DICT, COMMON);
  assert.ok(s.length > 0);
  assert.equal(s[0], 'the');
});

test('one-edit fixes land within the candidate set ("wrok" → "work")', () => {
  const s = suggestWords('wrok', DICT, COMMON);
  assert.ok(s.includes('work'));
});

test('two-edit radius engages at 5+ letters ("definately" → "definitely")', () => {
  const s = suggestWords('definately', DICT, COMMON);
  assert.ok(s.includes('definitely'));
});

test('a dictionary word yields no suggestions', () => {
  assert.deepEqual(suggestWords('hello', DICT, COMMON), []);
});

test('respects the limit + is deterministic across calls', () => {
  const a = suggestWords('wrokd', DICT, COMMON, 3);
  const b = suggestWords('wrokd', DICT, COMMON, 3);
  assert.deepEqual(a, b);
  assert.ok(a.length <= 3);
});

// ── matchFirstLetterCase / applySuggestion ─────────────────────────────
test('replacement inherits the original first-letter casing', () => {
  assert.equal(matchFirstLetterCase('teh', 'the'), 'the');
  assert.equal(matchFirstLetterCase('Teh', 'the'), 'The');
  // All-caps tokens are never checkable, so this path is unreachable in
  // the UI — the function still applies the only rule it knows:
  assert.equal(matchFirstLetterCase('WROKD', 'work'), 'Work');
});

test('applySuggestion splices the fix + reports the caret after it', () => {
  const r = applySuggestion('The wrold of teh quill', 4, 9, 'world');
  assert.equal(r.text, 'The world of teh quill');
  assert.equal(r.caret, 9);
  const r2 = applySuggestion('Hello, teh end', 7, 10, 'the');
  assert.equal(r2.text, 'Hello, the end');
  assert.equal(r2.caret, 10);
});

test('applySuggestion clamps out-of-range spans safely', () => {
  const r = applySuggestion('abc', 100, 200, 'x');
  assert.equal(r.text, 'abcx');
  assert.equal(r.caret, 4);
});

// ── buildDictionary ────────────────────────────────────────────────────
test('buildDictionary unions the common list in (common-only words enter the dictionary)', () => {
  // "th" rides BOTH lists (words_alpha OCR debris + a subtitle fragment
  // in the common list) — only the 2-letter whitelist can keep it out.
  const { set } = buildDictionary(['hello', 'th', 'aq', 'e', 'of', 'world'], ['the', 'weird', 'of', 'th']);
  assert.ok(set.has('hello'));
  assert.ok(set.has('weird'));   // common-only word — words_alpha lacks it
  assert.ok(set.has('the'));
  assert.ok(set.has('of'));      // legit 2-letter word survives the filter
  // Junk guards: single letters never enter; 2-letter debris ("th",
  // "aq") never enters, even common-ranked debris.
  assert.ok(!set.has('th'));
  assert.ok(!set.has('aq'));
  assert.ok(!set.has('e'));
});

test('buildDictionary ranks common words by file order', () => {
  const { commonRank } = buildDictionary([], ['the', 'of', 'and', 'the']);
  assert.deepEqual([...commonRank.keys()], ['the', 'of', 'and']);
  assert.equal(commonRank.get('the'), 0);
});

// ── British variants ───────────────────────────────────────────────────
test('the US→UK transform derives real British twins (colour, realise, centre…)', () => {
  const set = new Set([
    'color', 'colors', 'colored', 'coloring', 'realize', 'realized',
    'realization', 'center', 'defense', 'traveled', 'organize',
    'size', 'seize', 'capsize', 'tenor', 'motor', 'gray',
  ]);
  addBritishVariants(set);
  for (const uk of [
    'colour', 'colours', 'coloured', 'colouring',          // -our family, inflected
    'realise', 'realised', 'realisation', 'organise',      // -ise family
    'centre', 'defence', 'travelled',                      // -re / -ce / doubled-l
    'grey',                                                // one-off pair
  ]) {
    assert.ok(set.has(uk), `missing British variant "${uk}"`);
  }
  // Blanket-suffix protection: no invented words from non-suffix -ize,
  // no -our added to words outside the curated families.
  for (const junk of ['sise', 'seise', 'capsise', 'tenour', 'motorour']) {
    assert.ok(!set.has(junk), `invented variant "${junk}"`);
  }
});

// ── the shipped dictionary files ───────────────────────────────────────
// The checker is only as good as the two files in public/bin/ (Vite
// copies them to dist/ unbundled → they ship in the install's bin/).
// This pins their presence + basic shape so a cleanup can't silently
// strip them.
test('public/bin/ dictionary files build a sane writer dictionary', () => {
  const dictLines = readFileSync(new URL('../public/bin/dict-en.txt', import.meta.url), 'utf8')
    .split(/\r?\n/);
  const commonLines = readFileSync(new URL('../public/bin/common-en.txt', import.meta.url), 'utf8')
    .split(/\r?\n/);
  assert.ok(dictLines.length > 200000, `dictionary suspiciously small: ${dictLines.length}`);
  assert.ok(commonLines.length >= 9000, `common list suspiciously small: ${commonLines.length}`);
  assert.equal(commonLines[0].trim().toLowerCase(), 'the');
  const { set } = buildDictionary(dictLines, commonLines);
  // Real words present (incl. the literary/fantasy register + SCOWL 80's
  // depth: surcoat, portcullis) + the British twins the transform adds…
  for (const w of [
    'the', 'weird', 'brazier', 'gilded', 'palisade', 'cafe', 'surcoat',
    'portcullis', 'colour', 'honour', 'realise', 'centre', 'travelled',
  ]) {
    assert.ok(set.has(w), `missing "${w}"`);
  }
  // …classic misspellings absent, and the 2-letter debris filtered.
  for (const w of ['teh', 'wrok', 'definately', 'th']) assert.ok(!set.has(w), `"${w}" present`);
});

// ── summary ────────────────────────────────────────────────────────────
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
