// =============================================================
// PRISM GALLERY — the Glass Vault.
//
// A masonry grid reading from prism_gallery_list (lazy-paged by scroll).
// Each thumbnail: convertFileSrc(path) for the <img>. Hover-state Quick
// Actions (Favorite, Delete, Send to Composer). Click a thumbnail → frosted-
// glass metadata panel slides in with the prompt + size + a Fork button
// (loads the image's params + seed into Fork & Edit).
//
// Filter bar: All / Favorites + a search box (case-insensitive
// prompt substring, handled Rust-side).
//
// DELETE IS PERMANENT (2026-08-18 ruling): no trash, no restore — the
// row AND the PNG are removed the moment Delete is confirmed.
//
// Build/wire/teardown triplet (FABLE convention).
// =============================================================

import { convertFileSrc } from '@tauri-apps/api/core';
import {
  galleryList, galleryFavorite, galleryDelete,
} from '../engine/api.js';

// The current filter + pagination cursor.
let filter = { favorites_only: false, search: '' };
let page = 0;              // offset = page * PAGE_SIZE
const PAGE_SIZE = 60;
let loading = false;
let exhausted = false;     // true when the last page returned < PAGE_SIZE

// Cached rows (so the metadata panel + quick actions don't re-fetch).
let rows = [];

// Injected router hooks.
let hooks = {};

// Listeners (removed in teardown).
let scrollHandler = null;

export function buildEl(routerHooks = {}) {
  hooks = routerHooks;
  filter = { favorites_only: false, search: '' };
  page = 0;
  rows = [];
  exhausted = false;
  loading = false;

  const el = document.createElement('div');
  el.className = 'prism-screen prism-gallery';
  el.innerHTML = `
    <div class="prism-gallery-bar">
      <div class="prism-gallery-filters">
        <button class="prism-chip is-active" data-filter="all">All</button>
        <button class="prism-chip" data-filter="favorites">★ Favorites</button>
      </div>
      <input class="prism-gallery-search" type="text" placeholder="Search prompts…"
             autocomplete="off" spellcheck="false" />
    </div>
    <div class="prism-masonry" data-masonry></div>
    <div class="prism-gallery-empty" hidden>No images yet. Generate one in Compose.</div>
    <div class="prism-gallery-loading" hidden>Loading…</div>
  `;
  return el;
}

export function wire(rootEl) {
  // Filter chips.
  rootEl.querySelectorAll('[data-filter]').forEach((chip) => {
    chip.addEventListener('click', () => onFilterChip(rootEl, chip));
  });
  // Search (debounced).
  const search = rootEl.querySelector('.prism-gallery-search');
  if (search) {
    let t = null;
    search.addEventListener('input', () => {
      clearTimeout(t);
      t = setTimeout(() => onSearch(rootEl, search.value), 350);
    });
  }
  // Infinite scroll.
  scrollHandler = () => onScroll(rootEl);
  const masonry = rootEl.querySelector('[data-masonry]');
  if (masonry) {
    masonry.addEventListener('scroll', scrollHandler);
    window.addEventListener('scroll', scrollHandler, { passive: true });
  }
  // Delegated click on the masonry (thumbnail open + quick actions).
  if (masonry) {
    masonry.addEventListener('click', (e) => onMasonryClick(rootEl, e));
  }
  // Initial load.
  refresh(rootEl);
}

export function teardown() {
  if (scrollHandler) {
    window.removeEventListener('scroll', scrollHandler);
    scrollHandler = null;
  }
}

// ── Data ────────────────────────────────────────────────────────────────

export async function refresh(rootEl) {
  page = 0;
  rows = [];
  exhausted = false;
  await loadMore(rootEl, true);
}

async function loadMore(rootEl, isRefresh = false) {
  if (loading || exhausted) return;
  loading = true;
  showLoading(rootEl, true);
  try {
    const offset = page * PAGE_SIZE;
    const batch = await galleryList(filter, PAGE_SIZE, offset);
    if (isRefresh) rows = batch;
    else rows = rows.concat(batch);
    if (batch.length < PAGE_SIZE) exhausted = true;
    page += 1;
    renderMasonry(rootEl);
  } catch (err) {
    if (hooks.onToast) hooks.onToast(String(err));
  } finally {
    loading = false;
    showLoading(rootEl, false);
  }
}

function onScroll(rootEl) {
  // Near-bottom → load the next page.
  const masonry = rootEl.querySelector('[data-masonry]');
  if (!masonry) return;
  const nearBottom = (window.innerHeight + window.scrollY) >= (document.body.scrollHeight - 400);
  if (nearBottom) loadMore(rootEl, false);
}

// ── Render ──────────────────────────────────────────────────────────────

function renderMasonry(rootEl) {
  const masonry = rootEl.querySelector('[data-masonry]');
  const empty = rootEl.querySelector('.prism-gallery-empty');
  if (!masonry) return;
  if (rows.length === 0) {
    masonry.innerHTML = '';
    if (empty) empty.hidden = false;
    return;
  }
  if (empty) empty.hidden = true;
  masonry.innerHTML = rows.map((img) => tileHtml(img)).join('');
}

function tileHtml(img) {
  const url = convertFileSrc(img.path);
  const fav = img.favorite ? 'is-favorite' : '';
  return `
    <div class="prism-tile ${fav}" data-id="${img.id}">
      <img class="prism-tile-img" loading="lazy" src="${escapeAttr(url)}" alt="" />
      <div class="prism-tile-overlay">
        <div class="prism-tile-actions">
          <button class="prism-tile-btn" data-act="favorite" title="Favorite">${img.favorite ? '★' : '☆'}</button>
          <button class="prism-tile-btn" data-act="compose" title="Send to Composer">✎</button>
          <button class="prism-tile-btn" data-act="fork" title="Fork & Edit">⇄</button>
          <button class="prism-tile-btn prism-tile-btn-danger" data-act="delete" title="Delete">🗑</button>
        </div>
      </div>
    </div>
  `;
}

