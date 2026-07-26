// In release builds, run on the Windows GUI subsystem so no console window is
// allocated. In debug builds we keep the console (the default) so log output is
// still visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Windows DLL search directory hook. MUST run before any code that touches
    // a CUDA / VC++ DLL — i.e. before `wupi_lib::run()` pulls in any module.
    //
    // Why: as of v0.3.7 the portable layout moved ALL shipped runtime DLLs
    // (CUDA cublas/cudart/etc. + VC++ msvcp140/vcomp140) out of the install
    // root into a sibling `bin/` subdirectory (AGENTS.md §8B). The 4 PE
    // static-import DLLs (cublas64_13, cudart64_13, msvcp140, vcomp140) are
    // compiled with /DELAYLOAD so they're resolved on FIRST CALL rather than
    // at process start; the other ~6 runtime-loaded CUDA DLLs are loaded by
    // ggml-cuda via LoadLibrary later. Both paths need the loader to search
    // `bin/`. `SetDllDirectoryW` adds one directory to the search path used
    // by LoadLibrary and the delay-load helper — that's the hook.
    //
    // Defensive: if `<exe_dir>\bin` doesn't exist (dev builds run from
    // src-tauri/target/debug), skip the call entirely. The default Windows
    // search path (exe dir + System32 + PATH) still applies, and on the dev
    // box the CUDA Toolkit's bin\x64 is on PATH so the imports resolve there.
    #[cfg(windows)]
    {
        if let Some(bin_path) = exe_bin_dir() {
            // SAFETY: SetDllDirectoryW is a kernel32 function exported since
            // XP. Its only side effect is mutating the per-process DLL
            // search path; passing a valid NUL-terminated wide string is the
            // documented contract. Failure is non-fatal: we ignore the
            // return value (the dev box's PATH already has CUDA, and on a
            // portable install the dir always exists).
            use std::os::windows::ffi::OsStrExt;
            unsafe {
                #[link(name = "kernel32")]
                extern "system" {
                    fn SetDllDirectoryW(lppathname: *const u16) -> i32;
                }
                let wide: Vec<u16> = bin_path
                    .as_os_str()
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let _ = SetDllDirectoryW(wide.as_ptr());
            }
        }
    }

    // WebView2 frontend-cache bust (v0.7.6+). MUST run before
    // `wupi_lib::run()` initializes the WebView2 window — once WebView2
    // starts it locks its cache files and they can't be deleted.
    //
    // Why: WebView2 persists compiled-JS + HTTP caches under
    // `%LOCALAPPDATA%\com.wupi.desktop\EBWebView`, keyed by the app
    // identifier (NOT by exe path or version). After an update the new exe
    // is on disk, but WebView2 happily serves the OLD frontend from cache
    // — producing the exact "ghost paw / no audio / missing themes / broken
    // intro" symptom cluster the v0.7.5 release hit. The build-time
    // `npm run clear:webview2` script only protects the dev box at build
    // time and only if no run happens between the clear and the test; it
    // does nothing for end users who update via the in-app updater.
    //
    // Fix: on every boot, compare the exe's compiled-in version against a
    // dotfile marker `<exe_dir>/.cache-gen`. If they differ (or the marker
    // is absent on first run), delete the EBWebView tree + write the new
    // marker. Cheap on the steady-state path (one tiny file read + compare)
    // and makes stale-frontend-after-update impossible across all three
    // distribution paths (dev test, manual overwrite, in-app updater).
    //
    // The marker lives at the install root (NOT under preserved `data/`)
    // so the updater's preserve rule never touches it — the new exe writes
    // the new version on first boot. Errors are logged-and-continued: a
    // failed cache clear is annoying (the user might see stale UI once) but
    // must NEVER block boot.
    #[cfg(windows)]
    bust_webview2_cache_if_version_changed();

    wupi_lib::run()
}

/// Resolve `<exe_dir>\bin` if (a) `current_exe()` succeeds and (b) that
/// `bin/` subdir exists on disk. Returns `None` for dev builds run from
/// `target/debug/` (no `bin/` there — the dev box's PATH has CUDA).
#[cfg(windows)]
fn exe_bin_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let bin = dir.join("bin");
    if bin.is_dir() {
        Some(bin)
    } else {
        None
    }
}

/// Boot-time WebView2 cache bust. See the long comment in `main()` for the
/// full rationale. Compares the compiled-in crate version against a dotfile
/// marker at the install root; on mismatch (or missing marker), deletes the
/// WebView2 persistent cache tree + writes the new marker. No-op on the
/// steady-state path (same version → same marker → skip). Windows-only: the
/// cache path + identifier are Windows/WebView2-specific.
#[cfg(windows)]
fn bust_webview2_cache_if_version_changed() {
    // Resolve the marker path: `<exe_dir>/.cache-gen`. Use the same
    // current_exe() approach as exe_bin_dir so this works identically on
    // portable installs (exe at install root) and dev runs (exe in
    // target/debug). If current_exe fails we can't safely place the marker,
    // so bail silently (boot must not fail here).
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let install_root = match exe.parent() {
        Some(p) => p,
        None => return,
    };
    let marker = install_root.join(".cache-gen");
    let current_version = env!("CARGO_PKG_VERSION");

    // Read the marker. Three states:
    //   - contents == current_version → steady state, skip the clear.
    //   - contents != current_version → version changed (update), clear.
    //   - file missing / unreadable   → first run or tampered, clear.
    let steady_state = match std::fs::read_to_string(&marker) {
        Ok(contents) => contents.trim() == current_version,
        Err(_) => false,
    };
    if steady_state {
        return;
    }

    // Locate the WebView2 cache tree. Tauri 2 places it under
    // %LOCALAPPDATA%\<identifier>\EBWebView, where <identifier> is the
    // tauri.conf.json `identifier` field (currently com.wupi.desktop per §8C).
    // If the env var is unset (non-Windows, stripped env) there's nothing we
    // can do; bail silently.
    let local_app_data = match std::env::var_os("LOCALAPPDATA") {
        Some(v) => v,
        None => return,
    };
    let cache_dir = std::path::Path::new(&local_app_data)
        .join("com.wupi.desktop")
        .join("EBWebView");

    if cache_dir.exists() {
        // Best-effort delete. The most common reason this fails is a leftover
        // WebView2 helper process holding a lock (msedgewebview2.exe can
        // outlive a crashed wupi.exe). We do NOT kill processes here — the
        // build-time `clear:webview2-cache.cjs` script already handles that
        // for dev runs, and a normal shutdown leaves no lock. On failure we
        // log + still write the marker: a single missed clear is recoverable
        // on the next version bump, but blocking boot would be worse.
        match std::fs::remove_dir_all(&cache_dir) {
            Ok(()) => {
                eprintln!(
                    "[cache-bust] cleared WebView2 cache (version changed to {})",
                    current_version
                );
            }
            Err(e) => {
                // Match the cache-clearer script's error classification so the
                // log line is greppable alongside that script's output.
                eprintln!(
                    "[cache-bust] WARN: could not remove {} ({}); \
                     frontend may be stale until next version bump",
                    cache_dir.display(),
                    e
                );
            }
        }
    }

    // Write the new marker (best-effort). create+truncate; ignore write
    // errors (a read-only install root just means we'll re-attempt the clear
    // every boot, which is wasteful but correct).
    if let Err(e) = std::fs::write(&marker, current_version) {
        eprintln!(
            "[cache-bust] WARN: could not write marker {} ({}); \
             cache clear will re-run next boot",
            marker.display(),
            e
        );
    }
}
