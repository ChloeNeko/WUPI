//! Temp-staged self-updater (the wupi.exe half of the pipeline).
//!
//! WUPI ships as a portable zip. This module downloads a new release zip, then
//! hands off to a SEPARATE headless binary — `bin/updater.exe` (built from the
//! standalone `crates/updater` crate) — which runs from `%TEMP%` AFTER
//! wupi.exe has exited. The split is load-bearing: a running Windows binary
//! cannot be overwritten in place, but a binary whose process has exited can.
//! So wupi.exe downloads + stages + spawns the updater, then `exit(0)`s; the
//! updater waits for that exit, overwrites the install in place (every lock
//! released), and relaunches the new wupi.exe.
//!
//! ## The handoff (`perform_update`)
//!
//! 1. Download the portable zip to `data/_update/portable.zip` (streaming, with
//!    `update-progress` events for the UI). Atomic `.part` → final rename.
//! 2. Copy `<exe_dir>/bin/updater.exe` → `%TEMP%/wupi_updater_<pid>.exe`. The
//!    temp COPY is what runs — so the install's own `bin/updater.exe` is just an
//!    ordinary unlocked file that the next update's payload can overwrite (the
//!    "who updates the updater" problem solves itself).
//! 3. Spawn the temp copy detached (`CREATE_NO_WINDOW` — headless) with
//!    `--pid <self_pid> --target-dir <exe_dir> --zip <zip> --version <v>`.
//! 4. Emit `update-relaunching` and `std::process::exit(0)`. The exit is the
//!    whole point — it releases wupi.exe's locks. The apply IPC therefore NEVER
//!    returns to the frontend on success (the process is gone); the frontend
//!    treats it as fire-and-forget, and every subsequent boot deletes the
//!    updater's `%TEMP%` presence marker (the takeover-guard signal — no
//!    outcome is surfaced anywhere; 2026-08-20 Chloe rulings).
//!
//! The `data/_update/` staging folder is DELETED COMPLETELY by the updater
//! binary when it finishes (`purge::purge_update_staging`, retried across
//! AV/indexer lock windows, every exit path) — wupi.exe never sweeps it
//! itself. Note the one-hop lag: the updater that applies a hop is the OLD
//! install's `bin/updater.exe`, so a purge fix first fires on the hop AFTER
//! the release that ships it.
//!
//! ## Why NO rename-dance (the §8C "Temp-Staged Update Pipeline" rule)
//!
//! Earlier versions overwrote wupi.exe in place via a `wupi.exe → wupi.exe.old`
//! rename dance + a boot sweep of `.old` remnants. That is RETIRED. The temp-
//! staged updater.exe replaces it: wupi.exe exits cleanly, the updater
//! overwrites the now-unlocked files directly, and there are NO `.old` files
//! ever. The boot sweep (`cleanup_old_files`) is GONE — nothing produces `.old`
//! files anymore.
//!
//! ## SQLite WAL crash-safety
//!
//! An abrupt `exit(0)` during a mid-session apply can leave `memory.sqlite`'s
//! WAL unflushed. WAL mode is designed for exactly this — the next open replays
//! the WAL. No corruption, just a possibly-pending checkpoint.
//!
//! See `crates/updater/src/main.rs` for the updater-binary half, and AGENTS.md
//! §8C "Temp-Staged Update Pipeline" for the full protocol + the documented
//! one-time transition (the LAST in-place delivery is processed by the prior
//! version's already-shipped code; from this version onward, updater.exe owns
//! all updates).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The URL of the static manifest the updater polls. Published to gh-pages by
/// `scripts/release.cjs`. Same endpoint the old Tauri updater used.
const MANIFEST_URL: &str = "https://chloeneko.github.io/WUPI/updater/latest.json";

/// The result of [`check_for_updates`]: a new version is available, with its
/// version string, the portable-zip URL, the release notes, and the optional
/// hex SHA-256 over the zip (verified post-download when present).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: String,
    pub sha256: Option<String>,
}

/// The manifest shape published by `release.cjs`. Mirrors the old Tauri
/// manifest fields (version/notes/pub_date) so the publish side barely
/// changes; we ignore the per-platform signature block (Tauri-specific).
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    notes: Option<String>,
    platforms: std::collections::HashMap<String, PlatformEntry>,
}

#[derive(Debug, Deserialize)]
struct PlatformEntry {
    url: Option<String>,
    // (2026-08-20) Optional hex SHA-256 over the portable zip, same digest
    // HF exposes as x-file-sha256. Present → hard post-download gate;
    // absent → warn + proceed (the unsigned-manifest transition window).
    // minisign verification stays deferred — this closes corrupted/wrong-
    // payload installs, not a malicious manifest (that needs the key
    // ceremony).
    sha256: Option<String>,
    // `signature` ignored — we don't verify minisig (deferred).
}

