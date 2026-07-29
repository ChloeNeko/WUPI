//! Codex: authored reference lore, seeded from disk at startup.
//!
//! A Codex entry is reference knowledge (system documentation, world
//! background) that lives in the SAME `memories` table as episodic turns,
//! distinguished by a `metadata_json` tag: `{"kind":"codex","title":...,
//! "hash":...}`. It is retrieved by the existing Memory v2 pipeline (same
//! embedder, same vec0 index, same RRF fusion) and rendered by
//! `memory::render_memory_block` under a distinct "reference knowledge"
//! epistemic frame (factual background to internalize, NOT archival records to
//! distrust). See AGENTS.md §2P.
//!
//! Source format: plain `.md` files in a `docs/` directory (renamed from
//! `codex/` on 2026-07-17: `resolve_codex_dir` in `lib.rs` walks for `docs`),
//! each with an optional YAML-ish front-matter block (`---\ntitle: X\ntags:
//! a, b\n---`) + a prose body. The seed loader parses each file, computes a
//! content hash, and reconciles the source set against what's already stored -
//! inserting new entries, updating changed ones (delete + re-insert), and
//! purging orphans (source file deleted). This is idempotent: re-running
//! against an unchanged source set produces no writes.
//!
//! Design contract (mirrors `sim_card.rs` + the embedder's graceful-
//! degradation pattern): a missing/empty `docs/` dir or a malformed file is
//! logged-and-skipped, never fatal. The Codex is best-effort; a bad source
//! file must never kill the OS boot.
//!
//! Per-file length budget: each `.md` body must stay under ~350 tokens (~1400
//! chars). `Embed.gguf` (bge-small) truncates silently at 512 tokens, so a
//! long reference doc gets a garbage embedding and scores near the floor even
//! on a perfect match. Split long docs into multiple small files rather than
//! building a chunking engine (Codex v1 deliberately defers chunking: see
//! §2N landmine #6). The loader warns (does not reject) when a body exceeds
//! the heuristic budget.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::memory::{MemoryEngine, MemoryId};
use crate::memory_embedder::Embedder;

/// The result of a seed run: logged at startup so the operator can see at a
/// glance whether the Codex synced cleanly. All four counts are mutually
/// exclusive (each source file resolves to exactly one outcome).
#[derive(Debug, Clone, Default)]
pub struct ReconcileReport {
    /// New source files inserted into the store.
    pub seeded: usize,
    /// Source files whose content hash changed since last seed (delete + re-insert).
    pub updated: usize,
    /// Stored entries whose source file no longer exists (purged).
    pub purged: usize,
    /// Source files whose hash matches the stored entry (no write needed).
    pub unchanged: usize,
}

/// One parsed Codex source file: title + tags from front-matter, body is the
/// prose, hash is over the raw file bytes. Ephemeral; lives only for the
/// reconcile pass.
pub(crate) struct ParsedEntry {
    pub(crate) title: String,
    pub(crate) tags: Vec<String>,
    pub(crate) body: String,
    pub(crate) hash: u64,
}

