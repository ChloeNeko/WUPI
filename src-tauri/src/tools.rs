//! Native tool-calling for the local Gemma 4 model (Wupi-as-agent).
//!
//! Gemma 4's documented dialogue protocol (`chat_format.rs::Gemma4Format`)
//! includes tool channels:
//!
//!   `<|tool>declaration:name{...}<tool|>`         — system-turn tool declaration
//!   `<|tool_call>call:name{args}<tool_call|>`     — model requests a tool
//!   `<|tool_response>response:name{val}<tool_response|>` — result back to model
//!
//! The system-turn declaration rendering already lives in `Gemma4Format::render_prompt`
//! (`chat_format.rs:177-183`). This module implements the other half: parsing the
//! model's emitted `<|tool_call>` markers, validating + executing them against the
//! install's file tree, and producing the `<|tool_response>` payload the next
//! iteration feeds back.
//!
//! # Pipeline (per `chat_send` turn, max `MAX_TOOL_ITERATIONS` rounds)
//!
//! 1. Local model decodes with `tools` rendered in the system turn.
//! 2. `parse_tool_calls(result.raw)` extracts every `<|tool_call>` block.
//! 3. If empty → normal chat path (the assistant turn is plain prose).
//! 4. If non-empty → for each call: validate_args → execute → emit
//!    `tool_result` event → insert as a `tool_call`+`tool_response` turn pair.
//! 5. Re-decode with the extended session; loop until no more tool calls or
//!    the iteration cap is hit.
//!
//! # Why prompt + 3-pass repair, not GBNF
//!
//! `llama-cpp-2 0.1.151` gates grammar samplers behind `feature = "common"`,
//! which we don't enable (it would force a full CUDA recompile — see ZCode
//! memory `critical_build-safety-no-target-touching.md`). Worse, GBNF defeats
//! JSON *malformedness*, not *hallucination* (`schema_validator.rs:7-14`): a
//! model that emits `{path: "/wupi.exe"}` under GBNF would emit it just as
//! confidently every pass. The Rust allowlist here is the structural defense
//! GBNF structurally cannot provide. Same philosophy as the schema engine's
//! 3-pass contract (`schema_engine.rs:25-48`).

use crate::chat_format::ToolSpec;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The maximum number of tool-call ↔ decode round-trips per `chat_send` turn.
/// Past this the model is stuck in a loop; we surface the last assistant reply
/// (or a "tools exhausted" note) rather than spin forever. Matches the empirical
/// spirit of `schema_engine::MAX_DELTA_PASSES = 3` (the LLM-repair cliff).
pub const MAX_TOOL_ITERATIONS: usize = 3;

/// Cap on how many failed-tool carriers we keep for the next-turn retry prompt.
/// Mirrors `MAX_FAILED_DELTA_ATTEMPTS` (lib.rs) — FIFO eviction above this.
pub const MAX_FAILED_TOOL_ATTEMPTS: usize = 8;

// ---------------------------------------------------------------------------
// Parsing: `<|tool_call>call:name{args}<tool_call|>` → ToolCall
// ---------------------------------------------------------------------------

/// One tool invocation parsed from the model's raw output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: serde_json::Value,
}

/// Parse every `<|tool_call>call:name{args}<tool_call|>` marker out of a raw
/// model turn. Malformed markers are skipped (logged at debug). Returns an
/// empty Vec if the turn contains no tool calls (the common chat case).
///
/// The format mirrors the documented protocol token at `chat_format.rs:145`:
/// `<|tool_call>call:name{args}<tool_call|>`. We accept JSON objects (`{...}`),
/// arrays (`[...]`, treated as positional args — rare), and bare scalars
/// (`"str"`, `123`, `true`) for tools that take a single unnamed arg.
///
/// Notes:
/// - The closing marker is `<tool_call|>` (matching the `<turn|>` / `<channel|>`
///   shape Gemma uses elsewhere), NOT `<|tool_call|>`.
/// - The model sometimes emits a trailing assistant reply *after* the tool call
///   block (e.g. a short "Okay, doing that now."). We extract the tool calls
///   and let the caller decide what to do with the prose (typically: discarded
///   in favor of the tool-response-driven next turn).
pub fn parse_tool_calls(raw: &str) -> Vec<ToolCall> {
    let open = "<|tool_call>call:";
    let close = "<tool_call|>";
    let mut calls = Vec::new();
    let mut search_from = 0;
    while let Some(open_idx) = raw[search_from..].find(open) {
        let abs_open = search_from + open_idx;
        let after_open = abs_open + open.len();
        // The tool name runs up to the first `{` (JSON-args form) or the
        // closing marker (no-args / bare form). Trim whitespace around it.
        let rest = &raw[after_open..];
        let close_rel = rest.find(close).map(|i| after_open + i);
        let close_idx = match close_rel {
            Some(idx) => idx,
            None => {
                // EOF SALVAGE (Gemini Option 3): the opener exists but the
                // closer is missing — the model hit max_tokens mid-call (the
                // runaway-recursion failure mode). Instead of dropping the
                // call, slice from the opener's `{` to the absolute end of
                // the output and let json_repair + the serde salvager close
                // the dangling brackets downstream. Only salvage if there's
                // an args block to salvage (a `{` exists); a bare truncated
                // name with no args has nothing to recover.
                let args_open_rel = rest.find(|c: char| c == '{' || c == '[');
                let (name, args_span) = match args_open_rel {
                    Some(rel) => {
                        let name_end = after_open + rel;
                        (raw[after_open..name_end].trim().to_string(), &raw[name_end..])
                    }
                    None => break, // no args block → nothing to salvage
                };
                if !name.is_empty() {
                    calls.push(ToolCall {
                        name,
                        args: parse_args_lenient(args_span),
                    });
                }
                break; // consumed to EOF; no more calls possible.
            }
        };

        // Find where the args block starts: the first `{` or `[` BEFORE the
        // closer. If neither appears, the call takes no args (the entire span
        // up to the closer is the name).
        let args_open_rel = rest[..(close_idx - after_open)]
            .find(|c: char| c == '{' || c == '[')
            .map(|i| after_open + i);

        let (name, args_span) = match args_open_rel {
            Some(name_end) => {
                (raw[after_open..name_end].trim().to_string(), &raw[name_end..close_idx])
            }
            None => (raw[after_open..close_idx].trim().to_string(), ""),
        };

        if name.is_empty() {
            search_from = close_idx + close.len();
            continue;
        }
        let args = if args_span.is_empty() {
            serde_json::Value::Null
        } else {
            parse_args_lenient(args_span)
        };
        calls.push(ToolCall { name, args });
        search_from = close_idx + close.len();
    }
    calls
}

/// Parse a single tool's args span as JSON, falling back to a string carrier
/// on failure. We do NOT silently drop the call: a malformed-args tool call is
/// exactly the case the 3-pass repair loop is designed to handle, and dropping
/// it here would prevent the repair prompt from ever showing the model its
/// error.
///
/// **Two-stage parse (§11.17 fix for the local Gemma 12B unquoted-key case):**
/// the strict `serde_json::from_str` is tried first. On failure, we run the
/// span through `json_repair::repair` (the microsecond-cost syntactic pre-
/// parser that fixes unquoted keys, smart quotes, trailing commas, bare
/// newlines, bracket imbalance) and retry. This recovers `{updates:[...]}` →
/// `{"updates":[...]}` at zero LLM cost — exactly the WEAVER-Scribe failure
/// the 2026-07-29 playtest surfaced (the model emits the `updates` wrapper
/// correctly but leaves the key unquoted; strict parse rejected the whole
/// batch, silently dropping every extracted fact). Only if BOTH stages fail
/// do we carry the raw text for the repair loop.
fn parse_args_lenient(span: &str) -> serde_json::Value {
    let trimmed = span.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Null;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => v,
        Err(_) => {
            // Stage 2: syntactic repair + retry. This is the cheap win that
            // rescues the common LLM mangling (unquoted keys chief among them)
            // without burning a 5-8s model repair pass.
            let repaired = crate::json_repair::repair(trimmed);
            if repaired != trimmed {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired) {
                    return v;
                }
            }
            // Both stages failed: carry the raw text so the executor can
            // surface a helpful error to the repair loop.
            serde_json::json!({ "raw": trimmed })
        }
    }
}

