// =============================================================
// SCREEN: WORLDS — the "Load" entry from the title (2026-08-05 rework).
//
// REWORK: this used to be a flat `.fable-card` grid (name + tone + setting
// blurb + "has saves" badge) that routed straight to the saves list. Chloe
// 2026-08-05: "The 'LOAD' button in FABLE is broken, should be hooked up to
// look exactly like the PLAYER load menu but for detecting valid .sim cards
// with the same divider and name of the card underneath." So this now mirrors
// player-picker.js: a grid of mini-cards (portrait + thin themed divider +
// NAME ONLY), each expanding into a centered modal on click.
//
// THE MODAL carries four actions: NEW / LOAD / EDIT / DELETE.
//   • NEW     → fade transition into the Player pair (slide 1), reverse-
//     spawn the buttons, then the New Game flow continues (the player
//     chooses/creates a player → SIM pair → Codex → Intro → fresh game).
//   • LOAD    → the saves list for this card (screens/saves.js). The per-turn
//     autosave is promoted to a one-click "Resume Latest" button at the top;
//     the list below shows the manual saves (most-recent first; the backend
//     already sorts by timestamp desc). The autosave IS the latest world state.
//   • EDIT    → the raw XML editor (engine/raw-editor.js) loaded with the
//     card's <sim_card> via fable_card_raw_get_by_id, saved via
//     fable_card_raw_set_by_id. The <persona> block is a lossy merge of
//     several wizard fields (the sim creators have no reverse-parser), so the
//     faithful edit surface is the XML itself (zero data loss).
//   • DELETE  → confirm → fable_card_delete → re-render the grid.
//
// PORTRAIT: the modal's portrait is CLICKABLE → opens the 2:3 cropper →
// fable_card_portrait_write. Mirrors the player modal's portrait-change path.
//
// AMBIENCE: the SAME newgame.mp3 + fire.mp3 pair + ember background as New
// Game (started/stopped in fable.js onLoadClicked / exitLoadToTitle).
//
// Reads FableCardMeta from fable_cards_list:
//   { id, name, card_type, subtype, setting_preview, tone,
//     opening_scene_preview, player_name, has_saves, portrait, has_portrait }
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { openPortraitCropper } from './portrait-cropper.js';
import { bytesToBase64 } from './wizard-engine.js';
import { open as openDialog } from '@tauri-apps/plugin-dialog';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

export function buildWorlds(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-player-picker-screen';
  root.dataset.fableScreen = 'worlds';
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
  // swaps (the worlds grid shares the Player Picker's exact layout). This
  // screen now carries ONLY the foreground UI (the worlds grid + modal).
  return root;
}

// Populate the grid. Called each time the screen is shown (the set of cards
// may have changed since the last visit — a New Game / Creator adds one).
// `handlers` carries onNewGame(card), onResume(card), onEdit(card); DELETE is
// internal (re-renders the grid).
//
// The grid + mini-card markup is the EXACT SAME `.fable-player-grid` +
// `.fable-player-mini-card` language the Player Picker uses (Chloe 2026-08-05:
// "make the load sim card the SAME as the load player card"). 2026-08-13 delta:
// a SIM card carries a centered TYPE label row (NPC CARD / WORLD CARD /
// SCENARIO CARD, from <subtype>) ABOVE the portrait, with the name below it.
// Player cards show name only (the gender glyph was removed from both).

// The card-type label for a SIM mini-card, from its <metadata><subtype>
// ("npc" | "scenario" | "world"). Old / pre-router cards have no subtype → ''
// (no label — they render like a plain named card).
function subtypeLabel(subtype) {
  const v = (subtype || '').toLowerCase();
  if (v === 'npc') return 'NPC CARD';
  if (v === 'scenario') return 'SCENARIO CARD';
  if (v === 'world') return 'WORLD CARD';
  return '';
}
export async function renderWorlds(root, handlers) {
  root._handlers = handlers || {};
  // pickMode: a plain "pick a card" grid (no NEW/LOAD/EDIT/DELETE modal). A
  // card click calls handlers.onSelect(card) instead of openModal. Used by the
  // New Game flow's LOAD SIM CARD step.
  const pickMode = !!(handlers && handlers.pickMode && handlers.onSelect);
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  closeModal(root);
  let cards = [];
  try {
    cards = await invoke('fable_cards_list');
  } catch (err) {
    host.innerHTML = `<div class="fable-flow-empty"><p>Couldn't load worlds: ${esc(err)}</p></div>`;
    return;
  }
  if (!cards.length) {
    host.innerHTML = `<div class="fable-flow-empty">
      <p>No scenario cards installed.</p>
      <p class="fable-flow-empty-hint">Use New Game to create one, or drop a <code>.sim</code> file into the cards folder.</p>
    </div>`;
    return;
  }
  for (const card of cards) {
    const tile = document.createElement('button');
    // SAME class as the Player Picker's mini-card → SAME CSS → identical look.
    tile.className = 'fable-player-mini-card';
    tile.type = 'button';
    tile.dataset.cardId = card.id;
    tile.title = card.name;
    tile.setAttribute('aria-label', `View card ${card.name}`);
    const portraitHTML = card.has_portrait && card.portrait_url
      ? `<div class="fable-player-mini-portrait"><img class="fable-player-mini-portrait-img" src="${esc(convertFileSrc(card.portrait_url))}" alt="" onerror="this.parentNode.classList.add('fable-player-mini-portrait--placeholder')"></div>`
      : `<div class="fable-player-mini-portrait fable-player-mini-portrait--placeholder" aria-hidden="true"></div>`;
    // A centered TYPE label (NPC/WORLD/SCENARIO CARD) sits in its own row
    // ABOVE the portrait; the name stays below the divider (2026-08-13).
    const typeLabel = subtypeLabel(card.subtype);
    tile.innerHTML = `
      ${typeLabel ? `<div class="fable-player-mini-type">${esc(typeLabel)}</div>` : ''}
      ${portraitHTML}
      <div class="fable-player-mini-divider" aria-hidden="true"></div>
      <div class="fable-player-mini-info">
        <span class="fable-player-mini-name">${esc(card.name)}</span>
      </div>`;
    tile.addEventListener('click', () => {
      if (pickMode) handlers.onSelect(card);
      else openModal(root, card);
    });
    host.appendChild(tile);
  }
}

