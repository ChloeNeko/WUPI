//! The Game Master's per-turn system prompt for the New Game interview flow.
//!
//! During a New Game interview (`interview_send`), GLM-5.2 plays the Game
//! Master — a captivating narrator asking atmospheric, one-at-a-time questions
//! to build the player's character + world. This module builds the GM's
//! per-turn system prompt, which folds in:
//!
//! 1. The GM persona from `data/gm.sim` (rendered via `SimCard::render_for_prompt`)
//! 2. Retrieved `fable.codex` entries (the deep playbook — question banks, genre
//!    guides, perfect-card examples) via `search_fable_visible` (Phase A)
//! 3. The current `InterviewDraft` state summary (compensates for the 6-turn
//!    history window — the GM always knows what's been established)
//! 4. Interview directives (one question at a time, react briefly, emit
//!    `[READY]` when done)
//!
//! **Naming contract (§11.29):** the player is always "User" unless they
//! volunteer a name. The GM addresses the player neutrally — "you", or by
//! their chosen name. The player is treated like any other person in the
//! world; no special framing, no coddling.

use crate::interview_draft::InterviewDraft;

/// The `[READY]` sentinel the GM emits as the LAST token of its turn when the
/// interview is complete. The orchestrator (`interview_send`) watches for
/// this in the GM's output; on detection it emits `{type:"ready"}` to the
/// frontend (enabling the Begin button) and skips the scribe (the draft is
/// final). Documented in the fable.codex "Core Question Ladder" entry.
pub const READY_SENTINEL: &str = "[READY]";

/// Build the GM's system prompt for one interview turn.
///
/// `gm_persona` is the rendered `data/gm.sim` persona block (from
/// `SimCard::render_for_prompt`). `retrieved_playbook` is the optional
/// `fable.codex` retrieval block (from `render_memory_block(hits)` — already
/// formatted with the codex frame). `draft` is the current interview draft
/// (folded in as a state summary so the GM knows what's established despite
/// the 6-turn history window).
pub fn build_gm_system_prompt(
    gm_persona: Option<&str>,
    retrieved_playbook: Option<&str>,
    draft: &InterviewDraft,
) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<gm_role>\n");
    out.push_str(GM_ROLE);
    out.push_str("\n</gm_role>\n\n");

    // The GM persona from gm.sim (the lean identity — the deep playbook lives
    // in fable.codex, surfaced via retrieval below).
    if let Some(persona) = gm_persona {
        if !persona.trim().is_empty() {
            out.push_str(persona.trim());
            out.push_str("\n\n");
        }
    }

    out.push_str("<gm_directives>\n");
    out.push_str(GM_DIRECTIVES);
    out.push_str("\n</gm_directives>\n\n");

    // Retrieved fable.codex entries (question banks, genre guides, perfect-card
    // examples). Surfaced under the codex frame: "Reference knowledge you
    // possess; internalize it, weave it naturally." Zero baseline-prompt cost
    // when no entries retrieve (the playbook loads on semantic match).
    if let Some(playbook) = retrieved_playbook {
        if !playbook.trim().is_empty() {
            out.push_str(playbook.trim());
            out.push_str("\n\n");
        }
    }

    // The current draft state — compensates for the 6-turn history window.
    // Even when early answers scroll out of the message array, the GM always
    // knows what's been established (name, setting, NPCs, tone, etc.).
    // Always renders at least "Player: User" per §11.29.
    if let Some(summary) = draft.render_state_summary() {
        out.push_str("<current_draft>\n");
        out.push_str(&summary);
        out.push_str("\n</current_draft>\n\n");
    }

    out
}

const GM_ROLE: &str = "\
You are the GAME MASTER — a captivating narrator conducting a 1-on-1 interview \
to design a new roleplay scenario with the player. You ask atmospheric, \
probing questions ONE AT A TIME, react briefly to each answer, and build a \
vivid world + character through conversation. The player watches their card \
assemble on screen as you talk; a hidden Scribe extracts your exchange into \
the building draft. You never see the draft — just keep the conversation \
vivid and factual.";

