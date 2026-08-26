// =============================================================
// SCREEN: CODEX LIBRARY — the title LOAD → CODEX browser (2026-08-23).
//
// The universal codex library (`apps/fable/data/codex/`, §6.5) browsed
// OUTSIDE the new-game flow: text-only tiles (no portraits — a codex is
// authored lore, not a character) in the same mini-card chrome the player /
// worlds grids use. Each tile shows the codex NAME with its entry count in
// small gray text beside it, and the display names of the CARDS that
// currently link it ("None" when unlinked).
//
// Clicking a tile expands the SAME centered modal language: every entry
// (title / tags / body) in a scrollable column, with three actions —
//   • LINK   → the INVERTED link popup: a centered multi-select of SIM
//     cards (pre-checked = currently linked). LINK writes each changed
//     card's `<linked_codices>` through fable_codex_link_set (this codex
//     appended at LOWEST priority on a new link, position preserved on a
//     kept one, removed on a deselection — every other link untouched).
//   • EDIT   → the name + raw-compound editor: the codex NAME is editable
//     (fable_codex_file_rename rewrites the file + every card's
//     linked_codices mention), and the raw text (titles / tags / bodies)
//     saves through fable_codex_file_write.
//   • DELETE → confirm → fable_codex_file_delete (file + every card link
//     swept server-side).
// Every mutation lands back on the refreshed grid (renderCodexLibrary).
//
// The ‹ / ⌂ chrome is owned by the flow controller (fable.js); ‹ routes to
// the LOAD three-way split. Like the other picker screens, this screen
// paints NO background — the persistent flow ambiance shows through.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

