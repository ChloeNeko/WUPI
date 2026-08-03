// =============================================================
// SCREEN: CREATOR — author a new .sim card.
//
// STRUCTURE (Chloe 2026-08-02): THREE HORIZONTAL giant-square
// burn-buttons on top — WORLD, CHARACTER, SCENARIO. Click one → the
// other two burn away → the survivor expands into its form fields.
// A collapse chip reverse-spawns the three. The burn/whoosh engine is
// the same one the menu pairs use (engine/burn-transition.js).
//
// The chrome (‹ / ⌂) is owned by the flow controller; no header bar.
//
// FIELD GROUPING:
//   World:     setting, plot, tone
//   Character: cast (the <cast> block — named NPCs for the world)
//   Scenario:  playerName, openingScene
//   (name + persona + traits + appearance live on the PLAYER, authored
//   in the Player Creator step that precedes this screen.)
//
// SERIALIZATION: flat-format <sim_card> XML with CDATA-wrapped prose.
// fable_write_card validates via the real parser.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
import { createEmbers } from './embers.js';
import { playBurnTransition, playReverseSpawn } from '../engine/burn-transition.js';

let creatorToastTimer = null;
function creatorToast(root, msg) {
  const host = root.querySelector('[data-creator-toast]');
  if (!host) return;
  host.textContent = msg;
  host.hidden = false;
  if (creatorToastTimer) clearTimeout(creatorToastTimer);
  creatorToastTimer = setTimeout(() => { host.hidden = true; }, 4000);
}

