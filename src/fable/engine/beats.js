// =============================================================
// GAMES BEATS — dialogue feed rendering (pure DOM, vanilla).
//
// Beat types map to the channel-event stream from fable_send:
//   narrator  → dark glass card with clean prose (the AI/Game Master), streams live.
//   character → speaker-labeled NPC line (from CHARACTER_TURN).
//   user      → dark charcoal bubble (brass border) for the player's action.
//   system    → small state-change beat (from OBJECT tags + saves).
//   error     → red beat for generation failures.
//
// LAYOUT (2026-07-27 sleek rework): profile-picture avatars are GONE. The
// feed is a centered column; each beat is a single card whose alignment
// gives the conversational rhythm:
//   - narrator/character/error: aligned to the LEFT of center.
//   - user: aligned to the RIGHT of center (mirrored via .fable-beat.user).
//   - system: no card, plain centered italic line.
// No avatar row, no SVG glyphs — just the bubbles. The beat is now the card
// directly (no .fable-beat-content wrapper).
//
// The feed is the scrolling container; beats are appended in order.
// Streaming: appendChunk() fills the active narrator/character beat;
// finalizeBeat() drops the .streaming class + caret.
// =============================================================

let feed = null;  // #fable-dialogue-feed

// Monotonic counter stamping `data-index` on every beat so the UX chat
// controls (edit / reroll / rewind-and-edit) can address messages by their
// position in the conversation. Reset in `clearFeed`. The counter is local
// to the rendered feed; it tracks the DOM order, which mirrors the backend
// `Conversation::messages` order at render time (loadHistory / chunk append).
let nextIndex = 0;

export function initBeats(feedEl) {
  feed = feedEl;
}

// Stamp a freshly-created beat with its conversation index + role. Called by
// every add* / start* builder. `role` is the wire-shape lowercase string
// ('user' | 'assistant' | 'system') matching what `fable_quick_resume` /
// `edit_message` / etc. return, so the backend ↔ frontend contract is
// symmetric. The index is the position in the rendered feed — which after a
// `rebuildFromMessages` equals the position in `Conversation::messages`.
function stamp(beat, role) {
  beat.dataset.index = String(nextIndex++);
  beat.dataset.role = role;
  return beat;
}

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

// Preserve line breaks in prose beats (narrator/character/system).
function prose(s) {
  return esc(s).replace(/\n/g, '<br>');
}

function scrollDown() {
  if (!feed) return;
  feed.scrollTop = feed.scrollHeight;
}

// Build a beat's inner card markup. `cardInner` is the HTML that goes inside
// .fable-beat-card. The 2026-07-27 sleek rework dropped the avatar row +
// .fable-beat-content wrapper — the beat element now holds the card
// directly, and alignment (left vs right of center) is driven purely by
// .fable-beat / .fable-beat.user CSS. Returns the innerHTML for the beat.
function beatCardHtml(cardInner) {
  return `<div class="fable-beat-card">${cardInner}</div>`;
}

export function addUserBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat user';
  b.innerHTML = beatCardHtml(`<div class="fable-beat-body">${prose(text)}</div>`);
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'user');
}

export function addSystemBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat system';
  // System beats skip the card — they're de-emphasized status lines.
  b.innerHTML = `<div class="fable-beat-body">${esc(text)}</div>`;
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'system');
}

export function addErrorBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat error';
  b.innerHTML = beatCardHtml(`<div class="fable-beat-body">${esc(text)}</div>`);
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'system');
}

// Start a streaming narrator beat. Returns the beat element so the
// caller can append chunks and finalize it.
export function startNarratorBeat() {
  const b = document.createElement('div');
  b.className = 'fable-beat narrator streaming';
  b.innerHTML = beatCardHtml(`<div class="fable-beat-body"></div>`);
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'assistant');
}

// Start a streaming character beat with a speaker label.
export function startCharacterBeat(speakerLabel) {
  const b = document.createElement('div');
  b.className = 'fable-beat character streaming';
  const cardInner =
    `<div class="fable-beat-speaker">${esc(speakerLabel)}</div>` +
    `<div class="fable-beat-body"></div>`;
  b.innerHTML = beatCardHtml(cardInner);
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'assistant');
}

