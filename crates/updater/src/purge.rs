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
}
