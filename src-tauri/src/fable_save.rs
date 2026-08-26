//! Multi-slot save files + session folders for the Fable app.
//!
//! ## Layout (2026-08-22 session decoupling)
//! Dynamic game data NO LONGER lives inside the card folder — cards under
//! `apps/fable/cards/<CardName>/` are STATIC assets only (`.sim`, portraits,
//! `.lnk`). Every playthrough owns an isolated session folder:
//!
//! `apps/fable/data/saves/<CardName>/<session_id>/`
//!   manifest.json                       session identity (id/name/created)
//!   session.json                        the live conversation for this run
//!   world.json / player.json / npc.json the run's floating schemas
//!   saves/<save_id>.json                autosave + manual atomic snapshots
//!
//! `<session_id>` is `session_<unix_ms>` (slug-sanitized). `<CardName>` is
//! the RESOLVED card folder name (display-named — resolved through the same
//! walker+cache as the card itself, never joined from the slug id raw).
//!
//! ## Save payload — the atomic compound snapshot
//! Each save bundles (1) the roleplay session (full message history), (2)
//! the world-state schema, (3) the schema-history RING BUFFER (the undo
//! snapshots, oldest-first — 2026-08-22 desync fix: a loaded snapshot now
//! restores the exact tracker timeline, rollbacks included), (4) a human
//! label + timestamp, (5) the card_id + session_id it belongs to (a moved/
//! orphaned save is self-identifying). Loading a snapshot overwrites the
//! active session's live schemas with the stored ones so the tracker
//! perfectly matches the story's timeline. The `summary` field is a short
//! UI label derived mechanically (last user message or schema summary, NOT
//! model-generated).
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

/// The session folder's identity file (one per playthrough).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionManifest {
    pub session_id: String,
    pub card_id: String,
    pub name: String,
    pub created_at: i64,
    /// (2026-08-24 Part II D1) The memory partition this playthrough routes
    /// through. ABSENT = the plain `card_id` partition (every existing
    /// session — the field is purely additive). A BRANCHED session carries
    /// `"<card_id>#<session_id>"` — entry installs it into
    /// `AppState.active_memory_partition`, giving the branch full post-fork
    /// episodic isolation while the codex partitions stay card-scoped
    /// (authored lore is shared by design).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_partition: Option<String>,
}

/// A session's list-row projection (the Session Manager UI shape).
/// `last_played` + `save_count` are DERIVED at list time (file mtimes /
/// directory walks) — the manifest is never rewritten during play.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub card_id: String,
    pub name: String,
    pub created_at: i64,
    pub last_played: i64,
    pub save_count: usize,
}

/// One undo-ring snapshot inside a save — the serialized form of a
/// `fable_schema_history` entry. Tag = session message count at push time
/// (see `push_fable_history_snapshot`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistorySnap {
    pub tag: usize,
    pub schema: WorldSchema,
}

/// Metadata for the Load Game list. Returned by `list_saves`. Excludes the
/// heavy session/schema payloads (the UI fetches those via `load_save`
/// only when the user actually picks one).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SaveMeta {
    pub save_id: String,
    pub card_id: String,
    /// (2026-08-22) The session this slot lives in — self-identifying for
    /// the Continue walk (which spans cards × sessions).
    #[serde(default)]
    pub session_id: String,
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
    /// (2026-08-22) The owning session — the folder the slot lives in.
    /// Default-empty for pre-session saves (the migration moves them into a
    /// session folder, so the FILENAME location is authoritative anyway).
    #[serde(default)]
    pub session_id: String,
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
    /// (2026-08-22 desync fix) The schema-history ring buffer at save time,
    /// oldest-first. Empty on pre-field saves (legacy behavior: a loaded
    /// snapshot starts with an empty undo ring).
    #[serde(default)]
    pub history: Vec<HistorySnap>,
}

