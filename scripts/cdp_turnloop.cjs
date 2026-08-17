// =============================================================
// WUPI TURN LOOP — drives the STAGE COMPOSER (real UI path) through
// the 50-turn Cinderfen script, capturing per-turn schema snapshots
// + narrator text into the playtest-results shape so grade_playtest
// .cjs can score it. Resumable: appends to the results JSON, starts
// at results.length. Companion of cdp_wizard_drive.cjs (CDP 9222).
//
// Usage: node scripts/cdp_turnloop.cjs [count]   (default: to 50)
// =============================================================
const fs = require('fs');
const RESULTS = 'C:/WUPI/logs/wizard_playtest_50.json';

// The 50-turn scenario (mirrors cdp_playtest_50.cjs — same world shape:
// Crooked Lantern start, Mara's offer, warehouse docks, Harsk, market).
const TURNS = [
`I study Mara for a long moment, then slide the silver coin off the table and into my palm. "I'm interested," I say, keeping my voice low. "But I want details. What's in the crate, where exactly at the docks, and who's the buyer I'm delivering to?" I order an ale to look casual while she talks.`,
`"So I'm moving mire-oil," I murmur over my ale. "I've heard the Guard here is... flexible. Captain Harsk. Does he take a cut of this kind of work, or do I need to avoid his patrols entirely? I'd rather know which faces to watch for."`,
`I lean closer. "And the warehouse — is it guarded at night? Locked? I need to know what I'm walking into. Locked doors, how many guards, any dogs." I watch her reaction carefully, trying to read whether she's telling me the whole truth or setting me up.`,
`I finish my ale and stand, tucking my courier's satchel close. "Alright. I'll head to the warehouse docks tonight and get the crate. Where exactly do I find it, and is there a way in that won't get me noticed?" I wait for her directions.`,
`Before I leave, I ask one more thing: "If something goes wrong — if the Guard catches me with the crate — what's our story? Am I on my own, or do you have people who can help?" I want to know if she's the kind to cut losses.`,
`I leave the Crooked Lantern and make my way through the fog toward the Market Square, keeping my hood up and my head down. I'm watching for any Guard patrols, trying to get a sense of how heavy their presence is in the streets tonight. I move carefully, not wanting to draw attention.`,
`In the Market Square, the stalls are shuttered for the night. I pause in the shadows of a colonnade, scanning the square for trouble before continuing toward the docks. Two figures loiter near a closed butcher's stall — I try to make out if they're Guard, thieves, or just drunks.`,
`I skirt wide around the two figures and continue toward the warehouse district. The fog thickens near the water. I keep to the alleyways and side paths, moving as quietly as I can — years of courier work taught me how to pass unseen when I need to. I'm trying to stay hidden.`,
`I reach the warehouse docks. The warehouses loom dark against the misty harbor. I spot the one Mara described and approach it slowly, looking for the side entrance she mentioned. I listen for any sounds — voices, footsteps, dogs — before I get too close.`,
`I find the side door. It's locked — a heavy iron padlock on a hasp. I crouch in the shadows and dig a thin pick from the lining of my satchel. I've picked locks like this before. I set to work on it as quietly as I can, glancing over my shoulder every few seconds.`,
`The padlock clicks open. I ease the door wide enough to slip through and ease it shut behind me. Inside, the warehouse is dark, lit only by a faint glow from the mire-oil vials stored somewhere within. I wait for my eyes to adjust, then start searching for the crate Mara described.`,
`I move between stacked crates and barrels, following the faint glow. There it is — a wooden crate with a Guild mark, half-hidden behind a stack of fish-barrels. I approach it and check its weight and size, figuring out how I'm going to carry it back through the fog without being seen.`,
`I decide the full crate is too heavy to carry quietly. I lift the lid and find it packed with straw and glass vials of mire-oil, each glowing faintly. I carefully take out a few vials — enough to fulfill the deal — and wrap them in my cloak, leaving the rest. I pocket one vial separately, just in case.`,
`As I'm wrapping the vials, I hear boots on the dock outside — heavy, deliberate. A lantern's glow sweeps under the warehouse door. My heart pounds. I duck behind the fish-barrels with the wrapped vials, holding my breath, and peer through a gap. Is it the Guard?`,
`The door creaks open. A figure steps in — broad shoulders, a soot-darkened tabard. Captain Harsk himself, lantern in one hand, the other resting on his sword. He's not surprised to find the warehouse occupied. "Vera," he says flatly. "Mara's little courier. Drop the oil. Now." He knows my name.`,
`I stay crouched, mind racing. Harsk knows who I am and what I'm doing. Fighting him head-on would be suicide. I slowly stand, hands open, letting the wrapped vials rest on the barrel. "Captain," I say carefully. "Maybe we can make a better deal than Mara's offering. What's the oil worth to you?" I try to negotiate.`,
`Harsk sneers. "Coin won't save you, courier. The oil's mine by right — this is my warehouse, my district." He steps closer, hand tightening on his sword. I keep talking, trying to buy time and read whether he'll really strike a lone woman over a few vials, or if there's a bribe that'll turn him. "Name a price. Everyone has one."`,
`Seeing negotiation failing, I make a snap decision. I snatch up the wrapped vials and bolt for the side door, shoving a stack of barrels over behind me to slow him down. I hear him curse and give chase. I'm faster, and I know the fog is my friend — I just need to lose him in the alleys.`,
`I sprint into the foggy alleyways, Harsk's boots pounding behind me. I duck left, then right, vaulting a low wall and ducking into a narrow gap between two leaning houses. I press myself flat against the damp wood, barely breathing, listening for his footsteps. Did I lose him?`,
`The footsteps thunder past, then slow, then stop. I hear Harsk cursing, barking orders to someone — maybe a patrol. I wait a full minute in the freezing dark before I dare move. I've lost him, for now, but he knows my face and my name. The Guard will be watching for me.`,
`I make my way back to the Crooked Lantern by a long, winding route, doubling back twice to make sure I'm not followed. The vials clink softly in my cloak. I slip in through the back and find Mara, setting the wrapped mire-oil on the table. "Harsk was waiting for me," I hiss. "He knew my name. You set me up?"`,
`Mara's face tightens, but she doesn't deny it cleanly. "Harsk moves fast. Doesn't mean I set you up — means he's got eyes on my people." She slides a small pouch of coin across the table, less than we agreed. "For the oil. The rest when the heat dies down." I weigh whether to press her for the full amount or take what's offered.`,
`I take the pouch but hold her gaze. "This isn't the deal, Mara. Harsk has my name now. I want the rest of the coin, or I find another buyer for what I know about your operation — and there's a certain captain who'd pay well for that." I bluff, trying to intimidate her into paying up.`,
`Mara eyes me, weighing the threat. I hold my expression steady, hand resting near my satchel strap. After a long moment she produces more coin from somewhere inside her cloak, slaps it down. "Fine. We're square. Now get out of my sight — and out of Cinderfen, if you're smart." I pocket the coin. I've made an enemy tonight.`,
`Before I leave, I order a hot meal and another ale — I haven't eaten properly in days, and the chase left me shaky. While I eat, I think about my options. Harsk wants me. Mara's no real ally. I need a plan to either get out of Cinderfen or get Harsk off my back. I listen to the tavern gossip for any useful rumors.`,
`I approach the tavernkeeper about buying some supplies for the road. I need a good knife, a length of rope, and maybe a cheap hooded cloak to replace the one I used to wrap the vials. What does he have, and what will it cost me? I count out the coin Mara paid me.`,
`I buy what I can afford — a worn belt knife, a coil of rough rope, and a patched traveler's cloak. I strap the knife to my belt, coil the rope over my shoulder, and pull on the new cloak, drawing the hood up. I also order a cheap flask of watered wine for the road and tuck it in my satchel. I feel a little more prepared.`,
`I sit back down and think. Harsk knows my face, but if I change my appearance — different hair, different clothes, maybe some soot on my face to look like a dockworker — I might move through the streets unnoticed. I consider disguising myself, using the new cloak and what's in my satchel.`,
`I smear some lamp-black from a wall sconce under my eyes and across my cheekbones, undo my hair and re-braid it tightly back, and pull the patched hood low. Between the soot, the braid, and the dockhand's cloak, I look like a different woman. I test the disguise by stepping out to the common room and seeing if anyone gives me a second glance.`,
`Disguised as a dockhand, I venture out into the dawn streets of Cinderfen. I keep my head down and walk with the slouch of a tired laborer, heading toward the Market Square to see if the Guard is actively searching for me. I pass a pair of guardsmen on patrol — do they even glance at me?`,
`The guardsmen ignore me — just another tired dockhand. My heart steadies. I move through the Market Square, listening to the morning hawkers and watching for any posted notices or increased patrols. Has the hunt for Vera gone public, or is Harsk keeping it quiet to protect his corruption?`,
`I overhear two merchants arguing about a shipment of mire-oil that "went missing" from the warehouse district last night. So it's not just me — Harsk is hunting for his stolen oil too, and he's keeping it quiet. This is useful. I file the rumor away and keep moving, toward the warehouse docks to see if Mara's contact is still around.`,
`Back near the docks, I spot a man watching the warehouse from across the street — not a guardsman, but someone in plain clothes with the bearing of a hired sword. Mara's buyer? One of Harsk's informants? I observe him from a doorway, trying to figure out who he is and who he works for.`,
`After watching for a while, I slip away before the watcher notices me. I need to find a way out of Cinderfen, or a way to make Harsk back off permanently. I head back toward the Crooked Lantern to find Mara — if she wants to stay in business, she owes me a way out of this mess. The morning is getting on.`,
`As I round a corner near the tavern, someone steps out of an alley and grabs my arm — a guardsman, young, nervous, but his hand is on his cudgel. "You're coming with me, courier. The Captain wants a word." He's alone, but I can hear another pair of boots nearby. I wrench my arm free and shove him hard into the wall.`,
`The young guardsman stumbles but doesn't fall — he swings his cudgel at my head. I duck, but not fast enough; it glances off my shoulder, a hot flare of pain. I yank out my belt knife and slash at him to keep him back, screaming at him to stay away. Where's his backup? I have to end this fast.`,
`I feint left, then drive my shoulder into the guardsman's chest, knocking the wind out of him. He goes down, cudgel clattering away. I hear the other boots getting closer — no time. I kick his cudgel into the gutter and run, clutching my bruised shoulder, the knife still in my hand. I have to find cover fast.`,
`I duck into a tannery reeking of chemicals, pressing myself behind a vat of dye while the second guardsman runs past, shouting. My shoulder throbs — the cudgel hit was solid, and I think it's going to bruise badly. I check myself: nothing broken, but I'm going to be stiff and sore. I catch my breath and wait for the pursuit to pass.`,
`Once the street goes quiet, I slip out of the tannery and make my way, hunched and limping slightly, to an herbalist's stall in the Market Square. I have some coin. I ask the old woman there for something for pain and bruising — a poultice, a salve, anything to keep my shoulder from seizing up. What does she have, and what will it cost?`,
`I buy a small pot of willow-bark salve and a rolled bandage. I find a quiet corner behind the stalls and, gritting my teeth, work the salve into my bruised shoulder as best I can one-handed, then bind it tight with the bandage. It's not proper healing, but it'll keep me moving. I should rest, but I can't afford to.`,
`It's well past midday now. I realize I've been awake and running all night. I'm exhausted, my shoulder aches, and I need somewhere safe to sleep — really sleep, not just doze in an alley. Is there a cheap room at the Crooked Lantern, or somewhere else I could hole up for a few hours without Harsk's men finding me?`,
`I pay for a dingy back room at the Crooked Lantern and lock the door behind me, dragging a chair under the handle. I eat the rest of the food I bought, drink the watered wine, apply more salve, and lie down on the straw mattress. Despite the danger, exhaustion wins — I sleep, hard, for several hours. The day passes into evening.`,
`I wake as dusk falls, groggy but steadier. My shoulder is stiff but the salve helped. I wash my face, re-bandage the bruise, and check the street outside my window. While I slept, what's happened in Cinderfen? I listen at the window to the sounds of the evening — any change in the Guard's activity, any new rumors?`,
`I go downstairs to the common room, still in my dockhand disguise. I order a bowl of stew and an ale, and I listen to the talk around me. Have there been any arrests? Any news of the missing mire-oil? Is Harsk still hunting, or has something else drawn his attention? I gather what intelligence I can from the gossip.`,
`I learn from the gossip that Harsk raided a different smuggler last night — someone he'd been watching for weeks — and the hunt for "the woman who hit his guard" has been pushed down his list. He's still annoyed, but the city hasn't been turned upside down for me. That's a small mercy. I eat my stew and weigh my next move.`,
`I make a decision. Cinderfen is too hot for me now, but I have coin, supplies, and a disguise. I'll leave tonight, on foot, before Harsk's attention swings back. I settle my tab, gather my gear — knife, rope, satchel, the remaining salve, my coin — and prepare to slip out of the town via the King's Road as soon as full dark falls.`,
`Full dark. I pull my hood low, check my disguise one last time, and leave the Crooked Lantern by the back door. I move through the foggy streets toward the western edge of Cinderfen, where the King's Road begins its climb out of the marsh. I'm watching for patrols, but moving with purpose — I want to be past the town gates before midnight.`,
`I pass the last of the leaning houses and reach the edge of the marsh, where the cobblestones give way to the packed earth of the King's Road. The fog thins here, and the stars are finally visible overhead. I pause, looking back at the sulfurous glow of Cinderfen one last time — a town that nearly swallowed me. Then I turn west, toward the road, and walk.`,
`An hour down the King's Road, the road forks. One path leads west, toward the lowlands and the cities beyond — safety, but also the debts that brought me to Cinderfen in the first place. The other leads north, into hill country I don't know, where I could disappear entirely and start over. I stand at the fork in the moonlight, weighing my options. Which way?`,
`I choose the northern road. The debts can wait; the hills offer something Cinderfen never could — a chance to vanish. I adjust my satchel, touch the knife at my belt for reassurance, and set off up the rutted track into the dark hills. Behind me, Cinderfen and Captain Harsk fade into the marsh-fog. Ahead, the unknown. I walk until dawn, and don't look back again.`,
`Dawn breaks pale over the hills. I'm exhausted, stiff, bruised, and lighter in coin than I started — but I'm free. I find a sheltered hollow off the track, eat the last of my road food, and finally allow myself a thin smile. I survived Cinderfen. Whatever comes next in these hills, I'll face it as I faced the marsh-town: one careful step at a time.`,
];

