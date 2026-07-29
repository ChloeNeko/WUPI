//! New Game Interview — scenario integration test.
//!
//! This is the script-only verification gate for the New Game interview
//! feature (Phase C.4). It exercises the full pure-Rust pipeline end-to-end
//! at the API level — `sim_draft` tool-call parse → `DraftUpdate` validation
//! → `InterviewDraft` mutation → `to_sim_card_xml` → `sim_card::parse`
//! round-trip → `to_world_schema` / `to_player_state` seeding — WITHOUT the
//! live app, WITHOUT the schema lock, WITHOUT IPC, WITHOUT a model.
//!
//! The unit tests in `interview_draft.rs`, `tools.rs`, `gm_prompt.rs`, and
//! `scribe_prompt.rs` pin each piece in isolation. THIS test is the
//! integration proof that they compose into the contract the locked design
//! specified:
//!
//!   "The local Gemma Scribe extracts facts from the conversation and emits
//!    batched sim_draft tool calls; Rust assembles a flawless .sim card +
//!    seeds the starting world/player state. The scribe NEVER writes XML;
//!    Rust builds it. Failure degrades gracefully."
//!
//! Scenarios covered:
//!
//!   A. Empty draft → not finalizable, missing_required lists name+setting.
//!   B. Minimal draft (name + setting) IS finalizable; player defaults to "User".
//!   C. Full fantasy draft → valid XML → parses back via sim_card::parse.
//!   D. Cyberpunk draft (genre-agnostic proof) → same round-trip.
//!   E. sim_draft tool applies a realistic batched update set atomically.
//!   F. sim_draft tool REJECTS a partially-invalid batch (atomicity).
//!   G. sim_draft tool degrades gracefully when no slot attached.
//!   H. Card id sanitization (name → filesystem-safe stem).
//!   I. World schema seeding: entities + player background/condition land.
//!   J. Player state seeding: "exhausted" token drops stamina.
//!   K. §11.29 naming: the produced XML + summary NEVER contain banned words.
//!   L. State summary compensates for the 6-turn window (always shows Player).
//!   M. Legacy <protagonist> tag auto-migrates to player_name on parse.
//!
//! Verification status: build + unit-test verified only. A consolidated live
//! CDP roleplay playtest (mirroring §11.38) is Phase E.

use wupi_lib::interview_draft::{
    sanitize_stem, DraftUpdate, InterviewDraft, DEFAULT_PLAYER_NAME,
};
use wupi_lib::sim_card;
use wupi_lib::tools::{interview_registry, interview_specs, Tool, ToolCtx};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a JSON arg string the way the production tool-call parser would
/// hand it to `SimDraft::execute`.
fn args(json: &str) -> serde_json::Value {
    serde_json::from_str(json).unwrap()
}

/// The banned-words list per §11.29 (hardened). NONE of these may appear in
/// any user/GM/Scribe-facing surface the model reads. The list itself is
/// test-internal — it never ships in a prompt (that would echo).
const BANNED_TITLE_WORDS: &[&str] = &[
    "hero",
    "chosen one",
    "main character",
    "adventurer",
    "protagonist",
];

