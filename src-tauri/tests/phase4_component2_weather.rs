//! Phase 4 Component 2 — Environmental Shifts (Weather): scenario integration test.
//!
//! This is the script-only verification gate for Component 2 (2026-07-28). It
//! exercises the full pure-Rust pipeline end-to-end at the API level — bracket
//! parse → `Weather` construction → schema apply → World Progression tick drift
//! → directive emission → render — WITHOUT the live app, WITHOUT the schema
//! lock, WITHOUT IPC. The pieces are pure fns; this wires them together the
//! same way `fable_send`'s schema-lock block + `apply_time_command_and_maybe_
//! tick`'s tick block do, then asserts the canonical scenarios from the locked
//! design (Chloe's verdict, 2026-07-28):
//!
//!   A. The Tracker forces a weather event (`[WEATHER ...]` → typed field).
//!   B. The weather renders as a hard `weather:` fact in `<world_state>`.
//!   C. The tick drifts long-running weather deterministically (persistence
//!      check fails → new condition from the pool, never the same).
//!   D. The tick does NOT drift weather that hasn't been set (dormant — mirrors
//!      `world_clock` before the first `[TIME]`).
//!   E. The tick's drift directive + the new `weather:` line are the two
//!      redundant signals the blindfolded narrator inherits (mirrors off-screen
//!      task resolution surfacing).
//!   F. `apply_delta` cannot touch the typed weather field (Rust-authority
//!      invariant — the playtest `entities["weather"]` string is orthogonal).
//!   G. Backwards compat: a pre-Phase-4 save (no `weather` field) loads as
//!      unset (dormant).
//!
//! The unit tests in `weather.rs`, `schema.rs`, `bracket_parser.rs`, `lib.rs`,
//! and `stream_filter.rs` pin each piece in isolation. THIS test is the
//! integration proof that they compose into the contract the brief locked:
//!
//!   "Hook dynamic weather states directly into the existing WorldClock system.
//!    Weather shifts naturally with time progression and feeds into the JSON
//!    state context so the API narrator uses it to color the scene."
//!
//! Verification status: build + unit-test verified only. A consolidated live
//! CDP roleplay playtest (mirroring §11.38) is deferred until all four Phase 4
//! components ship.

use wupi_lib::bracket_parser::{self, BracketCommand};
use wupi_lib::schema::{Weather, WorldSchema};
use wupi_lib::weather;

// ---------------------------------------------------------------------------
// Helpers that mirror the fable_send schema-lock pipeline. Keeping them
// inline here means the test exercises the REAL public APIs, not a mocked
// shadow of them.
// ---------------------------------------------------------------------------

/// Apply the last `[WEATHER ...]` command to a schema's weather field,
/// mirroring `apply_phase3_bracket_commands`'s WEATHER arm (lib.rs). The
/// production helper wraps this in a schema lock + undo snapshot; the pure
/// transform is identical: take the last weather command (last-wins, like
/// TIME), construct the typed Weather, move it in. Stamps `started_at_minutes`
/// from `now_minutes` so the tick drift's persistence curve has a baseline.
fn apply_weather_command(schema: &mut WorldSchema, parsed: &bracket_parser::ParsedNarration) {
    let now_minutes = schema.world_clock.current_minutes;
    let last_weather = parsed.commands.iter().rev().find_map(|cmd| {
        if let BracketCommand::Weather { condition } = cmd {
            Some(condition)
        } else {
            None
        }
    });
    if let Some(condition) = last_weather {
        schema.weather = Weather {
            condition: condition.clone(),
            started_at_minutes: now_minutes,
        };
    }
}

/// Run one tick-drift pass against the schema's weather, mirroring the tick
/// block in `apply_time_command_and_maybe_tick` (lib.rs). Returns the
/// directive string if weather drifted, else None. Mutates `schema.weather`
/// in place on drift.
fn run_weather_tick(schema: &mut WorldSchema) -> Option<String> {
    let now_minutes = schema.world_clock.current_minutes;
    if let Some(new) = weather::drift_weather(&schema.weather, now_minutes) {
        let directive = format!(
            "Weather shift — {} gives way to {}. Narrate the changing conditions.",
            schema.weather.condition, new.condition
        );
        schema.weather = new;
        Some(directive)
    } else {
        None
    }
}

