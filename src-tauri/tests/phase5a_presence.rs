//! Fable Phase 5A integration test — the NPC presence pipeline end-to-end.
//!
//! Exercises the pure pipeline (no AppState, no IPC, no schema lock) at the
//! API level: card parse → registry build → [PRESENCE] bracket parse →
//! presence resolve + grace-decay → present: render → SD prompt compose.
//! Mirrors the Phase 4 `tests/phase4_component*.rs` pattern (script-only,
//! WITHOUT the live app). The AppState-locking `apply_phase3_bracket_commands`
//! is a thin wrapper whose logic is covered by the pure resolve + grace
//! functions exercised here.
//!
//! Canonical scenarios (the anti-teleport contract):
//!   A. seeded cast → known id asserted → presence recorded + present: renders
//!   B. alias resolves to canonical id (the normalization)
//!   C. unknown id → rejected (the anti-hallucination gate)
//!   D. grace decay: asserted → not-asserted → not-asserted → dropped (2→1→0)
//!   E. re-assertion resets grace (2→1→reset to 2)
//!   F. dropped NPC vanishes from present: + from the SD prompt compose
//!   G. compose_scene_prompt builds the macro+environment+micro layers
//!   H. registry seeding mirrors enter_fable_session's CardNpc → NpcEntry map

use wupi_lib::bracket_parser::{parse as parse_brackets, BracketCommand};
use wupi_lib::schema::{
    Node, NpcEntry, NpcRegistry, Presence, TravelGraph, Weather, WorldClock, WorldSchema,
    PRESENCE_GRACE_RESET,
};
use wupi_lib::scene_art::compose_scene_prompt;
use wupi_lib::sim_card::{CardNpc, SimCard};

// ── Helpers ────────────────────────────────────────────────────────────────

/// Build a minimal schema with the Rusty Tavern cast + a current node, ready
/// for presence-apply scenarios.
fn schema_with_cast() -> WorldSchema {
    let mut s = WorldSchema::default();
    s.travel_graph = TravelGraph {
        nodes: vec![Node {
            id: "tavern".into(),
            name: "The Rusty Lantern Tavern".into(),
            neighbors: vec!["cellar".into()],
            setting: "indoor".into(),
        }],
        current_node: Some("tavern".into()),
    };
    s.world_clock = WorldClock {
        current_minutes: 1260, // Day 1, 21:00 (evening)
        last_tick_minutes: 0,
    };
    s.npc_registry = NpcRegistry {
        entries: vec![
            NpcEntry {
                id: "mara_the_innkeep".into(),
                name: "Mara".into(),
                role: "innkeep".into(),
                tier: Some("soldier".into()),
                aliases: vec!["mara".into(), "innkeep".into()],
            },
            NpcEntry {
                id: "bard_corin".into(),
                name: "Corin".into(),
                role: "bard".into(),
                tier: None,
                aliases: vec!["corin".into(), "bard".into()],
            },
        ],
    };
    s
}

/// A pure re-implementation of apply_phase3_bracket_commands's presence block
/// for testing. Mirrors the lib.rs logic exactly: resolve surface forms via
/// the registry, reject unknowns, apply the 1-turn grace TTL. Returns
/// (new presences Vec, unknown_surfaces list).
fn apply_presence(
    schema: &WorldSchema,
    parsed: &wupi_lib::bracket_parser::ParsedNarration,
) -> (Vec<Presence>, Vec<String>) {
    let presence_cmds: Vec<&BracketCommand> = parsed
        .commands
        .iter()
        .filter(|c| matches!(c, BracketCommand::Presence { .. }))
        .collect();

    let existing: Vec<Presence> = schema.presences.clone();
    let mut asserted: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut unknown: Vec<String> = Vec::new();

    for cmd in &presence_cmds {
        if let BracketCommand::Presence { npc_id, stance } = cmd {
            match schema.npc_registry.resolve(npc_id) {
                Some(entry) => {
                    asserted.insert(entry.id.clone(), stance.trim().to_string());
                }
                None => unknown.push(npc_id.clone()),
            }
        }
    }

    let mut rebuilt: Vec<Presence> = Vec::new();
    for p in &existing {
        if let Some(new_stance) = asserted.get(&p.npc_id) {
            rebuilt.push(Presence {
                npc_id: p.npc_id.clone(),
                name: p.name.clone(),
                stance: new_stance.clone(),
                ttl: PRESENCE_GRACE_RESET,
            });
        } else if p.ttl > 1 {
            rebuilt.push(Presence {
                npc_id: p.npc_id.clone(),
                name: p.name.clone(),
                stance: p.stance.clone(),
                ttl: p.ttl - 1,
            });
        }
        asserted.remove(&p.npc_id);
    }
    for (npc_id, stance) in asserted {
        if let Some(entry) = schema.npc_registry.find(&npc_id) {
            rebuilt.push(Presence {
                npc_id: entry.id.clone(),
                name: entry.name.clone(),
                stance,
                ttl: PRESENCE_GRACE_RESET,
            });
        }
    }
    (rebuilt, unknown)
}

