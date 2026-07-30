pub const OS_DIRECTIVES: &str = "\
You are operating as a process within WUPI: a local, AI-native simulation \
runtime. You are the active Simulation Card: a simulation interface reasoning \
through a structured environment, not a generic chatbot.

Structural discipline: respect the tags and channels provided. Context marked \
<retrieved_memory> holds PAST records, not the current scene: they are \
reference material only, never continuity to adopt. Context marked \
<world_state> is persistent ground truth about the simulated world. Context \
marked <user_profile> describes the operator you are speaking with: treat it \
as authoritative identity, not a suggestion. When memory and the live \
conversation disagree, the live conversation always wins.";

/// The agent/tool-calling protocol. Scoped to the TOOL pass of `chat_send`
/// ONLY (see `lib.rs::chat_send`'s `agent_system_prompt` branch): the catgirl
/// persona carries conversational text, and this block carries the structured-
/// output contract for tool arguments, file contents, and code blocks. The
/// prose pass (API handoff, local fallbacks, the no-tool final decode) never
/// sees it — mirroring the narrator-prose vs tracker-mechanical split already
/// in place on the Fable side (`build_api_narrator_system_prompt` vs the
/// tracker/scribe prompts).
///
/// **§1C compliance — written POSITIVELY.** No "suppress", no "do not", no
/// "CRITICAL WALL": negative framing is the Logit Echo Effect engine (it
/// surfaces the very behavior it prohibits). The contract is stated as what
/// the structured output IS, not what it must stop being. The natural catgirl
/// voice is the default for all conversational text; this block only governs
/// the machine-readable surface. The two are complementary, not in tension —
/// co-residence in the system turn causes zero persona contamination (unlike
/// the §11.52 fable tracker split, which needed a `clear_kv_cache` because the
/// narrator's "never emit brackets" directly conflicted with the tracker's
/// "emit brackets"). No clear-KV is needed here.
pub const WUPI_AGENT_PROTOCOL: &str = "\
<agent_protocol>\n\
Structured outputs — tool arguments, file contents, code blocks, and any \
machine-readable payload — are raw, syntactically valid, and exact. Emit only \
the payload itself: valid JSON for tool arguments, valid source for code, the \
literal file body for file writes. No prose, commentary, emoji, or framing \
around these payloads; your natural conversational voice carries all \
surrounding chat text.\n\
</agent_protocol>";

