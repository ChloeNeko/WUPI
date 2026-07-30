// =============================================================
// SCREEN: INTERVIEW — the New Game authoring surface (Phase D).
//
// A conversational interview between the user and the Game Master, with a
// live "sim card" preview that takes shape at the top of the screen as the
// detached Scribe extracts facts from the exchange. This is the durable
// counterpart to Quick Play's ephemeral void flow: instead of four fixed
// questions + a single generation pass, the user carries on a free-form
// chat with the GM, and the backend's scribe pass incrementally fills a
// draft (name, setting, tone, NPCs, traits, ...). When the GM emits
// [READY] (or the user clicks Begin once the draft is finalizable), the
// draft is committed to a .sim file and the user is handed off to the
// stage with the seeded world/player state.
//
// LAYOUT (vertical, per spec): the sim-card preview pins to the TOP
// CENTER, the GM chat feed fills the MIDDLE, and the compose box + Begin
// button pin to the BOTTOM CENTER. A void-particle backdrop renders
// behind everything (reused from void-particles.js — same cool motes).
//
// LIFECYCLE (driven by fable.js):
//   buildInterview()                 → constructs the screen DOM (once at boot)
//   wireInterview(root, hooks)       → binds input + IPC + scribe listener,
//                                       starts particles, kicks the GM (per entry)
//   teardownInterview()              → cancels RAF, drops listeners, resets state
//
// EVENT ROUTING:
//   - interview_send streams GM chunks via a Channel: chunk/gm_done/ready/
//     fallback/error. The feed's beats.js instance renders them live.
//   - The detached scribe emits facts as global 'interview-fact' events
//     (field-by-field ScribeFact deltas, plus a 'scribe_done' snapshot at
//     the end of each scribe pass, plus 'scribe_warning' on graceful
//     degrade). The preview panel consumes them: per-field events are the
//     "what just popped in" hints, the scribe_done snapshot is the source
//     of truth (applied verbatim each pass so the preview never drifts).
//
// NAMING (§11.29): the user is referred to neutrally everywhere — "Your
// card is taking shape...", "Name:", "Setting:", etc. No titles.
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { getCurrentWebview } from '@tauri-apps/api/webview';
import { createVoidParticles } from './void-particles.js';
import * as beats from '../engine/beats.js';

// ── Tunable timings (ms) ────────────────────────────────────────────────
const FINAL_FADE      = 1500;   // the screen's fade-to-black overlay duration
const SCRIBE_TOAST_MS = 4000;   // how long a scribe-warning chip stays up
const FINALIZE_HINT_MS = 99999; // the "Weaving your tale..." line holds until swap

// ── The preview's field manifest ────────────────────────────────────────
// Each entry maps a draft field to a labeled slot in the preview card. The
// `label` is the user-facing prefix; `key` is the InterviewDraftSnapshot
// field name. NPCs + traits render as chip rows (collections), not slots.
const SLOTS = [
  { key: 'name',             label: 'Name' },
  { key: 'setting',          label: 'Setting' },
  { key: 'tone',             label: 'Tone' },
  { key: 'player_name',      label: 'Player' },
  { key: 'player_background',label: 'Background' },
  { key: 'starting_condition', label: 'Starting condition' },
];

// ── Module state ────────────────────────────────────────────────────────
let interviewRoot = null;       // the screen element
let particles = null;           // particle system controller (destroyed on teardown)
let feedEl = null;              // the chat feed element (passed to beats.initBeats)
let inputEl = null;             // the compose textarea
let beginBtn = null;            // the Begin button (revealed on ready)
let stopBtn = null;             // the Stop button (visible during a GM stream)
let scribeToast = null;         // the scribe-warning chip
let progressFill = null;        // the progress bar's fill element
let progressLabel = null;       // the progress bar's pct label
let slotValueEls = {};          // key → the value span for a slot
let npcChipsEl = null;          // the NPC chips container
let traitChipsEl = null;        // the trait chips container
let fallbackChip = null;        // the "local fallback" chip (subtle)

let draft = null;               // last-known InterviewDraftSnapshot (source of truth)
let streaming = false;          // true while a GM stream is in flight
let finalized = false;          // true once interview_finalize has fired
let aborted = false;            // set by teardownInterview to break the await chain
let ready = false;              // true once the GM emitted [READY] (Begin revealed)
let beginHook = null;           // hooks.onFinalized, fired after a successful finalize
let listeners = [];             // [el, type, handler] tracked for teardown
let unlistenFact = null;        // the interview-fact unsubscribe fn
let activeBeat = null;          // the currently-streaming narrator beat
let toastTimer = null;          // scribeToast auto-dismiss timer

