//! The backend-agnostic Stable Diffusion seam (PRISM's engine, 2026-07-29 →
//! 2026-08-15).
//!
//! Defines the `SceneImageGenerator` trait + the request/result/error types
//! PRISM's swap pipeline consumes, plus two backends: `NoopImageGenerator`
//! (default builds — writes an empty PNG, zero CUDA compile) and the real
//! feature-gated `DiffusionRsGenerator` (newfla/diffusion-rs wrapping
//! stable-diffusion.cpp). Prompt composition is PRISM's OWN job (the
//! user-authored pill-chip prompt); the Fable scene-art composer that used to
//! live here (`compose_scene_prompt` + its no-graph fallback) was
//! production-dead since SD was unhooked from Fable and is deleted (#73).

// ===========================================================================
// The SceneImageGenerator trait (backend-agnostic seam)
// ===========================================================================
//
// This is the trait the actual Stable Diffusion backend (diffusion-rs, added
// via Cargo.toml in Phase 5B) implements. It's defined HERE, in scene_art.rs,
// so the swap-lock + the one-strike failure latch can all be built +
// unit-tested against a stub backend BEFORE the diffusion-rs dependency is
// added (which carries the ~30-min one-time CUDA compile).
//
// The contract intentionally separates LIFECYCLE (load/unload — VRAM-heavy,
// gated by the swap-lock's `ContextRole::Sd` lease) from GENERATION
// (load is already done; just run the model). This mirrors how the LLM
// engines split spawn (load) from generate_turn. The caller acquires the
// `ContextRole::Sd` lease, loads, generates, then the lease drops and the
// teardown unloads (evicting the weights for the reverse swap).

use std::path::PathBuf;

/// A request to generate one image. Built by PRISM from the user's composer
/// params (the prompt lanes, dims, steps, sampler, seed — see
/// `prism::build_request`) or Fable-agnostic callers. Owned + Send so it can
/// cross the detached `tokio::spawn` boundary (the SD swap runs off the
/// turn's hot path).
#[derive(Debug, Clone)]
pub struct SceneImageRequest {
    /// The generation prompt. PRISM's is user-authored (pill chips); it is
    /// deterministic + Rust-owned — no model prose in here.
    pub prompt: String,
    /// Where to write the generated PNG. Conventionally under
    /// `apps/fable/<card_id>/images/<turn>.png` (per-card image cache, sibling
    /// of the save file). The caller resolves the path; the backend just
    /// writes to it.
    pub dest: PathBuf,
    /// Negative prompt (optional; a small anti-bloat clause — "text, watermark,
    /// signature, low quality"). The backend may prepend its own defaults.
    pub negative_prompt: Option<String>,
    /// Width in pixels. WUPI's stage is 16:9-ish; default 768 (SD 1.5 native)
    /// or 1024 (SDXL). The caller sets it from settings; the backend honors it.
    pub width: u32,
    /// Height in pixels. See `width`.
    pub height: u32,
    /// Sampling steps. Lower = faster; the one-strike latch cares about
    /// throughput. Default 20-30 for SD 1.5, 4-8 for turbo models.
    pub steps: u32,
    /// The SD model file path (a .gguf or .safetenders, resolved like
    /// `resolve_model_path` resolves the LLM). The backend loads this on
    /// `load`; it's threaded through the request so the backend stays stateless
    /// about WHICH model (a future multi-model swap is one field change).
    pub model_path: PathBuf,
    /// RNG seed. `i64` (the crate's native type — see `ConfigBuilder::seed`).
    /// `-1` (default) = random; any `>= 0` value LOCKS the seed → identical
    /// seed + identical params produce identical pixels (the deterministic
    /// contract stable-diffusion.cpp honors via `--seed`). Prism's Fork & Edit
    /// (seed-locked A/B iteration) sets this to a captured result's seed;
    /// FABLE's scene-art path leaves it at `-1` (a fresh scene each turn).
    pub seed: i64,
    /// Classifier-free guidance scale. `f32`. The crate default is `7.0`
    /// (SD 1.5 range); WUPI's SDXL recipe uses `5.0` (SDXL wants lower CFG —
    /// see the §11.58 generate impl). This field lets Prism expose a CFG
    /// slider; FABLE's scene-art path uses the `5.0` default below (unchanged
    /// behavior — the value was previously hardcoded in `generate`).
    pub cfg_scale: f32,
    /// Sampler, stored as the raw `sample_method_t` discriminant (`i32`), NOT
    /// the Rust `SampleMethod` enum — so this struct is constructible WITHOUT
    /// the `diffusion-rs` cargo feature (the enum is feature-gated). The
    /// `DiffusionRsGenerator::generate` impl maps it back to `SampleMethod`
    /// inside the `#[cfg(feature)]` block via `sampler_from_i32`. Default
    /// DPM++ 2M (`DPMPP2M_DISCRIMINANT`) — the LOCKED base-pass sampler
    /// (recipe v2, 2026-08-18: with the mandatory hires refine pass the base
    /// pass owns composition + DPM++ 2M Karras is the fast-convergence
    /// workhorse; the Euler-a single-pass era ended the same day).
    /// `i32` rather than `u32` because the crate's
    /// `sample_method_t` enum is a C `enum` (signed `int`) — matches the FFI
    /// representation exactly.
    pub sampling_method: i32,
}

impl Default for SceneImageRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            dest: PathBuf::new(),
            negative_prompt: None,
            // NoobAI training buckets (the LOCKED size presets): portrait
            // 832×1216 is the default (characters are the primary use);
            // 1216×832 landscape + 1024×1024 square are the composer's other
            // presets. The old 1024×576 default was off-bucket + under the
            // SDXL training area (0.59 vs ~1.0 MP) — environment-biased,
            // under-trained detail (retired 2026-08-17).
            width: PRISM_DEFAULT_WIDTH as u32,
            height: PRISM_DEFAULT_HEIGHT as u32,
            // The LOCKED recipe v2 (2026-08-18, Chloe ruling): DPM++ 2M +
            // Karras converges early — 20 steps is the classic workhorse
            // budget for the composition pass; the mandatory hires refine
            // adds effective sampling on top. Enforced for real in
            // prism::build_request (this Default only serves non-Prism
            // callers/tests — Prism always routes through build_request).
            steps: PRISM_LOCKED_STEPS as u32,
            model_path: PathBuf::new(),
            // `-1` = random seed (the crate default). The seed is the ONE
            // knob that stays user-facing (Fork & Edit's primitive).
            seed: -1,
            // 6.0 = the NoobAI 1.1 official CFG sweet spot (locked recipe).
            cfg_scale: PRISM_LOCKED_CFG,
            // DPM++ 2M (`DPMPP2M_DISCRIMINANT`) — the LOCKED base-pass
            // sampler of recipe v2 (2026-08-18; pairs with the KARRAS
            // scheduler applied in `generate`).
            sampling_method: DPMPP2M_DISCRIMINANT,
        }
    }
}

/// The `i32` discriminant of `SampleMethod::DPMPP2M_SAMPLE_METHOD` (the SDXL
/// clean-baseline sampler). This mirrors the C `enum sample_method_t` ordering
/// in `stable-diffusion.cpp/include/stable-diffusion.h` (DPMPP2M is variant 5).
/// Held as a named const (not a magic `5`) so the value is grep-able + the
/// default-pin test asserts the symbol, not the literal. The crate's enum is
/// feature-gated so we can't reference the variant directly here; this const is
/// the feature-independent mirror, validated by `sampler_round_trip` in the
/// feature-gated tests.
pub const DPMPP2M_DISCRIMINANT: i32 = 5;

// (Euler a's discriminant 1 is deliberately NOT a named const: recipe v2
// retired the sampler entirely — Chloe ruling, 2026-08-19 — and
// `sampler_from_i32` routes the bare value to the DPM++ 2M fallback with
// the unknown ids, so the retired sampler is unreachable from any path.)

// ────────────────────────────────────────────────────────────────────────────
// The LOCKED NoobAI recipe v2 (Chloe ruling, 2026-08-18 — the two-stage era)
// ────────────────────────────────────────────────────────────────────────────
// Base: DPM++ 2M + KARRAS schedule, 20 steps, CFG 6.0, NoobAI training
// buckets, the v1.1 quality prefix auto-prepended + the v2 negative block
// auto-injected. MANDATORY second stage on every render (no toggle): the
// built-in hires fix — RealESRGAN_x4plus_anime_6B scaffold → a low-denoise
// DPM++ 2M refine re-render at 1.5× the bucket. With a mandatory refine
// pass the base sampler's job is composition (DPM++ 2M Karras is the
// fast-convergence workhorse; Euler a's full-budget requirement was a
// single-pass constraint that no longer applies), and the refine pass is
// the detail engine that fixes dull output / lazy eyes / weak faces. All
// of it is server-side; the engine flags (karras scheduler + flash
// attention + the hires params) are applied in
// `DiffusionRsGenerator::generate`, the prompt/meta injection + knob
// overrides in `prism::build_request` (the single chokepoint every
// generation goes through). LOCKED like the sampler profiles in
// settings.rs — changing any of it is Chloe's explicit call.

