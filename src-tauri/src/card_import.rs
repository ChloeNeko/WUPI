//! Universal card-file import (Slice A of the "Create vs Import" New Game flow).
//!
//! Reads the two common portable card formats that circulate across AI-chat
//! ecosystems and reduces them to a neutral [`RawCard`] / [`RawLorebook`]
//! shape that the decipher pass (`import_decipher_card`) feeds to the model.
//!
//! **Rust owns structure here; the model owns prose.** This module never
//! writes a `.sim` or codex file — it only *parses*. The decipher IPC builds
//! an [`crate::interview_draft::InterviewDraft`] from the parsed fields and
//! lets the model rebuild the prose. Structure never goes to the model; prose
//! never bypasses Rust validation.
//!
//! Supported inputs (universal — never named after any one tool in code/UI):
//! - **PNG with an embedded card.** The card JSON lives in a `tEXt` (or
//!   `iTXt`) chunk keyed `chara`, base64-encoded. We hand-roll both the PNG
//!   chunk walker and the base64 decode — zero new crates (the project
//!   intentionally stays lean; base64 adds a dep for ~30 lines of code).
//! - **Plain card JSON.** Both the flat ("V1") shape (`name`, `description`,
//!   `first_mes` at the root) and the nested ("V2") shape (same fields under
//!   a `data` object) are auto-detected and normalized to [`RawCard`].
//! - **Lorebook JSON.** Detected by an `entries` object map; normalized to
//!   [`RawLorebook`]. The legacy keyword/trigger%/position/order/strategy
//!   machinery is dropped on the floor — WUPI's hybrid retrieval (BM25 +
//!   dense cosine fused via RRF) replaces all of it. We keep only the title
//!   (`comment`/`name`) and `content`.
//!
//! Everything is a pure function — fully unit-testable with synthetic inputs,
//! no AppState, no filesystem, no model calls.

use serde_json::Value;

/// The union of fields across the common card JSON shapes, captured RAW (the
/// un-deciphered source text). These strings are the INPUT to the AI rebuild;
/// they are never written to a `.sim` directly. Every field is optional
/// because cards vary wildly in completeness.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawCard {
    pub name: Option<String>,
    pub description: Option<String>,
    pub personality: Option<String>,
    pub scenario: Option<String>,
    pub first_mes: Option<String>,
    pub mes_example: Option<String>,
    pub creator_notes: Option<String>,
    pub tags: Vec<String>,
    pub alternate_greetings: Vec<String>,
}

/// One lorebook entry, reduced to just what WUPI's codex cares about. The
/// legacy activation machinery (keys, trigger %, position, order, strategy,
/// selective/constant flags) is intentionally dropped — the hybrid memory
/// engine handles relevance retrieval without it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLoreEntry {
    /// Prefer the human `comment`/`name`; falls back to the entry's key.
    pub title: String,
    /// The entry body. May be long; the decipher pass may split it.
    pub content: String,
    /// Any tags the entry carried (optional flavor).
    pub tags: Vec<String>,
}

/// A parsed lorebook: a flat list of entries plus a name for the file stem.
#[derive(Debug, Clone, Default)]
pub struct RawLorebook {
    pub name: Option<String>,
    pub entries: Vec<RawLoreEntry>,
}

/// What a parsed import payload can be. A single file is exactly one of these
/// (a character card PNG/JSON, or a lorebook JSON). The decipher IPC branches
/// on this.
#[derive(Debug, Clone)]
pub enum ParsedImport {
    Card(RawCard),
    Lorebook(RawLorebook),
}

// ---------------------------------------------------------------------------
// PNG: chunk walk → find `chara` tEXt/iTXt → base64-decode
// ---------------------------------------------------------------------------

