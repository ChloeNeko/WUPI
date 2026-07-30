//! The Scribe's system prompt for the New Game interview flow.
//!
//! The Scribe (local Gemma 12B) runs hidden in the background after each GM
//! turn. It listens to the full interview transcript + the current draft
//! state and emits `sim_draft` tool calls that incrementally build the
//! `InterviewDraft`. This module builds the Scribe's tool-calling system
//! prompt.
//!
//! **Naming contract (§11.29, load-bearing):** the player is "User" unless
//! they volunteer a name. The Scribe surfaces `player_name` only when the
//! player explicitly gives one. The player is referred to neutrally
//! throughout — treated like any other person in the world, no special
//! framing.
//!
//! **Idempotent by design:** the Scribe sees the FULL transcript every turn
//! (no windowing on extraction). Re-extracting the same fact across turns is
//! safe — `InterviewDraft::apply_updates` is idempotent on exact matches
//! (AddTrait / AddNpc / AddActivity dedupe; SetField / SetPlayerBackground
//! overwrite cleanly). The Scribe does NOT need to remember what it already
//! extracted — it just extracts what's true NOW.

use crate::interview_draft::InterviewDraft;

/// Build the Scribe's system prompt for one extraction pass.
///
/// `transcript` is the FULL interview exchange so far (GM + player turns,
/// serialized as plain text — the Scribe reads it like a script). `draft` is
/// the current draft state (folded in so the Scribe knows what's already
/// established — though re-extraction is safe, surfacing the current state
/// helps it avoid noisy re-emissions and focus on what's NEW this turn).
pub fn build_scribe_system_prompt(transcript: &str, draft: &InterviewDraft) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<scribe_role>\n");
    out.push_str(SCRIBE_ROLE);
    out.push_str("\n</scribe_role>\n\n");

    out.push_str("<scribe_contract>\n");
    out.push_str(SCRIBE_CONTRACT);
    out.push_str("\n</scribe_contract>\n\n");

    out.push_str("<sim_draft_tool>\n");
    out.push_str(SIM_DRAFT_TOOL_DOC);
    out.push_str("\n</sim_draft_tool>\n\n");

    out.push_str("<card_target_shape>\n");
    out.push_str(CARD_TARGET_SHAPE);
    out.push_str("\n</card_target_shape>\n\n");

    out.push_str("<world_entity_namespaces>\n");
    out.push_str(WORLD_ENTITY_NAMESPACES);
    out.push_str("\n</world_entity_namespaces>\n\n");

    out.push_str("<worked_example>\n");
    out.push_str(WORKED_EXAMPLE);
    out.push_str("\n</worked_example>\n\n");

    // The current draft state — so the Scribe sees what's established and
    // can focus on what's NEW this turn. (Re-extracting is safe per the
    // idempotent contract above; this just reduces noise.)
    if let Some(summary) = draft.render_state_summary() {
        out.push_str("<current_draft_state>\n");
        out.push_str(&summary);
        out.push_str("\n</current_draft_state>\n\n");
    }

    out.push_str("<interview_transcript>\n");
    out.push_str(transcript);
    out.push_str("\n</interview_transcript>\n\n");

    out.push_str("<final_instruction>\n");
    out.push_str(FINAL_INSTRUCTION);
    out.push_str("\n</final_instruction>");
    out
}

const SCRIBE_ROLE: &str = "\
You are the SCRIBE — a quiet background assistant listening to a Game Master \
interviewing a player about a new roleplay scenario. You NEVER speak to the \
player. Your ONLY output is a single `sim_draft` tool call extracting the \
facts established in the transcript into the building SimCard draft.";

const SCRIBE_CONTRACT: &str = "\
EXTRACT — do not invent. Only capture what the player or GM explicitly stated \
or directly implied. If the player said 'her name is Mara', capture npc=mara. \
If the player said 'she seems like a Mara type', DO NOT capture — that's \
ambiguous.\
\n\n\
QUALITATIVE ONLY — no numbers, no stats, no HP, no levels, no 'Charisma 16'. \
The descriptive layer is banned from numbers; Rust owns the dice. A condition \
is 'exhausted from the road', never 'stamina 3/10'. Wealth is 'low on coin', \
never '50 gold'.\
\n\n\
NAMING (§11.29): the player is 'User' unless they explicitly volunteer a \
name. If the player says 'I'm Kaelen', extract player_name='Kaelen'. If they \
never name themselves, leave player_name unset; the system defaults to \
'User' for them.\
\n\n\
IDEMPOTENT: you see the FULL transcript every turn. Re-extracting a fact you \
already captured is safe (the system dedupes), but for cleanliness, focus on \
what's NEW or CHANGED this turn. The <current_draft_state> block shows what's \
already established.";

