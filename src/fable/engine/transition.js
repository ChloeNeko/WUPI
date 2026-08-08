// =============================================================
// FABLE MAGICAL TRANSITION — the screen-change cinematic.
//
// Used by the title-screen menu buttons to transition between screens with
// a deliberate, atmospheric hand-off:
//   1. A full-screen overlay dims the title to complete black over 2s.
//   2. At peak darkness (fully black), the scene swaps underneath. The
//      screen then HOLDS at full black for `blackHoldMs` (default 150ms).
//   3. The overlay undims back to transparent over 2s, revealing the new
//      scene.
//
// Total transition time: ~4s (2s out + hold + 2s in). The scene swap happens
// in the MIDDLE, invisibly — the user experiences a clean crossfade through
// darkness, never seeing either screen tear or pop.
//
// PROMISE CONTRACT: `playMagicalTransition({ onMidpoint, blackHoldMs })`
// returns a Promise that resolves when the FULL transition (undim complete)
// finishes. The caller passes `onMidpoint` — a sync callback fired at peak
// darkness, right when the screen is fully black. This is where the screen
// swap happens (the dim hides it). The Promise is for callers that need to
// know the transition is fully done (e.g. to focus an input).
//
// AUDIO: NO sound plays during the transition (Chloe 2026-08-03: "I don't
// want any sound effects playing during the transition — it's awful"). The
// midpoint used to fire a synthesized "magical chime" shimmer; it's gone.
// The authored button-press SFX (fableButtonSFX.mp3) still fires on the
// click itself (title.js playButtonSfx) — that's the press cue, separate
// from this transition. The New Game music + fire bed fade in on the reveal
// side (fable.js onNewGameClicked), not here.
// =============================================================

// --- The visual transition ---------------------------------------

const DIM_OUT_MS = 2000;  // title → black
const DIM_IN_MS = 2000;   // black → new scene

// --- The transition driver --------------------------------------

// Play the full magical transition. Returns a Promise that resolves when
// the undim completes (the new scene is fully visible). `onMidpoint` is
// called synchronously at peak darkness — this is where the caller swaps
// the screen (the overlay hides it). `blackHoldMs` extends the hold at
// complete black before the undim begins (default 150ms). Mounts a single
// overlay element on document.body (z above the fable window), self-cleans
// on completion. NO audio plays here — the transition is silent (the press
// SFX already fired on the click; the New Game music blooms on reveal).
export function playMagicalTransition({ onMidpoint, blackHoldMs = 150 } = {}) {
  return new Promise((resolve) => {
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
    // This is the invisible hand-off — the overlay hides it. NO audio fires
    // here (silent transition — see header). The blackHoldMs gap lets the
    // black settle before the undim reveals the new scene.
    setTimeout(() => {
      try { if (onMidpoint) onMidpoint(); } catch (e) {
        console.error('[fable] transition midpoint threw', e);
      }
    }, DIM_OUT_MS);

    // Phase 3: undim (opacity 1 → 0 over DIM_IN_MS), starting after the
    // blackHoldMs hold. Total time from start: DIM_OUT_MS + blackHoldMs + DIM_IN_MS.
    setTimeout(() => {
      overlay.classList.remove('dimming');
      overlay.classList.add('clearing');
    }, DIM_OUT_MS + blackHoldMs);

    // Phase 4: cleanup + resolve once the undim finishes.
    setTimeout(() => {
      if (overlay.parentNode) overlay.parentNode.removeChild(overlay);
      resolve();
    }, DIM_OUT_MS + blackHoldMs + DIM_IN_MS);
  });
}