// ── Import flow state (2026-07-29, the "Create vs Import" fork) ──────────
let choiceMode = false;         // true while the Create/Import buttons are showing
let dropMode = false;           // true while the screen is listening for a file drop
let deciphering = false;        // true while import_decipher_card is running
let unlistenDragDrop = null;    // the tauri://drag-drop unsubscribe fn
let dropOverlay = null;         // the full-screen drop-zone overlay element

export function buildInterview() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-interview-screen';
  root.dataset.fableScreen = 'interview';
  root.hidden = true;
  // The slots + chip rows are built once here; their contents refresh in
  // renderPreview() as facts arrive. The particle host + feed + input are
  // also static — only their runtime state churns per entry.
  const slotRows = SLOTS.map((s) =>
    `      <div class="fable-interview-slot" data-slot="${s.key}">
        <span class="fable-interview-slot-label">${s.label}</span>
        <span class="fable-interview-slot-value" data-slot-value="${s.key}">…</span>
      </div>`
  ).join('\n');
  root.innerHTML = `
    <div class="fable-interview-particles" aria-hidden="true"></div>

    <!-- TOP CENTER: the sim-card preview. Always visible; fields pop in. -->
    <div class="fable-interview-preview" data-preview>
      <div class="fable-interview-preview-title">Your card is taking shape…</div>
      <div class="fable-interview-preview-grid">
${slotRows}
      </div>
      <div class="fable-interview-npc-chips" data-npc-chips></div>
      <div class="fable-interview-trait-chips" data-trait-chips></div>
      <div class="fable-interview-progress" data-progress>
        <div class="fable-interview-progress-track">
          <div class="fable-interview-progress-fill" data-progress-fill></div>
        </div>
        <span class="fable-interview-progress-label" data-progress-label>0%</span>
      </div>
      <div class="fable-interview-fallback-chip" data-fallback-chip hidden>local fallback</div>
    </div>

    <!-- MIDDLE: the GM chat feed (a SEPARATE beats.js instance). -->
    <div class="fable-interview-feed" data-feed></div>

    <!-- BOTTOM CENTER: compose + Begin/Stop. -->
    <div class="fable-interview-bottom">
      <div class="fable-interview-scribe-toast" data-scribe-toast hidden></div>
      <div class="fable-interview-input-row">
        <textarea class="fable-interview-input" data-input rows="1"
                  placeholder="Describe your world… (Enter to send, Shift+Enter for a new line)"
                  aria-label="Your message"></textarea>
        <button class="fable-interview-stop-btn" data-stop-btn hidden>Stop</button>
        <button class="fable-interview-begin-btn" data-begin-btn hidden>Begin ▶</button>
      </div>
    </div>
  `;
  return root;
}

