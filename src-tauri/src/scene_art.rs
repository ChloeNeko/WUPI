//! Scene-art prompt composition (Fable Phase 5A, 2026-07-29).
//!
//! The deterministic recipe that feeds the (Phase 5B) Stable Diffusion image
//! generator. Pure functions over [`crate::schema::WorldSchema`] — zero SD
//! dependency, fully unit-testable in 5A, consumed by the generator in 5B.
//!
//! This is the Multihog `generateLocationImagePrompt` recipe (portraits.js:1887)
//! ported onto Rust-owned typed state instead of a regex name-scan of recent
//! output. The improvement over the source: every layer of the prompt is built
//! from a Rust-authoritative struct (`TravelGraph` for the macro, `Weather` +
//! `WorldClock` for the environment, `presences` for the micro subjects) rather
//! than a fragile parse of free-form prose — so the prompt is deterministic +
//! the anti-teleport whitelist (`present:`) is the exact same source the image
//! composes its subjects from. The image cannot show an absent NPC because the
//! prompt is built from the same `presences` Vec that gates the narrator.
//!
//! Phase 5A ships ONLY the composer (no SD backend). The 5B integration wires
//! this output to a `SceneImageGenerator` trait; the composer itself is stable
//! + frozen by unit tests here.

use crate::schema::WorldSchema;

/// Minutes per in-world day (mirrors `WorldClock`'s 1440 convention).
const MINUTES_PER_DAY: i64 = 1440;

/// Compose a single deterministic image prompt from the Rust-owned world
/// state. Three layers, in order:
///
/// 1. **Macro** — the current node's name + setting (the location aesthetic).
/// 2. **Environment** — weather condition (suppressed indoors, mirroring the
///    §11.45 `current_is_indoor()` gate) + time-of-day derived from the clock.
/// 3. **Micro (subjects)** — the aggregated presence stances ("Mara standing
///    by the bar, arms crossed; Corin tuning a lute").
///
/// Returns an empty `String` when there is no current node (nothing to depict
/// — the camera has no place to be). The caller (Phase 5B) treats empty as
/// "skip this turn's image gen."
///
/// The format is deliberately a single comma-separated phrase (not a JSON
/// blob or nested structure) — image-gen models train on natural-language
/// prompts, and the SD backend (5B) may prepend style/quality tokens. This
/// fn owns the SCENE CONTENT only; the backend owns the aesthetic wrapper.
pub fn compose_scene_prompt(s: &WorldSchema) -> String {
    let cur = match s.travel_graph.current() {
        Some(node) => node,
        None => return String::new(),
    };

    let mut parts: Vec<String> = Vec::new();

    // ── Macro: location aesthetic ──────────────────────────────────────────
    let mut macro_phrase = String::new();
    if !cur.name.trim().is_empty() {
        macro_phrase.push_str(cur.name.trim());
    } else {
        // Fall back to the bare id if the node has no diegetic name (defensive).
        macro_phrase.push_str(&cur.id);
    }
    // Setting hint ("indoor" / "outdoor") — guides the image's lighting +
    // framing. Empty setting is omitted (the model infers from the name).
    let setting = cur.setting.trim();
    if !setting.is_empty() {
        macro_phrase.push_str(", ");
        macro_phrase.push_str(setting);
        macro_phrase.push_str(" setting");
    }
    parts.push(macro_phrase);

    // ── Environment: weather (outdoors only) + time-of-day ────────────────
    let mut env_bits: Vec<String> = Vec::new();
    // Weather renders only outdoors (the §11.45 gate — a windowless cellar
    // doesn't show rain). Mirrors `render_for_prompt`'s weather suppression.
    if !s.travel_graph.current_is_indoor() {
        if let Some(cond) = s.weather.render_line() {
            env_bits.push(cond);
        }
    }
    // Time-of-day from the clock (dawn / morning / midday / afternoon /
    // evening / night). Only when the clock is set; dormant otherwise.
    if s.world_clock.is_set() {
        if let Some(tod) = time_of_day_label(s.world_clock.current_minutes) {
            env_bits.push(tod);
        }
    }
    if !env_bits.is_empty() {
        parts.push(env_bits.join(", "));
    }

    // ── Micro: on-camera subjects (the presence whitelist) ────────────────
    // Only NPCs the Tracker asserted this turn (or within grace). This is the
    // load-bearing anti-teleport property for image gen: an absent NPC cannot
    // appear in the image because they're absent from the prompt's subject
    // list. Each subject is "Name (stance)"; empty-stance subjects render as
    // bare names. Joined by "; " (semicolons — stances contain commas).
    if !s.presences.is_empty() {
        let subjects: Vec<String> = s
            .presences
            .iter()
            .map(|p| {
                let name = if p.name.trim().is_empty() {
                    p.npc_id.clone()
                } else {
                    p.name.clone()
                };
                if p.stance.trim().is_empty() {
                    name
                } else {
                    format!("{name} ({})", p.stance.trim())
                }
            })
            .collect();
        parts.push(subjects.join("; "));
    }

    parts.join(", ")
}

