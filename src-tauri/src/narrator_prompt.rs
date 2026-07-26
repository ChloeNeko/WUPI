//! Narrator system-prompt builder (Games app Seam 3).
//!
//! The narrator is the **Simulation Narrative Engine** for a roleplay card:
//! the invisible authoring intelligence that animates the world. It portrays
//! environment, NPCs, and consequence — but is forbidden from speaking for
//! the player or generating the player's actions. The agency split is borrowed
//! from UIE's `omniscientEngine.js:8-28` ("narrator agency split"), adapted
//! to WUPI's strict-XML-prompt aesthetic (Prime Directive §1B.3) and the
//! "infinite roleplay immersion" thesis.
//!
//! # Bracket-command protocol
//!
//! The narrator emits bracket commands alongside its prose so the engine
//! can route structured events to the UI deterministically:
//!
//! - `[CHARACTER_TURN:npc_id] ... [CHARACTER_TURN:end]`: wraps an NPC's
//!   spoken line. The UI renders this with the npc_id as the speaker label,
//!   visually distinct from narrator prose. MVP: the narrator writes the
//!   line (no separate NPC sidecar yet). Phase 2 will let a per-NPC model
//!   own the voice; the wrapper stays the same.
//! - `[OBJECT id=iron_chest state=open]`: a tracked world detail changed
//!   state. This is the load-bearing "memory" surface — it's how the
//!   narrator tells the engine "the diamond necklace is now in the
//!   player's inventory" or "the barkeeper's trust has fallen." Routed
//!   through the schema engine so it survives across turns.
//! - `[FX rain]`, `[FX letterbox]`, `[FX shake-heavy]`: a scene-FX class
//!   should activate. Names match UIE's `sceneEffects.js` vocabulary so
//!   the eventual UI port is direct.
//!
//! The parser lives in `bracket_parser.rs::BracketCommand`.

use crate::sim_card::SimCard;

