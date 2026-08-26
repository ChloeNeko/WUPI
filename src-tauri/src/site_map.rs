//! Hidden site maps — the Schrödinger's Goblin upgrade (2026-08-19).
//!
//! A `SiteMap` is the OBJECTIVE, pre-generated truth of one interior (a
//! dungeon, a manor, a warren): its areas, the connections between them
//! (incl. locked/blocked routes — inaccessibility is never omission), and
//! its assets (creatures, groups, traps, hazards, loot, objects) with real
//! states. The map is generated ONCE by the JIT Architect (a deterministic
//! E4B pass in `fable_send`, see `maybe_run_site_architect`) the moment the
//! player ARRIVES at a node (indoor setting or carrying seeds) — reality is
//! committed before the narrator improvises over it, so a goblin behind a
//! locked door stays exactly there whether the player looks or not.
//!
//! # Knowledge model (the "hidden" in hidden site maps)
//!
//! Every area carries `AreaKnowledge` (Unrevealed → Discovered → Visited)
//! and every asset `AssetKnowledge` (Unrevealed → Suspected → Known). The
//! RENDER is the only leak channel and it is knowledge-filtered:
//! `render_narrator_slice` shows Visited areas with geometry + Known/Suspected
//! assets (hidden truth never renders — an unrevealed creature is a `?`),
//! `render_tracker_slice` shows compact ids so the tracker can emit real
//! `[ROOM]`/`[ASSET]` brackets (door TARGET ids are visible to the tracker —
//! a door is a visible fact; the room behind it is not).
//!
//! # Rust-authoritative (the npc_interior pattern)
//!
//! `WorldSchema.site_maps` is written ONLY by: the architect insert, the
//! `[ROOM]`/`[ASSET]` bracket appliers, and `last_visit_minutes` stamping.
//! `apply_delta` has no field for it and `merge_patch` has no arm — the
//! unknown-field refusal IS the immunity. Maps are write-once: a mapped
//! node never re-architects and is excluded from the Stale Roulette.
//!
//! # Stale Roulette (off-screen evolution)
//!
//! Un-mapped nodes carry `seeds` (≤2 short hooks) + a `last_evolved_minutes`
//! watermark on the travel `Node`. The world-progression tick designates the
//! 3 stalest un-mapped nodes and the pass may emit `site_seeds` for them;
//! Rust stamps the watermark on ALL designated sites (no-change is a valid
//! outcome; stamping all guarantees rotation — the no-op-starvation fix).
//! When the player later arrives at a seeded node, the architect folds the
//! seeds into the map it generates.
//!
//! All data-shape caps live here (the single-module cap rule from
//! settings.rs): see the consts below.

use std::collections::HashMap;

use crate::player_state::AttackerTier;
use crate::schema::TravelGraph;

// ---------------------------------------------------------------------------
// Caps (module-scope, per the settings.rs single-module rule)
// ---------------------------------------------------------------------------

/// A site needs at least entrance + two beyond (a corridor site is a hallway,
/// not a dungeon).
pub const MIN_SITE_AREAS: usize = 3;
/// Upper bound on generated areas — 8 areas + assets fit comfortably inside
/// the architect's 1024-token decode (`SITE_ARCHITECT_MAX_TOKENS`). A larger
/// map truncates mid-JSON, fails both passes, and re-architects every turn
/// (the 16-30s/turn stall, killed 2026-08-19 by lowering 12 → 8).
pub const MAX_SITE_AREAS: usize = 8;
/// Total assets per site (creatures + traps + loot + objects).
pub const MAX_SITE_ASSETS: usize = 16;
/// Outgoing connections any one area may declare (reciprocity doubles the
/// stored edges).
pub const MAX_SITE_CONNECTIONS_PER_AREA: usize = 6;
/// Group member count ceiling (0 = not-a-group).
pub const ASSET_COUNT_MAX: u32 = 99;
/// Free-text detail lines (asset/asset connection flavor).
pub const SITE_DETAIL_CHAR_MAX: usize = 160;
/// Geometry: at most 3 lines of 120 chars each per area.
pub const SITE_GEOMETRY_LINES_MAX: usize = 3;
pub const SITE_GEOMETRY_LINE_MAX: usize = 120;
/// Total concurrent site maps on the schema (LRU-evicted at the cap).
pub const MAX_SITE_MAPS: usize = 24;
/// Per-node seed hook cap (the tick's `site_seeds` push, FIFO).
pub const NODE_SEEDS_MAX: usize = 2;
/// Char cap for one seed hook (the `clean_free_text` discipline).
pub const SITE_SEED_CHAR_MAX: usize = 140;
/// The architect decode's generation reserve (a full site JSON fits well
/// under 1024 tokens; the sniper is the primary stop, this is the wall).
/// (2026-08-24 Chloe sign-off, 512 → 1024: the v0.30.0 live test showed
/// real settlement maps genuinely overflow 512 — every pass truncated at
/// exactly the wall, failed, and the idempotence gate re-architected the
/// same node EVERY turn (~41s/turn). The sniper still stops a well-formed
/// map at its closing fence; the wall only bites unclosed rambles.)
pub const SITE_ARCHITECT_MAX_TOKENS: i32 = 1024;
/// Char cap for area/asset ids (kebab ids are short by construction).
pub const SITE_ID_CHAR_MAX: usize = 64;
/// (2026-08-22 living-world) Re-entry digest bounds — the "changed since
/// your last visit" briefing the evolution pass writes + the narrator slice
/// renders on return. FIFO at the cap (the most recent changes win).
pub const DIGEST_LINE_MAX: usize = 6;
/// Per-line char cap for one digest entry (the `clean_free_text`
/// discipline).
pub const DIGEST_LINE_CHARS: usize = 160;
/// (2026-08-22 living-world) Terminal assets whose last off-screen change
/// is older than this (by the WORLD CLOCK — the schema carries no turn
/// counter, and the clock is the only deterministic "N turns" equivalent)
/// collapse out of the per-area renders into the bounded `remnants:` line
/// (Feature 4's dead-asset pruning: 5 dead bandits become 5 bodies).
pub const DEAD_ASSET_COLLAPSE_MINUTES: i64 = 1440;
/// (2026-08-22 multihog WS2) The JIT architect's CORRECTION passes after
/// the initial generation (total runs = 1 + this). Raised from 1: a
/// reciprocal-connection or reachability slip on pass 1 used to be the
/// whole budget, and the site silently re-architected every turn instead
/// of converging.
pub const SITE_ARCHITECT_REPAIR_PASSES: u8 = 2;
/// (2026-08-24 stall fix) After this many failed FULL rounds (each round =
/// 1 + `SITE_ARCHITECT_REPAIR_PASSES` decodes), both architects stand down
/// on that node — it stays deliberately map-less instead of re-burning the
/// whole decode cycle every turn via the write-once idempotence gate. One
/// counter per node (`Node::architect_fail_rounds`) guards both architects
/// for that settlement. 2 rounds = 6 decodes burned: enough evidence the
/// map doesn't fit the model's emit budget.
pub const ARCHITECT_FAIL_STANDDOWN: u8 = 2;
/// (2026-08-22 multihog WS2) The GM hidden-contents block's entity cap —
/// strictly the 1-hop hidden truth, ranked by threat (the narrator gets
/// enough to adjudicate and foreshadow, never a monster manual).
pub const GM_HIDDEN_MAX_ENTITIES: usize = 3;
/// (2026-08-22 multihog WS2) Per-line char cap inside the GM block (the
/// `SITE_DETAIL_CHAR_MAX` discipline).
pub const GM_HIDDEN_LINE_CHARS: usize = 160;
/// (2026-08-22 multihog WS2) Whole-block char ceiling for the GM
/// hidden-contents frame (frame law + ≤3 lines). A noise-level cost on the
/// CTX_API 16,384 narrator budget.
pub const GM_HIDDEN_BLOCK_CHARS: usize = 600;
/// (2026-08-23 WS5) Open causal threads per site, FIFO (the `pending_digest`
/// bounding discipline). Threads are RARE by construction — they open only
/// on terminal kills and live removals — so the cap is a degenerate-composition
/// backstop, not an expected occupancy.
pub const THREADS_MAX_PER_SITE: usize = 4;
/// (2026-08-23 WS5) Thread lines rendered into the tick's `## DEPARTED SITES`
/// section per site (the newest threads lead — they carry the freshest
/// cause/actor).
pub const THREAD_RENDER_MAX: usize = 2;
/// (2026-08-23 WS5) Per-line char cap for one rendered thread line (the
/// `SITE_DETAIL_CHAR_MAX` discipline).
pub const THREAD_LINE_CHARS: usize = 120;
/// (2026-08-23 WS5) An open thread older than this (by the WORLD CLOCK —
/// the `DEAD_ASSET_COLLAPSE_MINUTES` pattern) age-collapses: one digest line,
/// thread closed. Three in-world days — slower than the remnant collapse
/// because a plot question outlives its corpse.
pub const THREAD_COLLAPSE_MINUTES: i64 = 4320;
/// (2026-08-23 hosted interiors) Max hosted building interiors per
/// settlement (`"{node}::{asset}"` child maps). The 7th enter REFUSES to
/// map — the interior stays unmapped improv space (the graceful degrade),
/// never a silent eviction of a write-once child.
pub const HOSTED_INTERIORS_MAX_PER_SETTLEMENT: usize = 6;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// The site's overall danger band. Drives the default mob tier for the
/// combat Referee when a present creature carries no explicit tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SiteThreat {
    #[default]
    Low,
    Moderate,
    High,
    Deadly,
}

impl SiteThreat {
    pub fn word(self) -> &'static str {
        match self {
            SiteThreat::Low => "low",
            SiteThreat::Moderate => "moderate",
            SiteThreat::High => "high",
            SiteThreat::Deadly => "deadly",
        }
    }
}

/// What the player knows about one area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AreaKnowledge {
    /// Never seen, never named — renders only as a `?` stub count.
    #[default]
    Unrevealed,
    /// The player learned the area exists (a door, a sound, a glimpse) but
    /// has not entered. Renders name-only.
    Discovered,
    /// The player has been inside. Renders geometry + its Known/Suspected
    /// assets. Only the entrance starts Visited (at map creation).
    Visited,
}

/// The state of one area→area route. A locked/blocked route stays a graph
/// edge — inaccessibility is never omission (BFS reachability runs over ALL
/// connections regardless of state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    #[default]
    Open,
    Locked,
    Blocked,
}

impl ConnState {
    pub fn word(self) -> &'static str {
        match self {
            ConnState::Open => "open",
            ConnState::Locked => "locked",
            ConnState::Blocked => "blocked",
        }
    }
}

/// What the asset is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    #[default]
    Object,
    Creature,
    Group,
    Trap,
    Hazard,
    Loot,
    /// (2026-08-23 hosted interiors) A significant STRUCTURE on a
    /// settlement-scale (district) map — a guildhall, temple, grand
    /// manor. Entering one lazily generates a hosted child site map
    /// (keyed `"{node}::{asset}"`, see [`hosted_key`]). Ordinary shops
    /// and homes stay narrative — only structures worth crawling become
    /// Building assets. ILLEGAL on hosted (room-scale) maps: buildings
    /// never nest (the depth-2 law, enforced at add-form + architect).
    Building,
}

impl AssetKind {
    pub fn word(self) -> &'static str {
        match self {
            AssetKind::Object => "object",
            AssetKind::Creature => "creature",
            AssetKind::Group => "group",
            AssetKind::Trap => "trap",
            AssetKind::Hazard => "hazard",
            AssetKind::Loot => "loot",
            AssetKind::Building => "building",
        }
    }
}

/// The asset's live state in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetState {
    #[default]
    Active,
    Dead,
    Taken,
    Triggered,
    /// (2026-08-22 living-world) A trap/mechanism the player disarmed — a
    /// TERMINAL state under the play-canon locks (disarmed stays disarmed).
    Deactivated,
    /// (2026-08-22 living-world) A creature/group that broke off — transient
    /// by design (the evolution pass may settle it elsewhere or remove it).
    Fleeing,
}

impl AssetState {
    pub fn word(self) -> &'static str {
        match self {
            AssetState::Active => "active",
            AssetState::Dead => "dead",
            AssetState::Taken => "taken",
            AssetState::Triggered => "triggered",
            AssetState::Deactivated => "deactivated",
            AssetState::Fleeing => "fleeing",
        }
    }
}

/// The PLAY-CANON terminal set — the states the Rust appliers refuse to
/// walk back (dead stays dead, looted stays looted, disarmed stays
/// disarmed). The ONE sanctioned transition out is Dead → Taken (looting
/// a corpse is aftermath, never resurrection). Both the tracker's
/// `[ASSET]` mutations and the evolution pass's ops run through
/// [`canon_transition`] — a model hallucinating a dead boss back to life
/// rejects with the remnant-entity directive instead. `remnants_line`
/// groups by exactly this array, so it is the one source both read.
pub const TERMINAL_STATES: [AssetState; 3] =
    [AssetState::Dead, AssetState::Taken, AssetState::Deactivated];

pub fn is_terminal(state: AssetState) -> bool {
    TERMINAL_STATES.contains(&state)
}

/// Whether a `from → to` state transition respects the play-canon locks.
/// Same-state refreshes are always legal (fresh cause/detail on a corpse).
pub fn canon_transition(from: AssetState, to: AssetState) -> bool {
    from == to || (from == AssetState::Dead && to == AssetState::Taken)
}

/// Parse a state word (model-emitted or hand-written) into an `AssetState`.
pub fn parse_asset_state_word(s: &str) -> Option<AssetState> {
    match s.trim().to_lowercase().as_str() {
        "active" => Some(AssetState::Active),
        "dead" | "slain" | "killed" => Some(AssetState::Dead),
        "taken" | "looted" => Some(AssetState::Taken),
        "triggered" | "sprung" => Some(AssetState::Triggered),
        "deactivated" | "disarmed" => Some(AssetState::Deactivated),
        "fleeing" | "fled" => Some(AssetState::Fleeing),
        _ => None,
    }
}

/// (2026-08-24 Part II B1) Parse a kind word into an `AssetKind` — the
/// `add_asset` evolution op's vocabulary. Buildings are DELIBERATELY
/// absent: a settlement structure never arrives off-screen (the depth-2
/// law — buildings are authored/minted on settlement maps only).
pub fn parse_asset_kind_word(s: &str) -> Option<AssetKind> {
    match s.trim().to_lowercase().as_str() {
        "object" => Some(AssetKind::Object),
        "creature" => Some(AssetKind::Creature),
        "group" => Some(AssetKind::Group),
        "trap" => Some(AssetKind::Trap),
        "hazard" => Some(AssetKind::Hazard),
        "loot" => Some(AssetKind::Loot),
        _ => None,
    }
}

/// (2026-08-22 living-world) Where an asset's current truth came from —
/// audit provenance for the evolution digest + the collapse rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetOrigin {
    /// Generated by the JIT architect (the map's initial truth).
    #[default]
    InitialMap,
    /// Minted in play by the tracker's `[ASSET +…]` add-form — the player
    /// witnessed its arrival.
    NarratorEstablished,
    /// Mutated off-screen by the world-progression evolution pass.
    Evolved,
    /// (2026-08-23 Playground) Minted by the Playground asset spawner —
    /// the god-mode sand table. Player-witnessed like
    /// [`AssetOrigin::NarratorEstablished`], but audit-distinct so the
    /// evolution digest + the Playground's own reports can tell a test
    /// spawn from a narrated arrival.
    Playground,
}

impl AssetOrigin {
    pub fn word(self) -> &'static str {
        match self {
            AssetOrigin::InitialMap => "initial_map",
            AssetOrigin::NarratorEstablished => "narrator_established",
            AssetOrigin::Evolved => "evolved",
            AssetOrigin::Playground => "playground",
        }
    }
}

/// What the player knows about one asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetKnowledge {
    #[default]
    Unrevealed,
    /// The player suspects something is there (sounds, tracks, a smell) —
    /// renders as a suspicion line, never the truth.
    Suspected,
    /// The player has seen it. Renders state/count/detail.
    Known,
}

/// One area→area route.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteConnection {
    /// Target area id (kebab).
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub state: ConnState,
    /// Short flavor ("iron-bound door", "rubble-choked arch").
    #[serde(default)]
    pub detail: String,
}

/// One area of a site.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteArea {
    /// Kebab id ("gatehouse", "rot-warren"). Emitted by the tracker in
    /// `[ROOM <area_id>]`.
    #[serde(default)]
    pub id: String,
    /// Diegetic name ("The Gatehouse").
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub knowledge: AreaKnowledge,
    /// ≤3 terse geometry/sensory lines, ≤120 chars each.
    #[serde(default)]
    pub geometry: Vec<String>,
    #[serde(default)]
    pub connections: Vec<SiteConnection>,
}

/// One creature/trap/loot/object in the site.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteAsset {
    /// Kebab id. Emitted by the tracker in `[ASSET <asset_id> …]`.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: AssetKind,
    /// The area id the asset lives in.
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub state: AssetState,
    #[serde(default)]
    pub knowledge: AssetKnowledge,
    /// Group-only member count (1–99; 0 = not-a-group).
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub detail: String,
    /// Optional explicit threat tier ("minion"/"soldier"/"elite"/"boss"/
    /// "legendary") — overrides the threat default in `present_mob_tier`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// (2026-08-22 living-world) Provenance of this asset's current truth.
    /// Tracker-driven in-scene mutations do NOT flip it (the player
    /// witnessed those); only the evolution pass stamps `Evolved`.
    #[serde(default)]
    pub origin: AssetOrigin,
    /// (2026-08-22 living-world) The in-world minute of the last OFF-SCREEN
    /// mutation (the evolution pass). 0 = never evolved off-screen —
    /// drives the remnants-collapse TTL; tracker-driven in-scene changes
    /// leave it at 0 (the player saw them happen).
    #[serde(default)]
    pub changed_at_minutes: i64,
    /// (2026-08-22 living-world) One-line cause of the last off-screen
    /// change ("scavengers stripped it", "the warband moved to the ridge")
    /// — ≤160, `clean_free_text`-cleaned at the apply.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cause: String,
    /// (2026-08-22 living-world) Who/what made the last off-screen change
    /// ("the town watch", "flood water") — ≤64.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub actor: String,
    /// (2026-08-22 multihog WS1) The in-world minute this asset's armed
    /// truth lapses (`[EXPIRY <asset>@<node>]`). `None` = no armed expiry.
    /// The deterministic clock sweep ([`sweep_asset_expiry`]) deactivates a
    /// lapsed NON-terminal asset; terminal assets skip (play-canon
    /// respected — the dead stay dead even when their timer runs out).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_minutes: Option<i64>,
}

/// The whole hidden map of one node's interior.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteMap {
    /// The travel-graph ROOT node this map belongs to — for a hosted
    /// building interior the SETTLEMENT node, never the composite key
    /// (every travel-graph correlation stays correct; the composite key
    /// lives only in the `WorldSchema::site_maps` HashMap key + `host`).
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub threat: SiteThreat,
    /// The area the player entered through. The ONLY Visited area at
    /// creation (validated).
    #[serde(default)]
    pub entrance: String,
    #[serde(default)]
    pub areas: Vec<SiteArea>,
    #[serde(default)]
    pub assets: Vec<SiteAsset>,
    /// In-world minute of the player's last visit (stamped at insert + on
    /// every `[ROOM … visited]`). The LRU key for map eviction.
    #[serde(default)]
    pub last_visit_minutes: i64,
    /// (2026-08-22 living-world) The pending "changed since your last
    /// visit" briefing — one bounded line per off-screen evolution op,
    /// written by the tick's apply, rendered by the narrator slice for as
    /// long as the player stands here, CLEARED when they depart (the
    /// `[TRAVEL]` applier). FIFO at [`DIGEST_LINE_MAX`]; event-driven by
    /// construction (every line IS an op that ran while the player was
    /// elsewhere), so no ack watermark is needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_digest: Vec<String>,
    /// (2026-08-22 multihog WS2) The area the player currently occupies
    /// inside this site — stamped at the architect insert (the entrance)
    /// and on every visited-form `[ROOM]` apply. The from-end of the
    /// traversal gate (`Locked`/`Blocked` connections bar `[ROOM]` entry)
    /// + the 1-hop anchor of the narrator's GM hidden-contents block.
    /// `None` on legacy pre-WS2 saves (the gate skips; the GM block falls
    /// back to the entrance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_area: Option<String>,
    /// (2026-08-23 WS5) The OPEN causal-thread ledger: every terminal kill
    /// or live removal opens a thread (who did it, why, where), and the
    /// thread stays open — carried into every `## DEPARTED SITES` evolution
    /// pass as a live plot — until something DETERMINISTIC closes it: a
    /// later applied op touching the same asset or the same area, the
    /// player's re-entry (the question becomes live play), or the
    /// [`THREAD_COLLAPSE_MINUTES`] age watermark. Every closure writes one
    /// bounded `pending_digest` line. Rust-authoritative like the rest of
    /// the map: no `apply_delta` field, no `merge_patch` arm, resolution is
    /// pure key matching (NEVER free-text/cause inference). Dormant on
    /// pre-WS5 saves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<SiteThread>,
    /// (2026-08-23 hosted interiors) Set ONLY on a hosted building child:
    /// the parent settlement map key, the Building asset id on it, and the
    /// parent area the player exits back into. The breadcrumb bridge —
    /// the parent and child graphs stay separate; this is how the exit
    /// transition and the narrator slice find their way back out. Dormant
    /// (`None`) on every node-level map + every pre-feature save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostRef>,
    /// (2026-08-23 hosted interiors) Set ONLY on a settlement (parent) map
    /// while the player stands INSIDE one of its Building assets — the
    /// Building ASSET id (not an area id: one district area can host
    /// several buildings, so the area alone cannot disambiguate). This is
    /// THE state anchor [`active_site_map_key`] resolves through; the
    /// enter/exit transitions stamp/clear it. Dormant on pre-feature saves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_building: Option<String>,
}

/// (2026-08-23 hosted interiors) The parent link on a hosted child map —
/// see [`SiteMap::host`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct HostRef {
    /// The parent map's key in `WorldSchema::site_maps` (the settlement's
    /// node id — parents are always node-level).
    #[serde(default)]
    pub parent_key: String,
    /// The Building asset id ON THE PARENT map whose interior this child
    /// maps.
    #[serde(default)]
    pub building_asset_id: String,
    /// The parent area the player exits back into (stamped at first enter;
    /// the building's own district area).
    #[serde(default)]
    pub exit_area_id: String,
}

// ---------------------------------------------------------------------------
// Hosted interiors — keys + the resolver law
// ---------------------------------------------------------------------------

/// The composite key of a hosted building interior in
/// `WorldSchema::site_maps`: `"{node_id}::{building_asset_id}"`. The `::`
/// separator cannot appear in kebab ids, so parsing is unambiguous.
pub fn hosted_key(node_id: &str, building_asset_id: &str) -> String {
    format!("{node_id}::{building_asset_id}")
}

/// Split a hosted composite key back into `(node_id, building_asset_id)`.
/// `None` for plain node keys. Empty halves reject (hand-edit hygiene).
pub fn parse_hosted_key(key: &str) -> Option<(&str, &str)> {
    let (node, asset) = key.split_once("::")?;
    if node.is_empty() || asset.is_empty() || asset.contains("::") {
        return None;
    }
    Some((node, asset))
}

/// Count a settlement's hosted child maps (the per-settlement cap read).
pub fn count_hosted_interiors(site_maps: &std::collections::HashMap<String, SiteMap>, node_id: &str) -> usize {
    site_maps
        .keys()
        .filter(|k| parse_hosted_key(k).is_some_and(|(node, _)| node == node_id))
        .count()
}

