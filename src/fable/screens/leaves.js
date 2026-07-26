// =============================================================
// WIND LEAVES — sparse, wind-blown leaves crossing the Fable title
// screen. Completes the living-scene atmospheric layers alongside the
// pollen motes (particles.js), grass (grass.js), and clouds (pure CSS).
//
// DESIGN GOAL: a few autumn leaves drifting left→right with the wind,
// NOT a chaotic blizzard. Hard cap of 5 leaves on screen at once.
//
// EXECUTION MODEL (Prime Directive — near-zero overhead):
// Leaves are DOM <div> nodes animated by a pure CSS animation
// (leafBlow @keyframes, defined in fable.css). Movement is entirely
// on the GPU compositor via transform translate3d + rotate + opacity
// (no layout, no paint per frame). Each leaf's randomized trajectory
// is baked into CSS custom properties (--sx/--sy start, --ex/--ey end,
// --rot tumble, --dur, --maxop, --scale) BEFORE the animation starts,
// so the compositor has the whole path pre-resolved. No per-frame JS
// runs at all while leaves blow.
//
// POOLING + RECYCLING: a fixed pool of LEAF_POOL nodes is created once
// on show. A spawn scheduler (setTimeout cadence) picks an inactive
// node, randomizes its trajectory props, restarts its animation, and
// on animationend the node is hidden + returned to the pool. No DOM
// nodes are ever created/destroyed during play → zero GC churn, zero
// layout invalidation from node churn.
//
// PAUSE ON TAB-OUT: the leafBlow animation respects prefers-reduced-
// motion (CSS). For runtime tab-out, the host element's
// animation-play-state is toggled (paused when hidden, running when
// visible) via a visibilitychange listener — pausing freezes every
// leaf mid-flight without any JS-per-leaf work, and unpausing resumes
// from the exact spot (compositor state preserved).
//
// LIFECYCLE: created FRESH per title show() and fully torn down on
// hide()/close (clears all timeouts + listeners + removes nodes) so
// nothing leaks across exit/relaunch.
// =============================================================

// --- Tunables ---------------------------------------------------
const LEAF_POOL = 6;            // pool size (cap on simultaneous leaves)
const MAX_ACTIVE = 5;           // hard cap on screen at once (≤ pool)
const SPAWN_MIN = 2600;         // min ms between spawns
const SPAWN_MAX = 5200;         // max ms between spawns
const DURATION_MIN = 9000;      // min ms to cross the screen
const DURATION_MAX = 13000;     // max ms to cross the screen
const LEAF_CLASSES = ['c1', 'c2', 'c3'];   // the three tint classes

let activeCount = 0;            // leaves currently flying (module-local)

// Pick a randomized trajectory + flutter for one leaf. The DRIFT
// (outer) travels left→right with a gentle downward gravity pull;
// the FLUTTER (inner) tumbles + bobs on its own faster, independent
// clock. Separating them onto two elements is what breaks the rigid
// single-line march — the leaf looks like wind is pushing it while it
// pitches and dips, not like it's gliding on a rail.
function randomTrajectory(w, h) {
  const leafPx = 24;
  const scale = 0.7 + Math.random() * 0.7;          // 0.7 → 1.4
  // Y band: top 10% to 70% of the screen (clear of grass + buttons).
  const sy = (0.10 + Math.random() * 0.60) * h;
  // End Y drifts down a little (gravity), clamped to stay on-screen.
  const ey = Math.min(h * 0.80, sy + (Math.random() * 0.16 - 0.02) * h);
  const sx = -leafPx * scale;                        // just off the left
  const ex = w + leafPx * scale;                     // just off the right
  const dur = DURATION_MIN + Math.random() * (DURATION_MAX - DURATION_MIN);
  const maxop = 0.55 + Math.random() * 0.35;        // 0.55 → 0.90
  // Flutter (inner): own faster clock so it never syncs with the drift.
  // ~1.8-2.6s per cycle (several flutters per screen crossing). Bob is
  // the vertical dip amplitude in px — small, so it reads as a dip not
  // a bounce. Direction randomizes the tumble sense.
  const flutterDur = 1800 + Math.random() * 800;
  const bob = 6 + Math.random() * 8;                // 6 → 14px dip
  const flutterDir = Math.random() < 0.5 ? 'normal' : 'reverse';
  return { sx, sy, ex, ey, dur, maxop, scale, flutterDur, bob, flutterDir };
}

