//! System prompt builder for **Selective Regenerate** — the highlight-to-
//! regenerate UX chat control (the 4th sibling of edit / reroll / rewind,
//! §11.23).
//!
//! The player highlights a passage inside the AI's most recent beat. A popup
//! appears; clicking it asks the model to rewrite ONLY the highlighted slice
//! so the new text flows seamlessly into the prose immediately before and
//! after it. The result is spliced back into the same beat in-place (no new
//! turn, no schema mutation — same line as `apply_edit`).
//!
//! ## Memoryless by construction
//!
//! Like Crossroads (`crossroads_prompt`) + Ghostwriter (`guided_prompt`), this
//! is a one-shot generation: no `state.session` push, no memory archive, no
//! schema delta. The output is a string splice, not a conversation turn.

/// Caller-supplied parameters. `before` / `highlight` / `after` are the three
/// slices of the beat's body text split at the highlight boundaries (the
/// frontend locates the selection via `String.indexOf`, then sends the three
/// pieces). `highlight` is the text being replaced; `before` + `after` are the
/// surrounding prose the new text must flow into.
///
/// All three are sent across the IPC boundary so the backend can verify
/// `before + highlight + after` is still a contiguous substring of the live
/// beat before splicing (defensive against a stale UI snapshot — the pure
/// helper `apply_regenerate_slice` in lib.rs enforces it).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct RegenerateSliceRequest {
    pub before: String,
    pub highlight: String,
    pub after: String,
}

impl RegenerateSliceRequest {
    /// True when the highlight is empty or whitespace — the caller should
    /// refuse to generate (and the popup shouldn't have appeared).
    pub fn highlight_is_empty(&self) -> bool {
        self.highlight.trim().is_empty()
    }
}

/// Build the generation system prompt. Sterile + structural voice (NOT the
/// catgirl persona — this is a generation prompt, like `guided_prompt` and
/// `crossroads_prompt`).
pub fn build_regenerate_slice_system_prompt(req: &RegenerateSliceRequest) -> String {
    let mut out = String::with_capacity(3000);

    out.push_str("<role>\n");
    out.push_str(ROLE);
    out.push_str("\n</role>\n\n");

    out.push_str("<task>\n");
    out.push_str(TASK);
    out.push_str("\n</task>\n\n");

    out.push_str("<output_contract>\n");
    out.push_str(OUTPUT_CONTRACT);
    out.push_str("\n</output_contract>\n\n");

    // The three slices. Each is wrapped in its own tag so the model can't
    // conflate them; the tags are clearly labeled with their structural role
    // (anchor before, the passage to replace, anchor after). Whitespace is
    // preserved verbatim — the model needs to see the exact spacing to know
    // whether its replacement should start/end mid-sentence.
    out.push_str("<passage_to_rewrite>\n");
    out.push_str("This is a slice of an in-progress narrator beat. The ");
    out.push_str("highlighted passage is what you will replace; the text ");
    out.push_str("before and after are immutable anchors the replacement ");
    out.push_str("must flow into.\n\n");

    out.push_str("<before_anchor>\n");
    if req.before.is_empty() {
        out.push_str("(the highlight is at the very start of the beat)\n");
    } else {
        out.push_str(&req.before);
        out.push('\n');
    }
    out.push_str("</before_anchor>\n\n");

    out.push_str("<highlighted_to_replace>\n");
    if req.highlight_is_empty() {
        // Defensive — the frontend should never send an empty highlight, but
        // if it does the model should bail with a clear marker rather than
        // hallucinate a replacement for nothing.
        out.push_str("(empty — emit a single line: \"[no highlight provided]\")\n");
    } else {
        out.push_str(&req.highlight);
        out.push('\n');
    }
    out.push_str("</highlighted_to_replace>\n\n");

    out.push_str("<after_anchor>\n");
    if req.after.is_empty() {
        out.push_str("(the highlight is at the very end of the beat)\n");
    } else {
        out.push_str(&req.after);
        out.push('\n');
    }
    out.push_str("</after_anchor>\n\n");

    out.push_str("</passage_to_rewrite>\n");

    out
}

// ── Prompt text blocks ───────────────────────────────────────────────────

const ROLE: &str = "\
You are the PROSE REWRITER for an in-progress roleplay. The player has \
highlighted a passage of the narrator's most recent beat and asked for it to \
be regenerated. Your job is to write a replacement that fills the EXACT gap \
left by the highlighted text — no more, no less.\n\
\n\
You are not the narrator. You do not write a new beat. You produce ONLY the \
replacement passage that will be spliced into the same spot the highlight \
occupied.";

