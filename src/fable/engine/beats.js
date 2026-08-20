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

import { variantCount, computeDrawerState, swipeNextAction, canEditMessage } from './drawer-logic.js';

let feedEl = null;

// Identity for the message headers + portraits. Forwarded here by
// narrator.initNarrator (which is fed by stage.js's
// refreshActiveCardName). cardName → narrator beats; playerName → user
// beats. The portrait URLs are already asset://-ready (or '' when
// absent — a sleek silhouette placeholder fills the frame; see buildMes).
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
// append to it without re-querying. The stage DOM (and thus the feed
// element) is REUSED across entries, so the scroll listener is swapped,
// never stacked: a raw addEventListener here would double-bind every
// re-entry (the stage.js stageListeners discipline, applied at the
// module boundary).
let boundScroll = null;
export function initBeats(el) {
  if (feedEl && boundScroll) feedEl.removeEventListener('scroll', boundScroll);
  feedEl = el;
  windowStart = 0;
  boundScroll = onFeedScroll;
  if (feedEl) feedEl.addEventListener('scroll', boundScroll, { passive: true });
}

// ── Paged history window (2026-08-19, Chloe) ────────────────────
// The DOM renders only the TRAILING window of the session (48 history
// beats + the 2 recent = 50). Scrolling to the top edge prepends the
// next 50-message page (a small white spinner pinned at the top center
// while it builds) with the scroll position anchored on the
// previously-first beat, so a quadruple-digit campaign never pays a
// full-transcript DOM build on load. (Do NOT reach for
// content-visibility:auto on the beats as a deep-DOM scroll guard — its
// paint containment clips the hover toolrail + histools, which spawn
// outside the beat box; see the ban note in fable.css.)
//
// The rendered range is CONTIGUOUS [windowStart, sessionLen): the tail is
// never virtualized away (live streaming appends to it, and scrollDown's
// bottom pinning depends on it). LOAD-BEARING INVARIANT:
//   windowStart + (role beats in the DOM) === session message count
// It holds across every path: rebuild (resets both sides), a live append
// (both +1), a reverted turn's DOM pop (both −1), and a page prepend
// (windowStart −50, DOM +50). appendMes's auto-index stamp + the delete
// cascade's doomed-count both derive from it — the old bare DOM-count
// stamp under-counts by windowStart the moment older pages are excluded.
const FEED_WINDOW = 50;        // 48 history + the 2 recent (per Chloe)
const FEED_PAGE = 50;          // messages per upward page load
const FEED_LOAD_EDGE_PX = 80;  // scrolled this close to the top → load a page
let windowStart = 0;           // session index of the first rendered role beat
let pageLoading = false;
let pagebarEl = null;
// Full message array from the last rebuild. Only ever READ at indexes
// BELOW windowStart (pages prepend older messages; the array's tail can
// be stale behind live appends/reverts, which is exactly the region we
// never touch — every tail mutation rebuilds through rebuildFromMessages
// and hands us a fresh array).
let sessionMsgs = null;
// Bumped by clearFeed + every rebuild — lets in-flight async work
// (entrance tweens, page loads) detect that the DOM under them was
// replaced and self-cancel.
let scrollGen = 0;

function roleBeatsInDom() {
  if (!feedEl) return [];
  return feedEl.querySelectorAll(
    '.fable-mes[data-role="user"],.fable-mes[data-role="assistant"]');
}

// The session message count (the delete-cascade warning's doomed-run math
// feeds off this). Derived from the invariant above — pure DOM, no IPC.
export function sessionMessageCount() {
  if (!feedEl) return 0;
  return windowStart + roleBeatsInDom().length;
}

function onFeedScroll() {
  if (!feedEl || pageLoading) return;
  if (windowStart > 0 && feedEl.scrollTop <= FEED_LOAD_EDGE_PX) {
    loadOlderPage();
  }
}

// The sticky zero-height spinner bar pinned at the feed's top center. Lives
// as the feed's first real child (before every beat) so prepended pages
// land between it and the former first beat; height 0 + pointer-events:none
// means it never shifts content or eats a click — visible only while a
// page build is in flight (`.is-loading` fades the circle in).
function ensurePagebar() {
  if (!feedEl || pagebarEl) return;
  const bar = document.createElement('div');
  bar.className = 'fable-feed-pagebar';
  bar.setAttribute('aria-hidden', 'true');
  bar.innerHTML = '<div class="fable-feed-pagebar-spin"></div>';
  feedEl.insertBefore(bar, feedEl.firstChild);
  pagebarEl = bar;
}

