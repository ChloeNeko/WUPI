// =============================================================
// FABLE SOUL GEMS — the six equipment-slot inventory triggers.
//
// Six glowing diamond gems BLOOM out from the backpack's origin and snap
// onto their anatomical body-region coordinates (Head → Foot) when the
// player clicks the backpack, then RETRACT back into the backpack on a
// second click. Each gem maps to one typed-inventory slot (equipment.rs):
//
//     Head  → Head        Hand  → MainHand/OffHand
//     Chest → Chest       Leg   → Legs
//     Foot  → Feet        Pack  → Pack (the deep-storage quick view)
//
// POSITIONING — bloom from backpack, land on the body (2026-08-08):
// The gems live in their OWN overlay (decoupled from .hud-paperdoll-section
// — they are no longer children of the paperdoll, hence "unanchored" from
// the paperdoll's DOM). But their bloom TARGETS still reference the body:
// each gem's rest position is computed from the paperdoll <img>'s box +
// the lower_torso hitbox centroid + a PER-GENDER nudge (the male + female
// silhouettes differ in width/height, so each gem lands at a different
// point per gender). This is the only way the gems stay glued to the right
// body region across the paperdoll's fluid height + a ♂↔♀ swap.
//
// The bloom is a CSS transform DELTA: each gem's base left/top is the
// backpack CENTER (the collapse point); --target-x/--target-y is the delta
// from the backpack center to the body-region position. Collapsed →
// translate(0,0) (at the backpack); bloomed → translate(target) (on the
// body). So the gems appear to shoot out of the backpack and magnetically
// snap to the head/chest/hands/legs/feet. The long travel (Head climbs
// the full figure) is the feature — the staggered spring snap is the
// whole visual appeal.
//
// STATE MACHINE (held here, driven by left-drawer.js):
//   .is-open   on the overlay  → gems at body targets (bloomed)
//   .is-closed on the overlay  → gems collapsed to backpack (default)
//   .is-active on a single gem → that gem is selected (gold pulse)
//
// Clicking the backpack toggles .is-open ↔ .is-closed. A 350ms cooldown
// (animating) guards against rapid multi-clicks thrashing the transitions —
// it debounces the backpack's own click rhythm ONLY; explicit close commands
// (closeSoulGems: Esc via the drawer's close, the stale-lock sweep) bypass
// it (D6a 2026-08-16, Chloe ruling: a debounce is for hover events, not
// commands). Closing the overlay clears all .is-active gems. Clicking a gem
// sets .is-active on it + deselects the others; the #inventory-panel-slot
// container (owned by left-drawer.js) is filled by inventory-panel.js, which
// renders the per-slot item-button list + the contextual action popup.
//
// RELATION TO body-parts.js:
//   Mirrors injury-heatmap.js's discipline: this module owns ONLY the gem
//   visualization + state. It reuses `getHitbox(id, gender)` to find the
//   anchor region, then computes that region's centroid to derive the body
//   target. body-parts.js remains the single source of truth for the
//   22-part hitbox layer.
// =============================================================

import { getHitbox } from './body-parts.js';

const SVGNS = 'http://www.w3.org/2000/svg';

// The data attribute every gem carries (its machine key on the attr value).
export const SOUL_GEM_DATA_ATTR = 'data-soul-gem';

// The CSS classes toggled on the overlay / gems.
export const SOUL_GEM_OPEN_CLASS = 'is-open';
export const SOUL_GEM_CLOSED_CLASS = 'is-closed';
export const SOUL_GEM_ACTIVE_CLASS = 'is-active';

// Rendered size of each gem node in CSS px (square SVG box).
const GEM_SIZE_PX = 30;

// ── The six gems + their PER-GENDER body-region nudges ─────────────
// `id` is the machine key (the data attr + later IPC); `label` is the
// aria-label. Each gem carries PER-GENDER nudge values (`male`/`female`) —
// px offsets from the lower_torso hitbox centroid (+X = right, −Y = up),
// EXPRESSED AT THE CALIBRATION HEIGHT (CALIB_HEIGHT_PX below).
//
// CALIBRATED 2026-08-08 via the dev tuner (__wupiDebug.gemTuner): dialed
// in by eye against the live paperdoll at Chloe's screen, then hand-
// adjusted. The female Y values are a touch more extreme than male (the
// female silhouette is slightly shorter/narrower, so the upper gems climb
// a bit higher + the lower ones land a touch higher too).
//
// RESOLUTION COMPLIANCE (the load-bearing detail): the paperdoll figure
// height is fluid via CSS clamp(510px, 76.85vh, 1200px). A FIXED-px nudge
// is a DIFFERENT fraction of the body at every figure height → the gems
// drift up/down relative to their body region when the viewport changes
// (fullscreen toggle, 1080p vs 4K). The fix: the stored nudges are scaled
// by (currentHeight / CALIB_HEIGHT_PX) at apply time, so a −276px offset
// at 820px tall stays −276px at 820px but becomes proportionally larger/
// smaller at other heights — same fraction of the body, gems glued at every
// resolution. CALIB_HEIGHT_PX = 820 (76.85vh of Chloe's 1067px screen).
//
// ANCHOR: all gems derive their base body position from the lower_torso
// centroid (resolved per gender via getHitbox), then apply their scaled
// nudge. The nudge values are large (Head climbs ~−276px at calib height
// from the lower chest to the head) because they offset from the lower-
// chest anchor up/down to the real body region.
const CALIB_HEIGHT_PX = 820;