/// Build the narrator system prompt for a roleplay card. The prompt is
/// injected as the `<|turn>system` block of the Gemma4 chat format. It tells
/// the model:
///   1. It is the Simulation Narrative Engine (not a character, not a player,
///      not an assistant — the invisible authoring intelligence).
///   2. The setting + tone + present NPCs + activities in play (from the
///      card's `<scenario>` block).
///   3. The player contract: NEVER speak for the player. Dynamic protagonist
///      name (was hardcoded "Alex"; now sourced from `card.protagonist_name`).
///   4. The bracket-command protocol (see module doc).
///   5. The current `<world_state>` (passed in by the caller, scoped to the
///      card's schema).
///   6. `<active_reality>` LAST (recency anchor; defeats cross-card KV bleed).
pub fn build_narrator_system_prompt(
    card: &SimCard,
    world_state: Option<&str>,
) -> String {
    // Resolve how the player is addressed across the prompt. The protagonist
    // name is now dynamic (the 2026-07-18 "Alex hallucination" was caused by
    // a hardcoded name in the constant; this structuring makes that class of
    // bug structurally impossible). When a card declares no protagonist, fall
    // back to second-person "you" so the prompt never invents a name.
    let protagonist_display = player_display_name(card);   // "Alex" / "the traveler"
    let protagonist_address = player_address(card);         // "Alex" / "you"

    let mut out = String::with_capacity(3072);

    // 1. Identity + role contract.
    out.push_str("<narrator_role>\n");
    out.push_str(&narrator_core(&protagonist_display));
    out.push_str("\n</narrator_role>\n\n");

    // 2. Scenario context.
    out.push_str("<scenario>\n");
    if let Some(setting) = card.setting.as_deref() {
        out.push_str("setting: ");
        out.push_str(setting.trim());
        out.push_str("\n\n");
    }
    if let Some(tone) = card.tone.as_deref() {
        out.push_str("tone: ");
        out.push_str(tone.trim());
        out.push_str("\n\n");
    }
    // Protagonist line: lets the model address the player by name when an NPC
    // speaks TO them, while narrator prose uses second-person "you."
    out.push_str("protagonist: ");
    out.push_str(&protagonist_display);
    out.push_str("\n\n");
    if !card.start_npc_ids.is_empty() {
        out.push_str("present_npcs: ");
        out.push_str(&card.start_npc_ids.join(", "));
        out.push_str("\n");
        out.push_str(
            "  (Each NPC above may speak. Wrap their dialogue with \
             [CHARACTER_TURN:<npc_id>] ... [CHARACTER_TURN:end]. \
             The id must match one of these exactly.)\n",
        );
    }
    if !card.declared_activities.is_empty() {
        out.push_str("\nactivities_in_play: ");
        out.push_str(&card.declared_activities.join(", "));
        out.push_str("\n");
    }
    out.push_str("</scenario>\n\n");

    // 3. Player contract: the load-bearing anti-self-insert guardrail.
    out.push_str("<player>\n");
    out.push_str(&player_contract(&protagonist_address));
    out.push_str("\n</player>\n\n");

    // 4. Bracket-command protocol.
    out.push_str("<bracket_commands>\n");
    out.push_str(BRACKET_PROTOCOL);
    out.push_str("\n</bracket_commands>\n\n");

    // 5. World state (card-scoped schema snapshot: what's true right now).
    if let Some(state) = world_state {
        if !state.trim().is_empty() {
            out.push_str("<world_state>\n");
            out.push_str(state.trim());
            out.push_str("\n</world_state>\n\n");
        }
    }

    // 6. DELIBERATELY last: closest to the user input — the loudest signal
    //    the model sees when generating. The FableEngine's KV cache may hold
    //    residual state from a PRIOR card (the 2026-07-18 "Alex
    //    hallucination": the cyberpunk narrator used the dungeon
    //    protagonist's name). Gemma 4, like all transformer LLMs, weights
    //    recent tokens heavily; putting the explicit card-identity
    //    reinforcement at the tail overrides those lingering vibes. Same
    //    principle that made §2O's persona injection work: explicit,
    //    structured, recently-positioned context wins over implicit residual
    //    state.
    out.push_str("<active_reality>\n");
    out.push_str(&format!(
        "You are narrating {}, NOT any other scenario. ",
        card.name.trim(),
    ));
    if let Some(name) = card.protagonist_name.as_deref() {
        out.push_str(&format!(
            "The protagonist is {name}: use this name exclusively when an \
             NPC addresses them; never use a different protagonist's name. "
        ));
    } else {
        out.push_str(
            "Refer to the protagonist as \"you\" (second person); never \
             invent or import a protagonist name. ",
        );
    }
    if let Some(setting) = card.setting.as_deref() {
        // Brief recap (full setting already lives in <scenario> above). The
        // recap here is the recency-reinforcement, not the source of truth.
        let brief: String = setting.trim().chars().take(160).collect();
        out.push_str(&format!("Setting recap: {brief}… "));
    }
    out.push_str(
        "Do NOT reference characters, locations, items, or elements from \
         any other scenario: only what belongs to this one.\n",
    );
    out.push_str("</active_reality>\n\n");

    out
}

/// The display name for the protagonist in scenario/prose context.
/// Returns the card's `protagonist_name` when declared, else a generic
/// placeholder that signals "unnamed" without inventing a name.
fn player_display_name(card: &SimCard) -> String {
    match card.protagonist_name.as_deref() {
        Some(n) if !n.trim().is_empty() => n.trim().to_owned(),
        _ => "the protagonist (unnamed)".to_owned(),
    }
}

/// The address form for the protagonist in the player-contract block.
/// Returns the name when declared (so the contract reads "NEVER speak for
/// Alex"), or "you" when no name exists (so the contract reads "NEVER speak
/// for you / decide what you do").
fn player_address(card: &SimCard) -> String {
    match card.protagonist_name.as_deref() {
        Some(n) if !n.trim().is_empty() => n.trim().to_owned(),
        _ => "you".to_owned(),
    }
}

