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
// portrait slot → cropper + a corner pencil → edit popup). CREATE
// serializes via card-serialize.js + writes through the existing IPCs
// (fable_player_write / fable_write_card / fable_card_sibling_write /
// fable_card_portrait_write). Mechanical integrity stays in JS/Rust —
// Prime-Mandate compliant.
//
// This is a CREATION-ONLY API role (AGENTS.md §3A, 2026-08-12 override):
// outside the runtime game loop. The IPC `creator_assistant_turn` does
// the one-shot HttpBackend call (no tracker, no schema, no world state).
//
// The INTRO wizard was REMOVED 2026-08-15 (Chloe): the SIM Wizard asks
// the mandatory intro question itself + serializeSimCard embeds the
// agreed `<intro>` sibling in-file — no post-card intro step exists.
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
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
  missingMandatoryFields,
  MANDATORY_LABELS,
} from '../engine/creator-engine.js';
import { renderIdCard, wireIdCard, PENCIL_SVG } from './id-card.js';

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
  // block + a single textarea (no chat log, no bubbles). 2026-08-14 Chloe: the
  // header/title is GONE (no "Player/World/Codex Wizard" banners). 2026-08-15
  // Chloe: the gold divider is GONE too — the AI box stands alone (nothing
  // crowns it). The renderCreatorChat backend (GLM turn loop, envelope parse
  // → draft → review → CREATE) binds to the [data-*] hooks below unchanged.
  root.innerHTML = `
    <div class="fable-player-wizard fable-creator-chat">
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
//   creatorKind: 'player' | 'sim' | 'codex'
//   cardId:      (codex) the sim card the artifact attaches to
//   onCreated:   (result) => flow advancement; result = { playerId } | { cardId }
//   back:        () => return to the prior flow step (flow-chrome ‹)
export function renderCreatorChat(root, config) {
  const { creatorKind, onCreated, back, cardId } = config;
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
  // The screen element is reused across wizards (player → sim → codex),
  // so strip any click/keydown listeners from the prior render before re-wiring.
  // cloneNode copies attributes/children but NOT event listeners → a clean slate.
  const inputElFresh = root.querySelector('[data-input]');
  if (inputElFresh) inputElFresh.replaceWith(inputElFresh.cloneNode(true));
  const inputEl = root.querySelector('[data-input]');

  const state = {
    history: [],        // [{role:'user'|'assistant', content}] — assistant = raw envelope text
    draft: {},          // accumulating fields
    importData: config.presetImportData || null,  // pre-seeded (IMPORT tile / codex Import)
    portraitBytes: config.presetPortraitBytes || null,        // pre-seeded portrait bytes (IMPORT tile — saved even w/o re-crop)
    portraitExt: config.presetPortraitExt || null,        // pre-seeded portrait ext (IMPORT tile)
    portraitPreview: config.presetPortraitDataUrl || null, // pre-seeded portrait preview (IMPORT tile)
    busy: false,
    done: false,
  };
  // (P1 fix) Stale-turn firewall: ‹/⌂ stay clickable during a GLM turn, so
  // exiting mid-generation left the turn running — its `done` handler then
  // corrupted the NEXT wizard run on this shared screen (hid the prompt
  // block, popped the OLD draft's review card). Abort the in-flight turn +
  // stamp an epoch every render; callApi's channel ignores events from any
  // prior epoch.
  root._creatorEpoch = (root._creatorEpoch || 0) + 1;
  const epoch = root._creatorEpoch;
  invoke('creator_assistant_stop').catch(() => {});
  // Pre-seed the draft's `intro` from an import's captured greetings
  // (first_mes + alternate_greetings). GLM may still override it on a later
  // turn (mergeDraft overwrites with non-empty), but the mechanically-captured
  // greetings are the floor so the authored opening survives into `<intro>`.
  if (config.presetIntro) state.draft.intro = config.presetIntro;
  root._creatorBack = back;

  // Reset residue from a prior wizard run on this SHARED screen. A completed
  // review (CREATE) never returns to chat, so the container keeps
  // .is-review-mode (bottom-anchored flex-end) + the prompt/composer stay
  // [hidden] — the next wizard (sim → codex chain) then rendered with
  // only its title pinned to the bottom of the screen (the World Wizard
  // glitch). Also strip launchGame's .is-launching fade if the prior run
  // launched from this screen (ADD INTRO → launchGame), + any stuck is-typing
  // veil from a mid-generation ‹ exit (the class sits on [data-messages]
  // itself, so innerHTML='' doesn't clear it). Every render starts from the
  // same clean state the first Player Wizard render enjoyed.
  if (containerEl) containerEl.classList.remove('is-review-mode');
  root.classList.remove('is-launching');
  messagesEl.hidden = false;
  messagesEl.classList.remove('is-typing');
  if (composerEl) composerEl.hidden = false;
  // Strip any edit-popup / edit-generation overlays a prior run left on this
  // shared screen (a ‹ exit mid-edit can leave the ring spinning invisibly).
  root.querySelectorAll('[data-genring], [data-edit-overlay]').forEach((el) => {
    if (el._cleanup) el._cleanup();
    el.remove();
  });

  messagesEl.innerHTML = '';
  reviewEl.hidden = true;
  reviewEl.innerHTML = '';
  inputEl.value = '';
  inputEl.disabled = false;
  // The greeting opens the conversation unless an initial message is supplied
  // or a seed draft is provided (edit mode skips straight to the review card).
  if (!config.initialMessage && !config.seedDraft) {
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
    // textarea stays disabled there anyway).
    if (!b && !state.done) inputEl.focus();
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
    if (!text) return;
    inputEl.value = '';
    state.history.push({ role: 'user', content: text });
    // No user-side bubble: the minimal two-block UI surfaces only the AI
    // prompt + the textarea. The user's turn is carried in state.history.
    // A fresh user send resets both validation-retry counters (codex Gate 1 +
    // the mandatory-field gate).
    state.codexValidationRetries = 0;
    state.mandatoryRetries = 0;
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

  // The shared turn engine. `opts.editMode` runs the turn FROM the review
  // card (the corner-pencil popup): instead of the prompt block's typing
  // indicator, the whole screen blurs + the centered bronze ring spins
  // (.fable-creator-genring overlay) until the turn settles — then the
  // review re-renders (`ready`) or the chat resumes with GLM's follow-up
  // questions (`ask`). Escape mid-generation cancels (creator_assistant_stop).
  async function callApi(opts = {}) {
    const editMode = !!opts.editMode;
    setBusy(true);
    // Capture the prior prompt text so a mid-stream cancel can restore it
    // (the single-block model has no partial bubble to drop — the block IS
    // the prompt surface, so we revert to what was there before this turn).
    const priorText = promptText();
    const bubble = editMode ? null : appendBubble(messagesEl, 'assistant', '');
    if (bubble) bubble.setTyping(true);
    if (editMode) beginEditGen();
    let acc = '';
    const channel = new Channel();
    channel.onmessage = (msg) => {
      // Stale-epoch turn (this render was replaced mid-generation) — ignore.
      if (epoch !== root._creatorEpoch) return;
      if (msg.type === 'chunk') {
        // Atomic reveal: accumulate chunks but do NOT touch the DOM. The
        // typing indicator stays up until `done`, then the new text fades in.
        acc += msg.text;
      } else if (msg.type === 'done') {
        handleDone(msg.text, bubble, editMode);
      } else if (msg.type === 'cancelled') {
        // Mid-stream abort (Escape/Stop): restore the prior prompt + re-enable.
        if (bubble) { bubble.setTyping(false); bubble.restore(priorText); }
        if (editMode) endEditGen();   // un-blur — the review card returns untouched
        setBusy(false);
        trace('cancelled (stop) — turn aborted');
      } else if (msg.type === 'api_lost') {
        if (bubble) { bubble.setTyping(false); bubble.update(`⚠ ${msg.message || 'The API connection was lost.'}`); }
        if (editMode) { endEditGen(); showReviewError(`⚠ ${msg.message || 'The API connection was lost.'}`); }
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
        if (bubble) {
          bubble.setTyping(false);
          bubble.update('⚠ A codex entry exceeded the 1400-character embedding cap — asking the assistant to split it…');
        }
        if (state.codexValidationRetries <= MAX_RETRIES) {
          // Brief pause so the notice is readable before the retry overwrites it.
          setTimeout(() => callApi(opts), 900);
        } else {
          if (editMode) { endEditGen(); showReviewError('⚠ The assistant could not fit a codex entry under the 1400-character cap — edit again or describe the split yourself.'); }
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
      if (bubble) { bubble.setTyping(false); bubble.update(`⚠ ${e.message || e}`); }
      if (editMode) { endEditGen(); showReviewError(`⚠ ${e.message || e}`); }
      setBusy(false);
    }
  }

  // --- the mandatory-field gate (2026-08-15 Chloe) ------------------------
  // A `ready` draft missing ANY mandatory field must NEVER reach the review
  // screen. The gate runs on the MERGED draft (fields accumulate across turns
  // + seeds, so only the frontend can judge completeness), pushes GLM's reply
  // + a corrective alert into history, and auto-retries — the same loop shape
  // as the codex validation_error path. Capped so a stuck model surfaces the
  // gap to the user instead of looping.
  function rejectReady(missing, bubble, editMode) {
    const MAX_RETRIES = 2;
    const alert = `SYSTEM ALERT: your ready draft is missing mandatory fields: ${missing.join(', ')}. ` +
      'Do not emit ready until every mandatory field is filled. Fill the missing fields now ' +
      'from the conversation — ask the user only for what you cannot infer.';
    state.history.push({ role: 'user', content: alert });
    state.mandatoryRetries = (state.mandatoryRetries || 0) + 1;
    const labels = missing.map((k) => MANDATORY_LABELS[k] || k).join(', ');
    trace(`ready REJECTED — missing mandatory [${missing.join(', ')}]; retry ${state.mandatoryRetries}/${MAX_RETRIES}`);
    if (state.mandatoryRetries <= MAX_RETRIES) {
      if (bubble) {
        bubble.setTyping(false);
        bubble.update('⚠ The draft was missing mandatory fields — asking the assistant to fill them…');
      }
      // Brief pause so the notice is readable before the retry overwrites it
      // (the ring persists across the pause in edit mode — beginEditGen
      // dedupes on the retried callApi).
      setTimeout(() => callApi({ editMode }), 900);
    } else {
      const msg = `⚠ The assistant could not fill the mandatory fields: ${labels}. Tell it the missing details and it will finalize.`;
      if (editMode) { endEditGen(); showReviewError(msg); }
      else if (bubble) { bubble.setTyping(false); bubble.update(msg); }
      setBusy(false);
      state.mandatoryRetries = 0;
      trace('mandatory retries exhausted — user intervention needed');
    }
  }

  function handleDone(text, bubble, editMode = false) {
    state.history.push({ role: 'assistant', content: text });
    const env = parseEnvelope(text);
    if (!env) {
      // Could not parse an envelope — surface the raw text, stay in chat.
      trace('envelope UNPARSEABLE — showing raw reply');
      if (editMode) {
        endEditGen();
        exitReviewToChat(stripToJsonFallback(text));
        setBusy(false);
      } else {
        bubble.setTyping(false);
        bubble.update(stripToJsonFallback(text));
        setBusy(false);
      }
      return;
    }
    if (env.draft && typeof env.draft === 'object') mergeDraft(state.draft, env.draft);
    trace(`envelope action=${env.action || '(none)'} draftKeys=[${Object.keys(env.draft || {}).join(',')}]`);
    if (env.action === 'ready') {
      // The gate: no incomplete draft ever shows the review card. Runs on the
      // merged draft (accumulated across turns + seed/import presets).
      const missing = missingMandatoryFields(creatorKind, state.draft);
      if (missing.length) {
        rejectReady(missing, bubble, editMode);
        return;
      }
      if (editMode) {
        endEditGen();
        showReview();          // fresh review card from the merged draft
        setBusy(false);
      } else {
        bubble.setTyping(false);
        bubble.update(env.message || 'Here is what I have — review it below.');
        showReview();
        setBusy(false);
      }
    } else {
      // ask (or unknown → treat as ask)
      const qText = Array.isArray(env.questions) && env.questions.length
        ? '\n\n' + env.questions.map((q) => `• ${q}`).join('\n')
        : '';
      const askText = (env.message || '').trim() + qText;
      if (editMode) {
        // GLM needs more info before the card can re-finalize → drop back to
        // the chat surface with the follow-up questions in the prompt block.
        endEditGen();
        exitReviewToChat(askText.trim() || 'Tell me what to change.');
        setBusy(false);
      } else {
        bubble.setTyping(false);
        bubble.update(askText);
        setBusy(false);
      }
    }
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

  // Leave the review card + return to the chat surface. `text` (optional)
  // replaces the prompt block's content — used by the edit path when GLM
  // answers with follow-up questions instead of a fresh `ready`.
  function exitReviewToChat(text) {
    reviewEl.hidden = true;
    reviewEl.innerHTML = '';
    state.done = false;
    if (containerEl) containerEl.classList.remove('is-review-mode');
    messagesEl.hidden = false;
    if (composerEl) composerEl.hidden = false;
    inputEl.disabled = false;
    if (text != null) appendBubble(messagesEl, 'assistant', text);
    inputEl.focus();
  }

  // Surface an error INSIDE the review card (the prompt block is hidden in
  // review mode — the old appendBubble-to-hidden-messagesEl path made CREATE
  // failures invisible, the "CREATE does nothing" report's root surface).
  function showReviewError(msg) {
    if (!reviewEl || reviewEl.hidden) return;
    reviewEl.querySelectorAll('.fable-creator-review-error').forEach((n) => n.remove());
    const note = document.createElement('p');
    note.className = 'fable-creator-review-error';
    note.textContent = msg;
    reviewEl.appendChild(note);
  }

  // --- the edit popup (corner pencil → centered mini-editor) --------------
  // NO send button by design (Chloe 2026-08-15): Enter sends, Escape/✕ close.
  // On Enter the popup disappears + the screen blurs under the same bronze
  // loading ring the chat uses while GLM reworks the draft; Escape during
  // that generation cuts it off (creator_assistant_stop → review restored).
  function openEditPopup() {
    if (state.busy) return;
    // One popup at a time (a leftover can only exist if a close animation
    // was interrupted — a plain replace is enough).
    root.querySelectorAll('[data-edit-overlay]').forEach((el) => el.remove());
    const overlay = document.createElement('div');
    overlay.className = 'fable-creator-edit-overlay';
    overlay.dataset.editOverlay = '';
    overlay.hidden = true;
    overlay.innerHTML = `
      <div class="fable-creator-edit-backdrop"></div>
      <div class="fable-creator-edit-modal" role="dialog" aria-modal="true" aria-label="Edit card">
        <div class="fable-creator-edit-head">
          <span class="fable-creator-edit-title">Edit</span>
          <button type="button" class="fable-creator-edit-close" data-edit-close title="Close" aria-label="Close editor">✕</button>
        </div>
        <textarea class="fable-creator-edit-input" data-edit-input rows="4"
          placeholder="Tell me what to change…"></textarea>
        <p class="fable-creator-edit-hint">ENTER to send · ESC to close</p>
      </div>`;
    root.appendChild(overlay);
    const input = overlay.querySelector('[data-edit-input]');
    let closed = false;
    const close = () => {
      if (closed) return;
      closed = true;
      document.removeEventListener('keydown', onKey, { capture: true });
      overlay.classList.remove('is-open');
      const finish = () => overlay.remove();
      overlay.addEventListener('transitionend', finish, { once: true });
      setTimeout(finish, 240);
    };
    overlay.querySelector('[data-edit-close]').addEventListener('click', close);
    overlay.addEventListener('click', (e) => {
      if (e.target === overlay || e.target.classList.contains('fable-creator-edit-backdrop')) close();
    });
    // Document-level (capture) so the keys fire wherever focus sits — the
    // textarea may lose focus to a click inside the modal. ENTER sends (no
    // send button by design); ESC closes; empty ENTER is a no-op.
    const onKey = (e) => {
      if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); close(); }
      else if (e.key === 'Enter' && !e.shiftKey && e.target === input) {
        e.preventDefault();
        const text = input.value.trim();
        if (!text) return;          // empty Enter is a no-op — Esc/✕ to leave
        close();
        requestEdit(text);
      }
    };
    document.addEventListener('keydown', onKey, { capture: true });
    overlay.hidden = false;
    void overlay.offsetWidth;
    overlay.classList.add('is-open');
    input.focus();
  }

  // Fire an edit turn from the review card: user text into history, then the
  // shared turn engine in edit mode (blur + bronze ring until it settles).
  function requestEdit(text) {
    state.history.push({ role: 'user', content: text });
    state.codexValidationRetries = 0;
    state.mandatoryRetries = 0;
    trace(`edit (${creatorKind}): ${text.slice(0, 140)}`);
    callApi({ editMode: true });
  }

  // --- the edit-generation overlay (full blur + the bronze ring) ---------
  function beginEditGen() {
    endEditGen();               // never stack two overlays
    const overlay = document.createElement('div');
    overlay.className = 'fable-creator-genring';
    overlay.dataset.genring = '';
    overlay.innerHTML = `<div class="fable-creator-genring-ring"></div>`;
    root.appendChild(overlay);
    // Escape is the cut-off path while the ring spins (the composer is hidden
    // in review mode, so its own Escape handler can't fire here).
    const onEsc = (e) => {
      if (e.key === 'Escape') { e.preventDefault(); stopTurn(); }
    };
    document.addEventListener('keydown', onEsc, { capture: true });
    overlay._cleanup = () => document.removeEventListener('keydown', onEsc, { capture: true });
  }

  function endEditGen() {
    root.querySelectorAll('[data-genring]').forEach((el) => {
      if (el._cleanup) el._cleanup();
      el.remove();
    });
  }

  function wireReview(el) {
    // Portrait slot → pick → crop → stash cropped bytes.
    // Pick-first is load-bearing: on a fresh card there is NO preview to crop,
    // and the old code handed the cropper '' — its load probe errored + the
    // modal self-closed within the 200ms fade, so clicking the placeholder
    // looked like a dead no-op. Now: OS file dialog (png/jpg/jpeg) →
    // server-side read (magic-byte-validated, same-origin data URL so the
    // cropper's canvas never taints) → crop → stash.
    const slot = el.querySelector('[data-portrait-slot]');
    if (slot) {
      slot.addEventListener('click', async () => {
        let picked;
        try {
          picked = await openDialog({
            multiple: false,
            filters: [{ name: 'Image', extensions: ['png', 'jpg', 'jpeg'] }],
          });
        } catch (_) {
          return; // dialog failure — keep current
        }
        if (!picked) return; // picker cancelled
        const srcPath = typeof picked === 'string' ? picked : (picked.path || picked);
        if (!srcPath) return;
        let dataUrl;
        try {
          dataUrl = await invoke('fable_player_portrait_read_bytes', { srcPath });
        } catch (e) {
          console.error('[creator-chat] portrait read failed:', e);
          return;
        }
        try {
          const cropped = await openPortraitCropper(root, dataUrl);
          if (cropped) {
            state.portraitBytes = cropped.bytes;
            state.portraitExt = cropped.ext;
            state.portraitPreview = cropped.dataUrl;
            slot.innerHTML = `<img src="${cropped.dataUrl}" alt="" onerror="this.style.display='none'">`;
          }
        } catch (_) { /* cropper failure — keep current portrait */ }
      });
    }
    // Back → return to chat to request changes.
    const backBtn = el.querySelector('[data-review-back]');
    if (backBtn) backBtn.addEventListener('click', () => exitReviewToChat());
    // The corner pencil → the edit popup (replaces the old review "Edit"
    // button, 2026-08-15 Chloe — popup + Enter + blur/ring generation).
    const pencilBtn = el.querySelector('[data-review-pencil]');
    if (pencilBtn) pencilBtn.addEventListener('click', openEditPopup);
    const createBtn = el.querySelector('[data-review-create]');
    if (createBtn) createBtn.addEventListener('click', () => doCreate(createBtn));
    // Card-icon details popup on the ID card (no-op for codex).
    wireIdCard(el);
  }

  // --- CREATE: serialize + write via the existing IPCs -------------------
  async function doCreate(btn) {
    if (btn.disabled) return;
    btn.disabled = true;
    btn.textContent = 'Creating...';
    trace(`CREATE: kind=${creatorKind} cardId=${cardId || '-'} portrait=${!!state.portraitBytes}`);
    try {
      // Final backstop: the ready gate should make this unreachable, but a
      // seeded/edited draft could still sneak a gap through — never WRITE an
      // incomplete card. Surfaces on the review card via the catch below.
      // (P2 fix) SKIPPED for edit runs (seedDraft): a legitimately-saved
      // legacy player missing optional fields could otherwise never re-save
      // ("Create failed … fix the concept" with no field editor).
      const missing = config.seedDraft ? [] : missingMandatoryFields(creatorKind, state.draft);
      if (missing.length) {
        const labels = missing.map((k) => MANDATORY_LABELS[k] || k).join(', ');
        throw new Error(`the draft is missing mandatory fields: ${labels}`);
      }
      // (P1 fix) Duplicate-name guard: both write IPCs are silent atomic
      // OVERWRITES — a CREATE reusing an existing slug replaced the prior
      // card/player (authored content lost, its saves orphaned). Edit runs
      // (seedDraft present) re-save the same entity and are exempt.
      if (!config.seedDraft && (creatorKind === 'player' || creatorKind === 'sim')) {
        const target = creatorKind === 'player'
          ? serializePlayer(state.draft).id
          : (slugify(state.draft.name || '') || 'world');
        const existing = creatorKind === 'player'
          ? (await invoke('fable_players_list').catch(() => []))
          : (await invoke('fable_cards_list').catch(() => []));
        if (existing.some((m) => m.id === target)) {
          throw new Error(`a ${creatorKind === 'player' ? 'player' : 'world'} named "${state.draft.name || target}" already exists — choose a different name`);
        }
      }
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
        // `<intro>` is embedded AFTER </sim_card> in the XML itself
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
      }
    } catch (e) {
      btn.disabled = false;
      btn.textContent = 'Create';
      trace(`CREATE FAILED (${creatorKind}): ${e.message || e}`);
      // The prompt block is HIDDEN in review mode — surface the failure ON the
      // review card so a rejected write is never an invisible "does nothing".
      showReviewError(`Create failed: ${e.message || e}. Fix the concept and try again.`);
      console.error('[creator-chat] create failed', e);
    }
  }

  // Auto-send an initial message (a caller-seeded opening turn, if ever
  // needed — currently unused by the three wizards).
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
// the compact ID card (header + license grid + card-icon extra disclosure) via
// the shared id-card renderer; codex keeps the generic section grid (no
// portrait). CREATE/‹ live in the .fable-player-review-create-wrap row
// appended under whichever card shape was produced. The edit affordance is
// the CORNER PENCIL in the headrow (2026-08-15 Chloe): beside the card-icon
// details button on ID cards, alone in the headrow's corner on the codex
// grid — in-flow, so it can never overlap content.
function renderReviewCard(kind, d, portraitPreview, sections) {
  const esc = escapeXml;
  const createWrap = `
    <div class="fable-player-review-create-wrap">
      <button type="button" class="fable-player-review-create" data-review-create>CREATE</button>
      <button type="button" class="fable-player-review-back" data-review-back aria-label="Back">${ARROW_SVG_LEFT}</button>
    </div>`;
  // ID-card kinds (player + sim). buildIdCard returns null for codex.
  const idModel = buildIdCard(kind, d);
  if (idModel) {
    return renderIdCard(idModel, { portraitClickable: true, portraitPreview, editable: true }) + createWrap;
  }
  // codex: generic section grid, no portrait slot. The pencil rides the same
  // headrow corner the ID cards use (in-flow top-right, no overlap).
  const pencil = `
      <button type="button" class="fable-id-card-edit" data-review-pencil title="Edit" aria-label="Edit card">${PENCIL_SVG}</button>`;
  const headrow = `
    <div class="fable-id-card-headrow">
      <span class="fable-id-card-head-spacer" style="width:46px" aria-hidden="true"></span>
      <div class="fable-id-card-headwrap"></div>
      <div class="fable-id-card-corner">${pencil}</div>
    </div>`;
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
    <div class="fable-player-review-card fable-codex-review-card">
      <div class="fable-player-review-top">
        <div class="fable-player-review-body">${headrow}${sectionsHtml}</div>
      </div>
    </div>` + createWrap;
}
