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
// (2026-08-20 P3) The persisted reconcile identity is `stable_hash64`
// (FNV-1a, below) — `DefaultHasher` is gone from every path (its std-
// internal algorithm carries no cross-version stability guarantee, and the
// hash lives in stored row metadata).
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

    // Chunk-budget gate: split any entry whose body exceeds the 1300-byte
    // CHUNK_CHAR_BUDGET (byte-enforced — see memory.rs) into paginated
    // "Title - Part N" children BEFORE the reconcile loop. Post-parse/
    // pre-reconcile placement keeps the parts the canonical title-keyed set
    // (idempotent re-seeds) and guarantees every body reaching
    // `add_codex_entry` is ≤1300 bytes — under the ~1400-byte bge
    // truncate (see memory.rs's clamp for the final backstop).
    //
    // Duplicate-title disambiguation runs FIRST (before the part split, so
    // an oversize duplicate's parts derive from its disambiguated title):
    // two source entries sharing a title (reachable with zero authoring
    // mistakes — front-matter that omits `title:` falls back to the same
    // stem) previously collapsed in the title-keyed reconcile map,
    // last-wins, and the loser's row became an unreachable zombie the purge
    // loop could never see. Later dupes are deterministically renamed
    // "Title (N)" (file order, N from 2) so every boot maps the same source
    // entry to the same title → re-seeds stay idempotent.
    let sources = expand_oversize_entries(dedupe_duplicate_titles(sources));

    // Reconcile diff: title-keyed, hash-detected. Rows are grouped per title
    // in Vecs — a title may LEGITIMATELY have multiple stored rows (legacy
    // duplicate-title damage, or pre-disambiguation seeds); grouping keeps
    // every row reachable by the purge loop instead of collapsing the map
    // last-wins (the 2026-08-15 audit: the loser's id became permanently
    // undeletable — `wipe_episodic_card` preserves codex rows, so no path
    // could ever remove it).
    let existing = engine.list_codex_entries(card_id).await?;
    // (2026-08-15 audit fix) Seed-vs-delete race guard: the parse + this list
    // span awaits during which `fable_card_delete` can purge the partition +
    // remove the card folder. Writing now would resurrect rows for a dead
    // partition (unreachable ghosts). The source file's existence is the
    // liveness signal — a delete removes it with the folder. Re-checked here,
    // directly before the write loop, the window shrinks from seconds (the
    // whole seed) to the loop's own duration.
    if !path.exists() {
        tracing::info!(
            card_id,
            namespace,
            "compound codex source vanished mid-seed (card deleted?); aborting seed"
        );
        return Ok(report);
    }
    let mut existing_by_title: HashMap<String, Vec<(MemoryId, Option<String>)>> = HashMap::new();
    for (id, metadata_json) in existing {
        let title = extract_metadata_field(metadata_json.as_deref(), "title").unwrap_or_default();
        existing_by_title
            .entry(title)
            .or_default()
            .push((id, extract_metadata_field(metadata_json.as_deref(), "hash")));
    }
    let mut consumed: HashSet<String> = HashSet::new();

    for src in &sources {
        // Take ALL stored rows for this title — matched keepers, stale-hash
        // rows, and same-title duplicates are handled in one pass.
        let had_rows;
        let mut kept_one = false;
        let mut delete_failed = false;
        match existing_by_title.remove(&src.title) {
            Some(rows) => {
                had_rows = true;
                for (id, hash) in rows {
                    let hash_matches = hash
                        .as_deref()
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|h| h == src.hash)
                        .unwrap_or(false);
                    if hash_matches && !kept_one {
                        // Keep exactly one row with the current hash.
                        kept_one = true;
                    } else {
                        // Stale hash (an update) or a duplicate row of the
                        // same title (the zombie heal) → delete.
                        if let Err(e) = engine.delete_memory(id).await {
                            tracing::warn!(
                                title = %src.title,
                                error = %format!("{e}"),
                                namespace,
                                "compound codex update: failed to delete old entry; skipping"
                            );
                            delete_failed = true;
                        }
                    }
                }
            }
            None => {
                had_rows = false;
            }
        }
        if kept_one {
            report.unchanged += 1;
            consumed.insert(src.title.clone());
            continue;
        }
        // No stored row matched the current hash: insert the source entry
        // (a brand-new seed, or an update after the deletes above). A failed
        // delete skips the insert so we never mint a second row for a title
        // whose stale row still lives.
        if delete_failed {
            continue;
        }
        match insert_entry(engine, src, card_id, namespace).await {
            Ok(()) => {
                if had_rows {
                    report.updated += 1;
                } else {
                    report.seeded += 1;
                }
                consumed.insert(src.title.clone());
            }
            Err(e) => tracing::warn!(
                title = %src.title,
                error = %format!("{e}"),
                namespace,
                "compound codex insert failed"
            ),
        }
    }
    // Purge stored entries whose source no longer exists. `existing_by_title`
    // now holds ONLY unconsumed titles (consumed ones were `remove`d above),
    // and every row under them is deletable — the grouped-Vec fix means a
    // legacy duplicate row can no longer hide from this loop.
    for (title, rows) in &existing_by_title {
        if !consumed.contains(title) {
            for (id, _) in rows {
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
    }
    Ok(report)
}

/// Deterministically disambiguate duplicate titles in parsed sources: the
/// first occurrence keeps the title; later duplicates become `"{title} ({n})"`
/// (n from 2, skipping collisions). Warns per rename. Pure — same input,
/// same output, so re-seeds reconcile cleanly on the renamed key.
fn dedupe_duplicate_titles(sources: Vec<ParsedEntry>) -> Vec<ParsedEntry> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(sources.len());
    for mut src in sources {
        if seen.insert(src.title.clone()) {
            out.push(src);
            continue;
        }
        let mut n = 2;
        while !seen.insert(format!("{} ({})", src.title, n)) {
            n += 1;
        }
        let renamed = format!("{} ({})", src.title, n);
        tracing::warn!(
            original = %src.title,
            renamed = %renamed,
            "compound codex: duplicate entry title — later entry disambiguated"
        );
        src.title = renamed;
        out.push(src);
    }
    out
}

