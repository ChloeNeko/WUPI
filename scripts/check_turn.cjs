// Quick inspector: dumps the last assistant message from the rusty_tavern
// autosave + runs hygiene checks (markers, hyphens, raw JSON/fence fragments).
const fs = require('fs');
const card = process.argv[2] || 'rusty_tavern';
const path = `C:/WUPI/src-tauri/target/debug/apps/fable/saves/${card}/autosave.json`;
try {
  const d = JSON.parse(fs.readFileSync(path, 'utf8'));
  const msgs = (d.session && d.session.messages) || [];
  console.log('=== messages:', msgs.length, '===');
  const last = msgs[msgs.length - 1];
  if (!last || last.role !== 'assistant') {
    console.log('NO assistant message found. Last msg role:', last && last.role);
    process.exit(0);
  }
  const c = last.content || '';
  console.log('ASSISTANT length:', c.length, 'chars');
  console.log('--- FULL TEXT ---');
  console.log(c);
  console.log('--- END ---');
  const markers = (c.match(/<\|[^|]+\|>/g) || []);
  const hyphenRuns = (c.match(/-{4,}/g) || []);
  const fenceFrag = (c.match(/```/g) || []);
  const jsonFrag = (c.match(/^[}\]{]/m) ? ['leading-brace'] : []);
  console.log('');
  console.log('=== HYGIENE ===');
  console.log('protocol markers leaked:', markers.length, markers.slice(0, 3));
  console.log('long hyphen runs (4+):', hyphenRuns.length);
  console.log('fence fragments (```):', fenceFrag.length);
  console.log('raw json leading brace:', jsonFrag.length);
  // schema state
  const ws = d.schema || d.world_schema || {};
  console.log('');
  console.log('=== SCHEMA ===');
  console.log('travel_graph current:', ws.travel_graph && ws.travel_graph.current_node,
    '| nodes:', (ws.travel_graph && ws.travel_graph.nodes || []).length);
  console.log('weather:', JSON.stringify(ws.weather));
  console.log('rumors:', (ws.rumors || []).length);
  if (ws.rumors && ws.rumors.length) console.log('  rumor[0]:', JSON.stringify(ws.rumors[0]));
  console.log('status_tags:', JSON.stringify(ws.status_tags));
  console.log('scene_pacing:', JSON.stringify(ws.scene_pacing));
  console.log('clock:', JSON.stringify(ws.world_clock));
  console.log('entities:', Object.keys(ws.entities || {}).length, 'keys');
} catch (e) {
  console.error('ERR:', e.message);
  process.exit(1);
}
