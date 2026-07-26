//! Dock + desktop layout: which apps live in the bottom quick-menu and which
//! are free-positioned on the "desktop" (over the aurora), persisted to
//! `layout.json` in the app data dir.
//!
//! This is the persistence half of the macOS-style drag-and-drop dock. The
//! frontend owns the apps registry (SVG, label, locked flag) and the
//! interaction layer (hold-to-drag, context menu); this module owns only the
//! persisted shape: the ordered list of app ids in the quick-menu + the
//! free-positioned desktop icons (id + x/y in viewport px).
//!
//! **Storage contract** (mirrors `theme.rs` exactly): `layout.json` in the app
//! data dir, atomic save (temp + rename), graceful default on any error. A
//! corrupt file must never block launch — the worst case is the dock resets
//! to its default arrangement.
//!
//! **Id model:** app ids are bare nouns matching the existing surface keys
//! (`api`, `chat`, `profile`, `fable`). The frontend filters unknown ids at
//! load time so a future build that removes an app doesn't render a ghost
//! tile. The `apps` launcher id is special: it is ALWAYS rendered at the end
//! of the dock and is never persisted here (it's locked / non-removable).

use std::path::PathBuf;

/// One free-positioned desktop icon. `x`/`y` are viewport pixels from the
/// top-left corner (CSS `left`/`top`), clamped client-side on render.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DesktopIcon {
    pub id: String,
    pub x: f64,
    pub y: f64,
}

/// The persisted dock + desktop arrangement. `dock` is the ordered list of
/// app ids shown in the bottom quick-menu (the `apps` launcher is appended
/// automatically by the frontend and is NOT stored here). `desktop` is the
/// set of free-positioned icons; an app may appear in BOTH dock + desktop.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LayoutSettings {
    #[serde(default = "default_dock")]
    pub dock: Vec<String>,
    #[serde(default)]
    pub desktop: Vec<DesktopIcon>,
}

/// Default quick-menu: AI → WUPI Chat → User Editor. Matches the original
/// hand-authored order in `wupi.html` (AI first by design; NOT alphabetical
/// — the home grid is the alphabetical view).
fn default_dock() -> Vec<String> {
    vec!["api".to_owned(), "chat".to_owned(), "profile".to_owned()]
}

impl Default for LayoutSettings {
    fn default() -> Self {
        Self {
            dock: default_dock(),
            desktop: Vec::new(),
        }
    }
}

impl LayoutSettings {
    /// Path to `layout.json` inside the app data dir. Computed once in setup
    /// and cached on AppState (the `layout_path` OnceLock) so the load/save
    /// helpers below stay `&Path`-based and need no `AppHandle`.
    pub fn resolve_path(app_data_dir: &std::path::Path) -> PathBuf {
        app_data_dir.join("layout.json")
    }

    /// Load from disk, falling back to defaults on any error (missing file,
    /// malformed JSON, IO). Persistence is best-effort: a corrupt
    /// layout.json should never block app launch.
    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str::<LayoutSettings>(&s).unwrap_or_default(),
            Err(_) => LayoutSettings::default(),
        }
    }

    /// Persist atomically: temp file + rename (same pattern as theme.rs /
    /// session.rs::save). On failure we log and continue: layout state still
    /// lives in memory and can be re-saved on the next change.
    pub fn save(&self, path: &std::path::Path) {
        let tmp = path.with_extension("json.tmp");
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "layout: serialize failed");
                return;
            }
        };
        if let Err(e) = std::fs::write(&tmp, json) {
            tracing::error!(error = %e, "layout: write tmp failed");
            return;
        }
        // Windows rename over existing uses MOVEFILE_REPLACE_EXISTING: atomic.
        if let Err(e) = std::fs::rename(&tmp, path) {
            tracing::error!(error = %e, "layout: rename failed");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}
