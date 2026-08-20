//! LLM backend façade + the process-wide llama.cpp backend singleton.
//!
//! The heavy generation logic now lives in [`crate::engine`]: a dedicated
//! thread owning a persistent `LlamaContext` with Q8_0 KV cache and a
//! [`KvBuffer`] that tracks the token IDs resident in the cache so each turn
//! only prefills the **delta** since the last turn.
//!
//! This module is a thin façade: it loads the model off-thread, leaks it
//! to `&'static` (so the engine can hold a `LlamaContext<'static>`), spawns
//! the engine, and exposes a [`GenerationClient`] impl that posts requests to
//! the engine thread.
//!
//! # Why `Box::leak`
//!
//! `LlamaContext<'a>` borrows `&'a LlamaModel`. Storing model + context together
//! is self-referential and rejected by the borrow checker. Leaking the model to
//! `&'static LlamaModel` dissolves the borrow: `new_context(&'static self)`
//! yields `LlamaContext<'static>`, which the engine thread can own freely.
//!
//! This is the idiomatic choice for a **process-lifetime singleton**: the
//! model is loaded once and lives until the OS exits. The memory is never
//! reclaimed, which is exactly what we want (we don't want to unload the
//! model mid-session). If hot-swap lands later (a P-phase settings feature),
//! reclaim via `Box::from_raw(ptr)` + `drop` before loading the replacement.
//!
//! # The shared backend
//!
//! [`shared_backend`] is the single chokepoint for `LlamaBackend::init()`,
//! which the crate documents as panic-on-double-init. Both the chat loader
//! (here) and the Memory embedder ([`crate::memory_embedder_llama`]) call it
//!: the `OnceLock` makes the race safe even if both load concurrently.

use crate::chat_format::ParsedOutput;
use crate::chat_format::ModelFamily;
use crate::engine::{ChatEngine, EngineReply, EngineRequest};
use crate::session::ApiMessage;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::model::LlamaModel;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;

pub type StreamFuture = Pin<Box<dyn Future<Output = anyhow::Result<ParsedOutput>> + Send>>;
pub type ChunkFn = Arc<dyn Fn(&str) + Send + Sync>;

/// (2026-08-15 audit fix) Cooperative cancel watcher raced against the
/// TTFT wait via `select!`: resolves once the token is signaled (polled at
/// 100ms — an AtomicBool has no async wakeup). A stop during the
/// first-token window is honored in ~100ms instead of the full 10s.
async fn cancel_poll(cancel: &CancelToken) {
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
/// Cancellation flag shared between `chat_send` and `chat_stop`. The engine's
/// decode loop checks this between tokens.
pub type CancelToken = Arc<std::sync::atomic::AtomicBool>;

pub trait GenerationClient: Send + Sync {
    fn stream(
        &self,
        messages: Vec<ApiMessage>,
        memory_block: Option<String>,
        world_state: Option<String>,
        tools: Vec<crate::chat_format::ToolSpec>,
        context_size: u32,
        on_chunk: ChunkFn,
        cancel: CancelToken,
    ) -> StreamFuture;

    fn is_ready(&self) -> bool {
        false
    }
}

/// The process-wide shared llama.cpp backend, leaked to `&'static`.
///
/// `LlamaBackend::init()` must be called exactly once per process: a second
/// call returns `Err(BackendAlreadyInitialized)` (the crate guards itself with
/// an internal `AtomicBool`). The chat engine (`load_blocking`) and the Memory
/// embedder both need the backend, so this function is the single chokepoint
/// that serializes them: the first caller runs `init()` and leaks the backend,
/// every later caller reuses the same `&'static` ref. The `OnceLock` makes the
/// race safe even if both threads load concurrently at startup.
///
/// The backend is a ZST (`pub struct LlamaBackend {}`) with no raw fields, so
/// `&'static LlamaBackend` is `Send + Sync` and safe to share across the
/// `wupi-engine` and `wupi-embedder` threads. Leaking is correct for a
/// process-lifetime singleton: it lives until the OS exits, matching the
/// model leak in `LlamaModelHandle::into_static`.
static SHARED_BACKEND: OnceLock<&'static LlamaBackend> = OnceLock::new();

pub fn shared_backend() -> &'static LlamaBackend {
    SHARED_BACKEND.get_or_init(|| {
        let backend = LlamaBackend::init()
            .expect("LlamaBackend::init failed: cannot start llama.cpp");
        Box::leak(Box::new(backend))
    })
}

/// Echo fallback used when no model file is found. Unchanged from Layer 1.
pub struct EchoBackend;

impl GenerationClient for EchoBackend {
    fn stream(
        &self,
        _messages: Vec<ApiMessage>,
        _memory_block: Option<String>,
        _world_state: Option<String>,
        _tools: Vec<crate::chat_format::ToolSpec>,
        _context_size: u32,
        on_chunk: ChunkFn,
        _cancel: CancelToken,
    ) -> StreamFuture {
        Box::pin(async move {
            let reply = "(echo backend) Wupi's model isn't loaded yet.";
            on_chunk(reply);
            Ok(ParsedOutput {
                content: reply.to_string(),
                reasoning: String::new(),
                raw: String::new(),
            })
        })
    }
}

/// HTTP API backend: talks to an OpenAI-compatible chat completions endpoint
/// (Z.AI, NanoGPT, OpenRouter, OpenAI itself, llama.cpp/vLLM/Ollama servers).
/// Implements the same [`GenerationClient`] trait as [`LlamaCppBackend`] so
/// `chat_send` can dispatch on `ModelSource` without caring which backend is
/// active.
///
/// **Streaming:** POST `{endpoint}/chat/completions` with
/// `{model, messages, stream:true, temperature?}`, read the SSE response
/// incrementally. Each `data: {...}` line carries a `choices[0].delta.content`
/// token; forward each to `on_chunk` for live UI rendering. Honors `cancel`
/// by aborting mid-stream (the equivalent of the local engine's between-token
/// cancel check).
///
/// **Memory + world_state injection:** the local backend splices these into
/// the inter-turn region via `render_prompt`. An API only takes a flat
/// `messages` list, so we fold them into the system message (they're already
/// XML-tagged blocks: `<retrieved_memory>`, `<world_state>` - and read fine
/// as additional system context). This preserves the retrieval + schema
/// injection that makes Wupi's memory work, just routed through the system
/// role instead of a protocol splice.
///
/// **No reasoning/raw:** the OpenAI streaming format has no equivalent of the
/// Gemma4 thought channel. `ParsedOutput.reasoning` + `.raw` are left empty
/// (the post-generation archiving + schema-delta pipeline keys off `.content`
/// only, so this is safe).
/// Cloning is cheap (`reqwest::Client` is an `Arc` pool internally) and is
/// how the AppState-level cache shares one warm client (connection pool +
/// TLS session) across turns — see `http_backend_cached` in lib.rs.
#[derive(Clone)]
pub struct HttpBackend {
    profile: crate::api::ApiProfile,
    client: reqwest::Client,
}