function esc(s) {
  return String(s == null ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

export function buildCodexLibrary() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-player-picker-screen';
  root.dataset.fableScreen = 'codex-library';
  root.hidden = true;
  // The grid + modal + confirm reuse the shared picker-screen structural
  // classes (the exact language worlds.js / player-picker.js use) so the
  // chrome reads identically; codex-specific looks live in the
  // .fable-codexlib-* rules (fable.css).
  root.innerHTML = `
    <div class="fable-codexlib-grid" data-host></div>
    <div class="fable-player-modal-overlay" data-modal hidden>
      <div class="fable-player-modal-backdrop" data-modal-backdrop></div>
      <div class="fable-player-modal fable-codexlib-modal" data-modal-card role="dialog" aria-modal="true"></div>
    </div>
    <div class="fable-player-confirm" data-confirm hidden>
      <div class="fable-player-confirm-card">
        <p data-confirm-msg></p>
        <div class="fable-player-confirm-actions">
          <button type="button" data-confirm-yes>Delete</button>
          <button type="button" data-confirm-no>Cancel</button>
        </div>
      </div>
    </div>
  `;
  return root;
}

// ── Pure helpers (exported for tests/codex-library.test.mjs) ──────────────

// Fold the `fable_codex_links_map` rows into the inverse index the tiles
// read: codex name (lowercased — links are case-insensitive display stems)
// → [{ cardId, cardName }] in card-name order (the IPC sorts by card name).
export function buildCodexLinkIndex(linksMap) {
  const index = new Map();
  for (const row of linksMap || []) {
    for (const name of (row && row.codices) || []) {
      const key = String(name).trim().toLowerCase();
      if (!key) continue;
      if (!index.has(key)) index.set(key, []);
      index.get(key).push({ cardId: row.card_id, cardName: row.card_name });
    }
  }
  return index;
}

// The LINK popup's save plan: one { cardId, codices } write per card whose
// list changes. A card that keeps its current state (linked + selected, or
// unlinked + deselected) is NOT written — its priority order stays exactly
// as-authored. A NEW link appends at the END (lowest priority — the
// right-drawer Codex tab owns re-ordering); a deselection removes just this
// codex, preserving the relative order of every other link.
export function computeLinkWrites(codexName, cards, linksByCardId, selectedCardIds) {
  const needle = String(codexName || '').trim().toLowerCase();
  const sel = new Set(selectedCardIds || []);
  const writes = [];
  for (const card of cards || []) {
    const current = (linksByCardId && linksByCardId.get(card.id)) || [];
    const linkedAt = current.findIndex((n) => String(n).trim().toLowerCase() === needle);
    const isSelected = sel.has(card.id);
    if (isSelected && linkedAt !== -1) continue;   // stays linked — untouched
    if (!isSelected && linkedAt === -1) continue;  // stays unlinked — untouched
    if (isSelected) {
      writes.push({ cardId: card.id, codices: [...current, String(codexName).trim()] });
    } else {
      writes.push({
        cardId: card.id,
        codices: current.filter((n) => String(n).trim().toLowerCase() !== needle),
      });
    }
  }
  return writes;
}

// ── The grid ──────────────────────────────────────────────────────────────

// Populate the grid. Called on every screen entry (links change with every
// link_set / rename / delete, and the library grows from the Creator).
export async function renderCodexLibrary(root) {
  const host = root.querySelector('[data-host]');
  // Tear down any leftover link popup / editor from a prior view (they live
  // on the screen ROOT, above the grid — a ‹ away mid-popup leaves them
  // mounted inside the hidden screen otherwise). Each overlay registers its
  // own close here; running it removes the element AND its document-level
  // Esc listener.
  (root._codexOverlays || []).forEach((close) => { try { close(); } catch (_) {} });
  root._codexOverlays = [];
  // (2026-08-23 audit fix) A delete-confirm left open by a ‹ exit must not
  // survive re-entry — its live Yes listener would delete a codex the user
  // may no longer expect under the freshly rendered grid.
  if (typeof root._confirmTeardown === 'function') {
    try { root._confirmTeardown(); } catch (_) {}
    root._confirmTeardown = null;
  }
  // Generation token (renderWorlds' pattern): a stale `fable_codex_links_map`
  // completion must abandon before appending, or two loads' tiles land.
  root._renderGen = (root._renderGen || 0) + 1;
  const gen = root._renderGen;
  host.innerHTML = '';
  closeModal(root);
  let library = [];
  let linksMap = [];
  try {
    [library, linksMap] = await Promise.all([
      invoke('fable_codex_library_list'),
      invoke('fable_codex_links_map'),
    ]);
  } catch (err) {
    if (gen !== root._renderGen) return;
    host.innerHTML = `<div class="fable-flow-empty"><p>Couldn't load the codex library: ${esc(err)}</p></div>`;
    return;
  }
  if (gen !== root._renderGen) return;
  root._linksMap = linksMap || [];
  if (!library.length) {
    host.innerHTML = `<div class="fable-flow-empty">
      <p>No codex files in the library yet.</p>
      <p class="fable-flow-empty-hint">Create one during New Game (the CODEX step) or import a lorebook.</p>
    </div>`;
    return;
  }
  const index = buildCodexLinkIndex(linksMap);
  for (const item of library) {
    const linked = index.get(String(item.name).trim().toLowerCase()) || [];
    const tile = document.createElement('button');
    tile.className = 'fable-player-mini-card fable-codexlib-tile';
    tile.type = 'button';
    tile.dataset.codexName = item.name;
    tile.setAttribute('aria-label', `View codex ${item.name}`);
    tile.innerHTML = `
      <div class="fable-codexlib-name-row">
        <span class="fable-codexlib-name">${esc(item.name)}</span>
        <span class="fable-codexlib-count">${Number(item.entry_count) || 0} ${Number(item.entry_count) === 1 ? 'entry' : 'entries'}</span>
      </div>
      <div class="fable-player-mini-divider" aria-hidden="true"></div>
      <div class="fable-codexlib-links">${esc(linked.length ? linked.map((l) => l.cardName).join(' · ') : 'None')}</div>`;
    tile.addEventListener('click', () => openDetail(root, item));
    host.appendChild(tile);
  }
}

// ── The detail modal (entries + LINK / EDIT / DELETE) ─────────────────────

function closeModal(root) {
  const overlay = root.querySelector('[data-modal]');
  if (!overlay) return;
  // ALWAYS release the double-open guard — even on the early return (the
  // "popup dead until restart" bug class the other pickers guard against).
  const wasOpen = !overlay.hidden;
  root._modalOpen = false;
  if (!wasOpen) return;
  if (root._modalCleanup) root._modalCleanup();
  overlay.classList.remove('is-open');
  root._modalGen = (root._modalGen || 0) + 1;
  const gen = root._modalGen;
  const finish = () => { if (root._modalGen !== gen) return; overlay.hidden = true; };
  overlay.addEventListener('transitionend', finish, { once: true });
  setTimeout(finish, 260);
}

async function openDetail(root, item) {
  if (root._modalOpen) return;
  root._modalOpen = true;
  root._actionConsumed = false;
  root._modalGen = (root._modalGen || 0) + 1;
  const overlay = root.querySelector('[data-modal]');
  const card = root.querySelector('[data-modal-card]');
  card.innerHTML = `<div class="fable-player-modal-loading">Loading…</div>`;
  overlay.hidden = false;
  void overlay.offsetWidth;
  overlay.classList.add('is-open');

  const onBackdropClick = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-player-modal-backdrop')) {
      closeModal(root);
    }
  };
  const onEsc = (e) => {
    // Don't steal Esc while a higher overlay (the link popup / the editor /
    // the confirm) is open above this modal — same capture-phase discipline
    // as worlds.js.
    if (root.querySelector('.fable-codex-link-overlay, .fable-raw-editor-overlay, .fable-player-confirm.is-open')) return;
    if (e.key === 'Escape') { e.stopPropagation(); closeModal(root); }
  };
  overlay.addEventListener('click', onBackdropClick);
  document.addEventListener('keydown', onEsc, { capture: true });
  root._modalCleanup = () => {
    overlay.removeEventListener('click', onBackdropClick);
    document.removeEventListener('keydown', onEsc, { capture: true });
    root._modalCleanup = null;
  };

  let data = null;
  try {
    data = await invoke('fable_codex_file_read', { name: item.name });
  } catch (err) {
    card.innerHTML = `<div class="fable-player-modal-loading">Couldn't load: ${esc(err)}</div>`;
    return;
  }
  card.innerHTML = renderDetailCard(item, data);

  // One-per-open latch on the layering actions (the worlds.js discipline):
  // closeModal's hide is deferred through the ~260ms fade, so the second
  // click of a double-click still lands on live buttons. LINK / EDIT keep
  // the modal open under their overlay → the latch releases so the buttons
  // are live again once the overlay closes. DELETE isn't latched (a declined
  // confirm must stay retryable).
  const consumeOnce = (fn) => () => {
    if (root._actionConsumed) return;
    root._actionConsumed = true;
    fn();
  };
  card.querySelector('[data-codex-link]').addEventListener('click', consumeOnce(() => {
    openLinkPopup(root, item);
    root._actionConsumed = false;
  }));
  card.querySelector('[data-codex-edit]').addEventListener('click', consumeOnce(() => {
    openCodexEditor(root, item, data && data.raw != null ? data.raw : '');
    root._actionConsumed = false;
  }));
  card.querySelector('[data-codex-delete]').addEventListener('click', () => {
    confirmDelete(root, item);
  });
}

