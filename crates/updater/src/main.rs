//! Headless temp-staged updater for WUPI — the second half of the update
//! pipeline (§8C "Temp-Staged Update Pipeline" in AGENTS.md).
//!
//! ## The protocol
//!
//! When the user accepts an update, `wupi.exe` (the Tauri backend in
//! `src-tauri/src/updater.rs`) downloads the portable zip, COPIES this binary
//! from `<install>/bin/updater.exe` into `%TEMP%/wupi_updater_<pid>.exe`, spawns
//! that temp copy as a detached process with `--pid / --target-dir / --zip /
//! --version`, and then calls `std::process::exit(0)` on itself. By the time
//! this binary does any real work, the spawning `wupi.exe` is gone and ALL of
//! its OS file locks (the exe itself, the `bin/` CUDA DLLs, `msvcp140.dll`) are
//! released. That is the entire reason this is a SEPARATE process rather than
//! in-process apply: a running Windows binary cannot be overwritten in place,
//! but a binary whose process has exited can.
//!
//! ## What this binary does, in order
//!
//! 1. Wait for `--pid` to fully exit (`OpenProcess` + `WaitForSingleObject` on
//!    Windows), then a short settle sleep so the kernel finishes releasing the
//!    last handles.
//! 2. Extract the WHOLE zip into a `%TEMP%` staging dir and verify the payload
//!    contains `wupi.exe`. The live install is NOT touched until staging
//!    succeeds — this is the bricking-safety gate (a corrupt/truncated/wrong
//!    zip is rejected before we overwrite anything).
//! 3. Copy the staged payload into `--target-dir`, honoring the §8C preserve
//!    rule (skip user data: `data/{user.xml,...}`, `memory/`, `models/`,
//!    `apps/`). Everything else overwrites in place.
//! 4. Clean up the staging dir + the downloaded zip, spawn the new
//!    `<target>/wupi.exe`, write a result marker, and exit.
//!
//! ## Why it runs from %TEMP%
//!
//! The install's own `bin/updater.exe` would be locked if we tried to run it
//! in place (and the payload might want to overwrite it). Running a temp COPY
//! means the install's `bin/updater.exe` is just an ordinary unlocked file —
//! the next update overwrites it naturally as part of the payload. Windows temp
//! cleanup eventually sweeps the stale copy.
//!
//! ## Failure model
//!
//! If anything fails BEFORE the copy phase touches the target (bad args, wait
//! error, staging/extract failure, missing `wupi.exe` in payload), the live
//! install is pristine — we spawn the still-intact old `wupi.exe`, write a
//! failure result marker, and exit. The user boots back into the old version
//! and sees the error. A failure DURING the copy phase (disk full, hardware)
//! can leave a partially-updated install — the residual bricking surface,
//! documented in AGENTS.md; it requires a disk-space fix + a clean redownload.

use std::path::Path;

mod cli;
mod preserve;
mod stage;

use cli::Args;

fn main() {
    let args = match cli::parse_args() {
        Ok(a) => a,
        Err(e) => {
            // No target-dir yet, so we can't write the result marker. Just log
            // + bail. This only happens if wupi.exe passed malformed args,
            // which it never does.
            log(format!("arg parse failed: {e}"));
            std::process::exit(2);
        }
    };
    log(format!(
        "updater started: pid={} target={} zip={} version={}",
        args.pid,
        args.target_dir.display(),
        args.zip.display(),
        args.version
    ));

    let result = run(&args);
    let (ok, error) = match &result {
        Ok(()) => {
            log("update applied successfully");
            (true, None)
        }
        Err(e) => {
            log(format!("update FAILED: {e}"));
            (false, Some(e.clone()))
        }
    };

    // Record the outcome for the relaunched wupi.exe to surface on its boot.
    write_result(&args.target_dir, ok, Some(&args.version), error.as_deref());

    // ALWAYS relaunch wupi.exe. On success the freshly-overwritten exe boots
    // into the new version; on a pre-copy failure the untouched old exe boots
    // back into the prior version. (A mid-copy failure is the one case where
    // the launched exe may be inconsistent — the documented residual risk.)
    spawn_wupi(&args.target_dir);

    std::process::exit(if ok { 0 } else { 1 });
}

