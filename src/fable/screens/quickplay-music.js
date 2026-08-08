// =============================================================
// QUICK PLAY MUSIC — the ominous single-track bed for the Quick Play
// void-form + drift phase.
//
// When the user enters Quick Play (Start New) the title theme has already
// faded out + this track fades in — QuickPlay.mp3 at volume 0.3, kicked off
// at the transition midpoint so the slow fade-in blooms while the void-form
// reveals. It loops for the whole void-form + the drift phase, then stops
// when the player enters the chat stage, goes back to the title, or exits
// Fable.
//
// This is a single-track sibling of newgame-music.js (which layers two
// tracks). The patterns are mirrored verbatim from newgame-music.js +
// reveal.js (the title-theme lifecycle):
//   • <audio> element is created fresh per Quick Play session and removed on
//     teardown — never a module-level singleton, so it can't leak or
//     double-up across re-entry. State is stashed on the host element.
//   • Audio playback is subject to the autoplay gesture policy; if blocked,
//     startQuickPlayMusic retries on the first user interaction. By the time
//     the user clicks "Quick Play" a gesture has already fired (the click),
//     so autoplay is unlocked in practice — the unlock path is a backstop.
//   • Fade-in/out use plain setInterval stepping on <audio>.volume — no Web
//     Audio graph for what's a one-time ramp (Prime Directive: cheapest path).
// =============================================================

import QUICKPLAY_SRC from '../assets/QuickPlay.mp3';

// The single Quick Play track sits at volume 0.3 (Chloe 2026-08-05). Lower
// than newgame.mp3 (0.2 melodic + 0.1 fire = ~0.3 effective) but as ONE
// track so the ominous bed reads clearly without a competing layer.
const QUICKPLAY_VOLUME = 0.3;
// Slow fade-in so the track blooms in as the void-form reveals (matches the
// New Game hand-off feel). Fade-out is snappier so backing out / entering
// the stage / exiting doesn't drag.
const FADE_IN_MS = 2500;
const FADE_OUT_MS = 1200;

const AUDIO_ID = 'fable-quickplay-music';
// The host-stashed state key (mirrors reveal.js / newgame-music.js).
const STATE_KEY = '_wupiQuickPlayMusic';

// Build a fresh <audio>. loop=true so it runs continuously for the whole
// void-form + drift. Volume starts at 0 when fading in (set by caller).
function makeTrack(host, id, src, startVolume) {
  const audio = document.createElement('audio');
  audio.id = id;
  audio.src = src;
  audio.loop = true;
  audio.volume = startVolume;
  audio.setAttribute('aria-hidden', 'true');
  host.appendChild(audio);
  return audio;
}

// Linear volume ramp via setInterval stepping. Mirrors newgame-music.js.
function fadeVolume(audio, from, to, ms, host) {
  const steps = 12;
  let i = 0;
  const timer = setInterval(() => {
    i++;
    // Guard: node may have been torn down by stopQuickPlayMusic mid-fade.
    if (!audio.parentNode) { clearInterval(timer); return; }
    const v = from + (to - from) * (i / steps);
    audio.volume = Math.max(0, Math.min(1, v));
    if (i >= steps) clearInterval(timer);
  }, ms / steps);
  return timer;
}

// ── Start: fade the track in from 0 → TARGET over FADE_IN_MS. ──
// Idempotent: if a track is already playing on this host, leave it.
export function startQuickPlayMusic(host, opts = {}) {
  if (!host) return;
  if (host[STATE_KEY]) return;  // already playing — never stack a second

  const fadeIn = opts.fadeIn !== false;  // default true

  const audio = makeTrack(host, AUDIO_ID, QUICKPLAY_SRC, fadeIn ? 0 : QUICKPLAY_VOLUME);

  const state = {
    audio,
    fadeTimers: [],
    unlock: null,     // autoplay-unlock gesture listener (if needed)
  };
  host[STATE_KEY] = state;

  // Kick off. If play() resolves, run the fade-in ramp. If autoplay blocks,
  // attach a one-shot gesture unlock (backstop — the Quick Play click
  // already unlocked audio in practice).
  let began = false;
  const begin = () => {
    if (began) return;
    began = true;
    if (fadeIn) state.fadeTimers.push(fadeVolume(audio, 0, QUICKPLAY_VOLUME, FADE_IN_MS, host));
  };
  const p = audio.play();
  const onBlocked = () => {
    if (state.unlock) return;  // already armed
    const unlock = () => {
      audio.play().then(begin).catch(() => {});
      host.removeEventListener('pointerdown', unlock);
      host.removeEventListener('keydown', unlock);
      state.unlock = null;
    };
    state.unlock = unlock;
    host.addEventListener('pointerdown', unlock);
    host.addEventListener('keydown', unlock);
  };
  if (p && typeof p.catch === 'function') p.then(begin).catch(onBlocked);
  else begin();
}

// ── Stop: fade out to 0 over FADE_OUT_MS, then remove the node. ──
// Returns immediately; the fade + removal is async. Idempotent. If
// `opts.immediate` is true, skip the fade and tear down now (used by
// closeFable on EXIT so there's no lingering fade after the app is gone).
export function stopQuickPlayMusic(host, opts = {}) {
  if (!host) return;
  const state = host[STATE_KEY];
  if (!state) return;

  // Strip the autoplay-unlock listener if one was armed.
  if (state.unlock) {
    host.removeEventListener('pointerdown', state.unlock);
    host.removeEventListener('keydown', state.unlock);
    state.unlock = null;
  }

  if (opts.immediate) {
    teardown(state);
    host[STATE_KEY] = null;
    return;
  }

  // Clear the state SYNCHRONOUSLY so a Quick Play re-entry during the
  // fade-out window sees no active track + can start fresh.
  host[STATE_KEY] = null;

  // Clear any in-flight fade-in timer so it doesn't fight the fade-out.
  state.fadeTimers.forEach((t) => clearInterval(t));
  state.fadeTimers = [];

  // Fade to 0, then remove the node after the ramp completes.
  if (state.audio.parentNode) {
    state.fadeTimers.push(fadeVolume(state.audio, state.audio.volume, 0, FADE_OUT_MS, host));
  }
  setTimeout(() => {
    state.fadeTimers.forEach((t) => clearInterval(t));
    try { state.audio.pause(); } catch (_) {}
    if (state.audio.parentNode) state.audio.remove();
  }, FADE_OUT_MS + 60);
}

// ── Pause/Resume (app-lifecycle focus loss). Mirrors newgame-music.js. ──
// Pauses in place (node stays mounted) so resume continues from the same
// spot. Idempotent.
export function pauseQuickPlayMusic(host) {
  if (!host) return;
  const state = host[STATE_KEY];
  if (!state) return;
  try { state.audio.pause(); } catch (_) {}
}
export function resumeQuickPlayMusic(host) {
  if (!host) return;
  const state = host[STATE_KEY];
  if (!state) return;
  const p = state.audio.play();
  if (p && typeof p.catch === 'function') p.catch(() => {});
}

// Remove the <audio> node + clear any timers. Pure teardown, no state flip
// (the caller nulls host[STATE_KEY]).
function teardown(state) {
  state.fadeTimers.forEach((t) => clearInterval(t));
  state.fadeTimers = [];
  try { state.audio.pause(); } catch (_) {}
  if (state.audio.parentNode) state.audio.remove();
  state.audio = null;
}
