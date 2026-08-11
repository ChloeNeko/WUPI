// =============================================================
// Fable BACKGROUND LIBRARY controller (2026-08-11).
//
// The 4th WUPI-drawer foot button ("Background" — far-left icon in the drawer
// footer) opens a modal gallery over the stage. Selecting a tile paints the
// dormant `.fable-stage-bg` layer (a `display:none` hook left in the stage DOM
// at buildStage precisely for this re-add). Importing a file dialog-picks a
// PNG/JPG, validates it server-side (magic bytes), and lands it in
// `<install>/apps/fable/images/backgrounds/`.
//
// The library + the active selection are GLOBAL to Fable (one marker file at
// `apps/fable/.active_background.json`, NOT per-card). "None (Black)" is the
// default — backgrounds are opt-in; the stage is a pure black void otherwise.
//
// Mirrors the save-modal lifecycle (a stage-appended `[hidden]` overlay at
// z-index 46) + the portrait import pipeline (`@tauri-apps/plugin-dialog` →
// server-side read + `write_atomic`). CSS lives next to `.fable-save-overlay`
// in fable.css and reuses the same brass/glass tokens.
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { bytesToBase64 } from '../screens/wizard-engine.js';
import { openBackgroundCropper } from '../screens/background-cropper.js';

let overlayEl = null;       // lazily-built `.fable-bg-overlay`, appended to the stage once
let lastStageEl = null;     // cached so import/select can re-paint without a re-arg
let activeFilename = null;  // currently-selected filename (null = none/black), mirror of the marker

// ---------------------------------------------------------------
// Apply: paint (or clear) the stage background layer from the marker.
// Idempotent — safe to call on every stage entry. A deleted/missing active
// background resolves to None on the backend, so this falls back to black.
// ---------------------------------------------------------------
export async function applyBackground(stageEl) {
  lastStageEl = stageEl || lastStageEl;
  const bgLayer = lastStageEl && lastStageEl.querySelector('.fable-stage-bg');
  if (!bgLayer) return;
  let meta = null;
  try {
    meta = await invoke('fable_background_active_get');
  } catch (_) {
    meta = null; // best-effort: never block stage entry on a bg fetch
  }
  if (meta && meta.path) {
    // ?t= cache-busts so a re-import (same filename, new bytes) repaints
    // immediately instead of serving the cached asset:// image.
    bgLayer.style.backgroundImage = `url("${convertFileSrc(meta.path)}?t=${Date.now()}")`;
    bgLayer.style.display = 'block';
    activeFilename = meta.filename;
    // has-bg scopes the readability card panels (.fable-mes-block glass) to
    // ONLY when a background is painted — the default void keeps the current
    // transparent-card aesthetic. (lastStageEl IS the .fable-stage section.)
    if (lastStageEl) lastStageEl.classList.add('has-bg');
  } else {
    bgLayer.style.backgroundImage = '';
    bgLayer.style.display = 'none';
    activeFilename = null;
    if (lastStageEl) lastStageEl.classList.remove('has-bg');
  }
}

// ---------------------------------------------------------------
// Open / close the gallery modal.
// ---------------------------------------------------------------
export function openBackgroundsPanel(stageEl) {
  lastStageEl = stageEl || lastStageEl;
  ensureOverlay(lastStageEl);
  overlayEl.hidden = false;
  void renderGallery();
}

export function closeBackgroundsPanel() {
  if (overlayEl) overlayEl.hidden = true;
}

// stage.js's Esc chain calls this — returns true when the overlay is open so
// the caller knows to swallow the Esc.
export function isOpen() {
  return !!(overlayEl && !overlayEl.hidden);
}