// Append a streamed text chunk to a beat (narrator or character).
export function appendChunk(beat, text) {
  if (!beat || !text) return;
  // Track raw text on the element so finalize can re-render cleanly.
  beat._raw = (beat._raw || '') + text;
  const body = beat.querySelector('.fable-beat-body');
  if (body) body.innerHTML = prose(beat._raw);
  scrollDown();
}

// Finalize: drop the streaming caret, optionally re-render final text.
export function finalizeBeat(beat, finalText) {
  if (!beat) return;
  beat.classList.remove('streaming');
  const body = beat.querySelector('.fable-beat-body');
  if (!body) return;
  if (finalText != null) body.innerHTML = prose(finalText);
  else if (beat._raw != null) body.innerHTML = prose(beat._raw);
  scrollDown();
}

// Re-class a live narrator beat as a character beat when a
// CHARACTER_TURN bracket arrives mid-stream. Speaker label prepended.
// MVP limitation (AGENTS.md §11.10): a second CHARACTER_TURN in the
// same narrator turn overwrites the first speaker label.
export function reclassToCharacter(beat, speakerLabel) {
  if (!beat) return;
  beat.classList.remove('narrator');
  beat.classList.add('character');
  // Prepend speaker label if absent.
  const card = beat.querySelector('.fable-beat-card');
  if (!card) return;
  if (!card.querySelector('.fable-beat-speaker')) {
    const lbl = document.createElement('div');
    lbl.className = 'fable-beat-speaker';
    lbl.textContent = speakerLabel;
    card.insertBefore(lbl, card.firstChild);
  } else {
    card.querySelector('.fable-beat-speaker').textContent = speakerLabel;
  }
}

export function clearFeed() {
  if (feed) feed.innerHTML = '';
  // Reset the index counter so a fresh rebuild (loadHistory / rebuildFrom
  // Messages) re-stamps beats 0,1,2,… in lockstep with the backend's
  // `Conversation::messages` order.
  nextIndex = 0;
}

// =============================================================
// FEED REBUILD — used by loadHistory (stage.js) AND the mutation
// wrappers (narrator.js editMessage / reroll / rewind). One source of
// truth for "wipe the feed + re-render a messages[] snapshot," so a
// server-side mutation and a fresh card load render identically.
//
// `messages` is the wire shape: `[{ role: 'user'|'assistant'|'system',
// content: string }]` (same shape `fable_quick_resume` /
// `edit_message` / `reroll_last_turn` / `rewind_and_edit_user` return).
// Assistant messages are finalized narrator beats (no streaming caret,
// no chunk-by-chunk); the bracket-parser / atmosphere scan do NOT
// re-fire on a rebuild (those only fire during live streaming).
// =============================================================
export function rebuildFromMessages(messages) {
  if (!feed) return;
  clearFeed();
  for (const m of messages || []) {
    if (m.role === 'user') {
      addUserBeat(m.content);
    } else if (m.role === 'assistant') {
      const b = startNarratorBeat();
      finalizeBeat(b, m.content);
    } else {
      addSystemBeat(m.content);
    }
  }
  scrollDown();
}

// Read the text content of the body of a beat (used to seed the inline
// editor + to grab the last user message's text for reroll). Strips the
// <br> back to newlines so editing round-trips cleanly.
export function getBeatText(beat) {
  if (!beat) return '';
  const body = beat.querySelector('.fable-beat-body');
  if (!body) return '';
  // innerText honors <br> as newlines; textContent would collapse them.
  return (body.innerText || body.textContent || '').trim();
}

// =============================================================
// UX CHAT CONTROLS — hover-revealed edit + reroll affordances on beats.
//
// `renderControls` injects a `.fable-beat-controls` element into the beat's
// `.fable-beat-card`. CSS (fable.css) hides it by default and reveals on
// `.fable-beat:hover`. Each button carries `data-action` so a single
// delegated click handler on the feed can dispatch (stage.js).
//
// The controls live INSIDE the beat because `.fable-dialogue-feed` has
// `pointer-events: none` (fable.css) — only `.fable-beat` descendants re-
// enable `pointer-events: auto`, so anything outside a beat is unclickable.
//
// Buttons shown:
//   - edit (pencil): always on user beats; on the LAST assistant beat too
//     (in-place typo fix per spec §1 — distinct from reroll, which regens).
//   - reroll (circular-arrow): only on the last assistant beat (regen path).
// =============================================================
const ICON_EDIT = '<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">' +
  '<path fill="currentColor" d="M11.5 1.5l3 3L5 14H2v-3z" opacity="0.9"/>' +
  '<path fill="currentColor" d="M11.5 1.5l3 3" stroke="currentColor" stroke-width="0.5" fill="none"/></svg>';
