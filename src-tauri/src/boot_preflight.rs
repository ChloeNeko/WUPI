//! Windows-only boot preflight, shared by BOTH launcher binaries
//! (`wupi.exe` via `src/main.rs` and `fable.exe` via `src/bin/fable.rs`).
//!
//! Two steps, both MUST run before `wupi_lib::run()` initializes anything:
//!   1. `<exe_dir>\bin` DLL search-directory hook — the portable layout keeps
//!      all CUDA/VC++ runtime DLLs in a sibling `bin/`; `SetDllDirectoryW`
//!      adds it to the loader path so the /DELAYLOAD + LoadLibrary paths
//!      resolve. Skipped silently in dev (no `bin/` next to `target/debug`).
//!   2. WebView2 frontend-cache bust — compares the compiled-in crate version
//!      against `<exe_dir>/.cache-gen`; on mismatch deletes the WebView2 cache
//!      tree so a stale frontend can't survive an update.
//!
//! Extracted from the former inline `main.rs` block so `fable.exe` reuses the
//! identical preflight (it loads the same CUDA DLLs + shares the same WebView2
//! identifier `com.wupi.desktop`). No-op on non-Windows.

/// Run the Windows boot preflight (DLL hook + WebView2 cache bust). Called by
/// every launcher binary's `main()` before `wupi_lib::run()`.
#[cfg(windows)]
pub fn windows_preflight() {
    // ── 1. DLL search-directory hook ───────────────────────────────────
    // As of v0.3.7 the portable layout moved ALL shipped runtime DLLs (CUDA
    // cublas/cudart/etc. + VC++ msvcp140/vcomp140) into a sibling `bin/`
    // subdirectory. The 4 PE static-import DLLs use /DELAYLOAD (resolved on
    // first call); the ~6 runtime-loaded CUDA DLLs are LoadLibrary'd by
    // ggml-cuda later. Both paths need the loader to search `bin/`.
    // `SetDllDirectoryW` adds one directory to that search path.
    // Defensive: if `<exe_dir>\bin` doesn't exist (dev builds run from
    // src-tauri/target/debug), skip — the dev box's PATH has CUDA.
    if let Some(bin_path) = exe_bin_dir() {
        // SAFETY: SetDllDirectoryW is a kernel32 function exported since XP.
        // Its only side effect is mutating the per-process DLL search path;
        // passing a valid NUL-terminated wide string is the documented
        // contract. Failure is non-fatal (we ignore the return value).
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

    // ── 2. WebView2 frontend-cache bust ────────────────────────────────
    bust_webview2_cache_if_version_changed();
}

#[cfg(not(windows))]
pub fn windows_preflight() {
    // No DLL hook / no WebView2 on non-Windows targets.
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

/// Boot-time WebView2 cache bust. Compares the compiled-in crate version
/// against a dotfile marker at the install root; on mismatch (or missing
/// marker), deletes the WebView2 persistent cache tree + writes the new
/// marker. No-op on the steady-state path (same version → same marker →
/// skip). Windows-only: the cache path + identifier are Windows/WebView2-
/// specific.
#[cfg(windows)]
fn bust_webview2_cache_if_version_changed() {
    // Resolve the marker path: `<exe_dir>/.cache-gen`. Use current_exe() so
    // this works identically on portable installs (exe at install root) and
    // dev runs (exe in target/debug). If current_exe fails we can't safely
    // place the marker, so bail silently (boot must not fail here).
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
    // tauri.conf.json `identifier` field (com.wupi.desktop). If the env var
    // is unset (non-Windows, stripped env) there's nothing we can do.
    let local_app_data = match std::env::var_os("LOCALAPPDATA") {
        Some(v) => v,
        None => return,
    };
    let cache_dir = std::path::Path::new(&local_app_data)
        .join("com.wupi.desktop")
        .join("EBWebView");

    if cache_dir.exists() {
        // Best-effort delete. The most common failure is a leftover WebView2
        // helper process holding a lock. We do NOT kill processes here — the
        // build-time `clear:webview2-cache.cjs` script handles dev runs, and
        // a normal shutdown leaves no lock. On failure we log + still write
        // the marker: a single missed clear is recoverable on the next
        // version bump, but blocking boot would be worse.
        match std::fs::remove_dir_all(&cache_dir) {
            Ok(()) => {
                eprintln!(
                    "[cache-bust] cleared WebView2 cache (version changed to {})",
                    current_version
                );
            }
            Err(e) => {
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