/// Map epoch-minutes → a coarse time-of-day label for the image prompt.
/// Derived purely from `minutes % MINUTES_PER_DAY` (the in-world clock's
/// convention). Returns `None` when the clock is at exactly minute 0 of day 1
/// (the dormant baseline — no time established yet). The buckets are coarse
/// (6) because image-gen models respond to broad lighting cues, not exact
/// hours: "night" vs "dawn" is the actionable distinction.
fn time_of_day_label(current_minutes: i64) -> Option<String> {
    if current_minutes <= 0 {
        return None;
    }
    let into_day = current_minutes.rem_euclid(MINUTES_PER_DAY); // minutes since midnight
    // Hour = into_day / 60. Buckets:
    //   00:00–05:59  night       (6h)
    //   06:00–08:59  dawn        (3h)
    //   09:00–11:59  morning     (3h)
    //   12:00–15:59  midday      (4h)
    //   16:00–19:59  afternoon   (4h)
    //   20:00–23:59  evening     (4h)
    let label = match into_day / 60 {
        0..=5 => "night",
        6..=8 => "dawn",
        9..=11 => "morning",
        12..=15 => "midday",
        16..=19 => "afternoon",
        _ => "evening",
    };
    Some(label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        Node, NpcEntry, NpcRegistry, Presence, TravelGraph, Weather, WorldClock, PRESENCE_GRACE_RESET,
    };

    /// No current node → empty prompt (nothing to depict). The caller treats
    /// empty as "skip image gen this turn."
    #[test]
    fn compose_returns_empty_when_no_current_node() {
        let s = WorldSchema::default();
        assert_eq!(compose_scene_prompt(&s), "");
    }

    /// Macro layer: node name + setting. No weather (no clock), no subjects
    /// (no presences) → just the location phrase.
    #[test]
    fn compose_emits_macro_layer_from_current_node() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "tavern".into(),
                name: "The Rusty Lantern Tavern".into(),
                neighbors: vec![],
                setting: "indoor".into(),
            }],
            current_node: Some("tavern".into()),
        };
        let prompt = compose_scene_prompt(&s);
        assert!(
            prompt.contains("The Rusty Lantern Tavern"),
            "macro layer must include the node name: {prompt}"
        );
        assert!(
            prompt.contains("indoor setting"),
            "macro layer must include the setting hint: {prompt}"
        );
    }

    /// Weather is suppressed indoors (the §11.45 gate). A tavern (indoor) with
    /// weather set must NOT include the condition in the prompt.
    #[test]
    fn compose_suppresses_weather_indoors() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "tavern".into(),
                name: "Tavern".into(),
                neighbors: vec![],
                setting: "indoor".into(),
            }],
            current_node: Some("tavern".into()),
        };
        s.weather = Weather {
            condition: "heavy rain".into(),
            started_at_minutes: 100,
        };
        let prompt = compose_scene_prompt(&s);
        assert!(!prompt.contains("heavy rain"), "indoor scene must suppress weather: {prompt}");
    }

    /// Weather renders outdoors. A market_square (outdoor) with weather set
    /// includes the condition.
    #[test]
    fn compose_includes_weather_outdoors() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "market_square".into(),
                name: "Market Square".into(),
                neighbors: vec![],
                setting: "outdoor".into(),
            }],
            current_node: Some("market_square".into()),
        };
        s.weather = Weather {
            condition: "clear skies".into(),
            started_at_minutes: 100,
        };
        let prompt = compose_scene_prompt(&s);
        assert!(prompt.contains("clear skies"), "outdoor scene must include weather: {prompt}");
    }

    /// Time-of-day derived from the clock. Day 3, 14:00 → "midday".
    #[test]
    fn compose_includes_time_of_day_from_clock() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "tavern".into(),
                name: "Tavern".into(),
                neighbors: vec![],
                setting: "indoor".into(),
            }],
            current_node: Some("tavern".into()),
        };
        // Day 3 = (3-1)*1440 = 2880 baseline; 14:00 = 14*60 = 840 → 3720.
        s.world_clock = WorldClock {
            current_minutes: 3720,
            last_tick_minutes: 0,
        };
        let prompt = compose_scene_prompt(&s);
        assert!(prompt.contains("midday"), "14:00 must map to midday: {prompt}");
    }

    /// Micro layer: the presence subjects, joined by "; " (semicolons since
    /// stances contain commas). Empty-stance subjects render as bare names.
    #[test]
    fn compose_includes_presence_subjects() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "tavern".into(),
                name: "Tavern".into(),
                neighbors: vec![],
                setting: "indoor".into(),
            }],
            current_node: Some("tavern".into()),
        };
        s.presences = vec![
            Presence {
                npc_id: "mara".into(),
                name: "Mara".into(),
                stance: "standing by the bar, arms crossed".into(),
                ttl: PRESENCE_GRACE_RESET,
            },
            Presence {
                npc_id: "corin".into(),
                name: "Corin".into(),
                stance: String::new(),
                ttl: PRESENCE_GRACE_RESET,
            },
        ];
        let prompt = compose_scene_prompt(&s);
        assert!(
            prompt.contains("Mara (standing by the bar, arms crossed)"),
            "subject with stance: {prompt}"
        );
        // Corin has an empty stance → bare name, no parens (NOT "Corin (...)").
        assert!(
            prompt.contains("Corin"),
            "bare-name subject present: {prompt}"
        );
        assert!(
            !prompt.contains("Corin ("),
            "empty-stance subject must not get parens: {prompt}"
        );
        assert!(
            prompt.contains("; "),
            "subjects joined by '; ' (stances contain commas): {prompt}"
        );
    }

    /// The full three-layer composition in order: macro, environment, micro.
    #[test]
    fn compose_full_three_layer_prompt() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "market_square".into(),
                name: "Ashford Market Square".into(),
                neighbors: vec![],
                setting: "outdoor".into(),
            }],
            current_node: Some("market_square".into()),
        };
        s.weather = Weather {
            condition: "light rain".into(),
            started_at_minutes: 100,
        };
        // Day 1, 21:00 = 21*60 = 1260.
        s.world_clock = WorldClock {
            current_minutes: 1260,
            last_tick_minutes: 0,
        };
        s.presences = vec![Presence {
            npc_id: "mara".into(),
            name: "Mara".into(),
            stance: "haggling with a vendor".into(),
            ttl: PRESENCE_GRACE_RESET,
        }];
        let prompt = compose_scene_prompt(&s);
        // Macro first, then environment, then micro — check ordering.
        let macro_idx = prompt.find("Ashford Market Square").unwrap();
        let env_idx = prompt.find("light rain").unwrap();
        let time_idx = prompt.find("evening").unwrap();
        let micro_idx = prompt.find("Mara (haggling").unwrap();
        assert!(macro_idx < env_idx, "macro before environment: {prompt}");
        assert!(env_idx < time_idx, "weather before time-of-day: {prompt}");
        assert!(time_idx < micro_idx, "environment before subjects: {prompt}");
    }

    /// time_of_day_label boundary checks (the 6 buckets).
    #[test]
    fn time_of_day_label_buckets() {
        // Dormant baseline (minute 0) → None.
        assert_eq!(time_of_day_label(0), None);
        // Night: 00:00–05:59. Day 1, 03:00 = 180.
        assert_eq!(time_of_day_label(180), Some("night".into()));
        // Dawn: 06:00–08:59. Day 1, 07:00 = 420.
        assert_eq!(time_of_day_label(420), Some("dawn".into()));
        // Morning: 09:00–11:59. Day 1, 10:00 = 600.
        assert_eq!(time_of_day_label(600), Some("morning".into()));
        // Midday: 12:00–15:59. Day 1, 14:00 = 840.
        assert_eq!(time_of_day_label(840), Some("midday".into()));
        // Afternoon: 16:00–19:59. Day 1, 18:00 = 1080.
        assert_eq!(time_of_day_label(1080), Some("afternoon".into()));
        // Evening: 20:00–23:59. Day 1, 22:00 = 1320.
        assert_eq!(time_of_day_label(1320), Some("evening".into()));
        // Wraps correctly across day boundary: Day 2, 00:30 = 1440 + 30 = 1470.
        assert_eq!(time_of_day_label(1470), Some("night".into()));
    }

    /// The anti-teleport property for image gen: an absent NPC does not appear
    /// in the prompt's subject list. This test pins that composing from the
    /// registry (full cast) vs presences (on-camera only) yields different
    /// prompts — the registry alone must never leak into the image prompt.
    #[test]
    fn compose_uses_presences_not_registry() {
        let mut s = WorldSchema::default();
        s.travel_graph = TravelGraph {
            nodes: vec![Node {
                id: "tavern".into(),
                name: "Tavern".into(),
                neighbors: vec![],
                setting: "indoor".into(),
            }],
            current_node: Some("tavern".into()),
        };
        // Registry has 2 NPCs; only 1 is present (on-camera). The absent one
        // must NOT appear in the image prompt.
        s.npc_registry = NpcRegistry {
            entries: vec![
                NpcEntry {
                    id: "mara".into(),
                    name: "Mara".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                },
                NpcEntry {
                    id: "corin".into(),
                    name: "Corin".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                },
            ],
        };
        s.presences = vec![Presence {
            npc_id: "mara".into(),
            name: "Mara".into(),
            stance: "at the bar".into(),
            ttl: PRESENCE_GRACE_RESET,
        }];
        let prompt = compose_scene_prompt(&s);
        assert!(prompt.contains("Mara"), "on-camera NPC appears: {prompt}");
        assert!(
            !prompt.contains("Corin"),
            "absent NPC must NOT appear in image prompt (anti-teleport): {prompt}"
        );
    }
}

