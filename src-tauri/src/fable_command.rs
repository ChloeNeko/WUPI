//! Wupi-as-game-manager intent router (Games app Seam E: the pivot's
//! headline feature).
//!
//! When a game is active (`FableEngine.is_some()`), Wupi's chat context
//! gains a second capability: she can read and mutate the active game's
//! scoped `<world_state>` via natural language. This module classifies the
//! player's message to Wupi and decides whether it's:
//!
//! - `MutateWorldState(delta)`: apply a state mutation ("make it stormy"
//!   → `{entities: {weather: "stormy"}}`).
//! - `QueryWorldState(what)`: return part of the state for Wupi to narrate
//!   ("what's the weather?").
//! - `NotACommand`: fall through to normal Wupi-assistant chat.
//!
//! # MVP compromise
//!
//! Intent detection is **heuristic** (keyword matching) for the MVP. This
//! WILL misroute edge cases. Phase 2 replaces it with an LLM-judge pre-pass
//! or a small classifier. Documented as a known limitation, not a hidden
//! bug: see the inline comments for what each branch covers and what it
//! misses.

use crate::schema::SchemaDelta;

/// Wupi's classification of a player message directed at her (not the
/// narrator) while a game is running.
#[derive(Debug, Clone)]
pub enum FableCommand {
    /// The player wants to change the game world. `delta` is the
    /// `SchemaDelta` to apply to the card's scoped schema.
    MutateWorldState(SchemaDelta),
    /// The player is asking about the game state. `what` is the focus
    /// (e.g. "weather", "inventory", "npcs"). Wupi will narrate the answer
    /// in her own voice: no mutation.
    QueryWorldState(String),
    /// Not a game-management request. Fall through to normal Wupi chat.
    NotACommand,
}

/// Classify a message to Wupi (while a game is active) into a FableCommand.
///
/// Returns `NotACommand` quickly for clearly non-management messages so
/// the chat path doesn't pay the heuristic cost in the common case where
/// the player is just chatting with Wupi.
///
/// The heuristic is **conservative toward `NotACommand`**: false-positives
/// (treating normal chat as a command) are worse than false-negatives
/// (missing a command: the player can rephrase). The bar to route to a
/// command is HIGH.
pub fn classify(text: &str) -> FableCommand {
    let lower = text.to_lowercase();
    let trimmed = lower.trim();

    if trimmed.is_empty() {
        return FableCommand::NotACommand;
    }

    // "what's the X", "show me X", "how is X", "status of X".
    let query_starters = [
        "what's ", "what is ", "whats ", "show me ", "show ",
        "how is ", "how's ", "status of ", "tell me about ",
        "list my ", "what do i have", "what am i carrying",
        "where am i", "who is here", "who's here",
    ];
    if query_starters.iter().any(|s| trimmed.starts_with(s)) {
        return FableCommand::QueryWorldState(extract_focus(trimmed));
    }

    // "make it X", "set X to Y", "change X to Y", "give me X", "remove X",
    // "teleport/travel to X", "fast-travel to X".
    let mutation_starters = [
        "make it ", "make the ", "make ",
        "set ", "change ", "turn ", "switch ",
        "give me ", "give alex ", "add ",
        "remove ", "delete ", "drop ",
        "teleport ", "travel to ", "fast-travel to ", "fast travel to ",
        "spawn ",
    ];
    if mutation_starters.iter().any(|s| trimmed.starts_with(s)) {
        // For the MVP we return a PLACEHOLDER delta: the actual LLM
        // translation ("make it stormy" → {weather: stormy}) happens in
        // `fable_command::translate_to_delta`, called from `chat_send` after
        // classification. Returning an empty delta here keeps the type
        // signature honest; the caller will populate it.
        return FableCommand::MutateWorldState(SchemaDelta::default());
    }

    // Some management intents don't start with a clear verb but contain
    // strong domain keywords. Match a few high-value ones.
    let keyword_signals = ["inventory", "weather", "time of day", "fast travel"];
    if keyword_signals.iter().any(|kw| trimmed.contains(kw)) {
        // Distinguish query vs mutation by verb presence.
        if contains_mutation_verb(trimmed) {
            return FableCommand::MutateWorldState(SchemaDelta::default());
        }
        return FableCommand::QueryWorldState(extract_focus(trimmed));
    }

    FableCommand::NotACommand
}

/// Extract the focus noun from a query ("what's the weather" → "weather").
/// For MVP this is a simple last-word grab; Phase 2 will use the LLM.
fn extract_focus(text: &str) -> String {
    // Take the last whitespace-delimited token, stripped of punctuation.
    text.split_whitespace()
        .last()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| "state".to_string())
}