const SIM_DRAFT_TOOL_DOC: &str = "\
Call `sim_draft` ONCE per turn with a batched `updates` array. Each update is \
one of:\
\n\n\
- {\"type\":\"set_field\",\"field\":\"name|setting|tone|opening_scene|player_name|core_persona\",\"value\":\"...\"}\
\n  - `name`: the scenario/card name (e.g. 'The Rusty Lantern Tavern', 'The Neon Dragon')\
\n  - `setting`: where this takes place, sensory detail\
\n  - `tone`: narrative voice ('slow-burn', 'noir', 'pulpy')\
\n  - `opening_scene`: the seed text for the first narrator turn\
\n  - `player_name`: ONLY if the player explicitly volunteered one\
\n  - `core_persona`: one-line summary of what the card IS\
\n- {\"type\":\"add_trait\",\"value\":\"- Measured, never cold.\"}  (a bullet, with the `- ` prefix)\
\n- {\"type\":\"add_npc\",\"id\":\"mara_the_innkeep\"}  (snake_case id; also stubs a char.<id>.name entity)\
\n- {\"type\":\"add_activity\",\"value\":\"conversation\"}\
\n- {\"type\":\"add_entity\",\"key\":\"loc.tavern\",\"state\":\"warm, half-full\"}\
\n- {\"type\":\"set_player_background\",\"value\":\"a traveling herbalist, three days on the road\"}\
\n- {\"type\":\"set_starting_condition\",\"value\":\"exhausted from the road\"}\
\n- {\"type\":\"set_start_node\",\"value\":\"tavern\"}  (optional; starting travel-graph node)\
\n- {\"type\":\"set_locations\",\"nodes\":[{\"id\":\"tavern\",\"name\":\"The Rusty Lantern\",\"setting\":\"indoor\",\"neighbors\":[\"cellar\",\"market\"]},{\"id\":\"cellar\",\"name\":\"The Cellar\",\"setting\":\"indoor\",\"neighbors\":[\"tavern\"]}]}\
\n  (the WHOLE travel graph in one call — idempotent overwrite; emit ONCE the geography is clear. 2-6 nodes. The FIRST node is where the player starts. `setting` is \"indoor\" or \"outdoor\" — indoor hides the weather line. CRITICAL: edges are NOT auto-symmetrized — if tavern lists cellar as a neighbor, cellar MUST list tavern back, or travel one way will fail.)\
\n\n\
If nothing new was established this turn, call sim_draft with an empty-rejecting \
update OR emit no tool call at all — both are valid. Do not force extractions.";

const CARD_TARGET_SHAPE: &str = "\
The draft builds toward a `.sim` card with this shape (Rust assembles the XML; \
you never write XML yourself):\
\n\n\
- metadata: id (derived from name), type='roleplay'\
- identity: name, core_persona, traits (bullet list)\
- scenario: setting, tone, the player's name field (XML tag `<player_name>`, \
  defaults to 'User'), opening_scene, start_npcs (id list), activities, locations \
  (the travel graph — what's reachable from where; first node is the player's start)\
\n\n\
A great card is specific + sensory + leaves room for the player. Bad cards are \
stat sheets. See the fable.codex 'Perfect World Card Examples' / 'Perfect \
Character Card Examples' entries for the quality bar.";

const WORLD_ENTITY_NAMESPACES: &str = "\
World entities follow these namespace conventions (the narrator's renderers \
key off the prefix):\
\n\n\
- loc.<place>     — a location's ATMOSPHERE (e.g. loc.tavern = 'warm fire, half-full'); freeform flavor\
\n- char.<id>.<facet> — an NPC facet (facets: name, role, demeanor, secret)\
\n- item.<id>       — a starting item in the player's possession\
\n- faction.<id>    — a group with its own agenda\
\n\n\
Note: `loc.<place>` entities are FLAVOR. The travel GRAPH (what connects to \
what, for `[TRAVEL]`/`[RUMOR]`) is a separate thing — author it via \
`set_locations`, NOT via loc.* entities. Do not duplicate.\
\n\n\
3-8 entities is plenty. The world grows via play; do not over-seed.";

