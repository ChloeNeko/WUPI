// Like cdp_fable_turn.cjs but for chat_send. Verifies the chat engine is
// alive (and serves as the dist(0) sampler test for the chat path too).
const playerText = process.argv[2] || "Hello Wupi, are you there?";

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
  const collectorId = 'chat_' + Date.now();
  await evalPage(`
    (async () => {
      const { Channel, invoke } = window.__TAURI__.core;
      const channel = new Channel();
      const log = [];
      channel.onmessage = (m) => log.push(m);
      window.__col_${collectorId} = { log, done: false, error: null };
      window.__col_${collectorId}.promise = (async () => {
        try {
          await invoke('chat_send', { text: ${JSON.stringify(playerText)}, onEvent: channel });
        } catch (e) {
          window.__col_${collectorId}.error = String(e && e.message || e);
        } finally {
          window.__col_${collectorId}.done = true;
        }
      })();
      return 'ready';
    })()
  `);
  const start = Date.now();
  let lastLen = 0;
  while (Date.now() - start < 180000) {
    await new Promise(r => setTimeout(r, 1500));
    const status = await evalPage(`(() => { const c = window.__col_${collectorId}; return { done: c.done, error: c.error, n: c.log.length }; })()`);
    if (status.n !== lastLen) {
      const txt = await evalPage(`(() => window.__col_${collectorId}.log.filter(e => e.type === 'chunk' || e.type === 'token').map(e => e.text || e.token || '').join(''))()`);
      process.stdout.write(`\r[t+${Math.round((Date.now()-start)/1000)}s] ${status.n} events, ${txt.length} chars`.padEnd(70));
      lastLen = status.n;
    }
    if (status.done) { process.stdout.write('\n'); if (status.error) console.error('TURN ERROR:', status.error); break; }
  }
  const full = await evalPage(`(() => {
    const c = window.__col_${collectorId};
    const events = c.log.slice();
    const chunks = events.filter(e => e.type === 'chunk' || e.type === 'token').map(e => e.text || e.token || '').join('');
    const dones = events.filter(e => e.type === 'done');
    return {
      total_events: events.length,
      event_types: events.map(e => e.type),
      streamed: chunks,
      final: dones.length ? dones[dones.length-1].final_text || dones[dones.length-1].text : null,
      errors: events.filter(e => e.type === 'error').map(e => e.message),
    };
  })()`);
  console.log('=== CHAT TURN SUMMARY ===');
  console.log(JSON.stringify({total_events: full.total_events, event_types: full.event_types, errors: full.errors}, null, 2));
  console.log('\n=== STREAMED/FINAL TEXT ===');
  console.log(full.final || full.streamed || '(none)');
  ws.close();
}
main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
