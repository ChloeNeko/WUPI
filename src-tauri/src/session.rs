use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::schema::WorldSchema;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub reasoning: String,
    /// The raw model output (pre-parse) for assistant turns. Used by the
    /// chat formatter to re-render the turn so the rendered token sequence
    /// matches the KV cache exactly: preserving delta-prefill across turns
    /// (Bug #3: Prefix Cache Extinction). Empty for user/system messages and
    /// legacy sessions; `render_prompt` falls back to `strip_thinking` then.
    #[serde(default)]
    pub raw_output: String,
    pub timestamp: i64,
    /// Swipeable reroll variants (the SillyTavern-style "1/N" UX, 2026-07-29).
    ///
    /// Holds EVERY variant of this message, INCLUDING the active one, indexed
    /// directly by `active_idx`. `content`/`raw_output` are kept as a
    /// denormalized mirror of `variants[active_idx]`/`raw_outputs[active_idx]`
    /// so the rest of the codebase (context assembly, the chat formatter) can
    /// keep reading `.content` for free — only these helpers ever touch the
    /// `variants` Vec. `variant_count() == variants.len()` (always ≥ 1).
    ///
    /// Backwards-compatible: a legacy save with only `content` (no `variants`)
    /// deserializes via `normalize_variants` into a single-element `variants`
    /// (a copy of `content`) + `active_idx` 0, so old saves behave identically.
    ///
    /// Only `content` ever reaches inference — `assemble_api_messages*` reads
    /// `.content` exclusively — so the variant list is pure UI/persistence
    /// state with zero model-context cost.
    #[serde(default)]
    pub variants: Vec<String>,
    /// Parallel to `variants`: the raw model output for each variant, so KV-
    /// cache-coherent re-render (Bug #3) still works after a swipe to an older
    /// variant. `variants[i]` corresponds to `raw_outputs[i]`.
    #[serde(default)]
    pub raw_outputs: Vec<String>,
    /// Which variant is active — a DIRECT index into `variants`. Default 0.
    /// Clamped on load by `normalize_variants`.
    #[serde(default)]
    pub active_idx: usize,
    /// The pre-turn world schema (the state of the world the player acted
    /// against). Captured once on the first variant's generation + reused
    /// across rerolls of this turn so each reroll can REVERT to it before
    /// re-tracking (the variant↔schema binding's double-mutation fix, 2026-08-
    /// 11). `None` on user/system messages + legacy assistant messages — a
    /// reroll of a legacy message has nothing to revert to, so it re-tracks on
    /// top (the old behavior). Only assistant turns ever carry this.
    #[serde(default)]
    pub base_schema: Option<WorldSchema>,
    /// Parallel to `variants`: the post-tracker world schema each roll
    /// produced. `variant_schemas[i]` is installed as the live schema when the
    /// user swipes to variant `i`, so the world state always matches the
    /// displayed prose with ZERO re-tracking (the local model never re-runs on
    /// a swipe). Empty for user/system messages + legacy assistant saves; swipe
    /// falls back to a graceful no-op when the target entry is absent.
    /// Seeded lazily — on the first reroll of a legacy turn, or at creation for
    /// turns generated after this feature shipped.
    #[serde(default)]
    pub variant_schemas: Vec<WorldSchema>,
    /// (2026-08-16 audit H3) WIRE-ONLY pool reference for `base_schema` —
    /// written by the save path's schema-pool transform, resolved + cleared
    /// by `hydrate_schema_refs` on load. Never populated in memory outside
    /// deserialization; never read by any runtime code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_schema_ref: Option<usize>,
    /// (2026-08-16 audit H3) WIRE-ONLY pool references for `variant_schemas`
    /// — same contract as `base_schema_ref`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variant_schema_refs: Vec<usize>,
}

impl Message {
    /// Total number of variants. This is the `N` in the UI's `1/N`. A freshly-
    /// constructed message has an empty `variants` Vec (the constructors don't
    /// seed a redundant copy) but is implicitly single-variant — `content`
    /// itself is the one variant — so this returns `max(1, variants.len())`.
    /// After the first `push_variant` (a reroll) or after `normalize_variants`
    /// (on load), `variants` carries every variant including the active one.
    pub fn variant_count(&self) -> usize {
        self.variants.len().max(1)
    }

    /// The text of variant `i`. For a fresh single-variant message (empty
    /// `variants`), `i == 0` returns the active `content`. Otherwise `i` is a
    /// direct index into `variants`. Returns `None` if out of range.
    pub fn variant_at(&self, i: usize) -> Option<&str> {
        if self.variants.is_empty() {
            return if i == 0 { Some(&self.content) } else { None };
        }
        self.variants.get(i).map(String::as_str)
    }

    /// The raw_output for variant `i` (parallel to `variant_at`).
    pub fn raw_at(&self, i: usize) -> Option<&str> {
        if self.raw_outputs.is_empty() {
            return if i == 0 { Some(&self.raw_output) } else { None };
        }
        self.raw_outputs.get(i).map(String::as_str)
    }

    /// Append a freshly-generated variant + make it the active one. Used by
    /// the reroll flow: the prior active content survives as a sibling, the
    /// new text becomes active. Mirrors `content`/`raw_output` to the new tail.
    /// For a fresh message (empty `variants`) the prior `content`/`raw_output`
    /// are seeded as variant 0 first so nothing is orphaned. Does NOT touch
    /// `variant_schemas` (no schema in hand) — use [`push_variant_with_schema`]
    /// for the variant↔schema binding path.
    pub fn push_variant(&mut self, new_content: String, new_raw: String) {
        self.push_variant_with_schema(new_content, new_raw, None);
    }

