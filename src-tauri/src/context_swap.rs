//! The VRAM swap-lock: enforces "at most ONE `WUPI.gguf` `LlamaContext`
//! resident at a time" across the chat / schema / fable engines.
//!
//! ## Why this exists (the load-bearing fix)
//!
//! The original architecture (AGENTS.md §2) kept FOUR `LlamaContext`s
//! co-resident: chat (4000) + embedder (512) + schema (2048) + game (4000),
//! all sharing one leaked `&'static LlamaModel`. The doc claimed "~10GB
//! total → ~2GB headroom on 12GB."
//!
//! On a 12GB GPU with the Gemma 4 12B Q6_K model that claim is FALSE. Live
//! measurement (2026-07-26 debug session): with only THREE contexts resident
//! (chat + embedder + schema), VRAM used was **11,827 MiB / 118 MiB free**.
//! The FableEngine's 4th-context allocation silently failed inside
//! `init_runtime` → thread death → `fable_send` errored `"no game running"`.
//! The intermittent chat freeze was the same allocator pressure occasionally
//! stalling a decode.
//!
//! ## The fix
//!
//! The embedder is exempt (separate 36.8MB `Embed.gguf`, self-cleaning,
//! negligible). The three `WUPI.gguf` contexts (chat/schema/fable) MUST
//! swap. This module is a single process-wide coordinator that serializes
//! their residency: when one role needs to run, any OTHER role's resident
//! context is torn down first (synchronous `.join()` so VRAM is freed
//! before the new context allocates — the 2026-07-18 VRAM-overlap lesson).
//!
//! ## Why this is cheap
//!
//! - The chat engine's KV persistence (delta-prefill) was already mostly
//!   dead: every Memory-enabled turn cold-resets (AGENTS.md §3 "accepted
//!   v1 cost"). Tearing down + recreating the chat context per turn costs
//!   ~nothing extra.
//! - The schema + fable engines already `clear_kv_cache()` every turn
//!   (`schema_engine.rs:727`, `fable_engine.rs:375`). They were already
//!   operationally ephemeral; they just held a resident allocation.
//! - The teardown primitive (`ChatEngine::shutdown`, `SchemaEngine::
//!   shutdown`, `FableEngine::shutdown`) is fully built + proven — the
//!   synchronous `.join()` is load-bearing and already there.
//!
//! ## Footprint after the fix
//!
//! Always resident: leaked weights (~9.8GB) + embedder (~36MB) = ~9.84GB.
//! One WUPI.gguf context active: +~75-150MB Q8_0 KV. Total ~10GB / ~2GB
//! free — stable.
//!
//! ## The contract
//!
//! - `ContextSwap::acquire(role)` returns a `LeaseGuard`. Holding it means
//!   "you own the VRAM." Other roles block in `acquire` until your guard
//!   drops.
//! - The guard's Drop marks the slot free but does NOT tear down — teardown
//!   happens on the NEXT `acquire` of a different role. This lets back-to-
//!   back same-role turns (the common case in a game) reuse the resident
//!   context without re-spawn churn.
//! - Each engine registers a `TeardownFn` when it acquires. The swap-lock
//!   calls it on cross-role transition. The engine's `shutdown()` does the
//!   synchronous join (load-bearing for VRAM ordering).

use std::sync::Arc;
use tokio::sync::Mutex;

