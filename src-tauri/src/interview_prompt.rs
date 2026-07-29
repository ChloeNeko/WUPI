//! System prompt for the Quick Play generation step.
//!
//! Quick Play is the title-screen flow where the user answers four hardcoded
//! questions in the void (character / setting / plot / extra), then sits in
//! silence while the model weaves a complete simulation from those answers.
//! This module owns the ONE system prompt that drives that generation.
//!
//! ## Output contract (load-bearing)
//!
//! The model emits THREE independently-parseable blocks, each wrapped in its
//! own XML tag — NOT a single JSON envelope. Rationale (Prime Directive
//! §1B.3): nesting a `<sim_card>` XML string inside a JSON value would force
//! the model to escape every quote + angle bracket, which it frequently
//! botches. Three tagged blocks let each payload be parsed by its native
//! parser (`sim_card::parse_from_xml_str` for the card, serde JSON for the
//! schema + player state) with `json_repair` as the JSON fallback. This is
//! the same XML-tagged-region pattern the rest of the engine uses.
//!
//! ```text
//! <sim_card> ...full card XML... </sim_card>
//! <world_schema>{ ...JSON... }</world_schema>
//! <player_state>{ ...JSON... }</player_state>
//! ```
//!
//! ## Memoryless by construction
//!
//! Nothing this prompt produces is ever archived. The four answers are
//! supplied by the caller (the frontend holds them client-side); the
//! generation is a single one-shot call.

/// The four answers the void collected. All fields are plain strings — the
/// frontend sends whatever the user typed (empty string if they skipped a
/// question; the prompt instructs the model to invent a reasonable default
/// for any blank field).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct QuickAnswers {
    /// "Now tell me, what is your character like?"
    pub character: String,
    /// "Now where does this story take place?"
    pub setting: String,
    /// "Is there a plot I should be aware of?"
    pub plot: String,
    /// "Anything else I should know before sending you off?"
    pub extra: String,
}

/// Build the generation system prompt. Given the four answers, emit
/// instructions for the model to produce the three tagged blocks. The voice
/// is deliberately sterile + structural — NOT the GM voice. The card needs
/// to be syntactically correct + narratively rich, not in-character.
pub fn build_generation_system_prompt(answers: &QuickAnswers) -> String {
    let mut out = String::with_capacity(6144);

    out.push_str("<generator_role>\n");
    out.push_str(GENERATOR_ROLE);
    out.push_str("\n</generator_role>\n\n");

    out.push_str("<output_contract>\n");
    out.push_str(OUTPUT_CONTRACT);
    out.push_str("\n</output_contract>\n\n");

    out.push_str("<card_format>\n");
    out.push_str(CARD_FORMAT_SPEC);
    out.push_str("\n</card_format>\n\n");

    out.push_str("<world_schema_format>\n");
    out.push_str(WORLD_SCHEMA_FORMAT);
    out.push_str("\n</world_schema_format>\n\n");

    out.push_str("<player_state_format>\n");
    out.push_str(PLAYER_STATE_FORMAT);
    out.push_str("\n</player_state_format>\n\n");

    out.push_str("<shape_example>\n");
    out.push_str(SHAPE_EXAMPLE);
    out.push_str("\n</shape_example>\n\n");

    // Fold the user's four answers in. Each is labeled so the model can map
    // them to the relevant card/schema fields. Empty answers get an explicit
    // "(blank — invent something fitting)" marker so the model doesn't echo
    // emptiness into the card.
    out.push_str("<user_answers>\n");
    out.push_str(&format!(
        "[CHARACTER]\n{}\n\n",
        block_or_blank(&answers.character)
    ));
    out.push_str(&format!("[SETTING]\n{}\n\n", block_or_blank(&answers.setting)));
    out.push_str(&format!("[PLOT]\n{}\n\n", block_or_blank(&answers.plot)));
    out.push_str(&format!("[EXTRA]\n{}\n\n", block_or_blank(&answers.extra)));
    out.push_str("</user_answers>\n\n");

    out.push_str("<final_reminder>\n");
    out.push_str(
        "Emit the THREE blocks in order: <sim_card>, <world_schema>, \
         <player_state>. Wrap all card prose in CDATA. The opening_scene is \
         the first narrator beat the player reads on spawn — make it a real \
         scene (sensory detail, one NPC beat, a hook), never a summary. The \
         world_schema + player_state seed the world the narrator will run \
         against; populate them with anything the narrator needs as hard \
         ground-truth from turn one (starting inventory, location, key NPC \
         relationships, the player's starting resources).\n",
    );
    out.push_str("</final_reminder>\n");

    out
}

fn block_or_blank(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        "(left blank by the user — invent something fitting the rest of their answers)"
    } else {
        trimmed
    }
}

