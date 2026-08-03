// =============================================================
// FABLE LEFT DRAWER — Visual HUD
//   §1  Paperdoll  — a static silhouette PNG (no injury overlay). The per-
//                     body-part hitbox + injury-coloring system was removed
//                     to be redone later; the figure is now a plain visual.
//                     Gender toggle (♂/♀) still swaps the base PNG.
//   §2  Ambient + Chronos & Climate — time-of-day background tint + weather
//                     overlay AND a visible gold Time/Weather panel, all
//                     driven by world_clock + weather.condition from
//                     fable_schema_get.
//
// The INVENTORY section (equip slots, drag-and-drop grid, context menu, and
// the consume/equip/unequip/drop pipelines) was REMOVED on 2026-08-02 to
// clean up the drawer for the two-column grid. The Rust schema-tracker
// machinery it spoke to (pending_player_action slot, fable_player_action_set
// IPC, <player_action> narrator render) is INTENTIONALLY left intact —
// dormant scaffolding for a future surface.
//
// Layout (2026-08-02): .hud-master-grid is a two-column CSS grid. The LEFT
// column holds the paperdoll (kept near its original size); the RIGHT column
// holds the Chronos & Climate panel at its top. The drawer mechanics below
// are UNCHANGED — stage.js's hover-strip + edge-lock wiring drives this
// drawer identically to the right Wupi drawer.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import MALE_URL from '../assets/paperdoll_male.png';
import FEMALE_URL from '../assets/paperdoll_female.png';

// ─── Drawer mechanics (UNCHANGED from the 2026-08-01 empty shell) ─────────
let drawerEl = null;
let isOpen = false;
let locked = false;
let edgeLockVisible = () => false;

// ─── HUD state ────────────────────────────────────────────────────────────
let gender = localStorage.getItem('wupi.paperdoll.gender') || 'male'; // 'male' | 'female'

// ===========================================================================
// §1  PAPERDOLL — static silhouette (body-part injury overlay REMOVED)
// ===========================================================================
// The figure is a plain PNG with a gender toggle. The per-region SVG hitbox
// overlay, injury-tier coloring, and hover tooltips (head/neck/stomach/etc.)
// were removed on 2026-08-02 to be redesigned later. The Rust body-part
// types (PlayerState.body, BodyPart, BodyPartState) remain as dormant
// scaffolding — this file no longer reads player_state_get for the paperdoll.
// renderPaperdoll() now just swaps the base image src on gender toggle.

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
// ESCAPE — shared with tab-rail.js convention (defensive, never trust raw
// schema strings as HTML).
// ===========================================================================
function esc(s) {
  return String(s ?? '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

// ===========================================================================
// BUILD — populate the drawer shell. Called once from stage.js buildStage.
// ===========================================================================
export function buildLeftDrawer() {
  drawerEl = document.createElement('aside');
  drawerEl.className = 'fable-left-drawer';
  drawerEl.dataset.leftDrawer = '';
  drawerEl.setAttribute('aria-hidden', 'true');
  drawerEl.dataset.gender = gender; // drives ambient + paperdoll theming
  drawerEl.innerHTML = `
    <div class="hud-ambient-layer" data-ambient aria-hidden="true"></div>
    <div class="hud-gender-toggle" role="group" aria-label="Silhouette base">
      <button type="button" data-gender-btn="male" aria-pressed="${gender === 'male'}" title="Male silhouette">♂</button>
      <button type="button" data-gender-btn="female" aria-pressed="${gender === 'female'}" title="Female silhouette">♀</button>
    </div>
    <div class="hud-master-grid">
      <div class="hud-col hud-col--left">
        <section class="hud-paperdoll-section" aria-label="Character condition">
          <div class="hud-paperdoll-wrap" data-paperdoll>
            <img class="hud-paperdoll-base" data-paperdoll-img alt="" aria-hidden="true">
          </div>
        </section>
      </div>
      <div class="hud-col hud-col--right">
        <div class="hud-environment-panel" aria-label="Time and weather">
          <div class="hud-env-label">Chronos</div>
          <div class="hud-env-time" data-env-time>—</div>
          <div class="hud-env-label">Climate</div>
          <div class="hud-env-weather" data-env-weather>—</div>
        </div>
      </div>
    </div>
  `;
  wireInteractions(drawerEl);
  return drawerEl;
}

// ===========================================================================
// WIRE — bind all interactions once (the drawer element is reused across
// stage entries, so bind in buildLeftDrawer, not on every refresh).
// ===========================================================================
function wireInteractions(root) {
  // ── Gender toggle ────────────────────────────────────────────────────
  root.querySelectorAll('[data-gender-btn]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const g = btn.dataset.genderBtn;
      if (g === gender) return;
      playUIClink();
      gender = g;
      localStorage.setItem('wupi.paperdoll.gender', g);
      drawerEl.dataset.gender = g;
      root.querySelectorAll('[data-gender-btn]').forEach((b) => {
        b.setAttribute('aria-pressed', String(b.dataset.genderBtn === g));
      });
      renderPaperdoll(); // swap base PNG + hitbox coordinate set
    });
  });
}

