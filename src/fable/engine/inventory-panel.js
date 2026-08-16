// =============================================================
// FABLE INVENTORY PANEL — the item inspection surface for the Soul Gems.
//
// When a Soul Gem is selected (.is-active), the #inventory-panel-slot reveals
// above the backpack (the scaffold + positioning are owned by left-drawer.js /
// soul-gem.js). THIS module owns the CONTENT: it reads the live
// `player_state.{equipment, belt, pack}` from `fable_schema_get`, aggregates the
// items for the selected gem's physical category, and renders them as a
// paginated list of fixed horizontal buttons. Clicking a button opens a sleek
// contextual action popup (CONSUME / EQUIP / POCKET / STORE / DISCARD).
//
// ── The 6-zone data mapping (Appearance = Inventory) ──────────────
// Each Soul Gem maps to a physical category. When a gem is active we aggregate
// ALL items that belong to its category, drawing from the typed equipment slots
// (Outer + Inner), the belt, AND the pack — every item the player carries that
// physically belongs in that zone appears in one flat list:
//
//   Head  → hair, face modifiers, neck accessories, helmets, masks, eyewear
//   Top   → torso gear, shirts, vests, jackets, chest armor
//   Hand  → held equippables (Main/Off-hand), gauntlets, gloves, rings
//   Bottom→ pants, leggings, toolbelts, + ALL pocketed items
//   Feet  → footwear, boots, foot-jewelry
//   Inventory → dedicated bagged storage (backpacks, luggage) on the person
//
// The Rust model has no explicit "category" tag on items — it has typed SLOTS
// (Head/Chest/MainHand/OffHand/Legs/Feet) + the two stacks (belt/pack). We map
// each slot/stack to the gem category it physically belongs to:
//
//   Head gem     ← equipment.head
//   Top gem      ← equipment.chest
//   Hand gem     ← equipment.main_hand + equipment.off_hand
//   Bottom gem   ← equipment.legs + belt (the 4-slot quick rack = pockets/toolbelt)
//   Feet gem     ← equipment.feet
//   Inventory gem← pack (the deep storage = bagged inventory)
//
// ── Encumbrance & limits (PERMANENTLY REMOVED 2026-08-09) ──────────
// The weight/encumbrance system was deleted entirely — no capacity headers, no
// fill bars, no rejection, ever (a deliberate roleplay-freedom call: the pack
// is infinite). The Rust `weight` field survives only for the narrator-summary
// text readout; this module NEVER renders it + no code path enforces it. The
// `pack_capacity_lbs` field was deleted from `PlayerState` on 2026-08-11.
//
// ── The action popup ──────────────────────────────────────────────
// Clicking an item button opens a sleek popup overlaying the panel with
// contextual actions. The item's TAGS drive which actions render:
//   CONSUME — only if the item is consumable
//   EQUIP   — only if the item is equippable (opens a destination sub-menu)
//   POCKET  — only if the item is pocketable (→ moves to belt)
//   STORE   — moves the item back to bagged inventory (pack)
//   DISCARD — always available
// Tags are read DIRECTLY from the item's backend `tags` array (assigned by the
// local tracker model via the [EQUIP]/[BELT]/[PACK] tags= field — no client-
// side name heuristics). The actions mutate the live WorldSchema via
// `fable_schema_set` (the user-edit trust path, undoable via fable_rollback),
// passing an `eventNote` trace that the next narrator turn sees in
// `<world_state>`'s recent_events. See `readTags` below.
// =============================================================

import { invoke } from '@tauri-apps/api/core';
// DEV PREVIEW mock: serves a frozen test schema (full inventory) so the Soul
// Gem inspection panel renders in the no-backend preview. The mock is
// deep-cloned per read so in-memory edits (handleAction mutates the schema
// before fable_schema_set) don't poison the source.
import { isDevPreview, getDevSchema } from './dev-preview-schema.js';

// The gem id → physical category key (mirrors soul-gem.js GEMS ids).
// `slot` = the typed equipment slot(s); `stack` = 'belt' | 'pack' | null.
const CATEGORY_MAP = Object.freeze({
  head:      { slots: ['head'],                       stack: null },
  chest:     { slots: ['chest'],                      stack: null },
  hand:      { slots: ['main_hand', 'off_hand'],      stack: null },
  leg:       { slots: ['legs'],                       stack: 'belt' },
  foot:      { slots: ['feet'],                       stack: null },
  pack:      { slots: [],                             stack: 'pack' },
});

// Pagination: exactly 6 buttons visible at once. The viewport is sized to
// exactly 6 button-heights; a wheel/trackpad scroll is intercepted + advanced
// by EXACTLY one button height (with scroll-snap as the visual backstop), so
// the view never shows a partial button + always jumps smoothly by 1.
const VISIBLE_BUTTONS = 6;
// The fixed per-button height (px). Must match the CSS .inv-item-btn height +
// the .inv-track gap. Single source of truth for the scroll-step math.
const BUTTON_HEIGHT_PX = 46;   // button height
const BUTTON_GAP_PX = 8;       // vertical gap between buttons
const BUTTON_SCROLL_STEP = BUTTON_HEIGHT_PX + BUTTON_GAP_PX;

// Module state for the currently-rendered panel. Reset on hide.
let activeSlotEl = null;       // the .inventory-slot-body we paint into
let activeGemId = null;        // the selected gem id
let currentItems = [];         // the normalized items for the active gem
let cachedSchema = null;       // last-fetched full schema (so actions can mutate without a refetch race)
let wheelAccumulator = 0;      // accumulates sub-step wheel deltas until a full step
let renderSeq = 0;             // render-ownership token (see renderInventoryPanel)