// ---------------------------------------------------------------------------
// Allowlist: which paths may a file_* tool touch?
// ---------------------------------------------------------------------------

/// True iff `path` is safe for a *write* tool (file_write, file_delete,
/// create_sim_card, delete_sim_card, edit_user_profile, codex_create,
/// codex_delete) to modify. Read tools bypass this (`file_read`, `file_list`).
///
/// Designed-by-predicate rather than capability-string: the install root
/// resolves at runtime (`resolve_install_root`), so we evaluate against a
/// normalized relative path. The deny-list is hardcoded and non-overridable —
/// it pins load-bearing engine files. The allow-list is a positive permit
/// for user-data trees. Anything not on either list is denied by default
/// (default-deny is the safe choice).
///
/// Callers MUST canonicalize/sandbox the path first via `sandbox_path` so the
/// relative path can't escape via `..` traversal.
pub fn is_writable(rel: &Path) -> bool {
    // Normalize to forward-slash string for cross-platform matching.
    let rel_str = path_to_unix(rel);
    if rel_str.is_empty() {
        return false;
    }

    // --- Deny-list: load-bearing engine files (non-overridable) ---
    if is_denied(&rel_str) {
        return false;
    }

    // --- Allow-list: user-data trees ---
    is_user_data(&rel_str)
}

/// The deny-list. Anything matching returns false from `is_writable`
/// regardless of the allow-list. Order matters only for readability.
fn is_denied(rel_str: &str) -> bool {
    // Engine binaries + DLLs.
    if rel_str == "wupi.exe" || rel_str == "wupi.html" {
        return true;
    }
    if rel_str.ends_with(".dll") {
        return true;
    }
    // bin/ subdir (all runtime DLLs after the v0.3.7 move).
    if rel_str.starts_with("bin/") || rel_str == "bin" {
        return true;
    }
    // Models — multi-GB, slow to redownload, and engine-critical.
    if rel_str.starts_with("models/") || rel_str == "models" {
        return true;
    }
    // Memory DB + WAL/SHM siblings.
    if rel_str.starts_with("memory/") || rel_str == "memory" {
        return true;
    }
    // Wupi's own persona + playbook (engine content per §8C, replaced on
    // update). She authors USER codex in data/docs/, never her own docs.
    // Same carve-out for the Game Master persona + the unified Fable
    // playbook — engine content for the New Game interview flow + the
    // simulation narrator, never tool-authored. (fable.codex is the
    // 2026-07-29 unified Fable partition; was gm.codex. fable.sim is the
    // 2026-07-29 rename of gm.sim.)
    if rel_str == "data/wupi.sim"
        || rel_str == "data/wupi.codex"
        || rel_str == "data/fable.sim"
        || rel_str == "data/fable.codex"
    {
        return true;
    }
    // API config (creds — user edits via the dedicated IPC).
    if rel_str == "data/api_config.json" || rel_str == "data/theme.json" {
        return true;
    }
    // Build artifacts + VCS + deps.
    if rel_str.starts_with("target/")
        || rel_str == "target"
        || rel_str.starts_with(".git/")
        || rel_str == ".git"
        || rel_str.starts_with("node_modules/")
        || rel_str == "node_modules"
    {
        return true;
    }
    // Frontend assets are engine content (replaced on update).
    if rel_str.starts_with("assets/") || rel_str == "assets" {
        return true;
    }
    if rel_str == "paw.png" {
        return true;
    }
    false
}

/// The allow-list. User-data trees that a tool may freely modify.
fn is_user_data(rel_str: &str) -> bool {
    // The user-facing codex library.
    if rel_str.starts_with("data/docs/") || rel_str == "data/docs" {
        return true;
    }
    // Operator profile.
    if rel_str == "data/user.xml" {
        return true;
    }
    // Fable scenario cards + per-card saves/schemas/sessions.
    if rel_str.starts_with("apps/fable/cards/")
        || rel_str == "apps/fable/cards"
        || rel_str.starts_with("apps/fable/profiles/")
        || rel_str == "apps/fable/profiles"
        || rel_str.starts_with("apps/fable/saves/")
        || rel_str == "apps/fable/saves"
        || rel_str.starts_with("apps/fable/schemas/")
        || rel_str == "apps/fable/schemas"
        || rel_str.starts_with("apps/fable/sessions/")
        || rel_str == "apps/fable/sessions"
    {
        return true;
    }
    false
}

/// Sandbox an arbitrary user-supplied path against the install root:
/// - Reject absolute paths (caller must pass install-relative).
/// - Reject any `..` component (canonicalization bypass).
/// - Strip Windows drive letters + UNC prefixes if present.
/// - Normalize separators to `/`.
///
/// Returns the install-relative path on success, or `None` if the path is
/// unsafe (absolute, traverses, or empty). Mirrors the defensive normalization
/// the portable updater uses (`updater.rs::apply_extracted`'s strip_prefix).
pub fn sandbox_path(user_path: &str) -> Option<PathBuf> {
    // Strip any drive letter or UNC prefix so we end up with a relative path.
    let p = Path::new(user_path);
    // Rebuild from components, rejecting root + parent traversal.
    let mut clean = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(s) => clean.push(s),
            Component::CurDir => {} // "." is harmless
            Component::ParentDir => return None,
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if clean.as_os_str().is_empty() {
        return None;
    }
    Some(clean)
}

/// Normalize a path's separators to `/` for cross-platform allowlist matching.
fn path_to_unix(p: &Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str().map(String::from),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// Per-tool failure. Carries a model-facing one-liner so the repair loop can
/// surface *why* the call failed (mirrors `schema_validator::ValidationFailure`).
#[derive(Debug, Clone)]
pub struct ToolError {
    pub message: String,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

impl ToolError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { message: msg.into() }
    }
}

/// Runtime context handed to every tool's `execute`. Carries the install root
/// (the only path tools resolve against) and best-effort sibling dir hints so
/// tools don't have to plumb the resolvers themselves.
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub install_root: PathBuf,
    /// Per-turn Director directive slot (the `set_directive` tool's write
    /// target). Minted fresh by `chat_send` per call, drained to
    /// `AppState::pending_directive` after the agent loop returns.
    /// `Tool::execute` is sync and the slot is never held across an await, so
    /// `std::sync::Mutex` is correct here (NOT `tokio::sync`). Default `None`
    /// → test/standalone unaffected; the `set_directive` tool errors
    /// gracefully if invoked without a slot.
    pub directive_slot: Option<Arc<std::sync::Mutex<Option<String>>>>,
}

impl ToolCtx {
    pub fn new(install_root: PathBuf) -> Self {
        Self {
            install_root,
            directive_slot: None,
        }
    }
    /// Attach the per-turn directive slot (called once per chat_send from
    /// lib.rs when a Fable session is active, before the agent loop iterates).
    /// The `set_directive` tool writes here; the chat-path post-loop block
    /// drains the slot into `state.pending_directive`.
    pub fn with_directive_slot(mut self, slot: Arc<std::sync::Mutex<Option<String>>>) -> Self {
        self.directive_slot = Some(slot);
        self
    }
    /// Resolve a sandboxed relative path against the install root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, ToolError> {
        let safe = sandbox_path(rel)
            .ok_or_else(|| ToolError::new(format!("unsafe path (absolute or traverses): {rel:?}")))?;
        Ok(self.install_root.join(safe))
    }
}

