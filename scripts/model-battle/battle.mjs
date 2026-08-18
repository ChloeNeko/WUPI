#!/usr/bin/env node
// GLM Prose Battle — three-way (or N-way) narrator shootout run on WUPI's REAL
// prompt pipeline, outside the app. Zero dependencies (Node >= 18). No Tauri,
// no cargo, no npm — plain node.
//
// Fidelity: this harness mirrors, piece by piece:
//   - lib.rs `build_api_narrator_system_prompt` + `assemble_narrator_skeleton`
//     (narrator prose from data/fable.prompt → card identity → world_state /
//     retrieved_knowledge / scene_pacing skeleton, exact tag names + order)
//   - session.rs `assemble_api_messages_windowed` (system + last
//     WINDOW_API_FABLE = 16 messages, user/assistant roles, alternating merge)
//   - llm.rs `HttpBackend::stream`: POST {endpoint}/chat/completions, body is
//     EXACTLY {model, messages, stream:true, temperature 0.85, top_p 0.95}
//     (no extra sampler fields — some providers 400 on them), SSE framing
//     accepting \n / \r\n / bare \r, final line flushed at EOF, in-band
//     `data:{"error":...}` converted to a thrown error, `delta.reasoning_content`
//     ignored (only delta.content counts), 10s absolute first-token watchdog +
//     120s between-chunks idle guard, no total request timeout.
//   - narrator-stage brackets are stripped from the finalized text (server-side
//     strip in production; same verb list the stream filter carries).
//
// The tracker (local E4B Stage-1) is deliberately absent: this tests the API
// narrator in isolation. The <world_state> / <scene_pacing> blocks come from
// hand-authored per-turn fixtures in scenario.json, shaped exactly like
// WorldSchema::render_for_prompt + render_fable_world_state output.
//
// Usage:
//   node battle.mjs                     # full 3-lane battle, named sections
//   node battle.mjs --blind             # shuffled A/B/C sections + key.json
//   node battle.mjs --turns 5           # first 5 turns only
//   node battle.mjs --models 4.7,5.3    # filter lanes by id/label substring
//   node battle.mjs --dry               # build turn-1 payloads, no network
//
// Config: copy config.example.json → config.json (endpoint + apiKey + models).
// Env overrides: WUPI_BATTLE_ENDPOINT, WUPI_BATTLE_KEY.

import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));

// ---- locked constants (mirror settings.rs / engine discipline) ------------
const WINDOW_API_FABLE = 16;
const API_TEMP = 0.85;
const API_TOP_P = 0.95;
const API_FIRST_TOKEN_TIMEOUT_MS = 10_000;
const API_CHUNK_IDLE_TIMEOUT_MS = 120_000;
const TURN_ATTEMPTS = 3;

// SceneMode::prose_guidance, verbatim from schema.rs.
const SCENE_MODES = {
  combat: {
    tag: 'combat',
    guidance:
      "Pace your prose for combat: short sentences, present-tense verbs, no interiority during the exchange — save reflection for after the dust settles. Each turn covers seconds, not minutes.",
  },
  exploration: {
    tag: 'exploration',
    guidance:
      'Pace your prose for exploration: balanced beats, a mix of action and atmosphere. Each turn covers roughly a minute of in-world time.',
  },
  downtime: {
    tag: 'downtime',
    guidance:
      'Pace your prose for downtime: linger on sensory detail, ambient sound, the texture of the place. Each turn can cover an hour or more — let time breathe.',
  },
};

// Narrator-stage bracket strip — every verb the parser + stream filter know,
// case-insensitive (mirror of the streaming regex arms).
const BRACKET_STRIP_RE =
  /\[(?:WEATHER|DATE|TRAVEL|RUMOR|PRESENCE|DISCOVER|NPC_REGISTER|TIME|EFFECT|MILESTONE|TASK|APPEARANCE|EQUIP|BELT|PACK|CHARACTER_TURN|OBJECT|FX)\s+[^\]]*\]\s*/gi;

const stripBrackets = (t) => t.replace(BRACKET_STRIP_RE, '').trim();
const nonEmpty = (s) => (typeof s === 'string' ? s.trim() : '');
const estTokens = (chars) => Math.round(chars / 4);

