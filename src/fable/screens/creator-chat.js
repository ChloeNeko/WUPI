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
//
// 2026-08-18 Chloe: IMPORTS are ONE-SHOT conversions — no interview chat.
// An import seeds a single conversion turn (the bronze ring + blur, same
// overlay as the pencil edit), and its `ready` lands straight on the FINAL
// review card; the corner pencil is the change path. The chat surface only
// appears as a fallback (GLM asks anyway, a failure, or ‹ from review).
// =============================================================

import { invoke, Channel, convertFileSrc } from '@tauri-apps/api/core';
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
  shouldRejectDuplicateName,
  creatorRetryAllowed,
  MAX_CREATOR_RETRIES,
  MANDATORY_LABELS,
} from '../engine/creator-engine.js';
import { renderIdCard, wireIdCard, PENCIL_SVG } from './id-card.js';

const GREETINGS = {
  player: "Describe your character in detail and I'll help you design your PLAYER Card. Be as vague or descriptive as you'd like and I'll help guide you.",
  sim: "Start by telling me whether your SIM Card is a character, a scenario, or perhaps a whole new world? Describe your SIM Card in detail, you may be as descriptive or vague as you'd like and I'll help guide you.",
  codex: "The CODEX is the facts of the simulation which is unique to your SIM Card only. This information can be accessed at any time by your narrator. You may start by giving me a detailed list or vague ideas and I'll help you craft the lore.",
};

