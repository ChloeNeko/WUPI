//! Ghost Writer: the composer's turn-shaping aids (Swipe / Continue /
//! Impersonate), 2026-08-22.
//!
//! Three narrator-side helpers live behind the composer's ghost icon:
//!
//! * **Swipe** is NOT implemented here — it is the existing `fable_send`
//!   reroll flow plus an optional player direction. `fable_send`'s
//!   `guidance` param carries the nudge into the narrator turn tail via
//!   [`swipe_direction`]; every other piece (variant stash, schema revert,
//!   re-track, streaming) is the untouched reroll machinery.
//! * **Continue** ([`beat_tail`] + [`continue_directive`] +
//!   [`merge_continuation`]): an API-only one-shot that extends the
//!   trailing beat from where it ends, then lands through the edit path
//!   (`apply_edit` + the assistant-edit re-track), so world state absorbs
//!   anything the continuation asserts. The directive quotes the beat's
//!   closing lines over an empty line so the continuation opens as a new
//!   paragraph at the exact seam the merge splices.
//! * **Impersonate** ([`impersonate_system_prompt`] + [`impersonate_task`]):
//!   an API-only one-shot that writes the player's NEXT message in the
//!   player's own voice; the result drops into the composer for review, it
//!   is never sent by itself.
//!
//! All three are pure prompt text + text hygiene here; the IPC commands in
//! `lib.rs` own state, guards, and assembly. Every prompt surface in this
//! module is positive-framing only and carries no em dash (the narrator
//! style invariant), pinned by the tests below.

/// Clamp a composer nudge: trim + char cap (never bytes — anti-pattern #6).
/// An empty result means "no nudge" (the caller treats it as absent).
pub fn cap_nudge(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed.chars().take(crate::settings::GUIDED_NUDGE_CHAR_CAP).collect()
}

/// The `<direction>` block riding a GUIDED reroll's narrator turn tail
/// (the `guidance` param of `fable_send`, rendered LAST in
/// `build_narrator_turn_tail`). The player's steer for the fresh variant,
/// stated once, positively.
pub fn swipe_direction(nudge: &str) -> String {
    format!(
        "<direction>\nThis beat takes a fresh path. The player's direction: {nudge}\n</direction>\n"
    )
}

