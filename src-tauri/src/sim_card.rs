//! Simulation Card (`.sim`) loader, parser, and renderer.
//!
//! A Simulation Card is the persona artifact for a WUPI entity: Wupi's
//! own card (the interface persona) or, later, a roleplay scenario card.
//! Each card carries its own identity, appearance, role, conversational style,
//! and an introduction list used for the randomized boot greeting.
//!
//! The card is strict XML with CDATA-wrapped prose blocks (so emoticons,
//! quotes, and any literal `<>` in the persona text parse cleanly). We parse
//! it once at startup with `roxmltree` (a tiny DOM parser that auto-merges
//! CDATA into text nodes: zero special handling), render the persona into a
//! `<persona>` block for the system prompt, and expose a randomized intro for
//! the boot UI flourish.
//!
//! Design contract (mirrors the embedder's graceful-degradation pattern in
//! §2M): if the card file is missing or malformed, `load_or_fallback` returns
//! a minimal stub persona so the app still boots. The persona is best-effort;
//! a bad card must never kill the OS.

use std::collections::BTreeMap;
use std::path::Path;

use rand::seq::IndexedRandom;

/// A location node as authored in a `.sim` card's `<scenario><locations>`
/// block (Fable Phase 4 Component 3, 2026-07-28). Converted to
/// [`crate::schema::Node`] by `enter_fable_session` at `fable_start`. Kept
/// in `sim_card` (not reusing `schema::Node` directly) so the card parser
/// stays decoupled from the schema module — the conversion is a one-liner
/// at the seed site.
///
/// Parsed from:
/// ```xml
/// <node id="tavern" setting="indoor">
///   <name>The Rusty Lantern Tavern</name>
///   <neighbor>cellar</neighbor>
///   <neighbor>market_square</neighbor>
/// </node>
/// ```
/// Edges are undirected in concept but each side must list the other — the
/// parser does NOT symmetrize (matches the [`crate::schema::TravelGraph`]
/// contract: `neighbors` is the source of truth).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CardNode {
    /// Bare slug ("tavern", "cellar"). NOT "node.tavern" — the `node.`
    /// prefix is a narrator convention only; the `[TRAVEL]` parser strips
    /// it for ergonomics.
    pub id: String,
    /// Diegetic prose label shown to the narrator ("The Rusty Anchor
    /// tavern"). This is the prose label only; flavor prose about the node
    /// lives in `entities`.
    pub name: String,
    /// Reachable neighbor node ids (bare slugs). Pure adjacency — no
    /// weights, no terrain (anti-bloat).
    pub neighbors: Vec<String>,
    /// Free hint, lowercased + matched against a tiny known set ("indoor" /
    /// "outdoor" / empty). Gates whether the global `weather:` line renders
    /// for this node (the only node→weather coupling in v1).
    pub setting: String,
}

/// A named NPC as authored in a `.sim` card's `<scenario><cast>` block
/// (Fable Phase 5A, 2026-07-29). Converted to
/// [`crate::schema::NpcEntry`] by `enter_fable_session` at `fable_start`.
/// Kept in `sim_card` (not reusing `schema::NpcEntry` directly) so the card
/// parser stays decoupled from the schema module — the conversion is a
/// one-liner at the seed site (mirrors the `CardNode` precedent).
///
/// Parsed from:
/// ```xml
/// <npc id="mara_the_innkeep" tier="soldier">
///   <name>Mara</name>
///   <role>The innkeeper behind the bar</role>
///   <alias>mara</alias>
///   <alias>innkeep</alias>
/// </npc>
/// ```
///
/// The `id` is the load-bearing field: it is the Rust-owned authoritative key
/// the `[PRESENCE]` bracket validates against (the anti-hallucination gate).
/// `name` is the diegetic prose label shown to the narrator; `role` is a
/// one-line vocation/hook; `tier` is forward-compat (feeds the
/// `select_attacker_tier_from_entities` heuristic later — left optional for
/// Phase 5A, where the registry's job is the ID whitelist + name only);
/// `aliases` are alternate surface forms the narrator may emit that the
/// presence applier normalizes back to `id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CardNpc {
    /// Bare slug ("mara_the_innkeep"). MUST match the ids in `<start_npcs>`
    /// so the existing narrator-prompt seeding (sim_card.rs start_npc_ids)
    /// and the new registry agree on canonical keys.
    pub id: String,
    /// Diegetic prose label shown to the narrator ("Mara"). This is the prose
    /// label only; personality/appearance prose lives in the card's own CDATA
    /// blocks (authored by the user).
    pub name: String,
    /// One-line vocation/role hint ("The innkeeper behind the bar"). Optional
    /// flavor; helps the narrator + the image-gen prompt composer.
    pub role: String,
    /// Optional combat tier label ("soldier" / "elite" / "boss" / ...).
    /// Forward-compat for the §11.30 tier heuristic; `None` for non-combat
    /// NPCs (civilians, vendors, atmosphere characters).
    #[serde(default)]
    pub tier: Option<String>,
    /// Alternate surface forms the narrator may emit ("mara", "innkeep").
    /// The presence applier normalizes any alias back to `id` so the
    /// `[PRESENCE mara "..."]` and `[PRESENCE mara_the_innkeep "..."]` forms
    /// both resolve to the same registry entry.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// The starting world-state anchors authored in a `.sim` card's `<start>`
/// block (2026-08-10, Issue #2 — the cold-start anchor gap). A fresh game's
/// `WorldClock` + `Weather` are dormant (zero/unset) until the first
/// `[TIME]`/`[WEATHER]` bracket lands — but the tracker renders no `clock:`/
/// `weather:` line while dormant, so it has nothing to maintain, and the
/// brackets never fire. The `<start>` block lets a card author seed the
/// initial time + weather so both lines render from turn 1, giving the
/// tracker the anchors it needs to advance/change them as the scene moves.
///
/// Parsed from (both children optional; flat-first, `<scenario>` back-compat):
/// ```xml
/// <start>
///   <time>Day 1, 08:00</time>
///   <weather>thick fog off the marsh</weather>
/// </start>
/// ```
/// `time` is a free-form in-world time string parsed by
/// `bracket_parser::parse_in_world_time` (the same parser `[TIME]` uses —
/// accepts "Day N, HH:MM", 12h with AM/PM, calendar dates). `None` when
/// absent or unparseable → the clock stays dormant (pre-start-block behavior).
/// `weather` is a free-form diegetic condition phrase seeded verbatim into
/// `Weather.condition`. `None` when absent → weather stays dormant.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct CardStart {
    /// In-world minutes since 0001-01-01, the resolved form of the authored
    /// `<time>` string. `None` when no `<time>` was authored or it didn't
    /// parse → the clock is NOT seeded (stays dormant, pre-start behavior).
    /// `fable_start` writes this directly into `WorldClock.current_minutes`
    /// AND `last_tick_minutes` (so the seed counts as the baseline — no
    /// immediate World Progression tick fires on turn 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_minutes: Option<i64>,
    /// Diegetic condition phrase ("thick fog off the marsh"). `None` when no
    /// `<weather>` was authored → weather is NOT seeded. `fable_start` writes
    /// this into `Weather.condition` + stamps `started_at_minutes` to the
    /// seeded clock (or 0 when the clock is also unseeded — the dormant
    /// baseline, harmless).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weather: Option<String>,
    /// Free-form calendar label (2026-08-13): the verbatim `<date>` string —
    /// month/year/type-of-day ("3rd of Harvest, Year 1247, Market Day"), NOT
    /// just "Day 1". Seeded into `WorldSchema.calendar` + rendered as `date:`;
    /// advanced in play by the `[DATE]` bracket (the tracker rewrites the
    /// label — no Rust calendar arithmetic). `None` when absent → the engine
    /// falls back to the legacy "Day N, HH:MM" clock render.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