/// The metadata-only projection `list_saves` extracts from a save's HEADER
/// PREFIX (everything before the `"session"` key). The wire order that makes
/// the prefix cut work is pinned BY CONSTRUCTION in `write_save` (every
/// header field is emitted before `"session"` — serde_json's BTreeMap-backed
/// Map would otherwise sort `summary`/`timestamp` AFTER `session`, and the
/// prefix would never parse) — pinned by test.
#[derive(Debug, serde::Deserialize)]
struct SaveHeader {
    name: String,
    summary: String,
    timestamp: i64,
    is_autosave: bool,
    #[serde(default)]
    turn_count: usize,
}

// ── session folder resolution ──────────────────────────────────────────────

/// Resolve `apps/fable/data/saves/<CardName>/` — the card's session-tree
/// root. The `<CardName>` half reuses the display-name folder resolver (the
/// SAME authority the card itself resolves through — walker + cache), so a
/// slug id can never forge a path the walker didn't enumerate. Fallback
/// mirrors `resolve_card_dir`: a display-stem join (covers un-migrated slug
/// folders).
pub fn resolve_sessions_root(fable_root: &Path, card_id: &str) -> PathBuf {
    let cards_root = fable_root.join("cards");
    let data_saves = fable_root.join("data").join("saves");
    if let Some(folder) = crate::resolve_card_folder(&cards_root, card_id) {
        if let Some(name) = folder.file_name().and_then(|n| n.to_str()) {
            return data_saves.join(name);
        }
    }
    data_saves.join(crate::safe_display_stem(card_id, "Card"))
}

/// Resolve a single session's folder. The `<session_id>` half is
/// slug-sanitized (same guard class as save ids — `fable_session_delete`
/// is a destructive consumer of this path).
pub fn resolve_session_root(fable_root: &Path, card_id: &str, session_id: &str) -> PathBuf {
    resolve_sessions_root(fable_root, card_id).join(clean_save_segment(session_id))
}

/// Resolve `<session>/saves/` — the per-session save slots.
pub fn resolve_saves_dir(fable_root: &Path, card_id: &str, session_id: &str) -> PathBuf {
    resolve_session_root(fable_root, card_id, session_id).join("saves")
}

/// Mint a fresh session id: `session_<unix_ms>` (collision with an existing
/// sibling is practically impossible — ms resolution at creation time).
pub fn mint_session_id() -> String {
    format!("session_{}", current_unix_ms())
}

/// Create a new session folder + manifest for a card. The display name
/// defaults to "Session N" (N = existing session count + 1). Idempotent at
/// the CALLER level (a new id is minted per call — this fn always creates a
/// NEW session).
pub fn create_session(
    fable_root: &Path,
    card: &SimCard,
    name_hint: Option<&str>,
) -> std::io::Result<SessionManifest> {
    let sessions_root = resolve_sessions_root(fable_root, &card.id);
    std::fs::create_dir_all(&sessions_root)?;
    let existing = std::fs::read_dir(&sessions_root)
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);
    let name = name_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(60).collect::<String>())
        .unwrap_or_else(|| format!("Session {}", existing + 1));
    let manifest = SessionManifest {
        session_id: mint_session_id(),
        card_id: card.id.clone(),
        name,
        created_at: current_unix_ms(),
        memory_partition: None,
    };
    let dir = sessions_root.join(&manifest.session_id);
    std::fs::create_dir_all(&dir)?;
    write_manifest(&dir, &manifest)?;
    Ok(manifest)
}

/// (2026-08-24 Part II D1) Read one session's manifest (pub read of the
/// private parser — `enter_fable_session` resolves the branch partition
/// through this, `fable_session_branch` reads the source's identity).
/// `None` for a missing/unparseable manifest (the caller falls back to the
/// plain card partition).
pub fn load_manifest(
    fable_root: &Path,
    card_id: &str,
    session_id: &str,
) -> Option<SessionManifest> {
    read_manifest(&resolve_session_root(fable_root, card_id, session_id))
}

