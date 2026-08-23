// =============================================================
// WIZARD ENGINE — the generic, config-driven slide-wizard core.
//
// EXTRACTED from player-creator.js (2026-08-05) so the same fullscreen
// scale, gold chevron arrows, step-by-step navigation, validation lock,
// + SIM-card review power ALL four creators (Player, NPC, World,
// Scenario). The Player Creator is now a thin config over this engine;
// NPC/World/Scenario are three more configs.
//
// THE CONTRACT (matches the pre-refactor player-creator.js exactly):
//   • fullscreen `.fable-player-wizard` container with the void glow +
//     rising embers (caller wires `_startAmbient`/`_stopAmbient`).
//   • a centered `.fable-wizard-slide-title` + glowing amber divider +
//     `.fable-wizard-stage` (the active slide's content) + the two
//     `.fable-wizard-arrow` medieval-arrow buttons (absolute-positioned
//     at the bottom).
//   • slide-to-slide forward is driven by the Next arrow; ‹ Back retreats.
//     A validation lock gates advance: each slide's `validate(state.fields)`
//     returns null (ok) or a reason string (the error). The error is
//     HIDDEN until the user actually presses Next (no nag on entry), then
//     revealed via the `.is-attempted` class.
//   • Enter advances (the clothing input owns its own Enter handler +
//     stops propagation).
//   • the final index past the slides array = the REVIEW slide: a SIM card
//     + a massive CREATE button + a ‹ Back arrow, bottom-anchored.
//
// RENDERER KINDS:
//   portrait, text, textarea, hair, gender, conditional, clothing,
//   codex-attach, review. Each renderer is (slide, stashed) → { html, wire }.
//   `wire(stage, onChange)` binds inputs that mutate `stashed.fields` +
//   calls `onChange` so the parent re-validates. New kinds can be added
//   by extending RENDERERS below.
//
// CONFIG SHAPE (passed to buildWizard):
//   {
//     screenId:       'player-creator' | 'npc-creator' | ...   (data-fable-screen)
//     screenClass:    'fable-player-creator-screen' | ...      (extra class)
//     slides:         [...]   (the slide list; see SLIDES in player-creator.js)
//     review: { title, render(stashed)→html, wire(stage, root, onCreate, back) }
//                              OR omit → engine builds a generic review from
//                              review.sections + a CREATE button (see below).
//     freshStashed:   () → initial stashed object (fields + portrait slots).
//     serialize:      (stashed) → the artifact to hand to onCreated.
//   }
// The engine owns: paint/revalidate/advance/navigation/Enter. The config
// owns: the slide list, the serializer, the review layout.
// =============================================================

import { createEmbers } from './embers.js';

// --- Shared validation caps (mirror Rust's player.rs / sim_card prose caps).
// Authored here so all four creators share the same numeric discipline.
export const NAME_MAX = 64;
export const TRAIT_MAX = 128;
export const PROSE_MAX = 4000;
const CONTROL_RE = /[\u0000-\u0008\u000B\u000C\u000E-\u001F]/;

export function traitInvalid(label, val) {
  const s = (val || '').trim();
  if (val && s === '') return `${label} cannot be empty.`;
  if (s.length > TRAIT_MAX) return `${label} must be ${TRAIT_MAX} characters or fewer.`;
  if (val && CONTROL_RE.test(val)) return `${label} contains invalid control characters.`;
  return null;
}

export function proseInvalid(label, val) {
  const s = (val || '').trim();
  if (s.length > PROSE_MAX) return `${label} must be ${PROSE_MAX} characters or fewer.`;
  if (val && CONTROL_RE.test(val)) return `${label} contains invalid control characters.`;
  return null;
}

// A text-field validation that requires non-empty + the trait caps. Used by
// the shared trait slides (Race/Age/Occupation/etc).
export function requiredTextValidate(label, key) {
  return (s) => {
    const v = (s[key] || '').trim();
    if (!v) return `${label} is required.`;
    return traitInvalid(label, s[key]);
  };
}

// An optional text-field validation: passes when empty, otherwise caps.
export function optionalTextValidate(label, key, cap) {
  const check = cap === 'prose' ? proseInvalid : traitInvalid;
  return (s) => {
    const v = (s[key] || '').trim();
    if (!v) return null;
    return check(label, s[key]);
  };
}

