//! Pure-Rust JSON syntax repair — the microsecond-cost pre-parser.
//!
//! Sits between `extract_reply_channel`/`strip_markdown_fences` (the channel +
//! fence cleanup in `schema.rs`) and `serde_json::from_str` (the parse). It
//! fixes the COMMON ways an LLM slightly mangles JSON syntax so that a delta
//! that would have failed parse + burned a 5-8s LLM repair pass instead parses
//! on the first try, at zero LLM cost.
//!
//! ## What this is NOT
//!
//! This is **syntactic** repair only. It does NOT:
//! - Add/remove keys the model forgot (semantic — stays for the 3-pass LLM
//!   loop per AGENTS.md §5).
//! - Coerce value types (a string where a number belongs is a semantic error).
//! - Validate key/value shape (that's `schema_validator.rs`, layer 1 of §5).
//!
//! The locked §5 contract (3 passes + accumulating context + failure queue) is
//! UNCHANGED. This module only reduces how often pass 1 fails on pure syntax.
//!
//! ## Why regex-free where possible
//!
//! The targets (trailing commas, smart quotes, unquoted keys) are simple char
//! walks. Pulling in `regex` for one-shot string munging per turn would violate
//! the Prime Directive (§1B: cheapest path). The unescaped-newline + bracket
//! balance passes use a single state-machine scan.
//!
//! ## Targets (the common LLM JSON mistakes)
//!
//! 1. Smart quotes → ASCII quotes (`"` `"` `''` `'` → `"` `'`).
//! 2. Trailing commas (`{"a":1,}` → `{"a":1}`).
//! 3. Unquoted object keys (`{a:1}` → `{"a":1}`).
//! 4. Top-level brace-less object bodies (`"a":1` → `{"a":1}` — the outer
//!    `{}` dropped entirely; a truncation artifact).
//! 5. Unescaped newlines inside string values (`"a\nb"` where the `\n` is a
//!    LITERAL newline, not the escape sequence → `"a\\nb"`).
//! 6. Unbalanced brackets/braces (best-effort: append the missing closers or
//!    drop a stray closer — a common truncation artifact when the model runs
//!    into `max_tokens`).
//! 6. Markdown fences (belt-and-suspenders with `extract_reply_channel`: a
//!    stray ``` after the channel strip gets removed).
//!
//! Each pass is idempotent and runs only on text that hasn't already been
//! mangled by a prior pass (the repair is a best-effort normalization, not a
//! guarantee of validity — if serde still rejects it, the 3-pass loop takes
//! over, exactly as before).

/// Repair common JSON syntax issues in `raw`. Returns the repaired string.
///
/// Always returns a `String` (owned): the repair may allocate, but it runs at
/// most once per schema turn, so the cost is irrelevant relative to the 5-8s
/// LLM pass it avoids.
///
/// The function is conservative: if a pass can't unambiguously fix something,
/// it leaves the input unchanged and lets serde report the original error (so
/// the 3-pass loop's repair prompt shows the model the real problem, not a
/// half-repaired artifact).
pub fn repair(raw: &str) -> String {
    let s = raw.to_string();
    let s = normalize_quotes(&s);
    let s = strip_trailing_commas(&s);
    // (#49) wrap_bare_object runs BEFORE quote_unquoted_keys: a brace-less
    // body's LEADING key (`summary: "ok"`) is never preceded by `{`/`,`, so
    // the quote pass can't fix it until the braces exist.
    let s = wrap_bare_object(&s);
    let s = quote_unquoted_keys(&s);
    let s = escape_bare_newlines_in_strings(&s);
    let s = strip_stray_code_fences(&s);
    let s = balance_brackets(&s);
    // Re-run the comma strip AFTER balance: truncation-at-comma — the most
    // common truncation shape (`{"summary":"ok",`) — only gains its closer
    // from balance_brackets, which doesn't check for a dangling comma. The
    // pre-balance strip can't see it, and without this pass the input
    // burned the full 3-pass LLM repair loop instead of the microsecond
    // fix. Idempotent + string-aware, so a second pass on already-clean
    // JSON is a no-op.
    strip_trailing_commas(&s)
}

