// Fast TEST build for WUPI — `npm run build:exe`.
//
// PURPOSE: rebuild src-tauri/target/release/{wupi.exe,fable.exe} for local
// testing WITHOUT the release ceremony (no version bump, no git, no updater
// crate, no staging, no zip, no upload) — and, critically, WITHOUT
// re-fingerprinting the CUDA cores (llama-cpp-sys-2 + diffusion-rs-sys).
//
// WHY a bare `npx tauri build` is NOT safe: cargo's unit fingerprints cover
// the feature set, the env vars the vendored build scripts declare
// (`cargo:rerun-if-env-changed`), RUSTFLAGS, and env vars read via
// `option_env!`. `npm run release` (scripts/release.cjs Step 3) invokes:
//
//     npx tauri build --features diffusion-rs
//     env: HF_TOKEN=<resolved from keys/hf.key>, CMAKE_BUILD_PARALLEL_LEVEL=8
//     cwd: repo root
//
// A bare `npx tauri build` diverges from that blessed invocation in every
// fingerprint-relevant input:
//   1. it omits `--features diffusion-rs` → different unit key for the wupi
//      crate, AND the exe compiles the NoopImageGenerator stub (PRISM
//      silently no-ops — wrong exe for testing even when it builds fast);
//   2. it omits HF_TOKEN (baked in at compile time via `option_env!` in
//      model_downloader.rs) → the wupi crate reruns;
//   3. it omits CMAKE_BUILD_PARALLEL_LEVEL → divergent build-script env.
// Flipping between unit-key sets is what triggers the ~20-40 min CUDA
// recompiles. This script pins all three inputs to the exact release values,
// so it hits the SAME warm units `npm run release` does.
//
// SYNC RULE: if scripts/release.cjs Step 3 / 3.6 / 3.7 ever changes (flags,
// env, cwd), THIS FILE MUST CHANGE WITH IT. Do not "simplify" either side.
//
// (Per AGENTS.md §0: the agent never RUNS npm/npx/cargo/tauri — Chloe
// executes this herself.)

const { readFileSync, existsSync } = require('fs');
const { join, basename } = require('path');
const { spawnSync } = require('child_process');
const { homedir } = require('os');