/// (2026-08-23 hosted interiors) Belt-and-suspenders settlement detector for
/// nodes minted before the tracker learned to emit `setting=settlement`:
/// word-boundary match of obvious settlement words in the node's NAME. The
/// `setting` classification stays the primary signal — this only rescues
/// clearly-named towns from staying unmapped.
pub fn looks_like_settlement(name: &str) -> bool {
    const MARKERS: [&str; 10] = [
        "town", "city", "village", "port", "hamlet", "keep", "citadel", "fort",
        "burg", "settlement",
    ];
    let lowered = name.to_lowercase();
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| MARKERS.contains(&word))
}

/// THE RESOLVER LAW (2026-08-23 hosted interiors): which site map owns the
/// player's current turn operations. The current node's map, UNLESS its
/// `current_building` names a Building asset whose hosted child exists —
/// then the child. Depth is structurally capped at 2 (children can never
/// carry Building assets), so one descent suffices — no recursion. Every
/// turn-path consumer (`[ROOM]`/`[ASSET]`/`[UNLOCK]` appliers, slice
/// renders, mob-tier reads) MUST go through this; a bare
/// `site_maps.get(current_node)` while the player stands in a building is
/// the split-brain bug this function exists to kill.
pub fn active_site_map_key(
    site_maps: &std::collections::HashMap<String, SiteMap>,
    current_node: Option<&str>,
) -> Option<String> {
    let node = current_node?;
    let map = site_maps.get(node)?;
    match &map.current_building {
        None => Some(node.to_string()),
        Some(building) => {
            let child = hosted_key(node, building);
            if site_maps.contains_key(&child) {
                Some(child)
            } else {
                // current_building set but the child is gone (evicted /
                // hand-edited): fall back to the parent rather than a dead
                // key. The exit transition or [TRAVEL] collapse will clear
                // the stale pointer on the next move.
                Some(node.to_string())
            }
        }
    }
}

/// (2026-08-23 hosted interiors) The ENTER transition's per-map stamps —
/// pure state math shared by the `[ROOM]` applier (the caller owns the
/// gates + reject text: per-settlement cap, reachability). Parent side:
/// `current_building` set (THE resolver anchor), visit stamped, the
/// Building asset revealed Known.
pub fn enter_building_parent_stamp(parent: &mut SiteMap, asset_id: &str, now_minutes: i64) {
    parent.current_building = Some(asset_id.to_string());
    parent.last_visit_minutes = now_minutes;
    if let Some(asset) = parent.assets.iter_mut().find(|a| a.id == asset_id) {
        asset.knowledge = AssetKnowledge::Known;
    }
}

/// The ENTER transition's child side — applies ONLY when the building was
/// mapped on an earlier visit (a fresh child arrives via the
/// hosted-interior architect later this turn): arrival stamps + the open
/// threads become live play.
pub fn enter_building_child_stamp(child: &mut SiteMap, now_minutes: i64) {
    child.last_visit_minutes = now_minutes;
    child.current_area = Some(child.entrance.clone());
    flush_threads_on_arrival(child);
}

/// (2026-08-23 hosted interiors) The EXIT transition's per-map stamps —
/// back out into `target_area` of the parent district. Parent side: the
/// chain pointer clears, the player stands in the district again, the area
/// is Visited.
pub fn exit_building_parent_stamp(parent: &mut SiteMap, target_area: &str, now_minutes: i64) {
    parent.current_building = None;
    parent.last_visit_minutes = now_minutes;
    parent.current_area = Some(target_area.to_string());
    if let Some(area) = parent.areas.iter_mut().find(|a| a.id == target_area) {
        area.knowledge = AreaKnowledge::Visited;
    }
}

/// The EXIT transition's child side: the player no longer stands inside —
/// position cleared, visit stamped, the re-entry briefing (read during the
/// stay) cleared.
pub fn exit_building_child_stamp(child: &mut SiteMap, now_minutes: i64) {
    child.current_area = None;
    child.last_visit_minutes = now_minutes;
    child.pending_digest.clear();
}

/// The display breadcrumb for a hosted child map: "{settlement name} >
/// {building name}" — for the narrator `site:` block's `in …` line + the
/// tracker's compact slice prefix. `None` for node-level keys or a broken
/// host chain (evicted parent — the resolver's fallback domain).
pub fn hosted_breadcrumb(
    site_maps: &std::collections::HashMap<String, SiteMap>,
    graph: &TravelGraph,
    child_key: &str,
) -> Option<String> {
    let host = site_maps.get(child_key)?.host.as_ref()?;
    let parent = site_maps.get(&host.parent_key)?;
    let building = parent
        .assets
        .iter()
        .find(|a| a.id == host.building_asset_id)?;
    let node_name = graph
        .find_node(&host.parent_key)
        .map(|n| n.name.clone())
        .unwrap_or_else(|| host.parent_key.clone());
    Some(format!("{node_name} > {}", building.name))
}

/// The player-bubble FREEZE set (2026-08-23 hosted interiors): every
/// `site_maps` key that must NOT be touched by off-screen evolution or LRU
/// eviction — the current node's map, the hosted child the player stands
/// in, and (when inside) the child's parent (mutating the district map
/// while the player is inside one of its buildings moves the building out
/// from under them). One predicate for the tick designation filter, the
/// apply-time re-check, and `evict_lru_site_map`.
pub fn player_frozen_keys(
    site_maps: &std::collections::HashMap<String, SiteMap>,
    current_node: Option<&str>,
) -> Vec<String> {
    let mut frozen: Vec<String> = Vec::with_capacity(2);
    let Some(node) = current_node else {
        return frozen;
    };
    frozen.push(node.to_string());
    if let Some(map) = site_maps.get(node) {
        if let Some(building) = &map.current_building {
            let child = hosted_key(node, building);
            if site_maps.contains_key(&child) {
                frozen.push(child);
            }
        }
    }
    frozen
}

/// (2026-08-23 WS5) One OPEN causal thread — the ledger entry that makes
/// the evolution pass's stamped `cause`/`actor` compound across ticks
/// ("the party killed the bandit chief" stays a live power-vacuum question
/// instead of a dead state + two strings).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteThread {
    /// The subject ASSET id — the primary resolution key (a later applied
    /// op on this asset closes the thread).
    #[serde(default)]
    pub subject: String,
    /// Display name (render-friendly; the digest + tick lines carry it).
    #[serde(default)]
    pub subject_name: String,
    /// The area id where the causal event happened — the secondary
    /// resolution key (activity in the same area closes the thread).
    #[serde(default)]
    pub area: String,
    /// Who/what caused it ("the town watch", "the player") — ≤64.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub actor: String,
    /// One-line cause ("killed in the raid") — ≤160.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cause: String,
    /// In-world minute the thread opened (kept on refresh — the ORIGINAL
    /// event time is the plot's anchor; the age-collapse watermark reads it).
    #[serde(default)]
    pub opened_at_minutes: i64,
    /// (2026-08-25) The subject was Unrevealed knowledge when the thread
    /// opened — closure/arrival/fade DIGEST lines suppress it (the P2
    /// knowledge-safe channel law: hidden-truth names never reach the
    /// narrator's re-entry briefing). The tick's own thread render is
    /// GM-side machinery and stays ungated.
    #[serde(default)]
    pub hidden: bool,
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// True for a bare kebab id: lowercase alphanumerics joined by single
/// hyphens, no leading/trailing hyphen, ≤64 chars.
pub fn is_kebab_id(id: &str) -> bool {
    if id.is_empty() || id.chars().count() > SITE_ID_CHAR_MAX {
        return false;
    }
    let mut prev_dash = false;
    for (i, c) in id.chars().enumerate() {
        match c {
            'a'..='z' | '0'..='9' => prev_dash = false,
            '-' => {
                if i == 0 || prev_dash {
                    return false;
                }
                prev_dash = true;
            }
            _ => return false,
        }
    }
    !prev_dash
}

/// Collapse free text to a kebab id ("The Rot Warren" → "the-rot-warren").
pub fn kebabify(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    // Truncate FIRST, then strip trailing dashes: cutting at 64 mid-word can
    // land ON the joining dash, and a trailing dash fails `is_kebab_id`
    // downstream (the architect's correction pass would reject the id).
    let mut out: String = out.chars().take(SITE_ID_CHAR_MAX).collect();
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Flatten one render line: newlines/tabs → spaces, control chars stripped
/// (the `flatten_inline` anti-forgery discipline — a hand-edited save must
/// not smuggle a forged render line into `<world_state>`).
fn flatten(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            _ => c,
        })
        .filter(|c| {
            let code = *c as u32;
            !((code <= 0x08) || code == 0x0B || code == 0x0C || (0x0E..=0x1F).contains(&code))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Structural validation of a (model-generated or hand-edited) map. Returns
/// EVERY failure (the architect's correction pass shows them all at once).
///
/// Checks: kebab ids, area/asset id uniqueness, area-count bounds, entrance
/// exists + is Visited + is the ONLY Visited area (a fresh map's knowledge
/// starts at the entrance), reciprocal connections with matching state +
/// detail, BFS reachability from the entrance over ALL connections
/// regardless of state (a locked door is still a door), every area ≥1
/// connection, per-area connection cap, asset locations valid, Group-only
/// count 1–99, geometry/detail caps.
pub fn validate(map: &SiteMap) -> Result<(), Vec<String>> {
    let mut errs: Vec<String> = Vec::new();
    if map.areas.len() < MIN_SITE_AREAS {
        errs.push(format!(
            "areas: {} is under the minimum of {MIN_SITE_AREAS}",
            map.areas.len()
        ));
    }
    if map.areas.len() > MAX_SITE_AREAS {
        errs.push(format!(
            "areas: {} exceeds the cap of {MAX_SITE_AREAS}",
            map.areas.len()
        ));
    }
    if map.assets.len() > MAX_SITE_ASSETS {
        errs.push(format!(
            "assets: {} exceeds the cap of {MAX_SITE_ASSETS}",
            map.assets.len()
        ));
    }
    for area in &map.areas {
        if !is_kebab_id(&area.id) {
            errs.push(format!("area id {:?} is not a bare kebab id", area.id));
        }
        if area.geometry.len() > SITE_GEOMETRY_LINES_MAX {
            errs.push(format!(
                "area {}: {} geometry lines exceeds {}",
                area.id,
                area.geometry.len(),
                SITE_GEOMETRY_LINES_MAX
            ));
        }
        for g in &area.geometry {
            if g.chars().count() > SITE_GEOMETRY_LINE_MAX {
                errs.push(format!(
                    "area {}: geometry line over {} chars",
                    area.id, SITE_GEOMETRY_LINE_MAX
                ));
            }
        }
        if area.connections.len() > MAX_SITE_CONNECTIONS_PER_AREA {
            errs.push(format!(
                "area {}: {} connections exceeds {}",
                area.id,
                area.connections.len(),
                MAX_SITE_CONNECTIONS_PER_AREA
            ));
        }
        if area.connections.is_empty() {
            errs.push(format!("area {}: no connections (orphan)", area.id));
        }
        for c in &area.connections {
            if !is_kebab_id(&c.to) {
                errs.push(format!(
                    "area {} connection target {:?} is not a kebab id",
                    area.id, c.to
                ));
            }
            if c.detail.chars().count() > SITE_DETAIL_CHAR_MAX {
                errs.push(format!(
                    "area {}: connection detail over {} chars",
                    area.id, SITE_DETAIL_CHAR_MAX
                ));
            }
        }
    }
    // Id uniqueness.
    for i in 0..map.areas.len() {
        for j in i + 1..map.areas.len() {
            if map.areas[i].id == map.areas[j].id {
                errs.push(format!("duplicate area id {}", map.areas[i].id));
            }
        }
    }
    for i in 0..map.assets.len() {
        for j in i + 1..map.assets.len() {
            if map.assets[i].id == map.assets[j].id {
                errs.push(format!("duplicate asset id {}", map.assets[i].id));
            }
        }
    }
    // Entrance: exists, Visited, and the only Visited area at creation.
    let entrance = map.areas.iter().find(|a| a.id == map.entrance);
    match entrance {
        None => errs.push(format!("entrance {:?} is not a known area", map.entrance)),
        Some(e) => {
            if e.knowledge != AreaKnowledge::Visited {
                errs.push("entrance area must be Visited".to_string());
            }
        }
    }
    let visited_count = map
        .areas
        .iter()
        .filter(|a| a.knowledge == AreaKnowledge::Visited)
        .count();
    if visited_count > 1 {
        errs.push(format!(
            "{visited_count} areas are Visited — only the entrance may be"
        ));
    }
    // Reciprocal connections (state + detail must match both ways).
    for area in &map.areas {
        for c in &area.connections {
            let Some(target) = map.areas.iter().find(|a| a.id == c.to) else {
                errs.push(format!(
                    "area {}: connection to unknown area {:?}",
                    area.id, c.to
                ));
                continue;
            };
            match target.connections.iter().find(|rc| rc.to == area.id) {
                None => errs.push(format!(
                    "connection {} -> {} is not reciprocal",
                    area.id, c.to
                )),
                Some(rc) => {
                    if rc.state != c.state || rc.detail != c.detail {
                        errs.push(format!(
                            "connection {} <-> {} state/detail mismatch",
                            area.id, c.to
                        ));
                    }
                }
            }
        }
    }
    // BFS reachability from the entrance over ALL connections.
    if entrance.is_some() {
        let mut seen: Vec<String> = Vec::new();
        let mut queue: Vec<String> = vec![map.entrance.clone()];
        while let Some(cur) = queue.pop() {
            if seen.iter().any(|s| *s == cur) {
                continue;
            }
            seen.push(cur.clone());
            if let Some(a) = map.areas.iter().find(|a| a.id == cur) {
                for c in &a.connections {
                    if !seen.iter().any(|s| *s == c.to) {
                        queue.push(c.to.clone());
                    }
                }
            }
        }
        for area in &map.areas {
            if !seen.iter().any(|s| *s == area.id) {
                errs.push(format!("area {} is unreachable from the entrance", area.id));
            }
        }
    }
    // Assets.
    for a in &map.assets {
        if !is_kebab_id(&a.id) {
            errs.push(format!("asset id {:?} is not a bare kebab id", a.id));
        }
        if !map.areas.iter().any(|area| area.id == a.location) {
            errs.push(format!(
                "asset {}: location {:?} is not a known area",
                a.id, a.location
            ));
        }
        if a.detail.chars().count() > SITE_DETAIL_CHAR_MAX {
            errs.push(format!(
                "asset {}: detail over {} chars",
                a.id, SITE_DETAIL_CHAR_MAX
            ));
        }
        match a.kind {
            AssetKind::Group => {
                if a.count == 0 || a.count > ASSET_COUNT_MAX {
                    errs.push(format!(
                        "asset {}: group count {} outside 1..={ASSET_COUNT_MAX}",
                        a.id, a.count
                    ));
                }
            }
            _ => {
                if a.count != 0 {
                    errs.push(format!(
                        "asset {}: count {} on a non-group asset (must be 0)",
                        a.id, a.count
                    ));
                }
            }
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs)
    }
}

// ---------------------------------------------------------------------------
// Model-output parse
// ---------------------------------------------------------------------------

impl SiteMap {
    /// Parse the architect pass's model output. Mirrors `SchemaDelta::
    /// from_model_output`'s tolerant pipeline: extract the reply channel →
    /// fenced-JSON extraction (the TrackerSniper is fence-aware, so a fenced
    /// body can't be decapitated) → `json_repair::repair` → serde. Falls
    /// back to repairing the whole reply when no fence was found. Returns
    /// the LAST parse error for the correction pass to quote.
    pub fn from_model_output(raw: &str) -> Result<SiteMap, String> {
        let reply = crate::schema::extract_reply_channel(raw);
        let (_prose, bodies) = crate::bracket_parser::extract_fenced_json(&reply);
        let candidates: Vec<String> = if bodies.is_empty() {
            vec![reply.trim().to_string()]
        } else {
            bodies
        };
        let mut last_err = "no JSON object found in the architect output".to_string();
        for body in &candidates {
            let repaired = crate::json_repair::repair(body);
            match serde_json::from_str::<SiteMap>(&repaired) {
                Ok(map) => return Ok(map),
                Err(e) => last_err = format!("JSON parse: {e}"),
            }
        }
        Err(last_err)
    }
}

// ---------------------------------------------------------------------------
// Renders (knowledge-filtered — the only leak channel)
// ---------------------------------------------------------------------------

fn area_name<'a>(map: &'a SiteMap, id: &str) -> &'a str {
    map.areas
        .iter()
        .find(|a| a.id == id)
        .map(|a| a.name.as_str())
        .unwrap_or("?")
}

fn area_knowledge(map: &SiteMap, id: &str) -> AreaKnowledge {
    map.areas
        .iter()
        .find(|a| a.id == id)
        .map(|a| a.knowledge)
        .unwrap_or(AreaKnowledge::Unrevealed)
}

/// True when a terminal asset is old enough to collapse out of the
/// per-area renders into the bounded `remnants:` line (Feature 4's
/// dead-asset pruning): it carries an off-screen change stamp AND that
/// change is at least [`DEAD_ASSET_COLLAPSE_MINUTES`] old. A terminal
/// state the player JUST caused keeps its full render — the corpse you
/// made this scene is scene-relevant; the one from three days ago is
/// clutter.
fn collapses_to_remnant(a: &SiteAsset, now_minutes: i64) -> bool {
    is_terminal(a.state)
        && a.changed_at_minutes > 0
        && now_minutes.saturating_sub(a.changed_at_minutes) >= DEAD_ASSET_COLLAPSE_MINUTES
}

/// The bounded remnants summary: stale terminal assets grouped by state
/// word, ≤6 names per group + `(+N more)`. One line; `None` when nothing
/// qualifies.
fn remnants_line(map: &SiteMap, now_minutes: i64) -> Option<String> {
    const NAMES_PER_STATE: usize = 6;
    let mut groups: Vec<String> = Vec::new();
    for state in TERMINAL_STATES {
        let all: Vec<&SiteAsset> = map
            .assets
            .iter()
            .filter(|a| {
                a.knowledge != AssetKnowledge::Unrevealed
                    && collapses_to_remnant(a, now_minutes)
                    && a.state == state
            })
            .collect();
        if all.is_empty() {
            continue;
        }
        let extra = all.len().saturating_sub(NAMES_PER_STATE);
        let names: Vec<&str> = all
            .iter()
            .take(NAMES_PER_STATE)
            .map(|a| a.name.as_str())
            .collect();
        // "disarmed" reads better than "deactivated" in prose context.
        let word = match state {
            AssetState::Dead => "dead",
            AssetState::Taken => "taken",
            _ => "disarmed",
        };
        groups.push(if extra > 0 {
            format!("{word} — {} (+{extra} more)", names.join(", "))
        } else {
            format!("{word} — {}", names.join(", "))
        });
    }
    if groups.is_empty() {
        None
    } else {
        Some(format!("remnants: {}", groups.join("; ")))
    }
}

/// The NARRATOR's slice: knowledge-filtered rich prose. Visited areas render
/// geometry + their Known assets (state/count/detail) + Suspected assets as
/// suspicion lines + named ways on; Discovered areas render name-only;
/// unrevealed neighbors render as `?` stub counts. Hidden truth (unrevealed
/// areas/assets) NEVER renders. `(+N more)` caps bound the block — sized to
/// the full architect cap (2026-08-21 evening follow-up to the 8192 ruling:
/// 6 → 8 areas, 6 → 8 assets per area): a fully-explored MAX_SITE_AREAS
/// dungeon renders EVERY revealed area; only unrevealed space counts hidden.
///
/// (2026-08-22 living-world) The re-entry digest renders FIRST when the
/// evolution pass wrote lines while the player was elsewhere, and stale
/// terminal assets collapse into one `remnants:` line (`now_minutes` is the
/// world clock — the collapse + digest are clock-driven, never turn-counted).
pub fn render_narrator_slice(map: &SiteMap, now_minutes: i64) -> Option<String> {
    const AREAS_SHOWN: usize = MAX_SITE_AREAS;
    const ASSETS_SHOWN: usize = 8;
    let mut out: Vec<String> = Vec::new();
    if !map.pending_digest.is_empty() {
        out.push("changed since your last visit:".to_string());
        for line in map.pending_digest.iter().take(DIGEST_LINE_MAX) {
            out.push(format!("  {line}"));
        }
    }
    out.push(format!(
        "threat: {}; entrance: {}",
        map.threat.word(),
        area_name(map, &map.entrance)
    ));
    let mut shown = 0usize;
    let mut hidden = 0usize;
    for area in &map.areas {
        if area.knowledge == AreaKnowledge::Unrevealed {
            hidden += 1;
            continue;
        }
        if shown >= AREAS_SHOWN {
            hidden += 1;
            continue;
        }
        shown += 1;
        match area.knowledge {
            AreaKnowledge::Visited => {
                let mut line = format!("{} ({}) — visited", area.name, area.id);
                if !area.geometry.is_empty() {
                    line.push_str(": ");
                    line.push_str(&area.geometry.join("; "));
                }
                out.push(line);
                let mut a_shown = 0usize;
                let mut a_hidden = 0usize;
                for asset in &map.assets {
                    if asset.location != area.id
                        || asset.knowledge == AssetKnowledge::Unrevealed
                        || collapses_to_remnant(asset, now_minutes)
                    {
                        continue;
                    }
                    if a_shown >= ASSETS_SHOWN {
                        a_hidden += 1;
                        continue;
                    }
                    a_shown += 1;
                    let mut line = match asset.knowledge {
                        AssetKnowledge::Suspected => format!("  (suspected) {}", asset.name),
                        _ => format!("  {}", asset.name),
                    };
                    // (2026-08-24 review fix) Group counts are KNOWN-only —
                    // a magnitude on a Suspected warband told the narrator
                    // exactly how many wait behind the rumor, contradicting
                    // the module's own law (suspicion carries the name +
                    // flag ONLY, never state or magnitude; the player DTO
                    // already enforced this).
                    if asset.count > 0 && asset.knowledge == AssetKnowledge::Known {
                        line.push_str(&format!(" ×{}", asset.count));
                    }
                    if asset.knowledge == AssetKnowledge::Known && asset.state != AssetState::Active
                    {
                        line.push_str(&format!(" ({})", asset.state.word()));
                    }
                    if !asset.detail.is_empty() {
                        line.push_str(&format!(" — {}", asset.detail));
                    }
                    out.push(line);
                }
                if a_hidden > 0 {
                    out.push(format!("  (+{a_hidden} more)"));
                }
                // Ways on: connections to KNOWN areas by name (+ state when
                // not open); unrevealed neighbors collapse to a `?` count.
                let mut ways: Vec<String> = Vec::new();
                let mut q = 0usize;
                for c in &area.connections {
                    if area_knowledge(map, &c.to) == AreaKnowledge::Unrevealed {
                        q += 1;
                    } else {
                        let name = area_name(map, &c.to).to_string();
                        ways.push(if c.state == ConnState::Open {
                            name
                        } else {
                            format!("{name} ({})", c.state.word())
                        });
                    }
                }
                let mut ways_line = String::new();
                if !ways.is_empty() {
                    ways_line.push_str(&format!("  ways on: {}", ways.join(", ")));
                }
                if q > 0 {
                    if ways_line.is_empty() {
                        ways_line = format!("  ? ways on: {q}");
                    } else {
                        ways_line.push_str(&format!("; ? ×{q}"));
                    }
                }
                if !ways_line.is_empty() {
                    out.push(ways_line);
                }
            }
            AreaKnowledge::Discovered => {
                let q = area
                    .connections
                    .iter()
                    .filter(|c| area_knowledge(map, &c.to) == AreaKnowledge::Unrevealed)
                    .count();
                out.push(format!(
                    "{} ({}) — found, not yet entered{}",
                    area.name,
                    area.id,
                    if q > 0 {
                        format!("; ? ways on: {q}")
                    } else {
                        String::new()
                    }
                ));
            }
            AreaKnowledge::Unrevealed => unreachable!("filtered above"),
        }
    }
    if hidden > 0 {
        out.push(format!("(+{hidden} more areas)"));
    }
    if let Some(remnants) = remnants_line(map, now_minutes) {
        out.push(remnants);
    }
    if out.is_empty() {
        None
    } else {
        Some(out.iter().map(|l| flatten(l)).collect::<Vec<_>>().join("\n"))
    }
}

