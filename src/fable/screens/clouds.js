// =============================================================
// CLOUDS — a soft cloud layer drifting across the top horizon of the
// Fable title screen. Replaces the earlier CSS background-gradient
// approach, which produced jagged edges (overlapping radial-gradient
// ellipses always show their puff boundaries) and a flat horizontal-
// strip look (all blobs at similar heights).
//
// EXECUTION MODEL (Prime Directive — near-zero overhead):
// The clouds are drawn ONCE to a wide canvas buffer at setup using
// ctx.filter='blur()' — canvas blur is pixel-perfect smooth, which CSS
// background gradients fundamentally can't achieve. The canvas element
// then drifts left via the existing CSS fableCloudDrift @keyframes
// (translate3d = pure GPU compositor). No RAF loop, no per-frame JS:
// the buffer is static, only the compositor moves it.
//
// ANTI-STRIP: puffs are placed at genuinely varied heights (7%-26% of
// the layer), with dramatically different sizes (85px wisps next to
// 230px billows) and irregular horizontal gaps between clusters — not a
// uniform band. This is what makes it read as a cloud field, not a bar.
//
// SEAMLESS LOOP: the tile pattern (TILE_W wide) is drawn repeated across
// a canvas of width (viewportW + TILE_W). Animating translate from 0 to
// -TILE_W shifts the pattern by exactly one tile → identical → seamless.
//
// LIFECYCLE: drawn fresh on show/resize, canvas removed on destroy.
// =============================================================

// The tile width — the pattern repeats every TILE_W px. Wide (2200px) so
// there's room for SPARSE clusters with real empty-sky GAPS between them.
// A dense tile (the prior 1500px with 13 puffs) read as a horizontal strip
// because ~99% of columns were populated. More width + fewer puffs + gaps
// is what breaks the strip read.
const TILE_W = 2200;

// Cloud puffs defined in TILE-LOCAL coordinates (x in [0, TILE_W),
// y as a fraction of layer height). The layout is deliberately SPARSE +
// IRREGULAR: only 4 clusters across 2200px, each a tight 2-3 puff group,
// separated by wide empty-sky gaps (~250-400px of clear sky between
// clusters). Heights vary a LOT — some clusters near the very top (3%),
// some lower (38%) — so the silhouette is jagged and organic, not a band.
//
// y fractions reach up to ~0.38; since the layer is 26vh tall, max puff
// center ≈ 0.38 × 26vh = 9.9vh. The FABLE heading top is ~9vh, but the
// heading has its own padding + the puffs fade softly, so a puff CENTER at
// 9.9vh still keeps its dense core above the glyphs. Kept under review.
const PUFFS = [
  // ── cluster A: a low dense billow + high wisp (far left) ──
  { x: 90,   y: 0.30, rx: 165, ry: 50, a: 0.55 },
  { x: 175,  y: 0.12, rx: 110, ry: 36, a: 0.42 },
  // ── GAP ~360px clear sky ──
  // ── cluster B: the big feature — large, mid-height, densest ──
  { x: 740,  y: 0.20, rx: 215, ry: 64, a: 0.60 },
  { x: 845,  y: 0.36, rx: 135, ry: 44, a: 0.46 },
  { x: 660,  y: 0.08, rx: 95,  ry: 30, a: 0.38 },
  // ── GAP ~400px clear sky ──
  // ── cluster C: high small puff trio ──
  { x: 1380, y: 0.05, rx: 105, ry: 34, a: 0.44 },
  { x: 1470, y: 0.22, rx: 125, ry: 40, a: 0.42 },
  // ── GAP ~350px clear sky ──
  // ── cluster D: low wisp (right edge, wraps toward A on repeat) ──
  { x: 1920, y: 0.34, rx: 145, ry: 46, a: 0.48 },
  { x: 2010, y: 0.14, rx: 85,  ry: 28, a: 0.36 },
];

// Draw one soft puff. Uses a multi-stop radial gradient for an ultra-
// gradual alpha falloff (no visible edge), PLUS ctx.filter='blur()' on
// the whole draw pass for pixel-perfect smoothness. Together these
// eliminate the jagged elliptical boundaries that plagued the CSS version.
function drawPuff(ctx, cx, cy, rx, ry, alpha) {
  const grad = ctx.createRadialGradient(cx, cy, 0, cx, cy, rx);
  grad.addColorStop(0,    `rgba(242,236,224,${alpha})`);
  grad.addColorStop(0.25, `rgba(240,234,222,${alpha * 0.75})`);
  grad.addColorStop(0.55, `rgba(238,232,220,${alpha * 0.45})`);
  grad.addColorStop(0.8,  `rgba(236,230,218,${alpha * 0.18})`);
  grad.addColorStop(1,    'rgba(234,228,216,0)');
  ctx.fillStyle = grad;
  ctx.beginPath();
  ctx.ellipse(cx, cy, rx, ry, 0, 0, Math.PI * 2);
  ctx.fill();
}

// Create the cloud canvas system over a host element. Draws once, then
// the CSS animation handles the drift. Returns { destroy() }.
export function createCloudLayer(host) {
  if (!host) return null;
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-cloud-canvas';
  canvas.setAttribute('aria-hidden', 'true');
  canvas.style.cssText =
    'position:absolute;top:0;left:0;height:100%;pointer-events:none;will-change:transform;';
  // Drive the seamless-loop translate distance via a CSS custom property
  // so the keyframe stays generic.
  canvas.style.setProperty('--tile-w', TILE_W + 'px');
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let dpr = 1;
  let cssW = 0;
  let cssH = 0;

  // Draw the full cloud field. Called once at setup + on resize.
  // The tile pattern is stamped repeatedly across the canvas width so
  // the CSS translate-by-TILE_W animation loops seamlessly.
  function draw() {
    dpr = window.devicePixelRatio || 1;
    cssW = host.clientWidth + TILE_W;           // wide enough to cover viewport at translate=-TILE_W
    cssH = host.clientHeight;
    canvas.width = Math.max(1, Math.floor(cssW * dpr));
    canvas.height = Math.max(1, Math.floor(cssH * dpr));
    canvas.style.width = cssW + 'px';
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, cssW, cssH);
    // Canvas blur: smooths every puff edge to pixel-perfect softness.
    // Scaled by DPR so the visual blur radius is consistent across
    // HiDPI displays (ctx.filter operates in backing-store px).
    ctx.filter = `blur(${5 * dpr}px)`;
    // Stamp the tile pattern across the full canvas width.
    const numTiles = Math.ceil(cssW / TILE_W) + 1;
    for (let t = 0; t < numTiles; t++) {
      const offsetX = t * TILE_W;
      for (const p of PUFFS) {
        drawPuff(ctx, offsetX + p.x, p.y * cssH, p.rx, p.ry, p.a);
      }
    }
    ctx.filter = 'none';
  }

  // Pause/resume the CSS drift on tab-out. CSS animations are throttled
  // by the browser on hidden tabs, but toggling play-state explicitly
  // guarantees a clean freeze (consistent with particles/grass/leaves).
  function onVisibility() {
    canvas.style.animationPlayState = document.hidden ? 'paused' : 'running';
  }

  draw();
  document.addEventListener('visibilitychange', onVisibility);
  // ResizeObserver: redraw on host resize so the canvas always covers
  // the viewport at the current width. One-time redraw, not per-frame.
  const ro = new ResizeObserver(() => draw());
  ro.observe(host);

  return {
    redraw: draw,
    destroy() {
      document.removeEventListener('visibilitychange', onVisibility);
      ro.disconnect();
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
    },
  };
}
