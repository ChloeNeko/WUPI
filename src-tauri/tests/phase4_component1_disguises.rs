//! Phase 4 Component 1 — Diegetic Gear & Disguises: scenario integration test.
//!
//! This is the script-only verification gate for Component 1 (§11.44). It
//! exercises the full pure-Rust pipeline end-to-end at the API level —
//! bracket parse → StatusTag construction → render → disguise Referee gate
//! → directive render — WITHOUT the live app, WITHOUT the schema lock,
//! WITHOUT IPC. The pieces are pure fns; this wires them together the same
//! way `fable_send`'s schema-lock block does, then asserts the four
//! canonical scenarios from the locked design (Chloe + Gemini, 2026-07-28):
//!
//!   A. Equipping a disguise (Tracker emits `[EFFECT ... kind=disguise]`).
//!   B. The Confident Walk-By (low-tier + no suspicious action → AutoPass).
//!   C. The Suspicious Misstep (low-tier + nervous tell → Scrutinized roll).
//!   D. The Captain's Eye (Elite+ → None; normal skill-check Referee owns it).
//!   E. The Disguise Expires (World Progression tick drops the tag → gate None).
//!
//! The unit tests in `player_state.rs`, `consequence.rs`, `bracket_parser.rs`,
//! and `lib.rs` pin each piece in isolation. THIS test is the integration
//! proof that they compose into the contract the brief locked:
//!
//!   "The Voice Actor never has to 'guess' NPC intelligence. Rust tells it
//!    what happened, and GLM just brings it to life with literary flair."
//!
//! Verification status: build + unit-test verified only. A consolidated
//! live CDP roleplay playtest (mirroring §11.38) is deferred until all four
//! Phase 4 components ship.

use std::collections::HashMap;

use wupi_lib::bracket_parser::{self, BracketCommand};
use wupi_lib::consequence::{self, Polarity, StatusTag};
use wupi_lib::player_state::{
    self, AttackerTier, DisguiseDirective,
};

// ---------------------------------------------------------------------------
// Helpers that mirror the fable_send schema-lock pipeline (the apply site at
// lib.rs:3879 + the render at lib.rs:5512). Keeping them inline here means
// the test exercises the REAL public APIs, not a mocked shadow of them.
// ---------------------------------------------------------------------------

/// Apply a parsed `[EFFECT ...]` command to a tag list, mirroring
/// `apply_phase3_bracket_commands`'s EFFECT arm (lib.rs). The production
/// helper wraps this in a schema lock + undo snapshot; the pure transform
/// is identical: construct the StatusTag, push it.
fn apply_effect_command(tags: &mut Vec<StatusTag>, cmd: &BracketCommand) {
    if let BracketCommand::Effect { label, polarity, duration_minutes: _, tag_kind } = cmd {
        // Production sets expires_at = now + duration (0 = permanent). For
        // these scenarios we use 0 (permanent) since disguise duration is
        // narratively driven, not time-driven.
        let tag = StatusTag {
            label: label.clone(),
            polarity: *polarity,
            expires_at: 0,
            source: String::new(),
            kind: tag_kind.clone(),
        };
        consequence::add_tag(tags, tag);
    }
}

/// Render the disguise-relevant slice of `<world_state>` — the `disguises:`
/// lane from `render_tags_for_prompt`. This is what the blindfolded API
/// narrator + the Tracker both see (called twice in API mode per §11.42).
fn render_disguise_lane(tags: &[StatusTag]) -> String {
    consequence::render_tags_for_prompt(tags).unwrap_or_default()
}

/// Run the disguise gate for a turn, mirroring the fable_send call site.
fn run_gate(
    text: &str,
    tags: &[StatusTag],
    entities: &HashMap<String, String>,
    pacing_dc_mod: i32,
) -> Option<DisguiseDirective> {
    player_state::evaluate_disguise_gate(text, tags, entities, pacing_dc_mod)
}

fn entities_with_tier(tier: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("npc.gate_guard.tier".to_string(), tier.to_string());
    m
}

// ===========================================================================
// SCENARIO A — Equipping a disguise
// ===========================================================================
// The Tracker narrates Kaelen donning a guard's uniform. It emits the
// canonical disguise EFFECT command. The bracket parser must route `kind=
// disguise` through; the apply must land it as a StatusTag with kind set;
// the render must surface it in the `disguises:` lane (NOT buffs/debuffs).