/// The TRACKER's slice: compact id-bearing line so the E4B can emit real
/// `[ROOM]`/`[ASSET]` brackets. Lists visited (`id:v doors=<to>:<state>,…`)
/// and discovered (`id:d`) areas + the doors out of them — a door's TARGET id is
/// a visible fact (that's how an unrevealed room is first entered), while
/// the room itself stays a `?`. Known/Suspected assets render id + state +
/// group count. Single flattened line (the lean surgery caps it further).
/// (2026-08-22 living-world) Stale terminal assets drop out of the id list
/// and collapse into a trailing `remnants=<n>` count — the tracker doesn't
/// need dead ids it can no longer mutate usefully.
pub fn render_tracker_slice(map: &SiteMap, now_minutes: i64) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut hidden = 0usize;
    for area in &map.areas {
        match area.knowledge {
            AreaKnowledge::Unrevealed => hidden += 1,
            AreaKnowledge::Visited => {
                let doors: Vec<String> = area
                    .connections
                    .iter()
                    .map(|c| format!("{}:{}", c.to, c.state.word()))
                    .collect();
                if doors.is_empty() {
                    parts.push(format!("{}:v", area.id));
                } else {
                    parts.push(format!("{}:v doors={}", area.id, doors.join(",")));
                }
            }
            AreaKnowledge::Discovered => parts.push(format!("{}:d", area.id)),
        }
    }
    let mut assets: Vec<String> = Vec::new();
    // (2026-08-22 living-world) Collapsed remnants keep their `id:state`
    // pairs in the TRACKER slice (capped like every bounded list): the
    // narrator-facing `remnants:` line renders prose names, but the tracker
    // must still be able to TARGET a day-old corpse for the sanctioned
    // Dead → Taken loot transition — a bare count made stale assets
    // effectively unlootable.
    let mut remnants: Vec<String> = Vec::new();
    for a in &map.assets {
        if a.knowledge == AssetKnowledge::Unrevealed {
            continue;
        }
        if collapses_to_remnant(a, now_minutes) {
            remnants.push(format!("{}:{}", a.id, a.state.word()));
            continue;
        }
        let mut p = format!("{}:{}", a.id, a.state.word());
        if a.count > 0 {
            p.push_str(&format!("x{}", a.count));
        }
        if a.knowledge == AssetKnowledge::Suspected {
            p.push('?');
        }
        assets.push(p);
    }
    if parts.is_empty() {
        return None;
    }
    let mut line = format!(
        "areas={} assets={}",
        parts.join(","),
        if assets.is_empty() {
            "-".to_string()
        } else {
            assets.join(",")
        }
    );
    if hidden > 0 {
        line.push_str(&format!(" hidden={hidden}"));
    }
    if !remnants.is_empty() {
        let extra = remnants.len().saturating_sub(6);
        let shown: Vec<&str> = remnants.iter().take(6).map(String::as_str).collect();
        line.push_str(&format!(" remnants={}", shown.join(",")));
        if extra > 0 {
            line.push_str(&format!("(+{extra})"));
        }
    }
    Some(flatten(&line))
}

// ---------------------------------------------------------------------------
// The PLAYER's slice (2026-08-23) — the fog-of-war map DTO
// ---------------------------------------------------------------------------

/// The knowledge word for a `PlayerMapArea` — the one glue spot between
/// `AreaKnowledge` and the wire string (2026-08-23).
impl AreaKnowledge {
    pub fn knowledge_word(self) -> &'static str {
        match self {
            AreaKnowledge::Unrevealed => "unrevealed",
            AreaKnowledge::Discovered => "discovered",
            AreaKnowledge::Visited => "visited",
        }
    }
}

/// One knowledge-filtered area for the player-facing map (IPC
/// `fable_site_map_get` → `engine/site-map.js`). Unrevealed areas surface
/// ONLY as fog stubs: a synthetic `?N` id (the `?` cannot appear in a kebab
/// id, so it can never collide with a real area), no name, no geometry, no
/// assets — exactly the `?` stub Multihog's player view renders.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlayerMapArea {
    pub id: String,
    /// Diegetic name; EMPTY on a fog stub.
    #[serde(default)]
    pub name: String,
    /// "visited" | "discovered" | "fog".
    pub knowledge: String,
    /// Geometry/sensory lines — Visited areas only (Discovered is name-only,
    /// the narrator-slice discipline).
    #[serde(default)]
    pub geometry: Vec<String>,
    /// Known/Suspected assets in this area — Visited areas only, capped at
    /// 8 per area (the narrator slice's `ASSETS_SHOWN`).
    #[serde(default)]
    pub assets: Vec<PlayerMapAsset>,
    /// (2026-08-24 review P2) How many MORE visible-knowledge assets this
    /// area holds past the 8-asset cap — the frontend's "+N" chip marker.
    /// The cap used to truncate silently, so the chip's overflow was always
    /// zero (dead code); the count makes it live without leaking anything
    /// (a NUMBER of hidden-in-plain-sight things, never their names).
    #[serde(default)]
    pub assets_overflow: usize,
    /// (2026-08-25 quest anchors) Titles of ACTIVE anchored objectives —
    /// quest titles + promise descriptions whose `area_anchor` names THIS
    /// area — the map's scroll-marker payload. Renders on KNOWN areas only
    /// (a fog anchor stays invisible until the room is revealed); capped
    /// at 3 per area (the wire-bounding law; >3 concurrent same-room
    /// objectives is pathological).
    #[serde(default)]
    pub quests: Vec<String>,
}

/// One player-visible asset. Known assets carry name + live state word +
/// group count; Suspected assets carry the name + the suspected flag ONLY
/// (suspicion never implies state — the narrator-slice law). Unrevealed
/// assets never enter the DTO at all.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlayerMapAsset {
    pub name: String,
    /// Live-state word, Known assets only ("" while Active or Suspected —
    /// an active asset renders bare, matching the narrator slice).
    #[serde(default)]
    pub state: String,
    /// Group member count (0 = not a group).
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub suspected: bool,
    /// (2026-08-25 location-card redesign) The MARKER word — the
    /// nine-marker display vocabulary the graph renders ("general" |
    /// "safe" | "shop" | "quest" | "loot" | "hazard" | "friendly" |
    /// "hostile" | "boss"), derived at slice time by [`marker_kind`]
    /// from the asset's kind + explicit tier + name vocabulary. Kind is
    /// never knowledge-gated (it rides the name; suspicion hides state,
    /// not what a thing is).
    #[serde(default)]
    pub kind: String,
}

/// One area→area route between VISIBLE areas. Known↔known edges carry both
/// real ids; known→fog edges carry the stub id — a door is a visible fact
/// (its state renders, the tracker-slice discipline), the room behind it is
/// not. Fog↔fog edges NEVER render: neither side of that door is visible,
/// so the edge would leak hidden structure (stricter than Multihog, matched
/// to the narrator slice's `?`-count semantics).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlayerMapEdge {
    pub from: String,
    pub to: String,
    /// "open" | "locked" | "blocked".
    pub state: String,
}

/// The whole player-facing map of the ACTIVE site — one IPC payload, laid
/// out by the frontend's BFS layered renderer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlayerSiteMap {
    /// Site display name: the travel node's diegetic name, or the hosted
    /// breadcrumb ("Ironhaven > The Gilded Rose") for a building child.
    pub site_name: String,
    /// Site-level danger band word — map metadata the narrator slice
    /// already renders; never per-area truth.
    pub threat: String,
    /// True when this is a hosted building interior (a child map).
    pub hosted: bool,
    /// Entrance area id (always a revealed area — the architect validates
    /// it Visited at creation).
    pub entrance: String,
    /// Area id the player currently occupies (the "you are here" anchor).
    /// Falls back to the entrance on legacy pre-WS2 saves (no
    /// `current_area` stamp).
    pub current_area: String,
    pub areas: Vec<PlayerMapArea>,
    pub edges: Vec<PlayerMapEdge>,
}

// ---------------------------------------------------------------------------
// The player-facing MARKER vocabulary (2026-08-25 location-card redesign)
// ---------------------------------------------------------------------------

/// Word-boundary hostile-noun vocabulary for the creature/group marker
/// split (the [`crate::equipment::pouch_fit`] / [`looks_like_settlement`]
/// precedent: a pure Rust name-vocab classifier). MAY-tune list — words
/// are deliberately CONSERVATIVE (unambiguous monster/criminal nouns +
/// their plurals); everything unmatched defaults to the FRIENDLY marker,
/// the safe read in settled places. States never enter this classification
/// — a dead wolf keeps its hostile marker with "(dead)" riding the hover
/// text.
const HOSTILE_NAME_WORDS: &[&str] = &[
    // outlaws & cutthroats
    "bandit", "bandits", "brigand", "brigands", "raider", "raiders",
    "marauder", "marauders", "thug", "thugs", "cutpurse", "cutpurses",
    "pickpocket", "pickpockets", "mugger", "muggers", "assassin", "assassins",
    "pirate", "pirates", "corsair", "corsairs", "cutthroat", "cutthroats",
    "outlaw", "outlaws", "criminal", "criminals", "warband", "warbands",
    // beasts & monsters
    "wolf", "wolves", "warg", "wargs", "rat", "rats", "goblin", "goblins",
    "hobgoblin", "hobgoblins", "orc", "orcs", "ogre", "ogres", "troll",
    "trolls", "spider", "spiders", "demon", "demons", "imp", "imps",
    "devil", "devils", "ghoul", "ghouls", "zombie", "zombies", "undead",
    "skeleton", "skeletons", "wraith", "wraiths", "specter", "specters",
    "spectre", "spectres", "vampire", "vampires", "ghost", "ghosts",
    "cultist", "cultists", "beast", "beasts", "monster", "monsters",
    "dragon", "dragons", "drake", "drakes", "wyvern", "wyverns", "harpy",
    "harpies", "gargoyle", "gargoyles", "gnoll", "gnolls", "kobold",
    "kobolds", "bugbear", "bugbears", "lizardfolk", "mimic", "mimics",
];

/// Commerce vocabulary for the Building marker's SHOP subtype — an active
/// trader/merchant structure ("LEDGER / trade context"). MAY-tune.
const SHOP_NAME_WORDS: &[&str] = &[
    "shop", "shoppe", "store", "market", "bazaar", "emporium", "smithy",
    "forge", "bakery", "butchery", "brewery", "winery", "tavern",
    "taverns", "exchange",
];

/// Sanctuary vocabulary for the Building marker's SAFE subtype — a
/// designated rest point where a REST can end (inn, temple, the like).
/// SAFE outranks SHOP when a name carries both. MAY-tune.
const SAFE_NAME_WORDS: &[&str] = &[
    "inn", "inns", "hostel", "hostels", "temple", "temples", "shrine",
    "shrines", "chapel", "chapels", "church", "churches", "abbey",
    "abbeys", "monastery", "monasteries", "cloister", "cloisters",
    "sanctuary", "sanctuaries", "refuge", "refuges", "safehouse",
    "homestead", "homesteads",
];

/// Word-boundary membership over a diegetic name (the
/// [`looks_like_settlement`] splitter — non-alphanumeric boundaries, so
/// hyphenation never hides a word).
fn name_has_word(name: &str, list: &[&str]) -> bool {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| list.contains(&word))
}

/// (2026-08-25 location-card redesign) Derive the player-facing MARKER
/// word for one asset — the nine-marker vocabulary the location card's
/// graph renders (`engine/site-map.js` ASSET_ICONS). The classification is
/// PURE + deterministic over tracked state:
///
/// * `Loot` → `loot` (uncollected stashes; the state word rides the hover)
/// * `Trap`/`Hazard` → `hazard` (traps + environmental dangers)
/// * `Object` → `quest` (points of interest — notice boards, shrines,
///   campsite markers; quest OBJECTIVES carry no area anchor in the
///   schema, so the interactable class stands in until they do)
/// * `Building` → `general` | `shop` | `safe` by name vocabulary
/// * `Creature`/`Group` → `boss` when the asset is Known AND the explicit
///   tier is Elite+ (the skull), else `hostile` on a hostile name word,
///   else `friendly`
///
/// The marker is a CLASS fact, never a state or knowledge fact: it applies
/// identically to Known and Suspected assets (suspicion hides state, not
/// what a thing is) and ignores `AssetState` (a dead wolf keeps its
/// marker; "(dead)" is hover text). ONE exception (2026-08-25 leak fix):
/// the boss skull is a TIER leak — tier is authored hidden truth, so a
/// merely-Suspected creature ("Something Behind the Crates") must never
/// show its strength class; only a Known creature earns the skull.
pub fn marker_kind(asset: &SiteAsset) -> &'static str {
    match asset.kind {
        AssetKind::Loot => "loot",
        AssetKind::Trap | AssetKind::Hazard => "hazard",
        AssetKind::Object => "quest",
        AssetKind::Building => {
            if name_has_word(&asset.name, SAFE_NAME_WORDS) {
                "safe"
            } else if name_has_word(&asset.name, SHOP_NAME_WORDS) {
                "shop"
            } else {
                "general"
            }
        }
        AssetKind::Creature | AssetKind::Group => {
            let boss_tier = asset
                .tier
                .as_deref()
                .and_then(parse_tier_word)
                .is_some_and(|t| {
                    matches!(
                        t,
                        AttackerTier::Elite | AttackerTier::Boss | AttackerTier::Legendary
                    )
                });
            match () {
                // The skull requires SEEN truth: Known knowledge + Elite+ tier.
                _ if boss_tier && asset.knowledge == AssetKnowledge::Known => "boss",
                _ if name_has_word(&asset.name, HOSTILE_NAME_WORDS) => "hostile",
                _ => "friendly",
            }
        }
    }
}

/// An active anchored objective for the player-facing map — `(area_id,
/// label)` pairs (quest titles + promise descriptions with a non-empty
/// `area_anchor`), collected by the `fable_site_map_get` IPC. The slice
/// matches them against THIS map's area ids; an anchor whose room is fog
/// or absent stays invisible.
pub type AnchoredObjective = (String, String);

/// Build the PLAYER-facing, knowledge-filtered map DTO — the fog-of-war
/// panel's whole payload. Hidden truth never crosses this boundary:
/// Visited areas render name + geometry + their Known/Suspected assets,
/// Discovered areas render name only, Unrevealed areas render ONLY when
/// 1-hop-adjacent to a known area (either direction of the reciprocal pair
/// — a hand-edited save may break reciprocity), as anonymous `?N` stubs.
/// Reciprocal edge pairs dedupe to one edge. Pure; no clock (stale remnant
/// collapse is a narrator-prose decluttering, not a knowledge filter — a
/// Known corpse stays Known here).
pub fn player_slice(
    map: &SiteMap,
    site_name: &str,
    hosted: bool,
    anchored_objectives: &[AnchoredObjective],
) -> PlayerSiteMap {
    const ASSETS_SHOWN: usize = 8;
    /// (2026-08-25 quest anchors) Per-area objective-title cap.
    const QUEST_TITLES_SHOWN: usize = 3;

    // Real ids of KNOWN areas (Visited or Discovered), file order.
    let known: Vec<&SiteArea> = map
        .areas
        .iter()
        .filter(|a| a.knowledge != AreaKnowledge::Unrevealed)
        .collect();
    let is_known = |id: &str| known.iter().any(|a| a.id == id);

    // (2026-08-25 quest anchors) Objective titles keyed by THIS map's
    // KNOWN area ids — the scroll-marker payload. The knowledge gate: a
    // marker may not reveal a room the player hasn't learned (an anchor on
    // a fog or absent area stays invisible until the room is revealed).
    let mut quests_by_area: HashMap<&str, Vec<String>> = HashMap::new();
    for (area_id, label) in anchored_objectives {
        if !is_known(area_id) {
            continue;
        }
        let bucket = quests_by_area.entry(area_id.as_str()).or_default();
        if bucket.len() < QUEST_TITLES_SHOWN {
            bucket.push(label.clone());
        }
    }

    // Fog stubs: unrevealed areas adjacent to a known area (either side of
    // the pair declares it). File order → stable `?N` numbering.
    let fog: Vec<&SiteArea> = map
        .areas
        .iter()
        .filter(|a| a.knowledge == AreaKnowledge::Unrevealed)
        .filter(|a| {
            a.connections.iter().any(|c| is_known(&c.to))
                || known
                    .iter()
                    .any(|k| k.connections.iter().any(|c| c.to == a.id))
        })
        .collect();

    // Real area id → visible node id (identity for known, `?N` for fog).
    let mut visible_id: HashMap<&str, String> = HashMap::with_capacity(known.len() + fog.len());
    for a in &known {
        visible_id.insert(a.id.as_str(), a.id.clone());
    }
    for (i, a) in fog.iter().enumerate() {
        visible_id.insert(a.id.as_str(), format!("?{}", i + 1));
    }

    let mut areas: Vec<PlayerMapArea> = Vec::with_capacity(known.len() + fog.len());
    for a in &known {
        let (geometry, assets, assets_overflow) = if a.knowledge == AreaKnowledge::Visited {
            let mut assets: Vec<PlayerMapAsset> = Vec::new();
            let mut assets_overflow = 0usize;
            for asset in &map.assets {
                if asset.location != a.id || asset.knowledge == AssetKnowledge::Unrevealed {
                    continue;
                }
                if assets.len() >= ASSETS_SHOWN {
                    // (2026-08-24 review P2) Keep COUNTING past the cap so
                    // the frontend's "+N" chip has a real number.
                    assets_overflow += 1;
                    continue;
                }
                assets.push(PlayerMapAsset {
                    name: asset.name.clone(),
                    state: if asset.knowledge == AssetKnowledge::Known
                        && asset.state != AssetState::Active
                    {
                        asset.state.word().to_string()
                    } else {
                        String::new()
                    },
                    // (2026-08-24 review fix) Group counts are KNOWN-only,
                    // matching the struct law ("Suspected assets carry the
                    // name + the suspected flag ONLY") — a magnitude badge on
                    // a suspected warband told the player exactly how many
                    // wait behind the rumor. Same gate as the narrator slice.
                    count: if asset.knowledge == AssetKnowledge::Known {
                        asset.count
                    } else {
                        0
                    },
                    suspected: asset.knowledge == AssetKnowledge::Suspected,
                    kind: marker_kind(asset).to_string(),
                });
            }
            (
                a.geometry.iter().map(|g| flatten(g)).collect(),
                assets,
                assets_overflow,
            )
        } else {
            (Vec::new(), Vec::new(), 0)
        };
        areas.push(PlayerMapArea {
            id: a.id.clone(),
            name: a.name.clone(),
            knowledge: a.knowledge.knowledge_word().to_string(),
            geometry,
            assets,
            assets_overflow,
            quests: quests_by_area.get(a.id.as_str()).cloned().unwrap_or_default(),
        });
    }
    for a in &fog {
        areas.push(PlayerMapArea {
            id: visible_id[a.id.as_str()].clone(),
            name: String::new(),
            knowledge: "fog".to_string(),
            geometry: Vec::new(),
            assets: Vec::new(),
            assets_overflow: 0,
            quests: Vec::new(),
        });
    }

    // Edges between visible areas, at least one endpoint KNOWN, deduped per
    // unordered pair (reciprocity makes every real edge appear twice).
    let mut edges: Vec<PlayerMapEdge> = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    for a in &map.areas {
        for c in &a.connections {
            let (Some(from), Some(to)) = (
                visible_id.get(a.id.as_str()),
                visible_id.get(c.to.as_str()),
            ) else {
                continue;
            };
            if !is_known(&a.id) && !is_known(&c.to) {
                continue; // fog↔fog — neither side is visible
            }
            let key = if from <= to {
                (from.clone(), to.clone())
            } else {
                (to.clone(), from.clone())
            };
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            edges.push(PlayerMapEdge {
                from: from.clone(),
                to: to.clone(),
                state: c.state.word().to_string(),
            });
        }
    }

    PlayerSiteMap {
        site_name: site_name.to_string(),
        threat: map.threat.word().to_string(),
        hosted,
        entrance: map.entrance.clone(),
        current_area: map
            .current_area
            .clone()
            .unwrap_or_else(|| map.entrance.clone()),
        areas,
        edges,
    }
}

/// (2026-08-22 multihog WS3) The per-node pressure queue cap (FIFO —
/// the seed-hook discipline).
pub const NODE_PRESSURE_MAX: usize = 3;

/// (2026-08-22 multihog WS3) Push one pressure line onto a node's pending
/// queue: `clean_free_text`-capped at the seed-hook char budget, exact-
/// duplicate-suppressed, FIFO at [`NODE_PRESSURE_MAX`]. Returns true when
/// the queue changed. Pure.
pub fn push_node_pressure(node: &mut crate::schema::Node, line: &str) -> bool {
    let cleaned = crate::bracket_parser::clean_free_text(line, SITE_SEED_CHAR_MAX);
    if cleaned.is_empty() {
        return false;
    }
    if node.pending_pressure.iter().any(|p| *p == cleaned) {
        return false;
    }
    node.pending_pressure.push(cleaned);
    let overflow = node.pending_pressure.len().saturating_sub(NODE_PRESSURE_MAX);
    if overflow > 0 {
        node.pending_pressure.drain(..overflow);
    }
    true
}

// ---------------------------------------------------------------------------
// Traversal gating + the GM hidden-contents block (multihog WS2, 2026-08-22)
// ---------------------------------------------------------------------------

/// The state of the `from → to` connection: `None` when either area is
/// unknown, `Some(state)` for a direct edge — and (2026-08-24 traversal
/// fix) `Some(ConnState::Blocked)` when BOTH areas exist but no direct
/// edge connects them. For the `[ROOM]` traversal gate an absent way is an
/// impassable way: the old `None` for a known-but-unconnected pair let a
/// `[ROOM vault visited]` from the gatehouse teleport across the map (the
/// locked-door bypass — locked/blocked edges refused while a MISSING edge
/// passed). `classify_unlock` checks adjacency itself before consulting
/// this, so `[UNLOCK]` keeps its distinct NotAdjacent directive. Pure read.
pub fn connection_state_between(map: &SiteMap, from: &str, to: &str) -> Option<ConnState> {
    let from_area = map.areas.iter().find(|a| a.id == from)?;
    if !map.areas.iter().any(|a| a.id == to) {
        return None;
    }
    Some(
        from_area
            .connections
            .iter()
            .find(|c| c.to == to)
            .map(|c| c.state)
            .unwrap_or(ConnState::Blocked),
    )
}

/// (2026-08-23) True when a walk exists from `from` to `to` over OPEN
/// connections only (locked/blocked bar off-screen movement — walls are
/// walls at 3 a.m. too). Distinct from `validate`'s reachability BFS, which
/// runs over ALL edges: a locked door keeps an area reachable for map
/// validity while still blocking travel. Graph is ≤8 areas — trivial.
/// `from == to` is always true (a same-area move is a no-op walk).
pub fn open_path_exists(map: &SiteMap, from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    let mut queue: Vec<&str> = vec![from];
    let mut seen: Vec<&str> = vec![from];
    while let Some(cur) = queue.pop() {
        let Some(area) = map.areas.iter().find(|a| a.id == cur) else {
            continue;
        };
        for c in &area.connections {
            if c.state != ConnState::Open {
                continue;
            }
            if c.to.as_str() == to {
                return true;
            }
            if !seen.contains(&c.to.as_str()) {
                seen.push(c.to.as_str());
                queue.push(c.to.as_str());
            }
        }
    }
    false
}

