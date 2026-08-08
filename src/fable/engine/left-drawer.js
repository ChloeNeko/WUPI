// =============================================================
// FABLE LEFT DRAWER — Visual HUD
//   §1  Paperdoll — a static PNG with a gender toggle (♂/♀). The image is
//                     scaled via a fixed px height in fable.css (.hud-paperdoll-
//                     base) + positioned clear of the gender toggle (top-left).
//                     This module just swaps the src per gender.
//
// The INVENTORY section (equip slots, drag-and-drop grid, context menu, the
// consume/equip/unequip/drop pipelines, the equipment-node paperdoll overlay,
// + the belt/pack hover widgets) was REMOVED (the original panel on
// 2026-08-02; the paperdoll-node overlay + belt/pack widgets on 2026-08-07).
// The standalone CLICKABLE backpack button (.hud-backpack) STAYS — it's the
// inventory affordance anchor, currently a no-op awaiting a new surface. The
// typed Rust inventory model (equipment.rs: 6 slots × 2 layers + belt +
// weight-bounded pack) + the [EQUIP]/[BELT]/[PACK] bracket commands +
// route_to_fable_query's inventory-summary narration path are INTENTIONALLY
// left intact — the data model is correct; only the HUD visualization layers
// over the paperdoll are gone (to be rebuilt later).
// The ambient time-of-day tint + Chronos & Climate panel was REMOVED on
// 2026-08-03 to be redone later.
//
// The drawer mechanics below are UNCHANGED — stage.js's hover-strip +
// edge-lock wiring drives this drawer identically to the right Wupi drawer.
// =============================================================

import MALE_URL from '../assets/paperdoll_male.png';
import FEMALE_URL from '../assets/paperdoll_female.png';
import BACKPACK_URL from '../assets/backpack.png';
// The Mars (♂) / Venus (♀) glyphs are shared verbatim with the Player Creator's
// Gender slide (wizard-engine.js) so the paperdoll toggle + the creator's
// toggle are pixel-identical symbols + colors. Single source of truth.
import { MARS_SVG, VENUS_SVG } from '../screens/wizard-engine.js';
// Backend IPC for the live HUD reads:
//   fable_active_card_get → { name, player_name }  (player_name = '' when unset)
//   fable_schema_get      → full WorldSchema (world_clock / weather / travel_graph)
import { invoke } from '@tauri-apps/api/core';
// The localized injury heatmap: paints a soft radial "bruise" per injured
// body part over the paperdoll. Reads the same schema's player_state.body
// the rest of the HUD trusts — no new IPC. Healthy parts render nothing.
import { paintInjuryHeatmap, clearInjuryHeatmap } from './injury-heatmap.js';
// paintDebugOverlay is the DEV-ONLY hitbox verification layer from
// body-parts.js (window.__wupiDebug.showHitboxes). body-parts.js is already
// pulled into the main chunk via injury-heatmap.js above, so we static-import
// the debug painter too (a lazy import() here would trip Vite's mixed-import
// warning + buy nothing — the module is resident regardless).
import { paintDebugOverlay } from './body-parts.js';

// ─── Time scrubber SVG art (2026-08-06 redesign) ────────────────────────
// Authored inline SVGs for the Sun (left endcap, amber) + Moon (right endcap,
// midnight white) glyphs. Pure SVG strings (no external assets) so they recolor
// via currentColor + scale with the slot. Kept here (not in a separate asset
// module) because they are small + tightly coupled to the scrubber's endcaps.
// currentColor = each slot's CSS color (amber for the Sun, white for the Moon).
const SUN_SVG = `
<svg class="scrubber-glyph-svg" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
  <circle cx="12" cy="12" r="4.6" fill="currentColor"/>
  <g stroke="currentColor" stroke-width="1.0" stroke-linecap="round">
    <line x1="12" y1="2.6" x2="12" y2="5.0"/>
    <line x1="12" y1="19.0" x2="12" y2="21.4"/>
    <line x1="2.6" y1="12" x2="5.0" y2="12"/>
    <line x1="19.0" y1="12" x2="21.4" y2="12"/>
    <line x1="5.0" y1="5.0" x2="6.8" y2="6.8"/>
    <line x1="17.2" y1="17.2" x2="19.0" y2="19.0"/>
    <line x1="19.0" y1="5.0" x2="17.2" y2="6.8"/>
    <line x1="6.8" y1="17.2" x2="5.0" y2="19.0"/>
  </g>
</svg>`;
const MOON_SVG = `
<svg class="scrubber-glyph-svg" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
  <path d="M15.5 14.5 A7 7 0 1 1 9.5 4.6 A5.4 5.4 0 0 0 15.5 14.5 Z"
        fill="currentColor"/>
</svg>`;

// ─── 3-icon status row art (REBUILT 2026-08-06) ─────────────────────────
// Minimalist LINE-ART SVGs for the three dock icons: calendar (date/day),
// weather (condition), + location (travel node). All outline-only via
// currentColor so they pick up the shared HUD icon color + recolor on hover.
//
// STROKE WEIGHT MATCHING (the load-bearing detail): these icons are calibrated
// to match the gender glyph (♂/♀) EXACTLY. The gender glyph is stroke-width 12
// in a 100-unit viewBox rendered at 38px → 4.56px of actual stroke. To match
// that in a 24-unit viewBox rendered at 48px: (4.56 / 48) × 24 ≈ 2.3.
//
// CAPS/JOINS (the OTHER half of the match — was the hidden bug): the gender
// glyph uses stroke-linecap:butt + stroke-linejoin:miter (FLAT ends, sharp
// corners). The prior versions of these icons used linecap:round +
// linejoin:round, which adds a full half-circle cap to every open path end →
// visibly fattens the strokes + softens corners. Matching butt/miter here is
// what makes them read as the same weight as the gender glyph.
const ICON_STROKE = 'stroke-width="1.5" stroke-linecap="butt" stroke-linejoin="miter" stroke-miterlimit="4"';

const CALENDAR_SVG = `
<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor"
     ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true">
  <rect x="1.5" y="5" width="17" height="15" rx="1.5"/>
  <line x1="1.5" y1="9.5" x2="18.5" y2="9.5"/>
  <line x1="6" y1="3" x2="6" y2="6.5"/>
  <line x1="14" y1="3" x2="14" y2="6.5"/>
</svg>`;