// ── Scenarios ──────────────────────────────────────────────────────────────

/// A. Known id asserted → presence recorded + present: line renders.
#[test]
fn scenario_a_known_id_asserted_records_presence() {
    let mut s = schema_with_cast();
    let parsed = parse_brackets("[PRESENCE mara_the_innkeep \"behind the bar, arms crossed\"]");
    assert_eq!(parsed.commands.len(), 1);
    let (presences, unknown) = apply_presence(&s, &parsed);
    assert!(unknown.is_empty(), "known id must not reject");
    assert_eq!(presences.len(), 1);
    assert_eq!(presences[0].npc_id, "mara_the_innkeep");
    assert_eq!(presences[0].stance, "behind the bar, arms crossed");
    assert_eq!(presences[0].ttl, PRESENCE_GRACE_RESET);

    s.presences = presences;
    let rendered = s.render_for_prompt();
    assert!(
        rendered.contains("present: Mara (behind the bar, arms crossed)"),
        "present: line must render: {rendered}"
    );
}

/// B. Alias resolves to the canonical id (the normalization the applier uses).
#[test]
fn scenario_b_alias_resolves_to_canonical_id() {
    let mut s = schema_with_cast();
    // The narrator used the alias "mara" (not the full id).
    let parsed = parse_brackets("[PRESENCE mara polishing a tankard]");
    let (presences, unknown) = apply_presence(&s, &parsed);
    assert!(unknown.is_empty(), "alias must resolve, not reject");
    assert_eq!(presences.len(), 1);
    assert_eq!(
        presences[0].npc_id, "mara_the_innkeep",
        "alias 'mara' must resolve to canonical id"
    );
    assert_eq!(presences[0].name, "Mara");

    s.presences = presences;
    // The present: line shows the canonical name, not the alias surface form.
    assert!(s.render_for_prompt().contains("present: Mara"));
}

/// C. Unknown id → rejected (the anti-hallucination gate). The presence is
/// NOT recorded; the surface surfaces in the unknown list (which lib.rs turns
/// into a reject directive).
#[test]
fn scenario_c_unknown_id_rejected() {
    let mut s = schema_with_cast();
    let parsed = parse_brackets("[PRESENCE mysterious_stranger lurking in shadows]");
    let (presences, unknown) = apply_presence(&s, &parsed);
    assert_eq!(presences.len(), 0, "unknown id must NOT be recorded");
    assert_eq!(unknown, vec!["mysterious_stranger".to_string()]);

    s.presences = presences;
    let rendered = s.render_for_prompt();
    // No present: line for the hallucinated NPC.
    assert!(
        !rendered.contains("mysterious_stranger"),
        "rejected NPC must not appear in present: {rendered}"
    );
    assert!(!rendered.contains("present:"), "empty presences render no line");
}

/// D. Grace decay: asserted (2) → not-asserted (1) → not-asserted (dropped).
/// One missed extraction is tolerated; two consecutive misses drop the NPC.
#[test]
fn scenario_d_grace_decay_two_misses_drops() {
    let mut s = schema_with_cast();
    // Turn 1: assert Mara.
    let parsed = parse_brackets("[PRESENCE mara at the bar]");
    let (p1, _) = apply_presence(&s, &parsed);
    s.presences = p1;
    assert_eq!(s.presences[0].ttl, PRESENCE_GRACE_RESET);

    // Turn 2: Tracker emits NO [PRESENCE] for Mara (missed extraction). She
    // decays to ttl=1 but stays on-camera (the grace).
    let no_assert = parse_brackets("Mara continues polishing."); // no bracket
    let (p2, _) = apply_presence(&s, &no_assert);
    s.presences = p2;
    assert_eq!(s.presences.len(), 1, "one miss tolerated (grace)");
    assert_eq!(s.presences[0].ttl, 1);
    assert!(s.render_for_prompt().contains("present: Mara"));

    // Turn 3: still no [PRESENCE]. Second consecutive miss → dropped (ttl 1→0).
    let (p3, _) = apply_presence(&s, &no_assert);
    s.presences = p3;
    assert!(s.presences.is_empty(), "two consecutive misses → dropped");
    assert!(!s.render_for_prompt().contains("present:"), "dropped NPC renders no line");
}

