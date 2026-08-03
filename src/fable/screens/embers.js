// =============================================================
// EMBERS — rising fire-ember motes for the New Game flow screens.
//
// A lightweight canvas particle system, the sibling of the title's
// particles.js (same lifecycle discipline, different motion + palette).
// Each ember spawns along the bottom edge, rises upward with its own
// velocity, gentle horizontal sine sway (fire flutter), and a fast
// per-ember opacity flicker. Embers fade as they climb and recycle at
// the top — so the field reads as heat shimmering up from an unseen
// hearth, never thinning out.
//
// WHY CANVAS, not CSS tiled backgrounds (same reason as particles.js):
// CSS background-position moves every dot in lockstep and can't give
// each mote an independent flicker life. Canvas gives each ember its
// own velocity, sway phase, and flicker cadence — that's what makes
// rising fire feel real rather than a scrolling sheet.
//
// LIFECYCLE: mirrors particles.js byte-for-byte in shape. Created fresh
// on screen show() and fully torn down on hide()/close (cancel the RAF,
// drop the canvas, detach listeners) so nothing leaks across screen
// changes or Fable exit/relaunch. The RAF self-pauses on document.hidden
// (alt-tab / minimize) to match the OS shell's discipline. ONE system per
// visible screen at a time (the router stops the previous screen's
// ambient before starting the next's).
// =============================================================

// Tunables. Counts kept modest — this is a menu backdrop, not a game
// scene — so the cost is negligible (fewer sprites than the title's 70
// motes already running). Warm orange-gold palette so the embers read
// clearly as fire while staying cohesive with Fable's brass accent.
const EMBER_COUNT = 45;
const EMBER_COLORS = [
  'rgba(255, 170, 60, ',   // hot amber
  'rgba(255, 130, 40, ',   // deep orange
  'rgba(255, 200, 95, ',   // pale gold
  'rgba(255, 110, 30, ',   // ember red-orange
];
const RISE_SPEED = 0.55;      // px/frame baseline vertical rise (slow drift up)
const SWAY_AMP = 0.9;         // px/frame max horizontal sway (fire flutter)
const MIN_R = 0.6;            // min ember radius (px)
const MAX_R = 2.2;            // max ember radius (px)
const FLICKER_PERIOD = 800;   // ms for one opacity flicker cycle (per-ember phase offset)

// Build a single ember with randomized properties. Near embers are larger,
// brighter, and rise a touch faster (closer to the heat); far embers are
// smaller + dimmer (depth). position is in CSS px relative to the canvas.
function makeEmber(w, h) {
  const depth = Math.random();             // 0 = far, 1 = near
  const r = MIN_R + depth * (MAX_R - MIN_R);
  return {
    // Spawn anywhere along the lower two-thirds of the screen so the field
    // is immediately populated (rather than empty for the first ~2 s while
    // everything climbs up from the floor).
    x: Math.random() * w,
    y: h - Math.random() * h * 0.66,
    r,
    // Negative vy = rising. Near embers rise a touch faster (closer to the
    // updraft); far embers drift up slowly.
    vy: -(RISE_SPEED * (0.45 + depth * 0.9)),
    // Each ember sways on its own sine phase so the column never moves in
    // unison — reads as individual tongues of heat.
    swayPhase: Math.random() * Math.PI * 2,
    swayFreq: 0.01 + Math.random() * 0.012,     // rad/frame (faster than motes)
    baseOpacity: 0.35 + depth * 0.5,            // 0.35 (far) → 0.85 (near)
    flickerPhase: Math.random() * Math.PI * 2,
    // Slightly different flicker cadence per ember so the shimmer isn't a
    // uniform pulse.
    flickerFreq: 0.7 + Math.random() * 0.6,     // × the FLICKER_PERIOD base rate
    color: EMBER_COLORS[(Math.random() * EMBER_COLORS.length) | 0],
  };
}