/// (2026-08-24 Part II D1) BRANCH — copy one playthrough into a fresh
/// session folder: session.json + the three split schemas + the whole
/// saves/ tree (the undo rings ride along inside the saves), a fresh
/// manifest carrying `memory_partition = "<card>#<new session>"`. The
/// SOURCE is never touched (copy, not move). FAIL-CLOSED: any error
/// removes the partial destination folder before propagating — the caller
/// pairs this with the memory fork (which it rolls back the same way).
/// The MEMORY copy itself lives in memory.rs (`fork_partition_to`) — this
/// fn owns only the folder.
pub fn branch_session(
    fable_root: &Path,
    card_id: &str,
    source_session_id: &str,
    name_hint: Option<&str>,
) -> std::io::Result<SessionManifest> {
    let source = resolve_session_root(fable_root, card_id, source_session_id);
    if !source.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("source session folder missing: {}", source.display()),
        ));
    }
    let sessions_root = resolve_sessions_root(fable_root, card_id);
    std::fs::create_dir_all(&sessions_root)?;
    let existing = std::fs::read_dir(&sessions_root)
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);
    let name = name_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(60).collect::<String>())
        .unwrap_or_else(|| format!("Session {}", existing + 1));
    let session_id = mint_session_id();
    let manifest = SessionManifest {
        memory_partition: Some(format!("{card_id}#{session_id}")),
        session_id,
        card_id: card_id.to_string(),
        name,
        created_at: current_unix_ms(),
    };
    let dest = sessions_root.join(&manifest.session_id);
    let result = copy_tree(&source, &dest).and_then(|()| write_manifest(&dest, &manifest));
    if result.is_err() {
        // Fail-closed: the partial branch never survives; the source is
        // untouched by construction.
        let _ = std::fs::remove_dir_all(&dest);
    }
    result.map(|()| manifest)
}

/// Recursive folder copy (files + subdirectories; no symlink chase — the
/// sessions tree is plain files). Pure std, no fs_extra dependency.
fn copy_tree(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Write a session's manifest atomically (temp + rename, the save-slot
/// pattern).
pub fn write_manifest(session_dir: &Path, manifest: &SessionManifest) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let final_path = session_dir.join("manifest.json");
    let tmp_path = final_path.with_extension(format!("json.{}", unique_tmp_suffix()));
    let write_result: std::io::Result<()> = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        atomic_rename(&tmp_path, &final_path)
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result
}

/// Read a session folder's manifest. `None` when absent/corrupt (the session
/// still lists — the folder name stands in as the id, the name falls back).
fn read_manifest(session_dir: &Path) -> Option<SessionManifest> {
    let bytes = std::fs::read(session_dir.join("manifest.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// List a card's sessions, most-recently-played first. `last_played` = the
/// newest mtime among the session's save slots + session.json (derived —
/// manifests are never rewritten during play); `save_count` = slots present.
pub fn list_sessions(fable_root: &Path, card_id: &str) -> std::io::Result<Vec<SessionMeta>> {
    let sessions_root = resolve_sessions_root(fable_root, card_id);
    if !sessions_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&sessions_root)?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let session_id = dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned)
            .unwrap_or_default();
        if session_id.is_empty() {
            continue;
        }
        let manifest = read_manifest(&dir);
        let name = manifest
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| session_id.clone());
        let created_at = manifest.as_ref().map(|m| m.created_at).unwrap_or_else(|| {
            entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        });
        // Derived freshness: newest save mtime, else session.json mtime,
        // else the folder's own mtime.
        let mut last_played = created_at;
        let mut save_count = 0usize;
        if let Ok(rd) = std::fs::read_dir(dir.join("saves")) {
            for save_entry in rd.flatten() {
                let p = save_entry.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                save_count += 1;
                if let Ok(modified) = save_entry.metadata().and_then(|m| m.modified()) {
                    if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                        last_played = last_played.max(d.as_millis() as i64);
                    }
                }
            }
        }
        if let Ok(modified) = std::fs::metadata(dir.join("session.json")).and_then(|m| m.modified()) {
            if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                last_played = last_played.max(d.as_millis() as i64);
            }
        }
        out.push(SessionMeta {
            session_id,
            card_id: card_id.to_owned(),
            name,
            created_at,
            last_played,
            save_count,
        });
    }
    out.sort_by(|a, b| b.last_played.cmp(&a.last_played));
    Ok(out)
}

