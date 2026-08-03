// =============================================================
// SCREEN: PICKER — the "pick a world" step of the New Game flow.
//
// New Game is now a simple card picker (no interview): list the shipped
// `.sim` cards, pick one, go straight into the stage at a fresh game.
// Mirrors the Load flow's worlds.js (same FableCardMeta shape, same
// .fable-card-grid CSS) but lists EVERY card (not just cards with saves)
// and its onSelect starts a fresh game rather than resuming a save.
//
// Reads FableCardMeta from fable_cards_list:
//   { id, name, card_type, setting_preview, tone,
//     opening_scene_preview, player_name, has_saves }
// Select a card → handlers.onSelect(card).
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { createEmbers } from './embers.js';

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
    <div class="fable-void-glow" aria-hidden="true"></div>
    <div class="fable-ember-host" aria-hidden="true"></div>
    <div class="fable-card-grid" data-host></div>
  `;
  // NOTE: no header bar / Back button — the flow chrome (‹ / ⌂) owns
  // nav for the whole New Game flow now. handlers.back is still invoked
  // by the flow controller when ‹ is clicked (wired in fable.js).

  // Ambient ember lifecycle (mirrors newgame-split.js / the title's
  // particles). Fresh on show, destroyed on hide — no RAF leak.
  const emberHost = root.querySelector('.fable-ember-host');
  let embers = null;
  root._startAmbient = () => { if (!embers) embers = createEmbers(emberHost); };
  root._stopAmbient = () => { if (embers) { embers.destroy(); embers = null; } };

  return root;
}

// Populate the grid. Called each time the screen is shown. `onSelect`
// receives the chosen FableCardMeta (fable.js starts a fresh game from it).
export async function renderPicker(root, onSelect) {
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  let cards = [];
  try {
    cards = await invoke('fable_cards_list');
  } catch (err) {
    host.innerHTML = `<div class="fable-card-empty">Couldn't load cards: ${esc(err)}</div>`;
    return;
  }
  if (!cards.length) {
    host.innerHTML = `<div class="fable-card-empty">
      <p>No scenario cards installed.</p>
      <p class="fable-card-empty-hint">Drop a <code>.sim</code> file into the cards folder to begin.</p>
    </div>`;
    return;
  }
  for (const card of cards) {
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
        <span class="fable-card-continue-badge">${card.has_saves ? 'has saves' : 'new'}</span>
      </div>
    `;
    tile.addEventListener('click', () => onSelect(card));
    host.appendChild(tile);
  }
}