/// Assert no banned title word appears (case-insensitive) in the produced
/// text. Used on the XML output + the state summary.
fn assert_no_banned_titles(text: &str, label: &str) {
    let lower = text.to_lowercase();
    for banned in BANNED_TITLE_WORDS {
        assert!(
            !lower.contains(banned),
            "[{}] produced text contains banned title word '{}':\n---\n{}\n---",
            label,
            banned,
            text
        );
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// A. Empty draft is not finalizable; the missing list reports what's needed.
#[test]
fn a_empty_draft_not_finalizable_reports_missing() {
    let d = InterviewDraft::default();
    assert!(!d.is_finalizable());
    let missing = d.missing_required();
    assert!(missing.contains(&"name"), "missing list has name: {:?}", missing);
    assert!(missing.contains(&"setting"), "missing list has setting: {:?}", missing);
    // player_name is NOT in the missing list — it's optional (defaults to User).
    assert!(
        !missing.iter().any(|m| m.contains("player")),
        "player_name must not be required: {:?}",
        missing
    );
    assert_eq!(d.completion_pct(), 0);
}

/// B. A minimal draft (name + setting) is finalizable; player defaults to "User".
#[test]
fn b_minimal_draft_finalizable_player_defaults_to_user() {
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![
        DraftUpdate::SetField {
            field: "name".into(),
            value: "The Neon Dragon".into(),
        },
        DraftUpdate::SetField {
            field: "setting".into(),
            value: "3 AM in the lower arcology.".into(),
        },
    ])
    .unwrap();
    assert!(d.is_finalizable());
    assert!(d.missing_required().is_empty());
    // Player name was never set → effective name is "User".
    assert_eq!(d.effective_player_name(), DEFAULT_PLAYER_NAME);
    assert_eq!(d.effective_player_name(), "User");
}

/// C. A full fantasy draft round-trips through the parser as a valid card.
#[test]
fn c_full_fantasy_draft_round_trips_as_valid_card() {
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![
        DraftUpdate::SetField {
            field: "name".into(),
            value: "The Rusty Lantern Tavern".into(),
        },
        DraftUpdate::SetField {
            field: "setting".into(),
            value: "Night; rain drums on the shutters. Lantern light, warped oak.".into(),
        },
        DraftUpdate::SetField {
            field: "tone".into(),
            value: "Slow-burn. NPCs are people, not quest dispensers.".into(),
        },
        DraftUpdate::SetField {
            field: "opening_scene".into(),
            value: "The door swings shut behind you, cutting off the cold.".into(),
        },
        DraftUpdate::SetField {
            field: "core_persona".into(),
            value: "A sandbox tavern. No fixed plot — the world breathes.".into(),
        },
        DraftUpdate::SetField {
            field: "player_name".into(),
            value: "Kaelen".into(),
        },
        DraftUpdate::AddNpc {
            id: "mara_the_innkeep".into(),
        },
        DraftUpdate::AddNpc {
            id: "the_hooded_stranger".into(),
        },
        DraftUpdate::AddActivity {
            value: "conversation".into(),
        },
        DraftUpdate::AddActivity {
            value: "exploration".into(),
        },
        DraftUpdate::AddTrait {
            value: "Atmospheric.".into(),
        },
    ])
    .unwrap();
    assert!(d.is_finalizable());
    // 7 of 8 slots filled (no player_background in this draft) → 87%.
    assert_eq!(d.completion_pct(), 87);

    let xml = d.to_sim_card_xml().expect("xml builds");
    // §11.29: no banned title words in the produced card.
    assert_no_banned_titles(&xml, "fantasy XML");

    // Round-trip through the real parser.
    let card = sim_card::parse_from_xml_str(&xml).expect("card parses");
    assert_eq!(card.id, "the-rusty-lantern-tavern");
    assert_eq!(card.name, "The Rusty Lantern Tavern");
    assert_eq!(card.card_type, "roleplay");
    assert_eq!(card.player_name.as_deref(), Some("Kaelen"));
    assert_eq!(card.start_npc_ids.len(), 2);
    assert!(card.start_npc_ids.contains(&"mara_the_innkeep".to_string()));
    assert!(card.declared_activities.contains(&"conversation".to_string()));
    assert!(card.setting.as_deref().unwrap().contains("rain"));
    assert!(card.tone.as_deref().unwrap().contains("Slow-burn"));
}