// ── Normalization: turn the raw schema payload into flat item records ──────
// A normalized item is { id, name, qty, source, slot, layer, tags } where:
//   source  — 'equipment' | 'belt' | 'pack' (where it lives)
//   slot    — the EquipSlot id (for equipment items) or null
//   layer   — 'outer' | 'inner' (for equipment items) or null
//   tags    — Set of backend-assigned tags: 'consumable' | 'equippable' | 'pocketable'
//             (read DIRECTLY from the item's `tags` array — no name heuristics).
//             The local tracker model assigns these via the [EQUIP]/[BELT]/
//             [PACK] tags= field (defined in fable.prompt's AGENT section); the
//             frontend trusts them verbatim. An item with no tags renders no
//             CONSUME/EQUIP/POCKET actions (only STORE/DISCARD).
function normalizeItems(gemId, schema) {
  const out = [];
  const cat = CATEGORY_MAP[gemId];
  if (!cat || !schema) return out;
  const ps = schema.player_state || {};

  // Equipment items (typed slots). Both Outer + Inner are surfaced in the panel
  // (the Inner-hidden-from-narrator rule is a PROMPT concern, not a UI concern
  // — the player can inspect everything they're wearing).
  if (ps.equipment && typeof ps.equipment === 'object') {
    for (const slotId of cat.slots) {
      const layers = ps.equipment[slotId];
      if (!layers || typeof layers !== 'object') continue;
      for (const layer of ['outer', 'inner']) {
        const item = layers[layer];
        if (!item || typeof item !== 'object' || !item.name) continue;
        out.push({
          id: `eq:${slotId}:${layer}`,
          name: String(item.name),
          qty: 1,
          source: 'equipment',
          slot: slotId,
          layer,
          tags: readTags(item.tags),
          // (#85 2026-08-15) stats ride on the normalized record: the
          // moveToEquipmentSlot / moveToStack mutators read `item.stats`,
          // but normalizeItems never carried it — every UI move of an item
          // with tracker-assigned stats silently stripped them (the #67/#68
          // preserve fixes read a field that was never populated here).
          // Equipped items carry no weight (the Rust EquippedItem has none).
          stats: typeof item.stats === 'string' && item.stats.trim() ? item.stats : undefined,
        });
      }
    }
  }

  // Stack items (belt or pack). qty rides through.
  if (cat.stack && Array.isArray(ps[cat.stack])) {
    for (let i = 0; i < ps[cat.stack].length; i++) {
      const item = ps[cat.stack][i];
      if (!item || typeof item !== 'object' || !item.name) continue;
      out.push({
        id: `${cat.stack}:${i}`,
        name: String(item.name),
        qty: Number.isFinite(item.qty) ? item.qty : 1,
        source: cat.stack,
        slot: null,
        layer: null,
        tags: readTags(item.tags),
        // (#85) stats + the real weight ride forward (see the equipment
        // branch note) — moveToStack's 1.0 fallback fires only for items
        // that genuinely carry no weight.
        stats: typeof item.stats === 'string' && item.stats.trim() ? item.stats : undefined,
        weight: Number.isFinite(item.weight) ? item.weight : undefined,
      });
    }
  }

  return out;
}

// ── Tag reading (backend-authoritative, no heuristics) ─────────────────────
// The Rust model now carries a `tags` array on every item (assigned by the
// local tracker model via the [EQUIP]/[BELT]/[PACK] tags= field). This reads
// it verbatim into a Set of lowercase tag ids. No name-based inference — the
// tracker is the single classification authority (its rules live in
// fable.prompt's AGENT section <item_tags>). Items without a tags array get an
// empty Set (they render no CONSUME/EQUIP/POCKET actions, only STORE/DISCARD).
//
// The backend enum serializes snake_case: 'consumable' | 'equippable' |
// 'pocketable'. We normalize defensively (trim + lowercase) so a stray
// 'Equippable' from a hand-edited save still matches.
function readTags(rawTags) {
  const out = new Set();
  if (!Array.isArray(rawTags)) return out;
  for (const t of rawTags) {
    if (typeof t === 'string') {
      const id = t.trim().toLowerCase();
      if (id === 'consumable' || id === 'equippable' || id === 'pocketable') {
        out.add(id);
      }
    }
  }
  return out;
}

// ── Public: render the panel for a gem id ──────────────────────────────────
// Called by left-drawer.js showInventorySlot. Fetches the live schema, normalizes
// the items for the gem's category, and paints the paginated button list. The
// caller owns the #inventory-panel-slot scaffold (header + body); we fill the
// body. Best-effort: an IPC failure renders an empty-state message, never throws.
export async function renderInventoryPanel(slotBodyEl, gemId) {
  activeSlotEl = slotBodyEl;
  activeGemId = gemId;
  // Render-ownership token: a rapid gem switch starts a second render before
  // the first's schema fetch resolves — the slower one must NOT paint over
  // the newer one's list (both share ONE slot-body element, so element
  // identity can't discriminate; the seq can). clearInventoryPanel bumps it
  // too, so a hide mid-fetch also voids the pending paint.
  const seq = ++renderSeq;
  wheelAccumulator = 0;
  if (!slotBodyEl) return;

  // Loading placeholder (brief — the schema read is fast).
  slotBodyEl.innerHTML = '<div class="inv-loading">…</div>';

  let schema = null;
  try {
    schema = isDevPreview() ? getDevSchema() : await invoke('fable_schema_get');
    if (seq !== renderSeq) return; // superseded by a newer render / clear
    cachedSchema = schema;
  } catch (_) {
    if (seq !== renderSeq) return;
    cachedSchema = null;
    currentItems = [];
    paintEmpty(slotBodyEl, 'No inventory data.');
    return;
  }

  currentItems = normalizeItems(gemId, schema);
  paintPage(slotBodyEl);
}