// (2026-08-18 Chloe) Imports are ONE-SHOT conversions, not interviews. The
// single user turn below replaces the gathering conversation: GLM must map
// the <import> block onto the WUPI schema and emit ready in that same turn —
// the result lands straight on the review card, where the corner pencil is
// the change path. Chat exists only as the fallback surface (ask / failure).
const IMPORT_CONVERT_INSTRUCTIONS = {
  player: 'Convert the imported character into a final PLAYER Card draft NOW — a one-shot conversion, not an interview. Map every content-bearing field from the import onto the schema (identity, appearance, clothing, inventory, persona, backstory), derive any missing mandatory field sensibly from the import itself, and emit ready in this single turn. Do not ask the user questions — they review and edit the card afterward.',
  sim: 'Convert the imported card into a final SIM Card draft NOW — a one-shot conversion, not an interview. Choose the card_type the import actually is (npc / scenario / world; an imported CHARACTER is an npc card — identity traits + the full persona set + inventory), map every content-bearing field onto that branch, derive the universal anchors (date, time, weather, tone, location) from the import\'s setting, and carry its first_mes / alternate_greetings straight into draft.intro as the agreed intro. Emit ready in this single turn. Do not ask the user questions — they review and edit the card afterward.',
  codex: 'Convert the imported lorebook into final codex entries NOW — one concept per entry, each body under 1400 characters (split longer concepts into "— Part 1/Part 2" entries so nothing is truncated). Emit ready in this single turn. Do not ask the user questions — they review and edit the result afterward.',
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
  // (2026-08-18 Chloe) Import runs skip the interview entirely: the bronze
  // ring spins over the blurred screen (same overlay as the review-card
  // pencil edit) while GLM converts the import in ONE turn, then the FINAL
  // review card lands — the corner pencil is the change path. The chat
  // surface survives only as the fallback (GLM asks anyway / a failure / the
  // review ‹ back).
  const importConvert = !!config.presetImportData && !config.seedDraft;
  // (P1 fix) Stale-turn firewall: ‹/⌂ stay clickable during a GLM turn, so
  // exiting mid-generation left the turn running — its `done` handler then
  // corrupted the NEXT wizard run on this shared screen (hid the prompt
  // block, popped the OLD draft's review card). Abort the in-flight turn +
  // stamp an epoch every render; callApi's channel ignores events from any
  // prior epoch.
  root._creatorEpoch = (root._creatorEpoch || 0) + 1;
  const epoch = root._creatorEpoch;
  invoke('creator_assistant_stop').catch(() => {});
  // (P1 fix) Kill any retry PAUSE a prior run left armed: the setTimeout
  // callbacks below bail on the epoch, but a dangling timer holds no turn
  // — clearing it here keeps the slot semantics simple (a new render owns
  // the screen outright).
  if (root._creatorRetryTimer) {
    clearTimeout(root._creatorRetryTimer);
    root._creatorRetryTimer = null;
  }
  // Pre-seed the draft's `intro` from an import's captured greetings
  // (first_mes + alternate_greetings). GLM may still override it on a later
  // turn (mergeDraft overwrites with non-empty), but the mechanically-captured
  // greetings are the floor so the authored opening survives into `<intro>`.
  if (config.presetIntro) state.draft.intro = config.presetIntro;
  // (2026-08-15 audit fix) ‹ mid-edit leak wrap: the edit popup + the edit
  // generation ring attach DOCUMENT-level capture keydowns; exiting the
  // screen with ‹ while either was open left them attached forever
  // (Enter/Escape firing into dead overlays on other screens). Every ‹ now
  // closes the open popup + strips the genring before routing back.
  root._creatorBack = () => {
    if (root._creatorCloseEditPopup) {
      try { root._creatorCloseEditPopup(); } catch (_) {}
      root._creatorCloseEditPopup = null;
    }
    endEditGen();
    if (typeof back === 'function') back();
  };

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
  // (2026-08-15 audit fix) _cleanup now exists on BOTH overlay shapes — the
  // edit popup's document keydown releases here too (a creator re-render is
  // the other teardown path besides ‹).
  root.querySelectorAll('[data-genring], [data-edit-overlay]').forEach((el) => {
    if (el._cleanup) el._cleanup();
    el.remove();
  });
  root._creatorCloseEditPopup = null;

  messagesEl.innerHTML = '';
  reviewEl.hidden = true;
  reviewEl.innerHTML = '';
  inputEl.value = '';
  inputEl.disabled = false;
  // The greeting opens the conversation unless an initial message is supplied,
  // a seed draft is provided (edit mode skips straight to the review card), or
  // this is an import run (no chat at all — the one-shot conversion fires at
  // the render tail below).
  if (!importConvert && !config.initialMessage && !config.seedDraft) {
    appendBubble(messagesEl, 'assistant', GREETINGS[creatorKind] || GREETINGS.player);
  }
  // Edit mode: a pre-seeded draft (e.g. editing a saved player) loads straight
  // into the review card — CREATE to save the edits, Edit to modify via chat.
  if (config.seedDraft) {
    Object.assign(state.draft, config.seedDraft);
    showReview();
    // (2026-08-15 audit fix) Edit-run portrait preview: flowEditPlayer passes
    // no presetPortraitDataUrl, so the review card opened with an empty
    // portrait slot even when the saved player HAS one. Load it via the same
    // fable_player_get the picker uses (its `portrait` is a raw absolute path
    // — must go through convertFileSrc) + re-render the still-open review.
    // fable.js is deliberately untouched (another agent's file).
    if (creatorKind === 'player' && !state.portraitPreview && config.seedDraft.id) {
      invoke('fable_player_get', { id: config.seedDraft.id })
        .then((full) => {
          if (root._creatorEpoch !== epoch || !full || !full.portrait) return;
          state.portraitPreview = convertFileSrc(full.portrait);
          if (!reviewEl.hidden) showReview();
        })
        .catch(() => { /* no portrait / IPC failure — empty slot is fine */ });
    }
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
  // A pending VALIDATION/MANDATORY RETRY is "in flight" too (busy stays
  // held across the 900ms pause) — Escape during the pause previously fired
  // at no active stream while the timer went on to start a fresh turn
  // anyway. Cancel the timer + unlock instead.
  async function stopTurn() {
    if (!state.busy) return;
    if (root._creatorRetryTimer) {
      clearTimeout(root._creatorRetryTimer);
      root._creatorRetryTimer = null;
      // (2026-08-18) endEditGen on this path too — Escape during a retry
      // pause left the bronze ring stuck over the screen with no turn behind
      // it (the timer was the only thing that would have re-fired callApi).
      endEditGen();
      if (importConvert) {
        exitReviewToChat('Import conversion cancelled — describe what you want here, or press ‹ to go back.');
      }
      setBusy(false);
      trace('retry pause cancelled (stop) — turn aborted');
      return;
    }
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
    // Stale-turn firewall (defensive entry check): the scheduled-retry
    // callbacks below bail at schedule time, but any future caller that
    // races a render must never start a GLM turn on a replaced screen.
    if (epoch !== root._creatorEpoch) return;
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
        if (importConvert) {
          exitReviewToChat('Import conversion cancelled — describe what you want here, or press ‹ to go back.');
        }
        setBusy(false);
        trace('cancelled (stop) — turn aborted');
      } else if (msg.type === 'api_lost') {
        if (bubble) { bubble.setTyping(false); bubble.update(`⚠ ${msg.message || 'The API connection was lost.'}`); }
        if (editMode) { endEditGen(); surfaceEditModeError(`⚠ ${msg.message || 'The API connection was lost.'}`); }
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
        const n = (msg.offenders && msg.offenders.length) || 0;
        trace(`validation_error: ${n} oversize codex entry/entries; retry ${state.codexValidationRetries}/${MAX_CREATOR_RETRIES}`);
        if (bubble) {
          bubble.setTyping(false);
          bubble.update('⚠ A codex entry exceeded the 1400-character embedding cap — asking the assistant to split it…');
        }
        if (creatorRetryAllowed(state.codexValidationRetries)) {
          // Brief pause so the notice is readable before the retry overwrites
          // it. The scheduled callback re-checks the epoch (the guard inside
          // channel.onmessage can't stop the CALL — a ‹/⌂ exit during the
          // pause must never start a new GLM turn on the next wizard run),
          // + the timer is cancellable via stopTurn.
          root._creatorRetryTimer = setTimeout(() => {
            root._creatorRetryTimer = null;
            if (epoch !== root._creatorEpoch) return;
            callApi(opts);
          }, 900);
        } else {
          const failMsg = '⚠ The assistant could not fit a codex entry under the 1400-character cap — edit again or describe the split yourself.';
          if (editMode) { endEditGen(); surfaceEditModeError(failMsg); }
          else if (bubble) { bubble.setTyping(false); bubble.update(failMsg); }
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
      if (editMode) { endEditGen(); surfaceEditModeError(`⚠ ${e.message || e}`); }
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
    const alert = `SYSTEM ALERT: your ready draft is missing mandatory fields: ${missing.join(', ')}. ` +
      'Do not emit ready until every mandatory field is filled. Fill the missing fields now ' +
      'from the conversation — ask the user only for what you cannot infer.';
    state.history.push({ role: 'user', content: alert });
    state.mandatoryRetries = (state.mandatoryRetries || 0) + 1;
    const labels = missing.map((k) => MANDATORY_LABELS[k] || k).join(', ');
    trace(`ready REJECTED — missing mandatory [${missing.join(', ')}]; retry ${state.mandatoryRetries}/${MAX_CREATOR_RETRIES}`);
    if (creatorRetryAllowed(state.mandatoryRetries)) {
      if (bubble) {
        bubble.setTyping(false);
        bubble.update('⚠ The draft was missing mandatory fields — asking the assistant to fill them…');
      }
      // Brief pause so the notice is readable before the retry overwrites it
      // (the ring persists across the pause in edit mode — beginEditGen
      // dedupes on the retried callApi). Epoch re-check + stopTurn-
      // cancellable, same as the codex validation retry above.
      root._creatorRetryTimer = setTimeout(() => {
        root._creatorRetryTimer = null;
        if (epoch !== root._creatorEpoch) return;
        callApi({ editMode });
      }, 900);
    } else {
      const msg = `⚠ The assistant could not fill the mandatory fields: ${labels}. Tell it the missing details and it will finalize.`;
      if (editMode) { endEditGen(); surfaceEditModeError(msg); }
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
    // (2026-08-18) Import supersession: GLM signals the user's concept
    // replaced the import (the <import> block's discard rule). Drop the import
    // context for every later turn + rebuild the draft from THIS envelope
    // alone — mergeDraft can never remove import-derived keys (empty-skip), so
    // a plain merge would keep fusing the two. The imported portrait goes with
    // it (it is the imported character's likeness, not the new concept's).
    if (env.discard_import === true && state.importData) {
      state.importData = null;
      state.portraitBytes = null;
      state.portraitExt = null;
      state.portraitPreview = null;
      const fresh = {};
      if (env.draft && typeof env.draft === 'object') mergeDraft(fresh, env.draft);
      state.draft = fresh;
      trace('import discarded — draft rebuilt from envelope');
    } else if (env.draft && typeof env.draft === 'object') {
      mergeDraft(state.draft, env.draft);
    }
    // (2026-08-18) An explicit intro decline clears any seeded/accumulated
    // intro — the model cannot blank a field itself (mergeDraft's empty-skip),
    // so the decline is enforced mechanically. Without this, a user who
    // declined an intro still shipped the pre-seeded import greeting as the
    // card's <intro>.
    if (env.draft && env.draft.intro_answered === false) {
      delete state.draft.intro;
    }
    trace(`envelope action=${env.action || '(none)'} draftKeys=[${Object.keys(env.draft || {}).join(',')}]`);
    if (env.action === 'ready') {
      // The gate: no incomplete draft ever shows the review card. Runs on the
      // merged draft (accumulated across turns + seed/import presets).
      // (P2 parity fix) Edit runs (seedDraft) are EXEMPT — a legacy player
      // saved before `body_type` was promoted could never re-save: every
      // pencil-edit `ready` was rejected → two corrective retries
      // re-interviewing for fields the user never touched. doCreate's
      // backstop carries the same exemption.
      const missing = config.seedDraft ? [] : missingMandatoryFields(creatorKind, state.draft);
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

  // Edit-mode error routing: with a review card on screen the error lands ON
  // it (showReviewError); during a one-shot import conversion there is no
  // review yet — showReviewError no-ops while review is hidden, which would
  // make the failure invisible. Drop to the chat surface instead so the ⚠ is
  // readable and the composer offers a retry path.
  function surfaceEditModeError(msg) {
    if (importConvert) exitReviewToChat(msg);
    else showReviewError(msg);
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
      if (root._creatorCloseEditPopup === close) root._creatorCloseEditPopup = null;
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
    // (2026-08-15 audit fix) The popup's document keydown releases via ‹
    // (root._creatorCloseEditPopup) or a creator re-render (overlay._cleanup)
    // — not only its own close paths.
    overlay._cleanup = () => document.removeEventListener('keydown', onKey, { capture: true });
    root._creatorCloseEditPopup = close;
    overlay.hidden = false;
    void overlay.offsetWidth;
    overlay.classList.add('is-open');
    input.focus();
  }

  // Fire an edit turn from the review card: user text into history, then the
  // shared turn engine in edit mode (blur + bronze ring until it settles).
  function requestEdit(text) {
    // (2026-08-16 audit M6) Seeded edit runs are blind: the run's history
    // carries ONLY the user's instruction ("make her hair red") — GLM never
    // sees the entity being edited, reinvents the whole identity from the
    // name, and mergeDraft then overwrites every field. Mid-creation edits
    // don't need this (their history already carries the gathering
    // conversation). Inject the entity's CURRENT state as context right
    // before the instruction, size-capped so a codex-laden draft can't
    // bloat the call. Consecutive user turns are fine for the API path.
    if (config.seedDraft) {
      try {
        let snapshot = JSON.stringify(state.draft, null, 2) || '';
        if (snapshot.length > 6000) snapshot = snapshot.slice(0, 6000) + '\n…(truncated)';
        state.history.push({
          role: 'user',
          content: `Current state of the ${creatorKind === 'player' ? 'player' : 'card'} being edited ` +
            `(rework THIS — keep every field the instruction does not mention):\n${snapshot}`,
        });
      } catch (_) { /* unserializable draft — the instruction goes alone */ }
    }
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
    if (backBtn) backBtn.addEventListener('click', () => {
      // (2026-08-16 yellow J5) Never exit mid-CREATE: the write is a
      // multi-IPC sequence whose late failure surfaces ON the review card
      // (showReviewError no-ops once the card is hidden) — a ‹ click during
      // the write made that failure invisible.
      if (state.busy) return;
      exitReviewToChat();
    });
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
  // (2026-08-15 audit fix) Ids THIS screen has already written. A CREATE that
  // fails AFTER the entity write (e.g. the portrait upload throws) left a
  // complete entity on disk; the retry then hit the duplicate-name guard on
  // ITS OWN half-finished id and dead-ended. The guard exempts our own
  // minted ids — a retry is a resume-overwrite, not a collision.
  const mintedIds = new Set();
  async function doCreate(btn) {
    if (btn.disabled) return;
    btn.disabled = true;
    btn.textContent = 'Creating...';
    // (2026-08-16 audit LOW) state.busy for the WHOLE create — the pencil +
    // the ‹ back stay clickable during the multi-IPC write otherwise
    // (openEditPopup gates on state.busy; the composer half of setBusy is a
    // no-op in review mode where it's already hidden).
    setBusy(true);
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
      // (seedDraft present) re-save the same entity and are exempt — but
      // ONLY while the write target is still the seeded entity's own id:
      // the target is re-derived from the possibly-RENAMED draft, and a
      // pencil-edit that renames "Kael" onto an existing "Nyx" would
      // silently destroy that player (the exact loss this guard prevents).
      // The decision lives in creator-engine (shouldRejectDuplicateName)
      // so it's unit-testable.
      if (creatorKind === 'player' || creatorKind === 'sim') {
        const target = creatorKind === 'player'
          ? serializePlayer(state.draft).id
          // Symbol-only name → empty slug → Rust derives the id "unknown"
          // (empty <id> is filtered, name-derivation is sentinel-filtered).
          // The folder fallback MUST match or state splits into a phantom
          // folder the id can never load (2026-08-16 audit low).
          : (slugify(state.draft.name || '') || 'unknown');
        const seededId = config.seedDraft ? config.seedDraft.id : undefined;
        let existingIds = [];
        try {
          const existing = creatorKind === 'player'
            ? (await invoke('fable_players_list'))
            : (await invoke('fable_cards_list'));
          existingIds = (existing || []).map((m) => m.id);
        } catch (_) { /* list IPC failure → skip the guard (the write may still fail server-side) */ }
        if (shouldRejectDuplicateName(target, seededId, existingIds) && !mintedIds.has(target)) {
          throw new Error(`a ${creatorKind === 'player' ? 'player' : 'world'} named "${state.draft.name || target}" already exists — choose a different name`);
        }
      }
      if (creatorKind === 'player') {
        const { id, player } = serializePlayer(state.draft);
        // (2026-08-15 audit fix) Edit-run identity preservation:
        // serializePlayer builds a fresh object modeling only the wizard's
        // fields — re-saving an EDIT silently dropped identity-file keys it
        // doesn't model (notably `portrait`, killing load_player_portrait).
        // Merge forward every key the serializer didn't set from the stored
        // player JSON (fetch via fable_player_get, same source the picker
        // lazy-loads portraits from); serializer-set keys always win.
        if (config.seedDraft && config.seedDraft.id) {
          try {
            const existing = await invoke('fable_player_get', { id: config.seedDraft.id });
            if (existing && typeof existing === 'object') {
              for (const [k, v] of Object.entries(existing)) {
                if (!(k in player) && k !== 'id') player[k] = v;
              }
            }
          } catch (_) { /* unreadable prior entity — write the fresh form */ }
        }
        trace(`serializePlayer → id=${id} fields=[${Object.keys(player).join(',')}]`);
        // (2026-08-15 audit fix) Rename-on-edit cleanup: pass the SEEDED id
        // (the entity's old id) as the write's `id` param so the backend's
        // rename branch fires (old folder → new slug, portrait rides along).
        // The fresh-derived id made `slug == id` always true, so the old
        // folder was silently orphaned on every rename edit.
        const writeId = (config.seedDraft && config.seedDraft.id) ? config.seedDraft.id : id;
        const meta = await invoke('fable_player_write', { id: writeId, player });
        mintedIds.add(meta.id);
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
        const stem = slugify(state.draft.name || '') || 'unknown';
        trace(`serializeSimCard → stem=${stem} xml=${xml.length}b intro=${intro ? intro.length + 'b' : 'none'}`);
        // `<intro>` is embedded AFTER </sim_card> in the XML itself
        // (2026-08-13), so fable_write_card carries it — no separate .intro
        // sibling-file write.
        const meta = await invoke('fable_write_card', { stem, xml });
        mintedIds.add(meta.id);
        // (2026-08-15 audit fix) Rename-on-edit cleanup: a card edit under a
        // NEW name writes a NEW folder (cards have no rename path); the OLD
        // card would linger in the worlds list + hold its saves/memory. The
        // duplicate guard already proved the new slug is fresh — reap the
        // seeded card after the successful write.
        if (config.seedDraft && config.seedDraft.id && config.seedDraft.id !== meta.id) {
          try {
            await invoke('fable_card_delete', { cardId: config.seedDraft.id });
            trace(`reaped pre-rename card id=${config.seedDraft.id}`);
          } catch (e) {
            console.warn('[creator-chat] old card reap failed (orphaned):', e);
          }
        }
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
      setBusy(false);
      trace(`CREATE FAILED (${creatorKind}): ${e.message || e}`);
      // The prompt block is HIDDEN in review mode — surface the failure ON the
      // review card so a rejected write is never an invisible "does nothing".
      showReviewError(`Create failed: ${e.message || e}. Fix the concept and try again.`);
      console.error('[creator-chat] create failed', e);
    }
  }

  // The one-shot import conversion: seed the history with the conversion
  // instruction + fire the turn in edit mode — the SAME bronze-ring blur the
  // review-card pencil edit uses (callApi({editMode:true})): ring up for the
  // whole conversion, showReview() lands the final card on `ready`
  // (mandatory-gate retries re-fire beneath the still-spinning ring). The
  // chat surface is only the fallback surface (ask / failure / ‹ from
  // review).
  if (importConvert) {
    state.history.push({
      role: 'user',
      content: IMPORT_CONVERT_INSTRUCTIONS[creatorKind] || IMPORT_CONVERT_INSTRUCTIONS.player,
    });
    trace(`import convert (${creatorKind}) — one-shot turn fired`);
    callApi({ editMode: true });
  } else if (config.initialMessage) {
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