/// Poll the manifest; return `Ok(Some(UpdateInfo))` if `remote_version >
/// current`, `Ok(None)` when the manifest was fetched and no newer version
/// exists, and `Err` when the check could not complete (network/manifest
/// failure).
///
/// The Ok(None)/Err split is load-bearing for the paw-menu panel: a FAILED
/// check must never render as "up to date" (the 2026-08-18 stuck-panel bug —
/// a transient boot-time fetch failure returned None, the panel showed
/// "WUPI is up to date", and with no re-check affordance the only recovery
/// was an app restart). The boot gate still swallows the Err (JS proceeds to
/// the desktop); the panel surfaces it with a Retry button instead.
pub async fn check_for_updates(
    current_version: &str,
) -> Result<Option<UpdateInfo>, String> {
    // One immediate retry: boot-time checks race NIC/VPN/DNS bring-up, and
    // re-fetching a ~1 KB manifest is free. A hard-down network fails in
    // milliseconds, so the retry adds ~nothing to the offline boot path; it
    // only pays the per-attempt timeout when a connection genuinely hangs.
    let bytes = match fetch_manifest().await {
        Ok(b) => b,
        Err(first) => match fetch_manifest().await {
            Ok(b) => b,
            Err(second) => {
                tracing::info!(?first, ?second, "updater: manifest fetch failed");
                return Err(format!("{first}; retry: {second}"));
            }
        },
    };
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|e| format!("manifest malformed: {e}"))?;
    if !is_newer(&manifest.version, current_version) {
        tracing::info!(
            new = %manifest.version,
            current = %current_version,
            "updater: on latest"
        );
        return Ok(None);
    }
    // A manifest that omits our platform entry (or its URL) is broken, not
    // "up to date" — a publish-side mistake must surface as an error, not
    // silently pin the user on the old version.
    let entry = manifest
        .platforms
        .get("windows-x86_64")
        .ok_or("manifest has no windows-x86_64 entry")?;
    let url = entry
        .url
        .clone()
        .ok_or("manifest windows-x86_64 entry has no url")?;
    let sha256 = entry.sha256.clone();
    Ok(Some(UpdateInfo {
        version: manifest.version,
        url,
        notes: manifest.notes.unwrap_or_default(),
        sha256,
    }))
}

/// Apply a pending update: download the zip → stage the updater binary to
/// `%TEMP%` → spawn it detached → `exit(0)`.
///
/// This function NEVER RETURNS on the happy path — the process exits so the
/// updater can overwrite the now-unlocked install files. On a staging/spawn
/// failure (before the exit), it returns `Err` and the caller proceeds with the
/// current binary. Progress events (`update-progress`, 0..=100) fire during the
/// download; a single `update-relaunching` event fires right before the exit.
pub async fn perform_update(
    app_handle: &tauri::AppHandle,
    update: UpdateInfo,
) -> Result<(), String> {
    use tauri::Emitter;

    let exe_dir = exe_dir(app_handle).ok_or("could not resolve exe dir")?;
    let staging = exe_dir.join("data").join("_update");
    std::fs::create_dir_all(&staging).map_err(|e| format!("create staging: {e}"))?;

    let zip_part = staging.join("portable.zip.part");
    let zip_final = staging.join("portable.zip");

    // ── Phase 1: download with progress events ────────────────────────────
    download_with_progress(&update.url, &zip_part, app_handle).await?;
    // ── Phase 1b: integrity gate (2026-08-20) ─────────────────────────────
    // The manifest's optional `sha256` over the zip. Present → hard gate: a
    // mismatched or corrupted payload is deleted + aborted BEFORE the atomic
    // rename (a bad zip must never reach the updater's overwrite pass).
    // Absent → warn + proceed (unsigned-manifest transition window; minisign
    // verification remains deferred pending the key ceremony — until a hash
    // ships, HTTPS is the only transport integrity).
    verify_download_sha256(&zip_part, update.sha256.as_deref()).await?;
    // Atomic rename: .part → final. A crash leaves only the .part; the next
    // attempt re-downloads.
    std::fs::rename(&zip_part, &zip_final).map_err(|e| format!("rename .part: {e}"))?;

    // ── Phase 2: stage the updater binary to %TEMP% ───────────────────────
    // Run a temp COPY (not the install's bin/updater.exe directly) so the
    // install copy stays an ordinary file the next payload can overwrite
    // (self-update) and so we're not executing a file about to be replaced.
    let pid = std::process::id();
    let install_updater = exe_dir.join("bin").join(updater_basename());
    if !install_updater.is_file() {
        return Err(format!(
            "updater binary missing at {} — cannot stage update",
            install_updater.display()
        ));
    }
    let temp_updater = std::env::temp_dir().join(format!("wupi_updater_{pid}.exe"));
    std::fs::copy(&install_updater, &temp_updater)
        .map_err(|e| format!("stage updater to temp: {e}"))?;

    // ── Phase 3: spawn detached + exit ────────────────────────────────────
    let mut cmd = std::process::Command::new(&temp_updater);
    cmd.arg("--pid")
        .arg(pid.to_string())
        .arg("--target-dir")
        .arg(&exe_dir)
        .arg("--zip")
        .arg(&zip_final)
        .arg("--version")
        .arg(&update.version);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW alone — NEVER combine with DETACHED_PROCESS: the
        // docs say CREATE_NO_WINDOW is IGNORED when paired with it, and a
        // console-less detached process that later spawns a console app with
        // default flags (e.g. cmd running an external exe) gets a NEW VISIBLE
        // console window for it. CREATE_NO_WINDOW alone gives the child a
        // hidden console its own grandchildren inherit — fully invisible.
        // (Lifetime is not a concern: Windows children always outlive their
        // parent; there is no "tied child" to detach from.)
        // 0x0800_0000 per winbase.h. Do NOT "correct" this to 0x0200_0000 —
        // that is CREATE_PRESERVE_CODE_AUTHZ_LEVEL, a no-op that leaves the
        // console-subsystem updater.exe with a VISIBLE console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map_err(|e| format!("spawn updater: {e}"))?;

    let _ = app_handle.emit("update-relaunching", &update);

    // (2026-08-20) No log flush before the hard exit: this apply runs at
    // the BOOT GATE — before logs_begin — so no session file exists yet;
    // once logging has begun elsewhere, logs.rs flushes per line anyway.

    // Exit immediately — releases every OS file lock on the install so the
    // updater's overwrite succeeds. The updater waits on our PID (passed above)
    // before it touches anything. This IPC call never returns to the frontend
    // on success (the process is gone); see the module doc.
    std::process::exit(0);
}

