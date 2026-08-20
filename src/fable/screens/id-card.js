// =============================================================
// ID CARD — the shared compact license-card renderer for Player,
// NPC, World, + Scenario cards (2026-08-13).
//
// The card has two faces:
//   • CORE  — a NAME/TYPE header (centered over the information space, gold
//             rule fitted to the text width) + a label/value license grid
//             (Player/NPC: Race/Gender, Age/Skin/Body, Height/Weight, the
//             stacked Hair cell + Eye Color; World/Scenario: Name, Setting,
//             Purpose, Tone). NO section headers — a real-life state-license
//             look. The portrait sits on the LEFT like a physical ID.
//   • EXTRA — every remaining tag, grouped under small section headers, held
//             in an inert <template> on the card (never rendered inline). The
//             small brass CARD-ICON button in the headrow's corner cluster
//             opens the full set in a CENTERED details popup (2026-08-14,
//             Chloe: replaces the old bronze-arrow inline disclosure — the
//             card itself never expands anymore).
//
// The data model comes from engine/creator-engine.js::buildIdCard (pure,
// unit-tested). This module owns ONLY the HTML string + the popup wiring —
// no Tauri, no business logic. It renders the card markup ONLY; each
// caller appends its own action-button row (CREATE/Edit/‹ or LOAD/EDIT/DELETE),
// so the same card serves the Creator review, the player-picker load modal,
// and (later) the installed-card modal.
// =============================================================

import { escapeXml } from './card-serialize.js';
import { SILHOUETTE_SVG, CARD_SVG } from './wizard-engine.js';

// The corner-pencil icon (the review card's edit affordance, 2026-08-15
// Chloe — replaces the old "Edit" button in the CREATE row). Stroke-based so
// it reads cleanly at 22px beside the filled CARD_SVG.
export const PENCIL_SVG = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">
  <path d="M12 20h9"/>
  <path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z"/>