// --- The expand-to-center modal (mirrors player-picker.openModal) --------
async function openModal(root, meta) {
  // Local double-open guard (the central flowBusy guard covers transitions;
  // modal-open isn't one). Prevents a double-fetch + double-mount.
  if (root._modalOpen) return;
  root._modalOpen = true;
  // Invalidate any in-flight close (see closeModal) — the same stale-timer
  // fix as player-picker: a re-click inside the 260ms close window must
  // never be hidden by the previous close's deferred hide.
  root._modalGen = (root._modalGen || 0) + 1;
  const overlay = root.querySelector('[data-modal]');
  const card = root.querySelector('[data-modal-card]');
  card.innerHTML = `<div class="fable-player-modal-loading">Loading…</div>`;
  overlay.hidden = false;
  void overlay.offsetWidth;
  overlay.classList.add('is-open');

  // Click-outside + Esc to close (same discipline as the player modal).
  const onBackdropClick = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-player-modal-backdrop')) {
      closeModal(root);
    }
  };
  const onEsc = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); closeModal(root); }
  };
  overlay.addEventListener('click', onBackdropClick);
  document.addEventListener('keydown', onEsc, { capture: true });
  root._modalCleanup = () => {
    overlay.removeEventListener('click', onBackdropClick);
    document.removeEventListener('keydown', onEsc, { capture: true });
    root._modalCleanup = null;
  };

  // The card meta already has everything we need (no second IPC).
  card.innerHTML = renderModalCard(meta);

  // Portrait click → cropper → fable_card_portrait_write.
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
        const srcPath = typeof picked === 'string' ? picked : picked.path;
        let dataUrl = null;
        try {
          dataUrl = await invoke('fable_player_portrait_read_bytes', { srcPath });
        } catch (_) { dataUrl = null; }
        if (!dataUrl) return;
        const cropped = await openPortraitCropper(root, dataUrl);
        if (!cropped || !cropped.bytes) return;
        const ext = cropped.ext === 'jpeg' ? 'jpg' : cropped.ext;
        await invoke('fable_card_portrait_write', {
          cardId: meta.id,
          bytesB64: bytesToBase64(cropped.bytes),
          ext,
        });
        // Reflect the new portrait immediately. fable_card_portrait_url
        // returns a RAW absolute filesystem path — it MUST go through
        // convertFileSrc before it's a loadable web URL (the old code
        // assigned it raw → the slot went blank until a re-open; 2026-08-15).
        meta.portrait = `portrait.${ext}`;
        meta.has_portrait = true;
        const absPath = await invoke('fable_card_portrait_url', { cardId: meta.id }).catch(() => null);
        if (absPath) {
          const freshSrc = `${convertFileSrc(absPath)}?t=${Date.now()}`;
          const img = portraitSlot.querySelector('img');
          if (img) { img.style.display = ''; img.src = freshSrc; }
          else portraitSlot.innerHTML = `<img src="${esc(freshSrc)}" alt="" onerror="this.style.display='none'">`;
          // Refresh the grid mini-card too (no screen swap needed).
          refreshMiniPortrait(root, meta.id, freshSrc);
        }
      } catch (err) {
        console.error('[fable] card portrait change failed', err);
      }
    });
  }

  // Bind the four action buttons.
  card.querySelector('[data-modal-new]').addEventListener('click', () => {
    closeModal(root);
    if (root._handlers.onNewGame) root._handlers.onNewGame(meta);
  });
  card.querySelector('[data-modal-resume]').addEventListener('click', () => {
    closeModal(root);
    if (root._handlers.onResume) root._handlers.onResume(meta);
  });
  card.querySelector('[data-modal-edit]').addEventListener('click', () => {
    if (root._handlers.onEdit) root._handlers.onEdit(meta);
  });
  card.querySelector('[data-modal-delete]').addEventListener('click', () => {
    confirmDelete(root, meta);
  });
}