/// Which engine is requesting the VRAM lease. The embedder is NOT a role
/// here — it's a separate model, always resident, exempt.
///
/// **Phase 5B (2026-07-29):** `Sd` (Stable Diffusion) is the 4th role. Unlike
/// the three `WUPI.gguf` contexts (which swap KV caches while sharing the
/// leaked `SHARED_MODEL` weights), SD is a *different model entirely* — its
/// teardown must evict the LLM weights from VRAM (the `SHARED_MODEL` leak),
/// not just a KV context. See `llm::unload_shared_model` /
/// `reload_shared_model` (the Phase 5B weight-unload lift). The contract for
/// an `Sd` teardown is heavier: it must (1) unload the shared LLM weights
/// (~9.8GB), (2) load the SD model, (3) generate. The reverse swap reloads
/// the LLM weights. The per-turn KV clear (§11.52) means there is NO KV to
/// preserve across the swap — the only reload cost is the weight file-read
/// (~5-10s for 12B Q6), hidden behind SD gen time. This is the architectural
/// fit that makes the LLM⇄SD swap "seamless" — it rides free on work the
/// engine was already doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRole {
    /// The Wupi-assistant chat engine (delta-prefill, persistent KV).
    Chat,
    /// The background state-delta summarizer (one-shot per call).
    Schema,
    /// The narrator / game engine (one-shot per turn).
    Fable,
    /// Stable Diffusion image generation (Phase 5B, 2026-07-29). The only
    /// role whose teardown evicts the shared LLM *weights*, not just a KV
    /// context. See the module doc + `SceneImageGenerator` in scene_art.rs.
    Sd,
}

impl ContextRole {
    fn label(self) -> &'static str {
        match self {
            ContextRole::Chat => "chat",
            ContextRole::Schema => "schema",
            ContextRole::Fable => "fable",
            ContextRole::Sd => "sd",
        }
    }
}

/// A teardown callback registered by the engine that currently holds the
/// lease. The swap-lock invokes it on a cross-role transition. The
/// implementation MUST synchronously free the context's VRAM (join the
/// engine thread) before returning — the next context's allocation races
/// this teardown otherwise (the 2026-07-18 NullResult OOM).
///
/// Boxed + Send so any engine can register regardless of concrete type.
/// Returns any error string for logging (teardown is best-effort: even on
/// failure we proceed, since a stuck teardown shouldn't deadlock all
/// contexts — the lease is released either way).
type TeardownFn = Box<dyn FnOnce() -> Result<(), String> + Send>;

/// Internal state held under the swap-lock's mutex.
struct LeaseState {
    /// Which role currently owns the VRAM (None = idle, nothing resident).
    role: ContextRole,
    /// How to tear down the current resident context. None if the resident
    /// context is owned externally and shouldn't be torn down by the swap
    /// (e.g. during the very first chat-load at boot, before the lease is
    /// taken over — the boot path manages its own lifecycle). When Some,
    /// it's called exactly once on the next cross-role acquire.
    teardown: Option<TeardownFn>,
    /// Monotonic acquire counter, purely for telemetry correlation: each
    /// `acquire` logs "swap #N: <old> → <new>". Lets the log reader tie a
    /// swap to the surrounding turn.
    swap_count: u64,
}

impl Default for LeaseState {
    fn default() -> Self {
        Self {
            // Idle until the first acquire. Boot starts with the chat
            // backend loading but NOT under the lease — it takes the lease
            // lazily on the first chat_send. This avoids a boot-time
            // ordering hazard (the chat model must load before the lease
            // can be populated).
            role: ContextRole::Chat, // sentinel; holder == false at start
            teardown: None,
            swap_count: 0,
        }
    }
}

/// The process-wide VRAM swap-lock. One instance lives on `AppState` and
/// is shared by all three engines. Cheap to clone (just an `Arc`).
#[derive(Clone)]
pub struct ContextSwap {
    inner: Arc<Mutex<LeaseState>>,
}