/// (2026-08-23) A revealed asset reveals its room: a non-Unrevealed asset
/// sitting in an Unrevealed area raises that area to Discovered (Visited
/// never downgrades). Closes the dead-write hole where `[ASSET x known]` on
/// an architect-seeded Suspected asset in a hidden room landed nowhere —
/// both slices render assets only under revealed areas. Unrevealed →
/// Discovered only; no-op otherwise.
pub fn promote_area_knowledge_for_asset(map: &mut SiteMap, asset_id: &str) {
    let Some(asset) = map.assets.iter().find(|a| a.id == asset_id) else {
        return;
    };
    if asset.knowledge == AssetKnowledge::Unrevealed {
        return;
    }
    let loc = asset.location.clone();
    if let Some(area) = map
        .areas
        .iter_mut()
        .find(|a| a.id == loc && a.knowledge == AreaKnowledge::Unrevealed)
    {
        area.knowledge = AreaKnowledge::Discovered;
    }
}

/// The `[UNLOCK]` classification (the applier's immutable peek): what
/// would happen to the reciprocal pair between the player's current area
/// and `target`. Split from [`apply_unlock`] so the applier can snapshot
/// BEFORE mutating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockOutcome {
    /// Locked → the flip applies (a real mutation).
    Flip,
    /// Already open — a no-op.
    AlreadyOpen,
    /// Blocked — an obstruction needs physical change, never a key.
    Blocked,
    /// The target area id is not part of this site.
    UnknownArea,
    /// No direct connection between the two areas.
    NotAdjacent,
}

/// Classify a `[UNLOCK <target>]` against the map (pure). Falls back to
/// the entrance when `current_area` is unset (legacy pre-WS2 saves — the
/// entrance is where the player came in).
pub fn classify_unlock(map: &SiteMap, target: &str) -> UnlockOutcome {
    let from = map
        .current_area
        .clone()
        .unwrap_or_else(|| map.entrance.clone());
    if !map.areas.iter().any(|a| a.id == target) {
        return UnlockOutcome::UnknownArea;
    }
    // (2026-08-24 traversal fix) Adjacency is checked DIRECTLY:
    // `connection_state_between` now folds "no direct edge between two
    // existing areas" into `Blocked` for the `[ROOM]` gate, but `[UNLOCK]`
    // must keep the two apart — a non-adjacent target gets the
    // walk-the-path directive, never the physical-change wording for a way
    // that does not exist.
    let adjacent = map
        .areas
        .iter()
        .find(|a| a.id == from)
        .is_some_and(|a| a.connections.iter().any(|c| c.to == target));
    if !adjacent {
        return UnlockOutcome::NotAdjacent;
    }
    match connection_state_between(map, &from, target) {
        Some(ConnState::Open) => UnlockOutcome::AlreadyOpen,
        Some(ConnState::Blocked) => UnlockOutcome::Blocked,
        Some(ConnState::Locked) => UnlockOutcome::Flip,
        None => UnlockOutcome::NotAdjacent,
    }
}

/// Apply the flip (`Locked → Open` on BOTH halves of the reciprocal
/// pair). Only valid after [`classify_unlock`] returned [`UnlockOutcome::
/// Flip`] — the other outcomes mutate nothing and this fn no-ops on them.
pub fn apply_unlock(map: &mut SiteMap, target: &str) {
    let from = map
        .current_area
        .clone()
        .unwrap_or_else(|| map.entrance.clone());
    for area in map.areas.iter_mut() {
        if area.id == from {
            for c in area.connections.iter_mut() {
                if c.to == target {
                    c.state = ConnState::Open;
                }
            }
        }
        if area.id == target {
            for c in area.connections.iter_mut() {
                if c.to == from {
                    c.state = ConnState::Open;
                }
            }
        }
    }
}

/// The `[UNLOCK]` applier's one-call form (classify + apply): `Ok(true)` =
/// flipped; `Ok(false)` = already open (no-op); `Err` = a human-readable
/// reject. The lib.rs applier uses the split pair (peek → snapshot → flip)
/// for its pre-mutation snapshot discipline; tests use this form — hence
/// `#[cfg(test)]` (no dead-code warning in release).
#[cfg(test)]
pub fn unlock_connection_pair(map: &mut SiteMap, target: &str) -> Result<bool, String> {
    match classify_unlock(map, target) {
        UnlockOutcome::AlreadyOpen => Ok(false),
        UnlockOutcome::Blocked => Err(format!(
            "The way into \"{target}\" is BLOCKED, not locked — rubble and cave-ins need \
             physical change in the narrative, never a key."
        )),
        UnlockOutcome::UnknownArea => Err(format!(
            "Area \"{target}\" is not part of this site. Use an area id from the site block."
        )),
        UnlockOutcome::NotAdjacent => Err(format!(
            "\"{target}\" does not connect to the area the player occupies — unlock the \
             next way on the path, not one further off."
        )),
        UnlockOutcome::Flip => {
            apply_unlock(map, target);
            Ok(true)
        }
    }
}

/// Threat-rank one asset for the GM block: Creature/Group first (tier
/// legendary > boss > elite > soldier > minion, then member count), then
/// Trap/Hazard, then Object/Loot. Lower = more newsworthy.
fn gm_hidden_rank(a: &SiteAsset) -> (u8, u8, u32) {
    let class = match a.kind {
        AssetKind::Creature | AssetKind::Group => 0,
        AssetKind::Trap | AssetKind::Hazard => 1,
        // Buildings are structures, not threats — the hidden-truth block
        // never wastes an entity slot on one (its interior has its own map).
        AssetKind::Object | AssetKind::Loot | AssetKind::Building => 2,
    };
    let tier_rank = match a.tier.as_deref().and_then(parse_tier_word) {
        Some(crate::player_state::AttackerTier::Legendary) => 0,
        Some(crate::player_state::AttackerTier::Boss) => 1,
        Some(crate::player_state::AttackerTier::Elite) => 2,
        Some(crate::player_state::AttackerTier::Soldier) => 3,
        Some(crate::player_state::AttackerTier::Minion) => 4,
        None => 5,
    };
    (class, tier_rank, a.count)
}

/// (2026-08-22 multihog WS2) The NARRATOR-ONLY GM hidden-contents block —
/// HER eyes, never the tracker's. Strictly 1-hop: hidden-truth assets
/// (knowledge Unrevealed/Suspected) in the player's current area, plus the
/// occupants of directly-connected areas (Locked/Blocked connections
/// included — truth behind a locked door is the point). Ranked
/// [`GM_HIDDEN_MAX_ENTITIES`] entities: Creature/Group first (tier, then
/// count), then Trap/Hazard, then Object/Loot. One line each (≤160 chars)
/// inside a `<hidden_truth>` frame carrying its own positive-form law.
/// Whole block ≤ [`GM_HIDDEN_BLOCK_CHARS`] chars. `now_minutes` is unused
/// for selection but kept for signature symmetry with the other renders.
///
/// **Leak discipline:** rendered ONLY by the API narrator's turn tail (+
/// the dev local-narrator arm) — NEVER `render_tracker_slice`, never
/// `to_json_prompt`, never saves or session messages.
pub fn render_gm_hidden_slice(map: &SiteMap, _now_minutes: i64) -> Option<String> {
    let base = map.current_area.as_deref().unwrap_or(&map.entrance);
    let Some(base_area) = map.areas.iter().find(|a| a.id == base) else {
        return None;
    };
    let mut hop_ids: Vec<&str> = vec![base_area.id.as_str()];
    hop_ids.extend(base_area.connections.iter().map(|c| c.to.as_str()));
    let mut candidates: Vec<&SiteAsset> = map
        .assets
        .iter()
        .filter(|a| {
            matches!(
                a.knowledge,
                AssetKnowledge::Unrevealed | AssetKnowledge::Suspected
            ) && hop_ids.contains(&a.location.as_str())
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    // Deterministic: rank, then stable file order.
    candidates.sort_by_key(|a| gm_hidden_rank(a));
    let mut lines: Vec<String> = Vec::new();
    for a in candidates.into_iter().take(GM_HIDDEN_MAX_ENTITIES) {
        let loc = if a.location == base {
            String::new()
        } else {
            format!(" beyond the way to {}", area_name(map, &a.location))
        };
        let mut line = match a.knowledge {
            AssetKnowledge::Suspected => format!("suspected: {}{}", a.name, loc),
            _ => format!("{}{}", a.name, loc),
        };
        if a.count > 0 {
            line.push_str(&format!(" ×{}", a.count));
        }
        if let Some(tier) = a.tier.as_deref() {
            if !tier.is_empty() {
                line.push_str(&format!(" ({tier})"));
            }
        }
        if a.state != AssetState::Active {
            line.push_str(&format!(" — {}", a.state.word()));
        }
        lines.push(format!("- {}", flatten(&line).chars().take(GM_HIDDEN_LINE_CHARS).collect::<String>()));
    }
    let mut block = String::new();
    block.push_str("<hidden_truth>\n");
    block.push_str(
        "GM truth, one step around the player. Shape sensory detail and adjudication with \
         it; it enters open play only through discovery, checks, and consequence.\n",
    );
    for l in &lines {
        block.push_str(l);
        block.push('\n');
    }
    block.push_str("</hidden_truth>");
    while block.chars().count() > GM_HIDDEN_BLOCK_CHARS && lines.len() > 1 {
        // Trim ranked-least-important lines off the tail; the frame + the
        // law always survive.
        lines.pop();
        block = format!(
            "<hidden_truth>\nGM truth, one step around the player. Shape sensory detail and \
             adjudication with it; it enters open play only through discovery, checks, and \
             consequence.\n{}\n</hidden_truth>",
            lines.join("\n")
        );
    }
    Some(block)
}

// ---------------------------------------------------------------------------
// Asset resolution + threat tiers
// ---------------------------------------------------------------------------

/// Resolve a `[ASSET]` surface form against the map's assets: exact id
/// first, then a UNIQUE ≥4-char name-fragment pass (the `resolve_npc_surface`
/// mirror — "warband" → "warband-scouts" via id or name words). Ambiguous
/// (2+ hits) or unspecific fragments return `None` (the caller's reject).
pub fn resolve_asset<'a>(map: &'a SiteMap, surface: &str) -> Option<&'a SiteAsset> {
    let c = surface.trim();
    if let Some(a) = map.assets.iter().find(|a| a.id.eq_ignore_ascii_case(c)) {
        return Some(a);
    }
    let words: Vec<String> = word_list(c);
    if words.iter().map(|w| w.chars().count()).max().unwrap_or(0) < 4 {
        return None;
    }
    let mut hits: Vec<&SiteAsset> = map
        .assets
        .iter()
        .filter(|a| {
            let mut aw = word_list(&a.id);
            aw.extend(word_list(&a.name));
            // Strict subset: every fragment word present AND the entry
            // strictly longer (an equal match was the exact pass above).
            aw.len() > words.len() && words.iter().all(|w| aw.contains(w))
        })
        .collect();
    hits.dedup_by(|a, b| a.id == b.id);
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}

/// Lowercase word list split on every non-alphanumeric char (the schema.rs
/// `word_list` discipline, kept local so this module stays self-contained).
fn word_list(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse an explicit asset tier word into an `AttackerTier`.
pub fn parse_tier_word(s: &str) -> Option<AttackerTier> {
    match s.trim().to_lowercase().as_str() {
        "minion" => Some(AttackerTier::Minion),
        "soldier" => Some(AttackerTier::Soldier),
        "elite" => Some(AttackerTier::Elite),
        "boss" => Some(AttackerTier::Boss),
        "legendary" => Some(AttackerTier::Legendary),
        _ => None,
    }
}

/// The combat-Referee tier a site of this threat band defaults to.
pub fn threat_default_tier(threat: SiteThreat) -> AttackerTier {
    match threat {
        SiteThreat::Low => AttackerTier::Minion,
        SiteThreat::Moderate => AttackerTier::Soldier,
        SiteThreat::High => AttackerTier::Elite,
        SiteThreat::Deadly => AttackerTier::Boss,
    }
}

/// The strongest mob tier the player can currently blunder into at this
/// site: max over Known/Suspected Creature/Group assets that are not
/// Dead/Taken (nor Deactivated/Fleeing — a disarmed mechanism poses
/// nothing and a fleeing host is leaving, 2026-08-22). An asset's explicit
/// `tier` wins; otherwise the site's `threat` default. `None` when nothing
/// hostile is on the table (the Referee falls back to its own entity-tier
/// selection).
pub fn present_mob_tier(map: &SiteMap) -> Option<AttackerTier> {
    let fallback = threat_default_tier(map.threat);
    let mut best: Option<AttackerTier> = None;
    for a in &map.assets {
        if a.knowledge == AssetKnowledge::Unrevealed {
            continue;
        }
        if !matches!(a.kind, AssetKind::Creature | AssetKind::Group) {
            continue;
        }
        if matches!(
            a.state,
            AssetState::Dead
                | AssetState::Taken
                | AssetState::Deactivated
                | AssetState::Fleeing
        ) {
            continue;
        }
        let tier = a.tier.as_deref().and_then(parse_tier_word).unwrap_or(fallback);
        best = Some(match best {
            Some(b) => b.max(tier),
            None => tier,
        });
    }
    best
}

/// (2026-08-23 hazard referees) Rumor → Suspected-asset seeding — the
/// rumor mill grows teeth on the hidden maps. A THREAT-stem rumor
/// ([`crate::hazard::is_threat_rumor`]) heard at a node mints a
/// **Suspected**, Active Creature/Group asset at the map's ENTRANCE area:
/// suspicion, never truth (the [`AssetKnowledge::Suspected`] render
/// contract — sounds/tracks/a smell, never the stat block). The asset is
/// `Evolved`-origin (off-screen truth) with `cause = "rumor: <label>"` as
/// the dedupe key. Invisible until encountered — NO digest line (the
/// knowledge-safe channel law: the player heard the rumor, the map
/// quietly agrees).
///
/// Caps: at most [`RUMOR_ASSET_MAX`] rumor-seeded assets per map + the
/// global [`MAX_SITE_ASSETS`] gate; a repeated label dedupes by cause.
/// Group-vs-Creature comes from the matched threat WORD (a word ending in
/// "s" is a group: "bandits" → a Bandit Group). Returns `true` when an
/// asset was minted. Pure-in, mutate-out — the caller owns the schema
/// snapshot. Called by the `[RUMOR]` applier (at the origin node) + the
/// tick's propagation apply (at each newly-reached node).
pub const RUMOR_ASSET_MAX: usize = 2;

pub fn seed_rumor_asset(map: &mut SiteMap, label: &str, now_minutes: i64) -> bool {
    let Some(word) = crate::hazard::rumor_threat_word(label) else {
        return false;
    };
    // Canonical Title-case name from the matched word.
    let name: String = {
        let mut chars = word.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
            None => return false,
        }
    };
    let is_group = word.ends_with('s');
    let cause: String = format!(
        "rumor: {}",
        label.chars().take(DIGEST_LINE_CHARS).collect::<String>()
    );
    // (2026-08-24 review P1) Dedupe by cause-label, by ASSET ID, and the
    // two caps. The id check is the id-uniqueness invariant every other
    // mint path enforces: the id is `kebabify(threat_word)`, so two
    // different rumor labels carrying the same threat word ("bandits raid
    // the road" + "the bandits robbed the mill") used to mint two assets
    // with id `bandits` — id-keyed consumers are first-match-wins, so the
    // duplicate silently shadowed. A second rumor of the same word adds no
    // information; skip it.
    let id = kebabify(&word);
    if map.assets.iter().any(|a| a.cause == cause || a.id == id) {
        return false;
    }
    let rumor_seeded = map
        .assets
        .iter()
        .filter(|a| a.cause.starts_with("rumor: "))
        .count();
    if rumor_seeded >= RUMOR_ASSET_MAX || map.assets.len() >= MAX_SITE_ASSETS {
        return false;
    }
    map.assets.push(SiteAsset {
        id,
        name,
        kind: if is_group {
            AssetKind::Group
        } else {
            AssetKind::Creature
        },
        location: map.entrance.clone(),
        state: AssetState::Active,
        knowledge: AssetKnowledge::Suspected,
        // (2026-08-24 review P1) A Group's count must land in 1..=99 (the
        // validator's hard rule); 0 was a law violation that only survived
        // because renderers gate on it. 1 = "a rumor of at least one" —
        // suspicion, never a stat block.
        count: if is_group { 1 } else { 0 },
        detail: String::new(),
        tier: None,
        origin: AssetOrigin::Evolved,
        changed_at_minutes: now_minutes,
        cause,
        actor: String::new(),
        expires_at_minutes: None,
    });
    true
}

// ---------------------------------------------------------------------------
// Site evolution (the deferred mutation pass, 2026-08-22 living-world)
// ---------------------------------------------------------------------------

/// One constrained mutation the world-progression LLM pass may emit for a
/// DEPARTED mapped site. The op-set is CLOSED to exactly four forms —
/// `set_asset`, `move_asset`, `remove_asset`, `add_asset` (the 2026-08-24
/// Part II restock op) — no node creation, no off-screen lore leakage
/// (scope guard by construction). `add_asset` is the ONE sanctioned asset
/// creation: bounded by [`MAX_SITE_ASSETS`], lands only in an EXISTING
/// area, `Unrevealed` knowledge (the player learns of the arrival on
/// encounter — the knowledge-safe channel law), and a required cause.
/// Parsed tolerantly via serde: an unknown `op` word rejects at the apply,
/// never panics.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SiteEvolutionOp {
    /// `"set_asset" | "move_asset" | "remove_asset" | "add_asset"`.
    #[serde(default)]
    pub op: String,
    /// Target asset id (exact or a unique ≥4-char fragment — the
    /// `resolve_asset` gate). Unused by `add_asset` (the new id rides
    /// `id`).
    #[serde(default)]
    pub asset: String,
    /// `add_asset` only: the new asset's kebab id (defaults to
    /// `kebabify(name)`; must not collide with an existing asset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// `add_asset` only: the display name (≤64, flattened).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `add_asset` only: creature|group|object|trap|hazard|loot — buildings
    /// never arrive off-screen (the depth-2 law).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `add_asset` only: the EXISTING area id the arrival lands in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// `set_asset` only: the target state word (parsed via
    /// `parse_asset_state_word`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// `set_asset` only: a Group's new member count (clamped 1..=99).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// `set_asset` only: an optional fresh detail line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// `move_asset` only: the target AREA id (must exist on the map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// What caused the change ("scavengers stripped the bodies") — ≤160.
    #[serde(default)]
    pub cause: String,
    /// Who/what made it happen ("the town watch") — ≤64.
    #[serde(default)]
    pub actor: String,
}

/// Char cap for the actor field (the `clean_free_text` discipline).
pub const ASSET_ACTOR_CHAR_MAX: usize = 64;

