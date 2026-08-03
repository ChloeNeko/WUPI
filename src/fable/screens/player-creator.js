// =============================================================
// SCREEN: PLAYER CREATOR — author a standalone, reusable player.
//
// A single form (NOT a burn-button flow): name, description, appearance,
// personality, accessories, + a portrait upload slot. All fields visible
// together — no section-hiding burn here (the burn is for the menu pairs).
//
// The chrome (‹ / ⌂) is owned by the flow controller — no header bar.
//
// PORTRAIT UPLOAD: a circular avatar slot opens the native file dialog
// (@tauri-apps/plugin-dialog → PNG/JPG). The picked path is passed to
// the fable_player_portrait_upload IPC, which reads + writes the bytes
// server-side. The returned absolute path is shown via convertFileSrc.
//
// VALIDATION LOCK: a client-side mirror of Rust's validate_player
// (player.rs). Runs on every input; Save stays disabled + a status line
// shows the reason until valid (mirrors raw-editor.js:setValid). The
// authoritative gate re-runs server-side on fable_player_write.
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { createEmbers } from './embers.js';

// --- Client-side validation mirror (player.rs::validate_player) ----
const NAME_MAX = 64;
const PROSE_MAX = 4000;
const CONTROL_RE = /[\u0000-\u0008\u000B\u000C\u000E-\u001F]/;

function validatePlayer(fields) {
  const name = (fields.name || '').trim();
  if (!name) return 'Name is required.';
  if (name.length > NAME_MAX) return `Name must be ${NAME_MAX} characters or fewer.`;
  if (CONTROL_RE.test(fields.name || '')) return 'Name contains invalid control characters.';
  for (const [label, val] of [
    ['Description', fields.description],
    ['Appearance', fields.appearance],
    ['Personality', fields.personality],
    ['Accessories', fields.accessories],
  ]) {
    const s = (val || '').trim();
    if (s.length > PROSE_MAX) return `${label} must be ${PROSE_MAX} characters or fewer.`;
    if (val && CONTROL_RE.test(val)) return `${label} contains invalid control characters.`;
  }
  return null;
}

let playerToastTimer = null;
function playerToast(root, msg) {
  const host = root.querySelector('[data-player-toast]');
  if (!host) return;
  host.textContent = msg;
  host.hidden = false;
  if (playerToastTimer) clearTimeout(playerToastTimer);
  playerToastTimer = setTimeout(() => { host.hidden = true; }, 4000);
}

const FIELDS = [
  { key: 'name', label: 'Name', required: true, tag: 'input', placeholder: 'e.g. Kaelen Voss', rows: 0 },
  { key: 'description', label: 'Description', tag: 'textarea', placeholder: 'Backstory, identity, who they are at their core.', rows: 4 },
  { key: 'appearance', label: 'Appearance', tag: 'textarea', placeholder: 'Physical description: age, race, build, hair, eyes, clothing.', rows: 5 },
  { key: 'personality', label: 'Personality', tag: 'textarea', placeholder: 'Demeanor, voice, mannerisms, temperament.', rows: 5 },
  { key: 'accessories', label: 'Accessories', tag: 'textarea', placeholder: 'Signature carried items, trinkets, gear.', rows: 3 },
];

function fieldMarkup(f) {
  const req = f.required ? ' <em>(required)</em>' : '';
  const control = f.tag === 'textarea'
    ? `<textarea data-field="${f.key}" rows="${f.rows}" placeholder="${f.placeholder || ''}"></textarea>`
    : `<input type="text" data-field="${f.key}" placeholder="${f.placeholder || ''}" autocomplete="off">`;
  return `<label class="fable-creator-field">
    <span class="fable-creator-label">${f.label}${req}</span>
    ${control}
  </label>`;
}

function readFields(root) {
  const f = {};
  root.querySelectorAll('[data-field]').forEach((el) => { f[el.dataset.field] = el.value; });
  return f;
}

function opt(v) { const s = (v || '').trim(); return s ? s : null; }
function slugify(name) {
  const s = (name || '').trim().toLowerCase()
    .replace(/[^a-z0-9_-]+/g, '-').replace(/^-+|-+$/g, '');
  return s || 'player';
}

