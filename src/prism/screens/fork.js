// =============================================================
// PRISM FORK & EDIT — seed-locked A/B iteration.
//
// A/B split-screen: left = the source image (seed-locked), right = a new
// generation with the SAME seed but an editable prompt. A vertical
// drag-to-compare slider scrubs between the two images so the user sees
// exactly how changing tags altered the base.
//
// The seed contract: two runs with identical seed + identical params produce
// identical pixels (the diffusion-rs `.seed(i64)` contract, verified in the
// §1a investigation). Forking locks the source's seed + re-renders with the
// edited prompt; only the changed tags differ. The "Edit" action hands the
// seed + edited prompt to the composer.
//
// LOCKED RECIPE (Chloe ruling, 2026-08-17): B's ONLY editable knob is the
// prompt — the sampler/steps/CFG (and the old negative lane) are locked
// server-side constants with NO control surface here or anywhere. The seed
// is display-locked to A's.
//
// Workflow:
//   1. fork.load(image) — seeds the source (A) + the editable form (B) with
//      the image's prompt. B's prompt is editable; seed is LOCKED.
//   2. "Regenerate B" — calls prism_generate with the locked seed + B's
//      edited prompt; the result lands as B's image.
//   3. "Keep B" — B's result is already in the gallery (prism_generate
//      inserted it); this just routes back to the gallery to show it.
//   4. The drag slider is pure CSS clip — no generation, just reveal.
//
// Build/wire/teardown triplet (FABLE convention).
// =============================================================

import { convertFileSrc } from '@tauri-apps/api/core';
import { generate } from '../engine/api.js';

// The source image (A) + the editable B params.
let source = null;            // GalleryImage
let bParams = null;           // the editable generation params (seed locked)
let bResultPath = null;       // the last B result path (for the compare img)

// The drag-slider position (0-100, % from left).
let splitPct = 50;

let hooks = {};
let unlistenGenDone = null;

// The compare-slider's WINDOW-level follow listeners (kept as refs so
// teardown can remove them + rewire re-attaches the SAME functions after a
// reopen), + the shared drag flag they close over.
let sliderWin = null;          // { mousemove, mouseup, touchmove, touchend }
let sliderDragging = false;

export function buildEl(routerHooks = {}) {
  hooks = routerHooks;
  source = null;
  bParams = null;
  bResultPath = null;
  splitPct = 50;
  // A rebuilt screen starts with a clean regen state (a prior open's
  // armed failsafe must not fire into the new DOM).
  regenInFlight = false;
  clearTimeout(regenFailsafeTimer);
  regenFailsafeTimer = null;

  const el = document.createElement('div');
  el.className = 'prism-screen prism-fork';
  el.innerHTML = `
    <div class="prism-fork-empty" data-fork-empty>
      <p>Select an image and choose <strong>Fork &amp; Edit</strong> to start a seed-locked comparison.</p>
    </div>
    <div class="prism-fork-workspace" data-fork-workspace hidden>
      <div class="prism-fork-canvas" data-fork-canvas>
        <div class="prism-fork-layer prism-fork-a" data-layer="a">
          <span class="prism-fork-tag">A · source</span>
        </div>
        <div class="prism-fork-layer prism-fork-b" data-layer="b">
          <span class="prism-fork-tag">B · edit</span>
        </div>
        <div class="prism-fork-handle" data-fork-handle>
          <div class="prism-fork-line"></div>
          <div class="prism-fork-grip">⇆</div>
        </div>
      </div>
      <aside class="prism-fork-editor">
        <h2 class="prism-section-title">Fork</h2>
        <div class="prism-field">
          <label class="prism-field-label" for="fork-prompt">Prompt (B)</label>
          <textarea id="fork-prompt" class="prism-textarea" data-b="prompt" rows="4"></textarea>
        </div>
        <div class="prism-field">
          <span class="prism-field-label">Seed <span class="prism-seed-locked">🔒 locked to A</span></span>
          <div class="prism-seed-locked-value mono" data-seed-display>—</div>
        </div>
        <div class="prism-fork-actions">
          <button class="prism-btn prism-btn-primary" data-act="regen">Regenerate B</button>
          <button class="prism-btn prism-btn-ghost" data-act="keep">Keep B →</button>
        </div>
      </aside>
    </div>
  `;
  return el;
}

export function wire(rootEl) {
  // B-field edits → bParams.
  rootEl.querySelectorAll('[data-b]').forEach((ctrl) => {
    ctrl.addEventListener('input', () => onBFieldChange(rootEl, ctrl));
    ctrl.addEventListener('change', () => onBFieldChange(rootEl, ctrl));
  });
  // Action buttons.
  const regen = rootEl.querySelector('[data-act="regen"]');
  if (regen) regen.addEventListener('click', () => onRegenerate(rootEl));
  const keep = rootEl.querySelector('[data-act="keep"]');
  if (keep) keep.addEventListener('click', () => { if (hooks.onKeep) hooks.onKeep(); });
  // Drag handle (the compare slider).
  wireSlider(rootEl);
}