/// D. The same pipeline produces a clean cyberpunk card — genre-agnostic.
#[test]
fn d_cyberpunk_draft_round_trips_as_valid_card() {
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![
        DraftUpdate::SetField {
            field: "name".into(),
            value: "The Neon Dragon".into(),
        },
        DraftUpdate::SetField {
            field: "setting".into(),
            value: "3 AM. Holographic koi drift under a stained ceiling. Bass hum.".into(),
        },
        DraftUpdate::SetField {
            field: "tone".into(),
            value: "Noir. Everyone wants something; nobody shows their hand.".into(),
        },
        DraftUpdate::SetField {
            field: "opening_scene".into(),
            value: "You slide into a booth. The fixer opposite doesn't look up.".into(),
        },
        DraftUpdate::AddNpc {
            id: "vex_the_fixer".into(),
        },
        DraftUpdate::AddNpc {
            id: "decker_rin".into(),
        },
    ])
    .unwrap();
    let xml = d.to_sim_card_xml().unwrap();
    assert_no_banned_titles(&xml, "cyberpunk XML");
    let card = sim_card::parse_from_xml_str(&xml).unwrap();
    assert_eq!(card.id, "the-neon-dragon");
    // No player_name set → defaults to "User" in the XML.
    assert_eq!(card.player_name.as_deref(), Some("User"));
    assert!(card.setting.as_deref().unwrap().contains("Holographic koi"));
    assert!(card.tone.as_deref().unwrap().contains("Noir"));
}

/// E. The sim_draft tool applies a realistic batched update atomically.
/// Mirrors what the Scribe would emit after a GM turn.
#[test]
fn e_sim_draft_applies_batched_updates() {
    let slot: Arc<Mutex<Option<InterviewDraft>>> = Arc::new(Mutex::new(Some(InterviewDraft::default())));
    let ctx = ToolCtx::new(PathBuf::from("/tmp")).with_interview_draft(slot.clone());

    // Realistic Scribe batch: name + setting + 2 NPCs + a trait.
    let payload = args(
        r#"{"updates":[
            {"type":"set_field","field":"name","value":"The Neon Dragon"},
            {"type":"set_field","field":"setting","value":"3 AM in the arcology."},
            {"type":"add_npc","id":"vex"},
            {"type":"add_npc","id":"rin"},
            {"type":"add_trait","value":"Noir."}
        ]}"#,
    );

    // Find the SimDraft tool in the interview registry.
    let tools = interview_registry();
    let sim_draft = tools
        .iter()
        .find(|t| t.spec().name == "sim_draft")
        .expect("interview_registry contains sim_draft");
    let out = sim_draft.execute(&payload, &ctx).expect("batch applies");
    assert!(out.contains("applied 5 updates"), "summary: {}", out);

    let g = slot.lock().unwrap();
    let draft = g.as_ref().unwrap();
    assert_eq!(draft.name.as_deref(), Some("The Neon Dragon"));
    assert_eq!(draft.start_npc_ids.len(), 2);
    // AddNpc stubs a char.<id>.name world entity.
    assert_eq!(
        draft.world_entities.get("char.vex.name").map(|s| s.as_str()),
        Some("vex")
    );
    assert_eq!(draft.last_updated_turn, 1);
}

/// F. The sim_draft tool REJECTS a partially-invalid batch atomically —
/// nothing is applied, the draft stays clean.
#[test]
fn f_sim_draft_rejects_partial_batch_atomically() {
    let slot: Arc<Mutex<Option<InterviewDraft>>> = Arc::new(Mutex::new(Some(InterviewDraft::default())));
    let ctx = ToolCtx::new(PathBuf::from("/tmp")).with_interview_draft(slot.clone());

    // First a valid update, then a bogus SetField field.
    let payload = args(
        r#"{"updates":[
            {"type":"set_field","field":"name","value":"X"},
            {"type":"set_field","field":"bogus_field","value":"Y"}
        ]}"#,
    );
    let tools = interview_registry();
    let sim_draft = tools.iter().find(|t| t.spec().name == "sim_draft").unwrap();
    let err = sim_draft.execute(&payload, &ctx).unwrap_err();
    assert!(err.message.contains("batch rejected"));

    // Atomicity: the valid update did NOT apply.
    let g = slot.lock().unwrap();
    let draft = g.as_ref().unwrap();
    assert!(draft.name.is_none(), "valid update in a rejected batch must not apply");
    assert_eq!(draft.last_updated_turn, 0);
}