const GEMS = Object.freeze([
  { id: 'head',  label: 'Head',
    male: { x: 12,  y: -276 }, female: { x: 10,  y: -292 } },
  { id: 'chest', label: 'Top',
    male: { x: 7,   y: -112 }, female: { x: 14,  y: -116 } },
  { id: 'hand',  label: 'Hand',
    male: { x: -98, y: 58   }, female: { x: -103, y: 46   } },
  // The pack gem is BACKPACK-ANCHORED (not body-anchored): it represents the
  // inventory/backpack itself, so its bloom target is a DIRECT offset from the
  // backpack's own box — NOT derived from the paperdoll's lower_torso centroid.
  // This is load-bearing: the backpack is drawer-anchored (bottom: clamp()),
  // the paperdoll is independently positioned, so a paperdoll-relative offset
  // only coincidentally lines up with the backpack at ONE resolution. The
  // `anchor: 'backpack'` flag routes it through the backpack-relative path in
  // the reposition loop. The nudge is NOT figure-height-scaled (the backpack
  // has its own sizing independent of the figure).
  { id: 'pack',  label: 'Inventory', anchor: 'backpack',
    male: { x: 0,   y: -135 }, female: { x: 0,   y: -135 } },
  { id: 'leg',   label: 'Bottom',
    male: { x: -13, y: 214  }, female: { x: -14,  y: 206  } },
  { id: 'foot',  label: 'Feet',
    male: { x: -29, y: 390  }, female: { x: -30,  y: 384  } },
]);

// The hitbox part all gems anchor to (the lower chest / solar plexus band).
// Every gem's body position = this part's centroid (per gender) + its nudge.
const ANCHOR_PART_ID = 'lower_torso';

// Per-gem staggered transition-delay (ms). 30ms increment per gem, in the
// array order above (top → bottom) so the bloom reads as a burst.
const STAGGER_STEP_MS = 30;

// The cooldown between backpack toggles. The bloom/retract spring takes
// ~320ms; a 350ms gate prevents a second click from firing mid-transition.
const TOGGLE_COOLDOWN_MS = 350;

// ── Inventory panel slot positioning constants ─────────────────────
// Single source of truth for the #inventory-panel-slot's offset from the
// backpack. Used by BOTH repositionToBody() (the full measure path) AND
// repositionSlotOnly() (the cheap panel-only path) so the two can't drift.
const SLOT_GAP_ABOVE_BACKPACK = 202;  // px the panel climbs above the backpack top
const SLOT_RIGHT_MARGIN = 5;          // px from the drawer's right edge (smaller = further right)

// ── DEV TUNER (live nudge calibration) ─────────────────────────────
// The per-gender nudge values above are Chloe's first-pass guesses — the
// original comment literally said "Chloe moves them to their real slots
// next." Rather than round-tripping screenshots, expose a live arrow-key
// tuner (window.__wupiDebug.gemTuner) so Chloe can dial each gem in by
// eye + read out the exact {x, y} to paste back into the GEMS table.
//
// `liveNudges` shadows the GEMS defaults once the tuner is active. Until
// then it stays null + setNudge() is a no-op (production never touches it).
let liveNudges = null;

// Average of a polygon's vertices → its centroid, in the polygon's own %
// space (0–100). Pure; returns null if the polygon is missing/malformed.
function centroid(poly) {
  if (!Array.isArray(poly) || poly.length === 0) return null;
  let sx = 0, sy = 0;
  for (const p of poly) {
    sx += p[0];
    sy += p[1];
  }
  return { x: sx / poly.length, y: sy / poly.length };
}

// Module state ────────────────────────────────────────────────
// One overlay per drawer. `overlayEl` holds the 6 gems; `backpackEl` is the
// master toggle; `paperdollImg` is the <img> whose box drives the body
// targets. `isOpen` mirrors the CSS class; `animating` is the cooldown flag;
// `activeGem` is the selected gem id (null = none). `currentGender` caches
// the last-placed gender so reposition() can re-measure for the right
// silhouette.
let overlayEl = null;
let backpackEl = null;
let paperdollImg = null;
let drawerRoot = null;
let isOpen = false;
let animating = false;
let activeGem = null;
let currentGender = 'male';

// (P2a, 2026-08-17 E4B shakedown) Last-known-GOOD stamp cache: the inline
// styles written by the last non-degenerate place() pass. On a degenerate
// measurement (hidden/mid-slide drawer — the save→title→Continue resume
// stamped the cluster at x=−119…−426, the whole inventory UI offscreen),
// place() re-stamps these instead of persisting garbage coordinates. Null
// until the first good pass (fresh nodes default to the backpack origin).
let lastGoodStamps = null;

