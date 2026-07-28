// Terminal-only CDP playtest harness for the running WUPI Tauri app.
// Talks to the WebView2 page already exposed on localhost:9222 — NO browser launch.
// Usage: node scripts/cdp_playtest.cjs <subcommand> [args]
//
// Subcommands:
//   ping           - confirm connection + dump app visible state
//   list-ipc       - enumerate registered Tauri IPC commands (best effort)
//   eval <code>    - run an arbitrary JS expression in the page, print result
//   invoke <cmd>   - call a Tauri IPC by name (args via stdin JSON)
//   feed           - take a JSON {cmd, args} from stdin and invoke
//
// The Tauri IPC bridge is window.__TAURI_INTERNALS__.invoke(cmd, args).
// On newer Tauri 2 builds window.__TAURI__.core.invoke also exists.

const target_url = 'http://localhost:9222/json/list';

async function findTarget() {
  const r = await fetch(target_url);
  const targets = await r.json();
  const page = targets.find(t => t.type === 'page' && /wupi\.html/.test(t.url));
  if (!page) throw new Error('No wupi.html target. Targets: ' + JSON.stringify(targets.map(t => t.url)));
  return page.webSocketDebuggerUrl;
}

let ws, idc = 0;
const pending = new Map();

async function connect() {
  const url = await findTarget();
  return new Promise((resolve, reject) => {
    ws = new WebSocket(url);
    ws.addEventListener('open', () => resolve());
    ws.addEventListener('error', e => reject(new Error('ws error')));
    ws.addEventListener('message', ev => {
      const msg = JSON.parse(typeof ev.data === 'string' ? ev.data : ev.data.toString());
      if (msg.id && pending.has(msg.id)) {
        const {ok, err} = pending.get(msg.id);
        pending.delete(msg.id);
        if (msg.error) err(new Error(JSON.stringify(msg.error)));
        else ok(msg.result);
      }
    });
  });
}

function cdp(method, params = {}) {
  const id = ++idc;
  return new Promise((ok, err) => {
    pending.set(id, {ok, err});
    ws.send(JSON.stringify({id, method, params}));
  });
}

async function evalInPage(expression, awaitPromise = true, returnByValue = true) {
  const r = await cdp('Runtime.evaluate', {
    expression,
    awaitPromise,
    returnByValue,
    // Don't allow side-effect-free reads to be optimized out.
    includeCommandLineAPI: true,
  });
  if (r.exceptionDetails) {
    throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails));
  }
  return r.result.value;
}

async function main() {
  const [, , cmd, ...rest] = process.argv;
  await connect();
  try {
    if (cmd === 'ping') {
      const visible = await evalInPage(`document.body.innerText.slice(0, 2000)`);
      console.log('=== CONNECTED. App visible text (first 2000 chars) ===');
      console.log(visible);
    } else if (cmd === 'eval') {
      const code = rest.join(' ');
      const result = await evalInPage(code);
      console.log(JSON.stringify(result, null, 2));
    } else if (cmd === 'list-ipc') {
      const result = await evalInPage(`
        (function(){
          const out = { has_tauri_internals: !!window.__TAURI_INTERNALS__,
                        has_tauri: !!window.__TAURI__,
                        internals_keys: window.__TAURI_INTERNALS__ ? Object.keys(window.__TAURI_INTERNALS__) : null,
                        tauri_keys: window.__TAURI__ ? Object.keys(window.__TAURI__) : null };
          if (window.__TAURI__ && window.__TAURI__.core) out.core_keys = Object.keys(window.__TAURI__.core);
          return out;
        })()
      `);
      console.log(JSON.stringify(result, null, 2));
    } else if (cmd === 'invoke') {
      const ipcCmd = rest[0];
      let args = {};
      if (rest[1]) {
        try { args = JSON.parse(rest[1]); } catch (e) {
          // Read from stdin instead.
          args = JSON.parse(await readStdin());
        }
      }
      const result = await evalInPage(`
        (async () => {
          const invoke = (window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke)
                      || (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke);
          if (!invoke) throw new Error('No Tauri invoke bridge found');
          return await invoke(${JSON.stringify(ipcCmd)}, ${JSON.stringify(args)});
        })()
      `);
      console.log(JSON.stringify(result, null, 2));
    } else {
      console.error('Unknown subcommand:', cmd);
      process.exit(2);
    }
  } finally {
    ws.close();
  }
}

function readStdin() {
  return new Promise((resolve, reject) => {
    let data = '';
    process.stdin.setEncoding('utf8');
    process.stdin.on('data', c => data += c);
    process.stdin.on('end', () => resolve(data));
    process.stdin.on('error', reject);
  });
}

main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
