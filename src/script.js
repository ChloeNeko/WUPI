import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { getVersion } from '@tauri-apps/api/app';
import { initFable, launchFable } from './fable/fable.js';
import { initPrism } from './prism/prism.js';
// Shell-wide rapid-click / double-launch guard (src/shell-guard.js). Importing
// it self-registers the capture-phase click/pointerdown swallower on `document`
// + exposes withShellBusy to wrap the shell-chrome transition entry points
// (home-grid tile launch, dock toggle, restart). Mirrors Fable's flowBusy one
// level higher — covers the whole OS shell, not just Fable's in-app transitions.
import { withShellBusy } from './shell-guard.js';
// Boot-paw sound design ships as authored .mp3 assets (Vite-bundled into
// assets/). Three voices, one each for the three motion classes:
//   INTRO_MOVE_SRC   — the fairy-tour + corner-flight whoosh (movement)
//   INTRO_HOP_SRC    — both hops baked into one clip (hop-1 at ~0.07s,
//                      hop-2 at ~0.78s — the choreography syncs to these)
//   INTRO_FINISH_SRC — the magical landing finale
import INTRO_MOVE_SRC from './assets/introMoveSFX.mp3';
import INTRO_HOP_SRC from './assets/introHopSFX.mp3';
import INTRO_FINISH_SRC from './assets/introFinishSFX.mp3';

const canvas = document.getElementById('aurora-canvas');
const ctx = canvas.getContext('2d');

// Each color code defines the aurora's sky gradient (top→bottom CSS color
// stops), the per-curtain hue/saturation/lightness generators, and a dark UI
// accent triplet (recolors the OS chrome via the --ui-accent* CSS vars — see
// applyTheme). The animate() loop reads `currentPalette`: switching color codes
// re-paints on the next frame.
//
// Curtain color math (solid mode):
//   hue   = hueBase + i*hueStep + sin(time + i) * hueRange
//   light = lightness + (i - 2) * lightStep      // i = curtain 0..4
// A palette may instead give explicit per-strip arrays `curtainHues[5]` +
// `curtainLights[5]` to plant each of the 5 strips on a genuinely different
// shade (the themed palettes do; Vibrant/Rainbow keep the arithmetic above,
// which is byte-identical to the original hardcoded aurora). Either way the
// strips slowly breathe within hueRange. Rainbow mode gives
// every strip its own ROYGBIV band and drifts them together through the full
// 360° over ~90 s. "Vibrant" reproduces the ORIGINAL hardcoded aurora
// exactly (hueStep 0, lightStep 0, sat 100, light 65 → byte-identical hsla);
// it stays visually untouched. New color codes = add an entry here + a
// matching .swatch-* in styles.css.
const COLOR_CODES = {
  Red: {
    // Each strip starts on a different shade of red/pink/red-orange: deep
    // blood red → bright red → crimson pink → vivid pink → red-orange.
    // The bottom two sky stops are dark themed reds so the curtain glow
    // blends into the horizon like Vibrant — NO separate light strip.
    skyGradient: ['#0a0203', '#140406', '#1d0607', '#370c11', '#49121a'],
    mode: 'solid',
    hueBase: 356, hueRange: 8,
    saturation: 82,
    curtainHues:  [348, 358, 332, 320, 12],   // dark-red, red, crimson, pink, red-orange
    curtainLights:[42, 56, 50, 66, 60],
    uiAccent: '#9b2d3f', uiAccentBright: '#d65a72', uiAccentDeep: '#5e1822',
  },
  Green: {
    // Each strip starts on a different green: forest → chartreuse → sea/turquoise
    // → spring/lawn → olive-gold. Hue spans ~46°, lightness ~36%, so the aurora
    // reads as a living canopy rather than one flat green. The bottom two sky
    // stops are dark themed greens so the curtain glow blends into the horizon
    // like Vibrant — NO separate light strip.
    skyGradient: ['#030503', '#070d07', '#0a130b', '#0f331e', '#164128'],
    mode: 'solid',
    hueBase: 140, hueRange: 8,
    saturation: 78,
    curtainHues:  [140, 90, 165, 150, 70],    // forest, chartreuse, sea/turquoise, spring/lawn, olive
    curtainLights:[40, 58, 52, 66, 50],
    uiAccent: '#2d7340', uiAccentBright: '#5ec878', uiAccentDeep: '#1a4828',
  },
  Blue: {
    // Each strip starts on a different blue/purple: midnight → royal/sapphire
    // → cerulean/cornflower → baby/sky → violet. Mixes blues with purple
    // accents per the brief; all 5 strips clearly distinct. The bottom two sky
    // stops are dark themed blues so the curtain glow blends into the horizon
    // like Vibrant — NO separate light strip.
    skyGradient: ['#020308', '#04081a', '#060b24', '#0c163b', '#131f4e'],
    mode: 'solid',
    hueBase: 220, hueRange: 8,
    saturation: 84,
    curtainHues:  [232, 220, 205, 200, 275],  // midnight, royal/sapphire, cerulean/cornflower, baby/sky, violet
    curtainLights:[42, 56, 62, 70, 50],
    uiAccent: '#3458a8', uiAccentBright: '#6e9ce8', uiAccentDeep: '#1e3370',
  },
  Neutral: {
    // Grayscale only (saturation 0): a mixture of white, dark grays, and
    // black. The 5 strips span a wide lightness band (near-white → near-black)
    // so each starts on a clearly different shade. The bottom sky stops are
    // dark grays (~13%/19%) so the curtain glow blends into the horizon like
    // Vibrant — NO separate light strip.
    skyGradient: ['#020203', '#050507', '#0a0a0d', '#222229', '#31313a'],
    mode: 'solid',
    hueBase: 0, hueRange: 0,
    saturation: 0,
    curtainHues:  [0, 0, 0, 0, 0],            // grayscale — hue is irrelevant at sat 0; kept for branch consistency
    curtainLights:[72, 54, 38, 22, 10],       // light gray → near black, each strip distinct
    uiAccent: '#3a3a42', uiAccentBright: '#8a8a96', uiAccentDeep: '#202024',
  },
  Vibrant: {
    // Reproduces the ORIGINAL hardcoded aurora EXACTLY — do not change these
    // values. Vibrant is the project default + the untouched reference look,
    // so it deliberately keeps hueStep/lightStep of 0. Every other color code
    // mirrors this same horizon treatment (curtain glow blends into the bottom
    // skyGradient stops — no separate light band).
    skyGradient: ['#02040a', '#060a17', '#150524', '#2b0b36', '#4a173d'],
    mode: 'solid',
    hueBase: 305, hueRange: 45, hueStep: 0,
    saturation: 100, lightness: 65, lightStep: 0,
    uiAccent: '#b534fa', uiAccentBright: '#ff66b2', uiAccentDeep: '#6a0dad',
  },
  Rainbow: {
    // Each curtain starts on a different ROYGBIV band; all bands drift
    // together through the full 360° over ~90 s (slow + subtle). The hue
    // jitter (hueRange) keeps adjacent strips from blending flat. The bottom
    // two sky stops are a dark neutral violet so the curtain glow blends into
    // the horizon like Vibrant — NO separate light strip.
    skyGradient: ['#040308', '#080612', '#0c0920', '#181236', '#201943'],
    mode: 'rainbow',
    hueBands: [0, 60, 120, 220, 280],
    hueRange: 9, hueDrift: 26.7,
    saturation: 84, lightness: 62, lightStep: 3,
    uiAccent: '#7878a8', uiAccentBright: '#a6a6d4', uiAccentDeep: '#4a4a66',
  },
};

// The live palette; initialized from the persisted theme at boot (see
// `applyTheme` below). Defaults to Vibrant so the canvas paints immediately
// even before the IPC round-trip completes.
let currentPalette = COLOR_CODES.Vibrant;

// CSS-pixel dimensions (all drawing math uses these). The backing store is
// scaled by devicePixelRatio so stars/curtains render at physical-pixel
// resolution on high-DPI / 4K / ultrawide displays instead of being upscaled
// and blurry. This is the "resolution loss" fix.
let width, height;

// Aurora offscreen buffer. The 5 curtains are rendered here ONCE per frame
// with NO blur (cheap fills, no Gaussian pass), then the whole composite is
// blitted to the main canvas through a SINGLE blur(30px). This collapses the
// expensive op from 5x per frame → 1x, which is what fixed the boot-wipe
// stutter (at-rest was OK because the pipeline was warm; the wipe hit cold
// and 5 cold blur passes/frame stuttered). The buffer is DPR-scaled so the
// blur resolves at physical-pixel resolution (no softness on high-DPI).
// Lazily (re)allocated in resize() to match viewport + DPR.
let auroraBuf = null;
let auroraBufCtx = null;

function resize() {
  const dpr = window.devicePixelRatio || 1;
  width = window.innerWidth;
  height = window.innerHeight;
  canvas.width = Math.floor(width * dpr);
  canvas.height = Math.floor(height * dpr);
  canvas.style.width = width + 'px';
  canvas.style.height = height + 'px';
  // Reset transform then re-apply: resize() can fire repeatedly, and the
  // scale accumulates if not reset first.
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.scale(dpr, dpr);
  // (Re)allocate the aurora offscreen buffer at physical-pixel resolution.
  // On first call (boot), this is what makes the boot wipe cheap.
  if (!auroraBuf) {
    auroraBuf = document.createElement('canvas');
    auroraBufCtx = auroraBuf.getContext('2d');
  }
  auroraBuf.width = canvas.width;
  auroraBuf.height = canvas.height;
}
window.addEventListener('resize', resize);
resize();

let mouseX = 0;
let mouseY = 0;
let currentX = 0;
let currentY = 0;

window.addEventListener('mousemove', (e) => {
  mouseX = (e.clientX / width) * 2 - 1;
  mouseY = (e.clientY / height) * 2 - 1;
});

const starCount = 1000;
const stars = Array.from({ length: starCount }, () => {
  const isTwinkling = Math.random() > 0.98;
  // colorIdx indexes STAR_COLORS: drawing buckets stars by color so the
  // context's fillStyle changes ~4×/frame instead of ~1000×.
  const colorIdx = Math.floor(Math.random() * 4);

  return {
    x: Math.random() * width,
    y: Math.random() * height,
    size: Math.random() * 0.9 + 0.4,
    alpha: Math.random() * 0.7 + 0.3,
    isTwinkling: isTwinkling,
    speed: isTwinkling ? (0.0005 + Math.random() * 0.0012) : 0,
    drift: Math.random() * 0.01 + 0.008 + 0.004,
    colorIdx: colorIdx,
  };
});

let time = 0;

// The gradient depends only on the palette + canvas height, both of which
// change rarely (theme switch / resize). Recreating it 60×/sec was pure waste
//: createLinearGradient + 5 addColorStop calls per frame. Rebuilt only when
// `currentPalette` or `height` changes.
let cachedSkyGrad = null;
let cachedSkyHeight = -1;
function skyGradient() {
  if (cachedSkyGrad && cachedSkyHeight === height) return cachedSkyGrad;
  const g = ctx.createLinearGradient(0, 0, 0, height);
  const stops = currentPalette.skyGradient;
  for (let i = 0; i < stops.length; i++) {
    g.addColorStop(i / (stops.length - 1), stops[i]);
  }
  cachedSkyGrad = g;
  cachedSkyHeight = height;
  return g;
}
// Invalidate the cache on resize (height changes → gradient must rebuild).
window.addEventListener('resize', () => { cachedSkyGrad = null; });

// Batching same-color stars into one fillStyle set + grouping alpha into a
// few bands collapses ~1000 state changes/frame into a handful. The visual
// difference is imperceptible (alpha quantized to 8 bands of 0.1).
const STAR_COLORS = ['#ffffff', '#e8f0ff', '#fff4e6', '#ffe6ee'];

// Boot reveal: aurora curtains reveal LEFT-TO-RIGHT (a "wipe" rather than a
// global opacity ramp). The wipe is BOTH an aesthetic choice AND a perf fix.
//
// Two gates work together:
//   auroraIntensity   — the overall fade-in (0 → 1 over AURORA_RAMP_MS).
//   auroraRevealX     — the left-to-right wipe position (px).
//
// The boot-wipe stutter fix (2 layered wins):
// 1. Offscreen buffer: 5 curtains rendered with NO blur, then ONE blurred
//    blit to main → 5x fewer Gaussian passes per frame.
// 2. Interpolated blur radius (10px → 30px with intensity): Gaussian cost
//    scales roughly with radius², so blur(10px) at wipe-start is ~9x cheaper
//    than blur(30px). The visual blooms as it reveals.
// Both fire only during the wipe. At rest the buffer redraws live with
// the full 30px blur, identical to the locked aesthetic.
//
// NOTE: the buffer is NEVER frozen during the wipe. An earlier version held
// a snapshot (auroraBufFrozen) to skip per-frame curtain redraws, but that
// caused a visible "frozen then resumes" color/shape snap when the freeze
// released — `time` advanced while the buffer didn't, so the curtain waves
// + hues jumped forward in their cycle. The single-blur-pass optimization
// above is enough on its own; the curtain fills are cheap path operations.
let auroraIntensity = 0;
let auroraRampStart = 0;
const AURORA_RAMP_MS = 900;
// The wipe runs concurrently with the intensity ramp. Shorter than RAMP_MS
// so the wipe front finishes ahead of the full-opacity settle.
let auroraRevealX = 0;          // current wipe x (px, CSS px)
let auroraRevealStart = 0;      // 0 = not yet armed
const AURORA_WIPE_MS = 950;
// Blur radius floor/ceiling (CSS px). Gaussian cost ~ radius².
const AURORA_BLUR_FLOOR = 10;
const AURORA_BLUR_CEIL = 30;

function animate() {
  if (auroraRampStart && auroraIntensity < 1) {
    auroraIntensity = Math.min(1, (performance.now() - auroraRampStart) / AURORA_RAMP_MS);
  }
  if (auroraRevealStart && auroraRevealX < width + 300) {
    // Ease-in-out so the wipe starts slow, accelerates, settles — reads as
    // "fluid" rather than a constant mechanical sweep.
    const t = Math.min(1, (performance.now() - auroraRevealStart) / AURORA_WIPE_MS);
    const eased = t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
    auroraRevealX = -150 + eased * (width + 600);
  }
  currentX += (mouseX - currentX) * 0.25;
  currentY += (mouseY - currentY) * 0.25;

  // Sky (cached gradient: see skyGradient()).
  ctx.globalCompositeOperation = 'source-over';
  ctx.globalAlpha = 1.0;
  ctx.fillStyle = skyGradient();
  ctx.fillRect(0, 0, width, height);

  // Stars: update positions/twinkle, then draw bucketed by color+alpha-band
  // so the context state changes once per bucket, not once per star.
  const px = currentX * 16;
  const py = currentY * 16;
  // buckets[colorIdx][alphaBand] = [{x,y,size}, ...]
  const buckets = [[[],[],[],[],[],[],[],[]],[[],[],[],[],[],[],[],[]],[[],[],[],[],[],[],[],[]],[[],[],[],[],[],[],[],[]]];
  for (let i = 0; i < stars.length; i++) {
    const s = stars[i];
    if (s.isTwinkling) {
      s.alpha += s.speed;
      if (s.alpha > 1 || s.alpha < 0.15) s.speed = -s.speed;
    }
    s.y -= s.drift;
    if (s.y < 0) s.y = height;
    const band = Math.min(7, Math.max(0, Math.floor(Math.abs(s.alpha) * 8)));
    buckets[s.colorIdx][band].push(s.x + px * s.size, s.y + py * s.size, s.size);
  }
  for (let c = 0; c < STAR_COLORS.length; c++) {
    ctx.fillStyle = STAR_COLORS[c];
    for (let b = 0; b < 8; b++) {
      const pts = buckets[c][b];
      if (pts.length === 0) continue;
      ctx.globalAlpha = (b + 0.5) / 8;
      for (let k = 0; k < pts.length; k += 3) {
        ctx.fillRect(pts[k], pts[k + 1], pts[k + 2], pts[k + 2]);
      }
    }
  }
  ctx.globalAlpha = 1.0;

  // Aurora borealis: 5 layered, independently-hued curtains. Each curtain
  // gets its own hue oscillation. The soft bloom (blur 30px) IS the look —
  // by design: do NOT collapse the visual into one fill.
  //
  // PERF ARCHITECTURE (the boot-wipe stutter fix):
  // The OLD code set ctx.filter='blur(30px)' and called ctx.fill() 5 times
  // per frame — 5 separate Gaussian blur passes. At rest that was tolerable
  // (warm pipeline), but the boot wipe fired into a cold pipeline and 5 cold
  // blur passes/frame stuttered visibly.
  //
  // The NEW code renders all 5 curtains to an offscreen buffer (auroraBuf)
  // with NO blur (cheap path fills), then blits the composite to the main
  // canvas through a SINGLE blur(30px). 5x fewer Gaussian passes per frame,
  // constant cost whether booting or at rest. The boot wipe is then a cheap
  // source-crop on the drawImage (only the revealed x-range is sampled),
  // so the blur also processes less data during the wipe — doubly cheap.
  if (auroraIntensity > 0.001 && auroraBufCtx) {
    const dpr = window.devicePixelRatio || 1;
    const curtains = 5;
    const baseCenterY = height * 0.42;

    // ── Pass 1: render curtains to offscreen buffer every frame (live
    //    animation through the wipe — the "frozen snapshot" optimization
    //    was REMOVED because it caused a visible color/shape snap when the
    //    freeze released). NO blur here; Pass 2 blurs the composite once.
    auroraBufCtx.setTransform(1, 0, 0, 1, 0, 0);
    auroraBufCtx.clearRect(0, 0, auroraBuf.width, auroraBuf.height);
    auroraBufCtx.scale(dpr, dpr);
    auroraBufCtx.globalCompositeOperation = 'source-over';

    // Per-curtain alpha scales with intensity so the fade-in is driven both
    // by the interpolated blur radius (10→30) AND by alpha. The wipe then
    // sweeps the composite left-to-right.
    const a = 0.18 * auroraIntensity;
    const pal = currentPalette;
    const sat = pal.saturation;
    const isRainbow = pal.mode === 'rainbow';
    // Rainbow drift: the bands rotate through the full 360° together. `time`
    // advances 0.0025/frame ≈ 0.15/s, so a full cycle = 360 / (hueDrift*0.15)
    // ≈ 90 s at hueDrift 26.7 — slow + subtle.
    const rainbowDrift = isRainbow ? (time * pal.hueDrift) : 0;
    for (let i = 0; i < curtains; i++) {
      const speed = time * (0.1 + i * 0.04);
      const thickness = 45 + i * 15;
      const yOffset = (i - (curtains / 2)) * 12;
      const activeCenterY = baseCenterY + yOffset;

      auroraBufCtx.beginPath();
      for (let x = -150; x <= width + 150; x += 40) {
        const y = activeCenterY
                + Math.sin(x * 0.0015 + speed + i * 2.3) * 85
                + Math.cos(x * 0.0008 - speed) * 45
                - thickness;
        if (x === -150) auroraBufCtx.moveTo(x, y);
        else auroraBufCtx.lineTo(x, y);
      }
      for (let x = width + 150; x >= -150; x -= 40) {
        const y = activeCenterY
                + Math.sin(x * 0.0015 + speed + i * 2.3) * 85
                + Math.cos(x * 0.0008 - speed) * 45
                + thickness;
        auroraBufCtx.lineTo(x, y);
      }
      auroraBufCtx.closePath();

      // Per-curtain color. Each strip must start on a DIFFERENT shade and
      // breathe slowly within hueRange:
      //  - solid: a palette may pin each strip via `curtainHues[i]` +
      //    `curtainLights[i]` (the themed palettes do — distinct shades). When
      //    it doesn't, the arithmetic fallback runs (base + i*step), which for
      //    Vibrant (hueStep 0 + lightStep 0 + sat 100 + light 65) collapses to
      //    the ORIGINAL hardcoded hsla byte-for-byte. Either way a slow sin
      //    jitter rides on top.
      //  - rainbow: each strip's own ROYGBIV band, all drifting together.
      let hue, light;
      if (isRainbow) {
        hue = (pal.hueBands[i] + rainbowDrift + Math.sin(time * 1.0 + i) * pal.hueRange + 720) % 360;
        light = pal.lightness + (i - 2) * pal.lightStep;
      } else if (pal.curtainHues) {
        hue = (pal.curtainHues[i] + Math.sin(time * 1.0 + i) * pal.hueRange + 720) % 360;
        light = pal.curtainLights[i];
      } else {
        hue = (pal.hueBase + i * pal.hueStep + Math.sin(time * 1.0 + i) * pal.hueRange + 720) % 360;
        light = pal.lightness + (i - 2) * pal.lightStep;
      }
      auroraBufCtx.fillStyle = `hsla(${hue}, ${sat}%, ${light}%, ${a})`;
      auroraBufCtx.fill();
    }

    // ── Bottom horizon. No separate glow band is drawn — every theme now
    //    takes the Vibrant approach: the curtain glow blends into the bottom
    //    skyGradient stops (dark themed hues, L≈13%→19%), which the sky pass
    //    already painted in `source-over`. No per-palette `horizon` key exists
    //    anymore; the old near-white strip-of-light band was removed for every
    //    non-Vibrant color code.

    // ── Pass 2: blit the composite with ONE interpolated blur pass.
    // Gaussian cost ~ radius², so scaling the radius 10→30 with intensity
    // makes the early wipe frames ~9x cheaper than the locked 30px. The
    // visual blooms as it reveals. At rest (intensity=1) the full 30px
    // returns and the look is identical to the locked aesthetic.
    ctx.globalCompositeOperation = 'screen';
    const blurPx = AURORA_BLUR_FLOOR +
      (AURORA_BLUR_CEIL - AURORA_BLUR_FLOOR) * auroraIntensity;
    ctx.filter = `blur(${blurPx.toFixed(1)}px)`;

    const wipeXCss = Math.min(Math.max(auroraRevealX, 0), width);
    const srcW = Math.floor(wipeXCss * dpr);
    if (srcW > 0) {
      ctx.drawImage(auroraBuf, 0, 0, srcW, auroraBuf.height, 0, 0, wipeXCss, height);
    }

    ctx.filter = 'none';
    ctx.globalCompositeOperation = 'source-over';
  } // end auroraIntensity > 0.001 cost gate
  time += 0.0025;
  // Don't schedule the next frame while paused: see `paused` + the
  // visibility/focus handlers below. The canvas RAF is the app's dominant
  // idle CPU/GPU cost; pausing it is what makes Sleep "barely noticeable"
  // AND what stops the lag when the window is covered/minimized.
  if (!paused) requestAnimationFrame(animate);
}

