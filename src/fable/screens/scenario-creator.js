// =============================================================
// SCREEN: SCENARIO CREATOR — a slide wizard authoring a scenario sim card.
//
// Thin config over the generic wizard-engine. A scenario is a staged
// beginning: Portrait/Cover → Name → Directive → Setting → Tone → Attach
// Codex → Intro Message → Review. The Directive → <persona>; Setting + Tone
// first-class; the Intro → the sibling .intro file (the one-shot first
// narrator beat — NOT in the cached <sim_card>, per the prime directive).
// No cast.
//
// OUTPUT: a flat-format <sim_card> (card_type=roleplay) via
// serializeScenarioCard → fable_write_card + fable_card_sibling_write
// (intro + codex) + fable_card_portrait_write (cover). Scenarios are "just
// sim cards in card folders" per the user's directive.
// =============================================================

import {
  buildWizard, renderWizard, runCreate, buildGenericReview,
  proseInvalid, NAME_MAX,
} from './wizard-engine.js';
import { serializeScenarioCard } from './card-serialize.js';
import { writeCardArtifact, pickPortrait, attachCodex } from './creator-shared.js';
import { openImportDialog } from './st-import.js';

const SCHEMA_KEY = 'scenario';

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
      placeholder: 'The scenario’s driving principle — the shape of the story.',
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
    {
      id: 'intro_message', title: 'Intro Message', kind: 'textarea', field: 'intro_message',
      rows: 8, required: true,
      placeholder: 'The opening narrator beat — where the player begins. (Lands in the .intro file, not the cached card.)',
      validate: (s) => {
        const v = (s.intro_message || '').trim();
        if (!v) return 'Intro message is required.';
        return proseInvalid('Intro Message', s.intro_message);
      },
    },
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

function buildScenarioReview() {
  return buildGenericReview({
    showPortrait: true,
    sections: (stashed) => {
      const f = stashed.fields;
      return [
        { title: 'Scenario', rows: [
          ['Name', f.name], ['Directive', f.directive],
        ]},
        { title: 'Narrative', rows: [
          ['Setting', f.setting], ['Tone', f.tone],
        ]},
        { title: 'Opening', rows: [
          ['Intro', f.intro_message],
        ]},
      ];
    },
  });
}

async function onCreated(root, stashed) {
  const built = serializeScenarioCard(stashed.fields);
  const stem = slugifyScenario(stashed.fields.name || '');
  const handlers = root._handlers || {};
  await writeCardArtifact({ built, stem, stashed });
  if (handlers.onSave) handlers.onSave(stem);
}

function slugifyScenario(s) {
  return (s || '').trim().toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '') || 'scenario';
}

function buildCtx() {
  return {
    onPickPortrait: pickPortrait,
    onAttachCodex: attachCodex,
    onImport: (screenEl) => importIntoScenarioCreator(screenEl),
    schemaKey: SCHEMA_KEY,
  };
}

export function buildScenarioCreator() {
  const root = buildWizard({
    screenId: 'scenario-creator',
    screenClass: 'fable-scenario-creator-screen',
    slides: buildSlides(),
    freshStashed,
    review: buildScenarioReview(),
    onCreated,
    ctx: buildCtx(),
  });
  root._runCreate = () => runCreate(root);
  return root;
}

export function renderScenarioCreator(root, handlers = {}) {
  renderWizard(root, handlers);
}

export async function importIntoScenarioCreator(root) {
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
