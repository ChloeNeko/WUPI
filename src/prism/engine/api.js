// =============================================================
// PRISM API — the Tauri IPC wrappers for the Prism app.
//
// Thin typed wrappers over `invoke()` for every Prism command registered
// in lib.rs's invoke_handler. The screens import these (never call invoke
// directly) so the IPC contract is in one place + the arg shapes match the
// Rust structs (prism::GenerateParams, prism::GalleryFilter, etc.).
//
// The generation flow is async via the `prism-gen-done` event (NOT a return
// value): prism_generate returns immediately with `{pending, path}`; the
// screen subscribes to the event for the final image row. This mirrors the
// FABLE scene-image pattern (the swap cycle is multi-second).
// =============================================================

import { invoke } from '@tauri-apps/api/core';

// ── SD status / latch ───────────────────────────────────────────────────

/**
 * @returns {Promise<{model_present: boolean, model_name: string, disabled: boolean, backend_real: boolean}>}
 * `backend_real` is false when the build lacks the `diffusion-rs` cargo feature
 * (the default) → generate() writes an EMPTY file + returns instantly (no
 * image renders). The UI surfaces a banner so the user isn't confused.
 */
export function sdStatus() {
  return invoke('prism_sd_status');
}

/** Clear the one-strike failure latch (after the user acks a prior failure). */
export function clearLatch() {
  return invoke('prism_sd_clear_latch');
}

// ── Generation ──────────────────────────────────────────────────────────

/**
 * Kick off one generation. Returns immediately with the resolved dest path
 * (`pending: true`); the final result arrives via the `prism-gen-done` event.
 *
 * @param {object} params — the GenerateParams. LOCKED RECIPE: only
 *   `prompt`, `seed`, `width`, `height` are live; `cfg`/`steps`/`sampler`/
 *   `negative_prompt` remain accepted by the IPC but are IGNORED — Rust
 *   enforces the locked NoobAI v1.1 official recipe (Euler a + discrete,
 *   30 steps, CFG 6.0, injected quality prefix + negative block).
 * @returns {Promise<{pending: boolean, path: string}>}
 */
export function generate(params) {
  return invoke('prism_generate', { params });
}

// ── Gallery CRUD ────────────────────────────────────────────────────────

/**
 * List gallery images (the masonry grid). Newest first, paginated.
 * @param {object} filter — { favorites_only?, search? }
 * @returns {Promise<Array<GalleryImage>>}
 */
export function galleryList(filter = {}, limit = 100, offset = 0) {
  return invoke('prism_gallery_list', { filter, limit, offset });
}

/** @param {number} id @returns {Promise<GalleryImage|null>} */
export function galleryGet(id) {
  return invoke('prism_gallery_get', { id });
}

/** Toggle favorite. @param {boolean} fav */
export function galleryFavorite(id, fav) {
  return invoke('prism_gallery_favorite', { id, fav });
}

/**
 * PERMANENT delete: remove the row + unlink the PNG. There is NO trash /
 * restore — one click and the image is gone, period (2026-08-18 ruling).
 * @param {number} id
 */
export function galleryDelete(id) {
  return invoke('prism_gallery_delete', { id });
}

// ── Generation defaults (mirror prism.rs's serde defaults) ──────────────
//
// LOCKED RECIPE (Chloe ruling, 2026-08-17): sampler (Euler a + discrete),
// steps (30), CFG (6.0) + the quality/negative meta are SERVER-SIDE
// constants enforced in prism::build_request — they are NOT settings and
// have no client representation. The only defaults that matter here are
// the two user-facing knobs: the default size (the portrait NoobAI bucket)
// + the random seed.

export const GEN_DEFAULTS = {
  seed: -1,        // -1 = random; >= 0 = locked (Fork & Edit)
  width: 832,
  height: 1216,
};