// Render loop control. `paused` is set by FOUR independent signals so the
// expensive RAF loop stops the moment the canvas isn't visible to the user:
//   0. BOOT GATE: `bootDone` is false until setupBootSplash()'s
//      revealAfterLand() runs (~0.5s after the paw lands). startLoop()
//      refuses to start while it's false, so no early focus/visibility event
//      can paint stars behind the boot paw. The canvas stays dormant while
//      the paw is hopping so the desktop is the only thing behind it.
//   1. `canvas-pause` event from Rust (system_menu power_sleep).
//   2. `document.visibilitychange` → hidden (alt-tab, minimize, another app
//      fully covering the window). The standard browser RAF throttle isn't
//      enough: WebView2 still fires RAF in some hidden states, and even a
//      throttled RAF re-runs the full animate() body.
//   3. `window.blur` (focus lost to another app) as a belt-and-suspenders
//      fallback when visibilitychange doesn't fire (e.g. another window
//      dragged over this one without minimizing).
// Resume mirrors all three. The animate() loop self-gates on `paused`.
let paused = true;
let bootDone = false;

// ── FABLE ENTRY (fable.exe / #fable) ──────────────────────────
// When active, the app SKIPS the entire OS boot ceremony — the 1s blank, the
// paw entry/hop/flight animation, the 8s loading screen, the aurora reveal —
// and launches straight into Fable, also skipping Fable's own fog gate + boot
// transition (see openFable's FABLE_ENTRY branch). The net effect: landing on
// Fable's title screen in well under a second — the fable.exe launcher's whole
// purpose, and handy for dev iteration (no ~16s of unskippable cinematics).
//
// PRODUCTION LAUNCHER (fable.exe): this IS true in shipped fable.exe. fable.exe
// is a second launcher binary whose Rust setup() builds the main window with
// the URL `wupi.html#fable` (the FABLE_ENTRY marker) instead of `wupi.html`.
// wupi.exe loads plain `wupi.html` → FABLE_ENTRY is false there → normal OS
// boot. The model still loads (boot_load_model fires here) + the canvas gate
// opens (bootDone flips here); only the cinematic choreography is bypassed.
//
// Trigger forms (all equivalent): #fable (the production marker — bare hash),
// ?fable (query), OR the legacy dev forms #dev=fable / ?dev=fable kept for
// `npm run dev` iteration (devUrl "http://localhost:1420/wupi.html#dev=fable").
// The hash form is preferred — some Tauri versions strip a query on devUrl /
// under the custom protocol but preserve the hash.
const FABLE_ENTRY = (() => {
  try {
    // True if `fable` is present (bare #fable → value "") or `dev=fable`.
    const has = (p) => p && (p.get('fable') !== null || p.get('dev') === 'fable');
    if (has(new URLSearchParams(window.location.search))) return true;
    const h = window.location.hash.replace(/^#/, '');
    if (h === 'fable') return true;          // bare #fable
    return has(new URLSearchParams(h));      // #fable=… / #dev=fable
  } catch (_) { return false; }
})();

// DEV SHORTCUT (?dev=preview or #dev=preview): a PURE-FRONTEND layout preview
// that skips the OS boot, skips Fable's title, skips ALL backend/IPC (no model
// load, no API, no fable_send), and drops straight into the chat stage with 4
// hardcoded placeholder messages + 2 placeholder portraits. Purpose: iterate
// on the VN chat UI visually without launching a real game. Refresh lands in
// the stage instantly. False in production (query/hash absent under Tauri).
const DEV_PREVIEW_SHORTCUT = (() => {
  try {
    if (new URLSearchParams(window.location.search).get('dev') === 'preview') return true;
    const h = window.location.hash.replace(/^#/, '');
    return new URLSearchParams(h).get('dev') === 'preview';
  } catch (_) { return false; }
})();

// The running app version, resolved by runBootGate() via getVersion() before
// the cosmetic terminal stream starts. Referenced by the first TERMINAL_LINES
// entry so the boot banner reflects the real version instead of a hardcoded
// stale one. Stays 'unknown' if getVersion() fails (graceful).
let bootVersion = 'unknown';

function startLoop() {
  // Boot dormancy: refuse to start until setupBootSplash()'s revealAfterLand()
  // opens the gate. Without this, an early focus/visibility event during the
  // 5s paw hop would un-pause and paint stars behind the boot paw.
  if (!bootDone) return;
  if (paused) { paused = false; requestAnimationFrame(animate); }
}

// Tauri emits these from system_menu power_sleep / power_wake. Guard with
// .catch so a dev preview outside Tauri doesn't throw on the listener.
listen('canvas-pause', () => { paused = true; }).catch(() => {});
listen('canvas-resume', () => { startLoop(); }).catch(() => {});

// Pause when the page is hidden (alt-tab / minimize / tab switch). This is
// THE fix for "lag when another app covers the window": without it the RAF
// keeps running the full animate() body at full speed even when nothing's
// visible. Resume on visible.
document.addEventListener('visibilitychange', () => {
  if (document.hidden) {
    paused = true;
  } else {
    startLoop();
  }
});

// Pause when the window loses focus (another app comes to the foreground).
// Belt-and-suspenders: visibilitychange covers most cases, but blur fires
// for "another window dragged over this one" where the page isn't technically
// hidden. Resume only if also visible + not manually paused via power_sleep.
window.addEventListener('blur', () => { paused = true; });
window.addEventListener('focus', () => {
  if (!document.hidden) startLoop();
});

// NOTE: animate() is NOT kicked off here at module-load time. The canvas is
// dormant during the boot paw phase (paused = true). setupBootSplash()'s
// revealAfterLand() opens bootDone + calls startLoop() ~0.5s after the paw
// lands — the first animate() frame paints sky + stars only, then the aurora
// blooms in over AURORA_RAMP_MS once the ramp is armed. Calling animate() at
// module load would paint stars behind the boot paw (the "background shows
// with the circle" bug) AND fight the boot gate.

// "WUPI" title: live AI-status indicator
// The title reflects the live state of the chat model. Wupi chat is LOCAL-ONLY
// (2026-08-08 override): the local model (Gemma 4 E4B) is ALWAYS the chat
// model — the API is
// reserved exclusively for Fable narration (a separate path). Three states:
//   - 'idle'    : connected, not generating → steady medium white glow
//   - 'offline' : boot pre-load or model error → fast red flash
//   - 'typing'  : local model actively generating tokens → subtle random
//                 white pulse spurts driven by a jittered setTimeout loop
//                 (CSS can't do random timing).
//
// State inputs:
//   1. The `model-status` Tauri event: Rust emits ready/error/no_model at
//      boot; this is the offline/idle authority.
//   2. The chat IIFE's setGenerating() flag: bridges to 'typing'/'idle'.
const osTitleEl = document.querySelector('.os-title');
let titleState = 'idle';      // 'idle' | 'offline' | 'typing'
let titleFlickerTimer = null;  // the setTimeout handle for the typing pulse

function applyTitleClass() {
  if (!osTitleEl) return;
  osTitleEl.classList.remove('is-offline', 'is-typing');
  if (titleState === 'offline') osTitleEl.classList.add('is-offline');
  else if (titleState === 'typing') osTitleEl.classList.add('is-typing');
}

// The random "typing" pulse: toggles .title-flicker on a jittered timer so
// the glow bursts feel organic (like someone actually typing). ON 80-200ms,
// OFF 120-500ms, re-rolled each cycle. Stops when state leaves 'typing'.
function scheduleNextFlicker() {
  if (titleState !== 'typing' || !osTitleEl) return;
  const isOn = osTitleEl.classList.contains('title-flicker');
  const delay = isOn
    ? 80 + Math.random() * 120   // ON duration: 80-200ms
    : 120 + Math.random() * 380; // OFF duration: 120-500ms
  titleFlickerTimer = setTimeout(() => {
    if (titleState !== 'typing') return;
    osTitleEl.classList.toggle('title-flicker');
    scheduleNextFlicker();
  }, delay);
}

function stopFlicker() {
  if (titleFlickerTimer) { clearTimeout(titleFlickerTimer); titleFlickerTimer = null; }
  if (osTitleEl) osTitleEl.classList.remove('title-flicker');
}

function setTitleState(state) {
  if (!osTitleEl || state === titleState) return;
  const wasTyping = titleState === 'typing';
  titleState = state;
  applyTitleClass();
  if (state === 'typing') {
    scheduleNextFlicker();
  } else if (wasTyping) {
    stopFlicker();
  }
}

// Subscribe to Rust's model-status events (already emitted, previously
// unobserved). Boot starts at 'idle' (steady white) by design: the
// pulse only fires for actual typing, and the red alarm only for confirmed
// offline/error states. The first model-status event then corrects to the
// real state.
(async () => {
  try {
    await listen('model-status', (e) => {
      const status = e?.payload?.status;
      // typing state is owned by the chat flag; don't clobber it here. Only
      // model-status transitions affect idle/offline.
      if (titleState === 'typing') return;
      if (status === 'ready') {
        setTitleState('idle');
        // (P2d, 2026-08-17 E4B shakedown) Retire the never-shown first-run
        // download overlay completely once the model is confirmed loaded —
        // it otherwise lingers in the DOM as display:flex; opacity:0;
        // pointer-events:none forever (harmless, but it pollutes DOM scans
        // + screenshots). An ACTIVE overlay (.show — models were missing,
        // download in flight) is never touched; that path ends in a reload.
        const dlOverlay = document.getElementById('download-overlay');
        if (dlOverlay && !dlOverlay.classList.contains('show')) {
          dlOverlay.style.display = 'none';
        }
      }
      else if (status === 'error' || status === 'no_model' || status === 'missing') setTitleState('offline');
    });
  } catch (err) {
    console.warn('[Wupi] model-status listen failed', err);
  }
})();

// ─── First-run LAUNCH button + chime ───────────────────────────────────────
// When the GGUF download completes, instead of auto-reloading we surface a
// sparkly LAUNCH button + a synthesized chime. The user clicks it to proceed
// to the boot choreography. Mirrors the boot-paw sparkle aesthetic: bright
// pink→magenta gradient + multi-hue sparkle burst + glow pulse.

// Lazy AudioContext singleton. Browsers require a user gesture to start
// audio; the LAUNCH click IS that gesture. Resumes on suspended (the tab
// was backgrounded). Returns null if Web Audio is unavailable.
function getAudioCtx() {
  if (!window.__wupiAudioCtx) {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    if (!Ctx) return null;
    try { window.__wupiAudioCtx = new Ctx(); }
    catch (e) { console.warn('[Wupi] AudioContext init failed', e); return null; }
  }
  if (window.__wupiAudioCtx.state === 'suspended') {
    window.__wupiAudioCtx.resume().catch(() => {});
  }
  return window.__wupiAudioCtx;
}

// ─── Boot-paw sound design (authored .mp3 assets) ──────────────────────────
// Three voices ship as .mp3 files (imported above, Vite-bundled), each voiced
// for one motion class. Played via a one-shot <audio> at BOOT_SFX_VOLUME so
// the visuals + audio share one authored sound world:
//
//   INTRO_MOVE_SRC   — the fairy-tour + corner-flight whoosh. Fires at every
//                      dart segment + the corner flight.
//   INTRO_HOP_SRC    — BOTH hops baked into one ~1.3s clip (hop-1 attack at
//                      ~0.07s, hop-2 attack at ~0.78s, inter-hop rest ~0.65–
//                      0.80s). Played ONCE at hop-1 launch; the choreography
//                      syncs the two visual hops to those two attacks.
//   INTRO_FINISH_SRC — the magical landing finale when the paw settles home.
//
// 0.6 volume = 40% lower than the authored full-volume masters.
//
// AUTOMUTE: the boot paw flies before any user gesture, so browsers may block
// autoplay. The visual boot carries regardless — sound is a bonus when live.
// Each playback creates a transient <audio> node that self-removes on end.
const BOOT_SFX_VOLUME = 0.6;

// Play a one-shot sound effect at BOOT_SFX_VOLUME. Creates a transient
// <audio> element, starts it, and removes it on ended/error so nothing leaks.
// Swallows autoplay rejection silently (the boot visual carries alone then).
function playSfx(src, opts) {
  const audio = document.createElement('audio');
  audio.src = src;
  audio.volume = BOOT_SFX_VOLUME;
  // Optional playbackRate (INTRO_HOP_SRC now plays at 1.0×, so this path is
  // currently unused; kept for future re-tuning of the two-hop cadence).
  if (opts && opts.playbackRate) audio.playbackRate = opts.playbackRate;
  audio.setAttribute('aria-hidden', 'true');
  // Self-clean on natural end OR error (errors fire under autoplay policy).
  const cleanup = () => { if (audio.parentNode) audio.parentNode.removeChild(audio); };
  audio.addEventListener('ended', cleanup, { once: true });
  audio.addEventListener('error', cleanup, { once: true });
  document.body.appendChild(audio);
  const p = audio.play();
  if (p && typeof p.catch === 'function') p.catch(cleanup);
}

// Two-ascending-note chime (A5 → E6) — a soft "ready" cue. Sine waves with
// a quick attack + exponential decay so it reads as a bell-like ping rather
// than a beep. Total duration ~0.55s. Synthesized = no asset file ships.
function playLaunchChime() {
  const ctx = getAudioCtx();
  if (!ctx) return;
  const now = ctx.currentTime;
  const notes = [
    { f: 880.0,   t: 0.00 },  // A5
    { f: 1318.51, t: 0.13 },  // E6
  ];
  notes.forEach(({ f, t }) => {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'sine';
    osc.frequency.value = f;
    gain.gain.setValueAtTime(0, now + t);
    gain.gain.linearRampToValueAtTime(0.18, now + t + 0.02);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + t + 0.45);
    osc.connect(gain).connect(ctx.destination);
    osc.start(now + t);
    osc.stop(now + t + 0.5);
  });
}

// Show the LAUNCH button on the download overlay. Hides the Start/Cancel
// buttons, fills the progress bar to 100%, then mounts the LAUNCH button
// below the stats. The button's click handler plays the chime, spawns a
// sparkle burst, and reloads.
function showLaunchButton(overlay, startBtn, cancelBtn, subtitle, stats, bar) {
  // Freeze the progress display at "complete" so the LAUNCH button reads as
  // the natural next step (no half-filled bar).
  if (bar) bar.style.width = '100%';
  if (stats) stats.textContent = '✓ Models ready';
  if (subtitle) subtitle.textContent = 'Click LAUNCH to enter WUPI.';
  startBtn.hidden = true;
  cancelBtn.hidden = true;

  // Mount the LAUNCH button if not already there (idempotent: a re-click
  // of the Start button on a retry wouldn't double-mount).
  let launchBtn = document.getElementById('launchBtn');
  if (!launchBtn) {
    launchBtn = document.createElement('button');
    launchBtn.id = 'launchBtn';
    launchBtn.className = 'launch-btn';
    launchBtn.textContent = '✦ LAUNCH ✦';
    // Insert below the actions row so it reads as the capstone CTA.
    const inner = overlay.querySelector('.download-overlay-inner');
    if (inner) inner.appendChild(launchBtn);
    else overlay.appendChild(launchBtn);
  }
  launchBtn.hidden = false;

  // Play the chime once on first appearance (no gesture yet — this is the
  // completion chime, NOT triggered by a click). Some browsers block audio
  // that wasn't user-gesture-initiated; the .catch swallows the Autoplay
  // rejection silently (the visual sparkle + button still cue completion).
  try { playLaunchChime(); } catch (e) { /* autoplay blocked: silent */ }

  launchBtn.addEventListener('click', function onLaunch() {
    launchBtn.removeEventListener('click', onLaunch);
    // User gesture: the chime is guaranteed to play here.
    try { playLaunchChime(); } catch (e) { /* ignore */ }
    // Spawn a sparkle burst around the button center (reuses the boot-paw
    // sparkle aesthetic: .boot-sparkle CSS + the spawnSparkles pattern).
    spawnLaunchSparkles(launchBtn);
    // Reload so setup() re-runs with WUPI.gguf present + the boot
    // choreography plays. (The window boots non-on-top per tauri.conf.json —
    // the 2026-07-23 locked decision — so there's nothing to "restore" here.
    // An earlier build flipped always-on-top on at this point; that call was
    // stale from when the config default was true and trapped the window on
    // top for the whole session. Removed.)
    // Tiny delay so the sparkle + chime land before the reload kills them.
    setTimeout(() => location.reload(), 350);
  });
}

// Sparkle burst for the LAUNCH button. Spawns N .boot-sparkle children of
// the button (position: relative via .launch-btn CSS), each flying outward
// in a random direction via the --burst CSS var. Self-cleans on
// animationend (mirrors spawnSparkles in the boot-paw code, ~line 740).
function spawnLaunchSparkles(parent, count = 18) {
  for (let i = 0; i < count; i++) {
    const s = document.createElement('div');
    s.className = 'boot-sparkle big';
    const angle = (Math.PI * 2 * i) / count + Math.random() * 0.4;
    const dist = 30 + Math.random() * 40;
    const dx = Math.cos(angle) * dist;
    const dy = Math.sin(angle) * dist - 8; // bias upward
    s.style.setProperty('--burst', `translate(${dx.toFixed(1)}px, ${dy.toFixed(1)}px)`);
    const hues = [320, 190, 270, 300, 220];
    const h = hues[Math.floor(Math.random() * hues.length)];
    s.style.background = `hsl(${h}, 100%, 75%)`;
    s.style.filter = `drop-shadow(0 0 4px hsla(${h}, 100%, 70%, 0.95))`;
    parent.appendChild(s);
    s.addEventListener('animationend', () => s.remove(), { once: true });
  }
}

// ─── First-run model download gate ─────────────────────────────────────────
// On a fresh install the GGUFs aren't on disk. setup() in Rust emits
// `model-status: missing` and we proactively invoke `check_models` here.
// If either model is missing, the #download-overlay takes over the screen
// (z-index 100002, opaque violet abyss) BEFORE the boot paw animation
// starts — so the user never sees the paw fly home into an empty OS. The
// overlay drives `download_models`; on completion the page reloads so
// setup() re-runs with WUPI.gguf present and the normal boot proceeds.
//
// Why reload rather than hot-load: setup() does a one-shot model spawn at
// boot (lib.rs:320). Re-entering that path from JS would duplicate the
// spawn-load + schema-engine + API-restore wiring. A reload is the clean
// re-entry: the runtime state is ephemeral anyway (WUPI launches fresh
// every time, AGENTS.md §5 "EPHEMERAL by default"), so we lose nothing.
(async function setupModelDownloadGate() {
  let status;
  try {
    status = await invoke('check_models');
  } catch (err) {
    console.error('[Wupi] check_models failed; assuming present to avoid blocking boot', err);
    return; // Don't trap the user behind a broken overlay; let boot proceed.
  }
  const needDownload = status?.wupi === 'missing' || status?.embed === 'missing'
    || status?.sd === 'missing';
  if (!needDownload) return;

  const overlay = document.getElementById('download-overlay');
  const startBtn = document.getElementById('downloadStartBtn');
  const cancelBtn = document.getElementById('downloadCancelBtn');
  const bar = document.getElementById('downloadProgressBar');
  const stats = document.getElementById('downloadStats');
  const subtitle = document.getElementById('downloadSubtitle');
  const errorEl = document.getElementById('downloadError');
  if (!overlay || !startBtn) {
    console.error('[Wupi] download overlay elements missing; cannot gate boot');
    return;
  }

  // Take over the screen immediately (before the paw animation can show
  // through). The overlay's CSS fade-in (0.8s) gives a soft reveal.
  overlay.classList.add('show');

  // Size hint for the idle subtitle, derived from WHAT is missing: a fresh
  // install pulls everything (~12 GB); a 0.21 → 0.22 update already has both
  // GGUFs and only fetches the PRISM SD set (~6 GB). Honest copy either way.
  const sdOnlyUpdate = status?.wupi === 'present' && status?.embed === 'present';
  const idleSubtitle = sdOnlyUpdate
    ? 'Update setup: download the PRISM image-generation models (~6 GB). One-time only.'
    : 'Setup: download the AI models (~12 GB — chat, memory, and PRISM image-gen). One-time only.';
  // Apply immediately — the static HTML copy is the generic fresh-install
  // variant; an update run must see the ~6 GB variant before any progress.
  if (subtitle) subtitle.textContent = idleSubtitle;

  // Human-readable byte formatting for the stats line.
  const fmtBytes = (n) => {
    if (!n || n <= 0) return '0 MB';
    if (n < 1024 * 1024) return (n / 1024).toFixed(0) + ' KB';
    if (n < 1024 * 1024 * 1024) return (n / (1024 * 1024)).toFixed(0) + ' MB';
    return (n / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
  };

  // Phase → subtitle text. Keeps the user oriented during the long pull.
  const phaseText = (phase, file) => {
    switch (phase) {
      case 'resolving': return `Preparing ${file || 'download'}…`;
      case 'downloading': return `Downloading ${file || 'models'}…`;
      case 'finalizing': return `Saving ${file || 'file'}…`;
      case 'done': return 'Download complete — click LAUNCH to continue.';
      case 'failed': return 'Download failed.';
      default: return idleSubtitle;
    }
  };

  // Live progress listener (throttled to 2/sec from Rust). Updates the bar
  // width + stats line. The poll below is the authoritative fallback.
  // Together with the poll, BOTH paths feed applyProgress; transitions are
  // idempotent (guarded by `downloadTerminal`), so double-firing is harmless.
  let progressUnlisten = null;
  try {
    progressUnlisten = await listen('download-progress', (e) => {
      const p = e?.payload;
      if (!p) return;
      applyProgress(p);
    });
  } catch (err) {
    console.warn('[Wupi] download-progress listen failed; falling back to poll only', err);
  }

  // download_models is fire-and-forget as of v0.3.7: it returns Ok(()) on
  // dispatch and the actual download runs on a detached tokio task. The UI's
  // terminal transitions (→ LAUNCH button on done, → retry on failed) are
  // therefore driven by the progress stream's `phase`, NOT by the IPC's
  // Promise resolution. downloadTerminal guards against double-transition
  // when both the poll + the live event fire for the same terminal state.
  let downloadTerminal = false;

  // Apply a progress snapshot to the DOM. Shared by the event listener +
  // the poll so both paths render identically. Side-effects on terminal
  // phases (done/failed) drive the overlay's UI state machine.
  function applyProgress(p) {
    const pct = p.overall_total > 0
      ? Math.min(100, (p.overall_downloaded / p.overall_total) * 100)
      : 0;
    bar.style.width = pct.toFixed(1) + '%';
    if (p.current_file && p.phase === 'downloading') {
      const filePct = p.current_file_total > 0
        ? Math.min(100, (p.current_file_offset / p.current_file_total) * 100).toFixed(0)
        : '?';
      stats.textContent =
        `${fmtBytes(p.overall_downloaded)} / ${fmtBytes(p.overall_total)} · ` +
        `${p.current_file} ${filePct}%`;
    } else if (p.phase === 'done') {
      stats.textContent = `${fmtBytes(p.overall_total)} downloaded`;
    } else {
      stats.textContent = `${fmtBytes(p.overall_downloaded)} / ${fmtBytes(p.overall_total)}`;
    }
    subtitle.textContent = phaseText(p.phase, p.current_file);
    if (p.phase === 'failed' && p.error) {
      errorEl.textContent = p.error;
    } else if (p.phase !== 'done') {
      // Don't clear a failure message while transitioning to done; the
      // LAUNCH button takes over the UI anyway. For all other non-failed
      // phases, clear any stale error text.
      errorEl.textContent = '';
    }

    // Terminal transitions (the v0.3.7 fire-and-forget state machine).
    if (!downloadTerminal) {
      if (p.phase === 'done') {
        downloadTerminal = true;
        clearInterval(pollHandle);
        if (progressUnlisten) { progressUnlisten(); progressUnlisten = null; }
        cancelBtn.hidden = true;
        showLaunchButton(overlay, startBtn, cancelBtn, subtitle, stats, bar);
      } else if (p.phase === 'failed') {
        downloadTerminal = true;
        clearInterval(pollHandle);
        if (progressUnlisten) { progressUnlisten(); progressUnlisten = null; }
        cancelBtn.hidden = true;
        // Re-enable Start so the user can retry (resume picks up from .part).
        startBtn.disabled = false;
        startBtn.textContent = 'Download';
      }
    }
  }

  // Poll fallback: the authoritative read between throttled emits. Closes
  // any gap if an event is dropped under load. Stops when applyProgress
  // transitions to a terminal phase (done/failed).
  const pollHandle = setInterval(async () => {
    try {
      const p = await invoke('get_download_progress');
      applyProgress(p);
    } catch (err) {
      console.warn('[Wupi] progress poll failed', err);
    }
  }, 500);

  // Cancel button: signal Rust to stop at the next chunk boundary. The
  // .part files stay on disk; the next Download click resumes from there.
  cancelBtn.addEventListener('click', () => {
    try { invoke('cancel_download'); } catch (err) {
      console.warn('[Wupi] cancel_download failed', err);
    }
  });

  // Start button: kicks off download_models (fire-and-forget). Disables
  // itself for the duration; reveals Cancel. The actual terminal UI
  // transitions (LAUNCH on done / retry on failed) are driven by applyProgress
  // observing the progress stream's `phase`, NOT by this IPC's Promise — see
  // the v0.3.7 note above. The IPC only resolves with an Err if SETUP failed
  // synchronously (couldn't resolve the models dir, mutex poisoned); those
  // surface immediately as error text + re-enabled Start.
  startBtn.addEventListener('click', async () => {
    startBtn.disabled = true;
    startBtn.textContent = 'Downloading…';
    cancelBtn.hidden = false;
    errorEl.textContent = '';
    downloadTerminal = false;  // reset for this attempt
    // Fire-and-forget: do NOT await. The download runs on a detached Rust
    // task whose lifetime is independent of this IPC call (v0.3.7 fix for
    // the alt-tab-kills-download bug — WebView2 suspension no longer drops
    // the download future).
    invoke('download_models').catch((err) => {
      console.error('[Wupi] download_models dispatch failed', err);
      const msg = typeof err === 'string' ? err : (err?.message || 'Unknown error');
      errorEl.textContent = msg;
      startBtn.disabled = false;
      startBtn.textContent = 'Download';
      cancelBtn.hidden = true;
    });
  });
})();

// ─── Boot paw → fly home → staged reveal ────────────────────────────────────
// The OS window boots transparent (tauri.conf.json) and STAYS transparent for
// its lifetime. What controls desktop bleed-through is the BODY background-color:
//   - body.booting         → transparent (CSS) → desktop shows through.
//   - body:not(.booting)   → #02040a (CSS)     → solid black covers desktop.
//
// CHOREOGRAPHY (per spec, refined):
//   0.0s  Blank screen (paw parked below the bottom edge, off-screen,
//         opacity:0 so no top-left flash).
//   0-1s  Pre-cache runway (ENTRY_DELAY): muted SFX decode warm-up + the
//         visual pre-cache (paw PNG decode + rAF heartbeat) — the pipeline
//         is hot before the first animated frame (2026-08-14).
//   1.0s  Paw ENTERS from the bottom, RISES to center, then ZOOMS in a
//         sporadic fairy path: dart LEFT → dart RIGHT → return CENTER.
//         Sparkle TRAIL follows the paw's path (small fixed-position
//         sparkles spawned every ~25ms) — reads as a comet tail.
//   ~2.5s Two QUICK hops. Each apex spawns a sparkle burst that ESCALATES:
//         hop 1 = 8 small sparkles, hop 2 = 16 bigger multi-colored ones.
//         Trail is paused during the hops so the bursts get the spotlight.
//   ~3.6s Paw FLIES to its home spot in the top-left (the real .paw-img
//         rect), shrinking ~153px → 45px as it travels. Trail restarts
//         for the flight.
//   land  Final big multi-colored burst (capstone), then LOADING SCREEN
//         fades in over everything (violet abyss + "LOADING OS . . ." text
//         + terminal stream). Runs LOADING_DURATION_MS (~8s).
//   load  The loading text lights L→R as progress fills; terminal streams
//         cosmetic boot lines; the real "✓ model ready" milestone appears
//         only when Rust's model-status:ready fires (honest sync).
//   done  Loading screen crossfades out → staged reveal (revealAfterLand):
//         body opaque → top-bar fades in → canvas paints sky+stars → aurora
//         LEFT-TO-RIGHT wipe → boot-paw removed → dock.
//
// STAGING NOTE: the top-bar's backdrop-filter:blur and the aurora's blur(30px)
// are the two heavy GPU costs. They are now staged so they DON'T overlap —
// the top-bar finishes its 0.6s fade BEFORE the aurora wipe arms. That's the
// real fix for "aurora load-in looks laggy": it's not the aurora alone, it's
// the aurora + top-bar blur running concurrently.
//
// Gate: chat `model-status: ready` (the local-model load — Rust's single source of
// truth, Rust is untouched) AND a minimum dwell timer. Both must resolve
// before the flight begins (the entry + hops always run regardless — they're
// the loading animation that hides the model load). The existing model-status
// listener above keeps its title-indicator job; this is a SEPARATE listener
// so the title's `typing` no-op guard can't swallow the wake signal.
(async function setupBootSplash() {
  // ── FABLE ENTRY (#fable / fable.exe): skip the entire OS boot ceremony. ──
  // When FABLE_ENTRY is active, the paw hops, the 8s loading screen, and
  // the aurora reveal are all bypassed. We instead: tear down the boot DOM
  // nodes (so they can't cover Fable), drop the body boot classes (so the OS
  // chrome behaves as already-revealed), fire boot_load_model (so the Fable
  // narrator has its model — without this the engine never spawns), and flip
  // bootDone (so the canvas gate is open for when the user later EXITS Fable
  // back to the OS desktop). The actual Fable launch is wired later, after
  // initFable() runs (see the FABLE_ENTRY block in the app wiring). We
  // still honor check_models: on first-run (models missing) we let the
  // download overlay do its job and bail just like the normal path.
  // DEV PREVIEW: pure-frontend layout preview — no models, no API, no IPC.
  // Clear the boot DOM + classes + bail; the app-wiring block below launches
  // Fable (which routes preview to devPreviewEnter, never invoking anything).
  if (DEV_PREVIEW_SHORTCUT) {
    console.log('[Wupi] dev-preview: pure-frontend layout preview (no backend)');
    document.getElementById('boot-paw')?.remove();
    document.getElementById('boot-loading')?.remove();
    document.body.classList.remove('booting', 'loading');
    bootDone = true;
    return;
  }

  if (FABLE_ENTRY) {
    const tag = 'fable-entry';

    // Reveal the (hidden) main window NOW. It was built .visible(false) in Rust
    // (lib.rs setup) to hide the WebView2 native-surface flash during init; the
    // F-logo splash (#fable-entry-splash, painted by static HTML + the inline
    // head script) is already composited, so this is the FIRST visible frame —
    // never the native surface. The splash holds ~2s while the Fable title
    // initializes underneath (launchFable runs during module eval, in parallel
    // with the check_models await below), then crossfades out (fadeSplash).
    invoke('fable_reveal_window').catch((e) =>
      console.warn(`[Wupi] ${tag}: fable_reveal_window failed`, e),
    );
    const splash = document.getElementById('fable-entry-splash');
    const fadeSplash = () => {
      if (!splash) {
        // No splash node → nothing to fade, but the html.fable-entry hold
        // (body-transparent + OS-chrome suppression, styles.css) MUST still
        // end or the OS chrome stays hidden forever. Drop it now.
        document.documentElement.classList.remove('fable-entry');
        return;
      }
      // Shared teardown (transitionend + the 1s safety net both land here;
      // every op is idempotent): once the splash NODE is gone, the
      // html.fable-entry class has nothing left to gate (the splash's display
      // rule keys on it) — dropping it also ends the entry hold's
      // body-transparent override (styles.css), restoring the OS base color
      // safely behind the now-visible Fable app.
      const teardown = () => {
        splash.remove();
        document.documentElement.classList.remove('fable-entry');
      };
      splash.classList.add('fade-out');
      splash.addEventListener(
        'transitionend',
        teardown,
        { once: true },
      );
      // Safety net: remove after the transition regardless (transitionend can
      // be missed if the tab is backgrounded mid-fade).
      setTimeout(teardown, 1000);
    };
    // Hold the splash ~2s so any remaining init quirks stay hidden behind it,
    // then dissolve into the now-rendered title screen underneath.
    setTimeout(fadeSplash, 2000);

    try {
      const status = await invoke('check_models');
      if (status?.wupi === 'missing' || status?.embed === 'missing' || status?.sd === 'missing') {
        console.log(`[Wupi] ${tag}: models missing, deferring to download overlay`);
        return;
      }
    } catch (err) {
      console.warn(`[Wupi] ${tag}: check_models failed, proceeding`, err);
    }
    // Remove the boot DOM so it can't sit on top of Fable.
    document.getElementById('boot-paw')?.remove();
    document.getElementById('boot-loading')?.remove();
    // Drop the boot body classes: .booting keeps the top-bar/dock hidden + body
    // transparent; .loading (if present) holds the dock back. Clearing both
    // mirrors what revealAfterLand() + endLoadingScreen() leave behind.
    document.body.classList.remove('booting', 'loading');
    // Open the canvas gate so the desktop can paint later. We do NOT call
    // startLoop() here: by the time this await resumes, launchFable() (fired
    // during module eval, before the IPC resolved) has already run openFable →
    // pauseAurora → paused=true, and Fable's full-screen overlay owns the
    // screen. Forcing startLoop() now would un-pause the canvas behind Fable
    // (wasted GPU) and fight pauseAurora. Instead we just flip bootDone so the
    // gate is open; when the user EXITS Fable, resumeAurora() → startLoop()
    // succeeds (bootDone gates startLoop) and the desktop paints correctly.
    bootDone = true;
    // Fire the model spawn exactly as the normal loading screen does. The
    // Fable narrator's first turn needs the WUPI.gguf load to have started;
    // without this the engine thread never spawns in dev mode.
    invoke('boot_load_model').catch((e) => {
      console.error(`[Wupi] boot_load_model failed (${tag})`, e);
    });
    console.log(`[Wupi] ${tag} active — skipping OS boot, launching Fable`);
    return;
  }

  // First-run gate: if the GGUFs are missing, DON'T play the paw animation.
  // The download overlay (setupModelDownloadGate) takes over the screen and
  // any paw motion underneath it creates a weird visual bug (the paw flies
  // through the overlay). We resolve check_models FIRST; on missing → bail
  // before scheduling any entry/hop/flight timers. The download gate runs
  // in parallel and is the sole animation when models need downloading.
  // Tolerant of IPC failure: if check_models throws, assume present (better
  // to play the paw on a misfire than strand the user on a blank screen).
  try {
    const status = await invoke('check_models');
    if (status?.wupi === 'missing' || status?.embed === 'missing' || status?.sd === 'missing') {
      console.log('[Wupi] models missing; skipping boot paw (download overlay takes over)');
      return;
    }
  } catch (err) {
    console.warn('[Wupi] check_models failed during boot gate; proceeding with paw', err);
  }

  // ── Warm the SFX buffers during the blank 1s pause. Each intro clip is
  //    played via a transient <audio> element created at play time; the FIRST
  //    play of any source incurs decode + buffer-setup latency (the "first
  //    move sound lags behind" symptom — the 2nd/3rd move sounds are fine
  //    because the decoded frames are then cached). preload='auto' only buffers
  //    the BYTES; it defers the expensive PCM decode lazily to the first play,
  //    so a bare preload does NOT eliminate the first-play lag. The fix: force
  //    the decode here by actually calling .play() (muted + volume 0 + seeked
  //    back to 0 immediately), driving each clip through the full
  //    load → decode → ready pipeline during the blank second. playSfx() then
  //    makes its own fresh nodes that hit the now-cached decoded frames
  //    near-instantly. (The muted warm node is paused + discarded once it has
  //    started; the decoded frames persist in the media cache.)
  for (const src of [INTRO_MOVE_SRC, INTRO_HOP_SRC, INTRO_FINISH_SRC]) {
    try {
      const warm = document.createElement('audio');
      warm.src = src;
      warm.muted = true;            // muted so the warm pass is silent
      warm.volume = 0;
      warm.setAttribute('aria-hidden', 'true');
      const tearDown = () => {
        try { warm.pause(); } catch (_) {}
        if (warm.parentNode) warm.parentNode.removeChild(warm);
      };
      // As soon as the clip is playable, kick a muted micro-play to force the
      // decode, then tear it down. 'canplay' (not 'canplaythrough') fires as
      // soon as the first frames are decoded — enough to populate the cache.
      warm.addEventListener('canplay', () => {
        const p = warm.play();
        if (p && typeof p.then === 'function') {
          p.then(() => { try { warm.currentTime = 0; } catch (_) {} tearDown(); })
           .catch(tearDown);
        } else {
          tearDown();
        }
      }, { once: true });
      warm.addEventListener('error', tearDown, { once: true });
      document.body.appendChild(warm);
    } catch (e) { /* warm is best-effort */ }
  }

  // ── Visual pre-cache during the ENTRY_DELAY runway (2026-08-14). The SFX
  //    loop above warms the AUDIO path; this warms the RENDER path so the
  //    paw's first frames aren't the pipeline's first frames:
  //    (1) Force the paw PNG's decode NOW. <img> decode is lazy — without
  //        this it lands on the entry animation's first frame (a 45→126px
  //        scaled decode = the opening stutter). decode() resolves once the
  //        bitmap is resident; best-effort, ignored if it rejects.
  //    (2) A rAF heartbeat for the whole runway. Each callback drives one
  //        full style/paint/composite pass, so the compositor keeps
  //        producing frames + GPU surfaces stay warm through the blank
  //        second — the entry animation then starts on an already-hot
  //        pipeline instead of a cold one. The loop self-terminates the
  //        moment the runway ends (the paw's own rAF/animation frames take
  //        over from there).
  try {
    const pawImg = document.querySelector('#boot-paw .boot-paw-img');
    if (pawImg && typeof pawImg.decode === 'function') pawImg.decode().catch(() => {});
  } catch (e) { /* warm is best-effort */ }

  // ENTRY_DELAY (ms) is a blank-screen runway before the paw enters. History:
  // removed 2026-08-04 (Chloe wanted the paw immediately), RESTORED at 1000ms
  // 2026-08-14 — on a cold wupi.exe launch the WebView2's first second lags
  // badly + skips frames (JS parse/JIT, first style/paint/composite passes,
  // GPU surface creation all landing on the animation's opening frames). The
  // runway shifts all of that ahead of the choreography; the visual pre-cache
  // block above warms the specific paw pipeline during it.
  // ORDER-DEPENDENT: declared here — BEFORE the warmHeartbeat IIFE below —
  // because that IIFE runs immediately and reads it. A `const` below the call
  // is a TDZ ReferenceError that aborts the whole boot splash before any
  // timer is scheduled: body stays .booting → the window stays transparent
  // forever (the infinite-blank-screen regression it caused, 2026-08-14).
  const ENTRY_DELAY = 1000;

  const warmStart = performance.now();
  (function warmHeartbeat(now) {
    if (now - warmStart < ENTRY_DELAY) requestAnimationFrame(warmHeartbeat);
  })(performance.now());

  // Timing constants (ms). (ENTRY_DELAY lives up with the pre-cache heartbeat
  // that reads it — its declaration order is load-bearing.)
  // Fairy-tour choreography: RISE STRAIGHT TO TOP-LEFT MIDDLE → dart to
  // TOP-RIGHT MIDDLE → dart to CENTER. Each dart is a hard ZOOM_EASE in/out
  // so the paw reads as a fairy teleporting with momentum. The rise gets the
  // biggest slice (longest distance). The TOTAL duration is LOAD-BEARING for
  // audio sync: the three move-whooshes fire at the rise/dart-right/dart-
  // center keyframe offsets (0 / 0.39 / 0.78), so at this duration they land
  // at 0 / 585 / 1170ms.
  // 2026-08-14 final: the 1800ms pass was "almost perfect, a tiny bit fast
  // needed"; 1200ms was too fast. Chloe's call: the SWEET SPOT between the
  // two — 1500ms, keeping the original fractional keyframe structure (rise
  // 450 / hold 135 / dart 210 / hold 450 / dart 180 / hold 150). Hops
  // untouched throughout.
  const ENTRY_DURATION = 1500;
  // Sharp accel + sharp decel — the "fairy dart" easing. Most of the
  // motion happens in the middle of the segment, with hard start/stop.
  const ZOOM_EASE = 'cubic-bezier(0.65, 0, 0.35, 1)';
  // Hop cadence is SYNCED TO INTRO_HOP_SRC: the clip has both hops baked in
  // (hop-1 attack ~0.07s, hop-2 attack ~0.78s). hop-1 launches at clip-start,
  // hop-2 launches HOP_2_DELAY_MS into the clip so its visual apex lands on
  // the clip's second attack. Each hop's up+down is HOP_DURATION; the gap
  // between hop-1 landing and hop-2 launching is the clip's inter-hop rest.
  // 2026-08-13: whole paw boot + audio sped up 20% per Chloe. HOP_DURATION
  // scaled 175 → 140 (×0.8) + the hop CLIP rate raised 1.2× → 1.5× (×1.25,
  // the inverse of 0.8) so the audio quickens to match the hops. The two
  // stay locked: faster hops + faster clip, apex-synced as before.
  const HOP_DURATION = 140;       // each hop (up + down) — 20% faster (was 175)
  const HOP_APEX = HOP_DURATION / 2;
  const HOP_HEIGHT = 70;          // px the inner img rises per hop
  // hop-2 launch offset (wall-clock ms after hop-1 launched at clip-start).
  // APEX-SYNCED to INTRO_HOP_SRC's second attack: hop-2's visual apex (launch +
  // HOP_APEX) lands on the clip's second boing, mirroring how hop-1's apex
  // lands on the first attack. (Anchoring hop-2's LAUNCH to the attack left its
  // apex ~88ms AFTER the boing — the "second hop jumped a tiny bit late"
  // symptom. hop-1 launches at clip-start so its apex naturally lands on
  // attack 1; hop-2's launch was set to the attack TIME, not attack-minus-apex.)
  // So: launch = (attackClipTime / rate) − HOP_APEX.
  // At 1.5×: (780 / 1.5) − 70 = 520 − 70 = 450ms.
  const HOP_2_DELAY_MS = 450;
  // The clip plays at 1.5× (raised from 1.2× on the 20%-faster pass). Still
  // shy of the rushed 1.7× (which collapsed the two hops into one "hop-hop").
  // 1.5× keeps the clip's own resonant decay tail (NOT silence) between the
  // two boings while tightening the cadence to match the 20%-faster hops.
  // HOP_2_DELAY_MS above is apex-synced for THIS rate; recompute if it changes.
  const HOP_PLAYBACK_RATE = 1.5;
  // The move-whoosh rate for the FOUR travel movements (3 entry darts + the
  // corner flight). 2026-08-14 final: mid-point of the tuning history like
  // the durations — 1.55× (the 1800ms pass) felt right, 2.3× (the 1200ms
  // pass) over-quick; 1.85 ≈ 2800/1500 tracks the entry's cumulative
  // speed-up (the flight's 720/350 ≈ 2.06 — one shared rate, weighted toward
  // the three entry whooshes). Tails shrink to ~0.68s vs the 0.59s fire
  // spacing: barely a kiss, no overlap roar. The hops themselves are
  // UNTOUCHED (HOP_DURATION / HOP_2_DELAY_MS / HOP_PLAYBACK_RATE unchanged).
  const INTRO_MOVE_PLAYBACK_RATE = 1.85;
  // Sparkle trail: a sparkle spawns every TRAIL_INTERVAL ms along the paw's
  // path during entry + flight (NOT during hops — those get the escalating
  // bursts). Tuned for perf: tighter interval was creating ~150 concurrent
  // animated DOM nodes (the lag source). 50ms + 1/tick = ~20 nodes/sec.
  const TRAIL_INTERVAL = 50;
  // Paw display size at center. The resting paw-img is 45px; ~2.8x makes
  // it ~126px — a touch smaller than the previous 3.4x per spec ("a little
  // smaller"), still prominent in the middle of the screen during the hops.
  const PAW_BOOT_SCALE = 2.8;
  const PAW_REST_SIZE = 45;
  // Loiter after hop 2 before the corner flight fires. 2026-08-13: cut 1000 →
  // 500 per Chloe ("have it move to the top left .5 seconds quicker so it
  // doesn't linger as long"). The hop-2 sparkle burst still gets a beat, just a
  // shorter one, before the paw lifts off for the corner.
  const POST_HOP_LOITER_MS = 500;
  // Straight-line corner flight: fires after the post-hop loiter. Per spec
  // ("just make it a straight line, you aren't curving it correctly") the
  // flight is a single CSS transition to the corner — no WAAPI arc. Tuning
  // history: 720 → 575 → 490 → 400 (the "almost perfect" pass) → 300 (too
  // fast) → 350 (the sweet spot, 2026-08-14 final). Its whoosh plays at
  // INTRO_MOVE_PLAYBACK_RATE to match. The finish-SFX lead-in (fired FLIGHT -
  // 120ms before land) still lands its attack on touchdown.
  const FLIGHT_DURATION_MS = 350;
  // Staged-reveal delays (ms) measured from flight-land (transitionend).
  // Top-bar fade is 0.6s in CSS; aurora wipe arms AFTER it finishes so the
  // two blur costs never overlap.
  const DELAY_SKY = 200;          // canvas RAF starts (sky + stars only)
  const DELAY_PAW_REMOVE = 400;   // boot-paw fades → real paw revealed
  const DELAY_AURORA = 800;       // aurora wipe arms (after top-bar's 0.6s fade)
  // Min-dwell is no longer a flight gate (the choreography's built-in 1s
  // holds + hop durations define the length). Kept as a backstop in case
  // a future regression needs a floor; not currently read for flight.
  const MIN_DWELL_MS = ENTRY_DELAY + ENTRY_DURATION + HOP_2_DELAY_MS + HOP_DURATION + 200;
  // Loading screen (runs AFTER the paw lands, BEFORE the staged reveal).
  // 8s per spec — adjustable. The text spans light L→R across this window;
  // the terminal streams fake boot lines + the real "model ready" milestone.
  const LOADING_DURATION_MS = 8000;
  const LOADING_TEXT = 'LOADING OS . . .';

  let flightApproved = false;
  let hopsDone = false;

  const bootPaw = document.getElementById('boot-paw');
  const bootPawImg = bootPaw ? bootPaw.querySelector('.boot-paw-img') : null;
  const realPaw = document.querySelector('.paw-img');
  const bootLoading = document.getElementById('boot-loading');
  const bootLoadingText = document.getElementById('bootLoadingText');
  const bootTerminal = document.getElementById('bootTerminal');

  // <body> already carries .booting from index.html; this is belt-and-suspenders
  // for the dev-preview case where the HTML might not have it.
  document.body.classList.add('booting');

  // ── Phase 0: park the paw off-screen below the viewport, scaled up.
  //    The CSS default `top: 110vh` already places the element off-screen
  //    at first paint (so there's never a top-left flash). Here we also
  //    set an explicit transform so the entry WAAPI animation has a
  //    concrete from-position. No transition-suppression / reflow dance
  //    needed anymore — the element is born invisible thanks to the CSS.
  if (bootPaw) {
    const restCx = (window.innerWidth - PAW_REST_SIZE) / 2;
    const parkCy = window.innerHeight + 50; // just below the bottom edge
    bootPaw.style.width = PAW_REST_SIZE + 'px';
    bootPaw.style.height = PAW_REST_SIZE + 'px';
    bootPaw.style.transform =
      `translate(${restCx}px, ${parkCy}px) scale(${PAW_BOOT_SCALE})`;
  }

  // ── Sparkle burst. Spawns N .boot-sparkle children of #boot-paw, each
  //    flying outward in a random direction via the --burst CSS var. They
  //    self-clean on animationend. The `tier` arg escalates the burst:
  //      0 = small/short (trail sparkles, default hop-1 burst)
  //      1 = bigger, more colorful, longer lifetime (hop-2 burst)
  //    so each hop reads as a bigger, prettier event than the last.
  function spawnSparkles(count = 8, tier = 0) {
    if (!bootPaw) return;
    for (let i = 0; i < count; i++) {
      const s = document.createElement('div');
      s.className = tier > 0 ? 'boot-sparkle big' : 'boot-sparkle';
      const angle = (Math.PI * 2 * i) / count + Math.random() * 0.4;
      // Burst distances scaled DOWN to match the smaller sparkles (the
      // old 30-70px range made them fly far past the paw's reduced halo).
      const baseDist = tier > 0 ? 28 : 16;
      const dist = baseDist + Math.random() * (tier > 0 ? 26 : 22);
      const dx = Math.cos(angle) * dist;
      const dy = Math.sin(angle) * dist - 6; // bias upward slightly
      s.style.setProperty('--burst', `translate(${dx.toFixed(1)}px, ${dy.toFixed(1)}px)`);
      // Hue jitter on the big tier so the escalated burst reads as
      // multi-colored (magenta/cyan/violet palette).
      if (tier > 0) {
        const hues = [320, 190, 270, 300, 220];
        const h = hues[Math.floor(Math.random() * hues.length)];
        s.style.background = `hsl(${h}, 100%, 75%)`;
        s.style.filter = `drop-shadow(0 0 4px hsla(${h}, 100%, 70%, 0.95))`;
      }
      bootPaw.appendChild(s);
      s.addEventListener('animationend', () => s.remove(), { once: true });
    }
  }

  // ── Trail sparkle: one small sparkle spawned at the paw's current screen
  //    position. Used by the trail timer during entry/darts/flight. The
  //    sparkle is appended to <body> (NOT #boot-paw) at fixed viewport
  //    coords so it stays where it spawned instead of inheriting the paw's
  //    transform. Self-cleans on animationend.
  function spawnTrailSparkle() {
    if (!bootPaw) return;
    const r = bootPaw.getBoundingClientRect();
    // 1 sparkle per tick (perf: the old 3/tick + 25ms interval was creating
    // ~150 concurrent animated DOM nodes, which was the lag source). With
    // 50ms interval + 1/tick + 0.8s lifetime we average ~16 concurrent nodes.
    const jx = (Math.random() - 0.5) * r.width * 0.5;
    const jy = (Math.random() - 0.5) * r.height * 0.5;
    const s = document.createElement('div');
    s.className = 'boot-sparkle trail';
    s.style.left = (r.left + r.width / 2 + jx) + 'px';
    s.style.top = (r.top + r.height / 2 + jy) + 'px';
    document.body.appendChild(s);
    s.addEventListener('animationend', () => s.remove(), { once: true });
  }

  // Trail control: setInterval-spawned trail sparkles while `trailActive`
  // is true. Started before entry, stopped when hops begin (hops get the
  // escalating bursts instead), restarted for the flight.
  let trailTimer = null;
  function startTrail() {
    if (trailTimer) return;
    trailTimer = setInterval(spawnTrailSparkle, TRAIL_INTERVAL);
  }
  function stopTrail() {
    if (trailTimer) { clearInterval(trailTimer); trailTimer = null; }
  }

  // ── Entry + hops. Uses the Web Animations API so we can dispatch sparkle
  //    bursts at exact hop apexes. The inner img's translateY animates the
  //    hops; #boot-paw's translate/scale are reserved for the entry darts +
  //    the later flight.
  function startEntryAndHops() {
    if (!bootPaw || !bootPawImg) { hopsDone = true; maybeFly(); return; }

    // Reveal the paw. CSS defaults it to opacity:0 (avoids a top-left flash
    // before this runs); now that we're about to animate it, flip it on.
    bootPaw.style.transition = 'transform 0.8s cubic-bezier(0.22, 1, 0.36, 1)';
    bootPaw.style.opacity = '1';

    // Start the sparkle trail — it follows the paw through the fairy-zoom.
    startTrail();

    // Movement SFX #1: plays as the paw lifts off for its first flight (the
    // rise to top-left). Schedules the two dart move sounds to fire at the
    // exact WAAPI offsets where those segments begin (dart-right + dart-center
    // keyframes below) so each "flight" cue lands on its motion. All three
    // play at INTRO_MOVE_PLAYBACK_RATE (2026-08-14 final: 1.85×, matched to
    // the 1500ms entry).
    try { playSfx(INTRO_MOVE_SRC, { playbackRate: INTRO_MOVE_PLAYBACK_RATE }); } catch (e) { /* autoplay blocked: silent */ }
    // Dart-whoosh timers at the keyframe offsets where those segments begin
    // (dart-right + dart-center keyframes below) so each "flight" cue lands
    // on its motion: ~585ms / ~1170ms at the 1500ms entry — 0.59s apart vs
    // the ~0.68s tails at 1.85×, no overlap roar.
    const dartRightAt = ENTRY_DURATION * 0.39;  // ~585ms
    const dartCenterAt = ENTRY_DURATION * 0.78; // ~1170ms
    setTimeout(() => { try { playSfx(INTRO_MOVE_SRC, { playbackRate: INTRO_MOVE_PLAYBACK_RATE }); } catch (e) {} }, dartRightAt);
    setTimeout(() => { try { playSfx(INTRO_MOVE_SRC, { playbackRate: INTRO_MOVE_PLAYBACK_RATE }); } catch (e) {} }, dartCenterAt);

    // Entry path: RISE STRAIGHT TO TOP-LEFT MIDDLE → dart to TOP-RIGHT
    // MIDDLE → dart to CENTER. The "fairy-tour": each dart is a hard
    // ZOOM_EASE so the paw reads as a fairy teleporting with momentum.
    // Per spec: rises directly to TOP-LEFT (no center visit first).
    const restCx = (window.innerWidth - PAW_REST_SIZE) / 2;
    const restCy = (window.innerHeight - PAW_REST_SIZE) / 2;
    const parkCy = window.innerHeight + 50;
    // Dart endpoints. TOP-LEFT MIDDLE + TOP-RIGHT MIDDLE = upper quadrants,
    // roughly y ≈ 32% of viewport height.
    const DART_X_RANGE = 0.58;    // horizontal reach toward each corner
    const TOP_Y_RATIO = 0.32;     // vertical position of the side stops
    const leftX = Math.max(40, restCx - window.innerWidth * DART_X_RANGE / 2);
    const rightX = Math.min(window.innerWidth - PAW_REST_SIZE - 40,
                            restCx + window.innerWidth * DART_X_RANGE / 2);
    const topY = window.innerHeight * TOP_Y_RATIO - PAW_REST_SIZE / 2;

    const entryAnim = bootPaw.animate(
      [
        // 0 → 0.30: rise from below STRAIGHT TO TOP-LEFT MIDDLE (no center).
        // The rise gets the biggest slice — it covers the most distance and
        // should read as a graceful arrival, not a snap upward (per spec:
        // "when it flies from the bottom it moves a bit too quick").
        // easeOutQuint so it decelerates as it approaches the corner.
        { transform: `translate(${restCx}px, ${parkCy}px) scale(${PAW_BOOT_SCALE})`,
          offset: 0, easing: 'cubic-bezier(0.22, 1, 0.36, 1)' },
        { transform: `translate(${leftX}px, ${topY}px) scale(${PAW_BOOT_SCALE})`,
          offset: 0.30, easing: 'linear' },
        // 0.30 → 0.39: HOLD at TOP-LEFT (~0.25s).
        { transform: `translate(${leftX}px, ${topY}px) scale(${PAW_BOOT_SCALE})`,
          offset: 0.39, easing: ZOOM_EASE },
        // 0.39 → 0.53: dart to TOP-RIGHT MIDDLE (crosses the whole top).
        { transform: `translate(${rightX}px, ${topY}px) scale(${PAW_BOOT_SCALE})`,
          offset: 0.53, easing: 'linear' },
        // 0.53 → 0.78: HOLD at TOP-RIGHT (~0.70s — long enough for the
        // dart-right whoosh tail to decay before dart-center fires).
        { transform: `translate(${rightX}px, ${topY}px) scale(${PAW_BOOT_SCALE})`,
          offset: 0.78, easing: ZOOM_EASE },
        // 0.78 → 0.90: dart down to CENTER.
        { transform: `translate(${restCx}px, ${restCy}px) scale(${PAW_BOOT_SCALE})`,
          offset: 0.90, easing: 'linear' },
        // 0.90 → 1.0: HOLD at CENTER (~0.28s before hops begin).
        { transform: `translate(${restCx}px, ${restCy}px) scale(${PAW_BOOT_SCALE})`,
          offset: 1, easing: 'linear' },
      ],
      { duration: ENTRY_DURATION, fill: 'forwards' }
    );
    entryAnim.onfinish = () => {
      entryAnim.commitStyles();
      entryAnim.cancel();
      // Stop the trail during hops — hops get the escalating bursts.
      stopTrail();
      runHops();
    };
  }

  function runHops() {
    if (!bootPawImg) { hopsDone = true; maybeFly(); return; }
    // INTRO_HOP_SRC has BOTH hops baked into one clip (hop-1 attack ~0.07s,
    // hop-2 attack ~0.78s). Play it ONCE here; the two visual hops sync to
    // the clip's two attacks — hop-2 launches HOP_2_DELAY_MS into the clip
    // so it leaves the ground exactly when the second attack hits.
    try { playSfx(INTRO_HOP_SRC, { playbackRate: HOP_PLAYBACK_RATE }); } catch (e) { /* autoplay blocked: silent */ }
    let hop = 0;
    const doHop = () => {
      hop++;
      const a = bootPawImg.animate(
        [
          { transform: 'translateY(0)' },
          { transform: `translateY(-${HOP_HEIGHT}px)` },
          { transform: 'translateY(0)' },
        ],
        { duration: HOP_DURATION, easing: 'ease-in-out', fill: 'forwards' }
      );
      // Escalating burst at the apex: hop 1 = 8 small, hop 2 = 16 big +
      // multi-colored. Each hop is prettier than the last per spec.
      setTimeout(() => {
        if (hop === 1) spawnSparkles(8, 0);
        else spawnSparkles(16, 1);
      }, HOP_APEX);
      a.onfinish = () => {
        if (hop < 2) {
          // hop-2 launches HOP_2_DELAY_MS into the clip (synced to its second
          // HOP_2_DELAY_MS already has the apex offset baked in (launch =
          // attack_time − HOP_APEX), so hop-2's apex — not its launch — lands
          // on the second boing. hop-1 landed at HOP_DURATION; the wait here is
          // the remainder of the clip's inter-hop rest, NOT a fixed pause —
          // that keeps hop-2's apex locked to the audio attack.
          setTimeout(doHop, HOP_2_DELAY_MS - HOP_DURATION);
        } else {
          a.commitStyles();
          a.cancel();
          hopsDone = true;
          // 1s STALL after hop 2 before the corner flight fires (per spec: the
          // paw holds still for a beat, then moves into the top-left).
          setTimeout(maybeFly, POST_HOP_LOITER_MS);
        }
      };
    };
    doHop();
  }

  // Kick off the entry + hops after the ENTRY_DELAY pre-cache runway (the
  // blank first second that warms the SFX decode + render pipelines).
  setTimeout(startEntryAndHops, ENTRY_DELAY);

  // ── Flight gate. NO model-ready gate: hop 2 chains IMMEDIATELY into the
  //    curved flight per spec ("right after it finishes its second hop it
  //    immediately curves into the top left corner"). The model loads in
  //    parallel during the loading screen; the boot animation is no longer
  //    blocked on it. (The 8s loading screen after landing is what hides
  //    any remaining model load — that's the right place for the gate.)
  function maybeFly() {
    if (flightApproved) return;
    if (!hopsDone) return;
    flyPawHome();
  }

  // Min-dwell floor is no longer used (the entry's built-in holds + hop
  // durations already define the choreography length). Kept as a no-op
  // safety net in case hops fail to fire — but it no longer gates flight.
  // (Intentionally no listener wiring here.)

  // ── Phase 2: fly the paw from center → home in a STRAIGHT LINE. Reads
  //    the real .paw-img's current rect so the landing is pixel-accurate.
  //    Per spec: "as it moves into the very top left corner just make it a
  //    straight line, you aren't curving it correctly." Implemented as a
  //    single CSS transition (transform FLIGHT_DURATION_MS) — no WAAPI arc.
  function flyPawHome() {
    flightApproved = true;
    if (!bootPaw) { startLoadingScreen(); return; }

    // Movement SFX: the corner flight (movement #4). Fires now (liftoff) at
    // INTRO_MOVE_PLAYBACK_RATE to match the ×0.8 flight (2026-08-14). The
    // finish SFX is triggered just before landing (see the pre-land timer
    // below) so its tail rings out through + past touchdown rather than
    // starting cold at the transitionend instant. playSfx appends the node to
    // document.body + self-cleans on 'ended', so the paw removal in onLand
    // never truncates it — the clip plays in full regardless of the boot paw's
    // lifecycle.
    try { playSfx(INTRO_MOVE_SRC, { playbackRate: INTRO_MOVE_PLAYBACK_RATE }); } catch (e) { /* autoplay blocked: silent */ }
    // Pre-land lead-in for the finish SFX: trigger it ~120ms before touchdown
    // so the finale's attack lands as the paw settles, not after.
    setTimeout(() => {
      try { playSfx(INTRO_FINISH_SRC); } catch (e) { /* autoplay blocked: silent */ }
    }, Math.max(0, FLIGHT_DURATION_MS - 120));

    // Read the real paw's resting rect. During boot the top-bar is at
    // opacity:0 but still laid out (NOT display:none), so getBoundingClientRect
    // returns the true home coordinates.
    let targetX = 0, targetY = 0;
    if (realPaw) {
      const r = realPaw.getBoundingClientRect();
      targetX = r.left;
      targetY = r.top;
    }

    // Restart the sparkle trail for the flight (it was stopped when hops
    // began). Stopped again on land.
    startTrail();

    // One-shot: when the flight transition ends, start the loading screen
    // phase. (The staged reveal is now reached only AFTER the 8s loading
    // screen finishes — see endLoadingScreen → revealAfterLand.)
    const onLand = (e) => {
      if (e.propertyName !== 'transform') return;
      bootPaw.removeEventListener('transitionend', onLand);
      stopTrail();
      // One final big burst on landing — a celebratory capstone.
      // (Finish SFX already fired via the pre-land timer in flyPawHome so its
      // tail rings out through landing — no duplicate play here.)
      spawnSparkles(14, 1);
      // Drop .booting NOW so the top bar fades in immediately and stays
      // visible through the loading screen (the loading screen sits BELOW
      // the top bar in z-order — see #boot-loading in styles.css). The
      // body's bg also flips transparent → #02040a here, but the loading
      // overlay covers it. The dock is held back by .loading until the
      // loading screen ends.
      document.body.classList.remove('booting');
      document.body.classList.add('loading');
      // Fade the boot paw out + remove it (v0.6.5 behavior). The fly-in set
      // inline `opacity:1` + inline `transition` on the paw (lines above);
      // both must be cleared so the `.fade-out` class's `opacity:0` (CSS)
      // applies + a transition fires for the removal callback. The fallback
      // setTimeout(remove, 350) guarantees the paw is gone even if
      // transitionend never fires (interrupted transition / hidden tab) —
      // without this the paw lingered as the "ghost paw" overlapping the
      // running OS indefinitely. 1s dwell lets the landing sparkle read.
      setTimeout(() => {
        if (!bootPaw) return;
        bootPaw.style.opacity = '';
        bootPaw.style.transition = '';
        bootPaw.classList.add('fade-out');
        setTimeout(() => bootPaw.remove(), 350);
      }, 1000);
      startLoadingScreen();
    };
    bootPaw.addEventListener('transitionend', onLand);

    // Straight-line flight via a single CSS transition: set the transform
    // target, the browser's compositor interpolates a linear diagonal from
    // the current position (center, post-hop-2) to the top-left corner.
    // easeInOut so the launch + landing are smooth (no snap). Scale shrinks
    // 2.8 → 1 over the same transition (composed in one matrix = one layer).
    // rAF double-buffer so the browser commits the start transform before
    // we set the target, guaranteeing the transition runs.
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        bootPaw.style.transition =
          `transform ${FLIGHT_DURATION_MS}ms cubic-bezier(0.45, 0, 0.55, 1), opacity 0.3s ease-out`;
        bootPaw.style.transform =
          `translate(${targetX}px, ${targetY}px) scale(1)`;
      });
    });
  }

  // ── Loading screen phase (between paw-land and the staged reveal).
  //    Fades in the violet abyss overlay, populates "LOADING OS . . ." as
  //    per-character spans that light L→R across LOADING_DURATION_MS,
  //    streams cosmetic boot lines into the terminal, and emits the real
  //    "✓ model ready" milestone line when Rust's model-status:ready fires.
  //    After LOADING_DURATION_MS, fades out and calls revealAfterLand().
  let loadingTimerHandle = null;
  let loadingEnded = false;

  function startLoadingScreen() {
    if (!bootLoading) {
      // No overlay → no terminal to stream into. Run the gate inline and
      // proceed straight to reveal. (Defensive: the HTML should always have
      // #boot-loading; this guards a malformed build.)
      runBootGate().then(() => revealAfterLand()).catch(() => revealAfterLand());
      return;
    }

    // Populate the loading text as one <span> per character so each can be
    // lit independently. Non-space chars get a span; spaces get a plain
    // space text node so layout spacing stays correct. The spans are NOT
    // lit yet — they stay dark until the boot gate (update check) resolves.
    if (bootLoadingText) {
      bootLoadingText.innerHTML = '';
      let spanIdx = 0;
      for (const ch of LOADING_TEXT) {
        if (ch === ' ') bootLoadingText.appendChild(document.createTextNode(' '));
        else {
          const s = document.createElement('span');
          s.textContent = ch;
          // Per-span negative animation-delay so each letter floats
          // out-of-phase (the bootCharFloat keyframe loops infinitely;
          // offsetting the start makes them shimmer independently instead
          // of bobbing in unison). ~0.32s between letters = clearly
          // desynchronized but still reads as one word.
          s.style.animationDelay = `-${(spanIdx * 0.32).toFixed(2)}s`;
          bootLoadingText.appendChild(s);
          spanIdx++;
        }
      }
    }

    // Fade the overlay in. LOADING OS text is dark (un-lit); the gate owns
    // the moment when it sweeps magenta L→R (post-update-check).
    bootLoading.classList.add('show');

    // Run the boot gate (update check first; model load + LOADING OS fill
    // do NOT start until it resolves). If an update is found, the gate
    // installs + restarts and never returns. Otherwise it lights the spans,
    // starts the cosmetic stream, calls boot_load_model, and starts the 8s
    // timer that ends the loading screen.
    runBootGate().catch((err) => {
      console.error('[Wupi] boot gate failed; proceeding to reveal', err);
      proceedAfterGate();
    });
  }

  // ── The boot gate: update check → install (if any) → proceed.
  //    The LOADING OS text + cosmetic terminal stream + model load are
  //    HELD DARK until this resolves. On update-found: install + restart
  //    (process dies, never proceeds). On up-to-date OR check failure
  //    (network/manifest unreachable): call proceedAfterGate() so a
  //    network blip can't strand the user on the loading screen.
  async function runBootGate() {
    // Surface the outcome of an update that just relanched us (if any). Rust
    // consumes (deletes) the marker, so this fires exactly once per update.
    try {
      const result = await invoke('updater_consume_result');
      if (result) {
        if (result.ok) {
          appendTerminalLine(`› updated to v${result.version} ✓`, false);
          // The update applied but the updater's relaunch retries all failed
          // (transient lock on the freshly-written exe) — this boot is a
          // manual launch. Honest, one line, no error styling.
          if (result.relaunched === false) {
            appendTerminalLine('› auto-restart failed — this was a manual launch', false);
          }
        } else {
          appendTerminalLine(
            `› last update failed: ${String(result.error || '').slice(0, 80)}`,
            false
          );
        }
      }
    } catch (e) {
      /* non-fatal — no marker to read */
    }

    // Stream the gate steps into the boot terminal as they happen so the
    // user sees exactly what's blocking the LOADING OS fill.
    appendTerminalLine('› checking for updates...', false);
    let current = 'unknown';
    try { current = await getVersion(); } catch (e) { /* fall through with 'unknown' */ }
    // Stash for the cosmetic terminal banner (TERMINAL_LINES[0]) so it shows
    // the real version instead of a stale hardcoded one.
    bootVersion = current;
    appendTerminalLine(`› current version ${current}`, false);

    let update = null;
    try {
      update = await invoke('updater_check');
    } catch (err) {
      // Don't block boot on a network/manifest error. Log + proceed.
      console.warn('[Wupi] boot update check failed (non-fatal)', err);
      appendTerminalLine('› update check failed — proceeding', false);
      proceedAfterGate();
      return;
    }

    if (!update) {
      appendTerminalLine('› no new updates found', false);
      proceedAfterGate();
      return;
    }

    // Update found — install + restart. The model never loads (no wasted
    // CPU/VRAM on a process about to be replaced). The update-progress
    // event still drives a progress bar in the paw-menu panel if the user
    // happened to have it open, but on the boot path the terminal lines
    // are the visible cue.
    appendTerminalLine(`› update version ${update.version} found`, false);
    appendTerminalLine('› installing update please wait..', false);
    // apply downloads the zip, then spawns updater.exe + EXITS this process
    // (the temp-staged handoff). The await NEVER resolves on success — the
    // process is gone; the relaunched boot surfaces the outcome via
    // updater_consume_result at the top of the next runBootGate. Only a
    // staging/spawn failure throws, in which case we proceed with the current
    // binary (the user can retry from the paw-menu panel).
    try {
      await invoke('updater_apply', { update });
    } catch (err) {
      console.error('[Wupi] updater_apply failed during boot gate', err);
      appendTerminalLine(`› update failed: ${String(err?.message || err).slice(0, 80)}`, false);
      appendTerminalLine('› proceeding with current version', false);
      proceedAfterGate();
    }
  }

  // Post-gate continuation: light the LOADING OS text, kick off the model
  // load + cosmetic terminal stream, start the 8s timer that ends the
  // loading screen. Called when the gate resolves up-to-date OR fails
  // (defensive — boot must never hang on the loading screen).
  function proceedAfterGate() {
    // Light the spans L→R staggered across the duration. Each span gets
    // .lit at progressively later moments so the magenta fill sweeps across
    // the whole word as progress climbs to 100%.
    const spans = bootLoadingText ? bootLoadingText.querySelectorAll('span') : [];
    const perChar = LOADING_DURATION_MS / Math.max(spans.length, 1);
    spans.forEach((sp, i) => {
      setTimeout(() => sp.classList.add('lit'), i * perChar);
    });

    // Terminal stream: cosmetic OS-flavored boot lines, one every ~330ms.
    // The "✓ model ready" milestone is emitted separately by the
    // model-status listener (see below) when the real event fires.
    startTerminalStream();

    // Trigger the deferred chat-model spawn (boot_load_model IPC). The
    // actual load runs on Rust's loader thread; the model-status:ready
    // event fires when it's done and the terminal listener emits the
    // milestone. Fire-and-forget — we don't await this; the 8s loading
    // timer hides any remaining load.
    invoke('boot_load_model').catch((e) => {
      console.error('[Wupi] boot_load_model failed', e);
    });

    // End the loading screen after the full duration. revealAfterLand
    // (called by endLoadingScreen) is what drops .booting and starts the
    // starry sky + aurora wipe.
    loadingTimerHandle = setTimeout(endLoadingScreen, LOADING_DURATION_MS);
  }

  function endLoadingScreen() {
    if (loadingEnded) return;
    loadingEnded = true;
    stopTerminalStream();
    if (bootLoading) {
      bootLoading.classList.add('fade-out');
      // Remove from DOM after the crossfade completes so it can't intercept
      // clicks (pointer-events:none in CSS, but cleanliness).
      bootLoading.addEventListener('transitionend', () => bootLoading.remove(), { once: true });
    }
    // Drop .loading → releases the dock (its CSS rule keys on
    // :not(.loading)). The top bar is already visible (it faded in at
    // paw-land and stayed visible through loading).
    document.body.classList.remove('loading');
    // If the model-ready milestone never fired (still loading), emit it
    // anyway so the terminal doesn't look like it gave up. The reveal
    // proceeds regardless — the boot animation no longer gates on the
    // model (the loading screen is the gate, hiding any remaining load).
    if (!milestoneEmitted) appendTerminalLine('› still loading — proceeding to UI', false);
    // The staged reveal: starry sky paints → aurora wipes. (.booting was
    // already dropped at paw-land so the top bar could appear; revealAfterLand
    // tolerates a redundant classList.remove.)
    revealAfterLand();
  }

  // ── Terminal stream. Cosmetic OS boot lines. The "model ready" milestone
  //    is special: it's only emitted when the real model-status:ready event
  //    fires (listened below). Other lines are fake but flavored to look real.
  const TERMINAL_LINES = [
    `› wupi v${bootVersion} — WUPI.gguf`,
    '› initializing kernel...',
    '› mounting shared_backend()...',
    '› allocating LlamaContext: chat (n_ctx=2048)',
    '› allocating LlamaContext: embedder (n_ctx=512)',
    '› allocating LlamaContext: schema (n_ctx=2048)',
    '› allocating LlamaContext: game (n_ctx=3072)',
    '› loading WUPI.gguf (9.79 GB, Q6_K)...',
    '› calibrating bge-small-en-v1.5 embedder...',
    '› embedder self-test: cosine check...',
    '› mounting memory.sqlite (WAL, FTS5, vec0)...',
    '› seeding codex from data/docs/...',
    '› loading data/user.xml (user profile)...',
    '› loading data/wupi.sim (persona card)...',
    '› arming schema-delta engine...',
    '› arming narrator engine...',
    '› KV cache: Q8_0 type-k/type-v',
    '› sampler: temp(0.85) top_p(0.95) min_p(0.1) dist(0)',
    '› canvas: aurora borealis (5 curtains, blur 30px)',
    '› render loop: paused=true (dormant)',
    '› boot paw: parked below viewport',
    '› awaiting model-ready milestone...',
  ];
  let terminalTimer = null;
  let terminalIdx = 0;
  let milestoneEmitted = false;

  function appendTerminalLine(text, isMilestone) {
    if (!bootTerminal) return;
    // Single-line status readout: REPLACE whatever line is showing instead of
    // stacking. The CSS fade-in keyframe runs on the fresh node each swap, so
    // each new boot line crossfades in over the last. No accumulation, no DOM
    // cap needed — the container holds exactly one .boot-terminal-line at a
    // time. All three sources (cosmetic drip, boot-gate, model-status) funnel
    // through here, so they all just overwrite the same one line.
    bootTerminal.innerHTML = '';
    const line = document.createElement('div');
    line.className = 'boot-terminal-line' + (isMilestone ? ' milestone' : '');
    line.textContent = text;
    bootTerminal.appendChild(line);
  }

  function startTerminalStream() {
    terminalIdx = 0;
    const tick = () => {
      if (terminalIdx < TERMINAL_LINES.length) {
        appendTerminalLine(TERMINAL_LINES[terminalIdx], false);
        terminalIdx++;
      }
    };
    // First line immediately, then steady drip.
    tick();
    terminalTimer = setInterval(tick, 330);
  }

  function stopTerminalStream() {
    if (terminalTimer) { clearInterval(terminalTimer); terminalTimer = null; }
  }

  // ── Model-ready milestone listener. Emits the terminal line when the
  //    model finishes loading. The boot animation no longer gates on this
  //    (the loading screen hides any remaining load); this just reports
  //    status to the terminal stream.
  listen('model-status', (e) => {
    if (milestoneEmitted) return;
    const s = e?.payload?.status;
    if (s === 'ready') {
      milestoneEmitted = true;
      appendTerminalLine('✓ model ready — WUPI.gguf loaded', true);
    } else if (s === 'missing') {
      // First-run: no GGUFs. The download overlay (setupModelDownloadGate)
      // has already taken over the screen by the time this fires, but emit
      // a milestone line anyway so the terminal reflects reality if the
      // overlay is ever bypassed.
      milestoneEmitted = true;
      appendTerminalLine('! models missing — download required', true);
    } else if (s === 'no_model' || s === 'error') {
      milestoneEmitted = true;
      appendTerminalLine('! model unavailable — echo fallback', true);
    }
  }).catch(() => {});

  // ── Phase 3: staged reveal. Called when the loading screen ends. Each
  //    step is a setTimeout off that moment.
  function revealAfterLand() {
    // +0.0s: drop .booting. Body goes opaque #02040a (CSS), AND the top-bar
    // + dock opacity transitions arm (their CSS rules key off :not(.booting)).
    // The top-bar starts fading in immediately (0.1s CSS delay, 0.6s fade).
    document.body.classList.remove('booting');

    // +0.2s: start the canvas RAF. First frame paints sky + stars only
    // (curtain block gated on auroraIntensity > 0.001, still 0 here, AND
    // auroraRevealX still ~0 so even if it weren't, nothing would draw).
    setTimeout(() => {
      bootDone = true;
      startLoop();
    }, DELAY_SKY);

    // +0.4s: fade + remove the boot paw. The top-bar is well into its fade
    // by now, so the real .paw-img reads as a continuous handoff.
    setTimeout(() => {
      if (!bootPaw) return;
      bootPaw.classList.add('fade-out');
      bootPaw.addEventListener('transitionend', () => bootPaw.remove(), { once: true });
    }, DELAY_PAW_REMOVE);

    // +0.8s: arm the aurora ramp + the left-to-right wipe. Staged AFTER the
    // top-bar's 0.6s fade (which started at +0.1s) so the two blur costs
    // don't overlap — this is the real fix for "aurora load-in looks laggy".
    // The buffer is NEVER frozen (an earlier version froze it to save per-
    // frame curtain redraws, but that caused a visible snap when the freeze
    // released because `time` advanced while the buffer didn't). The
    // single-blur-pass optimization on the composite carries the wipe cheaply.
    setTimeout(() => {
      auroraRampStart = performance.now();
      auroraRevealStart = performance.now();
    }, DELAY_AURORA);
  }
})();

