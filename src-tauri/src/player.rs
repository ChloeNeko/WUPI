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
// PERSISTENCE: one folder per player under `apps/fable/players/<Name>/`
// (display-named): `<Name>.player` (this struct, XML) + an optional
//   <Name>.png / <Name>.jpg  — uploaded portrait (namesake sibling,
//   discovered by `find_portrait_sibling` in lib.rs)
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
/// `portrait` carries the ABSOLUTE portrait path on the wire (resolved at
/// load by `load_player_portrait` — the file is the namesake
/// `<Name>.<ext>` sibling; presence is a stat fact, no XML field).
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

    // --- v2 format fields (2026-08-19 Chloe ruling) ----------------------
    // The `.player` disk format is now XML (`<player>` root + an optional
    // `<inventory>` sibling). Everything inside `<player>` is the KV-cache
    // payload (always read by the narrator); the sibling is mutable state
    // seed. The DTO below stays the IPC shape — the XML is a
    // serialization-boundary adapter, exactly like the card side.

    /// The OPT-IN persona block (players only; NPC personas are mandatory,
    /// player personas exist solely when the user answers the wizard's final
    /// question with content — Chloe 2026-08-19). Omitted entirely (file +
    /// cache) when absent. `Conversation Style` is parsed if hand-authored
    /// but NEVER offered by the player wizard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<PlayerPersona>,

    /// The `<inventory>` sibling: clothing (garments), equipped (readied),
    /// accessories, stored. Optional in full — a player with no items
    /// carries no inventory block at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inventory: Option<PlayerInventory>,

    /// Absolute portrait path, resolved at load (the file is the namesake
    /// `<Name>.<ext>` sibling in the player folder — presence is a stat
    /// fact). None when no portrait was uploaded.
    #[serde(default)]
    pub portrait: Option<String>,

    /// Creation timestamp (epoch-ms). Decorative; set on write.
    #[serde(default)]
    pub created_at_ms: i64,
}

/// The player's opt-in persona block — mirrors the NPC card's `<persona>`
/// labels minus the mandatory-for-NPCs framing. Every field optional; the
/// whole struct absent when the player declined (rendered NOWHERE — not in
/// the file, not in the cache block).
#[derive(Clone, Default, serde::Serialize, serde::Deserialize, Debug)]
pub struct PlayerPersona {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    /// Parsed when hand-authored; never offered by the wizard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub likes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dislikes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flaws: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupation: Option<String>,
}

impl PlayerPersona {
    pub fn is_empty(&self) -> bool {
        self.personality.is_none()
            && self.conversation_style.is_none()
            && self.likes.is_none()
            && self.dislikes.is_none()
            && self.flaws.is_none()
            && self.goals.is_none()
            && self.occupation.is_none()
    }
}

/// The player's `<inventory>` sibling — comma-separated item lines. The
/// attach seam routes these into the typed equipment/pack model (clothing
/// via `seed_clothing_items`, weapons-ish equipped items to hand slots,
/// accessories/stored to the pack).
#[derive(Clone, Default, serde::Serialize, serde::Deserialize, Debug)]
pub struct PlayerInventory {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clothing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub equipped: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accessories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stored: Vec<String>,
}

