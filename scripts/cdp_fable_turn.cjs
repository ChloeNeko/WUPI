// Terminal-only CDP driver: sends a single fable turn and prints all
// channel events. Uses the page's own @tauri-apps/api Channel + invoke,
// exactly the same path as src/fable/engine/narrator.js. No browser launch.
//
// Usage: node scripts/cdp_fable_turn.cjs "<player text>" [--regenerate]
//   Reads the final assembled event log from the page after the turn resolves.
const playerText = process.argv[2];
const regenerate = process.argv.includes('--regenerate');
if (!playerText && !regenerate) {
  console.error('Usage: node cdp_fable_turn.cjs "<text>" [--regenerate]');
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
        const {ok, err} = pending.get(msg.id);
        pending.delete(msg.id);
        msg.error ? err(new Error(JSON.stringify(msg.error))) : ok(msg.result);
      }
    });
  });
}
function cdp(method, params = {}) {
  const id = ++idc;
  return new Promise((ok, err) => { pending.set(id, {ok, err}); ws.send(JSON.stringify({id, method, params})); });
}
async function evalPage(expression, awaitPromise = true) {
  const r = await cdp('Runtime.evaluate', {expression, awaitPromise, returnByValue: true});
  if (r.exceptionDetails) throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails));
  return r.result.value;
}

async function main() {
  await connect();

  // Set up an event collector on the page. The collector ID lets us poll
  // progress + read the final log. We do NOT touch any frontend module —
  // this is exactly the same Channel+invoke path the frontend uses.
  const collectorId = 'col_' + Date.now();
  await evalPage(`
    (async () => {
      const { Channel, invoke } = window.__TAURI__.core;
      const channel = new Channel();
      const log = [];
      channel.onmessage = (m) => log.push(m);
      window.__col_${collectorId} = { log, done: false, error: null };
      window.__col_${collectorId}.promise = (async () => {
        try {
          await invoke('fable_send', {
            text: ${JSON.stringify(playerText || '')},
            onEvent: channel,
            regenerate: ${regenerate},
          });
        } catch (e) {
          window.__col_${collectorId}.error = String(e && e.message || e);
        } finally {
          window.__col_${collectorId}.done = true;
        }
      })();
      return 'collector_ready';
    })()
  `);

  // Poll for completion (chunks stream in the meantime).
  const start = Date.now();
  const TIMEOUT_MS = 240000;
  let lastLen = 0;
  while (true) {
    await new Promise(r => setTimeout(r, 1500));
    const status = await evalPage(`(() => {
      const c = window.__col_${collectorId};
      return { done: c.done, error: c.error, eventCount: c.log.length };
    })()`);
    if (status.eventCount !== lastLen) {
      const chunks = await evalPage(`(() => {
        const c = window.__col_${collectorId};
        return c.log.filter(e => e.type === 'chunk').map(e => e.text).join('');
      })()`);
      process.stdout.write(`\r[t+${Math.round((Date.now()-start)/1000)}s] ${status.eventCount} events, ${chunks.length} chars streamed`.padEnd(80));
      lastLen = status.eventCount;
    }
    if (status.done) {
      process.stdout.write('\n');
      if (status.error) console.error('TURN ERROR:', status.error);
      break;
    }
    if (Date.now() - start > TIMEOUT_MS) {
      console.error('\nTIMEOUT after', TIMEOUT_MS/1000, 's');
      break;
    }
  }

  // Dump the full event log + assembled final text.
  const full = await evalPage(`(() => {
    const c = window.__col_${collectorId};
    const events = c.log.slice();
    const chunks = events.filter(e => e.type === 'chunk').map(e => e.text).join('');
    const dones = events.filter(e => e.type === 'done');
    const sceneEvents = events.filter(e => e.type === 'scene_event');
    const errors = events.filter(e => e.type === 'error');
    return {
      total_events: events.length,
      event_types: events.map(e => e.type),
      streamed_chunk_text: chunks,
      final_text: dones.length ? dones[dones.length-1].final_text : null,
      cancelled: dones.length ? !!dones[dones.length-1].cancelled : false,
      scene_events: sceneEvents.map(e => e.command),
      errors: errors.map(e => e.message),
    };
  })()`);

  console.log('=== TURN SUMMARY ===');
  console.log(JSON.stringify({
    total_events: full.total_events,
    cancelled: full.cancelled,
    errors: full.errors,
    scene_events: full.scene_events,
  }, null, 2));
  console.log('\n=== FINAL NARRATOR TEXT ===');
  console.log(full.final_text || full.streamed_chunk_text || '(none)');

  // Check for protocol markers / hyphen spam that the dist(0) fix targeted.
  const markers = (full.final_text || full.streamed_chunk_text || '').match(/<\|[^|]+\|>|<[^>]+>\|[^\s]*|\bhyphen\b/gi) || [];
  const hyphenRuns = (full.final_text || '').match(/-{4,}/g) || [];
  console.log('\n=== HYGIENE ===');
  console.log('protocol markers leaked:', markers.length, markers.slice(0,5));
  console.log('long hyphen runs (4+):', hyphenRuns.length);

  ws.close();
}

main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
