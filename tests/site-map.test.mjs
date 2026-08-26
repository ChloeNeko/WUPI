// Unit tests for the fog-of-war site map's pure surfaces (2026-08-23;
// REWRITTEN 2026-08-25 for the location-card redesign — the
// location-ui-scenarios demo transfer). Plain Node ESM — no test runner.
// Run: `node tests/site-map.test.mjs`. Exits non-zero on any failure.
//
// The DOM pieces (mountSiteMap tooltips, grab panning, wheel zoom, the
// morphing map key) are browser-only + exercised manually; this file pins
// the caption, the barycenter layout, the corner-routed edges, the chip
// priority, and the SVG string — including the app-wide <title>-element
// ban (native tooltips are banned; the JS tooltip is the only sanctioned
// hover surface).
import { strict as assert } from 'node:assert';
import {
  siteMapLabel,
  layoutSiteMap,
  buildSiteMapSvg,
  buildAssetChips,
  assetChipSvg,
} from '../src/fable/engine/site-map.js';

let passed = 0;
let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log('  ok   %s', name);
    passed++;
  } catch (e) {
    console.error('  FAIL %s\n       %s', name, e.message);
    failed++;
  }
}

// A fixture mirroring the Rust `player_slice` serde output EXACTLY (the
// wire shape fable_site_map_get hands the frontend): snake_case knowledge
// words, ""-name fog stubs with synthetic "?N" ids, deduped edges, and
// the NINE-MARKER kind vocabulary Rust's site_map::marker_kind emits.
const SLICE = {
  site_name: 'The Warren',
  threat: 'high',
  hosted: false,
  entrance: 'gatehouse',
  current_area: 'hall',
  areas: [
    {
      id: 'gatehouse', name: 'Gatehouse', knowledge: 'visited',
      geometry: ['cold draft through murder holes'],
      assets: [
        { name: 'Gate Keeper', kind: 'friendly', state: '', count: 0, suspected: false },
        { name: 'Warband Scouts', kind: 'hostile', state: '', count: 6, suspected: true },
      ],
    },
    { id: 'hall', name: 'Great Hall', knowledge: 'discovered', geometry: [], assets: [] },
    {
      id: 'vault', name: 'Vault', knowledge: 'visited',
      geometry: ['shelves of lead caskets'],
      assets: [{ name: 'Hoard Coins', kind: 'loot', state: 'taken', count: 0, suspected: false }],
    },
    { id: '?1', name: '', knowledge: 'fog', geometry: [], assets: [] },
    { id: '?2', name: '', knowledge: 'fog', geometry: [], assets: [] },
  ],
  edges: [
    { from: 'gatehouse', to: 'hall', state: 'open' },
    { from: 'gatehouse', to: '?1', state: 'locked' },
    { from: 'hall', to: 'vault', state: 'blocked' },
    { from: 'hall', to: '?2', state: 'open' },
  ],
};

// ── siteMapLabel (2026-08-24 Chloe ruling: the threat word alone) ─────────
test('siteMapLabel: the threat word alone, empty fallback', () => {
  assert.equal(siteMapLabel(SLICE), 'high threat');
  assert.equal(siteMapLabel({ ...SLICE, hosted: true, threat: 'low' }), 'low threat');
  assert.equal(siteMapLabel({ threat: '' }), '');
  assert.equal(siteMapLabel(null), '');
});

// ── buildAssetChips (ONE marker per bubble, home priority) ────────────────
test('buildAssetChips: ONE chip per bubble, safe > shop > general priority', () => {
  const { chips, overflow } = buildAssetChips([
    { name: 'Bell Tower', kind: 'general' },
    { name: 'The Smithy', kind: 'shop' },
    { name: "Cooper's Shop", kind: 'shop' },
  ]);
  assert.equal(chips.length, 1);
  assert.equal(chips[0].kind, 'shop', 'the first shop outranks general homes');
  assert.equal(overflow, 2, 'the unpicked assets count as overflow');
  const safeWins = buildAssetChips([
    { name: 'The Smithy', kind: 'shop' },
    { name: 'Wayside Inn', kind: 'safe' },
  ]);
  assert.equal(safeWins.chips[0].kind, 'safe');
  // Non-home kinds keep payload order below the homes.
  const mixed = buildAssetChips([
    { name: 'Cutpurse', kind: 'hostile' },
    { name: 'Temple', kind: 'safe' },
    { name: 'Mira the Barkeep', kind: 'friendly' },
  ]);
  assert.equal(mixed.chips[0].kind, 'safe', 'a safe home outranks every marker');
  assert.equal(mixed.overflow, 2);
});

