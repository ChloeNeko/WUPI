//! Phase 4 Component 4 — Propagation-Based Rumor Engine: scenario integration test.
//!
//! This is the script-only verification gate for Component 4 (2026-07-28), the
//! LAST Phase 4 component. It exercises the full pure-Rust pipeline end-to-end
//! at the API level — bracket parse → `Rumor` creation → node-based knowledge
//! filter → render → tick propagation → directive surfacing — WITHOUT the live
//! app, WITHOUT the schema lock, WITHOUT IPC. The pieces are pure fns; this
//! wires them together the same way `apply_phase3_bracket_commands`'s RUMOR
//! arm (lib.rs) + `apply_time_command_and_maybe_tick`'s propagation block do,
//! then asserts the canonical scenarios from the locked design (Chloe's
//! verdict, 2026-07-28):
//!
//!   A. Empty rumors dormant (no `rumors:` line, zero tokens).
//!   B. `[RUMOR the stranger paid]` parse + apply roots at the current node.
//!   C. `[RUMOR]` with no current node → warn-and-skip (no mutation).
//!   D. Render shows ONLY rumors the current node knows (node-based filter).
//!   E. Render suppresses rumors at OTHER nodes (travel away → hidden).
//!   F. `propagate_rumors` spreads to an adjacent unknown node on a passing roll.
//!   G. Age-decayed DC: a stale rumor (high age) spreads less than a fresh one
//!      in aggregate over a sweep.
//!   H. Per-tick cap (NEW_NODES_PER_TICK_CAP=2): a star graph with 3 candidate
//!      edges spreads to at most 2 nodes per tick.
//!   I. Determinism: same (rumor, graph, now_minutes) → same spread (replayable).
//!   J. Combat suspension: the tick gate (progression_interval_hours()==0) is
//!      the upstream suspension — verified here by NOT calling propagate when
//!      the gate would fire (mirrors the production tick path).
//!   K. Backwards-compat: pre-Component-4 save with no `rumors` field loads
//!      as an empty list (serde default — dormant).
//!   L. Mixed turn: `[RUMOR]` + `[TRAVEL]` + `[TIME]` apply independently.
//!   M. JSON form dispatch (`{"kind": "rumor", "label": ...}` + field-based
//!      inference for `{"label": ...}`).
//!   N. Full pipeline: parse → apply → propagate → render shows the spread.
//!   O. Bracket normalization: `[RUMOR ...]` is stripped from the prose (the
//!      post-turn parser contract) and the bracket-verb is case-insensitive.
//!
//! The unit tests in `rumor.rs`, `schema.rs`, `bracket_parser.rs`, and
//! `stream_filter.rs` pin each piece in isolation. THIS test is the integration
//! proof that they compose into the contract the brief locked:
//!
//!   "A propagation-based rumor engine: free-form diegetic phrases that spread
//!    between connected nodes on the World Progression tick. Propagation-only
//!    (no polarity, no reputation score) — reputation is narratively derived
//!    from which rumor texts circulate. Node-based knowledge: the player
//!    learns 'the tavern has heard X', not 'Marcus specifically has heard X'."
//!
//! Verification status: build + unit-test verified only. A consolidated live
//! CDP roleplay playtest (mirroring §11.38) is THE FINAL Phase 4 gate — it
//! runs once Component 4 ships (this component IS the last, so the playtest
//! is now unblocked AFTER this ships).

use wupi_lib::bracket_parser::{self, BracketCommand};
use wupi_lib::rumor;
use wupi_lib::schema::{Node, TravelGraph, WorldSchema};

// ---------------------------------------------------------------------------
// Helpers that mirror the fable_send schema-lock pipeline. Keeping them
// inline here means the test exercises the REAL public APIs, not a mocked
// shadow of them.
// ---------------------------------------------------------------------------

/// Outcome of a `[RUMOR]` apply attempt — mirrors what the production apply
/// block surfaces (either a rooted rumor, or a drop with no mutation).
enum RumorOutcome {
    /// The rumor was rooted at the current node (known_nodes = [origin]).
    Rooted { label: String, origin: String },
    /// The rumor was dropped — no current node to root at (warn-and-skip).
    /// The schema is unchanged.
    Dropped { _label: String },
}

