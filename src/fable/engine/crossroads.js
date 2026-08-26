// =============================================================
// CROSSROADS — the composer's options deck (2026-08-22).
//
// Behind the octagon icon at the composer's right edge. Two stages, both
// narrator-side API one-shots:
//   1. DRAW — pick one of five decks (Player / World / NPC / Plot /
//      Explicit); a centered rectangular panel opens on the wizard's
//      generation ring, then fills with 6 selectable bubbles (emoji +
//      1-3 word title over a 2-3 sentence summary).
//   2. EXPAND — picking a bubble raises the ring over the panel, then the
//      fully written-out fork drops into the composer (replacing whatever
//      it held) and the panel closes. The text is the player's to review
//      and send; nothing is ever auto-sent.
//
// Deck laws (Chloe's spec): Player options involve the player alone; World
// moves the world and its ESTABLISHED people (never new NPCs); NPC
// introduces NEW characters; Plot deals twists; Explicit deals fully
// uncensored adult interactions. The laws live in Rust (crossroads.rs) —
// this module only carries the menu labels.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { showComposerNotice } from './ghost-writer.js';

// The deck menu, display order. `id` is the wire key for the Rust side.
export const CROSSROADS_CATEGORIES = [
  { id: 'player', label: 'Player' },
  { id: 'world', label: 'World' },
  { id: 'npc', label: 'NPC' },
  { id: 'plot', label: 'Plot' },
  { id: 'explicit', label: 'Explicit' },
];

export function categoryLabel(id) {
  const cat = CROSSROADS_CATEGORIES.find((c) => c.id === id);
  return cat ? cat.label : '';
}

// Defensive client-side shaping of the options payload (the backend already
// sanitizes; this is the UI trust boundary). Codepoint-aware clamps.
export function sanitizeOptions(list) {
  if (!Array.isArray(list)) return [];
  const clamp = (v, cap) =>
    typeof v === 'string' ? [...v.trim()].slice(0, cap).join('') : '';
  return list
    .map((o) => ({
      emoji: clamp(o && o.emoji, 16),
      title: clamp(o && o.title, 60),
      summary: clamp(o && o.summary, 500),
    }))
    .filter((o) => o.emoji && o.title && o.summary)
    .slice(0, 6);
}

let deps = null;           // wiring context
let menuEl = null;         // the open category menu (null when closed)
let deckEl = null;         // the open deck overlay (null when closed)
let deckEpoch = 0;         // bumped on every open/close; late resolves die
let outsideCloser = null;  // transient document listener while menu is open
let deckEscCloser = null;  // transient document listener while deck is open

export function wireCrossroads(next) {
  deps = next;
  pendingPin = null;        // a fresh wiring starts with no staged DC pin
  pendingPinText = '';
  closeCrossroadsMenu();
  closeCrossroadsDeck();
}

export function teardownCrossroads() {
  deps = null;
  pendingPin = null;        // the stash dies with the stage (the backend
  pendingPinText = '';      // clears its slot at session boundaries too)
  closeCrossroadsMenu();
  closeCrossroadsDeck();
}

export function isCrossroadsMenuOpen() {
  return menuEl !== null;
}

export function isCrossroadsDeckOpen() {
  return deckEl !== null;
}

export function closeCrossroadsMenu() {
  if (outsideCloser) {
    document.removeEventListener('pointerdown', outsideCloser, true);
    outsideCloser = null;
  }
  if (menuEl) {
    menuEl.remove();
    menuEl = null;
  }
}

// Close the deck + invalidate every in-flight draw/expand (epoch bump): a
// resolve landing after the close must never touch the composer or render
// into a reopened deck.
export function closeCrossroadsDeck() {
  deckEpoch++;
  if (deckEscCloser) {
    document.removeEventListener('keydown', deckEscCloser, true);
    deckEscCloser = null;
  }
  if (deckEl) {
    deckEl.remove();
    deckEl = null;
  }
}

function openMenu() {
  if (!deps || menuEl) return;
  menuEl = document.createElement('div');
  menuEl.className = 'fable-aid-menu fable-crossroads-menu';
  menuEl.setAttribute('role', 'menu');
  for (const cat of CROSSROADS_CATEGORIES) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'fable-aid-menu-item';
    btn.textContent = cat.label;
    btn.dataset.crossroadsCategory = cat.id;
    btn.setAttribute('role', 'menuitem');
    menuEl.appendChild(btn);
  }
  menuEl.addEventListener('click', (e) => {
    const btn = e.target.closest('[data-crossroads-category]');
    if (!btn) return;
    e.stopPropagation();
    closeCrossroadsMenu();
    void openDeck(btn.dataset.crossroadsCategory);
  });
  deps.anchor.appendChild(menuEl);
  outsideCloser = (ev) => {
    if (menuEl && !menuEl.contains(ev.target) && !deps.icon.contains(ev.target)) {
      closeCrossroadsMenu();
    }
  };
  document.addEventListener('pointerdown', outsideCloser, true);
}

