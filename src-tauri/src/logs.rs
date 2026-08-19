//! Share-safe rotating diagnostic log — `logs/diagnostics.log` beside the exe.
//!
//! PRIVACY CONTRACT (load-bearing, 2026-08-18): this file is what users
//! attach to bug reports, so it must NEVER carry prose: no user messages,
//! no narrator beats, no card/codex bodies, no prompts, no save payloads.
//! Free text enters ONLY through [`brief`] / [`brief_with`] (whitespace-
//! collapsed, char-capped preview + source length) or as short mechanical
//! tokens (verbs, keys, slugs, ids, dice). The verbose internal tracing log
//! (`%TEMP%\wupi.log`) is a DIFFERENT file and deliberately stays out of the
//! portable `logs/` folder for exactly this reason.
//!
//! Rotation is crash-report style: the active session writes
//! `logs/diagnostics.log`; at boot (and when the active file passes
//! [`DIAG_MAX_FILE_BYTES`]) it is renamed to `diagnostics-YYYYMMDD-HHMMSS.log`
//! (local time, stamped from the file's last-write time at boot) and a fresh
//! file opens. The prune matches ONLY this module's own `diagnostics-*.log`
//! prefix — anything else a user keeps in `logs/` is untouchable.
//!
//! Writes are line-flushed (a crash must not eat the tail), mutex-guarded,
//! and failure-tolerant: if the disk refuses writes the logger goes silent
//! for the rest of the process rather than spamming or panicking — but the
//! RAM ring buffer keeps capturing. Callable from any thread (engine
//! threads, spawn_blocking closures, tokio workers).
//!
//! CRASH REPORTS: the last [`DIAG_RING_LINES`] lines also live in a
//! fixed-size `VecDeque<String>` in RAM. [`dump_crash`] — wired to the
//! global panic hook in lib.rs and the SD-abort callback — writes
//! `logs/crash-YYYYMMDD-HHMMSS.log` containing the panic message plus that
//! ring tail, so an abnormal crash can be located and read WITHOUT opening
//! the full session log. The full log survives alongside (both, just in
//! case); crash files are pruned to the newest [`DIAG_KEEP_CRASH_FILES`].
//! Note: C-level aborts (CUDA / ggml) are not Rust panics — the SD-abort
//! callback covers the image-gen path; the raw backtrace lives in
//! `%TEMP%\wupi_panic.txt` (the pre-existing hook output).
//!
//! Categories in use: SYS (boot/system) · ENG (local decode engines) ·
//! MEM (memory + embedder) · SCHEMA (delta/repair/queues) · FABLE (turn
//! flow/session ops + the per-turn world digest) · BRK (bracket commands +
//! world tracking: travel/map, weather, date, rumors, presence) · INV
//! (inventory: equip/belt/pack appliers, fragment resolution, spills,
//! soul-gem UI edits) · REF (referees/dice) · TOOL (tool calls) ·
//! API (HTTP narrator/slice/creator).

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use chrono::Local;

/// Master switch — flip to `false` to ship a build with diagnostics off.
pub const DIAG_LOG_ENABLED: bool = true;
/// Active file name inside `logs/`.
pub const DIAG_CURRENT_NAME: &str = "diagnostics.log";
/// Rotate mid-session once the active file grows past this.
const DIAG_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Timestamped files to retain (newest kept, oldest pruned).
const DIAG_KEEP_FILES: usize = 20;
/// Crash reports to retain (newest kept, oldest pruned).
const DIAG_KEEP_CRASH_FILES: usize = 10;
/// RAM ring buffer size — the crash-report tail (Chloe ruling, 2026-08-18).
const DIAG_RING_LINES: usize = 100;
/// Default preview budget for [`brief`] — CHARS, not bytes (CJK-safe).
const DIAG_SCRUB_MAX_CHARS: usize = 48;
/// Hard per-line cap — a malformed caller can never emit a prose wall.
const DIAG_LINE_MAX_CHARS: usize = 600;

struct DiagWriter {
    writer: Option<BufWriter<File>>,
    dir: PathBuf,
    written: u64,
    dead: bool,
    /// The last [`DIAG_RING_LINES`] formatted lines, RAM-only. Updated on
    /// EVERY emit — including after `dead` flips (disk refused writes) — so
    /// a crash dump still carries the tail context.
    ring: VecDeque<String>,
}

static DIAG: OnceLock<Mutex<DiagWriter>> = OnceLock::new();

/// True when the logger is armed — gate expensive call-site prep (e.g.
/// serializing a command for preview) on this.
pub fn is_on() -> bool {
    DIAG_LOG_ENABLED && DIAG.get().is_some()
}

