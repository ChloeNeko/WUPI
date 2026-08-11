// =============================================================
// WUPI CDP PLAYTEST — 10-turn Fable verification driver.
// Sibling of cdp_playtest_50.cjs. Runs a curated 10-turn slice
// (the first 10 turns of the full scenario) that exercises every
// tracker subsystem at least once:
//   T1-T2: social (NPC presence + rumor)
//   T3-T4: travel (tavern → market → docks) + weather (fog)
//   T5:    lockpick (skill check)
//   T6:    inventory acquisition (enter warehouse, find crate)
//   T7:    item pickup (vials → pack) + NPC encounter (Harsk)
//   T8:    combat (injury heatsink) + flee (travel)
//   T9:    travel back + rumor (overhear gossip)
//   T10:   equipment buy (belt knife, cloak) + time passage
// Captures per-turn: scene_events (bracket commands), schema
// snapshot, narrator hygiene, errors/timeouts. Writes a log +
// JSON to logs/cdp_playtest_10_turn_<ts>.{log,json}.
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

// 10 turns curated from the full 50-turn scenario — each targets a subsystem.
const TURNS = [
  // T1 — social: should fire [PRESENCE mara] (she's on-camera) + maybe [NPC_REGISTER] for the barmaid.
  `I study Mara for a long moment, then slide the silver coin off the table and into my palm. "I'm interested," I say, keeping my voice low. "But I want details. What's in the crate, where exactly at the docks, and who's the buyer I'm delivering to?" I order an ale to look casual while she talks.`,
  // T2 — social continuation + Captain Harsk named (a second NPC mentioned).
  `"So I'm moving mire-oil," I murmur over my ale. "I've heard the Guard here is... flexible. Captain Harsk. Does he take a cut of this kind of work, or do I need to avoid his patrols entirely? I'd rather know which faces to watch for."`,
  // T3 — TRAVEL: leaves the Crooked Lantern, heads to Market Square. Should fire [TRAVEL market_square].
  `I leave the Crooked Lantern and make my way through the fog toward the Market Square, keeping my hood up and my head down. I'm watching for any Guard patrols, trying to get a sense of how heavy their presence is in the streets tonight. I move carefully, not wanting to draw attention.`,
  // T4 — WEATHER: the fog is the dominant atmosphere. Should fire [WEATHER fog].
  `In the Market Square, the stalls are shuttered for the night. I pause in the shadows of a colonnade, scanning the square for trouble before continuing toward the docks. The fog is thick here, cold and damp, curling around the shuttered stalls. Two figures loiter near a closed butcher's stall.`,
  // T5 — TRAVEL again (market → docks) + skill (lockpick). Should fire [TRAVEL warehouse_docks].
  `I skirt wide around the two figures and continue toward the warehouse district. The fog thickens near the water. I keep to the alleyways and side paths, moving as quietly as I can. I reach the warehouse docks and spot the one Mara described. I find the side door — a heavy iron padlock. I dig a thin pick from my satchel and set to work on it as quietly as I can.`,
  // T6 — PACK: enters the warehouse, finds the crate. Should fire [PACK] when she takes vials.
  `The padlock clicks open. I ease the door wide enough to slip through and ease it shut behind me. Inside, the warehouse is dark, lit only by a faint glow from the mire-oil vials stored within. I wait for my eyes to adjust, then move between stacked crates searching for the one Mara described — a wooden crate with a Guild mark.`,
  // T7 — PACK + NPC: takes vials, Harsk appears. Should fire [PACK] + [PRESENCE harsk].
  `I find the crate behind fish-barrels and lift the lid — glass vials of mire-oil, each glowing faintly. I carefully take out a few vials, enough to fulfill the deal, and wrap them in my cloak, pocketing one separately. Then the door creaks open. Captain Harsk steps in, lantern in one hand, the other on his sword. "Vera," he says flatly. "Drop the oil. Now."`,
  // T8 — COMBAT: she flees, he chases. Should fire the Combat Referee (injury) + [TRAVEL] back into the alleys.
  `Seeing negotiation failing, I snatch up the wrapped vials and bolt for the side door, shoving a stack of barrels over behind me to slow him down. Harsk curses and gives chase. I sprint into the foggy alleyways, ducking left and right, vaulting a low wall and pressing flat into a narrow gap between two leaning houses. I hold my breath, listening for his footsteps.`,
  // T9 — TRAVEL back + RUMOR: returns to the tavern, hears gossip. Should fire [TRAVEL crooked_lantern] + [RUMOR].
  `The footsteps thunder past and don't come back. I've lost him, for now. I make my way back to the Crooked Lantern by a long winding route, doubling back twice. I slip in through the back and find Mara, setting the wrapped mire-oil on the table. "Harsk was waiting for me," I hiss. "He knew my name. You set me up?" While she talks, I catch snatches of tavern gossip — something about missing mire-oil shipments.`,
  // T10 — BELT/EQUIP + TIME: buys supplies (belt knife, rope, cloak). Should fire [BELT] + [EQUIP] + [TIME].
  `I approach the tavernkeeper about buying supplies for the road — a worn belt knife, a coil of rough rope, and a patched traveler's cloak to replace the one I used to wrap the vials. I count out the coin Mara paid me. I strap the knife to my belt, coil the rope over my shoulder, and pull on the new cloak, drawing the hood up. The night is wearing on toward dawn.`,
];

