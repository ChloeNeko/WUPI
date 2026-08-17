// =============================================================
// WUPI PLAYTEST GRADER — post-hoc tracker scorecard.
// Consumes the JSON written by cdp_playtest_50.cjs (or _10) and
// grades the LOCAL TRACKER MODEL's performance:
//   • world-liveness (did the sim actually move: loc / clock /
//     weather / presence / inventory) — the anti-freeze metrics
//   • bracket-emission coverage vs the 50-turn cinderfen script's
//     ground truth (must / should / bonus kinds)
//   • schema checkpoints (loc at plot beats, disguise by T30,
//     sleep clock-jump T41→T45, knife/salve acquired, town exited)
//   • invariants (clock monotonic, presence ids valid, belt cap,
//     appearance allowlist, narrator hygiene, legacy-verb leak,
//     "(player)" template leak)
//   • quiet-turn precision (over-emission on static dialogue turns)
//   • compare mode: grade two runs side by side (old vs new model)
//
// Usage:
//   node scripts/grade_playtest.cjs                       (newest run)
//   node scripts/grade_playtest.cjs <run.json>
//   node scripts/grade_playtest.cjs <new.json> <baseline.json>
//
// Checkpoint turn numbers are ONLY valid for the 50-turn
// cinderfen script (scripts/cdp_playtest_50.cjs TURNS); other
// lengths get invariants + liveness + histogram only.
// =============================================================

const fs = require('fs');
const path = require('path');
const LOGS = 'C:/WUPI/logs';

// ── helpers ───────────────────────────────────────────────────────────
const clean = (x) => String(x ?? '').replace(/<[^>]*>/g, ' ').trim();
const num = (x) => (typeof x === 'number' && isFinite(x) ? x : null);
const dtSec = (r) => parseFloat(String(r.dt || '0')) || 0;

function newestRun() {
  const cands = fs.readdirSync(LOGS)
    .filter(f => /^cdp_playtest_.*\.json$/.test(f))
    .map(f => ({ f, t: fs.statSync(path.join(LOGS, f)).mtimeMs }))
    .sort((a, b) => b.t - a.t);
  if (!cands.length) throw new Error('No cdp_playtest_*.json in ' + LOGS);
  return path.join(LOGS, cands[0].f);
}

const kinds = (r) => (r.sceneCmds || []).map(c => String(c.kind || '').toLowerCase());
const cmdName = (c) => {
  switch (String(c.kind || '').toLowerCase()) {
    case 'belt': case 'pack': return c.item_name || '';
    case 'equip': return c.item_name || '';
    case 'weather': return c.condition || '';
    case 'travel': return c.destination || '';
    case 'rumor': return c.label || '';
    case 'presence': return c.npc_id || '';
    default: return '';
  }
};
const itemsOf = (snap) => [...(snap?.player?.belt || []), ...(snap?.player?.pack || [])]
  .map(s => String(s).split('[')[0]);
const tagsOf = (snap) => (snap?.statusTags || []).map(clean);
const appearKeys = (snap) => (snap?.player?.appearance || []);
const presenceIds = (snap) => (snap?.presences || []).map(p => String(p).split(':')[0]);
const clockMin = (r) => num(r?.schema?.clock?.min);

const APPEARANCE_ALLOWLIST = new Set(['body_type', 'breast_size', 'disguise', 'ears',
  'eye_color', 'hair_color', 'hair_length', 'hair_style', 'horn', 'outfit', 'scars',
  'skin_complexion', 'tail', 'tattoos', 'wounds']);
const CAST_IDS = new Set(['harsk', 'mara', 'captain-harsk']);
const normId = (x) => String(x || '').replace(/[_-]/g, '');
const AUTHORED_NODES = new Set(['crooked_lantern', 'market_square', 'warehouse_docks']);
// Turns (1-based) that are pure seated dialogue / observation — a TRAVEL or
// DISCOVER there is over-emission, not tracking.
const QUIET_TURNS = new Set([2, 3, 4, 5, 16, 17, 22, 23, 24, 44, 45, 46, 47]);
const LEGACY_KINDS = new Set(['characterturn', 'object', 'fx']);

