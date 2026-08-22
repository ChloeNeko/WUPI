//! Crash reports + ONE post-loading session log (2026-08-20 Chloe rulings).
//!
//! History: the 2026-08-19 posture teed every formatted line into
//! `%TEMP%\wupi.log` + a per-boot `logs/wupi-YYYYMMDD-HHMMSS.log` mirror
//! (stamped at launch, one file per process — restarts and updater
//! relaunches each minted another, which read as "multiple logs for one
//! session"). 2026-08-20 morning: Chloe ruled launch-time logging OFF —
//! nothing logs (and no file exists) until the frontend confirms the first
//! real screen via the `logs_begin` IPC. 2026-08-20 evening amendment:
//! the ring-only interim build wrote NO session file at all and Chloe
//! flagged the empty `logs/` folder as a bug — so `logs_begin` now also
//! mints exactly ONE session file, `logs/wupi-YYYYMMDD-HHMMSS.log`, and
//! every formatted line tees to it (line-flushed) AND to the crash ring.
//! Boots that die before the first real screen (crash-and-restart loops,
//! updater relaunches that never reach the reveal) still mint nothing.
//! Boot retention keeps the newest [`KEEP_SESSION_FILES`] session logs.
//!
//! - The last [`RING_LINES`] verbose tracing lines live in a fixed-size RAM
//!   `VecDeque` fed by the [`SessionWriter`] tee (crash-report tail source).
//! - [`dump_crash`] — wired to the global panic hook in lib.rs and the
//!   SD-abort callback in scene_art.rs — writes
//!   `logs/crash-YYYYMMDD-HHMMSS.log` containing the panic message plus
//!   that ring tail, ONLY on an actual crash. C-level aborts (CUDA / ggml)
//!   are not Rust panics — the SD-abort callback covers the image-gen
//!   path; the raw backtrace stays in `%TEMP%\wupi_panic.txt` (the
//!   pre-existing hook output).
//!
//! Retention at boot: `crash-*.log` newest [`KEEP_CRASH_FILES`] kept,
//! `wupi-*.log` newest [`KEEP_SESSION_FILES`] kept (both minted-shape
//! matched only — see [`is_minted_log_name`]; anything else a user keeps
//! in `logs/`, including their own `wupi-notes.log`, is untouchable). The
//! retired diagnostics system's `README.txt` is removed only when its
//! content carries our generated header — a user-authored README survives.
//! No README is ever written into `logs/`.
//!
//! Fail-silent discipline throughout: every write path swallows errors (a
//! logger must never take the app down), the ring survives disk failures
//! in RAM, and everything is callable from any thread.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::Local;

/// Crash reports to retain (newest kept, oldest pruned).
const KEEP_CRASH_FILES: usize = 10;
/// Session logs (`wupi-*.log`) to retain at boot (newest kept).
const KEEP_SESSION_FILES: usize = 10;
/// RAM ring buffer size — the crash-report tail.
const RING_LINES: usize = 100;
/// Hard cap for the panic line in a crash report (one line, no prose wall).
const CRASH_LINE_MAX_CHARS: usize = 600;
/// A partial line still larger than this after a write is garbage (no
/// newline for a whole buffer) — drop it instead of growing the feed.
const RING_PARTIAL_MAX_BYTES: usize = 16 * 1024;

struct CrashLog {
    dir: PathBuf,
    /// The last [`RING_LINES`] formatted verbose lines, RAM-only, fed by
    /// [`SessionWriter`]. The single crash-report source.
    ring: VecDeque<String>,
}

static CRASH: OnceLock<Mutex<CrashLog>> = OnceLock::new();

