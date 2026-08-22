//! Model downloader: pulls every required model file from a private
//! Hugging Face repo so the installer can ship without the ~12 GB of
//! weights baked in. Originally the two GGUFs on first launch (v0.3.x);
//! since v0.22.0 the set also includes the four PRISM Stable Diffusion
//! files, so EXISTING installs see the boot download overlay again on the
//! 0.21 → 0.22 update to fetch them.
//!
//! ## Why this exists
//! GitHub caps single files at 100 MB (and soft-caps repos at ~1 GB), so the
//! model files can't live in the repo. Beta testers get a small installer
//! exe; when files are missing the boot overlay hands control to this
//! module, which streams them into `<install>/models/` (chat GGUFs at the
//! root, SD files under `sd/`) — every location is on
//! `model_search_dirs`'s candidate list (lib.rs), so the existing resolvers
//! pick them up on the next boot scan with ZERO resolver changes.
//!
//! ## Auth model
//! The HF repo is PRIVATE. A fine-grained read-only access token scoped to
//! ONLY `ChloeNeko/WUPI` is baked into the binary as `HF_TOKEN`. This is the
//! accepted trade-off for a private beta: zero friction for testers (no
//! token-pasting), blast radius limited to that one repo's files if the exe
//! is reverse-engineered, and the token is revocable/rotatable anytime from
//! the HF settings page. To rotate: generate a new token, replace the
//! constant, rebuild.
//!
//! ## Resume correctness (the load-bearing detail)
//! HF `/resolve/<rev>/<file>` returns a 302 to a CDN backing URL
//! (`cdn-lfs.hf.co` / `cas-bridge.xethub.hf.co`). That signed URL EXPIRES
//! (typically minutes to ~1 hour). So a naive "save the redirect URL and
//! reuse it on resume" scheme breaks the moment the URL lapses mid-download.
//! The fix: on every (re)start of a file's download, re-hit `/resolve/` with
//! the Bearer token to obtain a FRESH signed URL, then issue
//! `Range: bytes=<existing-.part-size>-` against it. The re-resolve is cheap
//! (one 302); the resume is correct indefinitely. This is the same flow
//! `hf_hub_download` uses under the hood.
//!
//! ## Atomicity
//! Each file downloads to `<name>.part` (in the file's destination dir). On
//! full completion: fsync the `.part`, then rename to the final name. A
//! crash / cancel / network drop leaves ONLY the `.part` file behind —
//! never a half-written final file — so the resolvers never see a corrupt
//! model. The `.part` is reused on the next resume attempt
//! (truncated-to-correct-offset logic below).
//!
//! ## Concurrency
//! One downloader at a time. The frontend gates the overlay on
//! `download_models` returning; there's no multi-file parallelism (the files
//! stream sequentially: WUPI first since the app can't boot without it,
//! then Embed, then the four SD files — PRISM needs all four to render).
//! Sequential is also friendlier to flaky upstream bandwidth than splitting
//! across sockets.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use serde::Serialize;
use tauri::Emitter;
use tokio::io::AsyncWriteExt;

// ── Configuration ──────────────────────────────────────────────────────────

/// The HF repo holding the GGUFs. Private; read access via `HF_TOKEN`.
const HF_REPO: &str = "ChloeNeko/WUPI";
/// HF revision (branch). `main` is where the user uploaded both files.
const HF_REVISION: &str = "main";