function loadOlderPage() {
  if (!feedEl || pageLoading || windowStart <= 0 || !sessionMsgs) return;
  pageLoading = true;
  const gen = scrollGen;
  ensurePagebar();
  if (pagebarEl) pagebarEl.classList.add('is-loading');
  // Two rAFs guarantee a PRESENTED frame with the spinner visible before
  // the synchronous 50-beat build (layout-heavy) runs.
  requestAnimationFrame(() => requestAnimationFrame(() => {
    if (gen !== scrollGen || !feedEl) {
      // A rebuild/clear superseded this load — its anchor target is gone.
      pageLoading = false;
      if (pagebarEl) pagebarEl.classList.remove('is-loading');
      return;
    }
    const firstRole = feedEl.querySelector(
      '.fable-mes[data-role="user"],.fable-mes[data-role="assistant"]');
    const anchorTop = firstRole ? firstRole.offsetTop : 0;
    const newStart = Math.max(0, windowStart - FEED_PAGE);
    const frag = document.createDocumentFragment();
    const fresh = [];
    for (let i = newStart; i < windowStart; i++) {
      const node = buildBeatFromMessage(sessionMsgs[i], i);
      frag.appendChild(node);
      fresh.push(node);
    }
    feedEl.insertBefore(frag, firstRole || null);
    windowStart = newStart;
    // Anchor: hold the previously-first beat at the same viewport
    // position (the feed runs overflow-anchor:none — scrollTop is ours
    // to fix up). Same-element offsetTop delta == exact content growth.
    if (firstRole) feedEl.scrollTop += firstRole.offsetTop - anchorTop;
    // Drawer states were stamped while the page nodes were detached —
    // re-sync the fresh assistant beats (the trailing pair is untouched
    // by a prepend, so a full-feed pass is waste).
    fresh.forEach((node) => {
      if (node.classList.contains('assistant')) refreshDrawer(node);
    });
    if (pagebarEl) pagebarEl.classList.remove('is-loading');
    pageLoading = false;
    // A fast flick can cross a whole page — keep feeding pages until the
    // reader is off the load edge (or the transcript start is reached).
    if (windowStart > 0 && feedEl.scrollTop <= FEED_LOAD_EDGE_PX) {
      loadOlderPage();
    }
  }));
}

// ── Entrance + push-up scroll tween (2026-08-19, Chloe) ─────────
// A newly appended beat rises out from behind the input row (CSS entrance
// keyframes on the card) while the feed eases up to the new bottom over
// ~1s — the "long smooth train": scrollTop moves every rendered beat in
// lockstep, so nothing teleports to its final position. The tween chases
// a LIVE target (recomputed each frame) so streaming growth during the
// tween is followed, not fought. Any user scroll intent cancels it
// instantly — the reader always wins.
const ENTRANCE_MS = 1000;
let entrance = null;

function cancelEntrance() {
  if (!entrance) return;
  if (feedEl) {
    feedEl.removeEventListener('wheel', entrance.stop);
    feedEl.removeEventListener('touchstart', entrance.stop);
  }
  entrance = null;
}

function runEntranceScroll() {
  if (!feedEl) return;
  // Short transcripts (no overflow) have nowhere to scroll — the CSS
  // entrance keyframes on the beat itself carry the whole effect.
  if (feedEl.scrollHeight <= feedEl.clientHeight + 4) return;
  cancelEntrance();
  const stop = cancelEntrance;
  feedEl.addEventListener('wheel', stop, { passive: true });
  feedEl.addEventListener('touchstart', stop, { passive: true });
  // A second beat arriving mid-tween continues from the CURRENT scroll
  // position (captured fresh here) — no restart hop back to an old origin.
  const ent = { gen: scrollGen, from: feedEl.scrollTop, start: performance.now(), stop };
  entrance = ent;
  const ease = (t) => 1 - Math.pow(1 - t, 3);
  const frame = () => {
    // Identity check: a newer entrance (or a cancel) replaced this tween —
    // its own frame chain owns the scroll from here.
    if (entrance !== ent || !feedEl) return;
    if (ent.gen !== scrollGen) { cancelEntrance(); return; }
    const t = Math.min(1, (performance.now() - ent.start) / ENTRANCE_MS);
    const target = feedEl.scrollHeight - feedEl.clientHeight;
    feedEl.scrollTop = ent.from + (target - ent.from) * ease(t);
    if (t < 1) requestAnimationFrame(frame);
    else cancelEntrance();
  };
  requestAnimationFrame(frame);
}

export function clearFeed() {
  scrollGen++;
  cancelEntrance();
  if (feedEl) feedEl.innerHTML = '';
  // (2026-08-16 audit fix #26) Drop the open-editor bookkeeping with the
  // wiped nodes: `editingBeat` holds a strong ref that survived the wipe, so
  // a stale editor crossed sessions and its next commit path fired a phantom
  // edit_message at the NEW session. The editor's listeners die with the
  // node (editClosers is a WeakMap) — only this ref needed clearing.
  editingBeat = null;
  windowStart = 0;
  pagebarEl = null;   // died with innerHTML wipe (if it existed)
  pageLoading = false;
  sessionMsgs = null;
}

