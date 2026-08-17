// =============================================================
// WUPI CDP UI DRIVER — clicks/types/waits in the REAL frontend.
// Purpose-built for the 2026-08-17 wizard walkthrough + playtest:
// every interaction goes through the actual DOM (shell-guard,
// composer keydown handlers, creator chat), never shortcut IPCs
// for setup. Reconnects per invocation; the app holds all state.
//
// Usage: node cdp_wizard_drive.cjs <command> [args...]
//   probe                      page/hash/visible screens + API signal
//   shot <name>                screenshot -> logs/wiz_<name>.png
//   eval <expr>                evaluate JS in page, print JSON
//   click <sel> [containsText] click first visible match
//   clickall <sel> [contains]  click every visible match, 350ms apart
//   type <sel> <text>          set value + input event
//   press <key> [sel]          KeyboardEvent on sel or activeElement
//   wait <timeoutMs> <expr>    poll predicate expr until truthy
//   invoke <cmd> [jsonArgs]    Tauri IPC (read-only inspection only)
// =============================================================
const fs = require('fs');
const path = require('path');
const LOGS = 'C:/WUPI/logs';

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
  if (r.exceptionDetails) throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails).slice(0, 600));
  return r.result.value;
}

// Page-side helpers injected per call (stateless).
const visible = `((el)=>{const r=el.getBoundingClientRect();return r.width>0&&r.height>0&&r.bottom>-1&&r.top<innerHeight+400&&getComputedStyle(el).visibility!=='hidden'})`;
const CLICK_FN = `(sel, contains) => {
  const els = [...document.querySelectorAll(sel)];
  const hit = els.filter(e => ${visible}(e) && (!contains || (e.textContent||'').trim().includes(contains)));
  if (!hit.length) return { ok:false, n:els.length };
  hit[0].scrollIntoView({ block:'center' });
  hit[0].dispatchEvent(new PointerEvent('pointerdown', { bubbles:true }));
  hit[0].click();
  return { ok:true, text:(hit[0].textContent||'').trim().slice(0,80) };
}`;

async function shot(name) {
  await cdp('Page.enable');
  const r = await cdp('Page.captureScreenshot', { format: 'jpeg', quality: 55 });
  const p = path.join(LOGS, `wiz_${name}.jpg`);
  fs.writeFileSync(p, Buffer.from(r.data, 'base64'));
  console.log('SHOT ' + p);
}