// Bind input + IPC + scribe listener + start particles. Called on every
// interview entry (the DOM is reused, so listeners go through on() which
// teardown removes — mirrors void.js's discipline).
export function wireInterview(root, hooks) {
  interviewRoot = root;
  beginHook = hooks && hooks.onFinalized ? hooks.onFinalized : null;

  feedEl = root.querySelector('[data-feed]');
  inputEl = root.querySelector('[data-input]');
  beginBtn = root.querySelector('[data-begin-btn]');
  stopBtn = root.querySelector('[data-stop-btn]');
  scribeToast = root.querySelector('[data-scribe-toast]');
  progressFill = root.querySelector('[data-progress-fill]');
  progressLabel = root.querySelector('[data-progress-label]');
  npcChipsEl = root.querySelector('[data-npc-chips]');
  traitChipsEl = root.querySelector('[data-trait-chips]');
  fallbackChip = root.querySelector('[data-fallback-chip]');

  // Cache the slot value spans (key → element) so renderPreview is O(slots).
  slotValueEls = {};
  for (const s of SLOTS) {
    slotValueEls[s.key] = root.querySelector(`[data-slot-value="${s.key}"]`);
  }

  // Fresh particle field per entry (mirrors void.js).
  const particleHost = root.querySelector('.fable-interview-particles');
  if (particleHost) particles = createVoidParticles(particleHost);

  // Reset runtime state on each entry — a new New Game starts fresh.
  draft = null;
  streaming = false;
  finalized = false;
  aborted = false;
  ready = false;
  activeBeat = null;
  // Import-flow state (2026-07-29): reset on every entry.
  choiceMode = false;
  dropMode = false;
  deciphering = false;
  if (unlistenDragDrop) { try { unlistenDragDrop(); } catch (_) {} unlistenDragDrop = null; }
  if (dropOverlay && dropOverlay.parentNode) dropOverlay.parentNode.removeChild(dropOverlay);
  dropOverlay = null;
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }

  // Reset the DOM to its empty state.
  if (feedEl) feedEl.innerHTML = '';
  if (inputEl) { inputEl.value = ''; inputEl.style.height = 'auto'; inputEl.disabled = false; }
  if (beginBtn) beginBtn.hidden = true;
  if (stopBtn) stopBtn.hidden = true;
  if (scribeToast) { scribeToast.hidden = true; scribeToast.textContent = ''; }
  if (fallbackChip) fallbackChip.hidden = true;
  renderPreview(null);

  // The chat feed uses the SAME beats.js module as the stage — but the
  // module is single-feed, so we re-init it on OUR feed element. The
  // stage's feed is torn down (teardownStage) before we show, so there's
  // no contention; we hand the feed back in teardownInterview.
  beats.initBeats(feedEl);

  // Enter to send (Shift+Enter for newline). Mirrors void.js's input
  // handling. The field is disabled while a GM stream is in flight so a
  // mid-stream Enter can't queue a second turn.
  on(inputEl, 'keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      if (streaming || finalized) return;
      const text = (inputEl.value || '').trim();
      if (!text) return;
      sendTurn(text);
    }
  });
  on(inputEl, 'input', () => autoGrow(inputEl));

  // Stop button: visible only while a GM stream is in flight.
  on(stopBtn, 'click', () => {
    if (!streaming) return;
    invoke('interview_stop').catch((err) => {
      console.error('[interview] interview_stop failed', err);
    });
  });

  // Begin button: finalize the draft → hand off to the stage.
  on(beginBtn, 'click', () => {
    if (finalized) return;
    finalizeInterview();
  });

  // Subscribe to the detached scribe's global 'interview-fact' events.
  // Per-field events animate the preview; the scribe_done snapshot is the
  // source of truth (applied verbatim). scribe_warning surfaces a toast.
  listen('interview-fact', (e) => onFact(e && e.payload)).catch((err) => {
    console.error('[interview] listen(interview-fact) failed', err);
  }).then((un) => { unlistenFact = un; });

  // Kick off the interview: reset session + draft server-side, then send
  // an opening beat so the GM greets the user. Fire-and-forget — wireInterview
  // returns immediately so fable.js can finish wiring.
  startInterview().catch((err) => {
    console.error('[interview] start sequence threw', err);
  });

  // Focus the compose box lazily so it's ready for the user's first reply.
  setTimeout(() => inputEl && inputEl.focus(), 80);
}

// ── The opening sequence ────────────────────────────────────────────────
//
// interview_start resets the server-side session + draft (call once on
// entry). We DON'T auto-send a greeting — the GM's opening beat arrives
// via the first interview_send. Instead we send an empty-ish "begin"
// turn so the GM has something to respond to. The backend appends the
// user turn + streams the GM reply; the scribe extracts facts after.
async function startInterview() {
  try {
    await invoke('interview_start');
  } catch (err) {
    console.error('[interview] interview_start failed', err);
    // Surface a soft error beat so the user isn't staring at a blank feed.
    beats.addErrorBeat('Could not start the interview. Try again.');
    return;
  }
  // Late-join: if a draft already exists (reconnect path), pull it so the
  // preview isn't empty until the first scribe pass lands.
  try {
    const snap = await invoke('interview_draft_state');
    if (snap) {
      draft = snap;
      renderPreview(snap);
      if (snap && snap.is_finalizable && !ready) revealBegin();
    }
  } catch (err) {
    console.error('[interview] interview_draft_state failed', err);
  }
  // CANNED greeting + the Create/Import choice (Gemini ruling #1: never spend
  // model tokens on a UI greeting). The GM beat is a canned system beat; the
  // two buttons are frontend-rendered, not model-generated. The user picks;
  // only AFTER the pick does the model get involved (Create → normal interview;
  // Import → the decipher pass).
  const greetBeat = beats.startNarratorBeat();
  beats.appendChunk(greetBeat,
    'Welcome. I am the Game Master.\n\n' +
    'Would you like to create something from scratch, or import something you own?');
  beats.finalizeBeat(greetBeat);
  showChoice();
}