/// Arm the logger. Call once at boot, before the Tauri builder —
/// `install_root` is the exe's parent dir (the same root `memory/` and
/// `apps/` live under). Fails soft: if `logs/` can't be created or the file
/// can't be opened, every [`log`] call is a no-op for the process.
pub fn init(install_root: &Path) {
    if !DIAG_LOG_ENABLED || DIAG.get().is_some() {
        return;
    }
    let dir = install_root.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    write_readme_if_missing(&dir);
    let current = dir.join(DIAG_CURRENT_NAME);
    if current.exists() {
        let mtime = std::fs::metadata(&current).and_then(|m| m.modified()).ok();
        rotate_closed_file(&current, mtime);
    }
    let file = match OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&current)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let writer = DiagWriter {
        writer: Some(BufWriter::new(file)),
        dir: dir.clone(),
        written: 0,
        dead: false,
        ring: VecDeque::with_capacity(DIAG_RING_LINES + 1),
    };
    if DIAG.set(Mutex::new(writer)).is_err() {
        return;
    }
    log(
        "SYS",
        &format!(
            "==== WUPI v{} session open — diagnostic log (redacted, share-safe) ====",
            env!("CARGO_PKG_VERSION")
        ),
    );
    prune_prefixed(&dir, "diagnostics-", DIAG_KEEP_FILES);
    prune_prefixed(&dir, "crash-", DIAG_KEEP_CRASH_FILES);
}

/// Emit one diagnostic line: `YYYY-MM-DD HH:MM:SS.mmm [CAT] message`.
/// No-op before [`init`], when disabled, or after a write failure.
pub fn log(cat: &str, msg: &str) {
    if !DIAG_LOG_ENABLED {
        return;
    }
    let Some(guard) = DIAG.get() else { return };
    let mut w = match guard.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    emit(&mut w, cat, msg);
}

/// Crash-report dump (Chloe ruling, 2026-08-18): write
/// `logs/crash-YYYYMMDD-HHMMSS.log` containing the panic message + the
/// RAM ring buffer's last lines, so a crash is readable without opening
/// the full session log. Mirrors a SYS line into the active log too.
/// Best-effort: failures are swallowed (`%TEMP%\wupi_panic.txt`, written
/// by lib.rs's hook, is the independent backstop with the full backtrace).
pub fn dump_crash(panic_info: &str) {
    let Some(guard) = DIAG.get() else { return };
    let mut w = match guard.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let path = unique_prefixed_path(&w.dir, "crash", &stamp);
    let mut body = format!(
        "==== WUPI v{} CRASH {} ====\npanic: {}\nfull backtrace: %TEMP%\\wupi_panic.txt\n\n---- last {} diagnostic lines (RAM ring buffer) ----\n",
        env!("CARGO_PKG_VERSION"),
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        one_line_capped(panic_info),
        w.ring.len()
    );
    for line in &w.ring {
        body.push_str(line);
        body.push('\n');
    }
    let _ = std::fs::write(&path, body);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    emit(
        &mut w,
        "SYS",
        &format!("CRASH REPORT written: {name} ({})", one_line_capped(panic_info)),
    );
}

/// Raw emit while the mutex is held — NEVER call [`log`] from inside here
/// (std Mutex is non-reentrant; this helper exists to keep rotation +
/// crash dumps safe). The ring is updated even when `dead` (disk refused
/// writes) so the crash tail survives a mid-session disk failure.
fn emit(w: &mut DiagWriter, cat: &str, msg: &str) {
    let body = format!(
        "{} [{}] {}",
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        cat,
        one_line_capped(msg)
    );
    w.ring.push_back(body);
    while w.ring.len() > DIAG_RING_LINES {
        w.ring.pop_front();
    }
    if w.dead {
        return;
    }
    let mut file_line = body;
    file_line.push('\n');
    if let Some(writer) = w.writer.as_mut() {
        if writer.write_all(file_line.as_bytes()).is_err() || writer.flush().is_err() {
            w.dead = true;
            return;
        }
    }
    w.written = w.written.saturating_add(file_line.len() as u64);
    if w.written >= DIAG_MAX_FILE_BYTES {
        rotate_active(w);
    }
}

fn rotate_active(w: &mut DiagWriter) {
    if let Some(mut old) = w.writer.take() {
        let _ = old.flush();
    }
    let current = w.dir.join(DIAG_CURRENT_NAME);
    rotate_closed_file(&current, Some(SystemTime::now()));
    w.written = 0;
    match OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&current)
    {
        Ok(f) => {
            w.writer = Some(BufWriter::new(f));
            let kb = DIAG_MAX_FILE_BYTES / 1024;
            emit(
                w,
                "SYS",
                &format!("size rotation at ~{kb} KiB — continuing in a fresh {DIAG_CURRENT_NAME}"),
            );
        }
        Err(_) => {
            w.dead = true;
        }
    }
}