async function main() {
  const [cmd, ...args] = process.argv.slice(2);
  await connect();
  switch (cmd) {
    case 'probe': {
      const info = await evalPage(`(() => {
        const vis = [...document.querySelectorAll('[class*="screen"], section, main')].filter(e => ${visible}(e)).map(e => e.className || e.tagName).slice(0, 12);
        const api = document.body.textContent.includes('API NOT CONNECTED');
        return { hash: location.hash, title: document.title, visible: vis, apiGateLabel: api };
      })()`);
      console.log(JSON.stringify(info, null, 1));
      await shot('probe');
      break;
    }
    case 'shot': await shot(args[0] || 'x'); break;
    case 'eval': console.log(JSON.stringify(await evalPage(args[0]))); break;
    case 'click': {
      const r = await evalPage(`(${CLICK_FN})(${JSON.stringify(args[0])}, ${args[1] ? JSON.stringify(args[1]) : 'null'})`);
      console.log(JSON.stringify(r));
      if (!r.ok) process.exitCode = 2;
      break;
    }
    case 'type': {
      const r = await evalPage(`(() => {
        const el = [...document.querySelectorAll(${JSON.stringify(args[0])})].find(e => ${visible}(e));
        if (!el) return { ok:false };
        el.focus();
        const set = Object.getOwnPropertyDescriptor(el.constructor.prototype, 'value').set;
        set.call(el, ${JSON.stringify(args[1])});
        el.dispatchEvent(new Event('input', { bubbles:true }));
        return { ok:true, len: el.value.length };
      })()`);
      console.log(JSON.stringify(r));
      if (!r.ok) process.exitCode = 2;
      break;
    }
    case 'press': {
      const r = await evalPage(`(() => {
        const el = ${args[1] ? `[...document.querySelectorAll(${JSON.stringify(args[1])})].find(e => ${visible}(e))` : 'document.activeElement'};
        if (!el) return { ok:false };
        el.dispatchEvent(new KeyboardEvent('keydown', { key: ${JSON.stringify(args[0])}, bubbles:true, cancelable:true }));
        el.dispatchEvent(new KeyboardEvent('keyup', { key: ${JSON.stringify(args[0])}, bubbles:true }));
        return { ok:true, tag: el.tagName };
      })()`);
      console.log(JSON.stringify(r));
      break;
    }
    case 'wait': {
      const timeout = parseInt(args[0], 10) || 30000;
      const expr = args[1];
      const t0 = Date.now();
      while (true) {
        const v = await evalPage(`(()=>{ try { return Boolean((${expr})) } catch(e){ return false } })()`);
        if (v) { console.log(`WAIT ok ${((Date.now()-t0)/1000).toFixed(1)}s`); break; }
        if (Date.now() - t0 > timeout) { console.error(`WAIT TIMEOUT ${timeout}ms: ${expr.slice(0,120)}`); process.exit(3); }
        await new Promise(r => setTimeout(r, 350));
      }
      break;
    }
    case 'itype': {
      // Trusted text input: focus el, set value natively, insert via CDP
      // (isTrusted input events — for composers that ignore synthetic keys).
      const focus = await evalPage(`(() => { const el=[...document.querySelectorAll(${JSON.stringify(args[0])})].find(e=>e.getBoundingClientRect().height>0); if(!el) return false; el.focus(); const set=Object.getOwnPropertyDescriptor(el.constructor.prototype,'value').set; set.call(el,''); el.dispatchEvent(new Event('input',{bubbles:true})); return true; })()`);
      if (!focus) { console.error('itype: no visible element ' + args[0]); process.exit(2); break; }
      await cdp('Input.insertText', { text: args[1] });
      const len = await evalPage(`((document.activeElement)||{}).value ? document.activeElement.value.length : -1`);
      console.log(JSON.stringify({ ok: len > 0, len }));
      break;
    }
    case 'ienter': {
      const focus = await evalPage(`(() => { const el=[...document.querySelectorAll(${JSON.stringify(args[0])})].find(e=>e.getBoundingClientRect().height>0); if(!el) return false; el.focus(); return true; })()`);
      if (!focus) { console.error('ienter: no visible element ' + args[0]); process.exit(2); break; }
      await cdp('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 });
      await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 });
      console.log('ienter ok');
      break;
    }
    case 'mhover': { // mhover <x> <y>
      await cdp('Input.dispatchMouseEvent', { type: 'mouseMoved', x: +args[0], y: +args[1] });
      await new Promise(r => setTimeout(r, 250));
      console.log('mhover ok');
      break;
    }
    case 'mclick': { // mclick <x> <y>
      await cdp('Input.dispatchMouseEvent', { type: 'mouseMoved', x: +args[0], y: +args[1] });
      await new Promise(r => setTimeout(r, 150));
      await cdp('Input.dispatchMouseEvent', { type: 'mousePressed', x: +args[0], y: +args[1], button: 'left', clickCount: 1 });
      await cdp('Input.dispatchMouseEvent', { type: 'mouseReleased', x: +args[0], y: +args[1], button: 'left', clickCount: 1 });
      console.log('mclick ok');
      break;
    }
    case 'invoke': {
      const v = await evalPage(`(async()=>{ return await window.__TAURI__.core.invoke(${JSON.stringify(args[0])}, ${args[1] || '{}'}) })()`);
      console.log(JSON.stringify(v));
      break;
    }
    default: console.error('unknown command: ' + cmd); process.exit(1);
  }
  ws.close();
}
main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