</svg>`;

// Render ONLY the ID-card markup (no action buttons).
//   model: { variant:'player'|'world', title?, banner?, tag?, core:[cell],
//            extra:[[title, [[l,v]]]] }
//     core cell: { label, value } | { label, value, third:true } |
//                { label, sub:[[sublabel, value], ...] }
//   opts:
//     portraitClickable: bool      — Creator review slot opens the cropper
//                                    (emits data-portrait-slot); the load modal
//                                    passes false (data-modal-portrait, static).
//     portraitPreview:    src|null — Creator review's data URL preview.
//     portraitHtml:       string   — prebuilt <img>/fallback HTML. The picker
//                                    builds this from convertFileSrc + its own
//                                    silhouette, so it wins when supplied.
//     editable:           bool     — Creator review only: renders the corner
//                                    PENCIL beside the card-icon details
//                                    button (data-review-pencil → the creator's
//                                    edit popup). The pickers never pass it.
//
// LAYOUT (2026-08-20 Chloe): the header (player/NPC NAME, world/scenario TYPE
// banner) lives in a HEADROW centered over the card's INFORMATION space (right
// of the portrait); the rule is fitted to the header text width plus a little
// breathing room on each side, not the card width. The details/pencil corner
// cluster is absolutely pinned to the CARD's top-right corner (the headrow
// carries symmetric padding so a long header still cannot run under it).
export function renderIdCard(model, opts = {}) {
  const esc = escapeXml;
  const m = model || {};
  const isWorld = m.variant === 'world';
  const cardClass = isWorld
    ? 'fable-player-review-card fable-id-card fable-id-card--world'
    : 'fable-player-review-card fable-id-card';

  // Portrait slot (LEFT — like a physical ID).
  const portraitInner = opts.portraitHtml != null
    ? opts.portraitHtml
    : (opts.portraitPreview
        ? `<img src="${esc(opts.portraitPreview)}" alt="" onerror="this.style.display='none'">`
        : `<span class="fable-player-review-portrait-fallback" aria-hidden="true">${SILHOUETTE_SVG}</span>`);
  const slotAttr = opts.portraitClickable
    ? 'data-portrait-slot'
    : 'data-modal-portrait';

  // The header: the NAME (every card — 2026-08-20 Chloe). The type subheader
  // ('NPC CARD' etc.) is NOT part of the header block — it renders as its own
  // full-width row under the headrow, centered on the info body's midline
  // (directly between RACE and GENDER).
  const headText = m.title;
  const tagHTML = m.tag ? `<div class="fable-id-card-tag">${esc(m.tag)}</div>` : '';
  const headHTML = headText
    ? `<div class="fable-id-card-headwrap">
          <span class="fable-id-card-head-text">${esc(headText)}</span>
          <span class="fable-id-card-head-rule" aria-hidden="true"></span>
        </div>`
    : '';
  // Core cells — plain label/value pairs (2026-08-20 Chloe: the six license
  // rows all sit in ONE horizontal row; no halves/thirds pairing).
  const cellHTML = (c) => {
    if (!c) return '';
    return `<div class="fable-id-card-cell"><dt>${esc(c.label)}</dt><dd>${esc(c.value)}</dd></div>`;
  };
  const coreHTML = (m.core || []).length
    ? `<dl class="fable-id-card-core">${(m.core || []).map((c) => cellHTML(c)).join('')}</dl>`
    : '';

  // Extra disclosure content — the full section/h3/dl grid reused from the
  // review card (clean, easy to parse), held in an inert <template>. The
  // centered details popup consumes it on demand.
  const extraInner = (m.extra || []).map(([title, rows]) => {
    // A row with an EMPTY label (the Intro paragraph, 2026-08-20 Chloe) skips
    // the dt entirely — a bare value line.
    const pairHtml = rows.map(([l, v]) => {
      const dt = l ? `<dt>${esc(l)}</dt>` : '';
      if (Array.isArray(v)) {
        const chips = v.map((c) => `<span class="fable-wizard-chip">${esc(c)}</span>`).join('');
        return `<div>${dt}<dd><div class="fable-player-review-chips">${chips}</div></dd></div>`;
      }
      return `<div>${dt}<dd>${esc(v)}</dd></div>`;
    }).join('');
    return `<section class="fable-player-review-section"><h3>${esc(title)}</h3><dl>${pairHtml}</dl></section>`;
  }).join('');
  const hasExtra = !!(m.extra && m.extra.length);
  const extraHTML = hasExtra
    ? `<template data-id-extra>${extraInner}</template>` : '';

  // The corner cluster is absolutely pinned to the CARD's top-right corner
  // (2026-08-20 Chloe — the in-flow headrow seat left dead space above it):
  // the brass card-icon trigger opens the details popup (wireIdCard), the
  // pencil (Creator review only) sits to its right.
  const expandHTML = hasExtra
    ? `<button type="button" class="fable-id-card-expand" data-id-expand aria-label="Show all details" aria-haspopup="dialog" aria-expanded="false">${CARD_SVG}</button>` : '';
  const editHTML = opts.editable
    ? `<button type="button" class="fable-id-card-edit" data-review-pencil aria-label="Edit card">${PENCIL_SVG}</button>` : '';
  const cornerHTML = (expandHTML || editHTML)
    ? `<div class="fable-id-card-corner">${expandHTML}${editHTML}</div>` : '';
  const headrowHTML = headText
    ? `<div class="fable-id-card-headrow">${headHTML}</div>`
    : '';

  return `
    <div class="${cardClass}">
      ${cornerHTML}
      <div class="fable-player-review-top">
        <div class="fable-player-review-portrait" ${slotAttr}>${portraitInner}</div>
        <div class="fable-player-review-body">
          ${headrowHTML}
          ${tagHTML}
          ${coreHTML}
        </div>
      </div>
      ${extraHTML}
    </div>`;
}

// Wire the card-icon details popup. Safe to call on any container that may
// hold an ID card — no-ops when there's no card / no extra (codex, intro, or
// a card with nothing extra to reveal).
export function wireIdCard(root) {
  const card = root && root.querySelector('.fable-id-card');
  if (!card) return;
  balanceHeadText(card.querySelector('.fable-id-card-head-text'));
  const btn = card.querySelector('[data-id-expand]');
  const tpl = card.querySelector('template[data-id-extra]');
  if (!btn || !tpl) return;
  btn.addEventListener('click', () => openIdDetails(card, btn, tpl.innerHTML));
}

// Balance a wrapped header so the FIRST line is always shorter than the
// second (2026-08-20 Chloe — natural wrapping left "THE NIGHT MARKET" over
// an orphan "HEIST"). REACTIVE: a ResizeObserver re-balances whenever the
// element's width changes (window resizes re-wrap naturally otherwise); the
// original text is kept in a data attribute and the element is re-evaluated
// from scratch each pass (idempotent). Measures with a canvas in the
// element's computed font (+ its letter-spacing, which canvas ignores).
function balanceHeadText(el) {
  if (!el) return;
  const original = el.dataset.headText || (el.dataset.headText = (el.textContent || '').trim());
  const words = original.split(/\s+/);
  if (!el.dataset.headObserved) {
    el.dataset.headObserved = '1';
    // Observe the HEADROW (not the element): a wrapped head-text's own width
    // doesn't change when the container widens, so its observer would never
    // fire to unwrap it. The row tracks the container.
    const row = el.closest('.fable-id-card-headrow');
    new ResizeObserver(() => balanceHeadText(el)).observe(row || el);
  }
  if (words.length < 2) return;
  const cs = getComputedStyle(el);
  const canvas = balanceHeadText._canvas || (balanceHeadText._canvas = document.createElement('canvas'));
  const ctx = canvas.getContext('2d');
  ctx.font = `${cs.fontStyle} ${cs.fontWeight} ${cs.fontSize} ${cs.fontFamily}`;
  const spacing = parseFloat(cs.letterSpacing) || 0;
  // Measure the UPPERCASED string — the header renders text-transform:
  // uppercase, and canvas has no text-transform (uppercase glyphs run ~10%
  // wider; measuring as-written made the fit-check pass while the real line
  // wrapped — the "THE NIGHT MARKET / HEIST" bug).
  const w = (s) => {
    const t = s.toUpperCase();
    return ctx.measureText(t).width + t.length * spacing;
  };
  const fullW = w(original);
  // Measure available width against the HEADROW's content box — the head-text
  // shrink-wraps once wrapped, so measuring the element itself would poison
  // the check (it could never unwrap again).
  const row = el.closest('.fable-id-card-headrow');
  const rowCS = row ? getComputedStyle(row) : null;
  const avail = rowCS
    ? row.clientWidth - parseFloat(rowCS.paddingLeft) - parseFloat(rowCS.paddingRight)
    : el.getBoundingClientRect().width;
  if (fullW <= avail + 1) {                   // fits one line — no forced wrap
    if (el.innerHTML !== escapeXml(original)) el.innerHTML = escapeXml(original);
    return;
  }
  // Longest prefix under 45% of the single-line width → line1 is always the
  // shorter line (and never wider than the box).
  let count = 0;
  for (let i = 1; i <= words.length; i++) {
    const prefix = words.slice(0, i).join(' ');
    if (w(prefix) > fullW * 0.45 || w(prefix) > avail) break;
    count = i;
  }
  const target = (count >= 1 && count < words.length)
    ? `${escapeXml(words.slice(0, count).join(' '))}<br>${escapeXml(words.slice(count).join(' '))}`
    : escapeXml(original);
  if (el.innerHTML !== target) el.innerHTML = target;
}

// Open the centered details popup — a standard Fable modal (dimmed blur
// backdrop + glass panel, fade/scale in; the same aesthetic as the
// player-picker modal). Mounted on the card's .fable-screen so it layers
// above whichever surface the card lives on (Creator review or the picker
// modal). Esc / backdrop click / ✕ all close + restore focus to the trigger.
function openIdDetails(card, btn, sectionsHtml) {
  const mount = card.closest('.fable-screen') || document.body;
  // One popup at a time — a leftover can only exist if a close animation was
  // interrupted, so a plain replace is enough.
  mount.querySelectorAll('[data-id-details]').forEach((el) => el.remove());

  // Popup title: the card's Name (first core value); generic fallback when
  // the card has no fields.
  const nameEl = card.querySelector('.fable-id-card-core dd');
  const title = (nameEl && nameEl.textContent || '').trim() || 'Card Details';

  const overlay = document.createElement('div');
  overlay.className = 'fable-id-details-overlay';
  overlay.dataset.idDetails = '';
  overlay.hidden = true;
  overlay.innerHTML = `
    <div class="fable-id-details-backdrop"></div>
    <div class="fable-id-details-modal" role="dialog" aria-modal="true" aria-label="Full card details">
      <div class="fable-id-details-head">
        <span class="fable-id-details-title">${escapeXml(title)}</span>
        <button type="button" class="fable-id-details-close" data-id-details-close aria-label="Close details">✕</button>
      </div>
      <div class="fable-id-details-body">${sectionsHtml}</div>
    </div>`;
  mount.appendChild(overlay);

  const onBackdrop = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-id-details-backdrop')) closeModal();
  };
  // Esc on the document (capture so it wins over the host surface's own Esc
  // chain while the popup is up — the picker modal underneath must not close).
  const onEsc = (e) => {
    if (e.key === 'Escape') { e.stopPropagation(); closeModal(); }
  };
  const closeModal = () => {
    overlay.removeEventListener('click', onBackdrop);
    document.removeEventListener('keydown', onEsc, { capture: true });
    overlay.classList.remove('is-open');
    const finish = () => overlay.remove();
    overlay.addEventListener('transitionend', finish, { once: true });
    setTimeout(finish, 260);   // fallback if no transition fires
    btn.setAttribute('aria-expanded', 'false');
    btn.focus();
  };

  overlay.addEventListener('click', onBackdrop);
  overlay.querySelector('[data-id-details-close]').addEventListener('click', closeModal);
  document.addEventListener('keydown', onEsc, { capture: true });

  // Fade/scale in (the picker-modal pattern: unhide → reflow → .is-open).
  overlay.hidden = false;
  void overlay.offsetWidth;
  overlay.classList.add('is-open');
  btn.setAttribute('aria-expanded', 'true');
  overlay.querySelector('[data-id-details-close]').focus();
}
