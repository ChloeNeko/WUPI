# Phase 4 Fix — Travel Graph Seeding from Card (2026-07-28)

> **Status:** SPEC (card data DONE; Rust edits PENDING tree stabilization).
> **Blocks:** Phase 4 Components 3 + 4 are functionally dead in live play
> without this. The consolidated live CDP playtest cannot verify them until
> this lands.
> **Conflict note:** `sim_card.rs`, `lib.rs`, `schema.rs`, `narrator_prompt.rs`
> are all in the other GLM's active edit set (New Game / `protagonist_name`
> purge). Implement ONLY once that work is committed + the tree builds clean.

## Root cause (verified via live CDP playtest 2026-07-28)

A fresh game starts with `travel_graph.nodes = []`, `current_node = None`
(`lib.rs:5719` — `schema::WorldSchema::default()`). The schema doc comment at
`schema.rs:210-212` claims "nodes seeded once by the scenario card," but:

1. **No card in the codebase has a locations block** (verified: grep across
   `apps/fable/cards/*.sim` + `data/*.sim` → zero matches for
   `<locations>`/`<node>`/`neighbors`).
2. **No code path seeds the graph from a card** (`SimCard` has no graph field;
   `fable_start`/`enter_fable_session` build the schema via
   `WorldSchema::default()` with no card-derived graph).
