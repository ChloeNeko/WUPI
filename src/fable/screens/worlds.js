// =============================================================
// SCREEN: WORLDS — the "Load" entry from the title (2026-08-05 rework).
//
// REWORK: this used to be a flat `.fable-card` grid (name + tone + setting
// blurb + "has saves" badge) that routed straight to the saves list. Chloe
// 2026-08-05: "The 'LOAD' button in FABLE is broken, should be hooked up to
// look exactly like the PLAYER load menu but for detecting valid .sim cards
// with the same divider and name of the card underneath." So this now mirrors
// player-picker.js: a grid of mini-cards (portrait + thin themed divider +
// NAME ONLY), each expanding into a centered modal on click.
//
// THE MODAL carries four actions: NEW / LOAD / EDIT / DELETE.
//   • NEW     → fade transition into the Player pair (slide 1), reverse-
//     spawn the buttons, with THIS card preset into the flow. Once the
//     player chooses/creates a player, the game launches straight into
//     this world (flowAfterPlayer in fable.js — no SIM pair re-pick, no
//     Codex step).
//   • LOAD    → the saves list for this card (screens/saves.js). The per-turn
//     autosave is promoted to a one-click "Resume Latest" button at the top;
//     the list below shows the manual saves (most-recent first; the backend
//     already sorts by timestamp desc). The autosave IS the latest world state.
//   • EDIT    → the raw XML editor (engine/raw-editor.js) loaded with the
//     card's <sim_card> via fable_card_raw_get_by_id, saved via
//     fable_card_raw_set_by_id. The <persona> block is a lossy merge of
//     several wizard fields (the sim creators have no reverse-parser), so the
//     faithful edit surface is the XML itself (zero data loss).
//   • DELETE  → confirm → fable_card_delete → re-render the grid.
//
// PORTRAIT: the modal's portrait is CLICKABLE → opens the 2:3 cropper →
// fable_card_portrait_write. Mirrors the player modal's portrait-change path.
//
// AMBIENCE: the SAME newgame.mp3 + fire.mp3 pair + ember background as New
// Game (started/stopped in fable.js onLoadClicked / exitLoadToTitle).
//
// Reads FableCardMeta from fable_cards_list:
//   { id, name, card_type, subtype, setting_preview, tone,
//     opening_scene_preview, player_name, has_saves, portrait, has_portrait }
// =============================================================

import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { openPortraitCropper } from './portrait-cropper.js';
import { bytesToBase64 } from './wizard-engine.js';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { buildIdCard } from '../engine/creator-engine.js';
import { renderIdCard, wireIdCard } from './id-card.js';

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