// Build one soul-gem SVG node (art only — positioning is applied by the
// caller). The diamond art: glow + brass frame + ruby stone + light facet.
function buildGemNode(gem) {
  const svg = document.createElementNS(SVGNS, 'svg');
  svg.setAttribute('class', 'soul-gem');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('preserveAspectRatio', 'xMidYMid meet');
  svg.setAttribute('role', 'button');
  svg.setAttribute('tabindex', '0');
  svg.setAttribute('aria-label', gem.label);
  svg.setAttribute(SOUL_GEM_DATA_ATTR, gem.id);
  svg.style.width = GEM_SIZE_PX + 'px';
  svg.style.height = GEM_SIZE_PX + 'px';

  svg.innerHTML = `
    <polygon class="gem-glow" points="12,5 18.5,12 12,19 5.5,12" />
    <polygon class="gem-frame" points="12,1 22,12 12,23 2,12"
             fill="none" stroke-linejoin="miter" />
    <polygon class="gem-stone" points="12,5 18.5,12 12,19 5.5,12" />
    <polygon class="gem-facet" points="12,5 18.5,12 12,12" />
  `;
  return svg;
}

// Build (or rebuild) the soul-gem overlay inside the given drawer root.
// Creates the #inventory-panel-slot container + the overlay holding the 6
// gems, appends each gem with its stagger delay, and starts collapsed.
// The gem positions are NOT final yet — they're placed at the backpack
// origin now + re-measured onto the body by repositionToBody() once the
// paperdoll <img> is resolved. Idempotent: removes any prior overlay first.
//
// `root`      — the drawer element (left-drawer.js's drawerEl).
// `backpack`  — the .hud-backpack button (the bloom origin + master toggle).
// `img`       — the .hud-paperdoll-base <img> (whose box drives body targets).
// `gender`    — 'male' | 'female' (selects the per-gender nudge set).
export function buildSoulGems(root, backpack, img, gender) {
  if (!root || !backpack) return;
  // Tear down any prior build first (idempotent).
  clearSoulGems(root);

  drawerRoot = root;
  overlayEl = null;
  backpackEl = backpack;
  paperdollImg = img || null;
  currentGender = gender === 'female' ? 'female' : 'male';
  isOpen = false;
  animating = false;
  activeGem = null;

  // The inspection panel slot — dedicated container above the backpack. Hidden
  // until a gem is selected; inventory-panel.js renders the item-button list +
  // action popup into it on selection (left-drawer.js::showInventorySlot).
  const slot = document.createElement('div');
  slot.id = 'inventory-panel-slot';
  slot.className = 'inventory-panel-slot';
  slot.setAttribute('aria-hidden', 'true');
  root.appendChild(slot);

  // The gem overlay. Absolutely positioned to cover the drawer; each gem's
  // base left/top is set to the backpack center (the collapse point) + the
  // bloom transform translates it to its body target. Starts collapsed.
  const overlay = document.createElement('div');
  overlay.className = 'soul-gem-overlay ' + SOUL_GEM_CLOSED_CLASS;
  overlay.setAttribute('aria-hidden', 'true');
  overlay.dataset.soulGemOverlay = '';

  // Build + append every gem with its stagger delay. Positions are stamped
  // by repositionToBody() (called next); until then they default to 0,0.
  GEMS.forEach((gem, i) => {
    const node = buildGemNode(gem);
    node.style.transitionDelay = (i * STAGGER_STEP_MS) + 'ms';
    overlay.appendChild(node);
  });

  root.appendChild(overlay);
  overlayEl = overlay;

  // Place every gem at the backpack center + compute body targets. Defers
  // on the <img> load event (the img uses width:auto, so pre-load its box
  // is zero-width → targets would collapse to the anchor). Mirrors the
  // injury-heatmap load-wait discipline.
  repositionToBody();
}