/// Read-only fine-grained HF token scoped to ONLY `ChloeNeko/WUPI`.
///
/// Injected at BUILD TIME from the `HF_TOKEN` environment variable via
/// `option_env!` — the token value is NEVER committed to source.
///
/// WUPI builds LOCALLY (not in CI): `llama-cpp-2` needs the CUDA Toolkit,
/// which GitHub's `windows-latest` runners lack (see docs/UPDATER_SETUP.md).
/// The release path is `npm run release` (scripts/release.cjs), which
/// forwards `HF_TOKEN` from the parent shell to the `npx tauri build` child
/// process (scripts/release.cjs + scripts/build-signed.cjs both warn loudly
/// if it's missing). So: export `HF_TOKEN` in the shell that runs the
/// release, and the compiled binary gets the real token baked in.
///
/// If `HF_TOKEN` is unset at build time, the constant compiles to `""` and
/// HF returns 401/403 to anonymous requests against the private
/// `ChloeNeko/WUPI` repo → the downloader surfaces a clear error string on
/// the first-run overlay. Local devs don't need it: they have the GGUFs on
/// disk so the overlay never fires.
///
/// Bearer auth is sent on the `/resolve/` hop ONLY — the 302 redirect's
/// signed CDN URL carries its own short-lived signature, so the token never
/// leaves HF's own domain.
///
/// Rotation: revoke at https://huggingface.co/settings/tokens, mint a new
/// fine-grained read-only token scoped to ChloeNeko/WUPI, `export HF_TOKEN=…`
/// in the shell, run `npm run release`. No source change needed.
///
/// NOTE: an earlier hardcoded version of this token (hf_GdgPcd…) was
/// committed then rewritten out of git history on 2026-07-19. That token
/// remains live until manually revoked at the HF settings page — it's
/// scoped read-only to ChloeNeko/WUPI so the realistic blast radius is
/// limited to GGUF downloads, but it should still be rotated when
/// convenient. See docs/UPDATER_SETUP.md.
const HF_TOKEN: &str = match option_env!("HF_TOKEN") {
    Some(t) => t,
    None => "",
};

/// One required file: its HF repo filename + the subdirectory under the
/// models root it lands in (`""` = the models root itself). `optional`
/// files still ride the overlay by default (fresh installs get them), but a
/// PERMANENT/exhausted download failure logs + continues instead of failing
/// the whole boot — only the render-time feature that wants the file
/// degrades (2026-08-20 audit P2-3).
pub struct RequiredFile {
    pub name: &'static str,
    pub subdir: &'static str,
    pub optional: bool,
}

/// The files we need, in download order. WUPI first because the chat engine
/// can't boot without it; Embed is best-effort (the embedder falls back to
/// StubEmbedder on miss, see lib.rs setup). The four SD files (v0.22.0) are
/// the PRISM checkpoint set: `image.gguf` (the full NoobAI-XL v1.1 Q8_0
/// checkpoint — embedded VAE + conditioners), the fp16 `clip_l`/`clip_g`
/// encoder sidecars (the provenance-correct ones EXTRACTED from the
/// checkpoint's own source, scene_art.rs ClipOverride), and `vae.safetensors`
/// (distributed for layout completeness; the single-file path deliberately
/// keeps the GGUF's embedded VAE — sd.cpp's SDXL Conv2D guard depends on it).
/// The ESRGAN file (recipe v2, 2026-08-18) is the hires-refine scaffold
/// (~18 MB; OPTIONAL — scene_art falls back to LANCZOS on a miss or a
/// corrupt/truncated file, so even a permanent fetch failure must not
/// block boot).
/// They land in `models/sd/`, exactly where `resolve_sd_model_path` looks.
pub const REQUIRED_FILES: &[RequiredFile] = &[
    RequiredFile { name: "WUPI.gguf", subdir: "", optional: false },
    RequiredFile { name: "Embed.gguf", subdir: "", optional: false },
    RequiredFile { name: "image.gguf", subdir: "sd", optional: false },
    RequiredFile { name: "vae.safetensors", subdir: "sd", optional: false },
    RequiredFile { name: "clip_l.safetensors", subdir: "sd", optional: false },
    RequiredFile { name: "clip_g.safetensors", subdir: "sd", optional: false },
    // The hires-refine ESRGAN scaffold (recipe v2, 2026-08-18 — ~18 MB, the
    // RealESRGAN_x4plus_anime_6B model under its short shipped name). A
    // missing file is NOT fatal (scene_art falls back to the LANCZOS
    // upscaler), and since 2026-08-20 neither is a failed download: it
    // rides the overlay so fresh installs get the sharper scaffold by
    // default, but `optional` keeps a dead fetch from bricking boot.
    RequiredFile { name: "esrgan.pth", subdir: "sd", optional: true },
];