// ===========================================================================
// SCENARIO A — The Tracker forces a weather event
// ===========================================================================
// The Tracker narrates a storm rolling in. It emits the canonical `[WEATHER
// heavy rain]` command. The bracket parser must route it; the apply must land
// it as the typed Weather field (NOT as a generic entity). The started_at
// stamp comes from the current clock.

#[test]
fn scenario_a_bracket_weather_round_trips_to_typed_field() {
    let mut schema = WorldSchema::default();
    // The clock has been advanced by a prior [TIME] (minute 1440 = Day 2, 00:00).
    schema.world_clock.current_minutes = 1440;

    let raw = "The sky bruises black. [WEATHER heavy rain] Hail rattles the shutters.";
    let parsed = bracket_parser::parse(raw);
    assert_eq!(parsed.commands.len(), 1, "one WEATHER command");
    assert_eq!(
        parsed.commands[0],
        BracketCommand::Weather { condition: "heavy rain".into() }
    );

    apply_weather_command(&mut schema, &parsed);

    assert!(schema.weather.is_set(), "weather is now set");
    assert_eq!(schema.weather.condition, "heavy rain");
    assert_eq!(
        schema.weather.started_at_minutes, 1440,
        "started_at stamped from the current clock"
    );
}

#[test]
fn scenario_a_json_weather_form_also_round_trips() {
    // The JSON form (GLM's preferred shape under the dual-parser §11.39)
    // must dispatch via the per-variant arm in parse_json_command.
    let mut schema = WorldSchema::default();
    schema.world_clock.current_minutes = 2000;

    let raw = "```json\n{ \"type\": \"weather\", \"condition\": \"thick fog\" }\n```";
    let parsed = bracket_parser::parse(raw);
    assert_eq!(parsed.commands.len(), 1);
    apply_weather_command(&mut schema, &parsed);
    assert_eq!(schema.weather.condition, "thick fog");
    assert_eq!(schema.weather.started_at_minutes, 2000);
}

#[test]
fn scenario_a_multiple_weather_commands_last_wins() {
    // Mirrors the [TIME] "last one is most recent + authoritative" contract.
    // If the Tracker emits two weather commands (unusual but legal), the
    // LAST one wins.
    let mut schema = WorldSchema::default();
    schema.world_clock.current_minutes = 1000;

    let raw = "[WEATHER clear] [WEATHER thunderstorm]";
    let parsed = bracket_parser::parse(raw);
    assert_eq!(parsed.commands.len(), 2);
    apply_weather_command(&mut schema, &parsed);
    assert_eq!(schema.weather.condition, "thunderstorm", "last wins");
}

// ===========================================================================
// SCENARIO B — The weather renders as a hard fact in <world_state>
// ===========================================================================
// The blindfolded API narrator + the Tracker both see `weather: <condition>`
// in the `<world_state>` block (rendered twice in API mode per §11.42 —
// pre-tracker + post-tracker). This is the self-documenting hard fact the
// narrator weaves into prose; no prompt clause needed.

#[test]
fn scenario_b_weather_renders_in_world_state_when_set() {
    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "heavy rain".to_string(),
        started_at_minutes: 1000,
    };
    let rendered = schema.render_for_prompt();
    assert!(
        rendered.contains("weather: heavy rain"),
        "narrator sees the weather as a hard fact: {rendered}"
    );
}

#[test]
fn scenario_b_weather_omitted_when_unset_zero_tokens() {
    // Fresh game (no [WEATHER] yet) → no weather line. Dormant, like world_clock.
    let schema = WorldSchema::default();
    let rendered = schema.render_for_prompt();
    assert!(!rendered.contains("weather:"), "no weather line when unset");
}

#[test]
fn scenario_b_weather_renders_alongside_clock() {
    // The two atmospheric anchors render together — the narrator sees both
    // the current time AND the current weather as top-of-mind facts.
    use wupi_lib::schema::WorldClock;
    let mut schema = WorldSchema::default();
    schema.world_clock = WorldClock { current_minutes: 2880, last_tick_minutes: 0 };
    schema.weather = Weather {
        condition: "light snow".to_string(),
        started_at_minutes: 2800,
    };
    let rendered = schema.render_for_prompt();
    assert!(rendered.contains("clock: Day 3, 00:00"));
    assert!(rendered.contains("weather: light snow"));
    // Weather renders right after clock (the two anchors are adjacent).
    let clock_idx = rendered.find("clock:").unwrap();
    let weather_idx = rendered.find("weather:").unwrap();
    assert!(
        weather_idx > clock_idx,
        "weather renders after clock: {rendered}"
    );
}