// ===========================================================================
// Phase 5B: the SceneImageGenerator trait (backend-agnostic seam)
// ===========================================================================
//
// This is the trait the actual Stable Diffusion backend (diffusion-rs, added
// via Cargo.toml in Phase 5B) implements. It's defined HERE, in scene_art.rs,
// so the swap-lock + the done-beat wiring + the one-strike failure latch can
// all be built + unit-tested against a stub backend BEFORE the diffusion-rs
// dependency is added (which carries the ~30-min one-time CUDA compile).
//
// The contract intentionally separates LIFECYCLE (load/unload — VRAM-heavy,
// gated by the swap-lock's `ContextRole::Sd` lease) from GENERATION
// (load is already done; just run the model). This mirrors how the LLM
// engines split spawn (load) from generate_turn. The caller acquires the
// `ContextRole::Sd` lease, loads, generates, then the lease drops and the
// teardown unloads (evicting the weights for the reverse swap).

use std::path::PathBuf;

/// A request to generate one scene image. Built by the caller from
/// `compose_scene_prompt`'s output + generation params (the model path, the
/// output destination, sampler knobs). Owned + Send so it can cross the
/// detached `tokio::spawn` boundary (the SD swap runs off the turn's hot path).
#[derive(Debug, Clone)]
pub struct SceneImageRequest {
    /// The composed prompt (from `compose_scene_prompt`). Already
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
}