// ---- args -----------------------------------------------------------------
const argv = process.argv.slice(2);
const flag = (name) => argv.includes(`--${name}`);
const opt = (name) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : null;
};

const DRY = flag('dry');
const BLIND = flag('blind');
const QUIET = flag('quiet');
const TURNS_LIMIT = opt('turns') ? parseInt(opt('turns'), 10) : null;
const MODELS_FILTER = opt('models') ? opt('models').split(',').map((s) => s.trim().toLowerCase()) : null;

// ---- config ---------------------------------------------------------------
const configPath = resolve(opt('config') || join(HERE, 'config.json'));
if (!existsSync(configPath)) {
  console.error(`config.json not found at ${configPath}`);
  console.error('Copy config.example.json → config.json, paste your API key, adjust model ids.');
  process.exit(1);
}
const config = JSON.parse(readFileSync(configPath, 'utf8'));

// ---- WUPI API profile import ------------------------------------------------
// Precedence: env vars > a real (non-placeholder) key in config.json > the
// CONNECTED profile from WUPI's own api_config.json (read-only borrow — the
// battle then uses exactly the endpoint + key the app uses). Candidates cover
// the dev builds (target/{debug,release}/data) and the portable data dir.
function findWupiApiConfig() {
  const root = config.wupiRoot || join(HERE, '..', '..');
  const candidates = config.apiConfigPath
    ? [resolve(config.apiConfigPath)]
    : [
        join(root, 'src-tauri', 'target', 'debug', 'data', 'api_config.json'),
        join(root, 'src-tauri', 'target', 'release', 'data', 'api_config.json'),
        join(root, 'data', 'api_config.json'),
      ];
  for (const p of candidates) if (existsSync(p)) return p;
  return candidates; // for the error message
}
function borrowWupiProfile() {
  const found = findWupiApiConfig();
  const path = Array.isArray(found) ? null : found;
  if (!path) return { searched: found };
  try {
    const cfg = JSON.parse(readFileSync(path, 'utf8'));
    const profiles = Array.isArray(cfg.profiles) ? cfg.profiles : [];
    const profile =
      profiles.find((x) => x.id === cfg.active_profile_id && (x.api_key || '').trim()) ||
      profiles.find((x) => (x.api_key || '').trim());
    return { path, profile };
  } catch (err) {
    return { path, error: err.message };
  }
}

const envEndpoint = process.env.WUPI_BATTLE_ENDPOINT;
const envKey = process.env.WUPI_BATTLE_KEY;
const cfgKey = (config.apiKey || '').trim();
const cfgKeyReal = cfgKey && !cfgKey.startsWith('PASTE');
let endpoint = envEndpoint || config.endpoint;
let apiKey = envKey || (cfgKeyReal ? cfgKey : '');
let borrowed = null;
if (!envKey && !cfgKeyReal) {
  const b = borrowWupiProfile();
  if (b.profile) {
    borrowed = b;
    endpoint = envEndpoint || b.profile.endpoint || endpoint;
    apiKey = b.profile.api_key;
  } else if (b.error) {
    console.error(`Found WUPI api_config.json at ${b.path} but could not parse it: ${b.error}`);
  }
}
if (!DRY && (!endpoint || !apiKey || String(apiKey).startsWith('PASTE'))) {
  console.error('Missing endpoint/apiKey. Looked for a key in:');
  console.error('  1. WUPI_BATTLE_KEY env var');
  console.error('  2. config.json apiKey (currently placeholder)' + (cfgKeyReal ? '' : ' — placeholder'));
  const searched = borrowed ? null : (Array.isArray(findWupiApiConfig()) ? findWupiApiConfig() : null);
  console.error('  3. WUPI api_config.json candidates:');
  for (const p of searched || []) console.error(`     - ${p}`);
  console.error('Connect the API inside WUPI (it persists api_config.json next to the running exe), or paste a key into config.json.');
  process.exit(1);
}
const maskKey = (k) => (k && k.length > 12 ? `${k.slice(0, 5)}…${k.slice(-4)}` : '••••');

let models = config.models || [];
if (MODELS_FILTER) {
  models = models.filter((m) => MODELS_FILTER.some((f) => m.id.toLowerCase().includes(f) || (m.label || '').toLowerCase().includes(f)));
}
if (!models.length) {
  console.error('No models selected (check --models filter / config.json models array).');
  process.exit(1);
}