// Re-measure the backpack center + paperdoll body targets, then stamp every
// gem's base left/top (backpack center) + its --target-x/--target-y (the
// delta from backpack center to the body-region position for the current
// gender). Called on build, on gender swap, on window load/resize. No-op if
// the overlay, backpack, or paperdoll img isn't ready.
//
// TRANSITION SUPPRESSION (the resolution-compliance fix): the bloom transform
// (translate(var(--target-x), var(--target-y))) carries a 320ms spring
// transition. When the viewport resizes, the fluid clamp() sizes on the
// paperdoll <img> + the backpack shift → --target-x/--target-y change → the
// gems would VISIBLY SPRING to their new positions, drifting a few px mid-
// resize. To keep the gems glued (resolution-compliance), the reposition
// path suppresses the transition: it adds .is-repositioning (which zeros the
// transition via CSS), writes the new left/top/--target-* values, forces a
// reflow so they commit in the suppressed state, then removes the class. The
// gems snap to their new positions instantly; only bloom/retract animates.
//
// LOAD-WAIT: the paperdoll <img> uses width:auto, so before it loads its
// Clamp the #inventory-panel-slot's height so its TOP edge can never climb
// past the astrolabe/time-bar header at the top of the drawer. The panel is
// bottom-anchored (its bottom sits a fixed gap above the backpack) + has a
// FIXED CSS height (2026-08-19: 376px — it never resizes with its content;
// the add-an-item growth bug); on shorter viewports this measured cap
// shrinks it so its top edge doesn't collide with + overlap the time bar.
// This measures the astrolabe header's bottom edge live, then caps
// max-height to the vertical space between that + the panel's bottom anchor.
// When content is taller than the cap, the panel shrinks + its internal
// scroll viewport takes over (no time-bar overlap). The min-height relax
// (inline) keeps the old CSS floor from ever forcing the panel past the
// astrolabe on very short viewports. ASTROLABE_FLOOR_PX is the fallback
// when the header element can't be measured (matches its CSS box:
// top:6px + ~64px of font/padding/border ≈ 72px; 80px gives a small breath).
const ASTROLABE_FLOOR_PX = 80;
function clampSlotBelowAstrolabe(slot, rootBox) {
  if (!slot || !rootBox) return;
  const header = drawerRoot.querySelector('[data-astrolabe-header]');
  let astrolabeBottomFromDrawerTop = ASTROLABE_FLOOR_PX;
  if (header) {
    const hBox = header.getBoundingClientRect();
    astrolabeBottomFromDrawerTop = hBox.bottom - rootBox.top;
  }
  // The slot's bottom edge (from the drawer's top) = drawer height − the
  // bottom offset we just wrote. Available height for the panel = that bottom
  // edge − the astrolabe's bottom edge, minus a 6px breath so the panel
  // doesn't kiss the time bar.
  const bottomOffset = parseFloat(slot.style.bottom) || 0;
  const slotBottomFromDrawerTop = rootBox.height - bottomOffset;
  const available = slotBottomFromDrawerTop - astrolabeBottomFromDrawerTop - 6;
  if (available > 0) {
    slot.style.maxHeight = available + 'px';
    // If the CSS min-height (300px) would force the top past the astrolabe,
    // relax it to the available space so the floor can't override the cap.
    if (available < 300) slot.style.minHeight = available + 'px';
    else slot.style.minHeight = '';
  }
}

