import { defineConfig } from "vite";

// ── Dev HMR policy: CSS hot-swaps, JS does NOT auto-reload. ──────────────
// By default Vite injects an HMR client that, on any file change, tries to
// hot-update. This codebase has ZERO `import.meta.hot` acceptance boundaries,
// so JS edits fall back to a FULL PAGE RELOAD — which wipes your in-app state
// (e.g. a half-filled Quick Play form) on every small JS tweak. Chloe finds
// the constant auto-refresh disruptive ("I hate it"), so this plugin changes
// the policy:
//   • CSS edits → still hot-swap silently (instant, state preserved — the
//     good kind of HMR).
//   • JS edits → the update is reported to the client as "ignored", so Vite
//     does NOT fall back to a full reload. You press Ctrl+R / F5 when YOU
//     want to see a JS change.
//
// How it works: Vite's HMR protocol lets a plugin's hotUpdate hook return
// an empty module list, signalling "this update produced nothing to apply"
// → no reload. CSS is exempted (handled by Vite's built-in CSS pipeline
// before this hook runs for .css files, and we explicitly early-return for
// .css/.html/assets so those keep their default behavior).
//
// PRODUCTION IS UNAFFECTED: this only runs in the dev server (`server`);
// `vite build` never invokes hotUpdate. Nothing about the shipped bundle
// changes. To fully restore default Vite behavior, delete this plugin.
function suppressJsHmrReload() {
  return {
    name: 'wupi-suppress-js-hmr-reload',
    apply: 'serve' as const, // dev server only — never affects `vite build`
    handleHotUpdate(ctx) {
      // Keep the good HMR: CSS hot-swaps via Vite's built-in client. Let
      // non-JS assets (html, images, fonts) through untouched too.
      const keep = /\.(css|html|htm|png|jpe?g|gif|svg|webp|ico|woff2?|ttf|otf|eot|mp3|wav|ogg|webm|mp4)$/;
      if (keep.test(ctx.file)) return; // undefined → default behavior
      // Any JS/TS module change: swallow it. Returning [] tells Vite no
      // modules were updated → no reload is triggered. The change is on
      // disk + will appear on the next manual Ctrl+R.
      ctx.server.ws.send({ type: 'custom', event: 'wupi:js-change-ignored', data: { file: ctx.file } });
      return [];
    },
  };
}

export default defineConfig({
  root: "src",
  publicDir: "../public",
  // Relative base so assets resolve correctly under Tauri's custom protocol
  // (tauri://localhost / https://tauri.localhost). An absolute "/base" 404s.
  base: "./",
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  plugins: [suppressJsHmrReload()],
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    // The entry HTML is `wupi.html` (renamed from the Vite-default
    // index.html per AGENTS.md §8C). Vite picks up the entry via
    // rollupOptions.input; without this it would look for index.html in
    // the `src` root and emit nothing. The Tauri window's `url: "wupi.html"`
    // (tauri.conf.json) loads this emitted file at runtime.
    rollupOptions: {
      input: "src/wupi.html",
    },
  },
});