// ── Prompt text blocks ───────────────────────────────────────────────────

const GENERATOR_ROLE: &str = "\
You are the simulation architect. The user has answered four questions about \
the simulation they want to enter. Your job is to weave those answers into a \
complete, ready-to-run simulation in ONE pass. You produce three blocks: a \
<sim_card> describing the world + opening scene, a <world_schema> seeding the \
initial world state, and a <player_state> seeding the player's starting \
resources. Output ONLY those three blocks — no preamble, no commentary, no \
markdown fence, no explanation before or after.";

const OUTPUT_CONTRACT: &str = "\
Emit EXACTLY three blocks, in this order, each wrapped in its own XML tag:\n\
\n\
<sim_card>\n  ...the full card XML (see <card_format>)...\n\
</sim_card>\n\
<world_schema>\n  ...one JSON object (see <world_schema_format>)...\n\
</world_schema>\n\
<player_state>\n  ...one JSON object (see <player_state_format>)...\n\
</player_state>\n\
\n\
Do NOT wrap the JSON blocks in CDATA — only the <sim_card> prose uses CDATA. \
Do NOT put the <sim_card> inside a JSON string. The three blocks sit \
side-by-side as siblings, each in its own tag. Nothing else is emitted.";

const CARD_FORMAT_SPEC: &str = "\
The <sim_card> XML format (every field is required unless noted):\n\
\n\
<sim_card>\n\
  <metadata>\n\
    <id>lowercase_id_no_spaces</id>\n\
    <type>roleplay</type>\n\
  </metadata>\n\
\n\
  <identity>\n\
    <name>Display Name</name>\n\
    <core_persona><![CDATA[ One paragraph: what kind of simulation this is, \
       the tone, how the world breathes around the player. ]]></core_persona>\n\
  </identity>\n\
\n\
  <scenario>\n\
    <setting><![CDATA[ The world + starting location. 2-4 sentences. Rich \
       sensory + situational detail drawn from the user's SETTING answer. \
       ]]></setting>\n\
    <tone><![CDATA[ 3-5 mood/atmosphere words + a one-line voice guide. \
       ]]></tone>\n\
    <player_name><![CDATA[ The player's name. Use the name from their \
       CHARACTER answer verbatim. If they didn't give one, use the literal \
       string 'User' — do NOT invent a name. ]]></player_name>\n\
    <opening_scene><![CDATA[ The FIRST narrator beat the player reads on \
       entering the simulation. Write it as a proper scene — sensory detail, \
       one NPC beat, a hook for what they might do. End with an implied \
       \"what do you do?\" (don't write those words literally). This is NOT \
       a summary; it's the opening paragraph of a story. ]]></opening_scene>\n\
    <start_npcs><![CDATA[\n\
- npc_id_one\n\
- npc_id_two\n\
- npc_id_three\n\
    ]]></start_npcs>\n\
    <activities><![CDATA[\n\
- conversation\n\
- exploration\n\
- (add activity keywords the simulation enables)\n\
    ]]></activities>\n\
  </scenario>\n\
</sim_card>\n\
\n\
NPC ids are lowercase_with_underscores (the narrator uses them in \
[CHARACTER_TURN:<id>] handoffs). 3-5 start NPCs is typical.";

const WORLD_SCHEMA_FORMAT: &str = "\
The <world_schema> block contains ONE JSON object seeding the world state the \
narrator reads as hard ground-truth from turn one. Shape:\n\
\n\
{\n\
  \"summary\": \"One-paragraph running summary of where the story starts.\",\n\
  \"recent_events\": [\"...\", \"...\"],\n\
  \"entities\": { \"namespace.key\": \"value\", \"...\": \"...\" }\n\
}\n\
\n\
- summary: 1-3 sentences capturing the starting situation (the narrator uses \
  this as the story's spine).\n\
- recent_events: 0-3 salient things that have just happened or are in motion \
  as the player arrives.\n\
- entities: a flat key→value map of hard data the narrator should treat as \
  truth. Namespace keys by convention: \"item.<id>\" for inventory the \
  player starts with, \"loc.<id>\" for notable locations, \
  \"char.<npc_id>.<facet>\" for NPC facets (trust, mood, secret), \
  \"faction.<id>.<facet>\" for faction standing. Only seed what's relevant to \
  the user's scenario; an empty entities object {} is fine for a minimal \
  start.\n\
\n\
Keep the whole block under ~60 tokens of JSON. The schema-delta engine will \
grow it as the story moves; you're just laying the foundation.";

