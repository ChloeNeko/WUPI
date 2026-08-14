// =============================================================
// CREATOR CHAT — the GLM-driven conversational authoring screen.
//
// A reusable shell that drives the three creation wizards (player, sim
// world, codex) through a chat with the API. GLM fills a JSON `draft`
// whose keys match the creator's field schema; it NEVER writes XML or
// files. Each turn it emits an envelope:
//   { "action":"ask", "message":..., "questions":[...], "draft":{...} }
//   { "action":"ready", "draft":{...full...} }
// On `ready`, the draft loads into an in-screen review card (with a
// portrait slot → cropper). CREATE serializes via card-serialize.js +
// writes through the existing IPCs (fable_player_write / fable_write_card
// / fable_card_sibling_write / fable_card_portrait_write). Mechanical
// integrity stays in JS/Rust — Prime-Mandate compliant.
//
// This is a CREATION-ONLY API role (AGENTS.md §3A, 2026-08-12 override):
// outside the runtime game loop. The IPC `creator_assistant_turn` does
// the one-shot HttpBackend call (no tracker, no schema, no world state).
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { openPortraitCropper } from './portrait-cropper.js';
import { bytesToBase64, ARROW_SVG_LEFT } from './wizard-engine.js';
import {
  serializePlayer,
  serializeSimCard,
  codexEntriesToCompound,
  slugify,
  escapeXml,
} from './card-serialize.js';
import {
  parseEnvelope,
  stripToJsonFallback,
  mergeDraft,
  buildReviewSections,
  buildIdCard,
} from '../engine/creator-engine.js';
import { renderIdCard, wireIdCard } from './id-card.js';

const GREETINGS = {
  player: "Describe your character in detail and I'll help you design your PLAYER Card. Be as vague or descriptive as you'd like and I'll help guide you.",
  sim: "Start by telling me whether your SIM Card is a character, a scenario, or perhaps a whole new world? Describe your SIM Card in detail, you may be as descriptive or as vague as you'd like and I'll help guide you.",
  codex: "The CODEX is the facts of the simulation which is unique to your SIM Card only. This information can be accessed at any time by your narrator. You may start by giving me a detailed list or vague ideas and I'll help you craft the lore.",
};

// Build the screen element (registered once in fable.js).
export function buildCreatorChat() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-creator-chat-screen';
  root.dataset.fableScreen = 'creator-chat';
  root.hidden = true;
  // 2026-08-13 Chloe: the chat UI was a retired empty shell. 2026-08-13 Chloe
  // pass #2: rebuilt as the minimal two-block layout — a single AI prompt
  // block + a single textarea (no chat log, no bubbles). The screen reuses
  // the wizard centering (.fable-player-wizard) + the glowing divider, so it
  // matches the rest of the creator suite aesthetically. The renderCreatorChat
  // backend (GLM turn loop, envelope parse → draft → review → CREATE) binds to
  // the [data-*] hooks below unchanged.
  root.innerHTML = `
    <div class="fable-player-wizard fable-creator-chat">
      <h2 class="fable-creator-chat-title" data-title></h2>
      <div class="fable-wizard-slide-divider"></div>
      <div class="fable-creator-chat-stage">
        <div class="fable-creator-chat-prompt" data-messages>
          <div class="fable-creator-chat-prompt-body"></div>
        </div>
        <div class="fable-creator-chat-review" data-review hidden></div>
      </div>
      <div class="fable-creator-chat-composer">
        <textarea class="fable-creator-chat-input" data-input
          rows="2"></textarea>
      </div>
    </div>`;
  return root;
}

