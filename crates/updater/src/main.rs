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
//! 5. RELAUNCH the new `<target>/wupi.exe` (retried + alive-verified —
//!    see `spawn_wupi_robust`) under the USER-TAKEOVER GUARD (2026-08-20):
//!    an EMPTY marker file is touched FIRST, and if any wupi.exe/fable.exe
//!    booted from the install in the meantime (consuming it — the boot gate
//!    deletes it), the relaunch CANCELS — never resurrect an app the user
//!    already launched or deliberately closed. A vanished marker is
//!    ATTRIBUTED before cancelling (2026-08-21): this binary's own spawned
//!    children run the same boot gate, so a child that boots far enough to
//!    consume the marker and then dies inside the alive-grace window is
//!    indistinguishable from a user launch by the marker alone — the guard
//!    cancels only when a wupi.exe/fable.exe is actually still alive
//!    (`any_wupi_process_alive`); otherwise it re-arms the marker and the
//!    retry ladder continues. The marker is presence-only:
//!    no outcome payload, nothing surfaced (2026-08-20 Chloe ruling — update
//!    failures are diagnosed via crash logs + this binary's %TEMP% log, never
//!    a UI notice). THEN clean up and exit. Cleanup owns both sides: the
//!    `%TEMP%` staging dir (best-effort here; the next update's residue
//!    sweep backstops it) and the download's `data/_update/` folder
//!    (`purge::purge_update_staging` in main — COMPLETE removal, retried
//!    across AV/indexer lock windows, on EVERY exit path so a failed
//!    extract/copy can't strand the 2–4 GB zip). The purge runs AFTER the
//!    relaunch (2026-08-20): its lock-retry ladder used to sit between
//!    "apply done" and "WUPI is back" — dead time the user stared at —
//!    and nothing in the new boot reads `data/_update/` (the marker lives
//!    in `%TEMP%`, `wupi_update_result.json` — NEVER inside the install,
//!    per the 2026-08-20 Chloe ruling: no updater file may exist in the
//!    user's folder).
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
//! install is pristine — we spawn the still-intact old `wupi.exe` and exit.
//! The user boots back into the old version. A failure DURING the copy phase
//! (disk full, hardware) leaves a version-MIXED install — but with per-file
//! copy-to-temp + rename (#40) every file is complete (never truncated), and
//! the updater does NOT relaunch a mixed install: the user re-launches via
//! the shortcut once the disk issue is resolved. NO update outcome is ever
//! surfaced in the UI (2026-08-20 Chloe ruling): this binary's `%TEMP%` log
//! is the apply-side forensic trail, and a crashed app leaves crash logs.
//!
//! A relaunch that fails EVERY retry (2026-08-18 fix — observed live on the
//! 0.23.3 → 0.23.5 update: the apply completed, but a single immediate
//! CreateProcess on the freshly rename-replaced 299 MB exe lost a race with
//! the transient handles holding new executable content — both cleanup
//! removes failed on the same locks that second, and the relaunch vanished
//! silently: no boot log line, no panic file, no WER) leaves the update
//! APPLIED with no app running — the user launches manually; the %TEMP% log
//! records what happened.
//!
//! The copy phase carries the SAME resilience (2026-08-18, second
//! relaunch-killer — observed live on the 0.24.1 → 0.24.2 hop): a transient
//! lock on ONE file (an AV/indexer pass over the fresh writes) used to fail
//! the whole phase and skip the relaunch of an install that was actually
//! complete. `copy_into_target` now retries locked files across ~30 s and,
//! for files that stay locked, exempts any already byte-identical to the
//! payload (an unchanged runtime DLL is not a version mix). See stage.rs.

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

    // (2026-08-20 no-surfacing ruling) The error STRING no longer rides out
    // of this match — it's already in the %TEMP% updater log above, and no
    // marker/result channel consumes it anymore. Only the relaunch decision
    // flags survive.
    let (ok, install_touched) = match &result {
        Ok(()) => {
            log("update applied successfully");
            (true, false)
        }
        Err(e) => {
            log(format!("update FAILED: {e}"));
            // `run` marks copy-phase failures: the install may be a mix of
            // old + new files. See the relaunch decision below.
            (false, e.install_touched)
        }
    };

    // Relaunch on SUCCESS + on PRE-COPY failures (the untouched old exe boots
    // back into the prior version). (#40 2026-08-15) SKIP the relaunch when
    // the copy phase itself errored: with per-file atomic replace every file
    // is complete, but a version-mixed install can fail to start (missing
    // exports etc.) — auto-booting it hid the failure behind a crash loop.
    // The user relaunches via the shortcut once the disk issue is resolved;
    // the %TEMP% updater log explains what happened.
    //
    // (2026-08-20 USER-TAKEOVER GUARD) An EMPTY marker file is touched before
    // the relaunch attempt: any wupi.exe booted from this install consumes
    // (deletes) it at its boot gate, which is the signal `spawn_wupi_robust`
    // polls between attempts — if the user already launched WUPI (and
    // possibly closed it again), the relaunch cancels instead of spawning a
    // duplicate or resurrecting a closed app. Presence-only by design: NO
    // update outcome is ever surfaced to the UI (2026-08-20 Chloe ruling —
    // a crashed update leaves crash logs; the %TEMP% updater log is the
    // apply-side forensic trail).
    touch_result_marker();
    if !install_touched {
        spawn_wupi_robust(&args.target_dir);
    } else {
        log("skipping relaunch: the copy phase failed (install possibly version-mixed)");
    }

    // The download staging is consumed — remove `data/_update` COMPLETELY,
    // on every exit path, retried across AV/indexer lock windows
    // (`purge::purge_update_staging`). (2026-08-20) This now runs AFTER the
    // relaunch: the purge's retry ladder (up to ~30s of sleeps on a locked
    // zip) used to be dead time between "apply done" and "WUPI is back" — the
    // minute-dark window that invited the user to launch manually in the
    // first place. Safe to defer: nothing in wupi.exe's boot reads
    // `data/_update/` (the result marker lives in `%TEMP%` —
    // `wupi_update_result.json` — not in the install).
    purge::purge_update_staging(&args.target_dir);

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

    // 2. Stage to %TEMP% + verify the payload is complete. Target untouched
    //    by writes (the identical-file check READS the live install only).
    //    Unchanged + preserved entries never stage — see stage.rs.
    let staging = std::env::temp_dir().join(format!("wupi_stage_{}", args.pid));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging).map_err(|e| format!("create staging: {e}"))?;
    let n = stage::extract_to_staging(&args.zip, &staging, &args.target_dir)?;
    log(format!("staged {n} entr(y/ies) to {}", staging.display()));

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

    // 3.5b. Retire the legacy root `spellcheck/` folder (2026-08-27 layout
    //       move: the word lists ship in `bin/` with the runtime DLLs,
    //       SOURCES.md ships no more). Rescue-then-delete, idempotent,
    //       best-effort — see purge.rs::retire_legacy_spellcheck.
    let retired = purge::retire_legacy_spellcheck(&args.target_dir);
    if retired > 0 {
        log(format!("retired legacy spellcheck/ folder ({retired} action(s))"));
    }

    // 3.6. Rename retired USER-FILE names (§8C `USER_FILE_RENAMES`). A pure
    //     same-volume rename — the file's content (API profiles) rides along
    //     byte-identical; no read, no rewrite, no working file (any updater
    //     bookkeeping must live in %TEMP%, never the install — and none is
    //     needed: the rename is its own completion signal). Idempotent +
    //     best-effort; a failed rename never fails the update (the app-side
    //     boot migration backstops it).
    let renamed = migrate_user_files(&args.target_dir);
    if renamed > 0 {
        log(format!("renamed {renamed} user file(s)"));
    }

    // 4. Clean up the %TEMP% staging dir. Best-effort: a locked dir defers
    //    to Windows temp cleanup + the next update's residue sweep. The
    //    download's `data/_update/` folder is NOT handled here — main purges
    //    it on every exit path (a failure used to return before this step
    //    and strand the zip forever).
    let _ = std::fs::remove_dir_all(&staging);
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