// NOTE: this file is loaded as type="module", which defers execution until
// after the DOM is parsed: so DOMContentLoaded has ALREADY fired by the time
// we run. Do NOT wrap the wiring in a DOMContentLoaded listener (it would
// never execute). The elements below all exist at module-eval time.
const pawBtn = document.getElementById('pawBtn');
const dropdownMenu = document.getElementById('dropdownMenu');
  const clockBtn = document.getElementById('clockBtn');
  const clockDropdownMenu = document.getElementById('clockDropdownMenu');
  const digitalTimeEl = document.getElementById('digitalTime');
  const calendarBtn = document.getElementById('calendarBtn');
  const calendarDropdownMenu = document.getElementById('calendarDropdownMenu');
  const dateDisplayEl = document.getElementById('dateDisplay');
  // v0.6.5 text-tile calendar: month strip + day number (replaces the old
  // SVG grid). Null-safe so a malformed DOM doesn't crash the clock loop.
  const calMonthEl = document.getElementById('calMonth');
  const calDayEl = document.getElementById('calDay');
  
  // New UI Elements
  const wifiBtn = document.getElementById('wifiBtn');
  const wifiDropdownMenu = document.getElementById('wifiDropdownMenu');
  const bluetoothBtn = document.getElementById('bluetoothBtn');
  const bluetoothDropdownMenu = document.getElementById('bluetoothDropdownMenu');
  const audioBtn = document.getElementById('audioBtn');
  const audioDropdownMenu = document.getElementById('audioDropdownMenu');
  
  const hourHand = document.querySelector('.hour-hand');
  const minuteHand = document.querySelector('.minute-hand');

  function toggleDropdown(menu, event) {
    event.stopPropagation();
    const isOpen = menu.classList.contains('show');
    
    // Clear all open menus
    dropdownMenu.classList.remove('show');
    clockDropdownMenu.classList.remove('show');
    calendarDropdownMenu.classList.remove('show');
    wifiDropdownMenu.classList.remove('show');
    bluetoothDropdownMenu.classList.remove('show');
    audioDropdownMenu.classList.remove('show');
    
    if (!isOpen) {
      menu.classList.add('show');
    }
  }

  pawBtn.addEventListener('click', (e) => toggleDropdown(dropdownMenu, e));
  clockBtn.addEventListener('click', (e) => toggleDropdown(clockDropdownMenu, e));
  calendarBtn.addEventListener('click', (e) => toggleDropdown(calendarDropdownMenu, e));
  wifiBtn.addEventListener('click', (e) => toggleDropdown(wifiDropdownMenu, e));
  bluetoothBtn.addEventListener('click', (e) => toggleDropdown(bluetoothDropdownMenu, e));
  audioBtn.addEventListener('click', (e) => toggleDropdown(audioDropdownMenu, e));

  // The three power commands exposed by system_menu.rs. Each closes the
  // dropdown first so it doesn't flash on the next launch. Also closes any
  // open cascade extension (theme/color/update) — those are visually
  // children of the paw menu, so they should never outlive it.
  const closePawMenu = () => {
    dropdownMenu.classList.remove('show');
    document.getElementById('themePanel')?.classList.remove('show');
    document.getElementById('colorCodePanel')?.classList.remove('show');
    document.getElementById('updatePanel')?.classList.remove('show');
  };

  document.getElementById('shutdownBtn')?.addEventListener('click', () => {
    // (#64) Same rapid-click guard as Restart: Sleep/Shut Down were left
    // unwrapped, so a double-tap fired the power command twice.
    withShellBusy(async () => {
      closePawMenu();
      invoke('power_shutdown_cmd');
    });
  });
  document.getElementById('restartBtn')?.addEventListener('click', () => {
    // Shell-wide rapid-click guard: a double-tap on Restart could fire two
    // power_restart_cmd invokes (two spawned children racing). The async
    // wrapper lets withShellBusy auto-clear; restart exits the process soon
    // anyway, so the 12s safety net never matters here.
    withShellBusy(async () => {
      closePawMenu();
      invoke('power_restart_cmd');
    });
  });
  document.getElementById('sleepBtn')?.addEventListener('click', () => {
    // (#64) Mirrors Shutdown/Restart — sleep is idempotent but the double
    // invoke raced the canvas-pause event handling.
    withShellBusy(async () => {
      closePawMenu();
      invoke('power_sleep_cmd');
    });
  });

  // ── Check for Updates. Drives the tauri-plugin-updater flow from the JS
  //    side, surfaced as an inline cascade panel (#updatePanel) — NO browser
  //    alert()/confirm(). The paw-menu item toggles the panel open (mirroring
  //    the Theme cascade pattern): the paw menu stays open + the update panel
  //    slides out to its right. Opening the panel auto-fires a check.
  //
  //    States are driven by setUpdateState(): idle → checking → (up-to-date |
  //    available → installing → relaunch) | error. The available state shows
  //    a magenta "Install & Restart" button that calls downloadAndInstall()
  //    with a streaming progress bar.
  const updateStatusEl = document.getElementById('updateStatus');
  const updateVersionEl = document.getElementById('updateCurrentVersion');
  let pendingUpdate = null;   // cached Update from last check() — drives install

  // Populate the running version once (best-effort; failure leaves the dash).
  getVersion().then((v) => { if (updateVersionEl) updateVersionEl.textContent = v; })
    .catch((e) => console.warn('[Wupi] getVersion failed', e));

  // Swap the status zone's innerHTML + data-state for one of:
  //   'idle' | 'checking' | 'up-to-date' | 'available' | 'installing' | 'error'
  // payload carries version/notes/percent/message as relevant to the state.
  function setUpdateState(state, payload = {}) {
    if (!updateStatusEl) return;
    updateStatusEl.dataset.state = state;
    const escape = (s) => String(s).replace(/[&<>"']/g, (c) => ({
      '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
    }[c]));
    if (state === 'idle') {
      updateStatusEl.innerHTML = `
        <div class="update-hint">Click to check for updates</div>
        <button class="dropdown-item update-action-btn" id="updateCheckBtn">
          <svg class="menu-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" stroke-linecap="round" stroke-linejoin="round"/>
          </svg>
          Check Now
        </button>`;
      document.getElementById('updateCheckBtn')?.addEventListener('click', (e) => {
        e.stopPropagation();
        runUpdateCheck();
      });
    } else if (state === 'checking') {
      updateStatusEl.innerHTML = `
        <div class="update-status-row">
          <svg class="menu-svg update-spin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
            <path d="M21 12a9 9 0 1 1-2.64-6.36M21 3v6h-6" stroke-linecap="round" stroke-linejoin="round"/>
          </svg>
          <span>Checking…</span>
        </div>`;
    } else if (state === 'up-to-date') {
      updateStatusEl.innerHTML = `
        <div class="update-status-row">
          <span class="status-dot connected"></span>
          <span>WUPI is up to date</span>
        </div>`;
    } else if (state === 'available') {
      const notesHtml = payload.notes
        ? `<div class="update-notes">${escape(payload.notes)}</div>`
        : '';
      updateStatusEl.innerHTML = `
        <div class="update-status-row">
          <span class="status-dot update-available-dot"></span>
          <span>v${escape(payload.version)} available</span>
        </div>
        ${notesHtml}
        <button class="dropdown-item update-install-btn" id="updateInstallBtn">
          Install &amp; Restart
        </button>`;
      document.getElementById('updateInstallBtn')?.addEventListener('click', (e) => {
        e.stopPropagation();
        runUpdateInstall();
      });
    } else if (state === 'installing') {
      const pct = payload.percent != null ? Math.min(100, Math.max(0, payload.percent)) : 0;
      updateStatusEl.innerHTML = `
        <div class="update-status-row">
          <span>Installing… ${pct.toFixed(0)}%</span>
        </div>
        <div class="update-progress-track">
          <div class="update-progress-bar" style="width:${pct}%"></div>
        </div>`;
    } else if (state === 'error') {
      updateStatusEl.innerHTML = `
        <div class="update-status-row">
          <span class="status-dot update-error-dot"></span>
          <span class="update-error-text">${escape(payload.message || 'Update failed')}</span>
        </div>
        <button class="dropdown-item update-action-btn" id="updateRetryBtn">
          Retry
        </button>`;
      document.getElementById('updateRetryBtn')?.addEventListener('click', (e) => {
        e.stopPropagation();
        runUpdateCheck();
      });
    }
  }

  // Run the portable updater's check IPC. Caches the result in
  // `pendingUpdate` for the install path. Errors → error state (no alert()).
  async function runUpdateCheck() {
    setUpdateState('checking');
    try {
      const update = await invoke('updater_check');
      pendingUpdate = update || null;
      if (update) {
        setUpdateState('available', { version: update.version, notes: update.notes || '' });
      } else {
        setUpdateState('up-to-date');
      }
    } catch (e) {
      setUpdateState('error', { message: String(e?.message || e) });
    }
  }

  // Install the cached pendingUpdate via the portable updater's apply IPC,
  // then restart. Progress streams from the `update-progress` event into the
  // installing-state progress bar (set up once in setupUpdateProgressListener).
  async function runUpdateInstall() {
    if (!pendingUpdate) {
      setUpdateState('error', { message: 'No pending update — check again.' });
      return;
    }
    setUpdateState('installing', { percent: 0 });
    // apply downloads, then spawns updater.exe + EXITS this process. The await
    // never resolves on success — the process is gone, + the relaunched boot
    // surfaces the outcome via updater_consume_result. Only a failure throws.
    try {
      await invoke('updater_apply', { update: pendingUpdate });
      // Unreachable on success (process exited). Defensive only.
      setUpdateState('installing', { percent: 100 });
    } catch (e) {
      setUpdateState('error', { message: String(e?.message || e) });
    }
  }

  // Subscribe to the `update-progress` event (emitted by updater_apply's
  // download phase) and route percent into the installing-state progress bar.
  // Fires only while the state is 'installing' — at other times the events
  // are ignored (defensive: a late event after a state change shouldn't
  // overwrite an error or up-to-date message).
  listen('update-progress', (e) => {
    if (updateStatusEl?.dataset.state !== 'installing') return;
    const pct = e?.payload?.percent;
    if (typeof pct === 'number') {
      setUpdateState('installing', { percent: pct });
    }
  }).catch((e) => console.warn('[Wupi] update-progress listen failed', e));

  // Initialize the panel once: idle state. Subsequent opens preserve the
  // last state so a user who already saw "v0.2.0 available" doesn't lose it
  // by closing + reopening the panel.
  setUpdateState('idle');

  // Paw-menu trigger: toggle the cascade panel. Mutual-excludes the theme
  // cascade (only one open at a time matches the existing UX). stopPropagation
  // prevents the document-click dismiss handler from immediately closing it.
  document.getElementById('checkForUpdatesBtn')?.addEventListener('click', (e) => {
    e.stopPropagation();
    themePanel?.classList.remove('show');
    colorCodePanel?.classList.remove('show');
    const open = updatePanel?.classList.toggle('show');
    if (open && updateStatusEl?.dataset.state === 'idle') {
      // First-open auto-fire: save the user one click. Subsequent opens keep
      // the last state visible.
      runUpdateCheck();
    }
  });

  // Three aligned panels. Clicking Theme opens panel 2; clicking a theme opens
  // panel 3 (color codes); clicking a color code persists + applies live. The
  // document-click dismiss handler (below) closes all three on outside click.
  const themePanel = document.getElementById('themePanel');
  const colorCodePanel = document.getElementById('colorCodePanel');
  // updatePanel is declared once here so the click handler above and the
  // dismiss handler below both see it (const is not hoisted).
  const updatePanel = document.getElementById('updatePanel');

  // Apply a theme + color code to the running canvas. Unknown color codes
  // silently fall back to Vibrant so a stale theme.json can't break the loop.
  function applyTheme(theme, colorCode) {
    currentPalette = COLOR_CODES[colorCode] || COLOR_CODES.Vibrant;
    // Sky gradient is palette-specific; invalidate the cache so the next
    // frame rebuilds it from the new skyGradient stops (skyGradient()).
    cachedSkyGrad = null;
    // Recolor the OS chrome live via the --ui-accent* CSS vars (consumed in
    // styles.css). Vibrant's triplet = the original hardcoded magenta, so its
    // accent is unchanged; every other code swaps in a darker, muted variant
    // of its aurora hue so accents follow the theme without going neon.
    const p = currentPalette;
    const rootEl = document.documentElement;
    const root = rootEl.style;
    root.setProperty('--ui-accent', p.uiAccent);
    root.setProperty('--ui-accent-bright', p.uiAccentBright);
    root.setProperty('--ui-accent-deep', p.uiAccentDeep);
    // Mirror the active color code as an attribute on <html> so CSS can scope
    // overrides to a single code (e.g. the LOADING OS text's Vibrant magenta).
    rootEl.dataset.colorcode = colorCode;
    // Mark the selected option in each panel (the `.selected` highlight).
    document.querySelectorAll('.theme-option').forEach((el) => {
      el.classList.toggle('selected', el.dataset.theme === theme);
    });
    document.querySelectorAll('.colorcode-option').forEach((el) => {
      el.classList.toggle('selected', el.dataset.colorcode === colorCode);
    });
  }

  // Load the persisted theme on boot and paint the cascade selection state.
  invoke('theme_get')
    .then((t) => { if (t) applyTheme(t.theme, t.colorCode); })
    .catch((e) => console.warn('[Wupi] theme_get failed', e));

  document.getElementById('themeBtn')?.addEventListener('click', (e) => {
    e.stopPropagation();
    // Toggle the theme panel; keep the paw menu open so the cascade reads as
    // an extension of it. Close the update cascade if open (mutual exclusion:
    // only one cascade extension of the paw menu at a time).
    updatePanel?.classList.remove('show');
    const open = themePanel.classList.toggle('show');
    if (!open) colorCodePanel.classList.remove('show');
  });

  // Terminal paw-menu button REMOVED (2026-07-31): the terminal drawer +
  // its terminalPanel() IIFE were dead (no #terminal DOM, and the
  // terminal_* IPCs were never registered in Rust). The paw-menu entry is
  // gone with it.

  document.querySelectorAll('.theme-option').forEach((el) => {
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      // Selecting a theme opens the color-code panel (cascade level 3).
      applyTheme(el.dataset.theme,
        document.querySelector('.colorcode-option.selected')?.dataset.colorcode || 'Vibrant');
      colorCodePanel.classList.add('show');
    });
  });

  document.querySelectorAll('.colorcode-option').forEach((el) => {
    el.addEventListener('click', (e) => {
      e.stopPropagation();
      const themeName = document.querySelector('.theme-option.selected')?.dataset.theme || 'Aurora';
      const cc = el.dataset.colorcode;
      applyTheme(themeName, cc);
      invoke('theme_set', { themeName, colorCode: cc }).catch((err) =>
        console.warn('[Wupi] theme_set failed', err)
      );
    });
  });

  document.addEventListener('click', () => {
    dropdownMenu.classList.remove('show');
    clockDropdownMenu.classList.remove('show');
    calendarDropdownMenu.classList.remove('show');
    wifiDropdownMenu.classList.remove('show');
    bluetoothDropdownMenu.classList.remove('show');
    audioDropdownMenu.classList.remove('show');
    themePanel?.classList.remove('show');
    colorCodePanel?.classList.remove('show');
    updatePanel?.classList.remove('show');
    // (#38) This path closes the audio menu WITHOUT the toggle handler's
    // cleanup — without clearing the poll here it kept firing the 1 Hz
    // audio_get_state IPC forever after any outside-click dismissal.
    clearInterval(audioPollTimer);
    audioPollTimer = null;
  });

  const wifiIcon = wifiBtn.querySelector('.status-icon');

  // Wired (Ethernet) detection: when a physical cabled NIC is up, it replaces
  // the Wi-Fi indicator entirely — the icon swaps to the wired glyph and the
  // dropdown becomes a simple "Connected" panel. Ethernet takes precedence
  // over Wi-Fi in the status bar.
  function showEthernetMode(name) {
    const wifiGlyph = wifiIcon.querySelector('.wifi-glyph');
    const ethGlyph = wifiIcon.querySelector('.ethernet-glyph');
    if (wifiGlyph) wifiGlyph.style.display = 'none';
    if (ethGlyph) ethGlyph.style.display = '';
    wifiIcon.classList.remove('disabled');
    // Rebuild the dropdown as a centered, NON-interactive status panel. The
    // adapter `name` from NDIS is a verbose Windows device string
    // ("Intel(R) Ethernet Connection (17) I219-LM") — noisy and useless here,
    // so it is dropped from the UI. Just a centered "Connected" status.
    wifiDropdownMenu.innerHTML = '';
    const title = document.createElement('div');
    title.className = 'dropdown-status-title';
    title.textContent = 'Ethernet';
    wifiDropdownMenu.appendChild(title);
    wifiDropdownMenu.appendChild(document.createElement('div')).className = 'dropdown-divider';
    const row = document.createElement('div');
    row.className = 'ethernet-status-row';
    row.innerHTML = `<span class="status-dot connected"></span><span class="ethernet-status-text">Connected</span>`;
    wifiDropdownMenu.appendChild(row);
  }

  function showWifiMode() {
    // Restore the Wi-Fi glyph; the dropdown is repopulated by refreshWifiWifi().
    const wifiGlyph = wifiIcon.querySelector('.wifi-glyph');
    const ethGlyph = wifiIcon.querySelector('.ethernet-glyph');
    if (wifiGlyph) wifiGlyph.style.display = '';
    if (ethGlyph) ethGlyph.style.display = 'none';
    // Reset to the base markup the Wi-Fi logic expects.
    wifiDropdownMenu.innerHTML =
      '<div class="dropdown-status-title">Wi-Fi Network</div>' +
      '<div class="dropdown-divider"></div>' +
      '<button class="dropdown-item wifi-toggle-row">' +
      '<span class="status-dot"></span>' +
      '<span class="toggle-text">Turn Wi-Fi On</span>' +
      '</button>';
  }

  function refreshWifiWifi() {
    // Current connection.
    invoke('wifi_get_current')
      .then((s) => {
        const dot = wifiDropdownMenu.querySelector('.wifi-toggle-row .status-dot');
        const toggleText = wifiDropdownMenu.querySelector('.wifi-toggle-row .toggle-text');
        if (s && s.connected) {
          dot?.classList.add('connected');
          wifiIcon.classList.remove('disabled');
          if (toggleText) toggleText.textContent = `Connected: ${s.ssid || '(unnamed)'}`;
        } else {
          dot?.classList.remove('connected');
          if (toggleText) toggleText.textContent = 'Turn Wi-Fi On';
        }
      })
      .catch((e) => console.warn('[Wupi] wifi_get_current failed', e));

    // Network list (deduped backend-side by SSID now). Rebuild only if absent
    // to avoid flicker; the toggle row above updates independently.
    const existingList = wifiDropdownMenu.querySelector('.scan-list');
    if (existingList) existingList.remove();
    invoke('wifi_scan')
      .then((nets) => {
        if (!nets || !nets.length) return;
        const list = document.createElement('div');
        list.className = 'scan-list';
        const header = document.createElement('div');
        header.className = 'dropdown-status-title';
        header.textContent = 'Available';
        list.appendChild(header);
        for (const n of nets) {
          const btn = document.createElement('button');
          btn.className = 'dropdown-item wifi-network';
          const lock = n.secure ? '🔒 ' : '';
          // No signal %: it was noisy and the same network appeared multiple
          // times at different strengths. SSID-only now (backend dedups).
          btn.innerHTML = `<span class="status-dot"></span>${lock}${n.ssid}`;
          btn.addEventListener('click', (ev) => {
            ev.stopPropagation();
            if (!n.secure) {
              invoke('wifi_connect', { ssid: n.ssid, password: null })
                .then(() => refreshWifi())
                .catch((err) => console.error('[Wupi] wifi_connect failed', err));
              return;
            }
            // Inline password row: native prompt() is dead in the Tauri
            // WebView (wry disables default script dialogs → returns null,
            // which used to silently send password:null for secure nets).
            // One row at a time; Enter connects, Esc/✕ dismisses.
            const stale = wifiDropdownMenu.querySelector('.wifi-pass-row');
            if (stale) stale.remove();
            const row = document.createElement('div');
            row.className = 'wifi-pass-row';
            const input = document.createElement('input');
            input.type = 'password';
            input.placeholder = `Password for ${n.ssid}`;
            input.autocomplete = 'off';
            const dismiss = () => row.remove();
            const submit = () => {
              const pw = input.value;
              row.remove();
              invoke('wifi_connect', { ssid: n.ssid, password: pw || null })
                .then(() => refreshWifi())
                .catch((err) => console.error('[Wupi] wifi_connect failed', err));
            };
            input.addEventListener('keydown', (ke) => {
              ke.stopPropagation();
              if (ke.key === 'Enter') submit();
              else if (ke.key === 'Escape') dismiss();
            });
            input.addEventListener('click', (ke) => ke.stopPropagation());
            const go = document.createElement('button');
            go.className = 'wifi-pass-go';
            go.textContent = 'Join';
            go.addEventListener('click', (ke) => { ke.stopPropagation(); submit(); });
            const x = document.createElement('button');
            x.className = 'wifi-pass-x';
            x.textContent = '✕';
            x.addEventListener('click', (ke) => { ke.stopPropagation(); dismiss(); });
            row.append(input, go, x);
            wifiDropdownMenu.appendChild(row);
            input.focus();
          });
          list.appendChild(btn);
        }
        wifiDropdownMenu.appendChild(list);
      })
      .catch((e) => console.warn('[Wupi] wifi_scan failed', e));
  }

  function refreshWifi() {
    // Ethernet takes precedence: when a cabled NIC is up, show the wired
    // panel and skip the Wi-Fi logic entirely. Otherwise fall through to the
    // normal Wi-Fi indicator + scan list.
    invoke('ethernet_get_state')
      .then((eth) => {
        if (eth && eth.connected) {
          showEthernetMode(eth.name);
        } else {
          showWifiMode();
          refreshWifiWifi();
        }
      })
      .catch((e) => {
        // Backend unavailable (e.g. non-Windows) — fall back to Wi-Fi.
        console.warn('[Wupi] ethernet_get_state failed', e);
        showWifiMode();
        refreshWifiWifi();
      });
  }

  // Refresh the active-link indicator once on load so the status icon reflects
  // Ethernet immediately, before the dropdown is ever opened.
  refreshWifi();

  // The Wi-Fi toggle row: disconnects when connected, connects (toggles radio)
  // when off. Windows exposes Wi-Fi radio via the WinRT Radio API (same as
  // Bluetooth), so we route through wifi_toggle_radio. Delegated on the menu
  // (not a captured element ref) because the row is rebuilt each refresh.
  wifiDropdownMenu.addEventListener('click', (e) => {
    const row = e.target.closest('.wifi-toggle-row');
    if (!row) return;
    e.stopPropagation();
    const dot = row.querySelector('.status-dot');
    const isOn = dot?.classList.contains('connected');
    invoke('wifi_toggle_radio', { on: !isOn })
      .then(() => refreshWifi())
      .catch((err) => console.error('[Wupi] wifi_toggle_radio failed', err));
  });

  wifiBtn.addEventListener('click', () => {
    setTimeout(() => {
      if (wifiDropdownMenu.classList.contains('show')) refreshWifi();
    }, 0);
  });

  const btToggle = document.querySelector('.bt-toggle-row');
  const btIcon = bluetoothBtn.querySelector('.status-icon');

  function refreshBluetooth() {
    invoke('bluetooth_get_state')
      .then((s) => {
        const dot = bluetoothDropdownMenu.querySelector('.bt-toggle-row .status-dot');
        const toggleText = btToggle.querySelector('.toggle-text');
        if (s && s.radio_on) {
          dot?.classList.add('connected');
          btIcon.classList.remove('disabled');
          toggleText.textContent = 'Turn Bluetooth Off';
        } else {
          dot?.classList.remove('connected');
          btIcon.classList.add('disabled');
          toggleText.textContent = 'Turn Bluetooth On';
        }
      })
      .catch((e) => console.warn('[Wupi] bluetooth_get_state failed', e));

    const existingList = bluetoothDropdownMenu.querySelector('.bt-device-list');
    if (existingList) existingList.remove();
    invoke('bluetooth_list_devices')
      .then((devs) => {
        if (!devs || !devs.length) return;
        const list = document.createElement('div');
        list.className = 'bt-device-list';
        const header = document.createElement('div');
        header.className = 'dropdown-status-title devices-header';
        header.textContent = 'My Devices';
        list.appendChild(header);
        for (const d of devs) {
          const btn = document.createElement('button');
          btn.className = 'dropdown-item device-opt';
          const state = d.connected ? '🟢 ' : (d.paired ? '⚪ ' : '');
          btn.innerHTML = `<span class="status-dot ${d.paired ? 'connected' : ''}"></span>${state}${d.name}`;
          if (!d.paired) {
            btn.addEventListener('click', (ev) => {
              ev.stopPropagation();
              invoke('bluetooth_pair', { deviceId: d.id })
                .then((ok) => { if (ok) refreshBluetooth(); })
                .catch((err) => console.error('[Wupi] bluetooth_pair failed', err));
            });
          }
          list.appendChild(btn);
        }
        bluetoothDropdownMenu.appendChild(list);
      })
      .catch((e) => console.warn('[Wupi] bluetooth_list_devices failed', e));
  }

  // The toggle row now actually flips the radio.
  btToggle.addEventListener('click', (e) => {
    e.stopPropagation();
    const isOff = btIcon.classList.contains('disabled');
    invoke('bluetooth_toggle_radio', { on: isOff })
      .then(() => refreshBluetooth())
      .catch((err) => console.error('[Wupi] bluetooth_toggle_radio failed', err));
  });

  bluetoothBtn.addEventListener('click', () => {
    setTimeout(() => {
      if (bluetoothDropdownMenu.classList.contains('show')) refreshBluetooth();
    }, 0);
  });

  // "Add Device": discover in-range unpaired BT devices and list them under
  // the button. Clicking one calls bluetooth_pair (Windows shows the native
  // PIN/confirmation UI for devices that need it).
  document.getElementById('btAddBtn')?.addEventListener('click', (e) => {
    e.stopPropagation();
    const existing = bluetoothDropdownMenu.querySelector('.bt-discover-list');
    if (existing) {
      existing.remove();
      return;
    }
    const list = document.createElement('div');
    list.className = 'bt-discover-list';
    const loading = document.createElement('div');
    loading.className = 'dropdown-status-title';
    loading.textContent = 'Searching…';
    list.appendChild(loading);
    bluetoothDropdownMenu.appendChild(list);
    invoke('bluetooth_discover')
      .then((devs) => {
        list.innerHTML = '';
        if (!devs || !devs.length) {
          const empty = document.createElement('div');
          empty.className = 'dropdown-status-title';
          empty.textContent = 'No devices found';
          list.appendChild(empty);
          return;
        }
        const header = document.createElement('div');
        header.className = 'dropdown-status-title';
        header.textContent = 'Available Devices';
        list.appendChild(header);
        for (const d of devs) {
          const btn = document.createElement('button');
          btn.className = 'dropdown-item';
          btn.innerHTML = `<span class="status-dot"></span>${d.name}`;
          btn.addEventListener('click', (ev) => {
            ev.stopPropagation();
            btn.textContent = 'Pairing…';
            invoke('bluetooth_pair', { deviceId: d.id })
              .then((ok) => {
                if (ok) refreshBluetooth();
                else btn.textContent = `${d.name} (failed)`;
              })
              .catch((err) => {
                console.error('[Wupi] bluetooth_pair failed', err);
                btn.textContent = `${d.name} (error)`;
              });
          });
          list.appendChild(btn);
        }
      })
      .catch((err) => {
        console.warn('[Wupi] bluetooth_discover failed', err);
        list.remove();
      });
  });

  const volumeSlider = document.getElementById('volumeSlider');
  const volumePercent = document.getElementById('volumePercent');
  const audioIcon = audioBtn.querySelector('.status-icon');

  // Set the audio icon based on a volume level (0 / low / high).
  function setAudioIcon(val) {
    if (val == 0) {
      audioIcon.innerHTML = `
        <svg class="status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
            <line x1="23" y1="9" x2="17" y2="15"></line>
            <line x1="17" y1="9" x2="23" y2="15"></line>
        </svg>`;
    } else if (val < 50) {
      audioIcon.innerHTML = `
        <svg class="status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
            <path d="M15.54 8.46a5 5 0 0 1 0 7.07"></path>
        </svg>`;
    } else {
      audioIcon.innerHTML = `
        <svg class="status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"></polygon>
            <path d="M19.07 4.93a10 10 0 0 1 0 14.14M15.54 8.46a5 5 0 0 1 0 7.07"></path>
        </svg>`;
    }
  }

  // Debounced volume set so dragging the slider doesn't spam IPC calls.
  let volTimer = null;
  volumeSlider.addEventListener('input', (e) => {
    const val = Number(e.target.value);
    volumePercent.textContent = `${val}%`;
    setAudioIcon(val);
    clearTimeout(volTimer);
    volTimer = setTimeout(() => {
      invoke('audio_set_volume', { volume: val }).catch((err) =>
        console.error('[Wupi] audio_set_volume failed', err)
      );
    }, 60);
  });

  // Split into two pieces to kill the flicker: the volume/mute is polled every
  // 1s (slider/percent/icon only: no DOM rebuild), and the output-device list
  // is built ONCE when the dropdown opens (it almost never changes mid-session).
  // The previous version rebuilt the whole list each tick → flicker.

  function refreshAudioVolume() {
    invoke('audio_get_state')
      .then((s) => {
        if (!s) return;
        // Only touch the slider/percent/icon: never rebuild the device list.
        volumeSlider.value = s.volume;
        volumePercent.textContent = `${s.volume}%`;
        setAudioIcon(s.muted ? 0 : s.volume);
      })
      .catch((e) => console.warn('[Wupi] audio_get_state failed', e));
  }

  function buildAudioOutputs() {
    const existingList = audioDropdownMenu.querySelector('.output-list');
    if (existingList) existingList.remove();
    invoke('audio_list_outputs')
      .then((outs) => {
        if (!outs || !outs.length) return;
        const list = document.createElement('div');
        list.className = 'output-list';
        const header = document.createElement('div');
        header.className = 'dropdown-status-title';
        header.textContent = 'Output';
        list.appendChild(header);
        for (const o of outs) {
          const btn = document.createElement('button');
          btn.className = 'dropdown-item output-option' + (o.is_default ? ' selected' : '');
          btn.innerHTML = `<span class="status-dot ${o.is_default ? 'connected' : ''}"></span>${o.name}`;
          if (!o.is_default) {
            btn.addEventListener('click', (ev) => {
              ev.stopPropagation();
              invoke('audio_set_default_output', { id: o.id })
                .then(() => buildAudioOutputs())
                .catch((err) => console.error('[Wupi] audio_set_default_output failed', err));
            });
          }
          list.appendChild(btn);
        }
        audioDropdownMenu.appendChild(list);
      })
      .catch((e) => console.warn('[Wupi] audio_list_outputs failed', e));
  }

  let audioPollTimer = null;
  audioBtn.addEventListener('click', () => {
    setTimeout(() => {
      if (audioDropdownMenu.classList.contains('show')) {
        // Opened: build the device list once + load volume, then poll volume only.
        buildAudioOutputs();
        refreshAudioVolume();
        clearInterval(audioPollTimer);
        audioPollTimer = setInterval(refreshAudioVolume, 1000);
      } else {
        clearInterval(audioPollTimer);
        audioPollTimer = null;
      }
    }, 0);
  });

  function updateClocks() {
    const now = new Date();
    const seconds = now.getSeconds();
    const minutes = now.getMinutes();
    const hours = now.getHours();

    const minuteDegrees = ((minutes / 60) * 360) + ((seconds / 60) * 6);
    const hourDegrees = ((hours / 12) * 360) + ((minutes / 60) * 30);

    hourHand.style.transform = `translate(-50%) rotate(${hourDegrees}deg)`;
    minuteHand.style.transform = `translate(-50%) rotate(${minuteDegrees}deg)`;

    let displayHours = hours;
    const displayMinutes = String(minutes).padStart(2, '0');
    const displaySeconds = String(seconds).padStart(2, '0');
    const ampm = displayHours >= 12 ? 'PM' : 'AM';
    
    displayHours = displayHours % 12;
    displayHours = displayHours ? displayHours : 12; 
    const formattedHours = String(displayHours).padStart(2, '0');

    digitalTimeEl.textContent = `${formattedHours}:${displayMinutes}:${displaySeconds} ${ampm}`;

    const options = { weekday: 'long', month: 'long', day: 'numeric', year: 'numeric' };
    dateDisplayEl.textContent = now.toLocaleDateString('en-US', options);

    // Text-tile calendar (v0.6.5): month abbreviation in the colored strip,
    // day-of-month number in the body. Null-safe so a malformed DOM doesn't
    // crash the clock loop.
    if (calMonthEl) calMonthEl.textContent = now.toLocaleString('en-US', { month: 'short' }).toUpperCase();
    if (calDayEl) calDayEl.textContent = String(now.getDate());
  }

  updateClocks();
  setInterval(updateClocks, 1000);

  // APP WINDOW MANAGER
  // The surfaces (Chat, User Editor, Codex, Docks) are DOM overlays in
  // the ONE Tauri window. Background rules (by design):
  //   - WUPI Chat (chat): the ONLY window that pauses the canvas (stars +
  //     aurora OFF). Its own background is ~80% opaque so the paused backdrop
  //     doesn't show through. Closing it resumes the canvas.
  //   - Everything else (Codex, Profile, Docks home): canvas keeps running -
  //     stars/aurora animate behind the translucent glass.
  //
  // The previous version painted a frozen gradient into the framebuffer while
  // paused, which caused the compositor to tear/glitch and froze the loop on
  // close. The fix: NEVER manually paint the canvas here. Only flip the
  // `paused` flag; the RAF loop (animate) already handles start/stop cleanly
  // via its `if (!paused) requestAnimationFrame(animate)` guard, and when
  // un-paused it repaints fresh on the next frame. No half-painted frames.

  const openWindows = new Set();
  let zCounter = 1000;
  // No window pauses the canvas anymore: the background stays active behind
  // every surface (Chat is now translucent enough that stars show through).
  // Kept as a hook in case a future surface wants to freeze the background.
  function syncCanvasForWindows() {
    /* no-op: background always active */
  }

  function openWindow(id) {
    const el = document.getElementById(id);
    if (!el) return;
    if (openWindows.has(id)) {
      // Already open: just raise it to the top.
      el.style.zIndex = ++zCounter;
      return;
    }
    openWindows.add(id);
    el.style.zIndex = ++zCounter;
    el.classList.add('show');
    el.setAttribute('aria-hidden', 'false');
    syncCanvasForWindows();
    // Fire an onOpen hook if the surface registered one (e.g. Profile loads
    // its fields, Codex loads its list, Chat may show intro).
    const hook = windowOpenHooks.get(id);
    if (hook) hook();
  }

  function closeWindow(id) {
    const el = document.getElementById(id);
    if (!el) return;
    if (!openWindows.has(id)) return;
    openWindows.delete(id);
    el.classList.remove('show');
    el.setAttribute('aria-hidden', 'true');
    syncCanvasForWindows();
    // Fire an onClose hook if the surface registered one (e.g. Chat tears
    // down an active ink-reveal timer so it doesn't tick against a detached
    // node).
    const closeHook = windowCloseHooks.get(id);
    if (closeHook) closeHook();
  }

  // Surfaces register an async onOpen hook (load data when first shown).
  const windowOpenHooks = new Map();
  // Surfaces register an onClose hook (tear down timers, etc.).
  const windowCloseHooks = new Map();

  // ✕ close buttons (data-close="winId"). Selector is class-agnostic so it
  // catches both the standard .app-window-close and the terminal's custom
  // .terminal-close (the magenta glow X) — every [data-close] in the codebase
  // is a close button inside an .app-window.
  document.querySelectorAll('[data-close]').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      closeWindow(btn.dataset.close);
    });
  });

  // Esc closes the topmost open window.
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || openWindows.size === 0) return;
    // Close the highest-z open window (last added to the set isn't strictly
    // topmost, but in practice users Esc the one they just opened). Find by
    // max z-index for correctness.
    let topId = null;
    let topZ = -1;
    for (const id of openWindows) {
      const el = document.getElementById(id);
      const z = parseInt(el?.style.zIndex || '0', 10);
      if (z > topZ) { topZ = z; topId = id; }
    }
    if (topId) closeWindow(topId);
  });

  // Clicks inside a window must NOT bubble to the document-level handler that
  // closes the top-bar dropdowns (that handler also doesn't close windows, but
  // stopping propagation keeps the dropdown logic from running needlessly and
  // prevents a window-open dock click from immediately re-closing dropdowns).
  document.querySelectorAll('.app-window').forEach((win) => {
    win.addEventListener('click', (e) => e.stopPropagation());
  });

  // Header is the drag handle. The window is absolutely positioned; dragging
  // updates `left`/`top`. Only windows with `.draggable` get this: Chat is
  // fixed (immovable per spec), Docks-home is full-screen (no drag).
  function makeDraggable(winEl) {
    const handle = winEl.querySelector('.app-window-header');
    if (!handle) return;
    handle.style.cursor = 'grab';
    let dragging = false;
    let startX = 0, startY = 0, startLeft = 0, startTop = 0;

    handle.addEventListener('mousedown', (e) => {
      // Don't drag when clicking the close button or interactive header el.
      if (e.target.closest('.app-window-close')) return;
      dragging = true;
      handle.style.cursor = 'grabbing';
      // Switch from transform-center to absolute left/top so we can move it.
      const rect = winEl.getBoundingClientRect();
      winEl.style.left = rect.left + 'px';
      winEl.style.top = rect.top + 'px';
      winEl.style.transform = 'none';
      winEl.classList.add('dragged'); // CSS: drop the centering transform
      startX = e.clientX;
      startY = e.clientY;
      startLeft = rect.left;
      startTop = rect.top;
      e.preventDefault();
    });
    window.addEventListener('mousemove', (e) => {
      if (!dragging) return;
      const dx = e.clientX - startX;
      const dy = e.clientY - startY;
      // Keep the title bar on-screen (don't let it vanish off an edge).
      const maxX = window.innerWidth - 80;
      const maxY = window.innerHeight - 48;
      const nl = Math.min(Math.max(startLeft + dx, 0), maxX);
      const nt = Math.min(Math.max(startTop + dy, 0), maxY);
      winEl.style.left = nl + 'px';
      winEl.style.top = nt + 'px';
    });
    window.addEventListener('mouseup', () => {
      if (!dragging) return;
      dragging = false;
      handle.style.cursor = 'grab';
    });
  }
  document.querySelectorAll('.app-window.draggable').forEach(makeDraggable);

  // Click an open app's dock item again → closes it (toggle behavior). The
  // quick-access dock order is fixed: API → Chat → Profile → Codex (NOT
  // alphabetical: that's the Docks home grid). Apps (Docks launcher) is
  // special: it closes any open surface windows then shows the home grid.
  function dockToggle(id) {
    // Shell-wide rapid-click guard: a rapid double-tap on a dock button could
    // otherwise toggle open→close (or fire openWindow twice on the same tick).
    // The async wrapper gives withShellBusy a Promise to auto-clear on.
    withShellBusy(async () => {
      if (openWindows.has(id)) closeWindow(id);
      else openWindow(id);
    });
  }

  document.getElementById('dockApi')?.addEventListener('click', (e) => {
    e.stopPropagation();
    dockToggle('api');
  });
  document.getElementById('dockChat')?.addEventListener('click', (e) => {
    e.stopPropagation();
    dockToggle('chat');
  });
  document.getElementById('dockProfile')?.addEventListener('click', (e) => {
    e.stopPropagation();
    dockToggle('profile');
  });
  document.getElementById('dockCodex')?.addEventListener('click', (e) => {
    e.stopPropagation();
    dockToggle('codex');
  });
  document.getElementById('dockApps')?.addEventListener('click', (e) => {
    e.stopPropagation();
    // Docks = "home": close any open surface windows and show the launcher
    // grid. (apps itself is the full-screen home overlay.) Toggle-closes on
    // re-click so the blurry home backdrop dismisses — matches every other
    // dock button (dockChat/dockProfile/etc.), which all use dockToggle.
    // Previously this was a no-op `return` when apps was already open, which
    // stranded the backdrop (the bug).
    // (#64) Wrapped in the rapid-click guard like the other shell-chrome
    // entry points: openWindow/closeWindow are synchronous DOM flips, so
    // the wrapper holds shellBusy through the window show/hide transition
    // (250ms) — the double-tap window — instead of clearing on the next
    // microtask.
    withShellBusy(async () => {
      if (openWindows.has('apps')) {
        closeWindow('apps');
      } else {
        closeWindow('api');
        closeWindow('chat');
        closeWindow('profile');
        closeWindow('codex');
        openWindow('apps');
      }
      await new Promise((resolve) => setTimeout(resolve, 250));
    });
  });

  // Home-grid launcher icons (inside apps): open the matching surface.
  document.querySelectorAll('.home-app[data-open]').forEach((icon) => {
    icon.addEventListener('click', (e) => {
      e.stopPropagation();
      // Shell-wide rapid-click guard: drop a second click that lands while a
      // prior launch is still settling (the fog/sweep transition for Fable, or
      // the window-show for plain apps). withShellBusy clears on Promise
      // resolve/reject + has a 12s safety-timeout fallback so the flag can
      // never dead-lock. The task returns a Promise so the auto-clear fires
      // even though launchFable/openWindow kick off async work internally.
      withShellBusy(async () => {
      const target = icon.dataset.open;
      // FABLE is special-cased: it is a registered AppLifecycle app, not a
      // plain .app-window. Its "Cloud Bank Sweep" transition (fable.js
      // openFable → playFogTransition) plays OVER THE OS DESKTOP and swaps to
      // the Fable title at the sweep midpoint (when the solid fog core covers
      // the screen) — so the home grid MUST stay open + visible until that
      // swap (the fog is the thing that hides it). Routing through
      // openWindow('fable') would call closeWindow('apps') + add .show to
      // #fable immediately, defeating the sweep. launchFable() fires
      // onOpen=openFable, whose fog onSwap callback owns the swap (show #fable
      // + close the home grid), fired invisibly under the solid fog. The
      // openHook for 'fable' (registered by initFable) already redirects
      // openWindow('fable') to launchFable, but that path still runs the
      // closeWindow('apps') above first — so for Fable we short-circuit
      // straight to launchFable and let the fog own the home-grid dismissal.
      if (target === 'fable') {
        await launchFable();
        return;
      }
      closeWindow('apps'); // leave home, open the app
      openWindow(target);
      }); // withShellBusy
    });
  });

  // USER EDITOR (was "Profile Editor"; renamed to match the §8C
  // operator.xml → user.xml move. The function name + 'profile' surface key
  // + IPC names are unchanged — wire identifiers, not user-facing labels.)
  (function profileEditor() {
    const nameEl = document.getElementById('profName');
    const descEl = document.getElementById('profDescription');
    const saveBtn = document.getElementById('profSaveBtn');
    const statusEl = document.getElementById('profStatus');
    if (!nameEl) return;

    function setStatus(msg, kind) {
      statusEl.textContent = msg || '';
      statusEl.className = 'profile-status' + (kind ? ' ' + kind : '');
    }

    // Load fresh every time the window opens: cheap, and guarantees the editor
    // reflects disk state (someone could have hand-edited user.xml).
    windowOpenHooks.set('profile', () => {
      setStatus('Loading…');
      invoke('operator_profile_get')
        .then((profile) => {
          if (profile) {
            nameEl.value = profile.name || '';
            descEl.value = profile.description || '';
          } else {
            nameEl.value = ''; descEl.value = '';
          }
          setStatus('');
        })
        .catch((err) => console.warn('[wupi] load failed', err));
    });

    saveBtn?.addEventListener('click', () => {
      saveBtn.disabled = true;
      setStatus('Saving…');
      invoke('operator_profile_set', {
        name: nameEl.value,
        description: descEl.value,
      })
        .then(() => setStatus('Saved: applies next message', 'ok'))
        .catch((err) => setStatus('Save failed: ' + err, 'err'))
        .finally(() => { saveBtn.disabled = false; });
    });
  })();

  // AI: Connection Profile panel (LOCAL | ONLINE mode selector + profile CRUD)
  // Source of truth = api_config.json (loaded at boot into AppState). The
  // panel shows two large mode boxes: LOCAL (the single WUPI E4B bubble) or
  // ONLINE (saved endpoint profiles + an editor). Selecting ONLINE is pure
  // bookkeeping (v0.6.3 local-always: the local model stays resident as the
  // silent agent + fallback — nothing unloads);
  // selecting LOCAL reverts it. Temperature is fixed at 1.0 (no UI field).
  // The model field is a dropdown populated from the endpoint's /models
  // list after a successful connect: never free text.
  (function apiPanel() {
    const root = document.getElementById('api');
    if (!root) return;
    const panel = document.getElementById('aiPanel');
    const editorEl = document.getElementById('apiEditor');
    const nameEl = document.getElementById('apiName');
    const endpointEl = document.getElementById('apiEndpoint');
    const keyEl = document.getElementById('apiKey');
    const addBtn = document.getElementById('aiAddBtn');
    const editProfileBtn = document.getElementById('aiEditProfileBtn');
    const deleteProfileBtn = document.getElementById('aiDeleteProfileBtn');
    const statusEl = document.getElementById('apiStatus');
    const profileSelect = document.getElementById('aiProfileSelect');
    const modelSelect = document.getElementById('apiModel');
    const onlineBubble = document.getElementById('aiOnlineBubble');
    const connectBtn = document.getElementById('aiConnectBtn');

    let editingId = null; // null = creating; string = editing existing
    let lastConfig = null; // cached for rendering
    let runtimeSource = 'local'; // actual backend source (api = Fable-narrator connected)
    let activeProfileId = null; // currently-connected profile (mirror of backend)
    // Model cache: profileId → { ids: [..], selected: str }. Avoids refetching
    // /models when toggling between already-loaded profiles.
    const modelCache = new Map();

    function setStatus(msg, kind) {
      statusEl.textContent = msg || '';
      statusEl.className = 'profile-status' + (kind ? ' ' + kind : '');
    }

    function escapeHtml(s) {
      return String(s || '').replace(/[&<>"']/g, (c) => ({
        '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
      }[c]));
    }

    function findProfile(id) {
      return lastConfig?.profiles.find((p) => p.id === id) || null;
    }

    // Render the profile dropdown from the cached config. Sorted alphabetically
    // by name. Active profile is flagged with a ● prefix. The "Create a New
    // Profile" placeholder option is ONLY shown when there are zero saved
    // profiles: once any exist it disappears (the + button is the create
    // affordance then). Selecting the placeholder focuses the editor.
    function renderProfileSelect(config) {
      lastConfig = config;
      const profiles = [...(config.profiles || [])].sort((a, b) =>
        (a.name || a.id).localeCompare(b.name || b.id)
      );
      // Capture the selection BEFORE we rebuild the DOM. After innerHTML
      // rebuilds the options, the .value reverts to "": so we must remember
      // it now and re-apply it after.
      const prevValue = profileSelect.value;

      if (profiles.length === 0) {
        // No saved profiles yet: the dropdown IS the "create" affordance.
        profileSelect.innerHTML = '<option value="">Create a New Profile</option>';
        profileSelect.disabled = false;
        editProfileBtn.disabled = true;
        deleteProfileBtn.disabled = true;
        return;
      }
      profileSelect.disabled = false;
      // Once profiles exist, drop the "Create a New Profile" placeholder -
      // the + button below handles creation.
      profileSelect.innerHTML = profiles.map((p) => {
        const isActive = p.id === config.active_profile_id;
        return `<option value="${escapeHtml(p.id)}">${isActive ? '● ' : ''}${escapeHtml(p.name || p.id)}</option>`;
      }).join('');
      // Re-apply the previous selection if that profile still exists.
      // Otherwise default to the active profile (or the first one).
      const stillExists = (id) => id && [...profileSelect.options].some((o) => o.value === id);
      const target = stillExists(prevValue) ? prevValue
                   : stillExists(config.active_profile_id) ? config.active_profile_id
                   : profiles[0].id;
      profileSelect.value = target;
      // Edit/trash are enabled whenever a real profile is selected.
      // By design: even a single profile must be editable/deletable.
      const hasRealSelection = !!profileSelect.value;
      editProfileBtn.disabled = !hasRealSelection;
      deleteProfileBtn.disabled = !hasRealSelection;
    }

    // Update the online bubble. Three states:
    //   - connected (runtime on API): magenta glow + "Name: model"
    //   - selection pending (profile+model picked, not yet Connect'd): subdued
    //     preview of what Connect will activate, no glow
    //   - nothing picked: muted "No profile connected"
    function renderOnlineBubble() {
      // Connected: API profile active (reserved for Fable narration).
      if (runtimeSource === 'api' && activeProfileId) {
        const p = findProfile(activeProfileId);
        if (p) {
          onlineBubble.classList.add('active');
          onlineBubble.classList.remove('pending');
          onlineBubble.innerHTML =
            `<span class="ai-online-bubble-text">${escapeHtml(p.name || p.id)}</span>` +
            `<span class="ai-online-bubble-sep">-</span>` +
            `<span class="ai-online-bubble-model">${escapeHtml(p.model || '?')}</span>`;
          return;
        }
      }
      // Selection pending: profile + model both picked in the dropdowns but
      // not yet connected. Show a preview so the user sees what they're about
      // to activate. Uses the "pending" style (no glow, lighter text).
      const pickedProfileId = profileSelect?.value;
      const pickedModel = modelSelect?.value;
      if (pickedProfileId && pickedModel) {
        const p = findProfile(pickedProfileId);
        if (p) {
          onlineBubble.classList.remove('active');
          onlineBubble.classList.add('pending');
          onlineBubble.innerHTML =
            `<span class="ai-online-bubble-text">${escapeHtml(p.name || p.id)}</span>` +
            `<span class="ai-online-bubble-sep">-</span>` +
            `<span class="ai-online-bubble-model">${escapeHtml(pickedModel)}</span>`;
          return;
        }
      }
      // Nothing useful to show.
      onlineBubble.classList.remove('active', 'pending');
      onlineBubble.innerHTML = '<span class="ai-online-bubble-text">No API connected</span>';
    }

    // Fetch /models for a profile + populate the model dropdown. Cached per
    // profile so switching back doesn't refetch. Default-selects the saved
    // model if present in the list, else the first alphabetically. The list
    // is sorted alphabetically (case-insensitive): NanoGPT's /models returns
    // 100+ models in provider-defined order (a chaotic mix of org/name), so
    // alphabetical is the only sane default. There's no membership/free-vs-
    // paid field in the OpenAI-standard /models response, so we can't group
    // by tier without custom metadata: just alphabetize for now.
    async function populateModelDropdown(profile) {
      if (!profile) {
        modelSelect.innerHTML = '<option value="">Pick a profile to load models…</option>';
        modelSelect.disabled = true;
        return;
      }
      // Cache hit. But HONOR the user's current in-UI selection: if the
      // dropdown already has a value and it's still in the cached list,
      // keep it selected. Otherwise a refresh() after Connect would fling
      // the selection back to the cache's stale `selected` field.
      const cached = modelCache.get(profile.id);
      if (cached) {
        const currentPick = modelSelect.value;
        const honored = (currentPick && cached.ids.includes(currentPick))
          ? currentPick
          : cached.selected;
        renderModelOptions(cached.ids, honored);
        return;
      }
      modelSelect.disabled = true;
      modelSelect.innerHTML = '<option value="">Loading models…</option>';
      try {
        const v = await invoke('api_profile_test', { profile });
        const rawIds = (v && Array.isArray(v.data))
          ? v.data.map((m) => (typeof m === 'string' ? m : m?.id)).filter(Boolean)
          : [];
        if (rawIds.length === 0) {
          modelSelect.innerHTML = '<option value="">No models returned</option>';
          return;
        }
        // Sort alphabetically, case-insensitive, deterministic for equal keys.
        const ids = [...rawIds].sort((a, b) =>
          a.toLowerCase().localeCompare(b.toLowerCase()) || a.localeCompare(b)
        );
        // Default to the profile's saved model if it's in the list; else the
        // first alphabetically. The user's in-UI pick (if any) takes priority
        // on cache hit (handled above).
        const preferred = (profile.model && ids.includes(profile.model)) ? profile.model : ids[0];
        modelCache.set(profile.id, { ids, selected: preferred });
        renderModelOptions(ids, preferred);
      } catch (err) {
        modelSelect.innerHTML = '<option value="">Failed to load models</option>';
        setStatus('Model list fetch failed: ' + err, 'err');
      }
    }

    function renderModelOptions(ids, selected) {
      modelSelect.innerHTML = ids.map((id) =>
        `<option value="${escapeHtml(id)}"${id === selected ? ' selected' : ''}>${escapeHtml(id)}</option>`
      ).join('');
      modelSelect.disabled = false;
    }

    // Update the Connect button's enabled state. Requires a profile + model.
    function updateConnectEnabled() {
      const ready = !!profileSelect.value && !!modelSelect.value;
      connectBtn.disabled = !ready;
    }

    async function refresh() {
      try {
        const config = await invoke('api_profiles_list');
        const extra = await invoke('model_source_get');
        lastConfig = config;
        runtimeSource = (extra?.source || config.model_source) === 'api' ? 'api' : 'local';
        activeProfileId = config.active_profile_id || null;
        renderProfileSelect(config);
        renderOnlineBubble();
        // ALWAYS populate the model dropdown for the currently-selected
        // profile (if any). Programmatic .value = ... doesn't fire the
        // change event, so this is the only reliable way to keep the model
        // list in sync after a refresh.
        if (profileSelect.value) {
          await populateModelDropdown(findProfile(profileSelect.value));
        }
        updateConnectEnabled();
        setStatus('');
      } catch (err) {
        console.warn('[wupi] load failed', err);
      }
    }

    // When the dropdown has no real selection (zero-profile state: the
    // "Create a New Profile" placeholder is selected), focus the editor so
    // the user can start typing their first profile.
    profileSelect?.addEventListener('change', async () => {
      const selectedId = profileSelect.value;
      if (!selectedId) {
        // "Create a New Profile" (or no selection): prep the editor.
        clearEditor();
        nameEl?.focus();
        // Edit/trash aren't meaningful without a real profile.
        editProfileBtn.disabled = true;
        deleteProfileBtn.disabled = true;
        updateConnectEnabled();
        renderOnlineBubble();
        return;
      }
      const p = findProfile(selectedId);
      await populateModelDropdown(p);
      updateConnectEnabled();
      renderOnlineBubble();
      // Real profile selected: enable edit/trash.
      editProfileBtn.disabled = false;
      deleteProfileBtn.disabled = false;
    });

    // Also writes the new pick back into the cache so a subsequent refresh()
    // (which hits the cache) honors it instead of flinging back to the old
    // default: the cause of the "dropdown flings to first after Connect" bug.
    modelSelect?.addEventListener('change', () => {
      const pickedProfileId = profileSelect.value;
      const pickedModel = modelSelect.value;
      if (pickedProfileId && pickedModel) {
        const cached = modelCache.get(pickedProfileId);
        if (cached && cached.selected !== pickedModel) {
          modelCache.set(pickedProfileId, { ...cached, selected: pickedModel });
        }
      }
      updateConnectEnabled();
      renderOnlineBubble();
    });

    connectBtn?.addEventListener('click', async () => {
      const profileId = profileSelect.value;
      const modelId = modelSelect.value;
      if (!profileId || !modelId) return;
      // Persist the chosen model into the profile before connecting: the
      // backend's api_connect validates non-empty model.
      const p = findProfile(profileId);
      if (p && p.model !== modelId) {
        // (2026-08-15 audit fix) no temperature here: the Rust backend's
        // locked fallback constant (0.85) must govern every API turn.
        const updated = { ...p, model: modelId };
        try {
          await invoke('api_profile_save', { profile: updated });
        } catch (err) {
          setStatus('Could not save model choice: ' + err, 'err');
          return;
        }
      }
      setTitleState('offline'); // red while swapping
      setStatus('Connecting…', '');
      connectBtn.disabled = true;
      try {
        await invoke('api_connect', { profileId });
        setStatus('Connected: API ready for Fable narration.', 'ok');
      } catch (err) {
        setStatus('Connect failed: ' + err + '.', 'err');
        setTitleState('idle');
      }
      await refresh();
    });

    function clearEditor() {
      editingId = null;
      nameEl.value = '';
      endpointEl.value = '';
      keyEl.value = '';
      editorEl.classList.remove('editing');
      setStatus('');
    }

    function loadEditor(profile) {
      editingId = profile?.id || null;
      nameEl.value = profile?.name || '';
      endpointEl.value = profile?.endpoint || '';
      keyEl.value = profile?.api_key || '';
      editorEl.classList.add('editing');
      setStatus('Editing "' + (profile?.name || '') + '". + overwrites.');
      nameEl.focus();
    }

    // Errors via the status line if any field is empty. Does NOT auto-connect
    //: just lands the profile in the dropdown and auto-selects it.
    addBtn?.addEventListener('click', async () => {
      const name = nameEl.value.trim();
      if (!name) { setStatus('Name is required.', 'err'); nameEl.focus(); return; }
      if (!endpointEl.value.trim()) { setStatus('API URL is required.', 'err'); endpointEl.focus(); return; }
      if (!keyEl.value.trim()) { setStatus('API key is required.', 'err'); keyEl.focus(); return; }
      // Preserve the existing model if editing; new profiles start empty and
      // get their model from the dropdown after selection.
      const existing = editingId ? findProfile(editingId) : null;
      const profile = {
        id: editingId || '',
        name,
        endpoint: endpointEl.value.trim(),
        api_key: keyEl.value,
        model: existing?.model || '',
      };
      addBtn.disabled = true;
      setStatus(editingId ? 'Saving…' : 'Adding…');
      try {
        const saved = await invoke('api_profile_save', { profile });
        const savedId = saved?.id || editingId || name;
        clearEditor();
        await refresh();
        // Auto-select the just-saved profile + populate its models.
        profileSelect.value = savedId;
        if (profileSelect.value === savedId) {
          profileSelect.dispatchEvent(new Event('change'));
          setStatus('Saved. Pick a model, then Connect.', 'ok');
        } else {
          setStatus('Saved.', 'ok');
        }
      } catch (err) {
        setStatus('Save failed: ' + err, 'err');
      } finally {
        addBtn.disabled = false;
      }
    });

    editProfileBtn?.addEventListener('click', () => {
      const p = findProfile(profileSelect.value);
      if (!p) { setStatus('Pick a profile to edit first.', 'err'); return; }
      loadEditor(p);
    });

    // Two-click inline delete confirm. Native confirm() is dead in the Tauri
    // WebView (wry disables default script dialogs → always false, a no-op
    // button), so the first click ARMS (red state + status prompt) and the
    // second click within the window deletes. Any selection change or other
    // click disarms via the timeout.
    let deleteArmTimer = 0;
    const disarmDeleteBtn = () => {
      clearTimeout(deleteArmTimer);
      if (!deleteProfileBtn) return;
      deleteProfileBtn.dataset.armed = '';
      deleteProfileBtn.title = 'Delete selected profile';
    };
    deleteProfileBtn?.addEventListener('click', async () => {
      const id = profileSelect.value;
      const p = findProfile(id);
      if (!p) { disarmDeleteBtn(); setStatus('Pick a profile to delete first.', 'err'); return; }
      if (deleteProfileBtn.dataset.armed !== '1') {
        deleteProfileBtn.dataset.armed = '1';
        deleteProfileBtn.title = `Really delete "${p.name || p.id}"? Click again.`;
        setStatus(`Click delete again to remove "${p.name || p.id}" (URL + key).`, 'err');
        clearTimeout(deleteArmTimer);
        deleteArmTimer = setTimeout(disarmDeleteBtn, 5000);
        return;
      }
      disarmDeleteBtn();
      setStatus('Deleting…');
      try {
        await invoke('api_profile_delete', { profileId: id });
        // If we were editing this profile, clear the editor.
        if (editingId === id) clearEditor();
        setStatus('Deleted.', 'ok');
        await refresh();
      } catch (err) {
        setStatus('Delete failed: ' + err, 'err');
      }
    });

    // Load fresh every time the window opens.
    windowOpenHooks.set('api', () => { refresh(); });
  })();

  // WUPI CHAT: full streaming chat surface
  (function wupiChat() {
    const msgsEl = document.getElementById('chatMessages');
    const inputEl = document.getElementById('chatInput');
    if (!msgsEl) return;

    // Tauri v2 Channel for streaming: imported statically at the top of the
    // module, so it's always available (no race with a dynamic import).
    let generating = false;
    let emptyShown = true;

    function showEmpty() {
      if (!emptyShown) return;
      msgsEl.innerHTML = `<div class="chat-empty">Say hello to Wupi.</div>`;
    }
    function clearEmpty() {
      if (!emptyShown) return;
      emptyShown = false;
      msgsEl.innerHTML = '';
    }

    function scrollBottom() {
      msgsEl.scrollTop = msgsEl.scrollHeight;
    }

    function addUserBubble(text) {
      clearEmpty();
      const div = document.createElement('div');
      div.className = 'msg user';
      div.textContent = text;
      msgsEl.appendChild(div);
      scrollBottom();
    }

    function addErrorBubble(msg) {
      const div = document.createElement('div');
      div.className = 'msg-error';
      div.textContent = msg;
      msgsEl.appendChild(div);
      scrollBottom();
    }

    // A static (non-streaming) Wupi message: used for the randomized intro
    // shown when Chat first opens. Mirrors the finalized bubble shape.
    function addWupiBubble(text) {
      clearEmpty();
      const div = document.createElement('div');
      div.className = 'msg wupi';
      div.textContent = text;
      msgsEl.appendChild(div);
      scrollBottom();
    }

    // Returns the wupi bubble element + a text setter.
    function startWupiBubble() {
      clearEmpty();
      const div = document.createElement('div');
      div.className = 'msg wupi streaming';
      msgsEl.appendChild(div);
      scrollBottom();
      return div;
    }

    function finalizeWupiBubble(div, finalText, reasoning) {
      // `reasoning` is unused post-2026-08-07 override (the player-facing
      // reasoning UI was removed; the local model still thinks internally).
      void reasoning;
      div.classList.remove('streaming');
      div.textContent = finalText || '(no response)';
      scrollBottom();
    }

    function setGenerating(on) {
      generating = on;
      // The input stays ENABLED (2026-07-27, no send/stop buttons): pressing
      // Enter on an EMPTY field while generating stops the turn. The placeholder
      // flips to hint that affordance; otherwise the idle hint stays.
      inputEl.placeholder = on
        ? 'Press Enter to stop…  (Shift+Enter for newline)'
        : 'Message Wupi…  (Enter to send, Shift+Enter for newline)';
      // Bridge to the title status indicator: the main model is "typing"
      // during a chat_send. This flag is the authoritative source: it only
      // flips on user-driven chat sends, so Agent.gguf (schema engine, own
      // thread, never drives chat_send) is excluded by construction.
      setTitleState(on ? 'typing' : 'idle');
    }

    async function send() {
      if (generating) return;
      const text = inputEl.value.trim();
      if (!text) return;

      inputEl.value = '';
      addUserBubble(text);

      const bubble = startWupiBubble();
      let streamed = '';
      setGenerating(true);

      // No client-side pacing: chunks write straight to the bubble at the
      // backend's natural ~30 tok/s speed. The backend's StreamFilter
      // (stream_filter.rs) already strips protocol markers (<|channel|>,
      // <audio|>, etc.) before they reach the DOM, so the visible text is
      // clean prose live. The .streaming class on the bubble drives the
      // blinking caret CSS until `done` finalizes it.
      const channel = new Channel();
      let toolChip = null;  // tool-call status chip, created on first tool_call
      channel.onmessage = (e) => {
        if (!e) return;
        if (e.type === 'chunk') {
          streamed += e.text || '';
          bubble.textContent = streamed;
          scrollBottom();
        } else if (e.type === 'tool_call') {
          // Tool-calling agent loop (Phase 5): show a small chip indicating
          // Wupi is executing a tool. The chip morphs on tool_result.
          // Created lazily so non-tool turns (the common case) see no chip.
          if (!toolChip) {
            toolChip = document.createElement('div');
            toolChip.className = 'msg tool-chip';
            msgsEl.insertBefore(toolChip, bubble);
          }
          toolChip.className = 'msg tool-chip running';
          toolChip.textContent = `🔧 ${e.name || 'tool'}…`;
          scrollBottom();
        } else if (e.type === 'tool_result') {
          if (toolChip) {
            toolChip.className = 'msg tool-chip ' + (e.ok ? 'ok' : 'fail');
            const out = String(e.output || '').slice(0, 120);
            toolChip.textContent = e.ok
              ? `✓ ${e.name || 'tool'}${out ? ': ' + out : ''}`
              : `✗ ${e.name || 'tool'}: ${e.output || 'failed'}`;
            scrollBottom();
          }
        } else if (e.type === 'done') {
          setGenerating(false);
          const finalText = e.final_text != null ? e.final_text : streamed;
          finalizeWupiBubble(bubble, finalText);
        }
      };

      invoke('chat_send', { text, onEvent: channel })
        .catch((err) => {
          if (generating) {
            setGenerating(false);
            bubble.remove();
            addErrorBubble('Failed to send: ' + err);
          }
        })
        .finally(() => {
          // (#39) Backstop for a backend early-return: the invoke RESOLVED
          // without ever emitting `done` — without this the composer locked
          // on "Press Enter to stop" forever (generating never cleared, so
          // send() dead-ended). On the normal path `done` has already
          // finalized; the catch path above cleared the flag too.
          if (!generating) return;
          setGenerating(false);
          if (streamed) finalizeWupiBubble(bubble, streamed);
          else bubble.remove();
          addErrorBubble('Turn ended unexpectedly.');
        });
    }

    // Enter does double duty (2026-07-27, no send/stop buttons): on a
    // non-empty field it sends; on an EMPTY field while a turn streams it
    // stops the generation. Shift+Enter is a literal newline. The input
    // stays focusable + enabled during generation so the stop gesture works.
    inputEl?.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        if (generating && !inputEl.value.trim()) {
          invoke('chat_stop').catch((err) => console.warn('[Wupi] chat_stop failed', err));
          return;
        }
        send();
      }
    });

    // On each open: reset to a fresh conversation view + show Wupi's randomized
    // intro (one per open, from the SIM card's introductions list via the
    // get_intro IPC). The intro is UI-only: never sent to the model or archived.
    function loadIntro() {
      emptyShown = true;
      msgsEl.innerHTML = '';
      invoke('get_intro')
        .then((intro) => {
          if (intro) {
            addWupiBubble(intro);
          } else {
            showEmpty();
          }
        })
        .catch((e) => {
          console.warn('[Wupi] get_intro failed', e);
          showEmpty();
        });
    }
    windowOpenHooks.set('chat', loadIntro);
    // (#65) Tear down an in-flight turn on close: closing the Chat window
    // mid-generation used to leave the turn streaming into a detached
    // bubble, and the next open wiped the view while the decode ran on
    // invisibly (self-healing only when `done` finally arrived). Mirror the
    // Fable drawer close paths — stop the backend turn, let the .finally
    // backstop finalize the (detached) UI state.
    windowCloseHooks.set('chat', () => {
      if (generating) {
        invoke('chat_stop').catch((err) => console.warn('[Wupi] chat_stop (close) failed', err));
      }
    });
    loadIntro();
  })();