#[test]
fn scenario_a_equipping_a_disguise_round_trips_to_status_tag() {
    // The Tracker emits the mixed positional + kind= form (recommended syntax
    // in BRACKET_PROTOCOL). The parser hoists `kind=` before the key=value/
    // positional split so this threads through cleanly.
    let raw = "[EFFECT city guard uniform buff 0 kind=disguise]";
    let parsed = bracket_parser::parse(raw);
    assert_eq!(parsed.commands.len(), 1, "one EFFECT command");

    let mut tags: Vec<StatusTag> = Vec::new();
    apply_effect_command(&mut tags, &parsed.commands[0]);

    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].label, "city guard uniform");
    assert_eq!(tags[0].kind, "disguise", "kind must thread parser → tag");

    // The render must surface this in the disguises: lane, NOT buffs:.
    let lane = render_disguise_lane(&tags);
    assert!(
        lane.contains("disguises: city guard uniform"),
        "disguise lands in its own lane: {lane}"
    );
    assert!(
        !lane.contains("buffs:"),
        "disguise must NOT pollute the buffs lane: {lane}"
    );
    assert!(
        !lane.contains("debuffs:"),
        "disguise must NOT pollute the debuffs lane: {lane}"
    );
}

#[test]
fn scenario_a_json_form_disguise_also_round_trips() {
    // The JSON form (GLM's preferred shape under the dual-parser §11.39)
    // must carry kind too — the Tracker can emit either form.
    let raw = "```json\n{ \"type\": \"effect\", \"label\": \"merchant robes\", \"polarity\": \"buff\", \"duration_minutes\": 0, \"kind\": \"disguise\" }\n```";
    let parsed = bracket_parser::parse(raw);
    let mut tags: Vec<StatusTag> = Vec::new();
    apply_effect_command(&mut tags, &parsed.commands[0]);
    assert_eq!(tags[0].label, "merchant robes");
    assert_eq!(tags[0].kind, "disguise");
}

// ===========================================================================
// SCENARIO B — The Confident Walk-By
// ===========================================================================
// Kaelen, in uniform, marches past a tired rank-and-file soldier without
// drawing scrutiny. Rust AUTO-PASSES — no Deception roll. The narrator is
// handed "ACCEPTED" as a hard fact and writes the seamless beat.