// ── one run's analysis ────────────────────────────────────────────────
function analyze(results, label) {
  const a = { label, results, lines: [], fails: [], warns: [] };
  const L = (s = '') => { a.lines.push(s); };
  const is50 = results.length >= 50;

  L(`┌─ RUN: ${label}`);
  L(`│ turns: ${results.length} (mode: ${is50 ? '50-turn cinderfen — FULL grading' : 'non-50 script — invariants + liveness only'})`);

  // --- run health ---
  const errored = results.filter(r => r.error || (r.errors && r.errors.length) || r.fatal || r.timedOut);
  const dts = results.map(dtSec).filter(x => x > 0).sort((x, y) => x - y);
  const med = dts.length ? dts[Math.floor(dts.length / 2)] : 0;
  L(`│ errors/fatals: ${errored.length}${errored.length ? '  turns: ' + errored.map(r => r.turn).join(',') : ''}`);
  L(`│ turn wall-clock: median ${med.toFixed(1)}s | max ${dts.length ? dts[dts.length - 1].toFixed(1) : 0}s (CAVEAT: includes the API narrator stage, not tracker-only)`);
  errored.forEach(r => a.fails.push(`T${r.turn} error/fatal: ${String(r.error || r.fatal || (r.errors || []).join('; ')).slice(0, 120)}`));

  // --- emission histogram ---
  const hist = {}; const kindTurns = {};
  for (const r of results) for (const k of kinds(r)) {
    hist[k] = (hist[k] || 0) + 1;
    (kindTurns[k] = kindTurns[k] || []).push(r.turn);
  }
  L(`│ bracket emissions: ${Object.values(hist).reduce((s, n) => s + n, 0)} total | ${JSON.stringify(hist)}`);

  // --- world liveness (the anti-freeze headline) ---
  const locs = [...new Set(results.map(r => r.schema?.loc).filter(Boolean))];
  // min=0 is the pre-[TIME] unset default, not midnight — anchor at the
  // first SET clock so a frozen world gets no credit for its bootstrap jump.
  let clockStart = null, clockEnd = null, maxJump = 0, jumpAt = 0, prev = null;
  let regressions = [];
  for (const r of results) {
    const m = clockMin(r);
    if (m == null || m === 0) continue;
    if (clockStart == null) { clockStart = m; prev = m; continue; }
    clockEnd = m;
    if (prev != null) {
      if (m - prev > maxJump) { maxJump = m - prev; jumpAt = r.turn; }
      if (m < prev) regressions.push(`T${r.turn}: ${prev}→${m}`);
    }
    prev = m;
  }
  const advance = (clockStart != null && clockEnd != null) ? clockEnd - clockStart : 0;
  const weathers = [...new Set(results.map(r => clean(r.schema?.weather)).filter(Boolean))];
  const presenceTurns = results.filter(r => (r.schema?.presences || []).length > 0);
  const presenceIdsSeen = [...new Set(results.flatMap(r => presenceIds(r.schema)))];

  // schema delta per turn (did ANYTHING tracked move) + frozen streaks
  const sig = (r) => JSON.stringify(r.schema && {
    l: r.schema.loc, c: r.schema.clock?.min, w: clean(r.schema.weather),
    p: r.schema.presences, t: r.schema.statusTags?.length, r: r.schema.rumors,
    b: r.schema.player?.belt, k: r.schema.player?.pack, e: r.schema.player?.equip,
    ap: r.schema.player?.appearance, inj: r.schema.player?.injured,
  });
  const changed = results.map((r, i) => i === 0 ? true : sig(r) !== sig(results[i - 1]));
  let streak = 0, worstStreak = 0, worstEnd = 0;
  changed.forEach((c, i) => {
    streak = c ? 0 : streak + 1;
    if (streak > worstStreak) { worstStreak = streak; worstEnd = i + 1; }
  });

  L(`│`);
  L(`│ WORLD LIVENESS:`);
  L(`│   distinct locations visited : ${locs.length}  [${locs.join(', ')}]`);
  L(`│   clock advance over run     : ${advance} min (${(advance / 60).toFixed(1)} h) | biggest single jump ${maxJump} min @T${jumpAt}`);
  L(`│   longest frozen streak      : ${worstStreak} consecutive no-change turns (ending T${worstEnd})`);
  L(`│   weather states seen        : ${weathers.length} [${weathers.join(' | ')}]`);
  L(`│   turns with NPCs on-camera  : ${presenceTurns.length}/50  ids: [${presenceIdsSeen.join(', ') || '—'}]`);
  if (regressions.length) a.fails.push(`clock REGRESSIONS: ${regressions.join(', ')}`);

  // --- checkpoints (50-turn script only) ---
  a.checkpoints = { pass: 0, total: 0, detail: [] };
  if (is50) {
    const at = (t) => results[t - 1];
    const snapWin = (from, to) => results.filter(r => r.turn >= from && r.turn <= to);
    const hasItem = (from, to, re) => snapWin(1, to).some(r => itemsOf(r.schema).some(n => re.test(n)));
    const hasTagOr = (to, re) => snapWin(1, to).some(r =>
      tagsOf(r.schema).some(t => re.test(t)) || appearKeys(r.schema).some(k => re.test(k)));
    const locBy = (to, want) => snapWin(1, to).some(r => r.schema?.loc === want);
    const presBy = (from, to, id) => snapWin(from, to).some(r => presenceIds(r.schema).includes(id));
    const CP = [
      ['loc=market_square by T8 (tavern→market journey)', locBy(8, 'market-square')],
      ['loc=warehouse_docks by T10', locBy(10, 'warehouse-docks')],
      ['harsk PRESENT by T16 (warehouse confrontation)', presBy(1, 16, 'harsk')],
      ['mara PRESENT by T2 (intro scene)', presBy(1, 2, 'mara')],
      ['mara re-PRESENT T20-24 (TTL re-assert)', presBy(20, 24, 'mara')],
      ['loc back to crooked_lantern @T21-22', snapWin(21, 22).some(r => normId(r.schema?.loc) === 'thecrookedlanterntavern')],
      ['knife/blade acquired by T28 (tavern purchase)', hasItem(1, 28, /knife|blade/i)],
      ['disguise marker by T30 (soot + re-braid)', hasTagOr(30, /disguise/i)],
      ['salve/bandage acquired by T42 (herbalist)', hasItem(1, 42, /salve|bandage|poultice|willow|balm/i)],
      ['sleep clock-jump ≥300min across T41→T45', (() => {
        const x = clockMin(at(41)), y = clockMin(at(45)); return x != null && y != null && (y - x) >= 300;
      })()],
      ['left town by T50 (loc ≠ crooked_lantern T49-50)', snapWin(49, 51).some(r => r.schema?.loc && normId(r.schema.loc) !== 'thecrookedlanterntavern')],
      ['≥10h world-time across the run (night + next day)', advance >= 600],
    ];
    L(`│`);
    L(`│ CHECKPOINTS (schema ground truth, cinderfen 50-turn script):`);
    for (const [name, ok] of CP) {
      a.checkpoints.total++;
      if (ok) a.checkpoints.pass++;
      a.checkpoints.detail.push(`${ok ? 'PASS' : 'MISS'}  ${name}`);
      L(`│   ${ok ? '✓' : '✗'} ${name}`);
    }
    if (a.checkpoints.pass < a.checkpoints.total)
      a.fails.push(`checkpoints: ${a.checkpoints.pass}/${a.checkpoints.total}`);
  }

  // --- emission coverage vs the script (50-turn only) ---
  a.cover = { pass: 0, total: 0 };
  if (is50) {
    const n = (k) => hist[k] || 0;
    const travAdds = results.flatMap(r => (r.sceneCmds || []).filter(c =>
      String(c.kind).toLowerCase() === 'travel').map(c => c.destination));
    const MUT = [
      ['MUST travel ≥3 emissions', n('travel') >= 3],
      ['MUST presence asserted ≥4 turns, ≥2 distinct ids',
        presenceTurns.length >= 4 && presenceIdsSeen.filter(i => CAST_IDS.has(i) || CAST_IDS.has(normId(i))).length >= 2],
      ['MUST time advanced ≥3 turns', n('time') >= 3],
      ['MUST inventory add (belt/pack !remove) ≥2 turns', results.filter(r =>
        (r.sceneCmds || []).some(c => ['belt', 'pack'].includes(String(c.kind).toLowerCase()) && !c.remove)).length >= 2],
      ['MUST appearance ≥1 emission', n('appearance') >= 1],
    ];
    const SHD = [
      ['SHOULD rumor ≥1', n('rumor') >= 1],
      ['SHOULD equip ≥1', n('equip') >= 1],
      ['SHOULD effect ≥1', n('effect') >= 1],
      ['SHOULD discover ≥1 (King\'s Road / hills are new)', n('discover') >= 1],
      ['SHOULD npc_register ≥1 (watcher/herbalist/tavernkeeper)', n('npc_register') >= 1],
      ['SHOULD milestone ≥1 (survived Cinderfen)', n('milestone') >= 1],
      ['SHOULD date ≥1 (multi-day run)', n('date') >= 1],
      ['SHOULD weather change ≥1', weathers.length >= 2],
    ];
    L(`│`);
    L(`│ EMISSION COVERAGE:`);
    for (const [name, ok] of [...MUT, ...SHD]) {
      if (MUT.some(m => m[0] === name)) { a.cover.total++; if (ok) a.cover.pass++; }
      ok ? null : (MUT.some(m => m[0] === name) ? a.fails : a.warns).push(name + ` [${JSON.stringify(hist)}]`);
      L(`│   ${ok ? '✓' : '✗'} ${name}`);
    }
    if (travAdds.length) L(`│   travel destinations: [${travAdds.join(', ')}]`);
  }

  // --- invariants (any script length) ---
  const regIds = new Set();
  const viols = [];
  results.forEach((r) => {
    const t = r.turn;
    for (const c of (r.sceneCmds || [])) {
      const k = String(c.kind || '').toLowerCase();
      if (k === 'npc_register' && c.npc_id) regIds.add(String(c.npc_id).toLowerCase());
      if (LEGACY_KINDS.has(k)) viols.push(`T${t}: legacy verb "${k}" reached scene_events (should be dropped in API mode)`);
      if (/\(player\)/i.test(JSON.stringify(c))) viols.push(`T${t}: "(player)" template leak in ${k}`);
    }
    const h = r.hygiene || {};
    if ((h.markers || h.bracketLeak || h.hyphens || h.fences || h.jsonLead))
      viols.push(`T${t}: narrator hygiene markers:${h.markers} bracketLeak:${h.bracketLeak} hyphens:${h.hyphens} fences:${h.fences} jsonLead:${h.jsonLead}`);
    if (/\(player\)/i.test(r.text || '')) viols.push(`T${t}: "(player)" template leak in narrator text`);
    for (const id of presenceIds(r.schema))
      if (!CAST_IDS.has(id) && !CAST_IDS.has(normId(id)) && !regIds.has(id) && !regIds.has(normId(id)) && normId(id) !== '') viols.push(`T${t}: presence on UNKNOWN npc "${id}" (anti-hallucination)`);
    const beltLen = (r.schema?.player?.belt || []).length;
    if (beltLen > 4) viols.push(`T${t}: belt has ${beltLen} items (cap 4 — spill broken)`);
    for (const k of appearKeys(r.schema))
      if (!APPEARANCE_ALLOWLIST.has(k)) viols.push(`T${t}: appearance key "${k}" outside allowlist`);
    if (QUIET_TURNS.has(t) && kinds(r).some(k => k === 'travel' || k === 'discover'))
      viols.push(`T${t}: TRAVEL/DISCOVER on a static dialogue turn (over-emission)`);
    if (!(r.text || '').trim() && !r.cancelled && !r.error && !r.fatal && !r.timedOut)
      viols.push(`T${t}: empty narrator beat (committed blank turn)`);
  });
  L(`│`);
  L(`│ INVARIANT VIOLATIONS: ${viols.length}`);
  viols.slice(0, 40).forEach(v => L(`│   ✗ ${v}`));
  if (viols.length) a.fails.push(`${viols.length} invariant violations`);

  L(`└${'─'.repeat(60)}`);

  // headline numbers for compare mode
  a.h = {
    turns: results.length, errors: errored.length,
    cmds: Object.values(hist).reduce((s, n) => s + n, 0), hist,
    locs: locs.length, advanceMin: advance, frozen: worstStreak,
    presenceTurns: presenceTurns.length, weatherStates: weathers.length,
    cp: is50 ? `${a.checkpoints.pass}/${a.checkpoints.total}` : 'n/a',
    must: is50 ? `${a.cover.pass}/${a.cover.total}` : 'n/a',
    violations: viols.length, medDt: med,
    fails: a.fails, warns: a.warns,
  };
  return a;
}