/// Chunk size: reqwest's `bytes_stream()` yields its own chunks (typically
/// 8-16 KB from the TLS layer); we don't impose a fixed read size. Progress
/// granularity is therefore driven by the emit throttle below, not by chunk
/// size.

/// Throttle window for `download-progress` event emission. Emitting on every
/// TLS-sized chunk of a 5.8 GB file = ~0.5M events over a long download; that
/// floods the IPC channel and starves the UI thread. Emit at most every 500ms
/// instead — the polled `get_download_progress` (a direct IPC read) is the
/// authoritative UI source between emits.
const EMIT_INTERVAL_MS: u64 = 500;

// ── Public state (shared with AppState) ────────────────────────────────────

/// Snapshot of download progress, read by `get_download_progress` (polled by
/// the frontend) and emitted as the `download-progress` event payload.
///
/// `current_file_offset` + `current_file_total` describe the file actively
/// streaming; `overall_downloaded` + `overall_total` span every file THIS RUN
/// still has to fetch (already-present files are skipped entirely, so the
/// overall bar reflects the remaining job — on a 0.21 → 0.22 update that's
/// just the ~6 GB of SD files, not the already-present GGUFs).
#[derive(Debug, Clone, Default, Serialize)]
pub struct DownloadProgress {
    /// Which phase the downloader is in.
    pub phase: DownloadPhase,
    /// Filename currently streaming (`"WUPI.gguf"` etc.), or `""` between files.
    pub current_file: String,
    /// Bytes of `current_file` written to disk so far.
    pub current_file_offset: u64,
    /// Total bytes of `current_file` (from HF `Content-Length`). 0 until the
    /// HEAD/first-range response sets it.
    pub current_file_total: u64,
    /// Bytes downloaded across ALL files this run (for the overall bar).
    pub overall_downloaded: u64,
    /// Sum of this run's files' totals — grows as each file's size is
    /// learned on its first response (a file's total must fold in EXACTLY
    /// once; retry attempts re-resolve and must not double-count).
    pub overall_total: u64,
    /// Filenames whose totals have already been folded into `overall_total`.
    /// Bookkeeping for the once-only accounting above; never serialized.
    #[serde(skip)]
    pub counted_files: std::collections::HashSet<String>,
    /// Human-readable error if `phase == Failed`. Empty otherwise.
    pub error: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DownloadPhase {
    /// Nothing running. Initial state before `download_models` is invoked.
    #[default]
    Idle,
    /// Resolving `/resolve/` → fresh signed CDN URL for the current file.
    Resolving,
    /// Streaming bytes to `<name>.gguf.part`.
    Downloading,
    /// fsync + rename of the just-finished `.part` → final file.
    Finalizing,
    /// Both files complete and renamed; the caller may now boot normally.
    Done,
    /// Unrecoverable error. See `error`.
    Failed,
}

pub type CancelToken = Arc<AtomicBool>;

// ── URL construction ───────────────────────────────────────────────────────

/// The HF `/resolve/` URL for a file. Hits this with the Bearer header to
/// obtain a 302 → signed CDN URL; do NOT use this URL directly for the byte
/// stream (it's a redirect, not the content).
fn resolve_url(filename: &str) -> String {
    format!(
        "https://huggingface.co/{repo}/resolve/{rev}/{file}",
        repo = HF_REPO,
        rev = HF_REVISION,
        file = filename
    )
}

// ── HTTP client ────────────────────────────────────────────────────────────

/// Build a reqwest client. `rustls-tls` (already a project dep via api.rs's
/// HttpBackend) avoids any system OpenSSL on Windows. Follow redirects so the
/// 302 → CDN hop is transparent to the byte-stream loop (the Bearer header is
/// NOT forwarded to the CDN host by reqwest's redirect policy by default,
/// which is exactly what we want — the signed URL is self-authenticating).
fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::default())
        // (2026-08-20) Connect cap only — no total timeout (the GB-scale
        // weights on slow links would trip it); the per-chunk idle guard in
        // download_one's stream loop is the hang net. reqwest's default is
        // NO timeout, and this overlay runs at the boot gate.
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

