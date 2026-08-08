// =============================================================
// FOG INTRO — the simple fog gate on FABLE entry.
// (Rewritten 2026-08-03: plain opacity fade. No canvas, no sweep, no rAF.)
//
// One full-screen overlay. It fogs UP over 2s, STAYS fully foggy for 2s, then
// UNFOGS over 2s. The screen swap (OS desktop → Fable title) happens during
// the 2s hold, when the overlay is at 100% opacity — so the swap is invisible.
//
// Total: 6s. The whole thing is driven by a single CSS opacity transition on
// one element (GPU-composited), so there's no per-frame JS at all — JS just
// flips classes at the phase boundaries.
//
//   t=0s     Mount overlay at opacity 0, then add .fog-in → opacity ramps to 1
//            over 2s (the fog-up).
//   t=2s     Overlay is fully foggy. The 2s hold begins.
//   t=3s     onSwap fires — swap the underlying DOM here (mid-hold, fully
//            foggy, invisible).
//   t=4s     Add .fog-out → opacity ramps back to 0 over 2s (the unfog).
//   t=6s     transitionend → remove the overlay.
// =============================================================

import WIND_SRC from '../assets/wind.mp3';

// --- Timing (ms) — must match the CSS transitions in fable.css -----------
// TIMELINE (absolute from the click):
//   0–2s    FOG_UP_MS    fog-up    (opacity 0 → 1)
//   2–4s    HOLD_MS      hold      (opacity 1) — swap fires at 3s (SWAP_AT_MS)
//   4–6s    FOG_DOWN_MS  unfog     (opacity 1 → 0)
const FOG_UP_MS   = 2000;  // fog-up duration (0 → 2s)
const HOLD_MS     = 2000;  // hold duration (2 → 4s)
const FOG_DOWN_MS = 2000;  // unfog duration (4 → 6s)
// Absolute time (from the click) when the seamless scene swap fires — 3s,
// midway through the hold so the fog is rock-solid when the DOM swaps.
const SWAP_AT_MS  = FOG_UP_MS + 1000;   // 3000ms

// Wind volume — low, atmospheric.
const WIND_VOLUME = 0.15;

