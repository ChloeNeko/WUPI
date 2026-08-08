// =============================================================
// FABLE BEATS — dialogue feed rendering (pure DOM, vanilla).
//
// Beat types map to the channel-event stream from fable_send:
//   narrator  → glass card with clean prose (the AI/Game Master), streams live.
//   character → speaker-labeled NPC line (from CHARACTER_TURN).
//   user      → glass bubble (right side) for the player's action.
//   system    → small state-change beat (from OBJECT tags + saves).
//   error     → red beat for generation failures.
//
// LAYOUT (card-based overhaul, 2026-08-01): each beat is a flex ROW of
// [avatar] + [card], aligned LEFT (AI/narrator/character/error) or RIGHT
// (user, mirrored so the avatar sits on the outer edge). Capped at 85% of
// the feed. The card carries a hover-only .message-header (name + time +
// action controls) — the feed stays clean until a card is hovered.
//
// STREAMING (ECHO rewrite): there is NO blinking caret. appendChunk()
// tokenizes the incoming delta into words and wraps each newly-arrived word
// in a .echo-word span that fades + lifts in once — text "pops in" like
// reading from a live narrative. finalizeBeat() just drops the .streaming
// class.
//
// STRUCTURAL CONTRACT (consumed by narrator.js + stage.js):
//   .fable-beat[role][data-index]   = the flex row (alignment + entrance anim)
//     .message-avatar               = 80×120 portrait (svg silhouette; img layer)
//     .message-card                 = glass card
//       .message-header             = hover-only: .msg-name + .msg-time + actions
//       .fable-beat-body             = the prose (streaming/finalize/variant/edit)
//   .fable-beat-card is kept as a class alias on the card element so the
//   legacy querySelector('.fable-beat-card') sites keep resolving.
// =============================================================

let feed = null;  // #fable-dialogue-feed

// Monotonic counter stamping `data-index` on every beat so the UX chat
// controls (edit / reroll / rewind-and-edit) can address messages by their
// position in the conversation. Reset in `clearFeed`. The counter tracks the
// logical message order, which mirrors the backend `Conversation::messages`
// order at render time (loadHistory / chunk append) — DOM order is
// chronological (appendChild), so data-index matches the backend position.
let nextIndex = 0;

// Identity for the beat headers. Set by the host (stage.js) via
// setIdentity() from the active card. Card name → AI/narrator beats; player
// name → user beats. Both fall back gracefully when unset (the header just
// omits the name line).
let cardName = '';     // the seated card's display name (the GM persona)
let playerName = '';   // the protagonist's name (card.player_name)

export function initBeats(feedEl) {
  feed = feedEl;
}

