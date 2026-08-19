//! The two-phase apply.
//!
//! 1. **Stage** the payload to a fresh `%TEMP%` dir and verify it contains
//!    `wupi.exe` (bricking-safety gate — a corrupt/truncated/wrong zip is
//!    rejected before the live install is touched).
//! 2. **Copy** the staged payload into the install root, honoring the preserve
//!    rule (skip user data). All locks are released (wupi.exe has exited), so
//!    plain `std::fs::copy` succeeds.

use std::path::{Path, PathBuf};

/// Extract the whole zip into `staging` (a fresh `%TEMP%` dir), then verify the
/// payload contains `wupi.exe`. Returns the entry count. The live install is
/// NOT touched here — staging is the bricking-safety gate.
pub fn extract_to_staging(zip_path: &Path, staging: &Path) -> Result<usize, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("open zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("read zip: {e}"))?;
    let staging_canon = staging
        .canonicalize()
        .map_err(|e| format!("canonicalize staging: {e}"))?;
    let count = archive.len();
    for i in 0..count {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;
        // enclosed_name() is the zip crate's own path-traversal guard.
        let entry_path = match entry.enclosed_name() {
            Some(p) => p,
            None => continue,
        };
        let out = staging.join(entry_path);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            // Belt-and-suspenders: the joined parent must stay under staging.
            if let Ok(parent_canon) = parent.canonicalize() {
                if !parent_canon.starts_with(&staging_canon) {
                    continue;
                }
            }
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&out)
                .map_err(|e| format!("mkdir {}: {e}", out.display()))?;
        } else {
            let mut out_file = std::fs::File::create(&out)
                .map_err(|e| format!("create {}: {e}", out.display()))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("write {}: {e}", out.display()))?;
        }
    }
    // bricking-safety gate: refuse a payload with no wupi.exe (truncated/wrong
    // zip) before we touch the live install.
    let exe_name = crate::exe_basename();
    if !staging.join(&exe_name).is_file() {
        return Err(format!("payload missing {exe_name} — refusing to apply"));
    }
    Ok(count)
}

/// Walk `staging` and copy each file into `target_dir`, skipping preserved
/// user-data paths. All locks are released (wupi.exe has exited), so plain
/// `std::fs::copy` succeeds; a failure here is a real I/O issue (disk full,
/// antivirus) and is recorded as a partial-apply error.
///
/// (#40 2026-08-15) Each file lands via copy-to-temp + RENAME: `fs::copy`
/// TRUNCATES the destination first, so a disk-full mid-copy used to leave
/// `wupi.exe` itself corrupt (half-written + unbootable). With the temp
/// staging name, a failed copy never touches the live file — the rename
/// (which REPLACES the destination atomically on Windows) only runs after
/// the temp file is complete. A mid-sequence failure now leaves every file
/// EITHER fully-old OR fully-new, never truncated.
///
/// (2026-08-18 relaunch-killer fix) A single-attempt copy is NOT enough:
/// transient lockers (AV/indexer scanning the freshly-written ~1.2 GB of
/// executable content) can hold one file for a few seconds, and ONE
/// `Access is denied` used to fail the whole phase → `install_touched` →
/// relaunch skipped → WUPI silently never came back (observed live on the
/// 0.24.1 → 0.24.2 hop: `bin/VCRUNTIME140.dll` lost one rename race). Two
/// resilience layers, both confined to the failure path:
///
/// 1. **Retry with backoff** — failed files are re-attempted across ~30 s
///    (same schedule shape as `spawn_wupi_robust`); a transient lock
///    converts into a wait.
/// 2. **Byte-identity exemption** — a file that is STILL locked after all
///    retries but already byte-identical to the payload (runtime DLLs
///    rarely change between releases — the 0.24.2 case: the "unreplaced"
///    May-dated VCRUNTIME140.dll hashed EQUAL to the payload copy) leaves
///    the install at the target content. It is not version-mixed, so it
///    neither fails the update nor justifies skipping the relaunch.
pub fn copy_into_target(staging: &Path, target: &Path) -> Result<(), String> {
    /// Backoff seconds between copy retry rounds. Five rounds spanning
    /// ~30 s: long enough to outlast a realistic AV/indexer scan window,
    /// short enough that a genuinely-broken copy doesn't hang the headless
    /// updater for minutes.
    const COPY_RETRY_BACKOFF_SECS: [u64; 5] = [2, 4, 6, 8, 10];

    let files = walk_files(staging)?;
    // (src, rel, last error) for files that could not be replaced — yet.
    let mut failed: Vec<(PathBuf, PathBuf, String)> = Vec::new();
    // Non-retryable structural failures (mkdir) go straight to the report.
    let mut errors: Vec<String> = Vec::new();
    for src in &files {
        let rel = match src.strip_prefix(staging) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if crate::preserve::is_preserved(rel) {
            continue;
        }
        let dst = target.join(rel);
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                errors.push(format!("mkdir {}: {e}", parent.display()));
                continue;
            }
        }
        if let Err(e) = copy_atomic(src, &dst) {
            failed.push((src.clone(), rel.to_path_buf(), e));
        }
    }

    if !failed.is_empty() {
        crate::log(format!(
            "{} file(s) failed the first copy pass (first: {}) — retrying with backoff",
            failed.len(),
            failed[0].2
        ));
    }
    for backoff in COPY_RETRY_BACKOFF_SECS {
        if failed.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(backoff));
        let mut still: Vec<(PathBuf, PathBuf, String)> = Vec::new();
        for (src, rel, _err) in failed {
            match copy_atomic(&src, &target.join(&rel)) {
                Ok(()) => {
                    crate::log(format!("copy of {} recovered on retry", rel.display()))
                }
                Err(e) => still.push((src, rel, e)),
            }
        }
        failed = still;
    }

    if !failed.is_empty() {
        failed.retain(|(src, rel, _err)| {
            let identical = files_identical(src, &target.join(rel));
            if identical {
                crate::log(format!(
                    "{} still locked but byte-identical to the payload — install not version-mixed",
                    rel.display()
                ));
            }
            !identical
        });
    }

    errors.extend(
        failed
            .iter()
            .map(|(_src, rel, e)| format!("copy {}: {e}", rel.display())),
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} file(s) failed to copy (first: {})",
            errors.len(),
            errors[0]
        ))
    }
}