export function buildWorlds(handlers) {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-player-picker-screen';
  root.dataset.fableScreen = 'worlds';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-player-grid" data-host></div>
    <div class="fable-player-modal-overlay" data-modal hidden>
      <div class="fable-player-modal-backdrop" data-modal-backdrop></div>
      <div class="fable-player-modal" data-modal-card role="dialog" aria-modal="true"></div>
    </div>
    <div class="fable-player-confirm" data-confirm hidden>
      <div class="fable-player-confirm-card">
        <p data-confirm-msg></p>
        <div class="fable-player-confirm-actions">
          <button type="button" data-confirm-yes>Delete</button>
          <button type="button" data-confirm-no>Cancel</button>
        </div>
      </div>
    </div>
  `;
  // NOTE: the deep-void background + hearth glow + rising embers NO LONGER
  // live here — they were hoisted to a persistent .fable-flow-ambiance layer
  // on #fable (fable.js) so the background stays consistent across screen
  // swaps (the worlds grid shares the Player Picker's exact layout). This
  // screen now carries ONLY the foreground UI (the worlds grid + modal).
  return root;
}

// Populate the grid. Called each time the screen is shown (the set of cards
// may have changed since the last visit — a New Game / Creator adds one).
// `handlers` carries onNewGame(card), onResume(card), onEdit(card); DELETE is
// internal (re-renders the grid).
//
// The grid + mini-card markup is the EXACT SAME `.fable-player-grid` +
// `.fable-player-mini-card` language the Player Picker uses (Chloe 2026-08-05:
// "make the load sim card the SAME as the load player card"). 2026-08-13 delta:
// a SIM card carries a centered TYPE label row (NPC CARD / WORLD CARD /
// SCENARIO CARD, from <subtype>) ABOVE the portrait, with the name below it.
// Player cards show name only (the gender glyph was removed from both).

// The card-type label for a SIM mini-card, from its <metadata><subtype>
// ("npc" | "scenario" | "world"). Old / pre-router cards have no subtype → ''
// (no label — they render like a plain named card).
function subtypeLabel(subtype) {
  const v = (subtype || '').toLowerCase();
  if (v === 'npc') return 'NPC CARD';
  if (v === 'scenario') return 'SCENARIO CARD';
  if (v === 'world') return 'WORLD CARD';
  return '';
}
export async function renderWorlds(root, handlers) {
  root._handlers = handlers || {};
  // pickMode: a plain "pick a card" grid (no NEW/LOAD/EDIT/DELETE modal). A
  // card click calls handlers.onSelect(card) instead of openModal. Used by the
  // New Game flow's LOAD SIM CARD step.
  const pickMode = !!(handlers && handlers.pickMode && handlers.onSelect);
  const host = root.querySelector('[data-host]');
  host.innerHTML = '';
  closeModal(root);
  let cards = [];
  try {
    cards = await invoke('fable_cards_list');
  } catch (err) {
    host.innerHTML = `<div class="fable-flow-empty"><p>Couldn't load worlds: ${esc(err)}</p></div>`;
    return;
  }
  if (!cards.length) {
    host.innerHTML = `<div class="fable-flow-empty">
      <p>No scenario cards installed.</p>
      <p class="fable-flow-empty-hint">Use New Game to create one, or drop a <code>.sim</code> file into the cards folder.</p>
    </div>`;
    return;
  }
  for (const card of cards) {
    const tile = document.createElement('button');
    // SAME class as the Player Picker's mini-card → SAME CSS → identical look.
    tile.className = 'fable-player-mini-card';
    tile.type = 'button';
    tile.dataset.cardId = card.id;
    tile.title = card.name;
    tile.setAttribute('aria-label', `View card ${card.name}`);
    const portraitHTML = card.has_portrait && card.portrait_url
      ? `<div class="fable-player-mini-portrait"><img class="fable-player-mini-portrait-img" src="${esc(convertFileSrc(card.portrait_url))}" alt="" onerror="this.parentNode.classList.add('fable-player-mini-portrait--placeholder')"></div>`
      : `<div class="fable-player-mini-portrait fable-player-mini-portrait--placeholder" aria-hidden="true"></div>`;
    // A centered TYPE label (NPC/WORLD/SCENARIO CARD) sits in its own row
    // ABOVE the portrait; the name stays below the divider (2026-08-13).
    const typeLabel = subtypeLabel(card.subtype);
    tile.innerHTML = `
      ${typeLabel ? `<div class="fable-player-mini-type">${esc(typeLabel)}</div>` : ''}
      ${portraitHTML}
      <div class="fable-player-mini-divider" aria-hidden="true"></div>
      <div class="fable-player-mini-info">
        <span class="fable-player-mini-name">${esc(card.name)}</span>
      </div>`;
    tile.addEventListener('click', () => {
      if (pickMode) handlers.onSelect(card);
      else openModal(root, card);
    });
    host.appendChild(tile);
  }
}

