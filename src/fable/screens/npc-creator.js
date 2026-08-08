// =============================================================
// SCREEN: NPC CREATOR — a slide wizard authoring an NPC as a sim card.
//
// Thin config over the generic wizard-engine (wizard-engine.js). The 13
// base slides (Portrait→Clothing) are the SAME renderers + validators as
// the Player Creator (reused verbatim — an NPC has the same appearance
// vocabulary). The 9 new slides (Occupation→Intro) add the NPC-only prose.
//
// THE OUTPUT is a flat-format <sim_card> (card_type=roleplay) whose single
// <cast> entry IS this NPC — so fable_start seeds it as a present NPC. The
// intro (if any) lands in the sibling .intro file (NOT in the cached XML).
// Codex entries (if attached via the ST import) land in the sibling .codex.
//
// PERSISTENCE: fable_write_card (the card XML) → fable_card_sibling_write
// (intro + codex) → fable_card_portrait_write (the portrait). All under the
// card's per-card folder (cards/<id>/). NPCs are "just sim cards in card
// folders" per the user's directive.
// =============================================================

import {
  buildWizard, renderWizard, runCreate, buildGenericReview,
  requiredTextValidate, optionalTextValidate, conditionalValidate,
  traitInvalid, proseInvalid, normalizeGender, slugify,
  NAME_MAX, TRAIT_MAX, PROSE_MAX,
} from './wizard-engine.js';
import { serializeNpcCard } from './card-serialize.js';
import { writeCardArtifact, pickPortrait, attachCodex } from './creator-shared.js';
import { openImportDialog } from './st-import.js';

const SCHEMA_KEY = 'npc';

// --- The slide list. The 13 base slides mirror the Player Creator; the 9
// new slides add the NPC-only prose (Occupation/Conversation Style/
// Personality/Backstory/Likes/Dislikes/Core Mission/Miscellaneous/Intro).
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
    // --- NPC-only prose slides ---
    traitSlide('occupation', 'Occupation'),
    traitSlide('conversation_style', 'Conversation Style'),
    {
      id: 'personality', title: 'Personality', kind: 'textarea', field: 'personality',
      rows: 6, required: true,
      validate: (s) => {
        const v = (s.personality || '').trim();
        if (!v) return 'Personality is required.';
        return proseInvalid('Personality', s.personality);
      },
    },
    {
      id: 'backstory', title: 'Backstory', kind: 'textarea', field: 'backstory',
      rows: 8, required: true,
      validate: (s) => {
        const v = (s.backstory || '').trim();
        if (!v) return 'Backstory is required.';
        return proseInvalid('Backstory', s.backstory);
      },
    },
    traitSlide('likes', 'Likes'),
    traitSlide('dislikes', 'Dislikes'),
    {
      id: 'core_mission', title: 'Core Mission', kind: 'textarea', field: 'core_mission',
      rows: 4,
      placeholder: 'What drives this NPC? (optional)',
      validate: optionalTextValidate('Core Mission', 'core_mission', 'prose'),
    },
    {
      id: 'miscellaneous', title: 'Miscellaneous', kind: 'textarea', field: 'miscellaneous',
      rows: 4,
      placeholder: 'Any other notes (optional)',
      validate: optionalTextValidate('Miscellaneous', 'miscellaneous', 'prose'),
    },
    {
      id: 'intro_message', title: 'Intro Message', kind: 'textarea', field: 'intro_message',
      rows: 6,
      placeholder: 'The opening beat when this NPC is first encountered (optional)',
      validate: optionalTextValidate('Intro Message', 'intro_message', 'prose'),
    },
  ];
}

function traitSlide(id, title) {
  return {
    id, title, kind: 'text', field: id,
    validate: requiredTextValidate(title, id),
  };
}

// --- freshStashed: the initial wizard state. Seeds gender from the paperdoll
// localStorage key (same as the Player Creator) + an empty codex list.
function freshStashed() {
  return {
    fields: {
      gender: normalizeGender(localStorage.getItem('wupi.paperdoll.gender') || 'male'),
      clothing: [],
      codex_entries: [],
    },
    portraitSrcPath: null,
    portraitCroppedBytes: null,
    portraitCroppedExt: null,
    portraitPreviewSrc: null,
  };
}

// --- The review config: a generic SIM-card review built from a sections
// function (so rows resolve at paint time from stashed.fields — which
// conditional toggles were set, which fields are non-empty). Portrait LEFT,
// Identity/Appearance/Personality/Mission sections RIGHT, CREATE + back beneath.
function buildNpcReview() {
  return buildGenericReview({
    showPortrait: true,
    sections: (stashed) => {
      const f = stashed.fields;
      const appearance = [
        ['Height', f.height], ['Weight', f.weight],
        ['Hair', [f.hair_color, f.hair_length, f.hair_style].filter(Boolean).join(' · ')],
        ['Body', f.body_type], ['Skin', f.skin_complexion], ['Eyes', f.eye_color],
      ];
      if (f.breast_size_enabled) appearance.push(['Breast', f.breast_size]);
      if (f.ears_enabled) appearance.push(['Ears', f.ears]);
      if (f.tail_enabled) appearance.push(['Tail', f.tail]);
      if (Array.isArray(f.clothing) && f.clothing.length) appearance.push(['Clothing', f.clothing]);
      return [
        { title: 'Identity', rows: [
          ['Name', f.name], ['Gender', f.gender], ['Race', f.race],
          ['Age', f.age], ['Occupation', f.occupation],
        ]},
        { title: 'Appearance', rows: appearance },
        { title: 'Personality', rows: [
          ['Personality', f.personality], ['Backstory', f.backstory],
          ['Likes', f.likes], ['Dislikes', f.dislikes],
          ['Conversation', f.conversation_style],
        ]},
        { title: 'Mission & Notes', rows: [
          ['Core Mission', f.core_mission], ['Miscellaneous', f.miscellaneous],
          ['Intro', f.intro_message],
        ]},
      ];
    },
  });
}

// --- onCreated: serialize → write the card + siblings → hand off.
async function onCreated(root, stashed) {
  const built = serializeNpcCard(stashed.fields);
  const stem = slugify(stashed.fields.name || '');
  const handlers = root._handlers || {};
  await writeCardArtifact({ built, stem, stashed });
  if (handlers.onSave) handlers.onSave(stem);
}

// --- The ctx: wires the portrait pick, codex attach, + ST import for the
// slide renderers that need them. The ST import is exposed as a method the
// slide-1 portrait renderer can call (the import button lives on slide 1).
function buildCtx() {
  return {
    onPickPortrait: pickPortrait,
    onAttachCodex: attachCodex,
    onImport: (screenEl) => importIntoNpcCreator(screenEl),
    schemaKey: SCHEMA_KEY,
  };
}

export function buildNpcCreator() {
  const root = buildWizard({
    screenId: 'npc-creator',
    screenClass: 'fable-npc-creator-screen',
    slides: buildSlides(),
    freshStashed,
    review: buildNpcReview(),
    onCreated,
    ctx: buildCtx(),
  });
  // Expose runCreate + the ST import trigger on the root for the review
  // CREATE button + the slide-1 import button.
  root._runCreate = () => runCreate(root);
  return root;
}

export function renderNpcCreator(root, handlers = {}) {
  renderWizard(root, handlers);
}

// Apply a parsed ST import to the wizard's stashed state (called by the
// slide-1 import button). Auto-fills the fields so the user clicks through
// to review/edit.
export async function importIntoNpcCreator(root) {
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
  if (root._paint) root._paint(); // re-render slide 1 to show the imported portrait
}
