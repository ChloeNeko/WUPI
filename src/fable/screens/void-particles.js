// =============================================================
// VOID CANVAS — the Quick Play void background.
//
// A single-canvas particle layer + a canvas-painted radial gradient
// backdrop. The ominous sibling of embers.js (same lifecycle discipline,
// opposite mood): where embers rise fast in warm amber from an unseen
// hearth, void motes drift slowly UPWARD across the whole frame as small
// sharp-edged pixel particles in deep purple / cosmic violet / dusky
// magenta — Minecraft-void-style cosmic dust suspended in a deep dark.
//
// DESIGN (Gemini-spec, Chloe-approved 2026-08-05):
//   • Background: radial gradient painted ON the canvas — center core
//     #07030d (ultra-dark void violet) bleeding into pure #000000 at the
//     edges. The host screen's flat #010103 base is fully covered by this.
//   • Particles: 60–80 small sharp square/rounded pixel motes, 1–3px.
//     Palette: deep dark purple #4b0082, cosmic violet #8a2be2, faint
//     dusky magenta #6a0dad (randomized per mote).
//   • Movement: spawn at random (x,y), drift slowly UPWARD at varying low
//     speeds + a gentle horizontal sine-wave wobble (floating dust).
//   • Lifecycle: spawn at opacity 0, fade IN smoothly to a max 0.3–0.6,
//     fade back OUT to 0, then reset to the bottom of the viewport.
//
// PERFORMANCE (mandate):
//   • ONE requestAnimationFrame loop drives the gradient + every mote.
//   • Canvas resizes to host dimensions on window resize (handled here +
//     via a ResizeObserver, mirroring embers.js — the host can reveal at
//     0×0 before the app window paints, and 'resize' never fires for that).
//   • RAF self-pauses on document.hidden (no cycles on invisible motes).
//
// SCOPE: kept scoped to the Quick Play screen (mounted into the screen's
// .fable-void-particle-host), NOT a global fixed z:-1 layer. Same reason as
// every other fable ambient: clean per-screen lifecycle (fresh on show,
// destroyed on hide) so the next screen starts clean + no RAF leaks across
// screen changes or Fable close. The Gemini spec's "fixed; z-index:-1" was a
// shape suggestion, not a hard requirement — the visual is identical when
// scoped to this one full-screen surface.
//
// LIFECYCLE: mirrors embers.js / particles.js. Fresh on screen show(),
// destroyed on hide()/close (cancel RAF, drop canvas, detach listeners).
// Reused by BOTH the void-form screen + the drift phase — the drift keeps
// this system running after the form fades out (quickplay-form.js +
// fable.js beginVoidDrift).
// =============================================================

// ── Tunables ────────────────────────────────────────────────────────────
// Particle count: keep it light so it reads as subtle background dust, not
// a snow globe. 70 is inside the spec's 60–80 band.
const MOTE_COUNT = 70;

// Palette (Minecraft-void cosmic): deep purple, cosmic violet, dusky magenta.
// Each entry is the [r,g,b] so we can compose `rgba(r,g,b,a)` per frame
// without string surgery.
const MOTE_COLORS = [
  [75, 0, 130],     // #4b0082 — deep dark purple
  [138, 43, 226],   // #8a2be2 — cosmic violet
  [106, 13, 173],   // #6a0dad — faint dusky magenta
];

// Movement.
const MIN_SIZE = 1;            // px — sharp-edged pixel motes (1–3px band)
const MAX_SIZE = 3;
const MIN_RISE = 0.10;         // px/frame upward drift (slow — floating dust)
const MAX_RISE = 0.35;
const SWAY_AMP = 0.6;          // px — gentle horizontal sine wobble magnitude
const SWAY_FREQ_MIN = 0.0008;  // rad/ms (very slow, independent per mote)
const SWAY_FREQ_MAX = 0.0020;
const FADE_MIN = 0.3;          // max opacity floor (fade-in target varies per mote)
const FADE_MAX = 0.6;          // max opacity ceiling
const FADE_RATE = 0.00045;     // opacity Δ per ms — smooth, ~2–3s each fade leg

// Background radial gradient stops (canvas-painted). Center core → edges.
const BG_CORE = '#07030d';     // ultra-dark void violet (center)
const BG_EDGE = '#000000';     // pure pitch black (edges)

