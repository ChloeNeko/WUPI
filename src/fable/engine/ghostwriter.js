// =============================================================
// GHOSTWRITER — Impersonate authoring utility (§11.24, refactored 2026-07-27).
//
// Originally a stage FAB (Impersonate short-click + Director long-press). The
// FAB was removed when Director moved to NL-triggering via the `set_directive`
// tool (Wupi fires it from the drawer chat — see tools.rs). What remains is
// the Impersonate utility: a button on the DRAWER compose box polishes rough
// notes into clean prose, in place, with toggle-undo.
//
// This module exports the helpers the drawer consumes. It no longer owns any
// DOM; the drawer builds the ✎ button and wires it to `runImpersonateOn`.
//
// MEMORYLESS by construction: ghostwriter_generate never touches the session
// or memory. Impersonate is a one-shot text transform.
//
// Channel event shapes (from ghostwriter_generate):
//   { type: 'chunk',     text: '' }      → heartbeat
//   { type: 'fallback',  reason, source }
//   { type: 'error',     message }
//   { type: 'done',      text }          → the rewritten prose
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';

// ── Impersonate (caller-supplied input element) ───────────────────────────
//
// The drawer passes its own compose box. The function:
//   1. Reads the current text.
//   2. If the text IS the last generated result → toggle-undo (restore prior).
//   3. Else → calls ghostwriter_generate, writes the polished prose back in
//      place, stashes the prior text for the next toggle.
//
// Returns true if a generation ran (so the caller can update button state).

let _lastInputByEl = new WeakMap();   // inputEl → { prior, result }

export async function runImpersonateOn(inputEl, { onBusy, onError } = {}) {
  if (!inputEl) return false;
  const current = inputEl.value;
  const stash = _lastInputByEl.get(inputEl);

  // Toggle-undo: if the current text IS the last result, restore the prior.
  if (stash && current === stash.result) {
    inputEl.value = stash.prior || '';
    inputEl.dispatchEvent(new Event('input', { bubbles: true }));
    _lastInputByEl.delete(inputEl);
    inputEl.focus();
    return false;
  }

  const trimmed = current.trim();
  if (!trimmed) {
    // Nothing to flesh out — caller can pulse the button as a hint.
    return false;
  }

  onBusy?.(true);
  try {
    const result = await invokeGhostwriter('impersonate', trimmed);
    if (result && result.trim()) {
      _lastInputByEl.set(inputEl, { prior: current, result });
      inputEl.value = result;
      inputEl.dispatchEvent(new Event('input', { bubbles: true }));
      // Caret at end so the player can keep writing immediately.
      inputEl.focus();
      inputEl.setSelectionRange(result.length, result.length);
    }
    return true;
  } catch (err) {
    console.warn('[ghostwriter] impersonate failed:', err);
    onError?.(String(err?.message || err || 'Impersonate failed.'));
    return false;
  } finally {
    onBusy?.(false);
  }
}

// ── Director badge refresh (kept for the drawer) ──────────────────────────
//
// Director is now armed via the `set_directive` tool (fired by Wupi from the
// drawer chat). The drawer can still show a "1 directive armed" chip by
// polling fable_director_peek. fable_send consumes the directive on the next
// narrator turn — the drawer should re-call this after each narrator turn to
// clear the chip.
export async function isDirectiveArmed() {
  try {
    const directive = await invoke('fable_director_peek');
    return !!(directive && typeof directive === 'string' && directive.trim());
  } catch {
    return false;
  }
}

// ── Generation invoke (Promise-wrap, mirrors void.js::invokeGeneration) ──

function invokeGhostwriter(mode, playerInput) {
  return new Promise((resolve, reject) => {
    const channel = new Channel();
    let resolved = false;
    channel.onmessage = (e) => {
      if (!e || resolved) return;
      if (e.type === 'chunk') return;     // heartbeat
      if (e.type === 'fallback') return;  // API dropped, on local
      if (e.type === 'error') {
        resolved = true;
        reject(new Error(e.message || 'ghostwriter_generate failed'));
        return;
      }
      if (e.type === 'done') {
        resolved = true;
        resolve(String(e.text || ''));
      }
    };
    invoke('ghostwriter_generate', { mode, playerInput, onEvent: channel })
      .catch((err) => { if (!resolved) { resolved = true; reject(err); } });
  });
}