// ---- CDP plumbing (same as cdp_wizard_drive.cjs) ----
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
  if (r.exceptionDetails) throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails).slice(0, 400));
  return r.result.value;
}

// ---- playtest-shaped capture ----
function hygiene(text) {
  const t = text || '';
  return {
    markers: (t.match(/<\|[^|]+\|>/g) || []).length,
    hyphens: (t.match(/-{4,}/g) || []).length,
    fences: (t.match(/```/g) || []).length,
    jsonLead: t.match(/^[}\]{]/m) ? 1 : 0,
    bracketLeak: (t.match(/\[(?:EQUIP|BELT|PACK|TIME|WEATHER|TRAVEL|RUMOR|PRESENCE|NPC|EFFECT|MILESTONE|TASK|APPEARANCE|CHARACTER|FX|OBJECT)\b/i) || []).length,
    length: t.length,
  };
}
function snapshotSchema(s) {
  if (!s) return null;
  const ps = s.player_state || {};
  return {
    clock: s.world_clock ? { min: s.world_clock.current_minutes, tick: s.world_clock.last_tick_minutes } : null,
    calendar: s.calendar || null,
    weather: s.weather ? s.weather.condition : null,
    loc: (s.travel_graph || {}).current_node || null,
    mode: (s.scene_pacing || {}).mode || null,
    statusTags: (s.status_tags || []).map(t => (typeof t === 'object' ? (t.kind || t.label || JSON.stringify(t)) : t)),
    rumors: (s.rumors || []).length,
    presences: (s.presences || []).map(p => p.npc_id + ':' + p.name),
    relationships: (s.relationships || []).length,
    offscreen: (s.offscreen_tasks || []).length,
    entities: Object.keys(s.entities || {}).length,
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

async function runTurn(i, log) {
  const t0 = Date.now();
  const before = await evalPage(STAGE_STATE);
  // Compose + submit via trusted input.
  await evalPage(`(() => { const ta = document.querySelector('.fable-stage textarea.fable-input'); ta.focus(); return true; })()`);
  await cdp('Input.insertText', { text: TURNS[i] });
  await cdp('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });
  await cdp('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 });

  // Wait for completion: beat count +2 (user+assistant), no streaming, composer unlocked.
  const TIMEOUT_MS = 240000;
  let st = null;
  while (true) {
    await new Promise(r => setTimeout(r, 1500));
    st = await evalPage(STAGE_STATE);
    if (st.apiLost) break;
    if (st.n >= before.n + 2 && !st.streaming && !st.readOnly) break;
    if (Date.now() - t0 > TIMEOUT_MS) return { turn: i + 1, dt: ((Date.now() - t0) / 1000).toFixed(1) + 's', timedOut: true, text: st.lastText.slice(0, 200) };
  }
  // Let post-turn work settle (autosave, archival spawns, tick).
  await new Promise(r => setTimeout(r, 1200));
  const after = await evalPage(STAGE_STATE);
  let schema = null;
  try { schema = await evalPage(`(async () => window.__TAURI__.core.invoke('fable_schema_get', {}))()`); } catch (e) { log.push(`[T${i + 1}] schema_get failed: ${e.message}`); }
  const dt = ((Date.now() - t0) / 1000).toFixed(1);
  const text = after.lastText || '';
  const snap = snapshotSchema(schema);
  const result = {
    turn: i + 1, dt: dt + 's',
    error: st.apiLost ? 'api_lost' : null,
    timedOut: false,
    nBeats: after.n,
    text,
    hygiene: hygiene(text),
    schema: snap,
  };
  log.push(`[T${i + 1}] ${dt}s | ${text.length}c | loc:${snap ? snap.loc : '?'} clock:${snap && snap.clock ? snap.clock.min : '?'} mode:${snap ? snap.mode : '?'} | belt:[${snap ? snap.player.belt.join(',') : ''}] pack:${snap ? snap.player.pack.length : '?'} inj:[${snap ? snap.player.injured.join(',') : ''}]${st.apiLost ? ' ⚠API_LOST' : ''}`);
  return result;
}

async function main() {
  const count = parseInt(process.argv[2], 10) || (50 - (fs.existsSync(RESULTS) ? JSON.parse(fs.readFileSync(RESULTS, 'utf8')).length : 0));
  let results = fs.existsSync(RESULTS) ? JSON.parse(fs.readFileSync(RESULTS, 'utf8')) : [];
  const start = results.length;
  const log = [];
  console.log(`TURNLOOP: running turns ${start + 1}..${start + count} of ${TURNS.length}`);
  await connect();
  for (let k = 0; k < count && start + k < TURNS.length; k++) {
    const i = start + k;
    try {
      const r = await runTurn(i, log);
      results.push(r);
    } catch (e) {
      results.push({ turn: i + 1, fatal: e.message });
      log.push(`[T${i + 1}] FATAL: ${e.message}`);
    }
    fs.writeFileSync(RESULTS, JSON.stringify(results, null, 1));
    log.forEach(l => console.log(l)); log.length = 0;
    if (results[results.length - 1].error === 'api_lost') {
      console.log('API LOST — aborting loop (composer locked). Partial results saved.');
      break;
    }
  }
  console.log(`DONE: ${results.length}/${TURNS.length} turns captured -> ${RESULTS}`);
  ws.close();
}
main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