const scenarioPath = resolve(opt('scenario') || join(HERE, 'scenario.json'));
const scenario = JSON.parse(readFileSync(scenarioPath, 'utf8'));
const turns = scenario.turns.slice(0, TURNS_LIMIT || scenario.turns.length);

// LANE ISOLATION (the fairness invariant): every lane receives the SAME
// system prompt (fable.prompt narrator + card identity + this turn's
// world_state / memory / pacing fixtures) and the SAME fixed player actions.
// The conversation window is each lane's PRIVATE history: session message 0
// is the authored card intro (identical to production's <intro> seed —
// authored content, not model output), and every assistant message after it
// is that lane's OWN beat. No lane can read another lane's prose — the
// fixtures are frozen so nothing can mutate shared state mid-run, and each
// turn's system prompt is fingerprinted so cross-lane equality is VERIFIED
// at runtime, not assumed.
function deepFreeze(o) {
  if (o && typeof o === 'object') {
    for (const v of Object.values(o)) deepFreeze(v);
    Object.freeze(o);
  }
  return o;
}
deepFreeze(scenario);

const fingerprint = (s) => createHash('sha256').update(s).digest('hex').slice(0, 16);

// ---- fable.prompt (real authored narrator voice) ---------------------------
const fablePromptPath = resolve(config.wupiRoot ? join(config.wupiRoot, 'data', 'fable.prompt') : join(HERE, '..', '..', 'data', 'fable.prompt'));
if (!existsSync(fablePromptPath)) {
  console.error(`fable.prompt not found at ${fablePromptPath} (set "wupiRoot" in config.json to the WUPI install root).`);
  process.exit(1);
}

// prompts.rs section split: === NARRATOR === … === AGENT === (whole trimmed lines).
function parseFablePrompts(text) {
  const narrator = [];
  const agent = [];
  let cur = null;
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim();
    if (t === '=== NARRATOR ===') { cur = narrator; continue; }
    if (t === '=== AGENT ===') { cur = agent; continue; }
    if (cur) cur.push(line);
  }
  return { narrator: narrator.join('\n').trim(), agent: agent.join('\n').trim() };
}
const prompts = parseFablePrompts(readFileSync(fablePromptPath, 'utf8'));
if (!prompts.narrator) {
  console.error('fable.prompt has no === NARRATOR === section.');
  process.exit(1);
}

// ---- prompt assembly (mirror of lib.rs) ------------------------------------
function assembleNarratorSkeleton({ playerAction, worldState, memoryBlock, pacing }) {
  let out = '';
  const pa = nonEmpty(playerAction);
  if (pa) out += `<player_action type="manual_override">\n${pa}\n</player_action>\n\n`;
  const ws = nonEmpty(worldState);
  if (ws) out += `<world_state>\n${ws}\n</world_state>\n\n`;
  const mem = nonEmpty(memoryBlock);
  if (mem) out += `<retrieved_knowledge>\n${mem}\n</retrieved_knowledge>\n\n`;
  const mode = SCENE_MODES[pacing] || SCENE_MODES.exploration;
  out += `<scene_pacing mode="${mode.tag}">\n${mode.guidance}\n</scene_pacing>\n\n`;
  return out;
}

function buildApiNarratorSystemPrompt(card, worldState, pacing, memoryBlock) {
  let out = '';
  const narrator = prompts.narrator;
  if (narrator) out += narrator + '\n\n';
  out += `Scenario: ${card.name.trim()}\n`;
  if (nonEmpty(card.setting)) out += `Setting: ${card.setting.trim()}\n`;
  if (nonEmpty(card.plot)) out += `Plot: ${card.plot.trim()}\n`;
  if (nonEmpty(card.tone)) out += `Tone: ${card.tone.trim()}\n`;
  const core = nonEmpty(card.core_persona);
  if (core) out += core + '\n';
  out += '\n';
  out += assembleNarratorSkeleton({ playerAction: null, worldState, memoryBlock, pacing });
  return out;
}

