//! Prism — the WUPI image-generation app (2026-07-31).
//!
//! A dedicated full-screen app (sibling of Fable) for authoring + browsing
//! locally-generated Stable Diffusion images. This module owns two things:
//!
//! 1. **The Glass Vault** — a SQLite gallery (`apps/prism/gallery.sqlite`)
//!    mapping each generated image to its full generation metadata (prompt,
//!    negative prompt, seed, sampler, CFG, steps, dimensions, model). The
//!    frontend reads this to render the masonry grid + the metadata panel +
//!    to seed Fork & Edit from a prior result.
//! 2. **The generation entrypoint** — [`generate`] builds a
//!    [`scene_art::SceneImageRequest`] from user-supplied [`GenerateParams`],
//!    resolves a unique destination path, and hands off to the shared VRAM
//!    swap pipeline (`run_sd_swap_core`, extracted from §11.58's
//!    `run_sd_swap_from_arcs`). Prism does NOT own VRAM management — it reuses
//!    the single global context-swap lease that Fable's scene-art path uses,
//!    so a Prism render evicts the text models, renders, and reloads them via
//!    the same proven cycle (the §2B invariant: never strand the system with
//!    no resident model).
//!
//! ## Why a separate DB (not the memory engine)
//!
//! The memory engine (`memory.rs`) is the WUPI assistant's semantic-recall
//! store; gallery metadata is unrelated (no embeddings, no RAG). A dedicated
//! `GalleryDb` keeps concerns separate + lets the gallery live under
//! `apps/prism/` alongside its images (the per-app state convention,
//! `lib.rs:resolve_apps_dir`), matching how Fable keeps saves/cards/images
//! under `apps/fable/`.
//!
//! ## Storage shape
//!
//! - Images: `apps/prism/gallery/<timestamp_ms>-<seed>.png` (timestamp-prefixed
//!   so a directory listing is chronologically sorted; seed-suffixed so two
//!   seed-locked forks of the same base don't collide).
//! - DB: `apps/prism/gallery.sqlite` (WAL mode, idempotent schema — the
//!   `CREATE TABLE IF NOT EXISTS` pattern from `memory::init_schema`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection};

use crate::scene_art::{self, SceneImageRequest};

// ────────────────────────────────────────────────────────────────────────────
// Row types
// ────────────────────────────────────────────────────────────────────────────

/// One gallery image + its full generation metadata. This is the shape the
/// frontend receives over IPC (serde → JSON). Every field the Tag Composer +
/// Fork & Edit need to reproduce or tweak a generation is here.
///
/// `id` is the SQLite rowid (stable across the image's lifetime). `path` is the
/// absolute on-disk PNG path; the frontend converts it to an `asset://` URL via
/// `convertFileSrc` (the asset-protocol scope includes `apps/prism/gallery/**`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GalleryImage {
    pub id: i64,
    /// Unix-epoch milliseconds at generation time (drives the grid sort +
    /// the metadata-panel timestamp display).
    pub created_at: i64,
    /// Absolute path to the PNG on disk.
    pub path: String,
    pub prompt: String,
    /// Empty string when no negative prompt was used (serde-friendly: the
    /// frontend treats empty as "none" rather than null-checking).
    pub negative_prompt: String,
    /// The RNG seed used. `-1` means the backend chose a random seed (the
    /// crate default); in that case the ACTUAL seed the backend used is not
    /// recoverable from the request (the crate doesn't echo it back), so the
    /// row records `-1` and Fork-from-this-image falls back to a fresh random
    /// seed. A `>= 0` seed is locked + reproducible (Fork & Edit's primitive).
    pub seed: i64,
    pub cfg: f32,
    pub steps: i32,
    pub width: i32,
    pub height: i32,
    /// The sampler discriminant (mirrors `SceneImageRequest.sampling_method`'s
    /// i32 contract — see `scene_art::DPMPP2M_DISCRIMINANT` + `sampler_from_i32`).
    pub sampler: i32,
    /// The model file name (not full path — just the leaf, e.g.
    /// "Image.safetensors"). The full path is reconstructable from
    /// `resolve_sd_model_path` at fork time, but the leaf is what the
    /// metadata panel displays.
    pub model: String,
    /// 1 if favorited, 0 otherwise. Drives the favorites filter.
    pub favorite: i32,
    /// 1 if soft-deleted (in the Trash), 0 otherwise. Delete is a trash-mark,
    /// not a row removal — matches every gallery UX the user will have used.
    /// A separate IPC purges trashed rows + their files.
    pub trashed: i32,
}