/// Seed the Codex: parse every `.md` in `codex_dir`, reconcile against the
/// Codex entries already stored in the active card partition, and apply the
/// minimal set of inserts/updates/deletes.
///
/// The reconcile matches on `title` (the stable key: a renamed file is a
/// delete + insert, by design) and detects changes via `hash` (over raw file
/// bytes). All DB ops go through the existing `MemoryEngine` async methods;
/// this fn is async and awaits them in sequence (N is small, ~5-10 files).
///
/// `codex_dir` missing or empty → returns an empty report (graceful, not an
/// error). A parse failure on one file → logs a warning and skips that file;
/// the rest still seed. Only a systemic failure (e.g. the DB list call dies)
/// returns `Err`.
///
/// **Phase 2 firewall:** `namespace` tags the seeded entries so callers can
/// distinguish user-authored codex (`"codex"`) from Wupi's non-editable system
/// knowledge (`"wupi_system"`). Both reuse the `kind=codex` discriminator
/// downstream (so the per-class floor + render frame apply automatically); the
/// `namespace` field is for future filtering and the audit log. Today only the
/// user codex seed path is live (§8C removed the `cards/wupi_knowledge/`
/// system-knowledge seed); a future system-knowledge injection path would
/// write to `WUPI_SYSTEM_CARD_ID` and reuse the `"wupi_system"` namespace.
pub async fn seed_codex(
    engine: &MemoryEngine<impl Embedder>,
    codex_dir: &Path,
    card_id: &str,
    namespace: &str,
) -> anyhow::Result<ReconcileReport> {
    let mut report = ReconcileReport::default();

    // Parse all source files first. A missing dir is not an error: the
    // Codex is optional.
    let sources = match parse_dir(codex_dir) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                dir = %codex_dir.display(),
                error = %format!("{e}"),
                namespace,
                "codex dir unreadable or missing; skipping seed"
            );
            return Ok(report);
        }
    };

    if sources.is_empty() {
        tracing::info!(dir = %codex_dir.display(), namespace, "codex dir empty; nothing to seed");
        return Ok(report);
    }

    // Load the existing Codex entries for this card. Keyed by title for the
    // reconcile diff. Each value carries (id, hash): id for delete, hash for
    // change detection.
    let existing = engine.list_codex_entries(card_id).await?;
    let mut existing_by_title: HashMap<String, (MemoryId, Option<String>)> = HashMap::new();
    for (id, metadata_json) in existing {
        let title = extract_metadata_field(metadata_json.as_deref(), "title")
            .unwrap_or_default();
        existing_by_title.insert(title, (id, extract_metadata_field(metadata_json.as_deref(), "hash")));
    }

    // Track which existing titles we consumed, so leftovers = orphans to purge.
    let mut consumed: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for src in &sources {
        let stored_hash = existing_by_title
            .get(&src.title)
            .and_then(|(_, h)| h.clone());
        let stored_hash_u64 = stored_hash.as_deref().and_then(|s| s.parse::<u64>().ok());

        match stored_hash_u64 {
            Some(h) if h == src.hash => {
                // Unchanged: no write.
                report.unchanged += 1;
                consumed.insert(&src.title);
            }
            Some(_) => {
                // Changed: delete old, insert new (re-embed with new text).
                if let Some(&(old_id, _)) = existing_by_title.get(&src.title) {
                    if let Err(e) = engine.delete_memory(old_id).await {
                        tracing::warn!(
                            title = %src.title,
                            error = %format!("{e}"),
                            "codex update: failed to delete old entry; skipping"
                        );
                        continue;
                    }
                }
                match insert_entry(engine, src, card_id, namespace).await {
                    Ok(()) => {
                        report.updated += 1;
                        consumed.insert(&src.title);
                    }
                    Err(e) => {
                        tracing::warn!(title = %src.title, error = %format!("{e}"), "codex update insert failed");
                    }
                }
            }
            None => {
                // New: insert.
                match insert_entry(engine, src, card_id, namespace).await {
                    Ok(()) => {
                        report.seeded += 1;
                        consumed.insert(&src.title);
                    }
                    Err(e) => {
                        tracing::warn!(title = %src.title, error = %format!("{e}"), "codex seed insert failed");
                    }
                }
            }
        }
    }

    // Purge orphans: stored Codex entries whose title wasn't consumed above
    // (their source file is gone).
    for (title, (id, _)) in &existing_by_title {
        if !consumed.contains(title.as_str()) {
            match engine.delete_memory(*id).await {
                Ok(()) => report.purged += 1,
                Err(e) => tracing::warn!(title = %title, error = %format!("{e}"), "codex orphan purge failed"),
            }
        }
    }

    Ok(report)
}