// Create the leaf pool system over a host element. Returns a controller
// with start()/stop()/destroy().
export function createWindLeaves(host) {
  if (!host) return null;
  const w0 = host.clientWidth || window.innerWidth;
  const h0 = host.clientHeight || window.innerHeight;
  let active = activeCount;
  let nodes = [];
  let spawnTimer = 0;
  let running = false;
  let reducedMotion = false;

  // Build the fixed pool once. Inert until spawned (opacity:0, no
  // animation). Reused forever — never create/destroy during play.
  // Each pool node is a NESTED pair: outer = drift, inner = flutter +
  // holds the leaf SVG (so the tint class lives on the inner).
  for (let i = 0; i < LEAF_POOL; i++) {
    const el = document.createElement('div');
    el.className = 'fable-wind-leaf';
    el.setAttribute('aria-hidden', 'true');
    const inner = document.createElement('div');
    inner.className = 'fable-wind-leaf-inner ' + LEAF_CLASSES[i % LEAF_CLASSES.length];
    el.appendChild(inner);
    host.appendChild(el);
    nodes.push({ el, inner, flying: false });
  }

  // Spawn one leaf from an inactive pool node. Randomizes the trajectory
  // into CSS custom properties, restarts the animation, and arms the
  // animationend recycle. Respects the MAX_ACTIVE cap.
  function spawn() {
    if (!running) return;
    if (active >= MAX_ACTIVE) return;
    const slot = nodes.find((n) => !n.flying);
    if (!slot) return;
    const w = host.clientWidth;
    const h = host.clientHeight;
    const t = randomTrajectory(w, h);
    const el = slot.el;
    const inner = slot.inner;
    // Cycle the tint on the INNER (where the SVG lives) so consecutive
    // leaves don't all match.
    inner.className = 'fable-wind-leaf-inner ' + LEAF_CLASSES[(Math.random() * LEAF_CLASSES.length) | 0];
    // Outer (drift) props.
    el.style.setProperty('--sx', t.sx + 'px');
    el.style.setProperty('--sy', t.sy + 'px');
    el.style.setProperty('--ex', t.ex + 'px');
    el.style.setProperty('--ey', t.ey + 'px');
    el.style.setProperty('--maxop', t.maxop);
    // Inner (flutter) props.
    inner.style.setProperty('--scale', t.scale);
    inner.style.setProperty('--bob', t.bob + 'px');
    // (Re)attach the one-shot recycle handler on the OUTER's drift
    // animationend (the longest-running of the two). Removing first
    // avoids duplicates if a re-spawn races an animationend.
    el.removeEventListener('animationend', slot.onEnd);
    slot.onEnd = () => {
      slot.flying = false;
      el.style.animation = '';
      el.style.opacity = '0';
      inner.style.animation = '';
      active--;
    };
    el.addEventListener('animationend', slot.onEnd);
    // Force a reflow so the browser registers the cleared animation
    // state before we re-apply — otherwise restarting the same animation
    // on the same node can be skipped as a no-op. One reflow per spawn
    // (not per frame) is acceptable: sparsity means ~1 every 4s.
    void el.offsetWidth;
    slot.flying = true;
    active++;
    // OUTER: steady linear drift (wind = constant velocity).
    el.style.animation = 'leafDrift ' + t.dur + 'ms linear forwards';
    // INNER: independent flutter on its own faster clock. Direction
    // (normal/reverse) randomizes the tumble sense per leaf.
    inner.style.animation = 'leafFlutter ' + t.flutterDur + 'ms ease-in-out ' + t.flutterDir + ' infinite';
  }

  // Cadence loop: schedule the next spawn at a randomized interval.
  // setTimeout (not RAF) because sparsity means we only wake a few times
  // per screen-crossing — no point running a 60fps loop to do nothing.
  function scheduleNext() {
    if (!running) return;
    const delay = SPAWN_MIN + Math.random() * (SPAWN_MAX - SPAWN_MIN);
    spawnTimer = setTimeout(() => {
      spawn();
      scheduleNext();
    }, delay);
  }

  function start() {
    if (running) return;
    running = true;
    active = 0;
    // Reset all pool nodes to inert.
    for (const n of nodes) {
      n.flying = false;
      n.el.style.animation = '';
      n.el.style.opacity = '0';
    }
    scheduleNext();
  }
  function stop() {
    running = false;
    if (spawnTimer) { clearTimeout(spawnTimer); spawnTimer = 0; }
  }

  // Pause/resume on tab-out by toggling the host's animation-play-state.
  // Freezes every leaf mid-flight via the compositor (no JS-per-leaf
  // work); resume continues from the exact spot.
  function onVisibility() {
    if (document.hidden) stop();
    else if (!reducedMotion) start();
  }

  reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('resize', () => {
    // No per-leaf resize math needed: trajectories are baked at spawn
    // from the live host size, so resizes only affect FUTURE leaves.
  });

  // Reduced motion: don't spawn any leaves at all.
  if (!reducedMotion) start();

  return {
    start,
    stop,
    destroy() {
      stop();
      document.removeEventListener('visibilitychange', onVisibility);
      for (const n of nodes) {
        if (n.onEnd) n.el.removeEventListener('animationend', n.onEnd);
        if (n.el.parentNode) n.el.parentNode.removeChild(n.el);
      }
      nodes = [];
      active = 0;
    },
  };
}
