// =============================================================
// FABLE RAW EDITOR — the centered, immovable raw-file modal.
//
// Opened by the ✎ icon on each tab dropdown (engine/tab-rail.js). Shows the
// tab's file in raw JSON/XML. Three controls top-right:
//   ✓  save     — validate + atomic-write. On success: snapshot the saved
//                 text as the new "last-good" + close.
//   ↻  revert   — reset the textarea to the JS-held last-good text (no IPC).
//                 Always works (even on invalid).
//   ✕  close    — discard the working text → restore last-good → close.
//                 Unconditional (always works, even on invalid).
//
// THE VALIDATION LOCK: on every input the client runs a pre-check (JSON.parse
// for the JSON tabs; a basic XML well-formedness sniff for the .sim tab). If
// invalid: ✓ is DISABLED + red outline; ✕ refuses to close (stays open with a
// "fix errors or revert" hint); only ↻ and ✓-after-fix work. The authoritative
// gate re-runs server-side on the *_raw_set IPC → an Err keeps the modal open
// and shows the server message.
//
// CRASH / ALT+F4 SAFETY: the file is written ONLY on ✓ via atomic temp+rename
// (backend write_atomic). A crash before ✓ = no write happened = the file is
// the last-good on disk. The JS last-good is for in-session revert; on a true
// process kill the on-disk last-good (untouched until ✓) is the source of
// truth. No .bak/backup files — atomic rename IS the safety net (house style).
//
// The overlay darkens the full stage so only the modal is in focus. It sits
// inside .fable-stage (below the OS-level Wupi top bar); exiting Fable from
// the top bar treats this as ✕ (teardownStage closes the modal; the file is
// whatever last successfully saved).
// =============================================================

import { invoke } from '@tauri-apps/api/core';
// (2026-08-16 bug 5) In-flight narrator guard — see onSave.
import * as narrator from './narrator.js';

// kind → { title, isXml, read(), save(text) }. The read/save pair selects the
// matching backend IPC. `isXml` chooses the client-side pre-check (XML sniff
// for the .sim tab; JSON.parse otherwise).
const FILE_FOR = {
  card:   { title: 'Sim Card (.sim)',   isXml: true,  read: () => invoke('fable_card_raw_get'), save: (t) => invoke('fable_card_raw_set', { xml: t }) },
  codex:  { title: 'Codex (.codex)',     isXml: false, read: async () => (await invoke('fable_codex_get')).raw, save: (t) => invoke('fable_codex_raw_set', { text: t }) },
  world:  { title: 'World (world.json)', isXml: false, read: () => invoke('fable_json_raw_get', { kind: 'world' }),  save: (t) => invoke('fable_json_raw_set', { kind: 'world',  json: t }) },
  player: { title: 'Player (player.json)', isXml: false, read: () => invoke('fable_json_raw_get', { kind: 'player' }), save: (t) => invoke('fable_json_raw_set', { kind: 'player', json: t }) },
  npc:    { title: 'NPC (npc.json)',     isXml: false, read: () => invoke('fable_json_raw_get', { kind: 'npc' }),     save: (t) => invoke('fable_json_raw_set', { kind: 'npc',     json: t }) },
};

let overlayEl = null;     // the .fable-raw-editor-overlay
let titleEl = null;       // the modal title bar
let textareaEl = null;    // the editor textarea
let saveBtn = null;
let revertBtn = null;
let closeBtn = null;
let current = null;       // the active FILE_FOR entry
let lastGood = '';        // the last successfully-saved text (JS-held revert target)
let isValid = true;       // client-side validity flag (drives the lock)
let onSavedCb = null;     // optional: refresh the dropdown after a save