function renderDetailCard(item, data) {
  const entries = (data && Array.isArray(data.entries)) ? data.entries : [];
  const count = entries.length;
  const entriesHTML = count
    ? entries.map((e) => `
      <div class="fable-codexlib-entry">
        <div class="fable-codexlib-entry-title">${esc(e.title)}</div>
        ${e.tags && e.tags.length ? `<div class="fable-codexlib-entry-tags">${esc(e.tags.join(' · '))}</div>` : ''}
        <div class="fable-codexlib-entry-body">${esc(e.body)}</div>
      </div>`).join('')
    : '<div class="fable-codexlib-entry-empty">This codex has no entries.</div>';
  return `
    <div class="fable-codexlib-detail-head">
      <span class="fable-codexlib-detail-name">${esc(item.name)}</span>
      <span class="fable-codexlib-count">${count} ${count === 1 ? 'entry' : 'entries'}</span>
    </div>
    <div class="fable-codexlib-detail-body">${entriesHTML}</div>
    <div class="fable-player-modal-actions">
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--load" data-codex-link>LINK</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--edit" data-codex-edit>EDIT</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--delete" data-codex-delete>DELETE</button>
    </div>`;
}

// ── LINK → the inverted multi-select popup (cards for THIS codex) ────────

// (2026-08-24 review fix) The library link popup's sanctioned closer — the
// npc-dossier pattern. The old one-at-a-time sweep was ALSO document-wide
// (`.fable-codex-link-overlay` is shared with the new-game flow's picker in
// fable.js), so a bare remove could orphan the NEW-GAME popup's Esc guard;
// and its own re-open orphaned THIS popup's guard. Scope narrowed to `root`
// (this popup mounts there) + the full teardown runs first.
let activeLinkClose = null;