// ── Public: clear state (called by hideInventorySlot) ──────────────────────
export function clearInventoryPanel() {
  renderSeq++;             // void any render still awaiting its schema fetch
  activeSlotEl = null;
  activeGemId = null;
  currentItems = [];
  cachedSchema = null;
  wheelAccumulator = 0;
  _forceCloseActionPopup(); // the slot is going away entirely — always tear down
}

// ── Public: is the action popup currently open? ────────────────────────────
// The popup is appended to document.body (so it can overlay the drawer edge +
// position freely against the viewport). Moving the mouse from a drawer item
// button onto the popup crosses the drawer's boundary → the drawer's mouseleave
// auto-close fires → the unlocked drawer yanks in mid-click (the user was
// reaching for CONSUME/EQUIP/etc.). stage.js wires this as a probe into
// left-drawer's onDrawerMouseLeave (mirroring the edgeLockVisible pattern) so
// the drawer holds while the popup is open + closes on the next genuine
// mouseleave after the popup dismisses.
export function isActionPopupOpen() {
  return popupEl !== null;
}

// ── Paint the scroll list ──────────────────────────────────────────────────
// Renders ALL the category's items into a scroll-snap track inside a viewport
// clipped to exactly VISIBLE_BUTTONS (6) button-heights. The native scroll is
// suppressed (preventDefault) + replaced by a JS step-advance that moves the
// scroll position by EXACTLY one (button + gap) per accumulated wheel notch, so
// the view always lands flush on a button boundary — never a half-button, never
// a jump of more than one. CSS scroll-snap-type is the visual backstop.
function paintPage(bodyEl) {
  if (!bodyEl) return;
  closeActionPopup(); // any open popup closes on re-render

  const total = currentItems.length;
  if (total === 0) {
    paintEmpty(bodyEl, 'Nothing here.');
    return;
  }

  bodyEl.innerHTML = '';

  const viewport = document.createElement('div');
  viewport.className = 'inv-viewport';
  // Fixed viewport height = exactly 6 buttons. Overflow hidden so a partial
  // 7th button never peeks; the JS step-scroll owns all motion.
  viewport.style.height = (VISIBLE_BUTTONS * BUTTON_HEIGHT_PX + (VISIBLE_BUTTONS - 1) * BUTTON_GAP_PX) + 'px';

  const track = document.createElement('div');
  track.className = 'inv-track';

  for (const item of currentItems) {
    track.appendChild(buildItemButton(item));
  }
  viewport.appendChild(track);
  bodyEl.appendChild(viewport);

  // ── One-button-per-scroll wheel handling ────────────────────────────────
  // Native wheel scrolling is suppressed; we accumulate deltaY until it crosses
  // one step threshold, then advance the scrollTop by exactly one button+gap in
  // the sign direction. This guarantees "scrolling jumps smoothly by exactly 1
  // button per scroll gap" regardless of trackpad inertia / mouse notch size.
  wheelAccumulator = 0;
  const WHEEL_STEP_THRESHOLD = 40; // px of accumulated delta → 1 button step
  let smoothScrolling = false;
  const stepScroll = (dir) => {
    if (smoothScrolling) return;
    smoothScrolling = true;
    const target = Math.max(0, Math.min(
      track.scrollHeight - viewport.clientHeight,
      track.scrollTop + dir * BUTTON_SCROLL_STEP
    ));
    track.scrollTo({ top: target, behavior: 'smooth' });
    // Re-enable once the smooth scroll settles (~250ms). The transitionend of
    // the scroll isn't reliably fired; a timeout matching the smooth duration
    // is the robust gate.
    setTimeout(() => { smoothScrolling = false; }, 250);
  };
  viewport.addEventListener('wheel', (e) => {
    e.preventDefault();
    wheelAccumulator += e.deltaY;
    if (Math.abs(wheelAccumulator) >= WHEEL_STEP_THRESHOLD) {
      const dir = wheelAccumulator > 0 ? 1 : -1;
      wheelAccumulator = 0;
      stepScroll(dir);
    }
  }, { passive: false });

  // Keyboard nav when the viewport is focused: Up/Down = one button step.
  viewport.tabIndex = 0;
  viewport.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown') { e.preventDefault(); stepScroll(1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); stepScroll(-1); }
  });

  // Item button clicks → action popup.
  viewport.querySelectorAll('.inv-item-btn').forEach((btn) => {
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      const id = btn.getAttribute('data-inv-item-id');
      openActionPopup(btn, id, bodyEl);
    });
  });
}

