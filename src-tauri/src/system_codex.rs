//! Hidden system-log codex: periodic snapshots of WUPI's runtime state, written
//! to the reserved `WUPI_SYSTEM_CARD_ID = "__wupi_system__"` partition.
//!
//! Wupi retrieves these via the *already-live* `search_wupi_visible` read path
//! (memory.rs:696) — she "knows every nook and cranny" of the running system
//! without any new IPC. The user cannot see or access this partition: every
//! user-facing codex IPC (`codex_list`/`codex_save`/`codex_delete`) is pinned
//! to `CODEX_CARD_ID = "__codex__"` and physically cannot touch `__wupi_system__`.
//!
//! # Privacy guarantee (by construction)
//!
//! - The partition key `__wupi_system__` is a Rust constant, never accepted as
//!   an IPC argument. No frontend IPC can read or write it.
//! - `search_wupi_visible` (the only read path) is called server-side inside
//!   `chat_send`; its results are folded into the model's `<retrieved_memory>`
//!   block, never exposed verbatim to the UI.
//! - The user-facing codex editor lists/saves/deletes via `CODEX_CARD_ID` only.
//!
//! # Reconcile contract (idempotent, hash-gated)
//!
//! Same shape as `codex::seed_codex`: each snapshot is hashed; if the hash
//! matches the existing entry, no write happens (steady-state = zero writes,
//! zero embedding work, zero bloat). Changed hashes trigger delete+re-insert;
//! orphans (entries no longer in the snapshot set) are deleted.
//!
//! # Why not wupi.log?
//!
//! `wupi.log` is plain trace prose — useful for engineering debug, useless for
//! semantic retrieval. The codex needs structured state the embedder can
//! meaningfully vectorize: "you are currently on GLM-5.2 via z.ai", "the active
//! card is rusty_tavern", "the world schema contains X". Those are the
//! snapshots we emit here.

use crate::api::ModelSource;
use crate::memory::{self, MemoryEngine};
use crate::memory_embedder::Embedder;
use std::collections::{HashMap, HashSet};

/// The partition this writer targets. Re-exported for tests; production callers
/// should use `memory::WUPI_SYSTEM_CARD_ID` directly.
pub const SYSTEM_NAMESPACE: &str = "wupi_system";

/// The unified Fable playbook partition. Sibling of [`SYSTEM_NAMESPACE`]:
/// tags `data/fable.codex` entries (the deep playbook shared by the Game
/// Master interview persona AND the simulation narrator — question banks,
/// genre guides, perfect-card examples, bracket-command reference, narrative
/// discipline) so they land in the `memory::FABLE_SYSTEM_CARD_ID` partition,
/// isolated from the OS catgirl's `wupi_system` knowledge. Production callers
/// should use `memory::FABLE_SYSTEM_CARD_ID` directly.
///
/// **Unification (2026-07-29):** was `GM_NAMESPACE = "gm_system"`. Renamed
/// because the GM and Narrator are both Fable-domain personas on one shared
/// knowledge base.
pub const FABLE_NAMESPACE: &str = "fable_system";

/// One snapshot of the runtime state Wupi should be aware of. Plain data so
/// it can be cloned out of AppState under brief locks and processed without
/// holding any mutex through the embedding work.
#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    /// Local-only (Gemma 12B) or API (cloud narrator + local silent agent).
    pub model_source: Option<ModelSource>,
    /// Active API profile (name, model) if any. None under pure Local mode.
    pub active_profile: Option<(String, String)>,
    /// The active persona card id (`__wupi__` for the OS assistant, or a
    /// roleplay card id when a game is running).
    pub active_card_id: String,
    /// Whether a Fable game session is currently active.
    pub fable_active: bool,
    /// The active Fable scenario card's name, if a game is running.
    pub fable_card_name: Option<String>,
    /// How many turns the in-memory Wupi-assistant session has accumulated
    /// this launch (not persisted across restarts).
    pub session_message_count: usize,
    /// Whether the pending schema-delta task is in flight at snapshot time.
    pub schema_delta_in_flight: bool,
    /// Wupi's current understanding of the conversation (the OS-assistant
    /// schema, not the fable schema), as pretty-printed JSON.
    pub world_schema_json: String,
}

