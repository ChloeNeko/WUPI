// =============================================================
// SAVED PLAYER — a standalone, reusable player identity library.
//
// A SavedPlayer is a PURE IDENTITY unit (no gameplay state — no body,
// stamina, wealth, reputation). It exists so a user can author a player
// once (name + appearance + personality + accessories + portrait) and
// attach it onto any sim card at game start, copying the name into the
// card's `player_name` anchor.
//
// This is distinct from `PlayerState` (player_state.rs), which is the
// in-session gameplay state (body mannequin / stamina / wealth). That
// stays per-card-session; SavedPlayer is the cross-card identity layer
// that sits ABOVE cards. Decoupled deliberately: identity is reusable,
// state is per-run.
//
// PERSISTENCE: one folder per player under `apps/fable/players/<id>/`:
//   <id>.json     — this struct, serialized
//   portrait.png  — optional uploaded portrait (referenced by `portrait`)
// Mirrors the per-card folder discipline (AGENTS.md §6B) at a sibling
// root. Atomic writes via `write_atomic` (lib.rs) — temp + fsync + rename.
//
// VALIDATION: `validate` is the structural + content gate (replaces the
// deleted "AI checker"). It runs server-side in the write IPC (the
// authoritative gate) AND has a client-side mirror pre-check in the
// Player Creator (keeps Save disabled until valid). Rejects empty names,
// oversize fields, and control characters (the load-bearing check the
// frontend XML sniff does NOT do).
// =============================================================

/// A standalone, reusable player identity. Serialized to
/// `apps/fable/players/<id>/<id>.json`. `id` is derived from `name`
/// (slugified) on write; the field is round-tripped for read paths.
///
/// `portrait` stores the relative filename ("portrait.png") so the
/// player folder can be moved without breaking the link. The read-side
/// IPC resolves it to an absolute path for `convertFileSrc`.
#[derive(Clone, serde::Serialize, serde::Deserialize, Debug)]
pub struct SavedPlayer {
    /// Slug identifier (also the folder name). Slugified from `name` on
    /// write by the IPC layer; round-tripped on read.
    #[serde(default)]
    pub id: String,

    /// The player's display name (e.g. "Kaelen", "Alex"). Required,
    /// non-empty after trim. This is what gets copied into the card's
    /// `player_name` anchor at game start.
    pub name: String,

    /// Free-form identity / backstory prose. Optional.
    #[serde(default)]
    pub description: Option<String>,

    /// Physical appearance prose. Optional.
    #[serde(default)]
    pub appearance: Option<String>,

    /// Personality / demeanor / voice prose. Optional.
    #[serde(default)]
    pub personality: Option<String>,

    /// Signature carried items / accessories prose. Optional.
    #[serde(default)]
    pub accessories: Option<String>,

    /// Relative portrait filename within the player folder (e.g.
    /// "portrait.png"). None when no portrait was uploaded. The read
    /// IPC resolves this to an absolute path for convertFileSrc.
    #[serde(default)]
    pub portrait: Option<String>,

    /// Creation timestamp (epoch-ms). Decorative; set on write.
    #[serde(default)]
    pub created_at_ms: i64,
}

/// Lightweight metadata for the player-picker list. Carries enough for
/// a tile UI (name, id, portrait flag) without loading every player's
/// full prose body.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlayerMeta {
    pub id: String,
    pub name: String,
    /// Whether a portrait file exists for this player (best-effort:
    /// a stat error degrades to false).
    pub has_portrait: bool,
}

// --- Field length caps (the validation authority) ----------------
//
// Generous enough for rich prose, tight enough to prevent a single
// player file from bloating retrieval or the picker. Name is short
// (it anchors the narrator + the picker tile); prose fields allow a
// substantial paragraph each.
const NAME_MAX: usize = 64;
const PROSE_MAX: usize = 4000;

