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
// for the .sim/.player tabs; JSON.parse otherwise).
// (2026-08-21, Chloe) The PLAYER tab edits the identity file — the attached
// SavedPlayer's .player XML (fable_player_raw_get/set, same parse-gated path
// as the player-picker's editor). (2026-08-22, Chloe) The tab now carries
// BOTH halves of the attached player in ONE text field: the `.player` XML on
// top, the active session's player.json — the `{ "player_state": … }`
// gameplay slice the same fable_json_raw_get/set pair owns — just underneath,
// behind a `===== player.json =====` divider line. The divider is the save
// split boundary; the `check` override validates each half in its own format
// (XML sniff above the divider, JSON.parse below) so the validation lock, the
// green ✓, and ↻ all keep working on the combined text. The attached id and
// the as-loaded halves are resolved at read time and stashed for save.
// (2026-08-22 codex decoupling) The CODEX tab edits one UNIVERSAL library
// file (fable_codex_file_read/write, name-keyed) — the target name is
// handed to openRawEditor by the tab-rail link manager (the first linked
// codex as its default).
let playerRawId = null;
let playerLoadedXml = '';
let playerLoadedJson = '';   // '' = the open offered no JSON half (missing file / read failure)
let codexFileName = null;
const FILE_FOR = {
  card:   { title: 'Sim Card (.sim)',   isXml: true,  read: () => invoke('fable_card_raw_get'), save: (t) => invoke('fable_card_raw_set', { xml: t }) },
  codex:  {
    // Compound text (front-matter + `---` fences + prose bodies) — NEVER
    // JSON, so the default JSON.parse gate would leave ✓ permanently
    // disabled for every non-empty file. No client-side structural check:
    // the backend's compound parse on save is authoritative (the codex
    // format has no client-cheap well-formedness invariant).
    title: 'Codex (.codex)', isXml: false, check: () => true,
    read: async () => (await invoke('fable_codex_file_read', { name: codexFileName })).raw,
    save: (t) => invoke('fable_codex_file_write', { name: codexFileName, text: t }),
  },
  world:  { title: 'World (world.json)', isXml: false, read: () => invoke('fable_json_raw_get', { kind: 'world' }),  save: (t) => invoke('fable_json_raw_set', { kind: 'world',  json: t }) },
  player: {
    title: 'Player (.player + player.json)', isXml: true,
    // The combined-format gate: each half validates in its own format (see
    // playerRawTextLooksValid) — this override replaces the whole-text sniff,
    // which the appended JSON would otherwise break.
    check: (text) => playerRawTextLooksValid(text),
    read: async () => {
      const p = await invoke('fable_active_player_get');
      if (!p || !p.id) throw new Error('No player attached to this game.');
      playerRawId = p.id;
      const xml = await invoke('fable_player_raw_get', { id: playerRawId });
      // The player.json half — the ACTIVE SESSION's gameplay slice. A
      // not-yet-written file returns '' (fresh session); a failed read
      // degrades to the XML-only view so the tab stays usable (the JSON
      // half simply isn't offered for editing that open).
      let json = '';
      try {
        json = await invoke('fable_json_raw_get', { kind: 'player' });
      } catch (err) {
        console.warn('[fable] raw editor player.json read failed', err);
      }
      playerLoadedXml = xml;
      playerLoadedJson = json;
      return combinePlayerRawText(xml, json);
    },
    save: async (t) => {
      const { xml, json } = splitPlayerRawText(t);
      if (json === null) {
        // No divider — the whole text is the .player XML (the pre-2026-08-22
        // shape: no JSON half was offered this open).
        await invoke('fable_player_raw_set', { id: playerRawId, xml: t });
        return;
      }
      // Each half writes ONLY when it changed since the open — an XML-only
      // edit must not round-trip the open-time player_state snapshot back
      // over the LIVE schema (fable_json_raw_set overwrites player_state
      // wholesale; the stale-open rollback class the narrator-in-flight
      // guard only partially covers).
      if (xml.trim() !== playerLoadedXml.trim()) {
        await invoke('fable_player_raw_set', { id: playerRawId, xml });
      }
      if (json.trim() !== playerLoadedJson.trim()) {
        await invoke('fable_json_raw_set', { kind: 'player', json });
      }
    },
  },
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
// (2026-08-24) True while openRawEditor's read() is still resolving. The
// textarea is the EMPTY pre-fill in that window — a ✓ clicked there would
// write '' over the real file (the codex wipe). ✓/↻ stay inert + revalidate
// keeps its hands off until the text lands; ✕/Esc/backdrop still close.
let loadPending = false;

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
          <button class="fable-raw-btn save"  data-raw-save     aria-label="Save">✓</button>
          <button class="fable-raw-btn revert" data-raw-revert aria-label="Revert to last saved">↻</button>
          <button class="fable-raw-btn close" data-raw-close   aria-label="Close without saving">✕</button>
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
// `opts.codexName` (codex kind, 2026-08-22) selects WHICH universal library
// file the editor targets — required before a codex open (no default file
// exists anymore).
export async function openRawEditor(kind, onSaved, opts = {}) {
  const file = FILE_FOR[kind];
  if (!file || !overlayEl) return;
  if (kind === 'codex') {
    const name = (opts && opts.codexName && String(opts.codexName).trim()) || null;
    if (!name) return; // no library file selected — nothing to edit
    codexFileName = name;
  }
  saveEpoch++;               // invalidate any save still resolving from a prior session
  const epoch = saveEpoch;   // (2026-08-24) also invalidates THIS open's read continuation
  current = file;
  onSavedCb = onSaved || null;
  titleEl.textContent = kind === 'codex' && codexFileName
    ? `Codex — ${codexFileName}`
    : file.title;
  textareaEl.value = '';
  overlayEl.hidden = false;
  // (2026-08-24) PENDING-LOAD LOCK: the read is in flight — ✓ + ↻ are inert
  // (disabled + guarded) until the text lands, so the still-empty textarea
  // can never be saved over the real file. ✕ / Esc / backdrop stay live.
  loadPending = true;
  saveBtn.disabled = true;
  revertBtn.disabled = true;
  try {
    const text = await file.read();
    // card/codex reads can return empty for a fresh file; that's valid (the
    // editor starts blank for a new file). JSON tabs return '' too.
    if (epoch !== saveEpoch) return; // superseded by a close/reopen mid-read
    loadPending = false;
    revertBtn.disabled = false;
    textareaEl.value = text || '';
    textareaEl.placeholder = '';   // clear any prior open's load-failure message
    lastGood = text || '';
    revalidate();
    setTimeout(() => textareaEl.focus(), 30);
  } catch (err) {
    if (epoch !== saveEpoch) return; // the editor it failed in is gone
    console.warn('[fable] raw editor load failed', err);
    // (2026-08-22 Chloe) A failed read used to close the modal SILENTLY —
    // the ✎ read as dead ("nothing happens but the drawer closes"). Keep
    // the popup OPEN with the failure visible: the title carries the
    // error, the textarea shows it as the placeholder (never as value — a
    // value would re-validate valid + re-enable ✓), and ✓ stays disabled
    // so the empty text can never overwrite the real file. ✕ / Esc /
    // backdrop close normally.
    const msg = String(err && err.message ? err.message : err);
    loadPending = false;
    revertBtn.disabled = false;
    current = null;           // ✓ can never save over a file that failed to load
    lastGood = '';
    isValid = true;
    saveBtn.disabled = true;
    titleEl.textContent = `${file.title} — load failed`;
    textareaEl.value = '';
    textareaEl.placeholder = msg;
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
  // (2026-08-24) Pending-load guard: the textarea is still the empty pre-fill
  // — saving it would wipe the file (the button is disabled for this window;
  // the guard is the authority for any programmatic path).
  if (loadPending) return;
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
  // Reset the textarea to the last-good text. Always works (no validity gate)
  // — except mid-load, where there is no last-good yet (the pending window's
  // empty revert would no-op confusingly at best).
  if (loadPending) return;
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
  loadPending = false;       // a close mid-read ends that window too
  revertBtn.disabled = false;
  textareaEl.value = '';
  textareaEl.placeholder = '';
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
  // Mid-load the textarea holds no real text yet — the pending-load lock owns
  // ✓'s state in that window (typing into the pre-fill can't re-arm it).
  if (loadPending) return;
  const text = textareaEl.value;
  if (!text.trim()) {
    // An empty file is valid for a fresh card/codex (the editor starts blank).
    setValid(true);
    return;
  }
  if (current && typeof current.check === 'function') {
    // A kind-specific gate (the codex compound-text editor) overrides the
    // XML/JSON sniff entirely.
    setValid(current.check(text));
  } else if (current && current.isXml) {
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

// ── the PLAYER tab's combined format (2026-08-22) ────────────────────────
// One text field, two halves: the `.player` XML on top, the session's
// player.json underneath, separated by a divider line. The divider is a
// whole line of `=` around `player.json` — matched tolerantly (any equals
// count) so a hand-typed `=== player.json ===` still splits; the canonical
// form is what read-side assembly writes.
const PLAYER_JSON_DIVIDER = '===== player.json =====';
const PLAYER_JSON_DIVIDER_RE = /^[=\s]*player\.json[=\s]*$/i;

// Split the combined text at the FIRST divider line. `{ xml, json }` — json
// is null when no divider is present (XML-only mode). Exported for the Node
// suite (tests/raw-editor-player.test.mjs).
export function splitPlayerRawText(text) {
  const lines = String(text).split('\n');
  const idx = lines.findIndex((l) => PLAYER_JSON_DIVIDER_RE.test(l.trim()));
  if (idx === -1) return { xml: text, json: null };
  return {
    // trimEnd/trimStart drop the blank line(s) the read-side assembly puts
    // around the divider (XML-only mode above stays verbatim).
    xml: lines.slice(0, idx).join('\n').trimEnd(),
    json: lines.slice(idx + 1).join('\n').trimStart(),
  };
}

// Assemble the combined text on read. An empty JSON half yields the XML
// untouched (no divider) — saving then writes the .player XML only and never
// touches the session state.
export function combinePlayerRawText(xml, json) {
  const j = String(json || '').trim();
  if (!j) return String(xml || '');
  return `${String(xml || '').trimEnd()}\n\n${PLAYER_JSON_DIVIDER}\n\n${j}`;
}

// The combined-format validity gate: the XML half passes the same sniff the
// .sim tab uses; when a divider is present, the JSON half below it must
// JSON.parse (the json checker, scoped to its section — an empty half after
// a divider is invalid: fix it or ↻). No divider = XML-only, as before.
export function playerRawTextLooksValid(text) {
  const { xml, json } = splitPlayerRawText(text);
  if (!xmlLooksValid(xml)) return false;
  if (json !== null) {
    try { JSON.parse(json); return true; } catch (_) { return false; }
  }
  // XML-only mode still guards its own hazard: trailing non-XML text (e.g.
  // the JSON half with its divider line deleted) — the tag-count sniff
  // balances straight through it, but the backend parse rejects it and the
  // save would silently drop the JSON half. Only whitespace may follow the
  // final close tag (comments stripped first, mirroring the sniff).
  const noComments = xml.replace(/<!--[\s\S]*?-->/g, '');
  const lastClose = noComments.lastIndexOf('</');
  if (lastClose !== -1) {
    const after = noComments.slice(noComments.indexOf('>', lastClose) + 1);
    if (after.trim() !== '') return false;
  }
  return true;
}

// Hard reset (called from teardownStage on stage exit so a close mid-edit
// can't leave the modal open on the next session). Mirrors ✕: the file is
// whatever last saved (the textarea's unsaved edits are discarded — the
// protection the spec describes for an Alt+F4 / shutdown mid-edit).
export function resetRawEditor() {
  if (isOpen()) closeModal();
}