// ---------------------------------------------------------------
// Build + cache the overlay element (one per stage lifetime). Appended to the
// stage (NOT document.body) so it inherits the stage's positioning context +
// tears down with it. Mirrors `.fable-save-overlay`'s structure: backdrop +
// centered card, `[hidden]` toggles visibility.
// ---------------------------------------------------------------
function ensureOverlay(stageEl) {
  if (overlayEl && overlayEl.isConnected) return;
  overlayEl = document.createElement('div');
  overlayEl.className = 'fable-bg-overlay';
  overlayEl.hidden = true;
  overlayEl.innerHTML = `
    <div class="fable-bg-backdrop" data-bg-backdrop></div>
    <div class="fable-bg-modal">
      <header class="fable-bg-head">
        <h2 class="fable-bg-title">Background</h2>
        <button class="fable-bg-import" data-bg-import title="Import a background image (1440p 16:9 recommended)">
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v12M12 4l-4 4M12 4l4 4" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/><path d="M4 16v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>
          <span>Import</span>
        </button>
        <button class="fable-bg-close" data-bg-close aria-label="Close">✕</button>
      </header>
      <div class="fable-bg-hint">1440p · 16:9 · PNG or JPG — the name shows without its extension.</div>
      <div class="fable-bg-grid" data-bg-grid></div>
    </div>`;
  // Single event delegate over the overlay. Delete is checked BEFORE tile so a
  // click on a tile's ✕ never also selects the tile underneath.
  overlayEl.addEventListener('click', (e) => {
    if (e.target.closest('[data-bg-close]') || e.target.closest('[data-bg-backdrop]')) {
      closeBackgroundsPanel();
      return;
    }
    if (e.target.closest('[data-bg-import]')) {
      void onImport();
      return;
    }
    const del = e.target.closest('[data-bg-delete]');
    if (del) {
      void onDelete(del.dataset.filename);
      return;
    }
    const tile = e.target.closest('[data-bg-tile]');
    if (tile) {
      void onSelect(tile.dataset.filename);
      return;
    }
  });
  const mount = stageEl || document.querySelector('.fable-stage');
  if (mount) mount.appendChild(overlayEl);
  else document.body.appendChild(overlayEl);
}

// ---------------------------------------------------------------
// Render the gallery grid. "None (Black)" is always first (clears → black
// void); the library follows, sorted by name on the backend.
// ---------------------------------------------------------------
async function renderGallery() {
  const grid = overlayEl && overlayEl.querySelector('[data-bg-grid]');
  if (!grid) return;
  let list = [];
  try {
    list = await invoke('fable_backgrounds_list');
  } catch (_) {
    list = [];
  }
  // Re-read the active marker (it may have changed elsewhere / last session).
  try {
    const a = await invoke('fable_background_active_get');
    activeFilename = a && a.filename ? a.filename : null;
  } catch (_) {
    activeFilename = null;
  }

  const cells = [renderNoneTile(!activeFilename)];
  for (const meta of list) {
    cells.push(renderTile(meta, meta.filename === activeFilename));
  }
  if (!list.length) {
    cells.push('<div class="fable-bg-empty">No backgrounds yet — click <b>Import</b> to add one.</div>');
  }
  grid.innerHTML = cells.join('');
}

// The "None" tile — a hollow frame glyph, no delete control.
function renderNoneTile(isActive) {
  const activeCls = isActive ? ' is-active' : '';
  return `<div class="fable-bg-cell${activeCls}">
    <button class="fable-bg-tile" data-bg-tile data-filename="" title="None (black void)">
      <div class="fable-bg-thumb fable-bg-thumb--none">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 5h14v14H5z" fill="none" stroke="currentColor" stroke-width="1.4"/><path d="M5 5l14 14M19 5L5 19" fill="none" stroke="currentColor" stroke-width="1.1" opacity="0.6"/></svg>
      </div>
      <span class="fable-bg-name">None</span>
    </button>
  </div>`;
}

// One library tile: thumbnail (preview) on top, name stem below. The delete ✕
// is a SIBLING button (not nested — a button-in-button is invalid HTML + the
// outer would swallow the click), absolutely positioned over the cell's corner.
function renderTile(meta, isActive) {
  const url = `${convertFileSrc(meta.path)}?t=${Date.now()}`;
  const activeCls = isActive ? ' is-active' : '';
  return `<div class="fable-bg-cell${activeCls}">
    <button class="fable-bg-tile" data-bg-tile data-filename="${attr(meta.filename)}" title="${attr(meta.name)}">
      <div class="fable-bg-thumb" style="background-image:url('${url}')"></div>
      <span class="fable-bg-name">${escapeHtml(meta.name)}</span>
    </button>
    <button class="fable-bg-delete" data-bg-delete data-filename="${attr(meta.filename)}" title="Delete" aria-label="Delete ${attr(meta.name)}">
      <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6l12 12M18 6L6 18" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>
    </button>
  </div>`;
}