// Build one fixed-dimension item button. The name is CENTERED, UPPERCASE, BOLD.
// Long names auto-fit: the JS measures the rendered text against the fixed box
// and (a) splits onto stacked lines + (b) shrinks the font-size so it fits
// without altering the button bounds. See fitLabel below.
function buildItemButton(item) {
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.className = 'inv-item-btn';
  btn.setAttribute('data-inv-item-id', item.id);
  // Title attribute = full name on hover (accessibility + a tooltip hint).
  btn.title = item.name + (item.qty > 1 ? ` ×${item.qty}` : '');

  const label = document.createElement('span');
  label.className = 'inv-item-label';
  label.textContent = item.name.toUpperCase();
  btn.appendChild(label);

  if (item.qty > 1) {
    const qty = document.createElement('span');
    qty.className = 'inv-item-qty';
    qty.textContent = '×' + item.qty;
    btn.appendChild(qty);
  }

  // Defer the fit to after insert (needs layout to measure).
  requestAnimationFrame(() => fitLabel(btn, label, item.name.toUpperCase()));
  return btn;
}

// Auto-fit the label to its fixed button: split long names onto stacked lines
// + shrink the font-size so the text fills the box without clipping. Runs once
// per button after layout settles. Idempotent (re-measures cleanly).
const FIT_MIN_FONT = 9;      // px — never shrink below this
const FIT_MAX_LINES = 3;     // max stacked lines before we just shrink
// (2026-08-15 audit fix) Build a multi-line stacked label via DOM
// construction (text nodes + <br>), never innerHTML — the line fragments are
// model-emitted item names, and the old `stacked.map(w => w).join('<br>')`
// injected them as raw HTML (an item name containing markup = XSS in the
// webview). Identical layout: text lines separated by <br> elements.
function setStackedLabel(label, lines) {
  const frag = document.createDocumentFragment();
  lines.forEach((line, i) => {
    if (i > 0) frag.appendChild(document.createElement('br'));
    frag.appendChild(document.createTextNode(line));
  });
  label.replaceChildren(frag);
}
function fitLabel(btn, label, text) {
  if (!btn || !label) return;
  // Reset to base so measurement starts clean.
  label.style.fontSize = '';
  label.textContent = text;

  const boxW = btn.clientWidth;
  const boxH = btn.clientHeight;
  if (!boxW || !boxH) return; // not laid out yet

  // Try fitting at progressively smaller font sizes, splitting words onto
  // lines as needed. Start from the CSS default (read once).
  const style = getComputedStyle(label);
  let fontPx = parseFloat(style.fontSize) || 14;

  // Greedy word-wrap into up to FIT_MAX_LINES lines at the current font size;
  // if it doesn't fit, drop the font size + retry. Stops at FIT_MIN_FONT.
  const fit = (px) => {
    label.style.fontSize = px + 'px';
    // Measure scrollWidth/Height vs the button's content box. We use the
    // label's own scroll dimensions against the button padding box.
    return label.scrollWidth <= boxW - 16 && label.scrollHeight <= boxH - 12;
  };

  // First try: single line at full size.
  if (fit(fontPx)) return;

  // Try shrinking on one line first (single short names).
  let p = fontPx;
  while (p >= FIT_MIN_FONT && !fit(p)) p -= 0.5;
  if (fit(p)) return;

  // Still doesn't fit on one line → stack words. Insert <br> at word
  // boundaries, up to FIT_MAX_LINES, shrinking font to fit the height.
  const words = text.split(/\s+/).filter(Boolean);
  if (words.length > 1) {
    for (let lines = 2; lines <= FIT_MAX_LINES; lines++) {
      const stacked = stackWords(words, lines);
      p = fontPx;
      while (p >= FIT_MIN_FONT) {
        label.style.fontSize = p + 'px';
        setStackedLabel(label, stacked);
        if (label.scrollWidth <= boxW - 16 && label.scrollHeight <= boxH - 12) return;
        p -= 0.5;
      }
    }
    // Last resort: keep the smallest font + the max-line stack.
    label.style.fontSize = FIT_MIN_FONT + 'px';
    setStackedLabel(label, stackWords(words, FIT_MAX_LINES));
  } else {
    // Single very-long word: just clamp to min font (it may clip slightly,
    // but the button bounds are fixed per spec — title attr carries the full).
    label.style.fontSize = FIT_MIN_FONT + 'px';
  }
}

// Split `words` into `nLines` balanced groups (greedy fill). Returns array of
// joined strings.
function stackWords(words, nLines) {
  const lines = Array.from({ length: nLines }, () => []);
  // Greedy: round-robin by character length for rough balance.
  const sorted = [...words].sort((a, b) => b.length - a.length);
  for (let i = 0; i < sorted.length; i++) {
    lines[i % nLines].push(sorted[i]);
  }
  // Re-join in original word order within each line for readability.
  const order = new Map(words.map((w, i) => [w, i]));
  return lines
    .map((l) => l.sort((a, b) => (order.get(a) ?? 0) - (order.get(b) ?? 0)).join(' '))
    .filter((s) => s.length > 0);
}

// ── Empty state ────────────────────────────────────────────────────────────
function paintEmpty(bodyEl, msg) {
  if (!bodyEl) return;
  bodyEl.innerHTML = `<div class="inv-empty">${msg}</div>`;
}

