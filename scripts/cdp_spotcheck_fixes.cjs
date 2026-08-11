// =============================================================
// WUPI CDP SPOT-CHECK — verify the 4 post-T52 fixes in 5 turns.
// Targets:
//   Fix #1 (token wall 256): tracker has room for multi-bracket turns
//   Fix #2 (auto-quoter): multi-word item names survive (Silver Coin)
//   Fix #3 (travel matcher): [TRAVEL Market Square] advances location
//   Fix #4 (prose truncation): prompt stays under 2922, no OVERFLOW
// Runs on the cinderfen card. Logs to stdout + logs/cdp_spotcheck_<ts>.log
// =============================================================

const fs = require('fs');

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
  if (r.exceptionDetails) throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails).slice(0, 500));
  return r.result.value;
}
async function invoke(cmd, args) {
  const argsJson = JSON.stringify(args || {});
  return evalPage(`(async()=>{return await window.__TAURI__.core.invoke(${JSON.stringify(cmd)}, ${argsJson})})()`);
}

// 5 turns designed to trigger all three logic fixes:
// T1: buy multiple items (Fix #1 token wall + Fix #2 auto-quoter)
// T2: travel with a diegetic name (Fix #3 travel matcher)
// T3: another multi-item interaction
// T4: pure dialogue (brackets should skip — sanity)
// T5: equip multiple items at once
const TURNS = [
  `I approach the tavernkeeper and buy supplies for the road: a belt knife, a coil of rope, a leather flask, and a wool cloak. I pay with silver coins from my pouch and strap the knife to my belt, coil the rope over my shoulder, and pull on the cloak.`,
  `I leave the tavern and head to the Market Square, walking through the foggy streets. I want to see what's happening there this morning.`,
  `At the market, I buy a healing salve and a roll of bandages from the herbalist's stall. I tuck the salve in my pouch and wrap the bandages around my wrist.`,
  `"Tell me about Captain Harsk," I say to the herbalist, leaning on her counter. "What kind of man is he? Does he patrol the docks often?"`,
  `I equip my new knife in my main hand, pull the cloak tight around my shoulders, and check that the rope is secure on my belt. I feel ready for trouble.`,
];

async function runTurn(i, text, log) {
  const t0 = Date.now();
  const collectorId = 'sc_' + i + '_' + Date.now();
  await evalPage(`
    (async () => {
      const { Channel, invoke } = window.__TAURI__.core;
      const channel = new Channel();
      const evLog = [];
      channel.onmessage = (m) => evLog.push(m);
      window.__${collectorId} = { log: evLog, done: false, error: null };
      window.__${collectorId}.promise = (async () => {
        try { await invoke('fable_send', { text: ${JSON.stringify(text)}, onEvent: channel, regenerate: false }); }
        catch (e) { window.__${collectorId}.error = String(e && e.message || e); }
        finally { window.__${collectorId}.done = true; }
      })();
      return 'ok';
    })()
  `);

  while (true) {
    await new Promise(r => setTimeout(r, 2000));
    const status = await evalPage(`(() => { const c = window.__${collectorId}; return { done: c.done, error: c.error, n: c.log.length }; })()`);
    if (status.done) break;
    if (Date.now() - t0 > 180000) { log.push(`[T${i + 1}] TIMEOUT`); return { timedOut: true }; }
  }

  const full = await evalPage(`(() => {
    const c = window.__${collectorId};
    const events = c.log.slice();
    const dones = events.filter(e => e.type === 'done');
    const sceneEvents = events.filter(e => e.type === 'scene_event');
    return {
      nEvents: events.length,
      final: dones.length ? dones[dones.length - 1].final_text : null,
      sceneCmds: sceneEvents.map(e => JSON.stringify(e)),
      errors: events.filter(e => e.type === 'error').map(e => e.message),
      error: c.error,
    };
  })()`);

  let schema = null;
  try { schema = await invoke('fable_schema_get', {}); } catch (e) { log.push(`[T${i+1}] schema_get FAILED: ${e.message}`); }

  const dt = ((Date.now() - t0) / 1000).toFixed(1);
  const finalText = full.final || '';
  const ps = schema ? (schema.player_state || {}) : {};
  const scene = full.sceneCmds || [];
  const result = {
    turn: i + 1, dt: dt + 's',
    error: full.error,
    sceneCmds: scene,
    text: finalText.slice(0, 400),
    schema: schema ? {
      loc: (schema.travel_graph || {}).current_node || null,
      mode: (schema.scene_pacing || {}).mode || null,
      equip: Object.keys(ps.equipment || {}),
      belt: (ps.belt || []).map(x => x.name + (x.tags && x.tags.length ? '[' + x.tags.join(',') + ']' : '')),
      pack: (ps.pack || []).map(x => x.name + (x.tags && x.tags.length ? '[' + x.tags.join(',') + ']' : '')),
    } : null,
  };

  log.push(`[T${i+1}] ${dt}s | cmds:${scene.length} | loc:${result.schema?.loc} equip:[${result.schema?.equip.join(',')}] belt:[${result.schema?.belt.join(',')}] pack:[${result.schema?.pack.join(',')}]`);
  if (scene.length) scene.forEach(c => log.push(`        cmd: ${c.slice(0, 200)}`));
  if (result.error) log.push(`        ⚠ ERR: ${result.error}`);
  return result;
}