/// The filter for [`GalleryDb::list`]. The default (all three false/empty) is
/// "all non-trashed images, newest first."
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct GalleryFilter {
    /// Only favorited images.
    #[serde(default)]
    pub favorites_only: bool,
    /// Only trashed images (the Trash view). When false, trashed images are
    /// excluded; when true, ONLY trashed images are returned.
    #[serde(default)]
    pub trashed_only: bool,
    /// Case-insensitive substring search across `prompt`. Empty = no filter.
    #[serde(default)]
    pub search: String,
}

/// The user-supplied generation request (from the Tag Composer / Fork & Edit).
/// Maps onto [`SceneImageRequest`] inside [`generate`]. Kept as a separate
/// type from `SceneImageRequest` because the user doesn't supply the `dest`
/// path or the `model_path` (those are resolved server-side) — the IPC surface
/// is intentionally narrower than the internal request.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GenerateParams {
    pub prompt: String,
    #[serde(default)]
    pub negative_prompt: Option<String>,
    /// `-1` = random; `>= 0` = locked (Fork & Edit). Mirrors the crate.
    #[serde(default = "default_seed")]
    pub seed: i64,
    #[serde(default = "default_cfg")]
    pub cfg: f32,
    #[serde(default = "default_steps")]
    pub steps: i32,
    #[serde(default = "default_width")]
    pub width: i32,
    #[serde(default = "default_height")]
    pub height: i32,
    #[serde(default = "default_sampler")]
    pub sampler: i32,
}

fn default_seed() -> i64 { -1 }
fn default_cfg() -> f32 { 5.0 }
fn default_steps() -> i32 { 28 }
fn default_width() -> i32 { 1024 }
fn default_height() -> i32 { 576 }
fn default_sampler() -> i32 { scene_art::DPMPP2M_DISCRIMINANT }

/// Manual `Default` so the derived shape matches the `#[serde(default)]`
/// helpers EXACTLY — a `GenerateParams { prompt, ..Default::default() }`
/// (used in tests + as the composer's initial state) must produce the same
/// generation knobs a frontend gets when it omits fields. Keeps serde +
/// in-process construction in lockstep.
impl Default for GenerateParams {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            negative_prompt: None,
            seed: default_seed(),
            cfg: default_cfg(),
            steps: default_steps(),
            width: default_width(),
            height: default_height(),
            sampler: default_sampler(),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// GalleryDb — the SQLite gallery
// ────────────────────────────────────────────────────────────────────────────

/// The gallery database. Mirrors `MemoryEngine`'s shape: an
/// `Arc<Mutex<Connection>>` held in AppState, WAL mode, idempotent schema.
/// Blocking SQLite work runs inside the IPC handlers (which are `async` but
/// the rusqlite calls are sync + fast on a local DB of this size).
pub struct GalleryDb {
    conn: Arc<Mutex<Connection>>,
}

impl GalleryDb {
    /// Open (or create) the gallery DB at `path`. Sets WAL + runs the
    /// idempotent schema. The parent dir MUST exist (the caller creates
    /// `apps/prism/` at boot).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .map_err(|e| anyhow::anyhow!("open gallery db: {e:?}"))?;
        // WAL: the gallery grid (read) + a generation insert (write) shouldn't
        // block each other. Cheap on SSD; mirrors the memory engine.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| anyhow::anyhow!("set gallery WAL: {e:?}"))?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert a freshly-generated image's metadata. Returns the new row id.
    /// `path` is the absolute PNG path; `model` is the model file leaf name.
    pub fn insert(&self, row: &NewImage) -> anyhow::Result<i64> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        conn.execute(
            "INSERT INTO images
                (created_at, path, prompt, negative_prompt, seed, cfg, steps,
                 width, height, sampler, model, favorite, trashed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, 0)",
            params![
                row.created_at,
                row.path,
                row.prompt,
                row.negative_prompt,
                row.seed,
                row.cfg,
                row.steps,
                row.width,
                row.height,
                row.sampler,
                row.model,
            ],
        )
        .map_err(|e| anyhow::anyhow!("gallery insert: {e:?}"))?;
        Ok(conn.last_insert_rowid())
    }

