// Terminal-only inspector: reads the rusty_tavern autosave and prints the
// Phase 4 schema fields (weather, travel_graph, rumors, status_tags,
// scene_pacing, world_clock, offscreen_tasks, relationships). Used after
// each CDP fable turn to verify Phase 4 component state mutated correctly.
//
// Usage: node scripts/inspect_schema.cjs [save_name]
//   save_name defaults to "autosave"
const fs = require('fs');
const saveName = process.argv[2] || 'autosave';
const path = `C:/WUPI/src-tauri/target/debug/apps/fable/saves/rusty_tavern/${saveName}.json`;

try {
  const d = JSON.parse(fs.readFileSync(path, 'utf8'));
  const ws = d.schema || d.world_schema || {};
  console.log(`=== ${saveName} schema (Phase 4 fields) ===`);
  console.log('  clock       :', JSON.stringify(ws.world_clock));
  console.log('  weather     :', JSON.stringify(ws.weather));
  console.log('  travel_graph:', JSON.stringify(ws.travel_graph));
  console.log('  rumors      :', JSON.stringify(ws.rumors));
  // Phase 5A: NPC presence whitelist (the anti-hallucination gate). Compact
  // form so a long stance list stays readable.
  const presences = (ws.presences || []).map(p => `${p.npc_id}: ${p.name} ("${(p.stance||'').slice(0,60)}") ttl=${p.ttl}`);
  console.log('  presences   :', JSON.stringify(presences));
  console.log('  status_tags :', JSON.stringify(ws.status_tags));
  console.log('  scene_pacing:', JSON.stringify(ws.scene_pacing));
  console.log('  offscreen   :', JSON.stringify(ws.offscreen_tasks));
  console.log('  relationships:', JSON.stringify(ws.relationships));
  console.log('  entities    :', Object.keys(ws.entities || {}).length, 'keys');
  // Player state
  const ps = ws.player_state || d.player_state || {};
  console.log('  player.wealth:', ps.wealth, 'stamina:', ps.stamina, 'reputation:', ps.reputation);
  // Session turn count
  const sess = d.session || {};
  console.log('  session.turns:', (sess.messages || []).length, 'messages');
  console.log('  meta.turn_count:', d.turn_count !== undefined ? d.turn_count : 'n/a');
} catch (e) {
  console.error('INSPECT ERROR:', e.message);
  process.exit(1);
}
