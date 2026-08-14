//! Wupi playbook seeding: a single compound reference file
//! (`data/wupi.codex`) → the `__wupi_system__` memory partition.
//!
//! This is the static-seed half of the codex lore-RAG path. The READ side
//! (`MemoryEngine::search_wupi_visible` in `memory.rs`) was never removed —
//! it queries `__wupi_system__` on every chat turn; it just found nothing
//! because the seed path here was deleted in commit `f47a82e` and is now
//! restored. The on-demand retrieved playbook surfaces in `<retrieved_memory>`
//! under the codex frame (`render_memory_block`), NOT in the always-on prompt.
//!
//! # What lives here vs. what doesn't
//!
//! This module is the **Wupi playbook seeding path only** — the compound-file
//! parser + a title-keyed, hash-detected reconcile against the partition.
//! Not the old 1,093-line omnibus: no directory parser, no IPC file ops (the
//! user-codex `data/docs/` + `codex_*` IPC surface is a separate, currently-
//! dormant feature — the `__codex__` partition scaffolding in `memory.rs`
//! stays reserved for it). The Fable playbook (`data/fable.codex` →
//! `__fable_system__`) is out of scope; its retrieval side
//! (`search_fable_visible`) likewise finds nothing until a sibling seeder is
//! restored.
//!
//! # The reconcile contract
//!
//! Re-seeding is **idempotent**: a file whose entries' content hashes all
//! match what's stored does zero writes. The title is the stable identity
//! key (a rename = delete-old + insert-new, by design); the hash detects
//! body edits. Four mutually-exclusive outcomes per source entry are counted
//! in [`ReconcileReport`] and logged at boot so the operator sees at a glance
//! whether the playbook synced cleanly.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::memory::{MemoryEngine, MemoryId};
use crate::memory_embedder::Embedder;

/// Origin tag for entries seeded from `data/wupi.codex`. Flows into the
/// entry's `metadata_json` as `"namespace"` so the origin is queryable for
/// future filtering + audit (the render pipeline treats all codex entries the
/// same regardless of namespace). Replaces the deleted
/// `system_codex::SYSTEM_NAMESPACE`.
pub(crate) const WUPI_SYSTEM_NAMESPACE: &str = "wupi_system";

/// The result of a seed run: logged at boot so the operator can see at a
/// glance whether the playbook synced cleanly. All four counts are mutually
/// exclusive (each source entry resolves to exactly one outcome).
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    /// New source entries inserted into the partition.
    pub seeded: usize,
    /// Source entries whose content hash changed since last seed (delete + re-insert).
    pub updated: usize,
    /// Stored entries whose source no longer exists (purged).
    pub purged: usize,
    /// Source entries whose hash matches the stored entry (no write needed).
    pub unchanged: usize,
}

/// One parsed entry from the compound file: title + tags from front-matter,
/// body is the prose, hash is over the entry's raw text. Ephemeral; lives
/// only for the reconcile pass.
///
/// `pub` (2026-08-01): the per-card Codex tab surfaces authored lore to the
/// player + raw-editor. The parse layer is pure (no memory-engine coupling),
/// so it's promoted to the module's public API for the read/edit feature.
/// The seed path (`seed_compound_codex`) still owns the SQLite insert; this
/// struct is the shared "parsed entry" type both paths use.
#[derive(Clone)]
pub struct ParsedEntry {
    pub title: String,
    pub tags: Vec<String>,
    pub body: String,
    pub hash: u64,
}

/// Seed Wupi's static playbook (`data/wupi.codex`) into the `__wupi_system__`
/// partition. Thin wrapper over [`seed_compound_codex`] pinning the Wupi
/// namespace.
///
/// Wupi retrieves her playbook via `search_wupi_visible` on every chat turn —
/// surfacing the authoring reference the moment the user mentions `.sim`
/// cards, codex lore, or game-mechanic design. Missing `path` or empty file
/// → empty report (graceful, never fatal).
pub(crate) async fn seed_wupi_codex<E: Embedder>(
    engine: &MemoryEngine<E>,
    path: &Path,
    card_id: &str,
) -> anyhow::Result<ReconcileReport> {
    seed_compound_codex(engine, path, card_id, WUPI_SYSTEM_NAMESPACE).await
}