// --- The expand-to-center modal (mirrors player-picker.openModal) --------
async function openModal(root, meta) {
  // Local double-open guard (the central flowBusy guard covers transitions;
  // modal-open isn't one). Prevents a double-fetch + double-mount.
  if (root._modalOpen) return;
  root._modalOpen = true;
  root._actionConsumed = false; // fresh open → fresh action latch
  // Invalidate any in-flight close (see closeModal) — the same stale-timer
  // fix as player-picker: a re-click inside the 260ms close window must
  // never be hidden by the previous close's deferred hide.
  root._modalGen = (root._modalGen || 0) + 1;
  const overlay = root.querySelector('[data-modal]');
  const card = root.querySelector('[data-modal-card]');
  card.innerHTML = `<div class="fable-player-modal-loading">Loading…</div>`;
  overlay.hidden = false;
  void overlay.offsetWidth;
  overlay.classList.add('is-open');

  // Click-outside + Esc to close (same discipline as the player modal).
  const onBackdropClick = (e) => {
    if (e.target === overlay || e.target.classList.contains('fable-player-modal-backdrop')) {
      closeModal(root);
    }
  };
  const onEsc = (e) => {
    // (2026-08-15 audit fix) Do NOT steal Esc while a higher overlay is open:
    // this handler runs on CAPTURE, so a stopPropagation here killed the raw
    // editor's bubble-phase Esc listener (fable.js) — the first Esc closed
    // the worlds modal UNDER the editor instead of the editor itself.
    if (document.querySelector('.fable-raw-editor-overlay')) return;
    if (e.key === 'Escape') { e.stopPropagation(); closeModal(root); }
  };
  overlay.addEventListener('click', onBackdropClick);
  document.addEventListener('keydown', onEsc, { capture: true });
  root._modalCleanup = () => {
    overlay.removeEventListener('click', onBackdropClick);
    document.removeEventListener('keydown', onEsc, { capture: true });
    root._modalCleanup = null;
  };

  // The card meta seeds the model; the raw XML parse (best-effort) fills the
  // full draft. renderModalCard renders through the shared ID-card face.
  card.innerHTML = await renderModalCard(meta);
  // Card-icon details popup (Session/Opening/anchors sections).
  wireIdCard(card);

  // Portrait click → cropper → fable_card_portrait_write.
  const portraitSlot = card.querySelector('[data-modal-portrait]');
  if (portraitSlot) {
    portraitSlot.style.cursor = 'pointer';
    portraitSlot.title = 'Change portrait';
    portraitSlot.addEventListener('click', async () => {
      try {
        const picked = await openDialog({
          multiple: false,
          filters: [{ name: 'Images', extensions: ['png', 'jpg', 'jpeg'] }],
        });
        if (!picked) return;
        const srcPath = typeof picked === 'string' ? picked : picked.path;
        let dataUrl = null;
        try {
          dataUrl = await invoke('fable_player_portrait_read_bytes', { srcPath });
        } catch (_) { dataUrl = null; }
        if (!dataUrl) return;
        const cropped = await openPortraitCropper(root, dataUrl);
        if (!cropped || !cropped.bytes) return;
        const ext = cropped.ext === 'jpeg' ? 'jpg' : cropped.ext;
        await invoke('fable_card_portrait_write', {
          cardId: meta.id,
          bytesB64: bytesToBase64(cropped.bytes),
          ext,
        });
        // Reflect the new portrait immediately. fable_card_portrait_url
        // returns a RAW absolute filesystem path — it MUST go through
        // convertFileSrc before it's a loadable web URL (the old code
        // assigned it raw → the slot went blank until a re-open; 2026-08-15).
        meta.has_portrait = true;
        const absPath = await invoke('fable_card_portrait_url', { cardId: meta.id }).catch(() => null);
        // Relative filename straight from the resolver (namesake
        // `<Name>.<ext>` since 2026-08-19 — never guess it client-side).
        meta.portrait = absPath ? absPath.split(/[\\/]/).pop() : null;
        if (absPath) {
          const freshSrc = `${convertFileSrc(absPath)}?t=${Date.now()}`;
          const img = portraitSlot.querySelector('img');
          if (img) { img.style.display = ''; img.src = freshSrc; }
          else portraitSlot.innerHTML = `<img src="${esc(freshSrc)}" alt="" onerror="this.style.display='none'">`;
          // Refresh the grid mini-card too (no screen swap needed).
          refreshMiniPortrait(root, meta.id, freshSrc);
        }
      } catch (err) {
        console.error('[fable] card portrait change failed', err);
      }
    });
  }

  // Bind the four action buttons. The NAVIGATING actions are one-per-open:
  // closeModal's hide is deferred through the ~260ms close-fade, so the
  // second click of a double-click still lands on live buttons — without
  // the latch it re-fires onResume → openWorldSaves (a double-rendered
  // saves list). _actionConsumed is reset by every openModal. Delete is
  // NOT latched (a declined confirm must stay retryable) — its own
  // confirmDelete carries the already-open guard instead.
  const consumeOnce = (fn) => () => {
    if (root._actionConsumed) return;
    root._actionConsumed = true;
    fn();
  };
  card.querySelector('[data-modal-new]').addEventListener('click', consumeOnce(() => {
    closeModal(root);
    if (root._handlers.onNewGame) root._handlers.onNewGame(meta);
  }));
  card.querySelector('[data-modal-resume]').addEventListener('click', consumeOnce(() => {
    closeModal(root);
    if (root._handlers.onResume) root._handlers.onResume(meta);
  }));
  card.querySelector('[data-modal-edit]').addEventListener('click', consumeOnce(() => {
    // (2026-08-15 audit fix) CLOSE the modal before opening the raw editor
    // (same ordering as player-picker's EDIT): leaving it open latched
    // _actionConsumed under the editor, so a later NEW/LOAD no-opped until
    // the modal was reopened.
    closeModal(root);
    if (root._handlers.onEdit) root._handlers.onEdit(meta);
  }));
  card.querySelector('[data-modal-delete]').addEventListener('click', () => {
    confirmDelete(root, meta);
  });
}