// getBoundingClientRect() has zero width → the body targets would land at
// the anchor. We defer to the img's load event in that case.
export function repositionToBody() {
  if (!overlayEl || !backpackEl || !drawerRoot) return;

  // (P2a) Re-stamp the last known good anchors when the current measurement
  // is degenerate — a hidden or mid-slide drawer must never overwrite good
  // coordinates with offscreen garbage.
  const restampLastGood = () => {
    if (!lastGoodStamps || !overlayEl) return;
    for (const [gemId, stamp] of Object.entries(lastGoodStamps.gems)) {
      const node = overlayEl.querySelector(`[${SOUL_GEM_DATA_ATTR}="${gemId}"]`);
      if (!node) continue;
      node.style.left = stamp.left;
      node.style.top = stamp.top;
      node.style.setProperty('--target-x', stamp.tx);
      node.style.setProperty('--target-y', stamp.ty);
    }
    const slot = drawerRoot.querySelector('#inventory-panel-slot');
    if (slot) {
      slot.style.right = lastGoodStamps.slot.right;
      slot.style.left = 'auto';
      slot.style.bottom = lastGoodStamps.slot.bottom;
      slot.style.top = 'auto';
    }
  };

  const place = () => {
    if (!overlayEl || !backpackEl || !drawerRoot) return false;
    const img = paperdollImg;
    // If there's no img to anchor against, fall back to collapsing all gems
    // onto the backpack center (targets = 0,0). The bloom still works; the
    // gems just stack at the backpack until an img is provided.
    const rootBox = drawerRoot.getBoundingClientRect();
    const bpBox = backpackEl.getBoundingClientRect();
    // (P2a) Degenerate-box guard: a hidden (display:none → 0×0) or
    // mid-slide (anchor outside the root box) drawer measures garbage —
    // the save→title→Continue resume stamped the cluster at negative x.
    // Bail to the last known good anchors instead of persisting them.
    const degenerate =
      rootBox.width < 2 || rootBox.height < 2 ||
      bpBox.width < 2 || bpBox.height < 2;
    const backpackCx = bpBox.left - rootBox.left + bpBox.width / 2;
    const backpackCy = bpBox.top - rootBox.top + bpBox.height / 2;
    const anchorOutsideRoot =
      backpackCx < -2 || backpackCx > rootBox.width + 2 ||
      backpackCy < -2 || backpackCy > rootBox.height + 2;
    if (degenerate || anchorOutsideRoot) {
      restampLastGood();
      return false;
    }

    // Resolve the body anchor (lower_torso centroid) for the current gender.
    const poly = getHitbox(ANCHOR_PART_ID, currentGender);
    const c = poly ? centroid(poly) : null;

    // The img box — needed to map the %-space centroid to drawer px. If the
    // img isn't loaded yet (zero width), defer.
    let imgBox = null;
    if (img) {
      imgBox = img.getBoundingClientRect();
      if (imgBox.width === 0) return false;   // not loaded → defer
    }

    // Suppress the bloom transition while we rewrite positions — a resize
    // must SNAP the gems to their new targets, not spring them.
    overlayEl.classList.add('is-repositioning');

    // RESOLUTION COMPLIANCE: the nudges are calibrated at CALIB_HEIGHT_PX
    // (820px figure height). Scale them by the CURRENT figure height ratio so
    // a nudge that's "1/3 of the way up the body" at 820px stays 1/3 of the
    // way up at any other height — otherwise the gems drift up/down relative
    // to their body region when the fluid figure resizes (fullscreen toggle,
    // 1080p vs 4K). Falls back to 1 (no scaling) if the img box is missing.
    const scale = (imgBox && imgBox.height > 0)
      ? imgBox.height / CALIB_HEIGHT_PX
      : 1;

    const stamps = { gems: {}, slot: {} };
    for (const gem of GEMS) {
      const node = overlayEl.querySelector(`[${SOUL_GEM_DATA_ATTR}="${gem.id}"]`);
      if (!node) continue;
      // Base position = backpack center (the collapse point).
      node.style.left = backpackCx + 'px';
      node.style.top = backpackCy + 'px';
      // Bloom target = delta from backpack center to the rest position.
      let tx = 0, ty = 0;
      // Read the live-tuner override if active, else the table default.
      const live = liveNudges && liveNudges[gem.id];
      const n = (live && live[currentGender]) || gem[currentGender] || gem.male;
      if (gem.anchor === 'backpack') {
        // BACKPACK-ANCHORED gem (the pack/inventory gem): its rest position is
        // a DIRECT offset from the backpack's own box, NOT derived from the
        // paperdoll. The backpack is drawer-anchored (bottom: clamp()), so a
        // paperdoll-relative offset would only line up at ONE resolution. The
        // nudge is unscaled (the backpack sizes independently of the figure).
        tx = n.x;
        ty = n.y;
      } else if (imgBox && c) {
        // BODY-ANCHORED gem: rest position = img-box origin + (% centroid →
        // px) + SCALED nudge. The nudge is scaled by the figure-height ratio
        // so it's the same fraction of the body at every resolution.
        const bodyX = (imgBox.left - rootBox.left) + (c.x / 100) * imgBox.width + n.x * scale;
        const bodyY = (imgBox.top - rootBox.top) + (c.y / 100) * imgBox.height + n.y * scale;
        tx = bodyX - backpackCx;
        ty = bodyY - backpackCy;
      }
      node.style.setProperty('--target-x', tx + 'px');
      node.style.setProperty('--target-y', ty + 'px');
      stamps.gems[gem.id] = { left: backpackCx + 'px', top: backpackCy + 'px', tx: tx + 'px', ty: ty + 'px' };
    }

    // Position the #inventory-panel-slot above the backpack. The slot is
    // measured live from the backpack box (same source as the gems) so it
    // can never drift on a resolution change.
    //   HORIZONTAL: right-aligned to the backpack's RIGHT edge (NOT centered
    //     on the backpack X — centering would push half the panel past the
    //     drawer's right boundary → clipped by overflow:hidden). Right-align
    //     keeps it inside the drawer.
    //   VERTICAL: anchored by its BOTTOM edge above the backpack's top edge
    //     (NOT by top — top-anchoring lets the slot grow DOWNWARD into the
    //     backpack when content is injected after the initial measure).
    //     Bottom-anchoring makes it grow upward.
    if (drawerRoot) {
      const slot = drawerRoot.querySelector('#inventory-panel-slot');
      if (slot) {
        slot.style.right = SLOT_RIGHT_MARGIN + 'px';
        slot.style.left = 'auto';
        // Bottom-anchor: distance from the drawer's bottom to the slot's
        // bottom edge = (drawer height − backpack top) + gap.
        const bpTopFromDrawerBottom = rootBox.bottom - bpBox.top;
        slot.style.bottom = (bpTopFromDrawerBottom + SLOT_GAP_ABOVE_BACKPACK) + 'px';
        slot.style.top = 'auto';
        clampSlotBelowAstrolabe(slot, rootBox);
        stamps.slot = { right: SLOT_RIGHT_MARGIN + 'px', bottom: slot.style.bottom };
      }
    }

    // (P2a) Cache the good pass (only once every gem + the slot stamped).
    if (Object.keys(stamps.gems).length === GEMS.length && stamps.slot.right) {
      lastGoodStamps = stamps;
    }

    // Force the writes to commit under the suppressed transition, then drop
    // the class so the next bloom/retract animates normally. Reading
    // offsetHeight is the standard force-reflow incantation.
    void overlayEl.offsetHeight;
    overlayEl.classList.remove('is-repositioning');
    return true;
  };

  if (!place() && paperdollImg) {
    paperdollImg.addEventListener('load', place, { once: true });
  }
}