// Render the screen for a creator run. Config:
//   creatorKind: 'player' | 'sim' | 'codex' | 'intro'
//   title:       header text
//   cardId:      (codex/intro) the sim card the artifact attaches to
//   onCreated:   (result) => flow advancement; result = { playerId } | { cardId }
//   back:        () => return to the prior flow step (flow-chrome ‹)
//   introNudge:  (intro only) collector mode — show `staticPrompt`, NO API call;
//                Enter (empty or a nudge) calls `onEnter(nudge)` instead of
//                generating in-chat (launchGame generates behind the fade).
//   staticPrompt:(intro only) the fixed prompt text for the nudge collector.
//   onEnter:     (intro only) (nudge) => launchGame(cardId, nudge).
export function renderCreatorChat(root, config) {
  const { creatorKind, title, onCreated, back, cardId, introNudge, staticPrompt, onEnter } = config;
  // The DOM scaffold lives in buildCreatorChat (rebuilt 2026-08-13). The
  // guard below is defensive: if the shell is ever stripped again, bail
  // cleanly with the back hook wired so the flow-chrome ‹ still routes.
  if (!root.querySelector('[data-messages]')) {
    root._creatorBack = back;
    return;
  }
  const containerEl = root.querySelector('.fable-creator-chat');
  const messagesEl = root.querySelector('[data-messages]');
  const reviewEl = root.querySelector('[data-review]');
  const composerEl = root.querySelector('.fable-creator-chat-composer');
  // The screen element is reused across wizards (player → sim → codex → intro),
  // so strip any click/keydown listeners from the prior render before re-wiring.
  // cloneNode copies attributes/children but NOT event listeners → a clean slate.
  const inputElFresh = root.querySelector('[data-input]');
  if (inputElFresh) inputElFresh.replaceWith(inputElFresh.cloneNode(true));
  const inputEl = root.querySelector('[data-input]');

  const state = {
    history: [],        // [{role:'user'|'assistant', content}] — assistant = raw envelope text
    draft: {},          // accumulating fields
    importData: config.presetImportData || null,  // pre-seeded (IMPORT tile / codex Import / intro context)
    portraitBytes: config.presetPortraitBytes || null,        // pre-seeded portrait bytes (IMPORT tile — saved even w/o re-crop)
    portraitExt: config.presetPortraitExt || null,        // pre-seeded portrait ext (IMPORT tile)
    portraitPreview: config.presetPortraitDataUrl || null, // pre-seeded portrait preview (IMPORT tile)
    busy: false,
    done: false,
  };
  // Pre-seed the draft's `intro` from an import's captured greetings
  // (first_mes + alternate_greetings). GLM may still override it on a later
  // turn (mergeDraft overwrites with non-empty), but the mechanically-captured
  // greetings are the floor so the authored opening survives into `<intro>`.
  if (config.presetIntro) state.draft.intro = config.presetIntro;
  root._creatorBack = back;

  messagesEl.innerHTML = '';
  reviewEl.hidden = true;
  reviewEl.innerHTML = '';
  inputEl.value = '';
  inputEl.disabled = false;
  root.querySelector('[data-title]').textContent = title;
  // The intro-nudge collector mode shows a fixed static prompt (no greeting,
  // no API call) — the user types a nudge (or empty) + Enter, then launchGame
  // generates the opening beat behind the fade. Otherwise the greeting opens
  // the conversation unless an initial message is supplied or a seed draft is
  // provided (edit mode skips straight to the review card).
  if (introNudge) {
    appendBubble(messagesEl, 'assistant', staticPrompt || '');
  } else if (!config.initialMessage && !config.seedDraft) {
    appendBubble(messagesEl, 'assistant', GREETINGS[creatorKind] || GREETINGS.player);
  }
  // Pre-seeded import (the IMPORT tile on the Player pair screen, or the
  // codex Import step): surface a confirmation bubble naming the imported
  // character so the user sees the import loaded + knows GLM will work from
  // it. Replaces the old in-screen Import button's post-pick bubble.
  if (config.presetImportData) {
    const nm = config.presetImportData.name || 'the imported file';
    appendBubble(
      messagesEl,
      'assistant',
      `Imported "${nm}". I will work from it — tell me anything you want changed, or send a concept to begin.`
    );
  }
  // Edit mode: a pre-seeded draft (e.g. editing a saved player) loads straight
  // into the review card — CREATE to save the edits, Edit to modify via chat.
  if (config.seedDraft) {
    Object.assign(state.draft, config.seedDraft);
    showReview();
  }

  const setBusy = (b) => {
    state.busy = b;
    inputEl.disabled = b;
    // Dim the whole composer while busy (CSS handles opacity + cursor). The
    // textarea is also disabled above — belt-and-braces.
    if (composerEl) composerEl.classList.toggle('is-busy', b);
    // Auto-refocus the instant the textarea unlocks (zero manual re-clicking
    // after every AI reply). Skipped when the review card is showing (the
    // textarea stays disabled there anyway) or in the intro-nudge collector
    // (launchGame fades the shell on Enter, so a refocus is moot).
    if (!b && !state.done && !introNudge) inputEl.focus();
  };

  // Tee a trace line to BOTH the devtools console + the creator playtest log
  // (Rust creator_log IPC → <temp>/wupi-creator.log). Best-effort.
  const trace = (msg) => {
    console.log('[creator]', msg);
    invoke('creator_log', { line: msg }).catch(() => {});
  };

  // --- send a user turn --------------------------------------------------
  async function send() {
    if (state.busy || state.done) return;
    const text = inputEl.value.trim();
    // The intro-nudge collector: empty Enter is valid (launchGame decides what
    // to do with an empty nudge). Hand the nudge to the caller + bail — the
    // opening beat is generated behind the fade in launchGame, NOT in-chat.
    if (introNudge) {
      inputEl.value = '';
      if (onEnter) onEnter(text);
      return;
    }
    if (!text) return;
    inputEl.value = '';
    state.history.push({ role: 'user', content: text });
    // No user-side bubble: the minimal two-block UI surfaces only the AI
    // prompt + the textarea. The user's turn is carried in state.history.
    // A fresh user send resets the codex validation-retry counter (Gate 1).
    state.codexValidationRetries = 0;
    trace(`user (${creatorKind}): ${text.slice(0, 140)}`);
    await callApi();
  }

  // Abort the in-flight turn (Escape). Signals the reserved creator cancel
  // token; the backend emits `cancelled` + the partial bubble is dropped.
  async function stopTurn() {
    if (!state.busy) return;
    try { await invoke('creator_assistant_stop'); } catch (_) {}
  }

  // Enter sends (Shift+Enter = newline). Escape cancels an in-flight turn —
  // the SEND/STOP button is gone (the wizards are Enter-only by design), so
  // Escape is the sole abort path.
  inputEl.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    } else if (e.key === 'Escape' && state.busy) {
      e.preventDefault();
      stopTurn();
    }
  });

  // --- the API round-trip ------------------------------------------------
  // Read the current prompt body text (for cancel-restore). Returns '' if the
  // body is missing or untouched.
  function promptText() {
    const body = messagesEl.querySelector('.fable-creator-chat-prompt-body');
    return body ? body.textContent : '';
  }

  async function callApi() {
    setBusy(true);
    // Capture the prior prompt text so a mid-stream cancel can restore it
    // (the single-block model has no partial bubble to drop — the block IS
    // the prompt surface, so we revert to what was there before this turn).
    const priorText = promptText();
    const bubble = appendBubble(messagesEl, 'assistant', '');
    bubble.setTyping(true);
    let acc = '';
    const channel = new Channel();
    channel.onmessage = (msg) => {
      if (msg.type === 'chunk') {
        // Atomic reveal: accumulate chunks but do NOT touch the DOM. The
        // typing indicator stays up until `done`, then the new text fades in.
        acc += msg.text;
      } else if (msg.type === 'done') {
        handleDone(msg.text, bubble);
      } else if (msg.type === 'cancelled') {
        // Mid-stream abort (Stop): restore the prior prompt + re-enable.
        bubble.setTyping(false);
        bubble.restore(priorText);
        setBusy(false);
        trace('cancelled (stop) — prompt restored');
      } else if (msg.type === 'api_lost') {
        bubble.setTyping(false);
        bubble.update(`⚠ ${msg.message || 'The API connection was lost.'}`);
        setBusy(false);
        trace(`api_lost: ${msg.message || ''}`);
      } else if (msg.type === 'validation_error') {
        // Codex Gate 1: Rust rejected a `ready` because an entry body exceeded
        // the 1400-char bge-small embed cap. Push GLM's reply + the corrective
        // alert into history (so GLM sees its prior draft + the split
        // instruction), surface a brief notice, then auto-retry (capped so a
        // stuck model can't loop). The review card never shows for a rejected
        // ready — only a clean `done` reaches it.
        state.history.push({ role: 'assistant', content: msg.text });
        state.history.push({ role: 'user', content: msg.alert });
        state.codexValidationRetries = (state.codexValidationRetries || 0) + 1;
        const MAX_RETRIES = 2;
        const n = (msg.offenders && msg.offenders.length) || 0;
        trace(`validation_error: ${n} oversize codex entry/entries; retry ${state.codexValidationRetries}/${MAX_RETRIES}`);
        bubble.setTyping(false);
        bubble.update('⚠ A codex entry exceeded the 1400-character embedding cap — asking the assistant to split it…');
        if (state.codexValidationRetries <= MAX_RETRIES) {
          // Brief pause so the notice is readable before the retry overwrites it.
          setTimeout(() => callApi(), 900);
        } else {
          setBusy(false);
          trace('codex validation retries exhausted — user intervention needed');
        }
      }
    };
    try {
      await invoke('creator_assistant_turn', {
        creatorKind,
        history: state.history.map((h) => ({ role: h.role, content: h.content })),
        importData: state.importData || null,
        onEvent: channel,
      });
    } catch (e) {
      bubble.setTyping(false);
      bubble.update(`⚠ ${e.message || e}`);
      setBusy(false);
    }
  }

  function handleDone(text, bubble) {
    state.history.push({ role: 'assistant', content: text });
    const env = parseEnvelope(text);
    if (!env) {
      // Could not parse an envelope — surface the raw text, stay in chat.
      trace('envelope UNPARSEABLE — showing raw reply');
      bubble.setTyping(false);
      bubble.update(stripToJsonFallback(text));
      setBusy(false);
      return;
    }
    if (env.draft && typeof env.draft === 'object') mergeDraft(state.draft, env.draft);
    trace(`envelope action=${env.action || '(none)'} draftKeys=[${Object.keys(env.draft || {}).join(',')}]`);
    if (env.action === 'ready') {
      bubble.setTyping(false);
      bubble.update(env.message || 'Here is what I have — review it below.');
      showReview();
    } else {
      // ask (or unknown → treat as ask)
      const qText = Array.isArray(env.questions) && env.questions.length
        ? '\n\n' + env.questions.map((q) => `• ${q}`).join('\n')
        : '';
      bubble.setTyping(false);
      bubble.update((env.message || '').trim() + qText);
    }
    setBusy(false);
  }

  // --- the review card ---------------------------------------------------
  function showReview() {
    state.done = true;
    inputEl.disabled = true;
    // Engage review-mode on the wizard container so the existing
    // .fable-player-wizard.is-review-mode CSS (bottom-anchored, card cap,
    // CREATE visible) applies wholesale. Hide the chat-only surfaces.
    if (containerEl) containerEl.classList.add('is-review-mode');
    messagesEl.hidden = true;
    if (composerEl) composerEl.hidden = true;
    const sections = buildReviewSections(creatorKind, state.draft);
    reviewEl.innerHTML = renderReviewCard(creatorKind, state.draft, state.portraitPreview, sections);
    reviewEl.hidden = false;
    wireReview(reviewEl);
    reviewEl.scrollTop = 0;
    trace(`review shown — ${sections.length} section(s), draftKeys=[${Object.keys(state.draft).join(',')}]`);
  }

  function exitReview() {
    reviewEl.hidden = true;
    reviewEl.innerHTML = '';
    state.done = false;
    if (containerEl) containerEl.classList.remove('is-review-mode');
    messagesEl.hidden = false;
    if (composerEl) composerEl.hidden = false;
    inputEl.disabled = false;
    inputEl.focus();
  }

  function wireReview(el) {
    // Portrait slot → cropper → stash cropped bytes.
    const slot = el.querySelector('[data-portrait-slot]');
    if (slot) {
      slot.addEventListener('click', async () => {
        try {
          const cropped = await openPortraitCropper(root, state.portraitPreview || '');
          if (cropped) {
            state.portraitBytes = cropped.bytes;
            state.portraitExt = cropped.ext;
            state.portraitPreview = cropped.dataUrl;
            slot.innerHTML = `<img src="${cropped.dataUrl}" alt="" onerror="this.style.display='none'">`;
          }
        } catch (_) { /* cancel — keep current */ }
      });
    }
    // Back → return to chat to request changes.
    const backBtn = el.querySelector('[data-review-back]');
    if (backBtn) backBtn.addEventListener('click', exitReview);
    // Edit → jump straight back to chat with a prompt.
    const editBtn = el.querySelector('[data-review-edit]');
    if (editBtn) {
      editBtn.addEventListener('click', () => {
        exitReview();
        appendBubble(messagesEl, 'system', 'Tell me what to change.');
      });
    }
    const createBtn = el.querySelector('[data-review-create]');
    if (createBtn) createBtn.addEventListener('click', () => doCreate(createBtn));
    // Bronze-arrow expand/collapse on the ID card (no-op for codex/intro).
    wireIdCard(el);
  }

  // --- CREATE: serialize + write via the existing IPCs -------------------
  async function doCreate(btn) {
    if (btn.disabled) return;
    btn.disabled = true;
    btn.textContent = 'Creating...';
    trace(`CREATE: kind=${creatorKind} cardId=${cardId || '-'} portrait=${!!state.portraitBytes}`);
    try {
      if (creatorKind === 'player') {
        const { id, player } = serializePlayer(state.draft);
        trace(`serializePlayer → id=${id} fields=[${Object.keys(player).join(',')}]`);
        const meta = await invoke('fable_player_write', { id, player });
        if (state.portraitBytes) {
          await invoke('fable_player_portrait_upload_bytes', {
            id: meta.id,
            bytesB64: bytesToBase64(state.portraitBytes),
          });
        }
        trace(`saved player id=${meta.id}`);
        if (onCreated) onCreated({ playerId: meta.id, draft: state.draft });
      } else if (creatorKind === 'sim') {
        const { xml, intro } = serializeSimCard(state.draft);
        const stem = slugify(state.draft.name || '') || 'world';
        trace(`serializeSimCard → stem=${stem} xml=${xml.length}b intro=${intro ? intro.length + 'b' : 'none'}`);
        // `<intro>` is now embedded AFTER </sim_card> in the XML itself
        // (2026-08-13), so fable_write_card carries it — no separate .intro
        // sibling-file write.
        const meta = await invoke('fable_write_card', { stem, xml });
        if (state.portraitBytes) {
          await invoke('fable_card_portrait_write', {
            cardId: meta.id,
            bytesB64: bytesToBase64(state.portraitBytes),
            ext: state.portraitExt || 'png',
          });
        }
        trace(`saved sim card id=${meta.id}`);
        if (onCreated) onCreated({ cardId: meta.id, draft: state.draft });
      } else if (creatorKind === 'codex') {
        const text = codexEntriesToCompound(state.draft.entries || []);
        if (cardId) await invoke('fable_card_sibling_write', { cardId, ext: 'codex', text });
        trace(`saved codex (${text.length}b) on cardId=${cardId}`);
        if (onCreated) onCreated({ cardId, draft: state.draft });
      } else if (creatorKind === 'intro') {
        const intro = (state.draft.intro || '').trim();
        // The intro step runs AFTER the card exists: inject/replace the
        // in-file `<intro>` sibling via Rust (owns the two-root XML edit).
        if (cardId && intro) {
          await invoke('fable_card_set_intro', { cardId, text: intro });
        }
        trace(`saved intro (${intro.length}b) on cardId=${cardId}`);
        if (onCreated) onCreated({ cardId, draft: state.draft });
      }
    } catch (e) {
      btn.disabled = false;
      btn.textContent = 'Create';
      trace(`CREATE FAILED (${creatorKind}): ${e.message || e}`);
      appendBubble(messagesEl, 'system', `Create failed: ${e.message || e}. Fix the concept and try again.`);
      console.error('[creator-chat] create failed', e);
    }
  }

  // Auto-send an initial message (used by the intro step's nudge → one-shot).
  if (config.initialMessage) {
    inputEl.value = config.initialMessage;
    send();
  } else if (!config.seedDraft) {
    // No edit-mode review + no auto-send: focus the textarea so the user can
    // start typing immediately (zero manual click on entry).
    inputEl.focus();
  }
}