// Build the modal DOM once (called from stage.js buildStage). Hidden by
// default. Returns the overlay element for stage.js to mount.
export function buildRawEditor() {
  overlayEl = document.createElement('div');
  overlayEl.className = 'fable-raw-editor-overlay';
  overlayEl.hidden = true;
  overlayEl.innerHTML = `
    <div class="fable-raw-editor-backdrop"></div>
    <div class="fable-raw-editor-modal" role="dialog" aria-modal="true">
      <div class="fable-raw-editor-head">
        <span class="fable-raw-editor-title" data-raw-title></span>
        <div class="fable-raw-editor-controls">
          <button class="fable-raw-btn save"  data-raw-save   title="Save"   aria-label="Save">✓</button>
          <button class="fable-raw-btn revert" data-raw-revert title="Revert" aria-label="Revert to last saved">↻</button>
          <button class="fable-raw-btn close" data-raw-close  title="Close"  aria-label="Close without saving">✕</button>
        </div>
      </div>
      <textarea class="fable-raw-editor-text" data-raw-text spellcheck="false"></textarea>
    </div>
  `;
  titleEl = overlayEl.querySelector('[data-raw-title]');
  textareaEl = overlayEl.querySelector('[data-raw-text]');
  saveBtn = overlayEl.querySelector('[data-raw-save]');
  revertBtn = overlayEl.querySelector('[data-raw-revert]');
  closeBtn = overlayEl.querySelector('[data-raw-close]');

  textareaEl.addEventListener('input', () => revalidate());
  saveBtn.addEventListener('click', onSave);
  revertBtn.addEventListener('click', onRevert);
  closeBtn.addEventListener('click', onClose);
  // Backdrop click → same as ✕ (close→last-good). Blocked when invalid (the
  // lock: a backdrop click can't discard an invalid edit either — must ↻ or
  // fix). Matches the ✕ rule so there's no escape hatch.
  overlayEl.querySelector('.fable-raw-editor-backdrop').addEventListener('click', () => {
    if (isOpen()) onClose();
  });
  return overlayEl;
}

// Open the editor for a file kind. Loads the current file text as both the
// textarea content AND the initial last-good. `onSaved` (optional) lets the
// caller refresh its dropdown view after a save (e.g. the tab rail re-reads).
export async function openRawEditor(kind, onSaved) {
  const file = FILE_FOR[kind];
  if (!file || !overlayEl) return;
  saveEpoch++;               // invalidate any save still resolving from a prior session
  current = file;
  onSavedCb = onSaved || null;
  titleEl.textContent = file.title;
  textareaEl.value = '';
  overlayEl.hidden = false;
  try {
    const text = await file.read();
    // card/codex reads can return empty for a fresh file; that's valid (the
    // editor starts blank for a new file). JSON tabs return '' too.
    textareaEl.value = text || '';
    lastGood = text || '';
    revalidate();
    setTimeout(() => textareaEl.focus(), 30);
  } catch (err) {
    console.warn('[fable] raw editor load failed', err);
    // (2026-08-16 audit LOW) Close + reset on a failed read. The old path
    // left an EMPTY open modal carrying the PREVIOUS session's `lastGood`
    // + validity — a stale-true ✓ enabled a save that would overwrite the
    // real file with nothing.
    overlayEl.hidden = true;
    current = null;
    onSavedCb = null;
    isValid = true;
    lastGood = '';
    saveBtn.disabled = false;
  }
}

export function isOpen() {
  return overlayEl && !overlayEl.hidden;
}

// Esc handler (called from stage.js's keydown chain, highest priority). On
// Esc: if valid → close (same as ✕); if invalid → refuse + flash the hint
// (Esc can't bypass the validation lock — only ↻ or fixing the text works).
export function onEsc() {
  if (!isOpen()) return false;
  if (isValid) {
    onClose();
  } else {
    textareaEl.classList.add('shake');
    setTimeout(() => textareaEl.classList.remove('shake'), 320);
  }
  return true;  // handled (intercepted)
}

// ── controls ────────────────────────────────────────────────────────────
// Ownership token for in-flight saves: a ✕ (or Esc / backdrop / teardown /
// reopen) while `current.save(text)` is still resolving must invalidate that
// save's continuation — otherwise the stale resolve closes the FRESH editor
// + stomps its last-good. Bumped on every open + close; the save only
// commits its results when its epoch is still current.
let saveEpoch = 0;