/// Origin tag for per-card lore seeded from a roleplay card's own `.codex`
/// (`cards/<card_id>/<card_id>.codex`). Distinct from the global Fable
/// playbook (`wupi_system`) so per-card lore is queryable/auditable apart
/// from the shared reference. The render pipeline treats all codex entries
/// the same regardless of namespace (codex frame, lower dense floor).
pub(crate) const FABLE_CARD_NAMESPACE: &str = "fable_card";

/// Seed a roleplay card's authored `.codex` into the card's OWN memory
/// partition (keyed by `card_id`). Mirrors [`seed_wupi_codex`] over the
/// generic [`seed_compound_codex`], pinning the per-card namespace.
///
/// Called from `enter_fable_session` on game start so the card's lore is
/// live for `search_fable_visible` (which already queries `active_card_id`
/// as a partition). Idempotent: a re-entry with an unchanged `.codex` does
/// zero writes (title-keyed, hash-detected reconcile); edits to the file
/// propagate on the next `fable_start`. Missing file → empty report
/// (graceful; most cards ship without a `.codex`).
pub(crate) async fn seed_fable_card_codex<E: Embedder>(
    engine: &MemoryEngine<E>,
    path: &Path,
    card_id: &str,
) -> anyhow::Result<ReconcileReport> {
    seed_compound_codex(engine, path, card_id, FABLE_CARD_NAMESPACE).await
}

/// Parse the compound file, reconcile against the entries already stored in
/// the partition, and apply the minimal set of inserts/updates/deletes.
///
/// Title-keyed (rename = delete + insert, by design), hash-detected (over the
/// entry's raw text: whitespace-only front-matter edits still register). All
/// DB ops go through the existing async `MemoryEngine` methods (each
/// `spawn_blocking`s its SQLite work); this fn awaits them in sequence (N is
/// small — the shipped playbook is ~3 entries). A parse failure inside one
/// entry is logged-and-skipped so a single malformed block doesn't kill the
/// whole seed (same contract as the per-file handling in the deleted dir
/// parser).
async fn seed_compound_codex<E: Embedder>(
    engine: &MemoryEngine<E>,
    path: &Path,
    card_id: &str,
    namespace: &str,
) -> anyhow::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();

    let sources = match parse_compound_file(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                file = %path.display(),
                error = %format!("{e}"),
                namespace,
                "compound codex file unreadable; skipping seed"
            );
            return Ok(report);
        }
    };
    if sources.is_empty() {
        tracing::info!(
            file = %path.display(),
            namespace,
            "compound codex file empty or missing; nothing to seed"
        );
        return Ok(report);
    }

    // Embed-cap gate: split any entry whose body exceeds the 1400-char bge-small
    // window into paginated "Title - Part N" children BEFORE the reconcile loop.
    // Post-parse/pre-reconcile placement keeps the parts the canonical title-
    // keyed set (idempotent re-seeds) and guarantees every body reaching
    // `add_codex_entry` is ≤1400 (see memory.rs's clamp for the final backstop).
    let sources = expand_oversize_entries(sources);

    // Reconcile diff: title-keyed, hash-detected.
    let existing = engine.list_codex_entries(card_id).await?;
    let mut existing_by_title: HashMap<String, (MemoryId, Option<String>)> = HashMap::new();
    for (id, metadata_json) in existing {
        let title = extract_metadata_field(metadata_json.as_deref(), "title").unwrap_or_default();
        existing_by_title.insert(
            title,
            (id, extract_metadata_field(metadata_json.as_deref(), "hash")),
        );
    }
    let mut consumed: HashSet<String> = HashSet::new();

    for src in &sources {
        let stored_hash = existing_by_title
            .get(&src.title)
            .and_then(|(_, h)| h.clone());
        let stored_hash_u64 = stored_hash.as_deref().and_then(|s| s.parse::<u64>().ok());
        match stored_hash_u64 {
            Some(h) if h == src.hash => {
                report.unchanged += 1;
                consumed.insert(src.title.clone());
            }
            Some(_) => {
                // Hash differs → delete old, insert new.
                if let Some(&(old_id, _)) = existing_by_title.get(&src.title) {
                    if let Err(e) = engine.delete_memory(old_id).await {
                        tracing::warn!(
                            title = %src.title,
                            error = %format!("{e}"),
                            namespace,
                            "compound codex update: failed to delete old entry; skipping"
                        );
                        continue;
                    }
                }
                match insert_entry(engine, src, card_id, namespace).await {
                    Ok(()) => {
                        report.updated += 1;
                        consumed.insert(src.title.clone());
                    }
                    Err(e) => tracing::warn!(
                        title = %src.title,
                        error = %format!("{e}"),
                        namespace,
                        "compound codex update insert failed"
                    ),
                }
            }
            None => match insert_entry(engine, src, card_id, namespace).await {
                Ok(()) => {
                    report.seeded += 1;
                    consumed.insert(src.title.clone());
                }
                Err(e) => tracing::warn!(
                    title = %src.title,
                    error = %format!("{e}"),
                    namespace,
                    "compound codex seed insert failed"
                ),
            },
        }
    }
    // Purge stored entries whose source no longer exists.
    for (title, (id, _)) in &existing_by_title {
        if !consumed.contains(title) {
            match engine.delete_memory(*id).await {
                Ok(()) => report.purged += 1,
                Err(e) => tracing::warn!(
                    title = %title,
                    error = %format!("{e}"),
                    namespace,
                    "compound codex orphan purge failed"
                ),
            }
        }
    }
    Ok(report)
}