/// Parse a *compound* codex file: a single `.md`-style file containing
/// multiple concatenated front-matter + body entries, separated by blank
/// lines. This is the format used by `data/wupi.codex` (Wupi's static
/// playbook — engine-shipped reference knowledge seeded into the
/// `__wupi_system__` partition at boot).
///
/// Each entry follows the exact same shape as a single-file `.md` codex
/// entry (front-matter between `---` fences + body below). The compound
/// format exists so engine-shipped playbook content stays in ONE file next
/// to `wupi.sim` (the updater replaces it verbatim) rather than scattering
/// across a directory the user might reasonably assume is theirs to edit.
///
/// A missing file is NOT an error: returns an empty Vec (graceful, mirrors
/// `parse_dir`). A parse failure inside one entry is logged-and-skipped so a
/// single malformed block doesn't kill the whole seed (same contract as
/// `parse_dir`'s per-file handling).
///
/// Hash semantics match `parse_file`: the hash is over the entry's raw bytes
/// (front-matter + body + fences), so whitespace-only edits to front-matter
/// still register as a change. The split point is the next `---` fence at the
/// start of a line preceded by a blank line.
pub(crate) fn parse_compound_file(path: &Path) -> anyhow::Result<Vec<ParsedEntry>> {
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

    let mut out = Vec::new();
    for chunk in split_compound(&text) {
        // Hash each entry's raw text independently (deterministic per-entry
        // identity for the reconcile diff). Hash the &str directly: `Hash` is
        // implemented for `str`, and hashing through UTF-8 bytes matches the
        // spirit of `parse_file`'s raw-bytes hash (a re-encoding to the same
        // bytes produces the same hash).
        let mut hasher = std::hash::DefaultHasher::new();
        chunk.hash(&mut hasher);
        let hash = hasher.finish();

        let (front, body) = split_front_matter(chunk);
        let (title, tags) = parse_front_matter(front, &stem);

        // Skip empty bodies (e.g. an entry that was just front-matter + nothing
        // after — usually a malformed block). Logging is overkill here: the
        // reconcile just won't see it, same as parse_dir skipping an empty file.
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
    Ok(out)
}

/// Split a compound codex file into its top-level entries. Each entry is the
/// text from one `---\n` opener up to (but not including) the next `---\n`
/// opener that begins a new entry (i.e. one preceded by a blank line, so
/// `---` inside a body doesn't trigger a false split). The first entry may or
/// may not begin with `---` (a leading body without front-matter is treated
/// as a single title-less entry — but in practice every shipped entry has
/// front-matter).
fn split_compound(text: &str) -> Vec<&str> {
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
        return if text.trim().is_empty() { Vec::new() } else { vec![text] };
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
    // Borrowing strings out of `chunk` (owned) requires leaking or re-slicing
    // the original. Re-slice from `text` by computing byte offsets from the
    // joined lines. Simpler: re-return `&str` slices into `text` by finding
    // the byte ranges directly.
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

/// Seed an engine-shipped static playbook (compound front-matter file) into a
/// reserved system partition. Generic core used by both [`seed_wupi_codex`]
/// (the OS catgirl's `data/wupi.codex` → `__wupi_system__`) and
/// [`seed_fable_codex`] (the unified Fable playbook `data/fable.codex` →
/// `__fable_system__`).
///
/// Mirrors `seed_codex` but reads from a single compound file (parsed via
/// `parse_compound_file`) instead of a directory, and takes both the partition
/// (`card_id`) and the metadata `namespace` as parameters so the same reconcile
/// logic serves any engine-shipped playbook.
///
/// Same reconcile contract as `seed_codex`: idempotent, hash-gated, deletes
/// orphans (entries in the partition whose source block was removed from the
/// compound file). Missing `path` → empty report (graceful).
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
        tracing::info!(file = %path.display(), namespace, "compound codex file empty or missing; nothing to seed");
        return Ok(report);
    }

    // Same reconcile diff as seed_codex: title-keyed, hash-detected.
    let existing = engine.list_codex_entries(card_id).await?;
    let mut existing_by_title: HashMap<String, (MemoryId, Option<String>)> = HashMap::new();
    for (id, metadata_json) in existing {
        let title = extract_metadata_field(metadata_json.as_deref(), "title").unwrap_or_default();
        existing_by_title.insert(title, (id, extract_metadata_field(metadata_json.as_deref(), "hash")));
    }
    let mut consumed: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for src in &sources {
        let stored_hash = existing_by_title
            .get(&src.title)
            .and_then(|(_, h)| h.clone());
        let stored_hash_u64 = stored_hash.as_deref().and_then(|s| s.parse::<u64>().ok());
        match stored_hash_u64 {
            Some(h) if h == src.hash => {
                report.unchanged += 1;
                consumed.insert(&src.title);
            }
            Some(_) => {
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
                        consumed.insert(&src.title);
                    }
                    Err(e) => tracing::warn!(title = %src.title, error = %format!("{e}"), namespace, "compound codex update insert failed"),
                }
            }
            None => match insert_entry(engine, src, card_id, namespace).await {
                Ok(()) => {
                    report.seeded += 1;
                    consumed.insert(&src.title);
                }
                Err(e) => tracing::warn!(title = %src.title, error = %format!("{e}"), namespace, "compound codex seed insert failed"),
            },
        }
    }
    for (title, (id, _)) in &existing_by_title {
        if !consumed.contains(title.as_str()) {
            match engine.delete_memory(*id).await {
                Ok(()) => report.purged += 1,
                Err(e) => tracing::warn!(title = %title, error = %format!("{e}"), namespace, "compound codex orphan purge failed"),
            }
        }
    }
    Ok(report)
}

/// Seed Wupi's static playbook (`data/wupi.codex`) into the `__wupi_system__`
/// partition. Thin wrapper over [`seed_compound_codex`] pinning the Wupi
/// namespace.
///
/// This fills the previously-empty static-seed side of `WUPI_SYSTEM_CARD_ID`
/// (the runtime-snapshot writer in `system_codex.rs` is the other writer).
/// Wupi retrieves her playbook via `search_wupi_visible` on every chat turn —
/// surface the design contract + file formats + worked examples the moment the
/// user mentions `.sim` cards, codex authoring, or game-mechanic design.
pub(crate) async fn seed_wupi_codex<E: Embedder>(
    engine: &MemoryEngine<E>,
    path: &Path,
    card_id: &str,
) -> anyhow::Result<ReconcileReport> {
    seed_compound_codex(engine, path, card_id, crate::system_codex::SYSTEM_NAMESPACE).await
}