async function onSave() {
  if (!current || !isValid) return;
  // (2026-08-16 bug 5) Refuse while a narrator turn is in flight — the same
  // M5 discipline the inventory panel got. The editor's text is an OPEN-TIME
  // snapshot: a tracker turn landing behind the modal mutates the live
  // schema, and fable_json_raw_set recomposes from LIVE state while
  // overwriting every key this JSON carries — the turn's mutations
  // (world_clock/weather/injuries/[EQUIP]s) silently roll back and the undo
  // ring records the stale state as "prior". Shake (the validation-lock
  // feedback gesture) + keep the modal open; retry after the beat lands.
  if (narrator.isGenerating()) {
    console.warn('[fable] raw editor save refused: a narrator turn is in flight — wait for the beat to land');
    textareaEl.classList.add('shake');
    setTimeout(() => textareaEl.classList.remove('shake'), 320);
    return;
  }
  const text = textareaEl.value;
  const epoch = ++saveEpoch;
  saveBtn.disabled = true;
  try {
    await current.save(text);
    if (epoch !== saveEpoch) return; // superseded by a close/reopen mid-save
    lastGood = text;             // snapshot the saved text as the new last-good
    if (onSavedCb) try { onSavedCb(); } catch (_) {}
    closeModal();                // ✓ on success closes (per spec)
  } catch (err) {
    if (epoch !== saveEpoch) return; // the editor it failed in is gone
    // Server-side validation rejected it (e.g. the XML sniff missed something
    // the real parser catches). Keep the modal open; the file is untouched
    // (write_atomic only runs after the backend parse succeeds). Status bar
    // removed 2026-08-12 per Chloe — failures log silently.
    console.warn('[fable] raw editor save failed', err);
    saveBtn.disabled = false;
  }
}

function onRevert() {
  // Reset the textarea to the last-good text. Always works (no validity gate).
  textareaEl.value = lastGood;
  revalidate();
  textareaEl.focus();
}

function onClose() {
  // ✕ discards the working text → restores last-good → closes. Unconditional
  // (the spec: X closes to the last successfully-saved file). The validation
  // lock is about not SAVING invalid text, not trapping the user — ✕ always
  // lets them escape to the last-good state.
  textareaEl.value = lastGood;
  closeModal();
}

function closeModal() {
  if (!overlayEl) return;
  saveEpoch++;               // any in-flight save's continuation is now stale
  overlayEl.hidden = true;
  current = null;
  onSavedCb = null;
  textareaEl.value = '';
  lastGood = '';
  textareaEl.classList.remove('invalid');
  saveBtn.disabled = false;
}

// ── client-side validation (the lock) ───────────────────────────────────
// JSON tabs: JSON.parse. .sim tab: a basic XML well-formedness sniff (the
// authoritative gate is the backend parse_from_xml_str on save — this pre-
// check just keeps ✓ disabled + ✕ refusing while obviously broken, so the
// user gets immediate feedback without a round-trip).
function revalidate() {
  const text = textareaEl.value;
  if (!text.trim()) {
    // An empty file is valid for a fresh card/codex (the editor starts blank).
    setValid(true);
    return;
  }
  if (current && current.isXml) {
    setValid(xmlLooksValid(text));
  } else {
    try { JSON.parse(text); setValid(true); }
    catch (_) { setValid(false); }
  }
}

function setValid(ok) {
  isValid = ok;
  textareaEl.classList.toggle('invalid', !ok);
  saveBtn.disabled = !ok;
}

// A cheap XML well-formedness check: balanced tags + a single root. NOT a full
// XML parser (the backend roxmltree parse is authoritative) — it only guards
// against the obvious cases (unclosed tags, no root) so the lock engages fast.
function xmlLooksValid(text) {
  const t = text.trim();
  if (!t.startsWith('<')) return false;
  // Strip comments + CDATA contents so their inner < > don't confuse the count.
  const stripped = t
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/<!\[CDATA\[[\s\S]*?\]\]>/g, '');
  // Count open vs close tags (ignoring self-closing + declarations).
  const opens = (stripped.match(/<[a-zA-Z_][^>/]*?(?<!\/)>/g) || []).length;
  const closes = (stripped.match(/<\/[a-zA-Z_][^>]*>/g) || []).length;
  const selfClosed = (stripped.match(/<[a-zA-Z_][^>]*?\/>/g) || []).length;
  return opens === closes + selfClosed || (opens === 0 && selfClosed > 0);
}

// Hard reset (called from teardownStage on stage exit so a close mid-edit
// can't leave the modal open on the next session). Mirrors ✕: the file is
// whatever last saved (the textarea's unsaved edits are discarded — the
// protection the spec describes for an Alt+F4 / shutdown mid-edit).
export function resetRawEditor() {
  if (isOpen()) closeModal();
}
