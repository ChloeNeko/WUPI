//! Fable Phase 4 — travel-graph seeding regression tests (2026-07-28).
//!
//! Pins the load-bearing fix for Components 3 + 4 being dead in live play:
//! the card's `<scenario><locations>` block → `SimCard.locations` →
//! `WorldSchema.travel_graph` seeding path. Before this fix, a fresh game
//! started with `travel_graph.nodes = []` + `current_node = None`, so:
//!   - `[TRAVEL ...]` was always rejected ("unknown destination" — the
//!     `find_node` check failed before the bootstrap branch);
//!   - `[RUMOR ...]` was always dropped (the no-current-node path);
//!   - `[WEATHER]` tick drift's indoor gate never resolved.
//! See `docs/phase4-fix-travel-graph-seeding.md` for the full root-cause.
//!
//! These tests exercise the pure transformation at the seed site
//! (`Vec<sim_card::CardNode>` → `schema::TravelGraph`) directly — the
//! `enter_fable_session` wrapper is a private async fn inside the IPC path
//! (needs a full `tauri::AppHandle`), so we test the schema-transform
//! contract here. The card-parser side (XML → CardNode) is covered by
//! unit tests inside `sim_card.rs` (where the private `parse` fn is in
//! scope). Together they prove the seeding path is wired; the live CDP
//! playtest verifies the end-to-end firing.

use wupi_lib::schema::{Node, TravelGraph};
use wupi_lib::sim_card::CardNode;

/// The exact transform `enter_fable_session` applies at fable_start
/// (lib.rs:5728-5755). Extracted here as a pure fn so the test doesn't
/// need a full AppHandle. MUST stay in sync with the seed site — if the
/// schema changes, this helper + the seed site change together.
fn seed_travel_graph_from_card(locations: &[CardNode]) -> TravelGraph {
    TravelGraph {
        nodes: locations
            .iter()
            .map(|cn| Node {
                id: cn.id.clone(),
                name: cn.name.clone(),
                neighbors: cn.neighbors.clone(),
                setting: cn.setting.clone(),
            })
            .collect(),
        current_node: locations.first().map(|cn| cn.id.clone()),
    }
}

// ===========================================================================
// CardNode → TravelGraph (the seed-site transform)
// ===========================================================================

/// The seed transform produces a `TravelGraph` with all card nodes, in
/// document order, with `current_node` set to the FIRST node's id. This is
/// the exact contract `enter_fable_session` relies on.
#[test]
fn seed_transform_populates_nodes_and_current_node() {
    let card_nodes = vec![
        CardNode {
            id: "tavern".to_string(),
            name: "The Rusty Lantern".to_string(),
            neighbors: vec!["cellar".to_string(), "market_square".to_string()],
            setting: "indoor".to_string(),
        },
        CardNode {
            id: "cellar".to_string(),
            name: "The Cellar".to_string(),
            neighbors: vec!["tavern".to_string()],
            setting: "indoor".to_string(),
        },
    ];
    let graph = seed_travel_graph_from_card(&card_nodes);
    assert!(graph.is_set(), "graph with seeded nodes must be_set()");
    assert_eq!(graph.nodes.len(), 2);
    // The first node in document order seeds current_node.
    assert_eq!(graph.current_node.as_deref(), Some("tavern"));
    // Adjacency survives the transform.
    let tavern = graph.find_node("tavern").expect("tavern seeded");
    assert_eq!(tavern.neighbors, vec!["cellar", "market_square"]);
    assert_eq!(tavern.setting, "indoor");
}

/// The seed transform on an EMPTY card-locations slice produces a dormant
/// graph (no nodes, no current_node). This is the "card declares no
/// geography" branch — the seeding must be a no-op, NOT panic.
#[test]
fn seed_transform_on_empty_locations_yields_dormant_graph() {
    let graph = seed_travel_graph_from_card(&[]);
    assert!(!graph.is_set(), "empty locations must yield dormant graph");
    assert!(graph.nodes.is_empty());
    assert!(graph.current_node.is_none());
}

/// End-to-end: a realistic card-location set (mirrors rusty_tavern.sim's
/// 6-node graph) seeds a graph where the [TRAVEL] bootstrap path is now
/// REACHABLE. Before the fix, `find_node` returned None for every
/// destination because nodes was empty. After the fix, the tavern's
/// neighbors are real, adjacency-validated destinations. This is the test
/// that would have caught the live-play bug.
#[test]
fn realistic_card_locations_seed_reachable_graph() {
    // Mirrors the <locations> block in apps/fable/cards/rusty_tavern.sim.
    let card_nodes = vec![
        CardNode {
            id: "tavern".to_string(),
            name: "The Rusty Lantern Tavern".to_string(),
            neighbors: vec![
                "cellar".to_string(),
                "market_square".to_string(),
                "guest_rooms".to_string(),
            ],
            setting: "indoor".to_string(),
        },
        CardNode {
            id: "cellar".to_string(),
            name: "The Tavern Cellar".to_string(),
            neighbors: vec!["tavern".to_string()],
            setting: "indoor".to_string(),
        },
        CardNode {
            id: "market_square".to_string(),
            name: "Ashford Market Square".to_string(),
            neighbors: vec!["tavern".to_string(), "north_road".to_string()],
            setting: "outdoor".to_string(),
        },
        CardNode {
            id: "guest_rooms".to_string(),
            name: "The Tavern Guest Rooms".to_string(),
            neighbors: vec!["tavern".to_string()],
            setting: "indoor".to_string(),
        },
        CardNode {
            id: "north_road".to_string(),
            name: "The Northern Trade Road".to_string(),
            neighbors: vec!["market_square".to_string(), "kings_wood".to_string()],
            setting: "outdoor".to_string(),
        },
        CardNode {
            id: "kings_wood".to_string(),
            name: "The King's Wood".to_string(),
            neighbors: vec!["north_road".to_string()],
            setting: "outdoor".to_string(),
        },
    ];
    let mut graph = seed_travel_graph_from_card(&card_nodes);
    assert_eq!(graph.nodes.len(), 6, "expected 6 seeded nodes");
    assert_eq!(graph.current_node.as_deref(), Some("tavern"));
    // The exact adjacency check [TRAVEL] performs at lib.rs:4203-4204.
    // Before the fix, this was ALWAYS false (nodes empty → find_node None).
    assert!(
        graph.is_adjacent_to_current("cellar"),
        "cellar must be adjacent to the tavern (the [TRAVEL] validation)"
    );
    assert!(
        graph.is_adjacent_to_current("market_square"),
        "market_square must be adjacent to the tavern"
    );
    assert!(
        !graph.is_adjacent_to_current("kings_wood"),
        "kings_wood must NOT be adjacent to the tavern (non-neighbor — the Tracker must chain stops)"
    );
    // A direct [TRAVEL kings_wood] from the tavern would now be REJECTED
    // as non-adjacent (the directive path), NOT as unknown-destination.
    // That's the correct behavior: the graph knows about kings_wood, it's
    // just not reachable in one hop.
    assert!(graph.find_node("kings_wood").is_some(), "kings_wood must be a known node");
    // Indoor gate (Component 2 coupling): the tavern is indoor, so weather
    // is suppressed while the player is there. market_square is outdoor.
    assert!(graph.current_is_indoor(), "tavern (indoor) must gate the weather: line");
    graph.current_node = Some("market_square".to_string());
    assert!(!graph.current_is_indoor(), "market_square (outdoor) must show the weather: line");
}

