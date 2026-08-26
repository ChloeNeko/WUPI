// =============================================================
// SESSION MANAGER — the centered playthrough list (2026-08-22
// session decoupling). Clicking LOAD on a card modal opens this
// popup over it: every playthrough of the card is an isolated
// session folder under apps/fable/data/saves/<CardName>/, and this
// lists them from fable_sessions_list:
//   { session_id, card_id, name, created_at, last_played, save_count }
//
// Row actions:
//   CONTINUE — resume the session's newest save (autosave included;
//              the saves list is newest-first). A saveless session
//              resumes its live state (sessionId + null saveId).
//   SAVES    — the existing saves popup, scoped to the session.
//   BRANCH   — (2026-08-24 Part II D1) fork the playthrough: session
//              folder + episodic memory partition copy into a fresh
//              session (non-destructive — no confirm; toast on success).
//   DELETE   — armed two-click confirm; removes the whole session
//              folder (the backend refuses the active session).
// Plus a NEW SESSION button → the new-game flow preset to this card
// (the backend mints the session folder at launch).
//
// AESTHETIC: the saves popup's chrome (black panel, brass-dim
// border, uppercase brass title, red-tinted ✕; z-50 over the card
// modal). Esc / backdrop click / ✕ all close.
// =============================================================

import { listSessions, deleteSession, branchSession } from '../engine/saves-io.js';

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
//   cardId      — whose sessions are listed
//   cardName    — shown in the popup title ("<Name> — Sessions")
//   onContinue  — receives the chosen SessionMeta
//   onOpenSaves — receives the chosen SessionMeta (opens the saves popup)
//   onNewSession — invoked for the NEW SESSION button

// (2026-08-24 review fix) The ONE sanctioned closer — the npc-dossier
// pattern. Both the one-at-a-time sweep in open and closeSessionsModal run
// the live popup's FULL teardown; the old bare `el.remove()` sweeps
// orphaned the document-capture Esc handler, which later stole an Escape
// from whatever modal was open next.
let activeSessionsClose = null;

export function openSessionsModal(host, opts) {
  if (!host) return;
  // One popup at a time: run the live popup's full teardown first.
  closeSessionsModal();
  host.querySelectorAll('.fable-sessions-popup-overlay').forEach((el) => el.remove());

  const overlay = document.createElement('div');
  overlay.className = 'fable-sessions-popup-overlay';
  overlay.innerHTML = `
    <div class="fable-sessions-popup-backdrop" aria-hidden="true"></div>
    <div class="fable-sessions-popup" role="dialog" aria-modal="true" aria-label="Sessions">
      <div class="fable-sessions-popup-head">
        <span class="fable-sessions-popup-title"></span>
        <button type="button" class="fable-sessions-popup-close" aria-label="Close sessions">✕</button>
      </div>
      <div class="fable-sessions-popup-body" data-host></div>
      <div class="fable-sessions-popup-foot">
        <button type="button" class="fable-sessions-popup-new" data-act="new">+ New Session</button>
      </div>
      <!-- Inline error surface: a swallowed delete failure must be visible. -->
      <p class="fable-sessions-popup-status" data-status hidden></p>
    </div>`;
  host.appendChild(overlay);
  overlay.querySelector('.fable-sessions-popup-title').textContent =
    opts.cardName ? `${opts.cardName} — Sessions` : 'Sessions';

  const onBackdrop = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-sessions-popup-backdrop')) close();
  };
  // Esc on the document (capture + stopPropagation so the card modal
  // underneath doesn't ALSO close — the raw-editor discipline).
  const onEsc = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); close(); }
  };
  function close() {
    if (activeSessionsClose === close) activeSessionsClose = null;
    overlay.removeEventListener('click', onBackdrop);
    document.removeEventListener('keydown', onEsc, { capture: true });
    overlay.remove();
  }
  activeSessionsClose = close;
  overlay.addEventListener('click', onBackdrop);
  document.addEventListener('keydown', onEsc, { capture: true });
  overlay.querySelector('.fable-sessions-popup-close').addEventListener('click', close);
  overlay.querySelector('[data-act="new"]').addEventListener('click', () => {
    close();
    if (opts.onNewSession) opts.onNewSession();
  });

  renderSessionsList(overlay, opts, close);
}