/// Apply a batch of evolution ops to one map. Returns `(applied_count,
/// rejects)` — rejects are human-readable directive lines the tick folds
/// into the failed-attempt retry so the NEXT pass sees the correction.
///
/// Laws enforced HERE (the Rust applier is the authority):
/// - **Play-canon locks:** a transition out of a terminal state that
///   [`canon_transition`] refuses REJECTS with the remnant-entity wording.
/// - **(2026-08-24 Part II B1) Restock:** `add_asset` mints ONLY into an
///   existing area, under [`MAX_SITE_ASSETS`], with a required cause,
///   `Unrevealed` knowledge (the knowledge-safe channel law — no digest
///   line), `Evolved` origin, and no building kinds (the depth-2 law).
/// - Every applied op stamps `changed_at_minutes` + `cause` + `actor` and
///   flips `origin` to [`AssetOrigin::Evolved`], and appends one bounded
///   line to `pending_digest` (the re-entry briefing).
/// - (2026-08-23 WS5) **Causal threads:** every applied op closes open
///   threads on its asset/areas (pure key match); a set_asset into `Dead`
///   and a live-asset removal OPEN one (see the thread ledger below).
/// - Pure: no clock reads — `now_minutes` is passed in.
pub fn apply_evolution_ops(
    map: &mut SiteMap,
    ops: &[SiteEvolutionOp],
    now_minutes: i64,
) -> (usize, Vec<String>) {
    let mut applied = 0usize;
    let mut rejects: Vec<String> = Vec::new();
    for op in ops {
        let cause = crate::bracket_parser::clean_free_text(&op.cause, SITE_DETAIL_CHAR_MAX);
        let actor = crate::bracket_parser::clean_free_text(&op.actor, ASSET_ACTOR_CHAR_MAX);
        // (2026-08-24 Part II B1) ADD_ASSET mints its target, so it runs
        // BEFORE the resolve gate (an empty/unresolvable `asset` field is
        // expected here — the new id rides `id`/`name`).
        if op.op.trim().eq_ignore_ascii_case("add_asset") {
            // Restock law: a cause ("scavengers moved in") is REQUIRED —
            // an arrival without a why is pure invention.
            if cause.is_empty() {
                rejects.push(
                    "site op add_asset: a cause (why they came) is required.".to_string(),
                );
                continue;
            }
            let Some(name) = op
                .name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| flatten(s).chars().take(64).collect::<String>())
                .filter(|s| !s.is_empty())
            else {
                rejects.push("site op add_asset: a name is required.".to_string());
                continue;
            };
            let Some(kind) = op
                .kind
                .as_deref()
                .and_then(parse_asset_kind_word)
            else {
                rejects.push(
                    "site op add_asset: kind must be one of creature, group, object, trap, hazard, loot."
                        .to_string(),
                );
                continue;
            };
            let Some(area) = op
                .area
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                rejects.push(
                    "site op add_asset: an existing area id (area=<area_id>) is required — never add areas."
                        .to_string(),
                );
                continue;
            };
            if !map.areas.iter().any(|a| a.id == area) {
                rejects.push(format!(
                    "site op add_asset: area \"{area}\" is not part of this site."
                ));
                continue;
            }
            if map.assets.len() >= MAX_SITE_ASSETS {
                rejects.push(format!(
                    "site op add_asset: map asset cap reached ({MAX_SITE_ASSETS})."
                ));
                continue;
            }
            let mut id = op
                .id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(kebabify)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| kebabify(&name));
            if id.is_empty() {
                id = "arrival".to_string();
            }
            if map.assets.iter().any(|a| a.id == id) {
                rejects.push(format!(
                    "site op add_asset: id \"{id}\" already exists on this site — pick another."
                ));
                continue;
            }
            let count = if kind == AssetKind::Group {
                op.count.unwrap_or(1).clamp(1, ASSET_COUNT_MAX)
            } else {
                0
            };
            // Arrival is activity in the area — open questions there
            // resolve (the move_asset arrival discipline).
            close_threads_by_area(map, area, &cause);
            map.assets.push(SiteAsset {
                id,
                name,
                kind,
                location: area.to_string(),
                state: AssetState::Active,
                // Knowledge-safe channel law (the seed_rumor_asset
                // precedent): off-screen truth is Unrevealed — the player
                // learns of the arrival on encounter, and NO digest line
                // is written (the map quietly agrees).
                knowledge: AssetKnowledge::Unrevealed,
                count,
                detail: String::new(),
                tier: None,
                origin: AssetOrigin::Evolved,
                changed_at_minutes: now_minutes,
                cause,
                actor,
                expires_at_minutes: None,
            });
            applied += 1;
            continue;
        }
        let Some(asset_id) = resolve_asset(map, &op.asset).map(|a| a.id.clone()) else {
            rejects.push(format!(
                "site op: asset \"{}\" is not part of this site — use an asset id from the site slice.",
                op.asset
            ));
            continue;
        };
        match op.op.trim().to_lowercase().as_str() {
            "set_asset" => {
                let Some(word) = op.state.as_deref().and_then(parse_asset_state_word) else {
                    rejects.push(format!(
                        "site op set_asset {asset_id}: state must be one of active, dead, taken, triggered, deactivated, fleeing."
                    ));
                    continue;
                };
                let Some(a) = map.assets.iter().find(|a| a.id == asset_id) else {
                    continue;
                };
                if !canon_transition(a.state, word) {
                    rejects.push(format!(
                        "{} is {} (terminal, play-canon locked) — add a new remnant entity instead of resurrecting.",
                        a.name,
                        a.state.word()
                    ));
                    continue;
                }
                // (2026-08-23 WS5) Causal-thread resolution — deterministic,
                // key-matched ONLY (never cause-string inference): activity
                // in the subject's area resolves every open question there,
                // and a non-Dead state write resolves the subject's own
                // thread (the Dead→Taken loot claim closes the death
                // question). A Dead write OPENS (or restates) below.
                let (thread_loc, thread_name, thread_hidden) = map
                    .assets
                    .iter()
                    .find(|a| a.id == asset_id)
                    .map(|a| {
                        (
                            a.location.clone(),
                            a.name.clone(),
                            a.knowledge == AssetKnowledge::Unrevealed,
                        )
                    })
                    .unwrap_or_default();
                close_threads_by_area(map, &thread_loc, &cause);
                if word != AssetState::Dead {
                    close_threads_by_subject(map, &asset_id, &cause);
                }
                let Some(a) = map.assets.iter_mut().find(|a| a.id == asset_id) else {
                    continue;
                };
                a.state = word;
                if let Some(n) = op.count {
                    if a.kind == AssetKind::Group {
                        a.count = n.clamp(1, ASSET_COUNT_MAX);
                    }
                }
                if let Some(d) = op.detail.as_deref() {
                    let d = crate::bracket_parser::clean_free_text(d, SITE_DETAIL_CHAR_MAX);
                    if !d.is_empty() {
                        a.detail = d;
                    }
                }
                a.cause = cause.clone();
                a.actor = actor.clone();
                a.changed_at_minutes = now_minutes;
                stamp_evolved(a);
                let name = a.name.clone();
                // (2026-08-24 review P2) Knowledge-safe channel law: the
                // digest is the NARRATOR-facing re-entry briefing — an
                // Unrevealed asset's change must NOT name it (the player
                // never knew it existed; hidden truth never renders). The
                // mutation still applies — the map quietly agrees.
                if a.knowledge != AssetKnowledge::Unrevealed {
                    push_digest_line(map, &name, word.word(), &cause);
                }
                if word == AssetState::Dead {
                    open_thread(
                        map,
                        &asset_id,
                        &thread_name,
                        &thread_loc,
                        &actor,
                        &cause,
                        thread_hidden,
                        now_minutes,
                    );
                }
                applied += 1;
            }
            "move_asset" => {
                let Some(to) = op
                    .to
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    rejects.push(format!(
                        "site op move_asset {asset_id}: a target area id is required (to=<area_id>)."
                    ));
                    continue;
                };
                if !map.areas.iter().any(|area| area.id == to) {
                    rejects.push(format!(
                        "site op move_asset {asset_id}: area \"{to}\" is not part of this site."
                    ));
                    continue;
                }
                let (name, from, was_unrevealed) = map
                    .assets
                    .iter()
                    .find(|a| a.id == asset_id)
                    .map(|a| {
                        (
                            a.name.clone(),
                            a.location.clone(),
                            a.knowledge == AssetKnowledge::Unrevealed,
                        )
                    })
                    .unwrap_or_default();
                // (2026-08-23) BFS movement guard: the move needs a walk of
                // OPEN connections from the asset's current area — locked/
                // blocked routes bar off-screen movement too. Rejects flow
                // into the failed-attempt retry like every other reject.
                if !open_path_exists(map, &from, to) {
                    rejects.push(format!(
                        "site op move_asset {asset_id}: no open way from \"{from}\" to \"{to}\" — \
                         move along the site's open routes."
                    ));
                    continue;
                }
                // (2026-08-23 WS5) The move resolves threads on the asset
                // and in BOTH areas — departure and arrival are activity.
                close_threads_by_subject(map, &asset_id, &cause);
                close_threads_by_area(map, &from, &cause);
                close_threads_by_area(map, to, &cause);
                let Some(a) = map.assets.iter_mut().find(|a| a.id == asset_id) else {
                    continue;
                };
                a.location = to.to_string();
                a.cause = cause.clone();
                a.actor = actor.clone();
                a.changed_at_minutes = now_minutes;
                stamp_evolved(a);
                // (2026-08-24 review P2) Same knowledge-safe gate as
                // set_asset — an Unrevealed asset moves in silence.
                if !was_unrevealed {
                    push_digest_line(map, &name, "moved", &cause);
                }
                applied += 1;
            }
            "remove_asset" => {
                let (name, loc, was_terminal, was_unrevealed) = map
                    .assets
                    .iter()
                    .find(|a| a.id == asset_id)
                    .map(|a| {
                        (
                            a.name.clone(),
                            a.location.clone(),
                            is_terminal(a.state),
                            a.knowledge == AssetKnowledge::Unrevealed,
                        )
                    })
                    .unwrap_or_default();
                // (2026-08-23 WS5) Removing the subject resolves its own
                // thread and anything open in its area; a LIVE asset
                // vanishing OPENS a thread (a thing that was alive is gone —
                // theft, flight, or worse) while a terminal remnant's
                // removal is just cleanup (opens nothing).
                close_threads_by_subject(map, &asset_id, &cause);
                close_threads_by_area(map, &loc, &cause);
                map.assets.retain(|a| a.id != asset_id);
                // (2026-08-24 review P2) Same knowledge-safe gate — an
                // Unrevealed asset vanishes without a briefing line.
                if !was_unrevealed {
                    push_digest_line(map, &name, "gone", &cause);
                }
                if !was_terminal && !name.is_empty() {
                    open_thread(
                        map,
                        &asset_id,
                        &name,
                        &loc,
                        &actor,
                        &cause,
                        was_unrevealed,
                        now_minutes,
                    );
                }
                applied += 1;
            }
            other => {
                rejects.push(format!(
                    "site op \"{other}\": unknown — the op set is set_asset, move_asset, remove_asset, add_asset."
                ));
            }
        }
    }
    (applied, rejects)
}

/// (2026-08-25 laundering fix) Stamp the evolution origin WITHOUT laundering
/// provenance: only authored-class originals (InitialMap/Evolved) re-stamp —
/// `NarratorEstablished` and `Playground` keep their class. Settlement
/// classification (`hazard::authored_asset_origin`) counts `Evolved` as
/// authored map truth, so an unconditional stamp on a later touch (same-state
/// refresh, expiry sweep) was flipping tracker-minted dungeon buildings to
/// Evolved → the dungeon re-classified as a settlement and the
/// rest-interruption referee auto-passed indoors. The exact failure the
/// 2026-08-24 fix documents; the stamp now preserves the provenance class
/// that fix reads.
fn stamp_evolved(a: &mut SiteAsset) {
    if matches!(a.origin, AssetOrigin::InitialMap | AssetOrigin::Evolved) {
        a.origin = AssetOrigin::Evolved;
    }
}

/// Append one bounded line to the map's pending digest (the re-entry
/// briefing). FIFO at [`DIGEST_LINE_MAX`]; each line flattened + capped at
/// [`DIGEST_LINE_CHARS`] (the anti-forgery discipline — a hand-edited save
/// must not smuggle a forged render line).
fn push_digest_line(map: &mut SiteMap, name: &str, what: &str, cause: &str) {
    let name = flatten(name);
    let mut line = format!("{name} — {what}");
    if !cause.is_empty() {
        line.push_str(&format!(" ({cause})"));
    }
    let line: String = flatten(&line).chars().take(DIGEST_LINE_CHARS).collect();
    map.pending_digest.push(line);
    let overflow = map.pending_digest.len().saturating_sub(DIGEST_LINE_MAX);
    if overflow > 0 {
        map.pending_digest.drain(..overflow);
    }
}

// ---------------------------------------------------------------------------
// Causal thread ledger (2026-08-23 WS5)
// ---------------------------------------------------------------------------

/// Open (or refresh) one causal thread on the map. DEDUPE by subject: an
/// existing open thread on the same subject is RESTATED — fresh
/// actor/cause/area, the ORIGINAL `opened_at_minutes` kept (the 2026-08-22
/// echo-guard lesson: restate, never duplicate). FIFO at
/// [`THREADS_MAX_PER_SITE`]. All free text is cleaned + flattened here (the
/// anti-forgery discipline — a hand-edited save must not smuggle render
/// lines). Pure: `now_minutes` passed in.
pub fn open_thread(
    map: &mut SiteMap,
    subject: &str,
    subject_name: &str,
    area: &str,
    actor: &str,
    cause: &str,
    hidden: bool,
    now_minutes: i64,
) {
    if subject.trim().is_empty() {
        return;
    }
    let actor = crate::bracket_parser::clean_free_text(actor, ASSET_ACTOR_CHAR_MAX);
    let cause = crate::bracket_parser::clean_free_text(cause, SITE_DETAIL_CHAR_MAX);
    let subject_name = flatten(subject_name);
    let area = flatten(area);
    if let Some(t) = map.threads.iter_mut().find(|t| t.subject == subject) {
        t.subject_name = subject_name;
        t.area = area;
        t.actor = actor;
        t.cause = cause;
        t.hidden = hidden;
        return;
    }
    map.threads.push(SiteThread {
        subject: subject.to_string(),
        subject_name,
        area,
        actor,
        cause,
        opened_at_minutes: now_minutes,
        hidden,
    });
    let overflow = map.threads.len().saturating_sub(THREADS_MAX_PER_SITE);
    if overflow > 0 {
        map.threads.drain(..overflow);
    }
}

/// Close every open thread whose subject matches — each closure writes one
/// bounded digest line (the re-entry briefing). Returns the closure count.
pub fn close_threads_by_subject(map: &mut SiteMap, subject: &str, cause: &str) -> usize {
    if subject.is_empty() {
        return 0;
    }
    close_threads_matching(map, cause, |t| t.subject == subject)
}

/// Close every open thread rooted in the given area — later activity there
/// resolves the open questions located in it. Returns the closure count.
pub fn close_threads_by_area(map: &mut SiteMap, area: &str, cause: &str) -> usize {
    if area.is_empty() {
        return 0;
    }
    close_threads_matching(map, cause, |t| t.area == area)
}

fn close_threads_matching<F: Fn(&SiteThread) -> bool>(
    map: &mut SiteMap,
    cause: &str,
    matches: F,
) -> usize {
    let mut closed: Vec<SiteThread> = Vec::new();
    map.threads.retain(|t| {
        if matches(t) {
            closed.push(t.clone());
            false
        } else {
            true
        }
    });
    let count = closed.len();
    for t in &closed {
        // (2026-08-25) The P2 knowledge gate, thread arm: a hidden subject's
        // resolution is silent — the thread still closes, the narrator's
        // re-entry briefing never names what the player never knew.
        if !t.hidden {
            push_digest_line(map, &t.subject_name, "resolved", cause);
        }
    }
    count
}

/// Re-entry flush: the player is BACK, so every open off-screen question
/// becomes live play. Each open thread lands in the pending digest as an
/// "open question" line (the narrator dramatizes it on arrival), then the
/// ledger clears. Called by the `[TRAVEL]` arrival arms.
pub fn flush_threads_on_arrival(map: &mut SiteMap) {
    let open: Vec<(String, String, bool)> = map
        .threads
        .iter()
        .map(|t| (t.subject_name.clone(), t.cause.clone(), t.hidden))
        .collect();
    for (subject, cause, hidden) in &open {
        // (2026-08-25) Same knowledge gate: a hidden open question stays
        // hidden on arrival — the ledger clears without briefing the
        // narrator on truth the player never saw.
        if !hidden {
            push_digest_line(map, subject, "open question", cause);
        }
    }
    map.threads.clear();
}

/// Deterministic thread age-collapse: an open thread older than
/// [`THREAD_COLLAPSE_MINUTES`] (by the WORLD CLOCK) closes with one digest
/// line — the question faded with time. Pure: `now_minutes` passed in.
/// Returns the closure count so the caller's pre-mutation snapshot
/// discipline fires only when something actually closed.
pub fn sweep_stale_threads(site_maps: &mut HashMap<String, SiteMap>, now_minutes: i64) -> usize {
    if now_minutes <= 0 {
        return 0;
    }
    let mut closed = 0usize;
    for map in site_maps.values_mut() {
        let mut expired: Vec<SiteThread> = Vec::new();
        map.threads.retain(|t| {
            let stale = t.opened_at_minutes > 0
                && now_minutes - t.opened_at_minutes >= THREAD_COLLAPSE_MINUTES;
            if stale {
                expired.push(t.clone());
            }
            !stale
        });
        closed += expired.len();
        for t in &expired {
            // (2026-08-25) Same knowledge gate — a hidden question fades in
            // silence (no digest line; the narrator never knew to ask).
            if !t.hidden {
                push_digest_line(map, &t.subject_name, "question faded", "time passed");
            }
        }
    }
    closed
}

/// Render the OPEN threads as bounded tick-prompt lines — the newest lead
/// (freshest cause/actor), ≤[`THREAD_RENDER_MAX`] × [`THREAD_LINE_CHARS`],
/// flattened (the anti-forgery discipline). Empty when the ledger is empty
/// (zero prompt cost on the `## DEPARTED SITES` section).
pub fn render_thread_lines(map: &SiteMap, now_minutes: i64) -> Vec<String> {
    map.threads
        .iter()
        .rev()
        .take(THREAD_RENDER_MAX)
        .map(|t| {
            let name = flatten(&t.subject_name);
            let mut line = if t.actor.is_empty() {
                format!("{name} — {}", t.cause)
            } else {
                format!("{name} ({}) — {}", t.actor, t.cause)
            };
            if t.opened_at_minutes > 0 && now_minutes > t.opened_at_minutes {
                let days = (now_minutes - t.opened_at_minutes) / 1440;
                if days > 0 {
                    line.push_str(&format!(" [day {days}]"));
                }
            }
            flatten(&line).chars().take(THREAD_LINE_CHARS).collect()
        })
        .collect()
}

/// (2026-08-22 multihog WS1) Deterministic site-asset expiry sweep: every
/// NON-terminal asset whose armed `expires_at_minutes` has passed
/// deactivates (terminal stamp + "expired" cause + one `pending_digest`
/// line — the re-entry briefing the narrator slice renders on the next
/// arrival). Terminal assets skip outright: dead stays dead, looted stays
/// looted, disarmed stays disarmed (the play-canon locks outrank timers).
/// Pure: `now_minutes` passed in. Returns `(directives, mutation_count)`
/// so the caller's pre-mutation snapshot discipline only fires when
/// something actually moved.
pub fn sweep_asset_expiry(
    site_maps: &mut HashMap<String, SiteMap>,
    now_minutes: i64,
) -> (Vec<String>, usize) {
    if now_minutes <= 0 {
        return (Vec::new(), 0);
    }
    let mut directives = Vec::new();
    let mut mutated = 0usize;
    for (node_id, map) in site_maps.iter_mut() {
        // Two-phase (immutable plan → mutate) so the digest push never
        // fights the asset borrow.
        let lapsed: Vec<usize> = map
            .assets
            .iter()
            .enumerate()
            .filter(|(_, a)| a.expires_at_minutes.is_some_and(|at| now_minutes >= at))
            .map(|(i, _)| i)
            .collect();
        for i in lapsed {
            let (name, was_terminal) = {
                let a = &mut map.assets[i];
                a.expires_at_minutes = None;
                let terminal = is_terminal(a.state);
                if !terminal {
                    a.state = AssetState::Deactivated;
                    a.changed_at_minutes = now_minutes;
                    a.cause = "expired".to_string();
                    stamp_evolved(a);
                }
                (a.name.clone(), terminal)
            };
            mutated += 1;
            if was_terminal {
                continue;
            }
            push_digest_line(map, &name, "deactivated", "its armed time ran out");
            directives.push(format!(
                "Expired: {name} (site {node_id}) — the feature's armed time passed. \
                 Narrate the lapse as settled fact."
            ));
        }
    }
    (directives, mutated)
}

// ---------------------------------------------------------------------------
// Stale Roulette + eviction
// ---------------------------------------------------------------------------

/// The Stale Roulette: pick the `k` most-stale UN-MAPPED nodes (exclude the
/// current node + every node that already has a map — maps are write-once),
/// sorted by `last_evolved_minutes` ASC (0 = never evolved = first). The
/// world-progression tick designates these; stamping all of them each tick
/// guarantees rotation even when the pass emits no seeds.
pub fn select_stale_sites(
    graph: &TravelGraph,
    site_maps: &HashMap<String, SiteMap>,
    current: Option<&str>,
    k: usize,
) -> Vec<String> {
    let mut candidates: Vec<(i64, String)> = graph
        .nodes
        .iter()
        .filter(|n| Some(n.id.as_str()) != current && !site_maps.contains_key(&n.id))
        .map(|n| (n.last_evolved_minutes, n.id.clone()))
        .collect();
    // Deterministic: watermark first, id as the tiebreak.
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    candidates.into_iter().take(k).map(|(_, id)| id).collect()
}

