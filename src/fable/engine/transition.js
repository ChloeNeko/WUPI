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
// AUDIO: the transition is visually-only. The button-press SFX
// (fableButtonSFX.mp3, played by the title click handler on every menu
// button press) accompanies the fade — it replaces the prior synthesized
// "magical chime." The chime + its AudioContext singleton were removed when
// the authored button SFX took over the sound-design role.
// =============================================================

// --- The visual transition ---------------------------------------

const DIM_OUT_MS = 2000;  // title → black
const DIM_IN_MS = 2000;   // black → new scene

// Play the full magical transition. Returns a Promise that resolves when
// the undim completes (the new scene is fully visible). `onMidpoint` is
// called synchronously at peak darkness — this is where the caller swaps
// the screen (the overlay hides it). `blackHoldMs` extends the hold at
// complete black before the undim begins (default 150ms). Mounts a single
// overlay element on document.body (z above the fable window), self-cleans
// on completion.
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
    // This is the invisible hand-off — the overlay hides it. The blackHoldMs
    // gap between this and Phase 3 below is a deliberate hold at full black:
    // it lets the swap settle + the chime's bloom peak land before the undim
    // starts, so the new scene reveals cleanly rather than mid-swap.
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