/// PNG signature (8 bytes).
const PNG_SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// Extract the embedded card JSON from a PNG's `chara` chunk, if present.
///
/// Walks the PNG chunk stream (8-byte signature, then repeated
/// `[length:u32 BE][type:4][payload:length][crc:u32]`). The card JSON is
/// conventionally stored in a `tEXt` chunk with keyword `chara`, base64-
/// encoded. Some tools emit `iTXt` instead (optionally compressed), so we
/// handle both. Returns the decoded JSON string, or `None` if no `chara`
/// chunk exists or the bytes aren't a valid PNG.
///
/// This is intentionally tolerant: a truncated/malformed chunk after we've
/// already found the `chara` payload is fine (we return what we have). We
/// only need the one chunk; we don't care about the image integrity.
pub fn extract_chara_from_png(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 8 || &bytes[..8] != PNG_SIG {
        return None;
    }
    let mut pos = 8;
    while pos + 8 <= bytes.len() {
        // length is big-endian u32; type is 4 ASCII bytes.
        let len = u32::from_be_bytes([
            bytes[pos],
            bytes[pos + 1],
            bytes[pos + 2],
            bytes[pos + 3],
        ]) as usize;
        let type_str = std::str::from_utf8(&bytes[pos + 4..pos + 8]).ok()?;
        let data_start = pos + 8;
        let data_end = data_start.checked_add(len)?;
        if data_end > bytes.len() {
            break; // truncated chunk — stop walking.
        }
        let payload = &bytes[data_start..data_end];
        // Advance past payload + 4-byte CRC.
        pos = data_end + 4;

        if let Some(json_b64) = chara_text_from_chunk(type_str, payload) {
            // The decoded text is the card JSON string.
            if let Ok(json) = decode_base64_to_string(&json_b64) {
                return Some(json);
            }
        }
    }
    None
}

/// Pull the base64 card text out of a `tEXt`/`iTXt` chunk if it's the `chara`
/// entry. Returns the (still base64-encoded) card string.
fn chara_text_from_chunk(chunk_type: &str, payload: &[u8]) -> Option<String> {
    match chunk_type {
        "tEXt" => {
            // tEXt: `keyword\0text`. Keyword + text are latin-1 in spec, but
            // chara payloads are base64 (ASCII), so utf8 is safe in practice.
            let (keyword, rest) = split_bytes_once_null(payload)?;
            if keyword == "chara" {
                Some(String::from_utf8_lossy(rest).into_owned())
            } else {
                None
            }
        }
        "iTXt" => {
            // iTXt layout: `keyword\0comp_flag(1)comp_method(1)lang\0trans_key\0text`.
            let (keyword, rest) = split_bytes_once_null(payload)?;
            if keyword != "chara" {
                return None;
            }
            if rest.len() < 2 {
                return None;
            }
            let compressed = rest[0] != 0;
            // Skip comp_method byte, then two null-terminated latin-1 strings
            // (lang tag + translated keyword), then the text.
            let mut p = &rest[2..];
            let (_lang, after_lang) = split_bytes_once_null(p)?;
            p = after_lang; // advance past lang\0
            let (_trans, after_trans) = split_bytes_once_null(p)?;
            p = after_trans; // advance past trans\0
            if compressed {
                // Compressed iTXt uses zlib/deflate. We don't pull a dep for
                // this rare path — fall back to None and let the caller try
                // tEXt elsewhere. (Virtually all card PNGs use uncompressed
                // tEXt; compressed iTXt chara is exotic.)
                None
            } else {
                Some(String::from_utf8_lossy(p).into_owned())
            }
        }
        _ => None,
    }
}

/// Split a byte slice on the first NUL into (before-as-string, after-slice).
/// `before` is UTF-8 (keywords are ASCII); `after` stays raw bytes because the
/// iTXt caller may need to keep slicing it.
fn split_bytes_once_null(payload: &[u8]) -> Option<(&str, &[u8])> {
    let idx = payload.iter().position(|&b| b == 0)?;
    let kw = std::str::from_utf8(&payload[..idx]).ok()?;
    Some((kw, &payload[idx + 1..]))
}

// ---------------------------------------------------------------------------
// base64 decode (hand-rolled — no crate dep)
// ---------------------------------------------------------------------------

