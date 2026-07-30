# WEAVER → Phase 4 Live CDP Playtest — 2026-07-29 (LOCAL-only)

> **Scope:** drive the WEAVER New Game interview → finalize → Phase 4 mechanics
> on the freshly-created card, LOCAL model only (Gemma 12B Q6_K). Terminal-only
> CDP harness via port 9222 (no browser). Goal: verify Phase 4 works on a
> WEAVER-generated card, flawlessly or restart.

## Headline

**The WEAVER → Phase 4 graph path is now functionally alive end-to-end for the
first time** — the interview produces a card with a real `<locations>` block,
`enter_fable_session` seeds `travel_graph` (current_node set, bidirectional
edges preserved). **But the live LOCAL narrator does not reliably emit the
`[TRAVEL]`/`[RUMOR]` tracking brackets** — the §11.38/§11.43.B local-Tracker
under-emission failure recurring (mechanics unit-green, live model narrates
moves in prose only, doesn't emit the brackets that drive the schema mutation).
This is a model-behavior gap, distinct from the pipeline work below (all
verified). Also surfaced: a comma-spam repetition variant + a `[READY]`
sentinel leak into the narrator.

## What was built this session (all build + unit verified, 988/2 lib)

The session closed a cascade of pre-existing gaps that had made the WEAVER
Scribe silently extract nothing. Four layered fixes, each mirroring the
PROMPT-CODEX mechanical-tolerance discipline (Rust absorbs model variation;
no prompt nagging, no retry-loop latency):

1. **WEAVER graph authoring** (6 files). `InterviewDraft` gained a
   `SetLocations { nodes: Vec<DraftNode> }` variant (whole-graph idempotent
   overwrite — the robust shape for a local 12B). `to_sim_card_xml` emits the
   `<locations>` block; the downstream `enter_fable_session` seeding path
   (§11.48) was already wired and now fires on WEAVER cards. GM prompt asks
   "what else is reachable from here?"; Scribe prompt + codex teach the shape.

2. **`parse_args_lenient` → `json_repair`** (`tools.rs`). The original blocker:
   the model emitted `{updates:[...]}` (unquoted key) → strict serde rejected
   → fell back to `{"raw":...}` → `validate_args` rejected "missing updates".
   Now routes through `json_repair::repair` on strict-parse failure (the module
   already had a `quote_unquoted_keys` pass — `parse_args_lenient` just wasn't
   using it). + structural tolerance via `normalize_updates_args` (accepts
   standard wrapper / single-update / bare-array).

3. **`deserialize_neighbor_ids` salvager** (`interview_draft.rs`, Gemini Option
   4). The model sometimes nests full node objects inside `neighbors` instead
   of bare id strings (conflating "neighbor = id" with "neighbor = node
   definition"), recursing until max_tokens. Custom Serde visitor: a string →
   that id; an object → extract its `id` AND recurse into nested `neighbors` to
   harvest deeper ids; dedupe. Converts a parse failure into a silent salvage.

4. **Greedy extractor** (`parse_scribe_calls`, Gemini Option 3). The dominant
   live failure: the model emitted PERFECT JSON wrapped in a ```` ```json ````
   fence with NO `<|tool_call>` marker → `parse_tool_calls` returned 0. New
   scribe-scoped `parse_scribe_calls`: primary WUPI-tag path + EOF salvage
   (truncated closers slice to EOF) + fenced/bare-JSON fallback scoped to the
   `sim_draft` signature (so it can't over-claim in the general chat path).

**Test deltas:** 963/2 (§11.49 baseline) → **988/2** (+25 net new tests, same
2 pre-existing `json_repair` pair, zero regressions). All 33 Phase 4
integration tests still green.

## Live verification (the proof)

A real WEAVER interview produced `the-mossy-gate.sim` carrying a 4-node
`<locations>` block:

```xml
<locations>
  <node id="mossy_gate" setting="indoor"><name>The Mossy Gate</name>
    <neighbor>village_common</neighbor><neighbor>cellar</neighbor><neighbor>forest_path</neighbor></node>
  <node id="village_common" setting="outdoor"><name>Village Common</name><neighbor>mossy_gate</neighbor></node>
  <node id="cellar" setting="indoor"><name>Cellar</name><neighbor>mossy_gate</neighbor></node>
  <node id="forest_path" setting="outdoor"><name>Forest Path</name><neighbor>mossy_gate</neighbor></node>
</locations>
```

After `interview_finalize` → `enter_fable_session`, the live schema showed:
```
travel_graph.current_node: mossy_gate
travel_graph.nodes: 4  (first: mossy_gate indoor → [village_common, cellar, forest_path])
```

**Bidirectionality verdict (Gemini's concern):** the model DOES naturally
produce two-way doors when the worked example demonstrates them. Every edge is
symmetric (mossy_gate lists each neighbor, each neighbor lists mossy_gate back).
The worked-example teaching landed.

## Findings (open issues, distinct from the verified pipeline)

### Finding A — Local Tracker does not emit `[TRAVEL]` brackets (CRITICAL, blocks Phase 4 live)

When the player moved to the cellar, the narrator narrated the move in prose
("The cellar is a vast, subterranean vault...") but emitted **zero
`[TRAVEL cellar]` brackets** → `scene_events: []` → `current_node` stayed
`mossy_gate`. The mechanical travel never registered. This is the
§11.38/§11.43.B local-Tracker under-emission class recurring: the local 12B
narrates but doesn't emit the schema-tracking brackets that drive Phase 3+4
mechanics. All bracket mechanics are unit-test-green; the live model just
doesn't produce them.

**Impact:** `[TRAVEL]`/`[RUMOR]`/`[EFFECT]`/`[MILESTONE]` are dormant in live
LOCAL play. The graph seeds (Finding's good half), but nothing mutates it.
**This is the same root issue AGENTS.md §11.38 documented** — not a regression
from this session's work (the pipeline is verified end-to-end; the model is the
bottleneck). The §11.42 DM/Voice-Actor split (API narrator + local tracker)
was the architectural answer; under LOCAL-only there is no separate tracker,
so the single local narrator must do both jobs and isn't reliably doing the
tracking half.

### Finding B — Comma-spam repetition + `[READY]` leak (model quality)

The cellar narration showed runaway comma insertion: "the, orbs", "a,
weathered", "several, broken pieces of, ancient structures". This is the
§11.41 repetition failure in a new token form (comma-spam vs the prior
hyphen-spam). The DRY sampler's `seq_breakers ["\n"]` resets at newlines but
doesn't catch intra-sentence comma loops. Also: the interview `[READY]`
sentinel leaked into the narrator output (the model echoed it from the GM
persona context). Both are model-output-quality issues, not pipeline bugs.

### Finding C — Stale "needs models" download overlay (cosmetic boot UX)

On every boot the app lands on the "WUPI needs her models / Download" overlay
even though `WUPI.gguf` (9.8GB) + `Embed.gguf` are present and load fine. The
overlay is stale paint — it doesn't block IPC (chat/fable/interview all work
through it). The boot model-gate (`resolve_model_path`) finds the model (the
chat path responds correctly), but the frontend's download overlay isn't
dismissed after `model-status: ready`. Likely the §11.28(C) re-scan concern or
a frontend transition gate. Cosmetic but confusing for a real user.

### Finding D — Scribe first-turn extraction is unreliable (model non-determinism)

Turn 1 of the interview sometimes extracts nothing (empty draft); the
cumulative re-extraction across turns 2+ catches up (the scribe sees the full
transcript each turn, so missed facts get salvaged later). Not blocking — the
draft reaches a finalizable state — but the first-turn UX is a blank preview.
The greedy extractor improved hit rate substantially but the local 12B's
tool-call emission is still inconsistent turn-to-turn.

### Finding E — Stale `__gm_system__` codex partition + namespace field

`memory.sqlite` carries BOTH `__gm_system__` (9 rows, pre-unification) AND
`__fable_system__` (15 rows, post-unification). Entries in the new partition
still carry `namespace: "gm_system"` in their metadata (the field wasn't
updated when the partition key was renamed on 2026-07-29). Retrieval keys on
`card_id` so queries work, but it's cruft + a latent confusion source. Cleanup
is a one-time DB migration or a re-seed.

### Finding F — No tracing subscriber in the debug build (observability gap)

The boot log contains ONLY llama.cpp stderr — zero Rust `tracing::` lines,
because no `tracing_subscriber` is initialized in `main.rs`/`lib.rs`. Every
diagnosis this session required temporary `eprintln!` instrumentation +
rebuilds. A one-line `tracing_subscriber::fmt().with_env_filter(...).init()`
in `run()` would restore forensic observability (retrieval hit counts, scribe
outcomes, sampler profiles, bracket application). High-value, low-effort.

## What is verified working (the wins)

- ✅ WEAVER interview produces a `.sim` with a real `<locations>` block
- ✅ `enter_fable_session` seeds `travel_graph` (current_node + nodes + edges)
- ✅ The graph is bidirectional (worked-example teaching is effective)
- ✅ The full scribe pipeline (greedy extract → json_repair → normalize →
  salvager) recovers the model's varied/malformed tool-call shapes
- ✅ Parser tolerance: unquoted keys, fenced JSON, truncated closers, nested
  neighbor objects — all salvaged mechanically, no retry loop, no prompt bloat
- ✅ LOCAL chat path works (Wupi responds in persona, no markers leaked)
- ✅ Build green: 988/2 lib + 33/33 Phase 4 integration + codex budget + npm

## Recommended next steps (priority order)

1. **Finding A (the blocker):** the local-Tracker bracket under-emission needs
   a dedicated solution — either a prompt-side nudge specific to the Tracker
   role (NOT a negative constraint), a sampler tweak, or accepting that
   LOCAL-only Phase 4 requires the §11.42 split (which needs the API). This is
   the gate to "Phase 4 passes flawlessly on a LOCAL WEAVER card."
2. **Finding F:** add the tracing subscriber — it's the force-multiplier for
   every future diagnosis.
3. **Finding B:** investigate whether the DRY sampler needs a comma-specific
   seq_breaker or whether `truncate_repetition` should token-check commas.
4. **Findings C/D/E:** cosmetic/cleanup; batch when convenient.
