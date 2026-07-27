pub mod api;
pub mod bracket_parser;
pub mod chat_format;
pub mod codex;
pub mod context_swap;
pub mod engine;
pub mod fable_command;
pub mod fable_engine;
pub mod fable_save;
#[cfg(windows)]
pub mod hardware;
pub mod json_repair;
pub mod kv_buffer;
pub mod llm;
pub mod memory;
pub mod memory_embedder;
pub mod memory_embedder_llama;
pub mod memory_rrf;
pub mod model_downloader;
pub mod narrator_prompt;
pub mod player_state;
pub mod prompts;
pub mod schema;
pub mod schema_engine;
pub mod schema_validator;
pub mod session;
pub mod sim_card;
pub mod stream_filter;
pub mod system_menu;
pub mod theme;
pub mod layout;
pub mod updater;
pub mod user_profile;
pub mod tools;

use std::sync::Arc;
use tauri::{Emitter, Manager};
use llm::GenerationClient;

/// The Memory engine's concrete embedder type, decided ONCE at startup. Using
/// `Box<dyn Embedder + Send + Sync>` lets `AppState` hold one concrete
/// `MemoryEngine` regardless of whether `Embed.gguf` was found: `LlamaCppEmbedder`
/// (real BERT backend) or `StubEmbedder` (byte-histogram fallback) both box into
/// this slot. One virtual call per `embed`, negligible next to multi-ms GPU work.
/// The `Embedder` trait is verified dyn-compatible (no `Self`, no generic
/// methods, manually-desugared `EmbedFuture` instead of `async fn`).
pub type DynEmbedder = Box<dyn memory_embedder::Embedder + Send + Sync>;

#[derive(Clone)]
pub struct AppState {
    pub session: Arc<tokio::sync::Mutex<session::Conversation>>,
    pub backend: Arc<std::sync::Mutex<Option<Arc<llm::LlamaCppBackend>>>>,
    pub settings: Arc<std::sync::Mutex<prompts::WupiSettings>>,
    /// The cancel token for the CURRENTLY active generation (if any). Each
    /// `chat_send` creates a fresh `CancelToken` and stores it here; `chat_stop`
    /// signals whatever is in this slot. This prevents overlapping sends from
    /// cross-wiring each other's cancellation (Bug #7).
    pub active_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    /// The Memory engine. Wrapped in `OnceLock` because the embedder needs a
    /// model path resolved from the Tauri `app` handle, which isn't available
    /// when `AppState::new()` runs (before `setup()`). `setup()` fills it once;
    /// reads after init are lock-free. Always `Some` after `setup()` completes.
    pub memory: Arc<std::sync::OnceLock<Arc<memory::MemoryEngine<DynEmbedder>>>>,
    /// The world-state schema: "the schema IS the summarizer." A persistent,
    /// semi-structured record of the simulated world's state, updated after
    /// every chat turn by the background state-delta pass (schema_engine.rs).
    /// Held under tokio::sync::Mutex because it's read by chat_send (to inject
    /// into the prompt) and written by the delta-completion path.
    pub schema: Arc<tokio::sync::Mutex<schema::WorldSchema>>,
    /// Handle to the in-flight schema delta pass (if any). chat_send checks
    /// this to implement the invisible queue: if a pass is running when the
    /// user sends, the message waits for it to finish before the next
    /// generation starts. None = no pass running, proceed immediately.
    /// Always `Some(JoinHandle)` between turn-finalize and the next chat_send.
    pub pending_delta: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// The schema delta engine. Wrapped in `OnceLock` because spawning it
    /// requires the chat model to have loaded first (`shared_model()` is the
    /// leaked `&'static LlamaModel` the schema context is created from). For
    /// The schema delta engine. Held under a resettable Mutex<Option<...>> so
    /// the model-swap code (api_connect/api_disconnect, chunk 4b) can tear it
    /// down + respawn it on a different model (WUPI.gguf ↔ Agent.gguf). Was
    /// OnceLock before the API feature; OnceLock can't be reset, which blocked
    /// the swap. None = not running (chat proceeds without schema deltas).
    pub schema_engine: Arc<std::sync::Mutex<Option<Arc<schema_engine::SchemaEngine>>>>,
    /// Fail-proof delta contract (§5 layer 3): auto-summarizer attempts that
    /// exhausted all 3 passes WITHOUT committing. Drained at the top of the
    /// next `chat_send` and folded into the next delta prompt as "previously
    /// deferred state changes — re-attempt with the new exchange as anchor."
    /// Bounded by `MAX_FAILED_DELTA_ATTEMPTS`; oldest evicted on overflow
    /// (rare: requires 8+ consecutive failures). Distinct from
    /// `failed_translation_queue` because the auto-summarizer and the
    /// game-manager translation path are separate request flows with separate
    /// trigger contexts (exchange vs player request).
    pub failed_delta_queue: Arc<tokio::sync::Mutex<Vec<schema_engine::FailedAttempt>>>,
    /// Fail-proof delta contract (§5 layer 3) — game-manager translation path
    /// sibling. Player requests ("make it stormy") that exhausted all 3
    /// passes WITHOUT committing. Drained on the next translation request and
    /// folded into its prompt.
    pub failed_translation_queue: Arc<tokio::sync::Mutex<Vec<schema_engine::FailedAttempt>>>,
    /// The active simulation card's id: the partition key for Memory
    /// retrieval and archiving (AGENTS.md §2M). Defaults to
    /// [`memory::WUPI_CARD_ID`] (the Wupi-as-assistant namespace) until
    /// the character/simulation card system exists; when a card loads, its
    /// loader sets this. Read on every chat turn (search + 2× archive).
    pub active_card_id: Arc<std::sync::Mutex<String>>,
    /// The active Simulation Card (the parsed persona artifact). Filled once
    /// in `setup()` from `data/wupi.sim` (§8C); reads after init are lock-free.
    /// `chat_send` renders it into the system-prompt persona section;
    /// `get_intro` reads its randomized introduction list. Always `Some`
    /// after `setup()` (the loader falls back to a stub, never `None`).
    pub active_card: Arc<std::sync::OnceLock<sim_card::SimCard>>,
    /// The resolved path to the user's profile
    /// (`<exe_dir>/data/user.xml`, §8C-renamed from Operator.xml), filled
    /// once in `setup()`. Single copy (no template/live split per §8C):
    /// the shipped zip contains the empty template; the user authors their
    /// identity via the User Editor and that file is preserved across
    /// updates. `None` when no profile resolved. The PATH is stable; the
    /// CONTENT is re-read fresh each `chat_send` (hot-reload: see
    /// `user_profile`).
    /// Lock-free reads after `setup`. Held as `Option<PathBuf>` so a missing
    /// profile is `None`, distinct from "not yet resolved."
    pub operator_path: Arc<std::sync::OnceLock<Option<std::path::PathBuf>>>,
    /// The resolved `docs/` directory (Codex lore library; renamed from
    /// `codex/` 2026-07-17). Filled once in setup; the codex_* IPC commands
    /// read/write `.md` files here. `None` when no docs/ dir resolved: the
    /// Codex UI shows empty.
    pub codex_dir: Arc<std::sync::OnceLock<Option<std::path::PathBuf>>>,
    /// The active theme + color code (defaults Aurora / Vibrant). Read by the
    /// frontend to paint the cascade panels; written by `theme_set`. Held
    /// under a std Mutex: never awaited across.
    pub theme: Arc<std::sync::Mutex<theme::ThemeSettings>>,
    /// The resolved path to `theme.json` in `<exe_dir>/data`. Filled once in
    /// setup; `theme_set` saves to it. OnceLock because it needs the Tauri
    /// app handle to resolve the portable data dir (not available in
    /// AppState::new()).
    pub theme_path: Arc<std::sync::OnceLock<std::path::PathBuf>>,
    /// The dock + desktop icon arrangement (which apps are in the bottom
    /// quick-menu + which are free-positioned on the desktop). Read by the
    /// frontend to render the dock/desktop; written by `layout_set`. Held
    /// under a std Mutex: never awaited across.
    pub layout: Arc<std::sync::Mutex<layout::LayoutSettings>>,
    /// The resolved path to `layout.json` in `<exe_dir>/data`. Filled once
    /// in setup; `layout_set` saves to it. OnceLock for the same reason as
    /// `theme_path`.
    pub layout_path: Arc<std::sync::OnceLock<std::path::PathBuf>>,
    /// The API connection config (saved profiles + active source). Read by
    /// the `api_*` IPC commands; written by `api_profile_save`/`api_connect`/
    /// `api_disconnect`. Held under a std Mutex: short critical sections.
    pub api_config: Arc<std::sync::Mutex<api::ApiConfig>>,
    /// The resolved path to `api_config.json` in `<exe_dir>/data`. Filled
    /// once in setup; the `api_*` IPC saves to it. OnceLock for the same
    /// reason as `theme_path`: needs the Tauri app handle.
    pub api_config_path: Arc<std::sync::OnceLock<std::path::PathBuf>>,
    /// The active chat source (`Local` = WUPI.gguf 12B, `Api` = HTTP endpoint).
    /// Mirrors `api_config.model_source` but held separately so `chat_send`
    /// reads it without locking the whole config (and so the swap logic can
    /// flip it atomically with the model teardown). Defaults to Local.
    pub model_source: Arc<std::sync::Mutex<api::ModelSource>>,

    // The FableEngine (narrator) lives here, NOT eagerly spawned at boot: it
    // spawns on `fable_start` and shuts down on `fable_end`. Costs VRAM only
    // while a game is actually running. Same shape as `schema_engine` (Mutex
    // of Option of Arc). None = no game running.
    pub fable_engine: Arc<std::sync::Mutex<Option<Arc<fable_engine::FableEngine>>>>,
    /// Per-game cancel token, parallel to `active_cancel`. Distinct slot so
    /// chat-stop and game-stop never cross-wire (Bug #7 pattern, §2C).
    pub active_fable_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    /// The game's scoped world-state schema (sibling to `schema`, which is
    /// Wupi-assistant's). Per-card: wiped/reloaded on card switch. Held
    /// under tokio Mutex because `fable_send` reads it + Wupi's game-manager
    /// path writes it (via `fable_command` deltas).
    pub fable_schema: Arc<tokio::sync::Mutex<schema::WorldSchema>>,
    /// The game's scoped conversation (sibling to `session`, which is
    /// Wupi-assistant's). Per-card: loaded on `fable_start` from
    /// `sessions/<card_id>.json`, saved on `fable_end`. Held under tokio Mutex
    /// because `fable_send` reads + writes it (windowing the narrator prompt +
    /// appending each turn). Phase 3 per-card persistence (AGENTS.md §2AA).
    pub fable_session: Arc<tokio::sync::Mutex<session::Conversation>>,
    /// The active roleplay card. `None` when no game is running. Set on
    /// `fable_start`, cleared on `fable_end`. The narrator prompt builder
    /// reads this each `fable_send` turn.
    pub active_fable_card: Arc<std::sync::Mutex<Option<sim_card::SimCard>>>,
    /// The Game-Master persona spoken by the Fable drawer's chat_send.
    /// Loaded from `data/gm.sim` on `fable_start` (mirrors the Wupi-assistant
    /// `active_card` OnceLock, but held under a Mutex so it can be swapped in/
    /// out with the game lifetime). When `fable_is_active`, the drawer's
    /// chat_send renders THIS instead of `active_card` (the OS catgirl): the
    /// drawer speaks in GM voice inside Fable, the OS chat stays the catgirl
    /// outside. `None` when no game is running OR when gm.sim is missing/
    /// malformed (graceful: drawer falls back to the OS catgirl persona).
    pub fable_persona: Arc<std::sync::Mutex<Option<sim_card::SimCard>>>,
    /// The card id BEFORE a game started, so `fable_end` can restore it. The
    /// system card (`__wupi__`) is the default; games swap to the
    /// roleplay card's id and restore on exit.
    pub pre_fable_card_id: Arc<std::sync::Mutex<String>>,
    /// First-run GGUF download progress (see `model_downloader.rs`). Polled
    /// by `get_download_progress` and emitted as the `download-progress`
    /// event. Held under a std Mutex: short critical sections only, never
    /// awaited across (the download task itself runs on a tokio task and
    /// briefly locks to update fields between awaits).
    pub download_progress: Arc<std::sync::Mutex<model_downloader::DownloadProgress>>,
    /// Cancel token for an in-flight first-run download. Signaled by
    /// `cancel_download`; read at the top of each chunk in the download loop
    /// (same `Ordering::Relaxed` invariant as the engine decode loop, §3).
    pub download_cancel: Arc<std::sync::Mutex<Option<model_downloader::CancelToken>>>,
    /// The resolved chat-model path, stashed by `setup()` for the deferred
    /// `boot_load_model` IPC. The boot UX defers the actual model spawn until
    /// AFTER the JS-side update check completes: if an update is found, the
    /// JS calls `updater_apply` + `updater_restart` (model never loads); if
    /// up-to-date, the JS calls `boot_load_model` which reads this stashed
    /// path + spawns the engine. `None` until `setup()` resolves the path;
    /// stays `None` if no GGUF was found (the frontend's first-run download
    /// overlay takes over and never calls `boot_load_model`).
    pub pending_model_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,

    /// The VRAM swap-lock. Enforces "at most ONE `WUPI.gguf` `LlamaContext`
    /// resident at a time" across chat/schema/fable (the embedder is exempt:
    /// separate 36.8MB BERT model). Without this, the 4-context-coresident
    /// design OOMs the FableEngine on a 12GB GPU (the 2026-07-26 freeze root
    /// cause — see `context_swap.rs` module doc + AGENTS.md §7B correction).
    /// Cheap to clone (just an `Arc`); shared by all three engines.
    pub context_swap: context_swap::ContextSwap,
}

impl AppState {
    fn new() -> Self {
        Self {
            session: Arc::new(tokio::sync::Mutex::new(session::Conversation::new())),
            backend: Arc::new(std::sync::Mutex::new(None)),
            settings: Arc::new(std::sync::Mutex::new(prompts::WupiSettings::default())),
            active_cancel: Arc::new(std::sync::Mutex::new(None)),
            memory: Arc::new(std::sync::OnceLock::new()),
            schema: Arc::new(tokio::sync::Mutex::new(schema::WorldSchema::default())),
            pending_delta: Arc::new(tokio::sync::Mutex::new(None)),
            failed_delta_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            failed_translation_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            schema_engine: Arc::new(std::sync::Mutex::new(None)),
            active_card_id: Arc::new(std::sync::Mutex::new(
                memory::WUPI_CARD_ID.to_owned(),
            )),
            active_card: Arc::new(std::sync::OnceLock::new()),
            operator_path: Arc::new(std::sync::OnceLock::new()),
            codex_dir: Arc::new(std::sync::OnceLock::new()),
            theme: Arc::new(std::sync::Mutex::new(theme::ThemeSettings::default())),
            theme_path: Arc::new(std::sync::OnceLock::new()),
            layout: Arc::new(std::sync::Mutex::new(layout::LayoutSettings::default())),
            layout_path: Arc::new(std::sync::OnceLock::new()),
            api_config: Arc::new(std::sync::Mutex::new(api::ApiConfig::default())),
            api_config_path: Arc::new(std::sync::OnceLock::new()),
            model_source: Arc::new(std::sync::Mutex::new(api::ModelSource::default())),
            fable_engine: Arc::new(std::sync::Mutex::new(None)),
            active_fable_cancel: Arc::new(std::sync::Mutex::new(None)),
            fable_schema: Arc::new(tokio::sync::Mutex::new(schema::WorldSchema::default())),
            fable_session: Arc::new(tokio::sync::Mutex::new(session::Conversation::new())),
            active_fable_card: Arc::new(std::sync::Mutex::new(None)),
            fable_persona: Arc::new(std::sync::Mutex::new(None)),
            pre_fable_card_id: Arc::new(std::sync::Mutex::new(memory::WUPI_CARD_ID.to_owned())),
            download_progress: Arc::new(std::sync::Mutex::new(
                model_downloader::DownloadProgress::default(),
            )),
            download_cancel: Arc::new(std::sync::Mutex::new(None)),
            pending_model_path: Arc::new(std::sync::Mutex::new(None)),
            context_swap: context_swap::ContextSwap::new(),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("{info}\nbacktrace: {}", std::backtrace::Backtrace::force_capture());
        let _ = std::fs::write(std::env::temp_dir().join("wupi_panic.txt"), &msg);
    }));

    let log_dir = std::env::temp_dir();
    let file_appender = tracing_appender::rolling::never(&log_dir, "wupi.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    std::mem::forget(_guard);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .with_target(false)
        .with_writer(non_blocking)
        .init();

    tracing::info!("=== WUPI starting ===");
    tauri::Builder::default()
        .manage(AppState::new())
        .manage(hardware::AudioRegistry)
        .setup(|app| {
            tracing::info!("setup hook entered");
            // Best-effort cleanup of `*.old` files from a prior self-update:
            // both `wupi.exe.old` (the locked-exe swap dance) AND any DLL
            // remnants (`msvcp140.dll.old`, `cublas64_13.dll.old`, …) left by
            // `copy_file_robust` when an update had to rename a locked/loaded
            // file out of the way. By the time this exe runs, the old
            // process's locks are gone.
            updater::cleanup_old_files(app.handle());
            // §8C portable layout: four top-level sibling dirs next to
            // wupi.exe hold all user state. Nothing leaves the install
            // folder. Created lazily here at boot so the rest of setup +
            // the IPC commands can assume they exist.
            let data_dir = resolve_data_dir(app.handle());
            let memory_dir = resolve_memory_dir(app.handle());
            let models_dir = resolve_models_dir(app.handle());
            let fable_dir = resolve_apps_dir(app.handle()).join("fable");
            std::fs::create_dir_all(&data_dir).ok();
            std::fs::create_dir_all(&memory_dir).ok();
            std::fs::create_dir_all(&models_dir).ok();
            // Per-card roleplay state (§8C): each subdir holds
            // `<card_id>.json` (or scenario `.sim`s) for the roleplay games
            // the user has played. `profiles/` is reserved for the future
            // scene-profile editor; shipped empty. `saves/` (v0.6.0+)
            // holds named save slots under `<card_id>/<save_id>.json`.
            std::fs::create_dir_all(fable_dir.join("sessions")).ok();
            std::fs::create_dir_all(fable_dir.join("schemas")).ok();
            std::fs::create_dir_all(fable_dir.join("cards")).ok();
            std::fs::create_dir_all(fable_dir.join("saves")).ok();
            std::fs::create_dir_all(fable_dir.join("profiles")).ok();
            // §8C v0.2.4 → v0.3.0 one-shot boot migration: a v0.2.4 install
            // has its user state scattered under data/ (memory.sqlite, models/,
            // sessions/, schemas/, Operator.xml). v0.3.0 promotes those to
            // top-level dirs (memory/, models/, apps/fable/{sessions,schemas}/,
            // data/user.xml). The updater preserves data/ verbatim, so without
            // this migration the user's memory + GGUFs (would force a 10GB
            // re-download!) + roleplay state would be orphaned at their old
            // paths. Idempotent: only moves when source exists AND target
            // doesn't, so a v0.3.0+ boot is a complete no-op.
            migrate_v0_2_4_to_v0_3_0(&data_dir, &memory_dir, &models_dir, &fable_dir);
            // Fable rename (§7 → Fable): relocate any v0.6.x apps/games/
            // roleplay state into the new apps/fable/ path. Idempotent; no-op
            // on fresh installs + post-migration boots.
            migrate_games_to_fable(&resolve_apps_dir(app.handle()));
            tracing::info!("portable data dir: {}", data_dir.display());

            let state: tauri::State<AppState> = app.state();

            // Resolved path cached on AppState so theme_get/theme_set don't
            // need the app handle; load now so the frontend can read the
            // persisted choice on boot.
            {
                let theme_path = theme::ThemeSettings::resolve_path(&data_dir);
                let loaded = theme::ThemeSettings::load(&theme_path);
                tracing::info!(
                    theme = %loaded.theme,
                    color_code = %loaded.color_code,
                    "theme loaded"
                );
                *state.theme.lock().expect("theme mutex") = loaded;
                let _ = state.theme_path.set(theme_path);
            }

            // Same pattern as theme: resolve path → load → cache path on
            // AppState so layout_get/layout_set don't need the app handle.
            // The dock/desktop arrangement is UI state; a corrupt file just
            // resets to the default quick-menu (never blocks launch).
            {
                let layout_path = layout::LayoutSettings::resolve_path(&data_dir);
                let loaded = layout::LayoutSettings::load(&layout_path);
                tracing::info!(
                    dock_count = loaded.dock.len(),
                    desktop_count = loaded.desktop.len(),
                    "layout loaded"
                );
                *state.layout.lock().expect("layout mutex") = loaded;
                let _ = state.layout_path.set(layout_path);
            }

            // Same pattern as theme: resolve path → load → cache path on
            // AppState so the api_* IPC commands don't need the app handle.
            // model_source is restored here; the actual model swap (if it was
            // Api at last shutdown) is re-performed later in setup once the
            // local model has finished loading, NOT here: we can't swap
            // models before the local model has loaded.
            {
                let api_path = api::ApiConfig::resolve_path(&data_dir);
                let loaded = api::ApiConfig::load(&api_path);
                tracing::info!(
                    profiles = loaded.profiles.len(),
                    source = ?loaded.model_source,
                    active = ?loaded.active_profile_id,
                    "api config loaded"
                );
                // Sync model_source to the loaded value (both track the same
                // thing; model_source is the fast-read copy for chat_send).
                *state.model_source.lock().expect("model_source mutex") = loaded.model_source;
                *state.api_config.lock().expect("api_config mutex") = loaded;
                let _ = state.api_config_path.set(api_path);
            }

            // WUPI launches into a FRESH session every time: no
            // session.json or world_schema.json load. Memory (memory.sqlite)
            // is the ONLY persistent state; it survives across launches and
            // is how Wupi "remembers" you. The session + schema live only in
            // memory for the current launch.
            //
            // Why: Wupi is a meta-assistant / Copilot (§1), not a roleplay
            // chat app. You don't resume your last Windows session every
            // reboot. Persisting the session caused cross-topic contamination
            // (a cyberpunk story's messages bled into a fresh dungeon run).
            //
            // The character/simulation card system (future, unbuilt) will
            // re-introduce SCOPED persistence: a card carries its own session
            // + its own schema, resumable on demand. That's an opt-in layer
            // on top of the ephemeral default, NOT a replacement for it. The
            // atomic save/load methods in session.rs + schema.rs are retained
            // for that future use (marked #[allow(dead_code)] until then).
            tracing::info!("fresh session + empty schema (ephemeral mode)");

            // Load the default card (`data/wupi.sim`, §8C) before anything else -
            // it's a single cheap file read + parse, independent of model
            // loading, and `get_intro` (called from the frontend's boot) may
            // race the model load. `load_or_fallback` degrades gracefully to
            // a stub persona on any error (missing file, bad XML), so the OS
            // always boots. The card's `id` becomes the active card partition
            // key for Memory once cards own their partition; today Memory
            // stays on the Wupi sentinel namespace.
            let card = match resolve_wupi_sim_path(app.handle()) {
                Some(path) => sim_card::load_or_fallback(&path),
                None => {
                    tracing::warn!(
                        "no data/wupi.sim found; using minimal fallback persona \
                         (persona section suppressed in the prompt)"
                    );
                    sim_card::fallback()
                }
            };
            let _ = state.active_card.set(card);

            // Resolve the user profile path (`data/user.xml`) once and cache
            // it. The CONTENT is re-read fresh each chat_send (hot-reload: a
            // live edit takes effect on the very next message, no reboot);
            // only the PATH is stable. `None` when no profile exists: the
            // common case until the user authors one. Wupi then runs without
            // a <user_profile> section (graceful: she just doesn't know who
            // she's talking to until the file exists).
            let user = resolve_user_path(app.handle());
            if let Some(p) = &user {
                tracing::info!("resolved user profile: {}", p.display());
            } else {
                tracing::info!("no user.xml found; running without a user profile");
            }
            let _ = state.operator_path.set(user);

            // Resolve the chat-model path and STASH it on AppState. The
            // actual spawn is deferred to the `boot_load_model` IPC (called
            // from JS after the boot update-check gate). If an update is
            // found, JS calls `updater_apply` + `updater_restart` instead
            // and the model never loads (no wasted CPU/VRAM on a doomed
            // process). If no GGUF is here at all (fresh install), emit
            // "missing" so the frontend's download overlay takes over —
            // `boot_load_model` is never called in that path.
            let model_path = resolve_model_path(app.handle());
            match model_path {
                Some(path) => {
                    tracing::info!(path = %path.display(), "model resolved; spawn deferred to boot_load_model IPC");
                    *state
                        .pending_model_path
                        .lock()
                        .expect("pending_model_path mutex") = Some(path);
                }
                None => {
                    // No GGUF found. On a fresh install (the beta-tester
                    // path), this means the first-run downloader hasn't run
                    // yet — emit "missing" so the frontend shows the
                    // download overlay instead of silently booting into
                    // echo mode. The frontend's overlay then drives
                    // `download_models`; on completion it reloads so setup
                    // re-enters with the now-present WUPI.gguf.
                    tracing::info!("no model file found; emitting 'missing' for first-run downloader");
                    let app_handle = app.handle().clone();
                    let _ = app_handle.emit(
                        "model-status",
                        serde_json::json!({ "status": "missing" }),
                    );
                }
            }

            // Build the MemoryEngine with the real BERT embedder if
            // `Embed.gguf` is on disk; fall back to StubEmbedder otherwise
            // (graceful degradation: documented contract in
            // memory_embedder_llama.rs::resolve_embed_model). The embedder is
            // boxed into `Box<dyn Embedder + Send + Sync>` so AppState holds
            // one concrete type regardless of which backend was chosen.
            //
            // `shared_backend()` (§2H) is the single `LlamaBackend::init()`
            // chokepoint: both the chat loader (above) and the embedder route
            // through it. The embedder thread does NOT block on chat-model
            // loading: `shared_backend` is a `OnceLock` that resolves on first
            // call; whichever loader hits it first inits, the other reuses.
            let embedder: DynEmbedder = match resolve_embed_model_dirs(app.handle()) {
                Some(path) => {
                    tracing::info!("spawning embed model load: {}", path.display());
                    let (embedder, init_rx) =
                        memory_embedder_llama::LlamaCppEmbedder::spawn_load(path, 99);
                    // Block on the readiness channel: same contract as the
                    // chat engine's Bug #6 fix. If init failed, fall back to
                    // the stub so the app still runs (memory just won't be
                    // semantic). This recv runs on the setup thread, which is
                    // fine: setup is allowed to block.
                    match init_rx.recv() {
                        Ok(Ok(())) => {
                            tracing::info!("memory engine: LlamaCppEmbedder ready");
                            Box::new(embedder)
                        }
                        Ok(Err(msg)) => {
                            tracing::warn!(
                                error = %msg,
                                "embedder init failed; falling back to StubEmbedder"
                            );
                            Box::new(memory_embedder::StubEmbedder {
                                dim: memory_embedder::EMBED_DIM,
                            })
                        }
                        Err(_) => {
                            tracing::warn!(
                                "embedder init channel closed; falling back to StubEmbedder"
                            );
                            Box::new(memory_embedder::StubEmbedder {
                                dim: memory_embedder::EMBED_DIM,
                            })
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        "no Embed.gguf found; memory engine using StubEmbedder (no semantic search)"
                    );
                    Box::new(memory_embedder::StubEmbedder {
                        dim: memory_embedder::EMBED_DIM,
                    })
                }
            };

            // §8C: the memory DB lives at `<exe_dir>/memory/memory.sqlite`
            // (promoted out of data/ so memory is a clean top-level artifact).
            let memory_db_path = memory_dir.join("memory.sqlite");
            match memory::MemoryEngine::open(&memory_db_path, embedder) {
                Ok(engine) => {
                    let _ = state.memory.set(Arc::new(engine));
                    tracing::info!(db = %memory_db_path.display(), "memory engine initialized");

                    // Reconcile authored `.md` files in `docs/` against the
                    // Codex-tagged entries already stored in memory.sqlite.
                    // Idempotent (hash-based): re-runs against an unchanged
                    // source set do zero writes. Best-effort: a failed seed
                    // is logged-and-dropped, never fatal (same contract as the
                    // embedder fallback). Runs synchronously here (setup is
                    // allowed to block: it already blocks on the embedder
                    // readiness channel above).
                    // (1) User-authored codex from `data/docs/` → CODEX_CARD_ID.
                    //     The user's blank slate: empty by default, populated
                    //     only via the codex_* IPC. Pinned to CODEX_CARD_ID
                    //     (not active_card_id) so editing lore during a game
                    //     lands in the user's namespace, NOT the active
                    //     roleplay card (the pre-Phase-2 bug).
                    //
                    // (Wupi's non-editable system knowledge seed from
                    // `cards/wupi_knowledge/` was REMOVED in §8C: those source
                    // files were deleted pre-session, and the seed path was
                    // dead code. The WUPI_SYSTEM_CARD_ID sentinel constant +
                    // search_wupi_visible READ side stay live: a future system-
                    // knowledge injection path would reuse them.)
                    if let Some(codex_dir) = resolve_codex_dir(app.handle()) {
                        // Cache the resolved path for the codex_* IPC (file CRUD).
                        let _ = state.codex_dir.set(Some(codex_dir.clone()));
                        if let Some(engine) = state.memory.get() {
                            match tauri::async_runtime::block_on(
                                codex::seed_codex(engine, &codex_dir, memory::CODEX_CARD_ID, "codex"),
                            ) {
                                Ok(report) => tracing::info!(
                                    seeded = report.seeded,
                                    updated = report.updated,
                                    purged = report.purged,
                                    unchanged = report.unchanged,
                                    "user codex seeded"
                                ),
                                Err(e) => tracing::warn!(
                                    error = %format!("{e:#}"),
                                    "user codex seed failed; continuing without authored lore"
                                ),
                            }
                        }
                    } else {
                        tracing::info!("no data/docs/ dir found; skipping user codex seed");
                    }
                }
                Err(e) => {
                    // DB open failure is fatal for memory but must not kill
                    // the app. Leave the OnceLock empty; callers check `get`.
                    tracing::error!(error = %format!("{e:#}"), "memory engine init failed");
                }
            }

            // ── System tray (paw icon): installed once the app handle exists.
            // Built last so an icon-build failure can't strand the earlier
            // engine init. A failure here is non-fatal: log and continue; the
            // app still runs, just without a tray (Sleep would then hide the
            // window with no way back except Restart/relaunch).
            if let Err(e) = system_menu::build_tray(&app.handle()) {
                tracing::error!(error = %format!("{e:#}"), "tray icon build failed");
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if window.app_handle().webview_windows().len() <= 1 {
                    // Destroy the tray BEFORE exit so Windows receives NIM_DELETE
                    // while we're still alive (prevents ghost-icon caching).
                    system_menu::destroy_tray(&window.app_handle());
                    window.app_handle().exit(0);
                }
            }
        })
        // Tray-menu item dispatch: "Wake" restores the window, "Quit" is a
        // full shutdown. Routed through the same power actions the paw
        // dropdown uses.
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                system_menu::TRAY_WAKE => system_menu::power_wake(&app),
                system_menu::TRAY_QUIT => system_menu::power_shutdown(&app),
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_ready,
            chat_send,
            chat_stop,
            get_settings,
            get_intro,
            debug_memory_query,
            debug_schema_delta,
            memory_list,
            memory_update,
            memory_delete,
            memory_wipe_card,
            codex_list,
            codex_save,
            codex_delete,
            operator_profile_get,
            operator_profile_set,
            api_profiles_list,
            api_profile_save,
            api_profile_delete,
            api_profile_test,
            api_connect,
            api_disconnect,
            model_source_get,
            check_models,
            download_models,
            get_download_progress,
            cancel_download,
            fable_cards_list,
            fable_start,
            fable_send,
            fable_stop,
            fable_end,
            fable_list_saves,
            fable_save_now,
            fable_load_save,
            fable_delete_save,
            fable_continue_target,
            player_state_get,
            system_menu::power_shutdown_cmd,
            system_menu::power_restart_cmd,
            system_menu::power_sleep_cmd,
            theme_get,
            theme_set,
            layout_get,
            layout_set,
            hardware::audio::audio_get_state,
            hardware::audio::audio_set_volume,
            hardware::audio::audio_list_outputs,
            hardware::audio::audio_set_default_output,
            hardware::wifi::wifi_get_current,
            hardware::wifi::wifi_scan,
            hardware::wifi::wifi_connect,
            hardware::wifi::wifi_toggle_radio,
            hardware::bluetooth::bluetooth_get_state,
            hardware::bluetooth::bluetooth_toggle_radio,
            hardware::bluetooth::bluetooth_list_devices,
            hardware::bluetooth::bluetooth_discover,
            hardware::bluetooth::bluetooth_pair,
            updater_check,
            updater_apply,
            updater_restart,
            boot_load_model,
            system_menu::set_always_on_top,
        ])
        // Build the app, then run the event loop with a callback. Splitting
        // .build() + App::run(callback) — instead of Builder::run(context)
        // which takes no callback — gives us the RunEvent hook for
        // belt-and-suspenders tray cleanup. Builder::run exists too but
        // internally calls App::run with `|_, _| {}` (see tauri app.rs:2449).
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Belt-and-suspenders tray cleanup. The dominant exit paths
            // (power_shutdown → std::process::exit, on_window_event close)
            // already destroy the tray explicitly. This catches any other
            // RunEvent::ExitRequested path (e.g. programmatic app.exit from
            // a future code path) so a ghost icon can never accumulate from
            // a graceful-exit route we didn't anticipate.
            if let tauri::RunEvent::ExitRequested { .. } = event {
                system_menu::destroy_tray(&app_handle);
            }
        });
    tracing::info!("=== WUPI event loop exited ===");
}

