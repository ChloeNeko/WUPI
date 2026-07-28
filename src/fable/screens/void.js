// =============================================================
// SCREEN: VOID — the Quick Play interview surface (cinematic).
//
// A pure-black infinite void with subtle drifting particles. The user is
// greeted by fading large text in the middle of the screen, then asked
// four questions one at a time (each fading in/out like magic). After the
// last answer the UI disappears and the user sits in the void while the
// backend weaves the simulation (single interview_generate call). On
// success the void hands off to the stage with the generated card + the
// seeded world/player state; the first narrator beat is the card's
// opening_scene, already on screen.
//
// This is NOT a chat. There are no bubbles, no GM replies during the Q&A,
// and no send button — the user submits each answer with Enter. The
// user's typed answer persists visibly under the question after Enter
// (the prior bug where the answer "disappeared" on submit is fixed by
// pinning it into a `.fable-void-answer` node that fades out WITH the
// question, not before).
//
// LIFECYCLE (driven by fable.js):
//   buildVoid()                → constructs the screen DOM (called once at boot)
//   wireVoid({ onBegin })      → binds input + IPC, starts particles (per entry)
//   teardownVoid()             → cancels RAF, clears input, resets state (per exit)
//
// The exit handoff (on Begin):
//   1. The four questions + the input all fade out together.
//   2. The void + particles hold (the world is gone, only the void remains).
//   3. interview_generate runs against the backend (single model pass).
//   4. On done: fade the whole screen to full black, swap to the stage
//      invisibly under the overlay, undim to reveal the spawned scene.
//      On error: restore the last question so the user can retry.
//
// HISTORY is held CLIENT-SIDE here (the four answer strings). The server
// is fully stateless across the whole flow — only the single
// `interview_generate` call ever touches the model, with the four answers
// as args. Nothing is ever written to disk or memory.sqlite, so the
// "erase the interview" requirement is satisfied by construction.
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { createVoidParticles } from './void-particles.js';

// ── Tunable timings (ms) ────────────────────────────────────────────────
// All fade durations. Easy to adjust after seeing the flow live.
const FADE_IN       = 1400;   // text fades in over this long
const HOLD_INTRO    = 2200;   // intro lines ("WELCOME..") hold before fading out
const HOLD_QUESTION = 99999;  // questions hold until the user answers (Enter)
const FADE_OUT      = 1100;   // text fades out over this long
const GAP_BETWEEN   = 350;    // brief beat between one line fading out + the next fading in
const VOID_HOLD     = 1200;   // post-Q4: hold in the void before generation starts
const FINAL_FADE    = 1500;   // the screen's fade-to-black overlay duration

// ── The fixed question sequence ─────────────────────────────────────────
// The intro lines have no input. The question lines each collect one
// answer. The order + wording are Chloe's, verbatim — do not paraphrase.
const INTRO_LINES = [
  'WELCOME..',
  'I AM THE GAME MASTER..',
];

const QUESTIONS = [
  { key: 'character', text: 'Now tell me, what is your character like?' },
  { key: 'setting',   text: 'Nice to meet you.. Now where does this story take place?' },
  { key: 'plot',      text: 'Quite interesting indeed.. Is there a plot I should be aware of?' },
  { key: 'extra',     text: 'I see, I see.. One more thing, anything else I should know before sending you off?' },
];

// ── Module state ────────────────────────────────────────────────────────
let voidRoot = null;          // the screen element
let particles = null;         // particle system controller (destroyed on teardown)
let lineEl = null;            // .fable-void-line (the big centered text)
let answerEl = null;          // .fable-void-answer (the user's typed answer, persists)
let inputEl = null;           // the textarea (no send button)
let inputWrap = null;         // .fable-void-input-wrap (fades in/out with the question)
let errorEl = null;           // .fable-void-error (shown on generation failure)

let answers = { character: '', setting: '', plot: '', extra: '' };
let generating = false;       // true while interview_generate is running
let aborted = false;          // set by teardownVoid to break the await chain
let beginCallback = null;     // set by wireVoid, fired after successful generation
let listeners = [];           // [el, type, handler] for teardown (no double-bind)
let pendingSubmit = null;     // resolver for the current question's await (Enter fires it)

