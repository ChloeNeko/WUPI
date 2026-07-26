// =============================================================
// SCREEN: SAVES — the save-slot list (Load Game flow).
// Reads SaveMeta from fable_list_saves:
//   { save_id, name, summary, timestamp, turn_count, is_autosave }
// Delete → fable_delete_save. Select → handlers.onSelect(save).
// =============================================================

import { listSaves, deleteSave } from '../engine/saves-io.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function fmtTime(ms) {
  if (!ms) return '';
  try {
    const d = new Date(ms);
    return d.toLocaleString();
  } catch (_) { return ''; }
}

export function buildSaves(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-saves-screen';
  root.dataset.fableScreen = 'saves';
  root.hidden = true;
  root.innerHTML = `
    <header class="fable-screen-header">
      <button class="fable-back-btn" data-act="back">‹ Back</button>
      <h2 class="fable-screen-title" data-title>Saves</h2>
    </header>
    <div class="fable-saves-list" data-host></div>
  `;
  root.querySelector('[data-act="back"]').addEventListener('click', () => handlers.back());
  return root;
}

export async function renderSaves(root, cardId, onSelect) {
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  let saves = [];
  try {
    saves = await listSaves(cardId);
  } catch (err) {
    host.innerHTML = `<div class="fable-saves-empty">Couldn't load saves: ${esc(err)}</div>`;
    return;
  }
  if (!saves.length) {
    host.innerHTML = `<div class="fable-saves-empty">
      <p>No saved fable yet for this world.</p>
    </div>`;
    return;
  }
  for (const save of saves) {
    const row = document.createElement('div');
    row.className = 'fable-save-row' + (save.is_autosave ? ' autosave' : '');
    row.innerHTML = `
      <div class="fable-save-info">
        <div class="fable-save-name">${esc(save.name)}${save.is_autosave ? '<span class="fable-save-tag">auto</span>' : ''}</div>
        ${save.summary ? `<div class="fable-save-summary">${esc(save.summary)}</div>` : ''}
        <div class="fable-save-meta">${save.turn_count ? save.turn_count + ' turns · ' : ''}${fmtTime(save.timestamp)}</div>
      </div>
      <div class="fable-save-actions">
        <button class="fable-save-btn" data-act="load">Load</button>
        ${!save.is_autosave ? '<button class="fable-save-btn danger" data-act="del">Delete</button>' : ''}
      </div>
    `;
    row.querySelector('[data-act="load"]').addEventListener('click', () => onSelect(save));
    const delBtn = row.querySelector('[data-act="del"]');
    if (delBtn) delBtn.addEventListener('click', async () => {
      try {
        await deleteSave(cardId, save.save_id);
      } catch (_) {}
      renderSaves(root, cardId, onSelect);
    });
    host.appendChild(row);
  }
}