pub fn build_system_content(
    settings: &WupiSettings,
    persona: Option<&str>,
    user_profile: Option<&str>,
    effective_ctx: u32,
) -> String {
    let mut sections = Vec::new();

    sections.push(format!(
        "<os_directives>\n{}\n</os_directives>",
        OS_DIRECTIVES
    ));

    if let Some(p) = persona.filter(|s| !s.trim().is_empty()) {
        sections.push(p.to_owned());
    }

    if let Some(p) = user_profile.filter(|s| !s.trim().is_empty()) {
        sections.push(p.to_owned());
    }

    sections.push(format!(
        "<current_context>\ncontext_size: {}\nconversation_budget: {}\n</current_context>",
        effective_ctx, settings.conversation_budget
    ));

    sections.join("\n\n")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WupiSettings {
    pub context_size: u32,
    pub conversation_budget: u32,
}

impl Default for WupiSettings {
    fn default() -> Self {
        Self {
            // 4096 (raised 2026-07-27 from the 3072 set in §2C). The §2C cut
            // to 3072 bought back ~300-400 MiB of VRAM headroom but left only
            // a 2304-token prompt budget (n_ctx - generation_reserve = 3072 -
            // 768) — too tight: the Quick Play interview system prompt alone
            // is ~2340 tokens, deterministically failing generation on a
            // default-settings Local install (`context too long even after
            // truncation: ... max 2304`, reproduced on a friend's PC). 4096
            // restores a safe 3328-token budget (1024-token reserve) with no
            // OOM risk on 12 GB under the v0.6.4 swap-lock (only ONE of
            // chat/schema/fable is resident at a time; weights ~9.8 GB +
            // one ~150 MB Q8_0 KV context + embedder ~36 MB ≈ 10 GB used).
            context_size: 4096,
            conversation_budget: 8192,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_system_content_includes_live_settings() {
        let settings = WupiSettings {
            context_size: 2048,
            conversation_budget: 8192,
        };

        let content = build_system_content(&settings, None, None, 2048);
        assert!(content.contains("<os_directives>"));
        assert!(content.contains("context_size: 2048"));
        assert!(content.contains("conversation_budget: 8192"));
    }

    #[test]
    fn persona_section_is_optional() {
        let settings = WupiSettings::default();

        let without = build_system_content(&settings, None, None, 4000);
        assert!(!without.contains("<persona>"));

        // With persona → section present.
        let with = build_system_content(&settings, Some("<persona>\nWupi\n</persona>"), None, 4000);
        assert!(with.contains("<persona>"));
    }

    #[test]
    fn empty_persona_is_suppressed() {
        let settings = WupiSettings::default();
        let content = build_system_content(&settings, Some("   "), None, 4000);
        assert!(!content.contains("<persona>"));
    }

    #[test]
    fn user_profile_section_is_optional_and_ordered_after_persona() {
        let settings = WupiSettings::default();

        let without = build_system_content(&settings, None, None, 4000);
        assert!(!without.contains("</user_profile>"));

        let profile = "<user_profile>\nname: Operator\n</user_profile>";
        let with = build_system_content(&settings, None, Some(profile), 4000);
        assert!(with.contains("</user_profile>"));

        let persona = "<persona>\nname: Wupi\n</persona>";
        let both = build_system_content(&settings, Some(persona), Some(profile), 4000);
        let persona_pos = both.find("</persona>").unwrap();
        let profile_pos = both.find("</user_profile>").unwrap();
        let ctx_pos = both.find("<current_context>").unwrap();
        assert!(persona_pos < profile_pos, "persona before user_profile");
        assert!(profile_pos < ctx_pos, "user_profile before current_context");
    }

    #[test]
    fn empty_user_profile_is_suppressed() {
        let settings = WupiSettings::default();
        let content = build_system_content(&settings, None, Some("   "), 4000);
        // Closing tag only appears in a rendered section (see note above).
        assert!(!content.contains("</user_profile>"));
    }

    #[test]
    fn effective_ctx_overrides_reported_context_size() {
        let settings = WupiSettings {
            context_size: 4000,
            conversation_budget: 16000,
        };
        let content = build_system_content(&settings, None, None, 2048);
        assert!(content.contains("context_size: 2048"));
        assert!(
            !content.contains("context_size: 4000"),
            "settings.context_size must NOT leak when effective_ctx differs"
        );
        assert!(content.contains("conversation_budget: 16000"));
    }

    #[test]
    fn agent_protocol_is_wrapped_and_positive() {
        // §1C (Prompt-Codex Discipline): the agent protocol must be wrapped in
        // its tagged section and written POSITIVELY — no negative framing that
        // would echo the very behavior it governs ("suppress", "do not",
        // "CRITICAL WALL", "never"). Negative meta-rules surface the banned
        // token in the model's context.
        assert!(WUPI_AGENT_PROTOCOL.contains("<agent_protocol>"));
        assert!(WUPI_AGENT_PROTOCOL.contains("</agent_protocol>"));
        let lower = WUPI_AGENT_PROTOCOL.to_lowercase();
        for banned in [
            "suppress",
            "do not",
            "don't",
            "critical wall",
            "never ",
            "must stop",
            "forbidden",
        ] {
            assert!(
                !lower.contains(banned),
                "WUPI_AGENT_PROTOCOL must not contain '{}' (§1C no-echo: negative framing surfaces the banned token)",
                banned
            );
        }
        // The positive contract must be present.
        assert!(WUPI_AGENT_PROTOCOL.contains("syntactically valid"));
    }
}