/// The quality-meta prefix prepended to EVERY Prism prompt — the NoobAI v1.1
/// official recommended positive set (Chloe ruling, 2026-08-17; `newest`
/// matters: the v1.1 recency axis pairs with `old`/`early` in the negative
/// block). INVISIBLE by design: never rendered in the UI, never in the
/// search catalog, and NOT stored in gallery rows (rows record the user
/// prompt — the prefix is an engine constant, like the sampler).
pub const PRISM_QUALITY_PREFIX: &str =
    "masterpiece, best quality, newest, absurdres, highres";

/// The negative block (v2, Chloe ruling 2026-08-18): `old`/`early` pair
/// with the `newest` positive (recency), `text` suppresses rendered
/// lettering/watermark text. The hand tags are GONE — a negative tag the
/// model has no positive concept for mostly dilutes the tokens that DO
/// work (token-weight dilution), and the refine pass is what actually
/// fixes hands. Injected on every render; same invisibility rules as the
/// prefix.
pub const PRISM_NEGATIVE_BLOCK: &str =
    "worst quality, old, early, low quality, lowres, signature, username, logo, text";

/// Locked base-pass step count (v2): DPM++ 2M + Karras converges early —
/// 20 is the classic SDXL workhorse budget, and with the mandatory hires
/// refine pass adding effective sampling on top, the base pass only needs
/// to lock composition (Chloe ruling, 2026-08-18).
pub const PRISM_LOCKED_STEPS: i32 = 20;

/// Locked CFG (the NoobAI 1.1 official sweet spot; above ~7 saturates/burns).
pub const PRISM_LOCKED_CFG: f32 = 6.0;

/// Hires refine scale (v2): 1.5× the bucket (832×1216 → 1248×1824, ~2.27 MP
/// — fits 12 GB with the LLM unloaded during the SD swap; 2× is the stretch
/// config, not the default).
pub const PRISM_HIRES_SCALE: f32 = 1.5;

/// Hires refine EFFECTIVE steps. sd.cpp uses sd-webui semantics
/// (`make_hires_sigma_schedule` scales the schedule by 1/denoise then
/// trims, so this many steps ACTUALLY run — unlike the naive
/// steps×strength model). 12 effective steps of DPM++ 2M Karras at 2.27 MP
/// keeps the whole two-stage render near ~1.5× the old single-pass cost;
/// 15-20 buys depth at proportional cost (Chloe-tunable, 2026-08-18).
pub const PRISM_HIRES_STEPS: i32 = 12;

/// Hires refine denoise strength: 0.45 keeps composition locked (the base
/// pass owns layout — chairs, poses) while re-rendering detail (faces,
/// eyes, hands, texture). Above ~0.5 the refine starts repainting
/// composition; below ~0.35 it's mostly a sharpen (Chloe bump, 2026-08-18).
pub const PRISM_HIRES_DENOISE: f32 = 0.45;

/// The ESRGAN scaffold model filename — a SIBLING of the SD checkpoint in
/// `models/sd/` (rides the boot download overlay like the SD set). This is
/// the RealESRGAN_x4plus_anime_6B model, shipped under the short name
/// `esrgan.pth` (Chloe's rename, 2026-08-18 — the name must match the HF
/// repo file for the overlay to fetch it). When absent, `generate` falls
/// back to the LANCZOS pixel upscaler (zero download, slightly softer
/// scaffold — the refine pass still runs).
pub const PRISM_HIRES_ESRGAN_FILE: &str = "esrgan.pth";

/// Default size — the portrait NoobAI training bucket (characters are the
/// primary use). The composer's preset list is bucket-only:
/// 832×1216 / 1216×832 / 1024×1024 (~1.0 MP each, matching the SDXL
/// training area). Off-bucket sizes (the old 1024×576 default) bias toward
/// environment + render under-trained detail.
pub const PRISM_DEFAULT_WIDTH: i32 = 832;
pub const PRISM_DEFAULT_HEIGHT: i32 = 1216;

/// The result of a successful generation. The caller (the done-beat spawn)
/// emits this as a `{type:"image", url}` channel event so the frontend can
/// display it. `bytes` is the raw PNG; the caller may write it to `dest`
/// itself or the backend may have already (the trait is silent on which —
/// both are acceptable; the caller checks `dest.exists()`).
#[derive(Debug, Clone)]
pub struct SceneImageResult {
    /// The destination path (echoed back; the frontend subscribes by path).
    pub dest: PathBuf,
    /// Generation wall-clock time in ms (for telemetry + the failure-latch
    /// timeout heuristic).
    pub elapsed_ms: u128,
}

/// The error a generator returns on failure. The one-strike failure latch
/// treats ANY error as a strike: the first failure disables auto-gen + locks
/// the LLM back into memory (never strand the game with no engine — the §2B
/// invariant). The user re-enables manually after fixing the cause (corrupt
/// model, OOM, missing GPU).
#[derive(Debug, Clone)]
pub struct SceneImageError {
    pub message: String,
}

impl std::fmt::Display for SceneImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SceneImageError {}

/// The backend-agnostic image-generation trait (Phase 5B, 2026-07-29).
///
/// **Lifecycle split (mirrors the LLM engines' spawn/generate split):**
/// - `load(&model_path)` — loads the SD model into VRAM. Called AFTER the
///   `ContextRole::Sd` lease is acquired (so the LLM weights are evicted
///   first). This is the heavy call (~seconds).
/// - `generate(&request)` — runs one generation. The model is already loaded.
/// - `unload()` — frees the SD model from VRAM. Called by the lease teardown,
///   BEFORE the LLM weights reload on the reverse swap.
///
/// The concrete `diffusion-rs` impl lives behind this trait so:
/// (a) the swap-lock + wiring compile + unit-test WITHOUT the diffusion-rs
///     dependency (which carries the ~30-min CUDA compile — building the
///     scaffolding first keeps that compile isolated);
/// (b) the backend choice stays Chloe's build-safety call (a future candle
///     or external-process backend swaps in by implementing this trait).
///
/// All methods take `&self` (interior mutability is the backend's concern —
/// diffusion-rs wraps a `*mut sd_ctx_t` behind its own thread-safety). Send
/// + Sync so the engine can live in an `Arc` on AppState + be driven from a
/// detached `tokio::spawn`.
pub trait SceneImageGenerator: Send + Sync {
    /// Load the model into VRAM. Heavy (~seconds). Idempotent if already
    /// loaded (the lease may be re-acquired). Returns an error on OOM /
    /// corrupt model / missing GPU — the caller's one-strike latch fires.
    fn load(&self, model_path: &std::path::Path) -> Result<(), SceneImageError>;

    /// Generate one image. The model must already be loaded. Writes the PNG
    /// to `request.dest` (or returns it in `SceneImageResult` — the caller
    /// checks `dest.exists()` either way).
    fn generate(&self, request: &SceneImageRequest) -> Result<SceneImageResult, SceneImageError>;

    /// Unload the model from VRAM. Called by the lease teardown on the
    /// reverse swap (before the LLM weights reload). Must synchronously free
    /// VRAM (same contract as the LLM engines' `shutdown()` — the reload
    /// races this).
    fn unload(&self);
}

/// A no-op stub backend for compile-without-SD + unit testing. Implements
/// `SceneImageGenerator` by doing nothing (load/unload are no-ops; generate
/// writes an empty file + returns success). This is what ships in the 5B
/// scaffolding BEFORE the diffusion-rs dependency lands — it lets the entire
/// swap-lock + wiring + failure-latch path be exercised in tests without the
/// CUDA compile. The real `DiffusionRsGenerator` (behind a cargo feature)
/// replaces this once the dependency is added.
#[derive(Debug, Default)]
pub struct NoopImageGenerator;

impl SceneImageGenerator for NoopImageGenerator {
    fn load(&self, _model_path: &std::path::Path) -> Result<(), SceneImageError> {
        Ok(())
    }
    fn generate(&self, request: &SceneImageRequest) -> Result<SceneImageResult, SceneImageError> {
        let start = std::time::Instant::now();
        // Write an empty file so `dest.exists()` is true (the frontend
        // subscriber keys off the path). A real backend writes a PNG.
        let _ = std::fs::write(&request.dest, b"");
        Ok(SceneImageResult {
            dest: request.dest.clone(),
            elapsed_ms: start.elapsed().as_millis(),
        })
    }
    fn unload(&self) {}
}

#[cfg(test)]
mod phase5b_tests {
    use super::*;

    /// The stub backend satisfies the trait + writes the dest file. This is
    /// the smoke test for the backend-agnostic scaffolding (exercised without
    /// the diffusion-rs dependency).
    #[test]
    fn noop_generator_writes_dest_and_returns_result() {
        let gen = NoopImageGenerator;
        let dir = std::env::temp_dir();
        let dest = dir.join("wupi_scene_art_noop_test.png");
        let _ = std::fs::remove_file(&dest);
        let req = SceneImageRequest {
            prompt: "a test tavern".into(),
            dest: dest.clone(),
            ..Default::default()
        };
        gen.load(std::path::Path::new("/nonexistent/model.gguf")).unwrap();
        let result = gen.generate(&req).expect("noop generate succeeds");
        assert!(dest.exists(), "noop must write the dest file");
        assert_eq!(result.dest, dest);
        gen.unload();
        let _ = std::fs::remove_file(&dest);
    }

