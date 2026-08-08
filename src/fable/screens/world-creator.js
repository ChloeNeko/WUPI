// =============================================================
// SCREEN: WORLD CREATOR — a slide wizard authoring a world sim card.
//
// Thin config over the generic wizard-engine. A world is a stage, not a
// character: Portrait/Cover → Name → Directive → Setting → Tone → Attach
// Codex → Review. The Directive is the world's driving principle (→
// <persona>); Setting + Tone are first-class. No cast, no intro (a world
// has no opening beat — it's entered, not encountered).
//
// OUTPUT: a flat-format <sim_card> (card_type=roleplay) via serializeWorldCard
// → fable_write_card + fable_card_sibling_write (codex) + fable_card_portrait_write
// (cover). Worlds are "just sim cards in card folders" per the user's directive.
// =============================================================

import {
  buildWizard, renderWizard, runCreate, buildGenericReview,
  requiredTextValidate, optionalTextValidate, proseInvalid,
  NAME_MAX,
} from './wizard-engine.js';
import { serializeWorldCard } from './card-serialize.js';
import { writeCardArtifact, pickPortrait, attachCodex } from './creator-shared.js';
import { openImportDialog } from './st-import.js';

const SCHEMA_KEY = 'world';

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
      id: 'directive', title: 'Directive', kind: 'textarea', field: 'directive',
      rows: 6, required: true,
      placeholder: 'The world’s driving principle — what makes this world breathe.',
      validate: (s) => {
        const v = (s.directive || '').trim();
        if (!v) return 'Directive is required.';
        return proseInvalid('Directive', s.directive);
      },
    },
    {
      id: 'setting', title: 'Setting', kind: 'textarea', field: 'setting',
      rows: 8, required: true,
      placeholder: 'The world premise: place, time, genre, what is possible here.',
      validate: (s) => {
        const v = (s.setting || '').trim();
        if (!v) return 'Setting is required.';
        return proseInvalid('Setting', s.setting);
      },
    },
    {
      id: 'tone', title: 'Tone', kind: 'textarea', field: 'tone',
      rows: 3, required: true,
      placeholder: "Narrative voice: 'grim, atmospheric, slow-burn'.",
      validate: (s) => {
        const v = (s.tone || '').trim();
        if (!v) return 'Tone is required.';
        return proseInvalid('Tone', s.tone);
      },
    },
    { id: 'codex', title: 'Attach Codex', kind: 'codex-attach' },
  ];
}

function freshStashed() {
  return {
    fields: {
      codex_entries: [],
    },
    portraitSrcPath: null,
    portraitCroppedBytes: null,
    portraitCroppedExt: null,
    portraitPreviewSrc: null,
  };
}

function buildWorldReview() {
  return buildGenericReview({
    showPortrait: true,
    sections: (stashed) => {
      const f = stashed.fields;
      return [
        { title: 'World', rows: [
          ['Name', f.name], ['Directive', f.directive],
        ]},
        { title: 'Narrative', rows: [
          ['Setting', f.setting], ['Tone', f.tone],
        ]},
      ];
    },
  });
}

async function onCreated(root, stashed) {
  const built = serializeWorldCard(stashed.fields);
  const stem = slugifyWorld(stashed.fields.name || '');
  const handlers = root._handlers || {};
  await writeCardArtifact({ built, stem, stashed });
  if (handlers.onSave) handlers.onSave(stem);
}

function slugifyWorld(s) {
  return (s || '').trim().toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '') || 'world';
}

function buildCtx() {
  return {
    onPickPortrait: pickPortrait,
    onAttachCodex: attachCodex,
    onImport: (screenEl) => importIntoWorldCreator(screenEl),
    schemaKey: SCHEMA_KEY,
  };
}

export function buildWorldCreator() {
  const root = buildWizard({
    screenId: 'world-creator',
    screenClass: 'fable-world-creator-screen',
    slides: buildSlides(),
    freshStashed,
    review: buildWorldReview(),
    onCreated,
    ctx: buildCtx(),
  });
  root._runCreate = () => runCreate(root);
  return root;
}

export function renderWorldCreator(root, handlers = {}) {
  renderWizard(root, handlers);
}

export async function importIntoWorldCreator(root) {
  const result = await openImportDialog(root, SCHEMA_KEY);
  if (!result) return;
  const stashed = root._stashed;
  Object.assign(stashed.fields, result.fields);
  if (result.codexEntries && result.codexEntries.length) {
    if (!Array.isArray(stashed.fields.codex_entries)) stashed.fields.codex_entries = [];
    stashed.fields.codex_entries.push(...result.codexEntries);
  }
  if (result.portraitBytes && result.portraitExt) {
    stashed.portraitCroppedBytes = result.portraitBytes;
    stashed.portraitCroppedExt = result.portraitExt;
    stashed.portraitPreviewSrc = result.portraitDataUrl;
  }
  if (root._paint) root._paint();
}
