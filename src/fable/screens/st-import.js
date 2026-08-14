// =============================================================
// SILLYTAVERN IMPORTER — auto-fills the wizard from a SillyTavern V2/V3
// PNG or a plain character JSON. The wizard is NOT skipped: fields are
// populated so the user clicks through + reviews/edits in our UI.
//
// PNG PATH: SillyTavern embeds the character JSON in a `tEXt`/`iTXt` chunk
// keyed `chara` (base64-encoded UTF-8 JSON). We walk the PNG chunks
// client-side (the bytes are already in the webview after the dialog
// read), find the chunk, decode, JSON.parse. The PNG bytes ALSO become
// the auto-filled portrait (routed through the cropper for the 2:3 crop,
// same as a manual pick).
//
// JSON PATH: JSON.parse accepts both the V2/V3 wrapper
// ({ spec: 'chara_card_v2', data: {...} }) and a plain character JSON
// ({ name, description, personality, scenario, first_mes, ... }).
//
// SCHEMA MAP: each creator declares which wizard field each ST field maps
// to (PLAYER/NPC/WORLD/SCENARIO). mapImport(parsed, schemaKey) returns a
// { fields, codexEntries, portraitBytes? } patch applied to stashed.
// =============================================================

import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { openPortraitCropper } from './portrait-cropper.js';
import {
  findCharaChunk,
  base64ToUtf8,
  normalizeCharJson,
  normalizeLorebookJson,
  lorebookToCodexEntries,
} from '../engine/creator-engine.js';

// Read a file's bytes as a Uint8Array via the existing portrait-bytes IPC
// (server-side read + magic-byte validation + base64 encode → data URL).
// Returns a data URL string (same-origin, cropper-safe) or null.
async function readImageAsDataUrl(srcPath) {
  try {
    return await invoke('fable_player_portrait_read_bytes', { srcPath });
  } catch (_) {
    return null;
  }
}

// Read a small JSON file's text via the server-side import-text IPC. Used for
// plain .json lorebook/character imports (not PNG). Returns parsed object or
// throws. Server-side read is load-bearing: `convertFileSrc` (`asset://`) is
// gated by the asset protocol scope (apps/fable/... only), so a user-picked
// .json from anywhere else 403s → import failed. `creator_read_import_text`
// reads OS-native paths directly (the same pattern as the portrait-bytes IPC).
async function readJsonFile(srcPath) {
  const text = await invoke('creator_read_import_text', { srcPath });
  return JSON.parse(text);
}

// --- PNG chunk walker + JSON normalize ------------------------------------
// The pure parse primitives (findCharaChunk, base64ToUtf8, normalizeCharJson,
// normalizeLorebookJson, lorebookToCodexEntries) live in
// engine/creator-engine.js so they're unit-testable + shared with the
// codex-import path. This module keeps only the Tauri-coupled I/O
// (readImageAsDataUrl / readJsonFile) + the import entry points.

// --- Schema maps ----------------------------------------------------------
// Each creator's config declares which wizard field each ST field maps to.
// `null` = ignore for this schema. The mapImport fn reads the schema's
// table + builds the fields patch.
const SCHEMA_MAPS = {
  // Player + NPC share the character-oriented map.
  npc: {
    name: 'name',
    description: 'backstory',
    personality: 'personality',
    scenario: 'backstory',
    first_mes: 'intro_message',
    mes_example: 'conversation_style',
    creator_notes: 'miscellaneous',
  },
  player: {
    name: 'name',
    description: 'backstory',   // a ST character's description → Backstory slide
    personality: null,
    scenario: 'backstory',      // scenario context also feeds Backstory
    first_mes: null,
    mes_example: null,
    creator_notes: null,
  },
  world: {
    name: 'name',
    description: 'setting',
    personality: 'tone',
    scenario: 'setting',
    first_mes: null,
    mes_example: 'tone',
    creator_notes: null,
  },
  scenario: {
    name: 'name',
    description: 'setting',
    personality: 'tone',
    scenario: 'setting',
    first_mes: 'intro_message',
    mes_example: null,
    creator_notes: null,
  },
};

// Build the { fields, codexEntries } patch for a given schema. Does NOT
// touch portrait (the caller handles the PNG portrait separately).
export function mapImport(charData, schemaKey) {
  const map = SCHEMA_MAPS[schemaKey] || SCHEMA_MAPS.npc;
  const fields = {};
  // For description→backstory: APPEND scenario to it (so both land in
  // backstory without overwriting). Build the value per target field.
  const accum = {};
  function add(target, value) {
    if (!target || !value) return;
    const v = value.toString().trim();
    if (!v) return;
    if (accum[target]) {
      // Only append if not already present (avoid dup when description +
      // scenario both map to backstory).
      if (!accum[target].includes(v)) accum[target] += `\n\n${v}`;
    } else {
      accum[target] = v;
    }
  }
  add(map.name, charData.name);
  add(map.description, charData.description);
  add(map.personality, charData.personality);
  add(map.scenario, charData.scenario);
  add(map.first_mes, charData.first_mes);
  add(map.mes_example, charData.mes_example);
  add(map.creator_notes, charData.creator_notes);
  Object.assign(fields, accum);
  const codexEntries = lorebookToCodexEntries(charData.character_book);
  return { fields, codexEntries };
}