// Reuses the .fable-codex-link-* popup chrome (the new-game flow's LINK
// CODEX picker) with the rows inverted: SIM cards instead of codex files.
// Rows toggle; LINK persists exactly the changed cards (computeLinkWrites).
async function openLinkPopup(root, item) {
  // One popup at a time: run the live popup's full teardown first.
  if (activeLinkClose) {
    const close = activeLinkClose;
    activeLinkClose = null;
    close();
  }
  root.querySelectorAll('.fable-codex-link-overlay').forEach((n) => n.remove());
  let cards = [];
  try {
    cards = await invoke('fable_cards_list');
  } catch (e) {
    console.warn('[fable] codex link popup: fable_cards_list failed', e);
    return;
  }
  const linksByCard = new Map((root._linksMap || []).map((r) => [r.card_id, r.codices || []]));
  const needle = String(item.name).trim().toLowerCase();
  const selected = new Set(
    [...linksByCard.entries()]
      .filter(([, links]) => links.some((n) => String(n).trim().toLowerCase() === needle))
      .map(([id]) => id),
  );

  const overlay = document.createElement('div');
  overlay.className = 'fable-codex-link-overlay';
  // Inline failure note (self-contained — the screen never imports other
  // screens; a failed write keeps the popup open with the reason).
  const showError = (msg) => {
    let note = overlay.querySelector('.fable-codexlib-popup-error');
    if (!note) {
      note = document.createElement('div');
      note.className = 'fable-codexlib-popup-error';
      overlay.querySelector('.fable-codex-link-foot').prepend(note);
    }
    note.textContent = msg;
  };
  const escGuard = (ev) => {
    if (ev.key === 'Escape') { ev.stopPropagation(); close(); }
  };
  // (2026-08-23 audit fix) Declare `close` BEFORE the registration below —
  // the old order read it inside the array literal while still in its
  // temporal dead zone (ReferenceError on every LINK click, popup dead).
  const close = () => {
    if (activeLinkClose === close) activeLinkClose = null;
    document.removeEventListener('keydown', escGuard, true);
    overlay.remove();
  };
  activeLinkClose = close;
  // Register the teardown so a later renderCodexLibrary (screen re-entry)
  // closes this popup cleanly instead of leaving it mounted in the hidden
  // screen.
  root._codexOverlays = [...(root._codexOverlays || []), close];
  const subtypeLabel = (subtype) => {
    const v = (subtype || '').toLowerCase();
    return v === 'npc' ? 'NPC CARD' : v === 'scenario' ? 'SCENARIO CARD' : v === 'world' ? 'WORLD CARD' : '';
  };
  overlay.innerHTML = `
    <div class="fable-codex-link-modal" role="dialog" aria-label="Link codex to sim cards">
      <div class="fable-codex-link-head">
        <span class="fable-codex-link-title">LINK ${esc(item.name)}</span>
        <button class="fable-codex-link-close" type="button" aria-label="Close">✕</button>
      </div>
      <div class="fable-codex-link-body"></div>
      <div class="fable-codex-link-foot">
        <span class="fable-codex-link-hint">Linked cards seed this codex's lore at play</span>
        <button class="fable-codex-link-load" type="button">LINK</button>
      </div>
    </div>`;
  const body = overlay.querySelector('.fable-codex-link-body');
  const renderRows = () => {
    body.innerHTML = '';
    if (!cards.length) {
      body.innerHTML = '<div class="fable-codex-link-empty">No sim cards yet — create one through New Game first.</div>';
      return;
    }
    for (const card of cards) {
      const row = document.createElement('button');
      row.type = 'button';
      row.className = 'fable-codex-link-row' + (selected.has(card.id) ? ' is-selected' : '');
      const label = subtypeLabel(card.subtype);
      row.innerHTML = `
        <span class="fable-codex-link-row-check"></span>
        <span class="fable-codex-link-row-name"></span>
        <span class="fable-codex-link-row-count">${esc(label)}</span>`;
      row.querySelector('.fable-codex-link-row-name').textContent = card.name;
      row.addEventListener('click', () => {
        if (selected.has(card.id)) selected.delete(card.id);
        else selected.add(card.id);
        renderRows();
      });
      body.appendChild(row);
    }
  };
  renderRows();
  overlay.querySelector('.fable-codex-link-close').addEventListener('click', close);
  overlay.addEventListener('click', (ev) => { if (ev.target === overlay) close(); });
  overlay.querySelector('.fable-codex-link-load').addEventListener('click', async (ev) => {
    const btn = ev.currentTarget;
    btn.disabled = true;
    try {
      const writes = computeLinkWrites(item.name, cards, linksByCard, [...selected]);
      await Promise.all(writes.map((w) => invoke('fable_codex_link_set', { cardId: w.cardId, codices: w.codices })));
      close();
      // Land on the refreshed grid — the tiles' "linked cards" line is the
      // visible result of the link change.
      renderCodexLibrary(root);
    } catch (e) {
      btn.disabled = false;
      showError(`Link failed: ${e.message || e}`);
    }
  });
  document.addEventListener('keydown', escGuard, true);
  root.appendChild(overlay);
}