// ── The action popup ───────────────────────────────────────────────────────
// A single floating overlay. Only one open at a time (singleton). Clicking
// outside or pressing Esc closes it. The popup has TWO views that swap in
// place (reusing the same element so the open-animation + position persist):
//
//   MAIN view — the contextual actions, gated on the item's backend tags:
//     CONSUME — tags has 'consumable'
//     EQUIP   — tags has 'equippable' (opens the SUB view, doesn't mutate)
//     POCKET  — tags has 'pocketable' AND item isn't already in belt
//     STORE   — item isn't already in pack
//     DISCARD — always
//
//   SUB view (EQUIP destinations) — opens when the player clicks EQUIP. Shows
//   the five body-zone destinations (HEAD/TOP/HAND/BOTTOM/FEET) + a ‹ BACK
//   button to return to the MAIN view. Picking a destination fires the slot
//   routing mutation + closes the popup. There is no AI slot-guessing — the
//   player picks where the item goes.
let popupEl = null;
let activePopupItem = null;   // the item whose popup is open (null = none)
let activePopupMode = null;   // 'main' | 'equip' (null = none)
let activePopupAnchor = null; // the anchor button (for position re-flow on swap)
let activePopupBody = null;   // the panel body (passed through to handlers)
// Opening-tick guard. openActionPopup stamps the current time when it creates
// a popup; closeActionPopup is a no-op if called within OPEN_GUARD_MS of that
// stamp. This prevents re-entrant teardown: paintPage (line ~223) + clearInventoryPanel
// (line ~211) both call closeActionPopup, and an async panel re-render (e.g. a
// gem re-select firing renderInventoryPanel, or a refreshAll-driven state change)
// can land in the SAME tick as the click that opened the popup — destroying it
// before the frame paints. The guard lets the opening click settle. Genuine
// closes (outside-click, Esc, an action button) happen on LATER ticks + a
// different call path (the action handlers call closeActionPopup directly, which
// is correct), so this guard does not block legitimate closes.
const OPEN_GUARD_MS = 60;
let popupOpenedAt = 0;

function openActionPopup(anchorBtn, itemId, bodyEl) {
  _forceCloseActionPopup(); // only one at a time — force-close any PRIOR popup
                            // (it has already survived its guard window by the
                            // time the user clicked a different item button)
  const item = currentItems.find((i) => i.id === itemId);
  if (!item) return;

  const popup = document.createElement('div');
  popup.className = 'inv-action-popup';
  popup.setAttribute('role', 'menu');
  popup.setAttribute('aria-label', item.name + ' actions');

  document.body.appendChild(popup);
  popupEl = popup;
  activePopupItem = item;
  activePopupAnchor = anchorBtn;
  anchorBtn.classList.add('is-selected'); // golden border so the player sees which item the popup acts on
  activePopupBody = bodyEl;
  popupOpenedAt = Date.now(); // arm the opening-tick guard

  // Render the MAIN view first.
  renderPopupActions(popup, item, bodyEl);
  positionPopup(popup, anchorBtn);

  // Close-on-outside + Esc. Delegated on document; cleaned up on close.
  const onDocClick = (e) => {
    if (!popupEl) return;
    if (e.target === popupEl || popupEl.contains(e.target)) return;
    if (e.target === anchorBtn || anchorBtn.contains(e.target)) return; // toggle handled by anchor re-click
    _forceCloseActionPopup(); // genuine outside click — user intent
  };
  const onKey = (e) => { if (e.key === 'Escape') _forceCloseActionPopup(); };
  // (2026-08-15 audit fix) Deferred-bind cancel flag: the outside-click/Esc
  // listeners are added inside setTimeout(...,0), but popup._cleanup can run
  // BEFORE that timeout fires (e.g. an action click force-closes in the same
  // tick) — a deferred add then attaches listeners to document forever (the
  // next _cleanup never runs; _forceCloseActionPopup already nulled popupEl).
  // The flag makes the deferred add a no-op once cleanup ran.
  let deferredBindCancelled = false;
  popup._cleanup = () => {
    deferredBindCancelled = true;
    document.removeEventListener('click', onDocClick, true);
    document.removeEventListener('keydown', onKey);
  };
  // Defer binding the outside-click so the opening click doesn't immediately close.
  setTimeout(() => {
    if (deferredBindCancelled) return;
    document.addEventListener('click', onDocClick, true);
    document.addEventListener('keydown', onKey);
  }, 0);
}

// Re-trigger the open animation on a view swap (MAIN ↔ EQUIP) so the popup
// reads as snapping to the new content, not flashing. Toggles the .is-swapping
// class off → forces reflow → on, which restarts the CSS animation.
function triggerSwapAnim(popup) {
  if (!popup) return;
  popup.classList.remove('is-swapping');
  void popup.offsetWidth;   // force reflow so the class re-add restarts the anim
  popup.classList.add('is-swapping');
}

// Render the MAIN action view into the popup. Clears the popup body + rebuilds
// the conditional action buttons. EQUIP routes to renderEquipDestinations (the
// sub-menu) rather than mutating directly.
function renderPopupActions(popup, item, bodyEl) {
  activePopupMode = 'main';
  triggerSwapAnim(popup);
  popup.innerHTML = '';
  popup.setAttribute('aria-label', item.name + ' actions');

  const actions = [];
  if (item.tags.has('consumable')) actions.push({ key: 'consume', label: 'CONSUME' });
  if (item.tags.has('equippable')) actions.push({ key: 'equip', label: 'EQUIP' });
  if (item.tags.has('pocketable') && item.source !== 'belt') actions.push({ key: 'pocket', label: 'POCKET' });
  if (item.source !== 'pack') actions.push({ key: 'store', label: 'STORE' });
  actions.push({ key: 'discard', label: 'DISCARD' });

  for (const act of actions) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'inv-action-btn' + (act.key === 'discard' ? ' is-danger' : '');
    b.setAttribute('role', 'menuitem');
    b.textContent = act.label;
    b.addEventListener('click', (e) => {
      e.stopPropagation();
      if (act.key === 'equip') {
        // Open the EQUIP destination sub-menu (no mutation yet).
        renderEquipDestinations(popup, item, bodyEl);
      } else {
        handleAction(act.key, item, bodyEl);
      }
    });
    popup.appendChild(b);
  }
  // Re-flow position in case the action count changed the popup height.
  if (activePopupAnchor) positionPopup(popup, activePopupAnchor);
}

