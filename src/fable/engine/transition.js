// =============================================================
// FABLE MAGICAL TRANSITION — the screen-change cinematic.
//
// Used by the title-screen "New Game" + "Quick Play" buttons to enter the
// simulation engine with a deliberate, atmospheric hand-off:
//   1. A magical chime fires (synthesized — no asset file).
//   2. A full-screen overlay dims the title to complete black over 2s.
//   3. At peak darkness (fully black), the scene swaps underneath.
//   4. The overlay undims back to transparent over 2s, revealing the new
//      scene.
//
// Total transition time: ~4s (2s out + 2s in). The scene swap happens in
// the MIDDLE, invisibly — the user experiences a clean crossfade through
// darkness, never seeing either screen tear or pop.
//
// PROMISE CONTRACT: `playMagicalTransition({ onMidpoint })` returns a
// Promise that resolves when the FULL transition (undim complete) finishes.
// The caller passes `onMidpoint` — a sync callback fired at peak darkness,
// right when the screen is fully black. This is where the screen swap
// happens (the dim hides it). The Promise is for callers that need to know
// the transition is fully done (e.g. to focus an input).
//
// AUDIO: a "magical button press" — a shimmer cascade. Three detuned sine
// partials (a shimmering chord) ring out while a higher sparkle voice
// arpeggiates above, all through a slow-opening lowpass filter for a
// "crystalline bloom" feel. Synthesized via Web Audio (no asset ships).
// =============================================================

// --- Audio --------------------------------------------------------

// Lazy AudioContext singleton (scoped to fable). Browsers require a user
// gesture to start audio — the title-button click that triggers this
// transition IS that gesture, so the chime is guaranteed to play.
let fableAudioCtx = null;
function getFableAudioCtx() {
  if (fableAudioCtx) return fableAudioCtx;
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return null;
  try {
    fableAudioCtx = new Ctx();
    return fableAudioCtx;
  } catch (e) {
    console.warn('[fable] AudioContext init failed', e);
    return null;
  }
}

// The magical-button chime. Designed to read as "a spell being cast" —
// NOT a sterile OS notification. The character comes from three choices:
//
// 1. TRIANGLE waves (not sine). Triangle carries odd harmonics → a glassy,
//    bell-like timbre. Pure sines sound electronic/clinical (the "Windows
//    ding" problem); triangle reads as a struck bell or crystal.
// 2. A RISING ARPEGGIO gesture. The notes ascend (C5 → E5 → G5 → C6 → E6),
//    staggered ~80ms apart — the classic "magic wand sweep upward." This
//    melodic motion is what distinguishes a spell from a status ping.
// 3. A SOFT PAD underneath. A quiet, slow-attack triangle sustained chord
//    (Cmaj) holds beneath the arpeggio, giving the sound body + length
//    without making it louder — a "glow" rather than a "hit."
//
// VOLUME: master bus at 0.55 (clearly audible, not blaring). Per-voice
// peaks scale with the master: pad ~0.022, arpeggio ~0.030, echo ~0.018.
// The triangle timbre carries the "magical" read at moderate volume.
function playMagicalChime() {
  const ctx = getFableAudioCtx();
  if (!ctx) return;
  const now = ctx.currentTime;

  // Master bus — the global volume cap. Every voice routes here.
  const master = ctx.createGain();
  master.gain.value = 0.55;
  master.connect(ctx.destination);

  // A gentle lowpass to soften the triangle's edge slightly (triangle is
  // brighter than sine; a touch of filtering keeps it bell-like, not harsh).
  // Fixed at 5000Hz — no sweep (the sweep was part of the "OS sound" feel).
  const tone = ctx.createBiquadFilter();
  tone.type = 'lowpass';
  tone.frequency.value = 5000;
  tone.Q.value = 0.5;
  tone.connect(master);

  // --- Soft pad: a quiet Cmaj chord (C4 + E4 + G4) with a slow attack.
  // This is the "glow" under the arpeggio — barely audible on its own but
  // it gives the chime body + length. Triangle waves, ~0.008 peak each,
  // 0.4s attack, 1.8s decay. No detune (a pure, steady chord).
  const pad = [
    { f: 261.63 },  // C4
    { f: 329.63 },  // E4
    { f: 392.00 },  // G4
  ];
  pad.forEach(({ f }) => {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'triangle';
    osc.frequency.value = f;
    gain.gain.setValueAtTime(0, now);
    gain.gain.linearRampToValueAtTime(0.022, now + 0.40);   // slow bloom in
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 1.8); // long fade
    osc.connect(gain).connect(tone);
    osc.start(now);
    osc.stop(now + 1.9);
  });

  // --- Rising arpeggio: the magic-wand gesture. C5 → E5 → G5 → C6 → E6,
  // staggered 80ms apart, each a short triangle bell with fast decay. The
  // ascending major arpeggio is the universal "sparkle ascending" motif.
  // Peaks slightly louder than the pad (0.010) so the gesture reads clearly
  // over the glow.
  const arp = [
    { f: 523.25, t: 0.10 },  // C5
    { f: 659.25, t: 0.18 },  // E5
    { f: 783.99, t: 0.26 },  // G5
    { f: 1046.50, t: 0.34 }, // C6
    { f: 1318.51, t: 0.42 }, // E6 (the top — the "sparkle" landing)
  ];
  arp.forEach(({ f, t }) => {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'triangle';
    osc.frequency.value = f;
    // Tiny random detune (±2 cents) so the arpeggio shimmers slightly
    // rather than sounding sample-exact — a hand-cast feel.
    osc.detune.value = (Math.random() - 0.5) * 4;
    gain.gain.setValueAtTime(0, now + t);
    gain.gain.linearRampToValueAtTime(0.030, now + t + 0.02);  // quick onset
    gain.gain.exponentialRampToValueAtTime(0.0001, now + t + 0.6); // bell decay
    osc.connect(gain).connect(tone);
    osc.start(now + t);
    osc.stop(now + t + 0.7);
  });

  // --- Echo of the top note: a single quiet E6 repeat at ~0.9s, the "ring
  // off" after the gesture. Gives the chime a tail without a long sustain.
  const echo = { f: 1318.51, t: 0.95 };
  {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'triangle';
    osc.frequency.value = echo.f;
    gain.gain.setValueAtTime(0, now + echo.t);
    gain.gain.linearRampToValueAtTime(0.018, now + echo.t + 0.03);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + echo.t + 0.8);
    osc.connect(gain).connect(tone);
    osc.start(now + echo.t);
    osc.stop(now + echo.t + 0.9);
  }
}

