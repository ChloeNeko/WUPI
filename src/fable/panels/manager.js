// =============================================================
// GAMES PANEL MANAGER — the summon router.
//
// THE LOCKED DESIGN (Direction 3): there are no persistent RPG panels.
// Panels are MODAL OVERLAYS summoned by asking Wupi. When a
// fable_state_query event arrives (player said "show my inventory"),
// the wupi-drawer calls summon(focus, entities, schema). This module
// routes by focus + entity prefixes to the matching read-view panel,
// mounts it as a full-stage overlay with a painted backplate, and
// dismisses on Esc / backdrop click.
//
// Each panel is a single-purpose render function over WorldSchema.
// entities is a plain { id: state } map (the WorldSchema.entities).
// =============================================================

import { renderInventory } from './inventory.js';
import { renderMap } from './map.js';
import { renderActionWheel } from './action-wheel.js';
import { renderSkills } from './skills.js';
import { renderParty } from './party.js';
import { renderCodex } from './codex.js';
import { renderCraft } from './craft.js';

let overlayEl = null;     // #fable-panel-overlay (created lazily, owned here)
let hostEl = null;        // the panel content host inside the overlay
let active = false;
let onDismissCb = null;   // optional: stage's hide-overlay hook

// focus keyword → panel type. First match wins. Pure-ish router.
function classifyFocus(focus, entities) {
  const f = (focus || '').toLowerCase();
  const keys = Object.keys(entities || {});
  const has = (prefix) => keys.some((k) => k.startsWith(prefix));

  if (/\binventory|bag|backpack|carrying|items?\b/.test(f)) return 'inventory';
  if (/\bmap|where|location|travel|fast.?travel|nearby\b/.test(f)) return 'map';
  if (/\bactions?|abilities|options|what can i\b/.test(f)) return 'actions';
  if (/\bskills?|stats?|abilities\b/.test(f)) return 'skills';
  if (/\bparty|companions?|npcs?|who.?s here|who is here\b/.test(f)) return 'party';
  if (/\bcraft|forge|alchemy|cook|kitchen|workshop\b/.test(f)) return 'craft';
  if (/\bcodex|lore|reference|world|summary|recap\b/.test(f)) return 'codex';
  // Fallback by what entities exist.
  if (has('item_') || has('inv_')) return 'inventory';
  if (has('loc_')) return 'map';
  if (has('npc_')) return 'party';
  if (has('skill_')) return 'skills';
  return 'codex'; // default: the world reference (always has summary/events)
}

const RENDERERS = {
  inventory: renderInventory,
  map: renderMap,
  actions: renderActionWheel,
  skills: renderSkills,
  party: renderParty,
  codex: renderCodex,
  craft: renderCraft,
};

export function initPanelManager(overlayElement, hostElement, opts = {}) {
  overlayEl = overlayElement;
  hostEl = hostElement;
  onDismissCb = opts.onDismiss || null;
  // Click backdrop (the .fable-panel-backdrop or the overlay itself) to dismiss.
  overlayEl && overlayEl.addEventListener('click', (e) => {
    if (e.target === overlayEl || e.target.classList.contains('fable-panel-backdrop')) {
      dismiss();
    }
  });
}

// Summon a panel. Routes by focus → renderer. Mounts into the overlay.
export function summon(focus, entities, schema) {
  const type = classifyFocus(focus, entities);
  const html = (RENDERERS[type] || renderCodex)(entities, schema);
  if (hostEl) hostEl.innerHTML = html;
  if (overlayEl) {
    overlayEl.dataset.panelType = type;
    overlayEl.classList.add('open');
  }
  active = true;
}

export function dismiss() {
  if (!overlayEl) return;
  overlayEl.classList.remove('open');
  overlayEl.dataset.panelType = '';
  if (hostEl) hostEl.innerHTML = '';
  active = false;
  if (onDismissCb) onDismissCb();
}

export function isActive() { return active; }

// Esc-to-dismiss (wired by fable.js).
export { dismiss as dismissPanel };