// ── Core: stream one file with resume ──────────────────────────────────────

/// Download `filename` into `dest_dir/filename`, resuming from any existing
/// `<filename>.part`. Updates `progress` (under its Mutex) and emits
/// `download-progress` events on `app` at most every `EMIT_INTERVAL_MS`.
///
/// Returns the final file size on success.
async fn download_one(
    filename: &str,
    dest_dir: &Path,
    client: &reqwest::Client,
    progress: Arc<std::sync::Mutex<DownloadProgress>>,
    cancel: CancelToken,
    app: tauri::AppHandle,
) -> Result<u64, String> {
    let url = resolve_url(filename);
    let part_path: PathBuf = dest_dir.join(format!("{filename}.part"));
    let final_path: PathBuf = dest_dir.join(filename);

    // If the final file already exists, the caller (download_models) should
    // have skipped us. Defensive: if we're here anyway, treat as done.
    if final_path.exists() {
        return std::fs::metadata(&final_path)
            .map(|m| m.len())
            .map_err(|e| format!("stat existing {filename}: {e}"));
    }

    // ── Phase: Resolving ── re-hit /resolve/ for a fresh signed URL each
    // attempt. HF's signed CDN URLs expire; a saved URL from a prior run is
    // useless. The Bearer token authenticates this hop only.
    {
        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
        p.phase = DownloadPhase::Resolving;
        p.current_file = filename.to_owned();
        p.current_file_offset = 0;
        p.current_file_total = 0;
    }
    let _ = app.emit("download-progress", progress_snapshot(&progress));

    // Existing .part = the resume offset. If it's larger than the remote file
    // (somehow), truncate to 0 and start over rather than write garbage.
    let resume_offset = match std::fs::metadata(&part_path) {
        Ok(m) => m.len(),
        Err(_) => 0,
    };

    let mut req = client
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {HF_TOKEN}"));
    if resume_offset > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={resume_offset}-"));
        tracing::info!(
            file = filename,
            offset = resume_offset,
            "resuming download from .part"
        );
    }
    let response = req
        .send()
        .await
        .map_err(|e| format!("HF resolve/send for {filename} failed: {e}"))?;

    // HF returns 200 for a fresh fetch, 206 for a ranged resume. Anything else
    // is a hard error (401 = bad/expired token, 404 = wrong repo/filename).
    let status = response.status();
    if !(status == reqwest::StatusCode::OK || status == reqwest::StatusCode::PARTIAL_CONTENT) {
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "HF returned {status} for {filename} (token expired? wrong repo?): {body}"
        ));
    }
    let remote_total = response.content_length().unwrap_or(0);
    // For a 206, Content-Length is the REMAINING bytes (from offset to end);
    // for a 200 it's the full size. Compute absolute totals accordingly.
    let absolute_total = if status == reqwest::StatusCode::PARTIAL_CONTENT && remote_total > 0 {
        resume_offset + remote_total
    } else {
        remote_total
    };
    {
        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
        p.phase = DownloadPhase::Downloading;
        p.current_file_offset = resume_offset;
        p.current_file_total = absolute_total;
        // Fold this file's total into the overall total EXACTLY once — the
        // first response that reports it. Retry attempts (fresh `/resolve/`
        // per attempt) hit this same block; the counted_files guard keeps
        // the sum honest across them. (The pre-0.22.0 code seeded
        // overall_total only when it was 0, which silently dropped the
        // second file's total — invisible at 36 MB, wrong at 6 GB of SD
        // files riding a 4th bar segment.)
        if absolute_total > 0 && p.counted_files.insert(filename.to_owned()) {
            p.overall_total += absolute_total;
        }
    }

    // ── Phase: Downloading ── open the .part in append mode (or create) and
    // stream the body chunk-by-chunk. Resume offset is the file's existing
    // length, so append continues exactly where we left off.
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("open {filename}.part for write: {e}"))?;

    let mut stream = response.bytes_stream();
    let mut written_this_run: u64 = 0;
    let mut last_emit = std::time::Instant::now();

    // (2026-08-20) Idle guard: a CDN connection that accepts then goes
    // silent stalls stream.next() forever under reqwest's no-timeout
    // default — the overlay boot gate wedges with a frozen progress bar.
    // 120s without a byte = dead link; fail loudly so the retry/backoff
    // pass can fire (and the .part resumes on the next attempt).
    const CHUNK_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    loop {
        let chunk_result = match tokio::time::timeout(CHUNK_IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(r)) => r,
            Ok(None) => break, // stream complete
            Err(_) => {
                return Err(format!(
                    "download of {filename} stalled: no data for {}s (dead link?)",
                    CHUNK_IDLE_TIMEOUT.as_secs()
                ))
            }
        };
        // Cancel check at the top of each chunk (between writes, never mid):
        // mirrors the engine decode-loop cancel invariant. Relaxed is correct
        // for the same reason (single-bit signal, no dependent data; §3).
        if cancel.load(Ordering::Relaxed) {
            // Flush what we have so the .part is reusable on next attempt.
            file.flush()
                .await
                .map_err(|e| format!("flush on cancel: {e}"))?;
            return Err("cancelled".to_owned());
        }
        let chunk = chunk_result.map_err(|e| format!("stream read {filename}: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write {filename}.part: {e}"))?;
        written_this_run = written_this_run
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| "byte counter overflow".to_owned())?;

        // Update shared progress. current_file_offset is the absolute byte
        // position in the file (resume_offset + written_this_run) so the UI's
        // percentage is correct across resumes, not just within one run.
        let new_offset = resume_offset + written_this_run;
        {
            let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
            p.current_file_offset = new_offset;
            p.overall_downloaded = p.overall_downloaded.saturating_add(chunk.len() as u64);
        }
        if last_emit.elapsed() >= std::time::Duration::from_millis(EMIT_INTERVAL_MS) {
            let _ = app.emit("download-progress", progress_snapshot(&progress));
            last_emit = std::time::Instant::now();
        }
    }

    // ── Phase: Finalizing ── fsync + atomic rename. The fsync guarantees the
    // bytes hit disk before the rename (so a power loss between rename and
    // the kernel flushing pages can't leave a short final file). The rename
    // is atomic on the same filesystem (NTFS rename = single metadata op).
    {
        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
        p.phase = DownloadPhase::Finalizing;
    }
    let _ = app.emit("download-progress", progress_snapshot(&progress));

    file.sync_all()
        .await
        .map_err(|e| format!("fsync {filename}.part: {e}"))?;
    drop(file);

    let final_size = resume_offset + written_this_run;
    std::fs::rename(&part_path, &final_path)
        .map_err(|e| format!("rename {filename}.part → {filename}: {e}"))?;

    tracing::info!(file = filename, bytes = final_size, "download complete");
    let _ = app.emit("download-progress", progress_snapshot(&progress));
    Ok(final_size)
}