// Render the EQUIP destination sub-menu into the popup. Shows the five body
// zones + a ‹ BACK button. Picking a zone fires handleEquip (the slot routing
// mutation) + closes the popup. BACK returns to renderPopupActions.
//
// The destinations map 1:1 to the Soul Gem categories (the player already
// thinks in those zones), and each resolves to a typed equipment slot:
//   HEAD → head,  TOP → chest,  HAND → main_hand (or off_hand),  BOTTOM → legs,  FEET → feet
const EQUIP_DESTINATIONS = Object.freeze([
  { id: 'head',  label: 'HEAD',   slot: 'head' },
  { id: 'chest', label: 'TOP',    slot: 'chest' },
  { id: 'hand',  label: 'HAND',   slot: 'main_hand', alt: 'off_hand' },
  { id: 'legs',  label: 'BOTTOM', slot: 'legs' },
  { id: 'feet',  label: 'FEET',   slot: 'feet' },
]);

function renderEquipDestinations(popup, item, bodyEl) {
  activePopupMode = 'equip';
  triggerSwapAnim(popup);
  popup.innerHTML = '';
  popup.setAttribute('aria-label', item.name + ' — equip where?');

  const ps = (cachedSchema && cachedSchema.player_state) || {};
  const eq = (ps.equipment && typeof ps.equipment === 'object') ? ps.equipment : {};

  for (const dest of EQUIP_DESTINATIONS) {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'inv-action-btn inv-equip-dest';
    b.setAttribute('role', 'menuitem');
    b.textContent = dest.label;
    b.addEventListener('click', (e) => {
      e.stopPropagation();
      handleEquip(item, dest, bodyEl);
    });
    popup.appendChild(b);
  }

  // ‹ BACK — return to the main action view.
  const back = document.createElement('button');
  back.type = 'button';
  back.className = 'inv-action-btn inv-back-btn';
  back.setAttribute('role', 'menuitem');
  back.textContent = '‹ BACK';
  back.addEventListener('click', (e) => {
    e.stopPropagation();
    renderPopupActions(popup, item, bodyEl);
  });
  popup.appendChild(back);

  if (activePopupAnchor) positionPopup(popup, activePopupAnchor);
}

function positionPopup(popup, anchorBtn) {
  const r = anchorBtn.getBoundingClientRect();
  // Prefer to the RIGHT of the button; if it would overflow the viewport,
  // place it to the LEFT; if neither fits, below.
  popup.style.visibility = 'hidden';
  const pw = popup.offsetWidth;
  const ph = popup.offsetHeight;
  const gap = 8;
  let left = r.right + gap;
  let top = r.top;
  if (left + pw > window.innerWidth - 8) left = r.left - pw - gap;
  if (left < 8) { left = r.left; top = r.bottom + gap; }
  if (top + ph > window.innerHeight - 8) top = Math.max(8, window.innerHeight - ph - 8);
  popup.style.left = left + 'px';
  popup.style.top = top + 'px';
  popup.style.visibility = '';
}

function closeActionPopup() {
  if (!popupEl) return;
  // Opening-tick guard: if the popup was opened within OPEN_GUARD_MS, this
  // close is a re-entrant teardown from a concurrent panel re-render
  // (paintPage at line ~223 calls this fn on every render). Let the opening
  // click settle first so a popup just opened isn't destroyed before it
  // paints. Every other close path uses _forceCloseActionPopup directly
  // (outside-click, Esc, action handlers, slot-hide, prior-popup cleanup),
  // so this guard ONLY affects the paintPage re-render path.
  if (Date.now() - popupOpenedAt < OPEN_GUARD_MS) return;
  _forceCloseActionPopup();
}

// Unconditional close — bypasses the opening-tick guard. Used by the action
// handlers (CONSUME/EQUIP/etc. — the user explicitly chose an action, the
// popup SHOULD close), by hideInventorySlot (the slot is going away entirely),
// and by openActionPopup's "only one at a time" cleanup of a PRIOR popup.
function _forceCloseActionPopup() {
  if (!popupEl) return;
  if (typeof popupEl._cleanup === 'function') popupEl._cleanup();
  popupEl.remove();
  popupEl = null;
  activePopupItem = null;
  activePopupMode = null;
  if (activePopupAnchor) activePopupAnchor.classList.remove('is-selected');
  activePopupAnchor = null;
  activePopupBody = null;
  popupOpenedAt = 0;
}

