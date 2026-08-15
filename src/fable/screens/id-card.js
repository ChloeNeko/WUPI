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
// LAYOUT (2026-08-15 Chloe): the header (player/NPC NAME, world/scenario TYPE
// banner) lives in a HEADROW — [invisible spacer | centered header + gold rule
// | corner buttons] — so the header is centered over the card's INFORMATION
// space (right of the portrait) and the details/pencil buttons physically
// cannot push or overlap it. The rule is fitted to the header text width plus
// a little breathing room on each side, not the card width.
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
    ? 'data-portrait-slot title="Click to choose or crop a portrait"'
    : 'data-modal-portrait';

  // The header: the NAME (player/NPC) or the TYPE banner (world/scenario).
  const headText = m.title || m.banner;
  const headHTML = headText
    ? `<div class="fable-id-card-headwrap">
          <span class="fable-id-card-head-text">${esc(headText)}</span>
          <span class="fable-id-card-head-rule" aria-hidden="true"></span>
        </div>`
    : '';
  // NPC's subtle corner chip sits ABOVE the headrow (world cards have neither).
  const tagHTML = m.tag
    ? `<div class="fable-id-card-tag">${esc(m.tag)}</div>` : '';

  // Core cells — plain pairs, narrow thirds (the AGE/SKIN/BODY row), and the
  // stacked HAIR cell (Color/Length/Style sub-lines, smaller + differently
  // colored via .fable-id-card-hair-line). The renderer simulates the CSS
  // grid's row flow (halves span 3 of 6 tracks, thirds span 2) to tag each
  // HALF cell with its column side — the --half-l/--half-r padding pulls the
  // left column's content right + the right column's left so every row reads
  // as a tight divided cluster, matching the thirds row (2026-08-15 Chloe).
  const cellSideClass = (cells) => {
    let col = 0;
    const cls = cells.map((c) => {
      const span = c && c.third ? 2 : 3;
      if (col + span > 6) col = 0;
      let side = '';
      if (!c || !c.third) side = col === 0 ? ' fable-id-card-cell--half-l' : ' fable-id-card-cell--half-r';
      col += span;
      if (col >= 6) col = 0;
      return side;
    });
    // The HAIR pair breaks the inward pull (2026-08-15 Chloe): HAIR centers
    // under AGE and EYE COLOR under BODY — the outer thirds' centers —
    // widening the last row out of the 31/69 column rhythm. Only when the
    // age/skin/body trio exists (its thirds define the 15/85 targets); on a
    // trio-less card the pair keeps the normal inward halves.
    const hasTrio = cells.some((c) => c && c.third);
    if (hasTrio) {
      const hairIdx = cells.findIndex((c) => c && Array.isArray(c.sub));
      if (hairIdx !== -1) {
        cls[hairIdx] = ' fable-id-card-cell--half-out-l';
        if (hairIdx + 1 < cls.length) cls[hairIdx + 1] = ' fable-id-card-cell--half-out-r';
      }
    }
    return cls;
  };
  const cellHTML = (c, sideCls) => {
    if (!c) return '';
    if (Array.isArray(c.sub)) {
      const lines = c.sub
        .map(([l, v]) => `<span class="fable-id-card-hair-line"><i>${esc(l)}:</i>${esc(v)}</span>`)
        .join('');
      return `<div class="fable-id-card-cell fable-id-card-cell--hair${sideCls}"><dt>${esc(c.label)}</dt><dd>${lines}</dd></div>`;
    }
    const third = c.third ? ' fable-id-card-cell--third' : '';
    return `<div class="fable-id-card-cell${third}${sideCls}"><dt>${esc(c.label)}</dt><dd>${esc(c.value)}</dd></div>`;
  };
  const sides = cellSideClass(m.core || []);
  const coreHTML = (m.core || []).length
    ? `<dl class="fable-id-card-core">${(m.core || []).map((c, i) => cellHTML(c, sides[i])).join('')}</dl>`
    : '';

  // Extra disclosure content — the full section/h3/dl grid reused from the
  // review card (clean, easy to parse), held in an inert <template>. The
  // centered details popup consumes it on demand.
  const extraInner = (m.extra || []).map(([title, rows]) => {
    const pairHtml = rows.map(([l, v]) => {
      if (Array.isArray(v)) {
        const chips = v.map((c) => `<span class="fable-wizard-chip">${esc(c)}</span>`).join('');
        return `<div><dt>${esc(l)}</dt><dd><div class="fable-player-review-chips">${chips}</div></dd></div>`;
      }
      return `<div><dt>${esc(l)}</dt><dd>${esc(v)}</dd></div>`;
    }).join('');
    return `<section class="fable-player-review-section"><h3>${esc(title)}</h3><dl>${pairHtml}</dl></section>`;
  }).join('');
  const hasExtra = !!(m.extra && m.extra.length);
  const extraHTML = hasExtra
    ? `<template data-id-extra>${extraInner}</template>` : '';

  // The corner cluster rides INSIDE the headrow (in-flow, right-aligned) so it
  // can never overlap the centered header — the brass card-icon trigger opens
  // the details popup (wireIdCard), the pencil (Creator review only) sits to
  // its right.
  const expandHTML = hasExtra
    ? `<button type="button" class="fable-id-card-expand" data-id-expand aria-label="Show all details" aria-haspopup="dialog" aria-expanded="false" title="Show all details">${CARD_SVG}</button>` : '';
  const editHTML = opts.editable
    ? `<button type="button" class="fable-id-card-edit" data-review-pencil title="Edit" aria-label="Edit card">${PENCIL_SVG}</button>` : '';
  const cornerHTML = (expandHTML || editHTML)
    ? `<div class="fable-id-card-corner">${expandHTML}${editHTML}</div>` : '';
  // The spacer mirrors the corner cluster's EXACT footprint (46px per button
  // + 10px gap) so the header between them is truly centered — the pickers
  // show one button, the Creator review two.
  const btnW = (expandHTML ? 46 : 0) + (editHTML ? 46 : 0) + ((expandHTML && editHTML) ? 10 : 0);
  const spacerHTML = cornerHTML
    ? `<span class="fable-id-card-head-spacer" style="width:${btnW}px" aria-hidden="true"></span>` : '';
  const headrowHTML = (headText || cornerHTML)
    ? `<div class="fable-id-card-headrow">${spacerHTML}${headHTML}${cornerHTML}</div>`
    : '';

  return `
    <div class="${cardClass}">
      <div class="fable-player-review-top">
        <div class="fable-player-review-portrait" ${slotAttr}>${portraitInner}</div>
        <div class="fable-player-review-body">
          ${tagHTML}
          ${headrowHTML}
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
  const btn = card.querySelector('[data-id-expand]');
  const tpl = card.querySelector('template[data-id-extra]');
  if (!btn || !tpl) return;
  btn.addEventListener('click', () => openIdDetails(card, btn, tpl.innerHTML));
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
        <button type="button" class="fable-id-details-close" data-id-details-close title="Close" aria-label="Close details">✕</button>
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
