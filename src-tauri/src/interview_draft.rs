//! The in-memory "building" SimCard for the New Game interview flow.
//!
//! During a Game Master interview (`interview_send`), the local Gemma Scribe
//! extracts facts from the conversation and emits `sim_draft` tool calls whose
//! `updates` are applied to an `InterviewDraft` (held in
//! `AppState::interview_draft`). The draft accumulates across turns; on
//! `interview_finalize`, [`InterviewDraft::to_sim_card_xml`] + the
//! [`crate::sim_card`] parser produce the final `.sim` file, and
//! [`InterviewDraft::to_world_schema`] / [`InterviewDraft::to_player_state`]
//! seed the starting world + character sheet.
//!
//! # Design contracts
//!
//! - **Pure data + pure transforms.** No AppState, no I/O, no locks. Mirrors
//!   `consequence.rs` / `offscreen_task.rs` / `weather.rs`: unit-testable in
//!   isolation, callable from the IPC layer under brief locks.
//! - **The Scribe NEVER emits XML.** [`InterviewDraft::to_sim_card_xml`] builds
//!   the XML via Rust string assembly + smoke-validates with `roxmltree`. XML
//!   well-formedness is impossible to corrupt by construction — the only
//!   failure mode is a missing required field, which [`Self::is_finalizable`]
//!   gates upstream.
//! - **Qualitative only.** Per the fable.codex "World Schema Seeding" +
//!   "Player State Seeding" entries: no stats, no numbers, no "Level 8 / HP
//!   50." Condition is a free-form diegetic string; Rust owns the dice. The
//!   only numeric that ever leaks is starting wealth, and only as a coarse
//!   bucket the IPC layer interprets (default = 0).

use std::collections::BTreeMap;

use crate::player_state::PlayerState;
use crate::schema::WorldSchema;

// For the custom `deserialize_neighbor_ids` salvager (neighbor shape tolerance).
use serde::de;

// ---------------------------------------------------------------------------
// DraftUpdate — the wire shape the Scribe's `sim_draft` tool emits.
// ---------------------------------------------------------------------------

/// One incremental mutation the Scribe applies to the draft. The
/// `#[serde(tag = "type")]` discriminator matches the JSON the tool-call
/// parser produces; field names are snake_case + kebab-tolerant (the parser
/// is lenient — see `tools::parse_args_lenient`).
///
/// Deliberately coarse: one `SetField` variant handles every scalar card field
/// (the Scribe passes `field: "name" | "setting" | "tone" | "opening_scene" |
/// "player_name" | "core_persona"`), while `AddNpc` / `AddTrait` /
/// `AddActivity` / `AddEntity` handle the collection fields. This keeps the
/// tool surface to a single batched call per scribe turn (one `sim_draft` with
/// a `Vec<DraftUpdate>`) — the §11.26 finding (Gemma 12B emits unquoted JSON
/// keys) made granular per-field tools fragile.
///
/// **Anti-positivity-bias contract (§11.29, load-bearing, hardened):** the
/// player is ALWAYS "User" unless they give an explicit name — never a titled
/// default. The deep reason is **anti-sycophancy**: instruction-tuned models
/// carry a strong prior toward being helpful + supportive, and titles like
/// "hero", "protagonist", "chosen one" activate that prior's "help the user
/// succeed" subspace. Once activated, the model coddles the player — enemies
/// pull punches, NPCs become quest dispensers, the world reshapes itself
/// around the player's success. That breaks simulation: a real world doesn't
/// care that you're the player. The neutral "User" / "the player" / "you"
/// framing keeps the player as *one agent among many*, subject to the same
/// physics + social friction as everyone else — no semantic hook for the
/// model to grab onto for special treatment.
///
/// This is why the banned words are purged ENTIRELY from the system — field
/// names, XML tags, prose, prompts, even "don't say X" prohibitions. The
/// words themselves are the contaminant regardless of framing: a "never call
/// the player 'hero'" clause still surfaces "hero" in the model's context and
/// can trigger the very bias it prohibits. The Scribe surfaces `player_name`
/// only when the player volunteers one; absent that, the card's `<player_name>`
/// field ships with "User".
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DraftUpdate {
    /// Set a scalar card field. `field` is one of: `name`, `setting`, `tone`,
    /// `opening_scene`, `player_name`, `core_persona`. Unknown fields
    /// reject (validation catches the typo).
    SetField { field: String, value: String },
    /// Append a trait bullet to `<traits>`. Idempotent on exact match.
    AddTrait { value: String },
    /// Append an alternate opening greeting to `<introductions>` (SillyTavern
    /// `alternate_greetings`). Idempotent on exact match. The primary opening
    /// is `opening_scene`; these are the swipeable extras.
    AddIntroduction { value: String },
    /// Register a starting NPC id (also stubs a `char.<id>.name` world entity
    /// so the narrator sees the NPC on turn one). Idempotent on exact match.
    AddNpc { id: String },
    /// Append an activity bullet to `<activities>`. Idempotent on exact match.
    AddActivity { value: String },
    /// Set/overwrite a world-schema entity (e.g. `key: "loc.tavern"`,
    /// `state: "warm, half-full"`). The Scribe is responsible for
    /// following the fable.codex namespace conventions.
    AddEntity { key: String, state: String },
    /// Free-form player background (becomes a `char.<player_stem>.background`
    /// world entity at finalize). Overwrites any prior value.
    SetPlayerBackground { value: String },
    /// Qualitative starting condition ("exhausted from the road", "nursing a
    /// wound"). Becomes a `char.<player_stem>.condition` world entity. The
    /// IPC layer may interpret well-known tokens ("injured", "exhausted") to
    /// seed actual `PlayerState` fields; otherwise it stays a diegetic note.
    SetStartingCondition { value: String },
    /// Set the starting travel-graph node id (Phase 4 Component 3 forward-compat;
    /// the draft captures it if the Scribe extracts it, `interview_finalize`
    /// stamps it onto `WorldSchema.travel_graph.current_node`). Optional.
    SetStartNode { value: String },
    /// Set the WHOLE travel-graph in one idempotent overwrite (Phase 4 Component
    /// 3). The Scribe emits the full graph in a single coherent call — no
    /// cross-update node-id ordering, no forward-reference fragility (the
    /// robust shape for a local 12B). `to_sim_card_xml` serializes this to the
    /// `<locations>` block; `enter_fable_session` seeds `WorldSchema
    /// .travel_graph` from it (first node = `current_node`, the player's start).
    SetLocations { nodes: Vec<DraftNode> },
}

/// A location node as authored by the Scribe via `SetLocations`. Mirrors the
/// `sim_card::CardNode` shape (id/name/neighbors/setting) 1:1 but kept in
/// `interview_draft` so the draft module stays decoupled from `sim_card` — the
/// conversion is a struct-literal copy at `to_sim_card_xml` time. `#[serde
/// (default)]` on the collection + setting fields so the Scribe's JSON can omit
/// `setting`/`neighbors` when empty (the parser tolerates both).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
pub struct DraftNode {
    /// Bare slug ("tavern", "cellar") — NOT "node.tavern" (the parser strips
    /// that prefix for ergonomics, but the authoring convention is bare).
    pub id: String,
    /// Diegetic prose label shown to the narrator on the `location:` line.
    pub name: String,
    /// Reachable neighbor node ids (bare slugs). Pure adjacency — each side of
    /// an undirected edge must list the other (the parser does NOT symmetrize).
    ///
    /// Deserialized via `deserialize_neighbor_ids` — a custom visitor that
    /// tolerates the local 12B's two emission shapes for this field: the
    /// correct form (`["cellar","market"]` — bare id strings) AND the runaway-
    /// recursion form it sometimes produces (`[{id:"cellar",neighbors:[{id:...
    /// neighbors:[...]}]}]` — full node objects nested inside neighbors). The
    /// visitor salvages the latter: for an object entry it extracts the `id`
    /// and recurses into any nested `neighbors` to harvest their ids too, then
    /// discards the rest. This converts a parse failure into a silent salvage
    /// at zero LLM cost (no retry loop, no prompt nagging — the PROMPT-CODEX
    /// mechanical-tolerance discipline). `json_repair` upstream closes the
    /// dangling brackets when the recursion hits max_tokens, so the salvager
    /// always sees a parseable structure.
    #[serde(default, deserialize_with = "deserialize_neighbor_ids")]
    pub neighbors: Vec<String>,
    /// `"indoor"` / `"outdoor"` / empty. Gates whether the global `weather:`
    /// line renders for this node (the only node→weather coupling in v1).
    #[serde(default)]
    pub setting: String,
}

