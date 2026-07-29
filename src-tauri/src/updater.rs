//! Portable self-updater: file-level replacement with user-data preservation.
//!
//! Replaces the Tauri updater plugin (which is installer-only and can't update
//! a portable exe). WUPI ships as a portable zip; this module downloads a new
//! zip from the GitHub release, extracts it in place, and replaces engine
//! files while preserving all four user-data top-level dirs (§8C):
//!   - `data/`   (user.xml, theme.json, api_config.json, docs/, _update/)
//!   - `memory/` (memory.sqlite)
//!   - `models/` (WUPI.gguf, Embed.gguf)
//!   - `apps/`   (per-card sessions, schemas, scenario cards, profiles)
//!
//! ## The preserve rule
//!
//! The portable zip ships engine content + the empty `data/` seed (wupi.sim +
//! wupi.codex + user.xml only). It never ships `memory/`, `models/`, or `apps/`
//! (release.cjs excludes them — fresh extracts have no runtime state). So the rule is:
//!
//! ```text
//! for each file in the zip:
//!     // Preserved user data (the four top-level dirs). Within data/, only
//!     // wupi.sim + wupi.codex are engine content and get overwritten on
//!     // update; user.xml is preserved so the user's identity survives.
//!     if rel starts with "data/" AND rel != "data/wupi.sim"
//!                              AND rel != "data/wupi.codex": skip
//!     if rel starts with "memory/": skip (defensive; zip shouldn't have it)
//!     if rel starts with "models/": skip (defensive; zip shouldn't have it)
//!     if rel starts with "apps/":   skip (defensive; zip shouldn't have it)
//!     else if the file is wupi.exe: apply the rename-and-relaunch dance
//!     else: overwrite the destination in place
//! ```
//!
//! The four-dir carve-out is the entire preservation contract. No per-file
//! classification list.
//!
//! ## The Windows locked-exe dance
//!
//! Windows locks the running `wupi.exe`: it can be renamed but not deleted or
//! overwritten while the process is alive. The update sequence is therefore:
//!
//! 1. Download `portable.zip` to `<exe_dir>/data/_update/portable.zip.part`.
//! 2. Verify (deferred for the beta — HTTPS + GitHub release auth is the
//!    trust boundary; signature verification can be layered on later without
//!    changing this flow).
//! 3. Extract the zip into `<exe_dir>/data/_update/extracted/`.
//! 4. For each file in the extract: apply the preserve rule (above).
//!    - For `wupi.exe`: rename the current exe to `wupi.exe.old` (Windows
//!      permits renaming a running binary), then move the new exe into place.
//!      `wupi.exe.old` is deleted on the next boot (see `cleanup_old_files`).
//!    - For every other file: [`copy_file_robust`] copies in place on the fast
//!      path; if the destination is locked by the Windows loader (e.g.
//!      `msvcp140.dll` at the install root, or the `bin/` CUDA DLLs once the
//!      model is loaded) it renames the locked file to `<name>.old` and copies
//!      the new one into the vacated path. The `.old` remnant is swept on the
//!      next boot alongside `wupi.exe.old`. This is what fixed the v0.6.0
//!      updater bug where an in-place DLL copy failed with
//!      `ERROR_SHARING_VIOLATION`, aborting the whole apply.
//! 5. Emit `update-applied`; the frontend prompts the user to restart. On
//!    restart, `app.restart()` relaunches the new exe and the old process
//!    exits (the OS releases its lock, allowing `wupi.exe.old` + DLL `.old`
//!    cleanup).
//!
//! ## Why not auto-restart
//!
//! The user always clicks "Restart now." A silent restart mid-session would
//! discard any in-flight generation; the click is the consent gate.

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
    // Semver-ish compare: equal or older = no update. We don't pull in a semver
    // crate for one comparison; the publish side bumps version monotonically,
    // so a string compare is sufficient in practice. (If versioning ever goes
    // non-monotonic, swap in semver here.)
    if !is_newer(&manifest.version, current_version) {
        tracing::info!(
            new = %manifest.version,
            current = %current_version,
            "updater: on latest"
        );
        return None;
    }
    // Find the windows-x86_64 platform entry (the only one we publish).
    let entry = manifest.platforms.get("windows-x86_64")?;
    let url = entry.url.clone()?;
    Some(UpdateInfo {
        version: manifest.version,
        url,
        notes: manifest.notes.unwrap_or_default(),
    })
}