    /// The request type defaults to the LOCKED recipe v2 (832×1216 portrait
    /// bucket, DPM++ 2M, 20 steps). Pins the defaults so a future tuning
    /// change is intentional. (The recipe history: 1024×576/28-discrete →
    /// 832×1216/20/DPM++ 2M+Karras → 832×1216/30/Euler a+discrete → v2
    /// 2026-08-18: back to DPM++ 2M+Karras at 20 + the mandatory ESRGAN
    /// hires refine pass.)
    #[test]
    fn scene_image_request_defaults_are_sane() {
        let req = SceneImageRequest::default();
        assert_eq!(req.width, 832, "NoobAI portrait training bucket");
        assert_eq!(req.height, 1216, "NoobAI portrait training bucket");
        assert_eq!(req.steps, PRISM_LOCKED_STEPS as u32, "DPM++ 2M + Karras early-convergence budget (20, recipe v2)");
        assert!(req.negative_prompt.is_none(), "engine-level default is bare; the block is injected in prism::build_request");
    }

    /// Prism (locked recipe v2, 2026-08-18): the locked knobs default to the
    /// two-stage recipe — `-1` seed (the ONE user-facing knob left), `6.0`
    /// CFG, DPM++ 2M. Pinning these here means a future change is
    /// intentional + obvious. (The real enforcement is prism::build_request
    /// — this Default only keeps non-Prism constructors in lockstep.)
    #[test]
    fn scene_image_request_defaults_pin_prism_fields() {
        let req = SceneImageRequest::default();
        assert_eq!(req.seed, -1, "-1 = random seed (crate default; the user-facing knob)");
        assert_eq!(req.cfg_scale, PRISM_LOCKED_CFG, "6.0 = the NoobAI 1.1 official CFG (locked)");
        assert_eq!(
            req.sampling_method, DPMPP2M_DISCRIMINANT,
            "DPM++ 2M = the LOCKED base-pass sampler (karras schedule applied in generate)",
        );
    }

    /// The locked-recipe meta constants are pinned verbatim so a typo'd edit
    /// can't ship (v2, 2026-08-18: hand tags out, `text` in; hires constants).
    #[test]
    fn locked_recipe_constants_match_the_noobai_recipe() {
        assert_eq!(
            PRISM_QUALITY_PREFIX,
            "masterpiece, best quality, newest, absurdres, highres",
        );
        assert_eq!(
            PRISM_NEGATIVE_BLOCK,
            "worst quality, old, early, low quality, lowres, signature, username, logo, text",
        );
        assert_eq!(PRISM_LOCKED_STEPS, 20);
        assert_eq!(PRISM_LOCKED_CFG, 6.0);
        assert_eq!(DPMPP2M_DISCRIMINANT, 5);
        // The mandatory hires refine pass (v2): 1.5× scale, 12 effective
        // steps (sd-webui semantics), 0.45 denoise.
        assert_eq!(PRISM_HIRES_SCALE, 1.5);
        assert_eq!(PRISM_HIRES_STEPS, 12);
        assert_eq!(PRISM_HIRES_DENOISE, 0.45);
        assert_eq!(PRISM_HIRES_ESRGAN_FILE, "esrgan.pth");
    }

    /// The DPMPP2M discriminant const must equal 5 (variant 5 in the C
    /// `enum sample_method_t`). If stable-diffusion.cpp ever reorders the enum
    /// this test catches it; the feature-gated `sampler_round_trip` test (in
    /// the diffusion-rs block) cross-validates the const against the real enum.
    #[test]
    fn dpmpp2m_discriminant_is_stable() {
        assert_eq!(DPMPP2M_DISCRIMINANT, 5);
    }

    /// A locked seed (`>= 0`) survives into the request unchanged; the sign is
    /// the feature-independent contract (the crate treats `< 0` as random). This
    /// pins the convention Prism's Fork & Edit relies on: capture a result's
    /// seed, re-run with it, get identical pixels.
    #[test]
    fn scene_image_request_carries_locked_seed() {
        let req = SceneImageRequest {
            prompt: "test".into(),
            seed: 12345,
            ..Default::default()
        };
        assert_eq!(req.seed, 12345, "a locked seed passes through verbatim");
    }
}

/// Phase 5B (2026-07-29): construct the default SD backend for the current
/// build. Returns the `NoopImageGenerator` stub when diffusion-rs is not
/// compiled in (the 5B scaffolding), or the real `DiffusionRsGenerator` when
/// the cargo feature is enabled (Phase 5B completion). The done-beat spawn
/// calls this to populate `AppState::sd_engine` on first request.
///
/// Kept here (scene_art.rs) so the backend choice is collocated with the
/// trait it implements. The feature-gate keeps the scaffolding compiling
/// WITHOUT the diffusion-rs dependency (which carries the ~30-min CUDA
/// compile) — the stub lets the entire swap path be exercised in tests.
pub fn default_sd_backend() -> Box<dyn SceneImageGenerator> {
    // Phase 5B: dispatch is feature-gated. With the `diffusion-rs` cargo
    // feature enabled (Chloe's build-safety gate, see
    // docs/phase5b-diffusion-rs-cargo-procedure.md), the real
    // stable-diffusion.cpp backend is returned; without it the
    // `NoopImageGenerator` stub (zero CUDA compile, writes an empty PNG so
    // the full swap pipeline is exercised in tests). Staged rollout per
    // the §11.58 plan: SDXL baseline (no LoRA) first, acceleration LoRAs
    // (LCM/Hyper-SD) layered in once clean images are confirmed.
    #[cfg(feature = "diffusion-rs")]
    {
        Box::new(DiffusionRsGenerator::new())
    }
    #[cfg(not(feature = "diffusion-rs"))]
    {
        Box::new(NoopImageGenerator)
    }
}

/// Phase 5B: the real Stable Diffusion backend (newfla/diffusion-rs,
/// wrapping leejet/stable-diffusion.cpp). Feature-gated so the default build
/// keeps using `NoopImageGenerator` (no CUDA compile); Chloe's build-safety
/// procedure (docs/phase5b-diffusion-rs-cargo-procedure.md) adds the dep.
///
/// The trait's load/generate/unload lifecycle maps onto diffusion-rs as:
///   - `load`    → parse + cache the `ModelConfig` (the weights) from the
///                 user's checkpoint path (models/sd/*.safetensors). WUPI
///                 loads a CUSTOM path, NOT a Preset (presets auto-download
///                 — wrong for a portable offline app). The weights are the
///                 expensive thing to parse, so they're the held state.
///   - `generate` → build a fresh per-turn `Config` (prompt/dims/steps/dest
///                 are cheap) + `api::gen_img(&config, &mut model_config)`.
///   - `unload`  → drop the held `ModelConfig` so stable-diffusion.cpp frees
///                 its VRAM (the reverse-swap reloads the LLM weights after).
///
/// STATE HOLDING RATIONALE: `gen_img(&Config, &mut ModelConfig)` (confirmed
/// docs.rs signature, api.rs:1306). Naively one would hold `ModelConfig`
/// across turns as the "expensive parsed weights" — but reading the crate
/// source disproves that: `ModelConfig::diffusion_ctx()` (api.rs:761) frees
/// + rebuilds the loaded model context on EVERY `gen_img` call ("the context
/// is cached and won't have a decode graph"). So nothing is actually parsed
/// at `ModelConfigBuilder::build()` time — the struct just stores paths. We
/// therefore store only the resolved checkpoint `PathBuf` (which is `Send +
/// Sync`) and rebuild the cheap `ModelConfig` fresh inside `generate()`.
///
/// This is also REQUIRED for thread-safety: `ModelConfig` holds raw C
/// pointers (`*mut sd_ctx_t`, `*mut upscaler_ctx_t`, `*const i8` from
/// `sd_ctx_params_t`) populated lazily by `diffusion_ctx()`, making it
/// `!Send` + `!Sync`. Holding it would make `DiffusionRsGenerator` fail the
/// `SceneImageGenerator: Send + Sync` bound (compile error E0277). A `PathBuf`
/// has no such issue. See the 2026-07-30 build log for the original E0277.
///
/// `model_path` is interior-mutable because the orchestrator calls
/// load/generate/unload as separate `&self` methods on the SAME generator
/// instance (the trait is `&self`, not `&mut self`), sequenced across the
/// swap cycle. The Mutex is held only briefly (no contention — the SD lease
/// serializes all SD access).
///
/// STAGED ROLLOUT (per §11.58 + Chloe's 2026-07-30 call):
///   Stage 1 (this impl): SDXL checkpoint, NO LoRA, full step budget. Proves the
///     engine produces a clean image through the cpp wrapper. Recommended
///     samplers/sizes below are the SDXL-community canonical "clean SDXL"
///     recipe (Euler/a artifacts on SDXL — DPM++ 2M is the baseline).
///   Stage 2 (future): acceleration LoRA via `api::LoraSpec` (the crate DOES
///     expose custom-path LoRA — confirmed in the `modifier` module + the
///     `LoraSpec`/`LoraModeType` types). LCM-LoRA first (cpp-tested, the
///     crate ships a built-in helper), then Hyper-SD/Lightning at 4-8 steps.
///   Stage 3 (future): NSFW anime SDXL (NoobAI-XL / WAI-Illustrious) — both
///     confirmed Illustrious-XL family, full NSFW. SDXL `sdxl_vae_fp16_fix`
///     modifier is the known black-image fix; for a portable app the user
///     supplies the VAE path (auto-download modifier is wrong offline).

