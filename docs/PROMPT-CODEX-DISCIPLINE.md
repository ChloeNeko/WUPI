# The Prompt-Codex Discipline — WUPI's Foundation Law

> **STATUS: CRITICAL FOUNDATION. IMMUTABLE.**
>
> This document defines the architectural discipline that makes every other
> phase of WUPI function. The narrator/tracker, the New Game interview, the
> schema engine, the world simulation — NONE of them work correctly if the
> prompts, sim cards, and codex drift into bloat. This is not a style guide.
> It is the load-bearing contract for the whole system.
>
> **These rules are LOCKED.** They may be changed ONLY by Chloe's direct,
> explicit overrule — relayed through the GLM/Gemini planning loop. No agent
> (Claude, GLM, Gemini, or any future one) may alter these values, add
> negative-constraint clauses, or inflate the prompts without that sign-off.
>
> Authored 2026-07-29 after the HARD STOP prompt scrub (commit `c61fbe7`),
> which fixed a near-fatal regression: the narrator system prompt had grown
> to 3,364 tokens of negative-constraint lectures while the narrator path
> did ZERO codex retrieval — exactly the failure mode the 3-week CODEX +
> bge-small embedder was built to prevent. This document exists so it never
> happens again.

---

## 1. The Prime Mandate: Bloat-Free, Echo-Free

**Every line of prompt text must earn its place in the system prompt. The
default home for any rule, archetype, or edge-case instruction is the CODEX,
not the prompt.**

The system prompt carries ONLY what the model needs in 100% of turns:
identity, the core job, the cold declarative laws. Everything else — bracket
syntax detail, genre guidance, narrative craft, common errors, worked
examples — lives in the codex and arrives on semantic match the instant it
becomes relevant, then leaves when it becomes irrelevant. That surgical
fetch-and-release is what keeps the prompt clean and the token budget safe.

### What this forbids, absolutely

- **NO negative prompting in the prompt.** No "do not" guardrails, no
  "common mistakes" blocks, no "avoid these" lists. Negative framing is the
  Logit Echo Effect engine: stating "do not repeat" surfaces the word
  "repeat" in the model's attention and amplifies the very behavior it
  prohibits. An RLHF model like Gemma 12B reads heavy negative constraints as
  pressure toward "safe tokens" — which reads as coddling, the opposite of
  the anti-sycophancy contract.
- **NO hard, stressful, forceful instructions.** Instructions should feel
  like natural laws the model abides, not guardrails forcing compliance.
  Cold declarative statements ("The world is indifferent; consequences are
  earned, not forgiven") beat 200-word lectures ten times over.
- **NO negative examples.** Showing the model what NOT to write plants the
  failure mode in its context. Show what TO write, or say nothing.
- **NO anti-bias lectures.** The Rust Referees (skill-check, combat +
  lethality, relationship state machine, off-screen tasks, disguise gate)
  MECHANICALLY enforce consequences via dice regardless of what the model
  wants. The model literally cannot be sycophantic when Rust holds the
  outcome. A single declarative law is sufficient; a lecture is pollution.
- **ZERO echo.** If a rule isn't needed every turn, it does not appear every
  turn. Period.

### What this requires

- Instructions are **direct, straight to the point, pure basic instructions.**
- The prompt states each law **once**, coldly.
- Anything the AI doesn't need at 100% goes to the codex.

---

## 2. The CODEX Retrieval Architecture (why the prompt can stay lean)

The CODEX hybrid memory engine + the local bge-small-en-v1.5 embedding model
(384-dim, CLS-pooled, asymmetric query prefix) is the surgical retrieval
system that makes aggressive prompt trimming safe. **Three prompt paths are
retrieval-enabled; this is the foundation, not optional:**

1. **`chat_send`** (the Wupi OS assistant) — queries `search_wupi_visible`
   (active card lore + the `__wupi_system__` partition where `wupi.codex`
   lives).
2. **`interview_send`** (the New Game Game Master) — queries
   `search_fable_visible` (the active card + the unified `__fable_system__`
   partition).
3. **`fable_send`** (the simulation narrator) — queries
   `search_fable_visible` (same unified partition). **This was the bug: the
   narrator was unwired from retrieval until the 2026-07-29 scrub. Never let
   it go unwired again.**

Retrieval is **per-turn, per-message**, best-effort: embed the just-typed
text, query, render via `render_memory_block`, empty-skip on zero hits (zero
cost when nothing clears the cosine floor). Capped at 5 fused hits per turn.
The rendered block is injected as `<retrieved_knowledge>` between
`<world_state>` and `<scene_pacing>` in the narrator prompts.

### The unified Fable partition

`data/fable.codex` → the `__fable_system__` partition. **One partition serves
BOTH Fable-domain personas** (the GM interview persona AND the simulation
narrator). One knowledge base, one retrieval query per turn — no fragmented
vector spaces splitting the model's logic. (Was `data/gm.codex` →
`__gm_system__` before the 2026-07-29 unification.)