/// Take a non-blocking snapshot of the shared progress for event emission.
fn progress_snapshot(progress: &Arc<std::sync::Mutex<DownloadProgress>>) -> DownloadProgress {
    progress.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// ── Public entry: download all required files ──────────────────────────────

/// Cap on automatic resume attempts per file within one `download_models`
/// invocation. A transient failure (network blip, idle-socket drop — which
/// correlate with the window being hidden/alt-tabbed, since the OS throttles
/// a non-foreground window's I/O and CDNs reap idle connections) used to
/// surface as a hard "Download failed", forcing the user to manually retry.
/// The loop re-hits `/resolve/` for a fresh signed URL + resumes from the
/// `.part` file each time, so a flaky few minutes self-heals instead of
/// stranding the user. Permanent errors (401/403/404 — bad token, wrong
/// repo) are NOT retried; they fail fast via `is_permanent_error`.
const MAX_RESUME_ATTEMPTS: u32 = 6;
/// Backoff base (seconds); attempt N waits `BASE * 2^(N-1)`, capped.
const RESUME_BACKOFF_BASE_SECS: u64 = 2;
const RESUME_BACKOFF_CAP_SECS: u64 = 30;

/// Classify a `download_one` error string as permanent (do NOT retry —
/// re-trying can't fix it and would only delay surfacing the real cause) vs
/// transient (retry with resume). The error strings are produced by
/// `download_one` above; we match on their stable prefixes.
///
/// Permanent:
///   - "HF returned 4xx ..." — bad/expired token (401/403), wrong repo/file
///     (404). The Bearer token authenticates the `/resolve/` hop; a 4xx here
///     is an auth/config problem, not a transient network fault.
///   - "open ... for write" / "fsync ..." / "rename ..." / "stat existing ..."
///     / "create models dir ..." — local filesystem failures (disk full,
///     permissions). Retrying won't change the disk state.
///   - "byte counter overflow" — u64 overflow; a logic bug, not transient.
///
/// Transient (the default): HF resolve/send failures, stream read errors,
/// and write errors mid-stream — all of which a fresh `/resolve/` + resume
/// from `.part` can recover from.
fn is_permanent_error(err: &str) -> bool {
    err.starts_with("HF returned 4")
        || err.starts_with("HF returned 5") // server errors may be transient, but a 5xx
        // on HF's resolve endpoint has repeatedly proven sticky within a single
        // session; treat as permanent so we surface it rather than burn 6 attempts.
        || err.contains(" for write: ")
        || err.starts_with("fsync ")
        || err.starts_with("rename ")
        || err.starts_with("stat existing ")
        || err.starts_with("create models dir ")
        || err.contains("byte counter overflow")
}

/// Download every file in `REQUIRED_FILES` into `dest_dir` (each under its
/// own `subdir`). Skips files that already exist at their final path
/// (idempotent re-runs — this is what makes the 0.21 → 0.22 update download
/// ONLY the SD files on installs that already hold the GGUFs). Updates
/// `progress` throughout; honors `cancel`. Transient mid-stream failures
/// (the kind that correlate with the window being alt-tabbed / hidden)
/// trigger an automatic resume-from-`.part` retry; permanent errors fail
/// fast.
/// (2026-08-21 Cinderfen playtest) The in-flight guard: two concurrent
/// download loops interleave appends on the same `.part` files — the
/// playtest's double-dispatch corrupted `image.gguf` (the final file GREW
/// after rename, then failed with a misleading cross-file rename error).
/// One downloader per process, ever; a second dispatch rejects cleanly.
/// Scope-based release: every exit path (Ok, Err, `?` propagation, panic
/// unwind) clears the slot.
static DOWNLOAD_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// RAII ownership of [`DOWNLOAD_IN_FLIGHT`].
struct DownloadGuard;

impl DownloadGuard {
    fn acquire() -> Result<Self, String> {
        DOWNLOAD_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                "a model download is already running in this process".to_owned()
            })?;
        Ok(DownloadGuard)
    }
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        DOWNLOAD_IN_FLIGHT.store(false, Ordering::Release);
    }
}