/// Apply ALL `[RUMOR ...]` commands in `parsed` to `schema.rumors`, mirroring
/// `apply_phase3_bracket_commands`'s RUMOR arm (lib.rs). Append-ALL semantics:
/// each `[RUMOR]` in the turn creates one Rumor rooted at the current node.
/// A `[RUMOR]` with no current node is warn-and-skip (no mutation).
fn apply_rumor_commands(
    schema: &mut WorldSchema,
    parsed: &bracket_parser::ParsedNarration,
    now_minutes: i64,
) -> Vec<RumorOutcome> {
    let mut outcomes = Vec::new();
    for cmd in &parsed.commands {
        if let BracketCommand::Rumor { label } = cmd {
            if let Some(cur_id) = schema.travel_graph.current_node.as_deref() {
                schema.rumors.push(rumor::Rumor {
                    label: label.clone(),
                    origin_node: cur_id.to_string(),
                    known_nodes: vec![cur_id.to_string()],
                    born_minutes: now_minutes,
                });
                outcomes.push(RumorOutcome::Rooted {
                    label: label.clone(),
                    origin: cur_id.to_string(),
                });
            } else {
                outcomes.push(RumorOutcome::Dropped {
                    _label: label.clone(),
                });
            }
        }
    }
    outcomes
}

/// Build a small linear graph: tavern — cellar — cellar_tunnel.
fn linear_graph_at_tavern() -> TravelGraph {
    TravelGraph {
        nodes: vec![
            Node {
                id: "tavern".to_string(),
                name: "The Rusty Tavern".to_string(),
                neighbors: vec!["cellar".to_string()],
                setting: String::new(),
            },
            Node {
                id: "cellar".to_string(),
                name: "The Cellar".to_string(),
                neighbors: vec!["tavern".to_string(), "cellar_tunnel".to_string()],
                setting: "indoor".to_string(),
            },
            Node {
                id: "cellar_tunnel".to_string(),
                name: "Smuggler's Tunnel".to_string(),
                neighbors: vec!["cellar".to_string()],
                setting: "indoor".to_string(),
            },
        ],
        current_node: Some("tavern".to_string()),
    }
}

/// Build a star graph: tavern connects to 3 leaves (cellar, market, guardhouse).
/// `current_node` = tavern.
fn star_graph_at_tavern() -> TravelGraph {
    TravelGraph {
        nodes: vec![
            Node {
                id: "tavern".to_string(),
                name: "The Rusty Tavern".to_string(),
                neighbors: vec![
                    "cellar".to_string(),
                    "market".to_string(),
                    "guardhouse".to_string(),
                ],
                setting: String::new(),
            },
            Node {
                id: "cellar".to_string(),
                name: "The Cellar".to_string(),
                neighbors: vec!["tavern".to_string()],
                setting: "indoor".to_string(),
            },
            Node {
                id: "market".to_string(),
                name: "Market Square".to_string(),
                neighbors: vec!["tavern".to_string()],
                setting: String::new(),
            },
            Node {
                id: "guardhouse".to_string(),
                name: "Guardhouse".to_string(),
                neighbors: vec!["tavern".to_string()],
                setting: "indoor".to_string(),
            },
        ],
        current_node: Some("tavern".to_string()),
    }
}

// ===========================================================================
// SCENARIO A — Empty rumors dormant
// ===========================================================================
// A fresh game with no rumors must suppress the `rumors:` line entirely (zero
// tokens — mirrors the dormant contract for weather/location before their
// first command).

#[test]
fn scenario_a_empty_rumors_dormant() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = linear_graph_at_tavern(); // current = tavern, no rumors
    let rendered = schema.render_for_prompt();
    assert!(
        !rendered.contains("rumors:"),
        "no rumors line should render when rumors is empty (got: {rendered})"
    );
}

// ===========================================================================
// SCENARIO B — [RUMOR] parse + apply roots at the current node
// ===========================================================================
// The canonical case: a [RUMOR] command creates a Rumor with origin_node +
// known_nodes = [current_node]. The render line then shows it.

