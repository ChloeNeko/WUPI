// =============================================================
// SCREEN: PLAYER PICKER — grid of mini SIM cards + expand-to-center modal.
//
// REWRITE 2026-08-04: replaced the portrait-only tile grid with a mini-SIM-
// card grid (portrait + Name + Race + ♂/♀ glyph, straight off PlayerMeta —
// no per-tile fable_player_get). Clicking a mini-card expands it into the
// dead center of the screen (CSS scale/transform, dimmed backdrop); the
// modal loads the full SavedPlayer via fable_player_get + renders the
// complete Appearance data, with three brass action buttons at the bottom
// center: LOAD · EDIT · DELETE.
//
//   LOAD   → handlers.onSelect(player) → advanceAfterPlayer(id) (existing).
//   EDIT   → handlers.onEdit(player) → fable.js routes to the Player Creator
//            seeded with editFrom (the loaded JSON).
//   DELETE → confirmation → fable_player_delete IPC → re-render the grid.
//
// EMPTY STATE: fully interactive (no disabled buttons). If the player has
// no saved players yet, an empty-state message invites them to create one —
// the ‹ / ⌂ chrome stays clickable. Per Chloe: "don't make the button
// unclickable."
//
// The chrome (‹ / ⌂) is owned by the flow controller; there is no header
// bar here. The modal is a centered overlay (z under the OS top bar).
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { openPortraitCropper } from './portrait-cropper.js';
import { bytesToBase64 } from './wizard-engine.js';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { buildIdCard } from '../engine/creator-engine.js';
import { renderIdCard, wireIdCard } from './id-card.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// NOTE 2026-08-13 (Chloe): the ♂/♀ gender glyph was REMOVED from the mini-card
// ("completely removed so it only shows the name"). The card now shows ONLY the
// portrait + the centered ivory name below. The MARS_SVG / VENUS_SVG /
// genderSVG helpers that fed the glyph are deleted with it. The full identity
// (gender included) still surfaces in the centered modal on click.

export function buildPlayerPicker(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-player-picker-screen';
  root.dataset.fableScreen = 'player-picker';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-player-grid" data-host></div>
    <div class="fable-player-modal-overlay" data-modal hidden>
      <div class="fable-player-modal-backdrop" data-modal-backdrop></div>
      <div class="fable-player-modal" data-modal-card role="dialog" aria-modal="true"></div>
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
  // NOTE: the deep-void background + hearth glow + rising embers NO LONGER
  // live here — they were hoisted to a persistent .fable-flow-ambiance layer
  // on #fable (fable.js) so the background stays consistent across screen
  // swaps. This screen now carries ONLY the foreground UI (the player grid).
  return root;
}

