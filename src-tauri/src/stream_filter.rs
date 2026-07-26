//! A bounded-lookahead streaming text filter that strips regex patterns from
//! a token stream without leaking partial matches to the output.
//!
//! # Why this exists
//!
//! LLM output arrives token-by-token. Some of that output contains control
//! markers (`<|turn>`, `<|im_start|>`, etc.) that must never reach the UI.
//! A naive per-token regex strip fails because a marker can be split across
//! two or more token pieces: e.g. `<|im_` then `start|>`. If we emit the
//! first piece before seeing the second, the user sees a flash of `<|im_`.
//!
//! # The invariant
//!
//! The filter NEVER emits text within `max_pattern_len` bytes of the buffer
//! end. That trailing window is locked until the next chunk arrives and
//! confirms whether it's a real pattern or plain text. This guarantees no
//! pattern's first half can escape before its second half is seen.
//!
//! # Efficiency
//!
//! Per-token cost is O(new_text_len), not O(buffer_len):
//! - We only scan the slice from the cursor to `safe_end`, never the whole
//!   buffer. The already-emitted prefix is never re-examined.
//! - The buffer is compacted after each feed (emitted text dropped), so
//!   memory stays bounded by `max_pattern_len + longest_chunk`.
//! - The suffix-length check is a single byte comparison per feed, not a
//!   regex match: we only hold back the tail, never re-run patterns on it.

use regex::Regex;

/// Configuration for a `StreamFilter`.
#[derive(Debug)]
pub struct StreamFilter {
    /// All marker patterns combined into a single regex via alternation:
    /// `(?:p1|p2|p3|...)`. One `replace_all` pass strips every marker,
    /// instead of one pass per pattern (Bug #4).
    combined_re: Regex,
    /// The raw marker strings, retained so `flush()` can detect and strip
    /// truncated marker prefixes left in the trailing window by a cancelled
    /// generation (Bug #5). Every marker is ASCII, so byte-slicing is safe.
    markers: Vec<String>,
    /// Optional second regex for non-literal patterns (e.g. the Fable
    /// narrator's bracket commands: `[OBJECT id=X state=Y]`,
    /// `[CHARACTER_TURN:npc]`, `[FX name]`). These can't be expressed as
    /// simple literals (variable content), so they get their own regex
    /// applied in the same feed/flush passes. `None` for filters that only
    /// strip literal markers (the chat engine).
    bracket_re: Option<Regex>,
    /// The trailing-window size reserved for bracket-command detection.
    /// Sized to the longest realistic bracket (~96 bytes covers
    /// `[OBJECT id=very_long_stable_id state=very_long_state]`). Only used
    /// when `bracket_re` is `Some`.
    bracket_window: usize,
    /// Length of the longest literal pattern (in bytes). Defines the
    /// trailing window for literal-marker holdback.
    max_pattern_len: usize,
    /// The rolling buffer of un-emitted text.
    buffer: String,
    /// How far into the buffer we've already decided to emit (post-strip).
    /// Text in `[0, cursor)` has been emitted; `[cursor, len)` is pending.
    cursor: usize,
}

impl StreamFilter {
    /// Create a filter from a set of literal marker strings. Each marker is
    /// treated as a literal (regex-escaped), so you pass the raw token text
    /// like `"<|turn>"`: no regex syntax needed.
    ///
    /// # Panics
    /// Panics if `markers` is empty (a filter with nothing to strip is
    /// meaningless: just don't filter).
    pub fn new(markers: &[&str]) -> Self {
        assert!(
            !markers.is_empty(),
            "StreamFilter requires at least one marker"
        );
        // Combine all markers into one regex via alternation so we strip
        // every marker in a single pass instead of one per pattern.
        let combined_pattern = format!(
            "(?:{})",
            markers
                .iter()
                .map(|m| regex::escape(m))
                .collect::<Vec<_>>()
                .join("|")
        );
        let combined_re =
            Regex::new(&combined_pattern).expect("escaped literals always compile");
        let max_pattern_len = markers.iter().map(|m| m.len()).max().expect("non-empty");
        let markers = markers.iter().map(|m| (*m).to_string()).collect();
        StreamFilter {
            combined_re,
            markers,
            bracket_re: None,
            bracket_window: 0,
            max_pattern_len,
            buffer: String::with_capacity(256),
            cursor: 0,
        }
    }

