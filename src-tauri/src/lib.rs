pub mod api;
pub mod boot_preflight;
pub mod bracket_parser;
pub mod chat_format;
pub mod codex;
pub mod consequence;
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
pub mod equipment;
pub mod model_downloader;
pub mod offscreen_task;
pub mod player_state;
pub mod player;
pub mod schema;
pub mod schema_engine;
pub mod schema_validator;
pub mod settings;
pub mod relationship;
pub mod scene_pacing;
pub mod scene_art;
pub mod prism;
pub mod prompts;
pub mod session;
pub mod sim_card;
pub mod shortcut;
pub mod stream_filter;
pub mod system_menu;
pub mod theme;
pub mod layout;
pub mod updater;
pub mod user_profile;
pub mod tools;
pub mod weather;
pub mod rumor;

use std::sync::Arc;
use tauri::{Emitter, Manager};
use llm::GenerationClient;

// Re-export the shared Windows boot preflight so both launcher binaries call
// it as `wupi_lib::windows_preflight()` (src/main.rs + src/bin/fable.rs).
pub use boot_preflight::windows_preflight;

// ── fable.exe entry mode ──────────────────────────────────────────────
// `fable.exe` (src/bin/fable.rs) is a second launcher binary sharing this
// crate's ENTIRE boot code. It calls set_fable_entry() before run() so the
// main window is built with the URL `wupi.html#fable` instead of
// `wupi.html`. The frontend detects `#fable` (script.js + fable.js) and
// skips the OS boot ceremony + Fable's fog-gate/ripple, landing straight on
// the Fable title screen — "no bootup, loading screen, or ripple effect."
// Defaults to false → wupi.exe boots normally with `wupi.html`.
static FABLE_ENTRY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this process as the fable.exe launcher (called only by
/// `src/bin/fable.rs`, before `run()`). Read once in setup() to choose the
/// main window's initial URL.
pub fn set_fable_entry() {
    FABLE_ENTRY.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// True when this process is the fable.exe launcher — the main window should
/// load `wupi.html#fable` (the Fable entry marker) instead of `wupi.html`.
pub fn is_fable_entry() -> bool {
    FABLE_ENTRY.load(std::sync::atomic::Ordering::SeqCst)
}

// ── Direct-launch context (fable.exe --card <slug> [--save <id>]) ──────────
// A sibling of the FABLE_ENTRY flag: the fable.exe bin parses `--card` /
// `--save` from argv (src/bin/fable.rs) and stashes them here before run().
// setup() then appends `?direct=1` to the window URL so the frontend knows to
// skip the title + boot straight into the card; the frontend reads the actual
// values back via the `get_launch_context` IPC. `None` for wupi.exe + a
// no-arg fable.exe launch (lands on the title as usual).
static LAUNCH_CONTEXT: std::sync::OnceLock<Option<LaunchContext>> = std::sync::OnceLock::new();

/// The parsed fable.exe CLI launch target. `save_id` follows `fable_start`
/// semantics: `None` = Continue (live session.json), `Some(id)` = a named
/// `saves/<id>.json` slot.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchContext {
    pub card_slug: String,
    pub save_id: Option<String>,
}

/// Stash the direct-launch target (called only by `src/bin/fable.rs`, before
/// `run()`). Read once in setup() (to pick the URL) + by get_launch_context.
pub fn set_launch_context(card_slug: String, save_id: Option<String>) {
    let _ = LAUNCH_CONTEXT.set(Some(LaunchContext {
        card_slug,
        save_id,
    }));
}

/// The direct-launch target, or `None` when fable.exe was launched with no
/// `--card` (or this is wupi.exe).
pub fn launch_context() -> Option<&'static LaunchContext> {
    LAUNCH_CONTEXT.get().and_then(|opt| opt.as_ref())
}

/// Frontend read of the direct-launch target. Returns the stashed context or
/// `null` (wupi.exe / a no-arg fable.exe launch). The `?direct=1` URL marker
/// is set iff this is `Some`, so the frontend only calls this when it already
/// knows a direct launch is in flight.
#[tauri::command]
fn get_launch_context() -> Option<LaunchContext> {
    launch_context().cloned()
}

/// The Memory engine's concrete embedder type, decided ONCE at startup. Using
/// `Box<dyn Embedder + Send + Sync>` lets `AppState` hold one concrete
/// `MemoryEngine` regardless of whether `Embed.gguf` was found: `LlamaCppEmbedder`
/// (real BERT backend) or `StubEmbedder` (byte-histogram fallback) both box into
/// this slot. One virtual call per `embed`, negligible next to multi-ms GPU work.
/// The `Embedder` trait is verified dyn-compatible (no `Self`, no generic
/// methods, manually-desugared `EmbedFuture` instead of `async fn`).
pub type DynEmbedder = Box<dyn memory_embedder::Embedder + Send + Sync>;

/// Minimal chat settings. The context-size field is retained for the
/// AppState field + the `effective_local_ctx` signature, but is no longer the
/// source of truth for context sizing (2026-07-31): those values now live in
/// `settings.rs` as named constants. The `context_size` field is read by
/// nothing on the hot path — see `effective_local_ctx` (returns the
/// `settings::` constants regardless of this field).
#[derive(Clone, Default)]
pub struct WupiSettings {
    pub context_size: u32,
    pub conversation_budget: u32,
}

#[derive(Clone)]
pub struct AppState {
    pub session: Arc<tokio::sync::Mutex<session::Conversation>>,
    pub backend: Arc<std::sync::Mutex<Option<Arc<llm::LlamaCppBackend>>>>,
    pub settings: Arc<std::sync::Mutex<WupiSettings>>,
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
    /// Prism (2026-07-31): the image-gallery database (`apps/prism/gallery
    /// .sqlite`). Same shape as `memory` (Arc<OnceLock<Arc<...>>>) — set once
    /// at boot, read lock-free thereafter. Holds the SQLite gallery that maps
    /// each generated image to its prompt/seed/cfg/steps/sampler/model
    /// metadata. Opened in `setup()` alongside the memory DB; `None` only if
    /// the open failed (best-effort — a gallery-open failure doesn't kill boot,
    /// the gallery screens degrade to "empty + read-only").
    pub prism_db: Arc<std::sync::OnceLock<Arc<prism::GalleryDb>>>,
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
    /// Fail-proof delta contract (§5 layer 3) — World Progression tick path
    /// (Fable Seam #4, 2026-07-27). Tick attempts that exhausted all 3
    /// passes WITHOUT committing. Drained on the next tick fire + folded
    /// into its prompt so the model gets another shot with the new interval
    /// as anchor. Same cap (`MAX_FAILED_DELTA_ATTEMPTS`) + FIFO eviction as
    /// the delta + translation siblings.
    pub failed_progression_queue: Arc<tokio::sync::Mutex<Vec<schema_engine::FailedAttempt>>>,
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
    /// The authored Fable prompt file (`data/fable.prompt`), loaded ONCE in
    /// `setup()` and cached for the process lifetime (mirrors `active_card`'s
    /// cache-once pattern; edit → restart). Holds the narrator + agent prose
    /// sections. Pure authored voice — NO sampler/context/token config (those
    /// live in `settings.rs`). The mechanical bracket-command list stays in
    /// `build_narrator_system_prompt` (Rust), NOT here. Always `Some` after
    /// `setup()` (the loader falls back to a built-in placeholder, never
    /// `None`).
    pub fable_prompts: Arc<std::sync::OnceLock<prompts::FablePrompts>>,
    /// The authored Wupi-assistant prompt file (`data/wupi.prompt`), loaded
    /// ONCE in `setup()` and cached for the process lifetime (mirrors
    /// `fable_prompts`). Single-section prose (role + capabilities +
    /// workflow + output discipline). The card persona (`wupi.sim`) holds
    /// identity/voice/personality; this file holds the WHAT — what she does,
    /// how she serves User. Always `Some` after `setup()` (the loader falls
    /// back to a built-in placeholder, never `None`).
    pub wupi_prompts: Arc<std::sync::OnceLock<prompts::WupiPrompts>>,
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
    /// Phase 5B (2026-07-29): the Stable Diffusion image generator. NOT
    /// eagerly spawned at boot (the SD model is heavy; load lazily on the
    /// first image-gen request). Same shape as `fable_engine` (Mutex of
    /// Option of Arc). The concrete type is `Box<dyn SceneImageGenerator>`
    /// so the backend stays swappable (the `NoopImageGenerator` stub ships
    /// before the diffusion-rs dependency lands; the real `DiffusionRs
    /// Generator` replaces it behind a cargo feature). Held under std Mutex
    /// (the trait is Sync; the generator's interior mutability is its own
    /// concern, never held across an await — the SD swap runs in a
    /// `spawn_blocking`-style detached task). `None` = no SD engine resident
    /// (the common state — SD only exists during the image-gen window).
    pub sd_engine: Arc<std::sync::Mutex<Option<Arc<Box<dyn scene_art::SceneImageGenerator>>>>>,
    /// Phase 5B (2026-07-29): the SD model path, resolved at boot via a
    /// sibling of `resolve_model_path` (looks in `models/sd/`). Stashed here
    /// (NOT consumed) so every image-gen request can re-resolve if needed +
    /// the boot path can populate it without an SD load. `None` = no SD model
    /// found on disk → image gen is disabled (the done-beat spawn no-ops).
    pub pending_sd_model_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    /// Phase 5B (2026-07-29): the one-strike failure latch. Set `true` the
    /// first time SD load/generate fails (OOM, corrupt model, missing GPU).
    /// Once set, auto-gen is disabled for the rest of the process lifetime
    /// AND the LLM is locked back into memory (never strand the game with no
    /// engine — the §2B invariant). Requires a user ack (IPC) to clear — a
    /// simple flip back to false after the user fixes the cause. `Relaxed`
    /// ordering is correct: single bit, no dependent data, the read site
    /// provides its own synchronization.
    pub sd_autogen_disabled: Arc<std::sync::atomic::AtomicBool>,
    /// One-shot manual player action (§11.30 Left-Drawer Visual HUD, 2026-08-02).
    /// Armed by the frontend `fable_player_action_set` IPC when the player
    /// performs a tactile UI action (Consume potion, Equip/Unequip from the
    /// inventory grid). `fable_send` consumes it (takes + clears, ONE turn
    /// only) at the top of its schema-lock block. Rendered into the narrator
    /// prompt as a NEW `<player_action type="manual_override">` block at the
    /// TOP of `assemble_narrator_skeleton` (leads the narrator's attention,
    /// before `<world_state>`). Visible to the narrator in its Rust-authored
    /// prompt (NOT a silent context injection — §7 honored). None = no action
    /// armed; the next `fable_send` emits no block. Held under tokio Mutex
    /// because `fable_player_action_set` (frontend) + the consume site inside
    /// `fable_send` can race; the consume path takes the lock first.
    pub pending_player_action: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Off-screen task directives emitted by the World Progression tick
    /// (Fable Phase 3 Slice 6 wiring, 2026-07-28). Each tick that resolves
    /// due tasks pushes their directives here; the next `fable_send`
    /// consumes them ALL (drains the slot) + injects them into the
    /// `<directives>` block alongside combat lethality + skill checks. Same
    /// consume-once-at-top-of-fable_send pattern as `pending_player_action`
    /// but a Vec (multiple tasks can resolve per tick). Held under tokio Mutex
    /// because the tick (writer) + the fable_send consume site (reader/drain)
    /// can race.
    pub pending_tick_directives: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Per-game cancel token, parallel to `active_cancel`. Distinct slot so
    /// chat-stop and game-stop never cross-wire (Bug #7 pattern, §2C).
    pub active_fable_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    /// Stage 3 (2026-08-11): the "discard + revert" flag for the drawer's ›
    /// interrupt. Set by `fable_interrupt_reroll` (alongside signaling the
    /// cancel token) when the user re-presses › mid-reroll to abandon the
    /// in-flight roll + start a fresh one. Distinct from the cancel token
    /// (which only halts decoding between tokens) so `fable_stop`'s
    /// "halt + keep partial" stays different from ›'s
    /// "halt + discard + revert to base + retry". Read + reset (swap) exactly
    /// once in fable_send's abort check after the reply lands.
    pub fable_abort_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Per-selective-regenerate cancel token, parallel to `active_cancel` +
    /// `active_fable_cancel`. Distinct slot so a future direct-`chat_stop`
    /// caller (or a fable_stop) can't cancel a slice regen mid-stream — same
    /// Bug #7 cross-wire lesson as `active_fable_cancel` (§2C). Currently
    /// unreachable from the UI (no stop gesture is wired for slice regen),
    /// but the symmetric slot removes the latent footgun.
    pub active_slice_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    /// Per-creation-assistant cancel token, parallel to the others. Distinct
    /// slot so `creator_assistant_stop` can abort a GLM wizard turn mid-stream
    /// without cross-wiring chat/fable/slice (same Bug #7 lesson, §2C). A
    /// creation-only API role (§3A, 2026-08-12 override) — outside the runtime
    /// game loop.
    pub active_creator_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    /// Phase 5B (2026-07-29): per-SD-generation cancel token, parallel to the
    /// others. Distinct slot so an image-gen in flight can be cancelled
    /// (user navigates away, ends the game, etc.) without cross-wiring with
    /// chat / fable / slice. Same Bug #7 lesson (§2C). The SD
    /// swap task checks this between phases (unload → load → gen → reload).
    pub active_sd_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    /// The game's scoped world-state schema (sibling to `schema`, which is
    /// Wupi-assistant's). Per-card: wiped/reloaded on card switch. Held
    /// under tokio Mutex because `fable_send` reads it + Wupi's game-manager
    /// path writes it (via `fable_command` deltas).
    pub fable_schema: Arc<tokio::sync::Mutex<schema::WorldSchema>>,
    /// Schema history ring buffer (1-click undo, 2026-07-27): the last
    /// `FABLE_HISTORY_CAP` WorldSchema snapshots, pushed BEFORE each mutation
    /// to `fable_schema` (so `pop()` restores the prior state). CLEARED on
    /// every wholesale overwrite (`fable_start`, `fable_end`, `fable_load_save`):
    /// those are session boundaries, not mutations to
    /// undo. Held under tokio Mutex: `fable_rollback` awaits while restoring.
    /// Mirrors `fable_load_save`'s precedent of bypassing the immutability
    /// lock on user-initiated restore (the §5 contract applies to LLM deltas,
    /// not user undo). See `push_fable_history` / `clear_fable_history`.
    pub fable_schema_history: Arc<tokio::sync::Mutex<std::collections::VecDeque<schema::WorldSchema>>>,
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
    /// The active saved-player id (`apps/fable/players/<id>/`). `None` when
    /// no player is attached or no game is running. Set on
    /// `enter_fable_session`, cleared on `fable_end`. Read by
    /// `fable_active_card_get` to resolve the player portrait path for the
    /// chat UI. Mirrors `active_fable_card` (short critical section, std
    /// mutex — never held across `.await`).
    pub active_player_id: Arc<std::sync::Mutex<Option<String>>>,
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
    /// JS calls `updater_apply` (which exits the process as part of the
    /// updater.exe handoff — model never loads); if
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

    /// **The local-model turn lock (2026-08-08).** Serializes the THREE
    /// local-model consumers — chat (`chat_send`), the Fable tracker
    /// (`fable_send` Stage 1), and the schema engine (`request_delta`/
    /// `request_translation`) — so at most ONE decodes on the local 12B at
    /// any instant. Whichever acquires first runs to completion; the others
    /// await at their `.lock().await`.
    ///
    /// **Why this exists alongside `context_swap`:** the swap-lock only
    /// serializes VRAM *eviction ordering* — its `LeaseGuard::drop` is a
    /// documented no-op (`context_swap.rs:268-278`) and the mutex is NOT
    /// held across a turn. The swap-lock's own module doc admits "the
    /// SERIALIZATION that matters happens at the engine level," but chat
    /// and fable are *different engines on different slots*, so those
    /// per-engine mutexes don't block each other. Without this lock, a
    /// mid-Fable Wupi-drawer chat and the Fable tracker could both reach
    /// the local 12B near-simultaneously → the swap-lock would evict one
    /// mid-turn → corruption. This lock is the actual "one local decode at
    /// a time" authority; the swap-lock is now a pure VRAM layer beneath it
    /// (with zero concurrent acquirers, eliminating the "concurrent
    /// cross-role contention" caveat at `context_swap.rs:230-231`).
    ///
    /// **The API narrator does NOT take this lock** — it touches zero
    /// local VRAM (pure HTTP), so it streams freely while a local turn runs.
    pub local_model_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            session: Arc::new(tokio::sync::Mutex::new(session::Conversation::new())),
            backend: Arc::new(std::sync::Mutex::new(None)),
            settings: Arc::new(std::sync::Mutex::new(WupiSettings::default())),
            active_cancel: Arc::new(std::sync::Mutex::new(None)),
            memory: Arc::new(std::sync::OnceLock::new()),
            prism_db: Arc::new(std::sync::OnceLock::new()),
            schema: Arc::new(tokio::sync::Mutex::new(schema::WorldSchema::default())),
            pending_delta: Arc::new(tokio::sync::Mutex::new(None)),
            failed_delta_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            failed_translation_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            failed_progression_queue: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            schema_engine: Arc::new(std::sync::Mutex::new(None)),
            active_card_id: Arc::new(std::sync::Mutex::new(
                memory::WUPI_CARD_ID.to_owned(),
            )),
            active_card: Arc::new(std::sync::OnceLock::new()),
            fable_prompts: Arc::new(std::sync::OnceLock::new()),
            wupi_prompts: Arc::new(std::sync::OnceLock::new()),
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
            sd_engine: Arc::new(std::sync::Mutex::new(None)),
            pending_sd_model_path: Arc::new(std::sync::Mutex::new(None)),
            sd_autogen_disabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pending_player_action: Arc::new(tokio::sync::Mutex::new(None)),
            pending_tick_directives: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            active_fable_cancel: Arc::new(std::sync::Mutex::new(None)),
            fable_abort_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active_slice_cancel: Arc::new(std::sync::Mutex::new(None)),
            active_creator_cancel: Arc::new(std::sync::Mutex::new(None)),
            active_sd_cancel: Arc::new(std::sync::Mutex::new(None)),
            fable_schema: Arc::new(tokio::sync::Mutex::new(schema::WorldSchema::default())),
            fable_schema_history: Arc::new(tokio::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            fable_session: Arc::new(tokio::sync::Mutex::new(session::Conversation::new())),
            active_fable_card: Arc::new(std::sync::Mutex::new(None)),
            active_player_id: Arc::new(std::sync::Mutex::new(None)),
            pre_fable_card_id: Arc::new(std::sync::Mutex::new(memory::WUPI_CARD_ID.to_owned())),
            download_progress: Arc::new(std::sync::Mutex::new(
                model_downloader::DownloadProgress::default(),
            )),
            download_cancel: Arc::new(std::sync::Mutex::new(None)),
            pending_model_path: Arc::new(std::sync::Mutex::new(None)),
            context_swap: context_swap::ContextSwap::new(),
            local_model_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("{info}\nbacktrace: {}", std::backtrace::Backtrace::force_capture());
        let _ = std::fs::write(std::env::temp_dir().join("wupi_panic.txt"), &msg);
    }));

    // §11.59 SD abort capture. Installed as early as possible (before any
    // gen_img call can fire) so a crash at any point in the SD pipeline lands
    // the assertion text in logs/sd-abort.txt. No-op + zero cost when the
    // diffusion-rs feature is off (the stub build). See scene_art::install_
    // sd_abort_callback for the full rationale (stderr buffering destroys
    // the default ggml_abort message; this callback intercepts it first).
    #[cfg(feature = "diffusion-rs")]
    scene_art::install_sd_abort_callback();

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
    // Single-instance lock: a second launch of wupi.exe focuses the existing
    // window instead of booting a duplicate (which would re-run all of
    // setup(), re-load the 9.8 GB model, and create a second tray paw). The
    // callback reuses system_menu::power_wake (show + set_focus + canvas-resume).
    //
    // RESTART OPT-OUT: power_restart spawns a fresh exe via Command::new while
    // the parent is still alive, then shuts the parent down. Under the
    // single-instance mutex the spawned child would detect the parent, signal
    // it, and exit itself → restart would silently leave nothing running. The
    // spawn sets system_menu::RESTART_SPAWN_SENTINEL=1 on the child; when that
    // env var is present we skip the plugin so the restart child boots cleanly
    // (the parent is gone within milliseconds, and the child itself acquires
    // the mutex for any subsequent double-launch). app.restart() (the updater
    // path) is Tauri-core and already handles this correctly — no opt-out
    // needed there.
    let is_restart_spawn = std::env::var(system_menu::RESTART_SPAWN_SENTINEL)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let mut builder = tauri::Builder::default();
    if !is_restart_spawn {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tracing::info!("second instance detected — focusing existing window");
            system_menu::power_wake(app);
        }));
    }
    builder
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .manage(hardware::AudioRegistry)
        .setup(|app| {
            tracing::info!("setup hook entered");

            // ── Main window: built at RUNTIME, not from tauri.conf.json. ──
            // tauri.conf.json's `app.windows` is intentionally empty. Creating
            // the single "main" window here (instead of declaring it in config)
            // is the only flicker-free way to give the fable.exe launcher a
            // different initial URL: `wupi.html#fable` for fable.exe (the
            // frontend detects `#fable` → skips the OS boot + Fable fog-gate/
            // ripple, landing on the Fable title), `wupi.html` for wupi.exe.
            // Config windows can't be cleanly swapped at runtime (closing
            // "main" trips the last-window→shutdown guard; a second window
            // double-fires boot_load_model), so both binaries create the window
            // here with a 1:1 port of the former config attrs. Label stays
            // "main" so capabilities/default.json (`windows: ["main"]`) +
            // system_menu::power_wake keep working.
            let entry_url = if is_fable_entry() {
                // Direct launch (fable.exe --card ...): append `?direct=1`
                // BEFORE the fragment so `location.hash` stays `#fable` (the
                // FABLE_ENTRY detection keys on the exact hash) and the
                // frontend reads `direct=1` from `location.search`.
                if launch_context().is_some() {
                    "wupi.html?direct=1#fable"
                } else {
                    "wupi.html#fable"
                }
            } else {
                "wupi.html"
            };
            let fable_entry = is_fable_entry();
            // fable.exe: build the window HIDDEN. The WebView2 native surface
            // flashes a system-tinted/purple cast for the first frame(s) before
            // any HTML renders, and no HTML/CSS can beat that (it's pre-paint).
            // A hidden window shows nothing while the frontend paints its F-logo
            // entry splash; the frontend then calls `fable_reveal_window` once
            // the splash is rendered → the first VISIBLE frame is already the
            // splash, never the native surface. wupi.exe stays visible (its boot
            // paw animation must be seen immediately).
            let main_window =
                tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::App(entry_url.into()))
                    .title("WUPI")
                    .inner_size(1440.0, 900.0)
                    .resizable(false)
                    .fullscreen(true)
                    .decorations(false)
                    .transparent(true)
                    .visible(!fable_entry)
                    .build()?;
            // fable.exe: set the running window's taskbar icon to the F too. The
            // exe FILE icon is already the F (via the build.rs resource, ID 1 <
            // tauri-build's 32512); without this the live window could fall back
            // to the paw. Best-effort + non-fatal — a decode or set_icon failure
            // just leaves the default icon (the window always builds above).
            // include_bytes! resolves relative to this file (src/), so ../icons.
            if is_fable_entry() {
                if let Ok(img) = tauri::image::Image::from_bytes(include_bytes!("../icons/fable.png")) {
                    let _ = main_window.set_icon(img);
                }
                // Safety net: fable.exe starts with the window hidden (above) so
                // the WebView2 init flash never shows. The frontend calls
                // fable_reveal_window once its splash is painted. If the bundle
                // ever fails to load/parse (catastrophic), that call never fires
                // → the window would strand hidden. This fallback shows it after
                // 5s no matter what, so fable.exe never leaves the user staring
                // at nothing. show() is a no-op if the frontend already revealed.
                let fallback_win = main_window.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    let _ = fallback_win.show();
                });
            }
            // (The `*.old` remnant sweep lived here — RETIRED with the rename-
            // dance updater. The temp-staged updater.exe overwrites files
            // directly after wupi.exe exits; no `.old` files are ever produced.
            // See updater.rs + crates/updater.)
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
            // Per-card roleplay state (§6B): each card owns a folder under
            // apps/fable/cards/<id>/ holding its .sim + session/world/player/
            // npc JSON + saves/ — created per-card at write time, not eagerly
            // here. (The eager sessions/schemas/saves/profiles roots are
            // DEAD — deleted 2026-08-14; the updater's §8C purge removes
            // them from upgraded installs.)
            std::fs::create_dir_all(fable_dir.join("cards")).ok();
            // The zip ships NO apps/ tree at all (empty dirs = pure zip
            // noise; 2026-08-14) — THIS block is what materializes apps/ on
            // a fresh install's first boot. The Background Library lives at
            // apps/fable/images/backgrounds (§7 "Stage Background Library"),
            // created eagerly so the first import never races a missing dir
            // (the old apps/fable/backgrounds scene-art dir was dead —
            // nothing ever resolved it — and is NOT created).
            std::fs::create_dir_all(fable_dir.join("images").join("backgrounds")).ok();
            // Saved Players (2026-08-02): a standalone, reusable player
            // identity library at apps/fable/players/<id>/. Sibling root to
            // cards/ — holds authored players that can be attached onto any
            // sim card at game start. Created eagerly so the first
            // list/write never races a missing dir.
            std::fs::create_dir_all(fable_dir.join("players")).ok();
            // Prism (2026-07-31): the image-generation app's per-app state root
            // (sibling of apps/fable/). `gallery/` holds the generated PNGs
            // (timestamp-seed-named); the gallery.sqlite DB lives directly
            // under apps/prism/. Created eagerly so the first Prism generation
            // + the boot DB-open never race a missing dir.
            let prism_dir = resolve_apps_dir(app.handle()).join("prism");
            std::fs::create_dir_all(prism_dir.join("gallery")).ok();
            // (The v0.2.4→v0.3.0 + games→fable boot migrations were DELETED
            // 2026-08-14: every install on the supported 0.18→0.19 path ran
            // them long ago, their sessions/schemas destinations are dead
            // folders (§6B per-card layout replaced them), and the updater's
            // §8C purge now reaps the leftovers. See crates/updater/src/purge.rs.)
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

            // Load the Fable prompt file (`data/fable.prompt`) once + cache it
            // for the process lifetime (mirrors `active_card`'s cache-once
            // pattern; edit → restart). Holds the authored narrator + agent
            // prose. `load_fable_prompts` degrades to a built-in placeholder
            // on any error (missing/malformed), so a missing file never blocks
            // boot. Pure authored voice — no token/sampler config lives here.
            let fable_prompts = match resolve_fable_prompt_path(app.handle()) {
                Some(path) => {
                    tracing::info!("resolved fable.prompt: {}", path.display());
                    prompts::load_fable_prompts(&path)
                }
                None => {
                    tracing::warn!(
                        "no data/fable.prompt path resolved; using built-in fallback prompts"
                    );
                    prompts::load_fable_prompts(std::path::Path::new("fable.prompt"))
                }
            };
            let _ = state.fable_prompts.set(fable_prompts);

            // Load the Wupi-assistant prompt file (`data/wupi.prompt`) once +
            // cache it (mirrors `fable_prompts` above). Single-section prose
            // (role + capabilities + workflow); `load_wupi_prompts` degrades
            // to a built-in placeholder on any error, so a missing file never
            // blocks boot. The card persona (`wupi.sim`) holds identity/
            // voice; this file holds the WHAT she does.
            let wupi_prompts = match resolve_wupi_prompt_path(app.handle()) {
                Some(path) => {
                    tracing::info!("resolved wupi.prompt: {}", path.display());
                    prompts::load_wupi_prompts(&path)
                }
                None => {
                    tracing::warn!(
                        "no data/wupi.prompt path resolved; using built-in fallback prompt"
                    );
                    prompts::load_wupi_prompts(std::path::Path::new("wupi.prompt"))
                }
            };
            let _ = state.wupi_prompts.set(wupi_prompts);

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
            // found, JS calls `updater_apply` (which exits the process via the
            // updater.exe handoff) instead and the model never loads (no wasted
            // CPU/VRAM on a doomed process).
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

            // Phase 5B: resolve the SD checkpoint (best-effort, no emit on
            // absence — image gen is opt-in via the PRISM app). Stashed the
            // same way as the chat model; the PRISM swap early-outs on `None`.
            // A fresh install never has this; the user drops a checkpoint into
            // `models/sd/` to enable PRISM image generation.
            let sd_path = resolve_sd_model_path(app.handle());
            match &sd_path {
                Some(p) => tracing::info!(path = %p.display(), "SD model resolved (image gen available once enabled)"),
                None => tracing::info!("no SD model found; image gen stays dormant"),
            }
            *state
                .pending_sd_model_path
                .lock()
                .expect("pending_sd_model_path mutex") = sd_path;

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

                    // ── Wupi playbook codex seed (RESTORED 2026-08-01).
                    // Seed `data/wupi.codex` (Wupi's static authoring playbook:
                    // the .sim card format, the codex-entry format, the SOFT/HARD
                    // game-mechanic distinction) into the `__wupi_system__`
                    // partition. The READ side (`search_wupi_visible`) was never
                    // removed — it queries this partition on every chat turn and
                    // surfaces hits under the codex frame of `<retrieved_memory>`;
                    // it just found nothing because the seeder was deleted in
                    // commit f47a82e. This call fills that hole.
                    //
                    // Idempotent (hash-based): an unchanged file does zero
                    // writes — on a normal boot the reconcile is N hash
                    // comparisons. Best-effort: a failed seed is logged-and-
                    // dropped, never fatal (same contract as the embedder
                    // fallback). `tauri::async_runtime::block_on` is the proven
                    // async-bridge out of the sync setup closure (it uses
                    // Tauri's managed runtime, valid here — bare `tokio::spawn`
                    // would panic: no reactor entered on the event-loop thread).
                    //
                    // Out of scope (still dormant, by design): the user-codex
                    // `data/docs/` → `__codex__` partition (no seeder; the
                    // `__codex__` read-side in `search_fable_visible` is reserved
                    // scaffolding for a future user-codex feature) and the Fable
                    // playbook `data/fable.codex` → `__fable_system__` (sibling
                    // seeder not yet restored).
                    let wupi_codex_path = resolve_data_dir(app.handle()).join("wupi.codex");
                    if let Some(engine) = state.memory.get() {
                        match tauri::async_runtime::block_on(codex::seed_wupi_codex(
                            engine,
                            &wupi_codex_path,
                            memory::WUPI_SYSTEM_CARD_ID,
                        )) {
                            Ok(report) => tracing::info!(
                                seeded = report.seeded,
                                updated = report.updated,
                                purged = report.purged,
                                unchanged = report.unchanged,
                                "wupi playbook codex seeded into __wupi_system__"
                            ),
                            Err(e) => tracing::warn!(
                                error = %format!("{e:#}"),
                                "wupi playbook seed failed; continuing without playbook"
                            ),
                        }
                    }
                }
                Err(e) => {
                    // DB open failure is fatal for memory but must not kill
                    // the app. Leave the OnceLock empty; callers check `get`.
                    tracing::error!(error = %format!("{e:#}"), "memory engine init failed");
                }
            }

            // ── Prism gallery DB (2026-07-31): open the image-gallery SQLite
            // at `apps/prism/gallery.sqlite`. Best-effort — a failure here
            // leaves the OnceLock empty + the Prism gallery screens degrade to
            // "empty + read-only" (never fatal, same contract as memory). The
            // `apps/prism/gallery/` dir was created in the boot dir-creation
            // block above, so the parent exists.
            {
                let prism_db_path = resolve_apps_dir(app.handle()).join("prism").join("gallery.sqlite");
                match prism::GalleryDb::open(&prism_db_path) {
                    Ok(db) => {
                        let _ = state.prism_db.set(Arc::new(db));
                        tracing::info!(db = %prism_db_path.display(), "prism gallery db initialized");
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "prism gallery db init failed; gallery will be read-only/empty");
                    }
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
                    // Last window closing = user is done. Route through the SAME
                    // shutdown path as the tray "Quit" / paw "Shutdown" buttons:
                    // destroy_tray + app.exit(0) (graceful). Keeping the event
                    // loop alive during teardown lets the Windows shell reconcile
                    // the tray's NIM_DELETE before the process goes away — the
                    // old `std::process::exit` hard-kill starved that handshake
                    // and left a "ghost" paw cached in the hidden-icons popover
                    // until the user hovered it. See system_menu::power_shutdown
                    // for the full history.
                    system_menu::power_shutdown(&window.app_handle());
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
            fable_reveal_window,
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
            operator_profile_get,
            operator_profile_set,
            api_profiles_list,
            api_profile_save,
            api_profile_delete,
            api_profile_test,
            api_connect,
            api_disconnect,
            model_source_get,
            // fable.exe --card <slug> [--save <id>] direct-launch context.
            get_launch_context,
            check_models,
            download_models,
            get_download_progress,
            cancel_download,
            fable_cards_list,
            fable_start,
            fable_send,
            fable_stop,
            fable_interrupt_reroll,
            fable_end,
            fable_list_saves,
            fable_save_now,
            fable_load_save,
            fable_delete_save,
            // UX chat controls (edit / reroll / rewind-and-edit). Pure session
            // mutators; the schema ring buffer is owned by fable_rollback
            // above, driven by each command's `schema_pop_count` return.
            edit_message,
            delete_message,
            reroll_last_turn,
            rewind_and_edit_user,
            swipe_variant,
            // Golden-pencil slice regenerate (2026-08-11): highlight a span of
            // an assistant message → the API rewrites only that span in place.
            // API-only, no tracker, no schema mutation, in-place. Streams
            // chunk/slice_done; fable_slice_stop cancels mid-stream.
            fable_regenerate_slice,
            fable_slice_stop,
            fable_continue_target,
            // GLM-driven creation assistant (2026-08-12): a creation-only API
            // role (§3A override). Conversational player/sim/codex authoring
            // outside the runtime game loop. Streams chunk/done.
            creator_assistant_turn,
            creator_assistant_stop,
            creator_log,
            creator_read_import_text,
            fable_card_set_intro,
            // §11.30 Left-Drawer HUD — manual player-action injection.
            fable_player_action_set,
            // Prism (image-generation app) IPCs (2026-07-31). prism_generate
            // drives the shared SD swap core with user params (seed/cfg/
            // sampler); the gallery_* IPCs are the Glass Vault CRUD.
            prism_sd_status,
            prism_sd_clear_latch,
            prism_generate,
            prism_gallery_list,
            prism_gallery_get,
            prism_gallery_favorite,
            prism_gallery_trash,
            prism_gallery_restore,
            prism_gallery_purge,
            player_state_get,
            fable_active_card_get,
            fable_schema_get,
            fable_schema_set,
            fable_card_get,
            fable_card_raw_get,
            fable_card_raw_set,
            fable_json_raw_get,
            fable_json_raw_set,
            fable_codex_get,
            fable_codex_get_by_id,
            fable_codex_raw_set,
            // New-creator sibling writes (explicit card_id; CREATE-time).
            fable_card_sibling_write,
            fable_card_portrait_write,
            fable_card_portrait_url,
            fable_card_raw_get_by_id,
            fable_card_raw_set_by_id,
            fable_card_delete,
            // Windows .lnk shortcut generator (shortcut.rs) — fable.exe --card.
            create_card_shortcut,
            fable_validate_card_xml,
            fable_write_card,
            // Saved Players (2026-08-02): standalone, reusable player
            // identity library at apps/fable/players/. list/get for the
            // picker; write/delete for the creator + management; portrait
            // _upload copies a dialog-picked image into the player folder.
            fable_players_list,
            fable_player_get,
            fable_active_player_get,
            fable_player_write,
            fable_player_delete,
            fable_player_portrait_upload,
            fable_player_portrait_upload_bytes,
            fable_player_portrait_read_bytes,
            // Fable Background Library (2026-08-11): a user-importable image
            // library at apps/fable/images/backgrounds/ (global) + a per-card,
            // save-persistent selection on WorldSchema.background. The 4th WUPI-
            // drawer foot button ("Background") drives these — list the library,
            // import cropped bytes, delete, + get/set the per-card selection.
            fable_backgrounds_list,
            fable_background_import_bytes,
            fable_background_delete,
            fable_background_active_get,
            fable_background_active_set,
            fable_rollback,
            fable_history_depth,
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
            hardware::ethernet::ethernet_get_state,
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
            updater_consume_result,
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

/// fable.exe entry: reveal the (hidden) main window. The main window is built
/// with `.visible(false)` for the fable launcher (lib.rs setup) to hide the
/// WebView2 native-surface flash during init — the frontend paints its F-logo
/// entry splash offscreen, then calls this once the splash is rendered, so the
/// first VISIBLE frame is already the splash (never the native surface).
/// `show()` is idempotent + safe to call for wupi.exe too (a no-op there since
/// the window is already visible + this IPC is only invoked on the fable path).
#[tauri::command]
fn fable_reveal_window<R: tauri::Runtime>(app: tauri::AppHandle<R>) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("main") {
        win.show().map_err(|e| e.to_string())?;
        let _ = win.set_focus();
    }
    Ok(())
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

/// `<exe_dir>/apps` — per-app user-state root. `apps/fable/` (cards/
/// players/ images/ + per-card state) and `apps/prism/` (the image-gen
/// gallery) live here. All preserved across updates.
fn resolve_apps_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_install_root(app).join("apps")
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

/// Phase 5B (2026-07-29): resolve the Stable Diffusion checkpoint. Sibling
/// of `resolve_model_path`, but searches each candidate dir's `sd/` subdir
/// (so `models/`, `src-tauri/models/`, etc. each carry an optional `sd/`
/// sibling). SD checkpoints are `.safetensors` (canonical) or `.gguf`; pick
/// the largest when several are present. Returns `None` when absent — the SD
/// swap core (`run_sd_swap_core`, driven by PRISM) early-outs cleanly on `None`
/// ("no SD model resolved on disk"), so a fresh install with no SD model
/// just silently skips image gen. The dependency add (diffusion-rs) is a
/// separate build-safety gate owned by Chloe (§0); this resolver only walks
/// the disk, so it needs no Cargo change.
fn resolve_sd_model_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    for dir in model_search_dirs(app) {
        let sd_dir = dir.join("sd");
        if sd_dir.exists() {
            if let Some(picked) = pick_sd_checkpoint(&sd_dir) {
                tracing::info!(
                    path = %picked.display(),
                    from = %sd_dir.display(),
                    "resolved SD model",
                );
                return Some(picked);
            }
        }
    }
    None
}

/// Pick a single SD checkpoint from a directory: `.safetensors` or `.gguf`,
/// largest first. SD checkpoints don't have a locked naming convention (unlike
/// the chat model's `WUPI.gguf`), so the size fallback is the primary selector.
/// The SD swap is opt-in (gated by the model path + the one-strike latch), so a
/// stray non-SD `.gguf` in `sd/` would just produce a load error that trips the
/// one-strike latch — no correctness risk to the narrator path.
fn pick_sd_checkpoint(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            matches!(
                e.path().extension().and_then(|x| x.to_str()).map(|x| x.to_lowercase()),
                Some(ref ext) if ext == "safetensors" || ext == "gguf"
            )
        })
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

/// Read + clear the updater's result marker (`data/_update_result.json`) so
/// the UI can surface "Updated to vX.Y.Z" (or the error) exactly once after an
/// update-driven relaunch. Returns `None` on a normal boot (no marker present).
/// The apply path (`updater_apply`) never returns to the frontend on success —
/// wupi.exe exits as part of the handoff — so the relaunched process reads the
/// outcome here instead. Mirrors the old `updater_restart` slot in the handler.
#[tauri::command]
fn updater_consume_result(app: tauri::AppHandle) -> Option<updater::UpdateResult> {
    let exe_dir = resolve_install_root(&app);
    updater::read_and_clear_result(&exe_dir)
}

/// Deferred chat-model spawn. The boot UX (script.js) calls this AFTER the
/// boot update-check gate resolves with "up-to-date" — so an update-found
/// path that calls `updater_apply` (which exits the process via the
/// updater.exe handoff) skips this entirely
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
    // If setup() couldn't resolve a GGUF at boot (fresh install), re-scan
    // disk now. The post-first-download reload path (script.js's LAUNCH
    // button → location.reload) reloads the WEB PAGE only — the Rust
    // process is the same one that started with no GGUF on disk, so
    // pending_model_path is still None. Without this re-scan, that reload
    // emits model-status:missing (red flash at the top) even though
    // WUPI.gguf is now physically present, forcing the user to restart a
    // SECOND time (a fresh process re-runs setup → resolves the path).
    // Reuses setup()'s resolver (DRY — single source of truth for the
    // search dirs + pick_main_model logic). Still None ⇒ genuinely missing.
    let path = match path {
        Some(p) => Some(p),
        None => {
            tracing::info!(
                "boot_load_model: pending_model_path was None — re-scanning disk (post-download reload path)"
            );
            resolve_model_path(&app)
        }
    };
    let Some(path) = path else {
        // Genuinely missing: no GGUF found even after re-scan. Surface so
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
    // Wupi chat is LOCAL-ONLY (2026-08-08 override): the chat backend ALWAYS
    // spawns at 2048 (CTX_LOCAL_WITH_API). The 4096 context is retired for
    // chat — it was for narrative, which the local model no longer does.
    // model_source no longer drives the boot context size; always 2048.
    let context_size = settings::CTX_LOCAL_WITH_API;
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

                // Register the boot-spawned chat context with the VRAM
                // swap-lock so a Fable/schema `acquire` that fires BEFORE
                // any `chat_send` can EVICT it (2026-08-10 freeze fix). The
                // chat backend spawns eagerly at boot for fast first-message
                // latency but does NOT take the lease (chat_send takes it
                // lazily). Without this registration, the lease's default
                // "idle" state (teardown = None) makes the first fable/schema
                // acquire skip eviction → chat + fable co-resident → VRAM
                // exhaust → CPU fallback → multi-minute PC freeze. The
                // teardown closure mirrors chat_send's (take backend from
                // slot + shutdown, freeing KV; weights stay leaked). The
                // registration is idempotent (a chat_send that ran first
                // already registered it → no-op). This is the chat-side
                // analog of the eager-schema-spawn bypass fix at lib.rs:1917.
                {
                    let context_swap = app_state.context_swap.clone();
                    let backend_slot = Arc::clone(&app_state.backend);
                    tauri::async_runtime::spawn(async move {
                        context_swap
                            .register_resident(
                                context_swap::ContextRole::Chat,
                                Box::new(move || {
                                    let backend = {
                                        let mut g = backend_slot
                                            .lock()
                                            .map_err(|e| e.to_string())?;
                                        g.take()
                                    };
                                    if let Some(backend) = backend {
                                        backend.shutdown();
                                        tracing::info!(
                                            "context-swap: boot chat backend torn down \
                                             (KV freed, weights retained)"
                                        );
                                    }
                                    Ok(())
                                }),
                            )
                            .await;
                    });
                }

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

/// Resolve the Fable prompt file path (`data/fable.prompt`). Brand-new file
/// (no pre-rename legacy), so this is lean: just the portable `<exe_dir>/
/// data/fable.prompt` + its dev-repo mirror (`<repo>/data/fable.prompt`, for
/// `cargo run` where exe lives under `target/`). Returns `None` when no file
/// is found; the caller (`setup`) then loads via `prompts::load_fable_prompts`
/// with a placeholder path, which degrades to the built-in fallback prompts.
fn resolve_fable_prompt_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // §8C portable layout: `<exe_dir>/data/fable.prompt`.
    candidates.push(resolve_data_dir(app).join("fable.prompt"));
    if let Some(d) = app.path().resource_dir().ok() {
        candidates.push(d.join("data").join("fable.prompt"));
    }
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            // Dev-repo: exe under target/{release,debug}; repo root is 3 up.
            if let Some(repo) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(repo.join("data").join("fable.prompt"));
            }
        }
    }

    for c in &candidates {
        if c.is_file() {
            tracing::info!("resolved fable.prompt: {}", c.display());
            return Some(c.clone());
        }
    }
    None
}

/// Resolve the Wupi-assistant prompt file path (`data/wupi.prompt`). Mirrors
/// [`resolve_fable_prompt_path`]: the portable `<exe_dir>/data/wupi.prompt` +
/// its dev-repo mirror (`<repo>/data/wupi.prompt`, for `cargo run` where exe
/// lives under `target/`). Returns `None` when no file is found; the caller
/// (`setup`) then loads via `prompts::load_wupi_prompts` with a placeholder
/// path, which degrades to the built-in fallback prompt.
fn resolve_wupi_prompt_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    use tauri::Manager;

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    // §8C portable layout: `<exe_dir>/data/wupi.prompt`.
    candidates.push(resolve_data_dir(app).join("wupi.prompt"));
    if let Some(d) = app.path().resource_dir().ok() {
        candidates.push(d.join("data").join("wupi.prompt"));
    }
    if let Some(exe) = std::env::current_exe().ok() {
        if let Some(parent) = exe.parent() {
            // Dev-repo: exe under target/{release,debug}; repo root is 3 up.
            if let Some(repo) = parent.parent().and_then(|g| g.parent()) {
                candidates.push(repo.join("data").join("wupi.prompt"));
            }
        }
    }

    for c in &candidates {
        if c.is_file() {
            tracing::info!("resolved wupi.prompt: {}", c.display());
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
) -> Result<(Arc<schema_engine::SchemaEngine>, context_swap::LeaseGuard, tokio::sync::OwnedMutexGuard<()>), String> {
    acquire_schema_engine_from_arcs(
        state.context_swap.clone(),
        Arc::clone(&state.schema_engine),
        Arc::clone(&state.local_model_lock),
    )
    .await
}

/// The Arcs-only inner of `acquire_schema_engine`. Exists so a detached
/// `tokio::spawn` task (the chat_send delta-fire path) can acquire the schema
/// engine without a `tauri::State<'_, AppState>` borrow — it owns its Arcs.
///
/// **Local-model turn lock (2026-08-08):** acquires the process-wide
/// `local_model_lock` and returns the guard as the third tuple element. The
/// caller holds it across the `request_delta`/`request_translation` decode so
/// the schema pass serializes against chat + the Fable tracker — at most ONE
/// local decode at any instant. Dropping the returned guard (end of caller
/// scope) releases the lock.
async fn acquire_schema_engine_from_arcs(
    context_swap: context_swap::ContextSwap,
    schema_engine_slot: Arc<std::sync::Mutex<Option<Arc<schema_engine::SchemaEngine>>>>,
    local_model_lock: Arc<tokio::sync::Mutex<()>>,
) -> Result<(Arc<schema_engine::SchemaEngine>, context_swap::LeaseGuard, tokio::sync::OwnedMutexGuard<()>), String> {
    // Acquire the local-model turn lock FIRST (before the VRAM lease): the
    // lock serializes the decode; the lease handles eviction beneath it.
    // lock_owned() returns an OwnedMutexGuard (no lifetime tie to this fn).
    let model_guard = local_model_lock.lock_owned().await;

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
    Ok((engine, lease, model_guard))
}

/// The SHARED core of the SD swap cycle (used by PRISM; the FABLE scene-art
/// wrapper was removed 2026-07-31 when SD was unhooked from Fable). PRISM drives
/// this core directly with a user-built request. Steps (unchanged from §11.58):
///
/// 1. **Gate** on the one-strike latch (`sd_autogen_disabled`) + the SD model
///    path. If disabled or no model, no-op (never strand the game).
/// 2. **Acquire the `ContextRole::Sd` lease** — the swap-lock evicts whatever
///    LLM context is resident. Held for the whole cycle.
/// 3. **`unload_shared_model()`** — frees the LLM weights (~9.8GB).
/// 4. **Load SD** — populate `sd_engine_slot` + `load(model_path)`.
/// 5. **Generate** — run the caller's fully-built `SceneImageRequest` (FABLE
///    passes a prompt+dest+defaults request; Prism passes a request built by
///    `prism::build_request` carrying seed/cfg/sampler). Writes the PNG to
///    `request.dest`.
/// 6. **Cleanup (always runs)** — reload the LLM weights, drop the lease (SD
///    teardown frees SD VRAM). The next fable_send/chat_send/prism_generate
///    re-spawns a fresh LlamaContext on the reloaded weights.
///
/// Taking a full `SceneImageRequest` (not prompt+dest) is the generalization:
/// FABLE composes from world state (the wrapper above); Prism passes a user-
/// authored request with seed/CFG/sampler. Both share steps 1-4 + 6 verbatim.
async fn run_sd_swap_core(
    context_swap: context_swap::ContextSwap,
    sd_engine_slot: Arc<std::sync::Mutex<Option<Arc<Box<dyn scene_art::SceneImageGenerator>>>>>,
    sd_model_path: Option<std::path::PathBuf>,
    sd_autogen_disabled: Arc<std::sync::atomic::AtomicBool>,
    active_sd_cancel: Arc<std::sync::Mutex<Option<llm::CancelToken>>>,
    llm_model_path: Arc<std::sync::Mutex<Option<std::path::PathBuf>>>,
    request: scene_art::SceneImageRequest,
    on_result: Box<dyn FnOnce(scene_art::SwapOutcome) + Send>,
) {
    use std::sync::atomic::Ordering;

    // 1. Gate: one-strike latch + no-SD-model early-out.
    if sd_autogen_disabled.load(Ordering::Relaxed) {
        tracing::debug!("sd-swap: skipped (auto-gen disabled by one-strike latch)");
        on_result(scene_art::SwapOutcome::Skipped);
        return;
    }
    let sd_model_path = match sd_model_path {
        Some(p) => p,
        None => {
            tracing::debug!("sd-swap: skipped (no SD model resolved on disk)");
            on_result(scene_art::SwapOutcome::Skipped);
            return;
        }
    };

    // 2. Acquire the Sd lease. The swap-lock evicts the resident LLM context
    //    (joining its thread). The teardown here is a no-op for the SD side —
    //    SD doesn't have a resident engine to evict on the FIRST acquire; the
    //    reload-unload dance is what frees VRAM. The lease is held for the
    //    whole cycle so no other role can grab VRAM mid-generation.
    let sd_teardown_slot = Arc::clone(&sd_engine_slot);
    let lease = context_swap
        .acquire(
            context_swap::ContextRole::Sd,
            Box::new(move || {
                // Synchronous teardown: take + drop the SD engine so its VRAM
                // is freed before the LLM reloads. unload() is the backend's
                // contract for synchronous VRAM release.
                let gen = {
                    let mut g = sd_teardown_slot.lock().map_err(|e| e.to_string())?;
                    g.take()
                };
                if let Some(gen) = gen {
                    gen.unload();
                    tracing::info!("context-swap: SD engine unloaded (VRAM freed for LLM reload)");
                }
                Ok(())
            }),
        )
        .await;

    // Helper: check the cancel token. Returns true if cancelled.
    let is_cancelled = || {
        active_sd_cancel
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|t| t.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    };
    if is_cancelled() {
        drop(lease);
        on_result(scene_art::SwapOutcome::Cancelled);
        return;
    }

    // 3-6. The blocking core: unload LLM → load SD → gen → unload SD → reload
    //      LLM. Runs in spawn_blocking so the CUDA calls don't stall the async
    //      runtime. The lease is moved in + moved out (held for the duration).
    let llm_model_path_clone = Arc::clone(&llm_model_path);
    // Clone the latch Arc for the closure (it's also used after the closure
    // returns, in the post-swap error paths — Arc isn't Copy, so clone first).
    let latch_in_closure = Arc::clone(&sd_autogen_disabled);
    let outcome = tokio::task::spawn_blocking(move || {
        // 3. Unload the LLM weights. The lease's teardown already joined every
        //    LLM context, so the precondition (no LlamaContext alive) holds.
        let unloaded = llm::unload_shared_model();
        if !unloaded {
            tracing::warn!("sd-swap: unload_shared_model reported nothing resident (unexpected — proceeding anyway)");
        }

        // 4. Load SD. Populate the slot + call load(model_path).
        let backend: Arc<Box<dyn scene_art::SceneImageGenerator>> =
            Arc::new(scene_art::default_sd_backend());
        {
            let mut g = sd_engine_slot.lock().expect("sd_engine slot mutex");
            *g = Some(Arc::clone(&backend));
        }
        if let Err(e) = backend.load(&sd_model_path) {
            tracing::error!(error = %e, "sd-swap: SD load failed — tripping one-strike latch");
            latch_in_closure.store(true, Ordering::Relaxed);
            // Jump to cleanup (the LLM must reload so the game continues).
            return scene_art::SwapOutcome::Failed(e);
        }

        // 5. Generate from the caller's fully-built request. FABLE scene-art
        //    + Prism both arrive here with their request shape already set
        //    (prompt/dest/seed/cfg/sampler); core just runs it.
        match backend.generate(&request) {
            Ok(result) => {
                tracing::info!(dest = %result.dest.display(), elapsed_ms = result.elapsed_ms, "sd-swap: image generated");
                scene_art::SwapOutcome::Generated(result)
            }
            Err(e) => {
                tracing::error!(error = %e, "sd-swap: SD generate failed — tripping one-strike latch");
                latch_in_closure.store(true, Ordering::Relaxed);
                scene_art::SwapOutcome::Failed(e)
            }
        }
        // The `backend` Arc drops here, but the slot still holds a clone —
        // the lease's teardown (registered above) unloads + clears it when the
        // lease drops. The SD VRAM is freed by that teardown, not here.
    })
    .await;

    // The spawn_blocking returned (either the outcome or a JoinError). The
    // lease drops here → the SD teardown fires (unloading SD), but the LLM
    // weights are STILL unloaded (step 3 freed them). We must reload the LLM
    // BEFORE the lease drops so the next fable_send/chat_send finds a live
    // model — otherwise the game is stranded with no narrator.
    //
    // Order: reload the LLM weights FIRST (so shared_model() is live again),
    // THEN drop the lease (the SD teardown frees SD VRAM, which the LLM
    // reload didn't need — they're sequential, not concurrent).
    let llm_path = llm_model_path_clone
        .lock()
        .ok()
        .and_then(|g| g.clone());
    if let Some(path) = llm_path {
        // reload_shared_model re-leaks the weights. ~5-10s file-read (the only
        // reverse-swap cost; hidden behind the user reading the new image).
        if let Err(e) = llm::reload_shared_model(&path, 99) {
            tracing::error!(error = %e, "sd-swap: LLM RELOAD FAILED — the game has no narrator until reboot. Tripping one-strike latch.");
            sd_autogen_disabled.store(true, Ordering::Relaxed);
        }
    } else {
        tracing::error!("sd-swap: no LLM model path to reload — the game has no narrator until reboot. Tripping one-strike latch.");
        sd_autogen_disabled.store(true, Ordering::Relaxed);
    }
    drop(lease); // SD teardown fires here (frees SD VRAM).

    match outcome {
        Ok(inner) => on_result(inner),
        Err(e) => {
            tracing::error!(error = %e, "sd-swap: spawn_blocking panicked");
            on_result(scene_art::SwapOutcome::Failed(scene_art::SceneImageError {
                message: format!("SD task panicked: {e}"),
            }));
        }
    }
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
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
) -> Result<(), String> {
    // 1. Acquire the Schema lease + spawn-or-reuse the schema engine under
    //    the VRAM swap-lock (v0.6.4). This evicts any resident chat/fable
    //    context BEFORE we generate — the load-bearing fix for the 2026-07-26
    //    freeze. The lease guard is held for the duration of the translation;
    //    dropping it at end of scope marks the slot free (the resident schema
    //    engine persists until a chat/fable turn evicts it).
    let (schema_engine, _schema_lease, _model_guard) = acquire_schema_engine(state).await?;

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
    let Some(mut delta) = reply.delta else {
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
        push_fable_history(&state).await;
        let mut s = state.fable_schema.lock().await;
        // Phase 3 Slice 5 wiring (2026-07-28): silently strip + canonically
        // apply any rel.<npc_id> writes BEFORE apply_delta. The LLM can't
        // directly write a gated tier (architect directive); only Stranger→
        // Acquaintance is allowed. Stripped keys logged for playtest verify.
        let stripped = strip_invalid_relationship_writes(&mut delta, &mut s);
        if !stripped.is_empty() {
            tracing::info!(
                count = stripped.len(),
                keys = ?stripped,
                "[rel] relationship writes stripped from fable-manager translation delta"
            );
        }
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

    // 6. Auto-save (best-effort, fire-and-forget) when the delta actually
    //    mutated state. Mirrors the `fable_send` autosave block (lib.rs ~5423).
    //    **2026-07-27 Bug #3 fix:** without this, a manager mutation (e.g.
    //    "give me 50 gold") updated `fable_schema` in memory but never hit
    //    disk until the NEXT narrator turn's autosave — a crash between the
    //    two lost the mutation. Now both manager + narrator paths persist
    //    atomically. Skipped on empty deltas (nothing changed → nothing to
    //    save → no disk churn). The save runs detached on the blocking pool;
    //    errors are logged-and-dropped (autosave is best-effort by contract).
    if delta_applied {
        let app_clone = app.clone();
        let state_active_card = state.active_fable_card.clone();
        let state_session = state.fable_session.clone();
        let state_schema = state.fable_schema.clone();
        tokio::spawn(async move {
            let card_opt = state_active_card
                .lock()
                .expect("active_fable_card mutex")
                .clone();
            let Some(card) = card_opt else {
                return;
            };
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
                Ok(Ok(_)) => tracing::debug!("game-manager autosave: ok"),
                Ok(Err(e)) => {
                    tracing::warn!(error = %format!("{e}"), "game-manager autosave write failed")
                }
                Err(e) => tracing::warn!(error = %format!("{e}"), "game-manager autosave join failed"),
            }
        });
    }
    Ok(())
}

/// Render a tight natural-language summary of the typed inventory for the
/// `QueryWorldState("inventory")` path (2026-08-07). Lists worn equipment
/// (both layers — Wupi sees everything, unlike the narrator's Outer-only
/// filter), the belt rack, + the pack with a `carried / capacity lb` text
/// readout. NOTE: the encumbrance system was PERMANENTLY REMOVED 2026-08-09 —
/// the `X / capacity lb` text here is the ONLY place the retired capacity
/// field still surfaces (no fill bar, no enforcement anywhere). Empty sections
/// are omitted. Pure; reads from a schema snapshot.
fn render_inventory_summary(schema: &schema::WorldSchema) -> String {
    use crate::equipment;
    let ps = &schema.player_state;
    let mut lines: Vec<String> = Vec::new();

    // Equipment — iterate slots in canonical order, show both layers.
    if !ps.equipment.is_empty() {
        let mut worn: Vec<String> = Vec::new();
        for slot in equipment::EquipSlot::all() {
            if let Some(layers) = ps.equipment.get(slot) {
                let outer = layers
                    .outer
                    .as_ref()
                    .map(|i| i.stats.as_ref().map_or(i.name.clone(), |s| format!("{} ({})", i.name, s)));
                let inner = layers
                    .inner
                    .as_ref()
                    .map(|i| i.stats.as_ref().map_or(i.name.clone(), |s| format!("{} ({})", i.name, s)));
                match (outer.as_deref(), inner.as_deref()) {
                    (Some(o), Some(i)) => worn.push(format!("  {}: {} (over {})", slot.label(), o, i)),
                    (Some(o), None) => worn.push(format!("  {}: {}", slot.label(), o)),
                    (None, Some(i)) => worn.push(format!("  {}: {} (under layer only)", slot.label(), i)),
                    (None, None) => {}
                }
            }
        }
        if !worn.is_empty() {
            lines.push(format!("Equipped:\n{}", worn.join("\n")));
        }
    }

    // Belt — the 4-slot quick-access rack.
    if !ps.belt.is_empty() {
        let belt: Vec<String> = ps
            .belt
            .iter()
            .map(|i| {
                let stats = i.stats.as_deref().map(|s| format!(" ({})", s)).unwrap_or_default();
                if i.qty > 1 {
                    format!("  {} x{}{}", i.name, i.qty, stats)
                } else {
                    format!("  {}{}", i.name, stats)
                }
            })
            .collect();
        lines.push(format!("Belt:\n{}", belt.join("\n")));
    }

    // Pack — deep storage + carried-weight readout.
    if !ps.pack.is_empty() {
        let pack: Vec<String> = ps
            .pack
            .iter()
            .map(|i| {
                let stats = i.stats.as_deref().map(|s| format!(" ({})", s)).unwrap_or_default();
                if i.qty > 1 {
                    format!("  {} x{} ({} lb){}", i.name, i.qty, i.total_weight(), stats)
                } else {
                    format!("  {} ({} lb){}", i.name, i.total_weight(), stats)
                }
            })
            .collect();
        let carried = equipment::stack_weight(&ps.pack);
        lines.push(format!(
            "Pack ({} lb):\n{}",
            carried, pack.join("\n")
        ));
    }

    if lines.is_empty() {
        "The player is carrying nothing.".to_string()
    } else {
        lines.join("\n")
    }
}

async fn route_to_fable_query(
    focus: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: &tauri::State<'_, AppState>,
) -> Result<(), String> {
    let snapshot = state.fable_schema.lock().await.clone();
    let state_json = snapshot.to_json_pretty();

    // Inventory focus (2026-08-07): items now live in the typed
    // player_state.{equipment,belt,pack} model, not the freeform entity map
    // (legacy item_*/inv_* entities are migrated out on load). So an
    // "inventory"/"items"/"equipment"/"carrying"/"pack"/"belt" focus must
    // render from the typed model — the entity substring match below would
    // find nothing. Build a tight summary Wupi narrates from.
    let lower = focus.to_lowercase();
    let inventory_focus = matches!(
        lower.as_str(),
        "inventory" | "items" | "item" | "equipment" | "carrying" | "pack" | "backpack" | "belt" | "carried"
    );
    let focused = if inventory_focus {
        render_inventory_summary(&snapshot)
    } else if focus.is_empty() {
        state_json.clone()
    } else {
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

/// Silently strip + canonically apply any `rel.<npc_id>` entity writes in a
/// SchemaDelta (Fable Phase 3 Slice 5 wiring, 2026-07-28).
///
/// The architect directive forbids the LLM from writing gated relationship
/// tiers directly — the relationship state machine is Rust-authoritative.
/// The ONE exception is Stranger → Acquaintance (the no-gate auto-advance on
/// a first positive interaction). Every other attempted tier write is
/// SILENTLY DROPPED — no repair queue (re-running the same prompt won't
/// clear a still-closed time gate, per the §11.34 contract).
///
/// This helper:
/// 1. Scans `delta.entities` for keys matching `rel.<npc_id>` (the
///    relationship-write convention).
/// 2. For each, parses the attempted tier via `relationship::parse_tier`.
/// 3. Looks up the canonical `RelationshipState` in `schema.relationships`
///    (or a default Stranger state if the NPC isn't tracked yet).
/// 4. Calls `relationship::validate_llm_tier_write`.
///    - `Accept` (Stranger → Acquaintance only): upserts the relationship
///      state — advances the tier + stamps `tier_entered_at_minutes` to the
///      current clock. Removes the key from the delta (it's been consumed;
///      the canonical state is the source of truth, not the entity map).
///    - `Reject` (any other gated transition): removes the key from the
///      delta silently. The canonical tier in `schema.relationships` stands.
///    - `Unparseable` (the value isn't a known tier keyword): removes the
///      key — same silent-drop.
/// 5. Returns the list of stripped keys (for `tracing::info!` at the call
///    site so the playtest can verify the firewall is firing).
///
/// Pure fn — no I/O, no locks. The caller holds the schema lock + passes
/// `&mut delta` + `&schema` + the current clock. Called at all three
/// `apply_delta` sites in lib.rs BEFORE `apply_delta` runs.
fn strip_invalid_relationship_writes(
    delta: &mut schema::SchemaDelta,
    schema: &mut schema::WorldSchema,
) -> Vec<String> {
    let Some(ents) = delta.entities.as_mut() else {
        return Vec::new();
    };

    let now_minutes = schema.world_clock.current_minutes;
    let mut stripped = Vec::new();
    let mut accepted_upserts: Vec<(String, relationship::RelationshipTier)> = Vec::new();

    // Walk the entity keys. Collect the rel.* keys to process (can't mutate
    // the HashMap while iterating it).
    let rel_keys: Vec<String> = ents
        .keys()
        .filter(|k| k.starts_with("rel."))
        .cloned()
        .collect();

    for key in &rel_keys {
        // The NPC id is the suffix after "rel.".
        let npc_id = &key[4..];
        if npc_id.is_empty() {
            continue;
        }
        let Some(attempted_value) = ents.get(key).cloned().flatten() else {
            // A delete (None) on a rel.* key — also strip (the LLM can't
            // delete a relationship; only Rust owns that).
            ents.remove(key);
            stripped.push(format!("{key} (delete attempt)"));
            continue;
        };

        // Relationship-tier keys are conventionally bare strings ("Friend",
        // "Ally"). A structured value at a rel.* key is unrecognized noise —
        // treat as Unparseable (the validator's silent-drop path).
        let attempted_str = attempted_value.as_str().unwrap_or("");

        // Look up the canonical state (default Stranger if untracked).
        let current = schema
            .relationships
            .get(npc_id)
            .cloned()
            .unwrap_or_default();

        let validation = relationship::validate_llm_tier_write(attempted_str, &current);
        match validation {
            relationship::RelationshipValidation::Accept => {
                // The one allowed LLM transition: Stranger → Acquaintance.
                // Record the upsert; apply after the loop (can't borrow schema
                // mutably while iterating). The attempted tier IS the new tier
                // (validate_llm_tier_write already checked it's Acquaintance).
                let new_tier = relationship::parse_tier(attempted_str)
                    .unwrap_or(relationship::RelationshipTier::Acquaintance);
                accepted_upserts.push((npc_id.to_string(), new_tier));
                // Remove from the delta — the canonical state is the source
                // of truth, not the entity map. apply_delta must not also
                // write rel.* into entities.
                ents.remove(key);
                tracing::info!(
                    npc_id = %npc_id,
                    new_tier = ?new_tier,
                    "[rel] LLM Stranger→Acquaintance auto-advance accepted"
                );
            }
            relationship::RelationshipValidation::Reject { actual_tier } => {
                ents.remove(key);
                stripped.push(format!("{key}={attempted_value} (rejected; actual tier: {:?})", actual_tier));
                tracing::info!(
                    npc_id = %npc_id,
                    attempted = %attempted_value,
                    actual = ?actual_tier,
                    "[rel] LLM gated-tier write silently dropped"
                );
            }
            relationship::RelationshipValidation::Unparseable => {
                ents.remove(key);
                stripped.push(format!("{key}={attempted_value} (unparseable tier)"));
                tracing::info!(
                    npc_id = %npc_id,
                    attempted = %attempted_value,
                    "[rel] LLM tier write unparseable — dropped"
                );
            }
        }
    }

    // Apply the accepted upserts. Each advances the tier + stamps the
    // entered-at timestamp so the time-floor gate for the NEXT advance
    // starts ticking from now.
    for (npc_id, new_tier) in accepted_upserts {
        let rel = schema.relationships.entry(npc_id).or_default();
        rel.tier = new_tier;
        rel.tier_entered_at_minutes = now_minutes;
    }

    stripped
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
                return route_to_fable_manager(text, on_event, &app, &state).await;
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
    // turn, await it BEFORE acquiring the local-model lock. This is load-
    // bearing for ordering: the pending delta task needs the local_model_lock
    // to run its decode, so we must NOT hold the lock while awaiting it (else
    // deadlock — the task can never acquire the lock we're holding). To the
    // user this looks like normal thinking time: the frontend gets no signal
    // until the first chunk arrives, so a pre-stream delay is indistinguishable
    // from model latency. Errors are ignored: schema is best-effort.
    if let Some(handle) = state.pending_delta.lock().await.take() {
        let _ = handle.await;
    }

    // ── Local-model turn lock (2026-08-08) ──────────────────────────────
    // Acquire the process-wide local-model lock for the chat turn. This
    // serializes chat against the Fable tracker + the schema engine (the
    // other two local-model consumers) so at most ONE local decode runs at
    // any instant. The swap-lock alone does NOT serialize turns (its guard
    // drop is a no-op) — see AppState::local_model_lock doc. Held across the
    // lease, the decode, and the schema-delta spawn; dropped on every exit
    // path (success, cancel, error) by RAII scope drop. The delta task itself
    // re-acquires the lock when it runs — fire-and-forget, never awaited
    // under this guard.
    let _local_model_guard = state.local_model_lock.lock().await;

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
    // "" → section suppressed. The drawer always speaks in this voice — even
    // inside Fable (the narrator path, `fable_send`, reads `active_fable_card`
    // independently for the in-world simulation).
    let persona = state
        .active_card
        .get()
        .map(|c| c.render_for_prompt());
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
    .map(|p| p.render_for_prompt())
    // Chloe 2026-07-27: anti-positivity-bias default. When the profile is
    // missing, malformed, OR totally blank (the shipped data/user.xml is
    // empty), `render_for_prompt` returns "" and `.filter` below drops it.
    // The model would then see NO name at all — and might invent one or
    // address the operator by an inferred title. Per the standing contract,
    // the operator is always their profile name or the literal "User".
    // Fall back to a minimal name-only block so the model always has a
    // neutral handle. (The Fable narrator path uses the SIM card's
    // <player_name> with the same "User" default — separate path.)
    .filter(|s| !s.trim().is_empty())
    .or_else(|| Some("<user_profile>\nname: User\n</user_profile>".to_owned()));
    // §2F eager-prefill sliding window (2026-07-13): cap visible history to
    // the last VISIBLE_WINDOW messages regardless of token budget. Memory (M)
    // backfills evicted turns via retrieval. Truncation in the engine becomes
    // a safety net that effectively never fires (4 short turns ≪ ~3000 budget).
    //
    // Wupi chat is LOCAL-ONLY (2026-08-08 override): the window is always the
    // local 8-message window + the 2048 context. The API path is deleted
    // (the API is exclusively the Fable narrator now); model_source no longer
    // drives chat routing. The local chat backend carries short assistant
    // replies only — no narration, no long payloads. 8 messages = 4
    // user↔assistant exchanges, plenty for a tracking assistant + small talk,
    // fits the 2048 budget.
    let visible_window = settings::WINDOW_LOCAL_CHAT;

    // System prompt: assembled inline from the authored copilot directive
    // (`data/wupi.prompt`) + persona + user_profile. The `.prompt` file holds
    // WHAT Wupi does (role/capabilities/workflow/output discipline); the card
    // persona (`wupi.sim`) holds WHO she is (identity/voice/appearance). Both
    // surfaces reach here: the main OS chat (`script.js`) AND the Fable right
    // drawer (`wupi-drawer.js`) — the Fable game-manager gate (`classify`
    // above) returns early for management intents, so this assembly only runs
    // for the NotACommand fall-through (normal Wupi chat) in either context.
    let effective_ctx = settings::CTX_LOCAL_WITH_API;
    let mut sections: Vec<String> = Vec::new();
    if let Some(w) = state
        .wupi_prompts
        .get()
        .map(|p| p.role.trim())
        .filter(|s| !s.is_empty())
    {
        sections.push(w.to_owned());
    }
    if let Some(p) = persona.as_deref().filter(|s| !s.trim().is_empty()) {
        sections.push(p.to_owned());
    }
    if let Some(p) = user_profile.as_deref().filter(|s| !s.trim().is_empty()) {
        sections.push(p.to_owned());
    }
    sections.push(format!(
        "<current_context>\ncontext_size: {effective_ctx}\nconversation_budget: {}\n</current_context>",
        settings.conversation_budget
    ));
    let system_prompt = sections.join("\n\n");

    // Append the user message to the session. The message window is re-read
    // later by `run_local_or_echo` (inside `run_agent_loop`) directly from
    // the session, so we don't need to build + carry a `messages` Vec here
    // (that was only for the now-deleted API `http.stream()` call).
    {
        let mut s = state.session.lock().await;
        s.add_message(session::Role::User, text.clone());
    }

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
    // Wupi chat is LOCAL-ONLY (2026-08-08 override): the chat backend ALWAYS
    // runs at 2048 (CTX_LOCAL_WITH_API). The 4096 context is retired for chat
    // — it was for narrative, which the local model no longer does. The API
    // path is deleted; there is no source-dependent teardown or re-spawn here.
    let chat_context_size = settings::CTX_LOCAL_WITH_API;
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
    // boot) left it resident at 2048. Slow path: re-spawn from shared model
    // at the same 2048. No source-dependent teardown — chat is always local.
    let spawn_ctx = chat_context_size;
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
                    spawn_ctx,
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
        // Always-on tools (file ops, codex, sim cards, user profile).
        let mut s = tools::specs();
        // Fable-only tools. Attached ONLY when a Fable session is active so
        // they stay invisible to the model in plain Wupi-assistant chat (the
        // false-tool-call guardrail at the prompt level — saves ~250 tokens
        // of tool declarations when no game is running). Two buckets:
        //   - `fable_specs()`     — sync Trait tools (currently empty; the
        //                            Director suite was removed).
        //   - `fable_state_specs()` — the 3 async-dispatched stateful tools
        //                            (fable_message_edit/delete,
        //                            fable_schema_patch). Their specs render
        //                            into the prompt; their execute path goes
        //                            through `dispatch_fable_state_tool`.
        if fable_is_active(&state) {
            s.extend(tools::fable_specs());
            s.extend(tools::fable_state_specs());
        }
        s
    } else {
        Vec::new()
    };

    // Per-pass system prompt (the core of the 2026-07-29 persona/protocol split).
    // The .sim cards are now PURE FLAVOR (identity, voice, personality) — they
    // carry no technical protocols. The structured-output protocol
    // (`WUPI_AGENT_PROTOCOL`) is Rust-injected here, scoped to the TOOL pass
    // ONLY. This mirrors the existing Fable split (narrator = prose, tracker/
    // scribe = mechanical): the catgirl persona carries all conversational text,
    // and the agent protocol carries the machine-readable surface (tool args,
    // file contents, code). The two are complementary — no KV clear is needed
    // (unlike the §11.52 tracker split, where the two prompts conflicted).
    //
    // `chat_tools.is_empty()` is the precise pass discriminator: when tools are
    // attached, this decode is an AGENT pass (may emit tool calls → needs the
    // protocol); when empty, it's a PROSE pass (pure persona, no protocol). The
    // API handoff (`messages` assembled at line ~2883) + both local fallbacks
    // (below) keep the bare `system_prompt` — the protocol would be inert there
    // anyway (no tools advertised), but keeping it out also keeps those decodes
    // byte-identical to the pre-change behavior (zero cold-reset risk).
    let agent_system_prompt = if chat_tools.is_empty() {
        system_prompt.clone()
    } else {
        // The `prompts` module's WUPI_AGENT_PROTOCOL was removed; the tool pass
        // now uses the base system prompt (the tool descriptions themselves
        // carry the structured-output contract via their schemas).
        system_prompt.clone()
    };

    // ── Wupi chat is LOCAL-ONLY (2026-08-08 override) ───────────────────
    // The API branch is DELETED. Wupi chat (OS home + the Fable Wupi drawer)
    // always runs on the local 12B at 2048, temp 0.85, with `<think>` always
    // on + tools. The API is reserved exclusively for Fable narration
    // (`fable_send`, a separate path). `model_source` no longer drives chat
    // routing — it's now purely a Fable-eligibility flag.
    //
    // `run_agent_loop` IS the user-visible reply path: it handles tool
    // calling end-to-end (fast path when no tools, multi-iteration loop
    // otherwise) and returns `(ParsedOutput, bool)` — the exact shape the
    // post-routing code (session append, memory archive, schema delta, done
    // event) consumes. It is model_source-agnostic.
    let (result, tools_fired) = match run_agent_loop(
        &state, &app, &on_event, &agent_system_prompt, visible_window,
        memory_block, world_state, chat_tools,
        settings::CTX_LOCAL_WITH_API,
        on_chunk.clone(), cancel.clone(), backend_opt.clone(),
    )
    .await
    {
        Ok((result, tools_fired)) => (result, tools_fired),
        Err(()) => {
            rollback_last_user_message(&state, &app).await;
            return Ok(());
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
        let local_model_lock = Arc::clone(&state.local_model_lock);
        let handle = tokio::spawn(async move {
            // Acquire the Schema lease inside the task. This blocks until
            // any resident chat/fable context is torn down (the chat turn's
            // lease drops when chat_send returns, which has already
            // happened by the time this task is scheduled — but the lease
            // makes the VRAM ordering explicit).
            let (schema_engine, _lease, _model_guard) = match acquire_schema_engine_from_arcs(
                context_swap,
                schema_engine_slot,
                local_model_lock,
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

// ---------------------------------------------------------------------------
// Fable-state tool dispatch (2026-08-11)
// ---------------------------------------------------------------------------

/// Dispatch a Fable-state tool call (one of `fable_message_edit` /
/// `fable_message_delete` / `fable_schema_patch`) against the LIVE AppState
/// mutexes. Returns `Ok(Some(msg))` on success, `Err(err)` on validation /
/// execution failure (the agent loop folds the error into the tool_result
/// payload), or `Ok(None)` if `call.name` isn't a Fable-state tool (so the
/// caller falls through to the sync `registry`).
///
/// Why this bypasses the sync `Tool::execute` trait: the three stateful tools
/// need to await `tokio::sync::Mutex` locks on `state.fable_session` /
/// `state.fable_schema`. The trait's `execute` is sync (the 7 file tools are
/// pure `std::fs` calls). Rather than make the whole trait async (cascading
/// to every tool + adding async-trait), the agent loop — which is itself
/// async + already holds the local-model turn lock — dispatches these three
/// names inline. Validation lives in `tools::validate_fable_state_tool` so
/// it stays unit-testable without AppState.
async fn dispatch_fable_state_tool(
    state: &tauri::State<'_, AppState>,
    app: &tauri::AppHandle,
    call: &tools::ToolCall,
) -> Result<Option<String>, String> {
    if !tools::is_fable_state_tool(&call.name) {
        return Ok(None);
    }
    // Validate args shape first (cheap, no locks).
    tools::validate_fable_state_tool(&call.name, &call.args)
        .map_err(|e| format!("invalid args: {e}"))?;
    // Seat check — every stateful tool needs an active Fable game.
    let card_id = active_fable_card_id(state)?;
    let outcome: String = match call.name.as_str() {
        "fable_message_edit" => {
            let index = call.args["index"].as_u64().unwrap_or(0) as usize;
            let content = call.args["content"].as_str().unwrap_or("").to_string();
            let messages = {
                let mut gs = state.fable_session.lock().await;
                apply_edit(&mut gs, index, content)?;
                project_messages(&gs)
            };
            persist_fable_session(app, state, &card_id).await?;
            let _ = app.emit(
                "fable-session-changed",
                serde_json::json!({ "kind": "messages", "messages": messages }),
            );
            format!("edited message at index {index}")
        }
        "fable_message_delete" => {
            let index = call.args["index"].as_u64().unwrap_or(0) as usize;
            let messages = {
                let mut gs = state.fable_session.lock().await;
                gs.remove_at(index)?;
                project_messages(&gs)
            };
            persist_fable_session(app, state, &card_id).await?;
            let _ = app.emit(
                "fable-session-changed",
                serde_json::json!({ "kind": "messages", "messages": messages }),
            );
            format!("deleted message at index {index}")
        }
        "fable_schema_patch" => {
            let patch = call.args.get("patch").cloned().unwrap_or(serde_json::Value::Null);
            // Snapshot prior schema → undo buffer (same trust-class as
            // fable_schema_set: the model path is undoable via fable_rollback).
            {
                let snap = state.fable_schema.lock().await.clone();
                push_fable_history_snapshot(state, snap).await;
            }
            let merged_keys = {
                let mut s = state.fable_schema.lock().await;
                s.merge_patch(patch)?
            };
            // Persist via the same path fable_schema_set uses.
            let roleplay_card_id = state
                .active_card_id
                .lock()
                .expect("active_card_id mutex")
                .clone();
            let schema_snapshot = state.fable_schema.lock().await.clone();
            save_schema(app, &roleplay_card_id, &schema_snapshot).await;
            let _ = app.emit(
                "fable-session-changed",
                serde_json::json!({ "kind": "schema", "merged_keys": merged_keys }),
            );
            format!("patched schema fields: {}", merged_keys.join(", "))
        }
        _ => return Ok(None), // unreachable: is_fable_state_tool gated above
    };
    Ok(Some(outcome))
}

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

    // NOTE: the API-mode `force_full_ctx` teardown used to live here but was
    // moved UP into `chat_send` (before the `backend_opt` computation). The
    // agent loop receives an already-fresh backend via `backend_opt`, so it
    // doesn't need its own teardown — putting it here would race with
    // `run_local_or_echo`'s use of the (stale) passed-in backend Arc,
    // producing "model not loaded yet" errors. See chat_send's
    // `force_full_ctx` block (~line 2646) for the full Bug #2 history.

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
    // The lookup registry. `chat_send` already folded `fable_specs()` into the
    // `tools` Vec when a Fable session is active (so the model sees them in the
    // prompt); the executor registry here must mirror that. `fable_registry()`
    // is currently empty (the Director suite was removed), so this is a no-op
    // today — when Fable tools are reintroduced, gate the extend on whether the
    // passed-in `tools` Vec contains fable tool names (or thread an explicit
    // flag), not on a side-channel slot.
    let mut registry = tools::registry();
    registry.extend(tools::fable_registry());

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

            // Dispatch path: Fable-state tools (async, need AppState mutex
            // access) go through `dispatch_fable_state_tool`; everything else
            // hits the sync `Tool::execute` registry. The stateful dispatcher
            // returns Ok(None) for unknown-to-it names so the fallthrough is
            // clean (Ok(Some(msg)) = success, Err(e) = folded into tool_result).
            let outcome: (bool, String) =
                match dispatch_fable_state_tool(state, app, call).await {
                    Ok(Some(msg)) => (true, msg),
                    Err(e) => (false, e),
                    Ok(None) => match registry.iter().find(|t| t.spec().name == call.name) {
                        Some(tool) => match tool.validate_args(&call.args) {
                            Ok(()) => match tool.execute(&call.args, &ctx) {
                                Ok(output) => (true, output),
                                Err(e) => (false, format!("error: {e}")),
                            },
                            Err(e) => (false, format!("invalid args: {e}")),
                        },
                        None => (false, format!("unknown tool: {}", call.name)),
                    },
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

/// Push a failed World Progression attempt onto the tick queue (Fable Seam
/// #4, 2026-07-27). Mirrors `enqueue_failed_translation` for the clock-tick
/// path. Drained on the next tick fire + folded into the next progression
/// prompt so the model gets another shot with the new interval.
async fn enqueue_failed_progression(
    state: &tauri::State<'_, AppState>,
    attempt: schema_engine::FailedAttempt,
) {
    let mut q = state.failed_progression_queue.lock().await;
    if q.len() >= MAX_FAILED_DELTA_ATTEMPTS {
        q.remove(0);
        tracing::warn!(
            queue_len = q.len(),
            "failed_progression_queue overflow; evicted oldest entry"
        );
    }
    q.push(attempt);
}

/// World Progression tick interval (Fable Seam #4, 2026-07-27). How much
/// in-world time must elapse before an off-screen simulation pass fires.
/// Default 24h matches Multihog's default + the natural "daily report"
/// cadence. Configurable via settings is deferred (v1 hardcoded; a future
/// settings knob + per-card override is straightforward once the UI exists).
///
/// The check is `world_clock.minutes_since_last_tick() >= INTERVAL * 60`.
/// Pure arithmetic on the typed `i64` — no calendar library, no fuzzy
/// comparison, no model discretion. Rust owns the gate; the model can't
/// talk itself out of generating an off-screen update.
/// Legacy World Progression tick interval (Fable Seam #4, 2026-07-27).
/// Superseded 2026-07-27 by the ScenePacing-driven interval
/// (`SceneMode::progression_interval_hours`) — Combat/Downtime/Exploration
/// each get their own interval. Retained for any future "fixed-interval"
/// debug mode + as documentation of the original 24h default.
#[allow(dead_code)]
const WORLD_PROGRESSION_INTERVAL_HOURS: u32 = 24;

/// Ring-buffer cap for `AppState::fable_schema_history` (1-click undo,
/// 2026-07-27). 5 snapshots = the last 5 world-state mutations are undoable.
/// Bounded to cap memory growth (each clone holds the full `entities`
/// HashMap + `recent_events` Vec — moderate cost, dominated by entity count).
const FABLE_HISTORY_CAP: usize = 5;

/// Max char length of a UI-action `event_note` passed to `fable_schema_set`
/// (2026-08-08). The note is appended to `recent_events` so the next narrator
/// turn sees the player's Soul Gem inventory action (EQUIP/CONSUME/...). 160
/// chars is generous for "equipped Iron Sword" / "consumed Health Potion" while
/// keeping the prompt-bloat cost trivial (the recent_events render caps at the
/// last 5, so the total injected budget is ≤ 800 chars).
const EVENT_NOTE_MAX: usize = 160;

/// Snapshot the current `fable_schema` into the history ring buffer before a
/// mutation. Pushes a clone of the LIVE (pre-mutation) state so a later
/// `fable_rollback` can restore it. Caps at `FABLE_HISTORY_CAP` (FIFO eviction
/// of the oldest). One brief lock on each side — no `await` while held.
async fn push_fable_history(state: &AppState) {
    let snapshot = state.fable_schema.lock().await.clone();
    push_fable_history_snapshot(state, snapshot).await;
}

/// Same as `push_fable_history` but accepts a pre-cloned snapshot, so callers
/// already holding the `fable_schema` lock can snapshot-and-mutate atomically
/// without re-acquiring it (avoids the drop-and-relock race). Use this inside
/// any `fable_schema.lock().await` critical section where the pre-mutation
/// state is already in hand.
async fn push_fable_history_snapshot(state: &AppState, snapshot: schema::WorldSchema) {
    let mut hist = state.fable_schema_history.lock().await;
    if hist.len() >= FABLE_HISTORY_CAP {
        hist.pop_front();
    }
    hist.push_back(snapshot);
}

/// Clear the history ring buffer. Called at every session boundary
/// (`fable_start`, `fable_end`, `fable_load_save`): those wholesale-overwrite
/// `fable_schema`, so the prior history is no
/// longer meaningful to undo INTO (it belongs to a different game session).
async fn clear_fable_history(state: &AppState) {
    state.fable_schema_history.lock().await.clear();
}

/// Apply any `[TIME ...]` bracket command from the narrator's output, then
/// check whether the in-world clock has advanced past the World Progression
/// interval. If so, fire the off-screen simulation pass against `fable_schema`
/// (Fable Seam #4, 2026-07-27).
///
/// Called from `fable_send` right after bracket parsing (after `scene_event`
/// emits, before memory archival). The insertion point is load-bearing: the
/// narrator's `[TIME ...]` emissions are already extracted into
/// `parsed.commands`, so we can scan them here without re-parsing.
///
/// # Monotonic guard
///
/// The clock only moves FORWARD. A narrator emitting `[TIME Day 1]` after
/// `[TIME Day 5]` (a regression) is warned + ignored — the simulation
/// depends on monotonic time for the tick gate to be meaningful.
///
/// # First-call baseline
///
/// The FIRST `[TIME]` ever emitted stamps `last_tick_minutes = current_minutes`
/// without firing (matches Multihog's first-call behavior: a campaign doesn't
/// simulate a day it hasn't established yet). Subsequent advances fire when
/// the elapsed delta crosses the interval.
///
/// # VRAM + lease ordering
///
/// The progression pass acquires `ContextRole::Schema` via the existing
/// Sanitize a raw id (from a `[DISCOVER]`/`[NPC_REGISTER]` bracket) into a
/// stable bare slug: lowercase, replace any non-alphanumeric char with `_`,
/// trim leading/trailing `_`. Returns empty string if the result is empty
/// (the caller treats empty as "skip this command"). Uses `_` as the
/// separator to match the card-seed node-id convention (`market_square`,
/// `north_road`, `shell_town`) — mirrors `api::sanitize_profile_id`'s
/// contract but with `_` instead of `-`.
fn sanitize_slug(raw: &str) -> String {
    let slug: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    slug.trim_matches('_').to_owned()
}

/// Apply the Fable Phase 3 bracket commands (`[EFFECT ...]`, `[MILESTONE ...]`,
/// `[TASK ...]`) emitted by the narrator this turn. Called from `fable_send`
/// right after bracket parsing, parallel to `apply_time_command_and_maybe_tick`.
///
/// Each command mutates Rust-authoritative state on `WorldSchema` (the new
/// `status_tags` / `relationships` / `offscreen_tasks` fields — never written
/// by LLM schema deltas). Defensive: a malformed command is a no-op + a
/// `tracing::warn!` log; never a panic. Returns true if any mutation landed
/// (so the caller can push an undo snapshot once).
///
/// `[EFFECT]` creates a status tag with `expires_at = current_clock +
/// duration_minutes` (0 duration = permanent, the sentinel).
/// `[MILESTONE]` records an event on the NPC's `RelationshipState` (creating
/// one if the NPC isn't tracked yet). The transition evaluation is LAZY —
/// it fires on the next render via `evaluate_transition`, not here.
/// `[TASK]` queues an off-screen task with `resolves_at_minutes =
/// current_clock + eta_minutes`. Resolution happens on the World Progression
/// tick (not here).
async fn apply_phase3_bracket_commands(
    parsed: &bracket_parser::ParsedNarration,
    state: &tauri::State<'_, AppState>,
) -> (bool, Vec<String>) {
    // Collect the relevant commands first (no lock held).
    let effects: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Effect { .. }))
        .collect();
    let milestones: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Milestone { .. }))
        .collect();
    let tasks: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Task { .. }))
        .collect();
    let weather_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Weather { .. }))
        .collect();
    let date_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Date { .. }))
        .collect();
    let travel_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Travel { .. }))
        .collect();
    // Component 4 (2026-07-28): rumor creation commands. The applier roots
    // each at the current node (known_nodes = [origin]); the tick propagates
    // them. No reject-directive channel — rumors don't reject (the §11.46
    // contract reserves that for [TRAVEL] adjacency). A [RUMOR] with no
    // current node is warn-and-skip (mirrors [MILESTONE]'s unknown-id drop).
    let rumor_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Rumor { .. }))
        .collect();
    // Phase 5A (2026-07-29): presence-assertion commands. The applier resolves
    // each surface form to a canonical id via NpcRegistry::resolve, rejects
    // unknown ids (the anti-hallucination gate — the §11.46 reject-directive
    // channel), + applies the 1-turn grace TTL to the presences Vec. Unlike
    // the other brackets, presence runs EVERY turn even when only asserting
    // (the grace-decay pass must fire on every turn, including turns with no
    // [PRESENCE] at all — handled by the dedicated decay step in fable_send's
    // turn flow, NOT here; this applier only handles the assertion side).
    let presence_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Presence { .. }))
        .collect();
    // Dynamic world-seeding (the fix for sandbox cards that seed no
    // <locations>/<cast>): discover + npc_register commands. The appliers
    // insert new travel-graph nodes / npc_registry entries idempotently (no-op
    // on duplicate id — never reject; the point is growth, not enforcement).
    // DISCOVER runs before TRAVEL (discover a place, then move into it the
    // same turn); NPC_REGISTER runs before PRESENCE (register + assert a new
    // NPC in one turn). A node/NPC pushed here is visible to the next render
    // automatically (render_for_prompt reads the Vecs fresh each turn).
    let discover_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Discover { .. }))
        .collect();
    let npc_register_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::NpcRegister { .. }))
        .collect();
    // Phase 4 Component 5 (2026-08-04): dynamic appearance deltas. Each
    // [APPEARANCE key=value] upserts one entry in
    // PlayerState::current_appearance_deltas (empty value clears). Pure
    // mutation under the schema lock — atomic with the world-state, same as
    // WEATHER/EFFECT. The deltas render into <world_state> via
    // PlayerState::render_for_prompt's appearance: block.
    let appearance_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Appearance { .. }))
        .collect();
    // Inventory (2026-08-07): the three equipment brackets. [EQUIP] writes the
    // worn-item two-layer model; [BELT]/[PACK] write the stack lists. All three
    // mutate PlayerState under the schema lock (atomic with world-state, same
    // as APPEARANCE). The rendered Outer-layer block flows into <world_state>
    // via PlayerState::render_for_prompt's equipped: sub-block.
    let equip_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Equip { .. }))
        .collect();
    let belt_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Belt { .. }))
        .collect();
    let pack_cmds: Vec<&bracket_parser::BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, bracket_parser::BracketCommand::Pack { .. }))
        .collect();

    if effects.is_empty()
        && milestones.is_empty()
        && tasks.is_empty()
        && weather_cmds.is_empty()
        && travel_cmds.is_empty()
        && rumor_cmds.is_empty()
        && presence_cmds.is_empty()
        && discover_cmds.is_empty()
        && npc_register_cmds.is_empty()
        && appearance_cmds.is_empty()
        && equip_cmds.is_empty()
        && belt_cmds.is_empty()
        && pack_cmds.is_empty()
    {
        // Phase 5A: even with no bracket commands this turn, presence grace-
        // decay must run if there are existing presences (a turn with zero
        // [PRESENCE] assertions should still decay the whitelist — otherwise
        // the cast would freeze on-camera forever once asserted). Fall through
        // to the presence block below; if presences is also empty, the block
        // is a no-op.
        // Peek under the lock: if there's nothing to decay, early-out (zero
        // work, zero snapshot).
        let nothing_to_decay = state.fable_schema.lock().await.presences.is_empty();
        if nothing_to_decay {
            return (false, Vec::new());
        }
    }

    let mut mutated = false;
    let mut undo_snapshot: Option<schema::WorldSchema> = None;
    // Reject directives surfaced to the narrator (Component 3, 2026-07-28):
    // e.g. "[TRAVEL] rejected — non-adjacent move". Caller merges these into
    // the same `<directives>` block as the Referees' output. Empty for the
    // historical bracket commands (EFFECT/MILESTONE/TASK/WEATHER never reject).
    let mut reject_directives: Vec<String> = Vec::new();

    {
        let mut s = state.fable_schema.lock().await;
        let now_minutes = s.world_clock.current_minutes;

        // [EFFECT] — create status tags.
        for cmd in &effects {
            if let bracket_parser::BracketCommand::Effect {
                label,
                polarity,
                duration_minutes,
                tag_kind,
            } = cmd
            {
                let expires_at = if *duration_minutes == 0 {
                    0 // permanent sentinel
                } else {
                    now_minutes.saturating_add(*duration_minutes)
                };
                let tag = consequence::StatusTag {
                    label: label.clone(),
                    polarity: *polarity,
                    expires_at,
                    source: String::new(),
                    // §11.44: thread the parser's tag_kind into StatusTag.kind.
                    kind: tag_kind.clone(),
                };
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                consequence::add_tag(&mut s.status_tags, tag);
                mutated = true;
                tracing::info!(
                    label = %label,
                    polarity = ?polarity,
                    expires_at,
                    "[EFFECT] status tag added"
                );
            }
        }

        // [MILESTONE] — record relationship events.
        for cmd in &milestones {
            if let bracket_parser::BracketCommand::Milestone { npc_id, event_id } = cmd {
                // Validate the event id against the default registry. Unknown
                // ids are dropped (no-op + warn) — the registry is the
                // authoritative list of diegetic events.
                let registry = relationship::MilestoneRegistry::defaults();
                if registry.get(event_id).is_none() {
                    tracing::warn!(
                        npc_id = %npc_id,
                        event_id = %event_id,
                        "[MILESTONE] unknown event id — dropping"
                    );
                    continue;
                }
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                let rel = s
                    .relationships
                    .entry(npc_id.clone())
                    .or_default();
                if rel.record_event(event_id) {
                    mutated = true;
                    tracing::info!(
                        npc_id = %npc_id,
                        event_id = %event_id,
                        "[MILESTONE] relationship event recorded"
                    );
                } else {
                    tracing::info!(
                        npc_id = %npc_id,
                        event_id = %event_id,
                        "[MILESTONE] duplicate event — already recorded (no-op)"
                    );
                }
            }
        }

        // [TASK] — queue off-screen tasks.
        for cmd in &tasks {
            if let bracket_parser::BracketCommand::Task {
                npc_id,
                description,
                difficulty,
                suitability,
                eta_minutes,
            } = cmd
            {
                // Parse the enum strings defensively. Unknown values → drop.
                let diff = match difficulty.to_lowercase().as_str() {
                    "trivial" => Some(offscreen_task::TaskDifficulty::Trivial),
                    "routine" => Some(offscreen_task::TaskDifficulty::Routine),
                    "challenging" => Some(offscreen_task::TaskDifficulty::Challenging),
                    "hard" => Some(offscreen_task::TaskDifficulty::Hard),
                    "nearimpossible" | "near_impossible" | "near-impossible" => {
                        Some(offscreen_task::TaskDifficulty::NearImpossible)
                    }
                    _ => None,
                };
                let suit = match suitability.to_lowercase().as_str() {
                    "hopeless" => Some(offscreen_task::Suitability::Hopeless),
                    "poor" => Some(offscreen_task::Suitability::Poor),
                    "adequate" => Some(offscreen_task::Suitability::Adequate),
                    "wellsuited" | "well_suited" | "well-suited" => {
                        Some(offscreen_task::Suitability::WellSuited)
                    }
                    "ideal" => Some(offscreen_task::Suitability::Ideal),
                    _ => None,
                };
                let (Some(diff), Some(suit)) = (diff, suit) else {
                    tracing::warn!(
                        npc_id = %npc_id,
                        difficulty = %difficulty,
                        suitability = %suitability,
                        "[TASK] unparseable difficulty/suitability — dropping"
                    );
                    continue;
                };
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                s.offscreen_tasks.push(offscreen_task::OffScreenTask {
                    npc_id: npc_id.clone(),
                    description: description.clone(),
                    difficulty: diff,
                    suitability: suit,
                    resolves_at_minutes: now_minutes.saturating_add(*eta_minutes),
                    resolved: false,
                });
                mutated = true;
                tracing::info!(
                    npc_id = %npc_id,
                    description = %description,
                    resolves_at = now_minutes.saturating_add(*eta_minutes),
                    "[TASK] off-screen task queued"
                );
            }
        }

        // [WEATHER] — set the global weather condition (Fable Phase 4
        // Component 2, 2026-07-28). Single-region schema-tracking command like
        // EFFECT/MILESTONE/TASK. Last-wins on multiples (mirrors the [TIME]
        // "last one is most recent + authoritative" contract at the top of
        // apply_time_command_and_maybe_tick). Stamps `started_at_minutes` so
        // the tick drift's persistence curve has a baseline to scale from.
        let last_weather = weather_cmds.iter().rev().find_map(|cmd| {
            if let bracket_parser::BracketCommand::Weather { condition } = cmd {
                Some(condition)
            } else {
                None
            }
        });
        if let Some(condition) = last_weather {
            if undo_snapshot.is_none() {
                undo_snapshot = Some(s.clone());
            }
            s.weather = schema::Weather {
                condition: condition.clone(),
                started_at_minutes: now_minutes,
            };
            mutated = true;
            tracing::info!(condition = %condition, "[WEATHER] weather condition set");
        }

        // [DATE] — rewrite the free-form calendar label (2026-08-13). The
        // tracker emits the NEW verbatim label on a calendar advance (a day+
        // passes, a month flips, etc. — no Rust arithmetic). Last-wins on
        // multiples (mirrors [WEATHER]). Empty clears the label.
        let last_date = date_cmds.iter().rev().find_map(|cmd| {
            if let bracket_parser::BracketCommand::Date { value } = cmd {
                Some(value)
            } else {
                None
            }
        });
        if let Some(value) = last_date {
            if undo_snapshot.is_none() {
                undo_snapshot = Some(s.clone());
            }
            s.calendar = if value.is_empty() { None } else { Some(value.clone()) };
            mutated = true;
            tracing::info!(calendar = %value, "[DATE] calendar label set");
        }

        // [DISCOVER] — dynamic world-seeding (the fix for sandbox cards that
        // seed no <locations> block). Registers a new travel-graph node so
        // [TRAVEL]/[RUMOR]/the location: line aren't frozen dead the whole
        // session. Idempotent: a node id that already exists is a no-op (the
        // tracker may re-emit a discovery; that's not an error). NEVER rejects
        // — discovery is growth, not enforcement (the §11.46 reject channel is
        // reserved for [TRAVEL] adjacency). Runs BEFORE [TRAVEL] so a turn can
        // discover a place + move into it together (the discovered node is
        // immediately a legal adjacency target). The id is sanitized to a bare
        // slug (lowercase, non-alphanumeric→underscore) so the tracker can't
        // inject garbage or collide with the `node.` prefix convention; the
        // diegetic name/setting/neighbors stay verbatim. The first DISCOVER on
        // an empty graph also sets current_node (so the player has somewhere to
        // "be" — mirrors the card-seed's first-node-as-seed behavior).
        for cmd in &discover_cmds {
            if let bracket_parser::BracketCommand::Discover {
                node_id,
                name,
                setting,
                neighbors,
            } = cmd
            {
                let id = sanitize_slug(node_id);
                if id.is_empty() {
                    continue;
                }
                let label = if name.trim().is_empty() {
                    id.clone()
                } else {
                    name.trim().to_string()
                };
                let node = schema::Node {
                    id: id.clone(),
                    name: label,
                    neighbors: neighbors
                        .iter()
                        .map(|n| sanitize_slug(n))
                        .filter(|n| !n.is_empty())
                        .collect(),
                    setting: setting.trim().to_string(),
                };
                let was_empty_graph = !s.travel_graph.is_set();
                if s.travel_graph.upsert_node(node) {
                    if undo_snapshot.is_none() {
                        undo_snapshot = Some(s.clone());
                    }
                    // First discovery on an empty graph seeds current_node so
                    // the player has a starting location (mirrors the card-
                    // seed's first-node-as-seed behavior). Subsequent discoveries
                    // don't move the player — use [TRAVEL] for that.
                    if was_empty_graph {
                        s.travel_graph.current_node = Some(id.clone());
                    }
                    mutated = true;
                    tracing::info!(
                        node_id = %id,
                        neighbor_count = neighbors.len(),
                        "[DISCOVER] travel-graph node registered"
                    );
                }
            }
        }

        // [TRAVEL] — advance `current_node` (Fable Phase 4 Component 3,
        // 2026-07-28). Single-region schema-tracking command, last-wins on
        // multiples (mirrors [WEATHER] / [TIME]). Rust is the SOLE authority
        // on legality:
        //   (a) destination must EXIST in the graph (else REJECT + directive —
        //       the anti-teleport gate that still matters);
        //   (b) if `current_node` is set + the destination is a KNOWN node but
        //       NOT a declared neighbor, AUTO-LINK the bidirectional edge +
        //       ALLOW the move (2026-08-10). The player walking there IS the
        //       evidence the two locations are connected — this is the organic
        //       edge-formation path for cards that ship no <locations> block,
        //       so dynamically-discovered nodes don't strand the player.
        //   (c) the FIRST `[TRAVEL]` from `current_node: None` is allowed
        //       (seeds initial location without scenario-card wiring).
        // Reject directives (only the unknown-node case now) surface to the
        // narrator via the return tuple; the caller merges them into the
        // `<directives>` block.
        let last_travel = travel_cmds.iter().rev().find_map(|cmd| {
            if let bracket_parser::BracketCommand::Travel { destination } = cmd {
                Some(destination.clone())
            } else {
                None
            }
        });
        if let Some(dest_raw) = last_travel {
            // Resolve the destination to a canonical node id. The tracker often
            // emits diegetic names ("Market Square") instead of bare slugs
            // ("market_square"); `resolve_node_id` normalizes + fuzzy-matches
            // so a legal move isn't rejected on a casing/spelling technicality
            // (2026-08-10, T52 Open Issue #1 — location was stuck all 52 turns
            // because every `[TRAVEL Market Square]` hit the unknown-node reject).
            match s.travel_graph.resolve_node_id(&dest_raw) {
                None => {
                    // Unknown destination — the Tracker invented a node id that
                    // doesn't fuzzy-match anything. List the known node ids.
                    let known: Vec<&str> =
                        s.travel_graph.nodes.iter().map(|n| n.id.as_str()).collect();
                    tracing::warn!(dest = %dest_raw, known = ?known, "[TRAVEL] rejected — unknown node");
                    reject_directives.push(format!(
                        "Travel to \"{dest_raw}\" is not possible — that location is not in the world. \
                         Known locations: {}. Stay where you are or travel to a known location.",
                        if known.is_empty() {
                            "(none defined)".to_string()
                        } else {
                            known.join(", ")
                        }
                    ));
                }
                Some(dest) if s.travel_graph.current_node.is_some()
                    && !s.travel_graph.is_adjacent_to_current(&dest) =>
                {
                    // Known but non-adjacent (2026-08-10 auto-link). The player
                    // traveling here IS evidence the two locations are connected
                    // — form the bidirectional edge + allow the move. This is the
                    // organic edge-formation path for cards with no <locations>
                    // block: a [DISCOVER] establishes node B, then a [TRAVEL B]
                    // from node A links A↔B without the tracker having to name
                    // the neighbor explicitly. (Previously this case rejected;
                    // that stranded players who reached a discovered node the
                    // graph hadn't pre-wired.)
                    if undo_snapshot.is_none() {
                        undo_snapshot = Some(s.clone());
                    }
                    let from_id = s.travel_graph.current_node.clone().unwrap_or_default();
                    s.travel_graph.link_nodes(&from_id, &dest);
                    let prev = s.travel_graph.current_node.clone();
                    s.travel_graph.current_node = Some(dest.clone());
                    mutated = true;
                    tracing::info!(
                        from = ?prev,
                        to = %dest,
                        "[TRAVEL] auto-linked edge + advanced (known non-adjacent)"
                    );
                }
                Some(dest) => {
                    // Legal adjacent move (or the bootstrap first-move-from-None
                    // case, which falls through here because the non-adjacent
                    // guard above requires current_node.is_some()).
                    if undo_snapshot.is_none() {
                        undo_snapshot = Some(s.clone());
                    }
                    let prev = s.travel_graph.current_node.clone();
                    s.travel_graph.current_node = Some(dest.clone());
                    mutated = true;
                    tracing::info!(
                        from = ?prev,
                        to = %dest,
                        "[TRAVEL] current_node advanced"
                    );
                }
            }
        }

    // [RUMOR] — seed a rumor at the current node (Component 4, 2026-07-28).
    // Runs inside the same schema lock as the TRAVEL/WEATHER/EFFECT appliers
    // above (so `s` + `now_minutes` are in scope). Append-ALL semantics (a
    // turn could legitimately seed 2 rumors): each [RUMOR] creates one Rumor
    // rooted at the current node. No last-wins dedupe (unlike [WEATHER], a
    // single global field) — the rumors field is a Vec, distinct labels are
    // distinct rumors. No reject-directive channel: rumors don't reject (a
    // [RUMOR] with no current node is warn-and-skip — mirrors [MILESTONE]'s
    // unknown-id drop pattern). The label is free-form diegetic text, so NO
    // registry validation (unlike [MILESTONE]'s event_id check).
    for cmd in &rumor_cmds {
        if let bracket_parser::BracketCommand::Rumor { label } = cmd {
            // Clone the current node id out of the immutable borrow before the
            // mutable `s.rumors.push` below (the borrow checker requires the
            // immutable `s.travel_graph.current_node.as_deref()` borrow to end
            // before `s` is borrowed mutably). Mirrors how the TRAVEL block
            // handles its `dest` clone above.
            if let Some(cur_id) = s.travel_graph.current_node.clone() {
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                s.rumors.push(rumor::Rumor {
                    label: label.clone(),
                    origin_node: cur_id.clone(),
                    known_nodes: vec![cur_id.clone()],
                    born_minutes: now_minutes,
                });
                mutated = true;
                tracing::info!(
                    label = %label,
                    origin = %cur_id,
                    "[RUMOR] rumor seeded at current node"
                );
            } else {
                // No current node → can't root the rumor. Warn-and-skip.
                tracing::warn!(
                    label = %label,
                    "[RUMOR] dropped — no current node to root at"
                );
            }
        }
    }

    // [NPC_REGISTER] — dynamic world-seeding (the fix for sandbox cards that
    // seed no <cast> block). Registers a new NPC into npc_registry so [PRESENCE]
    // is not frozen dead the whole session. Idempotent: an id that already
    // exists is a no-op (the tracker may re-emit a registration). NEVER rejects
    // — registration is growth, not enforcement. Runs BEFORE [PRESENCE] so a
    // turn can register a new NPC + assert its presence together (the new id is
    // immediately resolvable by the presence block below). The id is sanitized
    // to a bare slug (lowercase, non-alphanumeric→underscore); diegetic
    // name/role/tier stay verbatim. No borrow dance needed — `upsert_entry` is
    // a single `&mut self` call, no concurrent immutable read of the registry.
    for cmd in &npc_register_cmds {
        if let bracket_parser::BracketCommand::NpcRegister {
            npc_id,
            name,
            role,
            tier,
        } = cmd
        {
            let id = sanitize_slug(npc_id);
            if id.is_empty() {
                continue;
            }
            let label = if name.trim().is_empty() {
                id.clone()
            } else {
                name.trim().to_string()
            };
            let entry = schema::NpcEntry {
                id: id.clone(),
                name: label,
                role: role.trim().to_string(),
                tier: tier.clone().filter(|t| !t.trim().is_empty()),
                aliases: vec![id.clone()], // the id is its own alias so [PRESENCE id] resolves
            };
            if s.npc_registry.upsert_entry(entry) {
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                mutated = true;
                tracing::info!(npc_id = %id, "[NPC_REGISTER] npc_registry entry registered");
            }
        }
    }

    // [PRESENCE] — assert who is on-camera this turn (Phase 5A, 2026-07-29).
    // Runs inside the same schema lock. The load-bearing anti-teleport logic:
    //   1. Resolve each [PRESENCE] surface form (id OR alias) via
    //      NpcRegistry::resolve. Unknown forms → reject directive (the
    //      anti-hallucination gate — a hallucinated npc_id surfaces as a hard
    //      fact the narrator obeys, NOT a silent accept). Known forms →
    //      upsert into presences with ttl = GRACE_RESET.
    //   2. Grace-decay: every existing presence NOT re-asserted this turn has
    //      its ttl decremented; it drops when ttl hits 0. This tolerates ONE
    //      missed extraction (the §11.51 Tracker under-emission failure mode)
    //      without vaporizing the barkeep mid-scene.
    // No node rooting (Option B — presence-implies-location; off-screen NPC
    // positions are the rumor/task engine's job). The presence applier ALWAYS
    // runs the decay pass when there are presence_cmds (even if all are
    // rejects) so the whitelist stays current. Note the borrow dance: the
    // registry lookup (immutable `s.npc_registry`) + the existing-presences
    // scan must complete before the mutable `s.presences` rebuild.
    // Gate: runs on EVERY turn that has assertions OR existing presences.
    // The decay-only path (assertions empty but presences non-empty) is the
    // load-bearing case: a turn where the Tracker emitted zero [PRESENCE]
    // brackets must still age the cast out, else NPCs freeze on-camera
    // forever once asserted. The early-out above already ensured we only
    // reach here when there's actual work to do.
    if !presence_cmds.is_empty() || !s.presences.is_empty() {
        // Snapshot the registry + existing presences out of the immutable
        // borrow before the mutable rebuild (mirrors the TRAVEL/RUMOR clone-
        // before-mutable-push pattern).
        let registry_entries: Vec<schema::NpcEntry> = s.npc_registry.entries.clone();
        let existing: Vec<schema::Presence> = s.presences.clone();

        // Resolve this turn's assertions: surface → (canonical id, name, stance).
        // Unknown surfaces collected for reject directives; known ones build
        // the asserted map (canonical_id → stance).
        let mut asserted: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut unknown_surfaces: Vec<String> = Vec::new();
        for cmd in &presence_cmds {
            if let bracket_parser::BracketCommand::Presence { npc_id, stance } = cmd {
                // Normalize the stance through the prose-cleanup contract
                // (free-text Tracker output carries the §11.41 repetition +
                // §11.29 whitespace risks — truncate_repetition is already
                // applied to the tracker's raw output upstream, but a defensive
                // whitespace-normalize here is cheap insurance).
                let stance_clean = stance.trim();
                // Resolve against the registry (id OR alias, case-insensitive).
                match registry_entries.iter().find(|e| e.matches(npc_id)) {
                    Some(entry) => {
                        asserted.insert(entry.id.clone(), stance_clean.to_string());
                    }
                    None => {
                        unknown_surfaces.push(npc_id.clone());
                    }
                }
            }
        }

        // Reject directives for unknown npc_ids (the anti-hallucination gate).
        // Format mirrors [TRAVEL]'s reject (lists valid options so the
        // narrator self-corrects next turn). Unknown surfaces are deduped +
        // joined so a single directive covers a multi-hallucination turn.
        if !unknown_surfaces.is_empty() {
            let valid_ids: Vec<&str> = registry_entries.iter().map(|e| e.id.as_str()).collect();
            let deduped: Vec<&str> = {
                let mut seen = std::collections::HashSet::new();
                unknown_surfaces
                    .iter()
                    .map(|s| s.as_str())
                    .filter(|s| seen.insert(*s))
                    .collect()
            };
            reject_directives.push(format!(
                "Presence not recorded — \"{}\" is not a known NPC. Known NPCs: {}. Re-assert presence with a valid id or alias next turn.",
                deduped.join("\", \""),
                if valid_ids.is_empty() { "(none registered)".to_string() } else { valid_ids.join(", ") }
            ));
            tracing::warn!(
                unknown = ?deduped,
                "[PRESENCE] rejected — unknown npc_id(s)"
            );
        }

        // Rebuild the presences Vec: re-asserted (reset ttl) + carried-over
        // (decrement ttl, drop at 0) + newly-asserted (fresh ttl). This is the
        // grace-decay pass. Order: existing first (preserves stable ordering
        // for the `present:` render line), then newly-asserted appended.
        if !asserted.is_empty() || !registry_entries.is_empty() {
            if undo_snapshot.is_none() {
                undo_snapshot = Some(s.clone());
            }
            let mut rebuilt: Vec<schema::Presence> = Vec::new();
            // Carry over existing: reset ttl if re-asserted, else decay.
            for p in &existing {
                if let Some(new_stance) = asserted.get(&p.npc_id) {
                    rebuilt.push(schema::Presence {
                        npc_id: p.npc_id.clone(),
                        name: p.name.clone(),
                        stance: new_stance.clone(),
                        ttl: schema::PRESENCE_GRACE_RESET,
                    });
                } else if p.ttl > 1 {
                    // Grace: decrement, keep. With PRESENCE_GRACE_RESET = 4 an
                    // NPC survives three missed re-assertions before dropping.
                    rebuilt.push(schema::Presence {
                        npc_id: p.npc_id.clone(),
                        name: p.name.clone(),
                        stance: p.stance.clone(),
                        ttl: p.ttl - 1,
                    });
                    tracing::debug!(
                        npc_id = %p.npc_id,
                        new_ttl = p.ttl - 1,
                        "[PRESENCE] grace-decay (not re-asserted this turn)"
                    );
                } else {
                    // ttl was 1 → drops to 0 → removed from the whitelist.
                    tracing::info!(
                        npc_id = %p.npc_id,
                        "[PRESENCE] dropped — grace expired (not re-asserted for {} turns",
                        schema::PRESENCE_GRACE_RESET
                    );
                }
                asserted.remove(&p.npc_id);
            }
            // Newly-asserted (not in existing): fresh presence at GRACE_RESET.
            for (npc_id, stance) in asserted {
                if let Some(entry) = registry_entries.iter().find(|e| e.id == npc_id) {
                    rebuilt.push(schema::Presence {
                        npc_id: entry.id.clone(),
                        name: entry.name.clone(),
                        stance,
                        ttl: schema::PRESENCE_GRACE_RESET,
                    });
                    tracing::info!(
                        npc_id = %entry.id,
                        "[PRESENCE] asserted (new on-camera)"
                    );
                }
            }
            s.presences = rebuilt;
            mutated = true;
        }
    }

        // [APPEARANCE] — dynamic appearance delta (Phase 4 Component 5,
        // 2026-08-04). Upserts one trait in PlayerState::current_appearance_deltas;
        // empty value clears the key. Mutates ONLY the per-run live layer —
        // the SavedPlayer identity baseline is never touched (it's the reusable
        // cross-card anchor). Last-wins per key within a single turn (a narrator
        // rarely emits two [APPEARANCE outfit=...] in one turn, but if it does
        // the second wins — matches the natural "what's true now" reading).
        // One undo snapshot for the batch (coalesced — mirrors every other
        // applier's single-snapshot-per-turn discipline).
        for cmd in &appearance_cmds {
            if let bracket_parser::BracketCommand::Appearance { key, value } = cmd {
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                if value.trim().is_empty() {
                    let existed = s.player_state.current_appearance_deltas.remove(key).is_some();
                    if existed {
                        mutated = true;
                        tracing::info!(key = %key, "[APPEARANCE] delta cleared");
                    }
                } else {
                    s.player_state
                        .current_appearance_deltas
                        .insert(key.clone(), value.clone());
                    mutated = true;
                    tracing::info!(key = %key, value = %value, "[APPEARANCE] delta set");
                }
            }
        }

        // [EQUIP] — worn equipment (2026-08-07). slot + layer select the write
        // target; item_name=None is the unequip form (clears that layer, then
        // drops the slot entirely if both layers end up empty so the equipment
        // map stays tight). One undo snapshot for the batch (coalesced). Last-
        // wins per (slot, layer) within a turn (two equips to the same slot —
        // the second replaces the first, matching the "what's true now" read).
        for cmd in &equip_cmds {
            if let bracket_parser::BracketCommand::Equip {
                slot,
                layer,
                item_name,
                item_stats,
                item_tags,
            } = cmd
            {
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                match item_name {
                    None => {
                        // Unequip: clear the named layer. If the slot ends up
                        // empty (both layers None), drop it from the map.
                        if let Some(layers) = s.player_state.equipment.get_mut(slot) {
                            let cleared = match layer {
                                equipment::ItemLayer::Outer => layers.outer.take().is_some(),
                                equipment::ItemLayer::Inner => layers.inner.take().is_some(),
                            };
                            if cleared {
                                mutated = true;
                                tracing::info!(slot = ?slot, layer = ?layer, "[EQUIP] unequipped");
                            }
                            if layers.is_empty() {
                                s.player_state.equipment.remove(slot);
                            }
                        }
                    }
                    Some(name) => {
                        let item = equipment::EquippedItem {
                            name: name.clone(),
                            stats: item_stats.clone(),
                            tags: item_tags.clone(),
                        };
                        let layers = s
                            .player_state
                            .equipment
                            .entry(*slot)
                            .or_insert_with(equipment::SlotLayers::default);
                        match layer {
                            equipment::ItemLayer::Outer => layers.outer = Some(item),
                            equipment::ItemLayer::Inner => layers.inner = Some(item),
                        }
                        mutated = true;
                        tracing::info!(slot = ?slot, layer = ?layer, name = %name, "[EQUIP] equipped");
                    }
                }
            }
        }

        // [BELT] — quick-access belt (fixed 4-slot rack). Add: upsert by name
        // (stack qty); on a NEW entry beyond BELT_MAX, FIFO-evict the oldest
        // first so the rack never exceeds 4. Remove: drop the whole stack.
        for cmd in &belt_cmds {
            if let bracket_parser::BracketCommand::Belt {
                item_name,
                qty,
                item_stats,
                remove,
                item_tags,
            } = cmd
            {
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                if *remove {
                    let existed = equipment::stack_remove(&mut s.player_state.belt, item_name, 0);
                    if existed {
                        mutated = true;
                        tracing::info!(name = %item_name, "[BELT] removed");
                    }
                } else {
                    let added = equipment::stack_upsert(
                        &mut s.player_state.belt,
                        equipment::StackItem {
                            name: item_name.clone(),
                            qty: *qty,
                            weight: 0.0, // belt items are weightless (quick-access)
                            stats: item_stats.clone(),
                            tags: item_tags.clone(),
                        },
                    );
                    // FIFO eviction: if this added a new entry past the cap,
                    // drop the oldest (index 0) so the rack stays at BELT_MAX.
                    while s.player_state.belt.len() > equipment::BELT_MAX {
                        let evicted = s.player_state.belt.remove(0);
                        tracing::info!(name = %evicted.name, "[BELT] FIFO-evicted (rack full)");
                    }
                    if added || *qty > 0 {
                        mutated = true;
                        tracing::info!(name = %item_name, qty, "[BELT] added");
                    }
                }
            }
        }

        // [PACK] — deep-storage pack (UNBOUNDED bagged inventory; the
        // encumbrance/weight system was PERMANENTLY REMOVED 2026-08-09). Add:
        // upsert by name (stack qty, take the heavier per-unit weight, union
        // tags). Remove: drop the whole stack. No capacity rejection — the pack
        // is infinite; `weight` survives only for the narrator-summary text
        // readout, enforcing nothing.
        for cmd in &pack_cmds {
            if let bracket_parser::BracketCommand::Pack {
                item_name,
                qty,
                weight,
                item_stats,
                remove,
                item_tags,
            } = cmd
            {
                if undo_snapshot.is_none() {
                    undo_snapshot = Some(s.clone());
                }
                if *remove {
                    let existed = equipment::stack_remove(&mut s.player_state.pack, item_name, 0);
                    if existed {
                        mutated = true;
                        tracing::info!(name = %item_name, "[PACK] removed");
                    }
                } else {
                    equipment::stack_upsert(
                        &mut s.player_state.pack,
                        equipment::StackItem {
                            name: item_name.clone(),
                            qty: *qty,
                            weight: *weight,
                            stats: item_stats.clone(),
                            tags: item_tags.clone(),
                        },
                    );
                    mutated = true;
                    tracing::info!(name = %item_name, qty, weight, "[PACK] added");
                }
            }
        }
    } // end schema lock

    // Push one undo snapshot for the whole bracket-command batch (mirrors the
    // single-snapshot-per-turn pattern in fable_send's schema-lock block).
    if let Some(snap) = undo_snapshot {
        push_fable_history_snapshot(state, snap).await;
    }

    (mutated, reject_directives)
}

/// `acquire_schema_engine_from_arcs` helper. The fable lease is still held
/// at this point (we're inside `fable_send`), so the schema acquire will
/// WAIT for `fable_send` to return + drop the lease before the schema engine
/// can spawn. That's correct: the tick is OFF-SCREEN simulation, decoupled
/// from the just-completed narrator turn — running it after the turn completes
/// (visible on the NEXT turn) is the right semantics, not a bug.
///
/// # Best-effort
///
/// Errors are logged + dropped: a failed tick must never block the gameplay
/// loop. The fail-queue contract still applies — a retryable failure
/// (parse/validation) lands in `failed_progression_queue` and is folded into
/// the next tick's prompt. Infrastructure failures (tokenize/prefill/decode)
/// are just logged.
async fn apply_time_command_and_maybe_tick(
    parsed: &bracket_parser::ParsedNarration,
    state: &tauri::State<'_, AppState>,
) {
    // 1. Extract the last `[TIME ...]` command (if any). Multiple in one turn
    //    is unusual but legal (the narrator might emit time mid-turn then
    //    again at the end); the LAST one is the most recent + authoritative.
    let last_time_cmd: Option<i64> = parsed
        .commands
        .iter()
        .rev()
        .find_map(|cmd| match cmd {
            bracket_parser::BracketCommand::Time { minutes, .. } => Some(*minutes),
            _ => None,
        });

    let Some(new_minutes) = last_time_cmd else {
        return; // no [TIME] this turn — clock unchanged, no tick.
    };

    // 2. Apply the clock advance under the schema lock. Monotonic guard:
    //    reject regressions (a buggy narrator going backward in time).
    //    Determine whether this is the first-ever [TIME] so we can stamp the
    //    baseline instead of firing.
    let (was_first_set, prev_minutes, schema_snapshot) = {
        let mut s = state.fable_schema.lock().await;
        let was_first = !s.world_clock.is_set();
        let prev = s.world_clock.current_minutes;
        if !was_first && new_minutes < prev {
            tracing::warn!(
                new_minutes,
                prev_minutes = prev,
                "ignoring [TIME] regression: clock only moves forward"
            );
            return;
        }
        // Snapshot for undo BEFORE the mutation. On the first-set the
        // baseline stamp below captures the same transition (clock was 0→N,
        // not a meaningful "undo" target); skip the snapshot to keep the
        // ring clean. The clone is dropped into the history push after we
        // release this lock.
        let undo_snapshot = if !was_first && new_minutes != prev {
            Some(s.clone())
        } else {
            None
        };
        s.world_clock.current_minutes = new_minutes;
        let live_snapshot = s.clone();
        if let Some(snap) = undo_snapshot {
            drop(s);
            push_fable_history_snapshot(&state, snap).await;
        }
        (was_first, prev, live_snapshot)
    };
    tracing::info!(
        new_minutes,
        prev_minutes,
        first_set = was_first_set,
        "clock advanced via [TIME] bracket"
    );

    // 3. First-call baseline: stamp `last_tick_minutes` and bail (no fire).
    //    A campaign doesn't simulate a day it hasn't established yet.
    if was_first_set {
        let mut s = state.fable_schema.lock().await;
        let snap = s.clone();
        s.world_clock.last_tick_minutes = new_minutes;
        drop(s);
        push_fable_history_snapshot(&state, snap).await;
        tracing::info!(baseline = new_minutes, "clock baseline stamped (no tick this turn)");
        return;
    }

    // 4. Tick gate: has enough in-world time elapsed since the last tick?
    //    The interval is now ScenePacing-driven (Fable Seam #4 expansion,
    //    2026-07-27): Combat → 0 (never fire mid-fight), Downtime → 1h (world
    //    moves fast while you rest), Exploration → 4h (balanced). The legacy
    //    const WORLD_PROGRESSION_INTERVAL_HOURS (24h) is retained as the
    //    fallback for any future "fixed-interval" mode but no longer used by
    //    the live gate.
    let interval_hours = schema_snapshot.scene_pacing.mode.progression_interval_hours();
    if interval_hours == 0 {
        tracing::debug!(
            mode = ?schema_snapshot.scene_pacing.mode,
            "world progression tick skipped: scene mode suspends background sim (combat)"
        );
        return;
    }
    let interval_minutes = (interval_hours as i64) * 60;
    let elapsed = schema_snapshot.world_clock.minutes_since_last_tick();
    if elapsed < interval_minutes {
        tracing::debug!(
            elapsed_hours = elapsed / 60,
            interval_hours,
            mode = ?schema_snapshot.scene_pacing.mode,
            "clock tick gate not met (no fire)"
        );
        return;
    }

    // 5. Gate met — fire the off-screen simulation pass. Acquire the schema
    //    engine + the Schema lease (will wait for the fable lease to drop on
    //    fable_send return). Drain the failed-progression queue first.
    tracing::info!(
        elapsed_hours = elapsed / 60,
        interval_hours,
        mode = ?schema_snapshot.scene_pacing.mode,
        "clock tick gate met — firing world progression pass"
    );

    // 5a. Phase 3 Slice 4 + Slice 6 wiring (2026-07-28): BEFORE the LLM
    //     progression pass, drop expired status tags + resolve due off-screen
    //     tasks. Doing these BEFORE the schema engine fires means the model
    //     sees the already-resolved state in the snapshot it diffs against.
    //     Both are pure Rust — no LLM cost. Snapshot for undo once if either
    //     mutates (same single-snapshot-per-tick pattern as the rest).
    let now_minutes = schema_snapshot.world_clock.current_minutes;
    let mut tick_mutated = false;
    let mut tick_snapshot: Option<schema::WorldSchema> = None;
    let mut tick_directives: Vec<String> = Vec::new();
    {
        let mut s = state.fable_schema.lock().await;

        // Slice 4: expire status tags whose WorldClock expiry has passed.
        // Permanent tags (expires_at == 0) survive. Returns the dropped count.
        let dropped_tags = consequence::expire_tags(&mut s.status_tags, now_minutes);
        if dropped_tags > 0 {
            tick_snapshot = Some(s.clone());
            tick_mutated = true;
            tracing::info!(
                dropped = dropped_tags,
                now_minutes,
                "[tick] status tags expired"
            );
        }

        // Slice 6: resolve due off-screen tasks. Each resolution produces a
        // directive the next fable_send surfaces to the narrator. Resolved
        // tasks are removed from the queue after directive emission (the
        // mechanic is one-shot per task; we don't re-roll).
        if !s.offscreen_tasks.is_empty() {
            let resolutions =
                offscreen_task::resolve_expired_tasks(&s.offscreen_tasks, now_minutes);
            if !resolutions.is_empty() {
                if tick_snapshot.is_none() {
                    tick_snapshot = Some(s.clone());
                }
                tick_mutated = true;
                let resolved_count = resolutions.len();
                for r in &resolutions {
                    tick_directives.push(r.directive.clone());
                    tracing::info!(
                        npc_id = %r.npc_id,
                        severity = ?r.severity,
                        "[tick] off-screen task resolved"
                    );
                }
                // Drop resolved tasks from the queue. (resolve_expired_tasks
                // skips already-resolved + not-yet-due, so every task it
                // returned is one we should remove.)
                let resolved_ids: std::collections::HashSet<(String, String, i64)> = resolutions
                    .iter()
                    .map(|r| (r.npc_id.clone(), r.description.clone(), r.dc as i64))
                    .collect();
                s.offscreen_tasks.retain(|t| {
                    !resolved_ids.contains(&(t.npc_id.clone(), t.description.clone(), t.difficulty.dc() as i64))
                });
                tracing::info!(
                    resolved = resolved_count,
                    remaining = s.offscreen_tasks.len(),
                    "[tick] off-screen tasks drained"
                );
            }
        }

        // Component 2 (2026-07-28): weather drift. Pure Rust, deterministic
        // (seeded by clock + current condition — mirrors offscreen_task::
        // resolve_task). The persistence DC scales with how long the current
        // condition has held — long-running weather is more likely to shift.
        // On drift, pick a new condition from the generic pool (never the
        // same one). Skipped if weather is unset (no [WEATHER] yet — dormant,
        // like world_clock before the first [TIME]). Combat ticks are
        // suspended upstream by progression_interval_hours()==0, so weather
        // is stable mid-fight unless the tracker forces [WEATHER].
        if s.weather.is_set() {
            if let Some(new) = weather::drift_weather(&s.weather, now_minutes) {
                if tick_snapshot.is_none() {
                    tick_snapshot = Some(s.clone());
                }
                tick_mutated = true;
                tick_directives.push(format!(
                    "Weather shift — {} gives way to {}. Narrate the changing \
                     conditions (sensory detail, NPCs reacting, visibility and \
                     footing). This is a hard fact; do not contradict it.",
                    s.weather.condition, new.condition
                ));
                tracing::info!(
                    from = %s.weather.condition,
                    to = %new.condition,
                    "[tick] weather drifted"
                );
                s.weather = new;
            }
        }
        // Component 4 (2026-07-28): rumor propagation between connected nodes.
        // Each rumor attempts to spread from its known_nodes to their adjacent
        // unknown neighbors via a per-edge d20 roll against an age-decayed DC
        // (fresh rumors spread fast, stale news slow). Anti-saturation cap:
        // at most NEW_NODES_PER_TICK_CAP new nodes per rumor per tick (the
        // load-bearing anti-bloat guard). Pure Rust, seeded RNG — mirrors
        // weather::drift_weather's shape exactly (snapshot-once, directive-
        // push, mutate, log). Skipped when no rumors OR no graph (dormant).
        // Combat ticks are suspended upstream by progression_interval_hours
        // ()==0, so rumors are stable mid-fight unless the tracker forces
        // [RUMOR].
        if !s.rumors.is_empty() && s.travel_graph.is_set() {
            let (new_rumors, rumor_dirs) =
                rumor::propagate_rumors(&s.rumors, &s.travel_graph, now_minutes);
            if new_rumors != s.rumors {
                if tick_snapshot.is_none() {
                    tick_snapshot = Some(s.clone());
                }
                tick_mutated = true;
                tick_directives.extend(rumor_dirs.iter().cloned());
                let spread_count = rumor_dirs.len();
                tracing::info!(
                    spread_events = spread_count,
                    "[tick] rumors propagated to adjacent nodes"
                );
                s.rumors = new_rumors;
            }
        }
    }

    // Surface tick directives to the next fable_send (consumed in the
    // <directives> block alongside combat lethality + skill checks).
    if !tick_directives.is_empty() {
        let mut td = state.pending_tick_directives.lock().await;
        td.extend(tick_directives);
    }

    // Push one undo snapshot for the tick's Rust mutations (separate from the
    // LLM progression delta's snapshot at step 9 below — different mutation
    // source, both restorable via fable_rollback).
    if let Some(snap) = tick_snapshot {
        push_fable_history_snapshot(&state, snap).await;
    }
    let _ = tick_mutated; // (kept for clarity; the snapshot push above is the real effect)
    let deferred = {
        let mut q = state.failed_progression_queue.lock().await;
        std::mem::take(&mut *q)
    };
    if !deferred.is_empty() {
        tracing::info!(deferred = deferred.len(), "progression tick includes deferred re-attempts");
    }

    let context_swap = state.context_swap.clone();
    let schema_engine_slot = Arc::clone(&state.schema_engine);
    let local_model_lock = Arc::clone(&state.local_model_lock);
    let (schema_engine, _schema_lease, _model_guard) = match acquire_schema_engine_from_arcs(
        context_swap,
        schema_engine_slot,
        local_model_lock,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "world progression: could not acquire schema engine; tick skipped");
            return;
        }
    };

    // 6. Post the progression request + await the reply off the tokio worker
    //    (the schema thread is a bare std::thread; its mpsc::Receiver blocks).
    let reply_rx = match schema_engine.request_world_progression(
        &schema_snapshot,
        interval_hours,
        deferred,
    ) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "world progression request failed; tick skipped");
            return;
        }
    };
    let reply = match tokio::task::spawn_blocking(move || reply_rx.recv()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e}"), "world progression reply channel closed");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e}"), "world progression reply join failed");
            return;
        }
    };

    // 7. Fail-proof contract: enqueue retryable failures for the next tick.
    if let Some(failed) = reply.failed_attempt.clone() {
        enqueue_failed_progression(state, failed).await;
    }
    if !reply.error.is_empty() {
        tracing::warn!(
            error = %reply.error,
            "world progression pass failed (queued for retry on next tick)"
        );
        return;
    }

    // 8. Apply the resulting delta to `fable_schema`. The next narrator turn
    //    sees the moved world via the existing `<world_state>` injection.
    if let Some(mut delta) = reply.delta {
        if delta.has_changes() {
            let mut s = state.fable_schema.lock().await;
            let snap = s.clone();
            // Phase 3 Slice 5 wiring (2026-07-28): same silent-strip as the
            // translation path. The world-progression LLM pass can also
            // attempt rel.* writes; they're gated the same way.
            let stripped = strip_invalid_relationship_writes(&mut delta, &mut s);
            if !stripped.is_empty() {
                tracing::info!(
                    count = stripped.len(),
                    keys = ?stripped,
                    "[rel] relationship writes stripped from world-progression delta"
                );
            }
            s.apply_delta(delta.clone());
            drop(s);
            push_fable_history_snapshot(&state, snap).await;
            tracing::info!(
                keys_changed = delta
                    .entities
                    .as_ref()
                    .map(|m| m.len())
                    .unwrap_or(0),
                "world progression delta applied to game_schema"
            );
        } else {
            tracing::debug!("world progression pass emitted empty delta (nothing moved)");
        }
    }

    // 9. Stamp the tick baseline so the next fire waits for another full
    //    interval. Done LAST so a mid-application crash doesn't advance the
    //    baseline without recording the result (if apply_delta failed above,
    //    we return early before this stamp).
    {
        let mut s = state.fable_schema.lock().await;
        let snap = s.clone();
        s.world_clock.last_tick_minutes = s.world_clock.current_minutes;
        drop(s);
        push_fable_history_snapshot(&state, snap).await;
    }
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

/// The effective n_ctx for the LOCAL CHAT context.
///
/// **Wupi chat is LOCAL-ONLY (2026-08-08 override):** the chat backend ALWAYS
/// runs at `CTX_LOCAL_WITH_API` (2048). The 4096 context is retired for chat
/// — it was for narrative, which the local model no longer does. Both params
/// are now read-ignored; the function always returns 2048.
///
/// `#[cfg(test)]` — all live call sites were replaced with the constant
/// directly (boot, chat_send, api_connect/disconnect). This fn survives only
/// to pin the contract in the test suite (`effective_local_ctx_always_
/// returns_with_api_constant`).
#[cfg(test)]
fn effective_local_ctx(_source: api::ModelSource, _settings: &WupiSettings) -> u32 {
    settings::CTX_LOCAL_WITH_API
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

    // Wupi chat is LOCAL-ONLY (2026-08-08 override): the chat backend ALWAYS
    // runs at 2048 regardless of API connection state. There is NO context
    // shrink on connect anymore — the backend stays resident at 2048. This
    // whole block used to tear down + re-spawn the backend at a different ctx;
    // now it's a no-op (the chat backend is untouched). api_connect is pure
    // bookkeeping: flip model_source to Api + persist (done above).
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

    // Wupi chat is LOCAL-ONLY (2026-08-08 override): the chat backend ALWAYS
    // runs at 2048 regardless of API connection state. There is NO context
    // restore on disconnect anymore — the backend was already at 2048 under
    // the API and stays at 2048 after disconnect. This whole block used to
    // tear down + re-spawn at 4096; now it's a no-op. api_disconnect is pure
    // bookkeeping: flip model_source to Local + persist (done above).

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
    /// The polymorphic SIM Wizard discriminator (2026-08-13): "npc" |
    /// "scenario" | "world" | None. Surfaced so a future picker can badge/group
    /// cards by type; `<card_type>` stays "roleplay" for all playable cards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtype: Option<String>,
    setting_preview: String,
    tone: Option<String>,
    /// First ~240 chars of `<scenario><opening_scene>`: the launcher card
    /// uses this as the evocative "what's this about" blurb below the title.
    /// None when the card doesn't declare one.
    opening_scene_preview: Option<String>,
    /// Declared player name. The launcher shows this so the player
    /// knows whose shoes they're stepping into before they start.
    player_name: Option<String>,
    /// Whether the player has any saves for this card (autosave counts).
    /// Lets the launcher show Continue vs New Game intelligently. Best-effort:
    /// a directory-read error degrades to false (the user can still start).
    has_saves: bool,
    /// Relative portrait filename within the card folder (e.g.
    /// "portrait.png"). None when no portrait sibling exists. Kept for
    /// debugging/identity; the frontend renders via `portrait_url` below.
    portrait: Option<String>,
    /// Whether a portrait file exists for this card (best-effort:
    /// a directory-read error degrades to false). Lets the launcher's mini-
    /// card render the portrait vs the placeholder without a second IPC.
    has_portrait: bool,
    /// Absolute filesystem path to the card's portrait, ready for the
    /// frontend's `convertFileSrc`. None when no portrait sibling exists.
    /// Built alongside `portrait` so the mini-card + modal render with zero
    /// extra IPCs (the assetProtocol scope includes apps/fable/cards/**).
    portrait_url: Option<String>,
}

/// Enumerate every `.sim` file in `apps/fable/cards/` and return parsed
/// metadata. The card-picker UI's data source. Returns an empty Vec when no
/// cards dir exists (the common case until cards are authored or imported):
/// graceful, not an error.
///
/// **2026-08-01 folder layout:** cards live in per-card folders
/// (`cards/<id>/<id>.sim`); `iter_card_sim_paths` is the single walker.
#[tauri::command]
fn fable_cards_list(app: tauri::AppHandle) -> Result<Vec<FableCardMeta>, String> {
    let dir = resolve_fable_cards_dir(&app);
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for path in iter_card_sim_paths(&dir) {
        let card = sim_card::load_or_fallback(&path);
        // Skip fallback stubs (a malformed file produced the fallback). The
        // id sentinel is the signal: see sim_card::FALLBACK_ID.
        if card.id == "__wupi_fallback__" {
            tracing::warn!(path = %path.display(), "skipping malformed game card");
            continue;
        }
        // Only list roleplay cards in this registry: the system card
        // (wupi.sim) lives in `data/`, not `apps/fable/cards/`, so this is
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
        let mut meta = card_to_meta(&card, has_saves);
        // The launcher blurb: the in-file `<intro>` sibling (canonical,
        // 2026-08-13) preferred, falling back to a legacy `.intro` sibling FILE
        // for cards authored before the in-file move. Capped to 240 chars.
        meta.opening_scene_preview = preview_from_intro(&card.intro)
            .or_else(|| intro_preview_for(&dir, &card.id));
        // The portrait sibling (`portrait.png`/`.jpg`) drives the launcher's
        // mini-card + modal portrait (2026-08-05). `portrait` is the relative
        // name; `portrait_url` is the absolute path for convertFileSrc.
        meta.portrait = portrait_path_for(&dir, &card.id);
        meta.has_portrait = meta.portrait.is_some();
        if let Some(ref fname) = meta.portrait {
            meta.portrait_url = Some(resolve_card_dir(&dir, &card.id).join(fname).to_string_lossy().into_owned());
        }
        out.push(meta);
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
    // opening_scene_preview (the launcher blurb) now reads the sibling `.intro`
    // file (capped at 240 chars). The intro moved out of the cached `<sim_card>`
    // 2026-08-05 — `card.opening_scene` no longer exists. The `.intro` is the
    // evocative beat; if absent, the launcher falls back to the setting blurb
    // (handled client-side via `setting_preview || opening_scene_preview`).
    // NOTE: card_to_meta doesn't have the cards dir in scope, so the `.intro`
    // read happens at the fable_cards_list call site (which does) via
    // `intro_preview_for`. This fn leaves opening_scene_preview = None; the
    // caller patches it in.
    FableCardMeta {
        id: card.id.clone(),
        name: card.name.clone(),
        card_type: card.card_type.clone(),
        subtype: card.subtype.clone(),
        setting_preview,
        tone: card.tone.clone(),
        opening_scene_preview: None,
        player_name: card.player_name.clone(),
        has_saves,
        // Portrait sibling is detected at the fable_cards_list call site
        // (which has the cards dir in scope) via portrait_path_for; this fn
        // leaves portrait = None / has_portrait = false (same pattern as
        // opening_scene_preview above).
        portrait: None,
        has_portrait: false,
        portrait_url: None,
    }
}

/// Read a card's sibling `.intro` file (capped at 240 chars) for the launcher
/// preview blurb. None when the card has no `.intro` (the common case). Best-
/// effort: a read error degrades to None.
/// Cap an in-file `<intro>` block (from the parsed `SimCard.intro` field) to
/// the 240-char launcher-blurb preview. None when the card carries no intro.
/// This is the canonical source (2026-08-13); `intro_preview_for` (the legacy
/// `.intro` FILE read) is the back-compat fallback.
fn preview_from_intro(intro: &str) -> Option<String> {
    let t = intro.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.chars().take(240).collect())
    }
}

fn intro_preview_for(cards_root: &std::path::Path, card_id: &str) -> Option<String> {
    let path = resolve_card_file(cards_root, card_id, "intro");
    match std::fs::read_to_string(&path) {
        Ok(t) => {
            let t = t.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.chars().take(240).collect::<String>())
            }
        }
        Err(_) => None,
    }
}

/// Detect a card's portrait sibling (`cards/<id>/portrait.png` or `.jpg`),
/// returning the relative filename when one exists. None when no portrait
/// sibling is present (the common case). Best-effort: a stat error degrades
/// to None. Mirrors the player.rs portrait convention so the launcher's mini-
/// card + modal render the portrait vs the placeholder without a second IPC.
/// The portrait filename is fixed `portrait.<ext>` (NOT `<card_id>.<ext>` —
/// see `fable_card_portrait_write`), so this builds the path directly rather
/// than going through `resolve_card_file` (which appends `<card_id>.<ext>`).
fn portrait_path_for(cards_root: &std::path::Path, card_id: &str) -> Option<String> {
    let card_dir = resolve_card_dir(cards_root, card_id);
    for ext in ["png", "jpg", "jpeg"] {
        let path = card_dir.join(format!("portrait.{ext}"));
        if path.is_file() {
            return Some(format!("portrait.{ext}"));
        }
    }
    None
}

/// Read a card's full `.intro` text (the one-shot first narrator beat). None
/// when the card has no `.intro`. The intro is read ONCE at game start +
/// surfaced on `FableLoadResult.intro` (NEVER injected into the cached system
/// prompt — it's a single-turn seed). Best-effort: a read error degrades to
/// None (the game starts without an opening beat).
fn load_card_intro(cards_root: &std::path::Path, card_id: &str) -> Option<String> {
    let path = resolve_card_file(cards_root, card_id, "intro");
    match std::fs::read_to_string(&path) {
        Ok(t) => {
            let t = t.trim();
            if t.is_empty() { None } else { Some(t.to_string()) }
        }
        Err(_) => None,
    }
}

/// Slugify a card name into a filesystem-safe stem (lowercase, non-alphanumerics
/// → dashes, leading/trailing dashes trimmed). Mirrors `tools::sanitize_stem`
/// but inlined here so the IPC layer doesn't depend on the agent-tool module.
/// The stem is the `<stem>.sim` filename AND the card's memory/save partition
/// key, so it must be stable + unique across cards.
fn slugify_card_stem(name: &str) -> Option<String> {
    let stem: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_owned();
    if stem.is_empty() { None } else { Some(stem) }
}

/// Validate a `.sim` card XML string against the real parser. Returns the
/// parsed card's metadata on success, or an error string on failure. This is
/// the single validation authority for the Creator's save path AND any
/// import-existing path — no client-side XML parser is needed, the Rust parser
/// IS the validator (catches everything: malformed XML, missing `<sim_card>`
/// root, missing required tags). The error string surfaces to the UI toast.
///
/// Added 2026-08-01 for the New Game Creator flow. Mirrors `parse_from_xml_str`
/// but returns `FableCardMeta` (the shape the frontend already consumes from
/// `fable_cards_list`) so the Creator can hand off straight to `fable_start`.
#[tauri::command]
fn fable_validate_card_xml(xml: String) -> Result<FableCardMeta, String> {
    let card = sim_card::parse_from_xml_str(&xml)
        .map_err(|e| format!("Invalid card format: {e}"))?;
    Ok(card_to_meta(&card, false))
}

/// Write a Creator-authored card to `apps/fable/cards/<stem>.sim`. Validates
/// the XML via the real parser first (rejects malformed cards before they hit
/// disk — the load-bearing safety gate), then writes + re-reads to confirm.
/// Returns the freshly-written card's metadata so the Creator can start a game
/// from it immediately. Reuses the path-sanitization discipline from the
/// `create_sim_card` agent tool but as a direct IPC (no agent loop).
///
/// `stem` is the filename (the `.sim` extension is appended; if the caller
/// includes it, it's stripped first). Slugified to filesystem-safe form. A
/// stem collision overwrites the existing card (the user is editing their own
/// authored card — same contract as the agent tool).
#[tauri::command]
fn fable_write_card(
    stem: String,
    xml: String,
    app: tauri::AppHandle,
) -> Result<FableCardMeta, String> {
    // 1. Validate the XML through the real parser BEFORE any disk touch.
    //    This is the load-bearing gate: a malformed card never lands on disk.
    //    The parsed card is intentionally discarded — the authoritative return
    //    value is the disk re-read below (`reloaded`), which catches any
    //    encoding quirk the in-memory parse might miss.
    let _validated = sim_card::parse_from_xml_str(&xml)
        .map_err(|e| format!("Invalid card format: {e}"))?;
    if xml.len() > 100_000 {
        return Err("Card exceeds 100 KB cap".into());
    }

    // 2. Slugify the stem + resolve the cards dir.
    let stem = slugify_card_stem(&stem)
        .ok_or_else(|| "card filename empty after sanitization".to_string())?;
    // Strip a trailing .sim if the caller included it (ergonomic; the canonical
    // form appends it below).
    let stem = stem.strip_suffix(".sim").unwrap_or(&stem).to_owned();
    let dir = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    // **2026-08-01 folder reorg:** the card lives in a per-card folder
    // `cards/<stem>/<stem>.sim` (sibling to `<stem>.codex`, world.json, etc.).
    let card_dir = resolve_card_dir(&dir, &stem);
    let path = resolve_card_file(&dir, &stem, "sim");

    // 3. Atomic write + re-read to confirm round-trip integrity (the on-disk
    //    parser path, not the in-memory parse — catches any encoding quirk).
    //    Atomic (temp+fsync+rename) so a crash mid-write never truncates an
    //    existing card — same guarantee the schema/session saves have.
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("mkdir card folder: {e}"))?;
    write_atomic(&path, xml.as_bytes()).map_err(|e| format!("write card: {e}"))?;
    let reloaded = sim_card::load_or_fallback(&path);
    if reloaded.id == "__wupi_fallback__" {
        // The write succeeded but the re-read produced the fallback stub —
        // the card is malformed in a way the in-memory parse missed (shouldn't
        // happen, but defense-in-depth). Remove the bad file so we don't leave
        // a broken card in the picker. (Sentinel literal matches the two other
        // check sites at lib.rs:2124 + :5943.)
        let _ = std::fs::remove_file(&path);
        return Err("card validated in memory but failed re-read after write".into());
    }
    tracing::info!(path = %path.display(), card_id = %reloaded.id, "Creator card written");
    // Auto-create the card's in-folder `Launch <Name>.lnk` (best-effort: a
    // shortcut failure must NEVER fail the card write itself — the card is the
    // source of truth, the launcher is a convenience). In-folder only so it's
    // self-contained + auto-reaped on delete; the desktop copy is an explicit
    // opt-in via the create_card_shortcut IPC. Idempotent: re-writing a card
    // refreshes the .lnk (the label/icon re-derive from the current .sim +
    // portrait) so a rename/portrait change keeps the shortcut in sync.
    if let Err(e) = build_card_shortcut(&app, &reloaded.id, false) {
        tracing::warn!(card_id = %reloaded.id, err = %e, "auto card shortcut creation failed (card write still succeeded)");
    }
    Ok(card_to_meta(&reloaded, false))
}

/// Atomic file write (temp + fsync + rename). A crash mid-write leaves the
/// destination at its prior complete state, never a truncated middle. Shared
/// by the card/codex writers + the raw-file editor's save path. The temp file
/// is a sibling `.<name>.tmp` in the same dir so `rename` is atomic on Windows
/// (`MOVEFILE_REPLACE_EXISTING`). Mirrors `schema::atomic_write_text` /
/// `session::Conversation::save` / the `file_write` agent tool.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from("wupi.tmp"));
    let mut tmp_name = file_name;
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);
    let _ = std::fs::remove_file(&tmp_path); // clear a stale temp from a prior crash
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        std::io::Write::write_all(&mut file, bytes)?;
        std::io::Write::flush(&mut file)?;
        let _ = file.sync_all();
    }
    std::fs::rename(&tmp_path, path)
}

/// Transient starting gameplay conditions threaded from the Player Wizard
/// draft into the per-run `PlayerState` at game attach (2026-08-13). NOT
/// persisted on the `SavedPlayer` identity (the §6C identity-only lock is
/// preserved) — held only long enough to seed the new run. Built by the
/// frontend from the draft's optional `wealth` / `reputation` / `fame`
/// fields (leading-integer parse). `Option<T>` IPC args are optional in
/// Tauri, so resume/legacy invokes that omit it default to `None`.
#[derive(Debug, Clone, serde::Deserialize, Default)]
pub struct PlayerStartingConditions {
    #[serde(default)]
    pub wealth: Option<u32>,
    #[serde(default)]
    pub reputation: Option<i32>,
}

/// Start a game: load the roleplay card, spawn the FableEngine (loads
/// WUPI.gguf as its own isolated context), swap `active_card_id` to the
/// card's id, and load the initial session/schema state. The
/// `pre_fable_card_id` is saved so `fable_end` can restore it.
///
/// **Save loading (v0.6.0+):** when `save_id` is supplied, the session +
/// schema are loaded from that named save slot (under
/// `apps/fable/cards/<card_id>/saves/<save_id>.json`, §6B) instead of the
/// card's default resume point. This is the "Load Game" path. When `save_id` is
/// None, the card's last auto-persisted session/schema is loaded (the
/// "Continue" path) — same as before v0.6.0. Pass `fresh = true` to
/// explicitly start a brand-new run (clears any prior state); this is the
/// "New Game" path.
#[tauri::command]
async fn fable_start(
    card_id: String,
    save_id: Option<String>,
    fresh: Option<bool>,
    player_id: Option<String>,
    player_starting_conditions: Option<PlayerStartingConditions>,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<FableLoadResult, String> {
    tracing::info!(card_id = %card_id, ?save_id, ?fresh, ?player_id, "fable_start: spawning FableEngine");
    require_api_for_fable(&state)?;
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
            .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
        find_card_by_id(&dir, &card_id)?
    };

    // 3. Resolve the model path (WUPI.gguf: same file the chat engine uses,
    //    freshly leaked as the FableEngine's own &'static ref).
    let model_path = resolve_model_path(&app)
        .ok_or_else(|| "no WUPI.gguf found: cannot start game".to_string())?;

    // 4. Hand off to the shared enter helper.
    enter_fable_session(card, save_id, fresh.unwrap_or(false), player_id, player_starting_conditions, model_path, card_id_arg, &app, &state).await
}

/// The cold-start anchor bootstrap (2026-08-10): derive starting clock + weather
/// (+ opening location) from a card's `.intro` text via ONE schema-engine pass,
/// for cards that ship no `<start>` block. The cold-start hole: a dormant
/// clock/weather renders no `clock:`/`weather:` line in `<world_state>` → the
/// tracker has nothing to maintain → `[TIME]`/`[WEATHER]` never fire. This
/// reads the intro (the one-shot first narrator beat, normally UI-only) + asks
/// the model to extract the implied time/weather/location, returning anchors
/// the caller seeds directly into the dormant fields (bypassing `apply_delta`,
/// which is test-pinned to never touch clock/weather — mirrors the `<start>`
/// block seed).
///
/// Single schema-engine pass (same lease + spawn pattern, same best-effort
/// contract). Single raw generation — NO 3-pass repair loop
/// (the bootstrap is best-effort; a failure returns `None` + the caller's
/// sensible-defaults fallback takes over). Runs at `enter_fable_session` time
/// (NOT inside `fable_send`): the schema engine's VRAM lease conflicts with the
/// Fable lease held mid-turn, so launch-time is the only safe window. Costs one
/// local decode (~1-2s) when entering a game with a dormant clock/weather.
///
/// Returns `None` on any failure (schema engine unavailable, decode error,
/// unparseable JSON). The caller falls through to sensible defaults — never
/// fatal to game entry.
async fn bootstrap_anchors_from_intro(
    state: &tauri::State<'_, AppState>,
    intro_text: &str,
    setting: Option<&str>,
    tone: Option<&str>,
    player_name: Option<&str>,
) -> Option<schema::BootstrapAnchors> {
    // Acquire the schema lease + local-model turn lock. Hold both for the
    // single decode; dropping releases.
    let (schema_engine, _schema_lease, _model_guard) = acquire_schema_engine(state).await.ok()?;
    let prompt = fable_command::render_bootstrap_prompt(intro_text, setting, tone, player_name);
    let reply_rx = schema_engine.request_bootstrap(prompt).ok()?;
    let reply = tokio::task::spawn_blocking(move || reply_rx.recv())
        .await
        .map_err(|e| tracing::warn!(error = %format!("{e}"), "bootstrap reply join failed"))
        .ok()?
        .map_err(|e| tracing::warn!(error = %format!("{e}"), "bootstrap reply channel closed"))
        .ok()?;
    if !reply.error.is_empty() {
        tracing::warn!(error = %reply.error, "bootstrap generation failed; falling back to defaults");
        return None;
    }
    match schema::BootstrapAnchors::from_model_output(&reply.raw_output) {
        Ok(anchors) => {
            tracing::info!(
                has_time = anchors.time_minutes.is_some(),
                has_weather = anchors.weather.is_some(),
                has_location = anchors.location.is_some(),
                "fable_start: derived cold-start anchors from intro"
            );
            Some(anchors)
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e}"),
                raw_len = reply.raw_output.len(),
                "bootstrap JSON parse failed; falling back to defaults"
            );
            None
        }
    }
}

/// The shared "stash model path + swap id + load state + seat card" tail of
/// starting a game. `card` is already resolved (loaded from disk for
/// `fable_start` or `fable_load_save`).
/// `card_id_for_meta` is the id to report in the returned meta (identical to
/// card.id but kept explicit so logs are unambiguous).
async fn enter_fable_session(
    mut card: sim_card::SimCard,
    save_id: Option<String>,
    fresh: bool,
    player_id: Option<String>,
    player_starting_conditions: Option<PlayerStartingConditions>,
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

    // Per-card codex seeding (Phase D). The card's authored `.codex`
    // (`cards/<id>/<id>.codex`) is reconciled into the card's OWN memory
    // partition (keyed by `card.id`), which `search_fable_visible` already
    // queries as `active_card_id`. Best-effort + detached: a seed failure
    // never blocks game entry. Idempotent (title-keyed, hash-detected), so a
    // re-entry with an unchanged `.codex` is a no-op; edits propagate on the
    // next `fable_start`. Most cards ship without a `.codex` → the path
    // doesn't exist → nothing to seed.
    if let Some(memory_engine) = state.memory.get() {
        let me = Arc::clone(memory_engine);
        let cards_root = resolve_fable_cards_dir(app);
        let card_id_owned = card.id.clone();
        tokio::spawn(async move {
            if let Some(root) = cards_root {
                let path = resolve_card_file(&root, &card_id_owned, "codex");
                if path.exists() {
                    match codex::seed_fable_card_codex(&me, &path, &card_id_owned).await {
                        Ok(r) => tracing::info!(
                            card_id = %card_id_owned,
                            seeded = r.seeded,
                            updated = r.updated,
                            purged = r.purged,
                            unchanged = r.unchanged,
                            "per-card codex seeded"
                        ),
                        Err(e) => tracing::warn!(
                            card_id = %card_id_owned,
                            error = %format!("{e:#}"),
                            "per-card codex seed failed"
                        ),
                    }
                }
            }
        });
    }

    // Resolve initial state. Priority: explicit save_id → fresh → fallback.
    let fable_root = resolve_apps_dir(app).join("fable");
    let (mut prior_schema, prior_session, resumed_save_label) = if let Some(sid) = save_id.as_deref() {
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
    // Saved Player attachment (2026-08-02): when a saved player id was
    // passed (the New Game flow's Pair 2), load it + override the card's
    // `player_name` anchor with the saved player's name. The narrator
    // reads `card.player_name` as the identity anchor (the `<active_
    // reality>` block), so this is the load-bearing identity injection.
    // The prose (description/appearance/personality/accessories) lands as
    // `player.*` schema entities the narrator + retrieval can read —
    // identity, not gameplay state (no body/wealth mutation). Runs for
    // ALL three branches (fresh/resume/save) so attaching a player works
    // regardless of which state-resolution path fired; a None player_id
    // is a complete no-op (the card's own player_name stands).
    if let Some(pid) = player_id.as_deref() {
        let players_root = resolve_fable_players_dir(app);
        let json_path = players_root.join(pid).join(format!("{pid}.json"));
        if let Some(sp) = load_player_at(&json_path) {
            // Override the card's player_name anchor (the load-bearing
            // identity swap). Only when the saved player has a real name.
            let name = sp.name.trim();
            if !name.is_empty() {
                card.player_name = Some(name.to_owned());
            }
            // Stash the prose as schema entities. These read as the
            // player's authored identity — the narrator + retrieval see
            // them as ground truth about WHO the player is, decoupled
            // from the card's world/setting (which stays the card's).
            // (Values are `Value::String` — these are flat prose fields, not
            // structured data; the widened entity type keeps them as strings.)
            if let Some(d) = sp.description.as_deref().filter(|s| !s.trim().is_empty()) {
                prior_schema.entities.insert(
                    "player.description".into(),
                    serde_json::Value::String(d.trim().to_owned()),
                );
            }
            if let Some(a) = sp.appearance.as_deref().filter(|s| !s.trim().is_empty()) {
                prior_schema.entities.insert(
                    "player.appearance".into(),
                    serde_json::Value::String(a.trim().to_owned()),
                );
            }
            if let Some(p) = sp.personality.as_deref().filter(|s| !s.trim().is_empty()) {
                prior_schema.entities.insert(
                    "player.personality".into(),
                    serde_json::Value::String(p.trim().to_owned()),
                );
            }
            if let Some(ac) = sp.accessories.as_deref().filter(|s| !s.trim().is_empty()) {
                prior_schema.entities.insert(
                    "player.accessories".into(),
                    serde_json::Value::String(ac.trim().to_owned()),
                );
            }
            // Backstory (2026-08-11): seed the authored history as a
            // `player.backstory` entity so the narrator + retrieval see the
            // character's history from turn 1. Same trust class as the legacy
            // prose above — identity ground truth, not gameplay state.
            if let Some(b) = sp.backstory.as_deref().filter(|s| !s.trim().is_empty()) {
                prior_schema.entities.insert(
                    "player.backstory".into(),
                    serde_json::Value::String(b.trim().to_owned()),
                );
            }
            // Optional descriptive identity fields (2026-08-13): each seeds a
            // `player.*` entity (mirrors backstory) so the narrator reads them
            // as identity ground truth from turn 1 — NOT gameplay state.
            for (key, val) in [
                ("player.job", sp.job.as_deref()),
                ("player.weakness", sp.weakness.as_deref()),
                ("player.distinguishing_marks", sp.distinguishing_marks.as_deref()),
                ("player.gender", sp.gender.as_deref()),
            ] {
                if let Some(s) = val.map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    prior_schema.entities.insert(
                        key.into(),
                        serde_json::Value::String(s.to_owned()),
                    );
                }
            }
            // Chip-list identity fields → one joined narratable line each.
            for (key, list) in [
                ("player.gear", sp.gear.as_ref()),
                ("player.tools", sp.tools.as_ref()),
                ("player.weapons", sp.weapons.as_ref()),
            ] {
                if let Some(items) = list.filter(|v| !v.is_empty()) {
                    let joined = items
                        .iter()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if !joined.is_empty() {
                        prior_schema
                            .entities
                            .insert(key.into(), serde_json::Value::String(joined));
                    }
                }
            }
            // Custom extensions (2026-08-13): seed the player's `custom_tags`
            // into `WorldSchema.custom_tags` so they reach the narrator via the
            // `custom:` render line (entities themselves are persisted but NOT
            // prompted, so the old `player.<key>` entity seed was invisible).
            // Merges with any card-authored custom_tags (card seed ran above).
            if let Some(tags) = sp.custom_tags.as_ref() {
                for (k, v) in tags {
                    let kt = k.trim();
                    let vt = v.trim();
                    if !kt.is_empty() && !vt.is_empty() {
                        prior_schema.custom_tags.insert(kt.to_owned(), vt.to_owned());
                    }
                }
            }
            // Seed the per-run live appearance layer (Phase 4 Component 5,
            // 2026-08-04). The structured trait fields the Player Creator
            // collected become the starting `current_appearance_deltas` so the
            // narrator sees the character's authored look from turn 1. This is
            // the ONE-TIME identity baseline; subsequent `[APPEARANCE]` brackets
            // mutate this map (never the SavedPlayer). Keys mirror the
            // `[APPEARANCE key=value]` allowlist (bracket_parser::APPEARANCE_KEYS)
            // so a mid-session bracket + the seed speak the same vocabulary.
            // The clothing chip list joins into a single `outfit` value (one
            // narratable line, not N fragmented keys).
            {
                let deltas = &mut prior_schema.player_state.current_appearance_deltas;
                let mut push = |k: &'static str, v: Option<&String>| {
                    if let Some(v) = v.map(|s| s.trim()).filter(|s| !s.is_empty()) {
                        deltas.insert(k.to_string(), v.to_string());
                    }
                };
                push("hair_color", sp.hair_color.as_ref());
                push("hair_length", sp.hair_length.as_ref());
                push("hair_style", sp.hair_style.as_ref());
                push("body_type", sp.body_type.as_ref());
                push("skin_complexion", sp.skin_complexion.as_ref());
                push("eye_color", sp.eye_color.as_ref());
                push("breast_size", sp.breast_size.as_ref());
                push("ears", sp.ears.as_ref());
                push("tail", sp.tail.as_ref());
                push("horn", sp.horn.as_ref());
                if let Some(items) = sp.clothing.as_ref().filter(|v| !v.is_empty()) {
                    let joined = items
                        .iter()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if !joined.is_empty() {
                        deltas.insert("outfit".to_string(), joined);
                    }
                }
            }
            // Transient starting gameplay conditions (2026-08-13): the Player
            // Wizard's optional wealth/reputation/fame seed the per-run
            // PlayerState. NOT persisted on the SavedPlayer identity (the §6C
            // identity-only lock is preserved) — these are gameplay numbers
            // that belong to the run, threaded here only because the player
            // just authored them. Loaded players send no conditions → defaults.
            if let Some(conds) = &player_starting_conditions {
                if let Some(w) = conds.wealth {
                    prior_schema.player_state.wealth = w;
                }
                if let Some(r) = conds.reputation {
                    prior_schema.player_state.reputation = r;
                }
            }
            tracing::info!(player_id = pid, player_name = %card.player_name.as_deref().unwrap_or("?"), "attached saved player");
        } else {
            tracing::warn!(player_id = pid, "saved player not found at attach time; proceeding with card default");
        }
    }
    // Store the player id (None when no SavedPlayer is attached) so the
    // chat UI can resolve the player portrait via fable_active_card_get.
    // Placed after the attach block so it always stores the authoritative id
    // (Some or None) — never leaves a stale id from a prior session.
    *state.active_player_id.lock().expect("active_player_id mutex") = player_id.clone();
    // Fable Phase 4 Component 3 (2026-07-28): seed the travel graph from the
    // card's `<locations>` block IF the resolved schema has no graph yet.
    // This is the load-bearing fix for Components 3 + 4 being dead in live
    // play (see docs/phase4-fix-travel-graph-seeding.md). Runs for ALL three
    // branches (fresh / resume / explicit save): the card's geography is
    // authoritative; a resumed save whose graph is empty (a pre-Phase-4
    // save, or a card whose graph was added in a later edit) picks up the
    // current card's graph. A save that already carries a seeded graph is
    // left alone (the player's current_node + any Tracker-added state is
    // preserved). Without this, `[TRAVEL]` is always rejected ("unknown
    // destination" — nodes is empty) + `[RUMOR]` is always dropped
    // (no-current-node path) → Components 3 + 4 never fire in live play.
    if prior_schema.travel_graph.nodes.is_empty() && !card.locations.is_empty() {
        prior_schema.travel_graph = schema::TravelGraph {
            nodes: card
                .locations
                .iter()
                .map(|cn| schema::Node {
                    id: cn.id.clone(),
                    name: cn.name.clone(),
                    neighbors: cn.neighbors.clone(),
                    setting: cn.setting.clone(),
                })
                .collect(),
            // The first <node> in document order is the seed location.
            current_node: card.locations.first().map(|cn| cn.id.clone()),
        };
        tracing::info!(
            node_count = card.locations.len(),
            seed = ?card.locations.first().map(|n| n.id.as_str()),
            "fable_start: seeded travel_graph from card <locations>"
        );
    }
    // Fable Phase 5A (2026-07-29): seed the named-NPC registry from the
    // card's <cast> block. Mirrors the travel_graph seed above (same gate
    // shape: only seed when the schema's registry is empty AND the card
    // declares a cast — a resumed save with a populated registry is left
    // alone, preserving the player's [PRESENCE] state). This is the
    // load-bearing fix for the "teleporting NPC" bug: without it the
    // [PRESENCE] bracket has no whitelist to validate against → every npc_id
    // is "unknown" → every bracket is rejected → the anti-teleport whitelist
    // (the `present:` line) never populates → the narrator is free to
    // hallucinate absent NPCs back into the scene (the §11.48-shaped gap,
    // recurring for Phase 5). The ids here are the Rust-authoritative keys;
    // a card without <cast> stays dormant (pre-Phase-5 behavior).
    if prior_schema.npc_registry.entries.is_empty() && !card.cast.is_empty() {
        prior_schema.npc_registry = schema::NpcRegistry {
            entries: card
                .cast
                .iter()
                .map(|cn| schema::NpcEntry {
                    id: cn.id.clone(),
                    name: cn.name.clone(),
                    role: cn.role.clone(),
                    tier: cn.tier.clone(),
                    aliases: cn.aliases.clone(),
                })
                .collect(),
        };
        tracing::info!(
            npc_count = card.cast.len(),
            ids = ?card.cast.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            "fable_start: seeded npc_registry from card <cast>"
        );
    }
    // Cold-start anchors (2026-08-10, Issue #2): seed the world clock + weather
    // from the card's <start> block IF they're still dormant. The tracker
    // renders no clock:/weather: line while these are unset → it has nothing to
    // maintain → [TIME]/[WEATHER] never fire (the cold-start hole). Seeding
    // them gives the tracker the anchors it needs from turn 1. Mirrors the
    // travel_graph/npc_registry seeds' gate shape: only seed when dormant, so a
    // resumed save that already advanced the clock (or set weather) is left
    // alone (the player's [TIME]/[WEATHER] progress is preserved).
    //
    // Clock: write BOTH current_minutes AND last_tick_minutes so the seed
    // counts as the baseline — the World Progression tick's first-call rule
    // (no fire on the first [TIME]) is honored (a campaign doesn't simulate a
    // day it just established). Weather: stamp started_at_minutes to the seeded
    // clock so the persistence curve has a sane origin (0 when the clock is
    // also unseeded — harmless, the drift DC just starts from baseline).
    if let Some(mins) = card.start.time_minutes {
        if !prior_schema.world_clock.is_set() {
            prior_schema.world_clock.current_minutes = mins;
            prior_schema.world_clock.last_tick_minutes = mins;
            tracing::info!(
                start_minutes = mins,
                "fable_start: seeded world_clock from card <start><time>"
            );
        }
    }
    if let Some(condition) = card.start.weather.as_deref() {
        if !prior_schema.weather.is_set() {
            prior_schema.weather.condition = condition.to_string();
            prior_schema.weather.started_at_minutes =
                prior_schema.world_clock.current_minutes;
            tracing::info!(
                condition = %condition,
                "fable_start: seeded weather from card <start><weather>"
            );
        }
    }
    // Calendar (2026-08-13): seed the free-form date label from `<start><date>`.
    // Dormancy-gated (mirrors clock/weather) so a resumed save that advanced the
    // label via [DATE] is preserved. None on cards without a <date> → the legacy
    // "Day N, HH:MM" clock render stands. No forced default (the wizard supplies
    // it; non-wizard cards stay dormant).
    if let Some(date) = card.start.date.as_deref().filter(|s| !s.is_empty()) {
        if prior_schema.calendar.as_deref().filter(|s| !s.is_empty()).is_none() {
            prior_schema.calendar = Some(date.to_string());
            tracing::info!(calendar = %date, "fable_start: seeded calendar from card <start><date>");
        }
    }
    // Custom extensions (2026-08-13): seed the card's `<custom_tags>` into the
    // schema so they reach the narrator via the `custom:` render line. Seed only
    // when the schema's map is empty (idempotent on re-entry; preserves a resumed
    // save's tags).
    if prior_schema.custom_tags.is_empty() && !card.custom_tags.is_empty() {
        prior_schema.custom_tags = card.custom_tags.clone();
        tracing::info!(
            count = prior_schema.custom_tags.len(),
            "fable_start: seeded custom_tags from card <custom_tags>"
        );
    }
    // Cold-start anchor bootstrap (2026-08-10): if the clock and/or weather are
    // STILL dormant after the <start> seed (no <start> block, or it seeded only
    // one of the two), derive them from the card's .intro via one schema-engine
    // pass. This closes the cold-start hole for Creator-authored cards that
    // ship no <start>: the tracker renders no clock:/weather: line while dormant
    // → it has nothing to maintain → [TIME]/[WEATHER] never fire. Seeding from
    // the intro gives the tracker its baseline from turn 1. Best-effort +
    // detached from game-entry: a failure falls through to sensible defaults
    // below, never blocks the launch. (The .intro is normally read once for the
    // UI at line ~7077; for the bootstrap we re-read here — cheap, small file.)
    if !prior_schema.world_clock.is_set() || !prior_schema.weather.is_set() {
        // The opening beat now lives in-file as the `<intro>` sibling
        // (2026-08-13); fall back to a legacy `.intro` FILE for old cards.
        let intro_text = if !card.intro.trim().is_empty() {
            Some(card.intro.clone())
        } else {
            resolve_fable_cards_dir(app)
                .and_then(|root| load_card_intro(&root, &card.id))
        };
        let derived = if let Some(intro) = intro_text.as_deref().filter(|s| !s.trim().is_empty()) {
            bootstrap_anchors_from_intro(
                state,
                intro,
                card.setting.as_deref(),
                card.tone.as_deref(),
                card.player_name.as_deref(),
            )
            .await
        } else {
            None
        };
        // Apply derived anchors (each only to its still-dormant field).
        if let Some(ref a) = derived {
            if let Some(mins) = a.time_minutes {
                if !prior_schema.world_clock.is_set() {
                    prior_schema.world_clock.current_minutes = mins;
                    prior_schema.world_clock.last_tick_minutes = mins;
                    tracing::info!(start_minutes = mins, "bootstrap: seeded clock from intro");
                }
            }
            if let Some(ref condition) = a.weather {
                if !prior_schema.weather.is_set() {
                    prior_schema.weather.condition = condition.clone();
                    prior_schema.weather.started_at_minutes =
                        prior_schema.world_clock.current_minutes;
                    tracing::info!(condition = %condition, "bootstrap: seeded weather from intro");
                }
            }
            // Location: only when the graph is still empty (a resumed save with
            // an existing graph is preserved). upsert_node + set current_node.
            if let Some((ref id, ref name)) = a.location {
                if prior_schema.travel_graph.nodes.is_empty() {
                    let node = schema::Node {
                        id: id.clone(),
                        name: name.clone(),
                        neighbors: vec![],
                        setting: String::new(),
                    };
                    if prior_schema.travel_graph.upsert_node(node) {
                        prior_schema.travel_graph.current_node = Some(id.clone());
                        tracing::info!(node_id = %id, "bootstrap: seeded opening location from intro");
                    }
                }
            }
        }
        // Sensible-defaults fallback: the "always get the tracker flow going"
        // guarantee. If a field is STILL dormant (bootstrap returned None, or
        // returned None for that specific field), seed a neutral default so the
        // tracker has an anchor. Location is deliberately left dormant (the
        // [DISCOVER] path handles it organically on turn 1 — forcing a default
        // location would be a guess).
        if !prior_schema.world_clock.is_set() {
            // Day 1, 09:00 = 540 minutes. Neutral mid-morning — the most
            // generic non-zero anchor (lets the tracker advance from a sane
            // baseline regardless of when the scene "actually" starts).
            prior_schema.world_clock.current_minutes = 540;
            prior_schema.world_clock.last_tick_minutes = 540;
            tracing::info!("bootstrap: seeded default clock (Day 1, 09:00)");
        }
        if !prior_schema.weather.is_set() {
            // Derive from tone if it mentions weather; else "clear".
            let default_weather = card
                .tone
                .as_deref()
                .map(|t| {
                    let tl = t.to_lowercase();
                    if tl.contains("fog") || tl.contains("mist") {
                        "fog"
                    } else if tl.contains("rain") || tl.contains("storm") {
                        "heavy rain"
                    } else if tl.contains("snow") {
                        "snowfall"
                    } else {
                        "clear"
                    }
                })
                .unwrap_or("clear");
            prior_schema.weather.condition = default_weather.to_string();
            prior_schema.weather.started_at_minutes =
                prior_schema.world_clock.current_minutes;
            tracing::info!(condition = %default_weather, "bootstrap: seeded default weather");
        }
    }
    *state.fable_schema.lock().await = prior_schema;
    clear_fable_history(&state).await;
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
            variants: m.variants.clone(),
            active_idx: m.active_idx,
            timestamp: m.timestamp,
            reasoning: m.reasoning.clone(),
        })
        .collect();
    let turn_count = messages.len();
    *state.fable_session.lock().await = prior_session;
    // Read the one-shot opening beat BEFORE the card is moved into
    // active_fable_card. Surfaced on FableLoadResult.intro so the UI renders the
    // first narrator beat on a FRESH game without a second IPC round-trip. It is
    // NEVER injected into the cached system prompt (a one-turn seed; prime
    // directive). The beat now lives in-file as the `<intro>` sibling after
    // </sim_card> (2026-08-13); fall back to a legacy `.intro` FILE for old cards.
    let intro = if !card.intro.trim().is_empty() {
        Some(card.intro.clone())
    } else {
        resolve_fable_cards_dir(app)
            .and_then(|root| load_card_intro(&root, &card.id))
    };
    *state.active_fable_card.lock().expect("active_fable_card mutex") = Some(card);

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
        intro,
    })
}

/// The title-screen CONTINUE button's resume target. Scans every New Game
/// world's saves dir + returns the single most-recent save of ANY kind
/// (autosave OR manual). `None` (→ JSON null) when no qualifying save exists,
/// which is the signal for the frontend to disable CONTINUE.
///
/// The contract is "Continue = land me exactly where I left off in a New Game
/// world." Autosaves are INCLUDED: an autosave is written every turn, so the
/// freshest save for any active world is almost always the autosave; resuming
/// the last *manual* save would silently discard turns. CONTINUE means "pick
/// up where I stopped," which is the per-turn checkpoint.
///
/// Why a dedicated IPC instead of `fable_list_saves`: that one is per-card and
/// the title sits BEFORE card selection, so CONTINUE must look across all New
/// Game worlds for "your last save, anywhere."
///
/// Returns a `SaveMeta` (lightweight — no session/schema payload). The
/// frontend stashes it + resumes via `fable_start(card_id, save_id)` only when
/// the user actually clicks CONTINUE.
#[tauri::command]
fn fable_continue_target(app: tauri::AppHandle) -> Result<Option<fable_save::SaveMeta>, String> {
    let fable_root = resolve_apps_dir(&app).join("fable");
    Ok(most_recent_continue_target(&fable_root))
}

/// Pure core of `fable_continue_target`: walks `saves/<card_id>/` for every
/// card, returning the globally-most-recent `SaveMeta` of
/// any slot type (autosave + manual). Extracted so the contract (New Game
/// worlds only; autosaves count) is unit-testable with a tempdir.
fn most_recent_continue_target(fable_root: &std::path::Path) -> Option<fable_save::SaveMeta> {
    // 2026-08-01 layout: saves live inside each card's folder at
    // `cards/<card_id>/saves/`. Walk the cards tree, and for each card folder
    // that has a `saves/` subdir, list its slots.
    let cards_dir = fable_root.join("cards");
    let Ok(entries) = std::fs::read_dir(&cards_dir) else {
        return None;
    };
    let mut best: Option<fable_save::SaveMeta> = None;
    for entry in entries.flatten() {
        // Each card has its own folder: cards/<card_id>/. Skip stray files.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let card_id = entry.file_name().to_string_lossy().to_string();
        if let Ok(saves) = fable_save::list_saves(fable_root, &card_id) {
            for s in saves {
                // list_saves is newest-first per-card; track the global newest.
                // Autosaves ARE eligible (see the command doc above).
                if best.as_ref().map_or(true, |b| s.timestamp > b.timestamp) {
                    best = Some(s);
                }
            }
        }
    }
    best
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
    // Always-on thinking: inject the Gemma4 `<|think|>` control token at the
    // end of the system turn so the tracker always reasons before its brackets.
    // (The narrator is the API — it uses OpenAI format and never sees this fn.)
    // DISABLED 2026-08-09 (`THINKING_ENABLED`) — see settings.rs.
    if crate::settings::THINKING_ENABLED {
        prompt.push_str("<|think|>");
    }
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

/// Cap each assistant message's content to `cap` chars (2026-08-10, T52
/// overflow fix). The tracker window feeds the API narrator's full beat (up to
/// ~4700 chars ≈ ~1200 tokens) into the local tracker's 3072-token context,
/// which blew the 2922 budget 7 times in T52 and front-chopped the bracket
/// protocol. The tracker only needs the gist to decide whether brackets fire,
/// so capping assistant prose mathematically bounds the window. User messages
/// are NOT capped (they're the player's action — the trigger the tracker keys
/// off). UTF-8-safe: truncates at a char boundary + appends `[…]`. Pure + unit-
/// tested (no model, no GPU).
pub(crate) fn cap_assistant_prose(
    mut window: Vec<session::Message>,
    cap: usize,
) -> Vec<session::Message> {
    for m in window.iter_mut() {
        if m.role == session::Role::Assistant && m.content.len() > cap {
            let cut = m
                .content
                .char_indices()
                .nth(cap)
                .map(|(idx, _)| idx)
                .unwrap_or(m.content.len());
            m.content.truncate(cut);
            m.content.push_str(" […]\n");
        }
    }
    window
}

// ───────────────────────────────────────────────────────────────────────────
// Narrator system-prompt builders (inline, lean — 2026-07-31).
//
// The dedicated prompt modules were removed; these two pure fns render the
// minimal system prompt the narrator + tracker turns need. Both share the
// same skeleton: identity (from the card) → world_state → scene_pacing →
// optional director directive → optional retrieved-knowledge block. They
// differ ONLY in whether the bracket protocol is included:
//   • build_narrator_system_prompt      → tracker prompt (emits brackets).
//   • build_api_narrator_system_prompt  → prose-only narrator (no brackets).
//
// The structural tags (<world_state>, <scene_pacing>, <retrieved_knowledge>)
// are load-bearing: the bracket-parser tests + the post-gen apply pipeline
// + the test below all key off them. Keep the tag names stable.
// ───────────────────────────────────────────────────────────────────────────

/// Render the player's handle for the narrator, falling back to a neutral
/// "User" when the card carries no player name (mirrors the §11.29
/// anti-positivity-bias neutral handle — never a title like "protagonist").
/// Compose the shared system-prompt skeleton (everything except the
/// tracker-vs-narrator voice/protocol preamble, which the two callers
/// prepend). The order of the tagged blocks is:
/// `<player_action>` → `<world_state>` → `<retrieved_knowledge>` →
/// `<scene_pacing>`. The manual player_action leads so the narrator's
/// attention lands on the player's last tactile UI action (§11.30) before
/// reading the world state.
fn assemble_narrator_skeleton(
    player_action: Option<&str>,
    world_state: Option<&str>,
    pacing: schema::ScenePacing,
    memory_block: Option<&str>,
) -> String {
    let mut out = String::with_capacity(2048);

    // Manual player action (§11.30) — leads the skeleton. Emits the exact
    // `<player_action type="manual_override">…</player_action>` form the
    // left-drawer HUD arms via `fable_player_action_set`.
    if let Some(pa) = player_action.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("<player_action type=\"manual_override\">\n");
        out.push_str(pa);
        out.push_str("\n</player_action>\n\n");
    }

    if let Some(ws) = world_state.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("<world_state>\n");
        out.push_str(ws);
        out.push_str("\n</world_state>\n\n");
    }

    if let Some(mem) = memory_block.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("<retrieved_knowledge>\n");
        out.push_str(mem);
        out.push_str("\n</retrieved_knowledge>\n\n");
    }

    out.push_str("<scene_pacing mode=\"");
    out.push_str(pacing.mode.tag());
    out.push_str("\">\n");
    out.push_str(pacing.mode.prose_guidance());
    out.push_str("\n</scene_pacing>\n\n");

    out
}

/// Tracker system prompt. The tracker reads the **agent** section of the
/// authored `.prompt` file (its mechanical job), the card identity (name/
/// setting/tone from the `.sim`), the live per-turn state blocks (assembled
/// by `assemble_narrator_skeleton`), and the bracket-command list.
///
/// **Voice-authoring is gone from Rust.** This function does NOT author any
/// narrator voice/style/behavior prose — that lives in `fable.prompt`. It only
/// (a) slots the authored agent prose, (b) slots the card identity, (c) slots
/// the live data tags, (d) appends the bracket-command protocol reference.
/// The bracket list stays in Rust (load-bearing: `bracket_parser::parse` keys
/// off the exact forms; a typo'd bracket in a `.prompt` file would silently
/// break state tracking with no compile error to catch it).
fn build_narrator_system_prompt(
    prompts: &prompts::FablePrompts,
    _card: &sim_card::SimCard,
    world_state: Option<&str>,
    pacing: schema::ScenePacing,
    player_action: Option<&str>,
    memory_block: Option<&str>,
) -> String {
    let mut out = String::with_capacity(2048);

    // The tracker is a STATE LEDGER, not a narrator. It needs ONLY:
    //   (a) the AGENT directive (how to track — from fable.prompt)
    //   (b) the live state delta (world_state — the current truth it mutates)
    //   (c) the bracket syntax reference (the mechanical interface)
    //
    // Card identity (setting/plot/tone/core_persona) is NARRATOR FLAVOR — it
    // belongs only in build_api_narrator_system_prompt (the API path). The
    // tracker doesn't need to know the setting is "a fogbound marsh-edge town"
    // to emit [TRAVEL market_square] or [NPC_REGISTER mara]. All world state
    // (NPCs, locations, player appearance, equipment) is already seeded into
    // the schema at fable_start + surfaced to the tracker via <world_state>.
    // Including the card here was ~400 tokens of bloat that contributed to the
    // prompt-overflow → bracket-protocol-chopped → zero-brackets bug.
    // (2026-08-09: card param kept as `_card` for signature stability.)

    // (a) Authored agent/tracker prose (from fable.prompt's AGENT section).
    let agent = prompts.agent.trim();
    if !agent.is_empty() {
        out.push_str(agent);
        out.push_str("\n\n");
    }

    // (b) Live per-turn state blocks (world_state, retrieved_knowledge,
    // scene_pacing, player_action). Zero authored prose here.
    out.push_str(&assemble_narrator_skeleton(
        player_action, world_state, pacing, memory_block,
    ));

    // (c) Bracket-command protocol reference. The tracker emits these
    // alongside its prose so Rust can apply the mechanical truth. One TIGHT
    // line each — the syntax only; the WHEN/WHY guidance lives in the AGENT
    // prose above (the <item_tags> block covers tag classification, the
    // <restraint> block covers emit-only-on-change). KEPT LEAN per the Prime
    // Directive: this list is in the always-on system prompt, so every byte
    // costs context budget on every tracker turn. Detailed bracket semantics
    // are in the card's codex (retrieved on-demand), NOT here.
    out.push_str("Track changes with these brackets (emit one per genuine change this turn — see <triggers> for exactly when each fires):\n");
    out.push_str("- [TIME Day N, HH:MM] — advance the clock when the turn spans hours (travel, sleep, a long task). Use the real time, never a placeholder.\n");
    out.push_str("- [DATE <new calendar label>] — when a day or more passes or a calendar boundary is crossed (a new day dawns, a month turns, a festival begins). Emit the NEW full date label verbatim (e.g. \"4th of Harvest, Year 1247, Market Day\") — the whole label is rewritten, no arithmetic. Only fires when a `date:` anchor is in use; otherwise omit.\n");
    out.push_str("- [WEATHER <condition>] — when the narrated weather shifts (fog lifts, rain starts, night falls). One word: fog, rain, snow, clear, overcast, storm, etc.\n");
    out.push_str("- [TRAVEL <node_id>] — when the player arrives at a different location. Use a node_id from the location line's exits (or one you seed via [DISCOVER]).\n");
    out.push_str("- [RUMOR <rumor_text>] — when a rumor is actually heard or spread in the scene. The whole phrase is the rumor.\n");
    out.push_str("- [NPC_REGISTER <npc_id> <name> (role) (tier)] — the FIRST time a named NPC who is NOT in the cast line appears. Register them once, then use [PRESENCE]. tier: minion|soldier|elite|boss|legendary (omit for civilians).\n");
    out.push_str("- [PRESENCE <npc_id> <stance or micro-location>] — when an NPC enters the scene or is already on-camera. Resolves names from the cast line. Example: [PRESENCE mara \"behind the bar, drying a glass\"].\n");
    out.push_str("- [DISCOVER <node_id> <name> (setting=indoor|outdoor) (neighbors=<ids>)] — when the player reaches a place that is not yet in the location line's exits. Adds it as a travelable node.\n");
    out.push_str("- [EFFECT <label> buff|debuff <secs> [kind=disguise]] — when a buff/debuff or disguise takes hold.\n");
    out.push_str("- [MILESTONE <npc_id> <event_id>] — when a relationship meaningfully shifts (alliance forged, trust broken).\n");
    out.push_str("- [TASK <npc> <description> <difficulty> <eta-minutes>] — when an NPC starts an off-screen task you should track.\n");
    out.push_str("- [APPEARANCE key=value] — when the player's look LASTINGLY changes (a disguise applied, hair cut, a scar earned). Keys: hair_color, outfit, eye_color, scars, wounds, tattoos, disguise, body_type, skin_complexion, hair_length, hair_style, breast_size, ears, tail. Bare [APPEARANCE key] clears.\n");
    out.push_str("- [EQUIP slot=<slot> name=<item> (layer=outer|inner) (stats=<note>) (tags=<tags>)] — when a garment or READY weapon is put ON. Slots: head, chest, main_hand, off_hand, legs, feet. main_hand/off_hand are for readied weapons/tools ONLY — a sheathed or belt-hung blade goes in [BELT], not a hand slot. Bare [EQUIP slot=<slot>] unequips. Quote multi-word names.\n");
    out.push_str("- [BELT name=<item> (qty=N) (tags=<tags>)] / [BELT -<name>] — when a quick-access item (a belt knife, a potion, a pouch) is gained or stowed. This is where belt-hung knives belong.\n");
    out.push_str("- [PACK name=<item> (qty=N) (tags=<tags>)] / [PACK -<name>] — when an item is acquired into deep storage or removed. Check the pack line first so you don't re-add what's already there.\n");
    out.push_str("Tags (EQUIP/BELT/PACK): consumable, equippable, pocketable — see <item_tags> above.\n");

    out
}

/// Prose-only narrator system prompt. The narrator reads the **narrator**
/// section of the authored `.prompt` file (its voice), the card identity
/// (name/setting/tone from the `.sim`), and the live per-turn state blocks.
/// NO bracket-command list — the tracker (Local Stage-2) owns mechanics;
/// this model only narrates what Rust says happened.
///
/// **Voice-authoring is gone from Rust.** No authored narrator voice/style/
/// behavior prose lives here — that's `fable.prompt`'s narrator section. This
/// function is a pure data-slotter: authored prose → card identity → live
/// state tags. Nothing else.
fn build_api_narrator_system_prompt(
    prompts: &prompts::FablePrompts,
    card: &sim_card::SimCard,
    world_state: Option<&str>,
    pacing: schema::ScenePacing,
    player_action: Option<&str>,
    memory_block: Option<&str>,
) -> String {
    let mut out = String::with_capacity(2048);

    // (a) Authored narrator prose (from fable.prompt's NARRATOR section).
    let narrator = prompts.narrator.trim();
    if !narrator.is_empty() {
        out.push_str(narrator);
        out.push_str("\n\n");
    }

    // (b) Card identity (from the active .sim scenario card).
    out.push_str("Scenario: ");
    out.push_str(card.name.trim());
    out.push('\n');
    if let Some(setting) = card.setting.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("Setting: ");
        out.push_str(setting.trim());
        out.push('\n');
    }
    if let Some(plot) = card.plot.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("Plot: ");
        out.push_str(plot.trim());
        out.push('\n');
    }
    if let Some(tone) = card.tone.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("Tone: ");
        out.push_str(tone.trim());
        out.push('\n');
    }
    let core = card.core_persona.trim();
    if !core.is_empty() {
        out.push_str(core);
        out.push('\n');
    }
    out.push('\n');

    // (c) Live per-turn state blocks. Zero authored prose.
    out.push_str(&assemble_narrator_skeleton(
        player_action, world_state, pacing, memory_block,
    ));

    out
}

/// System prompt for the **golden-pencil slice regenerate** (2026-08-11): the
/// player highlighted a span inside an existing assistant message and the API
/// rewrites ONLY that span, splicing cleanly against the surrounding text.
///
/// Mirrors `build_api_narrator_system_prompt`'s (a) authored narrator voice +
/// (b) card identity blocks — voice continuity + scenario framing — then
/// appends a terse splice-discipline section instead of the live-state
/// skeleton. The slice job is narrower than a full narrator turn: it needs the
/// voice + the scenario + the splice contract, NOT `<world_state>` /
/// `<scene_pacing>` / `<retrieved_knowledge>` (Prime Mandate — only what 100%
/// of these turns need; the prev/next beats + the pre/selection/post payload
/// in the user message supply all the context the model requires). API-only,
/// never thinks, never emits brackets (any accidental brackets in the regen
/// are STRIPPED by the caller via `bracket_parser::parse`, never applied).
fn build_slice_regenerate_system_prompt(
    prompts: &prompts::FablePrompts,
    card: &sim_card::SimCard,
) -> String {
    let mut out = String::with_capacity(2048);

    // (a) Authored narrator prose — same voice the original beat was written in.
    let narrator = prompts.narrator.trim();
    if !narrator.is_empty() {
        out.push_str(narrator);
        out.push_str("\n\n");
    }

    // (b) Card identity — identical to build_api_narrator_system_prompt (b).
    out.push_str("Scenario: ");
    out.push_str(card.name.trim());
    out.push('\n');
    if let Some(setting) = card.setting.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("Setting: ");
        out.push_str(setting.trim());
        out.push('\n');
    }
    if let Some(plot) = card.plot.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("Plot: ");
        out.push_str(plot.trim());
        out.push('\n');
    }
    if let Some(tone) = card.tone.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str("Tone: ");
        out.push_str(tone.trim());
        out.push('\n');
    }
    let core = card.core_persona.trim();
    if !core.is_empty() {
        out.push_str(core);
        out.push('\n');
    }
    out.push('\n');

    // (c) Splice discipline — terse, mechanical, per-turn (Prime-Mandate
    // compliant: this is the core job instruction, not bloat). Genericized,
    // no copyable concrete examples (anti-pattern #4).
    out.push_str(
        "A single passage of your own narration is being revised. Output ONLY the rewritten passage.\n\
         Do not restate the text that precedes or follows it.\n\
         Splice cleanly: your first character continues directly from the preceding text; your last character leads directly into the following text. Add a leading or trailing space only if the adjacent character requires one. Do not duplicate adjacent punctuation or quotation marks.\n\
         Wrap only spoken dialogue in straight double quotes. Do not wrap the whole output in quotes.\n\
         No preamble, commentary, or explanation — only the passage.\n",
    );

    out
}

/// Build the system prompt for the GLM-driven creation assistant — the
/// conversational wizard for player / sim-world / codex authoring. This is a
/// CREATION-ONLY API role (AGENTS.md §3A, 2026-08-12 override): it runs
/// outside the runtime game loop, so the narrator/tracker/chat isolation
/// rules do not apply. Modeled on `build_slice_regenerate_system_prompt`'s
/// lean 3-section shape: (a) identity, (b) the target schema spec (the exact
/// keys the model must emit — this IS the core job instruction,
/// Prime-Mandate-compliant), (c) the JSON-envelope discipline. GLM NEVER
/// writes XML or files — it fills a `draft` JSON object; the frontend
/// serializer + the existing write IPCs persist it.
fn build_creator_assistant_system_prompt(creator_kind: &str) -> String {
    let mut out = String::with_capacity(3072);

    // (a) Identity.
    out.push_str("You are WUPI's creation assistant. You help the user design ");
    match creator_kind {
        "player" => out.push_str("a player character"),
        "sim" => out.push_str("a roleplay world (a Fable sim card)"),
        "codex" => out.push_str("a lorebook (a codex of world rules and lore)"),
        _ => out.push_str("a creative element"),
    }
    out.push_str(
        " through conversation. Ask focused follow-up questions until you have enough \
         to finalize, then present the result. Be concise, warm, and concrete. Never \
         invent details the user has not supplied or clearly implied — ask instead.\n\n",
    );

    // (b) Schema spec — the exact field keys the `draft` must carry. Emit only
    // these keys; omit a key if it is genuinely unknown rather than guessing.
    out.push_str("You are authoring THIS schema:\n");
    match creator_kind {
        "player" => out.push_str(
            "CORE FIELDS — all required; do not emit ready until every one is filled: \
             name (string), gender (free-form identity text, e.g. \"female\", \
             \"masculine\", \"nonbinary\" — NOT restricted to male/female), age, race, \
             skin_complexion, height, weight, hair_color, hair_length, hair_style, \
             eye_color (short string traits), clothing (array of garment strings).\n\
             CONTEXTUAL FIELDS — use context clues; ask only when they apply and omit \
             otherwise: breast_size (only if the character is female), ears, tail, horn \
             (only if the race is non-human).\n\
             OPTIONAL FIELDS — surface only if the player raises them or they fit \
             naturally: body_type (build/frame), accessories, gear (array), tools \
             (array), weapons (array), distinguishing_marks, job, personality, weakness, \
             backstory (prose), wealth (a starting amount), reputation or fame (a \
             starting standing).\n\
             CUSTOM EXTENSIONS — if the player mentions any extra stat, currency, \
             faction standing, curse, or attribute that does not fit a field above \
             (e.g. \"start me with 200 gold and -20 rep with the guards\"), route it \
             into custom_tags as a flat {key: value} string map (e.g. \
             {\"starting_currency\":\"200 gold\",\"guard_reputation\":\"-20\"}). Never \
             duplicate a core or optional field as a custom tag.\n\n",
        ),
        "sim" => out.push_str(
            "TURN 1 IS A TYPE ROUTER. First decide card_type — one of \"npc\" \
             (a character: companion, enemy, shopkeeper), \"scenario\" (an event, quest \
             seed, or encounter), or \"world\" (a biome, town, dungeon, or lore place) — \
             plus the name. Set draft.card_type + draft.name, then gather ONLY that \
             branch's fields. Never switch types mid-card.\n\
             UNIVERSAL WORLD ANCHORS (required for EVERY card_type; if the player is \
             vague, supply a sensible default — never leave any empty): date (a rich \
             free-form calendar label — month/year/type-of-day, e.g. \"3rd of Harvest, \
             Year 1247, Market Day\"; NOT just \"Day 1\"), time (time-of-day, e.g. \
             \"09:00\" or \"late morning\"), weather (e.g. \"clear\", \"heavy rain\"), \
             location (the opening place name — becomes the first locations entry).\n\
             NPC BRANCH (card_type \"npc\"): copy the Player Wizard identity fields \
             (name, gender, age, race, skin_complexion, height, weight, hair_color, \
             hair_length, hair_style, eye_color, clothing, + contextual breast_size / \
             ears / tail / horn when they apply) — all mandatory; PLUS personality, \
             flaws, job, backstory, dialogue_style, tone — mandatory. The card's NPC is \
             the single cast entry {name, identity=job}.\n\
             SCENARIO BRANCH (card_type \"scenario\"): directive (the core event \
             premise), trigger_condition (what sets it off), primary_objective, \
             participating_actors (NPC or faction names), tone — all mandatory; \
             environmental_hazards + outcomes (success/failure hooks) — optional.\n\
             WORLD BRANCH (card_type \"world\"): directive (the world's purpose), setting \
             (the world's identity), tone — all mandatory.\n\
             SHARED (all branches): locations = array of {name, neighbors:[strings]} \
             (the location anchor is the FIRST entry; the graph grows in play). cast = \
             array of {name, identity} (npc: the one NPC; scenario/world: any starting \
             NPCs, or omit). custom_tags = optional flat {key:value} string map for \
             anything that doesn't fit a field above (currency, faction standing, \
             curse, custom attribute).\n\
             THE INTRO QUESTION (mandatory for EVERY card_type — the card cannot be \
             finalized without it): before emitting ready you MUST ask the player \
             whether they want an INTRO (the opening narrator beat that starts the \
             game). If yes, ask what they'd like it to be — or offer to write one \
             yourself from the card's tone and anchors if they have no preference — \
             and set draft.intro to the agreed 2-4 sentence opening (second person, \
             present tense, no dialogue tags or bracket commands). If the player \
             declines, leave intro empty. An imported card's first_mes / \
             alternate_greetings (in the <import> block) count as an agreed intro — \
             confirm + carry them into draft.intro rather than rewriting.\n\n",
        ),
        "codex" => out.push_str(
            "entries (array of objects each {title, tags:[short keywords], body}). One \
             concept per entry. For EACH body: tighten the prose (strip filler, \
             redundancy, and meta-talk) but PRESERVE every lore fact — do not \
             compress lore away to fit. The body MUST stay under 1400 characters \
             (the embedding model's per-entry window — anything longer won't \
             embed or retrieve whole). If a concept still exceeds 1400 chars \
             after tightening, SPLIT it across multiple entries titled \
             \"<Concept> — Part 1\", \"<Concept> — Part 2\", … so the full \
             content carries across parts that each embed cleanly. Never \
             truncate or drop lore to meet the ceiling.\n\n",
        ),
        _ => {}
    }

    // (c) Input curation — how to treat what the user gives you. The draft lands
    // on a human-in-the-loop review card, so every field must be clean,
    // structured, and game-ready — never raw chat text. Curating = reformatting
    // + condensing SUPPLIED facts; it never licenses inventing (still ask when
    // something is missing).
    out.push_str(
        "INPUT CURATION — format the user's words into game-ready fields, never \
         transcribe them raw:\n\
         - Array/chip fields (clothing, gear, tools, weapons): parse \
         conversational input into clean title-cased items. \"a worn-out iron \
         broadsword with a notched hilt\" becomes \"Notched Iron Broadsword\".\n\
         - Prose fields (backstory, job, distinguishing_marks, personality): \
         synthesize the user's words into punchy, third-person narrative prose, \
         preserving every core lore fact — factions, names, hooks, ties. Target \
         ~150-300 words for backstory/description; if the user pastes a very long \
         lore block (roughly >600 words), compress it to its core beats while \
         keeping every key faction, name, and hook.\n\
         - IMPORTED reference data (delivered as an <import> block): preserve its \
         authored lore faithfully — map fields onto the schema with high fidelity, \
         not a rewrite. LIVE chat responses: actively curate + format them as \
         above. The distinction matters: imports are authored work to keep; chat \
         is rough material to refine.\n\
         Curating is reformatting and condensing facts the user gave you — never \
         inventing details they did not supply.\n\n",
    );

    // (d) Envelope discipline — terse, mechanical, per-turn.
    out.push_str(
        "Respond with ONE JSON object and nothing else — no markdown fences, no prose \
         around it. Each turn choose exactly one action:\n\
         - To ask follow-ups: {\"action\":\"ask\", \"message\":\"<one short paragraph to the user>\", \"questions\":[\"<q1>\", ...], \"draft\":{<fields already decided>}}\n\
         - When ready to finalize: {\"action\":\"ready\", \"draft\":{<every required field filled>}}\n\
         The draft accumulates across turns — repeat the fields decided so far on every turn (or omit unchanged ones); never blank out a field already set. Ask at most three short questions per turn, one focused thread at a time — never a wall of questions. \
         For the player schema: settle gender and race early, then ask only the contextual fields that actually apply (breast_size if the character is female; ears, tail, horn if the race is non-human). Do not emit ready until every core field, every applicable contextual field, and any optional or custom-tag thread the player raised are resolved. \
         For the sim schema: do not emit ready until draft.card_type + name are set, every universal anchor (date, time, weather, location) is filled, the chosen branch's mandatory fields are complete (npc: the identity fields + personality/flaws/job/backstory/dialogue_style/tone; scenario: directive/trigger_condition/primary_objective/participating_actors/tone; world: directive/setting/tone), AND the INTRO question has been answered (an agreed draft.intro, or an explicit no — never assume either way). \
         Keep every string value in plain prose (no JSON, no markup) except where the value is itself prose.\n",
    );

    out
}

/// `WorldSchema::render_for_prompt` + the read-time-derived `condition:` +
/// active status tags + a `<directives>` block of turn-scoped hard facts the
/// narrator must obey. Pure read of Rust-authoritative state (the schema lock
/// is held by the caller).
///
/// §11.41 (2026-07-28, DM / Voice-Actor split): factored out of `fable_send`'s
/// pre-narrator schema block so it can be invoked TWICE in API mode — once for
/// the tracker stage (pre-tracker state) and once for the API narrator stage
/// (post-tracker state, after the tracker's brackets have mutated the schema).
/// The turn-scoped directives (combat lethality, skill checks, tick directives)
/// are passed in by the caller so the SAME directives ride both renders — the
/// Referees run ONCE (pre-tracker), not twice (several mutate state).
fn render_fable_world_state(
    s: &schema::WorldSchema,
    turn_directives: &[String],
) -> String {
    let mut rendered = s.render_for_prompt();

    // Condition + active status tags (Phase 3 Slice 2 + 4 render).
    let buffs_count = consequence::count_by_polarity(&s.status_tags, consequence::Polarity::Buff);
    let debuffs_count = consequence::count_by_polarity(&s.status_tags, consequence::Polarity::Debuff);
    let condition = consequence::derive_condition(&s.player_state.body, buffs_count, debuffs_count);
    if !rendered.is_empty() {
        rendered.push_str("\n\n");
    }
    rendered.push_str(&format!("condition: {}\n", condition.label().to_lowercase()));
    if let Some(tags_block) = consequence::render_tags_for_prompt(&s.status_tags) {
        rendered.push_str(&tags_block);
    }

    // Turn-scoped directives block (combat lethality + skill checks + tick
    // directives). The caller passes the already-assembled directive lines in
    // priority order (lethality first, then skills, then ticks).
    if !turn_directives.is_empty() {
        if !rendered.is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str("<directives>\n");
        for d in turn_directives {
            rendered.push_str(&format!("[DIRECTIVE: {}]\n", d));
        }
        rendered.push_str("</directives>");
    }

    rendered
}

/// Backend launch guard (2026-08-07 architectural override). Fable narration
/// is API-only, so every Fable entry IPC (`fable_start`)
/// refuses to start a session unless an API provider is active + selected. The
/// frontend gate (`launchFable` → `model_source_get`) is the primary block;
/// this is the backend backstop so a stale frontend or a direct IPC call can
/// never start a game in a non-API state. `fable_send` has its own inline copy
/// of this check (it cannot use this helper because it returns a different
/// error type). Returns `Ok(())` when API is ready, else `Err(message)`.
fn require_api_for_fable(state: &tauri::State<'_, AppState>) -> Result<(), String> {
    let source = *state.model_source.lock().expect("model_source mutex");
    if source != api::ModelSource::Api {
        return Err(
            "Fable requires an active API connection to narrate. Connect an API provider in Settings first.".into(),
        );
    }
    let has_profile = state
        .api_config
        .lock()
        .expect("api_config mutex")
        .active_profile()
        .is_some();
    // Dev-only bypass (2026-08-08): `npm run dev` / `cargo tauri dev` run with
    // no reachable API (the browser dev context can't push an API connection),
    // which otherwise locks Fable behind the title gate + the four `require_
    // api_for_fable` call sites. In a DEBUG build only, skip the check so the
    // UI can be edited / narrated locally for development. `cfg!` is a
    // compile-time literal → this branch is compiled out entirely in the
    // release build (`cargo build --release`), so the 2026-08-07 API-only
    // override + all four gates behave identically in shipped `wupi.exe`.
    if cfg!(debug_assertions) {
        return Ok(());
    }
    if !has_profile {
        return Err(
            "Fable requires an active API connection to narrate. Connect an API provider in Settings first.".into(),
        );
    }
    Ok(())
}

/// Mid-session API-failure handler (2026-08-07 architectural override).
///
/// Fable narration is API-only; there is no local-narrator fallback. When the
/// API stream fails (or no active profile is present despite the guards), this:
///   1. clears the in-flight cancel slot,
///   2. writes the reserved `autosave` slot so the player never loses progress
///      to an API outage (best-effort, fire-and-forget — never blocks),
///   3. emits an `{ type: "api_lost", message }` event so the frontend locks
///      the composer with the red "API LOST CONNECTION" state + the player can
///      reconnect via Settings and retry,
///   4. returns `Ok(())` — the turn aborts WITHOUT appending a phantom
///      assistant turn or archiving (there is no narrator prose to record).
/// The user's turn stays in the composer; the frontend retries on reconnect.
async fn emit_fable_api_lost(
    on_event: &tauri::ipc::Channel<serde_json::Value>,
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    message: &str,
) -> Result<(), String> {
    // Clear the cancel slot (mirror the normal turn-finalize cleanup).
    {
        let mut slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
        *slot = None;
    }
    // Autosave (best-effort) — identical to the normal turn-finalize autosave.
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
                Ok(Ok(_)) => tracing::debug!("autosave (api_lost): ok"),
                Ok(Err(e)) => tracing::warn!(error = %format!("{e}"), "autosave (api_lost) write failed"),
                Err(e) => tracing::warn!(error = %format!("{e}"), "autosave (api_lost) join failed"),
            }
        });
    }
    on_event
        .send(serde_json::json!({
            "type": "api_lost",
            "message": message,
        }))
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Acquire the Fable lease (v0.6.4 VRAM swap-lock) + spawn-or-reuse the
/// resident FableEngine under it. Extracted from `fable_send` (2026-08-14)
/// so the edit-retrack path (`edit_message`) reaches the local tracker
/// through the SAME lease + spawn-or-reuse logic — every context-creation
/// site routes through the lease (the §2C boot-bypass lesson).
///
/// The lease evicts any resident chat/schema context (synchronous `.join`,
/// VRAM freed) BEFORE the engine allocates — the load-bearing fix for the
/// 2026-07-26 freeze (4 contexts couldn't co-reside on 12GB). If a prior
/// fable turn left the engine resident (same-role reuse), this is a fast
/// no-op. Returns the lease guard (the caller holds it for the duration of
/// its local-model work; drop marks the slot free) + the engine handle.
async fn acquire_fable_engine_leased(
    state: &AppState,
    app: &tauri::AppHandle,
) -> Result<(context_swap::LeaseGuard, Arc<fable_engine::FableEngine>), String> {
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
                // `pending_model_path` is the normal source (stashed by
                // setup() at boot, re-stashed by enter_fable_session).
                // BUT boot_load_model's `.take()` consumes it after the chat
                // model spawns — so by the time the first fable turn fires,
                // the slot is often empty (the path was stashed BEFORE boot
                // consumed it; enter_fable_session's `if g.is_none()` preserve
                // check then can't refill from a still-Some slot). Mirror
                // boot_load_model's re-scan fallback (lib.rs:1697-1703): if
                // the slot is None, re-resolve from disk. FableEngine's
                // init_runtime prefers shared_model() anyway (set once at
                // boot, never cleared) — the path is only a defensive
                // fallback for the API-only-with-no-local edge case.
                let path = state
                    .pending_model_path
                    .lock()
                    .expect("pending_model_path mutex")
                    .clone()
                    .or_else(|| {
                        tracing::info!(
                            "fable spawn: pending_model_path was None — re-scanning disk (boot_load_model consumed it)"
                        );
                        resolve_model_path(app)
                    })
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
    Ok((lease, engine))
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
    regenerate: Option<bool>,
    #[allow(non_snake_case)]
    // `reroll: Some(true)` (2026-07-29, swipeable variants): set by the
    // frontend after a reroll. The last message is an assistant turn whose
    // current content we want to KEEP as a swipeable sibling while generating
    // a fresh variant. Distinct from `regenerate` (which assumes the mutation
    // left a user tail): here the assistant message STAYS, stashed into
    // variants, and we regenerate from the user turn BEFORE it.
    reroll: Option<bool>,
) -> Result<(), String> {
    tracing::info!(?text, regenerate, reroll, "fable_send");
    // `regenerate: Some(true)` is set by the frontend after a rewind-and-edit
    // mutation: the user turn to reply to is already the last message in
    // `fable_session` (the mutation command left it there), so we SKIP pushing
    // a fresh user message and generate from the existing tail. The window
    // slice below picks it up unchanged.

    // TOOL EXCLUSION GUARD (v0.8): the narrator path NEVER gets tools. This is
    // enforced structurally — fable_send dispatches to the FableEngine (a
    // separate engine from the chat backend), which builds its prompt via
    // `build_narrator_prompt` (no tools parameter) and generates via
    // `FableEngine::generate_turn`. The chat-only `run_agent_loop` is never
    // called here. If a future refactor routes fable_send through the chat
    // backend, it MUST pass `Vec::new()` for tools — the narrator is creative
    // prose + bracket commands, never structured tool calls. The
    // `fable_send_never_includes_tools` test pins this invariant.
    debug_assert!(true, "narrator path: tools excluded by construction");

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

    // v0.6.4 VRAM swap-lock: acquire the Fable lease + spawn-or-reuse the
    // engine under it. Extracted into `acquire_fable_engine_leased` (2026-08-
    // 14) so the edit-retrack path (edit_message) reaches the local tracker
    // through the SAME lease — every local-model Fable consumer routes
    // through the lease (the §2C boot-bypass lesson).
    let (lease, engine) = acquire_fable_engine_leased(&state, &app).await?;
    // The lease is now held for the duration of this turn. It will be
    // released when `lease` drops at end of scope.
    let _ = &lease;
    // Context size for the API narrator path (the local FableEngine ignores it:
    // it clears KV per turn on its own fixed context).
    let context_size = state.settings.lock().expect("settings mutex").context_size;

    // Build the narrator system prompt from the card + current game schema.
    //
    // THREE Rust-authoritative engines fire HERE, inside the schema lock,
    // BEFORE the render (all pure-eval, then apply, then render — atomic):
    //
    // 1. Scene Pacing (Fable Seam #4 expansion, 2026-07-27): `scene_pacing::
    //    evaluate(text)` classifies the turn into Downtime/Exploration/Combat
    //    from three keyword-driven pillars (Spatial/Emotional/Kinetic). The
    //    mode drives narrator prose cadence (via the `<scene_pacing>` tag),
    //    the World Progression tick interval, AND the skill-check DC modifier.
    //    Computed FIRST so its DC modifier threads into the skill Referee.
    //
    // 2. Combat Referee (Fable Seam #7): `player_state::referee_evaluate`
    //    scans for combat/exertion keywords, rolls dice, mutates the canonical
    //    PlayerState. The injury flows into `<world_state>` as a hard fact.
    //
    // 3. Skill-Check Referee (anti-sycophancy core, 2026-07-27):
    //    `player_state::referee_evaluate_skill_checks` scans for non-combat
    //    risky actions (lockpick/sneak/persuade/...), rolls d20 vs DC
    //    (modified by the scene-pacing DC modifier from #1), and injects
    //    `[DIRECTIVE: ...]` lines into `<world_state>` that the narrator MUST
    //    obey. This is the sycophancy-killer: Rust decides the outcome, the
    //    LLM has no choice but to write prose that matches.
    //
    // We hold the lock across all three + the render so the persisted state
    // and the injected state are the SAME atomic snapshot — a concurrent
    // autosave can't tear them apart. The LLM does ZERO math.
    //
    // (0) Manual player action (§11.30 Left-Drawer HUD, 2026-08-02):
    // consumed + cleared atomically, ONE turn only. Armed by
    // `fable_player_action_set` when the player clicks Consume/Equip/Unequip
    // in the left-drawer inventory grid. Rendered as a new
    // `<player_action type="manual_override">` block at the top of
    // `assemble_narrator_skeleton` so the narrator acknowledges the tactile UI
    // action in this turn's prose. Taken BEFORE the schema lock so the consume
    // + the world_state render are atomic from the narrator's POV.
    let player_action = {
        let mut g = state.pending_player_action.lock().await;
        g.take()
    };

    let (world_state, pacing, mut turn_directives) = {
        let mut s = state.fable_schema.lock().await;

        // (1) Scene pacing FIRST — its DC modifier threads into (3).
        let pacing = scene_pacing::evaluate(&text);

        // Track whether anything mutated so we snapshot once for undo. All
        // three engines share a single history push (the snapshot captures
        // the pre-mutation state; one push per turn is the right granularity
        // for "undo this turn's world changes").
        let mut mutated = false;
        let mut undo_snapshot: Option<schema::WorldSchema> = None;

        // Persist the freshly-computed pacing so the narrator's NEXT turn
        // inherits it (soft memory; next non-neutral turn re-classifies).
        // This is a real mutation → snapshot.
        if s.scene_pacing != pacing {
            if undo_snapshot.is_none() {
                undo_snapshot = Some(s.clone());
            }
            s.scene_pacing = pacing;
            mutated = true;
        }

        // (1b) Phase 3 Slice 5 reconciliation (2026-07-28): evaluate every
        // tracked NPC's relationship for a pending transition. Hostility
        // triggers (betrayal/murder) bypass the gates + fire instantly;
        // affinity advances respect the dual time-floor + milestone gates.
        // Applied here so the render below reflects the canonical tiers +
        // the narrator sees the post-transition state. Snapshot once if any
        // transition lands. Uses the WorldClock's current minutes as "now".
        //
        // Two-pass: first collect which NPCs should transition (read-only
        // borrow of s.relationships), then apply (mutable borrow). Can't
        // snapshot s.clone() mid-iter_mut.
        let now_rel_minutes = s.world_clock.current_minutes;
        let registry = relationship::MilestoneRegistry::defaults();
        let pending_rel_transitions: Vec<(String, relationship::RelationshipTier, relationship::TransitionReason)> = s
            .relationships
            .iter()
            .filter_map(|(npc_id, rel_state)| {
                let outcome = relationship::evaluate_transition(rel_state, &registry, now_rel_minutes);
                if let relationship::TransitionOutcome::Transition { new_tier, reason } = outcome {
                    if rel_state.tier != new_tier {
                        return Some((npc_id.clone(), new_tier, reason));
                    }
                }
                None
            })
            .collect();
        if !pending_rel_transitions.is_empty() {
            if undo_snapshot.is_none() {
                undo_snapshot = Some(s.clone());
            }
            let mut rel_transitions_logged = Vec::with_capacity(pending_rel_transitions.len());
            for (npc_id, new_tier, reason) in &pending_rel_transitions {
                if let Some(rel_state) = s.relationships.get_mut(npc_id) {
                    relationship::apply_transition(
                        rel_state,
                        &relationship::TransitionOutcome::Transition {
                            new_tier: *new_tier,
                            reason: reason.clone(),
                        },
                        now_rel_minutes,
                    );
                    rel_transitions_logged.push(format!("{npc_id}→{:?}({:?})", new_tier, reason));
                }
            }
            mutated = true;
            tracing::info!(
                count = pending_rel_transitions.len(),
                transitions = ?rel_transitions_logged,
                "[rel] relationship transitions applied on render"
            );
        }

        // (2) Combat Referee. Slice 3 (2026-07-28): the outcome now carries a
        // `lethal: bool` + a `directive: String`. The directive is non-empty
        // only on a lethal blow; we capture it here + inject it into the
        // `<directives>` block below alongside any skill-check directives, so
        // the narrator sees a single coherent block of hard facts to obey.
        //
        // Slice 3 wiring (2026-07-28): select the attacker tier from any
        // `npc.*.tier` entity keys the card author declared (or the narrator
        // emitted). Falls back to Soldier (the v1 default) when no tier keys
        // exist — preserves the original severity distribution exactly.
        // Passes the real tier to referee_evaluate_with_tier so the severity
        // weights + lethality DC scale with the threat.
        let attacker_tier =
            player_state::select_attacker_tier_from_entities(&s.entities);
        let combat_directive: Option<String> = if let Some(outcome) =
            player_state::referee_evaluate_with_tier(&text, &s.player_state, attacker_tier)
        {
            tracing::info!(
                part = outcome.part.id(),
                state = ?outcome.new_state,
                stamina = ?outcome.stamina_after,
                lethal = outcome.lethal,
                attacker_tier = ?attacker_tier,
                "referee fired on combat/exertion keyword"
            );
            if undo_snapshot.is_none() {
                undo_snapshot = Some(s.clone());
            }
            player_state::apply_outcome(&mut s.player_state, &outcome);
            mutated = true;
            if outcome.lethal && !outcome.directive.is_empty() {
                Some(outcome.directive)
            } else {
                None
            }
        } else {
            None
        };

        // (3) Skill-check Referee. Turn-scoped — NOT persisted (no schema
        // mutation, no undo push). The directives append to the rendered
        // world_state so they ride inside the existing `<world_state>` tag
        // the narrator already treats as hard fact.
        let skills = player_state::referee_evaluate_skill_checks(&text, pacing.mode.dc_modifier());

        // (3a) Phase 4 §11.44 (Component 1): Disguise Referee — the Rust-side
        // gate. Pure fn, no mutation (mirrors the skill-check Referee's
        // contract). Returns None when there's no active disguise tag OR when
        // an Elite+ NPC is present (the normal skill-check Referee handles
        // that Deception roll with no disguise framing). Otherwise either
        // AutoPass (low-tier + confident walk-by) or Scrutinized (suspicious
        // behavior revoked the auto-pass → a Deception roll fires here so the
        // directive carries the disguise context, not a bare skill line).
        let disguise_directive = player_state::evaluate_disguise_gate(
            &text,
            &s.status_tags,
            &s.entities,
            pacing.mode.dc_modifier(),
        );

        // (3b) Combat lethality directive (Slice 3): same injection path as
        // skill checks — the narrator sees ONE `<directives>` block with all
        // hard facts for the turn (combat lethality + skill outcomes).
        // (3c) Phase 3 Slice 6 wiring (2026-07-28): off-screen task directives
        // drained from `pending_tick_directives` (filled by the World
        // Progression tick) join the same block — they're also hard facts the
        // narrator must obey ("marcus returned from scouting — failure").
        let tick_directives: Vec<String> = {
            let mut td = state.pending_tick_directives.lock().await;
            std::mem::take(&mut *td)
        };

        // Assemble the turn-scoped directives in priority order: lethality
        // first (most consequential), then the disguise outcome (scene-
        // establishing — "your disguise holds" / "scrutinized: FAIL"), then
        // skill checks, then off-screen tick directives (background facts).
        // §11.41 (2026-07-28): these are returned alongside the world-state
        // render so the API branch can RE-render world-state after the
        // tracker stage mutates the schema — the Referees run ONCE (pre-
        // tracker), not twice (several mutate state), so the same directive
        // lines ride both the tracker prompt AND the post-tracker API-
        // narrator prompt.
        let mut turn_directives: Vec<String> = Vec::new();
        if !skills.is_empty() {
            tracing::info!(
                count = skills.len(),
                mode = ?pacing.mode,
                "skill-check referee fired"
            );
        }
        if combat_directive.is_some() {
            tracing::info!("combat lethality directive injected");
        }
        if disguise_directive.is_some() {
            tracing::info!("disguise gate directive injected");
        }
        if !tick_directives.is_empty() {
            tracing::info!(
                count = tick_directives.len(),
                "off-screen task directives injected (from world progression tick)"
            );
        }
        if let Some(cd) = &combat_directive {
            turn_directives.push(cd.clone());
        }
        if let Some(dd) = &disguise_directive {
            turn_directives.push(dd.render());
        }
        for sc in &skills {
            turn_directives.push(sc.directive.clone());
        }
        for td in &tick_directives {
            turn_directives.push(td.clone());
        }

        // Render the canonical world-state snapshot (now reflects any injury +
        // any relationship transitions above). The render + the turn-scoped
        // directives block are factored into render_fable_world_state so the
        // API branch can re-invoke it after the tracker stage mutates the
        // schema (§11.41 DM / Voice-Actor split).
        let rendered = render_fable_world_state(&s, &turn_directives);

        // Push the single undo snapshot (if any engine mutated). Done after
        // the lock release to keep lock-ordering uniform.
        let _ = mutated; // (kept for clarity; undo_snapshot.is_some() is the real gate)
        if let Some(snap) = undo_snapshot {
            drop(s);
            push_fable_history_snapshot(&state, snap).await;
        }

        let world_state_opt = if rendered.trim().is_empty() { None } else { Some(rendered) };
        (world_state_opt, pacing, turn_directives)
    };

    // Fable codex retrieval (2026-07-29, the core fix): the narrator now
    // retrieves from the unified fable.codex partition (the deep playbook —
    // bracket-command reference, narrative discipline, common errors) fused
    // with the active card's own lore, via search_fable_visible. Mirrors the
    // proven chat_send pattern at lib.rs:2750. Empty-skip on zero hits = zero
    // cost when nothing clears the cosine floor. This is what makes the
    // Phase 3 prompt distillation safe: the offloaded detail lives in the
    // codex and arrives on semantic match the instant it's relevant, instead
    // of bloating the system prompt every turn. (FableEngine clears KV every
    // turn — no eager-prefill invariant to protect here, so the block can go
    // straight into the system prompt.)
    let memory_block: Option<String> = match state.memory.get() {
        Some(engine) => match engine.search_fable_visible(&text, &card.id, 5, None).await {
            Ok(hits) if !hits.is_empty() => Some(memory::render_memory_block(&hits)),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %format!("{e}"), "fable codex retrieval failed; injecting nothing");
                None
            }
        },
        None => {
            tracing::trace!("memory engine not initialized; skipping fable codex retrieval");
            None
        }
    };

    // The authored narrator + agent prose, loaded once in setup() (cache-once
    // pattern). Passed to the build_* prompt builders so authored voice comes
    // from fable.prompt, not from Rust. Always Some after setup (loader falls
    // back to a placeholder).
    let fable_prompts = state
        .fable_prompts
        .get()
        .expect("fable_prompts is set in setup()");

    let system_prompt = build_narrator_system_prompt(
        fable_prompts,
        &card,
        world_state.as_deref(),
        pacing,
        player_action.as_deref(),
        memory_block.as_deref(),
    );

    // Append the user turn to the per-card game conversation, then window the
    // visible history. Same sliding-window strategy as chat_send's VISIBLE_WINDOW
    // (§2I M2): old turns drop from the prompt and the fable codex retrieval
    // (wired 2026-07-29, the block above) backfills relevant context on
    // semantic match — so the prompt stays small (~5KB not ~80KB) WITHOUT
    // losing continuity. The full conversation is persisted on fable_end so
    // games resume across reboots.
    //
    // v0.6.3: window doubles when the API is the active chat source, matching
    // chat_send's 4 → 12. The cloud model has the context budget and the local
    // model stays hot as the silent agent doing schema tracking.
    //
    // LOCAL window = 8 *messages* (4 beats — 4 user actions + 4 narrator
    // replies). Token math (post the 2026-07-29 prompt-distillation scrub):
    // narrator system prompt ~2050 tok (narrator_core ~830 + BRACKET_PROTOCOL
    // ~400 + scenario/player/world_state/anchors ~820) + 8 messages (~1200 tok
    // at ~150/msg) + 1024-token gen reserve ≈ 4274. FABLE_CTX=4096 leaves a
    // ~180 tok shortfall on the rare maximal turn — the engine's
    // truncate_from_front guard drops the oldest turn(s) to fit (its designed
    // purpose: a rare safety valve, not a routine crutch). Before the scrub
    // the prompt alone was ~5000 tok (the two anti-bias lectures + full
    // BRACKET_PROTOCOL + COMMON MISTAKES), forcing the window 8→6 and
    // FABLE_MAX_TOKENS 1024→512 just to fit — a shortcut chain that amputated
    // generation budget + memory to make room for bloat. The scrub recovered
    // ~3000 tok and restored both. The distilled declarative-law prompt
    // produces punchy 2-4 paragraph beats naturally; the §11.41 DRY sampler
    // + post-gen truncator are the mechanical loop backstop.
    //
    // v0.7: the API window was cut 16 → 12 (6 beats). 12 narrator messages
    // + the ~2050-tok narrator prompt + generation fits comfortably inside
    // the API profile's max_context (default 8192), and 12 matches the chat
    // API window for a consistent "6 beats of recent history" feel across
    // both surfaces.
    // Architectural override (2026-08-07): Fable narration is API-ONLY. The
    // local 12B never narrates (Gemma 12B cannot roleplay without stat-spam —
    // it compulsively emits tracking brackets in prose). The frontend launch
    // gate (`launchFable` → `model_source_get`) blocks entering Fable without
    // an active API profile; this is the backend backstop so a stale frontend
    // or a direct IPC call can never start a narration turn in a non-API state.
    require_api_for_fable(&state)?;
    let fable_visible_window = settings::WINDOW_API_FABLE;
    let regenerate = regenerate.unwrap_or(false);
    let reroll = reroll.unwrap_or(false);
    {
        let mut gs = state.fable_session.lock().await;
        if reroll {
            // Reroll path (variant↔schema binding, 2026-08-11): regenerate a
            // FRESH variant of the trailing assistant turn. The bookkeeping —
            // stashing the old prose as a swipeable sibling, advancing
            // active_idx, and appending the new variant's post-tracker schema
            // — is all handled by `push_variant_with_schema` AFTER generation
            // (below). We do NOT pop the assistant message; it stays + the
            // window slice below excludes it (last message dropped) so the
            // model regenerates from the preceding user turn. The schema revert
            // (so the re-track doesn't double-mutate on top of the prior roll)
            // + the variant_schemas seed happen in the pre-Stage-1 block below.
            let last_is_assistant = gs
                .messages
                .last()
                .map(|m| m.role == session::Role::Assistant)
                .unwrap_or(false);
            if !last_is_assistant {
                return Err(
                    "fable_send: reroll=true but the last message is not an assistant turn".into(),
                );
            }
        } else if regenerate {
            // Re-generation path: the user turn to reply to is already the
            // last message (a mutation command — rewind_and_edit_user — left
            // it there). Bail if the contract is violated so we don't generate
            // from a stale or assistant tail silently.
            if !gs.last_message_is_user() {
                return Err(
                    "fable_send: regenerate=true but the last message is not a user turn".into(),
                );
            }
        } else {
            gs.add_message(session::Role::User, text.clone());
        }
    }

    // Build windowed prompts. Two windows are needed (2026-08-08):
    //   • `window` (WINDOW_API_FABLE = 16) — the API narrator sees the full
    //     narrative history (it has the 16k context budget for it).
    //   • `tracker_window` (WINDOW_TRACKER = 2) — the local tracker sees only
    //     the last 1 turn (player action + preceding narrator response). It
    //     does NOT re-read history: it relies on the schema delta (incremental
    //     state) + Rust state as the authority. Keeping its window to 1 turn
    //     (the AGENT directive's "this turn") keeps the prompt lean so the
    //     bracket protocol survives the truncation guard.
    // Same Gemma4 `<|turn>` protocol the chat path uses (assistant → "model").
    // Single-shot prefill (FableEngine clears KV every turn), so cleaned
    // content is fine (no cache-coherent raw_output re-render needed).
    let (window, tracker_window): (Vec<session::Message>, Vec<session::Message>) = {
        let gs = state.fable_session.lock().await;
        let msgs = &gs.messages;
        // Reroll: the trailing assistant message has empty content (stashed into
        // variants above). Exclude it from the prompt window so the model
        // regenerates from the preceding user turn, not from an empty assistant
        // turn. We slice to `len - 1` in that case.
        let end = if reroll && msgs.last().map(|m| m.role == session::Role::Assistant).unwrap_or(false) {
            msgs.len().saturating_sub(1)
        } else {
            msgs.len()
        };
        let narr_start = end.saturating_sub(fable_visible_window);
        let track_start = end.saturating_sub(settings::WINDOW_TRACKER);
        // Cap assistant-message prose in the TRACKER window (2026-08-10, T52
        // overflow fix). See `cap_assistant_prose` + `TRACKER_ASSISTANT_CHAR_CAP`.
        // The narrator window (16k API budget) is uncapped.
        let tracker_window = cap_assistant_prose(
            msgs[track_start..end].to_vec(),
            settings::TRACKER_ASSISTANT_CHAR_CAP,
        );
        (msgs[narr_start..end].to_vec(), tracker_window)
    };

    // Variant↔schema binding (2026-08-11): capture the pre-turn base schema
    // (the world the player acted against) BEFORE the tracker mutates it. For
    // a reroll, FIRST seed `variant_schemas` with the active variant's schema
    // (the current live schema) when this is the turn's first reroll, THEN
    // revert the live schema to the message's stored `base_schema` so the re-
    // track doesn't double-mutate on top of the prior roll (the fix). The
    // captured `base_schema_snapshot` is stored on the message after generation
    // (normal turn); rerolls reuse the base set by the first generation. Locks
    // are taken in separate scopes (schema, then session) — never nested.
    let base_schema_snapshot: schema::WorldSchema = {
        if reroll {
            // (a) Seed variant_schemas with the active variant's schema when
            //     this is the first reroll of the turn (empty for a turn made
            //     before this feature, OR a fresh turn whose post-tracker
            //     snapshot is pushed after generation below).
            {
                let live = state.fable_schema.lock().await.clone();
                let mut gs = state.fable_session.lock().await;
                if let Some(m) = gs.messages.last_mut() {
                    if m.role == session::Role::Assistant && m.variant_schemas.is_empty() {
                        m.variant_schemas.push(live);
                    }
                }
            }
            // (b) Revert the live schema to the message's stored base_schema.
            //     Legacy messages with no base_schema can't revert — they re-
            //     track on top (the old behavior) until the session refreshes.
            {
                let base_opt = {
                    let gs = state.fable_session.lock().await;
                    gs.messages
                        .last()
                        .filter(|m| m.role == session::Role::Assistant)
                        .and_then(|m| m.base_schema.clone())
                };
                if let Some(b) = base_opt {
                    *state.fable_schema.lock().await = b;
                }
            }
        }
        state.fable_schema.lock().await.clone()
    };

    // Streaming callback wraps the Channel send.
    let on_chunk: llm::ChunkFn = Arc::new({
        let on_event = on_event.clone();
        move |piece: &str| {
            let _ = on_event.send(serde_json::json!({ "type": "chunk", "text": piece }));
        }
    });

    // v0.6.3 API routing for the narrator. §11.41 (2026-07-28, DM / Voice-Actor
    // split): a narrator turn is TWO stages.
    //
    //   Stage 1 — TRACKER (local 12B): runs FIRST. Emits bracket/JSON tracking
    //   commands that decide the mechanical truth of the turn (time advanced,
    //   buffs gained, milestones hit, tasks queued). NO prose — its output is
    //   hidden from the user via a no-op on_chunk. The brackets are applied to
    //   fable_schema via the existing apply_phase3_bracket_commands +
    //   apply_time_command_and_maybe_tick pipeline. The tracker ALWAYS thinks
    //   (`<|think|>` is injected in `build_narrator_prompt`).
    //
    //   Stage 2 — NARRATOR (API): runs SECOND. Gets a prose-only system prompt
    //   (build_api_narrator_system_prompt — no BRACKET_PROTOCOL at all) + the
    //   authoritative POST-tracker state as <world_state>. It is a blindfolded
    //   storyteller: it narrates what Rust tells it happened, never inventing
    //   outcomes the engine didn't track. Its prose streams live to the user
    //   via the real on_chunk. The API path uses OpenAI format, so it NEVER
    //   injects `<|think|>` — API narration never thinks.
    //
    // Architectural override (2026-08-07): this API path is now the ONLY path.
    // The local 12B never narrates. On any API failure there is NO local
    // fallback — instead the turn aborts, an `api_lost` event is emitted, the
    // autosave runs (so progress is never lost), and the frontend locks the
    // composer until the API is reconnected. The LOCAL two-pass path (local
    // model narrating then tracking) and the local-narrator fallback were
    // deleted here.
    //
    // The reply shape ({ content, raw }) is normalized to FableReply
    // ({ raw_output, error, cancelled }) so the downstream bracket-parsing +
    // archival path is identical. The narrator emits no brackets (the tracker
    // does), so the final bracket parse is a no-op for state (the tracker
    // already applied them) but still cleans/strips the prose.
    let reply: fable_engine::FableReply = {
        // ---- Stage 1: TRACKER (local 12B, hidden) ----
        // The tracker gets the Tracker prompt (build_narrator_system_prompt —
        // narration + brackets), but with the PRE-tracker world_state (the
        // authoritative state before this turn's brackets land). Its prose is
        // discarded; only its brackets are
        // applied. The no-op on_chunk ensures the tracker's prose/bracket
        // output never reaches the frontend.
        //
        // **Local-model turn lock (2026-08-08):** the tracker decode + bracket
        // application run under the process-wide `local_model_lock` so they
        // never overlap a concurrent `chat_send` or schema-delta decode on the
        // local 12B. Whichever local consumer acquires first runs to
        // completion; the others queue. The guard drops at the end of this
        // block — BEFORE Stage 2 (the API narrator never takes it).
        let noop_chunk: llm::ChunkFn = Arc::new(|_: &str| {});
        // The tracker uses the tight 1-turn window (WINDOW_TRACKER = 2), NOT
        // the narrator's 16-message window. It relies on the schema delta +
        // Rust state, not re-read history (2026-08-10).
        let tracker_prompt = build_narrator_prompt(&system_prompt, &tracker_window);
        tracing::info!("fable_send: API mode — tracker stage (local) starting");
        let _tracker_model_guard = state.local_model_lock.lock().await;
        let tracker_reply_opt: Option<fable_engine::FableReply> = match engine
            .request_turn(tracker_prompt, noop_chunk.clone(), cancel.clone(), true)
        {
            Ok(reply_rx) => {
                match tokio::task::spawn_blocking(move || reply_rx.recv()).await {
                    Ok(Ok(r)) => Some(r),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "fable_send: tracker channel recv failed; skipping tracker stage");
                        None
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "fable_send: tracker join failed; skipping tracker stage");
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "fable_send: tracker request_turn failed; skipping tracker stage");
                None
            }
        };

        // Apply the tracker's brackets to fable_schema (if it produced any).
        // Reuses the existing apply pipeline verbatim — zero new apply code.
        // The tracker's prose is discarded (we keep only parsed.commands).
        if let Some(tracker_reply) = &tracker_reply_opt {
            if tracker_reply.error.is_empty() {
                let cleaned_tracker = schema::extract_reply_channel(&tracker_reply.raw_output);
                // TEMP DIAGNOSTIC (2026-08-09): log the tracker's RAW + CLEANED
                // output so we can see what the local 12B actually emits when
                // brackets aren't firing. Remove after diagnosis.
                tracing::info!(
                    raw_len = tracker_reply.raw_output.len(),
                    cleaned_len = cleaned_tracker.len(),
                    "TRACKER DIAGNOSTIC — raw_output (first 800 chars):\n{}",
                    &tracker_reply.raw_output.chars().take(800).collect::<String>()
                );
                tracing::info!(
                    "TRACKER DIAGNOSTIC — cleaned/extract_reply_channel (first 800 chars):\n{}",
                    cleaned_tracker.chars().take(800).collect::<String>()
                );
                let tracker_parsed = bracket_parser::parse(&cleaned_tracker);
                if !tracker_parsed.commands.is_empty() {
                    tracing::info!(
                        cmd_count = tracker_parsed.commands.len(),
                        "fable_send: tracker emitted bracket commands; applying"
                    );
                    // Emit the tracker's SCHEMA-TRACKING scene_events to the
                    // frontend (Time / Effect / Milestone / Task — these are
                    // state changes the UI may want to surface as
                    // notifications: "Time advanced to day 3", "Berserk Rage
                    // fades"). The UI-EFFECT-ONLY commands (CharacterTurn /
                    // Object / Fx) are deliberately DROPPED here in API mode:
                    // the API narrator (stage 2, below) produces its own
                    // — and VASTLY better — NPC dialogue + scene description
                    // in prose, so emitting the tracker's CharacterTurn
                    // would either (a) duplicate the narrator's dialogue
                    // or (b) leak the local 12B's malformed bracket
                    // arguments straight to the user (the §11.43.A bug,
                    // found 2026-07-28: the local 12B loops on `(player)`
                    // template-placeholder syntax inside character_turn
                    // lines, producing garbage like "the (player) is
                    // (player) at the (player)"). The apply_phase3 helpers
                    // still run on ALL commands below — only the frontend
                    // notification is gated.
                    for cmd in &tracker_parsed.commands {
                        let is_ui_only = matches!(
                            cmd,
                            bracket_parser::BracketCommand::CharacterTurn { .. }
                                | bracket_parser::BracketCommand::Object { .. }
                                | bracket_parser::BracketCommand::Fx { .. }
                        );
                        if is_ui_only {
                            continue;
                        }
                        let _ = on_event.send(serde_json::json!({
                            "type": "scene_event",
                            "command": cmd,
                            "source": "tracker",
                        }));
                    }
                    // Capture the reject directives (Component 3, 2026-07-28):
                    // e.g. "[TRAVEL] rejected — non-adjacent". Merge into
                    // turn_directives so the API narrator (stage 2) sees them
                    // in its `<directives>` block + obeys ("the move is not
                    // possible from here; <exits>"). Listed LAST in the block
                    // (after lethality / disguise / skill / tick directives)
                    // — a rejected travel is the least consequential of the
                    // hard facts, and the narrator has already seen the legal
                    // `location:` line + exits in `<world_state>`.
                    let (_, travel_rejects) =
                        apply_phase3_bracket_commands(&tracker_parsed, &state).await;
                    turn_directives.extend(travel_rejects);
                    apply_time_command_and_maybe_tick(&tracker_parsed, &state).await;
                } else {
                    tracing::info!("fable_send: tracker produced no bracket commands");
                }
            } else {
                tracing::warn!(
                    error = %tracker_reply.error,
                    "fable_send: tracker stage errored; proceeding to API narrator with pre-tracker state"
                );
            }
        }

        // The tracker's local-model work is done — release the turn lock NOW
        // so a concurrent chat_send / schema-delta can use the local 12B
        // while Stage 2 (the API narrator, below) streams over HTTP. The API
        // never touches the local model, so holding the lock across it would
        // needlessly block every Wupi-drawer message for the entire narrator
        // generation. Drop explicitly before the re-render below.
        drop(_tracker_model_guard);

        // Re-render the authoritative world-state for the API narrator. This
        // reflects the tracker's bracket mutations (time advanced, buffs
        // added, milestones recorded, tasks queued). The SAME turn_directives
        // (combat lethality + skill checks + tick directives — assembled
        // pre-tracker, Referees run once) ride this render so the narrator
        // sees one coherent block of hard facts.
        let narrator_world_state: Option<String> = {
            let s = state.fable_schema.lock().await;
            let rendered = render_fable_world_state(&s, &turn_directives);
            if rendered.trim().is_empty() { None } else { Some(rendered) }
        };

        // Build the prose-only API narrator prompt (no BRACKET_PROTOCOL).
        // memory_block is shared with the tracker stage — same turn, same
        // player text, same active card, so the one retrieval query above
        // serves both Fable stages (the GM/Narrator unification principle
        // extended to the two-stage turn: one codex, one query).
        let narrator_system_prompt = build_api_narrator_system_prompt(
            fable_prompts,
            &card,
            narrator_world_state.as_deref(),
            pacing,
            player_action.as_deref(),
            memory_block.as_deref(),
        );

        // ---- Stage 2: NARRATOR (API, or LOCAL in dev with no API) ----
        // The launch gate + the top-of-fable_send guard guarantee an active
        // API profile here, so the `None` profile case is purely defensive.
        let profile = {
            let cfg = state.api_config.lock().expect("api_config mutex");
            cfg.active_profile().cloned()
        };

        // Dev-only local-narrator bypass (2026-08-08): when running under
        // `cargo tauri dev` (a DEBUG build) AND no API profile is connected,
        // route the narrator turn to the already-resident local FableEngine
        // instead of HttpBackend. The browser dev context can't push an API
        // connection, so without this the Stage-2 HTTP call would fail →
        // `api_lost` → composer lockout, making the Fable UI uneditable in
        // dev. `cfg!(debug_assertions)` is a compile-time literal `false` in
        // `cargo build --release`, so the optimizer dead-code-eliminates this
        // arm entirely; the shipped wupi.exe ALWAYS takes the API path below
        // exactly as the 2026-08-07 override mandates (no local-narrator
        // fallback in production). Using the `cfg!()` macro (not `#[cfg]`)
        // keeps the dev arm type-checked in release builds too.
        //
        // The local model will produce stat-spammy prose (the override's
        // exact reason for API-only) — acceptable for UI editing / bracket /
        // save-state dev work. The engine is already spawned above (Stage 1)
        // and clears KV every turn, so a second request_turn is a fresh
        // prefill with no cache concern. The tracker already applied its
        // brackets; any brackets this pass emits are harmless duplicates
        // (same contract as the API path: narrator brackets are a no-op for
        // state, the tracker owns mechanics). tracker_mode=false → creative
        // sampler for more natural prose to look at while styling.
        if cfg!(debug_assertions) && profile.is_none() {
            tracing::info!(
                "fable_send: DEV MODE — no API profile; narrating via local FableEngine (stat-spammy)"
            );
            let narrator_prompt = build_narrator_prompt(&narrator_system_prompt, &window);
            let dev_reply: fable_engine::FableReply = match engine.request_turn(
                narrator_prompt,
                on_chunk.clone(),
                cancel.clone(),
                false, // tracker_mode=false → creative sampler
            ) {
                Ok(reply_rx) => match tokio::task::spawn_blocking(move || reply_rx.recv()).await {
                    Ok(Ok(r)) => fable_engine::FableReply {
                        raw_output: r.raw_output,
                        error: r.error,
                        cancelled: r.cancelled,
                    },
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "fable_send: DEV local narrator channel recv failed");
                        return emit_fable_api_lost(
                            &on_event, &app, &state,
                            "Dev local narrator failed (see logs).",
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "fable_send: DEV local narrator join failed");
                        return emit_fable_api_lost(
                            &on_event, &app, &state,
                            "Dev local narrator failed (see logs).",
                        )
                        .await;
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "fable_send: DEV local narrator request_turn failed");
                    return emit_fable_api_lost(
                        &on_event, &app, &state,
                        "Dev local narrator failed (see logs).",
                    )
                    .await;
                }
            };
            dev_reply
        } else {
            let Some(profile) = profile else {
                // Defensive: no active profile despite the guards (e.g. the
                // profile was deleted mid-turn). No local fallback — emit
                // api_lost, autosave, and abort the turn.
                return emit_fable_api_lost(&on_event, &app, &state, "No active API profile.").await;
            };
            // Re-render the windowed history as flat ApiMessages for the HTTP path
            // (system + windowed turns; the API folds memory + world_state into
            // the system message itself).
            let mut api_msgs: Vec<session::ApiMessage> =
            Vec::with_capacity(window.len() + 1);
        api_msgs.push(session::ApiMessage {
            role: "system".into(),
            content: narrator_system_prompt.trim().to_string(),
            raw_output: String::new(),
        });
        for m in &window {
            let role = match m.role {
                // **2026-07-27 Bug #4 fix:** the OpenAI /chat/completions
                // standard role for assistant turns is `"assistant"`, NOT
                // `"model"`. The prior `"model"` mapping (a Gemini/Google
                // convention) was rejected by z.ai / GLM-5.2 with HTTP error
                // code 1214 "Incorrect role information". Verified live:
                // GLM accepts `"assistant"`, rejects `"model"`.
                session::Role::Assistant => "assistant",
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
            Ok(out) => fable_engine::FableReply {
                // The API has no Gemma4 protocol markers, so raw_output is
                // just the content. extract_reply_channel downstream is a
                // no-op on marker-free text (rsplit_once finds no
                // "<channel|>", returns the input unchanged).
                raw_output: if out.raw.is_empty() { out.content } else { out.raw },
                error: String::new(),
                cancelled: false,
            },
            Err(e) => {
                // Architectural override (2026-08-07): NO local narration
                // fallback. The API is the sole narrator. On failure we save
                // progress, tell the frontend to lock the composer, and abort
                // the turn. The player reconnects via Settings and retries.
                tracing::warn!(error = %e, "fable_send: API narrator failed; emitting api_lost (no local fallback)");
                return emit_fable_api_lost(
                    &on_event,
                    &app,
                    &state,
                    "The API connection was lost.",
                )
                .await;
            }
        }
        }  // close else (API path)
    };

    // Clear the cancel slot now that the turn is done.
    {
        let mut slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
        *slot = None;
    }

    // Abort check (Stage 3, 2026-08-11): if `fable_interrupt_reroll` requested
    // an abort (the user re-pressed › mid-reroll to abandon this roll), discard
    // it entirely — revert the schema to the pre-turn `base_schema_snapshot`
    // (undoing this roll's tracker mutations), SKIP the variant install, emit
    // `cancelled`, + return. The frontend arms a deferred reroll that fires
    // when it sees the `cancelled` event, so the net effect is
    // "stop + undo + fresh roll." Read-and-reset (swap) so one request is
    // consumed once. This is distinct from a soft `fable_stop` cancel
    // (reply.cancelled without the abort flag), which keeps its partial prose.
    if state.fable_abort_requested.swap(false, std::sync::atomic::Ordering::Relaxed) {
        *state.fable_schema.lock().await = base_schema_snapshot.clone();
        // Defensive: for a normal turn (not reroll) the user message was added
        // at turn start — pop it so the conversation reverts to its pre-turn
        // tail. The frontend only triggers abort for the reroll case, so this
        // branch is normally dead; kept so an unexpected abort never leaves a
        // dangling user turn.
        if !reroll {
            let mut gs = state.fable_session.lock().await;
            if gs.messages.last().map(|m| m.role == session::Role::User).unwrap_or(false) {
                gs.messages.pop();
            }
        }
        let _ = on_event.send(serde_json::json!({ "type": "cancelled" }));
        return Ok(());
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
    // Extract the thought channel (if any) from the raw output, parallel to
    // Architectural override (2026-08-07): the narrator is the API, which uses
    // OpenAI format and never emits a Gemma4 thought channel, so there is no
    // narrator reasoning to extract or carry on the wire. Keep the field as an
    // empty string for payload-shape compatibility with the frontend + history
    // reload (the player-facing reasoning UI has been removed regardless).
    let narrator_reasoning = String::new();
    let mut parsed = bracket_parser::parse(&cleaned_raw);
    // Post-generation repetition firewall (§11.40.E follow-up fix 2026-07-28):
    // the deterministic backstop for the smuggler-loop / tail-repetition
    // failure mode. Runs AFTER bracket_parser::parse so the model's bracket
    // commands are preserved (commands are orthogonal to prose repetition;
    // truncating cleaned_raw before parse would risk eating bracket bodies
    // that legitimately repeat NPC names). Operates on the prose only:
    // conservative — requires a 4+ word sequence repeated 3+ times back-to-
    // back, so legitimate rhetorical anaphora (2x) and short dialogue
    // echoes (<4 words) are preserved. On a qualifying loop, keeps one
    // clean instance + drops everything after (a detected loop signals
    // model breakdown). The complementary DRY sampler stage in both
    // engines handles short token-sequence repetition at generation time;
    // this fn is the firewall for longer 4+-word loops that slip past it.
    // Protects all three downstream consumers of parsed.prose: the bracket-
    // command extraction (6131, commands unaffected), the memory archive
    // (6163), and the stored assistant turn (6188). reply.raw_output stays
    // untruncated as a forensic record (FableEngine clears KV every turn,
    // so it's never re-tokenized — no cache-coherence concern).
    parsed.prose = stream_filter::truncate_repetition(&parsed.prose);
    for cmd in &parsed.commands {
        on_event
            .send(serde_json::json!({ "type": "scene_event", "command": cmd }))
            .map_err(|e| e.to_string())?;
    }

    // Apply any [TIME ...] bracket command + check the World Progression tick
    // gate (Fable Seam #4, 2026-07-27). If the narrator advanced the in-world
    // clock past the configured interval, fire an off-screen simulation pass
    // against `fable_schema` so the world moves independently of the player's
    // bubble. Best-effort: errors logged + dropped (the gameplay loop never
    // blocks on a tick). The fail-queue contract still applies — retryable
    // failures land in `failed_progression_queue` for the next tick.
    //
    // The progression pass acquires `ContextRole::Schema` via the swap-lock,
    // which will wait for THIS fable lease to drop on fable_send return before
    // the schema engine can spawn. The result is applied to `fable_schema`
    // + visible on the NEXT narrator turn (off-screen sim is decoupled from
    // the just-completed narrator turn by design).
    //
    // Phase 3 bracket commands ([EFFECT], [MILESTONE], [TASK]) are applied
    // FIRST so the tick sees any freshly-queued tasks / freshly-recorded
    // milestones when it fires (parallel ordering to the [TIME] consume).
    //
    // Component 3 (2026-07-28): the return tuple's reject directives (e.g.
    // "[TRAVEL] rejected — non-adjacent") are DROPPED here. The tracker-stage
    // caller above already merged its rejects into turn_directives, so `parsed`
    // here is the API narrator's bracket-free output → empty commands. This
    // shared site is thus a no-op for rejects; it remains the defensive apply
    // site for any bracket the narrator might have slipped through despite the
    // prose-only prompt. The `tracing::warn!` inside the apply helper captures
    // rejections for forensics.
    let _ = apply_phase3_bracket_commands(&parsed, &state).await;
    apply_time_command_and_maybe_tick(&parsed, &state).await;
    // §11.41: the reply parsed here is the prose-only API NARRATOR's output
    // (the build_api_narrator_system_prompt contract). The narrator emits NO
    // brackets — the tracker (Stage 1 above) already applied them + emitted
    // its scene_events with source:"tracker". So `parsed.commands` is empty
    // here and both apply_* calls are harmless no-ops. The scene_event loop
    // above also emits nothing.

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
    //
    // Variant↔schema binding (2026-08-11): capture the post-tracker schema
    // (the world state THIS roll produced) + store it alongside the variant.
    // Reroll → push_variant_with_schema appends it as variant_schemas[tail];
    // normal turn → seed base_schema (captured pre-Stage-1) + variant_schemas
    // = [post-tracker] so the first variant carries its world-state snapshot.
    // The narrator (Stage 2) is API + never mutates the schema, so this read
    // equals the state right after the tracker's brackets landed.
    let post_tracker_schema = state.fable_schema.lock().await.clone();
    {
        let mut gs = state.fable_session.lock().await;
        if reroll {
            // Install the freshly-generated prose as the NEW active variant.
            // push_variant_with_schema seeds variant 0 from the current content
            // on the first reroll + appends the new variant + its schema.
            if let Some(m) = gs.messages.last_mut() {
                m.push_variant_with_schema(
                    parsed.prose.clone(),
                    reply.raw_output.clone(),
                    Some(post_tracker_schema.clone()),
                );
                // narrator_reasoning is always "" (API narrator never thinks);
                // kept for payload-shape compat with Message.reasoning.
                m.reasoning = narrator_reasoning.clone();
            }
        } else {
            gs.add_assistant_turn(
                parsed.prose.clone(),
                narrator_reasoning.clone(),
                reply.raw_output.clone(),
            );
            // Seed the binding for the first variant: base = the pre-turn world
            // (captured before Stage 1), variant 0's schema = post-tracker.
            if let Some(m) = gs.messages.last_mut() {
                m.base_schema = Some(base_schema_snapshot.clone());
                m.variant_schemas.push(post_tracker_schema.clone());
            }
        }
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
            "reasoning": narrator_reasoning,
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

/// Stage 3 (2026-08-11): the drawer's › interrupt. The user re-pressed ›
/// mid-reroll to abandon the in-flight roll + start a fresh one. This signals
/// the cancel token (halt decoding, same as `fable_stop`) AND sets the
/// `fable_abort_requested` flag, which fable_send's post-reply abort check
/// reads to discard the partial output + revert the schema to the pre-turn
/// base + emit `cancelled` (vs `fable_stop`'s soft cancel that keeps its
/// partial). The frontend awaits that `cancelled` event, then calls
/// `reroll_last_turn` → `fable_send(reroll=true)` for the fresh roll.
#[tauri::command]
async fn fable_interrupt_reroll(state: tauri::State<'_, AppState>) -> Result<(), String> {
    tracing::info!("fable_interrupt_reroll requested");
    state
        .fable_abort_requested
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
    if let Some(cancel) = slot.as_ref() {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// Cancel the in-flight slice regenerate (the golden-pencil partial regen).
/// Signals the reserved `active_slice_cancel` token — DISTINCT from
/// `active_fable_cancel` (the full-turn slot) so a `fable_stop` or `chat_stop`
/// caller can't cross-wire a slice regen (Bug #7 lesson, §2C). The HTTP stream
/// loop checks the token between chunks and breaks cleanly; the partial prose
/// is discarded (the slice path never mutates `msg.content` until the full
/// regeneration lands, so cancel = no-op on the session).
#[tauri::command]
async fn fable_slice_stop(state: tauri::State<'_, AppState>) -> Result<(), String> {
    tracing::info!("fable_slice_stop requested");
    let slot = state.active_slice_cancel.lock().expect("active_slice_cancel mutex");
    if let Some(cancel) = slot.as_ref() {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
}

/// **Golden-pencil slice regenerate** (2026-08-11). The player highlighted a
/// span inside an assistant message; the API rewrites ONLY that span, splicing
/// cleanly against the surrounding text. The frontend supplies the authoritative
/// 3-way split (`pre`/`selection`/`post`) computed from the DOM Selection via
/// `Range.toString()` — what the player saw is what gets regenerated.
///
/// **Contract:**
/// - API-only (Fable narration is API-only — §3A). No local model, no VRAM
///   lease (pure HTTP).
/// - NO tracker re-run, NO schema mutation, NO bracket application. Brackets
///   in the regenerated span are STRIPPED via `bracket_parser::parse` (the
///   slice is a prose fix, not a world-state change — the `edit_message`
///   discipline, but AI-generated).
/// - In-place, no undo (the original span is overwritten).
/// - Streams `{ type: "chunk", text }` events; finalizes with
///   `{ type: "slice_done", final_text }` (the full new message text —
///   pre + regen + post). On mid-stream cancel: `{ type: "cancelled" }` (no
///   mutation). On API failure: `{ type: "api_lost", message }`.
///
/// Cancelable via `fable_slice_stop` (the reserved `active_slice_cancel` slot).
#[tauri::command]
async fn fable_regenerate_slice(
    index: usize,
    pre: String,
    selection: String,
    post: String,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    require_api_for_fable(&state)?;
    if selection.trim().is_empty() {
        return Err("fable_regenerate_slice: selection is empty".into());
    }

    let card = {
        let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
        guard
            .clone()
            .ok_or_else(|| "no active game card: call fable_start first".to_string())?
    };
    let card_id = card.id.clone();

    // Fresh cancel token in the reserved slice slot (distinct from
    // active_fable_cancel — Bug #7 cross-wire lesson, §2C).
    let cancel: llm::CancelToken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut slot = state
            .active_slice_cancel
            .lock()
            .expect("active_slice_cancel mutex");
        *slot = Some(Arc::clone(&cancel));
    }

    // Validate the target + capture context under the session lock. The window
    // is the trailing WINDOW_API_FABLE messages BEFORE index (excludes the beat
    // being edited — the payload below is the sole source of the selection, so
    // the model never sees it duplicated as an assistant turn). prev/next beats
    // are the immediate narrative neighbors for the splice-contract payload.
    let (prev_content, next_content, window): (Option<String>, Option<String>, Vec<session::Message>) = {
        let gs = state.fable_session.lock().await;
        let msgs = &gs.messages;
        let len = msgs.len();
        let _msg = msgs.get(index).ok_or_else(|| {
            format!("fable_regenerate_slice: index {index} out of bounds (len {len})")
        })?;
        if _msg.role != session::Role::Assistant {
            return Err("fable_regenerate_slice: target message is not an assistant turn".into());
        }
        let prev_content = if index > 0 {
            Some(msgs[index - 1].content.clone())
        } else {
            None
        };
        let next_content = if index + 1 < len {
            Some(msgs[index + 1].content.clone())
        } else {
            None
        };
        let fable_visible_window = settings::WINDOW_API_FABLE;
        let start = index.saturating_sub(fable_visible_window);
        let window = msgs[start..index].to_vec();
        (prev_content, next_content, window)
    };

    // Streaming callback wraps the Channel send (mirrors fable_send's on_chunk).
    let on_chunk: llm::ChunkFn = Arc::new({
        let on_event = on_event.clone();
        move |piece: &str| {
            let _ = on_event.send(serde_json::json!({ "type": "chunk", "text": piece }));
        }
    });

    // Resolve the active API profile (the launch gate + require_api_for_fable
    // guarantee one; the `None` case is purely defensive → api_lost).
    let profile = {
        let cfg = state.api_config.lock().expect("api_config mutex");
        cfg.active_profile().cloned()
    };
    let Some(profile) = profile else {
        // Clear the slot, emit api_lost (mirror emit_fable_api_lost's contract
        // but for the slice slot — no autosave: a slice regen mutates no world
        // state, so there's nothing to preserve beyond what's already saved).
        {
            let mut slot = state
                .active_slice_cancel
                .lock()
                .expect("active_slice_cancel mutex");
            *slot = None;
        }
        let _ = on_event.send(serde_json::json!({
            "type": "api_lost",
            "message": "No active API profile.",
        }));
        return Ok(());
    };

    let fable_prompts = state
        .fable_prompts
        .get()
        .expect("fable_prompts is set in setup()");
    let system_prompt = build_slice_regenerate_system_prompt(fable_prompts, &card);

    // Assemble the API message list: system + windowed history (roles mapped
    // OpenAI-style: assistant → "assistant", NOT "model" — Bug #4) + a final
    // user message carrying the tagged splice payload.
    let mut api_msgs: Vec<session::ApiMessage> = Vec::with_capacity(window.len() + 2);
    api_msgs.push(session::ApiMessage {
        role: "system".into(),
        content: system_prompt.trim().to_string(),
        raw_output: String::new(),
    });
    for m in &window {
        let role = match m.role {
            session::Role::Assistant => "assistant",
            session::Role::User => "user",
            session::Role::System => "system",
        };
        api_msgs.push(session::ApiMessage {
            role: role.into(),
            content: m.content.clone(),
            raw_output: String::new(),
        });
    }

    // The splice payload: immediate narrative neighbors + the precise 3-way
    // split. Genericized tags (no copyable concrete examples — anti-pattern #4).
    let mut user_payload = String::with_capacity(
        pre.len() + selection.len() + post.len()
            + prev_content.as_ref().map_or(16, |s| s.len() + 32)
            + next_content.as_ref().map_or(16, |s| s.len() + 32)
            + 256,
    );
    user_payload.push_str(
        "A passage of your own narration is marked below. Rewrite ONLY <passage_to_rewrite>. Splice it so the result reads as one continuous piece with <this_beat_before> and <this_beat_after>. Output the replacement passage and nothing else.\n\n",
    );
    user_payload.push_str("<preceding_turn>\n");
    user_payload.push_str(prev_content.as_deref().unwrap_or("(none — this is the opening beat)"));
    user_payload.push_str("\n</preceding_turn>\n\n<this_beat_before>\n");
    user_payload.push_str(&pre);
    user_payload.push_str("\n</this_beat_before>\n\n<passage_to_rewrite>\n");
    user_payload.push_str(&selection);
    user_payload.push_str("\n</passage_to_rewrite>\n\n<this_beat_after>\n");
    user_payload.push_str(&post);
    user_payload.push_str("\n</this_beat_after>\n\n<following_turn>\n");
    user_payload.push_str(next_content.as_deref().unwrap_or("(none — this is the current end)"));
    user_payload.push_str("\n</following_turn>\n");
    api_msgs.push(session::ApiMessage {
        role: "user".into(),
        content: user_payload,
        raw_output: String::new(),
    });

    let http = llm::HttpBackend::new(profile);
    let reply = http
        .stream(
            api_msgs,
            None,
            None,
            Vec::new(),
            settings::CTX_API,
            on_chunk,
            cancel.clone(),
        )
        .await;

    // Clear the cancel slot now that the stream is done (mirror fable_send's
    // post-turn cleanup at the success exit).
    {
        let mut slot = state
            .active_slice_cancel
            .lock()
            .expect("active_slice_cancel mutex");
        *slot = None;
    }

    let out = match reply {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(error = %e, "fable_regenerate_slice: API failed; emitting api_lost");
            let _ = on_event.send(serde_json::json!({
                "type": "api_lost",
                "message": "The API connection was lost.",
            }));
            return Ok(());
        }
    };

    // Mid-stream cancel: the HTTP loop broke early + returned Ok with partial
    // content. Discard it (the slice path never mutated the session); emit
    // cancelled so the frontend restores the beat.
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = on_event.send(serde_json::json!({ "type": "cancelled" }));
        return Ok(());
    }

    // Strip any accidental brackets from the regenerated span (the narrator
    // writes prose-only; if the model strayed into `[TIME …]` etc., drop it).
    // Brackets are NEVER applied here — pure prose swap. Trim leading/trailing
    // whitespace so the splice butts cleanly against `pre`/`post`. Returns
    // None on an empty result → soft error (don't delete the highlighted span).
    let raw_content = if out.raw.is_empty() { out.content } else { out.raw };
    let new_content = match clean_and_splice_slice(&pre, &raw_content, &post) {
        Some(s) => s,
        None => {
            tracing::warn!("fable_regenerate_slice: regenerated span empty after cleaning");
            let _ = on_event.send(serde_json::json!({
                "type": "error",
                "message": "The model returned an empty response. Try again.",
            }));
            return Ok(());
        }
    };

    // Persist the splice. apply_slice_splice overwrites content + the active
    // variant mirror + clears raw_output (keeps the mirror honest across
    // reload). No schema touch.
    {
        let mut gs = state.fable_session.lock().await;
        apply_slice_splice(&mut gs, index, new_content.clone())?;
    }
    if let Err(e) = persist_fable_session(&app, state.inner(), &card_id).await {
        tracing::warn!(error = %e, "fable_regenerate_slice: persist failed (in-memory splice still applied)");
    }

    let _ = on_event.send(serde_json::json!({
        "type": "slice_done",
        "final_text": new_content,
    }));
    Ok(())
}

/// Append a timestamped line to the creator playtest trace at
/// `<temp_dir>/wupi-creator.log`. Used by `creator_assistant_turn` (Rust-side
/// events) + the `creator_log` IPC (frontend events) so ONE file holds the
/// full creator playtest trace, interleaved chronologically. Best-effort: a
/// logging failure is silently dropped (must never break creation).
fn creator_trace(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("wupi-creator.log");
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let line = format!("[{secs:.3}] {msg}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Frontend → creator-trace bridge. The creator-chat UI calls this at each
/// pipeline step (envelope parsed, draft merged, review shown, CREATE
/// dispatched, write result) so the JS-side decisions land alongside the
/// Rust-side events in `wupi-creator.log` (see `creator_trace`). Best-effort.
#[tauri::command]
fn creator_log(line: String) -> Result<(), String> {
    creator_trace(&line);
    Ok(())
}

// ── Codex embed-cap Gate 1 (turn-loop validation) ─────────────────────────
//
// The creator assistant streams raw text; the frontend parses the `{action,
// draft}` envelope. For a codex `ready`, Rust re-parses the buffered reply
// post-stream + rejects it if any entry body exceeds the 1400-char bge-small
// window (the embedder silently truncates longer bodies, corrupting RAG).
// Rejection emits `validation_error` (not `done`) so the frontend injects a
// corrective turn + GLM retries. Mirrors the frontend's `parseEnvelope`:
// optional markdown-fence strip + `{`…`}` slice + serde_json parse.

/// Extract the JSON envelope object from a creator reply. Strips a markdown
/// code fence if present, then parses the `{`…`}` span. Returns `None` if no
/// valid JSON object is found (the caller treats that as "not a ready draft" —
/// never blocks on an unparseable reply).
fn extract_creator_envelope(reply: &str) -> Option<serde_json::Value> {
    let mut s = reply.trim();
    if s.starts_with("```") {
        if let Some(nl) = s.find('\n') {
            s = s[nl + 1..].trim_start();
        }
        if let Some(end) = s.rfind("```") {
            s = s[..end].trim();
        }
    }
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&s[start..=end]).ok()
}

/// If the reply is a codex `ready` draft with any `draft.entries[].body` over
/// the 1400-char embed cap, return those entries as `(title, body_len)`.
/// Returns `None` for non-codex drafts, `ask` turns, ready drafts that fit, or
/// unparseable replies (never blocks those).
fn find_oversize_codex_entries(reply: &str) -> Option<Vec<(String, usize)>> {
    const CAP: usize = 1400;
    let env = extract_creator_envelope(reply)?;
    if env.get("action").and_then(|a| a.as_str()) != Some("ready") {
        return None;
    }
    let entries = env.get("draft")?.get("entries")?.as_array()?;
    let offenders: Vec<(String, usize)> = entries
        .iter()
        .filter_map(|e| {
            let body = e.get("body")?.as_str()?;
            if body.len() > CAP {
                let title = e
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("(untitled)")
                    .to_string();
                Some((title, body.len()))
            } else {
                None
            }
        })
        .collect();
    if offenders.is_empty() {
        None
    } else {
        Some(offenders)
    }
}

/// Build the corrective alert GLM sees on retry. Matches the operator spec:
/// names the offending title + length + the 1400 cap + the split/condense remedy.
fn build_codex_oversize_alert(offenders: &[(String, usize)]) -> String {
    offenders
        .iter()
        .map(|(title, len)| {
            format!(
                "SYSTEM ALERT: Entry '{title}' length is {len} chars, exceeding the \
                 1400-character vector embedding cap. You must split this content across \
                 paginated entries (e.g. '{title} - Part 1', '{title} - Part 2') or \
                 condense the text."
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The GLM-driven creation assistant (2026-08-12). A creation-only API role
/// (AGENTS.md §3A override): conversational authoring of player / sim-world /
/// codex artifacts OUTSIDE the runtime game loop — no tracker, no schema, no
/// world state. Streams the model's reply as `chunk` events, then a single
/// `done` carrying the full text; the frontend parses the ask/ready JSON
/// envelope + drives the review-card handoff. GLM never writes files — it
/// fills a `draft`; the frontend serializer + the existing write IPCs persist.
/// Mirrors `fable_regenerate_slice`'s API-only one-shot shape.
#[tauri::command]
async fn creator_assistant_turn(
    creator_kind: String,
    history: Vec<session::ApiMessage>,
    import_data: Option<serde_json::Value>,
    on_event: tauri::ipc::Channel<serde_json::Value>,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // Hard gate: the assistant needs an active API connection (the creator is
    // part of Fable, so the Fable launch gate applies). Works pre-game: this
    // checks the API profile + model_source, NOT an active game card.
    require_api_for_fable(&state)?;

    // Validate the creator kind up front (the prompt builder branches on it).
    // The "intro" kind was REMOVED 2026-08-15 (Chloe): the SIM Wizard asks the
    // mandatory intro question itself + serializeSimCard embeds the agreed
    // `<intro>` sibling — there is no post-card intro generation pass.
    match creator_kind.as_str() {
        "player" | "sim" | "codex" => {}
        other => return Err(format!("creator_assistant_turn: unknown creator_kind {other:?}")),
    }

    tracing::info!(%creator_kind, hist = history.len(), has_import = import_data.is_some(), "creator_assistant_turn");
    creator_trace(&format!(
        "→ turn start: kind={creator_kind} history={}turns import={}",
        history.len(),
        if import_data.is_some() { "yes" } else { "no" },
    ));

    // Stream chunks to the frontend as they arrive (live chat-bubble typing).
    let on_chunk: llm::ChunkFn = Arc::new({
        let on_event = on_event.clone();
        move |piece: &str| {
            let _ = on_event.send(serde_json::json!({ "type": "chunk", "text": piece }));
        }
    });

    // Resolve the active API profile (require_api_for_fable guarantees one; the
    // None case is purely defensive → api_lost).
    let profile = {
        let cfg = state.api_config.lock().expect("api_config mutex");
        cfg.active_profile().cloned()
    };
    let Some(profile) = profile else {
        let _ = on_event.send(serde_json::json!({
            "type": "api_lost",
            "message": "No active API profile.",
        }));
        return Ok(());
    };

    let system_prompt = build_creator_assistant_system_prompt(&creator_kind);

    // Assemble the message list: system prompt + an optional import-context
    // system message + the conversation history (roles already OpenAI-shaped
    // by the caller). memory_block / world_state are unused on this path — the
    // creator carries its own context in the system prompt + history.
    let mut api_msgs: Vec<session::ApiMessage> = Vec::with_capacity(history.len() + 2);
    api_msgs.push(session::ApiMessage {
        role: "system".into(),
        content: system_prompt.trim().to_string(),
        raw_output: String::new(),
    });
    if let Some(data) = import_data {
        // Fold the mechanically-extracted import data in as a second system
        // message: the user's starting concept, to map onto the schema + refine
        // (never invent details that conflict with it).
        let mut content = String::with_capacity(128 + data.to_string().len());
        content.push_str(
            "Imported reference data, mechanically extracted from an external file. \
             Treat it as the user's AUTHORED starting concept — map its fields onto \
             the schema with high fidelity (preserve its lore; do not rewrite, \
             condense, or paraphrase it away), fill gaps by asking, and never invent \
             details that conflict with it:\n<import>",
        );
        content.push_str(&data.to_string());
        content.push_str("</import>");
        api_msgs.push(session::ApiMessage {
            role: "system".into(),
            content,
            raw_output: String::new(),
        });
    }
    for m in history {
        api_msgs.push(m);
    }

    // Fresh cancel token in the reserved creator slot (distinct from
    // active_fable_cancel / active_slice_cancel — Bug #7 lesson, §2C).
    let cancel: llm::CancelToken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut slot = state
            .active_creator_cancel
            .lock()
            .expect("active_creator_cancel mutex");
        *slot = Some(Arc::clone(&cancel));
    }

    let http = llm::HttpBackend::new(profile);
    let reply = http
        .stream(api_msgs, None, None, Vec::new(), settings::CTX_API, on_chunk, cancel.clone())
        .await;

    // Clear the slot now that the stream is done (mirrors the slice path's
    // post-turn cleanup).
    {
        let mut slot = state
            .active_creator_cancel
            .lock()
            .expect("active_creator_cancel mutex");
        *slot = None;
    }

    let out = match reply {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(error = %e, "creator_assistant_turn: API failed; emitting api_lost");
            creator_trace(&format!("✗ api_lost: {e}"));
            let _ = on_event.send(serde_json::json!({
                "type": "api_lost",
                "message": "The API connection was lost.",
            }));
            return Ok(());
        }
    };

    // Mid-stream cancel: the HTTP loop broke early + returned Ok with partial
    // content. Discard it + emit cancelled so the frontend drops the partial
    // assistant turn (it was never added to history).
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        creator_trace("✗ cancelled (stop)");
        let _ = on_event.send(serde_json::json!({ "type": "cancelled" }));
        return Ok(());
    }

    tracing::info!(len = out.content.len(), "creator_assistant_turn: done");
    let preview: String = out.content.chars().take(200).collect();
    creator_trace(&format!(
        "← reply: {} bytes | preview: {}",
        out.content.len(),
        preview.replace(['\n', '\r'], " ")
    ));

    // Gate 1 — codex embed-cap validation: if GLM emitted a `ready` codex draft
    // with any entry body >1400 chars, reject it (do NOT emit `done`) + emit a
    // validation_error so the frontend injects the alert as a corrective turn +
    // GLM retries. bge-small silently truncates >~1400-char bodies, corrupting
    // the vector; the seed-path split (Gate 2) is the final backstop, but this
    // catches it at authoring time so the user never sees a malformed card.
    if creator_kind == "codex" {
        if let Some(offenders) = find_oversize_codex_entries(&out.content) {
            creator_trace(&format!(
                "✗ codex ready rejected: {} oversize entry/entries (max body {}) — emitting validation_error",
                offenders.len(),
                offenders.iter().map(|(_, l)| *l).max().unwrap_or(0)
            ));
            let alert = build_codex_oversize_alert(&offenders);
            let _ = on_event.send(serde_json::json!({
                "type": "validation_error",
                "text": out.content,
                "alert": alert,
                "offenders": offenders.iter().map(|(t, l)| serde_json::json!({"title": t, "len": l})).collect::<Vec<_>>(),
            }));
            return Ok(());
        }
    }

    // The HTTP path fills only `content` (raw is empty there). The frontend
    // parses the ask/ready JSON envelope from this text.
    let _ = on_event.send(serde_json::json!({
        "type": "done",
        "text": out.content,
    }));
    Ok(())
}

/// Signals the reserved `active_creator_cancel` token — aborts a GLM creation
/// wizard turn mid-stream. Distinct from `fable_slice_stop` / `fable_stop` /
/// `chat_stop` so no other consumer can cross-wire a creator turn (Bug #7
/// lesson, §2C). The partial assistant turn is discarded (never added to
/// history), so cancel = clean slate for the next send.
#[tauri::command]
async fn creator_assistant_stop(state: tauri::State<'_, AppState>) -> Result<(), String> {
    tracing::info!("creator_assistant_stop requested");
    creator_trace("→ stop requested");
    let slot = state
        .active_creator_cancel
        .lock()
        .expect("active_creator_cancel mutex");
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

    // 2. Per-card persistence. Best-effort: a failure logs a warning but
    //    doesn't block fable_end — the in-memory state is cleared regardless;
    //    the user just loses the resume point on a disk error, not the running
    //    game. Save session + schema under the roleplay id
    //    (apps/fable/{sessions,schemas}/<card_id>.json).
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
    clear_fable_history(&state).await;
    *state.fable_session.lock().await = session::Conversation::new();
    *state.active_fable_card.lock().expect("active_fable_card mutex") = None;
    *state.active_player_id.lock().expect("active_player_id mutex") = None;

    // 4. Clear any leftover game cancel token.
    *state.active_fable_cancel.lock().expect("active_fable_cancel mutex") = None;

    tracing::info!("game ended: narrator engine down, per-card state persisted, memory scope restored");
    Ok(())
}

// ===========================================================================
// §11.30 Left-Drawer Visual HUD — manual player-action injection pipeline.
// The frontend `fable_player_action_set` arms a one-shot `<player_action>`
// string when the player performs a tactile UI action in the left drawer
// (Consume potion, Equip/Unequip). The next `fable_send` takes + clears it
// (ONE turn) and renders it as a new `<player_action type="manual_override">`
// block at the top of `assemble_narrator_skeleton`, leading the narrator's
// attention. Held under tokio Mutex for the same race reason (frontend
// writer vs the consume site inside `fable_send`).
// ===========================================================================

#[tauri::command]
async fn fable_player_action_set(
    text: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        // Empty = clear (treat as a cancel).
        let mut g = state.pending_player_action.lock().await;
        *g = None;
        return Ok(());
    }
    // Hard cap so a runaway UI loop can't bloat the prompt (§1C: shrink the
    // payload, never raise the context). 500 chars is generous for any
    // "Player equipped X to Y" line.
    let capped = if trimmed.len() > 500 {
        let mut s = trimmed[..500].to_string();
        // Avoid splitting a multi-byte char — back up to a char boundary.
        while !s.is_char_boundary(s.len()) {
            s.pop();
        }
        s
    } else {
        trimmed.to_string()
    };
    let mut g = state.pending_player_action.lock().await;
    *g = Some(capped);
    tracing::info!("player action armed (one-shot, will fire on next fable_send)");
    Ok(())
}

// ===========================================================================
// Prism — the image-generation app IPCs (2026-07-31).
//
// Prism reuses the shared SD swap core (`run_sd_swap_core`, the §1c extraction)
// for the VRAM eviction/reload cycle, but drives generation DIRECTLY (NOT via
// the FABLE `sd_autogen_enabled` opt-in — that gate is FABLE's per-turn
// scene-art switch). Prism's `prism_generate` is the user-facing Generate
// button: it builds a `SceneImageRequest` from the user's params, resolves a
// unique gallery dest path, + hands off to the same proven swap cycle. On
// success it inserts a gallery row + emits `prism-gen-done` (the frontend's
// subscriber swaps in the new image + refreshes the grid).
// ===========================================================================

/// The Prism status snapshot for the UI's status banner. `model_present`
/// tells the UI whether a checkpoint exists in `models/sd/`; `disabled` is
/// the one-strike failure latch (a prior render OOM'd/errored). Both gate the
/// Generate button's enabled state.
#[tauri::command]
async fn prism_sd_status(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    use std::sync::atomic::Ordering;
    let model_path = state
        .pending_sd_model_path
        .lock()
        .expect("pending_sd_model_path mutex")
        .clone();
    Ok(serde_json::json!({
        "model_present": model_path.is_some(),
        "model_name": model_path.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or(""),
        // The one-strike latch. `sd_autogen_enabled` is FABLE's per-turn
        // switch (irrelevant to Prism — Prism drives gen directly); only the
        // latch matters here (a render failure latches BOTH off until ack).
        "disabled": state.sd_autogen_disabled.load(Ordering::Relaxed),
        // Whether the REAL Stable Diffusion backend (diffusion-rs) is compiled
        // into this build. When false, `default_sd_backend()` returns the
        // NoopImageGenerator stub — generate() writes an EMPTY file + returns
        // in ~0ms (no image renders). The frontend surfaces a banner in that
        // case so the user isn't confused by instant "generated" toasts with
        // no visible image. Real images require the `--features diffusion-rs`
        // build (Chloe's build-safety procedure).
        "backend_real": cfg!(feature = "diffusion-rs"),
    }))
}

/// Clear the one-strike failure latch after the user acks a prior generation
/// failure (e.g. after swapping in a smaller model or closing VRAM-hogging
/// apps). Mirrors the contract a future FABLE ack-toggle would use.
#[tauri::command]
async fn prism_sd_clear_latch(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state
        .sd_autogen_disabled
        .store(false, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("prism: one-strike SD latch cleared (user ack)");
    Ok(())
}

/// Generate one image from the user's params (the Generate button). Spawns the
/// shared SD swap cycle (evict text models → load SD → render → reload text
/// models) in the background; returns immediately with the resolved dest path
/// so the UI can show a loading state. The `prism-gen-done` event fires when
/// the render completes (success or failure) — the frontend subscriber swaps
/// in the image / surfaces the error.
///
/// The generation row is inserted into the gallery DB ONLY on success (a
/// failed/empty render leaves no orphan row). The returned `GalleryImage`
/// carries the full metadata the UI needs to render the new thumbnail.
#[tauri::command]
async fn prism_generate(
    params: prism::GenerateParams,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    // Gate: the gallery DB must be open (best-effort init at boot; if it
    // failed, the gallery can't record results — surface a clear error rather
    // than a silent no-op).
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable (boot init failed)".to_string())?;

    // Resolve the SD model path (None → no checkpoint in models/sd/).
    let sd_model_path = state
        .pending_sd_model_path
        .lock()
        .expect("pending_sd_model_path mutex")
        .clone();
    let sd_model_path = sd_model_path.ok_or_else(|| {
        "No Stable Diffusion model found. Drop a checkpoint (.safetensors or .gguf) into the models/sd/ folder.".to_string()
    })?;

    // Resolve a unique gallery dest + build the request. `created_at` is
    // captured NOW (before the multi-second swap) so the gallery sort reflects
    // when the user clicked Generate, not when the render finished.
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let gallery_dir = resolve_apps_dir(&app).join("prism").join("gallery");
    let dest = prism::dest_path(&gallery_dir, params.seed, created_at);
    let model_name = sd_model_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let request = prism::build_request(&params, sd_model_path.clone(), dest.clone());

    // Snapshot the params + metadata for the gallery insert (the request is
    // moved into the swap core; the row needs its own copies).
    let row_prompt = params.prompt.clone();
    let row_negative = params
        .negative_prompt
        .clone()
        .unwrap_or_default();
    let row_seed = params.seed;
    let row_cfg = params.cfg;
    let row_steps = params.steps;
    let row_width = request.width as i32;
    let row_height = request.height as i32;
    let row_sampler = params.sampler;
    let row_model = model_name.clone();
    let dest_for_insert = dest.clone();
    let created_at_for_insert = created_at;
    let gallery_for_insert = gallery.clone();

    // Clone the arcs the swap core needs (mirrors the FABLE done-beat spawn).
    let context_swap = state.context_swap.clone();
    let sd_engine_slot = state.sd_engine.clone();
    let sd_autogen_disabled = state.sd_autogen_disabled.clone();
    let active_sd_cancel = state.active_sd_cancel.clone();
    // Resolve the LLM model path. pending_model_path is CONSUMED by
    // boot_load_model's `.take()` (§21) at boot, so by the time Prism runs
    // it's almost always None — reloading from it would have no path + trip
    // the one-strike latch (the exact bug observed in the first playtest:
    // generate succeeded via the stub, then "no LLM model path to reload"
    // latched SD off for the rest of the process). Re-resolve from disk via
    // the same resolver boot_load_model uses (DRY — single source of truth
    // for the search dirs + pick_main_model logic). This is the Prism analog
    // of boot_load_model's line-1958 re-scan fix.
    let llm_model_path = {
        let stashed = state
            .pending_model_path
            .lock()
            .expect("pending_model_path mutex")
            .clone();
        match stashed {
            Some(p) => Arc::new(std::sync::Mutex::new(Some(p))),
            None => {
                // Re-scan disk. Wrap in the same Arc<Mutex<Option<PathBuf>>>
                // shape run_sd_swap_core reads (it locks + clones internally).
                let resolved = resolve_model_path(&app);
                if resolved.is_none() {
                    tracing::warn!("prism_generate: no LLM model path resolved (pending_model_path empty + disk re-scan found none). The post-swap LLM reload will fail + trip the one-strike latch.");
                }
                Arc::new(std::sync::Mutex::new(resolved))
            }
        }
    };
    let app_handle = app.clone();

    // Detach the swap cycle (it's multi-second + blocking on CUDA). The
    // command returns immediately with the dest path; the event fires on
    // completion. This mirrors FABLE's done-beat spawn shape.
    tokio::spawn(async move {
        run_sd_swap_core(
            context_swap,
            sd_engine_slot,
            Some(sd_model_path),
            sd_autogen_disabled.clone(),
            active_sd_cancel,
            llm_model_path,
            request,
            Box::new(move |outcome| {
                use tauri::Emitter;
                match outcome {
                    scene_art::SwapOutcome::Generated(result) => {
                        tracing::info!(
                            dest = %result.dest.display(),
                            elapsed_ms = result.elapsed_ms,
                            "prism: image generated — inserting gallery row"
                        );
                        // Insert the gallery row (the full metadata record).
                        let new_row = prism::NewImage {
                            created_at: created_at_for_insert,
                            path: dest_for_insert.to_string_lossy().into_owned(),
                            prompt: row_prompt,
                            negative_prompt: row_negative,
                            seed: row_seed,
                            cfg: row_cfg,
                            steps: row_steps,
                            width: row_width,
                            height: row_height,
                            sampler: row_sampler,
                            model: row_model,
                        };
                        match gallery_for_insert.insert(&new_row) {
                            Ok(id) => {
                                // Re-read the row to get the full GalleryImage
                                // (id + favorite/trashed defaults) for the event.
                                let img = gallery_for_insert.get(id).ok().flatten();
                                let _ = app_handle.emit(
                                    "prism-gen-done",
                                    serde_json::json!({ "ok": true, "image": img }),
                                );
                            }
                            Err(e) => {
                                tracing::error!(error = %format!("{e:#}"), "prism: gallery insert failed");
                                let _ = app_handle.emit(
                                    "prism-gen-done",
                                    serde_json::json!({
                                        "ok": false,
                                        "error": format!("Generation succeeded but the gallery row could not be saved: {e}"),
                                        "path": dest_for_insert.to_string_lossy(),
                                    }),
                                );
                            }
                        }
                    }
                    scene_art::SwapOutcome::Skipped => {
                        // The swap core only Skips on the latch/no-model gate;
                        // both were pre-checked before spawn, so this is rare
                        // (a latch trip raced the spawn). Surface it.
                        let _ = app_handle.emit(
                            "prism-gen-done",
                            serde_json::json!({ "ok": false, "error": "Generation skipped (disabled latch or no model)" }),
                        );
                    }
                    scene_art::SwapOutcome::Cancelled => {
                        let _ = app_handle.emit(
                            "prism-gen-done",
                            serde_json::json!({ "ok": false, "error": "Generation cancelled", "cancelled": true }),
                        );
                    }
                    scene_art::SwapOutcome::Failed(err) => {
                        tracing::warn!(error = %err, "prism: generation failed — latch tripped");
                        let _ = app_handle.emit(
                            "prism-gen-done",
                            serde_json::json!({
                                "ok": false,
                                "error": format!("Generation failed: {err}. Auto-generation is now disabled; use the status banner to retry after freeing VRAM."),
                            }),
                        );
                    }
                }
            }),
        )
        .await;
    });

    // Return immediately with the dest path (the UI shows a loading state on
    // the thumbnail slot; the event swaps in the real image).
    Ok(serde_json::json!({
        "pending": true,
        "path": dest.to_string_lossy(),
    }))
}

/// List gallery images (the masonry grid). See `prism::GalleryDb::list` for
/// the filter + pagination contract.
#[tauri::command]
async fn prism_gallery_list(
    filter: prism::GalleryFilter,
    limit: i64,
    offset: i64,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<prism::GalleryImage>, String> {
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable".to_string())?;
    gallery
        .list(&filter, limit, offset)
        .map_err(|e| format!("{e:#}"))
}

/// Fetch one gallery image by id (the metadata panel).
#[tauri::command]
async fn prism_gallery_get(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<Option<prism::GalleryImage>, String> {
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable".to_string())?;
    gallery.get(id).map_err(|e| format!("{e:#}"))
}

/// Toggle the favorite flag (the ★ quick action).
#[tauri::command]
async fn prism_gallery_favorite(
    id: i64,
    fav: bool,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable".to_string())?;
    gallery.set_favorite(id, fav).map_err(|e| format!("{e:#}"))
}

/// Soft-delete (move to trash).
#[tauri::command]
async fn prism_gallery_trash(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable".to_string())?;
    gallery.trash(id).map_err(|e| format!("{e:#}"))
}

/// Restore from trash.
#[tauri::command]
async fn prism_gallery_restore(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable".to_string())?;
    gallery.restore(id).map_err(|e| format!("{e:#}"))
}

/// Hard-delete: remove the row AND unlink the PNG file. Used by the trash
/// empty action. Best-effort file unlink (a missing file isn't an error — the
/// row removal is the source of truth).
#[tauri::command]
async fn prism_gallery_purge(
    id: i64,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    let gallery = state
        .prism_db
        .get()
        .cloned()
        .ok_or_else(|| "Prism gallery database is unavailable".to_string())?;
    let path = gallery.purge(id).map_err(|e| format!("{e:#}"))?;
    if let Some(p) = path {
        let _ = std::fs::remove_file(&p); // best-effort; missing file is fine
    }
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
    // from the title screen. Use `active_fable_card` (NOT `fable_engine`):
    // under the §2B VRAM swap-lock the engine is evicted whenever chat/schema
    // acquires the lease, so the engine slot is regularly `None` mid-game.
    // The card, by contrast, is seated on `fable_start` and cleared on
    // `fable_end` — the true game-lifetime signal. Same fix as
    // `fable_is_active` (lib.rs ~2099) and `fable_rollback` below.
    if !fable_is_active(&state) {
        return Err("no fable game active: call fable_start first".to_string());
    }
    let s = state.fable_schema.lock().await;
    Ok(serde_json::to_value(&s.player_state)
        .map_err(|e| format!("serialize player state: {e}"))?)
}

/// Return the identity of the currently-seated fable card plus the chat-UI
/// portrait data: `{ name, player_name, card_id, card_portrait_url,
/// player_portrait_url, npc_names }`.
///
/// - `name` / `player_name` — the stage's "(name) is currently typing.."
///   indicator + message-header names (card name → narrator beats;
///   player_name → user beats). `player_name` is "" when the card omits
///   `<player_name>`.
/// - `card_id` — the seated card's id (for resolving the card portrait
///   sibling file `cards/<id>/portrait.*`).
/// - `card_portrait_url` — absolute path to the card/narrator portrait, or
///   null when no portrait sibling exists. Resolved via the same png/jpg/jpeg
///   stat-walk as `fable_card_portrait_url`. The narrator + every NPC (NPC
///   portraits are deferred) fall back to this in the VN chat UI.
/// - `player_portrait_url` — absolute path to the active saved player's
///   portrait, or null (playerless game / no portrait). Resolved
///   via `load_player_portrait` from `active_player_id`.
/// - `npc_names` — `{ id, name }` pairs from the card's `<cast>`, so the chat
///   can render real NPC display names on character beats (replaces the old
///   slug-title-case fallback).
///
/// `None` (serialized as null) when no game is active.
///
/// FRONTEND CONTRACT: the JS consumer (stage.js `refreshActiveCardName`)
/// accepts BOTH this object shape AND a legacy plain-string shape, so a
/// frontend built against the older `Option<String>` return still works — it
/// reads each new field defensively. Uses the same `fable_is_active` gate as
/// the other read-only fable queries.
#[tauri::command]
fn fable_active_card_get(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Option<serde_json::Value>, String> {
    let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
    Ok(guard.as_ref().map(|c| {
        // Resolve the card portrait sibling (png/jpg/jpeg stat-walk, mirrors
        // fable_card_portrait_url). Best-effort — a missing cards dir degrades
        // to null.
        let card_portrait_url = resolve_fable_cards_dir(&app)
            .map(|root| {
                let card_dir = resolve_card_dir(&root, &c.id);
                for ext in ["png", "jpg", "jpeg"] {
                    let path = card_dir.join(format!("portrait.{ext}"));
                    if path.is_file() {
                        return Some(path.to_string_lossy().into_owned());
                    }
                }
                None
            })
            .flatten();
        // Resolve the active player portrait (null when no player is attached).
        let player_portrait_url = state
            .active_player_id
            .lock()
            .expect("active_player_id mutex")
            .as_deref()
            .and_then(|pid| load_player_portrait(&app, pid));
        // The cast NPC id→name map (real speaker labels on character beats).
        let npc_names: Vec<serde_json::Value> = c.cast.iter().map(|n| {
            serde_json::json!({ "id": n.id, "name": n.name })
        }).collect();
        serde_json::json!({
            "name": c.name,
            "player_name": c.player_name.clone().unwrap_or_default(),
            "card_id": c.id,
            "card_portrait_url": card_portrait_url,
            "player_portrait_url": player_portrait_url,
            "npc_names": npc_names,
        })
    }))
}

/// Return the full live `WorldSchema` as JSON for the Tracker tab (left
/// drawer). The frontend renders the editable fields (player, entities, clock,
/// weather, location, rumors) from this; a manual edit is written back via
/// `fable_schema_set`. Same `fable_is_active` gate as the other read-only
/// queries.
#[tauri::command]
async fn fable_schema_get(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    if !fable_is_active(&state) {
        return Err("no fable game active: call fable_start first".to_string());
    }
    let s = state.fable_schema.lock().await;
    serde_json::to_value(&*s).map_err(|e| format!("serialize world schema: {e}"))
}

/// Replace the live `WorldSchema` with a manually-edited one (from the Tracker
/// tab OR the Soul Gem inventory panel). This is a USER-INITIATED edit (the
/// same trust class as `fable_rollback` + `fable_load_save`): it bypasses the
/// §11.19 immutability lock (that lock gates LLM-initiated deltas, not user
/// edits). The pre-edit schema is pushed to the history ring buffer first so
/// the manual edit is undoable via the existing `fable_rollback` (1-click undo)
/// — mirrors every other mutation site. `schema_json` is a full serialized
/// `WorldSchema`.
///
/// `event_note` (optional, 2026-08-08): when the Soul Gem inventory panel fires
/// a physical action (EQUIP/CONSUME/POCKET/STORE/DISCARD), it passes a short
/// past-tense description here ("equipped Iron Sword", "consumed Health Potion").
/// The note is appended to the installed schema's `recent_events`, which the
/// next narrator turn renders inside `<world_state>` — so the API narrator is
/// AWARE of the player's UI action without needing the player to re-type it.
/// This closes the schema-to-narrator loop: a UI action leaves a trace the
/// narrator sees, not just a silent state mutation. The Tracker-tab raw editor
/// passes `None` (no trace — it's a bulk editorial edit, not an in-fiction act).
/// The note is capped at EVENT_NOTE_MAX chars + trimmed; an empty/whitespace
/// note is dropped (no trace). The `recent_events` render caps at the last 5,
/// so older UI-action traces age out naturally as the world moves.
#[tauri::command]
async fn fable_schema_set(
    schema_json: serde_json::Value,
    event_note: Option<String>,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !fable_is_active(&state) {
        return Err("no fable game active: call fable_start first".to_string());
    }
    let mut new_schema: schema::WorldSchema = serde_json::from_value(schema_json)
        .map_err(|e| format!("deserialize world schema: {e}"))?;
    // If a UI-action trace was supplied, append it to recent_events so the next
    // narrator turn sees the physical action in <world_state>. Trim + cap; drop
    // empty. This runs BEFORE install so the persisted schema carries the trace.
    if let Some(note) = event_note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            let capped = if trimmed.chars().count() > EVENT_NOTE_MAX {
                trimmed.chars().take(EVENT_NOTE_MAX).collect::<String>()
            } else {
                trimmed.to_string()
            };
            new_schema.recent_events.push(capped);
        }
    }
    // Snapshot the current schema for undo, then install the edit.
    {
        let snap = state.fable_schema.lock().await.clone();
        push_fable_history_snapshot(&state, snap).await;
    }
    {
        let mut s = state.fable_schema.lock().await;
        *s = new_schema;
    }
    // Persist the edited schema per-card (best-effort, mirrors the autosave
    // contract — a disk error is logged, not surfaced, since the in-memory
    // state already holds the edit).
    let roleplay_card_id = state.active_card_id.lock().expect("active_card_id mutex").clone();
    let schema_snapshot = state.fable_schema.lock().await.clone();
    save_schema(&app, &roleplay_card_id, &schema_snapshot).await;
    tracing::info!(card_id = %roleplay_card_id, "fable_schema_set: manual edit applied + persisted");
    Ok(())
}

/// The active card's identity slice, returned to the read-only Sim Card tab
/// (right drawer). `null` when no game is active. The tab renders these as
/// read-only prose; editing happens via the ✎ raw editor (`fable_card_raw_*`,
/// which writes the `.sim` on disk) or by talking to WUPI. (The old inline
/// `fable_card_save` session-only edit path was retired 2026-08-11 when the
/// tab went read-only.)
#[tauri::command]
fn fable_card_get(
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
    Ok(guard.as_ref().map(|c| {
        serde_json::json!({
            "name": c.name,
            "core_persona": c.core_persona,
            "setting": c.setting.clone().unwrap_or_default(),
            "plot": c.plot.clone().unwrap_or_default(),
            "tone": c.tone.clone().unwrap_or_default(),
            // opening_scene removed 2026-08-05: the intro lives in a sibling
            // .intro file, not on the cached card.
            "player_name": c.player_name.clone().unwrap_or_default(),
        })
    }))
}

// ── Per-card raw-file IPCs (2026-08-01 Fable tab rail + raw editor) ───────
// The right-drawer tab rail (Player / Sim Card / Codex / World / NPC) shows
// each topic's data as friendly prose, and a ✎ icon opens the raw JSON/XML
// file in a centered modal. These IPCs are the read/write surface for that.
//
// All paths resolve under the active card's per-card folder
// (`cards/<card_id>/...`) via `resolve_card_file` — never accept a caller-
// supplied path (the raw editor never escapes the card's folder). Writes go
// through `write_atomic` (temp+fsync+rename) so a crash mid-write never
// truncates the existing file. JSON files validate via `serde_json::from_str`
// before write; the `.sim` validates via the real parser (`parse_from_xml_str`).

/// The active roleplay card's id (or `Err` when no game is active). The raw-
/// file IPCs take this implicitly rather than as an arg so the frontend never
/// has to track it — same `fable_is_active` gate as the other fable queries.
fn active_roleplay_card_id(state: &tauri::State<'_, AppState>) -> Result<String, String> {
    if !fable_is_active(state) {
        return Err("no fable game active: call fable_start first".to_string());
    }
    Ok(state
        .active_card_id
        .lock()
        .expect("active_card_id mutex")
        .clone())
}

/// Read the raw text of the active card's `.sim` file. The raw editor's load
/// path for the Sim Card tab. Returns the file's full XML text (NotFound →
/// empty string, though an active game always has a seated card on disk).
#[tauri::command]
fn fable_card_raw_get(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let card_id = active_roleplay_card_id(&state)?;
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_card_file(&cards_root, &card_id, "sim");
    std::fs::read_to_string(&path).map_err(|e| format!("read card: {e}"))
}

/// Validate + atomically write the active card's `.sim` file (the raw editor's
/// save path for the Sim Card tab). Validates through the REAL parser
/// (`parse_from_xml_str`) before any disk touch — malformed XML is rejected,
/// the file untouched. This writes the `.sim` to disk (the raw-editor
/// contract: edit the whole file, persist it). The live narrator card is
/// NOT re-seated here (the next `fable_start`
/// picks up the edit); a live re-seat is a future enhancement.
#[tauri::command]
fn fable_card_raw_set(
    xml: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let card_id = active_roleplay_card_id(&state)?;
    if xml.len() > 100_000 {
        return Err("Card exceeds 100 KB cap".into());
    }
    // Validate through the real parser BEFORE any disk touch (same gate as
    // fable_validate_card_xml / fable_write_card).
    sim_card::parse_from_xml_str(&xml).map_err(|e| format!("Invalid card format: {e}"))?;
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("mkdir card folder: {e}"))?;
    let path = resolve_card_file(&cards_root, &card_id, "sim");
    write_atomic(&path, xml.as_bytes()).map_err(|e| format!("write card: {e}"))?;
    tracing::info!(card_id = %card_id, "fable_card_raw_set: .sim written to disk");
    Ok(())
}

/// Read the raw `.sim` text of an EXPLICIT card by id (no active-game
/// requirement). Used by the Load menu's EDIT action (2026-08-05) — there's no
/// active card when the user edits from the title's Load menu, so the
/// active-card-keyed `fable_card_raw_get` doesn't apply. Same path resolution
/// + NotFound → empty string contract. Path-traversal guarded by the slug
/// shape of card ids (the resolve_card_dir join is under the cards root).
#[tauri::command]
fn fable_card_raw_get_by_id(card_id: String, app: tauri::AppHandle) -> Result<String, String> {
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_card_file(&cards_root, &card_id, "sim");
    std::fs::read_to_string(&path).map_err(|e| format!("read card: {e}"))
}

/// Validate + atomically write an EXPLICIT card's `.sim` by id (no active-game
/// requirement). The Load menu's EDIT save path (2026-08-05). Same validation
/// gate (`parse_from_xml_str`) + atomic write as `fable_card_raw_set`. The
/// edit is picked up on the next `fable_start`; a live game's card is NOT
/// re-seated here (mirrors fable_card_raw_set's contract).
#[tauri::command]
fn fable_card_raw_set_by_id(card_id: String, xml: String, app: tauri::AppHandle) -> Result<(), String> {
    if xml.len() > 100_000 {
        return Err("Card exceeds 100 KB cap".into());
    }
    sim_card::parse_from_xml_str(&xml).map_err(|e| format!("Invalid card format: {e}"))?;
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("mkdir card folder: {e}"))?;
    let path = resolve_card_file(&cards_root, &card_id, "sim");
    write_atomic(&path, xml.as_bytes()).map_err(|e| format!("write card: {e}"))?;
    tracing::info!(card_id = %card_id, "fable_card_raw_set_by_id: .sim written to disk");
    Ok(())
}

/// Read the raw text of one of the active card's JSON files (`world` /
/// `player` / `npc`). `kind` is one of those three strings. The raw editor's
/// load path for the World / Player / NPC tabs. NotFound → empty string.
#[tauri::command]
fn fable_json_raw_get(
    kind: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let card_id = active_roleplay_card_id(&state)?;
    let ext = match kind.as_str() {
        "world" => "world.json",
        "player" => "player.json",
        "npc" => "npc.json",
        _ => return Err(format!("unknown kind '{kind}' (world|player|npc)")),
    };
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_card_file(&cards_root, &card_id, ext);
    match std::fs::read_to_string(&path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("read {ext}: {e}")),
    }
}

/// Validate + atomically write one of the active card's JSON files, then
/// recompose it into the LIVE `WorldSchema` so the edit applies immediately
/// (not just on next reload). `kind` ∈ {world, player, npc}. The raw editor's
/// save path for the World / Player / NPC tabs.
///
/// The recompose: read the live schema, apply the edited slice (overwrite the
/// matching subtree — `player_state` for player, the npc.* entities +
/// npc_registry/relationships/presences/offscreen_tasks for npc, everything
/// else for world), push the prior schema to the undo ring buffer, install,
/// persist via `save_schema`. Mirrors the trust class of `fable_schema_set`
/// (user-initiated, bypasses the immutability lock, undoable).
#[tauri::command]
async fn fable_json_raw_set(
    kind: String,
    json: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let card_id = active_roleplay_card_id(&state)?;
    let parsed: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("invalid JSON: {e}"))?;

    // Recompose into the live schema: take the current snapshot, overwrite the
    // edited slice, push undo, install, persist.
    let prior = state.fable_schema.lock().await.clone();
    let mut full = serde_json::to_value(&prior).map_err(|e| format!("serialize schema: {e}"))?;
    let full_obj = full
        .as_object_mut()
        .ok_or_else(|| "live schema serialized to non-object".to_string())?;
    match kind.as_str() {
        "world" => {
            // The world slice carries: all fields EXCEPT player_state + the
            // npc-grouped fields + npc.* entities. The raw editor shows the
            // full world.json (which the split wrote), so on save we apply
            // every key the edited JSON carries (overwriting the live value),
            // WITHOUT clobbering player_state/npc fields the world slice
            // doesn't own. Entities is special: world.json's entities are the
            // non-npc.* ones — replace only those, preserve npc.* in the live
            // schema.
            let edited = parsed
                .as_object()
                .ok_or_else(|| "world JSON must be an object".to_string())?;
            for (k, v) in edited {
                if k == "entities" {
                    // Replace non-npc.* entities with the edited set; keep npc.*.
                    let edited_ents = v
                        .as_object()
                        .ok_or_else(|| "world entities must be an object".to_string())?;
                    let live_ents = full_obj
                        .entry("entities".to_string())
                        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                    let live_map = live_ents
                        .as_object_mut()
                        .ok_or_else(|| "live entities not an object".to_string())?;
                    // Drop the live non-npc.* keys, then re-insert from edited.
                    live_map.retain(|k, _| k.starts_with("npc."));
                    for (ek, ev) in edited_ents {
                        if !ek.starts_with("npc.") {
                            live_map.insert(ek.clone(), ev.clone());
                        }
                    }
                } else if !matches!(
                    k.as_str(),
                    "player_state" | "npc_registry" | "relationships" | "presences"
                        | "offscreen_tasks"
                ) {
                    // A world-slice key: overwrite the live value.
                    full_obj.insert(k.clone(), v.clone());
                }
                // else: a key the world slice doesn't own — ignore (defensive).
            }
        }
        "player" => {
            // The player slice is `{ "player_state": {...} }`.
            let ps = parsed
                .get("player_state")
                .ok_or_else(|| "player JSON must have a player_state field".to_string())?;
            full_obj.insert("player_state".to_string(), ps.clone());
        }
        "npc" => {
            // The npc slice carries npc_registry/relationships/presences/
            // offscreen_tasks + an `entities` object of npc.* keys.
            let edited = parsed
                .as_object()
                .ok_or_else(|| "npc JSON must be an object".to_string())?;
            for key in ["npc_registry", "relationships", "presences", "offscreen_tasks"] {
                if let Some(v) = edited.get(key) {
                    full_obj.insert(key.to_string(), v.clone());
                }
            }
            if let Some(ents_val) = edited.get("entities") {
                let edited_ents = ents_val
                    .as_object()
                    .ok_or_else(|| "npc entities must be an object".to_string())?;
                let live_ents = full_obj
                    .entry("entities".to_string())
                    .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                let live_map = live_ents
                    .as_object_mut()
                    .ok_or_else(|| "live entities not an object".to_string())?;
                // Drop the live npc.* keys, then re-insert from edited.
                live_map.retain(|k, _| !k.starts_with("npc."));
                for (ek, ev) in edited_ents {
                    live_map.insert(ek.clone(), ev.clone());
                }
            }
        }
        _ => return Err(format!("unknown kind '{kind}' (world|player|npc)")),
    }

    let new_schema: schema::WorldSchema = serde_json::from_value(full)
        .map_err(|e| format!("recompose schema: {e}"))?;

    // Snapshot the prior schema for undo (mirrors fable_schema_set), then install.
    push_fable_history_snapshot(&state, prior).await;
    {
        let mut s = state.fable_schema.lock().await;
        *s = new_schema;
    }
    // Persist the recomposed schema to the three split files + return.
    save_schema(&app, &card_id, &state.fable_schema.lock().await.clone()).await;
    tracing::info!(card_id = %card_id, kind = %kind, "fable_json_raw_set: slice recomposed + persisted");
    Ok(())
}

/// Read the active card's `.codex` text (raw, for the raw editor) + as parsed
/// entries (for the prose dropdown). One IPC serves both: the dropdown reads
/// `entries`, the editor reads `raw`. `raw` is the file's verbatim text
/// (empty string when no `.codex` exists yet — a fresh card). `entries` is the
/// parsed `{title, tags, body}` list (empty Vec when none).
#[derive(serde::Serialize)]
struct FableCodexRead {
    raw: String,
    entries: Vec<FableCodexEntry>,
}
#[derive(serde::Serialize)]
struct FableCodexEntry {
    title: String,
    tags: Vec<String>,
    body: String,
}
#[tauri::command]
fn fable_codex_get(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<FableCodexRead, String> {
    let card_id = active_roleplay_card_id(&state)?;
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_card_file(&cards_root, &card_id, "codex");
    let raw = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read codex: {e}")),
    };
    let entries = codex::parse_compound_text(&raw, &card_id)
        .into_iter()
        .map(|e| FableCodexEntry {
            title: e.title,
            tags: e.tags,
            body: e.body,
        })
        .collect();
    Ok(FableCodexRead { raw, entries })
}

/// Read an EXPLICIT card's `.codex` by id (no active-game requirement). Mirrors
/// `fable_codex_get` for the New Game flow, where codex presence is detected
/// before any game is active (no seated card → `active_roleplay_card_id` would
/// fail). Same `FableCodexRead` shape (raw + entries); NotFound → empty raw +
/// empty entries (i.e. "no codex").
#[tauri::command]
fn fable_codex_get_by_id(card_id: String, app: tauri::AppHandle) -> Result<FableCodexRead, String> {
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_card_file(&cards_root, &card_id, "codex");
    let raw = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("read codex: {e}")),
    };
    let entries = codex::parse_compound_text(&raw, &card_id)
        .into_iter()
        .map(|e| FableCodexEntry {
            title: e.title,
            tags: e.tags,
            body: e.body,
        })
        .collect();
    Ok(FableCodexRead { raw, entries })
}

/// Atomically write the active card's `.codex` text (the raw editor's save
/// path for the Codex tab). No structural validation — `.codex` is freeform
/// authored prose; the parser is tolerant (entries without `---` fences parse
/// as one body-only entry). Created on first write.
#[tauri::command]
fn fable_codex_raw_set(
    text: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let card_id = active_roleplay_card_id(&state)?;
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("mkdir card folder: {e}"))?;
    let path = resolve_card_file(&cards_root, &card_id, "codex");
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("write codex: {e}"))?;
    tracing::info!(card_id = %card_id, "fable_codex_raw_set: .codex written");
    Ok(())
}

/// Write a sibling text file (`.intro` or `.codex`) under an EXPLICIT card_id's
/// folder. Used by the new Creators (NPC/World/Scenario) at CREATE time, when
/// no card is active yet (so the `active_roleplay_card_id`-keyed raw-file IPCs
/// don't apply). `ext` MUST be "intro" or "codex" (the only text siblings this
/// exposes; the `.sim` is written by `fable_write_card`, the JSON state files
/// by their own IPCs). Atomic write (temp+fsync+rename). Empty text is allowed
/// (writes an empty file / no-op for the intro). Created on first write.
#[tauri::command]
fn fable_card_sibling_write(
    card_id: String,
    ext: String,
    text: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if ext != "intro" && ext != "codex" {
        return Err(format!("unsupported sibling ext '{ext}': only intro/codex"));
    }
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("mkdir card folder: {e}"))?;
    let path = resolve_card_file(&cards_root, &card_id, &ext);
    write_atomic(&path, text.as_bytes()).map_err(|e| format!("write {ext}: {e}"))?;
    tracing::info!(card_id = %card_id, ext = %ext, "fable_card_sibling_write: sibling written");
    Ok(())
}

/// Set/replace a card's `<intro>` block — the Fable opening narrator beat that
/// lives as a SIBLING after `</sim_card>` in the `.sim` file (2026-08-13). Rust
/// owns the XML edit so the two-root shape (`<sim_card>` + `<intro>`) stays
/// well-formed under the parser's validation. Used by the Creator's dedicated
/// intro step (which runs AFTER `fable_write_card` made the card) + by the
/// import path that captures SillyTavern `first_mes`/`alternate_greetings`.
/// `text` empty → strips any existing `<intro>`/`<introduction>` sibling (clears
/// the beat). Validates via `parse_from_xml_str` BEFORE the atomic write so a
/// malformed edit never lands on disk.
fn cdata_escape(s: &str) -> String {
    s.replace("]]>", "]]]]><![CDATA[>")
}

#[tauri::command]
fn fable_card_set_intro(card_id: String, text: String, app: tauri::AppHandle) -> Result<(), String> {
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_card_file(&cards_root, &card_id, "sim");
    let existing = std::fs::read_to_string(&path)
        .map_err(|e| format!("read card for set_intro: {e}"))?;
    // Slice up to + including the closing `</sim_card>` — this drops any prior
    // sibling `<intro>`/`<introduction>` (the only elements that live after the
    // card). The `.codex`/`.world.json` siblings are separate FILES, untouched.
    let close = "</sim_card>";
    let end = existing
        .find(close)
        .ok_or_else(|| "card XML missing </sim_card>".to_string())?
        + close.len();
    let trimmed = text.trim();
    let mut out = String::from(&existing[..end]);
    if !trimmed.is_empty() {
        out.push_str("\n\n<intro><![CDATA[");
        out.push_str(&cdata_escape(trimmed));
        out.push_str("]]></intro>\n");
    } else {
        out.push('\n');
    }
    // Validate the rebuilt file through the real parser BEFORE the disk touch
    // (the same gate `fable_write_card` uses).
    sim_card::parse_from_xml_str(&out).map_err(|e| format!("set_intro rebuild invalid: {e}"))?;
    write_atomic(&path, out.as_bytes()).map_err(|e| format!("write card (set_intro): {e}"))?;
    tracing::info!(card_id = %card_id, len = trimmed.len(), "fable_card_set_intro: <intro> sibling written");
    Ok(())
}

/// Write a portrait/cover image as a sibling of an EXPLICIT card_id's `.sim`.
/// Used by the new Creators at CREATE time (after `fable_write_card` makes the
/// folder). Takes `bytes_b64: String` (base64-over-JSON — a bare `Vec<u8>` arg
/// poisons Tauri v2 command registration at startup, see anti-pattern #5) +
/// `ext` ("png"/"jpg", from the cropper's magic-byte detection). Validates the
/// ext against an allowlist. The portrait filename is fixed `portrait.<ext>`.
#[tauri::command]
fn fable_card_portrait_write(
    card_id: String,
    bytes_b64: String,
    ext: String,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let ext_lower = ext.to_lowercase();
    if ext_lower != "png" && ext_lower != "jpg" && ext_lower != "jpeg" {
        return Err(format!("unsupported portrait ext '{ext}': png/jpg only"));
    }
    let bytes = base64_decode(&bytes_b64)?;
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    std::fs::create_dir_all(&card_dir).map_err(|e| format!("mkdir card folder: {e}"))?;
    // Normalize jpeg → jpg for the filename.
    let file_ext = if ext_lower == "jpeg" { "jpg" } else { &ext_lower };
    let path = card_dir.join(format!("portrait.{file_ext}"));
    write_atomic(&path, &bytes).map_err(|e| format!("write portrait: {e}"))?;
    tracing::info!(card_id = %card_id, bytes = bytes.len(), "fable_card_portrait_write: portrait written");
    Ok(())
}

/// Resolve the absolute filesystem path to a card's portrait sibling (if one
/// exists), ready for the frontend's `convertFileSrc`. Returns None when no
/// `portrait.png`/`.jpg` sibling is present. Used by the Load menu's modal to
/// refresh the portrait in place after a crop/re-save without re-fetching the
/// whole card list. Best-effort: a stat error degrades to None.
#[tauri::command]
fn fable_card_portrait_url(card_id: String, app: tauri::AppHandle) -> Result<Option<String>, String> {
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    for ext in ["png", "jpg", "jpeg"] {
        let path = card_dir.join(format!("portrait.{ext}"));
        if path.is_file() {
            return Ok(Some(path.to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

// ─────────────────────────────────────────────────────────────────────────
// Fable BACKGROUND LIBRARY (2026-08-11): a user-importable image library at
// `apps/fable/images/backgrounds/` (GLOBAL, shared across cards) + a PER-CARD
// "active background" selection stored on `WorldSchema.background` (an
// `Option<String>` filename). The selection rides inside EVERY save — the full
// schema is snapshotted by `fable_save_now` + the per-turn autosave — and
// `fable_load_save` restores it, so leaving + continuing
// brings the background back with the save. The AI tracker never touches it
// (`apply_delta`/`merge_patch` omit it; `render_for_prompt` doesn't emit it).
//
// Import is CROPPER-driven: the frontend picks a file → reads bytes via
// `fable_player_portrait_read_bytes` (same-origin data URL, avoids canvas
// tainting) → runs the background cropper → sends the CROPPED bytes here as
// base64-over-JSON (anti-pattern #5: a bare `Vec<u8>` arg poisons IPC
// registration). `write_atomic` + a path-traversal guard on the filename.
// Resolution is uncapped — the frontend paints with `background-size: contain`,
// so the whole image always shows (non-16:9 / ultrawide → transparent bars =
// the black void). 1440p 16:9 is the documented sweet spot (UI hint only).
// ─────────────────────────────────────────────────────────────────────────

/// Resolve the backgrounds library root: `<install_root>/apps/fable/images/
/// backgrounds/`. Mirrors `resolve_fable_players_dir`. The dir is created
/// lazily by list/import (mkdir on first use), so a fresh install resolves
/// even before any import. GLOBAL — shared across all cards (only the
/// SELECTION on `WorldSchema.background` is per-card).
fn resolve_fable_backgrounds_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_apps_dir(app).join("fable").join("images").join("backgrounds")
}

/// One row in the backgrounds library — the shape the frontend gallery renders.
/// `name` is the filename STEM (no extension) for display; `filename` is the
/// real on-disk name (case/spaces preserved — it's the user's own import);
/// `path` is absolute, ready for the frontend's `convertFileSrc`; `ext` is the
/// normalized "png"/"jpg".
#[derive(serde::Serialize, Clone)]
struct BackgroundMeta {
    name: String,
    filename: String,
    path: String,
    ext: String,
}

/// Path-traversal + sanity guard shared by import/delete/active_set: the
/// filename must be a bare file name (no separators, no `..`, no drive/colon,
/// no NUL), and must end in a png/jpg/jpeg ext. Mirrors the `fable_card_delete`
/// id guard + the portrait ext allowlist.
fn validate_bg_filename(filename: &str) -> Result<(), String> {
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains(':')
        || filename.contains('\0')
        || filename.contains(std::path::MAIN_SEPARATOR)
    {
        return Err(format!("invalid background filename '{filename}'"));
    }
    let lower = filename.to_lowercase();
    if !(lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")) {
        return Err(format!("unsupported background ext '{filename}': png/jpg only"));
    }
    Ok(())
}

/// Build a `BackgroundMeta` from a library file path. The stem is the display
/// name; `ext` is normalized (jpeg → jpg) so the frontend never branches on it.
fn bg_meta_from_path(path: &std::path::Path) -> Option<BackgroundMeta> {
    let fname = path.file_name()?.to_str()?.to_string();
    let stem = path.file_stem()?.to_str()?.to_string();
    let ext_raw = path.extension()?.to_str()?.to_lowercase();
    let ext = if ext_raw == "jpeg" { "jpg".to_string() } else { ext_raw };
    Some(BackgroundMeta {
        name: stem,
        filename: fname,
        path: path.to_string_lossy().into_owned(),
        ext,
    })
}

/// List every importable background in the library (png/jpg). The dir is
/// created on first call so a fresh install returns an empty list (NOT an
/// error) — the gallery shows its empty state + the Import button. Sorted by
/// display name, case-insensitive (stable order for the gallery tiles).
#[tauri::command]
fn fable_backgrounds_list(app: tauri::AppHandle) -> Result<Vec<BackgroundMeta>, String> {
    let dir = resolve_fable_backgrounds_dir(&app);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir backgrounds: {e}"))?;
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(lower_ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
        else {
            continue;
        };
        if lower_ext != "png" && lower_ext != "jpg" && lower_ext != "jpeg" {
            continue;
        }
        // Skip the marker file if it ever lands inside the library dir (it
        // lives one level up, but defensive — never show it as a tile).
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with(".active_background"))
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(meta) = bg_meta_from_path(&path) {
            out.push(meta);
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Import a CROPPED background image (bytes from the frontend cropper) into
/// the library. Takes `bytes_b64: String` (base64-over-JSON — a bare `Vec<u8>`
/// arg poisons Tauri v2 IPC registration at startup, anti-pattern #5) +
/// `filename_stem` (the picker's original stem, threaded through the cropper so
/// the library shows the user's chosen name). Magic-byte validation is
/// authoritative: a `.JPEG` source normalizes to `.jpg`, a mis-named file with
/// real PNG magic bytes lands as `.png` (mirrors `fable_player_portrait_
/// upload_bytes`). Collisions overwrite atomically. Returns the new row for an
/// immediate gallery insert + auto-select.
#[tauri::command]
fn fable_background_import_bytes(
    bytes_b64: String,
    filename_stem: String,
    app: tauri::AppHandle,
) -> Result<BackgroundMeta, String> {
    let bytes = base64_decode(&bytes_b64)?;
    let detected = player::validate_image_magic(&bytes)?; // "png" | "jpg" (magic-byte authoritative)
    // Sanitize the caller-supplied stem: strip path separators / traversal so a
    // hostile or malformed stem can't escape the library dir. Preserve spaces +
    // case — it's the user's own filename stem + that's the gallery display name.
    let stem_clean: String = filename_stem
        .trim()
        .chars()
        .filter(|c| !matches!(*c, '/' | '\\' | ':' | '\0') && *c != std::path::MAIN_SEPARATOR)
        .collect();
    let stem = if stem_clean.is_empty() || stem_clean.contains("..") {
        // Defensive fallback for an empty / all-hostile stem.
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("background_{ms}")
    } else {
        stem_clean
    };
    let filename = format!("{stem}.{detected}");
    validate_bg_filename(&filename)?;
    let dir = resolve_fable_backgrounds_dir(&app);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir backgrounds: {e}"))?;
    let path = dir.join(&filename);
    write_atomic(&path, &bytes).map_err(|e| format!("write background: {e}"))?;
    tracing::info!(filename = %filename, bytes = bytes.len(), "fable_background_import_bytes: cropped background written");
    bg_meta_from_path(&path).ok_or_else(|| "imported background vanished".to_string())
}

/// Delete a background from the library by filename. The caller (frontend)
/// clears the active card's selection first via `fable_background_active_set
/// (None)` when the deleted file was the active selection; this command does
/// NOT touch `WorldSchema.background` — it only removes the library file.
/// Path-traversal guard before the join. Idempotent: a missing file is Ok.
#[tauri::command]
fn fable_background_delete(filename: String, app: tauri::AppHandle) -> Result<(), String> {
    validate_bg_filename(&filename)?;
    let dir = resolve_fable_backgrounds_dir(&app);
    let path = dir.join(&filename);
    match std::fs::remove_file(&path) {
        Ok(()) => tracing::info!(filename = %filename, "fable_background_delete: removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("delete background: {e}")),
    }
    Ok(())
}

/// Read the ACTIVE CARD's selected background + resolve it to a
/// `BackgroundMeta`. The selection lives on `WorldSchema.background` (per-card,
/// save-persistent). Returns None when no game is active, no selection is set,
/// OR the referenced library file no longer exists (a deleted background auto-
/// clears — the stage falls back to the black void). Never errors: a missing
/// game/selection/stat all degrade to None so the frontend never blocks on it.
#[tauri::command]
async fn fable_background_active_get(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Option<BackgroundMeta>, String> {
    if !fable_is_active(&state) {
        return Ok(None);
    }
    let filename = state.fable_schema.lock().await.background.clone();
    let Some(filename) = filename else {
        return Ok(None);
    };
    let path = resolve_fable_backgrounds_dir(&app).join(&filename);
    if !path.is_file() {
        return Ok(None);
    }
    Ok(bg_meta_from_path(&path))
}

/// Set (Some) or clear (None) the ACTIVE CARD's selected background. Writes
/// `WorldSchema.background` + persists the schema per-card (so the selection
/// survives card re-entry) — and because the full schema is snapshotted by
/// every save path, the selection rides into the next manual/quick/autosave
/// automatically. `Some(filename)` must reference an existing library file
/// (validated before the schema is touched, so the field never points at a
/// ghost). `None` clears the field (→ the stage reverts to the default black
/// void).
#[tauri::command]
async fn fable_background_active_set(
    filename: Option<String>,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if !fable_is_active(&state) {
        return Err("no fable game active: call fable_start first".to_string());
    }
    match filename {
        None => {
            {
                let mut s = state.fable_schema.lock().await;
                s.background = None;
            }
            tracing::info!("fable_background_active_set: cleared");
        }
        Some(name) => {
            validate_bg_filename(&name)?;
            let path = resolve_fable_backgrounds_dir(&app).join(&name);
            if !path.is_file() {
                return Err(format!("background '{name}' not in library"));
            }
            {
                let mut s = state.fable_schema.lock().await;
                s.background = Some(name.clone());
            }
            tracing::info!(filename = %name, "fable_background_active_set: selected");
        }
    }
    // Persist the edited schema per-card (best-effort, mirrors fable_schema_set).
    let roleplay_card_id = state
        .active_card_id
        .lock()
        .expect("active_card_id mutex")
        .clone();
    let schema_snapshot = state.fable_schema.lock().await.clone();
    save_schema(&app, &roleplay_card_id, &schema_snapshot).await;
    Ok(())
}

/// Sanitize a card display name for use in a Windows `.lnk` filename. Strips
/// the OS-forbidden filename chars (`<>:"/\|?*` + control chars) to spaces,
/// trims, drops trailing dots. Falls back to "Fable Card" if empty so the
/// filename is never blank. (A separate concern from `slugify_card_stem`,
/// which produces a URL-style slug — here we want a readable, human label.)
fn safe_shortcut_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => ' ',
            c if (c as u32) < 32 => ' ',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim();
    if cleaned.is_empty() {
        "Fable Card".to_string()
    } else {
        cleaned.to_string()
    }
}

/// The `.lnk` label for a card's shortcut: the card's parsed `<identity>
/// <name>`, falling back to the folder slug for pre-reorg (2026-08-01) cards
/// whose legacy top-level `<name>` the parser doesn't read (parse yields
/// "unknown") — so a shortcut is "Launch One Piece.lnk", never "Launch
/// unknown.lnk". Shared by `build_card_shortcut` (create) AND
/// `fable_card_delete` (Desktop reap) so the two always agree on the filename.
fn card_shortcut_label(sim_path: &std::path::Path, card_slug: &str) -> String {
    let card_name = sim_card::load_or_fallback(sim_path).name;
    let display_name = if card_name.trim().is_empty() || card_name == "unknown" {
        card_slug.to_string()
    } else {
        card_name
    };
    safe_shortcut_name(&display_name)
}

/// Build (or refresh) the `Launch <Card Name>.lnk` shortcut(s) for a card.
/// Always writes the `.lnk` into the card's own folder (so deleting the card
/// reaps it automatically via `remove_dir_all`); when `export_to_desktop` is
/// true, an additional copy is written to the user's Desktop.
///
/// Icon: if the card has a `portrait.png`, it's wrapped into a sibling
/// `portrait.ico` (Vista+ PNG-compressed icon) and used; otherwise the
/// shortcut falls back to `fable.exe`'s embedded F icon (cards with no
/// portrait, or a `.jpg` portrait which can't be embedded in ICO).
///
/// Shared by the `create_card_shortcut` IPC (manual / desktop export) AND the
/// auto-creation hook in `fable_write_card` (in-folder only, best-effort).
fn build_card_shortcut(
    app: &tauri::AppHandle,
    card_slug: &str,
    export_to_desktop: bool,
) -> Result<String, String> {
    // Same slug path-traversal guard as fable_card_delete.
    if card_slug.is_empty()
        || card_slug.contains('/')
        || card_slug.contains('\\')
        || card_slug.contains("..")
        || card_slug.contains(std::path::MAIN_SEPARATOR)
    {
        return Err("invalid card id".into());
    }
    let cards_root = resolve_fable_cards_dir(app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, card_slug);
    let sim_path = resolve_card_file(&cards_root, card_slug, "sim");
    if !sim_path.exists() {
        return Err(format!("no card with id '{card_slug}'"));
    }
    // Display name for the .lnk filename — see card_shortcut_label (shared
    // with fable_card_delete's Desktop reap so the two always agree).
    let label = card_shortcut_label(&sim_path, card_slug);
    let lnk_name = format!("Launch {label}.lnk");

    // Target fable.exe (sibling of whichever exe is running) + working dir =
    // the portable install root (same dir resolution the card folders use).
    let install_root = resolve_install_root(app);
    let fable_exe = install_root.join("fable.exe");
    if !fable_exe.is_file() {
        return Err(format!(
            "fable.exe not found next to the running app (looked in {})",
            install_root.display()
        ));
    }
    // QUOTE the slug: card folder names may contain spaces ("One Piece"), and
    // the .lnk Arguments string is re-split by CommandLineToArgvW on launch —
    // unquoted, `--card One Piece` parses as card="One" + a dropped "Piece"
    // (observed live: get_launch_context returned cardSlug "One"). Quoted,
    // it round-trips as the single argv token the parser expects.
    let args = format!("--card \"{card_slug}\"");

    // Icon: wrap portrait.png → portrait.ico (only PNG — ICO can't embed a
    // JPEG). Anything else (jpg / no portrait) → None → fable.exe's F icon.
    let icon_path: Option<std::path::PathBuf> = {
        let png_rel = portrait_path_for(&cards_root, card_slug);
        if png_rel.as_deref() == Some("portrait.png") {
            let png_abs = card_dir.join("portrait.png");
            match std::fs::read(&png_abs)
                .ok()
                .and_then(|bytes| shortcut::png_to_ico(&bytes))
            {
                Some(ico) => {
                    let ico_path = card_dir.join("portrait.ico");
                    if let Err(e) = write_atomic(&ico_path, &ico) {
                        tracing::warn!(card_id = %card_slug, err = %e, "portrait.ico write failed; shortcut falls back to F icon");
                        None
                    } else {
                        Some(ico_path)
                    }
                }
                None => None, // not a valid PNG → F icon fallback
            }
        } else {
            None // jpg / no portrait → F icon fallback
        }
    };

    // Primary .lnk in the card folder (auto-reaped on card delete).
    let card_lnk = card_dir.join(&lnk_name);
    shortcut::write_lnk(
        &fable_exe,
        &args,
        icon_path.as_deref(),
        &install_root,
        &card_lnk,
    )?;
    let mut written = vec![card_lnk.display().to_string()];

    // Optional Desktop copy (cleaned up explicitly by fable_card_delete since
    // it lives outside the card folder).
    if export_to_desktop {
        if let Some(desk) = shortcut::desktop_dir() {
            let desk_lnk = desk.join(&lnk_name);
            match shortcut::write_lnk(
                &fable_exe,
                &args,
                icon_path.as_deref(),
                &install_root,
                &desk_lnk,
            ) {
                Ok(()) => written.push(desk_lnk.display().to_string()),
                Err(e) => {
                    tracing::warn!(card_id = %card_slug, err = %e, "desktop shortcut write failed (card-folder copy still created)")
                }
            }
        } else {
            tracing::warn!(card_id = %card_slug, "Desktop dir unresolved; skipping desktop shortcut");
        }
    }

    tracing::info!(card_id = %card_slug, export_to_desktop, "card shortcut created");
    Ok(written.join("\n"))
}

/// Manual shortcut-creation IPC. In-folder `.lnk` always; Desktop copy when
/// `export_to_desktop` is true. (Auto-creation on card write goes through the
/// shared `build_card_shortcut` helper directly — in-folder only.)
#[tauri::command]
fn create_card_shortcut(
    card_slug: String,
    export_to_desktop: Option<bool>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    build_card_shortcut(&app, &card_slug, export_to_desktop.unwrap_or(false))
}

/// Delete a card's entire per-card folder (`cards/<id>/` — the `.sim` + every
/// sibling: `.intro`, `.codex`, `world.json`, `player.json`, `npc.json`,
/// `portrait.*`, `portrait.ico`, the `saves/` tree, AND the in-folder `Launch
/// <Name>.lnk`). Added 2026-08-05 for the LOAD menu's DELETE action; the
/// desktop `.lnk` (exported by `create_card_shortcut`) is reaped here too.
/// Mirrors `fable_player_delete`'s discipline: path-traversal guard on the id
/// + a confirm-the-folder-is-a-real-card check (the namesake `<id>.sim` must
/// exist) before `remove_dir_all`, so a stray id can't nuke an unrelated
/// sibling directory. Idempotent: a missing folder is Ok.
#[tauri::command]
fn fable_card_delete(card_id: String, app: tauri::AppHandle) -> Result<(), String> {
    // Path-traversal guard: the card id is a slug (lowercase + dashes), so any
    // separator / dot-segment is malformed or malicious. Reject before join.
    if card_id.is_empty()
        || card_id.contains('/')
        || card_id.contains('\\')
        || card_id.contains("..")
        || card_id.contains(std::path::MAIN_SEPARATOR)
    {
        return Err("invalid card id".into());
    }
    let cards_root = resolve_fable_cards_dir(&app)
        .ok_or_else(|| "no apps/fable/cards/ dir resolved".to_string())?;
    let card_dir = resolve_card_dir(&cards_root, &card_id);
    if !card_dir.exists() {
        return Ok(()); // idempotent
    }
    // Confirm the folder actually contains its namesake `<id>.sim` so a stray
    // id can't nuke an unrelated sibling directory. A missing .sim means the
    // folder isn't a valid card dir — refuse rather than guess.
    let sim_path = resolve_card_file(&cards_root, &card_id, "sim");
    if !sim_path.exists() {
        return Err(format!("no card with id '{card_id}'"));
    }
    // Reap the DESKTOP shortcut (if any) BEFORE remove_dir_all — the in-folder
    // .lnk + portrait.ico + saves tree are all caught by remove_dir_all below,
    // but a `Launch <Name>.lnk` exported to the Desktop lives outside the card
    // folder. Best-effort: a NotFound / unresolvable Desktop is not fatal.
    let lnk_name = format!("Launch {}.lnk", card_shortcut_label(&sim_path, &card_id));
    if let Some(desk) = shortcut::desktop_dir() {
        let desk_lnk = desk.join(&lnk_name);
        match std::fs::remove_file(&desk_lnk) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(card_id = %card_id, err = %e, "desktop shortcut cleanup failed (continuing)"),
        }
    }
    std::fs::remove_dir_all(&card_dir)
        .map_err(|e| format!("could not delete card: {e}"))?;
    tracing::info!(card_id = %card_id, "card deleted");
    Ok(())
}

/// A single entity-key change in a rollback diff (1-click undo, 2026-07-27).
/// `before`/`after` are `Option` because the diff covers set, change, AND
/// delete (None = key absent in that snapshot). Values are `serde_json::Value`
/// to mirror the widened `WorldSchema.entities` type (2026-08-11).
#[derive(Clone, Debug, serde::Serialize)]
struct EntityDiff {
    key: String,
    before: Option<serde_json::Value>,
    after: Option<serde_json::Value>,
}

/// The structured diff between two consecutive `fable_schema` snapshots.
/// Emitted by `fable_rollback` so the frontend drawer can visually render
/// exactly what changed (e.g. "Gold: 100 → 80", "Marcus: tavern → blacksmith",
/// "Iron Sword: removed"). Pure value type — computed by `diff_schemas`.
#[derive(Clone, Debug, serde::Serialize)]
struct RollbackDiff {
    entities_changed: Vec<EntityDiff>,
    summary_changed: bool,
    /// The schema's recent_events grew or shrank (rare for a single undo).
    events_count_before: usize,
    events_count_after: usize,
}

/// Compute the diff between two `WorldSchema` snapshots. Pure fn — no locks,
/// no I/O. Used by `fable_rollback` to report what was undone. Entity keys
/// are unioned; for each, before/after are looked up (None = absent).
/// Summary + recent_events counts are compared directly. PlayerState / clock
/// / immutable_keys / scene_pacing changes are NOT surfaced in the diff
/// (kept to the user-visible "what the world looks like" surface; a future
/// revision can extend if needed).
fn diff_schemas(before: &schema::WorldSchema, after: &schema::WorldSchema) -> RollbackDiff {
    use std::collections::HashSet;
    let keys: HashSet<&str> = before
        .entities
        .keys()
        .chain(after.entities.keys())
        .map(|s| s.as_str())
        .collect();
    let mut entities_changed: Vec<EntityDiff> = keys
        .into_iter()
        .filter_map(|k| {
            let b = before.entities.get(k).cloned();
            let a = after.entities.get(k).cloned();
            if b == a {
                None
            } else {
                Some(EntityDiff {
                    key: k.to_owned(),
                    before: b,
                    after: a,
                })
            }
        })
        .collect();
    // Sort for deterministic IPC output (frontend may diff consecutive
    // rollbacks; stable order keeps the diff meaningful).
    entities_changed.sort_by(|a, b| a.key.cmp(&b.key));
    RollbackDiff {
        entities_changed,
        summary_changed: before.summary != after.summary,
        events_count_before: before.recent_events.len(),
        events_count_after: after.recent_events.len(),
    }
}

/// Roll the game's world schema back to its state before the last mutation
/// (1-click undo, 2026-07-27). Pops the last snapshot from
/// `fable_schema_history` and restores it as the live `fable_schema`. The
/// diff between the post-rollback (popped) state and the pre-rollback (live)
/// state is emitted as a `fable_rollback` Tauri event AND returned so the
/// UI can show "Gold: 100 → 80" etc. Bypasses the immutability lock, same
/// precedent as `fable_load_save` (the §5 contract applies to LLM deltas,
/// not user-initiated restore). Errors if the history is empty.
#[tauri::command]
async fn fable_rollback(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<RollbackDiff, String> {
    // Bail when no game is active (undo is meaningless on the title screen).
    // Use `fable_is_active` (active_fable_card check), NOT `fable_engine` —
    // under the §2B VRAM swap-lock the engine slot is `None` whenever chat/
    // schema holds the lease, which is the default state between turns. The
    // prior `fable_engine.is_none()` check made rollback unreachable in the
    // common case (2026-07-27 playtest finding). Mirrors `player_state_get`.
    if !fable_is_active(&state) {
        return Err("no fable game active: call fable_start first".to_string());
    }
    let prior = {
        let mut hist = state.fable_schema_history.lock().await;
        hist.pop_back()
    };
    let prior = prior.ok_or_else(|| "nothing to roll back: history is empty".to_string())?;
    let diff = {
        let live = state.fable_schema.lock().await;
        // diff_schemas(prior=before-rollback-state, live=after-rollback-state):
        // the diff describes what will change WHEN we restore `prior`. The
        // emitted event reports "before/after" from the player's POV (what
        // they currently see → what they'll see post-undo), so we pass
        // (prior, live).
        diff_schemas(&prior, &live)
    };
    // Restore: wholesale overwrite, same shape as fable_load_save. Bypasses
    // the immutability lock by design (user-initiated restore).
    *state.fable_schema.lock().await = prior;
    tracing::info!(
        entities_changed = diff.entities_changed.len(),
        summary_changed = diff.summary_changed,
        "fable_rollback: restored prior world state"
    );
    let _ = app.emit("fable_rollback", &diff);
    Ok(diff)
}

/// Return the current depth of the schema history ring buffer. The frontend
/// uses this to enable/disable the undo button (depth == 0 → nothing to undo).
#[tauri::command]
async fn fable_history_depth(state: tauri::State<'_, AppState>) -> Result<usize, String> {
    Ok(state.fable_schema_history.lock().await.len())
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
            variants: m.variants.clone(),
            active_idx: m.active_idx,
            timestamp: m.timestamp,
            reasoning: m.reasoning.clone(),
        })
        .collect();
    *state.fable_session.lock().await = save.session;
    *state.fable_schema.lock().await = save.schema;
    clear_fable_history(&state).await;
    tracing::info!(save_id = %meta.save_id, "game state loaded");
    // intro is None on a save-load: resumed games render their feed from
    // `messages`, not from the card's opening beat. Only fresh starts
    // (enter_fable_session) surface the intro.
    Ok(FableLoadResult { meta, messages, intro: None })
}

/// Result of `fable_load_save`: the save meta + a flat list of the loaded
/// messages so the UI can re-render its dialogue feed in one round-trip.
#[derive(Debug, Clone, serde::Serialize)]
struct FableLoadResult {
    meta: fable_save::SaveMeta,
    messages: Vec<FableLoadMessage>,
    /// The card's full intro text (untruncated) from the sibling `.intro`
    /// file. Surfaced so the UI can render the first narrator beat on a FRESH
    /// game (no resumed messages yet) without a second IPC round-trip. `None`
    /// when the card has no `.intro`. The intro is read ONCE here + NEVER
    /// injected into the cached system prompt — it's a one-turn seed (2026-08-05:
    /// moved out of the cached `<sim_card>` to keep the per-turn KV cache lean).
    /// NOTE: this is the FULL text — the per-card `opening_scene_preview` on
    /// `FableCardMeta` (capped at 240 chars, also read from the `.intro`) is
    /// only for the launcher card picker, NOT for the first beat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intro: Option<String>,
}

/// One loaded message, role as a lowercase string (matches `session::Role`'s
/// `rename_all = "lowercase"` serialization but trimmed to just role+content
/// so we don't leak `raw_output`/`reasoning`/`id` to the UI).
///
/// `variants` + `active_idx` carry the swipeable-reroll state (2026-07-29) so
/// the feed can render the `1/N` control + recover older variants on swipe.
/// `variants` holds the INACTIVE siblings only (parallel to
/// `session::Message::variants`); `content` is the active variant. Omitted from
/// serialization when empty so legacy frontends/JSON stay clean.
///
/// `timestamp` (2026-08-01): the message's epoch-millis stamp, surfaced for
/// the hover-only message header's time line. Decorative-only (the UI omits
/// the line when 0/absent); never reaches inference. `#[serde(default)]` so a
/// value built without it (none today, but defensive) serializes cleanly.
#[derive(Debug, Clone, serde::Serialize)]
struct FableLoadMessage {
    role: &'static str,
    content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    variants: Vec<String>,
    #[serde(default)]
    active_idx: usize,
    #[serde(default)]
    timestamp: i64,
    /// The assistant turn's thought channel. Post-2026-08-07 override the
    /// narrator is the API (never thinks), so this is always "" on the wire —
    /// kept for struct/serde compatibility with saved sessions. The player-
    /// facing reasoning UI was removed; the local Wupi chat still thinks but
    /// its reasoning is never surfaced.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    reasoning: String,
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

// =============================================================
// FABLE EDIT / REROLL / REWIND (UX chat controls — 2026-07-27)
//
// Three mutators over `fable_session` + the world schema. They touch the
// message history; the schema ring buffer (`AppState::fable_schema_history`)
// is owned by `fable_rollback` — pushes happen INSIDE the commands that
// displace live state (swipe_variant, the edit re-track), so each returns
// `schema_pop_count` = 0 and there is nothing for the caller to pop:
//   edit_message           → assistant: edit + TRACKER RE-TRACK (2026-08-14,
//                            revert to base_schema + re-run the local
//                            tracker, storing the fresh snapshot into the
//                            variant binding); user: pure prose swap
//   reroll_last_turn       → validation gate; the fable_send(reroll) call
//                            that follows does the variant bookkeeping
//   rewind_and_edit_user   → N assistant turns truncated away (the caller
//                            pops the ring buffer that many times)
//
// The commands return the new `messages[]` (same shape as
// `fable_load_save`) so the frontend can rebuild the feed via
// `loadHistory`, then re-invoke `fable_send(text, { regenerate: true })`
// for rewind-and-edit. NO command re-triggers API narration — the only
// local-model inference is the edit re-track's tracker pass.
//
// Core logic is split into pure helpers (apply_*) that take `&mut
// Conversation` and are unit-tested without Tauri plumbing; the
// `#[tauri::command]` wrappers are thin shells that lock + call the
// helper + persist.
// =============================================================

/// Return shape for the three mutation commands. Mirrors `FableLoadResult`'s
/// `messages` field (role as lowercase string, content trimmed — no
/// `raw_output`/`reasoning`/`id`/`timestamp` leaked to the UI) plus
/// `schema_pop_count` so the parallel schema-ring-buffer work can pop the
/// matching number of snapshots when it lands.
#[derive(Debug, Clone, serde::Serialize)]
struct EditResponse {
    messages: Vec<FableLoadMessage>,
    schema_pop_count: usize,
}

/// In-place content overwrite for either a user or assistant message. Does
/// NOT change role/id/timestamp. Clears `raw_output` so a stale raw can't
/// desync a future KV-cache-coherent re-render (the edited content is now
/// the source of truth). Returns `Err` on out-of-bounds.
fn apply_edit(conv: &mut session::Conversation, index: usize, new_text: String) -> Result<(), String> {
    let len = conv.messages.len();
    let msg = conv
        .messages
        .get_mut(index)
        .ok_or_else(|| format!("edit_message: index {index} out of bounds (len {len})"))?;
    msg.content = new_text;
    msg.raw_output.clear();
    // Variant-mirror honesty (2026-08-14): overwrite the ACTIVE variant's
    // slot too when the message was previously rerolled — otherwise
    // `normalize_variants` on load restores `content` from the stale sibling
    // + the edit silently reverts across a reload (the same latent footgun
    // `apply_slice_splice` closes for the golden-pencil path).
    if !msg.variants.is_empty() {
        let ai = msg.active_idx.min(msg.variants.len() - 1);
        msg.variants[ai] = msg.content.clone();
        if let Some(slot) = msg.raw_outputs.get_mut(ai) {
            slot.clear();
        }
    }
    Ok(())
}

/// In-place splice of a regenerated selection into an assistant message —
/// the persistence step behind `fable_regenerate_slice` (the golden-pencil
/// partial regen). Overwrites `content` with `new_content` (pre + regenerated +
/// post, assembled by the caller) and clears `raw_output` (the regenerated
/// prose is the new source of truth; never the KV-cache-coherent raw).
///
/// **Variant-mirror honesty:** if the message was previously rerolled (non-
/// empty `variants`), the active variant's slot is overwritten too — otherwise
/// `normalize_variants` on load would revert the splice to the stale sibling
/// (the latent footgun `apply_edit` leaves open for rerolled messages; the
/// slice path closes it because a partial regen silently reverting across
/// reload would be especially confusing). Does NOT touch role/id/timestamp/
/// base_schema. NO schema mutation, NO tracker (this is a pure prose swap).
fn apply_slice_splice(
    conv: &mut session::Conversation,
    index: usize,
    new_content: String,
) -> Result<(), String> {
    let len = conv.messages.len();
    let msg = conv
        .messages
        .get_mut(index)
        .ok_or_else(|| format!("fable_regenerate_slice: index {index} out of bounds (len {len})"))?;
    msg.content = new_content;
    msg.raw_output.clear();
    if !msg.variants.is_empty() {
        let ai = msg.active_idx.min(msg.variants.len() - 1);
        msg.variants[ai] = msg.content.clone();
        if let Some(slot) = msg.raw_outputs.get_mut(ai) {
            slot.clear();
        }
    }
    Ok(())
}

/// Clean + splice a regenerated slice into its surrounding text — the pure
/// post-processing behind `fable_regenerate_slice`. Strips any Gemma4 channel
/// marker (`schema::extract_reply_channel`) + any accidental narrator brackets
/// (`bracket_parser::parse` — the slice is prose-only; brackets are NEVER
/// applied, only REMOVED), trims leading/trailing whitespace so the splice
/// butts cleanly against `pre`/`post`, then concatenates. Returns `None` when
/// the cleaned span is empty (the caller treats this as a soft error rather
/// than splicing an empty string, which would delete the highlighted passage).
fn clean_and_splice_slice(pre: &str, regen_raw: &str, post: &str) -> Option<String> {
    let cleaned = schema::extract_reply_channel(regen_raw);
    let parsed = bracket_parser::parse(&cleaned);
    let regen = parsed.prose.trim();
    if regen.is_empty() {
        return None;
    }
    Some(format!("{pre}{regen}{post}"))
}

/// Validate that the last message is an assistant turn (the reroll contract).
/// The actual stashing of the old content into a swipeable variant now happens
/// inside `fable_send` (the `reroll=true` path) so the generation + variant
/// install happen atomically in one pass. Returns `Err` if there's nothing to
/// reroll.
fn apply_reroll(conv: &session::Conversation) -> Result<(), String> {
    match conv.messages.last() {
        Some(m) if m.role == session::Role::Assistant => Ok(()),
        Some(_) => Err("reroll_last_turn: last message is not an assistant turn".into()),
        None => Err("reroll_last_turn: conversation is empty".into()),
    }
}

/// Truncate right after `index` (drops everything past the target user
/// message), then overwrite the target with `new_text`. The edited user
/// message is now the last message. Counts how many assistant turns were
/// removed by the truncation and returns that count so the caller can pop
/// the schema ring buffer in step. Returns `Err` if `index` is out of
/// bounds or doesn't point at a user message.
fn apply_rewind_and_edit(
    conv: &mut session::Conversation,
    index: usize,
    new_text: String,
) -> Result<usize, String> {
    let len = conv.messages.len();
    let target_role = conv
        .messages
        .get(index)
        .map(|m| m.role)
        .ok_or_else(|| format!("rewind_and_edit_user: index {index} out of bounds (len {len})"))?;
    if target_role != session::Role::User {
        return Err(format!(
            "rewind_and_edit_user: index {index} is not a user message (role {target_role:?})"
        ));
    }
    // Count assistant turns that will be removed by the truncation BEFORE we
    // mutate, so the count is computed on the pre-truncation vector.
    let deleted_assistant = conv.messages[index + 1..]
        .iter()
        .filter(|m| m.role == session::Role::Assistant)
        .count();
    conv.messages.truncate(index + 1);
    // Now overwrite the target (still at `index`, now the last message).
    apply_edit(conv, index, new_text)?;
    Ok(deleted_assistant)
}

/// Render a snapshot of the conversation as the wire shape the frontend
/// expects (lowercase role + content, no internals). Mirrors the projection
/// used by `fable_load_save`.
fn project_messages(conv: &session::Conversation) -> Vec<FableLoadMessage> {
    conv.messages
        .iter()
        .map(|m| FableLoadMessage {
            role: match m.role {
                session::Role::User => "user",
                session::Role::Assistant => "assistant",
                session::Role::System => "system",
            },
            content: m.content.clone(),
            variants: m.variants.clone(),
            active_idx: m.active_idx,
            timestamp: m.timestamp,
            reasoning: m.reasoning.clone(),
        })
        .collect()
}

/// Resolve the active fable card_id for persistence. Errors if no game is
/// seated (the mutation commands require an active game — they're meaningless
/// without one).
fn active_fable_card_id(state: &AppState) -> Result<String, String> {
    let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
    guard
        .as_ref()
        .map(|c| c.id.clone())
        .ok_or_else(|| "no active fable card: call fable_start first".to_string())
}

/// Persist `fable_session` to `cards/<card_id>/session.json`. Used by the
/// three mutation commands. Unlike the best-effort autosave in `fable_send`,
/// failures here are surfaced to the caller (the user explicitly initiated
/// the mutation — silent data loss would be a bug). MUST route through the
/// same `resolve_fable_cards_dir` root as `save_session`/`load_session` —
/// passing the fable root here previously wrote to `<fable_root>/<card_id>/
/// session.json` (outside `cards/`), so edits/swipes/rewinds silently
/// vanished across restart.
async fn persist_fable_session(
    app: &tauri::AppHandle,
    state: &AppState,
    card_id: &str,
) -> Result<(), String> {
    let cards_root = resolve_fable_cards_dir(app)
        .ok_or_else(|| "persist_fable_session: no apps/fable/cards/ dir resolved".to_string())?;
    let path = resolve_session_path(&cards_root, card_id);
    // Snapshot under the lock, drop the guard, then do the blocking write.
    // Atomic save (temp+fsync+rename) lives in `Conversation::save`.
    let snapshot = {
        let gs = state.fable_session.lock().await;
        gs.clone()
    };
    tokio::task::spawn_blocking(move || snapshot.save(&path))
        .await
        .map_err(|e| format!("persist_fable_session join: {e}"))?
        .map_err(|e| format!("persist_fable_session write: {e}"))
}

/// In-place edit for either a user or assistant message. The prose edit is
/// applied + persisted; for ASSISTANT messages the edit is new information,
/// so the turn's last track is undone + re-derived: `retrack_edited_
/// assistant_message` reverts the live schema to the message's `base_schema`
/// and re-runs the local tracker over the edited beat, storing the fresh
/// post-track snapshot into the variant↔schema binding (2026-08-14, Chloe).
/// User-beat edits stay pure prose swaps (the frontend routes user edits
/// through `rewind_and_edit_user`'s full truncate-and-regen instead).
/// schema_pop_count is 0 — the ring-buffer push happens inside the re-track
/// (the swipe_variant precedent), so there is nothing for the caller to pop.
#[tauri::command]
async fn edit_message(
    index: usize,
    new_text: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<EditResponse, String> {
    let card_id = active_fable_card_id(&state)?;
    let is_assistant = {
        let mut gs = state.fable_session.lock().await;
        apply_edit(&mut gs, index, new_text)?;
        gs.messages
            .get(index)
            .map(|m| m.role == session::Role::Assistant)
            .unwrap_or(false)
    };
    // Edit re-track (best-effort: a tracker failure never fails the edit —
    // the prose stands, the pre-edit schema is restored, a warn is logged).
    if is_assistant {
        retrack_edited_assistant_message(index, &state, &app).await;
    }
    let messages = {
        let gs = state.fable_session.lock().await;
        project_messages(&gs)
    };
    persist_fable_session(&app, &state, &card_id).await?;
    Ok(EditResponse { messages, schema_pop_count: 0 })
}

/// Re-run the local tracker over an EDITED assistant message (2026-08-14,
/// Chloe's "edit re-track"). An edit is new information — the world state
/// the tracker extracted from the OLD prose is stale — so this mirrors the
/// variant↔schema binding a reroll uses:
///   1. push the displaced live schema to the ring buffer (manual
///      `fable_rollback` can restore it — the swipe_variant precedent),
///   2. revert the live schema to the message's `base_schema` (undo the
///      last track; a legacy message with no base re-tracks on top, the
///      documented reroll fallback),
///   3. run the Stage-1 tracker pass over the edited turn's window
///      ([preceding player action, edited beat] — the WINDOW_TRACKER=2
///      shape fable_send uses, prose-capped the same way),
///   4. store the fresh post-track schema as `variant_schemas[active_idx]`
///      (seeding the list when empty), so a later swipe away + back
///      reinstalls exactly this state with ZERO local-model work.
/// The tracker runs under the Fable lease (engine spawn-or-reuse) + the
/// process-wide local-model turn lock — the same serialization fable_send's
/// Stage 1 uses, so it never overlaps a chat/schema decode. A cancelled or
/// failed pass is best-effort (fable_send's tracker-stage policy): the
/// prose edit stands, the displaced pre-edit schema is restored, + the
/// error is logged.
async fn retrack_edited_assistant_message(
    index: usize,
    state: &tauri::State<'_, AppState>,
    app: &tauri::AppHandle,
) {
    // The edited message's bookkeeping + the tracker window (one lock scope).
    let (player_action, tracker_window, base_schema) = {
        let gs = state.fable_session.lock().await;
        let Some(msg) = gs.messages.get(index) else { return };
        if msg.role != session::Role::Assistant {
            return;
        }
        // The turn window: the preceding player action (when the message
        // before the edit target is a user turn) + the edited beat — the
        // same 1-turn shape the tracker sees on a live turn. An intro beat
        // at index 0 (no predecessor) tracks from the beat alone.
        let player_action = index
            .checked_sub(1)
            .and_then(|i| gs.messages.get(i))
            .filter(|m| m.role == session::Role::User)
            .map(|m| m.content.clone());
        let start = index.saturating_sub(1);
        let tracker_window = cap_assistant_prose(
            gs.messages[start..=index].to_vec(),
            settings::TRACKER_ASSISTANT_CHAR_CAP,
        );
        (player_action, tracker_window, msg.base_schema.clone())
    };

    // (1) Displaced live schema → ring buffer; (2) revert to the pre-turn
    // base so the re-track doesn't double-mutate on top of the old roll.
    let displaced = state.fable_schema.lock().await.clone();
    if let Some(base) = &base_schema {
        push_fable_history_snapshot(state, displaced.clone()).await;
        *state.fable_schema.lock().await = base.clone();
    }

    // (3) Stage-1 tracker pass — the same prompt shape fable_send builds:
    // system prompt from the REVERTED world state (no turn directives — the
    // Rust Referees rolled once for the original turn and their dice stand;
    // no memory block — the tracker's job is mechanical bracket extraction,
    // codex retrieval is a per-turn narrator concern).
    let card = {
        let guard = state.active_fable_card.lock().expect("active_fable_card mutex");
        guard.clone()
    };
    let Some(card) = card else {
        tracing::warn!("edit re-track: no active card; skipping");
        *state.fable_schema.lock().await = displaced;
        return;
    };
    let pacing_text = player_action
        .clone()
        .unwrap_or_else(|| {
            tracker_window
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default()
        });
    let pacing = scene_pacing::evaluate(&pacing_text);
    let world_state = {
        let s = state.fable_schema.lock().await;
        let rendered = render_fable_world_state(&s, &[]);
        if rendered.trim().is_empty() { None } else { Some(rendered) }
    };
    let fable_prompts = state
        .fable_prompts
        .get()
        .expect("fable_prompts is set in setup()");
    let system_prompt = build_narrator_system_prompt(
        fable_prompts,
        &card,
        world_state.as_deref(),
        pacing,
        player_action.as_deref(),
        None,
    );
    let tracker_prompt = build_narrator_prompt(&system_prompt, &tracker_window);

    // Fresh cancel token registered in the shared slot (Bug #7 discipline)
    // so the composer stop / fable_stop can abort the pass; cleared on exit.
    let cancel: llm::CancelToken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
        *slot = Some(Arc::clone(&cancel));
    }

    tracing::info!("edit re-track: tracker pass starting (index {index})");
    let tracker_result: Result<(), String> = async {
        // Fable lease (engine spawn-or-reuse) + the local-model turn lock —
        // the identical serialization fable_send's Stage 1 runs under.
        let (_lease, engine) = acquire_fable_engine_leased(state, app).await?;
        let _model_guard = state.local_model_lock.lock().await;
        let noop_chunk: llm::ChunkFn = Arc::new(|_: &str| {});
        let reply_rx = engine
            .request_turn(tracker_prompt, noop_chunk, cancel.clone(), true)
            .map_err(|e| format!("tracker request_turn: {e}"))?;
        let reply = tokio::task::spawn_blocking(move || reply_rx.recv())
            .await
            .map_err(|e| format!("tracker join: {e}"))?
            .map_err(|e| format!("tracker channel: {e}"))?;
        if !reply.error.is_empty() {
            return Err(format!("tracker decode: {}", reply.error));
        }
        if reply.cancelled {
            return Err("tracker pass cancelled".into());
        }
        let cleaned = schema::extract_reply_channel(&reply.raw_output);
        let parsed = bracket_parser::parse(&cleaned);
        if parsed.commands.is_empty() {
            tracing::info!("edit re-track: tracker produced no bracket commands");
        } else {
            tracing::info!(count = parsed.commands.len(), "edit re-track: applying brackets");
            // Travel rejects are ignored — there is no narrator stage here
            // to carry them as directives.
            let _ = apply_phase3_bracket_commands(&parsed, state).await;
            apply_time_command_and_maybe_tick(&parsed, state).await;
        }
        Ok(())
    }
    .await;
    {
        let mut slot = state.active_fable_cancel.lock().expect("active_fable_cancel mutex");
        *slot = None;
    }

    if let Err(e) = tracker_result {
        tracing::warn!(
            error = %e,
            "edit re-track failed; restoring the pre-edit world schema (the prose edit stands)"
        );
        *state.fable_schema.lock().await = displaced;
        return;
    }

    // (4) Store the fresh post-track schema as the ACTIVE variant's snapshot
    // — the variant↔schema binding: a later swipe away + back reinstalls
    // exactly this state with zero local-model work. Seed the list when
    // empty (a never-rerolled message's active variant is implicitly 0).
    let post_track = state.fable_schema.lock().await.clone();
    {
        let mut gs = state.fable_session.lock().await;
        if let Some(msg) = gs.messages.get_mut(index) {
            if msg.role == session::Role::Assistant {
                if msg.variant_schemas.is_empty() {
                    msg.variant_schemas.push(post_track);
                } else {
                    let ai = msg.active_idx.min(msg.variant_schemas.len() - 1);
                    msg.variant_schemas[ai] = post_track;
                }
            }
        }
    }
    tracing::info!("edit re-track complete (index {index})");
}

/// Permanently remove a single message by index + shift subsequent messages
/// down (the same primitive the model-facing `fable_message_delete` tool uses:
/// `Conversation::remove_at`). No inference, no schema change (schema_pop_count
/// = 0 — world state is untouched; deleting a mid-history message does NOT
/// retroactively undo its tracker mutations, by design). Returns the new
/// message list. Destructive (no conversation-undo), so the drawer gates it
/// behind a two-step inline confirm.
#[tauri::command]
async fn delete_message(
    index: usize,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<EditResponse, String> {
    let card_id = active_fable_card_id(&state)?;
    let messages = {
        let mut gs = state.fable_session.lock().await;
        gs.remove_at(index)?;
        project_messages(&gs)
    };
    persist_fable_session(&app, &state, &card_id).await?;
    Ok(EditResponse { messages, schema_pop_count: 0 })
}

/// Validate that the last message is an assistant turn that can be re-rolled.
/// With swipeable variants (2026-07-29), the reroll no longer POPS the
/// assistant message — instead the old content is stashed as a swipeable
/// sibling inside `fable_send` (the `reroll=true` path) so the player keeps
/// every reroll. So this command is now a pure validation gate; the variant
/// bookkeeping + fresh generation all happen in the subsequent
/// `fable_send(text, { reroll: true })` call. Returns the current message
/// list (unchanged) + schema_pop_count = 0 (a prose re-roll no longer undoes
/// the turn's world-state mutation — the deterministic mechanics for the turn
/// stand; only the prose varies across variants).
#[tauri::command]
async fn reroll_last_turn(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<EditResponse, String> {
    let card_id = active_fable_card_id(&state)?;
    let messages = {
        let gs = state.fable_session.lock().await;
        apply_reroll(&gs)?;
        project_messages(&gs)
    };
    // No mutation here (no persist): the state is unchanged until fable_send's
    // reroll path runs. Persisting now would write identical bytes.
    let _ = card_id;
    let _ = app;
    Ok(EditResponse { messages, schema_pop_count: 0 })
}

/// Swipe to a different variant of an assistant message (the ‹ 1/N › UX,
/// 2026-07-29). Swaps the active `content`/`raw_output` with the sibling at
/// `variant_idx`, persists, returns the new message list + schema_pop_count
/// = 0 (a swipe changes only which prose is displayed — it does NOT undo
/// world state). Returns `Err` on a bad index or if the message has only one
/// variant.
#[tauri::command]
async fn swipe_variant(
    index: usize,
    variant_idx: usize,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<EditResponse, String> {
    let card_id = active_fable_card_id(&state)?;
    // Select the variant (swaps content/raw/active_idx) + capture its stored
    // schema (the variant↔schema binding, 2026-08-11) in ONE session-lock
    // scope. The schema install below runs in its own scope — no nested locks.
    let (install_schema, messages) = {
        let mut gs = state.fable_session.lock().await;
        let len = gs.messages.len();
        let msg = gs.messages.get_mut(index).ok_or_else(|| {
            format!("swipe_variant: index {index} out of bounds (len {len})")
        })?;
        if variant_idx >= msg.variant_count() {
            return Err(format!(
                "swipe_variant: variant_idx {variant_idx} out of range (count {})",
                msg.variant_count()
            ));
        }
        msg.select_variant(variant_idx);
        // The world state this variant produced. None for legacy messages with
        // no stored schema → graceful no-op (the alt prose shows, the schema
        // stays as whatever was last live).
        let install = msg.variant_schemas.get(variant_idx).cloned();
        (install, project_messages(&gs))
    };
    // Install the variant's stored schema as the live world state so the
    // paperdoll/inventory/etc. match the displayed prose — ZERO local-model
    // work (no re-tracking, per the binding's design). Push the displaced live
    // schema to the ring buffer so manual fable_rollback can still return.
    if let Some(schema) = install_schema {
        let snap = state.fable_schema.lock().await.clone();
        push_fable_history_snapshot(&state, snap).await;
        *state.fable_schema.lock().await = schema;
    }
    persist_fable_session(&app, &state, &card_id).await?;
    Ok(EditResponse { messages, schema_pop_count: 0 })
}

/// Branch the timeline: edit a user message from N turns ago. Truncates the
/// conversation right after the target index (dropping every subsequent
/// AI/user turn), overwrites the target with `new_text`, persists, returns
/// the new message list + schema_pop_count = (count of assistant turns the
/// truncation removed). The frontend rebuilds the feed and re-invokes
/// `fable_send(text, { regenerate: true })` with the edited text.
#[tauri::command]
async fn rewind_and_edit_user(
    index: usize,
    new_text: String,
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<EditResponse, String> {
    let card_id = active_fable_card_id(&state)?;
    let (messages, schema_pop_count) = {
        let mut gs = state.fable_session.lock().await;
        let pop = apply_rewind_and_edit(&mut gs, index, new_text)?;
        (project_messages(&gs), pop)
    };
    persist_fable_session(&app, &state, &card_id).await?;
    Ok(EditResponse { messages, schema_pop_count })
}

/// Resolve the roleplay scenario-card registry root, where per-card folders
/// live at `<exe_dir>/apps/fable/cards/`. Returns `None` if no such dir
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

/// Find a roleplay card by id within `apps/fable/cards/`. Returns an
/// error string (not a panic) if no card with that id exists.
///
/// **2026-08-01 folder layout:** cards live in per-card folders
/// (`cards/<id>/<id>.sim`). `iter_card_sim_paths` is the single walker both
/// enumerators share. First id match wins.
fn find_card_by_id(dir: &std::path::Path, target_id: &str) -> Result<sim_card::SimCard, String> {
    for path in iter_card_sim_paths(dir) {
        let card = sim_card::load_or_fallback(&path);
        if card.id == target_id && card.card_type == "roleplay" {
            return Ok(card);
        }
    }
    Err(format!("no roleplay card with id '{target_id}' in {}", dir.display()))
}

/// Enumerate every `.sim` file under a cards root. Each card lives in a
/// per-card folder `cards/<name>/`, and its namesake card is
/// `<name>/<name>.sim`. A folder holding a differently-named `.sim` is ignored
/// (a folder's namesake is its only card — defensive against a hand-edited
/// tree). Shared by `find_card_by_id` + `fable_cards_list` so the two walkers
/// can never disagree on what counts as a card. Order is filesystem-dependent
/// (callers sort).
fn iter_card_sim_paths(cards_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(cards_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        // Per-card folder: the namesake `<name>/<name>.sim`.
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let sim = path.join(format!("{name}.sim"));
        if sim.is_file() {
            out.push(sim);
        }
    }
    out
}

/// Resolve a card's per-card folder: `cards_root/<card_id>/`. Created on
/// demand by the writers; readers treat NotFound as "no folder yet".
fn resolve_card_dir(cards_root: &std::path::Path, card_id: &str) -> std::path::PathBuf {
    cards_root.join(card_id)
}

/// Resolve a sibling file inside a card's per-card folder:
/// `cards_root/<card_id>/<card_id>.<ext>` (e.g. `<id>.sim`, `<id>.codex`).
fn resolve_card_file(
    cards_root: &std::path::Path,
    card_id: &str,
    ext: &str,
) -> std::path::PathBuf {
    resolve_card_dir(cards_root, card_id).join(format!("{card_id}.{ext}"))
}

// === SAVED PLAYERS (2026-08-02) ===========================================
// A standalone, reusable player identity library at apps/fable/players/.
// Mirrors the per-card folder discipline (§6B) at a sibling root. Each
// player owns a folder `players/<id>/` holding `<id>.json` + an optional
// portrait image. See `player.rs` for the data model + validation.

/// Resolve the saved-players root: `<install_root>/apps/fable/players/`.
/// Simpler than `resolve_fable_cards_dir` (no legacy layout — players is
/// freshly-created user state, no migration surface). The dir is created
/// eagerly at boot (see the mkdir in setup), so this always resolves.
fn resolve_fable_players_dir(app: &tauri::AppHandle) -> std::path::PathBuf {
    resolve_apps_dir(app).join("fable").join("players")
}

/// Resolve a saved player's portrait to an absolute filesystem path (ready
/// for the frontend's `convertFileSrc`), or `None` when the player has no
/// portrait or the portrait file is missing/stale. Loads the player JSON,
/// reads the relative `portrait` filename, and stat-checks the sibling file.
/// Shared by `fable_player_get` (full player load) and `fable_active_card_get`
/// (chat portrait bridge) so the resolution logic lives in one place.
/// Best-effort: a missing player JSON or a stat error degrades to `None`.
fn load_player_portrait(app: &tauri::AppHandle, id: &str) -> Option<String> {
    let dir = resolve_fable_players_dir(app);
    let json_path = dir.join(id).join(format!("{id}.json"));
    let player = load_player_at(&json_path)?;
    let fname = player.portrait?;
    let abs = dir.join(id).join(&fname);
    if abs.is_file() {
        Some(abs.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Enumerate every saved player's namesake JSON under a players root.
/// Mirrors `iter_card_sim_paths`: each entry is a per-player folder
/// `players/<name>/`, and only the namesake file `<name>/<name>.json`
/// counts (a folder holding a differently-named `.json` is ignored —
/// defensive against a hand-edited tree). Returns the JSON paths; callers
/// parse + skip malformed entries.
fn iter_player_json_paths(players_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(players_root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let json = path.join(format!("{name}.json"));
        if json.is_file() {
            out.push(json);
        }
    }
    out
}

/// Load + parse a SavedPlayer from a namesake JSON path. Returns None on
/// any read/parse error (malformed entries are skipped by the list IPC,
// not fatal). Best-effort, mirroring `sim_card::load_or_fallback`'s
// graceful-degradation intent (but without a fallback stub — a bad
// player file simply isn't listed).
fn load_player_at(path: &std::path::Path) -> Option<player::SavedPlayer> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<player::SavedPlayer>(&bytes).ok()
}

/// Enumerate every saved player + return lightweight metadata for the
/// picker UI. Returns an empty Vec when no players dir exists or it
/// holds no valid players (the common case until the user authors one):
/// graceful, not an error. Malformed entries are skipped (logged).
#[tauri::command]
fn fable_players_list(app: tauri::AppHandle) -> Result<Vec<player::PlayerMeta>, String> {
    let dir = resolve_fable_players_dir(&app);
    let mut out = Vec::new();
    for path in iter_player_json_paths(&dir) {
        let player = match load_player_at(&path) {
            Some(p) => p,
            None => {
                tracing::warn!(path = %path.display(), "skipping malformed saved player");
                continue;
            }
        };
        // Portrait exists? Best-effort: the portrait filename is stored
        // relative; resolve against the player's folder.
        let has_portrait = player
            .portrait
            .as_deref()
            .and_then(|fname| path.parent().map(|d| d.join(fname)))
            .map(|p| p.is_file())
            .unwrap_or(false);
        out.push(player::PlayerMeta {
            id: player.id,
            name: player.name,
            has_portrait,
            // Surface gender + race on the mini-card so the picker grid can
            // render the ♂/♀ glyph + race subtitle without a per-tile
            // fable_player_get round-trip (Phase 4 Component 5, 2026-08-04).
            gender: player.gender,
            race: player.race,
            // Identity strip fields (2026-08-04: show all identity info except
            // gender at the bottom of each mini-card).
            age: player.age,
            height: player.height,
            weight: player.weight,
        });
    }
    // Sort by name for stable picker ordering (mirrors fable_cards_list).
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Load a single saved player by id, resolving the portrait to an
/// absolute path string for the frontend's `convertFileSrc`. Returns an
/// error string if the player folder/JSON is missing (the picker only
/// offers existing ids, so this is a stale-state edge case).
#[tauri::command]
fn fable_player_get(id: String, app: tauri::AppHandle) -> Result<player::SavedPlayer, String> {
    let dir = resolve_fable_players_dir(&app);
    let json_path = dir.join(&id).join(format!("{id}.json"));
    let mut player = load_player_at(&json_path)
        .ok_or_else(|| format!("no saved player with id '{id}'"))?;
    // Resolve the portrait filename to an absolute path via the shared
    // helper (same logic the chat portrait bridge uses).
    player.portrait = load_player_portrait(&app, &id);
    Ok(player)
}

/// The SavedPlayer identity attached to the active game, or `None` when no
/// player is attached (a game started without choosing a saved
/// player). The right-drawer Player tab reads this to render the identity +
/// backstory sections. Reuses `fable_player_get`'s load + portrait-resolve
/// path over `active_player_id` (set in `enter_fable_session`, cleared in
/// `fable_end`) so it stays correct across reload/resume — the frontend does
/// not retain the player id it passed to `fable_start`.
#[tauri::command]
fn fable_active_player_get(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<Option<player::SavedPlayer>, String> {
    let pid = state
        .active_player_id
        .lock()
        .expect("active_player_id mutex")
        .clone();
    let id = match pid {
        None => return Ok(None),
        Some(id) => id,
    };
    let dir = resolve_fable_players_dir(&app);
    let json_path = dir.join(&id).join(format!("{id}.json"));
    let mut player = load_player_at(&json_path)
        .ok_or_else(|| format!("attached player '{id}' not found on disk"))?;
    player.portrait = load_player_portrait(&app, &id);
    Ok(Some(player))
}

/// Write (create or overwrite) a saved player. Validates via
/// `player::validate_player` BEFORE any disk touch (the load-bearing
/// gate — a malformed player never lands on disk), slugifies the id,
/// creates the per-player folder, and atomic-writes the JSON. Returns
/// the freshly-written player's metadata. Reuses the same validate →
/// mkdir → atomic-write → resolve discipline as `fable_write_card`.
///
/// `id` is the caller-supplied slug (the frontend derives it from the
/// name via the same slug rule; the backend re-slugs to be defensive).
#[tauri::command]
fn fable_player_write(
    id: String,
    player: player::SavedPlayer,
    app: tauri::AppHandle,
) -> Result<player::PlayerMeta, String> {
    // 1. Validate structure + content BEFORE any disk touch.
    player::validate_player(&player)
        .map_err(|e| format!("Invalid player: {e}"))?;
    // 2. (Re-)slugify the id from the name. If the caller's slug is
    //    unusable, derive from the name; if THAT'S unusable, the
    //    validator already rejected an empty name, so this is a hard
    //    error only on a logic bug.
    let slug = player::slugify_player_id(&player.name)
        .or_else(|| player::slugify_player_id(&id))
        .ok_or_else(|| "could not derive a valid player id".to_string())?;
    let dir = resolve_fable_players_dir(&app);
    let player_dir = dir.join(&slug);
    let json_path = player_dir.join(format!("{slug}.json"));
    // 3. If the slug changed (rename), move the existing folder so the
    //    portrait + JSON stay together. No-op when the slug is unchanged.
    if slug != id {
        let old_dir = dir.join(&id);
        if old_dir.is_dir() && !player_dir.exists() {
            let _ = std::fs::rename(&old_dir, &player_dir);
        }
    }
    std::fs::create_dir_all(&player_dir)
        .map_err(|e| format!("could not create player folder: {e}"))?;
    // 4. Serialize with the resolved slug + timestamp.
    let mut to_write = player.clone();
    to_write.id = slug.clone();
    if to_write.created_at_ms == 0 {
        to_write.created_at_ms = current_unix_ms_i64();
    }
    let bytes = serde_json::to_vec_pretty(&to_write)
        .map_err(|e| format!("could not serialize player: {e}"))?;
    write_atomic(&json_path, &bytes)
        .map_err(|e| format!("could not write player: {e}"))?;
    // 5. Best-effort portrait flag for the returned meta.
    let has_portrait = to_write
        .portrait
        .as_deref()
        .map(|fname| player_dir.join(fname).is_file())
        .unwrap_or(false);
    Ok(player::PlayerMeta {
        id: slug,
        name: to_write.name.clone(),
        has_portrait,
        gender: to_write.gender.clone(),
        race: to_write.race.clone(),
        age: to_write.age.clone(),
        height: to_write.height.clone(),
        weight: to_write.weight.clone(),
    })
}

/// Upload (copy) a portrait image into a player's folder. `src_path` is
/// an OS-native path from the file dialog (read server-side — the
/// established WUPI pattern, no plugin-fs dependency). Validates the
/// image magic bytes BEFORE writing (rejects non-images even if the
/// dialog filter let one through). Writes via `write_atomic`, updates
/// the JSON's `portrait` field, returns the absolute portrait path for
/// `convertFileSrc`.
#[tauri::command]
fn fable_player_portrait_upload(
    id: String,
    src_path: String,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let dir = resolve_fable_players_dir(&app);
    let player_dir = dir.join(&id);
    if !player_dir.is_dir() {
        return Err(format!("no player folder for id '{id}'"));
    }
    // 1. Read the picked file's bytes server-side (no plugin-fs).
    let bytes = std::fs::read(&src_path)
        .map_err(|e| format!("could not read portrait: {e}"))?;
    // 2. Validate magic bytes BEFORE disk touch (the load-bearing gate).
    let ext = player::validate_image_magic(&bytes)?;
    let fname = format!("portrait.{ext}");
    let dest = player_dir.join(&fname);
    write_atomic(&dest, &bytes)
        .map_err(|e| format!("could not write portrait: {e}"))?;
    // 3. Update the JSON's portrait field so get/list reflect the truth.
    let json_path = player_dir.join(format!("{id}.json"));
    if let Some(mut p) = load_player_at(&json_path) {
        p.portrait = Some(fname.clone());
        if let Ok(b) = serde_json::to_vec_pretty(&p) {
            let _ = write_atomic(&json_path, &b);
        }
    }
    Ok(dest.to_string_lossy().into_owned())
}

/// Upload pre-cropped portrait BYTES (from the Player Creator's in-browser
/// cropper) into a player's folder. The cropper produces a JPEG whose content
/// is exactly the crop the user selected; this IPC writes those bytes directly
/// — no uncropped-original mismatch. Mirrors `fable_player_portrait_upload`
/// but takes bytes instead of a src path. Still validates the magic bytes
/// BEFORE any disk touch (the load-bearing gate), updates the JSON's
/// `portrait` field, + returns the absolute portrait path for `convertFileSrc`.
///
/// `bytes_b64` is the cropped image as a STANDARD base64 string (data-URL or
/// raw base64 — the prefix is stripped). base64-over-JSON is the well-trodden
/// Tauri v2 path; a bare `Vec<u8>` command arg does NOT deserialize through
/// the default invoke IPC and was the root cause of the 2026-08-04 "can't
/// connect to localhost" break (a Vec<u8> arg poisons command registration at
/// startup → the whole IPC layer fails → the webview never loads). The image
/// format is detected from the magic bytes and used verbatim for the filename
/// (the authoritative source — a caller-supplied ext would only be a hint).
#[tauri::command]
fn fable_player_portrait_upload_bytes(
    id: String,
    bytes_b64: String,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let dir = resolve_fable_players_dir(&app);
    let player_dir = dir.join(&id);
    if !player_dir.is_dir() {
        return Err(format!("no player folder for id '{id}'"));
    }
    // 0. Decode base64. Strip an optional data-URL prefix
    //    ("data:image/jpeg;base64,...") so the JS side may pass either form.
    let b64 = bytes_b64
        .split(',')
        .last()
        .unwrap_or("")
        .trim();
    let bytes = base64_decode(b64)
        .map_err(|e| format!("could not decode portrait bytes: {e}"))?;
    // 1. Validate magic bytes BEFORE disk touch (same gate as the path IPC).
    let detected = player::validate_image_magic(&bytes)?;
    // Use the detected ext for the filename (the magic bytes are
    // authoritative). Keep "jpg"/"png" only.
    let fname = format!("portrait.{detected}");
    let dest = player_dir.join(&fname);
    write_atomic(&dest, &bytes)
        .map_err(|e| format!("could not write portrait: {e}"))?;
    // 2. Update the JSON's portrait field so get/list reflect the truth.
    let json_path = player_dir.join(format!("{id}.json"));
    if let Some(mut p) = load_player_at(&json_path) {
        p.portrait = Some(fname.clone());
        if let Ok(b) = serde_json::to_vec_pretty(&p) {
            let _ = write_atomic(&json_path, &b);
        }
    }
    Ok(dest.to_string_lossy().into_owned())
}

/// Minimal RFC 4648 standard base64 decoder (no external dep). Used only by
/// `fable_player_portrait_upload_bytes` for the cropped-portrait payload. Tolerant
/// of missing padding + stray whitespace; rejects the URL-safe alphabet + any
/// non-base64 char. Not constant-time (not a secret).
fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    // Per-byte value 0..63, or None for invalid. Avoids a const-eval lookup
    // table (kept simple + obviously correct over clever).
    fn b64_val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut bits: u32 = 0;
    let mut count: u32 = 0;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for b in input.bytes() {
        // Skip whitespace + padding; the bit-accumulation handles the
        // missing-padding case naturally (trailing bits < 8 are dropped).
        if matches!(b, b'\n' | b'\r' | b' ' | b'\t' | b'=') {
            continue;
        }
        let v = b64_val(b).ok_or_else(|| format!("invalid base64 character: 0x{:02X}", b))?;
        bits = (bits << 6) | (v as u32);
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8 & 0xFF);
        }
    }
    Ok(out)
}

/// Minimal RFC 4648 standard base64 ENCODER (no external dep). The mirror of
/// `base64_decode`. Used by `fable_player_portrait_read_bytes` to hand a
/// dialog-picked image's bytes to the frontend as a same-origin data URL so
/// the in-browser cropper's canvas is never tainted by a cross-origin
/// `convertFileSrc` (`asset://`) URL (the 2026-08-05 root cause of the cropper
/// hanging on Confirm — `drawImage` tainted the canvas, `toBlob` threw a
/// SecurityError, no try/catch, modal froze). Not constant-time (not a secret).
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = bytes[i + 1] as u32;
        let b2 = bytes[i + 2] as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push('=');
    }
    out
}

/// Read a dialog-picked portrait file + return it as a same-origin
/// `data:image/<ext>;base64,...` data URL. The Player Creator's portrait picker
/// hands this data URL straight to the in-browser cropper (and uses it for the
/// local preview slot) so the cropper's `canvas.drawImage` → `toBlob` path
/// works — a `convertFileSrc` (`asset://`) URL is cross-origin to the webview
/// and taints the canvas, which was hanging the cropper on Confirm (see
/// `base64_encode` for the full root-cause note). Magic-byte-validated BEFORE
/// encoding (rejects non-images), reusing the same gate as the portrait-upload
/// IPCs. `src_path` is an OS-native path from the file dialog (read server-side
/// — the established WUPI pattern, no plugin-fs dependency).
#[tauri::command]
fn fable_player_portrait_read_bytes(src_path: String) -> Result<String, String> {
    let bytes = std::fs::read(&src_path)
        .map_err(|e| format!("could not read portrait: {e}"))?;
    let ext = player::validate_image_magic(&bytes)?;
    let mime = if ext == "png" { "image/png" } else { "image/jpeg" };
    Ok(format!("data:{mime};base64,{}", base64_encode(&bytes)))
}

/// Read a dialog-picked import file's UTF-8 text server-side. The creator
/// import path (.json character / lorebook) needs the raw text, but a
/// `convertFileSrc` (`asset://`) fetch is gated by the asset protocol scope
/// (apps/fable/... only — see tauri.conf.json `assetProtocol.scope`) → a
/// user-picked .json from anywhere else 403s, breaking the whole .json import.
/// Server-side read mirrors `fable_player_portrait_read_bytes` (the image
/// path): no plugin-fs dependency, no asset protocol, works for any
/// dialog-picked OS-native path. 4 MiB cap (an import larger than that is
/// almost certainly a mistake). Returns UTF-8 text for the frontend to
/// `JSON.parse`.
#[tauri::command]
fn creator_read_import_text(src_path: String) -> Result<String, String> {
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    let bytes = std::fs::read(&src_path)
        .map_err(|e| format!("could not read import file: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!(
            "import file is too large ({} bytes; max {})",
            bytes.len(),
            MAX_BYTES
        ));
    }
    String::from_utf8(bytes)
        .map_err(|e| format!("import file is not valid UTF-8 text: {e}"))
}

/// Delete a saved player: removes the whole `players/<id>/` folder (JSON +
/// portrait + any future per-player artifacts). Idempotent — a missing folder
/// is Ok (the picker's DELETE button may be re-clicked, or the folder was
/// already gone). Path-traversal hard gate: rejects any id containing a path
/// separator or `..` so the resolver can never escape `players/`. The Player
/// Picker's DELETE action is the sole caller today; it re-pulls the list on
/// success so the tile vanishes from the grid.
#[tauri::command]
async fn fable_player_delete(id: String, app: tauri::AppHandle) -> Result<(), String> {
    // Path-traversal guard: the id is a slug (lowercase + dashes), so any
    // separator / dot-segment is malformed or malicious. Reject before join.
    if id.is_empty()
        || id.contains('/')
        || id.contains('\\')
        || id.contains("..")
        || id.contains(std::path::MAIN_SEPARATOR)
    {
        return Err("invalid player id".into());
    }
    let dir = resolve_fable_players_dir(&app);
    let player_dir = dir.join(&id);
    if !player_dir.exists() {
        return Ok(()); // idempotent
    }
    // Best-effort: confirm the folder actually contains a `<id>.json` so a
    // stray id can't nuke an unrelated sibling directory. A missing JSON
    // means the folder isn't a valid player dir — refuse rather than guess.
    if !player_dir.join(format!("{id}.json")).exists() {
        return Err(format!("no saved player with id '{id}'"));
    }
    std::fs::remove_dir_all(&player_dir)
        .map_err(|e| format!("could not delete player: {e}"))?;
    tracing::info!(player_id = %id, "saved player deleted");
    Ok(())
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
    let Some(cards_root) = resolve_fable_cards_dir(app) else {
        tracing::warn!("save_session: no apps/fable/cards/ dir resolved");
        return;
    };
    // Ensure the per-card folder exists (a fresh card may have no folder yet).
    let card_dir = resolve_card_dir(&cards_root, card_id);
    let _ = std::fs::create_dir_all(&card_dir);
    let path = resolve_session_path(&cards_root, card_id);
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
    let cards_root = resolve_fable_cards_dir(app)?;
    let path = resolve_session_path(&cards_root, card_id);
    let path_cloned = path.clone();
    tokio::task::spawn_blocking(move || session::Conversation::load(&path_cloned))
        .await
        .ok()?
        .ok()
}

/// Persist the world-state schema off the Tokio worker pool. Mirrors
/// `save_session`: `WorldSchema::save_split` is atomic (temp + fsync + rename)
/// but synchronous, so `spawn_blocking` keeps the async runtime free.
///
/// **2026-08-01 folder layout:** the schema now persists as three sibling
/// files inside the card's folder — `world.json` + `player.json` + `npc.json`
/// — via `WorldSchema::save_split` (the in-memory struct stays one piece; the
/// disk form splits so the Player / World / NPC tabs each own a file). Only
/// the active game's schema persists (Wupi-assistant's schema stays ephemeral).
async fn save_schema(
    app: &tauri::AppHandle,
    card_id: &str,
    schema: &schema::WorldSchema,
) {
    let cards_root = match resolve_fable_cards_dir(app) {
        Some(d) => d,
        None => {
            tracing::warn!("save_schema: no apps/fable/cards/ dir resolved");
            return;
        }
    };
    let world_path = resolve_card_file(&cards_root, card_id, "world.json");
    let player_path = resolve_card_file(&cards_root, card_id, "player.json");
    let npc_path = resolve_card_file(&cards_root, card_id, "npc.json");
    let schema = schema.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = schema.save_split(&world_path, &player_path, &npc_path) {
            tracing::warn!(?e, "failed to persist world schema");
        }
    })
    .await;
}

/// Load a card-scoped world schema. Returns a fresh default `WorldSchema`
/// when no saved files exist (the `WorldSchema::load_split` NotFound path
/// handles a missing slice as its `#[serde(default)]`). Symmetric to
/// `save_schema`.
async fn load_schema(
    app: &tauri::AppHandle,
    card_id: &str,
) -> Option<schema::WorldSchema> {
    let cards_root = resolve_fable_cards_dir(app)?;
    let world_path = resolve_card_file(&cards_root, card_id, "world.json");
    let player_path = resolve_card_file(&cards_root, card_id, "player.json");
    let npc_path = resolve_card_file(&cards_root, card_id, "npc.json");
    tokio::task::spawn_blocking(move || {
        schema::WorldSchema::load_split(&world_path, &player_path, &npc_path)
    })
    .await
    .ok()?
    .ok()
}

/// `<cards_root>/<card_id>/session.json`. The per-card folder layout
/// (2026-08-01): each card owns its session as a sibling of its `.sim` /
/// `.codex` / `world.json`. Only roleplay game sessions persist today;
/// Wupi-assistant chat stays ephemeral per §5. The card_id is the folder name:
/// roleplay card ids are filesystem-safe (lowercased, derived from
/// `<metadata><id>` in `sim_card.rs`).
fn resolve_session_path(cards_root: &std::path::Path, card_id: &str) -> std::path::PathBuf {
    resolve_card_dir(cards_root, card_id).join("session.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_local_ctx_always_returns_with_api_constant() {
        // Wupi chat is LOCAL-ONLY (2026-08-08 override): the chat backend
        // ALWAYS runs at CTX_LOCAL_WITH_API (2048). The 4096 context is
        // retired for chat — it was for narrative, which the local model no
        // longer does. The source param is now read-ignored; both variants
        // return the same constant.
        let settings = WupiSettings { context_size: 8192, conversation_budget: 16000 };
        assert_eq!(
            effective_local_ctx(api::ModelSource::Local, &settings),
            settings::CTX_LOCAL_WITH_API,
            "Local mode must return CTX_LOCAL_WITH_API (chat is always local-only now)"
        );
        assert_eq!(
            effective_local_ctx(api::ModelSource::Api, &settings),
            settings::CTX_LOCAL_WITH_API,
            "API mode must return CTX_LOCAL_WITH_API (the chat context is always 2048)"
        );
        // The settings value must NOT leak through (read-ignored).
        let default = WupiSettings::default();
        assert_eq!(
            effective_local_ctx(api::ModelSource::Local, &default),
            settings::CTX_LOCAL_WITH_API
        );
    }

    #[tokio::test]
    async fn local_model_lock_serializes_consumers() {
        // The local-model turn lock (2026-08-08) guarantees at most ONE local
        // decode runs at any instant across chat / tracker / schema. Simulate
        // two concurrent consumers both holding the lock + a "currently
        // running" counter; assert it NEVER exceeds 1.
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let (lock, running, max_seen) =
                (Arc::clone(&lock), Arc::clone(&running), Arc::clone(&max_seen));
            handles.push(tokio::spawn(async move {
                let _g = lock.lock().await;
                let cur = running.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                max_seen.fetch_max(cur, std::sync::atomic::Ordering::SeqCst);
                // Yield a few times so the other task gets a chance to race.
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }
                running.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "local-model turn lock must serialize — at most ONE consumer at a time"
        );
    }

    // ── most_recent_continue_target (title CONTINUE resume target) ───────
    // Pins the CONTINUE contract: resume the freshest save for any New Game
    // world, autosaves included. Builds real save files via fable_save::write_save
    // so the test exercises the same on-disk layout the command reads.
    fn fake_card_for_continue(id: &str) -> sim_card::SimCard {
        // Mirrors fable_save.rs's fake_card() (SimCard has no Default derive).
        sim_card::SimCard {
            id: id.into(),
            name: id.into(),
            card_type: "roleplay".into(),
            subtype: None,
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            intro: String::new(),
            setting: Some("A test place.".into()),
            plot: None,
            tone: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            player_name: Some("Tester".into()),
            locations: Vec::new(),
            cast: Vec::new(),
            start: sim_card::CardStart::default(),
            custom_tags: Default::default(),
        }
    }

    #[test]
    fn continue_target_picks_most_recent_save_across_worlds() {
        let tmp = tempfile::tempdir().unwrap();
        // Two New Game worlds, each with a save. The newer one (newer_world)
        // should win regardless of which card it came from.
        let older = fake_card_for_continue("older_world");
        let newer = fake_card_for_continue("newer_world");
        fable_save::write_save(
            tmp.path(), &older, "save_a", "A",
            &session::Conversation::new(), &schema::WorldSchema::default(),
        ).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fable_save::write_save(
            tmp.path(), &newer, "save_b", "B",
            &session::Conversation::new(), &schema::WorldSchema::default(),
        ).unwrap();

        let target = most_recent_continue_target(tmp.path()).expect("a target exists");
        assert_eq!(target.card_id, "newer_world");
        assert_eq!(target.save_id, "save_b");
    }

    #[test]
    fn continue_target_includes_autosaves() {
        // The freshest save is an autosave. CONTINUE must resume it (the per-
        // turn checkpoint), not skip it for an older manual save.
        let tmp = tempfile::tempdir().unwrap();
        let card = fake_card_for_continue("world_with_auto");
        fable_save::write_save(
            tmp.path(), &card, "save_manual", "Manual",
            &session::Conversation::new(), &schema::WorldSchema::default(),
        ).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fable_save::write_save(
            tmp.path(), &card, fable_save::AUTOSAVE_ID, "Autosave",
            &session::Conversation::new(), &schema::WorldSchema::default(),
        ).unwrap();

        let target = most_recent_continue_target(tmp.path()).expect("a target exists");
        assert_eq!(target.save_id, fable_save::AUTOSAVE_ID);
        assert!(target.is_autosave);
    }


    /// NEVER includes tool declarations. The narrator is creative prose +
    /// bracket commands, never structured tool calls. This test pins the
    /// structural exclusion so a future refactor that accidentally routes
    /// tools into the narrator path fails loudly.
    #[test]
    fn fable_send_never_includes_tools() {
        let prompt = build_narrator_prompt("You are the narrator.", &[]);
        // No tool declaration markers in the narrator prompt.
        assert!(
            !prompt.contains("<|tool>declaration:"),
            "narrator prompt must never include tool declarations: {prompt}"
        );
        // No tool_call / tool_response protocol tokens either.
        assert!(
            !prompt.contains("<|tool_call>"),
            "narrator prompt must never include tool_call markers: {prompt}"
        );
        assert!(
            !prompt.contains("<|tool_response>"),
            "narrator prompt must never include tool_response markers: {prompt}"
        );
    }

    /// §7 sibling (2026-07-29): the narrator prompt builders MUST accept + render
    /// a `memory_block` (the retrieved fable.codex knowledge) as a
    /// `<retrieved_knowledge>` block. This is the core fix that makes the prompt
    /// distillation safe: the offloaded detail (bracket semantics, narrative
    /// discipline, common errors) lives in the codex and arrives on semantic
    /// match, keeping the inline prompt lean. Pin the wiring so a future refactor
    /// that drops the `memory_block` param fails loudly. Mirrors
    /// `fable_send_never_includes_tools` for the retrieval path.
    #[test]
    fn narrator_prompt_renders_retrieved_knowledge_block() {
        use crate::schema::{SceneMode, ScenePacing};
        use crate::sim_card::SimCard;

        let card = SimCard {
            id: "test".to_owned(),
            name: "Test Scenario".to_owned(),
            card_type: "roleplay".to_owned(),
            subtype: None,
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            intro: String::new(),
            setting: Some("A test.".to_owned()),
            plot: None,
            tone: Some("atmospheric".to_owned()),
            player_name: Some("Kaelen".to_owned()),
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            locations: Vec::new(),
            cast: Vec::new(),
            start: crate::sim_card::CardStart::default(),
            custom_tags: Default::default(),
        };
        let pacing = ScenePacing {
            mode: SceneMode::Exploration,
            spatial: 0,
            emotional: 0,
            kinetic: 0,
        };
        let block = "Reference knowledge\n<c title=\"Bracket Commands Reference\">the full per-command detail</c>";

        // Tracker path: the block must render between <world_state> and <scene_pacing>.
        let tracker_prompt = build_narrator_system_prompt(
            &prompts::FablePrompts::test_default(),
            &card,
            Some("gold: 100"),
            pacing,
            None,
            Some(block),
        );
        assert!(
            tracker_prompt.contains("<retrieved_knowledge>"),
            "tracker prompt must render the <retrieved_knowledge> block when memory_block is Some"
        );
        assert!(
            tracker_prompt.contains("Bracket Commands Reference"),
            "tracker prompt must contain the retrieved codex content"
        );
        // Ordering: retrieved_knowledge AFTER world_state, BEFORE scene_pacing.
        let ws = tracker_prompt.find("<world_state>").expect("world_state tag");
        let rk = tracker_prompt
            .find("<retrieved_knowledge>")
            .expect("retrieved_knowledge tag");
        let sp = tracker_prompt.find("<scene_pacing").expect("scene_pacing tag");
        assert!(ws < rk, "retrieved_knowledge must come after world_state");
        assert!(rk < sp, "retrieved_knowledge must come before scene_pacing");

        // None memory_block → no block emitted (zero baseline cost).
        let no_block_prompt = build_narrator_system_prompt(
            &prompts::FablePrompts::test_default(),
            &card,
            None,
            pacing,
            None,
            None,
        );
        assert!(
            !no_block_prompt.contains("<retrieved_knowledge>"),
            "tracker prompt must NOT render the block when memory_block is None"
        );

        // API narrator path: same wiring.
        let api_prompt = build_api_narrator_system_prompt(
            &prompts::FablePrompts::test_default(),
            &card,
            Some("gold: 100"),
            pacing,
            None,
            Some(block),
        );
        assert!(
            api_prompt.contains("<retrieved_knowledge>"),
            "API narrator prompt must render the <retrieved_knowledge> block"
        );
    }

    /// Phase 3 guard: the tool registry is non-empty (so the chat path CAN
    /// tool-call) but the narrator builder doesn't touch it. This double
    /// assertion catches the case where someone adds tools to the registry
    /// but forgets the narrator exclusion.
    #[test]
    fn tool_registry_populated_but_narrator_excluded() {
        let specs = tools::specs();
        assert!(
            !specs.is_empty(),
            "chat tool registry must be populated for the agent loop"
        );
        // The narrator builder takes no tools arg at all — compile-time
        // enforcement. This test just documents the runtime invariant.
        let prompt = build_narrator_prompt("narrator", &[]);
        for spec in &specs {
            assert!(
                !prompt.contains(&spec.name),
                "narrator prompt leaked tool name {:?}: {prompt}",
                spec.name
            );
        }
    }

    // --- Phase 1: Schema history ring buffer (2026-07-27) ---
    //
    // We test the pure pieces directly (`diff_schemas` + the cap math) and a
    // ring-buffer simulation that mirrors `push_fable_history_snapshot`'s
    // exact FIFO-eviction logic. Constructing a full `AppState` for these
    // would pull in the GPU backend + every IPC dependency — not worth it
    // for logic this isolated. The mutation-site instrumentation is verified
    // by `cargo test` compiling the call sites (any signature drift fails
    // the build) + by reading the inline `push_fable_history*` call sites.

    fn make_test_schema(entities: &[(&str, &str)]) -> schema::WorldSchema {
        let mut s = schema::WorldSchema::default();
        for (k, v) in entities {
            s.entities
                .insert((*k).to_owned(), serde_json::Value::String((*v).to_owned()));
        }
        s
    }

    #[test]
    fn ring_buffer_caps_at_five_with_fifo_eviction() {
        // Mirrors push_fable_history_snapshot's exact cap + eviction logic.
        let mut ring: std::collections::VecDeque<schema::WorldSchema> =
            std::collections::VecDeque::new();
        for i in 0..7 {
            if ring.len() >= 5 {
                ring.pop_front();
            }
            ring.push_back(make_test_schema(&[("turn", &i.to_string())]));
        }
        assert_eq!(ring.len(), 5, "ring buffer must cap at FABLE_HISTORY_CAP");
        // FIFO: oldest (turn 2) should be at the front, newest (turn 6) at the back.
        assert_eq!(
            ring.front().unwrap().entities.get("turn").and_then(|v| v.as_str()),
            Some("2"),
            "FIFO eviction must drop the OLDEST snapshot"
        );
        assert_eq!(
            ring.back().unwrap().entities.get("turn").and_then(|v| v.as_str()),
            Some("6"),
            "newest snapshot must be at the back"
        );
    }

    #[test]
    fn diff_schemas_detects_entity_value_change() {
        // Gold: 100 → 80 (the canonical "undo" use case).
        let before = make_test_schema(&[("gold", "100"), ("location", "tavern")]);
        let after = make_test_schema(&[("gold", "80"), ("location", "tavern")]);
        let diff = diff_schemas(&before, &after);
        assert_eq!(diff.entities_changed.len(), 1, "only the changed key should diff");
        let e = &diff.entities_changed[0];
        assert_eq!(e.key, "gold");
        assert_eq!(e.before.as_ref().and_then(|v| v.as_str()), Some("100"));
        assert_eq!(e.after.as_ref().and_then(|v| v.as_str()), Some("80"));
        assert!(!diff.summary_changed, "summary unchanged");
    }

    #[test]
    fn diff_schemas_detects_entity_added_and_removed() {
        // Marcus: tavern → blacksmith (value change) + iron_sword removed + key2 added.
        let before = make_test_schema(&[
            ("npc.marcus", "tavern"),
            ("item.iron_sword", "1"),
        ]);
        let after = make_test_schema(&[
            ("npc.marcus", "blacksmith"),
            ("item.health_potion", "3"),
        ]);
        let diff = diff_schemas(&before, &after);
        // All three keys differ (none are equal).
        assert_eq!(diff.entities_changed.len(), 3);
        // Find each by key (sorted output, but verify by lookup not position).
        let by_key: std::collections::HashMap<&str, &EntityDiff> = diff
            .entities_changed
            .iter()
            .map(|e| (e.key.as_str(), e))
            .collect();
        // Value change.
        assert_eq!(
            by_key["npc.marcus"].before.as_ref().and_then(|v| v.as_str()),
            Some("tavern")
        );
        assert_eq!(
            by_key["npc.marcus"].after.as_ref().and_then(|v| v.as_str()),
            Some("blacksmith")
        );
        // Removed (before Some, after None).
        assert_eq!(
            by_key["item.iron_sword"].before.as_ref().and_then(|v| v.as_str()),
            Some("1")
        );
        assert_eq!(by_key["item.iron_sword"].after, None);
        // Added (before None, after Some).
        assert_eq!(by_key["item.health_potion"].before, None);
        assert_eq!(
            by_key["item.health_potion"].after.as_ref().and_then(|v| v.as_str()),
            Some("3")
        );
    }

    #[test]
    fn diff_schemas_detects_summary_and_events_change() {
        let mut before = make_test_schema(&[("k", "v")]);
        before.summary = "Act 1 begins.".to_owned();
        before.recent_events.push("met the king".to_owned());
        let mut after = make_test_schema(&[("k", "v")]);
        after.summary = "Act 1 ends.".to_owned();
        after.recent_events.push("met the king".to_owned());
        after.recent_events.push("killed the dragon".to_owned());
        let diff = diff_schemas(&before, &after);
        assert!(diff.summary_changed, "summary change must surface");
        assert!(diff.entities_changed.is_empty(), "entities identical");
        assert_eq!(diff.events_count_before, 1);
        assert_eq!(diff.events_count_after, 2);
    }

    #[test]
    fn diff_schemas_empty_when_identical() {
        let s = make_test_schema(&[("k", "v")]);
        let diff = diff_schemas(&s, &s);
        assert!(diff.entities_changed.is_empty());
        assert!(!diff.summary_changed);
        assert_eq!(diff.events_count_before, diff.events_count_after);
    }

    // =============================================================
    // Fable edit / reroll / rewind-and-edit mutator tests (2026-07-27).
    // These cover the PURE helpers (apply_*) — the #[tauri::command]
    // wrappers are thin lock+call+persist shells over them and need no
    // Tauri plumbing to exercise. The helpers are the load-bearing logic.
    // =============================================================

    /// Build a Conversation with an alternating user/assistant/user/assistant
    /// sequence. Assistant turns carry a non-empty raw_output so the
    /// "edit clears raw_output" assertion is meaningful.
    fn make_test_conversation() -> session::Conversation {
        let mut c = session::Conversation::new();
        c.add_message(session::Role::User, "I attack the goblin.".into());
        c.add_assistant_turn(
            "The goblin falls.".into(),
            String::new(),
            "<raw>goblin dies</raw>".into(),
        );
        c.add_message(session::Role::User, "I loot the body.".into());
        c.add_assistant_turn(
            "You find 100 gold.".into(),
            String::new(),
            "<raw>100 gold</raw>".into(),
        );
        c.add_message(session::Role::User, "I leave the room.".into());
        c.add_assistant_turn(
            "The door creaks shut.".into(),
            String::new(),
            "<raw>door shuts</raw>".into(),
        );
        c
    }

    #[test]
    fn apply_edit_overwrites_content_and_clears_raw_output() {
        let mut c = make_test_conversation();
        // Edit the assistant turn at index 1 ("The goblin falls.").
        apply_edit(&mut c, 1, "The goblin staggers but lives.".into()).unwrap();
        assert_eq!(c.messages[1].content, "The goblin staggers but lives.");
        assert_eq!(c.messages[1].raw_output, "");
    }

    #[test]
    fn apply_edit_preserves_role_id_and_timestamp() {
        let mut c = make_test_conversation();
        let before = (
            c.messages[3].role,
            c.messages[3].id.clone(),
            c.messages[3].timestamp,
        );
        apply_edit(&mut c, 3, "You find 50 gold.".into()).unwrap();
        let after = (
            c.messages[3].role,
            c.messages[3].id.clone(),
            c.messages[3].timestamp,
        );
        assert_eq!(before, after, "edit must NOT touch role/id/timestamp");
    }

    #[test]
    fn apply_edit_out_of_bounds_errors() {
        let mut c = make_test_conversation();
        let len = c.messages.len();
        let err = apply_edit(&mut c, len, "x".into()).unwrap_err();
        assert!(err.contains("out of bounds"), "got: {err}");
        // Negative-equivalent: index at len is one past the end.
    }

    #[test]
    fn apply_edit_mirrors_active_variant_slot() {
        // Variant-mirror honesty (2026-08-14): editing a rerolled message
        // must overwrite the ACTIVE variant's slot too — otherwise
        // normalize_variants (the load path) restores `content` from the
        // stale sibling + the edit silently reverts across a reload.
        let mut c = make_test_conversation();
        {
            let last = c.messages.last_mut().unwrap();
            last.push_variant("variant B".into(), "<rawB>".into());
            assert_eq!(last.active_idx, 1);
        }
        apply_edit(&mut c, 5, "The door SLAMS shut.".into()).unwrap();
        let last = c.messages.last_mut().unwrap();
        assert_eq!(last.content, "The door SLAMS shut.");
        assert_eq!(last.variants[1], "The door SLAMS shut.", "active slot mirrored");
        assert_eq!(last.variants[0], "The door creaks shut.", "sibling untouched");
        assert_eq!(
            last.raw_outputs.get(1).map(String::as_str),
            Some(""),
            "active raw slot cleared"
        );
        // The load path must not revert the edit.
        last.normalize_variants();
        assert_eq!(last.content, "The door SLAMS shut.", "normalize keeps the edit");
    }

    #[test]
    fn apply_edit_on_single_variant_message_leaves_variants_empty() {
        // A never-rerolled message keeps its implicit single-variant shape
        // (empty variants Vec) — the edit must not seed a redundant copy.
        let mut c = make_test_conversation();
        apply_edit(&mut c, 1, "The goblin retreats.".into()).unwrap();
        assert!(c.messages[1].variants.is_empty());
        assert_eq!(c.messages[1].content, "The goblin retreats.");
    }

    #[test]
    fn apply_reroll_validates_last_is_assistant_without_mutating() {
        // Swipeable-variant reroll (2026-07-29): apply_reroll is now a pure
        // validation gate. It does NOT pop the message (the stashing happens
        // in fable_send's reroll=true path so the old content survives as a
        // swipeable sibling). The message count is unchanged.
        let c = make_test_conversation();
        assert_eq!(c.messages.len(), 6);
        apply_reroll(&c).unwrap();
        assert_eq!(c.messages.len(), 6, "apply_reroll no longer pops");
        // The last message is still the assistant turn — fable_send rerolls
        // it in place.
        assert_eq!(c.messages.last().unwrap().role, session::Role::Assistant);
        assert_eq!(c.messages.last().unwrap().content, "The door creaks shut.");
    }

    #[test]
    fn apply_reroll_errors_when_last_is_user() {
        let mut c = session::Conversation::new();
        c.add_message(session::Role::User, "hi".into());
        let err = apply_reroll(&mut c).unwrap_err();
        assert!(err.contains("not an assistant turn"), "got: {err}");
    }

    #[test]
    fn apply_reroll_errors_when_empty() {
        let mut c = session::Conversation::new();
        let err = apply_reroll(&mut c).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    // ---- swipe_variant (the ‹ 1/N › UX) ------------------------------------
    // These test the apply logic the `swipe_variant` IPC uses (select_variant
    // + bounds checks) without spinning up a full Tauri command harness —
    // mirroring the apply_edit / apply_reroll test pattern above.

    #[test]
    fn select_variant_swaps_active_content() {
        let mut c = make_test_conversation();
        // The last message ("The door creaks shut.") now has 3 variants after
        // two simulated rerolls; variant C is active.
        {
            let last = c.messages.last_mut().unwrap();
            last.push_variant("variant B".into(), "<rawB>".into());
            last.push_variant("variant C".into(), "<rawC>".into());
            assert_eq!(last.content, "variant C");
            assert_eq!(last.active_idx, 2);
        }
        // Swipe back to variant 0 (the original "door creaks shut").
        c.messages.last_mut().unwrap().select_variant(0);
        let last = c.messages.last().unwrap();
        assert_eq!(last.content, "The door creaks shut.", "swipe restores old text");
        assert_eq!(last.raw_output, "<raw>door shuts</raw>");
        assert_eq!(last.active_idx, 0);
        assert_eq!(last.variant_count(), 3, "all variants still present");
        // Swipe forward to variant 1.
        c.messages.last_mut().unwrap().select_variant(1);
        let last = c.messages.last().unwrap();
        assert_eq!(last.content, "variant B");
        assert_eq!(last.active_idx, 1);
    }

    #[test]
    fn select_variant_out_of_range_is_noop() {
        let mut c = make_test_conversation();
        let last = c.messages.last_mut().unwrap();
        last.push_variant("variant B".into(), "<rawB>".into());
        let content_before = last.content.clone();
        last.select_variant(99);
        assert_eq!(last.content, content_before, "out-of-range swipe is a no-op");
    }

    #[test]
    fn reroll_keeps_variants_across_simulated_reruns() {
        // The full reroll contract: two push_variants (simulating two rerolls
        // applied by fable_send's reroll=true path) yield 3 variants, all
        // retrievable, with the last generated one active.
        let mut c = make_test_conversation();
        {
            let last = c.messages.last_mut().unwrap();
            last.push_variant("second prose".into(), "<raw2>".into());
            last.push_variant("third prose".into(), "<raw3>".into());
        }
        let last = c.messages.last().unwrap();
        assert_eq!(last.variant_count(), 3);
        assert_eq!(last.content, "third prose", "latest reroll is active");
        assert_eq!(last.variant_at(0), Some("The door creaks shut."));
        assert_eq!(last.variant_at(1), Some("second prose"));
        assert_eq!(last.variant_at(2), Some("third prose"));
    }

    #[test]
    fn apply_rewind_truncates_after_index_and_overwrites_target() {
        let mut c = make_test_conversation();
        // Rewind to user index 2 ("I loot the body.") and edit it. The pop
        // count is asserted in `apply_rewind_counts_deleted_assistant_turns`;
        // here we only validate the truncation + overwrite shape.
        let _popped = apply_rewind_and_edit(&mut c, 2, "I check the walls.".into()).unwrap();
        // Truncated right after index 2 → only indexes 0,1,2 survive.
        assert_eq!(c.messages.len(), 3);
        // The edited message is now the last and is a user turn.
        assert_eq!(c.messages.last().unwrap().role, session::Role::User);
        assert_eq!(c.messages.last().unwrap().content, "I check the walls.");
        // The earlier messages are untouched.
        assert_eq!(c.messages[0].content, "I attack the goblin.");
        assert_eq!(c.messages[1].content, "The goblin falls.");
    }

    #[test]
    fn apply_rewind_counts_deleted_assistant_turns() {
        let mut c = make_test_conversation();
        // Rewinding from user index 0 drops indexes 1..5: assistants at
        // 1, 3, 5 → 3 assistant turns removed.
        let popped = apply_rewind_and_edit(&mut c, 0, "I enter the tavern.".into()).unwrap();
        assert_eq!(popped, 3, "should count 3 deleted assistant turns");
        // Rewinding from user index 2 drops indexes 3..5: assistant at
        // 3, 5 → 2 assistant turns removed.
        let mut c2 = make_test_conversation();
        let popped2 = apply_rewind_and_edit(&mut c2, 2, "x".into()).unwrap();
        assert_eq!(popped2, 2);
        // Rewinding from the LAST user index (4) drops only index 5 (the
        // final assistant turn) → 1 assistant turn removed.
        let mut c3 = make_test_conversation();
        let popped3 = apply_rewind_and_edit(&mut c3, 4, "x".into()).unwrap();
        assert_eq!(popped3, 1);
    }

    #[test]
    fn apply_rewind_errors_when_index_is_assistant() {
        let mut c = make_test_conversation();
        let err = apply_rewind_and_edit(&mut c, 1, "x".into()).unwrap_err();
        assert!(err.contains("not a user message"), "got: {err}");
    }

    #[test]
    fn apply_rewind_errors_when_out_of_bounds() {
        let mut c = make_test_conversation();
        let len = c.messages.len();
        let err = apply_rewind_and_edit(&mut c, len, "x".into()).unwrap_err();
        assert!(err.contains("out of bounds"), "got: {err}");
    }

    // --- slice regenerate (golden pencil, 2026-08-11) ---------------------
    // The new partial-regen primitive: highlight a span of an assistant
    // message → the API rewrites only that span in place. These pin the two
    // pure helpers behind fable_regenerate_slice (clean_and_splice_slice +
    // apply_slice_splice) + the prompt builder. The old regenerate_slice
    // feature (deleted 2026-07-31) had its own apply_regenerate_slice +
    // strip_code_fence helpers; those tests pinned code that no longer exists.

    #[test]
    fn clean_and_splice_slice_trims_and_concatenates() {
        let out = clean_and_splice_slice(
            "The goblin ",
            "  snarls and lunges.  ",
            " You parry.",
        ).unwrap();
        assert_eq!(out, "The goblin snarls and lunges. You parry.");
    }

    #[test]
    fn clean_and_splice_slice_strips_accidental_brackets() {
        // If the model strays into narrator brackets, they are REMOVED (the
        // slice is prose-only — brackets are never applied on this path).
        // [TIME …] is the canonical Time bracket (AGENTS.md §7), so a safe
        // parser fixture.
        let out = clean_and_splice_slice(
            "Before. ",
            "[TIME Day 1, 10:00]The dragon roars.",
            " After.",
        ).unwrap();
        assert!(!out.contains("[TIME"), "bracket leaked: {out}");
        assert!(out.contains("dragon roars"), "prose dropped: {out}");
        assert_eq!(out, "Before. The dragon roars. After.");
    }

    #[test]
    fn clean_and_splice_slice_empty_after_clean_is_none() {
        // Whitespace-only / bracket-only regen → None (the caller surfaces a
        // soft error rather than deleting the highlighted span).
        assert!(clean_and_splice_slice("a", "   ", "b").is_none());
        assert!(clean_and_splice_slice("a", "[TIME Day 1, 10:00]", "b").is_none());
    }

    #[test]
    fn clean_and_splice_slice_handles_empty_pre_and_post() {
        // Selection at the very start or end of the message.
        assert_eq!(
            clean_and_splice_slice("", "Opening line.", "").unwrap(),
            "Opening line."
        );
        assert_eq!(
            clean_and_splice_slice("Pre ", "end", "").unwrap(),
            "Pre end"
        );
        assert_eq!(
            clean_and_splice_slice("", "start", " post").unwrap(),
            "start post"
        );
    }

    #[test]
    fn apply_slice_splice_overwrites_content_clears_raw_and_keeps_variant_mirror() {
        // The variant-mirror honesty fix: a rerolled message (non-empty
        // variants) has its active variant slot overwritten too, else a future
        // normalize_variants would revert the splice to the stale sibling.
        let mut c = make_test_conversation();
        // Simulate a prior reroll on the assistant turn at index 1: seed two
        // variants + pick active index 1.
        c.messages[1].variants = vec!["old variant 0".into(), "old variant 1".into()];
        c.messages[1].raw_outputs = vec!["raw0".into(), "raw1".into()];
        c.messages[1].active_idx = 1;
        apply_slice_splice(&mut c, 1, "The goblin staggers but lives.".into()).unwrap();
        assert_eq!(c.messages[1].content, "The goblin staggers but lives.");
        assert_eq!(c.messages[1].raw_output, "");
        // The active variant mirror was overwritten in lockstep.
        assert_eq!(c.messages[1].variants[1], "The goblin staggers but lives.");
        assert_eq!(c.messages[1].raw_outputs[1], "");
        // The inactive sibling is preserved (still a swipeable alternate).
        assert_eq!(c.messages[1].variants[0], "old variant 0");
    }

    #[test]
    fn apply_slice_splice_on_never_rerolled_message_leaves_variants_empty() {
        // A fresh assistant message (empty variants) just takes the content +
        // raw_output overwrite; no mirror to update.
        let mut c = make_test_conversation();
        assert!(c.messages[1].variants.is_empty());
        apply_slice_splice(&mut c, 1, "Rewritten.".into()).unwrap();
        assert_eq!(c.messages[1].content, "Rewritten.");
        assert_eq!(c.messages[1].raw_output, "");
        assert!(c.messages[1].variants.is_empty(), "no variants should be seeded");
    }

    #[test]
    fn apply_slice_splice_out_of_bounds_errors() {
        let mut c = make_test_conversation();
        let len = c.messages.len();
        let err = apply_slice_splice(&mut c, len, "x".into()).unwrap_err();
        assert!(err.contains("out of bounds"), "got: {err}");
    }

    #[test]
    fn build_slice_regenerate_system_prompt_carries_voice_card_and_splice_rules() {
        let prompts = prompts::FablePrompts {
            narrator: "<narrator_directive>the voice</narrator_directive>".into(),
            agent: String::new(),
        };
        // SimCard has no Default; start from fallback() (all roleplay fields
        // empty) + override the identity fields the prompt builder reads.
        let mut card = sim_card::fallback();
        card.name = "The Rusty Tavern".into();
        card.setting = Some("A foggy port town.".into());
        card.plot = Some("A missing sailor.".into());
        card.tone = Some("Gritty low fantasy.".into());
        card.core_persona = "The narrator is terse and sensory.".into();
        let s = build_slice_regenerate_system_prompt(&prompts, &card);
        // (a) authored narrator voice.
        assert!(s.contains("the voice"), "missing narrator voice: {s}");
        // (b) card identity.
        assert!(s.contains("Scenario: The Rusty Tavern"), "missing scenario: {s}");
        assert!(s.contains("Setting: A foggy port town."), "missing setting: {s}");
        assert!(s.contains("Plot: A missing sailor."), "missing plot: {s}");
        assert!(s.contains("Tone: Gritty low fantasy."), "missing tone: {s}");
        assert!(s.contains("The narrator is terse"), "missing core_persona: {s}");
        // (c) splice discipline markers.
        assert!(s.contains("ONLY the rewritten passage"), "missing splice rule: {s}");
        assert!(s.contains("Splice cleanly"), "missing splice rule: {s}");
    }

    #[test]
    fn build_creator_assistant_system_prompt_carries_each_schema_and_envelope() {
        // Each creator kind names its exact schema keys + the ask/ready envelope
        // contract. Pinned so a "lean prompt" distillation can't silently drop a
        // field (the §7 bracket-verbs-omitted incident's lesson, applied here).
        let player = build_creator_assistant_system_prompt("player");
        // Core (mandatory) set pinned so a "lean prompt" distillation can't
        // silently drop one of the 12.
        assert!(player.contains("CORE FIELDS"));
        assert!(player.contains("name (string)"));
        assert!(player.contains("gender"));
        assert!(player.contains("skin_complexion"));
        assert!(player.contains("clothing (array"));
        // Contextual (context-clues) + custom routing pinned.
        assert!(player.contains("CONTEXTUAL FIELDS"));
        assert!(player.contains("horn"));
        assert!(player.contains("custom_tags"));
        // Optional + the player completion gate + the null/clobber rule.
        assert!(player.contains("backstory"));
        assert!(player.contains("Do not emit ready until every core field"));
        assert!(player.contains("never blank out a field already set"));
        assert!(player.contains("\"action\":\"ready\""));

        let sim = build_creator_assistant_system_prompt("sim");
        // Type Router + the 3 branches + universal anchors + the per-branch
        // completion gate. Pinned so a "lean prompt" pass can't drop one.
        assert!(sim.contains("TYPE ROUTER"));
        assert!(sim.contains("card_type"));
        assert!(sim.contains("npc"));
        assert!(sim.contains("scenario"));
        assert!(sim.contains("world"));
        assert!(sim.contains("dialogue_style"));
        assert!(sim.contains("trigger_condition"));
        assert!(sim.contains("primary_objective"));
        assert!(sim.contains("date"));
        assert!(sim.contains("custom_tags"));
        assert!(sim.contains("do not emit ready until draft.card_type"));
        // The mandatory INTRO question (2026-08-15, Chloe: the SIM Wizard —
        // not a post-card step — asks it; a card can't finalize without an
        // answer). Pinned so a lean-prompt pass can't silently demote it back
        // to an optional field.
        assert!(sim.contains("THE INTRO QUESTION"));
        assert!(sim.contains("whether they want an INTRO"));
        assert!(sim.contains("the INTRO question has been answered"));

        let codex = build_creator_assistant_system_prompt("codex");
        assert!(codex.contains("entries"));
        assert!(codex.contains("1400"));
        // Codex embed-window ceiling + the split-don't-truncate rule. Pinned so
        // a lean-prompt pass can't drop the 1400-char constraint or revert to
        // truncating long entries (each body must embed whole).
        assert!(codex.contains("under 1400 characters"));
        assert!(codex.contains("SPLIT"));
        assert!(codex.contains("Part 1"));
        assert!(codex.contains("Never truncate or drop lore"));

        // Input curation block (player used as the representative kind — the
        // block is shared across all kinds). Pinned so a "lean prompt" pass
        // can't silently drop the curate/condense discipline + the import-vs-
        // live-chat distinction.
        assert!(player.contains("INPUT CURATION"));
        assert!(player.contains("title-cased items"));
        assert!(player.contains("Notched Iron Broadsword"));
        assert!(player.contains("~150-300 words"));
        assert!(player.contains("high fidelity"));
        assert!(player.contains("<import>"));
    }

    #[test]
    fn find_oversize_codex_entries_detects_ready_over_cap() {
        let long_body = "x".repeat(1500);
        let reply = format!(
            "{{\"action\":\"ready\",\"draft\":{{\"entries\":[{{\"title\":\"Big\",\"body\":\"{}\"}}]}}}}",
            long_body
        );
        let offenders = find_oversize_codex_entries(&reply).expect("oversize ready detected");
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].0, "Big");
        assert_eq!(offenders[0].1, 1500);
    }

    #[test]
    fn find_oversize_codex_entries_passes_under_cap() {
        let reply = "{\"action\":\"ready\",\"draft\":{\"entries\":[{\"title\":\"Ok\",\"body\":\"short\"}]}}";
        assert!(find_oversize_codex_entries(reply).is_none());
    }

    /// An `ask` turn is never a finalize → never blocked, even if its draft
    /// carries an oversize entry mid-conversation.
    #[test]
    fn find_oversize_codex_entries_ignores_ask() {
        let long = "z".repeat(2000);
        let reply = format!(
            "{{\"action\":\"ask\",\"message\":\"...\",\"questions\":[],\"draft\":{{\"entries\":[{{\"title\":\"Big\",\"body\":\"{}\"}}]}}}}",
            long
        );
        assert!(find_oversize_codex_entries(&reply).is_none());
    }

    #[test]
    fn find_oversize_codex_entries_strips_markdown_fence() {
        let long_body = "y".repeat(2000);
        let reply = format!(
            "```json\n{{\"action\":\"ready\",\"draft\":{{\"entries\":[{{\"title\":\"Fenced\",\"body\":\"{}\"}}]}}}}\n```",
            long_body
        );
        let offenders = find_oversize_codex_entries(&reply).expect("fenced oversize ready detected");
        assert_eq!(offenders[0].0, "Fenced");
        assert_eq!(offenders[0].1, 2000);
    }

    #[test]
    fn find_oversize_codex_entries_unparseable_is_none() {
        // Never block on an unparseable reply — let the frontend handle it.
        assert!(find_oversize_codex_entries("not json at all").is_none());
        assert!(find_oversize_codex_entries("").is_none());
    }

    #[test]
    fn build_codex_oversize_alert_names_title_len_and_cap() {
        let alert = build_codex_oversize_alert(&[("Dwarven Kingdoms".to_string(), 2847)]);
        assert!(alert.contains("'Dwarven Kingdoms'"));
        assert!(alert.contains("2847 chars"));
        assert!(alert.contains("1400-character"));
        assert!(alert.contains("Dwarven Kingdoms - Part 1"));
    }

    #[test]
    fn project_messages_returns_lowercase_role_and_content_only() {
        let c = make_test_conversation();
        let projected = project_messages(&c);
        assert_eq!(projected.len(), 6);
        assert_eq!(projected[0].role, "user");
        assert_eq!(projected[0].content, "I attack the goblin.");
        assert_eq!(projected[1].role, "assistant");
        assert_eq!(projected[1].content, "The goblin falls.");
        // Spot-check: the wire shape is role+content(+variants/active_idx/
        // timestamp when set). `timestamp` IS a deliberate wire field (the
        // feed's per-beat clock); `raw_output` + `reasoning` are internal
        // and must never leak to the UI.
        let serialized = serde_json::to_string(&projected[1]).unwrap();
        assert!(
            !serialized.contains("raw_output"),
            "raw_output must NOT leak to UI: {serialized}"
        );
        assert!(
            !serialized.contains("reasoning"),
            "reasoning must NOT leak to UI: {serialized}"
        );
    }

    // ---- cap_assistant_prose (2026-08-10, T52 overflow fix) ---------------
    // The tracker window must not blow the 2922 budget when the API narrator's
    // beat is long. These pin the truncation logic without any model/GPU.

    /// Build a small message window for cap tests (avoids the full Message
    /// constructor's required fields — we only care about role + content).
    fn make_cap_window(user: &str, assistant: &str) -> Vec<session::Message> {
        let mut c = session::Conversation::new();
        c.add_message(session::Role::User, user.into());
        c.add_message(session::Role::Assistant, assistant.into());
        c.messages.clone()
    }

    #[test]
    fn cap_assistant_prose_truncates_long_assistant_message() {
        let long = "x".repeat(1000);
        let window = make_cap_window("go", &long);
        let capped = cap_assistant_prose(window, 100);
        // User message untouched.
        assert_eq!(capped[0].content, "go");
        // Assistant capped to ~100 chars + ellipsis marker.
        assert!(capped[1].content.len() < 120, "should be truncated well under original 1000");
        assert!(capped[1].content.ends_with(" […]\n"), "ellipsis marker appended");
    }

    #[test]
    fn cap_assistant_prose_leaves_short_messages_untouched() {
        let window = make_cap_window("I swing my sword.", "The orc parries.");
        let capped = cap_assistant_prose(window.clone(), 600);
        assert_eq!(capped[0].content, window[0].content);
        assert_eq!(capped[1].content, window[1].content, "short assistant message untouched");
    }

    #[test]
    fn cap_assistant_prose_is_utf8_safe_on_multibyte_chars() {
        // Smart quotes + em-dashes (multi-byte) — must not split mid-char.
        let prose = "“Hello — world.” ".repeat(80); // ~1120 chars, all multi-byte
        let window = make_cap_window("act", &prose);
        let capped = cap_assistant_prose(window, 100);
        // Must be valid UTF-8 (no panic = the char_indices truncation worked).
        assert!(capped[1].content.chars().count() <= 105, "truncated near the 100-char cap");
        assert!(capped[1].content.ends_with(" […]\n"));
    }

    #[test]
    fn cap_assistant_prose_does_not_cap_user_messages() {
        let long_user = "y".repeat(1000);
        let mut c = session::Conversation::new();
        c.add_message(session::Role::User, long_user.clone());
        let capped = cap_assistant_prose(c.messages.clone(), 100);
        assert_eq!(capped[0].content, long_user, "user messages are never capped");
    }

}

// ===========================================================================
// Phase 3 integration tests (2026-07-28).
//
// Purpose: the six Phase 3 slices (§11.30–§11.35) shipped as build + unit-
// test verified mechanics. Four of the six (Slices 2, 4, 5, 6) are NOT yet
// wired into the live fable_send / World Progression tick game loop — they
// ship the public seam functions + unit tests but no call sites. A live-app
// playtest cannot exercise unwired mechanics. These integration tests are
// the verification path: they drive every slice's public seam function the
// way Phase 4's wiring eventually will, in scenario form, cross-module.
//
// What this module is NOT: it does not duplicate the per-module unit tests.
// Those pin the mechanics' edge cases (nat 1/nat 20, cap enforcement, etc.)
// at high granularity. This module verifies the SEAMS COMPOSE — that the
// output of one slice is the correct shape for the next slice's input, and
// that the architect-defining scenarios (anti-Oblivion, betrayal asymmetry,
// no-apocalyptic-shift, lethality directive flow) hold end-to-end across
// modules. If a slice's public API drifts in a way that breaks composition,
// these tests fail FIRST (before any wiring work begins).
//
// Test count: +N (added to the §11.30 baseline of 672/2). Same 2
// pre-existing json_repair pair, zero regressions expected.
// ===========================================================================
#[cfg(test)]
mod phase3_integration_tests {
    use crate::consequence::{
        self, Condition, Polarity, StatusTag,
    };
    use crate::offscreen_task::{
        self, FocusMagnitude, FocusTarget, OffScreenTask, OutcomeSeverity,
        Suitability, TaskDifficulty,
    };
    use crate::player_state::{
        self, AttackerTier, BodyPart, BodyPartState, PlayerState, Roller,
    };
    use crate::relationship::{
        self, MilestoneRegistry, RelationshipState, RelationshipTier,
        RelationshipValidation, TransitionOutcome, TransitionReason,
    };

    use std::collections::{HashMap, HashSet};

    // ------------------------------------------------------------------------
    // Slice 1 (§11.30.A): NPC Tier Bands — anti-Oblivion clause.
    // Prompt-only — no Rust seam to call. The clause itself is pinned by the
    // existing narrator_prompt builder unit tests. Here we verify the
    // MECHANICAL companion (AttackerTier severity scaling) that the clause
    // promises: a Legendary-tier attacker's wound distribution is biased
    // toward Purple + lethality, vs a Minion's bias toward Yellow + non-
    // lethal. This is the Rust side of the same thesis the prompt clause
    // states narratively.
    // ------------------------------------------------------------------------

    /// Across many seeded rolls, a Legendary attacker produces a higher
    /// proportion of severe (Purple) wounds than a Minion. This is the
    /// mechanical enforcement of "a dragon is an apex predator regardless
    /// of your level." We don't assert a single roll (RNG-dependent); we
    /// assert the DISTRIBUTION tilts the way the tier ladder promises.
    #[test]
    fn slice1_legendary_tier_skews_severe_vs_minion() {
        let mut state = PlayerState::default();
        let text = "the dragon attacks me with its claws";

        let mut minion_purple = 0usize;
        let mut legendary_purple = 0usize;
        const ROLLS: usize = 40;

        // Run ROLLS iterations per tier. We vary the seed by perturbing the
        // text between runs so referee_evaluate_with_tier's internal seed
        // (hash of text + injury count) differs per iteration.
        for i in 0..ROLLS {
            let perturbed = format!("{text} #{i}");
            if let Some(outcome) = player_state::referee_evaluate_with_tier(
                &perturbed,
                &state,
                AttackerTier::Minion,
            ) {
                if outcome.new_state == BodyPartState::Purple {
                    minion_purple += 1;
                }
                // Reset state between rolls so injury_count doesn't drift
                // the seed away from the per-iteration perturbation.
                state = PlayerState::default();
            }
        }
        for i in 0..ROLLS {
            let perturbed = format!("{text} #{i}");
            if let Some(outcome) = player_state::referee_evaluate_with_tier(
                &perturbed,
                &state,
                AttackerTier::Legendary,
            ) {
                if outcome.new_state == BodyPartState::Purple {
                    legendary_purple += 1;
                }
                state = PlayerState::default();
            }
        }

        // The Legendary distribution must produce strictly more Purple wounds
        // than the Minion distribution. (Both could be 0 in a pathological
        // seed set, but the tier weights — Legendary [5,15,35,45] vs Minion
        // [80,18,2,0] — make that effectively impossible across 40 rolls.)
        assert!(
            legendary_purple > minion_purple,
            "Legendary tier ({} purple) must skew severe vs Minion ({} purple) \
             across {ROLLS} rolls each — the anti-Oblivion mechanical contract",
            legendary_purple,
            minion_purple,
        );
    }

    // ------------------------------------------------------------------------
    // Slice 3 (§11.32): AttackerTier + lethality directive flow.
    // Verify the directive a narrator would see when a lethal blow lands,
    // AND that the directive composes correctly into the <directives> block
    // shape fable_send consumes (per the lib.rs:5306 wiring).
    // ------------------------------------------------------------------------

    /// When referee_evaluate_with_tier produces a lethal outcome, the
    /// directive string is non-empty, names the player as DOWNED, and is
    /// shaped so fable_send can wrap it as `[DIRECTIVE: ...]`. We sweep
    /// seeds until we find a lethal roll (high tier + failed save) and
    /// verify the directive's contract.
    #[test]
    fn slice3_lethality_directive_is_narrator_consumable() {
        // High tier + many seeds → we expect at least one lethal outcome
        // (Legendary lethality DC is BASE_LETHAL_DC + tier_modifier, and the
        // weights are stacked toward severe wounds which raise condition
        // penalty on the save).
        let mut found_lethal = false;
        for i in 0..200 {
            let mut state = PlayerState::default();
            // NOTE: the text MUST contain a COMBAT_KEYWORDS token (attack/
            // strike/slash/etc) or referee_evaluate_with_tier returns None
            // without rolling. "bites" alone does NOT trigger — the keyword
            // list is the player's combat-action vocabulary. Use "strikes" so
            // the referee fires on every iteration.
            let text = format!("the ancient wyrm strikes me down #{i}");
            let Some(outcome) = player_state::referee_evaluate_with_tier(
                &text,
                &state,
                AttackerTier::Legendary,
            ) else {
                continue;
            };

            if outcome.lethal {
                found_lethal = true;
                // Contract 1: directive is non-empty on a lethal outcome.
                assert!(
                    !outcome.directive.is_empty(),
                    "lethal outcome must carry a non-empty directive for the narrator"
                );
                // Contract 2: the directive references the player being
                // DOWNED (the canonical lethality vocabulary).
                let lower = outcome.directive.to_lowercase();
                assert!(
                    lower.contains("downed") || lower.contains("down"),
                    "lethal directive must reference the player being DOWNED; \
                     got: {:?}",
                    outcome.directive,
                );
                // Contract 3: the directive is free-form prose the narrator
                // can splice into [DIRECTIVE: ...] verbatim — no leading
                // brackets, no trailing newline. fable_send wraps it.
                assert!(
                    !outcome.directive.starts_with('['),
                    "directive must be the inner text, not pre-wrapped; \
                     fable_send adds [DIRECTIVE: ...]"
                );
                assert!(
                    !outcome.directive.ends_with('\n'),
                    "directive must not carry a trailing newline; fable_send \
                     appends one as part of the <directives> block"
                );
                // The state is unaffected by reading the outcome here; we
                // don't apply it because we're verifying the contract, not
                // the state transition. Keep clippy happy about unused mut.
                let _ = &mut state;
                break;
            }
        }
        assert!(
            found_lethal,
            "Expected at least one lethal outcome across 200 Legendary-tier \
             rolls — if none fired, the lethality threshold may be too high \
             OR the seed sweep needs widening. Investigate before adjusting \
             the threshold; the architect pinned lethality as a real risk."
        );
    }

    /// The <directives> block a narrator sees composes lethality FIRST
    /// (most consequential) then skill-check directives. This mirrors the
    /// lib.rs:5324 wiring. We construct the block shape directly to verify
    /// the contract the wiring relies on.
    #[test]
    fn slice3_directives_block_orders_lethality_first() {
        // Simulate the wiring: lethality directive (if any) precedes skill
        // directives in the <directives> block.
        let combat_directive: Option<String> =
            Some("the player is DOWNED — a lethal blow has landed".to_string());
        let skill_directives: Vec<String> = vec![
            "Lockpick (DC 12): FAIL. The pick snaps.".to_string(),
            "Persuade (DC 15): SUCCESS. The guard hesitates.".to_string(),
        ];

        let mut rendered = String::from("<directives>\n");
        if let Some(cd) = &combat_directive {
            rendered.push_str(&format!("[DIRECTIVE: {cd}]\n"));
        }
        for sc in &skill_directives {
            rendered.push_str(&format!("[DIRECTIVE: {sc}]\n"));
        }
        rendered.push_str("</directives>");

        // The lethality directive must come before the skill directives.
        let lethal_pos = rendered.find("DOWNED").expect("lethality present");
        let lockpick_pos = rendered.find("Lockpick").expect("skill present");
        assert!(
            lethal_pos < lockpick_pos,
            "lethality directive must precede skill directives in the \
             <directives> block — it's the most consequential fact for the turn"
        );
        // The block must open + close with the right tags (fable_send's
        // parser depends on this shape).
        assert!(rendered.starts_with("<directives>\n"));
        assert!(rendered.ends_with("</directives>"));
    }

    // ---- Phase 4 §11.44 (Component 1): disguise gate wiring ----

    #[test]
    fn component1_disguise_directive_orders_between_lethality_and_skills() {
        // The §11.42 <directives> block assembly order is:
        //   lethality → disguise → skill checks → tick directives.
        // Disguise sits after lethality (the most consequential fact) but
        // before skill checks — it's scene-establishing ("your disguise
        // holds" gates whether the skill checks even fire in spirit).
        let combat_directive: Option<String> =
            Some("Lethal blow (soldier tier, DC 18): the player is DOWNED".to_string());
        let disguise_directive: Option<String> =
            Some(player_state::DisguiseDirective::AutoPass {
                label: "city guard uniform".into(),
                tier_tag: "soldier",
            }.render());
        let skill_directives: Vec<String> = vec![
            "Lockpick (DC 12): FAIL. The pick snaps.".to_string(),
        ];

        // Mirror the fable_send assembly order exactly.
        let mut turn_directives: Vec<String> = Vec::new();
        if let Some(cd) = &combat_directive { turn_directives.push(cd.clone()); }
        if let Some(dd) = &disguise_directive { turn_directives.push(dd.clone()); }
        for sc in &skill_directives { turn_directives.push(sc.clone()); }

        let lethal_pos = turn_directives.iter().position(|d| d.contains("DOWNED"))
            .expect("lethality present");
        let disguise_pos = turn_directives.iter().position(|d| d.contains("ACCEPTED"))
            .expect("disguise present");
        let skill_pos = turn_directives.iter().position(|d| d.contains("Lockpick"))
            .expect("skill present");
        assert!(
            lethal_pos < disguise_pos && disguise_pos < skill_pos,
            "order must be lethality → disguise → skill; got {:?}",
            turn_directives
        );
    }

    #[test]
    fn component1_gate_autopass_renders_into_directives_block() {
        // End-to-end: the gate produces a DisguiseDirective whose render()
        // output is a clean one-liner that slots into the <directives> block.
        use std::collections::HashMap;
        let mut entities = HashMap::new();
        entities.insert(
            "npc.gate_guard.tier".to_string(),
            serde_json::Value::String("soldier".into()),
        );
        let tags = vec![consequence::StatusTag {
            label: "city guard uniform".into(),
            polarity: consequence::Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: "disguise".into(),
        }];
        let dd = player_state::evaluate_disguise_gate(
            "I nod to the guard and walk past confidently.",
            &tags,
            &entities,
            0,
        ).expect("soldier + confident → AutoPass");
        let rendered = dd.render();
        assert!(rendered.contains("ACCEPTED"));
        assert!(rendered.contains("city guard uniform"));
        // It must read as a single directive line (no newlines — the block
        // wraps each entry in [DIRECTIVE: ...]).
        assert!(!rendered.contains('\n'), "directive must be one line: {rendered}");
    }

    #[test]
    fn component1_gate_returns_none_when_no_disguise_tag_so_no_directive() {
        // The wiring must NOT inject a disguise directive when the player
        // has no disguise tag — the gate returning None means the assembly
        // skips it entirely.
        use std::collections::HashMap;
        let entities = HashMap::new();
        let tags = vec![consequence::StatusTag {
            label: "Blessed".into(),
            polarity: consequence::Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(), // generic buff, NOT a disguise
        }];
        let dd = player_state::evaluate_disguise_gate(
            "I walk past the guard.",
            &tags,
            &entities,
            0,
        );
        assert!(dd.is_none(), "no disguise tag → no directive");
    }

    // ------------------------------------------------------------------------
    // Slice 2 (§11.31): consequence.rs Driver taxonomy + read-time derived
    // Condition. Verify the read-time derivation contract: the same wound +
    // buff/debuff counts always yield the same Condition, AND the Condition
    // re-derives correctly after a buff expires (the Slice 4 expiry
    // interaction). This is the seam the World Progression tick + the
    // narrator prompt renderer will call.
    // ------------------------------------------------------------------------

    /// derive_condition is a pure fn of (wounds, buffs_count, debuffs_count).
    /// The same inputs must yield the same Condition every call — this is
    /// the "never stored, recomputed every render" contract.
    #[test]
    fn slice2_condition_is_pure_function_of_inputs() {
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::UpperTorso, BodyPartState::Orange);
        let buffs = 0;
        let debuffs = 0;

        let c1 = consequence::derive_condition(&wounds, buffs, debuffs);
        let c2 = consequence::derive_condition(&wounds, buffs, debuffs);
        let c3 = consequence::derive_condition(&wounds, buffs, debuffs);

        assert_eq!(
            c1, c2,
            "derive_condition must be deterministic — same inputs, same output"
        );
        assert_eq!(
            c2, c3,
            "derive_condition must be stable across repeated calls (no hidden state)"
        );
        // A single Orange (Medium) wound → Wounded (per the documented mapping).
        assert_eq!(
            c1,
            Condition::Wounded,
            "single Orange wound must derive to Wounded (per the rank mapping)"
        );
    }

    /// The Condition escalates as wounds worsen, following the documented
    /// rank ladder: Unscathed → Haggard → Wounded → Battered → Critical →
    /// Downed. This is the spine the narrator renders as a qualitative
    /// label (the descriptive layer never sees the wound map directly).
    #[test]
    fn slice2_condition_escalates_with_wound_severity() {
        // Empty wounds → Unscathed.
        let empty: HashMap<BodyPart, BodyPartState> = HashMap::new();
        assert_eq!(
            consequence::derive_condition(&empty, 0, 0),
            Condition::Unscathed,
            "no wounds → Unscathed"
        );

        // Single Yellow → Haggard.
        let mut yellow = HashMap::new();
        yellow.insert(BodyPart::LeftHand, BodyPartState::Yellow);
        assert_eq!(
            consequence::derive_condition(&yellow, 0, 0),
            Condition::Haggard,
            "Yellow-only wound → Haggard"
        );

        // Single Red → Battered.
        let mut red = HashMap::new();
        red.insert(BodyPart::UpperTorso, BodyPartState::Red);
        assert_eq!(
            consequence::derive_condition(&red, 0, 0),
            Condition::Battered,
            "Red wound → Battered"
        );

        // Multiple severe wounds (3+) → Downed (body can't sustain them).
        let mut multi = HashMap::new();
        multi.insert(BodyPart::Head, BodyPartState::Red);
        multi.insert(BodyPart::UpperTorso, BodyPartState::Red);
        multi.insert(BodyPart::LeftUpperLeg, BodyPartState::Red);
        assert_eq!(
            consequence::derive_condition(&multi, 0, 0),
            Condition::Downed,
            "3+ severe wounds → Downed (the multi-severe escalation rule)"
        );
    }

    /// Buffs can lift a Haggard body to Unscathed (one tier), but wounds
    /// dominate above Haggard — no buff rescues Wounded/Battered/Critical.
    /// This pins the "wounds dominate" anti-trivialization rule.
    #[test]
    fn slice2_buffs_lift_only_at_marginal_conditions() {
        let mut yellow = HashMap::new();
        yellow.insert(BodyPart::LeftHand, BodyPartState::Yellow);

        // Yellow + 1 buff + 0 debuffs → lifted to Unscathed.
        assert_eq!(
            consequence::derive_condition(&yellow, 1, 0),
            Condition::Unscathed,
            "a single buff lifts a Haggard (Yellow-only) body to Unscathed"
        );

        // Red + buff → still Battered (wounds dominate).
        let mut red = HashMap::new();
        red.insert(BodyPart::UpperTorso, BodyPartState::Red);
        assert_eq!(
            consequence::derive_condition(&red, 1, 0),
            Condition::Battered,
            "buffs cannot lift above Haggard when wounds are Medium-or-worse \
             — wounds dominate"
        );
    }

    // ------------------------------------------------------------------------
    // Slice 4 (§11.33): StatusTag expiry + the Condition re-derives after
    // expiry. This is the seam interaction the World Progression tick will
    // drive: drop expired tags, then derive_condition sees the new counts.
    // ------------------------------------------------------------------------

    /// Tags with `expires_at == 0` are permanent (the sentinel). expire_tags
    /// must NOT drop them — they end only via an explicit event. This is
    /// the contract that lets "Cursed by the Witch King" persist until
    /// narratively lifted.
    #[test]
    fn slice4_permanent_tags_survive_expiry_check() {
        let mut tags = vec![
            StatusTag {
                label: "Cursed by the Witch King".to_string(),
                polarity: Polarity::Debuff,
                expires_at: 0, // permanent
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Berserk Rage".to_string(),
                polarity: Polarity::Buff,
                expires_at: 1000, // expires at minute 1000
                source: String::new(),
            kind: String::new(),
            },
        ];

        // Tick to minute 5000 — well past 1000 but the permanent tag has
        // expires_at == 0 so it must survive.
        let dropped = consequence::expire_tags(&mut tags, 5000);
        assert_eq!(dropped, 1, "only the timed tag (expires_at=1000) drops");
        assert_eq!(tags.len(), 1, "one tag remains");
        assert_eq!(
            tags[0].label, "Cursed by the Witch King",
            "the permanent tag (expires_at=0) survives the tick"
        );
    }

    /// The Condition re-derives correctly after a buff tag expires. Before
    /// expiry: Yellow wound + 1 buff → Unscathed (buff lifts it). After the
    /// buff expires on the tick: Yellow wound + 0 buffs → Haggard. This is
    /// the "tag's effect fades automatically when it expires" contract —
    /// no restoration math, just re-derive.
    #[test]
    fn slice4_condition_rederives_after_buff_expiry() {
        let mut wounds = HashMap::new();
        wounds.insert(BodyPart::LeftHand, BodyPartState::Yellow);

        let mut tags = vec![StatusTag {
            label: "Blessed by the Sun Priest".to_string(),
            polarity: Polarity::Buff,
            expires_at: 2000,
            source: String::new(),
        kind: String::new(),
        }];

        // Before the tick (minute 1000): buff active → Unscathed.
        let _ = consequence::expire_tags(&mut tags, 1000);
        let buffs_active = consequence::count_by_polarity(&tags, Polarity::Buff);
        assert_eq!(buffs_active, 1, "buff still active before minute 2000");
        assert_eq!(
            consequence::derive_condition(&wounds, buffs_active, 0),
            Condition::Unscathed,
            "with buff active, Yellow wound is lifted to Unscathed"
        );

        // Tick past expiry (minute 3000): buff drops → Haggard.
        let dropped = consequence::expire_tags(&mut tags, 3000);
        assert_eq!(dropped, 1, "buff expired");
        let buffs_after = consequence::count_by_polarity(&tags, Polarity::Buff);
        assert_eq!(buffs_after, 0, "no buffs remain after expiry");
        assert_eq!(
            consequence::derive_condition(&wounds, buffs_after, 0),
            Condition::Haggard,
            "after buff expires, Yellow wound re-derives to Haggard — \
             the tag's effect fades automatically"
        );
    }

    /// compute_frustration is monotonic non-decreasing in elapsed time within
    /// the window, and lands in a valid MoodTier. This is the seam the World
    /// Progression tick will call to derive NPC mood directives.
    #[test]
    fn slice4_frustration_curve_is_monotonic_and_bounded() {
        // A moderate-volatility NPC whose quest deadline was 1000 minutes
        // ago, with a 2000-minute window and volatility 1.0.
        let window = 2000_i64;
        let volatility = 1.0;

        let f_early = consequence::compute_frustration(500, window, volatility)
            .expect("frustration computes for valid window+elapsed");
        let f_mid = consequence::compute_frustration(1000, window, volatility)
            .expect("frustration computes at the deadline");
        let f_late = consequence::compute_frustration(1500, window, volatility)
            .expect("frustration computes past the deadline");

        // The underlying mood_score is monotonic non-decreasing in elapsed
        // time (higher score = more frustrated). MoodTier itself doesn't
        // derive PartialOrd, so we verify monotonicity on the score and
        // then check the categorical tier landed in a valid ladder slot.
        assert!(
            f_early.mood_score <= f_mid.mood_score
                && f_mid.mood_score <= f_late.mood_score,
            "mood_score must be monotonic non-decreasing in elapsed time \
             (early={} ≤ mid={} ≤ late={})",
            f_early.mood_score,
            f_mid.mood_score,
            f_late.mood_score
        );

        // Each FrustrationState yields a categorical MoodTier the narrator
        // renders as a directive. Verify the seam produces a tier (not a
        // panic) for each sampled point — the tier() fn is what Phase 4's
        // tick will call to get the directive text.
        let _ = f_early.tier();
        let _ = f_mid.tier();
        let _ = f_late.tier();
    }

    // ------------------------------------------------------------------------
    // Slice 5 (§11.34): Relationship State Machine. The three architect-
    // defining scenarios, exercised through the seam fns Phase 4 will wire:
    //   (a) Shiny-sword-on-Day-1 gift REJECTED by validate_llm_tier_write
    //       (silent-drop, no repair queue).
    //   (b) Betrayal drops to Hostile instantly via evaluate_transition —
    //       NO time floor (gravity of betrayal is asymmetric).
    //   (c) Murder drops to Nemesis regardless of prior bond.
    // ------------------------------------------------------------------------

    /// An LLM attempt to write "trusted" on a brand-new Stranger NPC is
    /// REJECTED — gated escalations don't clear without the Referee path.
    /// This is the anti-sycophancy firewall: the model can't talk its way
    /// past the gates by writing a flattering tier.
    #[test]
    fn slice5_llm_gated_escalation_rejected_silent_drop() {
        let state = RelationshipState::default(); // Stranger, tier_entered_at=0
        let validation = relationship::validate_llm_tier_write("trusted", &state);
        match validation {
            RelationshipValidation::Reject { actual_tier } => {
                assert_eq!(
                    actual_tier,
                    RelationshipTier::Stranger,
                    "rejection must carry the actual current tier"
                );
            }
            other => panic!(
                "LLM attempt to write 'trusted' on a Stranger must be Reject, \
                 not {:?} — gated escalations are silent-dropped",
                other
            ),
        }
    }

    /// The ONE allowed LLM-initiated transition is Stranger → Acquaintance
    /// (the auto-advance on first positive interaction, no gates). Every
    /// other transition must route through the Referee path.
    #[test]
    fn slice5_stranger_to_acquaintance_is_the_one_llm_allowed_path() {
        let state = RelationshipState::default();
        let v = relationship::validate_llm_tier_write("acquaintance", &state);
        assert!(
            matches!(v, RelationshipValidation::Accept),
            "Stranger → Acquaintance must be the one LLM-allowed transition; got {:?}",
            v
        );

        // From any non-Stranger tier, even Acquaintance → Friendly must be
        // rejected (it's gated — needs time floor + milestones).
        let mut acquaint = RelationshipState::default();
        acquaint.tier = RelationshipTier::Acquaintance;
        let v2 = relationship::validate_llm_tier_write("friendly", &acquaint);
        assert!(
            matches!(v2, RelationshipValidation::Reject { .. }),
            "Acquaintance → Friendly must be rejected from the LLM path — it's gated"
        );
    }

    /// Betrayal drops to Hostile with NO time floor — the asymmetric gravity
    /// of betrayal. A brand-new Stranger betrayed on Day 1 drops instantly.
    #[test]
    fn slice5_betrayal_drops_instantly_no_time_floor() {
        let mut state = RelationshipState::default(); // tier_entered_at = 0
        let registry = MilestoneRegistry::defaults();

        // Record the betrayal event. The state must now carry it.
        assert!(
            state.record_event("betrayed_trust"),
            "record_event must accept a known event id"
        );

        // Evaluate at now_minutes = 0 (literally the moment the relationship
        // started). The betrayal short-circuit must fire anyway.
        let outcome = relationship::evaluate_transition(&state, &registry, 0);
        match outcome {
            TransitionOutcome::Transition { new_tier, reason } => {
                assert_eq!(
                    new_tier,
                    RelationshipTier::Hostile,
                    "betrayed_trust drops to Hostile"
                );
                assert_eq!(
                    reason,
                    TransitionReason::HostilityTriggered,
                    "the transition reason must be the hostility short-circuit"
                );
            }
            other => panic!(
                "betrayal must fire the hostility short-circuit even at now=0; got {:?}",
                other
            ),
        }
    }

    /// Murder drops to Nemesis regardless of prior bond. Even a Bonded NPC
    /// who has sworn an oath, if you kill their family, drops to Nemesis.
    /// This pins the "gravity of betrayal is asymmetric" rule at its
    /// extreme.
    #[test]
    fn slice5_murder_drops_to_nemesis_regardless_of_bond() {
        let mut state = RelationshipState::default();
        // Establish a maximal bond: sworn_oath + long_loyalty + saved_life.
        state.record_event("sworn_oath");
        state.record_event("long_loyalty");
        state.record_event("saved_life");
        state.tier = RelationshipTier::Bonded;
        state.tier_entered_at_minutes = -100_000; // ancient bond

        let registry = MilestoneRegistry::defaults();

        // Now commit the atrocity.
        state.record_event("killed_family");

        let outcome = relationship::evaluate_transition(&state, &registry, 0);
        match outcome {
            TransitionOutcome::Transition { new_tier, .. } => {
                assert_eq!(
                    new_tier,
                    RelationshipTier::Nemesis,
                    "killed_family drops a Bonded NPC to Nemesis — no bond survives this"
                );
            }
            other => panic!(
                "murder must drop to Nemesis regardless of bond; got {:?}",
                other
            ),
        }
    }

    /// A legitimate affinity advance requires BOTH gates (time floor AND
    /// milestone threshold). Recording milestones without enough time, OR
    /// enough time without milestones, must NOT advance. Only both clearing
    /// advances the tier.
    #[test]
    fn slice5_affinity_advance_requires_both_gates() {
        let registry = MilestoneRegistry::defaults();

        // Case A: milestones but no time. Record enough points to clear the
        // milestone gate for Stranger → Acquaintance.
        let mut state_a = RelationshipState::default();
        state_a.record_event("first_positive_interaction");
        state_a.record_event("shared_drink");
        // Evaluate at now = 0 (no time elapsed). (out_a is intentionally
        // unused — the real test is out_a2 below, after we set tier past
        // Acquaintance so the gate logic actually applies.)
        let _out_a = relationship::evaluate_transition(&state_a, &registry, 0);
        // Stranger → Acquaintance is the no-gate auto-advance, so it WOULD
        // fire here. To test the gate properly we need to be PAST
        // Acquaintance. Set tier to Acquaintance + try for Friendly.
        state_a.tier = RelationshipTier::Acquaintance;
        state_a.tier_entered_at_minutes = 0;
        let out_a2 = relationship::evaluate_transition(&state_a, &registry, 0);
        match out_a2 {
            TransitionOutcome::NoTransition { reason } => {
                // Must be a gate failure (time floor or milestone), NOT a
                // transition. The exact reason depends on the threshold math.
                assert!(
                    matches!(
                        reason,
                        TransitionReason::TimeFloorNotMet
                            | TransitionReason::MilestoneThresholdNotMet
                    ),
                    "milestones-without-time must hit a gate, not {:?}",
                    reason
                );
            }
            TransitionOutcome::Transition { .. } => {
                panic!("Acquaintance → Friendly must NOT fire without both gates clearing");
            }
        }

        // Case B: time but no milestones. Ancient Acquaintance with zero
        // recorded events must NOT advance on time alone.
        let mut state_b = RelationshipState::default();
        state_b.tier = RelationshipTier::Acquaintance;
        state_b.tier_entered_at_minutes = -1_000_000; // very old
        let out_b = relationship::evaluate_transition(&state_b, &registry, 1_000_000);
        match out_b {
            TransitionOutcome::NoTransition { reason } => {
                assert_eq!(
                    reason,
                    TransitionReason::MilestoneThresholdNotMet,
                    "time-without-milestones must fail on the milestone gate"
                );
            }
            TransitionOutcome::Transition { .. } => {
                panic!("time alone must NOT clear the milestone gate");
            }
        }
    }

    // ------------------------------------------------------------------------
    // Slice 6 (§11.35): Off-Screen Risk Referee + Focus Randomization.
    // Verify the hard no-apocalyptic-shift constraint end-to-end: across
    // many seeded focus selections, no tick ever exceeds the per-tick caps
    // (Minor=8/Moderate=3/Major=1), and no Apocalyptic tier is selectable
    // (enforced by enum shape). Plus the off-screen task resolution
    // directive is narrator-consumable.
    // ------------------------------------------------------------------------

    /// select_focus NEVER exceeds the per-tick caps, across many seeds and
    /// candidate pools. The caps are the hard no-apocalyptic-shift
    /// constraint — a single tick can do at most 1 Major shift, never more.
    #[test]
    fn slice6_focus_caps_never_exceeded_across_seeds() {
        // Build a large candidate pool that exceeds every cap.
        let mut candidates = Vec::new();
        for i in 0..30 {
            candidates.push(FocusTarget {
                entity_key: format!("minor_{i}"),
                magnitude: FocusMagnitude::Minor,
                seed: i.to_string(),
            });
        }
        for i in 0..15 {
            candidates.push(FocusTarget {
                entity_key: format!("moderate_{i}"),
                magnitude: FocusMagnitude::Moderate,
                seed: i.to_string(),
            });
        }
        for i in 0..10 {
            candidates.push(FocusTarget {
                entity_key: format!("major_{i}"),
                magnitude: FocusMagnitude::Major,
                seed: i.to_string(),
            });
        }
        let excluded: HashSet<String> = HashSet::new();

        // Sweep many seeds. Every result must respect the caps.
        for seed in 0..100 {
            let mut roller = Roller::new(seed);
            let selected = offscreen_task::select_focus(&candidates, &excluded, &mut roller);

            let minor_count = selected
                .iter()
                .filter(|t| t.magnitude == FocusMagnitude::Minor)
                .count();
            let moderate_count = selected
                .iter()
                .filter(|t| t.magnitude == FocusMagnitude::Moderate)
                .count();
            let major_count = selected
                .iter()
                .filter(|t| t.magnitude == FocusMagnitude::Major)
                .count();

            assert!(
                minor_count <= FocusMagnitude::Minor.per_tick_cap(),
                "seed {seed}: Minor cap violated ({} > {})",
                minor_count,
                FocusMagnitude::Minor.per_tick_cap()
            );
            assert!(
                moderate_count <= FocusMagnitude::Moderate.per_tick_cap(),
                "seed {seed}: Moderate cap violated ({} > {})",
                moderate_count,
                FocusMagnitude::Moderate.per_tick_cap()
            );
            assert!(
                major_count <= FocusMagnitude::Major.per_tick_cap(),
                "seed {seed}: Major cap violated ({} > {}) — the hard \
                 no-apocalyptic-shift constraint",
                major_count,
                FocusMagnitude::Major.per_tick_cap()
            );
        }
    }

    /// The player's bubble entities are excluded from focus selection — the
    /// world is alive, but not at the player's immediate location. This is
    /// the "no off-screen catastrophe lands on the player's head" rule.
    #[test]
    fn slice6_focus_excludes_player_bubble() {
        let candidates = vec![
            FocusTarget {
                entity_key: "player_companion_marcus".to_string(),
                magnitude: FocusMagnitude::Major,
                seed: "1".to_string(),
            },
            FocusTarget {
                entity_key: "tavern_back_room".to_string(),
                magnitude: FocusMagnitude::Moderate,
                seed: "2".to_string(),
            },
            FocusTarget {
                entity_key: "distant_village".to_string(),
                magnitude: FocusMagnitude::Minor,
                seed: "3".to_string(),
            },
        ];
        let mut excluded = HashSet::new();
        excluded.insert("player_companion_marcus".to_string());
        excluded.insert("tavern_back_room".to_string());

        let mut roller = Roller::new(42);
        let selected = offscreen_task::select_focus(&candidates, &excluded, &mut roller);

        // Marcus + the tavern (the player's bubble) must never be selected.
        for t in &selected {
            assert!(
                !excluded.contains(&t.entity_key),
                "player-bubble entity {:?} must never be a focus target",
                t.entity_key
            );
        }
        // The distant village is fair game.
        assert!(
            selected.iter().any(|t| t.entity_key == "distant_village"),
            "a non-excluded candidate should be selectable"
        );
    }

    /// The off-screen task directive is narrator-consumable: free-form prose
    /// that names the NPC, states the outcome qualitatively, and enforces
    /// the no-apocalyptic constraint verbatim. This is the shape the World
    /// Progression tick will wrap as `[DIRECTIVE: ...]`.
    #[test]
    fn slice6_task_directive_is_narrator_consumable() {
        let task = OffScreenTask {
            npc_id: "marcus".to_string(),
            description: "scout the bandit camp".to_string(),
            difficulty: TaskDifficulty::Challenging,
            suitability: Suitability::Adequate,
            resolves_at_minutes: 1000,
            resolved: false,
        };
        let resolution = offscreen_task::resolve_task(&task);

        // Contract 1: the directive is non-empty prose.
        assert!(
            !resolution.directive.is_empty(),
            "task resolution must carry a directive"
        );
        // Contract 2: it names the NPC (the narrator needs to know who returned).
        assert!(
            resolution.directive.contains("marcus"),
            "directive must name the NPC; got: {:?}",
            resolution.directive
        );
        // Contract 3: it carries the qualitative outcome tag (snake_case
        // converted to spaces — e.g. "complicated success").
        let lower = resolution.directive.to_lowercase();
        assert!(
            lower.contains("success") || lower.contains("failure"),
            "directive must state the qualitative outcome (success/failure); got: {:?}",
            resolution.directive
        );
        // Contract 4: the no-apocalyptic-shift constraint is stated verbatim.
        assert!(
            lower.contains("do not invent global") || lower.contains("world-shaking"),
            "directive must enforce the no-apocalyptic-shift constraint verbatim; got: {:?}",
            resolution.directive
        );
        // Contract 5: the d20 roll + DC are NOT in the directive (engine-room
        // only — never shown to the narrator).
        assert!(
            !resolution.directive.contains(&format!("roll: {}", resolution.roll)),
            "the d20 roll must NOT appear in the directive — engine-room only"
        );
        assert!(
            !resolution.directive.contains(&format!("dc {}", resolution.dc))
                && !resolution.directive.contains(&format!("DC {}", resolution.dc)),
            "the DC must NOT appear in the directive — engine-room only"
        );
    }

    /// resolve_expired_tasks skips not-yet-due tasks and already-resolved
    /// tasks (no re-roll). This is the contract the World Progression tick
    /// relies on: due tasks resolve once, the queue drains correctly.
    #[test]
    fn slice6_resolve_expired_tasks_skips_wrong_state() {
        let tasks = vec![
            // Due + unresolved → resolves.
            OffScreenTask {
                npc_id: "due_unresolved".to_string(),
                description: "task A".to_string(),
                difficulty: TaskDifficulty::Routine,
                suitability: Suitability::Ideal,
                resolves_at_minutes: 1000,
                resolved: false,
            },
            // Not yet due → skipped.
            OffScreenTask {
                npc_id: "future".to_string(),
                description: "task B".to_string(),
                difficulty: TaskDifficulty::Routine,
                suitability: Suitability::Ideal,
                resolves_at_minutes: 5000,
                resolved: false,
            },
            // Already resolved → skipped (no re-roll).
            OffScreenTask {
                npc_id: "already_done".to_string(),
                description: "task C".to_string(),
                difficulty: TaskDifficulty::Routine,
                suitability: Suitability::Ideal,
                resolves_at_minutes: 500,
                resolved: true,
            },
        ];

        let resolutions = offscreen_task::resolve_expired_tasks(&tasks, 2000);
        // Only the due + unresolved task should produce a resolution.
        assert_eq!(
            resolutions.len(),
            1,
            "only due+unresolved tasks resolve; got {:?}",
            resolutions.iter().map(|r| &r.npc_id).collect::<Vec<_>>()
        );
        assert_eq!(resolutions[0].npc_id, "due_unresolved");
    }

    /// Cross-slice composition: a CatastrophicFailure on an off-screen task
    /// is BOUNDED — OutcomeSeverity's worst tier is CatastrophicFailure,
    /// never anything beyond. The enum shape enforces the no-apocalyptic
    /// constraint at the type level. Verify the ladder's worst element.
    #[test]
    fn slice6_outcome_severity_worst_tier_is_bounded() {
        // The worst possible OutcomeSeverity is CatastrophicFailure. There
        // is no Apocalyptic variant — the enum shape enforces it.
        let worst = OutcomeSeverity::CatastrophicFailure;
        // Every variant must be ≤ worst (worst is the floor of the ladder).
        // We verify by checking the documented variants are all <= worst
        // (PartialOrd + Ord derived, worst→best by variant order).
        let all = [
            OutcomeSeverity::CatastrophicFailure,
            OutcomeSeverity::Failure,
            OutcomeSeverity::ComplicatedSuccess,
            OutcomeSeverity::Success,
            OutcomeSeverity::CriticalSuccess,
        ];
        for v in &all {
            assert!(
                worst <= *v,
                "CatastrophicFailure must be the worst (lowest) tier; {:?} is worse",
                v
            );
        }
    }

    // ------------------------------------------------------------------------
    // Cross-slice scenario: the full lethality → death → reputation flow.
    // This stitches Slice 3 (lethality) + Slice 5 (relationship) into the
    // kind of multi-module interaction Phase 4 will wire end-to-end.
    // ------------------------------------------------------------------------

    /// When a lethal blow lands AND the player has a `killed_ally` event
    /// recorded against an NPC, BOTH directives compose: the lethality
    /// directive from Slice 3 (the player is DOWNED) AND the relationship
    /// drop to Nemesis from Slice 5 (the witness becomes a sworn enemy).
    /// This verifies the two slices' outputs don't collide when the
    /// narrator renders them together.
    #[test]
    fn cross_slice_lethality_plus_witness_relationship_drop() {
        // Slice 5: an NPC witnesses the player kill their ally.
        let mut witness = RelationshipState::default();
        witness.record_event("killed_ally");
        let registry = MilestoneRegistry::defaults();
        let rel_outcome = relationship::evaluate_transition(&witness, &registry, 0);
        let witness_new_tier = match rel_outcome {
            TransitionOutcome::Transition { new_tier, .. } => new_tier,
            other => panic!("witness must drop to Nemesis; got {:?}", other),
        };
        assert_eq!(
            witness_new_tier,
            RelationshipTier::Nemesis,
            "witness to killed_ally drops to Nemesis"
        );

        // Slice 3: find a lethal outcome (sweep seeds against a Legendary foe).
        let mut lethal_directive: Option<String> = None;
        for i in 0..200 {
            let text = format!("the ancient wyrm strikes me down #{i}");
            if let Some(outcome) = player_state::referee_evaluate_with_tier(
                &text,
                &PlayerState::default(),
                AttackerTier::Legendary,
            ) {
                if outcome.lethal {
                    lethal_directive = Some(outcome.directive);
                    break;
                }
            }
        }
        let lethal_directive =
            lethal_directive.expect("expected at least one lethal outcome across 200 rolls");

        // Compose the two into a single <directives> block the way fable_send
        // would. They must coexist without collision.
        let mut block = String::from("<directives>\n");
        block.push_str(&format!("[DIRECTIVE: {lethal_directive}]\n"));
        block.push_str(&format!(
            "[DIRECTIVE: witness relationship — {} now regards the player as Nemesis]\n",
            "witness"
        ));
        block.push_str("</directives>");

        // Both directives present, lethality first.
        let lethal_pos = block.find("DOWNED").or_else(|| {
            // Some directives may phrase it differently; accept any lethal
            // marker. Re-check the directive content for "down" (lowercase).
            block.to_lowercase().find("down")
        });
        assert!(
            lethal_pos.is_some(),
            "lethality directive must appear in the composed block"
        );
        assert!(
            block.contains("Nemesis"),
            "witness relationship drop must appear in the composed block"
        );
    }
}

// ===========================================================================
// Phase 3 WIRING tests (2026-07-28).
//
// Companion to `phase3_integration_tests`. Where that module verified the
// SEAMS COMPOSE (the public APIs of the six slices), this module verifies
// the WIRING — the new code paths that connect those seams to the live game
// loop. Covers: the silent-strip relationship firewall, the three new
// bracket commands ([EFFECT]/[MILESTONE]/[TASK]) parsing + state mutation,
// the tier-selection heuristic, and the bracket→schema→tick data flow.
//
// These tests use the PURE helpers directly (no AppState, no async runtime,
// no schema lock) — the wiring functions that touch AppState are exercised
// via the live playtest instead, mirroring the §11.26 pattern.
// ===========================================================================
#[cfg(test)]
mod phase3_wiring_tests {
    use super::strip_invalid_relationship_writes;
    use crate::bracket_parser::{self, BracketCommand};
    use crate::consequence::{self, Polarity, StatusTag};
    use crate::offscreen_task::{self, OffScreenTask, Suitability, TaskDifficulty};
    use crate::player_state;
    use crate::relationship::{self, RelationshipTier};
    use crate::schema::{SchemaDelta, WorldSchema};

    use std::collections::HashMap;

    // Helper: build a SchemaDelta with a single entity write.
    fn delta_with_entity(key: &str, value: &str) -> SchemaDelta {
        let mut entities = HashMap::new();
        entities.insert(key.to_string(), Some(serde_json::Value::String(value.to_string())));
        SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(entities),
        }
    }

    // ------------------------------------------------------------------------
    // Slice 5 wiring: strip_invalid_relationship_writes (the silent firewall).
    // ------------------------------------------------------------------------

    /// An LLM attempt to write `rel.npc.marcus=trusted` on a Stranger NPC is
    /// silently stripped from the delta (the key is removed). The rest of the
    /// delta is untouched. This is the anti-sycophancy firewall: the model
    /// can't talk its way past the gates by writing a flattering tier.
    #[test]
    fn wiring_rel_gated_write_stripped_silently() {
        let mut delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some({
                let mut m = HashMap::new();
                m.insert("rel.npc.marcus".to_string(), Some(serde_json::Value::String("trusted".into())));
                // A legitimate non-rel write that MUST survive the strip.
                m.insert("weather".to_string(), Some(serde_json::Value::String("rainy".into())));
                m
            }),
        };
        let mut schema = WorldSchema::default();

        let stripped = strip_invalid_relationship_writes(&mut delta, &mut schema);

        // The rel.* key was stripped.
        assert_eq!(
            stripped.len(),
            1,
            "exactly the rel.* key should be stripped"
        );
        // The non-rel key survived.
        let ents = delta.entities.as_ref().expect("entities still present");
        assert!(
            !ents.contains_key("rel.npc.marcus"),
            "the rel.* key must be removed from the delta"
        );
        assert_eq!(
            ents.get("weather"),
            Some(&Some(serde_json::Value::String("rainy".into()))),
            "the non-rel key must survive the strip"
        );
        // The schema's relationship map was NOT mutated (the write was
        // rejected, not accepted — Stranger stays Stranger).
        assert!(
            !schema.relationships.contains_key("npc.marcus"),
            "a rejected tier write must NOT create a relationship entry"
        );
    }

    /// The ONE accepted LLM transition is Stranger → Acquaintance. When the
    /// LLM writes `rel.npc.smith=acquaintance` on an untracked NPC, the
    /// helper accepts it, upserts the RelationshipState (tier=Acquaintance,
    /// tier_entered_at = current clock), AND removes the key from the delta
    /// (the canonical state is the source of truth, not the entity map).
    #[test]
    fn wiring_rel_stranger_to_acquaintance_accepted_and_upserted() {
        let mut delta = delta_with_entity("rel.npc.smith", "acquaintance");
        let mut schema = WorldSchema::default();
        schema.world_clock.current_minutes = 5000;

        let stripped = strip_invalid_relationship_writes(&mut delta, &mut schema);

        // Nothing stripped (the write was accepted).
        assert!(
            stripped.is_empty(),
            "the accepted Stranger→Acquaintance write must not be in the stripped list; got {:?}",
            stripped
        );
        // The key was removed from the delta (consumed into canonical state).
        assert!(
            !delta
                .entities
                .as_ref()
                .unwrap()
                .contains_key("rel.npc.smith"),
            "the accepted rel.* key must be removed from the delta (canonical state is source of truth)"
        );
        // The schema's relationship map now tracks the NPC at Acquaintance.
        let rel = schema
            .relationships
            .get("npc.smith")
            .expect("the accepted write must create a relationship entry");
        assert_eq!(
            rel.tier,
            RelationshipTier::Acquaintance,
            "the tier must be Acquaintance"
        );
        assert_eq!(
            rel.tier_entered_at_minutes, 5000,
            "tier_entered_at_minutes must be stamped to the current clock"
        );
    }

    /// A delete attempt (None value) on a rel.* key is also stripped — the
    /// LLM can't delete a relationship; only Rust owns that.
    #[test]
    fn wiring_rel_delete_attempt_stripped() {
        let mut entities = HashMap::new();
        entities.insert("rel.npc.foe".to_string(), None); // delete signal
        let mut delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(entities),
        };
        let mut schema = WorldSchema::default();

        let stripped = strip_invalid_relationship_writes(&mut delta, &mut schema);

        assert_eq!(stripped.len(), 1, "the delete attempt must be stripped");
        assert!(
            !delta.entities.as_ref().unwrap().contains_key("rel.npc.foe"),
            "the delete key must be removed from the delta"
        );
    }

    /// An unparseable tier value (e.g. "best_friend_forever") is silently
    /// dropped — same silent-drop policy as a gated rejection.
    #[test]
    fn wiring_rel_unparseable_value_dropped() {
        let mut delta = delta_with_entity("rel.npc.mara", "best_friend_forever");
        let mut schema = WorldSchema::default();

        let stripped = strip_invalid_relationship_writes(&mut delta, &mut schema);

        assert_eq!(stripped.len(), 1);
        assert!(
            stripped[0].contains("unparseable"),
            "the strip reason must note unparseable; got: {:?}",
            stripped[0]
        );
    }

    /// A delta with NO rel.* keys is a no-op — the helper returns empty +
    /// doesn't touch the delta.
    #[test]
    fn wiring_rel_no_rel_keys_is_noop() {
        let mut delta = SchemaDelta {
            summary: Some("scene advanced".to_string()),
            recent_events: Some(vec!["the guard arrived".to_string()]),
            entities: Some({
                let mut m = HashMap::new();
                m.insert("weather".to_string(), Some(serde_json::Value::String("stormy".into())));
                m.insert("door.cellar".to_string(), Some(serde_json::Value::String("open".into())));
                m
            }),
        };
        let mut schema = WorldSchema::default();
        let original_summary = delta.summary.clone();
        let original_events_len = delta.recent_events.as_ref().unwrap().len();
        let original_entities_len = delta.entities.as_ref().unwrap().len();

        let stripped = strip_invalid_relationship_writes(&mut delta, &mut schema);

        assert!(stripped.is_empty(), "no rel.* keys → nothing stripped");
        // Delta entirely unchanged.
        assert_eq!(delta.summary, original_summary);
        assert_eq!(delta.recent_events.as_ref().unwrap().len(), original_events_len);
        assert_eq!(delta.entities.as_ref().unwrap().len(), original_entities_len);
    }

    // ------------------------------------------------------------------------
    // Bracket command parsing: [EFFECT], [MILESTONE], [TASK].
    // ------------------------------------------------------------------------

    /// [EFFECT Berserk Rage buff 60] parses into the Effect command with the
    /// label preserved (spaces allowed), buff polarity, 60-minute duration.
    #[test]
    fn wiring_effect_bracket_parses_multi_word_label() {
        let parsed = bracket_parser::parse("The rage takes you. [EFFECT Berserk Rage buff 60]");
        let effects: Vec<&BracketCommand> = parsed
            .commands
            .iter()
            .filter(|c| matches!(c, BracketCommand::Effect { .. }))
            .collect();
        assert_eq!(effects.len(), 1, "exactly one EFFECT command");
        if let BracketCommand::Effect {
            label,
            polarity,
            duration_minutes,
            ..
        } = effects[0]
        {
            assert_eq!(label, "Berserk Rage");
            assert_eq!(*polarity, Polarity::Buff);
            assert_eq!(*duration_minutes, 60);
        } else {
            panic!("wrong variant");
        }
        // The bracket is stripped from the prose.
        assert!(
            !parsed.prose.contains("[EFFECT"),
            "the EFFECT bracket must be stripped from prose"
        );
    }

    /// [EFFECT ... debuff 0] — duration 0 is the permanent sentinel.
    #[test]
    fn wiring_effect_bracket_zero_duration_is_permanent() {
        let parsed =
            bracket_parser::parse("She feels the curse settle. [EFFECT Cursed by the Witch King debuff 0]");
        if let Some(BracketCommand::Effect {
            duration_minutes, ..
        }) = parsed.commands.iter().find_map(|c| match c {
            BracketCommand::Effect { .. } => Some(c),
            _ => None,
        }) {
            assert_eq!(*duration_minutes, 0, "duration 0 = permanent sentinel");
        } else {
            panic!("EFFECT command not found");
        }
    }

    /// A malformed [EFFECT ...] is dropped gracefully — the bracket leaks
    /// into prose as a literal, no panic.
    ///
    /// NOTE: the EFFECT parser is deliberately TOLERANT of an unknown
    /// polarity token (the 2026-07-28 playtest found the model emits both
    /// strict and sloppy shapes): an unknown word between the label and
    /// the duration is folded into the label and the polarity is inferred
    /// from the label. So `[EFFECT Rage super 60]` is ACCEPTED as
    /// `Effect { label: "Rage super", polarity: Debuff, duration: 60 }`,
    /// NOT rejected. This test therefore pins only the genuinely-fatal
    /// malformed shapes: missing duration, negative duration, float
    /// duration, and bare `[EFFECT]`. The acceptance-tolerance contract
    /// is pinned separately by `wiring_effect_bracket_parses_*`.
    #[test]
    fn wiring_effect_bracket_malformed_is_noop() {
        for inp in [
            "text [EFFECT Rage buff]",        // missing duration
            "text [EFFECT Rage]",             // label only
            "text [EFFECT]",                  // bare
            "text [EFFECT Rage buff -5]",     // negative duration
            "text [EFFECT Rage buff 3.5]",    // float duration
        ] {
            let parsed = bracket_parser::parse(inp);
            assert!(
                !parsed
                    .commands
                    .iter()
                    .any(|c| matches!(c, BracketCommand::Effect { .. })),
                "malformed EFFECT must not produce a command: {:?}",
                inp
            );
        }
    }

    /// [MILESTONE npc.marcus saved_life] parses cleanly.
    #[test]
    fn wiring_milestone_bracket_parses() {
        let parsed = bracket_parser::parse(
            "He pulls you from the river. [MILESTONE npc.marcus saved_life]",
        );
        if let Some(BracketCommand::Milestone { npc_id, event_id }) = parsed
            .commands
            .iter()
            .find_map(|c| match c {
                BracketCommand::Milestone { .. } => Some(c),
                _ => None,
            })
        {
            assert_eq!(npc_id, "npc.marcus");
            assert_eq!(event_id, "saved_life");
        } else {
            panic!("MILESTONE command not found");
        }
    }

    /// [TASK npc.marcus scout the bandit camp | challenging adequate 240]
    /// parses into the Task command with the multi-word description preserved.
    #[test]
    fn wiring_task_bracket_parses_multi_word_description() {
        let parsed = bracket_parser::parse(
            "Marcus nods and slips out. [TASK npc.marcus scout the bandit camp | challenging adequate 240]",
        );
        if let Some(BracketCommand::Task {
            npc_id,
            description,
            difficulty,
            suitability,
            eta_minutes,
        }) = parsed.commands.iter().find_map(|c| match c {
            BracketCommand::Task { .. } => Some(c),
            _ => None,
        }) {
            assert_eq!(npc_id, "npc.marcus");
            assert_eq!(description, "scout the bandit camp");
            assert_eq!(difficulty, "challenging");
            assert_eq!(suitability, "adequate");
            assert_eq!(*eta_minutes, 240);
        } else {
            panic!("TASK command not found");
        }
    }

    /// A malformed [TASK ...] is dropped gracefully — the bracket leaks
    /// into prose as a literal, no panic.
    ///
    /// NOTE: the TASK parser is deliberately TOLERANT of a missing `|`
    /// separator (the 2026-07-28 playtest found the model sometimes omits
    /// the pipe): `parse_task_no_pipe` falls back to splitting the whole
    /// body by whitespace — first token = npc_id, last 3 =
    /// difficulty/suitability/eta, middle (joined) = description. So
    /// `[TASK npc.marcus scout challenging adequate 240]` is ACCEPTED,
    /// NOT rejected. This test therefore pins only the genuinely-fatal
    /// malformed shapes: non-numeric eta, zero eta, negative eta, bare
    /// `[TASK]`, and a too-short body that can't yield 4 fields. The
    /// no-pipe acceptance contract is pinned separately by
    /// `wiring_task_bracket_parses_multi_word_description`.
    #[test]
    fn wiring_task_bracket_malformed_is_noop() {
        for inp in [
            // Non-numeric eta (well-formed pipe, garbage eta).
            "text [TASK npc.marcus scout | challenging adequate soon]",
            // Zero eta — rejected (eta must be > 0; a 0-minute task is meaningless).
            "text [TASK npc.x d | e i 0]",
            // Negative eta.
            "text [TASK npc.x d | e i -1]",
            // Bare.
            "text [TASK]",
            // Too short to yield npc_id + description + 3 tail fields.
            "text [TASK a | b c d]",
        ] {
            let parsed = bracket_parser::parse(inp);
            assert!(
                !parsed
                    .commands
                    .iter()
                    .any(|c| matches!(c, BracketCommand::Task { .. })),
                "malformed TASK must not produce a command: {:?}",
                inp
            );
        }
    }

    // ------------------------------------------------------------------------
    // Slice 3 wiring: select_attacker_tier_from_entities.
    // ------------------------------------------------------------------------

    /// No npc.*.tier keys → defaults to Soldier (preserves the v1 distribution).
    #[test]
    fn wiring_tier_selection_defaults_to_soldier() {
        let entities = HashMap::new(); // empty
        assert_eq!(
            player_state::select_attacker_tier_from_entities(&entities),
            player_state::AttackerTier::Soldier,
            "no tier keys → Soldier default (backwards-compatible)"
        );

        // Entities present but no tier keys.
        let mut entities = HashMap::new();
        entities.insert("weather".to_string(), serde_json::Value::String("rainy".into()));
        entities.insert("npc.marcus.name".to_string(), serde_json::Value::String("Marcus".into()));
        assert_eq!(
            player_state::select_attacker_tier_from_entities(&entities),
            player_state::AttackerTier::Soldier,
            "entities without tier keys → Soldier default"
        );
    }

    /// A single npc.dragon.tier=legendary → Legendary tier (the anti-Oblivion
    /// enforcement: a dragon's blows weight toward Critical + lethality).
    #[test]
    fn wiring_tier_selection_picks_declared_legendary() {
        let mut entities = HashMap::new();
        entities.insert(
            "npc.dragon.tier".to_string(),
            serde_json::Value::String("legendary".into()),
        );
        assert_eq!(
            player_state::select_attacker_tier_from_entities(&entities),
            player_state::AttackerTier::Legendary,
        );
    }

    /// When multiple tier keys exist, the HIGHEST threat wins (the dangerous
    /// foe dominates the severity distribution in a multi-foe fight).
    #[test]
    fn wiring_tier_selection_picks_highest_threat() {
        let mut entities = HashMap::new();
        entities.insert("npc.thug1.tier".to_string(), serde_json::Value::String("soldier".into()));
        entities.insert("npc.dragon.tier".to_string(), serde_json::Value::String("legendary".into()));
        entities.insert("npc.goblin1.tier".to_string(), serde_json::Value::String("minion".into()));
        assert_eq!(
            player_state::select_attacker_tier_from_entities(&entities),
            player_state::AttackerTier::Legendary,
            "Legendary must dominate over Soldier + Minion"
        );
    }

    /// Tier synonyms parse correctly (dragon → Legendary, bandit → Soldier,
    /// goblin → Minion). This is the narrator-friendly tolerant parse.
    #[test]
    fn wiring_tier_selection_accepts_synonyms() {
        let cases = [
            ("dragon", player_state::AttackerTier::Legendary),
            ("apex", player_state::AttackerTier::Legendary),
            ("ancient", player_state::AttackerTier::Legendary),
            ("warlord", player_state::AttackerTier::Boss),
            ("troll", player_state::AttackerTier::Boss),
            ("veteran", player_state::AttackerTier::Elite),
            ("knight", player_state::AttackerTier::Elite),
            ("bandit", player_state::AttackerTier::Soldier),
            ("grunt", player_state::AttackerTier::Soldier),
            ("goblin", player_state::AttackerTier::Minion),
            ("wolf", player_state::AttackerTier::Minion),
        ];
        for (input, expected) in &cases {
            assert_eq!(
                player_state::parse_attacker_tier(input),
                Some(*expected),
                "synonym {:?} should parse to {:?}",
                input,
                expected
            );
        }
    }

    // ------------------------------------------------------------------------
    // Slice 2 wiring: derive_condition consumes the tag counts the wiring
    // computes. Verify the wiring's count computation matches what
    // derive_condition expects (the seam between the render hook + the pure
    // derive fn).
    // ------------------------------------------------------------------------

    /// count_by_polarity + derive_condition compose: a body with a Yellow
    /// wound + 1 buff tag derives to Unscathed (the buff lifts it), and the
    /// wiring's count computation feeds derive_condition correctly.
    #[test]
    fn wiring_condition_uses_tag_counts_correctly() {
        let mut body = std::collections::HashMap::new();
        body.insert(
            player_state::BodyPart::LeftHand,
            player_state::BodyPartState::Yellow,
        );
        let tags = vec![
            StatusTag {
                label: "Berserk Rage".to_string(),
                polarity: Polarity::Buff,
                expires_at: 1000,
                source: String::new(),
            kind: String::new(),
            },
        ];
        // The wiring computes these counts then passes them to derive_condition.
        let buffs = consequence::count_by_polarity(&tags, Polarity::Buff);
        let debuffs = consequence::count_by_polarity(&tags, Polarity::Debuff);
        assert_eq!(buffs, 1);
        assert_eq!(debuffs, 0);
        // Yellow + 1 buff + 0 debuffs → Unscathed (the buff lifts Haggard→Unscathed).
        assert_eq!(
            consequence::derive_condition(&body, buffs, debuffs),
            consequence::Condition::Unscathed,
        );
    }

    // ------------------------------------------------------------------------
    // Slice 4 wiring: expire_tags drops expired tags but keeps permanent ones
    // (expires_at == 0). Verify the tick hook's contract.
    // ------------------------------------------------------------------------

    /// The tick's expire_tags call drops tags whose expiry has passed but
    /// keeps permanent (expires_at == 0) tags. This is the seam the tick
    /// handler in apply_time_command_and_maybe_tick calls.
    #[test]
    fn wiring_tick_expiry_drops_expired_keeps_permanent() {
        let mut tags = vec![
            StatusTag {
                label: "Permanent Curse".to_string(),
                polarity: Polarity::Debuff,
                expires_at: 0, // permanent
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Short Buff".to_string(),
                polarity: Polarity::Buff,
                expires_at: 500, // expired at minute 1000
                source: String::new(),
            kind: String::new(),
            },
            StatusTag {
                label: "Active Buff".to_string(),
                polarity: Polarity::Buff,
                expires_at: 2000, // still active at minute 1000
                source: String::new(),
            kind: String::new(),
            },
        ];
        let dropped = consequence::expire_tags(&mut tags, 1000);
        assert_eq!(dropped, 1, "only the expired timed tag drops");
        assert_eq!(tags.len(), 2, "permanent + active remain");
        let labels: Vec<&str> = tags.iter().map(|t| t.label.as_str()).collect();
        assert!(labels.contains(&"Permanent Curse"), "permanent tag survives");
        assert!(labels.contains(&"Active Buff"), "active timed tag survives");
        assert!(
            !labels.contains(&"Short Buff"),
            "expired timed tag is dropped"
        );
    }

    // ------------------------------------------------------------------------
    // Slice 6 wiring: resolve_expired_tasks is the seam the tick calls. The
    // resolved-task retention logic (drop resolved, keep pending) is what the
    // tick handler implements — verify the data flow shape.
    // ------------------------------------------------------------------------

    /// A task queue with one due + one not-yet-due task: resolve_expired_tasks
    /// returns one resolution, and the caller's retain logic keeps the
    //  not-yet-due task. This mirrors the tick's retain pattern.
    #[test]
    fn wiring_tick_task_resolution_retain_pattern() {
        let tasks = vec![
            OffScreenTask {
                npc_id: "npc.marcus".to_string(),
                description: "scout".to_string(),
                difficulty: TaskDifficulty::Routine,
                suitability: Suitability::Ideal,
                resolves_at_minutes: 500, // due at minute 1000
                resolved: false,
            },
            OffScreenTask {
                npc_id: "npc.lyra".to_string(),
                description: "negotiate".to_string(),
                difficulty: TaskDifficulty::Challenging,
                suitability: Suitability::Adequate,
                resolves_at_minutes: 5000, // not yet due at minute 1000
                resolved: false,
            },
        ];
        let now = 1000_i64;
        let resolutions = offscreen_task::resolve_expired_tasks(&tasks, now);
        assert_eq!(resolutions.len(), 1, "only the due task resolves");
        assert_eq!(resolutions[0].npc_id, "npc.marcus");

        // The tick's retain pattern: keep tasks that weren't just resolved.
        // (In the live code this uses (npc_id, description, dc) tuples; here
        // we approximate by npc_id+description to verify the data flow.)
        let resolved_keys: std::collections::HashSet<(String, String)> = resolutions
            .iter()
            .map(|r| (r.npc_id.clone(), r.description.clone()))
            .collect();
        let remaining: Vec<&OffScreenTask> = tasks
            .iter()
            .filter(|t| !resolved_keys.contains(&(t.npc_id.clone(), t.description.clone())))
            .collect();
        assert_eq!(remaining.len(), 1, "the not-yet-due task is retained");
        assert_eq!(remaining[0].npc_id, "npc.lyra");
    }

    // ------------------------------------------------------------------------
    // Schema deserialization: the new fields default cleanly on pre-Phase-3
    // saves (the #[serde(default)] contract). This is the load-compatibility
    // guarantee — an old save without the new fields must deserialize.
    // ------------------------------------------------------------------------

    /// A JSON world-schema with NO status_tags / relationships / offscreen_tasks
    /// fields (a pre-Phase-3 save) deserializes with all three defaulting to
    /// empty. This is the save-compatibility firewall.
    #[test]
    fn wiring_schema_loads_pre_phase3_save_with_empty_defaults() {
        // A minimal pre-Phase-3 save: only the original fields, none of the
        // new Phase 3 wiring fields.
        let pre_phase3_json = r#"{
            "summary": "an old save",
            "recent_events": [],
            "entities": {},
            "player_state": {
                "body": {},
                "stamina": "Fresh",
                "wealth": 0,
                "reputation": 0
            },
            "world_clock": { "current_minutes": 0, "last_tick_minutes": 0 },
            "immutable_keys": [],
            "scene_pacing": {
                "mode": "Exploration",
                "spatial": 0,
                "emotional": 0,
                "kinetic": 0
            }
        }"#;
        let schema: WorldSchema =
            serde_json::from_str(pre_phase3_json).expect("pre-Phase-3 save must deserialize");
        assert!(schema.status_tags.is_empty(), "status_tags defaults to empty");
        assert!(schema.relationships.is_empty(), "relationships defaults to empty");
        assert!(schema.offscreen_tasks.is_empty(), "offscreen_tasks defaults to empty");
        // The original fields load correctly.
        assert_eq!(schema.summary, "an old save");
    }

    /// A round-trip serialize → deserialize preserves the new fields. This
    /// pins the save/load integrity for the wiring storage.
    #[test]
    fn wiring_schema_roundtrip_preserves_new_fields() {
        let mut schema = WorldSchema::default();
        schema.status_tags.push(StatusTag {
            label: "Test Buff".to_string(),
            polarity: Polarity::Buff,
            expires_at: 1000,
            source: "test".to_string(),
        kind: String::new(),
        });
        schema.world_clock.current_minutes = 500;
        let mut rel = relationship::RelationshipState::default();
        rel.tier = RelationshipTier::Friendly;
        schema.relationships.insert("npc.test".to_string(), rel);
        schema.offscreen_tasks.push(OffScreenTask {
            npc_id: "npc.test".to_string(),
            description: "test task".to_string(),
            difficulty: TaskDifficulty::Routine,
            suitability: Suitability::Adequate,
            resolves_at_minutes: 1000,
            resolved: false,
        });

        let json = serde_json::to_string(&schema).expect("serialize");
        let loaded: WorldSchema = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(loaded.status_tags.len(), 1);
        assert_eq!(loaded.status_tags[0].label, "Test Buff");
        assert_eq!(loaded.relationships.len(), 1);
        assert_eq!(
            loaded.relationships.get("npc.test").unwrap().tier,
            RelationshipTier::Friendly
        );
        assert_eq!(loaded.offscreen_tasks.len(), 1);
        assert_eq!(loaded.offscreen_tasks[0].npc_id, "npc.test");
    }

    // ------------------------------------------------------------------------
    // SchemaDelta shape: confirm the new wiring doesn't change the delta's
    // serialization (the LLM contract is unchanged — the model never sees the
    // new storage fields, only the entity map it already writes).
    // ------------------------------------------------------------------------

    /// SchemaDelta still serializes/deserializes with the same shape (no new
    /// fields added to the delta itself — the wiring intercepts at apply time).
    #[test]
    fn wiring_schema_delta_shape_unchanged() {
        let delta = delta_with_entity("rel.npc.x", "trusted");
        let json = serde_json::to_string(&delta).expect("serialize");
        // The delta has only summary/recent_events/entities — no relationship
        // or tag fields. The LLM contract is unchanged.
        let loaded: SchemaDelta = serde_json::from_str(&json).expect("deserialize");
        assert!(loaded.summary.is_none());
        assert!(loaded.recent_events.is_none());
        assert!(loaded.entities.is_some());
    }

    // ------------------------------------------------------------------------
    // Bug A fix wiring (2026-07-28): JSON dual-parser produces the SAME
    // BracketCommand variants the bracket-path wiring tests above assert
    // against. Each test below mirrors an existing bracket wiring test with
    // the equivalent JSON input the model actually emits. This proves the
    // downstream consumers (apply_phase3_bracket_commands, scene_event
    // emission, the World Progression tick) are format-agnostic — they only
    // ever see BracketCommand, never the raw JSON or bracket text.
    // ------------------------------------------------------------------------

    /// JSON equivalent of wiring_effect_bracket_parses_multi_word_label.
    /// Same fields, same variant, same assertion shape.
    #[test]
    fn wiring_effect_json_parses_to_same_variant() {
        let parsed = bracket_parser::parse(
            "The rage takes you.\n```json\n{\"type\":\"effect\",\"label\":\"Berserk Rage\",\"polarity\":\"buff\",\"duration_minutes\":60}\n```",
        );
        let effects: Vec<&BracketCommand> = parsed
            .commands
            .iter()
            .filter(|c| matches!(c, BracketCommand::Effect { .. }))
            .collect();
        assert_eq!(effects.len(), 1, "exactly one Effect from JSON");
        if let BracketCommand::Effect { label, polarity, duration_minutes, .. } = effects[0] {
            assert_eq!(label, "Berserk Rage");
            assert_eq!(*polarity, Polarity::Buff);
            assert_eq!(*duration_minutes, 60);
        } else {
            unreachable!();
        }
        assert!(!parsed.prose.contains("```"), "fence stripped from prose");
        assert!(!parsed.prose.contains("Berserk"), "JSON body stripped from prose");
    }

    /// The exact shape the model emitted in the 2026-07-28 playtest
    /// (`effect_name` / `effect_label` / `effect_duration_minutes` field
    /// names — the model's invented aliases). Must map cleanly to Effect.
    #[test]
    fn wiring_effect_json_accepts_model_invented_field_names() {
        let parsed = bracket_parser::parse(
            "Prose.\n```json\n{\"effect_name\":\"exploration\",\"effect_label\":\"exploration\",\"effect_duration_minutes\":15}\n```",
        );
        let effects: Vec<&BracketCommand> = parsed
            .commands
            .iter()
            .filter(|c| matches!(c, BracketCommand::Effect { .. }))
            .collect();
        assert_eq!(effects.len(), 1);
        if let BracketCommand::Effect { label, duration_minutes, .. } = effects[0] {
            assert_eq!(label, "exploration");
            assert_eq!(*duration_minutes, 15);
        } else {
            unreachable!();
        }
    }

    /// JSON equivalent of wiring_milestone_bracket_parses.
    #[test]
    fn wiring_milestone_json_parses_to_same_variant() {
        let parsed = bracket_parser::parse(
            "He pulls you from the river.\n```json\n{\"type\":\"milestone\",\"npc_id\":\"npc.marcus\",\"event_id\":\"saved_life\"}\n```",
        );
        let milestones: Vec<&BracketCommand> = parsed
            .commands
            .iter()
            .filter(|c| matches!(c, BracketCommand::Milestone { .. }))
            .collect();
        assert_eq!(milestones.len(), 1);
        if let BracketCommand::Milestone { npc_id, event_id } = milestones[0] {
            assert_eq!(npc_id, "npc.marcus");
            assert_eq!(event_id, "saved_life");
        } else {
            unreachable!();
        }
    }

    /// JSON equivalent of wiring_task_bracket_parses_multi_word_description.
    #[test]
    fn wiring_task_json_parses_to_same_variant() {
        let parsed = bracket_parser::parse(
            "Marcus nods and slips out.\n```json\n{\"type\":\"task\",\"npc_id\":\"npc.marcus\",\"description\":\"scout the bandit camp\",\"difficulty\":\"challenging\",\"suitability\":\"adequate\",\"eta_minutes\":240}\n```",
        );
        let tasks: Vec<&BracketCommand> = parsed
            .commands
            .iter()
            .filter(|c| matches!(c, BracketCommand::Task { .. }))
            .collect();
        assert_eq!(tasks.len(), 1);
        if let BracketCommand::Task { npc_id, description, difficulty, suitability, eta_minutes } =
            tasks[0]
        {
            assert_eq!(npc_id, "npc.marcus");
            assert_eq!(description, "scout the bandit camp");
            assert_eq!(difficulty, "challenging");
            assert_eq!(suitability, "adequate");
            assert_eq!(*eta_minutes, 240);
        } else {
            unreachable!();
        }
    }
}
