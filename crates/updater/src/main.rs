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
//! 5. Clean up the staging dir + the downloaded zip, RELAUNCH the new
//!    `<target>/wupi.exe` (retried + alive-verified — see
//!    `spawn_wupi_robust`), write a result marker carrying the relaunch
//!    outcome, and exit.
//!
//! ## Why it runs from %TEMP% — and why it self-deletes
//!
//! The install's own `bin/updater.exe` would be locked if we tried to run it
//! in place (and the payload might want to overwrite it). Running a temp COPY
//! means the install's `bin/updater.exe` is just an ordinary unlocked file —
//! the next update overwrites it naturally as part of the payload.
//!
//! The temp copy is then REMOVED by `self_delete_temp_copy` before exit: this
//! binary is the one-shot "file deleter" of the §8C purge design. (A running
//! Windows exe can't delete its own image, so the delete is done by a tiny
//! detached `cmd` that outlives this process by ~2s; guarded to ONLY fire
//! when actually running from `%TEMP%` so dev builds in `target/` never eat
//! their own binary.) The DEBUG LOG is deliberately kept for that sweep —
//! `sweep_temp_residue` cleans logs older than the 10-minute freshness floor
//! on the next update, and in the meantime the log is the only forensic trail
//! when a relaunch fails.
//!
//! ## Failure model
//!
//! If anything fails BEFORE the copy phase touches the target (bad args, wait
//! error, staging/extract failure, missing `wupi.exe` in payload), the live
//! install is pristine — we spawn the still-intact old `wupi.exe`, write a
//! failure result marker, and exit. The user boots back into the old version
//! and sees the error. A failure DURING the copy phase (disk full, hardware)
//! leaves a version-MIXED install — but with per-file copy-to-temp + rename
//! (#40) every file is complete (never truncated), and the updater does NOT
//! relaunch a mixed install: the user re-launches via the shortcut once the
//! disk issue is resolved; the failure marker + log explain what happened.
//!
//! A relaunch that fails EVERY retry (2026-08-18 fix — observed live on the
//! 0.23.3 → 0.23.5 update: the apply completed, but a single immediate
//! CreateProcess on the freshly rename-replaced 299 MB exe lost a race with
//! the transient handles holding new executable content — both cleanup
//! removes failed on the same locks that second, and the relaunch vanished
//! silently: no boot log line, no panic file, no WER) leaves the update
//! APPLIED and the marker reports `relaunched: false`, so the next manual
//! boot tells the user exactly what happened instead of looking dead.

// Headless binary: no console is ever allocated for it, even if a future
// spawn site drops the creation flag (belt-and-braces alongside the
// CREATE_NO_WINDOW spawn flag in wupi.exe). Debug builds keep the console
// subsystem so `cargo run` diagnostics still print. Same convention as
// src-tauri/src/main.rs. Safe because this crate never prints — all
// diagnostics go through `log()` to a %TEMP% file.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

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
    let (ok, error, install_touched) = match &result {
        Ok(()) => {
            log("update applied successfully");
            (true, None, false)
        }
        Err(e) => {
            log(format!("update FAILED: {e}"));
            // `run` marks copy-phase failures: the install may be a mix of
            // old + new files. See the relaunch decision below.
            (false, Some(e.clone()), e.install_touched)
        }
    };

    // Relaunch on SUCCESS + on PRE-COPY failures (the untouched old exe boots
    // back into the prior version). (#40 2026-08-15) SKIP the relaunch when
    // the copy phase itself errored: with per-file atomic replace every file
    // is complete, but a version-mixed install can fail to start (missing
    // exports etc.) — auto-booting it hid the failure behind a crash loop.
    // The user relaunches via the shortcut once the disk issue is resolved;
    // the failure marker + log explain what happened.
    //
    // The relaunch runs BEFORE the marker write so the marker can carry the
    // relaunch OUTCOME (`relaunched`). The spawned app reads the marker
    // seconds later (frontend boot gate), well after the ~2.5s alive-check
    // below resolves — and a missing toast is cosmetic, a lying one is not.
    let relaunched: Option<bool> = if !install_touched {
        Some(spawn_wupi_robust(&args.target_dir))
    } else {
        log("skipping relaunch: the copy phase failed (install possibly version-mixed)");
        None
    };

    // Record the outcome for the relaunched wupi.exe to surface on its boot.
    write_result(
        &args.target_dir,
        ok,
        Some(&args.version),
        error.as_ref().map(|e| e.message.as_str()),
        relaunched,
    );

    // Sweep stale %TEMP% residue left by PRIOR updates (a pre-0.19 updater
    // never self-deleted its temp copy). Our own live files are excluded;
    // our log intentionally survives for a LATER sweep (forensics).
    sweep_temp_residue();

    // Remove our own %TEMP% exe copy (the no-remnants rule, exe only — the
    // log stays, see self_delete_temp_copy). No-op when not running from
    // %TEMP% (dev builds) or on non-Windows.
    self_delete_temp_copy();

    std::process::exit(if ok { 0 } else { 1 });
}

