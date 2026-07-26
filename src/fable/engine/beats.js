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
// LAYOUT (2026-07-26 chat overhaul): every beat is a flex ROW holding a
// circular profile-picture avatar + a content card. Alignment is per-type:
//   - narrator/character/error: avatar LEFT, card flows right (row).
//   - user: avatar RIGHT, card flows left (row-reverse via .fable-beat.user).
//   - system: no avatar, plain centered italic line.
// The avatar SVGs are inline (no asset files) + colored via currentColor so
// the brass treatment lives in CSS. User = person bust; AI/narrator = paw.
//
// The feed is the scrolling container; beats are appended in order.
// Streaming: appendChunk() fills the active narrator/character beat;
// finalizeBeat() drops the .streaming class + caret.
// =============================================================

let feed = null;  // #fable-dialogue-feed

export function initBeats(feedEl) {
  feed = feedEl;
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

// --- Inline avatar SVGs (no asset files; currentColor = brass via CSS) ---
// USER: a clean person-bust silhouette (head + shoulders) — the universal
// "this is you" affordance. ViewBox 24x24, strokeless solid fill so it reads
// as a crisp glyph at 22px.
const AVATAR_USER_SVG = `
<svg viewBox="0 0 24 24" aria-hidden="true">
  <circle cx="12" cy="8" r="4.2"/>
  <path d="M4 20.5c0-3.6 3.6-6.2 8-6.2s8 2.6 8 6.2c0 0.9-0.3 1.5-1 1.5H5c-0.7 0-1-0.6-1-1.5z"/>
</svg>`;
// AI / NARRATOR: a paw mark — the Wupi motif (the OS mascot paw, reused as
// the Game Master's avatar). Four toe-beans + a main pad. Solid fill so it
// matches the user glyph's visual weight.
const AVATAR_AI_SVG = `
<svg viewBox="0 0 24 24" aria-hidden="true">
  <ellipse cx="8" cy="7" rx="1.8" ry="2.4"/>
  <ellipse cx="12" cy="5.8" rx="1.8" ry="2.4"/>
  <ellipse cx="16" cy="7" rx="1.8" ry="2.4"/>
  <ellipse cx="19.2" cy="10.5" rx="1.5" ry="2"/>
  <ellipse cx="4.8" cy="10.5" rx="1.5" ry="2"/>
  <path d="M12 11.5c3.8 0 6.5 2.6 6.5 5.6 0 2.2-1.6 3.4-3.8 3.4-1.2 0-1.8-0.4-2.7-0.4s-1.5 0.4-2.7 0.4c-2.2 0-3.8-1.2-3.8-3.4 0-3 2.7-5.6 6.5-5.6z"/>
</svg>`;

function scrollDown() {
  if (!feed) return;
  feed.scrollTop = feed.scrollHeight;
}

// Build the avatar element for a beat. `kind` is 'user' (right-side person
// glyph) or 'ai' (left-side paw glyph). Returns a fresh <div> node.
function buildAvatar(kind) {
  const a = document.createElement('div');
  a.className = 'fable-beat-avatar';
  a.innerHTML = (kind === 'user') ? AVATAR_USER_SVG : AVATAR_AI_SVG;
  return a;
}

// Wrap a beat's inner card markup in the avatar+card flex row. `cardInner`
// is the HTML that goes inside .fable-beat-card. `avatarKind` selects the
// glyph ('user' or 'ai'). Returns the innerHTML string for the beat.
function beatRowHtml(avatarKind, cardInner) {
  return (
    `<div class="fable-beat-content">` +
      `<div class="fable-beat-avatar">${(avatarKind === 'user') ? AVATAR_USER_SVG : AVATAR_AI_SVG}</div>` +
      `<div class="fable-beat-card">${cardInner}</div>` +
    `</div>`
  );
}

export function addUserBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat user';
  b.innerHTML = beatRowHtml('user', `<div class="fable-beat-body">${prose(text)}</div>`);
  feed.appendChild(b);
  scrollDown();
  return b;
}

export function addSystemBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat system';
  // System beats skip the avatar row — they're de-emphasized status lines.
  b.innerHTML = `<div class="fable-beat-body">${esc(text)}</div>`;
  feed.appendChild(b);
  scrollDown();
  return b;
}

export function addErrorBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat error';
  b.innerHTML = beatRowHtml('ai', `<div class="fable-beat-body">${esc(text)}</div>`);
  feed.appendChild(b);
  scrollDown();
  return b;
}

// Start a streaming narrator beat. Returns the beat element so the
// caller can append chunks and finalize it.
export function startNarratorBeat() {
  const b = document.createElement('div');
  b.className = 'fable-beat narrator streaming';
  b.innerHTML = beatRowHtml('ai', `<div class="fable-beat-body"></div>`);
  feed.appendChild(b);
  scrollDown();
  return b;
}

// Start a streaming character beat with a speaker label.
export function startCharacterBeat(speakerLabel) {
  const b = document.createElement('div');
  b.className = 'fable-beat character streaming';
  const cardInner =
    `<div class="fable-beat-speaker">${esc(speakerLabel)}</div>` +
    `<div class="fable-beat-body"></div>`;
  b.innerHTML = beatRowHtml('ai', cardInner);
  feed.appendChild(b);
  scrollDown();
  return b;
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
}