// Weather glyph set — one SVG per mapped condition. The matcher below picks
// the closest line-art glyph; an unknown/empty condition falls back to the
// sun-cloud default. All outline-only, currentColor, butt/miter caps.
const WEATHER_SVGS = {
  default: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><circle cx="8" cy="8" r="2.5"/><path d="M8 3v1.8M8 11.2V13M3 8h1.8M11.2 8H13M4.5 4.5l1.3 1.3M10.2 10.2l1.3 1.3M4.5 11.5l1.3-1.3M10.2 5.8l1.3-1.3"/><path d="M10.5 18a4 4 0 0 1 4-4 4 4 0 0 1 3.7 2.5A3 3 0 0 1 23 19.5a3 3 0 0 1-3 3H10.5a3 3 0 0 1 0-4.5z"/></svg>`,
  clear: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><circle cx="12" cy="12" r="3.5"/><line x1="12" y1="2" x2="12" y2="5"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="2" y1="12" x2="5" y2="12"/><line x1="19" y1="12" x2="22" y2="12"/><line x1="5" y1="5" x2="7" y2="7"/><line x1="17" y1="17" x2="19" y2="19"/><line x1="19" y1="5" x2="17" y2="7"/><line x1="7" y1="17" x2="5" y2="19"/></svg>`,
  cloud: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M6.5 18a4 4 0 0 1 0-8 5 5 0 0 1 9.4-1.3A4.5 4.5 0 0 1 18 18z"/></svg>`,
  rain: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M6.5 14a4 4 0 0 1 0-8 5 5 0 0 1 9.4-1.3A4.5 4.5 0 0 1 18 14z"/><line x1="8" y1="17" x2="7" y2="20"/><line x1="12" y1="17" x2="11" y2="20"/><line x1="16" y1="17" x2="15" y2="20"/></svg>`,
  snow: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M6.5 13a4 4 0 0 1 0-8 5 5 0 0 1 9.4-1.3A4.5 4.5 0 0 1 18 13z"/><line x1="8" y1="17.5" x2="8" y2="18.5"/><line x1="12" y1="17.5" x2="12" y2="18.5"/><line x1="16" y1="17.5" x2="16" y2="18.5"/><line x1="8" y1="20.5" x2="8" y2="21.5"/><line x1="12" y1="20.5" x2="12" y2="21.5"/><line x1="16" y1="20.5" x2="16" y2="21.5"/></svg>`,
  fog: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><line x1="3.5" y1="8" x2="20.5" y2="8"/><line x1="5" y1="12" x2="19" y2="12"/><line x1="3.5" y1="16" x2="17" y2="16"/><line x1="7" y1="20" x2="20.5" y2="20"/></svg>`,
  storm: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M6.5 13a4 4 0 0 1 0-8 5 5 0 0 1 9.4-1.3A4.5 4.5 0 0 1 18 13z"/><polyline points="13,14 10.5,18 13,18 11,22"/></svg>`,
};

// Location glyph set — keyed to the travel node's `setting` hint (schema.rs)
// when available, else a sensible default sign. Outline-only, currentColor,
// butt/miter caps. The `home` (safe zone) + `hostile` keys ship in the map for
// forward-compat with a future zone-safety field; today only indoor/outdoor/
// default are reachable (the backend has no safe-zone/hostile classification).
const LOCATION_SVGS = {
  default: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M12 21s-6.5-6-6.5-11a6.5 6.5 0 0 1 13 0c0 5-6.5 11-6.5 11z"/><circle cx="12" cy="10" r="2.5"/></svg>`,
  indoor: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M4 21V9.5l8-5 8 5V21"/><path d="M9.5 21v-6h5v6"/><line x1="4" y1="21" x2="20" y2="21"/></svg>`,
  outdoor: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M3.5 21h17"/><path d="M7 21V11.5l5-4 5 4V21"/><path d="M12 3.5v3.5"/><path d="M3.5 16.5l3.5-2.7 5 2.7 5-2.7 3.5 2.7"/></svg>`,
  home: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M3.5 11.5L12 4l8.5 7.5"/><path d="M5.5 10v10h13V10"/><path d="M10 20v-5h4v5"/></svg>`,
  hostile: `<svg class="hud-status-svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" ${ICON_STROKE} xmlns="http://www.w3.org/2000/svg" aria-hidden="true"><path d="M12 3l9 16H3z"/><line x1="12" y1="10" x2="12" y2="14"/><circle cx="12" cy="16.8" r="0.6" fill="currentColor" stroke="none"/></svg>`,
};

// Maps a travel node's free-form `setting` hint to a location glyph key.
// Backend only emits 'indoor' / 'outdoor' / '' (schema.rs Node.setting); a
// future zone-safety field could route to 'home' / 'hostile' here. Pure.
function classifyLocation(setting) {
  const s = String(setting || '').toLowerCase().trim();
  if (s === 'indoor') return 'indoor';
  if (s === 'outdoor') return 'outdoor';
  return 'default';
}

// ─── Drawer mechanics (UNCHANGED from the 2026-08-01 empty shell) ─────────
let drawerEl = null;
let isOpen = false;
let locked = false;
let edgeLockVisible = () => false;

// ─── DEV-ONLY hitbox overlay state ──────────────────────────────────────────
// True while the body-parts debug overlay (window.__wupiDebug.showHitboxes) is
// on screen. The gender-toggle handler reads this to decide whether to
// re-paint the overlay for the newly-swapped silhouette — so a developer
// verifying hitboxes can flip ♂↔♀ + the outlines follow automatically. Stays
// false in normal use (the overlay is never shown to users), so the toggle's
// re-paint branch is a no-op in production.
let hitboxDebugVisible = false;

// ─── HUD state ────────────────────────────────────────────────────────────
// The stored value may be capitalized ("Male"/"Female") — the Player Creator
// writes the display form into the same localStorage key. Normalize on every
// comparison so a capitalized value drives the right PNG + toggle state.
let gender = localStorage.getItem('wupi.paperdoll.gender') || 'male'; // 'male' | 'female' | 'Male' | 'Female'
function normGender(g) {
  const v = String(g || '').trim().toLowerCase();
  return v === 'female' ? 'female' : 'male';
}

// ─── Slide-up info-card state (§3, 2026-08-06) ───────────────────────────
// Only ONE of {calendar, weather, location} is open at a time; clicking the
// active icon (or outside the dock + card) closes it. `lastSnap` caches the
// most recent renderStatusRow snapshot so a card body can be re-rendered from
// live data after a narrator turn without a fresh IPC (refreshAll drives both).
let activeCardDock = null;            // null | 'calendar' | 'weather' | 'location'
let lastSnap = null;                  // { playerName, cardName, clock, weather, node }

// ─── Injury heatmap state ────────────────────────────────────────────────
// The last body map refreshAll() fetched. Cached so the gender toggle can
// RE-PAINT the heatmap against the new silhouette's hitboxes WITHOUT a fresh
// IPC (the injuries don't change when the user flips the cosmetic gender
// toggle — only the polygon set does). null until the first successful
// refreshAll; reset to null in resetLeftDrawer so a stale map can't bleed
// into a new session.
let lastBodyMap = null;

