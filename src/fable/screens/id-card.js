// =============================================================
// ID CARD — the shared compact license-card renderer for Player,
// NPC, World, + Scenario cards (2026-08-13).
//
// The card has two faces:
//   • CORE  — a flat label/value grid of the few always-visible fields
//             (Player/NPC: Name, Gender, Race, Age, Hair Color, Eye Color,
//              Height, Weight; World/Scenario: a TYPE banner + Name, Setting,
//              Purpose, Tone). NO section headers — a real-life state-license
//             look. The portrait sits on the LEFT like a physical ID.
//   • EXTRA — every remaining tag, grouped under small section headers, hidden
//             behind a small bronze arrow button on the card's right edge.
//             Click the arrow to reveal/collapse (§6C/§6D).
//
// The data model comes from engine/creator-engine.js::buildIdCard (pure,
// unit-tested). This module owns ONLY the HTML string + the arrow toggle
// wiring — no Tauri, no business logic. It renders the card markup ONLY; each
// caller appends its own action-button row (CREATE/Edit/‹ or LOAD/EDIT/DELETE),
// so the same card serves the Creator review, the player-picker load modal,
// and (later) the installed-card modal.
// =============================================================

import { escapeXml } from './card-serialize.js';
import { SILHOUETTE_SVG, ARROW_SVG_RIGHT } from './wizard-engine.js';

// Render ONLY the ID-card markup (no action buttons).
//   model: { variant:'player'|'world', banner?, tag?, core:[[l,v]], extra:[[title,[[l,v]]]] }
//   opts:
//     portraitClickable: bool      — Creator review slot opens the cropper
//                                    (emits data-portrait-slot); the load modal
//                                    passes false (data-modal-portrait, static).
//     portraitPreview:    src|null — Creator review's data URL preview.
//     portraitHtml:       string   — prebuilt <img>/fallback HTML. The picker
//                                    builds this from convertFileSrc + its own
//                                    silhouette, so it wins when supplied.
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

  // Banner (world/scenario: prominent, centered) OR tag (NPC: subtle chip).
  // Mutually exclusive with each other; player cards have neither.
  const bannerHTML = m.banner
    ? `<div class="fable-id-card-banner">${esc(m.banner)}</div>` : '';
  const tagHTML = m.tag
    ? `<div class="fable-id-card-tag">${esc(m.tag)}</div>` : '';

  // Compact core grid — a flat <dl>, NO section headers.
  const coreRows = (m.core || []).map(([l, v]) =>
    `<div><dt>${esc(l)}</dt><dd>${esc(v)}</dd></div>`).join('');
  const coreHTML = coreRows ? `<dl class="fable-id-card-core">${coreRows}</dl>` : '';

  // Extra disclosure — the full section/h3/dl grid reused from the review card
  // (clean, easy to parse). Hidden until the bronze arrow is pressed.
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
    ? `<div class="fable-id-card-extra" data-id-extra hidden>${extraInner}</div>` : '';

  // The bronze arrow toggle (card's right edge). Only when there's extra to
  // reveal. Rotates 90° on expand (CSS).
  const expandHTML = hasExtra
    ? `<button type="button" class="fable-id-card-expand" data-id-expand aria-label="Show all details" aria-expanded="false" title="Show all details">${ARROW_SVG_RIGHT}</button>` : '';

  return `
    <div class="${cardClass}">
      <div class="fable-player-review-top">
        <div class="fable-player-review-portrait" ${slotAttr}>${portraitInner}</div>
        <div class="fable-player-review-body">
          ${bannerHTML}${tagHTML}
          ${coreHTML}
        </div>
      </div>
      ${extraHTML}
      ${expandHTML}
    </div>`;
}

// Wire the bronze-arrow expand/collapse toggle. Safe to call on any container
// that may hold an ID card — no-ops when there's no card / no arrow (codex,
// intro, or a card with nothing extra to reveal).
export function wireIdCard(root) {
  const card = root && root.querySelector('.fable-id-card');
  if (!card) return;
  const btn = card.querySelector('[data-id-expand]');
  const extra = card.querySelector('[data-id-extra]');
  if (!btn || !extra) return;
  btn.addEventListener('click', () => {
    const open = card.classList.toggle('is-expanded');
    extra.hidden = !open;
    btn.setAttribute('aria-expanded', open ? 'true' : 'false');
    if (open) extra.scrollTop = 0;
  });
}