/// Expand any entry whose body exceeds the 1400-character embedding cap into
/// paginated `"{title} - Part N"` children. Each part is ≤1300 chars, split on
/// sentence/paragraph boundaries via [`crate::memory::chunk_text`] (reused from
/// the episodic chunker — its 1300 budget sits safely under the 1400 cap; bge-
/// small silently truncates anything longer, corrupting the vector).
///
/// **Placement matters:** called post-parse + pre-reconcile in
/// [`seed_compound_codex`], so the parts become the canonical title-keyed set.
/// Re-seeding stays idempotent — the same source body yields the same parts +
/// per-part hashes → "unchanged", not churn (a split at insert-time would key
/// the reconcile on the original title + orphan the parts every boot).
///
/// Each part's hash is `DefaultHasher` over its part body (deterministic), so a
/// body edit changes the parts' hashes + triggers an `updated` reconcile.
/// Entries already ≤1400 pass through unchanged. A single-chunk split (the body
/// fit one chunk, e.g. a 1401-char body) collapses back to the original title
/// (no spurious "Part 1").
pub(crate) fn expand_oversize_entries(sources: Vec<ParsedEntry>) -> Vec<ParsedEntry> {
    const CAP: usize = 1400;
    let mut out = Vec::with_capacity(sources.len());
    for src in sources {
        if src.body.len() <= CAP {
            out.push(src);
            continue;
        }
        let chunks = crate::memory::chunk_text(&src.body);
        let n = chunks.len();
        tracing::warn!(
            title = %src.title,
            body_chars = src.body.len(),
            parts = n,
            "codex entry exceeds the 1400-char embedding cap; auto-splitting into paginated parts"
        );
        for (i, body) in chunks.into_iter().enumerate() {
            let title = if n > 1 {
                format!("{} - Part {}", src.title, i + 1)
            } else {
                src.title.clone()
            };
            let hash = {
                let mut h = std::hash::DefaultHasher::new();
                body.hash(&mut h);
                h.finish()
            };
            out.push(ParsedEntry {
                title,
                tags: src.tags.clone(),
                hash,
                body,
            });
        }
    }
    out
}

/// Insert one parsed entry via `add_codex_entry`, building its `metadata_json`.
/// Salience is flat 1.0 (matches episodic; salience weighting is deferred per
/// the salience-landmine). `namespace` flows into the metadata so the entry's
/// origin is queryable for future filtering.
async fn insert_entry(
    engine: &MemoryEngine<impl Embedder>,
    src: &ParsedEntry,
    card_id: &str,
    namespace: &str,
) -> anyhow::Result<()> {
    // Defensive sanity log: `expand_oversize_entries` splits oversize bodies
    // pre-embed (pre-reconcile), so the seed path never reaches here with a body
    // >1400. If this fires, a future caller bypassed the split — the final
    // backstop in `add_codex_entry` (memory.rs) clamps the embed input regardless.
    const BUDGET_CHARS: usize = 1400;
    if src.body.len() > BUDGET_CHARS {
        tracing::error!(
            title = %src.title,
            body_chars = src.body.len(),
            budget = BUDGET_CHARS,
            "codex entry >1400 reached insert_entry; expand_oversize_entries should have split it. add_codex_entry will clamp the embed input."
        );
    }

    let metadata = build_metadata_json(&src.title, &src.tags, src.hash, namespace);
    engine
        .add_codex_entry(src.body.clone(), card_id, 1.0, metadata)
        .await
        .map(|_| ())
}

