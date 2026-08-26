// =============================================================
// GAMES SAVES I/O — thin wrappers over the fable_save_* IPCs.
// (2026-08-22 session decoupling) Every slot IPC is SESSION-scoped:
//   fable_list_saves({ cardId, sessionId })           → SaveMeta[]
//   fable_save_now({ saveId, name })                  → SaveMeta
//   fable_load_save({ saveId })                       → { meta, messages }
//   fable_delete_save({ cardId, sessionId, saveId })  → ()
//   fable_sessions_list({ cardId })                   → SessionMeta[]
//   fable_session_delete({ cardId, sessionId })       → ()
//
// Pure async functions; no state. UI calls these and renders results.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

export function listSaves(cardId, sessionId) {
  return invoke('fable_list_saves', { cardId, sessionId });
}

export function saveNow(saveId, name) {
  return invoke('fable_save_now', { saveId, name });
}

export function loadSave(saveId) {
  return invoke('fable_load_save', { saveId });
}

export function deleteSave(cardId, sessionId, saveId) {
  return invoke('fable_delete_save', { cardId, sessionId, saveId });
}

export function listSessions(cardId) {
  return invoke('fable_sessions_list', { cardId });
}

export function deleteSession(cardId, sessionId) {
  return invoke('fable_session_delete', { cardId, sessionId });
}

// (2026-08-24 Part II D1) Fork one playthrough into a fresh session (folder
// + episodic memory partition). Non-destructive; the backend fail-closes.
export function branchSession(cardId, sessionId, name) {
  return invoke('fable_session_branch', { cardId, sessionId, name: name || null });
}