/// Apply a pending update: download → extract → swap files → emit event.
///
/// `app_handle` is used for path resolution (finds `<exe_dir>`) and event
/// emission. `update` is the [`UpdateInfo`] from [`check_for_updates`].
/// Progress events (`update-progress`, 0..=100) fire as the download streams.
/// The `update-applied` event fires once when the swap is complete + safe.
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
    // attempt re-downloads (correct: the zip is small enough that resuming
    // adds complexity for no real win).
    std::fs::rename(&zip_part, &zip_final).map_err(|e| format!("rename .part: {e}"))?;

    // ── Phase 2: extract ──────────────────────────────────────────────────
    let extracted = staging.join("extracted");
    // Clean slate: remove any leftover from a prior attempt.
    if extracted.exists() {
        std::fs::remove_dir_all(&extracted)
            .map_err(|e| format!("clean extracted/: {e}"))?;
    }
    std::fs::create_dir_all(&extracted).map_err(|e| format!("create extracted/: {e}"))?;
    extract_zip(&zip_final, &extracted)?;

    // ── Phase 3: apply with the preserve rule ─────────────────────────────
    apply_extracted(&extracted, &exe_dir)?;

    // ── Phase 4: cleanup staging ──────────────────────────────────────────
    // Remove the ENTIRE staging dir (`data/_update/`) — the zip, the
    // extracted tree, AND the dir itself. Nothing the update left behind
    // should persist: no extra folders, no .old files, no zip cache. The dir
    // is recreated on the next update's Phase 1 if needed.
    let _ = std::fs::remove_dir_all(&staging);

    let _ = app_handle.emit("update-applied", &update);
    Ok(())
}

/// Delete EVERY remnant from a prior self-update. Called from `setup()` on
/// every boot — by the time the new process runs, the old one's locks are
/// gone. This is the "leave nothing behind" sweep. It clears THREE classes
/// of leftover:
///
/// 1. `wupi.exe.old` — the running-exe swap dance (`swap_running_exe`).
/// 2. Any `<name>.old` DLL remnant — `copy_file_robust` renames a locked/
///    loaded DLL (`msvcp140.dll.old`, `cublas64_13.dll.old`, …) out of the
///    way to copy the new one in. By boot the lock is gone, so these delete
///    cleanly.
/// 3. The whole `data/_update/` staging dir — if an update was interrupted
///    (crash, power loss, killed mid-download) the staging zip + extracted
///    tree can be left behind. `perform_update` removes it on success; this
///    is the defensive sweep for the failure case.
///
/// **The contract: after this runs, there are no `.old` files, no update
/// folders, no backups, no staging cache — only the live install.** Per spec:
/// "completely delete everything old that the update is meant to replace."
///
/// Best-effort: a delete failure (file transiently in use, perms) is logged
/// and retried next boot. Scans `exe_dir` non-recursively + `bin/` (flat) —
/// the only two places the rename dance ever writes `.old` files.
pub fn cleanup_old_files(app_handle: &tauri::AppHandle) {
    let Some(exe_dir) = exe_dir(app_handle) else {
        return;
    };
    let mut swept = 0;
    // ── .old file sweep: exe_dir root + bin/. Non-recursive (bin/ is flat,
    //    and the rename dance only ever touches files at these two levels).
    let mut scan_dirs = vec![exe_dir.clone()];
    let bin_dir = exe_dir.join("bin");
    if bin_dir.is_dir() {
        scan_dirs.push(bin_dir);
    }
    for dir in scan_dirs {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Match `<anything>.old` exactly (case-insensitive on Windows).
            let is_old = path
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("old"))
                .unwrap_or(false);
            if !is_old {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    swept += 1;
                    tracing::info!(?path, "cleaned up .old remnant from prior update");
                }
                Err(e) => tracing::warn!(?path, ?e, "could not remove .old remnant; will retry next boot"),
            }
        }
    }
    // ── Staging dir sweep: data/_update/ (interrupted-update remnants).
    //    Removed wholesale — the zip, the extracted tree, and the dir itself.
    //    Recreated on the next update's Phase 1 if needed.
    let staging = exe_dir.join("data").join("_update");
    if staging.is_dir() {
        match std::fs::remove_dir_all(&staging) {
            Ok(()) => {
                swept += 1;
                tracing::info!(?staging, "removed leftover update staging dir");
            }
            Err(e) => tracing::warn!(?staging, ?e, "could not remove staging dir; will retry next boot"),
        }
    }
    if swept > 0 {
        tracing::info!(swept, "cleaned up remnants from prior update");
    }
}