/// Custom deserializer for `DraftNode::neighbors`. Tolerates BOTH shapes the
/// local Gemma 12B emits (the 2026-07-29 WEAVER playtest finding):
///
/// - **Correct:** `["cellar", "market"]` — an array of bare id strings.
/// - **Runaway recursion:** `[{id:"cellar", neighbors:[{id:"market", neighbors:
///   [...]}]}]` — the model nests full node objects inside `neighbors`
///   (conflating "neighbor = an id" with "neighbor = the node definition"),
///   recursing until max_tokens.
///
/// The salvager: for a string entry → push it. For an object entry → extract
/// its `id` (push it) AND recurse into the object's own `neighbors` array to
/// harvest those ids too, then discard everything else (name/setting/recurse-
/// depth). This maximizes salvage from even a badly-runaway output: after
/// `json_repair` closes the dangling brackets, every reachable `id` at every
/// nesting depth is collected. Duplicates are deduped (a node listing the same
/// neighbor twice, or a recursion that revisits an id, yields one entry).
///
/// This is the mechanical-tolerance answer (PROMPT-CODEX discipline): no
/// prompt nagging, no retry loop, no latency. Rust absorbs the model's shape
/// confusion at the parse boundary.
fn deserialize_neighbor_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: de::Deserializer<'de>,
{
    struct NeighborIdsVisitor;

    impl<'de> de::Visitor<'de> for NeighborIdsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an array of neighbor id strings (objects-with-id tolerated + salvaged)")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Vec<String>, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut out: Vec<String> = Vec::new();
            while let Some(entry) = seq.next_element::<serde_json::Value>()? {
                harvest_neighbor_ids(&entry, &mut out);
            }
            Ok(out)
        }
    }

    deserializer.deserialize_seq(NeighborIdsVisitor)
}

/// Walk one neighbor-array entry (a string OR an object-with-id, possibly
/// carrying its own nested `neighbors`) and collect every reachable id into
/// `out`. Recursive on nested `neighbors` arrays so a runaway-recursion output
/// still yields its full id set. Dedupes against `out` (order-preserving).
fn harvest_neighbor_ids(entry: &serde_json::Value, out: &mut Vec<String>) {
    match entry {
        // The correct shape: a bare id string.
        serde_json::Value::String(s) => {
            let s = s.trim();
            if !s.is_empty() && !out.iter().any(|x| x == s) {
                out.push(s.to_string());
            }
        }
        // The recursion shape: an object (with an `id` + maybe nested
        // `neighbors`). Salvage the id, then recurse to harvest deeper ids.
        serde_json::Value::Object(obj) => {
            if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
                let id = id.trim();
                if !id.is_empty() && !out.iter().any(|x| x == id) {
                    out.push(id.to_string());
                }
            }
            // Recurse into nested neighbors (the recursion source). Each
            // nesting level's ids get harvested too — we want the full set
            // of places the model was trying to connect, not just the
            // outermost. A nested `neighbors` that's itself an array of
            // objects/strings recurses through harvest_neighbor_ids per entry.
            if let Some(nested) = obj.get("neighbors") {
                if let Some(arr) = nested.as_array() {
                    for e in arr {
                        harvest_neighbor_ids(e, out);
                    }
                }
            }
        }
        // Numbers/bools/null/arrays-at-this-position: not a valid neighbor
        // entry. Silently skip (don't fail the whole parse over one bad entry).
        _ => {}
    }
}

/// The scalar fields `SetField` accepts. anything else rejects at validation.
pub const SETFIELD_FIELDS: &[&str] = &[
    "name",
    "setting",
    "tone",
    "opening_scene",
    "player_name",
    "core_persona",
    "appearance",
];

/// The default player name when the player hasn't volunteered one. Per §11.29
/// the player is ALWAYS "User" — never anything else. This is the value
/// stamped into the card's `<player_name>` field.
pub const DEFAULT_PLAYER_NAME: &str = "User";

// ---------------------------------------------------------------------------
// InterviewDraft — the building card.
// ---------------------------------------------------------------------------

/// The in-progress SimCard + world + player seeds assembled during an
/// interview. Held under `AppState::interview_draft` as
/// `Arc<Mutex<Option<InterviewDraft>>>`; `None` when no interview is active.
///
/// All fields `Option`/collection so the draft starts empty (the GM hasn't
/// asked anything yet) and fills incrementally as the Scribe extracts facts.
/// `last_updated_turn` is a forensic counter (how many scribe turns have
/// applied updates); not load-bearing for logic but useful for debugging.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct InterviewDraft {
    // --- Card fields (mirror SimCard's roleplay-relevant subset) ---
    pub name: Option<String>,
    pub core_persona: Option<String>,
    /// Physical description of the world's central NPC / the player character /
    /// the setting's defining figure. Added 2026-07-29 (Gemini hard ruling #2:
    /// "do not drop appearance" — ST users spend hours crafting exact physical
    /// descriptions for image generation; dropping them on import is a data-
    /// loss riot). Serialized to `<appearance>` by `to_sim_card_xml`.
    pub appearance: Option<String>,
    pub traits: Vec<String>,
    pub setting: Option<String>,
    pub tone: Option<String>,
    pub opening_scene: Option<String>,
    /// Alternate opening messages (the SillyTavern `alternate_greetings` field
    /// from an imported card). Added 2026-07-29. Serialized to `<introductions>`
    /// by `to_sim_card_xml`; consumed downstream by the swipeable-variant UX
    /// (the same model the reroll-swipe feature uses) so an imported card with
    /// several greetings surfaces them as ‹ 1/N › swipeable intro variants.
    /// `opening_scene` (index 0) is always the primary; these are the extras.
    pub introductions: Vec<String>,
    /// The player's chosen name, if they volunteered one. `None` until the
    /// Scribe extracts a name from the conversation. **Per §11.29 (load-
    /// bearing): the player is ALWAYS "User" — never defaulted to anything
    /// else.** At finalize, a `None` here stamps `DEFAULT_PLAYER_NAME`
    /// ("User") into the card's `<player_name>` field.
    pub player_name: Option<String>,
    pub start_npc_ids: Vec<String>,
    pub declared_activities: Vec<String>,

    // --- World schema seeds ---
    /// One-line world summary (becomes `WorldSchema.summary`). Optional; if
    /// absent the narrator opens without a summary anchor.
    pub world_summary: Option<String>,
    /// Flat entity map (becomes `WorldSchema.entities`). Ordered for
    /// deterministic test output; the IPC layer HashMap-ifies at finalize.
    pub world_entities: BTreeMap<String, String>,
    /// Optional starting travel-graph node id (Phase 4 Component 3).
    pub start_node: Option<String>,
    /// The authored travel-graph locations (Phase 4 Component 3). Set wholesale
    /// by `SetLocations` (idempotent overwrite — the Scribe emits the full graph
    /// in one coherent call). Serialized to `<locations>` by `to_sim_card_xml`;
    /// seeded into `WorldSchema.travel_graph` by `enter_fable_session` (first
    /// node = `current_node`). Without this, `[TRAVEL]` is always rejected
    /// ("unknown destination" — nodes empty) + `[RUMOR]` always dropped (no
    /// current node) → Components 3 + 4 dead in live play (the §11.48 failure
    /// recurring upstream for WEAVER-generated cards).
    pub locations: Vec<DraftNode>,

    // --- Player state seeds ---
    pub player_background: Option<String>,
    pub starting_condition: Option<String>,

    /// Forensic: how many scribe turns have applied updates. Not load-bearing.
    pub last_updated_turn: usize,
}

impl InterviewDraft {
    /// Apply a batch of updates atomically. Either all succeed (returns Ok)
    /// or the draft is untouched (the validation pass runs before any
    /// mutation). Mirrors the all-or-nothing contract the scribe's tool
    /// `execute()` relies on: a partially-applied batch would leave the draft
    /// in a confusing half-state for the live preview.
    pub fn apply_updates(&mut self, updates: Vec<DraftUpdate>) -> Result<(), String> {
        // Validate the whole batch first (cheap; no mutation yet).
        for u in &updates {
            validate_update(u)?;
        }
        // All valid → apply.
        for u in updates {
            self.apply_one(u);
        }
        self.last_updated_turn = self.last_updated_turn.saturating_add(1);
        Ok(())
    }

