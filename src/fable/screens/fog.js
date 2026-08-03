// =============================================================
// FABLE FOG INTRO — the wind-blown fog gate on FABLE entry.
// (Re-added 2026-08-01; the prior fog intro was removed 2026-07-26.)
//
// When the user clicks the Fable tile, a full-screen fog overlay covers the
// app for a short hold while wind.mp3 loops at a LOW volume AND the fog
// texture drifts continuously (it looks alive, not a static slab). Then a
// SLOW SOFT LEFT→RIGHT FEATHERED WIPE reveals the title: the wipe line moves
// left→right with a WIDE blurred feather at its edge, so fog thins out into
// the menu — there is NO hard vertical cut line, and the banks keep drifting
// throughout so it genuinely reads as wind blowing the fog away.
//
// Why rAF instead of a CSS transition: the mask's reveal position must
// interpolate smoothly, and CSS can't transition an unregistered custom
// property's value reliably across engines. A tiny rAF loop rebuilding a
// linear-gradient mask each frame is cheap (one style write), robust
// everywhere, and gives precise eased control over the feathered edge.
//
// ARCHITECTURE (fixed 2026-08-02): the overlay is ONE element. The three
// .fable-fog-bank children are the drifting texture; the wipe mask is applied
// to the OVERLAY ITSELF, so the whole composite (base + banks) clears
// left→right together and the menu underneath shows through. The prior
// version drove the mask on a separate SOLID child div stacked above the
// banks — that hid the drifting banks during hold ("fog doesn't move") and
// only revealed the banks (not the menu) during the wipe, so the menu
// appeared in an instant snap when the node was removed. Driving the overlay's
// own mask fixes both.
//
// Patterns mirrored from siblings:
//   • wind.mp3 is a Vite-bundled asset import (same as fable_theme.mp3 /
//     fable_ripple.mp3 in reveal.js / boot.js).
//   • playLoopedSfx + its fade-out stop() are lifted from boot.js.
//   • The autoplay-gesture unlock pattern is lifted from reveal.js.
//   • The overlay is a standalone <div> on document.body, self-cleaned on
//     completion — same discipline as .fable-magical-overlay (transition.js).
//
// Prime Directive: the banks animate via background-position (compositor),
// the wipe rebuilds one mask string per frame (no layout thrash), and there
// is exactly one rAF loop. Honors prefers-reduced-motion (freeze drift +
// collapse the wipe to a near-instant fade).
// =============================================================

import WIND_SRC from '../assets/wind.mp3';

// --- Tunable timing (ms) --------------------------------------
const HOLD_MS  = 1400;  // fog fully covers the screen, wind looping, drifting
const CLEAR_MS = 2600;  // slow soft left→right feathered wipe reveals the menu
// Feather width as a fraction of the viewport width. Wide = softer edge, no
// visible vertical cut line. 0.45 means the gradient transition spans ~45% of
// the screen — a broad, diffuse edge that reads as fog thinning, not a wipe.
const FEATHER  = 0.45;