// ── Action handlers ────────────────────────────────────────────────────────
// Each mutates the live WorldSchema via fable_schema_set (the user-edit trust
// path: bypasses the immutability lock, pushes the prior schema to the undo
// ring buffer, persists per-card). After a successful mutation we refetch +
// re-render so the panel reflects the new state. Best-effort: a failure is
// swallowed (the popup closes; the panel stays as-is).
//
// SCHEMA-TO-NARRATOR LOOP: every action passes a short past-tense `eventNote`
// ("equipped Iron Sword to Hand", "consumed Health Potion"). The backend
// appends it to the schema's `recent_events`, which the next narrator turn
// renders inside `<world_state>` — so the API narrator is AWARE of the
// player's UI action without the player re-typing it. The note mirrors the
// in-fiction verb so the narrator can weave it naturally into the next beat.
//
// NOTE: EQUIP does NOT come through here — the main-view EQUIP button opens the
// destination sub-menu (renderEquipDestinations); picking a zone calls
// handleEquip directly (below).
async function handleAction(action, item, bodyEl) {
  _forceCloseActionPopup(); // user explicitly chose an action — close decisively
  let mutated = false;
  let verb = '';   // the past-tense trace for recent_events (empty = no trace)
  try {
    // (P1 fix) ALWAYS refetch fresh in production — cachedSchema goes stale
    // across narrator turns (refreshAll deliberately does not re-render an
    // open panel), and fable_schema_set installs wholesale: acting on the
    // pre-turn cache silently rolled back the turn's tracker mutations.
    // The one-IPC refetch is cheap next to a whole-schema overwrite.
    const schema = isDevPreview() ? getDevSchema() : await invoke('fable_schema_get');
    cachedSchema = schema;
    const ps = schema.player_state || {};

    if (action === 'discard') {
      mutated = removeItem(ps, item);
      verb = 'discarded ' + item.name;
    } else if (action === 'consume') {
      // Consume = remove the item (a qty-1 stack or a single worn item). A
      // future stamina/buff effect can hook here; today it just removes + the
      // narrator learns the item was consumed.
      mutated = removeItem(ps, item);
      verb = 'consumed ' + item.name;
    } else if (action === 'pocket') {
      mutated = moveToStack(ps, item, 'belt');
      verb = 'pocketed ' + item.name;
    } else if (action === 'store') {
      mutated = moveToStack(ps, item, 'pack');
      verb = 'stored ' + item.name;
    }

    if (mutated) {
      // DEV PREVIEW has no backend — skip the persist (the in-memory mutation
      // on the cloned schema is enough for the re-render). Production persists
      // via fable_schema_set so the narrator learns the action next turn.
      if (!isDevPreview()) {
        await invoke('fable_schema_set', {
          schemaJson: schema,
          eventNote: verb || null,   // the recent_events trace (null = no trace)
        });
      }
      // Re-render the panel from the fresh state.
      await renderInventoryPanel(bodyEl, activeGemId);
    }
  } catch (e) {
    console.warn('[inventory-panel] action failed:', action, e);
  }
}

// ── EQUIP routing (the sub-menu destination → typed slot) ──────────────────
// Fires when the player picks a body zone in the EQUIP sub-menu. Routes the
// item into the destination's typed equipment slot (Outer layer), removing it
// from its origin first. The HAND destination defaults to main_hand; if
// main_hand's Outer is occupied it falls back to off_hand (two-handed logic is
// deferred — keep it simple: fill the empty hand). The event_note carries the
// destination label so the narrator knows WHERE the item physically resides
// ("equipped Iron Sword to Hand"). If the chosen slot's Outer is full AND it's
// not the hand-fallback case, the item replaces the Outer occupant (the prior
// occupant is pushed to the pack so nothing is lost — mirrors the legacy
// migration's preserve-existing discipline).
async function handleEquip(item, dest, bodyEl) {
  _forceCloseActionPopup(); // user picked an equip destination — close decisively
  let mutated = false;
  let verb = '';
  try {
    // (P1 fix) Same discipline as handleAction: refetch, never act on the
    // possibly-stale cache.
    const schema = isDevPreview() ? getDevSchema() : await invoke('fable_schema_get');
    cachedSchema = schema;
    const ps = schema.player_state || {};

    mutated = moveToEquipmentSlot(ps, item, dest);
    verb = 'equipped ' + item.name + ' to ' + dest.label;

    if (mutated) {
      // DEV PREVIEW has no backend — skip the persist (see handleAction).
      if (!isDevPreview()) {
        await invoke('fable_schema_set', {
          schemaJson: schema,
          eventNote: verb || null,
        });
      }
      await renderInventoryPanel(bodyEl, activeGemId);
    }
  } catch (e) {
    console.warn('[inventory-panel] equip failed:', item.name, '→', dest.label, e);
  }
}

// The item's tags as a backend-shaped array (for re-serialization on a move).
// Carries the tags forward so a moved item keeps its classification (a consumed
// potion stays a consumable in the trace; an equipped ring stays equippable).
function tagsArray(item) {
  return Array.from(item.tags);
}

// Remove an item from wherever it lives. Mutates `ps` in place; returns true
// if something was removed.
function removeItem(ps, item) {
  if (item.source === 'equipment') {
    const layers = ps.equipment && ps.equipment[item.slot];
    if (layers && layers[item.layer]) {
      // (2026-08-15 audit fix) Mirror the belt/pack P2 name-recheck: handlers
      // refetch FRESH schema before mutating, and a narrator turn whose
      // [EQUIP] bracket lands between render + click can have REPLACED the
      // worn item in this layer — deleting `layers[item.layer]` unverified
      // then vaporizes the WRONG (new) item and persists it. Only delete
      // when the resident item is still the one the player clicked; a
      // mismatch aborts the whole move (callers gate the persist on the
      // returned false, same as the belt/pack branch).
      if (String(layers[item.layer].name) !== item.name) return false;
      delete layers[item.layer];
      // If the slot is now empty, drop the key (mirrors the Rust invariant).
      if ((!layers.outer && !layers.inner) && ps.equipment) {
        delete ps.equipment[item.slot];
      }
      return true;
    }
  } else if (item.source === 'belt' || item.source === 'pack') {
    const arr = ps[item.source];
    if (Array.isArray(arr)) {
      // (P2 fix) NEVER splice by the render-time index (`pack:2`): handlers
      // refetch FRESH schema before mutating (the P1 fix), and a narrator
      // turn whose bracket lands between render + click shifts the indexes
      // — the splice then consumes/discards the WRONG item and persists it.
      // Re-resolve by NAME on the fresh array (names are stack identities —
      // same-name items merge into one stack); absent (already consumed or
      // renamed by a turn) → no-op, never a wrong-item delete.
      const idx = arr.findIndex((s) => s && s.name === item.name);
      if (idx !== -1) {
        arr.splice(idx, 1);
        return true;
      }
    }
  }
  return false;
}

