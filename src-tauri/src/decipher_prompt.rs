//! The decipher system prompt for the import flow (Slice B of the
//! "Create vs Import" New Game feature, 2026-07-29).
//!
//! When a player drops an existing card file (PNG with embedded JSON, or a
//! plain JSON character card / lorebook), Rust parses the deterministic
//! structure ([`crate::card_import`]) and this module builds the prompt that
//! asks the model to REBUILD the prose fields into clean, WUPI-native text.
//!
//! **The split (mirrors the §11.49 Scribe pattern):** Rust owns structure
//! (PNG chunks, JSON shapes, field mapping, filenames, the denylist, path
//! sandboxing, validation). The model owns prose (rewriting `description`→
//! `core_persona`, etc.). Structure never goes to the model; prose never
//! bypasses Rust validation.
//!
//! **Fidelity bias (Chloe's call on the "Full AI rebuild" fork):** if the
//! source field is already clean narrative prose, preserve it faithfully and
//! verbatim where possible; only rewrite/rebuild when the source is W++
//! pseudo-format, raw HTML, or example-chat cruft. This gives creative
//! rebuild where the source is messy (the common case) and faithful
//! preservation where an author put care into clean prose. The refinement
//! loop (the existing interview) is the drift safety net regardless.
//!
//! **Output contract:** the model emits a single JSON object with the rebuilt
//! fields. Rust parses it (3-pass repair + validator, same fail-proof
//! contract as the schema engine) and builds an [`InterviewDraft`]. Malformed
//! JSON → the decipher fails loudly (never writes a corrupt card); the user
//! can retry or fall back to manual authoring.

use crate::card_import::RawCard;

/// Build the decipher system prompt carrying the raw (un-deciphered) card
/// fields. The model rewrites the prose into clean WUPI-native text and emits
/// a single JSON object (no commentary, no fences).
pub fn build_decipher_system_prompt(raw: &RawCard) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<decipher_role>\n");
    out.push_str(DECIPHER_ROLE);
    out.push_str("\n</decipher_role>\n\n");

    out.push_str("<fidelity_bias>\n");
    out.push_str(FIDELITY_BIAS);
    out.push_str("\n</fidelity_bias>\n\n");

    out.push_str("<output_contract>\n");
    out.push_str(OUTPUT_CONTRACT);
    out.push_str("\n</output_contract>\n\n");

    out.push_str("<source_card>\n");
    out.push_str(&render_raw_card(raw));
    out.push_str("\n</source_card>\n\n");

    out.push_str("<final_instruction>\n");
    out.push_str(FINAL_INSTRUCTION);
    out.push_str("\n</final_instruction>");
    out
}

/// Render the raw card fields into the prompt as labeled blocks. Empty fields
/// are omitted (the model can't rewrite what isn't there). Each field is
/// truncated to a sane cap so a pathological source can't blow the context.
fn render_raw_card(raw: &RawCard) -> String {
    let mut lines: Vec<String> = Vec::new();
    let cap = 4000; // chars per field — generous; the decipher is a one-shot.
    let mut add = |label: &str, val: Option<&str>| {
        if let Some(v) = val {
            let v = v.trim();
            if !v.is_empty() {
                let v = if v.chars().count() > cap {
                    let cut: String = v.chars().take(cap).collect();
                    format!("{cut}\n[...truncated...]")
                } else {
                    v.to_string()
                };
                lines.push(format!("--- {label} ---\n{v}"));
            }
        }
    };
    add("name", raw.name.as_deref());
    add("description (→ core_persona)", raw.description.as_deref());
    add("personality (→ traits)", raw.personality.as_deref());
    add("scenario (→ setting)", raw.scenario.as_deref());
    add("first_mes (→ opening_scene)", raw.first_mes.as_deref());
    add("mes_example (fold into style)", raw.mes_example.as_deref());
    add("creator_notes (→ background)", raw.creator_notes.as_deref());
    if !raw.alternate_greetings.is_empty() {
        let ag = raw
            .alternate_greetings
            .iter()
            .map(|g| format!("- {g}"))
            .collect::<Vec<_>>()
            .join("\n");
        lines.push(format!("--- alternate_greetings (→ introductions) ---\n{ag}"));
    }
    if !raw.tags.is_empty() {
        lines.push(format!("--- tags ---\n{}", raw.tags.join(", ")));
    }
    lines.join("\n\n")
}

const DECIPHER_ROLE: &str = "\
You are a card-transformation engine. You receive a character card in a legacy \
chat format and rebuild it into clean, vivid, WUPI-native prose. You do NOT \
invent new characters, relationships, or plot — you transform what's given. \
Your only output is a single JSON object (no commentary, no markdown fences, \
no preamble).";