### Codex entry budget (load-bearing)

Each codex entry MUST stay **under ~1400 characters** (~350 tokens). bge-small
silently truncates input past 512 tokens → a garbage embedding that scores
near the floor even on a perfect match. **SPLIT long topics into multiple
small entries — do NOT build a chunking engine.** The
`shipped_fable_codex_parses_cleanly` test enforces this at build time.

### The three engine-content files (never tool-authored, replaced verbatim on update)

- `data/wupi.sim` + `data/wupi.codex` — the OS catgirl persona + playbook.
- `data/gm.sim` + `data/fable.codex` — the Game Master persona + the unified
  Fable playbook.

These are carved out of the `codex_create` tool denylist (`tools.rs`) and the
updater preserve rule (`updater.rs`): tool-authoring is blocked, and the
updater replaces them verbatim on update.

---

## 3. Context Sizes (LOCKED)

| Context | Size | Role | When |
|---|---|---|---|
| **Local model** | **4096 tokens** | The narrator / tracker / full local engine | Always (local is the foundation) |
| **API model** | **8192 tokens** | The voice/narrator when connected | When an API profile is active |
| **Local-as-agent** | **2048 tokens** | The silent agency model serving the API | When API is the voice + local is the tracker |

The LOCAL context is **always 4096.** This is not negotiable. The narrator
system prompt (~2050 tokens after the scrub) + 8-message window (~1200 tok) +
1024-token generation reserve fits 4096 with the front-truncation guard as a
rare safety valve. If the prompt ever grows past what 4096 holds, **the fix
is to shrink the prompt (offload to codex), NEVER to raise the context.**
Raising the context was tried (→ 8192) and overran 12 GB VRAM.

---

## 4. Sampler Configuration (LOCKED)

**`greedy()` is NEVER used — only `dist(0)`.** This lesson is permanent: pure
argmax after temp/top_p collapsed Gemma 12B into degenerate output. `dist(0)`
stochastic sampling is the only terminal stage. (See `engine.rs:752` — the
`greedy()` history comment documents its removal.)

### Narrator / creative generation (NARRATING)

```
temp        = 0.85
top_p       = 0.95
min_p       = 0.1
dry         = (multiplier 0.8, base 1.75, allowed_length 2, seq_breakers ["\n"])
logit_bias  = hyphen_biases (the §11.40 anti-hyphen-spam fix)
dist(0)
```

Source: `engine.rs:770-776` (chat path) + `fable_engine.rs` narrator profile
(`sampler_config` NarratorDefaults, `fable_engine.rs:227-231`).

The DRY sampler (`§11.41`) + the post-generation `truncate_repetition`
truncator are the **mechanical loop backstop** — they replace the deleted
"CRITICAL — DO NOT REPEAT" prompt clause. DRY's `seq_breakers ["\n"]` is
load-bearing: it resets the DRY window at every newline so deliberate
paragraph-level rhetorical anaphora is never penalized.

### Tracker / agent actions (JSON parsing, tracking, tool calling)

Same chain EXCEPT temperature lowered to **0.2** and top_p tightened to
**0.9**:

```
temp        = 0.20
top_p       = 0.90
min_p       = 0.1
dry         = (multiplier 0.8, base 1.75, allowed_length 1, seq_breakers ["\n"])
logit_bias  = hyphen_biases
dist(0)
```

