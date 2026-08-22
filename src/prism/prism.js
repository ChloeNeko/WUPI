// =============================================================
// PRISM APP ENTRY — composition root for the image-gen subsystem.
//
// Prism is a REGISTERED APP under the WUPI app-lifecycle framework
// (src/app-lifecycle.js): onOpen / onClose / onPause / onResume. Same shape
// as Fable — a dedicated full-screen experience with NO window bar. The only
// way back to the OS desktop is the EXIT button (top-right), which calls
// AppLifecycle.closeApp('prism') → full teardown.
//
// Lifecycle contract (the GUARANTEE: zero memory leaks, zero background
// listeners):
//   onOpen   → show #prism, hide OS chrome, pause OS aurora, show composer,
//              re-attach the screens' window/document-level listeners.
//   onPause  → (alt-tab) nothing to freeze yet (no audio/RAF in v1).
//   onResume → (focus return) no-op mirror.
//   onClose  → full teardown: tear down every screen (clear the composer's
//              generate failsafe timer + unlock its button, remove the
//              gallery's window-scroll + the fork slider's window mouse/touch
//              listeners), drop the outstanding render tags, restore OS
//              chrome, resume aurora, close the OS window slot. The gen-done
//              subscription itself is process-static (see initPrism) — a
//              post-close event finds no render tag and is ignored.
//
// SCREENS (the router toggles one visible at a time):
//   composer → the Tag Composer (primary entry point).
//   gallery  → the Glass Vault (masonry grid + metadata panel).
//   fork     → Fork & Edit (seed-locked A/B + drag-compare slider).
//
// GENERATION FLOW: the composer/fork call prism_generate (returns immediately
// with a pending path). The result arrives seconds later via the `prism-gen-
// done` event (the SD swap evicts/reloads the text models — multi-second).
// This module owns the single gen-done subscriber + routes the result to the
// screen that ORIGINATED the render (origin-tag routing, 2026-08-20 audit
// M8 — never to whichever screen is active when the swap finishes: composer
// re-enables its button; fork swaps B's image).
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { AppLifecycle } from '../app-lifecycle.js';
import './prism.css';

import { activateChrome, deactivateChrome } from './engine/chrome.js';
import { sdStatus, clearLatch } from './engine/api.js';
import { buildEl as buildComposer, wire as wireComposer, teardown as teardownComposer,
  rewire as rewireComposer, setDone as composerDone, loadFromImage as composerLoadFromImage } from './screens/composer.js';
import { buildEl as buildGallery, wire as wireGallery, teardown as teardownGallery,
  rewire as rewireGallery, refresh as galleryRefresh } from './screens/gallery.js';
import { buildEl as buildFork, wire as wireFork, teardown as teardownFork,
  rewire as rewireFork, load as forkLoad, onGenDone as forkOnGenDone,
  onGenFail as forkOnGenFail } from './screens/fork.js';
import { resolveGenDoneTarget } from './engine/routing.js';

let prismRoot = null;       // the #prism app-window element
let screens = {};           // name → { el, wire, teardown }
let activeScreen = 'composer';

// ── Render-origin tracking (the gen-done router's source of truth) ──────
//
// Every generate call is tagged AT RENDER START with its origin screen +
// (once the invoke resolves) the dest path prism_generate returned — the
// done event echoes that path back, so completions pair with their render
// by token instead of by whichever screen is active when they land.
// Start-ordered; the SD turn lock serializes the swap cycles server-side,
// so completions fire in start order (engine/routing.js leans on that for
// its FIFO fallback). Close clears the list — a post-close done event
// pairs with nothing and is ignored.
let pendingRenders = [];

// Tag a render BEFORE the generate invoke fires (a fast stub done can
// never beat the tag).
function beginRender(origin) {
  pendingRenders.push({ origin, path: null });
}

// The generate invoke resolved — attach the dest-path token to the most
// recent still-untagged render of this origin (starts are ordered).
function tagRenderPath(origin, path) {
  for (let i = pendingRenders.length - 1; i >= 0; i--) {
    const r = pendingRenders[i];
    if (r.origin === origin && r.path === null) {
      if (path) r.path = String(path);
      return;
    }
  }
}

// The generate invoke REJECTED (never started server-side) — drop the most
// recent still-untagged tag of this origin so the list can't accumulate
// ghosts.
function abortRender(origin) {
  for (let i = pendingRenders.length - 1; i >= 0; i--) {
    const r = pendingRenders[i];
    if (r.origin === origin && r.path === null) {
      pendingRenders.splice(i, 1);
      return;
    }
  }
}

