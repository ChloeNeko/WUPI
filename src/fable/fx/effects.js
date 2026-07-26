// =============================================================
// GAMES FX ENGINE — port of UIE's sceneEffects.js, keyed on the
// WUPI narrator's 13-name bracket vocabulary.
//
// The backend (bracket_parser.rs) parses [FX name] and emits it as
// { kind: 'fx', effect: '<name>' } in a scene_event. This module
// renders that name. No macro parser here — the backend already
// did the parsing (cheaper, single source of truth).
//
// Effect taxonomy (Prime Directive: stateful vs stateless split):
//   AMBIENT  (rain/snow/fog/vignette/spotlight/letterbox/glitch)
//     → toggled on/off, persistent until cleared.
//   TRANSIENT (flash/whiteout/blackout/thunder)
//     → fire once, auto-remove when the animation ends.
//   SHAKE    (shake-light/shake-heavy)
//     → applied to the stage root for ~600ms.
//
// Particles are CSS-animated <i> elements (GPU-composited); no
// per-frame JS loops. Respects prefers-reduced-motion.
// =============================================================

import './fx.css';

// WUPI vocab → { kind, className, particle? }
// kind: 'ambient' | 'transient' | 'shake'
// particle: if set, spawn N <i> children with this selector.
const FX_TABLE = {
  rain:        { kind: 'ambient',   className: 'fable-fx-rain',     particle: { count: 90, driftRange: 50 } },
  snow:        { kind: 'ambient',   className: 'fable-fx-snow',     particle: { count: 70, driftRange: 60 } },
  fog:         { kind: 'ambient',   className: 'fable-fx-fog' },
  vignette:    { kind: 'ambient',   className: 'fable-fx-vignette' },
  spotlight:   { kind: 'ambient',   className: 'fable-fx-spotlight' },
  letterbox:   { kind: 'ambient',   className: 'fable-fx-letterbox' },
  glitch:      { kind: 'ambient',   className: 'fable-fx-glitch' },
  flash:       { kind: 'transient', className: 'fable-fx-flash',    ms: 500 },
  whiteout:    { kind: 'transient', className: 'fable-fx-whiteout', ms: 1200 },
  blackout:    { kind: 'transient', className: 'fable-fx-blackout', ms: 1500 },
  thunder:     { kind: 'transient', className: 'fable-fx-thunder',  ms: 1000, duck: true },
  'shake-light': { kind: 'shake',   className: 'shake-light', ms: 600 },
  'shake-heavy': { kind: 'shake',   className: 'shake-heavy', ms: 800 },
  // Extras beyond the 13 (still rendered if the model emits them).
  dust:        { kind: 'ambient',   className: 'fable-fx-dust',     particle: { count: 40, driftRange: 80 } },
  sparks:      { kind: 'ambient',   className: 'fable-fx-sparks',   particle: { count: 30, driftRange: 60 } },
};

const reducedMotion = () =>
  window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;

let fxLayer = null;       // the .fable-fx-layer root (set by initFX)
let stageEl = null;       // the .fable-stage root (for shake classes)
let activeAmbient = new Set();  // classNames currently active (idempotent toggles)
let transientTimers = new Map(); // className -> timeout id (for early clear)
let onTransient = null;   // optional callback (audio duck hook for thunder)

// Spawn particle <i> children with randomized CSS vars. Mirrors UIE's
// pattern: each particle is one <i> with --x/--d/--delay/--drift.
function spawnParticles(host, className, count, driftRange) {
  if (reducedMotion()) return;
  const frag = document.createDocumentFragment();
  for (let i = 0; i < count; i++) {
    const p = document.createElement('i');
    const x = Math.random() * 100;        // vw position
    const d = (0.6 + Math.random() * 1.4); // duration multiplier
    const delay = (Math.random() * -3).toFixed(2); // negative = mid-cycle start
    const drift = (Math.random() * driftRange - driftRange / 2).toFixed(0);
    p.style.setProperty('--x', x + 'vw');
    p.style.setProperty('--d', d + 's');
    p.style.setProperty('--delay', delay + 's');
    p.style.setProperty('--drift', drift + 'px');
    frag.appendChild(p);
  }
  host.appendChild(frag);
}

// Init: mount the fx layer + remember the stage for shake.
//   layerSel: the .fable-fx-layer element (created by stage.js)
//   stageSel: the .fable-stage element (shake target)
export function initFX(layerEl, stageElement, hooks = {}) {
  fxLayer = layerEl;
  stageEl = stageElement;
  onTransient = hooks.onTransient || null;
}

// Play an effect by name. Idempotent for ambient (re-playing rain while
// rain is active is a no-op). Transients always re-fire.
export function playFX(name) {
  if (!fxLayer) return;
  const def = FX_TABLE[name];
  if (!def) return; // unknown effect: silently dropped (parser already validated)

  if (def.kind === 'shake') {
    applyShake(def);
    return;
  }

  if (def.kind === 'transient') {
    fireTransient(def);
    return;
  }

  // Ambient: toggle on.
  if (activeAmbient.has(def.className)) return;
  activeAmbient.add(def.className);

  const host = document.createElement('div');
  host.className = def.className;
  host.dataset.fxName = name;
  if (def.particle) {
    spawnParticles(host, def.className, def.particle.count, def.particle.driftRange);
  }
  fxLayer.appendChild(host);
}

function applyShake(def) {
  if (!stageEl || reducedMotion()) return;
  stageEl.classList.add(def.className);
  setTimeout(() => stageEl.classList.remove(def.className), def.ms);
}

function fireTransient(def) {
  const host = document.createElement('div');
  host.className = def.className;
  fxLayer.appendChild(host);
  if (onTransient && def.duck) onTransient(def.className); // thunder → music duck
  const ms = reducedMotion() ? Math.min(def.ms, 200) : def.ms;
  const t = setTimeout(() => {
    host.remove();
    transientTimers.delete(def.className);
  }, ms + 50);
  transientTimers.set(def.className, t);
}

// Clear one ambient effect by name (e.g. [FX rain] off).
export function clearFX(name) {
  const def = FX_TABLE[name];
  if (!def || def.kind !== 'ambient') return;
  activeAmbient.delete(def.className);
  const host = fxLayer && fxLayer.querySelector('.' + def.className);
  if (host) host.remove();
}

// Clear all ambient effects (scene change, game exit).
export function clearAllFX() {
  if (!fxLayer) return;
  activeAmbient.clear();
  // Remove only ambient + lingering transient hosts; keep the layer shell.
  fxLayer.innerHTML = '';
  transientTimers.forEach((t) => clearTimeout(t));
  transientTimers.clear();
}

// Which ambient effects are currently active (for save-state / debug).
export function activeFX() {
  return [...activeAmbient];
}

// ── App-lifecycle pause/resume (freeze CSS particles on alt-tab) ───────
// Fable's FX are CSS-animated <i> particles (GPU-composited, no JS loop), so
// "pausing" them means flipping animation-play-state to paused on the layer.
// This is the onPause/onResume hook target from fable.js's lifecycle: when
// the user alt-tabs away, the rain/snow/dust particles freeze in place (no
// wasted compositor cycles) and resume smoothly on return. Transient effects
// (flash/shake) use one-shot JS setTimeouts that are harmless while blurred,
// so only the persistent ambient particles need freezing. Idempotent.
const FX_PAUSED_CLASS = 'fable-fx-paused';

export function pauseFX() {
  if (fxLayer) fxLayer.classList.add(FX_PAUSED_CLASS);
}

export function resumeFX() {
  if (fxLayer) fxLayer.classList.remove(FX_PAUSED_CLASS);
}
