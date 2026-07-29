//! Phase 4 Component 3 — Node-Based Spatial Travel Graph: scenario integration test.
//!
//! This is the script-only verification gate for Component 3 (2026-07-28). It
//! exercises the full pure-Rust pipeline end-to-end at the API level — bracket
//! parse → `TravelGraph` mutation → adjacency validation → reject-directive
//! surfacing → render — WITHOUT the live app, WITHOUT the schema lock, WITHOUT
//! IPC. The pieces are pure fns; this wires them together the same way
//! `apply_phase3_bracket_commands`'s TRAVEL arm (lib.rs) does, then asserts the
//! canonical scenarios from the locked design (Chloe's verdict, 2026-07-28):
//!
//!   A. Empty graph dormant (no `location:` line, zero tokens).
//!   B. Bootstrap: first `[TRAVEL cellar]` from `current_node: None` seeds the
//!      initial location (no adjacency check — the player has to start somewhere).
//!   C. Adjacent move succeeds (current_node advances to a declared neighbor).
//!   D. Non-adjacent move rejected (directive surfaced, current_node unchanged).
//!   E. Unknown destination rejected (directive surfaced listing known ids).
//!   F. Last-wins on multiple `[TRAVEL]` in one turn (mirrors [TIME]/[WEATHER]).
//!   G. `node.` prefix stripped in both bracket + JSON forms.
//!   H. Case-insensitive verb (`[travel ...]` / `[Travel ...]`).
//!   I. Indoor current node suppresses the `weather:` line in render.
//!   J. Outdoor current node keeps the `weather:` line.
//!   K. Node with empty `setting` keeps weather (back-compat — the default).
//!   L. Mixed turn: `[TIME]` + `[TRAVEL]` + `[WEATHER]` apply independently.
//!   M. Pre-existing save without `travel_graph` field loads (serde default).
//!   N. Full narrator turn pipeline: the `location:` + `weather:` (gated) hard
//!      facts flow to `<world_state>` as the narrator sees them.
//!
//! The unit tests in `schema.rs`, `bracket_parser.rs`, and `stream_filter.rs`
//! pin each piece in isolation. THIS test is the integration proof that they
//! compose into the contract the brief locked:
//!
//!   "A node-based spatial travel graph: discrete locations + adjacency edges +
//!    a single current location. Rust is the sole authority on legality
//!    (adjacency validation); the Tracker owns 'the player moved' via [TRAVEL]."
//!
//! Verification status: build + unit-test verified only. A consolidated live
//! CDP roleplay playtest (mirroring §11.38) is deferred until all four Phase 4
//! components ship.

use wupi_lib::bracket_parser::{self, BracketCommand};
use wupi_lib::schema::{Node, TravelGraph, Weather, WorldClock, WorldSchema};

// ---------------------------------------------------------------------------
// Helpers that mirror the fable_send schema-lock pipeline. Keeping them
// inline here means the test exercises the REAL public APIs, not a mocked
// shadow of them.
// ---------------------------------------------------------------------------

/// Outcome of a `[TRAVEL]` apply attempt — mirrors what the production apply
/// block surfaces (either a successful advance, or a reject directive string).
enum TravelOutcome {
    /// The move succeeded; `current_node` advanced from `from` to `to`.
    Advanced { from: Option<String>, to: String },
    /// The move was rejected; `current_node` is unchanged. The directive is
    /// what the production code pushes into `reject_directives` for the
    /// narrator's `<directives>` block.
    Rejected { directive: String },
}