pub async fn download_all(
    dest_dir: PathBuf,
    progress: Arc<std::sync::Mutex<DownloadProgress>>,
    cancel: CancelToken,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let _in_flight = DownloadGuard::acquire()?;
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("create models dir {}: {e}", dest_dir.display()))?;

    let client = http_client()?;

    for rf in REQUIRED_FILES {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_owned());
        }
        // The file's destination dir (models root, or models/sd/ for the SD
        // set) — created lazily so a no-download run never materializes an
        // empty sd/ folder.
        let file_dir = if rf.subdir.is_empty() {
            dest_dir.clone()
        } else {
            let d = dest_dir.join(rf.subdir);
            std::fs::create_dir_all(&d)
                .map_err(|e| format!("create models dir {}: {e}", d.display()))?;
            d
        };
        let final_path = file_dir.join(rf.name);
        if final_path.exists() {
            tracing::info!(file = rf.name, "already present; skipping");
            continue;
        }

        // Retry-with-resume loop. Each attempt re-hits `/resolve/` for a fresh
        // signed CDN URL and resumes from the `.part` file's current length,
        // so a transient blip a few minutes into a ~6 GB pull doesn't throw
        // away the bytes already on disk. download_one is already
        // resume-correct (its resume_offset = existing .part size, append mode),
        // so a retry is literally a second `download_one` call for the same
        // file — no special per-attempt state.
        let mut last_err: Option<String> = None;
        for attempt in 1..=MAX_RESUME_ATTEMPTS {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_owned());
            }
            match download_one(
                rf.name,
                &file_dir,
                &client,
                Arc::clone(&progress),
                Arc::clone(&cancel),
                app.clone(),
            )
            .await
            {
                Ok(_size) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    // "cancelled" is user-initiated; propagate immediately
                    // without surfacing as a failure.
                    if e.eq_ignore_ascii_case("cancelled") {
                        return Err(e);
                    }
                    if is_permanent_error(&e) || attempt == MAX_RESUME_ATTEMPTS {
                        last_err = Some(e);
                        break;
                    }
                    // Transient + attempts remaining → back off + retry. Keep
                    // the phase as Downloading (don't flip to Failed) so the
                    // UI's bar holds steady; the user sees continuous progress.
                    tracing::warn!(
                        file = rf.name,
                        attempt,
                        error = %e,
                        "transient download error; will resume from .part after backoff"
                    );
                    let backoff_secs = std::cmp::min(
                        RESUME_BACKOFF_BASE_SECS.saturating_mul(1 << (attempt - 1)),
                        RESUME_BACKOFF_CAP_SECS,
                    );
                    // Sleep in 500ms ticks so a Cancel click is honored within
                    // half a second even mid-backoff (the cancel check at the
                    // top of the next iteration then exits). Uses
                    // tokio::time::sleep (NOT std::thread::sleep) so we don't
                    // block the tokio worker thread — this download task shares
                    // the runtime with the rest of the app.
                    let mut waited = 0u64;
                    while waited < backoff_secs * 1000 {
                        if cancel.load(Ordering::Relaxed) {
                            return Err("cancelled".to_owned());
                        }
                        let step = std::cmp::min(500, backoff_secs * 1000 - waited);
                        tokio::time::sleep(std::time::Duration::from_millis(step)).await;
                        waited += step;
                    }
                }
            }
        }
        if let Some(e) = last_err {
            // (2026-08-20 audit P2-3) An optional file's permanent failure
            // degrades its feature, never the boot: log + continue so a
            // dead ESRGAN fetch can't brick the overlay for the GGUFs and
            // the SD set that DO gate boot.
            if rf.optional {
                tracing::warn!(
                    file = rf.name,
                    error = %e,
                    "optional model file failed to download; continuing (the render-time consumer falls back)"
                );
                if let Ok(mut p) = progress.lock() {
                    p.current_file.clear();
                }
                let _ = app.emit("download-progress", progress_snapshot(&progress));
                continue;
            }
            return Err(e);
        }
    }

    {
        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
        p.phase = DownloadPhase::Done;
        p.current_file.clear();
    }
    let _ = app.emit("download-progress", progress_snapshot(&progress));
    Ok(())
}
