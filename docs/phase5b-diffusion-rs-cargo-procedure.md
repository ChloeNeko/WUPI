# Phase 5B — diffusion-rs Cargo Procedure (Chloe's build-safety gate)

> **Owner: Chloe.** Per AGENTS.md §0 + locked decision #12, the
> `diffusion-rs` dependency add is the build-safety gate — the agent makes
> source edits only, never touches `Cargo.toml` / `.cargo/config.toml`, never
> runs cargo. This doc is the precise hand-off: the exact lines to add, the
> environment, the isolated-build command, and the known-risk flags to watch.
>
> **Prerequisite:** Phase 5B slices 1–5 are DONE (source-only, build-clean
> with the `NoopImageGenerator` stub). This procedure flips
> `default_sd_backend()` from the stub to the real `DiffusionRsGenerator`
> behind a cargo feature. Until this runs, scene-image generation writes an
> empty PNG (good for verifying the wiring pipeline, not for real images).

## Why it's a gate (not just a dep add)

`diffusion-rs` (newfla/diffusion-rs v0.1.20, wrapping
leejet/stable-diffusion.cpp) compiles **its own vendored ggml + CUDA kernels
from source via CMake** — a NEW ~20-40 min one-time compile, separate from
llama's artifact. Three risks make this Chloe's call, not the agent's:

1. **A new env var.** It reads `CUDA_COMPUTE_CAP` (NOT
   `LLAMA_CMAKE_CUDA_ARCHITECTURES`). WUPI doesn't set it yet.
2. **A possible linker-flag interaction.** Its `cargo:rustc-link-lib=cublas`/
   `cudart` may interact with WUPI's `/DELAYLOAD:cublas64_13` rustflags —
   untested. If it forces a rustflags touch, it re-triggers the 30-min llama
   recompile (the §8B cost).
3. **The 20-40 min CUDA compile itself** — must be isolated so a linker-
   flag conflict can be diagnosed BEFORE it touches llama's warm cache.

## The exact procedure

### 1. `.cargo/config.toml` — add the CUDA compute-cap env var

WUPI's existing `LLAMA_CMAKE_CUDA_ARCHITECTURES = "120"` is CUDA arch 12.0.
diffusion-rs wants the SAME value under its OWN var name. Add to the `[env]`
block (the existing block in `src-tauri/.cargo/config.toml`):

```toml
# Phase 5B: diffusion-rs (stable-diffusion.cpp) reads THIS var for its CUDA
# arch target — NOT LLAMA_CMAKE_CUDA_ARCHITECTURES. Same value (12.0 = "120")
# as llama's so both kernels target the same GPU. See
# docs/phase5b-diffusion-rs-cargo-procedure.md.
CUDA_COMPUTE_CAP = "120"
```

### 2. `src-tauri/Cargo.toml` — add the dependency + point the feature at it

The `[features]` block **already exists** (declared as an empty flag so the
`#[cfg(feature = "diffusion-rs")]` impl forward-compiles). Two edits:

**(a)** Point the `diffusion-rs` feature at the optional dep (change the
empty `diffusion-rs = []` to):

```toml
diffusion-rs = ["dep:diffusion-rs"]
```

**(b)** Add the dep to `[dependencies]` (optional, behind the feature):

```toml
# Phase 5B: local Stable Diffusion (wraps leejet/stable-diffusion.cpp). OFF
# by default (see [features]); enables only with --features diffusion-rs.
# Same toolchain shape as llama-cpp-2 (Ninja + CUDA_PATH + VULKAN_SDK from
# .cargo/config.toml); vendors its OWN ggml (no symbol collision). Note it
# reads CUDA_COMPUTE_CAP, NOT LLAMA_CMAKE_CUDA_ARCHITECTURES.
diffusion-rs = { version = "0.1.20", optional = true, features = ["cuda", "vulkan"] }
```

### 3. `src-tauri/src/scene_art.rs` — DONE (2026-07-30)

The dispatch is flipped + the `DiffusionRsGenerator` impl is **filled in
with the confirmed builder API** (no more TODOs). The agent resolved the
setter names from docs.rs so the first `cargo check --features diffusion-rs`
(Step 4) should compile clean without a docs-lookup round-trip.

**What's encoded (Stage 1 = no-LoRA SDXL baseline):**
- `ModelConfigBuilder::default().model(path)` — the confirmed "Path to full
  model" setter. Held in `model_state` between turns (the weights are the
  expensive thing to parse). stable-diffusion.cpp auto-detects SD 1.5 vs
  SDXL from checkpoint headers — no version setter exists.
- `ConfigBuilder` rebuilt per turn with confirmed setters: `.prompt`,
  `.negative_prompt`, `.width`, `.height`, `.steps`, `.sampling_method`,
  `.cfg_scale`, `.output`.
