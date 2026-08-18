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

## Status (2026-08-17, evening)

- 2026-08-15: first render end-to-end to the PNG (multi-file GGUF layout,
  defaults) — then the dual-CRT UCRT assert killed the process post-PNG.
  Dual-CRT fix landed 2026-08-16 (vendored llama-cpp-sys-2
  `build.rs-no-msvcrtd.patch`); verified 2026-08-17: PNG lands, gallery row
  commits, `prism-gen-done` fires, LLM reloads, one-strike latch stays clear.
- Both 08-15 + 08-17 app renders were NOISE (saturated mush, mean RGB
  ~[190,235,231] / [250,238,188] vs a healthy ~[96,68,51]). Root cause
  eliminated 2026-08-17: that layout used the placeholder UNet-only GGUF +
  an external "valid" fp16-fix VAE, and sd.cpp applies its SDXL Conv2D 1/32
  overflow guard ONLY when no valid external VAE is present (see the
  `stable-diffusion.cpp:886` "No valid VAE specified ... using Conv2D scale
  0.031" warning — it never fired on the old path, fires now).
- **2026-08-17: FIRST CLEAN RENDER** — `sd_repro` on the new
  `models/sd/image.gguf` (the FULL NoobAI-XL-v1.1-merge checkpoint quantized
  to Q8_0, ~4.18 GB, embedded CLIP-L/G + VAE, no GGUF metadata): coherent
  apple-on-table image, `Version: SDXL`, eps-prediction, 28 steps DPM++ 2M
  (discrete schedule), seed 42, ~40 s in the debug build (~1.1 s/it).
- New machinery this cycle (§11.61, all in `scene_art.rs`):
  - `install_sd_log_bridge` — registers `sd_set_log_callback`; without it
    sd.cpp DROPS every LOG_* line (no stderr default). Engine logs now flow
    to stderr (`[sd]`-prefixed → `sd-out.log` via `sd:dev`'s 2>&1) +
    `wupi.log`. Wired in `lib.rs` boot + `sd_repro`.
  - `gguf_embeds_full_checkpoint` — a GGUF carrying embedded VAE/conditioner
    tensors routes to the SINGLE-FILE `model` path (the `diffusion_model`
    prefix pass would bury its embedded names; the model path needs no
    sibling files). UNet-only GGUFs still go multi-file.
  - Single-file path no longer applies runtime `weight_type(Q8_0)` to GGUFs
    (already quantized; re-quantizing is lossy + a multi-GB pointless pass).
  - `sd_repro` seed LOCKED at 42 — A/B determinism (one lever per run).
- Model inventory: `models/sd/` = the FOUR-FILE shipped set (2026-08-18,
  live as of v0.23.0): `image.gguf` (full NoobAI-XL-v1.1-merge Q8_0,
  embedded VAE + conditioners), `clip_l.safetensors` + `clip_g.safetensors`
  (the provenance-correct fp16 sidecars EXTRACTED from the checkpoint's own
  source — the ClipOverride path consumes these; stock SDXL encoders
  render mush, see the weight-provenance rule), and `vae.safetensors`
  (distributed for completeness; the engine deliberately does NOT consume
  an external VAE — the GGUF's embedded VAE + sd.cpp's SDXL Conv2D guard
  stay active). All four are auto-downloaded via the boot overlay
  (`model_downloader::REQUIRED_FILES`) from the private `ChloeNeko/WUPI`
  HF repo under these exact names. **They must be uploaded there BEFORE
  testers pull the v0.23.0 update** — the overlay has no skip button, and
  a missing repo file is a permanent 404 failure (fail-fast, no retry), so
  an updated install is stuck on the overlay until the files land.
- **v0.23.0 release note (published 2026-08-18) + the launch-crash
  incident:** `release.cjs` built BOTH exes with `--features diffusion-rs`
  (diffusion-rs-sys compiled, both exes embed-verified) — but the portable
  staging MISSED the SD stack's DLLs and **v0.23.0 could not launch on any
  fresh install** ("stable-diffusion.dll not found"). Root cause: the
  shared-libs build emits `stable-diffusion.dll` + `ggml{,-base,-cpu,
  -cuda,-vulkan}.dll` next to the exes, and BOTH exes STATICALLY imported
  that chain (+ `ggml-cuda.dll → cublas64_13.dll`) — resolved at process
  start, before `SetDllDirectoryW(bin)`. **v0.23.1 fix (clean-root
  ruling: the ONLY root DLL is msvcp140.dll, everything else in bin/,
  never a duplicate):** `src-tauri/.cargo/config.toml` adds
  `/DELAYLOAD:stable-diffusion.dll` (mirroring llama's cublas/cudart
  delayloads) so the whole chain resolves at the first SD symbol call,
  through `SetDllDirectoryW(bin)`; `release.cjs` stages the six SD DLLs +
  the VC++ trio (`VCRUNTIME140.dll`, `VCRUNTIME140_1.dll`,
  `MSVCP140_CODECVT_IDS.dll`) into `bin/` (cublas64_13 already lives
  there — no duplicate), and runs a **static-import guard** (PE
  import-table parse over both exes) that fails the release on ANY
  non-system static import other than msvcp140.dll — the mechanical
  backstop so this class of bug can never ship again. NOTE: the delayload
  rustflags change invalidates the cached bin links — expect a relink of
  the exes (NOT a full CUDA rebuild; the .obj fingerprint unchanged). If a
  future -sys upgrade drags a data import into the exe link it fails
  LOUDLY with LNK1194 — fix the binding, never move the DLL to the root.
  v0.23.0 installs cannot self-update (the updater runs inside the app) —
  minimal manual unstick: drop the six SD DLLs + a copy of
  `bin\cublas64_13.dll` at the install ROOT of the v0.23.0 install (its
  exe statically imports them there), then the in-app updater pulls the
  next release and the root copies can be deleted.
- Pending: ~~verify through the FULL app~~ **VERIFIED 2026-08-17 23:47** —
  PRISM generate through `sd:dev` with the single-file routing: 41.99 s,
  clean swap + gallery row + LLM reload. Backend + app integration DONE.
  Remaining work is quality tuning, not correctness: the 23:47 render
  ("1girl, classroom, sunset, masterpiece" @ CFG 5, 1024×576) produced the
  classroom but dropped the girl — prompt-recipe/CFG/aspect-ratio territory
  (NoobAI wants its quality-tag prefix, CFG 5-7, portrait buckets like
  832×1216; clip_skip is already correct — sd.cpp auto-resolves 2 for SDXL,
  conditioner.hpp:377). Scheduler still DISCRETE for DPM++ 2M; karras is the
  usual companion.