/// Delete a whole session folder (recursive). Idempotent.
pub fn delete_session(fable_root: &Path, card_id: &str, session_id: &str) -> std::io::Result<()> {
    let dir = resolve_session_root(fable_root, card_id, session_id);
    if dir.is_dir() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// Bare slug-segment sanitizer (separators / parent refs / drive prefixes
/// → `-`, trimmed, empty → `__unknown__`). SAVE-SLOT + SESSION ids only
/// (2026-08-19: the card half resolves through the folder resolver, which
/// builds paths exclusively from enumerated directories — the
/// destructive-consumer traversal guard for the card half is structural
/// now).
fn clean_save_segment(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "__unknown__".to_string() } else { s }
}

/// Resolve a single save file's path.
pub fn resolve_save_path(
    fable_root: &Path,
    card_id: &str,
    session_id: &str,
    save_id: &str,
) -> PathBuf {
    // (P2 hardening) The save-id half is slug-sanitized: separators, parent
    // refs, or drive prefixes in a caller-supplied slot id can never escape
    // the saves tree (fable_delete_save is a destructive consumer). The card
    // half resolves through the walker (see `resolve_saves_dir`).
    resolve_saves_dir(fable_root, card_id, session_id)
        .join(format!("{}.json", clean_save_segment(save_id)))
}

