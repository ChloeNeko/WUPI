//! The §8C preserve rule — which paths the updater must NOT overwrite because
//! they hold user state.
//!
//! This is the SINGLE implementation (the old duplicate in
//! `src-tauri/src/updater.rs` was deleted when the apply pipeline moved into
//! this crate). The authoritative prose lives in AGENTS.md §8C "Preserve
//! rule" — keep the two in sync when editing.
//!
//! This predicate governs the COPY step only.

use std::path::Path;

/// Returns true for paths that must NOT be overwritten by an update (user data).
///
/// Case-insensitive on purpose (#41 2026-08-15): the Windows filesystem treats
/// `DATA/user.xml` and `data/user.xml` as the SAME file, so a mixed-case zip
/// entry must be judged by its lowercased form or it would slip past the
/// preserve check and overwrite preserved user data.
pub fn is_preserved(rel: &Path) -> bool {
    // Lowercase component-wise (build a new Path from lowercased parts) —
    // `Path::to_str().to_lowercase()` would also work for these ASCII names
    // but component-wise avoids normalizing separators on exotic inputs.
    let lowered: std::path::PathBuf = rel
        .components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().to_lowercase();
            std::ffi::OsString::from(s)
        })
        .collect();
    // data/: preserved EXCEPT the engine-content files, which ship in the
    // zip and overwrite the local copy verbatim on update.
    if lowered.starts_with("data") {
        return lowered != Path::new("data/wupi.sim")
            && lowered != Path::new("data/wupi.codex")
            && lowered != Path::new("data/fable.prompt")
            && lowered != Path::new("data/wupi.prompt");
    }
    // memory/, models/, apps/: fully preserved.
    lowered.starts_with("memory") || lowered.starts_with("models") || lowered.starts_with("apps")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_user_data_under_data() {
        assert!(is_preserved(Path::new("data/user.xml")));
        assert!(is_preserved(Path::new("data/theme.json")));
        assert!(is_preserved(Path::new("data/api_config.json")));
        // The 2026-08-20 rename target — user creds, equally preserved.
        assert!(is_preserved(Path::new("data/api.json")));
        assert!(is_preserved(Path::new("data/docs/lore.md")));
    }

    #[test]
    fn overwrites_engine_content_under_data() {
        assert!(!is_preserved(Path::new("data/wupi.sim")));
        assert!(!is_preserved(Path::new("data/wupi.codex")));
        assert!(!is_preserved(Path::new("data/wupi.prompt")));
        assert!(!is_preserved(Path::new("data/fable.prompt")));
    }

    #[test]
    fn preserves_runtime_dirs() {
        assert!(is_preserved(Path::new("memory/memory.sqlite")));
        assert!(is_preserved(Path::new("models/WUPI.gguf")));
        assert!(is_preserved(Path::new("apps/fable/cards/x/x.sim")));
        assert!(is_preserved(Path::new("apps/fable/players/p/p.json")));
    }

    #[test]
    fn overwrites_engine_files_outside_preserved_dirs() {
        assert!(!is_preserved(Path::new("wupi.exe")));
        assert!(!is_preserved(Path::new("wupi.html")));
        assert!(!is_preserved(Path::new("bin/cublas64_13.dll")));
        assert!(!is_preserved(Path::new("msvcp140.dll")));
        assert!(!is_preserved(Path::new("assets/app.js")));
    }

    /// #41: the Windows FS is case-insensitive — a mixed-case zip entry
    /// (`DATA/user.xml`) resolves onto the SAME file as preserved
    /// `data\user.xml`, so the predicate must judge by lowercased form.
    #[test]
    fn preserve_rule_is_case_insensitive() {
        assert!(is_preserved(Path::new("DATA/user.xml")));
        assert!(is_preserved(Path::new("Data/API_Config.json")));
        assert!(is_preserved(Path::new("MEMORY/memory.sqlite")));
        assert!(is_preserved(Path::new("Models/WUPI.gguf")));
        assert!(is_preserved(Path::new("APPS/fable/cards/x/x.sim")));
        // Engine-content files still overwrite regardless of case.
        assert!(!is_preserved(Path::new("DATA/WUPI.SIM")));
        assert!(!is_preserved(Path::new("Data/Fable.prompt")));
    }
}