impl SystemSnapshot {
    /// Render this snapshot into a set of (title, body) entries for embedding.
    /// Each entry stays under ~1300 chars (the codex chunk budget; bge-small
    /// truncates at 512 tokens, ~1300 chars is the safe ceiling per chunk).
    ///
    /// Splitting by concern (model source, world state, etc.) means retrieval
    /// can surface the relevant slice without pulling the whole snapshot into
    /// the prompt.
    pub fn to_entries(&self) -> Vec<(&'static str, String)> {
        let mut entries = Vec::new();

        // Model + connection state. Wupi knowing "you're on GLM-5.2 via z.ai"
        // or "running fully local on Gemma 12B" helps her answer questions
        // about her own setup accurately.
        let source_str = match self.model_source {
            Some(ModelSource::Api) => "API (cloud primary, local silent agent)",
            Some(ModelSource::Local) => "Local (Gemma 12B, fully offline)",
            None => "unknown",
        };
        let profile_str = self
            .active_profile
            .as_ref()
            .map(|(name, model)| format!(" Active profile: {name} (model: {model})."))
            .unwrap_or_default();
        entries.push((
            "system_model_source",
            format!(
                "Wupi's current inference mode: {source_str}.{profile_str}\n\
                 Active persona card: {}.",
                self.active_card_id
            ),
        ));

        // Fable state.
        if self.fable_active {
            let card = self
                .fable_card_name
                .as_deref()
                .unwrap_or("(unnamed scenario)");
            entries.push((
                "system_fable_state",
                format!(
                    "A Fable roleplay session is currently active. Scenario: {card}.\n\
                     The narrator engine is resident; the player is in-game."
                ),
            ));
        }

        // Conversation + schema state. Only emit if there's meaningful content
        // (skip empty schemas to avoid zero-information entries).
        if self.session_message_count > 0 {
            entries.push((
                "system_conversation",
                format!(
                    "Wupi-assistant has {} messages in the current in-memory session this launch.\n\
                     Schema delta in flight: {}.",
                    self.session_message_count,
                    if self.schema_delta_in_flight { "yes" } else { "no" }
                ),
            ));
        }
        let schema_trimmed = self.world_schema_json.trim();
        if !schema_trimmed.is_empty() && schema_trimmed != "{}" {
            entries.push((
                "system_world_state",
                format!(
                    "Wupi-assistant's current understanding of the conversation (world schema):\n\
                     {schema_trimmed}"
                ),
            ));
        }

        entries
    }
}

/// Hash a snapshot entry's body for the idempotent reconcile gate. Matches
/// `codex.rs`'s use of `DefaultHasher` (the seeded std hasher — fast, good
/// enough for change detection; not cryptographic).
fn hash_body(body: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    body.hash(&mut h);
    h.finish()
}

/// Reconcile a snapshot into the `WUPI_SYSTEM_CARD_ID` partition. Idempotent:
/// entries with unchanged hashes are skipped; changed entries are delete +
/// re-insert; orphan entries (no longer in the snapshot) are deleted.
///
/// Errors are logged-and-returned (the caller decides whether to retry); the
/// memory engine treats each insert as best-effort.
pub async fn seed<E: Embedder>(
    engine: &MemoryEngine<E>,
    snapshot: &SystemSnapshot,
) -> anyhow::Result<()> {
    let new_entries = snapshot.to_entries();

    // Build the desired state: title -> (hash, body).
    let mut desired: HashMap<String, (u64, String)> = HashMap::new();
    for (title, body) in &new_entries {
        let hash = hash_body(body);
        desired.insert((*title).to_string(), (hash, body.clone()));
    }

    // Read existing entries in the system partition.
    let existing = engine
        .list_codex_entries(memory::WUPI_SYSTEM_CARD_ID)
        .await?;

    // Parse existing into title -> (id, hash).
    let mut existing_by_title: HashMap<String, (memory::MemoryId, Option<String>)> = HashMap::new();
    for (id, metadata_json) in existing {
        let title = extract_metadata_field(metadata_json.as_deref(), "title")
            .unwrap_or_default();
        existing_by_title.insert(title, (id, metadata_json));
    }

    // Reconcile: delete orphans, insert/update changed, skip unchanged.
    let mut to_delete: Vec<memory::MemoryId> = Vec::new();
    let mut to_insert: Vec<(String, String, u64)> = Vec::new();

    // Orphans: existing titles not in desired.
    for (title, (id, _)) in &existing_by_title {
        if !desired.contains_key(title) {
            to_delete.push(*id);
        }
    }

    // Inserts/updates: desired titles that are new or changed.
    for (title, (hash, body)) in &desired {
        match existing_by_title.get(title) {
            None => {
                to_insert.push((title.clone(), body.clone(), *hash));
            }
            Some((_, metadata_json)) => {
                let existing_hash = extract_metadata_field(metadata_json.as_deref(), "hash")
                    .and_then(|h| h.parse::<u64>().ok());
                if existing_hash != Some(*hash) {
                    to_insert.push((title.clone(), body.clone(), *hash));
                }
            }
        }
    }

    // If nothing changed, return early — steady state is zero writes.
    if to_delete.is_empty() && to_insert.is_empty() {
        return Ok(());
    }

    // Find ids to delete for entries being updated (insert replaces them).
    let insert_titles: HashSet<&str> = to_insert.iter().map(|(t, _, _)| t.as_str()).collect();
    for (title, (id, _)) in &existing_by_title {
        if insert_titles.contains(title.as_str()) {
            to_delete.push(*id);
        }
    }

    for id in to_delete {
        if let Err(e) = engine.delete_memory(id).await {
            tracing::warn!(error = %format!("{e}"), "system_codex: delete orphan failed");
        }
    }
    for (title, body, hash) in to_insert {
        let metadata = crate::codex::build_metadata_json(&title, &[], hash, SYSTEM_NAMESPACE);
        if let Err(e) = engine
            .add_codex_entry(body, memory::WUPI_SYSTEM_CARD_ID, 1.0, metadata)
            .await
        {
            tracing::warn!(error = %format!("{e}"), "system_codex: insert {} failed", title);
        }
    }

    Ok(())
}

