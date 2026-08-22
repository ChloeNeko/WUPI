//! The §8C legacy-path purge — dead files + folders removed from the live
//! install as part of applying an update.
//!
//! **Design (2026-08-14, replacing the retired `delete.json` manifest):** the
//! removal list is COMPILE-TIME constants in this binary — no manifest file
//! ships in the zip, lands in the install root, or needs syncing between repo
//! and release. The updater itself IS the temp-file deleter Chloe described:
//! wupi.exe creates the `%TEMP%` copy, that copy deletes the dead paths during
//! apply, and then it self-deletes (`main.rs::self_delete_temp_copy`) so there
//! are no remnants of the deleter anywhere — not in the install, not in
//! `%TEMP%`.
//!
//! Because the list is compiled in, EVERY update re-runs the purge —
//! idempotent by construction (`remove_dir_all`/`remove_file` on a missing
//! path is a logged no-op), so any install hopping from any old version gets
//! cleaned, and appending a future removal is a one-line const edit + release.
//!
//! This is the ONLY deletion mechanism — the updater does NOT do implicit
//! "delete anything not in the zip" reconciliation (that would be a footgun
//! against the preserve rule: user data legitimately exists that no zip
//! contains).

use std::path::Path;

/// Backoff seconds between `purge_update_staging` removal attempts — the
/// same ~30 s shape as `stage::copy_into_target`'s retry schedule, because it
/// outlives the same enemy: the AV/indexer pass over freshly-written content.
/// The documented 0.23.3→0.23.5 live failure was this exact race — the one-
/// shot `data/_update` delete lost to a lock on the downloaded zip, the error
/// was swallowed, and nothing ever retried.
const UPDATE_STAGING_BACKOFF_SECS: [u64; 5] = [2, 4, 6, 8, 10];

/// Dead paths (forward-slash, relative to the install root) purged from the
/// live install on apply. Compile-time constants — no parsing, no traversal
/// surface (unlike the old delete.json, which needed canonicalize +
/// containment checks against a SHIPPED file).
///
/// - `apps/fable/{sessions,schemas,saves,profiles}` — pre-per-card-layout
///   state roots, dead since the 2026-08-01 §6B reorg (state lives inside
///   `cards/<card_id>/` now; players live in `apps/fable/players/`). v0.17/
///   v0.18 still created them eagerly at boot, so every upgraded install
///   has all four.
/// - `apps/fable/backgrounds` — the pre-Library scene-art dir. v0.18 created
///   it eagerly at boot; the live path is `apps/fable/images/backgrounds`
///   (§7 "Stage Background Library"). Nothing ever resolved the old dir.
/// - `apps/games` — the entire v0.6.x pre-Fable-rename state root (the boot
///   migration that drained it is gone; this reaps whatever it left).
/// - `data/{sessions,schemas}` — the v0.2.4 flat-layout state dirs.
/// - `data/fable.sim` — the retired global Narrator card; deleted from the
///   payload + would otherwise linger forever under the preserve rule.
/// - `data/prism.prompt` — the never-wired PRISM prompt stub; same story.
pub const LEGACY_PATHS: &[&str] = &[
    "apps/fable/sessions",
    "apps/fable/schemas",
    "apps/fable/saves",
    "apps/fable/profiles",
    "apps/fable/backgrounds",
    "apps/games",
    "data/sessions",
    "data/schemas",
    "data/fable.sim",
    "data/prism.prompt",
];

/// Delete every [`LEGACY_PATHS`] entry under `target`. Best-effort BY DESIGN:
/// a locked/undeletable legacy path logs a warning and never fails the update
/// (a dead folder surviving one more boot is harmless; a failed apply is
/// not). Returns the number of paths actually removed.
pub fn purge_legacy(target: &Path) -> usize {
    let mut removed = 0usize;
    for rel in LEGACY_PATHS {
        let path = target.join(rel);
        // remove_dir_all handles the folder case; a plain FILE (data/fable.sim,
        // data/prism.prompt) errors with NotADirectory → fall through to
        // remove_file. NotFound (already clean) is the common steady state.
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                crate::log(format!("purged legacy path {}", path.display()));
                removed += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => match std::fs::remove_file(&path) {
                Ok(()) => {
                    crate::log(format!("purged legacy file {}", path.display()));
                    removed += 1;
                }
                Err(e2) => {
                    crate::log(format!(
                        "legacy path {} not removable ({e} / {e2}) — leaving it",
                        path.display()
                    ));
                }
            },
        }
    }
    removed
}

