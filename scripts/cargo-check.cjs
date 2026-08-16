// Quick compile-verification for the Rust core: runs `cargo check --release
// --lib` against the warm RELEASE profile (minutes, NOT the 30+ min dev-
// profile CUDA recompile that bare `cargo check`/`build`/`test` triggers —
// see AGENTS.md §12 + the ZCode build-safety memory).
//
// AUTO-CLOSES potentially-locking processes first, mirroring what
// `npm run release` does via `clear-webview2-cache.cjs`:
//   • cargo.exe       — the silent lock-deadlock risk: a SECOND cargo started
//                       while one holds target/ blocks quietly + looks frozen
//                       forever. Killed so a stray one can never deadlock us.
//   • wupi.exe        — locks install files + the WebView2 cache.
//   • msedgewebview2  — wupi.exe's WebView2 child; can outlive the parent and
//                       keep handles open.
// Same /F /T (force + tree) + swallow-errors pattern as clear-webview2-cache.
// Safe no-op when nothing is running. Windows-only + guarded so non-Windows
// CI is unaffected.
//
// Usage:  npm run check
//         (optionally with extra cargo flags after --, e.g. `npm run check -- --tests`)
const { spawnSync } = require('child_process');
const { join } = require('path');
const { platform } = require('os');

const repoRoot = join(__dirname, '..');
const manifest = join(repoRoot, 'src-tauri', 'Cargo.toml');

// ── Kill step (mirrors scripts/clear-webview2-cache.cjs exactly). ──
// taskkill /F = force (no "save before quit" — the user explicitly ran a
// build step), /T = take down child processes too, /IM = by image name.
// Errors (process not found, not on Windows) are swallowed: we only care
// that they're gone now, and the most common case is nothing running.
function killLockingProcesses() {
  if (platform() !== 'win32') return;
  const targets = ['cargo.exe', 'wupi.exe', 'msedgewebview2.exe'];
  for (const exe of targets) {
    try {
      spawnSync('taskkill', ['/F', '/T', '/IM', exe], { stdio: 'ignore' });
      console.log(`[cargo-check] closed running ${exe}`);
    } catch {
      // Not running (most common) or taskkill unavailable — nothing to kill.
    }
  }
}

killLockingProcesses();

// ── The check itself. --release uses the warm profile (no CUDA recompile).
// --lib checks only the wupi_lib crate (the app's code), not the thin binary
// wrapper — enough to catch compile errors in our Rust with minimal work.
// --manifest-path pins us to src-tauri/Cargo.toml regardless of cwd.
// stdio: 'inherit' streams cargo's output live so progress/errors are visible.
// shell: true because cargo may resolve to a .cmd shim on Windows.
const extraArgs = process.argv.slice(2);
const args = ['check', '--release', '--lib', '--manifest-path', manifest, ...extraArgs];
console.log(`[cargo-check] running: cargo ${args.join(' ')}`);

const result = spawnSync('cargo', args, {
  stdio: 'inherit',
  shell: true,
});

// Propagate cargo's exit code so a failed check fails the npm script
// (and any CI / chained command depending on it).
process.exit(result.status === null ? 1 : result.status);
