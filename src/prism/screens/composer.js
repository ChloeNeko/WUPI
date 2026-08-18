// =============================================================
// PRISM COMPOSER — the Guided Slot Pipeline (Chloe ruling, 2026-08-18).
//
// THE SLOT PIPELINE (replaces the flat tag bar): CLIP reads prompts
// sequentially — the first ~15-20 tokens carry the heaviest weight for
// composition, subject count, and core structure. The composer therefore
// presents pre-defined category slots that build the prompt in expert
// order no matter what order the user clicks:
//
//   1. Subject & Count  (MANDATORY) — quick-pick chips (1girl, 1boy, 2girls,
//      no humans…; auto-solo is engine-side, prism.rs crowd gate).
//   2. Framing & Pose   (MANDATORY) — framing single-select + pose
//      multi-select chips; satisfied when either has a pick.
//   3. Environment      (recommended) — chips; does not gate anything.
//   4. Freeform         (UNLOCKED by 1+2) — the Danbooru tag search below
//      (src/prism/data/danbooru-tags.json, ~140K tags; general + series +
//      character rows, artist + meta excluded; ranked most→least popular;
//      click/Enter adds a pill; typed text NEVER becomes a pill — users can
//      only add tags the model knows; focusing the empty search greets with
//      the TOP 100 tags).
//
// Mandatory slots can never be left EMPTY once filled — you can SWAP a
// choice (pick another chip), but only Clear prompt empties everything.
// The freeform search stays disabled until slots 1+2 are satisfied.
//
// THE TOGGLES (2026-08-18, replacing tag-detection steering): NSFW and
// Furry switches in the settings rail. They route danbooru rating/furry
// tags server-side (prism.rs): NSFW on → `nsfw, explicit` positive +
// `safe` negative; off → the reverse. Furry on → `furry, anthro`
// positive; off → they ride the negative (the checkpoint's training mix
// is furry-heavy — leakage suppression). Persisted in localStorage.
//
// THE LOCKED RECIPE v2 (server-side, invisible): base DPM++ 2M + Karras at
// 20 steps + the MANDATORY ESRGAN hires refine (1.5×, 12 effective steps,
// 0.45 denoise) + the quality prefix + negative block are enforced in
// prism::build_request / scene_art (Rust). The engine-injected meta
// families are excluded from search here — they never surface. The
// user-facing knobs: the slot pills, the size presets (the 7 NoobAI
// buckets), the seed (always -1/random here — seed iteration is Fork &
// Edit's primitive), + the two toggles.
//
// Build/wire/teardown triplet (FABLE convention): buildEl() constructs the
// DOM + returns it; wire() attaches listeners + subscribes to events;
// teardown() removes listeners so a close mid-generation can't fire handlers
// against a detached node.
// =============================================================

import { GEN_DEFAULTS, generate } from '../engine/api.js';
import CATALOG from '../data/danbooru-tags.json';
import {
  SLOT_SETS, createSlots, slotsSatisfied, compilePrompt, splitIntoSlots,
} from '../engine/slots.js';

// ── The search index (built once per process, lazily on first use) ───────
//
// Rows in the JSON are [tag, category, postCount, "alias;alias"] — compact
// arrays, count-sorted descending (the CSV source order). General (0),
// series (3) + character (4) tags are indexed; artist (1) + meta (5) are
// excluded at index time. Danbooru tags use underscores ("long_hair");
// users type spaces. We match on the underscore form + DISPLAY/PROMPT in
// the space form (the standard SD prompt convention). Aliases go through
// the same key form ("oppai" → "breasts"; "luffy" → "monkey d. luffy").

// The engine-injected families under the locked recipe — never visible in
// search, never clickable (2026-08-17 ruling: if Rust injects it, the user
// can't also add it). Groups: the NoobAI v1.1 positive prefix + the v2
// negative block, the crowd-logic subject tags (solo / no humans), and the
// TOGGLE-ROUTED steering tags (safe / nsfw / explicit / furry / anthro —
// toggles own them now, not tag detection).
// (Keep in sync with scene_art.rs PRISM_QUALITY_PREFIX /
// PRISM_NEGATIVE_BLOCK, prism.rs's subject/rating constants + scripts/
// build-danbooru-catalog.cjs.)
const META_EXCLUDED = new Set([
  // positive prefix
  'masterpiece', 'best_quality', 'newest', 'absurdres', 'highres',
  // negative block (v2 — the retired hand tags stay excluded: negative-block
  // vocabulary is never a positive pill)
  'worst_quality', 'old', 'early', 'low_quality', 'lowres', 'signature',
  'username', 'logo', 'bad_hands', 'mutated_hands',
  // crowd-logic subject gate (prism.rs)
  'solo', 'no_humans',
  // toggle steering (prism.rs — the NSFW/Furry toggles route these)
  'safe', 'nsfw', 'explicit', 'furry', 'anthro',
]);