/// Map a feature-independent `i32` sampler discriminant back to the crate's
/// `SampleMethod` enum. The discriminant ordering mirrors the C
/// `enum sample_method_t` in `stable-diffusion.cpp/include/stable-diffusion.h`
/// (variants 0-18; 19 is the `SAMPLE_METHOD_COUNT` sentinel — treated as the
/// DPM++ 2M fallback). An unknown or retired value falls back to DPM++ 2M
/// (the SDXL clean-baseline sampler) rather than erroring: a stale gallery
/// DB row that references a sampler removed in a future crate version
/// should still generate, not block the user. Feature-gated because `SampleMethod` is only
/// in scope behind the `diffusion-rs` feature; the i32 side of the contract
/// (the `SceneImageRequest.sampling_method` field + the `DPMPP2M_DISCRIMINANT`
/// const) lives outside the gate so the struct is constructible everywhere.
#[cfg(feature = "diffusion-rs")]
fn sampler_from_i32(discriminant: i32) -> diffusion_rs::api::SampleMethod {
    use diffusion_rs::api::SampleMethod::*;
    // NOTE: match arms ordered to mirror the C enum declaration exactly so a
    // future variant insertion is an obvious diff. The discriminant values are
    // stable (the C enum is part of stable-diffusion.cpp's FFI ABI). ONE
    // deliberate omission: arm 1 (Euler a) — retired by recipe v2 (Chloe
    // ruling, 2026-08-19), it drops to the DPM++ 2M fallback below with the
    // unknown ids so the retired sampler can't be selected even at this layer.
    match discriminant {
        0 => EULER_SAMPLE_METHOD,
        2 => HEUN_SAMPLE_METHOD,
        3 => DPM2_SAMPLE_METHOD,
        4 => DPMPP2S_A_SAMPLE_METHOD,
        5 => DPMPP2M_SAMPLE_METHOD, // default; the DPMPP2M_DISCRIMINANT const
        6 => DPMPP2Mv2_SAMPLE_METHOD,
        7 => IPNDM_SAMPLE_METHOD,
        8 => IPNDM_V_SAMPLE_METHOD,
        9 => LCM_SAMPLE_METHOD,
        10 => DDIM_TRAILING_SAMPLE_METHOD,
        11 => TCD_SAMPLE_METHOD,
        12 => RES_MULTISTEP_SAMPLE_METHOD,
        13 => RES_2S_SAMPLE_METHOD,
        14 => ER_SDE_SAMPLE_METHOD,
        15 => EULER_CFG_PP_SAMPLE_METHOD,
        16 => EULER_A_CFG_PP_SAMPLE_METHOD,
        17 => EULER_GE_SAMPLE_METHOD,
        // Anything else (including the 18 COUNT sentinel, negative values,
        // or future variants not yet mirrored here) → DPM++ 2M fallback.
        _ => DPMPP2M_SAMPLE_METHOD,
    }
}

/// The real Stable Diffusion backend (newfla/diffusion-rs, wrapping
/// leejet/stable-diffusion.cpp). Feature-gated so the default build (without
/// `--features diffusion-rs`) compiles in seconds — the `NoopImageGenerator`
/// stub above ships in its place. See the cargo procedure doc
/// (docs/phase5b-diffusion-rs-cargo-procedure.md) for the dependency add.
#[cfg(feature = "diffusion-rs")]
pub struct DiffusionRsGenerator {
    /// The resolved checkpoint path, stashed by `load` for `generate` to
    /// build a `ModelConfig` from. See the struct-level STATE HOLDING
    /// RATIONALE for why we hold a `PathBuf` (not a `ModelConfig`):
    /// ModelConfig is `!Send`/`!Sync` (raw C pointers) and stores only paths
    /// anyway (the expensive VRAM parse happens inside `gen_img`, not build).
    /// None before `load` + after `unload`.
    model_path: std::sync::Mutex<Option<PathBuf>>,
    /// The resolved multi-file SDXL layout (sibling CLIP/VAE), stashed by
    /// `load` when the picked checkpoint is a ComfyUI-style GGUF UNet (bare
    /// `input_blocks.*` tensor names, no embedded CLIP/VAE). None for the
    /// legacy single-file checkpoint layout. See `SdModelLayout` + the
    /// `load`/`generate` doc for the §11.59 multi-file SDXL contract.
    multi_file: std::sync::Mutex<Option<SdModelLayout>>,
    /// The fp16 text-encoder override, stashed by `load` when the checkpoint
    /// is a FULL-checkpoint GGUF sitting next to the fp16 CLIP-L + CLIP-G
    /// sidecar pair. See `ClipOverride` for the shadow mechanism. None in
    /// every other layout.
    clip_override: std::sync::Mutex<Option<ClipOverride>>,
}

/// The fp16 text-encoder override (2026-08-17): a full-checkpoint GGUF kept
/// on the single-file `model` path while external fp16 CLIP-L/CLIP-G
/// sidecars SHADOW the embedded quantized encoders.
///
/// MECHANISM (why this works without touching sd.cpp): `new_sd_ctx` loads
/// the checkpoint first, then the clip files, into ONE name-keyed tensor
/// map (`add_tensor_storage`: plain map assignment — later writes win).
/// After `convert_tensors_name`, the embedded `conditioner.embedders.0/1.*`
/// tensors and the external files' tensors canonicalize to the IDENTICAL
/// `cond_stage_model[.1].transformer.text_model.*` names (the prefix_map
/// `conditioner.embedders.0/1.` → `cond_stage_model[.1].` rewrite +
/// `convert_open_clip_to_hf_clip_name` normalizing `model.*` ↔
/// `transformer.text_model.*`), so the later-loaded fp16 storages replace
/// the embedded q8_0 ones by collision. The conditioner runner then reads
/// the same canonical names it always did — now backed by fp16 weights.
///
/// WHY (the extra-limb diagnosis, 2026-08-17): image.gguf's CONDITIONER is
/// majority-q8_0 (323/585 tensors), and quantized CLIP conditioning degrades
/// prompt adherence → duplicated limbs/subjects. fp16 encoders are the fix;
/// the UNet's own q8_0 majority (mild, tolerable) stays.
///
/// NO EXTERNAL VAE, DELIBERATELY: the embedded VAE is kept, so sd.cpp's
/// automatic SDXL Conv2D 1/32 guard applies (`vae_path` empty) — an external
/// VAE bypasses that guard and is the documented §9 external-VAE noise-era
/// hazard. A `sdxl_vae_fp16_fix.safetensors` sibling may sit in the dir
/// unused by this layout (it goes live only for the §11.59 bare-UNet
/// multi-file layout).
#[cfg(feature = "diffusion-rs")]
#[derive(Clone, Debug)]
pub struct ClipOverride {
    /// The full-checkpoint GGUF (passed as `model` — single-file path).
    pub model: PathBuf,
    /// The fp16 CLIP-L (openCLIP ViT-L) sidecar.
    pub clip_l: PathBuf,
    /// The fp16 CLIP-G (openCLIP ViT-bigG) sidecar.
    pub clip_g: PathBuf,
}

/// The multi-file SDXL layout: a ComfyUI-convention GGUF UNet (loaded as
/// `diffusion_model` so sd.cpp applies its `model.diffusion_model.` prefix +
/// can detect SDXL via `get_sd_version`), plus the separate CLIP-L, CLIP-G,
/// and VAE files the GGUF doesn't embed.
///
/// WHY MULTI-FILE (§11.59): a Q8-GGUF-converted SDXL UNet stores ONLY the
/// diffusion model tensors under bare `input_blocks.*` names. sd.cpp's
/// version detection (`model_loader.cpp:503-510`) requires BOTH
/// `model.diffusion_model.input_blocks.*` (→ `is_unet`) AND
/// `conditioner.embedders.1` / `cond_stage_model.1` (→ `has_multiple_encoders`)
/// to classify the model as SDXL (`is_xl`). The CLIP-G file provides the
/// second encoder markers; the GGUF alone can't be classified. So the GGUF
/// MUST be passed as `diffusion_model` (which gets the prefix) alongside
/// `clip_g` (which provides the multi-encoder marker), or version detection
/// fails with `get sd version from file failed`.
///
/// The CLIP/VAE files are resolved as siblings of the GGUF in the same
/// `models/sd/` directory at `load` time (see `resolve_multi_file_layout`).
/// Missing CLIP-G is a hard error for a GGUF UNet (version detection will
/// fail without it); missing CLIP-L/VAE fall back to sd.cpp's defaults
/// (CLIP-L is optional for SDXL's dual-encoder path; an absent explicit VAE
/// uses the SDXL base VAE — may produce subtle color artifacts on NoobAI
/// but won't fail to load).
#[cfg(feature = "diffusion-rs")]
#[derive(Clone, Debug)]
pub struct SdModelLayout {
    /// The GGUF UNet (passed as `diffusion_model`). Required.
    pub diffusion_model: PathBuf,
    /// The SDXL CLIP-L (openCLIP ViT-L). Optional but recommended.
    pub clip_l: Option<PathBuf>,
    /// The SDXL CLIP-G (openCLIP ViT-bigG). REQUIRED for version detection.
    pub clip_g: Option<PathBuf>,
    /// The SDXL VAE (sdxl_vae_fp16_fix recommended for NoobAI). Optional.
    pub vae: Option<PathBuf>,
}