Source: `fable_engine.rs` tracker profile (`sampler_config` TrackerDefaults,
`fable_engine.rs:215-219`). The tighter temp/top_p focuses the model on
high-probability tokens for deterministic structured output (bracket
commands, tool-call JSON). DRY `allowed_length` drops to 1 (stricter —
tracker output is mechanical, not rhetorical).

### API models

```
temp  = 0.85
top_p = 0.95
```

Only those two. Providers lock the rest of the sampler knobs; the
`StreamRepetitionDetector` (`§11.43`) is the API-path repetition backstop
(same detect primitive, aborts mid-stream on a confirmed loop).

---

## 5. What May Be Tuned Later (and what NEVER is)

### MAY be tuned (with normal care)

- **The stream filter** (`stream_filter.rs`) — the marker list +
  `with_brackets()` regex may need entries as new bracket commands are added.
  This is the `§11.37` recurrence guard: if a new bracket command is added to
  `parse_one`, it MUST also be added to the streaming regex or it leaks raw
  during live streaming.
- **The logit bias** (`hyphen_biases`) — may need adjustment if a new
  token-level spam mode appears.
- **The DRY sampler values** (`multiplier`, `base`, `allowed_length`) — may
  need a live tuning pass if repetition resurfaces in a new form.

### NEVER touched without Chloe's explicit overrule (relayed via the GLM/Gemini loop)

- The **context sizes** (4096 local / 8192 API / 2048 local-as-agent).
- The **sampler terminal stage** (`dist(0)` only — `greedy()` is permanently
  banned).
- The **narrator sampler profile** (temp 0.85 / top_p 0.95 / min_p 0.1).
- The **tracker sampler profile** (temp 0.2 / top_p 0.9).
- The **API sampler** (temp 0.85 / top_p 0.95).
- The **prime mandate** (bloat-free, echo-free, codex-offload).
- The **unified `__fable_system__` partition** (one query/turn for both Fable
  personas).

Any agent that alters these without sign-off has broken the foundation. The
fix is to revert, not to rationalize the change.

---

## 6. The Anti-Pattern Catalog (what nearly killed the project)

These are the exact shortcuts that accumulated before the 2026-07-29 scrub.
Recognize them; refuse them.

1. **Adding a negative-constraint clause to "fix" a model-behavior bug.**
   Every §11.29 / §11.30 / §11.40 / §11.43.B clause was a shortcut that made
   the next problem worse. The fix is mechanical (Rust Referee, sampler) or
   codex-offload — never more prompt text.
2. **Compensating for prompt bloat by amputating generation budget + memory.**
   `FABLE_MAX_TOKENS` was cut 1024→512 and the LOCAL window 8→6 to "fit" a
   5,770-token prompt into 4,096. The lying comment at `lib.rs:6367` even
   admitted "5724 > 4096" and papered over it. The fix is to shrink the
   prompt, not the budget. Both were restored in the scrub.
3. **Leaving a prompt path unwired from codex retrieval.** The narrator was
   the biggest prompt path AND the only one without retrieval — while a stale
   comment claimed "memory backfills via retrieval." The fix is to wire it;
   the scrub did.
4. **Verbatim worked examples the model copies.** The §11.48 Tracker copied
   "Berserk Rage" + "the stranger paid in gold coins" verbatim from prompt
   examples. The fix is genericized angle-bracket templates, never concrete
   copyable strings.
5. **One codex entry over the bge-small budget.** A 2,800-char entry gets a
   garbage embedding and scores near the floor even on a perfect match. The
   fix is to split — the `shipped_fable_codex_parses_cleanly` test catches
   this, but review it on every codex edit.

---

## 7. Governance

This document + its mirror in `AGENTS.md` are the authoritative source. If
they ever disagree, **this file wins** (it is the dedicated foundation doc;
the AGENTS.md section is a cross-reference).

When in doubt about whether something belongs in the prompt or the codex, the
test is: **"Does the model need this in 100% of turns?"** If no → codex. If
yes → prompt, stated once, coldly, with zero echo.