// External hooks set by script.js (the OS chrome integration).
let hooks = { pauseAurora: null, resumeAurora: null, openHooks: null, closeHooks: null, closeWindow: null };

// Whether the app window is currently shown (the static gen-done listener
// uses this to skip post-close side work).
function isOpen() {
  return !!(prismRoot && prismRoot.classList.contains('show'));
}

// ── Router ──────────────────────────────────────────────────────────────

function showScreen(name) {
  activeScreen = name;
  for (const key of Object.keys(screens)) {
    screens[key].el.hidden = (key !== name);
  }
}

// The toast helper (a transient banner at the bottom). Shared by all screens
// via the hooks.onToast they receive.
function toast(msg) {
  if (!msg) return;
  let t = prismRoot.querySelector('.prism-toast');
  if (!t) {
    t = document.createElement('div');
    t.className = 'prism-toast';
    prismRoot.appendChild(t);
  }
  t.textContent = msg;
  t.classList.add('is-visible');
  clearTimeout(t._timer);
  t._timer = setTimeout(() => t.classList.remove('is-visible'), 3500);
}

// ── The router hooks each screen receives ───────────────────────────────
//
// These let a screen navigate or trigger cross-screen flows (e.g. the gallery's
// "Send to Composer" / "Fork" quick actions) without each screen importing the
// others.

function screenHooks() {
  return {
    onToast: (m) => toast(m),
    // Gallery → Composer: load an image's params into the composer + switch.
    onSendToComposer: (img) => {
      composerLoadFromImage(screens.composer.el, img);
      showScreen('composer');
      toast('Loaded into composer — tweak + regenerate.');
    },
    // Gallery → Fork: load an image as the Fork A + switch.
    onFork: (img) => {
      forkLoad(screens.fork.el, img);
      showScreen('fork');
    },
    // Fork → Gallery (Keep B): the B result is already saved; just show it.
    onKeep: () => {
      galleryRefresh(screens.gallery.el);
      showScreen('gallery');
    },
    // Render-origin tagging (the gen-done router pairs completions with
    // these, never with the active screen): a screen announces its render
    // BEFORE the invoke (onRenderStart), attaches the dest-path token once
    // the invoke resolves (onRenderPath), and drops the tag if the invoke
    // rejected outright (onRenderFail).
    onRenderStart: (origin) => beginRender(origin),
    onRenderPath: (origin, path) => tagRenderPath(origin, path),
    onRenderFail: (origin) => abortRender(origin),
  };
}

// ── Lifecycle callbacks (registered with AppLifecycle in initPrism) ─────

// onOpen: show the app, hide chrome, pause aurora, go to composer, subscribe
// to the gen-done event.
function openPrism() {
  if (!prismRoot) return;
  prismRoot.classList.add('show');
  prismRoot.setAttribute('aria-hidden', 'false');
  activateChrome();
  if (hooks.pauseAurora) hooks.pauseAurora();
  showScreen('composer');
  // Re-attach the screens' window/document-level listeners the previous
  // closePrism tore down (idempotent — re-adding the same function refs
  // never double-binds). The gen-done subscription is process-static (see
  // initPrism), so no open can race a completion past its listener.
  if (screens.composer) rewireComposer(screens.composer.el);
  if (screens.gallery) rewireGallery(screens.gallery.el);
  if (screens.fork) rewireFork(screens.fork.el);
  // Refresh the SD status banner (model present? real backend compiled in?
  // latch tripped?). This drives the banner that explains why generation
  // produces an empty file (stub backend) or is blocked (latch).
  refreshStatusBanner();
}

// The SD status banner. Three states:
//   • stub backend (default build, no diffusion-rs feature) → amber banner:
//     "generate writes empty files; rebuild with --features diffusion-rs".
//   • no model in models/sd/ → amber banner: "drop a checkpoint in models/sd/".
//   • latch tripped (a prior gen failed) → red banner + a Retry button that
//     clears the latch.
// All hidden when healthy + real backend + model present.
async function refreshStatusBanner() {
  if (!prismRoot) return;
  let banner = prismRoot.querySelector('.prism-status-banner');
  if (!banner) {
    banner = document.createElement('div');
    banner.className = 'prism-status-banner';
    prismRoot.querySelector('.prism-topbar').after(banner);
  }
  let st;
  try {
    st = await sdStatus();
  } catch (err) {
    banner.className = 'prism-status-banner is-error';
    banner.innerHTML = `Status unavailable: ${escapeText(String(err))}`;
    return;
  }
  if (!st.backend_real) {
    banner.className = 'prism-status-banner is-warn';
    banner.innerHTML = `<strong>Stub backend active.</strong> This build lacks the <code>diffusion-rs</code> cargo feature, so Generate writes empty placeholder files (no real image). Rebuild with <code>--features diffusion-rs</code> to render real images.`;
    return;
  }
  if (!st.model_present) {
    banner.className = 'prism-status-banner is-warn';
    banner.innerHTML = `No Stable Diffusion model found. Drop a checkpoint (<code>.safetensors</code> or <code>.gguf</code>) into <code>models/sd/</code> + reopen Prism.`;
    return;
  }
  if (st.disabled) {
    banner.className = 'prism-status-banner is-error';
    banner.innerHTML = `Generation is disabled (a prior render failed + tripped the safety latch). <button class="prism-status-retry">Retry — clear latch</button>`;
    const btn = banner.querySelector('.prism-status-retry');
    if (btn) btn.addEventListener('click', async () => {
      try { await clearLatch(); } catch (_) {}
      refreshStatusBanner();
    });
    return;
  }
  // Healthy: hide the banner.
  banner.className = 'prism-status-banner';
  banner.innerHTML = '';
}

