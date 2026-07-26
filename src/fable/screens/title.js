// =============================================================
// SCREEN: TITLE — the boot intro for the fable app.
// Pure-DOM: builds the title screen markup, exposes onAction callbacks.
// Button order (top→bottom): Continue / New Game / Quick Play / Load / Exit.
//
// MENU STATE: the 4 menu buttons (Continue / New Game / Quick Play / Load)
// are currently NO-OPS — their flows are being rebuilt. The buttons stay in
// the DOM (clickable, but their handlers do nothing) so the title layout is
// intact for the new UI to wire into. Only EXIT is a real action (closes
// the app via the lifecycle manager).
//
// PARTICLES: the floating pollen/spore motes are a canvas particle system
// (see particles.js) mounted into .fable-title-leaves. It's started when
// the title shows + destroyed when the title hides so no RAF leaks across
// screen changes. The particle RAF self-pauses on document.hidden to match
// the OS shell + the app-lifecycle onPause discipline.
//
// GRASS: a canvas blade field rooted along the bottom edge (see grass.js).
// Three depth layers of independently swaying blades — the dense, living
// meadow fringe that replaced the earlier static SVG tint. Same lifecycle
// discipline as the particles: fresh per show, torn down on hide.
// =============================================================

import { createTitleParticles } from './particles.js';
import { createTitleGrass } from './grass.js';
import { createWindLeaves } from './leaves.js';
import { createCloudLayer } from './clouds.js';
import { createTitleSparkle } from './sparkle.js';
import { invoke } from '@tauri-apps/api/core';

