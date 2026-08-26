// =============================================================
// FABLE SITE MAP — the player-facing fog-of-war node graph (2026-08-23;
// REDESIGNED 2026-08-25 from the location-ui-scenarios demo, Chloe's
// final approved design). A knowledge-filtered node graph where visited
// rooms are named, discovered rooms render dim, unrevealed neighbors
// surface as "?" fog stubs, and the current room's border pulses.
//
// DATA: the `fable_site_map_get` IPC (lib.rs) → the Rust-side
// knowledge-filtered slice (`site_map::player_slice`). Hidden truth never
// crosses the IPC (Unrevealed areas arrive only as anonymous "?N" stubs;
// Unrevealed assets never arrive at all), so this module NEVER re-filters
// — it lays out + renders exactly what Rust vouches the player knows.
// The wire's asset `kind` is the NINE-MARKER display vocabulary
// (general/safe/shop/quest/loot/hazard/friendly/hostile/boss), derived
// Rust-side from tracked state (site_map::marker_kind) — never guessed
// here.
//
//   siteMapLabel(slice)          — pure: the caption line above the graph
//                                  (the threat word alone).
//   layoutSiteMap(slice)         — pure: BFS ranks from the entrance,
//                                  barycenter-ordered rows, continuous-x
//                                  placement, corner-routed edges.
//   buildSiteMapSvg(slice)       — pure: the SVG string. NO <title>
//                                  elements — native tooltips are banned
//                                  app-wide; the hover surface below is
//                                  the sanctioned one.
//   mountSiteMap(scrollEl, slice)— DOM: idempotent render + the hover
//                                  tooltip over nodes, fog stubs, AND
//                                  connectors (state word). Built like the
//                                  injury heatmap's tooltip: one delegated
//                                  mouseover/mouseout pair, a JS-built
//                                  dark-gold card anchored to the hovered
//                                  element's bbox center clamped into the
//                                  visible frame, textContent-only text,
//                                  focusin/focusout parity, mousedown
//                                  default suppressed.
//   wireGrabPanning(scrollEl)    — DOM: click-hold-grab panning (no
//                                  visible scrollbar).
//   wireWheelZoom(scrollEl)      — DOM: wheel zoom 0.7×–2.5× anchored on
//                                  the cursor.
//   buildMapLegend()             — DOM: the morphing hamburger map key
//                                  (Area / Path / Marker sections).
//
// Node ids are opaque keys on the wire: real kebab ids for known areas,
// "?N" synthetics for fog stubs (a "?" cannot appear in a kebab id, so
// the two namespaces can never collide).
// =============================================================

// ─── Layout constants (2026-08-25 demo: generous spacing) ──────────────────
const NODE_W = 150;      // known-area box width
const NODE_H = 38;       // known-area box height
const FOG_R = 13.5;      // fog-stub circle radius
const RANK_GAP = 40;     // tight verticals — short connector runs
const COL_GAP = 64;      // generous horizontal spread between bubbles
const PAD = 14;          // outer padding
const LABEL_MAX = 18;    // label truncation budget (chars)
// Paths TUCK 8px BEHIND the bubbles (negative gap pulls the line endpoint
// deep inside the node shape, past the rounded-corner arcs — a shallow 3px
// tuck let diagonals peek out at the corner curvature). Edges render
// BEFORE nodes and every bubble has an opaque backer, so the overshoot is
// fully hidden.
const EDGE_NODE_GAP = -8;

// Asset chip sizing (demo): one BIG glyph per bubble.
const CHIP_ICON = 20;
const CHIP_GAP = 8;
const CHIP_ROW_H = 22;

// MARKER VOCABULARY (redesign): bronze markers inherit currentColor; the
// colored ones (quest white, hazard amber, friendly blue, hostile red, boss
// white) carry an inline-styled <g> so their color survives the
// currentColor CSS on both chips and tooltip icons.
const ASSET_ICONS = {
  // ONE home glyph shape; the SUBTYPE carries the color on the map —
  // general white, safe light blue, shop yellow. (The map key shows the
  // single white icon with the three colored words beside it.)
  general: '<g style="stroke:#e8e8e8" stroke-width="0.85"><path d="M2 6.1L6 2.6l4 3.5" fill="none" stroke-linejoin="round" stroke-linecap="round"/><path d="M3.1 5.5v4.1h5.8V5.5" fill="none" stroke-linejoin="round"/><path d="M5.1 9.6V7.3h1.8v2.3" fill="none"/></g>',
  safe: '<g style="stroke:#8FC3EE" stroke-width="0.85"><path d="M2 6.1L6 2.6l4 3.5" fill="none" stroke-linejoin="round" stroke-linecap="round"/><path d="M3.1 5.5v4.1h5.8V5.5" fill="none" stroke-linejoin="round"/><path d="M5.1 9.6V7.3h1.8v2.3" fill="none"/></g>',
  shop: '<g style="stroke:#E8C84A" stroke-width="0.85"><path d="M2 6.1L6 2.6l4 3.5" fill="none" stroke-linejoin="round" stroke-linecap="round"/><path d="M3.1 5.5v4.1h5.8V5.5" fill="none" stroke-linejoin="round"/><path d="M5.1 9.6V7.3h1.8v2.3" fill="none"/></g>',
  // Quest = the 📜 emoji itself (Chloe ruling 2026-08-25, ending the
  // hand-drawn scroll attempts). Rendered as SVG <text> in the same 12×12
  // glyph slot. The size MUST be an inline style — the attribute form is
  // overridden by the `.site-map-node text { font-size: 13px }` rule
  // (the same descendant-selector trap as the old pulsing-scroll bug).
  quest: '<text x="6" y="8.6" text-anchor="middle" style="font-size:8px" font-family="\'Segoe UI Emoji\',\'Noto Color Emoji\',emoji" fill="#e8e8e8">\u{1F4DC}</text>',
  // Loot = the REAL left-drawer POUCH glyph (left-drawer.js POUCH_SVG),
  // verbatim 24×24 paths nested into the 12×12 marker space, drawn with
  // thin outlines (1.2 / cords 0.8) so the ruffle + cinch + cords still
  // read at marker size.
  loot: '<svg viewBox="0 0 24 24" overflow="visible" fill="none" style="stroke:#9CD48A" stroke-width="1" stroke-linecap="butt" stroke-linejoin="miter" stroke-miterlimit="4"><path d="M9.5 6.3 C8.9 5.8 8.5 5.2 8.3 4.4 C9.2 4.5 10 4.9 10.6 5.5"/><path d="M10.6 5.5 C10.8 4.6 11.3 3.8 12 3.2 C12.7 3.8 13.2 4.6 13.4 5.5"/><path d="M13.4 5.5 C14 4.9 14.8 4.5 15.7 4.4 C15.5 5.2 15.1 5.8 14.5 6.3"/><path d="M9.2 7.1 H14.8"/><path stroke-width="0.7" d="M11.2 7.1 C10.9 8.4 10.7 9.8 10.5 11.2"/><path stroke-width="0.7" d="M12.8 7.1 C13.1 8.6 13.4 10.4 13.5 12.6"/><path d="M9.2 7.1 C8.4 9.1 6.4 11.4 5.7 13.5 C4.9 17.2 8 20.4 12 20.4 C16 20.4 19.1 17.2 18.3 13.5 C17.6 11.4 15.6 9.1 14.8 7.1"/></svg>',
  hazard: '<g style="stroke:#E0A33C"><path d="M6 2.3L10.2 9.7H1.8z" fill="none" stroke-width="0.85" stroke-linejoin="round"/><path d="M6 5.2v1.9" fill="none" stroke-width="0.8" stroke-linecap="round"/><circle cx="6" cy="8.4" r=".45" fill="#E0A33C" stroke="none"/></g>',
  friendly: '<g style="stroke:#4A9BFF"><circle cx="6" cy="6" r="4.1" fill="none"/><circle cx="6" cy="6" r="1.8" fill="#4A9BFF" stroke="none"/></g>',
  hostile: '<g style="stroke:#E05555"><circle cx="6" cy="6" r="4.1" fill="none"/><circle cx="6" cy="6" r="1.8" fill="#E05555" stroke="none"/></g>',
  // Boss = the 💀 emoji, same treatment as the quest 📜 (inline font-size
  // style — the `.site-map-node text` CSS rule would otherwise pin it).
  boss: '<text x="6" y="8.6" text-anchor="middle" style="font-size:8px" font-family="\'Segoe UI Emoji\',\'Noto Color Emoji\',emoji" fill="#e8e8e8">\u{1F480}</text>',
};
const ASSET_ICON_DEFAULT = ASSET_ICONS.quest;