// Populate the grid. Called each time the screen is shown. `handlers`
// carries onSelect (LOAD), onEdit (EDIT), and the DELETE path is internal.
export async function renderPlayerPicker(root, handlers) {
  root._handlers = handlers || {};
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  closeModal(root);
  let players = [];
  try {
    players = await invoke('fable_players_list');
  } catch (_) {
    // Swallow: the user never needs to see a load failure here. Fall through
    // to the empty-state below ("No saved players yet") so the picker stays
    // usable + navigable instead of surfacing a raw TypeError.
    players = [];
  }
  if (!players.length) {
    host.innerHTML = `<div class="fable-flow-empty">
      <p>No saved players yet.</p>
      <p class="fable-flow-empty-hint">Use ‹ to go back and Create a Player.</p>
    </div>`;
    return;
  }
  for (const p of players) {
    const tile = document.createElement('button');
    tile.className = 'fable-player-mini-card';
    tile.type = 'button';
    tile.dataset.playerId = p.id;
    tile.title = p.name;
    tile.setAttribute('aria-label', `View player ${p.name}`);
    const portraitHTML = p.has_portrait
      ? `<div class="fable-player-mini-portrait" data-lazy-portrait="${esc(p.id)}"></div>`
      : `<div class="fable-player-mini-portrait fable-player-mini-portrait--placeholder" aria-hidden="true"></div>`;
    // 2026-08-13 (Chloe): the mini-card shows ONLY the portrait + the centered
    // ivory name below the divider. The gender glyph is gone; the full identity
    // surfaces in the centered modal on click.
    tile.innerHTML = `
      ${portraitHTML}
      <div class="fable-player-mini-divider" aria-hidden="true"></div>
      <div class="fable-player-mini-info">
        <span class="fable-player-mini-name">${esc(p.name)}</span>
      </div>`;
    tile.addEventListener('click', () => openModal(root, p));
    host.appendChild(tile);
  }
  // Lazy-resolve portraits (one get per portrait-bearing player).
  host.querySelectorAll('[data-lazy-portrait]').forEach(async (el) => {
    try {
      const id = el.dataset.lazyPortrait;
      const full = await invoke('fable_player_get', { id });
      if (full.portrait) {
        const img = document.createElement('img');
        img.className = 'fable-player-mini-portrait-img';
        img.src = convertFileSrc(full.portrait);
        img.alt = '';
        img.onerror = () => { /* leave placeholder */ };
        el.classList.remove('fable-player-mini-portrait--placeholder');
        el.appendChild(img);
      }
    } catch (_) { /* leave placeholder */ }
  });
}