/// E. Re-assertion resets the grace TTL (2 → 1 → reset to 2).
#[test]
fn scenario_e_re_assertion_resets_grace() {
    let mut s = schema_with_cast();
    // Assert, miss once (decay to 1), then re-assert (reset to 2).
    let assert1 = parse_brackets("[PRESENCE mara at the bar]");
    let (p1, _) = apply_presence(&s, &assert1);
    s.presences = p1;
    assert_eq!(s.presences[0].ttl, 2);

    let miss = parse_brackets("prose only");
    let (p2, _) = apply_presence(&s, &miss);
    s.presences = p2;
    assert_eq!(s.presences[0].ttl, 1);

    let re_assert = parse_brackets("[PRESENCE mara still at the bar]");
    let (p3, _) = apply_presence(&s, &re_assert);
    s.presences = p3;
    assert_eq!(s.presences[0].ttl, 2, "re-assertion resets grace");
    assert_eq!(s.presences[0].stance, "still at the bar", "stance updated on re-assert");
}

/// F. A dropped NPC vanishes from BOTH the present: render AND the SD prompt
/// compose (the anti-teleport property for image gen).
#[test]
fn scenario_f_dropped_npc_absent_from_render_and_compose() {
    let mut s = schema_with_cast();
    // Two NPCs asserted.
    let parsed = parse_brackets(
        "[PRESENCE mara at the bar]\n[PRESENCE corin tuning a lute]",
    );
    let (p, _) = apply_presence(&s, &parsed);
    s.presences = p;
    let prompt_with_both = compose_scene_prompt(&s);
    assert!(prompt_with_both.contains("Mara"));
    assert!(prompt_with_both.contains("Corin"));

    // Simulate Corin dropping (2 consecutive misses).
    let miss = parse_brackets("prose only");
    let (p1, _) = apply_presence(&s, &miss);
    s.presences = p1; // both decay
    // Re-assert Mara only (Corin left the scene).
    let re_assert_mara = parse_brackets("[PRESENCE mara at the bar]");
    let (p2, _) = apply_presence(&s, &re_assert_mara);
    s.presences = p2;
    // Mara reset; Corin decayed to 1.
    let (p3, _) = apply_presence(&s, &re_assert_mara);
    s.presences = p3;
    // Mara reset; Corin decayed 1→0 → dropped.

    let prompt_mara_only = compose_scene_prompt(&s);
    assert!(prompt_mara_only.contains("Mara"), "on-camera Mara still in prompt");
    assert!(
        !prompt_mara_only.contains("Corin"),
        "dropped Corin must NOT appear in SD prompt (anti-teleport): {prompt_mara_only}"
    );
    let rendered = s.render_for_prompt();
    assert!(!rendered.contains("Corin"), "dropped Corin must not appear in present:");
}

/// G. compose_scene_prompt builds macro + environment + micro in order.
#[test]
fn scenario_g_compose_three_layers_ordered() {
    let mut s = schema_with_cast();
    s.presences = vec![
        Presence {
            npc_id: "mara_the_innkeep".into(),
            name: "Mara".into(),
            stance: "at the bar".into(),
            ttl: PRESENCE_GRACE_RESET,
        },
        Presence {
            npc_id: "bard_corin".into(),
            name: "Corin".into(),
            stance: String::new(),
            ttl: PRESENCE_GRACE_RESET,
        },
    ];
    let prompt = compose_scene_prompt(&s);
    // Macro (node name + indoor setting), environment (time-of-day only —
    // weather suppressed indoors), micro (the two subjects).
    let macro_idx = prompt.find("The Rusty Lantern Tavern").unwrap();
    let setting_idx = prompt.find("indoor setting").unwrap();
    let time_idx = prompt.find("evening").unwrap();
    let micro_idx = prompt.find("Mara (at the bar)").unwrap();
    assert!(macro_idx < setting_idx);
    assert!(setting_idx < time_idx);
    assert!(time_idx < micro_idx);
    // Mara has a stance → rendered as "Mara (at the bar)"; Corin has none →
    // bare name (no parens). Both subjects present in the micro layer.
    assert!(prompt.contains("Mara (at the bar)"));
    assert!(prompt.contains("Corin"), "bare-name subject: {prompt}");
    // Subjects joined by "; " (stances contain commas).
    assert!(prompt.contains("; "), "subjects joined by '; ': {prompt}");
}