// --- GLM-creator import: parse without the cropper/mapImport -------------
// A slim variant of openImportDialog for the conversational creator: open the
// picker, parse the .png/.json to a normalized charData object (for GLM to
// refine via the import_data IPC arg), + return the raw portrait data URL
// (uncropped) so the review slot can pre-fill + crop on click. Does NOT run
// the cropper or mapImport (GLM does its own field mapping). Returns
// { charData, portraitDataUrl?, portraitExt?, portraitBytes? } or null on
// cancel. `portraitBytes` carries the RAW PNG bytes so the imported portrait
// is saved on CREATE even if the user never re-opens the cropper — without
// it the review preview shows a portrait that `doCreate` silently drops
// (state.portraitBytes stays null). A later cropper confirm overrides these
// with the cropped bytes (cropped always wins when the user crops).
export async function parseImportFile(screenEl) {
  void screenEl; // kept for signature parity with openImportDialog
  let picked;
  try {
    picked = await openDialog({
      multiple: false,
      filters: [{ name: 'Character / World', extensions: ['png', 'json'] }],
    });
  } catch (_) {
    return null;
  }
  if (!picked) return null;
  const srcPath = typeof picked === 'string' ? picked : (picked.path || picked);
  if (!srcPath) return null;
  const lower = srcPath.toLowerCase();

  let charData = null;
  let portraitDataUrl = null;
  let portraitExt = null;
  let portraitBytes = null;
  if (lower.endsWith('.png')) {
    const dataUrl = await readImageAsDataUrl(srcPath);
    if (!dataUrl) throw new Error('could not read PNG');
    const res = await fetch(dataUrl);
    const u8 = new Uint8Array(await res.arrayBuffer());
    const b64 = findCharaChunk(u8);
    if (!b64) throw new Error('no SillyTavern character data found in this PNG');
    const json = JSON.parse(base64ToUtf8(b64));
    charData = normalizeCharJson(json);
    if (!charData) throw new Error('embedded character JSON is empty');
    portraitDataUrl = dataUrl; // uncropped preview — the review slot crops on click
    portraitExt = 'png';
    // Keep the raw PNG bytes as the save fallback (see jsdoc above).
    portraitBytes = u8;
  } else if (lower.endsWith('.json')) {
    const json = await readJsonFile(srcPath);
    // Try a character shape first (V2/V3 wrapper or plain), then a standalone
    // lorebook ({ entries } with no character fields) — both surface as a
    // charData object so GLM/the codex path convert them uniformly.
    charData = normalizeCharJson(json) || normalizeLorebookJson(json);
    if (!charData) throw new Error('character or lorebook JSON is empty / unrecognized');
  } else {
    throw new Error('select a .png or .json file');
  }
  // Mechanically capture the SillyTavern greetings (first_mes + alternate_
  // greetings) as the opening-beat text — NOT left to GLM's refinement, so the
  // authored greetings survive verbatim. The flow carries this into the SIM
  // card's `<intro>` (via the serializer's draft.intro), where Rust reads it as
  // the Fable opening beat. One greeting per line (matches wupi.sim's shape).
  const introText = charData
    ? [
        charData.first_mes,
        ...(Array.isArray(charData.alternate_greetings) ? charData.alternate_greetings : []),
      ]
        .map((s) => (s == null ? '' : String(s)).trim())
        .filter(Boolean)
        .join('\n')
    : '';
  return { charData, portraitDataUrl, portraitExt, portraitBytes, introText };
}

// --- The public entry: openImportDialog ----------------------------------
// Opens a file picker (.png/.json), parses, + returns
// { charData, fields, codexEntries, portraitDataUrl?, portraitBytes?, portraitExt? }
// or null on cancel/error. The caller applies the patch to stashed.
//
// `screenEl` is the wizard root (needed for the cropper modal's parent).
// `schemaKey` selects the field map. For PNG imports, the portrait is run
// through the cropper so the saved portrait is exactly the crop.
export async function openImportDialog(screenEl, schemaKey) {
  let picked;
  try {
    picked = await openDialog({
      multiple: false,
      filters: [
        { name: 'Character / World', extensions: ['png', 'json'] },
      ],
    });
  } catch (_) {
    return null;
  }
  if (!picked) return null;
  const srcPath = typeof picked === 'string' ? picked : (picked.path || picked);
  if (!srcPath) return null;
  const lower = srcPath.toLowerCase();

  let charData = null;
  let portraitDataUrl = null;
  let portraitBytes = null;
  let portraitExt = null;

  if (lower.endsWith('.png')) {
    // Read the PNG bytes (via the data-URL IPC) + walk the chunks.
    const dataUrl = await readImageAsDataUrl(srcPath);
    if (!dataUrl) throw new Error('could not read PNG');
    // Fetch the data URL into bytes.
    const res = await fetch(dataUrl);
    const buf = await res.arrayBuffer();
    const u8 = new Uint8Array(buf);
    const b64 = findCharaChunk(u8);
    if (!b64) throw new Error('no SillyTavern character data found in this PNG');
    let json;
    try {
      json = JSON.parse(base64ToUtf8(b64));
    } catch (e) {
      throw new Error(`could not parse embedded character JSON: ${e}`);
    }
    charData = normalizeCharJson(json);
    if (!charData) throw new Error('embedded character JSON is empty');
    // The PNG itself is the portrait → route through the cropper.
    try {
      const cropped = await openPortraitCropper(screenEl, dataUrl);
      if (cropped) {
        portraitDataUrl = cropped.dataUrl;
        portraitBytes = cropped.bytes;
        portraitExt = cropped.ext;
      }
    } catch (_) {
      // Cropper cancel → keep the char data, skip the portrait.
    }
  } else if (lower.endsWith('.json')) {
    const json = await readJsonFile(srcPath);
    charData = normalizeCharJson(json);
    if (!charData) throw new Error('character JSON is empty');
  } else {
    throw new Error('select a .png or .json file');
  }

  const { fields, codexEntries } = mapImport(charData, schemaKey);
  return { charData, fields, codexEntries, portraitDataUrl, portraitBytes, portraitExt };
}
