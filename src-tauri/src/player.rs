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

    /// Free-form identity / backstory prose. LEGACY — the Player Creator no
    /// longer emits this (2026-08-04 overhaul: players don't have prose
    /// fields; identity is the structured trait set below). Retained on the
    /// struct + in the validator so pre-overhaul JSON + any future writer
    /// stays bounded + deserializes cleanly. Optional.
    ///
    /// `skip_serializing_if` (2026-08-05): a `None` value is OMITTED from the
    /// JSON (not written as `null`) — the Player Creator never populates
    /// these, so new player files stay clean of dead `"description": null`
    /// keys. The `#[serde(default)]` still deserializes old JSON that carries
    /// the field, so back-compat reads are unaffected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Physical appearance prose. LEGACY (see `description`). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appearance: Option<String>,

    /// Personality / demeanor / voice prose. LEGACY (see `description`).
    /// NPC-only in practice — the cast editor writes this, not the Player
    /// Creator. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,

    /// Signature carried items / accessories prose. LEGACY (see
    /// `description`). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessories: Option<String>,

    // --- Structured identity/appearance traits (2026-08-04 overhaul).
    // The Player Creator's slide-by-slide wizard writes these directly
    // (typed fields, not composed prose) so the tracker pipeline can
    // target individual traits by stable key via `[APPEARANCE key=value]`
    // and so the SIM-card review screen can render a clean breakdown.
    // All optional + defaulted for back-compat with pre-overhaul JSON.

    /// `"male"` | `"female"`. The paperdoll driver — written to the
    /// SAME `localStorage['wupi.paperdoll.gender']` key the Left Drawer
    /// HUD reads, and round-tripped here so a reattach keeps the choice.
    #[serde(default)]
    pub gender: Option<String>,

    /// Race / species / lineage ("human", "high elf", "orc"). Optional.
    #[serde(default)]
    pub race: Option<String>,

    /// Age as free text (accepts digits OR words like "young adult").
    /// Stored as a string to avoid integer-parsing risk on "eternal"
    /// / "appears 30" style answers. Optional.
    #[serde(default)]
    pub age: Option<String>,

    /// Height as free text ("6'1\"", "tall", "182 cm"). Optional.
    #[serde(default)]
    pub height: Option<String>,

    /// Weight as free text ("lean", "180 lbs"). Optional.
    #[serde(default)]
    pub weight: Option<String>,

    /// Hair color / length / style — three independent trait fields.
    #[serde(default)]
    pub hair_color: Option<String>,
    #[serde(default)]
    pub hair_length: Option<String>,
    #[serde(default)]
    pub hair_style: Option<String>,

    /// Build / frame ("wiry", "broad-shouldered"). Optional.
    #[serde(default)]
    pub body_type: Option<String>,

    /// Skin tone / complexion ("pale", "weathered bronze"). Optional.
    #[serde(default)]
    pub skin_complexion: Option<String>,

    /// Eye color ("hazel", "one gold, one blue"). Optional.
    #[serde(default)]
    pub eye_color: Option<String>,

    // --- Conditional traits. When the Creator's Yes/No toggle is No,
    // these stay None and are OMITTED from the JSON entirely
    // (`skip_serializing_if`) so the file + the tracker pipeline never
    // carry a "no" — the trait simply doesn't exist for this character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breast_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ears: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,

    /// Clothing / outfit items as a chip list from the dynamic-list
    /// slide. Each entry is one garment ("travel cloak", "leather
    /// boots"). None when the slide was left empty (omitted from JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clothing: Option<Vec<String>>,

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
/// a mini-SIM-card tile UI (name, race, gender, id, portrait flag)
/// without loading every player's full prose body — the grid renders
/// straight off this, deferring the full `fable_player_get` to the
/// click-to-expand modal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlayerMeta {
    pub id: String,
    pub name: String,
    /// Whether a portrait file exists for this player (best-effort:
    /// a stat error degrades to false).
    pub has_portrait: bool,
    /// `"male"` | `"female"` | None — surfaces on the mini-card so the
    /// ♂/♀ glyph can render without a full load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    /// Race / lineage for the mini-card subtitle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub race: Option<String>,
    // Identity fields surfaced on the mini-card's info strip (2026-08-04 Chloe
    // pass: "include all the identity information at the bottom of the card
    // besides gender"). Each is None when the player never set it → omitted
    // from JSON by skip_serializing_if.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<String>,
}

// --- Field length caps (the validation authority) ----------------
//
// Generous enough for rich prose, tight enough to prevent a single
// player file from bloating retrieval or the picker. Name is short
// (it anchors the narrator + the picker tile); prose fields allow a
// substantial paragraph each. Trait fields (single-value appearance
// answers from the wizard slides) get a tighter cap — they're labels,
// not prose.
const NAME_MAX: usize = 64;
const PROSE_MAX: usize = 4000;
const TRAIT_MAX: usize = 128;

