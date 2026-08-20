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
/// the architect's 512-token decode (`SITE_ARCHITECT_MAX_TOKENS`). A larger
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
/// under 512 tokens; the sniper is the primary stop, this is the wall).
pub const SITE_ARCHITECT_MAX_TOKENS: i32 = 512;
/// Char cap for area/asset ids (kebab ids are short by construction).
pub const SITE_ID_CHAR_MAX: usize = 64;

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
}

impl AssetState {
    pub fn word(self) -> &'static str {
        match self {
            AssetState::Active => "active",
            AssetState::Dead => "dead",
            AssetState::Taken => "taken",
            AssetState::Triggered => "triggered",
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
}

/// The whole hidden map of one node's interior.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct SiteMap {
    /// The travel-graph node this map belongs to (forced to the current node
    /// at insert — the map is keyed by it in `WorldSchema::site_maps`).
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

/// The NARRATOR's slice: knowledge-filtered rich prose. Visited areas render
/// geometry + their Known assets (state/count/detail) + Suspected assets as
/// suspicion lines + named ways on; Discovered areas render name-only;
/// unrevealed neighbors render as `?` stub counts. Hidden truth (unrevealed
/// areas/assets) NEVER renders. `(+N more)` caps bound the block.
pub fn render_narrator_slice(map: &SiteMap) -> Option<String> {
    const AREAS_SHOWN: usize = 6;
    const ASSETS_SHOWN: usize = 6;
    let mut out: Vec<String> = Vec::new();
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
                    if asset.count > 0 {
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
    if out.is_empty() {
        None
    } else {
        Some(out.iter().map(|l| flatten(l)).collect::<Vec<_>>().join("\n"))
    }
}

/// The TRACKER's slice: compact id-bearing line so the E4B can emit real
/// `[ROOM]`/`[ASSET]` ids. Lists visited (`id:v doors=<to>:<state>,…`) and
/// discovered (`id:d`) areas + the doors out of them — a door's TARGET id is
/// a visible fact (that's how an unrevealed room is first entered), while
/// the room itself stays a `?`. Known/Suspected assets render id + state +
/// group count. Single flattened line (the lean surgery caps it further).
pub fn render_tracker_slice(map: &SiteMap) -> Option<String> {
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
    for a in &map.assets {
        if a.knowledge == AssetKnowledge::Unrevealed {
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
    Some(flatten(&line))
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
/// Dead/Taken. An asset's explicit `tier` wins; otherwise the site's
/// `threat` default. `None` when nothing hostile is on the table (the
/// Referee falls back to its own entity-tier selection).
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
        if matches!(a.state, AssetState::Dead | AssetState::Taken) {
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
    let victim = site_maps
        .iter()
        .filter(|(k, _)| k.as_str() != current_node)
        .min_by_key(|(_, m)| m.last_visit_minutes)
        .map(|(k, _)| k.clone());
    if let Some(v) = &victim {
        site_maps.remove(v);
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
                },
            ],
            last_visit_minutes: 1_000,
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
        let slice = render_narrator_slice(&m).expect("non-empty slice");
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
        let slice = render_tracker_slice(&m).expect("non-empty slice");
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
            },
            crate::schema::Node {
                id: "beta".into(),
                name: "Beta".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 0,
            },
            crate::schema::Node {
                id: "gamma".into(),
                name: "Gamma".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 2_000,
            },
            crate::schema::Node {
                id: "mapped".into(),
                name: "Mapped".into(),
                neighbors: vec![],
                setting: String::new(),
                seeds: vec![],
                last_evolved_minutes: 0,
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
}