// Host-side identity setter (called from stage.js once the active card is
// known). Kept separate from initBeats so a late-arriving name (the
// fable_active_card_get fetch is async) doesn't race feed init.
export function setIdentity({ cardName: cn, playerName: pn } = {}) {
  if (typeof cn === 'string') cardName = cn;
  if (typeof pn === 'string') playerName = pn;
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

// Escape + preserve line breaks + wrap quoted speech in .dialogue spans for
// the visual-novel coloring (accent + italic). Handles straight quotes ("...")
// and smart/curly quotes ("..." / '...'). Used by BOTH the streaming renderer
// and finalizeBeat so a live beat + its finalized form look identical.
//
// The dialogue wrap is a single regex pass over the ESCAPED string (so the
// quotes are already &quot;/&#39; entities and can't break out of the span).
// We deliberately keep this cheap — one replace, no lookahead stacks.
function renderProse(s) {
  const escaped = esc(s);
  // Straight double quotes: "&quot;...&quot;"  (the most common narrator form).
  // Smart double quotes (left/right U+201C/U+201D) and single quotes are
  // handled as their raw chars (they weren't entity-encoded above since esc()
  // only touches the 5 XML specials).
  return escaped
    .replace(/&quot;([^&]*?(?:&(?!quot;)[^&]*?)*)&quot;/g, '<span class="dialogue">&quot;$1&quot;</span>')
    .replace(/\u201C([^\u201D]*)\u201D/g, '<span class="dialogue">\u201C$1\u201D</span>')
    .replace(/(^|\s)\u2018([^\u2019]*)\u2019/g, '$1<span class="dialogue">\u2018$2\u2019</span>')
    .replace(/\n/g, '<br>');
}

// Back-compat prose() — escapes + newlines only, no dialogue wrap. Kept for
// the rare path that must NOT colorize (e.g. raw system text already shown
// in a system beat). The streaming + finalize paths use renderProse.
function prose(s) {
  return esc(s).replace(/\n/g, '<br>');
}

// Keep the newest beat in view. The feed is a normal top-to-bottom column,
// so scroll to the bottom (the newest beat sits just above the input row).
export function scrollDown() {
  if (!feed) return;
  feed.scrollTop = feed.scrollHeight;
}

// --- Avatar rendering -------------------------------------------------------
// There is NO portrait art in the project today (cards have no image field;
// the user profile is name+description only). So every avatar renders the
// inline-SVG silhouette fallback. The slot is built behind `resolveAvatar`,
// which returns null today — when art lands later, point it at a URL and the
// <img> layers over the svg automatically (onerror hides it → svg shows).
//
// `variant` picks one of two geometric silhouettes: 'player' (a hooded/
// traveler bust) vs 'npc' (a broader shoulders bust). Both are pure
// currentColor paths so they tint with the message-type accent (CSS).
function resolveAvatar(/* role, identity */) {
  return null;  // no art yet — the svg fallback always shows.
}

function silhouetteSvg(variant) {
  // Single-line SVGs (matches the footer-icon convention in stage.js).
  // viewBox 0 0 24 24, fill none, stroke currentColor, round caps/joins.
  if (variant === 'player') {
    // Hooded traveler: a hood + head + shoulders, reads as a protagonist.
    return '<svg viewBox="0 0 24 24" aria-hidden="true" preserveAspectRatio="xMidYMid slice">'
      + '<path d="M12 4c-2.8 0-5 2.2-5 5 0 1.7.8 3.1 2 4-2.2.8-4 2.6-4.6 5L4 21h16l-.4-3c-.6-2.4-2.4-4.2-4.6-5 1.2-.9 2-2.3 2-4 0-2.8-2.2-5-5-5z"'
      + ' fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round"/></svg>';
  }
  // NPC / AI: a broader shoulders + head bust (the GM's neutral silhouette).
  return '<svg viewBox="0 0 24 24" aria-hidden="true" preserveAspectRatio="xMidYMid slice">'
    + '<circle cx="12" cy="8.5" r="3.6" fill="none" stroke="currentColor" stroke-width="1.4"/>'
    + '<path d="M4.5 21c.5-3.6 3.6-6 7.5-6s7 2.4 7.5 6" fill="none" stroke="currentColor'
    + '" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>';
}

// Build the avatar block: the always-present svg fallback + an optional img
// layer (shown only if resolveAvatar returns a URL). The img's onerror hides
// it so a broken/missing image falls straight back to the svg. `role` is the
// beat's wire role; `identity` is the name used to resolve art later.
function avatarMarkup(role, identity) {
  const variant = role === 'user' ? 'player' : 'npc';
  const svg = silhouetteSvg(variant);
  const url = resolveAvatar(role, identity);
  const img = url ? `<img src="${esc(url)}" alt="" onerror="this.style.display='none'" />` : '';
  return `<div class="message-avatar" aria-hidden="true">${svg}${img}</div>`;
}

// Format an epoch-ms timestamp into a short locale time string for the header.
// Null/0/NaN → '' (the time line is omitted entirely when there's no ts).
function formatTime(ms) {
  if (!ms || !Number.isFinite(ms)) return '';
  try {
    return new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  } catch (_) {
    return '';
  }
}

// Build a beat's full innerHTML: avatar + card(header + body). `role` is the
// wire role ('user' | 'assistant'); `name` is the header name; `ts` is the
// optional epoch-ms timestamp; `bodyHtml` is the prose HTML for the body.
// `extraHeader` injects additional header nodes (none today; reserved).
function beatRowHtml({ role, name, ts, bodyHtml, avatarIdentity }) {
  const avatar = avatarMarkup(role, avatarIdentity || name);
  const timeLine = formatTime(ts) ? `<span class="msg-time">${esc(formatTime(ts))}</span>` : '';
  const nameLine = name ? `<span class="msg-name">${esc(name)}</span>` : '';
  return `${avatar}<div class="message-card fable-beat-card">`
    + `<div class="message-header">${nameLine}${timeLine}`
    + `<span class="message-actions"></span></div>`
    + `<div class="fable-beat-body">${bodyHtml}</div></div>`;
}

export function addUserBeat(text, opts = {}) {
  const b = document.createElement('div');
  b.className = 'fable-beat user';
  // Live beats (just sent) get a fresh timestamp; loaded history passes ts.
  const ts = opts.ts || Date.now();
  const name = playerName || 'You';
  b.innerHTML = beatRowHtml({ role: 'user', name, ts, bodyHtml: renderProse(text), avatarIdentity: name });
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'user');
}