/// The tool trait. Each implementation owns one tool's spec, validation, and
/// execution. Tools are pure (no AppState access) — the agent loop in lib.rs
/// owns stateful coordination.
pub trait Tool: Send + Sync {
    /// The name + description rendered into the system turn via
    /// `Gemma4Format::render_prompt` (`chat_format.rs:177-183`). The
    /// description is the model's only guidance, so it MUST be precise.
    fn spec(&self) -> ToolSpec;

    /// Cheap structural validation of args before any I/O. Returns a
    /// model-facing error string on failure (so the repair loop can fold it
    /// back into the prompt). Mirrors `schema_validator::validate`.
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError>;

    /// Execute the tool. Returns the tool's success payload (a short string —
    /// the agent loop wraps it in `<|tool_response>`). Errors carry a
    /// model-facing one-liner so the repair loop can retry with feedback.
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError>;
}

/// The full registry of tools Wupi may call. Built once at setup; cloned cheaply
/// (Arc internals where needed). v1 ships the 8 tools below.
pub fn registry() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(FileRead),
        Box::new(FileWrite),
        Box::new(FileDelete),
        Box::new(FileList),
        Box::new(CreateSimCard),
        Box::new(DeleteSimCard),
        Box::new(WireSimCard),
        Box::new(EditUserProfile),
        Box::new(CodexCreate),
        Box::new(CodexDelete),
        Box::new(CodexList),
    ]
}

/// The Fable-only tools (the "Director" suite): `generate_options` opens the
/// Crossroads option picker; `set_directive` arms a one-shot narrator steer.
/// These are attached to the chat agent loop ONLY when a Fable session is
/// active (`chat_send` gates this in lib.rs) — they're invisible to the model
/// outside a game, which prevents false-firing in plain Wupi-assistant chat.
pub fn fable_registry() -> Vec<Box<dyn Tool>> {
    vec![Box::new(GenerateOptions), Box::new(SetDirective)]
}

/// The full tool spec list, ready to hand to `Gemma4Format::render_prompt`.
pub fn specs() -> Vec<ToolSpec> {
    registry().iter().map(|t| t.spec()).collect()
}

/// The Fable-only tool spec list. `chat_send` extends `specs()` with these
/// when a Fable session is active (lib.rs).
pub fn fable_specs() -> Vec<ToolSpec> {
    fable_registry().iter().map(|t| t.spec()).collect()
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// Helper: require a string field from args, else a model-facing error.
fn req_str(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::new(format!("missing or non-string argument `{key}`")))
}

/// Helper: require a string field that's also non-empty.
fn req_str_nonempty(args: &serde_json::Value, key: &str) -> Result<String, ToolError> {
    let s = req_str(args, key)?;
    if s.trim().is_empty() {
        return Err(ToolError::new(format!("argument `{key}` must not be empty")));
    }
    Ok(s)
}

/// Sanitize a user-supplied filename into a safe stem (lowercase alphanumeric
/// + `-`/`_`, leading/trailing dashes trimmed). Returns None if the result is
/// empty. Inlined from the removed `codex` module so the card/asset tools that
/// depend on it keep working without the codex (lore RAG) feature.
fn sanitize_stem(filename: &str) -> Option<String> {
    let stem: String = filename
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_owned();
    if stem.is_empty() { None } else { Some(stem) }
}


// --- file_read -------------------------------------------------------------

struct FileRead;
impl Tool for FileRead {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file_read".into(),
            description: "Read a UTF-8 text file from the WUPI install. \
                          Argument: {\"path\": \"data/docs/notes.md\"} \
                          (install-relative, no .. or absolute paths). \
                          Returns the file contents."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let _ = req_str_nonempty(args, "path")?;
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = req_str(args, "path")?;
        let path = ctx.resolve(&rel)?;
        // Read is allowed anywhere under the install (no allowlist gate).
        std::fs::read_to_string(&path)
            .map_err(|e| ToolError::new(format!("read {}: {e}", rel)))
    }
}

// --- file_write ------------------------------------------------------------

struct FileWrite;
impl Tool for FileWrite {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file_write".into(),
            description: "Write a UTF-8 text file under the WUPI install. \
                          Arguments: {\"path\": \"data/docs/notes.md\", \"content\": \"...\"}. \
                          `path` must be inside a user-data tree (data/docs/, apps/fable/, \
                          data/user.xml). Engine files (wupi.exe, *.dll, models/, memory/, \
                          data/wupi.sim) are denied. Atomic (temp+rename)."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let path = req_str_nonempty(args, "path")?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolError::new("missing or non-string argument `content`")
        })?;
        if content.len() > 200_000 {
            return Err(ToolError::new(
                "content exceeds 200 KB cap; split into smaller writes",
            ));
        }
        let safe = sandbox_path(&path)
            .ok_or_else(|| ToolError::new(format!("unsafe path: {path:?}")))?;
        if !is_writable(&safe) {
            return Err(ToolError::new(format!(
                "path {path:?} is not in a writable user-data tree"
            )));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = req_str(args, "path")?;
        let content = req_str(args, "content")?;
        let path = ctx.resolve(&rel)?;
        let parent = path
            .parent()
            .ok_or_else(|| ToolError::new("path has no parent"))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| ToolError::new(format!("create dir: {e}")))?;
        // Atomic: write sibling temp, fsync, rename.
        let tmp = parent.join(format!(
            ".{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
        ));
        std::fs::write(&tmp, content.as_bytes())
            .map_err(|e| ToolError::new(format!("write temp: {e}")))?;
        // Best-effort fsync (Windows rename is atomic regardless).
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            if let Ok(f) = std::fs::File::open(&tmp) {
                let _ = libc::fsync(f.as_raw_fd());
            }
        }
        let _ = std::fs::rename(&tmp, &path);
        Ok(format!("wrote {} bytes to {}", content.len(), rel))
    }
}

// --- file_delete -----------------------------------------------------------

struct FileDelete;
impl Tool for FileDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file_delete".into(),
            description: "Delete a file from the WUPI install. \
                          Argument: {\"path\": \"apps/fable/cards/old.sim\"}. \
                          Same writable-tree restriction as file_write. \
                          No-op if the file doesn't exist."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let path = req_str_nonempty(args, "path")?;
        let safe = sandbox_path(&path)
            .ok_or_else(|| ToolError::new(format!("unsafe path: {path:?}")))?;
        if !is_writable(&safe) {
            return Err(ToolError::new(format!(
                "path {path:?} is not in a writable user-data tree"
            )));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = req_str(args, "path")?;
        let path = ctx.resolve(&rel)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(format!("deleted {}", rel)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(format!("already absent: {}", rel))
            }
            Err(e) => Err(ToolError::new(format!("delete {}: {e}", rel))),
        }
    }
}

// --- file_list -------------------------------------------------------------

struct FileList;
impl Tool for FileList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "file_list".into(),
            description: "List files in a directory under the WUPI install. \
                          Argument: {\"path\": \"apps/fable/cards\"}. \
                          Returns one path per line (install-relative)."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let _ = req_str_nonempty(args, "path")?;
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let rel = req_str(args, "path")?;
        let path = ctx.resolve(&rel)?;
        let entries = std::fs::read_dir(&path)
            .map_err(|e| ToolError::new(format!("read dir {}: {e}", rel)))?;
        let mut lines = Vec::new();
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let stripped = entry_path.strip_prefix(&ctx.install_root).unwrap_or(&entry_path);
            let marker = if entry_path.is_dir() { "/" } else { "" };
            lines.push(format!("{}{}", stripped.display(), marker));
        }
        lines.sort();
        if lines.is_empty() {
            Ok("(empty)".into())
        } else {
            Ok(lines.join("\n"))
        }
    }
}