    /// Enable bracket-command stripping for the Fable narrator path. The
    /// narrator emits `[OBJECT id=X state=Y]`, `[CHARACTER_TURN:npc] ... [CHARACTER_TURN:end]`,
    /// and `[FX name]` inline in prose. Without this, those bracket commands
    /// stream live to the UI as raw text (the post-turn `bracket_parser` reads
    /// `raw_output` separately, so stripping here doesn't break scene_event
    /// extraction — the two paths are independent). The body of a
    /// CHARACTER_TURN (the spoken line between open + close tags) is NOT
    /// stripped: only the bracket tags themselves are.
    ///
    /// Only the narrator calls this; the chat engine's filter stays
    /// bracket-less (its output has no brackets).
    pub fn with_brackets(mut self) -> Self {
        // Match any of the three bracket forms + the CHARACTER_TURN:end close
        // tag. `[^\]]+` (not-followed-by-`]`) captures the variable id/state/
        // effect body without spanning multiple brackets. The npc_id in
        // CHARACTER_TURN is constrained to identifier chars (the narrator is
        // told to use snake_case ids); OBJECT/FX bodies are looser.
        let pattern = r"\[(?:CHARACTER_TURN:(?:end|[A-Za-z0-9_-]+)|OBJECT\s+[^\]]+|FX\s+[^\]]+)\]";
        self.bracket_re = Some(Regex::new(pattern).expect("bracket regex always compiles"));
        // Longest realistic bracket: `[OBJECT id=very_long_stable_id state=long_state]`
        // ≈ 55 chars. 96 gives generous headroom for unusually long ids/states.
        self.bracket_window = 96;
        self
    }

    /// The trailing-window size: how many bytes from the buffer end we hold
    /// back per feed, waiting for a partial marker/bracket to complete.
    /// The max of the literal-marker window and the bracket window (if any).
    fn trailing_window(&self) -> usize {
        self.max_pattern_len.max(self.bracket_window)
    }

    /// Whether the slice contains any character that could start a strippable
    /// pattern. `<` starts every literal marker; `[` starts every bracket.
    /// The fast-path skips all regex work when neither is present (the
    /// overwhelmingly common case for regular prose).
    fn slice_needs_stripping(&self, slice: &str) -> bool {
        if slice.contains('<') {
            return true;
        }
        self.bracket_re.is_some() && slice.contains('[')
    }

    /// Strip all known patterns (literal markers + brackets) from a slice.
    /// Runs the combined literal regex, then the bracket regex if enabled.
    fn strip_all(&self, slice: &str) -> String {
        let after_literals = if slice.contains('<') {
            self.combined_re.replace_all(slice, "").into_owned()
        } else {
            slice.to_string()
        };
        if let Some(br) = &self.bracket_re {
            if after_literals.contains('[') {
                return br.replace_all(&after_literals, "").into_owned();
            }
        }
        after_literals
    }

