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
/// Cancellation flag shared between `chat_send` and `chat_stop`. The engine's
/// decode loop checks this between tokens.
pub type CancelToken = Arc<std::sync::atomic::AtomicBool>;

pub trait GenerationClient: Send + Sync {
    fn stream(
        &self,
        messages: Vec<ApiMessage>,
        memory_block: Option<String>,
        world_state: Option<String>,
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
pub struct HttpBackend {
    profile: crate::api::ApiProfile,
    client: reqwest::Client,
}

impl HttpBackend {
    /// Construct from a saved profile. Builds a reqwest client with a generous
    /// timeout (generation can take minutes for long replies) + the bearer
    /// token pre-attached so every request on this client is authenticated.
    pub fn new(profile: crate::api::ApiProfile) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if !profile.api_key.is_empty() {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                profile.api_key
            )) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(300))
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
        _context_size: u32,
        on_chunk: ChunkFn,
        cancel: CancelToken,
    ) -> StreamFuture {
        let url = self.completions_url();
        let model = self.profile.model.clone();
        let temperature = self.profile.temperature;
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

            // v0.7: enforce the profile's max_context (default 8192) as a soft
            // input-token budget. We don't ship a real tokenizer to the API
            // path (the provider does its own counting server-side), so this
            // is a conservative safety net: estimate at ~4 chars/token and
            // drop the OLDEST non-system messages from the front until under
            // budget. The system message (index 0 if role=="system") is
            // always preserved — it carries OS_DIRECTIVES + persona +
            // memory/world_state. Mirrors the local engine's front-truncation
            // guard (engine.rs).
            let max_ctx = max_context.unwrap_or(8192);
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
            let body = serde_json::json!({
                "model": model,
                "messages": wire_messages,
                "stream": true,
                "temperature": temperature.unwrap_or(0.85),
                "top_p": 0.95,
            });

            let response = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("API request to {url} failed: {e}"))?;

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
            let mut buffer = String::new();
            let mut full_content = String::new();

            while let Some(chunk_res) = stream.next().await {
                // Honor cancel: stop reading + return what we have so far.
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let bytes = chunk_res.map_err(|e| anyhow::anyhow!("SSE read error: {e}"))?;
                // The chunk may not be UTF8-aligned at boundaries; lossy-convert
                // since SSE is ASCII-framed and the JSON payloads are UTF8.
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                // Process complete lines. Keep any trailing partial line in buffer.
                while let Some(newline_pos) = buffer.find('\n') {
                    let line = buffer[..newline_pos].trim().to_string();
                    buffer = buffer[newline_pos + 1..].to_string();
                    if line.is_empty() || !line.starts_with("data:") {
                        continue;
                    }
                    let data = line["data:".len()..].trim();
                    if data == "[DONE]" {
                        buffer.clear();
                        break;
                    }
                    // Parse the JSON chunk; on failure skip (some providers
                    // send keep-alive comments or partial events we don't care
                    // about). A malformed chunk must never kill the stream.
                    if let Ok(parsed) = serde_json::from_str::<ChatStreamChunk>(data) {
                        if let Some(choice) = parsed.choices.into_iter().next() {
                            if let Some(piece) = choice.delta.content {
                                if !piece.is_empty() {
                                    on_chunk(&piece);
                                    full_content.push_str(&piece);
                                }
                            }
                        }
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

/// Process-level slot for the leaked `&'static LlamaModel`. Filled once when
/// the chat backend loads (so the leaked model survives the loader thread
/// exiting). The schema delta engine reads this to create its OWN isolated
/// `LlamaContext` on the same model: true context isolation, the same
/// pattern as the embedder (§3B). `LlamaModel` is `Sync`, so a `&'static` ref
/// is safely shareable across the chat, embedder, and schema threads.
static SHARED_MODEL: std::sync::OnceLock<&'static LlamaModel> = std::sync::OnceLock::new();

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
                // delta engine can create an isolated context on the same model.
                // set() is a no-op if already set (it won't be: first load).
                let _ = SHARED_MODEL.set(model_ref);

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

/// Free function: the leaked `&'static LlamaModel`, available after the chat
/// backend finishes loading. Used by the schema delta engine to create an
/// isolated `LlamaContext` on the same model. Returns `None` if the model
/// hasn't loaded yet (callers should gate on backend readiness first).
pub fn shared_model() -> Option<&'static LlamaModel> {
    SHARED_MODEL.get().copied()
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
}