async function openDeck(categoryId) {
  if (!deps || deckEl) return;
  // A turn may have started while the category menu was open — the deck
  // refuses with the same notice the icons use.
  if (deps.isBusy()) {
    showComposerNotice('The narrator is busy right now.');
    return;
  }
  const myEpoch = ++deckEpoch;
  const label = categoryLabel(categoryId);

  deckEl = document.createElement('div');
  deckEl.className = 'fable-crossroads-overlay';
  deckEl.innerHTML = `
    <div class="fable-crossroads-backdrop"></div>
    <div class="fable-crossroads-panel" role="dialog" aria-label="Crossroads">
      <header class="fable-crossroads-head">
        <div class="fable-crossroads-title">Crossroads<span class="fable-crossroads-sub" data-deck-sub></span></div>
        <button class="fable-crossroads-close" type="button" aria-label="Close">✕</button>
      </header>
      <div class="fable-crossroads-body is-loading" data-deck-body></div>
    </div>`;
  const sub = deckEl.querySelector('[data-deck-sub]');
  if (sub && label) sub.textContent = `  ·  ${label}`;
  deckEl.querySelector('.fable-crossroads-close').addEventListener('click', () => closeCrossroadsDeck());
  deckEl.querySelector('.fable-crossroads-backdrop').addEventListener('click', () => closeCrossroadsDeck());
  deckEscCloser = (ev) => {
    if (ev.key === 'Escape') {
      ev.stopPropagation();
      closeCrossroadsDeck();
    }
  };
  document.addEventListener('keydown', deckEscCloser, true);
  showDeckLoading(deckEl, 'Drawing the deck...');
  deps.root.appendChild(deckEl);

  let options = [];
  try {
    const reply = await invoke('crossroads_options', { category: categoryId });
    if (myEpoch !== deckEpoch) return; // deck closed/reopened mid-draw
    options = sanitizeOptions(reply);
  } catch (err) {
    if (myEpoch !== deckEpoch) return;
    closeCrossroadsDeck();
    showComposerNotice(String(err));
    return;
  }
  if (myEpoch !== deckEpoch) return;
  if (!options.length) {
    closeCrossroadsDeck();
    showComposerNotice('The deck came back empty. Try again.');
    return;
  }
  renderDeckCards(deckEl, categoryId, options);
}

function showDeckLoading(el, labelText) {
  const body = el.querySelector('[data-deck-body]');
  if (!body) return;
  body.classList.add('is-loading');
  const stack = document.createElement('div');
  stack.className = 'fable-creator-genring-stack';
  const ring = document.createElement('div');
  ring.className = 'fable-creator-genring-ring';
  const label = document.createElement('div');
  label.className = 'fable-creator-genring-label';
  label.textContent = labelText;
  stack.append(ring, label);
  body.replaceChildren(stack);
}

function renderDeckCards(el, categoryId, options) {
  const body = el.querySelector('[data-deck-body]');
  if (!body) return;
  body.classList.remove('is-loading');
  const grid = document.createElement('div');
  grid.className = 'fable-crossroads-grid';
  for (const opt of options) {
    const card = document.createElement('button');
    card.type = 'button';
    card.className = 'fable-crossroads-card';
    const head = document.createElement('div');
    head.className = 'fable-crossroads-card-head';
    const emoji = document.createElement('span');
    emoji.className = 'fable-crossroads-card-emoji';
    emoji.textContent = opt.emoji;
    const title = document.createElement('span');
    title.className = 'fable-crossroads-card-title';
    title.textContent = opt.title;
    head.append(emoji, title);
    const summary = document.createElement('div');
    summary.className = 'fable-crossroads-card-summary';
    summary.textContent = opt.summary;
    card.append(head, summary);
    card.addEventListener('click', () => void expandChoice(categoryId, opt));
    grid.appendChild(card);
  }
  body.replaceChildren(grid);
}

