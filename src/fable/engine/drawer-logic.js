// =============================================================
// DRAWER LOGIC — pure, DOM-free decisions for the per-message hover drawer.
//
// Extracted from beats.renderMessageDrawer + stage.js's delegated click
// handler so the count formula, the ‹/› disabled-state edges, and the
// "› folds into Regenerate at the last variant" decision are unit-testable
// without a DOM. The DOM layer (beats.js builds the markup, stage.js routes
// the clicks) imports + consumes these. Keep this module side-effect-free +
// dependency-free so it loads in plain Node.
// =============================================================

// The variant count for the drawer footer's `N` in `current/N`. `variants`
// INCLUDES the active variant (matches the backend's
// `Message::variant_count() == variants.len().max(1)`). The prior frontend
// code used `variants.length + 1` — an off-by-one that displayed N+1 and let
// › swipe one position past the real last variant (a no-op select_variant).
export function variantCount(variants) {
  return Array.isArray(variants) ? Math.max(1, variants.length) : 1;
}

// Compute the footer's disabled states + the › label for one beat's drawer.
//   role             : 'user' | 'assistant'
//   count, active    : the variant count + the 0-based active index
//   isLastAssistant  : true iff this beat is the trailing assistant message
//
// ‹ (swipe-prev) is enabled iff a previous variant exists.
// › (swipe-next) is enabled when a next variant exists, OR — only on the
//   trailing assistant beat — to roll a FRESH variant via reroll (Regenerate
//   is folded into ›). User beats can never reroll; a mid-history assistant
//   beat can't either (reroll targets the trailing turn).
// nextLabel reads "Regenerate" at the last variant (where › would reroll),
// "Next variant" otherwise.
export function computeDrawerState({ role, count, active, isLastAssistant }) {
  const atEnd = active >= count - 1;
  const canPrev = active > 0;
  const canNext = !atEnd || (role === 'assistant' && !!isLastAssistant);
  const nextLabel = atEnd ? 'Regenerate' : 'Next variant';
  return { atEnd, canPrev, canNext, nextLabel };
}

// Decide what a › click does: step to the next existing variant, OR — when at
// the last variant — roll a fresh one (reroll). Callers gate this on
// computeDrawerState().canNext first; this pure helper only encodes the
// swipe-vs-reroll branch that stage.js's delegated handler routes. Mirrors
// the backend contract (swipe_variant for an existing index, reroll_last_turn
// + fable_send(reroll) for a fresh generation).
export function swipeNextAction({ count, active }) {
  if (active + 1 < count) return { kind: 'swipe', variantIdx: active + 1 };
  return { kind: 'reroll' };
}

// (P2b, 2026-08-17 E4B shakedown) Mirror of the backend `edit_message`
// contract: ANY user beat is editable; an assistant beat only when it is the
// TRAILING one (the backend refuses a mid-history AI edit — "index N is not
// the trailing assistant message" — installing an older turn's schema would
// discard later turns' world state while their prose stays). The ✎ affordance
// used to open regardless: the failed save left the beat blank until a feed
// rebuild. Re-derived at CLICK TIME like canNext (#84 pattern — the stamped
// state is advisory).
export function canEditMessage({ role, isLastAssistant }) {
  return role === 'user' || !!isLastAssistant;
}