/// One Simulation Card, parsed from a `.sim` file. Owned and immutable for the
/// process lifetime after `setup()` loads it.
///
/// `Serialize` + `Deserialize` (so a card can round-trip through JSON) — every
/// field is a primitive
/// `String`/`Option<String>`/`Vec<String>`, so serde handles the round trip
/// with no custom impl. `#[serde(default)]` on the `Option` fields keeps older
/// save JSON (written before a field existed) loading cleanly.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimCard {
    pub id: String,
    pub name: String,
    /// `"system"` for the OS interface persona (Wupi), `"roleplay"` for
    /// future scenario cards. Drives behavior upstream (e.g. whether the card
    /// owns a resumable session + schema).
    pub card_type: String,
    /// The polymorphic SIM Wizard discriminator (2026-08-13): `"npc"` |
    /// `"scenario"` | `"world"` | None. `<type>` STAYS `"roleplay"` for every
    /// playable card (so the `fable_cards_list` + `find_card_by_id` filters
    /// are unchanged — non-breaking); `<subtype>` carries the wizard's Type
    /// Router choice for serializer/review routing + a future picker badge.
    /// None on pre-2026-08-13 cards (the distinction was purely implicit in
    /// which optional fields the author filled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    pub core_persona: String,
    pub traits: String,
    pub appearance: String,
    pub role_instruction: String,
    pub responsibilities: String,
    pub conversational_rules: String,
    /// Technical/output protocols carried by the card. **Deprecated as a
    /// persona mechanism (2026-07-29):** technical protocols (tool-call
    /// formats, file output, bracket syntax, state tracking) are now
    /// Rust-injected per pass — see `prompts::WUPI_AGENT_PROTOCOL` (the chat
    /// agent pass) and the narrator/tracker/scribe prompt builders on the
    /// Fable side. `.sim` cards are now PURE FLAVOR (identity, voice,
    /// personality, tone, world state).
    ///
    /// This field is retained as a **dormant back-compat shim**: it renders
    /// NOTHING when empty (see `render_for_prompt`), so the shipped `wupi.sim`
    /// card leaves it unset → zero tokens + zero behavior
    /// change. A user-authored `.sim` card that still includes a
    /// `<technical_protocols>` block parses and renders it unchanged (graceful
    /// migration; the field is not ripped to avoid a wide blast radius +
    /// breakage of existing user cards). Do NOT add new shipped content here.
    pub technical_rules: String,
    /// One greeting string per line in `<introductions>`. Empty if the card
    /// omits the block. Used by [`random_intro`] for the boot flourish.
    #[serde(default)]
    pub introductions: Vec<String>,
    /// The card's intro text — a SIBLING `<intro>` (canonical) or
    /// `<introduction>` (legacy alias) element AFTER `</sim_card>`, kept OUT of
    /// `<sim_card>` so it never inflates the cached system prompt (prime
    /// directive). For roleplay cards this is the Fable opening narrator beat
    /// (surfaced on `FableLoadResult.intro`); for `wupi.sim` it's the multi-line
    /// boot-greeting block (each line a `random_intro` pick). 2026-08-13: the
    /// prior parser only read `<introductions>` (plural) INSIDE `<sim_card>`, so
    /// `wupi.sim`'s sibling `<introduction>` was silently dropped — now read.
    /// Empty when the card carries neither form.
    #[serde(default)]
    pub intro: String,
    // All `None` / empty for the system card (Wupi). A roleplay scenario card
    // carries a `<scenario>` block that populates these. The parser already
    // handles optional elements via `nested_text` returning `None` for absent
    // parents, so adding fields here is non-breaking: `Wupi.sim` parses as
    // before with every field below at its default.
    //
    // All `#[serde(default)]` so a Quick-Play card JSON built by the model
    // (which may omit any of these) deserializes with the missing fields at
    // their `None`/empty defaults instead of failing the load.
    /// The world/setting premise. Injected into the narrator's system prompt
    /// as the ground-truth scenario context. `None` for system cards.
    ///
    /// Parsed flat-first (a top-level `<setting>` child of `<sim_card>`) with
    /// a fallback to the legacy `<scenario><setting>` wrapper so cards authored
    /// before the flat-format reorg still load. The flat shape is canonical
    /// (2026-08-01); the scenario-wrapper fallback is back-compat only.
    #[serde(default)]
    pub setting: Option<String>,
    /// Narrative consequence philosophy — authored prose directing how the
    /// world moves + how conflict resolves ("drive story through consequence,
    /// embed clues, let pressure gather"). Sibling to [`setting`]/[`tone`]:
    /// a flat top-level `<plot>` child. `None` when the card omits it.
    ///
    /// Added 2026-08-01 (the flat-format reorg surfaced `<plot>` as a
    /// first-class card field; previously it lived un-parsed inside setting
    /// prose). The Creator's World/Scenario tab exposes it.
    #[serde(default)]
    pub plot: Option<String>,
    /// Narrative tone directive ("grim, atmospheric, slow-burn"). Guides the
    /// narrator's voice. `None` for system cards. Flat-first parse with a
    /// `<scenario><tone>` fallback (mirrors [`setting`]).
    #[serde(default)]
    pub tone: Option<String>,
    /// Seed text for the first narrator turn (the opening scene). The
    /// FableEngine uses this to prime the first generation if the conversation
    /// is empty. `None` for system cards.
    ///
    /// **REMOVED 2026-08-05:** the intro now lives in a SIBLING `.intro` file
    /// (`cards/<id>/<id>.intro`), NOT inside the cached `<sim_card>`. The
    /// intro is a one-shot first-turn seed — baking it into the system prompt
    /// would inflate every turn's KV cache with text only relevant to turn 1
    /// (a prime-directive violation). The `.intro` is read ONCE at game start
    /// + surfaced on `FableLoadResult.intro` (not on `SimCard`). The field is
    /// gone from the struct; the parser no longer reads `<opening_scene>` (a
    /// clean break per Chloe 2026-08-05 — old user cards lose their intro
    /// silently, which is the accepted cost; the shipped cards were migrated).
    /// Stable NPC ids present at scene start. Used by the Phase 2 NPC runtime
    /// to spawn the initial cast. Empty for system cards.
    #[serde(default)]
    pub start_npc_ids: Vec<String>,
    /// Activities this card activates (e.g. `["combat","crafting"]`). Phase
    /// 2+ hint: the engine registry will match these against available
    /// activity modules. Empty for system cards.
    #[serde(default)]
    pub declared_activities: Vec<String>,
    /// The player's chosen name for roleplay cards (e.g. "Alex", "Kaelen").
    /// Used by the narrator prompt's `<active_reality>` tail block (Phase E,
    /// 2026-07-18) to anchor the model in the current card's identity and
    /// prevent cross-card KV-cache contamination (the "Alex hallucination"
    /// where one card's narrator used another card's player name).
    /// `None` for system cards (defaults to "User"). The XML tag is
    /// `<player_name>` (legacy saves using `<protagonist>` are auto-migrated
    /// on load — see `parse`).
    #[serde(default)]
    pub player_name: Option<String>,
    /// Fable Phase 4 Component 3 (2026-07-28): the spatial travel graph
    /// seeded from the card's `<scenario><locations>` block. Empty for
    /// system cards (Wupi) and for roleplay cards that omit the block
    /// (stays dormant — the pre-Phase-4 behavior; `travel_graph.nodes`
    /// empty, no `location:` line, `[TRAVEL]`/`[RUMOR]` bracket commands
    /// have nowhere to root). When non-empty, `enter_fable_session` seeds
    /// [`crate::schema::WorldSchema::travel_graph`] from this; the first
    /// node in document order becomes `current_node` (the player's
    /// starting location). See `docs/phase4-fix-travel-graph-seeding.md`
    /// for the full rationale (this field is the load-bearing fix for
    /// Components 3 + 4 being dead in live play).
    ///
    /// `Vec<CardNode>` (not `schema::TravelGraph` directly) to keep
    /// `sim_card` free of the `schema` module dependency — `fable_start`
    /// does the one-line conversion. `#[serde(default)]` keeps older
    /// card JSON (written before this field existed)
    /// loading cleanly.
    #[serde(default)]
    pub locations: Vec<CardNode>,
    /// Fable Phase 5A (2026-07-29): the named NPC registry seeded from the
    /// card's `<scenario><cast>` block. Empty for system cards (Wupi) and for
    /// roleplay cards that omit the block (stays dormant — no `npc_registry`,
    /// no `[PRESENCE]` validation, the whitelist is empty so the narrator
    /// follows the pre-Phase-5 behavior). When non-empty, `enter_fable_session`
    /// seeds [`crate::schema::WorldSchema::npc_registry`] from this; the
    /// `[PRESENCE]` bracket's anti-hallucination gate (unknown id → reject)
    /// keys off the seeded registry.
    ///
    /// `Vec<CardNpc>` (not `schema::NpcRegistry` directly) to keep `sim_card`
    /// free of the `schema` module dependency — `enter_fable_session` does the
    /// one-line conversion (mirrors the `locations`/`CardNode` precedent).
    /// `#[serde(default)]` keeps older card JSON (written before this field
    /// existed) loading cleanly.
    ///
    /// This is the load-bearing fix for the "teleporting NPC" problem: before
    /// Phase 5A there was no Rust-owned authoritative NPC id set, so the
    /// `[PRESENCE]` bracket had nothing to validate against (the same shape
    /// as the §11.48 travel-graph-was-never-seeded gap). The registry is the
    /// whitelist the narrator obeys.
    #[serde(default)]
    pub cast: Vec<CardNpc>,
    /// Fable cold-start anchors (2026-08-10, Issue #2): the optional `<start>`
    /// block's seeded initial time + weather. Empty `Default` (no seed) for
    /// system cards + roleplay cards that omit the block — the clock + weather
    /// stay dormant until the first `[TIME]`/`[WEATHER]` bracket (the
    /// pre-start-block behavior). When populated, `fable_start` seeds
    /// `WorldClock` + `Weather` so both render from turn 1, giving the tracker
    /// the anchors it needs to maintain them. See [`CardStart`].
    #[serde(default)]
    pub start: CardStart,
    /// Custom extensions (2026-08-13): a flat key→value string map authored
    /// via the SIM Wizard's optional `custom_tags` (any extra stat / faction
    /// standing / curse / currency / attribute that doesn't fit a standard
    /// field). Parsed from `<custom_tags><entry key="...">value</entry>…
    /// </custom_tags>`. At game start these seed `WorldSchema.custom_tags`
    /// (rendered as a bounded `custom:` line) so they reach the narrator.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_tags: BTreeMap<String, String>,
}