/// Wrap a top-level brace-less object body in `{…}`. A truncated/mangled LLM
/// emit can drop the outer braces entirely (`"summary": "ok"` — smart quotes
/// normalized, keys quoted, but no `{`/`}` in sight). Unambiguous shapes:
/// the trimmed input (A) STARTS with a string opener OR (#49) starts with a
/// BARE key run (`summary: "ok"` — quote_unquoted_keys only fires after
/// `{`/`,`, so the leading key can't be quoted before the braces exist), AND
/// in both cases contains a top-level `:` (outside any string). A bare
/// top-level JSON string (`"hello"` — no top-level colon), number, bool, or
/// already-braced/bracketed value is left alone (they're valid JSON as-is;
/// wrapping would corrupt them).
fn wrap_bare_object(s: &str) -> String {
    let t = s.trim();
    let shape_quoted = t.starts_with('"');
    // Shape B: a leading bare-key run followed by `:`. (2026-08-15 audit H1)
    // The run is matched over BYTES with ASCII-only membership (mirroring
    // `is_key_char` below) — `key_len` is then a genuine BYTE offset for the
    // `t[key_len..]` slice. The prior char-count over Unicode-aware
    // `is_alphanumeric` made any non-ASCII bare key (`é: 1`, `名: "x"`) slice
    // mid-char → panic (the anti-pattern #6 shape); a non-ASCII key now
    // simply stops the run → no wrap → serde reports the original error to
    // the 3-pass loop (the conservative contract, unchanged).
    let shape_bare_key = !shape_quoted && {
        let key_len = t
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-' || *b == b'.')
            .count();
        key_len > 0 && t[key_len..].trim_start().starts_with(':')
    };
    if !(shape_quoted || shape_bare_key) {
        return s.to_string();
    }
    // Scan for a top-level colon (outside strings) — same string-state
    // machine as balance_brackets, byte-linear per §1B.2.
    let mut in_string = false;
    let mut escape_next = false;
    let mut has_top_level_colon = false;
    for b in t.bytes() {
        if in_string {
            if escape_next {
                escape_next = false;
            } else if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b':' => {
                has_top_level_colon = true;
                break;
            }
            _ => {}
        }
    }
    if has_top_level_colon {
        format!("{{{t}}}")
    } else {
        s.to_string()
    }
}

/// Normalize smart quotes to ASCII. LLMs trained on prose corpora emit `" "`
/// (U+201C/U+201D) and `' '` (U+2018/U+2019) even inside JSON they intend
/// to be valid. serde_json rejects these as invalid string delimiters.
///
/// String-state aware (#31 2026-08-15): a smart DOUBLE quote is rewritten to
/// ASCII only in DELIMITER position — outside any string, or as the
/// opener/closer of a smart-delimited string. Inside an ASCII-delimited
/// string value it is CONTENT (narrator prose quoting dialogue with “ ”) and
/// is left untouched; the old blind replace turned such legally-valid JSON
/// INVALID (`"she said “hi”"` → `"she said "hi""`), converting pass-1
/// successes into 3-pass repair failures. Single smart quotes (`' '`) are
/// rewritten everywhere — they carry no JSON meaning, so the swap can never
/// change validity (only glyph shape).
fn normalize_quotes(s: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum StrState {
        /// Outside any string — any double quote (smart or ASCII) is a
        /// delimiter.
        Out,
        /// Inside an ASCII `"`-delimited string — smart doubles are content.
        Ascii,
        /// Inside a `“`-delimited string (the model delimited with smart
        /// quotes) — the closer is `”` or a stray ASCII `"` (mixed form).
        Smart,
    }
    let mut out = String::with_capacity(s.len());
    let mut st = StrState::Out;
    let mut escape_next = false;
    for ch in s.chars() {
        let single = match ch {
            '\u{2018}' | '\u{2019}' => Some('\''),
            _ => None,
        };
        match st {
            StrState::Out => match ch {
                '"' => {
                    out.push('"');
                    st = StrState::Ascii;
                }
                '\u{201C}' | '\u{201D}' => {
                    out.push('"');
                    st = StrState::Smart;
                }
                _ => out.push(single.unwrap_or(ch)),
            },
            StrState::Ascii => {
                if escape_next {
                    out.push(single.unwrap_or(ch));
                    escape_next = false;
                } else if ch == '\\' {
                    out.push('\\');
                    escape_next = true;
                } else if ch == '"' {
                    out.push('"');
                    st = StrState::Out;
                } else {
                    // Smart double quotes here are prose content — preserved.
                    out.push(single.unwrap_or(ch));
                }
            }
            StrState::Smart => {
                if escape_next {
                    out.push(single.unwrap_or(ch));
                    escape_next = false;
                } else if ch == '\\' {
                    out.push('\\');
                    escape_next = true;
                } else if ch == '"' || ch == '\u{201D}' {
                    out.push('"');
                    st = StrState::Out;
                } else {
                    out.push(single.unwrap_or(ch));
                }
            }
        }
    }
    out
}