// The searchable Danbooru categories: 0 general, 3 copyright/series,
// 4 character (2026-08-17 re-ruling — character/series names returned to
// the index; removing them broke "Luffy"-style name search). Artist (1)
// + meta (5) rows stay excluded.
const SEARCHABLE_CATEGORY = new Set([0, 3, 4]);

const MAX_SUGGESTIONS = 14;
// Focusing the empty search greets with the popularity head of the catalog.
const TOP_TAGS_ON_FOCUS = 100;
// Alias scanning over the index is the expensive pass — only for queries
// long enough that alias hits are likely worth the cost.
const MIN_ALIAS_QUERY = 3;

// The NoobAI bucket presets (Chloe ruling, 2026-08-17 — the model card's
// recommended ~1.0 MP set). Ordered tall → square → wide; the default is
// the portrait 832×1216 (characters are the primary use).
const BUCKETS = [
  [768, 1344], [832, 1216], [896, 1152], [1024, 1024],
  [1152, 896], [1216, 832], [1344, 768],
];

// localStorage keys for the steering toggles (persist across composer
// opens — an NSFW preference shouldn't silently reset to SFW).
const LS_NSFW = 'prism.toggle.nsfw';
const LS_FURRY = 'prism.toggle.furry';

let index = null; // { keys, display, count, aliases } — parallel arrays

function buildIndex() {
  const keys = [];     // lowercase match key (spaces/parens/periods collapsed away)
  const display = [];  // space form (pill text + suggestion label)
  const count = [];
  const aliases = [];  // key-form "a;b" ('' when none)
  for (const row of CATALOG.tags) {
    if (!SEARCHABLE_CATEGORY.has(row[1])) continue;
    const tag = String(row[0] || '').toLowerCase();
    if (!tag || META_EXCLUDED.has(tag)) continue;
    keys.push(matchKey(tag));
    display.push(String(row[0]).replace(/_/g, ' '));
    count.push(row[2]);
    aliases.push(matchKey(String(row[3] || '')));
  }
  index = { keys, display, count, aliases };
}

/**
 * Normalize a tag or query to the match-key form: lowercase, whitespace →
 * underscores, parens + periods stripped. The paren strip is load-bearing
 * for the 40K+ disambiguated tags (`rem_(re:zero)` → key `rem_re:zero`) —
 * without it, typing "rem re" can never prefix-match the real tag. The
 * period strip covers name tags (`monkey_d._luffy` → `monkey_d_luffy`) so
 * "monkey d luffy" typed without punctuation still matches.
 */
function matchKey(s) {
  return s.trim().toLowerCase().replace(/\s+/g, '_').replace(/[().]/g, '');
}

/** Normalize a user query to the match key form. */
function queryKey(q) {
  return matchKey(q);
}

/**
 * The popularity head of the catalog (the index is count-sorted, so the
 * first N rows ARE the top-N tags) — the focus greeting list.
 */
function topTagRows(n) {
  if (!index) buildIndex();
  const out = [];
  for (let i = 0; i < index.keys.length && out.length < n; i++) out.push({ i });
  return out;
}

/**
 * Search the catalog. Tag-key hits (startsWith OR substring — "luffy"
 * substring-matches "monkey d. luffy") are merged and ranked by post
 * count: the most popular match sits at the very top, prefix or not.
 * Each bucket collects its own top-MAX (the index is count-sorted, so the
 * first hits ARE each bucket's most popular — the merged top-MAX by
 * count is exact without a full-collection sort). Alias matches fill any
 * shortfall after key hits.
 * @returns {Array<{i: number}>} index rows (use index.display)
 */
function searchTags(query) {
  if (!index) buildIndex();
  const q = queryKey(query);
  if (!q) return [];
  const { keys, count, aliases } = index;
  const seen = new Set();
  const prefix = [];   // top startsWith hits, count order
  const substr = [];   // top substring hits, count order
  for (let i = 0; i < keys.length; i++) {
    if (prefix.length >= MAX_SUGGESTIONS && substr.length >= MAX_SUGGESTIONS) break;
    // An overflowed bucket still marks the row seen so the alias pass
    // can't re-add a key hit that already lost its slot to a popular peer.
    if (keys[i].startsWith(q)) {
      seen.add(i);
      if (prefix.length < MAX_SUGGESTIONS) prefix.push(i);
    } else if (keys[i].includes(q)) {
      seen.add(i);
      if (substr.length < MAX_SUGGESTIONS) substr.push(i);
    }
  }
  const out = prefix.concat(substr).sort((a, b) => count[b] - count[a])
    .slice(0, MAX_SUGGESTIONS);
  // Alias contains (the "oppai → breasts" pass) — only for queries long
  // enough that alias hits are likely worth the scan.
  if (out.length < MAX_SUGGESTIONS && q.length >= MIN_ALIAS_QUERY) {
    for (let i = 0; i < aliases.length && out.length < MAX_SUGGESTIONS; i++) {
      if (!seen.has(i) && aliases[i].includes(q)) { out.push(i); seen.add(i); }
    }
  }
  return out.map((i) => ({ i }));
}