/// Seed the unified Fable playbook (`data/fable.codex`) into the
/// `__fable_system__` partition. Sibling of [`seed_wupi_codex`]: same
/// reconcile contract, isolated partition so Fable-domain knowledge (the
/// deep playbook shared by the Game Master interview persona AND the
/// simulation narrator — question banks, genre guides, perfect-card
/// examples, bracket-command reference, narrative discipline) never leaks
/// into the OS catgirl's prompts and vice versa. Both the GM and the
/// narrator retrieve their playbook via `search_fable_visible` (sibling of
/// `search_wupi_visible`) — one query/turn serves both Fable personas.
pub(crate) async fn seed_fable_codex<E: Embedder>(
    engine: &MemoryEngine<E>,
    path: &Path,
    card_id: &str,
) -> anyhow::Result<ReconcileReport> {
    seed_compound_codex(engine, path, card_id, crate::system_codex::FABLE_NAMESPACE).await
}

/// Insert one parsed entry via `add_codex_entry`, building its `metadata_json`.
/// Salience is flat 1.0 (matches episodic; salience weighting is deferred per
/// §2N landmine #4). `namespace` flows into the metadata so the entry's origin
/// (user codex vs Wupi-system) is queryable for future filtering.
async fn insert_entry(
    engine: &MemoryEngine<impl Embedder>,
    src: &ParsedEntry,
    card_id: &str,
    namespace: &str,
) -> anyhow::Result<()> {
    // Body-length guard: warn (don't reject) when the body exceeds the
    // ~350-token heuristic budget. The entry still seeds: the operator sees
    // the warning and can split the file.
    const BUDGET_CHARS: usize = 1400;
    if src.body.len() > BUDGET_CHARS {
        tracing::warn!(
            title = %src.title,
            body_chars = src.body.len(),
            budget = BUDGET_CHARS,
            "codex entry exceeds the ~350-token budget; bge-small may truncate the embedding. Split into smaller files."
        );
    }

    let metadata = build_metadata_json(&src.title, &src.tags, src.hash, namespace);
    engine
        .add_codex_entry(src.body.clone(), card_id, 1.0, metadata)
        .await
        .map(|_| ())
}

// The Codex UI treats the `.md` files in docs/ as the source of truth: the
// DB is a derived retrieval index, re-seeded at boot. These functions read and
// write the FILES directly, so edits persist across reboots and stay
// git-trackable. After any mutation the caller re-seeds so retrieval stays in
// sync within the running session.

/// One Codex file as the UI sees it. `filename` is the stem (no `.md`, no
/// path): it's the stable identity of the entry across edits. A rename =
/// delete-old + save-new (the caller's job).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodexFile {
    /// Stem of the `.md` file (e.g. `neo-kyoto`). The on-disk key.
    pub filename: String,
    pub title: String,
    pub tags: Vec<String>,
    /// The prose body (everything after the front-matter).
    pub body: String,
}

/// List every Codex `.md` file in `dir`, parsed into `CodexFile` rows. Sorted
/// by title for a stable library view. Empty Vec for a missing/empty dir.
pub fn list_files(dir: &Path) -> anyhow::Result<Vec<CodexFile>> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::anyhow!("read codex dir {}: {e}", dir.display())),
    };
    let mut paths: Vec<std::path::PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()).map_or(false, |s| s.eq_ignore_ascii_case("md")))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        match parse_file(&path) {
            Ok(entry) => {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("untitled").to_owned();
                out.push(CodexFile {
                    filename: stem,
                    title: entry.title,
                    tags: entry.tags,
                    body: entry.body,
                });
            }
            Err(e) => tracing::warn!(file = %path.display(), error = %format!("{e}"), "codex file parse failed; skipping in list"),
        }
    }
    // Sort by title (case-insensitive) for a clean library order.
    out.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    Ok(out)
}

/// Sanitize a filename into a file-system-safe stem: lowercase, replace any
/// non-alphanumeric/`-`/`_` char with `-`, trim leading/trailing `-`. Returns
/// `None` if the result is empty. Public so the IPC layer can echo back the
/// exact stem `save_file` will use (the UI tracks entries by this key).
pub fn sanitize_stem(filename: &str) -> Option<String> {
    let stem: String = filename
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_owned();
    if stem.is_empty() { None } else { Some(stem) }
}