/// The marker path in `%TEMP%` — an EMPTY file touched before the relaunch so
/// any manual wupi.exe/fable.exe boot consumes it (the boot gate deletes it on
/// read): the user-takeover signal honored by `spawn_wupi_robust`. (2026-08-20
/// Chloe rulings) The marker NEVER lives inside the install — `%TEMP%` is the
/// updater's side of the fence — and NEVER carries content: presence-only, no
/// outcome is surfaced anywhere. Fixed name (not pid-keyed) because the
/// relaunched/manual wupi.exe is a different process that cannot know the old
/// pid — a fixed path is the only discoverable rendezvous.
fn result_marker_path() -> std::path::PathBuf {
    std::env::temp_dir().join("wupi_update_result.json")
}

/// One-shot user-file RENAMES applied during an update — the rename sibling
/// of `purge_legacy`'s delete list (2026-08-20 Chloe ruling: retire the
/// `api_config.json` filename → `api.json`, content untouched). (from, to)
/// pairs, forward-slash, relative to the install root.
///
/// NEVER destructive: a pure same-volume `fs::rename` (API profiles ride
/// along byte-identical — no parse, no rewrite), skipped when the source is
/// gone (idempotent); when BOTH exist the NEW file wins (it is the
/// app-written current config) and the old is LEFT — the updater never
/// deletes user data. No marker/ledger file: the rename is its own
/// completion signal, and any updater working file must live in `%TEMP%`,
/// never the install.
///
/// NO retry path exists (the v0.30.0 clean break deleted the app-side
/// `ApiConfig::migrate_legacy_name` boot migration): the app reads ONLY
/// `api.json`, so a rename that fails here (e.g. a locked source) leaves
/// the user's API profiles stranded under the old name — they re-enter
/// the config by hand. The window is small by construction (this runs
/// while the old app is shut down for the swap), but the failure is real
/// and terminal, which is why it is logged loudly.
const USER_FILE_RENAMES: &[(&str, &str)] = &[("data/api_config.json", "data/api.json")];