// The live settings state. Mutated by the settings rail controls; read by
// the Generate button. Size + the two steering toggles are user-facing
// (locked recipe v2; the seed rides along at -1/random — GEN_DEFAULTS —
// and is never mutated here).
let settings = { ...GEN_DEFAULTS };

// The guided slot state (engine/slots.js shape: subject/framing single,
// pose/env/free arrays). The compiled prompt is ALWAYS slot-ordered.
let slotState = createSlots();

// Suggestion dropdown state (per-composer-open).
let suggestionRows = []; // [{ i }] — the current search results
let activeIdx = -1;      // highlighted row (keyboard nav)

// Event listener handles (nulled in teardown).
let unlistenGenDone = null;

// The generate-button failsafe timer. prism-gen-done (routed by prism.js)
// is the authoritative reset — but a backend failure path that never emits
// the event would leave the button spinning forever (observed during the
// 2026-08-17 broken-encoder session: one silent swap failure stuck the
// button until the app reopened). Five minutes covers the slowest legit
// swap cycle with huge margin; every done/reset path clears it.
let genFailsafeTimer = null;

// Bound handlers (kept as refs so teardown can removeExacts).
let onGenerateBound = null;
let onDocClickBound = null;

// Callbacks injected by prism.js (the router): where to go after a generate
// kicks off, + how to surface toasts.
let hooks = {};

// ── Build ───────────────────────────────────────────────────────────────

export function buildEl(routerHooks = {}) {
  hooks = routerHooks;
  settings = {
    ...GEN_DEFAULTS,
    nsfw: localStorage.getItem(LS_NSFW) === '1',
    furry: localStorage.getItem(LS_FURRY) === '1',
  };
  slotState = createSlots();
  suggestionRows = [];
  activeIdx = -1;

  const el = document.createElement('div');
  el.className = 'prism-screen prism-composer';
  el.innerHTML = `
    <div class="prism-composer-grid">
      <section class="prism-prompt-panel">
        <h2 class="prism-section-title">Compose</h2>
        <div class="prism-lane" data-lane="positive">
          <div class="prism-lane-label">Prompt</div>
          ${slotsHtml()}
          <div class="prism-pill-box" data-pillbox="positive"></div>
          <div class="prism-tagsearch">
            <input class="prism-tagsearch-input" data-input="search" type="text"
                   placeholder="Search and select a variety of tags to create your image..."
                   autocomplete="off" spellcheck="false" />
            <div class="prism-suggest" data-suggest hidden></div>
          </div>
        </div>
        <div class="prism-compose-actions">
          <button class="prism-btn prism-btn-ghost" data-action="clear-positive">Clear prompt</button>
          <button class="prism-btn prism-btn-primary prism-generate-btn" data-action="generate">
            <span class="prism-generate-label">Generate</span>
            <span class="prism-generate-spinner" hidden></span>
          </button>
        </div>
      </section>
      <aside class="prism-settings-rail">
        <h2 class="prism-section-title">Settings</h2>
        ${settingsRailHtml()}
      </aside>
    </div>
  `;
  return el;
}

/** The guided slot rows (1 Subject · 2 Framing & Pose · 3 Environment). */
function slotsHtml() {
  const chip = (slot, tag) => `
    <button type="button" class="prism-chip" data-chip="${escapeAttr(tag)}" data-slot="${slot}">${escapeHtml(tag)}</button>`;
  return `
    <div class="prism-slots">
      <div class="prism-slot" data-slotrow="subject">
        <div class="prism-slot-head">
          <span class="prism-slot-name">1 · Subject</span>
          <span class="prism-slot-rule">required</span>
        </div>
        <div class="prism-chip-row">${SLOT_SETS.subject.map((t) => chip('subject', t)).join('')}</div>
      </div>
      <div class="prism-slot" data-slotrow="framingpose">
        <div class="prism-slot-head">
          <span class="prism-slot-name">2 · Framing &amp; Pose</span>
          <span class="prism-slot-rule">required</span>
        </div>
        <div class="prism-chip-group">
          <span class="prism-chip-group-label">Framing</span>
          <div class="prism-chip-row" data-chipgrp="framing">${SLOT_SETS.framing.map((t) => chip('framing', t)).join('')}</div>
        </div>
        <div class="prism-chip-group">
          <span class="prism-chip-group-label">Pose</span>
          <div class="prism-chip-row" data-chipgrp="pose">${SLOT_SETS.pose.map((t) => chip('pose', t)).join('')}</div>
        </div>
      </div>
      <div class="prism-slot" data-slotrow="env">
        <div class="prism-slot-head">
          <span class="prism-slot-name">3 · Environment</span>
          <span class="prism-slot-rule">recommended</span>
        </div>
        <div class="prism-chip-row" data-chipgrp="env">${SLOT_SETS.env.map((t) => chip('env', t)).join('')}</div>
      </div>
    </div>
  `;
}