function normalizeKind(kind) {
  const k = String(kind || '').trim().toLowerCase();
  return ASSET_ICONS[k] ? k : 'quest';
}

/// PURE: the chip row model over one area's assets. At most ONE chip per
/// bubble, chosen by priority — an ACTIVE QUEST ANCHOR outranks everything
/// (the player's live objective is the most actionable pin — Chloe's
/// Market Ward ruling), then the home family (a SAFE home outranks a SHOP
/// home outranks a GENERAL home), then every other kind in payload order.
/// Exported for tests.
export function buildAssetChips(assets, hasQuestAnchor = false) {
  const list = (Array.isArray(assets) ? assets : [])
    .filter((a) => a && typeof a.name === 'string' && a.name)
    .map((a) => ({
      kind: normalizeKind(a.kind),
      state: String(a.state || ''),
      suspected: !!a.suspected,
      count: Number(a.count) | 0,
      name: a.name,
    }));
  const HOME_RANK = { safe: 0, shop: 1, general: 2 };
  if (hasQuestAnchor) {
    // The anchor chip: the quest glyph with no asset behind it (the hover
    // text carries the actual objective titles).
    return {
      chips: [{ kind: 'quest', state: '', suspected: false, count: 0, name: '' }],
      overflow: 0,
    };
  }
  const pick = list
    .map((a, i) => ({ a, i, rank: HOME_RANK[a.kind] ?? 3 }))
    .sort((x, y) => x.rank - y.rank || x.i - y.i)[0];
  const chips = pick ? [pick.a] : [];
  return { chips, overflow: Math.max(0, list.length - chips.length) };
}

/// PURE: one chip's SVG inner markup — the CONTENT glyph only. No state
/// overlays (slash, hollow ring, corner dot) and no suspected dimming:
/// states are expressed as words in the hover text alone ("Aged Cask
/// (taken)"). Exported for tests.
export function assetChipSvg(chip) {
  const glyph = ASSET_ICONS[chip.kind] || ASSET_ICON_DEFAULT;
  return `<g class="site-map-chip"><g class="site-map-chip-glyph">${glyph}</g></g>`;
}