#[tauri::command]
fn app_ready(state: tauri::State<'_, AppState>) -> String {
    let backend = state.backend.lock().expect("backend mutex");
    if let Some(b) = backend.as_ref() {
        if b.is_ready() {
            return "ready · model loaded".to_string();
        }
        return "loading model…".to_string();
    }
    "ready · no model (echo mode)".to_string()
}

/// Randomized boot greeting: picks one line from the active card's
/// `<introductions>` list. The result is a UI-only flourish: the frontend
/// renders it as a Wupi bubble but it is NEVER added to the conversation,
/// sent to the model, or archived to memory (an assistant turn with no
/// preceding user turn would be a malformed structure + memory noise). Returns
/// `null` when the card has no introductions (e.g. the fallback stub) → the
/// frontend shows no boot bubble.
#[tauri::command]
fn get_intro(state: tauri::State<'_, AppState>) -> Option<String> {
    state
        .active_card
        .get()
        .and_then(|c| c.random_intro().map(|s| s.to_owned()))
}

/// Read the active theme + color code. The frontend paints the cascade
/// panels from this and applies the palette to the aurora canvas.
#[tauri::command]
fn theme_get(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let t = state.theme.lock().expect("theme mutex");
    serde_json::json!({ "theme": t.theme, "colorCode": t.color_code })
}

/// Persist a new theme + color code and return the updated value. The
/// frontend re-paints the canvas on the next frame after the round-trip.
#[tauri::command]
fn theme_set(
    theme_name: String,
    color_code: String,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let path = state
        .theme_path
        .get()
        .ok_or_else(|| "theme path not initialized".to_string())?
        .clone();
    let new_settings = theme::ThemeSettings {
        theme: theme_name,
        color_code,
    };
    new_settings.save(&path);
    *state.theme.lock().expect("theme mutex") = new_settings.clone();
    tracing::info!(
        theme = %new_settings.theme,
        color_code = %new_settings.color_code,
        "theme updated"
    );
    Ok(serde_json::json!({
        "theme": new_settings.theme,
        "colorCode": new_settings.color_code,
    }))
}

/// Read the dock + desktop layout. The frontend renders the quick-menu and
/// the free-positioned desktop icons from this on boot.
#[tauri::command]
fn layout_get(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let l = state.layout.lock().expect("layout mutex");
    serde_json::json!({ "dock": l.dock, "desktop": l.desktop })
}

/// Persist a new dock + desktop arrangement and return the updated value.
/// The frontend debounces saves (one per finished drag / context-menu
/// mutation), so this is called sparingly — not on every mousemove. The
/// `apps` launcher id is NOT stored in `dock` (the frontend appends it
/// automatically as the locked final item); passing it here is a no-op for
/// the save (it's filtered client-side before the call).
#[tauri::command]
fn layout_set(
    dock: Vec<String>,
    desktop: Vec<layout::DesktopIcon>,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let path = state
        .layout_path
        .get()
        .ok_or_else(|| "layout path not initialized".to_string())?
        .clone();
    let new_settings = layout::LayoutSettings { dock, desktop };
    new_settings.save(&path);
    *state.layout.lock().expect("layout mutex") = new_settings.clone();
    tracing::info!(
        dock_count = new_settings.dock.len(),
        desktop_count = new_settings.desktop.len(),
        "layout updated"
    );
    Ok(serde_json::json!({
        "dock": new_settings.dock,
        "desktop": new_settings.desktop,
    }))
}

fn resolve_model_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    let candidates = model_search_dirs(app);
    for dir in &candidates {
        if dir.exists() {
            if let Some(picked) = pick_main_model(dir) {
                tracing::info!("resolved model: {} (from {})", picked.display(), dir.display());
                return Some(picked);
            }
        }
    }
    None
}

/// The portable install root: `<exe_dir>`. WUPI is a portable app — nothing
/// leaves the install folder. All user-writable state lives in sibling dirs
/// next to `wupi.exe` (§8C layout): `data/`, `memory/`, `models/`, `apps/`.
/// Each top-level dir has its own resolver below; this fn is the shared root
/// they all derive from. The dir is created lazily by callers
/// (`create_dir_all`); this fn just resolves the path.
///
/// Falls back to `app_data_dir()` only if `current_exe()` fails (defensive —
/// shouldn't happen on Windows; keeps the resolver total so callers can
/// `.expect` it). The fallback preserves dev-mode behavior in the unlikely
/// event the exe path is unobtainable.
fn resolve_install_root(app: &tauri::AppHandle) -> std::path::PathBuf {
    use tauri::Manager;
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    // Defensive fallback: keeps the function total. In practice never taken
    // on Windows; logged so a future platform regression surfaces.
    tracing::warn!("current_exe() unresolved; falling back to app_data_dir");
    app.path()
        .app_data_dir()
        .expect("neither current_exe nor app_data_dir resolved")
}

/// `<exe_dir>/data` — the user-identity + ephemeral config dir. Holds the
/// active persona (`wupi.sim`), the user profile (`user.xml`), the codex
/// library (`docs/`), `theme.json`, and `api_config.json`. All preserved
/// across updates.
fn resolve_data_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_install_root(app).join("data")
}

/// `<exe_dir>/memory` — the memory engine's single SQLite DB. Promoted out
/// of `data/` (§8C) so the memory partition is a clean top-level artifact
/// (the only persistent Wupi-assistant state). Holds `memory.sqlite`.
fn resolve_memory_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_install_root(app).join("memory")
}

/// `<exe_dir>/models` — the GGUF weights. Promoted out of `data/` (§8C) so
/// the multi-GB weight files are a clean top-level artifact. Holds
/// `WUPI.gguf` + `Embed.gguf` (downloaded once on first run).
fn resolve_models_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_install_root(app).join("models")
}

/// `<exe_dir>/apps` — per-app user-state root. Today only `apps/games/`
/// exists (per-card roleplay sessions + schemas + scenario cards + scene
/// profiles). Future apps would get `apps/<app>/`. All preserved across
/// updates.
fn resolve_apps_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_install_root(app).join("apps")
}

/// §8C v0.2.4 → v0.3.0 one-shot boot migration. The §8C reorg promoted
/// memory/, models/, apps/ to top-level siblings of data/. A v0.2.4 install
/// has its user state scattered under data/ (the old monolithic data dir):
///   - `data/memory.sqlite` (+ `.wal`, `.shm` sidecars)
///   - `data/models/*.gguf`
///   - `data/sessions/<card_id>.json`
///   - `data/schemas/<card_id>.json`
///   - `data/Operator.xml`
/// The updater preserves data/ verbatim, so without this migration those
/// files would be orphaned at their old paths after a v0.3.0 update (the
/// resolvers look at the new top-level locations). Result: the user's
/// chat memory + GGUFs (would force a 10GB re-download!) + roleplay state
/// would silently disappear.
///
/// **Idempotent:** a file moves ONLY when (source exists AND target doesn't
/// exist). So a v0.3.0+ boot is a complete no-op (sources are already at
/// their new locations or absent entirely). Safe to run on every boot.
///
/// **Best-effort:** errors are logged-and-continued (mirrors the rest of
/// setup). A partial migration is bad, but a boot-killing migration is worse;
/// the user can manually finish the move if a single file fails.
///
/// `data/{theme.json, api_config.json, docs/*}` stay under data/ in BOTH
/// layouts — not migrated. The `data/Operator.xml → data/user.xml` rename
/// is also handled by `resolve_user_path`'s legacy-adoption path; we do it
/// here too so the rename is unconditional on boot (cleaner state).
fn migrate_v0_2_4_to_v0_3_0(
    data_dir: &std::path::Path,
    memory_dir: &std::path::Path,
    models_dir: &std::path::Path,
    fable_dir: &std::path::Path,
) {
    // Helper: move a single file from src to dst if src exists AND dst is
    // absent. Idempotent. Returns true iff a move actually happened (for
    // logging). Errors are logged-and-swallowed by the caller pattern.
    let move_if = |src: &std::path::Path, dst: &std::path::Path| -> bool {
        if !src.is_file() || dst.exists() {
            return false;
        }
        match std::fs::rename(src, dst) {
            Ok(()) => {
                tracing::info!(
                    "§8C migration: {} → {}",
                    src.display(),
                    dst.display()
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    ?e,
                    src = %src.display(),
                    dst = %dst.display(),
                    "§8C migration: rename failed; trying copy+delete"
                );
                // Cross-volume renames can fail (EXDEV). Fall back to
                // copy + delete so a volume boundary doesn't strand state.
                if std::fs::copy(src, dst).is_ok() {
                    if std::fs::remove_file(src).is_ok() {
                        tracing::info!(
                            "§8C migration (copy): {} → {}",
                            src.display(),
                            dst.display()
                        );
                        return true;
                    }
                }
                tracing::warn!(
                    src = %src.display(),
                    "§8C migration: gave up on this file (manual move needed)"
                );
                false
            }
        }
    };

    let mut moved = 0usize;

    // 1. memory.sqlite (+ WAL sidecars) → memory/
    moved += move_if(
        &data_dir.join("memory.sqlite"),
        &memory_dir.join("memory.sqlite"),
    ) as usize;
    // SQLite WAL mode writes -wal + -shm sidecars next to the main DB. Move
    // them too if present (a clean shutdown leaves none; a mid-write crash
    // leaves them). Skipping them on a live DB would corrupt the WAL state.
    for ext in &["-wal", "-shm"] {
        let src = data_dir.join(format!("memory.sqlite{ext}"));
        let dst = memory_dir.join(format!("memory.sqlite{ext}"));
        move_if(&src, &dst);
    }

    // 2. models/*.gguf → models/ (the multi-GB weights; the load-bearing one).
    // Walk data/models/ and move each file into models/. Best-effort: an
    // unreadable/in-use GGUF is logged-and-skipped (the downloader will
    // re-fetch on the next boot if missing).
    let legacy_models = data_dir.join("models");
    if legacy_models.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&legacy_models) {
            for entry in entries.flatten() {
                let src = entry.path();
                if !src.is_file() {
                    continue;
                }
                let name = match src.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_owned(),
                    None => continue,
                };
                let dst = models_dir.join(&name);
                moved += move_if(&src, &dst) as usize;
            }
        }
        // Best-effort cleanup of the now-empty legacy dir. ignore_errors: a
        // leftover GGUF (move failed) keeps it populated, which is correct.
        let _ = std::fs::remove_dir(&legacy_models);
    }

    // 3. sessions/<id>.json → apps/games/sessions/<id>.json
    let legacy_sessions = data_dir.join("sessions");
    if legacy_sessions.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&legacy_sessions) {
            for entry in entries.flatten() {
                let src = entry.path();
                if !src.is_file() {
                    continue;
                }
                let name = match src.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_owned(),
                    None => continue,
                };
                let dst = fable_dir.join("sessions").join(&name);
                moved += move_if(&src, &dst) as usize;
            }
        }
        let _ = std::fs::remove_dir(&legacy_sessions);
    }

    // 4. schemas/<id>.json → apps/games/schemas/<id>.json
    let legacy_schemas = data_dir.join("schemas");
    if legacy_schemas.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&legacy_schemas) {
            for entry in entries.flatten() {
                let src = entry.path();
                if !src.is_file() {
                    continue;
                }
                let name = match src.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_owned(),
                    None => continue,
                };
                let dst = fable_dir.join("schemas").join(&name);
                moved += move_if(&src, &dst) as usize;
            }
        }
        let _ = std::fs::remove_dir(&legacy_schemas);
    }

    // 5. data/Operator.xml → data/user.xml (also handled by resolve_user_path,
    // but doing it here unconditionally is cleaner state).
    moved += move_if(
        &data_dir.join("Operator.xml"),
        &data_dir.join("user.xml"),
    ) as usize;

    if moved > 0 {
        tracing::info!(moved, "§8C migration: v0.2.4 → v0.3.0 layout complete");
    }
}

/// Phase 0 (Fable rename) one-shot boot migration: a v0.6.x install has its
/// roleplay state under `apps/games/{cards,sessions,schemas,saves,profiles}/`.
/// The Fable rename (§7 → Fable) moved the runtime path to `apps/fable/`. The
/// updater preserves `apps/` verbatim (§8B preserve rule), so without this
/// migration the user's scenario `.sim`s + save slots + per-card sessions/
/// schemas would be orphaned at their old `apps/games/` paths.
///
/// Idempotent: only moves when source exists AND target is absent, so a
/// post-migration boot (or a fresh install with no `apps/games/`) is a no-op.
/// Walks each known subdir + moves every file; cross-volume renames fall back
/// to copy+delete (EXDEV). Errors are logged-and-continued (a partial
/// migration is bad, but a boot-killing one is worse — mirrors §8C policy).
fn migrate_games_to_fable(apps_dir: &std::path::Path) {
    let legacy = apps_dir.join("games");
    if !legacy.is_dir() {
        return; // Fresh install or already migrated — nothing to do.
    }
    let dest = apps_dir.join("fable");
    // The dest subdirs are pre-created by setup() just before this runs, so we
    // only need to move files into them.
    let mut moved = 0usize;
    for sub in &["cards", "sessions", "schemas", "saves", "profiles"] {
        let src_sub = legacy.join(sub);
        if !src_sub.is_dir() {
            continue;
        }
        let dst_sub = dest.join(sub);
        std::fs::create_dir_all(&dst_sub).ok();
        if let Ok(entries) = std::fs::read_dir(&src_sub) {
            // Recurse one level: saves/<card_id>/<save_id>.json is nested.
            fn migrate_entry(
                src: std::path::PathBuf,
                dst_sub: &std::path::Path,
                moved: &mut usize,
            ) {
                let name = match src.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_owned(),
                    None => return,
                };
                let dst = dst_sub.join(&name);
                if src.is_dir() {
                    // Nested subdir (e.g. saves/<card_id>/). Recurse into it.
                    std::fs::create_dir_all(&dst).ok();
                    if let Ok(inner) = std::fs::read_dir(&src) {
                        for e in inner.flatten() {
                            migrate_entry(e.path(), &dst, moved);
                        }
                    }
                    // Best-effort cleanup of the now-empty legacy subdir.
                    let _ = std::fs::remove_dir(&src);
                    return;
                }
                if dst.exists() {
                    return; // Idempotent: never overwrite.
                }
                match std::fs::rename(&src, &dst) {
                    Ok(()) => {
                        tracing::info!(
                            "Fable migration: {} → {}",
                            src.display(),
                            dst.display()
                        );
                        *moved += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            ?e,
                            src = %src.display(),
                            "Fable migration: rename failed; trying copy+delete"
                        );
                        if std::fs::copy(&src, &dst).is_ok()
                            && std::fs::remove_file(&src).is_ok()
                        {
                            tracing::info!(
                                "Fable migration (copy): {} → {}",
                                src.display(),
                                dst.display()
                            );
                            *moved += 1;
                        } else {
                            tracing::warn!(
                                src = %src.display(),
                                "Fable migration: gave up on this file (manual move needed)"
                            );
                        }
                    }
                }
            }
            for entry in entries.flatten() {
                migrate_entry(entry.path(), &dst_sub, &mut moved);
            }
        }
    }
    // Best-effort cleanup of the now-empty legacy apps/games/ dir. A leftover
    // file (move failed) keeps it populated, which is correct — don't force.
    let _ = std::fs::remove_dir(&legacy);
    if moved > 0 {
        tracing::info!(moved, "Fable migration: apps/games/ → apps/fable/ complete");
    }
}

/// The candidate `models/` directories, in search order. Shared by the chat
/// model resolver, the embedder resolver, and the schema-engine model
/// resolver (so they all agree on where `.gguf` files live). Extracted from
/// `resolve_model_path` so the swap logic can resolve Agent.gguf / WUPI.gguf
/// by name against the same dirs.
fn model_search_dirs(app: &tauri::AppHandle) -> Vec<std::path::PathBuf> {
    use tauri::Manager;
    let mut v = Vec::new();
    if let Some(d) = app.path().resource_dir().ok() {
        v.push(d.join("models"));
    }
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            v.push(parent.join("models"));
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                v.push(grand.join("src-tauri").join("models"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                v.push(gg.join("src-tauri").join("models"));
            }
        }
    }
    // Portable layout: GGUFs live inside the install folder at
    // `<exe_dir>/models` (§8C promoted out of `data/`). Searched LAST so a
    // dev-mode `src-tauri/models/` is preferred when present (lets local devs
    // keep models where they already are), but the portable install always
    // finds its GGUFs.
    v.push(resolve_models_dir(app));
    v
}

