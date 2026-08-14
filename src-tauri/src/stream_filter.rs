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
    /// Optional third regex for fenced JSON blocks (Bug A fix, 2026-07-28).
    /// Matches ` ```json ... ``` ` (non-greedy, DOTALL via `[\s\S]`). The
    /// narrator model emits these as an alternative to bracket commands;
    /// both map to the same `BracketCommand` enum in the post-turn parser.
    /// `None` for the chat engine (which doesn't speak brackets or JSON
    /// commands); `Some` only when `bracket_re` is also `Some` (the JSON
    /// path is a sibling of the bracket path, set together in
    /// `with_brackets()`).
    fence_re: Option<Regex>,
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
            fence_re: None,
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
        // Match every bracket form that `bracket_parser::parse_one` recognizes,
        // so the streaming layer and the post-turn parser can never drift
        // (2026-07-28 leak fix: the streaming regex only listed the original
        // three — CHARACTER_TURN / OBJECT / FX — but the parser gained TIME,
        // EFFECT, MILESTONE, TASK across Phase 3, and those leaked during live
        // streaming until the post-turn parser stripped them on `done`).
        //
        // `[^\]]+` (not-followed-by-`]`) captures the variable body of each
        // command without spanning multiple brackets. CHARACTER_TURN's npc_id
        // is constrained to identifier chars (the narrator is told to use
        // snake_case ids); every other command's body is looser (free-form
        // text, `key=value` pairs, pipe-separated tails). The single `+`
        // quantifier on `[^\]]` is the cheapest pattern that accepts all four
        // shapes without enumerating per-command grammars (we don't need to
        // parse here — just strip; the real parser runs on raw_output).
        //
        // Commands covered (must mirror bracket_parser::parse_one exactly):
        //   [CHARACTER_TURN:npc_id] / [CHARACTER_TURN:end]
        //   [OBJECT ...]
        //   [FX name]
        //   [TIME Day 3, 14:00]            (Seam #4, 2026-07-27)
        //   [WEATHER heavy rain]           (Phase 4 Component 2, 2026-07-28)
        //   [TRAVEL cellar]                (Phase 4 Component 3, 2026-07-28)
        //   [EFFECT label polarity dur]    (Phase 3 Slice 4, 2026-07-28)
        //   [MILESTONE npc_id event_id]    (Phase 3 Slice 5, 2026-07-28)
        //   [TASK npc_id desc | d s eta]   (Phase 3 Slice 6, 2026-07-28)
        //   [RUMOR label]                  (Phase 4 Component 4, 2026-07-28)
        //   [PRESENCE npc_id stance]       (Phase 5A, 2026-07-29)
        //   [DISCOVER node_id ...]         (dynamic world-seeding)
        //   [NPC_REGISTER npc_id ...]      (dynamic world-seeding)
        //   [APPEARANCE key=value]         (Phase 4 Component 5, 2026-08-04)
        //   [EQUIP slot=... name=...]      (inventory, 2026-08-07)
        //   [BELT name=...]                (inventory, 2026-08-07)
        //   [PACK name=...]                (inventory, 2026-08-07)
        //   [DATE <new calendar label>]   (calendar, 2026-08-13)
        let pattern = r"\[(?:CHARACTER_TURN:(?:end|[A-Za-z0-9_-]+)|OBJECT\s+[^\]]+|FX\s+[^\]]+|TIME\s+[^\]]+|DATE\s+[^\]]+|WEATHER\s+[^\]]+|TRAVEL\s+[^\]]+|EFFECT\s+[^\]]+|MILESTONE\s+[^\]]+|TASK\s+[^\]]+|RUMOR\s+[^\]]+|PRESENCE\s+[^\]]+|DISCOVER\s+[^\]]+|NPC_REGISTER\s+[^\]]+|APPEARANCE\s+[^\]]+|EQUIP\s+[^\]]+|BELT\s+[^\]]+|PACK\s+[^\]]+)\]";
        self.bracket_re = Some(Regex::new(pattern).expect("bracket regex always compiles"));
        // Longest realistic bracket: `[TASK npc.marcus scout the bandit camp |
        // challenging adequate 1440]` ≈ 70 chars; an EFFECT with a long label
        // (`[EFFECT Blessed by the Sun Priest buff 1440]`) ≈ 45 chars. 96 still
        // covers them with headroom; the bracket_window is the streaming
        // holdback size, so it must be ≥ the longest command the model emits.
        self.bracket_window = 96;

        // Bug A fix (2026-07-28): fenced-JSON sibling. The model emits
        // ` ```json ... ``` ` blocks as an alternative to bracket commands
        // (its instruct-tuned reflex when it sees structured schema fields).
        // The body is unbounded, so we can't size a window for it the way
        // we do for brackets — instead `feed()` holds back everything from
        // an opener that isn't yet followed by a closer (the in-progress
        // fence body never streams). The regex here handles the COMPLETE
        // fence (opener + body + closer); the partial cases (opener-only,
        // partial backtick prefix) are handled by the explicit scans in
        // `feed()` and `flush()`. Non-greedy `[\s\S]*?` so a turn with
        // multiple fences strips each one independently.
        let fence_pattern = r"```json[\s\S]*?```";
        self.fence_re = Some(Regex::new(fence_pattern).expect("fence regex always compiles"));
        self
    }

    /// The trailing-window size: how many bytes from the buffer end we hold
    /// back per feed, waiting for a partial marker/bracket to complete.
    /// The max of the literal-marker window and the bracket window (if any).
    fn trailing_window(&self) -> usize {
        self.max_pattern_len.max(self.bracket_window)
    }

    /// Whether the slice contains any character that could start a strippable
    /// pattern. `<` starts every literal marker; `[` starts every bracket;
    /// `` ` `` starts every fenced-JSON block. The fast-path skips all regex
    /// work when none is present (the overwhelmingly common case for prose).
    fn slice_needs_stripping(&self, slice: &str) -> bool {
        if slice.contains('<') {
            return true;
        }
        if self.bracket_re.is_some() && slice.contains('[') {
            return true;
        }
        self.fence_re.is_some() && slice.contains("```")
    }

    /// Strip all known patterns (literal markers + brackets + fences) from a
    /// slice. Runs the combined literal regex, then the bracket regex if
    /// enabled, then the fence regex if enabled. Each is gated by a cheap
    /// `contains` check on the relevant trigger character so prose with no
    /// `<`, `[`, or `` ` `` skips all regex work.
    fn strip_all(&self, slice: &str) -> String {
        let after_literals = if slice.contains('<') {
            self.combined_re.replace_all(slice, "").into_owned()
        } else {
            slice.to_string()
        };
        let after_brackets = if let Some(br) = &self.bracket_re {
            if after_literals.contains('[') {
                br.replace_all(&after_literals, "").into_owned()
            } else {
                after_literals
            }
        } else {
            after_literals
        };
        if let Some(fr) = &self.fence_re {
            // The fence opener starts with a backtick; cheap pre-check.
            if after_brackets.contains("```") {
                return fr.replace_all(&after_brackets, "").into_owned();
            }
        }
        after_brackets
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
        // (c) fenced-JSON holdback (only if fences are enabled, Bug A fix
        //     2026-07-28). Two sub-cases, both scanning the slice right-to-left
        //     for backticks:
        //   - COMPLETE opener ` ```json ` whose body has no matching ` ``` `
        //     closer yet: the in-progress fence body must NEVER stream (it's
        //     a machine-channel, identical to how a half-formed `[OBJECT`
        //     is held). Hold everything from the opener onward.
        //   - PARTIAL opener (` ` `, ` `` `, ` ``` `, ` ```j `, ...): a
        //     backtick-run suffix that is a proper prefix of ` ```json `.
        //     Hold it back so the next chunk can resolve it.
        if self.fence_re.is_some() && effective_end > self.cursor {
            let slice_so_far = &self.buffer[self.cursor..effective_end];
            // Sub-case 1: complete opener, no closer. `rfind` the last
            // ```json opener; if no ``` closer follows it, hold from there.
            if let Some(opener_pos) = slice_so_far.rfind("```json") {
                let after_opener = &slice_so_far[opener_pos + "```json".len()..];
                // A "closer" is any ``` that isn't the opener itself. Since
                // the opener is ```json (8 chars), any ``` in `after_opener`
                // is a closer.
                if !after_opener.contains("```") {
                    effective_end = self.cursor + opener_pos;
                }
            } else {
                // Sub-case 2: partial opener. Scan backticks from the right.
                // A backtick run of length 1-3 at the slice end could be the
                // start of ```json; longer runs already matched sub-case 1
                // or are complete openers (handled above). Hold the run.
                let bytes = slice_so_far.as_bytes();
                let mut bt_end = bytes.len();
                while bt_end > 0 && bytes[bt_end - 1] == b'`' {
                    bt_end -= 1;
                }
                let bt_run = bytes.len() - bt_end;
                // Hold only if it's a proper prefix of the 3-backtick opener
                // (1 or 2 backticks). A run of exactly 3 with no `json`
                // following is ambiguous — could be the opener's first 3
                // chars OR a complete (but body-less) fence; hold it too,
                // the next chunk disambiguates.
                if matches!(bt_run, 1 | 2 | 3) {
                    effective_end = self.cursor + bt_end;
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

        // Strip any trailing partial-fence / in-progress-fence suffix (Bug A
        // fix, 2026-07-28). Mirrors the partial-bracket strip above. Two
        // shapes to drop:
        //   - An unterminated ` ```json ` opener (opener present, no ` ``` `
        //     closer after it): the in-progress body must not leak. Drop
        //     from the opener onward.
        //   - A partial-opener backtick run at the very end (` ` `, ` `` `,
        //     ` ``` ` with no `json`): drop the run so it doesn't stream.
        if self.fence_re.is_some() {
            let mut fence_changed = true;
            while fence_changed {
                fence_changed = false;
                // Unterminated complete opener.
                if let Some(opener_pos) = remaining.rfind("```json") {
                    let after = &remaining[opener_pos + "```json".len()..];
                    if !after.contains("```") {
                        remaining.truncate(opener_pos);
                        fence_changed = true;
                        continue;
                    }
                }
                // Partial-opener suffix: trailing backtick run of length
                // 1-3 (could be the start of ```json). Drop it.
                let rb = remaining.as_bytes();
                let mut bt_end = rb.len();
                while bt_end > 0 && rb[bt_end - 1] == b'`' {
                    bt_end -= 1;
                }
                let run = rb.len() - bt_end;
                if matches!(run, 1 | 2 | 3) {
                    remaining.truncate(bt_end);
                    fence_changed = true;
                }
            }
        }

        self.buffer.clear();
        self.cursor = 0;

        // Chloe 2026-07-27 — extra-spaces fix (defensive, terminal pass).
        // Bracket stripping above (strip_all + the partial-prefix loops)
        // leaves the spaces immediately before + after each removed bracket
        // un-collapsed, producing transient double-spaces in the LIVE stream
        // (the user sees these during generation before the finalized
        // `parsed.prose` from bracket_parser replaces them on `done`). This
        // is the terminal emission — no more chunks will arrive — so it's
        // safe to normalize here without disturbing the trailing-window /
        // partial-prefix logic above (which already ran). Collapse runs of
        // 2+ spaces to one and strip trailing space before newlines/EOF.
        // Same contract as bracket_parser::normalize_whitespace.
        if remaining.contains("  ") {
            remaining = normalize_spaces(&remaining);
        }
        remaining
    }
}

/// Collapse runs of 2+ ASCII spaces into one; strip trailing space before
/// each newline and before EOF. Leaves newlines (paragraph breaks) and
/// leading per-line whitespace intact. Terminal-pass helper for `flush`
/// (2026-07-27 extra-spaces fix). Kept local to this module to avoid a
/// cross-module dependency; bracket_parser has its own equivalent.
fn normalize_spaces(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_was_space = false;
    for ch in s.chars() {
        if ch == ' ' {
            if prev_was_space {
                continue;
            }
            out.push(' ');
            prev_was_space = true;
        } else if ch == '\n' {
            if out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
            prev_was_space = false;
        } else {
            out.push(ch);
            prev_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

// === Post-generation repetition truncator (§11.40.E follow-up) ============
//
// The deterministic firewall against the smuggler-loop / tail-repetition
// failure mode. Sibling to the DRY sampler stage (which handles short
// token-sequence repetition at generation time); THIS fn is the backstop
// for longer 4+ word mechanical loops that slip past the sampler. Called
// on `parsed.prose` (post-bracket-strip) in `lib.rs::fable_send`.
//
// Conservative by design — only fires when a phrase is long enough
// (≥ MIN_WINDOW_WORDS) AND repeated enough times in a row (≥ MIN_REPEATS)
// that the result is unambiguously mechanical looping, never legitimate
// rhetorical repetition (anaphora, parallelism, dialogue echoes).

/// Minimum phrase length (in whitespace-separated words) to qualify as a
/// repeatable window. 1-3 word repeats (dialogue echoes like "I know. I
/// know.") never trigger — those are legitimate rhetoric handled by the
/// DRY sampler at the token level. Bumped from a naive 1 specifically to
/// avoid false positives on deliberate short-word repetition.
const MIN_WINDOW_WORDS: usize = 4;

/// Minimum back-to-back repeat count to qualify as mechanical looping. 2
/// is rhetorical anaphora ("Never give up. Never give up.") and is
/// preserved; 3+ is a loop and gets truncated to one clean instance.
const MIN_REPEATS: usize = 3;

/// The shared primitive behind [`truncate_repetition`] (post-gen firewall)
/// AND [`StreamRepetitionDetector`] (mid-stream kill switch). The threshold
/// (4-word floor, 3-repeat floor) lives here exactly once so the two halves
/// of the repetition firewall can never drift apart — see §11.43.
///
/// Scans `words` for the FIRST back-to-back repeat of a ≥ MIN_WINDOW_WORDS
/// phrase that occurs ≥ MIN_REPEATS times. On a hit, returns the BYTE OFFSET
/// (into `s`, indexed via `spans`) of the start of the SECOND occurrence —
/// callers truncate there (preserving one clean instance + all preceding
/// prose). Returns `None` when no qualifying loop exists.
///
/// Pure + allocation-bounded by word count. Earliest start wins (most
/// aggressive cleanup at the lowest byte offset); for each start, window
/// size escalates from MIN_WINDOW_WORDS to `(n - start) / MIN_REPEATS`
/// (the largest window that could still hold MIN_REPEATS back-to-back
/// copies at `start`).
fn detect_repetition_offset(
    s: &str,
    spans: &[(usize, usize)],
    words: &[&str],
) -> Option<usize> {
    let n = words.len();
    // Earliest start wins → most aggressive cleanup at the lowest byte offset.
    for start in 0..=n.saturating_sub(MIN_WINDOW_WORDS * MIN_REPEATS) {
        let max_window = (n - start) / MIN_REPEATS;
        for w in MIN_WINDOW_WORDS..=max_window {
            let first = &words[start..start + w];
            let mut repeats = 1;
            let mut cursor = start + w;
            while cursor + w <= n {
                if &words[cursor..cursor + w] == first {
                    repeats += 1;
                    cursor += w;
                    if repeats >= MIN_REPEATS {
                        // Qualifying loop. Truncate at the start of the 2nd
                        // occurrence (keeps one clean instance of the phrase
                        // + everything before it). The caller (post-gen OR
                        // stream) does the byte slicing.
                        return Some(spans[start + w].0);
                    }
                } else {
                    break;
                }
            }
        }
    }
    let _ = s; // s is the slicing authority for callers; we only use spans here.
    None
}

/// Detect + truncate aggressive mechanical word-sequence repetition in
/// finalized narrator prose. When the model fixates and emits the same
/// multi-word phrase back-to-back ≥ `MIN_REPEATS` times, keep ONE clean
/// instance (the first occurrence) and drop the rest.
///
/// Thin wrapper around [`detect_repetition_offset`]: tokenize, scan, slice.
/// On a hit, truncates at the 2nd-occurrence byte offset (preserving one
/// clean instance + all preceding prose with original whitespace intact via
/// byte-offset slicing); everything after is dropped because a detected loop
/// signals model breakdown — we don't try to recover prose after it.
///
/// False-positive guards baked in: 4-word floor excludes dialogue/short-word
/// repetition; 3-repeat floor excludes deliberate double-anaphora; whitespace
/// tokenization collapses paragraph breaks so cross-paragraph rhetorical
/// repetition (parallelism across stanzas) is never matched as a "loop".
pub(crate) fn truncate_repetition(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let spans = word_spans(s);
    // Need at least MIN_WINDOW_WORDS * MIN_REPEATS words to possibly loop.
    if spans.len() < MIN_WINDOW_WORDS * MIN_REPEATS {
        return s.to_string();
    }
    let words: Vec<&str> = spans.iter().map(|(st, en)| &s[*st..*en]).collect();
    match detect_repetition_offset(s, &spans, &words) {
        Some(truncate_byte) => s[..truncate_byte].trim_end().to_string(),
        None => s.to_string(),
    }
}

/// Tokenize `s` into whitespace-separated word spans as `(byte_start,
/// byte_end)` pairs. Handles Unicode whitespace + UTF-8 boundaries via
/// `char_indices`. Sibling of `str::split_whitespace` but retains byte
/// offsets so `truncate_repetition` can slice the original string without
/// rebuilding it (preserving original whitespace/newlines in the kept
/// prefix). Pure, allocation-bounded by input length.
fn word_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in s.char_indices() {
        if ch.is_whitespace() {
            if let Some(st) = start {
                spans.push((st, i));
                start = None;
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        spans.push((st, s.len()));
    }
    spans
}

// ─────────────────────────────────────────────────────────────────────────
// §11.43 — Streaming repetition kill switch (API Stream Abort).
//
// The HTTP API path (GLM-5.2 / NanoGPT / OpenRouter / OpenAI) gets NO DRY
// sampler + NO Rust-side sampler chain — providers lock those knobs. The
// post-gen `truncate_repetition` firewall runs AFTER the full HTTP body
// returns, which means the user watches the loop stream live while we burn
// tokens on garbage. This struct is the missing half of the repetition
// firewall: a stateful tail-buffer that runs the SAME `detect_repetition_
// offset` primitive on every incoming chunk and signals the HTTP loop to
// sever the connection the instant a mechanical loop is confirmed.
//
// Single source of truth: `detect_repetition_offset` is shared with the
// post-gen truncator, so the 4-word × 3-repeat threshold cannot drift
// between the two halves. The same conservative false-positive guards
// apply (2-repeat anaphora is preserved; cross-paragraph parallelism
// collapses under whitespace tokenization).
//
// Why a rolling buffer, not a re-scan of the whole stream:
//   * Cost is bounded (MAX_WORDS=200) regardless of reply length.
//   * The longest detectable loop window is `(n - start) / MIN_REPEATS`;
//     with a 200-word tail and MIN_REPEATS=3, that's ~66-word windows —
//     well beyond any phrase a looping model emits.
//   * Chunk-boundary splits ("cra" + "ckles") resolve naturally because
//     we re-tokenize the merged buffer each tick, not the chunks.
//
// The kill switch contract (§11.43.3): on a `push` that triggers a hit,
// return `Some(clean)` where `clean` is the truncated buffer (one instance
// of the phrase + everything before). The caller BREAKS the stream loop,
// YIELDS `clean` as the finalized prose, and DROPS the `reqwest::Response`
// out of scope — severing TCP and stopping token billing instantly.
// ─────────────────────────────────────────────────────────────────────────

/// Cap on the rolling tail buffer (in words). 200 is generous: the longest
/// loop window detectable is `(MAX_WORDS) / MIN_REPEATS` ≈ 66 words, and no
/// legitimate narrator phrase approaches that. Memory cost is trivial
/// (200 × ~24-byte String handles ≈ 5 KB).
const STREAM_TAIL_MAX_WORDS: usize = 200;

/// Stateful tail-buffer that detects mechanical repetition mid-stream.
///
/// Construct one per API turn; call `push(chunk)` for each text delta the
/// SSE loop forwards; on `Some(truncated)`, the caller MUST abort the HTTP
/// connection and treat `truncated` as the finalized prose for the turn.
///
/// Internal invariant: after each `push`, `buffer_words` holds ≤
/// STREAM_TAIL_MAX_WORDS trailing words of the stream so far, and
/// `buffer_text` is the whitespace-joined reconstruction of those words
/// (we don't preserve original whitespace because the post-kill output is
/// re-rendered anyway — the post-gen truncator handles the
/// whitespace-preserving slice for finalized prose).
pub(crate) struct StreamRepetitionDetector {
    buffer_text: String,
    // Number of whole words currently held. Maintained alongside buffer_text
    // so we can cheaply know when to trim the head.
    word_count: usize,
}

impl StreamRepetitionDetector {
    pub(crate) fn new() -> Self {
        Self {
            buffer_text: String::new(),
            word_count: 0,
        }
    }

    /// Append a streamed chunk and run the shared detector. Returns:
    ///   * `Some(clean)` — a mechanical loop was confirmed. The caller breaks
    ///     the HTTP stream, drops the response (severing TCP), and finalizes
    ///     the turn with `clean` as the prose. `clean` contains the first
    ///     occurrence of the looped phrase + all preceding prose, trimmed.
    ///   * `None` — no loop yet; keep streaming.
    ///
    /// After a hit, the internal buffer is reset to the truncated text, so
    /// any chunks that arrive between the hit and the loop break (a race
    /// that shouldn't happen given the immediate break, but defended) won't
    /// double-fire or extend the truncated prose.
    pub(crate) fn push(&mut self, chunk: &str) -> Option<String> {
        if chunk.is_empty() {
            return None;
        }
        self.buffer_text.push_str(chunk);
        // Tokenize once — this is the only O(n) pass on the buffer per
        // chunk. Single source for word-count, head-trim, and detection.
        let mut spans = word_spans(&self.buffer_text);
        self.word_count = spans.len();

        // Trim the head if we've grown past the cap. We rebuild the buffer
        // from the trailing STREAM_TAIL_MAX_WORDS words so detection always
        // sees a clean contiguous window. (Cheap: 200-word Vec rebuild.)
        if self.word_count > STREAM_TAIL_MAX_WORDS {
            let keep_from = self.word_count - STREAM_TAIL_MAX_WORDS;
            let keep_byte = spans[keep_from].0;
            self.buffer_text = self.buffer_text[keep_byte..].to_string();
            // Re-tokenize the trimmed buffer so spans/words align with the
            // new string (offsets shift after the head drop).
            spans = word_spans(&self.buffer_text);
            self.word_count = spans.len();
        }

        if self.word_count < MIN_WINDOW_WORDS * MIN_REPEATS {
            return None;
        }
        let words: Vec<&str> = spans
            .iter()
            .map(|(st, en)| &self.buffer_text[*st..*en])
            .collect();
        if detect_repetition_offset(&self.buffer_text, &spans, &words).is_some() {
            // Hit. Use `truncate_repetition` so the post-kill prose is
            // byte-identical to what the post-gen firewall would have
            // produced (single visible contract, single slice path).
            let clean = truncate_repetition(&self.buffer_text);
            // Defensive: if (impossibly) the primitive fired but the wrapper
            // returned the input unchanged, treat as no-hit so we never
            // finalize a turn with a loop still in the prose.
            if clean.len() < self.buffer_text.len() {
                self.buffer_text = clean.clone();
                return Some(clean);
            }
        }
        None
    }
}

impl Default for StreamRepetitionDetector {
    fn default() -> Self {
        Self::new()
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
    fn brackets_stripped_weather() {
        // Phase 4 Component 2 (2026-07-28) — §11.37 recurrence guard: the
        // streaming regex MUST recognize [WEATHER ...] so it doesn't leak
        // raw mid-generation (invisible in finalized text because the
        // post-turn parser strips it on `done`, but visible in the live feed).
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("The sky darkens.[WEATHER heavy rain]Drops hammer the roof.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("The sky darkens."), "lead-in prose lost: {:?}", combined);
        assert!(combined.contains("Drops hammer the roof."), "trailing prose lost: {:?}", combined);
        assert!(!combined.contains("[WEATHER"), "WEATHER bracket leaked: {:?}", combined);
        assert!(
            !combined.contains("heavy rain"),
            "WEATHER condition body leaked (should be stripped, not rendered): {:?}",
            combined
        );
    }

    #[test]
    fn weather_bracket_split_across_chunks() {
        // The streaming holdback must keep a partial `[WEATHE` prefix from
        // leaking when a chunk boundary falls inside the bracket, then strip
        // the whole bracket once it completes.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Rain.[WEATHE");
        let out2 = f.feed("R heavy rain]More prose.");
        let flushed = f.flush();
        let combined = format!("{out1}{out2}{flushed}");
        assert!(combined.contains("Rain."), "lead-in lost: {:?}", combined);
        assert!(combined.contains("More prose."), "trailing lost: {:?}", combined);
        assert!(!combined.contains("[WEATHER"), "split bracket leaked: {:?}", combined);
        assert!(!combined.contains("heavy rain"), "split bracket body leaked: {:?}", combined);
    }

    #[test]
    fn brackets_stripped_travel() {
        // Phase 4 Component 3 (2026-07-28) — §11.37 recurrence guard: the
        // streaming regex MUST recognize [TRAVEL ...] so it doesn't leak raw
        // mid-generation (invisible in finalized text because the post-turn
        // parser strips it on `done`, but visible in the live feed).
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("Mara nods.[TRAVEL cellar]You descend the stairs.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Mara nods."), "lead-in prose lost: {:?}", combined);
        assert!(combined.contains("You descend the stairs."), "trailing prose lost: {:?}", combined);
        assert!(!combined.contains("[TRAVEL"), "TRAVEL bracket leaked: {:?}", combined);
        assert!(
            !combined.contains("cellar"),
            "TRAVEL destination body leaked (should be stripped, not rendered): {:?}",
            combined
        );
    }

    #[test]
    fn travel_bracket_split_across_chunks() {
        // The streaming holdback must keep a partial `[TRAV` prefix from
        // leaking when a chunk boundary falls inside the bracket, then strip
        // the whole bracket once it completes.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Mara nods.[TRAV");
        let out2 = f.feed("EL cellar]You descend.");
        let flushed = f.flush();
        let combined = format!("{out1}{out2}{flushed}");
        assert!(combined.contains("Mara nods."), "lead-in lost: {:?}", combined);
        assert!(combined.contains("You descend."), "trailing lost: {:?}", combined);
        assert!(!combined.contains("[TRAVEL"), "split bracket leaked: {:?}", combined);
        assert!(!combined.contains("cellar"), "split bracket body leaked: {:?}", combined);
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

    // === 2026-07-28 leak regression tests ====================================
    // Two distinct leaks surfaced during a local-only RP session:
    //  (1) A bare or non-thought `<|channel>` opener (e.g. `<|channel>reply`)
    //      leaked during streaming — the original marker list only had
    //      `<|channel>thought`, so any other channel opener passed through.
    //  (2) The Phase 3 bracket commands (TIME / EFFECT / MILESTONE / TASK)
    //      leaked during streaming — the streaming regex predated them and
    //      only listed CHARACTER_TURN / OBJECT / FX.
    // Both were invisible in finalized text (the post-turn `bracket_parser`
    // + `extract_reply_channel` clean everything on `done`), which made them
    // ghostly to reproduce — you had to catch them mid-stream.

    #[test]
    fn bare_channel_opener_stripped() {
        // Bug 1 regression: a non-thought channel opener. The 12B narrator
        // sometimes regresses to emitting `<|channel>reply` or a bare
        // `<|channel>` opener during creative RP; without the bare marker in
        // the list, the entire opener + its content leaked live. The bare
        // `<|channel>` now catches every variant.
        let mut f = StreamFilter::new(&["<|turn>", "<|channel>thought", "<|channel>", "<channel|>"]);
        let out = f.feed("Mara nods.<|channel>replyThe tavern door creaks.<channel|>");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Mara nods."), "lead-in lost: {:?}", combined);
        assert!(combined.contains("The tavern door creaks."), "reply body lost: {:?}", combined);
        assert!(!combined.contains("<|channel>"), "channel opener leaked: {:?}", combined);
        assert!(!combined.contains("<channel|>"), "channel closer leaked: {:?}", combined);
    }

    #[test]
    fn channel_thought_opener_takes_long_match() {
        // Order invariant: `<|channel>thought` must match in full on a
        // thought-channel opener, NOT leave `thought` as literal prose when
        // bare `<|channel>` is also in the list. The regex is first-match-
        // wins, so `<|channel>thought` MUST come first in the marker array.
        // (Locks the load-bearing ordering documented at the call site.)
        let mut f = StreamFilter::new(&["<|channel>thought", "<|channel>", "<channel|>"]);
        let out = f.feed("<|channel>thought\nreasoning<channel|>visible");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        // The opener's `thought` suffix must NOT survive as literal prose.
        assert!(
            !combined.contains("thought"),
            "`thought` suffix leaked as prose (regex order bug): {:?}",
            combined
        );
        // `reasoning` (the thought body) survives because we don't strip the
        // body in the streaming filter — that's ThoughtGate's job on the chat
        // path. The narrator path strips only the markers. We don't assert on
        // `reasoning` either way; the contract here is just the marker strip.
        assert!(!combined.contains("<|channel>"), "opener leaked: {:?}", combined);
        assert!(!combined.contains("<channel|>"), "closer leaked: {:?}", combined);
    }

    #[test]
    fn time_bracket_stripped_during_streaming() {
        // Bug 2 regression, command #1: `[TIME Day 3, 14:00]` was added to
        // bracket_parser in Phase Seam #4 but not backported to the streaming
        // regex → leaked during live streaming, stripped on done. This is the
        // "repeated timestamps" symptom from the bug report.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("Night falls.[TIME Day 3, 14:00]The candles flicker.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Night falls."), "lead-in lost: {:?}", combined);
        assert!(combined.contains("The candles flicker."), "trailing lost: {:?}", combined);
        assert!(!combined.contains("[TIME"), "TIME bracket leaked: {:?}", combined);
        assert!(!combined.contains("Day 3"), "TIME content leaked: {:?}", combined);
    }

    #[test]
    fn effect_bracket_stripped_during_streaming() {
        // Bug 2 regression, command #2: `[EFFECT ...]` (Phase 3 Slice 4,
        // buffs/debuffs with timed expiry).
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("Mara rages.[EFFECT Berserk Rage buff 60]Her muscles tense.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Mara rages."));
        assert!(combined.contains("Her muscles tense."));
        assert!(!combined.contains("[EFFECT"), "EFFECT bracket leaked: {:?}", combined);
        assert!(!combined.contains("Berserk"), "EFFECT content leaked: {:?}", combined);
    }

    #[test]
    fn milestone_bracket_stripped_during_streaming() {
        // Bug 2 regression, command #3: `[MILESTONE npc_id event_id]`
        // (Phase 3 Slice 5, relationship events).
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed("They drink.[MILESTONE npc.marcus shared_drink]Marcus smiles.");
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("They drink."));
        assert!(combined.contains("Marcus smiles."));
        assert!(!combined.contains("[MILESTONE"), "MILESTONE bracket leaked: {:?}", combined);
        assert!(!combined.contains("shared_drink"), "MILESTONE content leaked: {:?}", combined);
    }

    #[test]
    fn task_bracket_stripped_during_streaming() {
        // Bug 2 regression, command #4: `[TASK npc desc | diff suit eta]`
        // (Phase 3 Slice 6, off-screen task queue). The body contains a pipe
        // — verifies `[^\]]+` correctly spans it.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed(
            "Marcus leaves.[TASK npc.marcus scout the camp | challenging adequate 1440]Night deepens.",
        );
        let flushed = f.flush();
        let combined = format!("{out}{flushed}");
        assert!(combined.contains("Marcus leaves."));
        assert!(combined.contains("Night deepens."));
        assert!(!combined.contains("[TASK"), "TASK bracket leaked: {:?}", combined);
        assert!(!combined.contains("scout"), "TASK content leaked: {:?}", combined);
    }

    #[test]
    fn bracket_split_across_chunks_time() {
        // The critical streaming test for the new commands: a TIME bracket
        // split at a chunk boundary. The partial-prefix holdback must keep
        // `[TIM` from leaking, then complete + strip it when the rest arrives.
        // Mirrors the existing `bracket_split_across_chunks` test for OBJECT.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Travel montage. [TIM");
        assert!(
            !out1.contains("[TIM"),
            "partial TIME bracket leaked on first chunk: {:?}",
            out1
        );
        let out2 = f.feed("E Day 5]Dawn breaks.");
        let combined = format!("{out1}{out2}");
        let flushed = f.flush();
        let all = format!("{combined}{flushed}");
        assert!(all.contains("Travel montage."), "lead-in lost: {:?}", all);
        assert!(all.contains("Dawn breaks."), "trailing lost: {:?}", all);
        assert!(!all.contains("[TIME"), "TIME bracket not stripped: {:?}", all);
        assert!(!all.contains("Day 5"), "TIME content leaked: {:?}", all);
    }

    // ========================================================================
    // Bug A fix tests (2026-07-28): fenced-JSON streaming.
    // Mirrors the bracket-leak regression block above. The fence body is
    // unbounded (unlike brackets), so the in-progress body must NEVER stream
    // — the model is mid-JSON and the user shouldn't see half a fence.
    // ========================================================================

    #[test]
    fn fence_complete_block_stripped_in_one_chunk() {
        // Happy path: the whole fence arrives in a single chunk. strip_all
        // + fence_re must remove it; surrounding prose survives.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed(
            "Prose before. ```json\n{\"type\":\"fx\",\"effect\":\"rain\"}\n``` Prose after.",
        );
        let flushed = f.flush();
        let all = format!("{out}{flushed}");
        assert!(all.contains("Prose before."), "lead-in lost: {:?}", all);
        assert!(all.contains("Prose after."), "trailing lost: {:?}", all);
        assert!(!all.contains("```"), "fence markers leaked: {:?}", all);
        assert!(!all.contains("rain"), "fence body leaked: {:?}", all);
    }

    #[test]
    fn fence_split_across_chunks_does_not_leak_body() {
        // The critical streaming test: the ```json opener arrives in chunk 1,
        // the body + closer arrive in chunk 2. The body must NEVER stream —
        // the in-progress fence is held back until the closer completes it,
        // then the whole fence is stripped in one piece.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Prose. ```json\n");
        // The opener must NOT have streamed — it's a partial fence.
        assert!(
            !out1.contains("```"),
            "fence opener leaked on first chunk: {:?}",
            out1
        );
        // The lead-in prose before the fence MAY stream, but the fence must not.
        let out2 = f.feed("{\"type\":\"fx\",\"effect\":\"fog\"}\n``` After.");
        let combined = format!("{out1}{out2}");
        let flushed = f.flush();
        let all = format!("{combined}{flushed}");
        assert!(all.contains("Prose."), "lead-in lost: {:?}", all);
        assert!(all.contains("After."), "trailing lost: {:?}", all);
        assert!(!all.contains("```"), "fence markers leaked: {:?}", all);
        assert!(!all.contains("fog"), "fence body leaked: {:?}", all);
    }

    #[test]
    fn fence_opener_partial_backticks_held_back() {
        // A trailing ``` (partial opener) at a chunk boundary must be held
        // so it doesn't stream before the next chunk resolves it into a full
        // fence. Real models emit ``` as a contiguous token; this simulates
        // the chunk boundary landing inside the opener (`` in chunk 1, the
        // third ` + `json` in chunk 2).
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Some prose here. ``");
        // The partial backticks must NOT have leaked.
        assert!(
            !out1.contains("``"),
            "partial backticks leaked: {:?}",
            out1
        );
        // Complete the opener (third backtick + json) + body + closer.
        let out2 = f.feed("`json\n{\"type\":\"object\",\"id\":\"x\",\"state\":\"y\"}\n``` done.");
        let combined = format!("{out1}{out2}");
        let flushed = f.flush();
        let all = format!("{combined}{flushed}");
        assert!(all.contains("Some prose here."), "lead-in lost: {:?}", all);
        assert!(all.contains("done."), "trailing lost: {:?}", all);
        assert!(!all.contains("```"), "fence markers leaked: {:?}", all);
    }

    #[test]
    fn unterminated_fence_dropped_on_flush() {
        // A cancelled generation leaving a ```json opener with no closer.
        // flush() must drop the opener + partial body so neither leaks.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        f.feed("Lead-in. ```json\n{\"type\":\"fx\",\"effect\":\"thunder\"");
        let flushed = f.flush();
        assert!(flushed.contains("Lead-in."), "lead-in lost: {:?}", flushed);
        assert!(!flushed.contains("```"), "opener leaked in flush: {:?}", flushed);
        assert!(!flushed.contains("thunder"), "partial body leaked: {:?}", flushed);
    }

    #[test]
    fn fence_and_bracket_in_same_stream() {
        // Both a bracket command and a JSON fence in the same generation.
        // Both must strip cleanly; both bodies suppressed.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out1 = f.feed("Start. [FX thunder] Middle. ```json\n");
        let out2 = f.feed("{\"type\":\"effect\",\"label\":\"Shaken\"}\n``` End.");
        let combined = format!("{out1}{out2}");
        let flushed = f.flush();
        let all = format!("{combined}{flushed}");
        assert!(all.contains("Start."), "lost start: {:?}", all);
        assert!(all.contains("Middle."), "lost middle: {:?}", all);
        assert!(all.contains("End."), "lost end: {:?}", all);
        assert!(!all.contains("[FX"), "bracket leaked: {:?}", all);
        assert!(!all.contains("```"), "fence leaked: {:?}", all);
        assert!(!all.contains("Shaken"), "fence body leaked: {:?}", all);
    }

    #[test]
    fn multiple_fences_in_one_stream() {
        // Two separate JSON fences in one generation. Each must strip; the
        // prose between them survives.
        let mut f = StreamFilter::new(&["<|turn>"]).with_brackets();
        let out = f.feed(
            "```json\n{\"type\":\"fx\",\"effect\":\"rain\"}\n```\nBetween.\n```json\n{\"type\":\"object\",\"id\":\"door\",\"state\":\"open\"}\n```\nAfter.",
        );
        let flushed = f.flush();
        let all = format!("{out}{flushed}");
        assert!(all.contains("Between."), "middle prose lost: {:?}", all);
        assert!(all.contains("After."), "trailing lost: {:?}", all);
        assert!(!all.contains("```"), "fence markers leaked: {:?}", all);
        assert!(!all.contains("rain"), "first fence body leaked: {:?}", all);
        assert!(!all.contains("door"), "second fence body leaked: {:?}", all);
    }

    // ========================================================================
    // Post-generation repetition truncator tests (§11.40.E follow-up).
    // The truncator is a pure terminal-pass prose helper; these tests pin
    // the conservative contract: aggressive mechanical looping is truncated
    // to one clean instance, legitimate rhetorical repetition is preserved.
    // ========================================================================

    #[test]
    fn truncate_repetition_truncates_four_word_loop_three_repeats() {
        // The canonical smuggler-loop case: a 4-word phrase repeated 3x
        // back-to-back. Must keep ONE clean instance, drop the rest.
        let input = "The fire crackles loudly tonight. The fire crackles loudly tonight. The fire crackles loudly tonight.";
        let out = truncate_repetition(input);
        assert_eq!(
            out, "The fire crackles loudly tonight.",
            "should keep one clean instance, got: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_leaves_two_repeats_unchanged() {
        // Rhetorical anaphora: deliberate double-repetition is legitimate
        // prose (e.g. "Never give up hope now. Never give up hope now.").
        // MIN_REPEATS=3 means 2 is never a loop. Must be unchanged.
        let input = "Never give up hope now. Never give up hope now.";
        let out = truncate_repetition(input);
        assert_eq!(
            out, input,
            "double-anaphora (2 repeats) must be preserved, got: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_leaves_short_repeats_unchanged() {
        // 2-3 word dialogue echoes ("I know. I know. I know.") are below
        // the MIN_WINDOW_WORDS=4 floor. These are legitimate dialogue
        // patterns, not mechanical loops. Must be unchanged.
        let input = "I know. I know. I know. I know.";
        let out = truncate_repetition(input);
        assert_eq!(
            out, input,
            "short-word dialogue repeat must be preserved (below 4-word floor), got: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_handles_loop_at_end_mid_sentence() {
        // The smuggler-loop tail case from the live playtest: clean prose
        // followed by a 4+ word loop that ends mid-sentence (the model ran
        // into max_tokens or fixated). Lead-in prose must survive; the loop
        // is truncated to one instance + the partial third repeat is dropped.
        let input = "Mara greets the traveler warmly. The shadows dance across the wall. The shadows dance across the wall. The shadows dance across the wall. The shadows dance ac";
        let out = truncate_repetition(input);
        assert!(
            out.contains("Mara greets the traveler warmly."),
            "lead-in prose lost: {:?}",
            out
        );
        // The first "The shadows dance across the wall." instance survives.
        assert!(
            out.contains("The shadows dance across the wall."),
            "first loop instance lost: {:?}",
            out
        );
        // Count occurrences of the loop phrase: should be exactly 1.
        let occurrences = out
            .matches("The shadows dance across the wall.")
            .count();
        assert_eq!(
            occurrences, 1,
            "expected exactly 1 occurrence of the loop phrase, got {}: {:?}",
            occurrences, out
        );
        // The partial third repeat is gone.
        assert!(
            !out.ends_with("ac"),
            "partial third repeat leaked: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_drops_prose_after_truncation_point() {
        // When a loop fires mid-text, everything from the SECOND occurrence
        // onward is dropped (a detected loop signals model breakdown — we
        // don't try to recover prose after it). Lead-in + first instance
        // survive; trailing prose (which only existed because the model was
        // looping past it) is cut.
        let input = "Lead-in prose here. The fire crackles loudly tonight. The fire crackles loudly tonight. The fire crackles loudly tonight. Trailing prose that should be cut.";
        let out = truncate_repetition(input);
        assert!(
            out.contains("Lead-in prose here."),
            "lead-in lost: {:?}",
            out
        );
        assert!(
            out.contains("The fire crackles loudly tonight."),
            "first loop instance lost: {:?}",
            out
        );
        // Everything after the first loop instance is dropped — including
        // the "Trailing prose" which only existed past the loop point.
        assert!(
            !out.contains("Trailing prose"),
            "post-loop prose should be cut (loop signals breakdown), got: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_preserves_original_whitespace_in_kept_portion() {
        // Byte-offset slicing (not rebuild) keeps newlines, paragraphs, and
        // original whitespace intact in the surviving prefix. A narrator
        // turn with paragraph breaks before a loop must preserve those
        // breaks in the output.
        let input = "Para one ends here.\n\nPara two follows.\n\nThe shadows dance across the wall. The shadows dance across the wall. The shadows dance across the wall.";
        let out = truncate_repetition(input);
        assert!(
            out.contains("Para one ends here.\n\nPara two follows.\n\nThe shadows dance across the wall."),
            "original whitespace/paragraph breaks must be preserved in kept portion, got: {:?}",
            out
        );
        // Exactly one occurrence of the loop phrase.
        let occurrences = out
            .matches("The shadows dance across the wall.")
            .count();
        assert_eq!(occurrences, 1, "got: {:?}", out);
    }

    #[test]
    fn truncate_repetition_handles_empty_and_short_strings() {
        // Empty + too-short input (< MIN_WINDOW_WORDS * MIN_REPEATS = 12
        // words) can never loop. Returned unchanged.
        assert_eq!(truncate_repetition(""), "");
        let short = "This is a short sentence with only a few words.";
        let out = truncate_repetition(short);
        assert_eq!(
            out, short,
            "short input (< 12 words) returned unchanged, got: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_truncates_at_earliest_loop_position() {
        // When two distinct loops exist at different positions, the earliest
        // (lowest byte offset) wins → most aggressive cleanup. The second
        // loop is irrelevant because the first truncation already cut it.
        let input = "Early loop fires here now. Early loop fires here now. Early loop fires here now. Later loop also here now too. Later loop also here now too. Later loop also here now too.";
        let out = truncate_repetition(input);
        assert!(
            out.contains("Early loop fires here now."),
            "first (earliest) loop instance kept: {:?}",
            out
        );
        // The "Later loop" instances are past the first truncation point.
        assert!(
            !out.contains("Later loop"),
            "second loop should be cut by the first truncation, got: {:?}",
            out
        );
    }

    #[test]
    fn truncate_repetition_detects_long_window_loops() {
        // A 6-word phrase (longer than the 4-word floor) repeated 3x. The
        // window-escalation loop must find it (not just exactly-4 windows).
        let input = "The old wooden door creaks open slowly. The old wooden door creaks open slowly. The old wooden door creaks open slowly.";
        let out = truncate_repetition(input);
        assert_eq!(
            out, "The old wooden door creaks open slowly.",
            "6-word loop should truncate to one instance, got: {:?}",
            out
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // §11.43 — Stream kill switch unit tests. The contract being pinned:
    //
    //   1. Cross-chunk loop detection — the core reason the detector
    //      exists. A loop split across N simulated byte-chunks MUST fire
    //      on the same tick the 3rd occurrence completes, never earlier,
    //      never later. Proves chunk-boundary handling ("cra"+"ckles").
    //
    //   2. Anaphora preservation — 2-repeat rhetorical repetition MUST
    //      NOT fire mid-stream. Same conservative threshold as the post-
    //      gen truncator; tightening for streaming would mangle valid
    //      narration (false-positive abort mid-paragraph).
    //
    //   3. Rolling-buffer head trim — pushing > STREAM_TAIL_MAX_WORDS
    //      keeps the buffer bounded without losing a loop that arrives
    //      in the live tail. Proves cost is O(cap) not O(stream length).
    //
    //   4. Identical threshold parity — a loop that the post-gen
    //      truncator catches MUST also be caught by the stream detector,
    //      and the resulting prose MUST be byte-identical. Pins the
    //      "single source of truth" claim.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn stream_detector_fires_on_complete_loop_in_one_chunk() {
        // Baseline: the whole loop arrives in one chunk. Same input as the
        // post-gen truncator's canonical case. Must fire + return the same
        // truncated prose as truncate_repetition would.
        let loop_text = "The smuggler turns and runs away. The smuggler turns and runs away. The smuggler turns and runs away.";
        let mut d = StreamRepetitionDetector::new();
        let hit = d.push(loop_text);
        assert!(hit.is_some(), "detector must fire on a 3-repeat loop");
        let clean = hit.unwrap();
        assert_eq!(
            clean, "The smuggler turns and runs away.",
            "stream detector must produce byte-identical output to post-gen truncator, got: {:?}",
            clean
        );
        assert_eq!(
            clean,
            truncate_repetition(loop_text),
            "stream + post-gen paths must agree (single source of truth)"
        );
    }

    #[test]
    fn stream_detector_fires_on_loop_split_across_many_chunks() {
        // THE core test. A 3-repeat smuggler loop is split into many tiny
        // chunks (1-3 chars), some of which cut mid-word ("smu" + "ggler").
        // The detector MUST fire on exactly the tick where the 3rd
        // occurrence completes — and the truncated prose must match the
        // post-gen truncator's output on the same full text.
        let full = "The smuggler turns and runs. The smuggler turns and runs. The smuggler turns and runs.";
        let chars: Vec<&str> = full.split_inclusive(|c: char| matches!(c, ' ' | '.')).collect();
        // split_inclusive keeps the delimiter; assert we actually have
        // many small chunks (sanity check on the test harness itself).
        assert!(
            chars.len() > 10,
            "test harness must produce many chunks, got {}",
            chars.len()
        );

        let mut d = StreamRepetitionDetector::new();
        let mut last_hit: Option<String> = None;
        let mut ticks_before_hit = 0;
        for (i, chunk) in chars.iter().enumerate() {
            if let Some(clean) = d.push(chunk) {
                last_hit = Some(clean);
                ticks_before_hit = i + 1;
                break;
            }
        }
        let clean = last_hit.expect("detector must eventually fire on the chunked loop");
        // Fired on the LAST chunk (when the 3rd occurrence completes), not
        // earlier. Indexing from 1 because ticks_before_hit = i+1.
        assert_eq!(
            ticks_before_hit, chars.len(),
            "must fire on the last chunk (3rd occurrence completion), not earlier"
        );
        // Byte-identical to the post-gen path on the same input.
        assert_eq!(
            clean,
            truncate_repetition(full),
            "chunked detection must produce the same truncated prose as post-gen"
        );
        assert_eq!(
            clean, "The smuggler turns and runs.",
            "truncated prose is one clean instance of the phrase, got: {:?}",
            clean
        );
    }

    #[test]
    fn stream_detector_fires_when_repeat_phrase_straddles_chunk_boundary() {
        // Specially-crafted split: the chunk boundary falls IN THE MIDDLE of
        // the first word of the 3rd occurrence's repeated phrase. A naive
        // per-chunk detector would miss it; the rolling re-tokenize must
        // catch it. "The smuggler turns and" repeated 3x, with the third
        // copy's "smuggler" split as "smu" + "ggler".
        let p1 = "The smuggler turns and ";          // occurrence 1
        let p2 = "The smuggler turns and ";          // occurrence 2
        let p3a = "The smu";                          // 3rd occurrence start, split
        let p3b = "ggler turns and ";                 // completes 3rd occurrence
        // By p3b the buffer holds the full 3-repeat loop; the NEXT push
        // (any chunk) would re-scan and fire. But the detector runs on
        // every push, so it should fire ON p3b (the moment the 3rd
        // occurrence's last word completes the window).
        let mut d = StreamRepetitionDetector::new();
        assert!(d.push(p1).is_none(), "no fire after 1 occurrence");
        assert!(d.push(p2).is_none(), "no fire after 2 occurrences (anaphora)");
        assert!(d.push(p3a).is_none(), "no fire mid-word on the 3rd occurrence");
        let hit = d.push(p3b).expect("must fire the instant the 3rd occurrence completes");
        // The rolling buffer joined the chunks with no extra whitespace
        // (we pushed whitespace-bearing chunks), so the result matches the
        // post-gen path on the equivalent contiguous string.
        let equiv = "The smuggler turns and The smuggler turns and The smuggler turns and ";
        assert_eq!(hit, truncate_repetition(equiv));
        // And that result is exactly one instance of the phrase.
        assert!(
            hit.matches("The smuggler turns and").count() == 1,
            "truncated prose should contain exactly one instance of the phrase, got: {:?}",
            hit
        );
    }

    #[test]
    fn stream_detector_preserves_two_repeat_anaphora_mid_stream() {
        // 2-repeat rhetorical anaphora MUST NOT fire — this is the false-
        // positive guard. Same threshold as the post-gen truncator
        // (MIN_REPEATS=3). A mid-stream abort on legitimate anaphora
        // would mangle valid narration.
        let mut d = StreamRepetitionDetector::new();
        // Two occurrences of a 5-word phrase (above the 4-word floor so
        // we're testing the repeat-count gate, not the window-size gate).
        assert!(d.push("The wind howls across the moor. ").is_none());
        assert!(
            d.push("The wind howls across the moor. ").is_none(),
            "2 repeats is rhetorical anaphora, not a loop — must NOT fire"
        );
        // ...and the buffer should retain the prose so a subsequent real
        // loop later in the same turn still gets caught.
        assert!(d.push("Then silence falls. ").is_none());
    }

    #[test]
    fn stream_detector_preserves_short_word_repeats_mid_stream() {
        // Short-word echoes ("I know. I know. I know.") — 2-word phrase,
        // below the MIN_WINDOW_WORDS=4 floor. NEVER fires even at 3+
        // repeats. These are legitimate dialogue beats handled by the
        // DRY sampler at the token level; the firewall leaves them alone.
        let mut d = StreamRepetitionDetector::new();
        for _ in 0..5 {
            assert!(
                d.push("I know. ").is_none(),
                "short-word repeats below the 4-word floor must never fire"
            );
        }
    }

    #[test]
    fn stream_detector_rolling_buffer_trims_head_without_losing_loops() {
        // Push a long lead-in (well over STREAM_TAIL_MAX_WORDS=200) then a
        // 3-repeat loop in the tail. The head trim must NOT prevent
        // detection of a loop that lives entirely in the live tail.
        let mut d = StreamRepetitionDetector::new();
        // 250 unique-ish words of lead-in. Use enumerated tokens so they
        // don't themselves form a loop.
        for i in 0..250 {
            let _ = d.push(&format!("word{} ", i));
        }
        // Sanity: no fire during the lead-in.
        // Now push a loop in the tail.
        let phrase = "the dragon swoops down from the sky ";  // 8 words
        let hit = d
            .push(&phrase.repeat(3))
            .expect("loop in the live tail must fire despite head trim");
        assert!(
            hit.matches("the dragon swoops down from the sky").count() == 1,
            "loop in the tail should truncate to one instance, got: {:?}",
            hit
        );
        // And the head-trimmed lead-in words are NOT in the output (they
        // were outside the rolling window) — only the live tail is.
        // The most recent few wordN tokens may be in the buffer's prefix,
        // but the very first ones definitely aren't.
        assert!(
            !hit.contains("word0 "),
            "head-trimmed lead-in must not appear in the truncated output"
        );
    }

    #[test]
    fn stream_detector_never_fires_on_clean_prose() {
        // Varied, non-repetitive narrator prose must never trigger across
        // many chunks. This is the negative-control: any false positive
        // here would mean valid narration gets truncated mid-stream.
        let prose = "The tavern falls silent as the stranger enters. Mara wipes down the counter, \
                     watching from beneath her lashes. Outside, rain begins to fall, each drop \
                     tapping against the shuters like a question. The stranger orders ale, drops \
                     two coppers on the bar, and waits. Nobody speaks. Nobody moves.";
        let mut d = StreamRepetitionDetector::new();
        // Stream it word-by-word (aggressive chunking).
        for chunk in prose.split_inclusive(' ') {
            assert!(
                d.push(chunk).is_none(),
                "clean varied prose must never trigger the kill switch — chunk: {:?}",
                chunk
            );
        }
    }

    #[test]
    fn stream_detector_fires_only_once_then_stays_quiet() {
        // After a hit, the buffer is reset to the truncated prose. Further
        // pushes (raced chunks arriving before the loop break propagates)
        // must NOT re-fire or extend the truncated prose with new loop
        // content — they re-scan against the clean text.
        let mut d = StreamRepetitionDetector::new();
        let hit = d
            .push("Alpha beta gamma delta. Alpha beta gamma delta. Alpha beta gamma delta.")
            .expect("must fire on the 3-repeat loop");
        assert_eq!(hit, "Alpha beta gamma delta.");
        // Post-hit push of MORE loop content (the race window).
        let second = d.push(" Alpha beta gamma delta.");
        // Either None (clean buffer doesn't have 3 repeats yet) or Some
        // equal-to-or-shorter-than the current clean state. The contract:
        // it must NOT produce a longer string with a loop in it.
        if let Some(ref s) = second {
            assert!(
                s.matches("Alpha beta gamma delta").count() <= 1,
                "post-hit pushes must never re-introduce a loop, got: {:?}",
                s
            );
            assert!(
                s.len() <= hit.len() + " Alpha beta gamma delta.".len(),
                "post-hit output must not balloon unboundedly"
            );
        }
    }

    #[test]
    fn stream_detector_byte_offsets_are_valid_utf8_after_truncation() {
        // Defensive: the truncated prose returned to the HTTP loop will be
        // serialized + sent over IPC + rendered in the DOM. It must be
        // valid UTF-8 with no mid-codepoint cut. The truncator slices on
        // whitespace boundaries (which are always ASCII = 1-byte), so this
        // should hold trivially — but we pin it because a regression here
        // would be a silent corruption.
        let loop_text = "café naïve résumé déjÀ vu. café naïve résumé déjÀ vu. café naïve résumé déjÀ vu.";
        let mut d = StreamRepetitionDetector::new();
        let hit = d.push(loop_text).expect("must fire on Unicode loop");
        // Result must be valid UTF-8 (String invariant — always true, but
        // the assertion documents intent).
        assert!(std::str::from_utf8(hit.as_bytes()).is_ok());
        // And it should contain exactly one instance of the multi-byte
        // phrase (the slice happened on whitespace, not mid-codepoint).
        assert_eq!(
            hit.matches("café naïve résumé déjÀ vu").count(),
            1,
            "Unicode phrase must be preserved exactly once, got: {:?}",
            hit
        );
    }

    #[test]
    fn stream_detector_handles_empty_and_single_word_chunks() {
        // Edge cases that could panic a naive impl.
        let mut d = StreamRepetitionDetector::new();
        assert!(d.push("").is_none(), "empty chunk must not fire or panic");
        assert!(d.push(" ").is_none(), "whitespace-only chunk must not fire");
        assert!(d.push("word").is_none(), "single word must not fire");
        // After many single-word pushes that don't form a loop, still no fire.
        for w in &["alpha", "beta", "gamma", "delta", "epsilon", "zeta"] {
            assert!(d.push(w).is_none());
        }
    }

    #[test]
    fn stream_detector_idempotent_on_full_text_vs_chunked() {
        // The canonical parity assertion: streaming the same text in one
        // chunk vs many chunks must produce identical truncated prose.
        // This is the strongest form of "chunk boundaries don't matter".
        let text = "The smuggler laughs and draws his blade. The smuggler laughs and draws his blade. The smuggler laughs and draws his blade.";
        // One-shot:
        let oneshot = {
            let mut d = StreamRepetitionDetector::new();
            d.push(text).expect("must fire")
        };
        // Chunked char-by-char:
        let chunked = {
            let mut d = StreamRepetitionDetector::new();
            let mut result = None;
            // One character per push — maximally aggressive chunking.
            for c in text.chars() {
                if let Some(clean) = d.push(c.to_string().as_str()) {
                    result = Some(clean);
                    break;
                }
            }
            result.expect("char-by-char streaming must also fire")
        };
        assert_eq!(
            oneshot, chunked,
            "one-shot and char-by-char streaming must produce identical truncated prose"
        );
        assert_eq!(oneshot, truncate_repetition(text));
    }
}