impl Default for SceneImageRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            dest: PathBuf::new(),
            negative_prompt: None,
            width: 768,
            height: 432,
            steps: 25,
            model_path: PathBuf::new(),
        }
    }
}

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

    /// The request type defaults to sane SD 1.5 values (768x432, 25 steps).
    /// Pins the defaults so a future tuning change is intentional.
    #[test]
    fn scene_image_request_defaults_are_sane() {
        let req = SceneImageRequest::default();
        assert_eq!(req.width, 768);
        assert_eq!(req.height, 432, "16:9-ish (the stage aspect)");
        assert_eq!(req.steps, 25);
        assert!(req.negative_prompt.is_none());
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
    // Phase 5B completion: when diffusion-rs is added behind a `diffusion-rs`
    // cargo feature, this becomes:
    //   #[cfg(feature = "diffusion-rs")]
    //   { Box::new(DiffusionRsGenerator::new()) }
    //   #[cfg(not(feature = "diffusion-rs"))]
    //   { Box::new(NoopImageGenerator) }
    // Until then, the stub is always returned (the cargo feature doesn't
    // exist yet — adding it is Chloe's build-safety call).
    Box::new(NoopImageGenerator)
}

/// Phase 5B completion: the real Stable Diffusion backend (newfla/diffusion-rs,
/// wrapping leejet/stable-diffusion.cpp). Feature-gated so the default build
/// keeps using `NoopImageGenerator` (no CUDA compile); Chloe's build-safety
/// procedure (docs/phase5b-diffusion-rs-cargo-procedure.md) adds the dep +
/// flips `default_sd_backend()` to return this.
///
/// The trait's load/generate/unload lifecycle maps onto diffusion-rs as:
///   - `load`    → build + hold the `ModelConfig` + `Config` from the user's
///                 checkpoint path (models/sd/*.safetensors). WUPI loads a
///                 CUSTOM path, NOT a Preset (presets auto-download — wrong
///                 for a portable offline app).
///   - `generate` → `api::gen_img(&config, &mut model_config)` writes the PNG.
///   - `unload`  → drop the held state so stable-diffusion.cpp frees its VRAM
///                 (the reverse-swap reloads the LLM weights right after).
///
/// `model_state` is interior-mutable because the orchestrator calls
/// load/generate/unload as separate `&self` methods on the SAME generator
/// instance (the trait is `&self`, not `&mut self`), sequenced across the
/// swap cycle. The Mutex is held only briefly (no contention — the SD lease
/// serializes all SD access).
///
/// FILL-IN: the exact `ConfigBuilder`/`ModelConfigBuilder` field setters are
/// TODO — they weren't exposed in the docs.rs module index at authoring time.
/// After `cargo add diffusion-rs` + `cargo doc --features diffusion-rs`, the
/// builder pages at docs.rs/diffusion_rs/latest/diffusion_rs/api/struct.
/// ConfigBuilder.html + struct.ModelConfigBuilder.html list the setter names
/// for: the model file path, width, height, sample steps, negative prompt,
/// + the sample method. Replace the `todo!()` calls below with those setters.
#[cfg(feature = "diffusion-rs")]
pub struct DiffusionRsGenerator {
    /// The built (config, model_config) pair, held between load + generate so
    /// the model isn't re-parsed each turn. None before `load` + after `unload`.
    model_state: std::sync::Mutex<Option<diffusion_rs::api::Config>>,
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
            model_state: std::sync::Mutex::new(None),
        }
    }
}