/// Completely remove `data/_update` — the download-staging folder wupi.exe's
/// `perform_update` creates (`portable.zip` / `portable.zip.part` + anything
/// else inside). Returns `true` when the folder is gone (or never existed).
///
/// NOT a [`LEGACY_PATHS`] entry: that list is one-shot best-effort for dead
/// paths, while this is LIVE staging the current run just consumed and must
/// actually remove — so it retries across the AV/indexer lock windows that
/// kill one-shot deletes. Called from `main` on EVERY exit path (success,
/// extract failure, copy failure): the old cleanup lived in `run()`'s success
/// path only, so a failed run returned early and stranded the 2–4 GB zip
/// forever — nothing on the wupi.exe side ever sweeps this folder.
///
/// Fixed to `<target>/data/_update` rather than the zip's parent dir: the old
/// `remove_dir_all(args.zip.parent())` trusted the caller's path shape — this
/// can only ever touch the one canonical staging folder.
pub fn purge_update_staging(target: &Path) -> bool {
    let path = target.join("data").join("_update");
    let attempts = UPDATE_STAGING_BACKOFF_SECS.len() + 1;
    for attempt in 1..=attempts {
        match remove_dir_or_file(&path) {
            Ok(()) => {
                if attempt > 1 {
                    crate::log(format!(
                        "update staging {} removed on attempt {attempt}/{attempts}",
                        path.display()
                    ));
                }
                return true;
            }
            // Absent is the clean steady state — every update from a fixed
            // updater lands here first try.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(e) => crate::log(format!(
                "update staging {} not removable yet (attempt {attempt}/{attempts}: {e})",
                path.display()
            )),
        }
        if let Some(&secs) = UPDATE_STAGING_BACKOFF_SECS.get(attempt - 1) {
            std::thread::sleep(std::time::Duration::from_secs(secs));
        }
    }
    crate::log(format!(
        "update staging {} survived {attempts} removal attempts — leaving it (the next update re-attempts)",
        path.display()
    ));
    false
}

/// Remove `path` whether it is a directory (the normal staging folder) or a
/// stray plain file at that path. `NotFound` propagates unchanged so the
/// caller can treat an absent path as already-clean; any other error from
/// either arm propagates for retry.
fn remove_dir_or_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(e),
        // A plain file errors NotADirectory out of remove_dir_all → the
        // remove_file arm; a locked child propagates for retry.
        Err(_) => std::fs::remove_file(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn purges_dirs_files_and_tolerates_missing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        // A dead folder with content, a dead plain file, and the rest absent.
        std::fs::create_dir_all(target.join("apps/fable/sessions")).unwrap();
        std::fs::write(target.join("apps/fable/sessions/x.json"), b"{}").unwrap();
        std::fs::create_dir_all(target.join("data")).unwrap();
        std::fs::write(target.join("data/fable.sim"), b"<sim_card/>").unwrap();
        // A live path the purge must NOT touch.
        std::fs::create_dir_all(target.join("apps/fable/cards/demo")).unwrap();
        std::fs::write(target.join("apps/fable/cards/demo/demo.sim"), b"live").unwrap();

        let removed = purge_legacy(target);

        assert_eq!(removed, 2);
        assert!(!target.join("apps/fable/sessions").exists());
        assert!(!target.join("data/fable.sim").exists());
        assert!(target.join("apps/fable/cards/demo/demo.sim").exists());
    }

    #[test]
    fn is_idempotent() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("apps/fable/profiles")).unwrap();
        assert_eq!(purge_legacy(tmp.path()), 1);
        // Second run on the now-clean tree: zero removals, no error.
        assert_eq!(purge_legacy(tmp.path()), 0);
    }

    #[test]
    fn update_staging_purge_removes_folder_completely_and_spares_siblings() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path();
        std::fs::create_dir_all(target.join("data/_update")).unwrap();
        std::fs::write(target.join("data/_update/portable.zip"), b"ZIP").unwrap();
        std::fs::write(target.join("data/_update/portable.zip.part"), b"PART").unwrap();
        // A `data/` sibling that must survive: user data under the preserve
        // rule. (The updater's result marker no longer lives here at all —
        // it is `%TEMP%\wupi_update_result.json` since the 2026-08-20
        // relocation; nothing updater-owned may exist in the install.)
        std::fs::write(target.join("data/user.xml"), b"<user/>").unwrap();

        assert!(purge_update_staging(target));
        assert!(!target.join("data/_update").exists());
        assert!(target.join("data/user.xml").exists());
    }

    #[test]
    fn update_staging_purge_is_idempotent_and_handles_stray_file() {
        let tmp = TempDir::new().unwrap();
        // Absent folder = clean (NotFound short-circuits before any sleep).
        assert!(purge_update_staging(tmp.path()));
        // A stray plain FILE at the folder's path is removed too, and a
        // second call on the clean tree stays a no-op.
        std::fs::create_dir_all(tmp.path().join("data")).unwrap();
        std::fs::write(tmp.path().join("data/_update"), b"stray").unwrap();
        assert!(purge_update_staging(tmp.path()));
        assert!(!tmp.path().join("data/_update").exists());
        assert!(purge_update_staging(tmp.path()));
    }
}