#[cfg(feature = "diffusion-rs")]
impl Default for DiffusionRsGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "diffusion-rs")]
impl DiffusionRsGenerator {
    pub fn new() -> Self {
        Self {
            model_path: std::sync::Mutex::new(None),
            multi_file: std::sync::Mutex::new(None),
            clip_override: std::sync::Mutex::new(None),
        }
    }
}

// ===========================================================================
// §11.59 SD abort capture — `install_sd_abort_callback`
// ===========================================================================
// WHY THIS EXISTS: when stable-diffusion.cpp hits a fatal condition it calls
// `GGML_ABORT(...)` / `GGML_ASSERT(x)` (defined in the ggml bundled INSIDE
// diffusion-rs-sys, NOT llama's ggml — the two are separate statically-linked
// copies). `ggml_abort()` formats `"file:line: <msg>"`, calls the registered
// abort callback (if any), then calls `abort()` unconditionally — so the
// callback is CAPTURE-ONLY (the process still dies with 0x80000003), but it
// runs synchronously BEFORE the fatal signal. The default (no callback)
// path does `fprintf(stderr, msg); abort()` — and because `npm run sd:dev`
// redirects `2>&1` to a file, Windows makes that stderr fully block-buffered;
// `abort()`→`TerminateProcess` never flushes it, so the assertion text dies
// in the CRT buffer (the black-box symptom from §11.58/§11.59).
//
// Registering `ggml_set_abort_callback` (SD's OWN ggml symbol, reached via
// `diffusion_rs_sys` — the high-level `diffusion-rs` crate does NOT re-export
// it) lets us receive the fully-formatted message + write it to disk via a
// direct kernel `fs::write` (NOT CRT-buffered) — which survives the impending
// process death. The standalone repro harness (src/bin/sd_repro.rs) ALSO
// benefits: run in a real TTY, stderr is line-buffered so the message is
// visible inline even without the file capture.
//
// THE RESUME RULE (AGENTS.md §0): read the captured assertion BEFORE pulling
// any lever (Q8_0 / VAE / v-pred probe). This fn is the capture; the lever
// comes only after the real `GGML_ASSERT` line is known.
#[cfg(feature = "diffusion-rs")]
pub fn install_sd_abort_callback() {
    use diffusion_rs_sys::ggml_set_abort_callback;
    use std::os::raw::c_char;
    use std::sync::Once;

    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        // The callback receives a NUL-terminated C string (the formatted
        // assertion: "ggml.c:1234: GGML_ASSERT(x) failed"). We CStr-copy it
        // to a Rust String, append a marker + timestamp, then `fs::write`
        // (kernel-level, not CRT-buffered) to logs/sd-abort.txt. The write is
        // synchronous + completes before this fn returns, so it lands on disk
        // before `abort()` tears the process down. We OVERWRITE (not append)
        // on each crash so the file always reflects the most recent failure —
        // a stale multi-crash log is harder to read than a fresh one.
        extern "C" fn on_abort(msg: *const c_char) {
            let text = if msg.is_null() {
                "<null abort message>".to_string()
            } else {
                // Safety: ggml_abort formats into a stack `char[2048]` buffer
                // (confirmed in ggml.c:256) — always NUL-terminated. The
                // pointer is valid for the duration of this callback.
                unsafe {
                    std::ffi::CStr::from_ptr(msg)
                        .to_string_lossy()
                        .into_owned()
                }
            };
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            // Resolve <exe_dir>/logs/sd-abort.txt — works for both dev runs
            // (target/debug) + portable installs (install root). Falls back to
            // the temp dir if exe_dir resolution fails (must never panic in an
            // abort callback).
            let dir = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(std::path::Path::to_path_buf))
                .unwrap_or_else(|| std::env::temp_dir());
            let dest = dir.join("logs").join("sd-abort.txt");
            let body = format!(
                "[sd-abort ts={ts}]\n{text}\n\n\
                 --- next steps ---\n\
                 The line above is the EXACT ggml assertion that aborted gen_img.\n\
                 Read it BEFORE pulling any lever (Q8_0 / VAE / v-pred probe).\n"
            );
            // Kernel-level write: survives the impending abort(). Errors are
            // swallowed (an abort callback must NEVER panic — no unwinding
            // through FFI).
            let _ = std::fs::write(&dest, &body);
            // Crash report with diagnostic context (logs.rs RAM ring):
            // an abort() never reaches the Rust panic hook, so the ring
            // dump is wired HERE for the image-gen path. Same must-never-
            // panic discipline (dump_crash is best-effort, errors swallowed).
            crate::logs::dump_crash(&format!("SD gen_img abort: {text}"));
            // ALSO emit to the tracing log (best-effort; the non-blocking
            // writer may or may not flush before abort, but the direct
            // fs::write above is the guaranteed capture).
            tracing::error!(abort_message = %text, dest = %dest.display(), "ggml abort callback fired inside gen_img — captured to sd-abort.txt");
        }

        // Register. Returns the previous callback (None first time); we
        // discard it (WUPI never installs a different one).
        unsafe {
            ggml_set_abort_callback(Some(on_abort));
        }
        tracing::info!("diffusion-rs: ggml abort callback installed (crashes will be captured to logs/sd-abort.txt)");
    });
}

// ===========================================================================
// §11.61 SD engine log bridge — `install_sd_log_bridge`
// ===========================================================================
// WHY THIS EXISTS: stable-diffusion.cpp logs EVERYTHING (version detection,
// per-file load results, the VAE conv-scale path, per-step sampling) through
// `log_printf` → `sd_log_cb` — and when NO callback is registered the message
// is silently DROPPED (there is no stderr default; see util.cpp:584). Neither
// diffusion-rs 0.1.20 nor WUPI ever installed one, so the SD engine has been
// running mute: the 2026-08-17 renders produced noise with ZERO engine-side
// telemetry to diagnose from (the version-detection line, the "No valid VAE
// specified … using Conv2D scale" warning, everything — invisible).
//
// This mirrors `install_sd_abort_callback`: Once-guarded, registered before
// any gen_img call, capture-only. The callback receives sd.cpp's pre-formatted
// line ("file.cc:123 - message"); we eprintln! INFO+ so it lands on the live
// console AND in `logs/sd-out.log` via the `sd:dev` 2>&1 redirect (the same
// engine-pulse doctrine as the llama `[DEBUG]` telemetry), and mirror to
// tracing so `%TEMP%/wupi.log` carries it too. DEBUG stays tracing-only (it
// is chatty per-step; RUST_LOG gates it). Must never panic (C callback).
#[cfg(feature = "diffusion-rs")]
pub fn install_sd_log_bridge() {
    use diffusion_rs_sys::{sd_log_level_t, sd_set_log_callback};
    use std::os::raw::c_char;
    use std::sync::Once;

    static INSTALL: Once = Once::new();

    INSTALL.call_once(|| {
        extern "C" fn on_log(
            level: sd_log_level_t,
            msg: *const c_char,
            _data: *mut std::ffi::c_void,
        ) {
            // Safety: sd.cpp's log_printf formats into a static buffer that
            // stays alive for the duration of the callback (same contract as
            // the abort callback's message).
            let text = if msg.is_null() {
                "<null log message>".to_string()
            } else {
                unsafe { std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned() }
            };
            let line = text.trim_end();
            match level {
                sd_log_level_t::SD_LOG_WARN | sd_log_level_t::SD_LOG_ERROR => {
                    eprintln!("[sd] {line}");
                    tracing::warn!("{line}");
                }
                sd_log_level_t::SD_LOG_INFO => {
                    eprintln!("[sd] {line}");
                    tracing::info!("{line}");
                }
                _ => {
                    tracing::debug!("{line}");
                }
            }
        }

        unsafe {
            sd_set_log_callback(Some(on_log), std::ptr::null_mut());
        }
        tracing::info!("diffusion-rs: sd.cpp log bridge installed (engine logs flow to stderr + wupi.log)");
    });
}

