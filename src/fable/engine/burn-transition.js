// =============================================================
// BURN TRANSITION — the cinematic button-burn effect system.
//
// A self-contained orchestrator for the New Game flow's destructive
// transitions: when the user clicks one button in a pair/group, the
// OTHERS burn away bottom→top (paper catching fire, glowing hot edge
// climbing, ash dissolving via an SVG <feTurbulence> displacement),
// the clicked button POPS (scale-down + amber glow burst), then the
// NEXT pair reverse-spawns (materialize top→bottom, the inverse mask).
//
// DESIGN CONSTRAINTS (from Chloe's spec):
//   • No canvas, no GIFs. SVG <feTurbulence> + <feDisplacementMap>
//     filter linked to a CSS class, COMBINED with a CSS mask-image
//     (linear-gradient) that transitions bottom→top.
//   • A glowing hot orange/red edge rides the mask line — reads as a
//     real burn front, not a wipe.
//   • The selected button pops instantly (scale 0.95 → glow burst → 1).
//   • Reverse-spawn is the burn played backward (mask 100→0).
//
// The mask-driver technique is a per-frame linear-gradient mask rebuild
// (one style write per frame, no layout thrash). NOTE: fog.js's intro fog
// used to share this gradient-mask approach but was rewritten 2026-08-03 to a
// canvas-composited carve (see screens/fog.js); the burn here still uses the
// mask technique because a burn front is a single element's edge, not a
// parallax field — a straight-ish feathered line is exactly right for fire.
// The displacement filter is a single inline <svg> defined ONCE and
// referenced by url(#…) from each burn twin's CSS.
//
// AUDIO: playIgnitionWhoosh() plays the authored Incinerate.mp3 asset at
// volume 0.6 (Chloe 2026-08-03: "Use Incinerate.mp3... volume 0.6 otherwise
// it'll be too loud"). It fires on BOTH the burn (rejected cards incinerate)
// AND the reverse-spawn (cards materialize) — "play the incinerator sound
// when the cards spawn in as well."
//
// Reduced motion: collapse the burn to a 250ms opacity fade (mirrors
// transition.js's discipline + fog.js's reduced-motion short-circuit).
// =============================================================

import INCINERATE_SRC from '../assets/Incinerate.mp3';

// --- Tunables ----------------------------------------------------
// Burn duration. Long enough to read as deliberate fire, short enough
// not to drag the menu flow. ~1s feels like a real burn.
const BURN_MS = 1050;
// How long the CLICKED button takes to fade out AFTER the rejected ones
// finish burning. Slow + graceful (it lingered while the others burned).
const SELECTED_FADE_MS = 650;
// Reverse-spawn: the reverse-burn (mask recedes 100→0, button
// materializes top→bottom). Same mechanics as burn, inverted.
const SPAWN_MS = 900;
// Mask feather width as a fraction of the element height. Wider = a
// broader, softer burn front (no hard horizontal cut line).
const FEATHER = 0.16;
// Turbulence displacement scale (px). Higher = more violent ash-edge
// wobble. Kept modest so the button stays readable until it burns.
const DISPLACE = 14;

// --- Audio: the incinerator sound (Incinerate.mp3 asset) ------------

// The authored fire/incineration cue. Replaces the prior synthesized
// layered-noise fire (which itself replaced the fart-like sub-bass whoosh)
// — the authored asset is the real, final sound.
// Vite-imported at the top of this module (hashed into dist/assets/, same
// idiom as fableButtonSFX.mp3). Played as a one-shot <audio> node at the
// burn/spawn moment: volume 0.6, self-removes on ended/error so nothing
// leaks across plays. Swallows autoplay rejection silently (the click IS
// the user gesture, so it plays). Same one-shot pattern as title.js
// playButtonSfx.

const INCINERATE_VOLUME = 0.6;   // Chloe: "otherwise it'll be too loud"

function reducedMotion() {
  return !!(window.matchMedia &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches);
}