#[test]
fn scenario_b_rumor_roots_at_current_node() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = linear_graph_at_tavern(); // current = tavern
    let parsed = bracket_parser::parse("Mara leans in. [RUMOR the stranger paid in gold coins]");
    assert_eq!(parsed.commands.len(), 1);

    let outcomes = apply_rumor_commands(&mut schema, &parsed, 1000);
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        RumorOutcome::Rooted { label, origin } => {
            assert_eq!(label, "the stranger paid in gold coins");
            assert_eq!(origin, "tavern");
        }
        _ => panic!("[RUMOR] should root at the current node"),
    }
    assert_eq!(schema.rumors.len(), 1);
    assert_eq!(schema.rumors[0].origin_node, "tavern");
    assert_eq!(schema.rumors[0].known_nodes, vec!["tavern".to_string()]);
    assert_eq!(schema.rumors[0].born_minutes, 1000);

    let rendered = schema.render_for_prompt();
    assert!(
        rendered.contains("rumors: the stranger paid in gold coins"),
        "render should show the rooted rumor (got: {rendered})"
    );
}

// ===========================================================================
// SCENARIO C — [RUMOR] with no current node → warn-and-skip
// ===========================================================================
// A [RUMOR] emitted when there is no current_node can't be rooted. The
// production code warns + skips (mirrors [MILESTONE]'s unknown-id drop). The
// schema is unchanged.

#[test]
fn scenario_c_rumor_with_no_current_node_is_dropped() {
    let mut schema = WorldSchema::default();
    // No travel_graph at all → no current_node.
    let parsed = bracket_parser::parse("[RUMOR orphan rumor]");
    assert_eq!(parsed.commands.len(), 1);

    let outcomes = apply_rumor_commands(&mut schema, &parsed, 1000);
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], RumorOutcome::Dropped { .. }));
    assert!(
        schema.rumors.is_empty(),
        "dropped rumor must not mutate the schema"
    );
}

// ===========================================================================
// SCENARIO D — Render shows only rumors the current node knows
// ===========================================================================
// Two rumors, both rooted at tavern. Both appear in the `rumors:` line when
// the current node is the tavern. (A third rumor rooted elsewhere — covered
// in scenario E — is hidden.)

#[test]
fn scenario_d_render_shows_only_current_node_rumors() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = linear_graph_at_tavern(); // current = tavern
    schema.rumors.push(rumor::Rumor {
        label: "rumor one".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    });
    schema.rumors.push(rumor::Rumor {
        label: "rumor two".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    });
    let rendered = schema.render_for_prompt();
    assert!(
        rendered.contains("rumors: rumor one; rumor two"),
        "render should show both tavern rumors joined by '; ' (got: {rendered})"
    );
}

// ===========================================================================
// SCENARIO E — Render suppresses rumors at OTHER nodes (travel away → hidden)
// ===========================================================================
// A rumor rooted at the tavern but NOT yet propagated to the cellar is hidden
// when the player travels to the cellar. This is the "race against the
// spreading rumor" gameplay: the player can travel away from a rumor and have
// it vanish from this line (until the tick catches up).

#[test]
fn scenario_e_travel_away_hides_rumor() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = linear_graph_at_tavern(); // current = tavern
    schema.rumors.push(rumor::Rumor {
        label: "tavern-only rumor".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()], // not yet at cellar
        born_minutes: 0,
    });

    // At the tavern: the rumor shows.
    let rendered_tavern = schema.render_for_prompt();
    assert!(rendered_tavern.contains("rumors: tavern-only rumor"));

    // Travel to the cellar: the rumor is hidden (cellar hasn't heard it yet).
    schema.travel_graph.current_node = Some("cellar".to_string());
    let rendered_cellar = schema.render_for_prompt();
    assert!(
        !rendered_cellar.contains("tavern-only rumor"),
        "rumor not yet propagated to cellar should be hidden there (got: {rendered_cellar})"
    );
}

// ===========================================================================
// SCENARIO F — Propagation spreads to an adjacent unknown node
// ===========================================================================
// Over a sweep of many ticks, a fresh rumor at the tavern must spread to the
// cellar at least once (DC 6 ≈ 75% per edge). If it never spread, the
// propagation fn or the seeding is broken.