- **Sampler: `SampleMethod::DPMPP2M_SAMPLE_METHOD`**, 28 steps, CFG 5.0 —
  the SDXL canonical-clean recipe (NOT SD 1.5 defaults; Euler/a artifacts on
  SDXL). Request defaults bumped to SDXL-native **1024×576** cinematic.
- State-holding corrected from the §11.58 scaffold: `ModelConfig` (weights)
  is held, `Config` (per-turn) is rebuilt. The scaffold held `Config` and
  rebuilt `ModelConfig::default()` — inverted from `gen_img`'s intent.

**The crate DOES expose custom-path LoRA** (the original doc's "TODO — may
not support LoRA" was wrong): `diffusion_rs::api::LoraSpec` + `LoraModeType`
+ the `modifier` module. Stage 2 (acceleration LoRA) uses these. Stage 1
ships without LoRA on purpose (de-risk the engine first).

**Stage 2 hook (not yet built):** `api::LoraSpec` is the custom local-path
LoRA loader. Add a `.loras(Vec<LoraSpec>)`-style setter to the request when
wiring LCM/Hyper-SD. Swap `SampleMethod` to `LCM_SAMPLE_METHOD` for LCM-LoRA.
The `modifier` module's built-in helpers (`lcm_lora_sdxl_base_1_0` etc.)
auto-download — do NOT use them (same offline-app reasoning as PresetBuilder).

### 4. The isolated build (keep llama's cache warm)

Build ONLY the feature first, watch for the linker-flag conflict + the CUDA
compile, BEFORE a full `tauri dev`:

```bash
# From src-tauri/ — feature-scoped so llama's artifact stays cached.
# This is the ~20-40 min one-time SD kernel compile. Walk away.
cargo check --features diffusion-rs
```

**What to watch for in the output:**
- A `rustflags` change warning → if the build says it's rebuilding `wupi_lib`
  from scratch, the SD dep forced a rustflags touch (risk #2) — STOP and
  diagnose before it burns the 30-min llama recompile too.
- `LNK1194` / `/DELAYLOAD` errors on `cublas64_13` → the interaction risk #2
  materialized. The fix is likely adding SD's libs to the same DELAYLOAD list
  or letting SD link cublas statically (it vendors its own ggml, so its
  symbols are private — but the CUDA runtime libs are shared).
- `CUDA_COMPUTE_CAP` unset warnings → step 1 didn't take.

### 5. End-to-end smoke (after `cargo check` succeeds)

```bash
# Full dev build with the feature on.
npm run tauri dev -- --features diffusion-rs
```

Then drop an SD checkpoint into `models/sd/` and flip the opt-in via the
IPC (the agent added `fable_set_sd_autogen`):

**Model choice (Stage 1 baseline):** target an **SDXL** checkpoint from the
Illustrious-XL family. Recommended, ranked:
1. **NoobAI-XL** — strong color/lighting base (great for atmospheric VN
   backgrounds), full NSFW support. ~6.5 GB fp16 / ~3.5 GB Q8 GGUF.
2. **WAI-NSFW-Illustrious-SDXL** — the community NSFW workhorse, equally
   capable, slightly more character-focused. Free — grab both to A/B.
3. **Animagine XL V4** — skip for now; safety-trained, NSFW unreliable.

`pick_sd_checkpoint` already accepts both `.safetensors` and `.gguf` — a Q8
GGUF SDXL (~3.5 GB) is the 12 GB VRAM-saver if fp16 OOMs. fp16 fits but
don't run other GPU-heavy apps during a swap.

```js
// From the devtools console once the app is up:
await window.__TAURI__.core.invoke('fable_set_sd_autogen', { enabled: true });
await window.__TAURI__.core.invoke('fable_sd_autogen_state');  // sanity check
```

Run a narrator turn. The done-beat spawn fires the LLM⇄SD⇄LLM cycle; on
success the `fable-scene-image` event swaps the PNG into the
`.fable-stage-bg` backdrop via the asset protocol.

## Known open questions (diagnose during step 4)

- **VRAM budget on 12 GB.** The §11.50 hardstop found the 12B Q6_K + ctx 4096
  is VRAM-bound (raising ctx to 8192 overran). The swap cycle sidesteps this
  (LLM + SD never co-reside), but the SDXL model's own footprint + the reload
  may still stress 12 GB. SDXL fp16 (~7 GB weights + ~2-3 GB activations)
  fits but is tight; a Q8 GGUF SDXL (~3.5 GB) is the escape hatch. If SD
  OOMs, the one-strike latch fires + auto-gen latches off — the game
  continues (narrator unaffected). The `modifier` module's `vae_tiling` +
  `offload_params_to_cpu` are the desperate-case fallbacks.
- **SDXL black-image risk.** stable-diffusion.cpp has documented historical
  issues with [SDXL producing black images](https://github.com/leejet/stable-
  diffusion.cpp/issues/167) + [LoRAs producing black images](https://github.
  com/leejet/stable-diffusion.cpp/issues/242). The crate ships
  `modifier::sdxl_vae_fp16_fix` ("to avoid black images with xl models") as
  the known fix — but it auto-downloads, so for a portable app the USER
  supplies the fp16-fix VAE file path. If Stage 1 produces black images,
  this VAE is the first thing to add (Stage 2 hook in scene_art.rs).
- **The roleplay-stall tradeoff (two halves).** (1) Generation time: SDXL at
  28 steps ≈ 15-40 s on 12 GB; a Stage 2 acceleration LoRA at 4-8 steps cuts
  this to ~3-8 s. (2) Swap-cycle overhead: the LLM-unload → SD-load →
  generate → LLM-reload is a FIXED ~12-18 s tax per image regardless of step
  count. At 4 steps you spend more time loading than generating — the swap
  design is correct for 12 GB but inherently stop-and-start. This is why
  Stage 2 (acceleration LoRA) is worth doing, but won't eliminate the stall.
- **Acceleration LoRA reliability in cpp.** The whole Lightning/Hyper-SD
  ecosystem is built around ComfyUI/A1111/diffusers. Nobody has verified
  Lightning/Hyper-SD layered on NoobAI-XL through stable-diffusion.cpp
  specifically. LCM-LoRA is the safest Stage 2 first attempt (the crate
  ships a built-in helper for it, signaling maintainer testing). Hyper-SD's
  8-step LoRA generally beats Lightning's on quality (community consensus).
- **The dest filename.** Slices 1-5 overwrite a single shared
  `apps/fable/backgrounds/scene.png` each turn (the locked decision). If you
  want per-card scene history later, that's a `scene_background_path` change.

---

## §11.59 update (2026-07-31): CRT-linkage patch + multi-file SDXL layout

Two fixes landed after the §11.58/§11.59 diagnosis was proven WRONG. The
"GGML_ABORT inside gen_img" theory was never verified (stderr buffering
destroyed the message); a VEH + SD-log-callback repro harness
(`src-tauri/examples/sd_repro.rs`) revealed the real bugs.

### Fix A — the debug-CRT popup (the `lowio/read.cpp:381` dialog) — REVERTED

**Symptom:** in debug builds (`cargo run` / `tauri dev`), sd.cpp's model
load trips the UCRT debug assertion `_osfile(fh) & FOPEN` at
`lowio/read.cpp:381`, showing a "Debug Assertion Failed" popup. Release
builds are unaffected (clean errors, no popup).

**Investigation + revert.** An earlier attempt patched `diffusion-rs-sys`'s
`build.rs` to drive the C++ profile from `cfg!(debug_assertions)` (Debug
host → Debug C++). It **failed at link with LNK2038**: llama-cpp-sys ALSO
always compiles Release (`/MD`, `_ITERATOR_DEBUG_LEVEL=0`) even in debug
builds, and MSVC's image-wide `LNK2038` requires every C++ object to agree
on `RuntimeLibrary` + `_ITERATOR_DEBUG_LEVEL`. Making sd Debug (`/MDd`,
IDL=2) while llama is Release (`/MD`, IDL=0) → hard link failure. The
vendor + `[patch.crates-io]` were removed; sd stays Release like llama.

**Status: NOT FIXED, but non-blocking.** The popup is debug-only. To test
image generation, use a **Release** build (`cargo run --release --example
sd_repro` or `npm run build`), which has no popup and surfaces the real
model-loading errors cleanly. A proper fix would need BOTH sd AND llama to
switch profiles together — a much larger change (llama's build is owned by
llama-cpp-sys-2). The real model-load bug (Fix B) was the actual blocker;
Fix A is a dev-experience wart only.

### Fix B — the model-load failure (NoobAI-XL Q8 GGUF)

**Root cause:** a ComfyUI-convention Q8 GGUF stores ONLY the UNet under
bare `input_blocks.*` names (no `model.diffusion_model.` prefix, no
embedded CLIP/VAE). sd.cpp's version detection
(`model_loader.cpp:503-510`) requires BOTH `model.diffusion_model.input_blocks.*`
(→ `is_unet`) AND `conditioner.embedders.1` / `cond_stage_model.1`
(→ `has_multiple_encoders`) to classify a model as SDXL. Passing the GGUF
as the single `model` field → tensors stay bare → detection fails with
`get sd version from file failed`. (The original fp16 safetensors had a
DIFFERENT bug — a tensor name-mapping gap on `conditioner.embedders.0.*`
that's moot now that we've switched to GGUF.)

**Fix:** `scene_art.rs::DiffusionRsGenerator` now detects a GGUF UNet at
`load` time (`is_gguf_unet`), resolves sibling CLIP-L / CLIP-G / VAE files
in the same `models/sd/` dir (`resolve_multi_file_layout`), and in
`generate` routes to `diffusion_model` + `clip_l` + `clip_g` + `vae`
setters instead of the single `model` setter. The GGUF is already Q8, so
NO `weight_type` override (the §11.58 OOM fix #3 is bypassed for GGUF — it
only applies to the legacy single-file fp16 path, preserved).

### The build (Chloe runs this from the terminal)

Vendored `diffusion-rs-sys` at `src-tauri/vendor/diffusion-rs-sys/` via
`[patch.crates-io]` in `Cargo.toml`. The vendor carries TWO C++ patches to
`stable-diffusion.cpp/src/model_loader.cpp` (Bug B + Bug C below); `build.rs`
is UNCHANGED (still Release — matches llama-cpp-sys, so no LNK2038). Each
C++ source edit invalidates the cached sd build → the ~20-40 min CUDA
recompile fires. Run:

```
cd src-tauri
cargo build --features diffusion-rs        # or: npm run sd:dev
```

If a `/DELAYLOAD` linker conflict appears, it's the §11.14 risk — isolate
before touching llama's cache.

### Bug B patch — GGUF architecture detection (in the vendor)

`init_from_gguf_file` reads `general.architecture` metadata (="sdxl") and
presets `version_` BEFORE the tensor-name scan; `get_sd_version()` early-
returns on a preset `version_`. This breaks the circular dependency
described in Fix B (is_unet ← get_sd_version ← needs CLIP that loads after
the check). Verified live: prints `gguf general.architecture = 'sdxl'` then
`Version: SDXL`.

### Bug C patch — ComfyUI orig_shape recovery (in the vendor)

`init_from_gguf_file` reads `comfy.gguf.orig_shape.<tensor>` int32 arrays
and overwrites each tensor's `ne` (reversed: PyTorch [out,in,kH,kW] → ggml
ne[0..3]=[kW,kH,in,out]) BEFORE the prefix is applied. Without this, every
conv/linear weight fails shape validation (`got [256,45,1,1], expected
[3,3,4,256]`) because ComfyUI GGUFs store quantization-reshaped dims on
the tensor + the true shape only in metadata (sd.cpp 0.1.20 doesn't read
it; ComfyUI/A1111 do). Logs `gguf: recovered N original tensor shapes`
when active.

### The CLIP/VAE files the GGUF needs (Fix B runtime requirement)

Drop these into `src-tauri/models/sd/` alongside `image.gguf`. The resolver
auto-detects by name (see `resolve_multi_file_layout`):

| File                    | Purpose            | Canonical source |
|-------------------------|--------------------|------------------|
| `clip_g.safetensors`    | SDXL CLIP-G (ViT-bigG) — **REQUIRED** for version detection | `laion/CLIP-ViT-bigG-14-laion2B-39B-b160k` (open_clip) or the SDXL `text_encoder_2` |
| `clip_l.safetensors`    | SDXL CLIP-L (ViT-L) — recommended | `openai/clip-vit-large-patch14` or SDXL `text_encoder` |
| `sdxl_vae_fp16_fix.safetensors` | fp16-fix VAE — recommended for NoobAI (avoids black images) | `madebyollin/sdxl-vae-fp16-fix` |

CLIP-G is the load-bearing one: without it, `get_sd_version` can't see the
`conditioner.embedders.1` markers and SDXL detection fails. The harness
logs a warning at load if it's missing.

### The repro harness (`src-tauri/examples/sd_repro.rs`)

Faithfully replicates `DiffusionRsGenerator::generate` as a standalone CLI
(no Tauri/IPC/frontend). Install an SD-log callback (so stable-diffusion.cpp
logs are visible) + a Windows VEH (so the faulting MODULE prints on crash).
Run from `src-tauri/`:

```
cargo run --example sd_repro --features diffusion-rs -- \
    --diffusion-model models/sd/image.gguf \
    --clip-l models/sd/clip_l.safetensors \
    --clip-g models/sd/clip_g.safetensors \
    --vae models/sd/sdxl_vae_fp16_fix.safetensors \
    --out /tmp/sd.png
```

The last lines printed before a crash name the failing phase; the VEH
block names the faulting module (`ucrtbased.dll` vs `ucrtbase.dll` vs a
CUDA driver DLL). Use release for the cleanest signal (the debug UCRT
assertion is gone after Fix A, but release avoids the popup entirely).