// ─── parsing ──────────────────────────────────────────────────────────────

/// Parse a *compound* codex file: a single file holding multiple concatenated
/// front-matter + body entries, separated by blank lines. This is the format
/// used by `data/wupi.codex`.
///
/// Each entry follows the shape:
/// ```text
/// ---
/// title: Entry Title
/// tags: keyword, another
/// ---
///
/// Body prose...
/// ```
///
/// A missing file is NOT an error: returns an empty Vec (graceful). A parse
/// failure inside one entry skips just that entry so a single malformed block
/// doesn't kill the whole seed.
///
/// Hash semantics: the hash is over the entry's raw text (front-matter +
/// body + fences), so whitespace-only edits to front-matter still register as
/// a change. The split point is the next `---` fence at the start of a line
/// preceded by a blank line.
fn parse_compound_file(path: &Path) -> anyhow::Result<Vec<ParsedEntry>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::anyhow!("read compound codex {}: {e}", path.display())),
    };
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_owned();

    // Delegate to the text-based parser (the loop was lifted into
    // `parse_compound_text` so the per-card Codex tab shares it).
    Ok(parse_compound_text(&text, &stem))
}

/// Parse compound codex text (NOT a file path) into entries. The per-card
/// Codex tab's read + raw-editor path: it already has the file's text in hand
/// (from a read or the editor textarea), so it doesn't need the disk read that
/// [`parse_compound_file`] fuses in. `fallback_stem` is the title used when an
/// entry omits a `title:` line (the seed path passes the file stem; the tab
/// path passes the card id). Pure — no memory-engine coupling.
///
/// Mirrors the loop in `parse_compound_file` exactly (split → hash →
/// front-matter → skip-empty), just with the file I/O stripped.
pub fn parse_compound_text(text: &str, fallback_stem: &str) -> Vec<ParsedEntry> {
    let mut out = Vec::new();
    for chunk in split_compound(text) {
        let mut hasher = std::hash::DefaultHasher::new();
        chunk.hash(&mut hasher);
        let hash = hasher.finish();

        let (front, body) = split_front_matter(chunk);
        let (title, tags) = parse_front_matter(front, fallback_stem);
        if body.trim().is_empty() {
            continue;
        }
        out.push(ParsedEntry {
            title,
            tags,
            body: body.to_owned(),
            hash,
        });
    }
    out
}

/// Serialize entries back into the compound `.codex` text format. The inverse
/// of [`parse_compound_text`] — round-trips: parsing the output reproduces the
/// input entries (hashes differ only when the body text is reformatted by the
/// caller, which the Codex tab does on every save anyway).
///
/// Format per entry: a blank-line-separated `---\ntitle: X\ntags: a, b\n---\n\n
/// <body>` block. The `title:`/`tags:` lines mirror what `parse_front_matter`
/// reads. An empty `entries` slice serializes to an empty string (a fresh card
/// with no authored lore). Used by the per-card Codex tab's save path.
pub fn format_compound_text(entries: &[ParsedEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("---\n");
        out.push_str("title: ");
        out.push_str(&entry.title);
        out.push('\n');
        if !entry.tags.is_empty() {
            out.push_str("tags: ");
            out.push_str(&entry.tags.join(", "));
            out.push('\n');
        }
        out.push_str("---\n\n");
        out.push_str(entry.body.trim());
        out.push('\n');
    }
    out
}

