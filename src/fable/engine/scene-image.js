// =============================================================
// SCENE IMAGE — the Phase 5B backdrop subscriber.
//
// Subscribes to the two app-wide events the SD swap orchestrator
// emits from `fable_send`'s detached done-beat spawn (lib.rs):
//   - 'fable-scene-image' { path }  → a fresh scene PNG is on disk;
//     convert via the asset protocol + swap into the dormant backdrop.
//   - 'fable-scene-failed' { message } → generation failed + the
//     one-strike latch tripped; surface a one-shot toast.
//
// Why app-wide `listen` (not the per-turn Channel): the SD swap runs
// SECONDS after the turn's `done` emit — the LLM⇄SD⇄LLM cycle evicts
// + reloads the weights. By the time the image lands, the fable_send
// Channel is closed. The Tauri event bus is the established pattern
// for late-arriving signals (mirrors 'model-status' / 'interview-fact').
//
// Render target: the pre-built dormant backdrop `.fable-stage-bg >
// [data-bg]` (stage.js:82, fable.css:881). It ships hidden with an
// empty src; the FIRST scene image un-hides it. CSS already has
// object-fit:cover + a breathe animation.
// =============================================================

import { listen } from '@tauri-apps/api/event';
import { convertFileSrc } from '@tauri-apps/api/core';

// Stored unlisten fns (mirrors interview.js:87). Nulled in teardown so a
// close-mid-generation can't swap an image into a torn-down stage.
let unlistenImage = null;
let unlistenFailed = null;
let bgImg = null;          // the [data-bg] <img>
let bgLayer = null;        // the .fable-stage-bg wrapper (un-hides on first img)
let onToast = null;        // fn(msg) → surface the failed-latch message

export function initSceneImage(stageEl, hooks = {}) {
  bgImg = stageEl.querySelector('[data-bg]');
  bgLayer = stageEl.querySelector('.fable-stage-bg');
  onToast = hooks.onToast || null;

  // The scene image: convert the disk path to an asset:// URL the webview
  // can load (asset protocol scoped to apps/fable/backgrounds/**), set it as
  // the <img> src, + un-hide the wrapper layer on the first arrival.
  listen('fable-scene-image', (e) => {
    const payload = e && e.payload;
    if (!payload || !payload.path || !bgImg) return;
    const url = convertFileSrc(payload.path);
    bgImg.src = url;
    if (bgLayer) bgLayer.style.display = 'block';
  }).catch((err) => {
    console.error('[scene-image] listen(fable-scene-image) failed', err);
  }).then((un) => { unlistenImage = un; });

  // The failure latch: generation OOM'd/errored + the one-strike latch is
  // now tripped Rust-side (auto-gen is hard-off until a user ack). Surface a
  // one-shot toast so the player knows why images stopped — do NOT spam
  // (the orchestrator fires this once per failure, not per turn).
  listen('fable-scene-failed', (e) => {
    const payload = e && e.payload;
    const msg = (payload && payload.message) || 'Scene generation failed; auto-gen disabled.';
    if (onToast) onToast(msg);
  }).catch((err) => {
    console.error('[scene-image] listen(fable-scene-failed) failed', err);
  }).then((un) => { unlistenFailed = un; });
}

// Hard reset (mirrors interview.js:840). Called from teardownStage so a
// close mid-generation can't swap an image into a torn-down backdrop. The
// bg layer is NOT re-hidden here — leaving the last scene visible across a
// brief teardown/re-wire is the lesser flicker (matches beats.clearFeed
// not touching the backdrop). The next session's wireStage re-binds.
export function teardownSceneImage() {
  if (unlistenImage) { try { unlistenImage(); } catch (_) {} unlistenImage = null; }
  if (unlistenFailed) { try { unlistenFailed(); } catch (_) {} unlistenFailed = null; }
  bgImg = null;
  bgLayer = null;
  onToast = null;
}
