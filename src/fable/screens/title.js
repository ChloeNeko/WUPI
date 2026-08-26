// =============================================================
// SCREEN: TITLE — the boot intro for the fable app.
// Pure-DOM: builds the title screen markup, exposes onAction callbacks.
// Button order (top→bottom): Continue / New Game / Load / Exit.
//
// MENU STATE: the menu buttons (Continue / New Game / Load / Online / Exit)
// are wired to real handlers in fable.js. Continue resumes the freshest New
// Game save (target stashed by _refreshTitleGate); New Game opens the card
// picker; Load opens the three-way LOAD split (PLAYER / SIM / CODEX, the
// 2026-08-23 rework); Online opens the in-Fable API connection window. EXIT
// is the only close path
// (closes the app via the lifecycle manager).
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
// The 2s fade-to-black → swap → 2s reveal cinematic. Used to wrap the
// instant-jump menu buttons (New Game / Load / Exit) so all five share the
// same fade hand-off.
import { playMagicalTransition } from '../engine/transition.js';
// The FABLE wordmark. Moved from public/ (served at /fable_title.png, flat
// at the install root) into src/fable/assets/ so Vite processes + hashes it
// into dist/assets/ (matches how paw.png is handled). Keeps the install
// root clean — only wupi.exe/html + assets/ + bin/ + data/ + msvcp140.dll.
import fableTitleUrl from '../assets/fable_title.png';
// The menu-button press SFX — a bundled asset (Vite hashes it into assets/,
// same idiom as fable_ripple.mp3). Plays on every main-menu button press.
import BUTTON_SFX_SRC from '../assets/fableButtonSFX.mp3';
import { invoke } from '@tauri-apps/api/core';

