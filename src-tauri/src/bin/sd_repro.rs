//! Standalone sd.cpp repro harness (§11.59). NOT shipped: the `[[bin]]` entry
//! carries `required-features = ["diffusion-rs"]`, so default dev/release
//! builds never compile it — it exists purely as the isolation tool for
//! debugging stable-diffusion.cpp load/generate failures outside the full
//! WUPI app (no Tauri, no LLM, no swap cycle, no gallery DB).
//!
//! WHAT IT ISOLATES: everything between "one checkpoint file on disk" and
//! "one PNG on disk". Because the process is a bare bin (no webview, no
//! llama weights), a repro here answers "is it sd.cpp / the checkpoint?" in
//! one run — if sd_repro succeeds, any failure inside WUPI's Prism path is
//! in the swap cycle / CRT / IPC layers, not the backend.
//!
//! USAGE (after `npm run sd:dev`-style feature build, from target/debug):
//!   sd_repro <checkpoint-path> [dest-png]
//!     e.g. sd_repro ..\..\models\sd\image.gguf out.png
//!          sd_repro ..\..\models\sd\image.safetensors
//!
//! Defaults: dest = `sd-repro-out.png` in the CWD; prompt/steps/cfg/sampler
//! = the `SceneImageRequest` defaults (the SDXL clean-baseline recipe —
//! this is a load-path debugger, not a prompt playground). Env: RUST_LOG
//! (default info) surfaces the same tracing lines the app emits, incl. the
//! multi-file layout resolution + CLIP-G warnings.
//!
//! On a ggml abort the process still dies (the callback is capture-only):
//! the assert text is visible inline in the TTY (line-buffered stderr) AND
//! captured to `<exe_dir>/logs/sd-abort.txt` by `install_sd_abort_callback`.
//! Read it BEFORE pulling any lever (Q8_0 / VAE / v-pred probe) — §9.

use wupi_lib::scene_art::{
    default_sd_backend, install_sd_abort_callback, install_sd_log_bridge, SceneImageRequest,
};

fn main() {
    // Surface the backend's tracing lines (GGUF layout resolution, CLIP-G
    // warnings, load/generate telemetry). stderr, line-buffered in a TTY —
    // the §11.59 rationale for why this harness sees abort text the app
    // redirect (`sd:dev`'s `2>&1` block buffering) swallowed.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: sd_repro <checkpoint-path> [dest-png]");
        eprintln!("  e.g. sd_repro models/sd/image.gguf out.png");
        std::process::exit(2);
    }
    let checkpoint = std::path::PathBuf::from(&args[1]);
    if !checkpoint.exists() {
        eprintln!("sd_repro: checkpoint not found: {}", checkpoint.display());
        std::process::exit(2);
    }
    let dest = args
        .get(2)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join("sd-repro-out.png")
        });

    // Capture ggml aborts BEFORE any backend call so even a load-time
    // GGML_ASSERT lands in logs/sd-abort.txt.
    install_sd_abort_callback();
    // §11.61: bridge sd.cpp's engine logs (version detection, VAE path,
    // per-step) to stderr — without a registered callback they are dropped.
    install_sd_log_bridge();

    let prompt = "a red apple on a wooden table, studio lighting, photograph".to_string();
    let request = SceneImageRequest {
        prompt,
        dest: dest.clone(),
        model_path: checkpoint.clone(),
        // LOCKED seed: the harness is an A/B isolation tool — identical
        // params must produce identical pixels so a changed lever (VAE,
        // layout, backend) is the only variable between runs. (-1 would
        // randomize every render + make comparisons meaningless.)
        seed: 42,
        ..SceneImageRequest::default()
    };

    eprintln!("sd_repro: loading {}", checkpoint.display());
    let backend = default_sd_backend();
    let t0 = std::time::Instant::now();
    if let Err(e) = backend.load(&checkpoint) {
        eprintln!("sd_repro: LOAD FAILED after {:?}: {e}", t0.elapsed());
        eprintln!("sd_repro: if the process also aborted, read <exe_dir>/logs/sd-abort.txt");
        std::process::exit(1);
    }
    eprintln!("sd_repro: load staged in {:?} (weights parse inside generate)", t0.elapsed());

    let t1 = std::time::Instant::now();
    match backend.generate(&request) {
        Ok(result) => {
            eprintln!(
                "sd_repro: OK — {} ({} ms total, dest exists: {})",
                result.dest.display(),
                t1.elapsed().as_millis(),
                result.dest.exists()
            );
            backend.unload();
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("sd_repro: GENERATE FAILED after {:?}: {e}", t1.elapsed());
            eprintln!("sd_repro: if the process also aborted, read <exe_dir>/logs/sd-abort.txt");
            std::process::exit(1);
        }
    }
}
