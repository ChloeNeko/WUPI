// =============================================================
// PRISM COMPOSER — the Tag Composer (prompt engine) + Generate.
//
// The primary screen. An input that converts typed text (comma/Enter-
// delimited) into draggable "pill" chips, with Positive + Negative lanes.
// Pills compile to a clean comma-separated Danbooru string before the IPC
// call. The settings rail (dimensions, steps, CFG, sampler dropdown, seed)
// sits beside the prompt lanes. The Generate button kicks off
// `prism_generate`; the result arrives via the `prism-gen-done` event
// (handled in prism.js, which routes to the gallery/preview).
//
// Seed-locked iteration: a "lock" toggle captures the last result's seed
// + carries it into the next generation (the Fork & Edit primitive). When
// unlocked, seed=-1 (random) each time.
//
// Build/wire/teardown triplet (FABLE convention): buildEl() constructs the
// DOM + returns it; wire() attaches listeners + subscribes to events;
// teardown() removes listeners so a close mid-generation can't fire handlers
// against a detached node.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { SAMPLERS, DEFAULT_SAMPLER, GEN_DEFAULTS, generate } from '../engine/api.js';

// The live settings state. Mutated by the settings rail controls; read by
// the Generate button. Initialized to the SDXL clean-baseline defaults.
let settings = { ...GEN_DEFAULTS };

// The pill arrays (each pill is a string token). Empty = no prompt.
let positivePills = [];
let negativePills = [];

// The seed-lock state: when true, the NEXT generate uses the captured seed
// (from the last successful result) instead of -1. Captured via setLastSeed.
let lockedSeed = null;   // number | null (null = unlocked / random)

// Event listener handles (nulled in teardown).
let unlistenGenDone = null;

// Bound handlers (kept as refs so teardown can removeExacts).
let onGenerateBound = null;

// Callbacks injected by prism.js (the router): where to go after a generate
// kicks off, + how to surface toasts.
let hooks = {};

// ── Build ───────────────────────────────────────────────────────────────

export function buildEl(routerHooks = {}) {
  hooks = routerHooks;
  settings = { ...GEN_DEFAULTS };
  positivePills = [];
  negativePills = [];
  lockedSeed = null;

  const el = document.createElement('div');
  el.className = 'prism-screen prism-composer';
  el.innerHTML = `
    <div class="prism-composer-grid">
      <section class="prism-prompt-panel">
        <h2 class="prism-section-title">Compose</h2>
        ${laneHtml('positive', 'Prompt', '1girl, classroom, sunset, masterpiece')}
        ${laneHtml('negative', 'Negative', 'low quality, watermark, text')}
        <div class="prism-compose-actions">
          <button class="prism-btn prism-btn-ghost" data-action="clear-positive">Clear prompt</button>
          <button class="prism-btn prism-btn-primary prism-generate-btn" data-action="generate">
            <span class="prism-generate-label">Generate</span>
            <span class="prism-generate-spinner" hidden></span>
          </button>
        </div>
      </section>
      <aside class="prism-settings-rail">
        <h2 class="prism-section-title">Settings</h2>
        ${settingsRailHtml()}
      </aside>
    </div>
  `;
  return el;
}

function laneHtml(kind, label, placeholder) {
  return `
    <div class="prism-lane" data-lane="${kind}">
      <div class="prism-lane-label">${label}</div>
      <div class="prism-pill-box" data-pillbox="${kind}"></div>
      <div class="prism-lane-input-row">
        <input class="prism-lane-input" data-input="${kind}"
               placeholder="${placeholder}" type="text"
               autocomplete="off" spellcheck="false" />
      </div>
    </div>
  `;
}