// --- create_sim_card -------------------------------------------------------

struct CreateSimCard;
impl Tool for CreateSimCard {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "create_sim_card".into(),
            description: "Create or overwrite a Fable scenario card (.sim). \
                          Arguments: {\"filename\": \"my_scenario\", \"xml\": \"<sim>...</sim>\"}. \
                          Writes to apps/fable/cards/<filename>.sim. The XML must follow the \
                          .sim format (strict XML, CDATA-wrapped prose). See rusty_tavern.sim \
                          for the shape."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let xml = req_str(args, "xml")?;
        // apps/fable/cards/<stem>.sim is on the allow-list, but verify the
        // computed path itself (defensive: a filename like ../escape would
        // have been caught by sandbox_path, but be explicit).
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("apps/fable/cards/{stem}.sim");
        let safe = sandbox_path(&rel)
            .ok_or_else(|| ToolError::new(format!("unsafe card path: {rel}")))?;
        if !is_writable(&safe) {
            return Err(ToolError::new(format!(
                "card path {rel} is not writable"
            )));
        }
        // Smoke-test the XML parses (roxmltree is strict enough for this).
        roxmltree::Document::parse(&xml)
            .map_err(|e| ToolError::new(format!("XML parse error: {e}")))?;
        if xml.len() > 100_000 {
            return Err(ToolError::new("XML exceeds 100 KB cap"));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let filename = req_str(args, "filename")?;
        let xml = req_str(args, "xml")?;
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("apps/fable/cards/{stem}.sim");
        let path = ctx.resolve(&rel)?;
        let parent = path.parent().ok_or_else(|| ToolError::new("no parent"))?;
        std::fs::create_dir_all(parent).map_err(|e| ToolError::new(format!("mkdir: {e}")))?;
        std::fs::write(&path, xml.as_bytes())
            .map_err(|e| ToolError::new(format!("write card: {e}")))?;
        Ok(format!("created {rel}"))
    }
}

// --- delete_sim_card -------------------------------------------------------

struct DeleteSimCard;
impl Tool for DeleteSimCard {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delete_sim_card".into(),
            description: "Delete a Fable scenario card. \
                          Argument: {\"filename\": \"my_scenario\"}. \
                          Removes apps/fable/cards/<filename>.sim. No-op if absent."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("apps/fable/cards/{stem}.sim");
        let safe = sandbox_path(&rel)
            .ok_or_else(|| ToolError::new(format!("unsafe card path: {rel}")))?;
        if !is_writable(&safe) {
            return Err(ToolError::new(format!("card path {rel} is not writable")));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let filename = req_str(args, "filename")?;
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("apps/fable/cards/{stem}.sim");
        let path = ctx.resolve(&rel)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(format!("deleted {rel}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(format!("already absent: {rel}"))
            }
            Err(e) => Err(ToolError::new(format!("delete {rel}: {e}"))),
        }
    }
}

// --- wire_sim_card ---------------------------------------------------------
// Fable Phase 5A (2026-07-29). Authoring-time assistance: reads an existing
// .sim card, extracts its <cast> + <locations> blocks, and writes one codex
// .md entry per NPC + per location so the card is "wired" into the retrieval
// index without the user hand-authoring each lore entry. This is the user-
// owned, user-initiated version of codex maintenance (explicitly NOT runtime
// autonomy — per the §1C discipline, AI must not mutate the codex at runtime;
// this tool runs only when Wupi is asked to wire a card, and the output is
// plain .md the user can review/edit via codex list).
//
// Composition of three existing capabilities (the §11.49 Scribe precedent):
//   1. sim_card::parse_from_xml_str — read + parse the card (the .sim format).
//   2. codex::save_file — write each entry (the CodexCreate path).
//   3. retrieval re-seed — so the new entries are visible on the next
//      narrator turn (the codex_save IPC re-seeds inline).
//
// The entries it authors serve two consumers:
//   - The narrator: canonical NPC identities + location flavor (the
//     `npc.<id>.core` immutable keys the relationship engine + image-gen
//     parent chain read from).
//   - The Phase 5B image generator: location lore for the parent-chain style
//     guide (the Multihog "match the parent's palette" recipe, ported to
//     codex entries instead of a cloud image API).

struct WireSimCard;
impl Tool for WireSimCard {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "wire_sim_card".into(),
            description: "Wire an existing Fable scenario card into the codex: \
                          read its <cast> and <locations> blocks and write one \
                          codex lore entry (.md) per NPC + per location so the \
                          card is fully retrievable. Argument: \
                          {\"filename\": \"rusty_tavern\"} (the card stem, no \
                          .sim extension). Reads apps/fable/cards/<stem>.sim, \
                          writes data/docs/npc_<id>.md + data/docs/loc_<id>.md. \
                          Overwrites existing entries with the same stem. Use \
                          this when the user drops in a new card and wants it \
                          wired up, or to refresh codex entries after editing a \
                          card's cast/geography."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        // Verify the card path resolves safely + is readable (the read target).
        let card_rel = format!("apps/fable/cards/{stem}.sim");
        let card_safe = sandbox_path(&card_rel)
            .ok_or_else(|| ToolError::new(format!("unsafe card path: {card_rel}")))?;
        if !is_writable(&card_safe) {
            return Err(ToolError::new(format!("card path {card_rel} not accessible")));
        }
        // Verify the codex dir is writable (the write targets). We don't
        // pre-validate each entry path (they're derived from the card's ids
        // + sanitized); sandbox_path is checked per-write in execute.
        let docs_rel = "data/docs";
        let docs_safe = sandbox_path(docs_rel)
            .ok_or_else(|| ToolError::new(format!("unsafe codex path: {docs_rel}")))?;
        if !is_writable(&docs_safe) {
            return Err(ToolError::new(format!("codex path {docs_rel} not writable")));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let _ = (args, ctx);
        Err(ToolError::new(
            "wire_sim_card is disabled (codex lore-RAG feature removed).",
        ))
    }
}

// --- edit_user_profile -----------------------------------------------------

struct EditUserProfile;
impl Tool for EditUserProfile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_user_profile".into(),
            description: "Set the user's identity profile (data/user.xml). \
                          Arguments: {\"name\": \"Operator\", \"description\": \"...\"}. \
                          Both fields optional; pass empty string to clear. \
                          Hot-reloads on the next chat turn."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        // Both fields optional; just type-check if present.
        if let Some(n) = args.get("name") {
            if n.as_str().is_none() {
                return Err(ToolError::new("`name` must be a string"));
            }
        }
        if let Some(d) = args.get("description") {
            if d.as_str().is_none() {
                return Err(ToolError::new("`description` must be a string"));
            }
            if d.as_str().unwrap_or("").len() > 50_000 {
                return Err(ToolError::new("`description` exceeds 50 KB"));
            }
        }
        if args.get("name").is_none() && args.get("description").is_none() {
            return Err(ToolError::new("at least one of `name`/`description` required"));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let path = ctx.resolve("data/user.xml")?;
        // Load current values (hot-reload: the file may have changed since boot).
        let mut profile = crate::user_profile::load(Some(&path)).unwrap_or_default();
        if let Some(n) = args.get("name").and_then(|v| v.as_str()) {
            profile.name = n.to_string();
        }
        if let Some(d) = args.get("description").and_then(|v| v.as_str()) {
            profile.description = d.to_string();
        }
        crate::user_profile::save(&path, &profile)
            .map_err(|e| ToolError::new(format!("save profile: {e}")))?;
        Ok(format!(
            "updated user profile (name={:?})",
            profile.name
        ))
    }
}

