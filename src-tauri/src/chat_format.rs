//! Chat-completion format presets.
//!
//! Each model family has its own turn protocol: the special tokens that
//! delimit system/user/model turns, thinking channels, and tool calls.
//! Rather than depend on llama.cpp's heuristic template matcher (which only
//! recognizes ~50 hardcoded formats and returns `-1` / FfiError on anything
//! modern like Gemma 4's `<|turn>` protocol), we hand-write the formatter
//! against each family's *documented* protocol.
//!
//! This is deterministic, dependency-free, and avoids shipping a Jinja engine.
//! Adding a new model family means writing one more `ChatFormat` impl: no
//! per-model completion logic.

use crate::session::ApiMessage;

/// What a turn looks like for a given model family.
pub trait ChatFormat: Send + Sync {
    /// Render a full conversation + system prompt into the model's native
    /// token protocol. The returned string is passed to `str_to_token`.
    ///
    /// `add_generation_prompt = true` should append the opening of a model
    /// turn (no closing) so the model continues from there.
    ///
    /// `memory_block`: an optional retrieved-memory annotation injected into
    /// the inter-turn region (between the last conversation turn and the
    /// generation prompt). `None`/empty renders nothing. This position is
    /// deliberate (2026-07-13, §2F eager-prefill design): keeping the memory
    /// block OUT of the system prompt means the stable prefix (system +
    /// turns, rendered with `memory_block=None`) is a true byte-prefix of the
    /// full prompt, which is what lets the eager prefill establish a cache
    /// the next turn can delta-prefill against. The block is a non-turn
    /// annotation (no turn markers around it) so it reads as context, not a
    /// conversational turn.
    ///
    /// `world_state`: an optional world-state schema annotation, sibling to
    /// `memory_block`. Same inter-turn position, same non-turn annotation
    /// shape (wrapped in `<world_state>` tags so the model can distinguish it
    /// from retrieved memory). Carries the persistent simulation state the
    /// 6-message window can't hold alone (summary, recent events, entities).
    fn render_prompt(
        &self,
        system: &str,
        messages: &[ApiMessage],
        tools: &[ToolSpec],
        memory_block: Option<&str>,
        world_state: Option<&str>,
        add_generation_prompt: bool,
    ) -> String;

    /// Parse raw model output into (reply, thought) channels.
    /// `reply` is the user-visible text; `thought` is the model's internal
    /// reasoning (may be empty if the model didn't think).
    fn parse_output(&self, raw: &str) -> ParsedOutput;

    /// Human-readable name for logging.
    fn name(&self) -> &'static str;
}

/// A tool declaration rendered into the prompt's system turn.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
}

/// Result of splitting model output into its channels.
#[derive(Debug, Clone, Default)]
pub struct ParsedOutput {
    /// The reply channel: what the user sees.
    pub content: String,
    /// The thought channel: the model's reasoning, if any.
    pub reasoning: String,
    /// The complete raw model output (pre-parse). Set by the engine's decode
    /// loop, NOT by `parse_output` itself. Persisted onto assistant `Message`s
    /// so `render_prompt` can re-render the turn cache-coherently (Bug #3).
    pub raw: String,
}

/// The set of model families we know how to format for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    /// Gemma 4 E4B / Gemma 4 variants. Uses `<|turn>` / `<turn|>` turn
    /// delimiters, `<|channel>thought` / `<channel|>` thinking channels,
    /// `<|tool_call>` / `<tool_call|>` for tool invocation.
    Gemma4,
    /// Fallback: plain text turns, no special protocol. Used when the loaded
    /// model isn't recognized: generation will work but without thinking or
    /// tool channels.
    Plain,
}

impl ModelFamily {
    /// Pick a family from the model's filename. Case-insensitive substring
    /// match. Extend as new models are added.
    pub fn from_model_name(filename: &str) -> Self {
        let lower = filename.to_lowercase();
        // The chat model is always shipped as `WUPI.gguf` (locked naming
        // convention 2026-07-12: any future chat model reuses this name).
        // Today's WUPI.gguf is a Gemma 4 E4B quant (Q6_K, swapped in
        // 2026-08-17 from the 12B), so it resolves to Gemma4.
        // If you ever ship a NON-Gemma chat model under `WUPI.gguf`, add a new
        // variant + ChatFormat impl and route on the model's GGUF metadata
        // (`general.architecture`) instead of the filename.
        if lower.contains("gemma") || lower.contains("wupi") {
            // Gemma 2/3 use <start_of_turn>; Gemma 4 uses <|turn>. The 4B/E4B
            // quants in this project are Gemma 4. If you load a Gemma 2/3 model,
            // add a separate variant and matcher.
            ModelFamily::Gemma4
        } else {
            ModelFamily::Plain
        }
    }

    /// Return the formatter for this family.
    pub fn formatter(&self) -> Box<dyn ChatFormat> {
        match self {
            ModelFamily::Gemma4 => Box::new(Gemma4Format),
            ModelFamily::Plain => Box::new(PlainFormat),
        }
    }