export function buildVoid() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-void-screen';
  root.dataset.fableScreen = 'void';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-particles" aria-hidden="true"></div>
    <div class="fable-void-stage">
      <p class="fable-void-line" data-void-line></p>
      <p class="fable-void-answer" data-void-answer></p>
      <div class="fable-void-input-wrap" data-void-input-wrap>
        <textarea class="fable-void-input" data-void-input rows="1"
                  placeholder="..." aria-label="Your answer"></textarea>
      </div>
      <p class="fable-void-error" data-void-error hidden></p>
    </div>
  `;
  return root;
}

// Bind input + IPC + start particles. Called on every void entry (the DOM
// is reused, so listeners go through on() which teardown removes).
export function wireVoid(root, hooks) {
  voidRoot = root;
  beginCallback = hooks.onBegin || null;

  lineEl = root.querySelector('[data-void-line]');
  answerEl = root.querySelector('[data-void-answer]');
  inputEl = root.querySelector('[data-void-input]');
  inputWrap = root.querySelector('[data-void-input-wrap]');
  errorEl = root.querySelector('[data-void-error]');

  // Fresh particle field per entry.
  const particleHost = root.querySelector('.fable-void-particles');
  if (particleHost) particles = createVoidParticles(particleHost);

  // Reset state on each entry (a new Quick Play starts fresh — the prior
  // interview is gone, by design).
  answers = { character: '', setting: '', plot: '', extra: '' };
  generating = false;
  aborted = false;
  pendingSubmit = null;
  if (lineEl) { lineEl.textContent = ''; lineEl.classList.remove('is-visible'); }
  if (answerEl) { answerEl.textContent = ''; answerEl.classList.remove('is-visible'); }
  if (inputEl) { inputEl.value = ''; inputEl.style.height = 'auto'; }
  if (inputWrap) inputWrap.classList.remove('is-visible');
  if (errorEl) { errorEl.hidden = true; errorEl.textContent = ''; }

  // Enter submits (Shift+Enter for newline). There is NO send button by
  // design — the user uses Enter only (per Chloe's directive).
  on(inputEl, 'keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      if (pendingSubmit) {
        const text = (inputEl.value || '').trim();
        if (!text) return;            // ignore empty submits
        pendingSubmit(text);
      }
    }
  });
  on(inputEl, 'input', () => autoGrow(inputEl));

  // Kick off the cinematic sequence. Not awaited — wireVoid returns
  // immediately so fable.js can finish wiring.
  runSequence().catch((e) => {
    console.error('[void] sequence threw', e);
  });

  // Focus the input lazily so the first question's input is ready.
  setTimeout(() => inputEl && inputEl.focus(), 60);
}

// ── The cinematic sequence ──────────────────────────────────────────────
//
// Each line goes: blank → set text → fade in (FADE_IN) → hold (HOLD_INTRO
// or until Enter) → fade out (FADE_OUT) → clear text. The input wrap
// fades in alongside each QUESTION line (not the intros) and fades out
// when the user submits (carrying their pinned answer into answerEl).
async function runSequence() {
  // Intro lines: no input, just fade in + hold + fade out.
  for (const text of INTRO_LINES) {
    if (aborted) return;
    await showLine(text, { withInput: false, holdMs: HOLD_INTRO });
    if (aborted) return;
    await wait(GAP_BETWEEN);
  }

  // Question lines: each fades in WITH its input, holds until Enter,
  // then fades out carrying the answer into the persistent answer slot.
  for (const q of QUESTIONS) {
    if (aborted) return;
    const answer = await askQuestion(q.text);
    if (aborted) return;
    answers[q.key] = answer;
    await wait(GAP_BETWEEN);
  }

  if (aborted) return;

  // Final phase: the UI has already faded out with the last question.
  // Hold the user in the void for a beat of suspension, then run the
  // single generation call. On error, restore the last question for retry.
  await wait(VOID_HOLD);
  if (aborted) return;

  await runGeneration();
}

// Show one line + (optionally) its input. Returns when the line has faded
// out again. For intro lines (withInput=false) the hold is fixed; for
// question lines the caller uses askQuestion which awaits Enter.
function showLine(text, { withInput, holdMs }) {
  return new Promise((resolve) => {
    if (!lineEl) { resolve(); return; }
    lineEl.textContent = text;
    // Force reflow so the CSS transition restarts cleanly.
    void lineEl.offsetWidth;
    lineEl.classList.add('is-visible');
    if (withInput && inputWrap) inputWrap.classList.add('is-visible');

    setTimeout(() => {
      lineEl.classList.remove('is-visible');
      if (inputWrap) inputWrap.classList.remove('is-visible');
      // After fade-out, clear the text so it doesn't linger invisibly.
      setTimeout(() => {
        if (lineEl) lineEl.textContent = '';
        resolve();
      }, FADE_OUT);
    }, holdMs);
  });
}

// Show a question line + its input, await Enter, pin the answer into the
// persistent answer slot, then fade the question + input out together.
function askQuestion(text) {
  return new Promise((resolve) => {
    if (!lineEl) { resolve(''); return; }

    // Render the question + slide the input in alongside it.
    lineEl.textContent = text;
    void lineEl.offsetWidth;
    lineEl.classList.add('is-visible');
    if (inputWrap) inputWrap.classList.add('is-visible');
    if (inputEl) { inputEl.value = ''; inputEl.style.height = 'auto'; }
    setTimeout(() => inputEl && inputEl.focus(), FADE_IN);

    // Arm the submit resolver. The keydown handler in wireVoid calls it.
    pendingSubmit = (text) => {
      pendingSubmit = null;

      // Pin the answer into the persistent answer slot BEFORE fading the
      // input out. This is the load-bearing fix for "your message
      // disappears when you enter": the answer is rendered legibly under
      // the question + stays visible through the question's fade-out.
      if (answerEl) {
        answerEl.textContent = text;
        void answerEl.offsetWidth;
        answerEl.classList.add('is-visible');
      }

      // Fade the question + input out together. The answer fades out WITH
      // them (it lives in the same stage) — the next question fades in
      // fresh. After fade-out, clear for the next iteration.
      lineEl.classList.remove('is-visible');
      if (inputWrap) inputWrap.classList.remove('is-visible');
      // Hold the answer visible a beat longer than the question so it
      // reads as "acknowledged" before the next question arrives.
      setTimeout(() => {
        if (answerEl) answerEl.classList.remove('is-visible');
        setTimeout(() => {
          if (lineEl) lineEl.textContent = '';
          if (answerEl) answerEl.textContent = '';
          resolve(text);
        }, FADE_OUT);
      }, FADE_OUT + 200);
    };
  });
}

// ── The single generation call + the exit handoff ───────────────────────

async function runGeneration() {
  if (generating) return;
  generating = true;
  if (errorEl) { errorEl.hidden = true; errorEl.textContent = ''; }

  let card = null;
  let worldSchema = null;
  let playerState = null;
  let genError = null;
  try {
    const result = await invokeGeneration(answers);
    card = result.card;
    worldSchema = result.world_schema;
    playerState = result.player_state;
  } catch (err) {
    genError = err;
  }

  generating = false;

  if (genError || !card) {
    // Restore the last question so the user can revise + re-trigger.
    // The generation is deterministic per-answers, so we re-ask the SAME
    // four questions is overkill — instead we surface the error inline
    // and let the user press Enter on the (restored) last question to
    // retry, OR edit any prior answer by typing here.
    if (errorEl) {
      errorEl.textContent = genError
        ? String((genError && genError.message) || genError)
        : 'The Game Master could not shape your world. Try again.';
      errorEl.hidden = false;
    }
    // Re-show the last question with the input so the user can re-trigger
    // generation by pressing Enter (their typed text becomes the new
    // "extra" answer; the prior three are preserved).
    const last = QUESTIONS[QUESTIONS.length - 1];
    const retry = await askQuestion(last.text);
    answers[last.key] = retry || answers[last.key];
    if (aborted) return;
    await wait(VOID_HOLD);
    if (aborted) return;
    // Recurse to re-run generation. Bounded by user patience (each retry
    // requires an explicit Enter); no infinite loop risk on the happy path.
    await runGeneration();
    return;
  }

  // Success: fade the whole screen to full black via the overlay, swap to
  // the stage invisibly at peak black, undim to reveal the spawned scene.
  await playExitOverlay(() => {
    try {
      beginCallback(card, worldSchema, playerState);
    } catch (e) {
      console.error('[void] onBegin threw', e);
    }
  });
}

// Stream interview_generate. Returns the parsed { card, world_schema,
// player_state }. The chunks are heartbeats (empty text) — the three
// blocks must NOT flicker as half-parsed XML in the UI; the void's hold
// IS the progress indicator.
function invokeGeneration(ans) {
  return new Promise((resolve, reject) => {
    const channel = new Channel();
    let resolved = false;
    channel.onmessage = (e) => {
      if (!e) return;
      if (e.type === 'chunk') {
        // Heartbeat — ignore. The void is already holding.
      } else if (e.type === 'error') {
        if (!resolved) {
          resolved = true;
          reject(new Error(e.message || 'generation failed'));
        }
      } else if (e.type === 'done') {
        if (!resolved) {
          resolved = true;
          if (e.card) {
            resolve({
              card: e.card,
              world_schema: e.world_schema,
              player_state: e.player_state,
            });
          } else {
            reject(new Error('no card in generation response'));
          }
        }
      }
    };
    invoke('interview_generate', {
      character: ans.character,
      setting: ans.setting,
      plot: ans.plot,
      extra: ans.extra,
      onEvent: channel,
    }).catch((err) => {
      if (!resolved) { resolved = true; reject(err); }
    });
  });
}

// The fade-to-black exit overlay. Fires onPeak() at peak black so fable.js
// can swap the screen to the stage invisibly, then undims to reveal it.
async function playExitOverlay(onPeak) {
  const overlay = document.createElement('div');
  overlay.className = 'fable-void-overlay';
  document.body.appendChild(overlay);
  void overlay.offsetWidth; // reflow
  overlay.classList.add('dimming');

  // At peak black, fire the handoff. fable.js swaps the screen to the
  // stage + wires it; the swap is invisible under the overlay.
  setTimeout(() => { try { onPeak(); } catch (e) { console.error('[void] onPeak threw', e); } }, FINAL_FADE);

  // Begin undimming slightly after peak so the stage is fully wired first.
  setTimeout(() => {
    overlay.classList.remove('dimming');
    overlay.classList.add('clearing');
  }, FINAL_FADE + 200);

  // Remove the overlay once it's fully clear.
  setTimeout(() => {
    if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
  }, FINAL_FADE + 200 + FINAL_FADE);
}

// ── Helpers ─────────────────────────────────────────────────────────────

function wait(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function autoGrow(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, 160) + 'px';
}

// Track a listener so teardownVoid removes it (the void DOM is reused, so
// raw addEventListener would double-bind on re-wireVoid).
function on(el, type, handler) {
  if (!el) return;
  el.addEventListener(type, handler);
  listeners.push([el, type, handler]);
}

// Tear down: cancel particles, abort any in-flight sequence, clear state,
// remove listeners. Called by fable.js on any exit from the void (the user
// began, OR the title's Exit fired mid-interview). The interview content
// is dropped here — nothing about it ever persists.
export function teardownVoid() {
  aborted = true;
  pendingSubmit = null;
  if (particles) { particles.destroy(); particles = null; }
  for (const [el, type, handler] of listeners) {
    el.removeEventListener(type, handler);
  }
  listeners = [];
  if (inputEl) { inputEl.value = ''; inputEl.style.height = 'auto'; }
  if (lineEl) { lineEl.textContent = ''; lineEl.classList.remove('is-visible'); }
  if (answerEl) { answerEl.textContent = ''; answerEl.classList.remove('is-visible'); }
  if (inputWrap) inputWrap.classList.remove('is-visible');
  if (errorEl) { errorEl.hidden = true; errorEl.textContent = ''; }
  answers = { character: '', setting: '', plot: '', extra: '' };
  generating = false;
  beginCallback = null;
  voidRoot = null;
  lineEl = null;
  answerEl = null;
  inputEl = null;
  inputWrap = null;
  errorEl = null;
}
