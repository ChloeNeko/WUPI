// =============================================================
// BOOT TRANSITION — the 2s-paused welcome that reveals Fable's title.
//
// ARCHITECTURE (fog-free, per Chloe 2026-07-26):
// The user clicks the Fable tile → Fable opens immediately (no fog, no
// wind, no parting clouds) → the title screen paints at full opacity but
// the MENU BUTTONS STAY HIDDEN for a deliberate 2-second beat (the title
// art + ambient scene get to breathe on their own). At t=2s the welcome
// arrives: theme music fades in, the magical ripple aura blooms on the
// Quick Play button, the ripple SFX plays, and the buttons cascade in
// (Quick Play → New Game → Load → Continue → Exit). Then the aura fades
// and the ripple SFX tails off.
//
// The prior fog-gate design (2s OS-layer fog buildup → handoff → HOLD →
// 5.5s cloud part → ripple) was removed wholesale: the fog node, the wind
// audio, the parting panels, the convert-in-place handoff, all gone. What
// remains is the ripple aura + button stagger, which were always the
// actual "welcome" — the fog was a separate preamble that's no longer
// wanted.
//
// TIMELINE (relative to playBootTransition being called = ~the click):
//   t=0.0s   Buttons hidden. Title screen + ambient visible, no menu.
//   t=2.0s   Music fade-in starts. Ripple Aura spawns on Quick Play.
//            Buttons stagger-reveal (Quick Play → … → Exit).
//   t=3.5s   Aura removed (2s + 1.5s life).
//   t=4.0s   Ripple SFX fades out.
//
// CANCEL: closeFable calls cancel() FIRST — removes the aura, stops the
// SFX, force-reveals the buttons. Idempotent + safe mid-sequence.
// REDUCED MOTION: short-circuits to the revealed end-state (music on,
// buttons shown).
// =============================================================

import { startThemeMusic } from './reveal.js';
// Ripple SFX as a bundled asset (Vite resolves the import to a hashed URL
// in assets/, NOT a publicDir file at the install root). See Issue 1.
import RIPPLE_SRC from '../assets/fable_ripple.mp3';

// --- Tunable timing (ms) --------------------------------------
// The deliberate pause between the click and the welcome. The title
// screen is visible (art + ambient) but the buttons stay hidden for this
// whole window — a beat of stillness before the magic arrives.
const REVEAL_DELAY_MS     = 2000;
// Buttons cascade deliberately so each settles before the next begins.
const BUTTON_STAGGER_MS   = 180;
const RIPPLE_AURA_LIFE_MS = 1500;
// Aura is removed REVEAL_DELAY_MS + RIPPLE_AURA_LIFE_MS after the click.
const AURA_REMOVE_MS      = REVEAL_DELAY_MS + RIPPLE_AURA_LIFE_MS;  // 3500
const RIPPLE_SFX_FADE_MS  = 400;

// --- SFX ------------------------------------------------------
// RIPPLE_SRC is imported as a bundled asset above (no publicDir copy).
const SFX_VOLUME  = 0.2;

