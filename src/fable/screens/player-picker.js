// =============================================================
// SCREEN: PLAYER PICKER — pick a saved player (Pair 2 → Load Player).
//
// Mirrors picker.js's structure (enumerate via IPC, render tiles, onSelect)
// but lists SavedPlayers (fable_players_list) instead of cards. Each tile
// shows name + portrait (convertFileSrc) or a silhouette fallback.
//
// EMPTY STATE: fully interactive (no disabled buttons). If the player has
// no saved players yet, an empty-state message invites them to create one
// — the ‹ / ⌂ chrome stays clickable. Per Chloe: "don't make the button
// unclickable or whatever."
//
// The chrome (‹ / ⌂) is owned by the flow controller; there is no header
// bar here.
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { createEmbers } from './embers.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function buildPlayerPicker(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-player-picker-screen';
  root.dataset.fableScreen = 'player-picker';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-glow" aria-hidden="true"></div>
    <div class="fable-ember-host" aria-hidden="true"></div>
    <div class="fable-player-grid" data-host></div>
  `;
  // Ambient embers.
  const emberHost = root.querySelector('.fable-ember-host');
  let embers = null;
  root._startAmbient = () => { if (!embers) embers = createEmbers(emberHost); };
  root._stopAmbient = () => { if (embers) { embers.destroy(); embers = null; } };
  return root;
}

// Populate the grid. Called each time the screen is shown. `onSelect`
// receives the chosen player's id (fable.js attaches it at game start).
export async function renderPlayerPicker(root, onSelect) {
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  let players = [];
  try {
    players = await invoke('fable_players_list');
  } catch (err) {
    host.innerHTML = `<div class="fable-flow-empty"><p>Couldn't load players: ${esc(err)}</p></div>`;
    return;
  }
  if (!players.length) {
    // Empty state — fully interactive (chrome handles nav). No text on
    // the load tiles themselves; this is just the empty-state message.
    host.innerHTML = `<div class="fable-flow-empty">
      <p>No saved players yet.</p>
      <p class="fable-flow-empty-hint">Use ‹ to go back and Create a Player.</p>
    </div>`;
    return;
  }
  for (const p of players) {
    const tile = document.createElement('button');
    tile.className = 'fable-player-card';
    tile.type = 'button';
    // Portrait-ONLY — no name/text on the tile (Chloe: "remove all text
    // from the load part"). The portrait (or a silhouette placeholder)
    // is the entire tile. title/aria carry the name for accessibility.
    tile.title = p.name;
    tile.setAttribute('aria-label', `Load player ${p.name}`);
    const portraitHTML = p.has_portrait
      ? `<div class="fable-player-card__portrait-placeholder" data-lazy-portrait="${esc(p.id)}"></div>`
      : `<div class="fable-player-card__portrait-placeholder" aria-hidden="true"></div>`;
    tile.innerHTML = portraitHTML;
    tile.addEventListener('click', () => onSelect(p));
    host.appendChild(tile);
  }
  // Lazy-resolve portraits (one get per portrait-bearing player).
  host.querySelectorAll('[data-lazy-portrait]').forEach(async (el) => {
    try {
      const id = el.dataset.lazyPortrait;
      const full = await invoke('fable_player_get', { id });
      if (full.portrait) {
        const img = document.createElement('img');
        img.className = 'fable-player-card__portrait';
        img.src = convertFileSrc(full.portrait);
        img.alt = '';
        el.replaceWith(img);
      }
    } catch (_) { /* leave placeholder */ }
  });
}