/// Apply the last `[TRAVEL ...]` command to a schema's travel_graph,
/// mirroring `apply_phase3_bracket_commands`'s TRAVEL arm (lib.rs). The
/// production helper wraps this in a schema lock + undo snapshot + the
/// reject_directives return tuple; the pure transform + legality logic is
/// identical:
///   (a) destination must exist in the graph (else REJECT);
///   (b) if current_node is set, destination must be a declared neighbor
///       (else REJECT — anti-sycophancy gate);
///   (c) the FIRST `[TRAVEL]` from current_node: None is allowed (bootstrap).
/// Last-wins on multiples (mirrors [TIME] / [WEATHER]).
fn apply_travel_command(
    schema: &mut WorldSchema,
    parsed: &bracket_parser::ParsedNarration,
) -> Option<TravelOutcome> {
    let last_travel = parsed.commands.iter().rev().find_map(|cmd| {
        if let BracketCommand::Travel { destination } = cmd {
            Some(destination.clone())
        } else {
            None
        }
    });
    let dest = last_travel?;

    if schema.travel_graph.find_node(&dest).is_none() {
        // Unknown destination.
        let known: Vec<&str> = schema.travel_graph.nodes.iter().map(|n| n.id.as_str()).collect();
        let directive = format!(
            "Travel to \"{dest}\" is not possible — that location is not in the world. \
             Known locations: {}.",
            if known.is_empty() {
                "(none defined)".to_string()
            } else {
                known.join(", ")
            }
        );
        return Some(TravelOutcome::Rejected { directive });
    }

    if schema.travel_graph.current_node.is_some()
        && !schema.travel_graph.is_adjacent_to_current(&dest)
    {
        // Non-adjacent move.
        let exits: Vec<String> = schema
            .travel_graph
            .current()
            .map(|cur| {
                cur.neighbors
                    .iter()
                    .map(|id| {
                        schema
                            .travel_graph
                            .find_node(id)
                            .map(|n| n.name.clone())
                            .filter(|n| !n.is_empty())
                            .unwrap_or_else(|| id.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let directive = format!(
            "Travel to \"{dest}\" is not possible from the current location \
             (it is not adjacent). Reachable exits: {}.",
            if exits.is_empty() {
                "(none — this location has no declared exits)".to_string()
            } else {
                exits.join(", ")
            }
        );
        return Some(TravelOutcome::Rejected { directive });
    }

    // Legal move (or bootstrap first-move-from-None).
    let prev = schema.travel_graph.current_node.clone();
    schema.travel_graph.current_node = Some(dest.clone());
    Some(TravelOutcome::Advanced { from: prev, to: dest })
}

/// Build the canonical sample graph: a small hub-and-spoke around a tavern.
/// tavern (indoor) ↔ cellar (outdoor), tavern ↔ market_square (empty setting).
/// The tavern is the player's start location. `cellar` and `market_square` are
/// NOT directly connected (the spoke layout — both connect only through tavern).
fn sample_graph_at_tavern() -> TravelGraph {
    TravelGraph {
        nodes: vec![
            Node {
                id: "tavern".to_string(),
                name: "The Rusty Anchor".to_string(),
                neighbors: vec!["cellar".to_string(), "market_square".to_string()],
                setting: "indoor".to_string(),
            },
            Node {
                id: "cellar".to_string(),
                name: "The Cellar".to_string(),
                neighbors: vec!["tavern".to_string()],
                setting: "outdoor".to_string(),
            },
            Node {
                id: "market_square".to_string(),
                name: "Market Square".to_string(),
                neighbors: vec!["tavern".to_string()],
                setting: "".to_string(),
            },
        ],
        current_node: Some("tavern".to_string()),
    }
}

// ===========================================================================
// SCENARIO A — Empty graph dormant
// ===========================================================================
// A fresh game (no scenario-card graph seeded yet) renders no `location:` line
// at all — zero tokens, mirroring world_clock/weather before their first command.

#[test]
fn scenario_a_empty_graph_renders_no_location_line() {
    let schema = WorldSchema::default();
    assert!(!schema.travel_graph.is_set());
    assert_eq!(schema.travel_graph.render_line(), None);
    let rendered = schema.render_for_prompt();
    assert!(!rendered.contains("location:"), "dormant graph leaked a line: {rendered}");
}

// ===========================================================================
// SCENARIO B — Bootstrap: first [TRAVEL] from current_node: None
// ===========================================================================
// The first [TRAVEL] the Tracker emits should seed the initial location WITHOUT
// an adjacency check (the player has to start somewhere — there's no "from" to
// be adjacent to). This is the bootstrap case the brief called out.

#[test]
fn scenario_b_first_travel_seeds_initial_location() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern();
    schema.travel_graph.current_node = None; // unseeded
    let parsed = bracket_parser::parse("You arrive at the tavern. [TRAVEL tavern]");
    assert_eq!(parsed.commands.len(), 1);

    let outcome = apply_travel_command(&mut schema, &parsed);
    match outcome {
        Some(TravelOutcome::Advanced { from, to }) => {
            assert_eq!(from, None, "bootstrap should report from=None");
            assert_eq!(to, "tavern");
        }
        other => panic!("bootstrap travel should succeed, got {:?}", other.map(|o| match o {
            TravelOutcome::Advanced { to, .. } => format!("Advanced({to})"),
            TravelOutcome::Rejected { directive } => format!("Rejected({directive})"),
        })),
    }
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("tavern"));
    // The location now renders.
    assert!(schema.render_for_prompt().contains("location:"));
}

// ===========================================================================
// SCENARIO C — Adjacent move succeeds
// ===========================================================================
// From tavern, the player travels to cellar (a declared neighbor). The move
// succeeds; current_node advances. This is the happy path.

#[test]
fn scenario_c_adjacent_move_succeeds() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern(); // current = tavern
    let parsed = bracket_parser::parse("Mara nods. You head down. [TRAVEL cellar]");
    assert_eq!(parsed.commands.len(), 1);

    let outcome = apply_travel_command(&mut schema, &parsed);
    match outcome {
        Some(TravelOutcome::Advanced { from, to }) => {
            assert_eq!(from.as_deref(), Some("tavern"));
            assert_eq!(to, "cellar");
        }
        _ => panic!("adjacent travel should succeed"),
    }
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("cellar"));
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("location: The Cellar [cellar]"));
}

