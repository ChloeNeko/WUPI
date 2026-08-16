//! Multi-slot save files for the Fable app.
//!
//! ## Layout
//! Saves live at `<exe_dir>/apps/fable/cards/<card_id>/saves/<save_id>.json`
//! (a sibling INSIDE the per-card folder, §6B). One file per save = atomic
//! write + trivial listing by directory walk (no separate index manifest
//! that can desync). `<save_id>` is either the reserved `autosave` sentinel
//! or a `save_<unix_ms>` stamp for named slots.
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
//! Save payloads grow with campaign length — the per-message world schemas
//! are collapsed into a deduplicated pool on the wire (2026-08-16 audit H3,
//! `session::pool_session_schemas`), so a slot costs roughly one unique
//! schema per turn instead of 2-3 full clones per turn. Save writes are
//! atomic (temp + rename). Listing reads a capped header PREFIX of each
//! file (`read_save_header`) — never the session payload — so the Load
//! list and the title Continue walk stay O(saves × prefix) regardless of
//! campaign size. Loading reads one file. No re-embedding, no schema
//! rebuild, no token cost.

use std::path::{Path, PathBuf};

use crate::schema::WorldSchema;
use crate::session::Conversation;
use crate::sim_card::SimCard;

/// The reserved save_id for the auto-save slot. The UI writes to this on
/// every turn end; it's the "Continue" option on the launcher.
pub const AUTOSAVE_ID: &str = "autosave";

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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SaveFile {
    pub card_id: String,
    pub save_id: String,
    pub name: String,
    pub summary: String,
    pub timestamp: i64,
    pub is_autosave: bool,
    /// (2026-08-16 audit H3) `session.messages.len()`, hoisted so
    /// `list_saves` can read the header prefix WITHOUT parsing the heavy
    /// session payload. `#[serde(default)]` keeps pre-field saves loadable
    /// (the header reader falls back to a full parse for them).
    #[serde(default)]
    pub turn_count: usize,
    pub session: Conversation,
    pub schema: WorldSchema,
}

/// The metadata-only projection `list_saves` extracts from a save's HEADER
/// PREFIX (everything before the `"session"` key). All fields must precede
/// `session` in `SaveFile`'s declaration order for the prefix cut to work —
/// pinned by test.
#[derive(Debug, serde::Deserialize)]
struct SaveHeader {
    name: String,
    summary: String,
    timestamp: i64,
    is_autosave: bool,
    #[serde(default)]
    turn_count: usize,
}

/// Resolve `<cards_root>/<card_id>/saves/` — the per-card saves subdir inside
/// the card's own folder (2026-08-01 layout: each card owns its `.sim`,
/// `.codex`, world/player/npc JSON, AND its save slots as siblings). The
/// subdir isolates each scenario's saves so the Load Game list shows only the
/// relevant ones.
///
/// Takes `fable_root` (= `apps/fable/`) for signature continuity with the many
/// call sites; the cards tree is `fable_root/cards/`.
pub fn resolve_saves_dir(fable_root: &Path, card_id: &str) -> PathBuf {
    fable_root.join("cards").join(card_id).join("saves")
}

/// Bare slug-segment sanitizer (separators / parent refs / drive prefixes
/// → `-`, trimmed, empty → `__unknown__`). Shared by `resolve_save_path`
/// AND the `create_dir_all` in `write_save` — the mkdir must target the
/// SAME directory the write does (2026-08-15 audit fix: the mkdir used the
/// raw id, so an id needing cleaning created a junk directory while the
/// cleaned write path had no parent).
fn clean_save_segment(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "__unknown__".to_string() } else { s }
}