/// Validate a SavedPlayer's structure + content. The authoritative gate
/// (runs server-side in `fable_player_write`). Returns a human-readable
/// error string on failure (surfaced to the Creator toast), or Ok on
/// success.
///
/// Checks:
///   • name required + non-empty after trim + ≤ NAME_MAX chars
///   • each prose field (if present) ≤ PROSE_MAX chars
///   • each trait field (if present) ≤ TRAIT_MAX chars + non-empty
///     after trim (empty traits should be None, not "")
///   • gender (if present) ∈ {"male", "female"}
///   • clothing (if present): each entry ≤ TRAIT_MAX + non-empty +
///     no control chars
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
    // Gender: if present must be one of the two paperdoll-driving values.
    if let Some(g) = &p.gender {
        let g = g.trim();
        if !g.eq_ignore_ascii_case("male") && !g.eq_ignore_ascii_case("female") {
            return Err("Gender must be 'male' or 'female'.".into());
        }
    }
    // Single-value trait fields. These come from wizard slides; an empty
    // string is a mistake (the slide should have left the field None).
    for (label, val) in trait_fields(p) {
        if let Some(s) = val {
            let s = s.trim();
            if s.is_empty() {
                return Err(format!("{} cannot be empty.", label));
            }
            if s.len() > TRAIT_MAX {
                return Err(format!("{} must be {} characters or fewer.", label, TRAIT_MAX));
            }
            if has_control_chars(s) {
                return Err(format!("{} contains invalid control characters.", label));
            }
        }
    }
    // Clothing chip list: each entry must be a non-empty, bounded label.
    if let Some(items) = &p.clothing {
        for c in items {
            let c = c.trim();
            if c.is_empty() {
                return Err("Clothing entries cannot be empty.".into());
            }
            if c.len() > TRAIT_MAX {
                return Err(format!("Each clothing entry must be {} characters or fewer.", TRAIT_MAX));
            }
            if has_control_chars(c) {
                return Err("A clothing entry contains invalid control characters.".into());
            }
        }
    }
    Ok(())
}

/// Collect the single-value trait fields into a (label, value) list so
/// the validator loop can apply one cap + control-char rule to all of
/// them. Keeps the trait set in one place (add a slide → add one line).
fn trait_fields(p: &SavedPlayer) -> Vec<(&'static str, &Option<String>)> {
    vec![
        ("Race", &p.race),
        ("Age", &p.age),
        ("Height", &p.height),
        ("Weight", &p.weight),
        ("Hair color", &p.hair_color),
        ("Hair length", &p.hair_length),
        ("Hair style", &p.hair_style),
        ("Body type", &p.body_type),
        ("Skin complexion", &p.skin_complexion),
        ("Eye color", &p.eye_color),
        ("Breast size", &p.breast_size),
        ("Ears", &p.ears),
        ("Tail", &p.tail),
    ]
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
            gender: None,
            race: None,
            age: None,
            height: None,
            weight: None,
            hair_color: None,
            hair_length: None,
            hair_style: None,
            body_type: None,
            skin_complexion: None,
            eye_color: None,
            breast_size: None,
            ears: None,
            tail: None,
            clothing: None,
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
    fn validate_rejects_bad_gender() {
        let mut bad = p("Alex");
        bad.gender = Some("helicopter".into());
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_accepts_case_insensitive_gender() {
        let mut ok = p("Alex");
        ok.gender = Some("FEMALE".into());
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn validate_rejects_empty_trait() {
        let mut bad = p("Alex");
        bad.race = Some("   ".into());
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_rejects_oversize_trait() {
        let mut bad = p("Alex");
        bad.eye_color = Some("x".repeat(TRAIT_MAX + 1));
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_rejects_empty_clothing_entry() {
        let mut bad = p("Alex");
        bad.clothing = Some(vec!["cloak".into(), "   ".into()]);
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_accepts_full_trait_set() {
        let mut ok = p("Kaelen");
        ok.gender = Some("male".into());
        ok.race = Some("half-elf".into());
        ok.age = Some("32".into());
        ok.hair_color = Some("raven black".into());
        ok.breast_size = None; // conditional toggle was No → omitted
        ok.tail = Some("panther, prehensile".into());
        ok.clothing = Some(vec!["travel cloak".into(), "leather boots".into()]);
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn conditional_traits_omitted_from_json_when_none() {
        // The skip_serializing_if contract: breast_size/ears/tail/clothing
        // vanish from the serialized JSON when None (the Yes/No toggle was No).
        let mut sp = p("Alex");
        sp.gender = Some("female".into());
        // breast_size / ears / tail / clothing left None
        let json = serde_json::to_string(&sp).unwrap();
        assert!(!json.contains("breast_size"));
        assert!(!json.contains("\"ears\""));
        assert!(!json.contains("\"tail\""));
        assert!(!json.contains("clothing"));
        assert!(json.contains("\"gender\":\"female\""));
    }

    #[test]
    fn conditional_traits_present_when_set() {
        let mut sp = p("Alex");
        sp.tail = Some("fox".into());
        sp.clothing = Some(vec!["robe".into()]);
        let json = serde_json::to_string(&sp).unwrap();
        assert!(json.contains("\"tail\":\"fox\""));
        assert!(json.contains("\"clothing\":[\"robe\"]"));
    }

    #[test]
    fn trait_round_trip_preserves_all_fields() {
        let mut sp = p("Mira");
        sp.gender = Some("female".into());
        sp.race = Some("dwarf".into());
        sp.age = Some("48".into());
        sp.hair_color = Some("auburn".into());
        sp.hair_length = Some("shoulder-length".into());
        sp.hair_style = Some("braided".into());
        sp.eye_color = Some("green".into());
        sp.ears = Some("slightly pointed".into());
        sp.clothing = Some(vec!["apron".into(), "ring mail".into()]);
        let json = serde_json::to_string(&sp).unwrap();
        let back: SavedPlayer = serde_json::from_str(&json).unwrap();
        assert_eq!(back.gender.as_deref(), Some("female"));
        assert_eq!(back.race.as_deref(), Some("dwarf"));
        assert_eq!(back.hair_color.as_deref(), Some("auburn"));
        assert_eq!(back.hair_style.as_deref(), Some("braided"));
        assert_eq!(back.ears.as_deref(), Some("slightly pointed"));
        assert_eq!(back.clothing.as_deref(), Some(&["apron".to_string(), "ring mail".to_string()][..]));
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
