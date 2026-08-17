# E4B Shakedown — Fix Plan (2026-08-17)

> **IMPLEMENTED 2026-08-17 (same day).** All P0/P1/P2 items below are landed
> on `ui-shell` + verified: `cargo test --lib` 1136/1136 green (incl. every
> new test this plan specified), all 6 JS suites green (136 tests), `vite
> build` clean. Deviations from the plan, flagged for Chloe:
> - **P1e two-tier:** "attack" stayed a HARD trigger (the plan soft-listed
>   it, but "I attack the goblin" / "The goblin is attacking me" are pinned
>   combat controls + the core combat loop — a soft-single attack would
>   break both). Soft = hunt/raid/arrest/fight/chase, which covers every
>   observed false positive.
> - **P1b:** besides the kind-domain strip, a disguise-kind tag whose LABEL
>   names no guise/costume ("Focus") also strips (mechanical keyword list) —
>   the plan's own test cases required label-awareness.
> - **P0 measurement:** `data/wupi.prompt` is ~1949 chars ≈ 527 tokens (well
>   under the 1200-token note threshold — no authored-file action needed).
>   The dominant prefix chunk is the `wupi.sim` persona render (~974 tokens).
> - Builds NOT run (Chloe-owned per §P3): `npx tauri build` for the release
>   exes + the CDP verification runbook below remain hers.

> **Handoff doc.** Self-contained plan for a fresh chat. Everything below is
> grounded in the 2026-08-17 wizard walkthrough + 51-turn CDP playtest of the
> E4B swap (`WUPI.gguf` = Gemma-4-E4B Q6_K). Evidence artifacts:
> - `logs/wizard_playtest_50.json` — 51 turns of per-turn schema snapshots
> - `logs/graded_wizard_playtest_50_2026-08-17T17-54-23.txt` — graded scorecard
> - `logs/wiz_*.jpg` — screen captures (gems offscreen, drawer, wizard steps)
> - Drivers: `scripts/cdp_wizard_drive.cjs`, `scripts/cdp_turnloop.cjs`,
>   `scripts/grade_playtest.cjs` (the re-run harness for verification)
> - Test card: `apps/fable/cards/stranded-in-cinderfen/` (release exe tree)
>
> **Verdict driving this plan:** the E4B turned the tracker from frozen (12B:
> 1 location, pinned clock, 0 presence turns, 13 brackets/50 turns) to alive
> (3 locations, 48/50 presence turns, live clock, tagged inventory, 9/12
> checkpoints). Its failure mode is now *sloppy enthusiasm*, not silence —
> fix with Rust mechanics + small prompt lines, per the Prime Mandate
> (mechanical fixes beat prompt bloat; negative prompting stays banned).

---

## P0 — Wupi chat context lockout (the copilot is dead)

**Symptom (reproduced twice):** every Wupi chat turn, including plain
"Hello Wupi", returns `context too long even after truncation: 2875 tokens,
system prefix 1541, max 1536`. Error site: `engine.rs:602`.

**Root cause:** the system prefix ALONE is 1541 tokens; the prompt budget is
`CTX_LOCAL_WITH_API (2048) − 512 generation reserve = 1536`. Truncation
(`truncate_to_fit`) can only drop conversation turns, never the system
prefix, so 1541 > 1536 is unfixable at runtime. The manager path makes it
worse (world-state slice + memory block ride the system message while a
Fable session is active).

**Correction to the proposed fix:** the context is ALREADY 2048 — "bump
max_tokens to 2048" is a no-op. The knob is `CTX_LOCAL_WITH_API` in
`settings.rs:79`. Chloe has signed off on raising it (this overrides the
locked-constant rule for this one value).

**Changes:**
1. `settings.rs`: `CTX_LOCAL_WITH_API: 2048 → 3072` (prompt budget
   1536 → 2560). Matches `CTX_FABLE`; KV cost of the persistent chat context
   grows ~50% (recalibrate the boot telemetry figure in AGENTS §3.1).
   4096 is overkill — the overflow is ~35 tokens today plus the manager
   slice (~600-800); 3072 leaves headroom without doubling KV.