function settingsRailHtml() {
  return `
    <div class="prism-field">
      <label class="prism-field-label" for="prism-dim">Size</label>
      <select id="prism-dim" class="prism-select" data-setting="dim">
        <option value="768x1344">768 × 1344 (tall portrait)</option>
        <option value="832x1216" selected>832 × 1216 (portrait)</option>
        <option value="896x1152">896 × 1152 (portrait)</option>
        <option value="1024x1024">1024 × 1024 (square)</option>
        <option value="1152x896">1152 × 896 (landscape)</option>
        <option value="1216x832">1216 × 832 (landscape)</option>
        <option value="1344x768">1344 × 768 (wide landscape)</option>
      </select>
    </div>
    <div class="prism-field">
      <span class="prism-field-label">Content</span>
      <label class="prism-toggle">
        <input type="checkbox" data-setting="nsfw" ${settings.nsfw ? 'checked' : ''} />
        <span class="prism-toggle-track"><span class="prism-toggle-knob"></span></span>
        <span class="prism-toggle-text">NSFW<em>explicit rating tags</em></span>
      </label>
      <label class="prism-toggle">
        <input type="checkbox" data-setting="furry" ${settings.furry ? 'checked' : ''} />
        <span class="prism-toggle-track"><span class="prism-toggle-knob"></span></span>
        <span class="prism-toggle-text">Furry<em>anthro style tags</em></span>
      </label>
    </div>
    <div class="prism-recipe-note">
      The engine recipe (sampler, steps, CFG, quality meta, refine pass) is tuned + locked for the default model.
    </div>
  `;
}

// ── Wire ────────────────────────────────────────────────────────────────

export function wire(rootEl) {
  // Guided slot chips: delegated click on the slots container.
  const slotsEl = rootEl.querySelector('.prism-slots');
  if (slotsEl) slotsEl.addEventListener('click', (e) => onChipClick(rootEl, e));

  // Tag search input: filter-as-you-type (debounced), keyboard nav. The
  // input stays disabled until the mandatory slots are satisfied (the
  // lock state is refreshed by commitState on every slot change).
  const search = rootEl.querySelector('[data-input="search"]');
  if (search) {
    let t = null;
    search.addEventListener('input', () => {
      clearTimeout(t);
      t = setTimeout(() => onSearchInput(rootEl, search.value), 60);
    });
    search.addEventListener('keydown', (e) => onSearchKeydown(rootEl, search, e));
    search.addEventListener('focus', () => {
      // Focusing the search greets with the top-100 popularity list when
      // empty (inspiration); a typed query re-runs the search.
      if (search.value.trim()) onSearchInput(rootEl, search.value);
      else showTopTags(rootEl);
    });
  }
  // Clicking outside the search box closes the dropdown.
  onDocClickBound = (e) => {
    const box = rootEl.querySelector('.prism-tagsearch');
    if (box && !box.contains(e.target)) hideSuggest(rootEl);
  };
  document.addEventListener('click', onDocClickBound);

  // Settings rail: the size select + the two steering toggles.
  rootEl.querySelectorAll('[data-setting]').forEach((ctrl) => {
    ctrl.addEventListener('input', () => onSettingChange(ctrl));
    ctrl.addEventListener('change', () => onSettingChange(ctrl));
  });

  // Generate + clear buttons.
  onGenerateBound = () => onGenerate(rootEl);
  const genBtn = rootEl.querySelector('[data-action="generate"]');
  if (genBtn) genBtn.addEventListener('click', onGenerateBound);
  const clearBtn = rootEl.querySelector('[data-action="clear-positive"]');
  if (clearBtn) clearBtn.addEventListener('click', () => {
    // "Clear the entire tag and start from scratch" — the ONE path that
    // empties the mandatory slots.
    slotState = createSlots();
    commitState(rootEl);
  });

  // The pill box gets a delegated click listener (pill × removal) + drag
  // reorder via the HTML drag-and-drop API (freeform pills only — the
  // slot-ordered pills are not reorderable by design).
  const box = rootEl.querySelector('[data-pillbox="positive"]');
  if (box) {
    box.addEventListener('click', (e) => onPillBoxClick(rootEl, e));
    wireDragReorder(box, rootEl);
  }

  // Initial render + search lock state.
  commitState(rootEl);
}