function closeModal(root) {
  const overlay = root.querySelector('[data-modal]');
  if (!overlay) return;
  // ALWAYS release the double-open guard — even on the early return (the
  // player-picker "popup dead until restart" bug class: a stale close timer
  // hides a re-opened modal while _modalOpen stays true).
  const wasOpen = !overlay.hidden;
  root._modalOpen = false;
  if (!wasOpen) return;
  if (root._modalCleanup) root._modalCleanup();
  overlay.classList.remove('is-open');
  // Generation-guarded hide: openModal bumps _modalGen, so a re-open inside
  // the close window invalidates this close's deferred hide.
  root._modalGen = (root._modalGen || 0) + 1;
  const gen = root._modalGen;
  const finish = () => {
    if (root._modalGen !== gen) return;
    overlay.hidden = true;
  };
  overlay.addEventListener('transitionend', finish, { once: true });
  setTimeout(finish, 260);
}

// Refresh one mini-card's portrait in the grid after an in-modal change
// (mirrors player-picker.refreshMiniPortrait).
function refreshMiniPortrait(root, cardId, src) {
  const tile = root.querySelector(`.fable-player-mini-card[data-card-id="${cardId}"]`);
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

const SILHOUETTE_SVG = `<svg class="fable-portrait-silhouette" viewBox="0 0 120 160" aria-hidden="true" focusable="false">
  <path fill="currentColor" d="M60 16c-13 0-23 11-23 25 0 9 4 16 11 21-15 6-27 19-30 36-1 6 4 12 11 12h62c7 0 12-6 11-12-3-17-15-30-30-36 7-5 11-12 11-21 0-14-10-25-23-25z"/>
</svg>`;

// The modal card: portrait on the LEFT (clickable), card identity on the
// right (name + tone + setting/intro blurb + player_name + saves state), +
// four action buttons in a centered row BELOW (NEW / LOAD / EDIT / DELETE).
// Reuses the player modal's classes so the look matches.
function renderModalCard(card) {
  const portraitSrc = (card.has_portrait && card.portrait_url) ? convertFileSrc(card.portrait_url) : '';
  const portraitHTML = portraitSrc
    ? `<img src="${esc(portraitSrc)}" alt="" onerror="this.style.display='none'">`
    : `<span class="fable-player-review-portrait-fallback" aria-hidden="true">${SILHOUETTE_SVG}</span>`;
  const rows = [];
  if (card.tone) rows.push(['Tone', card.tone]);
  if (card.player_name) rows.push(['Player', card.player_name]);
  rows.push(['Saves', card.has_saves ? 'Has saved games' : 'No saves yet']);
  const blurb = card.setting_preview || card.opening_scene_preview || '';
  const blurbHTML = blurb
    ? `<p class="fable-world-modal-blurb">${esc(blurb)}</p>`
    : '';
  const rowsHTML = rows.length
    ? `<dl class="fable-world-modal-rows">${rows.map(([k, v]) => `<div><dt>${esc(k)}</dt><dd>${esc(v)}</dd></div>`).join('')}</dl>`
    : '';
  return `
    <div class="fable-player-review-card fable-world-modal-card">
      <div class="fable-player-review-top">
        <div class="fable-player-review-portrait" data-modal-portrait>${portraitHTML}</div>
        <div class="fable-player-review-body">
          <section class="fable-player-review-section">
            <h3>${esc(card.name)}</h3>
            ${blurbHTML}
            ${rowsHTML}
          </section>
        </div>
      </div>
    </div>
    <div class="fable-player-modal-actions">
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--load" data-modal-new>NEW</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--load" data-modal-resume>LOAD</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--edit" data-modal-edit>EDIT</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--delete" data-modal-delete>DELETE</button>
    </div>`;
}

// --- Delete confirmation (mirrors player-picker.confirmDelete) ------------
function confirmDelete(root, card) {
  const confirmEl = root.querySelector('[data-confirm]');
  const msg = root.querySelector('[data-confirm-msg]');
  const yes = root.querySelector('[data-confirm-yes]');
  const no = root.querySelector('[data-confirm-no]');
  msg.textContent = `Delete ${card.name}? This removes the card and ALL its saves. This cannot be undone.`;
  confirmEl.hidden = false;
  // Invalidate any in-flight close (the same stale-timer class as closeModal).
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
      await invoke('fable_card_delete', { cardId: card.id });
      // Re-render the grid (reflects the deletion + closes the modal).
      renderWorlds(root, root._handlers);
    } catch (err) {
      const cardEl = root.querySelector('[data-modal-card]');
      const note = document.createElement('p');
      note.className = 'fable-player-modal-error';
      note.textContent = `Delete failed: ${err}`;
      cardEl.appendChild(note);
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
