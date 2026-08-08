// =============================================================
// FABLE BODY PARTS — canonical 22-part paperdoll hitbox layer
//   §1  PARTS          — the locked list of 22 body parts + their
//                        stable machine ids (snake_case).
//   §2  HITBOXES       — the hand-placed 8-vertex polygons per gender,
//                        loaded from paperdoll-hitboxes.json (the file
//                        Chloe authors via src/hitbox-editor.html).
//   §3  LOOKUP         — partAt(point, gender): which part a 2D point
//                        (in % of the paperdoll PNG) lands on. Pure +
//                        allocation-free (ray-cast inside the polygon).
//   §4  DEBUG OVERLAY  — paintDebugOverlay(stageEl, gender): draws the
//                        outlines + ids so a developer can verify the
//                        hitboxes line up with the silhouette. NEVER
//                        shown to end users — the runtime injury
//                        display is a separate, deferred surface.
//
// WHY A MODULE (not inline in left-drawer.js):
//   The 22 parts are referenced from multiple future call sites — the
//   injury display, the click-to-inspect-a-region interaction, any
//   body-targeted bracket command. One module owns the names + the
//   polygons + the lookup so they can't drift apart. left-drawer.js
//   imports from here; nothing else hardcodes part names.
//
// DATA INVARIANT: every entry in PARTS MUST exist in the JSON under
// BOTH "male" and "female" with exactly 8 [x,y] vertices. The
// paperdoll-hitbox-editor enforces this on export; the loader below
// asserts it on import.
// =============================================================

import HITBOXES_JSON from '../data/paperdoll-hitboxes.json';

// ─── §1  PARTS ─────────────────────────────────────────────────────────────
// The 22 body parts (LOCKED — do not rename or add without Chloe's sign-off).
// Order = body top → bottom (matches the hitbox-editor sidebar + the JSON
// authoring order).
//
// `id`  : the stable machine identifier (snake_case). This is what any
//         future persistence (schema field, status-tag kind, IPC) keys on —
//         never the human label, which may be restyled. Lowercase, no spaces.
// `label`: the human-facing display name. Matches the hitbox-editor + JSON
//          keys exactly so the editor's output maps 1:1 to these entries.
//
// LEFT/RIGHT CONVENTION: the stored polygons use the VIEWER's (mirror)
// perspective — "Left Shoulder" is the box at screen-RIGHT (the character's
// own left). This is the standard for a forward-facing figure and matches
// how Chloe placed them. The ids carry the same viewer-perspective names.
// If a future backend BodyPart enum uses the character's-own perspective,
// the mapping happens at the seam (not here).
export const PARTS = Object.freeze([
  { id: 'head',            label: 'Head' },
  { id: 'neck',            label: 'Neck' },
  { id: 'upper_torso',     label: 'Upper Torso' },
  { id: 'lower_torso',     label: 'Lower Torso' },
  { id: 'left_shoulder',   label: 'Left Shoulder' },
  { id: 'right_shoulder',  label: 'Right Shoulder' },
  { id: 'left_upper_arm',  label: 'Left Upper Arm' },
  { id: 'right_upper_arm', label: 'Right Upper Arm' },
  { id: 'left_elbow',      label: 'Left Elbow' },
  { id: 'right_elbow',     label: 'Right Elbow' },
  { id: 'left_lower_arm',  label: 'Left Lower Arm' },
  { id: 'right_lower_arm', label: 'Right Lower Arm' },
  { id: 'left_hand',       label: 'Left Hand' },
  { id: 'right_hand',      label: 'Right Hand' },
  { id: 'left_upper_leg',  label: 'Left Upper Leg' },
  { id: 'right_upper_leg', label: 'Right Upper Leg' },
  { id: 'left_knee',       label: 'Left Knee' },
  { id: 'right_knee',      label: 'Right Knee' },
  { id: 'left_lower_leg',  label: 'Left Lower Leg' },
  { id: 'right_lower_leg', label: 'Right Lower Leg' },
  { id: 'left_foot',       label: 'Left Foot' },
  { id: 'right_foot',      label: 'Right Foot' } ]);

// Frozen lookup tables built once from PARTS.
const BY_ID = Object.freeze(
  Object.fromEntries(PARTS.map((p) => [p.id, p])));
const BY_LABEL = Object.freeze(
  Object.fromEntries(PARTS.map((p) => [p.label, p])));

// All 22 ids in order. Convenience for iteration + assertion checks.
export const PART_IDS = Object.freeze(PARTS.map((p) => p.id));

// Accessors. labelToId('Left Shoulder') → 'left_shoulder'; partById(...) →
// the full {id,label} entry. All return undefined on a miss (never throw).
export function labelToId(label) { return BY_LABEL[label] && BY_LABEL[label].id; }
export function idToLabel(id)     { return BY_ID[id] && BY_ID[id].label; }
export function partById(id)      { return BY_ID[id]; }
export function partByLabel(label){ return BY_LABEL[label]; }

