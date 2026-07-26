// =============================================================
// SCREEN: LOADING — the engine-spawn wait screen.
// Shown briefly between card-select and stage-enter while the
// backend spins up the game engine (if needed) + loads save state.
// =============================================================

export function buildLoading() {
  const root = document.createElement('section');
  root.className = 'fable-screen fable-loading-screen';
  root.dataset.fableScreen = 'loading';
  root.hidden = true;
  root.innerHTML = `
    <div class="fable-loading-spinner"></div>
    <p class="fable-loading-text" data-text>Entering the world…</p>
  `;
  return root;
}

export function setLoadingText(root, text) {
  const el = root && root.querySelector('[data-text]');
  if (el) el.textContent = text;
}
