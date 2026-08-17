// =============================================================
// WUPI E4B SHAKEDOWN FIX VERIFICATION (2026-08-17) — drives the REAL
// frontend over CDP 9222 to verify docs/e4b-shakedown-fix-plan.md in a
// live session continued from the cinderfen autosave (via the card .lnk
// direct-launch path).
//
// Commands:
//   probe                      hash/screens/API gate/composer/drawer/overlay
//   shot <name>                screenshot -> logs/e4b_<name>.jpg
//   eval <expr>                evaluate JS in page, print JSON
//   invoke <cmd> [jsonArgs]    read-only Tauri IPC
//   schema                     current schema snapshot (fix-relevant slice)
//   gemrect                    soul-gem cluster rects (P2a offscreen check)
//   openDrawer / closeDrawer   drive the corner-dwell / close btn
//   chat <text>                Wupi drawer turn (P0 local chat + manager)
//   turn <label> <text...>     composer turn + before/after schema diff
//                              (appends to logs/e4b_verify_turns.json)
// Results JSON: logs/e4b_verify_turns.json
// =============================================================
const fs = require('fs');
const path = require('path');
const LOGS = 'C:/WUPI/logs';
const RESULTS = path.join(LOGS, 'e4b_verify_turns.json');

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
  return evalPage(`(async()=>{return await window.__TAURI__.core.invoke(${JSON.stringify(cmd)}, ${JSON.stringify(args || {})})})()`);
}
const sleep = ms => new Promise(r => setTimeout(r, ms));
async function shot(name) {
  await cdp('Page.enable');
  const r = await cdp('Page.captureScreenshot', { format: 'jpeg', quality: 60 });
  const p = path.join(LOGS, `e4b_${name}.jpg`);
  fs.writeFileSync(p, Buffer.from(r.data, 'base64'));
  console.log('SHOT ' + p);
}

// ---- schema snapshot (fix-relevant slice; mirrors cdp_turnloop) ----
const DISGUISE_TERMS = ['pose','uniform','costume','garb','attire','outfit','mask','cloak','veil','persona','identity','guise','semblance','disguise'];
function snapshotSchema(s) {
  if (!s) return null;
  const ps = s.player_state || {};
  const tags = (s.status_tags || []).map(t => ({ label: t.label, kind: t.kind || '', polarity: t.polarity }));
  return {
    clock: s.world_clock ? { min: s.world_clock.current_minutes } : null,
    calendar: s.calendar || null,
    weather: s.weather ? s.weather.condition : null,
    loc: (s.travel_graph || {}).current_node || null,
    travelNodes: Object.keys(((s.travel_graph || {}).nodes) || {}).length,
    mode: (s.scene_pacing || {}).mode || null,
    tags,
    presences: (s.presences || []).map(p => p.npc_id + ':' + p.name),
    player: {
      wealth: ps.wealth, stamina: ps.stamina,
      injured: ps.body ? Object.entries(ps.body).filter(([_, v]) => v && v !== 'Transparent').map(([k, v]) => k + '=' + v) : [],
      belt: (ps.belt || []).map(i => i.name + 'x' + i.qty),
      pack: (ps.pack || []).map(i => i.name + 'x' + i.qty),
      equip: Object.keys(ps.equipment || {}),
    },
  };
}
async function getSchema() {
  try { return snapshotSchema(await invoke('fable_schema_get', {})); }
  catch (e) { console.error('schema_get failed: ' + e.message); return null; }
}

