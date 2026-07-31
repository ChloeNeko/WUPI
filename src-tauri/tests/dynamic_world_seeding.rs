//! Dynamic World Seeding — `[DISCOVER]` + `[NPC_REGISTER]`: scenario
//! integration test.
//!
//! This is the script-only verification gate for the dynamic-seeding feature
//! (the fix for sandbox cards that seed no `<locations>`/`<cast>` block). It
//! exercises the full pure-Rust pipeline end-to-end at the API level — bracket
//! parse → `TravelGraph`/`NpcRegistry` mutation → the downstream `[TRAVEL]`/
//! `[PRESENCE]` legality check — WITHOUT the live app, WITHOUT the schema
//! lock, WITHOUT IPC.
//!
//! The pieces are pure fns; this wires them together the same way
//! `apply_phase3_bracket_commands`'s DISCOVER + NPC_REGISTER arms (lib.rs) do,
//! then asserts the canonical contract:
//!
//!   A. `[DISCOVER]` on an empty graph registers a node + seeds current_node.
//!   B. After `[DISCOVER]`, a subsequent `[TRAVEL]` to that node is LEGAL
//!      (the core claim — the discovery unblocks the frozen-travel bug).
//!   C. `[DISCOVER]` back-links existing neighbors (undirected graph).
//!   D. `[DISCOVER]` is idempotent (re-discovery is a no-op).
//!   E. `[NPC_REGISTER]` inserts a new NPC into the registry.
//!   F. After `[NPC_REGISTER]`, a subsequent `[PRESENCE]` for that id RESOLVES
//!      (the core claim — the registration unblocks the frozen-presence bug).
//!   G. `[NPC_REGISTER]` is idempotent (re-registration is a no-op).
//!   H. A sandbox schema (no seeded locations/cast) renders no location:/present:
//!      lines until discovery/registration happens, then renders them after.
//!   I. Id sanitization: a messy tracker id becomes a clean bare slug.
//!
//! The unit tests in `schema.rs` + `bracket_parser.rs` pin each piece in
//! isolation. THIS test is the integration proof that they compose into the
//! contract: discovery makes the world reachable, registration makes NPCs
//! addressable.
//!
//! Verification status: build + unit-test verified only. A live CDP playtest
//! to confirm the tracker actually *emits* `[DISCOVER]`/`[NPC_REGISTER]` at
//! the right moments is a separate gate.

use wupi_lib::bracket_parser::{self, BracketCommand};
use wupi_lib::schema::{NpcEntry, NpcRegistry, Node, TravelGraph, WorldSchema};

// ---------------------------------------------------------------------------
// Helpers that mirror the production apply pipeline (lib.rs). Pure fns over
// the public schema API — the production helper wraps these in a schema lock
// + undo snapshot; the transform logic is identical.
// ---------------------------------------------------------------------------

/// Mirror `sanitize_slug` (lib.rs): lowercase, non-alphanumeric → `_`, trim.
fn sanitize_slug(raw: &str) -> String {
    raw.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_owned()
}

/// Apply a `[DISCOVER]` command to a schema's travel_graph, mirroring the
/// production DISCOVER arm in `apply_phase3_bracket_commands`. Idempotent;
/// back-links existing neighbors; seeds current_node on the first discovery
/// (the empty-graph bootstrap). Returns true if a node was inserted.
fn apply_discover(schema: &mut WorldSchema, cmd: &BracketCommand) -> bool {
    let BracketCommand::Discover { node_id, name, setting, neighbors } = cmd else {
        return false;
    };
    let id = sanitize_slug(node_id);
    if id.is_empty() {
        return false;
    }
    let label = if name.trim().is_empty() {
        id.clone()
    } else {
        name.trim().to_string()
    };
    let node = Node {
        id: id.clone(),
        name: label,
        neighbors: neighbors.iter().map(|n| sanitize_slug(n)).filter(|n| !n.is_empty()).collect(),
        setting: setting.trim().to_string(),
    };
    let was_empty = !schema.travel_graph.is_set();
    let inserted = schema.travel_graph.upsert_node(node);
    if inserted && was_empty {
        schema.travel_graph.current_node = Some(id);
    }
    inserted
}

/// Apply an `[NPC_REGISTER]` command, mirroring the production NPC_REGISTER
/// arm. Idempotent. Returns true if an entry was inserted.
fn apply_npc_register(schema: &mut WorldSchema, cmd: &BracketCommand) -> bool {
    let BracketCommand::NpcRegister { npc_id, name, role, tier } = cmd else {
        return false;
    };
    let id = sanitize_slug(npc_id);
    if id.is_empty() {
        return false;
    }
    let label = if name.trim().is_empty() { id.clone() } else { name.trim().to_string() };
    schema.npc_registry.upsert_entry(NpcEntry {
        id: id.clone(),
        name: label,
        role: role.trim().to_string(),
        tier: tier.clone().filter(|t| !t.trim().is_empty()),
        aliases: vec![id],
    })
}