/// Delete deprecated assets left at the install root by older versions.
///
/// Historically every file in `public/` was copied flat to the install root
/// by Vite + `release.cjs`. Several of those assets turned out to be dead
/// (never referenced by the running app) or were relocated/renamed:
///
/// - The 4-track soundtrack library (soundtrack.js is not wired in):
///   `Across_the_Verdant_Ridge.mp3`, `Iron_and_Silk.mp3`,
///   `Promises_in_the_Pavilion.mp3`, `Thunder_and_Salt.mp3`.
/// - The map atlases (panels/map.js is not wired in): `map-fantasy-atlas.png`,
///   `map-futuristic-atlas.png`, `map-modern-atlas.png`.
/// - The starting-scene PNGs (zero references anywhere): the 6
///   `starting-*.png` files.
/// - `fable_title.png` — relocated into `src/fable/assets/` (Vite now hashes
///   it into `assets/`); the old flat-root copy is stale.
/// - `fable_whoosh.mp3` — renamed to `wind.mp3` (2026-07-27); the old name
///   is stale.
/// - `wind.mp3` — the renamed successor of `fable_whoosh.mp3`, intended for a
///   future intro cue. Removed (2026-07-27): nothing in the app references it
///   and it was leaking flat to the install root on every build. It lives in
///   the build workstation archive for later reintroduction.
///
/// New builds no longer ship any of these to the root. This sweep clears
/// them from installs that received them via a prior update, so the root
/// converges on the §8C layout (just `wupi.exe`, `wupi.html`, `assets/`,
/// `bin/`, `data/`, `msvcp140.dll`). Called from `setup()` on every boot,
/// alongside [`cleanup_old_files`].
///
/// Non-recursive (scans `exe_dir` only): every deprecated asset lived flat
/// at the root. Best-effort: a delete failure is logged + retried next boot.
/// The list is exact lowercase basenames; Windows FS is case-insensitive so
/// the case-matching is forgiving in practice, but the names below are the
/// ones Vite emitted.
pub fn cleanup_deprecated_assets(app_handle: &tauri::AppHandle) {
    let Some(exe_dir) = exe_dir(app_handle) else { return; };
    let _ = cleanup_deprecated_in_dir(&exe_dir);
}

/// The testable core: walks `DEPRECATED_ASSETS` against `dir` and removes
/// any that exist. Returns the count removed. Best-effort — a delete failure
/// is logged and the loop continues (the file gets a retry next boot).
fn cleanup_deprecated_in_dir(dir: &Path) -> usize {
    let mut swept = 0;
    for name in DEPRECATED_ASSETS {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                swept += 1;
                tracing::info!(?path, "removed deprecated asset from prior version");
            }
            Err(e) => tracing::warn!(?path, ?e, "could not remove deprecated asset; will retry next boot"),
        }
    }
    if swept > 0 {
        tracing::info!(swept, "cleaned up deprecated assets from prior version");
    }
    swept
}