// === FABLE APP WIRING (Phase 2, 2026-07-26) ============================
// initFable builds the #fable app window, registers it with AppLifecycle
// (onOpen/onClose/onPause/onResume), and bridges the OS window system to
// the Fable launch. The hooks passed in:
//   pauseAurora / resumeAurora — flip the canvas RAF `paused` flag so the
//     OS aurora stops painting while the full-screen Fable stage is up
//     (Fable has its own background; the OS canvas would waste cycles +
//     compete for the GPU otherwise). Mirrors the visibilitychange/blur
//     pause pattern at line 339.
//   openHooks — the Map openWindow() consults; Fable registers its id →
//     launchFable() → AppLifecycle.launchApp('fable'). The 2s paused
//     welcome (music + ripple + button reveal) lives inside boot.js now,
//     not in a pre-launch fog gate.
//   closeHooks — the Map closeWindow() consults; Fable routes to
//     AppLifecycle.closeApp (full teardown).
//   closeWindow — ref to the OS closeWindow so Fable's own close paths
//     (the title-screen EXIT button) keep the openWindows set in sync.
//
// This is the wiring the Fable UI shell has been waiting on: the source
// under src/fable/ was complete but initFable() was never called, so the
// Games home tile was an inert "coming soon" stub. With this call the
// full-screen immersion stage + Simulation Narrative Engine become
// reachable from the OS desktop for the first time.
initFable({
  pauseAurora: () => { paused = true; },
  resumeAurora: () => { startLoop(); },
  openHooks: windowOpenHooks,
  closeHooks: windowCloseHooks,
  closeWindow,
});