export function teardown() {
  if (unlistenGenDone) { try { unlistenGenDone(); } catch (_) {} unlistenGenDone = null; }
  clearTimeout(genFailsafeTimer);
  genFailsafeTimer = null;
  if (onDocClickBound) {
    document.removeEventListener('click', onDocClickBound);
    onDocClickBound = null;
  }
  onGenerateBound = null;
}

// ── The guided slot pipeline ─────────────────────────────────────────────

/** Handle a quick-pick chip click (delegated from .prism-slots). */
function onChipClick(rootEl, e) {
  const chipEl = e.target.closest('[data-chip]');
  if (!chipEl) return;
  const slot = chipEl.dataset.slot;
  const tag = chipEl.dataset.chip;
  if (slot === 'subject') {
    // Single-select REPLACE — the slot can never be left empty once filled
    // (a different subject is a swap, not a removal).
    slotState.subject = tag;
    commitState(rootEl);
    advanceFocus(rootEl, 'framing');
    return;
  }
  if (slot === 'framing') {
    // Single-select REPLACE (one framing at a time).
    slotState.framing = tag;
    commitState(rootEl);
    advanceFocus(rootEl, 'env');
    return;
  }
  if (slot === 'pose') {
    // The mandatory slot-2 invariant: the LAST framing-or-pose pick can't
    // be deselected while there's no framing (swap to another chip or
    // Clear prompt instead) — the slot may never be left empty.
    const removing = slotState.pose.some((t) => t.toLowerCase() === tag.toLowerCase());
    if (removing && !slotState.framing && slotState.pose.length === 1) return;
    toggleListTag(slotState.pose, tag);
    commitState(rootEl);
    advanceFocus(rootEl, 'env');
    return;
  }
  if (slot === 'env') {
    toggleListTag(slotState.env, tag);
    commitState(rootEl);
    // Picking an environment advances to the unlocked freeform search.
    advanceFocus(rootEl, 'search');
  }
}

/** Toggle a tag in a multi-select list (case-insensitive dedupe). */
function toggleListTag(list, tag) {
  const i = list.findIndex((t) => t.toLowerCase() === tag.toLowerCase());
  if (i >= 0) list.splice(i, 1);
  else list.push(tag);
}

/**
 * Move the focus ring to the next station after a pick: the next slot's
 * first chip, or the freeform search once everything before it is done.
 */
function advanceFocus(rootEl, target) {
  if (target === 'search') {
    if (!slotsSatisfied(slotState)) return;
    const search = rootEl.querySelector('[data-input="search"]');
    if (search && !search.disabled) search.focus();
    return;
  }
  const row = rootEl.querySelector(`[data-chipgrp="${target}"] .prism-chip`);
  if (row) row.focus();
}

/**
 * Re-render EVERYTHING derived from the slot state: chip selected-states,
 * the ordered pill box, and the freeform search lock. The single commit
 * path — every mutation goes through here so the UI can never drift from
 * the state.
 */
function commitState(rootEl) {
  renderChips(rootEl);
  renderPills(rootEl);
  refreshSearchLock(rootEl);
}

/** Reflect the slot state onto the chip rows (is-selected). */
function renderChips(rootEl) {
  const selected = {
    subject: slotState.subject,
    framing: slotState.framing,
    pose: new Set(slotState.pose.map((t) => t.toLowerCase())),
    env: new Set(slotState.env.map((t) => t.toLowerCase())),
  };
  rootEl.querySelectorAll('[data-chip]').forEach((chipEl) => {
    const slot = chipEl.dataset.slot;
    const tag = chipEl.dataset.chip;
    const isSel = slot === 'subject' || slot === 'framing'
      ? selected[slot] === tag
      : selected[slot].has(tag.toLowerCase());
    chipEl.classList.toggle('is-selected', isSel);
    chipEl.setAttribute('aria-pressed', isSel ? 'true' : 'false');
  });
  // The mandatory slot rows glow when satisfied (visual gating feedback).
  const subjectRow = rootEl.querySelector('[data-slotrow="subject"]');
  if (subjectRow) subjectRow.classList.toggle('is-done', !!slotState.subject);
  const fpRow = rootEl.querySelector('[data-slotrow="framingpose"]');
  if (fpRow) fpRow.classList.toggle('is-done', !!slotState.framing || slotState.pose.length > 0);
}