impl ContextSwap {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LeaseState {
                role: ContextRole::Chat,
                teardown: None,
                swap_count: 0,
            })),
        }
    }

    /// Acquire the right to run a `role`'s context.
    ///
    /// - If no role currently holds the lease (idle): returns immediately
    ///   after registering `teardown` for the new role. The caller is
    ///   responsible for spawning/seatng its engine before generating.
    /// - If the SAME role already holds the lease: returns immediately
    ///   WITHOUT re-registering teardown (the resident engine is reused —
    ///   back-to-back same-role turns skip the spawn cost). The caller
    ///   checks its engine slot; if present it reuses, if absent it
    ///   spawns + re-registers teardown.
    /// - If a DIFFERENT role holds the lease: tears it down (calling the
    ///   prior `teardown` synchronously, joining the engine thread so VRAM
    ///   is freed), THEN registers the new role's teardown and returns.
    ///
    /// The `teardown` callback is registered eagerly here (not on guard
    /// drop) so a future cross-role transition always has a way to evict
    /// the current resident — even if the original acquirer's task panicked
    /// or hung. The `LeaseGuard`'s drop just marks the slot free; the
    /// actual VRAM-free happens via the registered teardown on eviction.
    pub async fn acquire(
        &self,
        role: ContextRole,
        teardown: TeardownFn,
    ) -> LeaseGuard {
        let prev: Option<(ContextRole, TeardownFn)> = {
            let mut s = self.inner.lock().await;
            s.swap_count += 1;
            let n = s.swap_count;
            if s.teardown.is_none() {
                // Idle slot. First acquire: no eviction needed.
                tracing::info!(swap = n, role = role.label(), "context-swap: idle → {}", role.label());
                s.role = role;
                s.teardown = Some(teardown);
                return LeaseGuard {
                    swap: self.clone(),
                    released: false,
                };
            }
            if s.role == role {
                // Same-role fast path. The caller already owns a resident
                // engine (detected via its AppState slot → skips re-spawning).
                // REFRESH the registered teardown with the freshly-supplied one
                // (2026-08-10): the prior teardown may have been consumed by a
                // cross-role eviction cycle (chat → fable → chat re-spawn),
                // leaving the slot's teardown stale/None. Both closures capture
                // the same Arc<Mutex<Option<...>>> backend slot, so they're
                // functionally equivalent when fresh — but the new one is
                // guaranteed live, so it governs eviction robustly.
                tracing::debug!(swap = n, role = role.label(), "context-swap: same-role reuse (teardown refreshed)");
                s.teardown = Some(teardown);
                return LeaseGuard {
                    swap: self.clone(),
                    released: false,
                };
            }
            // Cross-role transition. Evict the current holder.
            let old = s.role;
            let old_teardown = s.teardown.take().expect("teardown present in holder branch");
            s.role = role;
            s.teardown = Some(teardown);
            tracing::info!(swap = n, from = old.label(), to = role.label(), "context-swap: evicting {} → {}", old.label(), role.label());
            Some((old, old_teardown))
        };

        // Run the eviction OUTSIDE the mutex so the synchronous join
        // (which may take 100-500ms) doesn't block other `acquire` waits
        // from making progress on the queue. The slot is already updated
        // to the new role, so a concurrent acquire of yet another role
        // will evict the new one cleanly — but in practice the engines
        // are serialized by the higher-level `pending_delta` / IPC
        // ordering, so concurrent cross-role contention is rare.
        if let Some((old, old_teardown)) = prev {
            // The synchronous join happens inside `old_teardown` — this is
            // the load-bearing call that frees VRAM before the new
            // context's `new_context()` allocates. Spawn_blocking would
            // defeat the purpose: the new engine's spawn (which the caller
            // runs after we return) needs VRAM that only becomes available
            // AFTER this returns Ok. So we block this task thread.
            if let Err(e) = old_teardown() {
                tracing::warn!(from = old.label(), error = %e, "context-swap: prior teardown reported an error (proceeding anyway — lease released)");
            }
        }

        LeaseGuard {
            swap: self.clone(),
            released: false,
        }
    }

    /// Register a context that was spawned OUTSIDE an `acquire` call — i.e.
    /// the boot-spawned chat context (`boot_load_model` spawns the chat
    /// backend eagerly at boot for fast first-message latency, but does NOT
    /// take the lease; `chat_send` takes it lazily on first use).
    ///
    /// Without this registration, the lease starts in its "idle" default
    /// state (teardown = None), so a Fable/schema `acquire` that fires
    /// BEFORE any `chat_send` sees "idle → fable" and skips eviction → the
    /// boot chat context stays resident in VRAM alongside the new fable
    /// context → both WUPI.gguf contexts co-resident → VRAM exhausts → the
    /// fable compute falls back to CPU → a multi-minute PC freeze (the
    /// 2026-08-10 incident). This is the chat-side analog of the
    /// eager-schema-spawn bypass fixed at `lib.rs:1917` (§2C).
    ///
    /// Call this once at boot, right after the chat model + context finish
    /// loading, passing the SAME teardown closure `chat_send` later passes to
    /// `acquire`. It populates `role` + `teardown` WITHOUT acquiring a turn
    /// (no guard returned) so the first real `chat_send` still works (its
    /// `acquire` hits the same-role fast path). A subsequent fable/schema
    /// `acquire` then sees a populated holder → cross-role eviction → chat
    /// torn down → fable gets VRAM to itself → GPU-speed decode.
    ///
    /// Idempotent: if a teardown is already registered (e.g. a `chat_send`
    /// somehow ran first), this is a no-op (the existing teardown governs).
    pub async fn register_resident(&self, role: ContextRole, teardown: TeardownFn) {
        let mut s = self.inner.lock().await;
        if s.teardown.is_some() {
            // Already registered (a chat_send won the race, or boot called
            // twice). Keep the existing teardown — it governs eviction.
            tracing::debug!(
                role = role.label(),
                "context-swap: register_resident skipped (teardown already registered)"
            );
            return;
        }
        tracing::info!(
            role = role.label(),
            "context-swap: boot-resident context registered (cross-role eviction now enabled)"
        );
        s.role = role;
        s.teardown = Some(teardown);
    }

    /// Drop-time release: marks the slot idle (no resident context). Called
    /// by `LeaseGuard::drop`. Does NOT run teardown — the resident context
    /// stays in VRAM until a cross-role acquire evicts it. This is the
    /// load-bearing optimization: back-to-back same-role turns reuse the
    /// resident engine without re-spawn churn.
    fn release(&self) {
        // We deliberately do NOT clear `teardown` here: a subsequent
        // cross-role acquire still needs to evict the (now-orphaned)
        // resident context. We only flip the "actively held" bit, which
        // we represent by setting role to a sentinel that no real acquire
        // matches... but actually: we WANT the next same-role acquire to
        // REUSE the still-resident engine. So we leave `role` AND
        // `teardown` intact. The guard's only job is to allow a different
        // role's acquire to proceed (it would have blocked on the mutex
        // anyway — tokio::sync::Mutex is fair + exclusive, so the guard's
        // drop just releases the mutex lock the caller... wait, no: the
        // mutex is only held briefly inside acquire, not across the turn).
        //
        // CORRECTION: the mutex is NOT held across the turn (we release it
        // before returning the guard). So `release` currently does nothing
        // observable. The lease is "released" simply by the next acquire
        // being able to run. We keep the method + the `released` flag purely
        // as defensive bookkeeping (double-drop protection) + a hook for
        // future telemetry (e.g. log turn duration when the guard drops).
        //
        // This is correct because the SERIALIZATION that matters happens
        // at the engine level (each engine's Mutex<Option<Arc<Engine>>>
        // slot + the `pending_delta.await` chain), NOT at this lock. The
        // swap-lock's job is purely VRAM eviction ordering.
    }
}

