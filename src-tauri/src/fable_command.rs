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
    if query_starters.iter().any(|s| trimmed.starts_with(*s)) {
        let focus = extract_focus(trimmed);
        return FableCommand::QueryWorldState(focus);
    }

    // "make it X", "set X to Y", "change X to Y", "give me X", "remove X",
    // "teleport/travel to X", "fast-travel to X".
    let mutation_starters = [
        "make it ", "make the ", "make ",
        "set ", "change ", "turn ", "switch ",
        "give me ", "add ",
        "remove ", "delete ", "drop ",
        "teleport ", "travel to ", "fast-travel to ", "fast travel to ",
        "spawn ",
    ];
    if mutation_starters.iter().any(|s| trimmed.starts_with(*s)) {
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
    if keyword_signals.iter().any(|kw| trimmed.contains(*kw)) {
        // Distinguish query vs mutation by verb presence.
        let is_mutation = contains_mutation_verb(trimmed);
        if is_mutation {
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
    // (P2 fix) Trailing conversation stopwords are dropped first: "what's
    // the weather like?" used to yield focus "like" (no entity match -> the
    // whole pretty-printed world JSON dumped into the chat bubble).
    const STOPWORDS: &[&str] = &[
        "like", "please", "now", "today", "here", "there", "again", "about",
        "exactly", "currently", "right",
    ];
    let tokens: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    let mut idx = tokens.len();
    while idx > 0 && STOPWORDS.contains(&tokens[idx - 1].to_lowercase().as_str()) {
        idx -= 1;
    }
    tokens.get(idx.saturating_sub(1))
        .map(|w| w.to_string())
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
    // DISABLED 2026-08-09 (`THINKING_ENABLED`) — see settings.rs.
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Current world state:\n");
    out.push_str(current_state_json);
    // (2026-08-16 bug 12) The player request runs through the same cap as
    // the delta exchange — a pasted block rode in unbounded and could push
    // the 2048-token translation prompt into the middle-drop.
    out.push_str("\n\nPlayer's request to Wupi:\n");
    out.push_str(&crate::schema_engine::cap_exchange_chars(player_request));
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
                crate::schema_engine::cap_attempt_error_chars(&attempt.errors)
            ));
        }
    }
    out.push_str("\n\nEmit ONLY the JSON delta object (changed keys only). If the request is not a state mutation, emit {}.\n");
    out.push_str("<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// Render the BOOTSTRAP prompt (2026-08-10): derive the starting clock + weather
/// + opening location from a card's `.intro` text (the one-shot first narrator
/// beat) when no `<start>` block seeded them. This is the cold-start anchor gap
/// fix for Creator-authored cards that ship no `<locations>`/`<start>`: the
/// tracker renders no `clock:`/`weather:` line while those are dormant → it has
/// nothing to maintain → `[TIME]`/`[WEATHER]` never fire. Seeding the anchors
/// from the intro gives the tracker its baseline from turn 1.
///
/// Same `<|turn>` framing + `THINKING_ENABLED` gate as the other schema-engine
/// prompts. Runs on the schema engine's isolated context at
/// `enter_fable_session` time (NOT inside `fable_send` — the schema engine's
/// VRAM lease conflicts with the Fable lease held mid-turn). The reply is
/// parsed by `schema::BootstrapAnchors::from_model_output` (NOT `SchemaDelta` —
/// `apply_delta` is test-pinned to never touch clock/weather). Generic
/// angle-bracket templates only (anti-pattern #4: no concrete copyable
/// examples).
///
/// `intro_text` is the full `.intro` (may be empty when a card has no intro —
/// the caller then relies on sensible defaults instead). `setting`/`tone`/
/// `player_name` carry the card's authored identity for extra context.
pub fn render_bootstrap_prompt(
    intro_text: &str,
    setting: Option<&str>,
    tone: Option<&str>,
    player_name: Option<&str>,
) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str("<|turn>system\n");
    out.push_str(BOOTSTRAP_INSTRUCTION);
    // Same thinking gate as the other schema passes (§3A — currently OFF).
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Opening scene (the first narrator beat):\n");
    out.push_str(intro_text.trim());
    if let Some(s) = setting.filter(|s| !s.trim().is_empty()) {
        out.push_str("\n\nWorld setting:\n");
        out.push_str(s.trim());
    }
    if let Some(t) = tone.filter(|t| !t.trim().is_empty()) {
        out.push_str("\n\nTone:\n");
        out.push_str(t.trim());
    }
    if let Some(p) = player_name.filter(|p| !p.trim().is_empty()) {
        out.push_str("\n\nPlayer character: ");
        out.push_str(p.trim());
    }
    out.push_str(
        "\n\nEmit ONLY the JSON object with the anchors the opening scene establishes. \
         Omit any field the scene does not clearly establish — do not invent.\n",
    );
    out.push_str("<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

const BOOTSTRAP_INSTRUCTION: &str = "\
You are deriving the starting world-state anchors for a roleplay scene from its\n\
opening narration. Read the opening scene + extract the anchors the scene\n\
establishes, emitting each as a JSON field (omit any the scene does not set):

{\n\
  \"time\": \"<Day N, HH:MM in 24h>\" — the in-world time the scene opens at.\n\
     Infer from cues like time of day, lighting, or explicit time words. Use the\n\
     form \"Day N, HH:MM\" (Day is 1-indexed; 24h clock). If the scene spans an\n\
     undefined moment with no time signal, omit.\n\
  \"weather\": \"<one short phrase>\" — the sky / atmospheric condition.\n\
     e.g. fog, heavy rain, clear night, snowfall. If the scene is indoors with\n\
     no outside cue, omit.\n\
  \"location_id\": \"<bare slug>\" — a snake_case id for the opening location.\n\
     Derive from the place name (e.g. \"crooked_lantern\"). Pair with\n\
     location_name. If the scene has no specific place, omit BOTH location\n\
     fields.\n\
  \"location_name\": \"<diegetic name>\" — the place's full prose name\n\
     (e.g. \"The Crooked Lantern\").\n\
  \"arcana\": \"<resource name>\" — the arcane resource the opening scene\n\
     names (mana, biotics, rage, ki). One short word. Omit when the fiction\n\
     has no such resource.\n\
}\n\
\n\
Rules:\n\
 - Emit raw JSON only. No markdown fences, no <|channel> protocol markers.\n\
 - Omit a field rather than guessing. A partial extraction is correct; the\n\
   caller fills gaps with sensible defaults.\n\
 - \"time\" MUST be the \"Day N, HH:MM\" form so it parses. Never a bare word\n\
   like \"night\" or \"morning\" — convert to the nearest HH:MM.\n\
 - Slugs are lowercase snake_case (letters/digits/underscores only).";

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
            surface: crate::schema_engine::DeltaSurface::Chat,
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