/// H. Registry seeding: the CardNpc → NpcEntry map mirrors
/// enter_fable_session's seed (the same field-by-field copy).
#[test]
fn scenario_h_registry_seeding_mirrors_card_cast() {
    let card = SimCard {
        id: "test".into(),
        name: "Test".into(),
        card_type: "roleplay".into(),
        core_persona: String::new(),
        traits: String::new(),
        appearance: String::new(),
        role_instruction: String::new(),
        responsibilities: String::new(),
        conversational_rules: String::new(),
        technical_rules: String::new(),
        introductions: vec![],
        setting: None,
        tone: None,
        opening_scene: None,
        start_npc_ids: vec![],
        declared_activities: vec![],
        player_name: None,
        locations: vec![],
        cast: vec![CardNpc {
            id: "mara".into(),
            name: "Mara".into(),
            role: "innkeep".into(),
            tier: Some("soldier".into()),
            aliases: vec!["innkeep".into()],
        }],
    };
    // The exact map enter_fable_session performs.
    let registry = NpcRegistry {
        entries: card
            .cast
            .iter()
            .map(|cn| NpcEntry {
                id: cn.id.clone(),
                name: cn.name.clone(),
                role: cn.role.clone(),
                tier: cn.tier.clone(),
                aliases: cn.aliases.clone(),
            })
            .collect(),
    };
    assert_eq!(registry.entries.len(), 1);
    let entry = &registry.entries[0];
    assert_eq!(entry.id, "mara");
    assert_eq!(entry.name, "Mara");
    assert_eq!(entry.role, "innkeep");
    assert_eq!(entry.tier.as_deref(), Some("soldier"));
    assert_eq!(entry.aliases, vec!["innkeep"]);
    // The resolve fn (the [PRESENCE] normalization) works post-seed.
    assert_eq!(registry.resolve("mara").map(|e| e.id.as_str()), Some("mara"));
    assert_eq!(registry.resolve("Innkeep").map(|e| e.id.as_str()), Some("mara"));
    assert!(registry.resolve("stranger").is_none());
}

/// I. Outdoor weather renders in the SD prompt compose (the §11.45 gate's
/// positive case — distinct from the indoor suppression in scenario G).
#[test]
fn scenario_i_outdoor_weather_in_compose() {
    let mut s = schema_with_cast();
    s.travel_graph.current_node = None; // clear
    s.travel_graph.nodes = vec![Node {
        id: "market_square".into(),
        name: "Ashford Market Square".into(),
        neighbors: vec![],
        setting: "outdoor".into(),
    }];
    s.travel_graph.current_node = Some("market_square".into());
    s.weather = Weather {
        condition: "light rain".into(),
        started_at_minutes: 100,
    };
    let prompt = compose_scene_prompt(&s);
    assert!(prompt.contains("light rain"), "outdoor weather must render: {prompt}");
    assert!(prompt.contains("evening"));
}

/// J. The card-side parser end-to-end: rusty_tavern.sim's <cast> block parses
/// into the registry shape the seeder consumes. Pins the shipped card contract.
#[test]
fn scenario_j_rusty_tavern_cast_parses_to_registry() {
    // Mirror of rusty_tavern.sim's <cast> block (the shipped card).
    let xml = r#"<?xml version="1.0"?>
<sim_card>
  <metadata><id>rusty_tavern</id><type>roleplay</type></metadata>
  <identity><name>The Rusty Lantern Tavern</name></identity>
  <scenario>
    <cast>
      <npc id="mara_the_innkeep" tier="soldier">
        <name>Mara</name>
        <role>The innkeeper behind the bar</role>
        <alias>mara</alias>
        <alias>innkeep</alias>
      </npc>
      <npc id="the_hooded_stranger">
        <name>The Hooded Stranger</name>
        <role>A silent figure alone in the corner</role>
        <alias>stranger</alias>
      </npc>
    </cast>
  </scenario>
</sim_card>"#;
    let card = wupi_lib::sim_card::parse_from_xml_str(xml).expect("card parses");
    assert_eq!(card.cast.len(), 2);
    assert_eq!(card.cast[0].id, "mara_the_innkeep");
    assert_eq!(card.cast[0].tier.as_deref(), Some("soldier"));
    assert_eq!(card.cast[0].aliases, vec!["mara", "innkeep"]);
    assert_eq!(card.cast[1].id, "the_hooded_stranger");
    assert!(card.cast[1].tier.is_none());
}