    fn apply_one(&mut self, update: DraftUpdate) {
        match update {
            DraftUpdate::SetField { field, value } => match field.as_str() {
                "name" => self.name = Some(value),
                "setting" => self.setting = Some(value),
                "tone" => self.tone = Some(value),
                "opening_scene" => self.opening_scene = Some(value),
                "player_name" => self.player_name = Some(value),
                "core_persona" => self.core_persona = Some(value),
                "appearance" => self.appearance = Some(value),
                _ => unreachable!("validate_update gates SetField fields"),
            },
            DraftUpdate::AddTrait { value } => {
                if !self.traits.iter().any(|t| t == &value) {
                    self.traits.push(value);
                }
            }
            DraftUpdate::AddIntroduction { value } => {
                if !self.introductions.iter().any(|i| i == &value) {
                    self.introductions.push(value);
                }
            }
            DraftUpdate::AddNpc { id } => {
                if !self.start_npc_ids.iter().any(|n| n == &id) {
                    self.start_npc_ids.push(id.clone());
                    // Stub the world entity so the narrator sees the NPC on
                    // turn one. The Scribe can refine via AddEntity later.
                    self.world_entities
                        .entry(format!("char.{}.name", id))
                        .or_insert_with(|| id.replace('_', " "));
                }
            }
            DraftUpdate::AddActivity { value } => {
                if !self.declared_activities.iter().any(|a| a == &value) {
                    self.declared_activities.push(value);
                }
            }
            DraftUpdate::AddEntity { key, state } => {
                self.world_entities.insert(key, state);
            }
            DraftUpdate::SetPlayerBackground { value } => {
                self.player_background = Some(value);
            }
            DraftUpdate::SetStartingCondition { value } => {
                self.starting_condition = Some(value);
            }
            DraftUpdate::SetStartNode { value } => {
                self.start_node = Some(value);
            }
            DraftUpdate::SetLocations { nodes } => {
                // Idempotent overwrite: the Scribe emits the WHOLE graph in one
                // call (the robust shape for a local 12B — no incremental merge
                // complexity, no cross-update ordering). Refinement = re-emit.
                self.locations = nodes;
            }
        }
    }

    /// The sanitized card id (lowercased, non-alphanumeric → dashes). Mirrors
    /// `codex::sanitize_stem` so the on-disk filename + the `<metadata><id>`
    /// stay in lockstep with how `create_sim_card` names files. `None` when
    /// no name is set yet (the draft isn't finalizable).
    pub fn card_id(&self) -> Option<String> {
        self.name.as_ref().map(|n| sanitize_stem(n))
    }

    /// Completion percentage for the UI progress bar. Counts how many of the
    /// 6 "core" slots are filled: name, setting, tone, opening_scene,
    /// player_name, + (at least one NPC OR a world_summary). The
    /// `is_finalizable` gate checks the first three; this is a richer signal
    /// for the preview's progress indicator. NOTE: `player_name` is NOT a
    /// finalization requirement (defaults to "User" per §11.29) — it just adds
    /// to the progress bar when the player volunteers one.
    pub fn completion_pct(&self) -> u8 {
        let mut filled = 0u8;
        let mut total = 6u8;
        if self.name.is_some() {
            filled += 1;
        }
        if self.setting.is_some() {
            filled += 1;
        }
        if self.tone.is_some() {
            filled += 1;
        }
        if self.opening_scene.is_some() {
            filled += 1;
        }
        if self.player_name.is_some() {
            filled += 1;
        }
        if !self.start_npc_ids.is_empty() || self.world_summary.is_some() {
            filled += 1;
        }
        // Optional slots that add to the bar but don't gate finalization —
        // they round out the card but a great card can ship without them.
        total += 4;
        if self.core_persona.is_some() {
            filled += 1;
        }
        if self.player_background.is_some() {
            filled += 1;
        }
        if !self.locations.is_empty() {
            filled += 1;
        }
        if self.appearance.is_some() {
            filled += 1;
        }
        ((filled as u32 * 100) / (total as u32)).min(100) as u8
    }

    /// The minimum bar to finalize: a name + a setting. (Player name is NOT
    /// required — it defaults to "User" per §11.29; the player is NEVER
    /// forced to name a character.) A card without name + setting is not a
    /// playable scenario. The IPC layer (`interview_finalize`) rejects with a
    /// missing-fields error when this returns false, listing what's missing.
    pub fn is_finalizable(&self) -> bool {
        self.name.is_some() && self.setting.is_some()
    }

    /// List the missing required fields (for the finalize error message).
    pub fn missing_required(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.name.is_none() {
            missing.push("name");
        }
        if self.setting.is_none() {
            missing.push("setting");
        }
        missing
    }

    /// The player-facing name to stamp into the card's `<player_name>` field.
    /// Returns the player's chosen name if set, otherwise `DEFAULT_PLAYER_NAME`
    /// ("User"). Per §11.29 the player is ALWAYS "User" — never anything else.
    pub fn effective_player_name(&self) -> &str {
        self.player_name.as_deref().unwrap_or(DEFAULT_PLAYER_NAME)
    }

