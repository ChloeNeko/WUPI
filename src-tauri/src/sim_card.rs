//! Simulation Card (`.sim`) loader, parser, and renderer.
//!
//! ## The v2 card format (2026-08-19, Chloe ruling)
//!
//! A `.sim` file is a TWO-ROOT document. Everything inside `<sim_card>` is
//! the KV-cache payload — rendered verbatim into the narrator's system
//! prompt every turn (identity + persona + custom tags NEVER change mid-
//! session, so they belong in the stable cache prefix). Everything AFTER
//! `</sim_card>` is mutable world state seed data (it changes in play, so
//! it must never sit in the cached root):
//!
//! ```xml
//! <sim_card>
//!   <metadata>
//!   <type>simulation</type>
//!   <subtype>npc</subtype>
//!   <id>liam</id>
//!   </metadata>
//!
//!   <identity><![CDATA[
//! Name: Liam
//! Gender: Male
//! Race: Human
//! Age: 21
//! Height: 5'10"
//! Weight: 150 lbs
//! Body: Petite and hairless
//! Skin: Pale
//! Eyes: Pink
//! Hair Color: Pink
//! Hair Length: Shoulder-length
//! Hair Style: Styled neatly
//!   ]]></identity>
//!
//!   <persona><![CDATA[
//! Personality: ...
//! Conversation Style: ...
//! Likes: ...
//! Dislikes: ...
//! Flaws: ...
//! Goals: ...
//! Occupation: ...
//! Backstory: ...
//!   ]]></persona>
//!
//!   <custom_tags>
//!     <entry key="scent"><![CDATA[...]]></entry>
//!   </custom_tags>
//! </sim_card>
//!
//! <world><![CDATA[
//! Date: March 15, present day
//! Time: 10:00 AM
//! Weather: clear and sunny
//! Tone: Smut, Romance
//! ]]></world>
//!
//! <location><![CDATA[
//! Liam's House
//! ]]></location>
//!
//! <intro><![CDATA[...opening beat...]]></intro>
//!
//! <inventory><![CDATA[
//! Clothing: Pink Oversized Hoodie, Gray Jeans
//! Equipped: ...
//! Accessories: ...
//! Stored: ...
//! ]]></inventory>
//! ```
//!
//! Rules (Chloe, 2026-08-19; v0.30.0 clean break 2026-08-22):
//! - `<type>` is `simulation`. `system` cards (`wupi.sim`) keep the LEGACY
//!   shape forever — the chat persona path (`render_for_prompt`) still reads
//!   the legacy fields. Pre-v2 FABLE cards are UNSUPPORTED (v0.30.0): the
//!   card list/resolvers skip `format_v2 == false` cards; users re-create
//!   them through the Creator.
//! - Empty fields are omitted from the file AND the cache block entirely.
//! - `<cast>` is REMOVED — an npc-subtype card IS the character; it self-
//!   registers into `npc_registry` at session start. `<inventory>` siblings
//!   exist only on npc cards (Clothing mandatory, the other lines optional).
//! - scenario/world cards keep their dedicated prose fields (`<setting>` /
//!   `<plot>`) inside `<sim_card>` (Chloe's 2026-08-19 ruling) — identity
//!   carries just `Name:` for them.
//! - `<world>`'s Tone seeds `WorldSchema.tone` — tone is injected per-turn
//!   with the time + weather via `<world_state>`, never as static prompt.
//!
//! The card is strict XML with CDATA-wrapped prose blocks (so emoticons,
//! quotes, and any literal `<>` in the persona text parse cleanly). We parse
//! it once at startup with `roxmltree` (a tiny DOM parser that auto-merges
//! CDATA into text nodes: zero special handling).
//!
//! Design contract (mirrors the embedder's graceful-degradation pattern in
//! §2M): if the card file is missing or malformed, `load_or_fallback` returns
//! a minimal stub persona so the app still boots. The persona is best-effort;
//! a bad card must never kill the OS.

use std::collections::BTreeMap;
use std::path::Path;

use rand::seq::IndexedRandom;

/// The v2 identity block — the physical, who-they-ARE traits, parsed from
/// the `<identity>` CDATA line list ("Label: value" per line). Every field
/// optional; empty lines never exist in the file (omitted-when-empty is the
/// format rule). The NAME lives on `SimCard.name` directly (one source of
/// truth) — this struct carries only the trait lines.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CardIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub race: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eyes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hair_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hair_length: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hair_style: Option<String>,
    /// Authored lines whose label matched no known field — preserved verbatim
    /// (they ride the cache block + survive re-serialization; "everything
    /// within <sim_card> is loaded by the KV cache" is the format contract).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<(String, String)>,
}

/// The v2 persona block — parsed from the `<persona>` CDATA line list.
/// All fields optional: for NPC cards the wizard gathers them all (mandatory
/// there); for PLAYERS the whole `<persona>` is opt-in and omitted entirely
/// when empty. `Conversation Style` is accepted here (NPC cards carry it) but
/// never offered by the player wizard (Chloe 2026-08-19).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CardPersona {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backstory: Option<String>,
    /// Authored lines with unrecognized labels — preserved verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<(String, String)>,
}

impl CardPersona {
    pub fn is_empty(&self) -> bool {
        self.personality.is_none()
            && self.conversation_style.is_none()
            && self.likes.is_none()
            && self.dislikes.is_none()
            && self.flaws.is_none()
            && self.goals.is_none()
            && self.occupation.is_none()
            && self.backstory.is_none()
            && self.extra.is_empty()
    }
}

/// The v2 `<world>` sibling — the mutable cold-start anchors. Seeded into
/// `WorldSchema` at session start (calendar/clock/weather/tone) and then
/// OWNED by the world state: tone renders per-turn with the time + weather.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CardWorld {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// Raw authored time text ("10:00 AM", "Day 1, 09:00") — parsed by
    /// `bracket_parser::parse_in_world_time` at the seed site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weather: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
}

impl CardWorld {
    pub fn is_empty(&self) -> bool {
        self.date.is_none() && self.time.is_none() && self.weather.is_none() && self.tone.is_none()
    }
}

/// The v2 `<inventory>` sibling — comma-separated item lists per line.
/// NPC cards: Clothing is mandatory, the rest optional. Player cards: the
/// whole block is optional. Lines are omitted from the file when empty.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CardInventory {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clothing: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub equipped: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accessories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stored: Vec<String>,
}

impl CardInventory {
    pub fn is_empty(&self) -> bool {
        self.clothing.is_empty()
            && self.equipped.is_empty()
            && self.accessories.is_empty()
            && self.stored.is_empty()
    }
}

/// One Simulation Card, parsed from a `.sim` file. Owned and immutable for the
/// process lifetime after `setup()` loads it.
///
/// Fable cards are v2-ONLY (v0.30.0 clean break): the line-block model
/// (`identity`/`persona`/`world`/`location`/`inventory`) plus the sibling
/// seeds. The legacy element fields (`core_persona`/`appearance`/
/// `conversational_rules`/…) survive for ONE consumer — the `wupi.sim`
/// SYSTEM card, which keeps the legacy shape forever (`render_for_prompt`,
/// the chat persona path).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimCard {
    pub id: String,
    pub name: String,
    /// `"system"` for the OS interface persona (Wupi), `"simulation"` for
    /// every playable card.
    pub card_type: String,
    /// The polymorphic wizard discriminator: `"npc"` | `"scenario"` |
    /// `"world"` | None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    /// True when the file was authored in the v2 layout (line-block
    /// `<identity>`/`<persona>` + `<world>`/`<location>`/`<inventory>`
    /// siblings). False ONLY for the system card (`wupi.sim`, legacy shape
    /// forever) — a `simulation` card with `format_v2 == false` is a
    /// pre-v2 card and is skipped by every Fable load path (v0.30.0).
    #[serde(default)]
    pub format_v2: bool,
    // ── v2 model ─────────────────────────────────────────────────────────
    /// The physical identity traits (line block).
    #[serde(default)]
    pub identity: CardIdentity,
    /// The persona traits (line block). Empty struct = no `<persona>` in the
    /// file (players' opt-in contract).
    #[serde(default)]
    pub persona: CardPersona,
    /// The `<world>` sibling anchors (date/time/weather/tone).
    #[serde(default)]
    pub world: CardWorld,
    /// The `<location>` sibling — the starting location's diegetic name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// The `<inventory>` sibling (npc cards; optional on players).
    #[serde(default)]
    pub inventory: CardInventory,
    /// (2026-08-20 Economy) The `<properties>` sibling — authored property
    /// seeds (pipe-kv lines, `economy::parse_property_lines`). ALL subtypes
    /// may carry it; seeding at session entry forces the owner by card kind
    /// (npc → the card's NPC, scenario/world → authored or Unowned).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<crate::economy::AuthoredProperty>,
    /// (2026-08-22 Codex decoupling) The `<linked_codices>` sibling — display
    /// names of UNIVERSAL codex files (`apps/fable/data/codex/<name>.codex`)
    /// linked to this card, in PRIORITY order (index 0 = top priority; a
    /// same-title collision between two linked files resolves toward the
    /// earlier entry at the seed phase).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_codices: Vec<String>,
    // ── legacy model (the wupi.sim system card ONLY, kept forever) ───────
    pub core_persona: String,
    pub traits: String,
    pub appearance: String,
    pub role_instruction: String,
    pub responsibilities: String,
    pub conversational_rules: String,
    /// Deprecated technical-protocols shim — renders nothing when empty.
    pub technical_rules: String,
    /// One greeting string per line in `<introductions>`. Empty if the card
    /// omits the block. Used by [`random_intro`] for the boot flourish.
    #[serde(default)]
    pub introductions: Vec<String>,
    /// The card's intro text — the SIBLING `<intro>` element AFTER
    /// `</sim_card>`, kept OUT of `<sim_card>` so it never inflates the
    /// cached system prompt (prime directive). The Fable opening beat /
    /// wupi.sim boot-greeting pool. Always mirrors `intro_variants[0]`
    /// when variants exist (parse maintains the invariant).
    #[serde(default)]
    pub intro: String,
    /// (2026-08-22 intro variants) EVERY `<intro>` sibling, in file order —
    /// the FIRST is the default opening (mirrored into [`intro`]), the rest
    /// are ALTERNATES (imported SillyTavern `alternate_greetings`).
    /// `enter_fable_session` seeds them all onto session message 0 as
    /// swipeable variants, so the player picks an opening via the existing
    /// ‹ 1/N › beat control right at game start. Empty ⇔ no intro at all.
    #[serde(default)]
    pub intro_variants: Vec<String>,
    /// The world/setting premise (scenario/world cards keep this dedicated
    /// field in v2 too — Chloe's 2026-08-19 ruling).
    #[serde(default)]
    pub setting: Option<String>,
    /// Narrative consequence philosophy / scenario premise (dedicated field,
    /// kept in v2 for scenario cards).
    #[serde(default)]
    pub plot: Option<String>,
    /// Runtime player-name anchor — SET by `attach_saved_player` when a
    /// SavedPlayer binds (the attached `.player` file owns the name). v2
    /// cards carry no player binding on disk.
    #[serde(default)]
    pub player_name: Option<String>,
    /// Custom extensions: a flat key→value string map. In v2 these ride the
    /// KV-cache block verbatim (narrator flavor), NOT the world-state
    /// `custom:` line.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_tags: BTreeMap<String, String>,
}