const PLAYER_STATE_FORMAT: &str = "\
The <player_state> block contains ONE JSON object seeding the player's \
starting resources + body. Shape:\n\
\n\
{\n\
  \"wealth\": 0,\n\
  \"reputation\": 0,\n\
  \"stamina\": \"Fresh\",\n\
  \"body\": {}\n\
}\n\
\n\
- wealth: starting coin/gold/credits (0 if not relevant to the scenario).\n\
- reputation: signed integer; negative = infamy, positive = renown. 0 if \
  unknown.\n\
- stamina: one of \"Fresh\" | \"Active\" | \"Winded\" | \"Exhausted\" | \
  \"Depleted\". Default \"Fresh\".\n\
- body: a map of INJURED body parts only. Omit healthy parts entirely (Rust \
  fills them in). Keys are the stable snake_case ids: \"head\", \"torso\", \
  \"left_bicep\", \"left_forearm\", \"left_hand\", \"right_bicep\", \
  \"right_forearm\", \"right_hand\", \"left_thigh\", \"left_calf\", \
  \"left_ankle\", \"left_foot\", \"right_thigh\", \"right_calf\", \
  \"right_ankle\", \"right_foot\". Values are the injury state: \
  \"Yellow\" (Minor) | \"Orange\" (Medium) | \"Red\" (Heavy) | \"Purple\" \
  (Critical) | \"Black\" (Amputated). An empty body {} means fully healthy — \
  the normal case for a fresh start.\n\
\n\
Only seed what the user's answers imply (a war veteran might start with an old \
wound; a noble might start with wealth + reputation). When in doubt, emit \
wealth 0, reputation 0, stamina \"Fresh\", body {}.";

const SHAPE_EXAMPLE: &str = "\
Here is a complete example showing the three-block structure + prose density \
to aim for. Use it as a shape reference — do NOT copy its content:\n\
\n\
<sim_card>\n\
  <metadata>\n\
    <id>frontier_tavern</id>\n\
    <type>roleplay</type>\n\
  </metadata>\n\
\n\
  <identity>\n\
    <name>The Rusty Lantern Tavern</name>\n\
    <core_persona><![CDATA[\n\
A sandbox tavern roleplay. No fixed plot — the world breathes around the \
player's choices. The narrator paints the tavern, its patrons, and the \
wider town as a living place: NPCs have their own errands, the weather \
turns, rumors circulate. Adventure is offered, never forced.\n\
    ]]></core_persona>\n\
  </identity>\n\
\n\
  <scenario>\n\
    <setting><![CDATA[\n\
A weathered, two-story roadside tavern on the trade road into a frontier \
market town. Night has fallen; rain drums on the shutters and lantern light \
throws long shadows across warped oak tables. The common room is half-full: \
a cloaked merchant counting coin, two off-duty guards playing dice, a \
hooded figure alone in the corner.\n\
    ]]></setting>\n\
    <tone><![CDATA[ Atmospheric, grounded, slow-burn. Sensory detail first, \
then character. NPCs are people with their own concerns. ]]></tone>\n\
    <player_name><![CDATA[Kaelen]]></player_name>\n\
    <opening_scene><![CDATA[\n\
The door swings shut behind you, cutting off the cold rain. Warmth and the \
smell of woodsmoke roll over you; the low murmur of conversation dips for a \
moment as a few patrons glance your way, then returns. Water drips from \
your cloak onto the flagstones. Behind the bar, the innkeep looks up from \
the tankard she's polishing and gives you a measured nod. \"Traveler,\" she \
says, loud enough to carry. \"Kitchen's closed, but the ale's warm and the \
fire's free. Sit where you like.\" The dice game by the hearth resumes; the \
hooded figure in the corner doesn't look up at all.\n\
    ]]></opening_scene>\n\
    <start_npcs><![CDATA[\n\
- mara_the_innkeep\n\
- the_hooded_stranger\n\
- bard_corin\n\
- merchant_aldric\n\
    ]]></start_npcs>\n\
    <activities><![CDATA[\n\
- conversation\n\
- exploration\n\
- mystery\n\
    ]]></activities>\n\
  </scenario>\n\
</sim_card>\n\
<world_schema>\n\
{\n\
  \"summary\": \"Night at the Rusty Lantern. Kaelen has just arrived, cold and road-worn. The hooded stranger in the corner is watched by no one.\",\n\
  \"recent_events\": [\"A merchant caravan arrived an hour ago, bringing rumors of bandits on the north road.\"],\n\
  \"entities\": {\n\
    \"loc.current\": \"rusty_lantern_common_room\",\n\
    \"item.travelers_pack\": \"a worn leather pack with a few days' rations\",\n\
    \"item.pouch\": \"a handful of copper\",\n\
    \"char.mara_the_innkeep.disposition\": \"neutral-curious\",\n\
    \"char.the_hooded_stranger.seen_by_player\": \"no\"\n\
  }\n\
}\n\
</world_schema>\n\
<player_state>\n\
{\n\
  \"wealth\": 3,\n\
  \"reputation\": 0,\n\
  \"stamina\": \"Active\",\n\
  \"body\": {}\n\
}\n\
</player_state>";