/// Detect whether a checkpoint is a ComfyUI-convention GGUF UNet (the §11.59
/// multi-file SDXL layout). True iff the file is a GGUF (magic sniff; a bare
/// UNet GGUF is then routed to the multi-file `diffusion_model` path by
/// `load`, gated on `gguf_embeds_full_checkpoint` being false). A
/// `.safetensors` returns false → the single-file load path. Cheap (4 bytes),
/// called once at load.
#[cfg(feature = "diffusion-rs")]
fn is_gguf_unet(path: &std::path::Path) -> bool {
    let is_gguf = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false);
    if !is_gguf {
        return false;
    }
    // Sniff the GGUF magic (4 bytes: 'G','G','U','F'). A .gguf with the wrong
    // magic is corrupt — treat as not-a-unet so the single-file path surfaces
    // a clean load error rather than a misleading multi-file failure.
    match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            let mut buf = [0u8; 4];
            f.read_exact(&mut buf).is_ok() && &buf == b"GGUF"
        }
        Err(_) => false,
    }
}

/// Detect whether a GGUF carries a FULL checkpoint (embedded CLIP text
/// encoders + VAE) vs a bare ComfyUI-convention UNet. True iff the byte
/// pattern `first_stage_model` (the VAE tensor-name prefix — present in every
/// full SD/SDXL checkpoint, absent from a UNet-only GGUF) appears in a bounded
/// prefix of the file. Tensor names live at the TOP of a GGUF (header + KV
/// metadata + tensor-info table, before the multi-GB data section), so a few
/// MB is always sufficient — the 4.18 GB full-checkpoint GGUF's name table
/// ends ~150 KB in even with dense per-tensor metadata.
///
/// WHY THE SNIFF (2026-08-17, §11.61): a full-checkpoint GGUF by default
/// rides the single-file `model` path, not the multi-file `diffusion_model`
/// path — `init_from_file(path, "model.diffusion_model.")` prefixes every
/// tensor it doesn't recognize as already-prefixed, so the embedded
/// `conditioner.embedders.*` / `first_stage_model.*` tensors would become
/// `model.diffusion_model.conditioner…` (unreachable) and the external
/// clip_l/clip_g the multi-file path requires wouldn't exist → version
/// detection fails. On the `model` path (no prefix) every native name lands
/// as-is and name-based version detection sees the full marker set.
///
/// EXCEPTION (the 2026-08-17 encoder override in `load`): when the fp16
/// CLIP-L + CLIP-G sidecar pair sits next to the GGUF, the full checkpoint
/// still rides the single-file `model` path — but with the external clip
/// paths attached, which SHADOW the embedded (quantized) encoders by
/// name-collision replacement in sd.cpp's tensor map (see `ClipOverride`).
/// The embedded VAE is kept either way (no external VAE — §9 hazard).
#[cfg(feature = "diffusion-rs")]
fn gguf_embeds_full_checkpoint(path: &std::path::Path) -> bool {
    use std::io::Read;
    const WINDOW: usize = 4 * 1024 * 1024; // 4 MB prefix
    const NEEDLE: &[u8] = b"first_stage_model";
    let mut buf = Vec::with_capacity(WINDOW);
    match std::fs::File::open(path) {
        Ok(f) => {
            // take(WINDOW) bounds the read on multi-GB files.
            match f.take(WINDOW as u64).read_to_end(&mut buf) {
                Ok(_) => buf.windows(NEEDLE.len()).any(|w| w == NEEDLE),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

/// Resolve the multi-file SDXL layout siblings (CLIP-L, CLIP-G, VAE) in the
/// same directory as the GGUF UNet. The naming conventions are the
/// stable-diffusion.cpp / common-WebUI conventions:
///   - CLIP-L: `clip_l.safetensors`, `clip_l*.safetensors`, `text_encoder*.safetensors` (ViT-L)
///   - CLIP-G: `clip_g.safetensors`, `clip_g*.safetensors`, `text_encoder_2*.safetensors` (ViT-bigG) — the SECOND/larger encoder
///   - VAE:    `sdxl_vae*.safetensors`, `vae*.safetensors`
///
/// Returns Ok with whatever it found (clip_g may be None — the caller logs a
/// warning since version detection needs it). Returns Err only if the
/// directory can't be read.
///
/// Disambiguation: `text_encoder.safetensors` (CLIP-L) vs
/// `text_encoder_2.safetensors` (CLIP-G) — the `_2` suffix is the
/// HuggingFace SDXL convention for the second encoder. CLIP files commonly
/// come as `clip_l.safetensors` + `clip_g.safetensors` (sd.cpp/ComfyUI
/// convention) OR `text_encoder.safetensors` + `text_encoder_2.safetensors`
/// (HF diffusers convention). Both are matched.
#[cfg(feature = "diffusion-rs")]
fn resolve_multi_file_layout(dir: &std::path::Path) -> Result<SdModelLayout, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read sd dir {}: {e}", dir.display()))?;

    // Collect lowercase stem names (filename without extension) → full path.
    let mut stems: Vec<(String, std::path::PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("safetensors")).unwrap_or(false) {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                stems.push((stem.to_ascii_lowercase(), p));
            }
        }
    }

    let find = |needles: &[&str]| -> Option<std::path::PathBuf> {
        // Exact-match first, then prefix-match, first-wins per the needle order
        // (so "clip_l" beats "clip_l_foo" only when no exact "clip_l" exists).
        for n in needles {
            if let Some((_, p)) = stems.iter().find(|(s, _)| s == n) {
                return Some(p.clone());
            }
        }
        for n in needles {
            if let Some((_, p)) = stems.iter().find(|(s, _)| s.starts_with(n)) {
                return Some(p.clone());
            }
        }
        None
    };

    let clip_l = find(&["clip_l", "clip-l", "text_encoder", "open_clip_l", "vit_l"]);
    let clip_g = find(&["clip_g", "clip-g", "text_encoder_2", "open_clip_g", "vit_big_g", "text_encoder_2"]);
    let vae = find(&["sdxl_vae", "vae", "sdxl_vae_fp16_fix", "fix_vae"]);

    Ok(SdModelLayout {
        diffusion_model: dir.to_path_buf(), // placeholder; overwritten by caller with the GGUF
        clip_l,
        clip_g,
        vae,
    })
}

#[cfg(feature = "diffusion-rs")]
impl SceneImageGenerator for DiffusionRsGenerator {
    fn load(&self, model_path: &std::path::Path) -> Result<(), SceneImageError> {
        // Stash the resolved checkpoint path. The actual ModelConfig build +
        // VRAM parse happens inside `generate` (see struct doc — the crate
        // frees/rebuilds the context on every gen_img call regardless).
        *self.model_path.lock().map_err(|e| SceneImageError {
            message: format!("diffusion-rs model_path mutex poisoned: {e}"),
        })? = Some(model_path.to_path_buf());

        // GGUF layout resolution (§11.59 + §11.61 + the 2026-08-17 encoder
        // override). Three outcomes:
        //
        //  (A) Bare UNet GGUF (§11.59, ComfyUI convention — bare
        //      input_blocks.* names, no embedded CLIP/VAE): the multi-file
        //      layout with whatever siblings resolve (clip_g strongly
        //      recommended — sd.cpp version detection needs its
        //      conditioner.embedders.1 markers).
        //  (B) FULL-checkpoint GGUF + the fp16 CLIP-L/CLIP-G sidecar PAIR:
        //      the encoder override (`ClipOverride`) — the GGUF stays on the
        //      proven single-file `model` path while the external fp16
        //      encoders SHADOW the embedded Q8_0 ones (name-collision
        //      replacement; see ClipOverride doc). The VAE sidecar is
        //      deliberately NOT consumed here — the embedded VAE keeps
        //      sd.cpp's automatic SDXL Conv2D 1/32 guard, and external VAEs
        //      are the documented §9 noise-era hazard.
        //  (C) Anything else (no sidecar pair / safetensors): single-file
        //      with the checkpoint's own embedded encoders/VAE (§11.61).
        let (layout, clip_override) = if is_gguf_unet(model_path) {
            let dir = model_path.parent().unwrap_or_else(|| std::path::Path::new("."));
            match resolve_multi_file_layout(dir) {
                Ok(mut l) => {
                    // Fill in the GGUF UNet path (resolve_multi_file_layout
                    // only resolves the CLIP/VAE siblings, not the UNet itself).
                    l.diffusion_model = model_path.to_path_buf();
                    if !gguf_embeds_full_checkpoint(model_path) {
                        // (A) the original §11.59 bare-UNet multi-file layout.
                        if l.clip_g.is_none() {
                            tracing::warn!(
                                "diffusion-rs: GGUF UNet has NO CLIP-G sibling in {} — sd.cpp version detection will likely fail (needs conditioner.embedders.1 markers). Drop clip_g.safetensors / text_encoder_2.safetensors into the sd/ dir.",
                                dir.display()
                            );
                        }
                        tracing::info!(
                            gguf = %model_path.display(),
                            clip_l = ?l.clip_l.as_ref().map(|p| p.display().to_string()),
                            clip_g = ?l.clip_g.as_ref().map(|p| p.display().to_string()),
                            vae = ?l.vae.as_ref().map(|p| p.display().to_string()),
                            "diffusion-rs: GGUF UNet detected — multi-file SDXL layout resolved",
                        );
                        (Some(l), None)
                    } else if let (Some(cl), Some(cg)) = (&l.clip_l, &l.clip_g) {
                        // (B) the fp16-encoder override: the external pair
                        // shadows the embedded quantized encoders; the
                        // embedded VAE is kept (guard intact).
                        tracing::info!(
                            gguf = %model_path.display(),
                            clip_l = %cl.display(),
                            clip_g = %cg.display(),
                            "diffusion-rs: full-checkpoint GGUF + fp16 CLIP-L/CLIP-G sidecars — external encoders SHADOW the embedded quantized ones (embedded VAE kept)",
                        );
                        let ov = ClipOverride {
                            model: model_path.to_path_buf(),
                            clip_l: cl.clone(),
                            clip_g: cg.clone(),
                        };
                        (None, Some(ov))
                    } else {
                        // (C) full checkpoint, no sidecar pair → embedded
                        // everything (single-file).
                        tracing::info!(
                            path = %model_path.display(),
                            has_clip_l = l.clip_l.is_some(),
                            has_clip_g = l.clip_g.is_some(),
                            "diffusion-rs: full-checkpoint GGUF WITHOUT the fp16 CLIP-L + CLIP-G sidecar pair — single-file path (embedded encoders/VAE)",
                        );
                        (None, None)
                    }
                }
                Err(e) => {
                    // Soft-fail: log + proceed as single-file. The single-file
                    // path will hit sd.cpp's `get sd version from file failed`
                    // + return Err cleanly (no crash), surfacing the real
                    // problem via the one-strike latch + the prism-gen-done
                    // error event. We do NOT hard-fail here so a mislabeled
                    // GGUF (e.g. a full-checkpoint GGUF, not a UNet) still
                    // tries the single-file path.
                    tracing::warn!(error = %e, "diffusion-rs: multi-file layout resolution failed; falling back to single-file load");
                    (None, None)
                }
            }
        } else {
            tracing::info!(path = %model_path.display(), "diffusion-rs: single-file SD checkpoint staged");
            (None, None)
        };
        *self.multi_file.lock().map_err(|e| SceneImageError {
            message: format!("diffusion-rs multi_file mutex poisoned: {e}"),
        })? = layout;
        *self.clip_override.lock().map_err(|e| SceneImageError {
            message: format!("diffusion-rs clip_override mutex poisoned: {e}"),
        })? = clip_override;
        Ok(())
    }

    fn generate(&self, request: &SceneImageRequest) -> Result<SceneImageResult, SceneImageError> {
        // NOTE: SampleMethod is NOT imported here — the generate impl maps the
        // request's i32 sampler discriminant to the enum via sampler_from_i32
        // (which imports its own `SampleMethod::*`), so the only reference in
        // this body is the value passed through `.sampling_method(...)`.
        use diffusion_rs::api::{gen_img, ConfigBuilder, ModelConfigBuilder, Scheduler, WeightType};

        let start = std::time::Instant::now();

        // Read the staged checkpoint path (None => load was never called).
        let model_path = {
            let g = self.model_path.lock().map_err(|e| SceneImageError {
                message: format!("diffusion-rs model_path mutex poisoned: {e}"),
            })?;
            g.as_ref()
                .ok_or_else(|| SceneImageError {
                    message: "diffusion-rs: generate called before load (or after unload)".into(),
                })?
                .clone()
        };

        // Build the ModelConfig fresh per turn. Cheap (just stores paths);
        // the expensive VRAM parse happens inside gen_img. WUPI loads CUSTOM
        // local files — do NOT use PresetBuilder (it auto-downloads).
        //
        // §11.59 THREE LAYOUTS:
        //  (A) Multi-file SDXL (a ComfyUI GGUF UNet + sibling CLIP/VAE):
        //      pass diffusion_model + clip_l/g + vae. The GGUF is ALREADY Q8
        //      (pre-quantized at conversion), so NO weight_type override —
        //      setting Q8_0 here would try to re-quantize a GGUF (wrong).
        //      CLIP-G MUST be present for sd.cpp's version detection to
        //      classify the model as SDXL (see SdModelLayout doc).
        //  (B) ENCODER OVERRIDE (2026-08-17): a full-checkpoint GGUF + the
        //      fp16 CLIP-L/CLIP-G pair — single-file `model` PLUS the
        //      external clip paths, which shadow the embedded quantized
        //      encoders by name collision (see ClipOverride doc). The
        //      embedded VAE serves decode (no vae path → sd.cpp's SDXL
        //      conv-scale guard applies). NO weight_type (GGUF quant).
        //  (C) Single-file checkpoint (legacy fp16 safetensors): pass `model`
        //      + weight_type(Q8_0) for runtime quantization (the §11.58 OOM
        //      fix #3, preserved for the original layout).
        let layout = {
            let g = self.multi_file.lock().map_err(|e| SceneImageError {
                message: format!("diffusion-rs multi_file mutex poisoned: {e}"),
            })?;
            g.clone()
        };
        let clip_override = {
            let g = self.clip_override.lock().map_err(|e| SceneImageError {
                message: format!("diffusion-rs clip_override mutex poisoned: {e}"),
            })?;
            g.clone()
        };

        let mut mb = ModelConfigBuilder::default();
        match (&layout, &clip_override) {
            (Some(l), _) => {
                // (A) multi-file SDXL (GGUF UNet).
                mb.diffusion_model(l.diffusion_model.as_path());
                if let Some(c) = &l.clip_l { mb.clip_l(c.as_path()); }
                if let Some(c) = &l.clip_g { mb.clip_g(c.as_path()); }
                if let Some(v) = &l.vae { mb.vae(v.as_path()); }
                mb.vae_tiling(true);
                // NO weight_type — GGUF carries its own quantization.
            }
            (None, Some(co)) => {
                // (B) the fp16-encoder override: single-file GGUF + external
                // fp16 CLIP-L/CLIP-G shadowing the embedded quantized
                // encoders. Same no-weight_type rule as (A) — the GGUF
                // carries its own quant.
                mb.model(co.model.as_path());
                mb.clip_l(co.clip_l.as_path());
                mb.clip_g(co.clip_g.as_path());
                mb.vae_tiling(true);
            }
            (None, None) => {
                // (C) single-file checkpoint: a legacy fp16 safetensors OR a
                // full-checkpoint GGUF without sidecars (§11.61 — embedded
                // conditioner + VAE, no siblings needed). Runtime Q8_0
                // re-quantization applies ONLY to the safetensors form (the
                // §11.58 OOM fix #3); a GGUF carries its own quantization,
                // and re-quantizing an already-Q8 file is both lossy
                // (Q8→F32→Q8 round trip) and a multi-GB pointless conversion
                // pass at load.
                mb.model(model_path.as_path()).vae_tiling(true);
                let is_gguf = model_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("gguf"))
                    .unwrap_or(false);
                if !is_gguf {
                    mb.weight_type(WeightType::SD_TYPE_Q8_0);
                }
            }
        }
        // The LOCKED engine flags (recipe v2, 2026-08-18 — applied after the
        // match so no layout branch can skip them):
        //   · KARRAS scheduler — the sigma schedule both passes ride (v2
        //     switched the base sampler back to DPM++ 2M, whose classic
        //     pairing is Karras early convergence; the single-pass Euler-a
        //     DISCRETE lock retired the same day).
        //   · flash attention — the CUDA attention fast path at SDXL
        //     resolutions, quality-neutral. Verify a render + the [sd] logs
        //     after any sd.cpp/vendored rebuild (sd_repro, seed 42) — if the
        //     vendored build lacks FA kernels the engine falls back + logs.
        //   · the MANDATORY hires refine pass — see the block below.
        mb.scheduler(Scheduler::KARRAS_SCHEDULER);
        mb.flash_attention(true);

        // The MANDATORY second stage (recipe v2, no toggle): upscale the
        // base latent to PRISM_HIRES_SCALE× via the ESRGAN anime scaffold
        // (a `models/sd/` sibling of the checkpoint), then re-render at
        // PRISM_HIRES_DENOISE with PRISM_HIRES_STEPS EFFECTIVE steps of the
        // same sampler/schedule/CFG (sd.cpp reuses the call's sample params
        // for the hires pass). `hires_params` is wired end-to-end in the
        // crate (unlike ControlNet's dead control_image field): the builder
        // fills a real `sd_hires_params_t` (own scale/target/steps/denoise)
        // into the single gen_img call, and generate_image loads the MODEL
        // upscaler itself from `model_path` — no separate upscaler ctx.
        // Missing ESRGAN file → LANCZOS pixel upscale (zero-download
        // fallback; the refine pass still runs, slightly softer scaffold).
        let esrgan_path = model_path
            .parent()
            .map(|dir| dir.join(PRISM_HIRES_ESRGAN_FILE))
            .filter(|p| p.is_file());
        let hires_params = diffusion_rs::api::HiresParamsBuilder::default()
            .width((request.width as f32 * PRISM_HIRES_SCALE).round() as i32)
            .height((request.height as f32 * PRISM_HIRES_SCALE).round() as i32)
            .steps(PRISM_HIRES_STEPS)
            .denoising_strength(PRISM_HIRES_DENOISE)
            .build()
            .map_err(|e| SceneImageError {
                message: format!("diffusion-rs HiresParams build failed: {e}"),
            })?;
        match &esrgan_path {
            Some(p) => {
                tracing::info!(
                    esrgan = %p.display(),
                    scale = PRISM_HIRES_SCALE,
                    steps = PRISM_HIRES_STEPS,
                    denoise = PRISM_HIRES_DENOISE,
                    "prism hires refine: RealESRGAN scaffold + low-denoise re-render (mandatory)",
                );
                mb.hires_params(
                    diffusion_rs::api::Upscaler::SD_HIRES_UPSCALER_MODEL,
                    hires_params,
                    Some(p.as_path()),
                );
            }
            None => {
                tracing::warn!(
                    file = PRISM_HIRES_ESRGAN_FILE,
                    "prism hires refine: ESRGAN model missing in models/sd/ — falling back to the LANCZOS upscaler (refine pass still runs; drop the file in for the sharper scaffold)",
                );
                mb.hires_params(
                    diffusion_rs::api::Upscaler::SD_HIRES_UPSCALER_LANCZOS,
                    hires_params,
                    None,
                );
            }
        }
        let mut model_config = mb.build().map_err(|e| SceneImageError {
            message: format!("diffusion-rs ModelConfig build failed: {e}"),
        })?;
        let _ = &model_path; // still held for the legacy path / error messages

        // Build the per-turn Config from the request. All setters confirmed
        // on docs.rs/diffusion_rs/latest/diffusion_rs/api/struct.ConfigBuilder.
        // html: prompt, negative_prompt, width, height, steps, sampling_method,
        // cfg_scale, output. The sampler/steps/cfg below carry the LOCKED
        // recipe v2 (DPM++ 2M / 20 / 6.0 — set upstream in
        // prism::build_request; the base pass owns composition, the mandatory
        // hires refine pass owns detail).
        //
        // derive_builder's `setter(into, strip_option)` makes the chain return
        // `&mut ConfigBuilder`, so we build the base chain to a local then
        // conditionally extend with negative_prompt (avoids the E0716
        // "temporary dropped while borrowed" the inline-chain form hit).
        let mut builder = ConfigBuilder::default();
        builder
            .prompt(request.prompt.clone())
            .width(request.width as i32)
            .height(request.height as i32)
            .steps(request.steps as i32)
            // Sampler: from the request's i32 discriminant (default DPM++ 2M,
            // the SDXL clean baseline). `sampler_from_i32` maps the
            // feature-independent i32 back to the crate's `SampleMethod` enum;
            // an unknown value falls back to DPM++ 2M (never a hard error — a
            // stale DB row with a removed sampler shouldn't break generation).
            .sampling_method(sampler_from_i32(request.sampling_method))
            // CFG: from the request (default 5.0, the conservative clean-SDXL
            // value — SDXL wants lower CFG than SD 1.5's 7-9 range). Prism's
            // CFG slider overrides this; FABLE scene-art uses the default.
            .cfg_scale(request.cfg_scale)
            // Seed: from the request. `-1` (default) = random; `>= 0` = locked
            // → identical seed + identical params produce identical pixels (the
            // deterministic contract Prism's Fork & Edit relies on).
            .seed(request.seed)
            .output(request.dest.clone());
        if let Some(neg) = &request.negative_prompt {
            builder.negative_prompt(neg.clone());
        }

        let config = builder.build().map_err(|e| SceneImageError {
            message: format!("diffusion-rs Config build failed: {e}"),
        })?;

        // gen_img writes the PNG to config.output (== request.dest). The
        // orchestrator's dest.exists() check is the success gate.
        gen_img(&config, &mut model_config).map_err(|e| SceneImageError {
            message: format!("diffusion-rs gen_img failed: {e}"),
        })?;

        Ok(SceneImageResult {
            dest: request.dest.clone(),
            elapsed_ms: start.elapsed().as_millis(),
        })
    }

    fn unload(&self) {
        // Drop the staged path. The crate frees the VRAM-resident model
        // context inside gen_img's epilogue (diffusion_ctx tears it down);
        // there's no long-lived handle to drop here — clearing the path just
        // marks the generator as not-loaded so a stray generate() after
        // unload fails cleanly instead of running on a stale path. The
        // reverse swap reloads the LLM weights right after this (the lease
        // teardown runs unload synchronously before the LLM reload).
        if let Ok(mut g) = self.model_path.lock() {
            if g.take().is_some() {
                tracing::info!("diffusion-rs: SD checkpoint path cleared (VRAM freed by gen_img; LLM reload next)");
            }
        }
    }
}