impl SimCard {
    /// The card's effective tone — the v2 `<world>` field.
    pub fn effective_tone(&self) -> Option<&str> {
        self.world
            .tone
            .as_deref()
            .filter(|s| !s.trim().is_empty())
    }

    /// Render the persona into a compact `<persona>` block for the WUPI CHAT
    /// system prompt (the `wupi.sim` system-card path — LEGACY model only;
    /// Fable cards use [`SimCard::render_cache_block`]). Returns an empty
    /// `String` for the minimal fallback.
    pub fn render_for_prompt(&self) -> String {
        if self.is_fallback() {
            return String::new();
        }
        let mut sections = Vec::new();

        sections.push(format!(
            "<identity>\nname: {}\npersona: {}\ntraits:\n{}\n</identity>",
            self.name.trim(),
            self.core_persona.trim(),
            indent(self.traits.trim()),
        ));

        if !self.appearance.trim().is_empty() {
            sections.push(format!(
                "<appearance>\n{}\n</appearance>",
                self.appearance.trim()
            ));
        }

        if !self.role_instruction.trim().is_empty() {
            let mut block = format!("<role>\ninstruction: {}\n", self.role_instruction.trim());
            if !self.responsibilities.trim().is_empty() {
                block.push_str(&format!("responsibilities:\n{}\n", indent(self.responsibilities.trim())));
            }
            block.push_str("</role>");
            sections.push(block);
        }

        if !self.conversational_rules.trim().is_empty() {
            sections.push(format!(
                "<conversational_style>\nrules:\n{}\n</conversational_style>",
                indent(self.conversational_rules.trim())
            ));
        }

        if !self.technical_rules.trim().is_empty() {
            sections.push(format!(
                "<technical_protocols>\nrules:\n{}\n</technical_protocols>",
                indent(self.technical_rules.trim())
            ));
        }

        format!("<persona>\n{}\n</persona>", sections.join("\n\n"))
    }

    /// Render the `<sim_card>` cache block — the payload injected verbatim
    /// into the API narrator's system prompt EVERY turn (2026-08-19 Chloe
    /// ruling: everything within `<sim_card>` is always read by the narrator
    /// AI). Identity lines, then persona lines, then custom tags; scenario/
    /// world cards additionally carry their dedicated Setting/Plot prose.
    /// Empty fields are omitted entirely (the same rule as the file).
    pub fn render_cache_block(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str("<sim_card>\n");
        let name = self.name.trim();
        if !name.is_empty() && name != "unknown" {
            out.push_str("Name: ");
            out.push_str(name);
            out.push('\n');
        }
        for (label, value) in identity_lines(&self.identity) {
            out.push_str(&format!("{label}: {value}\n"));
        }
        let persona_block = persona_lines(&self.persona);
        if !persona_block.is_empty() {
            out.push('\n');
            for (label, value) in persona_block {
                out.push_str(&format!("{label}: {value}\n"));
            }
        }
        if !self.custom_tags.is_empty() {
            out.push('\n');
            for (k, v) in &self.custom_tags {
                out.push_str(&format!("{k}: {}\n", v.trim()));
            }
        }
        let setting = self.setting.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if let Some(s) = setting {
            out.push_str(&format!("\nSetting:\n{s}\n"));
        }
        let plot = self.plot.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if let Some(p) = plot {
            out.push_str(&format!("\nPlot:\n{p}\n"));
        }
        out.push_str("</sim_card>");
        out
    }

    /// Serialize the card to the canonical v2 file layout (every Rust-side
    /// rewrite: card write, intro edit, codex link). Byte-matches the
    /// Chloe-authored reference cards (`liam.sim`): 2-space element indent,
    /// metadata children at the same indent, blank lines between blocks,
    /// identity/persona close with an indented `]]>`, siblings close flush.
    /// Never called for `system` cards (they keep the legacy shape forever).
    pub fn serialize_v2(&self) -> String {
        let mut xml = String::with_capacity(2048);
        xml.push_str("<sim_card>\n");
        xml.push_str("  <metadata>\n");
        xml.push_str("  <type>simulation</type>\n");
        if let Some(sub) = self.subtype.as_deref().filter(|s| !s.is_empty()) {
            xml.push_str(&format!("  <subtype>{}</subtype>\n", escape_xml_text(sub)));
        }
        xml.push_str(&format!("  <id>{}</id>\n", escape_xml_text(&self.id)));
        xml.push_str("  </metadata>\n");

        // <identity> — Name always leads; trait lines omitted when empty.
        let mut identity_body = String::new();
        let name = self.name.trim();
        if !name.is_empty() {
            identity_body.push_str(&format!("Name: {name}\n"));
        }
        for (label, value) in identity_lines(&self.identity) {
            identity_body.push_str(&labeled_line(&label, &value));
        }
        xml.push_str(&format!(
            "  <identity><![CDATA[\n{}  ]]></identity>\n",
            cdata_body(&identity_body)
        ));

        // <persona> — omitted ENTIRELY when empty (the player opt-in rule).
        let persona_block = persona_lines(&self.persona);
        if !persona_block.is_empty() {
            let mut body = String::new();
            for (label, value) in persona_block {
                body.push_str(&labeled_line(&label, &value));
            }
            xml.push_str(&format!(
                "  <persona><![CDATA[\n{}  ]]></persona>\n",
                cdata_body(&body)
            ));
        }

        // scenario/world dedicated prose fields (Chloe's ruling: kept).
        if let Some(s) = self.setting.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            xml.push_str(&format!("  <setting><![CDATA[\n{}\n  ]]></setting>\n", cdata_body(s)));
        }
        if let Some(p) = self.plot.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            xml.push_str(&format!("  <plot><![CDATA[\n{}\n  ]]></plot>\n", cdata_body(p)));
        }

        if !self.custom_tags.is_empty() {
            xml.push_str("  <custom_tags>\n");
            for (k, v) in &self.custom_tags {
                xml.push_str(&format!(
                    "    <entry key=\"{}\"><![CDATA[{}]]></entry>\n",
                    escape_xml_text(k),
                    cdata_body(v.trim())
                ));
            }
            xml.push_str("  </custom_tags>\n");
        }
        xml.push_str("</sim_card>\n");

        // ── siblings (mutable world-state seeds, OUTSIDE the cache root) ──
        if !self.world.is_empty() {
            let mut body = String::new();
            if let Some(v) = self.world.date.as_deref().filter(|s| !s.trim().is_empty()) {
                body.push_str(&format!("Date: {}\n", v.trim()));
            }
            if let Some(v) = self.world.time.as_deref().filter(|s| !s.trim().is_empty()) {
                body.push_str(&format!("Time: {}\n", v.trim()));
            }
            if let Some(v) = self.world.weather.as_deref().filter(|s| !s.trim().is_empty()) {
                body.push_str(&format!("Weather: {}\n", v.trim()));
            }
            if let Some(v) = self.world.tone.as_deref().filter(|s| !s.trim().is_empty()) {
                body.push_str(&format!("Tone: {}\n", v.trim()));
            }
            xml.push_str(&format!("\n<world><![CDATA[\n{}]]></world>\n", cdata_body(&body)));
        }

        if let Some(loc) = self.location.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            xml.push_str(&format!("\n<location><![CDATA[\n{}\n]]></location>\n", cdata_body(loc)));
        }

        // (2026-08-22 intro variants) ONE `<intro>` sibling per variant — the
        // first is the default opening, the rest are the player-selectable
        // alternates (imported SillyTavern alternate_greetings). `intro` is
        // the variant-0 mirror, so the single-intro card shape is identical
        // to what this always emitted.
        let intro_variants: Vec<&str> = if self.intro_variants.is_empty() {
            // Defensive mirror discipline: a hand-built card with only
            // `intro` set still serializes its one intro.
            let intro = self.intro.trim();
            if intro.is_empty() { Vec::new() } else { vec![intro] }
        } else {
            self.intro_variants
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect()
        };
        for variant in intro_variants {
            xml.push_str(&format!("\n<intro><![CDATA[\n{}]]></intro>\n", cdata_body(variant)));
        }

        // Inventory: mandatory-and-validated on npc cards (Chloe: clothing
        // mandatory there), but ROUND-TRIPPED for every subtype — the parser
        // reads the sibling regardless of `<subtype>`, so dropping a
        // hand-authored `<inventory>` on a scenario/world card here would
        // silently eat it on the next Rust-side rewrite (intro reroll,
        // linked-codex change). Omitted entirely when empty, all subtypes.
        if !self.inventory.is_empty() {
            let mut body = String::new();
            if !self.inventory.clothing.is_empty() {
                body.push_str(&format!("Clothing: {}\n", self.inventory.clothing.join(", ")));
            }
            if !self.inventory.equipped.is_empty() {
                body.push_str(&format!("Equipped: {}\n", self.inventory.equipped.join(", ")));
            }
            if !self.inventory.accessories.is_empty() {
                body.push_str(&format!("Accessories: {}\n", self.inventory.accessories.join(", ")));
            }
            if !self.inventory.stored.is_empty() {
                body.push_str(&format!("Stored: {}\n", self.inventory.stored.join(", ")));
            }
            xml.push_str(&format!("\n<inventory><![CDATA[\n{}]]></inventory>\n", cdata_body(&body)));
        }

        // (2026-08-20 Economy) The authored <properties> sibling — ALL
        // subtypes (an npc card's forge, a world card's town treasuries).
        // Omitted entirely when empty.
        if !self.properties.is_empty() {
            let body = crate::economy::render_property_lines(&self.properties);
            xml.push_str(&format!(
                "\n<properties><![CDATA[\n{}]]></properties>\n",
                cdata_body(&body)
            ));
        }

        // (2026-08-22 Codex decoupling) The universal-codex link list — one
        // display name per line, priority order. Omitted entirely when empty.
        if !self.linked_codices.is_empty() {
            let body = self.linked_codices.join("\n");
            xml.push_str(&format!(
                "\n<linked_codices><![CDATA[\n{}]]></linked_codices>\n",
                cdata_body(&body)
            ));
        }

        xml
    }

    /// Pick one introduction line at random. Returns `None` if the card has
    /// no introductions (the caller then shows no boot bubble). Called once
    /// per boot via the `get_intro` IPC command.
    pub fn random_intro(&self) -> Option<&str> {
        if self.introductions.is_empty() {
            return None;
        }
        let mut rng = rand::rng();
        self.introductions.choose(&mut rng).map(String::as_str)
    }

    /// The fallback stub has this sentinel id so `render_for_prompt` can
    /// detect it and emit nothing (suppressing the `<persona>` section
    /// entirely).
    fn is_fallback(&self) -> bool {
        self.id == FALLBACK_ID
    }
}