// Same shell-safe single-string command shape release.cjs uses (DEP0190).
const shCommand = (cmd, args) => {
  const quote = (a) => {
    const s = String(a);
    return /[\s"^&|<>]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  return [cmd, ...args].map(quote).join(' ');
};

const repoRoot = join(__dirname, '..');
const argv = process.argv.slice(2);

// ──────────────────────────────────────────────────────────────────────────
// HF_TOKEN gate — verbatim mirror of release.cjs Step 2.5 (same discovery
// order: keys/hf.key → process.env → ~/.bashrc). The token is baked into the
// binary at COMPILE time; building without it re-fingerprints the wupi crate
// and mints an exe that 403s on first-run GGUF download.
// ──────────────────────────────────────────────────────────────────────────
const findHfToken = () => {
  // 1. keys/hf.key (preferred — keeps all secrets in one gitignored dir)
  const keyFilePath = join(repoRoot, 'keys', 'hf.key');
  if (existsSync(keyFilePath)) {
    const raw = readFileSync(keyFilePath, 'utf8').trim();
    // Accept either a bare token or `export HF_TOKEN=hf_…` (legacy)
    const m = raw.match(/(hf_[A-Za-z0-9]+)/);
    if (m) return m[1];
  }
  // 2. process.env.HF_TOKEN (explicit shell export)
  if (process.env.HF_TOKEN) return process.env.HF_TOKEN;
  // 3. ~/.bashrc fallback (legacy)
  const bashrcPath = join(homedir(), '.bashrc');
  if (existsSync(bashrcPath)) {
    const bashrc = readFileSync(bashrcPath, 'utf8');
    const m = bashrc.match(/^\s*export\s+HF_TOKEN\s*=\s*(hf_[A-Za-z0-9]+)/m);
    if (m) return m[1];
  }
  return null;
};
const hfToken = findHfToken();
if (hfToken) {
  process.env.HF_TOKEN = hfToken;  // re-export so the childEnv spread sees it
  console.log(`[build:exe] HF_TOKEN resolved (len=${hfToken.length}, prefix=${hfToken.slice(0, 7)}…).`);
} else if (argv.includes('--allow-missing-hf-token')) {
  console.warn('[build:exe] !! HF_TOKEN not found and --allow-missing-hf-token passed.');
  console.warn('               The compiled binary will have HF_TOKEN="" — fresh installs will');
  console.warn('               403 on the first-run GGUF download.');
} else {
  console.error('[build:exe] !! HF_TOKEN not found in keys/hf.key, env, or ~/.bashrc.');
  console.error('               Override (NOT recommended): npm run build:exe -- --allow-missing-hf-token');
  process.exit(1);
}

// ──────────────────────────────────────────────────────────────────────────
// Step 3 mirror: `npx tauri build --features diffusion-rs` with the same
// childEnv release.cjs builds. Same flags, same env, same cwd → same unit
// fingerprints → the CUDA sys crates stay warm.
// ──────────────────────────────────────────────────────────────────────────
const childEnv = {
  ...process.env,
  HF_TOKEN: process.env.HF_TOKEN || '',
  CMAKE_BUILD_PARALLEL_LEVEL: process.env.CMAKE_BUILD_PARALLEL_LEVEL || '8',
};

// CARGO_TARGET_DIR passthrough — mirror of release.cjs: follow wherever cargo
// actually puts the exe if the redirect is set, else src-tauri/target.
const cargoTargetDir = process.env.CARGO_TARGET_DIR
  ? join(process.env.CARGO_TARGET_DIR)
  : join(repoRoot, 'src-tauri', 'target');

console.log('[build:exe] running: npx tauri build --features diffusion-rs');
const buildResult = spawnSync(shCommand('npx', [
  'tauri', 'build',
  '--features', 'diffusion-rs',
]), {
  env: childEnv,
  stdio: 'inherit',
  shell: true,   // npx is a .cmd on Windows; shell:true to invoke
  cwd: repoRoot, // same effective cwd release.cjs's Step 3 spawn inherits
});
if (buildResult.status !== 0) {
  console.error(`[build:exe] tauri build failed (exit ${buildResult.status}).`);
  process.exit(buildResult.status ?? 1);
}
console.log('[build:exe] build complete.');

// ──────────────────────────────────────────────────────────────────────────
// Step 3.6 mirror: fable.exe — same feature set as the tauri build (a
// fable.exe built without tauri/custom-protocol embeds an EMPTY asset set —
// the v0.19.0–v0.19.2 "localhost refused to connect" break). Cheap: the main
// build already compiled the lib + every heavy dep.
// ──────────────────────────────────────────────────────────────────────────
const builtFableExe = join(cargoTargetDir, 'release', 'fable.exe');
console.log('[build:exe] building fable.exe (src/bin/fable.rs)…');
const fableBuild = spawnSync(shCommand('cargo', [
  'build', '--release', '--bin', 'fable',
  '--features', 'tauri/custom-protocol,diffusion-rs',
]), { stdio: 'inherit', cwd: join(repoRoot, 'src-tauri'), shell: true });
if (fableBuild.status !== 0) {
  console.error(`[build:exe] fable build failed (exit ${fableBuild.status}).`);
  process.exit(fableBuild.status ?? 1);
}

// ──────────────────────────────────────────────────────────────────────────
// Step 3.7 mirror: verify BOTH exes embed the current frontend (generate_
// context! bakes dist/ in at compile time; a stale/empty embed opens to
// "localhost refused to connect"). Cheap static check, same as release.
// ──────────────────────────────────────────────────────────────────────────
const distHtml = readFileSync(join(repoRoot, 'dist', 'wupi.html'), 'utf8');
const jsAsset = distHtml.match(/assets\/wupi-[A-Za-z0-9_-]+\.js/);
if (!jsAsset) {
  console.error('[build:exe] !! could not find the hashed JS bundle reference in dist/wupi.html.');
  console.error('               Expected something like assets/wupi-<hash>.js — did the Vite build change shape?');
  process.exit(1);
}
const marker = Buffer.from(jsAsset[0]);
const wupiExePath = join(cargoTargetDir, 'release', 'wupi.exe');
for (const exe of [wupiExePath, builtFableExe]) {
  if (!existsSync(exe)) {
    console.error(`[build:exe] !! ${exe} missing despite a successful build.`);
    process.exit(1);
  }
  const buf = readFileSync(exe);
  if (buf.indexOf(marker) === -1) {
    console.error(`[build:exe] !! ${basename(exe)} does NOT embed the current frontend (no "${jsAsset[0]}" in the binary).`);
    process.exit(1);
  }
  console.log(`[build:exe] ${basename(exe)} embeds the current frontend (${jsAsset[0]} found).`);
}

console.log('');
console.log('[build:exe] done — TEST build only (no version bump, git, updater, staging, or zip).');
console.log(`[build:exe]   ${wupiExePath}`);
console.log(`[build:exe]   ${builtFableExe}`);
console.log('[build:exe] For the shippable portable zip, run `npm run release` as usual.');