impl SimCard {
    /// Render the persona into a compact `<persona>` block for the system
    /// prompt. Only the identity-shaping fields are rendered: `introductions`
    /// are a UI concern, not model context. Returns an empty `String` for the
    /// minimal fallback (so the caller's `Option<&str>` gate suppresses the
    /// section cleanly when there's no real persona).
    ///
    /// XML-tagged sections match the prompt's existing aesthetic (Prime
    /// Directive §1B.3: rigid structure exploits instruction-tuned attention).
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

    /// Pick one introduction line at random. Returns `None` if the card has no
    /// introductions (the caller then shows no boot bubble). Called once per
    /// boot via the `get_intro` IPC command.
    pub fn random_intro(&self) -> Option<&str> {
        if self.introductions.is_empty() {
            return None;
        }
        let mut rng = rand::rng();
        self.introductions.choose(&mut rng).map(String::as_str)
    }

    /// The fallback stub has this sentinel id so `render_for_prompt` can detect
    /// it and emit nothing (suppressing the `<persona>` section entirely).
    fn is_fallback(&self) -> bool {
        self.id == FALLBACK_ID
    }
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
        core_persona: String::new(),
        traits: String::new(),
        appearance: String::new(),
        role_instruction: String::new(),
        responsibilities: String::new(),
        conversational_rules: String::new(),
        technical_rules: String::new(),
        introductions: Vec::new(),
        intro: String::new(),
        // Roleplay-only fields: all empty for the system-card fallback.
        setting: None,
        plot: None,
        tone: None,
        start_npc_ids: Vec::new(),
        declared_activities: Vec::new(),
        player_name: None,
        locations: Vec::new(),
        cast: Vec::new(),
        start: CardStart::default(),
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
fn find_root_close(xml: &str) -> Option<usize> {
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
        if starts(i, b"</sim_card") {
            let mut j = i + b"</sim_card".len();
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
    // A `.sim` file is a TWO-ROOT document: `<sim_card>…</sim_card>` + an
    // optional sibling `<intro>`/`<introduction>` AFTER it (§6B — the intro
    // stays out of the cached card so it never inflates the system prompt).
    // roxmltree (0.10) REJECTS multi-root documents outright ("unknown
    // token"), so slice at the root's closing tag + parse the head as the
    // document; the TAIL (everything after) is re-parsed below, wrapped in a
    // synthetic root, to fish out the sibling intro. A string with no
    // close tag (the Creator's draft output, in-memory parses) slices to
    // itself + an empty tail — behavior unchanged.
    let (head, tail) = match find_root_close(xml) {
        Some(end) => xml.split_at(end),
        None => (xml, ""),
    };
    let doc = roxmltree::Document::parse(head).map_err(|e| {
        let hint = if tail.trim().is_empty() {
            String::new()
        } else {
            " (a sibling <intro> tail exists after the root — the head is unbalanced; \
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

    // `id` is OPTIONAL and derived from <identity><name> when <metadata> is
    // absent. Whatever its source, the id is NORMALIZED through the SAME slug
    // rules `fable_write_card` uses for the folder stem (lowercase, non-
    // [alphanumeric_-]→'-', dashes trimmed). The parsed id and the on-disk
    // folder must agree or every by-id path (state saves, codex seed, portrait,
    // raw editor, auto .lnk) lands in a phantom folder — the multi-word-name
    // bug ("Mistwood Vale" → id "mistwood vale" vs folder "mistwood-vale").
    // A metadata id that normalizes onto a memory sentinel is ignored: a card
    // must never share a partition key with __wupi__/__codex__ etc.
    let name = first_child(root, "identity")
        .and_then(|n| child_text(n, "name"))
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
    let id = nested_text(root, "metadata", "id")
        .as_deref()
        .and_then(crate::slugify_card_stem)
        .filter(|v| !is_sentinel(v))
        .or_else(|| crate::slugify_card_stem(&name))
        .unwrap_or_else(|| "unknown".to_owned());
    let card_type = nested_text(root, "metadata", "type").unwrap_or_else(|| "system".to_owned());
    let subtype = nested_text(root, "metadata", "subtype").filter(|s| !s.is_empty());

    let identity = first_child(root, "identity");
    let core_persona = identity
        .and_then(|n| child_text(n, "persona"))
        .unwrap_or_default();
    let traits = identity
        .and_then(|n| child_text(n, "traits"))
        .unwrap_or_default();

    let appearance = first_child(root, "appearance")
        .map(|n| {
            // Render the whole appearance block as-is: each child element on
            // its own line as `tag: text`, preserving the list-style children
            // (hair, clothing) verbatim.
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

    // Deprecated as a persona mechanism (2026-07-29): technical protocols are
    // now Rust-injected per pass (prompts::WUPI_AGENT_PROTOCOL for the chat
    // agent pass; the narrator/tracker/scribe builders for Fable). Retained as
    // a dormant back-compat shim — see the field doc on `SimCard.technical_rules`.
    // The shipped `wupi.sim` card no longer carries this block; user
    // cards that still do parse + render it unchanged.
    let technical_rules = first_child(root, "technical_protocols")
        .and_then(|n| child_text(n, "rules"))
        .unwrap_or_default();

    // `<intro>` (canonical) / `<introduction>` (legacy alias) as a SIBLING
    // element after `</sim_card>` — the Fable opening beat + wupi.sim's
    // boot-greeting block. Kept OUT of `<sim_card>` so it never inflates
    // the cached system prompt (prime directive). Lives in the post-close
    // TAIL sliced off above (roxmltree can't parse the two-root file
    // whole); the tail is wrapped in a synthetic root so stray text nodes
    // + whitespace can't break the parse. The prior parser only read
    // `<introductions>` (plural) INSIDE `<sim_card>`, so wupi.sim's sibling
    // block was silently dropped — fixed 2026-08-13.
    let intro = extract_sibling_intro(tail);

    let introductions = first_child(root, "introductions")
        .map(|n| {
            // The block is a CDATA bullet list: one intro per `- ` line.
            // Strip the leading `- ` and trim each. Empty lines drop.
            text_content(n)
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(|l| l.strip_prefix("- ").unwrap_or(l).trim().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            // No legacy inside-`<sim_card>` `<introductions>` block: seed the
            // greeting list from the sibling `<intro>`/`<introduction>` block's
            // non-empty lines so `random_intro` (wupi.sim's boot splash) works.
            // Each line is a standalone greeting; strip an optional `- ` prefix.
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

    // ── World/scenario fields ──────────────────────────────────────────────
    // FLAT-FIRST parse (2026-08-01 reorg): `setting`/`plot`/`tone`/
    // `opening_scene`/`player_name`/`start_npcs`/`activities`/`locations`/
    // `cast` are read as DIRECT children of `<sim_card>` (the canonical
    // shape authored by the Creator). Each falls back to
    // the legacy `<scenario>` wrapper if the top-level read returns None, so
    // cards authored before the reorg (e.g. `rusty_tavern.sim` at migration
    // time, and old user cards in the wild) still load unchanged. The flat
    // shape is canonical; the scenario-wrapper fallback is back-compat only.
    //
    // `field_or` + `field_node_or` are the flat-first helpers (defined below
    // near `first_child`). All fields optional; absent on system cards
    // (Wupi) → every field at its default (None / empty).
    let scenario = first_child(root, "scenario");
    let setting = field_or(root, scenario, "setting")
        .filter(|s| !s.is_empty());
    let plot = field_or(root, scenario, "plot")
        .filter(|s| !s.is_empty());
    let tone = field_or(root, scenario, "tone")
        .filter(|s| !s.is_empty());
    // NOTE: `opening_scene` is no longer parsed here (2026-08-05). The intro
    // lives in a sibling `.intro` file, read once at game start + surfaced on
    // `FableLoadResult.intro`. See the struct doc above.
    let start_npc_ids = field_node_or(root, scenario, "start_npcs")
        .map(|n| parse_bullet_list(&text_content(n)))
        .unwrap_or_default();
    let declared_activities = field_node_or(root, scenario, "activities")
        .map(|n| parse_bullet_list(&text_content(n)))
        .unwrap_or_default();
    // Player name (Phase E narrator hardening, 2026-07-18). Optional;
    // absent on system cards and on roleplay cards that don't declare one.
    // Reads `<player_name>` (current); falls back to the legacy tag for old
    // saves authored before the rename. Auto-migration, NOT deletion — old
    // user .sim files in the wild must still load. Flat-first like the rest.
    let player_name = field_or(root, scenario, "player_name")
        .or_else(|| field_or(root, scenario, "protagonist"))
        .filter(|s| !s.is_empty());

    // Fable Phase 4 Component 3 (2026-07-28): optional <locations> block.
    // Each <node> has an `id` attribute, an optional `setting` attribute
    // ("indoor"/"outdoor"/empty), a <name> child, and 0+ <neighbor>
    // children (bare node ids). Absent on system cards + roleplay cards
    // that don't declare geography → empty Vec (dormant graph, pre-Phase-4
    // behavior). This is the load-bearing fix for Components 3 + 4 being
    // dead in live play — see docs/phase4-fix-travel-graph-seeding.md.
    let locations = field_node_or(root, scenario, "locations")
        .map(|loc| {
            loc.children()
                .filter(|c| c.is_element() && c.has_tag_name("node"))
                .map(|node_el| {
                    let id = node_el.attribute("id").unwrap_or("").trim().to_owned();
                    let setting = node_el
                        .attribute("setting")
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let name = child_text(node_el, "name").unwrap_or_default();
                    // Postel's law for adjacency edges (2026-08-10, Issue #1):
                    // accept BOTH authoring shapes so a card author can't break
                    // their travel graph over a typo'd 's':
                    //   (a) FLAT — one <neighbor> child element per edge:
                    //         <neighbor>cellar</neighbor>
                    //         <neighbor>market_square</neighbor>
                    //     (the canonical form documented in rusty_tavern.sim)
                    //   (b) WRAPPED — a single <neighbors> element holding a
                    //     comma- and/or whitespace-separated id list:
                    //         <neighbors>market_square, warehouse_docks</neighbors>
                    //     (the form the hand-authored cinderfen card used — every
                    //     node parsed with neighbors: [] because only <neighbor>
                    //     singular was recognized, freezing travel graph-wide).
                    // Both forms may combine; flat children are collected first,
                    // then the wrapped element's ids are split + appended. Dupes
                    // are NOT deduped here (the schema's upsert_node is the
                    // idempotent back-link authority; a harmless double-edge is
                    // cheaper than a dedupe pass on the hot parse path).
                    let mut neighbors: Vec<String> = node_el
                        .children()
                        .filter(|c| c.is_element() && c.has_tag_name("neighbor"))
                        .map(|n| text_content(n).trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if let Some(wrapped) = child_text(node_el, "neighbors") {
                        for id in wrapped.split(|c: char| c == ',' || c.is_whitespace()) {
                            let id = id.trim();
                            if !id.is_empty() {
                                neighbors.push(id.to_owned());
                            }
                        }
                    }
                    CardNode { id, name, neighbors, setting }
                })
                .filter(|n| !n.id.is_empty()) // defensive: drop id-less nodes
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Custom extensions (2026-08-13): optional <custom_tags> block. Each
    // <entry key="...">value</entry> becomes one key→value pair (CDATA-safe via
    // text_content). Absent → empty map (dormant). At game start these seed
    // WorldSchema.custom_tags so they reach the narrator via the `custom:` line.
    let custom_tags = field_node_or(root, scenario, "custom_tags")
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

    // Fable Phase 5A (2026-07-29): optional <cast> block. Each <npc> has an
    // `id` attribute, an optional `tier` attribute, a <name> child, a <role>
    // child, and 0+ <alias> children (alternate surface forms). Absent on
    // system cards + roleplay cards that don't declare a cast → empty Vec
    // (dormant registry, pre-Phase-5 behavior). This is the load-bearing
    // source for the `[PRESENCE]` whitelist — without it the bracket has
    // nothing to validate against (the §11.48-shaped gap).
    let cast = field_node_or(root, scenario, "cast")
        .map(|cast_el| {
            cast_el
                .children()
                .filter(|c| c.is_element() && c.has_tag_name("npc"))
                .map(|npc_el| {
                    let id = npc_el.attribute("id").unwrap_or("").trim().to_owned();
                    let tier = npc_el
                        .attribute("tier")
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty());
                    let name = child_text(npc_el, "name").unwrap_or_default();
                    let role = child_text(npc_el, "role").unwrap_or_default();
                    let aliases: Vec<String> = npc_el
                        .children()
                        .filter(|c| c.is_element() && c.has_tag_name("alias"))
                        .map(|n| text_content(n).trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                    CardNpc { id, name, role, tier, aliases }
                })
                .filter(|n| !n.id.is_empty()) // defensive: drop id-less npcs
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Fable cold-start anchors (2026-08-10, Issue #2): optional <start> block
    // seeding the initial world clock + weather so they render from turn 1
    // (giving the tracker the anchors it needs to maintain them). Flat-first
    // (top-level <start>), falling back to <scenario><start>. Both children
    // are optional + independently parsed: a card may seed time-only,
    // weather-only, both, or neither (the dormant pre-start behavior).
    // <time> is parsed by bracket_parser::parse_in_world_time (the SAME parser
    // [TIME] uses — "Day N, HH:MM", 12h AM/PM, calendar dates); an unparseable
    // <time> is a warn-and-skip (None), never a card-load failure. <weather>
    // is free-form diegetic prose taken verbatim.
    let start = if let Some(start_el) = field_node_or(root, scenario, "start") {
        let time_minutes = child_text(start_el, "time")
            .filter(|s| !s.is_empty())
            .and_then(|s| {
                match crate::bracket_parser::parse_in_world_time(&s) {
                    Some(mins) => Some(mins),
                    None => {
                        tracing::warn!(
                            card_id = %id,
                            time_text = %s,
                            "card <start><time> did not parse; clock will stay dormant"
                        );
                        None
                    }
                }
            });
        let weather = child_text(start_el, "weather")
            .filter(|s| !s.is_empty());
        let date = child_text(start_el, "date").filter(|s| !s.is_empty());
        CardStart { time_minutes, weather, date }
    } else {
        CardStart::default()
    };

    Ok(SimCard {
        id,
        name,
        card_type,
        subtype,
        core_persona,
        traits,
        appearance,
        role_instruction,
        responsibilities,
        conversational_rules,
        technical_rules,
        introductions,
        intro,
        setting,
        plot,
        tone,
        start_npc_ids,
        declared_activities,
        player_name,
        locations,
        cast,
        start,
        custom_tags,
    })
}

/// Parse a CDATA bullet list (`- item one\n- item two`) into owned Strings.
/// Shared by `<introductions>`, `<scenario><start_npcs>`, and
/// `<scenario><activities>`. Strips the leading `- ` and trims each line;
/// empty lines drop. Factored out so the three callers don't duplicate the
/// same line-walk.
fn parse_bullet_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_prefix("- ").unwrap_or(l).trim().to_owned())
        .collect::<Vec<_>>()
}

// roxmltree's API is verbose; these thin wrappers keep the parser readable.
// CDATA is already merged into `.text()` by roxmltree, so `text_content`
// returns the full text of a node regardless of how it was wrapped.

/// Read the sibling `<intro>`/`<introduction>` from the TAIL of a `.sim`
/// file (everything after the first `</sim_card>`). The tail is wrapped in a
/// synthetic root so whitespace + stray text between the two real roots
/// can't break the parse (roxmltree rejects bare multi-root/text documents).
/// Absent / empty / unparsable tail → `String::new()` (the common no-intro
/// card case, never an error — the intro is an optional flourish).
fn extract_sibling_intro(tail: &str) -> String {
    if tail.trim().is_empty() {
        return String::new();
    }
    let wrapped = format!("<wupi_sim_siblings>{tail}</wupi_sim_siblings>");
    let doc = match roxmltree::Document::parse(&wrapped) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(?e, "sim_card: unparsable post-</sim_card> tail — ignoring");
            return String::new();
        }
    };
    doc.root_element()
        .children()
        .find(|n| {
            n.is_element() && {
                let name = n.tag_name().name();
                name == "intro" || name == "introduction"
            }
        })
        .map(|n| text_content(n).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

/// The concatenated text of a node (CDATA + plain text children merged).
fn text_content(node: roxmltree::Node) -> String {
    node.text().unwrap_or("").to_owned()
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

/// FLAT-FIRST text read (2026-08-01 card-format reorg): the trimmed text of a
/// top-level `<tag>` child of `root`, falling back to `<scenario><tag>` when
/// the top-level read is absent. The flat shape (direct children of
/// `<sim_card>`) is canonical; the `<scenario>` wrapper is back-compat for
/// cards authored before the reorg. `scenario` is the pre-resolved optional
/// `<scenario>` node (passed in by the caller so it's resolved once, not per
/// field). Returns `None` when neither location carries the tag — the caller's
/// `.filter(|s| !s.is_empty())` turns an empty-string hit into None.
fn field_or(
    root: roxmltree::Node,
    scenario: Option<roxmltree::Node>,
    tag: &str,
) -> Option<String> {
    child_text(root, tag).or_else(|| scenario.and_then(|n| child_text(n, tag)))
}

/// FLAT-FIRST element read: the top-level `<tag>` child node of `root`, or the
/// `<scenario><tag>` child as fallback. Like [`field_or`] but returns the
/// *node* (not its text) so callers that walk children (`<locations>`,
/// `<cast>`, `<start_npcs>`, `<activities>`) can iterate it.
fn field_node_or<'a, 'input>(
    root: roxmltree::Node<'a, 'input>,
    scenario: Option<roxmltree::Node<'a, 'input>>,
    tag: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    first_child(root, tag).or_else(|| scenario.and_then(|n| first_child(n, tag)))
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
        // technical_protocols block was removed from the SAMPLE fixture
        // (2026-07-29): technical protocols are now Rust-injected per pass,
        // never authored in a .sim card. The shipped cards leave technical_rules
        // empty. Back-compat (a card WITH the block still rendering it) is
        // pinned by `technical_protocols_block_still_renders_for_back_compat`.
        assert!(card.technical_rules.is_empty());
        assert_eq!(card.introductions.len(), 2);
        assert!(card.introductions[0].contains("Hello Master"));
        // The literal `>` in the emoticon survives (the XML/CDATA contract).
        assert!(card.introductions[1].contains("ฅ^>⩊<^ฅ"));
    }

    #[test]
    fn parse_strips_intro_bullet_prefix() {
        let card = parse(SAMPLE).expect("parses");
        // Intros should not carry the leading `- ` marker into the UI text.
        for intro in &card.introductions {
            assert!(!intro.starts_with("- "), "intro kept its bullet: {intro}");
        }
    }

    #[test]
    fn parse_reads_sibling_intro_after_sim_card() {
        // wupi.sim's actual shape: `<introduction>` (singular) as a SIBLING
        // after `</sim_card>`. The prior parser only read `<introductions>`
        // (plural) inside `<sim_card>`, so this block was silently dropped.
        // 2026-08-13: the sibling `<intro>`/`<introduction>` is now the canonical
        // intro home (drives the Fable opening beat + seeds the greeting list).
        let xml = r#"<?xml version="1.0"?>
<sim_card>
  <identity><name>Wupi</name></identity>
</sim_card>

<introduction><![CDATA[
Hello, Master~ ฅ^>⩊<^ฅ
Booted up, nya~!
]]></introduction>"#;
        let card = parse(xml).expect("sibling intro parses");
        // `intro` carries the full block (the Fable opening beat).
        assert!(card.intro.contains("Hello, Master~"));
        assert!(card.intro.contains("Booted up, nya~!"));
        // The greeting list is seeded from the block's lines (random_intro).
        assert_eq!(card.introductions.len(), 2);
        assert!(card.introductions[0].contains("Hello, Master~"));
    }

    #[test]
    fn parse_reads_canonical_intro_tag_alias() {
        // `<intro>` (canonical) + `<introduction>` (legacy alias) both read.
        // Empty/whitespace-only block → empty intro + empty greeting list.
        let xml = r#"<sim_card><identity><name>X</name></identity></sim_card>
<intro><![CDATA[The fog rolls in over Aldermoor.]]></intro>"#;
        let card = parse(xml).expect("canonical <intro> parses");
        assert_eq!(card.intro, "The fog rolls in over Aldermoor.");
        // A single-paragraph Fable beat still yields one greeting-list entry.
        assert_eq!(card.introductions, vec!["The fog rolls in over Aldermoor.".to_string()]);
    }

    #[test]
    fn parse_intro_absent_when_card_has_no_sibling() {
        // A roleplay card with neither sibling `<intro>` nor inside
        // `<introductions>` → both empty (the common Fable card case).
        let xml = r#"<sim_card>
  <metadata><type>roleplay</type></metadata>
  <identity><name>Aldermoor</name></identity>
</sim_card>"#;
        let card = parse(xml).expect("parses");
        assert!(card.intro.is_empty());
        assert!(card.introductions.is_empty());
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
        // technical_protocols section is suppressed when empty (the SAMPLE
        // fixture no longer carries the block). Back-compat rendering is
        // pinned by `technical_protocols_block_still_renders_for_back_compat`.
        assert!(!rendered.contains("<technical_protocols>"));
        // Introductions must NOT leak into the model persona block.
        assert!(!rendered.contains("Hello Master"));
    }

    #[test]
    fn technical_protocols_block_still_renders_for_back_compat() {
        // 2026-07-29 deprecation: the `<technical_protocols>` block is no
        // longer shipped in any card (protocols are Rust-injected per pass),
        // BUT a user-authored .sim card that still includes one must parse +
        // render it unchanged — the field is a dormant back-compat shim, not
        // ripped. This test pins that contract so a future cleanup doesn't
        // silently break existing user cards.
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
        assert!(
            rendered.contains("<technical_protocols>"),
            "a card carrying the block must still render it (back-compat)"
        );
        assert!(rendered.contains("Legacy rule that still works."));
    }

    #[test]
    fn random_intro_returns_none_when_empty() {
        let card = SimCard {
            id: "x".into(),
            name: "x".into(),
            card_type: "system".into(),
            subtype: None,
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            intro: String::new(),
            setting: None,
            plot: None,
            tone: None,
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            player_name: None,
            locations: Vec::new(),
            cast: Vec::new(),
            start: CardStart::default(),
            custom_tags: BTreeMap::new(),
        };
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
        // The fallback suppresses the persona section entirely: empty render.
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
        // Metadata is OPTIONAL: a clean, persona-only card (no <metadata>
        // block) must still parse. The id derives from <identity><name>,
        // lowercased. This is the card format going forward.
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

    /// A roleplay scenario card (Games app Seam 1). Same strict-XML + CDATA
    /// format as `Wupi.sim`, but with a `<scenario>` block holding setting,
    /// tone, start_npcs, and activities. (The opening beat is no longer in
    /// the card — 2026-08-05 it moved to a sibling `.intro` file.) The
    /// system card (Wupi) omits this block entirely: those fields stay at
    /// their default (None / empty). The dungeon card below is also the §2L-test seed
    /// (the dungeon half of the cross-topic memory rejection test).
    #[test]
    fn parse_roleplay_scenario_block() {
        let roleplay = r#"<?xml version="1.0"?>
<sim_card>
  <metadata>
    <id>dungeon_tavern</id>
    <name>The Rusty Tankard</name>
    <type>roleplay</type>
  </metadata>
  <identity>
    <name>The Rusty Tankard</name>
    <persona>A one-shot dungeon scenario.</persona>
  </identity>
  <scenario>
    <setting><![CDATA[
A remote frontier tavern at the edge of the Goblinwood. Travellers
shelter here before braving the ruined keep to the north.
    ]]></setting>
    <tone>grim, atmospheric, slow-burn</tone>
    <start_npcs><![CDATA[
- barkeeper
- goblin
    ]]></start_npcs>
    <activities><![CDATA[
- combat
    ]]></activities>
    <player_name>Alex</player_name>
  </scenario>
</sim_card>"#;
        let card = parse(roleplay).expect("roleplay card parses");
        assert_eq!(card.id, "dungeon_tavern");
        assert_eq!(card.card_type, "roleplay");
        assert!(card.setting.as_deref().unwrap().contains("frontier tavern"));
        assert_eq!(card.tone.as_deref(), Some("grim, atmospheric, slow-burn"));
        // opening_scene is no longer parsed (2026-08-05): the intro lives in
        // a sibling .intro file. A leftover <opening_scene> element is now
        // silently ignored by the parser.
        assert_eq!(card.start_npc_ids, vec!["barkeeper".to_string(), "goblin".to_string()]);
        assert_eq!(card.declared_activities, vec!["combat".to_string()]);
        assert_eq!(card.player_name.as_deref(), Some("Alex"));
    }

    /// The CANONICAL flat format (2026-08-01 card reorg): `setting`/`plot`/
    /// `tone` as DIRECT children of `<sim_card>` (no `<scenario>` wrapper),
    /// plus the `<persona>` tag (renamed from `<core_persona>`). (The opening
    /// beat moved to a sibling `.intro` file 2026-08-05 — no longer a card
    /// field.) This is the flat shape the Creator emits. The `<plot>` field
    /// is new to the reorg — it pins
    /// that top-level `<plot>` parses into `SimCard.plot`.
    #[test]
    fn parse_flat_format_top_level_fields() {
        let flat = r#"<?xml version="1.0"?>
<sim_card>
  <identity>
    <name>Narrator</name>
    <persona><![CDATA[ An impartial world-simulation engine. ]]></persona>
  </identity>
  <setting><![CDATA[ A frontier tavern at the edge of the woods. ]]></setting>
  <plot><![CDATA[ Drive story through consequence. Let complications grow organically. ]]></plot>
  <tone><![CDATA[ Atmospheric, grounded, slow-burn. ]]></tone>
</sim_card>"#;
        let card = parse(flat).expect("flat-format card parses");
        assert_eq!(card.name, "Narrator");
        assert_eq!(card.core_persona, "An impartial world-simulation engine.");
        assert!(card.setting.as_deref().unwrap().contains("frontier tavern"));
        assert!(
            card.plot.as_deref().unwrap().contains("Drive story through consequence"),
            "top-level <plot> must parse into SimCard.plot"
        );
        assert!(card.tone.as_deref().unwrap().contains("Atmospheric"));
    }

    /// Back-compat: the legacy `<scenario>` wrapper STILL loads (the flat-first
    /// parser falls back to it). A card authored before the 2026-08-01 reorg
    /// must not break. Pins the `field_or` fallback path.
    #[test]
    fn scenario_wrapper_still_loads_via_fallback() {
        let wrapped = r#"<sim_card>
  <identity><name>Old Card</name><persona>Legacy.</persona></identity>
  <scenario>
    <setting>Wrapped setting.</setting>
    <tone>Wrapped tone.</tone>
  </scenario>
</sim_card>"#;
        let card = parse(wrapped).expect("scenario-wrapped card parses via fallback");
        assert_eq!(card.setting.as_deref(), Some("Wrapped setting."));
        assert_eq!(card.tone.as_deref(), Some("Wrapped tone."));
        assert!(card.plot.is_none(), "no <plot> in this fixture → None");
    }

    /// The system card (Wupi.sim) has NO `<scenario>` block. Every roleplay
    /// field stays at its default. This guards against the additive fields
    /// accidentally picking up stray values from a system card.
    #[test]
    fn system_card_has_no_scenario_fields() {
        let card = parse(SAMPLE).expect("system card parses");
        assert_eq!(card.card_type, "system");
        assert!(card.setting.is_none());
        assert!(card.tone.is_none());
        assert!(card.start_npc_ids.is_empty());
        assert!(card.declared_activities.is_empty());
        assert!(card.player_name.is_none());
    }

    /// Pins the Serialize/Deserialize round trip so a card survives
    /// write + read intact (the roleplay fields are the load-bearing
    /// ones for the narrator prompt).
    #[test]
    fn simcard_serializes_to_json_roundtrip() {
        let original = SimCard {
            id: "qp_test".into(),
            name: "Test Sim".into(),
            card_type: "roleplay".into(),
            subtype: None,
            core_persona: "cp".into(),
            traits: "t".into(),
            appearance: "a".into(),
            role_instruction: "ri".into(),
            responsibilities: "r".into(),
            conversational_rules: "cr".into(),
            technical_rules: "tr".into(),
            introductions: vec!["hi".into()],
            intro: String::new(),
            setting: Some("A test place.".into()),
            plot: None,
            tone: Some("grim".into()),
            start_npc_ids: vec!["npc_one".into(), "npc_two".into()],
            declared_activities: vec!["combat".into()],
            player_name: Some("Kaelen".into()),
            locations: Vec::new(),
            cast: Vec::new(),
            start: CardStart::default(),
            custom_tags: BTreeMap::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: SimCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, original.id);
        assert_eq!(back.name, original.name);
        assert_eq!(back.card_type, "roleplay");
        assert_eq!(back.setting, original.setting);
        assert_eq!(back.tone, original.tone);
        assert_eq!(back.start_npc_ids, original.start_npc_ids);
        assert_eq!(back.declared_activities, original.declared_activities);
        assert_eq!(back.player_name, original.player_name);
        assert_eq!(back.introductions, original.introductions);
    }

    /// `parse_from_xml_str` is the public entry for parsing an in-memory
    /// `<sim_card>` string. Pins that it accepts the same shape as the on-disk
    /// parser. Uses the modern
    /// `<player_name>` tag.
    #[test]
    fn parse_from_xml_str_works() {
        let xml = r#"<sim_card>
  <metadata><id>test_id</id><type>roleplay</type></metadata>
  <identity><name>Test Scenario</name></identity>
  <scenario>
    <setting>A place.</setting>
    <tone>atmospheric</tone>
    <player_name>Kaelen</player_name>
    <start_npcs>- npc_a</start_npcs>
    <activities>- exploration</activities>
  </scenario>
</sim_card>"#;
        let card = parse_from_xml_str(xml).expect("parses");
        assert_eq!(card.id, "test_id");
        assert_eq!(card.name, "Test Scenario");
        assert_eq!(card.card_type, "roleplay");
        assert_eq!(card.player_name.as_deref(), Some("Kaelen"));
        assert_eq!(card.start_npc_ids, vec!["npc_a".to_string()]);
    }

    /// The 2026-08-13 SIM Wizard fields parse + round-trip: `<subtype>`, the
    /// `<start><date>` calendar anchor, + the `<custom_tags>` map.
    #[test]
    fn parses_subtype_date_and_custom_tags() {
        let xml = r#"<sim_card>
  <metadata><type>roleplay</type><subtype>npc</subtype></metadata>
  <identity><name>Mara</name></identity>
  <start>
    <date>3rd of Harvest, Year 1247</date>
    <time>Day 1, 09:00</time>
    <weather>clear</weather>
  </start>
  <custom_tags>
    <entry key="faction">thieves guild</entry>
    <entry key="bounty">500 gold</entry>
  </custom_tags>
</sim_card>"#;
        let card = parse_from_xml_str(xml).expect("parses");
        assert_eq!(card.card_type, "roleplay");
        assert_eq!(card.subtype.as_deref(), Some("npc"));
        assert_eq!(card.start.date.as_deref(), Some("3rd of Harvest, Year 1247"));
        assert_eq!(card.start.weather.as_deref(), Some("clear"));
        assert_eq!(
            card.custom_tags.get("faction").map(|s| s.as_str()),
            Some("thieves guild")
        );
        assert_eq!(card.custom_tags.get("bounty").map(|s| s.as_str()), Some("500 gold"));
        // Round-trip through JSON keeps the new fields.
        let json = serde_json::to_string(&card).expect("serialize");
        let back: SimCard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.subtype.as_deref(), Some("npc"));
        assert_eq!(back.start.date.as_deref(), Some("3rd of Harvest, Year 1247"));
        assert_eq!(back.custom_tags.get("faction").map(|s| s.as_str()), Some("thieves guild"));
    }

    /// Legacy auto-migration: an old `.sim` file using the pre-rename tag
    /// `<protagonist>` must still load, with the value migrated to the new
    /// `player_name` field. This is for old user-authored cards in the wild —
    /// the on-disk file format must stay backwards-compatible.
    #[test]
    fn parse_legacy_tag_auto_migrates_to_player_name() {
        let xml = r#"<sim_card>
  <metadata><id>legacy</id><type>roleplay</type></metadata>
  <identity><name>Legacy</name></identity>
  <scenario>
    <setting>x.</setting>
    <protagonist>Kaelen</protagonist>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("legacy card parses");
        assert_eq!(card.player_name.as_deref(), Some("Kaelen"));
    }

    /// The modern tag takes precedence when BOTH are present (defensive — a
    /// card author shouldn't write both, but if they do, modern wins).
    #[test]
    fn modern_player_name_tag_wins_over_legacy() {
        let xml = r#"<sim_card>
  <metadata><id>dual</id><type>roleplay</type></metadata>
  <identity><name>Dual</name></identity>
  <scenario>
    <setting>x.</setting>
    <player_name>Modern</player_name>
    <protagonist>Legacy</protagonist>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("dual-tag card parses");
        assert_eq!(card.player_name.as_deref(), Some("Modern"));
    }

    /// A JSON object missing the roleplay fields (an older save, or a
    /// minimal card) must deserialize with those fields at their
    /// defaults, NOT fail. `#[serde(default)]` is the load-bearing attribute.
    #[test]
    fn simcard_deserialize_partial_json_fills_defaults() {
        let partial = r#"{"id":"x","name":"X","card_type":"roleplay","core_persona":"","traits":"","appearance":"","role_instruction":"","responsibilities":"","conversational_rules":"","technical_rules":""}"#;
        let card: SimCard = serde_json::from_str(partial).expect("partial JSON loads");
        assert_eq!(card.id, "x");
        assert!(card.setting.is_none());
        assert!(card.start_npc_ids.is_empty());
        assert!(card.player_name.is_none());
        assert!(card.introductions.is_empty());
        // Phase 4 Component 3: locations defaults to empty (backward-compat).
        assert!(card.locations.is_empty());
        // Phase 5A: cast defaults to empty (backward-compat).
        assert!(card.cast.is_empty());
    }

    /// Phase 4 Component 3 (2026-07-28): a card with no `<locations>` block
    /// must parse with `locations` empty (the dormant-graph contract — the
    /// pre-Phase-4 behavior, preserved for every card that doesn't declare
    /// geography). This is the backward-compat invariant.
    #[test]
    fn card_without_locations_loads_empty() {
        let xml = r#"<sim_card>
  <metadata><id>noloc</id><type>roleplay</type></metadata>
  <identity><name>No Locations</name></identity>
  <scenario>
    <setting>A setting with no geography declared.</setting>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card without <locations> parses");
        assert!(card.locations.is_empty(), "card without <locations> must have empty locations");
    }

    /// Phase 4 Component 3 (2026-07-28): a card WITH a `<locations>` block
    /// must parse every `<node>` in document order, capturing id / setting
    /// attribute, `<name>` child, and all `<neighbor>` children. This pins
    /// the parser's behavior — the load-bearing path for Components 3 + 4
    /// being reachable in live play (see docs/phase4-fix-travel-graph-seeding.md).
    #[test]
    fn card_with_locations_parses_nodes_in_document_order() {
        let xml = r#"<sim_card>
  <metadata><id>graphed</id><type>roleplay</type></metadata>
  <identity><name>Graphed</name></identity>
  <scenario>
    <setting>A setting with declared geography.</setting>
    <locations>
      <node id="tavern" setting="indoor">
        <name>The Rusty Lantern</name>
        <neighbor>cellar</neighbor>
        <neighbor>market_square</neighbor>
      </node>
      <node id="cellar" setting="indoor">
        <name>The Cellar</name>
        <neighbor>tavern</neighbor>
      </node>
      <node id="market_square" setting="outdoor">
        <name>Market Square</name>
        <neighbor>tavern</neighbor>
      </node>
    </locations>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card with <locations> parses");
        assert_eq!(card.locations.len(), 3, "expected 3 parsed nodes");

        // Document order preserved (the first node seeds current_node).
        assert_eq!(card.locations[0].id, "tavern");
        assert_eq!(card.locations[0].name, "The Rusty Lantern");
        assert_eq!(card.locations[0].setting, "indoor");
        assert_eq!(card.locations[0].neighbors, vec!["cellar", "market_square"]);

        assert_eq!(card.locations[1].id, "cellar");
        assert_eq!(card.locations[1].setting, "indoor");
        assert_eq!(card.locations[1].neighbors, vec!["tavern"]);

        assert_eq!(card.locations[2].id, "market_square");
        assert_eq!(card.locations[2].setting, "outdoor");
        assert_eq!(card.locations[2].neighbors, vec!["tavern"]);
    }

    /// Postel's law for adjacency edges (2026-08-10, Issue #1): the WRAPPED
    /// `<neighbors>` element (a single child holding a comma- and/or whitespace-
    /// separated id list) must parse to the same `neighbors` Vec as the flat
    /// `<neighbor>`-per-edge form. The hand-authored cinderfen card used this
    /// wrapped shape; before the fix every node parsed with `neighbors: []`,
    /// freezing the entire travel graph (every [TRAVEL] rejected as
    /// non-adjacent). Both comma + whitespace separators are tolerated.
    #[test]
    fn card_location_wrapped_neighbors_element_parses() {
        let xml = r#"<sim_card>
  <metadata><id>wrapped</id><type>roleplay</type></metadata>
  <identity><name>Wrapped</name></identity>
  <locations>
    <node id="tavern">
      <name>The Crooked Lantern</name>
      <neighbors>market_square, warehouse_docks</neighbors>
    </node>
    <node id="market_square">
      <name>Market Square</name>
      <neighbors>warehouse_docks tavern</neighbors>
    </node>
  </locations>
</sim_card>"#;
        let card = parse(xml).expect("card with wrapped <neighbors> parses");
        assert_eq!(card.locations.len(), 2);
        // Comma-separated ids in the wrapped element.
        assert_eq!(card.locations[0].neighbors, vec!["market_square", "warehouse_docks"]);
        // Whitespace-separated ids in the wrapped element (also tolerated).
        assert_eq!(card.locations[1].neighbors, vec!["warehouse_docks", "tavern"]);
    }

    /// The flat `<neighbor>` + wrapped `<neighbors>` forms may combine on the
    /// same node (flat children collected first, then the wrapped ids
    /// appended). Idempotent back-link in `upsert_node` handles any dupes.
    #[test]
    fn card_location_flat_and_wrapped_neighbors_combine() {
        let xml = r#"<sim_card>
  <metadata><id>combo</id><type>roleplay</type></metadata>
  <identity><name>Combo</name></identity>
  <locations>
    <node id="tavern">
      <name>Tavern</name>
      <neighbor>cellar</neighbor>
      <neighbors>market_square, docks</neighbors>
    </node>
  </locations>
</sim_card>"#;
        let card = parse(xml).expect("card with combined neighbor forms parses");
        // Flat child first, then the wrapped element's ids appended in order.
        assert_eq!(card.locations[0].neighbors, vec!["cellar", "market_square", "docks"]);
    }

    /// Cold-start anchors (2026-08-10, Issue #2): a card with a `<start>`
    /// block seeds both `time_minutes` (parsed from the free-form time string
    /// via the same parser `[TIME]` uses) + the verbatim `weather` phrase.
    /// This is the load-bearing fix for the cold-start anchor gap — without
    /// seeded anchors the tracker renders no clock:/weather: line + never
    /// maintains them.
    #[test]
    fn card_start_block_seeds_time_and_weather() {
        let xml = r#"<sim_card>
  <metadata><id>anchored</id><type>roleplay</type></metadata>
  <identity><name>Anchored</name></identity>
  <start>
    <time>Day 1, 08:00</time>
    <weather>thick fog off the marsh</weather>
  </start>
</sim_card>"#;
        let card = parse(xml).expect("card with <start> parses");
        // Day 1 08:00 = (1-1)*1440 + 8*60 = 480 minutes.
        assert_eq!(card.start.time_minutes, Some(480));
        assert_eq!(card.start.weather.as_deref(), Some("thick fog off the marsh"));
    }

    /// A `<start>` block is fully optional — a card without one parses with an
    /// empty (dormant) `CardStart`. Both anchors stay `None` → the clock +
    /// weather stay dormant at fable_start (the pre-start-block behavior).
    #[test]
    fn card_without_start_block_has_dormant_anchors() {
        let xml = r#"<sim_card>
  <metadata><id>plain</id><type>roleplay</type></metadata>
  <identity><name>Plain</name></identity>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert!(card.start.time_minutes.is_none());
        assert!(card.start.weather.is_none());
    }

    /// A `<start>` block may seed time-only or weather-only (independent
    /// children). A card seeding weather-only leaves time dormant.
    #[test]
    fn card_start_block_weather_only() {
        let xml = r#"<sim_card>
  <metadata><id>weatheronly</id><type>roleplay</type></metadata>
  <identity><name>WeatherOnly</name></identity>
  <start><weather>clear night</weather></start>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert!(card.start.time_minutes.is_none(), "time absent → dormant clock");
        assert_eq!(card.start.weather.as_deref(), Some("clear night"));
    }

    /// Phase 4 Component 3 (2026-07-28): a node with no `setting` attribute
    /// must default to empty string (not crash, not "indoor"). The indoor/
    /// outdoor gate treats empty as outdoor (renders weather: line). This
    /// pins the optional-attribute handling.
    #[test]
    fn card_location_node_without_setting_defaults_to_empty() {
        let xml = r#"<sim_card>
  <metadata><id>settingless</id><type>roleplay</type></metadata>
  <identity><name>Settingless</name></identity>
  <scenario>
    <locations>
      <node id="void">
        <name>The Void</name>
      </node>
    </locations>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert_eq!(card.locations.len(), 1);
        assert_eq!(card.locations[0].id, "void");
        assert_eq!(card.locations[0].setting, "", "missing setting attribute must default to empty");
        assert!(card.locations[0].neighbors.is_empty(), "node with no <neighbor> children must have empty neighbors");
    }

    /// Phase 4 Component 3 (2026-07-28): a `<node>` with no `id` attribute
    /// is defensively DROPPED (an id-less node is unreferenceable — the
    /// [TRAVEL] parser + rumor propagation both key on id). The parser
    /// must not panic; other valid nodes in the same block still parse.
    #[test]
    fn card_location_node_without_id_is_dropped() {
        let xml = r#"<sim_card>
  <metadata><id>idless</id><type>roleplay</type></metadata>
  <identity><name>Idless</name></identity>
  <scenario>
    <locations>
      <node setting="indoor"><name>No Id Here</name></node>
      <node id="valid"><name>Valid Node</name></node>
    </locations>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert_eq!(card.locations.len(), 1, "id-less node must be dropped, valid node kept");
        assert_eq!(card.locations[0].id, "valid");
    }

    /// Phase 4 Component 3 (2026-07-28): a `<locations>` block that is an
    /// empty container (no `<node>` children) parses to an empty Vec. This
    /// is the "card declares geography but provides none" edge case —
    /// handled gracefully (dormant graph, same as no block at all).
    #[test]
    fn card_empty_locations_block_parses_to_empty_vec() {
        let xml = r#"<sim_card>
  <metadata><id>emptyloc</id><type>roleplay</type></metadata>
  <identity><name>Empty Locations</name></identity>
  <scenario>
    <locations></locations>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert!(card.locations.is_empty(), "empty <locations> block must yield empty Vec");
    }

    /// Phase 5A (2026-07-29): a card with no `<cast>` block must parse with
    /// `cast` empty (the dormant-registry contract — the pre-Phase-5 behavior,
    /// preserved for every card that doesn't declare a named cast). This is
    /// the backward-compat invariant (mirrors `card_without_locations_loads_empty`).
    #[test]
    fn card_without_cast_loads_empty() {
        let xml = r#"<sim_card>
  <metadata><id>nocast</id><type>roleplay</type></metadata>
  <identity><name>No Cast</name></identity>
  <scenario>
    <setting>A setting with no named cast.</setting>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card without <cast> parses");
        assert!(card.cast.is_empty(), "card without <cast> must have empty cast");
    }

    /// Phase 5A (2026-07-29): a card WITH a `<cast>` block must parse every
    /// `<npc>` in document order, capturing the `id`/`tier` attributes,
    /// `<name>`/`<role>` children, and all `<alias>` children. This pins the
    /// parser's behavior — the load-bearing path for the `[PRESENCE]`
    /// whitelist being reachable in live play (the §11.48-shaped fix).
    #[test]
    fn card_with_cast_parses_npcs_in_document_order() {
        let xml = r#"<sim_card>
  <metadata><id>casted</id><type>roleplay</type></metadata>
  <identity><name>Casted</name></identity>
  <scenario>
    <setting>A setting with a named cast.</setting>
    <cast>
      <npc id="mara_the_innkeep" tier="soldier">
        <name>Mara</name>
        <role>The innkeeper behind the bar</role>
        <alias>mara</alias>
        <alias>innkeep</alias>
      </npc>
      <npc id="bard_corin">
        <name>Corin</name>
        <role>A traveling bard tuning a lute</role>
      </npc>
    </cast>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card with <cast> parses");
        assert_eq!(card.cast.len(), 2, "expected 2 parsed npcs");

        // Document order preserved.
        assert_eq!(card.cast[0].id, "mara_the_innkeep");
        assert_eq!(card.cast[0].name, "Mara");
        assert_eq!(card.cast[0].role, "The innkeeper behind the bar");
        assert_eq!(card.cast[0].tier.as_deref(), Some("soldier"));
        assert_eq!(card.cast[0].aliases, vec!["mara", "innkeep"]);

        assert_eq!(card.cast[1].id, "bard_corin");
        assert_eq!(card.cast[1].name, "Corin");
        assert_eq!(card.cast[1].tier, None, "missing tier attribute must default to None");
        assert!(card.cast[1].aliases.is_empty(), "npc with no <alias> children must have empty aliases");
    }

    /// Phase 5A (2026-07-29): an `<npc>` with no `id` attribute is defensively
    /// DROPPED (an id-less npc is unreferenceable — the `[PRESENCE]` bracket
    /// + the whitelist both key on id). Other valid npcs in the same block
    /// still parse (mirrors `card_location_node_without_id_is_dropped`).
    #[test]
    fn card_cast_npc_without_id_is_dropped() {
        let xml = r#"<sim_card>
  <metadata><id>idlesscast</id><type>roleplay</type></metadata>
  <identity><name>Idless Cast</name></identity>
  <scenario>
    <cast>
      <npc tier="soldier"><name>No Id Here</name></npc>
      <npc id="valid"><name>Valid Npc</name></npc>
    </cast>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert_eq!(card.cast.len(), 1, "id-less npc must be dropped, valid npc kept");
        assert_eq!(card.cast[0].id, "valid");
    }

    /// Phase 5A (2026-07-29): the `<cast>` and `<locations>` blocks are
    /// independent — a card can declare both, either, or neither. This pins
    /// that parsing `<cast>` doesn't disturb `<locations>` and vice versa
    /// (defensive against a future refactor that shares a walk loop).
    #[test]
    fn card_cast_and_locations_coexist() {
        let xml = r#"<sim_card>
  <metadata><id>both</id><type>roleplay</type></metadata>
  <identity><name>Both</name></identity>
  <scenario>
    <locations>
      <node id="tavern" setting="indoor"><name>Tavern</name></node>
    </locations>
    <cast>
      <npc id="mara"><name>Mara</name></npc>
    </cast>
  </scenario>
</sim_card>"#;
        let card = parse(xml).expect("card parses");
        assert_eq!(card.locations.len(), 1);
        assert_eq!(card.locations[0].id, "tavern");
        assert_eq!(card.cast.len(), 1);
        assert_eq!(card.cast[0].id, "mara");
    }

    /// Phase 5A (2026-07-29): serde round trip for CardNpc + the cast field
    /// (the round trip must survive write + read, same contract as `locations`).
    #[test]
    fn card_cast_serializes_roundtrip() {
        let original = CardNpc {
            id: "mara_the_innkeep".into(),
            name: "Mara".into(),
            role: "The innkeeper".into(),
            tier: Some("soldier".into()),
            aliases: vec!["mara".into(), "innkeep".into()],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: CardNpc = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, original.id);
        assert_eq!(back.name, original.name);
        assert_eq!(back.role, original.role);
        assert_eq!(back.tier, original.tier);
        assert_eq!(back.aliases, original.aliases);
    }
}