/** Render the ordered pill box from the slot state (slot-badged pills). */
function renderPills(rootEl) {
  const box = rootEl.querySelector('[data-pillbox="positive"]');
  if (!box) return;
  const pill = (tag, slot, removable) => `
    <span class="prism-pill prism-pill-slot" data-pill="${escapeAttr(tag)}" data-slot="${slot}"${slot === 'free' ? ' draggable="true"' : ''}>
      <span class="prism-pill-text">${escapeHtml(tag)}</span>
      ${removable ? `<button class="prism-pill-x" data-remove="1" data-rem-slot="${slot}" data-rem-tag="${escapeAttr(tag)}" aria-label="remove">×</button>` : ''}
    </span>`;
  const html = [
    slotState.subject ? pill(slotState.subject, 'subject', false) : '',
    slotState.framing ? pill(slotState.framing, 'framing', false) : '',
    ...slotState.pose.map((t) => pill(t, 'pose', false)),
    ...slotState.env.map((t) => pill(t, 'env', true)),
    ...slotState.free.map((t) => pill(t, 'free', true)),
  ].join('');
  box.innerHTML = html || '<span class="prism-pill-empty">Pick a subject + framing or pose to start…</span>';
}

/**
 * The freeform search lock: disabled with a hint until the two mandatory
 * slots are satisfied (the Guided Slot Pipeline's unlock rule).
 */
function refreshSearchLock(rootEl) {
  const search = rootEl.querySelector('[data-input="search"]');
  if (!search) return;
  const ok = slotsSatisfied(slotState);
  search.disabled = !ok;
  search.placeholder = ok
    ? 'Search and select a variety of tags to create your image...'
    : 'Pick a subject and a framing or pose to unlock the tag search…';
  const wrap = rootEl.querySelector('.prism-tagsearch');
  if (wrap) wrap.classList.toggle('is-locked', !ok);
  if (!ok) hideSuggest(rootEl);
}

// ── Tag search ──────────────────────────────────────────────────────────

function onSearchInput(rootEl, value) {
  const suggest = rootEl.querySelector('[data-suggest]');
  if (!suggest) return;
  // An emptied query (while focused — input events imply focus) falls back
  // to the top-100 greeting rather than an empty dropdown.
  if (!value.trim()) {
    showTopTags(rootEl);
    return;
  }
  suggestionRows = searchTags(value);
  activeIdx = suggestionRows.length > 0 ? 0 : -1;
  if (suggestionRows.length === 0) {
    hideSuggest(rootEl);
    return;
  }
  renderSuggestions(rootEl);
  suggest.hidden = false;
}

/** The focus/empty-query greeting: the catalog's popularity head. */
function showTopTags(rootEl) {
  const suggest = rootEl.querySelector('[data-suggest]');
  if (!suggest) return;
  suggestionRows = topTagRows(TOP_TAGS_ON_FOCUS);
  activeIdx = suggestionRows.length > 0 ? 0 : -1;
  if (suggestionRows.length === 0) {
    hideSuggest(rootEl);
    return;
  }
  renderSuggestions(rootEl);
  suggest.hidden = false;
}

function renderSuggestions(rootEl) {
  const suggest = rootEl.querySelector('[data-suggest]');
  if (!suggest || !index) return;
  suggest.innerHTML = suggestionRows.map(({ i }, n) => `
    <button type="button" class="prism-suggest-item${n === activeIdx ? ' is-active' : ''}"
            data-sug="${n}" data-idx="${i}">
      <span class="prism-suggest-tag">${escapeHtml(index.display[i])}</span>
    </button>
  `).join('');
  // Delegated interactions: click adds; hover moves the highlight.
  suggest.querySelectorAll('[data-sug]').forEach((row) => {
    row.addEventListener('click', () => addSuggestion(rootEl, Number(row.dataset.sug)));
    row.addEventListener('mouseenter', () => {
      activeIdx = Number(row.dataset.sug);
      suggest.querySelectorAll('[data-sug]').forEach((r) =>
        r.classList.toggle('is-active', Number(r.dataset.sug) === activeIdx));
    });
  });
}

function addSuggestion(rootEl, n) {
  const row = suggestionRows[n];
  if (!row || !index) return;
  addPill(rootEl, index.display[row.i]);
}

function addPill(rootEl, tag) {
  // Freeform pills append to the free lane (slot order is compile-side).
  if (!slotState.free.some((t) => t.toLowerCase() === tag.toLowerCase())) {
    slotState.free.push(tag);
  }
  commitState(rootEl);
  // Clear the query + keep focus for the next search (rapid tag entry).
  const search = rootEl.querySelector('[data-input="search"]');
  if (search) { search.value = ''; search.focus(); }
  hideSuggest(rootEl);
}