// Play the Incinerate.mp3 cue once. The single source of truth for the
// burn + the reverse-spawn (card materialization) — both fire the same
// incineration sound (Chloe 2026-08-03: "play the incinerator sound when
// the cards spawn in as well").
export function playIgnitionWhoosh() {
  const audio = document.createElement('audio');
  audio.src = INCINERATE_SRC;
  audio.volume = INCINERATE_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  const cleanup = () => { if (audio.parentNode) audio.parentNode.removeChild(audio); };
  audio.addEventListener('ended', cleanup, { once: true });
  audio.addEventListener('error', cleanup, { once: true });
  document.body.appendChild(audio);
  const p = audio.play();
  if (p && typeof p.catch === 'function') p.catch(cleanup);
}

// --- The SVG turbulence filter (defined once) --------------------

// Inject a hidden <svg> holding the <feTurbulence>+<feDisplacementMap>
// filter into the document, ONCE. Each burn twin references it via
// filter: url(#fable-burn-displace). The turbulence's baseFrequency is
// animated per-twin via rAF (writing the filter element's attribute)
// so each burn has its own dissolve life — see driveBurn.
const FILTER_ID = 'fable-burn-displace';
let filterHostMounted = false;
function ensureFilterHost() {
  if (filterHostMounted) return;
  if (document.getElementById(FILTER_ID)) { filterHostMounted = true; return; }
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('aria-hidden', 'true');
  svg.style.cssText = 'position:absolute;width:0;height:0;pointer-events:none;';
  svg.innerHTML = `
    <filter id="${FILTER_ID}" x="-20%" y="-20%" width="140%" height="140%">
      <feTurbulence type="fractalNoise" baseFrequency="0.012 0.02" numOctaves="2" seed="7" result="noise"/>
      <feDisplacementMap in="SourceGraphic" in2="noise" scale="${DISPLACE}" xChannelSelector="R" yChannelSelector="G"/>
    </filter>`;
  document.body.appendChild(svg);
  filterHostMounted = true;
}

// --- The burn core (shared by burn + reverse-spawn) --------------

// Drive a mask-image + displacement burn over `twin` from `from`→`to`
// (burnLine %). resolve() on completion. `dir` selects the gradient
// axis so the same driver does burn (bottom→top, line up) and spawn
// (top→bottom reveal, line down). The hot-edge element's `top` tracks
// the burnLine so the glow rides the mask front.
//   twin   — the overlay clone element (positioned over the original)
//   edge   — the hot-edge ::after stand-in (a real child, since we
//            can't easily animate a pseudo's top from JS)
//   from/to — start/end burnLine (0..100+)
//   durMs  — duration
//   resolve — completion callback
function driveBurn(twin, edge, from, to, durMs, resolve) {
  const filterEl = document.getElementById(FILTER_ID)
    && document.querySelector('#' + FILTER_ID + ' feTurbulence');
  const start = performance.now();
  // Slight ease-in-out so the burn accelerates then settles — reads as
  // fire catching then consuming the last fibers.
  const ease = (t) => t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
  let raf = 0;
  const tick = (now) => {
    const t = Math.min(1, (now - start) / durMs);
    const e = ease(t);
    const line = from + (to - from) * e;
    // Three-stop gradient. Below the line = transparent (gone/showing),
    // the feather = transition, above = opaque (still there / still hidden).
    // For a BURN (to top): opaque above the line, transparent below.
    const featherStart = Math.max(0, line - FEATHER * 50);
    const grad = `linear-gradient(to top, rgba(0,0,0,0) 0%, rgba(0,0,0,0) ${featherStart}%, rgba(0,0,0,1) ${Math.min(100, line)}%)`;
    twin.style.webkitMaskImage = grad;
    twin.style.maskImage = grad;
    // Hot edge rides the burn front. Position it at the line.
    if (edge) edge.style.top = `${Math.max(0, Math.min(100, 100 - line))}%`;
    // Animate the turbulence so the ash edge wobbles/violates over time.
    if (filterEl) {
      const f = (0.010 + 0.020 * e).toFixed(4);
      filterEl.setAttribute('baseFrequency', `${f} ${(parseFloat(f) * 1.6).toFixed(4)}`);
    }
    if (t < 1) raf = requestAnimationFrame(tick);
    else { cancelAnimationFrame(raf); resolve(); }
  };
  raf = requestAnimationFrame(tick);
}

