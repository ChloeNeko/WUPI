// Unit tests for the pure gen-done render-origin router (PRISM, 2026-08-20
// audit M8). Plain Node ESM — no test runner. Run:
// `node tests/prism-routing.test.mjs`. Exits non-zero on any failure so it
// can gate CI.
//
// The routing DECISION lives in src/prism/engine/routing.js
// (resolveGenDoneTarget); prism.js applies it (composer origin → re-enable
// the Generate button, fork origin → swap B + clear .is-rendering). The
// decision takes NO active-screen input — that is the pinned invariant: a
// completion routes to the render's ORIGIN, never to whichever screen is
// visible when the (multi-second) SD swap finishes.
import { strict as assert } from 'node:assert';
import { resolveGenDoneTarget } from '../src/prism/engine/routing.js';

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

const ok = (path) => ({ ok: true, image: { path } });

// ── The M8 misroutes, pinned ──────────────────────────────────────────────

test('composer render done while Fork is active → routes to composer (fork untouched)', () => {
  // The user clicked Generate on Compose, then navigated to Fork while the
  // swap cycle ran. Origin routing must answer "composer" — the old
  // active-screen router answered "fork" and swapped Fork's B layer.
  const renders = [{ origin: 'composer', path: '/gallery/a.png' }];
  const t = resolveGenDoneTarget(renders, ok('/gallery/a.png'));
  assert.deepEqual(t, { origin: 'composer', index: 0 });
});

test('fork render done elsewhere → routes to fork (is-rendering cleared there)', () => {
  // The user clicked Regenerate B, then navigated to Compose/Gallery. The
  // old router never reached forkOnGenDone, leaving .is-rendering stuck.
  const renders = [{ origin: 'fork', path: '/gallery/b.png' }];
  const t = resolveGenDoneTarget(renders, ok('/gallery/b.png'));
  assert.deepEqual(t, { origin: 'fork', index: 0 });
});

test('unknown token ignored (stale/post-close event routes nowhere)', () => {
  const renders = [{ origin: 'composer', path: '/gallery/a.png' }];
  assert.equal(resolveGenDoneTarget(renders, ok('/gallery/zzz.png')), null);
});

test('no outstanding renders → null (post-close done event ignored)', () => {
  assert.equal(resolveGenDoneTarget([], ok('/gallery/a.png')), null);
  assert.equal(resolveGenDoneTarget(null, ok('/gallery/a.png')), null);
  assert.equal(resolveGenDoneTarget([], { ok: false, error: 'boom' }), null);
});

// ── Token pairing ─────────────────────────────────────────────────────────

test('pairs by path among stacked renders, reporting the right index', () => {
  // Two outstanding renders (a composer pending + a queued fork): the done
  // event names ITS render by the dest path, not by queue position.
  const renders = [
    { origin: 'composer', path: '/gallery/a.png' },
    { origin: 'fork', path: '/gallery/b.png' },
  ];
  const t = resolveGenDoneTarget(renders, ok('/gallery/b.png'));
  assert.deepEqual(t, { origin: 'fork', index: 1 });
});

test('two stacked fork renders pair by path (order-independent)', () => {
  const renders = [
    { origin: 'fork', path: '/gallery/b1.png' },
    { origin: 'fork', path: '/gallery/b2.png' },
  ];
  assert.deepEqual(
    resolveGenDoneTarget(renders, ok('/gallery/b2.png')),
    { origin: 'fork', index: 1 }
  );
  assert.deepEqual(
    resolveGenDoneTarget(renders, ok('/gallery/b1.png')),
    { origin: 'fork', index: 0 }
  );
});

test('insert-failure payload ({ok:false, path}) still pairs by path', () => {
  // The Rust insert-failure branch is the one failure shape that carries
  // the dest path — it must reach the render it belongs to.
  const renders = [
    { origin: 'composer', path: '/gallery/a.png' },
    { origin: 'fork', path: '/gallery/b.png' },
  ];
  const t = resolveGenDoneTarget(renders, {
    ok: false, error: 'gallery row could not be saved', path: '/gallery/b.png',
  });
  assert.deepEqual(t, { origin: 'fork', index: 1 });
});

// ── Pathless failures + the attach race ───────────────────────────────────

test('pathless failure → FIFO head (the SD turn lock serializes swaps in start order)', () => {
  const renders = [
    { origin: 'composer', path: '/gallery/a.png' },
    { origin: 'fork', path: '/gallery/b.png' },
  ];
  const t = resolveGenDoneTarget(renders, { ok: false, error: 'Generation failed' });
  assert.deepEqual(t, { origin: 'composer', index: 0 });
});

test('cancelled failure (no path) → FIFO head too', () => {
  const renders = [{ origin: 'fork', path: '/gallery/b.png' }];
  const t = resolveGenDoneTarget(renders, { ok: false, error: 'cancelled', cancelled: true });
  assert.deepEqual(t, { origin: 'fork', index: 0 });
});

test('attach race: untagged renders + a path-bearing done → FIFO head, not dropped', () => {
  // Every tag is still path-null (the generate invoke's path-attach hasn't
  // landed) when the fastest stub done arrives — a completion must never be
  // discarded just because its token isn't attached yet.
  const renders = [{ origin: 'composer', path: null }];
  const t = resolveGenDoneTarget(renders, ok('/gallery/a.png'));
  assert.deepEqual(t, { origin: 'composer', index: 0 });
});

test('null/garbage payload → FIFO head when a render is outstanding', () => {
  // Defensive: a malformed event still frees the head render (the caller's
  // failure branch resets both surfaces anyway).
  const renders = [{ origin: 'fork', path: null }];
  assert.deepEqual(resolveGenDoneTarget(renders, null), { origin: 'fork', index: 0 });
  assert.deepEqual(resolveGenDoneTarget(renders, {}), { origin: 'fork', index: 0 });
});

// ── Report ────────────────────────────────────────────────────────────────

console.log(`\nprism-routing: ${passed} passed, ${failed} failed`);
process.exit(failed > 0 ? 1 : 0);