fn pick_main_model(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let ggufs: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("gguf"))
        .collect();
    // Locked naming convention (2026-07-12): the chat model is always
    // `WUPI.gguf`. Match the canonical name first (case-insensitive) so
    // resolution never depends on the size fallback. Embed.gguf is excluded
    // implicitly: it isn't named WUPI and is far smaller than WUPI.gguf, so
    // the size fallback would skip it anyway.
    if let Some(m) = ggufs.iter().find(|e| {
        e.file_name().to_string_lossy().to_lowercase() == "wupi.gguf"
    }) {
        return Some(m.path());
    }
    ggufs
        .into_iter()
        .max_by_key(|e| e.metadata().ok().map(|m| m.len()).unwrap_or(0))
        .map(|e| e.path())
}

/// Walk the same candidate dirs as `resolve_model_path`, but for the embeddings
/// model (`Embed.gguf`). Sibling to the chat model's discovery so the embedder
/// loader is self-contained at the wiring seam. Returns `None` when no embed
/// model is present: the caller falls back to `StubEmbedder` (graceful, not a
/// crash). Exact-name match only; no size fallback (only one file will ever be
/// named `Embed.gguf`).
fn resolve_embed_model_dirs(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(d) = app.path().resource_dir().ok() {
        dirs.push(d.join("models"));
    }
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.join("models"));
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                dirs.push(grand.join("src-tauri").join("models"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                dirs.push(gg.join("src-tauri").join("models"));
            }
        }
    }
    // Portable layout: see `model_search_dirs` for rationale. Same LAST
    // candidate so the portable install's downloaded `Embed.gguf` is found.
    dirs.push(resolve_models_dir(app));
    memory_embedder_llama::resolve_embed_model(&dirs)
}

// ── First-run GGUF downloader IPC ──────────────────────────────────────────
//
// Four commands power the boot download overlay:
//   - check_models            : are WUPI.gguf / Embed.gguf present?
//   - download_models         : stream both from HF into <exe_dir>/data/models
//   - get_download_progress   : polled snapshot of the in-flight download
//   - cancel_download         : signal an in-flight download to stop
//
// The flow (driven from script.js's setupBootSplash gate):
//   1. setup emits `model-status: missing` when no gguf is found (lib.rs
//      setup() above).
//   2. script.js calls check_models to confirm, shows #download-overlay.
//   3. User clicks "Download" → download_models fires; the overlay subscri
//      cribes to `download-progress` events + polls get_download_progress.
//   4. On Done, script.js calls app_ready (which re-resolves the model and
//      triggers a reload via the existing model-status path).

/// Check whether the chat model + embedder model are present in any candidate
/// `models/` dir. Returns a JSON object the frontend uses to decide whether
/// to show the download overlay. Both-present ⇒ boot normally; either
/// missing ⇒ show the overlay (the downloader fetches BOTH regardless, so
/// the simple "either missing" gate is correct).
#[tauri::command]
fn check_models(app: tauri::AppHandle) -> serde_json::Value {
    let wupi = resolve_model_path(&app).is_some();
    let embed = resolve_embed_model_dirs(&app).is_some();
    serde_json::json!({
        "wupi": if wupi { "present" } else { "missing" },
        "embed": if embed { "present" } else { "missing" },
    })
}

/// The target `models/` dir for downloads: `<exe_dir>/models` (§8C promoted
/// out of `data/`). Portable layout — GGUFs live inside the install folder,
/// never in the OS app-data dir. This matches the candidate list in
/// `model_search_dirs` (which now includes `<exe_dir>/models`), so a
/// freshly-downloaded GGUF is picked up on the next boot scan with zero
/// resolver changes.
fn download_target_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    Some(resolve_models_dir(app))
}

/// Start streaming both GGUFs into `<exe_dir>/models`. Returns IMMEDIATELY
/// (fire-and-forget); the actual download runs on a detached tokio task and
/// reports progress via the shared `download_progress` slot + `download-
/// progress` events. The frontend polls `get_download_progress` and reacts
/// to `phase === 'done' | 'failed'` to drive the overlay's terminal UI.
///
/// Why fire-and-forget (the v0.3.7 fix): the previous shape `await`ed the
/// download from the JS `invoke('download_models')` call. When WebView2
/// suspends the JS event loop (alt-tab → fully-covered window), the IPC
/// channel closes from the JS side, which drops the Rust download future
/// mid-stream — the download silently died whenever the user looked away.
/// Decoupling the lifetime of the download (a tokio task) from the lifetime
/// of the IPC call (a single round-trip) means the OS's WebView suspension
/// can no longer kill it. The poll/listen observers in the overlay pick up
/// the terminal `phase` whenever the webview next runs.
///
/// Setup errors (can't resolve dest dir, mutex poisoned) return `Err`
/// synchronously so the overlay can show them. Once the spawn is dispatched
/// we return `Ok(())`; the spawned task owns terminal-state reporting.
#[tauri::command]
async fn download_models(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // Resolve target dir + mint a fresh cancel token. The cancel slot is
    // scoped into its own block so the MutexGuard drops before we spawn
    // (the `!Send` guard invariant, same as api_connect at lib.rs:2092).
    let dest_dir = download_target_dir(&app)
        .ok_or_else(|| "could not resolve <exe_dir>/models".to_owned())?;
    let cancel = {
        let fresh = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut slot = state
            .download_cancel
            .lock()
            .expect("download_cancel mutex");
        *slot = Some(std::sync::Arc::clone(&fresh));
        fresh
    };

    // Reset progress to a clean Idle so a re-run after a prior failure
    // doesn't show stale totals.
    {
        let mut p = state.download_progress.lock().expect("progress mutex");
        *p = model_downloader::DownloadProgress::default();
    }

    // Detached task: lives independently of this IPC call. Captures only
    // `'static` clones (Arcs + AppHandle), so it's Send + 'static and safe
    // to spawn. On completion it sets terminal phase + emits the final
    // progress event + clears the cancel slot — these were previously done
    // inline after the await.
    let progress = Arc::clone(&state.download_progress);
    let cancel_slot = Arc::clone(&state.download_cancel);
    let app_handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = model_downloader::download_all(
            dest_dir,
            Arc::clone(&progress),
            cancel,
            app_handle.clone(),
        )
        .await;

        // Clear the cancel slot regardless of outcome (no in-flight download
        // to cancel anymore).
        {
            let mut slot = cancel_slot.lock().expect("download_cancel mutex");
            *slot = None;
        }

        match result {
            Ok(()) => {
                // download_all already set phase = Done on success.
                let _ = app_handle.emit(
                    "download-progress",
                    progress.lock().expect("progress mutex").clone(),
                );
            }
            Err(e) => {
                // Mark progress Failed so the overlay can show the error. The
                // `.part` files are retained by the downloader for resume.
                // "cancelled" is NOT a hard failure — leave the phase alone
                // so the overlay keeps its current bar (the user can resume
                // with another Download click).
                let is_cancel = e.eq_ignore_ascii_case("cancelled");
                if !is_cancel {
                    let mut p = progress.lock().expect("progress mutex");
                    if p.phase != model_downloader::DownloadPhase::Done {
                        p.phase = model_downloader::DownloadPhase::Failed;
                        p.error = e.clone();
                    }
                }
                let _ = app_handle.emit(
                    "download-progress",
                    progress.lock().expect("progress mutex").clone(),
                );
            }
        }
    });

    Ok(())
}

/// Polled snapshot of download progress. The frontend calls this on a timer
/// (e.g. every 250ms) as the authoritative source between throttled
/// `download-progress` events. Cheaper and more reliable than relying on
/// catching every emitted event (events can coalesce or drop under load).
#[tauri::command]
fn get_download_progress(state: tauri::State<'_, AppState>) -> model_downloader::DownloadProgress {
    state
        .download_progress
        .lock()
        .expect("progress mutex")
        .clone()
}

/// Cancel an in-flight download. The download loop checks the token at the
/// top of each chunk and exits with `Err("cancelled")`; the `.part` files
/// stay on disk for the next attempt to resume from. No-op if no download
/// is running.
#[tauri::command]
fn cancel_download(state: tauri::State<'_, AppState>) {
    if let Some(token) = state
        .download_cancel
        .lock()
        .expect("download_cancel mutex")
        .as_ref()
    {
        token.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

// ── Portable self-updater IPC ──────────────────────────────────────────────
//
// Two commands drive the file-level updater (src/updater.rs). The frontend
// calls `updater_check` on boot; if a new version is available it surfaces a
// toast + "Update & restart" button. `updater_apply` downloads + applies the
// update (preserving everything under data/), then the frontend prompts the
// user to restart. Updates never touch user data — see src/updater.rs for the
// preserve rule.

#[tauri::command]
async fn updater_check(app: tauri::AppHandle) -> Option<updater::UpdateInfo> {
    let current = app.package_info().version.to_string();
    updater::check_for_updates(&current).await
}

#[tauri::command]
async fn updater_apply(
    app: tauri::AppHandle,
    update: updater::UpdateInfo,
) -> Result<(), String> {
    updater::perform_update(&app, update).await
}

/// Relaunch the app after a successful update. Uses Tauri core's
/// `AppHandle::restart()` — no plugin needed (the JS-side `relaunch()` lives
/// in `@tauri-apps/plugin-process`, which we removed when we dropped the
/// installer-only Tauri updater; the Rust side is core API). The swap has
/// already placed the new `wupi.exe` on disk; restart loads it + drops the
/// .old on the next boot's `cleanup_old_files`.
#[tauri::command]
fn updater_restart(app: tauri::AppHandle) {
    app.restart();
}

/// Deferred chat-model spawn. The boot UX (script.js) calls this AFTER the
/// boot update-check gate resolves with "up-to-date" — so an update-found
/// path that calls `updater_apply` + `updater_restart` skips this entirely
/// (no wasted CPU/VRAM loading a model the process is about to abandon).
///
/// Reads the path stashed by `setup()` from
/// `AppState::pending_model_path`. If `None` (no GGUF resolved at setup),
/// emits `model-status: missing` so the frontend can take over with the
/// download overlay (defensive — the JS first-run gate should have already
/// routed to the overlay before calling this, but the IPC stays total).
///
/// The spawn closure is the original setup()-inline block, unchanged:
/// `LlamaCppBackend::spawn_load` → on ready, emit `model-status: ready` +
/// eagerly spawn the schema engine + re-perform the API-restore swap if the
/// user was on API mode at last shutdown. On error emit `model-status:
/// error`. The schema-engine + API-restore logic stays INSIDE the closure
/// because it must run on the chat-loader thread once the chat model is
/// ready (not on the IPC caller's thread).
#[tauri::command]
async fn boot_load_model(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let path = {
        let mut g = state.pending_model_path.lock().expect("pending_model_path mutex");
        g.take()
    };
    let Some(path) = path else {
        // Defensive: setup() couldn't resolve a GGUF. Surface as missing so
        // the frontend can prompt for the first-run download (which the JS
        // gate normally handles BEFORE calling boot_load_model).
        let _ = app.emit(
            "model-status",
            serde_json::json!({ "status": "missing" }),
        );
        return Ok(());
    };
    tracing::info!(path = %path.display(), "boot_load_model: spawning chat model");

    let app_handle = app.clone();
    // context_size fixes the persistent context's n_ctx for the session.
    // v0.7: the size is source-dependent. Read model_source FIRST so the chat
    // backend is born at the correct size — under API it's 2048 (silent-agent
    // mode); under Local-only it's the user's tuned settings.context_size
    // (4000). This avoids a wasteful re-spawn right after boot when an API
    // profile was active at last shutdown (the restore branch below only
    // flips the flag now — no re-spawn needed).
    let context_size = {
        let source = *state.model_source.lock().expect("model_source mutex");
        let s = state.settings.lock().expect("settings mutex");
        effective_local_ctx(source, &s)
    };
    let backend = llm::LlamaCppBackend::spawn_load(path.clone(), 99, context_size, Box::new(move |result| {
        match &result {
            Ok(name) => {
                let _ = app_handle.emit(
                    "model-status",
                    serde_json::json!({ "status": "ready", "model": name }),
                );

                // v0.6.4 VRAM swap-lock: the schema engine is NO LONGER
                // eager-spawned at boot. Spawning it here would create a
                // second resident WUPI.gguf context (chat + schema) BEFORE
                // any ContextSwap lease is taken — defeating the swap-lock
                // (the lease has no teardown registered for the eager
                // context, so it can't evict it on a cross-role acquire).
                // This was the root cause of the residual OOM window found
                // 2026-07-26 in pre-launch stress testing: a game started
                // right after boot hit chat+schema+fable co-resident the
                // instant fable_send allocated its context.
                //
                // The schema engine is now spawned LAZILY on the first
                // schema request (delta fire after a chat turn, or a
                // game-manager translation) via `acquire_schema_engine`,
                // which takes the ContextRole::Schema lease FIRST and
                // registers its teardown — so cross-role eviction can
                // actually free it. `spawn_load` uses `shared_model()`
                // (populated by the chat load just above), so the lazy
                // spawn works whenever it's first called.
                //
                // The API-restore logic below does NOT depend on the
                // schema engine (it only flips model_source so chat_send
                // routes to the API), so it stays here on the model-ready
                // path — un-nested from the deleted schema spawn.
                let app_state = app_handle.state::<AppState>();

                // If the user was on an API profile at last shutdown, boot
                // brought the 12B up as a safe default. Now that the chat
                // model is ready, re-perform the API swap so Wupi comes
                // back up on the same connection the user last had. On any
                // error we stay on local 12B: boot must never fail.
                let restore = {
                    let cfg = app_state
                        .api_config
                        .lock()
                        .expect("api_config mutex");
                    (cfg.model_source, cfg.active_profile_id.clone())
                };
                if matches!(restore.0, api::ModelSource::Api) {
                    if let Some(profile_id) = restore.1 {
                        tracing::info!(
                            profile_id = %profile_id,
                            "boot: restoring last-used API connection"
                        );
                        // v0.6.3 local-always: do NOT tear down the
                        // freshly-loaded 12B. It stays resident as the
                        // silent agent + seamless fallback. Just flip
                        // model_source so chat_send routes to the API; the
                        // local engine remains hot.
                        *app_state
                            .model_source
                            .lock()
                            .expect("model_source mutex") =
                            api::ModelSource::Api;
                        let _ = app_handle.emit(
                            "model-status",
                            serde_json::json!({
                                "status": "ready",
                                "model": "api (restored)",
                            }),
                        );
                        tracing::info!(
                            "boot: API connection restored (local resident)"
                        );
                    } else {
                        // model_source was Api but no active profile:
                        // downgrade to Local so chat_send doesn't route
                        // to a non-existent API path.
                        tracing::warn!(
                            "boot: model_source was Api but no \
                             active profile; downgrading to Local"
                        );
                        *app_state
                            .model_source
                            .lock()
                            .expect("model_source mutex") =
                            api::ModelSource::Local;
                        let mut cfg = app_state
                            .api_config
                            .lock()
                            .expect("api_config mutex");
                        cfg.model_source = api::ModelSource::Local;
                    }
                }
            }
            Err(msg) => {
                let _ = app_handle.emit(
                    "model-status",
                    serde_json::json!({ "status": "error", "message": msg }),
                );
            }
        }
    }));
    *state.backend.lock().expect("backend mutex") = Some(backend);
    Ok(())
}


/// Resolve Wupi's active persona card. Per §8C the active persona is a single
/// copy at `<exe_dir>/data/wupi.sim` (lowercase w) — no template/live split.
/// The shipped zip contains Chloe's personal `wupi.sim` content directly
/// (asymmetric with `user.xml`, which ships empty); it's engine content,
/// replaced verbatim on update.
///
/// Walks the candidate list in order: the portable `data/wupi.sim` FIRST
/// (the active persona the user runs against), then legacy/dev paths:
/// `<exe_dir>/cards/Wupi.sim` (pre-§8C portable layout) and the dev repo's
/// `cards/` dir (for local development). Exact-name match (case-insensitive)
/// against `wupi.sim`. Returns `None` when no card is found; the caller falls
/// back to a minimal stub persona so the app still boots.
fn resolve_wupi_sim_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // §8C portable layout: the active persona at `<exe_dir>/data/wupi.sim`.
    candidates.push(resolve_data_dir(app).join("wupi.sim"));
    if let Some(d) = app.path().resource_dir().ok() {
        candidates.push(d.join("data").join("wupi.sim"));
        // Legacy pre-§8C portable layout (`cards/Wupi.sim`) + dev-repo paths.
        // Kept as fallbacks so a v0.2.4 → v0.3.0 in-place upgrade finds the
        // old card if the new data/wupi.sim isn't present yet.
        candidates.push(d.join("cards"));
    }
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            // Legacy pre-§8C portable layout: cards shipped next to wupi.exe.
            candidates.push(parent.join("cards"));
            // Dev-repo paths (exe lives in target/release or target/debug).
            // The §8C source tree has the persona at `<repo>/data/wupi.sim`;
            // the legacy `<repo>/cards/` is kept for in-place upgrades that
            // haven't been re-extracted from the new zip yet.
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(grand.join("data").join("wupi.sim"));
                candidates.push(grand.join("cards"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                candidates.push(gg.join("data").join("wupi.sim"));
                candidates.push(gg.join("cards"));
            }
        }
    }

    for dir_or_file in &candidates {
        // The first candidate is a FILE path (`data/wupi.sim`); the rest are
        // DIR paths (`cards/`) to be scanned for a `wupi.sim`/`Wupi.sim` entry.
        if dir_or_file.is_file() {
            tracing::info!("resolved card: {}", dir_or_file.display());
            return Some(dir_or_file.clone());
        }
        let dir = match dir_or_file {
            p if p.is_dir() => p,
            _ => continue,
        };
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name == "wupi.sim" {
                    let path = entry.path();
                    tracing::info!("resolved card: {} (from {})", path.display(), dir.display());
                    return Some(path);
                }
            }
        }
    }
    None
}

/// Resolve the Game-Master persona path (`data/gm.sim`). Mirrors
/// `resolve_wupi_sim_path` but for the GM card spoken inside the Fable drawer.
/// Only the primary portable path + its dev-repo mirror are consulted — the
/// legacy `cards/` fallbacks are NOT (gm.sim is a Fable-v1 artifact with no
/// pre-rename history). Returns None if absent (graceful: the drawer then
/// falls back to the OS catgirl persona in chat_send).
fn resolve_gm_sim_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // §8C portable layout: `<exe_dir>/data/gm.sim`.
    candidates.push(resolve_data_dir(app).join("gm.sim"));
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            // Dev-repo path (exe lives in target/{release,debug}).
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(grand.join("data").join("gm.sim"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                candidates.push(gg.join("data").join("gm.sim"));
            }
        }
    }
    for c in &candidates {
        if c.is_file() {
            tracing::info!("resolved gm card: {}", c.display());
            return Some(c.clone());
        }
    }
    None
}

/// Resolve the user's profile (`<exe_dir>/data/user.xml`, renamed from
/// `Operator.xml` per §8C). Single copy at `data/user.xml`: no template/live
/// split. The shipped zip contains the EMPTY template directly (the user
/// authors their identity via the User Editor); it's preserved on update.
///
/// Returns the path to `data/user.xml` if the file exists. Returns `None` if
/// no file exists: Wupi then runs without a `<user_profile>` section
/// (graceful). The data dir is created lazily here on first run so a fresh
/// install with no user.xml still has the dir ready for the User Editor to
/// write into.
///
/// Only the PATH is resolved here (once, in setup). The CONTENT is re-read
/// fresh each `chat_send` via `user_profile::load`: that's the hot-reload
/// mechanism (live edits take effect on the next message, no reboot, no
/// watcher thread).
///
/// Legacy fallback: a pre-§8C install may have `data/Operator.xml` (or
/// `cards/Operator.xml`) from a prior version. If found AND no `user.xml`
/// exists yet, prefer it so the user's prior identity survives the upgrade.
/// Once `user.xml` exists, the legacy path is ignored.
fn resolve_user_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    let data_dir = resolve_data_dir(app);
    let live_path = data_dir.join("user.xml");
    if live_path.exists() {
        return Some(live_path);
    }
    // Legacy fallback: pre-§8C install with data/Operator.xml. Adopt it as
    // the live user.xml (the XML structure is unchanged: name + description,
    // same strict-XML + CDATA + roxmltree parser).
    let legacy_data_path = data_dir.join("Operator.xml");
    if legacy_data_path.exists() {
        if std::fs::create_dir_all(&data_dir).is_ok() {
            if std::fs::rename(&legacy_data_path, &live_path).is_ok() {
                tracing::info!(
                    "adopted legacy user profile: {} -> {}",
                    legacy_data_path.display(),
                    live_path.display()
                );
                return Some(live_path);
            }
        }
    }
    // No user.xml yet. Pre-create the data dir so the User Editor has a
    // home for its first save; do NOT seed an empty file (the absence of
    // user.xml is the "no profile" signal — `user_profile::load` returns
    // `None` on NotFound, which suppresses the <user_profile> section).
    let _ = std::fs::create_dir_all(&data_dir);
    None
}

/// Resolve the `docs/` directory: the Codex lore source. Per §8C user-
/// authored `.md` files live at `<exe_dir>/data/docs/` (user data, preserved
/// across updates — `docs/` is a child of the top-level `data/` dir). The
/// dev-repo `docs/` is a fallback candidate for local development. Returns
/// the *directory* (not a single file), since it holds a set of `*.md`
/// files. Returns `None` if no `docs/` dir exists in any candidate location
/// (graceful: the Codex is optional; the seed loader treats a missing dir as
/// "nothing to seed").
fn resolve_codex_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // §8C layout FIRST: codex is user data, lives in `<exe_dir>/data/docs/`.
    // The user authors `.md` files here via the Codex UI (codex.rs writes the
    // files directly). This MUST be the primary candidate so user edits are
    // found; the dev-repo `docs/` below is only for local development.
    candidates.push(resolve_data_dir(app).join("docs"));
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            // Dev-repo layout: `<repo>/docs/` (exe lives in target/release).
            candidates.push(parent.join("docs"));
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(grand.join("docs"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                candidates.push(gg.join("docs"));
            }
        }
    }

    for dir in &candidates {
        if dir.is_dir() {
            tracing::info!("resolved codex (docs/) dir: {}", dir.display());
            return Some(dir.clone());
        }
    }
    None
}

// Three small functions: fable_is_active checks whether a FableEngine is
// running; route_to_fable_manager handles the MutateWorldState intent
// (translates the player's request to a SchemaDelta via the schema engine's
// isolated context, applies it, streams a confirmation); route_to_fable_query
// handles QueryWorldState (returns a slice of the game's world-state schema).
//
/// Acquire the Schema VRAM lease + spawn-or-reuse the schema engine.
///
/// v0.6.4 VRAM swap-lock: the schema engine is NO LONGER eager-resident.
/// It's spawned lazily here on the first schema request (delta or
/// translation) and torn down when chat/fable next acquires (cross-role
/// eviction). Between back-to-back schema requests it stays resident
/// (same-role reuse, no re-spawn churn).
///
/// Returns `(engine, lease_guard)`. The caller MUST hold the guard for the
/// duration of the schema request — dropping it marks the slot free (the
/// resident engine persists until a different role evicts it).
///
/// The teardown callback tears down the schema engine on the next
/// cross-role acquire. It clones the AppState Arc it needs so it can run
/// independently of this task.
async fn acquire_schema_engine(
    state: &tauri::State<'_, AppState>,
) -> Result<(Arc<schema_engine::SchemaEngine>, context_swap::LeaseGuard), String> {
    acquire_schema_engine_from_arcs(
        state.context_swap.clone(),
        Arc::clone(&state.schema_engine),
    )
    .await
}

/// The Arcs-only inner of `acquire_schema_engine`. Exists so a detached
/// `tokio::spawn` task (the chat_send delta-fire path) can acquire the
/// schema engine without a `tauri::State<'_>` borrow — it owns its Arcs.
async fn acquire_schema_engine_from_arcs(
    context_swap: context_swap::ContextSwap,
    schema_engine_slot: Arc<std::sync::Mutex<Option<Arc<schema_engine::SchemaEngine>>>>,
) -> Result<(Arc<schema_engine::SchemaEngine>, context_swap::LeaseGuard), String> {
    // Clone the slot Arc for the teardown closure (the closure moves it; we
    // still need the original for the spawn-or-reuse below).
    let teardown_slot = Arc::clone(&schema_engine_slot);
    let lease = context_swap
        .acquire(
            context_swap::ContextRole::Schema,
            Box::new(move || {
                // Synchronous teardown: take the engine out of the slot +
                // join its thread so VRAM is freed before the next context
                // allocates. Mirrors the fable teardown path.
                let engine = {
                    let mut g = teardown_slot.lock().map_err(|e| e.to_string())?;
                    g.take()
                };
                if let Some(engine) = engine {
                    let engine = Arc::try_unwrap(engine).map_err(|_| {
                        "schema teardown: other Arc refs still held".to_string()
                    })?;
                    engine.shutdown(); // synchronous .join
                    tracing::info!("context-swap: schema engine torn down (VRAM freed)");
                }
                Ok(())
            }),
        )
        .await;

    // Spawn-or-reuse. Fast path: a prior schema request left it resident.
    let engine = {
        let existing = schema_engine_slot
            .lock()
            .map_err(|e| format!("schema_engine mutex: {e}"))?
            .clone();
        match existing {
            Some(e) => {
                tracing::debug!("context-swap: schema engine reused (resident)");
                e
            }
            None => {
                tracing::info!("context-swap: spawning schema engine on demand");
                let (engine, init_rx) = schema_engine::SchemaEngine::spawn_load();
                let ready = tokio::task::spawn_blocking(move || init_rx.recv())
                    .await
                    .map_err(|e| format!("schema engine init join: {e}"))?
                    .map_err(|e| format!("schema engine init channel: {e}"))?;
                match ready {
                    Ok(()) => {
                        let engine = Arc::new(engine);
                        if let Ok(mut slot) = schema_engine_slot.lock() {
                            *slot = Some(Arc::clone(&engine));
                        }
                        engine
                    }
                    Err(msg) => {
                        return Err(format!("schema engine init failed: {msg}"));
                    }
                }
            }
        }
    };
    Ok((engine, lease))
}