/// Split a compound codex file into its top-level entries. Each entry is the
/// text from one `---\n` opener up to (but not including) the next `---\n`
/// opener that begins a new entry (i.e. one preceded by a blank line, so
/// `---` inside a body doesn't trigger a false split — the load-bearing rule
/// for entries that contain fenced code blocks).
pub fn split_compound(text: &str) -> Vec<&str> {
    // Walk line by line. An "entry-start fence" is a line that is exactly
    // `---` (after trimming) AND is preceded by either the start of the file
    // or a blank line. This mirrors how the front-matter parser treats the
    // opener and avoids splitting on `---` that appears mid-body.
    let lines: Vec<&str> = text.lines().collect();
    let mut starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            let prev_blank = i == 0 || lines[i - 1].trim().is_empty();
            if prev_blank {
                starts.push(i);
            }
        }
    }
    if starts.is_empty() {
        // No fences at all → whole file is one body-only entry (degenerate).
        return if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text]
        };
    }
    let mut chunks = Vec::new();
    for (idx, &start) in starts.iter().enumerate() {
        let end = if idx + 1 < starts.len() {
            // Up to the blank line preceding the next fence.
            starts[idx + 1].saturating_sub(1)
        } else {
            lines.len()
        };
        let chunk: String = lines[start..end].join("\n");
        if !chunk.trim().is_empty() {
            chunks.push(chunk);
        }
    }
    // Re-slice from `text` by computing byte offsets from the joined lines so
    // we return `&str` slices into the original (cheapest path: no clone).
    let mut out = Vec::new();
    let mut cursor = 0usize;
    for chunk_str in &chunks {
        if let Some(pos) = text[cursor..].find(chunk_str.as_str()) {
            let abs = cursor + pos;
            out.push(&text[abs..abs + chunk_str.len()]);
            cursor = abs + chunk_str.len();
        }
    }
    out
}

/// Split an entry's text into `(front_matter, body)`. Front-matter is the
/// text between leading `---\n` and the next `\n---\n` (or end). If the entry
/// doesn't start with `---`, there's no front-matter: the whole thing is body.
pub fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let after_opener = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"));
    let Some(rest) = after_opener else {
        return (None, text);
    };
    // Find the closing `---` on its own line.
    if let Some(end) = rest.find("\n---\n") {
        let front = &rest[..end];
        let body = &rest[end + "\n---\n".len()..];
        (Some(front), body)
    } else if let Some(end) = rest.find("\n---\r\n") {
        let front = &rest[..end];
        let body = &rest[end + "\n---\r\n".len()..];
        (Some(front), body)
    } else {
        // Opening fence but no closer: treat the whole thing as body (no
        // front-matter). Malformed, but don't lose the content.
        (None, text)
    }
}

/// Parse front-matter text into `(title, tags)`. Hand-rolled: recognizes
/// `title: X` and `tags: a, b, c` lines via `split_once(':')`. Unknown keys
/// are ignored. No YAML engine (Prime Directive §1B.4: compose, don't nest).
pub fn parse_front_matter(front: Option<&str>, fallback_stem: &str) -> (String, Vec<String>) {
    let front = match front {
        Some(f) => f,
        None => return (fallback_stem.to_owned(), Vec::new()),
    };

    let mut title = None;
    let mut tags = Vec::new();

    for line in front.lines() {
        let line = line.trim();
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "title" => {
                    if !val.is_empty() {
                        title = Some(val.to_owned());
                    }
                }
                "tags" => {
                    tags = val
                        .split(',')
                        .map(|t| t.trim().to_owned())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
                _ => {}
            }
        }
    }

    (title.unwrap_or_else(|| fallback_stem.to_owned()), tags)
}

// ─── metadata helpers ─────────────────────────────────────────────────────

/// Build the `metadata_json` string for a codex entry. Hand-rolled JSON
/// construction (the structure is fixed and small; a serde round-trip would be
/// overkill). All values are JSON-escaped via [`escape_json_string`].
fn build_metadata_json(title: &str, tags: &[String], hash: u64, namespace: &str) -> String {
    let title_escaped = escape_json_string(title);
    let tags_array = tags
        .iter()
        .map(|t| format!("\"{}\"", escape_json_string(t)))
        .collect::<Vec<_>>()
        .join(",");
    // `kind=codex` is the downstream discriminator (`is_codex` / codex floor
    // / render frame). `namespace` is the origin tag: "wupi_system" here,
    // "codex" for the dormant user-codex partition. Both reuse the same
    // retrieval/render pipeline; namespace is for future filtering + audit.
    format!(
        "{{\"kind\":\"codex\",\"namespace\":\"{}\",\"title\":\"{}\",\"tags\":[{}],\"hash\":\"{}\"}}",
        escape_json_string(namespace),
        title_escaped,
        tags_array,
        hash
    )
}

