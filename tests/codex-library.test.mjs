// Pure-helper tests for the codex library screen (title LOAD → CODEX,
// 2026-08-23): the links-map inverse index the tiles read + the LINK
// popup's per-card write plan. The screen module's only import is the
// Tauri API core (safe to load outside the WebView — the helpers are pure
// and touch no DOM). Plain Node ESM. Run: `node --test tests/codex-library.test.mjs`.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  buildCodexLinkIndex,
  computeLinkWrites,
} from '../src/fable/screens/codex-library.js';

// ── buildCodexLinkIndex (fable_codex_links_map rows → codex → cards) ────

test('buildCodexLinkIndex: folds rows into codex → linking cards', () => {
  const index = buildCodexLinkIndex([
    { card_id: 'liam', card_name: 'Liam', codices: ['Lore', 'Bestiary'] },
    { card_id: 'cinderfen', card_name: 'Cinderfen', codices: ['lore'] }, // case fold
    { card_id: 'ghost', card_name: 'Ghost', codices: [] },
  ]);
  assert.deepEqual(index.get('lore'), [
    { cardId: 'liam', cardName: 'Liam' },
    { cardId: 'cinderfen', cardName: 'Cinderfen' },
  ]);
  assert.deepEqual(index.get('bestiary'), [{ cardId: 'liam', cardName: 'Liam' }]);
  assert.equal(index.has('ghost'), false);
});

test('buildCodexLinkIndex: null/empty rows never throw', () => {
  assert.equal(buildCodexLinkIndex(null).size, 0);
  assert.equal(buildCodexLinkIndex([]).size, 0);
  assert.equal(buildCodexLinkIndex([{ card_id: 'x', card_name: 'X', codices: null }]).size, 0);
  // A blank/whitespace link name is dropped, not keyed.
  assert.equal(buildCodexLinkIndex([{ card_id: 'x', card_name: 'X', codices: ['  '] }]).size, 0);
});

// ── computeLinkWrites (the LINK popup's save plan) ───────────────────────

test('computeLinkWrites: new link appends at LOWEST priority', () => {
  const cards = [{ id: 'liam' }];
  const links = new Map([['liam', ['Lore']]]);
  const writes = computeLinkWrites('Bestiary', cards, links, ['liam']);
  assert.deepEqual(writes, [{ cardId: 'liam', codices: ['Lore', 'Bestiary'] }]);
});

test('computeLinkWrites: kept link → NO write (priority position preserved)', () => {
  // Case-insensitive match against the stored name — the kept card's list
  // is never re-emitted, so its authored order survives byte-for-byte.
  const cards = [{ id: 'liam' }];
  const links = new Map([['liam', ['Lore', 'Bestiary']]]);
  assert.deepEqual(computeLinkWrites('bestiary', cards, links, ['liam']), []);
});

test('computeLinkWrites: deselection removes ONLY this codex', () => {
  const cards = [{ id: 'liam' }];
  const links = new Map([['liam', ['Lore', 'Bestiary', 'Maps']]]);
  const writes = computeLinkWrites('Bestiary', cards, links, []);
  assert.deepEqual(writes, [{ cardId: 'liam', codices: ['Lore', 'Maps'] }]);
});

test('computeLinkWrites: unlinked + deselected → no write (unlink-all never fires)', () => {
  // A card that never linked the codex and stays deselected must NOT be
  // written: fable_codex_link_set([]) on that card would unlink EVERYTHING.
  const cards = [{ id: 'liam' }];
  const links = new Map([['liam', ['Lore']]]);
  assert.deepEqual(computeLinkWrites('Bestiary', cards, links, []), []);
  // Even a card with NO other links is untouched.
  const links2 = new Map([['liam', []]]);
  assert.deepEqual(computeLinkWrites('Bestiary', [{ id: 'liam' }], links2, []), []);
});

test('computeLinkWrites: full unlink of just this codex keeps other links', () => {
  // The ONLY remaining link removed → the write carries an empty list (the
  // legitimate unlink-everything signal), never a dangling name.
  const cards = [{ id: 'liam' }];
  const links = new Map([['liam', ['Bestiary']]]);
  const writes = computeLinkWrites('Bestiary', cards, links, []);
  assert.deepEqual(writes, [{ cardId: 'liam', codices: [] }]);
});

test('computeLinkWrites: mixed selection — only changed cards written', () => {
  const cards = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];
  const links = new Map([
    ['a', ['Lore', 'Bestiary']], // stays linked → untouched
    ['b', ['Lore']],             // newly selected → append
    ['c', ['Bestiary', 'Lore']], // deselected → removed, rest order kept
  ]);
  const writes = computeLinkWrites('Bestiary', cards, links, ['a', 'b']);
  assert.deepEqual(writes, [
    { cardId: 'b', codices: ['Lore', 'Bestiary'] },
    { cardId: 'c', codices: ['Lore'] },
  ]);
});