// Both route helpers are invoked from the top of chat_send via an early
// return, so the existing Wupi-assistant chat body is never entered when a
// management intent is detected.

/// True when a FableEngine is currently running (a game is active). Cheap:
/// locks the Mutex briefly, checks for Some, drops the guard. Used at the top
/// of `chat_send` to decide whether to run the management-intent classifier.
fn fable_is_active(state: &tauri::State<'_, AppState>) -> bool {
    // v0.6.4: a game is "active" when a Fable card is seated
    // (`active_fable_card.is_some()`), NOT when the FableEngine is resident.
    // Under the VRAM swap-lock the engine is lazily spawned on the first
    // `fable_send` and torn down when chat/schema needs VRAM — so the engine
    // slot is regularly `None` mid-game (e.g. right after the user asks
    // Wupi a question, which evicts fable to run the schema translation).
    // The card, by contrast, is seated on `fable_start` and cleared on
    // `fable_end`, which is the true game-lifetime signal the chat_send
    // gate + persona selector need.
    state
        .active_fable_card
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

/// Handle a `MutateWorldState` intent: translate the player's natural-language
/// request into a `SchemaDelta` via the schema engine's isolated context,
/// apply it to the active game's scoped `fable_schema`, and stream a
/// confirmation back through the same `on_event` Channel Wupi's chat uses.
///
/// The translation reuses `SchemaEngine::request_translation` (Phase E,
/// 2026-07-18): the same isolated context the auto-summarizer runs on, no
/// KV pollution to chat or narrator. The confirmation text is a template
/// filled from the actual delta (no LLM needed for the confirmation itself -
/// keeps the management path cheap).
///
/// Errors surface as a single `error` Channel event + an `Err` return, so the
/// UI can render them like any chat error. The active game's schema is left
/// unchanged on any failure path.
async fn route_to_fable_manager(
    text: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: &tauri::State<'_, AppState>,
) -> Result<(), String> {
    // 1. Acquire the Schema lease + spawn-or-reuse the schema engine under
    //    the VRAM swap-lock (v0.6.4). This evicts any resident chat/fable
    //    context BEFORE we generate — the load-bearing fix for the 2026-07-26
    //    freeze. The lease guard is held for the duration of the translation;
    //    dropping it at end of scope marks the slot free (the resident schema
    //    engine persists until a chat/fable turn evicts it).
    let (schema_engine, _schema_lease) = acquire_schema_engine(state).await?;

    // 2. Snapshot the current game schema (the delta diffs against this).
    //    Clone out + drop the guard before the awaited translation call.
    let current_schema = state.fable_schema.lock().await.clone();

    // 2b. Drain the failed-translation queue (fail-proof contract §5 layer 3).
    //     Any prior player request that exhausted all 3 passes is folded into
    //     this request's prompt so the model gets another shot with the new
    //     request as anchor. Take() empties the slot: if THIS turn also fails,
    //     the new failure re-enqueues below.
    let deferred = {
        let mut q = state.failed_translation_queue.lock().await;
        std::mem::take(&mut *q)
    };
    if !deferred.is_empty() {
        tracing::info!(
            deferred = deferred.len(),
            "translation attempt includes deferred re-attempts"
        );
    }

    // 3. Post the translation request + await the reply off the tokio worker
    //    (the schema thread is a bare std::thread; its mpsc::Receiver blocks).
    let reply_rx = schema_engine
        .request_translation(text.clone(), &current_schema, deferred)
        .map_err(|e| format!("{e:#}"))?;
    let reply = tokio::task::spawn_blocking(move || reply_rx.recv())
        .await
        .map_err(|e| format!("translation reply join: {e}"))?
        .map_err(|e| format!("translation reply channel: {e}"))?;

    // 3b. Fail-proof contract: if the reply carries a retryable failed_attempt
    //     (all 3 passes failed on parse/validation), enqueue it for the next
    //     translation request. Never silently dropped.
    if let Some(failed) = reply.failed_attempt.clone() {
        enqueue_failed_translation(state, failed).await;
    }

    if !reply.error.is_empty() {
        on_event
            .send(serde_json::json!({
                "type": "error",
                "message": format!("couldn't translate that: {} (queued for retry on next request)", reply.error),
            }))
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    // 4. Apply the delta to the game schema. If the model emitted `{}` (no
    //    changes), the delta is empty: treat as "didn't understand, nothing
    //    changed" and confirm that specifically.
    let Some(delta) = reply.delta else {
        on_event
            .send(serde_json::json!({
                "type": "error",
                "message": "couldn't translate that into a state change".to_string(),
            }))
            .map_err(|e| e.to_string())?;
        return Ok(());
    };

    let delta_applied = delta.has_changes();
    if delta_applied {
        let mut s = state.fable_schema.lock().await;
        s.apply_delta(delta.clone());
    }

    // 5. Build + stream the confirmation. The text is template-filled from
    //    the delta (no LLM call): keeps the management path cheap. The
    //    frontend renders it as a normal Wupi bubble via the same `chunk` +
    //    `done` event shape chat uses.
    let confirmation = if delta_applied {
        format_confirmation(&delta, &text)
    } else {
        "I couldn't turn that into a state change: try rephrasing? \
         For example: \"make it stormy\", \"set the weather to clear\", \
         \"give Alex a torch\".".to_string()
    };
    on_event
        .send(serde_json::json!({ "type": "chunk", "text": &confirmation }))
        .map_err(|e| e.to_string())?;
    on_event
        .send(serde_json::json!({
            "type": "done",
            "final_text": confirmation,
            "reasoning": "",
            "fable_manager": true,
        }))
        .map_err(|e| e.to_string())?;
    tracing::info!(
        request = %text.chars().take(80).collect::<String>(),
        applied = delta_applied,
        "game-manager: mutation request handled"
    );
    Ok(())
}

/// Handle a `QueryWorldState` intent: return a slice of the active game's
/// world-state schema so Wupi can narrate it. The `focus` (e.g. "weather",
/// "inventory") is matched against the schema's entity keys; if nothing
/// matches, the whole schema is returned (so Wupi can still describe the
/// state of the world generally).
async fn route_to_fable_query(
    focus: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: &tauri::State<'_, AppState>,
) -> Result<(), String> {
    let snapshot = state.fable_schema.lock().await.clone();
    let state_json = snapshot.to_json_pretty();

    // Best-effort focus match: look for entity keys containing the focus
    // substring. If none match, send the full schema.
    let focused = if focus.is_empty() {
        state_json.clone()
    } else {
        let lower = focus.to_lowercase();
        snapshot
            .entities
            .iter()
            .filter(|(k, _)| k.to_lowercase().contains(&lower))
            .map(|(k, v)| format!("  {k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let body = if focused.is_empty() {
        format!("Here's what I know about the world right now:\n{state_json}")
    } else {
        format!("Here's what I know about {focus}:\n{focused}")
    };

    // Emit two messages: the structured `fable_state_query` (machine-readable,
    // for any future UI that wants to render state differently) + the
    // chunk/done pair Wupi's chat UI renders as a normal bubble.
    on_event
        .send(serde_json::json!({
            "type": "fable_state_query",
            "focus": focus,
            "state": state_json,
        }))
        .map_err(|e| e.to_string())?;
    on_event
        .send(serde_json::json!({ "type": "chunk", "text": &body }))
        .map_err(|e| e.to_string())?;
    on_event
        .send(serde_json::json!({
            "type": "done",
            "final_text": body,
            "reasoning": "",
            "fable_manager": true,
        }))
        .map_err(|e| e.to_string())?;
    tracing::info!(
        focus = %focus,
        "game-manager: query handled"
    );
    Ok(())
}

/// Build a short natural-language confirmation of a `SchemaDelta`. Avoids an
/// extra LLM call: the mutation translation already did the work; this just
/// narrates the result. Falls back to a generic "Done." if the delta has no
/// recognizable changes (the `has_changes` gate upstream should prevent that).
fn format_confirmation(delta: &schema::SchemaDelta, original_request: &str) -> String {
    let mut bits = Vec::new();
    if let Some(summary) = delta.summary.as_deref() {
        bits.push(format!("Summary updated: \"{summary}\""));
    }
    if let Some(events) = delta.recent_events.as_ref() {
        if !events.is_empty() {
            let preview = events.last().map(|s| s.as_str()).unwrap_or("");
            let preview: String = preview.chars().take(80).collect();
            bits.push(format!("Logged event: {preview}"));
        }
    }
    if let Some(ents) = delta.entities.as_ref() {
        for (k, v) in ents.iter() {
            match v {
                Some(val) => bits.push(format!("{k} → {val}")),
                None => bits.push(format!("{k} removed")),
            }
        }
    }
    if bits.is_empty() {
        format!("Done: \"{}\" applied.", original_request)
    } else {
        format!("Done! {}", bits.join("; "))
    }
}

#[tauri::command]
async fn chat_send(
    text: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    tracing::info!(?text, "chat_send");

    // Bug #7: create a FRESH cancel token for this request only. Each
    // chat_send gets its own token stored in active_cancel; chat_stop signals
    // whatever is there. This prevents overlapping sends from un-canceling
    // each other.
    let cancel: llm::CancelToken =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut slot = state.active_cancel.lock().expect("active_cancel mutex");
        *slot = Some(Arc::clone(&cancel));
    }

    // When a game is active, check whether the player's message to Wupi is a
    // game-management intent (mutate world state / query world state). If so,
    // route to the dedicated handlers and RETURN EARLY: the existing chat
    // body is never entered, so Wupi-assistant chat behavior is unchanged
    // when no game is active OR when the message isn't management-shaped.
    // See docs/games-app-design.md §1.4 + fable_command.rs for the heuristic.
    if fable_is_active(&state) {
        match fable_command::classify(&text) {
            fable_command::FableCommand::MutateWorldState(_) => {
                clear_active_cancel(&state);
                return route_to_fable_manager(text, on_event, &state).await;
            }
            fable_command::FableCommand::QueryWorldState(focus) => {
                clear_active_cancel(&state);
                return route_to_fable_query(focus, on_event, &state).await;
            }
            fable_command::FableCommand::NotACommand => {
                // Fall through to normal Wupi-assistant chat.
            }
        }
    }

    // If a background schema delta pass is still in flight from the PREVIOUS
    // turn, await it before doing anything else. To the user this looks like
    // normal thinking time: the frontend gets no signal until the first chunk
    // arrives, so a pre-stream delay is indistinguishable from model latency.
    // The await resolves when the delta task completes (success or failure);
    // the schema is already updated in AppState by the task before it exits.
    // Errors are ignored: schema is best-effort, a failed delta must not
    // block chat (the schema stays at its last-good state).
    if let Some(handle) = state.pending_delta.lock().await.take() {
        let _ = handle.await;
    }

    let settings = state.settings.lock().expect("settings mutex").clone();

    // Embed the user's just-typed text and pull top hits BEFORE the session
    // lock. This is ON the chat path by design (§3A): embedding takes ms on
    // GPU, the SQLite work is spawn_blocking-internal. The just-typed message
    // isn't archived yet (pillar 2 archives after generation), so we never
    // retrieve the thing we're about to send.
    //
    // §2F cost: the retrieved block differs per query → the prompt structure
    // changes every turn → the structural-divergence guard (engine.rs) cold-
    // resets the KV cache. Delta-prefill is dead on Memory-enabled turns. This
    // is the accepted v1 cost; the cache-layout optimization is a later pass.
    // The retrieved block is NO LONGER baked into the system prompt. It's
    // threaded separately as `memory_block` and injected into the inter-turn
    // region by `render_prompt`. This keeps the system+turns prefix
    // byte-identical across turns (the precondition for eager prefill).
    let memory_block = {
        // Read the active card id once (cheap clone of a short string) so the
        // search and both archive calls below use the same scope within a turn.
        let card_id = state
            .active_card_id
            .lock()
            .expect("active_card_id mutex")
            .clone();
        match state.memory.get() {
            // Phase 2 firewall: Wupi-as-assistant retrieves from BOTH the
            // active card AND her reserved system-knowledge partition
            // (WUPI_SYSTEM_CARD_ID) via search_wupi_visible. She always knows
            // her own OS docs regardless of which card is active. Roleplay
            // cards never see each other: only system knowledge leaks through.
            Some(engine) => match engine.search_wupi_visible(&text, &card_id, 5, None).await {
                Ok(hits) if !hits.is_empty() => Some(memory::render_memory_block(&hits)),
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "memory search failed; injecting nothing");
                    None
                }
            },
            // OnceLock empty = memory engine failed to init at startup. Memory is
            // best-effort; chat proceeds with no retrieved context.
            None => {
                tracing::trace!("memory engine not initialized; skipping retrieval");
                None
            }
        }
    };

    // Capture BEFORE `memory_block` is moved into `.stream()` below. If the
    // block contained a Codex reference, the post-turn archiver skips saving
    // the assistant's reply (which would otherwise echo authored lore back
    // into retrieval: the self-contamination loop, §2N landmine #5). The
    // marker is shared with `render_memory_block` via `CODEX_FRAME_MARKER`.
    let codex_was_injected = memory_block
        .as_deref()
        .map(|b| b.contains(memory::CODEX_FRAME_MARKER))
        .unwrap_or(false);

    // Render the current schema into the inter-turn region as a sibling
    // annotation to memory_block. `render_for_prompt()` returns "" for an
    // empty schema → we pass None → no <world_state> block on the first turn
    // (before any deltas have landed). Same empty-skip pattern as memory.
    // The schema is read here (before the session lock) so the chat engine
    // sees the state as of turn-start; any delta fired by the PREVIOUS turn
    // has already landed via the pending_delta await above.
    let world_state = {
        let s = state.schema.lock().await;
        let rendered = s.render_for_prompt();
        if rendered.is_empty() { None } else { Some(rendered) }
    };
    // Persona: rendered once per turn. The Wupi-assistant card (`active_card`)
    // is immutable after setup → byte-identical across turns → the persona
    // block is stable and does NOT trigger the §2F cold-reset guard (only the
    // inter-turn memory block does, by design). The fallback card renders to
    // "" → section suppressed.
    //
    // Phase 2 (gm.sim split): when a game is active, the drawer's chat_send
    // (the `NotACommand` fallthrough) renders the Game-Master persona
    // (`fable_persona`) instead of `active_card` (the OS catgirl). So the
    // drawer speaks in GM voice inside Fable; OS chat outside Fable stays the
    // catgirl. The narrator path (`fable_send`) reads `active_fable_card`
    // independently and is unaffected. `fable_persona` is None when gm.sim is
    // missing/malformed → graceful fallback to the OS catgirl (best-effort,
    // mirrors the embedder's degradation contract).
    let persona = if fable_is_active(&state) {
        state
            .fable_persona
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .map(|c| c.render_for_prompt())
            .or_else(|| state.active_card.get().map(|c| c.render_for_prompt()))
    } else {
        state
            .active_card
            .get()
            .map(|c| c.render_for_prompt())
    };
    // Operator profile: re-read FRESH from disk each turn (hot-reload). The
    // path is cached (stable); only the content refreshes: so a live edit to
    // user.xml (§8C-renamed from Operator.xml) takes effect on the very next
    // message. `load` returns None
    // on missing/malformed → section silently suppressed (graceful). Like the
    // persona, the rendered text is byte-identical across turns until the file
    // is edited → no cold-reset (cache-friendly, Prime Directive).
    let user_profile = user_profile::load(
        state.operator_path.get().and_then(std::option::Option::as_deref),
    )
    .map(|p| p.render_for_prompt());
    // §2F eager-prefill sliding window (2026-07-13): cap visible history to
    // the last VISIBLE_WINDOW messages regardless of token budget. Memory (M)
    // backfills evicted turns via retrieval. Truncation in the engine becomes
    // a safety net that effectively never fires (4 short turns ≪ ~3000 budget).
    //
    // Window is source-dependent (the v0.6.3 local-always redesign): the
    // API path keeps a wider 12-message window (the cloud model has the
    // context budget + the local model is kept hot as a silent agent doing
    // schema/memory tracking, so the API can carry more narrative); the Local
    // path is 4 (2 beats: the local chat backend is the silent agent at
    // effective_local_ctx(Api)=2048 — short assistant replies only, no
    // narration, no long payloads. 4 messages = 2 user↔assistant exchanges,
    // plenty for a tracking assistant + small talk, fits the 2048 budget).
    let source = *state.model_source.lock().expect("model_source mutex");
    let visible_window = if source == api::ModelSource::Api { 12 } else { 4 };

    let system_prompt = prompts::build_system_content(
        &settings,
        persona.as_deref(),
        user_profile.as_deref(),
        effective_local_ctx(source, &settings),
    );

    let messages = {
        let mut s = state.session.lock().await;
        s.add_message(session::Role::User, text.clone());
        s.assemble_api_messages_windowed(&system_prompt, visible_window)
    };

    let on_chunk: llm::ChunkFn = Arc::new({
        let on_event = on_event.clone();
        move |piece: &str| {
            let _ = on_event.send(serde_json::json!({ "type": "chunk", "text": piece }));
        }
    });

    // v0.6.4 VRAM swap-lock: the local backend is NO LONGER always-resident.
    // It's the DEFAULT idle role (resident when nothing else needs VRAM) but
    // yields to fable/schema when they acquire their leases (which evict the
    // chat context via synchronous teardown). When this chat_send runs, if a
    // fable/schema turn since the last chat evicted the backend, the slot is
    // `None` and we re-spawn it from the shared model (no file read —
    // `LlamaCppBackend::spawn_from_shared`, ~instant context creation on the
    // leaked weights that never left VRAM).
    //
    // Acquire the Chat lease first (evicts any resident fable/schema context
    // via synchronous teardown, freeing VRAM before we re-spawn). The lease
    // guard is held for the duration of the turn; dropping it at end of scope
    // marks the slot free (the resident backend persists until a fable/schema
    // turn evicts it — back-to-back chat turns reuse it).
    //
    // The teardown callback tears down the chat backend on the next
    // cross-role acquire. It calls `LlamaCppBackend::shutdown` (synchronous
    // .join, frees the chat context's ~75-150MB Q8_0 KV — NOT the weights,
    // which stay leaked for the process lifetime + reuse by the next
    // spawn_from_shared).
    let context_swap_clone = state.context_swap.clone();
    let backend_slot = Arc::clone(&state.backend);
    // v0.7: chat_context_size is now source-aware. Under API the local chat
    // backend runs at 2048 (silent-agent mode); under Local-only at the user's
    // tuned settings.context_size. This is the load-bearing fix for the
    // eviction-revert landmine: if a fable/schema turn evicted the chat
    // backend mid-API-session, the slow-path re-spawn below MUST re-spawn at
    // 2048 (not settings.context_size = 4000) — otherwise the reduced context
    // silently reverts the first time the chat backend gets evicted.
    let chat_context_size = effective_local_ctx(source, &settings);
    let _chat_lease = context_swap_clone
        .acquire(
            context_swap::ContextRole::Chat,
            Box::new(move || {
                // Synchronous teardown: take the backend out + shutdown
                // (joins the engine thread, frees the KV context). The
                // weights stay leaked (process-lifetime); only the context
                // goes.
                let backend = {
                    let mut g = backend_slot.lock().map_err(|e| e.to_string())?;
                    g.take()
                };
                if let Some(backend) = backend {
                    // LlamaCppBackend::shutdown is &self and takes the
                    // inner engine under its own lock + joins. Drop on a
                    // spawn_blocking-equivalent: this closure runs on the
                    // task thread that calls the next acquire, blocking it
                    // until the join completes (correct — the next context
                    // needs this VRAM).
                    backend.shutdown();
                    tracing::info!("context-swap: chat backend torn down (KV freed, weights retained)");
                }
                Ok(())
            }),
        )
        .await;

    // Spawn-or-reuse the chat backend. Fast path: a prior chat turn (or
    // boot) left it resident. Slow path: re-spawn from shared model.
    let backend_opt = {
        let existing = state.backend.lock().expect("backend mutex").clone();
        match existing {
            Some(b) => {
                tracing::debug!("context-swap: chat backend reused (resident)");
                Some(b)
            }
            None => {
                tracing::info!("context-swap: re-spawning chat backend from shared model");
                // Block on readiness via a oneshot so we don't return
                // before the context is live (chat_send needs the backend
                // ready for stream() below). Mirrors boot_load_model's
                // readiness-await pattern.
                let (tx, rx) = tokio::sync::oneshot::channel();
                let backend = llm::LlamaCppBackend::spawn_from_shared(
                    chat_context_size,
                    Box::new(move |result| {
                        let _ = tx.send(result);
                    }),
                );
                match backend {
                    Some(backend) => {
                        // Stash optimistically; stream() will find it once
                        // the watcher thread populates the internal slot.
                        {
                            let mut g = state.backend.lock().expect("backend mutex");
                            *g = Some(Arc::clone(&backend));
                        }
                        // Await readiness.
                        let ready = tokio::task::spawn_blocking(move || {
                            // The watcher thread in spawn_from_shared calls
                            // on_result (our tx) once init resolves. Wait
                            // for it. oneshot::Receiver::blocking_recv
                            // returns Result<T, RecvError> (sender dropped
                            // → Err).
                            rx.blocking_recv()
                        })
                        .await
                        .map_err(|e| format!("chat re-spawn readiness join: {e}"))?;
                        match ready {
                            Ok(Ok(_)) => Some(backend),
                            Ok(Err(e)) => {
                                return Err(format!("chat backend re-spawn failed: {e}"));
                            }
                            Err(_) => {
                                return Err("chat backend re-spawn readiness dropped".to_string());
                            }
                        }
                    }
                    None => {
                        // shared_model() is None — boot never loaded the
                        // chat model. This shouldn't happen (boot_load_model
                        // runs before chat_send is callable) but defend.
                        tracing::warn!("context-swap: shared_model() is None; chat backend unavailable (echo mode)");
                        None
                    }
                }
            }
        }
    };

    // Dispatch with seamless per-turn fallback. The contract (per Chloe's
    // spec): if the API is selected but fails on this turn (network, 4xx/5xx,
    // stream error, or no profile), the turn transparently falls back to the
    // local model with the 6-message window — the user sees a reply, not an
    // error. `model_source` stays Api so the NEXT turn retries the API
    // automatically (the moment it's healthy, chat returns to API + 12). This
    // gives the seamless back-and-forth: every turn tries API, drops to local
    // on failure, returns to API the instant it recovers. No manual reconnect.
    //
    // To make the fallback cache-coherent with the local engine's delta-prefill,
    // the local path re-assembles the message window at 6 (the API may have
    // assembled 12 above): the local engine's KV cache only ever saw the
    // 6-window render, so feeding it the same shape keeps the delta path fast.
    //
    // TOOL ROUTING (v0.8): tool calling is ALWAYS local, even when the API is
    // the active narrator. The local 12B stays resident as the silent agent
    // (§8 v0.6.3), so we run the agent loop against it FIRST. If the model
    // emits tool calls → execute locally, the local reply is final. If no
    // tools fired AND source==Api → discard the local prose and hand the turn
    // to the API for the narrative reply. `fable_send` (the narrator path)
    // never enters this block — it has its own dispatch and never gets tools.
    let chat_tools: Vec<chat_format::ToolSpec> = if backend_opt.is_some() {
        tools::specs()
    } else {
        Vec::new()
    };

    let (result, tools_fired) = if source == api::ModelSource::Api {
        // API mode: tool-routing pre-pass on local. If tools fire, we keep
        // the local result; otherwise we hand off to the API for narration.
        match run_agent_loop(
            &state, &app, &on_event, &system_prompt, 6,
            memory_block.clone(), world_state.clone(), chat_tools.clone(),
            effective_local_ctx(api::ModelSource::Api, &settings),
            on_chunk.clone(), cancel.clone(), backend_opt.clone(),
        )
        .await
        {
            Ok((local_result, true)) => {
                // Tools fired → local agent handled it. Skip the API.
                tracing::info!("chat_send: API mode but tools fired locally; using local reply");
                (local_result, true)
            }
            Ok((_, false)) => {
                // No tools fired → discard local prose, hand to the API for
                // the narrative reply (the API is the better narrator).
                let profile_opt = {
                    let cfg = state.api_config.lock().expect("api_config mutex");
                    cfg.active_profile().cloned()
                };
                let api_result = match profile_opt {
                    Some(profile) => {
                        let http = llm::HttpBackend::new(profile);
                        match http
                            .stream(messages, memory_block.clone(), world_state.clone(), Vec::new(), settings.context_size, on_chunk.clone(), cancel.clone())
                            .await
                        {
                            Ok(text) => text,
                            Err(e) => {
                                // Seamless fallback (the load-bearing path). Do NOT
                                // surface the error or roll back the user message:
                                // re-run the turn on the local model at the 6-window
                                // so the user gets a reply and immersion is preserved.
                                tracing::warn!(error = %e, "chat_send: API stream failed; falling back to local");
                                let _ = on_event.send(serde_json::json!({
                                    "type": "fallback",
                                    "reason": "api_unreachable",
                                    "source": "local",
                                }));
                                match run_local_or_echo(
                                    &state,
                                    &on_event,
                                    &system_prompt,
                                    6,
                                    memory_block,
                                    world_state,
                                    Vec::new(),
                                    settings.context_size,
                                    on_chunk.clone(),
                                    cancel.clone(),
                                    backend_opt.as_ref(),
                                )
                                .await
                                {
                                    Ok(text) => text,
                                    Err(()) => {
                                        rollback_last_user_message(&state, &app).await;
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // No active profile but source=Api. Same seamless fallback.
                        tracing::warn!("chat_send: source=Api but no active profile; falling back to local");
                        let _ = on_event.send(serde_json::json!({
                            "type": "fallback",
                            "reason": "no_active_profile",
                            "source": "local",
                        }));
                        match run_local_or_echo(
                            &state,
                            &on_event,
                            &system_prompt,
                            6,
                            memory_block,
                            world_state,
                            Vec::new(),
                            settings.context_size,
                            on_chunk.clone(),
                            cancel.clone(),
                            backend_opt.as_ref(),
                        )
                        .await
                        {
                            Ok(text) => text,
                            Err(()) => {
                                rollback_last_user_message(&state, &app).await;
                                return Ok(());
                            }
                        }
                    }
                };
                (api_result, false)
            }
            Err(()) => {
                // Agent loop itself failed (local backend unavailable /
                // errored mid-iteration). Roll back + bail.
                rollback_last_user_message(&state, &app).await;
                return Ok(());
            }
        }
    } else {
        // Local mode: the agent loop IS the user-visible reply path.
        match run_agent_loop(
            &state, &app, &on_event, &system_prompt, visible_window,
            memory_block, world_state, chat_tools,
            settings.context_size,
            on_chunk.clone(), cancel.clone(), backend_opt.clone(),
        )
        .await
        {
            Ok((result, tools_fired)) => (result, tools_fired),
            Err(()) => {
                rollback_last_user_message(&state, &app).await;
                return Ok(());
            }
        }
    };
    // tools_fired is read by the done event below to flag a tool-call turn.
    let _ = &tools_fired;

    // Bug #3 Step 4: hold the raw model output alongside the cleaned content +
    // reasoning so the formatter can re-render cache-coherently next turn (no
    // full re-prefill of the previous reply). Session is ephemeral now
    // (2026-07-14): no save; the turn lives only in memory for this launch.
    {
        let mut s = state.session.lock().await;
        s.add_assistant_turn(
            result.content.clone(),
            result.reasoning.clone(),
            result.raw.clone(),
        );

        // Trigger is turn-COMPLETION, not truncation. We read from the
        // Conversation (clean strings), sidestepping the engine.rs:480
        // token-boundary-drift landmine entirely: truncate_to_fit operates on
        // LlamaToken slices with no safe mapping back to Message text.
        //
        // Both turns archived (user + assistant) so search can match either.
        // spawn detaches → add_memory's internal spawn_blocking runs the SQLite
        // insert off the hot path. The chat loop never awaits it. Errors are
        // logged-and-dropped inside the task: memory is best-effort, a failed
        // archive must not break chat.
        //
        // Salience flat 1.0 for v1 (the field is stored but unused by
        // retrieval today; a heuristic is a later concern). chunk_index stays
        // 0 (whole-message; no chunking yet).
        //
        // `codex_was_injected` was captured before `memory_block` was moved
        // into `.stream()`. If true, skip archiving the assistant's reply -
        // it's a paraphrase of authored Codex lore, and saving it would
        // pollute retrieval with echoes of the Codex itself (the self-
        // contamination loop, §2N landmine #5).
        if codex_was_injected {
            tracing::debug!("codex echo-skip: archiving suppressed (codex reference was injected this turn)");
        }
        if !codex_was_injected {
        if let Some(engine) = state.memory.get() {
            // The user message is the second-to-last (last is the assistant
            // turn we just appended). checked_sub(2) guards the cold-start
            // edge where messages is unexpectedly short.
            let user_text = s.messages.len().checked_sub(2).and_then(|i| s.messages.get(i)).map(|m| m.content.clone());
            let asst_text = result.content.clone();
            let card_id = state
                .active_card_id
                .lock()
                .expect("active_card_id mutex")
                .clone();
            let engine = Arc::clone(engine);
            tokio::spawn(async move {
                if let Some(text) = user_text {
                    if let Err(e) = engine.add_memory(text, &card_id, memory::Role::User, 1.0).await {
                        tracing::warn!(error = %format!("{e:#}"), "archive user turn failed");
                    }
                }
                if let Err(e) = engine.add_memory(asst_text, &card_id, memory::Role::Assistant, 1.0).await {
                    tracing::warn!(error = %format!("{e:#}"), "archive assistant turn failed");
                }
            });
        }
        } // end echo-skip gate (if !codex_was_injected)
    }

    // Fire the background schema delta pass for the turn that just completed.
    // Mirrors the memory archive spawn above: detached, best-effort, errors
    // logged-and-dropped. The handle is stored in pending_delta so the NEXT
    // chat_send awaits it (the invisible queue) before reading the schema -
    // guaranteeing the next turn sees this turn's schema update.
    //
    // The delta pass runs on the dedicated wupi-schema thread (isolated
    // context, never touches the chat KV cache). The JoinHandle wraps the
    // post-generation work: post the request, await the reply via
    // spawn_blocking, apply the delta, persist. If the schema engine isn't
    // available (init failed, or chat proceeded in echo mode, or mid-swap),
    // skip silently.
    //
    // v0.6.4 VRAM swap-lock: the schema engine is NO LONGER eager-resident.
    // The delta task itself acquires the Schema lease + spawn-or-reuses the
    // engine (see `acquire_schema_engine`). This means the delta waits for
    // the chat turn's lease to drop (chat_send returning) before it can
    // acquire Schema — which is correct and desirable: the delta already
    // can't run until the chat decode finishes (both need the GPU), the
    // lease just makes that dependency explicit + OOM-safe.
    //
    // Capture the exchange from the session (clean strings, same source as
    // the memory archive: sidesteps the token-boundary-drift landmine the
    // same way). Read inside a brief lock, clone out, then drop the guard
    // before spawning so the task doesn't pin the session mutex.
    let (user_text, asst_text) = {
        let s = state.session.lock().await;
        let user = s.messages.len().checked_sub(2).and_then(|i| s.messages.get(i)).map(|m| m.content.clone());
        (user, result.content.clone())
    };
    // The delta pass is a full 12B forward pass. Skip it for clearly non-
    // substantive turns (short filler like "ok"/"thanks", or empty replies)
    //: see `should_fire_delta` for the conservative heuristic. 99% of real
    // turns still fire; the user's typing time masks the generation cost.
    // A skipped turn leaves pending_delta empty, so the next chat_send
    // doesn't wait: zero latency hit for filler turns.
    let user_text_for_gate = user_text.as_deref().unwrap_or("");
    if !schema_engine::should_fire_delta(user_text_for_gate, &asst_text) {
        tracing::debug!(
            user_words = user_text_for_gate.split_whitespace().count(),
            "schema delta skipped by content gate (non-substantive turn)"
        );
    } else {
        let current_schema = state.schema.lock().await.clone();
        // Drain the failed-delta queue (fail-proof contract §5 layer 3):
        // prior turns' attempts that exhausted all 3 passes WITHOUT
        // committing. Folded into this turn's delta prompt so the model
        // gets a fresh shot with the new exchange as anchor. Take()
        // empties the slot: if THIS turn also fails, the new failure is
        // re-enqueued below.
        let deferred = {
            let mut q = state.failed_delta_queue.lock().await;
            std::mem::take(&mut *q)
        };
        if !deferred.is_empty() {
            tracing::info!(
                deferred = deferred.len(),
                "delta attempt includes deferred re-attempts"
            );
        }
        let schema_slot = state.schema.clone();
        let failed_queue_slot = state.failed_delta_queue.clone();
        // The delta task needs its own AppState handle to acquire the lease
        // + spawn-or-reuse the schema engine. We can't pass the
        // `tauri::State<'_, AppState>` borrow into the 'static spawned
        // future, so we clone the Arc fields the task needs instead. The
        // `acquire_schema_engine` helper reads from `state.schema_engine`
        // + `state.context_swap`; clone both.
        let context_swap = state.context_swap.clone();
        let schema_engine_slot = Arc::clone(&state.schema_engine);
        let handle = tokio::spawn(async move {
            // Acquire the Schema lease inside the task. This blocks until
            // any resident chat/fable context is torn down (the chat turn's
            // lease drops when chat_send returns, which has already
            // happened by the time this task is scheduled — but the lease
            // makes the VRAM ordering explicit).
            let (schema_engine, _lease) = match acquire_schema_engine_from_arcs(
                context_swap,
                schema_engine_slot,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(error = %e, "schema delta: could not acquire schema engine; schema unchanged");
                    return;
                }
            };
            // Post the delta request. The reply comes back on a std::mpsc
            // channel (the schema thread is a bare std::thread), so we await
            // it via spawn_blocking: same pattern as the chat engine reply.
            let reply_rx = match schema_engine
                .request_delta(
                    (user_text.unwrap_or_default(), asst_text),
                    &current_schema,
                    deferred,
                )
            {
                Ok(rx) => rx,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "schema delta request failed; schema unchanged");
                    return;
                    }
                };
                let reply = match tokio::task::spawn_blocking(move || reply_rx.recv()).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        tracing::warn!(error = %format!("{e}"), "schema delta reply channel closed");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e}"), "schema delta reply join failed");
                        return;
                    }
                };
                if let Some(delta) = reply.delta {
                    let mut s = schema_slot.lock().await;
                    s.apply_delta(delta);
                    tracing::debug!("schema delta applied (in-memory; ephemeral)");
                } else {
                    // Fail-proof contract: a retryable failure carries
                    // `failed_attempt`. Enqueue it for the next turn — never
                    // silently dropped. Infrastructure failures (panic,
                    // tokenize/prefill error) leave failed_attempt = None and
                    // are simply logged.
                    if let Some(failed) = reply.failed_attempt.clone() {
                        let mut q = failed_queue_slot.lock().await;
                        if q.len() >= MAX_FAILED_DELTA_ATTEMPTS {
                            q.remove(0);
                            tracing::warn!(
                                queue_len = q.len(),
                                "failed_delta_queue overflow; evicted oldest entry"
                            );
                        }
                        q.push(failed);
                        tracing::warn!(
                            error = %reply.error,
                            "schema delta failed all 3 passes; queued for next-turn re-attempt"
                        );
                    } else if !reply.error.is_empty() {
                        tracing::warn!(
                            error = %reply.error,
                            "schema delta infrastructure failure; not queued (not retryable)"
                        );
                    }
                }
            });
            *state.pending_delta.lock().await = Some(handle);
        }

    clear_active_cancel(&state);


    on_event
        .send(serde_json::json!({
            "type": "done",
            "final_text": result.content,
            "reasoning": result.reasoning,
            "tool_call": tools_fired,
        }))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Clear the active cancel slot. Called at every exit path of `chat_send`