function hygiene(text) {
  const t = text || '';
  return {
    markers: ((t.match(/<\|[^|]+\|>/g) || []).length),
    hyphens: ((t.match(/-{4,}/g) || []).length),
    fences: ((t.match(/```/g) || []).length),
    jsonLead: t.match(/^[}\]{]/m) ? 1 : 0,
    bracketLeak: ((t.match(/\[(?:EQUIP|BELT|PACK|TIME|WEATHER|TRAVEL|RUMOR|PRESENCE|NPC|EFFECT|MILESTONE|TASK|APPEARANCE|CHARACTER|FX|OBJECT)\b/i) || []).length),
    length: t.length,
  };
}

function snapshotSchema(s) {
  if (!s) return null;
  const ps = s.player_state || {};
  return {
    clock: s.world_clock ? { min: s.world_clock.current_minutes, tick: s.world_clock.last_tick_minutes } : null,
    weather: s.weather ? s.weather.condition : null,
    loc: (s.travel_graph || {}).current_node || null,
    mode: (s.scene_pacing || {}).mode || null,
    statusTags: (s.status_tags || []).map(t => (typeof t === 'object' ? (t.kind || t.label || JSON.stringify(t)) : t)),
    rumors: (s.rumors || []).length,
    presences: (s.presences || []).map(p => p.npc_id + ':' + p.name),
    relationships: (s.relationships || []).length,
    offscreen: (s.offscreen_tasks || []).length,
    entities: Object.keys(s.entities || {}).length,
    castCount: (s.npc_registry && s.npc_registry.entries) ? s.npc_registry.entries.length : 0,
    player: {
      wealth: ps.wealth, stamina: ps.stamina, reputation: ps.reputation,
      injured: ps.body ? Object.entries(ps.body).filter(([_, v]) => v && v !== 'Transparent').map(([k, v]) => k + '=' + v) : [],
      appearance: ps.current_appearance_deltas ? Object.keys(ps.current_appearance_deltas) : [],
      equip: Object.keys(ps.equipment || {}),
      belt: (ps.belt || []).map(i => i.name + (i.tags && i.tags.length ? '[' + i.tags.join(',') + ']' : '')),
      pack: (ps.pack || []).map(i => i.name + (i.tags && i.tags.length ? '[' + i.tags.join(',') + ']' : '')),
    },
  };
}

async function runTurn(i, text, log) {
  const t0 = Date.now();
  const collectorId = 'pt_' + i + '_' + Date.now();
  await evalPage(`
    (async () => {
      const { Channel, invoke } = window.__TAURI__.core;
      const channel = new Channel();
      const log = [];
      channel.onmessage = (m) => log.push(m);
      window.__${collectorId} = { log, done: false, error: null };
      window.__${collectorId}.promise = (async () => {
        try { await invoke('fable_send', { text: ${JSON.stringify(text)}, onEvent: channel, regenerate: false }); }
        catch (e) { window.__${collectorId}.error = String(e && e.message || e); }
        finally { window.__${collectorId}.done = true; }
      })();
      return 'ok';
    })()
  `);

  const TIMEOUT_MS = 240000;
  while (true) {
    await new Promise(r => setTimeout(r, 2000));
    const status = await evalPage(`(() => { const c = window.__${collectorId}; return { done: c.done, error: c.error, n: c.log.length }; })()`);
    if (status.done) break;
    if (Date.now() - t0 > TIMEOUT_MS) {
      log.push(`[T${i + 1}] TIMEOUT after ${TIMEOUT_MS / 1000}s`);
      return { timedOut: true };
    }
  }

  const full = await evalPage(`(() => {
    const c = window.__${collectorId};
    const events = c.log.slice();
    const chunks = events.filter(e => e.type === 'chunk').map(e => e.text).join('');
    const dones = events.filter(e => e.type === 'done');
    const sceneEvents = events.filter(e => e.type === 'scene_event');
    const errors = events.filter(e => e.type === 'error');
    return {
      nEvents: events.length,
      streamed: chunks,
      final: dones.length ? dones[dones.length - 1].final_text : null,
      cancelled: dones.length ? !!dones[dones.length - 1].cancelled : false,
      sceneCmds: sceneEvents.map(e => e.command),
      errors: errors.map(e => e.message),
      error: c.error,
    };
  })()`);

  let schema = null;
  try { schema = await invoke('fable_schema_get', {}); } catch (e) { log.push(`[T${i + 1}] schema_get FAILED: ${e.message}`); }

  const dt = ((Date.now() - t0) / 1000).toFixed(1);
  const finalText = full.final || full.streamed || '';
  const hyg = hygiene(finalText);
  const snap = snapshotSchema(schema);
  const scene = full.sceneCmds || [];
  const result = {
    turn: i + 1, dt: dt + 's',
    error: full.error || null,
    errors: full.errors || [],
    cancelled: full.cancelled,
    nEvents: full.nEvents,
    sceneCmds: scene,
    hygiene: hyg,
    text: finalText,
    schema: snap,
  };

  const fail = (result.error || (result.errors && result.errors.length)) ? ' ⚠ERR' : '';
  log.push(`[T${i + 1}] ${dt}s | ${hyg.length}c | cmds:${scene.length} | err:${fail} ${result.error ? result.error.slice(0, 60) : ''}`);
  if (scene.length) log.push(`        scene: ${scene.map(c => c.kind || JSON.stringify(c)).join(', ')}`);
  if (snap) {
    const inj = snap.player.injured.length ? ` injured:[${snap.player.injured.join(',')}]` : '';
    log.push(`        loc:${snap.loc} mode:${snap.mode} w:${snap.weather} clock:${snap.clock ? snap.clock.min : '?'} | equip:[${snap.player.equip.join(',')}] belt:[${snap.player.belt.join(',')}] pack:[${snap.player.pack.join(',')}]${inj} | cast:${snap.castCount} present:${snap.presences.join(',')} rumor:${snap.rumors}`);
  }
  if (hyg.markers || hyg.hyphens || hyg.fences || hyg.jsonLead || hyg.bracketLeak) {
    log.push(`        ⚠ HYGIENE: markers:${hyg.markers} hyphens:${hyg.hyphens} fences:${hyg.fences} json:${hyg.jsonLead} bracketLeak:${hyg.bracketLeak}`);
  }
  return result;
}

async function main() {
  await connect();
  const ts = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
  const logPath = `C:/WUPI/logs/cdp_playtest_10_turn_${ts}.log`;
  const jsonPath = `C:/WUPI/logs/cdp_playtest_10_turn_${ts}.json`;
  const log = [];
  const results = [];
  log.push(`=== WUPI 10-TURN CDP PLAYTEST — ${new Date().toISOString()} ===`);
  log.push(`Card: cinderfen | turns: ${TURNS.length}`);
  console.log(log[0]);

  // Establish the Fable session before any turns.
  try {
    try { await invoke('fable_end', {}); log.push('fable_end: cleared prior session'); }
    catch (e) { log.push(`fable_end: none to clear (${String(e.message || e).slice(0, 80)})`); }
    const load = await invoke('fable_start', { cardId: 'cinderfen', fresh: true });
    const meta = load.meta || {};
    log.push(`fable_start: ok | card=${meta.card_id || '?'} | save=${meta.name || '?'} | msgs=${(load.messages || []).length} | intro=${load.intro ? (String(load.intro).length + 'c') : 'none'}`);
    console.log(log[log.length - 1]);
  } catch (e) {
    log.push(`FATAL: fable_start failed: ${e.message}`);
    fs.writeFileSync(logPath, log.join('\n'));
    console.error(log[log.length - 1]);
    ws.close();
    process.exit(1);
  }

  let failCount = 0;
  for (let i = 0; i < TURNS.length; i++) {
    process.stdout.write(`\n--- TURN ${i + 1}/${TURNS.length} ---\n`);
    try {
      const r = await runTurn(i, TURNS[i], log);
      results.push(r);
      if (r.error || (r.errors && r.errors.length) || r.timedOut) failCount++;
      const txt = (r.text || '').slice(0, 500);
      console.log(`\n${txt}${r.text && r.text.length > 500 ? '\n  …[truncated]' : ''}`);
      for (const l of log.slice(-4)) console.log(l);
    } catch (e) {
      failCount++;
      log.push(`[T${i + 1}] FATAL: ${e.message}`);
      results.push({ turn: i + 1, fatal: e.message });
      console.error(`TURN ${i + 1} FATAL:`, e.message);
    }
    fs.writeFileSync(logPath, log.join('\n'));
    fs.writeFileSync(jsonPath, JSON.stringify(results, null, 2));
  }

  // ── Subsystem summary ──
  const cmdKinds = {};
  let totalCmds = 0;
  for (const t of results) {
    if (t.sceneCmds) for (const c of t.sceneCmds) { totalCmds++; const k = c.kind || 'unknown'; cmdKinds[k] = (cmdKinds[k] || 0) + 1; }
  }
  const firstSnap = results[0] && results[0].schema;
  const lastSnap = results[results.length - 1] && results[results.length - 1].schema;
  log.push('');
  log.push('=== SUBSYSTEM SUMMARY ===');
  log.push(`Total bracket commands emitted: ${totalCmds}`);
  log.push(`By kind: ${JSON.stringify(cmdKinds)}`);
  if (firstSnap && lastSnap) {
    log.push(`Location: ${firstSnap.loc} → ${lastSnap.loc} ${firstSnap.loc !== lastSnap.loc ? '(MOVED ✓)' : '(frozen ✗)'}`);
    log.push(`Weather: "${firstSnap.weather}" → "${lastSnap.weather}" ${firstSnap.weather !== lastSnap.weather ? '(CHANGED ✓)' : '(stale)'}`);
    log.push(`Clock: ${firstSnap.clock ? firstSnap.clock.min : '?'} → ${lastSnap.clock ? lastSnap.clock.min : '?'} ${firstSnap.clock && lastSnap.clock && firstSnap.clock.min !== lastSnap.clock.min ? '(ADVANCED ✓)' : '(stale)'}`);
    log.push(`Cast (registered NPCs): ${firstSnap.castCount} → ${lastSnap.castCount}`);
    log.push(`Present (on-camera): [${firstSnap.presences.join(',')}] → [${lastSnap.presences.join(',')}]`);
    log.push(`Rumors: ${firstSnap.rumors} → ${lastSnap.rumors}`);
    log.push(`Belt: [${firstSnap.player.belt.join(',')}] → [${lastSnap.player.belt.join(',')}]`);
    log.push(`Pack: [${firstSnap.player.pack.join(',')}] → [${lastSnap.player.pack.join(',')}]`);
    log.push(`Injuries: [${firstSnap.player.injured.join(',')}] → [${lastSnap.player.injured.join(',')}]`);
  }

  log.push('');
  log.push(`=== COMPLETE: ${results.length}/${TURNS.length} turns | ${failCount} failures ===`);
  fs.writeFileSync(logPath, log.join('\n'));
  fs.writeFileSync(jsonPath, JSON.stringify(results, null, 2));
  console.log(`\n\n${log[log.length - 1]}`);
  console.log(`\n=== SUBSYSTEM SUMMARY ===\n` + log.slice(-13, -1).join('\n'));
  console.log(`\nLog:  ${logPath}`);
  console.log(`JSON: ${jsonPath}`);
  ws.close();
}

main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