/// Build the `<narrator_role>` body. Takes the protagonist display name so
/// the role contract can reference the player without a hardcoded name.
fn narrator_core(protagonist: &str) -> String {
    format!(
        "\
You are the SIMULATION NARRATIVE ENGINE for this scenario. You are not a \
character, not a player, not an assistant — you are the invisible \
authoring intelligence that animates this world.

YOUR PURPOSE
Give the player the illusion of infinite roleplay: a living world that \
responds to their every action with coherence, consequence, and surprise.

WHAT YOU DO
- Portray the WORLD: environment, weather, sounds, smells, the small \
details that make a scene feel lived-in.
- Portray NPCs: their observable behavior, reactions, and spoken dialogue. \
Wrap each spoken NPC line in [CHARACTER_TURN:npc_id] ... [CHARACTER_TURN:end].
- Drive the scene forward with tension, momentum, and meaningful choices.
- Track world state: emit [OBJECT id=<stable_id> state=<new_state>] whenever \
a meaningful detail is introduced or changes — a gift received, a door \
locked, an NPC's trust won or lost, a clue discovered. This is how the \
world REMEMBERS across turns. Lean toward emitting these: the engine \
deduplicates, so overspecifying is cheap and underspecifying loses detail.
- End your turn the moment {protagonist} needs to act. Leave the next move \
open.

WHAT YOU NEVER DO
- Never speak for {protagonist}. Never decide what {protagonist} does, \
says, thinks, or feels.
- Never write {protagonist}'s dialogue, choices, or internal monologue.
- Never address the player out-of-character, break the fourth wall, or \
reference game mechanics, AI, prompts, or that this is a simulation.
- Never narrate in first person as any character. You are the narrator, \
not a participant.

NARRATIVE DISCIPLINE
- Tight prose: 2-5 sentences per beat unless the scene demands more.
- Sensory detail over spectacle. Show, don't summarize.
- Vary sentence rhythm: short for tension, longer for description.
- Each beat should leave the next move to the player.
- NPCs are people, not props. They have their own goals, moods, secrets, \
and histories that do not revolve around {protagonist}.

THE LIVING WORLD
The setting is alive. Background NPCs have errands, gossip, grievances, \
and relationships of their own. A passing comment may be unrelated to the \
main thread. This autonomy is what makes the world feel real — preserve \
it. When {protagonist} is absent, the world keeps moving.

PLAYER STATE IS HARD FACT
If the `<world_state>` block carries a `player_state:` section, those \
injuries, amputations, fatigue, and limitations are ABSOLUTE TRUTH — \
Rust computes them off-screen and the numbers are not your concern. \
Honor them exactly: a character with a Heavy Injury to the right arm \
cannot swing a sword effectively; a character who is Exhausted moves \
slowly and clumsily; an amputated limb is gone and cannot be used. \
Never have {protagonist} perform beyond those limits, never ignore or \
hand-wave an injury away, never spontaneously heal. Weave the \
limitation into the prose naturally — show its effect on action and \
dialogue, do not lecture about it."
    )
}

/// Build the `<player>` block — the player contract. Uses the address form
/// ("Alex" or "you") so the contract is grammatical in both branches.
fn player_contract(protagonist: &str) -> String {
    let is_you = protagonist == "you";
    // Build the "never speak for" line in the right grammatical person.
    let never_line = if is_you {
        "Never decide what you do, say, think, or feel".to_owned()
    } else {
        format!("Never decide what {protagonist} does, says, thinks, or feels")
    };
    let person = if is_you { "second" } else { "third" };
    format!(
        "The player controls {protagonist}. This is the player's one and only \
channel into the world.\n\n\
- Narrate the world {person}-person. The narrator's camera follows \
{protagonist}; NPCs address {protagonist} by name when speaking to them.\n\
- {never_line} — those belong to the player alone.\n\
- When the player's input implies an action, narrate its diegetic \
consequences (how the world responds, what changes, who notices), not the \
action itself. Trust the player's stated intent; do not reinterpret it.\n\
- If the player attempts something impossible or rule-breaking, let the \
world push back naturally (an NPC refuses, a door holds, a guard frowns) \
rather than refusing out-of-character."
    )
}

/// The bracket-command vocabulary the narrator emits alongside prose.
/// Mirrors UIE's scene-effect names so the eventual UI port is direct.
const BRACKET_PROTOCOL: &str = "\
Emit bracket commands alongside your prose to drive the UI deterministically:

- [CHARACTER_TURN:npc_id] ... [CHARACTER_TURN:end]
    Wrap an NPC's spoken line. Use the npc_id from <scenario>present_npcs.