// ── The Create/Import choice ────────────────────────────────────────────
//
// Renders two buttons (Create / Import) as a system beat under the greeting.
// Create → canned GM acknowledgement + the normal interview question ladder.
// Import → canned GM acknowledgement + drop-listening mode (the whole screen
// becomes a drop zone + a folder-icon fallback button).
function showChoice() {
  choiceMode = true;
  const choice = document.createElement('div');
  choice.className = 'fable-interview-choice';
  const createBtn = document.createElement('button');
  createBtn.type = 'button';
  createBtn.className = 'fable-interview-choice-btn';
  createBtn.textContent = 'Create';
  const importBtn = document.createElement('button');
  importBtn.type = 'button';
  importBtn.className = 'fable-interview-choice-btn';
  importBtn.textContent = 'Import';
  choice.appendChild(createBtn);
  choice.appendChild(importBtn);
  // Append the choice row into the feed as a system-style beat so it sits
  // right under the greeting + scrolls naturally.
  if (feedEl) feedEl.appendChild(choice);
  beats.scrollDown();

  const choose = (kind) => {
    if (!choiceMode) return;
    choiceMode = false;
    if (choice.parentNode) choice.remove();
    if (kind === 'create') {
      onChooseCreate();
    } else {
      onChooseImport();
    }
  };
  createBtn.addEventListener('click', () => choose('create'));
  importBtn.addEventListener('click', () => choose('import'));
}

// Create: canned GM ack, then kick the normal question ladder via the first
// real interview_send turn.
function onChooseCreate() {
  const ack = beats.startNarratorBeat();
  beats.appendChunk(ack,
    'Ah, so you wish to create something new? Very well — let us begin.');
  beats.finalizeBeat(ack);
  // The first real turn: hand control to the persona-driven GM. A short
  // handshake is enough; the GM's system prompt drives the question ladder.
  sendTurn("Let's build a new world together.");
}

// Import: canned GM ack, then enter drop-listening mode.
function onChooseImport() {
  const ack = beats.startNarratorBeat();
  beats.appendChunk(ack,
    'Oh, you already have something? Please drag it over here and show me.');
  beats.finalizeBeat(ack);
  enterDropMode();
}

// ── Drop-listening mode ─────────────────────────────────────────────────
//
// Gemini ruling #3: a GATED JS listener scoped to the interview screen. We do
// NOT toggle Tauri's global dragDropEnabled flag (that races the chat input's
// text-drop). Instead we listen to Tauri's native drag-drop events
// (tauri://drag-drop) ONLY while dropMode is true + the interview is mounted.
// The whole screen becomes a drop target (a dimming overlay + central prompt)
// + a white folder-icon button below for users who prefer a native picker.
function enterDropMode() {
  if (dropMode) return;
  dropMode = true;

  // The drop overlay: dims the screen + shows a central "drop here" prompt +
  // the folder-icon fallback. Lives inside the interview screen so teardown
  // removes it with the DOM.
  dropOverlay = document.createElement('div');
  dropOverlay.className = 'fable-interview-drop';
  dropOverlay.innerHTML = `
    <div class="fable-interview-drop-inner">
      <div class="fable-interview-drop-prompt">Drop your card file here</div>
      <div class="fable-interview-drop-hint">.png or .json · character card or lorebook</div>
      <button type="button" class="fable-interview-folder-btn" data-folder-btn>
        <span class="fable-interview-folder-icon">🗀</span>
        <span>or browse…</span>
      </button>
    </div>`;
  if (interviewRoot) interviewRoot.appendChild(dropOverlay);

  // The folder-icon fallback → native OS picker, filtered to card files.
  const folderBtn = dropOverlay.querySelector('[data-folder-btn]');
  if (folderBtn) {
    folderBtn.addEventListener('click', () => {
      openDialog({
        multiple: false,
        filters: [
          { name: 'Card files', extensions: ['png', 'json'] },
        ],
      }).then((selected) => {
        if (!selected) return;
        // The dialog returns either a single path string or {path} on some
        // platforms — normalize to a path string.
        const p = typeof selected === 'string' ? selected : (selected && selected.path) || null;
        if (p) handleDroppedFile(p);
      }).catch((err) => {
        console.error('[interview] folder picker failed', err);
      });
    });
  }

  // The gated Tauri drag-drop listener. Tauri 2 emits 'tauri://drag-drop'
  // with payload { type: 'drop', paths: [abs, ...] }. We only act on it while
  // dropMode is true (the gate) + the interview screen is the active screen.
  // Other screens' drops do nothing here (the listener is unsubscribed on
  // teardown). This keeps the chat input's text-drop behavior intact outside
  // the import flow.
  try {
    const wv = getCurrentWebview();
    const off = wv.onDragDropEvent((e) => {
      if (!dropMode) return;
      if (e && e.payload && e.payload.type === 'drop') {
        const paths = Array.isArray(e.payload.paths) ? e.payload.paths : [];
        if (paths.length > 0) handleDroppedFile(paths[0]);
      }
    });
    unlistenDragDrop = off;
  } catch (err) {
    console.error('[interview] drag-drop listener bind failed', err);
  }
}