export function addSystemBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat system';
  // System beats skip the card + avatar — they're de-emphasized status lines.
  b.innerHTML = `<div class="fable-beat-body">${esc(text)}</div>`;
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'system');
}

export function addErrorBeat(text) {
  const b = document.createElement('div');
  b.className = 'fable-beat error';
  const ts = Date.now();
  b.innerHTML = beatRowHtml({ role: 'assistant', name: 'Error', ts, bodyHtml: esc(text) });
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'system');
}

// Start a streaming narrator beat. Returns the beat element so the
// caller can append chunks and finalize it.
export function startNarratorBeat(opts = {}) {
  const b = document.createElement('div');
  b.className = 'fable-beat narrator streaming';
  const ts = opts.ts || Date.now();
  const name = opts.name || cardName || 'Game Master';
  b.innerHTML = beatRowHtml({ role: 'assistant', name, ts, bodyHtml: '', avatarIdentity: name });
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'assistant');
}

// Start a streaming character beat with a speaker label. `speakerLabel`
// becomes the .msg-name (the NPC speaking); the avatar stays the npc
// silhouette (no per-NPC art yet).
export function startCharacterBeat(speakerLabel) {
  const b = document.createElement('div');
  b.className = 'fable-beat character streaming';
  const ts = Date.now();
  b.innerHTML = beatRowHtml({ role: 'assistant', name: speakerLabel, ts, bodyHtml: '', avatarIdentity: speakerLabel });
  feed.appendChild(b);
  scrollDown();
  return stamp(b, 'assistant');
}

// Append a streamed text chunk to a beat (narrator or character). The chunk
// is accumulated into `beat._raw`; only the newly-arrived words (the tail
// since the last render) are wrapped in `.echo-word` spans so they fade +
// lift in once — the "text popping in from a narrative" effect. Words that
// were already rendered stay still (their .echo-word animation already ran).
//
// Throttled via requestAnimationFrame so a fast token stream coalesces to one
// render per frame (the animation is on the new spans, not per-token).
export function appendChunk(beat, text) {
  if (!beat || !text) return;
  beat._raw = (beat._raw || '') + text;
  if (beat._rafPending) return;
  beat._rafPending = true;
  requestAnimationFrame(() => {
    beat._rafPending = false;
    renderStreamingBody(beat);
    scrollDown();
  });
}

