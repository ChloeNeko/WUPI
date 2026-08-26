// =============================================================
// PANEL: PARTY — read view over the tracked cast (npc entities).
// Both entity-key conventions render: flat `npc_<id>` keys and the
// dotted `npc.<id>.<field>` keys (one card per NPC, grouped).
// Each card's state chip is their current disposition/relationship
// (e.g. "wary", "trusted ally", "hostile"). Rendered as character
// cards with a glyph + state chip.
//
// (2026-08-24 Part II C1) Cards are CLICKABLE — `wirePartyCards`
// (called by the manager after mount) opens the NPC dossier popup
// over one fresh `fable_schema_get`.
// =============================================================

import { openNpcDossier } from '../engine/npc-dossier.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

export function renderParty(entities, schema) {
  // (2026-08-24 fix) The cast collects from BOTH entity-key conventions the
  // schema has used: flat `npc_<id>` keys (one entity per NPC, value = the
  // disposition string) and the live dotted `npc.<id>.<field>` per-field
  // keys (grouped + deduped to ONE card per NPC id; the tier field wins the
  // state label, else mood, else any field). The old `npc_`-only filter
  // rendered dot-convention sessions as an empty Cast panel.
  const cast = collectCast(entities);
  const head = `<div class="panel-head">
      <h2>The Cast</h2>
      <p class="panel-hint">Who's in this with you.</p>
    </div>`;
  if (!cast.length) {
    return head + `<div class="panel-empty">
      <p>No one notable tracked yet.</p>
      <p class="panel-empty-hint">Meet someone and ask Wupi to remember them.</p>
    </div>`;
  }
  const cards = cast.map(({ surface, state }) => npcCard(surface, state)).join('');
  return head + `<div class="party-grid">${cards}</div>`;
}

// Field-suffix preference for the dotted convention's state label (the
// first entry with a value wins; falls through to any field).
const DOT_FIELD_PRIORITY = ['tier', 'mood', 'status', 'state', 'note'];

/// Pure: entities map → [{ surface, state }] in stable key order. Exported
/// for tests.
export function collectCast(entities) {
  const ent = entities || {};
  const flat = [];
  const dotted = new Map(); // npcId → Map(fieldSuffix → value)
  for (const [key, value] of Object.entries(ent)) {
    if (key.startsWith('npc_')) {
      const surface = key.slice('npc_'.length);
      if (surface) flat.push({ surface, state: value });
    } else if (key.startsWith('npc.')) {
      const rest = key.slice('npc.'.length);
      const dot = rest.indexOf('.');
      if (dot <= 0) continue; // `npc.<id>` with no field — skip (no label)
      const npcId = rest.slice(0, dot);
      const field = rest.slice(dot + 1);
      if (!dotted.has(npcId)) dotted.set(npcId, new Map());
      dotted.get(npcId).set(field, value);
    }
  }
  const dottedCards = [...dotted.entries()].map(([npcId, fields]) => {
    let state = '';
    for (const f of DOT_FIELD_PRIORITY) {
      const v = fields.get(f);
      if (v !== undefined && v !== null && String(v).trim() !== '') {
        state = String(v);
        break;
      }
    }
    if (!state) {
      const first = [...fields.values()].find(
        (v) => v !== undefined && v !== null && String(v).trim() !== ''
      );
      state = first !== undefined ? String(first) : '';
    }
    return { surface: npcId, state };
  });
  return [...flat, ...dottedCards];
}

// Post-mount wiring: delegated click + Enter/Space on the cast cards open
// the dossier. Delegated (one host listener) so re-summoned panels need no
// re-wiring churn; wired ONCE per host element (repeat summons stack no
// listeners), and the delegated check only ever acts on [data-npc] cards.
export function wirePartyCards(hostEl) {
  if (!hostEl || hostEl.dataset.partyWired === '1') return;
  hostEl.dataset.partyWired = '1';
  const onClick = (e) => {
    const card = e.target.closest('[data-npc]');
    if (!card || !hostEl.contains(card)) return;
    void openNpcDossier(hostEl, card.dataset.npc || '');
  };
  const onKey = (e) => {
    if (e.key !== 'Enter' && e.key !== ' ') return;
    const card = e.target.closest && e.target.closest('[data-npc]');
    if (!card || !hostEl.contains(card)) return;
    e.preventDefault();
    void openNpcDossier(hostEl, card.dataset.npc || '');
  };
  hostEl.addEventListener('click', onClick);
  hostEl.addEventListener('keydown', onKey);
}

function npcCard(surface, state) {
  const name = prettify(surface);
  const rel = relationClass(state);
  // data-npc carries the STRIPPED surface (the registry id or name stem) —
  // the dossier's resolver accepts it raw or prefixed, but the bare form
  // exact-hits the registry id directly.
  return `<div class="party-card party-${rel}" data-npc="${esc(surface)}" tabindex="0" role="button" aria-label="Open dossier for ${esc(name)}">
    <div class="party-card-glyph">${glyphFor(name)}</div>
    <div class="party-card-body">
      <div class="party-card-name">${esc(name)}</div>
      <div class="party-card-state">${esc(state || 'an unknown figure')}</div>
    </div>
  </div>`;
}

function relationClass(state) {
  const s = (state || '').toLowerCase();
  if (/hostile|enemy|hates?|angry|furious/.test(s)) return 'hostile';
  if (/wary|suspicious|distrust|unease/.test(s)) return 'wary';
  if (/neutral|indifferent|stranger/.test(s)) return 'neutral';
  if (/friendly|ally|trusted|devoted|loyal/.test(s)) return 'ally';
  return 'neutral';
}

function prettify(id) {
  return id.replace(/^npc_/, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

function glyphFor(name) {
  // Crude role guess from name keywords — no asset dependency.
  const n = name.toLowerCase();
  if (/keeper|barkeep|innkeeper|bartender/.test(n)) return '🍺';
  if (/guard|soldier|warrior|knight/.test(n)) return '⚔';
  if (/mage|wizard|witch|sorcer/.test(n)) return '✦';
  if (/stranger|hooded|cloaked/.test(n)) return '🕵';
  if (/merchant|trader|shopkeep/.test(n)) return '⚖';
  if (/child|kid|young/.test(n)) return '✿';
  return '☻';
}