/// The ordered `(label, value)` view of a [`CardIdentity`] — the single
/// ordering authority for both the cache block and `serialize_v2`. Fixed
/// labels first, then the parsed `extra` pairs verbatim: hand-authored
/// lines like "Alignment: Chaotic Good" are identity payload — they ride
/// the narrator cache AND survive re-serialization (2026-08-20 audit H1:
/// the old `&'static str` return type could not carry them, so every
/// cache render + rewrite silently dropped them).
fn identity_lines(id: &CardIdentity) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (label, v) in [
        ("Gender", &id.gender),
        ("Race", &id.race),
        ("Age", &id.age),
        ("Height", &id.height),
        ("Weight", &id.weight),
        ("Body", &id.body),
        ("Skin", &id.skin),
        ("Eyes", &id.eyes),
        ("Hair Color", &id.hair_color),
        ("Hair Length", &id.hair_length),
        ("Hair Style", &id.hair_style),
    ] {
        if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push((label.to_owned(), s.to_owned()));
        }
    }
    for (label, value) in &id.extra {
        let label = label.trim();
        let value = value.trim();
        if !label.is_empty() && !value.is_empty() {
            out.push((label.to_owned(), value.to_owned()));
        }
    }
    out
}

/// The ordered `(label, value)` view of a [`CardPersona`] — fixed labels
/// first, then the `extra` pairs (same discipline as `identity_lines`).
fn persona_lines(p: &CardPersona) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (label, v) in [
        ("Personality", &p.personality),
        ("Conversation Style", &p.conversation_style),
        ("Likes", &p.likes),
        ("Dislikes", &p.dislikes),
        ("Flaws", &p.flaws),
        ("Goals", &p.goals),
        ("Occupation", &p.occupation),
        ("Backstory", &p.backstory),
    ] {
        if let Some(s) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push((label.to_owned(), s.to_owned()));
        }
    }
    for (label, value) in &p.extra {
        let label = label.trim();
        let value = value.trim();
        if !label.is_empty() && !value.is_empty() {
            out.push((label.to_owned(), value.to_owned()));
        }
    }
    out
}

/// Escape text for XML element content (attribute-safe: quotes escaped too,
/// so the same helper serves `<entry key="…">`).
fn escape_xml_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// CDATA inner body — split any literal `]]>` so the section can't close
/// early (the same segmentation the JS serializer uses).
fn cdata_body(s: &str) -> String {
    s.replace("]]>", "]]]]><![CDATA[>")
}