- [OBJECT id=object_id state=new_state]
    Announce a tracked detail changed. Use stable snake_case ids that \
will stay consistent across turns (e.g. item_diamond_necklace, \
npc_gorm_trust, door_cellar). Good for: items gained/lost, NPC moods, \
doors opened, secrets learned.

- [FX effect_name]
    Trigger a scene effect. Valid names: rain, snow, fog, letterbox, \
flash, vignette, shake-light, shake-heavy, spotlight, thunder, glitch, \
blackout, whiteout. Use sparingly: only when the ambiance meaningfully \
shifts.

Bracket commands are machine-read; keep their syntax exact (square brackets, \
colon for character turns, equals sign for object state). Put them on their \
own line, separate from prose.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dungeon_card() -> SimCard {
        SimCard {
            id: "dungeon_tavern".into(),
            name: "The Rusty Tankard".into(),
            card_type: "roleplay".into(),
            core_persona: "A dungeon scenario.".into(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            setting: Some("A frontier tavern at the edge of the Goblinwood.".into()),
            tone: Some("grim, atmospheric".into()),
            opening_scene: Some("Rain lashes the shutters.".into()),
            start_npc_ids: vec!["gorm".into(), "goblin".into()],
            declared_activities: vec!["combat".into()],
            protagonist_name: Some("Alex".into()),
        }
    }

    #[test]
    fn narrator_prompt_contains_core_sections() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("<narrator_role>"));
        assert!(prompt.contains("<scenario>"));
        assert!(prompt.contains("frontier tavern"));
        assert!(prompt.contains("grim, atmospheric"));
        assert!(prompt.contains("gorm, goblin"));
        assert!(prompt.contains("<bracket_commands>"));
        // New Simulation Narrative Engine identity framing.
        assert!(prompt.contains("SIMULATION NARRATIVE ENGINE"));
    }

    /// The protagonist name is now dynamic (was hardcoded "Alex" in the
    /// constant). A card declaring protagonist_name must have it appear
    /// across the prompt — in the role block, the scenario, and the
    /// active_reality anchor.
    #[test]
    fn narrator_prompt_uses_dynamic_protagonist_name() {
        let mut card = dungeon_card();
        card.protagonist_name = Some("Kaelen".into());
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("Kaelen"));
        // The hardcoded "Alex" must NOT leak into a Kaelen card.
        assert!(!prompt.contains("Alex"));
    }

    #[test]
    fn narrator_prompt_forbids_speaking_for_player() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, None);
        // The contract block uses the dynamic name.
        assert!(prompt.contains("Never decide what Alex does"));
    }

    #[test]
    fn narrator_prompt_falls_back_to_you_when_no_protagonist() {
        let card = SimCard {
            id: "minimal".into(),
            name: "Some Scene".into(),
            card_type: "roleplay".into(),
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            setting: Some("A place.".into()),
            tone: None,
            opening_scene: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            protagonist_name: None,
        };
        let prompt = build_narrator_system_prompt(&card, None);
        // The "you" branch must take effect, NOT any hardcoded name.
        assert!(!prompt.contains("Never decide what Alex does"));
        assert!(prompt.contains("second-person") || prompt.contains("second person"));
        assert!(prompt.contains("\"you\""));
    }

    #[test]
    fn narrator_prompt_includes_world_state_when_provided() {
        let card = dungeon_card();
        let state = "weather: stormy\nchest_state: locked";
        let prompt = build_narrator_system_prompt(&card, Some(state));
        assert!(prompt.contains("<world_state>"));
        assert!(prompt.contains("stormy"));
    }

    #[test]
    fn narrator_prompt_omits_world_state_when_empty() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, Some("   "));
        assert!(!prompt.contains("<world_state>"));
    }

    #[test]
    fn narrator_prompt_handles_minimal_card() {
        let card = SimCard {
            id: "minimal".into(),
            name: "Minimal".into(),
            card_type: "roleplay".into(),
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            setting: None,
            tone: None,
            opening_scene: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            protagonist_name: None,
        };
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("<narrator_role>"));
        assert!(prompt.contains("<scenario>"));
        assert!(prompt.contains("<player>"));
    }

    /// The `<active_reality>` anchor (Phase E, 2026-07-18): reinforces the
    /// active card's identity at the prompt tail to override cross-card KV
    /// contamination. Must include the card name + protagonist name when
    /// declared.
    #[test]
    fn narrator_prompt_has_active_reality_with_protagonist() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("<active_reality>"));
        assert!(prompt.contains("The Rusty Tankard"));
        assert!(prompt.contains("The protagonist is Alex"));
        assert!(prompt.contains("NOT any other scenario"));
    }

    /// When `protagonist_name` is None (a card that doesn't declare one),
    /// the anchor falls back to generic phrasing and explicitly forbids
    /// inventing a name: which is the actual defense against the Alex
    /// hallucination when no protagonist is named.
    #[test]
    fn narrator_prompt_active_reality_falls_back_when_no_protagonist() {
        let card = SimCard {
            id: "minimal".into(),
            name: "Some Scene".into(),
            card_type: "roleplay".into(),
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            setting: Some("A place.".into()),
            tone: None,
            opening_scene: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            protagonist_name: None,
        };
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("<active_reality>"));
        // Generic fallback: must NOT contain a hardcoded name.
        assert!(!prompt.contains("The protagonist is Alex"));
        assert!(prompt.contains("never invent or import a protagonist name"));
        assert!(prompt.contains("Some Scene"));
    }

    /// The `<active_reality>` block is the LAST section in the prompt
    /// (closest to the user input). Verify ordering: <narrator_role> first,
    /// <active_reality> last.
    #[test]
    fn active_reality_is_last_section() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, Some("weather: stormy"));
        let narrator_idx = prompt.find("<narrator_role>").expect("narrator_role present");
        let scenario_idx = prompt.find("<scenario>").expect("scenario present");
        let player_idx = prompt.find("<player>").expect("player present");
        let bracket_idx = prompt.find("<bracket_commands>").expect("bracket_commands present");
        let world_idx = prompt.find("<world_state>").expect("world_state present");
        let reality_idx = prompt.find("<active_reality>").expect("active_reality present");
        assert!(narrator_idx < scenario_idx);
        assert!(scenario_idx < player_idx);
        assert!(player_idx < bracket_idx);
        assert!(bracket_idx < world_idx);
        assert!(world_idx < reality_idx);
    }

    /// The "living world" immersion discipline (adapted from UIE) must
    /// survive into the shipped prompt: this is what makes the simulation
    /// feel infinite rather than reactive.
    #[test]
    fn narrator_prompt_contains_living_world_discipline() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("THE LIVING WORLD"));
        assert!(prompt.contains("autonomy"));
        assert!(prompt.contains("NPCs are people"));
    }

    /// The PLAYER STATE discipline (Seam #7) tells the narrator the
    /// injected `<player_state>` block is hard fact — Rust computes it
    /// off-screen, the LLM does zero math, and the prose must honor the
    /// injuries/fatigue exactly. Without this line the narrator would
    /// ignore the injected state or invent its own.
    #[test]
    fn narrator_prompt_contains_player_state_discipline() {
        let card = dungeon_card();
        let prompt = build_narrator_system_prompt(&card, None);
        assert!(prompt.contains("PLAYER STATE IS HARD FACT"));
        assert!(prompt.contains("ABSOLUTE TRUTH"));
        assert!(prompt.contains("never spontaneously heal"));
    }

    /// When a non-default player_state is injected (via the world_state
    /// render), the narrator must see it as part of `<world_state>` and
    /// the discipline block must still be present to govern how it's read.
    #[test]
    fn narrator_prompt_with_player_state_in_world_state() {
        let card = dungeon_card();
        // Simulate what fable_send's render produces after the Referee fires.
        let world = "player_state:\n  stamina: Winded\n  injuries: Left Bicep (Medium Injury)";
        let prompt = build_narrator_system_prompt(&card, Some(world));
        assert!(prompt.contains("<world_state>"));
        assert!(prompt.contains("player_state:"));
        assert!(prompt.contains("Left Bicep (Medium Injury)"));
        assert!(prompt.contains("PLAYER STATE IS HARD FACT"));
    }
}
