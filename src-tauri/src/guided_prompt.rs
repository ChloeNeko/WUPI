//! System prompt builder for **Ghostwriter** — the Fable authoring assistant.
//!
//! Ghostwriter has two modes, both invoked from a floating action button above
//! the narrator input:
//!
//!   - **Impersonate**: the player types a rough outline in the narrator
//!     compose box, hits Ghostwriter → Impersonate, and the model fleshes it
//!     out into immersive second-person prose in the player's voice. The
//!     result drops back into the compose box for the player to review/edit
//!     before sending. (POV is fixed at second-person to match the narrator's
//!     camera — the 1st/2nd/3rd-person selectors from the SillyTavern original
//!     were dropped per the design decision.)
//!
//!   - **Director**: the player types a steer ("make the barkeeper suspicious
//!     of me"), Ghostwriter rewrites it as a single concise
//!     `<director_directive>` XML block, and the result is stored in
//!     `AppState::pending_directive`. The NEXT narrator turn consumes it
//!     (one-shot) and threads it into the narrator prompt as a hard constraint
//!     the narrator must obey for that turn. Honors §7's no-silent-injection
//!     rule: the directive is visible to the narrator in its Rust-authored
//!     prompt, not a hidden context injection.
//!
//! ## Memoryless by construction
//!
//! Neither mode touches `state.session` or archives to memory. Both are pure
//! authoring aids that produce text the caller (the frontend) places somewhere
//! specific (compose box for Impersonate; `pending_directive` slot for Director).

/// The two Ghostwriter modes. Passed across the IPC boundary as a snake_case
/// string; the frontend's FAB UI picks one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhostwriterMode {
    Impersonate,
    Director,
}

impl GhostwriterMode {
    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "impersonate" => GhostwriterMode::Impersonate,
            "director" => GhostwriterMode::Director,
            _ => return None,
        })
    }
}

/// Caller-supplied parameters. `player_input` is the rough text the player
/// typed (compose-box contents for Impersonate, steer text for Director).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GhostwriterRequest {
    pub mode: Option<GhostwriterMode>,
    pub player_input: String,
}

/// Build the generation system prompt for one mode. Sterile + structural voice
/// (NOT the catgirl persona — these are generation prompts, like the Quick Play
/// `interview_prompt` and `crossroads_prompt` builders).
pub fn build_ghostwriter_system_prompt(req: &GhostwriterRequest) -> String {
    let mode = req.mode.unwrap_or(GhostwriterMode::Impersonate);
    let (role, body_prompt, output_contract) = match mode {
        GhostwriterMode::Impersonate => (IMPERSONATE_ROLE, IMPERSONATE_BODY, IMPERSONATE_OUTPUT),
        GhostwriterMode::Director => (DIRECTOR_ROLE, DIRECTOR_BODY, DIRECTOR_OUTPUT),
    };

    let mut out = String::with_capacity(2000);

    out.push_str("<ghostwriter_role>\n");
    out.push_str(role);
    out.push_str("\n</ghostwriter_role>\n\n");

    out.push_str("<task>\n");
    out.push_str(body_prompt);
    out.push_str("\n</task>\n\n");

    out.push_str("<output_contract>\n");
    out.push_str(output_contract);
    out.push_str("\n</output_contract>\n\n");

    out.push_str("<player_input>\n");
    if req.player_input.trim().is_empty() {
        // Defensive — the frontend should never send an empty input, but if it
        // does the model should emit a clearly-marked placeholder rather than
        // hallucinate a random scene.
        out.push_str("(empty — emit a single line: \"[no input provided]\")\n");
    } else {
        out.push_str(req.player_input.trim());
        out.push('\n');
    }
    out.push_str("</player_input>\n");

    out
}

// ── Prompt text blocks ───────────────────────────────────────────────────

const IMPERSONATE_ROLE: &str = "\
You are the PLAYER'S VOICE POLISHER for an in-progress roleplay. The player has \
written a rough outline of what their character does or says next. Your job is \
to flesh that outline into immersive, second-person prose that the player can \
drop into the narrator's input box and send as their next action.\n\
\n\
You are not the narrator. You do not describe the world, the NPCs, or the \
consequences of the action. You write ONLY what the player does — the \
action itself, in the player's voice, second-person present tense (\"You \
walk to the bar. You set three coppers down with a deliberate click.\").\n\
\n\
The narrator will narrate the world's response. Your output ends the instant \
the player's action ends.";

const IMPERSONATE_BODY: &str = "\
FLESH OUT THE OUTLINE: read the player's input as their intent. Translate it \
into prose that:\n\
- Matches the surrounding scene's tone and prose voice (the player sees the \
  narrator's beats around this action; match their density).\n\
- Honors the player's physical limits (injuries, exhaustion, what they're \
carrying).\n\
- Adds sensory specificity the player's outline implies but didn't spell out \
  (the click of coin on wood, the creak of a chair pushed back, the weight of \
  a hand on a hilt).\n\
- Stops the moment the action stops. Do not narrate the result. Do not narrate \
  NPC reactions. The narrator does both — your job ends with the action.\n\
\n\
Do NOT invent consequences. Do NOT put words in NPCs' mouths. Do NOT describe \
what the world does in response. The action, the action only.";

const IMPERSONATE_OUTPUT: &str = "\
OUTPUT: emit ONLY the rewritten prose. No preamble, no closing remark, \
no markdown fence. No quotation marks wrapping the whole thing. No labels, no \
headers. The first character of your response is the first character of the \
prose. The last character is the final period of the action.\n\
\n\
(\"No quotation marks wrapping the whole thing\" means don't put the ENTIRE \
output inside one pair of quotes. Dialogue the player SPEAKS within the action \
should still be wrapped in double quotes per the RP CONVENTIONS: \
You say \"I'll take the room,\" and drop a coin on the bar.)\n\
\n\
Do NOT narrate the result of the action. Do NOT put words in NPCs' mouths. \
Do NOT describe what the world does in response. The action, the action only.\n\
\n\
If the player's input is already a complete, well-written action, return it \
essentially unchanged (light polish only). The point is to fill in outlines, \
not to rewrite finished prose.";