// session.rs assemble_api_messages_windowed: system + last N messages,
// consecutive same-role merged (no-op here — the session strictly alternates).
function assembleApiMessages(systemPrompt, session) {
  const visible = session.slice(-WINDOW_API_FABLE);
  const out = [{ role: 'system', content: systemPrompt }];
  for (const m of visible) out.push({ role: m.role, content: m.content });
  const merged = [];
  for (const m of out) {
    const prev = merged[merged.length - 1];
    if (prev && prev.role === m.role) prev.content += '\n\n' + m.content;
    else merged.push({ ...m });
  }
  return merged;
}

// ---- SSE streaming (mirror of llm.rs HttpBackend::stream) ------------------
function normalizeEndpoint(base) {
  const b = String(base || '').trim();
  if (b.endsWith('/chat/completions')) return b;
  return b.replace(/\/+$/, '') + '/chat/completions';
}

async function readWithTimeout(reader, ms, msg) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(msg)), ms);
    if (timer.unref) timer.unref();
  });
  timeout.catch(() => {});
  try {
    return await Promise.race([reader.read(), timeout]);
  } finally {
    clearTimeout(timer);
  }
}

async function streamChat({ model, messages, onPiece }) {
  const url = normalizeEndpoint(endpoint);
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${apiKey}` },
    body: JSON.stringify({ model, messages, stream: true, temperature: API_TEMP, top_p: API_TOP_P }),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    throw new Error(`HTTP ${res.status} ${res.statusText}: ${body.slice(0, 300)}`);
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  const t0 = Date.now();
  let lineBuf = '';
  let content = '';
  let sawFirst = false;
  let ttftMs = null;
  let terminated = false;

  const handleLine = (rawLine) => {
    const t = rawLine.trim();
    if (!t || !t.startsWith('data:')) return;
    const payload = t.slice(5).trim();
    if (payload === '[DONE]') { terminated = true; return; }
    let evt;
    try { evt = JSON.parse(payload); } catch { return; }
    if (evt.error) throw new Error(`provider error: ${JSON.stringify(evt.error).slice(0, 300)}`);
    const piece = evt.choices?.[0]?.delta?.content; // reasoning_content ignored, as in llm.rs
    if (typeof piece === 'string' && piece) {
      if (!sawFirst) { sawFirst = true; ttftMs = Date.now() - t0; }
      content += piece;
      if (onPiece) onPiece(piece);
    }
  };

  while (!terminated) {
    const idleLimit = sawFirst ? API_CHUNK_IDLE_TIMEOUT_MS : API_FIRST_TOKEN_TIMEOUT_MS;
    const what = sawFirst
      ? `idle timeout: no chunk for ${API_CHUNK_IDLE_TIMEOUT_MS / 1000}s`
      : `first-token timeout: nothing in ${API_FIRST_TOKEN_TIMEOUT_MS / 1000}s`;
    const chunk = await readWithTimeout(reader, idleLimit, what);
    if (chunk.done) break;
    lineBuf += decoder.decode(chunk.value, { stream: true });
    let m;
    while ((m = /\r\n|\r|\n/.exec(lineBuf)) !== null) {
      const line = lineBuf.slice(0, m.index);
      lineBuf = lineBuf.slice(m.index + m[0].length);
      handleLine(line);
      if (terminated) break;
    }
  }
  if (!terminated && lineBuf.trim()) handleLine(lineBuf); // EOF flush, as in llm.rs
  if (!content.trim()) throw new Error('empty reply (HTTP-200 in-band provider failure)');
  return { content, ttftMs, elapsedMs: Date.now() - t0 };
}

// ---- lane runner ------------------------------------------------------------
async function runLane(model, scenario, turns, log) {
  const lane = {
    model,
    ok: true,
    error: null,
    beats: [],
    stats: [],
    session: [{ role: 'assistant', content: scenario.intro }],
  };
  const card = scenario.card;
  const memoryBlock = scenario.memoryBlock || null;

  for (let i = 0; i < turns.length; i++) {
    const turn = turns[i];
    lane.session.push({ role: 'user', content: turn.action });
    const system = buildApiNarratorSystemPrompt(card, turn.worldState || null, turn.pacing, turn.memoryBlock ?? memoryBlock);
    const messages = assembleApiMessages(system, lane.session);

    let result = null;
    let lastErr = null;
    for (let attempt = 1; attempt <= TURN_ATTEMPTS; attempt++) {
      try {
        if (!QUIET) log(`turn ${i + 1}/${turns.length} streaming…`);
        result = await streamChat({ model: model.id, messages });
        break;
      } catch (err) {
        lastErr = err;
        if (attempt < TURN_ATTEMPTS) {
          const backoff = attempt * 2500;
          log(`turn ${i + 1} attempt ${attempt} failed (${err.message}) — retry in ${backoff / 1000}s`);
          await new Promise((r) => setTimeout(r, backoff));
        }
      }
    }
    if (!result) {
      lane.ok = false;
      lane.error = `turn ${i + 1} failed after ${TURN_ATTEMPTS} attempts: ${lastErr.message}`;
      lane.session.pop(); // the unanswered action does not become a phantom turn
      log(lane.error);
      break;
    }
    const beat = stripBrackets(result.content);
    lane.session.push({ role: 'assistant', content: beat });
    lane.beats.push(beat);
    lane.stats.push({
      turn: i + 1,
      ttftMs: result.ttftMs,
      elapsedMs: result.elapsedMs,
      chars: beat.length,
      sysHash: fingerprint(system),
    });
    log(
      `turn ${i + 1}/${turns.length} ok — ${result.ttftMs ? (result.ttftMs / 1000).toFixed(1) : '?'}s TTFT, ` +
        `${(result.elapsedMs / 1000).toFixed(1)}s, ${beat.length} chars`
    );
    if (!QUIET) {
      const teaser = beat.replace(/\s+/g, ' ').slice(0, 140);
      log(`  “${teaser}…”`);
    }
  }
  return lane;
}

// ---- output writers ---------------------------------------------------------
const ts = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
const runDir = join(HERE, 'results', `run-${ts}`);
mkdirSync(join(runDir, 'raw'), { recursive: true });

function writeTranscript(lane) {
  const lines = [];
  lines.push(`# GLM Prose Battle — transcript: ${lane.model.label}`);
  lines.push('');
  lines.push(`- Model id: \`${lane.model.id}\``);
  lines.push(`- Run: ${ts}`);
  lines.push(`- Scenario: ${scenario.card.name} — ${turns.length} turns`);
  lines.push(`- Pipeline: WUPI narrator prompt (fable.prompt + card + world_state fixtures), window ${WINDOW_API_FABLE}, temp ${API_TEMP}, top_p ${API_TOP_P}`);
  lines.push(`- Status: ${lane.ok ? 'complete' : 'FAILED — ' + lane.error}`);
  lines.push('');
  lines.push('---');
  lines.push('');
  lines.push('## Intro (session message 0)');
  lines.push('');
  lines.push(scenario.intro);
  for (let i = 0; i < lane.beats.length; i++) {
    lines.push('');
    lines.push('---');
    lines.push('');
    lines.push(`## Turn ${i + 1} — ${turns[i].pacing}`);
    lines.push('');
    lines.push('**PLAYER**');
    lines.push('');
    lines.push(`> ${turns[i].action.replace(/\n/g, '\n> ')}`);
    lines.push('');
    lines.push(`**NARRATOR — ${lane.model.label}**`);
    lines.push('');
    lines.push(lane.beats[i]);
  }
  lines.push('');
  writeFileSync(join(runDir, `transcript-${slug(lane.model.label)}.md`), lines.join('\n'));
}