/// Resolve a single save file's path.
pub fn resolve_save_path(fable_root: &Path, card_id: &str, save_id: &str) -> PathBuf {
    // (P2 hardening) Sanitize BOTH ids to bare slug segments: separators,
    // parent refs, or drive prefixes in a caller-supplied id can never
    // escape the saves tree (fable_delete_save is a destructive consumer).
    resolve_saves_dir(fable_root, &clean_save_segment(card_id))
        .join(format!("{}.json", clean_save_segment(save_id)))
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
    // (2026-08-15 audit fix) mkdir from the SANITIZED card id — the write
    // below goes through `resolve_save_path` (which cleans), so an unclean
    // mkdir created a junk directory AND missed the real one (File::create
    // then failed on a missing parent).
    let dir = resolve_saves_dir(fable_root, &clean_save_segment(&card.id));
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
        turn_count: session.messages.len(),
        session: session.clone(),
        schema: schema.clone(),
    };

    let final_path = resolve_save_path(fable_root, &card.id, save_id);
    // (2026-08-15 audit fix) Byte-stable saves: `to_value` routing first —
    // serde_json's Map is BTreeMap-backed (no preserve_order feature), so
    // HashMap-keyed subtrees (schema entities, custom_tags, …) serialize in
    // sorted key order instead of per-process hash order. Identical logical
    // state → identical bytes across boots.
    let mut value = serde_json::to_value(&save)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    // (2026-08-16 audit H3) Same schema-pool collapse as `Conversation::save`
    // — the save slot embeds the whole session, so it inherits the
    // O(messages × schema clones) growth. The header fields (incl.
    // `turn_count`) all precede `session` in the value, so the prefix cut in
    // `read_save_header` still lands before any pooled payload.
    if let Some(session_value) = value.get_mut("session") {
        crate::session::pool_session_schemas(session_value);
    }
    let json = serde_json::to_vec_pretty(&value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Atomic write pattern (matches session.rs / schema.rs): write temp +
    // fsync + rename. The temp lives in the same dir/volume so the rename
    // is atomic on Windows (MOVEFILE_REPLACE_EXISTING).
    //
    // (#59) The temp name is UNIQUE per write (pid + process-local counter):
    // a manual save racing the detached per-turn autosave on the SAME slot
    // used to share one fixed `<slot>.json.tmp` — two open handles on one
    // file interleaved writes and could corrupt the slot. Unique names mean
    // each racing writer stages its own file; the rename keeps last-writer-
    // wins semantics. Stale temps from crashed writes can never collide
    // (different counter/pid) and are invisible to list_saves (.tmp ext).
    let tmp_path = final_path.with_extension(format!("json.{}", unique_tmp_suffix()));
    let write_result: std::io::Result<()> = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
        atomic_rename(&tmp_path, &final_path)
    })();
    if let Err(e) = write_result {
        // Never leave THIS write's temp behind on failure (list_saves
        // ignores .tmp files, but stale temps in the slot dir are grime).
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(save)
}

/// Per-write temp suffix: `<pid>.<counter>.tmp`. Uniqueness is what makes
/// concurrent same-slot writes safe (#59); `Relaxed` is correct (no
/// dependent data rides on the counter). (2026-08-16 audit fix #13) Shared
/// by the save slots AND `session::Conversation::save` +
/// `schema::WorldSchema::save_split` — the fixed `.tmp` names there had the
/// same remove-then-create interleave race this suffix was built to kill.
pub(crate) fn unique_tmp_suffix() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}.{}.tmp", std::process::id(), n)
}


/// List all saves for a card. Sorted: most-recent first (the natural order
/// for a Load Game list). Returns an empty Vec when no saves dir exists
/// (the common case until the user saves the first time).
///
/// (2026-08-16 audit H3) HEADER-ONLY: reads a capped byte prefix of each
/// file (everything before the `"session"` key) instead of parsing the
/// multi-MB session payload. Long campaigns made the old full parse the
/// dominant cost of the Load list AND the title screen's Continue walk
/// (every card × every slot). Falls back to a full parse when the prefix
/// doesn't yield (pre-`turn_count` saves, truncated prefix, odd hand-edits).
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
        // (The pre-#59 fixed-name `<slot>.json.tmp` skip is gone: unique
        // per-write temp suffixes end in `.tmp`, so the extension check
        // above already excludes them — the old file_stem=="*.tmp" guard
        // was dead code that could never match.)
        // (2026-08-16 audit LOW) FILENAME truth: the file stem is the
        // authoritative save_id (every load path resolves id→filename), and
        // the walked card folder is the authoritative card_id — internal
        // values that disagree (hand-edited payload, manual file rename)
        // used to make Continue target a nonexistent path. Internal values
        // remain only as non-UTF8-stem fallbacks.
        let stem_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_owned);
        let (name, summary, timestamp, is_autosave, turn_count) = match read_save_header(&path) {
            Some(h) => h,
            None => {
                // Header prefix failed to yield — full-parse fallback (old
                // saves without turn_count, or a hand-mangled head).
                let Ok(bytes) = std::fs::read(&path) else { continue };
                let Ok(save) = serde_json::from_slice::<SaveFile>(&bytes) else {
                    // A corrupt save shouldn't hide the others. Skip + log.
                    tracing::warn!(path = %path.display(), "skipping unreadable save file");
                    continue;
                };
                let mut session = save.session;
                session.hydrate_schema_refs();
                (save.name, save.summary, save.timestamp, save.is_autosave, session.messages.len())
            }
        };
        out.push(SaveMeta {
            save_id: stem_id.unwrap_or_else(|| card_id.to_owned()),
            card_id: card_id.to_owned(),
            name,
            summary,
            timestamp,
            is_autosave,
            turn_count,
        });
    }
    // Most-recent first.
    out.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(out)
}