/// The downstream `[TRAVEL]` legality check (mirrors the production TRAVEL arm
/// — the gate that was frozen-dead before discovery). Returns true if a move
/// to `dest` would be legal: destination exists AND (it's the current node, OR
/// it's adjacent, OR this is the bootstrap first-move-from-None).
fn travel_is_legal(schema: &WorldSchema, dest: &str) -> bool {
    if schema.travel_graph.find_node(dest).is_none() {
        return false;
    }
    // Staying put (dest == current) is trivially legal.
    if schema.travel_graph.current_node.as_deref() == Some(dest) {
        return true;
    }
    // Bootstrap: first move from None is allowed.
    if schema.travel_graph.current_node.is_none() {
        return true;
    }
    schema.travel_graph.is_adjacent_to_current(dest)
}

// ===========================================================================
// A. DISCOVER on empty graph registers a node + seeds current_node
// ===========================================================================

#[test]
fn discover_on_empty_graph_seeds_current_node() {
    let mut schema = WorldSchema::default();
    assert!(!schema.travel_graph.is_set(), "fresh schema has no graph");

    let parsed = bracket_parser::parse("[DISCOVER shell_town name=\"Shell Town\" setting=outdoor]");
    assert_eq!(parsed.commands.len(), 1);
    let inserted = apply_discover(&mut schema, &parsed.commands[0]);
    assert!(inserted);

    // The node exists + current_node was auto-seeded (empty-graph bootstrap).
    assert_eq!(schema.travel_graph.nodes.len(), 1);
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("shell_town"));
    assert_eq!(schema.travel_graph.current().unwrap().name, "Shell Town");
}

// ===========================================================================
// B. Core CLAIM: after DISCOVER, a subsequent TRAVEL to that node is LEGAL
// ===========================================================================

#[test]
fn discover_unblocks_subsequent_travel() {
    // This is the whole point of the feature. Before: a sandbox card with no
    // seeded graph made [TRAVEL] always-reject for the entire session (the node
    // didn't exist). After: a [DISCOVER] registers the node, and the node
    // becomes a legal travel target.
    let mut schema = WorldSchema::default();

    // Travel to an undiscovered node → ILLEGAL (the pre-feature bug: the node
    // isn't in the graph at all).
    assert!(!travel_is_legal(&schema, "shell_town"));

    // Discover shell_town (empty-graph bootstrap seeds current_node = shell_town).
    apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER shell_town name=\"Shell Town\"]").commands[0]);
    // shell_town now exists in the graph → reachable.
    assert!(travel_is_legal(&schema, "shell_town"), "discovered node is reachable");

    // Discover an adjacent node. Travel there is LEGAL via adjacency — the
    // load-bearing unblock: the player can now actually move around the world.
    apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER foosha name=\"Foosha Village\" neighbors=shell_town]").commands[0]);
    assert!(travel_is_legal(&schema, "foosha"),
        "adjacent discovered node is a legal travel target");

    // And a node that was never discovered stays illegal (the gate still holds).
    assert!(!travel_is_legal(&schema, "skypiea"),
        "undiscovered node remains unreachable");
}

// ===========================================================================
// C. DISCOVER back-links existing neighbors (undirected graph)
// ===========================================================================

#[test]
fn discover_back_links_existing_neighbor() {
    let mut schema = WorldSchema::default();
    // Discover A first (seeds current_node = A).
    apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER island_a name=\"Island A\"]").commands[0]);
    // Discover B naming A as a neighbor → A should gain a back-edge to B.
    apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER island_b name=\"Island B\" neighbors=island_a]").commands[0]);

    assert!(schema.travel_graph.find_node("island_a").unwrap().neighbors.contains(&"island_b".to_string()),
        "A back-linked to B");
    assert!(schema.travel_graph.find_node("island_b").unwrap().neighbors.contains(&"island_a".to_string()),
        "B lists A (forward edge)");
}

// ===========================================================================
// D. DISCOVER is idempotent (re-discovery is a no-op)
// ===========================================================================

#[test]
fn discover_is_idempotent() {
    let mut schema = WorldSchema::default();
    apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER shell_town name=\"Shell Town\"]").commands[0]);
    // Re-discover with different data → no-op (first writer wins).
    let inserted = apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER shell_town name=\"DIFFERENT\"]").commands[0]);
    assert!(!inserted);
    assert_eq!(schema.travel_graph.nodes.len(), 1);
    assert_eq!(schema.travel_graph.find_node("shell_town").unwrap().name, "Shell Town");
}

// ===========================================================================
// E. NPC_REGISTER inserts a new NPC into the registry
// ===========================================================================