function showLoading(rootEl, on) {
  const ld = rootEl.querySelector('.prism-gallery-loading');
  if (ld) ld.hidden = !on;
}

// ── Interactions ────────────────────────────────────────────────────────

function onFilterChip(rootEl, chip) {
  rootEl.querySelectorAll('[data-filter]').forEach((c) => c.classList.remove('is-active'));
  chip.classList.add('is-active');
  const f = chip.dataset.filter;
  filter = {
    favorites_only: f === 'favorites',
    search: filter.search,
  };
  refresh(rootEl);
}

function onSearch(rootEl, q) {
  filter.search = (q || '').trim();
  refresh(rootEl);
}

async function onMasonryClick(rootEl, e) {
  const tile = e.target.closest('[data-id]');
  if (!tile) return;
  const id = Number(tile.dataset.id);
  const actBtn = e.target.closest('[data-act]');
  if (actBtn) {
    e.stopPropagation();
    const act = actBtn.dataset.act;
    await onQuickAction(rootEl, id, act, actBtn);
    return;
  }
  // No action button → open the metadata panel.
  openMetadata(rootEl, id);
}

async function onQuickAction(rootEl, id, act, btn) {
  const img = rows.find((r) => r.id === id);
  if (!img) return;
  try {
    if (act === 'favorite') {
      const now = !img.favorite;
      await galleryFavorite(id, now);
      img.favorite = now ? 1 : 0;
      btn.textContent = now ? '★' : '☆';
      btn.closest('.prism-tile').classList.toggle('is-favorite', now);
    } else if (act === 'delete') {
      // PERMANENT: no trash, no undo — confirm, then the row AND the PNG
      // are gone. One confirm guards the irreversible click.
      if (!confirm('Permanently delete this image? This cannot be undone.')) return;
      await galleryDelete(id);
      await refresh(rootEl);
    } else if (act === 'compose') {
      if (hooks.onSendToComposer) hooks.onSendToComposer(img);
    } else if (act === 'fork') {
      if (hooks.onFork) hooks.onFork(img);
    }
  } catch (err) {
    if (hooks.onToast) hooks.onToast(String(err));
  }
}

// ── Metadata panel ──────────────────────────────────────────────────────

function openMetadata(rootEl, id) {
  const img = rows.find((r) => r.id === id);
  if (!img) return;
  // Build (or reuse) the slide-in panel at the gallery root.
  let panel = rootEl.querySelector('.prism-meta-panel');
  if (!panel) {
    panel = document.createElement('div');
    panel.className = 'prism-meta-panel';
    rootEl.appendChild(panel);
  }
  const url = convertFileSrc(img.path);
  // The panel shows ONLY the prompts + the size (2026-08-17 ruling) — the
  // seed/sampler/CFG/steps/model machinery is invisible; seed iteration
  // lives in Fork & Edit.
  panel.innerHTML = `
    <div class="prism-meta-backdrop" data-act="close"></div>
    <aside class="prism-meta-card">
      <button class="prism-meta-close" data-act="close" aria-label="close">✕</button>
      <img class="prism-meta-img" src="${escapeAttr(url)}" alt="" />
      <div class="prism-meta-fields">
        <div class="prism-meta-field"><span class="prism-meta-k">Prompt</span><p class="prism-meta-v">${escapeHtml(img.prompt) || '<em>(empty)</em>'}</p></div>
        ${img.negative_prompt ? `<div class="prism-meta-field"><span class="prism-meta-k">Negative</span><p class="prism-meta-v">${escapeHtml(img.negative_prompt)}</p></div>` : ''}
        <div class="prism-meta-field"><span class="prism-meta-k">Size</span><p class="prism-meta-v mono">${img.width}×${img.height}</p></div>
        ${(img.nsfw || img.furry) ? `<div class="prism-meta-field"><span class="prism-meta-k">Toggles</span><p class="prism-meta-v">
          ${img.nsfw ? '<span class="prism-meta-badge is-nsfw">NSFW</span>' : ''}
          ${img.furry ? '<span class="prism-meta-badge is-furry">FURRY</span>' : ''}
        </p></div>` : ''}
        <div class="prism-meta-actions">
          <button class="prism-btn prism-btn-ghost" data-act="compose">Send to Composer</button>
          <button class="prism-btn prism-btn-primary" data-act="fork">Fork &amp; Edit</button>
        </div>
      </div>
    </aside>
  `;
  panel.classList.add('is-open');
  panel.addEventListener('click', function onPanelClick(e) {
    const t = e.target.closest('[data-act]');
    if (!t) return;
    const a = t.dataset.act;
    if (a === 'close') {
      panel.classList.remove('is-open');
      panel.removeEventListener('click', onPanelClick);
    } else if (a === 'compose') {
      if (hooks.onSendToComposer) hooks.onSendToComposer(img);
      panel.classList.remove('is-open');
      panel.removeEventListener('click', onPanelClick);
    } else if (a === 'fork') {
      if (hooks.onFork) hooks.onFork(img);
      panel.classList.remove('is-open');
      panel.removeEventListener('click', onPanelClick);
    }
  });
}

// ── Helpers ─────────────────────────────────────────────────────────────

function escapeHtml(s) {
  if (s == null) return '';
  return String(s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}
function escapeAttr(s) {
  return escapeHtml(s).replace(/'/g, '&#39;');
}