function closeModal(root) {
  const overlay = root.querySelector('[data-modal]');
  if (!overlay) return;
  // ALWAYS release the double-open guard — even on the early return (the
  // player-picker "popup dead until restart" bug class: a stale close timer
  // hides a re-opened modal while _modalOpen stays true).
  const wasOpen = !overlay.hidden;
  root._modalOpen = false;
  if (!wasOpen) return;
  if (root._modalCleanup) root._modalCleanup();
  overlay.classList.remove('is-open');
  // Generation-guarded hide: openModal bumps _modalGen, so a re-open inside
  // the close window invalidates this close's deferred hide.
  root._modalGen = (root._modalGen || 0) + 1;
  const gen = root._modalGen;
  const finish = () => {
    if (root._modalGen !== gen) return;
    overlay.hidden = true;
  };
  overlay.addEventListener('transitionend', finish, { once: true });
  setTimeout(finish, 260);
}

// Refresh one mini-card's portrait in the grid after an in-modal change
// (mirrors player-picker.refreshMiniPortrait).
function refreshMiniPortrait(root, cardId, src) {
  const tile = root.querySelector(`.fable-player-mini-card[data-card-id="${cardId}"]`);
  if (!tile) return;
  const holder = tile.querySelector('.fable-player-mini-portrait');
  if (!holder) return;
  holder.classList.remove('fable-player-mini-portrait--placeholder');
  let img = holder.querySelector('img');
  if (!img) {
    img = document.createElement('img');
    img.className = 'fable-player-mini-portrait-img';
    img.alt = '';
    holder.appendChild(img);
  }
  img.onerror = () => { holder.classList.add('fable-player-mini-portrait--placeholder'); };
  img.src = src;
}

const SILHOUETTE_SVG = `<svg class="fable-portrait-silhouette" viewBox="0 0 120 160" aria-hidden="true" focusable="false">
  <path fill="currentColor" d="M60 16c-13 0-23 11-23 25 0 9 4 16 11 21-15 6-27 19-30 36-1 6 4 12 11 12h62c7 0 12-6 11-12-3-17-15-30-30-36 7-5 11-12 11-21 0-14-10-25-23-25z"/>
</svg>`;

// The modal card: portrait on the LEFT (clickable), card identity on the
// right (name + tone + setting/intro blurb + player_name + saves state), +
// four action buttons in a centered row BELOW (NEW / LOAD / EDIT / DELETE).
// Reuses the player modal's classes so the look matches.
// CDATA-aware root-close scan (2026-08-15 audit fix): a literal
// `</sim_card>` inside authored CDATA prose fooled the naive indexOf — the
// head sliced mid-CDATA, DOMParser threw parsererror, and the modal fell
// back to meta-only. Mirrors sim_card.rs::find_root_close's discipline:
// skip `<![CDATA[ … ]]>` spans while hunting the close tag. Returns the index
// of the `</sim_card` open, or -1.
function findRootClose(text) {
  let i = 0;
  let inCdata = false;
  while (i < text.length) {
    if (inCdata) {
      const close = text.indexOf(']]>', i);
      if (close === -1) return -1; // unterminated CDATA — malformed card
      i = close + 3;
      inCdata = false;
    } else {
      const open = text.indexOf('<![CDATA[', i);
      const closeTag = text.indexOf('</sim_card', i);
      if (closeTag === -1) return -1;
      if (open !== -1 && open < closeTag) {
        i = open + 9;
        inCdata = true;
        continue;
      }
      return closeTag;
    }
  }
  return -1;
}