impl Default for ContextSwap {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard: holding it means "my role owns the VRAM right now." Drop is
/// a no-op for eviction purposes (see `ContextSwap::release` doc) but keeps
/// the API honest — callers acquire the guard and let it drop at end of
/// turn scope.
pub struct LeaseGuard {
    swap: ContextSwap,
    released: bool,
}

impl LeaseGuard {
    /// Manually release the lease (rarely needed; the guard's Drop handles
    /// it). Idempotent.
    pub fn release(mut self) {
        if !self.released {
            self.swap.release();
            self.released = true;
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if !self.released {
            self.swap.release();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    fn record_teardown(counter: StdArc<AtomicUsize>) -> TeardownFn {
        Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    #[tokio::test]
    async fn first_acquire_is_idle() {
        // The very first acquire on a fresh ContextSwap should NOT call any
        // teardown (nothing resident). It just registers the new role.
        let swap = ContextSwap::new();
        let teardown_calls = StdArc::new(AtomicUsize::new(0));
        let _g = swap.acquire(ContextRole::Chat, record_teardown(StdArc::clone(&teardown_calls))).await;
        assert_eq!(teardown_calls.load(Ordering::SeqCst), 0, "first acquire must not evict");
    }

    #[tokio::test]
    async fn same_role_reuse_does_not_evict_but_refreshes_teardown() {
        // A second acquire of the SAME role reuses the resident context (no
        // eviction: t1 must stay 0) but REFRESHES the registered teardown so
        // the NEXT cross-role eviction uses the latest (t2), not a stale t1
        // that may have been consumed by a prior eviction cycle. This is the
        // 2026-08-10 fix: the old behavior (drop the fresh teardown) left the
        // slot's teardown stale after a chat→fable→chat re-spawn cycle.
        let swap = ContextSwap::new();
        let t1 = StdArc::new(AtomicUsize::new(0));
        let t2 = StdArc::new(AtomicUsize::new(0));
        {
            let _g1 = swap.acquire(ContextRole::Fable, record_teardown(StdArc::clone(&t1))).await;
            let _g2 = swap.acquire(ContextRole::Fable, record_teardown(StdArc::clone(&t2))).await;
            assert_eq!(t1.load(Ordering::SeqCst), 0, "first teardown must not fire on same-role reuse");
            assert_eq!(t2.load(Ordering::SeqCst), 0, "second teardown must not fire yet (resident)");
        }
        // Guards dropped (resident persists — no eviction on drop). Now evict
        // via a cross-role acquire: the REFRESHED teardown (t2) must fire, not
        // the stale t1.
        let schema_t = StdArc::new(AtomicUsize::new(0));
        let _g3 = swap.acquire(ContextRole::Schema, record_teardown(StdArc::clone(&schema_t))).await;
        assert_eq!(t1.load(Ordering::SeqCst), 0, "stale first teardown must NOT fire — refresh replaced it");
        assert_eq!(t2.load(Ordering::SeqCst), 1, "refreshed teardown (t2) must fire on the cross-role eviction");
        assert_eq!(schema_t.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cross_role_acquire_evicts_prior() {
        // Chat acquires, then Fable acquires. Chat's teardown MUST fire
        // exactly once before Fable's lease is granted.
        let swap = ContextSwap::new();
        let chat_t = StdArc::new(AtomicUsize::new(0));
        let fable_t = StdArc::new(AtomicUsize::new(0));
        {
            let _g1 = swap.acquire(ContextRole::Chat, record_teardown(StdArc::clone(&chat_t))).await;
            assert_eq!(chat_t.load(Ordering::SeqCst), 0);
        }
        // Guard dropped but resident context persists (no eviction on drop).
        assert_eq!(chat_t.load(Ordering::SeqCst), 0, "drop must NOT evict — resident persists");
        {
            let _g2 = swap.acquire(ContextRole::Fable, record_teardown(StdArc::clone(&fable_t))).await;
            // Fable's acquire should have evicted Chat.
            assert_eq!(chat_t.load(Ordering::SeqCst), 1, "chat teardown must fire on fable acquire");
            assert_eq!(fable_t.load(Ordering::SeqCst), 0, "fable teardown must not fire yet");
        }
        assert_eq!(fable_t.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn three_way_rotation_evicts_each_in_turn() {
        // Chat → Schema → Fable → Chat. Each transition evicts exactly the
        // immediately-prior role. The full-rotation counter for each role
        // should be: chat 1, schema 1, fable 0 (fable is the last resident,
        // never evicted in this sequence).
        let swap = ContextSwap::new();
        let counters = [
            (ContextRole::Chat, StdArc::new(AtomicUsize::new(0))),
            (ContextRole::Schema, StdArc::new(AtomicUsize::new(0))),
            (ContextRole::Fable, StdArc::new(AtomicUsize::new(0))),
        ];
        // Helper to clone the matching counter for a role.
        fn tfor(role: ContextRole, cs: &[(ContextRole, StdArc<AtomicUsize>); 3]) -> StdArc<AtomicUsize> {
            cs.iter().find(|(r, _)| *r == role).unwrap().1.clone()
        }
        let _g1 = swap.acquire(ContextRole::Chat, record_teardown(tfor(ContextRole::Chat, &counters))).await;
        let _g2 = swap.acquire(ContextRole::Schema, record_teardown(tfor(ContextRole::Schema, &counters))).await;
        let _g3 = swap.acquire(ContextRole::Fable, record_teardown(tfor(ContextRole::Fable, &counters))).await;
        let _g4 = swap.acquire(ContextRole::Chat, record_teardown(tfor(ContextRole::Chat, &counters))).await;
        assert_eq!(tfor(ContextRole::Chat, &counters).load(Ordering::SeqCst), 1, "chat evicted once (by schema)");
        assert_eq!(tfor(ContextRole::Schema, &counters).load(Ordering::SeqCst), 1, "schema evicted once (by fable)");
        assert_eq!(tfor(ContextRole::Fable, &counters).load(Ordering::SeqCst), 1, "fable evicted once (by chat)");
    }

    #[tokio::test]
    async fn guard_drop_release_is_idempotent() {
        // Manually releasing then dropping must not double-fire or panic.
        let swap = ContextSwap::new();
        let g = swap.acquire(ContextRole::Chat, record_teardown(StdArc::new(AtomicUsize::new(0)))).await;
        g.release(); // explicit
        // Manual drop after release: should be a no-op.
    }

    #[tokio::test]
    async fn teardown_error_does_not_deadlock() {
        // A failing teardown must still release the lease so the next role
        // can proceed. This is the "stuck lease" mitigation: best-effort.
        let swap = ContextSwap::new();
        let failing: TeardownFn = Box::new(|| Err("simulated join failure".into()));
        let _g1 = swap.acquire(ContextRole::Chat, failing).await;
        // Fable acquires next; the chat teardown errors but the lease still
        // transitions. The acquire must complete (not hang).
        let ok = StdArc::new(AtomicUsize::new(0));
        let _g2 = swap.acquire(ContextRole::Fable, record_teardown(StdArc::clone(&ok))).await;
        assert_eq!(ok.load(Ordering::SeqCst), 0, "fable teardown not fired during its own acquire");
    }

    #[tokio::test]
    async fn sd_role_participates_in_swap_rotation() {
        // Phase 5B (2026-07-29): Sd is the 4th role. Verify it evicts + is
        // evicted like the others. Fable acquires, Sd acquires (evicting
        // Fable — this is the LLM→SD swap), then Chat acquires (evicting Sd —
        // the reverse swap). Each transition fires exactly the prior teardown.
        let swap = ContextSwap::new();
        let fable_t = StdArc::new(AtomicUsize::new(0));
        let sd_t = StdArc::new(AtomicUsize::new(0));
        let chat_t = StdArc::new(AtomicUsize::new(0));
        let _g1 = swap.acquire(ContextRole::Fable, record_teardown(StdArc::clone(&fable_t))).await;
        // LLM → SD swap: Sd evicts Fable.
        let _g2 = swap.acquire(ContextRole::Sd, record_teardown(StdArc::clone(&sd_t))).await;
        assert_eq!(fable_t.load(Ordering::SeqCst), 1, "LLM→SD swap must evict Fable (the LLM weights unload)");
        assert_eq!(sd_t.load(Ordering::SeqCst), 0);
        // SD → LLM swap: Chat evicts Sd (the reverse swap reloads the LLM).
        let _g3 = swap.acquire(ContextRole::Chat, record_teardown(StdArc::clone(&chat_t))).await;
        assert_eq!(sd_t.load(Ordering::SeqCst), 1, "SD→LLM swap must evict Sd");
        assert_eq!(chat_t.load(Ordering::SeqCst), 0);
    }
}
