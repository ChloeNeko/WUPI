// =============================================================
// WUPI CDP PLAYTEST — fresh player + scenario card setup.
// Creates a brand-new SavedPlayer + a rich scenario <sim_card> from
// scratch via the Creator IPCs (NOT rusty_tavern — fully fresh),
// writes the .intro + .codex siblings, then verifies every file
// landed on disk. This exercises the file-creation path the
// playtest depends on.
// =============================================================

const fs = require('fs');
const path = require('path');

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
  if (r.exceptionDetails) throw new Error('Page eval exception: ' + JSON.stringify(r.exceptionDetails).slice(0, 800));
  return r.result.value;
}
async function invoke(cmd, args) {
  // Serialize the args as a JSON literal so nested objects (player) pass through.
  const argsJson = JSON.stringify(args || {});
  return evalPage(`(async () => {
    const invoke = window.__TAURI__.core.invoke;
    return await invoke(${JSON.stringify(cmd)}, ${argsJson});
  })()`);
}

// ── The fresh test scenario ────────────────────────────────────────────
// A rich world designed to exercise EVERY Fable subsystem over 50 turns:
//  • a walled frontier town (travel graph: 3 nodes + exits) → travel brackets
//  • a corrupt guard captain + a thief guild contact (2 NPCs) → NPC register,
//    relationships, presence, disguise, skill checks
//  • a tavern, a market, a smuggler's warehouse (locations) → scene variety
//  • night setting → weather/scene-pacing variety, rumors
// The player is a down-on-their-luck courier pulled into a smuggling plot.
const PLAYER_ID = 'cdp_courier';
const CARD_STEM = 'cinderfen';

const PLAYER = {
  id: PLAYER_ID,
  name: 'Vera',
  gender: 'female',
  race: 'human',
  age: '24',
  height: "5'7\"",
  weight: '130 lbs',
  hair_color: 'auburn',
  hair_length: 'shoulder-length',
  hair_style: 'tied back',
  body_type: 'lean',
  skin_complexion: 'weathered tan',
  eye_color: 'green',
  breast_size: null,
  ears: null,
  tail: null,
  clothing: ['a worn traveling cloak', 'scuffed leather boots', 'a courier\'s satchel'],
  portrait: null,
  created_at_ms: 0,
};

// Flat-format <sim_card> XML. CDATA wraps all prose. metadata/type=roleplay
// is REQUIRED for the card to appear in the New Game picker.
const CARD_XML = `<sim_card>
  <metadata><type>roleplay</type><id>cinderfen</id></metadata>
  <identity>
    <name>Cinderfen</name>
    <persona><![CDATA[
Cinderfen is a mud-and-timber frontier town huddled at the marshy edge of the Ashen Mire. A generation ago a peat-fire burned slow beneath the fen, and the town still smells of smoke and sulfur. The Reeve's Guard keeps a stiff peace; smugglers move glass vials of "mire-oil" through the warehouse district by night. The locals are superstitious, clannish, and distrustful of outlanders. Money talks; questions don't.
]]></persona>
  </identity>
  <setting><![CDATA[
A fogbound marsh-edge town of cobbled lanes, leaning houses, and lantern-lit taverns. The Ashen Mire stretches east — treacherous bog, rumor-haunted. West lies the King's Road back toward civilization. Key places: the Crooked Lantern tavern (a smugglers' haunt), the Market Square (hawkers and cutpurses), and the Reeve's warehouse on the docks (where mire-oil changes hands).
]]></setting>
  <plot><![CDATA[
The player is Vera, an indebted courier stranded in Cinderfen when her contact fails to pay. To clear her debts she gets pulled into a smuggling job: move a sealed crate of mire-oil from the warehouse to a buyer at the Crooked Lantern. The corrupt Guard Captain Harsk wants the oil for himself; the thieves' guild contact Mara wants it for a rival buyer. Vera is caught between them. Choices have weight: trust the wrong person and the Guard cracks down; trust the right one and a path out of debt opens. Violence has real, lasting consequences.
]]></plot>
  <tone><![CDATA[
Gritty low-fantasy noir. Grounded, atmospheric, morally grey. Tension over spectacle. Every action carries risk; wounds linger, trust is scarce, and the world moves on its own whether Vera is watching or not. Prose is vivid and sensory, third-person, past tense. No omniscience — show only what Vera can perceive. Let dice and consequence decide outcomes, never author fiat.
]]></tone>
  <player_name>Vera</player_name>
  <locations>
    <node id="crooked_lantern">
      <name>The Crooked Lantern</name>
      <neighbors>market_square</neighbors>
      <setting>tavern</setting>
    </node>
    <node id="market_square">
      <name>Market Square</name>
      <neighbors>crooked_lantern,warehouse_docks</neighbors>
      <setting>market</setting>
    </node>
    <node id="warehouse_docks">
      <name>Warehouse Docks</name>
      <neighbors>market_square</neighbors>
      <setting>docks</setting>
    </node>
  </locations>
  <cast>
    <npc id="harsk">
      <name>Captain Harsk</name>
      <role>The corrupt captain of the Reeve's Guard. Greedy, suspicious, quick to violence.</role>
      <tier>elite</tier>
    </npc>
    <npc id="mara">
      <name>Mara</name>
      <role>A thieves' guild contact. Sharp, transactional, loyal only to coin.</role>
      <tier>minion</tier>
    </npc>
  </cast>
</sim_card>`;