/// Extract a string field's value from a `metadata_json` blob. Mirrors
/// `codex::extract_metadata_field` (kept private there); we need our own copy
/// here to avoid widening the codex module's public surface.
fn extract_metadata_field(metadata_json: Option<&str>, key: &str) -> Option<String> {
    let s = metadata_json?;
    let needle = format!("\"{key}\":");
    let idx = s.find(&needle)? + needle.len();
    let rest = &s[idx..];
    let rest = rest.trim_start();
    if !rest.starts_with('"') {
        // Non-string value (e.g. a number); take up to the next comma/brace.
        let end = rest
            .find(|c: char| c == ',' || c == '}')
            .unwrap_or(rest.len());
        return Some(rest[..end].trim().to_string());
    }
    // String value: find the closing quote (no escape handling — our writer
    // doesn't emit escapes in these fields, and roxmltree-style fields are
    // plain).
    let after_open = &rest[1..];
    let end = after_open.find('"')?;
    Some(after_open[..end].to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_to_entries_includes_model_source() {
        let snap = SystemSnapshot {
            model_source: Some(ModelSource::Api),
            active_profile: Some(("z.ai".into(), "glm-5.2".into())),
            active_card_id: "__wupi__".into(),
            ..Default::default()
        };
        let entries = snap.to_entries();
        let model_entry = entries
            .iter()
            .find(|(t, _)| *t == "system_model_source")
            .expect("model source entry must exist");
        assert!(model_entry.1.contains("API"));
        assert!(model_entry.1.contains("z.ai"));
        assert!(model_entry.1.contains("glm-5.2"));
    }

    #[test]
    fn snapshot_includes_fable_state_when_active() {
        let snap = SystemSnapshot {
            fable_active: true,
            fable_card_name: Some("Rusty Tavern".into()),
            ..Default::default()
        };
        let entries = snap.to_entries();
        assert!(entries.iter().any(|(t, _)| *t == "system_fable_state"));
        let fable = entries
            .iter()
            .find(|(t, _)| *t == "system_fable_state")
            .unwrap();
        assert!(fable.1.contains("Rusty Tavern"));
    }

    #[test]
    fn snapshot_omits_fable_state_when_inactive() {
        let snap = SystemSnapshot {
            fable_active: false,
            ..Default::default()
        };
        let entries = snap.to_entries();
        assert!(!entries.iter().any(|(t, _)| *t == "system_fable_state"));
    }

    #[test]
    fn snapshot_omits_empty_world_schema() {
        let snap = SystemSnapshot {
            world_schema_json: "{}".into(),
            ..Default::default()
        };
        let entries = snap.to_entries();
        assert!(!entries.iter().any(|(t, _)| *t == "system_world_state"));
    }

    #[test]
    fn snapshot_includes_world_schema_when_populated() {
        let snap = SystemSnapshot {
            world_schema_json: r#"{"summary":"chatting","entities":{"mood":"happy"}}"#.into(),
            ..Default::default()
        };
        let entries = snap.to_entries();
        assert!(entries.iter().any(|(t, _)| *t == "system_world_state"));
    }

    #[test]
    fn snapshot_entries_stay_under_chunk_budget() {
        // Each entry body must stay under ~1300 chars (the codex chunk budget;
        // bge-small truncates at 512 tokens ≈ ~1300 chars of prose).
        let snap = SystemSnapshot {
            model_source: Some(ModelSource::Local),
            active_card_id: "__wupi__".into(),
            session_message_count: 42,
            world_schema_json: "{\"summary\":\"x\"}".into(),
            ..Default::default()
        };
        for (_, body) in snap.to_entries() {
            assert!(
                body.len() < 1300,
                "entry body {} chars exceeds 1300-char budget: {body:?}",
                body.len()
            );
        }
    }

    #[test]
    fn hash_body_is_deterministic() {
        let a = hash_body("hello world");
        let b = hash_body("hello world");
        assert_eq!(a, b);
        assert_ne!(a, hash_body("hello world!"));
    }

    #[test]
    fn extract_metadata_field_handles_string_value() {
        let meta = r#"{"kind":"codex","title":"system_model_source","hash":"12345"}"#;
        assert_eq!(
            extract_metadata_field(Some(meta), "title"),
            Some("system_model_source".to_string())
        );
        assert_eq!(
            extract_metadata_field(Some(meta), "hash"),
            Some("12345".to_string())
        );
    }

    #[test]
    fn extract_metadata_field_returns_none_for_missing() {
        assert!(extract_metadata_field(None, "title").is_none());
        assert!(extract_metadata_field(Some("{}"), "title").is_none());
    }
}