const DIRECTOR_ROLE: &str = "\
You are the DIRECTIVE REWRITER for an in-progress roleplay. The player wants to \
steer the world — make an NPC suspicious, change the weather, advance time, \
shift a faction's stance. Their free-text steer needs to become a single \
concise directive the narrator will treat as a hard, one-turn constraint.\n\
\n\
You are not the narrator. You will not narrate. You produce ONLY a single \
`<director_directive>` XML block — nothing else.";

const DIRECTOR_BODY: &str = "\
CONVERT THE STEER: read the player's input as their intent for how the WORLD \
should shift on the next narrator turn. Rewrite it as a single, imperative, \
specific directive the narrator can obey in one beat. The directive should:\n\
- Be ONE sentence. Concise. Concrete enough that the narrator cannot \
  misinterpret it.\n\
- Address the WORLD, not the player. \"The barkeeper becomes suspicious \
  of the player\" is correct. \"I try to read the barkeeper's mood\" is \
  wrong — that's a player action, not a world shift.\n\
- Be plausible given the scene. A directive that contradicts established canon \
  (\"the barkeeper suddenly loves the player\" with no setup) should be \
  softened to what the scene can actually deliver (\"the barkeeper's distrust \
  wavers for a moment\").\n\
- Be scoped to one turn. The narrator obeys this for the NEXT beat only; it \
  does not permanently rewrite the world. Phrase accordingly.";

const DIRECTOR_OUTPUT: &str = "\
OUTPUT: emit ONLY a single XML block in this exact shape:\n\
\n\
<director_directive>\n\
Your single-sentence directive here.\n\
</director_directive>\n\
\n\
No preamble. No explanation. No markdown fence. No other tags. The directive \
goes on its own line between the opening and closing tags. If the player's \
input is genuinely unparseable as a world-shift, emit:\n\
\n\
<director_directive>\n\
(unparseable — ignored)\n\
</director_directive>";

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn req(mode: GhostwriterMode, input: &str) -> GhostwriterRequest {
        GhostwriterRequest { mode: Some(mode), player_input: input.to_string() }
    }

    // --- Prompt builder structure ---

    #[test]
    fn prompt_carries_all_blocks_for_each_mode() {
        for mode in [GhostwriterMode::Impersonate, GhostwriterMode::Director] {
            let p = build_ghostwriter_system_prompt(&req(mode, "test"));
            for tag in ["<ghostwriter_role>", "<task>", "<output_contract>", "<player_input>"] {
                assert!(p.contains(tag), "mode {mode:?} missing {tag}");
            }
        }
    }

    // --- Impersonate-specific ---

    #[test]
    fn impersonate_prompt_forbids_preamble() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Impersonate, "x"));
        assert!(p.contains("No preamble"));
        assert!(p.contains("no markdown fence"));
    }

    #[test]
    fn impersonate_prompt_pins_second_person() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Impersonate, "x"));
        assert!(p.contains("second-person"));
        assert!(p.contains("present tense"));
    }

    #[test]
    fn impersonate_prompt_forbids_narrating_results() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Impersonate, "x"));
        assert!(p.contains("Do NOT narrate the result"));
        assert!(p.contains("Do NOT put words in NPCs"));
    }

    #[test]
    fn impersonate_prompt_folds_player_input() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Impersonate, "I want to order an ale and probe the innkeep about the hooded stranger"));
        assert!(p.contains("order an ale"));
        assert!(p.contains("hooded stranger"));
    }

    // --- Director-specific ---

    #[test]
    fn director_prompt_emits_director_directive_block_contract() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Director, "x"));
        assert!(p.contains("<director_directive>"));
        assert!(p.contains("</director_directive>"));
    }

    #[test]
    fn director_prompt_forbids_preamble() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Director, "x"));
        assert!(p.contains("No preamble"));
        assert!(p.contains("No markdown fence"));
    }

    #[test]
    fn director_prompt_addresses_world_not_player() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Director, "x"));
        assert!(p.contains("Address the WORLD, not the player"));
    }

    #[test]
    fn director_prompt_requires_one_sentence() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Director, "x"));
        assert!(p.contains("ONE sentence"));
    }

    #[test]
    fn director_prompt_folds_player_input() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Director, "make the barkeeper suspicious of me"));
        assert!(p.contains("make the barkeeper suspicious"));
    }

    // --- Edge cases ---

    #[test]
    fn prompt_marks_blank_input() {
        let p = build_ghostwriter_system_prompt(&req(GhostwriterMode::Impersonate, "   "));
        assert!(p.contains("empty"));
    }

    #[test]
    fn mode_from_id_roundtrip() {
        for mode in [GhostwriterMode::Impersonate, GhostwriterMode::Director] {
            let id = match mode {
                GhostwriterMode::Impersonate => "impersonate",
                GhostwriterMode::Director => "director",
            };
            assert_eq!(GhostwriterMode::from_id(id), Some(mode));
        }
    }

    #[test]
    fn mode_from_id_rejects_unknown() {
        assert!(GhostwriterMode::from_id("nonsense").is_none());
        assert!(GhostwriterMode::from_id("").is_none());
    }

    #[test]
    fn mode_from_id_is_case_insensitive() {
        assert_eq!(GhostwriterMode::from_id("IMPERSONATE"), Some(GhostwriterMode::Impersonate));
    }
}