export function buildPlayerCreator() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-player-creator-screen';
  root.dataset.fableScreen = 'player-creator';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-void-glow" aria-hidden="true"></div>
    <div class="fable-ember-host" aria-hidden="true"></div>
    <div class="fable-player-creator-body">
      <div class="fable-portrait-slot" data-portrait-slot>
        <span class="fable-portrait-slot__placeholder" aria-hidden="true"></span>
        <span class="fable-portrait-slot__hint">Portrait</span>
      </div>
      <div class="fable-creator-form" data-form>
        ${FIELDS.map(fieldMarkup).join('')}
      </div>
      <div class="fable-player-status" data-status></div>
    </div>
    <button class="fable-creator-save" type="button" data-act="save" disabled>Save Player</button>
    <div class="fable-creator-toast" data-player-toast hidden></div>
  `;

  const saveBtn = root.querySelector('[data-act="save"]');
  const statusEl = root.querySelector('[data-status]');
  const portraitSlot = root.querySelector('[data-portrait-slot]');
  let currentPortraitPath = null;

  // Re-validate on every input.
  root.querySelectorAll('[data-field]').forEach((el) => {
    el.addEventListener('input', () => revalidate());
  });

  function revalidate() {
    const fields = readFields(root);
    const err = validatePlayer(fields);
    if (err) {
      saveBtn.disabled = true;
      statusEl.textContent = err;
      statusEl.classList.remove('is-valid');
    } else {
      saveBtn.disabled = false;
      statusEl.textContent = 'Ready to save.';
      statusEl.classList.add('is-valid');
    }
  }
  root._revalidate = revalidate;

  // Portrait upload flow.
  portraitSlot.addEventListener('click', async () => {
    try {
      const picked = await openDialog({
        multiple: false,
        filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg'] }],
      });
      if (!picked) return;
      const srcPath = typeof picked === 'string' ? picked : (picked.path || picked);
      if (!srcPath) return;
      const fields = readFields(root);
      if (!(fields.name || '').trim()) {
        playerToast(root, 'Enter a name before uploading a portrait.');
        return;
      }
      playerToast(root, 'Uploading portrait…');
      const id = slugify(fields.name);
      const err = validatePlayer(fields);
      if (err) { playerToast(root, err); return; }
      await invoke('fable_player_write', { id, player: buildPlayer(id, fields, currentPortraitPath) });
      const absPath = await invoke('fable_player_portrait_upload', { id, srcPath });
      currentPortraitPath = absPath;
      portraitSlot.innerHTML = `<img src="${convertFileSrc(absPath)}" alt="Portrait"><span class="fable-portrait-slot__hint">Change</span>`;
      playerToast(root, 'Portrait set.');
    } catch (err) {
      playerToast(root, String(err));
    }
  });
  root._getCurrentPortrait = () => currentPortraitPath;

  function buildPlayer(id, fields, portraitPath) {
    return {
      id,
      name: (fields.name || '').trim(),
      description: opt(fields.description),
      appearance: opt(fields.appearance),
      personality: opt(fields.personality),
      accessories: opt(fields.accessories),
      portrait: portraitPath ? 'portrait' : null,
      created_at_ms: 0,
    };
  }
  root._buildPlayer = buildPlayer;

  // Ambient embers.
  const emberHost = root.querySelector('.fable-ember-host');
  let embers = null;
  root._startAmbient = () => { if (!embers) embers = createEmbers(emberHost); };
  root._stopAmbient = () => { if (embers) { embers.destroy(); embers = null; } };

  return root;
}

export function renderPlayerCreator(root, handlers) {
  // Reset all fields.
  root.querySelectorAll('[data-field]').forEach((el) => { el.value = ''; });
  const saveBtn = root.querySelector('[data-act="save"]');
  const statusEl = root.querySelector('[data-status]');
  const portraitSlot = root.querySelector('[data-portrait-slot]');
  if (saveBtn) { saveBtn.disabled = true; }
  if (statusEl) { statusEl.textContent = ''; statusEl.classList.remove('is-valid'); }
  if (portraitSlot) {
    portraitSlot.innerHTML = `<span class="fable-portrait-slot__placeholder" aria-hidden="true"></span><span class="fable-portrait-slot__hint">Portrait</span>`;
  }
  // Re-wire Save (clone-detach to avoid stacking).
  const oldSave = root.querySelector('[data-act="save"]');
  if (oldSave) {
    const newSave = oldSave.cloneNode(true);
    oldSave.replaceWith(newSave);
    newSave.addEventListener('click', () => onSave(root, handlers));
  }
}

async function onSave(root, handlers) {
  const fields = readFields(root);
  const id = slugify(fields.name);
  const player = root._buildPlayer(id, fields, root._getCurrentPortrait());
  const saveBtn = root.querySelector('[data-act="save"]');
  if (saveBtn) { saveBtn.disabled = true; saveBtn.textContent = 'Saving…'; }
  try {
    await invoke('fable_player_write', { id, player });
    if (handlers.onSave) handlers.onSave(id);
  } catch (err) {
    playerToast(root, String(err));
  } finally {
    if (saveBtn) { saveBtn.disabled = false; saveBtn.textContent = 'Save Player'; }
  }
}
