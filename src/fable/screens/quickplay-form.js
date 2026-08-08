// =============================================================
// SCREEN: QUICK PLAY VOID FORM — the ominous Quick Play entry.
//
// Replaces the old quickplay-split choice screen (Chloe 2026-08-05: "remove
// the entire player creation thing [from Quick Play]... 3 sleek dark charcoal
// gray text boxes vertically stacked... a large black and white CREATE button
// ... make the vibe feel ominous"). Player creation now lives ONLY in New
// Game; Quick Play drops the player step entirely + seeds the run from three
// free-text descriptions instead.
//
// LAYOUT: a full-screen near-black void (`--fable-void-deep`) with slow dark
// void-particles drifting in the background (screens/void-particles.js), a
// centered column of three field blocks (DESCRIBE YOUR PLAYER / DESCRIBE THE
// SCENARIO / DESCRIBE WHAT YOU DESIRE) vertically stacked + equally spaced,
// and a large black-and-white CREATE button at the very bottom that emits
// subtle dark particles + a dark glow.
//
// FIELDS: each is a sleek dark charcoal-gray `<textarea>` FIXED at exactly 2
// lines tall. Text beyond 2 lines scrolls internally (scroll wheel only — the
// scrollbar is hidden across all browsers). No resize handle, no auto-grow.
//
// VALIDATION LOCK: CREATE stays disabled + dim until ALL THREE fields have
// non-empty trimmed text. On enable it turns white (border + text + glow) +
// becomes clickable.
//
// AMBIENCE: the void-particle host's lifecycle is driven by showScreen()'s
// `_startAmbient/_stopAmbient` convention (fresh on show, destroyed on hide).
// The QuickPlay.mp3 bed is started/stopped by fable.js at the transition
// midpoints (same hand-off as New Game) — this screen owns only the visual
// ambient, not the audio.
//
// DRIFT HANDOFF: onCreate fires the registered callback with the three
// values; fable.js's beginVoidDrift then calls fadeFormOut() (which fades
// everything EXCEPT the void + particles to opacity 0) and the void-particle
// system keeps running through the drift phase until the stage loads.
// =============================================================

import { createVoidParticles } from './void-particles.js';