// ===========================================================================
// §1  PAPERDOLL — static silhouette
// ===========================================================================
// The figure is a plain PNG with a gender toggle. The per-region SVG hitbox
// overlay, injury-tier coloring, and hover tooltips were removed on 2026-08-02
// to be redesigned later. The Rust body-part types (PlayerState.body,
// BodyPart, BodyPartState) remain as dormant scaffolding — this file no
// longer reads player_state_get for the paperdoll.

// ===========================================================================
// AUDIO — soft glass clink (synthesized via the shared AudioContext).
// Mirror of script.js playLaunchChime: two sine oscillators, short decay.
// No asset file. Lazily (re)creates window.__wupiAudioCtx if absent (the OS
// shell owns the canonical singleton, but the Fable stage can be entered
// before any prior user gesture created it).
// ===========================================================================
function playUIClink() {
  const Ctx = window.AudioContext || window.webkitAudioContext;
  if (!Ctx) return;
  let ctx = window.__wupiAudioCtx;
  if (!ctx) {
    try { ctx = new Ctx(); window.__wupiAudioCtx = ctx; }
    catch (e) { return; }
  }
  if (ctx.state === 'suspended') ctx.resume().catch(() => {});
  const now = ctx.currentTime;
  // Two-tone clink: 880Hz then 1318.51Hz (A5 → E6), short attack, exp decay.
  [{ f: 880.0, t: 0.0 }, { f: 1318.51, t: 0.06 }].forEach(({ f, t }) => {
    const osc = ctx.createOscillator();
    const gain = ctx.createGain();
    osc.type = 'sine';
    osc.frequency.value = f;
    gain.gain.setValueAtTime(0, now + t);
    gain.gain.linearRampToValueAtTime(0.14, now + t + 0.01);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + t + 0.32);
    osc.connect(gain).connect(ctx.destination);
    osc.start(now + t);
    osc.stop(now + t + 0.36);
  });
}

// ===========================================================================
// BUILD — populate the drawer shell. Called once from stage.js buildStage.
// ===========================================================================
export function buildLeftDrawer() {
  drawerEl = document.createElement('aside');
  drawerEl.className = 'fable-left-drawer';
  drawerEl.dataset.leftDrawer = '';
  drawerEl.setAttribute('aria-hidden', 'true');
  drawerEl.dataset.gender = normGender(gender); // drives paperdoll theming
  // ── Single gender toggle (Chloe 2026-08-06): ONE symbol-only button, no
  //    circle/pill border. Shows the CURRENT gender's glyph (♂ male / ♀
  //    female); clicking it flips to the OTHER gender + swaps the glyph +
  //    recolors to match the Player Creator exactly (male = royal blue,
  //    female = scarlet). The data-gender attribute on the button drives the
  //    CSS color rules (same [data-glyph="..."] selector scheme the creator
  //    uses), so the symbol is always the active gender's color. Reuses the
  //    exact MARS_SVG / VENUS_SVG from wizard-engine.js.
  drawerEl.innerHTML = `
    <div class="astrolabe-header" data-astrolabe-header></div>
    <div class="time-scrubber" data-time-scrubber aria-hidden="true">
      <div class="scrubber-backdrop" aria-hidden="true"></div>
      <div class="scrubber-endcap scrubber-sun" data-scrubber-sun aria-hidden="true"></div>
      <div class="scrubber-track" data-scrubber-track>
        <div class="scrubber-keepalive" aria-hidden="true">
          <div class="scrubber-hit" aria-hidden="true"></div>
        </div>
        <div class="scrubber-time-bubble" data-scrubber-bubble></div>
        <div class="scrubber-diamond" data-scrubber-diamond></div>
        <div class="scrubber-major scrubber-major-8" aria-hidden="true"></div>
      </div>
      <div class="scrubber-endcap scrubber-moon" data-scrubber-moon aria-hidden="true"></div>
    </div>
    <div class="hud-paperdoll-section" aria-label="Character condition">
      <img class="hud-paperdoll-base" data-paperdoll-img alt="" aria-hidden="true">
    </div>
    <button type="button" class="hud-backpack" data-backpack-btn
            aria-label="Inventory" title="Inventory">
      <img class="hud-backpack-img" src="${BACKPACK_URL}" alt="" draggable="false">
    </button>
    <div class="hud-dock" data-hud-dock>
      <button type="button" class="hud-dock-btn" data-gender-btn
              data-gender="${normGender(gender)}" aria-label="Toggle silhouette gender"
              title="Toggle silhouette gender"></button>
      <button type="button" class="hud-dock-btn" data-dock="calendar"
              aria-label="Calendar" title="Calendar">
        <span class="hud-dock-icon">${CALENDAR_SVG}</span>
        <span class="hud-dock-day" data-dock-day aria-hidden="true"></span>
      </button>
      <button type="button" class="hud-dock-btn" data-dock="weather"
              aria-label="Weather" title="Weather">
        <span class="hud-dock-icon" data-weather-icon>${WEATHER_SVGS.default}</span>
      </button>
      <button type="button" class="hud-dock-btn" data-dock="location"
              aria-label="Location" title="Location">
        <span class="hud-dock-icon" data-location-icon>${LOCATION_SVGS.default}</span>
      </button>
    </div>
    <div class="info-card-container" data-info-card aria-hidden="true">
      <div class="info-card-body" data-info-card-body></div>
    </div>
  `;
  wireInteractions(drawerEl);
  renderGenderGlyph(); // paint the initial glyph + color
  // Paint the fixed Sun/Moon glyphs (these never change), then load the live
  // world state once. The header (player name), the diamond position, + the
  // 3-icon status row are all driven by refreshAll() — no more hardcoded
  // "DAY 14 • AUTUMN EQUINOX" placeholder.
  setScrubberSunGlyph(SUN_SVG);
  setScrubberMoonGlyph(MOON_SVG);
  // Initial dormant-state render so the HUD isn't empty before the first IPC
  // resolves (refreshAll paints over this once the schema arrives).
  setScrubberMinutes(16 * 60 + 30); // default notch position: 4:30 PM (Chloe 2026-08-06) — overwritten by refreshAll once the live clock arrives
  renderStatusRow({ playerName: '', cardName: '', clock: null, weather: '', node: null });
  refreshAll().catch(() => {});              // best-effort first paint
  // ── DEV-ONLY: expose a debug toggle for the body-parts hitbox overlay.
  //    NEVER surfaced in the shipped UI — this exists so a developer can
  //    verify the hand-placed hitbox polygons (body-parts.js §4) line up
  //    with the silhouette after an edit. Call from the console:
  //       window.__wupiDebug.showHitboxes()   // paint over the paperdoll
  //       window.__wupiDebug.hideHitboxes()   // remove
  //    The overlay auto-tracks the active gender: flipping the ♂/♀ toggle
  //    re-paints for the new silhouette (see the gender-toggle handler in
  //    wireInteractions, which checks `hitboxDebugVisible`). The runtime
  //    injury display (injury-heatmap.js) is a separate, user-visible surface.
  window.__wupiDebug = window.__wupiDebug || {};
  window.__wupiDebug.showHitboxes = () => {
    if (!drawerEl) return;
    const section = drawerEl.querySelector('.hud-paperdoll-section');
    if (section) paintDebugOverlay(section, normGender(gender));
    hitboxDebugVisible = true;
  };
  window.__wupiDebug.hideHitboxes = () => {
    if (!drawerEl) return;
    const prior = drawerEl.querySelector('[data-body-parts-debug]');
    if (prior) prior.remove();
    hitboxDebugVisible = false;
  };
  return drawerEl;
}

