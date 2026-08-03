//! The `.prompt` file loader.
//!
//! `.prompt` files hold **authored prose only** — the narrator voice / job
//! instructions the AI reads. They contain NO sampler config, NO context
//! sizes, NO token rules (those live in [`crate::settings`]). This keeps the
//! mechanical numbers in one auditable Rust file and the creative prose in an
//! easily-edited text file.
//!
//! Two files today:
//! - `data/fable.prompt` → [`FablePrompts`] (two marker-delimited sections:
//!   narrator + agent — Fable runs two model passes with different samplers).
//! - `data/wupi.prompt` → [`WupiPrompts`] (single-section: the chat path is
//!   one pass, so the whole file is prose — no marker ceremony).
//!
//! ## Load model
//!
//! Mirrors `sim_card.rs`: loaded ONCE in `setup()` into a struct, cached for
//! the process lifetime. To pick up an edit, restart the app. Paths are
//! resolved by `resolve_fable_prompt_path` / `resolve_wupi_prompt_path` in
//! `lib.rs` and cached on AppState as `fable_prompts: OnceLock<FablePrompts>`
//! + `wupi_prompts: OnceLock<WupiPrompts>`.
//!
//! ## File format
//!
//! A `.prompt` file contains two sections delimited by marker lines on their
//! own line. The marker is the literal text (case-sensitive) matched as a
//! whole trimmed line (never a substring). Everything between a marker and
//! the next marker (or EOF) is that section's prose, trimmed of surrounding
//! blank lines.
//!
//! ```text
//! === NARRATOR ===
//! <narrator voice / job prose — API reads this; Local-narrator-pass reads this>
//!
//! === AGENT ===
//! <agent / tracker prose — Local-agent/tracker-pass reads this>
//! ```
//!
//! Section order is not significant; both markers must appear exactly once.
//! The file MUST begin with a marker line — any prose before the first marker
//! is rejected (a format guard so a silently-wrong file is caught loudly).
//!
//! ## Graceful degradation
//!
//! On any failure (missing file, IO error, malformed — missing/duplicate
//! section, empty section) the loader logs at `warn!` and returns
//! [`FablePrompts::fallback`]. The engine NEVER hard-fails on a prompt-file
//! problem: a missing `fable.prompt` produces a minimal built-in placeholder
//! so the simulation keeps running (mirrors `sim_card::fallback`). The
//! placeholder is deliberately terse — it is NOT authored voice, just a
//! safety net so the model gets *something* to roleplay against.

use std::path::Path;

/// The two authored prompt sections for the Fable subsystem.
///
/// - `narrator` — read by the API narrator stage (API mode) AND the Local
///   Stage-1 narrator pass (Local-only mode). Pure voice/job prose.
/// - `agent` — read by the Local Stage-2 tracker/agent pass (both modes).
///   Pure mechanical-extraction instructions; the bracket-command list stays
///   in Rust (`build_narrator_system_prompt`), NOT here.
///
/// Both fields are non-empty after construction (fallback fills them).
#[derive(Debug, Clone)]
pub struct FablePrompts {
    pub narrator: String,
    pub agent: String,
}

impl FablePrompts {
    /// The built-in safety net. Returned when the file is missing or
    /// malformed so the engine never hard-fails. Deliberately minimal — this
    /// is NOT authored voice, just a placeholder.
    fn fallback() -> Self {
        FablePrompts {
            narrator: "[narrator prompt missing or malformed — edit fable.prompt]".to_owned(),
            agent: "[agent prompt missing or malformed — edit fable.prompt]".to_owned(),
        }
    }

    /// Unit-test constructor: a trivial non-empty pair. Used by the
    /// `build_*_narrator_*` tests so they don't need the filesystem or the
    /// real `fable.prompt`. The content is deliberately marked so a test that
    /// asserts authored-prose passthrough can spot it.
    #[cfg(test)]
    pub fn test_default() -> Self {
        FablePrompts {
            narrator: "[test narrator prose]".to_owned(),
            agent: "[test agent prose]".to_owned(),
        }
    }
}

/// The marker preceding the narrator section. Matched as a whole trimmed
/// line, case-sensitive.
pub const NARRATOR_MARKER: &str = "=== NARRATOR ===";
/// The marker preceding the agent section. Matched as a whole trimmed line,
/// case-sensitive.
pub const AGENT_MARKER: &str = "=== AGENT ===";

// ─────────────────────────────────────────────────────────────────────────
// wupi.prompt — the Wupi-assistant (chat) copilot prompt.
// ─────────────────────────────────────────────────────────────────────────

/// The authored Wupi-assistant prompt (`data/wupi.prompt`).
///
/// Unlike [`FablePrompts`] (two marker-delimited sections for two model
/// passes), the chat path runs **one** pass — so `wupi.prompt` carries a
/// single prose body with no marker ceremony. The whole file (trimmed of
/// surrounding whitespace) is the copilot directive: role + capabilities +
/// workflow + output discipline. Identity/voice/personality stay in the
/// `.sim` card (`SimCard::render_for_prompt` emits `<identity>` +
/// `<appearance>`); this file holds the WHAT (what she does, how she serves
/// User), not the WHO — mirroring Fable's card-vs-prompt split.
///
/// `role` is non-empty after construction (fallback fills it). Spliced into
/// the chat system prompt at the TOP of the `sections` vec (before the card
/// persona + user profile) so the job framing leads.
#[derive(Debug, Clone)]
pub struct WupiPrompts {
    pub role: String,
}

