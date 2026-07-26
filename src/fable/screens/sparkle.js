// =============================================================
// SPARKLE — tiny white twinkles sitting ON the FABLE title text.
//
// The sparkles are pinned to actual TEXT pixels of the title PNG, not
// spread across the whole image box. We achieve that by drawing the
// <img> into an offscreen canvas once (per layout), reading its alpha
// channel, collecting the opaque (text) pixels, and anchoring each
// sparkle to a randomly-chosen text pixel. So glints land ON the
// lettering strokes, not floating in the empty box around them.
//
// Tuned subtle: white, small radii, low average opacity, slow twinkle.
// Pure ambient — no pointer reactivity.
//
// LIFECYCLE: identical contract to the other ambient systems
// (particles/grass/leaves/clouds). Created FRESH per title show() and
// fully torn down on hide()/close (cancelAnimationFrame + canvas
// removed + listeners off) so nothing leaks across screen changes or
// exit/relaunch. The RAF self-pauses on document.hidden to match the
// OS shell's discipline. Wired into title.js's _startAmbient/_stopAmbient.
// =============================================================

// Tunables. Counts kept small + radii tiny so the effect is genuinely
// subtle — these read as fine dust catching light on the lettering, not
// foreground glints. White only (per the directive) so they read as
// reflected sparkle on the gold strokes.
const SPARKLE_COUNT = 24;
const SPARKLE_COLOR = 'rgba(255, 255, 255, ';   // pure white

// Build one sparkle anchored to a TEXT pixel. textPoint is {x,y} already in
// CSS-px image space (sampled from the alpha mask). Everything timing-related
// is per-point so the field never twinkles in unison.
function makeSparkle(textPoint) {
  return {
    x: textPoint.x,
    y: textPoint.y,
    // Tiny radii — fine glints ON the stroke, not foreground fireflies.
    r: 0.4 + Math.random() * 0.8,
    // Independent twinkle phase + period so each point blinks on its own
    // rhythm (2.5–5.5s per full cycle — slow, so it reads as gentle shimmer).
    twinklePhase: Math.random() * Math.PI * 2,
    twinklePeriod: 2500 + Math.random() * 3000,
    // Mostly-dim baseline with a modest peak. Keeps average brightness LOW
    // so the gold title stays the focus and the white reads as a subtle
    // sheen, not a strobe.
    baseOp: 0.04 + Math.random() * 0.14,
    amp: 0.16 + Math.random() * 0.24,
    // Flare scheduling (the subtle "alive" layer). Each sparkle occasionally
    // brightens a touch for a few hundred ms. nextFlare is absolute
    // ms-from-start; re-scheduled after each flare.
    nextFlare: 1500 + Math.random() * 8000,
    flareStart: -1,                          // -1 = not currently flaring
    flareDur: 400 + Math.random() * 400,
    flareGap: 3000 + Math.random() * 8000,   // gap between flares
  };
}