/// The ordered apply pipeline. See the module doc. Returns `Err` WITHOUT
/// touching the live install when staging fails (the bricking-safety gate);
/// returns `Err` AFTER a partial copy only on a mid-copy I/O failure.
fn run(args: &Args) -> Result<(), String> {
    // 1. Wait for the spawning wupi.exe to exit + release its locks. The parent
    //    exit(0)s immediately after spawning us, so the normal wait is sub-
    //    second; the 30s timeout is a generous pathological-case ceiling.
    wait_for_exit(args.pid, 30_000);

    // 2. Stage to %TEMP% + verify the payload is complete. Target untouched.
    let staging = std::env::temp_dir().join(format!("wupi_stage_{}", args.pid));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging).map_err(|e| format!("create staging: {e}"))?;
    let n = stage::extract_to_staging(&args.zip, &staging)?;
    log(format!("staged {n} entries to {}", staging.display()));

    // 3. Copy into the live install (preserve rule applied). All locks released.
    stage::copy_into_target(&staging, &args.target_dir)?;

    // 4. Clean up. Best-effort: a locked staging dir defers to Windows temp
    //    cleanup; the zip dir is the `data/_update/` the download created.
    let _ = std::fs::remove_dir_all(&staging);
    if let Some(update_dir) = args.zip.parent() {
        let _ = std::fs::remove_dir_all(update_dir);
    }
    Ok(())
}

/// Block until `pid` has exited. On Windows: open a sync handle and wait on it
/// (the canonical, efficient wait). Then a fixed 500ms settle sleep — the
/// process may have exited but the kernel can still be tearing it down +
/// releasing its last file locks (esp. the `bin/` CUDA DLLs). The sleep bridges
/// that window so the overwrite of just-freed files succeeds first try.
#[cfg(windows)]
fn wait_for_exit(pid: u32, timeout_ms: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            // Best-effort: WAIT_OBJECT_0 (signaled) is the normal near-instant
            // result; WAIT_TIMEOUT means the parent somehow outlived the
            // ceiling — we proceed + rely on the settle sleep.
            let _ = WaitForSingleObject(handle, timeout_ms);
            let _ = CloseHandle(handle);
        }
        // OpenProcess failure ⇒ the process has already exited (parent called
        // exit(0)). Fall through to the settle sleep either way.
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
}

/// Non-Windows stub. Only reached via `cargo test` on a non-Windows host; the
/// shipping target is Windows-only.
#[cfg(not(windows))]
fn wait_for_exit(_pid: u32, _timeout_ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(100));
}

/// Spawn the (now-current) `wupi.exe` in the target dir as a detached process
/// with no console window, so this updater can exit without taking the new app
/// down with it.
fn spawn_wupi(target_dir: &Path) {
    let exe = target_dir.join(exe_basename());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS: the new wupi.exe must outlive THIS updater's own
        //   imminent exit (it is not a tied child).
        // CREATE_NO_WINDOW: headless launch — no console flash for the user.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0200_0000;
        if let Err(e) = std::process::Command::new(&exe)
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()
        {
            log(format!("spawn wupi.exe failed: {e}"));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new(&exe).spawn();
    }
}

/// Write `<target>/data/_update_result.json` for the relaunched wupi.exe to
/// read on its next boot (so it can show "Updated to vX.Y.Z" or surface the
/// error). Best-effort — a failure here just means the user won't see the toast.
fn write_result(target_dir: &Path, ok: bool, version: Option<&str>, error: Option<&str>) {
    let data_dir = target_dir.join("data");
    let _ = std::fs::create_dir_all(&data_dir);
    let body = serde_json::json!({
        "ok": ok,
        "version": version,
        "error": error,
    });
    if let Ok(s) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write(data_dir.join("_update_result.json"), s);
    }
}

/// The exe basename on this platform (`wupi.exe` on Windows).
fn exe_basename() -> String {
    if cfg!(windows) {
        "wupi.exe".to_string()
    } else {
        "wupi".to_string()
    }
}

/// Headless debug log: append a line to `%TEMP%/wupi_updater_<pid>.log`. There
/// is no console (the launcher uses CREATE_NO_WINDOW), so this file is the only
/// trail when diagnosing a failed update. Best-effort, never panics.
fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let path = std::env::temp_dir().join(format!("wupi_updater_{}.log", std::process::id()));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}