// ===========================================================================
// WIRE — bind all interactions once (the drawer element is reused across
// stage entries, so bind in buildLeftDrawer, not on every refresh).
// ===========================================================================
function wireInteractions(root) {
  // ── Gender toggle (single symbol button) ─────────────────────────────
  // One button: clicking flips male↔female, swaps the glyph, recolors to the
  // other gender's color, + persists + swaps the paperdoll PNG. The
  // data-gender attribute on the button is the single source for both the
  // glyph choice + the CSS color rule.
  const btn = root.querySelector('[data-gender-btn]');
  if (btn) {
    btn.addEventListener('click', () => {
      const next = normGender(gender) === 'male' ? 'female' : 'male';
      playUIClink();
      gender = next;
      localStorage.setItem('wupi.paperdoll.gender', next);
      drawerEl.dataset.gender = next;
      renderGenderGlyph();
      renderPaperdoll(); // swap base PNG
      // Re-paint the injury heatmap against the new silhouette's hitbox set.
      // The male + female PNGs differ in intrinsic aspect (450×1510 vs
      // 470×1480), so the paperdoll <img>'s rendered box changes on swap —
      // the heatmap renderer measures that box to position its overlay, so
      // the repaint MUST wait for the new <img> to load (measuring pre-load
      // would use the stale box). The body map is unchanged (cosmetic
      // gender toggle doesn't move injuries) — reuse the cached lastBodyMap
      // rather than re-fetching. No-op when there are no injuries.
      const img = drawerEl.querySelector('[data-paperdoll-img]');
      const repaint = () => {
        const section = drawerEl.querySelector('.hud-paperdoll-section');
        if (section) {
          try {
            paintInjuryHeatmap(section, normGender(gender), lastBodyMap);
          } catch (e) {
            console.warn('[left-drawer] heatmap repaint on gender swap failed:', e);
          }
          // DEV-ONLY: if the hitbox debug overlay is on screen, re-paint it
          // against the new silhouette too (same load-wait reason — the
          // overlay measures the <img> box to position itself). No-op in
          // normal use (hitboxDebugVisible stays false).
          if (hitboxDebugVisible) {
            try {
              paintDebugOverlay(section, normGender(gender));
            } catch (e) {
              console.warn('[left-drawer] hitbox overlay repaint on gender swap failed:', e);
            }
          }
        }
      };
      if (img && !img.complete) {
        img.addEventListener('load', repaint, { once: true });
      } else {
        repaint();
      }
    });
  }

  // ── Slide-up info cards (calendar / weather / location) ─────────────
  // Clicking one of the three dock icons toggles its card; re-clicking the
  // active icon closes it; clicking anywhere in the drawer OUTSIDE the dock +
  // the card itself also closes it (one card open at a time).
  root.querySelectorAll('[data-dock]').forEach((icon) => {
    icon.addEventListener('click', (e) => {
      e.stopPropagation(); // the drawer-level click-outside handler below
      toggleInfoCard(icon.dataset.dock);
    });
  });
  // Click-outside close: a drawer click that didn't originate in the dock or
  // the card collapses the card. stopPropagation on the dock icons + the card
  // body keeps this firing only for genuine outside clicks.
  root.addEventListener('click', (e) => {
    if (!activeCardDock) return;
    if (e.target.closest('[data-hud-dock]')) return;
    if (e.target.closest('[data-info-card]')) return;
    closeInfoCard();
  });

  // ── Time-notch hysteresis (Chloe 2026-08-07) ─────────────────────────
  // The time dropdown should (a) only TRIGGER when the mouse is on the notch
  // (a tight 30×48 trigger box), but (b) not VANISH the instant the mouse
  // drifts slightly off — once active, a larger keep-alive zone (2× the
  // trigger) holds it lit until the mouse leaves that bigger area. Pure CSS
  // :hover can't do this (the :hover target IS the trigger, so it deactivates
  // the moment you leave it). Solution: a JS-driven `.is-notch-active` class
  // on the track. mouseenter on the small trigger → activate; mouseleave on
  // the large keep-alive wrapper → deactivate. The CSS reveals the notch
  // scale + bubble off `.is-notch-active` instead of `:hover`.
  const hit = root.querySelector('.scrubber-hit');
  const keepalive = root.querySelector('.scrubber-keepalive');
  const track = root.querySelector('[data-scrubber-track]');
  if (hit && keepalive && track) {
    const activate = () => track.classList.add('is-notch-active');
    const deactivate = () => track.classList.remove('is-notch-active');
    hit.addEventListener('mouseenter', activate);
    keepalive.addEventListener('mouseleave', deactivate);
  }
}

// Paint the gender glyph into the single toggle button + tag it with the
// active gender so the CSS color rules ([data-gender="male"] / ["female"])
// apply. Called on build + every toggle.
function renderGenderGlyph() {
  if (!drawerEl) return;
  const btn = drawerEl.querySelector('[data-gender-btn]');
  if (!btn) return;
  const g = normGender(gender);
  btn.dataset.gender = g;
  btn.innerHTML = g === 'female' ? VENUS_SVG : MARS_SVG;
}

// ===========================================================================
// RENDERERS — each is idempotent (safe to call repeatedly).
// ===========================================================================

// §1 — paperdoll. Swaps the base PNG for the active gender. The CSS
// (.hud-paperdoll-base in fable.css) sets the px height + positions the
// figure clear of the gender toggle; this just swaps the src.
function renderPaperdoll() {
  if (!drawerEl) return;
  const img = drawerEl.querySelector('[data-paperdoll-img]');
  if (img) img.src = normGender(gender) === 'female' ? FEMALE_URL : MALE_URL;
}

