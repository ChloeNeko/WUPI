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
 * @param {object} params — the GenerateParams (prompt, negative_prompt?,
 *   seed, cfg, steps, width, height, sampler). Defaults applied Rust-side.
 * @returns {Promise<{pending: boolean, path: string}>}
 */
export function generate(params) {
  return invoke('prism_generate', { params });
}

// ── Gallery CRUD ────────────────────────────────────────────────────────

/**
 * List gallery images (the masonry grid). Newest first, paginated.
 * @param {object} filter — { favorites_only?, trashed_only?, search? }
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

/** Soft-delete (move to trash). */
export function galleryTrash(id) {
  return invoke('prism_gallery_trash', { id });
}

/** Restore from trash. */
export function galleryRestore(id) {
  return invoke('prism_gallery_restore', { id });
}

/** Hard-delete: remove the row + unlink the PNG. */
export function galleryPurge(id) {
  return invoke('prism_gallery_purge', { id });
}

// ── Sampler catalog (the dropdown source of truth) ─────────────────────
//
// Mirrors the i32 discriminants in scene_art.rs's sampler_from_i32 (the C
// enum sample_method_t ordering). 18 real variants (the 19th is the COUNT
// sentinel — never exposed). The `value` is what crosses the IPC; the Rust
// side maps it back to the crate enum. If stable-diffusion.cpp adds a
// variant, extend BOTH this list + sampler_from_i32.

export const SAMPLERS = [
  { value: 0,  label: 'Euler' },
  { value: 1,  label: 'Euler a' },
  { value: 2,  label: 'Heun' },
  { value: 3,  label: 'DPM2' },
  { value: 4,  label: 'DPM++ 2S a' },
  { value: 5,  label: 'DPM++ 2M' },          // default (DPMPP2M_DISCRIMINANT)
  { value: 6,  label: 'DPM++ 2M v2' },
  { value: 7,  label: 'IPNDM' },
  { value: 8,  label: 'IPNDM v' },
  { value: 9,  label: 'LCM' },                // for LCM-LoRA acceleration
  { value: 10, label: 'DDIM trailing' },
  { value: 11, label: 'TCD' },
  { value: 12, label: 'Res multistep' },
  { value: 13, label: 'Res 2S' },
  { value: 14, label: 'ER SDE' },
  { value: 15, label: 'Euler CFG++' },
  { value: 16, label: 'Euler a CFG++' },
  { value: 17, label: 'Euler GE' },           // guidance-embedded (Flux/SD3)
];

/** The default sampler value (DPM++ 2M, the SDXL clean baseline). */
export const DEFAULT_SAMPLER = 5;

// ── Generation defaults (mirror prism.rs's serde defaults) ──────────────

export const GEN_DEFAULTS = {
  seed: -1,        // -1 = random; >= 0 = locked (Fork & Edit)
  cfg: 5.0,
  steps: 28,
  width: 1024,
  height: 576,
  sampler: DEFAULT_SAMPLER,
};