    /// Same as `push_variant` but also appends `new_schema` to `variant_schemas`
    /// (aligned with the just-pushed variant). The caller MUST have pre-seeded
    /// `variant_schemas` for the implicit variant 0 when this is the first
    /// reroll of the turn (the fable_send reroll path does so by capturing the
    /// prior live schema before reverting). If `new_schema` is `None`, the
    /// schema list is left untouched (kept aligned only if it already was).
    pub fn push_variant_with_schema(
        &mut self,
        new_content: String,
        new_raw: String,
        new_schema: Option<WorldSchema>,
    ) {
        if self.variants.is_empty() {
            self.variants.push(self.content.clone());
            self.raw_outputs.push(self.raw_output.clone());
        }
        self.variants.push(new_content.clone());
        self.raw_outputs.push(new_raw.clone());
        if let Some(s) = new_schema {
            self.variant_schemas.push(s);
        }
        self.content = new_content;
        self.raw_output = new_raw;
        self.active_idx = self.variants.len() - 1;
    }

    /// Make variant `i` the active one + re-mirror `content`/`raw_output` to
    /// it. No-op if `i == active_idx` or out of range. Used by swipe-left/right.
    pub fn select_variant(&mut self, i: usize) {
        if i == self.active_idx || i >= self.variant_count() {
            return;
        }
        // A fresh message with empty variants has only the implicit content
        // variant (index 0); selecting anything else is out of range (no-op).
        if self.variants.is_empty() {
            return;
        }
        self.active_idx = i;
        self.content = self.variants[i].clone();
        self.raw_output = self.raw_outputs.get(i).cloned().unwrap_or_default();
    }