impl WupiPrompts {
    /// The built-in safety net. Returned when the file is missing or
    /// malformed so the engine never hard-fails. Deliberately minimal — NOT
    /// authored voice, just a placeholder so the chat path gets *something*.
    fn fallback() -> Self {
        WupiPrompts {
            role: "[wupi prompt missing or malformed — edit wupi.prompt]".to_owned(),
        }
    }

    /// Unit-test constructor: a trivial non-empty body. Used by chat-prompt
    /// tests so they don't need the filesystem or the real `wupi.prompt`.
    #[cfg(test)]
    pub fn test_default() -> Self {
        WupiPrompts {
            role: "[test wupi prose]".to_owned(),
        }
    }
}

/// Load + parse `wupi.prompt`. On ANY error (missing/IO/malformed) logs a
/// `warn!` and returns [`WupiPrompts::fallback`] — never panics, never
/// propagates an error. Mirrors [`load_fable_prompts`]. The single-section
/// format is just "trim + reject empty" (no markers to parse).
pub fn load_wupi_prompts(path: &Path) -> WupiPrompts {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let role = text.trim().to_owned();
            if role.is_empty() {
                tracing::warn!(
                    path = %path.display(),
                    "wupi.prompt is empty; using built-in fallback (edit the file + restart)"
                );
                return WupiPrompts::fallback();
            }
            WupiPrompts { role }
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "wupi.prompt not found / unreadable; using built-in fallback"
            );
            WupiPrompts::fallback()
        }
    }
}

/// Load + parse `fable.prompt`. On ANY error (missing/IO/malformed) logs a
/// `warn!` and returns [`FablePrompts::fallback`] — never panics, never
/// propagates an error. Separated from [`parse`] so unit tests avoid the
/// filesystem (mirrors `sim_card::try_load` / `parse`).
pub fn load_fable_prompts(path: &Path) -> FablePrompts {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "fable.prompt malformed; using built-in fallback (edit the file + restart)"
                );
                FablePrompts::fallback()
            }
        },
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "fable.prompt not found / unreadable; using built-in fallback"
            );
            FablePrompts::fallback()
        }
    }
}

/// Which authored section is currently being collected by [`parse`]. Lives at
/// module scope so `commit` can take it by value (no borrow-across-iterations
/// issue — a `&str` here would dangle against the loop-local `line`).
#[derive(Clone, Copy)]
enum Section {
    Narrator,
    Agent,
}

/// Parse `.prompt` text into [`FablePrompts`]. Pure fn — unit-testable without
/// the filesystem. Errors on: prose before the first marker, a missing
/// section, a duplicate section, or an empty (whitespace-only) section body.
pub fn parse(text: &str) -> Result<FablePrompts, String> {
    // Walk line-by-line. `open` names the section currently being collected
    // (or None before the first marker); `narrator`/`agent` hold committed
    // bodies. On hitting a marker, flush the currently-open buffer into its
    // slot, then open the new section.
    let mut narrator: Option<String> = None;
    let mut agent: Option<String> = None;
    let mut open: Option<Section> = None;
    let mut buf = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == NARRATOR_MARKER || trimmed == AGENT_MARKER {
            // Flush whatever section was open into its slot (if any).
            commit(open.take(), &buf, &mut narrator, &mut agent)?;
            buf.clear();
            open = Some(if trimmed == NARRATOR_MARKER {
                Section::Narrator
            } else {
                Section::Agent
            });
        } else {
            if open.is_none() {
                // Prose before any marker: the file doesn't follow the format.
                return Err(format!(
                    "fable.prompt must start with a marker line ('{NARRATOR_MARKER}' or '{AGENT_MARKER}'); found prose first"
                ));
            }
            buf.push_str(line);
            buf.push('\n');
        }
    }
    // Flush the trailing section.
    commit(open, &buf, &mut narrator, &mut agent)?;

    let narrator = narrator
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("narrator section ('{NARRATOR_MARKER}') is missing or empty"))?;
    let agent = agent
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("agent section ('{AGENT_MARKER}') is missing or empty"))?;

    Ok(FablePrompts { narrator, agent })
}