/// Strip trailing commas before `}` or `]`. The classic LLM JSON mistake:
/// `{"a":1,}`. serde_json is strict (rejects trailing commas) while many LLM
/// training corpora include lenient JSON5-style examples.
///
/// A single char-walk with a `prev_significant` cursor: when we hit `}` or `]`
/// (OUTSIDE a string) and the last non-whitespace char was `,`, drop that
/// comma. String-aware + char-based (P0 fix): a `,` before a `}`/`]` inside a
/// string VALUE is content, not syntax — and iterating bytes while pushing
/// `byte as char` re-encoded every multi-byte UTF-8 char as Latin-1 mojibake.
fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape_next = false;
    let mut last_significant: Option<char> = None;
    for ch in s.chars() {
        if in_string {
            out.push(ch);
            if escape_next {
                escape_next = false;
            } else if ch == '\\' {
                escape_next = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if (ch == '}' || ch == ']') && last_significant == Some(',') {
            // Trim back the trailing comma we already pushed (plus any
            // whitespace between it and the closer). Stops at the closing
            // quote of a preceding string value — never pops into content.
            while matches!(out.chars().next_back(), Some(',' | ' ' | '\n' | '\t' | '\r')) {
                out.pop();
            }
        }
        out.push(ch);
        if !matches!(ch, ' ' | '\n' | '\t' | '\r') {
            last_significant = Some(ch);
        }
    }
    out
}

/// Quote unquoted object keys: `{a: 1}` → `{"a": 1}`. A single stateful scan
/// that's aware of string context (so a key that happens to be the word `true`
/// inside a string value isn't touched).
///
/// An unquoted key is a run of `[A-Za-z0-9_]` (and `-`, `.` for namespaced keys
/// like `char.mira.trust`) that sits in "key position": immediately after `{`
/// or after a `,` at the object level, followed by `:`. We only rewrite when
/// all three hold AND we're not inside a string.
///
/// Char-based (P0 fix): the prior byte loop pushed `byte as char`, so every
/// multi-byte UTF-8 char in a value (em-dashes, accents, CJK) was re-encoded
/// as Latin-1 mojibake — valid JSON, silently wrong data.
fn quote_unquoted_keys(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape_next = false;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if in_string {
            out.push(ch);
            if escape_next {
                escape_next = false;
            } else if ch == '\\' {
                escape_next = true;
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        // Not in a string.
        if ch == '"' {
            in_string = true;
            out.push(ch);
            i += 1;
            continue;
        }

        // Detect an unquoted key: preceded by `{` or `,` (ignoring whitespace),
        // is a run of key-chars, followed by optional whitespace then `:`.
        if is_key_char(ch) {
            // Confirm we're in key position: look back at the last non-ws char.
            let back_ok = last_non_ws(&out) == Some('{') || last_non_ws(&out) == Some(',');
            // Collect the candidate key run.
            let start = i;
            while i < chars.len() && is_key_char(chars[i]) {
                i += 1;
            }
            let key: String = chars[start..i].iter().collect();
            // Look ahead for `:` (skipping whitespace).
            let mut j = i;
            while j < chars.len() && matches!(chars[j], ' ' | '\t' | '\n' | '\r') {
                j += 1;
            }
            if back_ok && j < chars.len() && chars[j] == ':' {
                // It's a real unquoted key — quote it. Re-escape any internal
                // `"` just in case (rare for an unquoted run, but safe).
                let quoted = key.replace('\\', "\\\\").replace('"', "\\\"");
                out.push('"');
                out.push_str(&quoted);
                out.push('"');
                continue; // i already past the key run
            } else {
                // Not a key (e.g. a bare value like `true`, a number). Emit
                // verbatim — don't risk mangling valid bare tokens.
                out.push_str(&key);
                continue;
            }
        }

        out.push(ch);
        i += 1;
    }
    out
}

/// Escape literal newlines/tabs/CRs INSIDE string values. An LLM sometimes
/// emits a real newline inside a `"..."` value (e.g. a multi-line note) instead
/// of the `\n` escape. serde_json rejects raw control chars in strings.
///
/// We walk with string-state awareness and replace each raw `\n`/`\r`/`\t`
/// encountered while `in_string` with its escape sequence. We do NOT touch
/// existing valid escapes (`\n` as two chars backslash-n is already fine — we
/// only act on the single control byte).
fn escape_bare_newlines_in_strings(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_string = false;
    let mut escape_next = false;
    for ch in s.chars() {
        if in_string {
            if escape_next {
                out.push(ch);
                escape_next = false;
                continue;
            }
            if ch == '\\' {
                out.push(ch);
                escape_next = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
                out.push(ch);
                continue;
            }
            match ch {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push(ch),
            }
        } else {
            if ch == '"' {
                in_string = true;
            }
            out.push(ch);
        }
    }
    out
}

/// Strip a stray markdown code fence (```` ``` ````) that survives the channel
/// strip. The common residue is a leading ```json or a trailing ``` wrapping
/// the actual JSON. We only strip fences that appear OUTSIDE string literals
/// (a fence inside a string value is legitimate prose).
fn strip_stray_code_fences(s: &str) -> String {
    // Cheap + conservative: only strip if the trimmed string still starts/ends
    // with a fence. A full walk risks mangling values that legitimately contain
    // triple-backticks (rare, but possible in prose notes).
    let trimmed = s.trim_start();
    let after_open = match trimmed.strip_prefix("```") {
        Some(rest) => {
            // Allow an optional language tag on the opening fence (```json).
            let rest = rest.trim_start_matches(|c: char| c.is_alphanumeric());
            rest.trim_start()
        }
        None => trimmed,
    };
    let after_close = after_open.trim_end();
    let without_close = after_close.strip_suffix("```").unwrap_or(after_close).trim_end();
    // Re-prefix any leading whitespace we stripped from `s` only if we made no
    // change (preserve the original exactly when there's nothing to do).
    if without_close.as_bytes() == s.as_bytes() {
        s.to_string()
    } else {
        without_close.to_string()
    }
}

/// Best-effort bracket/brace balance repair. Two truncation artifacts:
/// 1. The model hit `max_tokens` mid-object: the JSON is missing one or more
///    closers. We append the needed `}`/`]` to close every still-open `{`/`[`.
/// 2. A stray closer (the model emitted `}}`): serde fails. We DON'T try to
///    drop extras (ambiguous + risky); we only ADD missing closers.
///
/// String-aware: brackets inside string literals don't count toward the stack.
fn balance_brackets(s: &str) -> String {
    let mut stack: Vec<u8> = Vec::new(); // of openers: b'{' or b'['
    let mut in_string = false;
    let mut escape_next = false;
    for b in s.bytes() {
        if in_string {
            if escape_next {
                escape_next = false;
            } else if b == b'\\' {
                escape_next = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => stack.push(b),
            b'}' => {
                if stack.last() == Some(&b'{') {
                    stack.pop();
                }
            }
            b']' => {
                if stack.last() == Some(&b'[') {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
    if stack.is_empty() {
        return s.to_string();
    }
    // Append the closers in reverse-open order. Cheap + unambiguous: every
    // unmatched opener gets its closer.
    let mut out = String::from(s);
    while let Some(opener) = stack.pop() {
        out.push(match opener {
            b'{' => '}',
            b'[' => ']',
            _ => unreachable!("stack only holds {{ or ["),
        });
    }
    out
}

// ── helpers ──

#[inline]
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// The last non-whitespace char in `s`. Used to detect key
/// position without scanning back through the whole output.
fn last_non_ws(s: &str) -> Option<char> {
    s.chars().rev().find(|&c| !matches!(c, ' ' | '\n' | '\t' | '\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- wrap_bare_object (2026-08-15 audit H1 regression) ----------

    /// Non-ASCII bare keys must NOT panic. The old run counted CHARS over
    /// Unicode-aware `is_alphanumeric` and sliced at that offset as BYTES —
    /// `é: 1` panicked (`byte index 1 is not a char boundary`). The ASCII-only
    /// byte run now stops at `é` → no wrap → the input passes through for
    /// serde to reject (the 3-pass loop's job, the conservative contract).
    /// Reachable from model output via `SchemaDelta::from_model_output` (the
    /// §5 fail-proof delta path) AND `bootstrap_anchors_from_intro` (which
    /// has no catch_unwind — a panic there killed game entry outright).
    #[test]
    fn repair_non_ascii_bare_key_does_not_panic() {
        assert_eq!(repair("é: 1"), "é: 1");
        assert_eq!(repair("名: \"x\""), "名: \"x\"");
        // Mixed: a valid ASCII key followed by non-ASCII prose VALUE is not a
        // bare-key shape at all — the wrap path must not touch it either.
        assert!(!repair("résumé").starts_with('{'));
    }

    /// The legit ASCII bare-key wrap still fires (the #49 behavior this
    /// shape exists for — unchanged by the fix).
    #[test]
    fn repair_ascii_bare_key_still_wraps() {
        let out = repair("summary: \"ok\"");
        assert!(out.starts_with('{') && out.ends_with('}'), "wrapped: {out}");
        assert!(serde_json::from_str::<serde_json::Value>(&out).is_ok());
    }

    // ---------- normalize_quotes ----------

    #[test]
    fn repairs_smart_double_quotes() {
        let repaired = repair("\u{201C}key\u{201D}: \u{201C}val\u{201D}");
        // After full repair: keys get quoted → {"key": "val"}
        assert_eq!(repaired, "{\"key\": \"val\"}");
    }

    #[test]
    fn repairs_smart_single_quotes() {
        let repaired = repair("'\u{2018}a\u{2019}'");
        assert!(repaired.contains("'a'"));
    }

    /// #31: legally-valid JSON with typographic quotes inside a string VALUE
    /// must survive repair unchanged. The old blind replace rewrote the inner
    /// “ ” to ASCII " — turning a pass-1 success into an INVALID document
    /// (`"she said "hello""`) that then burned all 3 LLM repair passes.
    #[test]
    fn preserves_smart_quotes_inside_string_values() {
        let valid = "{\"summary\": \"she said \u{201C}hello\u{201D} and left\"}";
        assert_eq!(repair(valid), valid);
        let parsed: serde_json::Value =
            serde_json::from_str(&repair(valid)).expect("must still parse");
        assert_eq!(parsed["summary"], "she said \u{201C}hello\u{201D} and left");
    }

    /// The repair side of #31 still works: a string DELIMITED by smart quotes
    /// (the model's substitute for ASCII ") is rewritten, mixed forms included.
    #[test]
    fn rewrites_smart_delimiters_in_mixed_form() {
        // Smart opener + smart closer → both become ASCII.
        assert_eq!(repair("{\u{201C}a\u{201D}: 1}"), "{\"a\": 1}");
        // Smart opener + ASCII closer (mixed) → both become ASCII.
        assert_eq!(repair("{\u{201C}a\": 1}"), "{\"a\": 1}");
    }

    /// #49: a brace-less body with an UNQUOTED leading key (`summary: "ok"`)
    /// must repair to a valid object. quote_unquoted_keys only fires after
    /// `{`/`,`, so the wrap pass (now first) must accept the bare-key shape
    /// and put the key into quotable position.
    #[test]
    fn repairs_bare_key_without_braces() {
        assert_eq!(repair("summary: \"ok\""), "{\"summary\": \"ok\"}");
        let parsed: serde_json::Value =
            serde_json::from_str(&repair("summary: \"ok\"")).expect("must parse");
        assert_eq!(parsed["summary"], "ok");
    }

    // ---------- strip_trailing_commas ----------

    #[test]
    fn repairs_trailing_comma_in_object() {
        assert_eq!(repair("{\"a\":1,}"), "{\"a\":1}");
    }

    #[test]
    fn repairs_trailing_comma_in_array() {
        assert_eq!(repair("[1,2,3,]"), "[1,2,3]");
    }

    #[test]
    fn repairs_trailing_comma_with_whitespace() {
        // The trim-back removes the comma + the whitespace between it and the
        // closer, so the closer sits flush against the value.
        assert_eq!(repair("{\"a\":1 , }"), "{\"a\":1}");
    }

    #[test]
    fn leaves_intra_comma_alone() {
        // A comma NOT before a closer stays.
        let r = repair("{\"a\":1,\"b\":2}");
        assert!(r.contains("\"a\":1"));
        assert!(r.contains("\"b\":2"));
    }

    // ---------- quote_unquoted_keys ----------

    #[test]
    fn repairs_unquoted_key() {
        assert_eq!(repair("{a:1}"), "{\"a\":1}");
    }

    #[test]
    fn repairs_namespaced_unquoted_key() {
        // The engine's keys are namespaced (char.mira.trust) — must survive.
        assert_eq!(repair("{char.mira.trust: 0.5}"), "{\"char.mira.trust\": 0.5}");
    }

    #[test]
    fn repairs_multiple_unquoted_keys() {
        assert_eq!(repair("{a:1,b:2}"), "{\"a\":1,\"b\":2}");
    }

    #[test]
    fn does_not_quote_bare_value() {
        // `true` as a VALUE is not a key — must not be wrapped.
        let r = repair("{\"flag\":true}");
        assert!(r.contains("true"));
        assert!(!r.contains("\"true\""));
    }

    #[test]
    fn respects_string_context() {
        // A colon inside a string value must not trigger key-quoting.
        let r = repair("{\"note\":\"a: b\"}");
        assert!(r.contains("\"a: b\""));
    }

    // ---------- escape_bare_newlines_in_strings ----------

    #[test]
    fn escapes_bare_newline_in_value() {
        let r = repair("{\"note\":\"line one\nline two\"}");
        assert!(r.contains("line one\\nline two"));
    }

    #[test]
    fn leaves_existing_escape_alone() {
        // A proper `\n` escape (backslash + n) is already valid — unchanged.
        let r = repair("{\"note\":\"ok\\n\"}");
        assert!(r.contains("\"ok\\n\""));
    }

    // ---------- strip_stray_code_fences ----------

    #[test]
    fn strips_leading_json_fence() {
        let r = repair("```json\n{\"a\":1}\n```");
        assert_eq!(r, "{\"a\":1}");
    }

    #[test]
    fn strips_bare_fences() {
        assert_eq!(repair("```\n42\n```"), "42");
    }

    // ---------- balance_brackets ----------

    #[test]
    fn appends_missing_closer() {
        // Truncated object missing its closer.
        assert_eq!(repair("{\"a\":1"), "{\"a\":1}");
    }

    #[test]
    fn appends_nested_missing_closers() {
        assert_eq!(repair("{\"a\":[1,2"), "{\"a\":[1,2]}");
    }

    #[test]
    fn ignores_brackets_inside_strings() {
        // A `{` inside a string must NOT count toward the stack.
        let r = repair("{\"note\":\"has {brace\"}");
        assert_eq!(r, "{\"note\":\"has {brace\"}");
    }

    #[test]
    fn preserves_multibyte_chars() {
        // P0 regression: the byte-as-char pushes re-encoded every multi-byte
        // UTF-8 char as Latin-1 mojibake ("Zoë — 刀" → "ZoÃ« â€” åˆ€").
        let r = repair("{\"summary\":\"Zoë — 刀\",\"note\":\"ok\"}");
        assert!(r.contains("Zoë — 刀"), "non-ASCII must survive repair verbatim: {r}");
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["summary"], "Zoë — 刀");
    }

    #[test]
    fn leaves_comma_before_bracket_inside_string_alone() {
        // P0 regression: the comma stripper wasn't string-aware — a `,`
        // immediately before `]`/`}` inside a string VALUE was deleted.
        let r = repair("{\"note\":\"bought bread, milk]\",\"a\":1}");
        assert!(r.contains("bought bread, milk]"), "intra-string `,]` is content: {r}");
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["note"], "bought bread, milk]");
    }

    // ---------- end-to-end: serde round-trips ----------

    #[test]
    fn end_to_end_trailing_comma_then_serde() {
        let broken = "{\"summary\":\"ok\",\"events\":[\"a\",\"b\",],}";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["summary"], "ok");
        assert_eq!(v["events"][1], "b");
    }

    #[test]
    fn end_to_end_unquoted_keys_then_serde() {
        let broken = "{summary:\"ok\",count:3}";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["summary"], "ok");
        assert_eq!(v["count"], 3);
    }

    #[test]
    fn end_to_end_truncation_then_serde() {
        // Model hit max_tokens mid-object.
        let broken = "{\"summary\":\"the party rested\",\"entities\":{\"char.mira.trust\":\"0.8\"";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["entities"]["char.mira.trust"], "0.8");
    }

    /// Truncation-at-comma (the most common truncation shape per the module
    /// doc): the input ends on a dangling comma, so the closer only appears
    /// via balance_brackets — the post-balance comma strip is what keeps
    /// this a microsecond fix instead of a 3-pass LLM repair loop.
    #[test]
    fn end_to_end_truncation_at_comma_then_serde() {
        let broken = "{\"summary\":\"ok\",";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["summary"], "ok");

        // Nested shape: comma dangling inside an array.
        let broken = "{\"summary\":\"ok\",\"events\":[\"a\",\"b\",";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["events"][1], "b");

        // Wrap path: a brace-less body truncated at the comma.
        let broken = "summary: \"ok\",";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["summary"], "ok");
    }

    #[test]
    fn end_to_end_smart_quotes_then_serde() {
        let broken = "\u{201C}summary\u{201D}: \u{201C}ok\u{201D}";
        let repaired = repair(broken);
        let v: serde_json::Value = serde_json::from_str(&repaired).expect("repaired parses");
        assert_eq!(v["summary"], "ok");
    }

    #[test]
    fn leaves_valid_json_unchanged_semantically() {
        let valid = "{\"summary\":\"ok\",\"events\":[\"a\"]}";
        let repaired = repair(valid);
        // Must still parse, and carry the same data.
        let orig: serde_json::Value = serde_json::from_str(valid).unwrap();
        let rep: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(orig, rep);
    }

    #[test]
    fn empty_object_unchanged() {
        assert_eq!(repair("{}"), "{}");
    }

    #[test]
    fn unrepairable_garbage_left_roughly_as_is() {
        // If it's truly broken, repair shouldn't make it WORSE or pretend it's
        // fixed. The result just won't parse; serde reports the error + the
        // 3-pass loop takes over. We assert it doesn't panic.
        let _ = repair("}}{{{not even close");
    }
}