    /// Sanitize on construction: ensure `variants`/`raw_outputs` are non-empty
    /// + length-aligned, seed them from `content`/`raw_output` for a legacy
    /// single-content save, and clamp `active_idx` into range. Defensive only
    /// — the accessors are already range-safe — but keeps hand-edited or
    /// future-different saves from panicking.
    pub(crate) fn normalize_variants(&mut self) {
        // Legacy save: no variants field → seed a single-element list from the
        // active content so the rest of the model can assume ≥1 variant.
        if self.variants.is_empty() {
            self.variants.push(self.content.clone());
            self.raw_outputs.push(self.raw_output.clone());
            self.active_idx = 0;
            return;
        }
        if self.raw_outputs.len() < self.variants.len() {
            self.raw_outputs.resize(self.variants.len(), String::new());
        } else if self.raw_outputs.len() > self.variants.len() {
            self.raw_outputs.truncate(self.variants.len());
        }
        if self.active_idx >= self.variants.len() {
            self.active_idx = 0;
        }
        // (2026-08-16 audit LOW) `variant_schemas` may legitimately be
        // SHORTER than `variants` (lazy seeding — accessors `.get()` and
        // degrade gracefully), but LONGER is a hand-edit artifact whose tail
        // entries can install a deleted variant's world state on a swipe.
        // Truncate to the variant count.
        if self.variant_schemas.len() > self.variants.len() {
            self.variant_schemas.truncate(self.variants.len());
        }
        // Keep the mirror honest (a hand-edit could have changed variants but
        // not content). The active variant is the source of truth here.
        if self.content != self.variants[self.active_idx] {
            self.content = self.variants[self.active_idx].clone();
        }
        // (2026-08-16 audit LOW) Same mirror discipline for `raw_output` —
        // a hand-edited variants list left the stale active raw in place,
        // desyncing KV-coherent re-render (Bug #3's invariant).
        if let Some(active_raw) = self.raw_outputs.get(self.active_idx) {
            if &self.raw_output != active_raw {
                self.raw_output = active_raw.clone();
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
    /// (2026-08-16 audit H3) WIRE-ONLY deduplicated schema pool written by
    /// the save path (`pool_session_schemas`) + resolved into the per-message
    /// inline fields by `hydrate_schema_refs` immediately after load. Always
    /// empty in live memory. Not serialized by the derived `Serialize` (the
    /// pool transform injects it at the `serde_json::Value` level) so the
    /// in-memory clone path never carries it.
    #[serde(default, skip_serializing)]
    pub schema_pool: Vec<WorldSchema>,
}

impl Conversation {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            schema_pool: Vec::new(),
        }
    }

    pub fn add_message(&mut self, role: Role, content: String) -> &Message {
        self.add_message_with_reasoning(role, content, String::new())
    }

    /// Add a message with an explicit reasoning (thought-channel) payload.
    /// Used for assistant turns where the model emitted a `<|channel>thought`
    /// block that we parsed out of the raw output.
    pub fn add_message_with_reasoning(
        &mut self,
        role: Role,
        content: String,
        reasoning: String,
    ) -> &Message {
        let msg = Message {
            id: gen_id(),
            role,
            content,
            reasoning,
            raw_output: String::new(),
            timestamp: chrono_now_millis(),
            variants: Vec::new(),
            raw_outputs: Vec::new(),
            active_idx: 0,
            base_schema: None,
            variant_schemas: Vec::new(),
            base_schema_ref: None,
            variant_schema_refs: Vec::new(),
        };
        self.messages.push(msg);
        self.messages.last().expect("just pushed")
    }

    /// Add an assistant turn with the raw model output alongside the cleaned
    /// content + reasoning. The raw output is what the chat formatter
    /// re-renders from so the token sequence matches the KV cache (Bug #3).
    pub fn add_assistant_turn(
        &mut self,
        content: String,
        reasoning: String,
        raw_output: String,
    ) -> &Message {
        let msg = Message {
            id: gen_id(),
            role: Role::Assistant,
            content,
            reasoning,
            raw_output,
            timestamp: chrono_now_millis(),
            variants: Vec::new(),
            raw_outputs: Vec::new(),
            active_idx: 0,
            base_schema: None,
            variant_schemas: Vec::new(),
            base_schema_ref: None,
            variant_schema_refs: Vec::new(),
        };
        self.messages.push(msg);
        self.messages.last().expect("just pushed")
    }

    /// True iff the most recent message is a user turn. Used by the error
    /// rollback in `chat_send` (Bug C fix, 2026-07-12) to decide whether the
    /// just-added user message should be popped when the backend errored
    /// before producing any assistant reply.
    pub fn last_message_is_user(&self) -> bool {
        self.messages.last().map(|m| m.role == Role::User).unwrap_or(false)
    }

    /// Remove the most recent message, if any. Used to roll back an orphaned
    /// user message when a generation fails (Bug C fix, 2026-07-12) so that
    /// session.json matches the actual visible conversation and the next
    /// send doesn't render two consecutive user turns.
    pub fn pop_last_message(&mut self) {
        self.messages.pop();
    }

    /// Remove the message at `index` + return it. Subsequent messages shift
    /// down by one. Used by the model-facing `fable_message_delete` tool (the
    /// chat-side stateful tool dispatched from `run_agent_loop`). Bounds-
    /// checked; returns `Err` with a model-friendly message otherwise.
    /// Mirrors the bounds-error shape `apply_edit` / `apply_rewind_and_edit`
    /// emit so the agent loop's `invalid args: …` path reads consistently.
    pub fn remove_at(&mut self, index: usize) -> Result<Message, String> {
        let len = self.messages.len();
        if index >= len {
            return Err(format!(
                "remove_at: index {index} out of bounds (len {len})"
            ));
        }
        Ok(self.messages.remove(index))
    }

    /// (2026-08-16 audit H3) Resolve the wire-only schema pool into the
    /// per-message inline fields. Idempotent + a no-op on the legacy inline
    /// format (empty pool). Out-of-range refs degrade to `None`/skipped —
    /// the same `.get()` fallback discipline the accessors use for
    /// hand-edited saves.
    pub fn hydrate_schema_refs(&mut self) {
        if self.schema_pool.is_empty() {
            return;
        }
        for m in &mut self.messages {
            if let Some(idx) = m.base_schema_ref.take() {
                m.base_schema = self.schema_pool.get(idx).cloned();
            }
            if !m.variant_schema_refs.is_empty() {
                m.variant_schemas = m
                    .variant_schema_refs
                    .iter()
                    .filter_map(|i| self.schema_pool.get(*i).cloned())
                    .collect();
                m.variant_schema_refs.clear();
            }
        }
        self.schema_pool.clear();
    }

    /// Persist the conversation **atomically**: serialize, write to a sibling
    /// temp file, then `rename` it over the destination.
    ///
    /// Atomicity matters because `save()` runs on every message: a plain
    /// `fs::write(path, ...)` truncates-then-writes, so a crash / power loss /
    /// disk-full mid-write leaves `session.json` truncated and the ENTIRE
    /// conversation unrecoverable. The temp+rename pattern guarantees the
    /// destination is either the previous complete file or the new complete
    /// file: never a half-written middle state.
    ///
    /// - The temp file is in the same directory as `path` (same volume →
    ///   `rename` is atomic; a cross-device rename would degrade to copy+delete
    ///   and lose the atomicity guarantee).
    /// - On Windows, `std::fs::rename` over an existing file uses
    ///   `MOVEFILE_REPLACE_EXISTING`, so the overwrite is atomic there too.
    /// - A stale `.tmp` from a prior crashed save is removed first so we never
    ///   accidentally rename a leftover corrupt temp over the good file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        // (2026-08-15 audit fix) Byte-stable saves: route through `to_value`
        // FIRST — serde_json's Map is BTreeMap-backed (the crate has no
        // `preserve_order` feature), so every HashMap (schema entities,
        // custom_tags, …) lands in sorted key order instead of per-process
        // hash order. The same logical state now produces identical bytes
        // across boots (save diffs stop being hash-order noise). f32 fields
        // serialize via their exact f64 widening — round-trip identical.
        let mut value = serde_json::to_value(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        // (2026-08-16 audit H3) Collapse the per-message inline schemas into
        // a dedup pool on the wire — see `pool_session_schemas`.
        pool_session_schemas(&mut value);
        let json = serde_json::to_string_pretty(&value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Temp file: sibling of the destination, same directory/volume,
        // UNIQUE per write (2026-08-16 audit fix #13 — see temp_path_for).
        let tmp_path = temp_path_for(path);

        // Write + flush the temp so the bytes are durable before the rename.
        // Without fsync, a crash after rename could expose an empty/journaled
        // file once the OS writeback catches up.
        {
            let mut file = std::fs::File::create(&tmp_path)?;
            std::io::Write::write_all(&mut file, json.as_bytes())?;
            std::io::Write::flush(&mut file)?;
            // Sync the file's data to disk. AllDataSync because metadata
            // (size) for an existing file is cheap; for the rename we only
            // truly need the data, but AllDataSync is the safer choice and
            // the perf cost is one extra syscall on a tiny JSON file.
            let _ = file.sync_all();
        }

        // Atomic replace. On Windows this uses MOVEFILE_REPLACE_EXISTING.
        // (audit #13) A failure past this point must never leave THIS write's
        // temp behind as grime — remove it before propagating.
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        Ok(())
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut conv: Self = serde_json::from_str(&text)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                // Resolve the wire-only schema pool (audit H3) into the
                // inline fields FIRST, then the defensive normalizations.
                conv.hydrate_schema_refs();
                // Defensive: clamp active_idx + reconcile variant/raw lengths
                // so a hand-edited or skewed save can't panic the accessors.
                for m in &mut conv.messages {
                    m.normalize_variants();
                }
                Ok(conv)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(e) => Err(e),
        }
    }

    pub fn assemble_api_messages(&self, system_prompt: &str) -> Vec<ApiMessage> {
        let mut out = Vec::with_capacity(self.messages.len() + 1);
        if !system_prompt.is_empty() {
            out.push(ApiMessage {
                role: "system".into(),
                content: system_prompt.into(),
                raw_output: String::new(),
            });
        }
        for m in &self.messages {
            out.push(ApiMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                }
                .into(),
                content: m.content.clone(),
                raw_output: m.raw_output.clone(),
            });
        }
        out
    }

    /// Windowed variant of [`assemble_api_messages`]: prepends the system
    /// message in full, then takes only the LAST `window` stored messages.
    ///
    /// This is the §2F eager-prefill sliding window (2026-07-13). Capping
    /// visible history to a fixed message count (regardless of token budget)
    /// does two things:
    /// 1. Makes `truncate_to_fit` effectively never fire (4 short turns +
    ///    system ≪ the ~3000-token budget), eliminating truncation-driven
    ///    cold-resets.
    /// 2. Keeps the stable prefix short and predictable so eager prefill is
    ///    cheap and the delta (memory block + new user) stays small.
    ///
    /// Memory (M) is supposed to backfill the evicted older turns via
    /// retrieval: that's the whole point of the offload. If retrieval misses,
    /// the model genuinely sees less recency than before; the cap is in a
    /// `const` at the call site, trivially tunable.
    ///
    /// **Alternating roll-up (2026-07-17):** before returning, consecutive
    /// same-role messages are merged into one block (content joined with
    /// `\n\n`, `raw_output` joined with `\n` for assistant turns). This
    /// guarantees a clean user↔assistant alternation for the downstream
    /// backend: GLM gets the strictly-alternating payload its chat template
    /// expects, and local Gemma 4 never emits adjacent `<|turn>user` or
    /// `<|turn>model` blocks that would confuse the chat-template tracking.
    /// The roll-up is a pure normalization on the assembled slice; stored
    /// session state is untouched. See `normalize_alternating`.
    pub fn assemble_api_messages_windowed(
        &self,
        system_prompt: &str,
        window: usize,
    ) -> Vec<ApiMessage> {
        let start = self.messages.len().saturating_sub(window);
        let visible = &self.messages[start..];

        let mut out = Vec::with_capacity(visible.len() + 1);
        if !system_prompt.is_empty() {
            out.push(ApiMessage {
                role: "system".into(),
                content: system_prompt.into(),
                raw_output: String::new(),
            });
        }
        for m in visible {
            out.push(ApiMessage {
                role: match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::System => "system",
                }
                .into(),
                content: m.content.clone(),
                raw_output: m.raw_output.clone(),
            });
        }
        normalize_alternating(out)
    }
}