function reducedMotion() {
  return window.matchMedia &&
         window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

// Play wind.mp3 looped. Returns { stop(fadeMs) } that fades out + removes the
// node. Degrades gracefully if autoplay is blocked (silent fog).
function playWind() {
  const audio = document.createElement('audio');
  audio.src = WIND_SRC;
  audio.loop = true;
  audio.volume = WIND_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  document.body.appendChild(audio);

  let fadeTimer = null;
  let stopped = false;

  const stop = (fadeMs = FOG_DOWN_MS) => {
    if (stopped) return;
    stopped = true;
    if (fadeTimer) { clearInterval(fadeTimer); fadeTimer = null; }
    const startVol = audio.volume;
    const steps = 10;
    let i = 0;
    fadeTimer = setInterval(() => {
      i++;
      audio.volume = Math.max(0, startVol * (1 - i / steps));
      if (i >= steps) {
        clearInterval(fadeTimer);
        fadeTimer = null;
        try { audio.pause(); } catch (_) {}
        if (audio.parentNode) audio.parentNode.removeChild(audio);
      }
    }, fadeMs / steps);
  };

  const p = audio.play();
  if (p && typeof p.catch === 'function') {
    p.catch(() => { if (!stopped && audio.parentNode) audio.parentNode.removeChild(audio); });
  }
  return { node: audio, stop };
}

// =============================================================
// PUBLIC: play the fog intro.
//
//   onSwap: () => void   — called mid-hold (fully foggy). Swap the underlying
//                          DOM/UI right here (invisible).
//
// Returns { cancel(), done: Promise<void> }.
//   • done     resolves once the overlay fades out + is removed (~6s).
//   • cancel() tears the overlay + audio down immediately (closeFable's
//              EXIT-mid-fog path).
//
// The overlay is a standalone node on document.body, self-cleaned on
// transitionend — re-triggerable fresh each click.
// =============================================================
export function playFogIntro({ onSwap: onSwapArg } = {}) {
  const overlay = document.createElement('div');
  overlay.className = 'fable-fog-overlay';
  overlay.setAttribute('aria-hidden', 'true');
  document.body.appendChild(overlay);

  const wind = playWind();

  let cancelled = false;
  let swapFired = false;
  let onSwapCb = onSwapArg || null;
  let resolveDone = null;
  const done = new Promise((r) => { resolveDone = r; });
  let swapTimer = null;
  let outTimer = null;
  let safetyTimer = null;

  const cleanup = () => {
    if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
  };

  // The unfog is the LAST transition; transitionend on it means we're done.
  const onTransitionEnd = (e) => {
    if (e.target !== overlay) return;
    // Only react to the opacity property reaching 0 (the unfog finishing).
    // The fog-up transition also fires transitionend at opacity 1 — ignore it.
    if (parseFloat(getComputedStyle(overlay).opacity) > 0.1) return;
    overlay.removeEventListener('transitionend', onTransitionEnd);
    cleanup();
    resolveDone();
  };
  overlay.addEventListener('transitionend', onTransitionEnd);

  const fireSwap = () => {
    if (swapFired || cancelled) return;
    swapFired = true;
    try { if (onSwapCb) onSwapCb(); } catch (e) {
      console.error('[fable/fog] swap callback threw', e);
    }
  };

  // Safety net: if transitionend never fires, still resolve + clean up.
  safetyTimer = setTimeout(() => {
    if (!cancelled) {
      overlay.removeEventListener('transitionend', onTransitionEnd);
      cleanup();
      resolveDone();
    }
  }, FOG_UP_MS + HOLD_MS + FOG_DOWN_MS + 500);

  if (reducedMotion()) {
    // Reduced motion: skip the fades. Swap immediately, remove next tick.
    fireSwap();
    setTimeout(() => {
      if (cancelled) return;
      overlay.removeEventListener('transitionend', onTransitionEnd);
      cleanup();
      resolveDone();
    }, 16);
  } else {
    // Phase 1 — fog up: force a reflow so opacity:0 takes, then .fog-in ramps
    // it to 1 over FOG_UP_MS.
    void overlay.offsetWidth;
    overlay.classList.add('fog-in');

    // Phase 2 — hold: the overlay is fully foggy from t=FOG_UP_MS onward.
    // Fire the seamless swap at SWAP_AT_MS (3s, mid-hold — fog rock-solid).
    swapTimer = setTimeout(fireSwap, SWAP_AT_MS);

    // Phase 3 — unfog: at t = FOG_UP_MS + HOLD_MS (4s) start the fade back to
    // 0. transitionend fires cleanup at 6s when it hits 0.
    outTimer = setTimeout(() => {
      if (cancelled) return;
      overlay.classList.remove('fog-in');
      overlay.classList.add('fog-out');
    }, FOG_UP_MS + HOLD_MS);
  }

  const cancel = () => {
    if (cancelled) return;
    cancelled = true;
    if (swapTimer) { clearTimeout(swapTimer); swapTimer = null; }
    if (outTimer) { clearTimeout(outTimer); outTimer = null; }
    if (safetyTimer) { clearTimeout(safetyTimer); safetyTimer = null; }
    overlay.removeEventListener('transitionend', onTransitionEnd);
    wind.stop(400);
    cleanup();
    resolveDone();
  };

  // Cut the wind shortly after the fog finishes. The fog transition ends at
  // t = FOG_UP_MS + HOLD_MS + FOG_DOWN_MS (= 6s); a short 500ms fade started
  // then reaches silence at 6.5s total — a hair of tail so it doesn't hard-
  // stop the instant the overlay vanishes, but no longer lingering past the
  // fog. (The prior version chained wind.stop(FOG_DOWN_MS=2000) off done,
  // which kept fading until t=8s — 2s of wind noise after the fog was gone.)
  done.then(() => wind.stop(500));

  return { cancel, done };
}