// --- The visual transition ---------------------------------------

const DIM_OUT_MS = 2000;  // title → black
const DIM_IN_MS = 2000;   // black → new scene

// Play the full magical transition. Returns a Promise that resolves when
// the undim completes (the new scene is fully visible). `onMidpoint` is
// called synchronously at peak darkness — this is where the caller swaps
// the screen (the overlay hides it). Mounts a single overlay element on
// document.body (z above the fable window), self-cleans on completion.
export function playMagicalTransition({ onMidpoint } = {}) {
  return new Promise((resolve) => {
    // Fire the chime first — it plays through the whole dim-out + into the
    // undim, so the user hears the "spell" land as the screen goes dark.
    try { playMagicalChime(); } catch (e) { /* autoplay blocked: silent */ }

    // Mount the overlay. One element, full-screen, opaque-black, starts
    // transparent. Mounted on document.body so it sits above #fable (z:4000)
    // + the boot fog (z:5000) — z:6000.
    const overlay = document.createElement('div');
    overlay.className = 'fable-magical-overlay';
    document.body.appendChild(overlay);

    // Force a layout frame so the initial opacity:0 takes effect before the
    // transition class is added (otherwise the transition can skip).
    void overlay.offsetWidth;

    // Phase 1: dim out (opacity 0 → 1 over DIM_OUT_MS).
    overlay.classList.add('dimming');

    // Phase 2: at peak darkness (overlay fully opaque), swap the scene.
    // This is the invisible hand-off — the overlay hides it. The 150ms gap
    // between this and Phase 3 below is a deliberate hold at full black: it
    // lets the swap settle + the chime's bloom peak land before the undim
    // starts, so the new scene reveals cleanly rather than mid-swap.
    setTimeout(() => {
      try { if (onMidpoint) onMidpoint(); } catch (e) {
        console.error('[fable] transition midpoint threw', e);
      }
    }, DIM_OUT_MS);

    // Phase 3: undim (opacity 1 → 0 over DIM_IN_MS), starting after the
    // 150ms hold. Total time from start: DIM_OUT_MS + 150 + DIM_IN_MS.
    setTimeout(() => {
      overlay.classList.remove('dimming');
      overlay.classList.add('clearing');
    }, DIM_OUT_MS + 150);

    // Phase 4: cleanup + resolve once the undim finishes.
    setTimeout(() => {
      if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
      resolve();
    }, DIM_OUT_MS + 150 + DIM_IN_MS);
  });
}
