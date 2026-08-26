// =============================================================
// PANEL: CHRONICLE (2026-08-24 Part II C3) — the pin/rollback
// surface over the card's episodic memory turns. Rows are
// turn-uuid-grouped archive records (snippet + time + chunk count
// + pin state) over ONE `memory_turns_list` fetch; PIN/UNPIN flips
// `memory_set_pinned`, ROLLBACK sits behind the count-first confirm
// (the delete-modal pattern — rollback is destructive) over
// `memory_rollback_turn`. Two entries: the Wupi-drawer brand-header
// book tile left of the playground wand (`openChroniclePanel`, the
// backgrounds-overlay pattern — moved there 2026-08-25 off the
// removed 6th foot tool) and the Wupi-chat summon route
// (`renderChronicle` via manager classifyFocus 'chronicle|memories').
// =============================================================

import { invoke } from '@tauri-apps/api/core';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function fmtTime(unixMs) {
  if (!unixMs) return '';
  try {
    return new Date(unixMs < 1e12 ? unixMs * 1000 : unixMs).toLocaleString();
  } catch (_) {
    return '';
  }
}

/// PURE: the view model over `memory_turns_list` rows — newest first, the
/// header line. Exported for tests.
export function buildChronicleModel(rows) {
  const list = Array.isArray(rows) ? rows : [];
  const turns = list.map((r) => ({
    turn_uuid: r.turn_uuid || '',
    snippet: String(r.snippet || '').trim() || '(no text)',
    time: fmtTime(r.timestamp),
    pinned: !!r.pinned,
    chunks: Number(r.chunks || 0),
  }));
  return {
    header: turns.length ? 'Saved memories for this story' : 'No saved memories yet',
    turns,
  };
}

function rowHtml(t) {
  return `
    <div class="fable-chronicle-row${t.pinned ? ' is-pinned' : ''}" data-turn="${esc(t.turn_uuid)}" data-chunks="${t.chunks}">
      <div class="fable-chronicle-row-main">
        <div class="fable-chronicle-snippet">${esc(t.snippet)}</div>
        <div class="fable-chronicle-meta">${esc(t.time)} · ${t.chunks} part${t.chunks === 1 ? '' : 's'}${t.pinned ? ' · PINNED' : ''}</div>
      </div>
      <div class="fable-chronicle-row-actions">
        <button type="button" class="fable-save-btn ghost fable-chronicle-pin" data-act="pin">${t.pinned ? 'UNPIN' : 'PIN'}</button>
        <button type="button" class="fable-save-btn danger fable-chronicle-rollback" data-act="rollback">ROLLBACK</button>
      </div>
    </div>`;
}

function bodyHtml(model) {
  if (!model.turns.length) {
    return `<div class="panel-empty">
      <p>Nothing here yet.</p>
      <p class="panel-empty-hint">Play a few turns and every exchange will be saved here.</p>
    </div>`;
  }
  return `<div class="fable-chronicle-list">${model.turns.map(rowHtml).join('')}</div>`;
}