// --- codex_create ----------------------------------------------------------

struct CodexCreate;
impl Tool for CodexCreate {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "codex_create".into(),
            description: "Create or overwrite a user codex reference entry (.md). \
                          Arguments: {\"filename\": \"lore_elves\", \"title\": \"Elves\", \
                          \"tags\": [\"fantasy\"], \"body\": \"...\"}. \
                          Writes to data/docs/<filename>.md. Used to give Wupi \
                          background knowledge she should treat as her own."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let title = req_str(args, "title")?;
        let body = req_str(args, "body")?;
        if body.len() > 50_000 {
            return Err(ToolError::new("body exceeds 50 KB"));
        }
        let _ = title; // any non-null string is fine
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("data/docs/{stem}.md");
        let safe = sandbox_path(&rel)
            .ok_or_else(|| ToolError::new(format!("unsafe codex path: {rel}")))?;
        if !is_writable(&safe) {
            return Err(ToolError::new(format!("codex path {rel} is not writable")));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let _ = (args, ctx);
        Err(ToolError::new(
            "codex_create is disabled (codex lore-RAG feature removed).",
        ))
    }
}

// --- codex_delete ----------------------------------------------------------

struct CodexDelete;
impl Tool for CodexDelete {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "codex_delete".into(),
            description: "Delete a user codex reference entry. \
                          Argument: {\"filename\": \"lore_elves\"}. \
                          Removes data/docs/<filename>.md. No-op if absent."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("data/docs/{stem}.md");
        let safe = sandbox_path(&rel)
            .ok_or_else(|| ToolError::new(format!("unsafe codex path: {rel}")))?;
        if !is_writable(&safe) {
            return Err(ToolError::new(format!("codex path {rel} is not writable")));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let _ = (args, ctx);
        Err(ToolError::new(
            "codex_delete is disabled (codex lore-RAG feature removed).",
        ))
    }
}

// --- codex_list ------------------------------------------------------------

struct CodexList;
impl Tool for CodexList {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "codex_list".into(),
            description: "List the user codex entries (data/docs/). \
                          No arguments. Returns one entry per line as \
                          `filename | title | tags`."
                .into(),
        }
    }
    fn validate_args(&self, _args: &serde_json::Value) -> Result<(), ToolError> {
        Ok(())
    }
    fn execute(&self, _args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let _ = ctx;
        Err(ToolError::new(
            "codex_list is disabled (codex lore-RAG feature removed).",
        ))
    }
}

// --- generate_options (Fable-only) ---------------------------------------

/// Open the Crossroads option picker. This is a **signal tool**: the model
/// emits the args, Rust validates, and the agent loop's `tool_call` event
/// carries them to the frontend drawer, which invokes `crossroads_generate`
/// to do the actual generation + render the modal. The tool itself produces
/// no work; it exists so the model has a structured NL→action path that
/// `validate_args` can gate (the false-tool-call guardrail).
///
/// `Tool::execute` returning a short success string means the agent loop's
/// standard `tool_result` event fires normally — the model sees "options
/// picker queued" in its `<|tool_response>` and can continue its prose turn
/// ("Alright, popping open the picker now…"). Meanwhile the frontend has
/// already received the `tool_call` event and opened the modal.
struct GenerateOptions;
impl Tool for GenerateOptions {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "generate_options".into(),
            description: "Open the Crossroads option picker — generates concrete, \
                          grounded story options the player picks from. ONLY available \
                          during an active Fable session. Natural triggers: 'what should \
                          I do next', 'give me options', 'ideas?', 'introduce an NPC', \
                          'branch the story'. \
                          \
                          Arguments: {\"lens\": \"action|plot|character|explicit|world\", \
                          \"count\": 6, \"seed\": \"optional free-text nudge\"}. \
                          All arguments optional. `lens` defaults to action. `count` \
                          defaults to 6, range 1-12 (HARD MAX). `seed` biases the \
                          generated options toward a theme. \
                          \
                          COUNT RULE (load-bearing): if the user asks for MORE than 12, \
                          DO NOT call this tool with count>12 — it will error. Instead \
                          apologize in prose ('12 is the max — is that cool?') and wait \
                          for the user's confirmation before calling with count=12. \
                          Never silently clamp.".into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        // lens: if present, must be one of the 5 valid ids. Inlined from the
        // removed `crossroads_prompt` module so validation still steers the model.
        if let Some(lens) = args.get("lens").and_then(|v| v.as_str()) {
            const VALID_LENS: &[&str] = &["action", "plot", "character", "explicit", "world"];
            if !VALID_LENS.contains(&lens) {
                return Err(ToolError::new(format!(
                    "unknown lens {lens:?}; expected one of action|plot|character|explicit|world"
                )));
            }
        }
        // count: if present, must be 1..=12. count>12 is a HARD error that steers
        // the model — the description tells it to apologize-then-ask in that case,
        // and this error message reinforces it so the repair loop's `<|tool_response>`
        // blocks a silent re-fire with the same out-of-range count.
        if let Some(n) = args.get("count").and_then(|v| v.as_u64()) {
            if n > 12 {
                return Err(ToolError::new(format!(
                    "count {n} exceeds the max of 12. Apologize to the user in prose \
                     ('12's the cap — want me to go with 12?') and wait for confirmation \
                     before calling again. Do NOT auto-clamp."
                )));
            }
            if n == 0 {
                return Err(ToolError::new(
                    "count must be at least 1. Default is 6; omit the field if unsure.",
                ));
            }
        }
        // seed: if present, ≤ 500 chars (keeps the user message cheap; the lens
        // + live scene are the main context anyway).
        if let Some(seed) = args.get("seed").and_then(|v| v.as_str()) {
            if seed.chars().count() > 500 {
                return Err(ToolError::new("seed exceeds 500 chars; trim it."));
            }
        }
        Ok(())
    }
    fn execute(&self, _args: &serde_json::Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        // Pure signal — the drawer reads the prior `tool_call` event (which
        // carries the validated args) and invokes crossroads_generate.
        Ok("options picker queued — the modal is opening.".into())
    }
}

// --- set_directive (Fable-only) ------------------------------------------

