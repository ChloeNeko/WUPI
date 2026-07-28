// =============================================================
// CROSSROADS — the "Choose a Director" option generator (§11.24, refactored
// 2026-07-27 to NL-triggered via the Wupi drawer).
//
// Originally a stage FAB cluster (Crossroads ✦ + Ghostwriter ✎). The FABs
// were removed when Director + Crossroads moved to natural-language
// triggering: the player asks Wupi in the drawer ("what should I do next",
// "give me options") → Wupi fires the `generate_options` tool → the drawer
// receives the `tool_call` event and calls `openCrossroadsModal` below.
//
// This module now exports ONLY the modal renderer + the LENSES lookup. It owns
// no DOM lifecycle; the drawer builds nothing here, it just calls in with the
// tool_call args + its own compose box as the fill target.
//
// MEMORYLESS by construction: crossroads_generate never touches the session
// or memory. Picks drop into the drawer compose box for the player to review
// — NEVER auto-injected into context (the §7 contract).
//
// Channel event shapes (from crossroads_generate):
//   { type: 'chunk',     text: '' }                → heartbeat (skeleton cards)
//   { type: 'fallback',  reason, source }          → API dropped, on local now
//   { type: 'error',     message }
//   { type: 'done',      options: [{icon,title,description}, ...] }
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';

// The five directorial lenses. All always visible (no card-activity gating).
// The id matches `CrossroadsCategory::id()` in Rust (crossroads_prompt.rs).
export const LENSES = [
  { id: 'action',    label: 'Action',    hint: 'Things your character could do next', glyph: '⚔' },
  { id: 'plot',      label: 'Plot',      hint: 'Curveballs the world throws at you',   glyph: '✺' },
  { id: 'character', label: 'Character', hint: 'NPCs who could enter the scene',       glyph: '♛' },
  { id: 'explicit',  label: 'Explicit',  hint: 'Intimate / sensual beats',             glyph: '♥' },
  { id: 'world',     label: 'World',     hint: 'Director-level world shifts',          glyph: '☄' },
];

export function lensById(id) {
  return LENSES.find((l) => l.id === id) || LENSES[0];
}

// ── Modal open + generation ───────────────────────────────────────────────
//
// Called by the drawer when it receives a `tool_call` event with
// name === 'generate_options'. The args are already Rust-validated
// (validate_args in tools.rs enforced lens ∈ 5 ids, count ∈ 1..=12); we just
// default-and-go.
//
//   opts.root       — the element to mount the overlay into (the stage root,
//                     so the modal dims the entire background per the UX spec).
//   opts.lensId     — 'action' | 'plot' | ... (default 'action').
//   opts.count      — 1..=12 (default 6). Threads through to the IPC + drives
//                     the skeleton-card count.
//   opts.seed       — optional free-text nudge.
//   opts.fillInput  — fn(text) called on Insert/Send to write the picked
//                     option's description into the caller's compose box.
//   opts.onSubmit   — fn() called after fillInput on Send (so the caller can
//                     submit its compose form). Optional.
//
// Returns a handle with { close } so the caller can close the modal
// programmatically (e.g. when the drawer closes).

let _currentOverlay = null;
let _currentListeners = [];

export function openCrossroadsModal({
  root,
  lensId = 'action',
  count = 6,
  seed = '',
  fillInput,
  onSubmit,
} = {}) {
  closeCrossroadsModal();
  const lens = lensById(lensId);
  const n = Math.max(1, Math.min(12, Number(count) || 6));

  const overlay = document.createElement('div');
  overlay.className = 'fable-crossroads-overlay';
  overlay.innerHTML = `
    <div class="fable-crossroads-backdrop"></div>
    <div class="fable-crossroads-modal">
      <header class="fable-crossroads-head">
        <div class="fable-crossroads-head-title">
          <span class="fable-crossroads-head-glyph">${lens.glyph}</span>
          <span>${escapeHtml(lens.label)} Director</span>
        </div>
        <button class="fable-crossroads-close" type="button" aria-label="Close">✕</button>
      </header>
      <div class="fable-crossroads-body" data-body></div>
    </div>
  `;
  (root || document.body).appendChild(overlay);
  _currentOverlay = overlay;

  // Loading state: N skeleton cards (matches the requested count so the
  // layout doesn't visibly reflow when results arrive).
  const body = overlay.querySelector('[data-body]');
  body.innerHTML = Array.from({ length: n })
    .map(() => '<div class="fable-crossroads-card skeleton"></div>')
    .join('');

  _on(overlay.querySelector('.fable-crossroads-close'), 'click', closeCrossroadsModal);
  _on(overlay.querySelector('.fable-crossroads-backdrop'), 'click', closeCrossroadsModal);

  // Fire the generation.
  _runGeneration({ lens, count: n, seed, body, fillInput, onSubmit });

  return { close: closeCrossroadsModal };
}

