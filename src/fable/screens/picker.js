// =============================================================
// SCREEN: PICKER — the card grid (New Game / Continue / Load entry).
// Reads FableCardMeta from fable_cards_list:
//   { id, name, tone, opening_scene_preview, setting_preview,
//     protagonist_name, has_saves }
// Empty state points the user at apps/fable/cards/.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function buildPicker(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-picker-screen';
  root.dataset.fableScreen = 'picker';
  root.hidden = true;
  root.innerHTML = `
    <header class="fable-screen-header">
      <button class="fable-back-btn" data-act="back">‹ Back</button>
      <h2 class="fable-screen-title">Choose a World</h2>
    </header>
    <div class="fable-card-grid" data-host></div>
  `;
  root.querySelector('[data-act="back"]').addEventListener('click', () => handlers.back());
  root.querySelector('[data-host]');
  return root;
}

export async function renderCards(root, mode, onSelect, onAuthorNew) {
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  let cards = [];
  try {
    cards = await invoke('fable_cards_list');
  } catch (err) {
    host.innerHTML = `<div class="fable-card-empty">Couldn't load cards: ${esc(err)}</div>`;
    return;
  }
  // "Author New World" tile: shown only in 'new' mode (the New Game entry).
  // Routes to the interview flow (Phase 4b/4d).
  if (mode === 'new' && onAuthorNew) {
    const el = document.createElement('button');
    el.className = 'fable-card fable-card-author';
    el.innerHTML = `
      <div class="fable-card-name">+ Author a New World</div>
      <div class="fable-card-preview">Answer a few questions and Wupi will weave you a scenario from scratch.</div>
    `;
    el.addEventListener('click', () => onAuthorNew());
    host.appendChild(el);
  }
  if (!cards.length && !(mode === 'new' && onAuthorNew)) {
    host.innerHTML = `<div class="fable-card-empty">
      <p>No scenario cards found.</p>
      <p class="fable-card-empty-hint">Drop <code>.sim</code> files into <code>apps/fable/cards/</code>, or use <strong>New Game → Author a New World</strong>.</p>
    </div>`;
    return;
  }
  for (const card of cards) {
    const el = document.createElement('button');
    el.className = 'fable-card';
    el.innerHTML = `
      <div class="fable-card-name">${esc(card.name)}</div>
      ${card.tone ? `<div class="fable-card-tone">${esc(card.tone)}</div>` : ''}
      <div class="fable-card-preview">${esc(card.opening_scene_preview || card.setting_preview || '')}</div>
      <div class="fable-card-foot">
        <span>${card.protagonist_name ? esc(card.protagonist_name) : 'Unnamed'}</span>
        ${card.has_saves ? '<span class="fable-card-continue-badge">● saved</span>' : ''}
      </div>
    `;
    el.addEventListener('click', () => onSelect(card));
    host.appendChild(el);
  }
}