function exitDropMode() {
  dropMode = false;
  if (unlistenDragDrop) { try { unlistenDragDrop(); } catch (_) {} unlistenDragDrop = null; }
  if (dropOverlay && dropOverlay.parentNode) dropOverlay.parentNode.removeChild(dropOverlay);
  dropOverlay = null;
}

// A file was dropped or picked. Hide the drop zone, show a canned "one
// moment" GM beat, then fire the decipher IPC.
function handleDroppedFile(path) {
  if (deciphering) return;
  exitDropMode();
  const beat = beats.startNarratorBeat();
  beats.appendChunk(beat, 'Oh, this is very interesting. Please give me one moment…');
  beats.finalizeBeat(beat);
  runDecipher(path);
}

// ── The decipher pass (import_decipher_card via Channel) ────────────────
//
// Mirrors the interview_send Channel pattern. Routes deciphering/done/error.
// On done (card kind), the draft snapshot fills the preview + a canned GM
// beat asks if it's alright. On done (lorebook kind), a beat notes how many
// entries were imported. The UNCHANGED refinement loop takes over from here.
async function runDecipher(path) {
  if (deciphering) return;
  deciphering = true;
  if (inputEl) inputEl.disabled = true;

  const channel = new Channel();
  channel.onmessage = (msg) => handleDecipherEvent(msg);

  try {
    await invoke('import_decipher_card', { path, onEvent: channel });
  } catch (err) {
    console.error('[interview] import_decipher_card failed', err);
    beats.addErrorBeat(String((err && err.message) || err || 'Could not read that file.'));
  } finally {
    deciphering = false;
    if (inputEl) inputEl.disabled = false;
  }
}

function handleDecipherEvent(msg) {
  if (!msg || typeof msg !== 'object') return;
  switch (msg.type) {
    case 'deciphering':
      // The model is rebuilding the prose. (The canned "one moment" beat is
      // already showing.) No UI action needed.
      break;
    case 'done': {
      if (msg.kind === 'lorebook') {
        const n = typeof msg.entry_count === 'number' ? msg.entry_count : 0;
        const beat = beats.startNarratorBeat();
        beats.appendChunk(beat,
          n > 0
            ? `I've absorbed ${n} entries from your lorebook into the world's memory. Is there a character you'd like to bring in next?`
            : 'That lorebook had no entries I could read. Is there something else you\'d like to import?');
        beats.finalizeBeat(beat);
      } else {
        // Card kind: the draft snapshot filled server-side. Pull it so the
        // preview reflects the imported card, then ask if it's alright.
        const snap = msg.draft || null;
        if (snap) {
          draft = snap;
          renderPreview(snap);
          if (snap.is_finalizable && !ready) revealBegin();
        }
        const beat = beats.startNarratorBeat();
        beats.appendChunk(beat,
          'Here is what I made of your card. Is this alright, or would you like to make some adjustments?');
        beats.finalizeBeat(beat);
      }
      break;
    }
    case 'error': {
      const m = typeof msg.message === 'string' ? msg.message : 'I could not decipher that file.';
      beats.addErrorBeat(m);
      break;
    }
    default:
      break;
  }
}

