//! Multi-slot save files for the Games app.
//!
//! ## Layout
//! Saves live at `<exe_dir>/apps/games/saves/<card_id>/<save_id>.json`. One
//! file per save = atomic write + trivial listing by directory walk (no
//! separate index manifest that can desync). `<save_id>` is either the
//! reserved `autosave` sentinel or a `save_<unix_ms>` stamp for named slots.
//!
//! ## Why per-card subdir
//! A single flat `saves/` dir would interleave saves from different scenarios;
//! the per-card subdir scopes the Load Game list to the active card and
//! lets the UI show only relevant saves. The dir is created lazily on first
//! save (idempotent `create_dir_all`).
//!
//! ## Save payload
//! Each save bundles (1) the roleplay session (full message history), (2)
//! the world-state schema, (3) a human label + timestamp, (4) the card_id
//! it belongs to (so a moved/orphaned save is self-identifying). The
//! `summary` field is a short label for the UI (derived from the most
//! recent user message or schema summary, NOT model-generated).
//!
//! ## Efficiency (Prime Directive §1B)
//! The save payload is small JSON (a few KB per slot). Save writes are
//! atomic (temp + rename). Listing reads only filenames + opens each file
//! once for the metadata header (cheap; O(saves)). Loading reads one file.
//! No re-embedding, no schema rebuild, no token cost.

use std::path::{Path, PathBuf};

use crate::schema::WorldSchema;
use crate::session::Conversation;
use crate::sim_card::SimCard;

/// The reserved save_id for the auto-save slot. The UI writes to this on
/// every turn end; it's the "Continue" option on the launcher.
pub const AUTOSAVE_ID: &str = "autosave";

/// The reserved save_id for the Quick Play slot. Quick Play is single-slot
/// persistence: one quicksave at a time, overwritten by each new Quick Play,
/// bundled with the in-memory card the GM generated during the interview (so
/// nothing needs to live in `apps/fable/cards/`). Excluded from the
/// `AUTOSAVE_ID` filter in `fable_continue_target` so the title's CONTINUE
/// button (and Quick Play's inline Resume) can pick it up.
pub const QUICKSAVE_ID: &str = "quicksave";

/// The fixed card_id Quick Play always runs under. One slot, no name
/// collisions — `fable_quick_start` overrides the GM-generated card's id to
/// this sentinel so the per-card saves/sessions/schemas dirs are stable
/// across Quick Play runs (and `fable_quick_reset` can wipe them by path).
pub const QUICK_PLAY_CARD_ID: &str = "__quickplay__";

/// Metadata for the Load Game list. Returned by `list_saves`. Excludes the
/// heavy session/schema payloads (the UI fetches those via `load_save`
/// only when the user actually picks one).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SaveMeta {
    pub save_id: String,
    pub card_id: String,
    pub name: String,
    pub summary: String,
    pub timestamp: i64,
    pub is_autosave: bool,
    /// Approx turn count (messages in the session). Pure display hint.
    pub turn_count: usize,
}

/// The on-disk save shape. Designed so `load_save` can restore state with a
/// single `serde_json::from_slice`.
///
/// `card` is `Option<SimCard>` with `#[serde(default)]` so older saves
/// (written before Quick Play) load with `card=None`. Quick Play is the only
/// path that sets it: the GM-generated card has no on-disk `.sim` file (per
/// the locked decision to bundle it inside the save), so the quicksave
/// carries it. Manual + autosave slots leave `card=None`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SaveFile {
    pub card_id: String,
    pub save_id: String,
    pub name: String,
    pub summary: String,
    pub timestamp: i64,
    pub is_autosave: bool,
    pub session: Conversation,
    pub schema: WorldSchema,
    /// Quick Play only: the card the GM generated during the interview,
    /// bundled so resume can rebuild the narrator prompt without a `.sim`
    /// file on disk. `None` on manual/autosave slots and on saves written
    /// before this field existed.
    #[serde(default)]
    pub card: Option<SimCard>,
}

/// Resolve `<fable_root>/saves/<card_id>/`. The per-card subdir isolates
/// each scenario's saves so the Load Game list shows only the relevant ones.
pub fn resolve_saves_dir(fable_root: &Path, card_id: &str) -> PathBuf {
    fable_root.join("saves").join(card_id)
}

/// Resolve a single save file's path.
pub fn resolve_save_path(fable_root: &Path, card_id: &str, save_id: &str) -> PathBuf {
    resolve_saves_dir(fable_root, card_id).join(format!("{save_id}.json"))
}

