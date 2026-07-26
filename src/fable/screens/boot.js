// =============================================================
// BOOT TRANSITION — the cloud-part + magical ripple that reveals Fable.
//
// ARCHITECTURE (the "seamless handoff"):
// The fog buildup happens in the OS layer (fog-gate.js): the user clicks
// the Fable tile → fog slowly fills the screen over 2s while wind noise
// fades in → then Fable opens underneath + the fog node is handed to
// THIS module.
//
// THE FLICKER FIX (why the fog stays at body level through HOLD):
// The fog node is a `position:fixed; z-index:5000` element on
// document.body — it lives ABOVE #fable (z:4000), which is what makes it
// visually obscure the desktop during the gate. #fable itself has
// `opacity:0 + transition:opacity 420ms ease`, so it is FADING IN for
// ~420ms after launchApp. If we reparented the fog into #fable at handoff
// (as the prior version did), three things stacked into a one-frame
// flicker: (1) the reparent changed the fog's stacking context from the
// root (z:5000) into #fable (z:4000, still at partial opacity) → a
// visible dip; (2) the fog's `position:fixed` identity was lost on
// reparent into #fable → a re-layout flash; (3) the forced reflow in
// convertToPartingFog ran AFTER the reparent, too late to coalesce the
// repaint. The fix keeps the fog AT document.body for the entire HOLD
// period (where its z:5000 + position:fixed identity is intact and #fable
// is fading in invisibly underneath it), then reparents + converts +
// triggers the part in ONE frame at Phase 2 — by which point #fable is
// fully opaque and the fog is about to recede, so any reparent side-effect
// is masked by the part animation starting on the very same paint.
//
// This module receives the already-full-opacity fog node + the already-
// playing wind handle, and drives ONLY:
//   1. HOLD (600ms): fog sits at body level at full opacity while #fable
//      completes its fade-in underneath. Wind keeps blowing.
//   2. The cloud part (5.5s): convert the gate fog in place (NO reparent —
//      it stays at body z:5000 for its whole lifetime) to two panels,
//      recede. Music fade-in starts here. Wind fades out BEFORE the fog
//      is fully gone so there's no audible linger.
//   3. The ripple aura + button stagger, ~1.25s after the wind stops.
//
// TIMELINE (relative to handoff = t=0, which is 2s after the tile click):
//   t=0.0s   HOLD — fog at body z:5000, #fable fading in underneath.
//   t=0.6s   Phase 2 — fog converted in place (stays at body) + parting (5.5s).
//            Music fade-in starts. Container background clears on .parting.
//   t=5.75s  wind starts fading (250ms fade → silent by t=6.0s).
//   t=6.1s   fog nodes removed (part done; wind already silent).
//   t=7.0s   Phase 3 — Ripple Aura spawns on Quick Play; buttons stagger-reveal
//            (~1.25s after wind dies — a deliberate beat of silence).
//   t≈8.5s   aura removed; ripple SFX fades out.
//
// CANCEL: closeFable calls cancel() FIRST — removes fog + aura, stops
// audio, force-reveals buttons. Idempotent + safe mid-sequence.
// REDUCED MOTION: short-circuits to revealed end-state.
// =============================================================

import { startThemeMusic } from './reveal.js';

// --- Tunable timing (ms) --------------------------------------
// All times are relative to handoff (t=0 = when boot receives the fog
// node from the gate, which is 2s after the tile click).
const HOLD_MS             = 600;   // fog sits at full opacity to let #fable finish its 420ms
                                    // opacity transition UNDER the fog before we convert it
                                    // (was 1000 — tightened because the only thing HOLD is
                                    // buying us now is the #fable fade, which is 420ms).
const PART_DURATION_MS    = 5500;  // CSS animation on the panels (slowed from 4.5s
                                    // per Chloe 2026-07-25 — the part now reads as
                                    // a slow, deliberate reveal rather than a quick
                                    // split). MUST match the duration in the CSS
                                    // .fable-boot-fog.parting animation rule.
const WIND_FADE_MS        = 250;   // wind volume fade-out duration (must be declared
                                    // before WIND_STOP_MS, which references it)