#[cfg(feature = "diffusion-rs")]
impl SceneImageGenerator for DiffusionRsGenerator {
    fn load(&self, model_path: &std::path::Path) -> Result<(), SceneImageError> {
        use diffusion_rs::api::{Config, ConfigBuilder, ModelConfig, ModelConfigBuilder};

        // Build the model config from the user's checkpoint. WUPI loads a
        // CUSTOM local file — do NOT use PresetBuilder (it auto-downloads).
        // TODO(diffusion-rs): set the .safetensors/.gguf path + the
        // appropriate diffusion variant. The exact ModelConfigBuilder setter
        // for the file path is on the builder's docs page (see struct doc).
        let model_config: ModelConfig = ModelConfigBuilder::default()
            // .model_path(model_path)  ← TODO: confirm the setter name
            .build()
            .map_err(|e| SceneImageError {
                message: format!("diffusion-rs ModelConfig build failed: {e}"),
            })?;

        // Build the top-level config. The request's width/height/steps/
        // negative_prompt are applied in `generate` (they're per-turn; the
        // loaded model is reused across turns), so load uses SD 1.5 defaults.
        // TODO(diffusion-rs): confirm the ConfigBuilder setters for the
        // model_config, width, height, steps, sample method, output path.
        let config: Config = ConfigBuilder::default()
            // .model_config(model_config)  ← TODO: confirm the setter name
            // .width(768).height(432)      ← TODO: confirm setter names
            // .sample_steps(25)            ← TODO: confirm setter name
            .build()
            .map_err(|e| SceneImageError {
                message: format!("diffusion-rs Config build failed: {e}"),
            })?;

        *self.model_state.lock().map_err(|e| SceneImageError {
            message: format!("diffusion-rs model_state mutex poisoned: {e}"),
        })? = Some(config);
        tracing::info!(path = %model_path.display(), "diffusion-rs: model loaded");
        Ok(())
    }