impl PlayerInventory {
    pub fn is_empty(&self) -> bool {
        self.clothing.is_empty()
            && self.equipped.is_empty()
            && self.accessories.is_empty()
            && self.stored.is_empty()
    }
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
// not prose. 2026-08-17: TRAIT_MAX 128 → 256 — rich hair/appearance
// detail kept overflowing; the cap stays Rust-side ONLY (stating a
// number in GLM's prompt makes it target that number).
const NAME_MAX: usize = 64;
const PROSE_MAX: usize = 4000;
const TRAIT_MAX: usize = 256;
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
    // The name rides a labeled LINE in the .player XML — a newline would
    // forge a second block line and mutate across round-trips.
    if p.name.contains('\n') || p.name.contains('\r') {
        return Err("Name must be a single line.".into());
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
    // non-empty, bounded label. The v2 <inventory> sibling rides the same
    // per-entry rule (the DTO path previously bypassed every cap —
    // 2026-08-20 audit).
    validate_chip_list("Clothing", p.clothing.as_deref().unwrap_or(&[]))?;
    validate_chip_list("Gear", p.gear.as_deref().unwrap_or(&[]))?;
    validate_chip_list("Tools", p.tools.as_deref().unwrap_or(&[]))?;
    validate_chip_list("Weapons", p.weapons.as_deref().unwrap_or(&[]))?;
    if let Some(inv) = &p.inventory {
        validate_chip_list("Clothing", &inv.clothing)?;
        validate_chip_list("Equipped", &inv.equipped)?;
        validate_chip_list("Accessories", &inv.accessories)?;
        validate_chip_list("Stored", &inv.stored)?;
    }
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

/// Validate a chip-list field (`clothing` / `gear` / `tools` / `weapons` /
/// the `<inventory>` sibling's lists): each entry non-empty after trim,
/// ≤ `TRAIT_MAX`, no control chars. Shared so all the lists share one rule.
fn validate_chip_list(label: &str, items: &[String]) -> Result<(), String> {
    for c in items {
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

// =============================================================
// THE .player XML FORMAT (2026-08-19 Chloe ruling)
//
// `<player>` root holding metadata/identity/persona?/custom_tags, plus an
// optional `<inventory>` SIBLING after `</player>` (mutable state seed —
// never part of the cached root). Everything inside `<player>` is the
// KV-cache payload the narrator reads every turn. The line-block grammar
// (Label: value per line) + label normalization are shared with sim_card.
//
//   <player>
//     <metadata>
//     <id>alex</id>
//     </metadata>
//
//     <identity><![CDATA[
//   Name: Alex
//   Gender: Male
//   ...
//     ]]></identity>
//
//     <persona><![CDATA[...optional, omitted entirely when empty...]]></persona>
//
//     <custom_tags>
//       <entry key="penis_size"><![CDATA[5 inches]]></entry>
//     </custom_tags>
//   </player>
//
//   <inventory><![CDATA[
//   Clothing: White Cropped Tee, Gray Denim Shorts
//   ]]></inventory>
// =============================================================

/// Parse a `.player` XML document into a `SavedPlayer`. The struct-level
/// validator (`validate_player`) still gates writes; this fn is purely
/// structural. Unknown identity/persona labels beyond the known set are
/// dropped (the wizard authors fixed labels); conditional traits
/// (Breast/Ears/Tail/Horn) parse back into their dedicated fields.
pub fn parse_player_xml(xml: &str) -> anyhow::Result<SavedPlayer> {
    use crate::sim_card::{find_tag_close, labeled_get, parse_labeled_lines, split_csv};

    let (head, tail) = match find_tag_close(xml, "player") {
        Some(end) => xml.split_at(end),
        None => (xml, ""),
    };
    let doc = roxmltree::Document::parse(head)
        .map_err(|e| anyhow::anyhow!("parsing player XML: {e}"))?;
    let root = doc
        .root_element()
        .has_tag_name("player")
        .then_some(doc.root_element())
        .ok_or_else(|| anyhow::anyhow!("root element must be <player>"))?;

    let mut p = SavedPlayer {
        id: String::new(),
        name: String::new(),
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
        persona: None,
        inventory: None,
        portrait: None,
        created_at_ms: 0,
    };

    // metadata/id — slugified through the same discipline as the write path.
    let id = root
        .children()
        .find(|c| c.is_element() && c.has_tag_name("metadata"))
        .and_then(|m| {
            m.children()
                .find(|c| c.is_element() && c.has_tag_name("id"))
                .map(|n| crate::sim_card::node_text(n))
        })
        .and_then(|s| slugify_player_id(&s))
        .unwrap_or_default();
    p.id = id;
    // Creation timestamp (preserved across rewrites; 0 when absent).
    if let Some(ms) = root
        .children()
        .find(|c| c.is_element() && c.has_tag_name("metadata"))
        .and_then(|m| {
            m.children()
                .find(|c| c.is_element() && c.has_tag_name("created_at_ms"))
        })
        .and_then(|n| crate::sim_card::node_text(n).trim().parse::<i64>().ok())
        .filter(|ms| *ms > 0)
    {
        p.created_at_ms = ms;
    }

    // identity line block.
    let identity_text = root
        .children()
        .find(|c| c.is_element() && c.has_tag_name("identity"))
        .map(crate::sim_card::node_text)
        .unwrap_or_default();
    let lines = parse_labeled_lines(&identity_text);
    p.name = labeled_get(&lines, &["name"]).unwrap_or_default();
    let get = |k: &str| labeled_get(&lines, &[k]).filter(|v| !v.is_empty());
    p.gender = get("gender");
    p.race = get("race");
    p.age = get("age");
    p.height = get("height");
    p.weight = get("weight");
    p.body_type = get("body");
    p.skin_complexion = get("skin");
    p.eye_color = get("eyes");
    p.hair_color = get("hair_color");
    p.hair_length = get("hair_length");
    p.hair_style = get("hair_style");
    p.breast_size = get("breast");
    p.ears = get("ears");
    p.tail = get("tail");
    p.horn = get("horn");
    // Trait-capped flavor fields (2026-08-20 audit H2 round-trip):
    // normalize_label maps "Job" → "occupation" (the shared drift table),
    // "Weakness" → "weakness", "Distinguishing Marks" → "distinguishing_marks".
    p.job = get("occupation");
    p.weakness = get("weakness");
    p.distinguishing_marks = get("distinguishing_marks");

    // persona line block — optional; Backstory rides as a persona label but
    // lands on the top-level field (the DTO's existing home).
    let persona_text = root
        .children()
        .find(|c| c.is_element() && c.has_tag_name("persona"))
        .map(crate::sim_card::node_text)
        .unwrap_or_default();
    if !persona_text.trim().is_empty() {
        let plines = parse_labeled_lines(&persona_text);
        let pget = |k: &str| labeled_get(&plines, &[k]).filter(|v| !v.is_empty());
        let persona = PlayerPersona {
            personality: pget("personality"),
            conversation_style: pget("conversation_style"),
            likes: pget("likes"),
            dislikes: pget("dislikes"),
            flaws: pget("flaws"),
            goals: pget("goals"),
            occupation: pget("occupation"),
        };
        p.backstory = pget("backstory");
        if !persona.is_empty() || p.backstory.is_some() {
            p.persona = Some(persona);
        }
    }

    // custom_tags.
    if let Some(ct) = root
        .children()
        .find(|c| c.is_element() && c.has_tag_name("custom_tags"))
    {
        let mut tags = std::collections::BTreeMap::new();
        for entry in ct.children().filter(|c| c.is_element() && c.has_tag_name("entry")) {
            let key = entry.attribute("key").unwrap_or("").trim().to_owned();
            if key.is_empty() {
                continue;
            }
            let value = crate::sim_card::node_text(entry).trim().to_owned();
            if value.is_empty() {
                continue;
            }
            tags.insert(key, value);
        }
        if !tags.is_empty() {
            p.custom_tags = Some(tags);
        }
    }

    // Legacy free-prose identity blocks (multi-paragraph — kept raw; the
    // line-block grammar cannot carry them).
    for (tag, field) in [("description", &mut p.description), ("appearance", &mut p.appearance)] {
        if let Some(t) = root
            .children()
            .find(|c| c.is_element() && c.has_tag_name(tag))
            .map(crate::sim_card::node_text)
        {
            let t = t.trim().to_owned();
            if !t.is_empty() {
                *field = Some(t);
            }
        }
    }

    // <inventory> sibling (outside </player>).
    let inv_text = crate::sim_card::sibling_text(tail, &["inventory"]).unwrap_or_default();
    if !inv_text.trim().is_empty() {
        let ilines = parse_labeled_lines(&inv_text);
        let iget = |k: &str| {
            labeled_get(&ilines, &[k])
                .map(|v| split_csv(&v))
                .unwrap_or_default()
        };
        let inv = PlayerInventory {
            clothing: iget("clothing"),
            equipped: iget("equipped"),
            accessories: iget("accessories"),
            stored: iget("stored"),
        };
        if !inv.is_empty() {
            p.inventory = Some(inv);
        }
    }

    Ok(p)
}

/// Render a `SavedPlayer` to the canonical `.player` XML layout
/// (byte-matches the Chloe-authored `alex.player` reference). Empty blocks
/// are omitted entirely: no persona lines → no `<persona>` element; no
/// items → no `<inventory>` sibling.
pub fn render_player_xml(p: &SavedPlayer) -> String {
    let mut xml = String::with_capacity(1024);
    xml.push_str("<player>\n");
    xml.push_str("  <metadata>\n");
    let id = if p.id.trim().is_empty() {
        slugify_player_id(&p.name).unwrap_or_else(|| "player".into())
    } else {
        p.id.trim().to_owned()
    };
    xml.push_str(&format!("  <id>{}</id>\n", xml_escape(&id)));
    // Creation timestamp — round-tripped so `fable_player_write`'s
    // preserve-existing logic actually sees a prior value (previously the
    // stamp was minted fresh on EVERY write: the file never carried it).
    if p.created_at_ms > 0 {
        xml.push_str(&format!("  <created_at_ms>{}</created_at_ms>\n", p.created_at_ms));
    }
    xml.push_str("  </metadata>\n");

    // <identity> — Name leads; trait lines (incl. conditionals) follow.
    // The name is flattened to single-line: a newline inside it would
    // forge a second line in the labeled block (the line-block grammar
    // cannot carry it; the validator rejects new captures, the flatten
    // covers legacy JSON + the boot migration).
    let mut body = String::new();
    let name = p.name.split_whitespace().collect::<Vec<_>>().join(" ");
    if !name.is_empty() {
        body.push_str(&format!("Name: {name}\n"));
    }
    for (label, value) in player_identity_lines(p) {
        body.push_str(&format!("{label}: {value}\n"));
    }
    xml.push_str(&format!("  <identity><![CDATA[\n{}  ]]></identity>\n", cdata_body(&body)));

    // <persona> — the opt-in block. Legacy top-level backstory rides as the
    // Backstory line; the block appears only when SOMETHING is present.
    let persona = p.persona.as_ref();
    let mut persona_body = String::new();
    let mut push = |label: &str, v: Option<&str>| {
        if let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) {
            persona_body.push_str(&format!("{label}: {s}\n"));
        }
    };
    push("Personality", persona.and_then(|x| x.personality.as_deref()));
    push("Conversation Style", persona.and_then(|x| x.conversation_style.as_deref()));
    push("Likes", persona.and_then(|x| x.likes.as_deref()));
    push("Dislikes", persona.and_then(|x| x.dislikes.as_deref()));
    push("Flaws", persona.and_then(|x| x.flaws.as_deref()));
    push("Goals", persona.and_then(|x| x.goals.as_deref()));
    push("Occupation", persona.and_then(|x| x.occupation.as_deref()));
    push("Backstory", p.backstory.as_deref());
    if !persona_body.is_empty() {
        xml.push_str(&format!(
            "  <persona><![CDATA[\n{}  ]]></persona>\n",
            cdata_body(&persona_body)
        ));
    }

    // Legacy free-prose identity fields (pre-v2 JSON): dedicated CDATA
    // blocks — they can be multi-paragraph, which the labeled-line grammar
    // cannot carry. Omitted entirely when empty (2026-08-20 audit H2:
    // previously validated + parsed but never emitted, so the first XML
    // write destroyed them).
    for (tag, v) in [("description", &p.description), ("appearance", &p.appearance)] {
        if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            xml.push_str(&format!("  <{tag}><![CDATA[\n{}\n  ]]></{tag}>\n", cdata_body(s)));
        }
    }

    if let Some(tags) = &p.custom_tags {
        if !tags.is_empty() {
            xml.push_str("  <custom_tags>\n");
            for (k, v) in tags {
                xml.push_str(&format!(
                    "    <entry key=\"{}\"><![CDATA[{}]]></entry>\n",
                    xml_escape(k),
                    cdata_body(v.trim())
                ));
            }
            xml.push_str("  </custom_tags>\n");
        }
    }
    xml.push_str("</player>\n");

    if let Some(inv) = p.inventory.as_ref().filter(|i| !i.is_empty()) {
        let mut body = String::new();
        if !inv.clothing.is_empty() {
            body.push_str(&format!("Clothing: {}\n", inv.clothing.join(", ")));
        }
        if !inv.equipped.is_empty() {
            body.push_str(&format!("Equipped: {}\n", inv.equipped.join(", ")));
        }
        if !inv.accessories.is_empty() {
            body.push_str(&format!("Accessories: {}\n", inv.accessories.join(", ")));
        }
        if !inv.stored.is_empty() {
            body.push_str(&format!("Stored: {}\n", inv.stored.join(", ")));
        }
        xml.push_str(&format!("\n<inventory><![CDATA[\n{}]]></inventory>\n", cdata_body(&body)));
    }

    xml
}

/// Render the `<player>` cache block — the payload injected verbatim into
/// the API narrator's system prompt every turn. Identity lines, persona
/// lines, custom tags. NEVER the inventory sibling (mutable state).
pub fn render_player_cache_block(p: &SavedPlayer) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("<player>\n");
    let name = p.name.trim();
    if !name.is_empty() {
        out.push_str(&format!("Name: {name}\n"));
    }
    for (label, value) in player_identity_lines(p) {
        out.push_str(&format!("{label}: {value}\n"));
    }
    let persona = p.persona.as_ref();
    let mut persona_rows: Vec<(&'static str, String)> = Vec::new();
    let mut push = |label: &'static str, v: Option<&str>| {
        if let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) {
            persona_rows.push((label, s.to_owned()));
        }
    };
    push("Personality", persona.and_then(|x| x.personality.as_deref()));
    push("Conversation Style", persona.and_then(|x| x.conversation_style.as_deref()));
    push("Likes", persona.and_then(|x| x.likes.as_deref()));
    push("Dislikes", persona.and_then(|x| x.dislikes.as_deref()));
    push("Flaws", persona.and_then(|x| x.flaws.as_deref()));
    push("Goals", persona.and_then(|x| x.goals.as_deref()));
    push("Occupation", persona.and_then(|x| x.occupation.as_deref()));
    push("Backstory", p.backstory.as_deref());
    if !persona_rows.is_empty() {
        out.push('\n');
        for (label, value) in persona_rows {
            out.push_str(&format!("{label}: {value}\n"));
        }
    }
    // Legacy free-prose identity paragraphs (same payload rule as the file:
    // present → narrator reads them, absent → nothing rendered).
    for (label, v) in [("Description", &p.description), ("Appearance", &p.appearance)] {
        if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str(&format!("\n{label}:\n{s}\n"));
        }
    }
    if let Some(tags) = &p.custom_tags {
        if !tags.is_empty() {
            out.push('\n');
            for (k, v) in tags {
                out.push_str(&format!("{k}: {}\n", v.trim()));
            }
        }
    }
    out.push_str("</player>");
    out
}