// Move an item into a typed equipment slot chosen by the player (via the EQUIP
// sub-menu). `dest` is an EQUIP_DESTINATIONS entry: { id, label, slot, alt? }.
//
// Routing rules:
//   - HAND: try main_hand first; if its Outer layer is occupied, try off_hand
//     (the alt). If BOTH are occupied, replace main_hand's Outer (push the prior
//     occupant to the pack so nothing is lost).
//   - Other zones (HEAD/TOP/BOTTOM/FEET): if the slot's Outer layer is occupied,
//     push the prior occupant to the pack, then place the new item in Outer.
//   - The Inner layer is left untouched (the player can layer via a future UI).
//
// The item is removed from its origin FIRST (equipment/belt/pack), so an intra-
// equipment move (re-equipping to a different slot) clears the old slot. If the
// origin removal fails, the move aborts (return false → no persist). Tags ride
// forward on both the equipped item + any displaced occupant. Returns true if
// the schema changed.
function moveToEquipmentSlot(ps, item, dest) {
  if (!ps.equipment || typeof ps.equipment !== 'object') ps.equipment = {};
  if (!Array.isArray(ps.pack)) ps.pack = [];

  // HAND: pick the slot — main_hand if its Outer is free, else off_hand, else
  // replace main_hand. Other zones: resolve straight to dest.slot.
  let targetSlot = dest.slot;
  if (dest.alt) {
    const mainOuter = ps.equipment[dest.slot] && ps.equipment[dest.slot].outer;
    const altOuter = ps.equipment[dest.alt] && ps.equipment[dest.alt].outer;
    if (mainOuter && altOuter) {
      targetSlot = dest.slot;             // both full → replace main_hand
    } else if (mainOuter) {
      targetSlot = dest.alt;              // main full, off free → off_hand
    } else {
      targetSlot = dest.slot;             // main free → main_hand
    }
  }

  // If the item is already in the target slot's Outer layer, this is a no-op
  // move (return false so we don't persist a no-change).
  const isAlreadyThere =
    item.source === 'equipment' && item.slot === targetSlot && item.layer === 'outer';
  if (isAlreadyThere) return false;

  // Remove from origin first. The origin may have been the target slot's Outer
  // (a re-equip), in which case this clears it before we re-place.
  const removed = removeItem(ps, item);
  if (!removed) return false;

  // Re-read the target slot AFTER origin removal. Ensure the slot object exists.
  if (!ps.equipment[targetSlot] || typeof ps.equipment[targetSlot] !== 'object') {
    ps.equipment[targetSlot] = { outer: null, inner: null };
  }

  // If the target Outer is occupied, displace the prior occupant to the pack
  // so nothing is lost. (#67) The old `currentOuter.name !== item.name` skip
  // silently MERGED same-named copies: equipping a pack copy over an
  // identically-named worn item vaporized the worn copy's stats/identity.
  // They are two physical items — the worn one always rides to the pack.
  // (`isAlreadyThere` above already no-ops an exact same-slot re-equip.)
  const currentOuter = ps.equipment[targetSlot].outer;
  if (currentOuter) {
    if (!Array.isArray(ps.pack)) ps.pack = [];
    ps.pack.push({
      name: currentOuter.name,
      qty: 1,
      weight: 1.0,
      stats: currentOuter.stats || undefined,
      tags: Array.isArray(currentOuter.tags) ? currentOuter.tags : [],
    });
  }

  // Place the new item in the Outer layer. (#68-adjacent) `stats` ride
  // forward — a pack/belt copy with stats used to lose them on equip.
  ps.equipment[targetSlot].outer = {
    name: item.name,
    stats: item.stats || undefined,
    tags: tagsArray(item),
  };
  // Drop the slot if both layers ended up empty (defensive — shouldn't happen
  // here since we just set Outer, but mirrors the Rust invariant).
  return true;
}

// Move an item to belt or pack. Mutates `ps`; returns true if moved. Tags ride
// forward so the item keeps its classification at the new location. (#68)
// `stats` + the real `weight` ride forward too — the old rebuild dropped
// both, so an equipped item STOREd/POCKETed lost its stats until re-authored.
function moveToStack(ps, item, target) {
  if (!Array.isArray(ps[target])) ps[target] = [];
  const removed = removeItem(ps, item);
  if (!removed) return false;
  ps[target].push({
    name: item.name,
    qty: item.qty,
    weight: typeof item.weight === 'number' ? item.weight : 1.0,
    stats: item.stats || undefined,
    tags: tagsArray(item),
  });
  return true;
}
