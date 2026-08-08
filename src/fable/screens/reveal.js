// =============================================================
// REVEAL — theme music for the Fable title screen.
//
// The fog intro was removed (Chloe 2026-07-23): it broke when mounted
// at body level. The Fable app now opens directly — the app shows, the
// title appears (no ignite animation after the scratch reset), and the
// theme music starts. No fog overlay, no SFX.
//
// This module is now JUST the music lifecycle. The <audio> element is
// created FRESH per Fable session and removed on teardown — never a
// module-level singleton, so it can't leak or double-up across
// exit/relaunch cycles. Every open starts clean.
// =============================================================

// Theme music as a bundled asset (Vite resolves the import to a hashed
// URL in assets/, NOT a publicDir file at the install root). See Issue 1.
import THEME_MUSIC_SRC from '../assets/fable_theme.mp3';

const MUSIC_ID = 'fable-theme-music';      // the <audio> element id

// ── Theme music ──────────────────────────────────────────────
// fable_theme.mp3 — bundled asset (Issue 1). 40% volume (history: 0.8 → 0.6
// → 0.3 per Chloe 2026-07-23, far too loud at the top of that range; raised
// 0.3 → 0.4 per Chloe 2026-08-03, "make the main menu music a bit louder"),
// looped. The <audio> element is created fresh per Fable session
// and removed on teardown — never a module-level singleton, so it can't
// leak or double-up across exit/relaunch cycles. Audio playback is
// subject to the autoplay gesture policy; if blocked, startThemeMusic
// retries on the first user interaction.
//
// opts.fadeIn (bool): ramp volume 0 → TARGET over ~1.5s instead of
// starting at full volume. Used by the boot transition so the music
// ignites alongside the cloud-part (Phase 2). Uses setInterval stepping
// (8 steps × ~190ms) on the plain <audio>.volume — avoids a Web Audio
// graph for what's a one-time ramp, matching the plain-<audio> pattern
// this module already uses everywhere else.
const MUSIC_VOLUME = 0.4;
const MUSIC_FADE_MS = 1500;
export function startThemeMusic(host, opts = {}) {
  if (!host) return;
  // Never stack a second <audio>. If one's already playing, leave it.
  if (host.querySelector('#' + MUSIC_ID)) return;
  const audio = document.createElement('audio');
  audio.id = MUSIC_ID;
  audio.src = THEME_MUSIC_SRC;
  audio.loop = true;
  // Fade-in path starts at 0 and ramps up; otherwise full volume.
  audio.volume = opts.fadeIn ? 0 : MUSIC_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  // play() returns a promise that rejects when autoplay is blocked;
  // catch it and attach a one-shot gesture listener to retry. This is
  // the standard HTML5 autoplay unlock pattern. The handler is stashed on
  // the host (host._wupiMusicUnlock) so stopThemeMusic can strip it even
  // if the user closes without ever gesturing — otherwise the listeners
  // would accumulate on the persistent host across open/close cycles
  // (the resource-isolation audit gap #4).
  host.appendChild(audio);
  // Fade-in ramp (opts.fadeIn). 8 linear steps to MUSIC_VOLUME. The
  // interval is cleared on completion; if the node is removed mid-ramp
  // (stopThemeMusic on close), the interval keeps firing harmlessly
  // against a detached node whose volume writes are no-ops. We also
  // stash the timer on the host so stopThemeMusic can clear it.
  const startFadeIn = () => {
    if (host._wupiMusicFade) clearInterval(host._wupiMusicFade);
    const steps = 8;
    let i = 0;
    host._wupiMusicFade = setInterval(() => {
      i++;
      // Guard: node may have been torn down by stopThemeMusic.
      if (!audio.parentNode) { clearInterval(host._wupiMusicFade); host._wupiMusicFade = null; return; }
      audio.volume = Math.min(MUSIC_VOLUME, MUSIC_VOLUME * (i / steps));
      if (i >= steps) { clearInterval(host._wupiMusicFade); host._wupiMusicFade = null; }
    }, MUSIC_FADE_MS / steps);
  };
  const p = audio.play();
  if (p && typeof p.catch === 'function') {
    p.then(() => { if (opts.fadeIn) startFadeIn(); }).catch(() => {
      // If a prior unlock handler is still attached (no gesture fired
      // before the last close), strip it first.
      if (host._wupiMusicUnlock) {
        host.removeEventListener('pointerdown', host._wupiMusicUnlock);
        host.removeEventListener('keydown', host._wupiMusicUnlock);
      }
      const unlock = () => {
        audio.play().then(() => { if (opts.fadeIn) startFadeIn(); }).catch(() => {});
        host.removeEventListener('pointerdown', unlock);
        host.removeEventListener('keydown', unlock);
        host._wupiMusicUnlock = null;
      };
      host._wupiMusicUnlock = unlock;
      host.addEventListener('pointerdown', unlock);
      host.addEventListener('keydown', unlock);
    });
  }
}