    /// List images matching `filter`, newest first, paginated. `limit` ≤ 0
    /// means a sane default (100); capped at 500 to avoid an accidental
    /// full-table dump over IPC.
    pub fn list(&self, filter: &GalleryFilter, limit: i64, offset: i64) -> anyhow::Result<Vec<GalleryImage>> {
        let limit = if limit <= 0 { 100 } else { limit.min(500) };
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        // Build two SQL variants (with/without search) so the `?` placeholder
        // count always matches the bound params. rusqlite uses positional `?`
        // (NOT named params), so the search clause contributes one extra `?`
        // before the limit/offset pair — the two branches bind matching tuples.
        let trashed_clause = if filter.trashed_only { "1" } else { "0" };
        let fav_clause = if filter.favorites_only { "AND favorite = 1" } else { "" };
        let order = "ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?";
        let select = "SELECT id, created_at, path, prompt, negative_prompt, seed, cfg,
                             steps, width, height, sampler, model, favorite, trashed
                      FROM images";
        if filter.search.trim().is_empty() {
            let sql = format!("{select} WHERE trashed = {trashed_clause} {fav_clause} {order}");
            let mut stmt = conn.prepare(&sql).map_err(|e| anyhow::anyhow!("gallery list prepare: {e:?}"))?;
            let rows = stmt
                .query_map(params![limit, offset], row_to_image)
                .map_err(|e| anyhow::anyhow!("gallery list query: {e:?}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("gallery list row: {e:?}"))
        } else {
            // LOWER() on both sides for case-insensitive substring match. The
            // `%...%` wildcards wrap the trimmed search term.
            let needle = format!("%{}%", filter.search.trim());
            let sql = format!(
                "{select} WHERE trashed = {trashed_clause} {fav_clause} \
                 AND LOWER(prompt) LIKE LOWER(?) {order}"
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| anyhow::anyhow!("gallery list prepare: {e:?}"))?;
            let rows = stmt
                .query_map(params![needle, limit, offset], row_to_image)
                .map_err(|e| anyhow::anyhow!("gallery list query: {e:?}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("gallery list row: {e:?}"))
        }
    }

    /// Fetch one image by id (regardless of trashed state).
    pub fn get(&self, id: i64) -> anyhow::Result<Option<GalleryImage>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, path, prompt, negative_prompt, seed, cfg,
                        steps, width, height, sampler, model, favorite, trashed
                 FROM images WHERE id = ?",
            )
            .map_err(|e| anyhow::anyhow!("gallery get prepare: {e:?}"))?;
        let mut rows = stmt
            .query_map(params![id], row_to_image)
            .map_err(|e| anyhow::anyhow!("gallery get query: {e:?}"))?;
        match rows.next() {
            Some(Ok(img)) => Ok(Some(img)),
            Some(Err(e)) => Err(anyhow::anyhow!("gallery get row: {e:?}")),
            None => Ok(None),
        }
    }

    /// Toggle the favorite flag. `fav=true` sets it; `false` clears it.
    pub fn set_favorite(&self, id: i64, fav: bool) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        conn.execute(
            "UPDATE images SET favorite = ? WHERE id = ?",
            params![if fav { 1 } else { 0 }, id],
        )
        .map_err(|e| anyhow::anyhow!("gallery set_favorite: {e:?}"))?;
        Ok(())
    }

    /// Soft-delete (move to trash). Reversible via [`restore`].
    pub fn trash(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        conn.execute("UPDATE images SET trashed = 1 WHERE id = ?", params![id])
            .map_err(|e| anyhow::anyhow!("gallery trash: {e:?}"))?;
        Ok(())
    }

    /// Restore from trash.
    pub fn restore(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        conn.execute("UPDATE images SET trashed = 0 WHERE id = ?", params![id])
            .map_err(|e| anyhow::anyhow!("gallery restore: {e:?}"))?;
        Ok(())
    }

    /// HARD delete: remove the row + return its path so the caller can unlink
    /// the PNG file. Used by the trash-empty action. Returns Ok(None) if the
    /// row didn't exist (idempotent).
    pub fn purge(&self, id: i64) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        let path: Option<String> = conn
            .query_row("SELECT path FROM images WHERE id = ?", params![id], |r| r.get(0))
            .ok();
        if path.is_some() {
            conn.execute("DELETE FROM images WHERE id = ?", params![id])
                .map_err(|e| anyhow::anyhow!("gallery purge: {e:?}"))?;
        }
        Ok(path)
    }
}