    /// The literal string that opens a new turn in this family's protocol, if
    /// it has one. Used by the engine to find turn boundaries for safe cache
    /// eviction. Returns `None` for families with no turn delimiter (Plain).
    pub fn turn_marker_literal(&self) -> Option<&'static str> {
        match self {
            ModelFamily::Gemma4 => Some("<|turn>"),
            ModelFamily::Plain => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Gemma 4 protocol
// ---------------------------------------------------------------------------

/// Renders the Gemma 4 E4B dialogue protocol.
///
/// Reference: https://ai.google.dev/gemma/docs/core/prompt-formatting-gemma4
///
/// Token summary:
///   `<|turn>{role}\n` ... `<turn|>\n`   - dialogue turn delimiters
///   `<|think|>`                         - activates thinking (system turn)
///   `<|channel>thought\n` ... `<channel|>`: internal reasoning channel
///   `<|tool>declaration:...{...}<tool|>`: tool definition
///   `<|tool_call>call:name{args}<tool_call|>`: model requests a tool
///   `<|tool_response>response:name{val}<tool_response|>`: tool result back
///
/// Roles: `system`, `user`, `model` (note: assistant → model).
pub struct Gemma4Format;

impl ChatFormat for Gemma4Format {
    fn name(&self) -> &'static str {
        "gemma4"
    }

    fn render_prompt(
        &self,
        system: &str,
        messages: &[ApiMessage],
        tools: &[ToolSpec],
        memory_block: Option<&str>,
        world_state: Option<&str>,
        add_generation_prompt: bool,
    ) -> String {
        let mut out = String::with_capacity(2048);

        // --- System turn (with optional tools + thinking activation) ---
        let has_system = !system.trim().is_empty();
        let has_tools = !tools.is_empty();
        if has_system || has_tools {
            out.push_str("<|turn>system\n");
            if has_system {
                out.push_str(system.trim());
            }
            // Tool declarations live inside the system turn, each wrapped in
            // <|tool> ... <tool|>.
            for t in tools {
                out.push_str("<|tool>declaration:");
                out.push_str(&t.name);
                out.push_str("{description:\"");
                push_escaped(&mut out, &t.description);
                out.push_str("\"}<tool|>");
            }
            // Always-on thinking: the Gemma4 `<|think|>` control token that
            // activates the model's thought channel. Injected at the END of the
            // system turn so the model emits `<|channel>thought ... <channel|>`
            // before its reply. Protocol-level (like `<|turn>`), NOT prompt
            // text — does not bloat the prose. Thinking is a CORE engine
            // requirement for coherence with the crafted .prompt files, NOT a
            // debug toggle: the model always reasons over the turn before it
            // answers. The thought body is held out of streamed prose by
            // ThoughtGate (engine.rs) + captured end-of-turn by parse_output.
            // The StreamFilter strips `<|think|>` if the model echoes it.
            // (Local-only — the API chat path is OpenAI format, no control
            // tokens; the Fable narrator is also API-only and never thinks.)
            //
            // DISABLED 2026-08-09 (`THINKING_ENABLED`): thinking 5×'d per-turn
            // wall-clock + could wedge into a non-terminating thought channel
            // (→ max_tokens hang). The Gemma 12B tracked cleanly without it. The
            // ThoughtGate / StreamFilter / extract_reasoning machinery stays
            // resident but dormant (no-ops when no thought is emitted).
            if crate::settings::THINKING_ENABLED {
                out.push_str("<|think|>");
            }
            out.push_str("<turn|>\n");
        }

        // --- Conversation turns ---
        for m in messages {
            // Gemma calls the assistant "model".
            let role = match m.role.as_str() {
                "assistant" => "model",
                other => other,
            };
            out.push_str("<|turn>");
            out.push_str(role);
            out.push('\n');

            if role == "model" {
                // Cache-coherent re-render (Bug #3): when raw_output is
                // present, re-render it so the token sequence matches what's
                // resident in the KV cache. Without this, the cleaned content
                // diverges from the cache and forces a full re-prefill each
                // turn. Tool-call turns (raw contains `<|tool_call>`) are
                // included automatically — the markers are part of the
                // verbatim raw.
                //
                // (2026-08-20 audit M2) The strip is VERBATIM-PRESERVING
                // (`strip_thought_blocks`): only complete thought spans are
                // removed; `<|channel>reply` markers, closers, tool markers,
                // and edge whitespace pass through byte-exact. The old
                // full-marker strip + trim re-rendered a DIFFERENT byte
                // sequence than the one resident in KV — every marker-bearing
                // turn (and every closer-append from a stopped turn)
                // cold-resetted, silently voiding the delta path. The §3
                // invariant still holds: the model never sees its own prior
                // thought, complete or partial.
                //
                // Thought-bearing turns are the accepted exception: stripping
                // the thought diverges from the resident KV by design → the
                // structural-divergence guard fires → cold-reset (§3).
                // Legacy turns (no raw_output) fall back to strip_thinking —
                // their content is already cleaned; markers there are
                // training residue, not cache payload.
                if !m.raw_output.is_empty() {
                    out.push_str(&strip_thought_blocks(&m.raw_output));
                } else {
                    out.push_str(&strip_thinking(&m.content));
                }
            } else if let Some(rendered) = render_tool_response_marker(&m.content) {
                // User-role turn carrying a tool response. The agent loop
                // (lib.rs::run_agent_loop) inserts these as user messages with
                // a JSON envelope `{"__tool_response__":true,...}`. We render
                // them as the Gemma 4 `<|tool_response>` protocol token so the
                // model sees the result in the channel it expects. Cache-
                // coherent: the marker is deterministic from content, so the
                // same input always renders the same tokens.
                out.push_str(&rendered);
            } else {
                out.push_str(m.content.trim());
            }
            out.push_str("<turn|>\n");
        }

        // Both retrieved_memory and world_state sit AFTER all conversation
        // turns, BEFORE the generation prompt. Non-turn annotations (no
        // `<|turn>` markers) so they read as context for the upcoming model
        // turn, not as conversational turns. Only emitted when a generation
        // prompt follows (no point annotating a render that won't generate).
        // Order: memory first, then world_state: retrieval is conversational
        // recall (transient per query), world_state is persistent ground truth.
        if add_generation_prompt {
            if let Some(block) = memory_block {
                let trimmed = block.trim();
                if !trimmed.is_empty() {
                    out.push_str("<retrieved_memory>\n");
                    out.push_str(trimmed);
                    out.push_str("\n</retrieved_memory>\n");
                }
            }
            if let Some(state) = world_state {
                let trimmed = state.trim();
                if !trimmed.is_empty() {
                    out.push_str("<world_state>\n");
                    out.push_str(trimmed);
                    out.push_str("\n</world_state>\n");
                }
            }
        }

        // --- Generation prompt ---
        if add_generation_prompt {
            out.push_str("<|turn>model\n");
        }

        out
    }

    fn parse_output(&self, raw: &str) -> ParsedOutput {
        // The model emits zero or more `<|channel>thought\n ... <channel|>`
        // blocks, optionally followed by a reply (either as trailing text
        // after the last closer, or an explicit `<|channel>reply` opener).
        // Classification is by the exact CHANNEL WORD after the opener (#43
        // 2026-08-15): the old "segment contains `<|channel>` → thought"
        // rule classified an explicit `<|channel>reply` emission as
        // reasoning, leaving `content` empty.
        //
        // The template's own strip_thinking macro uses this same split logic.
        let mut content = String::new();
        let mut reasoning = String::new();

        for part in raw.split("<channel|>") {
            match part.split_once("<|channel>") {
                None => {
                    // No opening marker: this is reply text (or trailing junk).
                    content.push_str(part);
                }
                Some((before, after)) => {
                    // Preserve any text that came before the opener in this
                    // segment (rare; usually empty).
                    if !before.trim().is_empty() {
                        content.push_str(before.trim());
                        content.push('\n');
                    }
                    let (name, body) = split_channel_word(after);
                    if name == "thought" {
                        let thought = body.trim();
                        if !thought.is_empty() {
                            if !reasoning.is_empty() {
                                reasoning.push('\n');
                            }
                            reasoning.push_str(thought);
                        }
                    } else {
                        // A non-thought channel (e.g. an explicit `reply`)
                        // carries CONTENT — keep its body, drop the marker.
                        content.push_str(body);
                    }
                }
            }
        }

        ParsedOutput {
            content: content.trim().to_string(),
            reasoning: reasoning.trim().to_string(),
            raw: String::new(),
        }
    }
}

/// Split the text after a `<|channel>` opener into `(channel_word, body)`:
/// the word is the leading alphanumeric run; the body is the remainder
/// (leading whitespace still attached). `<|channel>thought\nsecret` →
/// `("thought", "\nsecret")`. A run with NO boundary ("thoughtful") yields
/// the whole run as the word + an empty body — so "thoughtful notes" is NOT
/// misparsed as channel "thought" + body "ful notes" (#43: the old
/// `trim_start_matches("thought")` stripped repeated leading "thought" from
/// genuine thought text).
fn split_channel_word(after: &str) -> (&str, &str) {
    let trimmed = after.trim_start_matches(['\n', '\r', ' ', '\t']);
    match trimmed.find(|c: char| !c.is_alphanumeric()) {
        Some(i) => (&trimmed[..i], &trimmed[i..]),
        None => (trimmed, ""),
    }
}

/// The Gemma 4 template's strip_thinking logic, in Rust. Removes
/// `<|channel>thought\n...<channel|>` blocks entirely and keeps the rest.
/// Used when re-rendering prior assistant turns so we don't re-feed the
/// raw thinking markers back to the model as literal text.
///
/// (#43) Only THOUGHT blocks are removed. The old "any segment containing
/// `<|channel>`" rule also stripped an explicit `<|channel>reply` block —
/// body included — so such a turn re-rendered EMPTY into history (a
/// content-identical prompt divergence + a lost turn). A non-thought
/// channel's BODY is preserved; only the marker is dropped.
fn strip_thinking(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for part in text.split("<channel|>") {
        match part.split_once("<|channel>") {
            None => out.push_str(part),
            Some((before, after)) => {
                // Keep only what came before the opening marker (usually
                // nothing)...
                out.push_str(before);
                // ...and the body when the channel is NOT thought.
                let (name, body) = split_channel_word(after);
                if name != "thought" {
                    out.push_str(body);
                }
            }
        }
    }
    out.trim().to_string()
}

/// The cache-coherent sibling of [`strip_thinking`]: remove ONLY complete
/// `<|channel>thought ... <channel|>` spans (plus any UNCLOSED trailing
/// thought span — §3: the model never sees its own prior thought, complete
/// or not) and keep every other byte VERBATIM — `<|channel>reply` markers,
/// `<channel|>` closers, tool markers, and edge whitespace included, no
/// final trim. Used for the raw_output re-render path (render_prompt's
/// model turns): the KV cache holds the marker-bearing tokens the model
/// actually generated, so re-rendering them stripped or trimmed produced a
/// different byte sequence → token-level prefix divergence → cold reset on
/// every marker-bearing turn (2026-08-20 audit M2). Strip only what the
/// §3 invariant forbids; keep everything else byte-exact.
fn strip_thought_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("<|channel>") {
        let (before, after) = rest.split_at(idx);
        out.push_str(before);
        let after_open = &after["<|channel>".len()..];
        let (name, body) = split_channel_word(after_open);
        if name == "thought" {
            // Drop the span through its closer; an unclosed thought runs to
            // end-of-string (a stopped thought never re-enters the prompt).
            match body.find("<channel|>") {
                Some(c) => rest = &body[c + "<channel|>".len()..],
                None => rest = "",
            }
        } else {
            // Not a thought: keep the opener verbatim and continue scanning
            // after it (its closer, if any, survives untouched).
            out.push_str("<|channel>");
            rest = after_open;
        }
    }
    out.push_str(rest);
    out
}