// Play the menu-button SFX on every press. One-shot <audio> node that
// self-removes on ended/error so nothing leaks across presses. Swallows
// autoplay rejection silently (the button click IS the user gesture, so it
// will normally play). 2026-08-15 Chloe: lowered a further 40% (0.6 → 0.36)
// — the authored master still read too loud against the title theme.
const BUTTON_SFX_VOLUME = 0.36;
function playButtonSfx() {
  const audio = document.createElement('audio');
  audio.src = BUTTON_SFX_SRC;
  audio.volume = BUTTON_SFX_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  const cleanup = () => { if (audio.parentNode) audio.parentNode.removeChild(audio); };
  audio.addEventListener('ended', cleanup, { once: true });
  audio.addEventListener('error', cleanup, { once: true });
  document.body.appendChild(audio);
  const p = audio.play();
  if (p && typeof p.catch === 'function') p.catch(cleanup);
}

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
      <img class="fable-title-img" src="${fableTitleUrl}" alt="">
    </div>
    <!-- Menu buttons — independent of the wordmark. Positioned via
         .fable-title-actions in fable.css. -->
    <div class="fable-title-actions">
      <!-- Bold brass "API NOT CONNECTED" warning — sits directly above the
           Continue button. Shown by _refreshTitleGate ONLY when no API is
           connected; hidden (via [hidden]) the moment an API connects. -->
      <div class="fable-api-warn" hidden>API Not Connected</div>
      <!-- All menu buttons are wired to handlers in fable.js.
           CONTINUE / NEW GAME / LOAD all ship DISABLED by default (the safe
           state — dim + no click). _refreshTitleGate enables NEW + LOAD once
           an API is connected, and CONTINUE once an API is connected AND a
           resume target is confirmed via fable_continue_target. ONLINE opens
           the in-Fable API connection window (between Load and Exit); EXIT
           closes the app. This load-bearing default keeps the game buttons
           dim + unclickable in a fresh build with no backend / no API rather
           than firing a no-op click. -->
      <button class="fable-title-btn" data-act="continue" disabled>Continue</button>
      <button class="fable-title-btn" data-act="new" disabled>New Game</button>
      <button class="fable-title-btn" data-act="load" disabled>Load</button>
      <button class="fable-title-btn fable-title-btn--online" data-act="online">Online</button>
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
      const handler = handlers[act];
      if (!handler) return;
      // Every menu press plays the button SFX (the authored press cue).
      try { playButtonSfx(); } catch (e) { /* autoplay blocked: silent */ }
      // NEW GAME + LOAD own their OWN transitions + audio hand-offs inside
      // their handlers (onNewGameClicked / onLoadClicked — the theme cuts at
      // click, a longer black hold, and the new tracks start at the reveal).
      // Wrapping either in the title-level fade here would DOUBLE-FADE: two
      // stacked overlays crossfade, and the title briefly shows through in the
      // gap between their midpoints (the black → menu → black flicker). So
      // 'new' + 'load' run their handlers directly.
      // CONTINUE is left to its own handler too — resumeSave wraps the stage
      // entry in the same black cinematic (viaTransition, 2026-08-21), so a
      // title-level fade here would double-fade.
      // EXIT is the only one wrapped: it jumps (closes the app) with no
      // transition of its own, so the 2s fade-to-black → close gives it the
      // same hand-off as the other buttons.
      if (act === 'exit') {
        playMagicalTransition({ onMidpoint: () => { try { handler(); } catch (e) { console.error('[fable] title handler "' + act + '" threw', e); } } })
          .catch((e) => { console.error('[fable] title fade failed, running handler directly', e); try { handler(); } catch (_) {} });
      } else {
        handler();
      }
    });
  });

  // ── CONTINUE enable/disable (the resume-state gate) ───────────────
  // CONTINUE reads "the very last save you left off on" for a New Game world.
  // Per the locked contract: it is DIMMED (disabled) when the user has no
  // save for any card (autosaves DO count — `fable_continue_target`
  // includes them). The :disabled visual (fade + desaturate + no-hover)
  // already lives in fable.css; this method just toggles the disabled
  // attribute based on whether a resume target exists. Called on every title
  // show via
  // _startAmbient (fresh save state each visit) + best-effort (an IPC error
  // leaves the button disabled rather than dead-locking the title).
  //
  // The stashed target (`root._continueTarget`) carries card_id + save_id; the
  // continue handler in fable.js (onContinueClicked) resumes it via
  // fable_start — no second IPC round-trip needed.
  const continueBtn = root.querySelector('[data-act="continue"]');
  const newBtn = root.querySelector('[data-act="new"]');
  const loadBtn = root.querySelector('[data-act="load"]');
  const apiWarnEl = root.querySelector('.fable-api-warn');

  // ── Title API + resume gate (the new Fable entry contract) ─────────
  // Replaces the old CONTINUE-only _refreshContinue. Narration is API-only,
  // so the three game buttons (Continue / New Game / Load) are grayed out
  // (disabled) until an API is connected; a bold brass "API NOT CONNECTED"
  // label sits above Continue while that's the case. The ONLINE button (never
  // disabled) opens the in-Fable API window; once an API connects it calls
  // back here to re-enable the buttons + hide the warning. Called on every
  // title show via _startAmbient + best-effort (an IPC error leaves the game
  // buttons disabled — the safe default).
  // (2026-08-16 yellow J10) Refresh-sequencing token: concurrent gate
  // refreshes (an ONLINE close callback racing a title re-show) could resolve
  // out of order, leaving a STALE `_continueTarget` + button states from the
  // earlier snapshot. Each refresh bumps the token; a superseded one stops
  // touching the DOM/stash at its first await.
  let gateSeq = 0;
  root._refreshTitleGate = async () => {
    const seq = ++gateSeq;
    // API state from the backend — the same check launchFable used to make.
    let apiReady = false;
    try {
      const extra = await invoke('model_source_get');
      if (seq !== gateSeq) return; // superseded by a newer refresh
      apiReady = !!(extra && extra.source === 'api' && extra.apiReady);
    } catch (err) {
      // IPC failure: treat as not-ready (the safe default — game buttons stay
      // disabled + the warning shows). Don't dead-lock: the ONLINE button is
      // always clickable so the user can still attempt a connection.
      console.error('[fable] model_source_get failed; leaving game buttons disabled', err);
    }
    // NEW / LOAD: need only an API connection.
    if (newBtn) newBtn.disabled = !apiReady;
    if (loadBtn) loadBtn.disabled = !apiReady;
    // The brass warning tracks the API state exactly.
    if (apiWarnEl) apiWarnEl.hidden = apiReady;
    // CONTINUE: needs an API connection AND a resume target. Stashes the
    // target (card_id + save_id) so onContinueClicked resumes it without a
    // second IPC round-trip.
    if (!continueBtn) return;
    if (!apiReady) {
      // No API → no point querying saves; keep CONTINUE disabled.
      continueBtn.disabled = true;
      return;
    }
    try {
      const target = await invoke('fable_continue_target');
      if (seq !== gateSeq) return; // superseded — don't stash a stale target
      // target is null when no manual/quick save exists → dim CONTINUE.
      // A returned SaveMeta → enable it.
      continueBtn.disabled = !target;
      root._continueTarget = target || null;
    } catch (err) {
      // IPC failure: leave CONTINUE DISABLED (the safe default). Without a
      // confirmed resume target we can't know whether a save exists.
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
    // Refresh the API + resume gate on every show (an API may have been
    // connected/disconnected, or a save written/loaded, since the last
    // visit). Fire-and-forget — the ambient show shouldn't block on the IPC.
    if (root._refreshTitleGate) root._refreshTitleGate();
  };
  root._stopAmbient = () => {
    if (particles) { particles.destroy(); particles = null; }
    if (grass)     { grass.destroy();     grass = null; }
    if (leaves)    { leaves.destroy();    leaves = null; }
    if (clouds)    { clouds.destroy();    clouds = null; }
    if (sparkle)   { sparkle.destroy();   sparkle = null; }
  };

  // ── Wordmark = PNG (no JS styling) ────────────────────────
  // The FABLE wordmark is a single image: src/fable/assets/fable_title.png,
  // imported at the top of this module (Vite hashes it into dist/assets/),
  // rendered via <img class="fable-title-img"> above (sized in fable.css).
  // The prior CSS-text approach (per-letter spans + fill/border/glow
  // experiments) is gone. No JS work happens here for the title.
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
