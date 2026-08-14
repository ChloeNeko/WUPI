import { defineConfig } from "vite";
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

// ── Dev HMR policy: CSS hot-swaps, JS does NOT auto-reload. ──────────────
// By default Vite injects an HMR client that, on any file change, tries to
// hot-update. This codebase has ZERO `import.meta.hot` acceptance boundaries,
// so JS edits fall back to a FULL PAGE RELOAD — which wipes your in-app state
// (e.g. a half-filled form) on every small JS tweak. Chloe finds
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

// ── Hitbox editor + save-to-file middleware (dev server only) ────────────
// The standalone hitbox authoring tool (src/hitbox-editor.html) is a dev-only
// page, NOT shipped. This plugin serves it at /hitbox-editor.html and exposes
// a POST /__hitbox-write endpoint that writes the editor's export directly to
// src/fable/data/paperdoll-hitboxes.json — so Chloe can iterate without the
// copy/paste/download dance. Dev server only (apply: 'serve'); `vite build`
// never invokes these hooks + the page isn't in the production input.
function hitboxEditorDevServer() {
  return {
    name: 'wupi-hitbox-editor-dev',
    apply: 'serve' as const,
    configureServer(server) {
      const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)));
      const editorPath = path.resolve(repoRoot, 'src/hitbox-editor.html');
      const dataPath = path.resolve(repoRoot, 'src/fable/data/paperdoll-hitboxes.json');
      // Serve the editor page.
      server.middlewares.use('/hitbox-editor.html', (_req, res) => {
        try {
          const html = fs.readFileSync(editorPath, 'utf-8');
          res.setHeader('Content-Type', 'text/html; charset=utf-8');
          res.end(html);
        } catch (e) {
          res.statusCode = 500;
          res.end('hitbox-editor.html not found: ' + (e as Error).message);
        }
      });
      // Save-to-file endpoint.
      server.middlewares.use('/__hitbox-write', (req, res) => {
        if (req.method !== 'POST') {
          res.statusCode = 405;
          res.setHeader('Allow', 'POST');
          res.end(JSON.stringify({ error: 'POST required' }));
          return;
        }
        const chunks: Buffer[] = [];
        req.on('data', (c) => chunks.push(c));
        req.on('end', () => {
          try {
            const body = Buffer.concat(chunks).toString('utf-8');
            // Validate it's well-formed JSON + structurally a {male,female}
            // map before touching the file (defensive — the editor is the only
            // caller, but a bad payload must never corrupt the canonical file).
            const parsed = JSON.parse(body);
            if (!parsed || typeof parsed !== 'object' || !parsed.male || !parsed.female) {
              throw new Error('payload is not a {male,female} hitbox map');
            }
            // Atomic write: temp file + rename (mirrors the Rust write_atomic
            // discipline so a crash mid-write can't leave a truncated file).
            const tmp = dataPath + '.tmp';
            fs.writeFileSync(tmp, body, 'utf-8');
            fs.renameSync(tmp, dataPath);
            res.setHeader('Content-Type', 'application/json');
            res.end(JSON.stringify({ ok: true, path: 'src/fable/data/paperdoll-hitboxes.json' }));
          } catch (e) {
            res.statusCode = 400;
            res.setHeader('Content-Type', 'application/json');
            res.end(JSON.stringify({ error: (e as Error).message }));
          }
        });
      });
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
  plugins: [suppressJsHmrReload(), hitboxEditorDevServer()],
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