/// (success + both error branches) so a stale token is never left behind.
fn clear_active_cancel(state: &tauri::State<'_, AppState>) {
    let mut slot = state.active_cancel.lock().expect("active_cancel mutex");
    *slot = None;
}

/// Run a chat turn on the local backend (or the EchoBackend last-resort when
/// no local model is loaded). The local backend is ALWAYS resident under the
/// v0.6.3 redesign: it's the silent agent doing schema/memory tracking AND the
/// seamless fallback when the API is unhealthy.
///
/// `window` is the message-window size to re-assemble. The API path may have
/// assembled 12 above; on fallback we re-assemble at 6 to match the local
/// engine's KV-cache-coherent delta-prefill (the local engine only ever saw
/// the 6-window render, so feeding it the same shape keeps the fast delta
/// path). The local-only path passes `visible_window` (6) through unchanged.
///
/// Re-assembles from the live session (the user message was already appended
/// by the caller) rather than re-slicing a stale `messages` vec, so the
/// window cut is always taken against current state.
///
/// `on_event` is the same per-request Channel chat_send created, threaded in
/// so this helper can emit `error` events on the local path's final failure
/// (no further fallback below the local engine). The API-fallback call sites
/// emit their own `fallback` event before calling this, so the user already
/// knows we dropped to local.
async fn run_local_or_echo(
    state: &tauri::State<'_, AppState>,
    on_event: &tauri::ipc::Channel<serde_json::Value>,
    system_prompt: &str,
    window: usize,
    memory_block: Option<String>,
    world_state: Option<String>,
    tools: Vec<chat_format::ToolSpec>,
    context_size: u32,
    on_chunk: llm::ChunkFn,
    cancel: llm::CancelToken,
    backend: Option<&Arc<llm::LlamaCppBackend>>,
) -> Result<chat_format::ParsedOutput, ()> {
    // Re-assemble the window from the live session (caller already appended
    // the user message). This is the cache-coherent path: the local engine's
    // KV cache expects the 6-window render.
    let messages = {
        let s = state.session.lock().await;
        s.assemble_api_messages_windowed(system_prompt, window)
    };
    if let Some(backend) = backend {
        match backend
            .stream(messages, memory_block, world_state, tools, context_size, on_chunk, cancel)
            .await
        {
            Ok(text) => Ok(text),
            Err(e) => {
                clear_active_cancel(state);
                let _ = on_event.send(serde_json::json!({
                    "type": "error",
                    "message": format!("{e}")
                }));
                Err(())
            }
        }
    } else {
        let echo = llm::EchoBackend;
        match echo.stream(messages, None, None, Vec::new(), context_size, on_chunk, cancel).await {
            Ok(t) => Ok(t),
            Err(e) => {
                clear_active_cancel(state);
                let _ = on_event.send(serde_json::json!({
                    "type": "error",
                    "message": format!("{e}")
                }));
                Err(())
            }
        }
    }
}

/// The tool-calling agent loop. Wraps `run_local_or_echo`: decodes locally
/// with `tools` rendered into the system prompt, inspects the raw output for
/// `<|tool_call>` markers, executes them, inserts `<|tool_response>` turns,
/// and re-decodes. Up to `tools::MAX_TOOL_ITERATIONS` rounds per `chat_send`
/// turn (mirrors the schema engine's 3-pass contract philosophy).
///
/// Returns `(final_parsed_output, tools_fired)`. The caller uses `tools_fired`
/// to decide whether to skip the API path: if the local agent handled the
/// request via tools, the API isn't consulted (the tool-response-driven final
/// decode IS the user-visible reply).
///
/// # Event channel
///
/// Each iteration emits `tool_call` and `tool_result` events through `on_event`
/// so the frontend can show chips ("🔧 calling file_read…", "✓ file_read").
/// The final iteration's `chunk`s stream normally via `on_chunk` inside
/// `run_local_or_echo`.
///
/// # Tool-turn insertion (cache-coherent)
///
/// When a tool fires, we insert TWO session messages *after* the assistant
/// turn that emitted the call:
///   1. The assistant's tool-call turn (re-emitted via `raw_output` so the
///      formatter can re-render the `<|tool_call>` protocol token cache-
///      coherently per Bug #3).
///   2. The tool-response turn (a user-role message carrying the
///      `<|tool_response>` content marker).
///
/// Both inserts go inside the session lock critical section. The existing
/// archiver/delta logic uses `checked_sub(2)` indexing against the final
/// message list, which still resolves correctly because the inserts land
/// AFTER the user's original message and BEFORE the next prefill.
async fn run_agent_loop(
    state: &tauri::State<'_, AppState>,
    app: &tauri::AppHandle,
    on_event: &tauri::ipc::Channel<serde_json::Value>,
    system_prompt: &str,
    window: usize,
    memory_block: Option<String>,
    world_state: Option<String>,
    tools: Vec<chat_format::ToolSpec>,
    context_size: u32,
    on_chunk: llm::ChunkFn,
    cancel: llm::CancelToken,
    backend_opt: Option<Arc<llm::LlamaCppBackend>>,
) -> Result<(chat_format::ParsedOutput, bool), ()> {
    use tools::ToolCtx;

    // No tools → no loop; just decode once. This is the common chat case.
    if tools.is_empty() {
        let result = run_local_or_echo(
            state, on_event, system_prompt, window,
            memory_block, world_state, Vec::new(),
            context_size, on_chunk, cancel, backend_opt.as_ref(),
        )
        .await?;
        return Ok((result, false));
    }

    let install_root = resolve_install_root(app);
    let ctx = ToolCtx::new(install_root);
    let registry = tools::registry();

    // Iterate. Each iteration: decode → parse tool calls → execute → insert
    // response turns → re-decode. Break when the model emits no tool calls
    // OR we hit the iteration cap (then we force a final no-tools prose decode
    // so the user gets a reply rather than a dangling tool-call echo).
    let mut tools_fired = false;
    let mut iteration = 0usize;

    while iteration < tools::MAX_TOOL_ITERATIONS {
        iteration += 1;
        let result = run_local_or_echo(
            state, on_event, system_prompt, window,
            memory_block.clone(), world_state.clone(),
            tools.clone(),
            context_size, on_chunk.clone(), cancel.clone(),
            backend_opt.as_ref(),
        )
        .await?;

        let calls = tools::parse_tool_calls(&result.raw);
        if calls.is_empty() {
            // No tool calls this round → the model produced a final reply.
            return Ok((result, tools_fired));
        }

        // We have tool calls. Commit the assistant's tool-call turn first
        // (so the session reflects what the model actually emitted; the
        // formatter renders the <|tool_call> marker cache-coherently from
        // raw_output next turn).
        {
            let mut s = state.session.lock().await;
            s.add_assistant_turn(
                result.content.clone(),
                result.reasoning.clone(),
                result.raw.clone(),
            );
        }

        // Execute each call + insert the response turns. Errors per-call are
        // surfaced to the model via the response payload (NOT dropped): the
        // model gets to see why its call failed and try again on the next
        // iteration (the same fail-proof contract as schema_engine).
        for call in &calls {
            let _ = on_event.send(serde_json::json!({
                "type": "tool_call",
                "iteration": iteration,
                "name": call.name,
                "args": call.args,
            }));

            let outcome = match registry.iter().find(|t| t.spec().name == call.name) {
                Some(tool) => match tool.validate_args(&call.args) {
                    Ok(()) => match tool.execute(&call.args, &ctx) {
                        Ok(output) => (true, output),
                        Err(e) => (false, format!("error: {e}")),
                    },
                    Err(e) => (false, format!("invalid args: {e}")),
                },
                None => (false, format!("unknown tool: {}", call.name)),
            };

            let _ = on_event.send(serde_json::json!({
                "type": "tool_result",
                "iteration": iteration,
                "name": call.name,
                "ok": outcome.0,
                "output": outcome.1,
            }));

            // Insert the tool-response turn. We use a content marker the
            // formatter recognizes (chat_format.rs renders `<|tool_response>`
            // from it, cache-coherently). User-role so it reads as the
            // system's reply to the model's tool_call.
            let response_marker = format!(
                "{{\"__tool_response__\":true,\"name\":{},\"ok\":{},\"output\":{}}}",
                serde_json::to_string(&call.name).unwrap_or_else(|_| "\"\"".into()),
                outcome.0,
                serde_json::to_string(&outcome.1).unwrap_or_else(|_| "\"\"".into())
            );
            let mut s = state.session.lock().await;
            s.add_message(session::Role::User, response_marker);
        }

        tools_fired = true;
        // Loop continues → next iteration decodes with the extended session.
    }

    // Iteration cap hit. The last assistant turn we committed was a tool-call
    // turn; do one final no-tools decode so the user gets a prose reply
    // rather than a dangling tool-call echo.
    tracing::warn!(
        iterations = tools::MAX_TOOL_ITERATIONS,
        "tool agent loop hit iteration cap; forcing a final prose decode"
    );
    let final_result = run_local_or_echo(
        state, on_event, system_prompt, window,
        memory_block, world_state, Vec::new(),
        context_size, on_chunk, cancel, backend_opt.as_ref(),
    )
    .await?;
    Ok((final_result, tools_fired))
}

/// Cap on accumulated deferred delta attempts per queue (fail-proof contract
/// §5 layer 3). Each failed attempt folds into the next turn's prompt; if the
/// model genuinely can't commit a particular change (rare: ambiguous player
/// request + schema mismatch), older attempts are evicted FIFO so the prompt
/// doesn't bloat indefinitely. 8 is generous: a single turn rarely fails, and
/// 8 consecutive failures across 8 turns means something systematically
/// wrong that re-prompting won't fix.
const MAX_FAILED_DELTA_ATTEMPTS: usize = 8;

/// Push a failed translation attempt onto the game-manager queue. Called from
/// `route_to_fable_manager` when `SchemaReply::failed_attempt` is `Some` after
/// a `request_translation` call. Same cap + best-effort semantics as the
/// auto-summarizer path (which inlines the same logic in its `chat_send`
/// spawn — the spawn owns an Arc to the queue across awaits, which doesn't
/// factor cleanly into a `&tauri::State` helper).
async fn enqueue_failed_translation(
    state: &tauri::State<'_, AppState>,
    attempt: schema_engine::FailedAttempt,
) {
    let mut q = state.failed_translation_queue.lock().await;
    if q.len() >= MAX_FAILED_DELTA_ATTEMPTS {
        q.remove(0);
        tracing::warn!(
            queue_len = q.len(),
            "failed_translation_queue overflow; evicted oldest entry"
        );
    }
    q.push(attempt);
}

async fn rollback_last_user_message(state: &tauri::State<'_, AppState>, _app: &tauri::AppHandle) {
    // Pop the orphaned user message on generation failure so the next send
    // doesn't see two consecutive user turns (Bug C, §2D). Session is
    // ephemeral now (2026-07-14): no disk save, just in-memory correction.
    // The `_app` param is retained for signature stability (callers pass it).
    let mut s = state.session.lock().await;
    if s.last_message_is_user() {
        s.pop_last_message();
    }
}