// External setter so the Player Creator's Gender slide can sync the
// paperdoll BEFORE stage entry even when this module was already imported
// (the module-load `gender` capture at line 32 would otherwise be stale on
// a stage re-entry without a page reload). Writes the same localStorage
// key + mirrors exactly what the in-drawer toggle does, minus the clink
// (the drawer isn't open at create time). Safe to call before buildLeftDrawer
// ran — it updates the module-level `gender` so the first render is correct.
export function setPaperdollGender(g) {
  // Accept any casing ("Male"/"female"/"FEMALE"); normalize internally.
  // The stored value + the drawer's dataset use the normalized lowercase
  // form (the drawer's own toggle also writes lowercase, so this keeps the
  // localStorage key consistent regardless of who wrote it).
  const n = normGender(g);
  if (n !== 'male' && n !== 'female') return;
  gender = n;
  localStorage.setItem('wupi.paperdoll.gender', n);
  if (drawerEl) {
    drawerEl.dataset.gender = n;
    if (drawerEl.querySelector('[data-paperdoll-img]')) renderPaperdoll();
    // Sync the single toggle button's glyph + color to the new gender.
    if (drawerEl.querySelector('[data-gender-btn]')) renderGenderGlyph();
  }
}

// ===========================================================================
// TIME SCRUBBER — top-half world-stats widget (2026-08-06 redesign).
// Replaced the semi-circle astrolabe with a SLEEK HORIZONTAL TIME SCRUBBER:
//   [Sun endcap] ─── thin brass line ─── [diamond indicator] ─── [Moon endcap]
// The diamond rides along the line; its horizontal position (left:%) maps to
// the time of day (0% = Sun/midnight-left, 100% = Moon/right). The gold Cinzel
// header (DAY 14 • AUTUMN EQUINOX) sits centered ABOVE the scrubber.
// The bottom half (paperdoll + gender toggle + backpack) is untouched — this
// widget is a descendant of .time-scrubber, absolutely positioned in the top
// region of the drawer.
// ===========================================================================

// Header row (gold Cinzel serif). 2026-08-06: repurposed from the old
// "DAY 14 • AUTUMN EQUINOX" date stamp to the PLAYER'S NAME — the protagonist's
// identity is the load-bearing top label now (the date moved to the Calendar
// tooltip). Text-only (no HTML); the CSS .astrolabe-header uppercases it.
export function setScrubberHeader(text) {
  if (!drawerEl) return;
  const el = drawerEl.querySelector('[data-astrolabe-header]');
  if (el) el.textContent = text || '';
}

// Sun (LEFT endcap) + Moon (RIGHT endcap) glyph slots. glyphHTML is trusted
// authored SVG/HTML; '' / null clears. The Sun is amber (dawn), the Moon is
// midnight white (dusk) — recolored via the CSS on each slot's currentColor.
export function setScrubberSunGlyph(glyphHTML) {
  if (!drawerEl) return;
  const el = drawerEl.querySelector('[data-scrubber-sun]');
  if (el) el.innerHTML = glyphHTML || '';
}
export function setScrubberMoonGlyph(glyphHTML) {
  if (!drawerEl) return;
  const el = drawerEl.querySelector('[data-scrubber-moon]');
  if (el) el.innerHTML = glyphHTML || '';
}

// Raw percentage setter for the diamond indicator (0–100). 0% = flush to the
// Sun/left endcap; 100% = flush to the Moon/right endcap. The diamond is
// translated by half its own width via CSS (transform: translateX(-50%)) so a
// given % centers the diamond on that point of the track. NOTE:
// setScrubberMinutes() below is the canonical path for the world clock; this
// raw setter remains for test/debug + any non-clock positioning use.
export function setScrubberPercent(pct) {
  if (!drawerEl) return;
  const diamond = drawerEl.querySelector('[data-scrubber-diamond]');
  const bubble = drawerEl.querySelector('[data-scrubber-bubble]');
  const hit = drawerEl.querySelector('.scrubber-hit');
  const keepalive = drawerEl.querySelector('.scrubber-keepalive');
  let p = Number(pct);
  if (!Number.isFinite(p)) return;
  p = Math.max(0, Math.min(100, p));     // clamp to [0,100]
  if (diamond) diamond.style.left = `${p}%`;
  // The time bubble rides under the notch — sync its left:% so it tracks the
  // diamond exactly. translateX(-50%) (CSS) centers the bubble ON the %.
  if (bubble) bubble.style.left = `${p}%`;
  // The hover trigger + its keep-alive wrapper both follow the notch (Chloe
  // 2026-08-06: hover must be NEAR the notch, not the whole bar). translate(-50%,
  // -50%) centers each on the notch's left:% so the trigger stays glued to the
  // notch as time moves.
  if (hit) hit.style.left = `${p}%`;
  if (keepalive) keepalive.style.left = `${p}%`;
}

// ── The time → diamond-position engine (12-hour AM/PM) ──────────────────
// The diamond sweeps left→right along the track from the Sun (left, 0%) to
// the Moon (right, 100%) over the full day. The world clock runs 0–1439
// minutes/day. Mapping:
//     minutes 0   →   0% (diamond at the Sun / left endcap — midnight)
//     minutes 720 →  50% (diamond mid-track — noon)
//     minutes 1439 → 100% (diamond at the Moon / right endcap — just before midnight)
// So pct = (minutes / 1439) * 100, a linear minutes→[0,100] map.
// (12-hour symmetry: 7:15 AM + 12h = 7:15 PM land at the same physical spot,
//  opposite sides of noon — see the AM/PM note in setScrubberMinutes.)

// 12-hour AM/PM formatter. minutes ∈ [0,1439] → "h:mm AM/PM" (no leading zero
// on the hour: 7:15 PM, 12:00 AM, 12:30 PM). Pure, no DOM.
function formatTime12h(minutes) {
  let m = Math.round(Number(minutes));
  if (!Number.isFinite(m)) return '';
  // Clamp + wrap so out-of-range callers don't throw.
  m = ((m % 1440) + 1440) % 1440;
  const h24 = Math.floor(m / 60);
  const mm = m % 60;
  let h12 = h24 % 12;
  if (h12 === 0) h12 = 12;            // 0h → 12 AM, 12h → 12 PM
  const ampm = h24 < 12 ? 'AM' : 'PM';
  return `${h12}:${mm.toString().padStart(2, '0')} ${ampm}`;
}