/// The last `n` non-empty lines of a beat (trailing blank lines skipped),
/// in original order — the closing anchor [`continue_directive`] quotes so
/// the model knows exactly where the prose stops. Narrator beats keep one
/// paragraph per line, so two lines is the final paragraph plus its
/// predecessor: enough context to carry the voice, little enough to stay
/// an anchor rather than a second copy of the beat.
pub fn beat_tail(beat: &str, n: usize) -> String {
    let lines: Vec<&str> = beat.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The trailing system message for the continue one-shot. Rides after
/// `<world_state>` in the same last-message slot a normal turn tail uses:
/// the model sees the beat it is extending as the final history message,
/// then this directive tells it to pick up exactly there. The beat's
/// closing lines are quoted with an empty line after them, so the
/// continuation opens as a fresh paragraph at the exact seam
/// [`merge_continuation`] will splice (it joins on that same blank line).
pub fn continue_directive(nudge: &str, tail: &str) -> String {
    let mut out = format!(
        "<direction>\nContinue the final beat from the exact point where it ends. The player's direction: {nudge}\n"
    );
    let tail = tail.trim();
    if tail.is_empty() {
        out.push_str(
            "Write only the continuation, opening mid flow in the established voice.\n</direction>\n",
        );
        return out;
    }
    out.push_str(&format!(
        "The beat's closing lines:\n{tail}\n\nWrite only the continuation, opening mid flow in the established voice. Start it as a new paragraph after that empty line, picking up naturally from those closing lines.\n</direction>\n"
    ));
    out
}

/// Join an existing beat with its generated continuation. The continuation
/// is trimmed by the caller's cleaning pass; a blank line separates the
/// halves so the merged beat reads as two passages of one beat.
pub fn merge_continuation(existing: &str, continuation: &str) -> String {
    format!("{existing}\n\n{continuation}")
}

/// The system prompt for the impersonation one-shot. Deliberately NOT the
/// authored narrator voice: the ghostwriter writes as the PLAYER, so the
/// identity blocks (`<player>`, `<sim_card>`, `<world_state>`) ride this
/// directive instead, and the player's own prior messages in the window
/// carry the voice.
pub fn impersonate_system_prompt() -> String {
    [
        "You are the player's ghostwriter inside an ongoing story. You write the player's NEXT message and nothing else: their action, their words, their intent, from inside their skin.",
        "Laws:",
        "Write in first person as the player, always.",
        "Match the voice, tone, and formatting of the player's own earlier messages in the conversation.",
        "Stay inside what the player knows, owns, and can reach right now.",
        "Output only the message itself, ready to send.",
    ]
    .join("\n")
}

/// The final instruction for the impersonation one-shot. `nudge` is the
/// optional composer content steering what the message should contain or
/// pursue; absent it, the ghostwriter reads the scene and writes the
/// player's most natural next move.
pub fn impersonate_task(nudge: Option<&str>) -> String {
    let capped = nudge.map(cap_nudge).filter(|s| !s.is_empty());
    match capped {
        Some(n) => format!("Write the player's next message now. The player's steer: {n}"),
        None => "Write the player's next message now.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every prompt surface this module emits is em-dash-free (the narrator
    /// style invariant) — a stray em dash ships straight into model context.
    #[test]
    fn prompts_carry_no_em_dash() {
        let surfaces = [
            swipe_direction("make it darker"),
            continue_directive("the guard returns", "She sheathes her blade.\nThe torch gutters out."),
            continue_directive("the guard returns", ""),
            impersonate_system_prompt(),
            impersonate_task(None),
            impersonate_task(Some("flirt with the smith")),
        ];
        for s in surfaces {
            assert!(!s.contains('—'), "em dash leaked into prompt: {s}");
        }
    }

    #[test]
    fn cap_nudge_trims_and_caps_by_chars() {
        assert_eq!(cap_nudge("  hello  "), "hello");
        let long: String = "α".repeat(crate::settings::GUIDED_NUDGE_CHAR_CAP + 25);
        let capped = cap_nudge(&long);
        assert_eq!(
            capped.chars().count(),
            crate::settings::GUIDED_NUDGE_CHAR_CAP,
            "cap must count chars, not bytes"
        );
        assert_eq!(cap_nudge("   "), "");
    }

    #[test]
    fn impersonate_task_carries_the_steer_only_when_present() {
        assert!(impersonate_task(None).ends_with("now."));
        assert!(impersonate_task(Some("  ")).ends_with("now."));
        let steered = impersonate_task(Some("buy the map"));
        assert!(steered.contains("buy the map"));
    }

    #[test]
    fn merge_continuation_separates_with_a_blank_line() {
        assert_eq!(
            merge_continuation("She opens the door.", "Cold air rushes in."),
            "She opens the door.\n\nCold air rushes in."
        );
    }

    #[test]
    fn beat_tail_grabs_the_last_two_non_empty_lines() {
        let beat = "First paragraph.\n\nSecond paragraph.\n\nThird paragraph.\n";
        assert_eq!(beat_tail(beat, 2), "Second paragraph.\nThird paragraph.");
        // Trailing blank lines never enter the grab.
        assert_eq!(beat_tail("Only paragraph.\n\n", 2), "Only paragraph.");
        assert_eq!(beat_tail("   \n  \n", 2), "");
    }

    #[test]
    fn continue_directive_quotes_the_tail_over_the_paragraph_break() {
        let d = continue_directive(
            "the guard returns",
            "She sheathes her blade.\nThe torch gutters out.",
        );
        assert!(
            d.contains(
                "The beat's closing lines:\nShe sheathes her blade.\nThe torch gutters out.\n\n"
            ),
            "the tail must land verbatim with an empty line after it: {d}"
        );
        assert!(d.contains("new paragraph"));
    }

    #[test]
    fn continue_directive_without_a_tail_keeps_the_plain_directive() {
        let d = continue_directive("the guard returns", "  ");
        assert!(d.contains("Write only the continuation"));
        assert!(!d.contains("closing lines"));
    }
}