// Fog removal is tied to the PART completing (HOLD + PART_DURATION), NOT to
// the ripple-aura lifetime. The actual visible fog ends at HOLD+PART_DURATION;
// remove the node then. The aura is its own leaf node and is cleaned up
// separately.
const FOG_REMOVE_MS       = HOLD_MS + PART_DURATION_MS;            // ≈6100
// Wind stops EARLY ENOUGH that its 250ms fade completes BEFORE the fog is
// gone — was stopping at PART_END-150, but the fade ran past fog removal,
// so the wind audibly lingered ~0.5s after the visual was already clear
// (Chloe 2026-07-25). Now WIND_STOP_MS accounts for the fade: wind must
// START fading at FOG_REMOVE - WIND_FADE_MS - 100ms lead, so it's silent
// ~100ms BEFORE the last panel frame leaves the screen (the lead ensures
// the ear registers silence as the visual clears, not after).
const WIND_STOP_MS        = FOG_REMOVE_MS - WIND_FADE_MS - 100;    // ≈5750
// Ripple fires ~1s AFTER wind stops. Chloe wants a deliberate beat of
// silence between the wind dying and the magical ripple blooming — gives
// the part room to breathe before the button reveal begins.
const RIPPLE_DELAY_MS     = WIND_STOP_MS + 1250;                   // ≈7000
// Buttons cascade more deliberately (was 100ms) so each one settles before
// the next begins, reading as a considered reveal rather than a quick pop.
const BUTTON_STAGGER_MS   = 180;
const RIPPLE_AURA_LIFE_MS = 1500;
const AURA_REMOVE_MS      = RIPPLE_DELAY_MS + RIPPLE_AURA_LIFE_MS; // ≈8500
const RIPPLE_SFX_FADE_MS  = 400;

// --- SFX ------------------------------------------------------
const RIPPLE_SRC  = '/fable_ripple.mp3';
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