function hideSuggest(rootEl) {
  const suggest = rootEl.querySelector('[data-suggest]');
  if (suggest) suggest.hidden = true;
  suggestionRows = [];
  activeIdx = -1;
}

function onSearchKeydown(rootEl, input, e) {
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    if (suggestionRows.length === 0) return;
    e.preventDefault();
    activeIdx = e.key === 'ArrowDown'
      ? (activeIdx + 1) % suggestionRows.length
      : (activeIdx - 1 + suggestionRows.length) % suggestionRows.length;
    renderSuggestions(rootEl);
    return;
  }
  if (e.key === 'Enter') {
    e.preventDefault();
    if (suggestionRows.length > 0 && activeIdx >= 0) {
      addSuggestion(rootEl, activeIdx);
    }
    return; // NO free-text commit — the input is a search box, not a prompt.
  }
  if (e.key === 'Escape') {
    input.value = '';
    hideSuggest(rootEl);
  }
  if (e.key === ',') e.preventDefault(); // commas belong to the compiled prompt, not the search
}

// ── Pill management ─────────────────────────────────────────────────────

function onPillBoxClick(rootEl, e) {
  const x = e.target.closest('[data-remove]');
  if (!x) return;
  const slot = x.dataset.remSlot;
  const tag = x.dataset.remTag;
  if (slot === 'env' || slot === 'free') {
    const list = slot === 'env' ? slotState.env : slotState.free;
    const i = list.findIndex((t) => t.toLowerCase() === tag.toLowerCase());
    if (i >= 0) list.splice(i, 1);
  }
  // subject / framing / pose pills carry no × — their slots are mandatory
  // (swap via the chip rows, or Clear prompt for the full reset).
  commitState(rootEl);
}

// Drag-to-reorder within the FREEFORM lane only (HTML DnD). The slot-
// ordered pills are not draggable — the ordering is the feature. A pill
// dropped onto another free pill moves it to that position.
function wireDragReorder(box, rootEl) {
  let dragFromIdx = null;
  box.addEventListener('dragstart', (e) => {
    const pill = e.target.closest('[data-pill]');
    if (!pill || pill.dataset.slot !== 'free') return;
    dragFromIdx = slotState.free.findIndex(
      (t) => t.toLowerCase() === pill.dataset.pill.toLowerCase()
    );
    if (dragFromIdx < 0) { dragFromIdx = null; e.preventDefault(); return; }
    e.dataTransfer.effectAllowed = 'move';
  });
  box.addEventListener('dragover', (e) => {
    if (dragFromIdx === null) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
  });
  box.addEventListener('drop', (e) => {
    if (dragFromIdx === null) return;
    e.preventDefault();
    const pill = e.target.closest('[data-pill]');
    if (!pill || pill.dataset.slot !== 'free') return;
    const toIdx = slotState.free.findIndex(
      (t) => t.toLowerCase() === pill.dataset.pill.toLowerCase()
    );
    if (toIdx < 0 || toIdx === dragFromIdx) { dragFromIdx = null; return; }
    const [moved] = slotState.free.splice(dragFromIdx, 1);
    slotState.free.splice(toIdx, 0, moved);
    dragFromIdx = null;
    commitState(rootEl);
  });
}

// ── Settings ────────────────────────────────────────────────────────────

function onSettingChange(ctrl) {
  if (ctrl.dataset.setting === 'dim') {
    const [w, h] = ctrl.value.split('x').map(Number);
    settings.width = w;
    settings.height = h;
  } else if (ctrl.dataset.setting === 'nsfw' || ctrl.dataset.setting === 'furry') {
    settings[ctrl.dataset.setting] = ctrl.checked;
    // Persist (an NSFW preference shouldn't silently reset on reopen).
    localStorage.setItem(
      ctrl.dataset.setting === 'nsfw' ? LS_NSFW : LS_FURRY,
      ctrl.checked ? '1' : '0'
    );
  }
}

// ── Generate ────────────────────────────────────────────────────────────