export function buildTitle(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-title-screen';
  root.dataset.fableScreen = 'title';
  root.hidden = true;
  root.innerHTML = `
    <!-- Slow horizon clouds (top edge, BEHIND the dim). Pure CSS: a
         seamless-looping cloud layer drifting left via a 90s translate3d
         @keyframes (compositor-only, no per-frame JS). Placed before the
         dim so the darkening softens it into distant scenery — alpha is
         set high enough to survive the ~0.32 dim and read clearly. -->
    <div class="fable-cloud-layer" aria-hidden="true"></div>
    <!-- Particle host (behind the dim, behind the content). The canvas
         particle system mounts its <canvas> in here on show(). Each mote
         drifts/sways/breathes independently for a real floating-pollen
         read (canvas, not CSS — see particles.js). -->
    <div class="fable-title-leaves" aria-hidden="true"></div>
    <!-- Wind-blown leaves host (behind the dim). A pooled DOM system
         (see leaves.js): ≤5 autumn leaves drift across left→right via a
         pure-CSS transform/opacity animation. Recycled, never GC'd. -->
    <div class="fable-wind-leaves" aria-hidden="true"></div>
    <div class="fable-title-dim" aria-hidden="true"></div>
    <!-- Swaying grass fringe along the bottom edge (Chloe 2026-07-23).
         A canvas blade field mounts into here on show() — three depth
         layers of independently swaying blades (back mat → mid → tall
         front tufts), drawn vividly as actual grass (NOT soft-light
         tinted into the bg). The host is 90px tall so the tallest front
         blades fit; it roots at the very bottom, clear of the buttons. -->
    <div class="fable-grass-container" aria-hidden="true"></div>
    <!-- TITLE + BUTTONS ARE TWO INDEPENDENT ENTITIES. Each is a direct,
         absolutely-positioned child of .fable-title-screen, anchored to its
         own coordinates. Neither is nested inside the other, so resizing,
         removing, or re-adding the wordmark can NEVER move the buttons (and
         vice versa). This is the load-bearing structural contract.
         Previously they shared one flex column (.fable-title-content) — that
         coupled their layout and every title tweak shoved the buttons. -->
    <!-- Wordmark host. Hosts the FABLE title PNG (.fable-title-img) as an
         INDEPENDENT entity from the buttons below. The wordmark is a sibling
         of .fable-title-actions, never nested in it — so the image and the
         buttons can never push each other. The PNG is rendered at its NATURAL
         size (1400x425); sizing/positioning lives in fable.css. -->
    <div class="fable-title-wordmark" aria-hidden="true">
      <img class="fable-title-img" src="/fable_title.png" alt="">
    </div>
    <!-- Menu buttons — independent of the wordmark. Positioned via
         .fable-title-actions in fable.css. -->
    <div class="fable-title-actions">
      <!-- The 4 menu buttons are no-ops for now (their UI is being redone).
           handlers will be empty functions; the buttons stay clickable so the
           title layout is intact. Only Exit is a real action.
           CONTINUE ships DISABLED by default (the safe state — dim + no
           click) and is only enabled by _refreshContinue once a resume
           target is confirmed via the fable_continue_target IPC. This is
           the load-bearing default: in a fresh browser build with no
           backend, Continue stays dim + unclickable rather than firing
           a no-op click. -->
      <button class="fable-title-btn" data-act="continue" disabled>Continue</button>
      <button class="fable-title-btn" data-act="new">New Game</button>
      <button class="fable-title-btn" data-act="quickplay">Quick Play</button>
      <button class="fable-title-btn" data-act="load">Load</button>
      <button class="fable-title-btn" data-act="exit">Exit</button>
    </div>
  `;
  root.querySelectorAll('[data-act]').forEach((btn) => {
    btn.addEventListener('click', () => {
      // Disabled buttons (e.g. CONTINUE with no saves) must do nothing — the
      // native :disabled styling already blocks the pointer cursor + hover,
      // but a stray click could still fire; this guard is belt-and-suspenders.
      if (btn.disabled) return;
      const act = btn.dataset.act;
      handlers[act] && handlers[act]();
    });
  });

  // ── CONTINUE enable/disable (the resume-state gate) ───────────────
  // CONTINUE reads "the very last save you left off on." Per the locked
  // contract: it is DIMMED (disabled) when the user has no manual or quick
  // save anywhere (autosaves don't count — `fable_continue_target` excludes
  // them). The :disabled visual (fade + desaturate + no-hover) already lives
  // in fable.css; this method just toggles the disabled attribute based on
  // whether a resume target exists. Called on every title show via
  // _startAmbient (fresh save state each visit) + best-effort (an IPC error
  // leaves the button enabled rather than dead-locking the title).
  //
  // NOTE: enabling the button is the ONLY thing wired here. The actual resume
  // flow (load the target save + enter the stage) is part of the 4 menu flows
  // being rebuilt — `handlers.continue` is still a no-op in fable.js. This
  // just makes the button correctly reflect resumability in the meantime.
  const continueBtn = root.querySelector('[data-act="continue"]');
  root._refreshContinue = async () => {
    if (!continueBtn) return;
    try {
      const target = await invoke('fable_continue_target');
      // target is null when no manual/quick save exists → dim CONTINUE.
      // A returned SaveMeta → enable it.
      continueBtn.disabled = !target;
      // Stash the target so the (future) continue handler can resume it
      // without a second IPC round-trip.
      root._continueTarget = target || null;
    } catch (err) {
      // IPC failure: leave CONTINUE DISABLED (the safe default). The button
      // ships disabled in markup; without a confirmed resume target from
      // the backend we can't know whether a save exists, so dim + lock it.
      // This is the load-bearing "Continue must be unclickable in a fresh
      // browser build" behavior — the prior "default to enabled" path let
      // a no-op click fire when the IPC was unreachable.
      console.error('[fable] continue-target check failed, leaving disabled', err);
      continueBtn.disabled = true;
      root._continueTarget = null;
    }
  };

  // ── Ambient canvas/DOM systems lifecycle ──────────────────
  // All ambient systems (clouds + floating motes + grass blades + wind
  // leaves) are created when the title shows + destroyed when it hides.
  // Fresh per show so nothing leaks across screen changes or exit/relaunch.
  // The methods below are the contract fable.js calls via the show/hide
  // + close paths.
  let particles = null;
  let grass = null;
  let leaves = null;
  let clouds = null;
  let sparkle = null;
  const particleHost = root.querySelector('.fable-title-leaves');
  const grassHost = root.querySelector('.fable-grass-container');
  const leavesHost = root.querySelector('.fable-wind-leaves');
  const cloudHost = root.querySelector('.fable-cloud-layer');
  // Sparkle overlays the TITLE image itself (the subtle twinkle on the gold
  // wordmark), so its host is the wordmark container — NOT the screen. It
  // shrink-wraps the <img>, so the canvas covers exactly the lettering.
  const sparkleHost = root.querySelector('.fable-title-wordmark');
  root._startAmbient = () => {
    if (!particles) particles = createTitleParticles(particleHost);
    if (!grass)     grass     = createTitleGrass(grassHost);
    if (!leaves)    leaves    = createWindLeaves(leavesHost);
    if (!clouds)    clouds    = createCloudLayer(cloudHost);
    if (!sparkle)   sparkle   = createTitleSparkle(sparkleHost);
    // Refresh the CONTINUE button's resume state on every show (a save may
    // have been written/loaded since the last visit). Fire-and-forget — the
    // ambient show shouldn't block on the IPC.
    if (root._refreshContinue) root._refreshContinue();
  };
  root._stopAmbient = () => {
    if (particles) { particles.destroy(); particles = null; }
    if (grass)     { grass.destroy();     grass = null; }
    if (leaves)    { leaves.destroy();    leaves = null; }
    if (clouds)    { clouds.destroy();    clouds = null; }
    if (sparkle)   { sparkle.destroy();   sparkle = null; }
  };

  // ── Wordmark = PNG (no JS styling) ────────────────────────
  // The FABLE wordmark is a single image: public/fable_title.png, rendered
  // via <img class="fable-title-img"> above (sized in fable.css). The prior
  // CSS-text approach (per-letter spans + fill/border/glow experiments) is
  // gone. No JS work happens here for the title.
  //
  // DESIGN HISTORY (tombstone — the CSS-text title is retired):
  //   - Per-letter font spans (F=Uncial Antiqua, ABL=Cinzel Decorative,
  //     E=Almendra) with a gradient fill + stroke border + glow: many
  //     iterations, all problematic (grain blends muddied the gold, text-
  //     stroke drew inner-line artifacts on the F, text-shadow hit a WebKit
  //     bug, ::before underlays didn't read right). Replaced by a hand-
  //     polished PNG — far easier to author effects against a fixed image.
  //   - fableTitleSheen sweep / fableTitleAuraPulse / fableTitleIgnite:
  //     all killed — no title animation.
  // If a future directive wants animation on the PNG, it can target
  // .fable-title-img directly.
  return root;
}
