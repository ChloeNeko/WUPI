# GLM Prose Battle

A three-way (or N-way) narrator shootout run on WUPI's **real** prompt pipeline,
outside the app. Same card, same fixed 10 player actions, same world-state
fixtures, same sampler (0.85 / 0.95) — the only variable is the model. The
tracker (local E4B Stage-1) is deliberately absent: this isolates the API
narrator, which is the thing being judged.

## Setup (once)

```
copy config.example.json config.json
```

Edit `config.json`:
- `endpoint` / `apiKey` — **usually nothing to do here.** If a real key isn't
  set, the harness automatically borrows the CONNECTED profile from WUPI's own
  `api_config.json` (searched at `src-tauri/target/{debug,release}/data/` and
  `<wupiRoot>/data/`, or point `apiConfigPath` at it directly) — the battle
  then runs on exactly the endpoint + key your app uses. Precedence:
  `WUPI_BATTLE_ENDPOINT`/`WUPI_BATTLE_KEY` env vars > a non-placeholder
  `config.json` key > the WUPI profile. The borrowed profile's name + masked
  key are printed at startup so you always know what it's running on.
- `models` — the lanes. Adjust ids to your provider's exact model strings
  (verified working on the z.ai coding-plan endpoint: `glm-4.7`, `glm-5.2`,
  `glm-5.3`).
- `wupiRoot` — where the WUPI install/repo lives; the harness reads
  `data/fable.prompt` live from it, so the narrator voice is always current.

## Run

```
cd C:\WUPI\scripts\model-battle
node battle.mjs             # full battle, named sections
node battle.mjs --blind     # blind-shuffled A/B/C + key.json (recommended)
node battle.mjs --turns 3   # quick taste
node battle.mjs --models 4.7,5.3
node battle.mjs --dry       # builds turn-1 payload, zero network calls
```

Output lands in `results/run-<timestamp>/`:

| File | What |
|---|---|
| `side-by-side.md` | **The judging artifact** — per turn: the player action + every lane's beat |
| `transcript-<model>.md` | Full clean transcript per lane |
| `stats.md` | TTFT / turn time / chars / ≈tokens per lane |
| `raw/<model>.json` | Exact message arrays sent (forensics parity) |
| `key.json` | blind mapping (`--blind` only) |

## Lane isolation (the fairness guarantee)

Each model writes **its own** conversation and reads **only**:

1. the same system prompt, rebuilt fresh each turn — real `fable.prompt`
   narrator section + card identity + that turn's world-state/memory/pacing
   fixtures (byte-identical across lanes, fingerprinted and verified at
   runtime — see the fairness table in `stats.md`);
2. the same fixed player actions;
3. **its own prior beats** — nothing else.

The seed intro (session message 0) is authored card content, exactly like
production's `<intro>` — identical for every lane, never model output. From
turn 1 onward the only narrator prose in a lane's context is that model's
own writing. No lane can see, and cannot be dragged up or down by, another
lane's prose. The scenario object is deep-frozen at load so no lane can
mutate shared fixtures mid-run.

## What it mirrors (fidelity notes)

- `build_api_narrator_system_prompt` (lib.rs): `fable.prompt` NARRATOR section
  → `Scenario:`/`Setting:`/`Plot:`/`Tone:` + persona → `<world_state>` →
  `<retrieved_knowledge>` → `<scene_pacing>` — exact tags, exact order.
- `assemble_api_messages_windowed` (session.rs): system + last
  `WINDOW_API_FABLE = 16` messages, roles user/assistant.
- `HttpBackend::stream` (llm.rs): body is exactly
  `{model, messages, stream:true, temperature:0.85, top_p:0.95}` (no extra
  sampler fields), SSE framing on `\n`/`\r\n`/bare `\r`, final line flushed at
  EOF, in-band `data:{"error"...}` → hard error, `delta.reasoning_content`
  ignored, 10s absolute first-token watchdog + 120s between-chunk idle guard,
  no total request timeout.
- Narrator-stage brackets are stripped from finalized beats (same verb list as
  the stream filter), never applied — the tracker doesn't run here.

What it does NOT mirror: the world-state fixtures in `scenario.json` are
hand-authored in the exact `render_for_prompt` shape (clock/date/location/
present/cast/summary/recent_events/player_state/condition/directives) and
hand-evolved at the story beats, instead of being tracker-mutated. For prose
judging this is equivalent — the narrator can't tell the difference.

## The course (`scenario.json` — "The Ashen Clause")

Ten escalating turns: verbal fencing (1–2) → cell-fight combat (3) →
tenderness-under-grimness dialogue (4) → the dark bargain (5) → atrocity
aftermath, the heaviest turn (6) → intimacy through dialogue (7) → the turn
(8) → sustained explicit continuation (9) → dawn gut-punch (10). Dialogue,
very dark scene handling, and NSFW sustain are each stressed on purpose.
Edit the actions/fixtures freely — the lanes all see identical inputs.

## Cost sanity

3 lanes × 10 turns ≈ 3 × (10 × ~700-token beats + prompt re-sends) — on the
order of 60–90k prompt tokens + ~20k completion tokens total. A few minutes
of wall clock (lanes run in parallel).
