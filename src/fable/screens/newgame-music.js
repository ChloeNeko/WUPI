// =============================================================
// NEW GAME MUSIC — the dual-track ambience for the New Game flow.
//
// When the user clicks "New Game" the title theme fades out and these TWO
// tracks fade in together — KICKED OFF at the transition midpoint so the
// slow 3s fade-in blooms while the screen undims, and the pair is partly
// audible as the New Game scene reveals. They play at the exact same time:
//   • newgame.mp3 — the melodic/atmospheric bed (0.2).
//   • fire.mp3    — the crackling-fire layer that sits under it (0.1).
// Each has its OWN target volume so the fire bed doesn't compete with the
// melodic track. Both loop for the whole New Game flow (the split screen +
// the card picker + the Creator), then stop when the player enters a game,
// goes back to the title, or exits Fable.
//
// Patterns mirrored from reveal.js (the title-theme lifecycle):
//   • <audio> elements are created fresh per New Game session and removed on
//     teardown — never module-level singletons, so they can't leak or
//     double-up across re-entry. State is stashed on the host element.
//   • Audio playback is subject to the autoplay gesture policy; if blocked,
//     startNewGameMusic retries on the first user interaction. By the time the
//     user clicks "New Game" a gesture has already fired (the click itself),
//     so autoplay is unlocked in practice — the unlock path is a backstop.
//   • Fade-in/out use plain setInterval stepping on <audio>.volume — no Web
//     Audio graph for what's a one-time ramp (Prime Directive: cheapest path).
// =============================================================

import NEWGAME_SRC from '../assets/newgame.mp3';
import FIRE_SRC from '../assets/fire.mp3';

// Each track has its OWN target volume (they layer together but sit at
// different levels so the fire bed doesn't compete with the melodic track).
const NEWGAME_VOLUME = 0.2;
const FIRE_VOLUME = 0.1;
// Fade-IN is longer than fade-OUT (Chloe 2026-08-03: "add more of a fade in
// for the audio"). A slow 3s ramp lets the fire + music bloom gradually as
// the New Game scene reveals, instead of the prior 1.5s that came in too
// hot. Fade-out stays at the snappier 1.5s so backing out / exiting the
// flow doesn't drag.
const FADE_IN_MS = 3000;
const FADE_OUT_MS = 1500;

const NEWGAME_ID = 'fable-newgame-music';
const FIRE_ID = 'fable-newgame-fire';
// The host-stashed state key (mirrors reveal.js's _wupiMusic* convention).
const STATE_KEY = '_wupiNewGameMusic';

// Build a fresh <audio> for one track. loop=true so both run continuously for
// the whole New Game flow. Volume starts at 0 when fading in (set by caller).
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

// Linear volume ramp via setInterval stepping. Mirrors reveal.js's fade
// pattern. Returns { cancel() } that clears the interval — call when the node
// is torn down mid-ramp so writes stop hitting a detached node.
function fadeVolume(audio, from, to, ms, host) {
  const steps = 12;
  let i = 0;
  const timer = setInterval(() => {
    i++;
    // Guard: node may have been torn down by stopNewGameMusic mid-fade.
    if (!audio.parentNode) { clearInterval(timer); return; }
    const v = from + (to - from) * (i / steps);
    audio.volume = Math.max(0, Math.min(1, v));
    if (i >= steps) clearInterval(timer);
  }, ms / steps);
  // Stash so stopNewGameMusic can clear it (mirrors reveal.js's _wupiMusicFade).
  return timer;
}