/// Indent every non-empty line of a block by two spaces, so list items nest
/// cleanly inside their parent XML section.
fn indent(block: &str) -> String {
    block
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("  {}", line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

const FALLBACK_ID: &str = "__wupi_fallback__";

/// Build the minimal fallback card used when the real card file is missing or
/// unparseable. The app still boots; the persona section is simply suppressed
/// (`render_for_prompt` returns empty for the fallback). Loud warning is the
/// caller's job: this fn is silent. Public so `setup()` can reach it directly
/// when no card path resolved at all.
pub fn fallback() -> SimCard {
    SimCard {
        id: FALLBACK_ID.to_owned(),
        name: "Wupi".to_owned(),
        card_type: "system".to_owned(),
        subtype: None,
        format_v2: false,
        identity: CardIdentity::default(),
        persona: CardPersona::default(),
        world: CardWorld::default(),
        location: None,
        inventory: CardInventory::default(),
        properties: Vec::new(),
        linked_codices: Vec::new(),
        core_persona: String::new(),
        traits: String::new(),
        appearance: String::new(),
        role_instruction: String::new(),
        responsibilities: String::new(),
        conversational_rules: String::new(),
        technical_rules: String::new(),
        introductions: Vec::new(),
        intro: String::new(),
        intro_variants: Vec::new(),
        setting: None,
        plot: None,
        player_name: None,
        custom_tags: BTreeMap::new(),
    }
}

/// Load a `.sim` card from disk, falling back to a minimal stub on any error
/// (missing file, IO error, malformed XML, missing required fields). The
/// persona is best-effort: a bad card must never kill the OS boot. Mirrors
/// the embedder's graceful-degradation contract (§2M).
pub fn load_or_fallback(path: &Path) -> SimCard {
    match try_load(path) {
        Ok(card) => {
            tracing::info!(
                card_path = %path.display(),
                card_id = %card.id,
                card_name = %card.name,
                intros = card.introductions.len(),
                "simulation card loaded"
            );
            card
        }
        Err(e) => {
            tracing::warn!(
                card_path = %path.display(),
                error = %format!("{e}"),
                "simulation card load failed; using minimal fallback (persona section suppressed)"
            );
            fallback()
        }
    }
}

fn try_load(path: &Path) -> anyhow::Result<SimCard> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading card file: {e}"))?;
    parse(&text)
}

/// Parse a `.sim` card from its XML text. Public entry point for callers that
/// have the XML in memory (not on disk): parses a `<sim_card>...</sim_card>`
/// string this way without writing a temp file. Mirrors `try_load`'s parser
/// exactly (delegates to the same private `parse`).
///
/// Returns `Err` on malformed XML or a missing `<sim_card>` root — the caller
/// decides the fallback.
pub fn parse_from_xml_str(xml: &str) -> anyhow::Result<SimCard> {
    parse(xml)
}

/// Find the byte offset just past the `</sim_card>` close tag terminating the
/// root element — CDATA/comment-aware + whitespace-tolerant (P2 hardening).
/// The naive `xml.find("</sim_card>")` broke on (a) the literal string inside
/// authored CDATA prose (legal XML — the slice cut mid-CDATA, the head became
/// unparseable, and the card silently degraded to the fallback stub) and
/// (b) whitespace before `>` (`</sim_card >` — valid XML the find missed).
/// Pub(crate): the intro-variant write-through paths slice the same two-root
/// boundary when rewriting the `<intro>` siblings.
pub(crate) fn find_root_close(xml: &str) -> Option<usize> {
    find_tag_close(xml, "sim_card")
}

/// The CDATA/comment-aware close-tag scanner shared by the two-root formats
/// (`.sim` slices at `</sim_card>`, `.player` at `</player>`): skips CDATA
/// bodies + comments, tolerates whitespace before `>`, rejects prefix
/// collisions (`</sim_cardboard`).
pub(crate) fn find_tag_close(xml: &str, tag: &str) -> Option<usize> {
    let close = format!("</{tag}");
    let b = xml.as_bytes();
    let starts = |i: usize, pat: &[u8]| b.len() >= i + pat.len() && &b[i..i + pat.len()] == pat;
    let mut i = 0;
    while i < b.len() {
        if starts(i, b"<![CDATA[") {
            // CDATA runs to the first `]]>` — everything inside is literal text.
            let mut j = i + 9;
            while j + 3 <= b.len() && !starts(j, b"]]>") {
                j += 1;
            }
            i = (j + 3).min(b.len());
            continue;
        }
        if starts(i, b"<!--") {
            let mut j = i + 4;
            while j + 3 <= b.len() && !starts(j, b"-->") {
                j += 1;
            }
            i = (j + 3).min(b.len());
            continue;
        }
        if starts(i, close.as_bytes()) {
            let mut j = i + close.len();
            while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'>' {
                return Some(j + 1);
            }
            // Not the close tag (e.g. `</sim_cardboard`) — keep scanning.
        }
        i += 1;
    }
    None
}

/// Parse a `.sim` card from its XML text. Separated from `try_load` so the
/// unit tests can exercise the parser without touching the filesystem.
fn parse(xml: &str) -> anyhow::Result<SimCard> {
    // A `.sim` file is a TWO-ROOT document: `<sim_card>…</sim_card>` + the
    // optional siblings (`<world>`/`<location>`/`<intro>`/`<inventory>`)
    // AFTER it. roxmltree REJECTS multi-root documents outright, so slice at
    // the root's closing tag + parse the head as the document; the TAIL is
    // re-parsed below, wrapped in a synthetic root, to fish out the
    // siblings. A string with no close tag slices to itself + an empty tail.
    let (head, tail) = match find_root_close(xml) {
        Some(end) => xml.split_at(end),
        None => (xml, ""),
    };
    let doc = roxmltree::Document::parse(head).map_err(|e| {
        let hint = if tail.trim().is_empty() {
            String::new()
        } else {
            " (a sibling tail exists after the root — the head is unbalanced; \
             check for an unclosed tag or CDATA section inside <sim_card>)"
                .to_string()
        };
        anyhow::anyhow!("parsing card XML: {e}{hint}")
    })?;
    let root = doc
        .root_element()
        .has_tag_name("sim_card")
        .then_some(doc.root_element())
        .ok_or_else(|| anyhow::anyhow!("root element must be <sim_card>"))?;

    // (2026-08-24 data-loss guard) A NON-EMPTY tail must be well-formed XML.
    // The sibling readers below are deliberately lenient (warn + default),
    // but a tail with one raw `&`/`<` outside CDATA parsed with EVERY
    // sibling silently defaulted — and the next serialize_v2 rewrite
    // (with_intro_variant / with_linked_codices / the raw editor) then
    // permanently deleted world/location/inventory/properties/
    // linked_codices from disk (the re-parse self-checks verify only the
    // field they touched). Refuse the whole card instead: the file is
    // broken, the user fixes it, and nothing rewrites a husk over authored
    // content.
    if !tail.trim().is_empty() {
        let wrapped = format!("<wupi_sim_siblings>{tail}</wupi_sim_siblings>");
        if let Err(e) = roxmltree::Document::parse(&wrapped) {
            anyhow::bail!(
                "card sibling tail (after </sim_card>) is not well-formed XML: {e} — \
                 fix the raw & or < outside CDATA; refusing to parse because a \
                 lenient parse would default every sibling and the next rewrite \
                 would delete them"
            );
        }
    }

    // ── v2 detection ──────────────────────────────────────────────────────
    // v2: the DIRECT `<identity>` child has NO element children (a pure
    // CDATA line block) and/or a DIRECT `<persona>` child exists. The
    // legacy shape (persona nested INSIDE `<identity>`, element children)
    // survives for ONE file: the `wupi.sim` system card. A `simulation`
    // card parsing as non-v2 is pre-v2 and is skipped by every Fable load
    // path (v0.30.0).
    let identity_el = first_child(root, "identity");
    let identity_is_line_block = identity_el
        .map(|n| !n.children().any(|c| c.is_element()))
        .unwrap_or(false);
    let persona_direct = first_child(root, "persona");
    let format_v2 = identity_is_line_block || persona_direct.is_some();

    // ── name + id ─────────────────────────────────────────────────────────
    let v2_identity_lines = if identity_is_line_block {
        parse_labeled_lines(&identity_el.map(text_content).unwrap_or_default())
    } else {
        Vec::new()
    };
    let name_from_lines = labeled_get(&v2_identity_lines, &["name"]);
    let name = name_from_lines
        .filter(|s| !s.is_empty())
        .or_else(|| {
            first_child(root, "identity").and_then(|n| child_text(n, "name"))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let is_sentinel = |s: &str| {
        matches!(
            s,
            crate::memory::WUPI_CARD_ID
                | crate::memory::WUPI_SYSTEM_CARD_ID
                | crate::memory::FABLE_SYSTEM_CARD_ID
                | crate::memory::CODEX_CARD_ID
        )
    };
    // The id is the slugified `<metadata><id>` (or name-derived fallback),
    // filtered against memory sentinels — a card must never share a
    // partition key with __wupi__/__codex__ etc. The MEMORY PARTITION keys
    // off this slug even though the FOLDER is now display-named (2026-08-19
    // identity split: folder = display name, id = slug).
    let id = nested_text(root, "metadata", "id")
        .as_deref()
        .and_then(crate::slugify_card_stem)
        .filter(|v| !is_sentinel(v))
        .or_else(|| crate::slugify_card_stem(&name))
        .filter(|v| !is_sentinel(v))
        .unwrap_or_else(|| "unknown".to_owned());
    // `<type>` read verbatim ("simulation" for playable cards; "system" for
    // wupi.sim; anything else default-treats as system).
    let card_type = nested_text(root, "metadata", "type").unwrap_or_else(|| "system".to_owned());
    let subtype = nested_text(root, "metadata", "subtype").filter(|s| !s.is_empty());

    // ── legacy model parse (the wupi.sim system card ONLY) ────────────────
    let legacy_identity = first_child(root, "identity");
    let core_persona = legacy_identity
        .and_then(|n| child_text(n, "persona"))
        .unwrap_or_default();
    let traits = legacy_identity
        .and_then(|n| child_text(n, "traits"))
        .unwrap_or_default();

    let appearance = first_child(root, "appearance")
        .map(|n| {
            let mut lines = Vec::new();
            for child in n.children().filter(|c| c.is_element()) {
                let tag = child.tag_name().name();
                let val = text_content(child);
                if val.trim().is_empty() {
                    lines.push(tag.to_owned());
                } else {
                    lines.push(format!("{tag}: {}", val.trim()));
                }
            }
            lines.join("\n")
        })
        .unwrap_or_default();

    let role = first_child(root, "role");
    let role_instruction = role
        .and_then(|n| child_text(n, "instruction"))
        .unwrap_or_default();
    let responsibilities = role
        .and_then(|n| child_text(n, "responsibilities"))
        .unwrap_or_default();

    let conversational_rules = first_child(root, "conversational_style")
        .and_then(|n| child_text(n, "rules"))
        .unwrap_or_default();

    let technical_rules = first_child(root, "technical_protocols")
        .and_then(|n| child_text(n, "rules"))
        .unwrap_or_default();

    // ── siblings from the tail (v2 world-state seeds + the intro) ────────
    let sibling_world = sibling_text(tail, &["world"])
        .map(|t| parse_world_lines(&t))
        .unwrap_or_default();
    let location = sibling_text(tail, &["location"])
        .map(|t| t.trim().to_owned())
        .filter(|s| !s.is_empty());
    let sibling_inventory = sibling_text(tail, &["inventory"])
        .map(|t| parse_inventory_lines(&t))
        .unwrap_or_default();
    // (2026-08-20 Economy) The authored <properties> sibling.
    let sibling_properties = sibling_text(tail, &["properties"])
        .map(|t| crate::economy::parse_property_lines(&t))
        .unwrap_or_default();
    // (2026-08-22 Codex decoupling) The universal-codex link list — one
    // display name per line, priority order.
    let linked_codices = sibling_text(tail, &["linked_codices"])
        .map(|t| parse_codex_name_lines(&t))
        .unwrap_or_default();
    // (2026-08-22 intro variants) ALL <intro> siblings — the first mirrors
    // into `intro`, the full list seeds the opening-beat variants at session
    // entry.
    let intro_variants = extract_sibling_intros(tail);
    let intro = intro_variants.first().cloned().unwrap_or_default();

    let introductions = first_child(root, "introductions")
        .map(|n| {
            text_content(n)
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(|l| l.strip_prefix("- ").unwrap_or(l).trim().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            if intro.is_empty() {
                Vec::new()
            } else {
                intro
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .map(|l| l.strip_prefix("- ").unwrap_or(l).trim().to_owned())
                    .collect()
            }
        });

    // ── scenario/world dedicated prose (direct top-level children, v2) ──
    let setting = child_text(root, "setting").filter(|s| !s.is_empty());
    let plot = child_text(root, "plot").filter(|s| !s.is_empty());

    let custom_tags = first_child(root, "custom_tags")
        .map(|ct_el| {
            ct_el
                .children()
                .filter(|c| c.is_element() && c.has_tag_name("entry"))
                .filter_map(|entry_el| {
                    let key = entry_el.attribute("key").unwrap_or("").trim().to_owned();
                    if key.is_empty() {
                        return None;
                    }
                    let value = text_content(entry_el).trim().to_owned();
                    if value.is_empty() {
                        return None;
                    }
                    Some((key, value))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    // ── build the v2 model ────────────────────────────────────────────────
    // Fable cards are v2-only (the walkers skip `format_v2 == false`
    // simulation cards — the v0.30.0 clean break); the system card carries
    // none of these blocks/siblings, so its model is the all-default shape.
    let identity = identity_from_lines(&v2_identity_lines);
    let persona = persona_from_lines(&parse_labeled_lines(
        &persona_direct.map(text_content).unwrap_or_default(),
    ));
    let world = sibling_world;
    let inventory = sibling_inventory;
    let properties = sibling_properties;

    Ok(SimCard {
        id,
        name,
        card_type,
        subtype,
        format_v2,
        identity,
        persona,
        world,
        location,
        inventory,
        properties,
        linked_codices,
        core_persona,
        traits,
        appearance,
        role_instruction,
        responsibilities,
        conversational_rules,
        technical_rules,
        introductions,
        intro,
        intro_variants,
        setting,
        plot,
        player_name: None,
        custom_tags,
    })
}

/// Parse a CDATA line block ("Label: value" lines) into ordered pairs.
/// Multi-line values append to the previous label; a line with no colon
/// appends to the previous label too (soft continuation); unknown labels are
/// KEPT (the extras ride the cache + survive re-serialization). Blank lines
/// separate. Shared with `player.rs` (the `.player` format speaks the same
/// line-block grammar).
///
/// (2026-08-24 multi-line round-trip) A line whose trimmed form starts with
/// `|` is an EXPLICIT continuation — it appends to the previous label
/// newline-preserved EVEN when it carries a colon. Prose like
/// "Chapter 1: The fire" would otherwise parse as a NEW label and silently
/// truncate the field at its first colon-bearing line (a multi-paragraph
/// backstory was mangled this way on every load). Writers mark continuation
/// lines through [`labeled_line`].
pub(crate) fn parse_labeled_lines(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix('|') {
            if let Some(last) = out.last_mut() {
                last.1.push('\n');
                last.1.push_str(rest.trim());
            }
            continue;
        }
        if let Some((label, value)) = line.split_once(':') {
            let label = label.trim();
            if !label.is_empty() {
                out.push((label.to_owned(), value.trim().to_owned()));
                continue;
            }
        }
        // Continuation line — append to the previous label's value.
        if let Some(last) = out.last_mut() {
            if !last.1.is_empty() {
                last.1.push(' ');
            }
            last.1.push_str(line.trim());
        }
    }
    out
}

/// Render one `Label: value` line-block entry with multi-line values
/// round-trip-safe: every line past the first carries the `|` marker
/// [`parse_labeled_lines`] folds back (newline-preserved). Without the
/// marker, a value line containing a colon would re-parse as a NEW label
/// and the field would silently truncate at it on the next load.
pub(crate) fn labeled_line(label: &str, value: &str) -> String {
    let mut out = String::new();
    let mut lines = value.lines();
    if let Some(first) = lines.next() {
        out.push_str(label);
        out.push_str(": ");
        out.push_str(first.trim());
        out.push('\n');
    }
    // Blank paragraph breaks ride as bare `|` lines so the parse folds
    // them back into the value verbatim.
    for cont in lines {
        out.push_str("| ");
        out.push_str(cont.trim());
        out.push('\n');
    }
    out
}

/// Fetch a value from labeled lines by normalized label (first match).
pub(crate) fn labeled_get<'a>(lines: &'a [(String, String)], candidates: &[&str]) -> Option<String> {
    for (label, value) in lines {
        let norm = normalize_label(label);
        if let Some(n) = norm.as_deref() {
            if candidates.contains(&n) {
                return Some(value.clone());
            }
        }
    }
    None
}

/// Normalize a human label to its field key: lowercase, spaces/underscores/
/// hyphens collapsed ("Hair Color" / "hair color" / "hair_color" →
/// "hair_color"; "Conversation Style" → "conversation_style"). Also tolerates
/// known drift ("Job" → "occupation", "Goal" → "goals", "Dialogue Style" →
/// "conversation_style").
pub(crate) fn normalize_label(label: &str) -> Option<String> {
    let mut key = String::new();
    for ch in label.trim().chars() {
        if ch.is_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
        } else if !key.is_empty() && !key.ends_with('_') {
            key.push('_');
        }
    }
    let key = key.trim_matches('_').to_owned();
    if key.is_empty() {
        return None;
    }
    Some(match key.as_str() {
        "job" => "occupation".to_owned(),
        "goal" => "goals".to_owned(),
        "dialogue_style" | "conversation" => "conversation_style".to_owned(),
        other => other.to_owned(),
    })
}

/// Build a [`CardIdentity`] from labeled lines, keeping unrecognized labels
/// as `extra`.
fn identity_from_lines(lines: &[(String, String)]) -> CardIdentity {
    let mut id = CardIdentity::default();
    let get = |k: &str| labeled_get(lines, &[k]);
    id.gender = get("gender").filter(|v| !v.is_empty());
    id.race = get("race").filter(|v| !v.is_empty());
    id.age = get("age").filter(|v| !v.is_empty());
    id.height = get("height").filter(|v| !v.is_empty());
    id.weight = get("weight").filter(|v| !v.is_empty());
    id.body = get("body").filter(|v| !v.is_empty());
    id.skin = get("skin").filter(|v| !v.is_empty());
    id.eyes = get("eyes").filter(|v| !v.is_empty());
    id.hair_color = get("hair_color").filter(|v| !v.is_empty());
    id.hair_length = get("hair_length").filter(|v| !v.is_empty());
    id.hair_style = get("hair_style").filter(|v| !v.is_empty());
    let known = [
        "name", "gender", "race", "age", "height", "weight", "body", "skin", "eyes",
        "hair_color", "hair_length", "hair_style",
    ];
    for (label, value) in lines {
        if let Some(n) = normalize_label(label) {
            if known.contains(&n.as_str()) || value.is_empty() {
                continue;
            }
        }
        id.extra.push((label.clone(), value.clone()));
    }
    id
}

/// Build a [`CardPersona`] from labeled lines, keeping unrecognized labels as
/// `extra`.
fn persona_from_lines(lines: &[(String, String)]) -> CardPersona {
    let mut p = CardPersona::default();
    let get = |k: &str| labeled_get(lines, &[k]);
    p.personality = get("personality").filter(|v| !v.is_empty());
    p.conversation_style = get("conversation_style").filter(|v| !v.is_empty());
    p.likes = get("likes").filter(|v| !v.is_empty());
    p.dislikes = get("dislikes").filter(|v| !v.is_empty());
    p.flaws = get("flaws").filter(|v| !v.is_empty());
    p.goals = get("goals").filter(|v| !v.is_empty());
    p.occupation = get("occupation").filter(|v| !v.is_empty());
    p.backstory = get("backstory").filter(|v| !v.is_empty());
    let known = [
        "personality", "conversation_style", "likes", "dislikes", "flaws", "goals",
        "occupation", "backstory",
    ];
    for (label, value) in lines {
        if let Some(n) = normalize_label(label) {
            if known.contains(&n.as_str()) || value.is_empty() {
                continue;
            }
        }
        p.extra.push((label.clone(), value.clone()));
    }
    p
}

/// Parse the `<world>` sibling's labeled lines (Date/Time/Weather/Tone).
fn parse_world_lines(text: &str) -> CardWorld {
    let lines = parse_labeled_lines(text);
    let get = |k: &str| labeled_get(&lines, &[k]);
    CardWorld {
        date: get("date").filter(|v| !v.is_empty()),
        time: get("time").filter(|v| !v.is_empty()),
        weather: get("weather").filter(|v| !v.is_empty()),
        tone: get("tone").filter(|v| !v.is_empty()),
    }
}

/// Parse the `<inventory>` sibling's labeled lines (comma lists per line).
fn parse_inventory_lines(text: &str) -> CardInventory {
    let lines = parse_labeled_lines(text);
    let get = |k: &str| labeled_get(&lines, &[k]).map(|v| split_csv(&v)).unwrap_or_default();
    CardInventory {
        clothing: get("clothing"),
        equipped: get("equipped"),
        accessories: get("accessories"),
        stored: get("stored"),
    }
}

/// Split a comma-separated item list ("Pink Hoodie, Gray Jeans") into trimmed
/// non-empty items. Shared with `player.rs`.
pub(crate) fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .collect()
}

/// Parse the `<linked_codices>` sibling's name-per-line list (plain lines,
/// not labeled — codex names are free-form display stems). Trimmed, non-empty
/// (a defensive `.codex` suffix is stripped), case-insensitively deduped
/// preserving order (index 0 = top priority).
fn parse_codex_name_lines(text: &str) -> Vec<String> {
    clean_codex_names(
        &text
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.to_owned())
            .collect::<Vec<_>>(),
    )
}

/// Canonicalize a caller-supplied codex-name list: trim, strip a defensive
/// `.codex` suffix, drop empties, case-insensitively dedupe preserving order.
pub(crate) fn clean_codex_names(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for raw in names {
        let name = raw.trim().strip_suffix(".codex").unwrap_or(raw).trim();
        if name.is_empty() {
            continue;
        }
        if !out.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            out.push(name.to_owned());
        }
    }
    out
}

/// (2026-08-22 intro variants) Rewrite ONE `<intro>` variant of a `.sim`
/// file — the live-sync write core behind Fable's in-game intro editing.
/// PURE: returns the new file bytes; the caller persists (`write_atomic`).
/// `index: Some(i < len)` REPLACES variant i (an in-game edit of the active
/// opening); `None` (or an out-of-range index) APPENDS a new variant (an
/// authored "save as new opening" or a reroll's fresh roll). The `intro`
/// mirror follows variant 0. Empty/whitespace text is an error (an intro
/// variant is never blank). Round-trips through parse → mutate →
/// `serialize_v2` with a re-parse self-check.
pub fn with_intro_variant(xml: &str, index: Option<usize>, text: &str) -> anyhow::Result<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        anyhow::bail!("intro variant text is empty");
    }
    let mut card = parse_from_xml_str(xml)?;
    // (2026-08-23 audit fix) Same serialize_v2 guard as with_linked_codices
    // — a pre-v2 card must never round-trip through the v2 serializer.
    anyhow::ensure!(
        card.card_type == "simulation" && card.format_v2,
        "with_intro_variant: refuses a pre-v2 card (serialize_v2 would destroy its legacy content)"
    );
    match index {
        Some(i) if i < card.intro_variants.len() => {
            card.intro_variants[i] = trimmed.to_owned();
        }
        _ => {
            // Append (a duplicate of an existing variant is allowed — the
            // session seeding dedupes, and an author may genuinely want two
            // openings that start identically).
            card.intro_variants.push(trimmed.to_owned());
        }
    }
    if let Some(first) = card.intro_variants.first() {
        card.intro = first.clone();
    }
    let out = card.serialize_v2();
    let reparsed = parse_from_xml_str(&out)?;
    // (2026-08-22 review) Compare CONTENT, not just count — a serializer/
    // parser asymmetry that permuted or mangled variant text while keeping
    // the count would have passed the old length-only check.
    if reparsed.intro_variants != card.intro_variants {
        anyhow::bail!(
            "intro variant rewrite self-check failed: wrote {} variants {:?}, re-parsed {} variants",
            card.intro_variants.len(),
            card.intro_variants,
            reparsed.intro_variants.len()
        );
    }
    Ok(out)
}