function escapeXml(v) {
  return String(v == null ? '' : v)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

function truncateLabel(name, max = LABEL_MAX) {
  const t = String(name || '').trim();
  if (t.length <= max) return t;
  return `${t.slice(0, Math.max(1, max - 1))}…`;
}

// Kebab id → readable fallback when an area carries no name (the architect
// emits names, but a hand-edited save may not).
function prettyId(id) {
  return String(id || '')
    .replace(/[-_]+/g, ' ')
    .trim()
    .replace(/(^|\s)([a-z])/g, (m, a, b) => a + b.toUpperCase());
}

// ─── Caption ───────────────────────────────────────────────────────────────
// The small line above the graph: the threat word alone (Chloe 2026-08-24).
// CSS uppercases it. Pure.
export function siteMapLabel(slice) {
  const threat = String(slice && slice.threat ? slice.threat : '').trim();
  return threat ? `${threat} threat` : '';
}

// ─── BFS ranks (the layered-layout root ordering) ──────────────────────────
// Root priority: the ENTRANCE (a revealed node), else the current area,
// else the first revealed node, else the first node. Rank = BFS hops from
// the root over the undirected VISIBLE graph. Pure.
function bfsRanks(nodes, edges, rootId) {
  const ids = new Set(nodes.map((n) => n.id));
  const adj = new Map(nodes.map((n) => [n.id, []]));
  for (const e of edges) {
    if (!ids.has(e.from) || !ids.has(e.to)) continue;
    adj.get(e.from).push(e.to);
    adj.get(e.to).push(e.from);
  }
  const rank = new Map();
  if (!rootId || !ids.has(rootId)) return rank;
  const queue = [rootId];
  rank.set(rootId, 0);
  while (queue.length) {
    const id = queue.shift();
    for (const next of adj.get(id) || []) {
      if (rank.has(next)) continue;
      rank.set(next, rank.get(id) + 1);
      queue.push(next);
    }
  }
  return rank;
}

// ─── Layout ────────────────────────────────────────────────────────────────
// Top-down layered layout: each BFS rank is a ROW; within a row, nodes are
// BARYCENTER-ordered near their parents' columns so a connector never
// streaks clear across the frame, then placed at CONTINUOUS x positions
// (desired x = the mean of the parents' actual x — children sit under
// their parents) resolved against same-row overlaps with a
// forward-push / backward-pull sweep. Every bubble in a rank is
// CENTER-ALIGNED on the row's midline. Pure.
export function layoutSiteMap(slice) {
  const areas = Array.isArray(slice && slice.areas) ? slice.areas : [];
  const edgesIn = Array.isArray(slice && slice.edges) ? slice.edges : [];
  const nodes = areas
    .filter((a) => a && typeof a.id === 'string' && a.id)
    .map((a) => {
      const knowledge =
        a.knowledge === 'discovered' ? 'discovered' : a.knowledge === 'fog' ? 'fog' : 'visited';
      return {
        id: a.id,
        name: typeof a.name === 'string' && a.name ? a.name : prettyId(a.id),
        knowledge,
        fog: knowledge === 'fog',
        current: a.id === slice.current_area,
        entrance: a.id === slice.entrance && knowledge !== 'fog',
        geometry: Array.isArray(a.geometry) ? a.geometry.filter((g) => typeof g === 'string') : [],
        assets: Array.isArray(a.assets) ? a.assets : [],
        assetsOverflow: Number(a.assets_overflow) | 0,
        // (2026-08-25 quest anchors) Active objective titles anchored to
        // this area (Rust-side knowledge gate already applied).
        quests: Array.isArray(a.quests)
          ? a.quests.filter((q) => typeof q === 'string' && q.trim())
          : [],
      };
    })
    .map((n) => {
      const { chips, overflow } = buildAssetChips(n.assets, n.quests.length > 0);
      return {
        ...n,
        chips,
        overflow: Math.max(overflow, n.assetsOverflow),
        chipsH: chips.length ? CHIP_ROW_H : 0,
      };
    });
  if (!nodes.length) return { nodes: [], edges: [], width: 0, height: 0 };

  const root =
    nodes.find((n) => n.entrance && !n.fog) ||
    nodes.find((n) => n.current && !n.fog) ||
    nodes.find((n) => !n.fog) ||
    nodes[0];
  const ranks = bfsRanks(
    nodes,
    edgesIn.filter((e) => e && typeof e.from === 'string' && typeof e.to === 'string'),
    root.id
  );

  const layers = new Map();
  for (const n of nodes) {
    const r = ranks.has(n.id) ? ranks.get(n.id) : 0;
    if (!layers.has(r)) layers.set(r, []);
    layers.get(r).push(n);
  }
  // BARYCENTER column ordering — each row's nodes sort near their parents'
  // columns, so a connector never streaks clear across the frame.
  const baseOrder = (a, b) => {
    if (a.fog !== b.fog) return a.fog ? 1 : -1;
    const byName = String(a.name).localeCompare(String(b.name));
    return byName !== 0 ? byName : String(a.id).localeCompare(String(b.id));
  };
  const colOf = new Map();
  for (const [rank, layer] of [...layers.entries()].sort((a, b) => a[0] - b[0])) {
    layer.sort(baseOrder);
    if (rank > 0) {
      const bcs = new Map(layer.map((n) => {
        const cols = [];
        for (const e of edgesIn) {
          const other = e.from === n.id ? e.to : e.to === n.id ? e.from : null;
          if (other != null && ranks.get(other) === rank - 1 && colOf.has(other)) {
            cols.push(colOf.get(other));
          }
        }
        const v = cols.length ? cols.reduce((s, c) => s + c, 0) / cols.length : Number.MAX_SAFE_INTEGER;
        return [n.id, v];
      }));
      layer.sort((a, b) => (bcs.get(a.id) - bcs.get(b.id)) || baseOrder(a, b));
      // FOG TUCK — inside each barycenter tie group (same parent column),
      // fog stubs slot in right after the FIRST named sibling instead of
      // sorting last, so the small stub hugs the parent side by side with
      // the named children.
      {
        const eps = 1e-9;
        const tucked = [];
        for (let i = 0; i < layer.length; ) {
          let j = i;
          while (j < layer.length && Math.abs(bcs.get(layer[j].id) - bcs.get(layer[i].id)) < eps) j++;
          const group = layer.slice(i, j);
          const named = group.filter((n) => !n.fog);
          const fogs = group.filter((n) => n.fog);
          if (named.length) tucked.push(named[0], ...fogs, ...named.slice(1));
          else tucked.push(...fogs);
          i = j;
        }
        layer.splice(0, layer.length, ...tucked);
      }
    }
    layer.forEach((n, i) => colOf.set(n.id, i));
  }

  // ── Continuous-x placement ───────────────────────────────────────────────
  // Each node's DESIRED x = the mean of its parents' ACTUAL x positions —
  // children sit under their parents — then same-row overlaps are resolved
  // with a forward-push / backward-pull sweep. Every connector leaves its
  // parent going DOWN; length ≈ the rank gap + a small fan for multi-child
  // parents.
  const halfSpan = (n) => (n.fog ? FOG_R + 4 : NODE_W / 2);
  const pairGap = (a, b) => halfSpan(a) + COL_GAP + halfSpan(b);
  const byId = new Map();
  const positioned = [];
  const xOf = new Map();
  let rowTop = PAD;
  let totalHeight = PAD;
  let maxRight = 0;
  for (const [rank, layer] of [...layers.entries()].sort((a, b) => a[0] - b[0])) {
    // Desired x: the parents' mean; parentless nodes (row 0) spread left
    // to right at full column pitch.
    const desired = layer.map((n, i) => {
      const px = [];
      for (const e of edgesIn) {
        const other = e.from === n.id ? e.to : e.to === n.id ? e.from : null;
        if (other != null && xOf.has(other)) px.push(xOf.get(other));
      }
      return px.length
        ? px.reduce((s, v) => s + v, 0) / px.length
        : PAD + halfSpan(n) + i * (NODE_W + COL_GAP);
    });
    // Sweep: push right to clear overlaps (stable barycenter order), pull
    // left to contract slack, clamp inside the left pad, push once more in
    // case the clamp squeezed anyone into its right neighbor.
    const xs = desired.slice();
    const pushRight = () => {
      for (let i = 1; i < layer.length; i++) {
        const min = xs[i - 1] + pairGap(layer[i - 1], layer[i]);
        if (xs[i] < min) xs[i] = min;
      }
    };
    pushRight();
    for (let i = layer.length - 2; i >= 0; i--) {
      const max = xs[i + 1] - pairGap(layer[i], layer[i + 1]);
      if (xs[i] > max) xs[i] = max;
    }
    for (let i = 0; i < layer.length; i++) {
      xs[i] = Math.max(xs[i], PAD + halfSpan(layer[i]));
    }
    pushRight();

    const rowH = Math.max(
      NODE_H,
      ...layer.map((n) => (n.fog ? NODE_H : NODE_H + n.chipsH))
    );
    // Every bubble in a rank is CENTER-ALIGNED on the row's midline — a
    // chip-less node sits level with its taller neighbors, never top-hung.
    const rowCy = rowTop + rowH / 2;
    layer.forEach((node, i) => {
      const cx = xs[i];
      const w = node.fog ? FOG_R * 2 : NODE_W;
      const h = node.fog ? FOG_R * 2 : NODE_H + node.chipsH;
      const placed = {
        ...node,
        cx,
        cy: rowCy,
        w, h,
        x: cx - w / 2,
        y: rowCy - h / 2,
      };
      positioned.push(placed);
      byId.set(node.id, placed);
      xOf.set(node.id, cx);
      maxRight = Math.max(maxRight, cx + halfSpan(node));
    });
    totalHeight = rowTop + rowH + PAD;
    rowTop += rowH + RANK_GAP;
  }
  const width = maxRight + PAD;
  const height = totalHeight;

  const edges = edgesIn
    .map((e) => {
      const from = byId.get(e.from);
      const to = byId.get(e.to);
      if (!from || !to) return null;
      const clipped = clipEdgeToNodeBorders(from, to);
      return {
        from: e.from, to: e.to,
        state: e.state === 'locked' || e.state === 'blocked' ? e.state : 'open',
        // PATH-LIGHT LAW: an open passage dims while EITHER endpoint is
        // unexplored (fog OR discovered-but-unvisited); the moment an area
        // is visited, its edges light up to the usual brass.
        fogward: e.state !== 'locked' && e.state !== 'blocked' && (from.knowledge !== 'visited' || to.knowledge !== 'visited'),
        x1: clipped.x1, y1: clipped.y1, x2: clipped.x2, y2: clipped.y2,
      };
    })
    .filter(Boolean);

  return { nodes: positioned, edges, width, height };
}

// CORNER-TO-CORNER routing. Each endpoint attaches at the rect corner
// facing the other bubble (sign of dx/dy picks the corner); the point then
// TUCKS `EDGE_NODE_GAP` INSIDE the rect along the direction to the other
// node's center, so the line emerges from behind the fill with no visible
// seam. Fog circles have no corners — they keep the radial border attach
// with the same tuck.
function edgeAttachPoint(node, otherCx, otherCy, gap = EDGE_NODE_GAP) {
  const dx = otherCx - node.cx;
  const dy = otherCy - node.cy;
  if (node.fog) {
    const len = Math.hypot(dx, dy) || 1;
    const t = (node.w / 2) / len;
    const extra = gap / len;
    return { x: node.cx + dx * (t + extra), y: node.cy + dy * (t + extra) };
  }
  // Mostly-VERTICAL link → bottom/top CENTER attach (the line stays
  // centered between the two bubbles, never the corner); anything with
  // real horizontal offset → the facing CORNER.
  let px, py;
  if (Math.abs(dx) < node.w * 0.22) {
    px = node.cx;
    py = dy >= 0 ? node.y + node.h : node.y;
  } else {
    px = dx >= 0 ? node.x + node.w : node.x;
    py = dy >= 0 ? node.y + node.h : node.y;
  }
  const len = Math.hypot(otherCx - px, otherCy - py) || 1;
  const ux = (otherCx - px) / len;
  const uy = (otherCy - py) / len;
  return { x: px + ux * gap, y: py + uy * gap };
}

function clipEdgeToNodeBorders(from, to) {
  const start = edgeAttachPoint(from, to.cx, to.cy);
  const end = edgeAttachPoint(to, from.cx, from.cy);
  return { x1: start.x, y1: start.y, x2: end.x, y2: end.y };
}

// ─── SVG render ─────────────────────────────────────────────────────────────
// Pure string builder. Edges render FIRST (under the nodes, so a connector
// crossing a rank never paints over a room box). No <title> elements —
// the hover surface is the mounted tooltip (mountSiteMap), never a native
// tooltip. Pure.
export function buildSiteMapSvg(slice) {
  const layout = layoutSiteMap(slice);
  if (!layout.nodes.length) return '';
  const siteName = String(slice && slice.site_name ? slice.site_name : 'Site map');

  const edges = layout.edges
    .map((e) =>
      // Wrapped in a hover group — the fat transparent hit-line makes the
      // connector itself hoverable for the state tooltip.
      `<g class="site-map-edge-g" data-map-edge="${e.state}">` +
        `<line class="site-map-edge-hit" x1="${round(e.x1)}" y1="${round(e.y1)}" x2="${round(e.x2)}" y2="${round(e.y2)}"></line>` +
        `<line class="site-map-edge is-${e.state}${e.fogward ? ' is-fogward' : ''}" x1="${round(e.x1)}" y1="${round(e.y1)}" x2="${round(e.x2)}" y2="${round(e.y2)}"></line>` +
      `</g>`
    )
    .join('');

  const nodes = layout.nodes
    .map((n) => {
      if (n.fog) {
        // Fog stubs are hoverable — the tooltip says UNDISCOVERED (the
        // player knows nothing more). Opaque backer circle (blended
        // #0A0A0A) hides the tucked edge ends that the 75%-alpha styled
        // fill would show through.
        return `<g class="site-map-node is-fog" data-map-node="${escapeXml(n.id)}" data-map-fog="1" tabindex="0" role="button" aria-label="Undiscovered">` +
          `<circle cx="${round(n.cx)}" cy="${round(n.cy)}" r="${FOG_R}" fill="#0A0A0A"></circle>` +
          `<circle cx="${round(n.cx)}" cy="${round(n.cy)}" r="${FOG_R}"></circle>` +
          `<text x="${round(n.cx)}" y="${round(n.cy + 6)}" text-anchor="middle">?</text>` +
          `</g>`;
      }
      const classes = ['site-map-node', `is-${n.knowledge}`];
      if (n.current) classes.push('is-current');
      const payload = JSON.stringify({
        name: n.name, knowledge: n.knowledge, here: n.current,
        geometry: n.geometry, assets: n.assets, quests: n.quests,
      });
      const aria = escapeXml(`${n.name} — ${n.current ? 'you are here' : n.knowledge}`);
      // Opaque backer rect in the blended fill color — the styled fills
      // are translucent (visited 90%, discovered 70%), which would let
      // the tucked 8px of edge line ghost through INSIDE the bubble. The
      // backer blocks the line while the translucent shape on top keeps
      // the identical look.
      const backFill = n.knowledge === 'discovered' ? '#0E0E0E' : '#16140D';
      const backer = `<rect x="${round(n.x)}" y="${round(n.y)}" width="${round(n.w)}" height="${round(n.h)}" rx="6" fill="${backFill}"></rect>`;
      const shape = `<rect x="${round(n.x)}" y="${round(n.y)}" width="${round(n.w)}" height="${round(n.h)}" rx="6"></rect>`;
      const notch = n.entrance
        ? `<path class="site-map-entry-notch" d="M ${round(n.x)} ${round(n.cy - 7.5)} L ${round(n.x + 9)} ${round(n.cy)} L ${round(n.x)} ${round(n.cy + 7.5)} Z"></path>`
        : '';
      // The "you are here" DOT is REMOVED — the pulsing border on the
      // current room's rect (CSS) is the sole here-marker.
      const label = escapeXml(truncateLabel(n.name));
      // Chips CENTERED under the name — the whole row is measured and
      // started at cx − rowW/2, tucked up close under the name.
      let chipsSvg = '';
      if (n.chips && n.chips.length) {
        const unit = CHIP_ICON + CHIP_GAP;
        const chipsW = n.chips.length * unit - CHIP_GAP;
        const startX = n.cx - chipsW / 2;
        const parts = n.chips.map((chip, i) => {
          const gx = round(startX + i * unit);
          const gy = round(n.y + NODE_H - 4);
          // Nested <svg> (NOT <g>): viewBox/width/height are ignored on
          // <g>, which rendered the glyph at raw 12px with its corner at
          // gx — 4px off center. The nested svg scales 12→CHIP_ICON and
          // anchors its top-left at (gx, gy) so the centering math holds
          // exactly.
          return `<svg x="${gx}" y="${gy}" viewBox="0 0 12 12" width="${CHIP_ICON}" height="${CHIP_ICON}" overflow="visible">${assetChipSvg(chip)}</svg>`;
        });
        // No "+N" overflow marker — the hover text carries the rest.
        chipsSvg = parts.join('');
      }
      // Explicit baseline (+5px for 13px Cinzel caps) — optical center, no
      // dominant-baseline (its web-font metrics sit high).
      return `<g class="${classes.join(' ')}" data-map-node="${escapeXml(n.id)}" data-map-info="${escapeXml(payload)}" tabindex="0" role="button" aria-label="${aria}">` +
        `${backer}${shape}${notch}<text x="${round(n.cx)}" y="${round(n.y + NODE_H / 2 + 5)}" text-anchor="middle">${label}</text>${chipsSvg}` +
        `</g>`;
    })
    .join('');

  return `<svg xmlns="http://www.w3.org/2000/svg" class="site-map-svg" viewBox="0 0 ${layout.width} ${layout.height}" width="${layout.width}" height="${layout.height}" role="img" aria-label="${escapeXml(siteName)}">${edges}${nodes}</svg>`;
}

function round(v) {
  return Math.round(v * 10) / 10;
}

// ─── DOM mount + the hover tooltip ──────────────────────────────────────────
// Render the SVG into the (exclusively-owned) scroll element + wire the
// tooltip. The tooltip div lives in the scroll element's PARENT (the
// .site-map-wrap frame) so it stays pinned over the VISIBLE area while the
// graph scrolls under it; it is removed + rebuilt on every mount
// (idempotent). One tooltip serves all three hover surfaces: nodes, fog
// stubs, and connectors.
export function mountSiteMap(scrollEl, slice) {
  if (!scrollEl) return;
  const svgStr = buildSiteMapSvg(slice);
  scrollEl.innerHTML = svgStr || '<div class="site-map-empty">No rooms revealed yet.</div>';
  const svg = scrollEl.querySelector('svg.site-map-svg');
  const wrap = scrollEl.parentElement;
  if (!svg || !wrap) return;

  // Drop any tooltip from a prior mount (idempotent re-render).
  wrap.querySelectorAll('[data-site-map-tooltip]').forEach((el) => el.remove());

  const tooltip = document.createElement('div');
  tooltip.setAttribute('class', 'site-map-tooltip');
  tooltip.setAttribute('data-site-map-tooltip', '');
  tooltip.setAttribute('role', 'tooltip');
  tooltip.style.display = 'none';
  wrap.appendChild(tooltip);

  // Shared reveal + anchor clamp. Hardened against the fast-hover escape
  // (tooltip slipping past the frame edge, cut by the card clip):
  //  (a) a pending hide timer is CANCELLED on show — a stale timer could
  //      otherwise race a rapid re-highlight;
  //  (b) the anchor point is first clamped INTO the visible scroll frame
  //      (a node half-scrolled out anchors at the frame edge, never
  //      off-frame);
  //  (c) one rAF later the clamp is re-run against final metrics, so any
  //      late layout (web-font swap, mid-transition re-hover) that changed
  //      the tooltip's size is corrected before it can sit outside.
  let hideTimer = null;
  function clampIntoFrame() {
    const tw = tooltip.offsetWidth;
    const th = tooltip.offsetHeight;
    const sw = wrap.clientWidth;
    const sh = wrap.clientHeight;
    const halfW = tw / 2;
    const halfH = th / 2;
    const loX = (tw >= sw) ? sw / 2 : halfW;
    const hiX = (tw >= sw) ? sw / 2 : sw - halfW;
    const loY = (th >= sh) ? sh / 2 : halfH;
    const hiY = (th >= sh) ? sh / 2 : sh - halfH;
    const clamp = (v, lo, hi) => Math.max(lo, Math.min(v, hi));
    tooltip.style.left = `${clamp(parseFloat(tooltip.style.left) || 0, loX, hiX)}px`;
    tooltip.style.top = `${clamp(parseFloat(tooltip.style.top) || 0, loY, hiY)}px`;
  }
  function revealTooltip(anchorEl) {
    if (hideTimer) { clearTimeout(hideTimer); hideTimer = null; }
    // PIN the natural width before positioning: an absolutely-positioned
    // shrink-to-fit box re-flows against the AVAILABLE width at its left
    // offset — near a frame edge that collapsed it to the 120px min-width
    // (the "thins out at the sides" bug). The measure must run with left
    // reset to 0 (the PREVIOUS anchor's stale left would collapse the
    // available width and misread the natural size); the pinned width is
    // then position-independent, clamped so it can never exceed the frame.
    tooltip.style.width = '';
    tooltip.style.left = '0px';
    tooltip.style.display = 'block';
    void tooltip.offsetWidth;
    const natural = tooltip.offsetWidth;
    const swFrame = wrap.clientWidth;
    tooltip.style.width = `${Math.min(natural, swFrame)}px`;
    tooltip.classList.add('is-visible');

    const r = anchorEl.getBoundingClientRect();
    const sr = scrollEl.getBoundingClientRect();
    const wr = wrap.getBoundingClientRect();
    // Anchor = the hovered element's bbox center, clamped into the VISIBLE
    // frame.
    const visX = Math.min(Math.max(r.left + r.width / 2, sr.left), sr.right);
    const visY = Math.min(Math.max(r.top + r.height / 2, sr.top), sr.bottom);
    tooltip.style.left = `${visX - wr.left}px`;
    tooltip.style.top = `${visY - wr.top}px`;
    clampIntoFrame();
    requestAnimationFrame(() => {
      if (!tooltip.classList.contains('is-visible')) return;
      clampIntoFrame();
    });
  }

  // Hovering a CONNECTOR opens the state tooltip (Open / Locked / Blocked)
  // — the fat transparent hit-line is the hover surface.
  const EDGE_STATE_WORDS = { open: 'Open', locked: 'Locked', blocked: 'Blocked' };
  function showTooltipForEdge(edgeG) {
    const state = edgeG.getAttribute('data-map-edge') || 'open';
    tooltip.innerHTML = '';
    // Path bubbles: no sub-line, tighter min-width AND tighter side/row
    // padding — the single state word shouldn't swim in the node card's
    // air. Font sizes untouched.
    tooltip.style.minWidth = '34px';
    tooltip.style.padding = '3px 8px';
    const header = document.createElement('div');
    header.className = 'injury-tooltip-header';
    const nameEl = document.createElement('span');
    nameEl.className = 'injury-tooltip-part';
    nameEl.textContent = EDGE_STATE_WORDS[state] || 'Open';
    // Header color mirrors the path state: locked = smoky gray, blocked =
    // crimson; open keeps the brass.
    if (state === 'locked') nameEl.style.color = 'rgba(160,160,160,0.9)';
    if (state === 'blocked') nameEl.style.color = '#DC143C';
    header.appendChild(nameEl);
    tooltip.appendChild(header);
    revealTooltip(edgeG);
  }

  function showTooltipFor(nodeG) {
    // Node bubbles restore the standard width + padding (an edge hover
    // tightened both).
    tooltip.style.minWidth = '';
    tooltip.style.padding = '';
    // A fog "?" stub shows UNDISCOVERED — nothing else is known.
    if (nodeG.hasAttribute('data-map-fog')) {
      tooltip.innerHTML = '';
      const header = document.createElement('div');
      header.className = 'injury-tooltip-header';
      const nameEl = document.createElement('span');
      nameEl.className = 'injury-tooltip-part';
      nameEl.textContent = 'UNDISCOVERED';
      nameEl.style.color = '#3d3d3d'; // charcoal — nothing is known
      header.appendChild(nameEl);
      // No sub-line — no "?", no brackets, and no empty span left behind
      // to pad the card with dead margin below the word.
      tooltip.appendChild(header);
      revealTooltip(nodeG);
      return;
    }
    let info = null;
    try {
      info = JSON.parse(nodeG.getAttribute('data-map-info') || 'null');
    } catch (_) {
      info = null;
    }
    if (!info) return;

    tooltip.innerHTML = '';
    const header = document.createElement('div');
    header.className = 'injury-tooltip-header';
    const nameEl = document.createElement('span');
    nameEl.className = 'injury-tooltip-part';
    nameEl.textContent = String(info.name == null ? '' : info.name).toUpperCase();
    // Header color mirrors exploration: visited keeps the light brass;
    // discovered-but-unvisited reads the dull bronze of unlit paths.
    if (!info.here && info.knowledge === 'discovered') {
      nameEl.style.color = 'rgba(212,175,55,0.5)';
    }
    header.appendChild(nameEl);
    const subEl = document.createElement('span');
    subEl.className = 'injury-tooltip-severity';
    const sub = info.here
      ? 'YOU ARE HERE'
      : info.knowledge === 'discovered'
        ? 'NOT VISITED'
        : 'VISITED';
    subEl.textContent = sub;
    header.appendChild(subEl);
    tooltip.appendChild(header);

    // MARKER PRIORITY: detail lines render in the map key's top-to-bottom
    // marker order (home family first, boss last); untagged geometry lines
    // always sink to the very bottom. (2026-08-25 quest anchors) Anchored
    // objective titles LEAD the whole list — the live pins outrank every
    // static marker — each carrying the scroll glyph.
    const KIND_ORDER = ['general', 'shop', 'safe', 'quest', 'loot', 'hazard', 'friendly', 'hostile', 'boss'];
    const questLines = [];
    for (const q of Array.isArray(info.quests) ? info.quests : []) {
      if (typeof q === 'string' && q.trim()) questLines.push({ text: q, kind: 'quest' });
    }
    const assetLines = [];
    for (const a of Array.isArray(info.assets) ? info.assets : []) {
      if (!a || typeof a.name !== 'string' || !a.name) continue;
      const count = Number(a.count) | 0;
      let name = a.suspected ? `(suspected) ${a.name}` : a.name;
      if (!a.suspected && count > 0) name = `${count} ${name}`;
      let s = name;
      if (!a.suspected && typeof a.state === 'string' && a.state && a.state !== 'active') {
        s += ` (${a.state})`;
      }
      assetLines.push({ text: s, kind: normalizeKind(a.kind) });
    }
    assetLines.sort((x, y) =>
      KIND_ORDER.indexOf(x.kind) - KIND_ORDER.indexOf(y.kind));
    const geoLines = [];
    for (const g of Array.isArray(info.geometry) ? info.geometry : []) {
      if (typeof g === 'string' && g.trim()) geoLines.push({ text: g });
    }
    const lines = [...questLines, ...assetLines, ...geoLines];
    if (lines.length > 0) {
      const divider = document.createElement('div');
      divider.className = 'injury-tooltip-divider';
      tooltip.appendChild(divider);
      const list = document.createElement('ul');
      list.className = 'injury-tooltip-list';
      for (const d of lines) {
        const li = document.createElement('li');
        if (d.kind) {
          li.classList.add('has-ico');
          const ico = document.createElement('span');
          ico.className = 'tip-ico';
          // Static vocabulary markup (never user data) — innerHTML is safe
          // here; the text itself rides textContent below.
          ico.innerHTML = `<svg viewBox="0 0 12 12" width="17" height="17"><g>${ASSET_ICONS[d.kind] || ASSET_ICON_DEFAULT}</g></svg>`;
          li.appendChild(ico);
          const txt = document.createElement('span');
          txt.textContent = d.text;
          li.appendChild(txt);
        } else {
          li.textContent = d.text;
        }
        list.appendChild(li);
      }
      tooltip.appendChild(list);
    }

    revealTooltip(nodeG);
  }

  function hideTooltip() {
    tooltip.classList.remove('is-visible');
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      hideTimer = null;
      if (!tooltip.classList.contains('is-visible')) {
        tooltip.style.display = 'none';
        // Reset the edge-hover tightening once hidden, so a stale compact
        // box never leaks into the next reveal's width measurement.
        tooltip.style.minWidth = '';
        tooltip.style.padding = '';
      }
    }, 200);
  }

  svg.addEventListener('mouseover', (e) => {
    const node = e.target.closest('[data-map-node]');
    if (node && node.ownerSVGElement === svg) { showTooltipFor(node); return; }
    const edge = e.target.closest('[data-map-edge]');
    if (edge && edge.ownerSVGElement === svg) showTooltipForEdge(edge);
  });
  svg.addEventListener('mouseout', (e) => {
    const hit = e.target.closest('[data-map-node], [data-map-edge]');
    const related = e.relatedTarget;
    if (hit && !(related && hit.contains(related))) hideTooltip();
  });
  // Suppress the browser-default click highlight (focus ring / selection)
  // without breaking hover or the click sequence — the injury heatmap's
  // mousedown discipline.
  svg.addEventListener('mousedown', (e) => {
    const hit = e.target.closest('[data-map-node], [data-map-edge]');
    if (hit && hit.ownerSVGElement === svg) e.preventDefault();
  });
  // Keyboard accessibility: focusable nodes reveal the tooltip too (tab
  // order follows the layer render order, entrance-first).
  svg.addEventListener('focusin', (e) => {
    const node = e.target.closest && e.target.closest('[data-map-node]');
    if (node && node.ownerSVGElement === svg) showTooltipFor(node);
  });
  svg.addEventListener('focusout', (e) => {
    const node = e.target.closest && e.target.closest('[data-map-node]');
    if (node) hideTooltip();
  });
  // Scrolling / panning / zooming moves the hovered element without a
  // mouseout firing — the tooltip's anchor would go stale. Hide on scroll
  // (wiredGrabPanning pans by scrolling; wireWheelZoom dispatches 'scroll').
  scrollEl.addEventListener('scroll', hideTooltip, { passive: true });
}

