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
}

/// The scalar fields `SetField` accepts. anything else rejects at validation.
pub const SETFIELD_FIELDS: &[&str] = &[
    "name",
    "setting",
    "tone",
    "opening_scene",
    "player_name",
    "core_persona",
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
    pub traits: Vec<String>,
    pub setting: Option<String>,
    pub tone: Option<String>,
    pub opening_scene: Option<String>,
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
                _ => unreachable!("validate_update gates SetField fields"),
            },
            DraftUpdate::AddTrait { value } => {
                if !self.traits.iter().any(|t| t == &value) {
                    self.traits.push(value);
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
        total += 2;
        if self.core_persona.is_some() {
            filled += 1;
        }
        if self.player_background.is_some() {
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
    /// (the same contract `wupi.sim` / `gm.sim` / `rusty_tavern.sim` rely on).
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
        out.push_str("  </scenario>\n");
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
        if let Some(c) = &self.starting_condition {
            lines.push(format!("Condition: {}", truncate_for_summary(c)));
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
        // 3 of 8 slots filled → ~37%.
        assert_eq!(d.completion_pct(), 37);
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
            DraftUpdate::SetPlayerBackground {
                value: "A traveling herbalist.".into(),
            },
            DraftUpdate::AddNpc {
                id: "mara".into(),
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