// Parse the card's raw .sim XML into a creator-engine draft so the modal can
// render through buildIdCard — the SAME license face the Creator review uses
// (2026-08-15 Chloe: the load menu must match the review exactly). The .sim is
// TWO-ROOT (<sim_card> + its siblings), so slice at </sim_card> first — the
// same trick sim_card.rs::parse uses; DOMParser rejects two-root documents.
// Handles BOTH generations: v2 line-block identity/persona + the
// world/location/inventory siblings, and the legacy element format.
// Best-effort: any failure returns {} and the caller falls back to list-meta.
function parseSimDraft(xmlText, subtype) {
  const out = { card_type: subtype || '' };
  try {
    const end = findRootClose(xmlText);
    let head = xmlText;
    let tail = '';
    if (end !== -1) {
      const gt = xmlText.indexOf('>', end); // tolerate `</sim_card >`
      if (gt !== -1) {
        head = xmlText.slice(0, gt + 1);
        tail = xmlText.slice(gt + 1);
      }
    }
    const doc = new DOMParser().parseFromString(head, 'text/xml');
    if (doc.querySelector('parsererror')) return out;
    const q = (sel) => {
      const el = doc.querySelector(sel);
      return el ? el.textContent.trim() : '';
    };
    // ── v2 detection: the direct <identity> child is a text LINE BLOCK (no
    // <name> element child) — the legacy shape nests <name>/<persona> inside.
    const identityEl = doc.querySelector('sim_card > identity');
    const isV2 = !!identityEl && !identityEl.querySelector('name');
    // Split a CDATA line block into {label → value} pairs.
    const labeled = (block) => {
      const map = {};
      if (!block) return map;
      for (const line of block.split(/\r?\n/)) {
        const m = line.match(/^\s*([^:]+?)\s*:\s*(.+?)\s*$/);
        if (m) map[m[1].trim().toLowerCase().replace(/[\s_-]+/g, '_')] = m[2].trim();
      }
      return map;
    };
    const listVal = (v) => (v ? v.split(',').map((s) => s.trim()).filter(Boolean) : []);

    if (isV2) {
      const idm = labeled(identityEl.textContent);
      out.name = idm.name || '';
      const ID_MAP = {
        gender: 'gender', race: 'race', age: 'age', height: 'height', weight: 'weight',
        body: 'body_type', skin: 'skin_complexion', eyes: 'eye_color',
        hair_color: 'hair_color', hair_length: 'hair_length', hair_style: 'hair_style',
        breast: 'breast_size', ears: 'ears', tail: 'tail', horn: 'horn',
      };
      for (const [k, key] of Object.entries(ID_MAP)) {
        if (idm[k]) out[key] = idm[k];
      }
      // The v2 <persona> line block (direct child).
      const personaEl = doc.querySelector('sim_card > persona');
      if (personaEl) {
        const pm = labeled(personaEl.textContent);
        if (pm.personality) out.personality = pm.personality;
        if (pm.conversation_style) out.dialogue_style = pm.conversation_style;
        if (pm.likes) out.likes = pm.likes;
        if (pm.dislikes) out.dislikes = pm.dislikes;
        if (pm.flaws) out.flaws = pm.flaws;
        if (pm.goals) out.goal = pm.goals;
        if (pm.occupation) out.job = pm.occupation;
        if (pm.backstory) out.backstory = pm.backstory;
      }
      // scenario/world dedicated prose.
      out.setting = q('sim_card > setting');
      const plot = q('sim_card > plot');
      if (plot) {
        const plm = labeled(plot);
        out.directive = plm.premise || '';
        if (plm.trigger) out.trigger_condition = plm.trigger;
        if (plm.objective) out.primary_objective = plm.objective;
        if (plm.actors) out.participating_actors = plm.actors;
        if (plm.hazards) out.environmental_hazards = plm.hazards;
        if (plm.outcomes) out.outcomes = plm.outcomes;
        // Legacy npc goal form.
        const gm = plot.match(/^Goal:\s*([\s\S]*)$/);
        if (gm && gm[1].trim()) out.goal = gm[1].trim();
      }
      // The tail siblings: world anchors + location + inventory.
      if (tail.trim()) {
        const tdoc = new DOMParser().parseFromString(`<wupi_siblings>${tail}</wupi_siblings>`, 'text/xml');
        if (!tdoc.querySelector('parsererror')) {
          const tq = (sel) => {
            const el = tdoc.querySelector(sel);
            return el ? el.textContent.trim() : '';
          };
          const wm = labeled(tq('world'));
          if (wm.date) out.date = wm.date;
          if (wm.time) out.time = wm.time;
          if (wm.weather) out.weather = wm.weather;
          if (wm.tone) out.tone = wm.tone;
          out.location = tq('location');
          const im = labeled(tq('inventory'));
          if (im.clothing) out.clothing = listVal(im.clothing);
          if (im.equipped) out.equipped = listVal(im.equipped);
          if (im.accessories) out.accessories = listVal(im.accessories);
          if (im.stored) out.stored = listVal(im.stored);
        }
      }
      return out;
    }

    // ── legacy format ──
    out.name = q('identity > name') || q('name');
    out.setting = q('setting');
    out.tone = q('tone');
    const persona = q('identity > persona') || q('persona');
    out.dialogue_style = q('conversational_style > rules');
    const npcRole = q('cast > npc > role');
    if (npcRole) out.job = npcRole;
    out.date = q('start > date');
    out.time = q('start > time');
    out.weather = q('start > weather');
    // The legacy NPC <appearance> block: `Tag: value` lines.
    const appearance = q('appearance');
    if (appearance) {
      const MAP = {
        Gender: 'gender', Race: 'race', Age: 'age', Height: 'height', Weight: 'weight',
        'Hair color': 'hair_color', 'Hair length': 'hair_length', 'Hair style': 'hair_style',
        Body: 'body_type', Skin: 'skin_complexion', Eyes: 'eye_color',
        Breast: 'breast_size', Ears: 'ears', Tail: 'tail', Horn: 'horn',
      };
      for (const line of appearance.split(/\r?\n/)) {
        const m = line.match(/^\s*([^:]+?)\s*:\s*(.+?)\s*$/);
        if (!m) continue;
        if (m[1] === 'Clothing') {
          out.clothing = m[2].split(',').map((s) => s.trim()).filter(Boolean);
        } else if (m[1] === 'Accessories') {
          out.accessories = m[2].split(',').map((s) => s.trim()).filter(Boolean);
        } else if (MAP[m[1]]) {
          out[MAP[m[1]]] = m[2];
        }
      }
    }
    // persona reads as the world/scenario Purpose (the serializer wrote the
    // directive there); on an NPC it's the serializer's labeled composition —
    // split the labels back into draft keys. Unlabeled prose falls to
    // backstory.
    if (persona) {
      if ((subtype || '').toLowerCase() === 'npc') {
        const LABELS = { Personality: 'personality', Flaws: 'flaws', Likes: 'likes', Dislikes: 'dislikes', Occupation: 'job', Backstory: 'backstory', Gear: 'gear', Tools: 'tools', Weapons: 'weapons' };
        const parts = persona.split(/\r?\n\r?\n(?=(?:Personality|Flaws|Likes|Dislikes|Occupation|Backstory|Gear|Tools|Weapons):)/);
        const leftovers = [];
        for (const part of parts) {
          const m = part.match(/^(Personality|Flaws|Likes|Dislikes|Occupation|Backstory|Gear|Tools|Weapons):\s*([\s\S]*)$/);
          if (!m) { if (part.trim()) leftovers.push(part.trim()); continue; }
          const val = m[2].trim();
          if (!val) continue;
          const key = LABELS[m[1]];
          out[key] = ['gear', 'tools', 'weapons'].includes(key)
            ? val.split(',').map((s) => s.trim()).filter(Boolean)
            : val;
        }
        if (leftovers.length) out.backstory = [out.backstory, ...leftovers].filter(Boolean).join('\n\n');
      } else {
        out.directive = persona;
      }
    }
    // NPC: the card-level <plot> carries the goal (the serializer wrote
    // "Goal: …"). Scenario <plot> stays a scenario-only concern.
    if ((subtype || '').toLowerCase() === 'npc') {
      const gm = q('plot').match(/^Goal:\s*([\s\S]*)$/);
      if (gm && gm[1].trim()) out.goal = gm[1].trim();
    }
  } catch (_) { /* best-effort — meta fallback */ }
  return out;
}

