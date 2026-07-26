// =============================================================
// FOG GATE — the OS-level fog buildup that precedes the Fable open.
//
// THE PROBLEM THIS SOLVES:
// The prior boot transition mounted fog INSIDE #fable after the app was
// already shown — so the desktop vanished instantly and the fog appeared
// already-full. Chloe wanted the opposite read: click the tile → the
// screen slowly FILLS with fog over 2 seconds (while the wind noise
// fades in) → THEN Fable appears underneath and the fog parts to reveal
// it. That requires the fog to live in the OS layer (over the desktop),
// NOT inside #fable.
//
// THE HANDOFF (fog stays at body for its ENTIRE lifetime — never reparented):
//   1. Click the Fable tile → fogGate.open() mounts a full-screen fog
//      node at document.body (z:5000, above everything including #fable).
//      Wind noise starts immediately (inside the click gesture) and fades
//      in over ~800ms.
//   2. Over 2 seconds the fog opacity ramps 0 → 1 (CSS animation). The
//      desktop is slowly obscured.
//   3. At 2s, fogGate calls onReady() → the caller opens Fable (which is
//      now hidden underneath the fog). The fog node STAYS at document.body
//      — it is NEVER adopted into #fable, not at handoff and not at Phase 2.
//      This is the handoff-seamlessness invariant: reparenting the fog
//      (even with identical backgrounds + coincident boxes) always produced
//      a visible discontinuity, because the node's containing block changed
//      from the viewport (position:fixed) to #fable (its own stacking
//      context + #050810 background), forcing a layout recompute that read
//      as "the fog is a bit different." Keeping the fog at body z:5000 for
//      its whole lifetime eliminates the reparent entirely.
//   4. boot.js takes over: holds the fog at body for HOLD_MS (600ms, long
//      enough for #fable's 420ms fade to finish), THEN converts it in place
//      (class swap + panel injection, position:fixed pinned inline so the
//      .fable-boot-fog class's position:absolute doesn't change the
//      containing block) + parts it at Phase 2. The parting panels reveal
//      the fully-opaque #fable through the gap (z:5000 fog > z:4000 #fable).
//      Then the ripple aura + button stagger.
//
// RELAUNCH: every click calls fogGate.open() fresh — a new fog node, a
// new wind audio node, a new 2s buildup. closeFable → boot.cancel()
// cleans up any in-flight fog; a subsequent click starts clean.
//
// CANCEL: if the user navigates away mid-buildup (Esc, etc.), cancel()
// removes the fog + stops the wind. Idempotent.
// =============================================================

const FOG_GATE_ID = 'fable-fog-gate';
const BUILDUP_MS = 2000;
const WIND_FADE_IN_MS = 800;
const WIND_VOLUME = 0.2;
const WHOOSH_SRC = '/fable_whoosh.mp3';

let activeGate = null;

// Open the fog gate. Returns a handle with { cancel(), fogNode }.
// onReady fires after BUILDUP_MS — the caller should open Fable there.
export function openFogGate(onReady) {
  // If a gate is already active (double-click), just return it.
  if (activeGate) return activeGate;

  let cancelled = false;
  let readyFired = false;
  let wind = null;
  let readyTimer = null;

  // Build the fog node. Single full-screen div (NOT two panels yet — the
  // buildup is a uniform fill; the two-panel split happens at handoff when
  // boot.js takes over). Mount WITHOUT the .building class first, force a
  // reflow so the browser commits opacity:0 as the initial state, THEN add
  // .building to trigger the animation. Without the reflow the animation
  // silently no-ops (the browser sees opacity:0 → opacity:1 in the same
  // frame commit and skips the transition — the same gotcha that plagued
  // the part animation earlier).
  const fog = document.createElement('div');
  fog.className = 'fable-fog-gate';
  fog.id = FOG_GATE_ID;
  fog.setAttribute('aria-hidden', 'true');
  document.body.appendChild(fog);
  // Force reflow: read offsetHeight to commit the initial computed style.
  void fog.offsetWidth;
  // NOW trigger the buildup animation.
  fog.classList.add('building');

  // Wind noise starts NOW (inside the click gesture — autoplay-safe) and
  // fades in from 0 to WIND_VOLUME over WIND_FADE_IN_MS.
  const audio = document.createElement('audio');
  audio.src = WHOOSH_SRC;
  audio.loop = true;
  audio.volume = 0;
  audio.setAttribute('aria-hidden', 'true');
  document.body.appendChild(audio);
  const p = audio.play();
  if (p && typeof p.catch === 'function') {
    p.catch(() => { /* autoplay blocked: wind is best-effort */ });
  }
  // Fade in the wind volume.
  let step = 0;
  const fadeSteps = 8;
  const fadeInterval = WIND_FADE_IN_MS / fadeSteps;
  const fadeTimer = setInterval(() => {
    step++;
    audio.volume = Math.min(WIND_VOLUME, WIND_VOLUME * (step / fadeSteps));
    if (step >= fadeSteps) clearInterval(fadeTimer);
  }, fadeInterval);
  wind = {
    node: audio,
    stop: (fadeOutMs = 250) => {
      clearInterval(fadeTimer);
      let s = 0;
      const outSteps = 10;
      const startVol = audio.volume;
      const outTimer = setInterval(() => {
        s++;
        audio.volume = Math.max(0, startVol * (1 - s / outSteps));
        if (s >= outSteps) {
          clearInterval(outTimer);
          try { audio.pause(); } catch (_) {}
          if (audio.parentNode) audio.parentNode.removeChild(audio);
        }
      }, fadeOutMs / outSteps);
    },
  };

  // After the buildup, fire onReady. The caller opens Fable but does NOT
  // adopt the fog — the node stays at document.body (z:5000) on top of
  // the fading-in #fable. boot.js reparents it into #fable at Phase 2
  // (see boot.js's flicker-fix comment for why the reparent is deferred).
  // CRITICAL: clear activeGate when onReady fires, NOT only in cancel(). If
  // it stays set, the next openFogGate() call returns the stale gate and the
  // app can't be reopened after EXIT — the "nothing happens at all" bug.
  readyTimer = setTimeout(() => {
    if (cancelled) return;
    readyFired = true;
    activeGate = null;  // release the gate slot so the next launch starts fresh
    if (typeof onReady === 'function') onReady(fog, wind);
  }, BUILDUP_MS);

  const cancel = () => {
    if (cancelled) return;
    cancelled = true;
    clearTimeout(readyTimer);
    clearInterval(fadeTimer);
    if (wind) { wind.stop(); wind = null; }
    if (fog.parentNode) fog.parentNode.removeChild(fog);
    activeGate = null;
  };

  activeGate = {
    fogNode: fog,
    wind,
    cancel,
    isReady: () => readyFired,
  };
  return activeGate;
}

// Is a fog gate currently active? (Used to prevent double-opens.)
export function isFogGateActive() {
  return activeGate !== null;
}