/// Arm the crash ring + prune old files. Call once at boot, before the
/// Tauri builder — `install_root` is the exe's parent dir (the same root
/// `memory/` and `apps/` live under). Fails soft: if `logs/` can't be
/// created, the ring stays unarmed and [`dump_crash`] is a no-op.
pub fn init(install_root: &Path) {
    let dir = install_root.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if CRASH
        .set(Mutex::new(CrashLog {
            dir: dir.clone(),
            ring: VecDeque::with_capacity(RING_LINES + 1),
        }))
        .is_err()
    {
        return;
    }
    prune_minted(&dir, "crash-", KEEP_CRASH_FILES);
    // Session-log retention (2026-08-20 evening amendment): keep the newest
    // KEEP_SESSION_FILES `wupi-*.log` files — `logs_begin` mints one per
    // session. Minted-shape-matched, so user-kept files never match.
    prune_minted(&dir, "wupi-", KEEP_SESSION_FILES);
    // Remove the README.txt the retired diagnostics system used to write —
    // CONTENT-VERIFIED against our generated header so a user's own
    // README.txt survives (an unconditional delete could eat a user file).
    if let Ok(text) = std::fs::read_to_string(dir.join("README.txt")) {
        if text.trim_start().starts_with(README_MARKER) {
            let _ = std::fs::remove_file(dir.join("README.txt"));
        }
    }
}

/// First line of the retired diagnostics system's generated `logs/README.txt`
/// — the content marker distinguishing our own stale artifact from a
/// user-authored file of the same name.
const README_MARKER: &str = "WUPI diagnostic logs";

/// The session log file opened by [`begin_session_logging`] — every
/// formatted complete line is appended here (line-flushed) in addition to
/// feeding the RAM ring. `None` until logging begins, or forever when the
/// file couldn't be created (ring-only degraded mode — logging must never
/// take the app down).
static SESSION_FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

/// Mint `logs/wupi-YYYYMMDD-HHMMSS.log` (unique-suffixed on collision) for
/// this session's tee sink. Best-effort: `None` when the dir/file is
/// unwritable.
fn open_session_file() -> Option<Mutex<std::fs::File>> {
    let guard = CRASH.get()?;
    let cl = guard.lock().ok()?;
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let name = unique_prefixed_name(&cl.dir, "wupi", &stamp);
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cl.dir.join(name))
        .ok()
        .map(Mutex::new)
}

/// Append + flush complete lines to the session file, if one exists.
/// Fail-silent per line-batch (a full disk downgrades to ring-only).
fn append_session_lines(lines: &[String]) {
    use std::io::Write as _;
    let Some(Some(file)) = SESSION_FILE.get() else { return };
    let Ok(mut guard) = file.lock() else { return };
    let f: &mut std::fs::File = &mut guard;
    for line in lines {
        let _ = writeln!(f, "{line}");
    }
    // Per-batch flush: a hard kill (updater exit(0), crash) must never
    // lose the lines that were already formatted.
    let _ = f.flush();
}

/// The tracing subscriber's ONLY sink (2026-08-20): complete lines tee into
/// the crash RAM ring AND the session file. Fail-silent — a panic in here
/// while unwinding a panic would abort the process.
pub struct SessionWriter {
    /// Partial-line accumulator. One writer is built per tracing event and
    /// fmt events end with `\n`, so lines complete per event; the buffer
    /// only ever holds a transient partial.
    line: String,
}

impl SessionWriter {
    pub fn new() -> Self {
        Self {
            line: String::new(),
        }
    }
}