export function teardown(rootEl) {
  if (unlistenGenDone) { try { unlistenGenDone(); } catch (_) {} unlistenGenDone = null; }
  // Drop the window-level drag listeners (the mousedown/touchstart starts
  // live on the canvas/handle elements — element-level, fine to keep).
  detachSliderWindow();
  sliderDragging = false;
  // Clear a stale B-rendering shimmer + re-arm the regen button + kill the
  // failsafe: the render's done event is dropped at close, so nothing else
  // would ever clear them.
  resetRegen(rootEl);
}

// Re-attach the window-level listeners teardown removed (openPrism on
// reopen). Idempotent — re-adding the same function refs never
// double-binds.
export function rewire() {
  attachSliderWindow();
}

// ── Load a source image ────────────────────────────────────────────────

export function load(rootEl, img) {
  if (!img) return;
  source = img;
  // Seed B with the source's prompt + LOCK the seed to the source's seed.
  // If the source seed was -1 (random), we can't truly lock — fall back to
  // a fresh random for B (Fork from a random-seed image isn't reproducible;
  // the metadata panel surfaces this). Dims ride the source verbatim (the
  // A/B compare must render at A's size, even for a legacy pre-bucket row);
  // the steering toggles inherit the source's bits (B's only EDITABLE knob
  // stays the prompt — toggle changes belong to the Composer); everything
  // else about the recipe is locked server-side.
  const lockedSeed = img.seed >= 0 ? img.seed : -1;
  bParams = {
    prompt: img.prompt || '',
    seed: lockedSeed,
    width: img.width,
    height: img.height,
    nsfw: !!img.nsfw,
    furry: !!img.furry,
  };
  bResultPath = img.path;  // initially, B mirrors A (no edit yet)

  // Reveal the workspace.
  const empty = rootEl.querySelector('[data-fork-empty]');
  const ws = rootEl.querySelector('[data-fork-workspace]');
  if (empty) empty.hidden = true;
  if (ws) ws.hidden = false;

  // Set the layer images + the editor fields.
  setLayerImage(rootEl, 'a', img.path);
  setLayerImage(rootEl, 'b', img.path);
  reflectBFields(rootEl);
  updateClip(rootEl);
}

function setLayerImage(rootEl, which, path) {
  const layer = rootEl.querySelector(`[data-layer="${which}"]`);
  if (!layer) return;
  // Set the background-image so the CSS clip-path slider reveals it.
  layer.style.backgroundImage = `url("${convertFileSrc(path)}")`;
}

function reflectBFields(rootEl) {
  const prompt = rootEl.querySelector('[data-b="prompt"]');
  if (prompt) prompt.value = bParams.prompt;
  const seedDisp = rootEl.querySelector('[data-seed-display]');
  if (seedDisp) seedDisp.textContent = bParams.seed >= 0 ? bParams.seed : 'random (source had no locked seed)';
}

function onBFieldChange(rootEl, ctrl) {
  bParams[ctrl.dataset.b] = ctrl.value;
}

// ── Regenerate B ────────────────────────────────────────────────────────

// The regen-button in-flight flag + failsafe timer (mirrors the Composer's
// generate-button discipline): prism_generate returns IMMEDIATELY (the
// multi-second swap cycle runs detached), so the button must stay disabled
// until the ORIGIN-ROUTED prism-gen-done flips it back — re-enabling on
// invoke-resolve (the old `finally` reset) let every extra click queue
// ANOTHER full unload→render→reload cycle behind the turn lock and reopen
// the dest-path duplicate-row race (same-ms clicks, one PNG, two rows).
// The ≤5min failsafe nets an emit-less backend failure path.
let regenInFlight = false;
let regenFailsafeTimer = null;

function resetRegen(rootEl) {
  regenInFlight = false;
  clearTimeout(regenFailsafeTimer);
  regenFailsafeTimer = null;
  if (rootEl) {
    const bLayer = rootEl.querySelector('[data-layer="b"]');
    if (bLayer) bLayer.classList.remove('is-rendering');
    const btn = rootEl.querySelector('[data-act="regen"]');
    if (btn) { btn.disabled = false; btn.textContent = 'Regenerate B'; }
  }
}