2. **Measure before/after (diagnosis, not a blocker):** the system prefix
   is `data/wupi.prompt` (authored file, loaded at `lib.rs:863`) + user
   profile + manager slice + memory block. Add a one-line `eprintln!`
   telemetry at prompt-render time: rendered system-prefix token count per
   path (chat vs manager). House style — same as the engine perf block.
   If `wupi.prompt` alone is >1200 tokens, note it for Chloe (the file is
   authored data; she owns whether it shrinks — do NOT trim it silently).
3. **Guard rail so this can't silently recur:** at boot, after the prompt
   renders once, `eprintln!` a warning if system prefix > 70% of prompt
   budget on any path.

**Tests:** existing engine tests + a new one: render the wupi system prompt
with a stub session + memory block; assert prefix < budget − 200.
**Verify:** with the app running, drawer chat "Hello Wupi" replies; manager
query ("what time is it in the game?") returns a QueryWorldState narration.

---

## P1 — Tracker mechanics (Rust) for the E4B's quirks

### P1a — Inventory phantom-quote corruption + duplicate spam

**Evidence (T46):** pack jumped 5→13 in one turn; entries included
`"Worn`, `"Rough`, `"WATERED`, `"WILLOW-BARK`, `"rolled` (names with literal
leading/trailing quote chars, sometimes truncated fragments) while the real
items also existed. Belt re-added existing items (coin, mire-oil) as new
entries and hit its 4-cap. The Soul-Gem panel renders both copies — user-visible.

**Root cause:** the E4B emits `[PACK name="Worn Traveling Cloak" …]` variants
whose quoting the tokenizer/parser occasionally mangles (unterminated or
doubled quotes survive into `item_name`), and the add path never merges
same-name items.

**Changes (both sides, mechanical):**
1. **Clean at parse time** — `bracket_parser.rs`, beside the existing
   `clean_stance`/`clean_free_text` gates (2026-08-16 audit M2 pattern):
   a `clean_item_name` applied to every Belt/Pack/Equip `item_name`:
   strip leading/trailing `"` `'` `“”‘’` + whitespace; collapse internal
   runs of `"`; drop the item entirely if the cleaned name is empty or a
   1-2 char fragment. Cap length (reuse the stance-style cap, 80).
