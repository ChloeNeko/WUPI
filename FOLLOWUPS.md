# FOLLOWUPS — the remaining audit tail (2026-08-15)

> Landed 2026-08-15 (second pass): items 1–16 + 20 below are FIXED and
> verified (1,047 Rust tests, 110 JS tests, Vite build green, `cargo check`
> clean — deps cached, no llama recompile). This file now holds ONLY the
> three deferred P3 efficiency items, each deferred for a concrete reason
> (see notes). Delete the file when they land or are waved off.

---

## Deferred P3 efficiency (judgment calls, not bugs)

17. **Incremental stream append** — `beats.js` `appendChunk` +
    `wupi-drawer.js` `appendToBubble` rebuild the full innerHTML per chunk
    (O(n²) over a long beat). DEFERRED because every in-flight design
    appends RAW text nodes during streaming + applies markdown only at
    finalize — a VISIBLE behavior change (bold/quote styling would pop in
    only at beat end), which contradicts this section's "no behavior
    change" contract. Needs Chloe's eye on the streaming aesthetic before
    building it. The chunk rate is the backend's natural stream rate and
    beats are typically a few KB, so the O(n²) is bounded in practice.

18. **Meta-only save parses** — `fable_continue_target` / `fable_cards_list`
    / `list_saves` parse full payloads for metadata. DEFERRED: the audit
    itself scoped this as "fix it if the tree grows" — it requires a new
    timestamp-only sidecar struct (a save-format compatibility surface) for
    a tree that is currently a handful of slots per card. Revisit if save
    listings ever show measurable latency.

19. **N+1 portrait fetches** in the player picker (~player-picker.js:119-133,
    acknowledged in-file). DEFERRED: fixing it properly means a new batched
    Rust IPC (list-with-portraits), a new surface for a picker that opens
    over a handful of local players — the round-trips are local IPC, not
    network. Not worth the IPC surface today.