// DEV SHORTCUT (?dev=fable / ?dev=preview): now that Fable is registered +
// its openHook is wired, launch it immediately. openFable() (fired by
// launchFable → AppLifecycle.launchApp) shows #fable, activates chrome, and
// would normally play the fog gate + boot transition — but its DEV shortcut
// branch (see openFable in fable.js) skips those cinematics too. From there
// openFable branches on which shortcut is active: ?dev=fable shows the title
// screen; ?dev=preview drops straight into a pure-frontend stage preview.
// Production builds never hit this (the param is absent under Tauri's custom
// protocol).
if (FABLE_ENTRY || DEV_PREVIEW_SHORTCUT) {
  // FABLE_ENTRY is true for fable.exe (loaded `wupi.html#fable`) OR the legacy
  // dev hash; DEV_PREVIEW_SHORTCUT is the pure-frontend layout preview. Both
  // auto-launch Fable now that it's registered. launchFable() no longer API-
  // gates entry (the title screen's own buttons gray out without an API), so
  // this lands on the title screen directly.
  launchFable().catch((e) => console.error('[Wupi] auto-launch failed', e));
}

// === PRISM APP WIRING (2026-07-31) =====================================
// initPrism builds the #prism app window, registers it with AppLifecycle
// (onOpen/onClose/onPause/onResume), and bridges the OS window system to
// the Prism launch — the same shape as initFable above. The hooks mirror
// Fable's: pauseAurora/resumeAurora freeze the OS canvas RAF while the
// full-screen Prism stage is up; openHooks/closeHooks bridge the OS
// window-set bookkeeping; closeWindow lets Prism's own close paths keep
// the openWindows set in sync. Prism reuses the shared SD swap core for
// generation (run_sd_swap_core), so no new VRAM plumbing — just the app
// shell + gallery + Tag Composer + Fork & Edit.
initPrism({
  pauseAurora: () => { paused = true; },
  resumeAurora: () => { startLoop(); },
  openHooks: windowOpenHooks,
  closeHooks: windowCloseHooks,
  closeWindow,
});