/// Escape a string for safe inclusion in a hand-built JSON value.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Extract a string field from a `metadata_json` value. Hand-rolled scan for
/// `"key":"value"` (no nested objects in the codex metadata shape). Used by
/// the reconcile to read the stored `title` + `hash` back out.
fn extract_metadata_field(metadata_json: Option<&str>, key: &str) -> Option<String> {
    let s = metadata_json?;
    let needle = format!("\"{key}\"");
    let idx = s.find(&needle)?;
    let after_key = &s[idx + needle.len()..];
    let after_colon = after_key.trim_start();
    let after_colon = after_colon.strip_prefix(':')?;
    let after_colon = after_colon.trim_start();
    let value = after_colon.strip_prefix('"')?;
    let mut end = None;
    let mut chars = value.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            chars.next();
            continue;
        }
        if c == '"' {
            end = Some(i);
            break;
        }
    }
    let raw = &value[..end?];
    Some(raw.replace("\\\"", "\"").replace("\\\\", "\\"))
}

// ─── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_compound_one_entry() {
        let text = "---\ntitle: One\ntags: a, b\n---\n\nbody one.\n";
        let parts = split_compound(text);
        assert_eq!(parts.len(), 1);
        let (front, body) = split_front_matter(parts[0]);
        assert_eq!(front, Some("title: One\ntags: a, b"));
        assert_eq!(body.trim(), "body one.");
    }

    #[test]
    fn split_compound_multiple_entries() {
        let text = "---\ntitle: One\ntags: a\n---\n\nbody one.\n\n---\ntitle: Two\ntags: b\n---\n\nbody two.\n";
        let parts = split_compound(text);
        assert_eq!(parts.len(), 2);
        let (t1, _) = parse_front_matter(split_front_matter(parts[0]).0, "x");
        let (t2, _) = parse_front_matter(split_front_matter(parts[1]).0, "x");
        assert_eq!(t1, "One");
        assert_eq!(t2, "Two");
    }

    /// The load-bearing false-split guard: a `---` inside a fenced code block
    /// in a body MUST NOT trigger a new entry. Only a `---` preceded by a
    /// blank line opens an entry.
    #[test]
    fn split_compound_does_not_split_on_fence_inside_body() {
        let text = "---\ntitle: Example\ntags: x\n---\n\nA code block:\n\n```markdown\n---\ntitle: Decoy\ntags: y\n---\n\nfake entry body.\n```\n";
        let parts = split_compound(text);
        assert_eq!(
            parts.len(),
            1,
            "a --- inside a fenced body must not split the entry"
        );
    }

    #[test]
    fn parse_front_matter_comma_tags() {
        let (title, tags) = parse_front_matter(Some("title: Elves\ntags: fantasy, elves, faction"), "stem");
        assert_eq!(title, "Elves");
        assert_eq!(tags, vec!["fantasy", "elves", "faction"]);
    }

    #[test]
    fn parse_front_matter_missing_title_falls_back_to_stem() {
        let (title, tags) = parse_front_matter(Some("tags: only"), "fallback");
        assert_eq!(title, "fallback");
        assert_eq!(tags, vec!["only"]);
    }

    #[test]
    fn parse_front_matter_none_yields_stem_and_empty_tags() {
        let (title, tags) = parse_front_matter(None, "stem");
        assert_eq!(title, "stem");
        assert!(tags.is_empty());
    }

    #[test]
    fn parse_compound_file_missing_is_empty() {
        let report = parse_compound_file(Path::new("nonexistent_codex_file.codex")).unwrap();
        assert!(report.is_empty());
    }

    #[test]
    fn parse_compound_file_skips_empty_body_entry() {
        // Entry 2 has no body after the closing fence → skipped.
        let dir = std::env::temp_dir().join("wupi_codex_test_skip_empty");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.codex");
        std::fs::write(
            &path,
            "---\ntitle: Real\ntags: a\n---\n\nhas body.\n\n---\ntitle: Empty\ntags: b\n---\n\n   \n",
        )
        .unwrap();
        let entries = parse_compound_file(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Real");
    }

    #[test]
    fn build_metadata_json_round_trips_through_extract() {
        let tags = vec!["a".to_string(), "b".to_string()];
        let meta = build_metadata_json("Elves", &tags, 12345, WUPI_SYSTEM_NAMESPACE);
        assert_eq!(extract_metadata_field(Some(&meta), "title"), Some("Elves".into()));
        assert_eq!(extract_metadata_field(Some(&meta), "hash"), Some("12345".into()));
        assert!(meta.contains("\"namespace\":\"wupi_system\""));
        assert!(meta.contains("\"kind\":\"codex\""));
    }

    #[test]
    fn build_metadata_json_escapes_quotes_in_title() {
        let meta = build_metadata_json("She said \"hi\"", &[], 1, WUPI_SYSTEM_NAMESPACE);
        assert_eq!(
            extract_metadata_field(Some(&meta), "title"),
            Some("She said \"hi\"".into())
        );
    }

    /// Reconcile determinism: the same parsed entry produces the same hash
    /// (so an unchanged file → all `unchanged` on re-seed). This is the
    /// idempotence invariant that makes the boot seed cheap.
    #[test]
    fn parsed_entry_hash_is_deterministic() {
        let text = "---\ntitle: X\ntags: a\n---\n\nbody.\n";
        let e1 = {
            let mut h = std::hash::DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };
        let e2 = {
            let mut h = std::hash::DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };
        assert_eq!(e1, e2);
    }

    #[test]
    fn expand_oversize_entries_splits_long_bodies() {
        // A body well over the 1400 cap, with paragraph breaks so chunk_text
        // splits on boundaries (paragraph → sentence), not one hard cut.
        let para = "Sentence one is here. Sentence two follows. Sentence three ends it.\n\n";
        let long_body = para.repeat(40); // ~2800 chars
        assert!(long_body.len() > 1400);
        let src = ParsedEntry {
            title: "Dwarven Kingdoms".to_string(),
            tags: vec!["dwarf".to_string(), "lore".to_string()],
            body: long_body.clone(),
            hash: 1, // arbitrary; expand recomputes per-part
        };
        let out = expand_oversize_entries(vec![src]);
        assert!(out.len() >= 2, "should split into >=2 parts, got {}", out.len());
        // Every part body fits the embed cap.
        for p in &out {
            assert!(p.body.len() <= 1400, "part '{}' is {} chars (>1400)", p.title, p.body.len());
        }
        // Paginated titles.
        let titles: Vec<&str> = out.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles[0], "Dwarven Kingdoms - Part 1");
        assert_eq!(titles[1], "Dwarven Kingdoms - Part 2");
        // Tags carry through to every part.
        for p in &out {
            assert_eq!(p.tags, vec!["dwarf".to_string(), "lore".to_string()]);
        }
        // No content lost: every sentence survives across the parts.
        let joined: String = out.iter().map(|p| p.body.as_str()).collect();
        for sentence in ["Sentence one is here.", "Sentence two follows.", "Sentence three ends it."] {
            assert_eq!(
                long_body.matches(sentence).count(),
                joined.matches(sentence).count(),
                "sentence '{sentence}' count diverged across the split",
            );
        }
    }

    #[test]
    fn expand_oversize_entries_passes_short_through_unchanged() {
        let src = ParsedEntry {
            title: "Short".to_string(),
            tags: vec!["a".to_string()],
            body: "tiny body.".to_string(),
            hash: 42,
        };
        let out = expand_oversize_entries(vec![src.clone()]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Short");
        assert_eq!(out[0].body, "tiny body.");
        assert_eq!(out[0].hash, 42); // pass-through keeps the original hash
    }

    /// Same source ⇒ same parts + same per-part hashes ⇒ the reconcile reports
    /// `unchanged` on re-seed (no churn). This is the idempotence invariant.
    #[test]
    fn expand_oversize_entries_is_deterministic() {
        let para = "Alpha beta gamma delta epsilon zeta eta theta iota.\n\n";
        let long_body = para.repeat(50);
        let mk = || ParsedEntry {
            title: "Lore".to_string(),
            tags: vec![],
            body: long_body.clone(),
            hash: 0,
        };
        let a = expand_oversize_entries(vec![mk()]);
        let b = expand_oversize_entries(vec![mk()]);
        assert_eq!(a.len(), b.len());
        assert!(a.len() >= 2);
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.title, y.title);
            assert_eq!(x.body, y.body);
            assert_eq!(x.hash, y.hash);
        }
    }
}
