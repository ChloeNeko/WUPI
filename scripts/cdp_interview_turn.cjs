// Terminal-only CDP driver for ONE WEAVER interview turn. Streams the GM reply,
// waits for the detached Scribe to finish, then dumps the draft state.
// Mirrors cdp_fable_turn.cjs's Channel+invoke pattern but for interview_send.
//
// Usage: node scripts/cdp_interview_turn.cjs "<player text>"
//   Prints: GM reply text + the post-turn draft snapshot (so you can watch the
//   card assemble, especially the `locations` field — the Phase 4 graph).

const playerText = process.argv[2];
if (!playerText) {
  console.error('Usage: node cdp_interview_turn.cjs "<text>"');
  process.exit(2);
}

const target_url = 'http://localhost:9222/json/list';
async function findTarget() {
  const r = await fetch(target_url);
  const targets = await r.json();
  const page = targets.find(t => t.type === 'page' && /wupi\.html/.test(t.url));
  if (!page) throw new Error('No wupi.html target.');
  return page.webSocketDebuggerUrl;
}

let ws, idc = 0;
const pending = new Map();
async function connect() {
  const url = await findTarget();
  return new Promise((resolve, reject) => {
    ws = new WebSocket(url);
    ws.addEventListener('open', () => resolve());
    ws.addEventListener('error', () => reject(new Error('ws error')));
    ws.addEventListener('message', ev => {
      const msg = JSON.parse(typeof ev.data === 'string' ? ev.data : ev.data.toString());
      if (msg.id && pending.has(msg.id)) {
        const { ok, err } = pending.get(msg.id);
        pending.delete(msg.id);
        msg.error ? err(new Error(JSON.stringify(msg.error))) : ok(msg.result);
      }
    });
  });
}
function cdp(method, params = {}) {
  const id = ++idc;
  return new Promise((ok, err) => { pending.set(id, { ok, err }); ws.send(JSON.stringify({ id, method, params })); });
}
async function evalPage(expression, awaitPromise = true) {
  const r = await cdp('Runtime.evaluate', { expression, awaitPromise, returnByValue: true });
  if (r.exceptionDetails) throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails));
  return r.result.value;
}

