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
//! 4. **Purge legacy dead paths** (`purge.rs`) — the compile-time removal
//!    list (dead pre-reorg state folders, retired engine files). Best-effort;
//!    a locked path never fails the update.
//! 5. Clean up the staging dir + the downloaded zip, spawn the new
//!    `<target>/wupi.exe`, write a result marker, and exit.
//!
//! ## Why it runs from %TEMP% — and why it self-deletes
//!
//! The install's own `bin/updater.exe` would be locked if we tried to run it
//! in place (and the payload might want to overwrite it). Running a temp COPY
//! means the install's `bin/updater.exe` is just an ordinary unlocked file —
//! the next update overwrites it naturally as part of the payload.
//!
//! The temp copy + its debug log are then REMOVED by `self_delete_temp_copy`
//! before exit: this binary is the one-shot "file deleter" of the §8C purge
//! design, and it leaves no remnants of itself anywhere — not in the install,
//! not in `%TEMP%`. (A running Windows exe can't delete its own image, so the
//! delete is done by a tiny detached `cmd` that outlives this process by ~2s;
//! guarded to ONLY fire when actually running from `%TEMP%` so dev builds in
//! `target/` never eat their own binary.)
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
mod purge;
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

    // Sweep stale %TEMP% residue left by PRIOR updates (a pre-0.19 updater
    // never self-deleted its temp copy + log). Our own files are excluded —
    // self_delete_temp_copy removes those at exit.
    sweep_temp_residue();

    // Remove our own %TEMP% copy + log (the no-remnants rule). No-op when not
    // running from %TEMP% (dev builds) or on non-Windows.
    self_delete_temp_copy();

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

    // 3.5. Purge legacy dead paths (§8C purge.rs). Runs AFTER the copy so a
    //     failed copy never leaves the install half-cleaned, and BEFORE the
    //     relaunch so the new wupi.exe boots into a clean tree. Best-effort —
    //     must not fail the update.
    let purged = purge::purge_legacy(&args.target_dir);
    if purged > 0 {
        log(format!("purged {purged} legacy path(s)"));
    }

    // 4. Clean up. Best-effort: a locked staging dir defers to Windows temp
    // cleanup; the zip dir is the `data/_update/` the download created.
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

/// Remove this binary's own `%TEMP%` copy + its debug log AFTER we exit —
/// the "no remnants of the file deleter" rule (§8C). A running Windows exe
/// cannot delete its own image, so we spawn a tiny detached `cmd` that waits
/// ~2s (by which point this process has exited) and then deletes both files.
///
/// GUARDED to only fire when the running exe actually lives under `%TEMP%`
/// (the normal wupi.exe handoff) — a dev invocation straight from
/// `crates/updater/target/release/` must not eat its own build output.
/// Best-effort throughout: a failed delete just leaves the sweep to Windows
/// temp cleanup, same as the pre-self-delete behavior.
#[cfg(windows)]
fn self_delete_temp_copy() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let temp = std::env::temp_dir();
    if !exe.starts_with(&temp) {
        return; // Not the %TEMP% handoff copy (dev build) — leave it alone.
    }
    let log_path = temp.join(format!("wupi_updater_{}.log", std::process::id()));
    // `ping -n 3` ≈ 2s sleep (the canonical console-free wait; `timeout`
    // misbehaves with redirected stdin). Quoted paths; %TEMP% never needs
    // escaping beyond quotes for cmd.
    let script = format!(
        "ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & del /f /q \"{}\"",
        exe.display(),
        log_path.display()
    );
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0200_0000;
    let mut cmd = std::process::Command::new("cmd.exe");
    // raw_arg, not args: std's default Windows argument quoting wraps the
    // script in quotes and escapes the inner path quotes as \" — cmd.exe
    // reads those as literal characters, so both `del`s silently miss their
    // targets and the temp copy + log survive (observed live on the
    // 0.19.0→0.19.1 hop). raw_arg passes the script verbatim so the quoted
    // paths reach cmd intact.
    cmd.raw_arg(format!("/C {script}"));
    let _ = cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW).spawn();
}

/// Non-Windows stub: the temp-copy handoff + self-delete are Windows-only.
#[cfg(not(windows))]
fn self_delete_temp_copy() {}

/// Delete leftover `%TEMP%` residue from PRIOR updates: any `wupi_updater_*.exe`
/// / `wupi_updater_*.log` file and any `wupi_stage_*` directory other than OUR
/// own (our temp copy + log are removed by `self_delete_temp_copy` at exit).
/// Updates applied by a pre-0.19 updater leave their temp copy + log behind —
/// this sweep runs on every update so `%TEMP%` converges without any boot-time
/// wiring. Scoped to WUPI's namespace only; best-effort (locked files are
/// skipped, never fatal).
fn sweep_temp_residue() {
    let temp = std::env::temp_dir();
    let own_exe = std::env::current_exe().ok();
    let own_log = temp.join(format!("wupi_updater_{}.log", std::process::id()));
    let entries = match std::fs::read_dir(&temp) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut swept = 0usize;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let ours = (name.starts_with("wupi_updater_")
            && (name.ends_with(".exe") || name.ends_with(".log")))
            || name.starts_with("wupi_stage_");
        if !ours {
            continue;
        }
        let path = entry.path();
        if own_exe.as_deref() == Some(path.as_path()) || path == own_log {
            continue; // ours — self_delete_temp_copy handles these at exit
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let removed = if is_dir {
            std::fs::remove_dir_all(&path).is_ok()
        } else {
            std::fs::remove_file(&path).is_ok()
        };
        if removed {
            swept += 1;
        }
    }
    if swept > 0 {
        log(format!("swept {swept} stale %TEMP% residue file(s)"));
    }
}

/// Headless debug log: append a line to `%TEMP%/wupi_updater_<pid>.log`. There
/// is no console (the launcher uses CREATE_NO_WINDOW), so this file is the only
/// trail when diagnosing a failed update. Best-effort, never panics.
fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    if cfg!(test) {
        return; // in-process tests: don't litter %TEMP% with per-run logs
    }
    let path = std::env::temp_dir().join(format!("wupi_updater_{}.log", std::process::id()));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}