#[tauri::command]
async fn chat_stop(state: tauri::State<'_, AppState>) -> Result<(), String> {
    tracing::info!("chat_stop requested");
    let slot = state.active_cancel.lock().expect("active_cancel mutex");
    if let Some(cancel) = slot.as_ref() {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// Debug probe into the Memory engine (pillar 4). Embeds the query, runs the
/// hybrid FTS5 + vec0 search, and returns the score-aware-RRF-fused ranked
/// results with raw dense cosine + per-list ranks per hit. Off the chat path
/// entirely: this is the observability surface for tuning retrieval
/// independently of generation, AND the calibration surface for
/// [`memory_rrf::DENSE_COSINE_FLOOR`] (AGENTS.md §2M Checkpoint E).
///
/// `top_k` defaults to 10 when `None`. `dense_floor` overrides the const for
/// live calibration: pass a value to see how the result set changes at that
/// threshold without a rebuild; leave `None` to use the compiled default.
/// Returns an error string (not a panic) if the memory engine isn't
/// initialized or the query fails: the panel renders it as a red message.
///
/// Retrieval is scoped to the active card id (AGENTS.md §2M): cards never
/// see each other's memory.
#[tauri::command]
async fn debug_memory_query(
    query: String,
    top_k: Option<usize>,
    dense_floor: Option<f32>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<memory::RankedMemory>, String> {
    let engine = state
        .memory
        .get()
        .ok_or_else(|| "memory engine not initialized".to_string())?;
    let card_id = state
        .active_card_id
        .lock()
        .expect("active_card_id mutex")
        .clone();
    engine
        .search(&query, &card_id, top_k.unwrap_or(10), dense_floor)
        .await
        .map_err(|e| format!("{e:#}"))
}

// The Codex UI lists / searches / edits / removes memories and can hard-reset
// the active card's episodic store. `debug_memory_query` above is the search
// path (reused by the Codex search box); these four commands cover enumerate /
// mutate / wipe. All scope to the active card id exactly as the search does.

/// Enumerate memories in the active card, newest first. The Codex browser's
/// default view. `limit` defaults to 200 (the per-card corpus is small); an
/// explicit `0` is clamped to 1 so the UI always gets at least the head row.
#[tauri::command]
async fn memory_list(
    limit: Option<usize>,
    offset: Option<usize>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<memory::MemoryEntry>, String> {
    let engine = state
        .memory
        .get()
        .ok_or_else(|| "memory engine not initialized".to_string())?;
    let card_id = state
        .active_card_id
        .lock()
        .expect("active_card_id mutex")
        .clone();
    let limit = limit.unwrap_or(200).max(1);
    let offset = offset.unwrap_or(0);
    engine
        .list_memories(&card_id, limit, offset)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Edit one memory's text in place (re-embeds + rewrites all three tables).
/// Silent no-op if `id` doesn't exist. Used by the Codex browser's inline
/// editor.
#[tauri::command]
async fn memory_update(
    id: i64,
    text: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let engine = state
        .memory
        .get()
        .ok_or_else(|| "memory engine not initialized".to_string())?;
    engine
        .update_memory(id, text)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Delete one memory by id (all three tables). Used by the Codex browser's
/// per-row Remove button. Wraps the existing engine method; lifted to IPC so
/// the frontend doesn't need a separate delete surface.
#[tauri::command]
async fn memory_delete(id: i64, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let engine = state
        .memory
        .get()
        .ok_or_else(|| "memory engine not initialized".to_string())?;
    engine
        .delete_memory(id)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Hard reset: wipe every EPISODIC memory in the active card, preserving
/// authored Codex lore. Returns the deleted count so the UI can confirm.
/// The Codex browser's "Hard Reset" button (confirm-gated on the frontend).
#[tauri::command]
async fn memory_wipe_card(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    let engine = state
        .memory
        .get()
        .ok_or_else(|| "memory engine not initialized".to_string())?;
    let card_id = state
        .active_card_id
        .lock()
        .expect("active_card_id mutex")
        .clone();
    engine
        .wipe_episodic_card(&card_id)
        .await
        .map_err(|e| format!("{e:#}"))
}

// The Codex is a library of authored reference "books" (world lore, TV/wiki
// facts, worldbuilding). Source of truth = `.md` files in the resolved
// `docs/` dir; the DB is a derived retrieval index re-seeded at boot. These
// three commands operate on the FILES directly, then re-seed so retrieval
// stays in sync within the running session. Nothing here touches episodic
// chat memory: the Codex is a separate, authored-only surface.

/// List every Codex entry (filename, title, tags, body). The Codex UI's
/// library view. Returns an empty Vec when no docs/ dir resolved.
#[tauri::command]
fn codex_list(state: tauri::State<'_, AppState>) -> Result<Vec<codex::CodexFile>, String> {
    let dir = state.codex_dir.get().and_then(|o| o.as_ref());
    let Some(dir) = dir else { return Ok(Vec::new()); };
    codex::list_files(dir).map_err(|e| format!("{e:#}"))
}

/// Create or overwrite a Codex `.md` file, then re-seed so retrieval sees the
/// change this session. `filename` is the stem (sanitized on disk). Returns
/// the (possibly-sanitized) filename so the UI can track the real key.
#[tauri::command]
async fn codex_save(
    filename: String,
    title: String,
    tags: Vec<String>,
    body: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let dir = state
        .codex_dir
        .get()
        .and_then(|o| o.as_ref().cloned())
        .ok_or_else(|| "no codex dir resolved".to_string())?;
    // Write the file off the tokio worker (synchronous FS I/O). `save_file`
    // returns the sanitized stem it actually wrote: echo it back so the UI
    // tracks the entry by its real on-disk key.
    let saved_name = tokio::task::spawn_blocking(move || codex::save_file(&dir, &filename, &title, &tags, &body))
        .await
        .map_err(|e| format!("codex save join: {e}"))?
        .map_err(|e| format!("{e:#}"))?;
    // Re-seed so the retrieval index reflects the edit without a reboot.
    // Pinned to CODEX_CARD_ID (NOT active_card_id): Phase 2 firewall fix:
    // pre-fix this read active_card_id, so editing lore DURING a game wrote
    // it into the active roleplay card's partition. User lore always lands in
    // the user's namespace regardless of what game is running.
    if let (Some(engine), Some(dir)) = (state.memory.get(), state.codex_dir.get().and_then(|o| o.as_ref())) {
        let _ = codex::seed_codex(engine, dir, memory::CODEX_CARD_ID, "codex").await;
    }
    Ok(saved_name)
}

/// Delete a Codex `.md` file by stem, then re-seed. Silent no-op if missing.
#[tauri::command]
async fn codex_delete(
    filename: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let dir = state
        .codex_dir
        .get()
        .and_then(|o| o.as_ref().cloned())
        .ok_or_else(|| "no codex dir resolved".to_string())?;
    tokio::task::spawn_blocking(move || codex::delete_file(&dir, &filename))
        .await
        .map_err(|e| format!("codex delete join: {e}"))?
        .map_err(|e| format!("{e:#}"))?;
    // Same CODEX_CARD_ID pin as codex_save (Phase 2 firewall).
    if let (Some(engine), Some(dir)) = (state.memory.get(), state.codex_dir.get().and_then(|o| o.as_ref())) {
        let _ = codex::seed_codex(engine, dir, memory::CODEX_CARD_ID, "codex").await;
    }
    Ok(())
}

// Two commands mirror the theme get/set pattern: read fresh from the cached
// path, write atomically back. Hot-reload is automatic (chat_send re-reads
// every turn), so a saved profile applies on the next chat turn with no extra
// wiring. `UserProfile` is Serialize/Deserialize so it crosses IPC directly.

/// Read the operator profile fresh from disk. Returns `None` when no
/// user.xml (§8C) resolved at startup (the User Editor renders empty
/// fields and a Create prompt in that case).
#[tauri::command]
async fn operator_profile_get(
    state: tauri::State<'_, AppState>,
) -> Result<Option<user_profile::UserProfile>, String> {
    // `operator_path` is `Arc<OnceLock<Option<PathBuf>>>`. `.get()` yields
    // `Option<&Option<PathBuf>>`; flatten + clone the inner PathBuf to an
    // OWNED Option<PathBuf> so it can move into the 'static spawn_blocking
    // closure (a borrow of `state` can't cross that boundary). `load` takes
    // Option<&Path>; `.as_deref()` on the owned Option<PathBuf> at the call
    // site yields exactly that.
    let path = state
        .operator_path
        .get()
        .and_then(|o| o.clone());
    // spawn_blocking: load does synchronous file I/O. Cheap, but keep it off
    // the tokio worker for consistency with the rest of the profile/memory IPC.
    tokio::task::spawn_blocking(move || user_profile::load(path.as_deref()))
        .await
        .map_err(|e| format!("profile get join: {e}"))
}

/// Write the operator profile atomically to the resolved `user.xml` path
/// (§8C; was Operator.xml). Creates the file (and its parent dir) if missing.
/// Returns an error string
/// if no path resolved at startup (shouldn't happen: `setup` always resolves
/// the candidates; `None` means none existed, in which case we can't write).
#[tauri::command]
async fn operator_profile_set(
    name: String,
    description: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // `operator_path` is `Arc<OnceLock<Option<PathBuf>>>`. `.get()` yields
    // `Option<&Option<PathBuf>>`; flatten + clone the inner PathBuf so we own
    // it and can move it into the spawn_blocking closure.
    let path = state
        .operator_path
        .get()
        .and_then(|o| o.clone())
        .ok_or_else(|| "no operator profile path resolved".to_string())?;
    let profile = user_profile::UserProfile { name, description };
    tokio::task::spawn_blocking(move || user_profile::save(&path, &profile))
        .await
        .map_err(|e| format!("profile set join: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

// Source of truth = `api_config.json` in the app data dir; the in-memory
// `AppState.api_config` is the fast read copy. These commands cover enumerate
// / mutate profiles + read/switch the active model source. `api_connect` and
// `api_disconnect` perform the actual model swap (chunk 4); in chunk 2 they
// just validate + set state so the IPC surface is testable end-to-end before
// the risky teardown code lands.

/// Read the full API config (all profiles + active source). The API panel's
/// default view. Returns the `ApiConfig` as-is: the frontend renders the
/// profile list + the model-source radio from it.
#[tauri::command]
fn api_profiles_list(state: tauri::State<'_, AppState>) -> api::ApiConfig {
    state.api_config.lock().expect("api_config mutex").clone()
}

/// Upsert a profile by id (replace if same id, append otherwise), persist,
/// return the saved profile. The id is sanitized from the name if the caller
/// passes an empty one: the UI tracks entries by the returned id.
#[tauri::command]
async fn api_profile_save(
    mut profile: api::ApiProfile,
    state: tauri::State<'_, AppState>,
) -> Result<api::ApiProfile, String> {
    if profile.id.trim().is_empty() {
        profile.id = api::sanitize_profile_id(&profile.name);
    } else {
        profile.id = api::sanitize_profile_id(&profile.id);
    }
    let path = state
        .api_config_path
        .get()
        .cloned()
        .ok_or_else(|| "api_config path not initialized".to_string())?;
    let saved = profile.clone();
    // Mutate under the lock, snapshot, then DROP the guard before awaiting
    // (std::sync::MutexGuard is !Send; can't hold across spawn_blocking.await).
    let cfg_snapshot = {
        let mut cfg = state.api_config.lock().expect("api_config mutex");
        cfg.upsert(profile);
        cfg.clone()
    };
    tokio::task::spawn_blocking(move || cfg_snapshot.save(&path))
        .await
        .map_err(|e| format!("api_config save join: {e}"))?;
    Ok(saved)
}

/// Delete a profile by id. If it was the active profile, clears active +
/// downgrades model_source to Local (can't stay on API with no profile).
/// Returns true if a profile was removed.
#[tauri::command]
async fn api_profile_delete(
    profile_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    let path = state
        .api_config_path
        .get()
        .cloned()
        .ok_or_else(|| "api_config path not initialized".to_string())?;
    // Mutate under the lock, snapshot, drop guard before awaiting.
    let (removed, downgrade_source, cfg_snapshot) = {
        let mut cfg = state.api_config.lock().expect("api_config mutex");
        let was_active = cfg.active_profile_id.as_deref() == Some(profile_id.as_str());
        let removed = cfg.remove(&profile_id);
        // If we just deleted the active profile, we can't stay on API. This
        // flips the in-memory state; the actual model swap (reload local)
        // happens in chunk 4's full disconnect path. For chunk 2 it's just
        // bookkeeping: the frontend reads model_source_get to reflect it.
        let downgrade_source = removed && was_active;
        if downgrade_source {
            cfg.model_source = api::ModelSource::Local;
        }
        (removed, downgrade_source, cfg.clone())
    };
    tokio::task::spawn_blocking(move || cfg_snapshot.save(&path))
        .await
        .map_err(|e| format!("api_config save join: {e}"))?;
    if downgrade_source {
        *state.model_source.lock().expect("model_source mutex") = api::ModelSource::Local;
    }
    Ok(removed)
}

/// Read the current model source + readiness flags. The frontend's source
/// selector reads this. `api_ready` = an active profile exists (so the API
/// radio is enabled); `local_ready` = the local backend is loaded.
#[tauri::command]
fn model_source_get(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let source = *state.model_source.lock().expect("model_source mutex");
    let cfg = state.api_config.lock().expect("api_config mutex");
    let api_ready = cfg.active_profile().is_some();
    let local_ready = state
        .backend
        .lock()
        .expect("backend mutex")
        .as_ref()
        .map(|b| b.is_ready())
        .unwrap_or(false);
    serde_json::json!({
        "source": source,
        "apiReady": api_ready,
        "localReady": local_ready,
    })
}

/// Connect an API profile: set it active, perform the model swap (Local→API),
/// flip model_source to Api. In chunk 2 the swap is a stub: it just validates
/// the profile exists + sets state. The real teardown lands in chunk 4.
#[tauri::command]
async fn api_connect(
    profile_id: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    match api_connect_inner(profile_id, app.clone(), &state).await {
        Ok(()) => Ok(()),
        // Safety net for the title indicator: if connect failed, the 12B is
        // still loaded (validation runs before any teardown; if teardown ran
        // and then something downstream failed, the runtime is genuinely
        // offline). Emit model-status so the frontend's title self-corrects
        //: never gets stuck in the red "swapping" state. JS handles this
        // too, but the backend is the authority.
        Err(e) => {
            let backend_loaded = state
                .backend
                .lock()
                .expect("backend mutex")
                .as_ref()
                .map(|b| b.is_ready())
                .unwrap_or(false);
            let status = if backend_loaded {
                serde_json::json!({ "status": "ready", "model": "WUPI.gguf" })
            } else {
                serde_json::json!({ "status": "error", "message": &e })
            };
            let _ = app.emit("model-status", status);
            Err(e)
        }
    }
}

/// The effective n_ctx for the LOCAL CHAT context given the active source.
/// This is the v0.7 source-of-truth for "how big should the local chat
/// context be?"
///
/// - **Local-only:** the user's tuned `settings.context_size` (default 4000).
///   The local 12B is the primary chat + narrator model and needs the room.
/// - **API connected:** 2048. The local 12B demotes to a silent tracking
///   agent (memory tracking + small assistant replies) — no narration, no
///   long payloads. 2048 is plenty for that and frees ~half the chat KV
///   vs the 4000-tok Local-only mode.
///
/// **Schema is NOT affected** — it's a fixed `SCHEMA_CTX = 2048` in both
/// modes (already the right size for delta work; no source-dependent sizing
/// needed). **Fable is EXEMPT** — it stays at `FABLE_CTX = 3072` regardless
/// (under API it only materializes as the fallback narrator, and a smaller
/// context would leave zero room for the ~1000-tok narrator prompt).
///
/// **Why a single helper:** every chat spawn site (boot, chat_send slow
/// path, api_connect/disconnect re-spawn) MUST read this — otherwise the
/// swap-lock eviction slow path would silently re-spawn at
/// `settings.context_size` mid-API-session (the eviction-revert landmine).
fn effective_local_ctx(source: api::ModelSource, settings: &prompts::WupiSettings) -> u32 {
    match source {
        api::ModelSource::Api => 2048,
        api::ModelSource::Local => settings.context_size,
    }
}

async fn api_connect_inner(
    profile_id: String,
    app: tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
) -> Result<(), String> {
    let path = state
        .api_config_path
        .get()
        .cloned()
        .ok_or_else(|| "api_config path not initialized".to_string())?;
    // Validate the profile exists + has the required fields (under lock, then
    // drop the guard before any await).
    {
        let cfg = state.api_config.lock().expect("api_config mutex");
        let profile = cfg
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .ok_or_else(|| format!("no API profile with id {profile_id}"))?;
        if profile.endpoint.trim().is_empty() {
            return Err("profile endpoint is empty".into());
        }
        if profile.model.trim().is_empty() {
            return Err("profile model is empty".into());
        }
        if profile.api_key.trim().is_empty() {
            return Err("profile api_key is empty".into());
        }
    }

    tracing::info!("api_connect: selecting API as chat source (local stays resident)");
    // v0.6.3 local-always redesign: the local 12B is NEVER torn down. It runs
    // all the time as the silent agent doing schema/memory tracking (the
    // schema engine already kept its own WUPI.gguf copy in both modes — this
    // just extends the same pattern to the chat backend). Keeping the chat
    // backend resident costs ~75MB of idle Q8_0 KV cache (weights are shared
    // via the leaked singleton), which is what makes the seamless per-turn
    // fallback in chat_send zero-latency: there's no model to reload when the
    // API drops, the local engine is already hot.
    //
    // So api_connect is now pure bookkeeping: set the active profile, flip
    // model_source to Api, persist. The next chat_send routes to HttpBackend;
    // if that fails, chat_send falls back to the still-resident local engine
    // automatically (no error surfaced, immersion preserved).
    *state.model_source.lock().expect("model_source mutex") = api::ModelSource::Api;
    let cfg_snapshot = {
        let mut cfg = state.api_config.lock().expect("api_config mutex");
        cfg.active_profile_id = Some(profile_id.clone());
        cfg.model_source = api::ModelSource::Api;
        cfg.clone()
    };
    tokio::task::spawn_blocking(move || cfg_snapshot.save(&path))
        .await
        .map_err(|e| format!("api_config save join: {e}"))?;
    tracing::info!(profile_id = %profile_id, "api connected: chat via API, local resident as agent + fallback");

    // v0.7: shrink the local chat context to the API-mode size (2048). The
    // local 12B is now just the silent tracking agent (memory + small replies),
    // not the narrator — the API carries narration. Take + shutdown the
    // resident 4000-ctx backend, re-spawn from shared weights at 2048.
    //
    // Weights stay leaked (shared_model singleton); only the KV context is
    // reallocated. The swap-lock lease is NOT acquired here — we synchronize
    // on state.backend directly (the same primitive the chat teardown closure
    // uses at line ~2350). Acquiring the lease would just drop immediately
    // and not protect the spawn window; the backend mutex does.
    //
    // Safe degradation: if spawn_from_shared returns None (shared_model not
    // loaded — shouldn't happen post-boot), we leave the slot empty;
    // chat_send's slow path will lazily re-spawn at effective_local_ctx(Api)
    // = 2048 on the next turn.
    {
        let api_ctx = effective_local_ctx(api::ModelSource::Api, &{
            let s = state.settings.lock().expect("settings mutex");
            s.clone()
        });
        let mut g = state.backend.lock().expect("backend mutex");
        if let Some(old) = g.take() {
            old.shutdown();
            tracing::info!("api_connect: chat backend torn down + re-spawning at smaller context");
        }
        let new_backend = llm::LlamaCppBackend::spawn_from_shared(
            api_ctx,
            Box::new(move |result| {
                if let Err(e) = &result {
                    tracing::warn!(error = %e, "api_connect: chat backend re-spawn reported error");
                }
            }),
        );
        if let Some(b) = new_backend {
            *g = Some(b);
            tracing::info!(n_ctx = api_ctx, "api_connect: chat backend re-spawned at API-mode context");
        } else {
            tracing::warn!("api_connect: spawn_from_shared returned None (shared_model not loaded); leaving slot empty");
        }
    }
    // Emit model-status so the title indicator flips to the API model name.
    // The local backend is also still ready (never torn down) — the frontend
    // reads `localReady` from model_source_get to show "local: active" too.
    let model_name = {
        let cfg = state.api_config.lock().expect("api_config mutex");
        cfg.active_profile().map(|p| p.model.clone()).unwrap_or_default()
    };
    let _ = app.emit(
        "model-status",
        serde_json::json!({ "status": "ready", "model": model_name }),
    );
    Ok(())
}

/// Disconnect the API: flip model_source back to Local. Under the v0.6.3
/// local-always redesign there is NO model to reload — the local 12B stayed
/// resident the whole time (it was the silent agent + fallback). So this is
/// now pure bookkeeping: clear nothing, just flip the source + persist. The
/// next chat_send routes to the local backend directly (no fallback needed).
#[tauri::command]
async fn api_disconnect(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    let path = state
        .api_config_path
        .get()
        .cloned()
        .ok_or_else(|| "api_config path not initialized".to_string())?;

    tracing::info!("api_disconnect: flipping chat source back to Local (local was resident)");
    *state.model_source.lock().expect("model_source mutex") = api::ModelSource::Local;
    let cfg_snapshot = {
        let mut cfg = state.api_config.lock().expect("api_config mutex");
        cfg.model_source = api::ModelSource::Local;
        cfg.clone()
    };
    tokio::task::spawn_blocking(move || cfg_snapshot.save(&path))
        .await
        .map_err(|e| format!("api_config save join: {e}"))?;

    // v0.7: restore the local chat context to the Local-mode size (the user's
    // tuned settings.context_size, default 4000). Symmetric to api_connect's
    // shrink: take + shutdown the 2048-ctx backend, re-spawn at the full size
    // since the local 12B is now the primary chat + narrator model again.
    {
        let local_ctx = effective_local_ctx(api::ModelSource::Local, &{
            let s = state.settings.lock().expect("settings mutex");
            s.clone()
        });
        let mut g = state.backend.lock().expect("backend mutex");
        if let Some(old) = g.take() {
            old.shutdown();
            tracing::info!("api_disconnect: chat backend torn down + re-spawning at full context");
        }
        let new_backend = llm::LlamaCppBackend::spawn_from_shared(
            local_ctx,
            Box::new(move |result| {
                if let Err(e) = &result {
                    tracing::warn!(error = %e, "api_disconnect: chat backend re-spawn reported error");
                }
            }),
        );
        if let Some(b) = new_backend {
            *g = Some(b);
            tracing::info!(n_ctx = local_ctx, "api_disconnect: chat backend re-spawned at Local-mode context");
        } else {
            tracing::warn!("api_disconnect: spawn_from_shared returned None; leaving slot empty");
        }
    }

    // Emit model-status: the local backend is what's serving now.
    let _ = app.emit(
        "model-status",
        serde_json::json!({ "status": "ready", "model": "WUPI.gguf" }),
    );
    tracing::info!("api disconnected: chat back on local WUPI.gguf");
    Ok(())
}

/// Test whether an API profile is reachable. Issues a lightweight GET to the
/// endpoint's `/models` path (the OpenAI-standard list endpoint). Returns Ok
/// with the model list if reachable, Err with a diagnostic if not. Used by
/// the API panel's "Test connection" button before the user commits.
#[tauri::command]
async fn api_profile_test(
    profile: api::ApiProfile,
) -> Result<serde_json::Value, String> {
    let base = profile.endpoint.trim_end_matches('/').to_string();
    // Hit /models if the endpoint is a bare base; if it already points at
    // /chat/completions, strip back to the base and try /models from there.
    let base = base.trim_end_matches("/chat/completions").to_string();
    let url = format!("{base}/models");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("build client: {e}"))?;
    let resp = client
        .get(&url)
        .bearer_auth(&profile.api_key)
        .send()
        .await
        .map_err(|e| format!("request to {url} failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "API returned {status}: {}",
            body.chars().take(300).collect::<String>()
        ));
    }
    // Return the raw JSON (shape varies by provider; the frontend just shows
    // "connected" + optionally the model count). Best-effort parse: if it's
    // not JSON, return a success marker with the text body.
    match resp.json::<serde_json::Value>().await {
        Ok(v) => Ok(v),
        Err(_) => Ok(serde_json::json!({ "connected": true })),
    }
}

/// Debug probe into the schema delta engine (B/C runtime test). Posts a
/// SYNTHETIC exchange (the caller supplies both sides) + the current schema,
/// waits for the delta pass to complete, and returns:
///   - the raw model output (what the schema model actually emitted)
///   - the parsed delta (if JSON was valid; else null)
///   - any error string
///   - the resulting schema JSON (after optionally applying the delta)
///
/// `apply: true` merges the delta into AppState.schema so the caller can
/// chain multiple calls and watch the schema evolve. `apply: false` is a dry
/// run: the schema is untouched, useful for prompt-tuning without side effects.
///
/// The schema engine is spawned LAZILY via `acquire_schema_engine` on the
/// first schema request (a chat turn's delta fire, or a game-manager
/// translation) — under the ContextRole::Schema VRAM lease (v0.6.4 swap-lock).
/// Returns an error string if the schema engine isn't resident yet (no chat
/// turn has primed it). This is a debug-only surface (per AGENTS.md §9 it has
/// no live UI); the production paths go through `acquire_schema_engine`.
#[tauri::command]
async fn debug_schema_delta(
    user_exchange: String,
    assistant_exchange: String,
    apply: Option<bool>,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    // The schema engine is spawned lazily under the VRAM swap-lock. This debug
    // path reads the slot directly (NOT via acquire_schema_engine) so it can
    // probe an engine that a prior chat turn already primed — but it will be
    // `None` until that happens. Sending a chat turn first primes the slot.
    let engine = state
        .schema_engine
        .lock()
        .map_err(|e| format!("schema_engine mutex: {e}"))?
        .clone()
        .ok_or_else(|| "schema engine not resident yet (lazy-spawned on the first chat turn via acquire_schema_delta; send a chat message first)".to_string())?;

    // Snapshot the current schema (the delta pass diffs against this).
    let current = state.schema.lock().await.clone();

    // Post the delta request + await the reply off the tokio worker (the
    // schema thread is a bare std::thread; its mpsc::Receiver is blocking).
    //
    // The debug path deliberately passes an empty deferred list AND does NOT
    // enqueue the reply's failed_attempt: this is a one-shot engineering
    // probe, not a production chat turn. Surfacing failed_attempt in the JSON
    // reply so an engineer can see when the validator + 3-pass fail is the
    // right scope for this surface.
    let reply_rx = engine
        .request_delta((user_exchange, assistant_exchange), &current, Vec::new())
        .map_err(|e| format!("{e:#}"))?;
    let reply = tokio::task::spawn_blocking(move || reply_rx.recv())
        .await
        .map_err(|e| format!("reply join: {e}"))?
        .map_err(|e| format!("reply channel: {e}"))?;

    // Optionally apply the delta so the caller can chain calls and watch the
    // schema evolve across a multi-turn scenario.
    let schema_after = if apply.unwrap_or(false) {
        if let Some(ref delta) = reply.delta {
            let mut s = state.schema.lock().await;
            s.apply_delta(delta.clone());
            s.to_json_pretty()
        } else {
            // Parse failed: return the unchanged schema.
            state.schema.lock().await.to_json_pretty()
        }
    } else {
        current.to_json_pretty()
    };

    Ok(serde_json::json!({
        "raw_output": reply.raw_output,
        "delta": reply.delta,
        "error": reply.error,
        "failed_attempt": reply.failed_attempt,
        "schema_after": schema_after,
    }))
}

#[tauri::command]
async fn get_settings(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    // Bug #9: read the actual settings instead of hardcoded values.
    // Clone the values out of the guard before awaiting the session lock so
    // the non-Send MutexGuard isn't held across an await point.
    let (context_size, conversation_budget) = {
        let s = state.settings.lock().expect("settings mutex");
        (s.context_size, s.conversation_budget)
    };
    Ok(serde_json::json!({
        "contextSize": context_size,
        "conversationBudget": conversation_budget,
        "messageCount": state.session.lock().await.messages.len(),
    }))
}

// ===========================================================================
// Games app IPC (Seam 1 + Seam 2, 2026-07-18): see docs/games-app-design.md
// ===========================================================================
// Five commands: enumerate roleplay cards, start a game (spawn FableEngine +
// swap active_card_id), send a narrator turn (streaming), stop a turn, end
// the game (shutdown engine + restore card id). The narrator system prompt
// is built per-turn from the active roleplay card + the card's scoped
// schema. Bracket commands are parsed from the final raw output + emitted
// as structured scene_event Channel messages so the (deferred) UI can route
// them. Memory archiving + schema delta reuse the existing paths: both
// scope to the active card_id automatically.

/// Lightweight metadata for one roleplay card, returned by `fable_cards_list`.
/// Carries enough for a card-picker UI (name, id, short description) without
/// loading the full persona body.
#[derive(Debug, Clone, serde::Serialize)]
struct FableCardMeta {
    id: String,
    name: String,
    card_type: String,
    setting_preview: String,
    tone: Option<String>,
    /// First ~240 chars of `<scenario><opening_scene>`: the launcher card
    /// uses this as the evocative "what's this about" blurb below the title.
    /// None when the card doesn't declare one.
    opening_scene_preview: Option<String>,
    /// Declared protagonist name. The launcher shows this so the player
    /// knows whose shoes they're stepping into before they start.
    protagonist_name: Option<String>,
    /// Whether the player has any saves for this card (autosave counts).
    /// Lets the launcher show Continue vs New Game intelligently. Best-effort:
    /// a directory-read error degrades to false (the user can still start).
    has_saves: bool,
}

/// Enumerate every `.sim` file in `apps/games/cards/` (§8C; was
/// `cards/fable_cards/`) and return parsed metadata. The card-picker UI's data
/// source. Returns an empty Vec when no cards dir exists (the common case
/// until cards are authored or imported): graceful, not an error.
#[tauri::command]
fn fable_cards_list(app: tauri::AppHandle) -> Result<Vec<FableCardMeta>, String> {
    let dir = resolve_fable_cards_dir(&app);
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read fable_cards/: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("sim") {
            continue;
        }
        let card = sim_card::load_or_fallback(&path);
        // Skip fallback stubs (a malformed file produced the fallback). The
        // id sentinel is the signal: see sim_card::FALLBACK_ID.
        if card.id == "__wupi_fallback__" {
            tracing::warn!(path = %path.display(), "skipping malformed game card");
            continue;
        }
        // Only list roleplay cards in this registry: the system card
        // (wupi.sim) lives in `data/`, not `apps/games/cards/`, so this is
        // belt-and-suspenders against a misplaced file.
        if card.card_type != "roleplay" {
            continue;
        }
        // Best-effort: has the user got any saves for this card? A dir-read
        // error degrades to false (the launcher still lets them start fresh).
        let fable_root = resolve_apps_dir(&app).join("fable");
        let has_saves = fable_save::list_saves(&fable_root, &card.id)
            .map(|list| !list.is_empty())
            .unwrap_or(false);
        out.push(card_to_meta(&card, has_saves));
    }
    // Stable order: alphabetical by name so the picker doesn't jitter.
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Build a `FableCardMeta` from a parsed card + a `has_saves` flag. Shared by
/// `fable_cards_list` (the picker) today; future card-authoring flows will
/// reuse it to return a freshly-written card's meta without a second round-trip.
fn card_to_meta(card: &sim_card::SimCard, has_saves: bool) -> FableCardMeta {
    let setting_preview = card
        .setting
        .as_deref()
        .map(|s| s.chars().take(160).collect::<String>())
        .unwrap_or_default();
    let opening_scene_preview = card
        .opening_scene
        .as_deref()
        .map(|s| s.chars().take(240).collect::<String>());
    FableCardMeta {
        id: card.id.clone(),
        name: card.name.clone(),
        card_type: card.card_type.clone(),
        setting_preview,
        tone: card.tone.clone(),
        opening_scene_preview,
        protagonist_name: card.protagonist_name.clone(),
        has_saves,
    }
}

/// Start a game: load the roleplay card, spawn the FableEngine (loads
/// WUPI.gguf as its own isolated context), swap `active_card_id` to the
/// card's id, and load the initial session/schema state. The
/// `pre_fable_card_id` is saved so `fable_end` can restore it.
///
/// **Save loading (v0.6.0+):** when `save_id` is supplied, the session +
/// schema are loaded from that named save slot (under
/// `apps/games/saves/<card_id>/<save_id>.json`) instead of the card's
/// default resume point. This is the "Load Game" path. When `save_id` is
/// None, the card's last auto-persisted session/schema is loaded (the
/// "Continue" path) — same as before v0.6.0. Pass `fresh = true` to
/// explicitly start a brand-new run (clears any prior state); this is the
/// "New Game" path.
#[tauri::command]
async fn fable_start(
    card_id: String,
    save_id: Option<String>,
    fresh: Option<bool>,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<FableLoadResult, String> {
    tracing::info!(card_id = %card_id, ?save_id, ?fresh, "fable_start: spawning FableEngine");
    let card_id_arg = card_id.clone();

    // 1. Refuse if a game is already running (the UI shouldn't allow this,
    //    but defense-in-depth).
    {
        let existing = state.fable_engine.lock().expect("fable_engine mutex");
        if existing.is_some() {
            return Err("a game is already running: call fable_end first".into());
        }
    }

    // 2. Resolve + load the roleplay card by id. The id comes from
    //    `fable_cards_list`, so it must exist in the registry.
    let card = {
        let dir = resolve_fable_cards_dir(&app)
            .ok_or_else(|| "no apps/games/cards/ dir resolved".to_string())?;
        find_card_by_id(&dir, &card_id)?
    };

    // 3. Resolve the model path (WUPI.gguf: same file the chat engine uses,
    //    freshly leaked as the FableEngine's own &'static ref).
    let model_path = resolve_model_path(&app)
        .ok_or_else(|| "no WUPI.gguf found: cannot start game".to_string())?;

    // 4. Hand off to the shared enter helper (also used by Quick Play, 4c).
    enter_fable_session(card, save_id, fresh.unwrap_or(false), model_path, card_id_arg, &app, &state).await
}

/// The shared "spawn engine + swap id + load state + seat card + seat GM
/// persona" tail of starting a game. `card` is already resolved (loaded from
/// disk for fable_start). `card_id_for_meta` is the id to report in the
/// returned meta (identical to card.id but kept explicit so logs are
/// unambiguous).
async fn enter_fable_session(
    card: sim_card::SimCard,
    save_id: Option<String>,
    fresh: bool,
    model_path: std::path::PathBuf,
    card_id_for_meta: String,
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
) -> Result<FableLoadResult, String> {
    // NOTE: the FableEngine is NO LONGER eagerly spawned here. Under the
    // v0.6.4 VRAM swap-lock (see `context_swap.rs` + AGENTS.md §7B
    // correction), only ONE `WUPI.gguf` context may be resident at a time.
    // Keeping a resident FableEngine while no turn is in flight would hold
    // ~75-150MB of Q8_0 KV idle for the whole game session — VRAM the chat
    // engine (Wupi-as-game-manager) needs when the user asks her a question.
    //
    // Instead the engine is spawned LAZILY on the first `fable_send` under
    // the Fable lease (which evicts any resident chat/schema context
    // first). Between turns the FableEngine stays resident (back-to-back
    // same-role reuse — no re-spawn churn during play); it's only torn
    // down when the user asks Wupi something (chat acquires → evicts
    // fable) or on `fable_end`. The `model_path` arg is retained for the
    // lazy spawn in `fable_send`; it's stashed on AppState below.
    //
    // The prior eager-spawn block (which OOM'd the 4th context on 12GB,
    // the 2026-07-26 freeze root cause) is gone.
    {
        if let Ok(mut g) = state.pending_model_path.lock() {
            // Stash for the lazy spawn. If a chat model path was already
            // stashed (the normal case — boot resolved it), preserve it:
            // the FableEngine reuses `shared_model()` anyway, so the path
            // is only a defensive fallback for API-only-with-no-local.
            if g.is_none() {
                *g = Some(model_path);
            }
        }
    }

    // Swap active_card_id + save the pre-game value for restoration. This
    // scopes all memory retrieval + archiving to the roleplay card.
    {
        let mut pre = state.pre_fable_card_id.lock().expect("pre_fable_card_id mutex");
        let mut active = state.active_card_id.lock().expect("active_card_id mutex");
        *pre = active.clone();
        *active = card.id.clone();
    }

    // Resolve initial state. Priority: explicit save_id → fresh → fallback.
    let fable_root = resolve_apps_dir(app).join("fable");
    let (prior_schema, prior_session, resumed_save_label) = if let Some(sid) = save_id.as_deref() {
        let fable_root_clone = fable_root.clone();
        let cid = card.id.clone();
        let sid_owned = sid.to_owned();
        let save = tokio::task::spawn_blocking(move || {
            fable_save::load_save(&fable_root_clone, &cid, &sid_owned)
        })
        .await
        .map_err(|e| format!("load save join: {e}"))?
        .map_err(|e| format!("failed to load save '{sid}': {e}"))?;
        (save.schema, save.session, Some(save.name))
    } else if fresh {
        (schema::WorldSchema::default(), session::Conversation::new(), None)
    } else {
        let s = load_schema(app, &card.id).await
            .unwrap_or_else(schema::WorldSchema::default);
        let c = load_session(app, &card.id).await
            .unwrap_or_else(session::Conversation::new);
        (s, c, None)
    };
    *state.fable_schema.lock().await = prior_schema;
    let messages: Vec<FableLoadMessage> = prior_session
        .messages
        .iter()
        .map(|m| FableLoadMessage {
            role: match m.role {
                session::Role::User => "user",
                session::Role::Assistant => "assistant",
                session::Role::System => "system",
            },
            content: m.content.clone(),
        })
        .collect();
    let turn_count = messages.len();
    *state.fable_session.lock().await = prior_session;
    // Clone the opening scene BEFORE the card is moved into active_fable_card
    // — it's surfaced on FableLoadResult so the UI can render the first
    // narrator beat on a fresh game without a second IPC round-trip.
    let opening_scene = card.opening_scene.clone();
    *state.active_fable_card.lock().expect("active_fable_card mutex") = Some(card);

    // Phase 2: seat the GM persona (best-effort; None → OS catgirl fallback).
    let gm_persona = resolve_gm_sim_path(app)
        .map(|p| sim_card::load_or_fallback(&p))
        .filter(|c| c.id != "__wupi_fallback__");
    if gm_persona.is_none() {
        tracing::info!("fable_start: no gm.sim found — drawer will use the OS catgirl persona");
    }
    *state.fable_persona.lock().expect("fable_persona mutex") = gm_persona;

    tracing::info!(?resumed_save_label, "game started: narrator engine live, memory scoped to card, state loaded");
    Ok(FableLoadResult {
        meta: fable_save::SaveMeta {
            save_id: resumed_save_label.clone().unwrap_or_default(),
            card_id: card_id_for_meta,
            name: resumed_save_label.unwrap_or_else(|| "Current".to_string()),
            summary: String::new(),
            timestamp: current_unix_ms_i64(),
            is_autosave: false,
            turn_count,
        },
        messages,
        opening_scene,
    })
}

/// The title-screen CONTINUE button's resume target. Scans EVERY card's saves
/// dir and returns the single most-recent MANUAL or QUICK save (autosaves are
/// EXCLUDED by contract — the directive is "dim Continue unless the user has a
/// manual or quick save to resume"). `None` (→ JSON null) when there are no
/// qualifying saves, which is the signal for the frontend to disable the
/// CONTINUE button.
///
/// Why a dedicated IPC instead of `fable_list_saves`: that one is per-card
/// and the title sits BEFORE card selection, so CONTINUE must look across all
/// cards for "your last manual save, anywhere."
///
/// Returns a `SaveMeta` (lightweight — no session/schema payload). The
/// frontend loads it via `fable_load_save` only when the user actually clicks
/// CONTINUE.
#[tauri::command]
fn fable_continue_target(app: tauri::AppHandle) -> Result<Option<fable_save::SaveMeta>, String> {
    let fable_root = resolve_apps_dir(&app).join("fable");
    let saves_dir = fable_root.join("saves");
    let Ok(entries) = std::fs::read_dir(&saves_dir) else {
        return Ok(None);
    };
    let mut best: Option<fable_save::SaveMeta> = None;
    for entry in entries.flatten() {
        // Each card has its own subdir: saves/<card_id>/. Skip stray files.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let card_id = entry.file_name().to_string_lossy().to_string();
        if let Ok(saves) = fable_save::list_saves(&fable_root, &card_id) {
            for s in saves {
                // Autosaves don't count toward the CONTINUE gate. The user must
                // have an explicit manual/quick save to resume.
                if s.is_autosave {
                    continue;
                }
                // list_saves is newest-first per-card; track the global newest.
                if best.as_ref().map_or(true, |b| s.timestamp > b.timestamp) {
                    best = Some(s);
                }
            }
        }
    }
    Ok(best)
}

/// Render the narrator prompt in the Gemma4 `<|turn>` protocol: system +
/// windowed message turns + an open `<|turn>model\n` generation cue. Shared
/// by the local FableEngine path and the API-fallback path so both render
/// identical token sequences for a given window (cache-coherence for the
/// local engine's KV, consistency for the API). The FableEngine clears KV
/// every turn, so cache-coherent re-render from raw_output isn't required
/// here; cleaned content is fine.
fn build_narrator_prompt(system_prompt: &str, window: &[session::Message]) -> String {
    let mut prompt = String::with_capacity(4096);
    prompt.push_str("<|turn>system\n");
    prompt.push_str(system_prompt.trim());
    prompt.push_str("<turn|>\n");
    for m in window {
        let role = match m.role {
            session::Role::Assistant => "model",
            session::Role::User => "user",
            session::Role::System => "system",
        };
        prompt.push_str("<|turn>");
        prompt.push_str(role);
        prompt.push('\n');
        prompt.push_str(&m.content);
        prompt.push_str("<turn|>\n");
    }
    prompt.push_str("<|turn>model\n");
    prompt
}

/// Send a narrator turn: render the narrator prompt from the active card +
/// current game schema, post the request to the FableEngine, stream chunks
/// to the Channel, parse bracket commands from the final raw output, and
/// emit them as scene_event messages. After the turn, archive to memory
/// (card-scoped) and fire the schema delta (card-scoped). Mirrors `chat_send`
/// shape but routes to the FableEngine + uses the narrator system prompt.
#[tauri::command]
async fn fable_send(
    text: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    tracing::info!(?text, "fable_send");

    // Fresh cancel token for this turn (Bug #7 pattern, scoped to game).
    let cancel: llm::CancelToken =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
        *slot = Some(Arc::clone(&cancel));
    }

    // Pull the card out under a brief lock, then drop the guard before any
    // .await (std::sync::Mutex guards are !Send). The engine pull happens
    // AFTER the lease acquire below (lazy spawn under the VRAM swap-lock).
    let card = {
        let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
        guard.clone().ok_or_else(|| "no active game card: call fable_start first".to_string())?
    };

    // v0.6.4 VRAM swap-lock: acquire the Fable lease. This evicts any
    // resident chat/schema context (synchronous .join, VRAM freed) BEFORE
    // we spawn-or-reuse the FableEngine — the load-bearing fix for the
    // 2026-07-26 freeze (4 contexts couldn't co-reside on 12GB). The lease
    // guard is held for the duration of the turn; dropping it at end of
    // scope marks the slot free (the resident FableEngine persists until a
    // chat/schema turn evicts it — back-to-back fable turns reuse it).
    //
    // The teardown callback tears down the FableEngine on the next
    // cross-role acquire. It clones the AppState Arcs it needs (the slot
    // + the engine itself) so it can run independently of this task.
    let fable_slot = Arc::clone(&state.fable_engine);
    let lease = state
        .context_swap
        .acquire(
            context_swap::ContextRole::Fable,
            Box::new(move || {
                // Synchronous teardown: take the engine out of the slot +
                // join its thread so VRAM is freed before the next context
                // allocates (the 2026-07-18 VRAM-overlap lesson). Wrapped
                // in spawn_blocking-equivalent inline work — this closure
                // runs on whatever task thread calls the next acquire,
                // blocking it until the join completes. That's correct:
                // the next context's `new_context()` needs this VRAM.
                let engine = {
                    let mut g = fable_slot.lock().map_err(|e| e.to_string())?;
                    g.take()
                };
                if let Some(engine) = engine {
                    let engine = Arc::try_unwrap(engine).map_err(|_| {
                        "fable teardown: other Arc refs still held".to_string()
                    })?;
                    engine.shutdown(); // synchronous .join
                    tracing::info!("context-swap: fable engine torn down (VRAM freed)");
                }
                Ok(())
            }),
        )
        .await;

    // Spawn-or-reuse the FableEngine under the lease. If a prior fable turn
    // left it resident (same-role reuse), this is a fast no-op. If the slot
    // is empty (first turn, or evicted by a chat/schema turn since), spawn
    // a fresh engine + block on readiness. The model path comes from
    // `pending_model_path` (stashed by `enter_fable_session`); the engine
    // prefers `shared_model()` and only uses the path as a fallback.
    let engine = {
        let existing = state.fable_engine.lock().expect("fable_engine mutex").clone();
        match existing {
            Some(e) => {
                tracing::debug!("context-swap: fable engine reused (resident)");
                e
            }
            None => {
                tracing::info!("context-swap: spawning fable engine on demand");
                let path = state
                    .pending_model_path
                    .lock()
                    .expect("pending_model_path mutex")
                    .clone()
                    .ok_or_else(|| "no model path resolved for fable spawn".to_string())?;
                let (engine, init_rx) = fable_engine::FableEngine::spawn_load(path, 99);
                let ready = tokio::task::spawn_blocking(move || init_rx.recv())
                    .await
                    .map_err(|e| format!("game engine init join: {e}"))?
                    .map_err(|e| format!("game engine init channel: {e}"))?;
                match ready {
                    Ok(()) => {
                        let engine = Arc::new(engine);
                        if let Ok(mut slot) = state.fable_engine.lock() {
                            *slot = Some(Arc::clone(&engine));
                        }
                        engine
                    }
                    Err(msg) => {
                        return Err(format!("game engine init failed: {msg}"));
                    }
                }
            }
        }
    };
    // The lease is now held for the duration of this turn. It will be
    // released when `lease` drops at end of scope.
    let _ = &lease;
    // Context size for the API narrator path (the local FableEngine ignores it:
    // it clears KV per turn on its own fixed context).
    let context_size = state.settings.lock().expect("settings mutex").context_size;

    // Build the narrator system prompt from the card + current game schema.
    // The Rust Referee (Fable Seam #7) fires HERE, inside the schema lock,
    // BEFORE the render: it scans the player's turn text for combat/exertion
    // keywords, rolls the dice, and mutates the canonical PlayerState. The
    // outcome then flows into the rendered `<world_state>` block as a hard
    // semantic fact ("Left Bicep (Medium Injury); stamina: Winded") that the
    // narrator reads as truth and writes prose to match. The LLM does ZERO
    // math. See `player_state::referee_evaluate`.
    //
    // We hold the lock across evaluate+apply+render so the persisted state
    // and the injected state are the SAME atomic snapshot — a concurrent
    // autosave can't tear them apart.
    let world_state = {
        let mut s = state.fable_schema.lock().await;
        if let Some(outcome) = player_state::referee_evaluate(&text, &s.player_state) {
            tracing::info!(
                part = outcome.part.id(),
                state = ?outcome.new_state,
                stamina = ?outcome.stamina_after,
                "referee fired on combat/exertion keyword"
            );
            player_state::apply_outcome(&mut s.player_state, &outcome);
        }
        let rendered = s.render_for_prompt();
        if rendered.is_empty() { None } else { Some(rendered) }
    };
    let system_prompt = narrator_prompt::build_narrator_system_prompt(&card, world_state.as_deref());

    // Append the user turn to the per-card game conversation, then window the
    // visible history. Same sliding-window strategy as chat_send's VISIBLE_WINDOW
    // (§2I M2): old turns drop from the prompt (memory backfills via retrieval)
    // so the prompt stays small (~5KB not ~80KB). The full conversation is
    // persisted on fable_end so games resume across reboots.
    //
    // v0.6.3: window doubles when the API is the active chat source, matching
    // chat_send's 4 → 12. The cloud model has the context budget and the local
    // model stays hot as the silent agent doing schema tracking.
    //
    // 2026-07-26 v2: the local window is 8 *messages* (4 beats — 4 user actions
    // + 4 narrator replies). The slice math at line ~4174 treats the window as
    // a raw message count (the code is the source of truth). Token math: the
    // narrator system prompt (~1000 tok) + 8 messages (~1200 tok at ~150/msg)
    // + 1024-token gen reserve ≈ 3224, fitting FABLE_CTX=3072 with ~270 tok
    // headroom; if a beat runs long, truncate_to_fit drops oldest turns.
    //
    // v0.7: the API window was cut 16 → 12 (6 beats). 12 narrator messages
    // + the ~1000-tok narrator prompt + generation fits comfortably inside
    // the API profile's max_context (default 8192), and 12 matches the chat
    // API window for a consistent "6 beats of recent history" feel across
    // both surfaces.
    let source = *state.model_source.lock().expect("model_source mutex");
    let fable_visible_window = if source == api::ModelSource::Api { 12 } else { 8 };
    {
        let mut gs = state.fable_session.lock().await;
        gs.add_message(session::Role::User, text.clone());
    }

    // Build a windowed prompt: system + last fable_visible_window messages +
    // generation cue. Same Gemma4 `<|turn>` protocol the chat path uses
    // (assistant → "model"). We render inline (no ChatFormat trait dependency)
    // because the narrator prompt is a single-shot prefill into the FableEngine
    // (no KV-cache reuse across turns: the FableEngine clears KV every turn,
    // see fable_engine.rs:375). So cache-coherent re-render from raw_output
    // isn't required here; cleaned content is fine.
    let window: Vec<session::Message> = {
        let gs = state.fable_session.lock().await;
        let msgs = &gs.messages;
        let start = msgs.len().saturating_sub(fable_visible_window);
        msgs[start..].to_vec()
    };

    // Streaming callback wraps the Channel send.
    let on_chunk: llm::ChunkFn = Arc::new({
        let on_event = on_event.clone();
        move |piece: &str| {
            let _ = on_event.send(serde_json::json!({ "type": "chunk", "text": piece }));
        }
    });

    // v0.6.3 API routing for the narrator: when an API is selected, route the
    // narrator turn through HttpBackend (the cloud model narrates; the local
    // model stays the silent agent doing tracking, NOT the narration per the
    // spec). On any API error, seamlessly fall back to the local FableEngine
    // at the 8-window — immersion preserved, no error surfaced. The local
    // FableEngine is always resident while a game is running (its KV is
    // cleared per turn, so the fallback window matches its expected shape).
    //
    // The reply shape ({ content, reasoning, raw }) is normalized to the
    // FableEngine's EngineReply ({ content, raw_output, error }) so the
    // downstream bracket-parsing + archival path is identical for both.
    let reply: fable_engine::FableReply = if source == api::ModelSource::Api {
        let profile_opt = {
            let cfg = state.api_config.lock().expect("api_config mutex");
            cfg.active_profile().cloned()
        };
        let attempted_api = profile_opt.is_some();
        let api_outcome: Option<chat_format::ParsedOutput> = if let Some(profile) = profile_opt {
            // Re-render the windowed history as flat ApiMessages for the
            // HTTP path (system + windowed turns; the API folds memory +
            // world_state into the system message itself).
            let mut api_msgs: Vec<session::ApiMessage> =
                Vec::with_capacity(window.len() + 1);
            api_msgs.push(session::ApiMessage {
                role: "system".into(),
                content: system_prompt.trim().to_string(),
                raw_output: String::new(),
            });
            for m in &window {
                let role = match m.role {
                    session::Role::Assistant => "model",
                    session::Role::User => "user",
                    session::Role::System => "system",
                };
                api_msgs.push(session::ApiMessage {
                    role: role.into(),
                    content: m.content.clone(),
                    raw_output: String::new(),
                });
            }
            let http = llm::HttpBackend::new(profile);
            match http
                .stream(api_msgs, None, None, Vec::new(), context_size, on_chunk.clone(), cancel.clone())
                .await
            {
                Ok(out) => Some(out),
                Err(e) => {
                    tracing::warn!(error = %e, "fable_send: API narrator failed; falling back to local FableEngine");
                    let _ = on_event.send(serde_json::json!({
                        "type": "fallback",
                        "reason": "api_unreachable",
                        "source": "local_narrator",
                    }));
                    None
                }
            }
        } else {
            None
        };
        match api_outcome {
            Some(out) => fable_engine::FableReply {
                // The API has no Gemma4 protocol markers, so raw_output is
                // just the content. extract_reply_channel downstream is a
                // no-op on marker-free text (rsplit_once finds no
                // "<channel|>", returns the input unchanged).
                raw_output: if out.raw.is_empty() { out.content } else { out.raw },
                error: String::new(),
                cancelled: false,
            },
            None => {
                // Fallback (API failed OR no profile): run the local FableEngine.
                // Re-render the prompt at the 8-window for cache-coherence
                // (the local engine only ever saw the 8-window render).
                if attempted_api {
                    let gs = state.fable_session.lock().await;
                    let msgs = &gs.messages;
                    let start = msgs.len().saturating_sub(8);
                    let fallback_window: Vec<session::Message> = msgs[start..].to_vec();
                    drop(gs);
                    let prompt = build_narrator_prompt(&system_prompt, &fallback_window);
                    let reply_rx = engine
                        .request_turn(prompt, on_chunk.clone(), cancel.clone())
                        .map_err(|e| format!("{e:#}"))?;
                    tokio::task::spawn_blocking(move || reply_rx.recv())
                        .await
                        .map_err(|e| format!("game reply join: {e}"))?
                        .map_err(|e| format!("game reply channel: {e}"))?
                } else {
                    let prompt = build_narrator_prompt(&system_prompt, &window);
                    let reply_rx = engine
                        .request_turn(prompt, on_chunk.clone(), cancel.clone())
                        .map_err(|e| format!("{e:#}"))?;
                    tokio::task::spawn_blocking(move || reply_rx.recv())
                        .await
                        .map_err(|e| format!("game reply join: {e}"))?
                        .map_err(|e| format!("game reply channel: {e}"))?
                }
            }
        }
    } else {
        // Local-only path (the default): render the prompt + run the FableEngine.
        let prompt = build_narrator_prompt(&system_prompt, &window);
        let reply_rx = engine
            .request_turn(prompt, on_chunk.clone(), cancel.clone())
            .map_err(|e| format!("{e:#}"))?;
        tokio::task::spawn_blocking(move || reply_rx.recv())
            .await
            .map_err(|e| format!("game reply join: {e}"))?
            .map_err(|e| format!("game reply channel: {e}"))?
    };

    // Clear the cancel slot now that the turn is done.
    {
        let mut slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
        *slot = None;
    }

    if !reply.error.is_empty() {
        on_event
            .send(serde_json::json!({ "type": "error", "message": reply.error }))
            .map_err(|e| e.to_string())?;
        return Ok(());
    }

    // Parse bracket commands from the final raw output + emit them as
    // scene_event messages (one per command). The cleaned prose is emitted
    // as a final "narration" message so the UI renders it as the dialogue.
    //
    // First strip the Gemma4 channel-protocol wrapping (the model emits
    // `<|channel>thought\n...<channel|>reply`: we only want the reply).
    // Reuses `schema::extract_reply_channel` (the same rsplit_once helper
    // the schema engine + chat_format use). Without this the protocol
    // markers leak into the narrator prose: runtime-discovered during the
    // 2026-07-18 MVP test. Also strips `<audio|>` (the Gemma4 audio-channel
    // closer the model emits mid-prose; without this it leaks as literal
    // text like "Mira's voice is a<audio|> whisper").
    let cleaned_raw = schema::extract_reply_channel(&reply.raw_output);
    let parsed = bracket_parser::parse(&cleaned_raw);
    for cmd in &parsed.commands {
        on_event
            .send(serde_json::json!({ "type": "scene_event", "command": cmd }))
            .map_err(|e| e.to_string())?;
    }

    // Archive both turns to the card-scoped memory. Best-effort, detached,
    // same pattern as chat_send's pillar-2 archive.
    let card_id = state.active_card_id.lock().expect("active_card_id mutex").clone();
    if let Some(memory_engine) = state.memory.get() {
        let memory_engine = Arc::clone(memory_engine);
        let user_text = text.clone();
        let asst_text = parsed.prose.clone();
        tokio::spawn(async move {
            if let Err(e) = memory_engine
                .add_memory(user_text, &card_id, memory::Role::User, 1.0)
                .await
            {
                tracing::warn!(error = %format!("{e:#}"), "archive game user turn failed");
            }
            if let Err(e) = memory_engine
                .add_memory(asst_text, &card_id, memory::Role::Assistant, 1.0)
                .await
            {
                tracing::warn!(error = %format!("{e:#}"), "archive game assistant turn failed");
            }
        });
    }

    // Phase 3: append the assistant turn to the per-card game conversation so
    // the next turn's windowed prompt includes it. We store the CLEANED prose
    // (parsed.prose): the FableEngine clears its KV cache every turn (no delta-
    // prefill), so cache-coherent raw_output re-render isn't required here
    // (unlike the chat path's Bug #3 fix). The reasoning channel is empty for
    // narrator turns (the bracket parser doesn't extract a thought channel).
    {
        let mut gs = state.fable_session.lock().await;
        gs.add_assistant_turn(parsed.prose.clone(), String::new(), reply.raw_output.clone());
    }

    // Auto-save (best-effort, fire-and-forget). Every narrator turn writes
    // the reserved `autosave` slot so the player never loses more than one
    // turn to a crash / quit. The save is small JSON (a few KB) and runs on
    // the blocking pool; errors are logged-and-dropped (autosave is
    // best-effort by contract — never block the gameplay loop on it).
    {
        let app_clone = app.clone();
        let state_active_card = state.active_fable_card.clone();
        let state_session = state.fable_session.clone();
        let state_schema = state.fable_schema.clone();
        tokio::spawn(async move {
            let card_opt = state_active_card.lock().expect("active_fable_card mutex").clone();
            let Some(card) = card_opt else { return };
            let session = state_session.lock().await.clone();
            let schema = state_schema.lock().await.clone();
            let fable_root = resolve_apps_dir(&app_clone).join("fable");
            let fable_root_clone = fable_root.clone();
            let card_clone = card.clone();
            let result = tokio::task::spawn_blocking(move || {
                fable_save::write_save(
                    &fable_root_clone,
                    &card_clone,
                    fable_save::AUTOSAVE_ID,
                    "Autosave",
                    &session,
                    &schema,
                )
            })
            .await;
            match result {
                Ok(Ok(_)) => tracing::debug!("autosave: ok"),
                Ok(Err(e)) => tracing::warn!(error = %format!("{e}"), "autosave write failed"),
                Err(e) => tracing::warn!(error = %format!("{e}"), "autosave join failed"),
            }
        });
    }

    on_event
        .send(serde_json::json!({
            "type": "done",
            "final_text": parsed.prose,
            "cancelled": reply.cancelled,
        }))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Cancel the in-flight narrator turn (parallel to `chat_stop`). Signals
/// the per-request token; the engine's decode loop checks it between tokens
/// and breaks cleanly (§2C KV-consistency contract).
#[tauri::command]
async fn fable_stop(state: tauri::State<'_, AppState>) -> Result<(), String> {
    tracing::info!("fable_stop requested");
    let slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
    if let Some(cancel) = slot.as_ref() {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// End the game: shut down the FableEngine (frees VRAM), persist the per-card
/// session + schema (Phase 3: resumable across reboots), restore the
/// pre-game `active_card_id`, clear the game state. After this, Wupi-assistant
/// chat works exactly as before the game (memory retrieval + schema delta
/// scope back to the system card).
#[tauri::command]
async fn fable_end(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    tracing::info!("fable_end: shutting down FableEngine");

    // 1. Take the engine out of AppState (so concurrent fable_send sees None
    //    and bails), then shut it down. shutdown() blocks on the JoinHandle
    //    until VRAM is freed (load-bearing: see FableEngine::shutdown doc).
    let engine_opt = {
        let mut guard = state.fable_engine.lock().expect("fable_engine mutex");
        guard.take()
    };
    if let Some(engine) = engine_opt {
        tokio::task::spawn_blocking(move || engine.shutdown())
            .await
            .map_err(|e| format!("game engine shutdown join: {e}"))?;
    }

    // 2. Phase 3 per-card persistence: capture the roleplay card id BEFORE
    //    the restore (step 3 swaps active_card_id back to the system value),
    //    then save the session + schema under the roleplay id. Both saves are
    //    best-effort: a failure logs a warning but doesn't block fable_end
    //    (the in-memory state is cleared regardless; the user just loses the
    //    resume point on a disk error, not the running game).
    let roleplay_card_id = state.active_card_id.lock().expect("active_card_id mutex").clone();
    if roleplay_card_id != memory::WUPI_CARD_ID {
        let schema_snapshot = state.fable_schema.lock().await.clone();
        let session_snapshot = state.fable_session.lock().await.clone();
        save_schema(&app, &roleplay_card_id, &schema_snapshot).await;
        save_session(&app, &roleplay_card_id, &session_snapshot).await;
        tracing::info!(card_id = %roleplay_card_id, "per-card state saved");
    }

    // 3. Restore the pre-game card id + clear the game-scoped state.
    {
        let pre = state.pre_fable_card_id.lock().expect("pre_fable_card_id mutex").clone();
        *state.active_card_id.lock().expect("active_card_id mutex") = pre;
    }
    *state.fable_schema.lock().await = schema::WorldSchema::default();
    *state.fable_session.lock().await = session::Conversation::new();
    *state.active_fable_card.lock().expect("active_fable_card mutex") = None;
    // Phase 2: drop the GM persona so OS chat outside Fable stays the catgirl.
    *state.fable_persona.lock().expect("fable_persona mutex") = None;

    // 4. Clear any leftover game cancel token.
    *state.active_fable_cancel.lock().expect("active_fable_cancel mutex") = None;

    tracing::info!("game ended: narrator engine down, per-card state persisted, memory scope restored");
    Ok(())
}

/// List all save slots for a card. Used by the Load Game screen. Most-recent
/// first (sorted in `fable_save::list_saves`). Returns an empty Vec when the
/// card has no saves dir yet (fresh install / never saved).
#[tauri::command]
fn fable_list_saves(
    card_id: String,
    app: tauri::AppHandle,
) -> Result<Vec<fable_save::SaveMeta>, String> {
    let fable_root = resolve_apps_dir(&app).join("fable");
    fable_save::list_saves(&fable_root, &card_id)
        .map_err(|e| format!("list saves for '{card_id}': {e}"))
}

/// Read the canonical PlayerState for the active Fable game (Seam #7).
/// Returns it as a JSON value the frontend's mannequin/stats panel renders
/// directly: `{ body: { head: "Transparent", left_bicep: "Orange", ... },
/// stamina: "Winded", wealth: 12, reputation: -3 }`. Body part keys are the
/// 16 stable `id()` strings ("head", "left_bicep", …); values are the
/// `BodyPartState` variants serialized as their PascalCase names.
///
/// Returns an error if no Fable game is active (the left stats panel only
/// exists inside a running session). The lock is held briefly under
/// `fable_schema` (tokio Mutex) — same field the narrator render reads, so
/// the panel always sees the same state the last turn was narrated against.
#[tauri::command]
async fn player_state_get(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    // Bail when no game is active — the stats panel shouldn't be queryable
    // from the title screen. Cheap check under the std Mutex.
    {
        let guard = state.fable_engine.lock().expect("fable_engine mutex");
        if guard.is_none() {
            return Err("no fable game active: call fable_start first".to_string());
        }
    }
    let s = state.fable_schema.lock().await;
    Ok(serde_json::to_value(&s.player_state)
        .map_err(|e| format!("serialize player state: {e}"))?)
}

/// Save the current game state into a named slot. `save_id` of "autosave"
/// writes the reserved auto-save slot; any other id is a named slot. `name`
/// is the human label (e.g. "Before the dragon" or "Autosave"). The UI
/// typically calls this with `save_id = autosave` after each turn and with
/// a fresh timestamped id when the user picks "Save" from the pause menu.
#[tauri::command]
async fn fable_save_now(
    save_id: String,
    name: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<fable_save::SaveMeta, String> {
    let card = {
        let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
        guard.clone().ok_or_else(|| "no active game: call fable_start first".to_string())?
    };
    let (session, schema) = {
        let session = state.fable_session.lock().await.clone();
        let schema = state.fable_schema.lock().await.clone();
        (session, schema)
    };
    let fable_root = resolve_apps_dir(&app).join("fable");
    let save_id_arg = save_id.clone();
    let name_arg = name.clone();
    let card_clone = card.clone();
    let session_clone = session.clone();
    let schema_clone = schema.clone();
    let fable_root_clone = fable_root.clone();
    let saved = tokio::task::spawn_blocking(move || {
        fable_save::write_save(
            &fable_root_clone,
            &card_clone,
            &save_id_arg,
            &name_arg,
            &session_clone,
            &schema_clone,
        )
    })
    .await
    .map_err(|e| format!("save join: {e}"))?
    .map_err(|e| format!("save write: {e}"))?;
    tracing::info!(save_id = %saved.save_id, card_id = %saved.card_id, "game saved");
    Ok(fable_save::SaveMeta {
        save_id: saved.save_id,
        card_id: saved.card_id,
        name: saved.name,
        summary: saved.summary,
        timestamp: saved.timestamp,
        is_autosave: saved.is_autosave,
        turn_count: saved.session.messages.len(),
    })
}

/// Load a named save into the running game. This OVERWRITES the current
/// session/schema state in memory (any unsaved progress is lost — the UI
/// should confirm). Doesn't restart the engine (it's already running);
/// just swaps the in-memory state. The next `fable_send` will use the loaded
/// history.
///
/// Returns the save meta PLUS the loaded messages as `{role, content}` pairs
/// so the UI can re-render the dialogue feed without a second IPC. This is
/// the pause-menu Load path: a game is already running on the stage, so
/// `fable_start` is the wrong IPC (it hard-fails with "a game is already
/// running"); use this hot-swap instead.
#[tauri::command]
async fn fable_load_save(
    save_id: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<FableLoadResult, String> {
    let card = {
        let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
        guard.clone().ok_or_else(|| "no active game: call fable_start first".to_string())?
    };
    let fable_root = resolve_apps_dir(&app).join("fable");
    let fable_root_clone = fable_root.clone();
    let cid = card.id.clone();
    let sid = save_id.clone();
    let save = tokio::task::spawn_blocking(move || {
        fable_save::load_save(&fable_root_clone, &cid, &sid)
    })
    .await
    .map_err(|e| format!("load join: {e}"))?
    .map_err(|e| format!("load read: {e}"))?;
    let meta = fable_save::SaveMeta {
        save_id: save.save_id,
        card_id: save.card_id,
        name: save.name,
        summary: save.summary,
        timestamp: save.timestamp,
        is_autosave: save.is_autosave,
        turn_count: save.session.messages.len(),
    };
    // Snapshot the messages for the UI BEFORE we move the session into state.
    let messages: Vec<FableLoadMessage> = save
        .session
        .messages
        .iter()
        .map(|m| FableLoadMessage {
            role: match m.role {
                session::Role::User => "user",
                session::Role::Assistant => "assistant",
                session::Role::System => "system",
            },
            content: m.content.clone(),
        })
        .collect();
    *state.fable_session.lock().await = save.session;
    *state.fable_schema.lock().await = save.schema;
    tracing::info!(save_id = %meta.save_id, "game state loaded");
    // opening_scene is None on a save-load: resumed games render their feed
    // from `messages`, not from the card's opening beat. Only fresh starts
    // (enter_fable_session) surface the opening scene.
    Ok(FableLoadResult { meta, messages, opening_scene: None })
}

/// Result of `fable_load_save`: the save meta + a flat list of the loaded
/// messages so the UI can re-render its dialogue feed in one round-trip.
#[derive(Debug, Clone, serde::Serialize)]
struct FableLoadResult {
    meta: fable_save::SaveMeta,
    messages: Vec<FableLoadMessage>,
    /// The card's full `<opening_scene>` text (untruncated). Surfaced so the
    /// UI can render the first narrator beat on a FRESH game (no resumed
    /// messages yet) without a second IPC round-trip. `None` when the card
    /// has no opening scene. NOTE: this is the FULL text — the per-card
    /// `opening_scene_preview` on `FableCardMeta` (capped at 240 chars) is
    /// only for the launcher card picker, NOT for the first beat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    opening_scene: Option<String>,
}

/// One loaded message, role as a lowercase string (matches `session::Role`'s
/// `rename_all = "lowercase"` serialization but trimmed to just role+content
/// so we don't leak `raw_output`/`reasoning`/`id`/`timestamp` to the UI).
#[derive(Debug, Clone, serde::Serialize)]
struct FableLoadMessage {
    role: &'static str,
    content: String,
}

/// Delete a save slot. Idempotent (Ok if the save is already gone).
#[tauri::command]
fn fable_delete_save(
    card_id: String,
    save_id: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let fable_root = resolve_apps_dir(&app).join("fable");
    fable_save::delete_save(&fable_root, &card_id, &save_id)
        .map_err(|e| format!("delete save '{save_id}': {e}"))
}

/// Resolve the roleplay scenario cards dir. Per §8C scenario `.sim` files
/// live at `<exe_dir>/apps/games/cards/`. Returns `None` if no such dir
/// exists in any candidate location (graceful: the picker shows empty).
fn resolve_fable_cards_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // §8C layout FIRST: scenario cards are user-state, live under
    // `<exe_dir>/apps/fable/cards/` (shipped empty; populated by the future
    // scenario-card authoring flow).
    candidates.push(resolve_apps_dir(app).join("fable").join("cards"));
    // DEV-REPO walk-up for the §8C path. `resolve_apps_dir` returns
    // `<exe_dir>/apps`, which in `cargo run` / `tauri dev` is
    // `target/{debug,release}/apps` — NOT the project-root `apps/` where
    // scenario cards actually live during development. Walk up from the exe
    // dir looking for `apps/fable/cards` so dev finds the source-tree cards
    // (e.g. `C:\WUPI\apps\fable/cards`). Same climb pattern as the legacy
    // `cards/fable_cards` walk-up below; in a shipped portable build the exe
    // sits at the install root so `<exe_dir>/apps/fable/cards` already hit on
    // candidate #1 and these never fire.
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("apps").join("fable").join("cards"));
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(grand.join("apps").join("fable").join("cards"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                candidates.push(gg.join("apps").join("fable").join("cards"));
            }
        }
    }
    // Legacy pre-§8C layout (`cards/fable_cards/`) + dev-repo paths. Kept as
    // fallbacks so a v0.2.4 → v0.3.0 in-place upgrade still finds any
    // pre-existing scenarios under the old path.
    if let Some(d) = app.path().resource_dir().ok() {
        candidates.push(d.join("cards").join("fable_cards"));
    }
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("cards").join("fable_cards"));
            if let Some(grand) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(grand.join("cards").join("fable_cards"));
            }
            if let Some(gg) = parent.parent().and_then(|g| g.parent()).and_then(|g| g.parent()) {
                candidates.push(gg.join("cards").join("fable_cards"));
            }
        }
    }

    for dir in &candidates {
        if dir.is_dir() {
            tracing::info!("resolved fable_cards dir: {}", dir.display());
            return Some(dir.clone());
        }
    }
    None
}

/// Find a roleplay card by id within `apps/games/cards/` (§8C). Returns an
/// error string (not a panic) if no card with that id exists.
fn find_card_by_id(dir: &std::path::Path, target_id: &str) -> Result<sim_card::SimCard, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read fable_cards/: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("sim") {
            continue;
        }
        let card = sim_card::load_or_fallback(&path);
        if card.id == target_id && card.card_type == "roleplay" {
            return Ok(card);
        }
    }
    Err(format!("no roleplay card with id '{target_id}' in {}", dir.display()))
}

/// Current time as unix milliseconds. Matches the timestamp convention
/// used by `session::Message` + `fable_save::SaveFile`. Inline (not shared
/// with `session::chrono_now_millis`, which is private) because this is a
/// one-off display hint in the synthesized `SaveMeta`.
fn current_unix_ms_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Persist the session off the Tokio worker pool.
///
/// `Conversation::save` is atomic (temp + fsync + rename, see §2E) but
/// synchronous: `File::create` / `write_all` / `sync_all` / `rename` all
/// block the calling thread on the disk. Running that on a Tokio worker
/// (which is what the old sync `save_session` did) stalls the async runtime
/// for the duration of the write + fsync. Harmless today (one user, one
/// chat, save is ~ms on SSD), but the moment the Memory engine adds
/// concurrent async work racing the save, a blocked worker becomes a real
/// stall. `spawn_blocking` moves the I/O onto the dedicated blocking thread
/// pool (default 512 threads) so workers stay free to poll futures.
///
/// The session mutex guard is still held across the `.await` by the caller
///: that's correct for a `tokio::sync::Mutex` (its guard is await-safe) and
/// serializes concurrent saves, which we want anyway.
///
/// **Phase 3 per-card persistence (AGENTS.md §2AA):** now scoped by `card_id`
/// → `sessions/<card_id>.json`. The Wupi-assistant session stays ephemeral
/// (§2K); only roleplay game sessions persist (a card carries its own
/// resumable session). The atomic-save machinery is reused as-is.
async fn save_session(
    app: &tauri::AppHandle,
    card_id: &str,
    conv: &session::Conversation,
) {
    let fable_root = resolve_apps_dir(app).join("fable");
    let path = resolve_session_path(&fable_root, card_id);
    // Clone so the closure owns its data (spawn_blocking needs 'static). The
    // Conversation is a Vec of small messages: cheap to clone relative to a
    // disk fsync.
    let conv = conv.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = conv.save(&path) {
            tracing::warn!(?e, "failed to persist session");
        }
    })
    .await;
}

/// Load a card-scoped session. Returns a fresh empty `Conversation` when no
/// saved file exists (the `Conversation::load` NotFound path already does
/// this: we just route through it). Symmetric to `save_session`.
async fn load_session(
    app: &tauri::AppHandle,
    card_id: &str,
) -> Option<session::Conversation> {
    let fable_root = resolve_apps_dir(app).join("fable");
    let path = resolve_session_path(&fable_root, card_id);
    let path_cloned = path.clone();
    tokio::task::spawn_blocking(move || session::Conversation::load(&path_cloned))
        .await
        .ok()?
        .ok()
}

/// Persist the world-state schema off the Tokio worker pool. Mirrors
/// `save_session`: `WorldSchema::save` is atomic (temp + fsync + rename) but
/// synchronous, so `spawn_blocking` keeps the async runtime free.
///
/// **Phase 3:** now scoped by `card_id` → `schemas/<card_id>.json`. Only the
/// active game's schema persists (Wupi-assistant's schema stays ephemeral).
async fn save_schema(
    app: &tauri::AppHandle,
    card_id: &str,
    schema: &schema::WorldSchema,
) {
    let fable_root = resolve_apps_dir(app).join("fable");
    let path = resolve_schema_path(&fable_root, card_id);
    let schema = schema.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = schema.save(&path) {
            tracing::warn!(?e, "failed to persist world schema");
        }
    })
    .await;
}