export function buildQuickPlayForm(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-quickplay-form-screen';
  root.dataset.fableScreen = 'quickplay-form';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-particle-host" aria-hidden="true"></div>
    <div class="fable-qp-stack">
      <div class="fable-qp-field-block">
        <label class="fable-qp-label" for="fable-qp-player">Describe Your Player</label>
        <textarea
          id="fable-qp-player"
          class="fable-qp-field"
          data-field="player"
          rows="2"
          autocomplete="off"
          spellcheck="false"></textarea>
      </div>
      <div class="fable-qp-field-block">
        <label class="fable-qp-label" for="fable-qp-scenario">Describe Your Scene</label>
        <textarea
          id="fable-qp-scenario"
          class="fable-qp-field"
          data-field="scenario"
          rows="2"
          autocomplete="off"
          spellcheck="false"></textarea>
      </div>
      <div class="fable-qp-field-block">
        <label class="fable-qp-label" for="fable-qp-desire">Describe Your Desire</label>
        <textarea
          id="fable-qp-desire"
          class="fable-qp-field"
          data-field="desire"
          rows="2"
          autocomplete="off"
          spellcheck="false"></textarea>
      </div>
      <button class="fable-qp-create" type="button" data-act="create" disabled>
        <span class="fable-qp-create-label">CREATE</span>
      </button>
    </div>
  `;

  // ── Field setup: fixed 2-line height (CSS-driven), validate on input. ──
  // The fields are FIXED at exactly 2 lines tall via CSS (.fable-qp-field
  // height:88px + overflow-y:auto). Text beyond 2 lines scrolls internally
  // (scroll wheel only — the scrollbar is hidden across all browsers). This
  // keeps the three boxes uniform + the whole form fitting the viewport
  // without resizing text. No JS auto-grow anymore.
  //
  // KEEP-CURSOR-VISIBLE-ON-TYPE: a fixed-height textarea does NOT reliably
  // auto-scroll to follow the cursor while the user types past the visible
  // 2 lines — some browsers pin scrollTop at the top, so the active line gets
  // cut at the bottom mid-keystroke. We force the field to scroll the cursor
  // into view on every input/keyup/click. This is the "bottom gets cut as
  // you're actively typing" fix (Chloe 2026-08-05).
  //
  // Strategy: estimate the cursor's pixel offset within the scroll content by
  // counting how many DISPLAY lines precede the caret (wrapping-aware: each
  // physical line may wrap over multiple of display rows depending on width).
  // We approximate by computing the caret's line number × line-height, which
  // is exact for non-wrapping content and close enough for the 2-line box
  // (the common case is typing at the end → scroll to bottom).
  const fields = Array.from(root.querySelectorAll('.fable-qp-field'));
  function keepCursorVisible(el) {
    if (el.scrollHeight <= el.clientHeight) return;     // nothing overflowed
    // Most common case while actively typing: caret at/near the end → show the
    // latest line. Scroll so the bottom of the content is visible.
    if (el.selectionStart >= el.value.length - 1) {
      el.scrollTop = el.scrollHeight - el.clientHeight;
      return;
    }
    // Caret elsewhere: estimate its line offset and scroll it into view.
    const cs = getComputedStyle(el);
    const lineH = parseFloat(cs.lineHeight) || 30;
    const textBeforeCaret = el.value.slice(0, el.selectionStart);
    const lineIdx = textBeforeCaret.split('\n').length - 1;
    const caretTop = lineIdx * lineH;
    // If the caret is below the visible window, scroll down to it; if above, up.
    if (caretTop < el.scrollTop) el.scrollTop = caretTop;
    else if (caretTop + lineH > el.scrollTop + el.clientHeight) {
      el.scrollTop = caretTop + lineH - el.clientHeight;
    }
  }
  fields.forEach((el) => {
    el.addEventListener('input', () => {
      revalidate();
      keepCursorVisible(el);
    });
    el.addEventListener('keyup', () => keepCursorVisible(el));
    el.addEventListener('click', () => keepCursorVisible(el));
  });

  // ── Validation lock: CREATE enabled only when all three are non-empty. ──
  const createBtn = root.querySelector('.fable-qp-create');
  function revalidate() {
    const allFilled = fields.every((el) => el.value.trim().length > 0);
    // The .is-ready class drives the white/glow treatment in CSS; the
    // disabled attr is the authoritative gate (pointer-events + a11y).
    createBtn.disabled = !allFilled;
    createBtn.classList.toggle('is-ready', allFilled);
  }
  revalidate();

  // ── onCreate: hand the three values to the flow controller. ──
  createBtn.addEventListener('click', () => {
    if (createBtn.disabled) return;
    if (handlers && typeof handlers.onCreate === 'function') {
      handlers.onCreate(getValues());
    }
  });

  // Public: read the three trimmed values.
  function getValues() {
    const byField = {};
    for (const el of fields) byField[el.dataset.field] = el.value.trim();
    return {
      player: byField.player || '',
      scenario: byField.scenario || '',
      desire: byField.desire || '',
    };
  }

  // ── fadeFormOut: the CREATE → void-drift handoff. ──
  // Fades everything EXCEPT the void + particles (the `.fable-qp-stack`) to
  // opacity 0, leaving the user adrift in the pure void with the particles
  // still drifting — matches "the Player drifts in the void while the AI
  // parses". Resolves when the fade transition ends (or after a safety
  // timeout). The void-particle system keeps running (the screen stays
  // shown + ambient stays started) until fable.js swaps to the stage.
  function fadeFormOut() {
    return new Promise((resolve) => {
      const stack = root.querySelector('.fable-qp-stack');
      if (!stack) { resolve(); return; }
      let done = false;
      const finish = () => {
        if (done) return;
        done = true;
        stack.removeEventListener('transitionend', onEnd);
        resolve();
      };
      const onEnd = (e) => {
        // Only resolve on the opacity transition (the stack only fades
        // opacity — guard against any child transition bubbling up).
        if (e.target === stack && e.propertyName === 'opacity') finish();
      };
      stack.addEventListener('transitionend', onEnd);
      stack.classList.add('is-fading');
      // Safety timeout: never let the drift hang if transitionend doesn't
      // fire (e.g. prefers-reduced-motion strips transitions). Slightly past
      // the CSS fade duration.
      setTimeout(finish, 900);
    });
  }

  // Reset the form to its initial state (clears fields, re-validates,
  // removes the fading class). Called by fable.js when returning to the
  // form from the flow chrome so a re-entry doesn't show stale text.
  function reset() {
    fields.forEach((el) => {
      el.value = '';
    });
    const stack = root.querySelector('.fable-qp-stack');
    if (stack) stack.classList.remove('is-fading');
    revalidate();
  }

  // Expose the public API for the flow controller (fable.js).
  root._getValues = getValues;
  root._fadeFormOut = fadeFormOut;
  root._reset = reset;

  // ── Ambient void-particle lifecycle ── (mirrors newgame-split's embers).
  // Fresh on show, destroyed on hide so no RAF/listener leaks. The drift
  // phase keeps this running by leaving the screen shown until the stage
  // swap; once the stage shows, showScreen hides this screen → _stopAmbient
  // fires → clean teardown.
  const particleHost = root.querySelector('.fable-void-particle-host');
  let particles = null;
  // The form reveal is JS-driven (not a :not([hidden]) CSS selector): while
  // the screen is [hidden] it's display:none, so a CSS opacity transition
  // would never fire from that state. showScreen calls _startAmbient the
  // instant the screen becomes visible, so we force a reflow here (so the
  // initial opacity:0 paints) then add .is-shown to run the fade-in.
  const stack = root.querySelector('.fable-qp-stack');
  root._startAmbient = () => {
    if (!particles) particles = createVoidParticles(particleHost);
    if (stack) {
      stack.classList.remove('is-shown');   // reset for re-entry
      void stack.offsetWidth;               // force reflow → paint opacity:0
      stack.classList.add('is-shown');      // → transition to opacity:1
    }
  };
  root._stopAmbient = () => {
    if (particles) { particles.destroy(); particles = null; }
    if (stack) stack.classList.remove('is-shown');
  };

  return root;
}
