// =============================================================
// PRISM FORK & EDIT — seed-locked A/B iteration.
//
// A/B split-screen: left = the source image (seed-locked), right = a new
// generation with the SAME seed but editable prompt/CFG/sampler. A vertical
// drag-to-compare slider scrubs between the two images so the user sees
// exactly how changing tags altered the base.
//
// The seed contract: two runs with identical seed + identical params produce
// identical pixels (the diffusion-rs `.seed(i64)` contract, verified in the
// §1a investigation). Forking locks the source's seed + re-renders with the
// edited prompt; only the changed tags differ. The "Edit" action hands the
// seed + edited params to the composer.
//
// Workflow:
//   1. fork.load(image) — seeds the source (A) + the editable form (B) with
//      the image's params. B's prompt/sampler/CFG are editable; seed is LOCKED.
//   2. "Regenerate B" — calls prism_generate with the locked seed + B's
//      edited params; the result lands as B's image.
//   3. "Keep B" — B's result is already in the gallery (prism_generate
//      inserted it); this just routes back to the gallery to show it.
//   4. The drag slider is pure CSS clip — no generation, just reveal.
//
// Build/wire/teardown triplet (FABLE convention).
// =============================================================

import { convertFileSrc } from '@tauri-apps/api/core';
import { SAMPLERS, generate } from '../engine/api.js';

// The source image (A) + the editable B params.
let source = null;            // GalleryImage
let bParams = null;           // the editable generation params (seed locked)
let bResultPath = null;       // the last B result path (for the compare img)

// The drag-slider position (0-100, % from left).
let splitPct = 50;

let hooks = {};
let unlistenGenDone = null;

export function buildEl(routerHooks = {}) {
  hooks = routerHooks;
  source = null;
  bParams = null;
  bResultPath = null;
  splitPct = 50;

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
          <label class="prism-field-label" for="fork-negative">Negative (B)</label>
          <textarea id="fork-negative" class="prism-textarea" data-b="negative_prompt" rows="2"></textarea>
        </div>
        <div class="prism-field">
          <label class="prism-field-label" for="fork-cfg">CFG <span class="prism-field-value" data-readout="cfg">5.0</span></label>
          <input id="fork-cfg" type="range" min="1" max="20" step="0.5" value="5" class="prism-range" data-b="cfg" />
        </div>
        <div class="prism-field">
          <label class="prism-field-label" for="fork-sampler">Sampler (B)</label>
          <select id="fork-sampler" class="prism-select" data-b="sampler">${samplerOptions()}</select>
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

function samplerOptions() {
  return SAMPLERS.map((s) => `<option value="${s.value}">${s.label}</option>`).join('');
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

export function teardown() {
  if (unlistenGenDone) { try { unlistenGenDone(); } catch (_) {} unlistenGenDone = null; }
}

// ── Load a source image ────────────────────────────────────────────────

export function load(rootEl, img) {
  if (!img) return;
  source = img;
  // Seed B with the source's params + LOCK the seed to the source's seed.
  // If the source seed was -1 (random), we can't truly lock — fall back to
  // a fresh random for B (Fork from a random-seed image isn't reproducible;
  // the metadata panel surfaces this).
  const lockedSeed = img.seed >= 0 ? img.seed : -1;
  bParams = {
    prompt: img.prompt || '',
    negative_prompt: img.negative_prompt || '',
    seed: lockedSeed,
    cfg: img.cfg,
    steps: img.steps,
    width: img.width,
    height: img.height,
    sampler: img.sampler,
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
  const neg = rootEl.querySelector('[data-b="negative_prompt"]');
  if (neg) neg.value = bParams.negative_prompt;
  const cfg = rootEl.querySelector('[data-b="cfg"]');
  if (cfg) cfg.value = bParams.cfg;
  const sampler = rootEl.querySelector('[data-b="sampler"]');
  if (sampler) sampler.value = String(bParams.sampler);
  const cfgRo = rootEl.querySelector('[data-readout="cfg"]');
  if (cfgRo) cfgRo.textContent = bParams.cfg.toFixed(1);
  const seedDisp = rootEl.querySelector('[data-seed-display]');
  if (seedDisp) seedDisp.textContent = bParams.seed >= 0 ? bParams.seed : 'random (source had no locked seed)';
}

function onBFieldChange(rootEl, ctrl) {
  const key = ctrl.dataset.b;
  let val = ctrl.value;
  if (key === 'cfg') val = Math.round(Number(val) * 10) / 10;
  if (key === 'sampler') val = Number(val);
  bParams[key] = val;
  if (key === 'cfg') {
    const ro = rootEl.querySelector('[data-readout="cfg"]');
    if (ro) ro.textContent = bParams.cfg.toFixed(1);
  }
}

// ── Regenerate B ────────────────────────────────────────────────────────

async function onRegenerate(rootEl) {
  if (!bParams) return;
  const btn = rootEl.querySelector('[data-act="regen"]');
  if (btn) { btn.disabled = true; btn.textContent = 'Generating…'; }
  try {
    const res = await generate(bParams);
    // The result arrives via prism-gen-done; prism.js calls onGenDone with
    // the path, which setLayerImage('b', path) swaps in. Until then, mark B
    // as "rendering".
    const bLayer = rootEl.querySelector('[data-layer="b"]');
    if (bLayer) bLayer.classList.add('is-rendering');
  } catch (err) {
    if (hooks.onToast) hooks.onToast(String(err));
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = 'Regenerate B'; }
  }
}

// Called by prism.js when a generation completes while Fork is the active
// screen: swap B's image + clear the rendering flag.
export function onGenDone(rootEl, payload) {
  const bLayer = rootEl.querySelector('[data-layer="b"]');
  if (bLayer) bLayer.classList.remove('is-rendering');
  if (payload && payload.image && payload.image.path) {
    bResultPath = payload.image.path;
    setLayerImage(rootEl, 'b', payload.image.path);
  }
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
  let dragging = false;
  const move = (clientX) => {
    const rect = canvas.getBoundingClientRect();
    const pct = ((clientX - rect.left) / rect.width) * 100;
    splitPct = Math.max(0, Math.min(100, pct));
    updateClip(rootEl);
  };
  handle.addEventListener('mousedown', (e) => { dragging = true; e.preventDefault(); });
  canvas.addEventListener('mousedown', (e) => {
    // Clicking anywhere on the canvas jumps the handle there + starts a drag.
    dragging = true;
    move(e.clientX);
  });
  window.addEventListener('mousemove', (e) => { if (dragging) move(e.clientX); });
  window.addEventListener('mouseup', () => { dragging = false; });
  // Touch support.
  handle.addEventListener('touchstart', (e) => { dragging = true; }, { passive: true });
  window.addEventListener('touchmove', (e) => {
    if (dragging && e.touches[0]) move(e.touches[0].clientX);
  }, { passive: true });
  window.addEventListener('touchend', () => { dragging = false; });
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
