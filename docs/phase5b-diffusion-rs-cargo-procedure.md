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

### 3. `src-tauri/src/scene_art.rs` — flip `default_sd_backend()`

The stub is already there (scene_art.rs:623) + the `DiffusionRsGenerator`
impl is already written behind `#[cfg(feature = "diffusion-rs)]` (scene_art.rs,
after the stub). Two edits needed once the dep resolves:

**(a) Flip the dispatch** — replace the stub body with the feature-gated
dispatch the comment already describes:

```rust
pub fn default_sd_backend() -> Box<dyn SceneImageGenerator> {
    #[cfg(feature = "diffusion-rs")]
    { Box::new(DiffusionRsGenerator::new()) }
    #[cfg(not(feature = "diffusion-rs"))]
    { Box::new(NoopImageGenerator) }
}
```

**(b) Fill in the `DiffusionRsGenerator` TODOs.** The impl structure is done
(load/generate/unload lifecycle, interior-mutable state for the `&self`
trait); only the exact builder field setters are TODO. The confirmed API
shape (from the newfla/diffusion-rs README + docs.rs/api):

```rust
use diffusion_rs::{api::gen_img, preset::{Preset, PresetBuilder}};
// README's preset example (auto-downloads — DON'T use for WUPI):
let (config, mut model_config) = PresetBuilder::default()
    .preset(Preset::SDXLTurbo1_0)
    .prompt("...").build().unwrap();
gen_img(&config, &mut model_config).unwrap();
```

WUPI must use the **custom-path** path (NOT `PresetBuilder` — presets
auto-download, wrong for a portable offline app). Build `ModelConfig` from
the user's `models/sd/*.safetensors` checkpoint via `ModelConfigBuilder` +
the top-level `Config` via `ConfigBuilder`. After `cargo doc --features
diffusion-rs`, the exact setter names are on:
- `docs.rs/diffusion_rs/latest/diffusion_rs/api/struct.ConfigBuilder.html`
- `docs.rs/diffusion_rs/latest/diffusion_rs/api/struct.ModelConfigBuilder.html`

Replace the `todo!()`-adjacent `.model_path(...)` / `.width(...)` /
`.sample_steps(...)` placeholder calls in the impl with the confirmed
setters. The `gen_img(&Config, &mut ModelConfig)` signature + the `Config`/
`ModelConfig` types are confirmed; only the builder field names need the
docs page.

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

Then drop an SD checkpoint into `models/sd/` (e.g. an SD 1.5 `.safetensors`),
flip the opt-in via the IPC (the agent added `fable_set_sd_autogen`):

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
  (LLM + SD never co-reside), but the SD model's own footprint + the reload
  may still stress 12 GB. If SD OOMs, the one-strike latch fires + auto-gen
  latches off — the game continues (narrator unaffected). A smaller SD model
  (SD 1.5 over SDXL) is the conservative first choice.
- **The dest filename.** Slices 1-5 overwrite a single shared
  `apps/fable/backgrounds/scene.png` each turn (the locked decision). If you
  want per-card scene history later, that's a `scene_background_path` change.