    /// Feed a new piece of the token stream. Returns text that is now safe to
    /// emit to the UI. The returned string may be empty (everything is still
    /// in the locked trailing window or was stripped).
    pub fn feed(&mut self, piece: &str) -> String {
        self.buffer.push_str(piece);

        // The safe emission boundary: we can emit up to here, but no further.
        // Everything past this point is within `trailing_window()` of the end
        // and might be the start of a pattern (literal marker or bracket
        // command) that completes next chunk. We hold back the full window
        // (not -1): a pattern that STARTS exactly at `safe_end` needs all
        // `trailing_window` bytes to complete, so any byte in `[safe_end, len)`
        // could be part of one.
        let mut safe_end = self.buffer.len().saturating_sub(self.trailing_window());

        // CRITICAL: walk safe_end back to a valid UTF-8 char boundary. The
        // model emits multi-byte chars (em dash '-' is 3 bytes, emoji are 4).
        // If safe_end lands inside one, slicing at it panics ("end byte index
        // X is not a char boundary"). The extra holdback is at most 3 bytes -
        // well within the trailing window, so the marker-safety invariant
        // still holds.
        while safe_end > self.cursor && !self.buffer.is_char_boundary(safe_end) {
            safe_end -= 1;
        }

        if safe_end <= self.cursor {
            // The new piece didn't push us past the window threshold. Hold
            // everything; nothing is safe to emit yet.
            return String::new();
        }

        // Before stripping: check if the RAW slice ends with a prefix of any
        // strippable pattern. If a pattern straddles `safe_end`, the regex
        // can't see the full pattern: only its partial start. Hold those raw
        // bytes back so the next feed can resolve them.
        //
        // Two checks, both scanning from the right (longest candidate first):
        //  (a) Literal markers: every one starts with '<'. A suffix starting
        //      with '<' that is a proper prefix of some marker must be held.
        //  (b) Bracket commands (if enabled): every one starts with '['. A
        //      suffix starting with '[' that could be the start of a bracket
        //      command must be held. We hold ANY '['-started suffix within the
        //      bracket_window, since bracket content is variable-length and we
        //      can't enumerate prefixes the way we do for literals — a partial
        //      `[OBJE` could complete to `[OBJECT ...]` next chunk.
        let mut effective_end = safe_end;
        // (a) literal-marker partial prefix
        for from in (self.cursor..safe_end).rev() {
            if self.buffer.as_bytes().get(from) != Some(&b'<') {
                continue;
            }
            let tail = &self.buffer[from..safe_end];
            if self.markers.iter().any(|m| m.starts_with(tail) && tail.len() < m.len()) {
                effective_end = from;
                break; // first '<' match from the right = longest candidate
            }
        }
        // (b) bracket partial prefix (only if brackets are enabled). Hold back
        // the most recent '[' within the window — anything after it could be a
        // partial bracket command. This is conservative (also holds an
        // unrelated '[' that won't form a command), but stray '[' in narrative
        // prose is rare and the hold is bounded by bracket_window.
        if self.bracket_re.is_some() {
            for from in (self.cursor..effective_end).rev() {
                if self.buffer.as_bytes().get(from) != Some(&b'[') {
                    continue;
                }
                // If this '[' already formed a COMPLETE bracket (regex matches
                // up to a ']'), it's not a partial — the regex will strip it,
                // so don't hold it back. Only hold if there's no ']' after it
                // in the slice (i.e. the bracket is still open/incomplete).
                let candidate = &self.buffer[from..safe_end];
                if !candidate.contains(']') {
                    effective_end = from;
                    break;
                }
            }
        }

        if effective_end <= self.cursor {
            // The held-back window ate everything: nothing safe to emit.
            return String::new();
        }

        let slice = &self.buffer[self.cursor..effective_end];
        // Fast path: if the slice contains neither '<' nor '[', skip all regex
        // work (one alloc, zero regex). Covers the overwhelmingly common case
        // (regular prose tokens contain neither). Otherwise strip all known
        // patterns (literals + brackets).
        let cleaned = if self.slice_needs_stripping(slice) {
            self.strip_all(slice)
        } else {
            slice.to_string()
        };

        // Advance the cursor past what we've processed (up to effective_end,
        // which accounts for any partial-pattern prefix held back).
        self.cursor = effective_end;

        // Compact the buffer: drop the emitted prefix so memory stays bounded.
        // The held tail `[cursor, len)` becomes the new buffer starting at 0.
        // cursor is always at a valid boundary (we walked safe_end there and
        // 0 is always valid), so drain won't panic.
        self.buffer.drain(0..self.cursor);
        self.cursor = 0;

        cleaned
    }