/// At the `MAX_SITE_MAPS` cap, evict the least-recently-visited map that is
/// NOT the current node's (never evict the site you're standing in). Returns
/// the evicted node id, if any. A revisited site re-architects fresh — the
/// frozen-truth trade lives in the plan's risk list by design.
pub fn evict_lru_site_map(
    site_maps: &mut HashMap<String, SiteMap>,
    current_node: &str,
) -> Option<String> {
    if site_maps.len() < MAX_SITE_MAPS {
        return None;
    }
    // (2026-08-23 hosted interiors) The chain-aware freeze: the player's
    // node map, the hosted child they stand in, and the child's parent are
    // all unevictable — `player_frozen_keys` derives the whole set from
    // the same state the resolver reads.
    let frozen = player_frozen_keys(site_maps, Some(current_node));
    // (2026-08-24 review fix) Deterministic victim: watermark first, key id
    // as the tiebreak — `min_by_key` over a HashMap picked whichever
    // same-watermark entry the hasher surfaced first, so an equal-timestamp
    // eviction was non-deterministic across boots.
    let victim = site_maps
        .iter()
        .filter(|(k, _)| !frozen.contains(k))
        .min_by(|a, b| {
            a.1.last_visit_minutes
                .cmp(&b.1.last_visit_minutes)
                .then_with(|| a.0.cmp(b.0))
        })
        .map(|(k, _)| k.clone());
    if let Some(v) = &victim {
        site_maps.remove(v);
        // Evicting a PARENT also removes its hosted children — an orphaned
        // child can never be re-entered (its Building asset died with the
        // parent) and would silently squat the cap until its own LRU turn.
        let prefix = format!("{v}::");
        let orphans: Vec<String> = site_maps
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        for orphan in orphans {
            site_maps.remove(&orphan);
        }
    }
    victim
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_map() -> SiteMap {
        SiteMap {
            node_id: "warren".into(),
            threat: SiteThreat::High,
            entrance: "gatehouse".into(),
            areas: vec![
                SiteArea {
                    id: "gatehouse".into(),
                    name: "Gatehouse".into(),
                    knowledge: AreaKnowledge::Visited,
                    geometry: vec!["cold draft through murder holes".into()],
                    connections: vec![SiteConnection {
                        to: "hall".into(),
                        state: ConnState::Open,
                        detail: "arched doorway".into(),
                    }],
                },
                SiteArea {
                    id: "hall".into(),
                    name: "Great Hall".into(),
                    knowledge: AreaKnowledge::Unrevealed,
                    geometry: vec!["long trestle tables".into()],
                    connections: vec![
                        SiteConnection {
                            to: "gatehouse".into(),
                            state: ConnState::Open,
                            detail: "arched doorway".into(),
                        },
                        SiteConnection {
                            to: "vault".into(),
                            state: ConnState::Locked,
                            detail: "iron-bound door".into(),
                        },
                    ],
                },
                SiteArea {
                    id: "vault".into(),
                    name: "Vault".into(),
                    knowledge: AreaKnowledge::Unrevealed,
                    geometry: vec![],
                    connections: vec![SiteConnection {
                        to: "hall".into(),
                        state: ConnState::Locked,
                        detail: "iron-bound door".into(),
                    }],
                },
            ],
            assets: vec![
                SiteAsset {
                    id: "warband-scouts".into(),
                    name: "Warband Scouts".into(),
                    kind: AssetKind::Group,
                    location: "hall".into(),
                    state: AssetState::Active,
                    knowledge: AssetKnowledge::Unrevealed,
                    count: 6,
                    detail: "playing dice".into(),
                    tier: Some("soldier".into()),
                    origin: AssetOrigin::InitialMap,
                    changed_at_minutes: 0,
                    cause: String::new(),
                    actor: String::new(),
                    expires_at_minutes: None,
                },
                SiteAsset {
                    id: "gate-keeper".into(),
                    name: "Gate Keeper".into(),
                    kind: AssetKind::Creature,
                    location: "gatehouse".into(),
                    state: AssetState::Active,
                    knowledge: AssetKnowledge::Known,
                    count: 0,
                    detail: "bored, one eye on the road".into(),
                    tier: Some("minion".into()),
                    origin: AssetOrigin::InitialMap,
                    changed_at_minutes: 0,
                    cause: String::new(),
                    actor: String::new(),
                    expires_at_minutes: None,
                },
            ],
            last_visit_minutes: 1_000,
            pending_digest: Vec::new(),
            current_area: None,
            threads: Vec::new(),
            host: None,
            current_building: None,
        }
    }

    #[test]
    fn validate_accepts_demo_map() {
        assert_eq!(validate(&demo_map()), Ok(()), "demo map must validate");
    }

    #[test]
    fn validate_rejects_disconnected_area() {
        let mut m = demo_map();
        m.areas.push(SiteArea {
            id: "cellar".into(),
            name: "Cellar".into(),
            knowledge: AreaKnowledge::Unrevealed,
            geometry: vec![],
            connections: vec![SiteConnection {
                to: "hall".into(),
                state: ConnState::Open,
                detail: String::new(),
            }],
        });
        // Not reciprocal: hall has no edge back to cellar → both a reciprocity
        // failure AND cellar still reachable? No — reachability runs over
        // cellar's own connection so it IS reachable; the failure is the
        // missing reciprocal edge.
        let errs = validate(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("not reciprocal")),
            "expected reciprocity failure, got {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_unreachable_area() {
        let mut m = demo_map();
        // Sever the hall <-> vault pair by rewriting hall's connections.
        for area in m.areas.iter_mut() {
            if area.id == "hall" {
                area.connections.retain(|c| c.to != "vault");
            }
            if area.id == "vault" {
                // Keep only an orphan self-cluster: vault -> cellar (nonexistent)
                area.connections = vec![SiteConnection {
                    to: "vault".into(),
                    state: ConnState::Open,
                    detail: String::new(),
                }];
            }
        }
        let errs = validate(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("unreachable")),
            "expected reachability failure, got {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_second_visited_area() {
        let mut m = demo_map();
        for area in m.areas.iter_mut() {
            if area.id == "hall" {
                area.knowledge = AreaKnowledge::Visited;
            }
        }
        let errs = validate(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("only the entrance")),
            "expected visited-count failure, got {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_group_count_on_non_group() {
        let mut m = demo_map();
        m.assets[1].count = 3; // gate-keeper is a Creature, not a Group
        let errs = validate(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("non-group")),
            "expected count failure, got {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_bad_kebab_id() {
        let mut m = demo_map();
        m.areas[0].id = "Gate_House".into();
        let errs = validate(&m).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("kebab")),
            "expected kebab failure, got {errs:?}"
        );
    }

    #[test]
    fn from_model_output_parses_fenced_json() {
        let raw = "Here is the site:\n```json\n{\"node_id\":\"warren\",\"threat\":\"high\",\"entrance\":\"gatehouse\",\"areas\":[{\"id\":\"gatehouse\",\"name\":\"Gatehouse\",\"knowledge\":\"visited\",\"geometry\":[\"cold draft\"],\"connections\":[{\"to\":\"hall\",\"state\":\"open\",\"detail\":\"arch\"}]},{\"id\":\"hall\",\"name\":\"Hall\",\"knowledge\":\"unrevealed\",\"geometry\":[],\"connections\":[{\"to\":\"gatehouse\",\"state\":\"open\",\"detail\":\"arch\"}]},{\"id\":\"vault\",\"name\":\"Vault\",\"knowledge\":\"unrevealed\",\"geometry\":[],\"connections\":[{\"to\":\"hall\",\"state\":\"locked\",\"detail\":\"iron\"}]}],\"assets\":[]}\n```\nDone.";
        let map = SiteMap::from_model_output(raw).expect("fenced map parses");
        assert_eq!(map.entrance, "gatehouse");
        assert_eq!(map.threat, SiteThreat::High);
        assert!(validate(&map).is_ok(), "parsed map must validate");
    }

    #[test]
    fn narrator_slice_hides_unrevealed_truth() {
        let m = demo_map();
        let slice = render_narrator_slice(&m, 1_000).expect("non-empty slice");
        // The hall is unrevealed: its name, its geometry, and the warband
        // inside it must NOT render.
        assert!(!slice.contains("Great Hall"), "slice leaked unrevealed area name");
        assert!(!slice.contains("trestle"), "slice leaked unrevealed geometry");
        assert!(
            !slice.contains("Warband") && !slice.contains("dice"),
            "slice leaked hidden asset truth"
        );
        // The gatehouse (visited) + its known asset + the ? stub DO render.
        assert!(slice.contains("Gatehouse"));
        assert!(slice.contains("Gate Keeper"));
        assert!(slice.contains("? ways on: 1"), "unrevealed door count missing");
    }

    #[test]
    fn tracker_slice_carries_ids_and_doors() {
        let m = demo_map();
        let slice = render_tracker_slice(&m, 1_000).expect("non-empty slice");
        assert!(slice.contains("gatehouse:v"), "visited area id missing");
        assert!(
            slice.contains("doors=hall:open"),
            "door target + state missing: {slice}"
        );
        assert!(slice.contains("gate-keeper:active"), "known asset missing");
        assert!(slice.contains("hidden=2"), "unrevealed count missing");
        // Hidden truth stays hidden even in the tracker slice.
        assert!(!slice.contains("warband"), "tracker slice leaked hidden asset");
    }

    #[test]
    fn resolve_asset_exact_then_fragment() {
        let m = demo_map();
        assert_eq!(resolve_asset(&m, "gate-keeper").map(|a| a.id.as_str()), Some("gate-keeper"));
        assert_eq!(resolve_asset(&m, "GATE-KEEPER").map(|a| a.id.as_str()), Some("gate-keeper"));
        // Hidden assets resolve too — the tracker may mutate what it named.
        assert_eq!(
            resolve_asset(&m, "warband").map(|a| a.id.as_str()),
            Some("warband-scouts"),
            "unique ≥4-char fragment should resolve"
        );
        assert_eq!(resolve_asset(&m, "zz"), None, "unspecific fragment must not resolve");
    }

    #[test]
    fn present_mob_tier_skips_dead_and_hidden() {
        let mut m = demo_map();
        // Both assets hidden/unrevealed → None (nothing Known/Suspected).
        assert_eq!(present_mob_tier(&m), None);
        // Reveal the warband → soldier tier (explicit).
        m.assets[0].knowledge = AssetKnowledge::Known;
        assert_eq!(present_mob_tier(&m), Some(AttackerTier::Soldier));
        // Kill it → falls back to the (revealed) gate-keeper minion.
        m.assets[0].state = AssetState::Dead;
        m.assets[1].knowledge = AssetKnowledge::Known;
        assert_eq!(present_mob_tier(&m), Some(AttackerTier::Minion));
        // An asset with no explicit tier inherits the site threat (High → Elite).
        m.assets[1].tier = None;
        m.assets[1].kind = AssetKind::Group;
        m.assets[1].count = 4;
        assert_eq!(present_mob_tier(&m), Some(AttackerTier::Elite));
    }

    #[test]
    fn threat_default_tier_mapping() {
        assert_eq!(threat_default_tier(SiteThreat::Low), AttackerTier::Minion);
        assert_eq!(threat_default_tier(SiteThreat::Moderate), AttackerTier::Soldier);
        assert_eq!(threat_default_tier(SiteThreat::High), AttackerTier::Elite);
        assert_eq!(threat_default_tier(SiteThreat::Deadly), AttackerTier::Boss);
    }

    #[test]
    fn roulette_prioritizes_never_evolved_and_skips_mapped() {
        let mut graph = TravelGraph::default();
        graph.nodes = vec![
            crate::schema::Node {
                id: "alpha".into(),
                name: "Alpha".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 5_000,
                ..Default::default()
            },
            crate::schema::Node {
                id: "beta".into(),
                name: "Beta".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 0,
                ..Default::default()
            },
            crate::schema::Node {
                id: "gamma".into(),
                name: "Gamma".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 2_000,
                ..Default::default()
            },
            crate::schema::Node {
                id: "mapped".into(),
                name: "Mapped".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 0,
                ..Default::default()
            },
        ];
        let mut maps = HashMap::new();
        maps.insert("mapped".to_string(), demo_map());
        let picked = select_stale_sites(&graph, &maps, Some("alpha"), 3);
        assert_eq!(picked, vec!["beta".to_string(), "gamma".to_string()]);
        // beta (never evolved) leads; alpha excluded as current; mapped excluded.
    }

    #[test]
    fn eviction_drops_lru_not_current() {
        let mut maps: HashMap<String, SiteMap> = HashMap::new();
        for i in 0..MAX_SITE_MAPS {
            let mut m = demo_map();
            m.last_visit_minutes = 100 + i as i64;
            maps.insert(format!("site-{i}"), m);
        }
        // Make site-3 the freshest and site-7 the stalest; current is site-0.
        maps.get_mut("site-3").unwrap().last_visit_minutes = 9_999;
        maps.get_mut("site-7").unwrap().last_visit_minutes = 1;
        // Below the cap → no eviction.
        assert_eq!(evict_lru_site_map(&mut maps, "site-0"), None);
        // Add one more → over the cap → evict the stalest non-current (site-7).
        maps.insert("site-new".to_string(), demo_map());
        assert_eq!(evict_lru_site_map(&mut maps, "site-0"), Some("site-7".to_string()));
        assert!(!maps.contains_key("site-7"));
        // Current-site protection: with only the current map stale it still wins.
        let mut one = HashMap::new();
        let mut m = demo_map();
        m.last_visit_minutes = 0;
        one.insert("here".to_string(), m);
        assert_eq!(evict_lru_site_map(&mut one, "here"), None);
    }

    // ---------- (2026-08-23) Hosted interiors ----------

    /// A parent district fixture with a Building asset + an enterable
    /// child, for the resolver / freeze / stamp tests.
    fn hosted_fixture() -> (HashMap<String, SiteMap>, String, String) {
        let mut parent = demo_map();
        parent.assets.push(SiteAsset {
            id: "the-sunken-flagon".to_string(),
            name: "The Sunken Flagon".to_string(),
            kind: AssetKind::Building,
            location: "gatehouse".to_string(),
            count: 0,
            ..demo_map().assets[0].clone()
        });
        let child = demo_map();
        let mut maps: HashMap<String, SiteMap> = HashMap::new();
        maps.insert("oakhaven".to_string(), parent);
        let child_key = hosted_key("oakhaven", "the-sunken-flagon");
        maps.insert(child_key.clone(), child);
        (maps, "oakhaven".to_string(), child_key)
    }

    #[test]
    fn hosted_keys_parse_and_roundtrip() {
        let k = hosted_key("oakhaven", "the-sunken-flagon");
        assert_eq!(k, "oakhaven::the-sunken-flagon");
        assert_eq!(
            parse_hosted_key(&k),
            Some(("oakhaven", "the-sunken-flagon")),
            "kebab ids never contain ::, so the split is unambiguous"
        );
        assert_eq!(parse_hosted_key("oakhaven"), None, "plain node key");
        assert_eq!(parse_hosted_key("::flagon"), None, "empty halves reject");
        assert_eq!(parse_hosted_key("a::b::c"), None, "double separator rejects");
    }

    #[test]
    fn host_fields_serde_dormant_on_legacy_saves() {
        // A pre-feature map JSON (no host/current_building keys) loads with
        // both dormant; set values round-trip (rides world.json for free).
        let legacy = serde_json::to_value(demo_map()).unwrap();
        let loaded: SiteMap = serde_json::from_value(legacy).unwrap();
        assert!(loaded.host.is_none());
        assert!(loaded.current_building.is_none());
        let mut armed = demo_map();
        armed.host = Some(HostRef {
            parent_key: "oakhaven".into(),
            building_asset_id: "the-sunken-flagon".into(),
            exit_area_id: "gatehouse".into(),
        });
        armed.current_building = Some("the-sunken-flagon".into());
        let back: SiteMap =
            serde_json::from_value(serde_json::to_value(&armed).unwrap()).unwrap();
        assert_eq!(back.host.as_ref().unwrap().parent_key, "oakhaven");
        assert_eq!(back.current_building.as_deref(), Some("the-sunken-flagon"));
        // Dormant maps serialize WITHOUT the keys (byte-identical saves).
        let quiet = serde_json::to_value(demo_map()).unwrap();
        assert!(quiet.get("host").is_none());
        assert!(quiet.get("current_building").is_none());
    }

    #[test]
    fn resolver_picks_child_only_while_inside() {
        let (mut maps, node, child_key) = hosted_fixture();
        // Outside: the node map is active.
        assert_eq!(active_site_map_key(&maps, Some(node.as_str())), Some(node.clone()));
        // current_building set + child exists → the child.
        maps.get_mut(&node).unwrap().current_building =
            Some("the-sunken-flagon".to_string());
        assert_eq!(active_site_map_key(&maps, Some(node.as_str())), Some(child_key.clone()));
        // Stale pointer (child evicted): falls back to the parent, never a
        // dead key.
        maps.remove(&child_key);
        assert_eq!(
            active_site_map_key(&maps, Some(node.as_str())),
            Some(node.clone()),
            "a dangling current_building falls back to the parent map"
        );
    }

    #[test]
    fn frozen_keys_cover_the_whole_chain() {
        let (mut maps, node, child_key) = hosted_fixture();
        // Outside: only the node map is frozen.
        assert_eq!(player_frozen_keys(&maps, Some(node.as_str())), vec![node.clone()]);
        // Inside: node + child both frozen (the parent sweep on eviction
        // covers the child's parent = the node itself).
        maps.get_mut(&node).unwrap().current_building =
            Some("the-sunken-flagon".to_string());
        let frozen = player_frozen_keys(&maps, Some(node.as_str()));
        assert!(frozen.contains(&node), "the settlement itself is frozen");
        assert!(frozen.contains(&child_key), "the hosted child is frozen");
        assert_eq!(frozen.len(), 2);
    }

    #[test]
    fn enter_exit_stamps_roundtrip_the_chain() {
        let (mut maps, node, child_key) = hosted_fixture();
        let now = 20_000i64;
        // ENTER: parent anchor set + building revealed; child arrival.
        {
            let child = maps.get_mut(&child_key).unwrap();
            enter_building_child_stamp(child, now);
        }
        enter_building_parent_stamp(
            maps.get_mut(&node).unwrap(),
            "the-sunken-flagon",
            now,
        );
        let parent = maps.get(&node).unwrap();
        assert_eq!(parent.current_building.as_deref(), Some("the-sunken-flagon"));
        assert_eq!(parent.last_visit_minutes, now);
        assert_eq!(
            parent
                .assets
                .iter()
                .find(|a| a.id == "the-sunken-flagon")
                .unwrap()
                .knowledge,
            AssetKnowledge::Known
        );
        let child = maps.get(&child_key).unwrap();
        assert_eq!(child.current_area.as_deref(), Some(child.entrance.as_str()));
        assert_eq!(active_site_map_key(&maps, Some(node.as_str())), Some(child_key.clone()));
        // EXIT: back to a district area — chain clear, area Visited, child
        // position + digest cleared.
        exit_building_child_stamp(maps.get_mut(&child_key).unwrap(), now + 5);
        exit_building_parent_stamp(maps.get_mut(&node).unwrap(), "gatehouse", now + 5);
        let parent = maps.get(&node).unwrap();
        assert!(parent.current_building.is_none());
        assert_eq!(parent.current_area.as_deref(), Some("gatehouse"));
        assert_eq!(
            parent.areas.iter().find(|a| a.id == "gatehouse").unwrap().knowledge,
            AreaKnowledge::Visited
        );
        assert!(maps.get(&child_key).unwrap().current_area.is_none());
        assert_eq!(active_site_map_key(&maps, Some(node.as_str())), Some(node.clone()));
    }

    #[test]
    fn count_hosted_interiors_scopes_to_one_settlement() {
        let (mut maps, node, _) = hosted_fixture();
        assert_eq!(count_hosted_interiors(&maps, &node), 1);
        let mut other = demo_map();
        other.host = Some(HostRef {
            parent_key: "elsewhere".into(),
            building_asset_id: "x".into(),
            exit_area_id: "y".into(),
        });
        maps.insert("elsewhere::x".to_string(), other);
        assert_eq!(
            count_hosted_interiors(&maps, &node),
            1,
            "another settlement's children never count"
        );
        assert_eq!(count_hosted_interiors(&maps, "elsewhere"), 1);
    }

    #[test]
    fn eviction_freezes_the_chain_and_sweeps_children() {
        // At cap: while the player stands in a building, NEITHER the child
        // NOR its parent is evictable — the stalest OTHER map goes instead.
        let (mut maps, node, child_key) = hosted_fixture();
        maps.get_mut(&node).unwrap().current_building =
            Some("the-sunken-flagon".to_string());
        for i in 0..(MAX_SITE_MAPS - maps.len()) {
            let mut m = demo_map();
            m.last_visit_minutes = 50 + i as i64;
            maps.insert(format!("site-{i}"), m);
        }
        // Make the CHAIN the stalest (it would lose any LRU race).
        maps.get_mut(&node).unwrap().last_visit_minutes = 1;
        maps.get_mut(&child_key).unwrap().last_visit_minutes = 1;
        let victim = evict_lru_site_map(&mut maps, &node);
        assert_ne!(victim.as_deref(), Some(node.as_str()), "parent frozen");
        assert_ne!(victim.as_deref(), Some(child_key.as_str()), "child frozen");
        // Evicting a PARENT sweeps its children (no orphans squatting the cap).
        let mut orphan_check: HashMap<String, SiteMap> = HashMap::new();
        let mut parent = demo_map();
        parent.last_visit_minutes = 1;
        orphan_check.insert("old-town".to_string(), parent);
        let mut child = demo_map();
        child.last_visit_minutes = 9_999;
        orphan_check.insert(hosted_key("old-town", "tavern"), child);
        // Fill to cap so eviction actually fires; old-town is stalest.
        for i in 0..(MAX_SITE_MAPS - 2) {
            let mut m = demo_map();
            m.last_visit_minutes = 100 + i as i64;
            orphan_check.insert(format!("f-{i}"), m);
        }
        assert_eq!(
            evict_lru_site_map(&mut orphan_check, "elsewhere"),
            Some("old-town".to_string())
        );
        assert!(
            !orphan_check.contains_key(&hosted_key("old-town", "tavern")),
            "the swept parent's children die with it"
        );
    }

    #[test]
    fn settlement_name_heuristic_matches_obvious_towns_only() {
        assert!(looks_like_settlement("Oakhaven Town"));
        assert!(looks_like_settlement("Port Vylle"));
        assert!(looks_like_settlement("the city-state of Karr"));
        assert!(!looks_like_settlement("Oakhaven"), "no marker word");
        assert!(!looks_like_settlement("Downtown Alleyway"), "substring ≠ word");
    }

    #[test]
    fn kebabify_and_is_kebab_id() {
        assert!(is_kebab_id("gatehouse"));
        assert!(is_kebab_id("rot-warren-2"));
        assert!(!is_kebab_id("Gatehouse"));
        assert!(!is_kebab_id("-lead"));
        assert!(!is_kebab_id("trail-"));
        assert!(!is_kebab_id("double--dash"));
        assert!(!is_kebab_id("under_score"));
        assert_eq!(kebabify("The Rot Warren!"), "the-rot-warren");
        assert_eq!(kebabify("  spaced   out  "), "spaced-out");
        // Truncation edge: a long name whose 64-char cut lands ON a joining
        // dash — the id must still satisfy `is_kebab_id` (the trailing-dash
        // strip runs AFTER the cut; running it before re-exposed the dash).
        let long = "aaaaaaa bbbbbbb ccccccc ddddddd eeeeeee fffffff ggggggg hhhhhhh iiiiiii";
        let id = kebabify(long);
        assert_eq!(id.chars().count(), 63);
        assert!(is_kebab_id(&id), "truncated id must not end on a dash: {id}");
    }

    // ---- (2026-08-22 living-world) site evolution -------------------------

    fn op(op_word: &str, asset: &str) -> SiteEvolutionOp {
        SiteEvolutionOp {
            op: op_word.to_string(),
            asset: asset.to_string(),
            id: None,
            name: None,
            kind: None,
            area: None,
            state: None,
            count: None,
            detail: None,
            to: None,
            cause: String::new(),
            actor: String::new(),
        }
    }

    #[test]
    fn canon_transition_matrix() {
        // Same-state refreshes always legal.
        for s in [
            AssetState::Active,
            AssetState::Dead,
            AssetState::Taken,
            AssetState::Triggered,
            AssetState::Deactivated,
            AssetState::Fleeing,
        ] {
            assert!(canon_transition(s, s), "{s:?} → itself must be legal");
        }
        // Dead → Taken is the ONE sanctioned exit (looting a corpse).
        assert!(canon_transition(AssetState::Dead, AssetState::Taken));
        // Every other exit from a terminal state refuses.
        assert!(!canon_transition(AssetState::Dead, AssetState::Active));
        assert!(!canon_transition(AssetState::Dead, AssetState::Fleeing));
        assert!(!canon_transition(AssetState::Taken, AssetState::Active));
        assert!(!canon_transition(AssetState::Deactivated, AssetState::Active));
        // Non-terminal states move freely.
        assert!(canon_transition(AssetState::Active, AssetState::Dead));
        assert!(canon_transition(AssetState::Fleeing, AssetState::Active));
        assert!(is_terminal(AssetState::Dead));
        assert!(is_terminal(AssetState::Taken));
        assert!(is_terminal(AssetState::Deactivated));
        assert!(!is_terminal(AssetState::Active));
        assert!(!is_terminal(AssetState::Triggered));
        assert!(!is_terminal(AssetState::Fleeing));
    }

    #[test]
    fn evolution_ops_enforce_play_canon_locks() {
        let mut m = demo_map();
        m.assets[1].knowledge = AssetKnowledge::Known;
        m.assets[1].state = AssetState::Dead;
        // Resurrection attempt → the exact remnant-entity wording.
        let mut bad = op("set_asset", "gate-keeper");
        bad.state = Some("active".into());
        let (applied, rejects) = apply_evolution_ops(&mut m, &[bad], 2_000);
        assert_eq!(applied, 0);
        assert!(
            rejects.iter().any(|r| r.contains("play-canon locked")
                && r.contains("add a new remnant entity instead of resurrecting")),
            "reject must carry the remnant wording: {rejects:?}"
        );
        assert_eq!(m.assets[1].state, AssetState::Dead, "dead stays dead");
        // Dead → Taken (looting) + same-state refresh are legal.
        let mut loot = op("set_asset", "gate-keeper");
        loot.state = Some("taken".into());
        loot.cause = "scavengers stripped it".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[loot], 2_000);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert_eq!(m.assets[1].state, AssetState::Taken);
        // Unknown asset + unknown op reject with usable messages.
        let (applied, rejects) = apply_evolution_ops(&mut m, &[op("set_asset", "zzz-none")], 2_000);
        assert_eq!(applied, 0);
        assert!(rejects.iter().any(|r| r.contains("not part of this site")));
        let (applied, rejects) = apply_evolution_ops(&mut m, &[op("explode", "gate-keeper")], 2_000);
        assert_eq!(applied, 0);
        assert!(rejects.iter().any(|r| r.contains("unknown")));
    }

    #[test]
    fn evolution_ops_stamp_provenance_and_digest() {
        let mut m = demo_map();
        let mut kill = op("set_asset", "warband-scouts");
        kill.state = Some("fleeing".into());
        kill.count = Some(3);
        kill.cause = "the watch burned the warren".into();
        kill.actor = "the town watch".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[kill], 5_000);
        assert_eq!((applied, rejects.len()), (1, 0));
        let a = &m.assets[0];
        assert_eq!(a.state, AssetState::Fleeing);
        assert_eq!(a.count, 3, "group count updates through set_asset");
        assert_eq!(a.changed_at_minutes, 5_000);
        assert_eq!(a.origin, AssetOrigin::Evolved);
        assert_eq!(a.actor, "the town watch");
        assert!(
            m.pending_digest
                .iter()
                .any(|l| l.contains("Warband Scouts") && l.contains("fleeing")),
            "digest line missing: {:?}",
            m.pending_digest
        );
        // Move to an unknown area rejects; to a real area works + digests.
        let mut bad_move = op("move_asset", "gate-keeper");
        bad_move.to = Some("nowhere".into());
        let (applied, rejects) = apply_evolution_ops(&mut m, &[bad_move], 5_000);
        assert_eq!(applied, 0);
        assert!(rejects.iter().any(|r| r.contains("not part of this site")));
        let mut move_ok = op("move_asset", "gate-keeper");
        move_ok.to = Some("hall".into());
        move_ok.cause = "chased the noise".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[move_ok], 5_100);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert_eq!(m.assets.iter().find(|a| a.id == "gate-keeper").unwrap().location, "hall");
        // Remove drops the row + digests its departure.
        let (applied, rejects) =
            apply_evolution_ops(&mut m, &[op("remove_asset", "gate-keeper")], 5_200);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert!(!m.assets.iter().any(|a| a.id == "gate-keeper"));
        assert!(m.pending_digest.iter().any(|l| l.contains("gone")));
        // The digest renders FIRST in the narrator slice.
        let slice = render_narrator_slice(&m, 5_200).expect("non-empty");
        assert!(
            slice.starts_with("changed since your last visit:"),
            "digest must lead the slice: {slice}"
        );
    }

    #[test]
    fn digest_caps_at_six_lines_fifo() {
        let mut m = demo_map();
        for i in 0..8 {
            // Exercise the bounded-store contract through the raw push (the
            // remove-op path is covered by the provenance test above).
            push_digest_line(&mut m, &format!("Thing {i}"), "gone", "cause");
        }
        assert_eq!(m.pending_digest.len(), DIGEST_LINE_MAX);
        assert!(m.pending_digest[0].contains("Thing 2"), "oldest lines fall");
        assert!(m.pending_digest.last().unwrap().contains("Thing 7"));
    }

    // ---- (2026-08-24 Part II B1) restock: add_asset -----------------------

    #[test]
    fn evolution_ops_add_asset_restocks_within_laws() {
        let mut m = demo_map();
        let mut add = op("add_asset", "");
        add.name = Some("Scavenger Crew".into());
        add.kind = Some("group".into());
        add.area = Some("gatehouse".into());
        add.count = Some(4);
        add.cause = "scavengers moved into the emptied warren".into();
        add.actor = "ragpickers off the road".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[add], 7_000);
        assert_eq!((applied, rejects.len()), (1, 0), "{rejects:?}");
        let a = m
            .assets
            .iter()
            .find(|a| a.id == "scavenger-crew")
            .expect("id derives from the name");
        assert_eq!(a.kind, AssetKind::Group);
        assert_eq!(a.count, 4, "group count clamps through add_asset");
        assert_eq!(a.location, "gatehouse");
        assert_eq!(a.state, AssetState::Active);
        assert_eq!(a.origin, AssetOrigin::Evolved);
        // Knowledge-safe channel law (the seed_rumor_asset precedent):
        // Unrevealed + NO digest line — the map quietly agrees until the
        // player walks in on the arrival.
        assert_eq!(a.knowledge, AssetKnowledge::Unrevealed);
        assert!(m.pending_digest.is_empty(), "no digest for hidden truth");
        // An explicit id is honored (kebabified).
        let mut explicit = op("add_asset", "");
        explicit.id = Some("carrion-birds".into());
        explicit.name = Some("Carrion Birds".into());
        explicit.kind = Some("creature".into());
        explicit.area = Some("hall".into());
        explicit.cause = "followed the smell".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[explicit], 7_100);
        assert_eq!((applied, rejects.len()), (1, 0), "{rejects:?}");
        assert!(m.assets.iter().any(|a| a.id == "carrion-birds"));
        // The reject ladder: unknown area / missing cause / id collision /
        // building (the depth-2 law — kind must be one of the six words).
        let mut bad_area = op("add_asset", "");
        bad_area.name = Some("Cultists".into());
        bad_area.kind = Some("group".into());
        bad_area.area = Some("basement".into());
        bad_area.cause = "moved in".into();
        let (_, r) = apply_evolution_ops(&mut m, &[bad_area], 7_200);
        assert!(r.iter().any(|x| x.contains("not part of this site")), "{r:?}");

        let mut no_cause = op("add_asset", "");
        no_cause.name = Some("Cultists".into());
        no_cause.kind = Some("group".into());
        no_cause.area = Some("hall".into());
        let (_, r) = apply_evolution_ops(&mut m, &[no_cause], 7_200);
        assert!(r.iter().any(|x| x.contains("cause")), "{r:?}");

        let mut collide = op("add_asset", "");
        collide.id = Some("gate-keeper".into());
        collide.name = Some("Gate Keeper Twin".into());
        collide.kind = Some("creature".into());
        collide.area = Some("gatehouse".into());
        collide.cause = "a twin".into();
        let (_, r) = apply_evolution_ops(&mut m, &[collide], 7_200);
        assert!(r.iter().any(|x| x.contains("already exists")), "{r:?}");

        let mut building = op("add_asset", "");
        building.name = Some("Wayside Shrine".into());
        building.kind = Some("building".into());
        building.area = Some("gatehouse".into());
        building.cause = "built overnight".into();
        let (_, r) = apply_evolution_ops(&mut m, &[building], 7_200);
        assert!(r.iter().any(|x| x.contains("kind must be one of")), "{r:?}");
        // The cap: a full map refuses further arrivals.
        while m.assets.len() < MAX_SITE_ASSETS {
            m.assets.push(SiteAsset {
                id: format!("filler-{}", m.assets.len()),
                name: "Filler".into(),
                kind: AssetKind::Object,
                location: "hall".into(),
                ..Default::default()
            });
        }
        let mut over = op("add_asset", "");
        over.name = Some("One Too Many".into());
        over.kind = Some("object".into());
        over.area = Some("hall".into());
        over.cause = "greed".into();
        let (applied, r) = apply_evolution_ops(&mut m, &[over], 7_300);
        assert_eq!(applied, 0);
        assert!(r.iter().any(|x| x.contains("cap reached")), "{r:?}");
    }

    // ---- (2026-08-23 WS5) causal thread ledger ---------------------------

    #[test]
    fn threads_open_dedupe_and_cap() {
        let mut m = demo_map();
        open_thread(
            &mut m, "warband-scouts", "Warband Scouts", "hall", "the watch", "burned out", false, 1_000,
        );
        assert_eq!(m.threads.len(), 1);
        // Restate: same subject refreshes fields, KEEPS the original open
        // time (restate-never-duplicate, the echo-guard lesson).
        open_thread(
            &mut m, "warband-scouts", "Warband Scouts", "hall", "the watch",
            "burned out again", false, 9_000,
        );
        assert_eq!(m.threads.len(), 1, "restate never duplicates");
        assert_eq!(m.threads[0].opened_at_minutes, 1_000);
        assert_eq!(m.threads[0].cause, "burned out again");
        // Distinct subjects accumulate; FIFO cap at THREADS_MAX_PER_SITE.
        for i in 0..6 {
            open_thread(
                &mut m, &format!("t-{i}"), &format!("T {i}"), "vault", "", "", false, 2_000 + i,
            );
        }
        assert_eq!(m.threads.len(), THREADS_MAX_PER_SITE);
        assert!(
            !m.threads.iter().any(|t| t.subject == "warband-scouts"),
            "oldest falls off"
        );
        assert!(m.threads.iter().any(|t| t.subject == "t-5"), "newest stays");
        // Empty subject is a defensive no-op.
        open_thread(&mut m, "", "Nobody", "hall", "", "", false, 3_000);
        assert_eq!(m.threads.len(), THREADS_MAX_PER_SITE);
    }

    #[test]
    fn threads_close_by_subject_and_area() {
        let mut m = demo_map();
        open_thread(
            &mut m, "warband-scouts", "Warband Scouts", "hall", "the watch", "scattered", false, 1_000,
        );
        open_thread(
            &mut m, "gate-keeper", "Gate Keeper", "gatehouse", "a rival", "killed", false, 1_100,
        );
        // Area activity resolves every question rooted there.
        let n = close_threads_by_area(&mut m, "hall", "the fire burned out");
        assert_eq!(n, 1);
        assert!(m.threads.iter().none(|t| t.subject == "warband-scouts"));
        assert!(
            m.pending_digest
                .iter()
                .any(|l| l.contains("Warband Scouts") && l.contains("resolved"))
        );
        // A subject hit resolves just that thread; idempotent after.
        let n = close_threads_by_subject(&mut m, "gate-keeper", "looted and buried");
        assert_eq!(n, 1);
        assert!(m.threads.is_empty());
        assert!(
            m.pending_digest
                .iter()
                .any(|l| l.contains("Gate Keeper") && l.contains("resolved"))
        );
        assert_eq!(close_threads_by_subject(&mut m, "gate-keeper", ""), 0);
    }

    #[test]
    fn evolution_ops_open_and_resolve_threads() {
        // A terminal kill OPENS the thread with the op's provenance.
        let mut m = demo_map();
        let mut kill = op("set_asset", "gate-keeper");
        kill.state = Some("dead".into());
        kill.cause = "ambushed on the road".into();
        kill.actor = "the town watch".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[kill], 5_000);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert_eq!(m.threads.len(), 1);
        assert_eq!(m.threads[0].subject, "gate-keeper");
        assert_eq!(m.threads[0].area, "gatehouse");
        assert_eq!(m.threads[0].actor, "the town watch");
        assert_eq!(m.threads[0].opened_at_minutes, 5_000);
        // A later op arriving in the thread's AREA resolves it (pure key
        // match — warband-scouts moves hall → gatehouse along the open way).
        let mut move_in = op("move_asset", "warband-scouts");
        move_in.to = Some("gatehouse".into());
        move_in.cause = "claimed the empty gatehouse".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[move_in], 6_000);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert!(m.threads.is_empty(), "arrival in the thread's area resolves it");
        assert!(
            m.pending_digest
                .iter()
                .any(|l| l.contains("Gate Keeper") && l.contains("resolved"))
        );

        // Non-terminal state writes never open (fleeing is movement, not a
        // causal question).
        let mut m = demo_map();
        let mut spook = op("set_asset", "warband-scouts");
        spook.state = Some("fleeing".into());
        let (applied, _) = apply_evolution_ops(&mut m, &[spook], 5_000);
        assert_eq!(applied, 1);
        assert!(m.threads.is_empty(), "fleeing opens nothing");

        // The Dead→Taken loot claim CLOSES the death question (a non-Dead
        // write resolves the subject's own thread; Taken never opens).
        let mut kill = op("set_asset", "gate-keeper");
        kill.state = Some("dead".into());
        kill.cause = "ambushed".into();
        apply_evolution_ops(&mut m, &[kill], 5_000);
        assert_eq!(m.threads.len(), 1);
        let mut loot = op("set_asset", "gate-keeper");
        loot.state = Some("taken".into());
        loot.cause = "scavengers stripped it".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[loot], 5_500);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert!(m.threads.is_empty(), "the loot claim closes the thread");

        // Removing a LIVE asset OPENS (a thing that was alive is gone);
        // removing a terminal remnant is cleanup — closes only, no open.
        let mut m = demo_map();
        let (applied, _) = apply_evolution_ops(&mut m, &[op("remove_asset", "warband-scouts")], 5_000);
        assert_eq!(applied, 1);
        assert_eq!(m.threads.len(), 1, "live removal opens");
        assert_eq!(m.threads[0].subject, "warband-scouts");
        let mut m2 = demo_map();
        let mut kill = op("set_asset", "gate-keeper");
        kill.state = Some("dead".into());
        apply_evolution_ops(&mut m2, &[kill], 5_000);
        let (applied, _) = apply_evolution_ops(&mut m2, &[op("remove_asset", "gate-keeper")], 5_100);
        assert_eq!(applied, 1);
        assert!(m2.threads.is_empty(), "remnant removal opens nothing");
    }

    #[test]
    fn threads_flush_on_arrival_and_age_collapse() {
        let mut m = demo_map();
        open_thread(
            &mut m, "gate-keeper", "Gate Keeper", "gatehouse", "a rival", "killed", false, 1_000,
        );
        open_thread(
            &mut m, "warband-scouts", "Warband Scouts", "hall", "", "scattered", false, 1_100,
        );
        flush_threads_on_arrival(&mut m);
        assert!(m.threads.is_empty());
        assert_eq!(
            m.pending_digest
                .iter()
                .filter(|l| l.contains("open question"))
                .count(),
            2
        );

        // Age sweep: just under the watermark stays open; at/past it the
        // question fades (closes + digests).
        let mut m2 = demo_map();
        open_thread(&mut m2, "gate-keeper", "Gate Keeper", "gatehouse", "", "killed", false, 100);
        let mut maps = HashMap::from([("keep".to_string(), m2)]);
        assert_eq!(
            sweep_stale_threads(&mut maps, 100 + THREAD_COLLAPSE_MINUTES - 1),
            0
        );
        assert_eq!(sweep_stale_threads(&mut maps, 100 + THREAD_COLLAPSE_MINUTES), 1);
        assert!(maps["keep"].threads.is_empty());
        assert!(
            maps["keep"]
                .pending_digest
                .iter()
                .any(|l| l.contains("question faded"))
        );
    }

    #[test]
    fn thread_render_is_bounded_and_clean() {
        let mut m = demo_map();
        assert!(
            render_thread_lines(&m, 5_000).is_empty(),
            "empty ledger renders nothing (zero prompt cost)"
        );
        for i in 0..5 {
            open_thread(
                &mut m,
                &format!("s-{i}"),
                &format!("Subject {i} with a fairly long name"),
                "hall",
                &format!("actor {i}"),
                &format!("cause {i} that goes on and on"),
                false,
                1_000 + 1440 * i as i64,
            );
        }
        let lines = render_thread_lines(&m, 1_000 + 1440 * 5);
        assert_eq!(lines.len(), THREAD_RENDER_MAX, "only the newest render");
        assert!(lines[0].contains("Subject 4"), "newest leads: {lines:?}");
        assert!(lines[0].contains("[day 1]"), "day marker renders");
        for l in &lines {
            assert!(
                l.chars().count() <= THREAD_LINE_CHARS,
                "line over cap: {l}"
            );
        }
    }

    #[test]
    fn remnants_collapse_after_a_day() {
        let mut m = demo_map();
        m.assets[1].knowledge = AssetKnowledge::Known;
        m.assets[1].state = AssetState::Dead;
        // Fresh kill (changed now) → full render, no remnants.
        let slice = render_narrator_slice(&m, 1_000).expect("non-empty");
        assert!(slice.contains("Gate Keeper"), "fresh corpse renders in place");
        assert!(!slice.contains("remnants:"));
        // Stale (changed 2 days ago) → collapses out of the area list into
        // the remnants line; the tracker slice still carries its id:state
        // pair so the Dead → Taken loot transition stays targetable.
        m.assets[1].changed_at_minutes = 1_000 - 2 * DEAD_ASSET_COLLAPSE_MINUTES;
        m.assets[1].origin = AssetOrigin::Evolved;
        let slice = render_narrator_slice(&m, 1_000).expect("non-empty");
        assert!(!slice.contains("  Gate Keeper"), "stale corpse leaves the area list");
        assert!(
            slice.contains("remnants: dead — Gate Keeper"),
            "remnants line missing: {slice}"
        );
        let tracker = render_tracker_slice(&m, 1_000).expect("non-empty");
        let live = tracker.split(" remnants=").next().unwrap_or("");
        assert!(!live.contains("gate-keeper:"), "stale id leaves the live tracker list: {tracker}");
        assert!(
            tracker.contains("remnants=gate-keeper:dead"),
            "tracker keeps the remnant targetable: {tracker}"
        );
    }

    #[test]
    fn fleeing_and_deactivated_skip_mob_tier() {
        let mut m = demo_map();
        m.assets[0].knowledge = AssetKnowledge::Known;
        assert_eq!(present_mob_tier(&m), Some(AttackerTier::Soldier));
        m.assets[0].state = AssetState::Fleeing;
        m.assets[1].knowledge = AssetKnowledge::Known;
        assert_eq!(present_mob_tier(&m), Some(AttackerTier::Minion));
        // A disarmed trap is not a mob host either (kind swap to Trap).
        m.assets[1].kind = AssetKind::Trap;
        m.assets[1].state = AssetState::Deactivated;
        assert_eq!(present_mob_tier(&m), None);
    }

    #[test]
    fn state_words_round_trip() {
        for (word, state) in [
            ("active", AssetState::Active),
            ("dead", AssetState::Dead),
            ("slain", AssetState::Dead),
            ("taken", AssetState::Taken),
            ("looted", AssetState::Taken),
            ("triggered", AssetState::Triggered),
            ("deactivated", AssetState::Deactivated),
            ("disarmed", AssetState::Deactivated),
            ("fleeing", AssetState::Fleeing),
            ("fled", AssetState::Fleeing),
        ] {
            assert_eq!(parse_asset_state_word(word), Some(state), "{word}");
        }
        assert_eq!(parse_asset_state_word("zapped"), None);
        assert_eq!(AssetState::Deactivated.word(), "deactivated");
        assert_eq!(AssetState::Fleeing.word(), "fleeing");
        assert_eq!(AssetOrigin::Evolved.word(), "evolved");
    }

    // ---- (2026-08-22 multihog WS1) deterministic asset expiry ------------

    /// A lapsed NON-terminal asset deactivates (terminal stamp + expired
    /// cause + a digest line); a terminal asset skips (play-canon outranks
    /// timers); a future deadline stands; the armed slot always drops once
    /// observed.
    #[test]
    fn sweep_asset_expiry_deactivates_nonterminal_only() {
        let mut maps = HashMap::new();
        let mut m = demo_map();
        m.assets[0].expires_at_minutes = Some(1_500); // warband-scouts, active
        m.assets[1].state = AssetState::Dead; // gate-keeper already terminal
        m.assets[1].expires_at_minutes = Some(1_500);
        maps.insert("warren".to_string(), m);

        let (directives, mutated) = sweep_asset_expiry(&mut maps, 2_000);
        assert_eq!(mutated, 2, "both lapsed slots observed");
        let m = maps.get("warren").unwrap();
        let warband = m.assets.iter().find(|a| a.id == "warband-scouts").unwrap();
        assert_eq!(warband.state, AssetState::Deactivated, "active lapses");
        assert_eq!(warband.changed_at_minutes, 2_000);
        assert_eq!(warband.cause, "expired");
        assert_eq!(warband.origin, AssetOrigin::Evolved);
        assert!(warband.expires_at_minutes.is_none(), "slot drops");
        let keeper = m.assets.iter().find(|a| a.id == "gate-keeper").unwrap();
        assert_eq!(keeper.state, AssetState::Dead, "dead stays dead on a timer");
        assert_eq!(keeper.changed_at_minutes, 0, "terminal asset untouched");
        assert!(
            m.pending_digest.iter().any(|l| l.contains("Warband Scouts") && l.contains("deactivated")),
            "digest line missing: {:?}",
            m.pending_digest
        );
        assert_eq!(directives.len(), 1, "only the real lapse directs");
        assert!(directives[0].contains("Warband Scouts"), "{}", directives[0]);

        // Future deadline stands; a second sweep is a no-op.
        maps.get_mut("warren").unwrap().assets[0].expires_at_minutes = Some(9_000);
        let (dirs2, n2) = sweep_asset_expiry(&mut maps, 2_000);
        assert_eq!((dirs2.len(), n2), (0, 0));
        assert!(maps["warren"].assets[0].expires_at_minutes.is_some());
        // Dormant clock never sweeps.
        assert_eq!(sweep_asset_expiry(&mut maps, 0).1, 0);
    }

    // ---- (2026-08-22 multihog WS2) traversal gating + GM block ------------

    /// The GM hidden-contents block: strictly 1-hop from the current area,
    /// hidden-truth (Unrevealed/Suspected) occupants only, ranked + capped
    /// at 3, framed with the positive-form law, whole-block bounded.
    #[test]
    fn gm_hidden_slice_is_one_hop_ranked_and_bounded() {
        let mut m = demo_map();
        m.current_area = Some("gatehouse".into());
        // A 2-hop hidden asset (vault, behind the locked hall↔vault door)
        // must never render; the 1-hop warband (in the hall) must.
        m.assets.push(SiteAsset {
            id: "crown-of-iron".into(),
            name: "Crown of Iron".into(),
            kind: AssetKind::Loot,
            location: "vault".into(),
            state: AssetState::Active,
            knowledge: AssetKnowledge::Unrevealed,
            count: 0,
            detail: String::new(),
            tier: None,
            origin: AssetOrigin::InitialMap,
            changed_at_minutes: 0,
            cause: String::new(),
            actor: String::new(),
            expires_at_minutes: None,
        });
        // A KNOWN asset in the current area is player-visible truth, never
        // GM truth — excluded.
        m.assets.push(SiteAsset {
            id: "hearth-cat".into(),
            name: "Hearth Cat".into(),
            kind: AssetKind::Creature,
            location: "gatehouse".into(),
            state: AssetState::Active,
            knowledge: AssetKnowledge::Known,
            count: 0,
            detail: String::new(),
            tier: None,
            origin: AssetOrigin::InitialMap,
            changed_at_minutes: 0,
            cause: String::new(),
            actor: String::new(),
            expires_at_minutes: None,
        });
        let block = render_gm_hidden_slice(&m, 1_000).expect("block renders");
        assert!(block.starts_with("<hidden_truth>"), "{block}");
        assert!(block.contains("discovery, checks, and consequence"), "the law rides: {block}");
        assert!(block.contains("Warband Scouts"), "1-hop hidden truth renders");
        assert!(
            !block.contains("Crown of Iron"),
            "2-hop truth never renders: {block}"
        );
        assert!(!block.contains("Hearth Cat"), "known truth is not GM truth");
        assert!(
            !block.contains("Gate Keeper"),
            "the KNOWN current-area asset stays out"
        );
        assert!(
            block.contains("beyond the way to Great Hall"),
            "the hop is named: {block}"
        );
        assert!(block.chars().count() <= GM_HIDDEN_BLOCK_CHARS, "{} chars", block.chars().count());

        // Bound: 5 hidden 1-hop candidates → only the top 3 render
        // (creatures/groups first, then tier, then count).
        let mut crowded = demo_map();
        crowded.current_area = Some("hall".into());
        for i in 0..4 {
            crowded.assets.push(SiteAsset {
                id: format!("loot-{i}"),
                name: format!("Shiny Loot {i}"),
                kind: AssetKind::Loot,
                location: "hall".into(),
                state: AssetState::Active,
                knowledge: AssetKnowledge::Unrevealed,
                count: 0,
                detail: String::new(),
                tier: None,
                origin: AssetOrigin::InitialMap,
                changed_at_minutes: 0,
                cause: String::new(),
                actor: String::new(),
                expires_at_minutes: None,
            });
        }
        let block = render_gm_hidden_slice(&crowded, 1_000).expect("block renders");
        assert_eq!(
            block.lines().filter(|l| l.starts_with("- ")).count(),
            GM_HIDDEN_MAX_ENTITIES,
            "capped at {GM_HIDDEN_MAX_ENTITIES}: {block}"
        );
        assert!(
            block.contains("Warband Scouts"),
            "the group outranks loot for the slots"
        );
        // No current_area (legacy save) → the entrance anchors the hop.
        let legacy = demo_map();
        let block = render_gm_hidden_slice(&legacy, 1_000).expect("falls back to entrance");
        assert!(block.contains("Warband Scouts"));
    }

    /// `[UNLOCK]` semantics: Locked → Open on BOTH halves of the reciprocal
    /// pair; an Open connection is a no-op; Blocked refuses; unknown areas
    /// and non-adjacent targets refuse.
    #[test]
    fn unlock_flips_reciprocal_pair_and_refuses_blocked() {
        let mut m = demo_map();
        m.current_area = Some("hall".into());
        // hall ↔ vault is Locked in the demo map.
        assert_eq!(
            connection_state_between(&m, "hall", "vault"),
            Some(ConnState::Locked)
        );
        assert_eq!(connection_state_between(&m, "vault", "hall"), Some(ConnState::Locked));
        assert_eq!(unlock_connection_pair(&mut m, "vault"), Ok(true));
        assert_eq!(connection_state_between(&m, "hall", "vault"), Some(ConnState::Open));
        assert_eq!(
            connection_state_between(&m, "vault", "hall"),
            Some(ConnState::Open),
            "the reciprocal half flips too"
        );
        // Already open → no-op.
        assert_eq!(unlock_connection_pair(&mut m, "vault"), Ok(false));
        // Unknown area → reject with the site-block wording.
        assert!(unlock_connection_pair(&mut m, "nowhere").is_err());
        // Non-adjacent (gatehouse ↔ vault has no direct edge) → reject.
        m.current_area = Some("gatehouse".into());
        let err = unlock_connection_pair(&mut m, "vault").expect_err("must refuse");
        assert!(err.contains("does not connect"), "{err}");
        // Blocked refuses with the physical-change wording.
        for area in m.areas.iter_mut() {
            for c in area.connections.iter_mut() {
                if c.to == "hall" || c.to == "gatehouse" {
                    c.state = ConnState::Blocked;
                }
            }
        }
        let err = unlock_connection_pair(&mut m, "hall").expect_err("must refuse");
        assert!(err.contains("BLOCKED"), "{err}");
        // Legacy map (no current_area) falls back to the entrance: the
        // gatehouse ↔ hall pair (Blocked above) still refuses; reset it to
        // Locked to prove the fallback FROM-end.
        for area in m.areas.iter_mut() {
            for c in area.connections.iter_mut() {
                if c.to == "hall" || c.to == "gatehouse" {
                    c.state = ConnState::Locked;
                }
            }
        }
        m.current_area = None;
        assert_eq!(unlock_connection_pair(&mut m, "hall"), Ok(true));
    }

    /// (2026-08-24 traversal fix) A `[ROOM]` move to a KNOWN area with NO
    /// connecting edge must be barred. The lib.rs gate composes this fn as
    /// `Some(non-Open)` = refuse — the old no-edge `None` passed the gate
    /// (the gatehouse→vault teleport across the demo map).
    #[test]
    fn no_edge_between_known_areas_is_barred_for_room_moves() {
        let m = demo_map();
        // gatehouse ↔ vault: both areas exist, no direct edge → Blocked.
        assert_eq!(
            connection_state_between(&m, "gatehouse", "vault"),
            Some(ConnState::Blocked)
        );
        // The gate composition (the lib.rs applier's filter): a known
        // target with no way must refuse the move.
        assert!(
            connection_state_between(&m, "gatehouse", "vault")
                .filter(|st| *st != ConnState::Open)
                .is_some()
        );
        // A real locked edge still refuses; the open pair still passes.
        assert_eq!(
            connection_state_between(&m, "hall", "vault"),
            Some(ConnState::Locked)
        );
        assert!(
            connection_state_between(&m, "hall", "vault")
                .filter(|st| *st != ConnState::Open)
                .is_some()
        );
        assert_eq!(
            connection_state_between(&m, "gatehouse", "hall"),
            Some(ConnState::Open)
        );
        assert!(
            connection_state_between(&m, "gatehouse", "hall")
                .filter(|st| *st != ConnState::Open)
                .is_none()
        );
        // Unknown areas stay None (the callers' "not part of this site"
        // domain — unchanged).
        assert_eq!(connection_state_between(&m, "gatehouse", "nowhere"), None);
        assert_eq!(connection_state_between(&m, "nowhere", "hall"), None);
        // [UNLOCK] keeps its distinct NotAdjacent directive for the same
        // no-edge pair (never the BLOCKED physical-change wording).
        let mut u = demo_map();
        u.current_area = Some("gatehouse".into());
        assert_eq!(classify_unlock(&u, "vault"), UnlockOutcome::NotAdjacent);
    }

    // ---- (2026-08-22 multihog WS3) site pressure queue --------------------

    /// The node pressure queue: `clean_free_text` capping, exact-duplicate
    /// suppression, FIFO at NODE_PRESSURE_MAX.
    #[test]
    fn push_node_pressure_caps_fifo_and_dedupes() {
        let mut node = crate::schema::Node::default();
        assert!(push_node_pressure(&mut node, "the garrison's debt comes due"));
        assert!(push_node_pressure(&mut node, "smoke on the ridge grows thicker"));
        assert!(push_node_pressure(&mut node, "a rival clan scouts the passes"));
        assert_eq!(node.pending_pressure.len(), NODE_PRESSURE_MAX);
        // A duplicate line never doubles.
        assert!(!push_node_pressure(&mut node, "smoke on the ridge grows thicker"));
        assert_eq!(node.pending_pressure.len(), NODE_PRESSURE_MAX);
        // The 4th distinct line FIFOs the oldest out.
        assert!(push_node_pressure(&mut node, "the well runs lower each week"));
        assert_eq!(node.pending_pressure.len(), NODE_PRESSURE_MAX);
        assert!(
            !node.pending_pressure.contains(&"the garrison's debt comes due".to_string()),
            "the oldest line fell"
        );
        assert!(node.pending_pressure[0].contains("smoke on the ridge"));
        // Oversize + empty lines clean to the seed-hook discipline.
        let long = "x".repeat(SITE_SEED_CHAR_MAX + 50);
        assert!(push_node_pressure(&mut node, &long));
        assert!(
            node.pending_pressure.last().unwrap().chars().count() <= SITE_SEED_CHAR_MAX,
            "clean_free_text caps the line"
        );
        assert!(!push_node_pressure(&mut node, "   "));
    }

    // ---- (2026-08-23) BFS movement guard + knowledge⇒area promotion -------

    /// An off-screen `move_asset` needs a walk of OPEN connections: an open
    /// path applies; a move whose only path crosses a Locked/Blocked edge
    /// rejects with the open-routes wording and never applies; a same-area
    /// move is legal.
    #[test]
    fn move_asset_requires_an_open_walk() {
        // Open path applies: gatehouse ↔ hall is Open in the demo map.
        let mut m = demo_map();
        let mut mv = op("move_asset", "gate-keeper");
        mv.to = Some("hall".into());
        mv.cause = "chased the noise".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[mv], 2_000);
        assert_eq!((applied, rejects.len()), (1, 0));
        assert_eq!(
            m.assets.iter().find(|a| a.id == "gate-keeper").unwrap().location,
            "hall"
        );

        // The only walk crosses a Locked edge (hall ↔ vault) → rejects and
        // the warband stays put.
        let mut m = demo_map();
        let mut mv = op("move_asset", "warband-scouts");
        mv.to = Some("vault".into());
        mv.cause = "smelled blood".into();
        let (applied, rejects) = apply_evolution_ops(&mut m, &[mv], 2_000);
        assert_eq!(applied, 0);
        assert!(
            rejects.iter().any(|r| r.contains("no open way") && r.contains("open routes")),
            "reject must carry the open-routes wording: {rejects:?}"
        );
        assert_eq!(
            m.assets.iter().find(|a| a.id == "warband-scouts").unwrap().location,
            "hall",
            "a walled-off move never applies"
        );

        // Same-area move is legal (`from == to` is a trivial walk).
        let mut m = demo_map();
        let mut mv = op("move_asset", "gate-keeper");
        mv.to = Some("gatehouse".into());
        let (applied, rejects) = apply_evolution_ops(&mut m, &[mv], 2_000);
        assert_eq!((applied, rejects.len()), (1, 0));

        // Direct BFS checks over the demo map: open walks, walled walks,
        // self, and unknown targets.
        let m = demo_map();
        assert!(open_path_exists(&m, "gatehouse", "hall"));
        assert!(open_path_exists(&m, "hall", "gatehouse"));
        assert!(!open_path_exists(&m, "hall", "vault"), "a locked door bars the walk");
        assert!(
            !open_path_exists(&m, "gatehouse", "vault"),
            "2 hops through a locked door is still walled"
        );
        assert!(open_path_exists(&m, "vault", "vault"));
        assert!(!open_path_exists(&m, "gatehouse", "nowhere"));
    }

    /// The knowledge⇒area invariant: a revealed asset reveals its room
    /// (Unrevealed → Discovered); Visited never downgrades; already-
    /// Discovered no-ops; an Unrevealed asset or unknown id promotes
    /// nothing.
    #[test]
    fn revealed_asset_promotes_its_area() {
        // Suspected asset in an Unrevealed room → the room becomes Discovered.
        let mut m = demo_map();
        m.assets[0].knowledge = AssetKnowledge::Suspected; // warband in the hall
        promote_area_knowledge_for_asset(&mut m, "warband-scouts");
        assert_eq!(
            m.areas.iter().find(|a| a.id == "hall").unwrap().knowledge,
            AreaKnowledge::Discovered
        );

        // A Visited room stays Visited (never downgraded, never re-stamped).
        let mut m = demo_map();
        promote_area_knowledge_for_asset(&mut m, "gate-keeper"); // Known, gatehouse
        assert_eq!(
            m.areas.iter().find(|a| a.id == "gatehouse").unwrap().knowledge,
            AreaKnowledge::Visited
        );

        // Already-Discovered stays Discovered (never raised to Visited).
        let mut m = demo_map();
        m.assets[0].knowledge = AssetKnowledge::Known;
        for area in m.areas.iter_mut() {
            if area.id == "hall" {
                area.knowledge = AreaKnowledge::Discovered;
            }
        }
        promote_area_knowledge_for_asset(&mut m, "warband-scouts");
        assert_eq!(
            m.areas.iter().find(|a| a.id == "hall").unwrap().knowledge,
            AreaKnowledge::Discovered
        );

        // An UNREVEALED asset promotes nothing (knowledge must lead), and an
        // unknown id is a clean no-op.
        let mut m = demo_map();
        promote_area_knowledge_for_asset(&mut m, "warband-scouts");
        promote_area_knowledge_for_asset(&mut m, "zzz-none");
        assert_eq!(
            m.areas.iter().find(|a| a.id == "hall").unwrap().knowledge,
            AreaKnowledge::Unrevealed
        );
    }

    // ── player_slice (2026-08-23 fog-of-war map) ──────────────────────────
    // The player-facing knowledge filter: the IPC payload must never carry
    // hidden truth (names, geometry, assets, or fog↔fog structure).

    #[test]
    fn player_slice_filters_hidden_truth_to_fog_stubs() {
        let m = demo_map();
        // Gatehouse Visited; hall + vault Unrevealed, hall adjacent to the
        // gatehouse (fog stub ?1), vault only adjacent to hall (NOT a stub —
        // 1-hop law).
        let s = player_slice(&m, "The Warren", false, &[]);
        assert_eq!(s.site_name, "The Warren");
        assert_eq!(s.threat, "high");
        assert_eq!(s.entrance, "gatehouse");
        // current_area falls back to the entrance (no WS2 stamp on demo).
        assert_eq!(s.current_area, "gatehouse");
        let ids: Vec<&str> = s.areas.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"gatehouse"));
        assert!(ids.contains(&"?1"), "hall must surface as anonymous ?1");
        assert!(!ids.contains(&"?2"), "vault is 2-hop — never a stub");
        assert_eq!(s.areas.len(), 2);
        // The fog stub is nameless + knowledge-tagged.
        let stub = s.areas.iter().find(|a| a.id == "?1").unwrap();
        assert_eq!(stub.name, "");
        assert_eq!(stub.knowledge, "fog");
        assert!(stub.geometry.is_empty());
        assert!(stub.assets.is_empty());
        // The Visited area carries its geometry + the Known gate keeper.
        let gate = s.areas.iter().find(|a| a.id == "gatehouse").unwrap();
        assert_eq!(gate.knowledge, "visited");
        assert_eq!(gate.geometry, vec!["cold draft through murder holes"]);
        assert_eq!(gate.assets.len(), 1);
        assert_eq!(gate.assets[0].name, "Gate Keeper");
        assert!(!gate.assets[0].suspected);
        assert_eq!(gate.assets[0].state, ""); // Active renders bare
        // Edges: gatehouse↔hall renders (known→fog, both endpoints visible);
        // hall↔vault is fog↔fog — NEVER (hidden structure).
        assert_eq!(s.edges.len(), 1);
        assert_eq!(s.edges[0].from, "gatehouse");
        assert_eq!(s.edges[0].to, "?1");
        assert_eq!(s.edges[0].state, "open");
    }

    #[test]
    fn player_slice_discovered_is_name_only() {
        let mut m = demo_map();
        for area in m.areas.iter_mut() {
            if area.id == "hall" {
                area.knowledge = AreaKnowledge::Discovered;
            }
        }
        let s = player_slice(&m, "The Warren", false, &[]);
        let hall = s.areas.iter().find(|a| a.id == "hall").unwrap();
        assert_eq!(hall.knowledge, "discovered");
        assert_eq!(hall.name, "Great Hall");
        assert!(hall.geometry.is_empty(), "discovered carries no geometry");
        assert!(hall.assets.is_empty(), "assets render only under Visited");
        // vault is now adjacent to a KNOWN area (hall) → ?1 stub.
        assert!(s.areas.iter().any(|a| a.id == "?1"));
    }

    #[test]
    fn player_slice_dedupes_reciprocal_pairs_and_carries_states() {
        let mut m = demo_map();
        for area in m.areas.iter_mut() {
            if area.id == "hall" {
                area.knowledge = AreaKnowledge::Discovered;
            }
        }
        // hall↔gatehouse (open, reciprocal) + hall↔vault (locked) both have a
        // known endpoint → exactly 2 edges, one per unordered pair.
        let s = player_slice(&m, "x", false, &[]);
        assert_eq!(s.edges.len(), 2);
        let locked = s
            .edges
            .iter()
            .find(|e| e.state == "locked")
            .expect("locked hall↔vault edge renders (door is a visible fact)");
        // Pair endpoints sorted: "?1" (vault stub) < "hall".
        assert_eq!(locked.from, "?1");
        assert_eq!(locked.to, "hall");
    }

    #[test]
    fn player_slice_suspected_assets_flag_without_state() {
        let mut m = demo_map();
        // Suspected scouts in the VISITED gatehouse (relocate).
        for a in m.assets.iter_mut() {
            if a.id == "warband-scouts" {
                a.location = "gatehouse".into();
                a.knowledge = AssetKnowledge::Suspected;
            }
        }
        let s = player_slice(&m, "x", false, &[]);
        let gate = s.areas.iter().find(|a| a.id == "gatehouse").unwrap();
        let scouts = gate.assets.iter().find(|a| a.name == "Warband Scouts").unwrap();
        assert!(scouts.suspected);
        assert_eq!(scouts.state, "", "suspicion never implies state");
        // (2026-08-24 review fix) ...and never a magnitude: the count badge
        // is Known-only (the struct law the old assertion contradicted).
        assert_eq!(scouts.count, 0, "suspicion never implies a group size");
    }

    #[test]
    fn player_slice_caps_assets_at_eight() {
        let mut m = demo_map();
        for i in 0..12 {
            m.assets.push(SiteAsset {
                id: format!("obj-{i}"),
                name: format!("Object {i}"),
                kind: AssetKind::Object,
                location: "gatehouse".into(),
                state: AssetState::Active,
                knowledge: AssetKnowledge::Known,
                count: 0,
                detail: String::new(),
                tier: None,
                origin: AssetOrigin::InitialMap,
                changed_at_minutes: 0,
                cause: String::new(),
                actor: String::new(),
                expires_at_minutes: None,
            });
        }
        let s = player_slice(&m, "x", false, &[]);
        let gate = s.areas.iter().find(|a| a.id == "gatehouse").unwrap();
        assert_eq!(gate.assets.len(), 8);
        // (2026-08-24 review P2) The overflow count rides beside the cap —
        // the frontend "+N" chip's number (12 assets → 8 shown + 4 hidden).
        assert_eq!(gate.assets_overflow, 4);
    }

    #[test]
    fn player_slice_finds_fog_stub_from_known_side_declaration() {
        // A hand-edited map whose unrevealed area declares NO connections of
        // its own, while the KNOWN area declares the edge to it — the stub
        // still surfaces (either side of the pair may declare).
        let mut m = demo_map();
        let gate = m.areas.iter_mut().find(|a| a.id == "gatehouse").unwrap();
        gate.connections.push(SiteConnection {
            to: "vault".into(),
            state: ConnState::Blocked,
            detail: "rubble-choked arch".into(),
        });
        let vault = m.areas.iter_mut().find(|a| a.id == "vault").unwrap();
        vault.connections.clear();
        let s = player_slice(&m, "x", false, &[]);
        // File-order stub numbering: hall (idx 1) is ?1, vault (idx 2) is ?2.
        assert!(s.areas.iter().any(|a| a.id == "?1"));
        assert!(s.areas.iter().any(|a| a.id == "?2"));
        let blocked = s.edges.iter().find(|e| e.state == "blocked").unwrap();
        assert_eq!(blocked.from, "gatehouse");
        assert_eq!(blocked.to, "?2");
    }

    #[test]
    fn player_slice_anchors_objectives_on_known_areas_only() {
        // (2026-08-25 quest anchors) hall Discovered (known, name-only);
        // vault stays Unrevealed → an anchor on it must stay invisible
        // (the knowledge gate), as must an anchor naming an absent room.
        let mut m = demo_map();
        for area in m.areas.iter_mut() {
            if area.id == "hall" {
                area.knowledge = AreaKnowledge::Discovered;
            }
        }
        let anchored: Vec<AnchoredObjective> = vec![
            ("gatehouse".into(), "Investigate the cutpurse".into()),
            ("hall".into(), "Question the steward".into()),
            ("vault".into(), "Secret vault errand".into()),
            ("nowhere".into(), "Ghost objective".into()),
        ];
        let s = player_slice(&m, "x", false, &anchored);
        let gate = s.areas.iter().find(|a| a.id == "gatehouse").unwrap();
        assert_eq!(gate.quests, vec!["Investigate the cutpurse"]);
        let hall = s.areas.iter().find(|a| a.id == "hall").unwrap();
        assert_eq!(
            hall.quests,
            vec!["Question the steward"],
            "a discovered-but-unvisited room still carries its go-here pin"
        );
        let stub = s.areas.iter().find(|a| a.id == "?1").unwrap();
        assert!(stub.quests.is_empty(), "the vault is fog — its anchor stays hidden");
        // The per-area cap: 5 objectives on one room → 3 ride the wire.
        let many: Vec<AnchoredObjective> = (0..5)
            .map(|i| ("gatehouse".into(), format!("Task {i}")))
            .collect();
        let s2 = player_slice(&m, "x", false, &many);
        let gate2 = s2.areas.iter().find(|a| a.id == "gatehouse").unwrap();
        assert_eq!(gate2.quests.len(), 3);
    }

    // ---- (2026-08-25 location-card redesign) the marker vocabulary ----

    #[test]
    fn marker_kind_maps_the_asset_classes() {
        let a = |kind, name: &str, tier: Option<&str>| SiteAsset {
            kind,
            name: name.into(),
            tier: tier.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(marker_kind(&a(AssetKind::Loot, "Wine Rack", None)), "loot");
        assert_eq!(marker_kind(&a(AssetKind::Trap, "Set Snare", None)), "hazard");
        assert_eq!(marker_kind(&a(AssetKind::Hazard, "Deeper Pools", None)), "hazard");
        assert_eq!(marker_kind(&a(AssetKind::Object, "Notice Board", None)), "quest");
        assert_eq!(marker_kind(&a(AssetKind::Building, "Bell Tower", None)), "general");
        assert_eq!(marker_kind(&a(AssetKind::Building, "The Smithy", None)), "shop");
        assert_eq!(marker_kind(&a(AssetKind::Building, "Wayside Inn", None)), "safe");
        // SAFE outranks SHOP when a name carries both (the key's priority).
        assert_eq!(marker_kind(&a(AssetKind::Building, "Inn & Tavern", None)), "safe");
    }

    #[test]
    fn marker_kind_creature_disposition_tier_then_vocabulary() {
        let a = |name: &str, tier: Option<&str>| SiteAsset {
            kind: AssetKind::Group,
            name: name.into(),
            tier: tier.map(str::to_string),
            knowledge: AssetKnowledge::Known,
            ..Default::default()
        };
        // Boss: the explicit Elite+ tier wins regardless of name.
        assert_eq!(marker_kind(&a("Something Behind the Crates", Some("boss"))), "boss");
        assert_eq!(marker_kind(&a("Honor Guard", Some("legendary"))), "boss");
        assert_eq!(marker_kind(&a("Champion", Some("elite"))), "boss");
        // (2026-08-25 leak fix) The skull is a tier leak on SUSPICION: a
        // merely-Suspected creature keeps the vocabulary classes.
        let mut sus = a("Something Behind the Crates", Some("boss"));
        sus.knowledge = AssetKnowledge::Suspected;
        assert_eq!(marker_kind(&sus), "friendly", "suspected hides the tier class");
        // Minion/soldier tiers stay disposition-classified.
        assert_eq!(marker_kind(&a("Warband Scouts", Some("soldier"))), "hostile");
        // Hostile vocabulary (word-boundary).
        assert_eq!(marker_kind(&a("Wolf Pack", None)), "hostile");
        assert_eq!(marker_kind(&a("Cellar Rat", None)), "hostile");
        // Unmatched names default to the FRIENDLY marker (the
        // settled-places safe read) — and a dead wolf keeps its hostile
        // class (states ride the hover text, never the marker).
        assert_eq!(marker_kind(&a("Gate Watch", None)), "friendly");
        let mut dead = a("Wolf Pack", None);
        dead.state = AssetState::Dead;
        assert_eq!(marker_kind(&dead), "hostile");
    }

    // ---- (2026-08-23 hazard referees) rumor → Suspected-asset seeding ----

    #[test]
    fn seed_rumor_asset_mints_suspected_threat_at_entrance() {
        let mut m = demo_map();
        assert!(seed_rumor_asset(&mut m, "bandits raid the eastern road", 1200));
        let seeded = m.assets.iter().find(|a| a.cause.starts_with("rumor: ")).unwrap();
        assert_eq!(seeded.kind, AssetKind::Group, "'bandits' is plural → Group");
        assert_eq!(seeded.name, "Bandits");
        assert_eq!(seeded.id, "bandits");
        assert_eq!(seeded.knowledge, AssetKnowledge::Suspected, "suspicion, never truth");
        assert_eq!(seeded.state, AssetState::Active);
        assert_eq!(seeded.location, "gatehouse", "placed at the entrance area");
        assert_eq!(seeded.origin, AssetOrigin::Evolved);
        assert_eq!(seeded.changed_at_minutes, 1200);
        assert!(seeded.cause.contains("bandits raid the eastern road"));
    }

    #[test]
    fn seed_rumor_asset_creature_for_singular_word() {
        let mut m = demo_map();
        assert!(seed_rumor_asset(&mut m, "a beast stalks the high fields", 60));
        let seeded = m.assets.iter().find(|a| a.cause.starts_with("rumor: ")).unwrap();
        assert_eq!(seeded.kind, AssetKind::Creature, "'beast' is singular → Creature");
        assert_eq!(seeded.name, "Beast");
    }

    #[test]
    fn seed_rumor_asset_dedupes_by_label() {
        let mut m = demo_map();
        assert!(seed_rumor_asset(&mut m, "bandits raid the road", 10));
        assert!(!seed_rumor_asset(&mut m, "bandits raid the road", 20));
        assert_eq!(
            m.assets.iter().filter(|a| a.cause.starts_with("rumor: ")).count(),
            1
        );
        // A DISTINCT threat label still seeds (until the cap).
        assert!(seed_rumor_asset(&mut m, "a monster prowls the wharf", 30));
        // …but the per-map rumor cap is 2.
        assert!(!seed_rumor_asset(&mut m, "orcs mass at the gate", 40));
        assert_eq!(
            m.assets.iter().filter(|a| a.cause.starts_with("rumor: ")).count(),
            RUMOR_ASSET_MAX
        );
    }

    /// (2026-08-24 review P1) The id-uniqueness invariant: two DIFFERENT
    /// rumor labels carrying the same threat word mints the SAME kebab id
    /// — the second must refuse (id-keyed consumers are first-match-wins;
    /// a duplicate silently shadowed). And a seeded Group's count must
    /// land in the validator's 1..=99 band.
    #[test]
    fn seed_rumor_asset_dedupes_by_id_and_mints_legal_group_count() {
        let mut m = demo_map();
        assert!(seed_rumor_asset(&mut m, "bandits raid the road", 10));
        // Different label, SAME threat word → same id `bandits` → refuse.
        assert!(!seed_rumor_asset(&mut m, "the bandits robbed the mill", 20));
        assert_eq!(
            m.assets.iter().filter(|a| a.id == "bandits").count(),
            1,
            "id must stay unique"
        );
        // The minted Group satisfies the validator's count law.
        let seeded = m.assets.iter().find(|a| a.id == "bandits").unwrap();
        assert_eq!(seeded.kind, AssetKind::Group);
        assert!(
            seeded.count >= 1 && seeded.count <= 99,
            "group count {} outside 1..=99",
            seeded.count
        );
        // And the whole map still validates (the rumor seed can't mint a
        // map the validator rejects).
        assert!(validate(&m).is_ok());
    }

    #[test]
    fn seed_rumor_asset_ignores_benign_labels() {
        let mut m = demo_map();
        assert!(!seed_rumor_asset(&mut m, "the stranger paid in gold", 10));
        assert!(m.assets.iter().all(|a| !a.cause.starts_with("rumor: ")));
    }

    #[test]
    fn seed_rumor_asset_respects_the_global_cap() {
        let mut m = demo_map();
        // demo_map carries 2 assets; fill to the global cap with distinct
        // threat labels — but the rumor cap (2) fires first, so bump the
        // global cap scenario directly: pre-place 15 assets.
        while m.assets.len() < MAX_SITE_ASSETS {
            m.assets.push(SiteAsset {
                id: format!("filler-{}", m.assets.len()),
                cause: String::new(),
                ..Default::default()
            });
        }
        assert!(!seed_rumor_asset(&mut m, "bandits raid the road", 10));
        assert_eq!(m.assets.len(), MAX_SITE_ASSETS);
    }
}
