// Unit tests for the slice-regen eligibility predicate (golden pencil,
// 2026-08-11). Plain Node ESM — no test runner. Run:
//   `node tests/slice-regen.test.mjs`
// Exits non-zero on any failure. Mirrors tests/drawer-logic.test.mjs style.
//
// The DOM-dependent pieces (Selection/Range math, pencil positioning) are
// browser-only + exercised manually; this file pins the DOM-free eligibility
// gate that decides whether a resolved selection should show the pencil.
import { strict as assert } from 'node:assert';
import { isSliceEligible } from '../src/fable/engine/slice-regen.js';

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

// ── isSliceEligible (the gate) ─────────────────────────────────────────────
test('assistant beat, clean selection → eligible', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), true);
});

test('user beat → never eligible (AI messages only)', () => {
  assert.equal(isSliceEligible({
    role: 'user',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('editing beat → not eligible (dblclick-to-edit owns it)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: true,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('streaming beat → not eligible (still generating)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: true,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('already-slice-regenerating beat → not eligible (no nested slices)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: true,
    collapsed: false,
    emptyText: false,
  }), false);
});

test('collapsed selection → not eligible (nothing highlighted)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: true,
    emptyText: false,
  }), false);
});

test('whitespace-only selection → not eligible (empty after trim)', () => {
  assert.equal(isSliceEligible({
    role: 'assistant',
    editing: false,
    streaming: false,
    sliceRegenerating: false,
    collapsed: false,
    emptyText: true,
  }), false);
});

test('multiple guards fail at once → still not eligible', () => {
  assert.equal(isSliceEligible({
    role: 'user',
    editing: true,
    streaming: true,
    sliceRegenerating: true,
    collapsed: true,
    emptyText: true,
  }), false);
});

// ── fragmentToText (bug 2: the BR-aware pre/selection/post serializer) ────
// The DOM-free node shapes ({ nodeType, nodeValue, childNodes, tagName })
// mirror what Range.cloneContents() produces for a rendered beat body.
import { fragmentToText } from '../src/fable/engine/slice-regen.js';

const t3 = (v) => ({ nodeType: 3, nodeValue: v, childNodes: [] });
const el = (tag, ...kids) => ({ nodeType: 1, tagName: tag, childNodes: kids });
const br = () => el('BR');

test('fragmentToText: plain text passes through', () => {
  assert.equal(fragmentToText(el('DIV', t3('hello world'))), 'hello world');
});

test('fragmentToText: <br> becomes a newline (the bug-2 core)', () => {
  // beats.renderMarkdown renders a newline as <br>; Range.toString() drops
  // them. A two-paragraph beat must reconstruct with its paragraph break.
  const frag = el('DIV', t3('first paragraph'), br(), t3('second paragraph'));
  assert.equal(fragmentToText(frag), 'first paragraph\nsecond paragraph');
});

test('fragmentToText: consecutive <br> each contribute a newline', () => {
  const frag = el('DIV', t3('a'), br(), br(), t3('b'));
  assert.equal(fragmentToText(frag), 'a\n\nb');
});

test('fragmentToText: structured inline markup recurses (strong/em/quote spans)', () => {
  const frag = el('DIV',
    t3('He drew '),
    el('STRONG', t3('the blade'), br(), t3('and smiled')),
    t3('.'));
  assert.equal(fragmentToText(frag), 'He drew the blade\nand smiled.');
});

test('fragmentToText: leading/trailing <br>, empty fragment, bare fragment', () => {
  assert.equal(fragmentToText(el('DIV', br(), t3('x'))), '\nx');
  assert.equal(fragmentToText(el('DIV', t3('x'), br())), 'x\n');
  assert.equal(fragmentToText(el('DIV')), '');
  // A bare fragment node (what cloneContents returns) works too.
  assert.equal(fragmentToText({ nodeType: 11, childNodes: [t3('a'), br(), t3('b')] }), 'a\nb');
});

test('fragmentToText: markdown-flattening trade-off holds (markers lost, text kept)', () => {
  // **bold** renders as <strong> — visible text keeps inner text only. The
  // documented accepted trade-off; the paragraph breaks are what must
  // survive (they did not before the fix).
  const frag = el('DIV', el('STRONG', t3('bold line')), br(), t3('plain line'));
  assert.equal(fragmentToText(frag), 'bold line\nplain line');
});

console.log('\n%d passed, %d failed', passed, failed);
process.exit(failed ? 1 : 0);
