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
    /// i32 contract — see `scene_art::EULER_A_DISCRIMINANT` + `sampler_from_i32`).
    pub sampler: i32,
    /// The model file name (not full path — just the leaf, e.g.
    /// "Image.safetensors"). The full path is reconstructable from
    /// `resolve_sd_model_path` at fork time, but the leaf is what the
    /// metadata panel displays.
    pub model: String,
    /// 1 if favorited, 0 otherwise. Drives the favorites filter.
    pub favorite: i32,
}

/// The filter for [`GalleryDb::list`]. The default (both false/empty) is
/// "all images, newest first."
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct GalleryFilter {
    /// Only favorited images.
    #[serde(default)]
    pub favorites_only: bool,
    /// Case-insensitive substring search across `prompt`. Empty = no filter.
    #[serde(default)]
    pub search: String,
}

/// The user-supplied generation request (from the Tag Composer / Fork & Edit).
/// Maps onto [`SceneImageRequest`] inside [`generate`]. Kept as a separate
/// type from `SceneImageRequest` because the user doesn't supply the `dest`
/// path or the `model_path` (those are resolved server-side) — the IPC surface
/// is intentionally narrower than the internal request.
///
/// **LOCKED RECIPE (Chloe ruling, 2026-08-17):** `cfg`, `steps`, `sampler`,
/// and `negative_prompt` are ACCEPTED BUT IGNORED — [`build_request`] always
/// normalizes onto the locked NoobAI v1.1 official recipe (Euler a + the
/// discrete schedule, 30 steps, CFG 6.0, injected quality prefix + negative
/// block; the launch recipe was DPM++ 2M + Karras at 20 — switched the same
/// day onto the model card's recommended parameters). The fields stay
/// deserializable so stale frontends + old Fork payloads keep working; the
/// only live knobs are `prompt`, `seed`, `width`, `height`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GenerateParams {
    pub prompt: String,
    /// IGNORED (locked recipe): the negative block is engine-injected.
    #[serde(default)]
    pub negative_prompt: Option<String>,
    /// `-1` = random; `>= 0` = locked (Fork & Edit).
    #[serde(default = "default_seed")]
    pub seed: i64,
    /// IGNORED (locked recipe): always 6.0.
    #[serde(default = "default_cfg")]
    pub cfg: f32,
    /// IGNORED (locked recipe): always 30.
    #[serde(default = "default_steps")]
    pub steps: i32,
    /// Bucket presets only (the 7 NoobAI buckets).
    #[serde(default = "default_width")]
    pub width: i32,
    #[serde(default = "default_height")]
    pub height: i32,
    /// IGNORED (locked recipe): always Euler a.
    #[serde(default = "default_sampler")]
    pub sampler: i32,
}

fn default_seed() -> i64 { -1 }
fn default_cfg() -> f32 { scene_art::PRISM_LOCKED_CFG }
fn default_steps() -> i32 { scene_art::PRISM_LOCKED_STEPS }
fn default_width() -> i32 { scene_art::PRISM_DEFAULT_WIDTH }
fn default_height() -> i32 { scene_art::PRISM_DEFAULT_HEIGHT }
fn default_sampler() -> i32 { scene_art::EULER_A_DISCRIMINANT }

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
                 width, height, sampler, model, favorite)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)",
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
        let fav_clause = if filter.favorites_only { "WHERE favorite = 1" } else { "" };
        let order = "ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?";
        let select = "SELECT id, created_at, path, prompt, negative_prompt, seed, cfg,
                             steps, width, height, sampler, model, favorite
                      FROM images";
        if filter.search.trim().is_empty() {
            let sql = format!("{select} {fav_clause} {order}");
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
                "{select} {fav_clause} \
                 {} LOWER(prompt) LIKE LOWER(?) {order}",
                if filter.favorites_only { "AND" } else { "WHERE" }
            );
            let mut stmt = conn.prepare(&sql).map_err(|e| anyhow::anyhow!("gallery list prepare: {e:?}"))?;
            let rows = stmt
                .query_map(params![needle, limit, offset], row_to_image)
                .map_err(|e| anyhow::anyhow!("gallery list query: {e:?}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("gallery list row: {e:?}"))
        }
    }

    /// Fetch one image by id.
    pub fn get(&self, id: i64) -> anyhow::Result<Option<GalleryImage>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, path, prompt, negative_prompt, seed, cfg,
                        steps, width, height, sampler, model, favorite
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

    /// PERMANENT delete (Chloe ruling, 2026-08-18): there is NO soft delete /
    /// trash for gallery images — one click on Delete and the image is gone,
    /// period. Remove the row + return its path so the caller can unlink the
    /// PNG file. The file unlink is the CALLER's job (lib.rs IPC) so this
    /// method stays pure-SQL + testable. Returns Ok(None) if the row didn't
    /// exist (idempotent).
    pub fn delete(&self, id: i64) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("gallery mutex: {e}"))?;
        let path: Option<String> = conn
            .query_row("SELECT path FROM images WHERE id = ?", params![id], |r| r.get(0))
            .ok();
        if path.is_some() {
            conn.execute("DELETE FROM images WHERE id = ?", params![id])
                .map_err(|e| anyhow::anyhow!("gallery delete: {e:?}"))?;
        }
        Ok(path)
    }
}