    /// Build the `.sim` file XML. Mirrors the shape of
    /// `apps/fable/cards/rusty_tavern.sim` exactly: `<metadata>` (id + type),
    /// `<identity>` (name + core_persona + traits), `<scenario>` (setting +
    /// tone + player_name + opening_scene + start_npcs + activities). CDATA-
    /// wraps every prose block so smart quotes / angle brackets parse cleanly
    /// (the same contract `wupi.sim` / `fable.sim` / `rusty_tavern.sim` rely on).
    ///
    /// Returns `Err` if `card_id()` is `None` (no name set) — the caller
    /// (`interview_finalize`) gates on `is_finalizable` first, so this is a
    /// defensive guard. The output is smoke-validated via `roxmltree` before
    /// return: if Rust ever emits malformed XML (a bug), we catch it here
    /// rather than letting `sim_card::parse_from_xml_str` fail at load.
    pub fn to_sim_card_xml(&self) -> Result<String, String> {
        let id = self.card_id().ok_or_else(|| {
            "cannot build sim card XML without a name (card id derives from name)".to_string()
        })?;
        let name = self.name.clone().unwrap_or_default();
        let mut out = String::with_capacity(2048);
        out.push_str("<sim_card>\n");
        // <metadata>
        out.push_str("  <metadata>\n");
        out.push_str(&format!("    <id>{}</id>\n", escape_text(&id)));
        out.push_str("    <type>roleplay</type>\n");
        out.push_str("  </metadata>\n\n");
        // <identity>
        out.push_str("  <identity>\n");
        out.push_str(&format!("    <name>{}</name>\n", escape_text(&name)));
        if let Some(p) = &self.core_persona {
            out.push_str("    <core_persona><![CDATA[");
            out.push_str(p.trim());
            out.push_str("]]></core_persona>\n");
        }
        if !self.traits.is_empty() {
            out.push_str("    <traits><![CDATA[\n");
            for t in &self.traits {
                out.push_str("- ");
                out.push_str(t.trim());
                out.push('\n');
            }
            out.push_str("]]></traits>\n");
        }
        out.push_str("  </identity>\n\n");
        // <appearance> — the physical description (Gemini ruling #2: never drop
        // on import). Root-level (sibling of <identity>, NOT nested in it) —
        // `sim_card::parse` reads `first_child(root, "appearance")`. The parser
        // renders `<appearance>` element children as `tag: text` lines, so we
        // wrap the prose in a single `<description>` child to preserve it as a
        // coherent block. Round-trips as `description: <prose>` on re-parse.
        if let Some(a) = &self.appearance {
            if !a.trim().is_empty() {
                out.push_str("  <appearance>\n");
                out.push_str("    <description><![CDATA[");
                out.push_str(a.trim());
                out.push_str("]]></description>\n");
                out.push_str("  </appearance>\n\n");
            }
        }
        // <scenario>
        out.push_str("  <scenario>\n");
        if let Some(s) = &self.setting {
            out.push_str("    <setting><![CDATA[");
            out.push_str(s.trim());
            out.push_str("]]></setting>\n");
        }
        if let Some(t) = &self.tone {
            out.push_str("    <tone><![CDATA[");
            out.push_str(t.trim());
            out.push_str("]]></tone>\n");
        }
        // ALWAYS emit <player_name> — defaults to "User" per §11.29 when the
        // player hasn't volunteered a name. The card never ships without one.
        out.push_str("    <player_name><![CDATA[");
        out.push_str(self.effective_player_name().trim());
        out.push_str("]]></player_name>\n");
        if let Some(o) = &self.opening_scene {
            out.push_str("    <opening_scene><![CDATA[");
            out.push_str(o.trim());
            out.push_str("]]></opening_scene>\n");
        }
        if !self.start_npc_ids.is_empty() {
            out.push_str("    <start_npcs><![CDATA[\n");
            for n in &self.start_npc_ids {
                out.push_str("- ");
                out.push_str(n.trim());
                out.push('\n');
            }
            out.push_str("]]></start_npcs>\n");
        }
        if !self.declared_activities.is_empty() {
            out.push_str("    <activities><![CDATA[\n");
            for a in &self.declared_activities {
                out.push_str("- ");
                out.push_str(a.trim());
                out.push('\n');
            }
            out.push_str("]]></activities>\n");
        }
        // Phase 4 Component 3: the <locations> travel-graph block. Emitted only
        // when the Scribe authored a graph (a draft with no SetLocations call
        // produces no block — back-compat with pre-Phase-4 cards). Matches the
        // parser's expected shape exactly: <node id=... setting=...> attributes
        // + <name>/<neighbor> children. Empty `setting` omits the attribute
        // (parser defaults to ""). `escape_text` keeps both attribute + text
        // values safe (smart quotes, angle brackets). This is the load-bearing
        // emission — without it, enter_fable_session's seeding block sees an
        // empty card.locations and Components 3+4 stay dead (§11.48 upstream).
        if !self.locations.is_empty() {
            out.push_str("    <locations>\n");
            for n in &self.locations {
                out.push_str("      <node id=\"");
                out.push_str(&escape_text(&n.id));
                out.push('"');
                if !n.setting.trim().is_empty() {
                    out.push_str(" setting=\"");
                    out.push_str(&escape_text(n.setting.trim()));
                    out.push('"');
                }
                out.push_str(">\n");
                out.push_str("        <name>");
                out.push_str(&escape_text(n.name.trim()));
                out.push_str("</name>\n");
                for nb in &n.neighbors {
                    let nb = nb.trim();
                    if nb.is_empty() {
                        continue;
                    }
                    out.push_str("        <neighbor>");
                    out.push_str(&escape_text(nb));
                    out.push_str("</neighbor>\n");
                }
                out.push_str("      </node>\n");
            }
            out.push_str("    </locations>\n");
        }
        out.push_str("  </scenario>\n");
        // <introductions> — alternate opening greetings (SillyTavern
        // `alternate_greetings` from an imported card). Root-level CDATA bullet
        // list, parsed identically to the start_npcs/activities lists. Emitted
        // only when the draft carries extras. The primary opening is
        // <opening_scene> above; these are the swipeable alternates.
        if !self.introductions.is_empty() {
            out.push_str("  <introductions><![CDATA[\n");
            for intro in &self.introductions {
                let intro = intro.trim();
                if intro.is_empty() {
                    continue;
                }
                out.push_str("- ");
                out.push_str(intro);
                out.push('\n');
            }
            out.push_str("]]></introductions>\n");
        }
        out.push_str("</sim_card>\n");

        // Smoke-validate: roxmltree parses the whole document. A bug in the
        // builder above (unclosed tag, bad CDATA) surfaces here, NOT at
        // `sim_card::parse_from_xml_str` time. CDATA content is opaque to
        // roxmltree so prose can't break this.
        roxmltree::Document::parse(&out)
            .map_err(|e| format!("to_sim_card_xml produced malformed XML (bug): {e}"))?;
        Ok(out)
    }

    /// Build the starting `WorldSchema` from the draft. Seeds `summary` +
    /// `entities` (the narrator sees these as hard facts on turn one). The
    /// player's name is NOT stamped here — it lives on the card's
    /// `<player_name>` and reaches the narrator via the narrator prompt. The
    /// background + condition entities (if set) ARE seeded so the narrator
    /// opens on the player's established texture.
    ///
    /// Mirrors the seeding block in `fable_quick_start` (lib.rs:7758-7783):
    /// the schema starts at `Default::default()` then summary/entities are
    /// overridden. `player_state` is set separately via [`Self::to_player_state`].
    pub fn to_world_schema(&self) -> WorldSchema {
        let mut schema = WorldSchema::default();
        if let Some(s) = &self.world_summary {
            schema.summary = s.trim().to_string();
        }
        // Copy the draft's entity map into the schema's HashMap.
        for (k, v) in &self.world_entities {
            schema.entities.insert(k.clone(), v.clone());
        }
        // Stamp the player's background + condition as entities so the
        // narrator opens on the player's established texture (per fable.codex
        // "Player State Seeding": background + qualitative condition seed the
        // opening scene's texture). Keyed off the effective player name
        // ("User" when the player hasn't volunteered one — §11.29).
        let player_stem = sanitize_stem(self.effective_player_name());
        if let Some(bg) = &self.player_background {
            let key = format!("char.{}.background", player_stem);
            schema.entities.insert(key, bg.trim().to_string());
        }
        if let Some(cond) = &self.starting_condition {
            let key = format!("char.{}.condition", player_stem);
            schema.entities.insert(key, cond.trim().to_string());
        }
        schema
    }

    /// Build the starting `PlayerState` from the draft. Default = fully
    /// healthy, fresh stamina, zero wealth. The draft's qualitative
    /// `starting_condition` is interpreted here ONLY for well-known tokens
    /// ("exhausted" → stamina drop, "injured"/"wounded" → a body part set to
    /// a wounded tier). Everything else stays a diegetic note in the world
    /// schema (the IPC layer passes it through; the narrator paints it).
    ///
    /// Per fable.codex "Player State Seeding": do NOT prompt for stats, HP, or
    /// skill numbers. The body model + condition are Rust-owned; the player
    /// never sees them. A player saying "I want to start strong" becomes a
    /// background note, not a stat bump.
    pub fn to_player_state(&self) -> PlayerState {
        let mut state = PlayerState::default();
        if let Some(cond) = self.starting_condition.as_ref() {
            let lower = cond.to_ascii_lowercase();
            use crate::player_state::{BodyPart, BodyPartState, Stamina};
            // Exhaustion: drop stamina. The exact tier mapping is a v1
            // heuristic; the narrator renders the qualitative result.
            if lower.contains("exhaust") || lower.contains("tired") || lower.contains("weary") {
                state.stamina = Stamina::Exhausted;
            } else if lower.contains("fatigue") || lower.contains("drained") {
                state.stamina = Stamina::Winded;
            }
            // Injury: mark one body part as wounded. We don't know which, so
            // pick a generic "torso" wound (the most common narrative default;
            // the Referee's injury model treats all parts uniformly). The
            // narrator renders the qualitative wound.
            if lower.contains("injur") || lower.contains("wound") || lower.contains("hurt") {
                state.body.insert(BodyPart::Torso, BodyPartState::Yellow);
            }
        }
        state
    }