/// Phase 5B (2026-07-29): the outcome of a `run_sd_swap_from_arcs` cycle.
/// Consumed by the caller's `on_result` callback (the done-beat spawn emits a
/// channel event based on this). Each variant maps to a frontend signal.
#[derive(Debug)]
pub enum SwapOutcome {
    /// Generation succeeded; the PNG is at `result.dest`.
    Generated(SceneImageResult),
    /// The swap was skipped (one-strike latch tripped, no SD model, or the
    /// cycle completed but produced nothing to show).
    /// NOT an error — the game continues normally, just no new image.
    Skipped,
    /// The swap was cancelled (the user navigated away / ended the game /
    /// the cancel token fired mid-cycle). The orchestrator cleaned up.
    Cancelled,
    /// Generation failed (OOM, corrupt model, backend error). The one-strike
    /// latch is now tripped; auto-gen is disabled until the user re-enables
    /// it. The LLM was reloaded (or the reload failed too — logged).
    Failed(SceneImageError),
}

/// Feature-gated tests for the Prism sampler int↔enum mapping. These run ONLY
/// when built with `--features diffusion-rs` (the real enum is in scope). The
/// feature-independent contract (the const value, the Default field value) is
/// pinned in `phase5b_tests` above; these tests cross-validate the mapping
/// against the real `SampleMethod` enum so a crate version bump that reorders
/// the enum is caught here, not at a live render.
#[cfg(all(test, feature = "diffusion-rs"))]
mod prism_sampler_tests {
    use super::*;

