// =============================================================
// SCREEN: ONLINE — the in-Fable API connection window.
//
// A Fable-styled twin of the WUPI-home AI panel (the `apiPanel` IIFE in
// script.js): SAME backend IPCs (api_profiles_list / api_profile_save /
// api_profile_delete / model_source_get / api_connect / api_disconnect /
// api_profile_test), SAME state machine (profile picker + model picker +
// status bubble + connect toggle + add/edit/delete editor), but rendered with
// Fable's brass + glass aesthetic instead of the OS magenta theme. Opened from
// the title screen's ONLINE button (between Load and Exit) via openOnlinePanel
// in fable.js.
//
// The panel is self-contained: buildOnlinePanel({ onChanged }) returns a DOM
// root that mounts on document.body, wires every handler, and manages its own
// close (✕ / Esc / backdrop). It calls onChanged() whenever the API connection
// or profile list changes AND on close — so the title re-runs _refreshTitleGate
// (grayed game buttons light up + "API NOT CONNECTED" hides the moment an API
// connects).
//
// Unlike the OS panel, the connect button is a Connect/Disconnect TOGGLE: when
// an API is already active it reads "Disconnect" and calls api_disconnect, so a
// player can drop the connection from inside Fable (the title buttons re-gray).
// =============================================================

import { invoke } from '@tauri-apps/api/core';