// Pause the theme music WITHOUT removing the node (the app-lifecycle onPause
// path — alt-tab/focus-loss freezes audio in place so onResume can continue
// from the exact spot). Idempotent: no node → no-op. Distinct from
// stopThemeMusic (the full teardown used on app close), which removes the
// node entirely. Resume is best-effort: play() can reject under autoplay
// policy, caught silently.
export function pauseThemeMusic(host) {
  if (!host) return;
  const audio = host.querySelector('#' + MUSIC_ID);
  if (audio) { try { audio.pause(); } catch (_) {} }
}

// Resume the theme music from where pauseThemeMusic froze it (the
// app-lifecycle onResume path — alt-tab back). Only resumes if a node
// already exists (onOpen is responsible for creating it). Does NOT
// auto-create: if the music was never started, there's nothing to resume.
// play() rejects under autoplay policy → caught silently (the unlock
// gesture listener from startThemeMusic still owns that case).
export function resumeThemeMusic(host) {
  if (!host) return;
  const audio = host.querySelector('#' + MUSIC_ID);
  if (audio) {
    const p = audio.play();
    if (p && typeof p.catch === 'function') p.catch(() => {});
  }
}

// Stop + remove the theme music. Pause first (so a quick reopen could
// reuse buffered state if we ever wanted), then detach from the DOM so
// no stale audio node accumulates across cycles. Also strips any pending
// autoplay-unlock gesture listeners (see startThemeMusic) AND the fade-in
// interval timer so they can't accumulate on the persistent host across
// open/close cycles. Idempotent.
export function stopThemeMusic(host) {
  if (!host) return;
  const audio = host.querySelector('#' + MUSIC_ID);
  if (audio) {
    try { audio.pause(); } catch (_) {}
    audio.remove();
  }
  if (host._wupiMusicUnlock) {
    host.removeEventListener('pointerdown', host._wupiMusicUnlock);
    host.removeEventListener('keydown', host._wupiMusicUnlock);
    host._wupiMusicUnlock = null;
  }
  if (host._wupiMusicFade) {
    clearInterval(host._wupiMusicFade);
    host._wupiMusicFade = null;
  }
}

// FADE OUT + remove the theme music over `ms` (default MUSIC_FADE_MS).
// Ramps the live node's volume to 0 (clearing any in-flight fade-in timer
// first so the two don't fight), then removes the node + state exactly like
// stopThemeMusic. Idempotent: no node → no-op.
//
// (Currently unused — New Game now hard-stops the theme at click time per
// Chloe. Kept as a utility: a future flow that wants a soft theme exit can
// reach for this instead of the hard stop. Delete if it stays unused.)
export function fadeOutThemeMusic(host, ms = MUSIC_FADE_MS) {
  if (!host) return;
  const audio = host.querySelector('#' + MUSIC_ID);
  if (!audio) return;
  // Clear any in-flight fade-in timer so it can't fight the fade-out.
  if (host._wupiMusicFade) {
    clearInterval(host._wupiMusicFade);
    host._wupiMusicFade = null;
  }
  const startVol = audio.volume;
  const steps = 8;
  let i = 0;
  host._wupiMusicFade = setInterval(() => {
    i++;
    if (!audio.parentNode) { clearInterval(host._wupiMusicFade); host._wupiMusicFade = null; return; }
    audio.volume = Math.max(0, startVol * (1 - i / steps));
    if (i >= steps) {
      clearInterval(host._wupiMusicFade);
      host._wupiMusicFade = null;
      // Full teardown now that the node is silent.
      try { audio.pause(); } catch (_) {}
      audio.remove();
      if (host._wupiMusicUnlock) {
        host.removeEventListener('pointerdown', host._wupiMusicUnlock);
        host.removeEventListener('keydown', host._wupiMusicUnlock);
        host._wupiMusicUnlock = null;
      }
    }
  }, ms / steps);
}

// (teardownTitleIgnite was removed in the scratch reset — the wordmark's
// .igniting class is no longer added anywhere, so there's nothing to clean
// up. The function lived here as a no-fog teardown helper for fable.js.)