/// The ordered identity-line view of a player (label order matches the sim
/// identity block; the conditional traits + the trait-capped flavor fields
/// ride as trailing lines). Job/Weakness/Distinguishing Marks are identity
/// payload — they ride BOTH the file and the narrator cache block
/// (2026-08-20 audit H2: the fields were validated + parsed but never
/// emitted, so the first XML write silently destroyed them).
fn player_identity_lines(p: &SavedPlayer) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut push = |label: &'static str, v: &Option<String>| {
        if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push((label, s.to_owned()));
        }
    };
    push("Gender", &p.gender);
    push("Race", &p.race);
    push("Age", &p.age);
    push("Height", &p.height);
    push("Weight", &p.weight);
    push("Body", &p.body_type);
    push("Skin", &p.skin_complexion);
    push("Eyes", &p.eye_color);
    push("Hair Color", &p.hair_color);
    push("Hair Length", &p.hair_length);
    push("Hair Style", &p.hair_style);
    push("Breast", &p.breast_size);
    push("Ears", &p.ears);
    push("Tail", &p.tail);
    push("Horn", &p.horn);
    push("Job", &p.job);
    push("Weakness", &p.weakness);
    push("Distinguishing Marks", &p.distinguishing_marks);
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn cdata_body(s: &str) -> String {
    s.replace("]]>", "]]]]><![CDATA[>")
}

