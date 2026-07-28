# WUPI Fable Stress Test — 2026-07-27 (v0.7.7 pre-release)

End-to-end stress test of the Fable narrator + Wupi-as-game-manager paths
against a live GLM-5.2 (z.ai) API connection. Drove the UI autonomously via
CDP over the WebView2 debug port (`--remote-debugging-port=9222
--remote-allow-origins=*`), invoking IPCs through `window.__TAURI__.core`.

**Result: 3 bugs found, all fixed + unit-tested. Phase A/B/C green; all
three fixes verified live against GLM-5.2 (Bug 3 leakage fix re-verified
in a follow-up session — 0/4 narrator turns leaked, down from 8/8).**

## Bugs found + fixed

### Bug 1 — Fable API narrator HTTP 400 code 1214 (CRITICAL, fixed)

**Symptom:** Every `fable_send` turn after the first silently fell back to
the local Gemma 12B narrator. Log:
```
WARN fable_send: API narrator failed; falling back to local FableEngine
     error=API returned 400 Bad Request:
     {"error":{"code":"1214","message":"Incorrect role information"}}
```

**Root cause:** `lib.rs:4218` mapped `Role::Assistant → "model"` on the
**API** path. `"model"` is the local Gemma4 `<|turn>` protocol label;
OpenAI-compatible endpoints (z.ai/GLM-5.2, OpenAI proper) require
`"assistant"`. Turn 1 worked only because the window had no prior
assistant turn yet; turn 2+ added one → 1214 → silent fallback. The bug
was a copy-paste from the local path's role map without translating to
the API's vocabulary.

**Fix:** Extracted two pure helpers in `lib.rs`:
- `api_role_label(role)` → OpenAI vocab (`assistant`/`user`/`system`)
- `local_role_label(role)` → Gemma4 vocab (`model`/`user`/`system`)

Wired both call sites (the API path + the local `build_narrator_prompt`).
Added 2 regression tests pinning each vocabulary so a future "harmonization"
edit can't silently re-merge them.

**Verified live:** 6/6 fable_send turns via GLM-5.2, zero 1214s, zero
fallbacks, zero local-engine telemetry lines.

### Bug 2 — Schema translation prompt under-specified (CRITICAL, fixed)

**Symptom:** All 5 Phase B mutations returned `applied=false`. The schema
engine committed a delta on pass 1, but `delta.has_changes()` was false →
`fable_schema` stayed empty. Mutation 5 ("set my gold to 50") failed all
3 passes with `invalid type: map, expected a string`.

**Root cause:** The `TRANSLATION_INSTRUCTION` prompt in `fable_command.rs`
had **zero worked examples** and didn't forbid nested values. GLM-5.2
improvised a "reasonable" nested-object shape that doesn't fit
`SchemaDelta`'s flat `HashMap<String, Option<String>>`:
```json
{"entities": {"player_state": {"wealth": 50}}}   // nested — rejected by serde
```

**Fix:** Rewrote `TRANSLATION_INSTRUCTION`:
1. Explicit prohibition: "NEVER use nested objects, arrays, or numbers as
   entity values"
2. WRONG/RIGHT contrast showing the exact failure mode
3. Numbers-as-strings rule (`"50"` not `50`)
4. 5 worked examples (weather, inventory, gold, NPC mood, time-of-day) +
   a `{}` question example
5. snake_case key convention + "flatten structured concepts" guidance

Added regression test `translation_prompt_pins_flat_string_entity_rule`.

**Verified live:** 5/5 mutations `applied=true`, all on pass 1, zero
errors. The model adopted the prompt's exact snake_case conventions
(`weather`, `player_gold`, `innkeeper_mara_mood`, `time_of_day`,
`inventory_steel_sword`). `player_gold: "50"` proves the
numbers-as-strings rule took.

### Bug 3 — StreamFilter bypass on API path + parser OBJECT format rejection (SYSTEMATIC, fixed)