// Create the sparkle system over the wordmark host (the element containing
// the <img>). Mounts a canvas sized to the host (= the image's rendered
// size) and runs the RAF loop. Returns a controller with start()/stop()/
// destroy(). Destroy MUST be called on close (the load-bearing reset
// against the relaunch bug) — title.js does this via _stopAmbient.
export function createTitleSparkle(host) {
  if (!host) return null;
  const img = host.querySelector('img.fable-title-img');
  if (!img) return null;
  const canvas = document.createElement('canvas');
  canvas.className = 'fable-title-sparkle';
  canvas.setAttribute('aria-hidden', 'true');
  // Overlay the <img> exactly (host shrink-wraps the image). Sits ABOVE the
  // image so glints read on top of the lettering. pointer-events none so the
  // title (and any future interaction) stays unblocked.
  canvas.style.cssText =
    'position:absolute;inset:0;width:100%;height:100%;z-index:1;pointer-events:none;';
  host.appendChild(canvas);
  const ctx = canvas.getContext('2d');

  let sparkles = [];
  let textPixels = [];          // [{x,y}] opaque text pixels in CSS-px image space
  let raf = 0;
  let running = false;
  let startTime = 0;
  let dpr = 1;

  // Sample the title <img>'s alpha channel to collect TEXT pixels (opaque
  // pixels = the lettering strokes). Drawn to an offscreen canvas at image
  // natural resolution, then coordinates are scaled back to CSS px. Only
  // called when the image is fully loaded (img.complete && naturalWidth>0).
  function sampleTextPixels() {
    if (!img.complete || !img.naturalWidth) return;
    const off = document.createElement('canvas');
    off.width = img.naturalWidth;
    off.height = img.naturalHeight;
    const octx = off.getContext('2d');
    try {
      octx.drawImage(img, 0, 0);
    } catch (e) {
      return;   // tainted canvas or not ready; leave textPixels empty
    }
    const data = octx.getImageData(0, 0, off.width, off.height).data;
    // Scale factor: natural px → CSS px (the canvas draws in CSS px space).
    const sx = host.clientWidth / off.width;
    const sy = host.clientHeight / off.height;
    const pts = [];
    // Step every few px for density control (collecting every opaque px would
    // be thousands of points — overkill; we just need a representative pool to
    // sample anchors from).
    const STEP = 3;
    for (let y = 0; y < off.height; y += STEP) {
      for (let x = 0; x < off.width; x += STEP) {
        const a = data[(y * off.width + x) * 4 + 3];
        if (a > 128) pts.push({ x: x * sx, y: y * sy });
      }
    }
    textPixels = pts;
  }

  // (Re)lay sparkles across the current text pixels. If we have no text
  // pixels yet (image not loaded), defer — the ResizeObserver / a later
  // frame will catch it once the image is ready.
  function laySparkles() {
    if (textPixels.length === 0) return;
    sparkles = [];
    for (let i = 0; i < SPARKLE_COUNT; i++) {
      const p = textPixels[(Math.random() * textPixels.length) | 0];
      sparkles.push(makeSparkle(p));
    }
  }

  // Size the canvas to the host (== image) + (re)sample text pixels + (re)lay
  // sparkles. Called on init + resize. Re-laying on every resize is cheap
  // (24 points) and guarantees glints track the lettering if the layout
  // changes (e.g. the image reflows).
  function resize() {
    dpr = window.devicePixelRatio || 1;
    const w = host.clientWidth;
    const h = host.clientHeight;
    canvas.width = Math.max(1, Math.floor(w * dpr));
    canvas.height = Math.max(1, Math.floor(h * dpr));
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    if (w === 0 || h === 0) return;   // not laid out yet; wait for ResizeObserver
    sampleTextPixels();
    laySparkles();
  }

  // One frame. For each sparkle: compute the twinkle opacity (sine), check
  // for an active/triggered flare, and draw a soft radial glint. Flares use
  // a sin(0→π) envelope so they ease in AND out (no pop).
  function frame(t) {
    if (!running) return;
    if (!startTime) startTime = t;
    const elapsed = t - startTime;
    const w = host.clientWidth;
    const h = host.clientHeight;
    ctx.clearRect(0, 0, w, h);
    for (const s of sparkles) {
      // Twinkle: opacity oscillates between baseOp and baseOp + amp.
      const tw = 0.5 + 0.5 * Math.sin((elapsed / s.twinklePeriod) * Math.PI * 2 + s.twinklePhase);
      let op = s.baseOp + s.amp * tw;
      let rad = s.r;
      // Flare layer. Trigger when scheduled, then ride a sin envelope for
      // the duration, then re-schedule the next flare.
      let flareEnv = 0;
      if (s.flareStart >= 0) {
        const f = (elapsed - s.flareStart) / s.flareDur;
        if (f >= 1) {
          s.flareStart = -1;
          s.nextFlare = elapsed + s.flareGap;
        } else {
          flareEnv = Math.sin(f * Math.PI);   // 0 → 1 → 0
        }
      } else if (elapsed >= s.nextFlare) {
        s.flareStart = elapsed;
      }
      if (flareEnv > 0) {
        op = Math.min(0.85, op + flareEnv * 0.45);
        rad = s.r * (1 + flareEnv * 0.6);
      }
      // Soft radial glint with a white glow falloff.
      const grad = ctx.createRadialGradient(s.x, s.y, 0, s.x, s.y, rad * 4);
      grad.addColorStop(0, SPARKLE_COLOR + op + ')');
      grad.addColorStop(0.4, SPARKLE_COLOR + (op * 0.5) + ')');
      grad.addColorStop(1, SPARKLE_COLOR + '0)');
      ctx.fillStyle = grad;
      ctx.beginPath();
      ctx.arc(s.x, s.y, rad * 4, 0, Math.PI * 2);
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
    // Reset startTime so the next start re-bases elapsed (avoids a huge
    // elapsed jump after a long pause that would skip all scheduled flares).
    startTime = 0;
  }

  // Pause on hidden (alt-tab / minimize) — same RAF discipline as the OS
  // shell + the other ambient systems.
  function onVisibility() {
    if (document.hidden) stop();
    else start();
  }
  const onResize = () => resize();

  resize();
  document.addEventListener('visibilitychange', onVisibility);
  window.addEventListener('resize', onResize);
  // ResizeObserver: the host measures 0×0 at first resize (createTitleSparkle
  // runs while #fable is still hidden), so window 'resize' never fires for
  // the first real sizing — a ResizeObserver on the host does. Matches
  // particles.js/grass.js.
  const ro = new ResizeObserver(() => resize());
  ro.observe(host);
  // If the image wasn't loaded at first resize() (race: <img> still
  // decoding), re-sample once it fires 'load'. Without this the sparkles
  // never lay because textPixels stays empty.
  if (!img.complete) img.addEventListener('load', resize, { once: true });
  start();

  // Destroy: stop the loop, remove listeners, drop the canvas. Called on
  // Fable hide/close so the next show starts clean.
  return {
    start,
    stop,
    destroy() {
      stop();
      document.removeEventListener('visibilitychange', onVisibility);
      window.removeEventListener('resize', onResize);
      ro.disconnect();
      img.removeEventListener('load', resize);
      if (canvas.parentNode) canvas.parentNode.removeChild(canvas);
      sparkles = [];
      textPixels = [];
    },
  };
}