/// G. The sim_draft tool degrades gracefully when invoked without a draft
/// slot attached (defensive — the spec says interview-only, but a model could
/// still emit it).
#[test]
fn g_sim_draft_degrades_without_slot() {
    let ctx = ToolCtx::new(PathBuf::from("/tmp")); // no with_interview_draft
    let payload = args(r#"{"updates":[{"type":"add_trait","value":"x"}]}"#);
    let tools = interview_registry();
    let sim_draft = tools.iter().find(|t| t.spec().name == "sim_draft").unwrap();
    let err = sim_draft.execute(&payload, &ctx).unwrap_err();
    assert!(err.message.contains("interview draft"));
}

/// H. Card id sanitization: name → filesystem-safe stem.
#[test]
fn h_card_id_sanitization() {
    let cases = [
        ("The Neon Dragon", "the-neon-dragon"),
        ("Mara the Innkeep!", "mara-the-innkeep"),
        ("  --Weird--  Name--  ", "weird-name"),
        ("UPPER", "upper"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            sanitize_stem(input),
            expected,
            "sanitize_stem({:?})",
            input
        );
    }
    // And via the draft's card_id() method.
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![DraftUpdate::SetField {
        field: "name".into(),
        value: "The Neon Dragon".into(),
    }])
    .unwrap();
    assert_eq!(d.card_id().as_deref(), Some("the-neon-dragon"));
}

/// I. World schema seeding: the draft's entities + player background +
/// condition land in the schema's entities map, keyed off the player name.
#[test]
fn i_world_schema_seeds_entities_and_player_notes() {
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![
        DraftUpdate::SetField {
            field: "name".into(),
            value: "The Rusty Lantern".into(),
        },
        DraftUpdate::SetField {
            field: "setting".into(),
            value: "Rain on the shutters.".into(),
        },
        DraftUpdate::SetField {
            field: "player_name".into(),
            value: "Kaelen".into(),
        },
        DraftUpdate::AddEntity {
            key: "loc.tavern".into(),
            state: "warm, half-full".into(),
        },
        DraftUpdate::SetPlayerBackground {
            value: "A traveling herbalist.".into(),
        },
        DraftUpdate::SetStartingCondition {
            value: "exhausted from the road".into(),
        },
    ])
    .unwrap();
    let schema = d.to_world_schema();
    assert_eq!(
        schema.entities.get("loc.tavern").map(|s| s.as_str()),
        Some("warm, half-full")
    );
    // Player notes keyed off the sanitized effective player name.
    assert_eq!(
        schema.entities.get("char.kaelen.background").map(|s| s.as_str()),
        Some("A traveling herbalist.")
    );
    assert_eq!(
        schema.entities.get("char.kaelen.condition").map(|s| s.as_str()),
        Some("exhausted from the road")
    );
}

/// I.b. When no player_name is set, the notes key off "User" (never a title).
#[test]
fn i_b_world_schema_keys_off_user_when_no_name() {
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![
        DraftUpdate::SetField {
            field: "name".into(),
            value: "The Neon Dragon".into(),
        },
        DraftUpdate::SetField {
            field: "setting".into(),
            value: "x".into(),
        },
        DraftUpdate::SetPlayerBackground {
            value: "A burnt-out decker.".into(),
        },
    ])
    .unwrap();
    let schema = d.to_world_schema();
    assert_eq!(
        schema.entities.get("char.user.background").map(|s| s.as_str()),
        Some("A burnt-out decker.")
    );
    // No banned-title entity keys leaked.
    for key in schema.entities.keys() {
        for banned in BANNED_TITLE_WORDS {
            let banned_key = banned.replace(' ', "_");
            assert!(
                !key.to_lowercase().contains(&banned_key),
                "entity key '{}' contains banned word '{}'",
                key,
                banned
            );
        }
    }
}

