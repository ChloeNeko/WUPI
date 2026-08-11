// =============================================================
// FABLE BEATS — the card-style dialogue feed.
//
// Owns the on-screen narrator/user message stream. Each message is a
// rounded glass card with: a DETACHED centered nameplate header
// (.fable-mes-header: brass speaker name + muted timestamp) above a
// row holding a large 2:3 portrait flush-left (its inner edge
// dissolved via a CSS mask) + the prose body. AI vs user differ only
// by card tint + which side the portrait hugs (left vs right).
// System/error beats are narrow, portrait-less centered cards.
//
// Consumers:
//   - narrator.js    → streaming mutators (start/append/finalize +
//                      CHARACTER_TURN reclass mid-stream)
//   - stage.js       → addUserBeat on send, rebuildFromMessages on
//                      load/resume, delegated swipe/edit/reroll wiring
//
// Backend shapes this module reads (from fable_send / load results):
//   messages: [{ role: 'user'|'assistant', content, variants?: [String],
//                active_idx?: usize, timestamp?: i64 }]
//     timestamp is decorative-only; on the rebuild path it seeds the
//     header's time line. Live beats (start*/addUser) stamp Date.now()
//     at creation since the streaming channel carries no timestamp.
//   channel events → see narrator.js
// =============================================================

import { ARROW_SVG_LEFT, ARROW_SVG_RIGHT } from '../screens/wizard-engine.js';
import { variantCount, computeDrawerState } from './drawer-logic.js';

// Feather-style icons (currentColor-driven) for the drawer tool buttons.
// Drawer-specific, so they live here rather than alongside the wizard arrows.
const EDIT_SVG = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>`;
const DELETE_SVG = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><line x1="10" y1="11" x2="10" y2="17"/><line x1="14" y1="11" x2="14" y2="17"/></svg>`;

let feedEl = null;

// Identity for the message headers + portraits. Forwarded here by
// narrator.initNarrator (which is fed by stage.js's
// refreshActiveCardName). cardName → narrator beats; playerName → user
// beats. The portrait URLs are already asset://-ready (or '' when
// absent — the avatar column collapses via the `no-avatar` class).
let identity = {
  cardName: '',
  playerName: '',
  cardPortrait: '',
  playerPortrait: '',
  npcNames: new Map(),
};

// Set the identity fields. Only overwrites keys it's handed, so a
// partial update (e.g. just cardName) leaves the rest intact.
export function setIdentity(next = {}) {
  if (typeof next.cardName === 'string') identity.cardName = next.cardName;
  if (typeof next.playerName === 'string') identity.playerName = next.playerName;
  if (typeof next.cardPortrait === 'string') identity.cardPortrait = next.cardPortrait;
  if (typeof next.playerPortrait === 'string') identity.playerPortrait = next.playerPortrait;
  if (next.npcNames instanceof Map) identity.npcNames = next.npcNames;
}

// Bind the feed container. Called from stage.js wireStage with the
// [data-feed] element. Stores the ref so every builder below can
// append to it without re-querying.
export function initBeats(el) {
  feedEl = el;
}

export function clearFeed() {
  if (feedEl) feedEl.innerHTML = '';
}

// Auto-scroll the feed to its bottom. Throttled per-call via rAF so a
// burst of appendChunk calls during streaming doesn't thrash layout.
let scrollPending = false;
export function scrollDown() {
  if (!feedEl || scrollPending) return;
  scrollPending = true;
  requestAnimationFrame(() => {
    scrollPending = false;
    feedEl.scrollTop = feedEl.scrollHeight;
  });
}

