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
  // Seed the GM with an opening turn. The text is a neutral handshake —
  // the GM is persona-driven server-side, so a short prompt is enough to
  // get the conversation rolling without biasing the draft.
  sendTurn("Hi! I'd like to set up a new game. Walk me through what you need.");
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