test('buildAssetChips: unknown kinds fall back to quest; junk filters', () => {
  const { chips } = buildAssetChips([{ name: 'Odd Thing', kind: 'creature' }]);
  assert.equal(chips[0].kind, 'quest', 'unknown kinds fall back to the scroll');
  assert.deepEqual(buildAssetChips(null), { chips: [], overflow: 0 });
  assert.deepEqual(buildAssetChips([{ name: '', kind: 'loot' }]), { chips: [], overflow: 0 });
});

// ── buildAssetChips (2026-08-25 quest anchors) ────────────────────────────
test('buildAssetChips: an anchored objective outranks every marker', () => {
  const { chips } = buildAssetChips([
    { name: 'Wayside Inn', kind: 'safe' },
    { name: 'Cutpurse', kind: 'hostile' },
  ], true);
  assert.equal(chips.length, 1);
  assert.equal(chips[0].kind, 'quest', 'the anchor pin wins over the safe home');
  // An anchored bubble with NO assets still grows its chip.
  const bare = buildAssetChips([], true);
  assert.equal(bare.chips.length, 1);
  assert.equal(bare.chips[0].kind, 'quest');
});

test('svg: an anchored area renders the scroll chip + carries the titles', () => {
  const slice = {
    entrance: 'ward', current_area: 'ward',
    areas: [
      {
        id: 'ward', name: 'Market Ward', knowledge: 'visited',
        geometry: [], assets: [],
        quests: ['Investigate the cutpurse', 'Return the ledger'],
      },
      { id: 'cellar', name: 'Cellar', knowledge: 'visited', geometry: [], assets: [] },
    ],
    edges: [{ from: 'ward', to: 'cellar', state: 'open' }],
  };
  const svg = buildSiteMapSvg(slice);
  // The anchored bubble grew a chip despite empty assets; the cellar
  // (no anchor, no assets) grew none.
  const ward = svg.split('<g').find((g) => g.includes('data-map-node="ward"'));
  assert.equal((ward.match(/<svg x="/g) || []).length, 1, 'the anchor chip renders');
  const cellar = svg.split('<g').find((g) => g.includes('data-map-node="cellar"'));
  assert.equal((cellar.match(/<svg x="/g) || []).length, 0, 'no anchor, no chip');
  assert.equal(ward.includes('viewBox="0 0 12 12" width="20" height="20"'), true);
  // The tooltip payload round-trips the objective titles.
  const info = mapInfoFor(svg, 'ward');
  assert.deepEqual(info.quests, ['Investigate the cutpurse', 'Return the ledger']);
});

// ── assetChipSvg (glyph-only — states ride the hover text) ────────────────
test('assetChipSvg: glyph-only, no state overlays or count badges', () => {
  const taken = assetChipSvg({ kind: 'loot', state: 'taken', suspected: false, count: 0 });
  assert.ok(taken.includes('site-map-chip-glyph'));
  assert.ok(!taken.includes('slash'));
  assert.ok(!taken.includes('hollow'));
  assert.ok(!taken.includes('site-map-chip-dot'));
  assert.ok(!taken.includes('×'), 'no count badge — counts ride the hover text');
  const suspected = assetChipSvg({ kind: 'hostile', state: '', suspected: true, count: 6 });
  assert.ok(!suspected.includes('is-suspected'), 'no suspected dimming on chips');
  // The colored markers carry their inline color (survives currentColor).
  const shop = assetChipSvg({ kind: 'shop', state: '', suspected: false, count: 0 });
  assert.ok(shop.includes('#E8C84A'), 'the shop glyph keeps its yellow');
});

// ── layoutSiteMap ────────────────────────────────────────────────────────
test('layout: single chain — exact continuous-x placement + corner routing', () => {
  // A→B, both visited: A centers at PAD + NODE_W/2 = 89; B takes its
  // parent's mean x (89). The vertical edge attaches at the bottom/top
  // CENTER and TUCKS 8px INSIDE each box (emerging from behind the fill).
  const slice = {
    entrance: 'a', current_area: 'b',
    areas: [
      { id: 'a', name: 'A', knowledge: 'visited' },
      { id: 'b', name: 'B', knowledge: 'visited' },
    ],
    edges: [{ from: 'a', to: 'b', state: 'open' }],
  };
  const { nodes, edges, width, height } = layoutSiteMap(slice);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  assert.equal(byId.get('a').cx, 89);
  assert.equal(byId.get('a').cy, 14 + 19);          // row 0 midline
  assert.equal(byId.get('b').cx, 89);               // under its parent
  assert.equal(byId.get('b').cy, 14 + 38 + 40 + 19); // row 1 midline
  assert.equal(width, 89 + 75 + 14);
  assert.equal(height, 14 + 38 + 40 + 38 + 14);
  const e = edges[0];
  assert.equal(e.x1, 89);
  assert.equal(e.x2, 89);
  assert.equal(e.y1, 14 + 38 - 8, 'start tucks 8px inside the parent box');
  assert.equal(e.y2, byId.get('b').y + 8, 'end tucks 8px inside the child box');
  assert.ok(e.y1 < byId.get('a').y + byId.get('a').h, 'the tucked start is inside A');
  assert.ok(e.y2 > byId.get('b').y, 'the tucked end is inside B');
  assert.equal(e.fogward, false, 'both endpoints visited — the path stays lit');
});

test('layout: rows are CENTER-ALIGNED on the row midline', () => {
  // One root (bare, 38) with two rank-1 children: left carries a chip
  // (h = 38 + 22), right is bare (h = 38) — both center on the SAME row
  // midline, never top-hung.
  const slice = {
    entrance: 'root', current_area: 'root',
    areas: [
      { id: 'root', name: 'Root', knowledge: 'visited' },
      { id: 'left', name: 'Left', knowledge: 'visited', assets: [{ name: 'Crate', kind: 'loot' }] },
      { id: 'right', name: 'Right', knowledge: 'discovered' },
    ],
    edges: [
      { from: 'root', to: 'left', state: 'open' },
      { from: 'root', to: 'right', state: 'open' },
    ],
  };
  const { nodes } = layoutSiteMap(slice);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  assert.equal(byId.get('root').cy, 14 + 38 / 2);
  assert.equal(byId.get('left').h, 38 + 22);
  assert.equal(byId.get('right').h, 38);
  assert.equal(byId.get('left').cy, byId.get('right').cy);
  assert.equal(byId.get('left').cy, 14 + 38 + 40 + 60 / 2, 'the tall row centers both');
});

test('layout: same-row neighbors clear the generous column gap', () => {
  // Two named children of one parent: |Δcx| ≥ halfSpan+COL_GAP+halfSpan.
  const slice = {
    entrance: 'root', current_area: 'root',
    areas: [
      { id: 'root', name: 'Root', knowledge: 'visited' },
      { id: 'left', name: 'Left', knowledge: 'visited' },
      { id: 'right', name: 'Right', knowledge: 'visited' },
    ],
    edges: [
      { from: 'root', to: 'left', state: 'open' },
      { from: 'root', to: 'right', state: 'open' },
    ],
  };
  const { nodes } = layoutSiteMap(slice);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const gap = Math.abs(byId.get('left').cx - byId.get('right').cx);
  assert.ok(gap >= 75 + 64 + 75 - 1e-9, `gap ${gap} ≥ pairGap 214`);
  // Left clamp: nobody starts left of the padding.
  for (const n of nodes) {
    const half = n.fog ? 13.5 + 4 : 75;
    assert.ok(n.cx >= 14 + half - 1e-9, `${n.id} respects the left pad`);
  }
});

test('layout: fog stubs tuck beside their named sibling, never drift a column', () => {
  // The hall row: hall (named) + ?1 (fog, locked partner). Same barycenter
  // tie group → the fog stub sits right beside the named sibling, within
  // one pairGap — never a full column+ away.
  const { nodes } = layoutSiteMap(SLICE);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const d = Math.abs(byId.get('hall').cx - byId.get('?1').cx);
  assert.ok(d <= 75 + 64 + 17.5 + 1e-9, `fog stub hugs its named sibling (Δ ${d})`);
  assert.equal(byId.get('?1').fog, true);
  assert.equal(byId.get('?1').w, 27);
});

test('layout: PATH-LIGHT — open edges dim toward unexplored endpoints only', () => {
  const { edges } = layoutSiteMap(SLICE);
  const find = (from, to) => edges.find((e) =>
    (e.from === from && e.to === to) || (e.from === to && e.to === from));
  assert.equal(find('gatehouse', 'hall').fogward, true, 'discovered-but-unvisited dims');
  assert.equal(find('hall', 'vault').fogward, false, 'blocked edges never dim (state wins)');
  assert.equal(find('gatehouse', '?1').fogward, false, 'locked edges never dim');
  assert.equal(find('hall', '?2').fogward, true, 'fog endpoint dims');
});

test('layout: current + entrance flags land on the right nodes', () => {
  const { nodes } = layoutSiteMap(SLICE);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  assert.equal(byId.get('hall').current, true);
  assert.equal(byId.get('gatehouse').entrance, true);
  assert.equal(byId.get('gatehouse').current, false);
});

test('layout: unreachable visible nodes fall back to rank 0', () => {
  const slice = {
    entrance: 'a', current_area: 'a',
    areas: [
      { id: 'a', name: 'A', knowledge: 'visited' },
      { id: 'b', name: 'B', knowledge: 'discovered' },
    ],
    edges: [], // no visible edges at all → B joins the entrance row
  };
  const { nodes } = layoutSiteMap(slice);
  const byId = new Map(nodes.map((n) => [n.id, n]));
  assert.equal(byId.get('b').cy, byId.get('a').cy);
});

test('layout: junk input degrades to an empty canvas', () => {
  assert.deepEqual(layoutSiteMap(null), { nodes: [], edges: [], width: 0, height: 0 });
  assert.equal(layoutSiteMap({ areas: [], edges: [] }).nodes.length, 0);
  const weird = layoutSiteMap({ areas: [{ id: 7 }, null, { id: 'ok', knowledge: 'bogus' }], edges: [] });
  assert.equal(weird.nodes.length, 1);
  assert.equal(weird.nodes[0].knowledge, 'visited'); // unknown word → visited bucket
});

// ── buildSiteMapSvg ──────────────────────────────────────────────────────
test('svg: NO <title> elements anywhere (the app-wide native-tooltip ban)', () => {
  const svg = buildSiteMapSvg(SLICE);
  assert.ok(!svg.includes('<title'), 'native SVG tooltips are banned');
  assert.ok(!svg.includes(' title='), 'title attributes are banned');
});

test('svg: knowledge classes + fog "?" labels', () => {
  const svg = buildSiteMapSvg(SLICE);
  assert.ok(svg.includes('is-visited'));
  assert.ok(svg.includes('is-discovered'));
  assert.ok(svg.includes('is-fog'));
  assert.ok(svg.includes('>?</text>'), 'fog stubs render a lone ?');
  assert.ok(svg.includes('>Gatehouse</text>'));
  assert.ok(svg.includes('>Great Hall</text>'));
});

test('svg: NO here-dot — the pulsing border is the sole here-marker', () => {
  const svg = buildSiteMapSvg(SLICE);
  assert.ok(!svg.includes('here-dot'), 'the dot is retired');
  const hall = svg.split('<g').find((g) => g.includes('data-map-node="hall"'));
  assert.ok(hall.includes('is-current'), 'the current room carries the pulse class');
});

test('svg: opaque backer rects behind every known node', () => {
  const svg = buildSiteMapSvg(SLICE);
  const gate = svg.split('<g').find((g) => g.includes('data-map-node="gatehouse"'));
  assert.ok(gate.includes('fill="#16140D"'), 'visited backer');
  const hall = svg.split('<g').find((g) => g.includes('data-map-node="hall"'));
  assert.ok(hall.includes('fill="#0E0E0E"'), 'discovered backer');
});

test('svg: fog stubs are hoverable UNDISCOVERED stubs with opaque backers', () => {
  const svg = buildSiteMapSvg(SLICE);
  const fog = svg.split('<g').find((g) => g.includes('data-map-fog="1"'));
  assert.ok(fog, 'fog stubs carry the fog flag');
  assert.ok(fog.includes('data-map-node="?'), 'fog stubs are hoverable');
  assert.ok(fog.includes('aria-label="Undiscovered"'));
  assert.ok(fog.includes('fill="#0A0A0A"'), 'the opaque backer circle hides tucked edges');
});

test('svg: edge hover groups + hit lines + state classes', () => {
  const svg = buildSiteMapSvg(SLICE);
  assert.ok(svg.includes('site-map-edge-g'), 'edges are wrapped in hover groups');
  assert.ok(svg.includes('data-map-edge="locked"'));
  assert.ok(svg.includes('data-map-edge="blocked"'));
  assert.ok(svg.includes('site-map-edge-hit'), 'the fat transparent hit-line renders');
  assert.ok(svg.includes('site-map-edge is-open is-fogward'), 'the dim open path class renders');
  assert.ok(svg.includes('site-map-edge is-locked'));
  assert.ok(svg.includes('site-map-edge is-blocked'));
});

test('svg: unknown edge state normalizes to open', () => {
  const svg = buildSiteMapSvg({
    entrance: 'a', current_area: 'a',
    areas: [{ id: 'a', name: 'A' }, { id: 'b', name: 'B' }],
    edges: [{ from: 'a', to: 'b', state: 'warped' }],
  });
  assert.ok(svg.includes('site-map-edge is-open'));
  assert.ok(!svg.includes('is-warped'));
});

// Extract a data-map-info payload from the SVG string + XML-decode it the
// way the DOM does automatically on attribute reads.
function decodeXmlEntities(s) {
  return s
    .replace(/&quot;/g, '"')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&amp;/g, '&');
}
function mapInfoFor(svg, id) {
  const g = svg.split('<g').find((x) => x.includes(`data-map-node="${id}"`));
  const m = g && g.match(/data-map-info="([^"]*)"/);
  return m ? JSON.parse(decodeXmlEntities(m[1])) : null;
}

test('svg: the stashed tooltip payload survives the attribute round-trip', () => {
  const svg = buildSiteMapSvg(SLICE);
  const info = mapInfoFor(svg, 'gatehouse');
  assert.equal(info.name, 'Gatehouse');
  assert.equal(info.knowledge, 'visited');
  assert.equal(info.here, false);
  assert.deepEqual(info.geometry, ['cold draft through murder holes']);
  assert.equal(info.assets.length, 2);
  assert.equal(info.assets[1].count, 6);
  assert.equal(info.assets[1].suspected, true);
});

test('svg: special characters in names survive escaping + round-trip', () => {
  const svg = buildSiteMapSvg({
    entrance: 'roost', current_area: 'roost',
    areas: [{ id: 'roost', name: 'Raven "Roost" & Belfry', knowledge: 'visited' }],
    edges: [],
  });
  const info = mapInfoFor(svg, 'roost');
  assert.equal(info.name, 'Raven "Roost" & Belfry');
  // The visible label keeps the quotes escaped inside the text node.
  assert.ok(svg.includes('&quot;'));
});

test('svg: labels truncate past the 18-char budget with an ellipsis', () => {
  const svg = buildSiteMapSvg({
    entrance: 'a', current_area: 'a',
    areas: [{ id: 'a', name: 'The Ridiculously Long Room Name', knowledge: 'visited' }],
    edges: [],
  });
  assert.ok(svg.includes('The Ridiculously …</text>'));
});

// ── asset chips (ONE big centered marker per bubble) ─────────────────────
test('svg: one chip renders centered under the name as a nested svg', () => {
  const svg = buildSiteMapSvg(SLICE);
  // The gatehouse carries 2 assets → exactly ONE chip; the vault carries
  // 1 → one chip. The chip is a NESTED <svg> (12→20 scaling, centered).
  // (The fragment ends at the chip's own <g, so the chip classes are
  // asserted on the whole svg below.)
  const gate = svg.split('<g').find((g) => g.includes('data-map-node="gatehouse"'));
  const chipCount = (gate.match(/<svg x="/g) || []).length;
  assert.equal(chipCount, 1, 'one marker per bubble');
  assert.ok(gate.includes('viewBox="0 0 12 12" width="20" height="20"'));
  // Centered: startX = cx − (CHIP_ICON − CHIP_GAP)/2 = cx − 10.
  const m = gate.match(/<svg x="([\d.]+)" y="([\d.]+)"/);
  const cxm = gate.match(/<text x="([\d.]+)"/);
  assert.ok(m && cxm);
  assert.equal(Math.round(Math.abs(parseFloat(m[1]) + 10 - parseFloat(cxm[1])) * 10) / 10, 0);
  assert.equal((svg.match(/class="site-map-chip"/g) || []).length, 2, 'two chip bubbles');
});

test('svg: NO "+N" overflow marker — the hover text carries the rest', () => {
  const slice = {
    entrance: 'a', current_area: 'a',
    areas: [
      {
        id: 'a', name: 'A', knowledge: 'visited',
        assets: [
          { name: 'Temple', kind: 'safe' },
          { name: 'Crate', kind: 'quest' },
          { name: 'Crate 2', kind: 'quest' },
        ],
        assets_overflow: 4,
      },
    ],
    edges: [],
  };
  const svg = buildSiteMapSvg(slice);
  assert.ok(!svg.includes('site-map-chip-more'), 'no +N chip marker');
  assert.ok(!svg.includes('>+4</text>'));
  // The chip is the safe home (priority), not the first asset — the blue
  // glyph markup rides the whole svg (a <g-split fragment ends early).
  assert.ok(svg.includes('#8FC3EE'), 'the safe home glyph carries its blue');
});

test('svg: empty slice renders the empty canvas (mount shows the fallback)', () => {
  assert.equal(buildSiteMapSvg(null), '');
  assert.equal(buildSiteMapSvg({ areas: [], edges: [] }), '');
});

console.log(failed === 0 ? `\nAll ${passed} site-map tests passed.` : `\n${failed} FAILED, ${passed} passed.`);
process.exit(failed === 0 ? 0 : 1);