// ── Send one GM turn ────────────────────────────────────────────────────
//
// Mirrors void.js's Channel + invoke pattern. Opens a fresh Channel per
// turn, routes chunk/gm_done/ready/fallback/error, and keeps the
// streaming flag + Stop button in sync. The scribe's facts arrive
// separately via the 'interview-fact' listener (not on this Channel).
async function sendTurn(text) {
  if (streaming || finalized || aborted) return;
  if (!text) return;

  // Pin the user's message into the feed BEFORE the invoke so it reads as
  // "sent" immediately (mirrors void.js's answer-pin discipline).
  beats.addUserBeat(text);
  if (inputEl) { inputEl.value = ''; inputEl.style.height = 'auto'; }

  streaming = true;
  setStreamingUI(true);
  activeBeat = beats.startNarratorBeat();

  const channel = new Channel();
  channel.onmessage = (msg) => handleEvent(msg);

  try {
    await invoke('interview_send', { text, onEvent: channel });
  } catch (err) {
    // The invoke itself rejected (transport / hard error). Surface it in
    // the feed + drop the streaming state so the user can retry.
    console.error('[interview] interview_send failed', err);
    if (activeBeat) {
      beats.finalizeBeat(activeBeat);
      activeBeat = null;
    }
    beats.addErrorBeat(String((err && err.message) || err || 'The GM could not respond.'));
  } finally {
    // The stream is done (the invoke resolved). The scribe may still be
    // running detached — its facts arrive via the listener.
    streaming = false;
    setStreamingUI(false);
    if (activeBeat) {
      // Defensive: if gm_done never arrived (e.g. a transport hiccup),
      // finalize whatever was streamed so the caret doesn't hang.
      beats.finalizeBeat(activeBeat);
      activeBeat = null;
    }
  }
}

// Route one Channel event from interview_send.
function handleEvent(msg) {
  if (!msg || typeof msg !== 'object') return;
  const t = msg.type;
  // Defensive: unknown event types are a no-op (never throw on the stream).
  switch (t) {
    case 'chunk': {
      // GM streaming a token. Append to the live narrator beat.
      const text = typeof msg.text === 'string' ? msg.text : '';
      if (text && activeBeat) beats.appendChunk(activeBeat, text);
      break;
    }
    case 'gm_done': {
      // GM turn finished. Finalize the beat with the authoritative text.
      const finalText = typeof msg.final_text === 'string' ? msg.final_text : null;
      if (activeBeat) {
        beats.finalizeBeat(activeBeat, finalText);
        activeBeat = null;
      }
      break;
    }
    case 'ready': {
      // The GM emitted [READY]. The draft is final → enable Begin.
      if (!ready) revealBegin();
      break;
    }
    case 'fallback': {
      // API failed, fell back to local/echo. Show a subtle chip.
      showFallbackChip(msg.reason || msg.source || 'local');
      break;
    }
    case 'error': {
      // Hard error. Show in an error beat (don't throw — keep the stream alive).
      const m = typeof msg.message === 'string' ? msg.message : 'The GM hit an error.';
      beats.addErrorBeat(m);
      break;
    }
    case 'scribe_done': {
      // The detached scribe finished this turn. The snapshot it carries is
      // the SOURCE OF TRUTH for the preview — apply it verbatim so the
      // per-field hints (handled in onFact) never drift against reality.
      const snap = msg.draft || msg.snapshot || null;
      if (snap) {
        draft = snap;
        renderPreview(snap);
        if (snap.is_finalizable && !ready) revealBegin();
      }
      break;
    }
    default:
      // Unknown event — ignore (forward-compatible, never throw).
      break;
  }
}

// ── The interview-fact event handler ────────────────────────────────────
//
// Per-field ScribeFact events are "what just popped in" hints — they're
// used here only for the scribe_warning toast (the per-field deltas are
// already captured in the scribe_done snapshot, which handleEvent applies
// verbatim, so we don't double-render from them). The warning is the one
// payload that does NOT come with a snapshot, so it needs its own path.
function onFact(payload) {
  if (!payload || typeof payload !== 'object') return;
  // scribe_warning: the scribe hiccuped, draft unchanged. Surface a toast.
  if (payload.type === 'scribe_warning') {
    const m = typeof payload.message === 'string'
      ? payload.message
      : 'The Scribe could not extract facts this turn.';
    showScribeToast(m);
    return;
  }
  // Per-field ScribeFact: {field, value, action}. We rely on the
  // scribe_done snapshot (handleEvent) as the source of truth, so these
  // are intentionally NOT rendered here — rendering from both would race
  // (the snapshot always wins, so per-field rendering is wasted work +
  // a potential flicker source). Kept defensive in case a future caller
  // wants per-field animation hooks.
}