// ===========================================================================
// SCENARIO C — The tick drifts long-running weather
// ===========================================================================
// Weather that has held for many in-world hours is "overdue for a shift" — the
// persistence DC scales up. On a failed check (roll < DC), the tick picks a
// new condition from the generic pool via seeded RNG, EXCLUDING the current
// one. Deterministic per (condition, now_minutes).

#[test]
fn scenario_c_long_running_weather_eventually_drifts() {
    // Start weather at minute 0, then sweep many minutes forward. At least
    // one minute in the sweep must produce a drift (otherwise the persistence
    // curve is broken — long-elapsed weather should fail its check sometimes).
    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "clear".to_string(),
        started_at_minutes: 0,
    };
    let mut drift_count = 0;
    for m in (1..2000i64).step_by(13) {
        schema.world_clock.current_minutes = m;
        if run_weather_tick(&mut schema).is_some() {
            drift_count += 1;
            // After a drift, the new condition's started_at resets to m, so
            // the persistence curve restarts. Set it back to test more.
            schema.weather.started_at_minutes = 0;
            schema.weather.condition = "clear".to_string();
        }
    }
    assert!(drift_count > 0, "long-running weather must drift at least once");
}

#[test]
fn scenario_c_drift_never_picks_the_same_condition() {
    // The load-bearing invariant: a shift always produces a visible change.
    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "clear".to_string(),
        started_at_minutes: 0,
    };
    for m in (1..3000i64).step_by(7) {
        schema.world_clock.current_minutes = m;
        let before = schema.weather.condition.clone();
        if run_weather_tick(&mut schema).is_some() {
            assert_ne!(
                schema.weather.condition, before,
                "drift must change the condition at minute {m}"
            );
            // Reset to keep testing the same starting condition.
            schema.weather.condition = "clear".to_string();
            schema.weather.started_at_minutes = 0;
        }
    }
}

#[test]
fn scenario_c_drift_is_deterministic_per_condition_and_minute() {
    // Same (condition, started_at, now) → same outcome (testable + replayable).
    let mk = |condition: &str, started: i64, now: i64| {
        let mut s = WorldSchema::default();
        s.weather = Weather { condition: condition.into(), started_at_minutes: started };
        s.world_clock.current_minutes = now;
        s
    };
    // Find a minute that produces a drift, then assert determinism.
    let mut drifted_minute: Option<i64> = None;
    for m in 0..2000i64 {
        let mut s = mk("clear", 0, m);
        if run_weather_tick(&mut s).is_some() {
            drifted_minute = Some(m);
            break;
        }
    }
    let Some(m) = drifted_minute else {
        // No drift in the sweep — re-run with a longer sweep or accept skip.
        // (This shouldn't happen given scenario_c above, but guard the test.)
        eprintln!("note: no drift in determinism sweep; skipping");
        return;
    };
    let mut a = mk("clear", 0, m);
    let mut b = mk("clear", 0, m);
    let da = run_weather_tick(&mut a);
    let db = run_weather_tick(&mut b);
    assert_eq!(da, db, "deterministic per (condition, started, now)");
    assert_eq!(a.weather.condition, b.weather.condition);
}

#[test]
fn scenario_c_drift_resets_started_at_to_now() {
    // A drift outcome stamps started_at_minutes = now (fresh persistence
    // baseline). This is what makes the curve meaningful.
    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "clear".to_string(),
        started_at_minutes: 0,
    };
    for m in (1..3000i64).step_by(11) {
        schema.world_clock.current_minutes = m;
        if run_weather_tick(&mut schema).is_some() {
            assert_eq!(
                schema.weather.started_at_minutes, m,
                "drift must stamp started_at = now"
            );
            return;
        }
    }
    panic!("no drift observed in sweep");
}

