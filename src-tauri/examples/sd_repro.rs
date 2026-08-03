//! Standalone SD reproduction harness (§11.59).
//!
//! Faithfully replicates `DiffusionRsGenerator::generate` (scene_art.rs) but
//! runs WITHOUT Tauri/IPC/frontend — driven from the CLI. Purpose: reproduce
//! the `GGML_ABORT` (exit 0x80000003) inside `gen_img` in a real TTY where
//! stderr is line-buffered, so the assertion text is visible inline + ALSO
//! captured to logs/sd-abort.txt by the registered ggml abort callback.
//!
//! Shares the already-compiled diffusion-rs artifacts (no 20-min recompile),
//! so iteration on a lever is a quick incremental rebuild of just this binary.
//!
//! WHY A SEPARATE BINARY (not a test): the failure is an uncatchable `abort()`
//! — it would kill the test runner. A binary prints the assertion to the
//! terminal (the whole point) and exits non-zero, which is the correct shape
//! for a "reproduce + read the error" harness.
//!
//! Usage (run from src-tauri/):
//!   cargo run --release --example sd_repro --features diffusion-rs -- \
//!       --model models/sd/Image.safetensors \
//!       --prompt "1girl, classroom, sunset, high quality" \
//!       --out /tmp/sd_repro.png
//!
//! Levers (default = mirror Prism's scene_art config EXACTLY):
//!   --no-q8         : disable the Q8_0 weight quantization override (use fp16)
//!   --vae <path>    : explicit VAE file (e.g. sdxl_vae_fp16_fix.safetensors)
//!   --steps <n>     : default 28
//!   --cfg <f>       : default 5.0
//!   --size WxH      : default 1024x576
//!   --steps-low     : 1 step (isolate whether the abort fires during LOAD
//!                     vs during SAMPLING — a 1-step run still builds the full
//!                     model/VAE graph, so if it survives, the abort is in
//!                     sampling; if it crashes at 1 step, it's in load/init)
//!   --dry-load      : build ModelConfig + call nothing (isolates the abort
//!                     to model-file parsing vs gen_img internals)
//!
//! Output: prints each phase to stderr (line-buffered in a TTY) so you see
//! exactly how far it got, then either "OK: wrote <path>" or the captured
//! ggml abort (also in logs/sd-abort.txt).