    fn generate(&self, request: &SceneImageRequest) -> Result<SceneImageResult, SceneImageError> {
        use diffusion_rs::api::gen_img;

        let start = std::time::Instant::now();
        // Take the config out, apply the per-turn request params, run, put it
        // back. (The trait is &self, so we rebuild the Config per turn with the
        // request's prompt/dest. A future optimization keeps the parsed model
        // hot + only swaps the prompt — depends on whether diffusion-rs's
        // Config is cheap to clone, which the API review will reveal.)
        //
        // TODO(diffusion-rs): apply request.prompt, request.width,
        // request.height, request.steps, request.negative_prompt, +
        // request.dest (the output PNG path) via the ConfigBuilder. The
        // current held Config was built with defaults in `load`.
        let mut g = self.model_state.lock().map_err(|e| SceneImageError {
            message: format!("diffusion-rs model_state mutex poisoned: {e}"),
        })?;
        let config = g.take().ok_or_else(|| SceneImageError {
            message: "diffusion-rs: generate called before load (or after unload)".into(),
        })?;

        // TODO(diffusion-rs): the config passed to gen_img must carry this
        // turn's prompt + dest. If ConfigBuilder is the only way to set those,
        // re-build here from the held config + the request fields. The
        // gen_img signature is gen_img(&Config, &mut ModelConfig) per the
        // README — confirm whether ModelConfig is also held or rebuilt.
        let mut model_config_holder = diffusion_rs::api::ModelConfig::default();
        gen_img(&config, &mut model_config_holder).map_err(|e| SceneImageError {
            message: format!("diffusion-rs gen_img failed: {e}"),
        })?;

        // gen_img writes to a path it was configured with (set above via TODO)
        // OR returns bytes — confirm which + write request.dest if it returns
        // bytes. The dest.exists() check in the orchestrator is the gate.
        *g = Some(config);
        Ok(SceneImageResult {
            dest: request.dest.clone(),
            elapsed_ms: start.elapsed().as_millis(),
        })
    }

    fn unload(&self) {
        // Drop the held config so stable-diffusion.cpp frees its VRAM. The
        // reverse swap reloads the LLM weights right after this (the lease
        // teardown runs unload synchronously before the LLM reload).
        if let Ok(mut g) = self.model_state.lock() {
            if g.take().is_some() {
                tracing::info!("diffusion-rs: model unloaded (VRAM freed for LLM reload)");
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
    /// The swap was skipped (one-strike latch tripped, no SD model, empty
    /// compose prompt, or the cycle completed but produced nothing to show).
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