async function onRegenerate(rootEl) {
  if (!bParams) return;
  if (regenInFlight) return; // one swap cycle at a time — never queue a second
  regenInFlight = true;
  const btn = rootEl.querySelector('[data-act="regen"]');
  if (btn) { btn.disabled = true; btn.textContent = 'Generating…'; }
  // Tag the render BEFORE the invoke fires (prism.js pairs the eventual
  // prism-gen-done with THIS render by origin token, never with whichever
  // screen is active when it lands — and a fast stub done can't beat the
  // tag).
  if (hooks.onRenderStart) hooks.onRenderStart('fork');
  try {
    const res = await generate(bParams);
    if (hooks.onRenderPath) hooks.onRenderPath('fork', res && res.path);
    // The result arrives via prism-gen-done; the routed onGenDone below
    // resets the button. Until then, mark B as "rendering" + arm the
    // failsafe as the net for an emit-less failure path.
    const bLayer = rootEl.querySelector('[data-layer="b"]');
    if (bLayer) bLayer.classList.add('is-rendering');
    clearTimeout(regenFailsafeTimer);
    regenFailsafeTimer = setTimeout(() => {
      resetRegen(rootEl);
      if (hooks.onToast) hooks.onToast('Generation timed out (no completion signal).');
    }, 5 * 60 * 1000);
  } catch (err) {
    // The invoke itself rejected — the render never started server-side;
    // drop its origin tag so the router can't accumulate ghosts.
    if (hooks.onRenderFail) hooks.onRenderFail('fork');
    resetRegen(rootEl);
    if (hooks.onToast) hooks.onToast(String(err));
  }
}

// Called by prism.js when a FORK-ORIGINATED generation completes (the
// origin-token router — regardless of which screen is active when the
// swap finishes): swap B's image, clear the rendering flag, re-arm the
// button.
export function onGenDone(rootEl, payload) {
  resetRegen(rootEl);
  if (payload && payload.image && payload.image.path) {
    bResultPath = payload.image.path;
    setLayerImage(rootEl, 'b', payload.image.path);
  }
}

// Called by prism.js on a FAILED fork-originated render (or a pathless
// failure payload the router resets defensively): re-arm the button. A
// queued server-side render, if one exists, still completes + inserts its
// gallery row on its own done event.
export function onGenFail(rootEl) {
  resetRegen(rootEl);
}

// ── Drag-to-compare slider ──────────────────────────────────────────────
//
// The canvas holds A (full width) under B (clipped to the left of the handle
// via clip-path inset). Dragging the handle moves the clip boundary. Pure
// CSS reveal — no generation work.

function wireSlider(rootEl) {
  const canvas = rootEl.querySelector('[data-fork-canvas]');
  const handle = rootEl.querySelector('[data-fork-handle]');
  if (!canvas || !handle) return;
  const move = (clientX) => {
    const rect = canvas.getBoundingClientRect();
    const pct = ((clientX - rect.left) / rect.width) * 100;
    splitPct = Math.max(0, Math.min(100, pct));
    updateClip(rootEl);
  };
  handle.addEventListener('mousedown', (e) => { sliderDragging = true; e.preventDefault(); });
  canvas.addEventListener('mousedown', (e) => {
    // Clicking anywhere on the canvas jumps the handle there + starts a drag.
    sliderDragging = true;
    move(e.clientX);
  });
  handle.addEventListener('touchstart', () => { sliderDragging = true; }, { passive: true });
  // The window-level follow listeners, created ONCE (module refs — teardown
  // removes them at close, rewire re-attaches the same functions on reopen).
  sliderWin = {
    mousemove: (e) => { if (sliderDragging) move(e.clientX); },
    mouseup: () => { sliderDragging = false; },
    touchmove: (e) => {
      if (sliderDragging && e.touches[0]) move(e.touches[0].clientX);
    },
    touchend: () => { sliderDragging = false; },
  };
  attachSliderWindow();
}

function attachSliderWindow() {
  if (!sliderWin) return;
  window.addEventListener('mousemove', sliderWin.mousemove);
  window.addEventListener('mouseup', sliderWin.mouseup);
  window.addEventListener('touchmove', sliderWin.touchmove, { passive: true });
  window.addEventListener('touchend', sliderWin.touchend);
}

function detachSliderWindow() {
  if (!sliderWin) return;
  window.removeEventListener('mousemove', sliderWin.mousemove);
  window.removeEventListener('mouseup', sliderWin.mouseup);
  window.removeEventListener('touchmove', sliderWin.touchmove);
  window.removeEventListener('touchend', sliderWin.touchend);
}

function updateClip(rootEl) {
  const bLayer = rootEl.querySelector('[data-layer="b"]');
  const handle = rootEl.querySelector('[data-fork-handle]');
  if (bLayer) {
    // B is revealed on the LEFT of the handle; A shows through on the right.
    bLayer.style.clipPath = `inset(0 ${100 - splitPct}% 0 0)`;
  }
  if (handle) {
    handle.style.left = `${splitPct}%`;
  }
}