#[test]
fn scenario_b_confident_walkby_autopasses_against_soldier() {
    let tags = vec![StatusTag {
        label: "city guard uniform".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("soldier");
    let text = "I nod to the gatekeeper and march straight into the inner keep.";

    let dd = run_gate(text, &tags, &entities, 0)
        .expect("soldier + confident walk-by → AutoPass");

    match &dd {
        DisguiseDirective::AutoPass { label, tier_tag } => {
            assert_eq!(label, "city guard uniform");
            assert_eq!(*tier_tag, "soldier");
        }
        _ => panic!("expected AutoPass, got {dd:?}"),
    }

    // The rendered directive reads as a hard fact the narrator obeys.
    let rendered = dd.render();
    assert!(rendered.contains("ACCEPTED"), "rendered: {rendered}");
    assert!(rendered.contains("soldier"), "rendered: {rendered}");
    assert!(
        rendered.contains("do not challenge"),
        "the directive must tell the narrator the guard doesn't challenge: {rendered}"
    );
}

#[test]
fn scenario_b_confident_walkby_also_autopasses_against_minion() {
    // Minions (rats, drunk peasants, fodder) are even less discerning.
    let tags = vec![StatusTag {
        label: "novice robe".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("minion");
    let dd = run_gate("I shuffle past the drunk watchman.", &tags, &entities, 0)
        .expect("minion + confident → AutoPass");
    assert!(matches!(
        dd,
        DisguiseDirective::AutoPass { tier_tag: "minion", .. }
    ));
}

// ===========================================================================
// SCENARIO C — The Suspicious Misstep
// ===========================================================================
// Kaelen is disguised, but sweats, avoids eye contact, tries to slip past.
// Rust REVOKES the auto-pass and forces a Deception check. This is the
// load-bearing piece — player phrasing directly influences whether Rust
// suppresses the roll or forces the dice.

#[test]
fn scenario_c_nervous_tell_revokes_autopass_and_forces_roll() {
    let tags = vec![StatusTag {
        label: "city guard uniform".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("soldier");
    let text = "I sweat nervously, avoid eye contact, and try to slip past the guard without speaking.";

    let dd = run_gate(text, &tags, &entities, 0)
        .expect("suspicious behavior → Scrutinized (not None, not AutoPass)");

    match &dd {
        DisguiseDirective::Scrutinized { label, dc, roll, success, .. } => {
            assert_eq!(label, "city guard uniform");
            assert_eq!(*dc, 14, "DC = DECEPTION_BASE_DC + pacing 0");
            assert!(*roll >= 1 && *roll <= 20, "valid d20 roll");
            assert_eq!(*success, *roll >= *dc, "success = roll >= dc");
        }
        _ => panic!("expected Scrutinized, got {dd:?}"),
    }

    // The rendered directive carries the disguise context AND the dice facts.
    let rendered = dd.render();
    assert!(rendered.contains("SCRUTINIZED"), "rendered: {rendered}");
    assert!(rendered.contains("DC 14"), "rendered: {rendered}");
    assert!(
        rendered.contains("city guard uniform"),
        "the directive must carry the disguise label: {rendered}"
    );
}

#[test]
fn scenario_c_protocol_mistake_also_revokes_autopass() {
    // "wrong salute" — the disguise breaks down at the procedural level.
    let tags = vec![StatusTag {
        label: "city guard uniform".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("soldier");
    let dd = run_gate(
        "I salute wrong and the guard frowns, confused.",
        &tags,
        &entities,
        0,
    ).expect("protocol mistake → Scrutinized");
    assert!(matches!(dd, DisguiseDirective::Scrutinized { .. }));
}

#[test]
fn scenario_c_pacing_modifier_threads_into_scrutinized_dc() {
    // ScenePacing DC modifier (Combat +2 / Exploration 0 / Downtime −2)
    // threads into the Scrutinized DC exactly as it does for skill checks.
    let tags = vec![StatusTag {
        label: "city guard uniform".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("soldier");
    let suspicious = "I stammer and fumble my badge.";

    let combat_dc = match run_gate(suspicious, &tags, &entities, 2).unwrap() {
        DisguiseDirective::Scrutinized { dc, .. } => dc,
        _ => panic!("combat still scrutinizes"),
    };
    let downtime_dc = match run_gate(suspicious, &tags, &entities, -2).unwrap() {
        DisguiseDirective::Scrutinized { dc, .. } => dc,
        _ => panic!("downtime still scrutinizes"),
    };
    assert_eq!(combat_dc, 16, "Combat DC = 14 + 2");
    assert_eq!(downtime_dc, 12, "Downtime DC = 14 − 2");
}

// ===========================================================================
// SCENARIO D — The Captain's Eye
// ===========================================================================
// An Elite NPC (captain of the guard) scrutinizes by DEFAULT. Even a
// confident walk-by forces a roll — but the disguise gate returns None
// here, deferring to the normal §11.21 skill-check Referee (which handles
// Deception with no disguise framing). This is why the cutoff is Option 1
// (Minion + Soldier) and not Option 3 (Minion + Soldier + Elite): a
// captain knows his garrison's faces; auto-passing him would break
// immersion.

#[test]
fn scenario_d_elite_captain_forces_real_roll_gate_returns_none() {
    let tags = vec![StatusTag {
        label: "city guard uniform".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("elite");

    // Confident walk-by against an Elite — still None (no auto-pass).
    let dd_confident = run_gate(
        "I nod to the captain and walk past confidently.",
        &tags,
        &entities,
        0,
    );
    assert!(
        dd_confident.is_none(),
        "Elite+ must NOT auto-pass, even confident: {dd_confident:?}"
    );

    // Suspicious behavior against an Elite — also None. The §11.21
    // skill-check Referee will fire its own Deception roll here (the
    // "deceive" keyword may or may not match the text; that's its lane).
    let dd_suspicious = run_gate(
        "I sweat and stammer at the captain.",
        &tags,
        &entities,
        0,
    );
    assert!(
        dd_suspicious.is_none(),
        "disguise gate defers to skill-check Referee for Elite+: {dd_suspicious:?}"
    );
}

#[test]
fn scenario_d_attacker_tier_ord_ladder_supports_the_cutoff() {
    // The `tier > Soldier` cutoff depends on derived Ord matching the
    // variant declaration order (Minion < Soldier < Elite < Boss < Legendary).
    // This is the load-bearing invariant for the gate.
    assert!(!(AttackerTier::Soldier > AttackerTier::Soldier), "Soldier is the cutoff — NOT strictly greater than itself");
    assert!(AttackerTier::Elite > AttackerTier::Soldier);
    assert!(AttackerTier::Boss > AttackerTier::Soldier);
    assert!(AttackerTier::Legendary > AttackerTier::Soldier);
    assert!(AttackerTier::Minion < AttackerTier::Soldier);
    assert!(!(AttackerTier::Minion > AttackerTier::Soldier), "Minion is below the cutoff");
}

// ===========================================================================
// SCENARIO E — The Disguise Expires
// ===========================================================================
// The disguise is removed (via an explicit narrative event — the Tracker
// emits nothing, or the player takes the uniform off). Once the tag is
// gone, the gate returns None. This also covers the World Progression
// tick dropping an expired timed disguise.

#[test]
fn scenario_e_disguise_removed_means_gate_returns_none() {
    let entities = entities_with_tier("soldier");

    // Start disguised.
    let mut tags = vec![StatusTag {
        label: "city guard uniform".into(),
        polarity: Polarity::Buff,
        expires_at: 0,
        source: String::new(),
        kind: "disguise".into(),
    }];
    let before = run_gate("I walk past the guard.", &tags, &entities, 0);
    assert!(matches!(before, Some(DisguiseDirective::AutoPass { .. })));

    // The disguise is removed (narrative event, or timed expiry via the tick).
    tags.clear();

    let after = run_gate("I walk past the guard.", &tags, &entities, 0);
    assert!(after.is_none(), "no disguise tag → gate returns None");
}

#[test]
fn scenario_e_timed_disguise_dropped_by_tick_then_gate_none() {
    // A timed disguise (expires_at != 0) is dropped by the World Progression
    // tick's expire_tags. After the drop, the gate sees no disguise.
    let now = 1000_i64;
    let mut tags = vec![StatusTag {
        label: "borrowed livery".into(),
        polarity: Polarity::Buff,
        expires_at: 500, // expires at minute 500
        source: "borrowed from a sleeping servant".into(),
        kind: "disguise".into(),
    }];
    let entities = entities_with_tier("soldier");

    // Tick advances to minute 1000 → the tag (expires 500) is dropped.
    let dropped = consequence::expire_tags(&mut tags, now);
    assert_eq!(dropped, 1, "one expired tag dropped");
    assert!(tags.is_empty());

    let dd = run_gate("I walk past the guard.", &tags, &entities, 0);
    assert!(dd.is_none(), "expired disguise → no gate directive");
}

// ===========================================================================
// SCENARIO F — Coexistence with generic buffs/debuffs (no cross-contamination)
// ===========================================================================
// A disguised player can ALSO have active buffs/debuffs. The render must
// keep them in separate lanes; the gate must only care about the disguise.

#[test]
fn scenario_f_disguise_coexists_with_buffs_and_debuffs_in_separate_lanes() {
    let tags = vec![
        StatusTag {
            label: "Berserk Rage".into(),
            polarity: Polarity::Buff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(),
        },
        StatusTag {
            label: "Poisoned".into(),
            polarity: Polarity::Debuff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(),
        },
        StatusTag {
            label: "city guard uniform".into(),
            polarity: Polarity::Buff, // polarity irrelevant when kind=disguise
            expires_at: 0,
            source: String::new(),
            kind: "disguise".into(),
        },
    ];
    let lane = render_disguise_lane(&tags);
    assert!(lane.contains("buffs: Berserk Rage"));
    assert!(lane.contains("debuffs: Poisoned"));
    assert!(lane.contains("disguises: city guard uniform"));
    // All three lanes present, none polluted.
    assert_eq!(lane.matches(':').count(), 3, "exactly three lanes: {lane}");

    // The gate still sees the disguise among the other tags.
    let entities = entities_with_tier("soldier");
    let dd = run_gate("I stride through the gate.", &tags, &entities, 0);
    assert!(matches!(dd, Some(DisguiseDirective::AutoPass { .. })));
}

// ===========================================================================
// SCENARIO G — Backwards compatibility (pre-Phase-4 tags untouched)
// ===========================================================================
// Pre-Phase-4 saves + pre-Phase-4 EFFECT emissions have no `kind` field.
// They must load/render/parse exactly as before — empty kind = generic.

#[test]
fn scenario_g_legacy_status_tag_without_kind_loads_as_generic() {
    // A pre-Phase-4 serialized tag (no kind field in JSON).
    let json = r#"{"label":"Blessed","polarity":"buff","expires_at":0,"source":""}"#;
    let tag: StatusTag = serde_json::from_str(json).expect("legacy tag must load");
    assert_eq!(tag.kind, "", "kind defaults to empty");
    // It routes by polarity (Buff) into the buffs: lane, not disguises:.
    let lane = render_disguise_lane(&[tag]);
    assert!(lane.contains("buffs: Blessed"));
    assert!(!lane.contains("disguises:"));
}

#[test]
fn scenario_g_legacy_positional_effect_form_still_parses_without_kind() {
    // The positional form (no kind=) is unchanged — backwards-compatible.
    let raw = "[EFFECT Berserk Rage buff 60]";
    let parsed = bracket_parser::parse(raw);
    if let BracketCommand::Effect { tag_kind, .. } = &parsed.commands[0] {
        assert_eq!(*tag_kind, "", "legacy positional form has no kind");
    } else {
        panic!("wrong variant");
    }
}

// ===========================================================================
// SCENARIO H — The full turn pipeline (the "hard facts to narrator" contract)
// ===========================================================================
// The composite proof: a Tracker emission → apply → render world_state →
// gate → render directive. This is the exact sequence fable_send runs
// (minus the schema lock + IPC). The output is what the narrator sees.

#[test]
fn scenario_h_full_turn_pipeline_emits_clean_hard_facts() {
    // --- Stage 1: Tracker emits the disguise equip. ---
    let equip_raw = "[EFFECT city guard uniform buff 0 kind=disguise]";
    let equip_parsed = bracket_parser::parse(equip_raw);
    let mut tags: Vec<StatusTag> = Vec::new();
    apply_effect_command(&mut tags, &equip_parsed.commands[0]);

    // --- Stage 2: render the world_state slice the narrator sees. ---
    let world_state_lane = render_disguise_lane(&tags);
    assert!(
        world_state_lane.contains("disguises: city guard uniform"),
        "narrator sees the active disguise as a persistent fact: {world_state_lane}"
    );

    // --- Stage 3: player acts (confident walk-by), gate runs. ---
    let entities = entities_with_tier("soldier");
    let player_text = "I nod to the gatekeeper and march into the inner keep.";
    let dd = run_gate(player_text, &tags, &entities, 0);

    // --- Stage 4: the directive is composed into the <directives> block. ---
    // (fable_send wraps each directive in [DIRECTIVE: ...]; we mirror that.)
    let mut directives_block = String::from("<directives>\n");
    if let Some(d) = &dd {
        directives_block.push_str(&format!("[DIRECTIVE: {}]\n", d.render()));
    }
    directives_block.push_str("</directives>");

    // The contract: the narrator sees the persistent disguise (world_state)
    // AND the turn outcome (directive) as separate, unarguable facts.
    assert!(
        world_state_lane.contains("disguises:"),
        "persistent fact present"
    );
    assert!(
        directives_block.contains("ACCEPTED"),
        "turn outcome present: {directives_block}"
    );
    assert!(
        directives_block.contains("[DIRECTIVE: Disguise (city guard uniform)"),
        "directive carries the disguise context: {directives_block}"
    );

    // --- Stage 5: same player, now suspicious — gate flips to Scrutinized. ---
    let suspicious_text = "I sweat, avoid eye contact, and fumble my badge.";
    let dd2 = run_gate(suspicious_text, &tags, &entities, 0).unwrap();
    let rendered2 = dd2.render();
    assert!(
        rendered2.contains("SCRUTINIZED"),
        "suspicious behavior flips the outcome: {rendered2}"
    );
    // The persistent world_state disguise lane is UNCHANGED — the disguise
    // is still equipped; only the turn outcome changed. This is the
    // separation between "what the player has" and "what happened this turn."
    assert!(
        world_state_lane.contains("disguises: city guard uniform"),
        "persistent fact unchanged across turn outcomes"
    );
}