// ── Markdown-lite ────────────────────────────────────────────────
// A tiny inline formatter: *italics* → <em>, **bold** → <strong>,
// "quotes" → an ivory-spanned quote run, and \n → <br>. No block
// syntax (no headings/lists/code) — narrator prose is paragraph text.
// Escapes HTML first so model output can't inject markup.
function esc(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

export function renderMarkdown(text) {
  let s = esc(text);
  // **bold** before *italics* so the greedy double-asterisk eats first.
  s = s.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  s = s.replace(/\*([^*]+)\*/g, '<em>$1</em>');
  // "dialogue" → ivory quoted run (curly or straight quotes).
  s = s.replace(/([“"][^"”]*["”])/g, '<span class="quote">$1</span>');
  s = s.replace(/\n/g, '<br>');
  return s;
}

// ── Beat construction ────────────────────────────────────────────
// Build a single message-card DOM node. role ∈ assistant/user/system/
// error. name + portraitUrl drive the header + avatar (system/error
// omit both). index is stamped onto dataset.index so the delegated
// swipe/edit handlers in stage.js can resolve the target message.
//
// Header architecture (2026-08-11): the speaker name + a muted time
// stamp live in a DETACHED .fable-mes-header nameplate that sits ABOVE
// the avatar+body row (centered over the card), not inside the body.
// This gives the VN-style centered nameplate called out in the layout
// directive + preps .fable-mes-name for a future metallic hover sheen.
// The header spans the full card width; the avatar+body sit in a
// .fable-mes-row flex container beneath it. The body (text + drawer)
// keeps its class names so reclassToCharacter / renderMessageDrawer / the
// delegated swipe+edit handlers all keep resolving unchanged.
function buildMes({ role, name, portraitUrl, index, timestamp }) {
  const root = document.createElement('div');
  root.className = 'fable-mes ' + role;
  root.dataset.role = role;
  if (typeof index === 'number') root.dataset.index = String(index);

  if (role === 'system' || role === 'error') {
    // Narrow centered card: no avatar, no name, just the body.
    root.innerHTML = `
      <div class="fable-mes-block">
        <div class="fable-mes-text"></div>
      </div>`;
    return root;
  }

  const hasAvatar = !!portraitUrl;
  if (!hasAvatar) root.classList.add('no-avatar');

  // The muted timestamp (2026-08-11 dual-placement, per Chloe). Omitted
  // entirely when timestamp is 0 / missing (matches the FableLoadMessage
  // "omit when 0" convention). TWO elements are rendered per beat and
  // toggled purely by the vn-recent / vn-history classes vn-interactions
  // stamps on the card — so a single buildMes works for fresh beats AND
  // the rebuilt archive, and a beat re-flows automatically when it ages
  // out of the latest 2 (no re-render needed):
  //   • --over   : the 2 LATEST beats. Centered OVER the avatar column,
  //                its own flex line between the nameplate and the bubble
  //                — "right above the pictures, outside the box," fully
  //                separate from the centered nameplate. It is a SIBLING
  //                flex child of .fable-mes (NOT inside .fable-mes-header)
  //                so the header's screen-centering counter-shift doesn't
  //                drag it to the middle — it stays pinned to the avatar's
  //                side and moves with the card's stagger.
  //   • --corner : the revealed HISTORY archive. Tucked into the far
  //                bottom corner of the bubble — bottom-LEFT for AI/NPC,
  //                bottom-RIGHT for user (mirrored), pushed toward the
  //                edge. Lives inside .fable-mes-block.
  // Only one is visible at a time (fable.css toggles on vn-recent/vn-history).
  const timeText = (typeof timestamp === 'number' && timestamp > 0)
    ? esc(formatTime(timestamp))
    : '';
  const overTimeHTML = timeText
    ? `<span class="fable-mes-time fable-mes-time--over">${timeText}</span>`
    : '';
  const cornerTimeHTML = timeText
    ? `<span class="fable-mes-time fable-mes-time--corner">${timeText}</span>`
    : '';

  // The detached nameplate: brass name centered, UNTOUCHED by the
  // timestamp (the two are completely separate elements now). The
  // over-avatar timestamp is a child of the header but absolutely
  // positioned (see fable.css) so it adds ZERO card height — it floats in
  // the header's bottom band over the portrait, without pushing the
  // nameplate upward the way an in-flow line would (recent cards are
  // bottom-anchored in the feed, so any added height lifts the nameplate).
  const headerHTML = `
    <div class="fable-mes-header">
      <span class="fable-mes-name">${esc(name || '')}</span>
      ${overTimeHTML}
    </div>`;

  // User cards MIRROR the assistant layout: the body column comes first
  // + the avatar column comes AFTER (so the portrait sits on the RIGHT
  // edge of the card, matching the "user messages from the right" rhythm).
  // Assistant/system cards keep the avatar on the left.
  const avatarHTML = `
    <div class="fable-mes-avatar">
      ${hasAvatar ? `<img class="fable-mes-avatar-img" src="${esc(portraitUrl)}" alt="">` : ''}
    </div>`;
  const blockHTML = `
    <div class="fable-mes-block">
      <div class="fable-mes-text"></div>
      ${cornerTimeHTML}
    </div>`;
  const rowHTML = (role === 'user')
    ? blockHTML + avatarHTML
    : avatarHTML + blockHTML;
  // The over-avatar timestamp lives inside the header (absolutely
  // positioned, see buildMes above); the row follows the header directly
  // so the card height is just header + row — the timestamp adds nothing.
  // The hover side-drawer (tools + ‹ N/1 › footer). A direct child of
  // .fable-mes — NOT inside .fable-mes-row (that's overflow:hidden) — so it
  // can absolute-anchor to the card + slide out into the center gutter on
  // hover. Populated by renderMessageDrawer (re-run on every variant stamp).
  const drawerHTML = `<div class="fable-mes-drawer" aria-hidden="true"></div>`;
  root.innerHTML = headerHTML + `<div class="fable-mes-row">${rowHTML}</div>` + drawerHTML;
  return root;
}

// Format an epoch-millis stamp as a short locale-aware clock string
// (e.g. "9:08 PM"). Decorative-only — never sent to the model, never
// stored. Used by the header nameplate.
function formatTime(ms) {
  try {
    return new Date(ms).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
  } catch (_) {
    return '';
  }
}

function appendMes(root) {
  if (!feedEl) return null;
  feedEl.appendChild(root);
  scrollDown();
  return root;
}

function bodyEl(root) {
  return root ? root.querySelector('.fable-mes-text') : null;
}

// ── Public beat builders ─────────────────────────────────────────

// A user (player) beat. Uses the player portrait if present; otherwise
// the avatar column collapses (no-avatar class) and only the tinted
// card + name remain.
export function addUserBeat(text, opts = {}) {
  if (!feedEl) return null;
  const name = opts.name || identity.playerName || 'You';
  const portrait = identity.playerPortrait || '';
  const root = appendMes(buildMes({ role: 'user', name, portraitUrl: portrait, timestamp: Date.now() }));
  if (root && text != null) bodyEl(root).innerHTML = renderMarkdown(text);
  return root;
}

// A centered system beat (object state changes, scene annotations).
export function addSystemBeat(text) {
  if (!feedEl) return null;
  const root = appendMes(buildMes({ role: 'system' }));
  if (root && text != null) bodyEl(root).innerHTML = renderMarkdown(text);
  return root;
}

// A centered error beat (stream failures, IPC errors). Tinted red.
export function addErrorBeat(text) {
  if (!feedEl) return null;
  const root = appendMes(buildMes({ role: 'error' }));
  if (root && text != null) bodyEl(root).textContent = String(text);
  return root;
}

// Start a narrator (assistant) beat. The body is empty + marked
// `.streaming` so the caret shows before the first chunk lands.
// `opts.name` overrides the card identity name (rarely needed).
export function startNarratorBeat(opts = {}) {
  if (!feedEl) return null;
  const name = opts.name || identity.cardName || 'Narrator';
  const portrait = identity.cardPortrait || '';
  const root = appendMes(buildMes({ role: 'assistant', name, portraitUrl: portrait, timestamp: Date.now() }));
  if (root) root.classList.add('streaming');
  return root;
}

// Start a character (NPC) beat — same shape as a narrator beat but
// labelled with the NPC's display name. Per-NPC portraits are deferred
// (fall back to the card portrait); the caller may pass a portraitUrl.
export function startCharacterBeat(speakerLabel, portraitUrl) {
  if (!feedEl) return null;
  const name = speakerLabel || 'Someone';
  const portrait = portraitUrl || identity.cardPortrait || '';
  const root = appendMes(buildMes({ role: 'assistant', name, portraitUrl: portrait, timestamp: Date.now() }));
  if (root) root.classList.add('streaming');
  return root;
}

// ── Streaming mutators ───────────────────────────────────────────

// Append a raw text chunk to a live beat. The body holds the raw
// (unformatted) streaming text; finalizeBeat does the markdown pass on
// the final string. Keeping the body raw during streaming avoids
// re-escaping/re-parsing on every chunk.
export function appendChunk(beat, text) {
  if (!beat || text == null) return;
  const body = bodyEl(beat);
  if (!body) return;
  beat.classList.add('streaming');
  // Stash raw on the dataset so the next append concatenates cleanly.
  const prev = beat.dataset.raw || '';
  const next = prev + String(text);
  beat.dataset.raw = next;
  body.innerHTML = renderMarkdown(next);
  scrollDown();
}

// Finalize a beat: write the final prose (preferred over the streamed
// concatenation — the backend's final_text is authoritative) + drop
// the streaming caret. The reasoning arg is unused post-2026-08-07
// (the API narrator never thinks) but kept for signature stability.
export function finalizeBeat(beat, finalText, _reasoning) {
  if (!beat) return;
  const body = bodyEl(beat);
  const text = finalText != null ? finalText : (beat.dataset.raw || '');
  if (body) body.innerHTML = renderMarkdown(text);
  beat.dataset.raw = text;
  beat.classList.remove('streaming');
}

// Mid-stream reclassification: the live narrator beat becomes a
// character (NPC) beat. Swaps the name + portrait src in place so the
// already-streamed prose reads as belonging to the NPC.
export function reclassToCharacter(beat, speakerLabel, portraitUrl) {
  if (!beat) return;
  const nameEl = beat.querySelector('.fable-mes-name');
  if (nameEl && speakerLabel) nameEl.textContent = speakerLabel;
  if (portraitUrl) {
    const img = beat.querySelector('.fable-mes-avatar-img');
    if (img) {
      img.src = portraitUrl;
    } else {
      // The beat was built no-avatar (no card portrait); promote it to
      // having one now that an NPC portrait arrived.
      const wrap = beat.querySelector('.fable-mes-avatar');
      if (wrap) {
        wrap.innerHTML = `<img class="fable-mes-avatar-img" src="${esc(portraitUrl)}" alt="">`;
        beat.classList.remove('no-avatar');
      }
    }
  }
}

// ── Variant (swipe) bookkeeping ──────────────────────────────────

// The last assistant beat in the feed (or null). Used by the reroll
// path to claim the in-place streaming target.
export function lastNarratorBeat() {
  if (!feedEl) return null;
  const beats = feedEl.querySelectorAll('.fable-mes[data-role="assistant"]');
  return beats.length ? beats[beats.length - 1] : null;
}

// Prepare a beat for an in-place re-stream (reroll): clear its body +
// raw stash, re-mark streaming, leave its identity/index intact.
export function beginReroll(beat) {
  if (!beat) return;
  const body = bodyEl(beat);
  if (body) body.innerHTML = '';
  delete beat.dataset.raw;
  beat.classList.add('streaming');
}

// Stamp the variant count + active index onto a beat (drives the drawer
// footer's ‹ N/1 ›). variants is the backend's Vec<String>; activeIdx is
// the 0-based active position. Both stored on dataset so renderMessageDrawer
// can read them without a re-arg.
export function stampVariants(beat, variants, activeIdx) {
  if (!beat) return;
  // count = variants.length (variants INCLUDES the active one — backend's
  // variant_count() == variants.len().max(1)). The prior `+ 1` was an
  // off-by-one (showed N+1 + let `>` swipe one past the real last variant).
  const count = variantCount(variants);
  beat.dataset.variantCount = String(count);
  beat.dataset.variantActive = String(activeIdx || 0);
  renderMessageDrawer(beat);
}

// Splice a newly-active variant's content into a beat's body in place
// (no feed rebuild). Used by the swipe ‹/› UX.
export function swapVariantBody(index, content) {
  if (!feedEl) return;
  const beat = feedEl.querySelector(`.fable-mes[data-index="${index}"]`);
  if (!beat) return;
  const body = bodyEl(beat);
  if (body) body.innerHTML = renderMarkdown(content);
  beat.dataset.raw = content;
}

// ── Feed lifecycle ───────────────────────────────────────────────

// Wipe + rebuild the feed from a message list (the backend's source of
// truth on load/resume/edit/rewind). Each message → one card; assistant
// messages with variants get the swipe bar stamped.
export function rebuildFromMessages(messages) {
  if (!feedEl) return;
  feedEl.innerHTML = '';
  if (!Array.isArray(messages)) return;
  messages.forEach((m, i) => {
    const role = m.role === 'user' ? 'user' : 'assistant';
    const name = role === 'user'
      ? (identity.playerName || 'You')
      : (identity.cardName || 'Narrator');
    const portrait = role === 'user'
      ? (identity.playerPortrait || '')
      : (identity.cardPortrait || '');
    const root = buildMes({ role, name, portraitUrl: portrait, index: i, timestamp: m.timestamp });
    feedEl.appendChild(root);
    const body = bodyEl(root);
    if (body) body.innerHTML = renderMarkdown(m.content || '');
    root.dataset.raw = m.content || '';
    // Variant stamp: only assistant turns carry variants; the count is
    // variants.length + 1 (the active content is itself a variant).
    if (role === 'assistant') {
      if (Array.isArray(m.variants) && m.variants.length) {
        root.dataset.variantCount = String(variantCount(m.variants));
        root.dataset.variantActive = String(m.active_idx || 0);
      } else {
        root.dataset.variantCount = '1';
        root.dataset.variantActive = '0';
      }
    }
    // Render the hover side-drawer for every user + assistant beat.
    // system/error have no drawer (renderMessageDrawer no-ops them).
    if (role === 'user' || role === 'assistant') {
      renderMessageDrawer(root);
    }
  });
  scrollDown();
}

// Read a beat body's text content (used by the edit path to seed the
// inline editor with the current prose).
export function getBeatText(beat) {
  if (!beat) return '';
  const body = bodyEl(beat);
  return body ? body.textContent : '';
}

// ── Per-beat hover side-drawer (tools + ‹ N/1 › nav) ─────────────
// The drawer (.fable-mes-drawer, emitted by buildMes as a direct child
// of .fable-mes) replaces the old in-flow .fable-mes-controls bar. It
// holds a vertical tool stack (Edit / Delete) over a ‹ N/1 › footer.
// ‹/› step between existing variants; › at the last variant of the LAST
// assistant beat rolls a FRESH variant (Regenerate is folded into › —
// there is no separate reroll button). data-action is stamped so
// stage.js's delegated handler can route the click. Runs for both user +
// assistant beats (system/error are no-op'd). The mutation calls live in
// narrator.js.
export function renderMessageDrawer(beat) {
  if (!beat) return;
  const drawer = beat.querySelector('.fable-mes-drawer');
  if (!drawer) return;
  const role = beat.dataset.role;
  if (role !== 'user' && role !== 'assistant') {
    drawer.innerHTML = '';
    return;
  }
  const count = Number.parseInt(beat.dataset.variantCount || '1', 10);
  const active = Number.parseInt(beat.dataset.variantActive || '0', 10);
  const isLastAssistant = role === 'assistant' && lastNarratorBeat() === beat;
  const { canPrev, canNext, nextLabel } =
    computeDrawerState({ role, count, active, isLastAssistant });

  drawer.innerHTML = `
    <div class="fable-mes-drawer-tools">
      <button class="fable-mes-drawer-tool" data-action="edit" aria-label="Edit message" title="Edit">${EDIT_SVG}</button>
      <button class="fable-mes-drawer-tool" data-action="delete" aria-label="Delete message" title="Delete">${DELETE_SVG}</button>
    </div>
    <div class="fable-mes-drawer-nav">
      <button class="fable-mes-drawer-arrow" data-action="swipe-prev" aria-label="Previous variant" ${canPrev ? '' : 'disabled'}>${ARROW_SVG_LEFT}</button>
      <span class="fable-mes-drawer-count">${active + 1}/${count}</span>
      <button class="fable-mes-drawer-arrow" data-action="swipe-next" aria-label="${nextLabel}" title="${nextLabel}" ${canNext ? '' : 'disabled'}>${ARROW_SVG_RIGHT}</button>
    </div>`;
}

// Swap a beat's body for an inline <textarea>. onSave(text) commits;
// onCancel() restores the original prose. Enter commits (Shift+Enter
// is a newline); Esc cancels. Used for both in-place user edits +
// assistant rewind-and-edit (the caller picks the commit path).
export function enterEditMode(beat, opts = {}) {
  if (!beat) return;
  const body = bodyEl(beat);
  if (!body) return;
  const original = beat.dataset.raw != null ? beat.dataset.raw : body.textContent;
  const ta = document.createElement('textarea');
  ta.className = 'fable-mes-editor';
  ta.value = original;
  ta.rows = Math.max(3, Math.min(14, original.split('\n').length + 1));
  body.innerHTML = '';
  body.appendChild(ta);
  beat.classList.add('editing');
  ta.focus();
  ta.selectionStart = ta.value.length;
  ta.selectionEnd = ta.value.length;

  const close = (commit) => {
    beat.classList.remove('editing');
    if (commit) {
      const text = ta.value;
      body.innerHTML = renderMarkdown(text);
      beat.dataset.raw = text;
      if (typeof opts.onSave === 'function') opts.onSave(text);
    } else {
      body.innerHTML = renderMarkdown(original);
      beat.dataset.raw = original;
      if (typeof opts.onCancel === 'function') opts.onCancel();
    }
  };

  ta.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); close(true); }
    else if (e.key === 'Escape') { e.preventDefault(); close(false); }
  });
}
