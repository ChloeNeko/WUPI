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

    /// The character's history / backstory prose (NEW 2026-08-11). Optional —
    /// the Player Creator's Backstory slide can be skipped; a skipped backstory
    /// stays `None` + is OMITTED from the JSON entirely
    /// (`skip_serializing_if`, mirroring the conditional traits ears/tail/
    /// breast). At game attach this is seeded as a `player.backstory` schema
    /// entity so the narrator + WUPI know the character's history from turn 1.
    /// Capped at `PROSE_MAX` (4000) by `validate_player`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backstory: Option<String>,

    // --- Structured identity/appearance traits (2026-08-04 overhaul).
    // The Player Creator's slide-by-slide wizard writes these directly
    // (typed fields, not composed prose) so the tracker pipeline can
    // target individual traits by stable key via `[APPEARANCE key=value]`
    // and so the SIM-card review screen can render a clean breakdown.
    // All optional + defaulted for back-compat with pre-overhaul JSON.

    /// Free-form gender / identity text (e.g. "female", "masculine",
    /// "nonbinary"). 2026-08-13: NO LONGER restricted to "male"/"female" —
    /// the validator accepts any non-empty value ≤ `TRAIT_MAX`. This is
    /// identity text only; the Left Drawer paperdoll is a SEPARATE manual
    /// ♂/♀ HUD toggle (`localStorage['wupi.paperdoll.gender'`) that does NOT
    /// read this field, and the narrator's build/frame signal is `body_type`
    /// (seeded into the appearance layer at attach). Seeded as a
    /// `player.gender` entity so the narrator can reference the character's
    /// identity from turn 1.
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
    /// Horns (the non-human trait; "curled ram", "spiral"). Conditional —
    /// the Wizard asks only when the race is non-human (context clues).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horn: Option<String>,

    /// Clothing / outfit items as a chip list from the dynamic-list
    /// slide. Each entry is one garment ("travel cloak", "leather
    /// boots"). None when the slide was left empty (omitted from JSON).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clothing: Option<Vec<String>>,

    // --- Optional descriptive identity fields (2026-08-13). The Player
    // Wizard's GLM interview MAY surface these; all optional + omitted from
    // JSON when None. At game attach each seeds a `player.*` schema entity
    // (mirrors `backstory`) so the narrator reads them as identity ground
    // truth — NOT gameplay state (no body/wealth mutation; the identity-only
    // invariant, §6C, holds).
    /// Occupation / trade ("blacksmith", "caravan guard"). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// Character flaw / vice ("prone to rage", "greedy"). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weakness: Option<String>,
    /// Visible identifying marks — scars / tattoos / birthmarks. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinguishing_marks: Option<String>,
    /// Carried gear chip list (like `clothing`). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gear: Option<Vec<String>>,
    /// Carried tools chip list. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Carried weapons chip list. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weapons: Option<Vec<String>>,

    // --- Custom extensions (2026-08-13). Any extra stat / asset /
    // reputation / currency / tracker the player requests that does NOT fit
    // a standard schema key. A flat key→value string map (BTreeMap for
    // deterministic save ordering — functionally the "flat key-value object
    // of string pairs"). At game attach each entry seeds a `player.<key>`
    // schema entity. This is DISTINCT from the named `wealth`/`reputation`
    // optional fields — those are transient (seed `PlayerState` at attach,
    // never persisted here; §6C identity-only lock preserved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_tags: Option<std::collections::BTreeMap<String, String>>,

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
/// Per-key cap for `custom_tags` keys (short identifiers like
/// "starting_currency", "guard_reputation"). Tighter than a trait — these
/// are field names, not values.
const CUSTOM_TAG_KEY_MAX: usize = 64;
/// Per-value cap for `custom_tags` values ("200 gold", "-20"). Roomier than
/// a trait label but tighter than prose — these are short stats/amounts.
const CUSTOM_TAG_VALUE_MAX: usize = 500;