export function closeCrossroadsModal() {
  if (_currentOverlay && _currentOverlay.parentNode) {
    _currentOverlay.parentNode.removeChild(_currentOverlay);
  }
  _currentOverlay = null;
  for (const [el, type, handler] of _currentListeners) {
    el.removeEventListener(type, handler);
  }
  _currentListeners = [];
}

export function isModalOpen() {
  return _currentOverlay !== null;
}

// ── Internals ─────────────────────────────────────────────────────────────

function _on(el, type, handler) {
  if (!el) return;
  el.addEventListener(type, handler);
  _currentListeners.push([el, type, handler]);
}

async function _runGeneration({ lens, count, seed, body, fillInput, onSubmit }) {
  try {
    const options = await _invokeCrossroads(lens.id, seed, count);
    if (options && options.length > 0) {
      _renderOptions({ body, options, fillInput, onSubmit });
    } else {
      _renderError(body, 'No options came back. Try again or rephrase.');
    }
  } catch (err) {
    _renderError(body, String(err?.message || err || 'Generation failed.'));
  }
}

function _invokeCrossroads(categoryId, seed, count) {
  return new Promise((resolve, reject) => {
    const channel = new Channel();
    let resolved = false;
    channel.onmessage = (e) => {
      if (!e || resolved) return;
      if (e.type === 'chunk') return;     // heartbeat
      if (e.type === 'fallback') return;  // API dropped, on local
      if (e.type === 'error') {
        resolved = true;
        reject(new Error(e.message || 'crossroads_generate failed'));
        return;
      }
      if (e.type === 'done') {
        resolved = true;
        resolve(Array.isArray(e.options) ? e.options : []);
      }
    };
    invoke('crossroads_generate', {
      category: categoryId,
      playerSeed: seed || '',
      count,
      onEvent: channel,
    }).catch((err) => { if (!resolved) { resolved = true; reject(err); } });
  });
}

function _renderOptions({ body, options, fillInput, onSubmit }) {
  body.innerHTML = '';
  for (const opt of options) {
    body.appendChild(_buildCard(opt, fillInput, onSubmit));
  }
}

function _buildCard(opt, fillInput, onSubmit) {
  const card = document.createElement('div');
  card.className = 'fable-crossroads-card';
  const icon = String(opt.icon || '✦').slice(0, 4);
  const title = String(opt.title || '').trim();
  const description = String(opt.description || '').trim();
  card.innerHTML = `
    <div class="fable-crossroads-card-head">
      <span class="fable-crossroads-card-icon">${escapeHtml(icon)}</span>
      <span class="fable-crossroads-card-title">${escapeHtml(title)}</span>
    </div>
    <div class="fable-crossroads-card-body">${escapeHtml(description).replace(/\n/g, '<br>')}</div>
    <div class="fable-crossroads-card-actions">
      <button class="fable-crossroads-card-btn" data-action="insert" type="button">Insert</button>
      <button class="fable-crossroads-card-btn primary" data-action="send" type="button">Send</button>
      <button class="fable-crossroads-card-btn ghost" data-action="copy" type="button">Copy</button>
    </div>
  `;
  _on(card.querySelector('[data-action="insert"]'), 'click', () => {
    fillInput?.(description);
    closeCrossroadsModal();
  });
  _on(card.querySelector('[data-action="send"]'), 'click', () => {
    fillInput?.(description);
    closeCrossroadsModal();
    onSubmit?.();
  });
  _on(card.querySelector('[data-action="copy"]'), 'click', (e) => {
    const btn = e.currentTarget;
    navigator.clipboard?.writeText(description).then(
      () => {
        const orig = btn.textContent;
        btn.textContent = 'Copied';
        setTimeout(() => { btn.textContent = orig; }, 1200);
      },
      () => { /* clipboard blocked — silent */ }
    );
  });
  return card;
}

function _renderError(body, message) {
  body.innerHTML = `<div class="fable-crossroads-error">${escapeHtml(message)}</div>`;
}

function escapeHtml(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}