// ── main ──────────────────────────────────────────────────────────────
const [fileA, fileB] = process.argv.slice(2);
const runA = fileA || newestRun();
const results = JSON.parse(fs.readFileSync(runA, 'utf8'));
const out = [];
const A = analyze(results, path.basename(runA));
out.push(...A.lines);

if (fileB) {
  const B = analyze(JSON.parse(fs.readFileSync(fileB, 'utf8')), path.basename(fileB) + '  [BASELINE]');
  out.push('', ...B.lines, '');
  out.push('═══ COMPARE (new vs baseline) ═══');
  const rows = [
    ['turns', 'turns'], ['cmds', 'bracket cmds'], ['locs', 'distinct locs'],
    ['advanceMin', 'clock advance (min)'], ['frozen', 'longest frozen streak'],
    ['presenceTurns', 'turns w/ presence'], ['weatherStates', 'weather states'],
    ['cp', 'checkpoints'], ['must', 'MUST coverage'], ['violations', 'violations'],
    ['errors', 'errors'], ['medDt', 'median turn s'],
  ];
  for (const [k, name] of rows)
    out.push(`  ${name.padEnd(24)} ${String(A.h[k]).padStart(8)}   vs   ${String(B.h[k]).padStart(8)}`);
  out.push(`  kinds A: ${JSON.stringify(A.h.hist)}`);
  out.push(`  kinds B: ${JSON.stringify(B.h.hist)}`);
  out.push('', `VERDICT vs baseline: ${A.h.cmds > B.h.cmds && A.h.locs > B.h.locs && A.h.advanceMin > B.h.advanceMin ? 'tracker is LIVE where baseline was frozen — compare checkpoints for correctness' : 'inspect the deltas above'}`);
}

out.push('', `FAILS (${(A.fails || []).length}):`);
(A.fails || []).forEach(f => out.push('  ✗ ' + f));
if ((A.warns || []).length) { out.push(`WARNS (${A.warns.length}):`); A.warns.forEach(w => out.push('  ⚠ ' + w)); }

const text = out.join('\n');
console.log(text);
const ts = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
const outPath = path.join(LOGS, `graded_${path.basename(runA, '.json')}_${ts}.txt`);
fs.writeFileSync(outPath, text);
console.log(`\nWritten: ${outPath}`);