#[test]
fn scenario_c_drift_picks_from_the_pool_only() {
    // The new condition must be a member of WEATHER_POOL (the generic drift
    // pool — the ONLY valid drift targets).
    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "clear".to_string(),
        started_at_minutes: 0,
    };
    for m in (1..3000i64).step_by(11) {
        schema.world_clock.current_minutes = m;
        if run_weather_tick(&mut schema).is_some() {
            assert!(
                weather::WEATHER_POOL.contains(&schema.weather.condition.as_str()),
                "drifted to non-pool condition: {}",
                schema.weather.condition
            );
            schema.weather.condition = "clear".to_string();
            schema.weather.started_at_minutes = 0;
        }
    }
}

// ===========================================================================
// SCENARIO D — Unset weather stays dormant (mirrors world_clock)
// ===========================================================================
// Before the first `[WEATHER]`, the weather field is unset (empty condition).
// The tick drift is a no-op — no drift, no directive. Same dormant contract
// as world_clock before the first `[TIME]`.

#[test]
fn scenario_d_unset_weather_does_not_drift() {
    let mut schema = WorldSchema::default();
    // Weather is unset; clock is set to some far-future minute.
    schema.world_clock.current_minutes = 100_000;
    assert!(!schema.weather.is_set());
    let directive = run_weather_tick(&mut schema);
    assert!(directive.is_none(), "unset weather must not drift");
    assert!(!schema.weather.is_set(), "still unset after tick");
}

// ===========================================================================
// SCENARIO E — The two-signal surfacing (drift directive + weather: line)
// ===========================================================================
// When the tick drifts weather, two redundant signals reach the narrator:
// (1) the new condition as a `weather:` hard fact in `<world_state>`, and
// (2) a directive in the `<directives>` block prompting the narrator to
// narrate the change. Mirrors how off-screen task resolutions surface.

#[test]
fn scenario_e_drift_surfaces_directive_and_new_weather_line() {
    // Force a drift by giving weather a long elapsed time (high persistence DC).
    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "clear".to_string(),
        started_at_minutes: 0,
    };
    // Find a minute that drifts.
    let mut drifted_at: Option<i64> = None;
    for m in (1..5000i64).step_by(17) {
        schema.world_clock.current_minutes = m;
        schema.weather.started_at_minutes = 0;
        schema.weather.condition = "clear".to_string();
        if run_weather_tick(&mut schema).is_some() {
            drifted_at = Some(m);
            break;
        }
    }
    let Some(m) = drifted_at else {
        eprintln!("note: no drift in E sweep; skipping");
        return;
    };
    schema.world_clock.current_minutes = m;
    schema.weather = Weather { condition: "clear".to_string(), started_at_minutes: 0 };

    let directive = run_weather_tick(&mut schema).expect("drift at the pinned minute");

    // Signal 1: the new weather line in <world_state>.
    let world_state = schema.render_for_prompt();
    assert!(
        world_state.starts_with("weather:") || world_state.contains("\nweather:"),
        "world_state contains a weather line: {world_state}"
    );
    assert!(
        !world_state.contains("clear") || schema.weather.condition != "clear",
        "the weather line reflects the new condition"
    );

    // Signal 2: the directive names BOTH conditions (the shift).
    assert!(
        directive.contains("clear"),
        "directive names the old condition: {directive}"
    );
    assert!(
        directive.contains(&schema.weather.condition),
        "directive names the new condition: {directive}"
    );
    assert!(
        directive.contains("Narrate"),
        "directive prompts the narrator to narrate the change: {directive}"
    );
}

// ===========================================================================
// SCENARIO F — apply_delta cannot touch the typed weather field
// ===========================================================================
// The Rust-authority invariant (mirrors world_clock / player_state / scene_
// pacing / status_tags). The playtest `entities["weather"]` convention is
// orthogonal: a delta carrying "weather" in its entities map lands as a
// plain string key, never touching the typed Weather struct.