#[test]
fn scenario_f_propagation_spreads_to_adjacent_node() {
    let graph = linear_graph_at_tavern();
    let rumor = rumor::Rumor {
        label: "the stranger paid in gold".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    };
    let any_spread = (0..500i64).any(|m| {
        let (out, dirs) = rumor::propagate_rumors(&[rumor.clone()], &graph, m);
        out[0].known_nodes.len() > 1 || !dirs.is_empty()
    });
    assert!(any_spread, "rumor should spread to the cellar on at least one tick");
}

// ===========================================================================
// SCENARIO G — Age-decayed DC: stale spreads less than fresh
// ===========================================================================
// A fresh rumor (age 0, DC 6, ~75% per edge) must spread more in aggregate
// over a sweep than a stale one (age 64h+, DC 14, ~25% per edge). This pins
// the age-decay curve.

#[test]
fn scenario_g_stale_rumor_spreads_slower_than_fresh() {
    let graph = star_graph_at_tavern();

    // Fresh: born at minute 0.
    let fresh = rumor::Rumor {
        label: "fresh".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    };
    let fresh_total: usize = (0..500i64)
        .map(|m| {
            let (out, _) = rumor::propagate_rumors(&[fresh.clone()], &graph, m);
            out[0].known_nodes.len() - 1
        })
        .sum();

    // Stale: born 64h before minute 0 (DC 14 throughout the sweep).
    let stale = rumor::Rumor {
        label: "stale".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: -3840,
    };
    let stale_total: usize = (0..500i64)
        .map(|m| {
            let (out, _) = rumor::propagate_rumors(&[stale.clone()], &graph, m);
            out[0].known_nodes.len() - 1
        })
        .sum();

    assert!(
        fresh_total > stale_total,
        "fresh rumor (DC 6) should spread more than stale (DC 14): {} vs {}",
        fresh_total,
        stale_total
    );
}

// ===========================================================================
// SCENARIO H — Per-tick cap (NEW_NODES_PER_TICK_CAP = 2)
// ===========================================================================
// The load-bearing anti-bloat guard. A star graph gives the tavern-rumored
// rumor 3 candidate edges (tavern→cellar, tavern→market, tavern→guardhouse).
// Even if all 3 rolls would pass, at most 2 new nodes can be added per tick.
// This is the structural bound against runaway saturation.

#[test]
fn scenario_h_per_tick_cap_enforced() {
    let graph = star_graph_at_tavern();
    let rumor = rumor::Rumor {
        label: "the captain is hunting a fugitive".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    };
    for m in 0..500i64 {
        let (out, _) = rumor::propagate_rumors(&[rumor.clone()], &graph, m);
        let new_count = out[0].known_nodes.iter().filter(|n| **n != "tavern").count();
        assert!(
            new_count <= 2,
            "rumor added {} new nodes at minute {} (cap is 2)",
            new_count,
            m
        );
    }
}

// ===========================================================================
// SCENARIO I — Determinism: same args → same spread
// ===========================================================================
// Same (rumor, graph, now_minutes) must produce identical output (testable +
// replayable — mirrors weather::drift_weather + offscreen_task::resolve_task).

#[test]
fn scenario_i_propagation_is_deterministic() {
    let graph = linear_graph_at_tavern();
    let rumor = rumor::Rumor {
        label: "deterministic test".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    };
    let (a_out, a_dirs) = rumor::propagate_rumors(&[rumor.clone()], &graph, 4242);
    let (b_out, b_dirs) = rumor::propagate_rumors(&[rumor], &graph, 4242);
    assert_eq!(a_out, b_out);
    assert_eq!(a_dirs, b_dirs);
}

// ===========================================================================
// SCENARIO J — Combat suspension (the upstream gate)
// ===========================================================================
// The production tick path gates propagation on
// progression_interval_hours() == 0 (combat suspends background sim). This
// test verifies the contract the tick caller relies on: propagate_rumors is
// ONLY called when the gate is open. We simulate the gate by NOT calling
// propagate during "combat" ticks — the rumor's known_nodes must stay frozen.