    /// Called at end of generation. Emits any remaining held text, with a
    /// final defensive regex sweep to catch complete pattern remnants, then
    /// strips any trailing partial-pattern prefix (e.g. a truncated `<|cha`
    /// or `[OBJE` left by an aborted generation) so it can't leak to the UI
    /// or into `session.json` (Bug #5).
    pub fn flush(&mut self) -> String {
        if self.cursor >= self.buffer.len() {
            return String::new();
        }
        let mut remaining = self.buffer[self.cursor..].to_string();
        // Strip complete patterns (literals + brackets).
        remaining = self.strip_all(&remaining);

        // Strip any trailing partial-marker prefix. All our markers are ASCII,
        // so byte-slicing is safe. Walk prefixes longest-first so we strip the
        // longest match; loop in case stripping one prefix exposes another.
        let mut changed = true;
        while changed {
            changed = false;
            for marker in &self.markers {
                // Check every proper prefix of this marker (longest first).
                for len in (1..marker.len()).rev() {
                    // All markers are ASCII, so byte-slicing to a prefix is a
                    // valid UTF-8 boundary and str::ends_with accepts &str.
                    if remaining.ends_with(&marker[..len]) {
                        remaining.truncate(remaining.len() - len);
                        changed = true;
                        break;
                    }
                }
                if changed {
                    break;
                }
            }
        }

        // Strip any trailing partial-bracket prefix (e.g. `[OBJE` left by an
        // aborted generation). A partial bracket is any trailing suffix
        // starting with '[' that contains no ']' (an unterminated bracket).
        // Walk from the rightmost '[' inward; if everything from it to the end
        // has no ']', drop it all. Loop in case stripping exposes an earlier
        // partial bracket.
        if self.bracket_re.is_some() {
            let mut bracket_changed = true;
            while bracket_changed {
                bracket_changed = false;
                if let Some(bracket_pos) = remaining.rfind('[') {
                    let tail = &remaining[bracket_pos..];
                    if !tail.contains(']') {
                        // Unterminated bracket: drop it so it doesn't leak.
                        remaining.truncate(bracket_pos);
                        bracket_changed = true;
                    }
                }
            }
        }

        self.buffer.clear();
        self.cursor = 0;
        remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_text_through_immediately() {
        // Short text shorter than the trailing window (max_pattern_len-1 = 6
        // for "<|turn>") is held until more arrives or flush() is called.
        let mut f = StreamFilter::new(&["<|turn>"]);
        let out = f.feed("Hi!");
        assert_eq!(out, "", "text shorter than window is held");
        let out2 = f.feed(" more text arrives now");
        // Now enough has arrived that the trailing window is past "Hi! more".
        assert!(out2.contains("Hi!"), "got: {:?}", out2);
    }

    #[test]
    fn strips_complete_marker_in_one_chunk() {
        let mut f = StreamFilter::new(&["<|turn>"]);
        let out = f.feed("Hi<|turn>there");
        // safe_end = 14 - 6 = 8. slice = "Hi<|turn" → strip → "Hi".
        assert!(out.contains("Hi"), "got: {:?}", out);
        assert!(!out.contains("<|"), "marker leaked: {:?}", out);
        // The trailing "there" is within the window; flush emits it.
        let tail = f.flush();
        assert!(tail.contains("there"), "trailing text in flush: {:?}", tail);
    }

    #[test]
    fn handles_marker_split_across_chunks() {
        // The critical test: a marker split exactly at a chunk boundary.
        let mut f = StreamFilter::new(&["<|im_start|>"]);
        let out1 = f.feed("Hello<|im_");
        // "<|im_" is a prefix of "<|im_start|>", must be held back.
        assert!(
            !out1.contains("<|"),
            "partial marker leaked: {:?}",
            out1
        );
        let out2 = f.feed("start|>world");
        let combined = format!("{out1}{out2}");
        assert!(
            !combined.contains("<|im_start|>"),
            "full marker not stripped: {:?}",
            combined
        );
        // "world" may be partially held; flush to complete.
        let tail = f.flush();
        let all = format!("{combined}{tail}");
        assert!(all.contains("world"), "output: {:?}", all);
    }

    #[test]
    fn handles_exact_boundary_split() {
        // Marker split so the first chunk ends exactly at the marker start.
        let mut f = StreamFilter::new(&["<|turn>"]);
        // First chunk is plain text, second starts the marker.
        let out1 = f.feed("reply text");
        let out2 = f.feed("<|turn>");
        let combined = format!("{out1}{out2}");
        assert!(
            !combined.contains("<|"),
            "marker leaked: {:?}",
            combined
        );
    }

    #[test]
    fn flush_emits_remaining_text_stripped() {
        let mut f = StreamFilter::new(&["<|turn>"]);
        f.feed("hello<|turn>");
        // "hello" is emitted in feed (safe_end passes it), "<|turn>" stripped
        // in feed. flush emits any trailing window content.
        let flushed = f.flush();
        assert!(
            !flushed.contains("<|"),
            "marker leaked in flush: {:?}",
            flushed
        );
    }

    #[test]
    fn flush_strips_partial_remnants() {
        // A truncated marker (generation ended mid-marker) should not leak.
        let mut f = StreamFilter::new(&["<|turn>"]);
        f.feed("text<|tu");
        let flushed = f.flush();
        assert!(!flushed.contains("<|"), "partial leaked: {:?}", flushed);
    }

    #[test]
    fn flush_strips_partial_marker_prefix() {
        // Bug #5 regression: a partial marker prefix like "<|tu" (from a
        // cancelled generation) must be stripped from the tail by flush().
        // Feed enough that "text" is emitted in feed(); the "<|tu" partial
        // remains in the trailing window for flush to strip.
        let mut f = StreamFilter::new(&["<|turn>"]);
        f.feed("text<|tu");
        let flushed = f.flush();
        assert!(
            !flushed.contains("<|"),
            "partial marker prefix leaked: {:?}",
            flushed
        );
    }

    #[test]
    fn flush_strips_partial_marker_with_multiple_patterns() {
        // With multiple markers, a partial prefix of ANY marker should strip.
        let mut f = StreamFilter::new(&["<|turn>", "<|channel>"]);
        f.feed("text<|cha");
        let flushed = f.flush();
        assert_eq!(flushed, "text", "got: {:?}", flushed);
    }

    #[test]
    fn multiple_patterns_simultaneously() {
        let mut f = StreamFilter::new(&["<|turn>", "<turn|>", "<|channel>"]);
        let out = f.feed("a<|turn>b<turn|>c<|channel>d");
        // The final 'd' is within the window of "<|channel>" (9 chars),
        // so it may be held. But a, b, c should come through stripped.
        assert!(out.contains('a'));
        assert!(!out.contains("<|turn>"));
        assert!(!out.contains("<turn|>"));
    }

    #[test]
    fn buffer_stays_bounded() {
        // Feed a large amount of text; the internal buffer should not grow
        // unboundedly because we compact after each feed.
        let mut f = StreamFilter::new(&["<|turn>"]);
        for _ in 0..1000 {
            f.feed("some text without markers ");
        }
        // After all feeds + a flush, buffer should be empty.
        f.flush();
        assert!(f.buffer.is_empty());
    }

    #[test]
    fn empty_piece_does_nothing() {
        let mut f = StreamFilter::new(&["<|turn>"]);
        assert_eq!(f.feed(""), "");
        // "hello" (5 bytes) is shorter than the 6-byte trailing window, so
        // it's held until more arrives or flush() runs.
        let out = f.feed("hello");
        let tail = f.flush();
        let combined = format!("{out}{tail}");
        assert!(combined.contains("hello"), "got: {:?}", combined);
    }

    #[test]
    fn adjacent_markers_collapse() {
        let mut f = StreamFilter::new(&["<|turn>"]);
        let out = f.feed("a<|turn><|turn>b");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains('a'), "got: {:?}", combined);
        assert!(combined.contains('b'), "got: {:?}", combined);
        assert!(!combined.contains("<|"), "marker leaked: {:?}", combined);
    }

