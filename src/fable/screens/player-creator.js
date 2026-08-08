// =============================================================
// SCREEN: PLAYER CREATOR — a 15-slide wizard authoring a reusable player.
//
// REFACTORED 2026-08-05: the slide engine, renderers, arrows, validation
// lock, + review control flow now live in the generic wizard-engine.js
// (shared with the NPC/World/Scenario creators). This file is the PLAYER-
// specific config: the 15-slide list, the SavedPlayer serializer, the
// portrait-crop ctx, + the bespoke review (player's review predates the
// generic one + has the clothing-as-its-own-entity layout — kept as-is so
// the player review's exact look is unchanged).
//
// THE 15 SLIDES (order is load-bearing — matches the Rust trait set):
//   1  Portrait        (optional — upload via @tauri-apps/plugin-dialog)
//   2  Name            (text, required, the identity anchor + slug source)
//   3  Gender          (♂/♀ toggle — persists to localStorage + JSON)
//   4-11 traits        (Race/Age/Height/Weight/Hair×3/Body/Skin/Eyes)
//   12-14 conditional  (Breast/Ears/Tail — Yes/No, No omits from JSON)
//   15 Clothing        (dynamic chip list)
//   16 [REVIEW]        (SIM card + CREATE button)
//
// The Player Creator is NOT a sim card — it authors a SavedPlayer (player.rs)
// via fable_player_write + fable_player_portrait_upload_bytes. The three new
// creators (NPC/World/Scenario) author sim cards via fable_write_card. This
// file keeps the SavedPlayer serializer + the player-portrait upload path.
//
// SillyTavern import (2026-08-05): the import button on slide 1 auto-fills
// the wizard's fields from a parsed ST character (the player schema maps
// only name → Name; the rest is ignored — players don't have ST-style prose).
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import {
  buildWizard, renderWizard, runCreate, buildGenericReview,
  requiredTextValidate, conditionalValidate, traitInvalid,
  normalizeGender, slugify, bytesToBase64, esc,
  NAME_MAX, TRAIT_MAX, SILHOUETTE_SVG, ARROW_SVG_LEFT,
} from './wizard-engine.js';
import { openPortraitCropper } from './portrait-cropper.js';
import { setPaperdollGender } from '../engine/left-drawer.js';
import { openImportDialog } from './st-import.js';

const SCHEMA_KEY = 'player';

// --- The 15-slide list (mirrors the pre-refactor player-creator.js exactly).
function buildSlides() {
  return [
    { id: 'portrait', title: 'Portrait', kind: 'portrait' },
    {
      id: 'name', title: 'Name', kind: 'text', field: 'name', required: true,
      validate: (s) => {
        const v = (s.name || '').trim();
        if (!v) return 'Name is required.';
        if (v.length > NAME_MAX) return `Name must be ${NAME_MAX} characters or fewer.`;
        return null;
      },
    },
    {
      id: 'gender', title: 'Gender', kind: 'gender',
      validate: (s) => {
        const g = (s.gender || '').trim().toLowerCase();
        if (g !== 'male' && g !== 'female') return 'Choose a silhouette.';
        return null;
      },
    },
    traitSlide('race', 'Race'),
    traitSlide('age', 'Age'),
    traitSlide('height', 'Height'),
    traitSlide('weight', 'Weight'),
    {
      id: 'hair', title: 'Hair', kind: 'hair',
      validate: (s) => {
        for (const key of ['hair_color', 'hair_length', 'hair_style']) {
          const v = (s[key] || '').trim();
          if (!v) return 'All three hair fields are required.';
          const err = traitInvalid('Hair', s[key]);
          if (err) return err;
        }
        return null;
      },
    },
    traitSlide('body_type', 'Body'),
    traitSlide('skin_complexion', 'Skin'),
    traitSlide('eye_color', 'Eyes'),
    { id: 'breast_size', title: 'Breast', kind: 'conditional', field: 'breast_size',
      validate: (s) => conditionalValidate('Breast', s.breast_size, s.breast_size_enabled) },
    { id: 'ears', title: 'Ears', kind: 'conditional', field: 'ears',
      validate: (s) => conditionalValidate('Ears', s.ears, s.ears_enabled) },
    { id: 'tail', title: 'Tail', kind: 'conditional', field: 'tail',
      validate: (s) => conditionalValidate('Tail', s.tail, s.tail_enabled) },
    { id: 'clothing', title: 'Clothing', kind: 'clothing',
      validate: (s) => {
        const list = Array.isArray(s.clothing) ? s.clothing : [];
        if (list.length === 0) return 'Add at least one garment.';
        return null;
      },
    },
  ];
}

function traitSlide(id, title) {
  return {
    id, title, kind: 'text', field: id,
    validate: requiredTextValidate(title, id),
  };
}

