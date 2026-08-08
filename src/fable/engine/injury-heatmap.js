// =============================================================
// FABLE INJURY HEATMAP — the localized paperdoll injury overlay
//   §1  SEVERITY MAP    — the 6 BodyPartState tiers → color +
//                         tooltip label. Mirrors the Rust enum
//                         (`src-tauri/src/player_state.rs`) verbatim:
//                         the wire format is the PascalCase variant
//                         name (`"Orange"`, `"Black"`), so the keys here
//                         ARE those PascalCase names — that's what
//                         `fable_schema_get`'s `player_state.body` ships.
//   §2  SEAM            — PascalCase wire key → the frontend snake_case
//                         part id. One table; rejects unknown keys.
//   §3  RENDER          — paintInjuryHeatmap(sectionEl, gender, bodyMap):
//                         one absolutely-positioned SVG over the
//                         paperdoll, one radialGradient per INJURED
//                         part only (healthy = renders nothing =
//                         completely invisible), a soft "bruise" glow
//                         centered on the part's bbox + fading to 0
//                         before the line-art boundary.
//   §4  HOVER           — a single delegated mouseenter/mouseleave pair
//                         reveals a dark-gold tooltip (the unified
//                         `.hud-tooltip` aesthetic, fable.css) anchored
//                         to the injured zone's center. Only injured
//                         zones are pointer-active (cursor: help).
//
// RELATION TO body-parts.js:
//   This is the "separate, deferred surface that will reuse getHitbox()
//   + the same polygons" promised in body-parts.js's header. It does
//   NOT redefine the 22 parts or the polygons — body-parts.js is the
//   single source of truth for both. This module owns ONLY the injury
//   visualization layer.
//
// IDEMPOTENT: re-painting replaces the prior overlay (keyed by
// `data-injury-overlay`), mirroring body-parts.js's paintDebugOverlay.
// =============================================================

import { getHitbox, idToLabel, PARTS } from './body-parts.js';

const SVGNS = 'http://www.w3.org/2000/svg';
const OVERLAY_DATA_ATTR = 'data-injury-overlay';
const TOOLTIP_DATA_ATTR = 'data-injury-tooltip';

// ─── §1  SEVERITY MAP ──────────────────────────────────────────────────────
// Each tier carries the radial-gradient color (as an "R,G,B" triple so the
// same value drives both the inner 0.8-alpha + outer 0-alpha stops) + the
// human label appended to the part name in the tooltip.
//
// `Transparent` (Healthy) is intentionally absent: a healthy part renders
// NOTHING — no polygon, no gradient, no pointer events. "Healthy =
// completely invisible" is satisfied trivially by omission, which is also
// cheaper than 22 hidden polygons.
//
// `Black` (Amputated) is the one tier that does NOT use a radial glow —
// there's no living tissue to bruise, so it renders a flat low-opacity
// dark fill instead. The `glow: false` flag routes it down the solid-fill
// path in the renderer.
const SEVERITY = Object.freeze({
  Yellow: { rgb: '255,215,0',   label: 'Minor Injury',        glow: true  },
  Orange: { rgb: '255,140,0',   label: 'Medium Injury',       glow: true  },
  Red:    { rgb: '220,20,60',   label: 'Heavy Injury',        glow: true  },
  Purple: { rgb: '128,0,128',   label: 'Critical Condition',  glow: true  },
  Black:  { rgb: '40,40,40',    label: 'Amputated',           glow: false },
});

// ─── §2  SEAM — PascalCase wire key → frontend snake_case part id ──────────
// The Rust `BodyPart` enum serializes its variants as PascalCase
// (`"LeftUpperArm"`, `"UpperTorso"`). The frontend hitbox layer keys on
// the snake_case id (`"left_upper_arm"`). This is the one place the two
// vocabularies meet. Built once from PARTS (the snake_case source of
// truth) by PascalCasing each id — so adding a part in body-parts.js
// auto-extends the seam without a second edit here.
const WIRE_TO_ID = Object.freeze((() => {
  const map = Object.create(null);
  for (const { id } of PARTS) {
    // snake_case id → PascalCase: "left_upper_arm" → "LeftUpperArm".
    const wire = id
      .split('_')
      .map((seg) => seg.charAt(0).toUpperCase() + seg.slice(1))
      .join('');
    map[wire] = id;
  }
  return map;
})());