2. **Merge-on-add** — `equipment.rs` add path (called from
   `lib.rs:5669`/`lib.rs:5735` apply sites): before pushing a new
   `StackItem`, look for an existing entry whose name matches
   case-insensitively after cleaning → increment qty (restack already
   tag-unions per the `equipment.rs:257` comment) instead of appending.
   This also fixes legit re-acquisitions (coin pouches stacking to ×26
   worked; belt re-adds didn't).

**Tests:** `bracket_parser` unit tests with the actual corrupted emissions
(`name=""Worn Traveling Cloak""`, `name="Worn Traveling Cloak`, bare
multiword); equipment test asserting belt add of `Coin` merges with `coin`.

### P1b — EFFECT `kind` confusion (disguise stamped on everything)

**Evidence:** six tags all `kind:"disguise"` with labels
`perception/Focus/Stealth/wounded/Stamina/disguise`. Consequence: kinded
tags are EXCLUDED from `count_by_polarity` → the lethality condition-penalty
DC ignored five phantom "buffs/debuffs".

**Changes (Chloe's spec, mechanical strip):**
1. `consequence.rs` / the bracket apply path: enum-validate `tag_kind`
   against the approved kinded-tag domain (currently just `disguise` —
   confirm the full domain at `StatusTag` (`consequence.rs:667`) and the
   `[EFFECT tag_kind=…]` grammar in the tracker prompt). An unapproved kind
   → **strip the kind, keep the tag as a pure buff/debuff** (polarity
   preserved) + `eprintln!` a one-line warning (telemetry, not silent).
2. Optional one-line prompt clarification in the `<bracket_commands>` EFFECT
   entry (MAY-tune zone): "tag_kind is only for disguise tags; ordinary
   effects omit it." Only if the strip-rate telemetry stays high — do not
   lead with prompt text.

**Tests:** validator test: `[EFFECT label=Focus polarity=buff kind=disguise]`
→ tag lands with NO kind, polarity kept; `[EFFECT label=dockhand pose kind=disguise]`
→ kind kept.

### P1c — Travel paralysis: the tracker won't invent nodes

**Evidence:** T49–50 (leaving via the King's Road) never moved `loc` off
`market-square`. Current applier: unknown destination → REJECT + directive
listing known nodes (`schema.rs:327 resolve_node_id`, reject site in the
apply path). The E4B obeys the anti-teleport directive so well it stops
traveling. DISCOVER was never emitted (0 uses in 51 turns).

**Changes (prompt line + applier change TOGETHER — the line alone changes
nothing while unknown ids still reject):**
1. Applier (`lib.rs` travel apply): unknown destination after
   `resolve_node_id` fuzzy-miss → **CREATE the node** (slugified id from the
   emitted name, registered with the fuzzy-resolver aliases, auto-linked
   bidirectionally to `current_node` — same edge treatment as the
   known-but-non-adjacent auto-link), capped by the existing 96-node travel
   limit. Emit a scene event equivalent to DISCOVER so the UI/world tick
   treat it as growth. This replaces reject-with-directive for TRAVEL
   (DISCOVER itself stays as-is).
2. Tracker prompt `<bracket_commands>` TRAVEL entry, one line (MAY-tune):
   "You may name a new destination id when the player travels somewhere
   unmapped; the engine links it."
3. Anti-typo guard: before creating, fuzzy-match ≥0.75 against known node
   names/aliases → resolve instead of mint (stops "mrket square" nodes).

**Tests:** apply `[TRAVEL king_s_road]` on the cinderfen graph → node
created, edge exists, `current_node` advances; `[TRAVEL mrket square]` →
resolves to market-square.

### P1d — TIME miscalibration + calendar desync

**Evidence:** clock silent for 27 turns, then +20175 min (~14 days) in ONE
jump at T28; `calendar` stayed "17th of Peatfall, Year 214, a fog-bound
night" forever while the clock reached Day ~17 (no `[DATE]` in 51 turns);
sleep-jump landed a turn early (T42 +360 to 18:45) then T43 added nothing.

**Changes:**
1. **Pacing-aware per-turn clamp** at the `[TIME]` apply site
   (`lib.rs:5820` neighborhood): max Δ per bracket =
   Downtime 24h / Exploration 6h / Combat 1h (SceneMode already threads
   into the referees — mirror it here). Overshoot → clamp to the cap +
   warn directive next turn. Kills the 14-day jump without judging prose.
2. **Day-crossing DATE coupling:** when an accepted `[TIME]` crosses a
   midnight boundary and no `[DATE]` rode the same turn, push a
   next-turn directive (existing `turn_directives` channel): "the calendar
   label is stale — emit `[DATE <new label>]`." Mechanical nudge, not a
   hard reject (can't retro-force a bracket). If the calendar stays >48h
   stale, the tick advances a rendered `day N` suffix as fallback.

**Tests:** clamp table unit test per SceneMode; day-crossing directive
fires exactly once per crossing.

### P1e — ScenePacing / referee keyword false-positives (quote-aware matching)

**Evidence, two distinct bugs, one root cause — keywords matching inside
SPOKEN text:**
- False-Combat: T46 "Have there been any arrests? … Is Harsk still
  **hunting**…" → Combat mode → real injury rolls on a stew-and-gossip turn
  (one Purple). T47 same ("raided").
- False-Rest: T22–24 healed 3 injury grades across tavern negotiation —
  Mara's quoted line "**The rest** when the heat dies down" inside the
  player text matched `REST_KEYWORDS` (`player_state.rs:831`, matcher
  `keyword_present` at `player_state.rs:1119`).

**Changes:**
1. **Strip double-quoted spans** from the action text before ANY keyword
   matching (ScenePacing pillars, `COMBAT_KEYWORDS`, `REST_KEYWORDS`, the
   skill-check list): one shared helper
   `strip_dialogue(&str) -> String` next to `keyword_present`; every
   matcher call site passes the stripped text. Dialogue is speech, not
   action — mechanical and cheap.
2. **Two-tier combat triggers** for the unquoted remainder: hard triggers
   (first-person violence verbs: swing/strike/stab/slash/punch/shove/
   block/parry/dodge/duck/lunge) fire alone; soft triggers
   (hunt/raid/arrest/attack/fight/chase) require ≥2 co-occurring OR one
   hard trigger present. `COMBAT_KEYWORDS` (`player_state.rs:793`) splits
   into two consts; the scene-pacing mirrors (§163 comment requires the
   lists stay in sync) update together.
3. **T43 sleep misclassification:** "exhaustion wins — I sleep, hard, for
   several hours" classified Exploration, so the Recovery Referee (Downtime
   + rest keyword) never fired. Add `sleep|lie down|doze|nap` to the
   Downtime pillar if absent (check `scene_pacing.rs:104`'s note about
   rest verbs living in REST_KEYWORDS but not the pillar — that's the gap).

**Tests:** a false-positive corpus built from the ACTUAL turn texts (quote
T22/T43/T46/T47 strings verbatim in the test) — the classification harness
asserts Downtime/None as appropriate; plus positive combat/rest controls.

---

## P2 — Frontend fixes (stage.js / soul-gem.js / wupi-drawer.js)

### P2a — Soul-Gem HUD offscreen after save/load (anchor reset)

**Evidence:** fresh session → cluster renders at the right edge (verified in
screenshots). After manual save → title → **Continue** resume: `.hud-backpack`
at x=−119, gems bloom to x=−134…−426, inventory panel at x=−220 — the whole
inventory UI is unusable (DOM clicks still work; actions fire fine, so it is
purely positioning).

**Root cause:** gems are DOM-unanchored from the paperdoll (see the header
comment in `soul-gem.js` — bloom targets are computed from the paperdoll
`<img>` box); on a load-resume stage entry the anchor box is stale/degenerate
when recompute doesn't run (likely worse with no portrait, where the img box
collapses).

**Changes:** one recompute entry point (the existing resize/re-nudge routine)
called on EVERY stage entry path — title Continue resume, `fable_load_save`
install, and the direct-launch `--card/--save` boot — scheduled after layout
(`requestAnimationFrame` post-`showScreen('stage')`), with a degenerate-box
guard: if the paperdoll img box is zero/absent, fall back to the last known
good anchors or stage-relative defaults instead of negative coordinates.
Chloe's framing ("hook whenever a world .lnk is loaded") is the
direct-launch branch of this — cover all three.

**Verify:** save → title → Continue → screenshot shows the cluster at the
right edge; repeat via `fable.exe --card stranded-in-cinderfen --save
midrun-test`.

### P2b — ✎ edit offered on beats the backend will refuse + blank-on-fail

**Evidence:** the ✎ editor opened on beat 44 (non-trailing AI beat);
`edit_message` correctly refused (`index 44 is not the trailing assistant
message` — the §7.2 invariant held); the beat then rendered BLANK until a
feed rebuild (backend `session.json` was never touched — Continue restored
the full text, so display-only).

**Changes:**
1. Gate the ✎ affordance in `beats.js`/`stage.js` drawer-state logic to
   the same rule the backend enforces (trailing assistant beat or any user
   beat) — the same `computeDrawerState`-style click-time re-derivation
   used for the › button (#84 pattern).
2. On an `edit_message` IPC error, revert the optimistic inline edit and
   restore the beat's prior text (today the error renders as an error beat
   but the edited beat stays blank).

**Test:** `drawer-logic.test.mjs` gains the canEdit derivation case.

### P2c — Drawer wedge → screen limbo (lifecycle race)

**Evidence:** sequence = drawer open → Save modal (covers drawer → drawer
auto-pull-in begins) → modal ✕ close → click at the Load button's stale
coords during the drawer's close animation → result: `.fable-wupi-drawer`
0×0, corner trigger 0×0, NO screen visible, `fable-flow-ambiance.is-active`
stuck, composer gone. Only a webview `location.reload()` recovered (Rust
state all survived — models, session, API connection).

**Changes:**
1. Disable the foot buttons while the drawer is closing (same cooldown
   pattern as the gems' 350ms `animating` gate) — no clicks mid-teardown.
2. Wrap the load-flow entry (exitStage → ambience → `showScreen('saves')`)
   in `withShellBusy` + a `try/finally` that guarantees SOME screen is
   shown if any step throws — never leave the app screenless.
3. Repro note for the fix's PR: the exact sequence above is the test case;
   verify the saves screen opens (or the stage stays) instead of limbo.

### P2d — Phantom download overlay (known, cosmetic)

The first-run overlay stays `display:flex; opacity:0; pointer-events:none`
after boot completes. Harmless but pollutes DOM scans + screenshots. Fix:
when the model is confirmed loaded, set `display:none` (one line in the
overlay's hide path). Not the wupi.exe-home bug Chloe already knows — same
overlay, but here it never blocks input.

---

## P3 — What NOT to change (guard rails for the implementing chat)

- **Prime Mandate / prompt discipline stays.** P0 bumps a locked constant
  with Chloe's explicit sign-off (recorded here). P1c/P1b add SINGLE-LINE
  grammar notes to the tracker prompt only if telemetry shows the mechanical
  strip isn't enough — never multi-sentence prompt patches.
- **No negative prompting** for any of the E4B quirks — every fix above is
  mechanical (parse-clean, validate, clamp, merge, strip dialogue).
- **`dist(0)`-only, sampler profiles, other context sizes: untouched.**
- **The sync invariant** (`AGENTS §7.3`): P1c's TRAVEL change touches the
  parser + applier + the `<bracket_commands>` prompt entry — if the emitted
  grammar changes shape anywhere, `stream_filter.rs` regex arms must agree.
- **Builds remain Chloe-owned.** This plan = source edits + exact commands
  for her to run. Suggested build after P0+P1: `npx tauri build` (or dev
  `sd:dev`-equivalent path she prefers), then the verification below.

---

## Verification runbook (after the fixes land)

1. Launch `fable.exe` with
   `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222`,
   connect API.
2. Wupi drawer chat: "Hello Wupi" → replies (P0). Manager query: "what time
   is it and where is Vera" → QueryWorldState narration (P0 + manager path).
3. Re-run the wizard pipeline fresh (player/world/codex via UI) or reuse
   `stranded-in-cinderfen`; then `node scripts/cdp_turnloop.cjs` for the
   51-turn pass. Watch for: no `"…` items in belt/pack (P1a), no
   non-disguise tags carrying `kind:disguise` (P1b), loc leaving town at
   T49-50 with a minted node (P1c), no day-scale clock jumps (P1d), gossip
   turns classified Downtime/Exploration with zero new injuries (P1e).
4. `node scripts/grade_playtest.cjs logs/wizard_playtest_50.json
   logs/cdp_playtest_50_turn_2026-08-10T02-40-07.json` — compare against
   both the 12B baseline and this run's scorecard.
5. Save → title → Continue → screenshot the gem cluster (P2a). Edit ✎ on a
   mid-history AI beat → should not open (P2b). Save-modal → ✕ → immediate
   Load click → saves screen shows (P2c).

## Suggested implementation order

P0 (unblocks the copilot) → P1a + P1b (data corruption, both small) →
P1e (quote-strip helper — unblocks rest/combat sanity) → P1d (clamps) →
P1c (travel minting, largest behavioral change) → P2a → P2b → P2c → P2d.
Each P1 item is independently shippable; P1c changes tracker behavior most
and deserves the full 51-turn re-run before the next.