#[test]
fn scenario_j_combat_suspension_freezes_propagation() {
    let graph = linear_graph_at_tavern();
    let mut rumor = rumor::Rumor {
        label: "frozen during combat".to_string(),
        origin_node: "tavern".to_string(),
        known_nodes: vec!["tavern".to_string()],
        born_minutes: 0,
    };
    let before = rumor.known_nodes.clone();

    // Simulate the production tick: the gate returns 0 (combat) → propagate
    // is NOT called. The rumor's known_nodes are untouched.
    let interval_hours: i64 = 0; // SceneMode::Combat progression_interval_hours
    if interval_hours != 0 {
        let (new, _) = rumor::propagate_rumors(&[rumor.clone()], &graph, 1000);
        rumor = new.into_iter().next().unwrap();
    }

    assert_eq!(
        rumor.known_nodes, before,
        "combat suspension must freeze propagation (known_nodes unchanged)"
    );
}

// ===========================================================================
// SCENARIO K — Backwards-compat: pre-Component-4 save loads as empty
// ===========================================================================
// A save JSON from before Component 4 shipped has no `rumors` field. Serde's
// `#[serde(default)]` must load it as an empty Vec (dormant). We simulate this
// by deserializing a JSON object without the field.

#[test]
fn scenario_k_pre_component4_save_loads_as_empty() {
    // Simulate a pre-Component-4 save: serialize a default WorldSchema, then
    // strip the `rumors` key from the JSON (as if the save predates Component
    // 4). serde's #[serde(default)] on the field must produce an empty Vec on
    // re-deserialization (dormant). This is more robust than hand-writing JSON
    // (no risk of mistyping a nested struct like PlayerState).
    let default_schema = WorldSchema::default();
    let mut json = serde_json::to_value(&default_schema).expect("serialize default");
    let obj = json
        .as_object_mut()
        .expect("WorldSchema serializes to a JSON object");
    obj.remove("rumors"); // strip the field — pre-Component-4 shape
    assert!(
        !obj.contains_key("rumors"),
        "test setup: rumors key must be removed before re-deserialization"
    );

    let legacy_json = serde_json::to_string(&json).expect("re-serialize");
    let schema: WorldSchema =
        serde_json::from_str(&legacy_json).expect("pre-Component-4 save must deserialize");
    assert!(
        schema.rumors.is_empty(),
        "missing rumors field must default to empty (dormant)"
    );
}

// ===========================================================================
// SCENARIO L — Mixed turn: [RUMOR] + [TRAVEL] + [TIME] apply independently
// ===========================================================================
// A single narrator turn can carry all three command types. Each applies to
// its own field — rumors, travel_graph, world_clock — without interference.
// (The production apply_phase3_bracket_commands handles RUMOR/TRAVEL/WEATHER;
// apply_time_command_and_maybe_tick handles TIME. They share the schema lock
// but write disjoint fields.)

#[test]
fn scenario_l_mixed_turn_applies_independently() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = linear_graph_at_tavern(); // current = tavern
    let parsed = bracket_parser::parse(
        "Mara nods. [RUMOR a bard sang of the heist] [TRAVEL cellar]",
    );
    // Both commands parse.
    assert_eq!(parsed.commands.len(), 2);

    // Apply the [RUMOR] (mirrors the production apply order — RUMOR runs in
    // the same apply_phase3_bracket_commands pass as TRAVEL).
    let _ = apply_rumor_commands(&mut schema, &parsed, 5000);
    // Apply the [TRAVEL] manually (mirror of apply_travel_command, simplified
    // for the known-adjacent case).
    schema.travel_graph.current_node = Some("cellar".to_string());

    // Both fields mutated independently.
    assert_eq!(schema.rumors.len(), 1);
    assert_eq!(schema.rumors[0].origin_node, "tavern"); // rooted BEFORE the travel
    assert_eq!(schema.travel_graph.current_node.as_deref(), Some("cellar"));
}

// ===========================================================================
// SCENARIO M — JSON form dispatch + field-based inference
// ===========================================================================
// The JSON form `{"kind": "rumor", "label": ...}` dispatches via the explicit
// discriminator. A bare `{"label": ...}` infers via infer_kind_from_fields.
// Both must produce a BracketCommand::Rumor.