/// Arm a one-shot world directive for the NEXT narrator turn. Unlike
/// `generate_options`, this tool does real work: it writes the validated
/// directive text into `ToolCtx::directive_slot`, which `chat_send` drains to
/// `AppState::pending_directive` after the agent loop returns. `fable_send`
/// then consumes the directive at the top of its schema-lock block and
/// threads it into the narrator prompt as a `<director_directive>` block.
struct SetDirective;
impl Tool for SetDirective {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "set_directive".into(),
            description: "Steer the next narrator turn — a one-shot world directive \
                          consumed by the NEXT fable_send, then cleared. ONLY available \
                          during an active Fable session. Use when the user asks to \
                          nudge the world off-screen: 'make the barkeeper suspicious of \
                          me', 'have a storm roll in', 'advance time to morning', 'shift \
                          the tone darker'. \
                          \
                          Argument: {\"text\": \"the directive in one or two sentences\"}. \
                          After arming, confirm to the user in prose what you armed.".into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let text = req_str_nonempty(args, "text")?;
        if text.chars().count() > 1000 {
            return Err(ToolError::new("directive text exceeds 1000 chars; tighten it."));
        }
        Ok(())
    }
    fn execute(&self, args: &serde_json::Value, ctx: &ToolCtx) -> Result<String, ToolError> {
        let text = req_str(args, "text")?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(ToolError::new("directive text must not be empty"));
        }
        let Some(slot) = &ctx.directive_slot else {
            // No slot = no Fable session. Defensive: the tool spec says
            // Fable-only, but a model could still emit it outside a game.
            return Err(ToolError::new(
                "no directive slot available (not in a Fable session?)",
            ));
        };
        let mut g = slot
            .lock()
            .map_err(|_| ToolError::new("directive slot poisoned"))?;
        // Overwrite any prior directive this turn — last write wins. The slot
        // is per-turn (drained after the agent loop), so this is not a global
        // state leak.
        *g = Some(trimmed.to_string());
        Ok("directive armed — fires on the next narrator turn.".into())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    // === parse_tool_calls ===================================================

    #[test]
    fn parses_single_object_args() {
        let raw = "<|tool_call>call:file_read{\"path\":\"data/docs/x.md\"}<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[0].args["path"], "data/docs/x.md");
    }

    #[test]
    fn parses_multiple_calls() {
        let raw = "<|tool_call>call:file_read{\"path\":\"a\"}<tool_call|> \
                   <|tool_call>call:file_write{\"path\":\"b\",\"content\":\"x\"}<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_write");
    }

    #[test]
    fn parses_empty_when_no_calls() {
        assert!(parse_tool_calls("Just a normal reply.").is_empty());
        assert!(parse_tool_calls("").is_empty());
    }

    #[test]
    fn unterminated_call_with_args_now_salvaged() {
        // EOF SALVAGE (Gemini Option 3): an opener with args but no closer
        // (the model hit max_tokens mid-call) is now SALVAGED to EOF, not
        // dropped. The args span is sliced from the `{` to the end of the
        // output + handed to parse_args_lenient (→ json_repair closes any
        // dangling brackets). This was `unterminated_call_dropped` before the
        // greedy extractor; the salvage is the correct, robust behavior.
        let raw = "<|tool_call>call:file_read{\"path\":\"a\"} ... no closer";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1, "truncated opener with args is salvaged");
        assert_eq!(calls[0].name, "file_read");
    }

    #[test]
    fn unterminated_call_without_args_still_dropped() {
        // A bare truncated NAME with no `{` args block has nothing to salvage
        // → still dropped (the salvage only fires when there are args to
        // recover). This pins the conservative edge of the EOF salvage.
        let raw = "<|tool_call>call:codex_list ... no closer, no braces";
        assert!(parse_tool_calls(raw).is_empty());
    }

    #[test]
    fn malformed_json_carried_as_raw_for_repair() {
        let raw = "<|tool_call>call:file_read{path: broken}<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        // Not silently dropped: carried so the repair loop can show the model.
        // The full args span (including braces) is preserved — it shows the
        // model exactly what it emitted that failed to parse.
        assert_eq!(calls[0].args["raw"], "{path: broken}");
    }

    #[test]
    fn no_args_call_parses_with_null() {
        let raw = "<|tool_call>call:codex_list<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "codex_list");
        assert!(calls[0].args.is_null());
    }

    // --- parse_args_lenient §11.17 fix: unquoted-key repair ---

    /// The live WEAVER-Scribe failure (2026-07-29 playtest): the local Gemma
    /// 12B emits the `updates` wrapper correctly but leaves the KEY unquoted
    /// (`{updates:[...]}` not `{"updates":[...]}`). Strict serde_json rejects
    /// at the lexer. The json_repair stage recovers it — this is the test that
    /// would have caught the bug that left every WEAVER draft empty.
    #[test]
    fn unquoted_updates_key_recovered_via_json_repair() {
        // Exactly the shape observed in the live scribe dump (verbatim form).
        let raw = "<|tool_call>call:sim_draft{updates:[{\"type\":\"add_npc\",\"id\":\"mara\"}]}<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        // NOT carried as raw — repaired + parsed into the real structure.
        assert!(calls[0].args.get("raw").is_none(), "must not fall back to raw carrier");
        let updates = calls[0].args.get("updates").and_then(|v| v.as_array());
        assert!(updates.is_some(), "updates array recovered");
        assert_eq!(updates.unwrap().len(), 1);
    }

    /// Truly unrepairable JSON (a bare word value, not just an unquoted key)
    /// still carries as `raw` so the repair loop can show the model. This pins
    /// that the repair stage is conservative — it doesn't silently mangle.
    #[test]
    fn genuinely_broken_json_still_carried_as_raw() {
        let raw = "<|tool_call>call:file_read{path: broken}<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        // `path: broken` — `broken` isn't a valid JSON token → unrepairable.
        assert_eq!(calls[0].args["raw"], "{path: broken}");
    }
    // === sandbox_path =======================================================

    #[test]
    fn sandbox_accepts_relative() {
        // Platform-agnostic: compare against a Path, not a string (Windows uses \).
        let got = sandbox_path("data/docs/x.md").unwrap();
        assert_eq!(got, PathBuf::from("data").join("docs").join("x.md"));
    }

    #[test]
    fn sandbox_rejects_traversal() {
        assert!(sandbox_path("../escape").is_none());
        assert!(sandbox_path("data/../../etc/passwd").is_none());
        assert!(sandbox_path("a/../b").is_none()); // any ParentDir → reject
    }

    #[test]
    fn sandbox_rejects_absolute() {
        assert!(sandbox_path("/etc/passwd").is_none());
        assert!(sandbox_path("C:\\Windows\\system32").is_none());
        assert!(sandbox_path("\\\\server\\share").is_none());
    }

    #[test]
    fn sandbox_strips_dot_components() {
        let got = sandbox_path("./data/./docs/x.md").unwrap();
        assert_eq!(got, PathBuf::from("data").join("docs").join("x.md"));
    }

    // === is_writable ========================================================

    #[test]
    fn writable_user_data_paths() {
        assert!(is_writable(Path::new("data/docs/notes.md")));
        assert!(is_writable(Path::new("data/user.xml")));
        assert!(is_writable(Path::new("apps/fable/cards/x.sim")));
        assert!(is_writable(Path::new("apps/fable/profiles/x.json")));
        assert!(is_writable(Path::new("apps/fable/saves/x/y.json")));
    }

    #[test]
    fn not_writable_engine_files() {
        // Every deny rule pinned.
        assert!(!is_writable(Path::new("wupi.exe")));
        assert!(!is_writable(Path::new("wupi.html")));
        assert!(!is_writable(Path::new("cudart64_13.dll")));
        assert!(!is_writable(Path::new("bin/cublas64_13.dll")));
        assert!(!is_writable(Path::new("models/WUPI.gguf")));
        assert!(!is_writable(Path::new("models/Embed.gguf")));
        assert!(!is_writable(Path::new("memory/memory.sqlite")));
        assert!(!is_writable(Path::new("memory/memory.sqlite-wal")));
        assert!(!is_writable(Path::new("data/wupi.sim")));
        assert!(!is_writable(Path::new("data/fable.sim"))); // 2026-07-29 rename of gm.sim
        assert!(!is_writable(Path::new("data/api_config.json")));
        assert!(!is_writable(Path::new("data/theme.json")));
        assert!(!is_writable(Path::new("target/debug/wupi.exe")));
        assert!(!is_writable(Path::new(".git/HEAD")));
        assert!(!is_writable(Path::new("node_modules/foo.js")));
        assert!(!is_writable(Path::new("assets/index.js")));
        assert!(!is_writable(Path::new("paw.png")));
    }

    #[test]
    fn not_writable_unknown_paths() {
        // Default-deny: anything not on the allow-list is rejected.
        assert!(!is_writable(Path::new("random.txt")));
        assert!(!is_writable(Path::new("data/random.txt")));
        assert!(!is_writable(Path::new("apps/other/x.json")));
        assert!(!is_writable(Path::new("docs/notes.md"))); // not data/docs/
    }

    #[test]
    fn not_writable_empty_path() {
        assert!(!is_writable(Path::new("")));
    }

    // === Tool round-trips (use tempdir) =====================================

    fn temp_ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(dir.path().to_path_buf());
        (dir, ctx)
    }

    #[test]
    fn file_write_then_read_round_trip() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data/docs")).unwrap();
        let w = FileWrite;
        w.execute(
            &args(r#"{"path":"data/docs/notes.md","content":"hello world"}"#),
            &ctx,
        )
        .unwrap();
        let r = FileRead;
        let out = r
            .execute(&args(r#"{"path":"data/docs/notes.md"}"#), &ctx)
            .unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn file_write_denied_for_engine_file() {
        let (_guard, ctx) = temp_ctx();
        let w = FileWrite;
        let err = w
            .validate_args(&args(r#"{"path":"wupi.exe","content":"x"}"#))
            .unwrap_err();
        assert!(err.message.contains("not in a writable"));
    }

    #[test]
    fn file_write_rejects_traversal() {
        let (_guard, ctx) = temp_ctx();
        let w = FileWrite;
        let err = w
            .validate_args(&args(r#"{"path":"../escape","content":"x"}"#))
            .unwrap_err();
        assert!(err.message.contains("unsafe"));
    }

    #[test]
    fn file_write_rejects_oversized_content() {
        let (_guard, ctx) = temp_ctx();
        let big = "x".repeat(200_001);
        let payload = serde_json::json!({
            "path": "data/docs/big.md",
            "content": big,
        });
        let err = FileWrite.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("200 KB"));
    }

    #[test]
    fn edit_user_profile_round_trip() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data")).unwrap();
        let t = EditUserProfile;
        t.execute(
            &args(r#"{"name":"Operator","description":"likes cats"}"#),
            &ctx,
        )
        .unwrap();
        let loaded =
            crate::user_profile::load(Some(&ctx.resolve("data/user.xml").unwrap())).unwrap();
        assert_eq!(loaded.name, "Operator");
        assert_eq!(loaded.description, "likes cats");
    }

    #[test]
    fn create_sim_card_writes_parseable_sim() {
        let (_guard, ctx) = temp_ctx();
        // Root element must be <sim_card> per sim_card.rs's parser. Use CDATA
        // wrapping for prose (the parser auto-merges it into text nodes).
        let xml = "<?xml version=\"1.0\"?>\n\
                   <sim_card>\n\
                     <identity>\n\
                       <name>Test Scenario</name>\n\
                       <core_persona><![CDATA[A test persona.]]></core_persona>\n\
                     </identity>\n\
                   </sim_card>";
        let payload = serde_json::json!({
            "filename": "test_scenario",
            "xml": xml,
        });
        let t = CreateSimCard;
        t.execute(&payload, &ctx).unwrap();
        // File exists at the right path.
        let written = ctx.resolve("apps/fable/cards/test_scenario.sim").unwrap();
        assert!(written.exists(), "card should be written");
        // Round-trips through the parser.
        let loaded = crate::sim_card::load_or_fallback(&written);
        assert_eq!(loaded.name, "Test Scenario");
    }

    #[test]
    fn create_sim_card_rejects_invalid_xml() {
        let (_guard, ctx) = temp_ctx();
        let payload = serde_json::json!({
            "filename": "bad",
            "xml": "not xml <", // malformed
        });
        let err = CreateSimCard.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("XML parse"));
    }

    #[test]
    fn codex_create_then_list_round_trip() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data/docs")).unwrap();
        let payload = serde_json::json!({
            "filename": "elves",
            "title": "Elves of the Wood",
            "tags": ["fantasy", "lore"],
            "body": "Elves live long.",
        });
        CodexCreate.execute(&payload, &ctx).unwrap();
        let listed = CodexList.execute(&serde_json::Value::Null, &ctx).unwrap();
        assert!(listed.contains("elves"));
        assert!(listed.contains("Elves of the Wood"));
    }

    #[test]
    fn codex_delete_after_create() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data/docs")).unwrap();
        let payload = serde_json::json!({
            "filename": "temp",
            "title": "T",
            "body": "x",
        });
        CodexCreate.execute(&payload, &ctx).unwrap();
        let path = ctx.resolve("data/docs/temp.md").unwrap();
        assert!(path.exists());
        CodexDelete
            .execute(&args(r#"{"filename":"temp"}"#), &ctx)
            .unwrap();
        assert!(!path.exists());
    }

    // === Registry shape =====================================================

    #[test]
    fn registry_specs_have_unique_names() {
        let specs = specs();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        let deduped: Vec<&str> = names.iter().copied().collect::<std::collections::HashSet<_>>().into_iter().collect();
        let mut sorted_dedup = deduped.clone();
        sorted_dedup.sort();
        assert_eq!(names, sorted_dedup, "tool name collision in registry");
    }

    #[test]
    fn registry_includes_all_v1_tools() {
        let names: Vec<String> = specs().iter().map(|s| s.name.clone()).collect();
        for expected in [
            "file_read",
            "file_write",
            "file_delete",
            "file_list",
            "create_sim_card",
            "delete_sim_card",
            "edit_user_profile",
            "codex_create",
            "codex_delete",
            "codex_list",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing tool: {expected} (have: {names:?})"
            );
        }
    }

    // === Fable-only Director tools (generate_options / set_directive) ========

    #[test]
    fn fable_registry_includes_director_tools() {
        let names: Vec<String> = fable_specs().iter().map(|s| s.name.clone()).collect();
        assert!(names.contains(&"generate_options".to_string()), "have: {names:?}");
        assert!(names.contains(&"set_directive".to_string()), "have: {names:?}");
    }

    #[test]
    fn fable_registry_specs_disjoint_from_main_registry() {
        // The two registries must not collide (would cause tool-name shadowing
        // when chat_send extends `specs()` with `fable_specs()`).
        let main: std::collections::HashSet<String> =
            specs().iter().map(|s| s.name.clone()).collect();
        for s in fable_specs() {
            assert!(!main.contains(&s.name), "fable tool {} also in main registry", s.name);
        }
    }

    #[test]
    fn fable_specs_have_unique_names() {
        let specs = fable_specs();
        let mut names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        let deduped: Vec<&str> = names
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let mut sorted_dedup = deduped.clone();
        sorted_dedup.sort();
        assert_eq!(names, sorted_dedup, "fable tool name collision");
    }

    // --- generate_options validation ---------------------------------------

    #[test]
    fn generate_options_validate_accepts_default_no_args() {
        assert!(GenerateOptions.validate_args(&serde_json::Value::Null).is_ok());
        assert!(GenerateOptions.validate_args(&serde_json::json!({})).is_ok());
    }

    #[test]
    fn generate_options_validate_accepts_each_lens() {
        for lens in ["action", "plot", "character", "explicit", "world"] {
            let payload = serde_json::json!({ "lens": lens });
            assert!(
                GenerateOptions.validate_args(&payload).is_ok(),
                "lens {lens} should be valid"
            );
        }
    }

    #[test]
    fn generate_options_validate_rejects_unknown_lens() {
        let payload = serde_json::json!({ "lens": "bogus" });
        let err = GenerateOptions.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("unknown lens"));
    }

    #[test]
    fn generate_options_validate_accepts_count_in_range() {
        for n in [1u64, 6, 12] {
            let payload = serde_json::json!({ "count": n });
            assert!(
                GenerateOptions.validate_args(&payload).is_ok(),
                "count {n} should be valid"
            );
        }
    }

    #[test]
    fn generate_options_validate_rejects_count_over_12_with_ask_first_message() {
        // The error message is load-bearing: it steers the model to apologize
        // in prose + wait for user confirmation rather than silently clamping.
        let payload = serde_json::json!({ "count": 50 });
        let err = GenerateOptions.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("50"), "error should name the offending count");
        assert!(err.message.contains("12"), "error should name the cap");
        assert!(
            err.message.to_lowercase().contains("apolog") || err.message.to_lowercase().contains("wait"),
            "error must steer the model to apologize+wait, not clamp. Got: {}",
            err.message
        );
    }

    #[test]
    fn generate_options_validate_rejects_zero_count() {
        let payload = serde_json::json!({ "count": 0 });
        assert!(GenerateOptions.validate_args(&payload).is_err());
    }

    #[test]
    fn generate_options_validate_rejects_oversized_seed() {
        let big = "x".repeat(501);
        let payload = serde_json::json!({ "seed": big });
        let err = GenerateOptions.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("500"));
    }

    #[test]
    fn generate_options_execute_returns_signal_string() {
        let (_guard, ctx) = temp_ctx();
        let out = GenerateOptions
            .execute(&serde_json::json!({ "lens": "action", "count": 6 }), &ctx)
            .unwrap();
        assert!(out.contains("picker queued"));
    }

    // --- set_directive validation + execution ------------------------------

    #[test]
    fn set_directive_validate_rejects_empty_text() {
        let payload = serde_json::json!({ "text": "   " });
        assert!(SetDirective.validate_args(&payload).is_err());
    }

    #[test]
    fn set_directive_validate_rejects_missing_text() {
        assert!(SetDirective.validate_args(&serde_json::json!({})).is_err());
    }

    #[test]
    fn set_directive_validate_rejects_oversized_text() {
        let big = "x".repeat(1001);
        let payload = serde_json::json!({ "text": big });
        let err = SetDirective.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("1000"));
    }

    #[test]
    fn set_directive_execute_writes_to_slot() {
        let (_guard, mut ctx) = temp_ctx();
        let slot: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        ctx = ctx.with_directive_slot(slot.clone());
        let payload = serde_json::json!({ "text": "  the barkeeper grows suspicious  " });
        let out = SetDirective.execute(&payload, &ctx).unwrap();
        assert!(out.contains("armed"));
        let guard = slot.lock().unwrap();
        assert_eq!(*guard, Some("the barkeeper grows suspicious".to_string()));
    }

    #[test]
    fn set_directive_execute_overwrites_prior_directive_same_turn() {
        // Last-write-wins within a single agent-loop turn (the slot is drained
        // post-loop, so multiple set_directive calls in one turn are sequential
        // overwrites — only the final one survives).
        let (_guard, mut ctx) = temp_ctx();
        let slot: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(None));
        ctx = ctx.with_directive_slot(slot.clone());
        SetDirective
            .execute(&serde_json::json!({ "text": "first" }), &ctx)
            .unwrap();
        SetDirective
            .execute(&serde_json::json!({ "text": "second" }), &ctx)
            .unwrap();
        let guard = slot.lock().unwrap();
        assert_eq!(*guard, Some("second".to_string()));
    }

    #[test]
    fn set_directive_execute_errors_without_slot() {
        // Defensive: model could emit the tool outside a Fable session
        // (the spec says Fable-only but enforcement is by chat_send gating,
        // not the tool itself). The tool must error cleanly, not panic.
        let (_guard, ctx) = temp_ctx(); // no with_directive_slot
        let err = SetDirective
            .execute(&serde_json::json!({ "text": "x" }), &ctx)
            .unwrap_err();
        assert!(err.message.contains("directive slot") || err.message.contains("Fable"));
    }

    // ---------- wire_sim_card (Phase 5A, 2026-07-29) ----------

    #[test]
    fn wire_sim_card_writes_npc_and_location_entries() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data/docs")).unwrap();
        std::fs::create_dir_all(ctx.install_root.join("apps/fable/cards")).unwrap();
        // Write a card with <cast> + <locations> blocks.
        let xml = r#"<?xml version="1.0"?>