// ===========================================================================
// SCENARIO D — Non-adjacent move rejected
// ===========================================================================
// From cellar (where the player now is after scenario C), they try to skip
// directly to market_square — but cellar's only neighbor is tavern. The move
// must be REJECTED with a directive listing the actual exits, and current_node
// must NOT change. This is the anti-sycophancy gate.

#[test]
fn scenario_d_non_adjacent_move_rejected() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern();
    schema.travel_graph.current_node = Some("cellar".to_string()); // cellar's only exit: tavern
    let parsed = bracket_parser::parse("You slip out the back. [TRAVEL market_square]");
    assert_eq!(parsed.commands.len(), 1);

    let outcome = apply_travel_command(&mut schema, &parsed);
    match outcome {
        Some(TravelOutcome::Rejected { directive }) => {
            // The directive must mention the destination + the legal exit.
            assert!(directive.contains("market_square"), "directive must name the rejected dest: {directive}");
            assert!(directive.contains("The Rusty Anchor") || directive.contains("tavern"),
                "directive must list the actual exit (tavern): {directive}");
            assert!(directive.contains("not adjacent"), "directive must explain why: {directive}");
        }
        _ => panic!("non-adjacent travel must be rejected"),
    }
    // current_node unchanged.
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("cellar"));
}

// ===========================================================================
// SCENARIO E — Unknown destination rejected
// ===========================================================================
// The Tracker invents a node id ("dragon_lair") that doesn't exist. The move
// must be REJECTED with a directive listing the KNOWN node ids, and current_node
// must NOT change.

#[test]
fn scenario_e_unknown_destination_rejected() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern(); // current = tavern
    let parsed = bracket_parser::parse("You march off. [TRAVEL dragon_lair]");
    assert_eq!(parsed.commands.len(), 1);

    let outcome = apply_travel_command(&mut schema, &parsed);
    match outcome {
        Some(TravelOutcome::Rejected { directive }) => {
            assert!(directive.contains("dragon_lair"), "directive must name the unknown dest: {directive}");
            // Lists the known locations so the next emission can be valid.
            assert!(directive.contains("tavern"), "directive must list known ids: {directive}");
            assert!(directive.contains("cellar"), "directive must list known ids: {directive}");
            assert!(directive.contains("market_square"), "directive must list known ids: {directive}");
            assert!(directive.contains("not in the world"), "directive must explain: {directive}");
        }
        _ => panic!("unknown destination must be rejected"),
    }
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("tavern"));
}

// ===========================================================================
// SCENARIO F — Last-wins on multiple [TRAVEL] in one turn
// ===========================================================================
// A turn with two [TRAVEL] commands (the Tracker waffled, then settled). Mirrors
// the [TIME] / [WEATHER] last-wins contract — only the final one applies.

#[test]
fn scenario_f_multiple_travel_last_wins() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern(); // current = tavern
    // Both cellar and market_square are valid neighbors of tavern. The Tracker
    // emits both; the LAST one (market_square) is authoritative.
    let parsed = bracket_parser::parse("You start for the cellar. [TRAVEL cellar] No wait — the square. [TRAVEL market_square]");
    assert_eq!(parsed.commands.len(), 2);

    let outcome = apply_travel_command(&mut schema, &parsed);
    match outcome {
        Some(TravelOutcome::Advanced { to, .. }) => assert_eq!(to, "market_square"),
        _ => panic!("last-wins travel should succeed"),
    }
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("market_square"));
}

// ===========================================================================
// SCENARIO G — node. prefix stripped in both bracket + JSON forms
// ===========================================================================
// The narrator may emit either bare id ("cellar") or "node.cellar". Both forms
// must parse to the same destination id.