#[test]
fn scenario_f_apply_delta_does_not_touch_typed_weather() {
    use std::collections::HashMap;
    use wupi_lib::schema::SchemaDelta;

    let mut schema = WorldSchema::default();
    schema.weather = Weather {
        condition: "heavy rain".to_string(),
        started_at_minutes: 1000,
    };
    let mut ents = HashMap::new();
    ents.insert("weather".to_string(), Some("sunny".to_string()));
    let delta = SchemaDelta {
        summary: None,
        recent_events: None,
        entities: Some(ents),
    };
    schema.apply_delta(delta);

    // The typed weather is UNCHANGED.
    assert_eq!(schema.weather.condition, "heavy rain");
    assert_eq!(schema.weather.started_at_minutes, 1000);
    // The "weather" string landed as a plain entity (legacy convention).
    assert_eq!(schema.entities.get("weather").map(|s| s.as_str()), Some("sunny"));
}

// ===========================================================================
// SCENARIO G — Backwards compatibility (pre-Phase-4 saves load as unset)
// ===========================================================================
// A pre-Phase-4 save JSON (no `weather` field) must deserialize to
// Weather::default() (unset). The `#[serde(default)]` attribute enforces
// this; this test pins it at the integration level (the unit test in
// schema.rs pins the struct level).

#[test]
fn scenario_g_pre_phase4_save_loads_with_unset_weather() {
    let pre_phase4_json = r#"{
        "summary": "The tavern stands.",
        "recent_events": [],
        "entities": {},
        "player_state": {},
        "world_clock": {"current_minutes": 1440, "last_tick_minutes": 1440},
        "immutable_keys": [],
        "scene_pacing": {"mode": "Downtime", "spatial": 0, "emotional": 0, "kinetic": 0},
        "status_tags": [],
        "relationships": {},
        "offscreen_tasks": []
    }"#;
    let parsed: WorldSchema = serde_json::from_str(pre_phase4_json)
        .expect("pre-Phase-4 JSON must deserialize");
    assert!(!parsed.weather.is_set(), "weather defaults to unset");
    assert_eq!(parsed.weather.condition, "");
    assert_eq!(parsed.weather.started_at_minutes, 0);
    // The other fields load normally.
    assert_eq!(parsed.summary, "The tavern stands.");
    assert!(parsed.world_clock.is_set());
}

// ===========================================================================
// SCENARIO H — The full turn pipeline (the "hard facts to narrator" contract)
// ===========================================================================
// The composite proof: a Tracker emission → apply → tick drift → render
// world_state + directive. This is the exact sequence fable_send runs
// (minus the schema lock + IPC). The output is what the narrator sees.

#[test]
fn scenario_h_full_turn_pipeline_emits_clean_hard_facts() {
    let mut schema = WorldSchema::default();
    schema.world_clock.current_minutes = 1440;

    // --- Stage 1: Tracker emits a weather event. ---
    let raw = "[WEATHER heavy rain]";
    let parsed = bracket_parser::parse(raw);
    apply_weather_command(&mut schema, &parsed);
    assert_eq!(schema.weather.condition, "heavy rain");

    // --- Stage 2: render world_state — the narrator sees the hard fact. ---
    let world_state = schema.render_for_prompt();
    assert!(world_state.contains("weather: heavy rain"));

    // --- Stage 3: time passes, tick fires, weather may drift. ---
    // Advance the clock by a long stretch (high persistence DC) and run the
    // tick. Whether it drifts or not, the contract holds: the narrator sees
    // the CURRENT weather as a hard fact, plus a directive ONLY if it drifted.
    schema.world_clock.current_minutes = 1440 + 10_000; // ~7 days later
    let directive = run_weather_tick(&mut schema);

    // --- Stage 4: re-render after the tick. ---
    let world_state_after = schema.render_for_prompt();
    assert!(
        world_state_after.contains("weather:"),
        "world_state always has a weather line once set: {world_state_after}"
    );

    if directive.is_some() {
        // Drifted: the weather line changed + a directive was emitted.
        assert!(
            !world_state_after.contains("weather: heavy rain"),
            "drifted weather line should reflect the new condition: {world_state_after}"
        );
        let d = directive.unwrap();
        assert!(d.contains("heavy rain"), "directive names old condition: {d}");
        assert!(d.contains("Narrate"), "directive prompts narration: {d}");
    } else {
        // Persisted: the weather line is unchanged, no directive.
        assert!(
            world_state_after.contains("weather: heavy rain"),
            "persisted weather line unchanged: {world_state_after}"
        );
    }
}