/// Serialize a Codex entry back to its `.md` form and write it atomically.
/// `filename` is the stem; `.md` is appended. The front-matter is regenerated
/// from title + tags; the body is written verbatim below it. Atomic write
/// (temp + rename) mirrors the operator-profile save pattern. Returns the
/// sanitized stem actually written (for the UI to track).
pub fn save_file(dir: &Path, filename: &str, title: &str, tags: &[String], body: &str) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("create codex dir: {e:?}"))?;

    let safe_stem = sanitize_stem(filename)
        .ok_or_else(|| anyhow::anyhow!("codex filename empty after sanitization"))?;

    let md = render_md(title, tags, body);
    let target = dir.join(format!("{safe_stem}.md"));
    let tmp = dir.join(format!(".{safe_stem}.md.tmp"));

    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| anyhow::anyhow!("create codex temp: {e:?}"))?;
        f.write_all(md.as_bytes()).map_err(|e| anyhow::anyhow!("write codex temp: {e:?}"))?;
        f.sync_all().map_err(|e| anyhow::anyhow!("fsync codex temp: {e:?}"))?;
    }
    std::fs::rename(&tmp, &target).map_err(|e| anyhow::anyhow!("rename codex temp → target: {e:?}"))?;
    Ok(safe_stem)
}

/// Delete a Codex `.md` file by stem. Silent no-op if it doesn't exist.
pub fn delete_file(dir: &Path, filename: &str) -> anyhow::Result<()> {
    let path = dir.join(format!("{filename}.md"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("delete codex file {}: {e:?}", path.display())),
    }
}

/// Render a Codex entry to its canonical `.md` form: YAML-ish front-matter
/// (title + tags) then a blank line then the body. Separated from `save_file`
/// so a round-trip test can exercise it without touching disk.
fn render_md(title: &str, tags: &[String], body: &str) -> String {
    let tags_line = tags.join(", ");
    format!(
        "---\ntitle: {title}\ntags: {tags_line}\n---\n\n{body}\n",
        body = body.trim_end(),
    )
}


/// Parse every `.md` file in `dir` (non-recursive). Returns an empty Vec for
/// an empty/missing dir (caller treats as "nothing to seed"). Files are sorted
/// by filename for deterministic seed order.
fn parse_dir(dir: &Path) -> anyhow::Result<Vec<ParsedEntry>> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("read codex dir {}: {e}", dir.display()))?;

    let mut paths: Vec<std::path::PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()).map_or(false, |s| s.eq_ignore_ascii_case("md")))
        .collect();
    paths.sort();

    for path in paths {
        match parse_file(&path) {
            Ok(entry) => entries.push(entry),
            Err(e) => tracing::warn!(file = %path.display(), error = %format!("{e}"), "codex file parse failed; skipping"),
        }
    }
    Ok(entries)
}

/// Parse one `.md` file into a `ParsedEntry`. Reads bytes, computes the hash
/// over the raw bytes (not the parsed fields: so whitespace-only edits to
/// front-matter still register as a change), then splits front-matter from body.
fn parse_file(path: &Path) -> anyhow::Result<ParsedEntry> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    let hash = hasher.finish();

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_owned();

    let (front, body) = split_front_matter(&text);
    let (title, tags) = parse_front_matter(front, &stem);

    Ok(ParsedEntry {
        title,
        tags,
        body: body.to_owned(),
        hash,
    })
}