// Time-of-day phase label. minutes ∈ [0,1439] → a named phase band. Pure,
// no DOM. Keep the bands in minute order so the table reads top-to-bottom =
// midnight→night.
function phaseForMinutes(minutes) {
  let m = Number(minutes);
  if (!Number.isFinite(m)) return '';
  m = ((m % 1440) + 1440) % 1440;
  // [lo, hi) minute ranges. Ordered for the first-match cascade.
  if (m < 300)  return 'Night';        // 00:00–04:59
  if (m < 360)  return 'Dawn';         // 05:00–05:59
  if (m < 540)  return 'Morning';      // 06:00–08:59
  if (m < 720)  return 'Day';          // 09:00–11:59
  if (m < 780)  return 'Noon';         // 12:00–12:59
  if (m < 1020) return 'Afternoon';    // 13:00–16:59
  if (m < 1140) return 'Dusk';         // 17:00–18:59
  if (m < 1260) return 'Twilight';     // 19:00–20:59  ← 19:15 lands here
  return 'Night';                      // 21:00–23:59
}

// minutes → diamond track percentage ([0, 100]). Pure, no DOM.
// CALIBRATED to the tick legend (Chloe 2026-08-06): the 8 ticks represent
// 3-hour blocks from 3AM (tick 0) to 12AM-next-day (tick 7), mapped across the
// inset band (6%→94% of the track). So the notch must use the SAME coordinate
// system as the ticks — NOT a raw (minutes/1439) map of the full day (that was
// the prior bug: 4:30PM computed as 68.8% landed past the 6PM tick). The tick
// band represents 3:00 (180 min) → 24:00 (1440 min) = a 1260-min span across
// 6%→94% of the track. Times before 3AM clamp to the band's left edge.
function minutesToPercent(minutes) {
  let m = Number(minutes);
  if (!Number.isFinite(m)) return 0;
  m = ((m % 1440) + 1440) % 1440;     // wrap to [0,1439]
  const BAND_START_MIN = 180;          // 3:00 AM — tick 0
  const BAND_END_MIN = 1440;           // 12:00 AM (next day) — tick 7
  const BAND_START_PCT = 6;            // tick band left edge (% of track)
  const BAND_WIDTH_PCT = 88;           // 94% − 6% — tick band span
  if (m <= BAND_START_MIN) return BAND_START_PCT;            // pre-3AM → clamp left
  if (m >= BAND_END_MIN) return BAND_START_PCT + BAND_WIDTH_PCT; // 12AM → right edge
  const frac = (m - BAND_START_MIN) / (BAND_END_MIN - BAND_START_MIN);
  return BAND_START_PCT + frac * BAND_WIDTH_PCT;
}

// The canonical world-clock setter. Positions the diamond along the track for
// the given minute-of-day (0–1439). Accepts EITHER a raw minute-of-day OR a
// backend epoch-minutes value (current_minutes since 0001-01-01) — it takes
// the value mod 1440 first so epoch minutes land at the right time-of-day
// without the caller having to pre-convert. No readout text rendered here;
// the scrubber is purely visual. This is the canonical call site for the
// live world clock.
export function setScrubberMinutes(minutes) {
  if (!drawerEl) return;
  setScrubberPercent(minutesToPercent(minutes));
  // The time bubble shows the 12-hour readout for the minute-of-day (the notch
  // is visual; the bubble is the textual readout that fades in on hover).
  const bubble = drawerEl.querySelector('[data-scrubber-bubble]');
  if (bubble) bubble.textContent = formatTime12h(minutes);
}

// ===========================================================================
// §2  3-ICON STATUS ROW (Calendar / Weather / Location) — 2026-08-06
// ===========================================================================
// A horizontal flex row of 3 icon clusters below the time scrubber. Each
// cluster is a <button> (keyboard-focusable) wrapping an inline-SVG line-art
// icon + a .hud-tooltip badge. Hovering/focusing the cluster fades the tooltip
// in; all three tooltips share the exact same dark, gold-bordered badge style.
//
// Data sources (verified against schema.rs + lib.rs):
//   • Calendar → world_clock.current_minutes (epoch minutes). The "Day N" +
//     "h:mm AM/PM" lines are DERIVED (no day field exists on WorldClock). The
//     backend has NO calendar/season concept — "Autumn Equinox" etc. is NOT a
//     real field, so the tooltip shows the derived day + time-of-day only.
//   • Weather  → weather.condition (a free-form diegetic phrase). The backend
//     has NO forecast/next/later fields — the spec's "Current › Next › Later"
//     sequence can't be sourced, so the tooltip shows the single live
//     condition (or "Fair" when dormant).
//   • Location → travel_graph.current_node (a node id) resolved to the node's
//     diegetic .name via travel_graph.nodes. The backend's travel graph is a
//     FLAT adjacency list — there is NO region/city/inn hierarchy, so the
//     tooltip shows the node's name + its indoor/outdoor setting (or
//     "Undiscovered" when dormant).
//
// Each cluster's icon SWAPS to a condition-appropriate line-art SVG when the
// live data resolves (sun for clear, cloud for overcast, etc.) so the HUD
// reads the state at a glance, not just on hover.
// ===========================================================================

// Maps a free-form weather condition phrase to a line-art glyph key. Pure.
// Lowercased substring match against common weather vocabulary; falls back to
// 'default' (sun+cloud) for unknown/empty conditions.
function classifyWeather(condition) {
  const c = String(condition || '').toLowerCase().trim();
  if (!c) return 'default';
  if (/(storm|thunder|lightning)/.test(c)) return 'storm';
  if (/(rain|drizzle|shower|downpour|monsoon)/.test(c)) return 'rain';
  if (/(snow|blizzard|hail|sleet|frost)/.test(c)) return 'snow';
  if (/(fog|mist|haze|smog)/.test(c)) return 'fog';
  if (/(cloud|overcast|grey|gray|gloom)/.test(c)) return 'cloud';
  if (/(clear|sunny|sun|fair|bright)/.test(c)) return 'clear';
  return 'default';
}

// Derives the display label for a calendar tooltip from a clock object.
// Returns { day, time } where day = floor(m/1440)+1 (mirrors schema.rs
// render_clock_line) and time = the 12-hour AM/PM readout of m mod 1440.
// Returns nulls when the clock is dormant (current_minutes = 0 / missing).
function deriveClockLines(clock) {
  const raw = clock && Number(clock.current_minutes);
  if (!Number.isFinite(raw) || raw <= 0) {
    return { day: null, time: null };
  }
  const day = Math.floor(raw / 1440) + 1;
  const tod = ((raw % 1440) + 1440) % 1440;
  return { day, time: formatTime12h(tod) };
}