fn stamp_for(mtime: Option<SystemTime>) -> String {
    match mtime {
        Some(st) => chrono::DateTime::<Local>::from(st)
            .format("%Y%m%d-%H%M%S")
            .to_string(),
        None => Local::now().format("%Y%m%d-%H%M%S").to_string(),
    }
}

fn unique_prefixed_path(dir: &Path, prefix: &str, stamp: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{prefix}-{stamp}.log"));
    let mut n = 2u32;
    while candidate.exists() {
        candidate = dir.join(format!("{prefix}-{stamp}-{n}.log"));
        n += 1;
    }
    candidate
}

fn rotate_closed_file(path: &Path, mtime: Option<SystemTime>) {
    let Some(dir) = path.parent() else { return };
    let stamp = stamp_for(mtime);
    let target = unique_prefixed_path(dir, "diagnostics", &stamp);
    let _ = std::fs::rename(path, target);
}

fn prune_prefixed(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(prefix) && n.ends_with(".log"))
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort_unstable(); // timestamped names sort chronologically
    let excess = names.len() - keep;
    for name in names.into_iter().take(excess) {
        let _ = std::fs::remove_file(dir.join(name));
    }
    log(
        "SYS",
        &format!("pruned {excess} old log(s) matching {prefix}* (keep {keep})"),
    );
}

fn write_readme_if_missing(dir: &Path) {
    let readme = dir.join("README.txt");
    if readme.exists() {
        return;
    }
    let _ = std::fs::write(&readme, README_TEXT);
}

const README_TEXT: &str = "\
WUPI diagnostic logs
====================
diagnostics.log                 - the CURRENT session
diagnostics-YYYYMMDD-HHMMSS.log - previous sessions (newest 20 kept)
crash-YYYYMMDD-HHMMSS.log       - crash reports: the panic + the last 100
                                  diagnostic lines (RAM ring buffer), newest
                                  10 kept

Safe to attach to bug reports: these logs deliberately contain no message
prose, prompts, or card/character text. Free text appears only as short
(~48 char) previews; everything else is lengths, scores (cosine / BM25 /
RRF), dice, timings, ids, and state transitions - enough to see how the
memory, tracker, and schema systems behaved without seeing what you wrote.

For a crash, attach BOTH the newest crash-*.log (instant summary) and
diagnostics.log (full session) if you can.
";

/// Redacted preview of free text: collapses whitespace, caps at `cap` CHARS
/// (CJK-safe), and appends the true length so magnitude survives redaction.
pub fn brief_with(s: &str, cap: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<&str>>().join(" ");
    let n = collapsed.chars().count();
    if n == 0 {
        return "(empty)".to_string();
    }
    if n <= cap {
        return format!("\"{collapsed}\"");
    }
    let head: String = collapsed.chars().take(cap).collect();
    format!("\"{head}\u{2026}\"(+{n}c)")
}

/// [`brief_with`] at the default [`DIAG_SCRUB_MAX_CHARS`] budget.
pub fn brief(s: &str) -> String {
    brief_with(s, DIAG_SCRUB_MAX_CHARS)
}

/// Flatten to one line (control chars -> space) and hard-cap so no caller
/// can smuggle a prose wall or forge log lines via newlines.
fn one_line_capped(msg: &str) -> String {
    let flat: String = msg
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let n = flat.chars().count();
    if n <= DIAG_LINE_MAX_CHARS {
        return flat;
    }
    let head: String = flat.chars().take(DIAG_LINE_MAX_CHARS).collect();
    format!("{head}\u{2026}(+{n}c)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_collapses_and_caps() {
        assert_eq!(brief("a"), "\"a\"");
        assert_eq!(brief("  a\n b  "), "\"a b\"");
        assert_eq!(brief(""), "(empty)");
        let long = "x".repeat(200);
        let b = brief(&long);
        assert!(b.starts_with('\"') && b.contains("(+200c)"));
        assert!(b.chars().count() < 80);
    }

    #[test]
    fn one_line_flattens_newlines() {
        assert!(!one_line_capped("a\nb\r\tc").contains('\n'));
        assert!(!one_line_capped("a\nb").contains('\r'));
    }

    #[test]
    fn disabled_log_is_noop() {
        // Never initialized in tests -> DIAG unset -> must not panic.
        if !is_on() {
            log("TEST", "hello");
        }
    }
}