// Conditional-slide validator (Yes/No toggle + text). exported so NPC/Player
// share it.
export function conditionalValidate(label, val, enabled) {
  if (enabled === false) return null;          // No = clean decline
  if (enabled === undefined || enabled === null) return `Choose Yes or No.`;
  const v = (val || '').trim();
  if (!v) return `${label} is required (or choose No).`;
  return traitInvalid(label, val);
}

export function opt(v) { const s = (v || '').trim(); return s ? s : null; }

// (The unsuffixed `slugify` export was removed 2026-08-15: dead — every
// importer uses card-serialize.js's slugify, the single CREATE-path slug
// source that also appends Windows reserved-name suffixes.)

export function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// Normalize a gender value to the display form "Male"/"Female" (capitalized
// first letter). Accepts any casing. Defaults to "Male" for anything
// unrecognized. Mirrors player-creator.js's helper.
export function normalizeGender(g) {
  const v = String(g || '').trim().toLowerCase();
  return v === 'female' ? 'Female' : 'Male';
}

// Convert a Uint8Array to a standard base64 string (base64-over-JSON for
// the Tauri v2 IPC — a bare Vec<u8> arg poisons command registration).
export function bytesToBase64(bytes) {
  const CHUNK = 0x8000;
  let bin = '';
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode.apply(null, bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}

// Default person silhouette (inline SVG) for empty portrait slots.
export const SILHOUETTE_SVG = `<svg class="fable-portrait-silhouette" viewBox="0 0 120 160" aria-hidden="true" focusable="false">
  <path fill="currentColor" d="M60 16c-13 0-23 11-23 25 0 9 4 16 11 21-15 6-27 19-30 36-1 6 4 12 11 12h62c7 0 12-6 11-12-3-17-15-30-30-36 7-5 11-12 11-21 0-14-10-25-23-25z"/>
</svg>`;

// --- The medieval ARROW SVGs (verbatim from player-creator.js — the full
// barbed-arrowhead + shaft + swept fletching design). Reused by the nav
// arrows + the review's small back arrow.
export const ARROW_SVG_RIGHT = `<svg viewBox="0 0 100 64" aria-hidden="true" focusable="false">
  <path d="M92 32 L66 10 L78 32 L66 54 Z
           M12 28 L58 28 L72 32 L58 36 L12 36 Z
           M12 28 L8 19 L26 28 Z
           M12 36 L8 45 L26 36 Z"
        fill="currentColor" stroke="currentColor" stroke-width="2.5"
        stroke-linejoin="round" stroke-linecap="round"/>
</svg>`;
export const ARROW_SVG_LEFT = `<svg viewBox="0 0 100 64" aria-hidden="true" focusable="false">
  <path d="M8 32 L34 10 L22 32 L34 54 Z
           M88 28 L42 28 L28 32 L42 36 L88 36 Z
           M88 28 L92 19 L74 28 Z
           M88 36 L92 45 L74 36 Z"
        fill="currentColor" stroke="currentColor" stroke-width="2.5"
        stroke-linejoin="round" stroke-linecap="round"/>
</svg>`;

// The ID-CARD glyph — the details trigger on the compact license-card face
// (2026-08-14, Chloe: replaces the bronze arrow). A card outline carrying a
// portrait bust + text lines; reads as "the full card" at button size.
export const CARD_SVG = `<svg viewBox="0 0 64 64" aria-hidden="true" focusable="false">
  <rect x="10" y="6" width="44" height="52" rx="6" fill="none" stroke="currentColor" stroke-width="3.5"/>
  <circle cx="24" cy="24" r="5" fill="currentColor"/>
  <path d="M15 42c2-7 16-7 18 0z" fill="currentColor"/>
  <line x1="38" y1="20" x2="49" y2="20" stroke="currentColor" stroke-width="3.5" stroke-linecap="round"/>
  <line x1="38" y1="29" x2="49" y2="29" stroke="currentColor" stroke-width="3.5" stroke-linecap="round"/>
  <line x1="15" y1="49" x2="49" y2="49" stroke="currentColor" stroke-width="3.5" stroke-linecap="round"/>
</svg>`;

// Mars (♂) + Venus (♀) symbols as inline SVG paths — verbatim from
// player-creator.js. Sharp brass glyphs, no bubbly text glyphs.
export const MARS_SVG = `<svg class="fable-wizard-glyph-svg" viewBox="0 0 100 120" aria-hidden="true" focusable="false">
  <g class="glyph-stroke">
    <circle cx="36" cy="80" r="28"/>
    <line x1="56" y1="60" x2="94" y2="22"/>
    <polyline points="72,22 94,22 94,44"/>
  </g>
</svg>`;
export const VENUS_SVG = `<svg class="fable-wizard-glyph-svg" viewBox="0 0 100 120" aria-hidden="true" focusable="false">
  <g class="glyph-stroke">
    <circle cx="50" cy="38" r="28"/>
    <line x1="50" y1="66" x2="50" y2="110"/>
    <polyline points="30,92 50,110 70,92"/>
  </g>
</svg>`;

// ===========================================================================
// SLIDE RENDERERS — each returns { html, wire(stage, onChange, ctx) }.
// `ctx` carries { root, stashed, revalidate } so renderers can reach the
// wizard root (for _advance) + the shared state. All renderers mutate
// `stashed.fields` then call onChange (=== ctx.revalidate).
// ===========================================================================

export function renderTextSlide(slide, stashed) {
  const v = stashed.fields[slide.field] || '';
  return {
    html: `<label class="fable-wizard-field">
      <input type="text" data-slide-input="${slide.field}" value="${esc(v)}"
             placeholder="" autocomplete="off">
    </label>`,
    wire(stage, onChange) {
      const el = stage.querySelector(`[data-slide-input="${slide.field}"]`);
      el.addEventListener('input', () => {
        stashed.fields[slide.field] = el.value;
        onChange();
      });
    },
  };
}

// Textarea slide — for prose fields (Directive/Setting/Tone/Personality/
// Backstory/Intro). Reuses .fable-wizard-field so the brass border + emboss
// hold; the CSS sizes the textarea. `slide.rows` controls height (default 6).
export function renderTextareaSlide(slide, stashed) {
  const v = stashed.fields[slide.field] || '';
  const rows = slide.rows || 6;
  return {
    html: `<label class="fable-wizard-field fable-wizard-field--prose">
      <textarea data-slide-input="${slide.field}" rows="${rows}"
                placeholder="${esc(slide.placeholder || '')}">${esc(v)}</textarea>
    </label>`,
    wire(stage, onChange) {
      const el = stage.querySelector(`[data-slide-input="${slide.field}"]`);
      el.addEventListener('input', () => {
        stashed.fields[slide.field] = el.value;
        onChange();
      });
    },
  };
}

export function renderHairSlide(_slide, stashed) {
  const subs = [
    ['hair_color', 'Color'],
    ['hair_length', 'Length'],
    ['hair_style', 'Style'],
  ];
  const html = `<div class="fable-wizard-triple">${subs.map(([key, label]) => `
    <label class="fable-wizard-field fable-wizard-field--hair">
      <span class="fable-wizard-sublabel">${label}</span>
      <div class="fable-wizard-mini-divider" aria-hidden="true"></div>
      <input type="text" data-slide-input="${key}" value="${esc(stashed.fields[key] || '')}"
             placeholder="" autocomplete="off">
    </label>`).join('')}</div>`;
  return {
    html,
    wire(stage, onChange) {
      subs.forEach(([key]) => {
        const el = stage.querySelector(`[data-slide-input="${key}"]`);
        el.addEventListener('input', () => {
          stashed.fields[key] = el.value;
          onChange();
        });
      });
    },
  };
}

export function renderGenderSlide(_slide, stashed, _onChange, ctx) {
  const cur = normalizeGender(stashed.fields.gender).toLowerCase();
  return {
    html: `<div class="fable-wizard-toggle fable-wizard-toggle--gender" role="group" aria-label="Silhouette base">
      <button type="button" data-gender-pick="male" data-glyph="male" aria-pressed="${cur === 'male'}">${MARS_SVG}</button>
      <button type="button" data-gender-pick="female" data-glyph="female" aria-pressed="${cur === 'female'}">${VENUS_SVG}</button>
    </div>`,
    wire(stage, onChange) {
      stage.querySelectorAll('[data-gender-pick]').forEach((btn) => {
        btn.addEventListener('click', () => {
          const key = btn.dataset.genderPick;
          const display = normalizeGender(key);
          stashed.fields.gender = display;
          // Auto-seed the Breast toggle to match gender (female→Yes, male→No),
          // mirroring player-creator.js. Only meaningful when a breast slide
          // exists in this config (Player/NPC); harmless otherwise.
          stashed.fields.breast_size_enabled = (key === 'female');
          if (!stashed.fields.breast_size_enabled) {
            stashed.fields.breast_size = '';
          }
          // Persist to the paperdoll localStorage key (the Left Drawer reads
          // it). Best-effort; the drawer may not be loaded.
          try {
            localStorage.setItem('wupi.paperdoll.gender', key);
            if (ctx && ctx.onGenderPicked) ctx.onGenderPicked(key);
          } catch (_) { /* storage unavailable */ }
          stage.querySelectorAll('[data-gender-pick]').forEach((b) => {
            b.setAttribute('aria-pressed', String(b.dataset.genderPick === key));
          });
          onChange();
        });
      });
    },
  };
}

export function renderConditionalSlide(slide, stashed) {
  const enabledKey = `${slide.field}_enabled`;
  // Default to No on first entry (clean decline validates immediately).
  if (stashed.fields[enabledKey] === undefined || stashed.fields[enabledKey] === null) {
    stashed.fields[enabledKey] = false;
  }
  const isEnabled = stashed.fields[enabledKey] === true;
  const v = stashed.fields[slide.field] || '';
  return {
    html: `<div class="fable-wizard-conditional">
      <div class="fable-wizard-toggle fable-wizard-toggle--yesno" role="group" aria-label="${esc(slide.title)}">
        <button type="button" data-yesno="yes" aria-pressed="${isEnabled}">YES</button>
        <button type="button" data-yesno="no" aria-pressed="${!isEnabled}">NO</button>
      </div>
      <label class="fable-wizard-field fable-wizard-conditional-value${isEnabled ? '' : ' is-hidden'}">
        <input type="text" data-slide-input="${slide.field}" value="${esc(v)}"
               placeholder="" autocomplete="off">
      </label>
    </div>`,
    wire(stage, onChange) {
      stage.querySelectorAll('[data-yesno]').forEach((btn) => {
        btn.addEventListener('click', () => {
          const yes = btn.dataset.yesno === 'yes';
          stashed.fields[enabledKey] = yes;
          stage.querySelectorAll('[data-yesno]').forEach((b) => {
            b.setAttribute('aria-pressed', String(b.dataset.yesno === (yes ? 'yes' : 'no')));
          });
          const valWrap = stage.querySelector('.fable-wizard-conditional-value');
          if (valWrap) valWrap.classList.toggle('is-hidden', !yes);
          if (!yes) stashed.fields[slide.field] = '';
          onChange();
        });
      });
      const input = stage.querySelector(`[data-slide-input="${slide.field}"]`);
      if (input) {
        input.addEventListener('input', () => {
          stashed.fields[slide.field] = input.value;
          onChange();
        });
      }
    },
  };
}

export function renderClothingSlide(_slide, stashed) {
  if (!Array.isArray(stashed.fields.clothing)) stashed.fields.clothing = [];
  const chipsHTML = stashed.fields.clothing
    .map((c, i) => `<span class="fable-wizard-chip">${esc(c)}<button type="button" data-chip-remove="${i}" aria-label="Remove">×</button></span>`)
    .join('');
  return {
    html: `<div class="fable-wizard-clothing">
      <input type="text" class="fable-wizard-chip-input" data-clothing-input
             placeholder="" autocomplete="off">
      <div class="fable-wizard-chips" data-chip-host>${chipsHTML}</div>
    </div>`,
    wire(stage, onChange) {
      const input = stage.querySelector('[data-clothing-input]');
      const host = stage.querySelector('[data-chip-host]');
      function rerender() {
        host.innerHTML = stashed.fields.clothing
          .map((c, i) => `<span class="fable-wizard-chip">${esc(c)}<button type="button" data-chip-remove="${i}" aria-label="Remove">×</button></span>`)
          .join('');
        bindRemoves();
      }
      function bindRemoves() {
        host.querySelectorAll('[data-chip-remove]').forEach((btn) => {
          btn.addEventListener('click', () => {
            const i = Number(btn.dataset.chipRemove);
            stashed.fields.clothing.splice(i, 1);
            rerender();
            onChange();
          });
        });
      }
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          e.stopPropagation();
          const v = input.value.trim();
          if (v && v.length <= TRAIT_MAX && !CONTROL_RE.test(v)) {
            stashed.fields.clothing.push(v);
            input.value = '';
            rerender();
            onChange();
          } else {
            // Empty/invalid → advance (validation surfaces the empty-list error
            // if there are no chips yet).
            const screenEl = stage.closest('[data-fable-screen]');
            if (screenEl && typeof screenEl._advance === 'function') {
              screenEl._advance();
            }
          }
        }
      });
      bindRemoves();
    },
  };
}