// Render the 3-icon status row from a normalized snapshot. Pure DOM writes;
// safe to call repeatedly. The snapshot shape:
//   { playerName, cardName, clock, weather, node }
//   clock: the raw world_clock object (epoch minutes) — may be null (dormant)
//   weather: the condition string ('' = dormant)
//   node: { name, setting } or null (no current location)
//
// Caches the snapshot in `lastSnap` so the slide-up info cards (§3) can render
// their bodies from the same live data without a second IPC. The dead
// [data-tooltip] hover-badge writes (those elements were never built — latent
// bug) are gone; the click-cards supersede the hover-tooltip intent.
function renderStatusRow(snap) {
  if (!drawerEl) return;
  lastSnap = snap;                       // cache for the info-card renderers
  const { clock, weather, node } = snap;

  // ── Calendar (day-number overlay on the icon) ─────────────────────
  const calLines = deriveClockLines(clock);
  const dayEl = drawerEl.querySelector('[data-dock-day]');
  if (dayEl) {
    dayEl.textContent = calLines.day ? String(calLines.day) : '';
    dayEl.classList.toggle('is-empty', !calLines.day);
  }

  // ── Weather (condition → icon swap) ───────────────────────────────
  const wKey = classifyWeather(weather);
  const wIcon = drawerEl.querySelector('[data-weather-icon]');
  if (wIcon) wIcon.innerHTML = WEATHER_SVGS[wKey] || WEATHER_SVGS.default;

  // ── Location (setting → icon swap) ────────────────────────────────
  const lSetting = (node && node.setting) || '';
  const lKey = classifyLocation(lSetting);
  const lIcon = drawerEl.querySelector('[data-location-icon]');
  if (lIcon) lIcon.innerHTML = LOCATION_SVGS[lKey] || LOCATION_SVGS.default;

  // If a card is currently open, refresh its body so a narrator turn's
  // world-state change reflects live (clock advanced, weather drifted, etc.).
  if (activeCardDock) renderInfoCardBody(activeCardDock);
}