/// Write a save atomically (temp + rename) so a crashed write never leaves
/// a half-written slot. Returns the saved file on success.
pub fn write_save(
    fable_root: &Path,
    card: &SimCard,
    save_id: &str,
    name: &str,
    session: &Conversation,
    schema: &WorldSchema,
) -> std::io::Result<SaveFile> {
    let dir = resolve_saves_dir(fable_root, &card.id);
    std::fs::create_dir_all(&dir)?;

    let is_autosave = save_id == AUTOSAVE_ID;
    let timestamp = current_unix_ms();
    let summary = derive_summary(session, schema);
    let save = SaveFile {
        card_id: card.id.clone(),
        save_id: save_id.to_owned(),
        name: name.to_owned(),
        summary,
        timestamp,
        is_autosave,
        session: session.clone(),
        schema: schema.clone(),
        // Manual + autosave slots never bundle a card — Quick Play is the
        // only path that does, via `write_quick_save`.
        card: None,
    };

    let final_path = resolve_save_path(fable_root, &card.id, save_id);
    let json = serde_json::to_vec_pretty(&save)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Atomic write pattern (matches session.rs / schema.rs): write temp +
    // fsync + rename. The temp lives in the same dir/volume so the rename
    // is atomic on Windows (MOVEFILE_REPLACE_EXISTING).
    let tmp_path = final_path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    atomic_rename(&tmp_path, &final_path)?;
    Ok(save)
}

/// Write the Quick Play quicksave. The single reserved slot under
/// `saves/__quickplay__/quicksave.json` — overwrites any prior quicksave
/// (the locked "one slot, overwrite on new Quick Play" contract). Bundles
/// `card` inside the file so resume can rebuild the narrator prompt without
/// an on-disk `.sim` (Quick Play cards never touch `apps/fable/cards/`).
///
/// `is_autosave = false` so `fable_continue_target` surfaces the quicksave
/// as a valid resume target (it only excludes the `AUTOSAVE_ID` slot).
pub fn write_quick_save(
    fable_root: &Path,
    card: &SimCard,
    session: &Conversation,
    schema: &WorldSchema,
) -> std::io::Result<SaveFile> {
    let dir = resolve_saves_dir(fable_root, QUICK_PLAY_CARD_ID);
    std::fs::create_dir_all(&dir)?;

    let timestamp = current_unix_ms();
    let summary = derive_summary(session, schema);
    let save = SaveFile {
        card_id: QUICK_PLAY_CARD_ID.to_owned(),
        save_id: QUICKSAVE_ID.to_owned(),
        name: "Quick Play".to_owned(),
        summary,
        timestamp,
        is_autosave: false,
        session: session.clone(),
        schema: schema.clone(),
        card: Some(card.clone()),
    };

    let final_path = resolve_save_path(fable_root, QUICK_PLAY_CARD_ID, QUICKSAVE_ID);
    let json = serde_json::to_vec_pretty(&save)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp_path = final_path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    atomic_rename(&tmp_path, &final_path)?;
    Ok(save)
}

/// Load the Quick Play quicksave. Errors if absent or corrupt (the frontend
/// only calls this after `quick_save_exists` returns true, so an absent file
/// is unexpected — surfaces as an error rather than a silent fallback).
pub fn load_quick_save(fable_root: &Path) -> std::io::Result<SaveFile> {
    load_save(fable_root, QUICK_PLAY_CARD_ID, QUICKSAVE_ID)
}

/// Whether a Quick Play quicksave exists on disk. Drives the title-screen
/// inline Resume/Start-New choice (no quicksave → straight into the interview).
pub fn quick_save_exists(fable_root: &Path) -> bool {
    resolve_save_path(fable_root, QUICK_PLAY_CARD_ID, QUICKSAVE_ID).exists()
}

/// Wipe the Quick Play quicksave (idempotent). Called by `fable_quick_reset`
/// before a brand-new interview so the new run starts from a clean slate.
pub fn delete_quick_save(fable_root: &Path) -> std::io::Result<()> {
    delete_save(fable_root, QUICK_PLAY_CARD_ID, QUICKSAVE_ID)
}

/// List all saves for a card. Sorted: most-recent first (the natural order
/// for a Load Game list). Returns an empty Vec when no saves dir exists
/// (the common case until the user saves the first time).
pub fn list_saves(fable_root: &Path, card_id: &str) -> std::io::Result<Vec<SaveMeta>> {
    let dir = resolve_saves_dir(fable_root, card_id);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        // Skip the temp files written mid-atomic-rename.
        if path.file_stem().and_then(|s| s.to_str()) == Some("*.tmp") {
            continue;
        }
        // Read just enough to pull the metadata header. serde_json needs the
        // full file but the payloads are small (a few KB), so we don't
        // bother with a streaming parser.
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(save) = serde_json::from_slice::<SaveFile>(&bytes) else {
            // A corrupt save shouldn't hide the others. Skip + log upstream.
            tracing::warn!(path = %path.display(), "skipping unreadable save file");
            continue;
        };
        out.push(SaveMeta {
            save_id: save.save_id,
            card_id: save.card_id,
            name: save.name,
            summary: save.summary,
            timestamp: save.timestamp,
            is_autosave: save.is_autosave,
            turn_count: save.session.messages.len(),
        });
    }
    // Most-recent first.
    out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(out)
}