/// Fold the LEGACY JSON chip lists (clothing/gear/tools/weapons) into the v2
/// `inventory` model when no explicit inventory exists — the boot migration
/// + read paths call this so one model serves the attach seam. Returns a
/// clone; the original is untouched.
pub fn fold_legacy_lists(p: &SavedPlayer) -> SavedPlayer {
    let mut out = p.clone();
    if out.inventory.is_none() {
        let mut inv = PlayerInventory::default();
        if let Some(c) = &out.clothing {
            inv.clothing = c.iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
        }
        // Legacy free-text accessories (the pre-v2 string field) split onto
        // the v2 accessories rack (2026-08-20 audit H2: previously folded
        // nowhere — the first XML write destroyed them).
        if let Some(a) = &out.accessories {
            inv.accessories = crate::sim_card::split_csv(a);
        }
        let mut stored: Vec<String> = Vec::new();
        for list in [&out.gear, &out.tools, &out.weapons] {
            if let Some(items) = list {
                for s in items {
                    let s = s.trim();
                    if !s.is_empty() {
                        stored.push(s.to_owned());
                    }
                }
            }
        }
        inv.stored = stored;
        if !inv.is_empty() {
            out.inventory = Some(inv);
        }
    }
    // Legacy top-level personality (pre-overhaul JSON) folds into the
    // opt-in persona block so it survives the XML rewrite.
    if out.persona.is_none() {
        if let Some(per) = out.personality.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.persona = Some(PlayerPersona {
                personality: Some(per.to_owned()),
                ..Default::default()
            });
        }
    }
    out
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
            persona: None,
            inventory: None,
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

    // ── .player XML format tests ──────────────────────────────────────────

    /// The Chloe-authored reference layout (alex.player).
    const ALEX_XML: &str = r#"<player>
  <metadata>
  <id>alex</id>
  </metadata>

  <identity><![CDATA[
Name: Alex
Gender: Male
Race: Human
Age: 18
Height: 5'6"
Weight: 130 lbs
Body: Petite
Skin: Pale with freckles around nose
Eyes: Bright blue
Hair Color: Blonde
Hair Length: Shoulder-Length
Hair Style: Messy
  ]]></identity>

  <custom_tags>
    <entry key="penis_size"><![CDATA[5 inches]]></entry>
  </custom_tags>