/// J. Player state seeding: the "exhausted" starting-condition token drops
/// stamina (the v1 heuristic). Default when no condition is fully healthy.
#[test]
fn j_player_state_seeds_stamina_from_condition_token() {
    use wupi_lib::player_state::Stamina;

    // Exhausted → stamina drops.
    let mut d_exhausted = InterviewDraft::default();
    d_exhausted.apply_updates(vec![
        DraftUpdate::SetField { field: "name".into(), value: "X".into() },
        DraftUpdate::SetField { field: "setting".into(), value: "x".into() },
        DraftUpdate::SetStartingCondition {
            value: "exhausted from the long road".into(),
        },
    ]).unwrap();
    let s_exhausted = d_exhausted.to_player_state();
    assert_eq!(s_exhausted.stamina, Stamina::Exhausted);

    // No condition → fully healthy.
    let mut d_default = InterviewDraft::default();
    d_default.apply_updates(vec![
        DraftUpdate::SetField { field: "name".into(), value: "X".into() },
        DraftUpdate::SetField { field: "setting".into(), value: "x".into() },
    ]).unwrap();
    let s_default = d_default.to_player_state();
    assert!(s_default.is_default(), "no condition → fully healthy default state");
}

/// K. §11.29 (hardened): the produced XML + state summary NEVER contain any
/// banned title word. The model reads these surfaces; a leak would re-bias
/// it toward coddling the player.
#[test]
fn k_no_banned_titles_in_any_model_facing_surface() {
    let mut d = InterviewDraft::default();
    d.apply_updates(vec![
        DraftUpdate::SetField {
            field: "name".into(),
            value: "Some Scenario".into(),
        },
        DraftUpdate::SetField {
            field: "setting".into(),
            value: "x".into(),
        },
        DraftUpdate::SetField {
            field: "player_name".into(),
            value: "Kaelen".into(),
        },
    ])
    .unwrap();

    // The XML the Scribe's output produces (the .sim file content).
    let xml = d.to_sim_card_xml().unwrap();
    assert_no_banned_titles(&xml, "XML");

    // The state summary the GM sees each turn.
    let summary = d.render_state_summary().unwrap();
    assert_no_banned_titles(&summary, "state summary");

    // The gm.codex playbook entries the GM retrieves.
    let candidates = [
        std::path::PathBuf::from("../data/gm.codex"),
        std::path::PathBuf::from("data/gm.codex"),
    ];
    if let Some(path) = candidates.iter().find(|p| p.is_file()) {
        let body = std::fs::read_to_string(path).unwrap();
        assert_no_banned_titles(&body, "gm.codex");
    }
}

/// L. The state summary ALWAYS shows a Player line (defaults to "User"),
/// even for an empty draft — it compensates for the 6-turn history window.
#[test]
fn l_state_summary_always_shows_player_line() {
    // Empty draft.
    let d_empty = InterviewDraft::default();
    let s_empty = d_empty.render_state_summary().unwrap();
    assert!(s_empty.contains("Player: User"));
    assert!(s_empty.starts_with("<draft_state>"));

    // Draft with a volunteered name.
    let mut d_named = InterviewDraft::default();
    d_named.apply_updates(vec![
        DraftUpdate::SetField { field: "name".into(), value: "X".into() },
        DraftUpdate::SetField { field: "setting".into(), value: "x".into() },
        DraftUpdate::SetField { field: "player_name".into(), value: "Kaelen".into() },
    ]).unwrap();
    let s_named = d_named.render_state_summary().unwrap();
    assert!(s_named.contains("Player: Kaelen"));
    assert!(s_named.contains("Name: X"));
}

/// M. Backwards-compat: a legacy `.sim` file using the pre-rename
/// `<protagonist>` tag auto-migrates to `player_name` on parse. (Old user-
/// authored cards in the wild must still load.)
#[test]
fn m_legacy_tag_auto_migrates_to_player_name() {
    let xml = r#"<sim_card>
  <metadata><id>legacy</id><type>roleplay</type></metadata>
  <identity><name>Legacy</name></identity>
  <scenario>
    <setting>x.</setting>
    <protagonist>Kaelen</protagonist>
  </scenario>
</sim_card>"#;
    let card = sim_card::parse_from_xml_str(xml).expect("legacy parses");
    assert_eq!(card.player_name.as_deref(), Some("Kaelen"));
}