function settingsRailHtml() {
  const samplerOptions = SAMPLERS.map(
    (s) => `<option value="${s.value}"${s.value === DEFAULT_SAMPLER ? ' selected' : ''}>${s.label}</option>`
  ).join('');
  return `
    <div class="prism-field">
      <label class="prism-field-label" for="prism-dim">Size</label>
      <select id="prism-dim" class="prism-select" data-setting="dim">
        <option value="1024x576">1024 × 576 (16:9)</option>
        <option value="832x1216">832 × 1216 (portrait)</option>
        <option value="1216x832">1216 × 832 (landscape)</option>
        <option value="1024x1024">1024 × 1024 (square)</option>
      </select>
    </div>
    <div class="prism-field">
      <label class="prism-field-label" for="prism-steps">Steps <span class="prism-field-value" data-readout="steps">${GEN_DEFAULTS.steps}</span></label>
      <input id="prism-steps" type="range" min="1" max="50" value="${GEN_DEFAULTS.steps}"
             class="prism-range" data-setting="steps" />
    </div>
    <div class="prism-field">
      <label class="prism-field-label" for="prism-cfg">CFG <span class="prism-field-value" data-readout="cfg">${GEN_DEFAULTS.cfg.toFixed(1)}</span></label>
      <input id="prism-cfg" type="range" min="1" max="20" step="0.5" value="${GEN_DEFAULTS.cfg}"
             class="prism-range" data-setting="cfg" />
    </div>
    <div class="prism-field">
      <label class="prism-field-label" for="prism-sampler">Sampler</label>
      <select id="prism-sampler" class="prism-select" data-setting="sampler">${samplerOptions}</select>
    </div>
    <div class="prism-field prism-seed-field">
      <label class="prism-field-label" for="prism-seed">Seed</label>
      <div class="prism-seed-row">
        <input id="prism-seed" type="number" class="prism-input" data-setting="seed"
               value="-1" min="-1" />
        <button class="prism-btn prism-btn-ghost prism-seed-lock" data-action="seed-lock"
                title="Lock seed for iteration">🔓</button>
      </div>
      <div class="prism-seed-hint">-1 = random. Lock to iterate on one seed.</div>
    </div>
  `;
}

// ── Wire ────────────────────────────────────────────────────────────────

export function wire(rootEl) {
  // Lane input: Enter or comma commits the typed text as one or more pills.
  rootEl.querySelectorAll('.prism-lane-input').forEach((input) => {
    input.addEventListener('keydown', (e) => onLaneKeydown(rootEl, input, e));
  });

  // Settings rail: range/select changes update `settings` + the readouts.
  rootEl.querySelectorAll('[data-setting]').forEach((ctrl) => {
    ctrl.addEventListener('input', () => onSettingChange(rootEl, ctrl));
    ctrl.addEventListener('change', () => onSettingChange(rootEl, ctrl));
  });

  // Generate + clear buttons.
  onGenerateBound = () => onGenerate(rootEl);
  const genBtn = rootEl.querySelector('[data-action="generate"]');
  if (genBtn) genBtn.addEventListener('click', onGenerateBound);
  const clearBtn = rootEl.querySelector('[data-action="clear-positive"]');
  if (clearBtn) clearBtn.addEventListener('click', () => setPills(rootEl, 'positive', []));
  const seedLockBtn = rootEl.querySelector('[data-action="seed-lock"]');
  if (seedLockBtn) seedLockBtn.addEventListener('click', () => onSeedLockToggle(rootEl));

  // The pill boxes get a delegated click listener (pill × removal) + a drag
  // reorder via the HTML drag-and-drop API.
  ['positive', 'negative'].forEach((kind) => {
    const box = rootEl.querySelector(`[data-pillbox="${kind}"]`);
    if (!box) return;
    box.addEventListener('click', (e) => onPillBoxClick(setPills, rootEl, kind, e));
    wireDragReorder(box, kind, rootEl);
  });
}

export function teardown() {
  if (unlistenGenDone) { try { unlistenGenDone(); } catch (_) {} unlistenGenDone = null; }
  onGenerateBound = null;
}

// ── Pill management ─────────────────────────────────────────────────────

function onLaneKeydown(rootEl, input, e) {
  const kind = input.dataset.input;
  const commit = e.key === 'Enter' || e.key === ',';
  if (!commit) return;
  e.preventDefault();
  const text = input.value.replace(/,/g, ' ').trim();
  if (!text) return;
  // Split on whitespace into multiple pills (a paste of "1girl, sunset" → 2).
  const tokens = text.split(/\s+/).filter(Boolean);
  const current = getPills(kind);
  setPills(rootEl, kind, [...current, ...tokens]);
  input.value = '';
}

function getPills(kind) {
  return kind === 'positive' ? positivePills.slice() : negativePills.slice();
}