#[test]
fn npc_register_inserts_entry() {
    let mut schema = WorldSchema::default();
    assert!(!schema.npc_registry.is_set());

    let parsed = bracket_parser::parse("[NPC_REGISTER coby name=Coby role=\"timid Marine recruit\" tier=elite]");
    let inserted = apply_npc_register(&mut schema, &parsed.commands[0]);
    assert!(inserted);
    assert_eq!(schema.npc_registry.entries.len(), 1);
    assert_eq!(schema.npc_registry.find("coby").unwrap().name, "Coby");
    // The id is registered as its own alias so [PRESENCE coby] resolves.
    assert!(schema.npc_registry.resolve("coby").is_some());
}

// ===========================================================================
// F. CORE CLAIM: after NPC_REGISTER, a subsequent PRESENCE RESOLVES
// ===========================================================================

#[test]
fn npc_register_unblocks_subsequent_presence() {
    // Before: a sandbox card with no seeded cast made [PRESENCE] always-reject
    // (the anti-hallucination gate rejected unknown ids). After: an
    // [NPC_REGISTER] makes the id resolvable.
    let mut schema = WorldSchema::default();
    assert!(schema.npc_registry.resolve("coby").is_none(), "unknown id doesn't resolve");

    apply_npc_register(&mut schema, &bracket_parser::parse("[NPC_REGISTER coby name=Coby]").commands[0]);

    // Now the [PRESENCE] surface form resolves to the canonical entry.
    let resolved = schema.npc_registry.resolve("coby");
    assert!(resolved.is_some(), "registered id resolves for [PRESENCE]");
    assert_eq!(resolved.unwrap().name, "Coby");
}

// ===========================================================================
// G. NPC_REGISTER is idempotent (re-registration is a no-op)
// ===========================================================================

#[test]
fn npc_register_is_idempotent() {
    let mut schema = WorldSchema::default();
    apply_npc_register(&mut schema, &bracket_parser::parse("[NPC_REGISTER coby name=Coby]").commands[0]);
    let inserted = apply_npc_register(&mut schema, &bracket_parser::parse("[NPC_REGISTER coby name=DIFFERENT]").commands[0]);
    assert!(!inserted);
    assert_eq!(schema.npc_registry.entries.len(), 1);
    assert_eq!(schema.npc_registry.find("coby").unwrap().name, "Coby");
}

// ===========================================================================
// H. Sandbox schema renders no location:/present: until discovery/registration
//     (location: comes from the travel_graph current node; the NPC registry is
//     the valid-targets whitelist for [PRESENCE], surfaced separately — a
//     registered NPC becomes a resolvable target, not an on-camera presence)
// ===========================================================================

#[test]
fn sandbox_renders_no_anchors_until_seeded() {
    let mut schema = WorldSchema::default();
    let render = schema.render_for_prompt();
    assert!(!render.contains("location:"), "no location: line before any discovery");

    apply_discover(&mut schema, &bracket_parser::parse("[DISCOVER shell_town name=\"Shell Town\"]").commands[0]);
    apply_npc_register(&mut schema, &bracket_parser::parse("[NPC_REGISTER coby name=Coby]").commands[0]);

    let render = schema.render_for_prompt();
    // After discovery, the location: anchor appears (render_for_prompt reads
    // travel_graph fresh — this is why no render change was needed).
    assert!(render.contains("Shell Town"), "discovered location renders in location: line");

    // The registry is the [PRESENCE] valid-target whitelist, not the on-camera
    // list — so registration doesn't put Coby in the present: line (that
    // requires a [PRESENCE] assertion). But the registry DOES render its own
    // roster line, and resolve() now finds Coby (test F covers that). Here we
    // confirm the registry holds the entry.
    assert!(schema.npc_registry.resolve("coby").is_some(),
        "registered NPC is a valid [PRESENCE] target");
    assert!(schema.npc_registry.render_line().unwrap_or_default().contains("Coby"),
        "registered NPC appears in the registry roster");
}

// ===========================================================================
// I. Id sanitization: messy tracker id → clean bare slug
// ===========================================================================

#[test]
fn sanitize_slug_cleans_messy_tracker_id() {
    // The tracker might emit a diegetic name as the id; the applier slugifies
    // (every non-alphanumeric char → `_`, so comma+space = double underscore).
    assert_eq!(sanitize_slug("Shell Town!"), "shell_town");
    assert_eq!(sanitize_slug("Loguetown, the Town of Beginnings"), "loguetown__the_town_of_beginnings");
    assert_eq!(sanitize_slug("  already_clean  "), "already_clean");
    assert_eq!(sanitize_slug("---weird---"), "weird");
    assert_eq!(sanitize_slug("!!!"), "", "all-punctuation → empty → skipped");
}