// (P2a, 2026-08-17 E4B shakedown) Stage-entry recompute: re-measure the gem
// cluster a bounded series of frames AFTER the stage is shown, so a
// mid-transition first measure (the immediate one inside buildSoulGems) is
// always superseded by a settled one. Every stage entry path funnels through
// wireStage (title Continue resume, Load, New Game, the direct-launch
// --card/--save boot) — wireStage calls this right after mountSoulGems.
// The frame ladder covers the entry-hold/wipe classes (~2 frames), the
// drawer slide (~6), and slow paints (~16 ≈ 260ms at 60fps); each run is
// transition-suppressed + degenerate-guarded, so extra runs are free.
let entryRepositionToken = 0;
export function scheduleSoulGemReposition() {
  const token = ++entryRepositionToken;
  // The frame ladder covers the entry-hold/wipe classes (~2 frames), the
  // drawer slide (~6), and slow paints (~16 ≈ 260ms at 60fps). A later
  // settled measure always wins — repositionToBody overwrites every stamp,
  // so mid-ladder passes simply get corrected.
  for (const delayFrames of [2, 4, 8, 16]) {
    setTimeout(() => {
      if (token !== entryRepositionToken) return; // a newer entry superseded us
      requestAnimationFrame(() => {
        if (token !== entryRepositionToken) return;
        repositionToBody();
      });
    }, delayFrames * 16);
  }
}

// rAF-coalesced reposition for the resize listener. A fullscreen-toggle or a
// dragged window edge fires DOZENS of resize events in a burst; measuring +
// reflowing on each thrashes layout. Coalesce to ONE measurement per frame.
// The pending token is module-level so only the last burst's frame runs.
let resizeRafToken = 0;
export function repositionOnResize() {
  if (resizeRafToken) return;
  resizeRafToken = requestAnimationFrame(() => {
    resizeRafToken = 0;
    repositionToBody();
  });
}

// Swap the per-gender nudge set + re-measure. Called by left-drawer.js on a
// ♂↔♀ toggle so the gems follow the new silhouette. Recomputes targets for
// the new gender's lower_torso centroid + nudges.
export function setGender(gender) {
  currentGender = gender === 'female' ? 'female' : 'male';
  repositionToBody();
}

// Re-measure ONLY the #inventory-panel-slot position (not the gems). Cheaper
// than a full repositionToBody() — skips the hitbox centroid + per-gem target
// math. Used by the gem-select path (showInventorySlot) so the panel always
// reveals at the current settled backpack position, not a stale cached one
// from the racey initial build measurement. The same stale-cache bug that hit
// the gems (fixed via fresh-measure-on-open) hits the panel too, but the
// panel's reveal path is gem-SELECT (not backpack-open), so it needs its own
// fresh-measure hook. No-op if the slot/backpack/drawer aren't present.
export function repositionSlotOnly() {
  if (!backpackEl || !drawerRoot) return;
  const slot = drawerRoot.querySelector('#inventory-panel-slot');
  if (!slot) return;
  const rootBox = drawerRoot.getBoundingClientRect();
  const bpBox = backpackEl.getBoundingClientRect();
  slot.style.right = SLOT_RIGHT_MARGIN + 'px';
  slot.style.left = 'auto';
  const bpTopFromDrawerBottom = rootBox.bottom - bpBox.top;
  slot.style.bottom = (bpTopFromDrawerBottom + SLOT_GAP_ABOVE_BACKPACK) + 'px';
  slot.style.top = 'auto';
  clampSlotBelowAstrolabe(slot, rootBox);
}

// ── The master toggle: bloom ↔ retract ───────────────────────────
// Guards against rapid multi-clicks via the animating flag + a 350ms
// cooldown. Toggling open blooms the 6 gems to their body targets; toggling
// close retracts them to the backpack origin + clears any active selection.
// No-op if the overlay isn't built.
//
// FRESH MEASUREMENT ON OPEN (the stale-cache fix): the gem positions are
// measured + cached as inline styles. The FIRST measurement (at stage build,
// inside the img load handler) can race with layout settling (the drawer
// sliding open, the img's drop-shadow filter + flex layout + font metrics
// still reflowing) → getBoundingClientRect() returns a box a few px off its
// final rested value → those slightly-wrong positions get cached + persist.
// Gender swap fixes this because it re-measures from a settled state.
// Reproducing that self-healing on EVERY bloom open: recompute the positions
// fresh before blooming. The recompute is transition-suppressed (the
// is-repositioning class inside repositionToBody) + the gems are collapsed
// (opacity 0) at this instant, so it's invisible — the user sees only the
// bloom to correct positions, never the reposition.
export function toggleSoulGems() {
  if (!overlayEl) return;
  if (animating) return;                 // cooldown guard
  animating = true;
  setTimeout(() => { animating = false; }, TOGGLE_COOLDOWN_MS);

  isOpen = !isOpen;
  if (isOpen) {
    // Re-measure from the current settled layout before blooming. This
    // overwrites any stale cached positions from the racey initial build
    // measurement, so the bloom always lands on correct coordinates.
    repositionToBody();
    overlayEl.classList.remove(SOUL_GEM_CLOSED_CLASS);
    overlayEl.classList.add(SOUL_GEM_OPEN_CLASS);
    overlayEl.setAttribute('aria-hidden', 'false');
  } else {
    applyClosed();
  }
}