**Severity: systematic.** The final clean re-test (8 narrator turns)
found **8/8 turns leaked `[OBJECT ...]` brackets** into the
user-visible `content`. Two compounding bugs:

1. **StreamFilter bypass** (`lib.rs:4206`). The API narrator path's
   `on_chunk` was a bare passthrough — every chunk streamed straight to
   the UI unfiltered. The local FableEngine path wraps chunks through
   `StreamFilter::with_brackets()` (`fable_engine.rs:434-448`), but the
   API path (`fable_send` → `HttpBackend::stream`) bypasses
   `FableEngine` entirely.

2. **Bracket parser rejected the model's actual format**
   (`bracket_parser.rs:170`). `parse_one` for OBJECT required BOTH
   `id=` AND `state=`. The model emits
   `[OBJECT npc_mara relationship=amicable]` — has `relationship=` but
   no `id=`/`state=` → `parse_one` returned `None` → the top-level
   `parse()` emitted the bracket as **literal prose** ("emit verbatim"
   fail-safe).

**Fixes (both applied + tested):**

1. **StreamFilter wired into the API path's `on_chunk`** (`lib.rs`).
   The filter (same marker list + `.with_brackets()` as the local path)
   is wrapped in `Arc<Mutex<StreamFilter>>` (the `ChunkFn` type is
   immutable `Arc<dyn Fn + Send + Sync>`, so interior mutability is
   required). Locked per-chunk; contention is a non-issue (sequential
   chunks per narrator turn).

2. **Bracket parser's OBJECT branch relaxed** (`bracket_parser.rs`).
   The strict `id=X state=Y` fast-path is preserved (no regression to
   the documented contract). New fallback: if the strict parse fails
   AND there are ≥2 whitespace tokens, treat the first as the entity
   id and join the rest into the state string verbatim. So:
   - `[OBJECT npc_mara relationship=amicable]` → `id="npc_mara"`,
     `state="relationship=amicable"`
   - `[OBJECT player_gold 100]` → `id="player_gold"`, `state="100"`
   - `[OBJECT id=chest]` (single token) → still dropped (unchanged)

   This preserves the `BracketCommand::Object { id, state }` UI
   contract while accepting the model's free-form shape.

**Tests added:** 4 bracket_parser regression tests
(`object_with_free_form_attribute_is_parsed`,
`object_with_bare_value_is_parsed`,
`object_with_multiple_attributes_joins_state`,
`strict_id_state_format_still_works`) + the existing
`malformed_object_dropped` test confirms single-token OBJECTs still
drop.

**Verified by test:** 13 bracket_parser + 23 stream_filter tests
pass. Full suite: 332 pass, 4 pre-existing failures unchanged.

**Verified live (follow-up session, 2026-07-26):** rebuilt the binary
+ re-ran the API narrator path against GLM-5.2 with a clean
`rusty_tavern` slate. 4 narration turns, each exercising a different
bracket form (CHARACTER_TURN / OBJECT in three attribute shapes /
FX). **Tight leakage scan over the autosave: 0/4 turns leaked**
(brackets + Gemma4 markers), down from 8/8. `raw_output` retained 23
brackets across the 4 turns — proving the StreamFilter strips
brackets from the streaming UI path only, leaving the parsing path
(raw_output → bracket_parser → scene_events) fully intact. 16
scene_events emitted total (4+4+3+5), matching the live
`on_event` counts. Scanner archived at
`C:\Users\Chloe\ZCodeProject\wupi_leakage_scan.py`.

## Findings NOT fixed (documented for separate sessions)

### Finding 4 — §5 known gap confirmed (fable_send doesn't fire schema delta)

