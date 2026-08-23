// =============================================================
// SILLYTAVERN IMPORTER — feeds the GLM conversational creators from a
// SillyTavern V2/V3 PNG or a plain character/lorebook JSON.
//
// PNG PATH: SillyTavern embeds the character JSON in a `tEXt`/`zTXt`/`iTXt`
// chunk keyed `chara` (base64-encoded UTF-8 JSON, possibly zlib-compressed).
// We walk the PNG chunks client-side (the bytes are already in the webview
// after the dialog read), decode EVERY candidate, and pick the card the file
// actually carries — re-export tools append fresh `chara` chunks instead of
// replacing, so one PNG can hold several different cards (see
// engine/creator-engine.js readCharaCard). The PNG bytes ALSO become the
// auto-filled portrait (saved on CREATE even without a re-crop; the review
// slot crops on click).
//
// JSON PATH: a standalone lorebook ({ entries }) is recognized FIRST and
// returns as `lorebook` (the codex import converts it — see creator-engine);
// otherwise JSON.parse accepts the V2/V3 wrapper ({ spec: 'chara_card_v2',
// data: {...} }) and the plain character JSON ({ name, description,
// personality, scenario, first_mes, ... }). An object with no content of
// either kind throws (unrecognized → the caller's bottom warning).
//
// GLM does its own field mapping from the normalized charData (the old
// fixed SCHEMA_MAPS/mapImport surface + the wizard-coupled
// openImportDialog entry were removed 2026-08-15 — parseImportFile below
// is the sole entry point).
// =============================================================

import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import {
  readCharaCard,
  fileBaseStem,
  collectIntroVariants,
  normalizeCharJson,
  extractStandaloneLorebook,
  charDataHasContent,
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
// The pure parse primitives (readCharaCard + the walker/normalize/lorebook
// helpers) live in engine/creator-engine.js so they're unit-testable +
// shared with the codex-import path. This module keeps only the
// Tauri-coupled I/O (readImageAsDataUrl / readJsonFile) + the import entry
// point.

// --- The public entry: parseImportFile ------------------------------------
// Open the picker, parse the .png/.json to a normalized charData object
// (for GLM to refine via the import_data IPC arg — GLM does its own field
// mapping), + return the raw portrait data URL (uncropped) so the review
// slot can pre-fill + crop on click. Returns
// { charData, portraitDataUrl?, portraitExt?, portraitBytes?, introText,
//   lorebook } or null on cancel. `portraitBytes` carries the RAW PNG bytes so
// the imported portrait is saved on CREATE even if the user never re-opens the
// cropper — without it the review preview shows a portrait that `doCreate`
// silently drops (state.portraitBytes stays null). A later cropper confirm
// overrides these with the cropped bytes (cropped always wins).
//
// `lorebook` (2026-08-19): {name, entries} when the file is a standalone ST
// lorebook — charData is null in that case and the CODEX import step owns the
// conversion (batched refinement → straight to the review card). Recognition
// order is load-bearing: the lorebook check runs FIRST — normalizeCharJson
// accepts any object, so a world book used to normalize into an all-empty
// character card and the codex wizard was fed a husk ("no lorebook content").
// An object with NO content of either kind throws → the caller's bottom
// warning fires and the flow never leaves the picker (no chat window).
export async function parseImportFile(screenEl) {
  void screenEl; // accepted for call-site symmetry; the picker needs no anchor element
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
    // (2026-08-22) Robust card extraction: re-exported PNGs can carry
    // SEVERAL `chara` chunks (embedding tools APPEND, never replace — one
    // real-world file holds two different cards, 344KB + 66KB).
    // readCharaCard decodes every candidate (tEXt/zTXt/iTXt, base64 or raw
    // JSON, any keyword case) and picks the card the file actually carries:
    // a name match against the file's stem, else the most recent embed.
    // The old first-`chara`-only walk handed GLM the WRONG card — and its
    // 250KB payload was then silently dropped by the API budget truncation,
    // so the import "didn't work" at all.
    const card = await readCharaCard(u8, fileBaseStem(srcPath));
    if (!card) throw new Error('no SillyTavern character data found in this PNG');
    charData = card.charData;
    portraitDataUrl = dataUrl; // uncropped preview — the review slot crops on click
    portraitExt = 'png';
    // Keep the raw PNG bytes as the save fallback (see jsdoc above).
    portraitBytes = u8;
  } else if (lower.endsWith('.json')) {
    let json;
    try {
      json = await readJsonFile(srcPath);
    } catch (_) {
      throw new Error('that file is not valid JSON');
    }
    if (!json || typeof json !== 'object' || Array.isArray(json)) {
      throw new Error('no character or lorebook content found in that file');
    }
    // Lorebook FIRST (see jsdoc) — a standalone ST world book never reaches
    // the character normalizer.
    const lore = extractStandaloneLorebook(json);
    if (lore) {
      return { charData: null, portraitDataUrl: null, portraitExt: null, portraitBytes: null, introText: '', introVariants: [], lorebook: lore };
    }
    charData = normalizeCharJson(json);
    if (!charDataHasContent(charData)) throw new Error('no character or lorebook content found in that file');
  } else {
    throw new Error('select a .png or .json file');
  }
  // Mechanically capture the SillyTavern greetings (first_mes + alternate_
  // greetings) as the opening-beat VARIANTS (2026-08-22) — NOT left to GLM's
  // refinement, so the authored greetings survive verbatim. The flow carries
  // the list into the SIM card's `<intro>` siblings (one per greeting via the
  // serializer's draft.intro_variants), where Rust seeds them onto session
  // message 0 as swipeable variants — the player picks an opening via the
  // ‹ 1/N › beat control right at game start. Computed from charData, so the
  // PNG and JSON import paths share ONE implementation. `introText` (the
  // newline-joined form) is kept for shape compatibility.
  const introVariants = collectIntroVariants(charData);
  const introText = introVariants.join('\n');
  return { charData, portraitDataUrl, portraitExt, portraitBytes, introText, introVariants, lorebook: null };
}

