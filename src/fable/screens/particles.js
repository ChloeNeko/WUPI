// =============================================================
// PARTICLES — ambient floating motes (pollen / spores / dust caught
// in light) for the Fable title screen. A lightweight canvas particle
// system: each mote drifts independently with its own velocity, gentle
// horizontal sway, and opacity breathing, so the field reads as air-
// borne dust rather than a shifting sheet.
//
// WHY CANVAS, not CSS tiled backgrounds: CSS background-position moves
// every dot in lockstep (reads as a scrolling sheet, not particles),
// and the per-dot opacity can't breathe independently. Canvas gives
// each mote an independent life — that's what makes floating pollen
// feel real.
//
// LIFECYCLE: the system is created FRESH per title show() and fully
// torn down on hide()/close (cancelAnimationFrame + canvas removed +
// listeners off) so nothing leaks across exit/relaunch. The RAF
// self-pauses on document.hidden (alt-tab / minimize) to match the
// OS shell's discipline.
// =============================================================

// Tunables. Counts kept modest (title screen, not a game scene) so the
// cost is negligible. Sizes/opacity set so the motes are genuinely
// VISIBLE (the earlier CSS version was washed out behind the dim).
const MOTE_COUNT = 70;
const MOTE_COLORS = [
  'rgba(255, 226, 150, ',   // warm gold (pollen)
  'rgba(255, 238, 190, ',   // pale cream
  'rgba(255, 210, 130, ',   // amber
];
const DRIFT_SPEED = 0.12;     // px/frame baseline vertical drift (slow rise)
const SWAY_AMP = 0.6;         // px/frame max horizontal sway magnitude
const MIN_R = 0.8;            // min mote radius (px)
const MAX_R = 2.6;            // max mote radius (px)
const BREATH_PERIOD = 4000;   // ms for one opacity breathe cycle (per-mote phase offset)

// Build a single mote with randomized properties. Far motes are smaller
// + dimmer (depth); near motes larger + brighter. position is in CSS px
// relative to the canvas size at spawn time.
function makeMote(w, h) {
  const depth = Math.random();             // 0 = far, 1 = near
  const r = MIN_R + depth * (MAX_R - MIN_R);
  return {
    x: Math.random() * w,
    y: Math.random() * h,
    r,
    // Near motes rise a touch faster (closer to the wind).
    vy: -(DRIFT_SPEED * (0.5 + depth)),
    // Each mote sways on its own sine phase so the field never moves
    // in unison.
    swayPhase: Math.random() * Math.PI * 2,
    swayFreq: 0.004 + Math.random() * 0.006,   // rad/frame
    baseOpacity: 0.25 + depth * 0.5,           // 0.25 (far) → 0.75 (near)
    breathPhase: Math.random() * Math.PI * 2,
    color: MOTE_COLORS[(Math.random() * MOTE_COLORS.length) | 0],
  };
}

// Create the particle system over a host element. Mounts a canvas sized
// to the host, spawns motes, and runs the RAF loop. Returns a controller
// with start()/stop()/destroy(). Destroy MUST be called on close to free
// the RAF + listeners (the load-bearing reset against the relaunch bug).
export function createTitleParticles(host) {
  if (!host) return null;
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-title-particles';
  canvas.setAttribute('aria-hidden', 'true');
  // The canvas sits behind the dim + content but ABOVE the base image,
  // so motes read as floating in the scene. Pointer-events none so the
  // menu stays clickable through it.
  canvas.style.cssText =
    'position:absolute;inset:0;width:100%;height:100%;z-index:2;pointer-events:none;';
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let motes = [];
  let raf = 0;
  let running = false;
  let dpr = 1;

  // Resize the canvas to match the host + (re)spawn motes to fill it.
  // Called on init + window resize so the field always covers the screen.
  function resize() {
    dpr = window.devicePixelRatio || 1;
    const w = host.clientWidth;
    const h = host.clientHeight;
    canvas.width = Math.max(1, Math.floor(w * dpr));
    canvas.height = Math.max(1, Math.floor(h * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Spawn if empty (first resize) — on later resizes keep motes but
    // wrap any that are now off-screen back into bounds.
    if (motes.length === 0) {
      for (let i = 0; i < MOTE_COUNT; i++) motes.push(makeMote(w, h));
    } else {
      for (const m of motes) {
        if (m.x > w) m.x = Math.random() * w;
        if (m.y > h) m.y = Math.random() * h;
      }
    }
  }

  // One animation frame. Advances each mote (rise + sway + wrap-around
  // at the top), computes its breathing opacity, and draws a soft radial
  // dot (a radial gradient gives the glow falloff that makes motes read
  // as luminous dust, not flat pixels).
  function frame(t) {
    if (!running) return;
    const w = host.clientWidth;
    const h = host.clientHeight;
    ctx.clearRect(0, 0, w, h);
    for (const m of motes) {
      // Drift up.
      m.y += m.vy;
      // Sway horizontally (independent sine per mote).
      m.swayPhase += m.swayFreq;
      m.x += Math.sin(m.swayPhase) * SWAY_AMP * 0.1;
      // Wrap: when a mote rises off the top, recycle it at the bottom
      // at a fresh x so the field never thins out.
      if (m.y < -10) {
        m.y = h + 10;
        m.x = Math.random() * w;
      }
      // Breathing opacity (per-mote phase so some glow while others dim).
      const breath = 0.5 + 0.5 * Math.sin((t / BREATH_PERIOD) * Math.PI * 2 + m.breathPhase);
      const op = m.baseOpacity * (0.35 + 0.65 * breath);
      // Soft radial dot with a warm glow.
      const grad = ctx.createRadialGradient(m.x, m.y, 0, m.x, m.y, m.r * 3);
      grad.addColorStop(0, m.color + op + ')');
      grad.addColorStop(0.4, m.color + (op * 0.5) + ')');
      grad.addColorStop(1, m.color + '0)');
      ctx.fillStyle = grad;
      ctx.beginPath();
      ctx.arc(m.x, m.y, m.r * 3, 0, Math.PI * 2);
      ctx.fill();
    }
    raf = requestAnimationFrame(frame);
  }

  function start() {
    if (running) return;
    running = true;
    raf = requestAnimationFrame(frame);
  }
  function stop() {
    running = false;
    if (raf) { cancelAnimationFrame(raf); raf = 0; }
  }

  // Pause on hidden (alt-tab / minimize) to match the OS shell's RAF
  // discipline — no point burning cycles drawing invisible motes.
  function onVisibility() {
    if (document.hidden) stop();
    else start();
  }
  const onResize = () => resize();

  resize();
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('resize', onResize);
  // ResizeObserver: re-size whenever the HOST changes size, not just on
  // window resize. createTitleParticles runs while #fable is still hidden
  // (init-time showScreen('title') fires before openFable adds .show), so
  // the host measures 0×0 at first resize() and only gains real size once
  // the app is revealed — window 'resize' never fires for that, but a
  // ResizeObserver on the host does. Matches grass.js's fix.
  const ro = new ResizeObserver(() => resize());
  ro.observe(host);
  start();

  // Destroy: stop the loop, remove listeners, drop the canvas. Called on
  // Fable close so the next open starts clean (no leaked RAF/listeners).
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