// Render the streaming body: the already-shown prefix as plain rendered prose,
// the new tail words wrapped in .echo-word spans. `_renderedChars` tracks how
// much of `_raw` has been rendered without animation so we only animate the
// delta. Tokenization is whitespace-based (split on spaces, keep delimiters);
// we animate whole words, never fragments.
//
// NOTE: the streaming path does NOT run the dialogue-color wrap on the tail
// (a partial quote mid-stream would flicker as it closes). The prefix uses
// renderProse so finished dialogue colors as it scrolls out of the tail; the
// finalize pass re-renders the whole body with renderProse for the clean end
// state. This keeps streaming cheap + flicker-free.
function renderStreamingBody(beat) {
  const body = beat.querySelector('.fable-beat-body');
  if (!body) return;
  const raw = beat._raw || '';
  const renderedChars = beat._renderedChars || 0;
  // The prefix (already shown) + the new tail (to animate).
  const prefix = raw.slice(0, renderedChars);
  const tail = raw.slice(renderedChars);
  // Tokenize the tail into words + whitespace runs (preserve both so spacing
  // + line breaks survive). A "word" is a maximal run of non-whitespace; a
  // "space" is a maximal run of whitespace.
  const parts = [];
  const re = /(\s+)|(\S+)/g;
  let m;
  while ((m = re.exec(tail)) !== null) {
    if (m[1] != null) parts.push({ space: true, text: m[1] });
    else parts.push({ space: false, text: m[2] });
  }
  // Build the HTML: rendered prefix (with <br> for newlines) + the tail parts,
  // each non-space part wrapped in a .echo-word span. Spaces are escaped too
  // (newlines → <br>).
  let html = renderProse(prefix);
  for (const p of parts) {
    if (p.space) {
      html += p.text.includes('\n') ? renderProse(p.text) : esc(p.text);
    } else {
      html += `<span class="echo-word">${esc(p.text)}</span>`;
      // Put a space back between animated words (the whitespace run was
      // already emitted above when present); if two words abut with no space
      // (rare), they'll just join.
    }
  }
  body.innerHTML = html;
  beat._renderedChars = raw.length;
}

// Finalize: drop the .streaming class + render the final text cleanly (no
// .echo-word spans — the finished beat is plain prose with dialogue coloring).
// Cancels any pending rAF so a late frame can't overwrite the finalized HTML.
// `reasoning` (unused post-2026-08-07 override): the player-facing reasoning UI
// was removed; the API narrator never emits a thought channel anyway.
export function finalizeBeat(beat, finalText, reasoning) {
  void reasoning;
  if (!beat) return;
  if (beat._rafPending) {
    beat._rafPending = false;
  }
  beat.classList.remove('streaming');
  const body = beat.querySelector('.fable-beat-body');
  if (!body) return;
  if (finalText != null) {
    body.innerHTML = renderProse(finalText);
    beat._raw = finalText;
  } else if (beat._raw != null) {
    body.innerHTML = renderProse(beat._raw);
  }
  beat._renderedChars = (beat._raw || '').length;
  scrollDown();
}

// =============================================================
// SWIPEABLE VARIANTS (2026-07-29) — the ‹ 1/N › UX.
// Three beat-level helpers used by narrator.js's reroll + swipe paths.
// All three mutate a single beat in place (no feed wipe) — mirroring the
// §11.29 selective-regenerate splice that proved single-beat mutation
// works without the full-rebuild flash.
// =============================================================

// The last assistant/narrator beat in the feed, or null. Used by the reroll
// path to claim the existing beat as the streaming target (so the new
// variant renders over the old prose in place).
export function lastNarratorBeat() {
  if (!feed) return null;
  const all = feed.querySelectorAll('.fable-beat.narrator, .fable-beat.character');
  return all.length ? all[all.length - 1] : null;
}

// Prepare a beat for an in-place reroll: drop into streaming state + clear
// its body + reset the raw-chunk accumulator so the new variant streams in
// fresh. The old text was already stashed into the message's `variants` by
// the backend (`fable_send` reroll=true), so clearing the DOM is safe.
export function beginReroll(beat) {
  if (!beat) return;
  beat._raw = '';
  beat._renderedChars = 0;
  beat.classList.add('streaming', 'regenerating');
  const body = beat.querySelector('.fable-beat-body');
  if (body) body.innerHTML = '';
}

// Swap a beat's displayed body to a different variant's content (the swipe-
// left/right action). Finds the beat by `data-index`, re-renders its body
// from `content`, clears the raw accumulator so a subsequent finalize won't
// restore stale text. No feed rebuild.
export function swapVariantBody(index, content) {
  if (!feed) return;
  const beat = feed.querySelector(`.fable-beat[data-index="${index}"]`);
  if (!beat) return;
  beat._raw = content || '';
  beat._renderedChars = (content || '').length;
  const body = beat.querySelector('.fable-beat-body');
  if (body) body.innerHTML = renderProse(content || '');
  scrollDown();
}