/// Merge consecutive same-role messages into single blocks. Applied to the
/// assembled `Vec<ApiMessage>` before it leaves `assemble_api_messages_windowed`,
/// so BOTH backends (local Gemma 4 via `Gemma4Format::render_prompt`, online
/// GLM via `HttpBackend::stream`) receive the same strictly-alternating
/// payload. Session storage is left untouched: this is a presentation-layer
/// transform only.
///
/// Rules:
/// - Walk the slice; if message `i` and `i+1` share a role, their `content`
///   strings are joined with `\n\n` and their `raw_output` strings with `\n`.
///   The pair collapses into one entry; the walk continues from the merged
///   entry so runs of 3+ same-role messages fold fully.
/// - The system block (index 0) participates: it never has a same-role
///   neighbor in practice (the conversation starts with user), but if a
///   legacy/hand-edited session had a leading system+system pair it would
///   merge cleanly rather than emit two system turns.
/// - `raw_output` is merged too because the local formatter renders assistant
///   turns from `raw_output` when present (Bug #3, §2C): joining only
///   `content` would desync the rendered tokens from the KV cache. Assistant
///   `raw_output` blocks are the Gemma4 channel protocol; joining with `\n`
///   (not `\n\n`) keeps the turn boundary inside the merged block legible.
///
/// Empty messages are NOT dropped: an empty user turn is still a turn (the
/// backend's alternation contract doesn't care about content length).
pub fn normalize_alternating(messages: Vec<ApiMessage>) -> Vec<ApiMessage> {
    if messages.len() < 2 {
        return messages;
    }
    let mut out: Vec<ApiMessage> = Vec::with_capacity(messages.len());
    for m in messages {
        if let Some(last) = out.last_mut() {
            if last.role == m.role {
                // Same-role neighbor: roll up into the last block.
                if !last.content.is_empty() && !m.content.is_empty() {
                    last.content.push_str("\n\n");
                }
                last.content.push_str(&m.content);
                if !last.raw_output.is_empty() && !m.raw_output.is_empty() {
                    last.raw_output.push('\n');
                }
                last.raw_output.push_str(&m.raw_output);
                continue;
            }
        }
        out.push(m);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiMessage {
    pub role: String,
    pub content: String,
    /// Raw model output for assistant turns. The formatter renders model
    /// turns from this (when present) so the rendered tokens match the KV
    /// cache exactly. Empty for non-assistant turns. See Bug #3.
    #[serde(default)]
    pub raw_output: String,
}

/// (2026-08-16 audit H3) Collapse the per-message inline `base_schema` /
/// `variant_schemas` of a serialized `Conversation` value into ONE
/// deduplicated `schema_pool` array + per-message integer refs.
///
/// Why: every assistant `Message` serialized its schemas inline — a mature
/// schema is tens of KB and a long campaign embeds 2-3 copies per turn
/// (base + every variant roll), so `session.json` AND every save slot grew
/// O(messages × full schema clones) into multi-MB files the UI fully
/// re-parses (`list_saves`, the title Continue walk). Consecutive turns
/// dedup heavily: turn N+1's `base_schema` IS turn N's active
/// `variant_schemas` entry, and reroll variants share their base — the
/// pool stores each unique schema exactly once.
///
/// Wire contract (back-compatible BOTH directions):
/// - A message with a pooled write carries `base_schema_ref: <pool idx>` /
///   `variant_schema_refs: [<idx>, …]` and NO inline schema keys.
/// - The conversation object carries `schema_pool: [...]` only when
///   non-empty.
/// - The legacy inline shape still deserializes (the ref fields default);
///   `hydrate_schema_refs` is a no-op without a pool.
///
/// Operates at the `serde_json::Value` level on PURPOSE: both save paths
/// already route through `to_value` for byte-stable key ordering, the
/// in-memory types stay untouched, and the dedup keys inherit that same
/// deterministic serialization (logically identical schemas always hash to
/// the same pool entry).
pub(crate) fn pool_session_schemas(session: &mut serde_json::Value) {
    let Some(obj) = session.as_object_mut() else {
        return;
    };
    let Some(messages) = obj
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let mut pool: Vec<serde_json::Value> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for msg in messages.iter_mut() {
        let Some(m) = msg.as_object_mut() else {
            continue;
        };
        if let Some(base) = m.remove("base_schema") {
            if !base.is_null() {
                let idx = intern_schema(&mut pool, &mut index, base);
                m.insert("base_schema_ref".to_string(), serde_json::Value::from(idx));
            }
        }
        if let Some(variants) = m.remove("variant_schemas") {
            if let Some(arr) = variants.as_array() {
                if !arr.is_empty() {
                    let refs: Vec<serde_json::Value> = arr
                        .iter()
                        .cloned()
                        .map(|v| serde_json::Value::from(intern_schema(&mut pool, &mut index, v)))
                        .collect();
                    m.insert("variant_schema_refs".to_string(), serde_json::Value::Array(refs));
                }
            }
        }
    }
    if !pool.is_empty() {
        obj.insert("schema_pool".to_string(), serde_json::Value::Array(pool));
    }
}

/// Intern one schema value into the dedup pool, returning its stable index.
fn intern_schema(
    pool: &mut Vec<serde_json::Value>,
    index: &mut std::collections::HashMap<String, usize>,
    value: serde_json::Value,
) -> usize {
    let key = serde_json::to_string(&value).unwrap_or_default();
    if let Some(&existing) = index.get(&key) {
        return existing;
    }
    let idx = pool.len();
    pool.push(value);
    index.insert(key, idx);
    idx
}

/// Build a sibling temp-file path for an atomic save: same directory + volume
/// as `path` (so `rename` is atomic), with a UNIQUE per-write
/// `<pid>.<counter>.tmp` suffix.
///
/// (2026-08-16 audit fix #13) `session.json` → `session.json.<pid>.<n>.tmp`
/// — the same per-write uniqueness the save slots got (#59). The old fixed
/// `session.json.tmp` let a writer racing another writer on the same file
/// share one temp (remove-then-create interleave → corrupt/lost history).
/// A crashed writer's stale temp is inert grime: nothing loads it, and the
/// next save stages its OWN fresh temp.
fn temp_path_for(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from("wupi.tmp"));
    name.push(".");
    name.push(crate::fable_save::unique_tmp_suffix());
    path.with_file_name(name)
}

fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut bytes = [0u8; 6];
    prng_fill(&mut bytes);
    let rand: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    format!("m_{:x}_{}", ts, &rand[..6])
}