/// (2026-08-22 Codex decoupling) Rewrite a `.sim` file's `<linked_codices>`
/// sibling — the universal-codex link list, priority order. PURE: returns the
/// new file bytes; the caller persists (`write_atomic`). Round-trips through
/// parse → set → `serialize_v2`; the result is re-parsed + verified
/// (`linked_codices == names`) before it is returned — a write that would
/// corrupt the file errors out instead of shipping.
pub fn with_linked_codices(xml: &str, names: &[String]) -> anyhow::Result<String> {
    let mut card = parse_from_xml_str(xml)?;
    // (2026-08-23 audit fix) The serialize_v2 round-trip re-emits only the
    // v2 model — rewriting a pre-v2 card here would destroy its legacy
    // content down to a husk (the sweep callers gate on format_v2 too; this
    // is the defensive backstop for any future caller).
    anyhow::ensure!(
        card.card_type == "simulation" && card.format_v2,
        "with_linked_codices: refuses a pre-v2 card (serialize_v2 would destroy its legacy content)"
    );
    let cleaned = clean_codex_names(names);
    card.linked_codices = cleaned.clone();
    let out = card.serialize_v2();
    // Self-check: the emitted file must parse and carry exactly the asked-for
    // link list.
    let reparsed = parse_from_xml_str(&out)?;
    if reparsed.linked_codices != cleaned {
        anyhow::bail!(
            "linked_codices rewrite self-check failed: wrote {:?}, re-parsed {:?}",
            cleaned,
            reparsed.linked_codices
        );
    }
    Ok(out)
}

// roxmltree's API is verbose; these thin wrappers keep the parser readable.
// CDATA is already merged into `.text()` by roxmltree, so `text_content`
// returns the full text of a node regardless of how it was wrapped.

/// (2026-08-22 intro variants) EVERY sibling `<intro>` in the tail, in file
/// order, trimmed + empties dropped. A card may carry several (imported
/// SillyTavern `alternate_greetings` — the importer writes one `<intro>` per
/// greeting); the first is the default opening, the rest are player-selectable
/// alternates at game start. A single-`<intro>` card parses to a one-element
/// vec — identical shape.
fn extract_sibling_intros(tail: &str) -> Vec<String> {
    if tail.trim().is_empty() {
        return Vec::new();
    }
    let wrapped = format!("<wupi_sim_siblings>{tail}</wupi_sim_siblings>");
    let doc = match roxmltree::Document::parse(&wrapped) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(?e, "sim_card: unparsable post-</sim_card> tail — ignoring");
            return Vec::new();
        }
    };
    doc.root_element()
        .children()
        .filter(|n| n.is_element() && n.has_tag_name("intro"))
        .map(text_content)
        .map(|t| t.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Read the trimmed text of the FIRST sibling element matching any of
/// `tags` from the post-root tail. `None` when the tail is empty,
/// unparsable, or carries no such element (all routine — never an error).
/// Pub(crate): `player.rs` reads its `<inventory>` sibling through it.
pub(crate) fn sibling_text(tail: &str, tags: &[&str]) -> Option<String> {
    if tail.trim().is_empty() {
        return None;
    }
    let wrapped = format!("<wupi_sim_siblings>{tail}</wupi_sim_siblings>");
    let doc = match roxmltree::Document::parse(&wrapped) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(?e, "sim_card: unparsable post-</sim_card> tail — ignoring");
            return None;
        }
    };
    doc.root_element()
        .children()
        .find(|n| n.is_element() && tags.iter().any(|t| n.has_tag_name(*t)))
        .map(|n| text_content(n))
        .filter(|s| !s.trim().is_empty())
}

/// The concatenated text of a node (CDATA + plain text children merged).
/// `Node::text()` returns only the FIRST text child — legal prose split
/// across CDATA/comment/CDATA spans silently truncated at the comment.
/// Concatenate EVERY text child in document order. Pub(crate) alias
/// `node_text` — `player.rs` shares it for the `.player` format.
fn text_content(node: roxmltree::Node) -> String {
    if !node.has_children() {
        return node.text().unwrap_or("").to_owned();
    }
    let mut out = String::new();
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                out.push_str(t);
            }
        }
    }
    out
}

/// Shared text-of-node read (see [`text_content`]).
pub(crate) fn node_text(node: roxmltree::Node) -> String {
    text_content(node)
}

/// Find the first direct child element with the given tag name.
fn first_child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    tag: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|c| c.is_element() && c.has_tag_name(tag))
}

/// Text of a direct child element, trimmed.
fn child_text(node: roxmltree::Node, tag: &str) -> Option<String> {
    first_child(node, tag).map(text_content).map(|s| s.trim().to_owned())
}

