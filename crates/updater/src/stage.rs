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
pub fn copy_into_target(staging: &Path, target: &Path) -> Result<(), String> {
    let files = walk_files(staging)?;
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
        if let Err(e) = std::fs::copy(src, &dst) {
            errors.push(format!("copy {}: {e}", rel.display()));
        }
    }
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