/// Validate a SavedPlayer's structure + content. The authoritative gate
/// (runs server-side in `fable_player_write`). Returns a human-readable
/// error string on failure (surfaced to the Creator toast), or Ok on
/// success.
///
/// Checks:
///   • name required + non-empty after trim + ≤ NAME_MAX chars
///   • each prose field (if present) ≤ PROSE_MAX chars
///   • each trait field (if present) ≤ TRAIT_MAX chars + non-empty
///     after trim (empty traits should be None, not ""). Gender is a
///     free-form trait (2026-08-13: no longer restricted to male/female).
///   • each chip list — clothing / gear / tools / weapons (if present):
///     each entry ≤ TRAIT_MAX + non-empty + no control chars
///   • custom_tags (if present): keys ≤ CUSTOM_TAG_KEY_MAX, values ≤
///     CUSTOM_TAG_VALUE_MAX, no control chars
///   • NO control characters ([\x00-\x08\x0B\x0C\x0E-\x1F]) in any
///     string field — the load-bearing sanitization the frontend XML
///     sniff does not do. Newlines (\x0A) + tabs (\x09) are allowed
///     (legitimate prose formatting); the C1 range (\x80-\x9F) is left
///     to the serializer (rare in practice, not a structuring risk).
///
/// All length caps count CHARS, not bytes (2026-08-15 audit fix): the byte
/// checks capped CJK/accented names at ~⅓ the advertised count while the
/// error strings said "characters". `chars().count()` makes the errors
/// honest and the caps script-neutral.
pub fn validate_player(p: &SavedPlayer) -> Result<(), String> {
    let name = p.name.trim();
    if name.is_empty() {
        return Err("Name is required.".into());
    }
    if name.chars().count() > NAME_MAX {
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
        ("Backstory", &p.backstory),
    ] {
        if let Some(s) = val {
            let s = s.trim();
            if s.chars().count() > PROSE_MAX {
                return Err(format!("{} must be {} characters or fewer.", label, PROSE_MAX));
            }
            if has_control_chars(s) {
                return Err(format!("{} contains invalid control characters.", label));
            }
        }
    }
    // Single-value trait fields (gender included as a free-form trait,
    // 2026-08-13). These come from wizard slides; an empty string is a
    // mistake (the slide should have left the field None).
    for (label, val) in trait_fields(p) {
        if let Some(s) = val {
            let s = s.trim();
            if s.is_empty() {
                return Err(format!("{} cannot be empty.", label));
            }
            if s.chars().count() > TRAIT_MAX {
                return Err(format!("{} must be {} characters or fewer.", label, TRAIT_MAX));
            }
            if has_control_chars(s) {
                return Err(format!("{} contains invalid control characters.", label));
            }
        }
    }
    // Chip lists: clothing / gear / tools / weapons. Each entry must be a
    // non-empty, bounded label.
    validate_chip_list("Clothing", &p.clothing)?;
    validate_chip_list("Gear", &p.gear)?;
    validate_chip_list("Tools", &p.tools)?;
    validate_chip_list("Weapons", &p.weapons)?;
    // Custom extensions: flat key→value string map. Keys are short ids;
    // values are short stats/amounts.
    if let Some(tags) = &p.custom_tags {
        for (k, v) in tags {
            let kt = k.trim();
            if kt.is_empty() {
                return Err("Custom tag keys cannot be empty.".into());
            }
            if kt.chars().count() > CUSTOM_TAG_KEY_MAX {
                return Err(format!("Custom tag keys must be {} characters or fewer.", CUSTOM_TAG_KEY_MAX));
            }
            if has_control_chars(kt) {
                return Err("A custom tag key contains invalid control characters.".into());
            }
            let vt = v.trim();
            if vt.chars().count() > CUSTOM_TAG_VALUE_MAX {
                return Err(format!("Custom tag values must be {} characters or fewer.", CUSTOM_TAG_VALUE_MAX));
            }
            if has_control_chars(vt) {
                return Err("A custom tag value contains invalid control characters.".into());
            }
        }
    }
    Ok(())
}