// ── EDIT → the name + raw-compound editor ────────────────────────────────

// The shared raw-editor frame (head + ✓/↻/✕ + textarea) with a NAME field
// above the text. Save: rename first when the name changed (the rename
// moves the file + rewrites every card link), then write the text under the
// canonical name. Failures keep the editor open (the 2026-08-12 silent-log
// ruling); success closes + refreshes the grid (a rename re-sorts it).
function openCodexEditor(root, item, raw) {
  // One editor at a time: run the live editor's FULL teardown first — a
  // bare overlay.remove() would orphan its document-capture keydown guard
  // (the exact leak class activeLinkClose exists to prevent).
  if (root._codexEditClose) {
    const prev = root._codexEditClose;
    root._codexEditClose = null;
    prev();
  }
  const existing = root.querySelector('.fable-codexlib-edit-overlay');
  if (existing) existing.remove();

  const overlay = document.createElement('div');
  overlay.className = 'fable-raw-editor-overlay fable-codexlib-edit-overlay';
  overlay.innerHTML = `
    <div class="fable-raw-editor-backdrop" aria-hidden="true"></div>
    <div class="fable-raw-editor-modal" role="dialog" aria-modal="true">
      <div class="fable-raw-editor-head">
        <span class="fable-raw-editor-title">${esc(`Edit ${item.name} — Codex`)}</span>
        <div class="fable-raw-editor-controls">
          <button type="button" class="fable-raw-btn save" data-raw-save>✓</button>
          <button type="button" class="fable-raw-btn revert" data-raw-revert>↻</button>
          <button type="button" class="fable-raw-btn close" data-raw-close>✕</button>
        </div>
      </div>
      <label class="fable-codexlib-edit-name-label" for="fable-codexlib-edit-name">Codex name</label>
      <input id="fable-codexlib-edit-name" class="fable-codexlib-edit-name" data-edit-name
             type="text" maxlength="64" spellcheck="false" autocomplete="off">
      <textarea class="fable-raw-editor-text" data-raw-text spellcheck="false"></textarea>
    </div>`;
  root.appendChild(overlay);

  const nameInput = overlay.querySelector('[data-edit-name]');
  const textarea = overlay.querySelector('[data-raw-text]');
  const saveBtn = overlay.querySelector('[data-raw-save]');
  const revertBtn = overlay.querySelector('[data-raw-revert]');
  const closeBtn = overlay.querySelector('[data-raw-close]');
  nameInput.value = item.name;
  textarea.value = raw != null ? raw : '';
  // The name the file currently sits under. A rename that succeeds but a
  // text write that FAILS must leave the editor retryable — the retry then
  // writes under the RENAMED file instead of erroring "no codex named <old>".
  let diskName = item.name;
  let lastGoodName = item.name;
  let lastGoodText = textarea.value;

  function validate() {
    const ok = nameInput.value.trim().length > 0;
    saveBtn.disabled = !ok;
    nameInput.classList.toggle('invalid', !ok);
  }
  nameInput.addEventListener('input', validate);
  validate();

  // (2026-08-25 fix) Document-capture keys — the sibling-modal discipline
  // (the link popup's escGuard, the detail modal's onEsc). The old
  // overlay-scoped keydown only fired while focus sat INSIDE the overlay:
  // focus starts on the underlying EDIT button and only reaches the name
  // field via the 50ms timer, and any later focus loss (a click on the
  // backdrop, a disabled save re-render) killed Esc + Ctrl+Enter for the
  // rest of the editor's life. stopPropagation on the closing arm keeps
  // deeper capture guards from also acting on the same Escape.
  const onKeys = (e) => {
    if (e.key === 'Escape') {
      if (nameInput.value === lastGoodName && textarea.value === lastGoodText) {
        e.stopPropagation();
        close();
      } else if (!saveBtn.disabled) {
        e.stopPropagation();
        close();
      }
    } else if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault(); save();
    }
  };
  function close() {
    if (root._codexEditClose === close) root._codexEditClose = null;
    document.removeEventListener('keydown', onKeys, true);
    overlay.remove();
  }
  root._codexEditClose = close;
  // Register the teardown (same contract as the link popup — see
  // renderCodexLibrary's sweep).
  root._codexOverlays = [...(root._codexOverlays || []), close];
  async function save() {
    if (saveBtn.disabled) return;
    const desired = nameInput.value.trim();
    try {
      if (desired !== diskName) {
        diskName = await invoke('fable_codex_file_rename', { oldName: diskName, newName: desired });
        // Reflect the canonical stem (safe_display_stem may normalize) so
        // both the field + the revert baseline tell the truth.
        nameInput.value = diskName;
        lastGoodName = diskName;
        validate();
      }
      await invoke('fable_codex_file_write', { name: diskName, text: textarea.value });
      lastGoodText = textarea.value;
      close();
      renderCodexLibrary(root);
    } catch (err) {
      // Keep the editor open (the user's text is untouched); the reason
      // surfaces inline under the head (self-contained screen — no
      // cross-screen imports).
      console.warn('[fable] codex editor save failed', err);
      let note = overlay.querySelector('.fable-codexlib-popup-error');
      if (!note) {
        note = document.createElement('div');
        note.className = 'fable-codexlib-popup-error fable-codexlib-edit-error';
        overlay.querySelector('.fable-raw-editor-modal').prepend(note);
      }
      note.textContent = `Save failed: ${err.message || err}`;
    }
  }
  saveBtn.addEventListener('click', save);
  revertBtn.addEventListener('click', () => {
    nameInput.value = lastGoodName;
    textarea.value = lastGoodText;
    validate();
  });
  closeBtn.addEventListener('click', close);
  document.addEventListener('keydown', onKeys, true);
  overlay.addEventListener('click', (e) => {
    if (e.target === overlay && (nameInput.value === lastGoodName && textarea.value === lastGoodText || !saveBtn.disabled)) close();
  });
  setTimeout(() => nameInput.focus(), 50);
}