// The modal card: the shared ID-card renderer over the parsed draft — the
// SAME face the Creator review shows (header + license grid + corner cluster
// + details popup). List-meta fields cover the fallback; Session info (bound
// player + saves) rides as an extra section behind the details popup.
async function buildModalModel(meta) {
  let draft = { card_type: meta.subtype || '', name: meta.name, setting: meta.setting_preview, tone: meta.tone };
  try {
    const xml = await invoke('fable_card_raw_get_by_id', { cardId: meta.id });
    draft = { ...draft, ...parseSimDraft(xml, meta.subtype) };
  } catch (_) { /* unreadable card — meta-only draft */ }
  const model = buildIdCard('sim', draft);
  if (!model) return null;
  const sessRows = [];
  if (meta.player_name) sessRows.push(['Player', meta.player_name]);
  sessRows.push(['Saves', meta.has_saves ? 'Has saved games' : 'No saves yet']);
  model.extra.push(['Session', sessRows]);
  if (meta.opening_scene_preview) {
    model.extra.push(['Opening scene', [['Preview', String(meta.opening_scene_preview)]]]);
  }
  return model;
}

async function renderModalCard(card) {
  const model = await buildModalModel(card);
  const portraitSrc = (card.has_portrait && card.portrait_url) ? convertFileSrc(card.portrait_url) : '';
  const portraitHTML = portraitSrc
    ? `<img src="${esc(portraitSrc)}" alt="" onerror="this.style.display='none'">`
    : `<span class="fable-player-review-portrait-fallback" aria-hidden="true">${SILHOUETTE_SVG}</span>`;
  const cardHTML = model
    ? renderIdCard(model, { portraitClickable: false, portraitHtml: portraitHTML })
    // Defensive fallback (buildIdCard never returns null for 'sim', but the
    // modal must never blank): the old hand-rolled section card.
    : `<div class="fable-player-review-card"><div class="fable-player-review-top">
        <div class="fable-player-review-portrait" data-modal-portrait>${portraitHTML}</div>
        <div class="fable-player-review-body"><section class="fable-player-review-section">
          <h3>${esc(card.name)}</h3></section></div></div></div>`;
  return cardHTML + `
    <div class="fable-player-modal-actions">
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--load" data-modal-new>NEW</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--load" data-modal-resume>LOAD</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--edit" data-modal-edit>EDIT</button>
      <button type="button" class="fable-player-modal-btn fable-player-modal-btn--delete" data-modal-delete>DELETE</button>
    </div>`;
}