const GM_DIRECTIVES: &str = "\
INTERVIEW FLOW:\
\n- Ask ONE question at a time. Never a questionnaire dump.\
\n- Be atmospheric — match the genre's sensory texture (see the retrieved \
playbook for genre-specific question flavors).\
\n- React briefly (one short sentence) to each answer before the next question.\
\n- When the player contradicts themselves, ask which they meant.\
\n\n\
NAMING (§11.29): the player is 'User' unless they explicitly volunteer a name. \
Address the player as 'you', or by their chosen name if they gave one. The \
player is a person in the world, treated like any other.\
\n\n\
WHEN DONE: when every relevant slot in the question ladder is filled (or the \
player says they're ready), ask one final question about the opening scene \
preference ('Where do you want the story to start? What should the first \
scene address?'). After the player answers, emit `[READY]` as the LAST token \
of your turn. The system catches this and enables the Begin button.\
\n\n\
NO STATS, NO NUMBERS, NO XML. You speak pure prose. The Scribe handles \
extraction; the system handles the file. A condition is 'exhausted from the \
road', never 'stamina 3/10'. Wealth is 'low on coin', never '50 gold'.";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview_draft::InterviewDraft;

    #[test]
    fn prompt_includes_role_and_directives() {
        let draft = InterviewDraft::default();
        let prompt = build_gm_system_prompt(None, None, &draft);
        assert!(prompt.contains("<gm_role>"));
        assert!(prompt.contains("<gm_directives>"));
        assert!(prompt.contains("GAME MASTER"));
    }

    #[test]
    fn prompt_folds_in_persona_when_present() {
        let draft = InterviewDraft::default();
        let persona = "<persona>\n  <name>Game Master</name>\n</persona>";
        let prompt = build_gm_system_prompt(Some(persona), None, &draft);
        assert!(prompt.contains("<name>Game Master</name>"));
    }

    #[test]
    fn prompt_folds_in_retrieved_playbook_when_present() {
        let draft = InterviewDraft::default();
        let playbook = "Reference knowledge:\n<c title=\"Card Archetypes\">...</c>";
        let prompt = build_gm_system_prompt(None, Some(playbook), &draft);
        assert!(prompt.contains("Reference knowledge"));
        assert!(prompt.contains("Card Archetypes"));
    }

    #[test]
    fn prompt_folds_in_current_draft_state() {
        let mut draft = InterviewDraft::default();
        draft.name = Some("The Neon Dragon".to_string());
        let prompt = build_gm_system_prompt(None, None, &draft);
        assert!(prompt.contains("<current_draft>"));
        assert!(prompt.contains("The Neon Dragon"));
        assert!(prompt.contains("Player: User"));
    }

    #[test]
    fn prompt_uses_no_titled_defaults_for_player() {
        // §11.29 (hardened, anti-positivity-bias + anti-echo): the prompt must
        // not contain titled defaults AND must not contain meta-rules that
        // mention those titles (a "don't call the player X" clause surfaces X
        // in the model's context and can trigger the very bias it prohibits).
        // The directives phrase everything positively: just refer to the
        // player neutrally. The player is a person in the world, treated like
        // any other — no special treatment, no coddling.
        let draft = crate::interview_draft::InterviewDraft::default();
        let prompt = build_gm_system_prompt(None, None, &draft);
        let lower = prompt.to_lowercase();
        for banned in [
            "hero",
            "chosen one",
            "main character",
            "adventurer",
            "traveler-king",
            "protagonist",
            "don't call",
            "don't refer",
            "never call",
            "never address",
            "never attach",
        ] {
            assert!(
                !lower.contains(banned),
                "GM prompt must not contain '{}' (banned title OR anti-echo meta-rule)",
                banned
            );
        }
        // Sanity: the directives convey the spirit via positive framing only.
        assert!(GM_DIRECTIVES.contains("Address the player as 'you'"));
    }

    #[test]
    fn prompt_documents_ready_sentinel() {
        let draft = InterviewDraft::default();
        let prompt = build_gm_system_prompt(None, None, &draft);
        assert!(prompt.contains("[READY]"));
        assert!(prompt.contains("LAST token"));
    }

    #[test]
    fn ready_sentinel_constant_is_stable() {
        // The orchestrator greps the GM output for this exact string. Pin it.
        assert_eq!(READY_SENTINEL, "[READY]");
    }
}
