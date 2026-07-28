// =============================================================
// VOID PARTICLES — the ambient mote field for the Quick Play void
// interview screen.
//
// Distinct aesthetic from the title's warm pollen motes (particles.js):
// these are SLOW, DIM, COOL-TONED specks drifting through an infinite
// black void. The read is deep space / the inside of a closed eye —
// ethereal, faintly unsettling, never warm. The user is between worlds
// here, about to describe one into being.
//
// WHY CANVAS, not CSS: same reason as particles.js — per-mote independent
// drift + opacity breathing reads as real suspended dust, not a scrolling
// sheet. Independent lives per mote is the load-bearing visual.
//
// LIFECYCLE: fresh per void show() (created in wireVoid), fully torn down
// on hide()/close (cancelAnimationFrame + canvas removed + listeners off).
// RAF self-pauses on document.hidden. Mirrors the title-particles
// discipline so the void leaves zero residual RAF across screen changes.
// =============================================================

// Tunables. Counts deliberately LOW (the void is sparse — a few dim motes,
// not a blizzard). Sizes tiny, opacity low, drift slow. Cool blue-white
// palette only.
const MOTE_COUNT = 45;
const MOTE_COLORS = [
  'rgba(180, 200, 230, ',   // cool pale blue
  'rgba(200, 210, 230, ',   // off-white blue
  'rgba(160, 180, 220, ',   // deeper dusk blue
];
const DRIFT_SPEED = 0.05;    // px/frame baseline drift (VERY slow)
const SWAY_AMP = 0.25;       // px/frame max sway (subtle)
const MIN_R = 0.6;           // min mote radius (px)
const MAX_R = 1.8;           // max mote radius (px)
const BREATH_PERIOD = 6000;  // ms per opacity breathe (slow, dreamy)

function makeMote(w, h) {
  const depth = Math.random();             // 0 = far, 1 = near
  const r = MIN_R + depth * (MAX_R - MIN_R);
  return {
    x: Math.random() * w,
    y: Math.random() * h,
    r,
    // Each mote drifts in its own slow direction (not all upward — the void
    // has no "up", so motes wander).
    vx: (Math.random() - 0.5) * DRIFT_SPEED * 2,
    vy: (Math.random() - 0.5) * DRIFT_SPEED * 2,
    swayPhase: Math.random() * Math.PI * 2,
    swayFreq: 0.002 + Math.random() * 0.003,
    // Dim: max opacity ~0.5 even for near motes. The void reads as mostly
    // empty space with faint motion, not a starfield.
    baseOpacity: 0.1 + depth * 0.4,
    breathPhase: Math.random() * Math.PI * 2,
    color: MOTE_COLORS[(Math.random() * MOTE_COLORS.length) | 0],
  };
}

// Create the particle system over a host <canvas>. Mounts + sizes the
// canvas to the host, spawns motes, runs the RAF loop. Returns a
// controller with destroy() — wireVoid stores it + teardownVoid calls it.
export function createVoidParticles(host) {
  if (!host) return { destroy() {} };
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-void-particles-canvas';
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let motes = [];
  let raf = null;
  let lastTime = 0;
  let running = false;

  function resize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = host.clientWidth || window.innerWidth;
    const h = host.clientHeight || window.innerHeight;
    canvas.width = w * dpr;
    canvas.height = h * dpr;
    canvas.style.width = w + 'px';
    canvas.style.height = h + 'px';
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Reseed on first sizing (motes need a real w/h) — but preserve across
    // a later resize so the field doesn't blink.
    if (motes.length === 0) {
      for (let i = 0; i < MOTE_COUNT; i++) motes.push(makeMote(w, h));
    }
  }

  function step(now) {
    if (!running) return;
    if (!lastTime) lastTime = now;
    const dt = Math.min(now - lastTime, 64); // clamp to avoid jumps on tab refocus
    lastTime = now;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;

    ctx.clearRect(0, 0, w, h);
    const breathFactor = (2 * Math.PI) / BREATH_PERIOD;

    for (const m of motes) {
      // Drift + sway.
      m.x += m.vx * (dt / 16) + Math.sin(now * m.swayFreq + m.swayPhase) * SWAY_AMP * (dt / 16);
      m.y += m.vy * (dt / 16);
      // Wrap around edges so motes never disappear (infinite void).
      if (m.x < -10) m.x = w + 10;
      else if (m.x > w + 10) m.x = -10;
      if (m.y < -10) m.y = h + 10;
      else if (m.y > h + 10) m.y = -10;

      // Opacity breathing (slow dreamy pulse).
      const breath = 0.5 + 0.5 * Math.sin(now * breathFactor + m.breathPhase);
      const opacity = m.baseOpacity * (0.4 + 0.6 * breath);

      ctx.beginPath();
      ctx.arc(m.x, m.y, m.r, 0, Math.PI * 2);
      ctx.fillStyle = m.color + opacity.toFixed(3) + ')';
      ctx.fill();
    }
    raf = requestAnimationFrame(step);
  }

  // Self-pause on document.hidden (matches title-particles discipline).
  function onVisibility() {
    if (document.hidden) {
      if (raf) { cancelAnimationFrame(raf); raf = null; }
      lastTime = 0;
      running = false;
    } else if (!running) {
      running = true;
      lastTime = 0;
      raf = requestAnimationFrame(step);
    }
  }

  resize();
  window.addEventListener('resize', resize);
  document.addEventListener('visibilitychange', onVisibility);
  running = true;
  raf = requestAnimationFrame(step);

  return {
    destroy() {
      running = false;
      if (raf) { cancelAnimationFrame(raf); raf = null; }
      window.removeEventListener('resize', resize);
      document.removeEventListener('visibilitychange', onVisibility);
      motes = [];
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
    },
  };
}