export function buildOnlinePanel({ onChanged } = {}) {
  const onChangedCb = typeof onChanged === 'function' ? onChanged : () => {};

  const root = document.createElement('div');
  root.className = 'fable-online-popup';
  root.setAttribute('role', 'dialog');
  root.setAttribute('aria-modal', 'true');
  root.innerHTML = `
    <div class="fable-online-card">
      <div class="fable-online-head">
        <p class="fable-online-title">API Connection</p>
        <button class="fable-online-close" type="button" data-act="close" aria-label="Close">✕</button>
      </div>
      <div class="fable-online-body">
        <div class="fable-online-section">
          <div class="fable-online-profile-head">
            <span class="fable-online-label">Profile</span>
            <div class="fable-online-tools">
              <button class="fable-online-tool" type="button" data-act="edit">Edit</button>
              <button class="fable-online-tool" type="button" data-act="delete">Delete</button>
            </div>
          </div>
          <select class="fable-online-select" id="fableOnlineProfile"></select>
          <select class="fable-online-select" id="fableOnlineModel"></select>
          <div class="fable-online-bubble" id="fableOnlineBubble">
            <span class="fable-online-bubble-text">No API connected</span>
          </div>
          <button class="fable-online-connect" type="button" data-act="connect" disabled>Connect</button>
        </div>
        <div class="fable-online-divider"></div>
        <div class="fable-online-editor" id="fableOnlineEditor">
          <label class="fable-online-field">
            <span class="fable-online-field-label">Name</span>
            <input class="fable-online-input" id="fableOnlineName" type="text" placeholder="My Profile" autocomplete="off">
          </label>
          <label class="fable-online-field">
            <span class="fable-online-field-label">API URL</span>
            <input class="fable-online-input" id="fableOnlineEndpoint" type="text" placeholder="https://api.example.com/v1" autocomplete="off">
          </label>
          <label class="fable-online-field">
            <span class="fable-online-field-label">API Key</span>
            <input class="fable-online-input" id="fableOnlineKey" type="password" placeholder="sk-…" autocomplete="off">
          </label>
          <div class="fable-online-status" id="fableOnlineStatus"></div>
          <button class="fable-online-add" type="button" data-act="add" aria-label="Save profile">+</button>
        </div>
      </div>
    </div>
  `;

  // ── Element refs ───────────────────────────────────────────────────
  const profileSelect = root.querySelector('#fableOnlineProfile');
  const modelSelect = root.querySelector('#fableOnlineModel');
  const onlineBubble = root.querySelector('#fableOnlineBubble');
  const connectBtn = root.querySelector('[data-act="connect"]');
  const editorEl = root.querySelector('#fableOnlineEditor');
  const nameEl = root.querySelector('#fableOnlineName');
  const endpointEl = root.querySelector('#fableOnlineEndpoint');
  const keyEl = root.querySelector('#fableOnlineKey');
  const statusEl = root.querySelector('#fableOnlineStatus');
  const addBtn = root.querySelector('[data-act="add"]');
  const editBtn = root.querySelector('[data-act="edit"]');
  const deleteBtn = root.querySelector('[data-act="delete"]');
  const closeBtn = root.querySelector('[data-act="close"]');

  // ── State (mirrors the OS apiPanel IIFE) ───────────────────────────
  let editingId = null;          // null = creating; string = editing existing
  let lastConfig = null;         // cached for rendering
  let runtimeSource = 'local';   // actual backend source ('api' = connected)
  let activeProfileId = null;    // currently-connected profile (backend mirror)
  // Model cache: profileId → { ids: [..], selected: str }. Avoids refetching
  // /models when toggling between already-loaded profiles.
  const modelCache = new Map();
  let isOpen = true;

  function escapeHtml(s) {
    return String(s || '').replace(/[&<>"']/g, (c) => ({
      '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
    }[c]));
  }
  function setStatus(msg, kind) {
    statusEl.textContent = msg || '';
    statusEl.className = 'fable-online-status' + (kind ? ' ' + kind : '');
  }
  function findProfile(id) {
    return lastConfig?.profiles.find((p) => p.id === id) || null;
  }

  // ── Profile dropdown ───────────────────────────────────────────────
  // Sorted alphabetically by name. Active profile is flagged with a ● prefix.
  // The "Create a New Profile" placeholder shows ONLY when zero profiles exist
  // (the + button is the create affordance otherwise). Selecting it focuses the
  // editor.
  function renderProfileSelect(config) {
    lastConfig = config;
    const profiles = [...(config.profiles || [])].sort((a, b) =>
      (a.name || a.id).localeCompare(b.name || b.id));
    const prevValue = profileSelect.value;
    if (profiles.length === 0) {
      profileSelect.innerHTML = '<option value="">Create a New Profile</option>';
      profileSelect.disabled = false;
      editBtn.disabled = true;
      deleteBtn.disabled = true;
      return;
    }
    profileSelect.disabled = false;
    profileSelect.innerHTML = profiles.map((p) => {
      const isActive = p.id === config.active_profile_id;
      return `<option value="${escapeHtml(p.id)}">${isActive ? '● ' : ''}${escapeHtml(p.name || p.id)}</option>`;
    }).join('');
    const stillExists = (id) => id && [...profileSelect.options].some((o) => o.value === id);
    const target = stillExists(prevValue) ? prevValue
                 : stillExists(config.active_profile_id) ? config.active_profile_id
                 : profiles[0].id;
    profileSelect.value = target;
    const hasRealSelection = !!profileSelect.value;
    editBtn.disabled = !hasRealSelection;
    deleteBtn.disabled = !hasRealSelection;
  }

  // ── Online bubble (three states) ───────────────────────────────────
  function renderOnlineBubble() {
    // Connected: API profile active.
    if (runtimeSource === 'api' && activeProfileId) {
      const p = findProfile(activeProfileId);
      if (p) {
        onlineBubble.classList.add('active');
        onlineBubble.classList.remove('pending');
        onlineBubble.innerHTML =
          `<span class="fable-online-bubble-text">${escapeHtml(p.name || p.id)}</span>` +
          `<span class="fable-online-bubble-sep">-</span>` +
          `<span class="fable-online-bubble-model">${escapeHtml(p.model || '?')}</span>`;
        return;
      }
    }
    // Pending: profile + model picked but not yet connected.
    const pickedProfileId = profileSelect?.value;
    const pickedModel = modelSelect?.value;
    if (pickedProfileId && pickedModel) {
      const p = findProfile(pickedProfileId);
      if (p) {
        onlineBubble.classList.remove('active');
        onlineBubble.classList.add('pending');
        onlineBubble.innerHTML =
          `<span class="fable-online-bubble-text">${escapeHtml(p.name || p.id)}</span>` +
          `<span class="fable-online-bubble-sep">-</span>` +
          `<span class="fable-online-bubble-model">${escapeHtml(pickedModel)}</span>`;
        return;
      }
    }
    onlineBubble.classList.remove('active', 'pending');
    onlineBubble.innerHTML = '<span class="fable-online-bubble-text">No API connected</span>';
  }

  // ── Model dropdown (per-profile /models fetch, cached) ─────────────
  function renderModelOptions(ids, selected) {
    modelSelect.innerHTML = ids.map((id) =>
      `<option value="${escapeHtml(id)}"${id === selected ? ' selected' : ''}>${escapeHtml(id)}</option>`
    ).join('');
    modelSelect.disabled = false;
  }
  // (2026-08-16 yellow J4) Fetch-sequencing token: rapid profile switches
  // start overlapping /models fetches, and a slower EARLIER response used to
  // render provider-A's model list into profile-B's dropdown (and visa-versa
  // on the way back). Each populate bumps the token; a superseded response
  // renders nothing.
  let modelFetchSeq = 0;
  async function populateModelDropdown(profile) {
    if (!profile) {
      modelFetchSeq++;
      modelSelect.innerHTML = '<option value="">Pick a profile to load models…</option>';
      modelSelect.disabled = true;
      return;
    }
    const cached = modelCache.get(profile.id);
    if (cached) {
      modelFetchSeq++;
      const currentPick = modelSelect.value;
      const honored = (currentPick && cached.ids.includes(currentPick)) ? currentPick : cached.selected;
      renderModelOptions(cached.ids, honored);
      return;
    }
    const seq = ++modelFetchSeq;
    modelSelect.disabled = true;
    modelSelect.innerHTML = '<option value="">Loading models…</option>';
    try {
      const v = await invoke('api_profile_test', { profile });
      if (seq !== modelFetchSeq) return; // superseded by a newer selection
      const rawIds = (v && Array.isArray(v.data))
        ? v.data.map((m) => (typeof m === 'string' ? m : m?.id)).filter(Boolean)
        : [];
      if (rawIds.length === 0) {
        modelSelect.innerHTML = '<option value="">No models returned</option>';
        return;
      }
      const ids = [...rawIds].sort((a, b) =>
        a.toLowerCase().localeCompare(b.toLowerCase()) || a.localeCompare(b));
      const preferred = (profile.model && ids.includes(profile.model)) ? profile.model : ids[0];
      modelCache.set(profile.id, { ids, selected: preferred });
      renderModelOptions(ids, preferred);
    } catch (err) {
      if (seq !== modelFetchSeq) return;
      modelSelect.innerHTML = '<option value="">Failed to load models</option>';
      setStatus('Model list fetch failed: ' + err, 'err');
    }
  }

  // ── Connect button: a Connect/Disconnect toggle ───────────────────
  function updateConnectButton() {
    const connected = runtimeSource === 'api' && !!activeProfileId;
    if (connected) {
      connectBtn.textContent = 'Disconnect';
      connectBtn.classList.add('is-connected');
      connectBtn.disabled = false;
    } else {
      connectBtn.textContent = 'Connect';
      connectBtn.classList.remove('is-connected');
      connectBtn.disabled = !(profileSelect.value && modelSelect.value);
    }
  }

  async function refresh() {
    try {
      const config = await invoke('api_profiles_list');
      const extra = await invoke('model_source_get');
      lastConfig = config;
      // (2026-08-15 audit fix) Match the title gate's source-of-truth exactly:
      // source==='api' AND apiReady. The old check ignored apiReady, so the
      // bubble claimed connected while _refreshTitleGate still blocked play.
      const src = extra?.source || config.model_source;
      runtimeSource = src === 'api' && !!extra?.apiReady ? 'api' : 'local';
      activeProfileId = config.active_profile_id || null;
      renderProfileSelect(config);
      renderOnlineBubble();
      if (profileSelect.value) {
        await populateModelDropdown(findProfile(profileSelect.value));
      }
      updateConnectButton();
      setStatus('');
    } catch (err) {
      console.warn('[fable] online panel load failed', err);
    }
  }

  // ── Editor (add / edit) ────────────────────────────────────────────
  function clearEditor() {
    editingId = null;
    nameEl.value = '';
    endpointEl.value = '';
    keyEl.value = '';
    editorEl.classList.remove('editing');
    setStatus('');
  }
  function loadEditor(profile) {
    editingId = profile?.id || null;
    nameEl.value = profile?.name || '';
    endpointEl.value = profile?.endpoint || '';
    keyEl.value = profile?.api_key || '';
    editorEl.classList.add('editing');
    setStatus('Editing "' + (profile?.name || '') + '". + overwrites.');
    nameEl.focus();
  }

  // ── Handlers ───────────────────────────────────────────────────────
  profileSelect.addEventListener('change', async () => {
    const selectedId = profileSelect.value;
    if (!selectedId) {
      clearEditor();
      nameEl?.focus();
      editBtn.disabled = true;
      deleteBtn.disabled = true;
      updateConnectButton();
      renderOnlineBubble();
      return;
    }
    await populateModelDropdown(findProfile(selectedId));
    updateConnectButton();
    renderOnlineBubble();
    editBtn.disabled = false;
    deleteBtn.disabled = false;
  });
  modelSelect.addEventListener('change', () => {
    const pickedProfileId = profileSelect.value;
    const pickedModel = modelSelect.value;
    if (pickedProfileId && pickedModel) {
      const cached = modelCache.get(pickedProfileId);
      if (cached && cached.selected !== pickedModel) {
        modelCache.set(pickedProfileId, { ...cached, selected: pickedModel });
      }
    }
    updateConnectButton();
    renderOnlineBubble();
  });

  connectBtn.addEventListener('click', async () => {
    // Toggle: disconnect if already connected.
    if (runtimeSource === 'api' && activeProfileId) {
      connectBtn.disabled = true;
      setStatus('Disconnecting…', '');
      try {
        await invoke('api_disconnect');
        setStatus('Disconnected.', 'ok');
      } catch (err) {
        setStatus('Disconnect failed: ' + err, 'err');
      }
      await refresh();
      onChangedCb();
      return;
    }
    const profileId = profileSelect.value;
    const modelId = modelSelect.value;
    if (!profileId || !modelId) return;
    // Persist the chosen model into the profile before connecting (the backend
    // validates non-empty model on api_connect).
    const p = findProfile(profileId);
    if (p && p.model !== modelId) {
      try {
        // (2026-08-15 audit fix) no temperature here: the Rust backend's
        // locked fallback constant (0.85) must govern every API turn.
        await invoke('api_profile_save', { profile: { ...p, model: modelId } });
      } catch (err) {
        setStatus('Could not save model choice: ' + err, 'err');
        return;
      }
    }
    setStatus('Connecting…', '');
    connectBtn.disabled = true;
    try {
      await invoke('api_connect', { profileId });
      setStatus('Connected: API ready for Fable narration.', 'ok');
    } catch (err) {
      setStatus('Connect failed: ' + err + '.', 'err');
    }
    await refresh();
    onChangedCb();
  });

  addBtn.addEventListener('click', async () => {
    const name = nameEl.value.trim();
    if (!name) { setStatus('Name is required.', 'err'); nameEl.focus(); return; }
    if (!endpointEl.value.trim()) { setStatus('API URL is required.', 'err'); endpointEl.focus(); return; }
    if (!keyEl.value.trim()) { setStatus('API key is required.', 'err'); keyEl.focus(); return; }
    const existing = editingId ? findProfile(editingId) : null;
    const profile = {
      id: editingId || '',
      name,
      endpoint: endpointEl.value.trim(),
      api_key: keyEl.value,
        model: existing?.model || '',
      };
    addBtn.disabled = true;
    setStatus(editingId ? 'Saving…' : 'Adding…');
    try {
      const saved = await invoke('api_profile_save', { profile });
      const savedId = saved?.id || editingId || name;
      // (2026-08-16 yellow J4) The cached /models list belongs to the OLD
      // endpoint/key — an edit (or an add that reuses an id) must re-fetch,
      // never serve the previous provider's models.
      modelCache.delete(savedId);
      if (editingId && editingId !== savedId) modelCache.delete(editingId);
      clearEditor();
      await refresh();
      profileSelect.value = savedId;
      if (profileSelect.value === savedId) {
        profileSelect.dispatchEvent(new Event('change'));
        setStatus('Saved. Pick a model, then Connect.', 'ok');
      } else {
        setStatus('Saved.', 'ok');
      }
      onChangedCb();
    } catch (err) {
      setStatus('Save failed: ' + err, 'err');
    } finally {
      addBtn.disabled = false;
    }
  });

  editBtn.addEventListener('click', () => {
    const p = findProfile(profileSelect.value);
    if (!p) { setStatus('Pick a profile to edit first.', 'err'); return; }
    loadEditor(p);
  });

  // (2026-08-15 audit fix) Two-click inline delete confirm. Native confirm()
  // is dead in the wry WebView (always false → the delete silently no-ops
  // forever), so the first click ARMS (label swap + status prompt) and the
  // second click within the 5s window deletes. Any selection change or
  // further delay disarms via the timeout.
  let deleteArmTimer = 0;
  const disarmDeleteBtn = () => {
    clearTimeout(deleteArmTimer);
    deleteBtn.dataset.armed = '';
    deleteBtn.textContent = 'Delete';
  };
  deleteBtn.addEventListener('click', async () => {
    const id = profileSelect.value;
    const p = findProfile(id);
    if (!p) { disarmDeleteBtn(); setStatus('Pick a profile to delete first.', 'err'); return; }
    if (deleteBtn.dataset.armed !== '1') {
      deleteBtn.dataset.armed = '1';
      deleteBtn.textContent = 'Sure?';
      setStatus(`Click delete again to remove "${p.name || p.id}" (URL + key).`, 'err');
      clearTimeout(deleteArmTimer);
      deleteArmTimer = setTimeout(disarmDeleteBtn, 5000);
      return;
    }
    disarmDeleteBtn();
    setStatus('Deleting…');
    try {
      await invoke('api_profile_delete', { profileId: id });
      modelCache.delete(id); // (yellow J4) drop the dead profile's cache entry
      if (editingId === id) clearEditor();
      setStatus('Deleted.', 'ok');
      await refresh();
      onChangedCb();
    } catch (err) {
      setStatus('Delete failed: ' + err, 'err');
    }
  });

  // ── Close (✕ / Esc / backdrop) ─────────────────────────────────────
  function close() {
    if (!isOpen) return;
    isOpen = false;
    document.removeEventListener('keydown', onDocKey);
    root.classList.remove('is-open');
    setTimeout(() => { if (root.parentNode) root.remove(); }, 260);
    onChangedCb();   // let the title refresh its gate on close
  }
  // Document-level Esc listener (the root div isn't focusable, so a keydown on
  // it would only fire if a child had focus). Removed on close to avoid leaks.
  const onDocKey = (e) => { if (e.key === 'Escape') close(); };
  document.addEventListener('keydown', onDocKey);

  closeBtn.addEventListener('click', close);
  // Backdrop click: only a click directly on the overlay (e.target === root)
  // closes; clicks inside the card bubble up but keep their inner target.
  root.addEventListener('click', (e) => { if (e.target === root) close(); });

  // ── Initial load ───────────────────────────────────────────────────
  refresh();
  // Focus the close button so keyboard users can Esc/Tab immediately + the
  // first keydown has a target. Deferred so it runs after openOnlinePanel
  // appends the root to the DOM.
  setTimeout(() => { try { closeBtn.focus(); } catch (_) {} }, 60);

  return root;
}