#[test]
fn scenario_g_node_prefix_stripped_in_bracket_form() {
    let parsed = bracket_parser::parse("[TRAVEL node.cellar]");
    assert_eq!(parsed.commands.len(), 1);
    assert_eq!(
        parsed.commands[0],
        BracketCommand::Travel { destination: "cellar".into() }
    );
}

#[test]
fn scenario_g_node_prefix_stripped_in_json_form() {
    let parsed = bracket_parser::parse("```json\n{ \"destination\": \"node.cellar\" }\n```");
    assert_eq!(parsed.commands.len(), 1);
    assert_eq!(
        parsed.commands[0],
        BracketCommand::Travel { destination: "cellar".into() }
    );
}

// ===========================================================================
// SCENARIO H — Case-insensitive verb
// ===========================================================================
// §11.41 follow-up: all command-verb prefixes are case-insensitive. The
// narrator may emit "travel", "Travel", or "TrAvEl".

#[test]
fn scenario_h_case_insensitive_verb() {
    for verb in ["travel", "Travel", "TrAvEl", "TRAVEL"] {
        let parsed = bracket_parser::parse(&format!("[{verb} cellar]"));
        assert_eq!(parsed.commands.len(), 1, "verb={verb}");
        assert_eq!(
            parsed.commands[0],
            BracketCommand::Travel { destination: "cellar".into() }
        );
    }
}

// ===========================================================================
// SCENARIO I — Indoor current node suppresses the weather line
// ===========================================================================
// Component 3 coupling: when the player is indoors, the narrator doesn't see
// weather (a windowless cellar doesn't show rain). The weather field may be
// set, but the `weather:` line is suppressed in render.

#[test]
fn scenario_i_indoor_node_suppresses_weather_line() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern(); // current = tavern (indoor)
    schema.weather = Weather {
        condition: "heavy rain".to_string(),
        started_at_minutes: 1000,
    };
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("location: The Rusty Anchor"));
    assert!(!rendered.contains("weather:"), "indoor node must suppress weather: {rendered}");
}

// ===========================================================================
// SCENARIO J — Outdoor current node keeps the weather line
// ===========================================================================

#[test]
fn scenario_j_outdoor_node_keeps_weather_line() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern();
    schema.travel_graph.current_node = Some("cellar".to_string()); // outdoor
    schema.weather = Weather {
        condition: "heavy rain".to_string(),
        started_at_minutes: 1000,
    };
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("weather: heavy rain"));
    assert!(rendered.contains("location: The Cellar"));
}

// ===========================================================================
// SCENARIO K — Node with empty setting keeps weather (back-compat)
// ===========================================================================
// A node with no `setting` flag (the historical case — pre-Component-3 saves
// won't have setting on any node) defaults to weather-renders. This is the
// back-compat path.

#[test]
fn scenario_k_empty_setting_keeps_weather() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern();
    schema.travel_graph.current_node = Some("market_square".to_string()); // setting = ""
    schema.weather = Weather {
        condition: "clear".to_string(),
        started_at_minutes: 0,
    };
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("weather: clear"));
}

// ===========================================================================
// SCENARIO L — Mixed turn: [TIME] + [TRAVEL] + [WEATHER] apply independently
// ===========================================================================
// A single turn carries all three schema-tracking commands. Each must apply to
// its own typed field without interfering with the others. This is the
// composition test — the production apply_phase3_bracket_commands +
// apply_time_command_and_maybe_tick both run on the same parsed.commands vec.