// Portrait slide — click the slot to open a file picker. `ctx.onPickPortrait`
// (wired by the config) owns the actual pick+crop flow (the Player/NPC flows
// route through the cropper; the World/Scenario cover-image flows may skip
// the crop). Falls back to a no-op if the config doesn't supply a picker
// (defensive — a portrait slide without a picker is read-only preview).
//
// SILLYTAVERN IMPORT (2026-08-05): when `ctx.onImport` is set, a large
// secondary "Import Character/World (PNG/JSON)" button renders BELOW the
// portrait slot. Clicking it calls `ctx.onImport(screenEl)` which opens the
// file picker + parses + auto-fills the wizard's fields (the wizard is NOT
// skipped — the user clicks through to review/edit). The button only renders
// on the portrait slide (slide 1) so it's a one-time entry-point affordance.
export function renderPortraitSlide(_slide, stashed, _onChange, ctx) {
  const previewSrc = stashed.portraitPreviewSrc;
  const inner = previewSrc
    ? `<img src="${esc(previewSrc)}" alt="Portrait preview">`
    : SILHOUETTE_SVG;
  const importBtn = (ctx && typeof ctx.onImport === 'function')
    ? `<button type="button" class="fable-wizard-import-btn" data-import-pick>IMPORT</button>`
    : '';
  return {
    html: `<div class="fable-wizard-portrait" data-portrait-slot>${inner}</div>${importBtn}`,
    wire(stage, onChange) {
      const slot = stage.querySelector('[data-portrait-slot]');
      slot.addEventListener('click', async () => {
        const screenEl = stage.closest('[data-fable-screen]');
        if (ctx && typeof ctx.onPickPortrait === 'function') {
          await ctx.onPickPortrait(screenEl, stashed, () => {
            const src = stashed.portraitPreviewSrc;
            slot.innerHTML = src ? `<img src="${esc(src)}" alt="Portrait preview">` : SILHOUETTE_SVG;
            onChange();
          });
        }
      });
      const importBtnEl = stage.querySelector('[data-import-pick]');
      if (importBtnEl && ctx && typeof ctx.onImport === 'function') {
        importBtnEl.addEventListener('click', async () => {
          const screenEl = stage.closest('[data-fable-screen]');
          await ctx.onImport(screenEl);
          // After the import auto-fills, refresh the portrait slot.
          const src = stashed.portraitPreviewSrc;
          slot.innerHTML = src ? `<img src="${esc(src)}" alt="Portrait preview">` : SILHOUETTE_SVG;
          onChange();
        });
      }
    },
  };
}