/// Delete the updater's marker files if present. The `%TEMP%` marker
/// (`wupi_update_result.json`) is PRESENCE-ONLY — the updater's
/// user-takeover signal (it touches the empty file before its relaunch
/// loop; a boot from this install deleting it = the user took over) — and
/// carries no outcome: NO update result is ever surfaced to the UI
/// (2026-08-20 Chloe ruling — a crashed update leaves crash logs; the
/// updater's `%TEMP%` log is the apply-side trail). `exe_dir` is used ONLY
/// for the one-hop legacy cleanup: the prior version's updater wrote
/// `data/_update_result.json`, and this is the only code that will ever
/// remove such a leftover — after one update hop + one boot the legacy
/// path can never reappear, so the install folder stays marker-free.
pub fn clear_result_markers(exe_dir: &Path) {
    let _ = std::fs::remove_file(std::env::temp_dir().join("wupi_update_result.json"));
    let _ = std::fs::remove_file(exe_dir.join("data").join("_update_result.json"));
}

/// Resolve `<exe_dir>` — the directory containing `wupi.exe`.
fn exe_dir(app_handle: &tauri::AppHandle) -> Option<PathBuf> {
    let _ = app_handle; // unused on the happy path; kept for symmetry/future use
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|parent| parent.to_path_buf()))
}

/// The updater binary's basename on this platform.
fn updater_basename() -> String {
    if cfg!(windows) {
        "updater.exe".to_string()
    } else {
        "updater".to_string()
    }
}

