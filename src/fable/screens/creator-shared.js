// =============================================================
// CREATOR SHARED — helpers shared by the NPC/World/Scenario creator
// configs (the portrait pick+crop flow, the codex-attach pick+parse flow,
// + the onCreated write path). These keep the three config modules thin.
//
// THE WRITE PATH (shared onCreated): every creator writes a flat-format
// <sim_card> XML via fable_write_card (the SAME IPC the deleted creator.js
// used). After the card folder exists, an authored .codex (if any) is
// written via fable_codex_raw_set so it's live for the per-card codex
// seed on the next fable_start (Phase D). A portrait/cover image, if
// attached, is written as a sibling .png/.jpg via a base64-bytes write
// (mirroring the SavedPlayer portrait path — base64-over-JSON, never a
// bare Vec<u8>).
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { openPortraitCropper } from './portrait-cropper.js';
import { bytesToBase64 } from './wizard-engine.js';
import { codexEntriesToCompound } from './card-serialize.js';

// Pick + crop a portrait (or cover image). `aspect` selects the cropper's
// aspect — portraits use 2:3 (the default); cover images use a wider aspect.
// Writes nothing to disk (the crop is held in stashed for CREATE-time
// upload). Mirrors the Player Creator's portrait slide flow.
export async function pickPortrait(screenEl, stashed, onChange) {
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

// Pick + parse a JSON lorebook for the Attach Codex slide. Appends parsed
// entries to stashed.fields.codex_entries (no de-dup — the user can prune
// via the chip list). Uses fetch on the convertFileSrc URL.
export async function attachCodex(screenEl, stashed) {
  try {
    const picked = await openDialog({
      multiple: false,
      filters: [{ name: 'Lorebook (JSON)', extensions: ['json'] }],
    });
    if (!picked) return;
    const srcPath = typeof picked === 'string' ? picked : (picked.path || picked);
    if (!srcPath) return;
    const res = await fetch(convertFileSrc(srcPath));
    if (!res.ok) throw new Error(`read failed: ${res.status}`);
    const obj = await res.json();
    // Accept SillyTavern lorebook shape { entries: [...] | {...} } or a
    // bare array of { title, body }.
    let entries;
    if (Array.isArray(obj)) {
      entries = obj;
    } else if (obj && Array.isArray(obj.entries)) {
      entries = obj.entries;
    } else if (obj && typeof obj.entries === 'object') {
      entries = Object.values(obj.entries);
    } else {
      throw new Error('not a recognized lorebook JSON');
    }
    const parsed = entries
      .filter((e) => e && (e.content || e.body || e.comment || e.title))
      .map((e, i) => ({
        title: String(e.comment || e.title || e.name || `Entry ${i + 1}`).slice(0, 128),
        tags: Array.isArray(e.key)
          ? e.key.slice(0, 8).map((k) => String(k).slice(0, 64))
          : (e.key ? [String(e.key).slice(0, 64)] : (e.tags || [])),
        body: String(e.content || e.body || '').trim(),
      }))
      .filter((e) => e.body);
    if (!parsed.length) {
      const toast = screenEl && screenEl._toast;
      if (toast) toast('No usable entries found in that file.');
      return;
    }
    if (!Array.isArray(stashed.fields.codex_entries)) stashed.fields.codex_entries = [];
    stashed.fields.codex_entries.push(...parsed);
  } catch (err) {
    const toast = screenEl && screenEl._toast;
    if (toast) toast(String(err));
  }
}

// The shared CREATE path. Writes the <sim_card>.xml via fable_write_card
// (creates the card folder), then writes the .intro + .codex (if any) + the
// portrait/cover image as siblings. Returns { id, meta } on success;
// throws on failure (the wizard-engine's runCreate surfaces the toast).
//
// `built` is { xml, intro } from the serializer (intro is the plain text for
// the sibling .intro file — empty string when the wizard collected none; it's
// NOT inside the cached <sim_card>). `stem` is the slugified name for the
// card id. `stashed` carries the codex_entries + the portrait bytes.
export async function writeCardArtifact({ built, stem, stashed, onAfterSave }) {
  const { xml, intro } = built;
  // 1. Write the card XML (creates cards/<id>/<id>.sim).
  const meta = await invoke('fable_write_card', { stem, xml });
  const cardId = meta.id;
  // 2. Write the .intro sibling (the one-shot first narrator beat). Written
  //    even when empty so the file exists + load_card_intro resolves cleanly
  //    (an empty file → None, which is correct). The intro is NEVER in the
  //    cached <sim_card> — it's read once at game start (prime directive).
  if (intro && intro.trim()) {
    try {
      await invoke('fable_card_sibling_write', { cardId, ext: 'intro', text: intro });
    } catch (err) {
      console.warn('[creator] intro write failed (non-fatal):', err);
    }
  }
  // 3. Write the .codex sibling if any codex entries were attached. The
  //    per-card codex seed (enter_fable_session) reconciles this into the
  //    card's memory partition on the next fable_start, making the lore live
  //    for retrieval (Phase D).
  const codexText = codexEntriesToCompound(stashed.fields.codex_entries);
  if (codexText) {
    try {
      await invoke('fable_card_sibling_write', { cardId, ext: 'codex', text: codexText });
    } catch (err) {
      console.warn('[creator] codex write failed (non-fatal):', err);
    }
  }
  // 4. Portrait/cover image (if attached). Write as a sibling of the .sim.
  if (stashed.portraitCroppedBytes && stashed.portraitCroppedExt) {
    try {
      await invoke('fable_card_portrait_write', {
        cardId,
        bytesB64: bytesToBase64(stashed.portraitCroppedBytes),
        ext: stashed.portraitCroppedExt,
      });
    } catch (err) {
      console.warn('[creator] portrait write failed (non-fatal):', err);
    }
  }
  if (typeof onAfterSave === 'function') {
    await onAfterSave(cardId, meta);
  }
  return { id: cardId, meta };
}