/// Validate a chip-list field (`clothing` / `gear` / `tools` / `weapons`):
/// each entry non-empty after trim, ≤ `TRAIT_MAX`, no control chars. Shared
/// so the four lists share one rule.
fn validate_chip_list(label: &str, items: &Option<Vec<String>>) -> Result<(), String> {
    if let Some(list) = items {
        for c in list {
            let c = c.trim();
            if c.is_empty() {
                return Err(format!("{} entries cannot be empty.", label));
            }
            if c.chars().count() > TRAIT_MAX {
                return Err(format!("Each {} entry must be {} characters or fewer.", label, TRAIT_MAX));
            }
            if has_control_chars(c) {
                return Err(format!("A {} entry contains invalid control characters.", label));
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
        ("Gender", &p.gender),
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
        ("Horn", &p.horn),
        ("Job", &p.job),
        ("Weakness", &p.weakness),
        ("Distinguishing marks", &p.distinguishing_marks),
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
/// non-alphanumerics → dashes, dash RUNS collapsed, trimmed, 64-char cap).
/// Mirrors `slugify_card_stem` (lib.rs) + the JS `slugify`
/// (card-serialize.js) on BOTH sides: the JS duplicate-name guard compares
/// its slug against backend-listed ids and `fable_player_write` re-derives
/// the id from the name — the old divergence ("Kaelen, the Bold" →
/// `kaelen--the-bold` here, `kaelen-the-bold` in JS) made the guard miss
/// and the write silently atomic-overwrite the first player's identity JSON
/// (the exact H5 card bug, still live for players). A slug landing on a
/// Windows reserved base name gets the "-card" suffix (the same suffix JS
/// appends) so `create_dir_all(players/con)` can't fail opaquely. Returns
/// None when the name reduces to empty (no usable slug) — the validator
/// rejects empty names first, so this is belt-and-suspenders.
pub fn slugify_player_id(name: &str) -> Option<String> {
    let mapped: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    // (2026-08-16 bugs 4+19) Run-collapse BEFORE the trim/cap + the reserved-
    // stem suffix — byte-for-byte the card discipline.
    let stem = crate::cap_slug_chars(
        crate::collapse_dash_runs(&mapped).trim_matches('-').to_owned(),
    );
    let stem = if crate::WINDOWS_RESERVED_STEMS.contains(&stem.as_str()) {
        format!("{stem}-card")
    } else {
        stem
    };
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
            backstory: None,
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
            horn: None,
            clothing: None,
            job: None,
            weakness: None,
            distinguishing_marks: None,
            gear: None,
            tools: None,
            weapons: None,
            custom_tags: None,
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
    fn validate_accepts_freeform_gender() {
        // 2026-08-13: gender is no longer restricted to male/female — any
        // non-empty value ≤ TRAIT_MAX is valid identity text.
        let mut ok = p("Alex");
        ok.gender = Some("helicopter".into());
        assert!(validate_player(&ok).is_ok());
        ok.gender = Some("nonbinary demigirl".into());
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn validate_rejects_oversize_gender() {
        let mut bad = p("Alex");
        bad.gender = Some("x".repeat(TRAIT_MAX + 1));
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
    fn validate_rejects_empty_gear_entry() {
        // The shared chip-list rule applies to gear/tools/weapons too.
        let mut bad = p("Alex");
        bad.gear = Some(vec!["rope".into(), "".into()]);
        assert!(validate_player(&bad).is_err());
        let mut bad2 = p("Alex");
        bad2.weapons = Some(vec!["   ".into()]);
        assert!(validate_player(&bad2).is_err());
    }

    #[test]
    fn validate_accepts_horn_trait() {
        let mut ok = p("Kaelen");
        ok.race = Some("tiefling".into());
        ok.horn = Some("curled, deep red".into());
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn validate_rejects_oversize_custom_tag_value() {
        let mut bad = p("Alex");
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("starting_currency".into(), "x".repeat(CUSTOM_TAG_VALUE_MAX + 1));
        bad.custom_tags = Some(tags);
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_rejects_empty_custom_tag_key() {
        let mut bad = p("Alex");
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("   ".into(), "200 gold".into());
        bad.custom_tags = Some(tags);
        assert!(validate_player(&bad).is_err());
    }

    /// 2026-08-15 audit fix: the caps count CHARS — a 64-char CJK name is
    /// 192 bytes and the old byte gate rejected it at ~⅓ the advertised
    /// count while the error said "characters".
    #[test]
    fn validate_caps_count_chars_not_bytes() {
        // 60 CJK chars = 180 bytes: over any byte-style NAME_MAX read,
        // comfortably under the 64-char cap.
        let cjk_name: String = "刀".repeat(60);
        let ok = p(&cjk_name);
        assert!(validate_player(&ok).is_ok(), "60 chars / 180 bytes must pass the 64-char cap");

        // 65 CJK chars = 195 bytes: over the CHAR cap by 5.
        let long_name: String = "刀".repeat(65);
        let bad = p(&long_name);
        let err = validate_player(&bad).unwrap_err();
        assert!(err.contains("64 characters"), "error names the char cap: {err}");
    }

    #[test]
    fn validate_accepts_custom_tags() {
        let mut ok = p("Alex");
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("starting_currency".into(), "200 gold".into());
        tags.insert("guard_reputation".into(), "-20".into());
        ok.custom_tags = Some(tags);
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn validate_accepts_full_trait_set() {
        let mut ok = p("Kaelen");
        ok.gender = Some("nonbinary".into());
        ok.race = Some("half-elf".into());
        ok.age = Some("32".into());
        ok.hair_color = Some("raven black".into());
        ok.breast_size = None; // conditional toggle was No → omitted
        ok.tail = Some("panther, prehensile".into());
        ok.horn = None; // human-leaning → no horns
        ok.clothing = Some(vec!["travel cloak".into(), "leather boots".into()]);
        ok.job = Some("cartographer".into());
        ok.weakness = Some("terrified of deep water".into());
        ok.gear = Some(vec!["brass compass".into(), "charcoal".into()]);
        let mut tags = std::collections::BTreeMap::new();
        tags.insert("starting_currency".into(), "200 gold".into());
        ok.custom_tags = Some(tags);
        assert!(validate_player(&ok).is_ok());
    }

    #[test]
    fn conditional_traits_omitted_from_json_when_none() {
        // The skip_serializing_if contract: breast_size/ears/tail/horn/clothing
        // + the new optional fields + custom_tags vanish from the serialized
        // JSON when None (the Yes/No toggle was No / the field was skipped).
        let mut sp = p("Alex");
        sp.gender = Some("female".into());
        // breast_size / ears / tail / horn / clothing / job / custom_tags left None
        let json = serde_json::to_string(&sp).unwrap();
        assert!(!json.contains("breast_size"));
        assert!(!json.contains("\"ears\""));
        assert!(!json.contains("\"tail\""));
        assert!(!json.contains("\"horn\""));
        assert!(!json.contains("clothing"));
        assert!(!json.contains("\"job\""));
        assert!(!json.contains("gear"));
        assert!(!json.contains("custom_tags"));
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

    /// (2026-08-16 bugs 4+19) Parity with `slugify_card_stem` + the JS
    /// `slugify`: dash-run collapse (the duplicate-name guard compares
    /// against JS-normalized ids) + the Windows reserved-stem suffix (an
    /// opaque `create_dir_all(players/con)` failure otherwise).
    #[test]
    fn slugify_matches_card_discipline() {
        assert_eq!(slugify_player_id("Kaelen, the Bold"), Some("kaelen-the-bold".into()));
        assert_eq!(slugify_player_id("Star - Falls"), Some("star-falls".into()));
        assert_eq!(slugify_player_id("Con"), Some("con-card".into()));
        assert_eq!(slugify_player_id("Nul"), Some("nul-card".into()));
        assert_eq!(slugify_player_id("COM3"), Some("com3-card".into()));
        // The 64-char cap re-trims a trailing dash exposed by the cut.
        let long = "a".repeat(70) + "-b";
        let slug = slugify_player_id(&long).unwrap();
        assert_eq!(slug.chars().count(), 64);
        assert!(!slug.ends_with('-'));
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