function escapeText(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// onClose: full teardown (the GUARANTEE). Tear down every screen (each
// teardown is idempotent + owns ONLY its own listeners/timers: the
// composer's failsafe timer + locked Generate button + document click-out,
// the gallery's window scroll + any armed delete, the fork slider's window
// mouse/touch listeners + stale rendering state), drop the outstanding
// render tags (a done event for an in-flight render arriving after close
// pairs with nothing and is ignored — the image still lands in the
// gallery; the next open's refresh shows it), restore chrome, resume
// aurora, sync the OS window set. Runs inside AppLifecycle.closeApp's
// transitioning guard, so a re-entrant closeApp here is a safe no-op.
function closePrism() {
  try {
    if (screens.composer) teardownComposer(screens.composer.el);
    if (screens.gallery) teardownGallery(screens.gallery.el);
    if (screens.fork) teardownFork(screens.fork.el);
    pendingRenders.length = 0;
  } finally {
    // Restore OS chrome + aurora ALWAYS (even if something above threw).
    deactivateChrome();
    if (hooks.resumeAurora) hooks.resumeAurora();
    if (prismRoot) {
      prismRoot.classList.remove('show');
      prismRoot.setAttribute('aria-hidden', 'true');
    }
    // Sync the OS openWindows set (mirrors Fable's closeWindow re-entry path).
    if (hooks.closeWindow) {
      try { hooks.closeWindow('prism'); } catch (_) {}
    }
  }
}

function pausePrism() {
  // v1: no audio/RAF to freeze. Placeholder for future render-preview pausing.
}

function resumePrism() {
  // v1: nothing to unfreeze.
}

// ── The gen-done router ─────────────────────────────────────────────────
//
// The single subscriber for `prism-gen-done`. Routes the result by RENDER
// ORIGIN (engine/routing.js pairs the payload with its tagged render), NOT
// by the active screen: a composer render re-enables the Generate button
// wherever the user has since navigated (and never touches Fork's B layer);
// a fork render swaps B + clears `.is-rendering` wherever it lands. On
// failure, surface a toast. Unmatched payloads (no outstanding render —
// stale or post-close) are ignored.

function onGenDone(payload) {
  if (!payload) return;
  const target = resolveGenDoneTarget(pendingRenders, payload);
  if (!target) return;
  pendingRenders.splice(target.index, 1);
  if (payload.ok && payload.image) {
    if (target.origin === 'fork') {
      forkOnGenDone(screens.fork.el, payload);
      toast('B regenerated.');
    } else {
      composerDone(screens.composer.el);
      toast('Image generated.');
    }
    // Always refresh the gallery so the new thumbnail appears (background,
    // non-blocking — the user stays on whatever screen they're on).
    galleryRefresh(screens.gallery.el);
  } else {
    // Failure: a pathless failure payload can't name its origin, so reset
    // BOTH surfaces defensively (each reset is idempotent) + surface the
    // error. Only the matched tag is consumed — a queued sibling render
    // still gets its own done event (the latch-skipped branch emits one),
    // and its tag stays paired for it.
    composerDone(screens.composer.el);
    // Fork's reset re-arms its regen button + clears the B shimmer (the
    // 2026-08-20 guard: the button now stays disabled until a routed
    // done/fail arrives, so a failure MUST re-arm it or it sticks).
    forkOnGenFail(screens.fork.el);
    if (payload.cancelled) {
      toast('Generation cancelled.');
    } else {
      toast(payload.error || 'Generation failed.');
    }
  }
}

// ── Launch / close bridge (the openHook the OS home tile triggers) ──────

function launchPrism() {
  AppLifecycle.launchApp('prism');
}

// ── initPrism — build the DOM, register, bridge the OS hooks ────────────

export function initPrism(extHooks = {}) {
  // Merge the OS hooks.
  hooks = { ...hooks, ...extHooks };

  // Build the #prism root (appended to body — NOT in wupi.html, same as
  // Fable). openWindow('prism') looks up getElementById('prism') to resolve.
  prismRoot = document.createElement('div');
  prismRoot.className = 'app-window prism-app';
  prismRoot.id = 'prism';
  prismRoot.setAttribute('aria-hidden', 'true');
  document.body.appendChild(prismRoot);

  // Build the top bar (EXIT + nav).
  const topbar = document.createElement('div');
  topbar.className = 'prism-topbar';
  topbar.innerHTML = `
    <div class="prism-brand">
      <span class="prism-brand-mark">◈</span>
      <span class="prism-brand-name">Prism</span>
    </div>
    <nav class="prism-nav">
      <button class="prism-nav-btn is-active" data-nav="composer">Compose</button>
      <button class="prism-nav-btn" data-nav="gallery">Gallery</button>
      <button class="prism-nav-btn" data-nav="fork">Fork</button>
    </nav>
    <button class="prism-exit" data-nav="exit">✕</button>
  `;
  prismRoot.appendChild(topbar);

  // Build the screens.
  const sh = screenHooks();
  const composerEl = buildComposer(sh);
  const galleryEl = buildGallery(sh);
  const forkEl = buildFork(sh);
  composerEl.classList.add('prism-screen-host');
  galleryEl.classList.add('prism-screen-host');
  forkEl.classList.add('prism-screen-host');
  prismRoot.appendChild(composerEl);
  prismRoot.appendChild(galleryEl);
  prismRoot.appendChild(forkEl);

  screens = {
    composer: { el: composerEl, wired: false },
    gallery: { el: galleryEl, wired: false },
    fork: { el: forkEl, wired: false },
  };

  // Wire all screens up front (cheap; listeners on detached-but-present nodes
  // are fine — the nodes are present in the DOM, just hidden).
  wireComposer(composerEl);
  wireGallery(galleryEl);
  wireFork(forkEl);
  screens.composer.wired = true;
  screens.gallery.wired = true;
  screens.fork.wired = true;

  // Subscribe to gen-done ONCE, statically (2026-08-20 audit LOW: a
  // per-open subscribe could attach after a fast render had already
  // completed and miss its done event; the close/open resubscribe also
  // raced a double listener). Process-lifetime — closePrism never
  // unsubscribes; it clears the render tags instead, so a post-close event
  // pairs with nothing and is ignored (the close-mid-generation guard the
  // old unsubscribe provided).
  listen('prism-gen-done', (e) => {
    onGenDone(e && e.payload);
    // A failure may have tripped the latch — refresh the banner so the
    // "Retry" affordance appears (only meaningful while the app is open).
    if (isOpen()) refreshStatusBanner();
  }).catch((err) => {
    console.error('[prism] listen(prism-gen-done) failed', err);
  });

  // Nav buttons (top bar). EXIT routes to closeApp.
  topbar.querySelectorAll('[data-nav]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const target = btn.dataset.nav;
      if (target === 'exit') { AppLifecycle.closeApp('prism'); return; }
      showScreen(target);
      // Active state on nav buttons.
      topbar.querySelectorAll('.prism-nav-btn').forEach((b) => b.classList.remove('is-active'));
      if (btn.classList.contains('prism-nav-btn')) btn.classList.add('is-active');
      // Refresh gallery when navigated to (picks up new images).
      if (target === 'gallery') galleryRefresh(galleryEl);
    });
  });

  // Show composer first.
  showScreen('composer');

  // Register with AppLifecycle (the OS app-lifecycle manager).
  AppLifecycle.registerApp({
    id: 'prism',
    onOpen: openPrism,
    onClose: closePrism,
    onPause: pausePrism,
    onResume: resumePrism,
  });

  // Bridge the OS home-tile openWindow('prism') → launchPrism() → launchApp.
  // openWindow fires the openHook (if registered); we redirect to the
  // lifecycle manager. Mirrors Fable's hook-bridge.
  if (hooks.openHooks) hooks.openHooks.set('prism', () => launchPrism());
  if (hooks.closeHooks) {
    hooks.closeHooks.set('prism', () => {
      // closeWindow('prism') re-enters here; AppLifecycle.closeApp's
      // transitioning guard makes the re-entry a no-op (the descriptor's
      // onClose isn't re-invoked). Safe.
      AppLifecycle.closeApp('prism');
    });
  }
}