/// The insert payload (the fields the generation entrypoint fills before
/// `GalleryDb::insert`). Sibling of `GalleryImage` minus the id/favorite
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
    })
}

/// Idempotent schema init (mirrors `memory::init_schema`). `CREATE TABLE IF
/// NOT EXISTS` makes fresh-create + reopen the same code path; no separate
/// migration runner. Additive only (a future column is an `ALTER TABLE ADD
/// COLUMN` guarded by a PRAGMA check, never a destructive change — gallery
/// rows are user data).
///
/// (2026-08-18) The `trashed` column is REMOVED from the fresh schema —
/// delete is permanent (no trash system). Pre-0.22.0 dev databases keep
/// their legacy `trashed` column on disk (harmless: every SELECT names its
/// columns, so an extra dead column never round-trips); only rows still
/// marked trashed there are strays from the deleted soft-delete era.
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
            favorite         INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_images_created_at ON images (created_at DESC);
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
///
/// (#72) Same-ms double-fire with a locked seed used to produce ONE PNG +
/// TWO gallery rows (the second render overwrote the first). When the exact
/// path already exists, a `-r2`, `-r3`, ... suffix is appended until a free
/// name is found — each row keeps its own file.
pub fn dest_path(gallery_dir: &Path, seed: i64, created_at_ms: i64) -> PathBuf {
    let base = gallery_dir.join(format!("{created_at_ms}-{seed}.png"));
    if !base.exists() {
        return base;
    }
    for retry in 2.. {
        let candidate = gallery_dir.join(format!("{created_at_ms}-{seed}-r{retry}.png"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u64 retry counter cannot exhaust the filesystem namespace")
}

// ────────────────────────────────────────────────────────────────────────────
// Subject classification — the crowd-logic tag gate (Chloe ruling, 2026-08-17)
// ────────────────────────────────────────────────────────────────────────────
// The composer's vocabulary is full danbooru, so subject-count tags are the
// reliable signal for how many figures the frame holds. The gate injects ONE
// derived tag into every real prompt (engine machinery, same invisibility
// rules as the quality prefix — never rendered, never stored in gallery
// rows):
//
// | Prompt                                  | Injected    | Why                          |
// |-----------------------------------------|-------------|------------------------------|
// | single subject tag (1girl/1boy/1other), | `solo`      | isolates the subject; locks  |
// | no crowd tags                           |             | down extra-limb/clone drift  |
// | crowd/multi tags (2boys, crowd, group…) | — (nothing) | secondary figures render     |
// |                                         |             | cleanly, no tag fighting     |
// | no subject tags at all (scenery mode)   | `no humans` | drop all character-render    |
// |                                         |             | logic; 100% environment      |

/// Injected when exactly ONE subject is named.
const SUBJECT_TAG_SOLO: &str = "solo";
/// Injected when NO subject is named (scenery mode).
const SUBJECT_TAG_NO_HUMANS: &str = "no humans";

/// Exactly-one-subject count tags (danbooru canonical). TWO DISTINCT singles
/// in one prompt (e.g. `1girl, 1boy`) is a two-subject scene → Multiple.
const SINGLE_SUBJECT_TAGS: [&str; 3] = ["1boy", "1girl", "1other"];

/// Many-subject tags: numeric counts ≥2, the `multiple *` family, and the
/// crowd/group family. Any one of these beats single tags — `1girl, crowd`
/// is a crowd scene with a foreground girl, not a solo.
const MULTI_SUBJECT_TAGS: &[&str] = &[
    "2boys", "2girls", "2others",
    "3boys", "3girls", "3others",
    "4boys", "4girls", "4others",
    "5boys", "5girls", "5others",
    "6boys", "6girls", "6others",
    "6+boys", "6+girls", "6+others",
    "multiple boys", "multiple girls", "multiple others", "multiple people",
    "crowd", "group", "couple", "twins",
];

/// The crowd-logic classification of a comma-separated tag prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectClass {
    /// Exactly one subject named → inject `solo`.
    Single,
    /// Two or more subjects named → inject nothing.
    Multiple,
    /// No subjects named → inject `no humans`.
    Scenery,
}

/// Normalize one comma-separated term for subject-tag comparison: trimmed,
/// lowercased, danbooru underscores folded to spaces. The composer emits
/// space-form tags, but hand-edited / forked / legacy payloads may carry the
/// underscore form (`multiple_girls`, `no_humans`) — both spellings of every
/// tag in the sets must match.
fn normalize_subject_tag(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace('_', " ")
}

/// Classify a tag prompt by its subject-count tags. Pure + order-independent
/// (flags are collected, then applied by precedence): an explicit
/// `no humans` wins over everything (the authored scenery declaration must
/// never fight an injected `solo`), crowd/multi tags beat singles, and an
/// absent-everything prompt is scenery.
pub fn classify_subject(prompt: &str) -> SubjectClass {
    let mut has_no_humans = false;
    let mut has_multi = false;
    let mut singles = 0usize;
    for raw in prompt.split(',') {
        let tag = normalize_subject_tag(raw);
        if tag == SUBJECT_TAG_NO_HUMANS {
            has_no_humans = true;
        } else if MULTI_SUBJECT_TAGS.contains(&tag.as_str()) {
            has_multi = true;
        } else if SINGLE_SUBJECT_TAGS.contains(&tag.as_str()) {
            singles += 1;
        }
    }
    if has_no_humans {
        SubjectClass::Scenery
    } else if has_multi || singles >= 2 {
        SubjectClass::Multiple
    } else if singles == 1 {
        SubjectClass::Single
    } else {
        SubjectClass::Scenery
    }
}

/// Apply the crowd-logic gate to a NON-EMPTY user prompt: append the
/// classified tag unless the user already authored it (a typed `solo` or
/// `no humans` is respected verbatim — never duplicated, never stripped).
fn inject_subject_tag(user_prompt: &str) -> String {
    let tag = match classify_subject(user_prompt) {
        SubjectClass::Single => SUBJECT_TAG_SOLO,
        SubjectClass::Multiple => return user_prompt.to_string(),
        SubjectClass::Scenery => SUBJECT_TAG_NO_HUMANS,
    };
    let already = user_prompt
        .split(',')
        .any(|t| normalize_subject_tag(t) == tag);
    if already {
        user_prompt.to_string()
    } else {
        format!("{user_prompt}, {tag}")
    }
}

// ── The SFW-default rating steering (Chloe ruling, 2026-08-17) ──────────
//
// The locked recipe carries no rating axis, so NoobAI falls back to its raw
// Danbooru prior — which skews suggestive/NSFW for perfectly innocent
// prompts like "1girl, classroom, school uniform". The default is
// therefore SFW steering in Danbooru's own rating vocabulary: `safe` rides
// the positive prefix, `nsfw` rides the negative block. The steering is
// DROPPED the moment the user prompt carries an explicit/questionable tag —
// people who want NSFW search with NSFW tags and the tags do the work;
// nothing is force-inverted and nobody fights an injected censor.

/// The SFW positive steering tag (appended to the quality prefix).
const RATING_TAG_SFW: &str = "safe";
/// The SFW negative steering tag (appended to the negative block).
const RATING_TAG_NSFW: &str = "nsfw";

/// Explicit/questionable markers — any ONE in the user prompt drops the SFW
/// steering. Chloe-tunable: this is the opt-out flip condition, not a censor
/// list. Two layers:
///
/// 1. this list — WHOLE-TAG equality (`classic` must never substring-match
///    `ass`; `pantyhose` must never match `panties`);
/// 2. [`EXPLICIT_PROMPT_ROOTS`] — substring roots for the danbooru families
///    with too many variants to enumerate (`nude cover`, `pussy juice`,
///    `puffy nipples`…).
const EXPLICIT_PROMPT_TAGS: &[&str] = &[
    // rating axis / meta words
    "nsfw", "explicit", "questionable", "sensitive", "ecchi", "hentai",
    "18+", "r18", "rating:explicit", "rating:questionable", "rating:sensitive",
    // orientation / h-families
    "yaoi", "yuri", "futanari", "trap", "otokonoko",
    // sex acts / aftermath
    "sex", "sexy", "vaginal", "anal", "oral", "group sex", "sex from behind",
    "imminent sex", "after sex", "cum", "cumshot", "creampie", "ahegao",
    "erection", "dildo", "vibrator", "sex toy", "bondage", "bdsm", "groping",
    "undressing", "striptease", "lactation", "deepthroat", "rimjob",
    "fingering", "spanking",
    // provocative poses / body focus
    "provocative pose", "spread legs", "bent over", "ass", "sideboob",
    "underboob", "cameltoe", "upskirt", "panty shot",
    // suggestive clothing / state
    "thong", "garter belt", "bra", "underwear", "see-through",
    "transparent clothes",
];

/// Substring roots — flip when the normalized tag CONTAINS the root. Every
/// entry is individually cleared against the SFW vocabulary: `panties` is a
/// root but `panty` is NOT (`pantyhose` is SFW); `nude`/`naked`/`lingerie`
/// are roots but `sex`/`anal`/`oral`/`cum` stay exact-only (`unisex`,
/// `canal`, `floral`, `cucumber` are real SFW tags).
const EXPLICIT_PROMPT_ROOTS: &[&str] = &[
    "nude", "naked", "topless", "bottomless",
    "nipple", "areolae", "pussy", "penis", "testicle",
    "lingerie", "panties", "cleavage",
    "blowjob", "handjob", "footjob", "cunnilingus", "paizuri",
    "masturbation", "presenting", "orgasm", "ejaculation",
];

/// True when the user prompt carries any explicit/questionable tag — the
/// SFW steering's opt-out. Underscore + case tolerant via
/// [`normalize_subject_tag`] (hand-edited / forked payloads).
fn prompt_wants_explicit(user_prompt: &str) -> bool {
    user_prompt.split(',').any(|raw| {
        let tag = normalize_subject_tag(raw);
        if EXPLICIT_PROMPT_TAGS.contains(&tag.as_str()) {
            return true;
        }
        EXPLICIT_PROMPT_ROOTS.iter().any(|root| tag.contains(*root))
    })
}

/// Convert user-supplied [`GenerateParams`] into the internal
/// [`SceneImageRequest`] that the shared swap pipeline consumes. Pure (no I/O);
/// pulled out so the params→request mapping is unit-testable without the SD
/// backend. `model_path` is the resolved SD checkpoint; `dest` is the output
/// PNG path (from [`dest_path`]).
///
/// **THE LOCKED-RECIPE CHOKEPOINT (Chloe ruling, 2026-08-17):** the sampler
/// (Euler a), schedule (discrete — applied engine-side in
/// `DiffusionRsGenerator::generate`), steps (30), CFG (6.0), the NoobAI
/// v1.1 quality prefix, and the negative block are LOCKED — no matter
/// what the frontend sends, this fn normalizes the request onto the recipe:
///
/// - `steps`/`cfg`/`sampler` from `params` are IGNORED (the IPC fields stay
///   deserializable for stale frontends + old Fork payloads; they just no
///   longer do anything).
/// - The prompt gets [`scene_art::PRISM_QUALITY_PREFIX`] prepended (skipped
///   only when the prompt already starts with it — the legacy-row guard; the
///   live UI never produces that, but a hand-edited payload might).
/// - The subject-tag gate ([`classify_subject`]) appends `solo` / `no humans`
///   per the crowd-logic table to every NON-EMPTY prompt — engine-injected
///   like the prefix, never stored in gallery rows, never duplicated against
///   an authored tag. An EMPTY prompt stays prefix-alone (the generic
///   top-cluster fallback is not "scenery mode").
/// - SFW rating steering (2026-08-17): `safe` rides the positive prompt +
///   `nsfw` rides the negative block by DEFAULT. A prompt carrying any
///   [`EXPLICIT_PROMPT_TAGS`] marker drops BOTH (an authored `safe` tag is
///   honored verbatim, never duplicated).
/// - `negative_prompt` from `params` is IGNORED — the request always carries
///   [`scene_art::PRISM_NEGATIVE_BLOCK`] (+ `nsfw` unless explicit).
/// - `width`/`height`/`seed` pass through (clamped as before): size is the
///   composer's bucket presets, seed is Fork & Edit's primitive.
pub fn build_request(
    p: &GenerateParams,
    model_path: PathBuf,
    dest: PathBuf,
) -> SceneImageRequest {
    let trimmed = p.prompt.trim();
    // The crowd-logic gate applies to real prompts only — an empty prompt
    // stays the prefix-alone fallback (generic top-cluster art; classifying
    // "nothing chosen" as scenery would silently flip that fallback to
    // environment-only art).
    let user_prompt = if trimmed.is_empty() {
        String::new()
    } else {
        inject_subject_tag(trimmed)
    };
    // SFW steering unless the user opted out with an explicit tag. A
    // hand-edited payload that already authors `safe` is honored as-is.
    let explicit = !user_prompt.is_empty() && prompt_wants_explicit(&user_prompt);
    let has_own_safe = user_prompt
        .split(',')
        .any(|t| normalize_subject_tag(t) == RATING_TAG_SFW);
    let steered = !explicit && !has_own_safe;
    let prompt = if user_prompt.is_empty() {
        // No user tags → the prefix alone (generic top-cluster art) + the
        // SFW steer. The UI's guard asks for ≥1 tag, but the IPC is not
        // trusted to enforce that.
        format!("{}, {}", scene_art::PRISM_QUALITY_PREFIX, RATING_TAG_SFW)
    } else if user_prompt.starts_with(scene_art::PRISM_QUALITY_PREFIX) {
        // Legacy/duplicate guard: already prefixed — never double-prefix.
        if steered {
            format!("{user_prompt}, {}", RATING_TAG_SFW)
        } else {
            user_prompt
        }
    } else if steered {
        format!("{}, {}, {}", scene_art::PRISM_QUALITY_PREFIX, RATING_TAG_SFW, user_prompt)
    } else {
        format!("{}, {}", scene_art::PRISM_QUALITY_PREFIX, user_prompt)
    };
    let negative_prompt = if explicit {
        // Explicit mode: no `nsfw` in the negative — the user asked for it;
        // the explicit tags carry the intent.
        scene_art::PRISM_NEGATIVE_BLOCK.to_string()
    } else {
        format!("{}, {}", scene_art::PRISM_NEGATIVE_BLOCK, RATING_TAG_NSFW)
    };
    SceneImageRequest {
        prompt,
        negative_prompt: Some(negative_prompt),
        seed: p.seed,
        cfg_scale: scene_art::PRISM_LOCKED_CFG,
        steps: scene_art::PRISM_LOCKED_STEPS.max(1) as u32,
        width: p.width.max(64) as u32,
        height: p.height.max(64) as u32,
        sampling_method: scene_art::EULER_A_DISCRIMINANT,
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

    /// Unique-per-process temp-name suffix. pid + a monotonic counter — NOT
    /// `SystemTime` nanos: the Windows clock tick (~0.5-15.6 ms) is coarser
    /// than parallel test scheduling, so two nanos-named temps created within
    /// one tick collide on the SAME file (observed 2026-08-17:
    /// `favorites_only_filter` listed a third row a sibling test had inserted
    /// into its DB). The counter makes uniqueness by construction.
    fn unique_suffix() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A fresh temp gallery DB. Each test gets its own file.
    fn temp_db() -> GalleryDb {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("wupi_prism_test_{}.sqlite", unique_suffix()));
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
        let path = dir.join(format!("wupi_prism_idem_{}.sqlite", unique_suffix()));
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

    // ── favorite / delete ──────────────────────────────────────────────────

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
    fn delete_is_permanent_and_idempotent() {
        let db = temp_db();
        let a = db.insert(&sample_new_image(1)).unwrap();
        let _b = db.insert(&sample_new_image(2)).unwrap();
        // Delete returns the path (the caller unlinks the PNG) and the row is
        // GONE — no trash, no restore, no trashed flag.
        let path = db.delete(a).unwrap();
        assert_eq!(path.as_deref(), Some("/tmp/img.png"));
        assert!(db.get(a).unwrap().is_none(), "deleted row gone for good");
        let live = db.list(&GalleryFilter::default(), 10, 0).unwrap();
        assert_eq!(live.len(), 1, "the other row survives");
        // delete again is a no-op (idempotent).
        let again = db.delete(a).unwrap();
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
    fn build_request_enforces_the_locked_recipe() {
        // Whatever a stale frontend / hand-crafted IPC payload sends for the
        // locked knobs, the request carries the LOCKED recipe (Chloe ruling
        // 2026-08-17): sampler Euler a, steps 30, CFG 6.0, the NoobAI v1.1
        // quality prefix prepended, the NoobAI negative block + the SFW
        // steer injected.
        let p = GenerateParams {
            prompt: "1girl".into(),
            negative_prompt: Some("user authored negative".into()), // ignored
            seed: 12345,
            cfg: 9.5,     // ignored → 6.0
            steps: 50,    // ignored → 30
            width: 1216,
            height: 832,
            sampler: 5,   // ignored → Euler a (DPM++ 2M sent on purpose: the
                          // old locked value, proving the field is ignored)
        };
        let req = build_request(&p, PathBuf::from("/m/Image.safetensors"), PathBuf::from("/o/out.png"));
        assert_eq!(
            req.prompt,
            format!("{}, safe, 1girl, solo", scene_art::PRISM_QUALITY_PREFIX),
            "quality-meta prefix + SFW steer prepended; the crowd-logic gate injects solo (single subject)",
        );
        let expected_negative = format!("{}, nsfw", scene_art::PRISM_NEGATIVE_BLOCK);
        assert_eq!(
            req.negative_prompt.as_deref(),
            Some(expected_negative.as_str()),
            "the locked NoobAI negative block + the nsfw steer replace any user negative",
        );
        assert_eq!(req.seed, 12345, "locked seed passes through");
        assert_eq!(req.cfg_scale, 6.0, "CFG is locked at 6.0");
        assert_eq!(req.steps, 30, "steps are locked at 30");
        assert_eq!(req.sampling_method, scene_art::EULER_A_DISCRIMINANT, "sampler is locked at Euler a");
        assert_eq!(req.width, 1216);
        assert_eq!(req.height, 832);
    }

    #[test]
    fn build_request_prefix_guards_empty_and_prefixed_prompts() {
        // Empty prompt → the prefix + the SFW steer alone (the UI asks for
        // ≥1 tag; the IPC is not trusted to). An already-prefixed prompt
        // (legacy row / hand payload) is never double-prefixed.
        let empty = GenerateParams { prompt: "   ".into(), ..Default::default() };
        let req = build_request(&empty, PathBuf::new(), PathBuf::new());
        assert_eq!(
            req.prompt,
            format!("{}, safe", scene_art::PRISM_QUALITY_PREFIX),
        );

        let pre = GenerateParams {
            prompt: format!("{}, 1girl", scene_art::PRISM_QUALITY_PREFIX),
            ..Default::default()
        };
        let req = build_request(&pre, PathBuf::new(), PathBuf::new());
        // Never double-PREFIXED; the subject gate + steer still apply (single
        // subject → solo appended, SFW default → safe appended).
        assert_eq!(
            req.prompt,
            format!("{}, 1girl, solo, safe", scene_art::PRISM_QUALITY_PREFIX),
            "already-prefixed prompt is never double-prefixed",
        );
    }

    #[test]
    fn build_request_defaults_are_the_locked_recipe() {
        // An empty/minimal params object produces the LOCKED NoobAI v1.1
        // official recipe — 30 steps, CFG 6.0, the portrait bucket, Euler a,
        // prefix + SFW steer + negative block injected.
        let p = GenerateParams {
            prompt: "x".into(),
            ..Default::default()
        };
        let req = build_request(&p, PathBuf::new(), PathBuf::new());
        assert_eq!(req.seed, -1, "default random seed");
        assert_eq!(req.cfg_scale, 6.0);
        assert_eq!(req.steps, 30);
        assert_eq!(req.width, 832);
        assert_eq!(req.height, 1216);
        assert_eq!(req.sampling_method, scene_art::EULER_A_DISCRIMINANT);
        assert_eq!(
            req.prompt,
            format!("{}, safe, x, no humans", scene_art::PRISM_QUALITY_PREFIX),
            "no subject tags → scenery mode injects no humans; the SFW steer rides the front",
        );
        let expected_negative = format!("{}, nsfw", scene_art::PRISM_NEGATIVE_BLOCK);
        assert_eq!(req.negative_prompt.as_deref(), Some(expected_negative.as_str()));
    }

    /// The SFW rating steering (2026-08-17): `safe` positive + `nsfw`
    /// negative by default; one explicit/questionable marker drops BOTH;
    /// matching is underscore/case tolerant (whole-tag OR a cleared
    /// substring root — see [`EXPLICIT_PROMPT_ROOTS`]); an authored `safe`
    /// is honored verbatim.
    #[test]
    fn build_request_sfw_steering_and_the_explicit_opt_out() {
        // The SFW default — "1girl, classroom, school uniform" must not ride
        // the model's raw Danbooru prior.
        let sfw = GenerateParams {
            prompt: "1girl, classroom, school uniform".into(),
            ..Default::default()
        };
        let req = build_request(&sfw, PathBuf::new(), PathBuf::new());
        assert!(req.prompt.contains(", safe, 1girl"), "steered: {}", req.prompt);
        let expected_negative = format!("{}, nsfw", scene_art::PRISM_NEGATIVE_BLOCK);
        assert_eq!(req.negative_prompt.as_deref(), Some(expected_negative.as_str()));

        // One explicit marker drops BOTH steering tags — the explicit tags
        // carry the intent; nothing is force-inverted.
        let explicit = GenerateParams {
            prompt: "1girl, classroom, nude".into(),
            ..Default::default()
        };
        let req = build_request(&explicit, PathBuf::new(), PathBuf::new());
        assert!(!req.prompt.contains("safe"), "explicit prompt is never steered: {}", req.prompt);
        assert_eq!(
            req.negative_prompt.as_deref(),
            Some(scene_art::PRISM_NEGATIVE_BLOCK),
            "no nsfw negative against an explicit prompt",
        );

        // Underscore/case tolerance (hand-edited / forked payloads).
        let ugly = GenerateParams { prompt: "1girl, Nude_Female".into(), ..Default::default() };
        let req = build_request(&ugly, PathBuf::new(), PathBuf::new());
        assert!(!req.prompt.contains("safe"), "underscored explicit marker still flips");

        // Whole-tag equality only — `classy` must never substring-match.
        let tricky = GenerateParams { prompt: "classy, 1girl".into(), ..Default::default() };
        let req = build_request(&tricky, PathBuf::new(), PathBuf::new());
        assert!(req.prompt.contains(", safe,"), "non-marker tags stay steered: {}", req.prompt);

        // An authored `safe` is honored, never duplicated.
        let authored = GenerateParams { prompt: "safe, 1girl".into(), ..Default::default() };
        let req = build_request(&authored, PathBuf::new(), PathBuf::new());
        assert_eq!(req.prompt.matches("safe").count(), 1);
    }

    /// The marker VOCABULARY: the yaoi/yuri/pose/clothing families flip the
    /// steer; the SFW look-alike traps (`pantyhose`, `floral print`,
    /// `cucumber`, `unisex`, `canal`) never do.
    #[test]
    fn build_request_explicit_marker_vocabulary() {
        let flips = [
            // orientation / h families
            "2girls, yuri", "2boys, yaoi", "1girl, futanari",
            // clothing / state
            "1girl, lingerie", "1girl, panties", "1girl, bra, underwear",
            "1girl, topless", "1girl, bottomless",
            // poses / focus
            "1girl, spread legs", "1girl, bent over", "1girl, provocative pose",
            "1girl, upskirt", "1girl, panty shot",
            // root-covered variants
            "1girl, completely nude", "1girl, naked apron", "1girl, puffy nipples",
            "1girl, spread pussy", "1girl, side cleavage", "1girl, double blowjob",
            // rating words
            "1girl, ecchi", "1girl, 18+",
        ];
        for p in flips {
            let req = build_request(
                &GenerateParams { prompt: p.into(), ..Default::default() },
                PathBuf::new(),
                PathBuf::new(),
            );
            assert!(!req.prompt.contains("safe"), "flip case must drop the steer: {p}");
            assert_eq!(
                req.negative_prompt.as_deref(),
                Some(scene_art::PRISM_NEGATIVE_BLOCK),
                "flip case must drop the nsfw negative: {p}",
            );
        }

        // SFW traps — near-miss spellings that must STAY steered.
        let stays = [
            "1girl, pantyhose",        // `panty` is deliberately NOT a root
            "1girl, floral print",     // `oral` is exact-only
            "cucumber, no humans",     // `cum` is exact-only
            "1girl, unisex clothes",   // `sex` is exact-only
            "canal, no humans",        // `anal` is exact-only
            "1girl, classy",           // `ass` is exact-only
            "1girl, swimsuit",         // sensitive-tier clothing stays SFW-default
        ];
        for p in stays {
            let req = build_request(
                &GenerateParams { prompt: p.into(), ..Default::default() },
                PathBuf::new(),
                PathBuf::new(),
            );
            assert!(req.prompt.contains(", safe,"), "trap case must stay steered: {p}");
        }
    }

    #[test]
    fn build_request_clamps_nonpositive_dims() {
        // Defensive: a bad UI value (0 / negative) never reaches the crate as
        // 0 (which would fail or hang the render). Clamped to sane floors.
        // (Steps need no clamp anymore — the value is locked, not passed.)
        let p = GenerateParams {
            prompt: "x".into(),
            width: -100,
            height: 0,
            ..Default::default()
        };
        let req = build_request(&p, PathBuf::new(), PathBuf::new());
        assert!(req.width >= 64);
        assert!(req.height >= 64);
    }

    // ── subject classification (the crowd-logic gate) ──────────────────────

    /// The crowd-logic table, row by row: single → solo, crowd/multi →
    /// nothing, scenery → no humans. Plus the precedence edges (two singles =
    /// multi, crowd beats single, authored no-humans wins, underscore tags
    /// match the space-form sets).
    #[test]
    fn subject_classification_matches_the_crowd_logic_table() {
        // Single subject chosen (1boy / 1girl / 1other), no crowd tags.
        assert_eq!(classify_subject("1girl, long hair, sunset"), SubjectClass::Single);
        assert_eq!(classify_subject("monkey d. luffy, 1boy, straw hat"), SubjectClass::Single);
        assert_eq!(classify_subject("1other, cloak"), SubjectClass::Single);

        // Crowd / multiple people chosen.
        for multi in [
            "2boys", "2girls", "6+girls", "multiple girls", "crowd", "group",
            "couple", "twins", "multiple people",
        ] {
            assert_eq!(classify_subject(multi), SubjectClass::Multiple, "tag: {multi}");
        }
        // A crowd tag beats a single tag ("1girl, crowd" is a crowd scene
        // with a foreground girl, not a solo).
        assert_eq!(classify_subject("1girl, crowd"), SubjectClass::Multiple);
        // Two DISTINCT singles = two subjects.
        assert_eq!(classify_subject("1girl, 1boy"), SubjectClass::Multiple);

        // No characters chosen / scenery mode.
        assert_eq!(classify_subject("landscape, ocean, sunset"), SubjectClass::Scenery);
        assert_eq!(classify_subject("dungeon, torches"), SubjectClass::Scenery);
        assert_eq!(classify_subject(""), SubjectClass::Scenery);

        // An authored no-humans is the explicit scenery declaration — it wins
        // even if stray character tags remain in the prompt.
        assert_eq!(classify_subject("1girl, no humans"), SubjectClass::Scenery);

        // Underscore (danbooru canonical) spellings match the space-form sets
        // (hand-edited / forked payloads may carry either form).
        assert_eq!(classify_subject("multiple_girls"), SubjectClass::Multiple);
        assert_eq!(classify_subject("no_humans, scenery"), SubjectClass::Scenery);
        assert_eq!(classify_subject("2girls, classroom"), SubjectClass::Multiple);

        // Case/whitespace tolerance.
        assert_eq!(classify_subject("  1Girl , Crowd "), SubjectClass::Multiple);
    }

    #[test]
    fn build_request_injects_the_classified_subject_tag() {
        // Single → solo appended after the user tags.
        let single = GenerateParams {
            prompt: "monkey d. luffy, 1boy, outdoors, ocean, straw hat, sunset".into(),
            ..Default::default()
        };
        let req = build_request(&single, PathBuf::new(), PathBuf::new());
        assert_eq!(
            req.prompt,
            format!(
                "{}, safe, monkey d. luffy, 1boy, outdoors, ocean, straw hat, sunset, solo",
                scene_art::PRISM_QUALITY_PREFIX
            )
        );

        // Crowd → NOTHING injected (secondary figures render without fighting).
        let multi = GenerateParams {
            prompt: "1girl, crowd, festival".into(),
            ..Default::default()
        };
        let req = build_request(&multi, PathBuf::new(), PathBuf::new());
        assert_eq!(
            req.prompt,
            format!("{}, safe, 1girl, crowd, festival", scene_art::PRISM_QUALITY_PREFIX)
        );

        // Scenery → no humans appended.
        let scenery = GenerateParams {
            prompt: "landscape, ocean, cloudy sky".into(),
            ..Default::default()
        };
        let req = build_request(&scenery, PathBuf::new(), PathBuf::new());
        assert_eq!(
            req.prompt,
            format!("{}, safe, landscape, ocean, cloudy sky, no humans", scene_art::PRISM_QUALITY_PREFIX)
        );
    }

    #[test]
    fn build_request_never_duplicates_an_authored_subject_tag() {
        // A typed `solo` is respected verbatim — no double injection.
        let solo = GenerateParams { prompt: "1girl, solo".into(), ..Default::default() };
        let req = build_request(&solo, PathBuf::new(), PathBuf::new());
        assert_eq!(req.prompt.matches("solo").count(), 1, "no duplicate solo");

        // A typed `no humans` likewise.
        let nh = GenerateParams { prompt: "castle, no humans".into(), ..Default::default() };
        let req = build_request(&nh, PathBuf::new(), PathBuf::new());
        assert_eq!(req.prompt.matches("no humans").count(), 1, "no duplicate no humans");
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

    /// (#72) Same-ms double-fire with a locked seed: the second render must
    /// get a suffixed path instead of overwriting the first PNG.
    #[test]
    fn dest_path_bumps_suffix_on_collision() {
        let dir = std::env::temp_dir().join(format!("wupi_prism_dest_{}", unique_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dest_path(&dir, 42, 1000);
        std::fs::write(&first, b"png").unwrap();
        let second = dest_path(&dir, 42, 1000);
        assert_ne!(first, second, "collision must bump the suffix");
        assert!(
            second.to_string_lossy().ends_with("1000-42-r2.png"),
            "got {}",
            second.display()
        );
        std::fs::write(&second, b"png").unwrap();
        let third = dest_path(&dir, 42, 1000);
        assert!(
            third.to_string_lossy().ends_with("1000-42-r3.png"),
            "got {}",
            third.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn noop_backend_generation_smoke() {
        // Exercise the SceneImageGenerator contract end-to-end with the stub
        // backend (no diffusion-rs feature, no model file, no CUDA). Proves
        // the request-building → backend → dest-exists path is wired.
        use crate::scene_art::SceneImageGenerator; // trait methods (load/generate/unload)
        let gen = scene_art::NoopImageGenerator;
        let dest = std::env::temp_dir().join(format!("wupi_prism_noop_{}.png", unique_suffix()));
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