// Codex-attach slide — a large "Attach Codex (JSON lorebook)" button that
// opens a .json picker. The parsed lorebook entries live on
// `stashed.fields.codex_entries` (an array of {title, tags, body}). A chip
// list shows attached entries; each is removable. The serializer converts
// these to the compound `.codex` format + writes into the universal library
// (+ auto-links) at CREATE time. `ctx.onAttachCodex` owns the pick+parse
// (so the ST-import path can also populate this).
export function renderCodexAttachSlide(_slide, stashed, _onChange, ctx) {
  if (!Array.isArray(stashed.fields.codex_entries)) stashed.fields.codex_entries = [];
  const chipsHTML = stashed.fields.codex_entries
    .map((e, i) => `<span class="fable-wizard-chip">${esc(e.title || 'untitled')}<button type="button" data-codex-remove="${i}" aria-label="Remove">×</button></span>`)
    .join('');
  return {
    html: `<div class="fable-wizard-codex-attach">
      <button type="button" class="fable-wizard-codex-btn" data-codex-pick>Attach Codex (JSON lorebook)</button>
      <div class="fable-wizard-chips" data-codex-host>${chipsHTML || '<span class="fable-wizard-codex-empty">No lorebook attached</span>'}</div>
    </div>`,
    wire(stage, onChange) {
      const btn = stage.querySelector('[data-codex-pick]');
      const host = stage.querySelector('[data-codex-host]');
      function rerender() {
        host.innerHTML = stashed.fields.codex_entries.length
          ? stashed.fields.codex_entries
              .map((e, i) => `<span class="fable-wizard-chip">${esc(e.title || 'untitled')}<button type="button" data-codex-remove="${i}" aria-label="Remove">×</button></span>`)
              .join('')
          : '<span class="fable-wizard-codex-empty">No lorebook attached</span>';
        bindRemoves();
      }
      function bindRemoves() {
        host.querySelectorAll('[data-codex-remove]').forEach((b) => {
          b.addEventListener('click', () => {
            const i = Number(b.dataset.codexRemove);
            stashed.fields.codex_entries.splice(i, 1);
            rerender();
            onChange();
          });
        });
      }
      btn.addEventListener('click', async () => {
        const screenEl = stage.closest('[data-fable-screen]');
        if (ctx && typeof ctx.onAttachCodex === 'function') {
          await ctx.onAttachCodex(screenEl, stashed);
          rerender();
          onChange();
        }
      });
      bindRemoves();
    },
  };
}