// --- The player-specific portrait pick ctx: routes through the cropper +
// persists the gender to the paperdoll (mirrors the pre-refactor flow).
async function pickPlayerPortrait(screenEl, stashed, onChange) {
  try {
    const picked = await openDialog({
      multiple: false,
      filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg'] }],
    });
    if (!picked) return;
    const srcPath = typeof picked === 'string' ? picked : (picked.path || picked);
    if (!srcPath) return;
    const pickedSrc = await invoke('fable_player_portrait_read_bytes', { srcPath });
    if (!pickedSrc) return;
    const cropped = await openPortraitCropper(screenEl, pickedSrc);
    if (!cropped) return;
    stashed.portraitSrcPath = srcPath;
    stashed.portraitCroppedBytes = cropped.bytes;
    stashed.portraitCroppedExt = cropped.ext;
    stashed.portraitPreviewSrc = cropped.dataUrl;
    if (onChange) onChange();
  } catch (err) {
    const toast = screenEl && screenEl._toast;
    if (toast) toast(String(err));
  }
}

function buildCtx() {
  return {
    onPickPortrait: pickPlayerPortrait,
    onGenderPicked: (key) => {
      // Persist to the paperdoll localStorage + the live drawer (mirrors the
      // pre-refactor player-creator gender slide).
      try { setPaperdollGender(key); } catch (_) { /* drawer not yet loaded */ }
    },
    onImport: (screenEl) => importIntoPlayerCreator(screenEl),
    schemaKey: SCHEMA_KEY,
  };
}

// --- freshStashed: the initial wizard state (mirrors the pre-refactor fn).
function freshStashed() {
  return {
    fields: {
      gender: normalizeGender(localStorage.getItem('wupi.paperdoll.gender') || 'male'),
      clothing: [],
    },
    portraitSrcPath: null,
    portraitCroppedBytes: null,
    portraitCroppedExt: null,
    portraitPreviewSrc: null,
  };
}

// --- Seed the wizard from a loaded SavedPlayer (the EDIT path).
function seedFromPlayer(stashed, sp) {
  const f = stashed.fields;
  f.name = sp.name || '';
  f.gender = normalizeGender(sp.gender || f.gender);
  f.race = sp.race || '';
  f.age = sp.age || '';
  f.height = sp.height || '';
  f.weight = sp.weight || '';
  f.hair_color = sp.hair_color || '';
  f.hair_length = sp.hair_length || '';
  f.hair_style = sp.hair_style || '';
  f.body_type = sp.body_type || '';
  f.skin_complexion = sp.skin_complexion || '';
  f.eye_color = sp.eye_color || '';
  f.breast_size = sp.breast_size || '';
  f.breast_size_enabled = sp.breast_size != null;
  f.ears = sp.ears || '';
  f.ears_enabled = sp.ears != null;
  f.tail = sp.tail || '';
  f.tail_enabled = sp.tail != null;
  f.clothing = Array.isArray(sp.clothing) ? sp.clothing.slice() : [];
  if (sp.portrait) {
    stashed.portraitPreviewSrc = convertFileSrc(sp.portrait);
  }
}

// --- The bespoke player review. Predates the generic review + has the
// clothing-as-its-own-entity layout (the 2026-08-05 overhaul Chloe signed
// off on). Kept verbatim so the player review's exact look is unchanged.
function renderReview(stashed) {
  const f = stashed.fields;
  const identityRows = [
    ['Name', f.name],
    ['Gender', f.gender],
    ['Race', f.race],
    ['Age', f.age],
    ['Height', f.height],
    ['Weight', f.weight],
  ].filter(([, v]) => (v || '').toString().trim());
  const appearanceRows = [
    ['Hair Color', f.hair_color],
    ['Hair Length', f.hair_length],
    ['Hair Style', f.hair_style],
    ['Body', f.body_type],
    ['Skin', f.skin_complexion],
    ['Eyes', f.eye_color],
  ].filter(([, v]) => (v || '').toString().trim());
  if (f.breast_size_enabled) appearanceRows.push(['Breast', f.breast_size]);
  if (f.ears_enabled) appearanceRows.push(['Ears', f.ears]);
  if (f.tail_enabled) appearanceRows.push(['Tail', f.tail]);
  const clothing = Array.isArray(f.clothing) && f.clothing.length ? f.clothing : null;

  const portraitHTML = stashed.portraitPreviewSrc
    ? `<img src="${esc(stashed.portraitPreviewSrc)}" alt="" onerror="this.style.display='none'">`
    : `<span class="fable-player-review-portrait-fallback" aria-hidden="true">${SILHOUETTE_SVG}</span>`;

  const pair = ([k, v]) => `<div><dt>${esc(k)}</dt><dd>${esc(v)}</dd></div>`;
  const identityHTML = identityRows.length
    ? `<section class="fable-player-review-section"><h3>Identity</h3><dl>${identityRows.map(pair).join('')}</dl></section>`
    : '';
  const appearanceHTML = appearanceRows.length
    ? `<section class="fable-player-review-section"><h3>Appearance</h3><dl>${appearanceRows.map(pair).join('')}</dl></section>`
    : '';
  const clothingHTML = `<section class="fable-player-review-section fable-player-review-clothing">
    <h3>Clothing</h3>
    <div class="fable-player-review-chips">${clothing ? clothing.map((c) => `<span class="fable-wizard-chip">${esc(c)}</span>`).join('') : '<span class="fable-player-review-chips-empty">No garments</span>'}</div>
  </section>`;

  return `<div class="fable-player-review-card">
    <div class="fable-player-review-top">
      <div class="fable-player-review-portrait">${portraitHTML}</div>
      <div class="fable-player-review-body">
        ${identityHTML}${appearanceHTML}
      </div>
    </div>
    ${clothingHTML}
  </div>
  <div class="fable-player-review-create-wrap">
    <button type="button" class="fable-player-review-create" data-review-create>CREATE</button>
    <button type="button" class="fable-player-review-back" data-review-back aria-label="Back">${ARROW_SVG_LEFT}</button>
  </div>`;
}