/// A pipeline failure that knows WHETHER the live install was already being
/// modified when the error struck (#40): `install_touched` is true only for
/// copy-phase failures — those leave a version-mixed install, and main must
/// NOT auto-relaunch it.
#[derive(Debug, Clone)]
struct RunError {
    message: String,
    install_touched: bool,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for RunError {
    fn from(message: String) -> Self {
        Self {
            message,
            install_touched: false,
        }
    }
}

/// The ordered apply pipeline. See the module doc. Returns `Err` WITHOUT
/// touching the live install when staging fails (the bricking-safety gate);
/// returns `Err` AFTER a partial copy only on a mid-copy I/O failure
/// (flagged `install_touched`).
fn run(args: &Args) -> Result<(), RunError> {
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
    //    A failure here has already replaced SOME files — flag it so main
    //    skips the relaunch of a version-mixed install.
    if let Err(e) = stage::copy_into_target(&staging, &args.target_dir) {
        return Err(RunError {
            message: e,
            install_touched: true,
        });
    }

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
/// down with it — RETRIED with backoff and VERIFIED alive (2026-08-18 fix).
///
/// Why retry: the one-shot spawn races whatever transiently holds handles on
/// freshly-written executable content (AV/indexer/sync scanning the ~1.2 GB
/// the copy just laid down). Observed live on the 0.23.3 → 0.23.5 update: the
/// apply completed, both best-effort cleanups (`wupi_stage_*`, `data/_update`)
/// failed on those same locks within the same second, and the single immediate
/// spawn vanished without a trace — no boot log line (not even the first,
/// which the async tracing writer flushes within milliseconds of a living
/// process), no panic file, no WER report. A CreateProcess failure on a locked
/// image logs here and used to end the update silently; retrying converts the
/// race into a wait. The alive-check catches the mirror case — a spawn that
/// SUCCEEDS but whose child dies in the loader before executing any code.
///
/// Returns whether a living wupi.exe was produced.
fn spawn_wupi_robust(target_dir: &Path) -> bool {
    /// Backoff between attempts (secs). Six attempts spanning ~45s total:
    /// long enough to outlast any realistic scan window, short enough that a
    /// genuinely-broken relaunch doesn't hang a headless updater for minutes.
    const BACKOFF_SECS: [u64; 5] = [2, 4, 6, 8, 10];
    /// How long a spawned child must survive before it counts as a real boot
    /// (a loader death happens well inside this; the app's first visible
    /// work starts immediately).
    const ALIVE_GRACE_MS: u64 = 2_500;

    let exe = target_dir.join(exe_basename());
    for attempt in 1..=(BACKOFF_SECS.len() + 1) {
        #[cfg(windows)]
        let spawned = {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW alone — headless launch, no console flash. Do NOT
            // add DETACHED_PROCESS: Windows ignores CREATE_NO_WINDOW when the two
            // are combined, and a console-less detached process spawning a console
            // app with default flags gets a NEW VISIBLE console for it. Lifetime
            // is not a concern either — Windows children always outlive their
            // parent, so the new wupi.exe survives this updater's exit regardless.
            // 0x0800_0000 per winbase.h — 0x0200_0000 is a different flag
            // (CREATE_PRESERVE_CODE_AUTHZ_LEVEL, a no-op) and leaves console
            // children with a VISIBLE window.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            std::process::Command::new(&exe)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
        };
        #[cfg(not(windows))]
        let spawned = std::process::Command::new(&exe).spawn();

        match spawned {
            Ok(mut child) => {
                // Dropping the Child never kills it (std's Child drop leaks
                // the handle on purpose); it only stops us from reaping.
                std::thread::sleep(std::time::Duration::from_millis(ALIVE_GRACE_MS));
                match child.try_wait() {
                    Ok(None) => {
                        log(format!(
                            "spawn wupi.exe succeeded on attempt {attempt} (child alive)"
                        ));
                        return true;
                    }
                    Ok(Some(status)) => log(format!(
                        "spawn wupi.exe attempt {attempt} child died within {ALIVE_GRACE_MS}ms (status {status})"
                    )),
                    // Can't query — assume the child is fine rather than
                    // risk spawning a duplicate instance.
                    Err(e) => {
                        log(format!("spawn wupi.exe try_wait failed: {e} — assuming alive"));
                        return true;
                    }
                }
            }
            Err(e) => log(format!("spawn wupi.exe attempt {attempt} failed: {e}")),
        }
        if attempt <= BACKOFF_SECS.len() {
            std::thread::sleep(std::time::Duration::from_secs(BACKOFF_SECS[attempt - 1]));
        }
    }
    log("spawn wupi.exe FAILED after all retries — update applied, WUPI must be started manually");
    false
}

/// Write `<target>/data/_update_result.json` for the relaunched wupi.exe to
/// read on its next boot (so it can show "Updated to vX.Y.Z" or surface the
/// error). `relaunched`: `Some(bool)` = a relaunch was attempted + how it
/// ended; `None` = skipped by the copy-phase-failure policy. Best-effort — a
/// failure here just means the user won't see the toast.
fn write_result(
    target_dir: &Path,
    ok: bool,
    version: Option<&str>,
    error: Option<&str>,
    relaunched: Option<bool>,
) {
    let data_dir = target_dir.join("data");
    let _ = std::fs::create_dir_all(&data_dir);
    let body = serde_json::json!({
        "ok": ok,
        "version": version,
        "error": error,
        "relaunched": relaunched,
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

/// Remove this binary's own `%TEMP%` copy AFTER we exit — the "no remnants
/// of the file deleter" rule (§8C), exe only. A running Windows exe cannot
/// delete its own image, so we spawn a tiny detached `cmd` that waits ~2s
/// (by which point this process has exited) and then deletes it.
///
/// The DEBUG LOG is intentionally NOT deleted anymore (2026-08-18): it is
/// the only forensic trail when a relaunch fails, and `sweep_temp_residue`
/// already owns cleaning `wupi_updater_*.log` files past the 10-minute
/// freshness floor on every subsequent update.
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
    // `ping -n 3` ≈ 2s sleep (the canonical console-free wait; `timeout`
    // misbehaves with redirected stdin). Quoted paths; %TEMP% never needs
    // escaping beyond quotes for cmd.
    let script = format!(
        "ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"",
        exe.display()
    );
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW ALONE — this is the load-bearing fix. With
    // DETACHED_PROCESS also set, Windows ignores CREATE_NO_WINDOW, cmd gets NO
    // console at all, and its external `ping.exe` child (launched with default
    // flags) then receives a NEW VISIBLE console window — the ~2s terminal the
    // user saw flashing in the background during updates. With
    // CREATE_NO_WINDOW alone, cmd gets a HIDDEN console that ping inherits:
    // nothing is ever visible. (Grandchildren only flash when the parent is
    // console-less; a hidden console is inherited silently.)
    // 0x0800_0000 per winbase.h — 0x0200_0000 is a different flag
    // (CREATE_PRESERVE_CODE_AUTHZ_LEVEL, a no-op) and leaves cmd with a
    // VISIBLE console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new("cmd.exe");
    // raw_arg, not args: std's default Windows argument quoting wraps the
    // script in quotes and escapes the inner path quotes as \" — cmd.exe
    // reads those as literal characters, so the `del` silently misses its
    // target and the temp copy survives (observed live on the
    // 0.19.0→0.19.1 hop). raw_arg passes the script verbatim so the quoted
    // path reaches cmd intact.
    cmd.raw_arg(format!("/C {script}"));
    let _ = cmd.creation_flags(CREATE_NO_WINDOW).spawn();
}

/// Non-Windows stub: the temp-copy handoff + self-delete are Windows-only.
#[cfg(not(windows))]
fn self_delete_temp_copy() {}

/// Delete leftover `%TEMP%` residue from PRIOR updates: any `wupi_updater_*.exe`
/// / `wupi_updater_*.log` file and any `wupi_stage_*` directory other than OUR
/// own live ones (our temp exe is removed by `self_delete_temp_copy` at exit;
/// our log is deliberately left for a LATER sweep — it is the forensic trail
/// if this run's relaunch fails). Updates applied by a pre-0.19 updater leave
/// their temp copy behind — this sweep runs on every update so `%TEMP%`
/// converges without any boot-time wiring. Scoped to WUPI's namespace only;
/// best-effort (locked files are skipped, never fatal).
///
/// **Age floor (#70):** only entries older than 10 minutes (by mtime) are
/// swept. During the exit→relaunch window a manual relaunch can re-detect
/// the update and spawn updater B while updater A is still mid-copy — B's
/// sweep used to `remove_dir_all` A's LIVE staging dir. A live update never
/// takes 10 minutes (the PID wait + copy are minutes at worst); genuine
/// residue from prior boots is always older than the floor.
fn sweep_temp_residue() {
    /// Minimum age (secs) before a namespace entry is considered residue.
    const RESIDUE_AGE_SECS: u64 = 10 * 60;

    let temp = std::env::temp_dir();
    let own_exe = std::env::current_exe().ok();
    let own_log = temp.join(format!("wupi_updater_{}.log", std::process::id()));
    let entries = match std::fs::read_dir(&temp) {
        Ok(e) => e,
        Err(_) => return,
    };
    let now = std::time::SystemTime::now();
    let mut swept = 0usize;
    let mut skipped_fresh = 0usize;
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
            continue; // ours — exe: self-delete at exit; log: a LATER sweep
        }
        // (#70) Young entries may belong to a CONCURRENT updater mid-run —
        // leave them for a later sweep rather than kill a live staging dir.
        let is_stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .map(|age| age.as_secs() >= RESIDUE_AGE_SECS)
            .unwrap_or(false);
        if !is_stale {
            skipped_fresh += 1;
            continue;
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
    if skipped_fresh > 0 {
        log(format!(
            "skipped {skipped_fresh} fresh %TEMP% entr(y/ies) (<{RESIDUE_AGE_SECS}s old — possibly a concurrent updater)"
        ));
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