function writeSideBySide(lanes, order, labels) {
  const lines = [];
  lines.push(`# GLM Prose Battle — side by side`);
  lines.push('');
  lines.push(`Scenario: **${scenario.card.name}** — ${turns.length} turns · run ${ts}`);
  lines.push('');
  lines.push(
    "*Isolation: every lane read the same system prompt (fable.prompt + card + per-turn world-state fixtures) and the same fixed player actions. From turn 1 on, the only narrator prose in each lane's context was its own — no model ever saw another lane's writing. The seed intro is authored card content, identical for all lanes.*"
  );
  lines.push('');
  if (BLIND) {
    lines.push(`Sections are **blind-shuffled** (A/B/C). The mapping lives in \`key.json\` — don't peek until you've scored.`);
  } else {
    lines.push('Sections are in config order.');
  }
  lines.push('');
  lines.push('---');
  lines.push('');
  lines.push('## Intro (identical seed beat for every lane)');
  lines.push('');
  lines.push(scenario.intro);
  for (let i = 0; i < turns.length; i++) {
    lines.push('');
    lines.push('---');
    lines.push('');
    lines.push(`## Turn ${i + 1} — ${turns[i].pacing}`);
    lines.push('');
    lines.push('**PLAYER**');
    lines.push('');
    lines.push(`> ${turns[i].action.replace(/\n/g, '\n> ')}`);
    for (let s = 0; s < order.length; s++) {
      const lane = order[s];
      lines.push('');
      lines.push(`### ${labels[s]}`);
      lines.push('');
      lines.push(lane.beats[i] ?? `_(lane stopped before this turn: ${lane.error})_`);
    }
  }
  lines.push('');
  writeFileSync(join(runDir, 'side-by-side.md'), lines.join('\n'));
}