// Card body text contains user-authored content (weather condition, node name),
// so escape it before injecting as innerHTML. The line wrappers / status-tag
// markup below are the only markup; the text itself is textContent-safe.
function escapeHtml(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// ─── Season derivation (Calendar card subtext) ──────────────────────────
// The backend has NO calendar/season field — WorldClock is pure epoch-minutes.
// A quarter heuristic on the derived day index gives a stable, sensible season
// label so the Calendar headline reads "Day 14 • Spring" (per spec) instead of
// just a bare number. 120-day seasons; Day 1–120 Spring, 121–240 Summer, etc.
function seasonForDay(day) {
  const d = Number(day);
  if (!Number.isFinite(d) || d <= 0) return '';
  const q = Math.floor((d - 1) / 120) % 4;
  return ['Spring', 'Summer', 'Autumn', 'Winter'][q];
}

// ===========================================================================
// §3  SLIDE-UP INFO CARDS — click-to-open (2026-08-06)
// One card, three possible bodies (calendar / weather / location). Only one
// dock can be active at a time; clicking the active icon or outside the dock +
// card closes it. Content is MINIMAL per spec — 1 headline line (+ a small
// status tag for Location). No events lists, no exits, no forecast: the backend
// doesn't carry those, so they stay clean rather than fabricated.
// ===========================================================================

// Build the HTML body for the given dock from `lastSnap`. Returns '' if no
// snapshot yet. Each body is a headline (Cinzel brass) + optional sub/tag.
function buildInfoCardHTML(dock) {
  const snap = lastSnap;
  if (!snap) return '';
  if (dock === 'calendar') {
    const { day } = deriveClockLines(snap.clock);
    if (!day) {
      return `<div class="info-card-headline info-card-dim">Time Unset</div>`;
    }
    const season = seasonForDay(day);
    return `<div class="info-card-headline">Day ${escapeHtml(day)}${
      season ? ` <span class="info-card-sep">•</span> ${escapeHtml(season)}` : ''
    }</div>`;
  }
  if (dock === 'weather') {
    const cond = String(snap.weather || '').trim();
    return cond
      ? `<div class="info-card-headline">${escapeHtml(titleCase(cond))}</div>`
      // No temperature/forecast fields exist on the Weather struct — subtext
      // is left clean rather than fabricated (flagged in the plan).
      : `<div class="info-card-headline info-card-dim">Fair</div>`;
  }
  if (dock === 'location') {
    const node = snap.node;
    if (!node || !node.name) {
      return `<div class="info-card-headline info-card-dim">Undiscovered</div>`;
    }
    // Flat travel graph — no region/city/inn breadcrumb exists (flagged in the
    // plan). Headline is the node name; tag is the indoor/outdoor setting.
    // A future zone-safety field could render [ Safe Zone ] / [ Hostile ] here.
    const setting = (node.setting || '').toLowerCase().trim();
    const tagLabel = setting === 'indoor' ? 'Indoor'
                   : setting === 'outdoor' ? 'Outdoor'
                   : '';
    const tag = tagLabel ? `<div class="info-card-tag">[ ${escapeHtml(tagLabel)} ]</div>` : '';
    return `<div class="info-card-headline">${escapeHtml(node.name)}</div>${tag}`;
  }
  return '';
}

// Render the active card's body into the container. Pure DOM write.
function renderInfoCardBody(dock) {
  if (!drawerEl) return;
  const body = drawerEl.querySelector('[data-info-card-body]');
  if (body) body.innerHTML = buildInfoCardHTML(dock);
}

// Title-case a free-form condition phrase ("heavy rain" → "Heavy Rain") for the
// weather headline. Pure.
function titleCase(s) {
  return String(s || '')
    .split(/\s+/)
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(' ');
}

// Toggle a dock's card open/closed. Same dock → close; different dock → swap
// content + open (one card at a time). Mirrors the active state onto the icon.
function toggleInfoCard(dock) {
  if (!drawerEl) return;
  if (activeCardDock === dock) {
    closeInfoCard();
    return;
  }
  activeCardDock = dock;
  // Reflect the active state onto the icon (persistent brighter + glow).
  drawerEl.querySelectorAll('[data-dock]').forEach((b) => {
    b.classList.toggle('is-active', b.dataset.dock === dock);
  });
  renderInfoCardBody(dock);
  const card = drawerEl.querySelector('[data-info-card]');
  if (card) {
    card.classList.add('is-open');
    card.setAttribute('aria-hidden', 'false');
  }
}

// Close the card + clear the active icon state.
function closeInfoCard() {
  if (!drawerEl) return;
  activeCardDock = null;
  const card = drawerEl.querySelector('[data-info-card]');
  if (card) {
    card.classList.remove('is-open');
    card.setAttribute('aria-hidden', 'true');
  }
  drawerEl.querySelectorAll('[data-dock]').forEach((b) => b.classList.remove('is-active'));
}

// ===========================================================================
// PUBLIC — re-render everything from live IPC data.
// Called by stage.js on drawer-open + after each narrator turn.
// ===========================================================================
export async function refreshAll() {
  if (!drawerEl) return;
  // The paperdoll is local-only (gender + PNG) — re-render it always.
  renderPaperdoll();

  // Pull the live card (player name) + the full world schema in parallel.
  // Both are best-effort: a failure leaves the prior render intact (the HUD
  // never blocks the UI on a dead IPC). When no game is active both reject →
  // the dormant fallbacks in renderStatusRow + setScrubberHeader apply.
  let playerName = '';
  let cardName = '';
  let clock = null;
  let weather = '';
  let node = null;
  let bodyMap = null;                 // player_state.body (PascalCase→PascalCase) or null

  try {
    const [cardRes, schema] = await Promise.all([
      invoke('fable_active_card_get').catch(() => null),
      invoke('fable_schema_get').catch(() => null),
    ]);
    // fable_active_card_get → { name, player_name } (object) when a game is
    // active, or null when none is. The player_name is the empty string in
    // Quick Play (no SavedPlayer attached) — fall back to the card name, then
    // to a neutral 'WANDERER' so the header is never blank.
    if (cardRes && typeof cardRes === 'object') {
      cardName = typeof cardRes.name === 'string' ? cardRes.name : '';
      playerName = typeof cardRes.player_name === 'string' ? cardRes.player_name : '';
    }
    if (schema && typeof schema === 'object') {
      clock = schema.world_clock || null;
      weather = (schema.weather && schema.weather.condition) || '';
      const tg = schema.travel_graph;
      const cur = tg && tg.current_node;
      if (cur && Array.isArray(tg.nodes)) {
        const found = tg.nodes.find((n) => n && n.id === cur);
        if (found) {
          node = { name: found.name || cur, setting: found.setting || '' };
        }
      }
      // player_state.body is the per-part injury map (PascalCase wire keys
      // → PascalCase BodyPartState). May be absent on a fresh/dormant schema;
      // null routes the heatmap to the no-op render path (renders nothing).
      bodyMap = (schema.player_state && schema.player_state.body) || null;
    }
  } catch (_) {
    // swallow — dormant fallbacks below stand.
  }

  // Header: player name → card name → 'WANDERER'. The CSS uppercases it.
  const header = (playerName || cardName || 'WANDERER').trim() || 'WANDERER';
  setScrubberHeader(header);

  // Diamond position from the epoch-minutes clock (setScrubberMinutes takes
  // the value mod 1440 internally, so epoch minutes are fine). When no live
  // clock is available (dormant / IPC failed), fall back to 4:30 PM so the
  // scrubber shows a sensible default position rather than zeroing to midnight.
  const DEFAULT_CLOCK_MINUTES = 16 * 60 + 30; // 4:30 PM
  setScrubberMinutes(clock ? (Number(clock.current_minutes) || DEFAULT_CLOCK_MINUTES) : DEFAULT_CLOCK_MINUTES);

  // The 3-icon row.
  renderStatusRow({ playerName, cardName, clock, weather, node });

  // The injury heatmap: paint over the paperdoll from the live body map.
  // Best-effort — a paint failure is logged + dropped (never blocks the UI
  // re-enable), matching the file's IPC-failure posture. Healthy body /
  // dormant schema → paintInjuryHeatmap's no-op path (renders nothing). The
  // paperdoll <img> must be loaded for the renderer to measure its box, so
  // renderPaperdoll() above (which sets the src) runs first. Cache the body
  // map so the gender toggle can re-paint without a fresh IPC.
  lastBodyMap = bodyMap;
  try {
    const section = drawerEl.querySelector('.hud-paperdoll-section');
    if (section) paintInjuryHeatmap(section, normGender(gender), bodyMap);
  } catch (e) {
    console.warn('[left-drawer] injury heatmap paint failed:', e);
  }
}

// ===========================================================================
// Drawer mechanics — UNCHANGED. stage.js drives these.
// ===========================================================================
export function openDrawer() {
  if (!drawerEl) return;
  isOpen = true;
  drawerEl.classList.add('open');
  drawerEl.setAttribute('aria-hidden', 'false');
}
export function closeDrawer() {
  if (!drawerEl) return;
  isOpen = false;
  drawerEl.classList.remove('open');
  drawerEl.setAttribute('aria-hidden', 'true');
  // Any open slide-up card collapses on drawer close (Chloe 2026-08-06: never
  // leave a card "active" behind a closed drawer — mouseleave/edge-lock/reset
  // all route through here, so this is the single chokepoint).
  closeInfoCard();
}
export function isOpenState() { return isOpen; }
export function isLocked() { return locked; }
export function toggleLock() {
  locked = !locked;
  if (drawerEl) drawerEl.classList.toggle('locked', locked);
  return locked;
}
export function setEdgeLockProbe(probe) {
  edgeLockVisible = typeof probe === 'function' ? probe : () => false;
}
export function onDrawerMouseLeave() {
  if (locked) return;
  if (edgeLockVisible()) return;
  closeDrawer();
}
// Hard reset (called from teardownStage on stage exit). Wipes HUD state too
// so a stale paperdoll from the prior session can't flash on re-entry.
export function resetLeftDrawer() {
  locked = false;
  closeInfoCard();          // collapse any open slide-up card + clear active dock
  activeCardDock = null;
  lastSnap = null;
  lastBodyMap = null;       // drop the cached injury map so it can't bleed into a new session
  hitboxDebugVisible = false; // drop the dev-overlay flag (the DOM clear below
                              // removes any painted overlay; this keeps the flag
                              // in sync so a later toggle won't re-paint stale)
  if (drawerEl) {
    drawerEl.classList.remove('locked');
    // Clear the injury heatmap overlay + tooltip so a stale injury glow from
    // the prior session can't flash on re-entry (mirrors the paperdoll reset).
    const section = drawerEl.querySelector('.hud-paperdoll-section');
    if (section) {
      try { clearInjuryHeatmap(section); }
      catch (e) { console.warn('[left-drawer] heatmap clear on reset failed:', e); }
      // DEV-ONLY: also clear the hitbox debug overlay if a developer left it
      // on screen before exiting the stage. No-op in normal use.
      const debugOverlay = section.querySelector('[data-body-parts-debug]');
      if (debugOverlay) debugOverlay.remove();
    }
  }
  closeDrawer();
}