// ─── §2  HITBOXES ──────────────────────────────────────────────────────────
// Load + validate the JSON once at module load. The file stores each part as
// an array of 8 [x,y] pairs in PERCENT of the paperdoll PNG's intrinsic
// dimensions (0–100). We convert to plain [x,y] arrays here (the lookup +
// renderer use the same shape, so no conversion needed — kept as arrays).
//
// Validate hard at import: a missing/malformed part is a DATA BUG (the editor
// or the file drifted), not a runtime condition to silently paper over. Fail
// loudly in the console so it's caught immediately during dev.
const HITBOXES = validateHitboxes(HITBOXES_JSON);

function validateHitboxes(json) {
  const out = { male: {}, female: {} };
  for (const gender of ['male', 'female']) {
    const g = json && json[gender];
    if (!g || typeof g !== 'object') {
      console.error(`[body-parts] hitbox JSON missing ".${gender}"`);
      continue;
    }
    for (const { id, label } of PARTS) {
      const raw = g[label];
      if (!Array.isArray(raw)) {
        console.error(`[body-parts] ${gender}.${label}: missing or not an array`);
        continue;
      }
      if (raw.length !== 8) {
        console.error(`[body-parts] ${gender}.${label}: expected 8 vertices, got ${raw.length}`);
        continue;
      }
      // normalize each vertex to [x,y] numbers + range-check
      const pts = raw.map((pt, i) => {
        const x = Array.isArray(pt) ? pt[0] : pt.x;
        const y = Array.isArray(pt) ? pt[1] : pt.y;
        const xn = Number(x), yn = Number(y);
        if (!Number.isFinite(xn) || !Number.isFinite(yn)) {
          console.error(`[body-parts] ${gender}.${label}[${i}]: bad vertex`, pt);
          return [0, 0];
        }
        return [xn, yn];
      });
      out[gender][id] = pts;
    }
  }
  return out;
}

// Get the 8-vertex polygon ([x,y][] in % of the PNG) for a part by id +
// gender. Returns undefined if the part or gender is unknown. Pure.
export function getHitbox(id, gender) {
  const g = HITBOXES[gender];
  return g ? g[id] : undefined;
}

// Get all hitboxes for a gender as { id: polygon } — useful for rendering.
// Returns a NEW object each call (callers may mutate freely). Pure.
export function getHitboxesForGender(gender) {
  const g = HITBOXES[gender];
  return g ? { ...g } : {};
}

// ─── §3  LOOKUP (point-in-polygon) ─────────────────────────────────────────
// Ray-cast: count crossings of a horizontal ray from the point. Odd = inside.
// The classic PNPOLY algorithm (W. Randolph Franklin). Handles convex +
// concave polygons, self-intersections are undefined (our polygons are simple).
//
// `px, py` + the polygon vertices are all in the SAME coordinate space
// (percent of the paperdoll PNG, 0–100). The caller converts a pixel click
// to % before calling (see stagePixelsToPercent below).
export function partAt(px, py, gender) {
  const g = HITBOXES[gender];
  if (!g) return undefined;
  // Iterate in stable PARTS order so the FIRST match (top-of-body first) wins
  // when polygons overlap at a boundary — predictable resolution.
  for (const { id } of PARTS) {
    const poly = g[id];
    if (poly && pointInPolygon(px, py, poly)) return id;
  }
  return undefined;
}

function pointInPolygon(px, py, poly) {
  // poly: array of [x,y] in the same space as px,py.
  let inside = false;
  const n = poly.length;
  for (let i = 0, j = n - 1; i < n; j = i++) {
    const xi = poly[i][0], yi = poly[i][1];
    const xj = poly[j][0], yj = poly[j][1];
    // does the edge (j→i) cross the horizontal ray at y=py to the right of px?
    const intersect = (yi > py) !== (yj > py) &&
      px < ((xj - xi) * (py - yi)) / (yj - yi) + xi;
    if (intersect) inside = !inside;
  }
  return inside;
}

// Convert a pixel point (e.g. a mouse event's clientX/Y relative to the
// paperdoll <img>) to the % coordinate space the hitboxes live in. Pass the
// paperdoll element's bounding rect (getBoundingClientRect). Pure.
export function stagePixelsToPercent(pixelX, pixelY, rect) {
  if (!rect || rect.width === 0 || rect.height === 0) return { x: -1, y: -1 };
  return {
    x: ((pixelX - rect.left) / rect.width) * 100,
    y: ((pixelY - rect.top) / rect.height) * 100,
  };
}