function setPills(rootEl, kind, pills) {
  // Dedupe (case-insensitive) + stash.
  const seen = new Set();
  const deduped = [];
  for (const p of pills) {
    const key = p.toLowerCase();
    if (!seen.has(key)) { seen.add(key); deduped.push(p); }
  }
  if (kind === 'positive') positivePills = deduped; else negativePills = deduped;
  renderPills(rootEl, kind);
}

function renderPills(rootEl, kind) {
  const box = rootEl.querySelector(`[data-pillbox="${kind}"]`);
  if (!box) return;
  const pills = getPills(kind);
  box.innerHTML = pills.map((p, i) => `
    <span class="prism-pill" draggable="true" data-pill="${escapeAttr(p)}" data-idx="${i}">
      <span class="prism-pill-text">${escapeHtml(p)}</span>
      <button class="prism-pill-x" data-remove="${i}" aria-label="remove">×</button>
    </span>
  `).join('');
}

function onPillBoxClick(setPillsFn, rootEl, kind, e) {
  const x = e.target.closest('[data-remove]');
  if (!x) return;
  const idx = Number(x.dataset.remove);
  const pills = getPills(kind);
  pills.splice(idx, 1);
  setPillsFn(rootEl, kind, pills);
}

// Drag-to-reorder within a lane (HTML DnD). A pill dropped onto another
// pill moves it to that position.
function wireDragReorder(box, kind, rootEl) {
  let dragFromIdx = null;
  box.addEventListener('dragstart', (e) => {
    const pill = e.target.closest('[data-pill]');
    if (!pill) return;
    dragFromIdx = Number(pill.dataset.idx);
    e.dataTransfer.effectAllowed = 'move';
  });
  box.addEventListener('dragover', (e) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
  });
  box.addEventListener('drop', (e) => {
    e.preventDefault();
    const pill = e.target.closest('[data-pill]');
    if (!pill || dragFromIdx === null) return;
    const toIdx = Number(pill.dataset.idx);
    if (toIdx === dragFromIdx) return;
    const pills = getPills(kind);
    const [moved] = pills.splice(dragFromIdx, 1);
    pills.splice(toIdx, 0, moved);
    setPills(rootEl, kind, pills);
    dragFromIdx = null;
  });
}

// ── Settings ────────────────────────────────────────────────────────────

function onSettingChange(rootEl, ctrl) {
  const key = ctrl.dataset.setting;
  if (key === 'dim') {
    const [w, h] = ctrl.value.split('x').map(Number);
    settings.width = w;
    settings.height = h;
    return;
  }
  let val = ctrl.type === 'number' || ctrl.type === 'range' ? Number(ctrl.value) : ctrl.value;
  // sampler is an int via the select value.
  if (key === 'sampler') val = Number(val);
  if (key === 'steps') val = Math.max(1, Math.min(50, val));
  if (key === 'cfg') val = Math.round(val * 10) / 10;
  settings[key] = val;
  // Update the readout span next to the label.
  const readout = rootEl.querySelector(`[data-readout="${key}"]`);
  if (readout) {
    readout.textContent = key === 'cfg' ? val.toFixed(1) : val;
  }
}

// ── Seed lock ───────────────────────────────────────────────────────────

function onSeedLockToggle(rootEl) {
  const btn = rootEl.querySelector('[data-action="seed-lock"]');
  const seedInput = rootEl.querySelector('[data-setting="seed"]');
  if (!btn || !seedInput) return;
  if (lockedSeed === null) {
    // Lock: capture the current seed (if -1, we can't lock a random —
    // surface a toast + wait for a real result seed).
    if (settings.seed < 0) {
      if (hooks.onToast) hooks.onToast('Generate once first, then lock to iterate on that seed.');
      return;
    }
    lockedSeed = settings.seed;
    btn.textContent = '🔒';
    btn.title = 'Seed locked — iterating';
    if (hooks.onToast) hooks.onToast('Seed locked. Next Generate reuses this seed.');
  } else {
    lockedSeed = null;
    btn.textContent = '🔓';
    btn.title = 'Lock seed for iteration';
    settings.seed = -1;
    seedInput.value = '-1';
  }
}

