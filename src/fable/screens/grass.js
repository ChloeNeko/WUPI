// =============================================================
// GRASS — a canvas blade field rooted along the bottom edge of
// the Fable title screen. Replaces the earlier static SVG fringe
// (which was a soft-light tint that read as a faint barcode, not
// grass) with a dense, depth-layered meadow of independently
// swaying blades.
//
// WHY CANVAS, not CSS/SVG: the same reasoning as particles.js — a
// CSS/SVG fringe either sits static or sways as ONE rigid block
// (lockstep), which never reads as living grass. Canvas lets each
// blade bend on its own phase so a breeze ripples through the
// field unevenly. That independent motion is what sells "grass".
//
// DEPTH: three layers (back→front), each drawn as a pass:
//   back  — many short, dark, slow blades   (depth haze)
//   mid   — medium blades, base green
//   front — fewer tall, brighter, faster blades (nearest the eye)
// Back-to-front draw order gives the parallax depth of a real
// tufted meadow edge.
//
// LIFECYCLE: created FRESH per title show() and fully torn down on
// hide()/close (cancelAnimationFrame + canvas removed + listeners
// off) so nothing leaks across exit/relaunch. RAF self-pauses on
// document.hidden, matching the OS shell + particles discipline.
// =============================================================

// --- Tunables ---------------------------------------------------
// Counts tuned for "lush, not sparse". Back layer is the densest
// (short ground cover); front layer is sparse (tall accents).
// On wide screens this still reads full because blades are spaced
// in CSS px, not a fixed tile.
const BACK_BLADE_SPACING  = 3.2;   // px between back blades (dense mat)
const MID_BLADE_SPACING   = 5.0;   // px between mid blades
const FRONT_BLADE_SPACING = 9.0;   // px between front blades (tall tufts)

const BACK_HEIGHT  = [8, 20];      // min,max back blade height (px)
const MID_HEIGHT   = [14, 32];     // min,max mid blade height
const FRONT_HEIGHT = [24, 52];     // min,max front blade height (tall accents)

// Green palette per layer (base→tip). Darker fantasy greens throughout
// (Chloe 2026-07-23) so the grass reads as deep meadow, not sunlit
// lawn. The grass sits BEHIND the dim, so it needs enough luminance to
// survive the ~40-50% dark overlay — dark green but not so dark it
// vanishes. A few dry blades get a warm straw tint for variation.
const BACK_COLORS  = ['#101e0a', '#15280e', '#1a3212'];
const MID_COLORS   = ['#1e3414', '#26401a', '#2c4a1f'];
const FRONT_COLORS = ['#305018', '#3a5e1e', '#426824'];
const DRY_TINT     = ['#6e5e2a', '#847038'];   // dry straw tips

const DRY_CHANCE_FRONT = 0.12;   // ~12% of front blades are dry/seeded

// Sway: per-blade sine on its own phase. Taller blades bend more
// (wind has more leverage on a long blade), front layer sways a
// touch faster (closer to the gust). The speed (frequency) was bumped
// up so the blades oscillate a touch quicker (Chloe 2026-07-23:
// "0.15" feel), while the amplitude was reduced so each arc is
// NARROWER — quicker little shivers, not wide sweeps.
const SWAY_FREQ_BACK  = 0.0013;
const SWAY_FREQ_MID   = 0.0017;
const SWAY_FREQ_FRONT = 0.0022;
const SWAY_AMP_PX     = 0.08;    // tip travel per px of height (narrow shiver)

// How tall the host/canvas is. Blades root at the very bottom and
// grow upward, so this must clear the tallest front blade. 64px is
// enough for FRONT_HEIGHT max (52) + sway + a little soil cover.
const HOST_HEIGHT = 64;

// Build the blade list for one layer. Blades are spaced evenly
// across the width with a small per-blade jitter so the row never
// reads as a comb. Each blade carries its own random height, curve
// lean, sway phase, and color pick.
function buildLayer(w, spacing, heightRange, colors, freq) {
  const blades = [];
  const count = Math.max(1, Math.ceil(w / spacing) + 2);
  for (let i = 0; i < count; i++) {
    const baseX = (i / count) * w + (Math.random() - 0.5) * spacing * 0.9;
    const height = heightRange[0] + Math.random() * (heightRange[1] - heightRange[0]);
    // Static lean: each blade has a resting curve direction (grass
    // rarely stands perfectly vertical — it folds one way).
    const lean = (Math.random() - 0.5) * height * 0.22;
    blades.push({
      baseX,
      height,
      lean,
      swayPhase: Math.random() * Math.PI * 2,
      swayFreq: freq * (0.8 + Math.random() * 0.4),
      // base width tapers with height so tall blades aren't stubby
      baseHalfW: 0.7 + Math.random() * 0.9 + (height / FRONT_HEIGHT[1]) * 0.5,
      colorBase: colors[(Math.random() * colors.length) | 0],
      colorTip:  colors[(Math.random() * colors.length) | 0],
    });
  }
  return blades;
}