// Re-class a live narrator beat as a character beat when a
// CHARACTER_TURN bracket arrives mid-stream. The NPC speaker label becomes
// the .msg-name (replacing the card name in the header).
// MVP limitation (AGENTS.md §11.10): a second CHARACTER_TURN in the
// same narrator turn overwrites the first speaker label.
export function reclassToCharacter(beat, speakerLabel) {
  if (!beat) return;
  beat.classList.remove('narrator');
  beat.classList.add('character');
  const card = beat.querySelector('.message-card');
  if (!card) return;
  const nameEl = card.querySelector('.msg-name');
  if (nameEl) {
    nameEl.textContent = speakerLabel;
  } else {
    // No header name yet (edge: reclass before header built) — inject one.
    const header = card.querySelector('.message-header');
    if (header) {
      const span = document.createElement('span');
      span.className = 'msg-name';
      span.textContent = speakerLabel;
      header.insertBefore(span, header.firstChild);
    }
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
// content: string, timestamp?, variants?, active_idx? }]` (same shape
// `fable_quick_resume` / `edit_message` / `reroll_last_turn` /
// `rewind_and_edit_user` return). `timestamp` is OPTIONAL on the wire
// (the Rust FableLoadMessage gains it alongside this UI; until then it's
// absent → the header just omits the time line). Assistant messages are
// finalized narrator beats (no streaming caret, no chunk-by-chunk); the
// bracket-parser does NOT re-fire on a rebuild (it only fires during live
// streaming).
// =============================================================
export function rebuildFromMessages(messages) {
  if (!feed) return;
  clearFeed();
  for (const m of messages || []) {
    if (m.role === 'user') {
      addUserBeat(m.content, { ts: m.timestamp });
    } else if (m.role === 'assistant') {
      const b = startNarratorBeat({ ts: m.timestamp });
      finalizeBeat(b, m.content);
      // Stamp the swipeable-variant state (2026-07-29) so refreshControls can
      // render the ‹ 1/N › bar without a separate message cache. `variants` on
      // the wire holds the INACTIVE siblings; total = variants.length + 1.
      stampVariants(b, m.variants, m.active_idx);
    } else {
      addSystemBeat(m.content);
    }
  }
  scrollDown();
}

// Record the variant state on a beat's dataset. `variants` is the array of
// inactive siblings (matches the wire shape); activeIdx is the 0-indexed
// position. Used by rebuildFromMessages + the reroll/swipe paths so the
// delegated controls renderer can read it straight off the DOM.
export function stampVariants(beat, variants, activeIdx) {
  if (!beat) return;
  const count = Array.isArray(variants) ? variants.length + 1 : 1;
  beat.dataset.variantCount = String(count);
  beat.dataset.activeVariant = String(Number.isInteger(activeIdx) ? activeIdx : 0);
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
// UX CHAT CONTROLS — the SillyTavern-style ‹ n/N › swipe bar, now folded
// INTO the hover-only .message-header (2026-08-01 overhaul). The old
// circular-arrow regenerate button is GONE.
//
// `renderControls` injects a `.fable-beat-controls` element (the swipe bar)
// into the beat's `.message-actions` slot (inside the header). Because the
// header is opacity:0 until .message-card:hover, the bar is hover-gated too
// — the feed stays clean until a card is hovered. Each button carries
// `data-action` so a single delegated click handler on the feed can dispatch
// (stage.js).
//
// Buttons shown:
//   - swipe bar (‹ n/N ›): on assistant beats. ‹ steps back (undo —
//     unlimited), › steps forward, and › on the last variant REGENERATES
//     (a new variant; the Vec is uncapped).
//
// Editing is the header's ✎ button (data-action="edit") on user beats +
// the last assistant beat (stage.js wires the handler). Double-click-to-edit
// is ALSO kept as a power-user shortcut (same beginEdit routing).
// =============================================================

// `isLastBeat`: when true, the › arrow on the last variant becomes the
// regenerate action. `canRegenerate`: when true (only meaningful on the last
// beat), the › arrow at the last variant is armed as a regenerate trigger
// rather than disabled. `canEdit`: when true, an edit (✎) button is added
// (user beats + the last assistant beat).
export function renderControls(beat, {
  variantCount = 1,
  activeVariant = 0,
  isLastBeat = false,
  canRegenerate = false,
  canEdit = false,
} = {}) {
  if (!beat) return;
  const card = beat.querySelector('.message-card');
  if (!card) return; // system beats have no card → no controls.
  const actions = card.querySelector('.message-actions');
  if (!actions) return;
  // Idempotent: clear the actions slot before injecting (so refreshControls
  // can re-stamp without accumulating).
  actions.innerHTML = '';

  // Edit button (✎). Present on user beats + the last assistant beat. Shares
  // the swipe-btn chrome family. The click is dispatched via data-action.
  if (canEdit) {
    const edit = document.createElement('button');
    edit.type = 'button';
    edit.className = 'msg-action-btn';
    edit.dataset.action = 'edit';
    edit.title = 'Edit';
    edit.setAttribute('aria-label', 'Edit message');
    edit.innerHTML = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 20l4-1L19 8l-3-3L5 16l-1 4z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><path d="M14 7l3 3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></svg>';
    actions.appendChild(edit);
  }

  // The swipe bar renders whenever there's more than one variant, OR this is
  // the last beat (so the regenerate › is reachable even on a fresh single-
  // variant assistant message — matches SillyTavern's always-present chevrons).
  const showBar = variantCount > 1 || isLastBeat;
  if (!showBar) return;
  const wrap = document.createElement('div');
  wrap.className = 'fable-beat-controls';
  if (!isLastBeat) wrap.classList.add('is-hidden'); // earlier beats: hover-gated via header.

  const bar = document.createElement('div');
  bar.className = 'fable-swipe-bar';

  // ‹ (left): step back to the previous variant (undo). Disabled at variant 0.
  const left = document.createElement('button');
  left.type = 'button';
  left.className = 'fable-swipe-btn';
  left.dataset.action = 'swipe-left';
  left.dataset.targetVariant = String(Math.max(0, activeVariant - 1));
  left.title = 'Previous variant';
  left.setAttribute('aria-label', 'Previous variant');
  left.innerHTML = '&#8249;';
  if (activeVariant === 0) left.disabled = true;

  const count = document.createElement('span');
  count.className = 'fable-swipe-count';
  count.textContent = `${activeVariant + 1}/${variantCount}`;

  // › (right): step forward; if on the LAST variant + this is the last beat +
  // regenerate is allowed, this is the REGENERATE trigger (a new variant is
  // generated + appended; variants are uncapped). Otherwise disabled at the
  // last variant (earlier beats can't spawn new variants).
  const right = document.createElement('button');
  right.type = 'button';
  right.className = 'fable-swipe-btn';
  const onLastVariant = activeVariant === variantCount - 1;
  if (onLastVariant && isLastBeat && canRegenerate) {
    right.dataset.action = 'regenerate';
    right.title = 'Regenerate response';
    right.setAttribute('aria-label', 'Regenerate response');
    right.classList.add('is-regenerate');
  } else {
    right.dataset.action = 'swipe-right';
    right.dataset.targetVariant = String(Math.min(variantCount - 1, activeVariant + 1));
    right.title = 'Next variant';
    right.setAttribute('aria-label', 'Next variant');
    if (onLastVariant) right.disabled = true;
  }
  right.innerHTML = '&#8250;';

  bar.appendChild(left);
  bar.appendChild(count);
  bar.appendChild(right);
  wrap.appendChild(bar);
  actions.appendChild(wrap);
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
// header) so the textarea is the sole focus.
// =============================================================
export function enterEditMode(beat, { onSave, onCancel } = {}) {
  if (!beat || beat.classList.contains('editing')) return;
  const card = beat.querySelector('.message-card');
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