// ---------------------------------------------------------------
// Actions
// ---------------------------------------------------------------
async function onSelect(filename) {
  // Empty filename string → clear (None tile). Backend takes Option<String>.
  const payload = filename ? { filename } : { filename: null };
  try {
    await invoke('fable_background_active_set', payload);
  } catch (err) {
    console.warn('[backgrounds] active_set failed:', err);
  }
  if (lastStageEl) await applyBackground(lastStageEl);
  await renderGallery();
}

async function onImport() {
  let picked;
  try {
    picked = await openDialog({
      multiple: false,
      filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg'] }],
    });
  } catch (_) {
    return;
  }
  if (!picked) return; // user cancelled
  const srcPath = typeof picked === 'string' ? picked : picked.path;
  if (!srcPath) return;
  const stem = stemFromPath(srcPath);
  // Read the picked bytes server-side → a same-origin data URL. A cross-origin
  // convertFileSrc `asset://` URL would taint the cropper's canvas →
  // SecurityError on toBlob. fable_player_portrait_read_bytes is a GENERIC
  // image-read IPC despite the name (validates magic bytes + returns a data URL
  // for any image path) — reused here verbatim.
  let dataUrl = null;
  try {
    dataUrl = await invoke('fable_player_portrait_read_bytes', { srcPath });
  } catch (err) {
    console.warn('[backgrounds] read picked image failed:', err);
    return;
  }
  if (!dataUrl) return;
  // Run the cropper (free-aspect, 16:9 default, natural-dim output). Cancel =
  // abort, nothing is written to the library.
  const mount = lastStageEl || document.querySelector('.fable-stage') || document.body;
  const cropped = await openBackgroundCropper(mount, dataUrl);
  if (!cropped || !cropped.bytes) return;
  // Send the cropped bytes → library. Backend builds {stem}.{detected-ext},
  // validates magic bytes, writes atomically, returns the new row.
  let meta;
  try {
    meta = await invoke('fable_background_import_bytes', {
      bytesB64: bytesToBase64(cropped.bytes),
      filenameStem: stem,
    });
  } catch (err) {
    console.warn('[backgrounds] import failed:', err);
    return;
  }
  // Auto-select the just-imported background (you import because you want to
  // see it). onSelect persists the selection to the active card's schema +
  // repaints the stage + re-renders the gallery with the new tile highlighted.
  if (meta && meta.filename) {
    await onSelect(meta.filename);
  } else {
    await renderGallery();
  }
}

async function onDelete(filename) {
  if (!filename) return;
  // If we're deleting the currently-active bg, clear the marker first so the
  // stage reverts to black cleanly (otherwise the marker points at a ghost
  // until active_get's stat-check degrades it to None on next entry).
  if (filename === activeFilename) {
    try {
      await invoke('fable_background_active_set', { filename: null });
    } catch (_) { /* best-effort */ }
    if (lastStageEl) await applyBackground(lastStageEl);
  }
  try {
    await invoke('fable_background_delete', { filename });
  } catch (err) {
    console.warn('[backgrounds] delete failed:', err);
  }
  await renderGallery();
}

// ---------------------------------------------------------------
// Tiny HTML helpers (no template dependency).
// ---------------------------------------------------------------
function attr(s) {
  return String(s).replace(/"/g, '&quot;');
}
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  }[c]));
}
// The picked file's stem (no extension, no directory) — becomes the gallery
// display name + the on-disk filename stem the backend writes.
function stemFromPath(p) {
  const norm = String(p).replace(/\\/g, '/');
  const leaf = norm.split('/').pop() || norm;
  const dot = leaf.lastIndexOf('.');
  return dot > 0 ? leaf.slice(0, dot) : leaf;
}