// Create the ember system over a host element. Mounts a canvas sized to
// the host, spawns embers, and runs the RAF loop. Returns a controller
// with start()/stop()/destroy(). Destroy MUST be called on screen hide
// (or Fable close) to free the RAF + listeners — the load-bearing reset
// against the relaunch leak (same contract as createTitleParticles).
export function createEmbers(host) {
  if (!host) return null;
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-embers';
  canvas.setAttribute('aria-hidden', 'true');
  // The canvas sits behind the screen content (z:0 host) so embers read
  // as floating in the void behind the UI. Pointer-events none so every
  // tile/card/input stays clickable through it.
  canvas.style.cssText =
    'position:absolute;inset:0;width:100%;height:100%;pointer-events:none;';
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let embers = [];
  let raf = 0;
  let running = false;
  let dpr = 1;

  // Resize the canvas to match the host + (re)spawn embers to fill it.
  // Called on init + window/host resize so the field always covers the
  // screen. On later resizes keep embers but wrap any now-off-screen ones
  // back into bounds (matches particles.js's resize contract).
  function resize() {
    dpr = window.devicePixelRatio || 1;
    const w = host.clientWidth;
    const h = host.clientHeight;
    canvas.width = Math.max(1, Math.floor(w * dpr));
    canvas.height = Math.max(1, Math.floor(h * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (embers.length === 0) {
      for (let i = 0; i < EMBER_COUNT; i++) embers.push(makeEmber(w, h));
    } else {
      for (const m of embers) {
        if (m.x > w) m.x = Math.random() * w;
        if (m.y > h) m.y = h - Math.random() * h * 0.66;
      }
    }
  }

  // One animation frame. Advances each ember (rise + sway + recycle at
  // the top), computes its flicker opacity + height-fade, and draws a
  // soft radial dot (the radial gradient gives the warm glow falloff that
  // makes embers read as luminous heat, not flat pixels).
  function frame(t) {
    if (!running) return;
    const w = host.clientWidth;
    const h = host.clientHeight;
    ctx.clearRect(0, 0, w, h);
    for (const m of embers) {
      // Rise up.
      m.y += m.vy;
      // Sway horizontally (independent sine per ember).
      m.swayPhase += m.swayFreq;
      m.x += Math.sin(m.swayPhase) * SWAY_AMP * 0.1;
      // Recycle: when an ember rises off the top, respawn it at the
      // bottom at a fresh x so the field never thins out. The ember
      // "re-ignites" at the hearth line.
      if (m.y < -10) {
        m.y = h + 10;
        m.x = Math.random() * w;
      }
      // Height fade: embers dim as they climb (heat dissipating), so the
      // column is brightest near the floor + wisps away near the top.
      // heightT = 1 at the floor, → 0 at the top.
      const heightT = Math.max(0, Math.min(1, m.y / h));
      const heightFade = 0.25 + 0.75 * heightT;
      // Flicker opacity (per-ember phase + cadence so some glow while
      // others dim — the shimmer of live coals).
      const flicker = 0.5 + 0.5 * Math.sin((t / FLICKER_PERIOD) * m.flickerFreq * Math.PI * 2 + m.flickerPhase);
      const op = m.baseOpacity * (0.4 + 0.6 * flicker) * heightFade;
      // Soft radial dot with a warm glow.
      const grad = ctx.createRadialGradient(m.x, m.y, 0, m.x, m.y, m.r * 3.2);
      grad.addColorStop(0, m.color + op + ')');
      grad.addColorStop(0.4, m.color + (op * 0.5) + ')');
      grad.addColorStop(1, m.color + '0)');
      ctx.fillStyle = grad;
      ctx.beginPath();
      ctx.arc(m.x, m.y, m.r * 3.2, 0, Math.PI * 2);
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
  // discipline — no point burning cycles drawing invisible embers.
  function onVisibility() {
    if (document.hidden) stop();
    else start();
  }
  const onResize = () => resize();

  resize();
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('resize', onResize);
  // ResizeObserver: re-size whenever the HOST changes size, not just on
  // window resize. The New Game screens can be shown while #fable measures
  // 0×0 at first paint (init-time build before the app is revealed), and
  // window 'resize' never fires for that reveal — but a ResizeObserver on
  // the host does. Matches particles.js / grass.js's fix.
  const ro = new ResizeObserver(() => resize());
  ro.observe(host);
  start();

  // Destroy: stop the loop, remove listeners, drop the canvas. Called on
  // screen hide / Fable close so the next show starts clean (no leaked
  // RAF/listeners). Identical contract to createTitleParticles.
  return {
    start,
    stop,
    destroy() {
      stop();
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('resize', onResize);
      ro.disconnect();
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
      embers = [];
    },
  };
}
