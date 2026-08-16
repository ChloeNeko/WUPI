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
/// create_sim_card, delete_sim_card, edit_user_profile) to modify. Read tools
/// bypass this (`file_read`, `file_list`).
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
    // Same carve-out for the Fable playbook — engine content (the simulation
    // narrator reference), never tool-authored.
    if rel_str == "data/wupi.sim"
        || rel_str == "data/wupi.codex"
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
}

impl ToolCtx {
    pub fn new(install_root: PathBuf) -> Self {
        Self { install_root }
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
/// owns stateful coordination. The Fable-state tools (`fable_message_*` /
/// `fable_schema_patch`) bypass this trait's `execute` entirely: their specs
/// + `validate_args` are exposed via `fable_state_tool_specs()` /
/// `validate_fable_state_tool()` and dispatched inline from `run_agent_loop`
/// (which has AppState access for the live mutexes). See
/// `dispatch_fable_state_tool` in lib.rs.
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
/// (Arc internals where needed). Ships the 7 tools below.
pub fn registry() -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(FileRead),
        Box::new(FileWrite),
        Box::new(FileDelete),
        Box::new(FileList),
        Box::new(CreateSimCard),
        Box::new(DeleteSimCard),
        Box::new(EditUserProfile),
    ]
}

/// The Fable-only tools. Currently EMPTY — the "Director" suite
/// (`generate_options` + `set_directive`) was removed in full (the
/// Crossroads option-picker frontend + the `pending_directive` narrator-steer
/// channel were both dead). Kept as a non-empty-call site so `chat_send`'s
/// Fable-session tool gating still compiles; add Fable-only tools here when
/// they're introduced.
pub fn fable_registry() -> Vec<Box<dyn Tool>> {
    Vec::new()
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
// Fable-stateful tools (dispatched async from `run_agent_loop`)
// ---------------------------------------------------------------------------
//
// The three tools below mutate LIVE Fable state (the in-memory `fable_session`
// `Conversation` + the `fable_schema` `WorldSchema`). They can't go through
// the sync `Tool::execute` trait path because they need to await tokio mutex
// locks. Instead `chat_send` includes their specs in the prompt via
// `fable_state_specs()` (only when a Fable game is active), and the agent
// loop dispatches them inline via `dispatch_fable_state_tool` in lib.rs (which
// has `&tauri::State<'_, AppState>` access). Validation lives here so it can
// be unit-tested without AppState.

/// The three stateful tool names. Used by `dispatch_fable_state_tool` to
/// decide whether a call name should bypass the sync registry. Kept in sync
/// with `fable_state_specs()` / `validate_fable_state_tool`.
pub const FABLE_STATE_TOOL_NAMES: &[&str] =
    &["fable_message_edit", "fable_message_delete", "fable_schema_patch"];

/// True iff `name` is one of the async-dispatched Fable-state tools.
pub fn is_fable_state_tool(name: &str) -> bool {
    FABLE_STATE_TOOL_NAMES.contains(&name)
}

/// The spec list for the three stateful tools. Attached to the chat system
/// prompt only when a Fable game is active (`chat_send` gating in lib.rs).
/// Each description is the model's only guidance — keep it tight.
pub fn fable_state_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "fable_message_edit".into(),
            description: "Edit the content of one message in the active Fable \
                          session by its 0-based array index. Args: \
                          {\"index\": 3, \"content\": \"new text\"}. \
                          Find the index first via file_read on \
                          apps/fable/cards/<card_id>/session.json (the messages \
                          array). Clears the message's raw_model_output (it \
                          will not be KV-cache-coherent for that turn). \"
                          No-op on system messages."
                .into(),
        },
        ToolSpec {
            name: "fable_message_delete".into(),
            description: "Permanently remove one message from the active Fable \
                          session by its 0-based array index. Args: \
                          {\"index\": 3}. Subsequent messages shift down. Use \
                          sparingly — fable_message_edit is usually better \
                          (preserves narrative continuity)."
                .into(),
        },
        ToolSpec {
            name: "fable_schema_patch".into(),
            description: "Merge a partial WorldSchema JSON into the active \
                          Fable session's tracked state. Args: \
                          {\"patch\": {<partial WorldSchema>}}. Per top-level \
                          key in the patch: entities shallow-merges (null \
                          value deletes a key); every other field full-\
                          replaces via typed deserialize. Excludes \
                          immutable_keys (the meta-lock). Pushes prior state \
                          to the undo buffer + persists. Read current state \
                          first via file_read on apps/fable/cards/<card_id>/\
                          {world,player,npc}.json."
                .into(),
        },
    ]
}