// Remove any popup living on `host` (renderWorlds calls this on every screen
// entry — a popup left behind must not cover the grid).
// (2026-08-24 review fix) Runs the live popup's FULL teardown — the old bare
// element sweep orphaned the document-capture Esc handler.
export function closeSessionsModal() {
  if (activeSessionsClose) {
    const close = activeSessionsClose;
    activeSessionsClose = null;
    close();
  }
}

// Render the session rows: name + last-played + save count, with CONTINUE /
// SAVES / DELETE (armed two-click). Generation token against re-render
// races (the saves-popup discipline).
async function renderSessionsList(overlay, opts, close) {
  const host = overlay.querySelector('[data-host]');
  overlay._renderGen = (overlay._renderGen || 0) + 1;
  const gen = overlay._renderGen;
  host.innerHTML = '';
  let sessions = [];
  try {
    sessions = await listSessions(opts.cardId);
  } catch (err) {
    if (gen !== overlay._renderGen) return;
    host.innerHTML = `<div class="fable-sessions-empty">Couldn't load sessions: ${esc(err)}</div>`;
    return;
  }
  if (gen !== overlay._renderGen) return;

  const showStatus = (text) => {
    const el = overlay.querySelector('[data-status]');
    if (!el) return;
    el.textContent = text;
    el.hidden = !text;
  };

  if (!sessions.length) {
    const empty = document.createElement('div');
    empty.className = 'fable-sessions-empty';
    empty.innerHTML = `<p>No sessions yet for this world — start one below.</p>`;
    host.appendChild(empty);
    return;
  }

  // Armed two-click delete (the saves-popup discipline: one armed button at
  // a time, 5s disarm, failures on the inline status line).
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

  for (const session of sessions) {
    const row = document.createElement('div');
    row.className = 'fable-session-row';
    const meta = [
      session.save_count ? `${session.save_count} save${session.save_count === 1 ? '' : 's'}` : 'no saves',
      session.last_played ? `played ${fmtTime(session.last_played)}` : '',
    ].filter(Boolean).join(' · ');
    row.innerHTML = `
      <div class="fable-session-info">
        <div class="fable-session-name">${esc(session.name)}</div>
        <div class="fable-session-meta">${esc(meta)}</div>
      </div>
      <div class="fable-session-actions">
        <button class="fable-save-btn" data-act="continue">Continue</button>
        <button class="fable-save-btn" data-act="saves">Saves</button>
        <button class="fable-save-btn ghost" data-act="branch">Branch</button>
        <button class="fable-save-btn danger" data-act="del">Delete</button>
      </div>
    `;
    row.querySelector('[data-act="continue"]').addEventListener('click', () => {
      close();
      if (opts.onContinue) opts.onContinue(session);
    });
    row.querySelector('[data-act="saves"]').addEventListener('click', () => {
      close();
      if (opts.onOpenSaves) opts.onOpenSaves(session);
    });
    // (2026-08-24 Part II D1) BRANCH — non-destructive by design (copy,
    // never move): no confirm, the row disables while it runs, the status
    // line toasts the new session's name on success.
    row.querySelector('[data-act="branch"]').addEventListener('click', async (e) => {
      const btn = e.currentTarget;
      if (btn.disabled) return;
      btn.disabled = true;
      btn.textContent = 'Branching…';
      try {
        const meta = await branchSession(opts.cardId, session.session_id);
        showStatus(`Branched → ${meta && meta.name ? meta.name : 'new session'}`);
        renderSessionsList(overlay, opts, close);
      } catch (err) {
        showStatus(`Branch failed: ${err}`);
        btn.disabled = false;
        btn.textContent = 'Branch';
      }
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
        await deleteSession(opts.cardId, session.session_id);
        showStatus('');
      } catch (err) {
        // showStatus renders via textContent — no esc() here.
        showStatus(`Delete failed: ${err}`);
      }
      renderSessionsList(overlay, opts, close);
    });
    host.appendChild(row);
  }
}