/// The exact basenames of assets older versions shipped flat to the install
/// root that this version no longer ships. See [`cleanup_deprecated_assets`]
/// for the per-entry rationale. Module-scope `const` so the unit test can
/// assert against it without going through `AppHandle`.
const DEPRECATED_ASSETS: &[&str] = &[
    // Dead soundtrack library (soundtrack.js not wired in).
    "Across_the_Verdant_Ridge.mp3",
    "Iron_and_Silk.mp3",
    "Promises_in_the_Pavilion.mp3",
    "Thunder_and_Salt.mp3",
    // Dead map atlases (panels/map.js not wired in).
    "map-fantasy-atlas.png",
    "map-futuristic-atlas.png",
    "map-modern-atlas.png",
    // Dead starting-scene PNGs (zero references).
    "starting-apartment-studio.png",
    "starting-classroom-modern.png",
    "starting-frontier-airship-dock.png",
    "starting-moonlit-guild-hall.png",
    "starting-neon-transit-platform.png",
    "starting-rainy-cafe.png",
    // Relocated/renamed: old flat-root copies are stale.
    "fable_title.png",  // → src/fable/assets/ (Vite-hashed into assets/)
    "fable_whoosh.mp3", // → renamed wind.mp3 (2026-07-27)
    // Dead: never wired in; lives in the build-workstation archive for later
    // reintroduction. New builds no longer ship it to the root.
    "wind.mp3",         // removed from public/ 2026-07-27 (dead mp3)
];

// ── Internals ──────────────────────────────────────────────────────────────

/// Resolve `<exe_dir>` — the directory containing `wupi.exe`.
fn exe_dir(app_handle: &tauri::AppHandle) -> Option<PathBuf> {
    let _ = app_handle; // unused on the happy path; kept for symmetry/future use
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
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
async fn download_with_progress(
    url: &str,
    dest: &Path,
    app_handle: &tauri::AppHandle,
) -> Result<(), String> {
    use futures_util::StreamExt;
    use tauri::Emitter;
    use tokio::io::AsyncWriteExt;

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

/// Extract `zip_path` into `dest`. Uses the `zip` crate (pure Rust, no system
/// deps). Preserves directory structure; skips entries that would escape
/// `dest` (path-traversal defense: reject any entry whose canonicalized path
/// isn't under `dest`).
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("open archive: {e}"))?;
    let dest_canon = dest
        .canonicalize()
        .map_err(|e| format!("canonicalize dest: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("read entry {i}: {e}"))?;
        let entry_path = match entry.enclosed_name() {
            Some(p) => p,
            None => continue, // skip unsafe paths (the zip crate's own guard)
        };
        let out = dest.join(&entry_path);
        // Belt-and-suspenders: re-check the joined path is under dest.
        let parent = out.parent().unwrap_or(dest);
        if let Ok(parent_canon) = parent.canonicalize().or_else(|_| std::fs::create_dir_all(parent).and_then(|_| parent.canonicalize())) {
            if !parent_canon.starts_with(&dest_canon) {
                tracing::warn!(?out, "zip entry escapes dest; skipping");
                continue;
            }
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
        } else {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
            let mut out_file = std::fs::File::create(&out)
                .map_err(|e| format!("create {}: {e}", out.display()))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("write {}: {e}", out.display()))?;
        }
    }
    Ok(())
}