/// Write a save atomically (temp + rename) so a crashed write never leaves
/// a half-written slot. Returns the saved file on success.
pub fn write_save(
    fable_root: &Path,
    card: &SimCard,
    session_id: &str,
    save_id: &str,
    name: &str,
    session: &Conversation,
    schema: &WorldSchema,
    history: &[(usize, WorldSchema)],
) -> std::io::Result<SaveFile> {
    // The mkdir targets the SAME directory the write does — both halves go
    // through `resolve_saves_dir` (the folder resolver), so they cannot
    // disagree.
    let dir = resolve_saves_dir(fable_root, &card.id, session_id);
    std::fs::create_dir_all(&dir)?;

    let is_autosave = save_id == AUTOSAVE_ID;
    let timestamp = current_unix_ms();
    let summary = derive_summary(session, schema);
    let save = SaveFile {
        card_id: card.id.clone(),
        session_id: session_id.to_owned(),
        save_id: save_id.to_owned(),
        name: name.to_owned(),
        summary,
        timestamp,
        is_autosave,
        turn_count: session.messages.len(),
        session: session.clone(),
        schema: schema.clone(),
        history: history
            .iter()
            .map(|(tag, schema)| HistorySnap { tag: *tag, schema: schema.clone() })
            .collect(),
    };

    let final_path = resolve_save_path(fable_root, &card.id, session_id, save_id);
    // (2026-08-15 audit fix) Byte-stable saves: `to_value` routing first —
    // serde_json's Map is BTreeMap-backed (no preserve_order feature), so
    // HashMap-keyed subtrees (schema entities, custom_tags, …) serialize in
    // sorted key order instead of per-process hash order. Identical logical
    // state → identical bytes across boots.
    let mut value = serde_json::to_value(&save)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    // (2026-08-16 audit H3) Same schema-pool collapse as `Conversation::save`
    // — the save slot embeds the whole session, so it inherits the
    // O(messages × schema clones) growth.
    if let Some(session_value) = value.get_mut("session") {
        crate::session::pool_session_schemas(session_value);
    }
    // (2026-08-16 bug 1) Compose the document with EVERY header field before
    // `"session"` BY CONSTRUCTION. `to_vec_pretty(&value)` emits BTreeMap-
    // SORTED keys, which put `summary` + `timestamp` AFTER `"session"` — the
    // prefix cut in `read_save_header` then yielded a header missing them on
    // EVERY current-format save (SaveHeader has no defaults for those), so
    // the header path always fell back to the full multi-MB parse + hydrate:
    // strictly MORE work than pre-H3, silently (the list results were still
    // correct via the fallback — which is why the existing tests passed).
    // Hand-assembling the outer object pins the wire order: header fields
    // first, then the (already sorted + pooled) session/schema payloads.
    // (`"session_id":` does NOT match the `"session":` prefix-cut needle —
    // the needle's closing quote breaks at the `_`.) Key order is
    // irrelevant to deserialization — load_save + legacy readers are
    // unaffected. Determinism is preserved (every input to this composition
    // is itself deterministic).
    let session_json = serde_json::to_string_pretty(&value.get("session").cloned().unwrap_or(serde_json::Value::Null))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let schema_json = serde_json::to_string_pretty(&value.get("schema").cloned().unwrap_or(serde_json::Value::Null))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let history_json = serde_json::to_string_pretty(&value.get("history").cloned().unwrap_or(serde_json::Value::Null))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let quoted = |s: &str| serde_json::to_string(s);
    let json = format!(
        "{{\n  \"card_id\": {},\n  \"session_id\": {},\n  \"save_id\": {},\n  \"name\": {},\n  \"summary\": {},\n  \"timestamp\": {},\n  \"is_autosave\": {},\n  \"turn_count\": {},\n  \"session\": {},\n  \"schema\": {},\n  \"history\": {}\n}}",
        quoted(&save.card_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        quoted(&save.session_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        quoted(&save.save_id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        quoted(&save.name)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        quoted(&save.summary)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?,
        save.timestamp,
        save.is_autosave,
        save.turn_count,
        session_json,
        schema_json,
        history_json,
    );

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
        f.write_all(json.as_bytes())?;
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
pub fn list_saves(
    fable_root: &Path,
    card_id: &str,
    session_id: &str,
) -> std::io::Result<Vec<SaveMeta>> {
    let dir = resolve_saves_dir(fable_root, card_id, session_id);
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
            session_id: session_id.to_owned(),
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
/// `"session":` KEY (a needle that cannot occur inside a JSON string
/// value: every `"` inside a value is backslash-escaped, breaking the
/// needle at both ends, and a VALUE that is exactly "session" is followed
/// by `,`/`}` — never `:` — so the colon terminator is what makes the
/// needle key-shaped; the bare `"session"` needle matched a header value
/// named "session" and forced the full-parse fallback, 2026-08-20 audit
/// L3), trims the trailing comma, closes the object, and deserializes the
/// flat header. Returns `None` whenever any step doesn't line up — the
/// caller falls back to a full parse.
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
        // Stop as soon as the prefix contains the session key marker.
        if buf.windows(10).any(|w| w == b"\"session\":") || buf.len() >= PREFIX_CAP {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let cut = text.find("\"session\":")?;
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

/// Public wrapper over the private header-prefix reader — lib.rs's
/// cross-session Continue walk reads individual save paths whose
/// card/session ids come from the DIRECTORY shape, not the payload.
pub fn read_save_header_public(path: &Path) -> Option<(String, String, i64, bool, usize)> {
    read_save_header(path)
}

/// Load a single save file. Returns Err if the file is missing or corrupt.
pub fn load_save(
    fable_root: &Path,
    card_id: &str,
    session_id: &str,
    save_id: &str,
) -> std::io::Result<SaveFile> {
    let path = resolve_save_path(fable_root, card_id, session_id, save_id);
    let bytes = std::fs::read(&path)?;
    let mut save: SaveFile = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    // (2026-08-16 audit H3) Resolve the wire-only schema pool into the
    // per-message inline fields — the same hydrate `Conversation::load`
    // runs, for the save-slot path.
    save.session.hydrate_schema_refs();
    // The FILENAME location is authoritative for session identity (the
    // migration moves pre-session saves without rewriting payloads).
    if save.session_id.is_empty() {
        save.session_id = session_id.to_owned();
    }
    Ok(save)
}

/// Delete a save slot. Idempotent (returns Ok if already gone).
pub fn delete_save(
    fable_root: &Path,
    card_id: &str,
    session_id: &str,
    save_id: &str,
) -> std::io::Result<()> {
    let path = resolve_save_path(fable_root, card_id, session_id, save_id);
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

    /// The session id every test slot lives under (path-shape tests assert
    /// the layout explicitly).
    const SID: &str = "session_1";

    fn fake_card() -> SimCard {
        SimCard {
            id: "test_card".into(),
            name: "Test Scenario".into(),
            card_type: "simulation".into(),
            subtype: None,
            format_v2: false,
            identity: Default::default(),
            persona: Default::default(),
            world: Default::default(),
            location: None,
            inventory: Default::default(),
            properties: Vec::new(),
            linked_codices: Vec::new(),
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            intro: String::new(),
            intro_variants: Vec::new(),
            setting: Some("A test place.".into()),
            plot: None,
            player_name: Some("Tester".into()),
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

        let saved = write_save(
            tmp.path(),
            &card,
            SID,
            "save_1",
            "First Save",
            &session,
            &schema,
            &[],
        )
        .unwrap();
        assert_eq!(saved.card_id, "test_card");
        assert_eq!(saved.session_id, SID);
        assert_eq!(saved.save_id, "save_1");
        assert!(!saved.is_autosave);

        // (2026-08-22) Session-scoped layout: the slot lives under
        // data/saves/<CardFolder>/<session>/saves/, never in the card folder.
        let slot = resolve_save_path(tmp.path(), "test_card", SID, "save_1");
        assert!(
            slot.starts_with(tmp.path().join("data").join("saves")),
            "save slots live under the centralized data tree, got {}",
            slot.display()
        );
        assert!(!tmp.path().join("cards").join("test_card").join("saves").exists());

        let loaded = load_save(tmp.path(), "test_card", SID, "save_1").unwrap();
        assert_eq!(loaded.session.messages.len(), 2);
        assert_eq!(loaded.schema.summary, "Tester opened the door.");
    }

    /// (2026-08-22 desync fix) The undo-ring history rides the compound
    /// snapshot oldest-first and restores verbatim; a save written with an
    /// empty ring loads empty (legacy behavior).
    #[test]
    fn save_history_ring_round_trips() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        let session = session_with("I camp for the night.");
        let mut s1 = WorldSchema::default();
        s1.summary = "before the fight".into();
        let mut s2 = WorldSchema::default();
        s2.summary = "after the fight".into();
        let history = vec![(2usize, s1.clone()), (4usize, s2.clone())];
        write_save(
            tmp.path(),
            &card,
            SID,
            "save_hist",
            "History",
            &session,
            &s2,
            &history,
        )
        .unwrap();
        let loaded = load_save(tmp.path(), "test_card", SID, "save_hist").unwrap();
        assert_eq!(loaded.history.len(), 2);
        assert_eq!(loaded.history[0].tag, 2);
        assert_eq!(loaded.history[0].schema.summary, "before the fight");
        assert_eq!(loaded.history[1].tag, 4);
        assert_eq!(loaded.history[1].schema.summary, "after the fight");

        let empty = load_save(tmp.path(), "test_card", SID, "save_1").ok();
        assert!(empty.is_none(), "sanity: unrelated slot absent");
    }

    /// A legacy save payload (no session_id, no history) written into a
    /// session folder still loads — serde defaults + the filename-location
    /// backfill.
    #[test]
    fn legacy_sessionless_save_loads_with_defaults() {
        let tmp = tempdir().unwrap();
        let dir = resolve_saves_dir(tmp.path(), "test_card", SID);
        std::fs::create_dir_all(&dir).unwrap();
        let save_value = serde_json::json!({
            "card_id": "test_card",
            "save_id": "save_old",
            "name": "Old",
            "summary": "old action",
            "timestamp": 123i64,
            "is_autosave": false,
            "session": serde_json::to_value(session_with("old action")).unwrap(),
            "schema": serde_json::to_value(WorldSchema::default()).unwrap(),
        });
        std::fs::write(
            dir.join("save_old.json"),
            serde_json::to_vec_pretty(&save_value).unwrap(),
        )
        .unwrap();
        let loaded = load_save(tmp.path(), "test_card", SID, "save_old").unwrap();
        assert_eq!(loaded.session_id, SID, "filename location backfills session identity");
        assert!(loaded.history.is_empty(), "no ring on legacy saves");
    }

    #[test]
    fn sessions_create_list_delete_lifecycle() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        assert!(list_sessions(tmp.path(), "test_card").unwrap().is_empty());

        let first = create_session(tmp.path(), &card, None).unwrap();
        assert_eq!(first.name, "Session 1", "auto-naming counts existing sessions");
        let second = create_session(tmp.path(), &card, Some("  Named Run  ")).unwrap();
        assert_eq!(second.name, "Named Run");

        // A save in the second session drives last_played + save_count.
        write_save(
            tmp.path(),
            &card,
            &second.session_id,
            AUTOSAVE_ID,
            "Auto",
            &session_with("playing"),
            &WorldSchema::default(),
            &[],
        )
        .unwrap();

        let list = list_sessions(tmp.path(), "test_card").unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, second.session_id, "most-recently-played first");
        assert_eq!(list[0].name, "Named Run");
        assert_eq!(list[0].save_count, 1);
        assert_eq!(list[1].name, "Session 1");

        delete_session(tmp.path(), "test_card", &second.session_id).unwrap();
        delete_session(tmp.path(), "test_card", &second.session_id).unwrap(); // idempotent
        let after = list_sessions(tmp.path(), "test_card").unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].session_id, first.session_id);
    }

    /// (2026-08-22 resume-attach fix) The SavedPlayer binding lives on the
    /// conversation and must survive the save-slot wire (hand-composed
    /// header + pooled session payload). A save written WITHOUT a binding
    /// (every pre-fix slot on disk) must load back as None, never error.
    #[test]
    fn save_slot_preserves_attached_player_id() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        let mut bound = session_with("I am Alex.");
        bound.attached_player_id = Some("alex".into());
        write_save(
            tmp.path(),
            &card,
            SID,
            "save_bound",
            "Bound",
            &bound,
            &WorldSchema::default(),
            &[],
        )
        .unwrap();
        let loaded = load_save(tmp.path(), "test_card", SID, "save_bound").unwrap();
        assert_eq!(
            loaded.session.attached_player_id.as_deref(),
            Some("alex"),
            "binding must round-trip through the pooled save wire"
        );

        let unbound = session_with("card default player");
        write_save(
            tmp.path(),
            &card,
            SID,
            "save_unbound",
            "Unbound",
            &unbound,
            &WorldSchema::default(),
            &[],
        )
        .unwrap();
        let loaded = load_save(tmp.path(), "test_card", SID, "save_unbound").unwrap();
        assert_eq!(
            loaded.session.attached_player_id, None,
            "pre-fix saves resume unattached, not broken"
        );
    }

    #[test]
    fn list_saves_returns_most_recent_first() {
        let tmp = tempdir().unwrap();
        let card = fake_card();

        // Write two saves with a small gap so timestamps differ.
        write_save(
            tmp.path(),
            &card,
            SID,
            "save_older",
            "Older",
            &session_with("older action"),
            &WorldSchema::default(),
            &[],
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_save(
            tmp.path(),
            &card,
            SID,
            "save_newer",
            "Newer",
            &session_with("newer action"),
            &WorldSchema::default(),
            &[],
        )
        .unwrap();

        let list = list_saves(tmp.path(), "test_card", SID).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].save_id, "save_newer");
        assert_eq!(list[1].save_id, "save_older");
        assert_eq!(list[0].session_id, SID, "meta carries the owning session");
    }

    #[test]
    fn list_saves_empty_when_no_dir() {
        let tmp = tempdir().unwrap();
        let list = list_saves(tmp.path(), "no_such_card", SID).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn autosave_id_flagged() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        write_save(
            tmp.path(),
            &card,
            SID,
            AUTOSAVE_ID,
            "Auto",
            &session_with("auto"),
            &WorldSchema::default(),
            &[],
        )
        .unwrap();
        let list = list_saves(tmp.path(), "test_card", SID).unwrap();
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
            SID,
            "save_x",
            "X",
            &session_with("x"),
            &WorldSchema::default(),
            &[],
        )
        .unwrap();
        delete_save(tmp.path(), "test_card", SID, "save_x").unwrap();
        // Second delete should not error.
        delete_save(tmp.path(), "test_card", SID, "save_x").unwrap();
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
        write_save(tmp.path(), &card, SID, "save_1", "Pooled", &session, &schema, &[]).unwrap();

        let raw = std::fs::read_to_string(
            resolve_save_path(tmp.path(), "test_card", SID, "save_1"),
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
        let loaded = load_save(tmp.path(), "test_card", SID, "save_1").unwrap();
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
            SID,
            "save_big",
            "Big",
            &campaign_session(),
            &schema_with("live"),
            &[],
        )
        .unwrap();
        let list = list_saves(tmp.path(), "test_card", SID).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].turn_count, 4, "turn_count from the header prefix");
        assert_eq!(list[0].summary, "turn 2");
        assert_eq!(list[0].name, "Big");
    }

    /// (2026-08-16 bug 1) The header path must SUCCEED on the current wire
    /// format — `list_saves_reads_header_prefix_only` above passes even when
    /// every read falls back to the full parse (the fallback yields the same
    /// values), so the optimization's "is it actually used" contract needs a
    /// direct pin on `read_save_header`.
    #[test]
    fn read_save_header_parses_current_wire_format_directly() {
        let tmp = tempdir().unwrap();
        let card = fake_card();
        // A schema payload far larger than the 16 KiB prefix read cap — pins
        // that the cut lands BEFORE any session/schema bytes, not just that
        // the fields happen to parse.
        let mut big = WorldSchema::default();
        big.summary = "x".repeat(60_000);
        write_save(
            tmp.path(),
            &card,
            SID,
            "save_hdr",
            "Header",
            &session_with("header action"),
            &big,
            &[],
        )
        .unwrap();
        let path = resolve_save_path(tmp.path(), "test_card", SID, "save_hdr");
        let (name, summary, timestamp, is_autosave, turn_count) =
            read_save_header(&path).expect("header prefix parse must succeed on the current format");
        assert_eq!(name, "Header");
        assert_eq!(summary, "header action");
        assert!(timestamp > 0);
        assert!(!is_autosave);
        assert_eq!(turn_count, 2);

        // Wire-order pin: the `"session"` key must appear before the schema
        // payload in the raw bytes (the composed header-first order), and
        // everything before it must be small (the capped read stops there).
        let raw = std::fs::read_to_string(&path).unwrap();
        let cut = raw.find("\"session\"").expect("session key present");
        assert!(cut < 16 * 1024, "header prefix stays under the read cap");
        assert!(
            raw.find("\"schema\"").map_or(false, |s| s > cut),
            "schema payload follows session"
        );
        // `summary` + `timestamp` must appear BEFORE the cut — the sorted-key
        // order that broke the prefix cut put them after.
        assert!(raw.find("\"summary\"").unwrap() < cut);
        assert!(raw.find("\"timestamp\"").unwrap() < cut);
    }

    /// A legacy pre-pool save (inline schemas, no turn_count) must still
    /// list + load through the fallback path.
    #[test]
    fn legacy_inline_save_still_lists_and_loads() {
        let tmp = tempdir().unwrap();
        let dir = resolve_saves_dir(tmp.path(), "test_card", SID);
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
        let list = list_saves(tmp.path(), "test_card", SID).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].turn_count, 2);
        assert_eq!(list[0].summary, "legacy action");

        // Loading hydrates the inline (pool-less) shape unchanged.
        let loaded = load_save(tmp.path(), "test_card", SID, "save_legacy").unwrap();
        assert_eq!(
            loaded.session.messages[1].base_schema.as_ref().unwrap().summary,
            "legacy world"
        );
    }

}
