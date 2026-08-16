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
//!    treats it as fire-and-forget, and the relaunched wupi.exe reads
//!    `data/_update_result.json` on next boot to surface success/failure.
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
/// version string, the portable-zip URL, and the release notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: String,
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
    // `signature` ignored — we don't verify minisig (deferred).
}

/// Poll the manifest; return `Some(UpdateInfo)` if `remote_version > current`.
/// Returns `None` when the manifest is unreachable, malformed, or already
/// up-to-date. Errors are logged-and-swallowed: the updater never blocks boot
/// and never surfaces fetch failures as user-visible errors (best-effort).
pub async fn check_for_updates(current_version: &str) -> Option<UpdateInfo> {
    let bytes = match fetch_manifest().await {
        Ok(b) => b,
        Err(e) => {
            tracing::info!(?e, "updater: manifest fetch failed (offline?)");
            return None;
        }
    };
    let manifest: Manifest = match serde_json::from_slice(&bytes) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(?e, "updater: manifest malformed");
            return None;
        }
    };
    if !is_newer(&manifest.version, current_version) {
        tracing::info!(
            new = %manifest.version,
            current = %current_version,
            "updater: on latest"
        );
        return None;
    }
    let entry = manifest.platforms.get("windows-x86_64")?;
    let url = entry.url.clone()?;
    Some(UpdateInfo {
        version: manifest.version,
        url,
        notes: manifest.notes.unwrap_or_default(),
    })
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

    // Exit immediately — releases every OS file lock on the install so the
    // updater's overwrite succeeds. The updater waits on our PID (passed above)
    // before it touches anything. This IPC call never returns to the frontend
    // on success (the process is gone); see the module doc.
    std::process::exit(0);
}

/// The outcome the updater.exe writes to `data/_update_result.json` for the
/// relaunched wupi.exe to read on its next boot (so the UI can show "Updated to
/// vX.Y.Z" or surface the error). Mirrors the JSON the updater binary writes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateResult {
    pub ok: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// Read + DELETE `data/_update_result.json` under `exe_dir`. Returns `None`
/// when the marker is absent (the common boot — no update just ran). The read
/// is destructive so the toast fires exactly once. A malformed marker is
/// dropped (returns `None`) — we never let a bad marker block boot.
pub fn read_and_clear_result(exe_dir: &Path) -> Option<UpdateResult> {
    let marker = exe_dir.join("data").join("_update_result.json");
    let contents = std::fs::read_to_string(&marker).ok()?;
    let result: UpdateResult = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, "updater: malformed _update_result.json — dropping");
            let _ = std::fs::remove_file(&marker);
            return None;
        }
    };
    let _ = std::fs::remove_file(&marker);
    Some(result)
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
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(MANIFEST_URL)
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
    while let Some(chunk) = stream
        .next()
        .await
        .transpose()
        .map_err(|e| format!("zip stream: {e}"))?
    {
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