// --- helpers --------------------------------------------------------------

// appendBubble is now a replace-or-create controller over the SINGLE AI
// prompt block (the minimal two-block UI surfaces one prompt + one textarea —
// no chat log, no bubbles). `list` is the [data-messages] / .fable-creator-
// chat-prompt element; `role` is accepted for signature stability but only
// 'assistant'/'system' paint (user turns never render). On first call the
// body is created; subsequent calls reuse the same body element. The returned
// API drives the atomic-reveal transition: update() fades old → swap → fades
// new; setTyping() toggles the processing indicator; restore() reverts to a
// prior text on cancel.
function appendBubble(list, role, text) {
  // The body is recreated if missing (renderCreatorChat clears the block on
  // re-render via messagesEl.innerHTML = ''). Once present, it persists for
  // the screen's lifetime — subsequent calls reuse it.
  let body = list.querySelector('.fable-creator-chat-prompt-body');
  if (!body) {
    body = document.createElement('div');
    body.className = 'fable-creator-chat-prompt-body';
    list.appendChild(body);
  }
  // First paint (greeting/seed): set the text at full opacity, no fade.
  if (!body.dataset.touched) {
    body.textContent = text;
    body.dataset.touched = '1';
    list.scrollTop = 0;
  } else if (text) {
    // Subsequent calls from outside a turn (import confirmation, edit prompt,
    // create-failed notice): swap text directly without the streaming fade.
    body.textContent = text;
  }
  return {
    // update(): fade the old text out (150ms), swap, fade the new text in
    // (200ms). The atomic-reveal contract: chunks do NOT call update() — only
    // the final `done`/error/ready text does.
    update(t) {
      if (!body) return;
      fadeSwap(body, t);
    },
    setTyping(on) {
      list.classList.toggle('is-typing', !!on);
    },
    // restore(): on mid-stream cancel, revert the block to the prior prompt
    // text (the typing indicator is cleared by the caller before this).
    restore(t) {
      if (!body) return;
      body.textContent = t || '';
      list.scrollTop = 0;
    },
    remove() { /* no-op: the block is the persistent prompt surface */ },
  };
}

