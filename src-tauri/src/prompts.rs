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
}