// Build a positioned overlay twin of `btn` for burning. The twin is an
// exact visual clone placed absolutely over the original, so the
// original can be hidden/removed without the layout collapsing mid-
// burn. Returns { twin, edge, cleanup }.
function buildTwin(btn) {
  const rect = btn.getBoundingClientRect();
  const twin = btn.cloneNode(true);
  twin.classList.add('fable-burn-twin');
  // Strip the id + data attrs so we don't double-wire listeners / ids.
  twin.removeAttribute('id');
  twin.removeAttribute('data-act');
  twin.style.position = 'fixed';
  twin.style.left = rect.left + 'px';
  twin.style.top = rect.top + 'px';
  twin.style.width = rect.width + 'px';
  twin.style.height = rect.height + 'px';
  twin.style.margin = '0';
  twin.style.pointerEvents = 'none';
  twin.style.zIndex = '5999';
  twin.style.filter = `url(#${FILTER_ID})`;
  // Hot edge: a thin glowing bar at the burn front. Real child element
  // (not a pseudo) so we can drive its `top` per-frame from JS.
  const edge = document.createElement('span');
  edge.className = 'fable-burn-edge';
  twin.appendChild(edge);
  document.body.appendChild(twin);
  const cleanup = () => { if (twin.parentNode) twin.parentNode.removeChild(twin); };
  return { twin, edge, cleanup };
}

// --- PUBLIC: burn rejected buttons bottom→top -------------------

// The CLICK sequence (Chloe's spec):
//   1. The CLICKED button (`selectedBtn`) POPS instantly (scale burst).
//   2. The OTHER buttons (`rejectedBtns`) BURN bottom→top concurrently.
//   3. When the burn finishes, the clicked button SLOWLY FADES OUT.
//   4. `onComplete` fires after the fade, then the Promise resolves.
//
// So the clicked button is NEVER burned — it pops, lingers while the
// others burn, then fades gracefully. `onComplete` is the screen-swap
// point (after the fade, when the stage is clear).
export function playBurnTransition({ rejectedBtns = [], selectedBtn = null, onComplete } = {}) {
  ensureFilterHost();
  return new Promise((resolve) => {
    // Phase 1: pop the clicked button instantly.
    if (selectedBtn) playButtonPop(selectedBtn);

    if (reducedMotion()) {
      rejectedBtns.forEach((b) => { b.style.transition = 'opacity 250ms ease'; b.style.opacity = '0'; });
      if (selectedBtn) {
        setTimeout(() => {
          selectedBtn.style.transition = 'opacity 400ms ease';
          selectedBtn.style.opacity = '0';
        }, 260);
      }
      setTimeout(() => {
        try { if (onComplete) onComplete(); } catch (e) { console.error('[burn] onComplete threw', e); }
        resolve();
      }, 700);
      return;
    }

    try { playIgnitionWhoosh(); } catch (_) { /* audio is a bonus */ }

    // Phase 2: burn the rejected buttons (the ones NOT clicked).
    const twins = rejectedBtns.map((b) => buildTwin(b));
    rejectedBtns.forEach((b) => { b.style.opacity = '0'; });

    const onBurnDone = () => {
      // Phase 3: the clicked button slowly fades out after the burn.
      if (selectedBtn) {
        selectedBtn.style.transition = `opacity ${SELECTED_FADE_MS}ms ease`;
        // Force a frame so the transition fires.
        void selectedBtn.offsetWidth;
        selectedBtn.style.opacity = '0';
        setTimeout(() => {
          try { if (onComplete) onComplete(); } catch (e) { console.error('[burn] onComplete threw', e); }
          resolve();
        }, SELECTED_FADE_MS);
      } else {
        try { if (onComplete) onComplete(); } catch (e) { console.error('[burn] onComplete threw', e); }
        resolve();
      }
    };

    if (twins.length === 0) {
      // Nothing to burn — skip straight to the selected fade.
      onBurnDone();
      return;
    }
    let remaining = twins.length;
    twins.forEach(({ twin, edge, cleanup }) => {
      driveBurn(twin, edge, 0, 100 + FEATHER * 50, BURN_MS, () => {
        cleanup();
        remaining--;
        if (remaining === 0) onBurnDone();
      });
    });
  });
}

// --- PUBLIC: reverse-spawn buttons into place --------------------