/// Walk `extracted/` and copy files into `exe_dir` with the preserve rule
/// (§8C). Carve-outs:
/// - `data/` is preserved EXCEPT `data/wupi.sim` + `data/wupi.codex` (engine
///   content; persona + playbook updates ship in the zip and overwrite the
///   local copy on update — Chloe's call on the §8C internal contradiction).
/// - `memory/`, `models/`, `apps/` are fully preserved (defensive — the zip
///   shouldn't ship these, but the rule is total).
/// - `wupi.exe` is swapped via the rename-and-relaunch dance.
/// - Everything else is overwritten in place via [`copy_file_robust`] — which
///   transparently handles the locked-DLL case (a loaded `msvcp140.dll` or
///   `bin/` CUDA DLL can't be overwritten in place, so it's renamed to `.old`
///   and the new file copied into the vacated path; swept on next boot).
fn apply_extracted(extracted: &Path, exe_dir: &Path) -> Result<(), String> {
    let entries = walk_files(extracted)?;
    let exe_name = exe_basename();
    for src in entries {
        // Relative path from the extract root → the install root.
        let rel = match src.strip_prefix(extracted) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // The preserve rule (§8C): the four user-data top-level dirs are
        // preserved. Within data/, wupi.sim + wupi.codex are engine content
        // and get overwritten; everything else in data/ (user.xml, theme.json,
        // api_config.json, docs/) is preserved.
        if is_preserved(rel) {
            tracing::info!(?rel, "preserve: user-data entry skipped");
            continue;
        }
        let dst = exe_dir.join(rel);
        if rel == Path::new(&exe_name) {
            // The running exe: rename + move. Windows permits renaming a
            // running binary but not overwriting it.
            swap_running_exe(&src, &dst)?;
            continue;
        }
        // Plain file overwrite. Create parent dirs (e.g. data/, assets/).
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir parent: {e}"))?;
        }
        copy_file_robust(&src, &dst)
            .map_err(|e| format!("copy {} -> {}: {e}", src.display(), dst.display()))?;
        tracing::info!(?rel, "updated");
    }
    Ok(())
}

/// The §8C preserve rule as a predicate. Returns true for paths that must NOT
/// be overwritten by an update (user data). The engine-content exceptions
/// (shipped in the zip, replaced verbatim on update) are:
/// - `data/wupi.sim` + `data/wupi.codex` — Wupi's persona + her static playbook.
/// - `data/gm.sim` + `data/fable.codex` — the Game Master persona + the unified
///   Fable playbook (shared by the New Game interview persona AND the
///   simulation narrator; fable.codex is the 2026-07-29 unification, was
///   gm.codex).
///
/// `rel` is the file's path relative to the extract root (e.g. `data/user.xml`,
/// `memory/memory.sqlite`, `wupi.exe`).
fn is_preserved(rel: &Path) -> bool {
    // data/: preserved EXCEPT the four engine-content files above.
    if rel.starts_with("data") {
        return rel != Path::new("data/wupi.sim")
            && rel != Path::new("data/wupi.codex")
            && rel != Path::new("data/gm.sim")
            && rel != Path::new("data/fable.codex");
    }
    // memory/, models/, apps/: fully preserved.
    rel.starts_with("memory") || rel.starts_with("models") || rel.starts_with("apps")
}

/// The exe basename on this platform (`wupi.exe` on Windows). Stubbed for
/// non-Windows to keep the module portable for tests.
fn exe_basename() -> String {
    if cfg!(windows) {
        "wupi.exe".to_string()
    } else {
        "wupi".to_string()
    }
}

/// Swap the running exe (`dst`, currently in use) with the new one (`src`).
///
/// Windows locks the running exe: it can be renamed but not overwritten while
/// the process is alive. So:
///   1. If a `wupi.exe.old` from a PRIOR update still exists (the last boot's
///      `cleanup_old_files` didn't run or failed), delete it first. This is
///      load-bearing: without it, the `rename` below would leave a STALE
///      `.old` that predates this update — the kind of remnant that
///      accumulates update-over-update and never clears. A pre-existing
///      `.old` is never the live file (it's always a leftover), so deleting
///      it here is safe.
///   2. Rename `dst` (`wupi.exe`) → `wupi.exe.old`. Windows permits this.
///   3. Move `src` (the extracted new exe) → `dst`.
///   4. `wupi.exe.old` is deleted on the NEXT boot by [`cleanup_old_files`]
///      (the old process still holds its lock until exit; deletion here
///      would fail with "file in use" and serve no purpose).
fn swap_running_exe(src: &Path, dst: &Path) -> Result<(), String> {
    let old = dst.with_extension("exe.old");
    // Defensive: clear any stale `.old` from a prior update before we drop a
    // fresh one. If it's somehow still locked (shouldn't be — it's not the
    // live exe), ignore the error; the rename below will still succeed and
    // `cleanup_old_files` will retry on the next boot.
    if old.exists() {
        match std::fs::remove_file(&old) {
            Ok(()) => tracing::info!(?old, "removed stale .old before swap"),
            Err(e) => tracing::warn!(?old, ?e, "could not remove stale .old; continuing"),
        }
    }
    std::fs::rename(dst, &old)
        .map_err(|e| format!("rename {} to {}: {e}", dst.display(), old.display()))?;
    std::fs::rename(src, dst)
        .map_err(|e| format!("move new exe into place: {e}"))?;
    tracing::info!("swapped running exe; .old will be cleaned up on next boot");
    Ok(())
}