/// Fill `buf` with pseudo-random bytes via a xorshift64 seeded from wall-clock
/// nanos. NOT cryptographic: used only for message-ID uniqueness in local
/// chat (see `gen_id`). The name reflects what it is: a PRNG, not the OS
/// CSPRNG (the old name `getrandom_fill` falsely implied the `getrandom`
/// syscall / crate). Renamed 2026-07-13 (Gemini review).
fn prng_fill(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut x = nanos as u64;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
}

fn chrono_now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique path in the temp dir, namespaced by pid + a counter so parallel
    /// test runs and repeated invocations don't collide. Avoids pulling in the
    /// `tempfile` crate for what is a one-line unique-name need.
    fn unique_test_path(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "wupi_test_{}{}_{}_{}.json",
            std::process::id(),
            ts,
            n,
            name
        ))
    }

    /// Build a throwaway conversation with one message for round-trip tests.
    fn sample_conv() -> Conversation {
        let mut c = Conversation::new();
        c.add_message(Role::User, "hello".into());
        c
    }

    /// `remove_at` (2026-08-11): the missing structural primitive the
    /// `fable_message_delete` tool needs. Shifts subsequent messages down + is
    /// bounds-checked. Mirrors the bounds-error shape the lib.rs appliers emit.
    #[test]
    fn remove_at_drops_message_and_shifts_tail() {
        let mut c = Conversation::new();
        c.add_message(Role::User, "first".into());
        c.add_message(Role::Assistant, "second".into());
        c.add_message(Role::User, "third".into());
        // Sanity: 3 messages, indexes 0..2.
        assert_eq!(c.messages.len(), 3);
        // Drop index 1 (the assistant turn).
        let removed = c.remove_at(1).expect("index 1 in bounds");
        assert_eq!(removed.content, "second");
        assert_eq!(c.messages.len(), 2, "must shrink by 1");
        // Subsequent messages shift down: old index 2 ("third") is now at 1.
        assert_eq!(c.messages[0].content, "first");
        assert_eq!(c.messages[1].content, "third");
    }

    #[test]
    fn remove_at_out_of_bounds_errors() {
        let mut c = Conversation::new();
        c.add_message(Role::User, "only".into());
        let err = c.remove_at(5).expect_err("index 5 out of bounds");
        assert!(err.contains("out of bounds"), "error: {err}");
        // Index 0 still works.
        c.remove_at(0).expect("index 0 in bounds");
        // Now empty — even index 0 is out of bounds.
        let err = c.remove_at(0).expect_err("empty → 0 is OOB");
        assert!(err.contains("out of bounds"), "error: {err}");
    }

    /// Every `<name>.*.tmp` sibling (the unique per-write temps, 2026-08-16
    /// audit fix #13). Empty = no leftover.
    fn sibling_temps(path: &Path) -> Vec<std::path::PathBuf> {
        let Some(dir) = path.parent() else { return Vec::new(); };
        let Some(stem) = path.file_name().and_then(|s| s.to_str()) else { return Vec::new(); };
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.starts_with(stem) && name.ends_with(".tmp") {
                    out.push(e.path());
                }
            }
        }
        out
    }

    /// Clean up the main file and any sibling unique temps so the temp dir
    /// doesn't accumulate test artifacts. NotFound is fine.
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        for t in sibling_temps(path) {
            let _ = std::fs::remove_file(t);
        }
    }

    #[test]
    fn temp_path_is_sibling_with_unique_tmp_suffix() {
        let p = std::path::PathBuf::from("dir/session.json");
        let tmp = temp_path_for(&p);
        let name = tmp.file_name().and_then(|s| s.to_str()).unwrap();
        // (2026-08-16 audit fix #13) UNIQUE per call: `<name>.<pid>.<n>.tmp`.
        assert!(name.starts_with("session.json."), "got {name}");
        assert!(name.ends_with(".tmp"), "got {name}");
        assert_eq!(tmp.parent(), p.parent(), "sibling dir so rename is atomic");
        // Two calls never collide — the interleave race the fix exists for.
        assert_ne!(temp_path_for(&p), temp_path_for(&p));
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = unique_test_path("roundtrip");
        cleanup(&path);
        let conv = sample_conv();

        conv.save(&path).expect("save should succeed");

        // No temp left behind after a successful save (renamed away).
        assert!(sibling_temps(&path).is_empty(), "temp file left behind");
        // Main file exists and round-trips.
        let loaded = Conversation::load(&path).expect("load should succeed");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content, "hello");

        cleanup(&path);
    }

    #[test]
    fn save_does_not_leave_temp_file_on_success() {
        // Regression guard: if a future refactor drops the rename or makes it
        // non-atomic, this test fails because the .tmp would remain.
        let path = unique_test_path("no_temp_leftover");
        cleanup(&path);
        sample_conv().save(&path).expect("save");
        assert!(path.exists(), "destination must exist");
        assert!(
            sibling_temps(&path).is_empty(),
            "temp file must be renamed away, not left behind"
        );
        cleanup(&path);
    }

    #[test]
    fn save_overwrites_existing_file_in_place() {
        // Save once, then save a second time with different content. The
        // destination must reflect the second save and no temp may remain.
        let path = unique_test_path("overwrite");
        cleanup(&path);

        let mut first = Conversation::new();
        first.add_message(Role::User, "first".into());
        first.save(&path).expect("first save");

        let mut second = Conversation::new();
        second.add_message(Role::User, "second".into());
        second.add_message(Role::User, "third".into());
        second.save(&path).expect("second save");

        let loaded = Conversation::load(&path).expect("load");
        assert_eq!(loaded.messages.len(), 2, "second save should win");
        assert_eq!(loaded.messages[0].content, "second");
        assert!(sibling_temps(&path).is_empty(), "temp leaked after overwrite");

        cleanup(&path);
    }

    #[test]
    fn stale_temp_from_prior_crash_is_inert() {
        // (2026-08-16 audit fix #13) With UNIQUE per-write temps, a crashed
        // writer's stale temp is INERT grime: the next save stages its own
        // fresh temp + renames it over the destination — the stale file is
        // never opened, never renamed, never loaded. (The old fixed-name
        // design removed-then-recreated it; that interleave is the race
        // uniqueness exists to kill.)
        let path = unique_test_path("stale_temp");
        cleanup(&path);

        let mut stale = path.clone().into_os_string();
        stale.push(".999999.99.tmp");
        let stale: std::path::PathBuf = stale.into();
        std::fs::write(&stale, b"stale garbage from a crashed save").expect("seed stale temp");
        assert!(stale.exists(), "precondition: stale temp exists");

        sample_conv().save(&path).expect("save must ignore the stale temp");
        assert!(path.exists(), "destination written");
        Conversation::load(&path)
            .expect("destination is valid (the stale temp never replaced it)");
        // The stale temp survives as accepted, invisible grime.
        assert!(stale.exists(), "stale unique temp is left as inert grime");

        cleanup(&path);
    }

    #[test]
    fn destination_survives_when_only_temp_would_be_corrupt() {
        // The atomicity guarantee: if a write were to fail partway, the
        // DESTINATION must remain the previously-saved good file. We can't
        // easily simulate a mid-write crash, but we CAN prove the invariant
        // indirectly: save a known-good file, then confirm a second save that
        // completes leaves a valid file. The point of the temp+rename design
        // is that the destination is never opened for write directly.
        let path = unique_test_path("atomicity");
        cleanup(&path);

        let mut good = Conversation::new();
        good.add_message(Role::User, "known-good state".into());
        good.save(&path).expect("first save");
        let before = std::fs::read(&path).expect("read good file");

        // Second save with new content.
        sample_conv().save(&path).expect("second save");
        let after = std::fs::read(&path).expect("read new file");

        assert_ne!(before, after, "second save must actually replace content");
        // Both reads must be valid JSON (no partial-write corruption).
        assert!(
            serde_json::from_slice::<Conversation>(&before).is_ok(),
            "pre-overwrite file must be valid"
        );
        assert!(
            serde_json::from_slice::<Conversation>(&after).is_ok(),
            "post-overwrite file must be valid"
        );

        cleanup(&path);
    }

    fn api(role: &str, content: &str) -> ApiMessage {
        ApiMessage {
            role: role.into(),
            content: content.into(),
            raw_output: String::new(),
        }
    }
    fn api_raw(role: &str, content: &str, raw: &str) -> ApiMessage {
        ApiMessage {
            role: role.into(),
            content: content.into(),
            raw_output: raw.into(),
        }
    }

    #[test]
    fn normalize_keeps_already_alternating_unchanged() {
        let msgs = vec![
            api("system", "sys"),
            api("user", "hi"),
            api("assistant", "hello"),
            api("user", "how are you?"),
            api("assistant", "fine"),
        ];
        let out = normalize_alternating(msgs);
        assert_eq!(out.len(), 5);
        assert_eq!(out.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
                   vec!["system", "user", "assistant", "user", "assistant"]);
    }

    #[test]
    fn normalize_merges_consecutive_user_messages() {
        // Simulates the "user clicks Save / fires multiple commands" case:
        // two user turns in a row should collapse into one.
        let msgs = vec![
            api("system", "sys"),
            api("user", "save this"),
            api("user", "and also that"),
            api("assistant", "done"),
        ];
        let out = normalize_alternating(msgs);
        assert_eq!(out.len(), 3, "two user msgs merge into one");
        assert_eq!(out[1].role, "user");
        assert_eq!(out[1].content, "save this\n\nand also that");
        assert_eq!(out[2].role, "assistant");
    }

    #[test]
    fn normalize_merges_consecutive_assistant_messages_with_raw_output() {
        // Cache-coherence (Bug #3): raw_output must be merged alongside
        // content, otherwise the local formatter renders the merged turn
        // from a stale raw_output and desyncs the KV cache.
        let msgs = vec![
            api("user", "q"),
            api_raw("assistant", "part 1", "<raw1>"),
            api_raw("assistant", "part 2", "<raw2>"),
            api("user", "next"),
        ];
        let out = normalize_alternating(msgs);
        assert_eq!(out.len(), 3, "two assistant msgs merge into one");
        assert_eq!(out[1].content, "part 1\n\npart 2");
        assert_eq!(out[1].raw_output, "<raw1>\n<raw2>",
                   "raw_output joined with single \\n (not \\n\\n)");
    }

    #[test]
    fn normalize_folds_runs_of_three_or_more() {
        let msgs = vec![
            api("system", "sys"),
            api("user", "a"),
            api("user", "b"),
            api("user", "c"),
            api("assistant", "reply"),
        ];
        let out = normalize_alternating(msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].content, "a\n\nb\n\nc");
    }

    #[test]
    fn normalize_handles_empty_messages_without_dropping() {
        // An empty user turn is still a turn: don't drop it. When the first
        // of the merged pair is empty, no leading separator is emitted (the
        // \n\n guard fires only when BOTH sides have content).
        let msgs = vec![
            api("user", ""),
            api("user", "real message"),
            api("assistant", "reply"),
        ];
        let out = normalize_alternating(msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content, "real message",
                   "empty-leading case yields just the non-empty content");
        assert_eq!(out[1].content, "reply");
    }

    #[test]
    fn normalize_preserves_system_at_index_zero() {
        // A legacy session with two leading system messages should merge them
        // into the index-0 system block, not emit two system turns.
        let msgs = vec![
            api("system", "directive A"),
            api("system", "directive B"),
            api("user", "hi"),
        ];
        let out = normalize_alternating(msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "system");
        assert_eq!(out[0].content, "directive A\n\ndirective B");
        assert_eq!(out[1].role, "user");
    }

    #[test]
    fn normalize_empty_and_single_pass_through() {
        assert_eq!(normalize_alternating(vec![]).len(), 0);
        let one = vec![api("user", "solo")];
        assert_eq!(normalize_alternating(one).len(), 1);
    }

    // ---- Variant model (swipeable rerolls) ----------------------------------

    fn variant_msg() -> Message {
        Message {
            id: "m_test".into(),
            role: Role::Assistant,
            content: "first".into(),
            reasoning: String::new(),
            raw_output: "<raw1>".into(),
            timestamp: 0,
            variants: Vec::new(),
            raw_outputs: Vec::new(),
            active_idx: 0,
            base_schema: None,
            variant_schemas: Vec::new(),
            base_schema_ref: None,
            variant_schema_refs: Vec::new(),
        }
    }

    #[test]
    fn push_variant_with_schema_keeps_variant_schemas_parallel() {
        use crate::schema::WorldSchema;
        let mk = |summary: &str| {
            let mut s = WorldSchema::default();
            s.summary = summary.into();
            s
        };
        let mut m = variant_msg();
        // fable_send seeds variant 0's schema (the post-tracker snapshot) at
        // creation; the first-reroll pre-Stage-1 block seeds it for legacy turns.
        m.variant_schemas.push(mk("roll0"));
        // First reroll appends variant 1 + its schema.
        m.push_variant_with_schema("second".into(), "<raw2>".into(), Some(mk("roll1")));
        assert_eq!(m.variant_count(), 2);
        assert_eq!(m.variant_schemas.len(), 2, "variant_schemas parallels variants");
        assert_eq!(m.variant_schemas[m.active_idx].summary, "roll1");
        // Second reroll appends variant 2 + its schema.
        m.push_variant_with_schema("third".into(), "<raw3>".into(), Some(mk("roll2")));
        assert_eq!(m.variant_count(), 3);
        assert_eq!(m.variant_schemas.len(), 3);
        assert_eq!(m.variant_schemas[m.active_idx].summary, "roll2");
    }

    #[test]
    fn variant_schema_round_trips_through_select() {
        // The swipe path: select_variant swaps content/active_idx, then the
        // caller installs variant_schemas[variant_idx] as the live schema.
        // Verify the stored schemas survive the select + stay aligned.
        use crate::schema::WorldSchema;
        let mk = |summary: &str| {
            let mut s = WorldSchema::default();
            s.summary = summary.into();
            s
        };
        let mut m = variant_msg();
        m.variant_schemas.push(mk("roll0"));
        m.push_variant_with_schema("second".into(), "<raw2>".into(), Some(mk("roll1")));
        // Swipe back to variant 0.
        assert_eq!(m.variant_schemas.get(0).map(|s| s.summary.as_str()), Some("roll0"));
        m.select_variant(0);
        assert_eq!(m.active_idx, 0);
        // Swipe forward to variant 1.
        assert_eq!(m.variant_schemas.get(1).map(|s| s.summary.as_str()), Some("roll1"));
        m.select_variant(1);
        assert_eq!(m.active_idx, 1);
        // A legacy message with no stored schema falls back to None (graceful).
        let legacy = variant_msg();
        assert!(legacy.variant_schemas.get(0).is_none());
    }

    #[test]
    fn fresh_message_has_one_variant() {
        let m = variant_msg();
        assert_eq!(m.variant_count(), 1);
        assert_eq!(m.variant_at(0), Some("first"));
        assert_eq!(m.variant_at(1), None, "no second variant yet");
        assert_eq!(m.raw_at(0), Some("<raw1>"));
    }

    #[test]
    fn push_variant_keeps_old_as_sibling_and_makes_new_active() {
        let mut m = variant_msg();
        m.push_variant("second".into(), "<raw2>".into());
        // content is now the new one; old is stashed as a sibling.
        assert_eq!(m.content, "second");
        assert_eq!(m.raw_output, "<raw2>");
        assert_eq!(m.active_idx, 1, "new variant becomes #1 (0-indexed)");
        assert_eq!(m.variant_count(), 2);
        // Both variants are retrievable by index.
        assert_eq!(m.variant_at(0), Some("first"), "old text at index 0");
        assert_eq!(m.variant_at(1), Some("second"), "new text at index 1");
        assert_eq!(m.raw_at(0), Some("<raw1>"));
        assert_eq!(m.raw_at(1), Some("<raw2>"));
    }

    #[test]
    fn two_pushes_yield_three_variants() {
        // The reroll-twice case from the plan.
        let mut m = variant_msg();
        m.push_variant("second".into(), "<raw2>".into());
        m.push_variant("third".into(), "<raw3>".into());
        assert_eq!(m.variant_count(), 3);
        assert_eq!(m.content, "third");
        assert_eq!(m.active_idx, 2);
        assert_eq!(m.variant_at(0), Some("first"));
        assert_eq!(m.variant_at(1), Some("second"));
        assert_eq!(m.variant_at(2), Some("third"));
    }

    #[test]
    fn select_variant_swaps_active_with_sibling() {
        let mut m = variant_msg();
        m.push_variant("second".into(), "<raw2>".into());
        // Swipe back to the first variant.
        m.select_variant(0);
        assert_eq!(m.content, "first", "active content is now the old one");
        assert_eq!(m.raw_output, "<raw1>");
        assert_eq!(m.active_idx, 0);
        assert_eq!(m.variant_at(0), Some("first"));
        assert_eq!(m.variant_at(1), Some("second"), "second still retrievable");
        // Swipe forward again.
        m.select_variant(1);
        assert_eq!(m.content, "second");
        assert_eq!(m.active_idx, 1);
    }

    #[test]
    fn select_variant_noop_on_active_or_out_of_range() {
        let mut m = variant_msg();
        m.push_variant("second".into(), "<raw2>".into());
        // Selecting the already-active variant is a no-op.
        m.select_variant(1);
        assert_eq!(m.content, "second");
        assert_eq!(m.active_idx, 1);
        // Out-of-range is a no-op (no panic).
        m.select_variant(99);
        assert_eq!(m.content, "second");
        assert_eq!(m.active_idx, 1);
    }

    #[test]
    fn legacy_save_with_only_content_loads_as_single_variant() {
        // A save written before the variant fields existed: only id/role/
        // content/reasoning/raw_output/timestamp. serde defaults must fill the
        // new fields so the message behaves as a single-variant message.
        let legacy = r#"{
            "id": "m_old",
            "role": "assistant",
            "content": "legacy prose",
            "reasoning": "",
            "raw_output": "<legacy_raw>",
            "timestamp": 123
        }"#;
        let m: Message = serde_json::from_str(legacy).expect("legacy message parses");
        assert_eq!(m.variants, Vec::<String>::new());
        assert_eq!(m.active_idx, 0);
        assert_eq!(m.variant_count(), 1);
        assert_eq!(m.content, "legacy prose");
        assert_eq!(m.variant_at(0), Some("legacy prose"));
    }

    #[test]
    fn normalize_variants_clamps_skewed_save() {
        // A hand-edited save with active_idx beyond range + mismatched lengths.
        // Under the "variants includes active" model: variants=["a","b"] →
        // 2 variants total; the out-of-range active_idx clamps to 0, which
        // re-mirrors content to variants[0]="a" (the active source of truth).
        let mut m = Message {
            id: "x".into(),
            role: Role::Assistant,
            content: "active".into(),
            reasoning: String::new(),
            raw_output: "<active>".into(),
            timestamp: 0,
            variants: vec!["a".into(), "b".into()],
            raw_outputs: vec!["<ra>".into()], // short by one
            active_idx: 99,                    // out of range
            base_schema: None,
            variant_schemas: Vec::new(),
            base_schema_ref: None,
            variant_schema_refs: Vec::new(),
        };
        m.normalize_variants();
        assert_eq!(m.active_idx, 0, "clamped to 0");
        assert_eq!(m.raw_outputs.len(), m.variants.len(), "lengths reconciled");
        assert_eq!(m.variant_count(), 2);
        assert_eq!(m.content, "a", "content re-mirrored to the active variant");
    }

    #[test]
    fn variant_roundtrips_through_conversation_save_load() {
        // The full persistence contract: a message with variants survives a
        // save→load cycle with all its swipe state intact.
        let path = unique_test_path("variants");
        cleanup(&path);

        let mut conv = Conversation::new();
        conv.add_message(Role::User, "roll the dice".into());
        conv.add_assistant_turn("first reply".into(), String::new(), "<raw1>".into());
        // Simulate two rerolls on the assistant turn.
        {
            let last = conv.messages.last_mut().unwrap();
            last.push_variant("second reply".into(), "<raw2>".into());
            last.push_variant("third reply".into(), "<raw3>".into());
        }
        conv.save(&path).expect("save");

        let loaded = Conversation::load(&path).expect("load");
        assert_eq!(loaded.messages.len(), 2);
        let a = &loaded.messages[1];
        assert_eq!(a.variant_count(), 3, "all three variants persisted");
        assert_eq!(a.content, "third reply", "active variant preserved");
        assert_eq!(a.active_idx, 2);
        assert_eq!(a.variant_at(0), Some("first reply"));
        assert_eq!(a.variant_at(1), Some("second reply"));
        assert_eq!(a.variant_at(2), Some("third reply"));

        cleanup(&path);
    }
}