/// Text of `root → <parent> → <child>`. Returns `None` if either step is
/// absent (so optional fields like `metadata/type` degrade cleanly).
fn nested_text(root: roxmltree::Node, parent: &str, child: &str) -> Option<String> {
    first_child(root, parent).and_then(|n| child_text(n, child))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<sim_card>
  <metadata>
    <id>wupi</id>
    <name>Wupi</name>
    <type>system</type>
  </metadata>
  <identity>
    <name>Wupi</name>
    <persona>A cheerful catgirl.</persona>
    <traits><![CDATA[
- Devoted to Master.
- Clumsy but eager.
    ]]></traits>
  </identity>
  <appearance>
    <race>Catgirl</race>
    <cat_ears>Perky and expressive.</cat_ears>
  </appearance>
  <role>
    <instruction>Help Master manage the system.</instruction>
    <responsibilities><![CDATA[
- Chat naturally.
- Manage settings.
    ]]></responsibilities>
  </role>
  <conversational_style>
    <rules><![CDATA[
- Use "nya~".
    ]]></rules>
  </conversational_style>
  <introductions><![CDATA[
- "Hello Master~" (=^-ω-^=)
- "Booted up, nya~" ฅ^>⩊<^ฅ
  ]]></introductions>
</sim_card>"#;

    /// The Chloe-authored v2 reference layout (liam.sim, minus the stray
    /// closing tag her hand-edit carried).
    const V2_NPC: &str = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>npc</subtype>
  <id>liam</id>
  </metadata>

  <identity><![CDATA[
Name: Liam
Gender: Male
Race: Human
Age: 21
Height: 5'10"
Weight: 150 lbs
Body: Petite and hairless
Skin: Pale
Eyes: Pink
Hair Color: Pink
Hair Length: Shoulder-length
Hair Style: Styled neatly
  ]]></identity>

  <persona><![CDATA[
Personality: Mischievous and a shameless flirt.
Conversation Style: Semi-innocent tone with playful, crude humor.
Likes: Cock, teasing and getting a reaction.
Dislikes: People flirting with Alex, being ignored.
Flaws: No filter when talking about sex.
Goals: Make Alex bi-curious.
Occupation: Full-time livestreamer and content creator.
Backstory: Liam and Alex were online friends for several years.
  ]]></persona>

  <custom_tags>
    <entry key="scent"><![CDATA[strawberry and faint boymusk when aroused]]></entry>
    <entry key="penis_size"><![CDATA[7 inches]]></entry>
  </custom_tags>
</sim_card>

<world><![CDATA[
Date: March 15, present day
Time: 10:00 AM
Weather: clear and sunny
Tone: Smut, Romance, Conversion Kink, Comedy
]]></world>

<location>
</location>

<intro><![CDATA[
You stand at the front door of Liam's house.
]]></intro>

<inventory><![CDATA[
Clothing: Pink Oversized Hoodie, Gray Jeans, Girl Panties, White Thigh-High Socks
]]></inventory>"#;

    #[test]
    fn parse_extracts_all_fields() {
        let card = parse(SAMPLE).expect("sample parses");
        assert_eq!(card.id, "wupi");
        assert_eq!(card.name, "Wupi");
        assert_eq!(card.card_type, "system");
        assert_eq!(card.core_persona, "A cheerful catgirl.");
        assert!(card.traits.contains("Devoted to Master."));
        assert!(card.appearance.contains("race: Catgirl"));
        assert!(card.appearance.contains("cat_ears: Perky and expressive."));
        assert_eq!(card.role_instruction, "Help Master manage the system.");
        assert!(card.responsibilities.contains("Manage settings."));
        assert!(card.conversational_rules.contains("nya~"));
        assert!(card.technical_rules.is_empty());
        assert_eq!(card.introductions.len(), 2);
        assert!(card.introductions[0].contains("Hello Master"));
        assert!(card.introductions[1].contains("ฅ^>⩊<^ฅ"));
    }

    #[test]
    fn parse_strips_intro_bullet_prefix() {
        let card = parse(SAMPLE).expect("parses");
        for intro in &card.introductions {
            assert!(!intro.starts_with("- "), "intro kept its bullet: {intro}");
        }
    }
    #[test]
    fn parse_reads_canonical_intro_tag_alias() {
        let xml = r#"<sim_card><identity><name>X</name></identity></sim_card>
<intro><![CDATA[The fog rolls in over Aldermoor.]]></intro>"#;
        let card = parse(xml).expect("canonical <intro> parses");
        assert_eq!(card.intro, "The fog rolls in over Aldermoor.");
        assert_eq!(card.introductions, vec!["The fog rolls in over Aldermoor.".to_string()]);
    }

    #[test]
    fn parse_intro_absent_when_card_has_no_sibling() {
        let xml = r#"<sim_card>
  <metadata><type>simulation</type></metadata>
  <identity><name>Aldermoor</name></identity>
</sim_card>";
        let card = parse(xml).expect("parses");
        assert!(card.intro.is_empty());
        assert!(card.introductions.is_empty());
    }

    /// (2026-08-24 multi-line round-trip) The labeled-line grammar's `|`
    /// continuation marker: parse folds marker lines into the previous
    /// label newline-preserved (even colon-bearing ones), render emits
    /// them — a multi-paragraph value round-trips through the pair
    /// byte-faithful instead of truncating at its first colon-bearing
    /// line.
    #[test]
    fn labeled_lines_pipe_continuation_round_trips() {
        let rendered = labeled_line("Backstory", "One.\nTwo: with a colon.\n\nThree.");
        assert_eq!(rendered, "Backstory: One.\n| Two: with a colon.\n| \n| Three.\n");
        let parsed = parse_labeled_lines(&rendered);
        assert_eq!(parsed.len(), 1, "no forged second label: {parsed:?}");
        assert_eq!(parsed[0].0, "Backstory");
        assert_eq!(parsed[0].1, "One.\nTwo: with a colon.\n\nThree.");
        // The soft continuation (no marker, no colon) still space-joins.
        let soft = parse_labeled_lines("Goals: Survive\nand prosper");
        assert_eq!(soft[0].1, "Survive and prosper");
    }

    /// (2026-08-24 data-loss guard) A tail with one raw `&`/`<` outside
    /// CDATA is NOT well-formed XML — the card REFUSES to parse instead of
    /// loading with every sibling silently defaulted (the next serialize_v2
    /// rewrite would permanently delete them from disk).
    #[test]
    fn parse_refuses_malformed_sibling_tail() {
        let xml = "<sim_card>\n  <metadata><type>simulation</type><id>liam</id></metadata>\n\
                   <identity><![CDATA[\nName: Liam\n  ]]></identity>\n\
                   </sim_card>\n\n<world><![CDATA[\nDate: soon\n]]></world>\n\
                   <location>Market &amp; Main</location>\n<intro>Stray < raw tag</intro>";
        let err = parse(xml).expect_err("malformed tail must refuse");
        assert!(
            err.to_string().contains("not well-formed"),
            "error explains the refusal: {err}"
        );
        // The SAME card with the tail fixed (entity-escaped, no raw <)
        // parses fine and keeps its siblings — the guard is about
        // legality, not content.
        let fixed = "<sim_card>\n  <metadata><type>simulation</type><id>liam</id></metadata>\n\
                     <identity><![CDATA[\nName: Liam\n  ]]></identity>\n\
                     </sim_card>\n\n<world><![CDATA[\nDate: soon\n]]></world>\n\
                     <location>Market &amp; Main</location>";
        let card = parse(fixed).expect("well-formed tail parses");
        assert_eq!(card.location.as_deref(), Some("Market & Main"));
    }

    // ── (2026-08-22 Codex decoupling) linked_codices sibling ──────────────

    #[test]
    fn linked_codices_sibling_round_trips_through_serialize_v2() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>world</subtype>
  <id>aldermoor</id>
  </metadata>

  <identity><![CDATA[
Name: Aldermoor
  ]]></identity>
</sim_card>

<world><![CDATA[
Date: March 15
Tone: Grim
]]></world>

<linked_codices><![CDATA[
Aldermoor Lore
Forgotten Realms
]]></linked_codices>"#;
        let card = parse(xml).expect("linked_codices sibling parses");
        assert_eq!(card.linked_codices, vec!["Aldermoor Lore", "Forgotten Realms"]);
        let out = card.serialize_v2();
        assert!(out.contains("<linked_codices>"), "serialize_v2 re-emits the sibling");
        let back = parse(&out).expect("re-serialized card parses");
        assert_eq!(back.linked_codices, card.linked_codices);
        // Empty list → sibling omitted entirely.
        let mut bare = card.clone();
        bare.linked_codices.clear();
        assert!(!bare.serialize_v2().contains("<linked_codices>"));
    }

    #[test]
    fn parse_codex_name_lines_strips_suffix_and_dedupes() {
        let parsed = parse_codex_name_lines(&" Aldermoor Lore \nForgotten Realms.codex\n\naldermoor lore\n");
        assert_eq!(parsed, vec!["Aldermoor Lore", "Forgotten Realms"]);
    }
    #[test]
    fn with_linked_codices_rewrites_v2_card_through_serialize() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>world</subtype>
  <id>one-piece</id>
  </metadata>

  <identity><![CDATA[
Name: One Piece
  ]]></identity>
</sim_card>"#;
        let out = with_linked_codices(xml, &["One Piece Lore".to_owned()]).expect("v2 rewrite");
        let card = parse(&out).expect("rewritten card parses");
        assert!(card.format_v2);
        assert_eq!(card.linked_codices, vec!["One Piece Lore"]);
    }

    // (2026-08-22 intro variants) Multiple <intro> siblings parse in file
    // order (first mirrors into `intro`), serialize back one-per-variant, and
    // a legacy single-<intro> card is a one-element list — identical shape.
    #[test]
    fn intro_variants_parse_all_siblings_in_order() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>scenario</subtype>
  <id>variants</id>
  </metadata>

  <identity><![CDATA[
Name: Variants
  ]]></identity>
</sim_card>

<intro><![CDATA[
First opening.
]]></intro>
<intro><![CDATA[
Second opening.
]]></intro>
<intro><![CDATA[
Third opening.
]]></intro>"#;
        let card = parse(xml).expect("multi-intro card parses");
        assert_eq!(card.intro, "First opening.");
        assert_eq!(
            card.intro_variants,
            vec!["First opening.", "Second opening.", "Third opening."]
        );
        // Round-trip: serialize_v2 re-emits one <intro> per variant + the
        // re-parse sees the same list.
        let out = card.serialize_v2();
        let back = parse(&out).expect("re-serialized card parses");
        assert_eq!(back.intro_variants, card.intro_variants);
        assert_eq!(back.intro, card.intro);
    }

    #[test]
    fn single_intro_card_parses_to_one_variant_list() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>scenario</subtype>
  <id>single</id>
  </metadata>

  <identity><![CDATA[
Name: Single
  ]]></identity>
</sim_card>

<intro><![CDATA[
The only opening.
]]></intro>"#;
        let card = parse(xml).expect("single-intro card parses");
        assert_eq!(card.intro, "The only opening.");
        assert_eq!(card.intro_variants, vec!["The only opening."]);
        // No intro at all → both stay empty (the fallback card shape).
        let none = parse(
            r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>scenario</subtype>
  <id>none</id>
  </metadata>

  <identity><![CDATA[
Name: None
  ]]></identity>
</sim_card>"#,
        )
        .expect("intro-less card parses");
        assert!(none.intro.is_empty());
        assert!(none.intro_variants.is_empty());
    }

    // (2026-08-22 intro variants) The live-sync write core: replace at an
    // index, append on None/out-of-range, the mirror follows variant 0,
    // empty text errors, and an intro-less card gains its first variant.
    #[test]
    fn with_intro_variant_replaces_appends_and_mirrors() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>scenario</subtype>
  <id>live</id>
  </metadata>

  <identity><![CDATA[
Name: Live
  ]]></identity>
</sim_card>