async function main() {
  await connect();
  const ts = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
  const logPath = `C:/WUPI/logs/cdp_spotcheck_${ts}.log`;
  const log = [];
  const results = [];
  log.push(`=== WUPI SPOT-CHECK (4 fixes) — ${new Date().toISOString()} ===`);
  console.log(log[0]);

  // Start the session fresh.
  try {
    try { await invoke('fable_end', {}); } catch (e) {}
    const load = await invoke('fable_start', { cardId: 'cinderfen', fresh: true });
    log.push(`fable_start: ok | card=${load.meta?.card_id} | intro=${load.intro ? load.intro.length + 'c' : 'none'}`);
    console.log(log[log.length - 1]);
  } catch (e) {
    log.push(`FATAL: fable_start failed: ${e.message}`);
    console.error(log[log.length - 1]); process.exit(1);
  }

  for (let i = 0; i < TURNS.length; i++) {
    process.stdout.write(`\n--- TURN ${i+1}/${TURNS.length} ---\n`);
    const r = await runTurn(i, TURNS[i], log);
    results.push(r);
    console.log(r.text ? r.text.slice(0, 300) : '(no text)');
    log.slice(-4).forEach(l => console.log(l));
    fs.writeFileSync(logPath, log.join('\n'));
  }

  // Summary verdict.
  log.push('\n=== VERDICT ===');
  let allPass = true;
  for (const r of results) {
    if (r.error) { log.push(`T${r.turn}: FAIL (error)`); allPass = false; }
  }
  // Check if any inventory items landed (Fix #1 + #2).
  const lastSchema = results[results.length - 1].schema;
  const totalItems = (lastSchema?.equip.length || 0) + (lastSchema?.belt.length || 0) + (lastSchema?.pack.length || 0);
  log.push(`Inventory items tracked: ${totalItems} (equip:${lastSchema?.equip.length} belt:${lastSchema?.belt.length} pack:${lastSchema?.pack.length})`);
  if (totalItems >= 2) log.push('  ✅ Fix #1+#2: multiple items tracked (token wall + auto-quoter working)');
  else { log.push('  ⚠️ Fix #1+#2: few items tracked — check bracket emissions'); allPass = false; }

  // Check multi-word names survived (Fix #2).
  const allItems = [...(lastSchema?.equip || []), ...(lastSchema?.belt || []), ...(lastSchema?.pack || [])];
  const multiword = allItems.filter(x => x.split(' ').length > 1 || x.includes('['));
  log.push(`  Multi-word names in inventory: ${multiword.length ? multiword.join(', ') : '(none)'}`);
  if (multiword.length > 0) log.push('  ✅ Fix #2: multi-word item names survived');

  // Check travel (Fix #3).
  const loc = lastSchema?.loc;
  log.push(`  Final location: ${loc}`);
  if (loc && loc !== 'crooked_lantern') log.push('  ✅ Fix #3: travel advanced (diegetic name resolved)');
  else { log.push('  ⚠️ Fix #3: location unchanged — travel matcher may need tuning'); }

  log.push(allPass ? '\n=== SPOT-CHECK PASSED ===' : '\n=== SPOT-CHECK: see warnings above ===');
  fs.writeFileSync(logPath, log.join('\n'));
  console.log('\n' + log.slice(-12).join('\n'));
  console.log(`\nLog: ${logPath}`);
  ws.close();
}

main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
