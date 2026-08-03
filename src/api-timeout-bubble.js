// =============================================================
// API TIMEOUT BUBBLE — a top-center, persistent-until-dismissed error
// overlay shown when the API hangs on a request (no first token within the
// 10s TTFT deadline). Distinct from the transient bottom toast: a timeout is
// a real failure worth interrupting for, and the Local fallback that handles
// the turn needs a visible signal so the user understands why the reply
// changed character/voice.
//
// The Rust side emits `{ type: "api_timeout", source: "local" }` from
// `chat_send`'s API branch when the stream errors with the `API_TIMEOUT`
// sentinel (see llm.rs). Both chat entry points — the Wupi home chat
// (script.js) and the Fable drawer (wupi-drawer.js) — call `showApiTimeout()`
// when their on_event handlers receive that type.
//
// The bubble is a singleton lazily appended to <body>; repeated calls while
// it's already shown are a no-op (don't stack). It self-dismisses on click or
// on the explicit `dismissApiTimeout()` call (e.g. on the next successful
// turn).
// =============================================================

let bubbleEl = null;

/**
 * Show the top-center API-timeout error bubble. Idempotent: if already shown,
 * this is a no-op (does not stack or flash).
 */
export function showApiTimeout() {
  if (bubbleEl) return; // already shown — don't stack
  const el = document.createElement('div');
  el.className = 'api-timeout-bubble';
  el.innerHTML = `
    <span class="api-timeout-bubble__icon">⚠</span>
    <span class="api-timeout-bubble__text">API connection timed out — switched to local model for this turn.</span>
    <button class="api-timeout-bubble__close" aria-label="Dismiss">×</button>
  `;
  el.addEventListener('click', dismissApiTimeout);
  document.body.appendChild(el);
  bubbleEl = el;
}

/**
 * Dismiss the bubble if present. Safe to call when not shown.
 */
export function dismissApiTimeout() {
  if (bubbleEl) {
    bubbleEl.remove();
    bubbleEl = null;
  }
}