// ── Legacy compat (removed APIs) ─────────────────────────────────────────
//
// The old `InterviewMessage` struct + `build_interview_system_prompt` +
// `build_finalize_system_prompt` were deleted when Quick Play moved from a
// multi-turn GM chat to a single-shot generation. The GM persona
// (`data/gm.sim`) is no longer referenced from this module.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_prompt_folds_in_all_four_answers() {
        let answers = QuickAnswers {
            character: "A retired spy named Veyra.".into(),
            setting: "A coastal city in the grip of a trade war.".into(),
            plot: "Someone is killing off the old spy network.".into(),
            extra: "She has a daughter who doesn't know what she was.".into(),
        };
        let prompt = build_generation_system_prompt(&answers);
        assert!(prompt.contains("A retired spy named Veyra."));
        assert!(prompt.contains("coastal city"));
        assert!(prompt.contains("killing off the old spy network"));
        assert!(prompt.contains("daughter who doesn't know"));
    }

    #[test]
    fn generation_prompt_marks_blank_answers() {
        let answers = QuickAnswers {
            character: "Kael the wanderer.".into(),
            setting: String::new(),
            plot: String::new(),
            extra: String::new(),
        };
        let prompt = build_generation_system_prompt(&answers);
        // Non-blank answer passes through verbatim.
        assert!(prompt.contains("Kael the wanderer."));
        // Each blank gets the explicit marker so the model doesn't echo
        // emptiness into the card.
        let blank_markers = prompt.matches("left blank by the user").count();
        assert_eq!(blank_markers, 3);
    }

    #[test]
    fn generation_prompt_has_all_three_block_specs() {
        let prompt = build_generation_system_prompt(&QuickAnswers::default());
        assert!(prompt.contains("<card_format>"));
        assert!(prompt.contains("<world_schema_format>"));
        assert!(prompt.contains("<player_state_format>"));
        // The output contract names the three tags in order.
        assert!(prompt.contains("<sim_card>"));
        assert!(prompt.contains("<world_schema>"));
        assert!(prompt.contains("<player_state>"));
    }

    #[test]
    fn generation_prompt_forbids_extra_prose() {
        let prompt = build_generation_system_prompt(&QuickAnswers::default());
        // The contract must forbid markdown fences + preamble.
        assert!(prompt.contains("no preamble"));
        assert!(prompt.contains("no markdown fence"));
        assert!(prompt.contains("Nothing else is emitted"));
    }

    #[test]
    fn generation_prompt_carries_shape_example_with_all_three_blocks() {
        let prompt = build_generation_system_prompt(&QuickAnswers::default());
        // The shape example must show all three blocks so the model has a
        // concrete reference for the structure.
        assert!(prompt.contains("The Rusty Lantern Tavern"));
        // The example's world_schema has a real summary + entities.
        assert!(prompt.contains("\"loc.current\""));
        assert!(prompt.contains("\"char.mara_the_innkeep.disposition\""));
        // The example's player_state has a non-default wealth + stamina.
        assert!(prompt.contains("\"wealth\": 3"));
        assert!(prompt.contains("\"stamina\": \"Active\""));
    }

    #[test]
    fn generation_prompt_lists_all_body_part_ids() {
        let prompt = build_generation_system_prompt(&QuickAnswers::default());
        // The prompt must enumerate every body part id so the model can
        // emit injured parts without guessing the wire format.
        for id in [
            "head", "torso", "left_bicep", "left_forearm", "left_hand",
            "right_bicep", "right_forearm", "right_hand", "left_thigh",
            "left_calf", "left_ankle", "left_foot", "right_thigh",
            "right_calf", "right_ankle", "right_foot",
        ] {
            assert!(prompt.contains(id), "prompt missing body part id: {id}");
        }
    }

    #[test]
    fn quick_answers_default_all_empty() {
        let a = QuickAnswers::default();
        assert!(a.character.is_empty());
        assert!(a.setting.is_empty());
        assert!(a.plot.is_empty());
        assert!(a.extra.is_empty());
    }

    #[test]
    fn quick_answers_serializes_roundtrip() {
        let a = QuickAnswers {
            character: "c".into(),
            setting: "s".into(),
            plot: "p".into(),
            extra: "e".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: QuickAnswers = serde_json::from_str(&json).unwrap();
        assert_eq!(back.character, "c");
        assert_eq!(back.setting, "s");
        assert_eq!(back.plot, "p");
        assert_eq!(back.extra, "e");
    }
}
