// Dump RAW chunks (before any frontend beat rendering) to inspect whether
// hyphens arrive from the model or are introduced downstream.
const playerText = process.argv[2] || "I order an ale.";
const target_url = 'http://localhost:9222/json/list';
async function findTarget() {
  const r = await fetch(target_url);
  const targets = await r.json();
  return targets.find(t => t.type === 'page' && /wupi\.html/.test(t.url)).webSocketDebuggerUrl;
}
let ws, idc = 0; const pending = new Map();
async function connect() {
  const ws_ = new WebSocket(await findTarget());
  return new Promise((res, rej) => {
    ws_.addEventListener('open', () => { ws = ws_; res(); });
    ws_.addEventListener('error', () => rej(new Error('ws')));
    ws_.addEventListener('message', ev => {
      const m = JSON.parse(typeof ev.data === 'string' ? ev.data : ev.data.toString());
      if (m.id && pending.has(m.id)) { const {ok,err} = pending.get(m.id); pending.delete(m.id); m.error?err(new Error(JSON.stringify(m.error))):ok(m.result); }
    });
  });
}
function cdp(method, params={}) { const id=++idc; return new Promise((ok,err)=>{pending.set(id,{ok,err}); ws.send(JSON.stringify({id,method,params}));}); }
async function evx(expr, awaitPromise=true) {
  const r = await cdp('Runtime.evaluate', {expression: expr, awaitPromise, returnByValue: true});
  if (r.exceptionDetails) throw new Error(JSON.stringify(r.exceptionDetails));
  return r.result.value;
}
async function main() {
  await connect();
  const cid = 'raw_' + Date.now();
  await evx(`
    (async () => {
      const { Channel, invoke } = window.__TAURI__.core;
      const channel = new Channel();
      const raw = [];
      channel.onmessage = (m) => { if (m.type === 'chunk') raw.push(m.text); };
      window.__col_${cid} = { raw, done: false, error: null };
      (async () => {
        try { await invoke('fable_send', { text: ${JSON.stringify(playerText)}, onEvent: channel }); }
        catch (e) { window.__col_${cid}.error = String(e&&e.message||e); }
        finally { window.__col_${cid}.done = true; }
      })();
    })()
  `);
  const start = Date.now();
  while (Date.now() - start < 180000) {
    await new Promise(r => setTimeout(r, 2000));
    const s = await evx(`(()=>{const c=window.__col_${cid};return {done:c.done,err:c.error,n:c.raw.length};})()`);
    if (s.done) { if (s.err) console.error('ERR:',s.err); break; }
  }
  const dump = await evx(`(()=>{const c=window.__col_${cid}; return { raw_count: c.raw.length, first_30_chunks: c.raw.slice(0,30), assembled: c.raw.join('') };})()`);
  console.log('=== RAW CHUNK COUNT:', dump.raw_count, '===');
  console.log('\n=== FIRST 30 CHUNKS (each on its own line, [bracketed]) ===');
  dump.first_30_chunks.forEach((c, i) => console.log(`[${i}]`, JSON.stringify(c)));
  console.log('\n=== ASSEMBLED RAW TEXT (first 1500 chars) ===');
  console.log(dump.assembled.slice(0, 1500));
  // Hyphen analysis
  const hyphenWords = (dump.assembled.match(/[a-zA-Z]+-[a-zA-Z]+(?:-[a-zA-Z]+)*/g) || []);
  const suspicious = hyphenWords.filter(w => /the-|a-|an-|of-|for-|in-|with-/i.test(w) || w.split('-').length > 2);
  console.log('\n=== HYPHEN ANALYSIS ===');
  console.log('total hyphenated tokens:', hyphenWords.length);
  console.log('suspicious (grammar-word-led or 3+ parts):', suspicious.length);
  console.log('first 20 suspicious:', suspicious.slice(0, 20));
  ws.close();
}
main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