/// Extract ONLY the thought channel from Gemma4 protocol-wrapped model output,
/// mirroring the reasoning capture in `parse_output` but as a standalone pure
/// fn the Fable path can call (the Fable engine does not go through
/// `Gemma4Format::parse_output` — it keeps the raw output for bracket parsing
/// and strips the reply separately via `schema::extract_reply_channel`). This
/// sibling fn pulls everything inside `<|channel>thought ... <channel|>`
/// blocks, so the chat path's reasoning can be captured for the `ParsedOutput.
/// reasoning` field. Returns "" only when the turn produced no thought channel
/// (rare — thinking is always injected on local passes, but the model may
/// still occasionally skip it). Local-only: the API chat path + the Fable
/// narrator (API) never emit a thought channel. The player-facing reasoning UI
/// was removed in the 2026-08-07 override; this fn stays for internal capture.
pub fn extract_reasoning_channel(raw: &str) -> String {
    let mut reasoning = String::new();
    for part in raw.split("<channel|>") {
        if let Some((_before, after)) = part.split_once("<|channel>") {
            // Only the exact `thought` channel contributes (#43 — see
            // split_channel_word).
            let (name, body) = split_channel_word(after);
            if name == "thought" {
                let thought = body.trim();
                if !thought.is_empty() {
                    if !reasoning.is_empty() {
                        reasoning.push('\n');
                    }
                    reasoning.push_str(thought);
                }
            }
        }
    }
    reasoning.trim().to_string()
}