/// Read a save's metadata from a capped byte prefix without parsing the
/// session/schema payloads. Cuts the text at the first unescaped
/// `"session"` key (a needle that cannot occur inside a JSON string value:
/// every `"` inside a value is backslash-escaped, breaking the needle at
/// both ends), trims the trailing comma, closes the object, and
/// deserializes the flat header. Returns `None` whenever any step doesn't
/// line up — the caller falls back to a full parse.
fn read_save_header(path: &Path) -> Option<(String, String, i64, bool, usize)> {
    use std::io::Read;
    const PREFIX_CAP: usize = 16 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = file.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        // Stop as soon as the prefix contains the session marker.
        if buf.windows(9).any(|w| w == b"\"session\"") || buf.len() >= PREFIX_CAP {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let cut = text.find("\"session\"")?;
    let mut prefix = text[..cut].trim_end().to_string();
    if prefix.ends_with(',') {
        prefix.pop();
        prefix = prefix.trim_end().to_string();
    }
    prefix.push('}');
    let header: SaveHeader = serde_json::from_str(&prefix).ok()?;
    Some((
        header.name,
        header.summary,
        header.timestamp,
        header.is_autosave,
        header.turn_count,
    ))
}

/// Load a single save file. Returns Err if the file is missing or corrupt.
pub fn load_save(
    fable_root: &Path,
    card_id: &str,
    save_id: &str,
) -> std::io::Result<SaveFile> {
    let path = resolve_save_path(fable_root, card_id, save_id);
    let bytes = std::fs::read(&path)?;
    let mut save: SaveFile = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // (2026-08-16 audit H3) Resolve the wire-only schema pool into the
    // per-message inline fields — the same hydrate `Conversation::load`
    // runs, for the save-slot path.
    save.session.hydrate_schema_refs();
    Ok(save)
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
            subtype: None,
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            intro: String::new(),
            setting: Some("A test place.".into()),
            plot: None,
            tone: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            player_name: Some("Tester".into()),
            locations: Vec::new(),
            cast: Vec::new(),
            start: crate::sim_card::CardStart::default(),
            custom_tags: Default::default(),
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
            entities: std::collections::BTreeMap::new(),
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
            entities: std::collections::BTreeMap::new(),
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

    // ---- (2026-08-16 audit H3) schema pool + header-only listing --------

    fn schema_with(summary: &str) -> WorldSchema {
        let mut s = WorldSchema::default();
        s.summary = summary.to_owned();
        s
    }

    /// A 4-turn campaign-shaped session: each assistant turn's base equals
    /// the PREVIOUS turn's active variant schema (the real production
    /// pattern — turn N+1 acts on the world turn N left behind), so the
    /// pool must collapse the 6 logical schema slots into 4 unique entries.
    fn campaign_session() -> Conversation {
        let mut c = Conversation::new();
        c.add_message(Role::User, "turn 1".into());
        c.add_assistant_turn("beat 1".into(), String::new(), "<r1>".into());
        let s1 = schema_with("after turn 1");
        {
            let m = c.messages.last_mut().unwrap();
            m.base_schema = Some(schema_with("start"));
            m.variant_schemas = vec![s1.clone()];
        }
        c.add_message(Role::User, "turn 2".into());
        c.add_assistant_turn("beat 2".into(), String::new(), "<r2>".into());
        {
            let m = c.messages.last_mut().unwrap();
            // base of turn 2 == turn 1's variant schema (dedup case).
            m.base_schema = Some(s1);
            m.variant_schemas = vec![schema_with("after turn 2"), schema_with("after turn 2 reroll")];
        }
        c
    }

    #[test]
    fn save_slots_pool_schemas_and_roundtrip() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        let session = campaign_session();
        let schema = schema_with("after turn 2"); // the live final state
        write_save(tmp.path(), &card, "save_1", "Pooled", &session, &schema).unwrap();

        let raw = std::fs::read_to_string(
            resolve_save_path(tmp.path(), "test_card", "save_1"),
        )
        .unwrap();
        // Wire shape: pooled refs, no inline schemas on messages.
        assert!(raw.contains("\"schema_pool\""), "pool array present");
        assert!(raw.contains("\"base_schema_ref\""), "base ref present");
        assert!(raw.contains("\"variant_schema_refs\""), "variant refs present");
        assert!(!raw.contains("\"base_schema\":"), "no inline base_schema");
        assert!(!raw.contains("\"variant_schemas\":"), "no inline variant_schemas");
        // 4 unique schemas in the pool (start, s1, after2, after2-reroll);
        // `"summary":` occurrences = 4 pool entries + the save header's own
        // summary + the live top-level schema's summary = 6.
        assert_eq!(raw.matches("\"summary\":").count(), 6);

        // Round-trip: full fidelity through the pool.
        let loaded = load_save(tmp.path(), "test_card", "save_1").unwrap();
        assert_eq!(loaded.session.messages.len(), 4);
        assert!(loaded.session.schema_pool.is_empty(), "hydrated away");
        let a1 = &loaded.session.messages[1];
        assert_eq!(a1.base_schema.as_ref().unwrap().summary, "start");
        assert_eq!(a1.variant_schemas.len(), 1);
        assert_eq!(a1.variant_schemas[0].summary, "after turn 1");
        let a2 = &loaded.session.messages[3];
        assert_eq!(a2.base_schema.as_ref().unwrap().summary, "after turn 1");
        assert_eq!(
            a2.variant_schemas.iter().map(|s| s.summary.as_str()).collect::<Vec<_>>(),
            vec!["after turn 2", "after turn 2 reroll"]
        );
    }

    #[test]
    fn list_saves_reads_header_prefix_only() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        write_save(
            tmp.path(),
            &card,
            "save_big",
            "Big",
            &campaign_session(),
            &schema_with("live"),
        )
        .unwrap();
        let list = list_saves(tmp.path(), "test_card").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].turn_count, 4, "turn_count from the header prefix");
        assert_eq!(list[0].summary, "turn 2");
        assert_eq!(list[0].name, "Big");
    }

    /// A legacy pre-pool save (inline schemas, no turn_count) must still
    /// list + load through the fallback path.
    #[test]
    fn legacy_inline_save_still_lists_and_loads() {
        let tmp = tempdir().unwrap();
        let dir = resolve_saves_dir(tmp.path(), "test_card");
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Conversation::new();
        session.add_message(Role::User, "legacy action".into());
        {
            session.add_assistant_turn("legacy beat".into(), String::new(), "<r>".into());
            let m = session.messages.last_mut().unwrap();
            m.base_schema = Some(schema_with("legacy world"));
            m.variant_schemas = vec![schema_with("legacy world")];
        }
        // Serialize WITHOUT the pool transform — the old wire shape (plus no
        // turn_count, as a pre-field save would have).
        let value = serde_json::to_value(&session).unwrap();
        let mut save_value = serde_json::json!({
            "card_id": "test_card",
            "save_id": "save_legacy",
            "name": "Legacy",
            "summary": "legacy action",
            "timestamp": 123i64,
            "is_autosave": false,
            "session": value,
            "schema": serde_json::to_value(schema_with("legacy world")).unwrap(),
        });
        // Strip turn_count if present (it isn't in this literal — belt+braces).
        if let Some(o) = save_value.as_object_mut() {
            o.remove("turn_count");
        }
        let path = dir.join("save_legacy.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&save_value).unwrap()).unwrap();

        // Listing falls back to a full parse + hydrates for the count.
        let list = list_saves(tmp.path(), "test_card").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].turn_count, 2);
        assert_eq!(list[0].summary, "legacy action");

        // Loading hydrates the inline (pool-less) shape unchanged.
        let loaded = load_save(tmp.path(), "test_card", "save_legacy").unwrap();
        assert_eq!(
            loaded.session.messages[1].base_schema.as_ref().unwrap().summary,
            "legacy world"
        );
    }

}