// ─── §3  RENDER ────────────────────────────────────────────────────────────

// Compute the bounding-box center + half-diagonal radius of a polygon (in
// the polygon's own % coordinate space). The center seeds the radialGradient
// cx/cy; the half-diagonal seeds `r` so the glow fills the region but fades
// to 0 alpha before it reaches the line-art boundary (the "soft bruise that
// seeps in then fades out" effect). Pure.
function bboxCenterAndRadius(poly) {
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const [x, y] of poly) {
    if (x < minX) minX = x;
    if (y < minY) minY = y;
    if (x > maxX) maxX = x;
    if (y > maxY) maxY = y;
  }
  const cx = (minX + maxX) / 2;
  const cy = (minY + maxY) / 2;
  // Half the diagonal of the bbox — reaches the corners, so an elongated
  // part (an upper arm) gets an elongated soft glow rather than a tight
  // dot clipped short of its ends.
  const r = Math.hypot(maxX - minX, maxY - minY) / 2;
  return { cx, cy, r: Math.max(r, 0.5) }; // floor so a degenerate poly still draws
}

// Escape a string for safe injection into a tooltip's textContent. The
// tooltip is built with textContent (not innerHTML), so this is belt-and-
// braces; kept so a future refactor to innerHTML can't introduce an XSS
// via a part label or severity word.
function escapeText(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

// paintInjuryHeatmap(sectionEl, gender, bodyMap)
//
//   sectionEl — the `.hud-paperdoll-section` element (the paperdoll <img>'s
//               positioning anchor; the overlay mirrors the img's CSS box
//               via the `.injury-heatmap-overlay` rule in fable.css).
//   gender    — 'male' | 'female' (drives which polygon set to use).
//   bodyMap   — the `player_state.body` object from fable_schema_get: a
//               { "LeftUpperArm": "Orange", ... } map of PascalCase wire
//               keys → PascalCase BodyPartState tier. May be {} / null /
//               undefined (dormant / no game) → renders nothing.
//
// Idempotent: a prior overlay under the same section is removed first.
// Healthy parts render NOTHING. Injured parts render one <polygon> + (for
// glow tiers) one <radialGradient>. Returns early on a no-op (no section,
// no body, or no injuries) so the common healthy state costs ~zero DOM.
export function paintInjuryHeatmap(sectionEl, gender, bodyMap) {
  if (!sectionEl) return;

  // Remove any prior overlay (idempotent re-paint). Also drop any tooltip
  // from the prior paint so a stale tooltip can't linger across a re-render.
  const priorOverlay = sectionEl.querySelector(`[${OVERLAY_DATA_ATTR}]`);
  if (priorOverlay) priorOverlay.remove();
  const priorTooltip = sectionEl.querySelector(`[${TOOLTIP_DATA_ATTR}]`);
  if (priorTooltip) priorTooltip.remove();

  // No body map → nothing to render. (Dormant / Quick Play pre-seed / IPC
  // failure — all route here. Leaves the paperdoll clean.)
  if (!bodyMap || typeof bodyMap !== 'object') return;

  // Collect the injured parts: each is a { id, tier } pair where id is the
  // frontend snake_case key + tier is the SEVERITY entry. Unknown wire keys
  // (a part Rust no longer tracks, or a future enum extension) are skipped
  // — the seam is the authority, not the raw body map.
  const injured = [];
  for (const [wireKey, tierName] of Object.entries(bodyMap)) {
    const id = WIRE_TO_ID[wireKey];
    if (!id) continue;                  // unknown wire key → drop, never throw
    const tier = SEVERITY[tierName];
    if (!tier) continue;                // unknown tier → drop (defensive)
    // Transparent is the only tier absent from SEVERITY, so a healthy part
    // never reaches here. Belt-and-braces: an explicit "Transparent" tier
    // name would also be skipped (SEVERITY.Transparent is undefined).
    injured.push({ id, tier, tierName });
  }
  if (injured.length === 0) return;     // fully healthy → render nothing

  // Build the SVG overlay. viewBox 0 0 100 100 + preserveAspectRatio none so
  // the stored %-coords land correctly regardless of the stage's rendered
  // size (same trick as body-parts.js's debug overlay — the stage is sized
  // to the PNG aspect, so x% and y% map straight onto the figure).
  //
  // POSITIONING: like body-parts.js's paintDebugOverlay, we measure the
  // paperdoll <img>'s rendered bounding box at paint time + inline-set the
  // SVG's width/height/left/top to match PIXEL-FOR-PIXEL. The paperdoll <img>
  // is sized by fable.css (a literal px height + width:auto), so its box is
  // stable for a given drawer width — but measuring (rather than hardcoding
  // the CSS values) is what keeps the overlay glued to the figure across
  // drawer resizes, gender swaps (the male/female PNGs differ in intrinsic
  // aspect), + any future CSS tweak. This is the load-bearing alignment
  // detail; the CSS rule sets position/z-index/pointer-events, NOT geometry.
  const img = sectionEl.querySelector('img.hud-paperdoll-base, .hud-paperdoll-base, img');
  const box = (img || sectionEl).getBoundingClientRect();
  const sectionBox = sectionEl.getBoundingClientRect();

  const svg = document.createElementNS(SVGNS, 'svg');
  svg.setAttribute('class', 'injury-heatmap-overlay');
  svg.setAttribute('viewBox', '0 0 100 100');
  svg.setAttribute('preserveAspectRatio', 'none');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute(OVERLAY_DATA_ATTR, gender);
  // pixel-perfect match to the paperdoll image: width/height in px + offset
  // from the section's top-left so absolute positioning lands the SVG on the
  // figure. Mirrors body-parts.js:243-246 verbatim.
  svg.style.width = box.width + 'px';
  svg.style.height = box.height + 'px';
  svg.style.left = (box.left - sectionBox.left) + 'px';
  svg.style.top = (box.top - sectionBox.top) + 'px';

  // <defs> holds one radialGradient per glow-tier injured part. Amputated
  // (Black, glow:false) skips the gradient entirely — it uses a flat fill.
  const defs = document.createElementNS(SVGNS, 'defs');

  for (const { id, tier, tierName } of injured) {
    const poly = getHitbox(id, gender);
    if (!poly) continue;                // part has no polygon for this gender

    const polygon = document.createElementNS(SVGNS, 'polygon');
    polygon.setAttribute('points', poly.map((p) => `${p[0]},${p[1]}`).join(' '));
    polygon.setAttribute('data-part-id', id);
    polygon.setAttribute('data-severity', tierName);
    polygon.setAttribute('class', 'is-injured'); // CSS drives opacity + pointer-events

    if (tier.glow) {
      // Soft radial bruise: center on the bbox center, radius = half-diagonal,
      // 0.8 alpha at the core fading to 0 at the edge. One gradient per part
      // (each part's bbox center differs, so the glow sits ON the injury).
      const { cx, cy, r } = bboxCenterAndRadius(poly);
      const gradId = `injury-grad-${id}`;
      const grad = document.createElementNS(SVGNS, 'radialGradient');
      grad.setAttribute('id', gradId);
      grad.setAttribute('cx', String(cx));
      grad.setAttribute('cy', String(cy));
      grad.setAttribute('r', String(r));
      grad.setAttribute('gradientUnits', 'userSpaceOnUse'); // cx/cy/r in the viewBox's % space
      const stopInner = document.createElementNS(SVGNS, 'stop');
      stopInner.setAttribute('offset', '0%');
      stopInner.setAttribute('stop-color', `rgba(${tier.rgb},0.8)`);
      const stopOuter = document.createElementNS(SVGNS, 'stop');
      stopOuter.setAttribute('offset', '100%');
      stopOuter.setAttribute('stop-color', `rgba(${tier.rgb},0)`);
      grad.appendChild(stopInner);
      grad.appendChild(stopOuter);
      defs.appendChild(grad);
      polygon.setAttribute('fill', `url(#${gradId})`);
    } else {
      // Amputated: flat low-opacity dark fill — no glow (no living tissue).
      polygon.setAttribute('fill', `rgba(${tier.rgb},0.55)`);
    }

    svg.appendChild(polygon);
  }

  if (defs.childNodes.length > 0) {
    svg.insertBefore(defs, svg.firstChild);
  }
  sectionEl.appendChild(svg);

  // ── §4  HOVER — one delegated listener reveals a dark-gold tooltip ──
  // Attached to the SVG (not per-polygon) so 1 injured part costs 0 extra
  // listeners + 22 injured parts still cost 0 extra. mouseenter/mouseleave
  // DON'T bubble, so we use mouseover/mouseout + check the relatedTarget
  // crossed a polygon boundary (the standard delegation pattern).
  const tooltip = document.createElement('div');
  tooltip.setAttribute('class', 'injury-heatmap-tooltip');
  tooltip.setAttribute(TOOLTIP_DATA_ATTR, '');
  tooltip.setAttribute('role', 'tooltip');
  tooltip.style.display = 'none';
  sectionEl.appendChild(tooltip);

  // Position the tooltip at the injured zone's CENTER (in section-relative
  // pixels). The polygon's coords are in % of the paperdoll <img>'s box;
  // the overlay SVG mirrors that box, so we read the SVG's bounding rect
  // + map the part's bbox center % → pixels.
  function showTooltipFor(polygon) {
    const id = polygon.getAttribute('data-part-id');
    const tierName = polygon.getAttribute('data-severity');
    const partLabel = idToLabel(id) || id;
    const tierLabel = (SEVERITY[tierName] && SEVERITY[tierName].label) || tierName;
    tooltip.textContent = `${escapeText(partLabel)} — ${escapeText(tierLabel)}`;
    tooltip.style.display = 'block';
    // Force a reflow so the opacity transition fires from 0 → 1 rather
    // than snapping (display:none → block + opacity:0 → 1 in the same
    // frame skips the transition).
    void tooltip.offsetWidth;
    tooltip.classList.add('is-visible');

    // Anchor to the part's bbox center, mapped from % coords → section pixels.
    const poly = getHitbox(id, svg.getAttribute(OVERLAY_DATA_ATTR));
    if (poly) {
      const { cx, cy } = bboxCenterAndRadius(poly);
      const rect = svg.getBoundingClientRect();
      // The section is position:relative; the SVG fills the img's box. Use
      // the SVG's rect relative to the section to place the tooltip.
      const sectionRect = sectionEl.getBoundingClientRect();
      const px = rect.left - sectionRect.left + (cx / 100) * rect.width;
      const py = rect.top - sectionRect.top + (cy / 100) * rect.height;
      // Center the tooltip on the point (translate -50%, -50% via CSS class
      // is applied; here we set the raw left/top). The CSS handles the
      // transform so the badge centers ON the point.
      tooltip.style.left = `${px}px`;
      tooltip.style.top = `${py}px`;
    }
  }

  function hideTooltip() {
    tooltip.classList.remove('is-visible');
    // Hide after the fade-out so the transition completes before display:none.
    setTimeout(() => {
      if (!tooltip.classList.contains('is-visible')) tooltip.style.display = 'none';
    }, 200);
  }

  svg.addEventListener('mouseover', (e) => {
    const poly = e.target.closest('polygon.is-injured');
    if (poly && poly.ownerSVGElement === svg) showTooltipFor(poly);
  });
  svg.addEventListener('mouseout', (e) => {
    const poly = e.target.closest('polygon.is-injured');
    const related = e.relatedTarget;
    if (poly && !(related && poly.contains(related))) hideTooltip();
  });
  // Keyboard accessibility: focusable injured polygons reveal the tooltip
  // too. Tab order follows document order (anatomical, head→foot).
  svg.addEventListener('focusin', (e) => {
    const poly = e.target.closest && e.target.closest('polygon.is-injured');
    if (poly && poly.ownerSVGElement === svg) showTooltipFor(poly);
  });
  svg.addEventListener('focusout', (e) => {
    const poly = e.target.closest && e.target.closest('polygon.is-injured');
    if (poly) hideTooltip();
  });
}

// Remove the heatmap overlay + tooltip from a section (the resetLeftDrawer
// path uses this so a stale heatmap can't flash on re-entry). Idempotent.
export function clearInjuryHeatmap(sectionEl) {
  if (!sectionEl) return;
  const overlay = sectionEl.querySelector(`[${OVERLAY_DATA_ATTR}]`);
  if (overlay) overlay.remove();
  const tooltip = sectionEl.querySelector(`[${TOOLTIP_DATA_ATTR}]`);
  if (tooltip) tooltip.remove();
}