/// Minimal JSON-string escaping for embedding values into the Gemma 4 tool
/// declaration / argument syntax. Escapes the characters that would break
/// the `{key:"value"}` rendering.
fn push_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
}

/// Detect the agent loop's tool-response content marker(s) and render them as
/// Gemma 4 `<|tool_response>` protocol tokens. Returns `None` for ordinary
/// user messages (the common case).
///
/// The agent loop inserts user-role messages with content shaped like:
///   `{"__tool_response__":true,"name":"file_read","ok":true,"output":"..."}`
///
/// We render that as:
///   `<|tool_response>response:file_read{"ok":true,"output":"..."}<tool_response|>`
///
/// (per the documented protocol token at line 146). The `name`, `ok`, and
/// `output` fields are extracted and re-serialized compactly so the wire
/// format is deterministic (cache-coherent: same input → same tokens).
///
/// (2026-08-20 audit fix) MULTI-CALL ITERATIONS: `run_agent_loop` inserts one
/// user message per executed call, and the window assembler's
/// `normalize_alternating` rolls consecutive same-role turns into ONE message
/// joined by `\n\n` — so a 2-call iteration arrives here as TWO concatenated
/// envelopes. Whole-string `from_str` on that blob fails → the old code fell
/// to the plain-text arm and leaked the raw `__tool_response__` JSON into the
/// prompt as literal prose for the rest of the visible window. Parse as a
/// STREAM of whitespace-separated JSON values instead: when EVERY value is a
/// valid envelope, render one protocol token per value (newline-joined);
/// any non-envelope value → `None` (the conservative plain-text fallback).
fn render_tool_response_marker(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if !trimmed.starts_with("{\"__tool_response__\":") {
        return None;
    }
    let mut out = String::new();
    let mut seen = 0usize;
    for item in serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>() {
        let v = item.ok()?;
        if v.get("__tool_response__")?.as_bool() != Some(true) {
            return None;
        }
        let name = v.get("name")?.as_str()?;
        let ok = v.get("ok")?.as_bool().unwrap_or(false);
        let output = v.get("output");
        // Build the compact payload: {"ok":...,"output":...}. The output value
        // is re-stringified to drop whitespace (deterministic tokens).
        let payload = serde_json::json!({
            "ok": ok,
            "output": output.unwrap_or(&serde_json::Value::Null),
        });
        let payload_compact = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
        if seen > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "<|tool_response>response:{}{}<tool_response|>",
            name,
            // The `{args}` slot is optional per Gemma's grammar; we always include it.
            payload_compact
        ));
        seen += 1;
    }
    if seen == 0 {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// ThoughtGate: stateful streaming filter for the variable-length thought block
// ---------------------------------------------------------------------------

/// The opening marker for Gemma 4's thinking channel.
const THOUGHT_OPEN: &str = "<|channel>thought";
/// The closing marker for any Gemma 4 channel (thought or reply).
const CHANNEL_CLOSE: &str = "<channel|>";

/// A stateful streaming filter that handles Gemma 4's variable-length thought
/// block (`<|channel>thought\n...<channel|>`).
///
/// Unlike `StreamFilter` (which handles bounded markers via regex), the thought
/// block has no known length: we can't predict when `<channel|>` will arrive.
/// The gate tracks three states:
///
/// - `Detecting`: we haven't seen enough to know if this is a thought turn or
///   a direct reply. A tiny buffer (len of the opening marker) is held until
///   we can tell. This is the first-token mode detection.
/// - `InThought`: we're inside the thought block. Everything is held back;
///   the UI should show a "thinking" indicator instead.
/// - `Reply`: the thought block closed (or there never was one). All text
///   passes through immediately with zero buffering.
///
/// The gate outputs clean reply text. The thought *content* is not emitted -
/// it's captured separately by `parse_output` at end of generation.
pub struct ThoughtGate {
    state: GateState,
    /// Small buffer used only in Detecting mode to peek at the first tokens.
    detect_buf: String,
    /// Accumulated text in InThought mode, scanned for the closer.
    thought_buf: String,
}

#[derive(Debug, PartialEq, Eq)]
enum GateState {
    /// First tokens: deciding if this turn uses the thought channel.
    Detecting,
    /// Inside the thought block, holding everything until `<channel|>`.
    InThought,
    /// Past the thought block (or never had one). Stream freely.
    Reply,
}

impl ThoughtGate {
    pub fn new() -> Self {
        ThoughtGate {
            state: GateState::Detecting,
            detect_buf: String::with_capacity(THOUGHT_OPEN.len()),
            thought_buf: String::new(),
        }
    }

    /// Feed a token piece. Returns `(reply_text, is_thinking)`.
    /// `reply_text` is text safe to show the user. `is_thinking` tells the
    /// UI to display a thinking indicator when true.
    pub fn feed(&mut self, piece: &str) -> (String, bool) {
        match self.state {
            GateState::Detecting => self.feed_detecting(piece),
            GateState::InThought => self.feed_in_thought(piece),
            GateState::Reply => (piece.to_string(), false),
        }
    }

    /// Emit any remaining held text at end of generation.
    pub fn flush(&mut self) -> String {
        match self.state {
            GateState::Detecting => {
                // Never saw enough to enter thought mode: emit the buffer.
                let out = std::mem::take(&mut self.detect_buf);
                out
            }
            GateState::InThought => {
                // Generation ended mid-thought (truncated). Discard the
                // incomplete thought: it's not useful reply text.
                self.thought_buf.clear();
                String::new()
            }
            GateState::Reply => String::new(),
        }
    }

    fn feed_detecting(&mut self, piece: &str) -> (String, bool) {
        self.detect_buf.push_str(piece);

        // Have we seen enough to decide?
        if self.detect_buf.len() >= THOUGHT_OPEN.len() {
            if self.detect_buf.starts_with(THOUGHT_OPEN) {
                // Thought turn. Strip the opening marker, capture the rest.
                let after_marker = &self.detect_buf[THOUGHT_OPEN.len()..];
                self.thought_buf.push_str(after_marker);
                self.detect_buf.clear();
                self.state = GateState::InThought;
                return (String::new(), true);
            } else {
                // Not a thought turn: emit the whole buffer as reply.
                let out = std::mem::take(&mut self.detect_buf);
                self.state = GateState::Reply;
                return (out, false);
            }
        }

        // Not enough yet to tell. Check if it COULD still become the marker.
        if THOUGHT_OPEN.starts_with(self.detect_buf.as_str()) {
            // Still a valid prefix: hold it.
            (String::new(), false)
        } else {
            // Can't possibly become the marker: emit and switch to Reply.
            let out = std::mem::take(&mut self.detect_buf);
            self.state = GateState::Reply;
            (out, false)
        }
    }

    fn feed_in_thought(&mut self, piece: &str) -> (String, bool) {
        self.thought_buf.push_str(piece);

        // Look for the channel closer. Everything after it is reply text.
        if let Some(idx) = self.thought_buf.find(CHANNEL_CLOSE) {
            let reply_start = idx + CHANNEL_CLOSE.len();
            let reply = self.thought_buf[reply_start..].to_string();
            self.thought_buf.clear();
            self.state = GateState::Reply;
            // Strip any leading whitespace/newline the model puts after the closer.
            (reply.trim_start().to_string(), false)
        } else {
            // Still thinking: hold everything.
            (String::new(), true)
        }
    }
}

impl Default for ThoughtGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod thought_gate_tests {
    use super::*;

    #[test]
    fn direct_reply_streams_immediately() {
        let mut g = ThoughtGate::new();
        let (out, thinking) = g.feed("Hello!");
        assert!(!thinking);
        assert!(out.contains("Hello!"));
    }

    #[test]
    fn thought_then_reply() {
        let mut g = ThoughtGate::new();
        // Feed the opening marker across chunks.
        let (out1, thinking1) = g.feed("<|channel>thought\n");
        assert_eq!(out1, "");
        assert!(thinking1, "should be thinking after open marker");

        let (out2, thinking2) = g.feed("reasoning here");
        assert_eq!(out2, "");
        assert!(thinking2);

        let (out3, thinking3) = g.feed("<channel|>visible reply");
        assert!(!thinking3, "should exit thinking after closer");
        assert!(out3.contains("visible reply"), "got: {:?}", out3);
    }

    #[test]
    fn detecting_prefix_not_marker_emits_immediately() {
        // Text that starts with < but isn't the thought marker.
        let mut g = ThoughtGate::new();
        let (out, thinking) = g.feed("Just a reply");
        assert!(!thinking);
        assert_eq!(out, "Just a reply");
    }

    #[test]
    fn partial_marker_prefix_held_then_released() {
        let mut g = ThoughtGate::new();
        // Feed a prefix of the marker that's ambiguous.
        let (out1, _) = g.feed("<|chan");
        assert_eq!(out1, "", "ambiguous prefix should be held");
        // Next piece makes it clearly not the marker.
        let (out2, thinking) = g.feed("not a marker");
        assert!(!thinking);
        assert!(out2.contains("not a marker"));
    }

    #[test]
    fn flush_in_detecting_emits_buffer() {
        // "partial" starts with 'p', not '<', so it can't be a prefix of the
        // thought marker. feed() emits it immediately and switches to Reply.
        // flush() then returns nothing (the buffer was already drained).
        let mut g = ThoughtGate::new();
        let (out, thinking) = g.feed("partial");
        assert!(!thinking);
        assert_eq!(out, "partial");
        let flushed = g.flush();
        assert_eq!(flushed, "");
    }

    #[test]
    fn flush_in_thought_discards() {
        let mut g = ThoughtGate::new();
        g.feed("<|channel>thought\nincomplete");
        let flushed = g.flush();
        assert_eq!(flushed, "", "incomplete thought should be discarded");
    }
}

// ---------------------------------------------------------------------------
// Plain fallback
// ---------------------------------------------------------------------------

/// No special protocol. Renders turns as `Role: content\n` so at least
/// generation works on an unrecognized model. No thinking/tools support.
pub struct PlainFormat;

impl ChatFormat for PlainFormat {
    fn name(&self) -> &'static str {
        "plain"
    }

    fn render_prompt(
        &self,
        system: &str,
        messages: &[ApiMessage],
        _tools: &[ToolSpec],
        memory_block: Option<&str>,
        world_state: Option<&str>,
        add_generation_prompt: bool,
    ) -> String {
        let mut out = String::new();
        if !system.trim().is_empty() {
            out.push_str("System: ");
            out.push_str(system.trim());
            out.push_str("\n\n");
        }
        for m in messages {
            let role = match m.role.as_str() {
                "assistant" => "Assistant".to_string(),
                other => {
                    let mut c = other.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                }
            };
            out.push_str(&role);
            out.push_str(": ");
            out.push_str(&m.content);
            out.push('\n');
        }
        // Best-effort inter-turn annotations for the fallback family. Plain
        // has no turn protocol to respect, so plain [memory] / [world] lines
        // before the generation prompt are the natural shape.
        if add_generation_prompt {
            if let Some(block) = memory_block {
                let trimmed = block.trim();
                if !trimmed.is_empty() {
                    out.push_str("[memory] ");
                    out.push_str(trimmed);
                    out.push('\n');
                }
            }
            if let Some(state) = world_state {
                let trimmed = state.trim();
                if !trimmed.is_empty() {
                    out.push_str("[world] ");
                    out.push_str(trimmed);
                    out.push('\n');
                }
            }
        }
        if add_generation_prompt {
            out.push_str("Assistant: ");
        }
        out
    }

    fn parse_output(&self, raw: &str) -> ParsedOutput {
        ParsedOutput {
            content: raw.trim().to_string(),
            reasoning: String::new(),
            raw: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ApiMessage {
        ApiMessage {
            role: role.into(),
            content: content.into(),
            raw_output: String::new(),
        }
    }

    #[test]
    fn gemma4_renders_basic_chat() {
        let f = Gemma4Format;
        let out = f.render_prompt(
            "You are Wupi.",
            &[msg("user", "Hello"), msg("model", "Hi there")],
            &[],
            None,
            None,
            true,
        );
        assert!(out.contains("<|turn>system\nYou are Wupi.<turn|>"));
        assert!(out.contains("<|turn>user\nHello<turn|>"));
        assert!(out.contains("<|turn>model\nHi there<turn|>"));
        assert!(out.ends_with("<|turn>model\n"));
    }

    #[test]
    fn gemma4_injects_memory_block_in_inter_turn_region() {
        // §2F eager-prefill design (2026-07-13): the retrieved-memory block sits
        // AFTER all conversation turns, BEFORE the generation prompt: and
        // crucially NOT inside the system prompt. This is what makes the stable
        // prefix (rendered with memory_block=None) a true byte-prefix of the
        // full prompt, enabling eager prefill. Verifies both position AND the
        // no-turn-marker annotation shape.
        let f = Gemma4Format;
        let block = "[user] earlier I mentioned the project plan";
        let out = f.render_prompt(
            "You are Wupi.",
            &[msg("user", "Hello"), msg("model", "Hi there")],
            &[],
            Some(block),
            None,
            true,
        );
        // Block appears AFTER the last turn close, BEFORE the generation prompt.
        let last_turn_end = out.rfind("<turn|>\n").unwrap();
        let mem_pos = out.find("<retrieved_memory>").unwrap();
        let gen_pos = out.rfind("<|turn>model\n").unwrap();
        assert!(last_turn_end < mem_pos, "memory block must come after all turns");
        assert!(mem_pos < gen_pos, "memory block must come before generation prompt");
        assert!(out.contains(block));
        assert!(out.ends_with("<|turn>model\n"), "still ends with the generation prompt");
        // No turn markers wrap the memory block: it's an annotation.
        assert!(!out.contains("<|turn>retrieved_memory"));
    }

    #[test]
    fn gemma4_memory_block_omitted_when_none() {
        // The stable-prefix render path passes None: no annotation leaks.
        let f = Gemma4Format;
        let out = f.render_prompt(
            "You are Wupi.",
            &[msg("user", "Hello")],
            &[],
            None,
            None,
            true,
        );
        assert!(!out.contains("<retrieved_memory>"));
        assert!(out.ends_with("<|turn>model\n"));
    }

    #[test]
    fn gemma4_memory_block_omitted_when_empty_or_whitespace() {
        let f = Gemma4Format;
        for empty in &["", "   ", "\n\n"] {
            let out = f.render_prompt(
                "You are Wupi.",
                &[msg("user", "Hello")],
                &[],
                Some(empty),
                None,
                true,
            );
            assert!(!out.contains("<retrieved_memory>"), "empty block should not render: {out}");
        }
    }

    #[test]
    fn gemma4_injects_world_state_as_sibling_annotation() {
        // Component D: the world-state schema block sits in the same inter-turn
        // region as retrieved_memory, AFTER all turns and BEFORE the generation
        // prompt. Sibling annotation (not a turn), wrapped in <world_state> tags
        // so the model can distinguish persistent ground truth from transient
        // retrieval. Verifies position, ordering (memory before world_state),
        // and the no-turn-marker shape.
        let f = Gemma4Format;
        let mem = "[user] earlier I mentioned the sword";
        let world = "summary: the hero has a sword\nentities:\n  iron sword: acquired";
        let out = f.render_prompt(
            "You are Wupi.",
            &[msg("user", "Hello"), msg("model", "Hi there")],
            &[],
            Some(mem),
            Some(world),
            true,
        );
        let last_turn_end = out.rfind("<turn|>\n").unwrap();
        let mem_pos = out.find("<retrieved_memory>").unwrap();
        let world_pos = out.find("<world_state>").unwrap();
        let gen_pos = out.rfind("<|turn>model\n").unwrap();
        assert!(last_turn_end < mem_pos, "memory must come after all turns");
        assert!(mem_pos < world_pos, "world_state must come after memory");
        assert!(world_pos < gen_pos, "world_state must come before generation prompt");
        assert!(out.contains(world));
        assert!(out.ends_with("<|turn>model\n"));
        // No turn markers wrap either annotation.
        assert!(!out.contains("<|turn>world_state"));
        assert!(!out.contains("<|turn>retrieved_memory"));
    }

    #[test]
    fn gemma4_world_state_omitted_when_none_or_empty() {
        let f = Gemma4Format;
        for world in [None, Some(""), Some("   \n")].into_iter() {
            let out = f.render_prompt(
                "You are Wupi.",
                &[msg("user", "Hello")],
                &[],
                None,
                world,
                true,
            );
            assert!(!out.contains("<world_state>"), "empty world_state should not render: {out}");
        }
    }

    #[test]
    fn gemma4_assistant_role_becomes_model() {
        let f = Gemma4Format;
        let out = f.render_prompt("", &[msg("assistant", "hi")], &[], None, None, false);
        assert!(out.contains("<|turn>model\nhi<turn|>"));
        assert!(!out.contains("<|turn>assistant"));
    }

    #[test]
    fn gemma4_parses_thought_then_reply() {
        let f = Gemma4Format;
        let raw = "<|channel>thought\nI should greet them.\n<channel|>Hello there!";
        let parsed = f.parse_output(raw);
        assert_eq!(parsed.reasoning, "I should greet them.");
        assert_eq!(parsed.content, "Hello there!");
    }

    #[test]
    fn gemma4_parses_reply_only() {
        let f = Gemma4Format;
        let parsed = f.parse_output("Just a plain reply.");
        assert_eq!(parsed.content, "Just a plain reply.");
        assert_eq!(parsed.reasoning, "");
    }

    #[test]
    fn gemma4_strip_thinking_removes_thought_blocks() {
        let cleaned = strip_thinking("<|channel>thought\nsecret\n<channel|>visible");
        assert_eq!(cleaned, "visible");
    }

    /// #43: an explicit `<|channel>reply` emission is CONTENT, not thought —
    /// the old substring rule routed it to `reasoning` (empty content) and
    /// strip_thinking deleted the body (turn re-rendered empty into history).
    #[test]
    fn gemma4_explicit_reply_channel_is_content() {
        let f = Gemma4Format;
        let parsed = f.parse_output("<|channel>reply\nHello there!<channel|>");
        assert_eq!(parsed.content, "Hello there!");
        assert_eq!(parsed.reasoning, "");
        // strip_thinking keeps the reply body (marker dropped only).
        assert_eq!(strip_thinking("<|channel>reply\nHello!<channel|>"), "Hello!");
    }

    /// #43: "thought" as a PREFIX of genuine thought text is not stripped
    /// repeatedly (`trim_start_matches` used to eat "thoughtthought…" and
    /// turn "thoughtful notes" into "ful notes").
    #[test]
    fn gemma4_thought_word_boundary_in_body() {
        let f = Gemma4Format;
        let parsed = f.parse_output("<|channel>thought\nthoughtful notes\n<channel|>ok");
        assert_eq!(parsed.reasoning, "thoughtful notes");
        assert_eq!(parsed.content, "ok");
    }

    #[test]
    fn gemma4_renders_assistant_tool_call_from_raw_output() {
        // Tool-call turn: the model emitted <|tool_call> in its raw output.
        // The formatter renders raw verbatim (cache-coherent) so the marker
        // survives into the next turn's prefill. This is what
        // run_agent_loop relies on when it commits the assistant tool-call turn.
        let f = Gemma4Format;
        let mut m = msg("assistant", "");
        m.raw_output = "<|tool_call>call:file_read{\"path\":\"data/docs/x.md\"}<tool_call|>".into();
        let out = f.render_prompt("", &[m], &[], None, None, false);
        assert!(
            out.contains("<|tool_call>call:file_read{\"path\":\"data/docs/x.md\"}<tool_call|>"),
            "raw tool_call marker must render verbatim, got: {out}"
        );
    }

    #[test]
    fn gemma4_renders_tool_response_marker_for_user_turn() {
        // The agent loop inserts user messages with the __tool_response__
        // content envelope. The formatter must render those as the Gemma 4
        // <|tool_response> protocol token, not as raw JSON prose.
        let f = Gemma4Format;
        let m = msg("user",
            "{\"__tool_response__\":true,\"name\":\"file_read\",\"ok\":true,\"output\":\"hello\"}");
        let out = f.render_prompt("", &[m], &[], None, None, false);
        assert!(
            out.contains("<|tool_response>response:file_read"),
            "tool_response marker must render as protocol token, got: {out}"
        );
        assert!(
            out.contains("<tool_response|>"),
            "must close with <tool_response|>, got: {out}"
        );
        // The raw JSON envelope should NOT leak as literal text.
        assert!(
            !out.contains("__tool_response__"),
            "raw marker envelope leaked into prompt: {out}"
        );
    }

    #[test]
    fn gemma4_ordinary_user_message_not_mistaken_for_tool_response() {
        // A user message that doesn't carry the marker must render as plain
        // content (the common chat case).
        let f = Gemma4Format;
        let m = msg("user", "what is the weather?");
        let out = f.render_prompt("", &[m], &[], None, None, false);
        assert!(out.contains("what is the weather?"));
        assert!(!out.contains("<|tool_response>"));
    }

    #[test]
    fn gemma4_renders_tool_declaration_in_system_turn() {
        // Tool declarations live inside the system turn, wrapped in
        // <|tool> ... <tool|>. Pinned so a future refactor of render_prompt
        // can't silently drop tool support.
        let f = Gemma4Format;
        let tools = vec![ToolSpec {
            name: "file_read".into(),
            description: "Read a file.".into(),
        }];
        let out = f.render_prompt("You are Wupi.", &[], &tools, None, None, false);
        assert!(out.contains("<|turn>system"));
        assert!(out.contains("<|tool>declaration:file_read"));
        assert!(out.contains("<tool|>"));
        assert!(out.contains("<turn|>"));
    }

    #[test]
    fn render_tool_response_marker_helper_round_trip() {
        // The pure helper: detect + render the marker.
        let rendered = render_tool_response_marker(
            "{\"__tool_response__\":true,\"name\":\"file_read\",\"ok\":true,\"output\":\"hello\"}",
        )
        .unwrap();
        assert!(rendered.starts_with("<|tool_response>response:file_read"));
        assert!(rendered.ends_with("<tool_response|>"));
        // Deterministic: same input → same output.
        let again = render_tool_response_marker(
            "{\"__tool_response__\":true,\"name\":\"file_read\",\"ok\":true,\"output\":\"hello\"}",
        )
        .unwrap();
        assert_eq!(rendered, again);
    }

    #[test]
    fn render_tool_response_marker_returns_none_for_plain_text() {
        assert!(render_tool_response_marker("just chatting").is_none());
        assert!(render_tool_response_marker("").is_none());
        // Not a tool response envelope.
        assert!(render_tool_response_marker("{\"name\":\"file_read\"}").is_none());
    }

    #[test]
    fn render_tool_response_marker_multi_envelope_stream() {
        // (2026-08-20 audit fix) A multi-call iteration arrives as ONE message
        // after normalize_alternating rolls the consecutive user turns
        // together (\n\n join). Every envelope must render as its own protocol
        // token — the raw JSON must NOT leak as literal prose.
        let merged = concat!(
            "{\"__tool_response__\":true,\"name\":\"file_read\",\"ok\":true,\"output\":\"a\"}\n\n",
            "{\"__tool_response__\":true,\"name\":\"memory_search\",\"ok\":false,\"output\":\"no hits\"}",
        );
        let rendered = render_tool_response_marker(merged).unwrap();
        assert_eq!(
            rendered,
            "<|tool_response>response:file_read{\"ok\":true,\"output\":\"a\"}<tool_response|>\n\
             <|tool_response>response:memory_search{\"ok\":false,\"output\":\"no hits\"}<tool_response|>"
        );
        // Deterministic: same merged input → same tokens (cache-coherent).
        assert_eq!(rendered, render_tool_response_marker(merged).unwrap());
        // A trailing non-envelope value poisons the whole message → the
        // conservative plain-text fallback (None), never a partial render.
        let poisoned = format!("{merged}\n\n{{\"ordinary\":true}}");
        assert!(render_tool_response_marker(&poisoned).is_none());
    }

    #[test]
    fn gemma4_renders_model_turn_from_raw_output_when_present() {
        // Always-on thinking invariant: a prior turn's raw_output carries its
        // own `<|channel>thought ... <channel|>` block. Re-rendering that
        // verbatim would feed the model's past reasoning back to it as literal
        // content next turn (context pollution). So the raw_output branch MUST
        // strip the thought — only the reply reaches the next prompt. The
        // KV-cache divergence this induces is handled by the structural-
        // divergence guard in engine.rs (cold-reset). This test pins the
        // invariant so a future change can't silently re-introduce the leak.
        let f = Gemma4Format;
        let mut m = msg("assistant", "visible reply");
        m.raw_output = "<|channel>thought\nsecret\n<channel|>visible reply".into();
        let out = f.render_prompt("", &[m], &[], None, None, false);
        assert!(
            !out.contains("secret"),
            "prior thought body must NOT re-enter the prompt, got: {out}"
        );
        assert!(
            !out.contains("<|channel>thought"),
            "prior thought opener must be stripped, got: {out}"
        );
        assert!(
            out.contains("visible reply"),
            "reply must survive the strip, got: {out}"
        );
    }

    #[test]
    fn gemma4_strip_thought_blocks_is_verbatim_preserving() {
        // Complete thought span dropped; every other byte survives exact —
        // reply markers, closers, edge whitespace, tool markers (the
        // cache-coherent re-render; 2026-08-20 audit M2).
        assert_eq!(
            strip_thought_blocks("<|channel>thought\nsecret\n<channel|>visible"),
            "visible"
        );
        assert_eq!(
            strip_thought_blocks("<|channel>reply\nHello!<channel|>"),
            "<|channel>reply\nHello!<channel|>"
        );
        assert_eq!(strip_thought_blocks("  edge whitespace  "), "  edge whitespace  ");
        // An unclosed trailing thought never re-enters (stopped thought).
        assert_eq!(strip_thought_blocks("ok<|channel>thought\npartial"), "ok");
        // A "thoughtful" run is NOT a thought channel (#43 semantics).
        assert_eq!(
            strip_thought_blocks("<|channel>thoughtful notes<channel|>"),
            "<|channel>thoughtful notes<channel|>"
        );
        assert_eq!(
            strip_thought_blocks("<|tool_call>call:x{}<tool_call|>"),
            "<|tool_call>call:x{}<tool_call|>"
        );
    }

    #[test]
    fn gemma4_raw_rerender_keeps_channel_markers_for_cache_coherence() {
        // M2: the raw re-render must keep reply markers byte-exact — the KV
        // cache holds the marker-bearing tokens; a stripped re-render
        // cold-resets every marker-bearing turn.
        let f = Gemma4Format;
        let mut m = msg("assistant", "Hello!");
        m.raw_output = "<|channel>reply\nHello!<channel|>".into();
        let out = f.render_prompt("", &[m], &[], None, None, false);
        assert!(
            out.contains("<|channel>reply\nHello!<channel|>"),
            "raw re-render must be byte-exact, got: {out}"
        );
    }

    #[test]
    fn gemma4_falls_back_to_strip_thinking_without_raw_output() {
        // Legacy turns (no raw_output) still get the strip_thinking path.
        let f = Gemma4Format;
        let m = msg("assistant", "<|channel>thought\nsecret\n<channel|>visible");
        let out = f.render_prompt("", &[m], &[], None, None, false);
        assert!(
            !out.contains("<|channel>"),
            "legacy turn should strip thinking, got: {out}"
        );
        assert!(out.contains("visible"));
    }

    #[test]
    fn detect_gemma4_from_name() {
        // Locked naming convention (2026-07-12): chat model is always
        // `WUPI.gguf`, embeddings model is always `Embed.gguf`.
        assert_eq!(ModelFamily::from_model_name("WUPI.gguf"), ModelFamily::Gemma4);
        assert_eq!(ModelFamily::from_model_name("wupi.gguf"), ModelFamily::Gemma4);
        // Legacy/foreign Gemma filenames still detect.
        assert_eq!(ModelFamily::from_model_name("Gemma 12B.gguf"), ModelFamily::Gemma4);
        assert_eq!(ModelFamily::from_model_name("gemma-4-E4B.gguf"), ModelFamily::Gemma4);
        // Non-Gemma foreign files fall through to Plain.
        assert_eq!(ModelFamily::from_model_name("llama.gguf"), ModelFamily::Plain);
        assert_eq!(ModelFamily::from_model_name("Embed.gguf"), ModelFamily::Plain);
    }
}