const INTRO = `The fog rolls thick off the Ashen Mire, wreathing Cinderfen in a sulfurous haze that clings to your courier's cloak. Three days now you've waited at the Crooked Lantern for a payment that will never come — your contact left word this morning, blunt and final: the debt is yours alone now.

A girl slides onto the bench across from you, uninvited. Dark hair, quick eyes, a smile that doesn't reach them. "You're Vera," she says. Not a question. "I'm Mara. I hear you need coin, and I need someone nobody will miss moving a crate from the warehouse docks. Tonight." She slides a single silver coin across the ale-stained table. "Interested, or should I find someone quieter?"`;

const CODEX = `---
title: Mire-Oil
tags: contraband, item
---

Mire-oil is a thick, iridescent fluid distilled from the Ashen Mire's peat. A vial glows faintly and burns slow. Smugglers prize it; the Reeve's Guard taxes it (or seizes it). Possession without a Guild stamp is a flogging offense in Cinderfen.

---
title: The Reeve's Guard
tags: faction, authority
---

The Reeve's Guard keeps order in Cinderfen under Captain Harsk. Ostensibly upholders of the law, most are bought. They wear soot-darkened tabards and carry cudgels and short swords. Bribery is expected; defiance is punished.

---
title: Cinderfen Law
tags: rule, consequence
---

Blade-work in the open draws the Guard fast and hard. Disguises, bribes, and back-alley deals are how things actually get done. A known thief is watched; a stranger with coin is tolerated. Wounds fester in the marsh damp — untreated injuries worsen.`;

async function main() {
  await connect();
  console.log('=== STEP 1: Create fresh SavedPlayer (Vera) ===');
  try {
    const res = await invoke('fable_player_write', { id: PLAYER_ID, player: PLAYER });
    console.log('  fable_player_write OK:', JSON.stringify(res));
  } catch (e) {
    console.error('  fable_player_write FAILED:', e.message);
  }

  console.log('\n=== STEP 2: Create fresh scenario card (Cinderfen) ===');
  try {
    const res = await invoke('fable_write_card', { stem: CARD_STEM, xml: CARD_XML });
    console.log('  fable_write_card OK:', JSON.stringify(res));
  } catch (e) {
    console.error('  fable_write_card FAILED:', e.message);
  }

  console.log('\n=== STEP 3: Write .intro sibling ===');
  try {
    await invoke('fable_card_sibling_write', { cardId: CARD_STEM, ext: 'intro', text: INTRO });
    console.log('  intro sibling OK');
  } catch (e) {
    console.error('  intro sibling FAILED:', e.message);
  }

  console.log('\n=== STEP 4: Write .codex sibling ===');
  try {
    await invoke('fable_card_sibling_write', { cardId: CARD_STEM, ext: 'codex', text: CODEX });
    console.log('  codex sibling OK');
  } catch (e) {
    console.error('  codex sibling FAILED:', e.message);
  }

  console.log('\n=== STEP 5: Verify files on disk ===');
  // Cards resolve to the exe-local apps dir (first candidate wins per
  // resolve_fable_cards_dir). Check both the exe-local + repo paths.
  const exeApps = 'C:/WUPI/src-tauri/target/release/apps/fable/cards/cinderfen';
  const repoApps = 'C:/WUPI/apps/fable/cards/cinderfen';
  const playersExe = 'C:/WUPI/src-tauri/target/release/apps/fable/players/cdp_courier';
  const playersRepo = 'C:/WUPI/apps/fable/players/cdp_courier';
  for (const dir of [exeApps, repoApps]) {
    console.log(`\n  -- ${dir} --`);
    for (const f of ['cinderfen.sim', 'cinderfen.intro', 'cinderfen.codex']) {
      const p = path.join(dir, f);
      try {
        const sz = fs.statSync(p).size;
        console.log(`    ${f}: ${sz} bytes ✓`);
      } catch { console.log(`    ${f}: MISSING`); }
    }
  }
  for (const dir of [playersExe, playersRepo]) {
    console.log(`\n  -- ${dir} --`);
    try {
      const sz = fs.statSync(path.join(dir, 'cdp_courier.json')).size;
      console.log(`    cdp_courier.json: ${sz} bytes ✓`);
    } catch { console.log(`    cdp_courier.json: MISSING`); }
  }

  console.log('\n=== STEP 6: Confirm card appears in fable_cards_list ===');
  try {
    const cards = await invoke('fable_cards_list', {});
    console.log('  cards:', JSON.stringify(cards, null, 2));
  } catch (e) {
    console.error('  fable_cards_list FAILED:', e.message);
  }

  console.log('\n=== STEP 7: Confirm player appears in fable_players_list ===');
  try {
    const players = await invoke('fable_players_list', {});
    const ours = players.filter(p => p.id === PLAYER_ID);
    console.log('  our player:', JSON.stringify(ours, null, 2));
  } catch (e) {
    console.error('  fable_players_list FAILED:', e.message);
  }

  ws.close();
}

main().catch(e => { console.error('FATAL:', e.message); process.exit(1); });