const B64_DEC: [i8; 256] = {
    let mut t = [-1i8; 256];
    let mut i = 0u8;
    while i < 26 {
        t[(b'A' + i) as usize] = i as i8;
        i += 1;
    }
    let mut i = 0u8;
    while i < 26 {
        t[(b'a' + i) as usize] = (26 + i) as i8;
        i += 1;
    }
    let mut i = 0u8;
    while i < 10 {
        t[(b'0' + i) as usize] = (52 + i) as i8;
        i += 1;
    }
    t[b'+' as usize] = 62;
    t[b'/' as usize] = 63;
    t
};

/// Decode a base64 string to bytes. Whitespace/newlines tolerated; padding
/// optional. Returns the raw bytes (the card JSON is UTF-8).
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = B64_DEC[c as usize];
        if v < 0 {
            return None; // invalid character
        }
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8 & 0xFF);
        }
    }
    Some(out)
}

fn decode_base64_to_string(input: &str) -> Result<String, &'static str> {
    decode_base64(input)
        .ok_or("invalid base64")
        .and_then(|b| String::from_utf8(b).map_err(|_| "decoded bytes are not UTF-8"))
}

// ---------------------------------------------------------------------------
// JSON: card (V1 flat / V2 nested) + lorebook
// ---------------------------------------------------------------------------

/// Parse a card-or-lorebook JSON string. Auto-detects shape:
/// - lorebook if the root (or `data`) has an `entries` object;
/// - otherwise a character card (V1 root fields, or V2 fields under `data`).
///
/// Tolerant: missing/extra fields are fine. The result is the raw, un-
/// deciphered text — the model does the prose rebuild downstream.
pub fn parse_import_json(json_str: &str) -> Result<ParsedImport, String> {
    let root: Value = serde_json::from_str(json_str).map_err(|e| format!("invalid JSON: {e}"))?;
    // Some lorebooks/cards wrap everything in a top-level object whose only
    // real key is `data`; normalize by pointing at `data` when it's an object
    // AND the root itself has no recognized fields.
    let card_obj = best_card_object(&root);

    // Lorebook detection: an `entries` map anywhere we'd look for card fields.
    if let Some(entries) = card_obj.get("entries").and_then(|v| v.as_object()) {
        if !entries.is_empty() {
            return Ok(ParsedImport::Lorebook(parse_lorebook(&root, entries)));
        }
    }

    Ok(ParsedImport::Card(parse_card(&root)))
}

/// Pick the object to read card fields from: `data` if present + object,
/// otherwise the root. (V2 nests everything under `data`; V1 is flat.)
fn best_card_object(root: &Value) -> &Value {
    if let Some(data) = root.get("data") {
        if data.is_object() {
            return data;
        }
    }
    root
}

fn parse_card(root: &Value) -> RawCard {
    let o = best_card_object(root);
    RawCard {
        name: str_field(o, "name").or_else(|| str_field(root, "name")),
        description: str_field(o, "description"),
        personality: str_field(o, "personality"),
        scenario: str_field(o, "scenario"),
        first_mes: str_field(o, "first_mes"),
        mes_example: str_field(o, "mes_example"),
        creator_notes: str_field(o, "creator_notes"),
        tags: str_array(o, "tags"),
        alternate_greetings: str_array(o, "alternate_greetings"),
    }
}

fn parse_lorebook(root: &Value, entries: &serde_json::Map<String, Value>) -> RawLorebook {
    let name = best_card_object(root)
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let mut out = Vec::with_capacity(entries.len());
    for (key, val) in entries {
        // Operate on the entry as a `&Value` (str_field/str_array are Value-based).
        if val.as_object().is_none() {
            continue;
        }
        let title = str_field(val, "comment")
            .or_else(|| str_field(val, "name"))
            .unwrap_or_else(|| key.clone());
        let content = str_field(val, "content").unwrap_or_default();
        if content.trim().is_empty() {
            continue; // skip empty entries
        }
        out.push(RawLoreEntry {
            title,
            content,
            tags: str_array(val, "keys"), // keys survive as flavor tags
        });
    }
    RawLorebook { name, entries: out }
}

fn str_field(o: &Value, key: &str) -> Option<String> {
    let s = o.get(key)?.as_str()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_owned())
    }
}