// The close state writes, shared by the toggle's retract branch + the
// explicit closeSoulGems entrypoint. Caller guarantees overlayEl exists.
function applyClosed() {
  overlayEl.classList.remove(SOUL_GEM_OPEN_CLASS);
  overlayEl.classList.add(SOUL_GEM_CLOSED_CLASS);
  overlayEl.setAttribute('aria-hidden', 'true');
  clearActiveGem();                      // closing clears selection (spec §3)
}

// Open + close entrypoints (for teardown / external control). openSoulGems
// rides the cooldown-guarded toggle; closeSoulGems is an EXPLICIT COMMAND
// (Esc via closeDrawer, the stale-edge-lock sweep, the drawer's mouseleave
// auto-close) and BYPASSES the cooldown (D6a 2026-08-16, Chloe ruling: a
// debounce is for hover events, not explicit commands) — a close landing
// inside the 350ms window used to no-op and leave the gems bloomed behind a
// just-closed drawer. The `animating` flag stays armed for its full window
// so a mid-retract backpack re-click is still debounced.
export function openSoulGems() {
  if (!overlayEl || isOpen) return;
  toggleSoulGems();
}
export function closeSoulGems() {
  if (!overlayEl || !isOpen) return;
  applyClosed();
}
export function soulGemsOpen() { return isOpen; }

// ── Gem selection (single-select) ────────────────────────────────
// Sets .is-active on the clicked gem + deselects the others. Clicking the
// already-active gem deselects it (toggle). Returns the now-active gem id
// (null if deselected) so the caller can drive the #inventory-panel-slot
// renderer. No-op if the overlay is closed.
export function selectGem(gemId) {
  if (!overlayEl || !isOpen) return null;
  const nodes = overlayEl.querySelectorAll(`[${SOUL_GEM_DATA_ATTR}]`);
  if (gemId && activeGem === gemId) {
    activeGem = null;                    // toggle off
  } else {
    activeGem = gemId || null;
  }
  nodes.forEach((n) => {
    n.classList.toggle(SOUL_GEM_ACTIVE_CLASS, n.getAttribute(SOUL_GEM_DATA_ATTR) === activeGem);
  });
  return activeGem;
}

// Clear the active selection (deselect all). Called on close + on teardown.
export function clearActiveGem() {
  if (!overlayEl) return;
  activeGem = null;
  overlayEl.querySelectorAll('.' + SOUL_GEM_ACTIVE_CLASS).forEach((n) =>
    n.classList.remove(SOUL_GEM_ACTIVE_CLASS)
  );
}

export function getActiveGem() { return activeGem; }

// ── Live nudge override (dev tuner) ──────────────────────────────
// Update ONE gem's nudge for the given gender in-place + re-measure so the
// move is visible immediately. Used by window.__wupiDebug.gemTuner (below).
// Passing null/omitted for x/y keeps that axis unchanged. No-op if the gem
// id is unknown or the overlay isn't built.
export function setNudge(gemId, gender, x, y) {
  const gem = GEMS.find((g) => g.id === gemId);
  if (!gem || !overlayEl) return;
  const g = gender === 'female' ? 'female' : 'male';
  if (!liveNudges) {
    // Seed from the current table so untouched gems keep their default.
    liveNudges = {};
    for (const gm of GEMS) {
      liveNudges[gm.id] = {
        male: { ...gm.male },
        female: { ...gm.female },
      };
    }
  }
  if (Number.isFinite(x)) liveNudges[gemId][g].x = Math.round(x);
  if (Number.isFinite(y)) liveNudges[gemId][g].y = Math.round(y);
  repositionToBody();
}

// Read back a gem's CURRENT effective nudge (live override if active, else
// the table default). Returns {x, y} or null. Used by the tuner's readout.
export function getNudge(gemId, gender) {
  const gem = GEMS.find((g) => g.id === gemId);
  if (!gem) return null;
  const g = gender === 'female' ? 'female' : 'male';
  const live = liveNudges && liveNudges[gemId];
  return (live && live[g]) ? { ...live[g] } : { ...(gem[g] || gem.male) };
}

// ── DEV TUNER self-registration ──────────────────────────────────
// Exposes window.__wupiDebug.gemTuner = { start(), stop() }.
//   start()  — opens the gems + binds the arrow-key handler. 1-6 selects a
//              gem, arrows nudge it (Shift = 10px steps), C prints the full
//              calibrated table formatted for paste-back into GEMS.
//   stop()   — removes the handler (live nudges persist until a reload).
// NEVER surfaced in the shipped UI — same trust class as the hitbox overlay.
//
// SCALE-AWARE: the tuner operates in CALIBRATION space (the values you dial
// in + paste back are always at CALIB_HEIGHT_PX = 820px, regardless of your
// current figure height). The arrow-step is converted from screen-px (what
// you see move) to calib-px (what gets stored) via the current scale factor,
// so pressing ↑ moves the gem 2 screen-px on YOUR monitor but writes the
// equivalent calib-px value. This keeps the printed paste-back values valid
// at every resolution.
const TUNER_GEM_KEYS = ['1', '2', '3', '4', '5', '6'];
let tunerActive = false;
let tunerGemIdx = 0;