    /// A compact one-paragraph summary of what's been established, folded
    /// into the GM's system prompt each turn. Compensates for the 6-turn
    /// history window: even when early answers scroll out of the message
    /// array, the GM still knows the draft state. Format:
    ///
    /// ```text
    /// <draft_state>
    /// Name: The Rusty Lantern Tavern
    /// Setting: Night; rain on the shutters...
    /// Tone: Slow-burn.
    /// Player: Kaelen
    /// NPCs: mara_the_innkeep, the_hooded_stranger
    /// Activities: conversation, exploration
    /// Background: a traveling herbalist, three days on the road
    /// Condition: exhausted from the road
    /// </draft_state>
    /// ```
    ///
    /// The `Player:` line ALWAYS renders once any other slot is filled — it
    /// shows the effective player name ("User" by default per §11.29, or the
    /// name the player volunteered). Only filled slots render otherwise.
    /// Returns `None` when the draft is completely empty (the very first GM
    /// turn — nothing to summarize yet).
    pub fn render_state_summary(&self) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        if let Some(n) = &self.name {
            lines.push(format!("Name: {}", n.trim()));
        }
        if let Some(s) = &self.setting {
            lines.push(format!("Setting: {}", truncate_for_summary(s)));
        }
        if let Some(t) = &self.tone {
            lines.push(format!("Tone: {}", truncate_for_summary(t)));
        }
        // Player line always renders (defaults to "User"). The GM benefits
        // from always knowing the player handle.
        lines.push(format!("Player: {}", self.effective_player_name().trim()));
        if !self.start_npc_ids.is_empty() {
            lines.push(format!("NPCs: {}", self.start_npc_ids.join(", ")));
        }
        if !self.declared_activities.is_empty() {
            lines.push(format!("Activities: {}", self.declared_activities.join(", ")));
        }
        if let Some(b) = &self.player_background {
            lines.push(format!("Background: {}", truncate_for_summary(b)));
        }
        if let Some(a) = &self.appearance {
            lines.push(format!("Appearance: {}", truncate_for_summary(a)));
        }
        if !self.introductions.is_empty() {
            lines.push(format!("Alternate openings: {}", self.introductions.len()));
        }
        if let Some(c) = &self.starting_condition {
            lines.push(format!("Condition: {}", truncate_for_summary(c)));
        }
        if !self.locations.is_empty() {
            // Surface authored geography so the GM knows what's reachable
            // (and can ask follow-ups about adjacent areas). Ids only — the
            // diegetic names live on the card, not in this compact summary.
            lines.push(format!(
                "Locations: {}",
                self.locations.iter().map(|n| n.id.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if lines.is_empty() {
            return None;
        }
        let mut out = String::from("<draft_state>\n");
        out.push_str(&lines.join("\n"));
        out.push_str("\n</draft_state>");
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Validation + helpers
// ---------------------------------------------------------------------------

/// Validate one update before applying. Returns `Err(message)` on any
/// violation; the batch-apply contract is "all valid or none applied."
fn validate_update(u: &DraftUpdate) -> Result<(), String> {
    match u {
        DraftUpdate::SetField { field, value } => {
            if !SETFIELD_FIELDS.iter().any(|f| f == field) {
                return Err(format!(
                    "SetField field '{}' is not one of: {}",
                    field,
                    SETFIELD_FIELDS.join(", ")
                ));
            }
            if value.trim().is_empty() {
                return Err(format!("SetField field '{}' value is empty", field));
            }
        }
        DraftUpdate::AddTrait { value }
        | DraftUpdate::AddIntroduction { value }
        | DraftUpdate::AddActivity { value }
        | DraftUpdate::SetPlayerBackground { value }
        | DraftUpdate::SetStartingCondition { value }
        | DraftUpdate::SetStartNode { value } => {
            if value.trim().is_empty() {
                return Err("value is empty".to_string());
            }
        }
        DraftUpdate::AddNpc { id } => {
            if id.trim().is_empty() {
                return Err("AddNpc id is empty".to_string());
            }
        }
        DraftUpdate::AddEntity { key, state } => {
            if key.trim().is_empty() {
                return Err("AddEntity key is empty".to_string());
            }
            if state.trim().is_empty() {
                return Err(format!("AddEntity state for key '{}' is empty", key));
            }
        }
        DraftUpdate::SetLocations { nodes } => {
            if nodes.is_empty() {
                return Err("SetLocations nodes is empty".to_string());
            }
            // Shallow per-node check: every node needs a non-empty id (the
            // parser drops id-less nodes defensively; rejecting here surfaces
            // the bug to the Scribe instead). name may be empty (the narrator
            // can paint an unnamed location). neighbors entries must be
            // non-empty slugs. We deliberately do NOT cross-check neighbor ids
            // against node ids here — the Scribe may emit a graph whose nodes
            // list each other, and validating within-batch ordering would force
            // fragility. Downstream `enter_fable_session` seeding is tolerant
            // of dangling neighbors (`find_node` just won't match).
            for (i, n) in nodes.iter().enumerate() {
                if n.id.trim().is_empty() {
                    return Err(format!("SetLocations nodes[{i}].id is empty"));
                }
                for (j, nb) in n.neighbors.iter().enumerate() {
                    if nb.trim().is_empty() {
                        return Err(format!(
                            "SetLocations nodes[{i}].neighbors[{j}] is empty"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Escape text for inline XML (used in `<id>` and `<name>` which are NOT
/// CDATA-wrapped). Only the four essential entities; apostrophes + quotes are
/// legal in XML text content and `roxmltree` handles them.
fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Sanitize a name into a filesystem-safe + id-safe stem. Mirrors
/// `codex::sanitize_stem`: lowercase, non-alphanumeric → dashes, collapse
/// repeats, trim leading/trailing dashes. Used for both the card id and the
/// on-disk filename (`apps/fable/cards/<stem>.sim`).
pub fn sanitize_stem(s: &str) -> String {
    let mut out: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of dashes + trim leading/trailing.
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Truncate a prose string for the state summary (keeps the prompt lean —
/// the GM doesn't need the full setting, just enough to remember what's
/// established). 120 chars is a rough sentence-and-a-half cap.
fn truncate_for_summary(s: &str) -> String {
    const CAP: usize = 120;
    let trimmed = s.trim();
    if trimmed.chars().count() <= CAP {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(CAP).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn draft_with_basics() -> InterviewDraft {
        // A minimal finalizable draft (name + setting). Player name is
        // optional — defaults to "User" per §11.29.
        InterviewDraft {
            name: Some("The Rusty Lantern Tavern".to_string()),
            setting: Some("Night; rain on the shutters.".to_string()),
            player_name: Some("Kaelen".to_string()),
            ..Default::default()
        }
    }

    // --- apply_updates + validation ---

    #[test]
    fn apply_setfield_name_populates_name() {
        let mut d = InterviewDraft::default();
        d.apply_updates(vec![DraftUpdate::SetField {
            field: "name".into(),
            value: "  The Neon Dragon  ".into(),
        }])
        .unwrap();
        assert_eq!(d.name.as_deref(), Some("  The Neon Dragon  "));
        assert_eq!(d.card_id().as_deref(), Some("the-neon-dragon"));
    }

    #[test]
    fn apply_setfield_rejects_unknown_field() {
        let mut d = InterviewDraft::default();
        let err = d
            .apply_updates(vec![DraftUpdate::SetField {
                field: "hit_points".into(),
                value: "50".into(),
            }])
            .unwrap_err();
        assert!(err.contains("hit_points"));
        // Nothing applied.
        assert!(d.name.is_none());
    }

    #[test]
    fn apply_setfield_rejects_empty_value() {
        let mut d = InterviewDraft::default();
        assert!(d
            .apply_updates(vec![DraftUpdate::SetField {
                field: "name".into(),
                value: "   ".into(),
            }])
            .is_err());
    }

    #[test]
    fn apply_batch_is_atomic_on_partial_invalid() {
        // First update is valid, second is invalid → NONE should apply.
        let mut d = InterviewDraft::default();
        let err = d
            .apply_updates(vec![
                DraftUpdate::SetField {
                    field: "name".into(),
                    value: "Valid".into(),
                },
                DraftUpdate::SetField {
                    field: "bogus".into(),
                    value: "Invalid".into(),
                },
            ])
            .unwrap_err();
        assert!(err.contains("bogus"));
        // The valid SetField did NOT apply (atomic).
        assert!(d.name.is_none());
        assert_eq!(d.last_updated_turn, 0);
    }

    #[test]
    fn add_npc_is_idempotent_and_stubs_entity() {
        let mut d = InterviewDraft::default();
        d.apply_updates(vec![
            DraftUpdate::AddNpc {
                id: "mara_the_innkeep".into(),
            },
            DraftUpdate::AddNpc {
                id: "mara_the_innkeep".into(),
            },
        ])
        .unwrap();
        assert_eq!(d.start_npc_ids.len(), 1);
        assert_eq!(
            d.world_entities.get("char.mara_the_innkeep.name").map(|s| s.as_str()),
            Some("mara the innkeep")
        );
    }

    #[test]
    fn add_trait_and_activity_are_idempotent() {
        let mut d = InterviewDraft::default();
        d.apply_updates(vec![
            DraftUpdate::AddTrait {
                value: "Measured.".into(),
            },
            DraftUpdate::AddTrait {
                value: "Measured.".into(),
            },
            DraftUpdate::AddActivity {
                value: "conversation".into(),
            },
            DraftUpdate::AddActivity {
                value: "conversation".into(),
            },
        ])
        .unwrap();
        assert_eq!(d.traits.len(), 1);
        assert_eq!(d.declared_activities.len(), 1);
    }

    #[test]
    fn add_entity_overwrites_prior_value() {
        let mut d = InterviewDraft::default();
        d.apply_updates(vec![
            DraftUpdate::AddEntity {
                key: "loc.tavern".into(),
                state: "warm".into(),
            },
            DraftUpdate::AddEntity {
                key: "loc.tavern".into(),
                state: "warm, half-full".into(),
            },
        ])
        .unwrap();
        assert_eq!(
            d.world_entities.get("loc.tavern").map(|s| s.as_str()),
            Some("warm, half-full")
        );
    }

    #[test]
    fn last_updated_turn_increments_per_batch() {
        let mut d = InterviewDraft::default();
        assert_eq!(d.last_updated_turn, 0);
        d.apply_updates(vec![DraftUpdate::AddTrait {
            value: "x".into(),
        }])
        .unwrap();
        assert_eq!(d.last_updated_turn, 1);
        d.apply_updates(vec![DraftUpdate::AddTrait {
            value: "y".into(),
        }])
        .unwrap();
        assert_eq!(d.last_updated_turn, 2);
    }

    // --- completion + finalization ---

    #[test]
    fn empty_draft_is_zero_percent_and_not_finalizable() {
        let d = InterviewDraft::default();
        assert_eq!(d.completion_pct(), 0);
        assert!(!d.is_finalizable());
        assert_eq!(d.missing_required(), vec!["name", "setting"]);
    }

    #[test]
    fn minimal_draft_is_finalizable() {
        let d = draft_with_basics();
        assert!(d.is_finalizable());
        assert!(d.missing_required().is_empty());
        // 3 of 10 slots filled (name + setting + player_name) → 30%.
        // (appearance was added as an optional slot in the 2026-07-29 import
        // extension, bumping the total 9 → 10.)
        assert_eq!(d.completion_pct(), 30);
    }

    #[test]
    fn draft_finalizes_without_player_name_defaulting_to_user() {
        // §11.29 contract: player is ALWAYS "User", never a titled default.
        // A draft with name + setting but NO player_name is still finalizable
        // (player_name is not required), and effective_player_name() is "User".
        let d = InterviewDraft {
            name: Some("The Neon Dragon".to_string()),
            setting: Some("3 AM in the lower arcology.".to_string()),
            ..Default::default()
        };
        assert!(d.is_finalizable(), "finalizes without player_name");
        assert_eq!(d.effective_player_name(), "User");
        // The XML stamps <player_name>User</player_name>.
        let xml = d.to_sim_card_xml().unwrap();
        assert!(xml.contains("<player_name><![CDATA[User]]></player_name>"));
        // The word "protagonist" must not appear anywhere in the output.
        assert!(!xml.to_lowercase().contains("protagonist"));
    }

    #[test]
    fn full_draft_is_100_percent() {
        let mut d = draft_with_basics();
        d.apply_updates(vec![
            DraftUpdate::SetField {
                field: "tone".into(),
                value: "Slow-burn.".into(),
            },
            DraftUpdate::SetField {
                field: "opening_scene".into(),
                value: "The door swings shut.".into(),
            },
            DraftUpdate::SetField {
                field: "core_persona".into(),
                value: "A sandbox tavern.".into(),
            },
            DraftUpdate::SetField {
                field: "appearance".into(),
                value: "Smoke-stained rafters, brass lanterns.".into(),
            },
            DraftUpdate::SetPlayerBackground {
                value: "A traveling herbalist.".into(),
            },
            DraftUpdate::AddNpc {
                id: "mara".into(),
            },
            DraftUpdate::SetLocations {
                nodes: vec![DraftNode {
                    id: "tavern".into(),
                    name: "The Tavern".into(),
                    neighbors: vec![],
                    setting: "indoor".into(),
                }],
            },
        ])
        .unwrap();
        assert_eq!(d.completion_pct(), 100);
    }

    // --- to_sim_card_xml ---

    #[test]
    fn to_sim_card_xml_matches_rusty_tavern_shape() {
        let mut d = draft_with_basics();
        d.apply_updates(vec![
            DraftUpdate::SetField {
                field: "core_persona".into(),
                value: "A sandbox tavern. No fixed plot.".into(),
            },
            DraftUpdate::SetField {
                field: "tone".into(),
                value: "Slow-burn.".into(),
            },
            DraftUpdate::SetField {
                field: "opening_scene".into(),
                value: "The door swings shut behind you, cutting off the cold.".into(),
            },
            DraftUpdate::AddNpc {
                id: "mara_the_innkeep".into(),
            },
            DraftUpdate::AddActivity {
                value: "conversation".into(),
            },
        ])
        .unwrap();
        let xml = d.to_sim_card_xml().unwrap();

        // Well-formed (the builder smoke-validates, but double-check here).
        roxmltree::Document::parse(&xml).expect("xml parses");

        // Shape checks: every expected element is present.
        assert!(xml.contains("<id>the-rusty-lantern-tavern</id>"));
        assert!(xml.contains("<type>roleplay</type>"));
        assert!(xml.contains("<name>The Rusty Lantern Tavern</name>"));
        assert!(xml.contains("<core_persona><![CDATA[A sandbox tavern."));
        assert!(xml.contains("<setting><![CDATA[Night; rain on the shutters.]]></setting>"));
        assert!(xml.contains("<tone><![CDATA[Slow-burn.]]></tone>"));
        assert!(xml.contains("<player_name><![CDATA[Kaelen]]></player_name>"));
        assert!(xml.contains("<start_npcs><![CDATA[\n- mara_the_innkeep\n]]></start_npcs>"));
        assert!(xml.contains("<activities><![CDATA[\n- conversation\n]]></activities>"));
    }

    #[test]
    fn to_sim_card_xml_round_trips_through_sim_card_parser() {
        // The real test: the XML we build must parse via the actual
        // `sim_card::parse_from_xml_str` loader. This is the contract
        // `interview_finalize` depends on.
        let mut d = draft_with_basics();
        d.apply_updates(vec![DraftUpdate::SetField {
            field: "tone".into(),
            value: "Noir.".into(),
        }])
        .unwrap();
        let xml = d.to_sim_card_xml().unwrap();
        let card = crate::sim_card::parse_from_xml_str(&xml).expect("parses");
        assert_eq!(card.id, "the-rusty-lantern-tavern");
        assert_eq!(card.name, "The Rusty Lantern Tavern");
        assert_eq!(card.card_type, "roleplay");
        assert_eq!(card.setting.as_deref(), Some("Night; rain on the shutters."));
        assert_eq!(card.tone.as_deref(), Some("Noir."));
        assert_eq!(card.player_name.as_deref(), Some("Kaelen"));
    }

    #[test]
    fn appearance_and_introductions_round_trip_through_xml() {
        // Gemini hard ruling #2 (2026-07-29): imported appearance prose MUST
        // survive the draft→.sim→parse pipeline (ST users spend hours crafting
        // physical descriptions). Alternate greetings (`alternate_greetings`)
        // surface as swipeable intro variants via the same model. Both must
        // round-trip through to_sim_card_xml + the real parser.
        let mut d = draft_with_basics();
        d.apply_updates(vec![
            DraftUpdate::SetField {
                field: "appearance".into(),
                value: "Tall, raven-haired, with a scar across one eye.".into(),
            },
            DraftUpdate::AddIntroduction {
                value: "The stranger looks up from their drink.".into(),
            },
            DraftUpdate::AddIntroduction {
                value: "Rain lashes the windows as you enter.".into(),
            },
        ])
        .unwrap();
        let xml = d.to_sim_card_xml().unwrap();
        let card = crate::sim_card::parse_from_xml_str(&xml).expect("parses");
        // Appearance survives (rendered as `description: <prose>` by the
        // parser's child-element renderer).
        assert!(
            card.appearance.contains("Tall, raven-haired"),
            "appearance prose must survive the round-trip; got: {}",
            card.appearance
        );
        // Both alternate greetings survived.
        assert_eq!(card.introductions.len(), 2, "both intros persisted");
        assert!(card.introductions[0].contains("The stranger looks up"));
        assert!(card.introductions[1].contains("Rain lashes the windows"));
    }

    // --- SetLocations: the Phase 4 travel-graph authoring path ---

    /// A small 2-node graph with BIDIRECTIONAL edges (tavern↔cellar). This is
    /// the canonical shape the worked example teaches + the CDP playtest
    /// verifies the Scribe actually produces (Gemini's bidirectionality concern:
    /// the parser does NOT symmetrize, so each side must list the other).
    fn sample_bidirectional_graph() -> Vec<DraftNode> {
        vec![
            DraftNode {
                id: "tavern".into(),
                name: "The Rusty Lantern Tavern".into(),
                neighbors: vec!["cellar".into()],
                setting: "indoor".into(),
            },
            DraftNode {
                id: "cellar".into(),
                name: "The Tavern Cellar".into(),
                neighbors: vec!["tavern".into()],
                setting: "indoor".into(),
            },
        ]
    }

    #[test]
    fn set_locations_round_trips_through_sim_card_parser() {
        // The load-bearing test for the WEAVER→Phase 4 unblock: a draft with a
        // SetLocations update must produce XML that (a) parses, (b) lands in
        // SimCard.locations, (c) is in document order with first node as seed.
        // enter_fable_session seeds WorldSchema.travel_graph from card.locations
        // — so if this passes, Components 3+4 are reachable from a WEAVER card.
        let mut d = draft_with_basics();
        d.apply_updates(vec![DraftUpdate::SetLocations {
            nodes: sample_bidirectional_graph(),
        }])
        .unwrap();
        assert_eq!(d.locations.len(), 2, "SetLocations populated the draft field");
        let xml = d.to_sim_card_xml().unwrap();
        let card = crate::sim_card::parse_from_xml_str(&xml).expect("WEAVER XML parses");
        assert_eq!(card.locations.len(), 2, "locations round-tripped into the card");
        // Document order preserved — first node is the seed (current_node).
        assert_eq!(card.locations[0].id, "tavern", "first node = seed");
        assert_eq!(card.locations[0].name, "The Rusty Lantern Tavern");
        assert_eq!(card.locations[0].setting, "indoor");
        assert_eq!(card.locations[0].neighbors, vec!["cellar"]);
        assert_eq!(card.locations[1].id, "cellar");
        assert_eq!(card.locations[1].neighbors, vec!["tavern"], "bidirectional edge");
    }

    #[test]
    fn set_locations_is_idempotent_overwrite() {
        // Refinement = re-emit the whole graph. A second SetLocations replaces,
        // does NOT merge. This is the contract that makes the single-call shape
        // robust for a local 12B (no incremental-merge complexity).
        let mut d = draft_with_basics();
        d.apply_updates(vec![DraftUpdate::SetLocations {
            nodes: sample_bidirectional_graph(),
        }])
        .unwrap();
        assert_eq!(d.locations.len(), 2);
        d.apply_updates(vec![DraftUpdate::SetLocations {
            nodes: vec![DraftNode {
                id: "ship".into(),
                name: "The Starship".into(),
                neighbors: vec![],
                setting: "indoor".into(),
            }],
        }])
        .unwrap();
        assert_eq!(d.locations.len(), 1, "second SetLocations overwrote (not merged)");
        assert_eq!(d.locations[0].id, "ship");
    }

    #[test]
    fn set_locations_validates_rejects_empty_id_and_empty_nodes() {
        let mut d = InterviewDraft::default();
        // Empty nodes vec.
        let err = d
            .apply_updates(vec![DraftUpdate::SetLocations { nodes: vec![] }])
            .unwrap_err();
        assert!(err.contains("empty"), "empty nodes rejected: {err}");
        // Node with empty id.
        let err = d
            .apply_updates(vec![DraftUpdate::SetLocations {
                nodes: vec![DraftNode {
                    id: "  ".into(),
                    name: "x".into(),
                    neighbors: vec![],
                    setting: "".into(),
                }],
            }])
            .unwrap_err();
        assert!(err.contains("id"), "empty node id rejected: {err}");
        // Empty neighbor slug.
        let err = d
            .apply_updates(vec![DraftUpdate::SetLocations {
                nodes: vec![DraftNode {
                    id: "a".into(),
                    name: "A".into(),
                    neighbors: vec!["   ".into()],
                    setting: "".into(),
                }],
            }])
            .unwrap_err();
        assert!(err.contains("neighbor"), "empty neighbor slug rejected: {err}");
        // Nothing applied on any rejection.
        assert!(d.locations.is_empty());
    }

    #[test]
    fn to_sim_card_xml_omits_locations_when_empty() {
        // Back-compat: a draft with no SetLocations produces NO <locations>
        // block (a pre-Phase-4 card shape — the parser yields empty Vec).
        let d = draft_with_basics();
        let xml = d.to_sim_card_xml().unwrap();
        assert!(!xml.contains("<locations>"), "no <locations> when empty");
        let card = crate::sim_card::parse_from_xml_str(&xml).expect("parses");
        assert!(card.locations.is_empty(), "card has no locations");
    }

    #[test]
    fn to_sim_card_xml_emits_locations_with_setting_omitted_when_empty() {
        // A node with empty `setting` must omit the setting= attribute entirely
        // (the parser defaults to ""). Verifies the conditional-attribute path.
        let mut d = draft_with_basics();
        d.apply_updates(vec![DraftUpdate::SetLocations {
            nodes: vec![
                DraftNode {
                    id: "outside".into(),
                    name: "The Road".into(),
                    neighbors: vec![],
                    setting: "outdoor".into(),
                },
                DraftNode {
                    id: "unknown".into(),
                    name: "Unspecified".into(),
                    neighbors: vec![],
                    setting: "".into(),
                },
            ],
        }])
        .unwrap();
        let xml = d.to_sim_card_xml().unwrap();
        // outdoor node carries the attribute; empty-setting node does not.
        assert!(xml.contains(r#"<node id="outside" setting="outdoor">"#));
        assert!(xml.contains(r#"<node id="unknown">"#));
        assert!(!xml.contains(r#"setting="""#));
    }

    // --- deserialize_neighbor_ids: the runaway-recursion salvager (Gemini's
    //     Option 4 — Serde coercion, the WUPI mechanical-tolerance answer) ---

    /// The correct shape: an array of bare id strings. Passes through untouched.
    #[test]
    fn neighbors_string_array_parses_unchanged() {
        let json = r#"{"id":"tavern","name":"T","neighbors":["cellar","market"],"setting":"indoor"}"#;
        let node: DraftNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.neighbors, vec!["cellar", "market"]);
    }

    /// The 2026-07-29 WEAVER failure: the model nested FULL node objects
    /// inside `neighbors` (conflating "neighbor = id" with "neighbor = node
    /// definition"), recursing until max_tokens. After json_repair closes the
    /// brackets, the salvager must harvest every reachable id at every depth
    /// and discard the rest. This is the test that pins the salvage contract.
    #[test]
    fn neighbors_nested_objects_salvaged_to_id_strings() {
        // A 3-level runaway recursion: tavern.neighbors has village_common
        // (full object), whose neighbors has cellar (full object), whose
        // neighbors has tavern (full object). The salvager should yield the
        // deduped id set: [village_common, cellar, tavern].
        let json = r#"{"id":"tavern","name":"T","setting":"indoor","neighbors":[
            {"id":"village_common","name":"VC","setting":"outdoor","neighbors":[
                {"id":"cellar","name":"C","setting":"indoor","neighbors":[
                    {"id":"tavern","name":"T","setting":"indoor","neighbors":[]}
                ]}
            ]}
        ]}"#;
        let node: DraftNode = serde_json::from_str(json).unwrap();
        assert_eq!(
            node.neighbors,
            vec!["village_common", "cellar", "tavern"],
            "nested-object neighbors must be salvaged to their ids (deduped)"
        );
    }

    /// Mixed array (some strings, some objects) — the salvager handles each
    /// entry independently. Mirrors the real model output (it may emit a
    /// correct string for one neighbor + a runaway object for another).
    #[test]
    fn neighbors_mixed_strings_and_objects_salvaged() {
        let json = r#"{"id":"tavern","name":"T","neighbors":[
            "cellar",
            {"id":"market","name":"M","neighbors":[{"id":"square","name":"S"}]}
        ]}"#;
        let node: DraftNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.neighbors, vec!["cellar", "market", "square"]);
    }

    /// An empty neighbors array is valid (a leaf node with no exits).
    #[test]
    fn neighbors_empty_array_parses_to_empty_vec() {
        let json = r#"{"id":"dead_end","name":"DE","neighbors":[]}"#;
        let node: DraftNode = serde_json::from_str(json).unwrap();
        assert!(node.neighbors.is_empty());
    }

    /// An object neighbor WITHOUT an `id` field is silently skipped (not a
    /// valid neighbor reference — don't fail the whole parse over one bad entry).
    #[test]
    fn neighbors_object_without_id_silently_skipped() {
        let json = r#"{"id":"tavern","name":"T","neighbors":[
            "cellar",
            {"name":"no id here","setting":"outdoor"},
            "market"
        ]}"#;
        let node: DraftNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.neighbors, vec!["cellar", "market"]);
    }

    /// Duplicate ids (the recursion revisits tavern) are deduped — one entry.
    #[test]
    fn neighbors_duplicate_ids_deduped() {
        let json = r#"{"id":"tavern","name":"T","neighbors":[
            "cellar","cellar",{"id":"cellar"}
        ]}"#;
        let node: DraftNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.neighbors, vec!["cellar"]);
    }

    #[test]
    fn to_sim_card_xml_without_name_is_err() {
        let d = InterviewDraft::default();
        assert!(d.to_sim_card_xml().is_err());
    }

    #[test]
    fn to_sim_card_xml_escapes_angle_brackets_in_name() {
        // A name with literal <> should not break the XML (escape_text handles
        // the non-CDATA <name> element).
        let mut d = InterviewDraft::default();
        d.apply_updates(vec![
            DraftUpdate::SetField {
                field: "name".into(),
                value: "Quest <Beta> & Test".into(),
            },
            DraftUpdate::SetField {
                field: "setting".into(),
                value: "x".into(),
            },
            DraftUpdate::SetField {
                field: "player_name".into(),
                value: "Hero".into(),
            },
        ])
        .unwrap();
        let xml = d.to_sim_card_xml().unwrap();
        roxmltree::Document::parse(&xml).expect("still well-formed");
        assert!(xml.contains("Quest &lt;Beta&gt; &amp; Test"));
    }

    // --- to_world_schema + to_player_state ---

    #[test]
    fn to_world_schema_seeds_summary_entities_and_player_notes() {
        let mut d = draft_with_basics();
        d.apply_updates(vec![
            DraftUpdate::AddEntity {
                key: "loc.tavern".into(),
                state: "warm, half-full".into(),
            },
            DraftUpdate::SetPlayerBackground {
                value: "A traveling herbalist.".into(),
            },
            DraftUpdate::SetStartingCondition {
                value: "exhausted from the road".into(),
            },
        ])
        .unwrap();
        let schema = d.to_world_schema();
        assert_eq!(schema.summary, ""); // no world_summary set
        assert_eq!(
            schema.entities.get("loc.tavern").map(|s| s.as_str()),
            Some("warm, half-full")
        );
        // Player stubs keyed off the effective player name ("Kaelen" — the
        // player volunteered one here; defaults to "user" stem otherwise).
        assert_eq!(
            schema.entities.get("char.kaelen.background").map(|s| s.as_str()),
            Some("A traveling herbalist.")
        );
        assert_eq!(
            schema.entities.get("char.kaelen.condition").map(|s| s.as_str()),
            Some("exhausted from the road")
        );
    }

    #[test]
    fn to_world_schema_keys_player_notes_off_user_when_no_name() {
        // §11.29: when no player_name volunteered, the player notes key off
        // "User" (sanitized to "user") — never a titled default.
        let mut d = InterviewDraft {
            name: Some("The Neon Dragon".to_string()),
            setting: Some("3 AM.".to_string()),
            ..Default::default()
        };
        d.apply_updates(vec![DraftUpdate::SetPlayerBackground {
            value: "A burnt-out decker.".into(),
        }])
        .unwrap();
        let schema = d.to_world_schema();
        assert_eq!(
            schema.entities.get("char.user.background").map(|s| s.as_str()),
            Some("A burnt-out decker.")
        );
        // No titled-default entity keys leaked in.
        assert!(schema.entities.keys().all(|k| !k.contains("hero")));
        assert!(schema.entities.keys().all(|k| !k.contains("chosen")));
    }

    #[test]
    fn to_player_state_interprets_exhaustion_token() {
        let mut d = draft_with_basics();
        d.apply_updates(vec![DraftUpdate::SetStartingCondition {
            value: "exhausted from the long road".into(),
        }])
        .unwrap();
        let state = d.to_player_state();
        use crate::player_state::Stamina;
        assert_eq!(state.stamina, Stamina::Exhausted);
    }

    #[test]
    fn to_player_state_interprets_injured_token() {
        let mut d = draft_with_basics();
        d.apply_updates(vec![DraftUpdate::SetStartingCondition {
            value: "nursing a wound".into(),
        }])
        .unwrap();
        let state = d.to_player_state();
        use crate::player_state::{BodyPart, BodyPartState};
        assert_eq!(
            state.body.get(&BodyPart::Torso).copied(),
            Some(BodyPartState::Yellow)
        );
    }

    #[test]
    fn to_player_state_default_when_no_condition() {
        let d = draft_with_basics();
        let state = d.to_player_state();
        assert!(state.is_default(), "no condition → fully default player state");
    }

    // --- render_state_summary ---

    #[test]
    fn render_state_summary_for_empty_draft_shows_only_player_user() {
        // §11.29: the Player: line always renders (defaults to "User"). So
        // even an empty draft produces a non-None summary with just that one
        // line — the GM always knows the player handle.
        let d = InterviewDraft::default();
        let summary = d.render_state_summary().expect("empty draft still has Player line");
        assert!(summary.contains("Player: User"));
        assert!(!summary.contains("Name:"));
        assert!(!summary.contains("Setting:"));
    }

    #[test]
    fn render_state_summary_includes_filled_slots_only() {
        let mut d = draft_with_basics();
        // Use a genuinely long background (>120 chars) to exercise truncation.
        let long_bg = "A traveling herbalist from the southern marches, three days on \
                       the road with nothing but a worn satchel and a single copper coin \
                       left to her name, fleeing a misunderstanding that wasn't hers.";
        d.apply_updates(vec![
            DraftUpdate::AddNpc {
                id: "mara".into(),
            },
            DraftUpdate::SetPlayerBackground {
                value: long_bg.to_string(),
            },
        ])
        .unwrap();
        let summary = d.render_state_summary().unwrap();
        assert!(summary.starts_with("<draft_state>"));
        assert!(summary.contains("Name: The Rusty Lantern Tavern"));
        assert!(summary.contains("Player: Kaelen"));
        assert!(summary.contains("NPCs: mara"));
        // Long background is truncated at 120 chars + ellipsis.
        assert!(summary.contains("Background: A traveling herbalist from the southern"));
        assert!(summary.contains('…'));
        assert!(summary.ends_with("</draft_state>"));
        // Tone was never set → not present.
        assert!(!summary.contains("Tone:"));
        // No banned title words leak into the summary (§11.29).
        for banned in ["hero", "chosen one", "main character"] {
            assert!(
                !summary.to_lowercase().contains(banned),
                "summary must not contain '{}'",
                banned
            );
        }
    }

    // --- sanitize_stem ---

    #[test]
    fn sanitize_stem_handles_spaces_punctuation_and_case() {
        assert_eq!(sanitize_stem("The Neon Dragon"), "the-neon-dragon");
        assert_eq!(sanitize_stem("Mara the Innkeep!"), "mara-the-innkeep");
        assert_eq!(sanitize_stem("  --Weird--  Name--  "), "weird-name");
        assert_eq!(sanitize_stem("UPPER"), "upper");
        assert_eq!(sanitize_stem(""), "");
    }
}