<intro><![CDATA[
First.
]]></intro>
<intro><![CDATA[
Second.
]]></intro>"#;
        // Replace variant 1 (an in-game edit of the active alternate).
        let out = with_intro_variant(xml, Some(1), "Edited second.").expect("replace");
        let card = parse(&out).expect("re-parse");
        assert_eq!(card.intro_variants, vec!["First.", "Edited second."]);
        assert_eq!(card.intro, "First.", "mirror still follows variant 0");
        // Append (a new authored opening).
        let out2 = with_intro_variant(&out, None, "Third.").expect("append");
        let card2 = parse(&out2).expect("re-parse 2");
        assert_eq!(card2.intro_variants.len(), 3);
        assert_eq!(card2.intro_variants[2], "Third.");
        // Out-of-range index appends rather than erroring.
        let out3 = with_intro_variant(&out2, Some(9), "Fourth.").expect("oob appends");
        let card3 = parse(&out3).expect("re-parse 3");
        assert_eq!(card3.intro_variants.len(), 4);
        // Editing variant 0 moves the mirror.
        let out4 = with_intro_variant(&out3, Some(0), "New first.").expect("replace 0");
        let card4 = parse(&out4).expect("re-parse 4");
        assert_eq!(card4.intro, "New first.");
        assert_eq!(card4.intro_variants[0], "New first.");
        // Blank text is refused.
        assert!(with_intro_variant(xml, None, "   ").is_err());
    }

    #[test]
    fn with_intro_variant_on_introless_card_creates_first() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>scenario</subtype>
  <id>fresh</id>
  </metadata>

  <identity><![CDATA[
Name: Fresh
  ]]></identity>
</sim_card>"#;
        let out = with_intro_variant(xml, None, "The first opening.").expect("append creates");
        let card = parse(&out).expect("re-parse");
        assert_eq!(card.intro_variants, vec!["The first opening."]);
        assert_eq!(card.intro, "The first opening.");
    }

    #[test]
    fn render_for_prompt_emits_tagged_sections() {
        let card = parse(SAMPLE).expect("parses");
        let rendered = card.render_for_prompt();
        assert!(rendered.starts_with("<persona>"));
        assert!(rendered.contains("<identity>"));
        assert!(rendered.contains("name: Wupi"));
        assert!(rendered.contains("<appearance>"));
        assert!(rendered.contains("<role>"));
        assert!(rendered.contains("<conversational_style>"));
        assert!(!rendered.contains("<technical_protocols>"));
        assert!(!rendered.contains("Hello Master"));
    }

    #[test]
    fn technical_protocols_block_still_renders_for_back_compat() {
        let card_xml = r#"<?xml version="1.0"?>
<sim_card>
  <identity>
    <name>Legacy</name>
    <persona>A card from before the protocol extraction.</persona>
    <traits><![CDATA[ - Old-school. ]]></traits>
  </identity>
  <technical_protocols>
    <rules><![CDATA[
- Legacy rule that still works.
    ]]></rules>
  </technical_protocols>
</sim_card>"#;
        let card = parse(card_xml).expect("legacy card with technical_protocols parses");
        assert!(card.technical_rules.contains("Legacy rule that still works."));
        let rendered = card.render_for_prompt();
        assert!(rendered.contains("<technical_protocols>"));
        assert!(rendered.contains("Legacy rule that still works."));
    }

    #[test]
    fn random_intro_returns_none_when_empty() {
        let card = fallback();
        assert!(card.random_intro().is_none());
    }

    #[test]
    fn random_intro_picks_from_list() {
        let card = parse(SAMPLE).expect("parses");
        let pick = card.random_intro().expect("non-empty list yields a pick");
        assert!(card.introductions.iter().any(|i| i == pick));
    }

    #[test]
    fn fallback_card_renders_empty() {
        let card = fallback();
        assert_eq!(card.render_for_prompt(), "");
        assert!(card.random_intro().is_none());
    }

    #[test]
    fn parse_rejects_wrong_root() {
        let bad = "<not_a_sim_card><id>x</id></not_a_sim_card>";
        assert!(parse(bad).is_err());
    }

    #[test]
    fn parse_derives_id_from_name_when_no_metadata() {
        let no_meta = r#"<sim_card>
  <identity>
    <name>Wupi</name>
    <persona>A catgirl.</persona>
  </identity>
</sim_card>"#;
        let card = parse(no_meta).expect("metadata-free card parses");
        assert_eq!(card.name, "Wupi");
        assert_eq!(card.id, "wupi");
        assert_eq!(card.card_type, "system");
    }

    #[test]
    fn parse_rejects_sentinel_id_from_name_branch() {
        let sentinel_named = r#"<sim_card>
  <metadata><type>simulation</type></metadata>
  <identity>
    <name>__wupi__</name>
    <persona>An impostor card.</persona>
  </identity>
</sim_card>"#;
        let card = parse(sentinel_named).expect("sentinel-named card still parses");
        assert_eq!(card.name, "__wupi__");
        assert_ne!(card.id, crate::memory::WUPI_CARD_ID);
        assert_eq!(card.id, "unknown");

        for sentinel in [
            crate::memory::WUPI_CARD_ID,
            crate::memory::WUPI_SYSTEM_CARD_ID,
            crate::memory::FABLE_SYSTEM_CARD_ID,
            crate::memory::CODEX_CARD_ID,
        ] {
            let xml = format!(
                "<sim_card><identity><name>{sentinel}</name><persona>x</persona></identity></sim_card>"
            );
            let card = parse(&xml).expect("parses");
            assert_ne!(card.id, sentinel, "name branch must not mint a sentinel id");
        }
    }
    #[test]
    fn system_card_has_no_scenario_fields() {
        let card = parse(SAMPLE).expect("system card parses");
        assert_eq!(card.card_type, "system");
        assert!(card.setting.is_none());
        assert!(card.player_name.is_none());
        assert!(card.world.is_empty());
        assert!(card.location.is_none());
        assert!(card.inventory.is_empty());
    }

    #[test]
    fn simcard_serializes_to_json_roundtrip() {
        let original = SimCard {
            id: "qp_test".into(),
            name: "Test Sim".into(),
            card_type: "simulation".into(),
            subtype: Some("npc".into()),
            format_v2: true,
            identity: CardIdentity { gender: Some("Male".into()), ..Default::default() },
            persona: CardPersona::default(),
            world: CardWorld::default(),
            location: None,
            inventory: CardInventory::default(),
            properties: Vec::new(),
            linked_codices: Vec::new(),
            core_persona: "cp".into(),
            traits: "t".into(),
            appearance: "a".into(),
            role_instruction: "ri".into(),
            responsibilities: "r".into(),
            conversational_rules: "cr".into(),
            technical_rules: "tr".into(),
            introductions: vec!["hi".into()],
            intro: String::new(),
            intro_variants: Vec::new(),
            setting: Some("A test place.".into()),
            plot: None,
            player_name: Some("Kaelen".into()),
            custom_tags: BTreeMap::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: SimCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, original.id);
        assert_eq!(back.name, original.name);
        assert_eq!(back.card_type, "simulation");
        assert_eq!(back.format_v2, true);
        assert_eq!(back.identity.gender, original.identity.gender);
        assert_eq!(back.setting, original.setting);
        assert_eq!(back.player_name, original.player_name);
        assert_eq!(back.introductions, original.introductions);
    }
    #[test]
    fn parses_subtype_date_and_custom_tags() {
        let xml = r#"<sim_card>
  <metadata><type>simulation</type><subtype>npc</subtype></metadata>
  <identity><![CDATA[
Name: Mara
  ]]></identity>
  <custom_tags>
    <entry key="faction">thieves guild</entry>
    <entry key="bounty">500 gold</entry>
  </custom_tags>
</sim_card>

<world><![CDATA[
Date: 3rd of Harvest, Year 1247
Time: Day 1, 09:00
Weather: clear
]]></world>"#;
        let card = parse_from_xml_str(xml).expect("parses");
        assert_eq!(card.card_type, "simulation");
        assert!(card.format_v2);
        assert_eq!(card.subtype.as_deref(), Some("npc"));
        assert_eq!(card.world.date.as_deref(), Some("3rd of Harvest, Year 1247"));
        assert_eq!(card.world.weather.as_deref(), Some("clear"));
        assert_eq!(card.world.time.as_deref(), Some("Day 1, 09:00"));
        assert_eq!(
            card.custom_tags.get("faction").map(|s| s.as_str()),
            Some("thieves guild")
        );
        let json = serde_json::to_string(&card).expect("serialize");
        let back: SimCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.subtype.as_deref(), Some("npc"));
        assert_eq!(back.world.date.as_deref(), Some("3rd of Harvest, Year 1247"));
        assert_eq!(back.custom_tags.get("faction").map(|s| s.as_str()), Some("thieves guild"));
    }
    #[test]
    fn simcard_deserialize_partial_json_fills_defaults() {
        let partial = r#"{"id":"x","name":"X","card_type":"simulation","core_persona":"","traits":"","appearance":"","role_instruction":"","responsibilities":"","conversational_rules":"","technical_rules":""}"#;
        let card: SimCard = serde_json::from_str(partial).expect("partial JSON loads");
        assert_eq!(card.id, "x");
        assert!(card.setting.is_none());
        assert!(card.format_v2 == false);
        assert!(card.identity.is_empty());
        assert!(card.persona.is_empty());
        assert!(card.world.is_empty());
        assert!(card.inventory.is_empty());
    }
    // ── v2 format tests ───────────────────────────────────────────────────

    #[test]
    fn v2_npc_card_parses_all_blocks() {
        let card = parse(V2_NPC).expect("v2 npc card parses");
        assert!(card.format_v2, "the liam-shaped layout must parse as v2");
        assert_eq!(card.card_type, "simulation");
        assert_eq!(card.subtype.as_deref(), Some("npc"));
        assert_eq!(card.id, "liam");
        assert_eq!(card.name, "Liam");
        // Identity traits.
        assert_eq!(card.identity.gender.as_deref(), Some("Male"));
        assert_eq!(card.identity.race.as_deref(), Some("Human"));
        assert_eq!(card.identity.age.as_deref(), Some("21"));
        assert_eq!(card.identity.height.as_deref(), Some("5'10\""));
        assert_eq!(card.identity.body.as_deref(), Some("Petite and hairless"));
        assert_eq!(card.identity.skin.as_deref(), Some("Pale"));
        assert_eq!(card.identity.eyes.as_deref(), Some("Pink"));
        assert_eq!(card.identity.hair_color.as_deref(), Some("Pink"));
        assert_eq!(card.identity.hair_length.as_deref(), Some("Shoulder-length"));
        assert_eq!(card.identity.hair_style.as_deref(), Some("Styled neatly"));
        // Persona fields.
        assert_eq!(
            card.persona.personality.as_deref(),
            Some("Mischievous and a shameless flirt.")
        );
        assert_eq!(
            card.persona.conversation_style.as_deref(),
            Some("Semi-innocent tone with playful, crude humor.")
        );
        assert_eq!(card.persona.goals.as_deref(), Some("Make Alex bi-curious."));
        // World anchors.
        assert_eq!(card.world.date.as_deref(), Some("March 15, present day"));
        assert_eq!(card.world.time.as_deref(), Some("10:00 AM"));
        assert_eq!(card.world.weather.as_deref(), Some("clear and sunny"));
        assert_eq!(
            card.world.tone.as_deref(),
            Some("Smut, Romance, Conversion Kink, Comedy")
        );
        assert_eq!(card.effective_tone(), Some("Smut, Romance, Conversion Kink, Comedy"));
        // Intro + inventory.
        assert!(card.intro.contains("front door of Liam's house"));
        assert_eq!(
            card.inventory.clothing,
            vec![
                "Pink Oversized Hoodie".to_string(),
                "Gray Jeans".to_string(),
                "Girl Panties".to_string(),
                "White Thigh-High Socks".to_string()
            ]
        );
        // Custom tags.
        assert_eq!(
            card.custom_tags.get("penis_size").map(|s| s.as_str()),
            Some("7 inches")
        );
    }

    #[test]
    fn v2_cache_block_renders_everything_within_sim_card() {
        let card = parse(V2_NPC).expect("parses");
        let block = card.render_cache_block();
        assert!(block.starts_with("<sim_card>\n"));
        assert!(block.ends_with("</sim_card>"));
        // Identity lines render verbatim.
        assert!(block.contains("Name: Liam\n"));
        assert!(block.contains("Hair Color: Pink\n"));
        // Persona lines render.
        assert!(block.contains("Personality: Mischievous and a shameless flirt.\n"));
        assert!(block.contains("Conversation Style: Semi-innocent"));
        // Custom tags render as key: value.
        assert!(block.contains("penis_size: 7 inches\n"));
        // The mutable siblings NEVER enter the cache block.
        assert!(!block.contains("March 15"));
        assert!(!block.contains("clear and sunny"));
        assert!(!block.contains("front door"));
        assert!(!block.contains("Pink Oversized Hoodie"));
    }

    #[test]
    fn v2_serialize_round_trips() {
        let card = parse(V2_NPC).expect("parses");
        let xml = card.serialize_v2();
        let back = parse(&xml).expect("re-serialized v2 card parses");
        assert!(back.format_v2);
        assert_eq!(back.id, card.id);
        assert_eq!(back.name, card.name);
        assert_eq!(back.identity.hair_color, card.identity.hair_color);
        assert_eq!(back.persona.personality, card.persona.personality);
        assert_eq!(back.persona.conversation_style, card.persona.conversation_style);
        assert_eq!(back.world.date, card.world.date);
        assert_eq!(back.world.tone, card.world.tone);
        assert_eq!(back.intro, card.intro);
        assert_eq!(back.inventory.clothing, card.inventory.clothing);
        assert_eq!(back.custom_tags, card.custom_tags);
    }

    #[test]
    fn v2_serialize_layout_matches_reference_shape() {
        let card = parse(V2_NPC).expect("parses");
        let xml = card.serialize_v2();
        // Metadata block shape.
        assert!(xml.contains("  <metadata>\n  <type>simulation</type>\n  <subtype>npc</subtype>\n  <id>liam</id>\n  </metadata>"));
        // Sibling order: world → location → intro → inventory.
        let world = xml.find("<world>").expect("world sibling");
        let location = xml.find("<location>").expect("location sibling");
        let intro = xml.find("<intro>").expect("intro sibling");
        let inventory = xml.find("<inventory>").expect("inventory sibling");
        assert!(world < location && location < intro && intro < inventory);
        // Inventory only carries non-empty lines.
        assert!(xml.contains("Clothing: Pink Oversized Hoodie, Gray Jeans"));
        assert!(!xml.contains("Equipped:"));
        assert!(!xml.contains("Stored:"));
    }

    #[test]
    fn v2_empty_blocks_omitted_from_serialization() {
        let card = SimCard {
            id: "blank".into(),
            name: "Blank".into(),
            card_type: "simulation".into(),
            subtype: Some("npc".into()),
            format_v2: true,
            ..fallback_fields()
        };
        let xml = card.serialize_v2();
        assert!(!xml.contains("<persona>"), "empty persona omitted");
        assert!(!xml.contains("<world>"), "empty world omitted");
        assert!(!xml.contains("<location>"), "empty location omitted");
        assert!(!xml.contains("<intro>"), "empty intro omitted");
        assert!(!xml.contains("<inventory>"), "empty inventory omitted");
        assert!(!xml.contains("<custom_tags>"), "empty tags omitted");
        // The file still parses.
        let back = parse(&xml).expect("minimal v2 card parses");
        assert_eq!(back.name, "Blank");
        assert!(back.persona.is_empty());
    }

    /// Helper: a fallback-shaped field set for tests building SimCards by
    /// struct literal (avoids repeating 20 field inits).
    fn fallback_fields() -> SimCard {
        let mut c = fallback();
        c.id = String::new();
        c.name = String::new();
        c.card_type = String::new();
        c
    }

    #[test]
    fn v2_world_card_keeps_dedicated_fields() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>world</subtype>
  <id>one-piece</id>
  </metadata>

  <identity><![CDATA[
Name: One Piece
  ]]></identity>

  <setting><![CDATA[
A pirate world of endless seas.
  ]]></setting>