/// Apply every [`USER_FILE_RENAMES`] entry under `target`. Returns the
/// number of files actually renamed. Best-effort by design — a locked source
/// logs and moves on (see [`USER_FILE_RENAMES`]: no retry, the stranded
/// file is only recoverable by hand).
fn migrate_user_files(target: &Path) -> usize {
    let mut renamed = 0usize;
    for (from, to) in USER_FILE_RENAMES {
        let src = target.join(from);
        let dst = target.join(to);
        if !src.is_file() || dst.is_file() {
            continue;
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => {
                log(format!("renamed user file {from} → {to}"));
                renamed += 1;
            }
            Err(e) => log(format!(
                "user-file rename {from} → {to} failed ({e}) — NO retry path exists; the app reads only {to}, so this config is stranded until re-entered by hand"
            )),
        }
    }
    renamed
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
/// (2026-08-20 USER-TAKEOVER GUARD) Between attempts the pre-written result
/// marker is polled: if it vanished, a wupi.exe or fable.exe booted from this
/// install since `main` wrote it. That consumer is ATTRIBUTED (2026-08-21
/// fix): this loop's own children also run the boot gate that eats the
/// marker, so a vanished marker alone can't mean "user" — a child that
/// boots far enough to consume it, then dies inside the grace window
/// (transient GPU/loader hiccup — exactly what the ladder exists for), used
/// to cancel every remaining retry and leave WUPI not running. Attribution
/// is by liveness: every child this loop spawned is already dead by the
/// time an attempt > 1 runs (any survivor returned early), so a living
/// wupi.exe/fable.exe can only be the USER's — cancel (spawning now would
/// die at the single-instance mutex or resurrect an app the user
/// deliberately closed). No live instance ⇒ the consumer was our own dead
/// child — re-arm the marker and keep retrying.
///
/// Returns whether a living wupi.exe was produced — or already existed via
/// a user takeover.
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
    let marker = result_marker_path();
    for attempt in 1..=(BACKOFF_SECS.len() + 1) {
        if attempt > 1 && !marker.exists() {
            if any_wupi_process_alive() {
                log("result marker consumed — the user already launched WUPI; relaunch cancelled");
                return true;
            }
            log("result marker was consumed by our own deceased child (booted, then died in the grace window) — re-arming marker, retrying");
            touch_result_marker();
        }
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

/// True when any `wupi.exe` / `fable.exe` process is running system-wide
/// (matched by image NAME — both launcher exes share the single-instance
/// mutex, so ANY live copy means a WUPI the updater must not spawn
/// alongside). The takeover guard's attribution probe: consulted only when
/// the result marker vanished between attempts AND every child this loop
/// spawned is known dead, so a hit can only be a USER-launched instance.
/// A snapshot failure returns false (no evidence of a user — keep the
/// retry ladder alive rather than strand the app).
#[cfg(windows)]
fn any_wupi_process_alive() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return false,
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
                if name == "wupi.exe" || name == "fable.exe" {
                    found = true;
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// Non-Windows stub: no probe, so a vanished marker reads as the old
/// takeover signal. The shipping target is Windows-only.
#[cfg(not(windows))]
fn any_wupi_process_alive() -> bool {
    false
}

/// Touch an EMPTY `%TEMP%\wupi_update_result.json` right before the relaunch
/// loop — the user-takeover signal `spawn_wupi_robust` polls (a wupi.exe boot
/// from this install deletes it; vanished + a live wupi.exe/fable.exe = the
/// user took over, vanished + none alive = our own deceased child — see the
/// attribution in `spawn_wupi_robust`). Also re-armed by that attribution
/// when a consumed marker is traced to our own child. Presence-only: no
/// outcome payload, nothing for the UI to surface —
/// update failures are diagnosed via crash logs + this binary's `%TEMP%` log
/// (2026-08-20 Chloe ruling: no failure surfacing at all). Best-effort — a
/// failed touch just means the takeover guard runs blind for this hop.
fn touch_result_marker() {
    let _ = std::fs::write(result_marker_path(), b"");
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
/// / `wupi_updater_*.log` file, the shared `wupi_update_result.json` marker,
/// and any `wupi_stage_*` directory other than OUR own live ones (our temp exe
/// is removed by `self_delete_temp_copy` at exit; our log is deliberately left
/// for a LATER sweep — it is the forensic trail if this run's relaunch fails).
/// Updates applied by a pre-0.19 updater leave their temp copy behind — this
/// sweep runs on every update so `%TEMP%` converges without any boot-time
/// wiring. Scoped to WUPI's namespace only; best-effort (locked files are
/// skipped, never fatal).
///
/// **Age floor (#70):** only entries older than 10 minutes (by mtime) are
/// swept. During the exit→relaunch window a manual relaunch can re-detect
/// the update and spawn updater B while updater A is still mid-copy — B's
/// sweep used to `remove_dir_all` A's LIVE staging dir. A live update never
/// takes 10 minutes (the PID wait + copy are minutes at worst); genuine
/// residue from prior boots is always older than the floor. The floor also
/// protects OUR OWN just-written result marker (written seconds before this
/// sweep — it must survive until the next wupi.exe boot consumes it); only a
/// marker orphaned by a prior run (>10 min, never consumed) is swept.
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
            || name.starts_with("wupi_stage_")
            || name == "wupi_update_result.json";
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
pub(crate) fn log(msg: impl AsRef<str>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The happy path: `data/api_config.json` becomes `data/api.json` with
    /// its bytes EXACTLY preserved (profiles ride along untouched).
    #[test]
    fn renames_legacy_api_config_preserving_bytes() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        let body = br#"{"profiles":[{"id":"z","name":"Z.AI","endpoint":"https://api.z.ai","model":"glm-4.6","api_key":"sk-secret"}]}"#;
        std::fs::write(tmp.path().join("data/api_config.json"), body).unwrap();

        assert_eq!(migrate_user_files(tmp.path()), 1);
        assert!(!tmp.path().join("data/api_config.json").exists());
        assert_eq!(
            std::fs::read(tmp.path().join("data/api.json")).unwrap(),
            body.to_vec()
        );
    }

    /// Idempotent (second run is a no-op) and never clobbers: when BOTH files
    /// exist the NEW one wins byte-for-byte and the old is left untouched —
    /// the updater never deletes user data.
    #[test]
    fn rename_is_idempotent_and_never_clobbers() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/api_config.json"), b"OLD").unwrap();
        assert_eq!(migrate_user_files(tmp.path()), 1);
        // Already migrated: source gone → zero renames.
        assert_eq!(migrate_user_files(tmp.path()), 0);

        std::fs::write(tmp.path().join("data/api_config.json"), b"OLD-AGAIN").unwrap();
        assert_eq!(migrate_user_files(tmp.path()), 0);
        assert_eq!(std::fs::read(tmp.path().join("data/api.json")).unwrap(), b"OLD".to_vec());
        assert_eq!(
            std::fs::read(tmp.path().join("data/api_config.json")).unwrap(),
            b"OLD-AGAIN".to_vec()
        );

        // Absent both → clean no-op.
        let empty = TempDir::new().unwrap();
        assert_eq!(migrate_user_files(empty.path()), 0);
    }
}