// ── DELETE → the confirm dialog ──────────────────────────────────────────

function confirmDelete(root, item) {
  const confirmEl = root.querySelector('[data-confirm]');
  // Already-open guard: a double-click on DELETE re-enters here while the
  // confirm is up — without this, every re-entry stacks another yes/no
  // listener pair (one click = N deletes).
  if (!confirmEl.hidden && confirmEl.classList.contains('is-open')) return;
  const msg = root.querySelector('[data-confirm-msg]');
  const yes = root.querySelector('[data-confirm-yes]');
  const no = root.querySelector('[data-confirm-no]');
  msg.textContent = `Delete ${item.name}? Every card that links it loses this lore. This cannot be undone.`;
  confirmEl.hidden = false;
  root._confirmGen = (root._confirmGen || 0) + 1;
  void confirmEl.offsetWidth;
  confirmEl.classList.add('is-open');

  const close = () => {
    confirmEl.classList.remove('is-open');
    root._confirmGen = (root._confirmGen || 0) + 1;
    const gen = root._confirmGen;
    const finish = () => { if (root._confirmGen === gen) confirmEl.hidden = true; };
    confirmEl.addEventListener('transitionend', finish, { once: true });
    setTimeout(finish, 200);
  };

  const onYes = async () => {
    cleanup();
    close();
    try {
      await invoke('fable_codex_file_delete', { name: item.name });
      renderCodexLibrary(root);
    } catch (err) {
      // Surface inline on the still-open detail modal (the worlds.js
      // discipline) so the user sees the failure without losing the view.
      const card = root.querySelector('[data-modal-card]');
      const note = document.createElement('p');
      note.className = 'fable-player-modal-error';
      note.textContent = `Delete failed: ${err}`;
      card.appendChild(note);
    }
  };
  const onNo = () => { cleanup(); close(); };
  function cleanup() {
    yes.removeEventListener('click', onYes);
    no.removeEventListener('click', onNo);
    root._confirmTeardown = null;
  }
  yes.addEventListener('click', onYes);
  no.addEventListener('click', onNo);
  // (2026-08-23 audit fix) Register the FULL teardown (listeners + hide) so
  // a ‹ exit with the confirm open can't leave a live Yes listener inside
  // the hidden screen — renderCodexLibrary invokes this on re-entry.
  root._confirmTeardown = () => { cleanup(); close(); };
}