const ICON_REROLL = '<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">' +
  '<path fill="none" stroke="currentColor" stroke-width="1.5" ' +
  'd="M3 8a5 5 0 1 1 1.5 3.6M3 11.5V8h3.5" stroke-linecap="round" stroke-linejoin="round"/></svg>';

export function renderControls(beat, { canEdit = false, canReroll = false } = {}) {
  if (!beat) return;
  const card = beat.querySelector('.fable-beat-card');
  if (!card) return; // system beats have no card → no controls.
  // Idempotent: remove any prior controls block before injecting.
  card.querySelector('.fable-beat-controls')?.remove();
  if (!canEdit && !canReroll) return;
  const wrap = document.createElement('div');
  wrap.className = 'fable-beat-controls';
  if (canEdit) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'fable-beat-btn';
    btn.dataset.action = 'edit';
    btn.title = 'Edit message';
    btn.setAttribute('aria-label', 'Edit message');
    btn.innerHTML = ICON_EDIT;
    wrap.appendChild(btn);
  }
  if (canReroll) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'fable-beat-btn';
    btn.dataset.action = 'reroll';
    btn.title = 'Regenerate response';
    btn.setAttribute('aria-label', 'Regenerate response');
    btn.innerHTML = ICON_REROLL;
    wrap.appendChild(btn);
  }
  card.appendChild(wrap);
}

// =============================================================
// INLINE EDIT MODE — swap a beat's body for a textarea + Save/Cancel.
//
// `onSave(newText)` is called with the trimmed editor value; the caller
// decides whether to invoke `edit_message` (in-place) or `rewind_and_edit_
// user` (timeline branch) based on the beat's position. `onCancel` is
// called with no args; both callbacks are responsible for restoring the
// beat to a non-editing state (typically by rebuilding the feed from the
// backend's authoritative messages[]).
//
// While editing, the beat gets `.editing` (CSS hides the controls + the
// read-only body) so the textarea is the sole focus.
// =============================================================
export function enterEditMode(beat, { onSave, onCancel } = {}) {
  if (!beat || beat.classList.contains('editing')) return;
  const card = beat.querySelector('.fable-beat-card');
  if (!card) return;
  const body = beat.querySelector('.fable-beat-body');
  if (!body) return;

  const original = getBeatText(beat);
  beat.classList.add('editing');
  body.style.display = 'none';

  const editor = document.createElement('textarea');
  editor.className = 'fable-beat-editor';
  editor.value = original;
  editor.rows = Math.max(2, original.split('\n').length);

  const footer = document.createElement('div');
  footer.className = 'fable-beat-editor-footer';

  const saveBtn = document.createElement('button');
  saveBtn.type = 'button';
  saveBtn.className = 'fable-beat-editor-btn primary';
  saveBtn.textContent = 'Save';
  const cancelBtn = document.createElement('button');
  cancelBtn.type = 'button';
  cancelBtn.className = 'fable-beat-editor-btn';
  cancelBtn.textContent = 'Cancel';

  footer.appendChild(cancelBtn);
  footer.appendChild(saveBtn);

  card.appendChild(editor);
  card.appendChild(footer);
  // Focus + select-all so the user can immediately start typing over the
  // existing text (most edit flows replace, not append).
  editor.focus();
  editor.select();

  const finish = (restoreBody) => {
    editor.remove();
    footer.remove();
    if (restoreBody) body.style.display = '';
    beat.classList.remove('editing');
  };

  saveBtn.addEventListener('click', () => {
    const next = editor.value.trim();
    finish(false); // caller will rebuild the feed from backend truth.
    if (onSave) onSave(next);
  });
  cancelBtn.addEventListener('click', () => {
    finish(true); // restore the original body — no backend round-trip.
    if (onCancel) onCancel();
  });
  // Ctrl/Cmd+Enter saves; Escape cancels (standard edit-field conventions).
  editor.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      saveBtn.click();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancelBtn.click();
    }
  });
}
