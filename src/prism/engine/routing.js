// =============================================================
// PRISM GEN-DONE ROUTING — the pure render-origin matcher.
//
// (2026-08-20 audit M8) A `prism-gen-done` completion must route to the
// screen that ORIGINATED the render, never to whichever screen happens to
// be active when the (multi-second) SD swap finishes — a composer render
// landing while the user sits on Fork used to swap Fork's B layer, and a
// fork render finishing anywhere else left `.is-rendering` stuck.
//
// prism.js tags every generate call AT RENDER START with `{ origin, path }`
// (origin = the calling screen; path = the dest path prism_generate
// returns, which the done event echoes back as `payload.image.path` on
// success / `payload.path` on the insert-failure branch). This module owns
// ONLY the pure pairing decision so the tests can pin it — no DOM, no
// Tauri, importable from plain Node.
// =============================================================

/**
 * The path a done payload carries (null when it carries none — the usual
 * failure shape: Skipped/Failed/Cancelled all omit it).
 */
function donePayloadPath(payload) {
  if (!payload) return null;
  if (payload.ok && payload.image && payload.image.path) return String(payload.image.path);
  if (payload.path) return String(payload.path);
  return null;
}

/**
 * Pair a `prism-gen-done` payload with the render that started it.
 * @param {Array<{origin: string, path: string|null}>} renders — the
 *   outstanding renders in START order. `path` is null from render start
 *   until the generate invoke resolves (the attach races the fastest stub
 *   done). The SD turn lock serializes swap cycles server-side, so
 *   completions fire in start order — the FIFO fallback's premise.
 * @param {object} payload — the gen-done payload ({ok, image?} on success;
 *   {ok: false, error, path?, cancelled?} on failure).
 * @returns {{origin: string, index: number}|null} the render the payload
 *   belongs to (index into `renders`), or null when no outstanding render
 *   claims it (unknown/stale token — the caller must ignore the event).
 */
export function resolveGenDoneTarget(renders, payload) {
  if (!Array.isArray(renders) || renders.length === 0) return null;
  const path = donePayloadPath(payload);
  if (path) {
    const idx = renders.findIndex((r) => r.path === path);
    if (idx >= 0) return { origin: renders[idx].origin, index: idx };
    // A path-bearing payload matching no tag is STALE only once some render
    // is path-tagged. While every tag is still path-null the invoke's
    // path-attach simply hasn't landed yet — fall through to start-order
    // FIFO instead of dropping a legitimate completion.
    if (renders.some((r) => r.path)) return null;
  }
  // No path to match (the typical failure shape) — start-order FIFO head.
  return { origin: renders[0].origin, index: 0 };
}