/// Byte-compare two files, chunked + length-gated. A missing/unreadable
/// file is never identical. Only called on the retry-exhausted failure
/// path, so the read cost is confined to files that could not be replaced.
fn files_identical(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    let (ma, mb) = match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => (ma, mb),
        _ => return false,
    };
    if ma.len() != mb.len() {
        return false;
    }
    let (mut fa, mut fb) = match (std::fs::File::open(a), std::fs::File::open(b)) {
        (Ok(fa), Ok(fb)) => (fa, fb),
        _ => return false,
    };
    let mut buf_a = vec![0u8; 64 * 1024];
    let mut buf_b = vec![0u8; 64 * 1024];
    loop {
        let (na, nb) = match (fa.read(&mut buf_a), fb.read(&mut buf_b)) {
            (Ok(na), Ok(nb)) => (na, nb),
            _ => return false,
        };
        if na == 0 && nb == 0 {
            return true;
        }
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return false;
        }
    }
}

/// Copy `src` to a sibling temp name of `dst`, then rename over `dst`.
/// `std::fs::rename` on Windows replaces an existing destination, so the
/// live file is only ever swapped for a COMPLETE copy — a failure at either
/// step leaves the destination exactly as it was (and no temp residue).
fn copy_atomic(src: &Path, dst: &Path) -> Result<(), String> {
    let base = dst
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("payload");
    let tmp = dst.with_file_name(format!("{base}.wupi_new"));
    let res = (|| -> Result<(), String> {
        std::fs::copy(src, &tmp).map_err(|e| format!("stage {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, dst).map_err(|e| format!("replace {}: {e}", dst.display()))
    })();
    if res.is_err() {
        // Never leave a temp residue next to the live file.
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Recursively collect all files under `root` (depth-first).
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
    use std::io::Write;
    use tempfile::TempDir;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        use zip::write::SimpleFileOptions;
        use zip::ZipWriter;
        let file = std::fs::File::create(path).unwrap();
        let mut zw = ZipWriter::new(file);
        let opts = SimpleFileOptions::default();
        for (name, data) in entries {
            zw.start_file(name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
    }

    #[test]
    fn extract_then_copy_respects_preserve() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("install");
        let staging = tmp.path().join("stage");
        let zip_path = tmp.path().join("payload.zip");

        // Pre-existing live install: an OLD wupi.exe + a user-data file that
        // must survive the update (data/user.xml is preserved).
        std::fs::create_dir_all(target.join("data")).unwrap();
        std::fs::write(target.join("wupi.exe"), b"OLD EXE").unwrap();
        std::fs::write(target.join("data/user.xml"), b"OLD USER").unwrap();

        // Payload: a NEW exe, a NEW engine file (data/wupi.sim overwrites), a
        // NEW user.xml (MUST be ignored — preserved), + a fresh asset file.
        make_zip(
            &zip_path,
            &[
                ("wupi.exe", b"NEW EXE".as_slice()),
                ("data/wupi.sim", b"NEW SIM"),
                ("data/user.xml", b"NEW USER (must not apply)"),
                ("assets/app.js", b"ASSET"),
            ],
        );

        std::fs::create_dir_all(&staging).unwrap();
        let n = extract_to_staging(&zip_path, &staging).unwrap();
        assert_eq!(n, 4);
        copy_into_target(&staging, &target).unwrap();

        // wupi.exe overwritten with the new payload.
        assert_eq!(std::fs::read(target.join("wupi.exe")).unwrap(), b"NEW EXE");
        // Engine content under data/ overwritten.
        assert_eq!(
            std::fs::read(target.join("data/wupi.sim")).unwrap(),
            b"NEW SIM"
        );
        // Fresh asset landed.
        assert_eq!(
            std::fs::read(target.join("assets/app.js")).unwrap(),
            b"ASSET"
        );
        // User data PRESERVED — the payload's data/user.xml was ignored.
        assert_eq!(
            std::fs::read(target.join("data/user.xml")).unwrap(),
            b"OLD USER"
        );
    }

    #[test]
    fn extract_rejects_payload_without_wupi_exe() {
        let tmp = TempDir::new().unwrap();
        let staging = tmp.path().join("stage");
        let zip_path = tmp.path().join("payload.zip");
        std::fs::create_dir_all(&staging).unwrap();
        make_zip(&zip_path, &[("assets/x.txt", b"x".as_slice())]);
        let err = extract_to_staging(&zip_path, &staging).unwrap_err();
        assert!(err.contains("missing"));
    }
}