// ── Preview rendering ───────────────────────────────────────────────────
//
// renderPreview(snap) refreshes every slot + chip row + the progress bar
// from an InterviewDraftSnapshot. Idempotent: safe to call with the same
// snapshot repeatedly, or with null to reset to placeholders.
function renderPreview(snap) {
  // Slots: empty → faint placeholder "…"; filled → the value.
  for (const s of SLOTS) {
    const el = slotValueEls[s.key];
    if (!el) continue;
    const v = snap && snap[s.key] != null ? String(snap[s.key]) : '';
    if (v) {
      el.textContent = v;
      el.parentElement.classList.add('fable-interview-slot--filled');
    } else {
      el.textContent = '…';
      el.parentElement.classList.remove('fable-interview-slot--filled');
    }
  }

  // NPCs: render as chips that pop in. Each chip is a discrete element so
  // CSS can animate it on mount.
  renderChips(npcChipsEl, snap && Array.isArray(snap.start_npc_ids) ? snap.start_npc_ids : [], 'npc');
  renderChips(traitChipsEl, snap && Array.isArray(snap.traits) ? snap.traits : [], 'trait');

  // Progress bar: completion_pct (0–100).
  const pct = snap && Number.isFinite(snap.completion_pct) ? Math.max(0, Math.min(100, snap.completion_pct)) : 0;
  if (progressFill) progressFill.style.width = pct + '%';
  if (progressLabel) progressLabel.textContent = pct + '%';
}

// Render a chip row. Idempotent: rebuilds the row from the values array
// each call (cheap — collections are small). New chips get an .is-new
// class for one frame so CSS can animate the pop-in.
function renderChips(host, values, kind) {
  if (!host) return;
  const list = values || [];
  // Track existing chip texts so we only flag genuinely-new ones.
  const existing = new Set(
    Array.from(host.querySelectorAll('.fable-interview-npc-chip'))
      .map((el) => el.textContent)
  );
  host.innerHTML = '';
  if (!list.length) {
    host.classList.remove('has-chips');
    return;
  }
  host.classList.add('has-chips');
  for (const v of list) {
    const chip = document.createElement('span');
    chip.className = 'fable-interview-npc-chip';
    chip.textContent = String(v);
    if (!existing.has(String(v))) chip.classList.add('is-new');
    host.appendChild(chip);
  }
}

// ── Finalize: commit the draft + hand off to the stage ──────────────────
//
// On Begin click: disable inputs, run interview_finalize, then on success
// fire hooks.onFinalized(result). The fade-to-black overlay is played
// here (mirroring void.js's playExitOverlay) so the swap to the stage is
// invisible. The onFinalized callback (wired in fable.js) tears down the
// interview + transitions to the stage with the loadResult.
async function finalizeInterview() {
  if (finalized) return;
  finalized = true;
  // Disable inputs during the finalize window.
  if (inputEl) inputEl.disabled = true;
  if (beginBtn) beginBtn.disabled = true;
  if (stopBtn) stopBtn.hidden = true;

  // Show a small "Weaving your tale..." line in the feed during the brief
  // finalize window (the .sim write + opening-scene generation).
  const weavingBeat = beats.addSystemBeat('Weaving your tale…');

  let result = null;
  let finError = null;
  try {
    // openingPreference is null for now (no opening-preference picker yet).
    result = await invoke('interview_finalize', { openingPreference: null });
  } catch (err) {
    finError = err;
  }

  if (finError || !result) {
    // Restore the UI so the user can retry / keep talking.
    finalized = false;
    if (inputEl) inputEl.disabled = false;
    if (beginBtn) beginBtn.disabled = false;
    if (weavingBeat && weavingBeat.parentNode) weavingBeat.remove();
    beats.addErrorBeat(String((finError && finError.message) || finError || 'Could not finalize the draft.'));
    return;
  }

  // Fade to black, fire the handoff at peak, undim to reveal the stage.
  // The onFinalized callback does the actual screen swap + stage wiring
  // (fable.js mirrors quickPlayBegin's structure).
  await playExitOverlay(() => {
    try {
      if (beginHook) beginHook(result);
    } catch (e) {
      console.error('[interview] onFinalized threw', e);
    }
  });
}

// ── UI helpers ──────────────────────────────────────────────────────────

// Toggle streaming chrome: the Stop button visibility + the input's
// placeholder. The input itself is left enabled-but-guarded so the user
// can keep typing (the Enter handler short-circuits while streaming).
function setStreamingUI(on) {
  if (stopBtn) stopBtn.hidden = !on;
  if (inputEl) {
    if (on) {
      inputEl.dataset.idlePlaceholder = inputEl.placeholder;
      inputEl.placeholder = 'The GM is responding…';
    } else if (inputEl.dataset.idlePlaceholder != null) {
      inputEl.placeholder = inputEl.dataset.idlePlaceholder;
      delete inputEl.dataset.idlePlaceholder;
    }
  }
}