function writeStats(lanes) {
  const lines = [];
  lines.push('# GLM Prose Battle — stats');
  lines.push('');
  lines.push(`Run ${ts} · ${turns.length} turns · temp ${API_TEMP} · top_p ${API_TOP_P}`);
  lines.push('');
  lines.push('## Fairness check (lane isolation)');
  lines.push('');
  // Every lane must have received a byte-identical system prompt at every
  // turn index (fixtures + fable.prompt + card). Windows diverge BY DESIGN —
  // each lane carries only its own prose + the shared authored intro/actions.
  let allEqual = true;
  const rows = [];
  for (let i = 1; i <= turns.length; i++) {
    const hashes = lanes.map((l) => l.stats.find((s) => s.turn === i)?.sysHash ?? '—');
    const equal = hashes.length > 0 && hashes.every((h) => h === hashes[0]);
    if (!equal) allEqual = false;
    rows.push(`| ${i} | ${hashes.join(' · ')} | ${equal ? 'identical' : '**DIVERGED**'} |`);
  }
  lines.push(
    allEqual
      ? `All lanes received **byte-identical system prompts** at every turn (sha256 prefixes below). Conversation windows are lane-private by design: each model read only its own beats, the authored intro, and the fixed player actions.`
      : `**WARNING** — system prompts diverged between lanes at some turn (see table). Do not judge this run.`
  );
  lines.push('');
  lines.push('| Turn | System-prompt fingerprints (one per lane) | Match |');
  lines.push('|---:|---|---|');
  lines.push(...rows);
  lines.push('');
  lines.push('| Lane | Turns | Status | Avg TTFT | Avg turn | Total chars | Chars/s | ≈ out tokens |');
  lines.push('|---|---:|---|---:|---:|---:|---:|---:|');
  for (const lane of lanes) {
    const n = lane.stats.length;
    const avg = (f) => (n ? (lane.stats.reduce((a, s) => a + f(s), 0) / n) : 0);
    const totalChars = lane.stats.reduce((a, s) => a + s.chars, 0);
    const totalMs = lane.stats.reduce((a, s) => a + s.elapsedMs, 0);
    lines.push(
      `| ${lane.model.label} | ${n}/${turns.length} | ${lane.ok ? 'ok' : 'failed'} | ${(avg((s) => s.ttftMs || 0) / 1000).toFixed(1)}s | ${(avg((s) => s.elapsedMs) / 1000).toFixed(1)}s | ${totalChars} | ${totalMs ? (totalChars / (totalMs / 1000)).toFixed(1) : '—'} | ${estTokens(totalChars)} |`
    );
  }
  lines.push('');
  lines.push('Per-turn detail:');
  lines.push('');
  for (const lane of lanes) {
    lines.push(`## ${lane.model.label}`);
    lines.push('');
    lines.push('| Turn | TTFT | Elapsed | Chars |');
    lines.push('|---:|---:|---:|---:|');
    for (const s of lane.stats) {
      lines.push(`| ${s.turn} | ${s.ttftMs ? (s.ttftMs / 1000).toFixed(1) + 's' : '—'} | ${(s.elapsedMs / 1000).toFixed(1)}s | ${s.chars} |`);
    }
    lines.push('');
  }
  writeFileSync(join(runDir, 'stats.md'), lines.join('\n'));
}