// Called by prism.js when a generation succeeds: capture the result's seed
// so a locked session iterates on the REAL seed (the backend may have used
// a random one if the request seed was -1).
export function setLastSeed(seed) {
  // If the user has locked, switch the locked value to the real result seed
  // (so Fork & Edit iterates on the actual base, not a -1 placeholder).
  if (lockedSeed !== null && typeof seed === 'number' && seed >= 0) {
    lockedSeed = seed;
  }
}

// ── Generate ────────────────────────────────────────────────────────────

async function onGenerate(rootEl) {
  const prompt = positivePills.join(', ').trim();
  if (!prompt) {
    if (hooks.onToast) hooks.onToast('Add at least one tag to the prompt.');
    return;
  }
  const negative = negativePills.join(', ').trim();
  // The seed: locked value if locking, else the settings seed (-1 = random).
  const seed = lockedSeed !== null ? lockedSeed : settings.seed;

  const params = {
    prompt,
    negative_prompt: negative || null,
    seed,
    cfg: settings.cfg,
    steps: settings.steps,
    width: settings.width,
    height: settings.height,
    sampler: settings.sampler,
  };

  // UI: disable the button + show a spinner.
  setGenerating(rootEl, true);
  try {
    await generate(params);
    // The result arrives via prism-gen-done (handled in prism.js). The button
    // stays in its "generating" state until that event flips it back.
    if (hooks.onGenerateStarted) hooks.onGenerateStarted(params);
  } catch (err) {
    setGenerating(rootEl, false);
    if (hooks.onToast) hooks.onToast(String(err));
  }
}

function setGenerating(rootEl, on) {
  const btn = rootEl.querySelector('.prism-generate-btn');
  if (!btn) return;
  btn.disabled = !!on;
  btn.classList.toggle('is-generating', !!on);
  const label = btn.querySelector('.prism-generate-label');
  const spinner = btn.querySelector('.prism-generate-spinner');
  if (label) label.textContent = on ? 'Generating…' : 'Generate';
  if (spinner) spinner.hidden = !on;
}

// prism.js calls this from the prism-gen-done handler to re-enable the
// button once the (possibly multi-second) swap completes.
export function setDone(rootEl) {
  setGenerating(rootEl, false);
}

// ── Load from a gallery row (Send to Composer / Fork) ──────────────────

// Populate the composer from an existing image's params — used by "Send to
// Composer" (gallery quick action) and Fork & Edit's seed-lock handoff.
export function loadFromImage(rootEl, img) {
  if (!img) return;
  setPills(rootEl, 'positive', splitPrompt(img.prompt));
  setPills(rootEl, 'negative', splitPrompt(img.negative_prompt));
  settings.cfg = img.cfg;
  settings.steps = img.steps;
  settings.width = img.width;
  settings.height = img.height;
  settings.sampler = img.sampler;
  settings.seed = img.seed;
  // Reflect into the controls.
  reflectSettings(rootEl);
}

function splitPrompt(s) {
  if (!s) return [];
  return s.split(',').map((t) => t.trim()).filter(Boolean);
}

function reflectSettings(rootEl) {
  const dim = rootEl.querySelector('[data-setting="dim"]');
  if (dim) dim.value = `${settings.width}x${settings.height}`;
  const steps = rootEl.querySelector('[data-setting="steps"]');
  if (steps) steps.value = settings.steps;
  const cfg = rootEl.querySelector('[data-setting="cfg"]');
  if (cfg) cfg.value = settings.cfg;
  const sampler = rootEl.querySelector('[data-setting="sampler"]');
  if (sampler) sampler.value = String(settings.sampler);
  const seed = rootEl.querySelector('[data-setting="seed"]');
  if (seed) seed.value = settings.seed;
  const stepsRo = rootEl.querySelector('[data-readout="steps"]');
  if (stepsRo) stepsRo.textContent = settings.steps;
  const cfgRo = rootEl.querySelector('[data-readout="cfg"]');
  if (cfgRo) cfgRo.textContent = settings.cfg.toFixed(1);
}

// ── HTML escaping (pill text comes from user input) ────────────────────

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}
function escapeAttr(s) {
  return escapeHtml(s).replace(/'/g, '&#39;');
}
