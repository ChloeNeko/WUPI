// =============================================================
// SAVES POPUP — the centered save-slot list (2026-08-20 Chloe
// ruling: the saves SCREEN page is retired — clicking LOAD on a
// card modal opens this popup over it instead of navigating to a
// new page; the card modal re-emerges when the popup closes).
//
// Lists SaveMeta for one playthrough session from fable_list_saves:
//   { save_id, session_id, name, summary, timestamp, turn_count, is_autosave }
//
// The per-turn AUTOSAVE is promoted to a one-click "Resume Latest" button at
// the top of the list — it IS the session's latest state. (Resume Latest
// reuses the autosave, which is exactly what CONTINUE on the title screen
// resumes too.) The list below shows the MANUAL saves only (most-recent
// first; the backend sorts by timestamp desc). Each manual row is Load +
// Delete (armed two-click confirm); the autosave button is resume-only
// (deleting it is pointless — the next turn writes a fresh one).
//
// AESTHETIC: the raw-XML editor modal's chrome (fable.js openXmlEditorModal)
// — black panel, brass-dim border, uppercase brass title, red-tinted ✕ —
// with the rows keeping their existing .fable-save-* look. Esc / backdrop
// click / ✕ all close. Load → onSelect(save) → resumeSave (fable.js).
// =============================================================

import { listSaves, deleteSave } from '../engine/saves-io.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function fmtTime(ms) {
  if (!ms) return '';
  try {
    const d = new Date(ms);
    return d.toLocaleString();
  } catch (_) { return ''; }
}

// Open the popup on `host` (the worlds screen — the card modal stays open
// behind it). opts:
//   cardId   — the partition whose saves are listed (+ deletes)
//   sessionId — (2026-08-22) the playthrough whose slots are listed
//   cardName — shown in the popup title ("<Name> — Saves")
//   onSelect — receives the chosen SaveMeta
export function openSavesModal(host, opts) {
  if (!host) return;
  // One popup at a time (the raw-editor pattern: a leftover can only exist
  // if a close was interrupted, so a plain replace is enough).
  host.querySelectorAll('.fable-saves-popup-overlay').forEach((el) => el.remove());

  const overlay = document.createElement('div');
  overlay.className = 'fable-saves-popup-overlay';
  overlay.innerHTML = `
    <div class="fable-saves-popup-backdrop" aria-hidden="true"></div>
    <div class="fable-saves-popup" role="dialog" aria-modal="true" aria-label="Saves">
      <div class="fable-saves-popup-head">
        <span class="fable-saves-popup-title"></span>
        <button type="button" class="fable-saves-popup-close" aria-label="Close saves">✕</button>
      </div>
      <div class="fable-saves-popup-body" data-host></div>
      <!-- Inline error surface: a swallowed delete failure must be visible
           (the popup has no toast of its own). -->
      <p class="fable-saves-popup-status" data-status hidden></p>
    </div>`;
  host.appendChild(overlay);
  overlay.querySelector('.fable-saves-popup-title').textContent =
    opts.cardName ? `${opts.cardName} — Saves` : 'Saves';

  const onBackdrop = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-saves-popup-backdrop')) close();
  };
  // Esc on the document (capture + stopPropagation so the card modal
  // underneath doesn't ALSO close — the raw-editor discipline; the worlds
  // modal's own Esc handler defers while this overlay exists).
  const onEsc = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); close(); }
  };
  function close() {
    overlay.removeEventListener('click', onBackdrop);
    document.removeEventListener('keydown', onEsc, { capture: true });
    overlay.remove();
  }
  overlay.addEventListener('click', onBackdrop);
  document.addEventListener('keydown', onEsc, { capture: true });
  overlay.querySelector('.fable-saves-popup-close').addEventListener('click', close);

  renderSavesList(overlay, opts, close);
}

// Remove any popup living on `host` (renderWorlds calls this on every screen
// entry — a popup left behind by a resumed game must not cover the grid).
export function closeSavesModal(host) {
  if (!host) return;
  host.querySelectorAll('.fable-saves-popup-overlay').forEach((el) => el.remove());
}

// Render the list into an open popup. Same shape as the retired screen's
// renderer: Resume Latest (autosave) on top, manual rows beneath, armed
// two-click delete, generation token against re-render races. `close` is the
// popup's own teardown (listener-clean) — the select paths use it so the
// document Esc handler never outlives the overlay.
async function renderSavesList(overlay, opts, close) {
  const host = overlay.querySelector('[data-host]');
  overlay._renderGen = (overlay._renderGen || 0) + 1;
  const gen = overlay._renderGen;
  host.innerHTML = '';
  let saves = [];
  try {
    saves = await listSaves(opts.cardId, opts.sessionId);
  } catch (err) {
    if (gen !== overlay._renderGen) return;
    host.innerHTML = `<div class="fable-saves-empty">Couldn't load saves: ${esc(err)}</div>`;
    return;
  }
  if (gen !== overlay._renderGen) return;

  const showStatus = (text) => {
    const el = overlay.querySelector('[data-status]');
    if (!el) return;
    el.textContent = text;
    el.hidden = !text;
  };

  // The autosave is the per-turn checkpoint = the world's latest state. Promote
  // it to a one-click Resume Latest button at the top; the list below shows
  // the manual saves only.
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
    resume.addEventListener('click', () => { close(); opts.onSelect(autosave); });
    host.appendChild(resume);
  }

  if (!manuals.length) {
    // No manual saves. If the Resume Latest button is shown, a soft hint
    // suffices; otherwise this world has no saves at all.
    const empty = document.createElement('div');
    empty.className = 'fable-saves-empty';
    empty.innerHTML = autosave
      ? `<p>No manual saves yet for this session.</p>`
      : `<p>No saved fable yet for this session.</p>`;
    host.appendChild(empty);
    return;
  }

  // Delete is destructive + must not be one unrecoverable click with errors
  // swallowed. Armed two-click confirm (native confirm() is dead in wry):
  // first click arms ("Sure?" + danger styling stays), second click within 5s
  // executes. Only ONE armed button at a time — arming another disarms the
  // previous. Failures surface on the inline status line.
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
    row.querySelector('[data-act="load"]').addEventListener('click', () => {
      close();
      opts.onSelect(save);
    });
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
        await deleteSave(opts.cardId, opts.sessionId, save.save_id);
        showStatus('');
      } catch (err) {
        // showStatus renders via textContent — no esc() here (a `&` in the
        // backend error must not double-escape).
        showStatus(`Delete failed: ${err}`);
      }
      renderSavesList(overlay, opts, close);
    });
    host.appendChild(row);
  }
}