// ─── §4  DEBUG OVERLAY ─────────────────────────────────────────────────────
// paintDebugOverlay(stageEl, gender, opts?) — draws an SVG layer of all 22
// outlines + their ids over a paperdoll container. This is for DEVELOPER
// verification (confirming the JSON lines up with the silhouette after an
// edit) and is NEVER shown to end users. The runtime injury display is a
// separate, deferred surface that will reuse getHitbox() + the same polygons.
//
// Idempotent: re-painting replaces the prior overlay (keyed by a data attr).
// opts.activeId highlights one part (thicker stroke). opts.visible=false
// removes the overlay.
//
// The SVG uses viewBox 0 0 100 100 + preserveAspectRatio none so the stored
// %-coords land correctly regardless of the stage's rendered size (the stage
// is sized to the PNG aspect, so x% and y% map straight onto the figure).
const SVGNS = 'http://www.w3.org/2000/svg';
const OVERLAY_DATA_ATTR = 'data-body-parts-debug';

export function paintDebugOverlay(stageEl, gender, opts = {}) {
  if (!stageEl) return;
  // remove any prior overlay (idempotent re-paint)
  const prior = stageEl.querySelector(`[${OVERLAY_DATA_ATTR}]`);
  if (prior) prior.remove();
  if (opts.visible === false) return;

  const g = HITBOXES[gender];
  if (!g) return;

  // ── SIZE THE SVG TO THE PAPERDOLL <img>'S RENDERED BOX ──────────────────
  // Load-bearing: an SVG with width:auto does NOT inherit a sibling <img>'s
  // aspect ratio — it collapses to its default box (often square), which
  // squashes viewBox 0 0 100 100 + stretches every polygon's X. The fix is
  // to measure the actual paperdoll image + set this SVG's pixel size +
  // position to match it EXACTLY. We look for the .hud-paperdoll-base img
  // inside the stage (the paperdoll section anchors both). Falls back to the
  // stage's own rect if the image isn't found (keeps the overlay usable in
  // the standalone hitbox-editor, which has no paperdoll-base img).
  const img = stageEl.querySelector('img.hud-paperdoll-base, .hud-paperdoll-base, img');
  const box = (img || stageEl).getBoundingClientRect();
  const parentBox = stageEl.getBoundingClientRect();

  const svg = document.createElementNS(SVGNS, 'svg');
  svg.setAttribute('class', 'body-parts-debug-overlay');
  svg.setAttribute('viewBox', '0 0 100 100');
  svg.setAttribute('preserveAspectRatio', 'none');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute(OVERLAY_DATA_ATTR, gender);
  // pixel-perfect match to the image: width/height in px + offset from the
  // stage's top-left so absolute positioning lands the SVG on the figure.
  svg.style.width = box.width + 'px';
  svg.style.height = box.height + 'px';
  svg.style.left = (box.left - parentBox.left) + 'px';
  svg.style.top = (box.top - parentBox.top) + 'px';

  for (const { id, label } of PARTS) {
    const poly = g[id];
    if (!poly) continue;
    const el = document.createElementNS(SVGNS, 'polygon');
    el.setAttribute('points', poly.map((p) => `${p[0]},${p[1]}`).join(' '));
    el.setAttribute('data-part-id', id);
    el.setAttribute('vector-effect', 'non-scaling-stroke');
    el.setAttribute('fill', 'rgba(74,158,255,0.10)');
    el.setAttribute('stroke', id === opts.activeId ? '#4a9eff' : 'rgba(212,175,55,0.55)');
    el.setAttribute('stroke-width', id === opts.activeId ? '2' : '1');
    svg.appendChild(el);

    // label at the polygon's centroid (avg of vertices) so the id sits in
    // the middle of each region
    const cx = poly.reduce((s, p) => s + p[0], 0) / poly.length;
    const cy = poly.reduce((s, p) => s + p[1], 0) / poly.length;
    const text = document.createElementNS(SVGNS, 'text');
    text.setAttribute('x', cx);
    text.setAttribute('y', cy);
    text.setAttribute('font-size', '1.6');
    text.setAttribute('text-anchor', 'middle');
    text.setAttribute('fill', '#e6e6e6');
    text.setAttribute('stroke', 'rgba(0,0,0,0.8)');
    text.setAttribute('stroke-width', '0.25');
    text.setAttribute('paint-order', 'stroke');
    text.setAttribute('font-family', 'monospace');
    // show the short id (snake_case) — the dev-facing key, not the label
    text.textContent = id;
    svg.appendChild(text);
  }

  stageEl.appendChild(svg);
}

// ─── Smoke self-check (dev only, logs to console) ──────────────────────────
// Runs once at import: confirms every part has a polygon for both genders.
// A failure here means the JSON file drifted from PARTS (someone renamed a
// key in the editor or hand-edited the file). Loud console warning, no throw
// — the app still runs, individual lookups just miss for the broken part.
(function selfCheck() {
  for (const { id, label } of PARTS) {
    for (const gender of ['male', 'female']) {
      const poly = HITBOXES[gender] && HITBOXES[gender][id];
      if (!poly || poly.length !== 8) {
        console.warn(`[body-parts] self-check: ${gender}.${label} (${id}) has no valid polygon`);
      }
    }
  }
})();
