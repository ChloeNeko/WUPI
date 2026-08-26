// =============================================================
// NPC DOSSIER (2026-08-24 Part II C1) — the cast card deep-dive.
// Clicking a cast card in panels/party.js opens a centered popup
// (the id-card openIdDetails pattern, mounted inside the panel host
// so z stays local) rendering everything the world-sim knows about
// ONE NPC over a single `fable_schema_get` fetch: identity, the
// bond, dispatched tasks, interior state, last seen. Pure render
// over existing state — zero Rust, zero sim risk.
// =============================================================

import { invoke } from '@tauri-apps/api/core';

// The shipped-default milestone registry mirrored from
// src-tauri/src/relationship.rs `MilestoneRegistry::defaults` (points only —
// the applicability ladders matter to transitions, not to a read-out). An
// event id absent here (a codex-authored custom milestone) renders without
// points rather than disappearing — the event itself is diegetic fact.
const MILESTONE_POINTS = {
  first_positive_interaction: 1,
  shared_drink: 1,
  shared_downtime: 2,
  helped_with_task: 2,
  defended_in_combat: 3,
  saved_life: 3,
  shared_secret: 3,
  long_loyalty: 3,
  sworn_oath: 3,
  risked_death_for: 3,
  betrayed_trust: 0,
  stole_from: 0,
  inspected_hostile: 0,
  killed_ally: 0,
  killed_family: 0,
  razed_home: 0,
};
const HOSTILITY_EVENTS = new Set([
  'betrayed_trust',
  'stole_from',
  'inspected_hostile',
  'killed_ally',
  'killed_family',
  'razed_home',
]);

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function prettifyKey(k) {
  return String(k || '')
    .replace(/^skill_/, '')
    .replace(/[_.-]+/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}

/// Resolve a surface form (a party entity key stem, an id, an alias, or a
/// name) to the registry entry — exact id, alias, name, then a unique
/// fragment. Pure; exported for tests.
///
/// (2026-08-24 fix) The party panel passes raw ENTITY KEYS, and the live
/// schema speaks two key conventions the registry never uses verbatim:
/// flat `npc_<id>` keys and the dotted `npc.<id>.<field>` per-field keys
/// (the backend's `npc.{id}.` prefix). Every candidate form (the raw
/// surface, the prefix-stripped stem, the dotted key's id segment) runs
/// the full exact→alias→name ladder before fragment matching — a bare
/// fragment check against `"npc_marcus"` could never hit an id of
/// `"marcus"` (the surface is LONGER than the id), which left the C1
/// dossier unreachable from every cast card.
export function resolveRegistryEntry(entries, surface) {
  const list = Array.isArray(entries) ? entries : [];
  const s = String(surface || '').trim().toLowerCase();
  if (!s) return null;
  const norm = (x) => String(x || '').trim().toLowerCase();
  // Candidate surface forms, most-specific first.
  const cands = [s];
  if (s.startsWith('npc_')) cands.push(s.slice('npc_'.length));
  if (s.startsWith('npc.')) {
    const rest = s.slice('npc.'.length);
    cands.push(rest);
    const dot = rest.indexOf('.');
    if (dot > 0) {
      cands.push(rest.slice(0, dot), rest.slice(dot + 1));
    }
  }
  for (const c of cands) {
    let hit = list.find((e) => norm(e.id) === c);
    if (hit) return hit;
    hit = list.find((e) => (e.aliases || []).some((a) => norm(a) === c));
    if (hit) return hit;
    hit = list.find((e) => norm(e.name) === c);
    if (hit) return hit;
  }
  for (const c of cands) {
    const frags = list.filter(
      (e) => norm(e.id).includes(c) || norm(e.name).includes(c)
    );
    if (frags.length === 1) return frags[0];
  }
  return null;
}

function fmtSeenAgo(minutes) {
  if (!minutes || minutes <= 0) return null;
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return `${days} day${days === 1 ? '' : 's'} ago`;
  if (hours > 0) return `${hours} hour${hours === 1 ? '' : 's'} ago`;
  return 'moments ago';
}

function fmtDuration(minutes) {
  if (minutes <= 0) return 'soon';
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return `${days} day${days === 1 ? '' : 's'}`;
  if (hours > 0) return `${hours} hour${hours === 1 ? '' : 's'}`;
  return 'a while';
}

/// The PURE dossier model: everything the popup renders, derived from ONE
/// schema snapshot + the clicked npc surface. Returns null when the npc
/// resolves to nothing. Exported for tests (the DOM layer only renders it).
export function buildNpcDossierModel(schema, surface) {
  if (!schema || typeof schema !== 'object') return null;
  const registry = (schema.npc_registry && schema.npc_registry.entries) || [];
  const entry = resolveRegistryEntry(registry, surface);
  if (!entry) return null;
  const now = Number((schema.world_clock && schema.world_clock.current_minutes) || 0);

  // The bond — relationships are keyed by registry id.
  const rel =
    (schema.relationships &&
      (schema.relationships[entry.id] ||
        (typeof schema.relationships.entries === 'object'
          ? schema.relationships.entries[entry.id]
          : null))) ||
    null;
  const events = (rel && Array.isArray(rel.events) ? rel.events : []).map((id) => ({
    id,
    points: MILESTONE_POINTS[id] ?? null,
    hostility: HOSTILITY_EVENTS.has(id),
  }));

  // Dispatched tasks (open + resolved, this npc only).
  const tasks = (Array.isArray(schema.offscreen_tasks) ? schema.offscreen_tasks : [])
    .filter((t) => resolveRegistryEntry(registry, t.npc_id || '') === entry)
    .map((t) => ({
      description: t.description || '',
      difficulty: t.difficulty || '',
      eta_minutes: t.resolves_at_minutes || 0,
      resolved: !!t.resolved,
      due: !t.resolved && now > 0 && now >= (t.resolves_at_minutes || 0),
    }));

  // Interior state (archived interiors render the stub, not cleared fields).
  const interior =
    (schema.npc_interior && (schema.npc_interior[entry.id] || null)) || null;
  const stackLine = (items) =>
    (Array.isArray(items) ? items : [])
      .map((it) => (it.qty && it.qty > 1 ? `${it.name} ×${it.qty}` : it.name))
      .filter(Boolean);

  // Presence — on-camera stance wins over "last seen".
  const presence = (Array.isArray(schema.presences) ? schema.presences : []).find(
    (p) => resolveRegistryEntry(registry, p.npc_id || '') === entry
  );

  const lastSeenMinutes =
    interior && interior.last_seen_minutes
      ? Math.max(0, now - Number(interior.last_seen_minutes || 0))
      : 0;

  return {
    id: entry.id,
    name: entry.name || prettifyKey(entry.id),
    role: entry.role || '',
    aliases: Array.isArray(entry.aliases) ? entry.aliases : [],
    prominence: entry.prominence || '',
    combatTier: entry.tier || '',
    bond: rel ? { tier: rel.tier || 'stranger', events } : null,
    tasks,
    interior: interior
      ? {
          archived: interior.archived || '',
          mood: interior.mood || '',
          intent: interior.intent || '',
          carries: stackLine(interior.items),
          wears: stackLine(interior.worn),
          interactions: Number(interior.interactions || 0),
        }
      : null,
    present: presence
      ? { stance: presence.stance || '', here: true }
      : null,
    lastSeen: presence ? 'on camera now' : fmtSeenAgo(lastSeenMinutes),
    _now: now,
  };
}

function modelToSections(m) {
  const parts = [];
  // Identity — single-column label/value rows.
  const idRows = [];
  if (m.role) idRows.push(['Role', m.role]);
  idRows.push(['Standing', m.prominence === 'core' ? 'Core cast' : 'Discovered']);
  if (m.combatTier) idRows.push(['Combat tier', m.combatTier]);
  if (m.aliases.length) idRows.push(['Also known as', m.aliases.join(', ')]);
  parts.push(
    section(
      'Identity',
      idRows.map(([l, v]) => `<div class="fable-npc-dossier-row"><dt>${esc(l)}</dt><dd>${esc(v)}</dd></div>`).join('')
    )
  );
  // The bond.
  if (m.bond) {
    const tierRow = `<div class="fable-npc-dossier-row"><dt>Bond</dt><dd>${esc(
      m.bond.tier
    )}</dd></div>`;
    const evRows = m.bond.events.length
      ? `<ul class="fable-npc-dossier-events">${m.bond.events
          .map(
            (e) =>
              `<li class="${e.hostility ? 'is-hostile' : ''}"><span>${esc(
                prettifyKey(e.id)
              )}</span><span class="fable-npc-dossier-pts">${
                e.points === null ? '—' : `+${e.points}`
              }</span></li>`
          )
          .join('')}</ul>`
      : '<div class="fable-npc-dossier-empty">No recorded milestones yet.</div>';
    parts.push(section('The bond', tierRow + evRows));
  }
  // Dispatched tasks.
  if (m.tasks.length) {
    parts.push(
      section(
        'Dispatched tasks',
        `<ul class="fable-npc-dossier-tasks">${m.tasks
          .map(
            (t) =>
              `<li><span>${esc(t.description)}</span><span class="fable-npc-dossier-task-meta">${
                t.resolved
                  ? 'resolved'
                  : t.due
                    ? 'due back'
                    : `due in ${fmtDuration(Math.max(0, t.eta_minutes - m._now))}`
              }</span></li>`
          )
          .join('')}</ul>`
      )
    );
  }
  // Interior.
  if (m.interior && (m.interior.archived || m.interior.mood || m.interior.intent || m.interior.carries.length || m.interior.wears.length)) {
    const rows = [];
    if (m.interior.archived) rows.push(['Archive', m.interior.archived]);
    if (m.interior.mood) rows.push(['Mood', m.interior.mood]);
    if (m.interior.intent) rows.push(['Intent', m.interior.intent]);
    if (m.interior.carries.length) rows.push(['Carries', m.interior.carries.join(', ')]);
    if (m.interior.wears.length) rows.push(['Wears', m.interior.wears.join(', ')]);
    parts.push(
      section(
        'Interior',
        rows
          .map(([l, v]) => `<div class="fable-npc-dossier-row"><dt>${esc(l)}</dt><dd>${esc(v)}</dd></div>`)
          .join('')
      )
    );
  }
  // Last seen.
  if (m.lastSeen) {
    parts.push(
      section(
        'Whereabouts',
        `<div class="fable-npc-dossier-row"><dt>${
          m.present ? 'Now' : 'Last seen'
        }</dt><dd>${esc(m.present ? m.present.stance || 'on camera' : m.lastSeen)}</dd></div>`
      )
    );
  }
  return parts.join('');
}

function section(label, inner) {
  return `<section class="fable-npc-dossier-section"><h3>${esc(
    label
  )}</h3>${inner}</section>`;
}

/// Open the centered dossier popup over the panel host (the openIdDetails
/// pattern: hidden → reflow → .is-open; ✕ / backdrop / Esc close with full
/// listener teardown). Fetches the schema ONCE.
/// (2026-08-24 review P1) `closeNpcDossier` is the ONE sanctioned closer:
/// a module-level ref so re-opening (and teardownStage) runs the PREVIOUS
/// overlay's full teardown — the old bare `el.remove()` sweep orphaned the
/// document-capture Esc handler, which later stole an Escape from whatever
/// modal was open next.
let activeDossierClose = null;

export function closeNpcDossier() {
  if (activeDossierClose) {
    const close = activeDossierClose;
    activeDossierClose = null;
    close();
  }
}

export async function openNpcDossier(hostEl, surface) {
  if (!hostEl) return;
  let schema = null;
  try {
    schema = await invoke('fable_schema_get');
  } catch (err) {
    console.warn('[npc-dossier] schema fetch failed:', err);
    // (2026-08-24 review P2) A silent dead click reads as a broken panel —
    // surface the failure where the click happened (transient inline
    // notice; no stage.js import — the engine/panel modules never import
    // the composition root).
    const mount = hostEl.closest('.fable-panel-overlay') || hostEl;
    const note = document.createElement('div');
    note.className = 'fable-npc-dossier-error';
    note.textContent = `Dossier unavailable: ${err}`;
    mount.appendChild(note);
    setTimeout(() => note.remove(), 4000);
    return;
  }
  const mount = hostEl.closest('.fable-panel-overlay') || hostEl;
  const model = buildNpcDossierModel(schema, surface);
  closeNpcDossier();
  mount.querySelectorAll('[data-npc-dossier]').forEach((el) => el.remove());
  if (!model) return;

  const overlay = document.createElement('div');
  overlay.className = 'fable-npc-dossier-overlay';
  overlay.dataset.npcDossier = '';
  overlay.hidden = true;
  overlay.innerHTML = `
    <div class="fable-npc-dossier-backdrop"></div>
    <div class="fable-npc-dossier-modal" role="dialog" aria-modal="true" aria-label="${esc(model.name)} dossier">
      <div class="fable-npc-dossier-head">
        <span class="fable-npc-dossier-title">${esc(model.name)}</span>
        <button type="button" class="fable-npc-dossier-close" data-npc-dossier-close aria-label="Close dossier">✕</button>
      </div>
      <div class="fable-npc-dossier-body">${modelToSections(model)}</div>
    </div>`;
  mount.appendChild(overlay);

  const onBackdrop = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-npc-dossier-backdrop')) {
      closeModal();
    }
  };
  const onEsc = (e) => {
    if (e.key === 'Escape') {
      e.stopPropagation();
      closeModal();
    }
  };
  const closeModal = () => {
    if (activeDossierClose === closeModal) activeDossierClose = null;
    overlay.removeEventListener('click', onBackdrop);
    document.removeEventListener('keydown', onEsc, { capture: true });
    overlay.classList.remove('is-open');
    const finish = () => overlay.remove();
    overlay.addEventListener('transitionend', finish, { once: true });
    setTimeout(finish, 260);
  };
  activeDossierClose = closeModal;
  overlay.addEventListener('click', onBackdrop);
  overlay.querySelector('[data-npc-dossier-close]').addEventListener('click', closeModal);
  document.addEventListener('keydown', onEsc, { capture: true });

  overlay.hidden = false;
  void overlay.offsetWidth;
  overlay.classList.add('is-open');
  overlay.querySelector('[data-npc-dossier-close]').focus();
}
