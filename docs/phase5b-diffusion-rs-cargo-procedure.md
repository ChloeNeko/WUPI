# Phase 5B — diffusion-rs / stable-diffusion.cpp build procedure

The SD feature (`--features diffusion-rs`, PRISM image generation) wraps
leejet/stable-diffusion.cpp via newfla/diffusion-rs. This doc is the
maintenance procedure the code references point at. **All builds are
Chloe-run** per the build-safety rule (ZCode memory
`critical_build-safety-no-target-touching.md`) — the agent never runs
cargo/npm/tauri or touches `target/`.

## Scripts

- `npm run sd:check` — `cargo check --lib --features diffusion-rs`. The
  compile smoke test; exercises build scripts (so it pays the one-time CUDA
  compile too, not just the Rust check).
- `npm run sd:dev` — `tauri dev -f diffusion-rs`, stdout+stderr redirected
  to `logs/sd-out.log`. The full dev app with the real backend.
- `src-tauri/src/bin/sd_repro.rs` — standalone harness (built only under
  the feature): `sd_repro <checkpoint> [dest.png]`. No Tauri, no LLM, no
  swap — the isolation tool for checkpoint load/generate failures. On a
  ggml abort, read `<exe_dir>/logs/sd-abort.txt` BEFORE pulling any lever
  (Q8_0 / VAE / v-pred probe).

## One-time (and re-triggered) CUDA compiles

First feature build compiles the SD stack (~20-40 min). It re-fires when:

- `target/` is wiped, or
- `src-tauri/vendor/diffusion-rs-sys/build.rs` changes (fingerprint), or
- any profile/config change re-fingerprints build scripts.

The 2026-08-16 llama-cpp-sys-2 vendoring (dual-CRT fix) re-fingerprints
**only** the llama build script → one full llama CUDA rebuild on the next
build; the SD stack is untouched by it.

## Fresh clone / new machine

`src-tauri/vendor/` is gitignored. Restore the vendored crates before
building: **`llama-cpp-sys-2` is patched via `[patch.crates-io]` and is a
dependency of EVERY build** (llama-cpp-2 always links it — the patch entry
is unconditional, not feature-gated), so even plain `npm run dev`/release
builds need it present; `diffusion-rs-sys` is only needed for
`--features diffusion-rs`:

```bash
bash scripts/vendor-restore.sh    # copies from ~/.cargo registry + applies the tracked patches
```

See `src-tauri/vendor-patches/README.md` for what each patch does.

## Known watch items

- **`/DELAYLOAD:cublas64_13` linker interaction** — flagged at bring-up
  (2026-07-30). If the link suddenly fails after a toolchain/SDK change,
  check the delay-load config in `.cargo/config.toml` first.
- **Stale `diffusion-rs-sys` build dirs** — after any shared/static lib
  flip in its build.rs, the old static `.lib`s shadow the import libs on
  the recursive link-search path; the flip procedure wipes
  `target/<profile>/build/diffusion-rs-sys-*` (Chloe's call — target/
  operations are never agent-run).
- **Dual ggml** — wupi.exe links llama's static ggml AND sd.cpp's ggml;
  the DLL-shared-build patch (`build.rs-shared-libs.patch`) is what keeps
  them from symbol-deduping into each other. Never build the SD stack
  static on MSVC.

## Status (2026-08-16)

- First render achieved 2026-08-15 end-to-end to the PNG (multi-file GGUF
  layout, defaults) — then the dual-CRT UCRT assert killed the process
  post-PNG (gallery row/event/LLM reload never ran).
- Dual-CRT fix landed 2026-08-16 via the vendored llama-cpp-sys-2 patch
  (`build.rs-no-msvcrtd.patch`). **Pending re-verification on the next
  `sd:dev` run**: PNG lands, gallery row commits, `prism-gen-done` fires,
  chat still answers after (LLM reload), one-strike latch stays clear.
- Next fix target after that: the single-file `image.safetensors` sd.cpp
  load (use `sd_repro` to capture the real error first).