// Build a single mote. Spawn at a random (x,y) scattered across the WHOLE
// frame; it will drift upward, fade in, then fade out, then respawn at a new
// random (x,y) anywhere in the frame (NOT just the bottom — see the reset
// comment in frame()). Each mote owns its rise speed, sway phase/freq, max
// opacity, and color so the field has independent life (no lockstep — why
// canvas, not CSS).
function makeMote(w, h) {
  const size = MIN_SIZE + Math.random() * (MAX_SIZE - MIN_SIZE);
  return {
    x: Math.random() * w,
    y: Math.random() * h,
    size,
    rise: MIN_RISE + Math.random() * (MAX_RISE - MIN_RISE),  // upward speed
    swayPhase: Math.random() * Math.PI * 2,
    swayFreq: SWAY_FREQ_MIN + Math.random() * (SWAY_FREQ_MAX - SWAY_FREQ_MIN),
    maxOpacity: FADE_MIN + Math.random() * (FADE_MAX - FADE_MIN),
    opacity: 0,             // spawn invisible → fade in
    fadingIn: true,         // true = fading toward maxOpacity; false = fading out
    color: MOTE_COLORS[(Math.random() * MOTE_COLORS.length) | 0],
  };
}

// Create the void-canvas system over a host element. Mounts a canvas sized
// to the host, paints the radial gradient, spawns motes, runs the RAF loop.
// Returns a controller with start()/stop()/destroy(). Destroy MUST be called
// on screen hide / Fable close (same contract as createEmbers).
export function createVoidParticles(host) {
  if (!host) return null;
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-void-motes';
  canvas.setAttribute('aria-hidden', 'true');
  // Canvas sits behind the screen content (host is z:0). Pointer-events none
  // so every field/button stays clickable.
  canvas.style.cssText =
    'position:absolute;inset:0;width:100%;height:100%;pointer-events:none;';
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let motes = [];
  let raf = 0;
  let running = false;
  let dpr = 1;
  let lastT = 0;

  // Paint the backdrop directly on the canvas: the core void gradient + an
  // OVAL vignette around the edges, all in one draw pass (no second canvas).
  // Called once per frame before the motes. Anchored to the viewport center.
  //
  //   Layer 1 — the core void gradient: center #07030d (ultra-dark void
  //             violet) bleeding to #000000 at the edges.
  //   Layer 2 — an OVAL black vignette painted over the core: a radial
  //             gradient drawn on a context scaled to the screen's aspect
  //             ratio, so the (circular) falloff stretches into an ellipse
  //             that reaches all four edges smoothly. The darkening only
  //             begins near the outer rim (inner stop ~70% of the way out)
  //             and is SOFT (not a hard rectangle), so it reads as a rounded
  //             oval darkening hugging the edges — not a hard rectangular
  //             frame, and not a tight circular spotlight.
  //
  // SHAPE HISTORY (load-bearing — don't regress):
  //   • First try: a radial vignette. Read as a circular "spotlight" because
  //     a radial gradient's darkening follows circular iso-lines + only
  //     reaches the corners (the longest diagonal) before the edge midpoints.
  //     (Chloe: "too circular like a weird spotlight.")
  //   • Second try: four linear gradients (one per edge). Darkened uniformly
  //     along the whole border but too HARD/rectangular — no curve.
  //     (Chloe: "the edges are too straight, it should be more oval.")
  //   • This (third) approach: scale the context so a radial gradient
  //     becomes an ELLIPSE matching the screen aspect. The oval darkening
  //     reaches every edge (it's stretched to fill) AND stays curved/soft.
  function paintBackground(w, h) {
    const cx = w / 2, cy = h / 2;
    // Layer 1: core void gradient (unchanged).
    const core = ctx.createRadialGradient(cx, cy, 0, cx, cy, Math.max(w, h) * 0.75);
    core.addColorStop(0, BG_CORE);
    core.addColorStop(1, BG_EDGE);
    ctx.fillStyle = core;
    ctx.fillRect(0, 0, w, h);

    // Layer 2: oval vignette via an aspect-scaled radial gradient. We draw
    // the vignette in a NORMALIZED unit space (a unit circle at the screen
    // center) by scaling the context so that unit circle stretches to cover
    // the full viewport as an ellipse. Then a radial gradient from transparent
    // (center) → black (rim) becomes an oval falloff that touches every edge.
    //
    // DPR CARE (load-bearing): the canvas context carries a dpr transform set
    // in resize() (ctx.setTransform(dpr,...)) so CSS-pixel coords map to
    // device pixels. If we ctx.scale ON TOP of that, the scales compound and
    // the normalized unit circle maps to the wrong device radius (this was the
    // "whole screen went black" bug — dpr×scale pushed the dark band over the
    // center). Fix: RESET to the identity transform for the vignette layer,
    // do the translate+scale in raw DEVICE space, then restore the dpr
    // transform for everything afterward (motes are drawn in CSS space).
    ctx.save();
    ctx.setTransform(1, 0, 0, 1, 0, 0);          // raw device space, no dpr
    const dcx = canvas.width / 2, dcy = canvas.height / 2;
    // Scale so a radius-1 unit circle stretches to reach each edge: an ellipse
    // inscribing the viewport. ×1.02 overshoot pushes the darkest band just
    // past the border so the very edge is fully dark, not half-faded.
    ctx.translate(dcx, dcy);
    ctx.scale(canvas.width / 2, canvas.height / 2);
    // In this normalized space the gradient is a unit circle (radius 1) at
    // the origin. The vignette is SMALL + hugs the edges/corners: the central
    // ~82% stays fully clear (so the darkening band is narrow, only the outer
    // rim), then ramps steeply to full black at the very edge + corners.
    // NOTE: addColorStop offsets MUST be within [0.0, 1.0] — a value > 1
    // throws an error that aborts the whole paint (the prior 1.02 stop
    // silently killed every frame → all-black canvas). The edge-overshoot is
    // baked into the scale (×1.02 above) so the darkest stop lands just past
    // the visible border; here all stops are clamped to ≤ 1.0.
    // (Chloe 2026-08-05: "make the vignette hug the edges and corners a bit
    // better, make the vignette itself a bit smaller.")
    const vig = ctx.createRadialGradient(0, 0, 0, 0, 0, 1);
    vig.addColorStop(0, 'rgba(0,0,0,0)');          // center: untouched
    vig.addColorStop(0.82, 'rgba(0,0,0,0)');       // inner 82%: still clear (smaller vignette)
    vig.addColorStop(0.92, 'rgba(0,0,0,0.45)');    // rim begins: light dark
    vig.addColorStop(0.98, 'rgba(0,0,0,0.9)');     // near edge: dark
    vig.addColorStop(1.0, 'rgba(0,0,0,1)');        // edge + corners: pure black (hugs corners)
    ctx.fillStyle = vig;
    // Fill a 2×2 square centered at origin (covers the full unit circle + a
    // hair beyond). In device space this maps to the full canvas.
    ctx.fillRect(-1, -1, 2, 2);
    ctx.restore();
    // Restore the dpr transform for the motes (drawn in CSS-pixel coords).
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  // Resize the canvas to match the host + (re)spawn motes to fill it. On
  // later resizes keep motes but wrap any now-off-screen ones back in.
  function resize() {
    dpr = window.devicePixelRatio || 1;
    const w = host.clientWidth;
    const h = host.clientHeight;
    canvas.width = Math.max(1, Math.floor(w * dpr));
    canvas.height = Math.max(1, Math.floor(h * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (motes.length === 0) {
      for (let i = 0; i < MOTE_COUNT; i++) motes.push(makeMote(w, h));
    } else {
      // Keep motes; wrap any now-off-screen ones back into the frame.
      for (const m of motes) {
        if (m.x > w || m.x < 0) m.x = Math.random() * w;
        if (m.y > h || m.y < 0) m.y = Math.random() * h;
      }
    }
  }

  // One animation frame. dt is milliseconds since last frame (clamped so a
  // tab-switch gap doesn't fast-forward motes across the screen). Paints the
  // background gradient, then advances + draws each mote.
  function frame(t) {
    if (!running) return;
    const dt = lastT ? Math.min(t - lastT, 64) : 16;
    lastT = t;
    const w = host.clientWidth;
    const h = host.clientHeight;

    // Backdrop first (covers the screen's flat base with the radial gradient).
    paintBackground(w, h);

    // Motes.
    for (const m of motes) {
      // Drift upward (subtract from y). Lower y = higher on screen.
      m.y -= m.rise * (dt / 16);
      // Gentle horizontal sine wobble (independent per mote).
      m.swayPhase += m.swayFreq * dt;
      const drawX = m.x + Math.sin(m.swayPhase) * SWAY_AMP;

      // Fade in → hold at max → fade out. Two-state machine per mote:
      //   fadingIn: opacity ramps toward maxOpacity, then flips to fade-out.
      //   !fadingIn: opacity ramps toward 0; at 0 the mote wraps to bottom.
      if (m.fadingIn) {
        m.opacity += FADE_RATE * dt;
        if (m.opacity >= m.maxOpacity) {
          m.opacity = m.maxOpacity;
          m.fadingIn = false;
        }
      } else {
        m.opacity -= FADE_RATE * dt;
        if (m.opacity <= 0) {
          // Fully faded out → respawn scattered across the WHOLE frame (not
          // just the bottom). The drift speed is slow (cosmic-dust ambiance),
          // so respawning only at the bottom made motes pile up there — they'd
          // fade in/out before climbing far. Scattering the respawn across the
          // full height keeps the field uniformly populated top-to-bottom.
          // (Chloe 2026-08-05: "all the particles pile at the very bottom,
          // I'd like all of them to be scattered all around the background.")
          m.opacity = 0;
          m.fadingIn = true;
          m.x = Math.random() * w;
          m.y = Math.random() * h;             // anywhere in the frame
          m.rise = MIN_RISE + Math.random() * (MAX_RISE - MIN_RISE);
          m.maxOpacity = FADE_MIN + Math.random() * (FADE_MAX - FADE_MIN);
          m.color = MOTE_COLORS[(Math.random() * MOTE_COLORS.length) | 0];
        }
      }

      // Draw: small sharp square pixel mote (slightly rounded via a tiny
      // radius when size ≥ 2). Sharp edges = the "pixel particle" aesthetic
      // from the spec; the faint rounding on bigger motes keeps them from
      // looking like dead pixels at 3px.
      const [r, g, b] = m.color;
      ctx.fillStyle = `rgba(${r},${g},${b},${m.opacity.toFixed(3)})`;
      if (m.size >= 2) {
        const rad = m.size * 0.25;
        // roundRect where supported (modern browsers); else fall back to a
        // plain rect (sharp) — the visual difference at 1–3px is negligible.
        if (typeof ctx.roundRect === 'function') {
          ctx.beginPath();
          ctx.roundRect(drawX - m.size / 2, m.y - m.size / 2, m.size, m.size, rad);
          ctx.fill();
        } else {
          ctx.fillRect(drawX - m.size / 2, m.y - m.size / 2, m.size, m.size);
        }
      } else {
        // 1px: a single device pixel, sharp.
        ctx.fillRect(drawX - 0.5, m.y - 0.5, 1, 1);
      }

      // Wrap upward-drifting motes that exit the top: re-enter scattered
      // anywhere in the frame (not just the bottom) so the field stays
      // uniformly populated. This is separate from the fade-out reset — a
      // mote that's still mid-fade when it crosses the top re-enters with its
      // current opacity intact so there's no visible "pop."
      if (m.y < -10) {
        m.y = Math.random() * h;
        m.x = Math.random() * w;
      }
    }
    raf = requestAnimationFrame(frame);
  }

  function start() {
    if (running) return;
    running = true;
    lastT = 0;
    raf = requestAnimationFrame(frame);
  }
  function stop() {
    running = false;
    if (raf) { cancelAnimationFrame(raf); raf = 0; }
  }

  // Pause on hidden (alt-tab / minimize) — no cycles on invisible motes.
  function onVisibility() {
    if (document.hidden) stop();
    else start();
  }
  const onResize = () => resize();

  resize();
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('resize', onResize);
  // ResizeObserver: re-size whenever the HOST changes size, not just on
  // window resize. The form screen can show while the host measures 0×0 at
  // first paint, and 'resize' never fires for that reveal — but a
  // ResizeObserver on the host does. Matches embers.js / particles.js.
  const ro = new ResizeObserver(() => resize());
  ro.observe(host);
  start();

  // Destroy: stop the loop, remove listeners, drop the canvas. Called on
  // screen hide / Fable close so the next show starts clean.
  return {
    start,
    stop,
    destroy() {
      stop();
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('resize', onResize);
      ro.disconnect();
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
      motes = [];
    },
  };
}