<sim_card>
  <metadata><id>test_cast</id><type>roleplay</type></metadata>
  <identity><name>Test Tavern</name></identity>
  <scenario>
    <locations>
      <node id="tavern" setting="indoor"><name>The Test Tavern</name><neighbor>cellar</neighbor></node>
      <node id="cellar" setting="indoor"><name>The Cellar</name><neighbor>tavern</neighbor></node>
    </locations>
    <cast>
      <npc id="mara" tier="soldier"><name>Mara</name><role>innkeep</role><alias>innkeep</alias></npc>
      <npc id="corin"><name>Corin</name><role>bard</role></npc>
    </cast>
  </scenario>
</sim_card>"#;
        std::fs::write(
            ctx.resolve("apps/fable/cards/test_cast.sim").unwrap(),
            xml.as_bytes(),
        )
        .unwrap();

        let result = WireSimCard
            .execute(&serde_json::json!({ "filename": "test_cast" }), &ctx)
            .unwrap();
        // 2 NPCs + 2 locations = 4 entries.
        assert!(result.contains("4 entries"), "result: {result}");

        // NPC entry exists + carries the canonical id.
        let npc_path = ctx.resolve("data/docs/npc_mara.md").unwrap();
        assert!(npc_path.exists(), "npc entry must be written");
        let npc_md = std::fs::read_to_string(&npc_path).unwrap();
        assert!(npc_md.contains("**Canonical id:** `mara`"));
        assert!(npc_md.contains("**Name:** Mara"));
        assert!(npc_md.contains("**Threat tier:** soldier"));

        // Location entry exists + carries the node id + exits.
        let loc_path = ctx.resolve("data/docs/loc_tavern.md").unwrap();
        assert!(loc_path.exists(), "location entry must be written");
        let loc_md = std::fs::read_to_string(&loc_path).unwrap();
        assert!(loc_md.contains("**Node id:** `tavern`"));
        assert!(loc_md.contains("**Exits:** cellar"));

        // An alias-only NPC (corin) gets its own entry too.
        assert!(ctx.resolve("data/docs/npc_corin.md").unwrap().exists());
    }

    #[test]
    fn wire_sim_card_reports_nothing_when_card_has_no_cast_or_locations() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data/docs")).unwrap();
        std::fs::create_dir_all(ctx.install_root.join("apps/fable/cards")).unwrap();
        let xml = r#"<?xml version="1.0"?>
<sim_card>
  <identity><name>Bare Card</name></identity>
</sim_card>"#;
        std::fs::write(
            ctx.resolve("apps/fable/cards/bare.sim").unwrap(),
            xml.as_bytes(),
        )
        .unwrap();
        let result = WireSimCard
            .execute(&serde_json::json!({ "filename": "bare" }), &ctx)
            .unwrap();
        assert!(result.contains("nothing to write"), "result: {result}");
    }

    #[test]
    fn wire_sim_card_errors_when_card_missing() {
        let (_guard, ctx) = temp_ctx();
        std::fs::create_dir_all(ctx.install_root.join("data/docs")).unwrap();
        std::fs::create_dir_all(ctx.install_root.join("apps/fable/cards")).unwrap();
        // validate_args passes (paths are accessible); execute fails on read.
        let payload = serde_json::json!({ "filename": "nonexistent" });
        WireSimCard.validate_args(&payload).unwrap();
        let err = WireSimCard.execute(&payload, &ctx).unwrap_err();
        assert!(err.message.contains("read card"), "err: {}", err.message);
    }
}