// ===========================================================================
// RENDERERS — each is async + idempotent (safe to call repeatedly).
// ===========================================================================

// §1 — paperdoll. Swaps the base silhouette PNG for the active gender.
// (The injury hitbox overlay was removed; this is now a plain static image.)
function renderPaperdoll() {
  if (!drawerEl) return;
  const img = drawerEl.querySelector('[data-paperdoll-img]');
  if (img) img.src = gender === 'female' ? FEMALE_URL : MALE_URL;
}

// §2 — ambient + Chronos & Climate panel. Reads the live clock + weather:
//   • sets drawer data attributes (data-time, data-weather) the CSS keys off
//     for the background tint + weather animation layer (unchanged), AND
//   • writes a human-readable Time + Weather into the env panel.
// If no active game, leaves the panel at its em-dash dormant default.
async function renderAmbient() {
  if (!drawerEl) return;
  let schema;
  try {
    schema = await invoke('fable_schema_get');
  } catch (err) {
    return; // no active game — leave ambient at default + panel dormant
  }
  const minutes = (schema.world_clock && schema.world_clock.current_minutes) || 0;
  const hasClock = minutes > 0;   // 0 = dormant (no [TIME] emitted yet)
  const intoDay = (minutes % 1440 + 1440) % 1440; // minutes since midnight
  const hour = hasClock ? Math.floor(intoDay / 60) : 10;   // default 10:00 when dormant
  const minute = hasClock ? (intoDay % 60) : 0;
  // 22:00–05:00 night · 05:00–08:00 & 17:00–20:00 twilight · else day.
  // When dormant, force 'day' so the drawer tints warm + the panel reads Noon.
  let timeOfDay = 'day';
  if (hasClock) {
    if (hour >= 22 || hour < 5) timeOfDay = 'night';
    else if (hour < 8 || hour >= 17) timeOfDay = 'twilight';
  }
  drawerEl.dataset.time = timeOfDay;

  // Time panel: HH:MM (wall-clock) + a crisp period word. current_minutes is
  // epoch minutes since 0001-01-01; with no canonical "day 1" anchor in the
  // JS-visible schema, we show wall-clock + period rather than a "Day N".
  // When the clock is dormant we show a sunny "10:00 · Day" DEFAULT so the
  // panel is populated for review on a fresh game; it's overwritten the
  // moment the first [TIME] lands.
  const clockStr = `${String(hour).padStart(2, '0')}:${String(minute).padStart(2, '0')}`;
  const periodWord = timeOfDay === 'night' ? 'Night'
    : timeOfDay === 'twilight' ? 'Twilight' : 'Day';
  const timeEl = drawerEl.querySelector('[data-env-time]');
  if (timeEl) {
    timeEl.textContent = `${clockStr}  ·  ${periodWord}`;
  }

  // Weather: classify for the background attr (rain/snow/fog/clear — reuses
  // the fx/atmosphere category set), then render a readable condition line.
  // When dormant, default to Sunny so the panel + the clear-sky tint show.
  const rawCondition = hasClock
    ? String((schema.weather && schema.weather.condition) || '')
    : 'Sunny';
  const condition = rawCondition.toLowerCase();
  let weather = 'clear';
  if (/rain|drizzle|downpour|storm|thunder/.test(condition)) weather = 'rain';
  else if (/snow|hail|blizzard|frost/.test(condition)) weather = 'snow';
  else if (/fog|mist|haze|overcast/.test(condition)) weather = 'fog';
  drawerEl.dataset.weather = weather;

  const weatherEl = drawerEl.querySelector('[data-env-weather]');
  if (weatherEl) {
    const glyph = weather === 'rain' ? '⛈'
      : weather === 'snow' ? '❄'
      : weather === 'fog' ? '🌫'
      : '☀';
    const label = titleCase(rawCondition || 'Sunny');
    weatherEl.innerHTML = `<span class="hud-env-glyph" aria-hidden="true">${glyph}</span> ${esc(label)}`;
  }
}

// Title Case a weather condition for display ("heavy rain" → "Heavy Rain").
function titleCase(s) {
  return s.replace(/\b\w/g, (c) => c.toUpperCase());
}

// ===========================================================================
// PUBLIC — re-render everything from live IPC data.
// Called by stage.js on drawer-open + after each narrator turn.
// ===========================================================================
export async function refreshAll() {
  if (!drawerEl) return;
  // The paperdoll no longer reads player_state_get (body-part overlay removed).
  // Only the ambient/env-panel needs live schema data now.
  renderPaperdoll();
  await renderAmbient();
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
// so a stale paperdoll/panel from the prior session can't flash on re-entry.
export function resetLeftDrawer() {
  locked = false;
  if (drawerEl) {
    drawerEl.classList.remove('locked');
    delete drawerEl.dataset.time;
    delete drawerEl.dataset.weather;
  }
  closeDrawer();
}