// --- The expand-to-center modal -----------------------------------------
async function openModal(root, meta) {
  // Guard against a rapid double-click on a mini-card firing two opens (the
  // central flowBusy guard covers transitions, but modal-open itself isn't a
  // transition — this local gate prevents the double-fetch + double-mount).
  if (root._modalOpen) return;
  root._modalOpen = true;
  // Invalidate any in-flight close (see closeModal): a re-click inside the
  // 260ms close window must never be hidden by the previous close's stale
  // transitionend/timer — that left the modal invisible with _modalOpen
  // stuck true, permanently killing every later click ("the popup never
  // opens again until restart", 2026-08-15).
  root._modalGen = (root._modalGen || 0) + 1;
  const overlay = root.querySelector('[data-modal]');
  const card = root.querySelector('[data-modal-card]');
  // Loading state while we fetch the full player.
  card.innerHTML = `<div class="fable-player-modal-loading">Loading…</div>`;
  overlay.hidden = false;
  // Force reflow so the .is-open transition runs.
  void overlay.offsetWidth;
  overlay.classList.add('is-open');

  // ── Click-outside-to-close + Esc-to-close (2026-08-05) ────────────────
  // Chloe: "allow the player to click outside the card to X out of it." A
  // click on the backdrop/overlay (NOT the card itself) closes the modal; Esc
  // closes it too. Registered once per open, cleaned up on close so no
  // listener stacks across re-opens.
  const onBackdropClick = (e) => {
    // Only close when the click landed on the overlay or the dedicated
    // backdrop — NOT the card or any of its descendants.
    if (e.target === overlay || e.target.classList.contains('fable-player-modal-backdrop')) {
      closeModal(root);
    }
  };
  const onEsc = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); closeModal(root); }
  };
  overlay.addEventListener('click', onBackdropClick);
  // Esc on the document (capture so it wins over the stage's own chain while
  // the modal is up). Removed on close.
  document.addEventListener('keydown', onEsc, { capture: true });
  root._modalCleanup = () => {
    overlay.removeEventListener('click', onBackdropClick);
    document.removeEventListener('keydown', onEsc, { capture: true });
    root._modalCleanup = null;
  };

  let full;
  try {
    full = await invoke('fable_player_get', { id: meta.id });
  } catch (err) {
    card.innerHTML = `<div class="fable-player-modal-loading">Couldn't load: ${esc(err)}</div>`;
    return;
  }
  card.innerHTML = renderModalCard(full);

  // ── Portrait click → cropper → re-save the portrait (2026-08-05) ──────
  // Chloe: "allow the user to change their picture ... when they click on the
  // card and you see the image on the left, allow it to be clickable so when
  // someone clicks on it, the same exact image cropping thing pops." The
  // portrait slot opens a file picker → the 2:3 cropper → upload_bytes. A
  // successful crop refreshes the modal portrait AND the grid mini-card
  // immediately (2026-08-15: the old code rebuilt a RELATIVE 'portrait.png'
  // + handed it to convertFileSrc → a broken asset URL → the slot went blank
  // until the modal was re-opened. The fresh ABSOLUTE path comes from a
  // re-fetch of the player — the server is the source of truth).
  const portraitSlot = card.querySelector('[data-modal-portrait]');
  if (portraitSlot) {
    portraitSlot.style.cursor = 'pointer';
    portraitSlot.title = 'Change portrait';
    portraitSlot.addEventListener('click', async () => {
      try {
        const picked = await openDialog({
          multiple: false,
          filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg'] }],
        });
        if (!picked) return;
        // The cropper takes a URL the browser can paint. Read the picked
        // file's bytes as a data URL via the existing portrait-bytes IPC
        // (server-side read + magic-byte validation).
        const srcPath = typeof picked === 'string' ? picked : picked.path;
        let dataUrl = null;
        try {
          dataUrl = await invoke('fable_player_portrait_read_bytes', { srcPath });
        } catch (_) { dataUrl = null; }
        if (!dataUrl) return;
        const cropped = await openPortraitCropper(root, dataUrl);
        if (!cropped || !cropped.bytes) return;
        await invoke('fable_player_portrait_upload_bytes', {
          id: full.id,
          bytesB64: bytesToBase64(cropped.bytes),
        });
        // Re-fetch for the fresh ABSOLUTE portrait path (cache-busted).
        let freshAbs = null;
        try {
          const again = await invoke('fable_player_get', { id: full.id });
          if (again && again.portrait) freshAbs = again.portrait;
        } catch (_) { /* keep the old path */ }
        if (freshAbs) full.portrait = freshAbs;
        if (full.portrait) {
          const freshSrc = `${convertFileSrc(full.portrait)}?t=${Date.now()}`;
          const img = portraitSlot.querySelector('img');
          if (img) { img.style.display = ''; img.src = freshSrc; }
          else portraitSlot.innerHTML = `<img src="${esc(freshSrc)}" alt="" onerror="this.style.display='none'">`;
          refreshMiniPortrait(root, full.id, freshSrc);
        }
      } catch (err) {
        console.error('[fable] player portrait change failed', err);
      }
    });
  }

  // Bind the three action buttons.
  card.querySelector('[data-modal-load]').addEventListener('click', () => {
    closeModal(root);
    if (root._handlers.onSelect) root._handlers.onSelect(full);
  });
  card.querySelector('[data-modal-edit]').addEventListener('click', () => {
    closeModal(root);
    if (root._handlers.onEdit) root._handlers.onEdit(full);
  });
  card.querySelector('[data-modal-delete]').addEventListener('click', () => {
    confirmDelete(root, full);
  });
  // Card-icon details popup on the ID card.
  wireIdCard(card);
}

function closeModal(root) {
  const overlay = root.querySelector('[data-modal]');
  if (!overlay) return;
  // ALWAYS release the double-open guard — even on the early return. A stale
  // close timer can hide a freshly re-opened modal (overlay.hidden === true)
  // while _modalOpen stays true; combined with the old early-return this
  // wedged the popup dead until an app restart.
  const wasOpen = !overlay.hidden;
  root._modalOpen = false;
  if (!wasOpen) return;
  if (root._modalCleanup) root._modalCleanup();
  overlay.classList.remove('is-open');
  // Generation-guard the deferred hide: openModal bumps _modalGen, so a
  // re-open inside the close window invalidates this close's transitionend/
  // fallback timer — they can never hide the new open.
  root._modalGen = (root._modalGen || 0) + 1;
  const gen = root._modalGen;
  const finish = () => {
    if (root._modalGen !== gen) return;
    overlay.hidden = true;
  };
  overlay.addEventListener('transitionend', finish, { once: true });
  setTimeout(finish, 260);
}