function cdata(text) {
  return `<![CDATA[${String(text || '').replace(/]]>/g, ']]]]><![CDATA[>')}]]>`;
}
function escapeXml(s) {
  return String(s || '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

// Build the full <sim_card> XML from the stashed field values. The cast
// is authored as a list of {name, role} pairs → serialized to <cast>.
function serializeCard(fields, castRows) {
  const name = (fields.name || '').trim();
  const persona = (fields.persona || '').trim();
  const traits = (fields.traits || '').trim();
  const appearance = (fields.appearance || '').trim();
  const setting = (fields.setting || '').trim();
  const plot = (fields.plot || '').trim();
  const tone = (fields.tone || '').trim();
  const playerName = (fields.playerName || '').trim();
  const openingScene = (fields.openingScene || '').trim();

  let xml = '<sim_card>\n';
  xml += `  <metadata><type>roleplay</type></metadata>\n`;
  xml += '  <identity>\n';
  // name is required; persona/traits/appearance may come from the
  // attached player (carried in via the onSave handler), but if the
  // user typed them here they serialize too.
  xml += `    <name>${escapeXml(name)}</name>\n`;
  if (persona) xml += `    <persona>${cdata(persona)}</persona>\n`;
  if (traits) xml += `    <traits>${cdata(traits)}</traits>\n`;
  xml += '  </identity>\n';
  if (appearance) xml += `  <appearance>${cdata(appearance)}</appearance>\n`;
  if (setting) xml += `  <setting>${cdata(setting)}</setting>\n`;
  if (plot) xml += `  <plot>${cdata(plot)}</plot>\n`;
  if (tone) xml += `  <tone>${cdata(tone)}</tone>\n`;
  // Cast block: each named character → an <npc> with a slug id + role.
  const npcs = castRows.filter((r) => (r.name || '').trim());
  if (npcs.length) {
    xml += '  <cast>\n';
    for (const r of npcs) {
      const id = slugify(r.name) || `npc${npcs.indexOf(r)}`;
      const role = (r.role || '').trim();
      const roleChild = role ? `\n      <role>${cdata(role)}</role>` : '';
      xml += `    <npc id="${escapeXml(id)}">\n      <name>${escapeXml(r.name.trim())}</name>${roleChild}\n    </npc>\n`;
    }
    xml += '  </cast>\n';
  }
  if (playerName) xml += `  <player_name>${escapeXml(playerName)}</player_name>\n`;
  if (openingScene) xml += `  <opening_scene>${cdata(openingScene)}</opening_scene>\n`;
  xml += '</sim_card>\n';
  return xml;
}

function slugify(s) {
  return (s || '').trim().toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
}

// The three horizontal gates + their fields.
const GATES = [
  { id: 'world', label: 'World', fields: [
    { key: 'setting', label: 'Setting', tag: 'textarea', placeholder: 'The world premise: place, time, genre, what is possible here.', rows: 6 },
    { key: 'plot', label: 'Plot Directive', tag: 'textarea', placeholder: 'How the story moves: consequence, pacing, pressure.', rows: 5 },
    { key: 'tone', label: 'Tone', tag: 'textarea', placeholder: "Narrative voice: 'grim, atmospheric, slow-burn'.", rows: 3 },
  ]},
  { id: 'character', label: 'Character', isCast: true },
  { id: 'scenario', label: 'Scenario', fields: [
    { key: 'playerName', label: 'Player Name', tag: 'input', placeholder: "The protagonist's name (optional)", rows: 0 },
    { key: 'openingScene', label: 'Opening Scene', tag: 'textarea', placeholder: 'The first narrator beat — where the player begins.', rows: 8 },
  ]},
];

function fieldMarkup(f) {
  const control = f.tag === 'textarea'
    ? `<textarea data-field="${f.key}" rows="${f.rows}" placeholder="${f.placeholder || ''}"></textarea>`
    : `<input type="text" data-field="${f.key}" placeholder="${f.placeholder || ''}" autocomplete="off">`;
  return `<label class="fable-creator-field">
    <span class="fable-creator-label">${f.label}</span>
    ${control}
  </label>`;
}

// The Character gate renders a dynamic cast-row editor (add/remove
// named characters). Each row: name + role + a remove ✕.
function castEditorMarkup() {
  return `<div class="fable-cast-editor" data-cast-editor>
    <div class="fable-cast-rows" data-cast-rows></div>
    <button class="fable-cast-add" type="button" data-cast-add>+ Add Character</button>
  </div>`;
}

export function buildCreator() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-creator-screen';
  root.dataset.fableScreen = 'creator';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-glow" aria-hidden="true"></div>
    <div class="fable-ember-host" aria-hidden="true"></div>
    <div class="fable-creator-gates fable-creator-gates--horizontal" data-gates>
      ${GATES.map((g) => `<button class="fable-newgame-tile fable-creator-gate" type="button" data-gate="${g.id}"><span class="fable-newgame-tile-caption">${g.label.toUpperCase()}</span></button>`).join('')}
    </div>
    <div class="fable-creator-form" data-form hidden></div>
    <button class="fable-creator-save" type="button" data-act="save" hidden>Save &amp; Play</button>
    <div class="fable-creator-toast" data-creator-toast hidden></div>
  `;

  const gatesHost = root.querySelector('[data-gates]');
  const formHost = root.querySelector('[data-form]');
  const saveBtn = root.querySelector('[data-act="save"]');
  // Stash field values across gate switches so collapsing doesn't lose work.
  root._stashed = { fields: {}, cast: [] };

  // Read current visible form values into the stash.
  function stashVisible() {
    formHost.querySelectorAll('[data-field]').forEach((el) => {
      root._stashed.fields[el.dataset.field] = el.value;
    });
    // Cast rows → array of {name, role}.
    if (formHost.querySelector('[data-cast-rows]')) {
      root._stashed.cast = Array.from(formHost.querySelectorAll('.fable-cast-row')).map((row) => ({
        name: (row.querySelector('[data-cast-name]') || {}).value || '',
        role: (row.querySelector('[data-cast-role]') || {}).value || '',
      }));
    }
  }

  // Click a gate → burn the other two, expand this one's form.
  gatesHost.querySelectorAll('[data-gate]').forEach((btn) => {
    btn.addEventListener('click', () => {
      const id = btn.dataset.gate;
      const rejected = Array.from(gatesHost.querySelectorAll('[data-gate]')).filter((b) => b !== btn);
      stashVisible();
      playBurnTransition({
        selectedBtn: btn,
        rejectedBtns: rejected,
        onComplete: () => {
          rejected.forEach((b) => { b.style.opacity = '0'; b.style.pointerEvents = 'none'; });
          btn.classList.add('is-survivor');
          const gate = GATES.find((g) => g.id === id);
          let body;
          if (gate.isCast) {
            body = `<button class="fable-creator-collapse" type="button" data-collapse>‹ Back</button>` + castEditorMarkup();
          } else {
            body = `<button class="fable-creator-collapse" type="button" data-collapse>‹ Back</button>` + gate.fields.map(fieldMarkup).join('');
          }
          formHost.innerHTML = body;
          formHost.hidden = false;
          saveBtn.hidden = false;
          // Restore stashed values into the freshly-rendered fields.
          formHost.querySelectorAll('[data-field]').forEach((el) => {
            const v = root._stashed.fields[el.dataset.field];
            if (v != null) el.value = v;
          });
          // Wire cast editor if present.
          if (gate.isCast) wireCastEditor(root);
          formHost.querySelector('[data-collapse]').addEventListener('click', () => collapseForm(root));
        },
      });
    });
  });

  function collapseForm(root) {
    stashVisible();
    formHost.hidden = true;
    saveBtn.hidden = true;
    const survivor = gatesHost.querySelector('.fable-creator-gate.is-survivor');
    const rejected = Array.from(gatesHost.querySelectorAll('[data-gate]')).filter((b) => !b.classList.contains('is-survivor'));
    gatesHost.querySelectorAll('[data-gate]').forEach((b) => {
      b.style.opacity = '';
      b.style.pointerEvents = '';
      b.classList.remove('is-survivor');
    });
    playReverseSpawn(rejected);
    if (survivor) survivor.style.opacity = '1';
  }

  // Cast editor: add/remove rows. Each row is a name + role + ✕.
  function wireCastEditor(root) {
    const rowsHost = formHost.querySelector('[data-cast-rows]');
    const addBtn = formHost.querySelector('[data-cast-add]');
    const renderRow = (name = '', role = '') => {
      const row = document.createElement('div');
      row.className = 'fable-cast-row';
      row.innerHTML = `
        <input type="text" data-cast-name placeholder="Name" value="${escapeAttr(name)}" autocomplete="off">
        <input type="text" data-cast-role placeholder="Role (optional)" value="${escapeAttr(role)}" autocomplete="off">
        <button class="fable-cast-remove" type="button" data-cast-remove aria-label="Remove">✕</button>
      `;
      row.querySelector('[data-cast-remove]').addEventListener('click', () => row.remove());
      return row;
    };
    // Seed with stashed cast (or one empty row).
    const seed = root._stashed.cast.length ? root._stashed.cast : [{ name: '', role: '' }];
    seed.forEach((c) => rowsHost.appendChild(renderRow(c.name, c.role)));
    addBtn.addEventListener('click', () => rowsHost.appendChild(renderRow()));
  }

  // Ambient embers.
  const emberHost = root.querySelector('.fable-ember-host');
  let embers = null;
  root._startAmbient = () => { if (!embers) embers = createEmbers(emberHost); };
  root._stopAmbient = () => { if (embers) { embers.destroy(); embers = null; } };

  return root;
}

function escapeAttr(s) {
  return String(s || '').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}

// Populate/refresh hook. Resets the form to the gates view.
export function renderCreator(root, handlers) {
  root._stashed = { fields: {}, cast: [] };
  const gatesHost = root.querySelector('[data-gates]');
  const formHost = root.querySelector('[data-form]');
  const saveBtn = root.querySelector('[data-act="save"]');
  if (formHost) formHost.hidden = true;
  if (saveBtn) saveBtn.hidden = true;
  if (gatesHost) {
    gatesHost.querySelectorAll('[data-gate]').forEach((b) => {
      b.style.opacity = '';
      b.style.pointerEvents = '';
      b.classList.remove('is-survivor');
    });
  }
  // Re-wire Save (clone-detach).
  const oldSave = root.querySelector('[data-act="save"]');
  if (oldSave) {
    const newSave = oldSave.cloneNode(true);
    oldSave.replaceWith(newSave);
    newSave.addEventListener('click', () => onSave(root, handlers));
  }
}

async function onSave(root, handlers) {
  // Stash the currently-visible gate's fields before serializing.
  const formHost = root.querySelector('[data-form]');
  formHost.querySelectorAll('[data-field]').forEach((el) => {
    root._stashed.fields[el.dataset.field] = el.value;
  });
  if (formHost.querySelector('[data-cast-rows]')) {
    root._stashed.cast = Array.from(formHost.querySelectorAll('.fable-cast-row')).map((row) => ({
      name: (row.querySelector('[data-cast-name]') || {}).value || '',
      role: (row.querySelector('[data-cast-role]') || {}).value || '',
    }));
  }
  const fields = root._stashed.fields;
  if (!(fields.name || '').trim()) {
    // name isn't on this screen anymore (it's on the player). Use a
    // derived name from the setting or a default so the card has one.
    fields.name = (fields.setting || '').trim().split(/\s+/).slice(0, 3).join(' ') || 'New World';
  }
  const saveBtn = root.querySelector('[data-act="save"]');
  if (saveBtn) { saveBtn.disabled = true; saveBtn.textContent = 'Saving…'; }
  try {
    const xml = serializeCard(fields, root._stashed.cast);
    const meta = await invoke('fable_write_card', { stem: fields.name, xml });
    if (handlers.onSave) handlers.onSave(meta.id);
  } catch (err) {
    creatorToast(root, String(err));
  } finally {
    if (saveBtn) { saveBtn.disabled = false; saveBtn.textContent = 'Save & Play'; }
  }
}