/// Split a markdown file into `(front_matter, body)`. Front-matter is the
/// text between leading `---\n` and the next `\n---\n` (or end). If the file
/// doesn't start with `---`, there's no front-matter: the whole thing is body.
fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let after_opener = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"));
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
fn parse_front_matter(front: Option<&str>, fallback_stem: &str) -> (String, Vec<String>) {
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

/// Build the `metadata_json` string for a Codex entry. Hand-rolled JSON
/// construction (the structure is fixed and small; a serde round-trip would be
/// overkill). All values are JSON-escaped via `escape_json_string`.
pub(crate) fn build_metadata_json(title: &str, tags: &[String], hash: u64, namespace: &str) -> String {
    let title_escaped = escape_json_string(title);
    let tags_array = tags
        .iter()
        .map(|t| format!("\"{}\"", escape_json_string(t)))
        .collect::<Vec<_>>()
        .join(",");
    // `kind=codex` is the downstream discriminator (is_codex / codex floor /
    // render frame). `namespace` is the origin tag: "codex" for user-authored
    // lore, "wupi_system" for Wupi's non-editable system docs. Both reuse the
    // same retrieval/render pipeline; namespace is for future filtering + audit.
    format!(
        "{{\"kind\":\"codex\",\"namespace\":\"{}\",\"title\":\"{}\",\"tags\":[{}],\"hash\":\"{}\"}}",
        escape_json_string(namespace),
        title_escaped,
        tags_array,
        hash
    )
}

/// Escape a string for safe inclusion inside a JSON string value. Handles the
/// six mandatory JSON escapes. The title/tags are author-controlled and may
/// contain quotes or backslashes.
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

/// Extract a string field's value from a `metadata_json` blob. Shared with
/// `memory::codex_title` in spirit but lives here too (the seed pass needs
/// `title` AND `hash`). Substring probe: finds `"key":"..."` and returns the
/// unescaped value. Returns `None` if the key is absent.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_parses_title_and_tags() {
        let md = "---\ntitle: Card Format\ntags: cards, xml, format\n---\nThe body text.";
        let (front, body) = split_front_matter(md);
        let (title, tags) = parse_front_matter(front, "fallback");
        assert_eq!(title, "Card Format");
        assert_eq!(tags, vec!["cards", "xml", "format"]);
        assert_eq!(body, "The body text.");
    }

    #[test]
    fn front_matter_missing_falls_back_to_stem() {
        let md = "Just body, no front-matter.";
        let (front, body) = split_front_matter(md);
        assert!(front.is_none());
        assert_eq!(body, "Just body, no front-matter.");
        let (title, tags) = parse_front_matter(front, "my-file");
        assert_eq!(title, "my-file");
        assert!(tags.is_empty());
    }

    #[test]
    fn front_matter_with_only_title() {
        let md = "---\ntitle: Solo Title\n---\nBody.";
        let (front, body) = split_front_matter(md);
        let (title, tags) = parse_front_matter(front, "x");
        assert_eq!(title, "Solo Title");
        assert!(tags.is_empty());
        assert_eq!(body, "Body.");
    }

    #[test]
    fn front_matter_unclosed_fence_treats_all_as_body() {
        // Opening `---` but no closing fence: don't lose the content.
        let md = "---\ntitle: Broken\nNo closing fence.";
        let (front, body) = split_front_matter(md);
        assert!(front.is_none());
        assert!(body.contains("No closing fence."));
    }

    #[test]
    fn build_metadata_json_round_trips_through_extract() {
        let tags = vec!["a".to_owned(), "b".to_owned()];
        let json = build_metadata_json("My Title", &tags, 12345, "codex");
        assert_eq!(extract_metadata_field(Some(&json), "title"), Some("My Title".to_owned()));
        assert_eq!(extract_metadata_field(Some(&json), "hash"), Some("12345".to_owned()));
        assert!(json.contains("\"kind\":\"codex\""));
        assert!(json.contains("\"namespace\":\"codex\""));
        assert!(json.contains("\"tags\":[\"a\",\"b\"]"));
    }

    #[test]
    fn build_metadata_json_escapes_quotes_in_title() {
        let json = build_metadata_json("He said \"hi\"", &[], 1, "codex");
        assert!(json.contains("\"title\":\"He said \\\"hi\\\"\""));
        assert_eq!(extract_metadata_field(Some(&json), "title"), Some("He said \"hi\"".to_owned()));
    }

    #[test]
    fn build_metadata_json_tags_wupi_system_namespace() {
        // The firewall's distinguishing field: Wupi-system docs carry the same
        // kind=codex (so the floor + render frame apply) but a different
        // namespace (for future filtering / audit).
        let json = build_metadata_json("Critical Wall", &[], 42, "wupi_system");
        assert!(json.contains("\"kind\":\"codex\""));
        assert!(json.contains("\"namespace\":\"wupi_system\""));
        assert_eq!(extract_metadata_field(Some(&json), "namespace"), Some("wupi_system".to_owned()));
    }

    #[test]
    fn hash_is_deterministic_for_identical_bytes() {
        // parse_file hashes raw bytes, so identical files → identical hash.
        // Verified by hashing the same content twice via the hasher directly.
        let bytes = b"hello codex";
        let h1 = {
            let mut h = std::hash::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        };
        let h2 = {
            let mut h = std::hash::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        };
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_differs_when_content_changes() {
        let h1 = {
            let mut h = std::hash::DefaultHasher::new();
            b"version one".hash(&mut h);
            h.finish()
        };
        let h2 = {
            let mut h = std::hash::DefaultHasher::new();
            b"version two".hash(&mut h);
            h.finish()
        };
        assert_ne!(h1, h2);
    }

    #[test]
    fn extract_field_handles_missing_key() {
        let json = r#"{"kind":"codex","title":"x"}"#;
        assert_eq!(extract_metadata_field(Some(json), "hash"), None);
        assert_eq!(extract_metadata_field(None, "title"), None);
    }

    // ── parse_compound_file (Wupi's playbook compound-file parser) ───────────
    //
    // The playbook lives at data/wupi.codex next to wupi.sim and contains
    // multiple front-matter + body entries concatenated. These tests pin the
    // parser's contract: it splits on top-level fences (NOT fences inside a
    // body), hashes each entry independently for the reconcile diff, and
    // degrades gracefully on missing/malformed input.

    fn write_tmp_compound(content: &str) -> (std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wupi.codex");
        std::fs::write(&path, content).expect("write tmp");
        (path, dir)
    }

    #[test]
    fn parse_compound_file_splits_multiple_entries() {
        // Three top-level fences → three entries. The parser splits ONLY on a
        // `---` line preceded by a blank line (or at start-of-file), so prose
        // that uses `---` mid-line — em-dash substitutions, signature lines,
        // etc. — does NOT fragment an entry.
        let content = "---\ntitle: First\ntags: a, b\n---\nBody one.\n\n---\ntitle: Second\ntags: c\n---\nBody two has an em-dash phrase --- inline, mid-sentence.\n\n---\ntitle: Third\n---\nBody three.\n";
        let (path, _dir) = write_tmp_compound(content);
        let entries = parse_compound_file(&path).expect("parse");
        assert_eq!(entries.len(), 3, "exactly three top-level entries");
        assert_eq!(entries[0].title, "First");
        assert_eq!(entries[0].tags, vec!["a".to_owned(), "b".to_owned()]);
        assert!(entries[0].body.contains("Body one."));
        assert_eq!(entries[1].title, "Second");
        assert!(entries[1].body.contains("em-dash phrase --- inline"));
        assert_eq!(entries[2].title, "Third");
        assert!(entries[2].body.contains("Body three."));
    }

    #[test]
    fn parse_compound_file_single_entry() {
        // Degenerate case: a single fenced entry parses to one ParsedEntry.
        let content = "---\ntitle: Solo\ntags: x\n---\nJust one body.\n";
        let (path, _dir) = write_tmp_compound(content);
        let entries = parse_compound_file(&path).expect("parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Solo");
        assert!(entries[0].body.contains("Just one body."));
    }

    #[test]
    fn parse_compound_file_missing_returns_empty() {
        // A missing file is NOT an error — returns empty Vec so the boot seed
        // is a no-op (mirrors parse_dir's graceful degradation).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.codex");
        let entries = parse_compound_file(&path).expect("parse missing");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_compound_file_skips_empty_body_entries() {
        // An entry with front-matter but no body (whitespace only) is dropped,
        // so a stray fence pair doesn't produce a phantom entry.
        let content = "---\ntitle: Has Body\n---\nReal body.\n\n---\ntitle: Empty\n---\n   \n";
        let (path, _dir) = write_tmp_compound(content);
        let entries = parse_compound_file(&path).expect("parse");
        assert_eq!(entries.len(), 1, "empty-body entry was skipped");
        assert_eq!(entries[0].title, "Has Body");
    }

    #[test]
    fn parse_compound_file_hashes_differ_per_entry() {
        // The reconcile diff keys on hash; entries in the same file must hash
        // differently (else all updates would look like no-ops).
        let content = "---\ntitle: A\n---\nbody a\n\n---\ntitle: B\n---\nbody b\n";
        let (path, _dir) = write_tmp_compound(content);
        let entries = parse_compound_file(&path).expect("parse");
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0].hash, entries[1].hash, "distinct entries must hash differently");
    }

    #[test]
    fn parse_compound_file_hash_stable_across_calls() {
        // Idempotent re-seed relies on the hash being deterministic for the
        // same input bytes across invocations (else unchanged entries would
        // be re-embedded every boot — expensive).
        let content = "---\ntitle: Same\n---\nidentical body\n";
        let (path, _dir) = write_tmp_compound(content);
        let h1 = parse_compound_file(&path).expect("parse")[0].hash;
        let h2 = parse_compound_file(&path).expect("parse")[0].hash;
        assert_eq!(h1, h2);
    }

    /// Pins the playbook's namespace discriminator. The compound-file seed
    /// path hard-wires `system_codex::SYSTEM_NAMESPACE` so playbook entries
    /// land in the `__wupi_system__` partition's namespace (distinct from
    /// user-authored codex in `__codex__`). The `kind=codex` discriminator
    /// stays shared so the render frame + per-class floor apply uniformly.
    #[test]
    fn wupi_codex_namespace_is_wupi_system() {
        let json = build_metadata_json(
            "Playbook Entry",
            &["game-design".to_owned()],
            999,
            crate::system_codex::SYSTEM_NAMESPACE,
        );
        assert!(json.contains("\"namespace\":\"wupi_system\""));
        assert!(json.contains("\"kind\":\"codex\""));
        assert_eq!(extract_metadata_field(Some(&json), "namespace"), Some("wupi_system".to_owned()));
    }

    /// Smoke test against the shipped playbook: the data/wupi.codex file at
    /// the repo root (sibling of this Cargo project's src-tauri/) must parse
    /// into exactly the four authored entries, in order, with their expected
    /// titles. Pins the file the boot seed actually reads so a typo'd fence
    /// or a stray title: line in body prose doesn't silently fragment it.
    #[test]
    fn shipped_playbook_parses_to_four_entries() {
        let candidates = [
            // Cargo test cwd = src-tauri/, so the playbook is one level up.
            std::path::PathBuf::from("../data/wupi.codex"),
            // Standalone invocation from repo root.
            std::path::PathBuf::from("data/wupi.codex"),
        ];
        let path = candidates.iter().find(|p| p.is_file());
        let Some(path) = path else {
            // Not running from the repo (e.g. CI on a bare crate). Skip
            // rather than fail: this is a shipped-asset smoke test, not a
            // parser-contract test (those live above).
            eprintln!("shipped_playbook_parses_to_four_entries: data/wupi.codex not found; skipping");
            return;
        };
        let entries = parse_compound_file(path).expect("shipped playbook parses");
        assert_eq!(entries.len(), 4, "shipped playbook has exactly four entries");
        assert_eq!(entries[0].title, "Game System Design");
        assert_eq!(entries[1].title, "Authoring .sim Cards");
        assert_eq!(entries[2].title, "Authoring Codex Entries");
        assert_eq!(entries[3].title, "Director Tools");
        // Each entry must stay under the bge-small chunk budget (~1400 chars).
        for e in &entries {
            assert!(
                e.body.len() <= 1400,
                "playbook entry '{}' body is {} chars (budget 1400); bge-small may truncate",
                e.title,
                e.body.len()
            );
        }
    }

    /// Sibling of `shipped_playbook_parses_to_four_entries`: the shipped
    /// `data/fable.codex` (unified Fable playbook — shared by the Game Master
    /// interview persona AND the simulation narrator) must parse cleanly into
    /// its authored entries with valid front-matter + non-empty bodies, all
    /// under the bge-small chunk budget. Pins the file the boot seed reads so a
    /// typo'd fence or fragmented entry doesn't silently degrade Fable
    /// retrieval. **Unification (2026-07-29):** was `shipped_gm_playbook_
    /// parses_cleanly` reading `gm.codex`; renamed + threshold bumped to 11
    /// (9 original GM entries + 3 narrator entries added in the Phase 3 scrub).
    #[test]
    fn shipped_fable_codex_parses_cleanly() {
        let candidates = [
            // Cargo test cwd = src-tauri/, so the playbook is one level up.
            std::path::PathBuf::from("../data/fable.codex"),
            // Standalone invocation from repo root.
            std::path::PathBuf::from("data/fable.codex"),
        ];
        let path = candidates.iter().find(|p| p.is_file());
        let Some(path) = path else {
            eprintln!("shipped_fable_codex_parses_cleanly: data/fable.codex not found; skipping");
            return;
        };
        let entries = parse_compound_file(path).expect("shipped fable codex parses");
        assert!(
            entries.len() >= 11,
            "shipped fable codex has {} entries (expected >= 11); fable.codex may be malformed",
            entries.len()
        );
        // Each entry must have a title + non-empty tags + non-empty body +
        // stay under the bge-small chunk budget.
        for e in &entries {
            assert!(!e.title.is_empty(), "fable codex entry has empty title");
            assert!(!e.tags.is_empty(), "fable codex entry '{}' has no tags", e.title);
            assert!(!e.body.trim().is_empty(), "fable codex entry '{}' has empty body", e.title);
            assert!(
                e.body.len() <= 1400,
                "fable codex entry '{}' body is {} chars (budget 1400); bge-small may truncate",
                e.title,
                e.body.len()
            );
        }
        // Spot-check that the canonical playbook sections are present (titles
        // from the plan). If the playbook is reorganized, update these.
        let titles: Vec<&str> = entries.iter().map(|e| e.title.as_str()).collect();
        for expected in [
            "Card Archetypes",
            "The Core Question Ladder",
            "World Card Example — Fantasy Tavern",
            "World Card Example — Cyberpunk Bar",
            "Perfect Character Card Examples",
            "The Scribe Contract",
        ] {
            assert!(
                titles.contains(&expected),
                "fable codex missing required entry '{}'; found: {:?}",
                expected,
                titles
            );
        }
    }
}