fn str_array(o: &Value, key: &str) -> Vec<String> {
    o.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Public entry point: bytes in → ParsedImport out
// ---------------------------------------------------------------------------

/// Parse raw file bytes (from a drop or a file pick) into a [`ParsedImport`].
/// Detects PNG by signature; otherwise treats the bytes as UTF-8 JSON.
pub fn parse_import_bytes(bytes: &[u8]) -> Result<ParsedImport, String> {
    if bytes.len() >= 8 && &bytes[..8] == PNG_SIG {
        let json = extract_chara_from_png(bytes)
            .ok_or_else(|| "no embedded card found in PNG".to_string())?;
        return parse_import_json(&json);
    }
    let text = String::from_utf8_lossy(bytes);
    parse_import_json(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- base64 ---------------------------------------------------------------

    #[test]
    fn b64_decodes_standard() {
        assert_eq!(decode_base64_to_string("aGVsbG8=").unwrap(), "hello");
        assert_eq!(decode_base64_to_string("Zm9vYmFy").unwrap(), "foobar");
    }

    #[test]
    fn b64_tolerates_whitespace_and_no_padding() {
        assert_eq!(
            decode_base64_to_string("aGVs\nbG8").unwrap(),
            "hello"
        );
    }

    #[test]
    fn b64_rejects_garbage() {
        assert!(decode_base64_to_string("@@@@").is_err());
    }

    // -- JSON card detection --------------------------------------------------

    #[test]
    fn parses_v1_flat_card() {
        let j = r#"{
            "name": "Mira",
            "description": "A quick-witted smuggler.",
            "personality": "brave, reckless",
            "scenario": "A cantina on the frontier.",
            "first_mes": "Mira looks up. *Well?*",
            "tags": ["scifi", "rogue"]
        }"#;
        let p = parse_import_json(j).unwrap();
        match p {
            ParsedImport::Card(c) => {
                assert_eq!(c.name.as_deref(), Some("Mira"));
                assert_eq!(c.first_mes.as_deref(), Some("Mira looks up. *Well?*"));
                assert_eq!(c.tags, vec!["scifi".to_string(), "rogue".to_string()]);
            }
            _ => panic!("expected a card"),
        }
    }

    #[test]
    fn parses_v2_nested_card() {
        let j = r#"{
            "spec": "chara_card_v2",
            "data": {
                "name": "Kael",
                "description": "An aging knight.",
                "first_mes": "*He offers a nod.*"
            }
        }"#;
        let p = parse_import_json(j).unwrap();
        match p {
            ParsedImport::Card(c) => {
                assert_eq!(c.name.as_deref(), Some("Kael"));
                assert_eq!(c.description.as_deref(), Some("An aging knight."));
            }
            _ => panic!("expected a card"),
        }
    }

    #[test]
    fn card_with_empty_fields_drops_them() {
        let j = r#"{"name": "X", "description": "   "}"#;
        let p = parse_import_json(j).unwrap();
        match p {
            ParsedImport::Card(c) => {
                assert_eq!(c.name.as_deref(), Some("X"));
                assert!(c.description.is_none());
            }
            _ => panic!("expected a card"),
        }
    }

    #[test]
    fn detects_lorebook_by_entries_map() {
        let j = r#"{
            "name": "World Lore",
            "entries": {
                "0": {"comment": "Elves", "content": "Elves are tall.", "keys": ["elf"]},
                "1": {"comment": "Dwarves", "content": "Dwarves mine."}
            }
        }"#;
        let p = parse_import_json(j).unwrap();
        match p {
            ParsedImport::Lorebook(lb) => {
                assert_eq!(lb.name.as_deref(), Some("World Lore"));
                assert_eq!(lb.entries.len(), 2);
                assert_eq!(lb.entries[0].title, "Elves");
                assert_eq!(lb.entries[0].tags, vec!["elf".to_string()]);
            }
            _ => panic!("expected a lorebook"),
        }
    }

    #[test]
    fn lorebook_skips_empty_entries() {
        let j = r#"{"entries": {"0": {"comment": "X", "content": "  "}}}"#;
        let p = parse_import_json(j).unwrap();
        match p {
            ParsedImport::Lorebook(lb) => assert!(lb.entries.is_empty()),
            _ => panic!("expected a lorebook"),
        }
    }

    #[test]
    fn lorebook_title_falls_back_to_key() {
        let j = r#"{"entries": {"abc123": {"content": "lore body"}}}"#;
        let p = parse_import_json(j).unwrap();
        match p {
            ParsedImport::Lorebook(lb) => {
                assert_eq!(lb.entries[0].title, "abc123");
                assert_eq!(lb.entries[0].content, "lore body");
            }
            _ => panic!("expected a lorebook"),
        }
    }

    #[test]
    fn rejects_garbage_json() {
        assert!(parse_import_json("not json").is_err());
    }

    // -- PNG chunk extraction -------------------------------------------------

    /// Build a minimal PNG (sig + IHDR + one tEXt chunk + IEND) carrying a
    /// base64 card JSON in a `chara` tEXt chunk.
    fn png_with_chara_text(keyword: &str, b64_text: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PNG_SIG);
        // IHDR (13 bytes data) — contents don't matter for chara extraction.
        out.extend(make_chunk(b"IHDR", &[0; 13]));
        // tEXt: keyword \0 text
        let mut text_payload = Vec::new();
        text_payload.extend_from_slice(keyword.as_bytes());
        text_payload.push(0);
        text_payload.extend_from_slice(b64_text.as_bytes());
        out.extend(make_chunk(b"tEXt", &text_payload));
        // IEND
        out.extend(make_chunk(b"IEND", &[]));
        out
    }

    fn make_chunk(type_: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = Vec::with_capacity(12 + data.len());
        c.extend_from_slice(&(data.len() as u32).to_be_bytes());
        c.extend_from_slice(type_);
        c.extend_from_slice(data);
        // CRC over type + data (we don't verify CRC on read, so a zero is fine).
        c.extend_from_slice(&0u32.to_be_bytes());
        c
    }

    #[test]
    fn png_extracts_chara_text_chunk() {
        let card_json = r#"{"name":"Test","description":"d"}"#;
        let b64 = encode_b64(card_json.as_bytes());
        let png = png_with_chara_text("chara", &b64);
        let got = extract_chara_from_png(&png).unwrap();
        assert_eq!(got, card_json);
    }

    #[test]
    fn png_ignores_non_chara_text_chunk() {
        let png = png_with_chara_text("author", &encode_b64(b"{}"));
        assert!(extract_chara_from_png(&png).is_none());
    }

    #[test]
    fn png_rejects_non_png_bytes() {
        assert!(extract_chara_from_png(b"not a png").is_none());
        assert!(extract_chara_from_png(&[]).is_none());
    }

    #[test]
    fn parse_import_bytes_routes_png() {
        let card_json = r#"{"name":"Bytes","first_mes":"hi"}"#;
        let png = png_with_chara_text("chara", &encode_b64(card_json.as_bytes()));
        match parse_import_bytes(&png).unwrap() {
            ParsedImport::Card(c) => assert_eq!(c.name.as_deref(), Some("Bytes")),
            _ => panic!("expected card"),
        }
    }

    #[test]
    fn parse_import_bytes_routes_json() {
        let j = br#"{"name":"J","description":"x"}"#;
        match parse_import_bytes(j).unwrap() {
            ParsedImport::Card(c) => assert_eq!(c.name.as_deref(), Some("J")),
            _ => panic!("expected card"),
        }
    }

    // Tiny reference base64 encoder for test fixtures.
    fn encode_b64(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | bytes[i + 2] as u32;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
            out.push(T[(n & 63) as usize] as char);
            i += 3;
        }
        let rem = bytes.len() - i;
        if rem == 1 {
            let n = (bytes[i] as u32) << 16;
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push('=');
            out.push('=');
        } else if rem == 2 {
            let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
            out.push(T[((n >> 18) & 63) as usize] as char);
            out.push(T[((n >> 12) & 63) as usize] as char);
            out.push(T[((n >> 6) & 63) as usize] as char);
            out.push('=');
        }
        out
    }
}