/// Mutation verbs that flip a keyword match from query to mutation.
fn contains_mutation_verb(text: &str) -> bool {
    let verbs = ["make", "set", "change", "turn", "give", "add", "remove", "spawn"];
    text.split_whitespace()
        .any(|w| verbs.contains(&w))
}

/// Translate a player's natural-language mutation request into a
/// `SchemaDelta` by asking the LLM (Wupi's chat context, briefly). This is
/// called from `chat_send` AFTER `classify` returns `MutateWorldState`.
///
/// The LLM is given the request + current world_state JSON and asked to
/// emit ONLY the changed keys as a delta. Same prompt structure as the
/// schema engine's delta pass, but driven by an explicit player command
/// rather than an automatic per-turn summarization.
///
/// **MVP note:** this function returns the prompt text; the actual LLM call
/// + parse happens in the caller (which has access to the FableEngine/
/// SchemaEngine). Keeping the prompt-construction pure lets us unit-test it
/// without a model.
pub fn render_translation_prompt(
    player_request: &str,
    current_state_json: &str,
    deferred_attempts: &[crate::schema_engine::FailedAttempt],
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("<|turn>system\n");
    out.push_str(TRANSLATION_INSTRUCTION);
    // Always-on thinking: inject `<|think|>` so the translation pass always
    // reasons before emitting JSON. Output flows through schema_engine's
    // generate_with_repair, which strips the thought before parsing (see
    // schema_engine.rs generate_with_repair + extract_reply_channel).
    out.push_str("<|think|>");
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Current world state:\n");
    out.push_str(current_state_json);
    out.push_str("\n\nPlayer's request to Wupi:\n");
    out.push_str(player_request);
    // Deferred re-attempt context (fail-proof contract §5 layer 3). When the
    // player's previous request failed all 3 passes, fold its trigger + errors
    // in here so the model has a fresh shot with the new request as anchor.
    // The carrier carries the *trigger*, not the broken raw output: the new
    // request + the prior errors are the useful signal.
    if !deferred_attempts.is_empty() {
        out.push_str("\n\n[Previously deferred state changes — re-attempt with the above request as the primary context:]\n");
        for (i, attempt) in deferred_attempts.iter().enumerate() {
            let trigger = attempt
                .trigger
                .as_deref()
                .or_else(|| attempt.exchange.as_ref().map(|(u, _)| u.as_str()))
                .unwrap_or("(no trigger recorded)");
            out.push_str(&format!(
                "  {}. prior request: {:?}\n     prior errors: {}\n",
                i + 1,
                trigger.chars().take(200).collect::<String>(),
                attempt.errors
            ));
        }
    }
    out.push_str("\n\nEmit ONLY the JSON delta object (changed keys only). If the request is not a state mutation, emit {}.\n");
    out.push_str("<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// Render the Quick Play SEED prompt (2026-08-05): fold three free-text
/// descriptions (player / scenario / desire) into an initial `SchemaDelta`
/// against an EMPTY world. This is a sibling of `render_translation_prompt`
/// but framed for world-creation, not mid-game mutation — the translation
/// instruction tells the model to emit `{}` for anything that "cannot be
/// expressed as a state change," which would no-op most free-form prose
/// ("describe what you desire" is rarely a discrete mutation). The seed
/// framing instead treats the three descriptions as the genesis of the
/// world and asks for a populated delta.
///
/// Same rigid JSON-delta contract + flat-string entity rule as translation.
/// Generic angle-bracket templates only (anti-pattern #4: no concrete
/// copyable worked examples). `current_state_json` is the empty `{}` for a
/// fresh Quick Play run, but is threaded through for symmetry + so a future
/// re-seed over an existing world diffs cleanly.
pub fn render_quick_play_seed_prompt(
    player_desc: &str,
    scenario_desc: &str,
    desire_desc: &str,
    current_state_json: &str,
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("<|turn>system\n");
    out.push_str(SEED_INSTRUCTION);
    // Always-on thinking (translation prompt above documents the pipeline).
    out.push_str("<|think|>");
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Current world state (empty — you are seeding it):\n");
    out.push_str(current_state_json);
    out.push_str("\n\nPlayer description:\n");
    out.push_str(player_desc);
    out.push_str("\n\nScenario description:\n");
    out.push_str(scenario_desc);
    out.push_str("\n\nWhat the player desires from this story:\n");
    out.push_str(desire_desc);
    out.push_str("\n\nEmit ONLY the JSON delta object that seeds this world from the above. Flatten every concept into descriptive snake_case entity keys. If a description gives nothing usable, omit it rather than emitting empty strings.\n");
    out.push_str("<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

const SEED_INSTRUCTION: &str = "\
You are seeding an EMPTY roleplay world from three free-text descriptions.
The world_state is a JSON object with three top-level keys: summary (string),
recent_events (array of strings), entities (object of key -> flat string).

 Emit the SEED as a JSON delta with this EXACT shape:
{
  \"summary\": \"...\" (a one-to-two sentence premise drawn from the scenario + desire),
  \"recent_events\": [\"...\"] (optional, 0-2 opening circumstances),
  \"entities\": {\"key\": \"value\"} (the bulk of the seed — see below)
}

 CRITICAL — entity value type rule:
   Every entity value MUST be a single flat STRING. Numbers MUST be quoted
   (\"50\", not 50). NEVER use nested objects, arrays, or numbers.
   WRONG:  {\"entities\": {\"player\": {\"name\": \"Kael\"}}}        // nested
   RIGHT:  {\"entities\": {\"player_name\": \"Kael\"}}               // flat string

 Use descriptive snake_case keys. Flatten structured concepts into keys like:
   <character_trait>, <npc_name_mood>, <location_feature>, <inventory_item>,
   <relationship_player_npc>, <world_rule>, <player_goal>, <tone_flavor>.
   Player identity from the player description lands as player_* keys
   (player_name, player_appearance, player_archetype, ...). Scenario facts
   land as world_* / location_* / faction_* keys. The desire becomes a
   player_goal / world_arc key.

 Emit the FULL seed (this is a fresh empty world, so every key is a change).
 Do NOT wrap the JSON in markdown fences. Do NOT include <|channel> protocol
 markers — emit raw JSON only.";

const TRANSLATION_INSTRUCTION: &str = "\
You are translating a player's natural-language request into a state delta
for the roleplay game's world_state. The world_state is a JSON object with
three top-level keys: summary (string), recent_events (array of strings),
entities (object of key -> flat string).

 Emit ONLY the changed keys as a JSON delta with this EXACT shape:
{
  \"summary\": \"...\" (optional, only if the arc shifted),
  \"recent_events\": [\"...\"] (optional, append-only),
  \"entities\": {\"key\": \"value\" | null} (optional; null deletes a key)
}

 CRITICAL — entity value type rule:
   Every entity value MUST be a single flat STRING, or null to delete.
   Numbers MUST be quoted as strings (\"50\", not 50).
   NEVER use nested objects, arrays, or numbers as entity values.
   WRONG:  {\"entities\": {\"player_state\": {\"wealth\": 50}}}      // nested object
   WRONG:  {\"entities\": {\"wealth\": 50}}                          // bare number
   RIGHT:  {\"entities\": {\"wealth\": \"50\"}}                       // flat string
   RIGHT:  {\"entities\": {\"weather\": \"stormy\"}}                  // flat string

 Use snake_case keys. Flatten structured concepts into descriptive keys
 (e.g. \"innkeeper_mood\", \"player_gold\", \"time_of_day\") rather than
 nesting sub-objects.

 Examples (player request -> your emitted JSON):
   \"make it stormy with heavy rain\"
     -> {\"entities\": {\"weather\": \"stormy_heavy_rain\"}}
   \"give me a steel sword\"
     -> {\"entities\": {\"inventory_steel_sword\": \"held\"}}
   \"set my gold to 50 coins\"
     -> {\"entities\": {\"player_gold\": \"50\"}}
   \"make the innkeeper Mara friendly toward me\"
     -> {\"entities\": {\"innkeeper_mara_mood\": \"friendly\"}}
   \"change the time of day to midnight\"
     -> {\"entities\": {\"time_of_day\": \"midnight\"}}
   \"what's the weather like?\"
     -> {}                                  // question, not a mutation

 Do NOT re-emit unchanged keys. Do NOT wrap the JSON in markdown fences.
 Do NOT include the <|channel> protocol markers — emit raw JSON only.
 If the request cannot be expressed as a state change, emit an empty
 object: {}";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_message_is_not_a_command() {
        assert!(matches!(classify(""), FableCommand::NotACommand));
        assert!(matches!(classify("   "), FableCommand::NotACommand));
    }

    #[test]
    fn normal_chat_is_not_a_command() {
        assert!(matches!(classify("hey wupi how are you"), FableCommand::NotACommand));
        assert!(matches!(classify("tell me a joke"), FableCommand::NotACommand));
        assert!(matches!(classify("nya~"), FableCommand::NotACommand));
    }

    #[test]
    fn query_starters_route_to_query() {
        match classify("what's the weather") {
            FableCommand::QueryWorldState(focus) => assert_eq!(focus, "weather"),
            _ => panic!("expected QueryWorldState"),
        }
        match classify("show me my inventory") {
            FableCommand::QueryWorldState(_) => {}
            _ => panic!("expected QueryWorldState"),
        }
        match classify("where am i") {
            FableCommand::QueryWorldState(_) => {}
            _ => panic!("expected QueryWorldState"),
        }
    }

    #[test]
    fn mutation_starters_route_to_mutate() {
        assert!(matches!(
            classify("make it stormy"),
            FableCommand::MutateWorldState(_)
        ));
        assert!(matches!(
            classify("give me a sword"),
            FableCommand::MutateWorldState(_)
        ));
        assert!(matches!(
            classify("travel to the dungeon"),
            FableCommand::MutateWorldState(_)
        ));
        assert!(matches!(
            classify("set weather to rain"),
            FableCommand::MutateWorldState(_)
        ));
    }

    #[test]
    fn keyword_without_verb_routes_to_query() {
        // "the weather is nice": mentions weather but no mutation verb.
        match classify("the weather is nice today") {
            FableCommand::QueryWorldState(_) => {}
            other => panic!("expected QueryWorldState, got {other:?}"),
        }
    }

    #[test]
    fn keyword_with_verb_routes_to_mutate() {
        // "change the weather": keyword + mutation verb.
        assert!(matches!(
            classify("change the weather"),
            FableCommand::MutateWorldState(_)
        ));
    }

    #[test]
    fn render_translation_prompt_contains_request_and_state() {
        let prompt = render_translation_prompt(
            "make it stormy",
            "{\"entities\":{\"weather\":\"clear\"}}",
            &[], // no deferred attempts in the common case
        );
        assert!(prompt.contains("make it stormy"));
        assert!(prompt.contains("\"weather\":\"clear\""));
        assert!(prompt.contains("<|turn>system"));
        assert!(prompt.contains("<|turn>model"));
    }

    // Regression guard for the 2026-07-26 Phase B bug: GLM-5.2 was emitting
    // nested-object entity values ({"entities":{"player_state":{"wealth":50}}})
    // because the prompt didn't explicitly forbid them. These assertions pin
    // the flat-string rule + the worked examples so a future prompt edit can't
    // silently drop them.
    #[test]
    fn translation_prompt_pins_flat_string_entity_rule() {
        let prompt = render_translation_prompt("set gold to 50", "{}", &[]);
        // The explicit prohibition of nested/bare-number entity values.
        assert!(
            prompt.contains("NEVER use nested objects"),
            "prompt must forbid nested entity values"
        );
        // At least one worked example showing a number quoted as a string.
        assert!(
            prompt.contains("\"player_gold\": \"50\""),
            "prompt must show numbers quoted as flat strings"
        );
        // The WRONG/RIGHT contrast that teaches the rule by negation.
        assert!(prompt.contains("WRONG"));
        assert!(prompt.contains("RIGHT"));
    }

    #[test]
    fn render_translation_prompt_folds_deferred_attempts() {
        // Fail-proof contract layer 3: prior translation failures must
        // surface in the next request's prompt.
        let deferred = vec![crate::schema_engine::FailedAttempt {
            exchange: None,
            trigger: Some("prior failed request".to_string()),
            errors: "pass 1 parse: ... | pass 2 validation: ...".to_string(),
            passes_used: 3,
        }];
        let prompt = render_translation_prompt(
            "new request",
            "{}",
            &deferred,
        );
        assert!(prompt.contains("Previously deferred"));
        assert!(prompt.contains("prior failed request"));
        assert!(prompt.contains("pass 1 parse"));
    }

    #[test]
    fn extract_focus_strips_punctuation() {
        assert_eq!(extract_focus("what's the weather?"), "weather");
        assert_eq!(extract_focus("show me my inventory."), "inventory");
    }
}

// Top-level `Display` impl (Phase E cleanup, 2026-07-18). Was previously
// inside `#[cfg(test)]` to silence unused-Debug-format warnings on `_` match
// arms in tests. Promoted to the module level: it's useful for log lines in
// the route helpers too (`tracing::info!(?cmd, ...)` falls back to Display
// when Debug isn't used). No behavior change.
impl std::fmt::Display for FableCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FableCommand::MutateWorldState(_) => write!(f, "MutateWorldState"),
            FableCommand::QueryWorldState(focus) => write!(f, "QueryWorldState({focus})"),
            FableCommand::NotACommand => write!(f, "NotACommand"),
        }
    }
}
