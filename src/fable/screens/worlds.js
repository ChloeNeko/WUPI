// =============================================================
// SCREEN: WORLDS — the "pick a world" step of the Load Game flow.
//
// Sits between the title + the saves list (the Load button routes here). The
// title has no concept of an active card, so Load is a two-level flow:
//   1. pick a world (this screen)  →  2. pick a save in that world (saves.js).
//
// Reads FableCardMeta from fable_cards_list:
//   { id, name, card_type, setting_preview, tone,
//     opening_scene_preview, player_name, has_saves }
// Select a card → handlers.onSelect(card).
//
// Reuses the existing .fable-card grid CSS (the picker screen styling lives in
// fable.css) — no new CSS needed for the cards themselves.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function buildWorlds(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-picker-screen fable-worlds-screen';
  root.dataset.fableScreen = 'worlds';
  root.hidden = true;
  root.innerHTML = `
    <header class="fable-screen-header">
      <button class="fable-back-btn" data-act="back">‹ Back</button>
      <h2 class="fable-screen-title">Load Game</h2>
    </header>
    <div class="fable-card-grid" data-host></div>
  `;
  root.querySelector('[data-act="back"]').addEventListener('click', () => handlers.back());
  return root;
}

// Populate the grid. Called each time the screen is shown (the set of worlds
// may have changed since the last visit — a New Game adds one). `onSelect`
// receives the chosen FableCardMeta.
export async function renderWorlds(root, onSelect) {
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  let cards = [];
  try {
    cards = await invoke('fable_cards_list');
  } catch (err) {
    host.innerHTML = `<div class="fable-card-empty">Couldn't load worlds: ${esc(err)}</div>`;
    return;
  }
  // Only worlds with saves make sense in the Load flow (a world with no saves
  // is a "New Game" target, not a "Load" one). Empty here means the user has
  // never saved in any world.
  const playable = cards.filter((c) => c.has_saves);
  if (!playable.length) {
    host.innerHTML = `<div class="fable-card-empty">
      <p>No saved worlds yet.</p>
      <p class="fable-card-empty-hint">Start a New Game or Quick Play to create one.</p>
    </div>`;
    return;
  }
  for (const card of playable) {
    const tile = document.createElement('button');
    tile.className = 'fable-card';
    tile.type = 'button';
    const toneLine = card.tone
      ? `<div class="fable-card-tone">${esc(card.tone)}</div>`
      : '';
    const playerLine = card.player_name
      ? `<span>${esc(card.player_name)}</span>`
      : '<span>—</span>';
    tile.innerHTML = `
      <div class="fable-card-name">${esc(card.name)}</div>
      ${toneLine}
      <div class="fable-card-preview">${esc(card.setting_preview || card.opening_scene_preview || '')}</div>
      <div class="fable-card-foot">
        ${playerLine}
        <span class="fable-card-continue-badge">has saves</span>
      </div>
    `;
    tile.addEventListener('click', () => onSelect(card));
    host.appendChild(tile);
  }
}