// ── Start: fade both tracks in together from 0 → TARGET over FADE_IN_MS. ──
// Idempotent: if a pair is already playing on this host, leave it (no double).
// The caller (fable.js onNewGameClicked) hard-stops the theme at click time
// and fires this at the transition midpoint so the tracks bloom in as the
// screen reveals.
export function startNewGameMusic(host, opts = {}) {
  if (!host) return;
  if (host[STATE_KEY]) return;  // already playing — never stack a second pair

  const fadeIn = opts.fadeIn !== false;  // default true

  // Each track has its OWN target volume. They start at 0 (fade-in) or at
  // their target (no fade), per the fadeIn flag.
  const ng = makeTrack(host, NEWGAME_ID, NEWGAME_SRC, fadeIn ? 0 : NEWGAME_VOLUME);
  const fire = makeTrack(host, FIRE_ID, FIRE_SRC, fadeIn ? 0 : FIRE_VOLUME);

  // State tracked on the host so teardown can find + clear everything. Each
  // track is paired with its target volume so stopNewGameMusic's fade-out
  // reads the right level per track.
  const state = {
    tracks: [ng, fire],
    targets: new Map([[ng, NEWGAME_VOLUME], [fire, FIRE_VOLUME]]),
    fadeTimers: [],
    unlock: null,     // autoplay-unlock gesture listener (if needed)
  };
  host[STATE_KEY] = state;

  // Kick off both tracks together. If play() resolves, run the fade-in ramp
  // on BOTH (each toward its own target). If autoplay blocks, attach a
  // one-shot gesture unlock (backstop — the New Game click already unlocked
  // audio in practice). `began` guards so the fade ramp only starts once even
  // if both promises resolve + an unlock fires.
  let began = false;
  const begin = () => {
    if (began) return;
    began = true;
    if (fadeIn) {
      state.fadeTimers.push(fadeVolume(ng, 0, NEWGAME_VOLUME, FADE_IN_MS, host));
      state.fadeTimers.push(fadeVolume(fire, 0, FIRE_VOLUME, FADE_IN_MS, host));
    }
  };
  // Start both; play() on each is independent but they begin within the same
  // tick (sub-millisecond apart) — "played at the exact same time".
  const p1 = ng.play();
  const p2 = fire.play();
  const onBlocked = () => {
    if (state.unlock) return;  // already armed
    const unlock = () => {
      ng.play().catch(() => {});
      fire.play().then(begin).catch(() => {});
      host.removeEventListener('pointerdown', unlock);
      host.removeEventListener('keydown', unlock);
      state.unlock = null;
    };
    state.unlock = unlock;
    host.addEventListener('pointerdown', unlock);
    host.addEventListener('keydown', unlock);
  };
  if (p1 && typeof p1.catch === 'function') p1.then(begin).catch(onBlocked);
  else begin();
  if (p2 && typeof p2.catch === 'function') p2.then(begin).catch(onBlocked);
  else begin();
}

// ── Stop: fade both out to 0 over FADE_OUT_MS, then remove the nodes. ──
// Returns immediately; the fade + removal is async. Idempotent: no state →
// no-op. If `opts.immediate` is true, skip the fade and tear down now (used
// by closeFable on EXIT so there's no lingering fade after the app is gone).
export function stopNewGameMusic(host, opts = {}) {
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

  // Clear the state SYNCHRONOUSLY so a New Game re-entry during the fade-out
  // window (host[STATE_KEY] guard in startNewGameMusic) sees no active pair and
  // can start fresh — otherwise the stale guard would make re-entry silently
  // no-op. The fade timers + node removal run independently below; this just
  // releases the "I own the audio" lock right away.
  host[STATE_KEY] = null;

  // Clear any in-flight fade-in timers so they don't fight the fade-out.
  state.fadeTimers.forEach((t) => clearInterval(t));
  state.fadeTimers = [];

  // Fade both to 0 in lockstep (identical ramp), then remove the nodes after
  // the ramp completes. One trailing timer (not per-track): both ramps share
  // the same FADE_OUT_MS so they reach 0 together.
  state.tracks.forEach((audio) => {
    if (audio.parentNode) {
      state.fadeTimers.push(fadeVolume(audio, audio.volume, 0, FADE_OUT_MS, host));
    }
  });
  setTimeout(() => {
    state.fadeTimers.forEach((t) => clearInterval(t));
    state.tracks.forEach((a) => {
      try { a.pause(); } catch (_) {}
      if (a.parentNode) a.remove();
    });
  }, FADE_OUT_MS + 60);
}

// ── Pause/Resume (app-lifecycle focus loss). Mirrors reveal.js. ──
// Pauses both in place (node stays mounted) so resume continues from the same
// spot. Idempotent.
export function pauseNewGameMusic(host) {
  if (!host) return;
  const state = host[STATE_KEY];
  if (!state) return;
  state.tracks.forEach((a) => { try { a.pause(); } catch (_) {} });
}
export function resumeNewGameMusic(host) {
  if (!host) return;
  const state = host[STATE_KEY];
  if (!state) return;
  state.tracks.forEach((a) => {
    const p = a.play();
    if (p && typeof p.catch === 'function') p.catch(() => {});
  });
}

// Remove the <audio> nodes + clear any timers. Pure teardown, no state flip
// (the caller nulls host[STATE_KEY]).
function teardown(state) {
  state.fadeTimers.forEach((t) => clearInterval(t));
  state.fadeTimers = [];
  state.tracks.forEach((a) => {
    try { a.pause(); } catch (_) {}
    if (a.parentNode) a.remove();
  });
  state.tracks = [];
}