/// N. The interview_specs() list is non-empty + contains sim_draft (the
/// scribe's only tool). Catches a registry regression that would silently
/// break the scribe.
#[test]
fn n_interview_specs_populated_with_sim_draft() {
    let specs = interview_specs();
    assert!(!specs.is_empty(), "interview specs non-empty");
    assert!(
        specs.iter().any(|s| s.name == "sim_draft"),
        "interview specs contain sim_draft: {:?}",
        specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
    );
}

/// O. End-to-end happy path: build a draft via realistic Scribe-shaped
/// batches → finalize → parse back → confirm every field landed. This is
/// the integration proof that the pieces compose into the contract.
#[test]
fn o_end_to_end_happy_path() {
    let slot: Arc<Mutex<Option<InterviewDraft>>> = Arc::new(Mutex::new(Some(InterviewDraft::default())));
    let ctx = ToolCtx::new(PathBuf::from("/tmp")).with_interview_draft(slot.clone());
    let tools = interview_registry();
    let sim_draft = tools.iter().find(|t| t.spec().name == "sim_draft").unwrap();

    // Turn 1 scribe batch (after the GM asks archetype + name + setting).
    sim_draft.execute(&args(
        r#"{"updates":[
            {"type":"set_field","field":"name","value":"The Rusty Lantern Tavern"},
            {"type":"set_field","field":"core_persona","value":"A sandbox tavern."},
            {"type":"set_field","field":"setting","value":"Night; rain on the shutters."},
            {"type":"set_field","field":"tone","value":"Slow-burn."}
        ]}"#,
    ), &ctx).unwrap();

    // Turn 2 scribe batch (after the GM asks about NPCs + player name).
    sim_draft.execute(&args(
        r#"{"updates":[
            {"type":"set_field","field":"player_name","value":"Kaelen"},
            {"type":"add_npc","id":"mara_the_innkeep"},
            {"type":"add_npc","id":"bard_corin"},
            {"type":"add_entity","key":"char.mara_the_innkeep.demeanor","state":"measured, watchful"},
            {"type":"set_player_background","value":"A traveling herbalist, three days on the road."}
        ]}"#,
    ), &ctx).unwrap();

    // Turn 3 scribe batch (after the GM asks about the opening scene).
    sim_draft.execute(&args(
        r#"{"updates":[
            {"type":"set_field","field":"opening_scene","value":"The door swings shut behind you."},
            {"type":"add_activity","value":"conversation"},
            {"type":"set_starting_condition","value":"weary from the road"}
        ]}"#,
    ), &ctx).unwrap();

    // Finalize: take the draft, build the card, round-trip.
    let draft = slot.lock().unwrap().take().unwrap();
    assert!(draft.is_finalizable(), "draft is finalizable after 3 turns");
    assert_eq!(draft.last_updated_turn, 3);
    let xml = draft.to_sim_card_xml().unwrap();
    let card = sim_card::parse_from_xml_str(&xml).unwrap();
    assert_eq!(card.id, "the-rusty-lantern-tavern");
    assert_eq!(card.player_name.as_deref(), Some("Kaelen"));
    assert_eq!(card.start_npc_ids.len(), 2);
    // World schema carries the entity the Scribe added.
    let schema = draft.to_world_schema();
    assert_eq!(
        schema.entities.get("char.mara_the_innkeep.demeanor").map(|s| s.as_str()),
        Some("measured, watchful")
    );
    // Player state reflects the weary condition.
    let state = draft.to_player_state();
    use wupi_lib::player_state::Stamina;
    assert_ne!(state.stamina, Stamina::Fresh, "weary condition dropped stamina");
    // No banned titles anywhere in the final surfaces.
    assert_no_banned_titles(&xml, "happy-path XML");
    let summary = draft.render_state_summary().unwrap();
    assert_no_banned_titles(&summary, "happy-path summary");
}