const WORKED_EXAMPLE: &str = "\
TRANSCRIPT EXCERPT:\
\n  GM: What kind of place are we building?\
\n  Player: A cyberpunk bar. Black market. The Neon Dragon.\
\n  GM: Lovely. Where exactly?\
\n  Player: Lower arcology, 3 AM, holo-koi drifting under the ceiling.\
\n  GM: Who's there when the player walks in?\
\n  Player: A fixer named Vex. She never looks up.\
\n  GM: And what else is reachable from the bar?\
\n  Player: A back office behind a curtain, and the alley out front.\
\n\n\
YOUR TOOL CALL:\
\n  <|tool_call>call:sim_draft{\"updates\":[\
    {\"type\":\"set_field\",\"field\":\"name\",\"value\":\"The Neon Dragon\"},\
    {\"type\":\"set_field\",\"field\":\"core_persona\",\"value\":\"A black-market fixer bar in the lower arcology.\"},\
    {\"type\":\"set_field\",\"field\":\"setting\",\"value\":\"3 AM in the lower arcology. Holographic koi drift under a stained ceiling. The bass hums.\"},\
    {\"type\":\"set_field\",\"field\":\"tone\",\"value\":\"Noir, paranoid.\"},\
    {\"type\":\"add_npc\",\"id\":\"vex_the_fixer\"},\
    {\"type\":\"add_entity\",\"key\":\"char.vex_the_fixer.name\",\"state\":\"Vex\"},\
    {\"type\":\"add_entity\",\"key\":\"char.vex_the_fixer.demeanor\",\"state\":\"never looks up; has seen three corps fall\"},\
    {\"type\":\"set_locations\",\"nodes\":[\
      {\"id\":\"bar\",\"name\":\"The Neon Dragon Bar\",\"setting\":\"indoor\",\"neighbors\":[\"back_office\",\"alley\"]},\
      {\"id\":\"back_office\",\"name\":\"The Back Office\",\"setting\":\"indoor\",\"neighbors\":[\"bar\"]},\
      {\"id\":\"alley\",\"name\":\"The Alley Out Front\",\"setting\":\"outdoor\",\"neighbors\":[\"bar\"]}\
    ]}\
  ]}<tool_call|>\
\n\n\
Note: no player_name extracted (the player never named themselves). No stats. \
The tone is qualitative. The NPC id is snake_case. The graph is bidirectional — \
bar lists back_office + alley, and EACH lists bar back (edges are not auto-symmetrized). \
`bar` is first → the player starts there. `alley` is outdoor → the weather line shows there.";

const FINAL_INSTRUCTION: &str = "\
Read the <interview_transcript>. Extract what the player and GM established \
into a single `sim_draft` call. If nothing new was established this turn, emit \
no tool call. Never speak to the player. Never write XML. Never invent.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview_draft::InterviewDraft;

    #[test]
    fn prompt_includes_all_sections() {
        let draft = InterviewDraft::default();
        let prompt = build_scribe_system_prompt("GM: Hello.\nPlayer: Hi.", &draft);
        for marker in [
            "<scribe_role>",
            "<scribe_contract>",
            "<sim_draft_tool>",
            "<card_target_shape>",
            "<world_entity_namespaces>",
            "<worked_example>",
            "<interview_transcript>",
            "<final_instruction>",
        ] {
            assert!(prompt.contains(marker), "missing section {}", marker);
        }
    }

    #[test]
    fn prompt_folds_in_transcript() {
        let draft = InterviewDraft::default();
        let transcript = "GM: What's your name?\nPlayer: I'm Kaelen.";
        let prompt = build_scribe_system_prompt(transcript, &draft);
        assert!(prompt.contains("I'm Kaelen."));
    }

    #[test]
    fn prompt_folds_in_current_draft_state_when_non_empty() {
        let mut draft = InterviewDraft::default();
        draft.name = Some("The Neon Dragon".to_string());
        let prompt = build_scribe_system_prompt("...", &draft);
        assert!(prompt.contains("<current_draft_state>"));
        assert!(prompt.contains("The Neon Dragon"));
    }

    #[test]
    fn prompt_omits_current_draft_state_when_empty() {
        // Empty draft → render_state_summary returns just "Player: User"
        // (§11.29: the Player line always renders). That's still useful
        // context for the Scribe, so it DOES appear. This test pins that the
        // section renders even for an empty draft (the Player default is
        // meaningful signal).
        let draft = InterviewDraft::default();
        let prompt = build_scribe_system_prompt("...", &draft);
        assert!(prompt.contains("<current_draft_state>"));
        assert!(prompt.contains("Player: User"));
    }

    #[test]
    fn prompt_uses_no_titled_defaults_for_player() {
        // §11.29 (hardened): the Scribe's prompt must not address or label
        // the player by any titled default — no "hero", "chosen one", "main
        // character", etc. The Scribe reads this prompt; if it saw a title it
        // might surface it in extracted facts.
        let draft = InterviewDraft::default();
        let prompt = build_scribe_system_prompt("...", &draft);
        let lower = prompt.to_lowercase();
        for banned in [
            "hero",
            "chosen one",
            "main character",
            "adventurer",
        ] {
            assert!(
                !lower.contains(banned),
                "Scribe prompt must not label the player as '{}'",
                banned
            );
        }
    }

    #[test]
    fn prompt_documents_player_name_as_optional() {
        let draft = InterviewDraft::default();
        let prompt = build_scribe_system_prompt("...", &draft);
        // The Scribe must be told player_name is extracted ONLY when the
        // player volunteers one; otherwise it stays unset (system defaults
        // to 'User'). Stated positively — no anti-echo meta-rules.
        assert!(prompt.contains("ONLY if the player explicitly volunteered"));
        // And the system-defaults-to-User contract must be stated.
        assert!(prompt.contains("defaults to 'User'"));
    }
}