async function main() {
  await connect();
  const collectorId = 'iv_' + Date.now();
  // Set up the collector: stream chunks, track done, capture the final text.
  await evalPage(`
    (async () => {
      const { Channel, invoke } = window.__TAURI__.core;
      const channel = new Channel();
      const chunks = [];
      let gmDoneText = null;
      let ready = false;
      let fallback = null;
      let errMsg = null;
      channel.onmessage = (m) => {
        if (!m || typeof m !== 'object') return;
        if (m.type === 'chunk') chunks.push(m.text || '');
        else if (m.type === 'gm_done') gmDoneText = m.final_text || null;
        else if (m.type === 'ready') ready = true;
        else if (m.type === 'fallback') fallback = m.reason || m.source || 'local';
        else if (m.type === 'error') errMsg = m.message || 'error';
      };
      window.__col_${collectorId} = { chunks, gmDoneText, ready, fallback, errMsg, done: false, scribeRan: false };
      window.__col_${collectorId}.promise = (async () => {
        try {
          await invoke('interview_send', { text: ${JSON.stringify(playerText)}, onEvent: channel });
        } catch (e) {
          window.__col_${collectorId}.errMsg = String((e && e.message) || e);
        } finally {
          window.__col_${collectorId}.done = true;
        }
      })();
      return 'collector_ready';
    })()
  `);

  // Poll for the GM stream to finish (the scribe runs detached after).
  const start = Date.now();
  const GM_TIMEOUT = 180000;
  let lastLen = 0;
  while (Date.now() - start < GM_TIMEOUT) {
    await new Promise(r => setTimeout(r, 1500));
    const status = await evalPage(`(() => {
      const c = window.__col_${collectorId};
      return { done: c.done, err: c.errMsg, n: c.chunks.length, chars: c.chunks.join('').length, ready: c.ready, fallback: c.fallback };
    })()`);
    if (status.chars !== lastLen) {
      process.stdout.write(`\r[t+${Math.round((Date.now()-start)/1000)}s] GM streaming: ${status.chars} chars`.padEnd(70));
      lastLen = status.chars;
    }
    if (status.done) {
      process.stdout.write('\n');
      if (status.err) console.error('TURN ERROR:', status.err);
      break;
    }
  }

  // The GM invoke resolved, but the detached Scribe may still be running.
  // Wait a beat + poll the draft state until it stabilizes (locations field
  // is what we care about — the Scribe's set_locations output).
  process.stdout.write('Waiting for Scribe to settle... ');
  let draft = null;
  let stableCount = 0;
  let lastLocLen = -1;
  for (let i = 0; i < 30; i++) {
    await new Promise(r => setTimeout(r, 1500));
    draft = await evalPage(`(async () => { try { return await (window.__TAURI__.core.invoke)('interview_draft_state', {}); } catch(e){ return {err: String(e&&e.message||e)}; } })()`);
    const locLen = draft && draft.locations ? draft.locations.length : 0;
    if (locLen === lastLocLen) {
      stableCount++;
      if (stableCount >= 2) break; // stable for 2 consecutive polls
    } else {
      stableCount = 0;
      lastLocLen = locLen;
    }
  }
  process.stdout.write('done\n');

  // Dump the GM reply + the draft.
  const gm = await evalPage(`(() => {
    const c = window.__col_${collectorId};
    return { streamed: c.chunks.join(''), final: c.gmDoneText, ready: c.ready, fallback: c.fallback };
  })()`);

  console.log('=== GM REPLY ===');
  console.log(gm.final || gm.streamed || '(none)');
  if (gm.ready) console.log('\n[GM emitted [READY] — draft is final]');
  if (gm.fallback) console.log(`\n[fallback: ${gm.fallback}]`);

  console.log('\n=== DRAFT SNAPSHOT (post-turn) ===');
  // Compact view — the fields that matter + a full locations dump.
  const compact = {
    name: draft && draft.name,
    setting: draft && draft.setting,
    tone: draft && draft.tone,
    player_name: draft && draft.player_name,
    core_persona: draft && draft.core_persona,
    start_npc_ids: draft && draft.start_npc_ids,
    declared_activities: draft && draft.declared_activities,
    player_background: draft && draft.player_background,
    starting_condition: draft && draft.starting_condition,
    world_summary: draft && draft.world_summary,
    completion_pct: draft && draft.completion_pct,
    is_finalizable: draft && draft.is_finalizable,
    locations: draft && draft.locations,
    world_entities: draft && draft.world_entities,
  };
  console.log(JSON.stringify(compact, null, 2));

  // Bidirectionality check on the graph (Gemini's concern).
  if (draft && draft.locations && draft.locations.length) {
    console.log('\n=== BIDIRECTIONALITY CHECK ===');
    const nodes = draft.locations;
    const idSet = new Set(nodes.map(n => n.id));
    let asymmetries = 0;
    for (const n of nodes) {
      for (const nb of (n.neighbors || [])) {
        const reverse = nodes.find(m => m.id === nb);
        if (!reverse) {
          console.log(`  ONE-WAY (dangling): ${n.id} → ${nb} (no node "${nb}" exists)`);
          asymmetries++;
        } else if (!(reverse.neighbors || []).includes(n.id)) {
          console.log(`  ONE-WAY (asymmetric): ${n.id} → ${nb}, but ${nb} does NOT list ${n.id} back`);
          asymmetries++;
        }
      }
    }
    console.log(`  total edges checked, ${asymmetries} asymmetric/dangling`);
    if (asymmetries === 0) console.log('  ✓ GRAPH IS BIDIRECTIONAL');
  }

  ws.close();
}

main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