/// Copy `src` → `dst`, transparently handling the case where `dst` is locked
/// by the Windows loader (a loaded DLL) or by another open handle.
///
/// **The locked-DLL problem (the v0.6.0 updater bug):** the Windows loader
/// maps static-import DLLs (`msvcp140.dll` at the install root) and
/// LoadLibrary'd DLLs (the `bin/` CUDA set, once the model is running) with
/// `FILE_SHARE_READ` but **not** `FILE_SHARE_WRITE`. `std::fs::copy` opens the
/// destination with `GENERIC_WRITE`, which the loader rejects with
/// `ERROR_SHARING_VIOLATION` (OS error 32). The same applies to any file held
/// open by the running process.
///
/// **The fix (same principle as `swap_running_exe`):** Windows permits
/// *renaming* a locked/loaded file even when it can't be overwritten. So on a
/// sharing/permission error we:
///   1. Rename `dst` → `dst.with_extension("<orig>.old")` (e.g.
///      `msvcp140.dll` → `msvcp140.dll.old`).
///   2. Copy `src` → `dst` into the now-vacated path.
///   3. The `.old` remnant is swept on the next boot by [`cleanup_old_files`]
///      (by then the process holding the lock has exited).
///
/// Fast path: for the vast majority of files (`wupi.html`, `assets/*`,
/// `data/wupi.sim` — none of which are locked) `std::fs::copy` succeeds and
/// we pay zero overhead. The rename fallback only fires on the rare locked
/// file.
fn copy_file_robust(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    match std::fs::copy(src, dst) {
        Ok(_) => Ok(()),
        Err(e) if is_sharing_violation(&e) => {
            // Locked by the loader / an open handle. Rename it out of the way
            // (Windows allows this even when overwrite/delete is blocked) and
            // copy the new file into the vacated path.
            let backup = with_old_extension(dst);
            // If a prior .old already exists (interrupted update), remove it
            // first — it's a remnant from a previous attempt, not the live
            // file, so deletion is safe once the old process is gone. If it's
            // STILL locked (shouldn't be — it's a stale remnant), fall through
            // to the error; the next boot's cleanup will retry.
            let _ = std::fs::remove_file(&backup);
            std::fs::rename(dst, &backup)?;
            std::fs::copy(src, dst)?;
            tracing::info!(
                dst = %dst.display(),
                "copied locked file via rename → .old (will be swept on next boot)"
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Heuristic: does this io::Error look like a Windows sharing violation or a
/// permission-denied lock? `ERROR_SHARING_VIOLATION` (32) surfaces via
/// `raw_os_error`. We also catch `PermissionDenied` because some lock
/// scenarios surface as EACCES rather than ERROR_SHARING_VIOLATION.
fn is_sharing_violation(e: &std::io::Error) -> bool {
    if let Some(code) = e.raw_os_error() {
        // 32 = ERROR_SHARING_VIOLATION (Windows). 5 = ERROR_ACCESS_DENIED.
        if code == 32 || code == 5 {
            return true;
        }
    }
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
}

/// Build the `.old` backup path for a locked destination. We can't use
/// `Path::with_extension("old")` because that *replaces* the extension (so
/// `msvcp140.dll` → `msvcp140.old`, which loses the `.dll` and confuses the
/// cleanup sweep). Instead append `.old` to the full filename:
/// `msvcp140.dll` → `msvcp140.dll.old`.
fn with_old_extension(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".old");
    path.with_file_name(name)
}

/// Recursively collect all files under `root` (depth-first). Used by
/// [`apply_extracted`] to walk the extracted tree.
fn walk_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read_dir: {e}"))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deprecated_list_is_nonempty_and_unique() {
        // Sanity: the list is the contract — every entry must be a real
        // filename we stopped shipping. Empty or duplicate entries would
        // indicate a copy-paste error in the cleanup definition.
        assert!(!DEPRECATED_ASSETS.is_empty(), "deprecated list must not be empty");
        let mut sorted = DEPRECATED_ASSETS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "deprecated list must have no duplicates");
        // Every entry has an extension (sanity: these are all asset files).
        for name in DEPRECATED_ASSETS {
            assert!(Path::new(name).extension().is_some(), "{name} has no extension");
        }
    }

    #[test]
    fn cleanup_removes_only_deprecated_assets() {
        // Stage a fake install root with: one deprecated asset, one legit
        // engine file (must survive), one user-data file (must survive),
        // and one nested file under a subdir (must survive — sweep is
        // non-recursive at the install root).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        std::fs::write(root.join("fable_whoosh.mp3"), b"old wind").unwrap(); // deprecated
        std::fs::write(root.join("wupi.exe"), b"engine").unwrap();           // legit engine
        std::fs::write(root.join("paw.png"), b"mascot").unwrap();            // legit engine asset
        std::fs::write(root.join("data").join("user.xml"), b"user").unwrap_or_else(|_| {
            std::fs::create_dir_all(root.join("data")).unwrap();
            std::fs::write(root.join("data").join("user.xml"), b"user").unwrap()
        });
        // A deprecated filename placed UNDER a subdir must NOT be swept — the
        // real sweep is non-recursive at the exe_dir root.
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets").join("fable_title.png"), b"hashed").unwrap();

        let swept = cleanup_deprecated_in_dir(root);
        assert_eq!(swept, 1, "exactly one deprecated file at root should be swept");

        // The deprecated flat-root files are gone.
        assert!(!root.join("fable_whoosh.mp3").exists(), "deprecated file should be removed");
        // Legit engine + mascot asset + user data survive.
        assert!(root.join("wupi.exe").exists(), "engine file must survive");
        assert!(root.join("paw.png").exists(), "legit engine asset must survive");
        assert!(root.join("data").join("user.xml").exists(), "user data must survive");
        // The nested same-named file under assets/ survives (non-recursive).
        assert!(
            root.join("assets").join("fable_title.png").exists(),
            "nested file under a subdir must survive (sweep is root-only)"
        );
    }

    #[test]
    fn cleanup_is_noop_on_clean_install() {
        // A fresh install that never received the deprecated files: the
        // sweep removes nothing and errors on nothing.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("wupi.exe"), b"engine").unwrap();
        let swept = cleanup_deprecated_in_dir(tmp.path());
        assert_eq!(swept, 0, "clean install should sweep nothing");
        assert!(tmp.path().join("wupi.exe").exists());
    }

    #[test]
    fn cleanup_sweeps_every_deprecated_entry_when_present() {
        // The strongest contract test: if EVERY deprecated file is present at
        // the root, ALL of them get removed in one pass.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for name in DEPRECATED_ASSETS {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let swept = cleanup_deprecated_in_dir(root);
        assert_eq!(
            swept,
            DEPRECATED_ASSETS.len(),
            "every deprecated file present should be swept"
        );
        for name in DEPRECATED_ASSETS {
            assert!(!root.join(name).exists(), "{name} should be gone");
        }
    }
}