// The REVERSE of the burn — the exact inverse animation. A twin (the
// SAME turbulence-displaced overlay clone used for burning) sits over
// the real button. Its mask starts FULLY OPAQUE (the button is hidden
// behind it) and the burn line descends 100→0: the button materializes
// TOP→BOTTOM as the mask recedes, with the same glowing hot edge riding
// the front. The REAL button stays at opacity:0 the WHOLE time — the
// twin is the only visible surface, so there's no double-image + no
// "card spawns instantly behind the animation" bug. Only at the very
// end does the real button snap to opacity:1 and the twin get removed.
//
// This is the literal reverse of playBurnTransition's per-button drive:
// burn = line 0→100 (consume bottom→top); spawn = line 100→0 (reveal
// top→bottom). Same filter, same edge, opposite direction.
export function playReverseSpawn(btns = []) {
  ensureFilterHost();
  return new Promise((resolve) => {
    if (btns.length === 0) { resolve(); return; }

    if (reducedMotion()) {
      btns.forEach((b) => { b.style.transition = 'opacity 250ms ease'; b.style.opacity = '1'; });
      setTimeout(resolve, 260);
      return;
    }

    // Play the incinerator cue as the cards materialize too (Chloe
    // 2026-08-03: "play the incinerator sound when the cards spawn in as
    // well"). Same asset + volume as the burn.
    try { playIgnitionWhoosh(); } catch (_) { /* audio is a bonus */ }

    let remaining = btns.length;
    btns.forEach((btn) => {
      // The real button must be VISIBLE at clone time or cloneNode
      // copies opacity:0 into the twin (the teleport bug). Callers
      // often pre-hide buttons (opacity:0) before calling spawn, so
      // force the real button visible here, capture geometry + clone,
      // THEN hide it. This makes the engine robust to caller discipline.
      btn.style.opacity = '1';
      const rect = btn.getBoundingClientRect();
      const twin = btn.cloneNode(true);
      twin.classList.add('fable-burn-twin');
      twin.removeAttribute('id');
      twin.removeAttribute('data-act');
      twin.style.position = 'fixed';
      twin.style.left = rect.left + 'px';
      twin.style.top = rect.top + 'px';
      twin.style.width = rect.width + 'px';
      twin.style.height = rect.height + 'px';
      twin.style.margin = '0';
      twin.style.pointerEvents = 'none';
      twin.style.zIndex = '5999';
      twin.style.opacity = '1';
      twin.style.filter = `url(#${FILTER_ID})`;
      // Twin starts matching driveBurn's line=100 state (the spawn start
      // point): mostly consumed — only a sliver visible at the very top.
      // Setting this BEFORE append prevents a one-frame flash of the
      // fully-visible twin. (FEATHER*50 = 8, so featherStart=92 at line=100.)
      const initialMask = 'linear-gradient(to top, rgba(0,0,0,0) 0%, rgba(0,0,0,0) 92%, rgba(0,0,0,1) 100%)';
      twin.style.webkitMaskImage = initialMask;
      twin.style.maskImage = initialMask;
      const edge = document.createElement('span');
      edge.className = 'fable-burn-edge';
      edge.style.top = '0%';             // edge at the top (line=100 → 100-100=0)
      twin.appendChild(edge);
      document.body.appendChild(twin);
      // NOW hide the real button (the twin covers it). Order matters.
      btn.style.opacity = '0';

      // Drive the mask 100→0 (reveal top→bottom). This is the inverse
      // of burn's 0→100 drive — same driveBurn, swapped direction.
      driveBurn(twin, edge, 100, 0 - FEATHER * 50, SPAWN_MS, () => {
        // Hand off: real button ON, twin removed — no visual change.
        btn.style.opacity = '1';
        if (twin.parentNode) twin.parentNode.removeChild(twin);
        remaining--;
        if (remaining === 0) resolve();
      });
    });
  });
}

// --- PUBLIC: the selected-button pop -----------------------------

// Instant scale-down to 0.95 + a bright amber/white glow burst, held
// briefly, then scale back to 1 + shadow fade. Pure class toggle.
export function playButtonPop(btn) {
  if (!btn) return;
  btn.classList.add('is-popping');
  // Clean up the class after the burst so it doesn't linger + can re-fire.
  const done = () => {
    btn.classList.remove('is-popping');
    btn.removeEventListener('animationend', done);
  };
  btn.addEventListener('animationend', done, { once: true });
  // Fallback clear in case animationend doesn't fire (e.g. display:none).
  setTimeout(() => btn.classList.remove('is-popping'), 320);
}