/// The insert payload (the fields the generation entrypoint fills before
/// `GalleryDb::insert`). Sibling of `GalleryImage` minus the id/favorite/trashed
/// (those are DB-assigned).
#[derive(Debug, Clone)]
pub struct NewImage {
    pub created_at: i64,
    pub path: String,
    pub prompt: String,
    pub negative_prompt: String,
    pub seed: i64,
    pub cfg: f32,
    pub steps: i32,
    pub width: i32,
    pub height: i32,
    pub sampler: i32,
    pub model: String,
}

/// Map a rusqlite row (in the fixed SELECT column order) to a `GalleryImage`.
/// Single source of truth for the row-to-struct mapping — `list` and `get`
/// both use it.
fn row_to_image(row: &rusqlite::Row<'_>) -> rusqlite::Result<GalleryImage> {
    Ok(GalleryImage {
        id: row.get(0)?,
        created_at: row.get(1)?,
        path: row.get(2)?,
        prompt: row.get(3)?,
        negative_prompt: row.get(4)?,
        seed: row.get(5)?,
        cfg: row.get(6)?,
        steps: row.get(7)?,
        width: row.get(8)?,
        height: row.get(9)?,
        sampler: row.get(10)?,
        model: row.get(11)?,
        favorite: row.get(12)?,
        trashed: row.get(13)?,
    })
}