function buildPlayerReview() {
  return {
    title: 'Review',
    render: renderReview,
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

// --- Serialize stashed → SavedPlayer JSON (mirrors the pre-refactor fn).
function buildPlayer(stashed) {
  const f = stashed.fields;
  const opt = (v) => { const s = (v || '').trim(); return s ? s : null; };
  const conditional = (field) => {
    if (f[`${field}_enabled`] !== true) return null;
    return opt(f[field]);
  };
  const clothing = Array.isArray(f.clothing) && f.clothing.length
    ? f.clothing.map((c) => String(c).trim()).filter(Boolean)
    : null;
  return {
    id: slugify(f.name || ''),
    name: (f.name || '').trim(),
    gender: normalizeGender(f.gender),
    race: opt(f.race),
    age: opt(f.age),
    height: opt(f.height),
    weight: opt(f.weight),
    hair_color: opt(f.hair_color),
    hair_length: opt(f.hair_length),
    hair_style: opt(f.hair_style),
    body_type: opt(f.body_type),
    skin_complexion: opt(f.skin_complexion),
    eye_color: opt(f.eye_color),
    breast_size: conditional('breast_size'),
    ears: conditional('ears'),
    tail: conditional('tail'),
    clothing,
    portrait: null,
    created_at_ms: 0,
  };
}

// --- onCreated: the SavedPlayer write path (keeps the pre-refactor flow).
async function onCreated(root, stashed) {
  const player = buildPlayer(stashed);
  const handlers = root._handlers || {};
  try {
    const meta = await invoke('fable_player_write', { id: player.id, player });
    if (stashed.portraitCroppedBytes && stashed.portraitCroppedExt) {
      try {
        await invoke('fable_player_portrait_upload_bytes', {
          id: meta.id,
          bytesB64: bytesToBase64(stashed.portraitCroppedBytes),
        });
      } catch (err) {
        if (root._toast) root._toast(`Player saved, but portrait upload failed: ${err}`);
      }
    } else if (stashed.portraitSrcPath) {
      try {
        await invoke('fable_player_portrait_upload', { id: meta.id, srcPath: stashed.portraitSrcPath });
      } catch (err) {
        if (root._toast) root._toast(`Player saved, but portrait upload failed: ${err}`);
      }
    }
    if (handlers.onSave) handlers.onSave(meta.id);
  } catch (err) {
    throw err; // runCreate surfaces the toast
  }
}

export function buildPlayerCreator() {
  const root = buildWizard({
    screenId: 'player-creator',
    screenClass: 'fable-player-creator-screen',
    slides: buildSlides(),
    freshStashed,
    seedFrom: seedFromPlayer,
    review: buildPlayerReview(),
    onCreated,
    ctx: buildCtx(),
  });
  root._runCreate = () => runCreate(root);
  return root;
}

export function renderPlayerCreator(root, handlers = {}) {
  renderWizard(root, handlers);
}

// Apply a parsed ST import to the player wizard (the player schema maps only
// name → Name; the rest of a ST character is ignored — players are pure
// identity, not prose). Called by the slide-1 import button.
export async function importIntoPlayerCreator(root) {
  const result = await openImportDialog(root, SCHEMA_KEY);
  if (!result) return;
  const stashed = root._stashed;
  Object.assign(stashed.fields, result.fields);
  if (result.portraitBytes && result.portraitExt) {
    stashed.portraitCroppedBytes = result.portraitBytes;
    stashed.portraitCroppedExt = result.portraitExt;
    stashed.portraitPreviewSrc = result.portraitDataUrl;
  }
  if (root._paint) root._paint();
}