// fadeSwap: opacity 1 → 0 (150ms out) → textContent swap → 0 → 1 (200ms in).
// Pure DOM, returns immediately; the CSS transition on .fable-creator-chat-
// prompt-body carries the fade. The swap happens at the midway opacity-0
// point so the user never sees a text "jump". The inline opacity is CLEARED
// after the fade-in so it doesn't override a later .is-typing veil (which
// sets opacity:0 via class — inline would win + break the typing indicator).
function fadeSwap(el, newText) {
  el.style.opacity = '0';
  setTimeout(() => {
    el.textContent = newText;
    el.scrollTop = 0;
    // Force a reflow so the opacity transition re-triggers from 0.
    void el.offsetHeight;
    el.style.opacity = '1';
    // Clear the inline style once the transition settles so class-based
    // opacity (the .is-typing veil) can take over on the next turn.
    setTimeout(() => { el.style.opacity = ''; }, 220);
  }, 150);
}

// Render the review card HTML. Player + sim (npc/scenario/world) render as
// the compact ID card (core face + bronze-arrow extra disclosure) via the
// shared id-card renderer; codex/intro keep the generic section grid (no
// portrait). CREATE/Edit/‹ live in the .fable-player-review-create-wrap row
// appended under whichever card shape was produced.
function renderReviewCard(kind, d, portraitPreview, sections) {
  const esc = escapeXml;
  const createWrap = `
    <div class="fable-player-review-create-wrap">
      <button type="button" class="fable-player-review-create" data-review-create>CREATE</button>
      <button type="button" class="fable-player-review-edit" data-review-edit>Edit</button>
      <button type="button" class="fable-player-review-back" data-review-back aria-label="Back">${ARROW_SVG_LEFT}</button>
    </div>`;
  // ID-card kinds (player + sim). buildIdCard returns null for codex/intro.
  const idModel = buildIdCard(kind, d);
  if (idModel) {
    return renderIdCard(idModel, { portraitClickable: true, portraitPreview }) + createWrap;
  }
  // codex/intro: generic section grid, no portrait slot.
  const sectionsHtml = sections.map(([title, rows]) => {
    const pairHtml = rows.map(([label, val]) => {
      if (Array.isArray(val)) {
        const chips = val.map((c) => `<span class="fable-wizard-chip">${esc(c)}</span>`).join('');
        return `<div><dt>${esc(label)}</dt><dd><div class="fable-player-review-chips">${chips}</div></dd></div>`;
      }
      return `<div><dt>${esc(label)}</dt><dd>${esc(val)}</dd></div>`;
    }).join('');
    return `<section class="fable-player-review-section"><h3>${esc(title)}</h3><dl>${pairHtml}</dl></section>`;
  }).join('');
  return `
    <div class="fable-player-review-card">
      <div class="fable-player-review-top">
        <div class="fable-player-review-body">${sectionsHtml}</div>
      </div>
    </div>` + createWrap;
}