async function onGenerate(rootEl) {
  const prompt = compilePrompt(slotState).trim();
  if (!slotsSatisfied(slotState) || !prompt) {
    if (hooks.onToast) hooks.onToast('Pick a subject and a framing or pose first (the guided slots at the top).');
    return;
  }

  // ONLY the live knobs cross the IPC — the locked recipe (steps, CFG,
  // sampler, quality prefix, negative block, refine pass) is enforced
  // Rust-side no matter what a payload claims. The seed is always random
  // from Compose; seed iteration is Fork & Edit's primitive. The steering
  // toggles ride along for prism.rs to route.
  const params = {
    prompt,
    seed: -1,
    width: settings.width,
    height: settings.height,
    nsfw: !!settings.nsfw,
    furry: !!settings.furry,
  };

  // UI: disable the button + show a spinner.
  setGenerating(rootEl, true);
  try {
    await generate(params);
    // The result arrives via prism-gen-done (handled in prism.js). The button
    // stays in its "generating" state until that event flips it back — with
    // the failsafe below as the net for an emit-less failure path.
    if (hooks.onGenerateStarted) hooks.onGenerateStarted(params);
    clearTimeout(genFailsafeTimer);
    genFailsafeTimer = setTimeout(() => {
      setGenerating(rootEl, false);
      if (hooks.onToast) hooks.onToast('Generation timed out (no completion signal).');
    }, 5 * 60 * 1000);
  } catch (err) {
    setGenerating(rootEl, false);
    if (hooks.onToast) hooks.onToast(String(err));
  }
}

function setGenerating(rootEl, on) {
  const btn = rootEl.querySelector('.prism-generate-btn');
  if (!btn) return;
  btn.disabled = !!on;
  btn.classList.toggle('is-generating', !!on);
  const label = btn.querySelector('.prism-generate-label');
  const spinner = btn.querySelector('.prism-generate-spinner');
  if (label) label.textContent = on ? 'Generating…' : 'Generate';
  if (spinner) spinner.hidden = !!on;
  if (!on) clearTimeout(genFailsafeTimer);
}

// prism.js calls this from the prism-gen-done handler to re-enable the
// button once the (possibly multi-second) swap completes.
export function setDone(rootEl) {
  setGenerating(rootEl, false);
}

// ── Load from a gallery row (Send to Composer / Fork) ──────────────────

// Populate the composer from an existing image's prompt + toggle bits —
// used by "Send to Composer" (gallery quick action). The seed does NOT
// ride along: Compose is always random-seed; seed iteration lives in
// Fork & Edit. Rows store the USER prompt (the injected quality meta +
// steering tags are invisible + never persisted), so the slot split loads
// clean. Legacy rows (pre-recipe) may carry non-bucket dims — snap to the
// closest bucket by aspect (the size presets are the only sizes the
// composer offers). Legacy rows pre-dating the toggles read both-off.
export function loadFromImage(rootEl, img) {
  if (!img) return;
  slotState = splitIntoSlots(splitPrompt(img.prompt).join(', '));
  const dim = snapToBucket(img.width, img.height);
  settings.width = dim[0];
  settings.height = dim[1];
  settings.nsfw = !!img.nsfw;
  settings.furry = !!img.furry;
  localStorage.setItem(LS_NSFW, settings.nsfw ? '1' : '0');
  localStorage.setItem(LS_FURRY, settings.furry ? '1' : '0');
  reflectSettings(rootEl);
  commitState(rootEl);
}

function splitPrompt(s) {
  if (!s) return [];
  return s.split(',')
    .map((t) => t.trim())
    .filter(Boolean)
    // Engine-injected tags never re-enter the composer: legacy rows (and
    // hand-edited payloads) may carry `solo` / `safe` / the quality family
    // as authored pills from before they became Rust machinery — drop them
    // so what the user sees + re-saves is exactly their own vocabulary.
    .filter((t) => !META_EXCLUDED.has(matchKey(t)));
}

function snapToBucket(w, h) {
  // Closest bucket by aspect ratio (log-scale distance, scale-free) over
  // the 7-bucket preset list — legacy rows (e.g. the old 1024×576 default)
  // land on the nearest wide bucket (1344×768).
  const target = Math.log2(w / h);
  let best = BUCKETS[1]; // default portrait 832×1216
  let bestD = Infinity;
  for (const b of BUCKETS) {
    const d = Math.abs(Math.log2(b[0] / b[1]) - target);
    if (d < bestD) { bestD = d; best = b; }
  }
  return best;
}

function reflectSettings(rootEl) {
  const dim = rootEl.querySelector('[data-setting="dim"]');
  if (dim) dim.value = `${settings.width}x${settings.height}`;
  const nsfw = rootEl.querySelector('[data-setting="nsfw"]');
  if (nsfw) nsfw.checked = !!settings.nsfw;
  const furry = rootEl.querySelector('[data-setting="furry"]');
  if (furry) furry.checked = !!settings.furry;
}

// ── HTML escaping (pill text comes from user input) ────────────────────

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}
function escapeAttr(s) {
  return escapeHtml(s).replace(/'/g, '&#39;');
}