// Convert the gate's single fog node into the two-panel parting structure
// IN PLACE — no reparent. This is the handoff-seamlessness fix: the gate
// fog stays at document.body (z:5000, position:fixed) for its ENTIRE
// lifetime. Reparenting it into #fable (as prior versions did) always
// produced a visible discontinuity — the node's containing block changed
// from the viewport (fixed) to #fable (which has its own stacking context
// + a #050810 background), and even though the boxes were coincident the
// reparent forced a layout recompute that read as "the fog is a bit
// different." By converting in place (just swap the class + inject the
// panel children, never move the node), the fog the user watched fill the
// OS is pixel-identically the fog that parts — no containing-block change,
// no recompute, no gap.
//
// Why this works visually: the gate fog is z:5000, ABOVE #fable (z:4000).
// By Phase 2 (after HOLD), #fable has finished its 420ms opacity fade and
// is fully opaque underneath. As the two panels slide apart, the fully-
// opaque #fable shows through the gap — the title is revealed. The fog
// never needs to be inside #fable for this; it just needs to be ON TOP of
// it, which z:5000 guarantees.
//
// The .fable-fog-gate → .fable-boot-fog class swap changes position from
// `fixed` to... actually we KEEP it fixed (see fable.css: .fable-boot-fog
// is position:absolute, but since the node stays at body level we override
// to fixed inline so the containing block doesn't change either). The
// inline position:fixed + the matching inset:0 from the class keep the
// box exactly where it was.
function convertToPartingFog(gateFog) {
  // Kill any residual buildup animation + lock full opacity. The gate fog
  // may still be mid-animation if the handoff fired slightly early; this
  // guarantees a solid full-opacity base before the panels render.
  gateFog.style.animation = 'none';
  gateFog.style.opacity = '1';
  // Keep the node at document.body — do NOT reparent. (See the header
  // comment for why reparenting was the source of the handoff gap.)
  // Force reflow so the style change commits before the class swap +
  // innerHTML injection (avoids the "no transition fires" gotcha when an
  // element is freshly restyled).
  void gateFog.offsetWidth;
  gateFog.className = 'fable-boot-fog';
  // Pin position:fixed inline so the .fable-boot-fog class's
  // position:absolute doesn't change the containing block — the node
  // stays viewport-fixed exactly as it was as .fable-fog-gate.
  gateFog.style.position = 'fixed';
  gateFog.innerHTML = `
    <div class="fable-fog-panel left"></div>
    <div class="fable-fog-panel right"></div>
  `;
  return gateFog;
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
// fogNode: the gate-built fog div (already at full opacity, still a child
//   of document.body — fable.js does NOT adopt it; THIS module reparents
//   it into #fable at Phase 2, after HOLD). May be null on a bare launch
//   (dev shortcuts) — then boot skips the part and just does ripple+reveal.
// wind: the gate's wind handle { stop() }, already playing. boot owns
//   stopping it when the clouds are gone.
// =============================================================
export function playBootTransition({
  fableRoot,
  titleScreen,
  musicHost,
  rippleAnchorBtn,
  allButtons = [],
  revealOrder = [],
  fogNode,
  wind,
}) {
  let cancelled = false;
  let fog = null;
  let aura = null;
  let rippleSfx = null;
  let windRef = wind || null;
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
    if (windRef)    { windRef.stop();    windRef = null; }
    if (rippleSfx)  { rippleSfx.stop();  rippleSfx = null; }
    staggerTimers.forEach((t) => clearTimeout(t));
    staggerTimers.length = 0;
    pendingTimers.forEach((t) => clearTimeout(t));
    pendingTimers.length = 0;
    if (fog && fog.parentNode)   fog.parentNode.removeChild(fog);
    if (aura && aura.parentNode) aura.parentNode.removeChild(aura);
    fog = null;
    aura = null;
    showButtonsImmediate();
  };

  // --- Reduced-motion fast path ----------------------------------
  if (reducedMotion()) {
    // Remove the gate fog, start music, done.
    if (fogNode && fogNode.parentNode) fogNode.parentNode.removeChild(fogNode);
    if (windRef) { windRef.stop(); windRef = null; }
    startThemeMusic(musicHost);
    return { cancel };
  }

  // --- Cinematic timeline ----------------------------------------
  (async () => {
    // Hide buttons first (idempotent — openFable already hid them, but a
    // bare launch path that skips openFable needs this).
    hideButtons();
    // Keep a handle to the gate fog. We do NOT reparent or convert it yet
    // — it stays at document.body (z:5000, position:fixed) above #fable for
    // the entire HOLD. This is the flicker fix: #fable is fading in (420ms
    // opacity transition) underneath the fog, and reparenting the fog into
    // #fable before that fade completes caused a one-frame stacking-context
    // dip + position:fixed→absolute re-layout flash. With the fog left at
    // body z:5000, the fade-in is completely hidden by the fog.
    fog = fogNode;

    // HOLD: let #fable complete its 420ms opacity transition underneath
    // the still-body-level fog. HOLD_MS (600ms) > 420ms fade so #fable is
    // fully opaque before we touch the fog.
    await sleep(HOLD_MS);
    if (cancelled) return;

    // Phase 2: convert + part, in ONE tick. The fog node STAYS at
    // document.body (no reparent — see convertToPartingFog's header for
    // why reparenting was the handoff-gap source). By this point #fable is
    // fully opaque underneath, so the parting panels reveal it through the
    // gap. The part animation then starts on the NEXT frame.
    fog = convertToPartingFog(fogNode);
    // Force a reflow so the panels' initial state is committed before the
    // .parting class triggers the animation (avoids the "no transition fires"
    // gotcha when an element is freshly inserted).
    void fog.offsetWidth;
    fog.classList.add('parting');
    startThemeMusic(musicHost, { fadeIn: true });

    // Wind stops as the part completes (WIND_STOP_MS = HOLD + PART_DURATION
    // − 150). Coupling them — instead of stopping the wind 1s early — is
    // the "fog lingers after wind dies" fix: the sound and the last visible
    // panel frame leave together.
    after(WIND_STOP_MS, () => {
      if (windRef) { windRef.stop(WIND_FADE_MS); windRef = null; }
    });

    // Phase 3: ripple aura + staggered button reveal, ~1s AFTER wind stops.
    // The deliberate silence between wind-out and the bloom lets the part
    // settle before the magic arrives.
    after(RIPPLE_DELAY_MS, () => {
      if (rippleAnchorBtn) {
        const { x, y } = centerViewport(rippleAnchorBtn);
        aura = spawnRippleAura(titleScreen, x, y);
      }
      rippleSfx = playLoopedSfx(RIPPLE_SRC, { fadeMs: RIPPLE_SFX_FADE_MS });
      revealOrder.forEach((b, i) => {
        if (!b) return;
        staggerTimers.push(setTimeout(() => {
          b.classList.remove('fable-title-btn--hidden');
        }, i * BUTTON_STAGGER_MS));
      });
    });

    // Fog removal is tied to the PART completing (HOLD + PART_DURATION),
    // NOT to the ripple-aura lifetime. The prior formula left the fog
    // nodes invisible-but-present in the DOM ~1.6s past their visible end.
    after(FOG_REMOVE_MS, () => {
      if (fog && fog.parentNode) fog.parentNode.removeChild(fog);
      fog = null;
    });

    await sleep(AURA_REMOVE_MS);
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