/// Mount the live chronicle into `container` (async fetch + render + wire).
/// Shared by both entry points; idempotent per container.
export async function mountChronicle(container) {
  if (!container) return;
  container.querySelectorAll('[data-chronicle-rows]').forEach((el) => el.remove());
  const shell = document.createElement('div');
  shell.className = 'fable-chronicle';
  shell.dataset.chronicleRows = '';
  shell.innerHTML = `<div class="panel-head">
      <h2>The Chronicle</h2>
      <p class="panel-hint" data-chronicle-head>Reading the archive…</p>
    </div>
    <div class="fable-chronicle-body" data-chronicle-body><div class="fable-chronicle-loading">Reading the archive…</div></div>`;
  container.appendChild(shell);

  let rows;
  try {
    // (2026-08-25) 500 = the backend's clamp max, deliberately asked for:
    // the retention window (2000-chunk cap ≈ ≤500 live turns at typical
    // multi-chunk density) stays fully reachable, because the OLDEST turns
    // are exactly the FIFO eviction candidates a PIN protects — a 100-fetch
    // hid them behind an unreachable cliff.
    rows = await invoke('memory_turns_list', { limit: 500 });
  } catch (err) {
    shell.querySelector('[data-chronicle-head]').textContent = String(err);
    shell.querySelector('[data-chronicle-body]').innerHTML =
      '<div class="fable-chronicle-loading">Memory unavailable.</div>';
    return;
  }
  const model = buildChronicleModel(rows);
  shell.querySelector('[data-chronicle-head]').textContent = model.header;
  const body = shell.querySelector('[data-chronicle-body]');
  body.innerHTML = bodyHtml(model);

  // PIN / UNPIN — immediate flip + re-render of the row's state.
  body.addEventListener('click', async (e) => {
    const btn = e.target.closest('[data-act="pin"]');
    if (!btn) return;
    const row = btn.closest('[data-turn]');
    const turnUuid = row.dataset.turn || '';
    const pinned = !row.classList.contains('is-pinned');
    btn.disabled = true;
    try {
      await invoke('memory_set_pinned', { turnUuid, pinned });
      row.classList.toggle('is-pinned', pinned);
      btn.textContent = pinned ? 'UNPIN' : 'PIN';
      const meta = row.querySelector('.fable-chronicle-meta');
      if (meta) {
        meta.textContent = meta.textContent.replace(/ · PINNED$/, '');
        if (pinned) meta.textContent += ' · PINNED';
      }
    } catch (err) {
      console.warn('[chronicle] pin failed:', err);
      rowError(row, 'Pin', err);
    } finally {
      btn.disabled = false;
    }
  });

  // ROLLBACK — the count-first confirm (the delete-modal pattern). The
  // preview IS the row's own chunk count: rollback deletes exactly this
  // turn's rows (turn-atomic — the same granularity pin + eviction use).
  // (2026-08-24 review P2) In-flight disabled (a second confirm-click used
  // to double-fire the IPC — Rust errors the second, but the UI spun) +
  // failures surface INLINE on the row, not just the console.
  const rowError = (row, label, err) => {
    if (!row) return;
    let el = row.querySelector('.fable-chronicle-row-error');
    if (!el) {
      el = document.createElement('div');
      el.className = 'fable-chronicle-row-error';
      row.appendChild(el);
    }
    el.textContent = `${label} failed: ${err}`;
  };
  body.addEventListener('click', async (e) => {
    const btn = e.target.closest('[data-act="rollback"]');
    if (!btn || btn.disabled) return;
    if (btn.dataset.armed !== '1') {
      btn.dataset.armed = '1';
      const row = btn.closest('[data-turn]');
      const n = Number((row && row.dataset.chunks) || 0);
      btn.textContent = `REMOVE ${n} PART${n === 1 ? '' : 'S'}?`;
      // The timer handle lives on the button: arm + confirm are separate
      // events, so a local variable can't carry it across.
      btn._disarmTimer = setTimeout(() => {
        btn._disarmTimer = 0;
        btn.dataset.armed = '';
        btn.textContent = 'ROLLBACK';
      }, 5000);
      return;
    }
    // Confirmed — cancel the pending disarm timer first: left running it
    // stamped 'ROLLBACK' back over 'REMOVING…' while the IPC was still in
    // flight (the slow-delete label flicker).
    clearTimeout(btn._disarmTimer);
    btn._disarmTimer = 0;
    btn.dataset.armed = '';
    const row = btn.closest('[data-turn]');
    const turnUuid = (row && row.dataset.turn) || '';
    btn.disabled = true;
    btn.textContent = 'REMOVING…';
    try {
      await invoke('memory_rollback_turn', { turnUuid });
      if (row && row.parentElement) row.remove();
    } catch (err) {
      console.warn('[chronicle] rollback failed:', err);
      btn.textContent = 'ROLLBACK';
      btn.disabled = false;
      rowError(row, 'Rollback', err);
    }
  });
}

/// The manager-summon route: sync shell the panel host mounts; the live
/// mount rides a microtask (the host's innerHTML is set right after this
/// returns, so the query below finds it mounted).
export function renderChronicle() {
  queueMicrotask(() => {
    void mountChronicle(document.querySelector('[data-chronicle-mount]'));
  });
  return '<div class="fable-chronicle-mount" data-chronicle-mount></div>';
}

/// The foot-icon route: a stage-appended centered overlay (the backgrounds
/// panel pattern — backdrop, Esc + ✕ close, z:46 above the drawer's 40).
/// (2026-08-24 review P1) `closeChroniclePanel` is the ONE sanctioned closer:
/// a module-level ref so re-opening (and teardownStage) runs the PREVIOUS
/// overlay's full teardown — the old bare `el.remove()` sweep orphaned the
/// document-capture Esc handler, which later stole an Escape from whatever
/// modal was open next.
let activeChronicleClose = null;

export function closeChroniclePanel() {
  if (activeChronicleClose) {
    const close = activeChronicleClose;
    activeChronicleClose = null;
    close();
  }
}

export function openChroniclePanel(root) {
  const host = root || document;
  closeChroniclePanel();
  host.querySelectorAll('[data-chronicle-overlay]').forEach((el) => el.remove());
  const overlay = document.createElement('div');
  overlay.className = 'fable-chronicle-overlay';
  overlay.dataset.chronicleOverlay = '';
  overlay.hidden = true;
  overlay.innerHTML = `
    <div class="fable-chronicle-backdrop" data-chronicle-backdrop></div>
    <div class="fable-chronicle-modal" role="dialog" aria-modal="true" aria-label="The Chronicle">
      <button type="button" class="fable-chronicle-close" data-chronicle-close aria-label="Close chronicle">✕</button>
      <div class="fable-chronicle-modal-body" data-chronicle-modal-body></div>
    </div>`;
  host.appendChild(overlay);
  const onEsc = (e) => {
    if (e.key === 'Escape') {
      e.stopPropagation();
      close();
    }
  };
  const close = () => {
    if (activeChronicleClose === close) activeChronicleClose = null;
    document.removeEventListener('keydown', onEsc, { capture: true });
    overlay.classList.remove('is-open');
    const finish = () => overlay.remove();
    overlay.addEventListener('transitionend', finish, { once: true });
    setTimeout(finish, 260);
  };
  activeChronicleClose = close;
  overlay.querySelector('[data-chronicle-close]').addEventListener('click', close);
  overlay.querySelector('[data-chronicle-backdrop]').addEventListener('click', close);
  document.addEventListener('keydown', onEsc, { capture: true });
  overlay.hidden = false;
  void overlay.offsetWidth;
  overlay.classList.add('is-open');
  void mountChronicle(overlay.querySelector('[data-chronicle-modal-body]'));
}
