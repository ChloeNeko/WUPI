//! The §8C preserve rule — which paths the updater must NOT overwrite because
//! they hold user state.
//!
//! **DUPLICATED from `src-tauri/src/updater.rs::is_preserved`** because this
//! crate is standalone (it cannot import `wupi_lib`). The two implementations
//! MUST stay in lockstep: a path preserved here must be preserved there, and
//! vice versa. This is the load-bearing sync contract of the temp-staged
//! pipeline — edit both together.
//!
//! This predicate governs the COPY step only. Explicit deletions from
//! `delete.json` BYPASS it (they are authoritative removals of shipped files
//! that may have lived under a preserved dir like `apps/`).

use std::path::Path;

/// Returns true for paths that must NOT be overwritten by an update (user data).
pub fn is_preserved(rel: &Path) -> bool {
    // data/: preserved EXCEPT the four engine-content files, which ship in the
    // zip and overwrite the local copy verbatim on update.
    if rel.starts_with("data") {
        return rel != Path::new("data/wupi.sim")
            && rel != Path::new("data/wupi.codex")
            && rel != Path::new("data/fable.sim")
            && rel != Path::new("data/fable.codex");
    }
    // memory/, models/, apps/: fully preserved.
    rel.starts_with("memory") || rel.starts_with("models") || rel.starts_with("apps")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_user_data_under_data() {
        assert!(is_preserved(Path::new("data/user.xml")));
        assert!(is_preserved(Path::new("data/theme.json")));
        assert!(is_preserved(Path::new("data/api_config.json")));
        assert!(is_preserved(Path::new("data/docs/lore.md")));
    }

    #[test]
    fn overwrites_engine_content_under_data() {
        assert!(!is_preserved(Path::new("data/wupi.sim")));
        assert!(!is_preserved(Path::new("data/wupi.codex")));
        assert!(!is_preserved(Path::new("data/fable.sim")));
        assert!(!is_preserved(Path::new("data/fable.codex")));
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
        assert!(!is_preserved(Path::new("delete.json")));
    }
}