// --- Helpers --------------------------------------------------
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function reducedMotion() {
  return window.matchMedia &&
         window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

// Play a LOOPED SFX <audio> at SFX_VOLUME. Returns { stop() } that
// fades the volume to 0 over fadeMs then pauses + removes the node.
function playLoopedSfx(src, { fadeMs = 200 } = {}) {
  const audio = document.createElement('audio');
  audio.src = src;
  audio.loop = true;
  audio.volume = SFX_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  document.body.appendChild(audio);

  let fadeTimer = null;
  let stopped = false;

  const stop = () => {
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
    p.catch(() => { if (audio.parentNode) audio.parentNode.removeChild(audio); });
  }
  return { node: audio, stop };
}

// Spawn the multi-element magical aura at a point (viewport coords).
function spawnRippleAura(titleScreen, x, y) {
  const aura = document.createElement('div');
  aura.className = 'fable-ripple-aura';
  aura.setAttribute('aria-hidden', 'true');
  aura.style.left = `${x}px`;
  aura.style.top  = `${y}px`;

  const bloom = document.createElement('div');
  bloom.className = 'fable-ripple-bloom';
  aura.appendChild(bloom);

  for (let i = 0; i < 3; i++) {
    const ring = document.createElement('div');
    ring.className = `fable-ripple-ring ring-${i + 1}`;
    aura.appendChild(ring);
  }

  for (let i = 0; i < 8; i++) {
    const spark = document.createElement('div');
    spark.className = 'fable-ripple-spark';
    spark.style.setProperty('--spark-pos', `rotate(${i * 45}deg) translateX(36px)`);
    spark.style.animationDelay = `${i * 35}ms`;
    aura.appendChild(spark);
  }

  titleScreen.appendChild(aura);
  return aura;
}

function centerViewport(el) {
  const r = el.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
}

// =============================================================
// PUBLIC: play the boot transition. Returns { cancel() }.
//
// No fog, no wind — those were removed in the 2026-07-26 fog-free
// rework. This now drives ONLY: hide buttons → 2s pause → music +
// ripple aura + button stagger → aura/SFX cleanup.
// =============================================================
export function playBootTransition({
  fableRoot,
  titleScreen,
  musicHost,
  rippleAnchorBtn,
  allButtons = [],
  revealOrder = [],
}) {
  let cancelled = false;
  let aura = null;
  let rippleSfx = null;
  const staggerTimers = [];
  const pendingTimers = [];

  const after = (ms, fn) => {
    const id = setTimeout(() => { if (cancelled) return; fn(); }, ms);
    pendingTimers.push(id);
    return id;
  };

  const hideButtons = () => {
    allButtons.forEach((b) => b && b.classList.add('fable-title-btn--hidden'));
  };
  const showButtonsImmediate = () => {
    allButtons.forEach((b) => b && b.classList.remove('fable-title-btn--hidden'));
  };

  const cancel = () => {
    cancelled = true;
    if (rippleSfx)  { rippleSfx.stop();  rippleSfx = null; }
    staggerTimers.forEach((t) => clearTimeout(t));
    staggerTimers.length = 0;
    pendingTimers.forEach((t) => clearTimeout(t));
    pendingTimers.length = 0;
    if (aura && aura.parentNode) aura.parentNode.removeChild(aura);
    aura = null;
    showButtonsImmediate();
  };

  // --- Reduced-motion fast path ----------------------------------
  if (reducedMotion()) {
    startThemeMusic(musicHost);
    showButtonsImmediate();
    return { cancel };
  }

  // --- Cinematic timeline ----------------------------------------
  (async () => {
    // Hide buttons first (idempotent — openFable already hid them, but a
    // bare launch path that skips openFable needs this). They stay hidden
    // through the REVEAL_DELAY pause so the title art has the screen to
    // itself.
    hideButtons();

    // The deliberate pause. Title screen + ambient are visible; the menu
    // is not. At expiry the welcome arrives: music + ripple + buttons.
    await sleep(REVEAL_DELAY_MS);
    if (cancelled) return;

    // Welcome. Music fades in alongside the ripple aura + button stagger —
    // one coordinated "the app is ready" moment.
    startThemeMusic(musicHost, { fadeIn: true });
    if (rippleAnchorBtn) {
      const { x, y } = centerViewport(rippleAnchorBtn);
      aura = spawnRippleAura(titleScreen, x, y);
    }
    rippleSfx = playLoopedSfx(RIPPLE_SRC, { fadeMs: RIPPLE_SFX_FADE_MS });
    // revealOrder entries may each be a single button OR an array of buttons
    // (a group revealed together at the same stagger beat). This lets the
    // reveal radiate outward from the ripple anchor: anchor first, then its
    // neighbors together, then the outermost together. A plain button entry
    // is treated as a one-element group (backward compatible).
    revealOrder.forEach((entry, i) => {
      const group = Array.isArray(entry) ? entry : [entry];
      staggerTimers.push(setTimeout(() => {
        group.forEach((b) => b && b.classList.remove('fable-title-btn--hidden'));
      }, i * BUTTON_STAGGER_MS));
    });

    await sleep(RIPPLE_AURA_LIFE_MS);
    if (cancelled) return;

    // Aura cleanup. Ripple SFX fades out 0.5s later.
    if (aura && aura.parentNode) aura.parentNode.removeChild(aura);
    aura = null;
    after(500, () => { if (rippleSfx) { rippleSfx.stop(); rippleSfx = null; } });
  })().catch((err) => {
    console.error('[fable/boot] transition threw, forcing reveal', err);
    cancel();
  });

  return { cancel };
}