const FIDELITY_BIAS: &str = "\
FIDELITY IS YOUR PRIMARY DIRECTIVE. The author spent real effort on this card.\n\
\n\
- If a source field is ALREADY clean narrative prose, PRESERVE it faithfully — \
ideally verbatim, or with only light tightening. Do not rewrite for the sake \
of rewriting.\n\
- Only REBUILD when the source is a legacy pseudo-format: W++ bracket notation \
([\"hair\"; \"black\"; ...]), raw HTML tags, example-chat formatting cruft \
({{user}}/{{char}} markers, <START> blocks), or comma-dump personality lists. \
In those cases, transform the data into fluid prose.\n\
- Never lose concrete detail. If the source says 'scar over the left eye', the \
rebuilt appearance says 'scar over the left eye' — not 'a weathered face'.\n\
- Keep the character's NAME and core identity exactly as authored.\n\
- Refer to the player neutrally as 'the player' (never 'hero'/'protagonist'/'\
chosen one' — those titles bias the simulation).";

const OUTPUT_CONTRACT: &str = "\
Emit EXACTLY one JSON object with these keys (omit any key whose value would \
be empty):\n\
{\n\
  \"name\": string,                     // the character/world name, verbatim\n\
  \"core_persona\": string,             // rebuilt from description\n\
  \"appearance\": string,               // rebuilt description (physical)\n\
  \"traits\": [string, ...],            // rebuilt from personality (short bullets)\n\
  \"setting\": string,                  // rebuilt from scenario\n\
  \"tone\": string,                     // 1-3 word mood (optional)\n\
  \"opening_scene\": string,            // rebuilt from first_mes (2nd person, present tense)\n\
  \"introductions\": [string, ...],     // rebuilt alternate_greetings (optional)\n\
  \"player_background\": string         // rebuilt from creator_notes (optional)\n\
}\n\
\n\
Rules:\n\
- opening_scene is second person ('you'), present tense, sensory — the player's \
first moment in the world.\n\
- traits are SHORT (2-5 words each) — bullet seeds, not paragraphs.\n\
- Do NOT emit fields that have no source. If there's no appearance in the \
source, omit appearance entirely.\n\
- Output the raw JSON only. No ```json fences. No explanation.";

const FINAL_INSTRUCTION: &str = "\
Rebuild the card now. Emit the single JSON object.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_every_present_field() {
        let raw = RawCard {
            name: Some("Mira".into()),
            description: Some("A smuggler.".into()),
            personality: Some("brave".into()),
            scenario: Some("A cantina.".into()),
            first_mes: Some("She looks up.".into()),
            ..Default::default()
        };
        let p = build_decipher_system_prompt(&raw);
        assert!(p.contains("Mira"));
        assert!(p.contains("A smuggler."));
        assert!(p.contains("A cantina."));
        assert!(p.contains("She looks up."));
    }

    #[test]
    fn prompt_omits_empty_fields() {
        let raw = RawCard {
            name: Some("Solo".into()),
            ..Default::default()
        };
        let p = build_decipher_system_prompt(&raw);
        assert!(p.contains("Solo"));
        // No description block since it's absent.
        assert!(!p.contains("→ core_persona"));
    }

    #[test]
    fn prompt_truncates_pathologically_long_fields() {
        let huge = "x".repeat(10_000);
        let raw = RawCard {
            name: Some("Big".into()),
            description: Some(huge),
            ..Default::default()
        };
        let p = build_decipher_system_prompt(&raw);
        assert!(p.contains("[...truncated...]"), "long field must be capped");
    }

    #[test]
    fn prompt_carries_fidelity_bias_verbatim() {
        let p = build_decipher_system_prompt(&RawCard::default());
        assert!(p.contains("PRESERVE it faithfully"), "fidelity bias present");
        assert!(p.contains("Never lose concrete detail"));
    }

    #[test]
    fn prompt_documents_json_output_contract() {
        let p = build_decipher_system_prompt(&RawCard::default());
        assert!(p.contains("\"opening_scene\""));
        assert!(p.contains("\"introductions\""));
        assert!(p.contains("No ```json fences"));
    }

    #[test]
    fn prompt_renders_alternate_greetings_as_bullets() {
        let raw = RawCard {
            name: Some("X".into()),
            alternate_greetings: vec!["Greeting one.".into(), "Greeting two.".into()],
            ..Default::default()
        };
        let p = build_decipher_system_prompt(&raw);
        assert!(p.contains("- Greeting one."));
        assert!(p.contains("- Greeting two."));
    }
}