// ---- fix assertions over a before/after pair ----
function cleanName(n) { return String(n || '').trim().replace(/^["'“”‘’]+|["'“”‘’]+$/g, '').replace(/"+/g, '"').trim().toLowerCase(); }
function diffTurn(label, before, after, text) {
  const findings = [];
  if (!before || !after) return { label, findings: ['SCHEMA MISSING'], before, after };
  const dClock = after.clock ? after.clock.min - before.clock.min : null;
  if (dClock !== null && Math.abs(dClock) > 1440) findings.push(`P1d: clock jumped ${(dClock / 1440).toFixed(1)}d in ONE turn (> 24h Downtime cap)`);
  const injB = new Set(before.player.injured), injA = new Set(after.player.injured);
  const newInj = after.player.injured.filter(x => !injB.has(x));
  const healed = before.player.injured.filter(x => !injA.has(x));
  // P1a: quote-corrupted or fragment names among NEW items
  const itemsB = new Set([...before.player.belt, ...before.player.pack].map(cleanName));
  const newItems = [...after.player.belt, ...after.player.pack].filter(n => !itemsB.has(cleanName(n)));
  for (const raw of newItems) {
    const n = raw.replace(/x\d+$/, '');
    if (/^["'“”‘’]/.test(n) || /["'“”‘’]$/.test(n)) findings.push(`P1a: quoted-edge item name "${raw}"`);
    if (n.replace(/["'“”‘’]/g, '').trim().length <= 2) findings.push(`P1a: fragment item name "${raw}"`);
  }
  // P1a: duplicates differing only by quotes/case (merge miss)
  const all = [...after.player.belt, ...after.player.pack];
  const seen = new Map();
  for (const raw of all) {
    const k = cleanName(raw);
    if (seen.has(k) && seen.get(k) !== raw) findings.push(`P1a: unmerged duplicate "${seen.get(k)}" vs "${raw}"`);
    if (!seen.has(k)) seen.set(k, raw);
  }
  // P1b: NEW tags carrying a kind
  const tagB = new Set(before.tags.map(t => t.label + '|' + t.kind));
  for (const t of after.tags) {
    if (tagB.has(t.label + '|' + t.kind)) continue;
    if (t.kind && t.kind !== 'disguise') findings.push(`P1b: new tag kind "${t.kind}" (unapproved domain) on "${t.label}"`);
    if (t.kind === 'disguise' && !DISGUISE_TERMS.some(w => (t.label || '').toLowerCase().includes(w)))
      findings.push(`P1b: kind=disguise kept on non-guise label "${t.label}"`);
  }
  return {
    label, mode: after.mode, loc: after.loc,
    clockBefore: before.clock ? before.clock.min : null,
    clockAfter: after.clock ? after.clock.min : null, dClock,
    calendarBefore: before.calendar, calendarAfter: after.calendar,
    newInjuries: newInj, healed,
    tagsNew: after.tags.filter(t => !before.tags.some(b => b.label === t.label && b.kind === t.kind)).map(t => `${t.label}${t.kind ? '/' + t.kind : ''}`),
    newItems, findings, textLen: (text || '').length,
  };
}

// ---- stage state (composer path, mirrors cdp_turnloop) ----
const STAGE_STATE = `(() => {
  const ta = document.querySelector('.fable-stage textarea.fable-input');
  const beats = [...document.querySelectorAll('.fable-mes')];
  return {
    n: beats.length,
    streaming: !!document.querySelector('.fable-mes.streaming'),
    readOnly: ta ? ta.readOnly : null,
    apiLost: /API LOST/i.test((document.querySelector('.fable-stage')||{textContent:''}).textContent),
    lastText: beats.length ? beats[beats.length-1].textContent : '',
  };
})()`;

async function runComposerTurn(label, text) {
  const t0 = Date.now();
  const before = await getSchema();
  const st0 = await evalPage(STAGE_STATE);
  await evalPage(`(() => { const ta = document.querySelector('.fable-stage textarea.fable-input'); ta.focus(); return true; })()`);
  await cdp('Input.insertText', { text });
  await cdp('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  const TIMEOUT_MS = 240000;
  let st = null;
  while (true) {
    await sleep(1500);
    st = await evalPage(STAGE_STATE);
    if (st.apiLost) break;
    if (st.n >= st0.n + 2 && !st.streaming && !st.readOnly) break;
    if (Date.now() - t0 > TIMEOUT_MS) break;
  }
  await sleep(1500); // post-turn settle (autosave, archival, deferred tick)
  const after = await getSchema();
  const stEnd = await evalPage(STAGE_STATE);
  const rec = diffTurn(label, before, after, stEnd.lastText);
  rec.dt = ((Date.now() - t0) / 1000).toFixed(1) + 's';
  rec.error = st.apiLost ? 'api_lost' : (Date.now() - t0 > TIMEOUT_MS ? 'timeout' : null);
  rec.nBeats = stEnd.n;
  rec.beatText = (stEnd.lastText || '').slice(0, 400);
  const results = fs.existsSync(RESULTS) ? JSON.parse(fs.readFileSync(RESULTS, 'utf8')) : [];
  results.push(rec);
  fs.writeFileSync(RESULTS, JSON.stringify(results, null, 1));
  console.log(JSON.stringify(rec, null, 1));
  return rec;
}

// ---- drawer chat (P0) ----
async function openDrawer() {
  await evalPage(`(() => { const c = document.querySelector('[data-corner-trigger]'); if (c) c.dispatchEvent(new MouseEvent('mouseenter')); return !!c; })()`);
  await sleep(450);
  const open = await evalPage(`(() => { const d = document.querySelector('[data-wupi-drawer]'); const i = document.querySelector('[data-wupi-input]'); return { cls: d ? d.className : null, inputVisible: !!(i && i.getBoundingClientRect().width > 0) }; })()`);
  console.log(JSON.stringify(open));
  return open.inputVisible;
}
async function drawerChat(text) {
  const okOpen = await openDrawer();
  if (!okOpen) { console.error('drawer did not open'); process.exit(2); }
  const n0 = await evalPage(`(() => document.querySelectorAll('[data-wupi-messages] > *').length)()`);
  await evalPage(`(() => { const i = document.querySelector('[data-wupi-input]'); i.focus(); return true; })()`);
  await cdp('Input.insertText', { text });
  await cdp('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  const t0 = Date.now();
  let n = n0, stable = 0, lastLen = -1;
  while (Date.now() - t0 < 120000) {
    await sleep(1200);
    const st = await evalPage(`(() => { const m = document.querySelectorAll('[data-wupi-messages] > *'); const last = m[m.length-1]; return { n: m.length, len: last ? last.textContent.length : 0 }; })()`);
    if (st.n > n0 + 1) break;                       // 2+ new bubbles = replied + next? (safety)
    if (st.n === n0 + 1) { if (st.len === lastLen) stable++; else stable = 0; lastLen = st.len; if (stable >= 3) break; }
    n = st.n;
  }
  const reply = await evalPage(`(() => { const m = document.querySelectorAll('[data-wupi-messages] > *'); const arr = [...m]; return (arr[arr.length-1] || {textContent:''}).textContent.slice(0, 600); })()`);
  console.log(JSON.stringify({ bubbles: n - n0, reply }, null, 1));
}

async function main() {
  const [cmd, ...args] = process.argv.slice(2);
  await connect();
  switch (cmd) {
    case 'probe': {
      const info = await evalPage(`(() => {
        const vis = [...document.querySelectorAll('[class*="screen"], section, main')].filter(e => e.getBoundingClientRect().width > 0 && e.getBoundingClientRect().height > 0).map(e => e.className || e.tagName).slice(0, 12);
        const ov = document.getElementById('download-overlay');
        const gems = [...document.querySelectorAll('[data-soul-gem]')].map(g => { const r = g.getBoundingClientRect(); return { id: g.getAttribute('data-soul-gem'), x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width) }; });
        const bp = document.querySelector('.hud-backpack'); const br = bp ? bp.getBoundingClientRect() : null;
        return {
          hash: location.hash, href: location.href.slice(-60), visible: vis,
          apiGate: document.body.textContent.includes('API NOT CONNECTED'),
          overlayDisplay: ov ? getComputedStyle(ov).display : 'ABSENT',
          overlayClass: ov ? ov.className : null,
          beats: document.querySelectorAll('.fable-mes').length,
          gems, backpack: br ? { x: Math.round(br.x), y: Math.round(br.y) } : null,
          vw: innerWidth, vh: innerHeight,
        };
      })()`);
      console.log(JSON.stringify(info, null, 1));
      break;
    }
    case 'shot': await shot(args[0] || 'x'); break;
    case 'eval': console.log(JSON.stringify(await evalPage(args[0]))); break;
    case 'invoke': console.log(JSON.stringify(await invoke(args[0], args[1] ? JSON.parse(args[1]) : {}))); break;
    case 'schema': console.log(JSON.stringify(await getSchema(), null, 1)); break;
    case 'gemrect': {
      const r = await evalPage(`(() => {
        const gems = [...document.querySelectorAll('[data-soul-gem]')].map(g => { const b = g.getBoundingClientRect(); return { id: g.getAttribute('data-soul-gem'), x: Math.round(b.x), y: Math.round(b.y), w: Math.round(b.width), h: Math.round(b.height) }; });
        const bp = document.querySelector('.hud-backpack'); const bb = bp ? bp.getBoundingClientRect() : null;
        const panel = document.querySelector('.soul-gem-overlay'); const pb = panel ? panel.getBoundingClientRect() : null;
        return { vw: innerWidth, gems, backpack: bb ? { x: Math.round(bb.x), y: Math.round(bb.y), w: Math.round(bb.width) } : null, panel: pb ? { x: Math.round(pb.x), y: Math.round(pb.y), w: Math.round(pb.width) } : null,
          offscreen: gems.filter(g => g.x < 0 || g.x + g.w > innerWidth).length };
      })()`);
      console.log(JSON.stringify(r, null, 1));
      await shot('gemrect');
      break;
    }
    case 'openDrawer': await openDrawer(); await shot('drawer'); break;
    case 'closeDrawer':
      await evalPage(`(() => { const b = document.querySelector('[data-wupi-close], [data-wupi-drawer] [aria-label="Close"]'); if (b) b.click(); return !!b; })()`);
      await sleep(600);
      console.log('close clicked');
      break;
    case 'chat': await drawerChat(args.join(' ')); break;
    case 'turn': await runComposerTurn(args[0], args.slice(1).join(' ')); break;
    default: console.error('unknown command: ' + cmd); process.exit(1);
  }
  ws.close();
}
main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