// ─── Grab panning ───────────────────────────────────────────────────────────
// Click-hold-grab panning: no visible scrollbar — hold the mouse down on
// the graph and drag. Panning scrolls, which hides an open tooltip.
// The window-level move/up listeners attach on drag START and detach on
// release — the location card re-mounts on every refresh, and always-on
// window listeners would accumulate dead closures holding detached DOM.
export function wireGrabPanning(scrollEl) {
  if (!scrollEl) return;
  scrollEl.addEventListener('mousedown', (e) => {
    if (e.button !== 0) return;
    const startX = e.clientX;
    const startY = e.clientY;
    const startLeft = scrollEl.scrollLeft;
    const startTop = scrollEl.scrollTop;
    scrollEl.classList.add('is-grabbing');
    e.preventDefault();
    const onMove = (ev) => {
      scrollEl.scrollLeft = startLeft - (ev.clientX - startX);
      scrollEl.scrollTop = startTop - (ev.clientY - startY);
    };
    const onUp = () => {
      window.removeEventListener('mousemove', onMove);
      window.removeEventListener('mouseup', onUp);
      scrollEl.classList.remove('is-grabbing');
    };
    window.addEventListener('mousemove', onMove);
    window.addEventListener('mouseup', onUp);
  });
}

// ─── Wheel zoom ──────────────────────────────────────────────────────────────
// Wheel zoom over the map — scales the SVG (0.7×–2.5×) anchored on the
// cursor: the graph point under the pointer stays under the pointer.
export function wireWheelZoom(scrollEl) {
  if (!scrollEl) return;
  const svg = scrollEl.querySelector('svg.site-map-svg');
  if (!svg) return;
  const baseW = parseFloat(svg.getAttribute('width')) || 1;
  const baseH = parseFloat(svg.getAttribute('height')) || 1;
  let scale = 1;
  scrollEl.addEventListener('wheel', (e) => {
    e.preventDefault();
    const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
    const next = Math.min(2.5, Math.max(0.7, scale * factor));
    if (next === scale) return;
    const rect = scrollEl.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    // Graph (unscaled) coordinates under the cursor before the zoom.
    const ux = (scrollEl.scrollLeft + mx) / scale;
    const uy = (scrollEl.scrollTop + my) / scale;
    scale = next;
    svg.style.width = `${baseW * scale}px`;
    svg.style.height = `${baseH * scale}px`;
    // Re-anchor: keep (ux, uy) under the cursor after the zoom.
    scrollEl.scrollLeft = ux * scale - mx;
    scrollEl.scrollTop = uy * scale - my;
    // Hide any open tooltip (zooming moves everything under it).
    scrollEl.dispatchEvent(new Event('scroll'));
  }, { passive: false });
}