const TASK: &str = "\
REWRITE THE HIGHLIGHTED PASSAGE. The new text must:\n\
- Flow seamlessly into the sentences immediately before and after it. Read \
the <before_anchor> and <after_anchor> carefully — your replacement is a \
middle fragment, not a standalone paragraph. If the anchor before ends \
mid-sentence, your replacement continues that sentence; if the anchor after \
begins mid-sentence, your replacement leads into it.\n\
- Match the surrounding prose exactly: same voice, same tense, same POV \
(second-person present, matching the narrator's camera), same tone, same \
density of sensory detail. A reader should not be able to tell where the \
original ends and your replacement begins.\n\
- Honor what the passage was doing structurally. If the highlight was \
narrating an action, the replacement narrates an action; if it was \
describing a setting, the replacement describes a setting. Do not change \
what kind of beat-fragment the passage is.\n\
- Stay scoped to the highlight. Do not narrate past where the original \
ended. Do not reach forward into consequences the anchor after doesn't \
already imply. The replacement occupies the same narrative footprint as \
the highlight.\n\
- Vary meaningfully from the highlight. The player asked for a regenerate \
because they didn't like the original — produce a genuinely different take \
on the same beat-fragment, not a paraphrase that lands in the same place.\n\
\n\
Do NOT advance the scene beyond where the original beat ended. Do NOT \
introduce new NPCs, new world facts, or new state the surrounding prose \
doesn't already support. Do NOT put words in a NPC's mouth that the anchor \
after doesn't carry forward. The replacement is a splice, not a new turn.";

const OUTPUT_CONTRACT: &str = "\
OUTPUT: emit ONLY the replacement passage. No preamble, no closing remark, \
no markdown fence, no quotation marks wrapping the whole thing, no labels, \
no headers. The first character of your response is the first character of \
the replacement; the last character is its final punctuation mark.\n\
\n\
(\"No quotation marks wrapping the whole thing\" means don't put the ENTIRE \
replacement inside one pair of quotes. Dialogue spoken within the passage \
should still be wrapped in double quotes per the RP CONVENTIONS: He says \
\"You shouldn't be here,\" and steps back.)\n\
\n\
Length: scale to the highlight. A one-line highlight gets a one-line \
replacement; a full paragraph gets a full paragraph. Do not pad. Do not \
truncate.\n\
\n\
Do NOT include the <before_anchor> or <after_anchor> text in your output. \
The caller splices your response between them — echoing them would double \
the prose.";

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn req(before: &str, highlight: &str, after: &str) -> RegenerateSliceRequest {
        RegenerateSliceRequest {
            before: before.to_string(),
            highlight: highlight.to_string(),
            after: after.to_string(),
        }
    }

    // --- Prompt builder structure ---

    #[test]
    fn prompt_carries_all_blocks() {
        let p = build_regenerate_slice_system_prompt(&req("The door creaks.", "A figure steps through.", "Rain follows."));
        for tag in [
            "<role>",
            "<task>",
            "<output_contract>",
            "<passage_to_rewrite>",
            "<before_anchor>",
            "<highlighted_to_replace>",
            "<after_anchor>",
        ] {
            assert!(p.contains(tag), "missing {tag}");
        }
    }

    // --- Output contract ---

    #[test]
    fn output_contract_forbids_preamble() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("No preamble"));
        assert!(p.contains("no markdown fence"));
    }

    #[test]
    fn output_contract_forbids_quoting_the_whole_thing() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("no quotation marks wrapping the whole thing"));
    }

    #[test]
    fn output_contract_forbids_echoing_anchors() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("Do NOT include the <before_anchor>"));
    }

    #[test]
    fn output_contract_pins_length_scaling() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("scale to the highlight"));
    }

    // --- Task contract ---

    #[test]
    fn task_pins_second_person_present() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("second-person present"));
    }

    #[test]
    fn task_pins_flow_into_anchors() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("Flow seamlessly into the sentences immediately before and after"));
    }

    #[test]
    fn task_forbids_advancing_scene() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("Do NOT advance the scene"));
    }

    #[test]
    fn task_requires_meaningful_variation() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("genuinely different take"));
    }

    #[test]
    fn task_forbids_new_npcs_or_state() {
        let p = build_regenerate_slice_system_prompt(&req("a", "b", "c"));
        assert!(p.contains("Do NOT introduce new NPCs"));
    }

    // --- Slice folding ---

    #[test]
    fn prompt_folds_all_three_slices_verbatim() {
        let p = build_regenerate_slice_system_prompt(
            &req("BEFORE_TEXT", "HIGHLIGHT_TEXT", "AFTER_TEXT"),
        );
        assert!(p.contains("BEFORE_TEXT"));
        assert!(p.contains("HIGHLIGHT_TEXT"));
        assert!(p.contains("AFTER_TEXT"));
    }

    #[test]
    fn prompt_marks_empty_before_anchor() {
        let p = build_regenerate_slice_system_prompt(&req("", "h", "a"));
        assert!(p.contains("the highlight is at the very start of the beat"));
    }

    #[test]
    fn prompt_marks_empty_after_anchor() {
        let p = build_regenerate_slice_system_prompt(&req("b", "h", ""));
        assert!(p.contains("the highlight is at the very end of the beat"));
    }

    // --- Edge cases ---

    #[test]
    fn highlight_is_empty_detects_blank() {
        assert!(req("a", "", "c").highlight_is_empty());
        assert!(req("a", "   ", "c").highlight_is_empty());
        assert!(!req("a", " x ", "c").highlight_is_empty());
    }

    #[test]
    fn prompt_marks_blank_highlight_defensively() {
        let p = build_regenerate_slice_system_prompt(&req("a", "   ", "c"));
        assert!(p.contains("empty"));
        assert!(p.contains("[no highlight provided]"));
    }
}