/// Load a single save file. Returns Err if the file is missing or corrupt.
pub fn load_save(
    fable_root: &Path,
    card_id: &str,
    save_id: &str,
) -> std::io::Result<SaveFile> {
    let path = resolve_save_path(fable_root, card_id, save_id);
    let bytes = std::fs::read(&path)?;
    serde_json::from_slice::<SaveFile>(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Delete a save slot. Idempotent (returns Ok if already gone).
pub fn delete_save(fable_root: &Path, card_id: &str, save_id: &str) -> std::io::Result<()> {
    let path = resolve_save_path(fable_root, card_id, save_id);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Derive a short UI summary from the most recent user message (or fall
/// back to the schema summary). NOT model-generated: this is the cheap
/// path that needs no tokens. We want a glance-able label like the player's
/// last action so the Load Game list reads "I sneak past the sleeping guard"
/// rather than "save_1721609821."
fn derive_summary(session: &Conversation, schema: &WorldSchema) -> String {
    // Prefer the last user message (the player's last action): most evocative.
    for msg in session.messages.iter().rev() {
        if msg.role == crate::session::Role::User {
            let trimmed = msg.content.trim();
            if !trimmed.is_empty() {
                return ellipsize(trimmed, 100);
            }
        }
    }
    // Fall back to schema summary (the running narrative arc).
    let s = schema.summary.trim();
    if !s.is_empty() {
        return ellipsize(s, 100);
    }
    // Last resort.
    "A new beginning.".to_owned()
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

fn current_unix_ms() -> i64 {
    // SystemTime since UNIX epoch. Matches the timestamp convention used by
    // session::Message.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Cross-platform atomic rename. On Windows this resolves to
/// MOVEFILE_REPLACE_EXISTING semantics via std::fs::rename (which calls
/// MoveFileExW with REPLACE_EXISTING on Windows).
fn atomic_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Role;
    use tempfile::tempdir;

    fn fake_card() -> SimCard {
        SimCard {
            id: "test_card".into(),
            name: "Test Scenario".into(),
            card_type: "roleplay".into(),
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            setting: Some("A test place.".into()),
            tone: None,
            opening_scene: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            protagonist_name: Some("Tester".into()),
        }
    }

    fn session_with(user_text: &str) -> Conversation {
        let mut c = Conversation::new();
        c.add_message(Role::User, user_text.to_owned());
        c.add_message(Role::Assistant, "The world reacts.".into());
        c
    }

    #[test]
    fn write_save_then_load_roundtrip() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        let session = session_with("I open the rusty door.");
        let schema = WorldSchema {
            summary: "Tester opened the door.".into(),
            recent_events: vec!["door_opened".into()],
            entities: std::collections::HashMap::new(),
            ..Default::default()
        };

        let saved =
            write_save(tmp.path(), &card, "save_1", "First Save", &session, &schema).unwrap();
        assert_eq!(saved.card_id, "test_card");
        assert_eq!(saved.save_id, "save_1");
        assert!(!saved.is_autosave);

        let loaded = load_save(tmp.path(), "test_card", "save_1").unwrap();
        assert_eq!(loaded.session.messages.len(), 2);
        assert_eq!(loaded.schema.summary, "Tester opened the door.");
    }

    #[test]
    fn list_saves_returns_most_recent_first() {
        let tmp = tempdir().unwrap();
        let card = fake_card();

        // Write two saves with a small gap so timestamps differ.
        write_save(
            tmp.path(),
            &card,
            "save_older",
            "Older",
            &session_with("older action"),
            &WorldSchema::default(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_save(
            tmp.path(),
            &card,
            "save_newer",
            "Newer",
            &session_with("newer action"),
            &WorldSchema::default(),
        )
        .unwrap();

        let list = list_saves(tmp.path(), "test_card").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].save_id, "save_newer");
        assert_eq!(list[1].save_id, "save_older");
    }

    #[test]
    fn list_saves_empty_when_no_dir() {
        let tmp = tempdir().unwrap();
        let list = list_saves(tmp.path(), "no_such_card").unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn autosave_id_flagged() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        write_save(
            tmp.path(),
            &card,
            AUTOSAVE_ID,
            "Auto",
            &session_with("auto"),
            &WorldSchema::default(),
        )
        .unwrap();
        let list = list_saves(tmp.path(), "test_card").unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].is_autosave);
    }

    #[test]
    fn delete_save_is_idempotent() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        write_save(
            tmp.path(),
            &card,
            "save_x",
            "X",
            &session_with("x"),
            &WorldSchema::default(),
        )
        .unwrap();
        delete_save(tmp.path(), "test_card", "save_x").unwrap();
        // Second delete should not error.
        delete_save(tmp.path(), "test_card", "save_x").unwrap();
    }

    #[test]
    fn derive_summary_prefers_last_user_message() {
        let mut c = Conversation::new();
        c.add_message(Role::User, "first action".into());
        c.add_message(Role::Assistant, "reply 1".into());
        c.add_message(Role::User, "more recent action".into());
        c.add_message(Role::Assistant, "reply 2".into());
        let summary = derive_summary(&c, &WorldSchema::default());
        assert_eq!(summary, "more recent action");
    }

    #[test]
    fn derive_summary_falls_back_to_schema() {
        let c = Conversation::new();
        let schema = WorldSchema {
            summary: "The saga so far.".into(),
            recent_events: Vec::new(),
            entities: std::collections::HashMap::new(),
            ..Default::default()
        };
        let summary = derive_summary(&c, &schema);
        assert_eq!(summary, "The saga so far.");
    }

    #[test]
    fn derive_summary_truncates_long_messages() {
        let long = "x".repeat(200);
        let c = session_with(&long);
        let summary = derive_summary(&c, &WorldSchema::default());
        assert!(summary.chars().count() <= 100);
        assert!(summary.ends_with('…'));
    }

    // ── Quick Play save tests ──────────────────────────────────────────

    #[test]
    fn quick_save_bundles_card() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        let session = session_with("I enter the simulation.");
        let schema = WorldSchema::default();

        let saved = write_quick_save(tmp.path(), &card, &session, &schema).unwrap();
        assert_eq!(saved.card_id, QUICK_PLAY_CARD_ID);
        assert_eq!(saved.save_id, QUICKSAVE_ID);
        assert!(!saved.is_autosave);
        // The bundled card survives the write.
        assert!(saved.card.is_some());
        assert_eq!(saved.card.as_ref().unwrap().id, "test_card");

        // And it round-trips through load.
        assert!(quick_save_exists(tmp.path()));
        let loaded = load_quick_save(tmp.path()).unwrap();
        assert_eq!(loaded.card_id, QUICK_PLAY_CARD_ID);
        assert!(loaded.card.is_some());
        assert_eq!(loaded.card.as_ref().unwrap().name, "Test Scenario");
        assert_eq!(loaded.card.as_ref().unwrap().protagonist_name.as_deref(), Some("Tester"));
        assert_eq!(loaded.session.messages.len(), 2);
    }

    #[test]
    fn quick_save_exists_false_when_absent() {
        let tmp = tempdir().unwrap();
        assert!(!quick_save_exists(tmp.path()));
    }

    #[test]
    fn delete_quick_save_is_idempotent() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        write_quick_save(tmp.path(), &card, &session_with("x"), &WorldSchema::default()).unwrap();
        assert!(quick_save_exists(tmp.path()));
        delete_quick_save(tmp.path()).unwrap();
        assert!(!quick_save_exists(tmp.path()));
        // Second delete must not error.
        delete_quick_save(tmp.path()).unwrap();
    }

    /// Backward compat: a save JSON written BEFORE the `card` field existed
    /// (no `card` key at all) must still deserialize, with `card=None`. This
    /// is the load-bearing `#[serde(default)]` contract on `SaveFile::card`.
    #[test]
    fn save_without_card_field_loads_as_none() {
        let tmp = tempdir().unwrap();
        let dir = resolve_saves_dir(tmp.path(), "legacy_card");
        std::fs::create_dir_all(&dir).unwrap();
        // Hand-write a save JSON with NO `card` key (the pre-Quick Play shape).
        let legacy_json = r#"{
            "card_id": "legacy_card",
            "save_id": "save_legacy",
            "name": "Legacy",
            "summary": "old save",
            "timestamp": 1700000000000,
            "is_autosave": false,
            "session": { "messages": [] },
            "schema": { "summary": "", "recent_events": [], "entities": {} }
        }"#;
        let path = resolve_save_path(tmp.path(), "legacy_card", "save_legacy");
        std::fs::write(&path, legacy_json).unwrap();

        let loaded = load_save(tmp.path(), "legacy_card", "save_legacy").unwrap();
        assert_eq!(loaded.card_id, "legacy_card");
        assert!(loaded.card.is_none(), "legacy save must load with card=None");
    }
}