// ─── The map key — one morphing box ─────────────────────────────────────────
// Collapsed = a 3-line hamburger chip pinned bottom-left of the frame.
// Click: the box widens, the 3 lines grow into the full-width underlines
// of the three section heads (Area / Path / Marker) and the rows unfold
// beneath them. Click the box again, or anywhere outside, and it condenses
// back into the chip. Lives in .site-map-wrap (NOT the scroll element), so
// it never moves with pan or zoom; its z-index sits below the tooltip.
let keyDismissWired = false;
function ensureKeyDismissWired() {
  // The click-away condenses any open key; wired ONCE per document (the
  // key is rebuilt on every card re-mount — a per-build listener would
  // accumulate). The box's own click stops propagation so it toggles.
  if (keyDismissWired) return;
  keyDismissWired = true;
  document.addEventListener('click', (e) => {
    document.querySelectorAll('.site-map-key.is-open').forEach((k) => {
      if (!k.contains(e.target)) {
        k.classList.remove('is-open');
        k.setAttribute('aria-expanded', 'false');
      }
    });
  });
}

function keyRow(icoHtml, name) {
  return `<div class="site-map-key-row"><span class="site-map-key-ico">${icoHtml}</span><span class="site-map-key-name">${name}</span></div>`;
}
// One section = head + the morphing rule (a hamburger line collapsed, the
// head's full-width underline expanded) + the folded rows.
function keySection(title, rowsHtml) {
  return `<div class="key-sec"><div class="key-sec-head">${title}</div><span class="key-sec-rule"></span><div class="key-sec-rows"><div class="key-sec-rows-in">${rowsHtml}</div></div></div>`;
}
function chipKey(kind) {
  // Glyph-only key icons — states ride the hover text.
  const glyph = ASSET_ICONS[kind] || ASSET_ICON_DEFAULT;
  return `<svg viewBox="0 0 12 12" width="13" height="13"><g class="site-map-chip"><g class="site-map-chip-glyph">${glyph}</g></g></svg>`;
}
function boxKey(mode) {
  if (mode === 'here') {
    return `<svg width="22" height="13"><rect x="1" y="1" width="20" height="11" rx="2.5" fill="rgba(24,21,14,0.9)" stroke="#F0CE6A" stroke-width="1.6" style="animation: site-map-current-pulse 3.6s ease-in-out infinite;"/></svg>`;
  }
  if (mode === 'entrance') {
    return `<svg width="22" height="13"><rect x="1" y="1" width="20" height="11" rx="2.5" fill="rgba(24,21,14,0.9)" stroke="rgba(212,175,55,0.55)" stroke-width="1.2"/><path d="M1 4 L5 6.5 L1 9 Z" fill="rgba(212,175,55,0.45)"/></svg>`;
  }
  if (mode === 'discovered') {
    return `<svg width="22" height="13"><rect x="1" y="1" width="20" height="11" rx="2.5" fill="rgba(16,16,16,0.7)" stroke="rgba(212,175,55,0.28)" stroke-width="1"/></svg>`;
  }
  return `<svg width="22" height="13"><rect x="1" y="1" width="20" height="11" rx="2.5" fill="rgba(24,21,14,0.9)" stroke="rgba(212,175,55,0.55)" stroke-width="1.2"/></svg>`;
}
function fogKey() {
  return `<svg width="22" height="13"><circle cx="11" cy="6.5" r="5" fill="rgba(10,10,10,0.75)" stroke="rgba(212,175,55,0.25)" stroke-dasharray="2 2.5"/><text x="11" y="9.5" text-anchor="middle" font-size="7" fill="rgba(212,175,55,0.5)">?</text></svg>`;
}
function lineKey(state) {
  return `<svg width="22" height="6"><line x1="1" y1="3" x2="21" y2="3" class="site-map-edge${state ? ' is-' + state : ''}"/></svg>`;
}
export function buildMapLegend() {
  ensureKeyDismissWired();
  const el = document.createElement('button');
  el.type = 'button';
  el.className = 'site-map-key';
  el.setAttribute('aria-expanded', 'false');
  el.setAttribute('aria-label', 'Toggle map key');
  el.innerHTML =
    keySection('Area',
      keyRow(boxKey('here'), 'You are here') +
      keyRow(boxKey('entrance'), 'Entrance') +
      keyRow(boxKey('visited'), 'Visited') +
      keyRow(boxKey('discovered'), 'Discovered') +
      keyRow(fogKey(), 'Undiscovered')) +
    keySection('Path',
      keyRow(lineKey(''), 'Open') +
      keyRow(lineKey('locked'), 'Locked') +
      keyRow(lineKey('blocked'), 'Blocked')) +
    keySection('Marker',
      keyRow(chipKey('general'),
        '<span style="color:#e8e8e8">General</span> / ' +
        '<span style="color:#E8C84A">Shop</span> / ' +
        '<span style="color:#8FC3EE">Safe</span>') +
      keyRow(chipKey('quest'), 'Quest') +
      keyRow(chipKey('loot'), 'Loot') +
      keyRow(chipKey('hazard'), 'Hazard') +
      keyRow(chipKey('friendly'), 'Friendly') +
      keyRow(chipKey('hostile'), 'Hostile') +
      keyRow(chipKey('boss'), 'Boss'));
  el.addEventListener('click', (e) => {
    e.stopPropagation();
    const open = el.classList.toggle('is-open');
    el.setAttribute('aria-expanded', String(open));
  });
  return el;
}