// Wind volume — LOWERED per Chloe (well under SFX_VOLUME 0.2 / MUSIC 0.3).
const WIND_VOLUME = 0.15;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function reducedMotion() {
  return window.matchMedia &&
         window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

// Play wind.mp3 looped at WIND_VOLUME. Returns { stop(fadeMs) } that fades
// the volume to 0 then pauses + removes the node. Mirrors boot.js's
// playLoopedSfx. Includes the reveal.js autoplay-gesture unlock pattern so a
// blocked autoplay degrades gracefully (silent fog) rather than throwing.
function playWind() {
  const audio = document.createElement('audio');
  audio.src = WIND_SRC;
  audio.loop = true;
  audio.volume = WIND_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  document.body.appendChild(audio);

  let fadeTimer = null;
  let stopped = false;

  const stop = (fadeMs = CLEAR_MS) => {
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
    // Autoplay blocked — strip the node silently. The fog still plays
    // visually; wind just won't be heard until a gesture unlocks audio
    // (acceptable degradation; we do NOT block the intro on it).
    p.catch(() => { if (!stopped && audio.parentNode) audio.parentNode.removeChild(audio); });
  }
  return { node: audio, stop };
}

// Build the fog overlay DOM: three parallax fog banks (continuous drift via
// CSS background-position animation). NO separate mask child — the wipe mask
// is applied to the overlay element itself (see clearFog), so the whole
// composite clears left→right together and the menu shows through.
function buildFogOverlay() {
  const overlay = document.createElement('div');
  overlay.className = 'fable-fog-overlay';
  overlay.setAttribute('aria-hidden', 'true');
  overlay.innerHTML = `
    <div class="fable-fog-bank fable-fog-bank--back"></div>
    <div class="fable-fog-bank fable-fog-bank--mid"></div>
    <div class="fable-fog-bank fable-fog-bank--front"></div>
  `;
  return overlay;
}

// Drive the feathered left→right wipe via rAF. reveal goes 0→1 (eased);
// each frame rebuilds the overlay's mask gradient so fog covers everything
// right of the wipe line with a WIDE blurred feather at the line. reveal=1
// means the menu is 100% visible (fully transparent mask) BEFORE the overlay
// is removed — no snap. Returns a Promise that resolves when the wipe
// completes.
function clearFog(overlay, durationMs) {
  return new Promise((resolve) => {
    // ease-out (quad): starts promptly so the menu begins emerging quickly,
    // then slows as the last wisps trail off — reads as wind dying down.
    // (A symmetric ease-in-out left the first ~700ms feeling like nothing was
    // happening; ease-out puts motion on screen immediately.)
    const easeOut = (t) => 1 - (1 - t) * (1 - t);
    // The reveal value drives the opaque stop's position. We overshoot to
    // (1 + FEATHER) so that at t=1 the opaque stop sits BEYOND the right edge
    // — the entire feather has cleared the viewport, the mask is fully
    // transparent everywhere, and the menu is 100% visible BEFORE the overlay
    // is removed. Without the overshoot, removing the node at t=1 would snap
    // away the last ~FEATHER of still-partially-opaque fog on the right edge.
    const REVEAL_MAX = 1 + FEATHER;
    const start = performance.now();
    let raf = 0;
    const setMask = (reveal) => {
      // Three-stop gradient. Left of the feather = transparent (menu shows),
      // the feather itself transitions transparent→opaque across FEATHER width
      // (wide + soft — NO hard vertical cut), right of the feather = opaque
      // (fog still covers). Once reveal passes 1.0, the opaque stop is off the
      // right edge and the whole viewport is in the transparent/clearing span.
      const featherStart = Math.max(0, reveal - FEATHER);
      const grad = `linear-gradient(to right, rgba(0,0,0,0) 0%, rgba(0,0,0,0) ${featherStart * 100}%, rgba(0,0,0,1) ${Math.min(100, reveal * 100)}%)`;
      overlay.style.webkitMaskImage = grad;
      overlay.style.maskImage = grad;
    };
    const tick = (now) => {
      const t = Math.min(1, (now - start) / durationMs);
      setMask(easeOut(t) * REVEAL_MAX);
      if (t < 1) {
        raf = requestAnimationFrame(tick);
      } else {
        resolve();
      }
    };
    raf = requestAnimationFrame(tick);
  });
}

// =============================================================
// PUBLIC: play the fog intro. Returns { cancel(), done: Promise<void> }.
// `done` resolves once the fog has cleared (so the caller can chain the
// boot transition). cancel() tears everything down immediately (used on
// teardown / rapid re-entry).
// =============================================================
export function playFogIntro() {
  const overlay = buildFogOverlay();
  document.body.appendChild(overlay);
  const wind = playWind();

  let cancelled = false;
  let resolveDone = null;
  const done = new Promise((r) => { resolveDone = r; });

  const cleanup = () => {
    if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
  };

  (async () => {
    // HOLD: fog fully covers the screen, wind looping, banks drifting.
    if (reducedMotion()) {
      await sleep(300);
    } else {
      await sleep(HOLD_MS);
    }
    if (cancelled) { resolveDone(); return; }
    // CLEAR: slow soft left→right feathered wipe. The wind fades over a slightly
    // longer window than the wipe so the audio tails off naturally as the last
    // fog wisps clear; the banks keep drifting throughout (CSS animation), so
    // it reads as continuous wind blowing the fog away, not a static slab.
    const wipeMs = reducedMotion() ? 250 : CLEAR_MS;
    wind.stop(wipeMs + 400);
    await clearFog(overlay, wipeMs);
    if (cancelled) { resolveDone(); return; }
    cleanup();
    resolveDone();
  })().catch((err) => {
    console.error('[fable/fog] intro threw, forcing cleanup', err);
    cleanup();
    resolveDone();
  });

  const cancel = () => {
    if (cancelled) return;
    cancelled = true;
    wind.stop(reducedMotion() ? 200 : 400);
    cleanup();
    resolveDone();
  };

  return { cancel, done };
}