#[cfg(feature = "diffusion-rs")]
fn main() {
    use diffusion_rs::api::{gen_img, ConfigBuilder, ModelConfigBuilder, WeightType};
    use std::path::PathBuf;

    // 1. Parse args. Minimal hand-rolled parser (no extra deps).
    let mut model = PathBuf::from("models/sd/Image.safetensors");
    let mut prompt = "1girl, classroom, sunset, masterpiece, best quality".to_string();
    let mut out = PathBuf::from("/tmp/sd_repro.png");
    let mut negative: Option<String> = None;
    let mut width: i32 = 1024;
    let mut height: i32 = 576;
    let mut steps: i32 = 28;
    let mut cfg: f32 = 5.0;
    let mut q8 = true;
    let mut vae: Option<PathBuf> = None;
    // Multi-file SDXL levers: a Q8-GGUF-converted UNet (ComfyUI convention,
    // bare `input_blocks.*` tensor names) must be passed as `--diffusion-model`
    // so sd.cpp applies its `model.diffusion_model.` prefix + detects SDXL.
    // The GGUF contains ONLY the UNet → CLIP-L, CLIP-G (openCLIP big), + the
    // fp16-fix VAE must be supplied separately. This is the standard
    // stable-diffusion.cpp multi-file SDXL layout.
    let mut diffusion_model: Option<PathBuf> = None;
    let mut clip_l: Option<PathBuf> = None;
    let mut clip_g: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--model" => model = PathBuf::from(args.next().expect("missing --model")),
            "--diffusion-model" => diffusion_model = Some(PathBuf::from(args.next().expect("missing --diffusion-model"))),
            "--clip-l" => clip_l = Some(PathBuf::from(args.next().expect("missing --clip-l"))),
            "--clip-g" => clip_g = Some(PathBuf::from(args.next().expect("missing --clip-g"))),
            "--prompt" => prompt = args.next().expect("missing --prompt"),
            "--out" => out = PathBuf::from(args.next().expect("missing --out")),
            "--negative" => negative = Some(args.next().expect("missing --negative")),
            "--size" => {
                let s = args.next().expect("missing --size");
                let (w, h) = s.split_once('x').expect("--size WxH");
                width = w.parse().expect("width int");
                height = h.parse().expect("height int");
            }
            "--steps" => steps = args.next().expect("missing --steps").parse().expect("steps int"),
            "--cfg" => cfg = args.next().expect("missing --cfg").parse().expect("cfg f32"),
            "--no-q8" => q8 = false,
            "--vae" => vae = Some(PathBuf::from(args.next().expect("missing --vae"))),
            "--steps-low" => steps = 1,
            "--dry-load" => {
                eprintln!("[repro] --dry-load: building ModelConfig only (no gen_img)");
                let mut mb = ModelConfigBuilder::default();
                mb.model(model.as_path());
                if q8 {
                    mb.weight_type(WeightType::SD_TYPE_Q8_0);
                }
                if let Some(v) = &vae {
                    mb.vae(v.as_path());
                }
                let mc = mb.build().expect("ModelConfig build");
                eprintln!("[repro] ModelConfig built OK (no abort in file parse). weight_type={}", if q8 {"Q8_0"} else {"fp16"});
                let _ = mc; // silence unused
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    // 2. Install the abort callback (same fn lib.rs::run() uses at boot) so
    //    the assertion text lands in logs/sd-abort.txt even if something
    //    swallows the TTY stderr. In a real terminal the message is ALSO
    //    visible inline (line-buffered).
    wupi_lib::scene_art::install_sd_abort_callback();

    // 2b. Install the SD LOG callback. By default stable-diffusion.cpp's logs
    //     go nowhere visible (the default log cb is effectively silent until
    //     sd_set_log_callback is called). Routing them to our eprintln! means
    //     every LOG_INFO/LOG_ERROR inside new_sd_ctx prints LIVE (line-buffered
    //     in a TTY) — so the LAST line printed before the crash names the exact
    //     phase that failed (backend init / model load / clip load / version
    //     detect / ...). This is the highest-signal diagnostic for "where does
    //     it die". Without it the silence is uninterpretable.
    install_sd_log_callback();

    // 2c. VEH: catch the fatal exception + resolve the faulting MODULE. Answers
    //     "is the bug in sd.cpp, ggml, ucrt, or a CUDA driver DLL" definitively
    //     without installing a debugger.
    installveh();

    eprintln!("================ SD REPRO HARNESS ================");
    eprintln!("[repro] model           : {}", model.display());
    if let Some(dm) = &diffusion_model {
        eprintln!("[repro] diffusion_model : {} (UNet; gets model.diffusion_model. prefix)", dm.display());
    }
    if let Some(c) = &clip_l { eprintln!("[repro] clip_l          : {}", c.display()); }
    if let Some(c) = &clip_g { eprintln!("[repro] clip_g          : {}", c.display()); }
    eprintln!("[repro] prompt: {prompt}");
    eprintln!("[repro] size  : {width}x{height}, steps={steps}, cfg={cfg}, q8={q8}, vae={}",
        vae.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "<none/embedded>".into()));
    eprintln!("[repro] out   : {}", out.display());
    eprintln!("[repro] weight_type = {}", if q8 { "Q8_0" } else { "fp16 (auto)" });

    // 3. Build ModelConfig (mirror scene_art.rs::generate exactly, plus
    //    optional levers). For the multi-file SDXL layout (Q8 GGUF UNet),
    //    diffusion_model/clip_l/clip_g/vae are the actual load targets; the
    //    `model` field is left at its default (empty) so sd.cpp doesn't ALSO
    //    try to parse the GGUF as a single-file checkpoint.
    eprintln!("[repro] phase: building ModelConfig ...");
    let mut mc_builder = ModelConfigBuilder::default();
    // Single-file checkpoint path. When using --diffusion-model (multi-file
    // layout), we clear --model so sd.cpp's single-file branch is skipped.
    if diffusion_model.is_some() {
        // multi-file: do NOT set model(); set diffusion_model + CLIP + VAE.
        if let Some(dm) = &diffusion_model {
            mc_builder.diffusion_model(dm.as_path());
        }
        if let Some(c) = &clip_l { mc_builder.clip_l(c.as_path()); }
        if let Some(c) = &clip_g { mc_builder.clip_g(c.as_path()); }
        // weight_type is irrelevant for GGUF (already quantized); keep q8 off
        // to avoid sd.cpp trying to re-quantize a GGUF.
        if let Some(v) = &vae {
            eprintln!("[repro] phase: setting explicit VAE: {}", v.display());
            mc_builder.vae(v.as_path());
        }
    } else {
        // single-file legacy layout (the original failing path).
        mc_builder.model(model.as_path());
        if q8 {
            mc_builder.weight_type(WeightType::SD_TYPE_Q8_0);
        }
        mc_builder.vae_tiling(true);
        if let Some(v) = &vae {
            eprintln!("[repro] phase: setting explicit VAE: {}", v.display());
            mc_builder.vae(v.as_path());
        }
    }
    let mut model_config = match mc_builder.build() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[repro] FAIL: ModelConfig build error: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[repro] phase: ModelConfig OK. Calling gen_img (this is where it aborts) ...");

    // 4. Build Config (per-turn) + call gen_img.
    let mut cb = ConfigBuilder::default();
    cb.prompt(prompt.clone())
        .width(width)
        .height(height)
        .steps(steps)
        .cfg_scale(cfg)
        .seed(-1)
        .output(out.clone());
    if let Some(n) = &negative {
        cb.negative_prompt(n.clone());
    }
    let config = match cb.build() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[repro] FAIL: Config build error: {e}");
            std::process::exit(1);
        }
    };

    // 5. gen_img. If this aborts, the ggml callback fires first (printing +
    //    writing sd-abort.txt); then the process dies with 0x80000003. There
    //    is NO way to catch it (abort is unconditional) — the absence of a
    //    return here IS the diagnostic.
    match gen_img(&config, &mut model_config) {
        Ok(()) => {
            eprintln!("[repro] gen_img returned Ok. Checking output file ...");
            match std::fs::metadata(&out) {
                Ok(m) => {
                    eprintln!("[repro] OK: wrote {} ({} bytes)", out.display(), m.len());
                    eprintln!("[repro] >>> SUCCESS — image generated. The lever that differs from the failing config is the fix.");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("[repro] WARN: gen_img Ok but output missing: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("[repro] FAIL: gen_img returned Err (did NOT abort): {e:?}");
            std::process::exit(1);
        }
    }
    // Unreachable.
    #[allow(unreachable_code)]
    {
        let _ = (&prompt, &model, &out, width, height, steps, cfg, q8);
    }
}

// SD LOG callback installer: routes stable-diffusion.cpp's LOG_INFO/LOG_WARN/
// LOG_ERROR to eprintln! so we can see how far new_sd_ctx gets before crashing.
// The default log callback prints nothing visible, so without this the loader
// is a black box.
#[cfg(feature = "diffusion-rs")]
fn install_sd_log_callback() {
    use diffusion_rs_sys::{sd_log_level_t, sd_set_log_callback};
    use std::os::raw::{c_char, c_void};

    extern "C" fn on_log(
        level: sd_log_level_t,
        text: *const c_char,
        _data: *mut c_void,
    ) {
        let tag = match level {
            sd_log_level_t::SD_LOG_DEBUG => "[sd DBG]",
            sd_log_level_t::SD_LOG_INFO => "[sd INF]",
            sd_log_level_t::SD_LOG_WARN => "[sd WRN]",
            sd_log_level_t::SD_LOG_ERROR => "[sd ERR]",
            _ => "[sd ???]",
        };
        let msg = if text.is_null() {
            "<null>".to_string()
        } else {
            unsafe { std::ffi::CStr::from_ptr(text).to_string_lossy().into_owned() }
        };
        eprintln!("{tag} {msg}");
    }

    // Safety: registers a global callback; thread-safe per sd.cpp contract.
    unsafe {
        sd_set_log_callback(Some(on_log), std::ptr::null_mut());
    }
    eprintln!("[repro] SD log callback installed (stable-diffusion.cpp logs now visible)");
}

// Vectored Exception Handler: catches the fatal exception (STATUS_BREAKPOINT
// 0x80000003 / STATUS_STACK_BUFFER_OVERRUN 0xC0000409) BEFORE the WER dialog,
// resolves the faulting address to its containing module + symbol-ish name,
// and prints it. This is the "poor dev's debugger" — gives us the faulting
// MODULE (ucrtbase? ggml-cuda? stable-diffusion? nvcuda?) without installing
// cdb/windbg. The handler must be minimal + never allocate.
#[cfg(feature = "diffusion-rs")]
fn installveh() {
    // Resolve RtlCaptureContext + AddVectoredExceptionHandler via the windows
    // crate-free FFI (kernel32). We only need the exception record's
    // ExceptionCode + ExceptionAddress.
    use std::os::raw::c_void;
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ExceptionPointers {
        exception_record: *mut ExceptionRecord,
        context_record: *mut u8, // we don't walk the full CONTEXT here
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut ExceptionRecord,
        exception_address: *mut c_void,
        // ...rest ignored
        _pad: [u32; 10],
    }
    type VectoredHandler = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;
    extern "system" {
        fn AddVectoredExceptionHandler(first: u32, handler: VectoredHandler) -> *mut c_void;
        fn GetModuleFileNameW(h: *mut c_void, buf: *mut u16, n: u32) -> u32;
    }

    unsafe extern "system" fn handler(ep: *mut ExceptionPointers) -> i32 {
        let rec = *(*ep).exception_record;
        let code = rec.exception_code;
        let addr = rec.exception_address as usize;
        // Map the faulting address to a module via the loaded-module list.
        // Simplest robust approach: try GetModuleHandleEx; but to avoid more
        // FFI, we just report the raw address + code — combined with the list
        // of loaded module base/range (printed at boot below) it localizes
        // the faulting module.
        eprintln!("\n========== VEH CAUGHT EXCEPTION ==========");
        eprintln!("ExceptionCode: 0x{code:08X}");
        eprintln!("ExceptionAddress: 0x{addr:016X}");
        // Exception code meanings:
        //  0x80000003 = STATUS_BREAKPOINT (int 3 / __debugbreak / assert)
        //  0xC0000005 = STATUS_ACCESS_VIOLATION
        //  0xC0000409 = STATUS_STACK_BUFFER_OVERRUN (/GS canary)
        //  0xC000001D = STATUS_ILLEGAL_INSTRUCTION
        //  0xC00000FD = STATUS_STACK_OVERFLOW
        match code {
            0x80000003 => eprintln!("  -> STATUS_BREAKPOINT (debug assert / __debugbreak / abort)"),
            0xC0000005 => eprintln!("  -> STATUS_ACCESS_VIOLATION (null/wild pointer)"),
            0xC0000409 => eprintln!("  -> STATUS_STACK_BUFFER_OVERRUN (/GS canary, or __fastfail)"),
            0xC00000FD => eprintln!("  -> STATUS_STACK_OVERFLOW (deep recursion)"),
            _ => eprintln!("  -> (see ntstatus.h)"),
        }
        // Try to resolve the module containing `addr`. GetModuleHandleExW with
        // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS + UNCHANGED_REFCOUNT.
        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleExW(flags: u32, addr: *const c_void, modh: *mut *mut c_void) -> i32;
        }
        let mut mh: *mut c_void = std::ptr::null_mut();
        // FROM_ADDRESS=4 | UNCHANGED_REFCOUNT=2
        if GetModuleHandleExW(6, addr as *const c_void, &mut mh) != 0 && !mh.is_null() {
            let mut buf = [0u16; 520];
            let n = GetModuleFileNameW(mh, buf.as_mut_ptr(), buf.len() as u32);
            if n > 0 {
                let name = String::from_utf16_lossy(&buf[..n as usize]);
                eprintln!("Faulting module: {name}");
            }
        } else {
            eprintln!("Faulting module: <unresolved — likely JIT code or unmapped>");
        }
        eprintln!("===========================================\n");
        // EXCEPTION_CONTINUE_SEARCH (= 0): let the default handler proceed
        // (process dies). We've captured what we need.
        0
    }

    unsafe {
        AddVectoredExceptionHandler(1, handler);
    }
    // Print loaded-module bases so the raw ExceptionAddress can be matched to
    // a range by hand if GetModuleHandleEx fails (it can for JIT GPU code).
    eprintln!("[repro] VEH installed — faulting module + address will print on crash");
}

#[cfg(not(feature = "diffusion-rs"))]
fn main() {
    eprintln!("sd_repro requires the diffusion-rs feature.");
    eprintln!("Rebuild with: cargo run --release --example sd_repro --features diffusion-rs -- <args>");
    std::process::exit(2);
}