</sim_card>

<world><![CDATA[
Date: Year 1522
Time: 09:00 AM
Weather: clear
Tone: Adventure
]]></world>

<location><![CDATA[Foosha Village]]></location>"#;
        let card = parse(xml).expect("v2 world card parses");
        assert!(card.format_v2);
        assert_eq!(card.subtype.as_deref(), Some("world"));
        assert_eq!(card.name, "One Piece");
        assert!(card.setting.as_deref().unwrap().contains("endless seas"));
        assert_eq!(card.location.as_deref(), Some("Foosha Village"));
        // Cache block carries Name + Setting, never the siblings.
        let block = card.render_cache_block();
        assert!(block.contains("Name: One Piece"));
        assert!(block.contains("Setting:\nA pirate world"));
        assert!(!block.contains("Foosha"));
        assert!(!block.contains("Adventure"));
    }
    #[test]
    fn v2_unlabeled_persona_lines_kept_as_extra() {
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>npc</subtype>
  <id>extra</id>
  </metadata>
  <identity><![CDATA[
Name: Extra
  ]]></identity>
  <persona><![CDATA[
Personality: Quiet.
Alignment: Chaotic Good
  ]]></persona>
</sim_card>"#;
        let card = parse(xml).expect("parses");
        assert_eq!(card.persona.personality.as_deref(), Some("Quiet."));
        assert_eq!(
            card.persona.extra,
            vec![("Alignment".to_string(), "Chaotic Good".to_string())]
        );
        // The extra line rides the cache block + survives re-serialization.
        assert!(card.render_cache_block().contains("Alignment: Chaotic Good"));
        let back = parse(&card.serialize_v2()).expect("round-trips");
        assert_eq!(back.persona.extra, card.persona.extra);
    }

    #[test]
    fn v2_identity_extra_lines_ride_cache_and_roundtrip() {
        // 2026-08-20 audit H1: identity extras (hand-authored lines like
        // "Alignment:") must ride the cache block AND survive re-serialization.
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>npc</subtype>
  <id>ida</id>
  </metadata>
  <identity><![CDATA[
Name: Ida
Alignment: Chaotic Good
  ]]></identity>
</sim_card>"#;
        let card = parse(xml).expect("parses");
        assert_eq!(
            card.identity.extra,
            vec![("Alignment".to_string(), "Chaotic Good".to_string())]
        );
        assert!(card.render_cache_block().contains("Alignment: Chaotic Good"));
        let back = parse(&card.serialize_v2()).expect("round-trips");
        assert_eq!(back.identity.extra, card.identity.extra);
        assert!(back.serialize_v2().contains("Alignment: Chaotic Good"));
    }

    #[test]
    fn v2_properties_sibling_roundtrip() {
        // (2026-08-20 Economy) The authored <properties> sibling parses on
        // v2 cards and survives serialize_v2 byte-faithfully (all subtypes).
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>npc</subtype>
  <id>bors</id>
  </metadata>
  <identity><![CDATA[
Name: Bors
  ]]></identity>
</sim_card>

<properties><![CDATA[
id: forge | node: iron-forge | kind: business | revenue: 8 | upkeep: 3
id: manor | node: hill | kind: estate | owner: bors | revenue: 2 | upkeep: 9 | price: 250
]]></properties>"#;
        let card = parse(xml).expect("parses");
        assert_eq!(card.properties.len(), 2);
        assert_eq!(card.properties[0].id, "forge");
        assert_eq!(card.properties[0].node, "iron-forge");
        assert!(card.properties[0].owner.is_none());
        assert_eq!(card.properties[1].owner.as_deref(), Some("bors"));
        assert_eq!(card.properties[1].price, Some(250));
        let out = card.serialize_v2();
        assert!(out.contains("<properties>"), "emitted: {out}");
        let back = parse(&out).expect("round-trips");
        assert_eq!(back.properties, card.properties);
        // A card with NO properties emits NO sibling.
        let bare = parse(r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>npc</subtype>
  <id>bors</id>
  </metadata>
  <identity><![CDATA[
Name: Bors
  ]]></identity>
</sim_card>"#)
        .expect("parses");
        assert!(bare.properties.is_empty());
        assert!(!bare.serialize_v2().contains("<properties>"));
    }

    #[test]
    fn v2_serialize_splits_cdata_terminators_in_every_body() {
        // 2026-08-20 audit M5: an authored `]]>` in ANY serialized body must
        // not close the CDATA section early (identity/persona/setting/plot/
        // world/inventory all ride the cdata_body wrap now).
        let xml = r#"<sim_card>
  <metadata>
  <type>simulation</type>
  <subtype>npc</subtype>
  <id>cd</id>
  </metadata>
  <identity><![CDATA[
Name: Cd
  ]]></identity>
  <persona><![CDATA[
Personality: quiet
  ]]></persona>
</sim_card>"#;
        let mut card = parse(xml).expect("parses");
        card.persona.personality = Some("wears a ]]> grin".into());
        card.identity.extra.push(("Motto".into(), "never say ]]>".into()));
        let out = card.serialize_v2();
        assert!(out.contains("]]]]><![CDATA[>"), "terminators must be split: {out}");
        let back = parse(&out).expect("re-parses through the split");
        assert_eq!(back.persona.personality.as_deref(), Some("wears a ]]> grin"));
        assert_eq!(
            back.identity.extra,
            vec![("Motto".to_string(), "never say ]]>".to_string())]
        );
    }
    #[test]
    fn normalize_label_handles_spacing_and_drift() {
        assert_eq!(normalize_label("Hair Color").as_deref(), Some("hair_color"));
        assert_eq!(normalize_label("hair color").as_deref(), Some("hair_color"));
        assert_eq!(normalize_label("hair_color").as_deref(), Some("hair_color"));
        assert_eq!(normalize_label("Equipped").as_deref(), Some("equipped"));
        assert_eq!(normalize_label("Job").as_deref(), Some("occupation"));
        assert_eq!(normalize_label("Goal").as_deref(), Some("goals"));
        assert_eq!(normalize_label("Dialogue Style").as_deref(), Some("conversation_style"));
        assert_eq!(normalize_label("Conversation Style").as_deref(), Some("conversation_style"));
        assert_eq!(normalize_label("!!").as_deref(), None);
    }
}