/// Expand any entry whose body exceeds the episodic chunker's
/// [`CHUNK_CHAR_BUDGET`] (1300, enforced on UTF-8 BYTES) into paginated
/// `"{title} - Part N"` children, each within that budget, split on
/// sentence/paragraph boundaries via [`crate::memory::chunk_text`] (reused
/// from the episodic chunker). Gating at the SAME budget the episodic path
/// uses keeps codex rows consistent with chat rows; bge-small silently
/// truncates bodies past its cap, so 1300 also sits safely under it.
/// (2026-08-20 audit P2-8) The gate reads BYTES — `chunk_text`, the
/// `add_codex_entry` embed clamp, and `insert_entry`'s backstop log all
/// measure bytes, and every downstream consumer of a "≤1300" body means
/// bytes. The old chars-count gate let a ≤1300-CHAR CJK/accented body
/// (2–4× its byte length) skip the split + take a permanently clamped,
/// degraded embedding.
///
/// **Placement matters:** called post-parse + pre-reconcile in
/// [`seed_compound_codex`], so the parts become the canonical title-keyed set.
/// Re-seeding stays idempotent — the same source body yields the same parts +
/// per-part hashes → "unchanged", not churn (a split at insert-time would key
/// the reconcile on the original title + orphan the parts every boot).
///
/// Each part's hash is `stable_hash64` over its part body (deterministic), so
/// a body edit changes the parts' hashes + triggers an `updated` reconcile.
/// Entries already ≤1300 (bytes) pass through unchanged. (A single-chunk
/// split is unreachable in practice — every chunk is ≤1300, so any body
/// >1300 must yield ≥2 — but the n==1 branch stays as a defensive
/// collapse-to-original in case `chunk_text`'s budget ever drifts above the
/// gate.)
pub(crate) fn expand_oversize_entries(sources: Vec<ParsedEntry>) -> Vec<ParsedEntry> {
    const CAP: usize = crate::memory::CHUNK_CHAR_BUDGET;
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
            body_bytes = src.body.len(),
            parts = n,
            "codex entry exceeds the 1300-byte chunk budget; auto-splitting into paginated parts"
        );
        for (i, body) in chunks.into_iter().enumerate() {
            let title = if n > 1 {
                format!("{} - Part {}", src.title, i + 1)
            } else {
                src.title.clone()
            };
            let hash = stable_hash64(&body);
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
    if src.body.chars().count() > BUDGET_CHARS {
        tracing::error!(
            title = %src.title,
            body_chars = src.body.chars().count(),
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
        let hash = stable_hash64(chunk);

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

/// (2026-08-20 audit P3) Stable 64-bit FNV-1a over the UTF-8 bytes — the
/// codex reconcile hash PERSISTS in row metadata, and `DefaultHasher`
/// (SipHash-with-zero-keys) carries NO cross-version stability guarantee
/// from std: a future toolchain bump could silently change what every
/// stored hash means, orphaning the reconcile (every entry suddenly reads
/// `updated`, forever churning). FNV-1a is specified, dependency-free, and
/// frozen here for the process's lifetime. NOT cryptographic — identity
/// only. (One-time migration cost on the switch: stored SipHash values
/// mismatch the first re-seed, every entry updates once, then idempotence
/// resumes on FNV values.)
pub(crate) fn stable_hash64(s: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET_BASIS;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
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
///
/// (2026-08-16 audit follow-up) ROUND-TRIP GUARD: the compound format is
/// inherently ambiguous — a body containing a blank-line-preceded `---` line
/// followed by a `title:` line re-parses as TWO entries, silently splitting
/// authored lore at the next load. The guard is verify-then-sanitize: build
/// the text verbatim, re-parse it, and if the round-trip reproduces the
/// intent — same COUNT and, per entry, the same title + trimmed body
/// (2026-08-20 audit L4: count-only went blind when one side of the
/// collision empty-dropped — a forged fence with a whitespace-only tail
/// re-assigned bodies between titles while the count stayed equal) — the
/// bodies are byte-exact (full fidelity, the overwhelmingly common case).
/// Only on a mismatch are the COLLIDING body fences neutralized (`---` → `—`,
/// the same transform the front-matter sanitizer applies) — that is
/// serialization hygiene on text that would otherwise be silently corrupted
/// by the round trip, not a rewrite of authored lore.
pub fn format_compound_text(entries: &[ParsedEntry]) -> String {
    let verbatim = format_compound_text_verbatim(entries);
    // The parser drops empty-body chunks; the intent is the entries that
    // will actually materialize.
    let intended: Vec<(&str, &str)> = entries
        .iter()
        .filter(|e| !e.body.trim().is_empty())
        .map(|e| (e.title.as_str(), e.body.trim()))
        .collect();
    let reparsed = parse_compound_text(&verbatim, "");
    let round_trips = reparsed.len() == intended.len()
        && reparsed.iter().zip(intended.iter()).all(|(r, (title, body))| {
            r.title.trim() == *title && r.body.trim() == *body
        });
    if round_trips {
        return verbatim;
    }
    tracing::warn!(
        intended = intended.len(),
        reparsed = reparsed.len(),
        "codex compound round-trip mismatch (count or per-entry title/body) — neutralizing body fence collisions"
    );
    let sanitized: Vec<ParsedEntry> = entries
        .iter()
        .map(|e| {
            let mut e2 = e.clone();
            e2.body = neutralize_body_fences(&e.body);
            e2
        })
        .collect();
    format_compound_text_verbatim(&sanitized)
}

/// The verbatim serializer (the old `format_compound_text` body, unchanged).
fn format_compound_text_verbatim(entries: &[ParsedEntry]) -> String {
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

/// Neutralize ONLY the body fences that would forge an entry boundary on the
/// next parse: a `---` line that is blank-preceded (or opens the body — the
/// formatter always emits a blank line after its own closer) and whose next
/// non-empty line starts a `title:` key. Everything else stays verbatim.
/// Line endings normalize to LF inside a REWRITTEN body only when the guard
/// fired (the verbatim path never passes through here).
fn neutralize_body_fences(body: &str) -> String {
    let lines: Vec<&str> = body.split('\n').collect();
    let mut out = String::with_capacity(body.len());
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            let prev_blank = i == 0 || lines[i - 1].trim().is_empty();
            let next_title = lines[i + 1..]
                .iter()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim_start().to_lowercase().starts_with("title:"))
                .unwrap_or(false);
            if prev_blank && next_title {
                out.push('—');
                if i + 1 < lines.len() {
                    out.push('\n');
                }
                continue;
            }
        }
        out.push_str(line);
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

/// Split a compound codex file into its top-level entries. Each entry is the
/// text from one `---\n` opener up to (but not including) the next `---\n`
/// opener that begins a new entry (i.e. one preceded by a blank line AND
/// followed by a `title:` line, so `---` inside a body doesn't trigger a
/// false split — the load-bearing rule for entries that contain fenced code
/// blocks or horizontal rules).
pub fn split_compound(text: &str) -> Vec<&str> {
    // Walk line by line. An "entry-start fence" is a line that is exactly
    // `---` (after trimming) AND is preceded by either the start of the file
    // or a blank line AND whose next non-empty line starts a `title:` front-
    // matter key — the shape `format_compound_text` always emits after an
    // opener. A bare blank-preceded `---` (a body horizontal rule or a
    // fenced-code delimiter) stays inside the current entry.
    //
    // Each line's byte span in `text` is recorded alongside it so the final
    // slices are exact views of the ORIGINAL text. A find()-based re-slice of
    // LF-joined lines never matches inside a CRLF file (`lines()` strips the
    // `\r`, so the joined chunk is not a substring) and silently dropped
    // every entry — a `.codex` saved by Windows Notepad seeded nothing.
    let mut lines: Vec<&str> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    for line in text.split('\n') {
        // Mirror `str::lines()`: strip one trailing '\r' from the content.
        let content_end = pos + line.strip_suffix('\r').map_or(line.len(), str::len);
        lines.push(&text[pos..content_end]);
        spans.push((pos, content_end));
        pos += line.len() + 1;
    }
    // `str::lines()` yields no trailing empty line — drop the phantom entry
    // `split('\n')` produces for a newline-terminated text.
    if text.ends_with('\n') {
        lines.pop();
        spans.pop();
    }
    let mut starts: Vec<usize> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            let prev_blank = i == 0 || lines[i - 1].trim().is_empty();
            if prev_blank && opens_entry(&lines, i) {
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
    let mut out = Vec::new();
    for (idx, &start) in starts.iter().enumerate() {
        let end = if idx + 1 < starts.len() {
            // Up to the blank line preceding the next fence.
            starts[idx + 1].saturating_sub(1)
        } else {
            lines.len()
        };
        if end <= start {
            continue;
        }
        // The chunk spans the first line's start byte to the last line's
        // content end (interior line terminators, LF or CRLF, ride along).
        let chunk = &text[spans[start].0..spans[end - 1].1];
        if !chunk.trim().is_empty() {
            out.push(chunk);
        }
    }
    out
}

/// True when the `---` line at `fence_idx` opens a NEW entry rather than
/// sitting inside a body: the next non-empty line must start a `title:`
/// front-matter key (case-insensitive leniency — `parse_front_matter` still
/// reads the exact lowercase key, so a mixed-case `Title:` merely falls back
/// to the stem title rather than corrupting the split).
fn opens_entry(lines: &[&str], fence_idx: usize) -> bool {
    lines
        .iter()
        .skip(fence_idx + 1)
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim_start().to_ascii_lowercase().starts_with("title:"))
        .unwrap_or(false)
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
    // (2026-08-16 audit LOW) LOAD-BEARING ORDER: title + hash keys precede
    // `tags` — `extract_metadata_field` scans for the FIRST `"key"` literal,
    // and a tag element named "title"/"hash" would shadow the real key (a
    // "hash"-tagged entry's hash never matched → delete+reinsert every
    // seed). All string values are escaped, so a title CONTAINING those
    // words can't collide — only raw tag elements can.
    format!(
        "{{\"kind\":\"codex\",\"namespace\":\"{}\",\"title\":\"{}\",\"hash\":\"{}\",\"tags\":[{}]}}",
        escape_json_string(namespace),
        title_escaped,
        hash,
        tags_array
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
/// (2026-08-16 audit LOW) Assumes `build_metadata_json`'s key order — the
/// string keys precede `tags`, so a tag element named like a key can't
/// shadow the real one.
fn extract_metadata_field(metadata_json: Option<&str>, key: &str) -> Option<String> {
    let s = metadata_json?;
    // (2026-08-16 audit LOW) Key-POSITION guard: the needle includes the
    // colon, and the preceding non-whitespace char must be `{` or `,`. The
    // old bare-`"key"` search matched the text INSIDE a value — a codex
    // entry TITLED exactly "hash" serialized as `"title":"hash"` made the
    // hash lookup miss → the reconcile treated the stored entry as stale
    // and re-inserted a duplicate EVERY boot.
    let needle = format!("\"{key}\":");
    let mut search_from = 0;
    loop {
        let rel = s[search_from..].find(&needle)?;
        let idx = search_from + rel;
        let before = s[..idx].trim_end();
        if before.ends_with('{') || before.ends_with(',') {
            let after_key = &s[idx + needle.len()..];
            let after_colon = after_key.trim_start();
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
            return Some(raw.replace("\\\"", "\"").replace("\\\\", "\\"));
        }
        // Not a key position — keep searching (a later real key may follow).
        search_from = idx + needle.len();
    }
}

// ─── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-15 audit fix: duplicate source titles are deterministically
    /// disambiguated — same input, same output (idempotent re-seeds), and
    /// the fallback-stem collision (two front-matter blocks omitting
    /// `title:`) is covered too.
    #[test]
    fn dedupe_duplicate_titles_renames_deterministically() {
        let mk = |title: &str, body: &str| ParsedEntry {
            title: title.into(),
            tags: vec![],
            body: body.into(),
            hash: 0,
        };
        let sources = vec![mk("Lore", "a"), mk("Other", "b"), mk("Lore", "c"), mk("Lore", "d")];
        let out = dedupe_duplicate_titles(sources);
        let titles: Vec<&str> = out.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["Lore", "Other", "Lore (2)", "Lore (3)"]);
        // Deterministic: a second run over the same shape produces the same
        // titles (re-seed idempotence rides on this).
        let again = dedupe_duplicate_titles(vec![mk("Lore", "a"), mk("Lore", "c"), mk("Lore", "d")]);
        let titles2: Vec<&str> = again.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles2, vec!["Lore", "Lore (2)", "Lore (3)"]);
        // A source title colliding with an auto-renamed form disambiguates
        // off ITS OWN text (nested suffix — deterministic + unique, which
        // is the contract; pretty-printing collided collisions isn't worth
        // parsing the suffix back off).
        let tricky = dedupe_duplicate_titles(vec![mk("Lore", "a"), mk("Lore", "b"), mk("Lore (2)", "c")]);
        let titles3: Vec<&str> = tricky.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles3, vec!["Lore", "Lore (2)", "Lore (2) (2)"]);
    }

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

    /// A blank-line-preceded `---` that is NOT followed by a `title:` line
    /// (a body horizontal rule, or imported lore prose that happens to
    /// contain the fence sequence) stays inside the current entry.
    #[test]
    fn split_compound_does_not_split_on_bare_fence_without_title() {
        let text = "---\ntitle: Chronicle\ntags: war\n---\n\nThe march began.\n\n---\n\nThe war ended.\n";
        let parts = split_compound(text);
        assert_eq!(parts.len(), 1);
        let (_, body) = split_front_matter(parts[0]);
        assert!(body.contains("The march began."));
        assert!(body.contains("---"));
        assert!(body.contains("The war ended."));
    }

    #[test]
    fn split_compound_still_splits_real_entry_with_title() {
        // Sanity for the tightened rule: a genuine opener (blank-preceded
        // `---` + `title:`) still splits even when the previous body ends
        // without a trailing blank issue.
        let text = "---\ntitle: One\n---\n\nbody one.\n\n---\ntitle: Two\n---\n\nbody two.\n";
        let parts = split_compound(text);
        assert_eq!(parts.len(), 2);
    }

    /// CRLF files (Windows Notepad saves) must split + parse identically to
    /// LF files. The old find()-based re-slice of LF-joined chunks never
    /// matched inside CRLF text and silently dropped EVERY entry.
    #[test]
    fn split_compound_crlf_roundtrip() {
        let text = "---\r\ntitle: One\r\ntags: a\r\n---\r\n\r\nbody one.\r\n\r\n---\r\ntitle: Two\r\n---\r\n\r\nbody two.\r\n";
        let parts = split_compound(text);
        assert_eq!(parts.len(), 2, "CRLF entries must not be silently dropped");
        let (t1, _) = parse_front_matter(split_front_matter(parts[0]).0, "x");
        let (t2, _) = parse_front_matter(split_front_matter(parts[1]).0, "x");
        assert_eq!(t1, "One");
        assert_eq!(t2, "Two");
        let (_, b1) = split_front_matter(parts[0]);
        assert_eq!(b1.trim(), "body one.");
        // The full parse path (what the seeder runs) must yield both entries.
        let entries = parse_compound_text(text, "stem");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "One");
        assert_eq!(entries[1].title, "Two");
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

    /// Reconcile determinism: the same parsed text produces the same hash
    /// (so an unchanged file → all `unchanged` on re-seed). This is the
    /// idempotence invariant that makes the boot seed cheap. Runs through
    /// `stable_hash64` (the persisted identity, 2026-08-20 P3) — NOT
    /// `DefaultHasher`, whose std-internal algorithm is exactly the
    /// cross-version churn the switch eliminated.
    #[test]
    fn parsed_entry_hash_is_deterministic() {
        let text = "---\ntitle: X\ntags: a\n---\n\nbody.\n";
        let e1 = parse_compound_text(text, "stem");
        let e2 = parse_compound_text(text, "stem");
        assert!(!e1.is_empty(), "fixture must parse to one entry");
        assert_eq!(
            e1[0].hash, e2[0].hash,
            "same text must parse to the same hash (idempotent re-seed)"
        );
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
        // Every part body fits the chunk budget.
        for p in &out {
            assert!(
                p.body.len() <= crate::memory::CHUNK_CHAR_BUDGET,
                "part '{}' is {} chars (> budget {})",
                p.title,
                p.body.len(),
                crate::memory::CHUNK_CHAR_BUDGET
            );
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

    /// The 1301–1400 band: over the 1300 chunk budget but under the ~1400 bge
    /// truncate. It used to bypass the split entirely (only the embedder clamp
    /// guarded it); the gate now sits at the chunk budget, so it splits.
    #[test]
    fn expand_oversize_entries_splits_the_1301_to_1400_band() {
        let body = "x".repeat(crate::memory::CHUNK_CHAR_BUDGET + 50);
        assert!(body.len() > crate::memory::CHUNK_CHAR_BUDGET);
        assert!(body.len() <= 1400, "test body must stay in the band");
        let src = ParsedEntry {
            title: "Band".to_string(),
            tags: vec![],
            body,
            hash: 7,
        };
        let out = expand_oversize_entries(vec![src]);
        assert!(out.len() >= 2, "band body must split, got {}", out.len());
        for p in &out {
            assert!(p.body.len() <= crate::memory::CHUNK_CHAR_BUDGET);
        }
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

    /// (2026-08-16 audit follow-up pin) A lore body containing a blank-line-
    /// preceded `---` + `title:` line — the compound format's one ambiguity —
    /// must round-trip as ONE entry: `format_compound_text` verifies the
    /// round-trip and neutralizes only the colliding fence.
    #[test]
    fn format_round_trips_body_containing_forged_entry_boundary() {
        let entries = vec![ParsedEntry {
            title: "The Schism".to_string(),
            tags: vec!["history".to_string()],
            body: "Ancient records quote the treaty scroll verbatim:\n\n\
                   ---\ntitle: Forged Treaty\n---\n\n\
                   The text of the treaty follows here.".to_string(),
            hash: 0,
        }];
        let text = format_compound_text(&entries);
        let reparsed = parse_compound_text(&text, "stem");
        assert_eq!(reparsed.len(), 1, "the forged boundary must not split the entry");
        assert_eq!(reparsed[0].title, "The Schism");
        assert!(
            reparsed[0].body.contains("The text of the treaty follows here."),
            "the tail prose after the collision survives inside the one entry"
        );
        assert!(
            !reparsed[0].body.contains("---\ntitle: Forged Treaty"),
            "the colliding fence is neutralized in the written form"
        );
    }

    /// The mirror case: bodies with NO collision stay byte-exact (the guard
    /// must not rewrite ordinary lore — plain `---` horizontal rules and
    /// fenced code blocks in bodies are untouched).
    #[test]
    fn format_keeps_collision_free_bodies_verbatim() {
        let body = "Section one.\n\n---\n\nSection two after a horizontal rule.".to_string();
        let entries = vec![ParsedEntry {
            title: "Rule".to_string(),
            tags: vec![],
            body: body.clone(),
            hash: 0,
        }];
        let text = format_compound_text(&entries);
        assert!(text.contains(&body), "no collision → body verbatim");
        assert_eq!(parse_compound_text(&text, "stem").len(), 1);
    }
}
