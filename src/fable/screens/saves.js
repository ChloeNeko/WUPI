// =============================================================
// SCREEN: SAVES — the save-slot list (Load Game flow, step 2).
//
// Reached after picking a world (screens/worlds.js) + clicking LOAD on its
// modal. Reads SaveMeta for that world from fable_list_saves:
//   { save_id, name, summary, timestamp, turn_count, is_autosave }
//
// The per-turn AUTOSAVE is promoted to a one-click "Resume Latest" button at
// the top of the list — it IS the world's latest state. (Resume Latest
// reuses the autosave, which is exactly what CONTINUE on the title screen
// resumes too.) The list below shows the MANUAL saves only (most-recent
// first; the backend sorts by timestamp desc). Each manual row is Load +
// Delete; the autosave button is resume-only (deleting it is pointless — the
// next turn writes a fresh one).
//
// Load → onSelect(save) → resumeSave. Delete → fable_delete_save → re-render.
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
      <!-- (2026-08-15 audit fix) inline error surface: the saves screen has no
           toast of its own, and a swallowed delete failure must be visible. -->
      <p class="fable-saves-status" data-status hidden></p>
    </header>
    <div class="fable-saves-list" data-host></div>
  `;
  root.querySelector('[data-act="back"]').addEventListener('click', () => handlers.back());
  return root;
}

// Render the list for a world. `cardName` is shown in the header title so the
// user knows which world's saves they're browsing; `cardId` scopes the list +
// delete calls. `onSelect` receives the chosen SaveMeta.
export async function renderSaves(root, cardId, onSelect, cardName) {
  const titleEl = root.querySelector('[data-title]');
  if (titleEl) titleEl.textContent = cardName ? `${cardName} — Saves` : 'Saves';
  const host = root.querySelector('[data-host]');
  // (2026-08-20 audit) Generation token (the worlds grid's _renderGen
  // pattern): a re-render can race an in-flight `listSaves` (delete's
  // re-fire, a quick Back → worlds → other world → saves) — the stale
  // completion abandons before appending, or both loads' rows land.
  root._renderGen = (root._renderGen || 0) + 1;
  const gen = root._renderGen;
  host.innerHTML = '';
  let saves = [];
  try {
    saves = await listSaves(cardId);
  } catch (err) {
    if (gen !== root._renderGen) return;
    host.innerHTML = `<div class="fable-saves-empty">Couldn't load saves: ${esc(err)}</div>`;
    return;
  }
  if (gen !== root._renderGen) return;
  // The autosave is the per-turn checkpoint = the world's latest state. Promote
  // it to a one-click Resume Latest button at the top; the list below shows the
  // manual saves only. (Resume Latest reuses the autosave, the same slot
  // CONTINUE on the title screen resumes.)
  const autosave = saves.find((s) => s.is_autosave) || null;
  const manuals = saves.filter((s) => !s.is_autosave);

  if (autosave) {
    const resume = document.createElement('button');
    resume.type = 'button';
    resume.className = 'fable-save-resume-latest';
    resume.innerHTML = `
      <span class="fable-save-resume-latest-main">
        <span class="fable-save-resume-latest-label">Resume Latest</span>
        ${autosave.summary ? `<span class="fable-save-resume-latest-summary">${esc(autosave.summary)}</span>` : ''}
      </span>
      <span class="fable-save-resume-latest-meta">${autosave.turn_count ? autosave.turn_count + ' turns · ' : ''}${fmtTime(autosave.timestamp)}</span>
    `;
    resume.addEventListener('click', () => onSelect(autosave));
    host.appendChild(resume);
  }

  if (!manuals.length) {
    // No manual saves. If the Resume Latest button is shown, a soft hint
    // suffices; otherwise this world has no saves at all.
    const empty = document.createElement('div');
    empty.className = 'fable-saves-empty';
    empty.innerHTML = autosave
      ? `<p>No manual saves yet for this world.</p>`
      : `<p>No saved fable yet for this world.</p>`;
    host.appendChild(empty);
    return;
  }

  // (2026-08-15 audit fix) delete is destructive + was one unrecoverable click
  // with errors swallowed. Armed two-click confirm (native confirm() is dead
  // in wry): first click arms ("Sure?" + danger styling stays), second click
  // within 5s executes. Only ONE armed button at a time — arming another
  // disarms the previous. Failures surface on the inline status line.
  let armedBtn = null;
  let armTimer = 0;
  const disarm = () => {
    clearTimeout(armTimer);
    if (armedBtn) {
      armedBtn.dataset.armed = '';
      armedBtn.textContent = 'Delete';
      armedBtn = null;
    }
  };
  const showStatus = (text) => {
    const el = root.querySelector('[data-status]');
    if (!el) return;
    el.textContent = text;
    el.hidden = !text;
  };

  for (const save of manuals) {
    const row = document.createElement('div');
    row.className = 'fable-save-row';
    row.innerHTML = `
      <div class="fable-save-info">
        <div class="fable-save-name">${esc(save.name)}</div>
        ${save.summary ? `<div class="fable-save-summary">${esc(save.summary)}</div>` : ''}
        <div class="fable-save-meta">${save.turn_count ? save.turn_count + ' turns · ' : ''}${fmtTime(save.timestamp)}</div>
      </div>
      <div class="fable-save-actions">
        <button class="fable-save-btn" data-act="load">Load</button>
        <button class="fable-save-btn danger" data-act="del">Delete</button>
      </div>
    `;
    row.querySelector('[data-act="load"]').addEventListener('click', () => onSelect(save));
    row.querySelector('[data-act="del"]').addEventListener('click', async (e) => {
      const btn = e.currentTarget;
      if (btn !== armedBtn) disarm();
      if (btn.dataset.armed !== '1') {
        btn.dataset.armed = '1';
        btn.textContent = 'Sure?';
        armedBtn = btn;
        clearTimeout(armTimer);
        armTimer = setTimeout(disarm, 5000);
        return;
      }
      disarm();
      try {
        await deleteSave(cardId, save.save_id);
        showStatus('');
      } catch (err) {
        // (2026-08-16 yellow J8) showStatus renders via textContent — the
        // caller-side esc() double-escaped (a `&` in the backend error showed
        // as literal `&amp;`).
        showStatus(`Delete failed: ${err}`);
      }
      renderSaves(root, cardId, onSelect, cardName);
    });
    host.appendChild(row);
  }
}