const slug = (s) => String(s).toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^-+|-+$/g, '');

// ---- dry mode ----------------------------------------------------------------
if (DRY) {
  const turn = turns[0];
  const session = [
    { role: 'assistant', content: scenario.intro },
    { role: 'user', content: turn.action },
  ];
  console.log(`DRY RUN — no network calls.`);
  console.log(`fable.prompt narrator section: ${prompts.narrator.length} chars`);
  console.log(`Turns in scenario: ${turns.length}`);
  console.log('');
  console.log('Turn-1 input fingerprints (proof all lanes read the same prompt structure):');
  const seen = new Set();
  for (const model of models) {
    const system = buildApiNarratorSystemPrompt(scenario.card, turn.worldState || null, turn.pacing, turn.memoryBlock ?? scenario.memoryBlock ?? null);
    const messages = assembleApiMessages(system, session);
    const body = { model: model.id, messages, stream: true, temperature: API_TEMP, top_p: API_TOP_P };
    writeFileSync(join(runDir, `dry-payload-${slug(model.label)}.json`), JSON.stringify(body, null, 2));
    const sysHash = fingerprint(system);
    const winHash = fingerprint(JSON.stringify(messages));
    seen.add(`${sysHash}/${winHash}`);
    console.log(`  [${(model.label || model.id).padEnd(10)}] system ${sysHash} · full-request ${winHash} · ${system.length} chars (≈${estTokens(system.length)} tok) · ${messages.length} messages`);
  }
  console.log('');
  console.log(seen.size === 1 ? 'fairness: all lanes byte-identical at turn 1 ✓ (windows diverge later BY DESIGN — each lane carries only its own beats)' : 'WARNING: lane inputs diverged!');
  console.log(`Payloads written: ${runDir}`);
  process.exit(0);
}

// ---- main ---------------------------------------------------------------------
console.log(`GLM Prose Battle — ${models.length} lanes × ${turns.length} turns`);
console.log(`Scenario: ${scenario.card.name}`);
console.log(`Endpoint: ${normalizeEndpoint(endpoint)}`);
if (borrowed) {
  console.log(`API profile: borrowed WUPI's connected profile "${borrowed.profile.name}" (key ${maskKey(apiKey)})`);
  console.log(`  from ${borrowed.path}`);
  if (borrowed.profile.model) {
    console.log(`  WUPI uses model "${borrowed.profile.model}" on this profile — if a lane 404s on its model id, align config.json's ids with your plan.`);
  }
}
console.log('');

const t0 = Date.now();
const lanes = [];
await Promise.all(
  models.map(async (model) => {
    const prefix = `[${(model.label || model.id).padEnd(10)}]`;
    const log = (msg) => console.log(`${prefix} ${msg}`);
    log('lane start');
    const lane = await runLane(model, scenario, turns, log);
    lanes.push(lane);
    writeTranscript(lane);
    writeFileSync(join(runDir, 'raw', `${slug(model.label)}.json`), JSON.stringify(lane.session, null, 2));
    log(lane.ok ? `lane complete — transcript written` : `lane FAILED — partial transcript written`);
  })
);

// blind shuffle decides the side-by-side order (key written separately)
const order = [...lanes];
if (BLIND) {
  for (let i = order.length - 1; i > 0; i--) {
    const j = Math.floor(Math.random() * (i + 1));
    [order[i], order[j]] = [order[j], order[i]];
  }
}
const labels = order.map((_, i) => `Lane ${String.fromCharCode(65 + i)}`);
writeSideBySide(lanes, order, labels);
writeStats(lanes);
if (BLIND) {
  writeFileSync(join(runDir, 'key.json'), JSON.stringify(Object.fromEntries(order.map((l, i) => [labels[i], { label: l.model.label, id: l.model.id }])), null, 2));
}

console.log('');
console.log(`Done in ${((Date.now() - t0) / 1000 / 60).toFixed(1)} min → ${runDir}`);
console.log(`  side-by-side.md   — the judging artifact${BLIND ? ' (blind; key.json holds the mapping)' : ''}`);
console.log(`  transcript-*.md   — full per-model transcripts`);
console.log(`  stats.md          — TTFT / speed / volume`);
if (lanes.some((l) => !l.ok)) process.exitCode = 1;