// Draw a single blade as a tapered, curved leaf shape anchored at
// (baseX, baseY). The blade is a closed path: base-left → curve up
// the left edge to the tip → curve down the right edge to base-
// right. A vertical gradient (dark base → lighter tip) is applied
// per blade so the tip catches light. Sway offsets the tip + mid.
function drawBlade(ctx, b, baseY, time, dryTip) {
  // Sway: sine on the blade's own phase. Taller blades sway more.
  const sway = Math.sin(time * b.swayFreq + b.swayPhase) * b.height * SWAY_AMP_PX;
  const tipX = b.baseX + b.lean + sway;
  const tipY = baseY - b.height;
  // Mid control points sit at ~half height, offset partway toward
  // the lean so the curve is smooth (not a kink).
  const midX = b.baseX + b.lean * 0.5 + sway * 0.5;
  const midY = baseY - b.height * 0.5;
  const hw = b.baseHalfW;

  // Per-blade vertical gradient: dark soil-end → lighter sun-end.
  const grad = ctx.createLinearGradient(b.baseX, baseY, tipX, tipY);
  grad.addColorStop(0, b.colorBase);
  grad.addColorStop(1, dryTip || b.colorTip);
  ctx.fillStyle = grad;

  ctx.beginPath();
  ctx.moveTo(b.baseX - hw, baseY);
  ctx.quadraticCurveTo(midX - hw * 0.55, midY, tipX, tipY);
  ctx.quadraticCurveTo(midX + hw * 0.55, midY, b.baseX + hw, baseY);
  ctx.closePath();
  ctx.fill();
}

// Create the grass system over a host element. Mounts a canvas
// sized to the host, builds the three blade layers, runs the RAF
// loop. Returns a controller with start()/stop()/destroy().
export function createTitleGrass(host) {
  if (!host) return null;
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-title-grass';
  canvas.setAttribute('aria-hidden', 'true');
  // The canvas fills the bottom host. Pointer-events none so the
  // menu + corner Wupi trigger stay clickable through it.
  canvas.style.cssText =
    'position:absolute;inset:0;width:100%;height:100%;pointer-events:none;';
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let back = [];
  let mid = [];
  let front = [];
  // Pre-pick which front blades are dry (seeded/straw tips) so the
  // tint is stable across frames, not flickering.
  let dryMap = new Map();
  let raf = 0;
  let running = false;
  let dpr = 1;
  let cssW = 0;
  let cssH = 0;
  let reducedMotion = false;

  function resize() {
    dpr = window.devicePixelRatio || 1;
    cssW = host.clientWidth;
    cssH = host.clientHeight;
    canvas.width = Math.max(1, Math.floor(cssW * dpr));
    canvas.height = Math.max(1, Math.floor(cssH * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    // Rebuild blades on resize so density tracks the new width.
    back  = buildLayer(cssW, BACK_BLADE_SPACING,  BACK_HEIGHT,  BACK_COLORS,  SWAY_FREQ_BACK);
    mid   = buildLayer(cssW, MID_BLADE_SPACING,   MID_HEIGHT,   MID_COLORS,   SWAY_FREQ_MID);
    front = buildLayer(cssW, FRONT_BLADE_SPACING, FRONT_HEIGHT, FRONT_COLORS, SWAY_FREQ_FRONT);
    // Stable dry-tip assignment for front blades.
    dryMap = new Map();
    for (const b of front) {
      if (Math.random() < DRY_CHANCE_FRONT) {
        dryMap.set(b, DRY_TINT[(Math.random() * DRY_TINT.length) | 0]);
      }
    }
  }

  function frame(t) {
    if (!running) return;
    ctx.clearRect(0, 0, cssW, cssH);
    const baseY = cssH;   // blades root at the bottom edge of the host
    // Back→front draw order for depth.
    for (const b of back)  drawBlade(ctx, b, baseY, t, null);
    for (const b of mid)   drawBlade(ctx, b, baseY, t, null);
    for (const b of front) drawBlade(ctx, b, baseY, t, dryMap.get(b) || null);
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

  function onVisibility() {
    if (document.hidden) stop();
    else if (!reducedMotion) start();
  }
  const onResize = () => resize();

  reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  resize();
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('resize', onResize);
  // ResizeObserver: re-size whenever the HOST changes size, not just on
  // window resize. This is the load-bearing fix for the 0×0-at-startup
  // timing bug — createTitleGrass runs while #fable is still hidden (the
  // init-time showScreen('title') fires before openFable adds .show), so
  // the host measures 0×0 at first resize() and only gains real size once
  // the app is revealed. window 'resize' never fires for that, but a
  // ResizeObserver on the host does. Robust against any future path that
  // unhides the host (screen swap, CSS transition settle, parent reveal).
  const ro = new ResizeObserver(() => resize());
  ro.observe(host);
  // Reduced motion: draw one static frame, don't run the RAF loop.
  if (reducedMotion) {
    ctx.clearRect(0, 0, cssW, cssH);
    const baseY = cssH;
    for (const b of back)  drawBlade(ctx, b, baseY, 0, null);
    for (const b of mid)   drawBlade(ctx, b, baseY, 0, null);
    for (const b of front) drawBlade(ctx, b, baseY, 0, dryMap.get(b) || null);
  } else {
    start();
  }

  return {
    start,
    stop,
    destroy() {
      stop();
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('resize', onResize);
      ro.disconnect();
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
      back = mid = front = [];
      dryMap.clear();
    },
  };
}
