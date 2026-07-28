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
        let Some(close_idx) = close_rel else {
            // Unterminated block: drop the rest (the model cut off mid-call).
            break;
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
fn parse_args_lenient(span: &str) -> serde_json::Value {
    let trimmed = span.trim();
    if trimmed.is_empty() {
        return serde_json::Value::Null;
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => v,
        Err(_) => {
            // Carry the raw text so the executor can surface a helpful error.
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
    if rel_str == "data/wupi.sim" || rel_str == "data/wupi.codex" {
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
    /// Codex-dirty flag shared with `AppState::codex_dirty`. Set by the
    /// `codex_create`/`codex_delete` tools after a successful write/delete so
    /// `chat_send`'s finalize block re-seeds the `__codex__` partition before
    /// the next turn (closing the hot-load seam gap: tool writes are otherwise
    /// invisible to retrieval until reboot — the `codex_save`/`codex_delete`
    /// IPCs re-seed inline, but `Tool::execute` is sync and has no engine
    /// handle, so it sets this flag and the async drain happens on the chat
    /// path). Default empty so test/standalone construction is unaffected.
    pub codex_dirty: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Per-turn Director directive slot (the `set_directive` tool's write
    /// target). Minted fresh by `chat_send` per call, drained to
    /// `AppState::pending_directive` after the agent loop returns — same
    /// lifetime pattern as `codex_dirty`. `Tool::execute` is sync and the slot
    /// is never held across an await, so `std::sync::Mutex` is correct here
    /// (NOT `tokio::sync`). Default `None` → test/standalone unaffected; the
    /// `set_directive` tool errors gracefully if invoked without a slot.
    pub directive_slot: Option<Arc<std::sync::Mutex<Option<String>>>>,
}

impl ToolCtx {
    pub fn new(install_root: PathBuf) -> Self {
        Self {
            install_root,
            codex_dirty: None,
            directive_slot: None,
        }
    }
    /// Attach the shared codex-dirty flag (called once per chat_send from
    /// lib.rs, before the agent loop iterates).
    pub fn with_codex_dirty(mut self, flag: Arc<std::sync::atomic::AtomicBool>) -> Self {
        self.codex_dirty = Some(flag);
        self
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
    /// Mark the codex partition dirty so the chat-path finalize block re-seeds.
    /// Cheap no-op when no flag is attached (test/standalone ctx).
    fn mark_codex_dirty(&self) {
        if let Some(f) = self.codex_dirty.as_ref() {
            f.store(true, std::sync::atomic::Ordering::Relaxed);
        }
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
        let stem = crate::codex::sanitize_stem(&filename)
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
        let stem = crate::codex::sanitize_stem(&filename)
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
        let stem = crate::codex::sanitize_stem(&filename)
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
        let stem = crate::codex::sanitize_stem(&filename)
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
        let stem = crate::codex::sanitize_stem(&filename)
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
        let filename = req_str(args, "filename")?;
        let title = req_str(args, "title")?;
        let body = req_str(args, "body")?;
        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let dir = ctx.resolve("data/docs")?;
        let stem = crate::codex::save_file(&dir, &filename, &title, &tags, &body)
            .map_err(|e| ToolError::new(format!("save codex: {e}")))?;
        // Mark the codex partition dirty so chat_send's finalize block re-seeds
        // the retrieval index before the next turn (otherwise the new entry is
        // invisible to the narrator until reboot — the codex_save IPC re-seeds
        // inline, but Tool::execute is sync and has no engine handle).
        ctx.mark_codex_dirty();
        Ok(format!("wrote data/docs/{stem}.md"))
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
        let stem = crate::codex::sanitize_stem(&filename)
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
        let filename = req_str(args, "filename")?;
        let dir = ctx.resolve("data/docs")?;
        crate::codex::delete_file(&dir, &filename)
            .map_err(|e| ToolError::new(format!("delete codex: {e}")))?;
        // Same dirty-flag rationale as codex_create: keep the retrieval index
        // in sync so the next narrator turn sees the deletion.
        ctx.mark_codex_dirty();
        Ok(format!("deleted {}", filename))
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
        let dir = ctx.resolve("data/docs")?;
        let entries = crate::codex::list_files(&dir)
            .map_err(|e| ToolError::new(format!("list codex: {e}")))?;
        if entries.is_empty() {
            return Ok("(empty)".into());
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|e| {
                format!(
                    "{} | {} | {}",
                    e.filename,
                    e.title,
                    e.tags.join(",")
                )
            })
            .collect();
        Ok(lines.join("\n"))
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
        // lens: if present, must be one of the 5 valid ids.
        if let Some(lens) = args.get("lens").and_then(|v| v.as_str()) {
            if crate::crossroads_prompt::CrossroadsCategory::from_id(lens).is_none() {
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
    fn unterminated_call_dropped() {
        // No closing marker → drop everything after.
        let raw = "<|tool_call>call:file_read{\"path\":\"a\"} ... no closer";
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
}