impl Default for SessionWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for SessionWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let lines = absorb_complete_lines(&mut self.line, buf);
        if !lines.is_empty() {
            ring_extend(&lines);
            append_session_lines(&lines);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install the global tracing subscriber — the single logging start point
/// (2026-08-20 Chloe ruling, evening amendment). NOTHING logs during early
/// boot: no subscriber exists at process start, so every tracing event
/// before this call is dropped — the LOADING OS screen, the boot updater
/// gate, and the model load all run silent (and file-less: no
/// `%TEMP%\wupi.log` is ever created). The frontend invokes the
/// `logs_begin` IPC the moment the first real screen is reached — the OS
/// home reveal (`endLoadingScreen` in script.js) for wupi.exe, the
/// Fable-entry boot teardown for fable.exe — and that lands here. At that
/// moment exactly ONE session file (`logs/wupi-*.log`) is minted and every
/// subsequent line tees to it plus the crash ring. Once-only: a second
/// call is a no-op (`.init()` panics on a second global default).
pub fn begin_session_logging() {
    static BEGUN: OnceLock<()> = OnceLock::new();
    if BEGUN.set(()).is_err() {
        return;
    }
    let _ = SESSION_FILE.set(open_session_file());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .with_target(false)
        .with_writer(|| SessionWriter::new())
        .init();
    tracing::info!("=== WUPI session logging begun (post-boot) ===");
}

/// Push complete lines into the RAM ring, evicting past [`RING_LINES`].
fn ring_extend(lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let Some(cl) = CRASH.get() else { return };
    let Ok(mut w) = cl.lock() else { return };
    for line in lines {
        w.ring.push_back(line.clone());
        while w.ring.len() > RING_LINES {
            w.ring.pop_front();
        }
    }
}

/// Split `bytes` into complete `\n`-terminated lines, buffering the
/// partial tail into `buf`. Lossy UTF-8 — a split multibyte must never
/// panic the writer.
fn absorb_complete_lines(buf: &mut String, bytes: &[u8]) -> Vec<String> {
    buf.push_str(&String::from_utf8_lossy(bytes));
    let mut out = Vec::new();
    while let Some(idx) = buf.find('\n') {
        let line: String = buf.drain(..=idx).collect();
        out.push(line.trim_end_matches(['\r', '\n']).to_string());
    }
    if buf.len() > RING_PARTIAL_MAX_BYTES {
        buf.clear();
    }
    out
}

/// Crash-report dump: write `logs/crash-YYYYMMDD-HHMMSS.log` containing
/// the panic message + the RAM ring tail (the last verbose log lines).
/// Best-effort: failures are swallowed (`%TEMP%\wupi_panic.txt`, written by
/// lib.rs's hook, is the independent backstop with the full backtrace).
/// The session file `logs/wupi-*.log` (when logging had begun) carries the
/// fuller session; this report is the crash-time excerpt.
pub fn dump_crash(panic_info: &str) {
    let Some(guard) = CRASH.get() else { return };
    let w = match guard.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let name = unique_prefixed_name(&w.dir, "crash", &stamp);
    let mut body = format!(
        "==== WUPI v{} CRASH {} ====\npanic: {}\nfull backtrace: %TEMP%\\wupi_panic.txt\nfull session (if begun): logs/wupi-*.log\n\n---- last {} verbose log lines (RAM ring buffer) ----\n",
        env!("CARGO_PKG_VERSION"),
        Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        one_line_capped(panic_info),
        w.ring.len()
    );
    for line in &w.ring {
        body.push_str(line);
        body.push('\n');
    }
    let _ = std::fs::write(w.dir.join(name), body);
}

fn unique_prefixed_name(dir: &Path, prefix: &str, stamp: &str) -> String {
    let mut candidate = format!("{prefix}-{stamp}.log");
    let mut n = 2u32;
    while dir.join(&candidate).exists() {
        candidate = format!("{prefix}-{stamp}-{n}.log");
        n += 1;
    }
    candidate
}

/// True only for a name THIS module minted: `{prefix}YYYYMMDD-HHMMSS.log`
/// or `{prefix}YYYYMMDD-HHMMSS-N.log` where `prefix` carries its own
/// trailing dash ("wupi-", "crash-"; the unique suffix comes from
/// [`unique_prefixed_name`]). The bare `wupi-*.log` glob also swept
/// user-kept files like `wupi-notes.log` — the prune must match the minted
/// shape exactly.
fn is_minted_log_name(name: &str, prefix: &str) -> bool {
    // `prefix` carries its own trailing dash ("wupi-", "crash-").
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".log") else {
        return false;
    };
    // Positional: the 15-char YYYYMMDD-HHMMSS stamp at the head, then either
    // end-of-name or "-N" (the unique suffix). A split-based parse can't
    // work — the stamp itself contains the dash.
    let b = stem.as_bytes();
    if b.len() < 15 || b[8] != b'-' {
        return false;
    }
    if !b[..8].iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if !b[9..15].iter().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let tail = &b[15..];
    tail.is_empty() || (tail[0] == b'-' && tail.len() > 1 && tail[1..].iter().all(|c| c.is_ascii_digit()))
}