/// Fetch the manifest bytes. No streaming — the manifest is tiny (~1KB).
async fn fetch_manifest() -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .user_agent("wupi-updater")
        // Total-request cap: a 1 KB manifest over a hung connection must not
        // hold the boot gate (or a panel check) dark indefinitely — reqwest's
        // default is NO timeout.
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(MANIFEST_URL)
        // Best-effort cache bust: gh-pages serves the manifest with a
        // ~10-minute Cache-Control, and a stale cached copy reads as "on
        // latest" for checks run shortly after a publish (another flavor of
        // the phantom up-to-date).
        .header("Cache-Control", "no-cache")
        .header("Pragma", "no-cache")
        .send()
        .await
        .map_err(|e| format!("manifest GET: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("manifest status: {}", resp.status()));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("manifest body: {e}"))
}

/// New-version check. Treats the version strings as monotonic dotted numbers:
/// compares component-by-component as integers, falling back to string compare
/// on parse failure. Sufficient because `release.cjs` bumps monotonically
/// (patch/minor/major). Not a full semver impl (no prerelease tags).
fn is_newer(remote: &str, current: &str) -> bool {
    let cmp = compare_dotted(remote, current);
    matches!(cmp, std::cmp::Ordering::Greater)
}

fn compare_dotted(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ait = a.split('.').fuse();
    let mut bit = b.split('.').fuse();
    loop {
        match (ait.next(), bit.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(a_part), Some(b_part)) => {
                let an: Option<u64> = a_part.parse().ok();
                let bn: Option<u64> = b_part.parse().ok();
                let ord = match (an, bn) {
                    (Some(an), Some(bn)) => an.cmp(&bn),
                    _ => a_part.cmp(b_part),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Stream-download `url` into `dest`, emitting `update-progress` events with
/// the percentage complete. The total size is taken from the Content-Length
/// header; if absent, no progress events fire (the download just completes).
///
/// **Size-bounded (#69):** the zip is a known artifact we build — a sane
/// hard cap + a Content-Length cross-check keep a wrong/hijacked URL from
/// filling the disk via `data/_update/portable.zip.part`. Content-Length
/// over the cap aborts immediately; a stream that outgrows the cap without
/// one (or lying below it) aborts mid-flight. HTTPS-beta mitigates the
/// threat, this is the mechanical backstop.
async fn download_with_progress(
    url: &str,
    dest: &Path,
    app_handle: &tauri::AppHandle,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use tauri::Emitter;
    use tokio::io::AsyncWriteExt;

    /// The WUPI zip ships ~2–4 GB of CUDA DLLs today; 16 GB leaves generous
    /// growth room while staying under any plausible system disk.
    const MAX_ZIP_BYTES: u64 = 16 * 1024 * 1024 * 1024;

    let client = reqwest::Client::builder()
        .user_agent("wupi-updater")
        // (2026-08-20) Connect cap only — the GB-scale body must have NO total
        // timeout (it would kill legit slow links); the per-chunk idle guard
        // in the stream loop below is the hang net, mirroring the narrator
        // path's TTFT/idle discipline. reqwest's default is NO timeout at
        // all, and this download runs at the boot gate: a hung connection
        // wedges the entire boot until process kill.
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("zip GET: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("zip status: {}", resp.status()));
    }
    let total = resp.content_length();
    if let Some(len) = total {
        if len > MAX_ZIP_BYTES {
            return Err(format!(
                "zip Content-Length {len} exceeds the {MAX_ZIP_BYTES}-byte download cap (wrong URL?)"
            ));
        }
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("create dest: {e}"))?;
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;
    let mut last_pct: u64 = 0;
    // (2026-08-20) Idle guard: a connection that accepts then goes silent
    // stalls stream.next() forever under reqwest's no-timeout default. A
    // healthy CDN serving a GB file streams chunks continuously; 60s without
    // a byte = dead link, fail loudly so the boot gate can surface it.
    const CHUNK_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    loop {
        let chunk = match tokio::time::timeout(CHUNK_IDLE_TIMEOUT, stream.next()).await {
            Ok(c) => c,
            Err(_) => {
                return Err(format!(
                    "zip stream stalled: no data for {}s (dead link?)",
                    CHUNK_IDLE_TIMEOUT.as_secs()
                ))
            }
        };
        let Some(chunk) = chunk
            .transpose()
            .map_err(|e| format!("zip stream: {e}"))?
        else {
            break;
        };
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write chunk: {e}"))?;
        written += chunk.len() as u64;
        if written > MAX_ZIP_BYTES {
            return Err(format!(
                "zip stream outgrew the {MAX_ZIP_BYTES}-byte download cap at {written} bytes (wrong URL?)"
            ));
        }
        if let Some(total) = total {
            let pct = (written * 100) / total.max(1);
            // Throttle: only emit on whole-percent change (2/sec cap at full
            // speed). Keeps the IPC channel quiet.
            if pct > last_pct {
                last_pct = pct;
                let _ = app_handle.emit(
                    "update-progress",
                    serde_json::json!({ "percent": pct, "downloaded": written, "total": total }),
                );
            }
        }
    }
    file.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

/// Streamed SHA-256 over a file (1 MB chunks — a 2-4 GB zip never loads whole
/// into RAM). Returns lowercase hex.
async fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open for hash: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("hash read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    Ok(out)
}

/// Post-download integrity gate. `expected` = the manifest's hex sha256.
/// `None` → warn + proceed (transition window); malformed → hard error;
/// mismatch → delete the .part + hard error (the updater must never receive
/// a corrupted payload).
async fn verify_download_sha256(part: &Path, expected: Option<&str>) -> Result<(), String> {
    let Some(expected) = expected else {
        tracing::warn!("update manifest carries no sha256 — zip integrity unverified (HTTPS only; add the field to release.cjs's manifest when convenient)");
        return Ok(());
    };
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("manifest sha256 is malformed (expected 64 hex chars)".into());
    }
    let actual = sha256_file(part).await?;
    if actual != expected {
        let _ = std::fs::remove_file(part);
        return Err(format!(
            "update zip checksum mismatch (expected {expected}, got {actual}) — download discarded"
        ));
    }
    tracing::info!("update zip checksum verified");
    Ok(())
}