// Refresh one mini-card's portrait in the grid (called after an in-modal
// portrait change so the tile updates without leaving + re-entering the
// screen — the "stays blank until I change the screen" report).
function refreshMiniPortrait(root, playerId, src) {
  const tile = root.querySelector(`.fable-player-mini-card[data-player-id="${playerId}"]`);
  if (!tile) return;
  const holder = tile.querySelector('.fable-player-mini-portrait');
  if (!holder) return;
  holder.classList.remove('fable-player-mini-portrait--placeholder');
  let img = holder.querySelector('img');
  if (!img) {
    img = document.createElement('img');
    img.className = 'fable-player-mini-portrait-img';
    img.alt = '';
    holder.appendChild(img);
  }
  img.onerror = () => { holder.classList.add('fable-player-mini-portrait--placeholder'); };
  img.src = src;
}

// The same default silhouette SVG the Player Creator uses for an empty
// portrait slot (player-creator.js::SILHOUETTE_SVG). Reused here so the
// modal's empty portrait reads identically to the creator's review card.
const SILHOUETTE_SVG = `<svg class="fable-portrait-silhouette" viewBox="0 0 120 160" aria-hidden="true" focusable="false">
  <path fill="currentColor" d="M60 16c-13 0-23 11-23 25 0 9 4 16 11 21-15 6-27 19-30 36-1 6 4 12 11 12h62c7 0 12-6 11-12-3-17-15-30-30-36 7-5 11-12 11-21 0-14-10-25-23-25z"/>
</svg>`;

// 2026-08-13 (Chloe): the modal renders the compact ID card (8 core fields,
// portrait left) shared with the Creator review — via buildIdCard +
// renderIdCard. Everything else (hair length/style, build, distinctive
// features, clothing, accessories, inventory, background, …) lives behind the
// card-icon details popup. The portrait stays clickable (data-modal-portrait)
// to re-pick; the three action buttons live in a centered wrapper BELOW the
// card.
function renderModalCard(sp) {
  const portraitHTML = sp.portrait
    ? `<img src="${esc(convertFileSrc(sp.portrait))}" alt="" onerror="this.style.display='none'">`
    : `<span class="fable-player-review-portrait-fallback" aria-hidden="true">${SILHOUETTE_SVG}</span>`;
  const model = buildIdCard('player', sp);
  return renderIdCard(model, { portraitClickable: false, portraitHtml: portraitHTML }) + `
    <div class="fable-player-modal-actions">
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--load" data-modal-load>LOAD</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--edit" data-modal-edit>EDIT</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--delete" data-modal-delete>DELETE</button>
    </div>
  `;
}

// --- Delete confirmation (the one irreversible action) -------------------
function confirmDelete(root, sp) {
  const confirmEl = root.querySelector('[data-confirm]');
  const msg = root.querySelector('[data-confirm-msg]');
  const yes = root.querySelector('[data-confirm-yes]');
  const no = root.querySelector('[data-confirm-no]');
  msg.textContent = `Delete ${sp.name}? This cannot be undone.`;
  confirmEl.hidden = false;
  // Invalidate any in-flight close (mirrors closeModal's open-side bump).
  root._confirmGen = (root._confirmGen || 0) + 1;
  void confirmEl.offsetWidth;
  confirmEl.classList.add('is-open');

  const close = () => {
    confirmEl.classList.remove('is-open');
    // Generation-guarded hide (same class as closeModal): a re-opened
    // confirm can never be hidden by a prior close's stale timer.
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
      await invoke('fable_player_delete', { id: sp.id });
      // Re-render the grid (reflects the deletion + closes the modal).
      renderPlayerPicker(root, root._handlers);
    } catch (err) {
      // Surface inline — keep the modal open so the user sees the failure.
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
  }
  yes.addEventListener('click', onYes);
  no.addEventListener('click', onNo);
}