// Auto-scroll the feed to its bottom. Throttled per-call via rAF so a
// burst of appendChunk calls during streaming doesn't thrash layout.
// Suppressed while an entrance tween owns scrollTop (the tween ends at
// the same target — a per-chunk jump mid-tween would stutter the train).
let scrollPending = false;
export function scrollDown() {
  if (!feedEl || scrollPending || entrance) return;
  scrollPending = true;
  requestAnimationFrame(() => {
    scrollPending = false;
    // An entrance may have started between the call and this frame — the
    // tween owns scrollTop until it settles (both end at the same bottom).
    if (entrance) return;
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
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

export function renderMarkdown(text) {
  let s = esc(text);
  // **bold** before *italics* so the greedy double-asterisk eats first.
  s = s.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  s = s.replace(/\*([^*]+)\*/g, '<em>$1</em>');
  // “dialogue” → ivory quoted run (CURLY quotes only — esc() already
  // rewrote every straight `"` to &quot; upstream, so a straight-quote arm
  // here could never match; it was dead pattern weight).
  s = s.replace(/(“[^”]*”)/g, '<span class="quote">$1</span>');
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
// .fable-mes-row flex container beneath it. The body keeps its class
// names so reclassToCharacter + the delegated handlers keep resolving.
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
  // When a beat has no portrait (no card portrait on the narrator side, no
  // SavedPlayer portrait on the player side), a sleek silhouette placeholder
  // (.fable-mes-avatar-ph) fills the SAME frame as a real <img> — same width,
  // same dissolve mask, same corner flush (fable.css). So the avatar column
  // always reserves its slot + pushes the body aside exactly like a populated
  // portrait; only the fill differs. The `no-avatar` class stays as a marker:
  // vn-interactions' click-to-snap is a no-op on placeholder beats (its
  // closest('.fable-mes-avatar-img') resolves null), and reclassToCharacter
  // swaps the placeholder for a real <img> when an NPC portrait arrives
  // mid-stream (it replaces wrap.innerHTML wholesale).
  const avatarInner = hasAvatar
    ? `<img class="fable-mes-avatar-img" src="${esc(portraitUrl)}" alt="">`
    : `<div class="fable-mes-avatar-ph" aria-hidden="true"></div>`;
  const avatarHTML = `
    <div class="fable-mes-avatar">
      ${avatarInner}
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
  //
  // HOVER TOOLRAIL (2026-08-14): a glassmorphic side-drawer tucked BEHIND
  // the bubble. It is a SIBLING of .fable-mes-row inside .fable-mes-body-
  // wrap, layered below the row (z-index 1 < row's 2) so the bubble
  // COVERS it at rest. It is 100% transparent (opacity:0) until hover, so
  // nothing bleeds through the bubble's own slight translucency. On hover
  // — ONLY the 2 latest beats (.vn-recent) — it slides out from behind
  // the bubble's edge: LEFT on user beats, RIGHT on AI beats, parking
  // at a -12px margin — 12px tucked UNDER the bubble's edge so the rail
  // reads attached (2026-08-14 per Chloe; the rail is 108px wide, with
  // square corners on the bubble-facing edge — only the outward corners
  // curve).
  //
  // It holds the per-beat controls in a VERTICAL rectangle sized to the
  // avatar's exact height (var(--fable-mes-avatar-h) in fable.css — a
  // long message NEVER grows it): the GOLD corner pair (Delete 🗑 over
  // Edit ✎, flush to the top-OUTER corner) + a ROLE-SHAPED variant nav
  // (user: the ▲ N/N ▼ capsule, hidden unless >1 variant; AI: the
  // ‹ N/N › bar pinned to the rail's bottom — silver medieval fraction
  // between two big 2.5D metallic-gold glyphs; at a SINGLE variant only
  // › shows — ‹ + the fraction stay hidden until variant 2 exists). There
  // is NO
  // dedicated Regenerate button (permanently removed 2026-08-14) — the
  // ▼/› arrow IS regeneration: it steps through the existing variants,
  // then at the last variant folds into a reroll that cuts the local
  // tracker + switches back and forth. Clicks route via stage.js's
  // delegated [data-drawer-act] handler to the narrator.js wrappers
  // (editMessage / deleteMessage / swipeVariant / rerollLastTurn via the
  // ▼/› fold / rewindAndEditUser).
  // refreshDrawer() (called here at build + via stampVariants) syncs
  // the nav text + button states to the beat's variant bookkeeping
  // (dataset.variantCount / variantActive).
  const drawerHTML = buildDrawerHTML(role);
  // HISTORY TOOLS sibling (2026-08-14): every dialogue beat carries the
  // iron trash+pencil column; CSS reveals it only on .vn-history hover.
  // System/error beats skip it (nothing to edit/delete there).
  const histoolsHTML = (role === 'user' || role === 'assistant') ? buildHistoolsHTML() : '';
  root.innerHTML =
    headerHTML +
    `<div class="fable-mes-body-wrap"><div class="fable-mes-row">${rowHTML}</div>${drawerHTML}${histoolsHTML}</div>`;
  refreshDrawer(root);
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

// ── Hover toolrail markup + state sync (2026-08-14) ──────────────────
// Feather-style line icons (currentColor stroke), inline so the rail has
// zero extra asset fetches + matches the rest of Fable's inline-SVG
// idiom. Variant chevrons are ROLE-SHAPED (2026-08-14 per Chloe): user
// beats keep the VERTICAL ▲/▼ capsule (the rail is a vertical column);
// AI beats get a horizontal ‹ N/N › bar pinned to the rail's bottom.
const ICO_CHEV_UP    = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M6 15l6-6 6 6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
const ICO_CHEV_DOWN  = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M6 9l6 6 6-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
const ICO_EDIT   = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M12 20h9" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
const ICO_TRASH  = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M3 6h18" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;

// The AI variant-bar chevrons are hand-drawn SVG PATHS with a metallic
// gold GRADIENT STROKE (url(#fableChevGold)) — deliberately NOT CSS
// background-clip:text. That technique breaks under Blink when an
// ancestor runs an animated filter (the hover flare), painting the
// whole button box as a gold block (2026-08-14 — the "square gold
// block on hover" bug, hit twice). SVG paint is immune: the gradient is
// a real stroke, the 2.5D extrusion is a CSS drop-shadow stack over the
// rendered svg alpha. Path strokes also give exact control the font
// glyphs lacked: BOLD (5.5 stroke), TALL (44-unit viewBox), sharp miter
// elbows + square caps. The gradient defs mount ONCE on <body>
// (id-stable — every rail's url(#) resolves to the same shared stops).
let chevDefsMounted = false;
function ensureChevDefs() {
  if (chevDefsMounted || typeof document === 'undefined' || !document.body) return;
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('width', '0');
  svg.setAttribute('height', '0');
  svg.setAttribute('aria-hidden', 'true');
  svg.style.position = 'absolute';
  svg.innerHTML = `<defs>
      <linearGradient id="fableChevGold" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stop-color="#B9963A"></stop>
        <stop offset="35%" stop-color="#E9CE79"></stop>
        <stop offset="60%" stop-color="#A5831F"></stop>
        <stop offset="100%" stop-color="#6E5511"></stop>
      </linearGradient>
      <linearGradient id="fableChevGoldHi" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stop-color="#D4B352"></stop>
        <stop offset="35%" stop-color="#F6E29B"></stop>
        <stop offset="60%" stop-color="#B8942E"></stop>
        <stop offset="100%" stop-color="#7A5F16"></stop>
      </linearGradient>
      <!-- IRON gradients: userSpaceOnUse spanning the 24×24 icon viewBox.
           objectBoundingBox (the default) would give EACH PATH its own
           gradient across its own bbox — degenerate (zero-height) on
           horizontal strokes like the trash lid, which then render wrong
           or vanish ("floating dots" bug). One shared vertical sheen
           across the whole glyph = coherent brushed metal. -->
      <linearGradient id="fableIron" x1="0" y1="0" x2="0" y2="24" gradientUnits="userSpaceOnUse">
        <stop offset="0%" stop-color="#5A616B"></stop>
        <stop offset="35%" stop-color="#A7B0BA"></stop>
        <stop offset="60%" stop-color="#6E7681"></stop>
        <stop offset="100%" stop-color="#3E444D"></stop>
      </linearGradient>
      <linearGradient id="fableIronHi" x1="0" y1="0" x2="0" y2="24" gradientUnits="userSpaceOnUse">
        <stop offset="0%" stop-color="#77808C"></stop>
        <stop offset="35%" stop-color="#C6CFD9"></stop>
        <stop offset="60%" stop-color="#8A929D"></stop>
        <stop offset="100%" stop-color="#4C525C"></stop>
      </linearGradient>
    </defs>`;
  document.body.appendChild(svg);
  chevDefsMounted = true;
}
const ICO_VAR_PREV = `<svg viewBox="0 0 24 38" aria-hidden="true" focusable="false"><path d="M17 10 L7 22 L17 34" fill="none" stroke="url(#fableChevGold)" stroke-width="5.5" stroke-linecap="square" stroke-linejoin="miter"/></svg>`;
const ICO_VAR_NEXT = `<svg viewBox="0 0 24 38" aria-hidden="true" focusable="false"><path d="M7 10 L17 22 L7 34" fill="none" stroke="url(#fableChevGold)" stroke-width="5.5" stroke-linecap="square" stroke-linejoin="miter"/></svg>`;

// HISTORY TOOLS (2026-08-14 per Chloe): iron-metallic trash + pencil for the
// ARCHIVE beats (.vn-history — everything older than the 2 recent). Same
// Feather-style paths as the gold rail pair, but stroked with the IRON
// gradient defs (mounted alongside the gold in ensureChevDefs) so the
// symbols read as forged iron, not gold. Revealed by CSS on history-beat
// hover; clicks ride the SAME data-drawer-act routing as the toolrail.
const ICO_HIST_TRASH = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M3 6h18" fill="none" stroke="url(#fableIron)" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" fill="none" stroke="url(#fableIron)" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" fill="none" stroke="url(#fableIron)" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
const ICO_HIST_EDIT  = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false"><path d="M12 20h9" fill="none" stroke="url(#fableIron)" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" fill="none" stroke="url(#fableIron)" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg>`;

// The archive-beat hover tools: a compact frosted-iron CAPSULE holding the
// vertical trash-over-pencil pair, tucked slightly OVER the bubble's right
// edge (see fable.css .fable-mes-histools). The overlap is also the hover
// bridge — the hit box is contiguous with the bubble, so the pointer never
// crosses a dead zone. Emitted for every dialogue beat; CSS reveals it
// ONLY on .vn-history hover, so the markup is rebuild-stable (no re-tagging
// needed when a beat ages out of the recent 2). The buttons carry the SAME
// data-drawer-act hooks as the toolrail (delete/edit) so stage.js's
// delegated router handles them with zero extra wiring.
function buildHistoolsHTML() {
  return `<div class="fable-mes-histools" aria-hidden="true">
      <div class="fable-mes-histools-inner">
        <button type="button" class="fable-mes-histool-btn" data-drawer-act="delete" aria-label="Delete message" title="Delete">${ICO_HIST_TRASH}</button>
        <button type="button" class="fable-mes-histool-btn" data-drawer-act="edit" aria-label="Edit message" title="Edit">${ICO_HIST_EDIT}</button>
      </div>
    </div>`;
}

// Build the drawer's static markup for one beat. A VERTICAL rectangle
// (avatar-height column — fable.css sizes it to var(--fable-mes-avatar-h)
// so a long message NEVER grows it). Both roles share the GOLD corner
// pair — Delete 🗑 over Edit ✎, two large square gold plates mounted
// flush to the rail's top-OUTER corner (AI: top-right; user: top-left,
// mirrored in CSS). The variant nav is ROLE-SHAPED (2026-08-14 per
// Chloe): USER beats keep the vertical ▲ N/N ▼ capsule below the pair;
// AI beats instead get a ‹ N/N › bar pinned to the rail's BOTTOM — two
// big bold 2.5D metallic-gold PATH chevrons (SVG gradient stroke — no
// plates, no background-clip) flanking a silver metallic medieval-font
// fraction; at a SINGLE variant only › shows (CSS .multi gates ‹ + the
// fraction — their grid slots stay reserved so nothing re-flows). Both
// layouts emit the SAME data-drawer-act hooks
// (prev/next) + [data-drawer-count], so refreshDrawer() + the
// stage.js delegated router are layout-agnostic. There is NO dedicated
// Regenerate button (permanently removed 2026-08-14) — regeneration
// lives in the ▼/› arrow: it steps through the existing variants, then
// at the last variant FOLDS into a reroll (cutting the local tracker +
// switching back and forth — engine/drawer-logic.js pins the branch).
function buildDrawerHTML(role) {
  ensureChevDefs();
  const variantNav = role === 'user'
    ? `<div class="fable-mes-drawer-stepper-grp">
        <div class="fable-mes-drawer-stepper">
          <button type="button" class="fable-mes-drawer-btn" data-drawer-act="prev" aria-label="Previous variant" title="Previous variant" disabled>${ICO_CHEV_UP}</button>
          <span class="fable-mes-drawer-count" data-drawer-count>1/1</span>
          <button type="button" class="fable-mes-drawer-btn" data-drawer-act="next" aria-label="Next variant" title="Next variant">${ICO_CHEV_DOWN}</button>
        </div>
      </div>`
    : `<div class="fable-mes-drawer-variantbar">
        <button type="button" class="fable-mes-variant-chev" data-drawer-act="prev" aria-label="Previous variant" title="Previous variant" disabled>${ICO_VAR_PREV}</button>
        <span class="fable-mes-variant-count" data-drawer-count>1 / 1</span>
        <button type="button" class="fable-mes-variant-chev" data-drawer-act="next" aria-label="Next variant" title="Next variant">${ICO_VAR_NEXT}</button>
      </div>`;
  return `<div class="fable-mes-drawer" aria-hidden="true">
    <div class="fable-mes-drawer-inner">
      <div class="fable-mes-drawer-gold">
        <button type="button" class="fable-mes-gold-btn fable-mes-gold-btn--delete" data-drawer-act="delete" aria-label="Delete message" title="Delete">${ICO_TRASH}</button>
        <span class="fable-mes-gold-div" aria-hidden="true"></span>
        <button type="button" class="fable-mes-gold-btn fable-mes-gold-btn--edit" data-drawer-act="edit" aria-label="Edit message" title="Edit">${ICO_EDIT}</button>
      </div>
      ${variantNav}
    </div>
  </div>`;
}

// Sync the drawer's reactive bits to the beat's variant bookkeeping:
//   • stepper count text (active+1 / N) + ▲/▼ disabled states
//   • stepper visibility (.multi — hidden unless >1 variant)
// Pure DOM read/write off dataset.variantCount / variantActive. Cheap;
// called at build + on every stampVariants (reroll / swipe / load).
// (#84 2026-08-15) True iff `beat` is the TRAILING assistant message in the
// feed. Extracted from refreshDrawer so stage.js's delegated click handler
// can re-derive it AT CLICK TIME: the stamped disabled-state goes stale in
// both directions — a live-streamed beat's › is born disabled (refreshDrawer
// ran while the beat was still detached, pre-append), and a rebuilt feed's
// transiently-last beats keep an enabled › (they were stamped before the
// later beats appended). A stale-enabled › on a mid-history beat used to
// reroll the WRONG (trailing) turn.
export function isTrailingAssistant(beat) {
  if (!beat || !feedEl) return false;
  const asst = feedEl.querySelectorAll('.fable-mes.assistant');
  return asst.length > 0 && asst[asst.length - 1] === beat;
}

export function refreshDrawer(beat) {
  if (!beat) return;
  const drawer = beat.querySelector('.fable-mes-drawer');
  if (!drawer) return;
  const role = beat.dataset.role === 'user' ? 'user' : 'assistant';
  const count = Number.parseInt(beat.dataset.variantCount || '1', 10) || 1;
  const active = Number.parseInt(beat.dataset.variantActive || '0', 10) || 0;
  // isLastAssistant: this beat is the trailing assistant in the feed. It
  // gates the ▼ fold — at the last variant, ▼ on the TRAILING assistant
  // regenerates (cutting the local tracker + rolling a fresh variant)
  // instead of disabling. Computed from the DOM so a load or a delete
  // that changes the tail is reflected without extra plumbing (plus the
  // post-append + post-rebuild passes below, and the click-time re-check
  // in stage.js — the stamped state is advisory, the click gate is the
  // authority).
  const isLastAssistant = role === 'assistant' && isTrailingAssistant(beat);
  const { canPrev, canNext } = computeDrawerState({ role, count, active, isLastAssistant });

  drawer.classList.toggle('multi', count > 1);

  const countEl = drawer.querySelector('[data-drawer-count]');
  if (countEl) {
    // The AI bar's fraction is SPACED around the slash ("1 / 1", per
    // Chloe 2026-08-14); the user capsule stays compact ("1/1").
    const sep = countEl.classList.contains('fable-mes-variant-count') ? ' / ' : '/';
    countEl.textContent = `${active + 1}${sep}${count}`;
  }
  const prevBtn = drawer.querySelector('[data-drawer-act="prev"]');
  const nextBtn = drawer.querySelector('[data-drawer-act="next"]');
  if (prevBtn) prevBtn.disabled = !canPrev;
  if (nextBtn) {
    // ▼ folds into Regenerate at the last variant (the trailing
    // assistant's last roll → reroll, cutting the tracker). Keep it
    // enabled in that case; disabled only when neither a next variant
    // nor a reroll is possible.
    nextBtn.disabled = !canNext;
    nextBtn.title = (!canNext) ? 'Next variant'
      : (active >= count - 1 ? 'Regenerate' : 'Next variant');
  }
}

// Re-export the pure swipe-next decision + the pure disabled-state helper so
// stage.js's delegated handler routes › through the same logic the unit
// tests pin (one import path).
export { swipeNextAction, computeDrawerState, canEditMessage };

function appendMes(root) {
  if (!feedEl) return null;
  // Auto-stamp the backend message index (P1 fix): role-bearing beats
  // (user/assistant) map exactly to backend indexes. rebuildFromMessages +
  // the page loader stamp explicitly; the LIVE builders never did, so beats
  // appended since the last rebuild (the two newest — exactly the .vn-recent
  // toolrail targets) carried no index and edit/delete/swipe IPC-failed on
  // -1. (2026-08-19 paging) The stamp is now WINDOW-AWARE: the DOM renders
  // only [windowStart, sessionLen), so a bare DOM count under-counts by
  // windowStart the moment older messages are paged out — the invariant
  // windowStart + DOM-count === sessionLen makes this exact.
  if (root.dataset.index === undefined) {
    const role = root.dataset.role;
    if (role === 'user' || role === 'assistant') {
      root.dataset.index = String(windowStart + roleBeatsInDom().length);
    }
  }
  feedEl.appendChild(root);
  // (#84) Post-append drawer sync: buildMes ran refreshDrawer while the beat
  // was DETACHED, so a fresh assistant beat's › was born disabled (it wasn't
  // the tail yet — the live-turn reroll affordance was dead until the next
  // rebuild). Re-sync the new beat now that it IS the tail, + the assistant
  // that previously held the trailing › (its fold-into-reroll must retire
  // to this beat). Non-assistant appends don't change which assistant is
  // trailing — no sync needed.
  if (root.classList.contains('assistant')) {
    refreshDrawer(root);
    const asst = feedEl.querySelectorAll('.fable-mes.assistant');
    if (asst.length >= 2) refreshDrawer(asst[asst.length - 2]);
  }
  // (2026-08-19) Entrance: the beat's own rise-from-the-input-row keyframes
  // (fable.css) + the 1s scrollTop tween that pushes the whole train up
  // smoothly — replaces the old instant scrollDown snap.
  runEntranceScroll();
  return root;
}

function bodyEl(root) {
  return root ? root.querySelector('.fable-mes-text') : null;
}

// ── Public beat builders ─────────────────────────────────────────

// A user (player) beat. Uses the player portrait if present; otherwise
// a sleek silhouette placeholder fills the avatar frame (see buildMes).
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
  // Only auto-follow when the reader is already near the bottom — measure
  // BEFORE the append grows scrollHeight, so a reader scrolled up during a
  // live narration is never yanked down per chunk.
  const nearBottom =
    !feedEl || feedEl.scrollHeight - feedEl.scrollTop - feedEl.clientHeight < 80;
  beat.classList.add('streaming');
  // Stash raw on the dataset so the next append concatenates cleanly.
  const prev = beat.dataset.raw || '';
  const next = prev + String(text);
  beat.dataset.raw = next;
  body.innerHTML = renderMarkdown(next);
  if (nearBottom) scrollDown();
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

// Stamp the variant count + active index onto a beat's dataset (the variant
// bookkeeping a future per-beat UI will read). variants is the backend's
// Vec<String>; activeIdx is the 0-based active position.
export function stampVariants(beat, variants, activeIdx) {
  if (!beat) return;
  // count = variants.length (variants INCLUDES the active one — backend's
  // variant_count() == variants.len().max(1)). The prior `+ 1` was an
  // off-by-one (showed N+1 + let `>` swipe one past the real last variant).
  const count = variantCount(variants);
  beat.dataset.variantCount = String(count);
  beat.dataset.variantActive = String(activeIdx || 0);
  // Keep the hover toolrail's stepper + button states in sync with the
  // new bookkeeping (reroll / swipe / load all flow through here).
  refreshDrawer(beat);
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

// ── Slice regenerate (golden pencil, 2026-08-11) ─────────────────
// In-place partial regen of a highlighted span. The beat keeps its
// surrounding prose (pre/post) + gains a streaming span where the
// regenerated passage flows in token-by-token. On finalize the whole
// beat is re-rendered from the backend's authoritative final_text
// (pre + regen + post). On cancel the original dataset.raw is restored.

// Resolve a beat by message index (the slice path targets an arbitrary
// assistant message, not just the trailing one).
export function beatByIndex(index) {
  if (!feedEl) return null;
  return feedEl.querySelector(`.fable-mes[data-index="${index}"]`);
}

// Prepare a beat for an in-place slice re-stream: split its body into
// pre + a streaming span + post, mark .slice-regenerating, capture the
// original raw for the cancel path. Returns the streaming span (or null).
export function beginSliceRegen(beat, { pre, post }) {
  if (!beat) return null;
  const body = bodyEl(beat);
  if (!body) return null;
  // Cancel-restore anchor: the pre-regen full prose.
  beat.dataset.sliceOriginalRaw = beat.dataset.raw != null ? beat.dataset.raw : body.textContent;
  beat.classList.add('slice-regenerating');
  body.innerHTML =
    renderMarkdown(pre) +
    '<span class="fable-slice-streaming" data-empty="1"></span>' +
    renderMarkdown(post);
  return body.querySelector('.fable-slice-streaming');
}

// Append a streamed chunk into the slice span. The span holds raw text
// (escaped on read via textContent); finalizeBeat re-renders the whole
// beat from the final string, so no markdown pass is needed mid-stream.
export function streamSliceChunk(span, piece) {
  if (!span || piece == null) return;
  // (2026-08-15 audit fix) Same nearBottom guard as appendChunk: a reader
  // scrolled up during a golden-pencil regen must not be yanked down per
  // chunk. Measure BEFORE the append grows scrollHeight.
  const nearBottom =
    !feedEl || feedEl.scrollHeight - feedEl.scrollTop - feedEl.clientHeight < 80;
  span.textContent += String(piece);
  span.removeAttribute('data-empty');
  if (nearBottom) scrollDown();
}

// Finalize: re-render the whole beat from the authoritative final text
// (pre + regen + post). Drops the .slice-regenerating state + the
// cancel anchor.
export function finalizeSliceRegen(beat, finalText) {
  if (!beat) return;
  const body = bodyEl(beat);
  const text = finalText != null ? finalText : (beat.dataset.sliceOriginalRaw || beat.dataset.raw || '');
  if (body) body.innerHTML = renderMarkdown(text);
  beat.dataset.raw = text;
  beat.classList.remove('slice-regenerating');
  delete beat.dataset.sliceOriginalRaw;
}

// Cancel/restore: put the beat back to its pre-regen prose.
export function cancelSliceRegen(beat) {
  if (!beat) return;
  const body = bodyEl(beat);
  const original = beat.dataset.sliceOriginalRaw != null
    ? beat.dataset.sliceOriginalRaw
    : (beat.dataset.raw || '');
  if (body) body.innerHTML = renderMarkdown(original);
  beat.dataset.raw = original;
  beat.classList.remove('slice-regenerating');
  delete beat.dataset.sliceOriginalRaw;
}

// ── Feed lifecycle ───────────────────────────────────────────────

// Build one beat node from a backend message shape (role/content/
// variants/active_idx/timestamp). Shared by the rebuild window + the
// upward page loader so both paths render byte-identical cards.
function buildBeatFromMessage(m, i) {
  const role = m.role === 'user' ? 'user' : 'assistant';
  const name = role === 'user'
    ? (identity.playerName || 'You')
    : (identity.cardName || 'Narrator');
  const portrait = role === 'user'
    ? (identity.playerPortrait || '')
    : (identity.cardPortrait || '');
  const root = buildMes({ role, name, portraitUrl: portrait, index: i, timestamp: m.timestamp });
  const body = bodyEl(root);
  if (body) body.innerHTML = renderMarkdown(m.content || '');
  root.dataset.raw = m.content || '';
  // Variant stamp via stampVariants so the hover toolrail refreshes too.
  // Only assistant turns carry variants; user turns are always 1/1.
  if (role === 'assistant') {
    stampVariants(root, Array.isArray(m.variants) ? m.variants : [], m.active_idx || 0);
  } else {
    stampVariants(root, [], 0);
  }
  return root;
}

// Wipe + rebuild the feed from a message list (the backend's source of
// truth on load/resume/edit/rewind/delete). Each message → one card;
// assistant messages with variants get the swipe bar stamped.
// (2026-08-19 paging) Only the TRAILING FEED_WINDOW messages render
// (48 history + the 2 recent) — older pages prepend on demand as the
// player scrolls up (see loadOlderPage). Mutations always land here
// fresh from a backend response, so the window resets to the tail +
// snaps to the bottom, exactly as before.
export function rebuildFromMessages(messages) {
  if (!feedEl) return;
  scrollGen++;
  cancelEntrance();
  pageLoading = false;   // a rebuild supersedes any in-flight page load
  feedEl.innerHTML = '';
  pagebarEl = null;
  // The rebuilt beats are fresh nodes — any open editor lived on a node we
  // just destroyed. Drop the single-editor ref (defensive: the entry paths
  // commit before mutating actions, so an editor should never be open
  // across a rebuild; a stale commit against shifted indexes would be
  // worse than dropping it).
  editingBeat = null;
  if (!Array.isArray(messages)) {
    windowStart = 0;
    sessionMsgs = null;
    return;
  }
  sessionMsgs = messages;
  windowStart = Math.max(0, messages.length - FEED_WINDOW);
  for (let i = windowStart; i < messages.length; i++) {
    feedEl.appendChild(buildBeatFromMessage(messages[i], i));
  }
  // (#84) Final drawer sync pass: the loop stamps each beat while the LATER
  // beats aren't appended yet, so every transiently-last assistant kept an
  // enabled › (› folds into reroll ONLY on the true trailing beat). One
  // cheap pass re-derives the tail state from the now-complete DOM.
  feedEl.querySelectorAll('.fable-mes.assistant').forEach(refreshDrawer);
  scrollDown();
}

// Read a beat body's text content (used by the edit path to seed the
// inline editor with the current prose).
export function getBeatText(beat) {
  if (!beat) return '';
  const body = bodyEl(beat);
  return body ? body.textContent : '';
}

// In-progress editors keyed by beat element, so an outside actor — the ✎
// button's SECOND click (the edit toggle, 2026-08-14) — can close the editor
// without reaching into the textarea. WeakMap: a destroyed beat's closer is
// GC'd with the element; the closer unregisters itself on close.
const editClosers = new WeakMap();

// SINGLE-EDITOR DISCIPLINE (2026-08-14): the feed allows exactly ONE inline
// editor at a time. `editingBeat` is the beat whose editor is open (a strong
// ref, cleared in close()); every entry path (✎ / dblclick) commits any
// other open editor first via commitOpenEditor() — a second editor would
// otherwise get silently vaporized (with its in-progress text) the moment
// the first save's feed rebuild lands.
let editingBeat = null;

// True while a beat's inline editor is open.
export function isEditing(beat) {
  return !!(beat && beat.classList.contains('editing'));
}

// The beat whose inline editor is currently open (or null). Lets callers
// inspect the pending editor (e.g. its role — a user-beat save rewinds the
// timeline) before committing it.
export function openEditingBeat() {
  return editingBeat && isEditing(editingBeat) ? editingBeat : null;
}

// Close a beat's open editor from outside the textarea. commit=true SAVES
// (the ✎-toggle's second press + the dblclick exit — the Enter path);
// commit=false cancels (Esc — restore the original prose). Returns the
// onSave promise when committing (the caller may await it — the save runs
// the backend tracker re-track), a falsy null when the beat wasn't editing
// (a no-op second press).
export function exitEditMode(beat, commit = false) {
  if (!isEditing(beat)) return null;
  const close = editClosers.get(beat);
  if (typeof close === 'function') return close(commit) || null;
  return null;
}

// Commit the ONE open editor (if any) + return its onSave promise (or null).
// The single-editor handoff: entering an edit on another beat — or firing
// any feed-mutating drawer action — settles the open editor FIRST so its
// text is never lost to a rebuild. Await the returned promise before
// opening the next editor: the save may rebuild the feed (a user-beat save
// rewinds + regenerates, which can TRUNCATE beats).
export function commitOpenEditor() {
  if (!editingBeat || !isEditing(editingBeat)) {
    editingBeat = null;
    return null;
  }
  const beat = editingBeat;
  const close = editClosers.get(beat);
  if (typeof close !== 'function') {
    editingBeat = null;
    return null;
  }
  return close(true) || null;
}

// Swap a beat's body for an inline <textarea>. onSave(text) commits (its
// return value — a promise — propagates out of exitEditMode/
// commitOpenEditor so callers can await the save); onCancel() restores the
// original prose. Enter commits (Shift+Enter is a newline); Esc cancels.
// Used for both in-place user edits + assistant rewind-and-edit (the caller
// picks the commit path). Callers enforce the single-editor rule via
// commitOpenEditor() before calling this.
// (D5 2026-08-16) opts.seed — a string overriding the textarea's initial
// text. The session-changed restore path (narrator.js) re-opens an editor
// with the player's in-progress edit after a feed rebuild, not the beat's
// committed prose. Esc still cancels to `original` — cancel always means
// revert to committed.
export function enterEditMode(beat, opts = {}) {
  if (!beat) return;
  const body = bodyEl(beat);
  if (!body) return;
  const original = beat.dataset.raw != null ? beat.dataset.raw : body.textContent;
  const seed = typeof opts.seed === 'string' ? opts.seed : original;
  const ta = document.createElement('textarea');
  ta.className = 'fable-mes-editor';
  ta.value = seed;
  ta.rows = Math.max(3, Math.min(14, seed.split('\n').length + 1));
  body.innerHTML = '';
  body.appendChild(ta);
  beat.classList.add('editing');
  editingBeat = beat;
  ta.focus();
  ta.selectionStart = ta.value.length;
  ta.selectionEnd = ta.value.length;

  const close = (commit) => {
    editClosers.delete(beat);
    if (editingBeat === beat) editingBeat = null;
    beat.classList.remove('editing');
    if (commit) {
      const text = ta.value;
      body.innerHTML = renderMarkdown(text);
      beat.dataset.raw = text;
      if (typeof opts.onSave === 'function') {
        const saving = opts.onSave(text);
        // (P2b, 2026-08-17 E4B shakedown) The optimistic write above must not
        // survive a REFUSED save: editMessage/rewindAndEditUser catch the
        // backend error internally + resolve `false` (the refusal is
        // surfaced as an error beat), which used to leave this beat showing
        // the never-saved text — or blank — until some later feed rebuild.
        // Restore the committed prose when the save reports failure.
        Promise.resolve(saving).then((ok) => {
          if (ok === false && bodyEl(beat)) {
            bodyEl(beat).innerHTML = renderMarkdown(original);
            beat.dataset.raw = original;
          }
        }).catch(() => {
          if (bodyEl(beat)) {
            bodyEl(beat).innerHTML = renderMarkdown(original);
            beat.dataset.raw = original;
          }
        });
        return saving;
      }
    } else {
      body.innerHTML = renderMarkdown(original);
      beat.dataset.raw = original;
      if (typeof opts.onCancel === 'function') opts.onCancel();
    }
    return null;
  };
  editClosers.set(beat, close);

  ta.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); close(true); }
    else if (e.key === 'Escape') { e.preventDefault(); close(false); }
  });
}