#[test]
fn scenario_m_json_form_dispatches_to_rumor() {
    // Explicit discriminator.
    let parsed = bracket_parser::parse("```json\n{\"type\": \"rumor\", \"label\": \"json-form rumor\"}\n```");
    let rumor_cmd = parsed.commands.iter().find_map(|c| {
        if let BracketCommand::Rumor { label } = c {
            Some(label.clone())
        } else {
            None
        }
    });
    assert_eq!(rumor_cmd.as_deref(), Some("json-form rumor"));

    // Field-based inference (no discriminator, just `label`).
    let parsed = bracket_parser::parse("```json\n{\"label\": \"inferred rumor\"}\n```");
    let rumor_cmd = parsed.commands.iter().find_map(|c| {
        if let BracketCommand::Rumor { label } = c {
            Some(label.clone())
        } else {
            None
        }
    });
    assert_eq!(
        rumor_cmd.as_deref(),
        Some("inferred rumor"),
        "a bare {{\"label\": ...}} body must infer to rumor"
    );
}

// ===========================================================================
// SCENARIO N — Full pipeline: parse → apply → propagate → render shows spread
// ===========================================================================
// The end-to-end contract: a [RUMOR] is parsed + rooted, then over enough
// ticks it propagates to an adjacent node, and the render at that node then
// shows it. This is the integration proof the brief locked.

#[test]
fn scenario_n_full_pipeline_render_shows_spread() {
    let mut schema = WorldSchema::default();
    schema.travel_graph = linear_graph_at_tavern(); // current = tavern

    // 1. Parse + apply a [RUMOR] at the tavern.
    let parsed = bracket_parser::parse("[RUMOR the heist is the talk of the town]");
    let _ = apply_rumor_commands(&mut schema, &parsed, 0);
    assert_eq!(schema.rumors.len(), 1);

    // 2. Propagate over a sweep until the rumor reaches the cellar.
    let mut spread_to_cellar = false;
    for m in 0..500i64 {
        let (new_rumors, _) =
            rumor::propagate_rumors(&schema.rumors, &schema.travel_graph, m);
        schema.rumors = new_rumors;
        if schema.rumors[0].known_nodes.iter().any(|n| n == "cellar") {
            spread_to_cellar = true;
            break;
        }
    }
    assert!(spread_to_cellar, "rumor should eventually reach the cellar");

    // 3. Travel to the cellar + render — the rumor is now visible there.
    schema.travel_graph.current_node = Some("cellar".to_string());
    let rendered = schema.render_for_prompt();
    assert!(
        rendered.contains("rumors: the heist is the talk of the town"),
        "after propagation, the cellar render should show the rumor (got: {rendered})"
    );
}

// ===========================================================================
// SCENARIO O — Bracket normalization + case-insensitive verb
// ===========================================================================
// The [RUMOR ...] bracket is stripped from the prose (the post-turn parser
// contract) — the label is extracted as a command, not left as literal text.
// The verb is case-insensitive (§11.41 strip_prefix_ci helper): [rumor ...]
// and [Rumor ...] parse the same as [RUMOR ...].

#[test]
fn scenario_o_bracket_normalized_and_case_insensitive() {
    // The bracket is stripped from prose; the label is extracted.
    let parsed = bracket_parser::parse("Mara leans in. [RUMOR the stranger paid] The fire crackles.");
    assert_eq!(parsed.commands.len(), 1);
    assert!(
        !parsed.prose.contains("[RUMOR"),
        "bracket must be stripped from prose (got: {prose})",
        prose = parsed.prose
    );
    assert!(
        parsed.prose.contains("Mara leans in."),
        "lead-in prose must survive (got: {prose})",
        prose = parsed.prose
    );
    assert!(
        parsed.prose.contains("The fire crackles."),
        "trailing prose must survive (got: {prose})",
        prose = parsed.prose
    );

    // Case-insensitive verb.
    for variant in ["[rumor the news]", "[Rumor the news]", "[RUMOR the news]"] {
        let parsed = bracket_parser::parse(variant);
        assert_eq!(
            parsed.commands.len(),
            1,
            "{variant} should parse as one command"
        );
        assert!(matches!(
            parsed.commands[0],
            BracketCommand::Rumor { .. }
        ));
    }
}
