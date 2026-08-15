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
    let s = quote_unquoted_keys(&s);
    let s = wrap_bare_object(&s);
    let s = escape_bare_newlines_in_strings(&s);
    let s = strip_stray_code_fences(&s);
    balance_brackets(&s)
}

/// Wrap a top-level brace-less object body in `{…}`. A truncated/mangled LLM
/// emit can drop the outer braces entirely (`"summary": "ok"` — smart quotes
/// normalized, keys quoted, but no `{`/`}` in sight). Unambiguous shape: the
/// trimmed input STARTS with a string opener AND contains a top-level `:`
/// (outside any string). A bare top-level JSON string (`"hello"` — no
/// top-level colon), number, bool, or already-braced/bracketed value is left
/// alone (they're valid JSON as-is; wrapping would corrupt them).
fn wrap_bare_object(s: &str) -> String {
    let t = s.trim();
    if !t.starts_with('"') {
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
/// (U+201C/U+201D) and `' '` (U+2018/U+2019) even inside JSON they intend to
/// be valid. serde_json rejects these as invalid string delimiters.
///
/// We swap ALL of them (not just the ones adjacent to a colon/comma): an LLM
/// rarely mixes smart and dumb quotes intentionally, and a value containing a
/// literal smart quote (e.g. a character note) is unaffected by becoming ASCII
/// (the semantic content is identical).
fn normalize_quotes(s: &str) -> String {
    s.replace('\u{201C}', "\"") // "
        .replace('\u{201D}', "\"") // "
        .replace('\u{2018}', "'") // '
        .replace('\u{2019}', "'") // '
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