// The current figure-height scale factor (currentHeight / CALIB_HEIGHT_PX).
// Returns 1 if the img isn't measurable. Used by the tuner to convert between
// screen-px (arrow steps) + calib-px (stored values).
function currentScale() {
  if (!paperdollImg) return 1;
  const h = paperdollImg.getBoundingClientRect().height;
  return h > 0 ? h / CALIB_HEIGHT_PX : 1;
}

function tunerPrintAll() {
  console.log('%c─── SOUL GEM NUDGES (paste into GEMS table, calibrated at ' +
    CALIB_HEIGHT_PX + 'px figure height) ───', 'color:#E6D58A;font-weight:bold');
  const lines = [];
  for (const gem of GEMS) {
    const m = getNudge(gem.id, 'male');
    const f = getNudge(gem.id, 'female');
    lines.push(
      `  { id: '${gem.id}',  label: '${gem.label}',\n` +
      `    male: { x: ${m.x},  y: ${m.y} }, female: { x: ${f.x},  y: ${f.y} } },`
    );
  }
  console.log(lines.join('\n'));
  console.log('%c─────────────────────────────────────────────', 'color:#E6D58A');
}

function tunerOnKey(e) {
  if (!tunerActive) return;
  const key = e.key;
  // 1-6: select gem.
  const idx = TUNER_GEM_KEYS.indexOf(key);
  if (idx >= 0 && idx < GEMS.length) {
    e.preventDefault();
    tunerGemIdx = idx;
    const n = getNudge(GEMS[tunerGemIdx].id, currentGender);
    console.log(`%c[gem-tuner] → ${GEMS[tunerGemIdx].label} (${GEMS[tunerGemIdx].id})  ${currentGender}  {x:${n.x}, y:${n.y}} (calib-px)`, 'color:#F0CE6A');
    return;
  }
  // Arrows: nudge the selected gem in SCREEN px. Shift = 10px, else 2px.
  // Convert to calib-px before storing so the printed values stay valid.
  const step = e.shiftKey ? 10 : 2;
  const s = currentScale();             // screen-px → calib-px conversion
  const gem = GEMS[tunerGemIdx];
  const n = getNudge(gem.id, currentGender);
  let dx = 0, dy = 0;
  if (key === 'ArrowUp')    { dy = -step; }
  else if (key === 'ArrowDown')  { dy = step; }
  else if (key === 'ArrowLeft')  { dx = -step; }
  else if (key === 'ArrowRight') { dx = step; }
  else if (key === 'c' || key === 'C') { e.preventDefault(); tunerPrintAll(); return; }
  else return;
  e.preventDefault();
  // Store in calib-px: (current calib value) + (screen-px delta / scale).
  setNudge(gem.id, currentGender, n.x + dx / s, n.y + dy / s);
  const after = getNudge(gem.id, currentGender);
  console.log(`%c[gem-tuner] ${gem.label} (${currentGender})  x:${after.x}  y:${after.y} (calib-px)`, 'color:#F0CE6A');
}

window.__wupiDebug = window.__wupiDebug || {};
window.__wupiDebug.gemTuner = {
  start() {
    if (tunerActive) return;
    if (!overlayEl) { console.warn('[gem-tuner] open the Fable drawer first'); return; }
    tunerActive = true;
    tunerGemIdx = 0;
    openSoulGems();               // bloom so you can see what you're tuning
    window.addEventListener('keydown', tunerOnKey);
    console.log(
      '%c[gem-tuner] ON — keys: 1-6 select gem · ←↑↓→ nudge (Shift=10px) · C print all · call __wupiDebug.gemTuner.stop()',
      'color:#F0CE6A;font-weight:bold'
    );
    const n = getNudge(GEMS[0].id, currentGender);
    console.log(`%c[gem-tuner] → ${GEMS[0].label}  {x:${n.x}, y:${n.y}}`, 'color:#F0CE6A');
  },
  stop() {
    if (!tunerActive) return;
    tunerActive = false;
    window.removeEventListener('keydown', tunerOnKey);
    console.log('%c[gem-tuner] OFF (nudges persist until reload)', 'color:#E6D58A');
  },
};
// Remove the overlay + the panel slot. Called from resetLeftDrawer so a
// stale open state can't flash on re-entry. Resets all module state.
export function clearSoulGems(root) {
  isOpen = false;
  animating = false;
  activeGem = null;
  overlayEl = null;
  backpackEl = null;
  paperdollImg = null;
  drawerRoot = null;
  // (P2a) Drop the last-good anchors with the rest of the state — the next
  // build re-measures from scratch (a stale anchor must never outlive its
  // overlay).
  lastGoodStamps = null;
  entryRepositionToken += 1; // cancel any in-flight entry ladder
  if (!root) return;
  const overlay = root.querySelector('.soul-gem-overlay');
  if (overlay) overlay.remove();
  const slot = root.querySelector('#inventory-panel-slot');
  if (slot) slot.remove();
}

// The full gem descriptor list (id + label + per-gender nudges). Exported
// so a future renderer can map a gem id → its label without re-encoding.
export function soulGemSet() { return GEMS; }