// The renderer registry. Configs may add kinds by mutating this object
// before buildWizard (rare; the built-in set covers all four flows).
export const RENDERERS = {
  portrait: renderPortraitSlide,
  text: renderTextSlide,
  textarea: renderTextareaSlide,
  hair: renderHairSlide,
  gender: renderGenderSlide,
  conditional: renderConditionalSlide,
  clothing: renderClothingSlide,
  'codex-attach': renderCodexAttachSlide,
};

// ===========================================================================
// BUILD WIZARD — the generic factory.
// ===========================================================================

export function buildWizard(config) {
  const {
    screenId,
    screenClass,
    slides,
    freshStashed,
    review,
    ctx = {},
  } = config;

  const root = document.createElement('section');
  root.className = `fable-screen ${screenClass || ''}`.trim();
  root.dataset.fableScreen = screenId;
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-glow" aria-hidden="true"></div>
    <div class="fable-ember-host" aria-hidden="true"></div>
    <div class="fable-player-wizard">
      <h2 class="fable-wizard-slide-title" data-slide-title></h2>
      <div class="fable-wizard-slide-divider" data-slide-divider aria-hidden="true"></div>
      <div class="fable-wizard-stage" data-stage></div>
      <div class="fable-wizard-nav" data-nav>
        <button class="fable-wizard-arrow fable-wizard-arrow--left" type="button" data-act="back" aria-label="Previous slide">${ARROW_SVG_LEFT}</button>
        <button class="fable-wizard-arrow fable-wizard-arrow--right" type="button" data-act="next" aria-label="Next slide">${ARROW_SVG_RIGHT}</button>
      </div>
      <div class="fable-player-status" data-status></div>
    </div>
    <div class="fable-creator-toast" data-player-toast hidden></div>
  `;

  root._stashed = freshStashed ? freshStashed() : { fields: {} };
  root._currentIndex = 0;
  root._isReview = false;
  root._attemptedNext = false;
  root._config = config;
  root._ctx = ctx;

  const stage = root.querySelector('[data-stage]');
  const titleEl = root.querySelector('[data-slide-title]');
  const navEl = root.querySelector('[data-nav]');
  const backBtn = root.querySelector('[data-act="back"]');
  const nextBtn = root.querySelector('[data-act="next"]');
  const statusEl = root.querySelector('[data-status]');
  const wizardEl = root.querySelector('.fable-player-wizard');

  // Toast helper (mirrors player-creator.js).
  let toastTimer = null;
  function toast(msg) {
    const host = root.querySelector('[data-player-toast]');
    if (!host) return;
    host.textContent = msg;
    host.hidden = false;
    if (toastTimer) clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { host.hidden = true; }, 4000);
  }
  root._toast = toast;

  function currentSlide() {
    return slides[root._currentIndex];
  }

  function paintSlide() {
    const slide = currentSlide();
    titleEl.textContent = slide.title;
    titleEl.classList.remove('is-review-title');
    root._isReview = false;
    navEl.classList.remove('is-hidden');
    if (wizardEl) wizardEl.classList.remove('is-review-mode');
    // The ‹ Back arrow is HIDDEN on slide 0 (it would be non-functional chrome
    // — there's nothing before the first slide). It reappears the moment the
    // user advances to slide ≥1. The › Next arrow stays visible on every slide
    // (it's always meaningful until the review, which owns its own CREATE/back).
    // 2026-08-05: the prior code left the back arrow visible-but-useless on
    // slide 0 across all four creators.
    if (root._currentIndex === 0) {
      backBtn.classList.add('is-hidden');
    } else {
      backBtn.classList.remove('is-hidden');
    }
    root._attemptedNext = false;
    statusEl.classList.remove('is-attempted');
    const renderer = RENDERERS[slide.kind];
    if (!renderer) {
      console.error(`[wizard] no renderer for kind "${slide.kind}" (slide ${slide.id})`);
      return;
    }
    const rendered = renderer(slide, root._stashed, revalidate, root._ctx);
    stage.innerHTML = rendered.html;
    rendered.wire(stage, revalidate, root._ctx);
    revalidate();
  }

  function paintReview() {
    titleEl.textContent = (review && review.title) || 'Review';
    titleEl.classList.add('is-review-title');
    root._isReview = true;
    navEl.classList.add('is-hidden');
    if (wizardEl) wizardEl.classList.add('is-review-mode');
    statusEl.textContent = '';
    statusEl.classList.remove('is-valid');
    // The review renders its own HTML (the SIM card + CREATE + back).
    stage.innerHTML = review.render(root._stashed, root._ctx);
    if (review.wire) review.wire(stage, root, onCreate, () => {
      // Back from review → last slide.
      root._isReview = false;
      root._currentIndex = slides.length - 1;
      paintSlide();
    });
  }

  function revalidate() {
    if (root._isReview) {
      const createBtn = stage.querySelector('[data-review-create]');
      if (!createBtn) return;
      const err = firstErrorAcrossSlides();
      createBtn.disabled = !!err;
      return;
    }
    const slide = currentSlide();
    if (!slide.validate) {
      statusEl.textContent = '';
      statusEl.classList.remove('is-attempted', 'is-valid');
      return;
    }
    const err = slide.validate(root._stashed.fields);
    if (err) {
      statusEl.textContent = err;
      statusEl.classList.toggle('is-attempted', !!root._attemptedNext);
      statusEl.classList.remove('is-valid');
    } else {
      statusEl.textContent = '';
      statusEl.classList.remove('is-attempted', 'is-valid');
    }
  }

  function firstErrorAcrossSlides() {
    for (const slide of slides) {
      if (!slide.validate) continue;
      const err = slide.validate(root._stashed.fields);
      if (err) return err;
    }
    return null;
  }

  function paint() {
    if (root._currentIndex >= slides.length) {
      paintReview();
    } else {
      paintSlide();
    }
  }

  function advance() {
    const slide = currentSlide();
    if (slide && slide.validate) {
      const err = slide.validate(root._stashed.fields);
      if (err) {
        root._attemptedNext = true;
        statusEl.textContent = err;
        statusEl.classList.add('is-attempted');
        return false;
      }
    }
    root._currentIndex += 1;
    paint();
    return true;
  }

  nextBtn.addEventListener('click', advance);
  backBtn.addEventListener('click', () => {
    if (root._isReview) return;
    if (root._currentIndex > 0) {
      root._currentIndex -= 1;
      paintSlide();
    }
  });

  // Enter advances (the clothing input owns its Enter + stops propagation).
  root.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter') return;
    if (root._isReview) return;
    if (e.target && e.target.dataset && e.target.dataset.clothingInput !== undefined) return;
    // Don't fire Enter-advance from a textarea (Enter = newline in prose).
    if (e.target && e.target.tagName === 'TEXTAREA') return;
    e.preventDefault();
    advance();
  });

  root._paint = paint;
  root._revalidate = revalidate;
  root._advance = advance;

  // Ambient embers (mirrors the per-screen wiring).
  const emberHost = root.querySelector('.fable-ember-host');
  let embers = null;
  root._startAmbient = () => { if (!embers) embers = createEmbers(emberHost); };
  root._stopAmbient = () => { if (embers) { embers.destroy(); embers = null; } };

  return root;
}

// ===========================================================================
// RENDER WIZARD — reset to a fresh state (or seed from `handlers.editFrom`)
// + paint slide 0. The config's `freshStashed` + `seedFrom` own the state.
// ===========================================================================

export function renderWizard(root, handlers = {}) {
  const config = root._config;
  const stashed = config.freshStashed ? config.freshStashed() : { fields: {} };
  if (config.seedFrom && handlers.editFrom) {
    config.seedFrom(stashed, handlers.editFrom);
  }
  root._stashed = stashed;
  root._currentIndex = 0;
  root._isReview = false;
  root._attemptedNext = false;
  const nextBtn = root.querySelector('[data-act="next"]');
  if (nextBtn) nextBtn.disabled = false;
  const stage = root.querySelector('[data-stage]');
  if (stage) stage.innerHTML = '';
  if (root._paint) root._paint();
  root._handlers = handlers;
}

// The CREATE path. Runs the config's `serialize(stashed)` → artifact, then
// calls `config.onCreated(root, artifact, stashed)` which owns the IPC write
// + the handoff. The CREATE button lives inside the review card; this fn
// disables + relabels it during the write.
export async function runCreate(root) {
  const config = root._config;
  const stashed = root._stashed;
  const createBtn = root.querySelector('[data-review-create]')
    || root.querySelector('[data-act="next"]');
  const origLabel = createBtn ? createBtn.textContent : '';
  if (createBtn) { createBtn.disabled = true; createBtn.textContent = 'Creating…'; }
  try {
    await config.onCreated(root, stashed);
  } catch (err) {
    if (root._toast) root._toast(String(err));
    if (createBtn) { createBtn.disabled = false; createBtn.textContent = origLabel || 'CREATE'; }
  }
}

// Exposed for the review's CREATE button wiring (configs call this).
export function onCreate(root) {
  return runCreate(root);
}

// --- Generic review builder ------------------------------------------------
// Configs that don't need a bespoke review can use this helper to build a
// `review` config from a sections spec. `sections` is either a static array
// OR a function (stashed) → array (so rows can depend on which conditional
// toggles were set, which fields are non-empty, etc.). Each section:
//   { title, rows: [[label, value], ...] }
// where value is a string, an array (rendered as chips), or empty (omitted).
// Portrait + CREATE + back arrow are always rendered.
export function buildGenericReview({ sections, showPortrait = true }) {
  return {
    title: 'Review',
    render(stashed, ctx) {
      const secs = typeof sections === 'function' ? sections(stashed) : sections;
      const sectionHTML = secs.map((sec) => {
        const rows = (sec.rows || []).filter(([, v]) => {
          if (Array.isArray(v)) return v.length > 0;
          return (v || '').toString().trim().length > 0;
        });
        if (!rows.length) return '';
        const pair = ([k, v]) => {
          if (Array.isArray(v)) {
            const chips = v.map((c) => `<span class="fable-wizard-chip">${esc(c)}</span>`).join('');
            return `<div><dt>${esc(k)}</dt><dd><div class="fable-player-review-chips">${chips}</div></dd></div>`;
          }
          return `<div><dt>${esc(k)}</dt><dd>${esc(v)}</dd></div>`;
        };
        return `<section class="fable-player-review-section"><h3>${esc(sec.title)}</h3><dl>${rows.map(pair).join('')}</dl></section>`;
      }).join('');

      const portraitHTML = showPortrait && stashed.portraitPreviewSrc
        ? `<img src="${esc(stashed.portraitPreviewSrc)}" alt="" onerror="this.style.display='none'">`
        : (showPortrait ? `<span class="fable-player-review-portrait-fallback" aria-hidden="true">${SILHOUETTE_SVG}</span>` : '');

      const topHTML = showPortrait
        ? `<div class="fable-player-review-top">
            <div class="fable-player-review-portrait">${portraitHTML}</div>
            <div class="fable-player-review-body">${sectionHTML}</div>
          </div>`
        : `<div class="fable-player-review-body">${sectionHTML}</div>`;

      return `<div class="fable-player-review-card">${topHTML}</div>
        <div class="fable-player-review-create-wrap">
          <button type="button" class="fable-player-review-create" data-review-create>CREATE</button>
          <button type="button" class="fable-player-review-back" data-review-back aria-label="Back">${ARROW_SVG_LEFT}</button>
        </div>`;
    },
    wire(stage, root, onCreateFn, back) {
      const createBtn = stage.querySelector('[data-review-create]');
      const reviewBack = stage.querySelector('[data-review-back]');
      if (createBtn) {
        createBtn.addEventListener('click', () => {
          if (createBtn.disabled) return;
          onCreateFn(root);
        });
      }
      if (reviewBack) reviewBack.addEventListener('click', back);
    },
  };
}