3. **The `[TRAVEL]` bootstrap path is dead code on a fresh game**
   (`lib.rs:4175` — "the FIRST `[TRAVEL]` from `current_node: None` is
   allowed"). It only sets `current_node`; it does NOT create the node. With
   `nodes = []`, the earlier `find_node(&dest).is_none()` check
   (`lib.rs:4188`) rejects the move as "unknown destination" BEFORE the
   bootstrap branch is reached.
4. **Integration tests pass only because they manually pre-seed the graph**
   (`tests/phase4_component3_travel_graph.rs:192` —
   `schema.travel_graph = sample_graph_at_tavern(); schema.travel_graph
   .current_node = None;`). This is the test-vs-reality gap §11.38 warned
   about: mechanics green, live narrator never reaches them.

**Cascade:** without `current_node` set, `[RUMOR]` is dropped (the §11.47
no-current-node path) → Component 4 is dead. Without a seeded graph, the
indoor-weather gate never resolves → Component 2's tick drift is gated. So
Components 2, 3, 4 ALL depend on this fix.

## The fix — card-seeded graph (locked verdict, 2026-07-28)

### 1. Card format: `<locations>` block (DONE — `rusty_tavern.sim`)

New optional `<scenario>` child. Parsed by `sim_card::parse` into
`TravelGraph.nodes`; the first `<node>` in document order seeds
`current_node`. Each node:

```xml
<locations>
  <node id="tavern" setting="indoor">
    <name>The Rusty Lantern Tavern</name>
    <neighbor>cellar</neighbor>
    <neighbor>market_square</neighbor>
  </node>
  ...
</locations>
```

- `id` — bare slug (no `node.` prefix; the `[TRAVEL]` parser strips it for
  ergonomics anyway).
- `setting` — `"indoor"` | `"outdoor"` | empty. Gates the global `weather:`
  line render for this node (the only node→weather coupling in v1).
- `<name>` — diegetic prose label (one per node).
- `<neighbor>` — one element per adjacency edge, bare node id. **Edges are
  undirected in concept but each side must list the other** — the parser
  does NOT symmetrize (matches the `TravelGraph` contract: `neighbors` is
  the source of truth, read by `[TRAVEL]` validation + rumor propagation).

A card with no `<locations>` block stays dormant (pre-Phase-4 behavior).
`rusty_tavern.sim` now has a 6-node graph: tavern ↔ cellar, tavern ↔
market_square, tavern ↔ guest_rooms, market_square ↔ north_road,
north_road ↔ kings_wood. `tavern` is the seed (document order).

### 2. Rust changes (PENDING — 3 surgical edits)

#### Edit A: `sim_card.rs` — `SimCard` gains a `locations` field

Add to the `SimCard` struct (after `player_name`, ~line 91):

```rust
/// Fable Phase 4 Component 3 (2026-07-28): the spatial travel graph seeded
/// from the card's `<scenario><locations>` block. Empty for system cards
/// (Wupi) and for roleplay cards that omit the block (stays dormant — the
/// pre-Phase-4 behavior). When non-empty, `fable_start` seeds
/// `WorldSchema.travel_graph` from this; the first node in document order
/// becomes `current_node` (the player's starting location).
///
/// `Vec<CardNode>` (not `TravelGraph` directly) to keep `sim_card` free of
/// the `schema` module dependency — `fable_start` does the one-line
/// conversion. `#[serde(default)]` keeps older quicksave JSON (bundled
/// cards written before this field existed) loading cleanly.
#[serde(default)]
pub locations: Vec<CardNode>,
```

New sibling struct (in `sim_card.rs`, near `SimCard`):

```rust
/// A location node as authored in a `.sim` card's `<locations>` block.
/// Converted to `schema::Node` by `fable_start`. Kept in `sim_card` (not
/// reusing `schema::Node` directly) so the card parser stays decoupled
/// from the schema module — the conversion is a one-liner at the seed site.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CardNode {
    pub id: String,
    pub name: String,
    pub neighbors: Vec<String>,
    /// `"indoor"` / `"outdoor"` / empty. Free-form; the schema matches
    /// against a tiny known set.
    pub setting: String,
}
```

#### Edit B: `sim_card.rs::parse` — walk `<locations>`

After the `player_name` parse (~line 369), before the `Ok(SimCard { ... })`:

```rust
// Fable Phase 4 Component 3 (2026-07-28): optional <locations> block.
// Each <node> has an `id` attribute, a `setting` attribute (optional),
// a <name> child, and 0+ <neighbor> children (bare node ids). Absent on
// system cards + roleplay cards that don't declare geography → empty Vec
// (dormant graph, pre-Phase-4 behavior).
let locations = scenario
    .and_then(|n| first_child(n, "locations"))
    .map(|loc| {
        loc.children()
            .filter(|c| c.is_element() && c.has_tag_name("node"))
            .map(|node_el| {
                let id = node_el.attribute("id").unwrap_or("").trim().to_owned();
                let setting = node_el.attribute("setting").unwrap_or("").trim().to_owned();
                let name = child_text(node_el, "name").unwrap_or_default();
                let neighbors: Vec<String> = node_el
                    .children()
                    .filter(|c| c.is_element() && c.has_tag_name("neighbor"))
                    .map(|n| text_content(n).trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                CardNode { id, name, neighbors, setting }
            })
            .filter(|n| !n.id.is_empty()) // defensive: drop id-less nodes
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
```

Add `locations` to the `Ok(SimCard { ... })` literal + the `fallback()` fn.

#### Edit C: `lib.rs::enter_fable_session` — seed the graph

After `prior_schema` is resolved (~line 5726, before
`*state.fable_schema.lock().await = prior_schema;` at 5727):

```rust
// Fable Phase 4 Component 3 (2026-07-28): seed the travel graph from the
// card's <locations> block IF the resolved schema has no graph yet. This
// is the load-bearing fix for Components 3 + 4 being dead in live play
// (see docs/phase4-fix-travel-graph-seeding.md). Runs for ALL three
// branches (fresh / resume / explicit save): the card's geography is
// authoritative; a resumed save whose graph is empty (pre-Phase-4 save,
// or a card whose graph was added later) gets the current card's graph.
// A save that already has a seeded graph is left alone (the player's
// current_node + any tracker-added state is preserved).
if prior_schema.travel_graph.nodes.is_empty() && !card.locations.is_empty() {
    prior_schema.travel_graph = schema::TravelGraph {
        nodes: card.locations.iter().map(|cn| schema::Node {
            id: cn.id.clone(),
            name: cn.name.clone(),
            neighbors: cn.neighbors.clone(),
            setting: cn.setting.clone(),
        }).collect(),
        // The first <node> in document order is the seed location.
        current_node: card.locations.first().map(|cn| cn.id.clone()),
    };
    tracing::info!(
        node_count = card.locations.len(),
        seed = ?card.locations.first().map(|n| n.id.as_str()),
        "fable_start: seeded travel_graph from card <locations>"
    );
}
```

Note: `prior_schema` must be `mut` (it already is — it's reassigned from the
match arms). `card` is still in scope here (it's moved into `active_fable_card`
at line 5747, AFTER this point).

### 3. Regression tests (PENDING — FIX-D)

Two new tests in `sim_card.rs`'s test module:

- `card_without_locations_loads_with_empty_graph` — parse a minimal card
  with no `<locations>` block, assert `locations.is_empty()`.
- `card_with_locations_block_parses_nodes_in_order` — parse a card with a
  3-node `<locations>` block, assert the `Vec<CardNode>` matches (ids,
  names, neighbors, settings) in document order.

One new integration test (or extend `phase4_component3_travel_graph.rs`):

- `fresh_game_with_card_locations_seeds_current_node` — simulate the
  `enter_fable_session` seed path: a `WorldSchema::default()` + a card with
  `locations` → after the seed logic, `travel_graph.nodes.len() == N` +
  `current_node == first_node_id`. This is the test that would have caught
  the live-play bug.

## Why this is safe (no regression risk)

- **Card-side:** `<locations>` is optional. Every existing card (Wupi.sim,
  gm.sim, any user-authored .sim) parses unchanged (`locations` defaults to
  empty). Zero migration.
- **Schema-side:** `travel_graph` already has `#[serde(default)]`; the
  seeding only fires when `nodes.is_empty()`. A save that already has a
  graph is untouched.
- **Tracker-side:** with a seeded graph, the Tracker now has REAL node ids
  to reference in `[TRAVEL]` (the `location:` line in `<world_state>` shows
  them). This was likely WHY the Tracker under-emitted `[TRAVEL]` on the
  playtest — it had no nodes to reference. The graph seeding may unblock
  `[TRAVEL]`/`[RUMOR]` emission without any prompt change. (The prompt
  worked-example fix, FIX-C, is still warranted independently.)
- **Test-side:** the existing integration tests (which manually pre-seed)
  still pass — they test the mechanics, not the seeding path.
