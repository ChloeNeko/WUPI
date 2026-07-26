// =============================================================
// GAMES SAVES I/O — thin wrappers over the fable_save_* IPCs.
// Signatures mirror lib.rs exactly (camelCase via Tauri's arg conversion):
//   fable_list_saves({ cardId })           → SaveMeta[]
//   fable_save_now({ saveId, name })       → SaveMeta
//   fable_load_save({ saveId })            → { meta, messages }
//   fable_delete_save({ cardId, saveId })  → ()
//
// Pure async functions; no state. UI calls these and renders results.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

export function listSaves(cardId) {
  return invoke('fable_list_saves', { cardId });
}

export function saveNow(saveId, name) {
  return invoke('fable_save_now', { saveId, name });
}

export function loadSave(saveId) {
  return invoke('fable_load_save', { saveId });
}

export function deleteSave(cardId, saveId) {
  return invoke('fable_delete_save', { cardId, saveId });
}