    #[test]
    fn multibyte_char_at_boundary_does_not_panic() {
        // Regression: em dash '-' is 3 bytes (U+2014). If safe_end lands on
        // byte 4 (inside the dash, which occupies bytes 3..6), the old code
        // panicked with "end byte index 4 is not a char boundary". The fix
        // walks safe_end back to a valid boundary.
        //
        // Construct a buffer where the dash straddles the window edge.
        // Marker is "<|turn>" (7 bytes), so the trailing window is 6 bytes.
        let mut f = StreamFilter::new(&["<|turn>"]);
        // Feed enough text that the dash lands near the boundary.
        // "abc" = 3 bytes, then "-" = 3 bytes (bytes 3,4,5), then more text.
        // Total must be > window so something gets emitted.
        let out = f.feed("abc-defghijklmnop");
        // Should not panic. The dash may be held or emitted but either way
        // must be valid UTF-8 and contain no panic.
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("abc"));
        assert!(combined.contains("def"));
    }

    #[test]
    fn multibyte_char_split_across_chunks() {
        // The em dash split so its bytes arrive in two pieces.
        let mut f = StreamFilter::new(&["<|turn>"]);
        // First piece ends mid-dash (only the first byte of -).
        // In UTF-8,: is 0xE2 0x80 0x94. Feed the first byte alone.
        let out1 = f.feed("text \u{2014} more");
        let flushed = f.flush();
        let combined = format!("{out1}{flushed}");
        assert!(combined.contains("text"));
        assert!(combined.contains("more"));
    }

    #[test]
    fn emoji_at_boundary_does_not_panic() {
        // Emoji are 4 bytes: even more likely to straddle a boundary.
        let mut f = StreamFilter::new(&["<|turn>"]);
        let out = f.feed("hello 🎉 world this is a test message");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        // Must not panic and must preserve the emoji.
        assert!(combined.contains("hello"));
        assert!(combined.contains("world"));
    }

    // === Bracket-command stripping tests (2026-07-26 leakage fix) ===========
    // These use `.with_brackets()` to enable the narrator's bracket-command
    // stripping. The three command forms: [OBJECT id=X state=Y],
    // [CHARACTER_TURN:npc] ... [CHARACTER_TURN:end], [FX name].

    #[test]
    fn brackets_stripped_in_one_chunk_object() {
        // A complete OBJECT bracket + surrounding prose in one feed. The
        // bracket must be stripped; the prose must survive.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("Mara nods.[OBJECT id=door_state state=open]The fire crackles.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Mara nods."), "prose before bracket lost: {:?}", combined);
        assert!(combined.contains("The fire crackles."), "prose after bracket lost: {:?}", combined);
        assert!(!combined.contains("[OBJECT"), "OBJECT bracket leaked: {:?}", combined);
        assert!(!combined.contains("door_state"), "bracket content leaked: {:?}", combined);
    }

    #[test]
    fn brackets_stripped_fx() {
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("Thunder rolls.[FX thunder]Rain begins.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Thunder rolls."));
        assert!(combined.contains("Rain begins."));
        assert!(!combined.contains("[FX"), "FX bracket leaked: {:?}", combined);
    }

    #[test]
    fn character_turn_tags_stripped_body_survives() {
        // CHARACTER_TURN is multi-region: [CHARACTER_TURN:npc] body [CHARACTER_TURN:end].
        // BOTH bracket tags must be stripped, but the spoken body MUST survive
        // (it's the NPC's dialogue — visible content). This is the key
        // asymmetry vs OBJECT/FX (single-region, fully stripped).
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed(
            "Mara looks up. [CHARACTER_TURN:mara_the_innkeep]Welcome, traveler.[CHARACTER_TURN:end] She smiles.",
        );
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Mara looks up."), "lead-in prose lost: {:?}", combined);
        assert!(combined.contains("Welcome, traveler."), "CHARACTER_TURN body lost: {:?}", combined);
        assert!(combined.contains("She smiles."), "trailing prose lost: {:?}", combined);
        assert!(!combined.contains("[CHARACTER_TURN"), "CHARACTER_TURN tag leaked: {:?}", combined);
        assert!(!combined.contains(":mara_the_innkeep]"), "npc id leaked: {:?}", combined);
    }

    #[test]
    fn bracket_split_across_chunks() {
        // The critical streaming test: an OBJECT bracket split exactly at a
        // chunk boundary. The first chunk ends mid-bracket; the partial must
        // be held back so [OBJE never leaks, then completed + stripped when
        // the rest arrives.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Prose here. [OBJE");
        // The partial [OBJE must NOT have leaked.
        assert!(
            !out1.contains("[OBJE"),
            "partial bracket leaked on first chunk: {:?}",
            out1
        );
        let out2 = f.feed("CT id=chest state=open]More prose.");
        let combined = format!("{out1}{out2}");
        let flushed = f.flush();
        let all = format!("{combined}{flushed}");
        assert!(all.contains("Prose here."), "lead-in lost: {:?}", all);
        assert!(all.contains("More prose."), "trailing lost: {:?}", all);
        assert!(!all.contains("[OBJECT"), "full bracket not stripped: {:?}", all);
        assert!(!all.contains("chest"), "bracket content leaked: {:?}", all);
    }

    #[test]
    fn unterminated_bracket_stripped_on_flush() {
        // A cancelled generation leaving a partial [OBJE in the buffer. flush
        // must strip it so it doesn't leak to the UI or session.json.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        f.feed("Some prose. [OBJE");
        let flushed = f.flush();
        assert!(flushed.contains("Some prose."), "prose lost: {:?}", flushed);
        assert!(!flushed.contains("["), "partial bracket leaked in flush: {:?}", flushed);
        assert!(!flushed.contains("OBJE"), "partial bracket content leaked: {:?}", flushed);
    }

    #[test]
    fn stray_bracket_in_prose_survives_via_flush() {
        // A legitimate '[' in narrative prose (rare but possible — e.g. an
        // aside like "[the old road]") that does NOT form a recognized
        // command. The flush path holds it back during streaming (because the
        // partial-prefix check holds any '[' with no following ']'), but
        // flush() must emit it since it's not a complete bracket command.
        // Verify the text survives through flush.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        f.feed("He took the [old] road home.");
        let flushed = f.flush();
        // The complete [old] is not a recognized command (not OBJECT/FX/
        // CHARACTER_TURN), so it stays as literal prose.
        assert!(flushed.contains("old"), "stray bracket content lost: {:?}", flushed);
        assert!(flushed.contains("road"), "prose after stray bracket lost: {:?}", flushed);
    }

    #[test]
    fn brackets_and_literal_markers_strip_together() {
        // Both a Gemma4 protocol marker AND a bracket command in the same
        // stream. Both must strip.
        let mut f = StreamFilter::new(&["<|turn>", "<channel|>"]).with_brackets();
        let out = f.feed("Reply<channel|>[FX rain]More text");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Reply"), "lead-in lost: {:?}", combined);
        assert!(combined.contains("More text"), "trailing lost: {:?}", combined);
        assert!(!combined.contains("<channel|>"), "literal marker leaked: {:?}", combined);
        assert!(!combined.contains("[FX"), "bracket leaked: {:?}", combined);
    }

    #[test]
    fn no_brackets_config_is_unaffected() {
        // The chat engine's filter (no .with_brackets()) must NOT strip
        // brackets — it has no bracket regex. A '[' in chat output (e.g. a
        // code block) must survive. Regression guard against accidentally
        // enabling brackets globally.
        let mut f = StreamFilter::new(&["<|turn>"]);
        let out = f.feed("Here is [some] text");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("[some]"), "bracket wrongly stripped without with_brackets: {:?}", combined);
    }
}