#[test]
fn scenario_l_mixed_turn_applies_independently() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern(); // current = tavern
    // The Tracker advances the clock, moves the player to the cellar, and
    // shifts the weather — all in one turn.
    let parsed = bracket_parser::parse(
        "Two hours pass. You head to the cellar as the storm breaks. \
         [TIME Day 3, 16:00] [TRAVEL cellar] [WEATHER thunderstorm]",
    );
    assert_eq!(parsed.commands.len(), 3);

    // The production order: TIME first (advances clock + maybe ticks), then
    // the bracket-command batch (EFFECT/MILESTONE/TASK/WEATHER/TRAVEL). For
    // this test we apply them in the same order the production code does.
    // (apply_time_command_and_maybe_tick + apply_phase3_bracket_commands both
    //  scan parsed.commands independently — order within the turn doesn't
    //  matter for the typed fields.)

    // [TIME] — set the clock directly (the production helper parses the raw).
    // Day 3, 16:00 = (3-1)*1440 + 16*60 = 3840 minutes. last_tick_minutes set
    // equal so no spurious world-progression tick fires (this test is about the
    // bracket-command apply, not the tick).
    schema.world_clock = WorldClock { current_minutes: 3840, last_tick_minutes: 3840 };

    // [WEATHER] — apply via the same last-wins pattern.
    let last_weather = parsed.commands.iter().rev().find_map(|c| {
        if let BracketCommand::Weather { condition } = c {
            Some(condition.clone())
        } else {
            None
        }
    });
    if let Some(condition) = last_weather {
        schema.weather = Weather {
            condition,
            started_at_minutes: schema.world_clock.current_minutes,
        };
    }

    // [TRAVEL] — apply via our helper.
    let travel_outcome = apply_travel_command(&mut schema, &parsed);
    assert!(matches!(travel_outcome, Some(TravelOutcome::Advanced { .. })));

    // All three typed fields landed independently.
    assert_eq!(schema.world_clock.current_minutes, 3840);
    assert_eq!(schema.weather.condition, "thunderstorm");
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("cellar"));

    // The render shows all three anchors. Weather IS rendered because cellar
    // is outdoor.
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("clock: Day 3, 16:00"));
    assert!(rendered.contains("weather: thunderstorm"));
    assert!(rendered.contains("location: The Cellar [cellar]"));
}

// ===========================================================================
// SCENARIO M — Pre-existing save without travel_graph field loads
// ===========================================================================
// A pre-Component-3 save (no "travel_graph" field in JSON) must deserialize to
// an empty dormant graph. The #[serde(default)] attribute enforces this; this
// test pins it end-to-end through serde_json.

#[test]
fn scenario_m_pre_component3_save_loads_as_empty_graph() {
    let pre_comp3_json = r#"{
        "summary": "A tavern scene.",
        "recent_events": ["The player arrived."],
        "entities": {},
        "player_state": {},
        "world_clock": {"current_minutes": 1440, "last_tick_minutes": 0},
        "weather": {"condition": "rain", "started_at_minutes": 1000},
        "immutable_keys": [],
        "scene_pacing": {"mode": "Exploration", "spatial": 0, "emotional": 0, "kinetic": 0},
        "status_tags": [],
        "relationships": {},
        "offscreen_tasks": []
    }"#;
    let parsed: WorldSchema =
        serde_json::from_str(pre_comp3_json).expect("pre-Component-3 JSON must deserialize");
    assert!(!parsed.travel_graph.is_set());
    assert!(parsed.travel_graph.nodes.is_empty());
    assert_eq!(parsed.travel_graph.current_node, None);
    // Other fields load normally.
    assert_eq!(parsed.weather.condition, "rain");
    assert_eq!(parsed.world_clock.current_minutes, 1440);
}

// ===========================================================================
// SCENARIO N — Full narrator turn pipeline: hard facts flow to <world_state>
// ===========================================================================
// The end-to-end: a graph is seeded, the player is at the tavern, weather is
// set. The render produces the full set of anchors the narrator sees. This is
// the "hard facts to the blindfolded narrator" contract — the API narrator
// (stage 2 in the §11.42 DM/Voice-Actor split) inherits exactly this block.

#[test]
fn scenario_n_full_pipeline_renders_all_anchors() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = sample_graph_at_tavern(); // current = tavern (indoor)
    schema.world_clock = WorldClock { current_minutes: 1440, last_tick_minutes: 0 };
    schema.weather = Weather {
        condition: "heavy rain".to_string(),
        started_at_minutes: 1200,
    };

    let rendered = schema.render_for_prompt();

    // Clock anchor.
    assert!(rendered.contains("clock: Day 2, 00:00"), "clock anchor missing: {rendered}");
    // Location anchor — the player is at the tavern, with exits to cellar +
    // market_square (rendered as diegetic names).
    assert!(
        rendered.contains("location: The Rusty Anchor [tavern] (exits: The Cellar, Market Square)"),
        "location anchor with exits missing: {rendered}",
    );
    // Weather is SUPPRESSED because the tavern is indoor — the indoor gate is
    // the only node→weather coupling in v1, and it must fire here.
    assert!(!rendered.contains("weather:"), "indoor tavern must suppress weather: {rendered}");

    // Now move the player outdoors + re-render — weather should appear.
    schema.travel_graph.current_node = Some("cellar".to_string());
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("weather: heavy rain"));
    assert!(rendered.contains("location: The Cellar [cellar]"));
}