// (2026-08-24 Part II B3) Parse "— [Skill DC N]" out of an option summary.
// PURE parse (exported for tests); the commit is staged by expandChoice and
// fired by consumePendingDcPin when the fork is actually SENT.
export function parseDeclaredDc(summary) {
  const m = /[—–-]\s*\[\s*([A-Za-z][A-Za-z '-]{1,40}?)\s+DC\s+(\d{1,2})\s*\]/.exec(
    String(summary || '')
  );
  if (!m) return null;
  const dc = Number(m[2]);
  if (!Number.isInteger(dc) || dc < 1 || dc > 30) return null;
  return { skill: m[1].trim(), dc };
}

// The staged one-shot DC pin (2026-08-24 timing fix). The OLD behavior
// committed the declared DC to the backend at OPTION CLICK — an abandoned
// expand (deck closed mid-write, a failed expand) left the pin armed, and
// the NEXT unrelated matching skill roll inherited the stale DC. Now the
// pin is only STASHED when a picked option's expand actually LANDS in the
// composer, and the COMMIT fires from the composer's submit funnel
// (consumePendingDcPin) at the moment the fork is truly sent — awaited so
// the pin is armed server-side before the turn's fable_send consumes it.
let pendingPin = null;       // { skill, dc } | null — stashed at expand landing
let pendingPinText = '';     // the expanded fork text the stash belongs to

// PURE (exported for tests): should the stashed DC pin ride THIS send? The
// sent text must still carry the expanded fork's opening line — a player
// who discarded the fork and typed their own action must not inherit its
// declared DC.
export function pinMatchesSentText(sentText, expandedText) {
  const sent = String(sentText || '').trim();
  if (!sent) return false;
  const firstLine = String(expandedText || '')
    .split('\n')
    .map((l) => l.trim())
    .find((l) => l);
  if (!firstLine) return false;
  // Case-insensitive: a lightly edited send (retyped casing) keeps lineage.
  return sent.toLowerCase().includes(firstLine.toLowerCase());
}

// Called by the composer's submit funnel with the text being sent. Commits
// the stashed one-shot pin (awaited — the backend must be armed before the
// same send's fable_send consumes it) only when the sent text still carries
// the fork; either way the stash is consumed. Best-effort: a rejected arm
// falls back to the computed DC.
export async function consumePendingDcPin(sentText) {
  const pin = pendingPin;
  const forkText = pendingPinText;
  pendingPin = null;
  pendingPinText = '';
  if (!pin) return;
  if (!pinMatchesSentText(sentText, forkText)) return;
  try {
    await invoke('crossroads_commit_dc', { skill: pin.skill, dc: pin.dc });
  } catch (err) {
    // The referee falls back to the computed DC — surface nothing; the
    // option text still reads exactly as offered.
    console.warn('[crossroads] DC commit rejected (computed DC applies):', err);
  }
}

async function expandChoice(categoryId, opt) {
  if (!deps || !deckEl) return;
  const myEpoch = deckEpoch;
  // (2026-08-24 Part II B3, timing fix) The declared DC is PARSED at offer
  // time but committed only when the fork is SENT: the stash below arms
  // AFTER the expanded text lands in the composer, and the composer's
  // submit funnel fires consumePendingDcPin. An abandoned expand (deck
  // closed mid-write / a failed call) never arms — a stale pin can never
  // poison the next unrelated skill roll.
  const pin = parseDeclaredDc(opt.summary);
  // The expand veil: the SAME wizard ring, now raised over the filled deck.
  const veil = document.createElement('div');
  veil.className = 'fable-creator-genring';
  const stack = document.createElement('div');
  stack.className = 'fable-creator-genring-stack';
  const ring = document.createElement('div');
  ring.className = 'fable-creator-genring-ring';
  const label = document.createElement('div');
  label.className = 'fable-creator-genring-label';
  label.textContent = 'Writing it out...';
  stack.append(ring, label);
  veil.append(stack);
  deckEl.querySelector('.fable-crossroads-panel').appendChild(veil);
  try {
    const text = await invoke('crossroads_expand', {
      category: categoryId,
      emoji: opt.emoji,
      title: opt.title,
      summary: opt.summary,
    });
    if (myEpoch !== deckEpoch) return; // deck closed mid-expand — never arm
    closeCrossroadsDeck();
    const input = deps.getInput();
    if (input && typeof text === 'string' && text.trim()) {
      input.value = text.trim();
      deps.onInputChanged();
      input.focus();
      // Arm the stash ONLY now that the fork actually landed (a later expand
      // that replaces the composer text re-arms with its own pin).
      if (pin) {
        pendingPin = pin;
        pendingPinText = text.trim();
      }
    }
  } catch (err) {
    if (myEpoch !== deckEpoch) return;
    veil.remove();
    showComposerNotice(String(err));
  }
}

// Stage wiring entry for the icon itself. `deps`:
//   icon / anchor  the octagon button + the menu anchor element
//   root           the stage root (the deck overlay mounts on it)
//   getInput / isBusy / onInputChanged  the shared composer surface
export function handleCrossroadsIconClick() {
  if (!deps) return;
  if (deps.isBusy()) {
    showComposerNotice('The narrator is busy right now.');
    return;
  }
  if (deckEl) return; // an open deck owns the icon until closed
  if (menuEl) closeCrossroadsMenu();
  else openMenu();
}
