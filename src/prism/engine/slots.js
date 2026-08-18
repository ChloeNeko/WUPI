// =============================================================
// PRISM SLOT PIPELINE — the guided tag-order model (Chloe ruling,
// 2026-08-18). PURE module: no DOM, no imports — pinned by
// tests/prism-slots.test.mjs (plain Node).
//
// WHY SLOTS: CLIP reads prompts sequentially; the first ~15-20 tokens
// carry the heaviest weight for composition, subject count, and core
// structure. A random tag dump (dungeon before 1girl) confuses primary vs
// secondary focus. The slot pipeline FORCES the expert ordering:
//
//   1. Subject & Count  (MANDATORY) — 1girl, 1boy, 2girls, no humans…
//   2. Framing & Pose   (MANDATORY) — cowboy shot / full body… + sitting /
//                                      standing / fighting stance…
//   3. Environment      (recommended) — outdoors, tavern, dungeon…
//   4. Freeform         (unlocked after 1+2) — the Danbooru tag search
//
// The compiled prompt is ALWAYS `subject, framing, …pose, …env, …free` —
// no matter which order the user clicked things in. The engine-side crowd
// gate (`solo` / `no humans`, prism.rs) still runs on the compiled string
// as the server-side backstop.
//
// Slot-2 semantics: framing is single-select (one framing at a time), pose
// is multi-select; the SLOT is satisfied when either has ≥1 pick. The
// mandatory slots can never be left EMPTY once filled (you can swap a
// choice, but only Clear All empties them) — that invariant is the whole
// point of the pipeline.
// =============================================================

/** The quick-pick chip vocabulary (danbooru canonical, space form). */
export const SLOT_SETS = {
  subject: [
    '1girl', '1boy', '2girls', '2boys', '1other', 'no humans', 'group',
    'multiple girls',
  ],
  framing: ['portrait', 'cowboy shot', 'full body', 'upper body', 'wide shot', 'close-up'],
  pose: [
    'sitting', 'standing', 'walking', 'running', 'lying', 'kneeling',
    'fighting stance', 'jumping',
  ],
  env: [
    'outdoors', 'indoors', 'tavern', 'dungeon', 'forest', 'cityscape',
    'simple background', 'night',
  ],
};

/** A fresh slot state (nothing picked). */
export function createSlots() {
  return { subject: null, framing: null, pose: [], env: [], free: [] };
}

/** Normalize one tag for slot comparison (lowercase, underscores folded). */
function tagKey(t) {
  return String(t || '').trim().toLowerCase().replace(/_/g, ' ');
}

const SUBJECT_KEYS = new Set(SLOT_SETS.subject.map(tagKey));
const FRAMING_KEYS = new Set(SLOT_SETS.framing.map(tagKey));
const POSE_KEYS = new Set(SLOT_SETS.pose.map(tagKey));
const ENV_KEYS = new Set(SLOT_SETS.env.map(tagKey));

/**
 * True when the two MANDATORY slots are satisfied (subject chosen AND at
 * least one framing-or-pose pick). This is the freeform search's unlock
 * gate + the Generate button's guard.
 */
export function slotsSatisfied(state) {
  if (!state || !state.subject) return false;
  return !!state.framing || (Array.isArray(state.pose) && state.pose.length > 0);
}

/**
 * Compile the slot state into the user prompt string (comma-joined, expert
 * order). Empty state → '' (the engine handles the bare-prefix fallback).
 */
export function compilePrompt(state) {
  const parts = [
    state.subject,
    state.framing,
    ...(state.pose || []),
    ...(state.env || []),
    ...(state.free || []),
  ];
  return parts.filter(Boolean).join(', ');
}

/**
 * Reconstruct a slot state from a stored prompt (Send to Composer / legacy
 * rows). Walks the tags in order; a known chip tag lands in its slot
 * (first-wins for the single-select slots, dedup for the multi slots),
 * everything else lands in freeform order-preserving. Round-trips
 * `compilePrompt` output exactly (compile emits grouped order → split
 * regroups); legacy interleaved rows get a best-effort grouping.
 */
export function splitIntoSlots(prompt) {
  const state = createSlots();
  const tags = String(prompt || '')
    .split(',')
    .map((t) => t.trim())
    .filter(Boolean);
  for (const tag of tags) {
    const key = tagKey(tag);
    if (SUBJECT_KEYS.has(key)) {
      if (!state.subject) state.subject = tag;
      // A SECOND DISTINCT subject tag (e.g. `1girl, 1boy`) is real prompt
      // content → freeform; an exact duplicate subject is dropped.
      else if (tagKey(state.subject) !== key) state.free.push(tag);
    } else if (FRAMING_KEYS.has(key)) {
      if (!state.framing) state.framing = tag;
      else if (tagKey(state.framing) !== key) state.free.push(tag);
    } else if (POSE_KEYS.has(key)) {
      // Exact duplicates drop; distinct pose tags stack.
      if (!state.pose.some((p) => tagKey(p) === key)) state.pose.push(tag);
    } else if (ENV_KEYS.has(key)) {
      if (!state.env.some((e) => tagKey(e) === key)) state.env.push(tag);
    } else {
      state.free.push(tag);
    }
  }
  return state;
}