fn prune_minted(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_minted_log_name(n, prefix))
        .collect();
    if names.len() <= keep {
        return;
    }
    names.sort_unstable(); // timestamped names sort chronologically
    let excess = names.len() - keep;
    for name in names.into_iter().take(excess) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Flatten to one line (control chars -> space) and hard-cap so the panic
/// header can't become a prose wall or forge extra log lines.
fn one_line_capped(msg: &str) -> String {
    let flat: String = msg
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let n = flat.chars().count();
    if n <= CRASH_LINE_MAX_CHARS {
        return flat;
    }
    let head: String = flat.chars().take(CRASH_LINE_MAX_CHARS).collect();
    format!("{head}\u{2026}(+{n}c)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absorb_splits_and_keeps_partial() {
        let mut buf = String::new();
        assert!(absorb_complete_lines(&mut buf, b"hel").is_empty());
        let lines = absorb_complete_lines(&mut buf, b"lo\r\nworld\n");
        assert_eq!(lines, vec!["hello".to_string(), "world".to_string()]);
        assert!(buf.is_empty());
    }

    #[test]
    fn absorb_is_lossy_on_split_multibyte() {
        let mut buf = String::new();
        // "é" is C3 A9 — feed the first byte alone, then the rest + newline.
        assert!(absorb_complete_lines(&mut buf, &[0xC3]).is_empty());
        let lines = absorb_complete_lines(&mut buf, &[0xA9, b'\n']);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn one_line_flattens_newlines() {
        assert!(!one_line_capped("a\nb\r\tc").contains('\n'));
        assert!(!one_line_capped("a\nb").contains('\r'));
    }

    #[test]
    fn minted_shape_filter_accepts_only_own_names() {
        for good in [
            "wupi-20260819-143005.log",
            "wupi-20260819-143005-2.log",
            "crash-20260819-143005.log",
            "crash-20260819-143005-12.log",
        ] {
            let prefix = if good.starts_with("crash-") { "crash-" } else { "wupi-" };
            assert!(is_minted_log_name(good, prefix), "should match: {good}");
        }
        for bad in [
            "wupi-notes.log",
            "wupi-.log",
            "wupi-20260819.log",
            "wupi-20260819-1430.log",
            "wupi-2026AB19-143005.log",
            "wupi-20260819-143005-.log",
            "wupi-20260819-143005-x.log",
            "crash-20260819-143005.txt",
        ] {
            assert!(!is_minted_log_name(bad, "wupi-"), "should NOT match: {bad}");
        }
        // A wupi- name checked against the crash prefix must not match either.
        assert!(!is_minted_log_name("crash-20260819-143005.log", "wupi-"));
    }

    #[test]
    fn dump_crash_writes_panic_plus_ring() {
        let dir = std::env::temp_dir().join(format!("wupi_logs_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init(&dir);
        ring_extend(&["line-1".to_string(), "line-2".to_string()]);
        dump_crash("boom: test panic\nsecond line");
        let mut found = Vec::new();
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("crash-") && name.ends_with(".log") {
                found.push((name, std::fs::read_to_string(e.path()).unwrap_or_default()));
            }
        }
        assert_eq!(found.len(), 1);
        let (_, body) = found.remove(0);
        assert!(body.contains("panic: "));
        assert!(body.contains("boom: test panic second line"));
        assert!(body.contains("line-2"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