// --- Delete confirmation (mirrors player-picker.confirmDelete) ------------
function confirmDelete(root, card) {
  const confirmEl = root.querySelector('[data-confirm]');
  // Already-open guard: a double-click on the modal's delete button re-enters
  // here while the confirm is up — without this, every re-entry stacks another
  // yes/no listener pair (one click = N deletes). A declined confirm closes
  // fully, so a fresh delete click re-runs cleanly.
  if (!confirmEl.hidden && confirmEl.classList.contains('is-open')) return;
  const msg = root.querySelector('[data-confirm-msg]');
  const yes = root.querySelector('[data-confirm-yes]');
  const no = root.querySelector('[data-confirm-no]');
  msg.textContent = `Delete ${card.name}? This removes the card and ALL its saves. This cannot be undone.`;
  confirmEl.hidden = false;
  // Invalidate any in-flight close (the same stale-timer class as closeModal).
  root._confirmGen = (root._confirmGen || 0) + 1;
  void confirmEl.offsetWidth;
  confirmEl.classList.add('is-open');

  const close = () => {
    confirmEl.classList.remove('is-open');
    root._confirmGen = (root._confirmGen || 0) + 1;
    const gen = root._confirmGen;
    const finish = () => { if (root._confirmGen === gen) confirmEl.hidden = true; };
    confirmEl.addEventListener('transitionend', finish, { once: true });
    setTimeout(finish, 200);
  };

  const onYes = async () => {
    cleanup();
    close();
    try {
      await invoke('fable_card_delete', { cardId: card.id });
      // Re-render the grid (reflects the deletion + closes the modal).
      renderWorlds(root, root._handlers);
    } catch (err) {
      const cardEl = root.querySelector('[data-modal-card]');
      const note = document.createElement('p');
      note.className = 'fable-player-modal-error';
      note.textContent = `Delete failed: ${err}`;
      cardEl.appendChild(note);
    }
  };
  const onNo = () => { cleanup(); close(); };
  function cleanup() {
    yes.removeEventListener('click', onYes);
    no.removeEventListener('click', onNo);
  }
  yes.addEventListener('click', onYes);
  no.addEventListener('click', onNo);
}
