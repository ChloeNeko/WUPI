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
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { bytesToBase64 } from './wizard-engine.js';
import { openPortraitCropper } from './portrait-cropper.js';

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

// Read a small JSON file's text via a fetch against its asset URL. Used for
// plain .json lorebook/character imports (not PNG). Returns parsed object
// or throws.
async function readJsonFile(srcPath) {
  // convertFileSrc yields an asset:// URL the webview can fetch (cross-
  // origin-safe for read). The file is user-selected so it's in scope.
  const url = convertFileSrc(srcPath);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`read json failed: ${res.status}`);
  return res.json();
}

// --- PNG chunk walker -----------------------------------------------------
// PNG = 8-byte signature + a sequence of (length:u32be, type:4bytes,
// data:length bytes, crc:u32be) chunks. We walk until we find a `tEXt` or
// `iTXt` chunk whose keyword is `chara` (SillyTavern's convention). tEXt
// stores `keyword\0value` (Latin-1); iTXt stores
// `keyword\0compressionFlag\0compressionMethod\0langTag\0translatedKey\0text`
// (UTF-8). The value is base64; decode → JSON.
function decodeLatin1(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return s;
}

function findCharaChunk(u8) {
  // PNG signature.
  const SIG = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
  if (u8.length < 8) return null;
  for (let i = 0; i < 8; i++) if (u8[i] !== SIG[i]) return null;
  let off = 8;
  const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
  while (off + 8 <= u8.length) {
    const len = dv.getUint32(off);
    const type = decodeLatin1(u8.subarray(off + 4, off + 8));
    const dataStart = off + 8;
    const dataEnd = dataStart + len;
    if (dataEnd > u8.length) return null; // truncated
    if (type === 'tEXt' || type === 'iTXt') {
      // Find the null terminator after the keyword.
      const chunk = u8.subarray(dataStart, dataEnd);
      let nul = -1;
      for (let i = 0; i < chunk.length; i++) {
        if (chunk[i] === 0) { nul = i; break; }
      }
      if (nul > 0) {
        const keyword = decodeLatin1(chunk.subarray(0, nul));
        if (keyword === 'chara') {
          let value;
          if (type === 'tEXt') {
            value = decodeLatin1(chunk.subarray(nul + 1));
          } else {
            // iTXt: compressionFlag(1) + compressionMethod(1) + langTag\0 +
            // translatedKey\0 + text(UTF-8). Skip the fixed + two null-
            // terminated fields.
            let p = nul + 1;
            p += 1 + 1; // flag + method
            // langTag
            while (p < chunk.length && chunk[p] !== 0) p++;
            p++; // past null
            // translatedKey
            while (p < chunk.length && chunk[p] !== 0) p++;
            p++; // past null
            value = new TextDecoder('utf-8').decode(chunk.subarray(p));
          }
          return value; // base64 string
        }
      }
    }
    off = dataEnd + 4; // skip data + crc
  }
  return null;
}

function base64ToUtf8(b64) {
  const bin = atob(b64.replace(/\s/g, ''));
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder('utf-8').decode(bytes);
}

// Normalize the parsed JSON into a flat character-data object regardless
// of V2/V3 wrapper or plain shape. Returns { name, description, persona,
// personality, scenario, first_mes, mes_example, creator_notes,
// character_book, spec }.
function normalizeCharJson(obj) {
  if (!obj || typeof obj !== 'object') return null;
  // V2/V3 wraps the real data under `data`.
  const data = (obj.spec && obj.data && typeof obj.data === 'object')
    ? obj.data
    : obj;
  return {
    spec: obj.spec || 'plain',
    name: data.name || obj.name || '',
    description: data.description || '',
    personality: data.personality || '',
    scenario: data.scenario || '',
    first_mes: data.first_mes || '',
    mes_example: data.mes_example || '',
    creator_notes: data.creator_notes || '',
    // character_book is the lorebook (V2/V3).
    character_book: data.character_book || null,
  };
}

// Convert a character_book (SillyTavern lorebook) into the codex_entries
// shape the Attach Codex slide + the serializer expect. Each entry:
// { title, tags, body }. The lorebook's entries have keys like keys,
// content, comment, enabled, etc.
function lorebookToCodexEntries(book) {
  if (!book || !Array.isArray(book.entries)) return [];
  // book.entries may be an array OR an object keyed by index.
  const raw = book.entries;
  const list = Array.isArray(raw) ? raw : Object.values(raw);
  return list
    .filter((e) => e && (e.content || e.comment))
    .map((e, i) => ({
      title: (e.comment || e.name || `Entry ${i + 1}`).toString().slice(0, 128),
      tags: Array.isArray(e.key)
        ? e.key.slice(0, 8).map((k) => String(k).slice(0, 64))
        : (e.key ? [String(e.key).slice(0, 64)] : []),
      body: (e.content || '').toString(),
    }))
    .filter((e) => e.body.trim());
}

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