/// Commit the buffered text for the section named by `which` into its slot.
/// Rejects a duplicate (slot already filled). `which == None` is a no-op (the
/// very first marker, when nothing is open yet). The body is trimmed of
/// surrounding newlines + whitespace so blank padding around a marker does
/// not produce an "empty" section that still passes the non-empty check.
fn commit(
    which: Option<Section>,
    body: &str,
    narrator: &mut Option<String>,
    agent: &mut Option<String>,
) -> Result<(), String> {
    let target: &mut Option<String> = match which {
        Some(Section::Narrator) => narrator,
        Some(Section::Agent) => agent,
        None => return Ok(()), // nothing was open
    };
    if target.is_some() {
        return Err(
            "duplicate section marker — each marker must appear exactly once".to_owned(),
        );
    }
    *target = Some(body.trim_matches('\n').trim().to_owned());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> &'static str {
        "=== NARRATOR ===\nYou are the narrator. Write vivid second-person prose.\n\n=== AGENT ===\nYou are the tracker. Emit brackets.\n"
    }

    #[test]
    fn parse_well_formed() {
        let p = parse(sample()).expect("well-formed file parses");
        assert!(p.narrator.contains("vivid second-person prose"));
        assert!(p.agent.contains("Emit brackets"));
    }

    #[test]
    fn parse_sections_in_reverse_order() {
        let text = "=== AGENT ===\nagent body\n=== NARRATOR ===\nnarrator body\n";
        let p = parse(text).expect("reverse order is allowed");
        assert_eq!(p.narrator.trim(), "narrator body");
        assert_eq!(p.agent.trim(), "agent body");
    }

    #[test]
    fn parse_rejects_prose_before_first_marker() {
        let text = "stray intro line\n=== NARRATOR ===\nx\n=== AGENT ===\ny\n";
        let err = parse(text).unwrap_err();
        assert!(err.contains("must start with a marker"), "{err}");
    }

    #[test]
    fn parse_rejects_missing_agent() {
        let text = "=== NARRATOR ===\nonly narrator here\n";
        let err = parse(text).unwrap_err();
        assert!(err.contains("agent section"), "{err}");
    }

    #[test]
    fn parse_rejects_missing_narrator() {
        let text = "=== AGENT ===\nonly agent here\n";
        let err = parse(text).unwrap_err();
        assert!(err.contains("narrator section"), "{err}");
    }

    #[test]
    fn parse_rejects_empty_section_body() {
        let text = "=== NARRATOR ===\n\n=== AGENT ===\nreal agent body\n";
        let err = parse(text).unwrap_err();
        assert!(err.contains("missing or empty"), "{err}");
    }

    #[test]
    fn parse_rejects_duplicate_marker() {
        let text =
            "=== NARRATOR ===\nfirst\n=== NARRATOR ===\nsecond\n=== AGENT ===\nagent\n";
        let err = parse(text).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn parse_marker_must_be_on_own_line_no_substring_match() {
        // A line that merely contains the marker text as part of prose must
        // NOT be treated as a marker.
        let text = "=== NARRATOR ===\nthis line === AGENT === is just prose\n=== AGENT ===\nreal agent\n";
        let p = parse(text).expect("substring-in-prose is not a marker");
        assert!(p.narrator.contains("is just prose"));
        assert_eq!(p.agent.trim(), "real agent");
    }

    #[test]
    fn fallback_is_non_empty() {
        let f = FablePrompts::fallback();
        assert!(!f.narrator.is_empty());
        assert!(!f.agent.is_empty());
    }

    #[test]
    fn load_missing_file_returns_fallback() {
        let p = load_fable_prompts(std::path::Path::new("nonexistent_fable_prompt_file.prompt"));
        assert!(p.narrator.contains("missing or malformed"));
        assert!(p.agent.contains("missing or malformed"));
    }

    // ── wupi.prompt (single-section) ──────────────────────────────────────

    #[test]
    fn wupi_load_trims_and_keeps_non_empty_body() {
        let dir = tempdir_for("wupi_load");
        let path = dir.join("wupi.prompt");
        std::fs::write(&path, "\n\n<wupi_directive>\nrole prose here\n</wupi_directive>\n\n").unwrap();
        let p = load_wupi_prompts(&path);
        assert!(p.role.starts_with("<wupi_directive>"));
        assert!(p.role.contains("role prose here"));
        assert!(p.role.ends_with("</wupi_directive>"));
        // Surrounding blank lines stripped, not the internal structure.
        assert!(!p.role.starts_with('\n'));
        assert!(!p.role.ends_with('\n'));
    }

    #[test]
    fn wupi_load_rejects_whitespace_only_file() {
        let dir = tempdir_for("wupi_ws");
        let path = dir.join("wupi.prompt");
        std::fs::write(&path, "   \n\n\t\n  ").unwrap();
        let p = load_wupi_prompts(&path);
        assert!(p.role.contains("missing or malformed"));
    }

    #[test]
    fn wupi_load_missing_file_returns_fallback() {
        let p = load_wupi_prompts(std::path::Path::new("nonexistent_wupi_prompt_file.prompt"));
        assert!(p.role.contains("missing or malformed"));
    }

    /// Tiny helper: make a unique temp dir for one test. Each test gets its
    /// own so parallel `cargo test` runs don't collide (mirrors the
    /// `tempfile` crate's behavior without pulling a dep for 3 tests).
    fn tempdir_for(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // Thread id + name + pid-ish nonce keep parallel runs isolated.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("wupi_prompt_test_{name}_{nonce}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