/// Load a card-scoped world schema. Returns a fresh default `WorldSchema`
/// when no saved file exists (the `WorldSchema::load` NotFound path already
/// does this). Symmetric to `save_schema`.
async fn load_schema(
    app: &tauri::AppHandle,
    card_id: &str,
) -> Option<schema::WorldSchema> {
    let fable_root = resolve_apps_dir(app).join("fable");
    let path = resolve_schema_path(&fable_root, card_id);
    let path_cloned = path.clone();
    tokio::task::spawn_blocking(move || schema::WorldSchema::load(&path_cloned))
        .await
        .ok()?
        .ok()
}

/// `<fable_root>/sessions/<card_id>.json`. Per §8C roleplay sessions live
/// under `apps/games/sessions/` (scoped under games/ because only roleplay
/// game sessions persist today; Wupi-assistant chat stays ephemeral per §5).
/// The `sessions/` subdir is created once in `setup()`. The card_id is the
/// filename stem: roleplay card ids are filesystem-safe (lowercased,
/// derived from `<metadata><id>` in `sim_card.rs`).
fn resolve_session_path(fable_root: &std::path::Path, card_id: &str) -> std::path::PathBuf {
    fable_root.join("sessions").join(format!("{card_id}.json"))
}

/// `<fable_root>/schemas/<card_id>.json`. Sibling to `resolve_session_path`;
/// same subdir convention + filesystem-safety assumption.
fn resolve_schema_path(fable_root: &std::path::Path, card_id: &str) -> std::path::PathBuf {
    fable_root.join("schemas").join(format!("{card_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_local_ctx_local_returns_settings() {
        let settings = prompts::WupiSettings {
            context_size: 4000,
            conversation_budget: 16000,
        };
        assert_eq!(
            effective_local_ctx(api::ModelSource::Local, &settings),
            4000,
            "Local mode must return the user's tuned settings.context_size"
        );
    }

    #[test]
    fn effective_local_ctx_api_returns_2048_regardless_of_settings() {
        // Even if the user has a huge settings.context_size, under API the
        // local chat backend demotes to 2048 (silent-agent mode). The settings
        // value must NOT leak through.
        let settings = prompts::WupiSettings {
            context_size: 8192,
            conversation_budget: 16000,
        };
        assert_eq!(
            effective_local_ctx(api::ModelSource::Api, &settings),
            2048,
            "API mode must always return 2048 regardless of settings.context_size"
        );
    }

    #[test]
    fn effective_local_ctx_default_settings() {
        // Default settings (context_size: 4000) under Local → 4000.
        let settings = prompts::WupiSettings::default();
        assert_eq!(effective_local_ctx(api::ModelSource::Local, &settings), 4000);
        // Under Api → 2048 (the constant, not the default).
        assert_eq!(effective_local_ctx(api::ModelSource::Api, &settings), 2048);
    }
}