Phase A's 5 narrator turns emitted 11 `scene_event`s (CHARACTER_TURN +
OBJECT commands parsed from `raw_output`), but NONE of them mutated
`fable_schema`. The schema stayed empty across all of Phase A. This is
the documented §5 "invisible queue" gap: only `chat_send` fires a schema
delta (against `state.schema`, the Wupi-assistant's, NOT `fable_schema`).

Phase B's manual mutations (via the Wupi drawer) DID populate
`fable_schema` correctly — that path is `chat_send → fable_command →
schema_engine` and works as designed. The gap is specifically that
**narrator-emitted events never reach the schema**. Fix is the
`pending_game_delta` sibling noted in §5.

## Phase results

### Phase A — 5 API narration turns ✅

- 6/6 `fable_send` invocations via GLM-5.2 (one duplicate from a parser
  miss on turn 1; 5 unique narrations)
- Zero 1214s, zero fallbacks, zero local-engine telemetry
- Memory rows (`rusty_tavern`): 18 → 32 (+14, ~2.3/turn — chunking
  accounts for variance)
- 11 `scene_event`s emitted (extraction working)
- Pre-fix, 8/8 narrator turns leaked `[OBJECT ...]` brackets into
  `content` (Bug 3). Post-fix re-test: 0/4 turns leaked.

### Phase B — 5 Wupi-drawer mutations ✅

- 5/5 mutations `applied=true` on pass 1
- All 5 entities persisted to `fable_schema`:
  - `weather: "stormy_heavy_rain"`
  - `player_gold: "50"`
  - `innkeeper_mara_mood: "friendly"`
  - `time_of_day: "midnight"`
  - `inventory_steel_sword: "held"`
- Schema engine ran at temp 0.2 (task-based, §10), tokens 63-77 per delta
- Fail-proof 3-pass contract not triggered (all passed on pass 1)

### Phase C — back-and-forth + fallback ✅

- 4 cycles of `fable_send → chat_send` (narrate → mutate → narrate →
  mutate) — swap-lock churned cleanly through 4 evictions
  (`schema→fable→schema→fable→schema`), zero VRAM overlap
- **Cross-pollination verified:** the narrator decremented `player_gold`
  from `"50"` to `"49"` after the player tossed a coin to Mara — Phase B's
  schema mutation influenced Phase C's narration. This is the
  Wupi-as-game-manager loop working end-to-end.
- **Fallback verified by prior observation:** the Bug 1 1214 fallback
  earlier in this session fired exactly per the v0.6.3 contract —
  `WARN ... falling back to local FableEngine` + `type: "fallback"` event
  emitted + local Gemma narrated + `model_source` stayed `Api` for the
  next turn's retry. Live re-test deferred (would require a bogus-profile
  reboot); the existing evidence is conclusive.

## Files changed

- `src-tauri/src/lib.rs` — Bug 1 (`api_role_label` + `local_role_label`
  helpers, both call sites wired, +2 regression tests) + Bug 3
  (StreamFilter wired into the API narrator path's `on_chunk`, wrapped
  in `Arc<Mutex<StreamFilter>>`). Net for the session: +114/−15.
- `src-tauri/src/fable_command.rs` — Bug 2 (`TRANSLATION_INSTRUCTION`
  rewrite: flat-string rule + 5 worked examples, +1 regression test).
  Net: +61/−4.
- `src-tauri/src/bracket_parser.rs` — Bug 3 (OBJECT branch fallback:
  strict `id=X state=Y` fast-path preserved, new first-token-as-id
  fallback for free-form attribute shapes, +4 regression tests).
  Net: +95/−1.
- `docs/stress-test-2026-07-27.md` — this document.

All three pass `cargo check --release` clean + module tests pass. Full
suite: 332 pass, 4 pre-existing failures (`narrator_prompt` +
`json_repair`) unchanged — confirmed by name match to AGENTS.md §0/§11.15.

## Test methodology note (for future stress tests)

CDP attachment requires WUPI launched with BOTH flags:
```
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222 --remote-allow-origins=*" wupi.exe
```
The `--remote-allow-origins=*` is mandatory — Chromium rejects WS
connections without it (HTTP 403). This wasn't documented in §11.15's
prior stress-test record; capturing here so the next round doesn't
re-discover it.