/// Idempotent schema init (mirrors `memory::init_schema`). `CREATE TABLE IF
/// NOT EXISTS` makes fresh-create + reopen the same code path; no separate
/// migration runner. Additive only (a future column is an `ALTER TABLE ADD
/// COLUMN` guarded by a PRAGMA check, never a destructive change — gallery
/// rows are user data).
fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS images (
            id               INTEGER PRIMARY KEY,
            created_at       INTEGER NOT NULL,
            path             TEXT    NOT NULL,
            prompt           TEXT    NOT NULL DEFAULT '',
            negative_prompt  TEXT    NOT NULL DEFAULT '',
            seed             INTEGER NOT NULL DEFAULT -1,
            cfg              REAL    NOT NULL DEFAULT 5.0,
            steps            INTEGER NOT NULL DEFAULT 28,
            width            INTEGER NOT NULL DEFAULT 1024,
            height           INTEGER NOT NULL DEFAULT 576,
            sampler          INTEGER NOT NULL DEFAULT 5,
            model            TEXT    NOT NULL DEFAULT '',
            favorite         INTEGER NOT NULL DEFAULT 0,
            trashed          INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_images_created_at ON images (created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_images_trashed     ON images (trashed);
        CREATE INDEX IF NOT EXISTS idx_images_favorite    ON images (favorite);
        "#,
    )
    .map_err(|e| anyhow::anyhow!("gallery init_schema: {e:?}"))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Generation entrypoint
// ────────────────────────────────────────────────────────────────────────────

/// Build a unique destination PNG path for a generation. Timestamp-prefixed
/// (chronological dir listing) + seed-suffixed (seed-locked forks of the same
/// base don't collide). Lives under `apps/prism/gallery/`.
pub fn dest_path(gallery_dir: &Path, seed: i64, created_at_ms: i64) -> PathBuf {
    gallery_dir.join(format!("{created_at_ms}-{seed}.png"))
}

/// Convert user-supplied [`GenerateParams`] into the internal
/// [`SceneImageRequest`] that the shared swap pipeline consumes. Pure (no I/O);
/// pulled out so the params→request mapping is unit-testable without the SD
/// backend. `model_path` is the resolved SD checkpoint; `dest` is the output
/// PNG path (from [`dest_path`]).
pub fn build_request(
    p: &GenerateParams,
    model_path: PathBuf,
    dest: PathBuf,
) -> SceneImageRequest {
    SceneImageRequest {
        prompt: p.prompt.clone(),
        negative_prompt: p.negative_prompt.clone().filter(|s| !s.trim().is_empty()),
        seed: p.seed,
        cfg_scale: p.cfg,
        steps: p.steps.max(1) as u32,
        width: p.width.max(64) as u32,
        height: p.height.max(64) as u32,
        sampling_method: p.sampler,
        model_path,
        dest,
    }
}

// ════════════════════════════════════════════════════════════════════════════
// TESTS
// ════════════════════════════════════════════════════════════════════════════
// The gallery DB is exercised with a temp-dir SQLite (real rusqlite, no mocks)
// — the same fidelity as the memory-engine tests. The generation entrypoint
// tests use `NoopImageGenerator` so they run WITHOUT the `diffusion-rs` cargo
// feature (no CUDA, no model file).

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp gallery DB. Each test gets its own file.
    fn temp_db() -> GalleryDb {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wupi_prism_test_{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // try a unique name per test invocation to avoid parallel-test collisions
        let path = dir.join(format!(
            "wupi_prism_test_{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        GalleryDb::open(&path).expect("open temp gallery db")
    }

    fn sample_new_image(seed: i64) -> NewImage {
        NewImage {
            created_at: 1700000000000,
            path: "/tmp/img.png".into(),
            prompt: "1girl, sunset".into(),
            negative_prompt: "low quality".into(),
            seed,
            cfg: 5.0,
            steps: 28,
            width: 1024,
            height: 576,
            sampler: scene_art::DPMPP2M_DISCRIMINANT,
            model: "Image.safetensors".into(),
        }
    }

    // ── Schema ──────────────────────────────────────────────────────────────

    #[test]
    fn open_is_idempotent() {
        // Opening twice (re-open existing) must succeed + not duplicate schema.
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "wupi_prism_idem_{}.sqlite",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let db = GalleryDb::open(&path).expect("first open");
            let id = db.insert(&sample_new_image(1)).unwrap();
            assert!(id > 0);
        }
        let db = GalleryDb::open(&path).expect("second open (re-open existing)");
        // The row from the first session survives the re-open.
        let rows = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(rows.len(), 1, "row survives a re-open");
    }

    // ── CRUD ────────────────────────────────────────────────────────────────

    #[test]
    fn insert_then_list_round_trip() {
        let db = temp_db();
        let id = db.insert(&sample_new_image(42)).unwrap();
        let rows = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.id, id);
        assert_eq!(r.prompt, "1girl, sunset");
        assert_eq!(r.negative_prompt, "low quality");
        assert_eq!(r.seed, 42);
        assert_eq!(r.cfg, 5.0);
        assert_eq!(r.steps, 28);
        assert_eq!(r.sampler, scene_art::DPMPP2M_DISCRIMINANT);
        assert_eq!(r.model, "Image.safetensors");
        assert_eq!(r.favorite, 0);
        assert_eq!(r.trashed, 0);
    }

    #[test]
    fn list_is_newest_first() {
        let db = temp_db();
        let mut older = sample_new_image(1);
        older.created_at = 1000;
        let mut newer = sample_new_image(2);
        newer.created_at = 2000;
        db.insert(&older).unwrap();
        db.insert(&newer).unwrap();
        let rows = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].created_at, 2000, "newest first");
        assert_eq!(rows[1].created_at, 1000);
    }

    #[test]
    fn get_returns_the_row() {
        let db = temp_db();
        let id = db.insert(&sample_new_image(7)).unwrap();
        let img = db.get(id).unwrap().expect("row exists");
        assert_eq!(img.seed, 7);
        // get on a missing id returns None (not an error).
        assert!(db.get(999999).unwrap().is_none());
    }

    // ── favorite / trash / restore / purge ─────────────────────────────────

    #[test]
    fn favorite_toggles() {
        let db = temp_db();
        let id = db.insert(&sample_new_image(1)).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().favorite, 0);
        db.set_favorite(id, true).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().favorite, 1);
        db.set_favorite(id, false).unwrap();
        assert_eq!(db.get(id).unwrap().unwrap().favorite, 0);
    }

    #[test]
    fn favorites_only_filter() {
        let db = temp_db();
        let a = db.insert(&sample_new_image(1)).unwrap();
        let _b = db.insert(&sample_new_image(2)).unwrap();
        db.set_favorite(a, true).unwrap();
        let all = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(all.len(), 2, "no filter → both");
        let fav = db.list(
            &GalleryFilter { favorites_only: true, ..Default::default() },
            10,
            0,
        ).unwrap();
        assert_eq!(fav.len(), 1);
        assert_eq!(fav[0].id, a);
    }

    #[test]
    fn trash_excludes_from_default_and_appears_in_trash_view() {
        let db = temp_db();
        let a = db.insert(&sample_new_image(1)).unwrap();
        let _b = db.insert(&sample_new_image(2)).unwrap();
        db.trash(a).unwrap();
        // default excludes trashed
        let live = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(live.len(), 1);
        assert_ne!(live[0].id, a, "trashed row excluded");
        // trashed_only shows only trashed
        let trash = db.list(
            &GalleryFilter { trashed_only: true, ..Default::default() },
            10,
            0,
        ).unwrap();
        assert_eq!(trash.len(), 1);
        assert_eq!(trash[0].id, a);
        // restore brings it back
        db.restore(a).unwrap();
        let live = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(live.len(), 2, "restored row reappears");
    }

    #[test]
    fn purge_removes_row_and_returns_path() {
        let db = temp_db();
        let id = db.insert(&sample_new_image(1)).unwrap();
        let path = db.purge(id).unwrap();
        assert_eq!(path.as_deref(), Some("/tmp/img.png"));
        assert!(db.get(id).unwrap().is_none(), "purged row gone");
        // purge again is a no-op (idempotent).
        let again = db.purge(id).unwrap();
        assert!(again.is_none());
    }

    // ── search ──────────────────────────────────────────────────────────────

    #[test]
    fn search_is_case_insensitive_substring_on_prompt() {
        let db = temp_db();
        let mut a = sample_new_image(1);
        a.prompt = "1girl, Classroom, sunset".into();
        let mut b = sample_new_image(2);
        b.prompt = "a dragon".into();
        db.insert(&a).unwrap();
        db.insert(&b).unwrap();
        let hits = db.list(
            &GalleryFilter { search: "class".into(), ..Default::default() },
            10,
            0,
        ).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].prompt.to_lowercase().contains("class"));
    }

    // ── generation entrypoint (no SD backend needed) ───────────────────────

    #[test]
    fn build_request_maps_all_params() {
        let p = GenerateParams {
            prompt: "1girl".into(),
            negative_prompt: Some("  ".into()), // whitespace-only → None
            seed: 12345,
            cfg: 7.5,
            steps: 20,
            width: 832,
            height: 1216,
            sampler: 1, // Euler a
        };
        let req = build_request(&p, PathBuf::from("/m/Image.safetensors"), PathBuf::from("/o/out.png"));
        assert_eq!(req.prompt, "1girl");
        assert!(req.negative_prompt.is_none(), "whitespace-only negative → None");
        assert_eq!(req.seed, 12345, "locked seed passes through");
        assert_eq!(req.cfg_scale, 7.5);
        assert_eq!(req.steps, 20);
        assert_eq!(req.width, 832);
        assert_eq!(req.height, 1216);
        assert_eq!(req.sampling_method, 1);
        assert_eq!(req.model_path, PathBuf::from("/m/Image.safetensors"));
        assert_eq!(req.dest, PathBuf::from("/o/out.png"));
    }

    #[test]
    fn build_request_defaults_match_fable_scene_art() {
        // An empty/minimal params object should produce a request whose
        // sampler/cfg/steps/dims defaults match the §11.58 FABLE scene-art
        // shape (so a no-op Prism gen is byte-identical to a FABLE scene gen).
        let p = GenerateParams {
            prompt: "x".into(),
            ..Default::default()
        };
        let req = build_request(&p, PathBuf::new(), PathBuf::new());
        assert_eq!(req.seed, -1, "default random seed");
        assert_eq!(req.cfg_scale, 5.0);
        assert_eq!(req.steps, 28);
        assert_eq!(req.width, 1024);
        assert_eq!(req.height, 576);
        assert_eq!(req.sampling_method, scene_art::DPMPP2M_DISCRIMINANT);
    }

    #[test]
    fn build_request_clamps_nonpositive_dims_and_steps() {
        // Defensive: a bad UI value (0 / negative) never reaches the crate as
        // 0 (which would fail or hang the render). Clamped to sane floors.
        let p = GenerateParams {
            prompt: "x".into(),
            steps: 0,
            width: -100,
            height: 0,
            ..Default::default()
        };
        let req = build_request(&p, PathBuf::new(), PathBuf::new());
        assert!(req.steps >= 1);
        assert!(req.width >= 64);
        assert!(req.height >= 64);
    }

    #[test]
    fn dest_path_is_seed_and_timestamp_unique() {
        let dir = Path::new("/gallery");
        let a = dest_path(dir, 42, 1000);
        let b = dest_path(dir, 42, 1001); // same seed, different time
        let c = dest_path(dir, 99, 1000); // same time, different seed
        assert_ne!(a, b, "different timestamp → different path");
        assert_ne!(a, c, "different seed → different path");
        assert!(a.to_string_lossy().ends_with("1000-42.png"));
        // Two seed-locked forks of the SAME base (same seed, close timestamps)
        // do NOT collide because the timestamp differs.
        let d = dest_path(dir, 42, 2000);
        assert_ne!(a, d, "forks of same seed don't collide (timestamp differs)");
    }

    #[test]
    fn noop_backend_generation_smoke() {
        // Exercise the SceneImageGenerator contract end-to-end with the stub
        // backend (no diffusion-rs feature, no model file, no CUDA). Proves
        // the request-building → backend → dest-exists path is wired.
        use crate::scene_art::SceneImageGenerator; // trait methods (load/generate/unload)
        let gen = scene_art::NoopImageGenerator;
        let dest = std::env::temp_dir().join(format!(
            "wupi_prism_noop_{}.png",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&dest);
        let req = build_request(
            &GenerateParams { prompt: "test".into(), ..Default::default() },
            PathBuf::from("/nonexistent/model.gguf"),
            dest.clone(),
        );
        gen.load(Path::new("/nonexistent/model.gguf")).unwrap();
        let result = gen.generate(&req).expect("noop generate succeeds");
        assert!(dest.exists(), "noop must write the dest file");
        assert_eq!(result.dest, dest);
        gen.unload();
        let _ = std::fs::remove_file(&dest);
    }
}