/// Validate a SavedPlayer's structure + content. The authoritative gate
/// (runs server-side in `fable_player_write`). Returns a human-readable
/// error string on failure (surfaced to the Creator toast), or Ok on
/// success.
///
/// Checks:
///   • name required + non-empty after trim + ≤ NAME_MAX chars
///   • each prose field (if present) ≤ PROSE_MAX chars
///   • NO control characters ([\x00-\x08\x0B\x0C\x0E-\x1F]) in any
///     string field — the load-bearing sanitization the frontend XML
///     sniff does not do. Newlines (\x0A) + tabs (\x09) are allowed
///     (legitimate prose formatting); the C1 range (\x80-\x9F) is left
///     to the serializer (rare in practice, not a structuring risk).
pub fn validate_player(p: &SavedPlayer) -> Result<(), String> {
    let name = p.name.trim();
    if name.is_empty() {
        return Err("Name is required.".into());
    }
    if name.len() > NAME_MAX {
        return Err(format!("Name must be {} characters or fewer.", NAME_MAX));
    }
    if has_control_chars(&p.name) {
        return Err("Name contains invalid control characters.".into());
    }
    for (label, val) in [
        ("Description", &p.description),
        ("Appearance", &p.appearance),
        ("Personality", &p.personality),
        ("Accessories", &p.accessories),
    ] {
        if let Some(s) = val {
            let s = s.trim();
            if s.len() > PROSE_MAX {
                return Err(format!("{} must be {} characters or fewer.", label, PROSE_MAX));
            }
            if has_control_chars(s) {
                return Err(format!("{} contains invalid control characters.", label));
            }
        }
    }
    Ok(())
}

/// Detect forbidden control characters (excluding tab \x09 + newline
/// \x0A + carriage return \x0D, which are legitimate prose). The
/// C0 control range minus those three is the rejection set.
fn has_control_chars(s: &str) -> bool {
    s.chars().any(|c| {
        let code = c as u32;
        (code <= 0x08) || code == 0x0B || code == 0x0C || (0x0E..=0x1F).contains(&code)
    })
}

/// Slugify a player name into a filesystem-safe id (lowercase,
/// non-alphanumerics → dashes, trimmed). Mirrors `slugify_card_stem`
/// (lib.rs) so player ids share the card-id discipline. Returns None
/// when the name reduces to empty (no usable slug) — the validator
/// rejects empty names first, so this is belt-and-suspenders.
pub fn slugify_player_id(name: &str) -> Option<String> {
    let stem: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_owned();
    if stem.is_empty() { None } else { Some(stem) }
}

/// Image magic-byte validation for portrait uploads. Accepts PNG + JPEG
/// only (the format set the dialog filter restricts to). Rejects
/// anything else BEFORE writing to disk — the load-bearing gate against
/// a malformed file masquerading as an image. Returns the detected
/// extension ("png" / "jpg") on success.
pub fn validate_image_magic(bytes: &[u8]) -> Result<&'static str, String> {
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Ok("png");
    }
    // JPEG: FF D8 FF
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok("jpg");
    }
    Err("Portrait must be a PNG or JPEG image.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> SavedPlayer {
        SavedPlayer {
            id: String::new(),
            name: name.into(),
            description: None,
            appearance: None,
            personality: None,
            accessories: None,
            portrait: None,
            created_at_ms: 0,
        }
    }

    #[test]
    fn validate_rejects_empty_name() {
        assert!(validate_player(&p("")).is_err());
        assert!(validate_player(&p("   ")).is_err());
    }

    #[test]
    fn validate_accepts_simple_name() {
        assert!(validate_player(&p("Kaelen")).is_ok());
    }

    #[test]
    fn validate_rejects_oversize_name() {
        let long = "x".repeat(NAME_MAX + 1);
        assert!(validate_player(&p(&long)).is_err());
        let exact = "x".repeat(NAME_MAX);
        assert!(validate_player(&p(&exact)).is_ok());
    }

    #[test]
    fn validate_rejects_control_chars() {
        let mut bad = p("Alex");
        bad.description = Some("has a null\0 here".into());
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_allows_newlines_in_prose() {
        let mut ok = p("Alex");
        ok.description = Some("line one\nline two\r\nthree".into());
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify_player_id("Kaelen Voss"), Some("kaelen-voss".into()));
        assert_eq!(slugify_player_id("  Alex!  "), Some("alex".into()));
        assert_eq!(slugify_player_id("---"), None);
    }

    #[test]
    fn magic_png() {
        let png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        assert_eq!(validate_image_magic(&png).unwrap(), "png");
    }

    #[test]
    fn magic_jpeg() {
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00];
        assert_eq!(validate_image_magic(&jpg).unwrap(), "jpg");
    }

    #[test]
    fn magic_rejects_unknown() {
        assert!(validate_image_magic(b"not an image").is_err());
        assert!(validate_image_magic(&[]).is_err());
    }
}