</player>

<inventory><![CDATA[
Clothing: White Cropped Tee, Gray Denim Shorts, White Knee-High Socks, White Sneakers
]]></inventory>"#;

    #[test]
    fn player_xml_parses_reference_layout() {
        let sp = parse_player_xml(ALEX_XML).expect("alex.player parses");
        assert_eq!(sp.id, "alex");
        assert_eq!(sp.name, "Alex");
        assert_eq!(sp.gender.as_deref(), Some("Male"));
        assert_eq!(sp.race.as_deref(), Some("Human"));
        assert_eq!(sp.height.as_deref(), Some("5'6\""));
        assert_eq!(sp.body_type.as_deref(), Some("Petite"));
        assert_eq!(sp.skin_complexion.as_deref(), Some("Pale with freckles around nose"));
        assert_eq!(sp.hair_color.as_deref(), Some("Blonde"));
        assert_eq!(sp.hair_style.as_deref(), Some("Messy"));
        assert!(sp.persona.is_none(), "alex carries no persona");
        assert_eq!(
            sp.custom_tags.as_ref().unwrap().get("penis_size").map(|s| s.as_str()),
            Some("5 inches")
        );
        let inv = sp.inventory.as_ref().expect("inventory sibling parses");
        assert_eq!(inv.clothing.len(), 4);
        assert_eq!(inv.clothing[0], "White Cropped Tee");
        assert!(validate_player(&sp).is_ok());
    }

    #[test]
    fn player_xml_round_trips() {
        let sp = parse_player_xml(ALEX_XML).expect("parses");
        let xml = render_player_xml(&sp);
        let back = parse_player_xml(&xml).expect("round-trip parses");
        assert_eq!(back.id, sp.id);
        assert_eq!(back.name, sp.name);
        assert_eq!(back.gender, sp.gender);
        assert_eq!(back.body_type, sp.body_type);
        assert_eq!(back.hair_color, sp.hair_color);
        assert_eq!(back.custom_tags, sp.custom_tags);
        assert_eq!(
            back.inventory.as_ref().unwrap().clothing,
            sp.inventory.as_ref().unwrap().clothing
        );
    }

    #[test]
    fn player_xml_persona_round_trips_and_omits_when_empty() {
        let mut sp = p("Mira");
        sp.persona = Some(PlayerPersona {
            personality: Some("Quiet and watchful.".into()),
            likes: Some("Maps, rain.".into()),
            ..Default::default()
        });
        sp.backstory = Some("Raised by cartographers.".into());
        let xml = render_player_xml(&sp);
        assert!(xml.contains("Personality: Quiet and watchful."));
        assert!(xml.contains("Likes: Maps, rain."));
        assert!(xml.contains("Backstory: Raised by cartographers."));
        let back = parse_player_xml(&xml).expect("parses");
        assert_eq!(
            back.persona.as_ref().unwrap().personality.as_deref(),
            Some("Quiet and watchful.")
        );
        assert_eq!(back.persona.as_ref().unwrap().likes.as_deref(), Some("Maps, rain."));
        assert_eq!(back.backstory.as_deref(), Some("Raised by cartographers."));

        // No persona → the element is omitted ENTIRELY (file + cache).
        let bare = render_player_xml(&p("Naked"));
        assert!(!bare.contains("<persona>"));
        let cache = render_player_cache_block(&p("Naked"));
        assert!(!cache.contains("Personality"));
    }

    #[test]
    fn player_cache_block_excludes_inventory() {
        let sp = parse_player_xml(ALEX_XML).expect("parses");
        let block = render_player_cache_block(&sp);
        assert!(block.starts_with("<player>\nName: Alex\n"));
        assert!(block.contains("Hair Color: Blonde\n"));
        assert!(block.contains("penis_size: 5 inches\n"));
        assert!(!block.contains("White Cropped Tee"), "inventory never rides the cache");
        assert!(block.ends_with("</player>"));
    }

    #[test]
    fn player_xml_conditional_traits_round_trip() {
        let mut sp = p("Kitsune");
        sp.ears = Some("fox".into());
        sp.tail = Some("nine tails".into());
        sp.horn = Some("small".into());
        sp.breast_size = Some("modest".into());
        let back = parse_player_xml(&render_player_xml(&sp)).expect("parses");
        assert_eq!(back.ears.as_deref(), Some("fox"));
        assert_eq!(back.tail.as_deref(), Some("nine tails"));
        assert_eq!(back.horn.as_deref(), Some("small"));
        assert_eq!(back.breast_size.as_deref(), Some("modest"));
    }

    #[test]
    fn fold_legacy_lists_into_inventory() {
        let mut sp = p("Legacy");
        sp.clothing = Some(vec!["travel cloak".into(), "boots".into()]);
        sp.gear = Some(vec!["rope".into()]);
        sp.weapons = Some(vec!["shortsword".into()]);
        sp.personality = Some("Old-school prose.".into());
        let folded = fold_legacy_lists(&sp);
        let inv = folded.inventory.expect("legacy lists folded");
        assert_eq!(inv.clothing, vec!["travel cloak".to_string(), "boots".to_string()]);
        assert!(inv.stored.contains(&"rope".to_string()));
        assert!(inv.stored.contains(&"shortsword".to_string()));
        assert_eq!(
            folded.persona.as_ref().unwrap().personality.as_deref(),
            Some("Old-school prose.")
        );
        // XML round-trip preserves the folded shape.
        let back = parse_player_xml(&render_player_xml(&folded)).expect("parses");
        assert_eq!(back.inventory.unwrap().clothing.len(), 2);
    }

    #[test]
    fn player_xml_rejects_wrong_root() {
        assert!(parse_player_xml("<sim_card><id>x</id></sim_card>").is_err());
    }

    #[test]
    fn player_xml_roundtrips_flavor_prose_stamp_and_accessories() {
        // 2026-08-20 audit H2: job/weakness/marks/description/appearance were
        // validated + parsed but never emitted — the first XML write silently
        // destroyed them. They must now ride the file AND survive a round-trip
        // (legacy accessories fold onto the v2 rack; the creation stamp
        // persists instead of re-minting every write).
        let mut sp = p("Alex");
        sp.job = Some("Blacksmith".into());
        sp.weakness = Some("prone to rage".into());
        sp.distinguishing_marks = Some("scar over left eye".into());
        sp.description = Some("Tall.\nSecond paragraph.".into());
        sp.appearance = Some("Weathered hands.".into());
        sp.accessories = Some("Copper Ring, Silver Chain".into());
        sp.created_at_ms = 1724000000000;
        let folded = fold_legacy_lists(&sp);
        let back = parse_player_xml(&render_player_xml(&folded)).expect("round-trips");
        assert_eq!(back.job.as_deref(), Some("Blacksmith"));
        assert_eq!(back.weakness.as_deref(), Some("prone to rage"));
        assert_eq!(back.distinguishing_marks.as_deref(), Some("scar over left eye"));
        assert_eq!(back.description.as_deref(), Some("Tall.\nSecond paragraph."));
        assert_eq!(back.appearance.as_deref(), Some("Weathered hands."));
        assert_eq!(
            back.inventory.as_ref().unwrap().accessories,
            vec!["Copper Ring".to_string(), "Silver Chain".to_string()]
        );
        assert_eq!(back.created_at_ms, 1724000000000);
        // The trait-capped flavor lines ride the narrator cache block too.
        let block = render_player_cache_block(&folded);
        assert!(block.contains("Job: Blacksmith\n"));
        assert!(block.contains("Distinguishing Marks: scar over left eye\n"));
        assert!(block.contains("Description:\nTall.\nSecond paragraph.\n"));
    }

    #[test]
    fn player_xml_flattens_newline_names() {
        // A newline inside a name would forge a second labeled line; the
        // render flattens it so the round-trip is stable (legacy JSON names
        // bypass the validator's single-line gate).
        let sp = p("Alex\nthe  Bold");
        let back = parse_player_xml(&render_player_xml(&sp)).expect("round-trips");
        assert_eq!(back.name, "Alex the Bold");
    }

    #[test]
    fn validate_rejects_multiline_name() {
        let bad = p("Alex\nthe Bold");
        assert!(validate_player(&bad).is_err());
    }

    #[test]
    fn validate_caps_inventory_entries() {
        let mut bad = p("Alex");
        bad.inventory = Some(PlayerInventory {
            stored: vec!["x".repeat(TRAIT_MAX + 1)],
            ..Default::default()
        });
        assert!(validate_player(&bad).is_err());
        // Empty-after-trim entries reject like chip lists.
        bad.inventory = Some(PlayerInventory {
            clothing: vec!["   ".into()],
            ..Default::default()
        });
        assert!(validate_player(&bad).is_err());
    }
}