// Reveal the Begin button (the draft is final / the GM signaled [READY]).
function revealBegin() {
  ready = true;
  if (beginBtn) beginBtn.hidden = false;
}

// Show the "local fallback" chip (API failed → local/echo took over).
function showFallbackChip(source) {
  if (!fallbackChip) return;
  const label = source ? ('local fallback · ' + String(source)) : 'local fallback';
  fallbackChip.textContent = label;
  fallbackChip.hidden = false;
}

// Show the scribe-warning toast. Auto-dismisses after SCRIBE_TOAST_MS.
function showScribeToast(message) {
  if (!scribeToast) return;
  scribeToast.textContent = message;
  scribeToast.hidden = false;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    if (scribeToast) scribeToast.hidden = true;
    toastTimer = null;
  }, SCRIBE_TOAST_MS);
}

// The fade-to-black exit overlay (mirrors void.js's playExitOverlay). Fires
// onPeak() at peak black so fable.js can swap to the stage invisibly, then
// undims to reveal it.
function playExitOverlay(onPeak) {
  return new Promise((resolve) => {
    const overlay = document.createElement('div');
    overlay.className = 'fable-void-overlay';
    document.body.appendChild(overlay);
    void overlay.offsetWidth; // reflow so the transition restarts cleanly
    overlay.classList.add('dimming');

    // At peak black, fire the handoff.
    setTimeout(() => {
      try { onPeak && onPeak(); } catch (e) { console.error('[interview] onPeak threw', e); }
    }, FINAL_FADE);

    // Begin undimming slightly after peak so the stage is fully wired first.
    setTimeout(() => {
      overlay.classList.remove('dimming');
      overlay.classList.add('clearing');
    }, FINAL_FADE + 200);

    // Remove the overlay once it's fully clear.
    setTimeout(() => {
      if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
      resolve();
    }, FINAL_FADE + 200 + FINAL_FADE);
  });
}

// ── Listener tracking + teardown ────────────────────────────────────────

function autoGrow(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, 160) + 'px';
}

// Track a listener so teardownInterview removes it (the interview DOM is
// reused, so raw addEventListener would double-bind on re-wireInterview).
function on(el, type, handler) {
  if (!el) return;
  el.addEventListener(type, handler);
  listeners.push([el, type, handler]);
}

// Tear down: cancel particles, unsubscribe the scribe listener, abort any
// in-flight stream, clear state, remove listeners. Called by fable.js on
// any exit from the interview (the user began, OR EXIT fired mid-chat).
export function teardownInterview() {
  aborted = true;
  if (particles) { particles.destroy(); particles = null; }
  if (unlistenFact) { try { unlistenFact(); } catch (_) {} unlistenFact = null; }
  // Import-flow cleanup: unsubscribe the drag-drop listener + remove the drop
  // overlay so a re-entry never double-binds or leaves a stale overlay.
  exitDropMode();
  choiceMode = false;
  deciphering = false;
  if (toastTimer) { clearTimeout(toastTimer); toastTimer = null; }
  // Best-effort: stop an in-flight GM stream so the backend's cancel token
  // doesn't dangle. Safe no-op if nothing is streaming.
  if (streaming) {
    try { invoke('interview_stop').catch(() => {}); } catch (_) {}
  }
  for (const [el, type, handler] of listeners) {
    el.removeEventListener(type, handler);
  }
  listeners = [];
  if (inputEl) { inputEl.value = ''; inputEl.style.height = 'auto'; inputEl.disabled = false; }
  if (feedEl) feedEl.innerHTML = '';
  if (beginBtn) { beginBtn.hidden = true; beginBtn.disabled = false; }
  if (stopBtn) stopBtn.hidden = true;
  if (scribeToast) { scribeToast.hidden = true; scribeToast.textContent = ''; }
  if (fallbackChip) fallbackChip.hidden = true;
  slotValueEls = {};
  draft = null;
  streaming = false;
  finalized = false;
  ready = false;
  activeBeat = null;
  beginHook = null;
  interviewRoot = null;
  feedEl = null;
  inputEl = null;
  beginBtn = null;
  stopBtn = null;
  scribeToast = null;
  progressFill = null;
  progressLabel = null;
  npcChipsEl = null;
  traitChipsEl = null;
  fallbackChip = null;
}