impl HttpBackend {
    /// Construct from a saved profile. Builds a reqwest client with the
    /// bearer token pre-attached so every request on this client is
    /// authenticated. (2026-08-15 audit fix) NO total timeout: the old
    /// `.timeout(300s)` is a whole-request deadline in reqwest — a legit
    /// 5+ minute narration died as `api_lost` mid-stream, contradicting "a
    /// slow stream is never killed mid-flight". Liveness is enforced in the
    /// read loop instead: the absolute TTFT deadline pre-first-token +
    /// `settings::API_CHUNK_IDLE_TIMEOUT_MS` between chunks after it. A
    /// connect timeout still bounds a dead-endpoint attempt.
    pub fn new(profile: crate::api::ApiProfile) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if !profile.api_key.is_empty() {
            match reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                profile.api_key
            )) {
                Ok(v) => {
                    headers.insert(reqwest::header::AUTHORIZATION, v);
                }
                Err(e) => {
                    // A key containing control chars / non-visible ASCII
                    // can't become a header value. The old silent skip sent
                    // UNAUTHENTICATED requests — the user saw an opaque
                    // provider 401 (as api_lost) with no hint of the cause.
                    // Loud error so the paste/typo is diagnosable.
                    tracing::error!(
                        error = %e,
                        "API key is not a valid header value; requests will be sent unauthenticated"
                    );
                }
            }
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { profile, client }
    }

    /// Resolve the full chat-completions URL from the profile's base endpoint.
    /// Accepts either a bare base (`https://nano-gpt.com/api/v1`) or one that
    /// already includes the path (`https://x/api/v1/chat/completions`). If the
    /// endpoint ends with `/`, it's trimmed first.
    fn completions_url(&self) -> String {
        let base = self.profile.endpoint.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

/// A single message in the OpenAI chat request body. The local `ApiMessage`
/// has a `raw_output` field the API doesn't want: this is the slim wire view.
/// (Could `#[serde(skip)]` raw_output on ApiMessage instead, but that would
/// couple the session type to the API wire format; a local view is cleaner.)
#[derive(serde::Serialize)]
struct ChatRequestMessage {
    role: String,
    content: String,
}

/// The streaming chunk envelope: `{ choices: [ { delta: { content: "..." } } ] }.
/// `content` is `Option` because the first chunk typically carries only `role`,
/// and the final chunk carries `finish_reason` instead. Everything else is
/// ignored: we only want the delta text.
///
/// `reasoning_content` (2026-08-09): GLM-5.2 (and other reasoning models) stream
/// their chain-of-thought in a SEPARATE `reasoning_content` delta field BEFORE
/// the first `content` token. We do NOT render reasoning (the API narrator must
/// never think, §3A) — but we DO need to know a token arrived so the TTFT
/// deadline retires (else a long reasoning phase trips the 10s timeout →
/// `api_lost`). The field is parsed + checked for "is the stream alive?" but
/// its body is discarded.
#[derive(serde::Deserialize)]
struct ChatStreamChunk {
    choices: Vec<ChatStreamChoice>,
}
#[derive(serde::Deserialize)]
struct ChatStreamChoice {
    delta: ChatStreamDelta,
}
#[derive(serde::Deserialize)]
struct ChatStreamDelta {
    #[serde(default)]
    content: Option<String>,
    /// GLM-5.2 reasoning-model chain-of-thought. Parsed so the TTFT deadline
    /// can retire on it; the body is discarded (never rendered, never stored).
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// v0.7: enforce a soft char-budget on the API message payload by dropping the
/// OLDEST non-system messages from the front. The system message (a message at
/// index 0 with role == "system") is always preserved — it carries
/// OS_DIRECTIVES + persona + memory/world_state.
///
/// This is a conservative safety net, not a tokenizer: the provider counts
/// tokens server-side. We estimate at ~4 chars/token (rounded down — most
/// English prose is 3-5 chars/token, so this slightly over-truncates, which
/// is the safe direction). Mirrors the local engine's front-truncation guard
/// (engine.rs::truncate_to_fit).
///
/// Invariants:
/// - The system message (if present at index 0) is NEVER dropped, even if it
///   alone exceeds the budget (it's load-bearing context).
/// - Otherwise: drop oldest-first until total ≤ budget or only the system
///   message + one most-recent message remain.
fn truncate_to_budget(msgs: &mut Vec<ChatRequestMessage>, budget_chars: usize) {
    let total: usize = msgs.iter().map(|m| m.content.chars().count()).sum();
    if total <= budget_chars {
        return;
    }
    // Find the system message index (only the FIRST message counts as system;
    // some providers accept multiple system messages but we only protect [0]).
    let system_idx = if msgs.first().map(|m| m.role == "system").unwrap_or(false) {
        Some(0)
    } else {
        None
    };

    // Walk from index 1 (or 0 if no system), dropping oldest-first. `cursor`
    // is never incremented: after `msgs.remove(cursor)`, the next message
    // shifts down into the same index, so we keep removing at `cursor` until
    // under budget or we hit the "keep system + last" floor.
    let cursor = system_idx.map_or(0, |i| i + 1);
    let mut current = total;
    while current > budget_chars && cursor < msgs.len() {
        // Always keep at least the system message + the last message.
        if cursor >= msgs.len().saturating_sub(1) {
            break;
        }
        let removed_len = msgs[cursor].content.chars().count();
        current = current.saturating_sub(removed_len);
        msgs.remove(cursor);
        // cursor stays put: after remove, the next message shifts into cursor.
    }
    if current > budget_chars {
        tracing::warn!(
            total_chars = current,
            budget_chars,
            "API payload still over budget after truncation (system + last msg preserved)"
        );
    }
}

impl GenerationClient for HttpBackend {
    fn stream(
        &self,
        messages: Vec<ApiMessage>,
        memory_block: Option<String>,
        world_state: Option<String>,
        _tools: Vec<crate::chat_format::ToolSpec>,
        _context_size: u32,
        on_chunk: ChunkFn,
        cancel: CancelToken,
    ) -> StreamFuture {
        let url = self.completions_url();
        let model = self.profile.model.clone();
        let max_context = self.profile.max_context;
        let client = self.client.clone();
        Box::pin(async move {
            // Fold memory_block + world_state into the system message. They're
            // already XML-tagged blocks; appending them to the existing system
            // content keeps the retrieval/schema context that Wupi depends on.
            let mut wire_messages: Vec<ChatRequestMessage> = Vec::with_capacity(messages.len());
            let mut extra_ctx = String::new();
            if let Some(mb) = memory_block.as_ref() {
                if !mb.trim().is_empty() {
                    extra_ctx.push_str("\n\n");
                    extra_ctx.push_str(mb);
                }
            }
            if let Some(ws) = world_state.as_ref() {
                if !ws.trim().is_empty() {
                    extra_ctx.push_str("\n\n");
                    extra_ctx.push_str(ws);
                }
            }
            for (i, m) in messages.into_iter().enumerate() {
                let content = if i == 0 && m.role == "system" && !extra_ctx.is_empty() {
                    format!("{}{extra_ctx}", m.content)
                } else {
                    m.content
                };
                wire_messages.push(ChatRequestMessage {
                    role: m.role,
                    content,
                });
            }

            // v0.7: enforce the profile's max_context (defaults to
            // settings::CTX_API, 16384) as a soft input-token budget. We don't ship a real tokenizer to the API
            // path (the provider does its own counting server-side), so this
            // is a conservative safety net: estimate at ~4 chars/token and
            // drop the OLDEST non-system messages from the front until under
            // budget. The system message (index 0 if role=="system") is
            // always preserved — it carries OS_DIRECTIVES + persona +
            // memory/world_state. Mirrors the local engine's front-truncation
            // guard (engine.rs).
            let max_ctx = max_context.unwrap_or(crate::settings::CTX_API);
            let budget_chars = (max_ctx as usize).saturating_mul(4);
            truncate_to_budget(&mut wire_messages, budget_chars);

            // Build the request body. `stream: true` requests SSE.
            // Sampler params: temp 0.85 + top_p 0.95 ONLY. These are the
            // OpenAI /chat/completions standard fields; min_p + top_k are
            // llama.cpp-native and at least one major provider (z.ai / GLM-5.2)
            // rejects unknown fields with HTTP 400 code 1210 ("Invalid API
            // parameter"), forcing a silent fallback to local — which
            // defeats the whole point of the API path. Cloud providers
            // handle their own sampler internals behind temperature/top_p,
            // so the llama.cpp-specific knobs add no value here anyway.
            //
            // PRECISION (2026-08-09): GLM-5.2 rejects top_p/temperature with
            // more than 2 decimal places (HTTP 400 code 1210 "限制小数点[2]位").
            // API_TEMP/API_TOP_P are `f32`, + the `json!` macro promotes f32 →
            // f64 for serialization, so `0.95_f32` becomes `0.949999988079071`
            // (16 places) → instant 400. Round to 2 decimals (as f64) BEFORE
            // serialization so the JSON emits clean `0.95`/`0.85`.
            // (2026-08-16 audit fix) `API_TEMP` governs every API turn, per the
            // locked sampler table. A legacy profile that persisted its own
            // `temperature` (the never-persist rule is enforced in the panels,
            // not in old saves) is IGNORED at stream time — it used to override
            // the locked 0.85 backend-side.
            let temp_val = crate::settings::API_TEMP as f64;
            let temp_r = (temp_val * 100.0).round() / 100.0;
            let topp_r = ((crate::settings::API_TOP_P as f64) * 100.0).round() / 100.0;
            let body = serde_json::json!({
                "model": model,
                "messages": wire_messages,
                "stream": true,
                "temperature": temp_r,
                "top_p": topp_r,
            });

            // (2026-08-16 audit fix) The request phase is part of the
            // first-token budget: the ABSOLUTE TTFT deadline is armed BEFORE
            // `send()` and raced against cancel, so a provider that accepts
            // the connection then stalls before responding (no headers, no
            // error) can no longer hang the await forever. The old bare
            // `.send().await` had no deadline and no cancel path — the wedged
            // turn kept the fable cancel slot occupied, so `fable_turn_in_
            // flight` stayed true and fable_end/load/start all refused; the
            // only recovery was killing the process.
            let ttft_start = std::time::Instant::now();
            let ttft_deadline = tokio::time::Instant::now()
                + std::time::Duration::from_millis(crate::settings::API_FIRST_TOKEN_TIMEOUT_MS);
            let response = {
                let send_fut = client.post(&url).json(&body).send();
                tokio::select! {
                    res = send_fut => {
                        res.map_err(|e| anyhow::anyhow!("API request to {url} failed: {e}"))?
                    }
                    _ = cancel_poll(&cancel) => {
                        // Cancel during the request phase: return the EMPTY Ok,
                        // NOT Err — the narrator's Err arm would surface this
                        // as api_lost. Callers re-check the token and finalize
                        // as a soft cancel, the same contract as a read-phase
                        // stop (the abandoned future severs the connection).
                        return Ok(ParsedOutput {
                            content: String::new(),
                            reasoning: String::new(),
                            raw: String::new(),
                        });
                    }
                    _ = tokio::time::sleep_until(ttft_deadline) => {
                        return Err(anyhow::anyhow!(
                            "API_TIMEOUT: no response headers within {}ms — provider hung on request",
                            crate::settings::API_FIRST_TOKEN_TIMEOUT_MS
                        ));
                    }
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "API returned {status}: {}",
                    text.chars().take(500).collect::<String>()
                ));
            }

            // Stream the SSE body. Each `data: {json}` line is one token chunk.
            // `data: [DONE]` terminates the stream. Lines not starting with
            // `data:` (comments, event headers) are ignored.
            use futures_util::StreamExt;
            let mut stream = response.bytes_stream();
            // Byte buffer (P0 fix): a TCP read can split a multi-byte UTF-8
            // char across chunk boundaries. Decoding each raw chunk lossily
            // replaced the split char's halves with two U+FFFDs — permanent
            // corruption in the streamed + stored narration. SSE lines are
            // framed by ASCII b'\n', so buffering BYTES and decoding each
            // complete line is always UTF-8-aligned.
            let mut buffer: Vec<u8> = Vec::new();
            let mut full_content = String::new();
            // §11.43 — streaming repetition kill switch. The API path gets no
            // DRY sampler + no Rust-side sampler chain (providers lock those
            // knobs), so a stateful tail-buffer runs the SAME
            // `detect_repetition_offset` primitive the post-gen truncator
            // uses, on every chunk. On a confirmed loop we BREAK the stream
            // loop: `response` + `stream` drop out of scope at function
            // return, severing TCP + stopping token billing instantly. The
            // finalized `full_content` is the truncated clean prose (one
            // instance of the phrase + lead-in) — byte-identical to what the
            // post-gen firewall would have produced for the same input.
            let mut rep_guard = crate::stream_filter::StreamRepetitionDetector::new();

            // First-token (TTFT) deadline. The API must deliver its FIRST
            // content token within `settings::API_FIRST_TOKEN_TIMEOUT_MS`. If
            // it doesn't, the call is treated as dead (the provider hung on
            // the request — no thinking, no nothing) and we bail with the
            // `API_TIMEOUT` sentinel so `chat_send` can fire the top-center
            // error bubble + fall back to local. Once the first token lands,
            // the deadline is retired: a slow-but-working stream is NEVER
            // killed mid-flight.
            //
            // (2026-08-15 audit fix) ABSOLUTE deadline, not per-read: the old
            // per-read `timeout(10s)` reset on every keep-alive ping <10s
            // apart, so a provider streaming only keep-alives never tripped
            // the watchdog. `timeout_at(deadline)` bounds the WHOLE wait.
            // Cancel is observed DURING the wait (select! against a 100ms
            // cancel poll) — a stop used to lag up to the full 10s window.
            // Post-first-token reads carry the `API_CHUNK_IDLE_TIMEOUT_MS`
            // stall guard (the reqwest 300s total timeout is gone — it killed
            // legit 5+ minute narrations).
            let mut got_first_token = false;
            let mut stream_done = false;
            let idle_dur =
                std::time::Duration::from_millis(crate::settings::API_CHUNK_IDLE_TIMEOUT_MS);

            // Per-line SSE parser, hoisted into a closure so the EOF tail
            // flush runs the EXACT same parse path as in-stream lines
            // (2026-08-16 audit fix: a provider ending the body without a
            // trailing newline on its final `data:` line used to have that
            // line silently dropped — the split loop only fires on
            // terminators).
            enum LineOutcome {
                Next,
                Done,
                Fail(String),
                RepKill(ParsedOutput),
            }
            let mut process_line =
                |full_content: &mut String, got_first_token: &mut bool, line: &str| -> LineOutcome {
                    if line.is_empty() || !line.starts_with("data:") {
                        return LineOutcome::Next;
                    }
                    let data = line["data:".len()..].trim();
                    if data == "[DONE]" {
                        // (P3 fix) Terminate the OUTER read loop too — the
                        // old `break` only exited this line-parse while, so
                        // termination relied on the server closing the
                        // connection; a provider that keeps it open hung the
                        // turn until the client timeout.
                        return LineOutcome::Done;
                    }
                    // (2026-08-15 audit fix) In-band error events: providers
                    // like OpenRouter stream failures as `data: {"error":…}`
                    // with HTTP 200. The chunk won't parse as ChatStreamChunk,
                    // so the old path silently skipped it and returned Ok("")
                    // — an empty beat committed as a normal turn. Detect the
                    // error shape FIRST and fail the stream loudly (the
                    // caller's api_lost path handles revert + composer lock).
                    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(data) {
                        if obj.get("error").is_some() && obj.get("choices").is_none() {
                            let msg = match obj.get("error") {
                                Some(serde_json::Value::String(s)) => s.clone(),
                                Some(other) => other.to_string(),
                                None => String::new(),
                            };
                            return LineOutcome::Fail(msg);
                        }
                    }
                    // Parse the JSON chunk; on failure skip (some providers
                    // send keep-alive comments or partial events we don't care
                    // about). A malformed chunk must never kill the stream.
                    if let Ok(parsed) = serde_json::from_str::<ChatStreamChunk>(data) {
                        if let Some(choice) = parsed.choices.into_iter().next() {
                            // GLM-5.2 (a reasoning model) streams its chain-of-
                            // thought in `reasoning_content` BEFORE the first
                            // `content` token. The reasoning body is DISCARDED
                            // (the API narrator never thinks, §3A) — but the
                            // arrival of a reasoning token proves the stream is
                            // alive, so retire the TTFT deadline on it. Without
                            // this, a long reasoning phase trips the 10s
                            // first-token timeout → `api_lost` mid-narration.
                            // TTFT (2026-08-19): logged ONCE per stream, at the
                            // first token of EITHER kind — reasoning_content
                            // counts (it is the TTFT deadline's own liveness
                            // signal). Rides the verbose tracing log (wupi.log
                            // + the logs/ mirror).
                            if let Some(rc) = choice.delta.reasoning_content {
                                if !rc.is_empty() {
                                    if !*got_first_token {
                                        tracing::debug!(
                                            ttft_ms = %ttft_start.elapsed().as_millis(),
                                            "API stream: first token (reasoning)"
                                        );
                                    }
                                    *got_first_token = true;
                                }
                            }
                            if let Some(piece) = choice.delta.content {
                                if !piece.is_empty() {
                                    // First content token landed — retire the
                                    // TTFT deadline window for all subsequent
                                    // reads (don't kill a slow-but-working stream).
                                    if !*got_first_token {
                                        tracing::debug!(
                                            ttft_ms = %ttft_start.elapsed().as_millis(),
                                            "API stream: first token (content)"
                                        );
                                    }
                                    *got_first_token = true;
                                    on_chunk(&piece);
                                    full_content.push_str(&piece);
                                    // §11.43 kill switch: scan the rolling
                                    // tail-buffer for a mechanical loop. On
                                    // hit, finalize by truncating the FULL
                                    // turn content at the loop's 2nd
                                    // occurrence and break BOTH loops (inner
                                    // line-parse + outer chunk-read). The
                                    // detector's tail buffer is a DETECTION
                                    // window only — finalizing from it would
                                    // silently drop all pre-tail prose
                                    // (2026-08-15 audit fix).
                                    // Dropping `response` (the moved owner
                                    // of `stream`) at function return severs
                                    // the TCP connection.
                                    if rep_guard.push(&piece) {
                                        // (2026-08-15 audit fix) Forensics
                                        // contract parity with the local
                                        // engines: raw_output carries the
                                        // FULL un-truncated turn (the loop
                                        // evidence); content carries the
                                        // truncated clean prose. The caller
                                        // prefers out.raw when non-empty.
                                        let raw_forensic = full_content.clone();
                                        let truncated = crate::stream_filter::truncate_repetition(
                                            full_content,
                                        );
                                        return LineOutcome::RepKill(ParsedOutput {
                                            content: truncated,
                                            reasoning: String::new(),
                                            raw: raw_forensic,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    LineOutcome::Next
                };

            loop {
                // The server signalled end-of-stream ([DONE]) — stop reading.
                if stream_done {
                    break;
                }
                // Honor cancel: stop reading + return what we have so far.
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let chunk_res = if got_first_token {
                    // Stream is alive — bound only the idle gap between chunks,
                    // RACED against cancel (2026-08-16 audit fix: a stop during
                    // a between-chunks stall used to lag the full 120s window —
                    // cancel was only checked at loop top between reads, so a
                    // dead-air stream held the stop hostage until the stall
                    // guard fired and finalized as api_lost instead of the
                    // requested soft cancel).
                    let read = tokio::time::timeout(idle_dur, stream.next());
                    tokio::select! {
                        res = read => match res {
                            Ok(opt) => opt,
                            Err(_elapsed) => {
                                buffer.clear();
                                drop(stream);
                                return Err(anyhow::anyhow!(
                                    "API_TIMEOUT: stream stalled for over {}ms after the first token — connection dead",
                                    crate::settings::API_CHUNK_IDLE_TIMEOUT_MS
                                ));
                            }
                        },
                        _ = cancel_poll(&cancel) => {
                            // Cancel observed mid-wait — fall through to the
                            // loop-top cancel check's break with partials.
                            continue;
                        }
                    }
                } else {
                    // Still waiting for the first token — the ABSOLUTE
                    // deadline bounds the whole window (armed before `send()`,
                    // so it also covered the request phase; what remains here
                    // is the post-headers remainder), raced against cancel
                    // so a stop is honored within ~100ms, not 10s.
                    let read = tokio::time::timeout_at(ttft_deadline, stream.next());
                    tokio::select! {
                        res = read => match res {
                            Ok(opt) => opt,
                            Err(_elapsed) => {
                                // Deadline elapsed with no first token. Drop
                                // the stream to sever TCP, return the sentinel
                                // error. `chat_send` branches on the
                                // `API_TIMEOUT` prefix.
                                buffer.clear();
                                drop(stream);
                                return Err(anyhow::anyhow!(
                                    "API_TIMEOUT: no first token within {}ms — provider hung on request",
                                    crate::settings::API_FIRST_TOKEN_TIMEOUT_MS
                                ));
                            }
                        },
                        _ = cancel_poll(&cancel) => {
                            // Cancel observed mid-wait — fall through to the
                            // loop-top cancel check's break with partials.
                            continue;
                        }
                    }
                };
                let Some(chunk_res) = chunk_res else { break };
                let bytes = chunk_res.map_err(|e| anyhow::anyhow!("SSE read error: {e}"))?;
                buffer.extend_from_slice(&bytes);

                // Process complete lines. Keep any trailing partial line in
                // buffer. (2026-08-16 audit fix) CR-aware framing: the SSE
                // spec permits \n, \r\n, AND bare \r as line terminators — the
                // old splitter only recognized \n (\r\n worked only because
                // `.trim()` ate the residual \r; a bare-\r stream never
                // framed a single line).
                while let Some(sep_pos) = buffer.iter().position(|&b| b == b'\n' || b == b'\r') {
                    let mut end = sep_pos + 1;
                    if buffer[sep_pos] == b'\r' && buffer.get(end) == Some(&b'\n') {
                        end += 1;
                    }
                    let line = String::from_utf8_lossy(&buffer[..sep_pos]).trim().to_string();
                    drop(buffer.drain(..end));
                    match process_line(&mut full_content, &mut got_first_token, &line) {
                        LineOutcome::Next => {}
                        LineOutcome::Done => {
                            stream_done = true;
                            buffer.clear();
                            break;
                        }
                        LineOutcome::Fail(msg) => {
                            buffer.clear();
                            drop(stream);
                            return Err(anyhow::anyhow!("API provider error (in-band): {msg}"));
                        }
                        LineOutcome::RepKill(out) => {
                            buffer.clear();
                            drop(stream);
                            return Ok(out);
                        }
                    }
                }
            }

            // EOF tail flush (2026-08-16 audit fix): the server closed the
            // body with a final line that never received its terminator.
            // Parse it through the same path instead of dropping it.
            if !buffer.is_empty() {
                let line = String::from_utf8_lossy(&buffer).trim().to_string();
                buffer.clear();
                match process_line(&mut full_content, &mut got_first_token, &line) {
                    LineOutcome::Next | LineOutcome::Done => {}
                    LineOutcome::Fail(msg) => {
                        drop(stream);
                        return Err(anyhow::anyhow!("API provider error (in-band): {msg}"));
                    }
                    LineOutcome::RepKill(out) => {
                        drop(stream);
                        return Ok(out);
                    }
                }
            }

            Ok(ParsedOutput {
                content: full_content,
                reasoning: String::new(),
                raw: String::new(),
            })
        })
    }

    fn is_ready(&self) -> bool {
        // An HttpBackend exists only when a profile is connected, so it's
        // always ready to stream (the network call itself will surface any
        // connectivity error at stream time).
        true
    }
}

/// The loaded model, as loaded from disk. This is an intermediate value: it
/// exists only between `load_blocking` and `into_static`, after which the model
/// is leaked to `&'static` and handed to the engine. The backend is NOT owned
/// here; it's the process-wide singleton from `shared_backend`.
pub struct LlamaModelHandle {
    model: LlamaModel,
    family: ModelFamily,
}

unsafe impl Send for LlamaModelHandle {}
unsafe impl Sync for LlamaModelHandle {}

impl LlamaModelHandle {
    /// Leak the model to a `&'static` reference so the engine can own a
    /// `LlamaContext<'static>`. See the module docs for the rationale.
    /// The shared backend ref is returned alongside it (already `&'static`).
    ///
    /// The family is returned by value (it's `Copy`).
    #[must_use]
    fn into_static(self) -> (&'static LlamaBackend, &'static LlamaModel, ModelFamily) {
        let backend_ref: &'static LlamaBackend = shared_backend();
        let model_ref: &'static LlamaModel = Box::leak(Box::new(self.model));
        (backend_ref, model_ref, self.family)
    }
}

/// The backend façade. Holds a handle to the engine thread (or `None` while
/// loading). Fully `Send`/`Sync`: no `LlamaContext` or `!Send` type crosses
/// out of the engine thread.
pub struct LlamaCppBackend {
    engine: Arc<std::sync::Mutex<Option<ChatEngine>>>,
}

/// Process-level slot for the leaked `LlamaModel`.
///
/// **Phase 5B (2026-07-29) — the weight-unload lift.** Was a
/// `OnceLock<&'static LlamaModel>` (set once, never clearable — `Box::leak`
/// memory cannot be reclaimed). The LLM⇄SD VRAM swap requires the weights to
/// UNLOAD so SD can use VRAM, so the slot is now a `RwLock<Option<*mut>>`
/// holding the **raw leaked pointer**. The pointer can be reclaimed via
/// `Box::from_raw` on unload (freeing VRAM) + re-leaked on reload.
///
/// `shared_model()` reconstitutes the `&'static LlamaModel` ref from the raw
/// pointer — **same signature, same lifetime** — so the three consumers
/// (`spawn_from_shared`, `fable_engine`, `schema_engine`) compile unchanged.
/// The `&'static` is honest as long as the model is resident: the pointer
/// stays valid from `reload`/first-boot until `unload`. Callers must NOT hold
/// the `&'static` across an `unload` (use-after-free) — the swap-lock's
/// teardown joins every engine's `LlamaContext` BEFORE `unload_shared_model`
/// runs, so no `&'static` ref outlives the model. This ordering invariant is
/// load-bearing + enforced by the SD teardown path, not assumed.
///
/// The stored pointer is `*mut` (not `&'static`) so `Box::from_raw` can
/// reclaim it. `*mut LlamaModel` is `!Send`/`!Sync` by default; we assert the
/// module-level invariant that access is serialized through the `RwLock` (the
/// raw pointer never escapes except as a reconstituted `&'static` that lives
/// only while the slot is `Some`). The unsafe impl is the same rationale as
/// the existing `LlamaModelHandle` Send/Sync.
static SHARED_MODEL: std::sync::RwLock<Option<SharedModelPtr>> =
    std::sync::RwLock::new(None);

/// Newtype around the leaked-model pointer so the `Send` impl satisfies the
/// orphan rules (foreign types like `NonNull` can't have a local `Send` impl
/// directly). Same safety rationale as the previous bare-NonNull impl below.
#[derive(Clone, Copy)]
struct SharedModelPtr(std::ptr::NonNull<LlamaModel>);

// SAFETY: SHARED_MODEL holds a raw pointer to a leaked LlamaModel. Access is
// serialized through the RwLock. The pointer is only dereferenced (reconstituted
// to &'static) while the write-lock guards against concurrent unload. The
// underlying LlamaModel is Sync (llama-cpp-2 declares it so). The only hazard
// is use-after-unload, prevented by the swap-lock ordering invariant (all
// LlamaContexts joined before unload_shared_model). This mirrors the existing
// SHARED_BACKEND + LlamaModelHandle unsafe rationales.
unsafe impl Send for SharedModelPtr {}
unsafe impl Sync for SharedModelPtr {}

/// Free function: the leaked `&'static LlamaModel`, available after the chat
/// backend finishes loading. Used by the schema delta engine + the fable engine
/// to create an isolated `LlamaContext` on the same model. Returns `None` if
/// the model hasn't loaded yet OR has been unloaded (the Phase 5B LLM⇄SD swap).
///
/// **Lifetime honesty:** the returned `&'static` is valid only while the model
/// is resident in `SHARED_MODEL`. The caller must NOT retain it across an
/// `unload_shared_model()` call (use-after-free). The swap-lock enforces this:
/// every engine's `LlamaContext` (which holds the `&'static`) is joined before
/// the SD teardown calls `unload_shared_model`.
pub fn shared_model() -> Option<&'static LlamaModel> {
    let g = SHARED_MODEL.read().ok()?;
    let ptr = (*g)?;
    let nn = ptr.0;
    // SAFETY: the pointer is a valid leaked Box<LlamaModel> that stays valid
    // until unload_shared_model reclaims it. The caller's LlamaContext is
    // joined before any unload (the swap-lock invariant), so this &'static ref
    // cannot outlive the allocation. Reconstituting &'static from the raw
    // pointer is honest for the pointer's lifetime.
    Some(unsafe { nn.as_ref() })
}

/// Phase 5B (2026-07-29): unload the shared LLM weights from VRAM. Reclaims
/// the leaked `Box<LlamaModel>` via `Box::from_raw` + drops it → frees ~5.8GB.
/// After this, `shared_model()` returns `None` until `reload_shared_model`
/// re-leaks.
///
/// **LOAD-BEARING PRECONDITION:** the caller MUST guarantee no `LlamaContext`
/// borrowing the model is alive when this runs. The swap-lock's teardown joins
/// every engine (chat/schema/fable) before the SD role's teardown fires, so by
/// the time this is called from an SD teardown, all LLM contexts are dropped.
/// Violating this is use-after-free. Returns `true` if a model was unloaded,
/// `false` if the slot was already empty (idempotent).
///
/// Takes a write lock; blocks `shared_model()` readers for the duration of the
/// drop (~instant — llama.cpp's LlamaModel Drop frees VRAM synchronously).
pub fn unload_shared_model() -> bool {
    let mut g = match SHARED_MODEL.write() {
        Ok(g) => g,
        Err(e) => {
            tracing::error!(error = %e, "unload_shared_model: SHARED_MODEL lock poisoned; aborting unload");
            return false;
        }
    };
    let ptr = g.take();
    drop(g); // release the write lock BEFORE the drop (the drop itself is fine,
             // but holding the lock through a ~5.8GB free is unnecessary).
    if let Some(wrapped) = ptr {
        // Unwrap the newtype; SAFETY as below.
        let nn = wrapped.0;
        // SAFETY: the pointer came from Box::leak(Box::new(model)) in
        // set_shared_model. Reclaiming it via Box::from_raw + dropping frees
        // the VRAM. The precondition guarantees no LlamaContext holds a borrow.
        let boxed: Box<LlamaModel> = unsafe { Box::from_raw(nn.as_ptr()) };
        drop(boxed);
        tracing::info!("shared model unloaded (VRAM freed for SD swap)");
        true
    } else {
        false
    }
}

/// Phase 5B (2026-07-29): reload the shared LLM weights into VRAM after an SD
/// swap. Re-runs `load_blocking` + re-leaks the model into the slot. After
/// this returns Ok, `shared_model()` is live again + the engines can spawn
/// fresh `LlamaContext`s on it.
///
/// Returns the model family (Gemma4 for WUPI.gguf — LOCKED per AGENTS.md §10).
/// On error, the slot stays empty (the caller must surface the failure —
/// typically by disabling auto-gen + keeping the user notified; the
/// one-strike latch handles this).
pub fn reload_shared_model(path: &std::path::Path, n_gpu_layers: u32) -> anyhow::Result<ModelFamily> {
    // load_blocking owns its own LlamaModel (no shared-state borrow). Safe to
    // run while the slot is empty (the SD teardown already freed it).
    let handle = LlamaCppBackend::load_blocking(path, n_gpu_layers)?;
    let model = handle.model;
    let family = handle.family;
    // Re-leak to &'static via Box::leak, storing the raw pointer for a future
    // unload. Mirrors into_static's leak but threads the pointer into the slot.
    let leaked: &'static LlamaModel = Box::leak(Box::new(model));
    let ptr = std::ptr::NonNull::new(leaked as *const LlamaModel as *mut LlamaModel)
        .expect("Box::leak returns a non-null pointer");
    let mut g = SHARED_MODEL.write().map_err(|e| anyhow::anyhow!("SHARED_MODEL lock poisoned: {e}"))?;
    // Expect-empty overwrite: a still-resident prior model must be RECLAIMED,
    // not silently replaced — the old behavior orphaned the prior leaked Box
    // (~5.8GB leak until process exit) if two reload paths ever raced. The
    // SD cycle is serialized under the local-model turn lock so this branch
    // is a should-never-happen backstop; reclaiming with a loud log is the
    // lesser evil vs leaking (mirrors unload_shared_model's SAFETY).
    if let Some(prior) = g.replace(SharedModelPtr(ptr)) {
        tracing::error!("reload_shared_model: prior model was still resident — reclaiming it (leak prevented; investigate the caller's unload ordering)");
        // SAFETY: the pointer came from the same Box::leak path in
        // set_shared_model/reload_shared_model; reclaiming frees the VRAM.
        let boxed: Box<LlamaModel> = unsafe { Box::from_raw(prior.0.as_ptr()) };
        drop(boxed);
    }
    tracing::info!(path = %path.display(), "shared model reloaded after SD swap");
    Ok(family)
}

/// Internal: store a freshly-leaked model into the slot. Called by
/// `spawn_load` at first boot (replacing the old `SHARED_MODEL.set(model_ref)`).
/// If a model is somehow already resident (shouldn't happen — first boot
/// only), the prior one is RECLAIMED + error-logged — the same discipline
/// as `reload_shared_model` (2026-08-20 audit L8: the old path silently
/// leaked the resident model where the doc claimed a log).
fn set_shared_model(model_ref: &'static LlamaModel) {
    let ptr = std::ptr::NonNull::new(model_ref as *const LlamaModel as *mut LlamaModel)
        .expect("model ref is non-null");
    let mut g = SHARED_MODEL.write().expect("SHARED_MODEL lock not poisoned at first boot");
    if let Some(prior) = g.replace(SharedModelPtr(ptr)) {
        tracing::error!("set_shared_model: prior model was still resident — reclaiming it (leak prevented; investigate the caller's load ordering)");
        // SAFETY: the pointer came from the same Box::leak path in
        // set_shared_model/reload_shared_model; reclaiming frees the VRAM.
        let boxed: Box<LlamaModel> = unsafe { Box::from_raw(prior.0.as_ptr()) };
        drop(boxed);
    }
}

impl LlamaCppBackend {
    /// Load the model off-thread, then spawn the persistent engine. Returns
    /// immediately with a backend handle; `on_result` fires when loading +
    /// engine init completes (success or failure).
    ///
    /// `context_size` fixes the `n_ctx` of the persistent context. It cannot
    /// change without re-spawning the engine: that's a future P concern
    /// (settings hot-reload).
    pub fn spawn_load(
        path: PathBuf,
        n_gpu_layers: u32,
        context_size: u32,
        on_result: Box<dyn FnOnce(Result<String, String>) + Send>,
    ) -> Arc<Self> {
        let engine_slot: Arc<std::sync::Mutex<Option<ChatEngine>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_clone = Arc::clone(&engine_slot);

        std::thread::spawn(move || match Self::load_blocking(&path, n_gpu_layers) {
            Ok(handle) => {
                tracing::info!("model loaded from {}", path.display());
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("model")
                    .to_string();

                // Leak the model to &'static (the backend is already the
                // process-wide &'static from shared_backend). The engine thread
                // owns both for the process lifetime.
                let (backend_ref, model_ref, family) = handle.into_static();

                // Stash the model ref in the process-level slot so the schema
                // delta engine + fable engine can create isolated contexts on
                // the same model. Phase 5B: set_shared_model replaces the old
                // OnceLock::set (the slot is now a reloadable RwLock<Option<*mut>>
                // so the LLM⇄SD swap can unload it).
                set_shared_model(model_ref);

                // Spawn the persistent engine with Q8_0 KV cache + delta prefill.
                let (engine, init_rx) =
                    ChatEngine::spawn(backend_ref, model_ref, family, context_size);

                // Bug #6: await engine init confirmation BEFORE signaling
                // readiness. We're already on a background thread, so
                // blocking here doesn't stall the UI. If init_runtime failed
                // (CUDA context alloc error, etc.), report the error instead
                // of falsely claiming "ready".
                match init_rx.recv() {
                    Ok(Ok(())) => {
                        {
                            let mut g = slot_clone.lock().expect("engine mutex");
                            *g = Some(engine);
                        }
                        on_result(Ok(name));
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "engine init failed");
                        on_result(Err(e));
                    }
                    Err(_) => {
                        let msg = "engine init channel closed unexpectedly".to_string();
                        tracing::error!(error = %msg);
                        on_result(Err(msg));
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "model load failed");
                on_result(Err(format!("{e}")));
            }
        });

        Arc::new(LlamaCppBackend {
            engine: engine_slot,
        })
    }

    fn load_blocking(path: &Path, n_gpu_layers: u32) -> anyhow::Result<LlamaModelHandle> {
        use llama_cpp_2::model::params::LlamaModelParams;
        let backend: &'static LlamaBackend = shared_backend();

        let params = LlamaModelParams::default().with_n_gpu_layers(n_gpu_layers);
        let model = LlamaModel::load_from_file(backend, path, &params)
            .map_err(|e| anyhow::anyhow!("model load: {e:?}"))?;

        let filename = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let family = ModelFamily::from_model_name(filename);
        tracing::info!(family = ?family, filename, "detected model family");

        Ok(LlamaModelHandle {
            model,
            family,
        })
    }

    /// v0.6.4 VRAM swap-lock: re-spawn the chat backend WITHOUT re-reading
    /// the model file. Reuses the leaked `&'static LlamaModel` from
    /// `shared_model()` (set once at boot, never cleared) +
    /// `shared_backend()`. Only the engine thread + its KV context are
    /// recreated — ~instant vs the ~5s a full `spawn_load` (file read)
    /// would cost on every chat turn after a fable/schema eviction.
    ///
    /// Used by `chat_send` when the chat lease is acquired but the backend
    /// slot is `None` (torn down by a prior fable/schema eviction). The
    /// weights never left VRAM (the leak is process-lifetime), so this is
    /// just a fresh `LlamaContext` allocation on the shared model.
    ///
    /// Mirrors `spawn_load`'s exact contract: returns a backend handle
    /// immediately; `on_result` fires when the engine thread + context
    /// are live (success) or failed (error). The caller (chat_send) does
    /// NOT need to await — it stashes the backend in `AppState.backend`
    /// right away, and `stream()` checks the internal slot (returns
    /// "not ready yet" if called before init completes, which the caller
    /// avoids by awaiting `on_result` via a oneshot). In practice
    /// `chat_send` awaits readiness the same way `boot_load_model` does.
    ///
    /// Returns `None` if `shared_model()` is `None` (boot hasn't loaded
    /// the chat model yet — should never happen by the time chat_send
    /// runs, but defensive). `context_size` fixes the new context's n_ctx.
    pub fn spawn_from_shared(
        context_size: u32,
        on_result: Box<dyn FnOnce(Result<String, String>) + Send>,
    ) -> Option<Arc<Self>> {
        let model_ref: &'static LlamaModel = shared_model()?;
        let backend_ref: &'static LlamaBackend = shared_backend();
        // The leaked model was already classified at first boot load; the
        // chat model is always WUPI.gguf → Gemma4 (LOCKED per AGENTS.md §10).
        let family = ModelFamily::Gemma4;

        let engine_slot: Arc<std::sync::Mutex<Option<ChatEngine>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_clone = Arc::clone(&engine_slot);

        std::thread::spawn(move || {
            let (engine, init_rx) =
                ChatEngine::spawn(backend_ref, model_ref, family, context_size);
            // Bug #6 pattern: await init confirmation BEFORE stashing +
            // signaling. If init_runtime failed (CUDA context alloc error,
            // etc.), report the error instead of falsely stashing a dead
            // engine.
            match init_rx.recv() {
                Ok(Ok(())) => {
                    {
                        let mut g = slot_clone.lock().expect("engine mutex");
                        *g = Some(engine);
                    }
                    tracing::info!("chat backend re-spawned from shared model (no file read)");
                    on_result(Ok("WUPI.gguf (shared)".to_string()));
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "chat backend re-spawn init failed");
                    on_result(Err(e));
                }
                Err(_) => {
                    let msg = "chat backend re-spawn init channel closed".to_string();
                    tracing::error!(error = %msg);
                    on_result(Err(msg));
                }
            }
        });

        Some(Arc::new(LlamaCppBackend {
            engine: engine_slot,
        }))
    }
}

impl GenerationClient for LlamaCppBackend {
    fn stream(
        &self,
        messages: Vec<ApiMessage>,
        memory_block: Option<String>,
        world_state: Option<String>,
        tools: Vec<crate::chat_format::ToolSpec>,
        _context_size: u32,
        on_chunk: ChunkFn,
        cancel: CancelToken,
    ) -> StreamFuture {
        let engine = Arc::clone(&self.engine);
        Box::pin(async move {
            let (reply_tx, reply_rx) = std::sync::mpsc::channel::<EngineReply>();
            {
                let guard = engine.lock().expect("engine mutex");
                let eng = guard
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("model not loaded yet"))?;
                eng.request(EngineRequest {
                    messages,
                    on_chunk,
                    cancel,
                    memory_block,
                    world_state,
                    tools,
                    reply: reply_tx,
                })
                .map_err(|e| anyhow::anyhow!(e))?;
            }

            // Await the reply off the async runtime: generation takes seconds
            // and we must not block a tokio worker. The engine streams chunks
            // directly to `on_chunk` (the Tauri Channel) while we wait.
            let reply = tokio::task::spawn_blocking(move || reply_rx.recv())
                .await
                .map_err(|e| anyhow::anyhow!("join: {e}"))?
                .map_err(|_| anyhow::anyhow!("engine reply channel closed"))?;

            match reply {
                EngineReply::Ok(parsed) => Ok(parsed),
                EngineReply::Err(msg) => Err(anyhow::anyhow!(msg)),
            }
        })
    }

    fn is_ready(&self) -> bool {
        self.engine
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }
}

impl LlamaCppBackend {
    /// Shut down the engine thread + clear the slot. Posts `EngineMsg::Shutdown`
    /// AND blocks on the JoinHandle until the thread has fully exited + dropped
    /// its `EngineRuntime` (LlamaContext + the borrowed `&'static LlamaModel`
    /// → VRAM actually freed), then sets the inner slot to `None` so further
    /// `stream()` calls return the "not ready" error instead of posting to a
    /// dead thread. The synchronous join is load-bearing during model swaps -
    /// the old fire-and-forget version raced VRAM teardown and OOM'd the next
    /// `load_from_file` (the 2026-07-18 VRAM-overlap diagnosis). Callers
    /// using this from an async context should wrap it in `spawn_blocking`.
    pub fn shutdown(&self) {
        if let Some(engine) = self.engine.lock().map(|mut g| g.take()).unwrap_or(None) {
            engine.shutdown();
            tracing::info!("chat engine shutdown complete (thread joined + context dropped)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: impl Into<String>) -> ChatRequestMessage {
        ChatRequestMessage {
            role: role.to_string(),
            content: content.into(),
        }
    }

    #[test]
    fn truncate_noop_when_under_budget() {
        let mut msgs = vec![msg("system", "sys"), msg("user", "hi"), msg("assistant", "hello")];
        let total: usize = msgs.iter().map(|m| m.content.chars().count()).sum();
        truncate_to_budget(&mut msgs, total + 100);
        assert_eq!(msgs.len(), 3, "nothing should be dropped when under budget");
    }

    #[test]
    fn truncate_drops_oldest_non_system_first() {
        // system(3) + user(2) + assistant(5) + user(2) = 12 chars. Budget = 7.
        // Should drop the first user (2) → 10, then the assistant (5) → 5 ≤ 7.
        // Result: [system, user(last)].
        let mut msgs = vec![
            msg("system", "sys"),
            msg("user", "hi"),
            msg("assistant", "hello"),
            msg("user", "yo"),
        ];
        truncate_to_budget(&mut msgs, 7);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "sys");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "yo", "the most-recent non-system message must survive");
    }

    #[test]
    fn truncate_preserves_system_even_if_over_budget() {
        // System alone exceeds budget: must NOT be dropped.
        let mut msgs = vec![msg("system", "x".repeat(100))];
        truncate_to_budget(&mut msgs, 10);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
    }

    #[test]
    fn truncate_keeps_at_least_system_plus_last() {
        // Even if wildly over budget, never drop below system + last message.
        let mut msgs = vec![
            msg("system", "sys"),
            msg("user", "x".repeat(50)),
            msg("assistant", "y".repeat(50)),
            msg("user", "z".repeat(50)),
        ];
        truncate_to_budget(&mut msgs, 5);
        assert_eq!(msgs.len(), 2, "system + last must survive");
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].content, "z".repeat(50));
    }

    #[test]
    fn truncate_no_system_message_works() {
        // No system message at index 0: protect nothing special, but still
        // keep at least the last message.
        let mut msgs = vec![
            msg("user", "x".repeat(50)),
            msg("assistant", "y".repeat(50)),
            msg("user", "z".repeat(50)),
        ];
        truncate_to_budget(&mut msgs, 55);
        assert!(msgs.len() >= 1);
        assert_eq!(msgs.last().unwrap().content, "z".repeat(50));
    }

    // ─────────────────────────────────────────────────────────────────────
    // §11.43 — Stream-abort kill switch integration tests.
    //
    // These verify the END-TO-END wiring inside `HttpBackend::stream`: that
    // the SSE loop correctly invokes `StreamRepetitionDetector::push` on
    // every chunk, breaks the stream on a confirmed loop, drops the response
    // (severing the TCP connection), and returns the truncated clean prose.
    // The detector itself has 11 unit tests in `stream_filter.rs`; these
    // tests cover the WIRING (the part that's unique to `HttpBackend`).
    //
    // Approach: spin up a sync mock HTTP/SSE server on an ephemeral port via
    // `std::net::TcpListener` (no Cargo.toml change — reqwest doesn't care
    // if the server side is sync). The mock speaks just enough HTTP to fool
    // reqwest's response parser: a status line + the right headers + a
    // streaming SSE body. The body is pre-scripted per test.
    // ─────────────────────────────────────────────────────────────────────

    /// Minimal HTTP/SSE mock server. Listens on an ephemeral port, accepts
    /// ONE connection, writes the provided SSE body bytes (which may include
    /// artificial delays between lines to simulate token-by-token streaming),
    /// then closes. Returns the URL the client should connect to.
    ///
    /// The server is sync (runs on its own OS thread); reqwest connects to it
    /// fine because TCP is transport-agnostic. The mock counts how many body
    /// bytes it actually wrote before the client closed the connection — that
    /// count lets tests assert "the connection was severed before the full
    /// body was sent" (the kill switch's TCP-abort behavior).
    struct MockSseServer {
        url: String,
        /// JoinHandle for the server thread. The thread exits after one
        /// client connection completes (or after the body is fully written,
        /// whichever comes first).
        _handle: std::thread::JoinHandle<()>,
        /// Shared counter for how many body bytes were written before the
        /// client closed the connection. Read via `Arc<AtomicUsize>` after
        /// the test completes + a brief sleep.
        bytes_written: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MockSseServer {
        /// Spin up a mock serving the given SSE lines. `lines` are the full
        /// `data: {...}` payloads (without the trailing `\n`); the server
        /// joins them with `\n\n` (SSE framing) + writes each with a small
        /// delay so the client has time to process + abort mid-stream.
        fn spawn(lines: Vec<String>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("local_addr").port();
            let url = format!("http://127.0.0.1:{port}/chat/completions");
            let bytes_written = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let bw_clone = bytes_written.clone();
            let handle = std::thread::spawn(move || {
                // Accept one connection (blocking). The TcpListener drops on
                // thread exit — that's fine, we only need it for the accept.
                if let Ok((mut stream, _addr)) = listener.accept() {
                    use std::io::Write;
                    // Read + discard the client's HTTP request (we don't
                    // care about its content; just drain it so reqwest's
                    // request completes before we write the response).
                    let mut buf = [0u8; 4096];
                    let _ = std::io::Read::read(&mut stream, &mut buf);

                    // Write a minimal HTTP response with `text/event-stream`
                    // so reqwest routes it through `bytes_stream()`.
                    let header = concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "Content-Type: text/event-stream\r\n",
                        "Cache-Control: no-cache\r\n",
                        "Connection: close\r\n",
                        "\r\n",
                    );
                    let _ = stream.write_all(header.as_bytes());
                    bw_clone.fetch_add(
                        header.len(),
                        std::sync::atomic::Ordering::Relaxed,
                    );

                    // Stream each SSE line with a tiny delay so the client
                    // can process + abort mid-stream (otherwise on a fast
                    // localhost connection the kernel may buffer everything
                    // before reqwest starts reading).
                    for line in lines {
                        let frame = format!("{line}\n\n");
                        let frame_bytes = frame.as_bytes();
                        let total = frame_bytes.len();
                        // Write in 16-byte chunks with 5ms sleeps so the
                        // kill switch has time to fire mid-frame (real API
                        // streams token-by-token, not line-by-line).
                        let mut written = 0;
                        while written < total {
                            let end = (written + 16).min(total);
                            if stream.write_all(&frame_bytes[written..end]).is_err() {
                                // Client closed — record bytes written so far.
                                let cumulative = bw_clone.load(std::sync::atomic::Ordering::Relaxed);
                                let _ = std::io::Write::flush(&mut stream);
                                // The cumulative count already includes what
                                // prior iterations wrote; add this partial.
                                bw_clone.store(
                                    cumulative + written,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                return;
                            }
                            written = end;
                            let _ = std::io::Write::flush(&mut stream);
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        bw_closed_fetch_add(&bw_clone, total);
                    }
                    // Body fully written; connection closed on thread exit.
                }
            });
            Self {
                url,
                _handle: handle,
                bytes_written,
            }
        }

        /// How many body bytes the server wrote before the client closed.
        /// Tests assert this is LESS than the full body size when the kill
        /// switch fires (proves the TCP connection was severed).
        fn bytes_written(&self) -> usize {
            self.bytes_written.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    // Helper to side-step borrow-checker confusion in the loop (inlined
    // atomic add with Relaxed ordering).
    fn bw_closed_fetch_add(
        c: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
        n: usize,
    ) {
        c.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }

    /// Build an `HttpBackend` pointed at the mock server. The api_key is
    /// irrelevant (the mock doesn't check it) but must be non-empty so the
    /// header-insert path doesn't skip the auth header.
    fn backend_for(url: &str) -> HttpBackend {
        let profile = crate::api::ApiProfile {
            id: "test".into(),
            name: "Test".into(),
            endpoint: url.into(),
            model: "test-model".into(),
            api_key: "sk-test".into(),
            temperature: Some(0.85),
            max_context: Some(8192),
        };
        HttpBackend::new(profile)
    }

    /// Helper to encode a content delta chunk. The OpenAI streaming shape:
    /// `{"choices":[{"delta":{"content":"..."}}]}`.
    fn sse_chunk(content: &str) -> String {
        let escaped = serde_json::to_string(content).unwrap_or_else(|_| "\"\"".into());
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{escaped}}}}}]}}")
    }

    #[tokio::test]
    async fn stream_abort_kills_smuggler_loop_mid_stream() {
        // THE canonical test: a normal lead-in, then the smuggler-loop body.
        // The kill switch MUST fire on the third occurrence of the looped
        // phrase, return the truncated clean prose (one instance + lead-in),
        // and break the stream loop (chunks after the kill don't reach the
        // final content or the on_chunk callback).
        let lead_in = "Mara nods. ";
        let loop_phrase = "The smuggler turns and runs away. ";
        // 5 repeats — well past the 3-repeat threshold.
        let mut lines = vec![sse_chunk(lead_in)];
        for _ in 0..5 {
            lines.push(sse_chunk(loop_phrase));
        }

        let server = MockSseServer::spawn(lines);
        let backend = backend_for(&server.url);

        // Count how many chunks the on_chunk callback receives. The kill
        // switch fires at the 3rd occurrence, so we expect callbacks for:
        //   lead-in (1) + occurrence 1 + occurrence 2 + occurrence 3 (the
        // trigger chunk) = 4 chunk callbacks. NOT 6 — the 4th + 5th loop
        // chunks must never reach the callback because the stream broke.
        let chunk_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = chunk_count.clone();
        let on_chunk: ChunkFn = std::sync::Arc::new(move |_: &str| {
            cc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let messages = vec![crate::session::ApiMessage {
            role: "system".into(),
            content: "test".into(),
            raw_output: String::new(),
        }];

        let result = backend
            .stream(messages, None, None, Vec::new(), 1024, on_chunk, cancel)
            .await
            .expect("stream should complete (not error)");

        // ── Assertion 1: the final content is the truncated clean prose ──
        // The kill switch must keep the lead-in + ONE instance of the looped
        // phrase, then drop the rest. `truncate_repetition` on the same input
        // would produce the same string — single source of truth.
        let expected = crate::stream_filter::truncate_repetition(
            &format!("{lead_in}{loop_phrase}{loop_phrase}{loop_phrase}{loop_phrase}{loop_phrase}"),
        );
        assert_eq!(
            result.content, expected,
            "stream-abort must return the same truncated prose as the post-gen truncator"
        );
        // The expected prose contains ONE instance of the loop phrase, not five.
        assert_eq!(
            result.content.matches("The smuggler turns and runs away").count(),
            1,
            "truncated prose must contain exactly one instance of the looped phrase"
        );
        // Lead-in is preserved.
        assert!(
            result.content.starts_with(lead_in),
            "lead-in prose must be preserved in truncated output"
        );
        // (2026-08-15 audit fix pin, corrected 2026-08-16) Forensics parity
        // with the local engines: raw carries the FULL UN-TRUNCATED content
        // RECEIVED before the sever — at kill time that is the lead-in +
        // the 3 occurrences the detector needed to confirm the loop (the
        // 4th + 5th chunks never arrive — the stream is severed, which is
        // the entire point). The original pin demanded 5 occurrences, which
        // contradicts its own Assertion 2 (≤4 chunk callbacks); it was
        // committed without ever running the test binary.
        assert!(
            !result.raw.is_empty()
                && result.raw.matches("The smuggler turns and runs away").count() == 3,
            "kill-switch raw_output must keep the full un-truncated received turn as forensics"
        );
        // Raw is strictly MORE than the truncated prose (the loop evidence).
        assert!(
            result.raw.len() > result.content.len(),
            "raw forensics must exceed the truncated content"
        );

        // ── Assertion 2: the stream broke at the trigger chunk ──
        // 4 chunk callbacks = lead-in + 3 occurrences (the trigger chunk
        // counts: the kill switch fires AFTER processing the trigger chunk's
        // content). If we see 5 or 6 callbacks, the kill switch did NOT
        // break the stream — it only truncated the final string after
        // reading everything (defeating the token-billing-abort contract).
        let observed = chunk_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            observed <= 4,
            "kill switch must break the stream after the 3rd occurrence — saw {observed} chunk \
             callbacks (expected ≤4: lead-in + 3 loop chunks). If 5+, the stream wasn't actually \
             severed mid-read; the kill switch is post-processing instead of mid-stream."
        );
        // And it definitely processed at least the 3 trigger chunks (sanity).
        assert!(
            observed >= 4,
            "kill switch must have processed the 3 trigger chunks before firing — saw {observed} \
             callbacks (expected ≥4)"
        );

        // Suppress unused warning on the mock server field.
        let _ = server.bytes_written();
    }

    #[tokio::test]
    async fn stream_abort_preserves_pre_tail_prose_on_long_lead_in() {
        // 2026-08-15 audit regression: a beat LONGER than the detector's
        // 200-word tail buffer that loops at the end must finalize with the
        // FULL lead-in intact. The old path replaced full_content with the
        // detector's tail-window truncation — the first ~150 words of good
        // narration silently vanished from the stored turn.
        let lead_in: String = (0..250)
            .map(|i| format!("lead{i} "))
            .collect::<Vec<_>>()
            .join("");
        let loop_phrase = "The smuggler turns and runs away. ";
        let mut lines = vec![sse_chunk(&lead_in)];
        for _ in 0..4 {
            lines.push(sse_chunk(loop_phrase));
        }

        let server = MockSseServer::spawn(lines);
        let backend = backend_for(&server.url);

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let on_chunk: ChunkFn = std::sync::Arc::new(|_: &str| {});
        let messages = vec![crate::session::ApiMessage {
            role: "system".into(),
            content: "test".into(),
            raw_output: String::new(),
        }];

        let result = backend
            .stream(messages, None, None, Vec::new(), 1024, on_chunk, cancel)
            .await
            .expect("stream should complete (not error)");

        // The pre-tail lead-in survived — the very first words of the beat
        // are still in the stored prose (the tail-buffer path dropped them).
        assert!(
            result.content.starts_with("lead0 lead1 lead2"),
            "pre-tail prose must survive the kill-switch finalization, got: {}…",
            result.content.chars().take(80).collect::<String>()
        );
        assert!(
            result.content.contains("lead249 "),
            "the tail-adjacent lead-in must also survive, got tail: {}…",
            result.content.chars().rev().take(80).collect::<String>()
        );
        // The loop is still cut to one instance.
        assert_eq!(
            result.content.matches("The smuggler turns and runs away").count(),
            1,
            "the looped phrase must appear exactly once"
        );
        // And the result matches running the post-gen truncator over the
        // full text directly (the single-source-of-truth contract).
        let expected = crate::stream_filter::truncate_repetition(&format!(
            "{lead_in}{loop_phrase}{loop_phrase}{loop_phrase}{loop_phrase}"
        ));
        assert_eq!(result.content, expected);
        let _ = server.bytes_written();
    }

    #[tokio::test]
    async fn stream_abort_passes_clean_prose_through_unchanged() {
        // NEGATIVE CONTROL: clean, varied prose must NOT trigger the kill
        // switch. The stream completes normally and the full content is
        // returned. This is the critical false-positive guard — if this
        // test fails, valid API narration is getting truncated mid-stream.
        let chunks = vec![
            "The tavern falls silent. ",
            "Mara wipes down the counter. ",
            "Rain begins to fall outside. ",
            "A stranger enters, dripping wet. ",
            "Nobody speaks for a long moment. ",
        ];
        let lines: Vec<String> = chunks.iter().map(|c| sse_chunk(c)).collect();
        let expected_full: String = chunks.iter().cloned().collect();

        let server = MockSseServer::spawn(lines);
        let backend = backend_for(&server.url);

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let on_chunk: ChunkFn = std::sync::Arc::new(|_: &str| {});
        let messages = vec![crate::session::ApiMessage {
            role: "system".into(),
            content: "test".into(),
            raw_output: String::new(),
        }];

        let result = backend
            .stream(messages, None, None, Vec::new(), 1024, on_chunk, cancel)
            .await
            .expect("stream should complete cleanly");

        assert_eq!(
            result.content, expected_full,
            "clean prose must pass through unchanged — kill switch must not fire"
        );
    }

    #[tokio::test]
    async fn stream_abort_preserves_two_repeat_anaphora() {
        // FALSE-POSITIVE GUARD: two repeats of a 4+ word phrase is rhetorical
        // anaphora, NOT a mechanical loop. The kill switch must NOT fire;
        // both instances must appear in the output. (Three repeats IS a loop
        // and would fire — that's covered by smuggler-loop test above.)
        let phrase = "The wind howls across the frozen moor. ";
        let chunks: Vec<&str> = vec![
            "The story begins. ",
            phrase,                              // occurrence 1
            phrase,                              // occurrence 2 (anaphora — OK)
            "Then silence falls. ",
        ];
        let lines: Vec<String> = chunks.iter().map(|c| sse_chunk(c)).collect();
        let expected_full: String = chunks.iter().cloned().collect();

        let server = MockSseServer::spawn(lines);
        let backend = backend_for(&server.url);

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let on_chunk: ChunkFn = std::sync::Arc::new(|_: &str| {});
        let messages = vec![crate::session::ApiMessage {
            role: "system".into(),
            content: "test".into(),
            raw_output: String::new(),
        }];

        let result = backend
            .stream(messages, None, None, Vec::new(), 1024, on_chunk, cancel)
            .await
            .expect("stream should complete");

        assert_eq!(
            result.content, expected_full,
            "two-repeat anaphora must NOT fire the kill switch"
        );
        assert_eq!(
            result.content.matches("The wind howls across the frozen moor").count(),
            2,
            "both anaphora instances must be preserved"
        );
    }
}