/// Cheap structural validation for the three stateful tools. Mirrors the
/// `Tool::validate_args` contract: returns a model-facing error string on
/// failure. Stateful precondition checks (e.g. "is fable_session seated?")
/// happen in `dispatch_fable_state_tool` — this is purely the args shape.
pub fn validate_fable_state_tool(
    name: &str,
    args: &serde_json::Value,
) -> Result<(), ToolError> {
    match name {
        "fable_message_edit" => {
            let idx = args
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| ToolError::new("missing or non-integer argument `index`"))?;
            if idx > i32::MAX as u64 {
                return Err(ToolError::new("`index` is implausibly large"));
            }
            let content = args
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new("missing or non-string argument `content`"))?;
            if content.len() > 50_000 {
                return Err(ToolError::new("`content` exceeds 50 KB cap"));
            }
            Ok(())
        }
        "fable_message_delete" => {
            let idx = args
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| ToolError::new("missing or non-integer argument `index`"))?;
            if idx > i32::MAX as u64 {
                return Err(ToolError::new("`index` is implausibly large"));
            }
            Ok(())
        }
        "fable_schema_patch" => {
            let patch = args
                .get("patch")
                .ok_or_else(|| ToolError::new("missing argument `patch`"))?;
            if !patch.is_object() {
                return Err(ToolError::new("`patch` must be a JSON object"));
            }
            // Hard-cap the serialized patch so the model can't balloon the
            // system prompt with a 50 KB schema write. 100 KB is generous
            // (a full WorldSchema typically serializes to <10 KB).
            let serialized = serde_json::to_string(patch).unwrap_or_default();
            if serialized.len() > 100_000 {
                return Err(ToolError::new(
                    "`patch` serializes to >100 KB; split into smaller patches",
                ));
            }
            Ok(())
        }
        _ => Err(ToolError::new(format!(
            "not a fable-state tool: {name}"
        ))),
    }
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
///
/// Pub because lib.rs's agent-loop dispatch re-derives the SAME stem after a
/// successful `delete_sim_card` call to purge the card's memory partition
/// (§4 retention): the partition key must equal the folder stem the tool just
/// deleted, so both sides must run through this one sanitizer — a second
/// derivation path could drift and silently miss the partition.
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
                          Writes to apps/fable/cards/<filename>/<filename>.sim (the per-card \
                          folder holds the .sim + sibling .codex + world/player/npc JSON). The \
                          XML must follow the .sim format (strict XML, CDATA-wrapped prose). \
                          See rusty_tavern.sim for the shape."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let xml = req_str(args, "xml")?;
        // apps/fable/cards/<stem>/<stem>.sim is on the allow-list, but verify
        // the computed path itself (defensive: a filename like ../escape would
        // have been caught by sandbox_path, but be explicit).
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        let rel = format!("apps/fable/cards/{stem}/{stem}.sim");
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
        let rel = format!("apps/fable/cards/{stem}/{stem}.sim");
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
                          Removes the per-card folder apps/fable/cards/<filename>/ (the .sim \
                          + sibling .codex/world/player/npc JSON + saves). No-op if absent."
                .into(),
        }
    }
    fn validate_args(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let filename = req_str_nonempty(args, "filename")?;
        let stem = sanitize_stem(&filename)
            .ok_or_else(|| ToolError::new("filename empty after sanitization"))?;
        // Validate the folder path (the delete removes the whole per-card dir).
        let rel = format!("apps/fable/cards/{stem}");
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
        let rel = format!("apps/fable/cards/{stem}");
        let path = ctx.resolve(&rel)?;
        match std::fs::remove_dir_all(&path) {
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
        let raw = "<|tool_call>call:file_list ... no closer, no braces";
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
        let raw = "<|tool_call>call:file_list<tool_call|>";
        let calls = parse_tool_calls(raw);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_list");
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

    // === Fable-state tool specs + validation (2026-08-11) ===================

    #[test]
    fn fable_state_specs_lists_three_tools() {
        let s = fable_state_specs();
        let names: Vec<&str> = s.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["fable_message_edit", "fable_message_delete", "fable_schema_patch"]);
        // Each spec must carry a non-empty description (it's the model's only
        // guidance).
        for spec in &s {
            assert!(!spec.description.trim().is_empty(), "empty description for {}", spec.name);
        }
    }

    #[test]
    fn is_fable_state_tool_matches_three_names() {
        assert!(is_fable_state_tool("fable_message_edit"));
        assert!(is_fable_state_tool("fable_message_delete"));
        assert!(is_fable_state_tool("fable_schema_patch"));
        // Negative cases: file tools + unknowns don't match.
        assert!(!is_fable_state_tool("file_read"));
        assert!(!is_fable_state_tool(""));
        assert!(!is_fable_state_tool("fable_schema_patch_typo"));
    }

    #[test]
    fn validate_fable_message_edit_requires_index_and_content() {
        // Happy path.
        let ok = args(r#"{"index": 3, "content": "fixed"}"#);
        validate_fable_state_tool("fable_message_edit", &ok).expect("valid args");
        // Missing index.
        let miss = args(r#"{"content": "x"}"#);
        let err = validate_fable_state_tool("fable_message_edit", &miss).expect_err("missing index");
        assert!(err.to_string().contains("index"));
        // Missing content.
        let miss = args(r#"{"index": 0}"#);
        let err = validate_fable_state_tool("fable_message_edit", &miss).expect_err("missing content");
        assert!(err.to_string().contains("content"));
        // Non-integer index.
        let bad = args(r#"{"index": "three", "content": "x"}"#);
        validate_fable_state_tool("fable_message_edit", &bad).expect_err("non-int index");
        // Content over cap.
        let huge = serde_json::json!({ "index": 0, "content": "x".repeat(50_001) });
        let err = validate_fable_state_tool("fable_message_edit", &huge).expect_err("content too big");
        assert!(err.to_string().contains("50 KB"));
    }

    #[test]
    fn validate_fable_message_delete_requires_index() {
        validate_fable_state_tool("fable_message_delete", &args(r#"{"index": 7}"#))
            .expect("valid");
        validate_fable_state_tool("fable_message_delete", &args(r#"{}"#))
            .expect_err("missing index");
    }

    #[test]
    fn validate_fable_schema_patch_requires_object_patch() {
        // Happy path: any JSON object.
        validate_fable_state_tool(
            "fable_schema_patch",
            &args(r#"{"patch": {"summary": "new arc"}}"#),
        )
        .expect("valid");
        // Missing patch.
        validate_fable_state_tool("fable_schema_patch", &args(r#"{}"#))
            .expect_err("missing patch");
        // Non-object patch.
        validate_fable_state_tool(
            "fable_schema_patch",
            &args(r#"{"patch": ["not", "an", "object"]}"#),
        )
        .expect_err("non-object patch");
    }

    #[test]
    fn validate_fable_state_tool_unknown_name_errors() {
        let err = validate_fable_state_tool("file_read", &args("{}"))
            .expect_err("not a stateful tool");
        assert!(err.to_string().contains("not a fable-state tool"));
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
        let (_guard, _ctx) = temp_ctx();
        let w = FileWrite;
        let err = w
            .validate_args(&args(r#"{"path":"wupi.exe","content":"x"}"#))
            .unwrap_err();
        assert!(err.message.contains("not in a writable"));
    }

    #[test]
    fn file_write_rejects_traversal() {
        let (_guard, _ctx) = temp_ctx();
        let w = FileWrite;
        let err = w
            .validate_args(&args(r#"{"path":"../escape","content":"x"}"#))
            .unwrap_err();
        assert!(err.message.contains("unsafe"));
    }

    #[test]
    fn file_write_rejects_oversized_content() {
        let (_guard, _ctx) = temp_ctx();
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
        // File exists at the right path — the per-card folder layout (§6B):
        // apps/fable/cards/<stem>/<stem>.sim.
        let written = ctx
            .resolve("apps/fable/cards/test_scenario/test_scenario.sim")
            .unwrap();
        assert!(written.exists(), "card should be written");
        // Round-trips through the parser.
        let loaded = crate::sim_card::load_or_fallback(&written);
        assert_eq!(loaded.name, "Test Scenario");
    }

    #[test]
    fn create_sim_card_rejects_invalid_xml() {
        let (_guard, _ctx) = temp_ctx();
        let payload = serde_json::json!({
            "filename": "bad",
            "xml": "not xml <", // malformed
        });
        let err = CreateSimCard.validate_args(&payload).unwrap_err();
        assert!(err.message.contains("XML parse"));
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
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing tool: {expected} (have: {names:?})"
            );
        }
    }

    // === Fable-only tools ===================================================
    // The "Director" suite (generate_options + set_directive) was removed in
    // full; fable_registry() is currently empty. These two property tests pin
    // the contract that the (currently-empty) fable registry never collides
    // with or shadows the main registry — useful scaffolding for when the
    // next Fable-only tool is added.

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

}