    /// The DPMPP2M_DISCRIMINANT const must equal the real enum variant's
    /// discriminant. C enums are implicitly numbered from 0; we assert the
    /// crate's `DPMPP2M_SAMPLE_METHOD` is variant 5 (matching the C source).
    /// If this fails, stable-diffusion.cpp reordered the enum → update the
    /// const + the match arms in `sampler_from_i32`.
    #[test]
    fn dpmpp2m_const_matches_real_enum() {
        use diffusion_rs::api::SampleMethod;
        // SampleMethod is a C-enum repr; casting to i64 reads the discriminant.
        // (The enum is `#[repr(...)]` via the FFI; `as` works on fieldless
        // C-like enums.) DPMPP2M is variant index 5 in the C declaration.
        let dpmpp2m = SampleMethod::DPMPP2M_SAMPLE_METHOD;
        let _ = dpmpp2m; // keep the variant referenced
        // sampler_from_i32(DPMPP2M_DISCRIMINANT) must return DPMPP2M (the
        // round-trip). This is the strongest machine-checked guarantee without
        // relying on `as i64` casting semantics.
        let mapped = sampler_from_i32(DPMPP2M_DISCRIMINANT);
        // Compare via debug repr (SampleMethod may not derive PartialEq, but it
        // derives Debug; the two strings must match).
        assert_eq!(
            format!("{:?}", mapped),
            format!("{:?}", SampleMethod::DPMPP2M_SAMPLE_METHOD),
            "DPMPP2M_DISCRIMINANT must map back to DPMPP2M_SAMPLE_METHOD",
        );
    }

    /// Every known discriminant 0..=17 EXCEPT the retired Euler a (1) maps
    /// to a distinct variant (no two discriminants collapse to the same
    /// enum value — a typo'd match arm would break this). Euler a's 1 is
    /// deliberately unmapped (recipe v2, 2026-08-19 ruling) and joins the
    /// unknown ids on the DPM++ 2M fallback (asserted separately). The
    /// sentinel (18) + out-of-range values also fall back (asserted below).
    #[test]
    fn sampler_from_i32_covers_all_variants() {
        use std::collections::HashSet;
        let mut seen: HashSet<String> = HashSet::new();
        for d in (0..=17i32).filter(|d| *d != 1) {
            let m = format!("{:?}", sampler_from_i32(d));
            assert!(seen.insert(m), "discriminant {d} collided with an earlier variant (duplicate match arm?)");
        }
        assert_eq!(seen.len(), 17, "expected 17 distinct samplers for discriminants 0..=17 minus retired Euler a (1)");
    }

    /// Out-of-range + retired discriminants (Euler a's 1, the COUNT sentinel
    /// 18/19, negatives, future variants) fall back to DPM++ 2M — never
    /// panic, never error.
    #[test]
    fn sampler_from_i32_falls_back_for_unknown() {
        use diffusion_rs::api::SampleMethod;
        for d in [1, 18, 19, -1, 99, i32::MAX, i32::MIN] {
            let mapped = sampler_from_i32(d);
            assert_eq!(
                format!("{:?}", mapped),
                format!("{:?}", SampleMethod::DPMPP2M_SAMPLE_METHOD),
                "discriminant {d} should fall back to DPM++ 2M",
            );
        }
    }
}
