//! The world-state schema: "the schema IS the summarizer."
//!
//! A persistent, semi-structured record of the simulated world's current
//! state: a running narrative summary, recent salient events, and a flexible
//! key→value entity map (characters, inventory, locations, stats, quest
//! flags). Updated after every chat turn by the background state-delta pass
//! (see `schema_engine.rs`), which emits ONLY the changed keys as a
//! [`SchemaDelta`]; this module's [`WorldSchema::apply_delta`] merges that
//! delta into the global state.
//!
//! # The micro-delta contract
//!
//! The delta pass NEVER rewrites the whole schema. It emits a small JSON
//! object containing only the keys that changed this turn. This keeps the
//! delta pass fast (token-bound autoregression: a 20-token delta takes
//! ~0.6s vs ~60s for a full regen) and lets the model focus on what
//! actually moved rather than re-describing the whole world each turn.
//!
//! # Key removal
//!
//! `null` in a delta's `entities` map means "delete this key." A non-null
//! value means "set/overwrite." This is unambiguous for a string-valued
//! schema and JSON-native.
//!
//! # Persistence
//!
//! `world_schema.json` in the app data dir (sibling to `session.json`).
//! Atomic save (temp + fsync + rename), same pattern as
//! [`crate::session::Conversation::save`]. Loaded at startup into `AppState`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

// Pull the player-state types in at the top of schema.rs so the new
// `player_state` field + render can reference them unqualified. Sibling
// module (declared in lib.rs); the structs themselves are pure data.
use crate::consequence::StatusTag;
use crate::offscreen_task::OffScreenTask;
use crate::player_state::PlayerState;
use crate::relationship::RelationshipState;
use crate::rumor; // Component 4 (2026-07-28): WorldSchema.rumors field references rumor::Rumor.

/// The in-world clock (Fable Seam #4, 2026-07-27). Pure data: two `i64`
/// minute-counters since the fixed ancient epoch 0001-01-01 (the same trick
/// Multihog's `parseInWorldTime` uses). All math is subtraction + comparison;
/// no calendar library needed.
///
/// `WorldSchema::apply_delta` deliberately does NOT touch this struct — it
/// lives outside the LLM delta path, same architectural line as `PlayerState`.
/// The ONLY writer is the bracket-command applier in `fable_send`, which reads
/// `[TIME ...]` emissions from the narrator + sets `current_minutes`. The
/// World Progression tick gate then compares against `last_tick_minutes` to
/// decide whether the off-screen simulation should fire.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, Default)]
pub struct WorldClock {
    /// Current in-world time, in minutes since 0001-01-01. `0` means unset
    /// (no `[TIME ...]` has been emitted yet — the clock is dormant). Once
    /// the first `[TIME]` lands this is monotonically non-decreasing: the
    /// bracket applier guards against regressions (a narrator emitting
    /// `[TIME Day 1]` after `[TIME Day 5]` is warned + ignored).
    #[serde(default)]
    pub current_minutes: i64,

    /// The in-world time we last fired a World Progression tick for, in the
    /// same epoch-minutes units. `0` means "never fired": the first parseable
    /// `[TIME]` stamps this as a baseline (no fire — matches Multihog's
    /// first-call behavior so a campaign doesn't immediately simulate a day
    /// it hasn't established yet).
    #[serde(default)]
    pub last_tick_minutes: i64,
}

impl WorldClock {
    /// True once the narrator has emitted at least one parseable `[TIME ...]`.
    /// Until then the clock is dormant: the tick gate is a no-op, the render
    /// emits no `clock:` block (zero tokens).
    pub fn is_set(&self) -> bool {
        self.current_minutes > 0
    }

    /// Minutes elapsed since the last World Progression tick (or since the
    /// baseline if no tick has fired yet). Pure subtraction — the gate just
    /// checks `>= interval_minutes`. Negative only if the clock was somehow
    /// set backward (shouldn't happen: the applier's monotonic guard rejects
    /// regressions).
    pub fn minutes_since_last_tick(&self) -> i64 {
        self.current_minutes - self.last_tick_minutes
    }

    /// Render the current clock as a compact prompt line. Returns `None`
    /// when the clock is unset (so `render_for_prompt` can skip the block
    /// entirely — zero tokens for a fresh game). The format is deliberately
    /// human-readable ("Day 3, 14:00") so the narrator can emit coherent
    /// `[TIME ...]` progressions: it sees the current time, advances it by
    /// the scene's elapsed time, emits the new value.
    ///
    /// The conversion back from epoch-minutes to "Day N, HH:MM" mirrors the
    /// forward parse in `bracket_parser::parse_in_world_time`: 1 day = 1440
    /// minutes, day index = `minutes / 1440 + 1`, time-of-day = `minutes % 1440`.
    pub fn render_clock_line(&self) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let day = self.current_minutes / 1440 + 1;
        let rem = self.current_minutes % 1440;
        let h24 = rem / 60;
        let m = rem % 60;
        Some(format!("Day {day}, {h24:02}:{m:02}"))
    }
}

/// The global atmospheric condition (Fable Phase 4 Component 2, 2026-07-28).
/// Pure data: a free-form diegetic condition phrase + the in-world minute at
/// which it started (drives the persistence-DC curve in `weather::drift_weather`).
///
/// `WorldSchema::apply_delta` deliberately does NOT touch this struct — it
/// lives outside the LLM delta path, same architectural line as `WorldClock` /
/// `PlayerState` / `ScenePacing` / `status_tags`. The ONLY writers are:
/// (1) the `[WEATHER ...]` bracket command (sets the condition + stamps the
/// start time), (2) the World Progression tick drift (pure Rust, seeded RNG —
/// shifts the condition from the generic pool when a persistence check fails).
/// Global for v1 — the narrator sees one `weather:` line for the whole world.
/// Component 3 (Node-Based Spatial Travel Graph, SHIPPED 2026-07-28) adds the
/// only node→weather coupling: the `weather:` line is suppressed when the
/// current node's `setting` is "indoor" (see `TravelGraph::current_is_indoor`).
/// Full per-node weather (climate-specific condition pools) is a Component 4+
/// refinement; the bracket syntax + tick hook are forward-compatible with it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Weather {
    /// Diegetic condition phrase the narrator weaves into prose ("heavy
    /// rain", "clear", "thick fog", "snowfall"). Free-form; the narrator owns
    /// the prose, Rust owns the dice. Empty = unset (no `[WEATHER]` emitted
    /// yet — weather is dormant, like a fresh clock with no `[TIME]`).
    #[serde(default)]
    pub condition: String,

    /// The in-world minute this condition started (epoch-minutes, same units
    /// as `WorldClock::current_minutes`). Drives the persistence curve: the
    /// longer a condition has held, the higher its drift DC (overdue for a
    /// shift). 0 = unset. Stamped by the `[WEATHER]` applier + by the tick
    /// drift on a successful shift.
    #[serde(default)]
    pub started_at_minutes: i64,
}

impl Weather {
    /// True once a `[WEATHER ...]` has set a non-empty condition (or the tick
    /// has drifted to one). Until then the weather is dormant: the tick drift
    /// is a no-op, the render emits no `weather:` block (zero tokens).
    pub fn is_set(&self) -> bool {
        !self.condition.is_empty()
    }

    /// Render the current weather as a compact prompt line. Returns `None`
    /// when unset (so `render_for_prompt` can skip the block entirely — zero
    /// tokens for a fresh game, mirroring `WorldClock::render_clock_line`).
    pub fn render_line(&self) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        Some(self.condition.clone())
    }
}

/// A discrete in-world location (Fable Phase 4 Component 3, 2026-07-28).
/// Structural truth Rust reasons about (edges, reachability); NOT free-form
/// narrative flavor — that lives in `entities` (`Node.name` is the diegetic
/// label only, `entities` holds the flavor prose). v1 is geography only:
/// flat adjacency graph, no weights / terrain / coordinates / distance.
///
/// `WorldSchema::apply_delta` deliberately does NOT touch nodes — they live
/// outside the LLM delta path, same line as `WorldClock` / `Weather` /
/// `PlayerState`. The ONLY writer in v1 is the scenario card (seeded at game
/// start); the player's `current_node` advances via `[TRAVEL]`. (Component 4
/// may add NPC movement between nodes on the World Progression tick — that
/// will require NPC-position state, NOT a mutation of the graph itself.)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Node {
    /// Stable identifier ("tavern", "cellar", "market_square"). Bare slug,
    /// NOT "node.tavern" — the `node.` prefix is a narrator convention only
    /// (the `[TRAVEL]` parser strips it for ergonomics).
    #[serde(default)]
    pub id: String,

    /// Diegetic name shown to the narrator ("The Rusty Anchor tavern"). This
    /// is the prose label only; flavor prose about the node lives in `entities`
    /// (e.g. `entities["node.tavern.flavor"]`).
    #[serde(default)]
    pub name: String,

    /// Reachable neighbors by node id. Pure adjacency — NO weights, NO
    /// terrain, NO distance (anti-bloat: the weighted-graph trap). Component 4
    /// rumor propagation reads this; the `[TRAVEL]` Referee validates against
    /// it (non-neighbor moves are rejected).
    #[serde(default)]
    pub neighbors: Vec<String>,

    /// Free hint, lowercased + matched against a tiny known set ("indoor" /
    /// "outdoor" / empty). Zero-cost when empty; "indoor" gates whether the
    /// global `weather:` line renders for the current node (the only
    /// node→weather coupling in v1 — see `render_for_prompt`). No enum —
    /// string match, forward-compat for richer flags without a migration.
    #[serde(default)]
    pub setting: String,
}

/// The spatial travel graph (Rust-authoritative — same line as `world_clock` /
/// `weather`). Nodes + a single `current_node` pointer. v1 is structural
/// geography: no NPC positions, no tick-resolved traversal mechanics, no
/// per-node weather. The only writer to `current_node` is the `[TRAVEL]`
/// bracket command (validated for adjacency); `nodes` itself is seeded once
/// by the scenario card + treated as read-only thereafter.
///
/// `WorldSchema::apply_delta` deliberately does NOT touch this field —
/// see `apply_delta_does_not_touch_travel_graph`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct TravelGraph {
    /// The nodes in the graph. `Vec` (not a map) — matches the
    /// `Vec<StatusTag>` / `Vec<OffScreenTask>` precedent; small graphs
    /// (dozens of nodes), O(n) edge validation is correct + cheap.
    #[serde(default)]
    pub nodes: Vec<Node>,

    /// Current location by node id. `None` = unseeded (dormant, like
    /// `WorldClock` before the first `[TIME]`). Collocated with `nodes` so
    /// "current_node refers to a real node" is enforceable in one place
    /// (no desync vector). The first `[TRAVEL]` from `None` is allowed —
    /// seeds initial location without scenario-card wiring.
    #[serde(default)]
    pub current_node: Option<String>,
}

impl TravelGraph {
    /// True once at least one node exists (the dormant contract — a fresh
    /// game with no seeded nodes suppresses the `location:` block entirely).
    pub fn is_set(&self) -> bool {
        !self.nodes.is_empty()
    }

    /// Linear scan for a node by id. Small graphs (dozens of nodes); O(n) is
    /// correct + cheap. Returns `None` for unknown ids.
    pub fn find_node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// `Some(&Node)` if `current_node` is set AND refers to a real node.
    /// Defensively returns `None` if the pointer has drifted off the graph
    /// (should never happen given the collocation invariant, but cheap to
    /// honor rather than unwrap).
    pub fn current(&self) -> Option<&Node> {
        self.current_node
            .as_ref()
            .and_then(|id| self.find_node(id))
    }

    /// True if `to` is a declared neighbor of the current node — the
    /// `[TRAVEL]` Referee's anti-sycophancy gate. Returns `false` if there
    /// is no current node, `to` is not in the graph, or `to` is not adjacent.
    /// (The first-move-from-`None` bootstrap case is handled by the caller,
    /// not here — this fn strictly answers "is it a neighbor?".)
    pub fn is_adjacent_to_current(&self, to: &str) -> bool {
        match self.current() {
            Some(cur) => cur.neighbors.iter().any(|n| n == to),
            None => false,
        }
    }

    /// True if the current node's `setting` (lowercased) is "indoor" — gates
    /// whether the global `weather:` line renders. Returns `false` when there
    /// is no current node or the setting is empty / outdoor / unrecognized
    /// (i.e. weather renders by default; only explicit "indoor" suppresses).
    pub fn current_is_indoor(&self) -> bool {
        match self.current() {
            Some(cur) => cur.setting.trim().eq_ignore_ascii_case("indoor"),
            None => false,
        }
    }

    /// Compact prompt render. Returns `None` when dormant (no current node).
    /// Mirrors `Weather::render_line` (single-region, dormant-suppress).
    /// Emits ONE line:
    ///   `<name> [<id>] (exits: <comma-joined neighbor names or ids>)`
    /// Neighbor names resolve to diegetic names where possible; unknown
    /// neighbor ids fall back to the bare id (defensive — never panics).
    pub fn render_line(&self) -> Option<String> {
        let cur = self.current()?;
        let exits: Vec<String> = cur
            .neighbors
            .iter()
            .map(|id| {
                self.find_node(id)
                    .map(|n| n.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone())
            })
            .collect();
        let exits_str = if exits.is_empty() {
            "none".to_string()
        } else {
            exits.join(", ")
        };
        Some(format!("{} [{}] (exits: {})", cur.name, cur.id, exits_str))
    }
}

/// The scene pacing mode (Fable Seam #4 expansion, 2026-07-27): a
/// Rust-computed per-turn classification of the scene's rhythm. Drives:
/// (1) the narrator prose cadence via a `<scene_pacing>` prompt tag, (2) the
/// World Progression tick interval (background sim speed), (3) the skill-check
/// DC modifier (tension raises stakes). Pure enum, no data — derived state
/// lives on the parent `ScenePacing` struct.
///
/// The three modes are NOT exhaustive buckets of "all possible scene types":
/// they are the operationally meaningful buckets for the three hooks above.
/// `Downtime` = "let the world breathe + easier checks"; `Exploration` =
/// "balanced"; `Combat` = "fast clock + harder checks + terse prose."
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum SceneMode {
    /// Combat / high-stakes action. Terse present-tense prose, fast clock
    /// (no background sim mid-fight), DC modifier +2 (tension).
    Combat,
    /// Default. Balanced prose, hourly tick (every 4h), DC modifier +0.
    #[default]
    Exploration,
    /// Rest / travel / trade / long dialogue. Lush slow prose, fast background
    /// sim (world moves while you recover), DC modifier −2 (relaxed, easier).
    Downtime,
}

impl SceneMode {
    /// Short lowercase tag for the prompt attribute: `<scene_pacing mode="combat">`.
    pub fn tag(self) -> &'static str {
        match self {
            SceneMode::Combat => "combat",
            SceneMode::Exploration => "exploration",
            SceneMode::Downtime => "downtime",
        }
    }

    /// Narrator-facing prose guidance for the `<scene_pacing>` tag. Each mode
    /// gets a one-line instruction the narrator obeys when pacing its prose.
    /// Tuned to be mode-specific (not just "be terse" vs "be lush" — the
    /// combat line calls for short sentences and present-tense verbs, the
    /// downtime line calls for sensory detail and slow beats).
    pub fn prose_guidance(self) -> &'static str {
        match self {
            SceneMode::Combat => "Pace your prose for combat: short sentences, present-tense verbs, no interiority during the exchange — save reflection for after the dust settles. Each turn covers seconds, not minutes.",
            SceneMode::Exploration => "Pace your prose for exploration: balanced beats, a mix of action and atmosphere. Each turn covers roughly a minute of in-world time.",
            SceneMode::Downtime => "Pace your prose for downtime: linger on sensory detail, ambient sound, the texture of the place. Each turn can cover an hour or more — let time breathe.",
        }
    }

    /// World Progression tick interval in hours, by mode. The off-screen
    /// simulation fires when in-world elapsed time crosses this gate. `Combat`
    /// returns 0 (a sentinel meaning "never fire mid-combat" — the applier
    /// short-circuits before the schema-engine dispatch). Tunable later via
    /// per-card overrides; today these are the v1 defaults.
    pub fn progression_interval_hours(self) -> u32 {
        match self {
            // Combat: seconds-scale action. The background sim is suspended —
            // the next non-combat turn resumes it. Returning 0 is a sentinel
            // the applier reads as "skip this tick" (it must check for 0
            // before dividing).
            SceneMode::Combat => 0,
            // Exploration: 4 hours. A long delve or journey spans enough
            // in-world time for the world to shift while you're away.
            SceneMode::Exploration => 4,
            // Downtime: 1 hour. Resting/traveling lets the off-screen world
            // move fast — by the time you finish your rest, news has arrived,
            // NPCs have shifted.
            SceneMode::Downtime => 1,
        }
    }

    /// Additive DC modifier for skill checks, by mode. Higher = harder.
    /// Combat scenes are tense (harder to persuade mid-fight); Downtime is
    /// relaxed (easier to talk someone around when nobody's stressed).
    pub fn dc_modifier(self) -> i32 {
        match self {
            SceneMode::Combat => 2,
            SceneMode::Exploration => 0,
            SceneMode::Downtime => -2,
        }
    }
}

/// Per-turn scene pacing state, computed by `scene_pacing::evaluate` from the
/// player's turn text and persisted on `WorldSchema` (so the most recent
/// mode survives across turns + autosave — the narrator's next turn inherits
/// the prior scene's rhythm unless the new turn re-classifies it).
///
/// The three pillar scores (0..=2 = low/med/high) are kept for tracing and
/// future tuning surface; the operationally meaningful field is `mode`. The
/// mode is derived from the pillars via a simple mapping (kinetic==2 →
/// Combat; kinetic==0 && emotional==0 → Downtime; else Exploration).
///
/// `WorldSchema::apply_delta` deliberately does NOT touch this struct — Rust
/// is the SOLE authority (mirrors `world_clock` + `player_state`). The only
/// writer is `fable_send`, which sets `schema.scene_pacing` each turn from
/// the freshly-evaluated value before the prompt render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct ScenePacing {
    /// The classified scene mode. This is the operationally consumed field.
    #[serde(default)]
    pub mode: SceneMode,
    /// Spatial scale: 0 = enclosed (room/tavern), 1 = open-but-civilized
    /// (street/market), 2 = wilderness (ocean/mountain/road). Kept for
    /// tracing + future tuning; not directly consumed by any hook today.
    #[serde(default)]
    pub spatial: u8,
    /// Emotional vector: 0 = calm (chat/rest/trade), 1 = tense (argument/
    /// suspicion), 2 = alarmed (fight/flight/panic). Drives nothing directly
    /// today; the mode mapping reads it.
    #[serde(default)]
    pub emotional: u8,
    /// Kinetic scale: 0 = static (talking/looking), 1 = mobile (walking/
    /// traveling), 2 = violent (reuses `player_state::COMBAT_KEYWORDS`).
    /// The dominant pillar for mode classification.
    #[serde(default)]
    pub kinetic: u8,
}

/// The persistent world-state schema. The single source of truth for the
/// simulated world's current state, maintained by the background delta pass.
///
/// Semi-structured by design: a fixed envelope (`summary`, `recent_events`)
/// gives the model a stable narrative anchor, while the flexible `entities`
/// map adapts to any scenario (fantasy inventory, sci-fi ship status, modern
/// day relationship tracker) without code changes. Keys are model-defined.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct WorldSchema {
    /// One-paragraph running narrative summary. The model rewrites this when
    /// the narrative arc shifts: NOT every turn. Carries the "where are we
    /// in the story" thread that the 6-message window can't hold alone.
    #[serde(default)]
    pub summary: String,

    /// Recent salient events, newest appended at the end. The model appends
    /// new events and may trim old ones in a delta. Bounded growth is the
    /// model's responsibility (the prompt instructs it to keep this list
    /// short); a Rust-side cap could be added later if it drifts.
    #[serde(default)]
    pub recent_events: Vec<String>,

    /// Flexible key→value store for hard data. Keys are model-defined and
    /// namespaced by convention (e.g. `"item.iron_sword"`,
    /// `"char.mira.trust"`, `"loc.current"`). Values are strings: structured
    /// enough to read programmatically, loose enough to hold anything.
    ///
    /// In a delta, a `None` value (JSON `null`) means "delete this key";
    /// `Some(v)` means "set/overwrite."
    #[serde(default)]
    pub entities: HashMap<String, String>,

    /// The player's canonical state (Fable Seam #7, Player State).
    /// Rust is the SOLE authority here — the schema-delta LLM pass never
    /// writes to it; the Referee (`player_state::referee_evaluate`) does.
    /// Nested inside WorldSchema so per-card persistence + autosave inherit
    /// for free (no new file, no new AppState field). `#[serde(default)]`
    /// keeps pre-PlayerState saves loadable as a fully-healthy body.
    #[serde(default)]
    pub player_state: PlayerState,

    /// The in-world clock (Fable Seam #4, 2026-07-27). Rust is the SOLE
    /// authority — the schema-delta LLM pass never writes here (mirrors
    /// `player_state`'s architectural line). Set by the `[TIME ...]`
    /// bracket-command parser from the narrator's emissions. Drives the
    /// World Progression tick gate: when in-world time advances past the
    /// interval, an off-screen simulation pass fires against the schema's
    /// entities (see `schema_engine::request_world_progression`).
    ///
    /// `current_minutes == 0` means "no [TIME] emitted yet" — the clock is
    /// unset, the tick gate never fires. The first parseable [TIME] stamps
    /// both `current_minutes` and `last_tick_minutes` (baseline; no fire),
    /// matching Multihog's first-call behavior.
    #[serde(default)]
    pub world_clock: WorldClock,

    /// Global weather state (Fable Phase 4 Component 2, 2026-07-28): the
    /// current atmospheric condition + the in-world minute it started (for the
    /// tick drift's persistence curve). Rust is the SOLE authority —
    /// `apply_delta` does NOT touch this field (mirrors `world_clock` /
    /// `player_state` / `scene_pacing` / `status_tags`). The only writers are:
    /// (1) the `[WEATHER ...]` bracket command (sets the condition + stamps
    /// the start), (2) the World Progression tick drift (`weather::drift_
    /// weather` — pure Rust, seeded RNG; shifts the condition from the
    /// generic pool when a persistence check fails). Global for v1 — the
    /// narrator sees one `weather:` line for the whole world. Component 3
    /// (Node-Based Spatial Travel Graph, SHIPPED 2026-07-28) adds the only
    /// node→weather coupling: the `weather:` line is suppressed when the
    /// current node's `setting` is "indoor" (the narrator doesn't see weather
    /// while indoors — see `TravelGraph::current_is_indoor`). Per-node weather
    /// pools (climate-specific) remain a Component 4+ refinement; the bracket
    /// syntax + tick hook are forward-compatible with it.
    /// `#[serde(default)]` keeps pre-Phase-4 saves loadable as unset (empty
    /// condition → dormant, no `weather:` line).
    #[serde(default)]
    pub weather: Weather,

    /// The spatial travel graph (Fable Phase 4 Component 3, 2026-07-28):
    /// discrete locations (nodes) + adjacency edges + a single current
    /// location pointer. Rust is the SOLE authority — `apply_delta` does NOT
    /// touch this field (mirrors `world_clock` / `weather` / `player_state`).
    /// The only writer to `current_node` is the `[TRAVEL ...]` bracket command
    /// (the Tracker owns "the player moved"; Rust validates adjacency + rejects
    /// non-neighbor moves). `nodes` itself is seeded once by the scenario card
    /// and treated as read-only thereafter. v1 is geography only: no NPC
    /// positions, no tick traversal mechanics, no weighted edges.
    /// `#[serde(default)]` keeps pre-Component-3 saves loadable as an empty
    /// graph (dormant — no `location:` line, no adjacency to validate).
    #[serde(default)]
    pub travel_graph: TravelGraph,

    /// Entity keys flagged immutable (the `[CORE]`-style lock; closes the §5
    /// "deliberately permissive at v1" NPC-drift hole). A key in this set can
    /// be SET once (on its first appearance in the schema) but never
    /// overwritten or deleted by a subsequent delta. The schema validator
    /// enforces this at §5 layer 1 (microsecond Rust check, zero LLM cost).
    ///
    /// Use case: an NPC's canonical identity (e.g. `"npc.marcus.core"`) is
    /// flagged immutable on creation. The model can still append to
    /// `"npc.marcus.chronicle"` to record character development — character
    /// arc allowed, continuity error blocked. Today the set ships EMPTY: the
    /// model isn't instructed to emit immutability flags yet. A follow-up
    /// (Wupi-as-game-manager) populates this when she creates NPCs.
    #[serde(default)]
    pub immutable_keys: HashSet<String>,

    /// Scene pacing state (Fable Seam #4 expansion, 2026-07-27): the most
    /// recently classified scene rhythm + its pillar scores. Rust is the SOLE
    /// authority — `apply_delta` does NOT touch it (mirrors `world_clock` +
    /// `player_state`). The only writer is `fable_send`, which sets this each
    /// turn from a fresh `scene_pacing::evaluate(text)` before the prompt
    /// render. Nested inside `WorldSchema` so per-card persistence + autosave
    /// inherit for free (no new file, no new AppState field). `#[serde(default)]`
    /// keeps pre-ScenePacing saves loadable as `Exploration` (the neutral
    /// default — a loaded save continues at balanced pacing until the next
    /// turn re-classifies).
    #[serde(default)]
    pub scene_pacing: ScenePacing,

    /// Active qualitative buff/debuff tags with WorldClock expiry (Fable
    /// Phase 3 Slice 4 wiring, 2026-07-28). Each tag carries a diegetic label
    /// ("Berserk Rage", "Feverish", "Blessed by the Sun Priest"), a polarity,
    /// and the in-world minute at which it expires (same epoch-minutes units
    /// as `world_clock.current_minutes`). Rust is the SOLE authority —
    /// `apply_delta` does NOT touch this field (mirrors `player_state` /
    /// `world_clock` / `scene_pacing`). The only writers are: (1) the
    /// `[EFFECT ...]` bracket command (creates a tag with timed expiry), (2)
    /// the World Progression tick (drops expired tags via
    /// `consequence::expire_tags`). The body's read-time-derived `Condition`
    /// consumes the active tag counts — see `consequence::derive_condition`.
    /// `#[serde(default)]` keeps pre-Phase-3 saves loadable as an empty list.
    #[serde(default)]
    pub status_tags: Vec<StatusTag>,

    /// Per-NPC relationship state (Fable Phase 3 Slice 5 wiring, 2026-07-28).
    /// Keyed by NPC id (matching the entity-map convention — e.g.
    /// `"npc.marcus"`). Each value is a `RelationshipState` (tier, entered-at
    /// timestamp, recorded events, volatility). Rust is the SOLE authority —
    /// `apply_delta` does NOT touch this field (same Rust-authoritative
    /// pattern). The only writers are: (1) the `[MILESTONE ...]` bracket
    /// command (records an event), (2) the inline silent-strip helper that
    /// upserts a Stranger→Acquaintance transition when the LLM emits an
    /// accepted `rel.<npc>=acquaintance` write. Hostility-triggered
    /// transitions fire lazily on render via `evaluate_transition`. The LLM
    /// CANNOT directly write a gated tier — `strip_invalid_relationship_writes`
    /// drops the attempt silently (per the architect directive: gated
    /// escalations are silent-dropped, not repaired). `#[serde(default)]`
    /// keeps pre-Phase-3 saves loadable as an empty map.
    #[serde(default)]
    pub relationships: HashMap<String, RelationshipState>,

    /// Pending off-screen task queue (Fable Phase 3 Slice 6 wiring,
    /// 2026-07-28). Each `OffScreenTask` carries an NPC id, description,
    /// difficulty, suitability, and ETA (the in-world minute at which it
    /// resolves). Rust is the SOLE authority — `apply_delta` does NOT touch
    /// this field. The only writers are: (1) the `[TASK ...]` bracket command
    /// (queues a task), (2) the World Progression tick (resolves due tasks
    /// via `offscreen_task::resolve_expired_tasks` + drops resolved ones).
    /// Resolved tasks emit directives into `pending_tick_directives` on
    /// AppState, consumed by the next `fable_send`'s `<directives>` block.
    /// `#[serde(default)]` keeps pre-Phase-3 saves loadable as an empty queue.
    #[serde(default)]
    pub offscreen_tasks: Vec<OffScreenTask>,

    /// Propagation-based rumor state (Fable Phase 4 Component 4, 2026-07-28):
    /// free-form diegetic phrases that spread between connected nodes on the
    /// World Progression tick. Each [`rumor::Rumor`] owns its `known_nodes`
    /// (the nodes that have heard it). Rust is the SOLE authority —
    /// `apply_delta` does NOT touch this field (mirrors `world_clock` /
    /// `weather` / `travel_graph` / `status_tags` / `offscreen_tasks`). The
    /// only writers are: (1) the `[RUMOR ...]` bracket command (creates a
    /// rumor rooted at the current node — `known_nodes` initialized to
    /// `[origin_node]`), (2) the World Progression tick propagation pass
    /// (`rumor::propagate_rumors` — pure Rust, seeded RNG; spreads each
    /// rumor to adjacent unknown nodes when an age-decayed DC check passes).
    ///
    /// The `rumors:` render line in `render_for_prompt` shows ONLY rumors the
    /// CURRENT node has heard (the node-based knowledge model — "the tavern
    /// has heard X", not "Marcus specifically has heard X"). Propagation-only
    /// by design: no polarity / truth field, no stored reputation score —
    /// reputation is narratively derived from which rumor texts circulate.
    /// `#[serde(default)]` keeps pre-Component-4 saves loadable as an empty
    /// list (dormant — no `rumors:` line, nothing to propagate).
    #[serde(default)]
    pub rumors: Vec<rumor::Rumor>,
}

impl WorldSchema {
    /// Deep-merge a micro-delta into self. The "native Rust merging"
    /// requirement: the model emits only changed keys, Rust applies them.
    ///
    /// Semantics:
    /// - `summary`: overwrite if the delta carries one (the model only emits
    ///   this when the narrative arc actually shifted).
    /// - `recent_events`: append the delta's events at the end (newest last).
    ///   No dedupe: the model is responsible for not re-emitting existing
    ///   events. Trimming old events is the model's job too (it sees the full
    ///   current list in the delta prompt and can drop stale ones by emitting
    ///   a replacement... actually no: append-only is the v1 contract; the
    ///   model can rewrite `summary` to fold old events in, and a future
    ///   "replace recent_events" signal could land if trimming becomes needed).
    /// - `entities`: for each (key, value) in the delta: `Some(v)` → upsert,
    ///   `None` → remove the key (no-op if it didn't exist).
    pub fn apply_delta(&mut self, delta: SchemaDelta) {
        if let Some(summary) = delta.summary {
            self.summary = summary;
        }
        if let Some(mut events) = delta.recent_events {
            self.recent_events.append(&mut events);
        }
        if let Some(ents) = delta.entities {
            for (key, value) in ents {
                match value {
                    Some(v) => {
                        self.entities.insert(key, v);
                    }
                    None => {
                        self.entities.remove(&key);
                    }
                }
            }
        }
    }

    /// Render the schema into a compact, prompt-friendly string for injection
    /// into the chat turn's `<world_state>` block. Compactness matters: this
    /// goes into the inter-turn region alongside the memory block, and every
    /// token is prefill cost. We emit the summary, the last few recent events
    /// (not all: the model doesn't need the deep history list in chat, that's
    /// what the delta pass sees in full), and the entities as `key: value`
    /// lines.
    ///
    /// Returns an empty string for an empty schema so the caller can skip
    /// emitting the `<world_state>` block entirely (matches the memory block's
    /// empty-skip behavior in `chat_format.rs`).
    pub fn render_for_prompt(&self) -> String {
        let empty = self.summary.trim().is_empty()
            && self.recent_events.is_empty()
            && self.entities.is_empty()
            && self.player_state.is_default()
            && !self.world_clock.is_set()
            && !self.weather.is_set()
            && !self.travel_graph.is_set()
            && self.rumors.is_empty();
        if empty {
            return String::new();
        }

        let mut out = String::with_capacity(512);
        // Clock renders FIRST (before summary): the narrator needs the current
        // time as its top-of-mind anchor so its [TIME ...] emissions advance
        // it coherently. Without seeing the current time, the narrator would
        // emit inconsistent timestamps. ~30 tokens; zero when unset.
        if let Some(clock_line) = self.world_clock.render_clock_line() {
            out.push_str("clock: ");
            out.push_str(&clock_line);
            out.push('\n');
        }
        // Weather renders alongside clock (Component 2, 2026-07-28): the two
        // atmospheric anchors the narrator needs top-of-mind to write coherent
        // scene-setting prose (a storm should color sound + visibility + NPC
        // reactions; the narrator can't weave weather it can't see). Empty
        // condition → no line (zero tokens for a fresh game, dormant until the
        // first [WEATHER] or the first tick-driven shift).
        //
        // Component 3 coupling (2026-07-28): when the current node's `setting`
        // is "indoor", the weather line is suppressed — the narrator doesn't
        // see weather while inside (a windowless cellar doesn't show rain).
        // Outdoor / empty / unset setting → weather renders as before. This is
        // the ONLY node→weather coupling in v1 (no per-node weather data).
        if !self.travel_graph.current_is_indoor() {
            if let Some(weather_line) = self.weather.render_line() {
                out.push_str("weather: ");
                out.push_str(&weather_line);
                out.push('\n');
            }
        }
        // Location renders alongside clock + weather (Component 3, 2026-07-28):
        // the third top-of-mind anchor. The narrator needs the current location
        // + its exits to write coherent movement prose + emit valid `[TRAVEL]`
        // commands (without seeing the exits, the Tracker would guess at node
        // ids). `None` when no current node is set (dormant — zero tokens,
        // mirroring `WorldClock` / `Weather` before their first command).
        if let Some(travel_line) = self.travel_graph.render_line() {
            out.push_str("location: ");
            out.push_str(&travel_line);
            out.push('\n');
        }
        // Rumors at the current node (Component 4, 2026-07-28): the fourth
        // "where/when" anchor + the node-based knowledge model. The narrator
        // sees ONLY the rumors the current node has heard — "the tavern has
        // heard X", not "Marcus specifically has heard X" (per-NPC knowledge
        // graphs are the anti-bloat trap, deliberately avoided at v1). This is
        // what frames ambient gossip + NPC reactions in prose (the propagated
        // rumor texts ARE the reputation signal — no stored score). Dormant
        // when no current node OR no heard rumors (zero tokens for a fresh
        // game, or for a node the rumor hasn't reached yet — the player can
        // travel away from a rumor and have it vanish from this line).
        if let Some(cur_id) = self.travel_graph.current_node.as_deref() {
            let heard: Vec<&str> = self
                .rumors
                .iter()
                .filter(|r| r.known_nodes.iter().any(|n| n == cur_id))
                .map(|r| r.label.as_str())
                .collect();
            if !heard.is_empty() {
                out.push_str("rumors: ");
                out.push_str(&heard.join("; "));
                out.push('\n');
            }
        }
        if !self.summary.trim().is_empty() {
            out.push_str("summary: ");
            out.push_str(self.summary.trim());
            out.push('\n');
        }
        // Cap recent events shown in chat at the last 5: older events live
        // in the persisted schema + memory retrieval, not the chat prompt.
        let show_events = self.recent_events.len().saturating_sub(5);
        if !self.recent_events[show_events..].is_empty() {
            out.push_str("recent_events:\n");
            for ev in &self.recent_events[show_events..] {
                out.push_str("  - ");
                out.push_str(ev);
                out.push('\n');
            }
        }
        if !self.entities.is_empty() {
            out.push_str("entities:\n");
            // Sort keys for deterministic output (stable prompt = stable tokens).
            let mut keys: Vec<&String> = self.entities.keys().collect();
            keys.sort();
            for key in keys {
                out.push_str("  ");
                out.push_str(key);
                out.push_str(": ");
                out.push_str(&self.entities[key]);
                out.push('\n');
            }
        }
        // Player state (the Rust Referee's canonical fact block). Rendered
        // LAST in the world-state block so it's the loudest signal — the
        // player's injuries + fatigue are the most turn-relevant facts.
        // Returns None when fully default, so a fresh game adds zero tokens.
        if let Some(player_block) = self.player_state.render_for_prompt() {
            out.push_str("player_state:\n");
            for line in player_block.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        // Trim the trailing newline: the caller wraps this in a tag block.
        out.trim_end().to_string()
    }

    /// Serialize for the delta pass's "current schema" prompt input. Pretty-
    /// printed JSON so the model can read it clearly; the schema is small.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Atomic save to `world_schema.json` (temp + fsync + rename, same pattern
    /// as `session::Conversation::save`). A crash mid-write can never truncate
    /// the existing file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let tmp_path = temp_path_for(path);
        let _ = std::fs::remove_file(&tmp_path); // clear stale temp

        {
            let mut file = std::fs::File::create(&tmp_path)?;
            std::io::Write::write_all(&mut file, json.as_bytes())?;
            std::io::Write::flush(&mut file)?;
            let _ = file.sync_all();
        }
        std::fs::rename(&tmp_path, path)
    }

    /// Load from `world_schema.json`. Returns an empty schema if the file
    /// doesn't exist yet (first run): never an error for the NotFound case.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
}

/// A micro-delta against [`WorldSchema`]. All fields optional: the model
/// emits ONLY the keys that changed this turn. Omitted fields = unchanged.
///
/// Deserialized from the JSON object the schema-delta model pass emits. The
/// `entities` field's inner `Option<String>` is load-bearing: outer `Option`
/// = "did any entity change?", inner `Option` = "is this a delete (`null`)
/// or a set (`Some`)?". `serde` deserializes JSON `null` to `None` and a
/// string to `Some(string)`, giving us the unambiguous delete-vs-set signal
/// for free.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SchemaDelta {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub recent_events: Option<Vec<String>>,
    #[serde(default)]
    pub entities: Option<HashMap<String, Option<String>>>,
}

impl SchemaDelta {
    /// Parse a model-emitted string into a delta. Tolerant of three layers of
    /// wrapping the model may apply:
    /// 1. The Gemma4 channel protocol (`<|channel>thought\n...<channel|>reply`).
    ///    The model emits this protocol for ALL output (including the schema
    ///    delta pass, which is instructed to emit raw JSON). The JSON lives in
    ///    the REPLY channel: the text after the last `<channel|>` marker.
    /// 2. Markdown fences (```` ```json ... ``` ````): stripped if present.
    /// 3. Surrounding whitespace.
    ///
    /// Runtime-discovered 2026-07-13: the delta pass emitted
    /// `<|channel>thought\n<channel|>{}`: a valid empty delta `{}` wrapped in
    /// the channel protocol. Without extracting the reply channel, serde saw
    /// `<|channel>...` and bailed at column 1.
    pub fn from_model_output(raw: &str) -> Result<Self, serde_json::Error> {
        let reply = extract_reply_channel(raw);
        let cleaned = strip_markdown_fences(&reply).trim();
        // Phase 3: microsecond-cost syntactic repair BEFORE the parse. Catches
        // the common LLM JSON mistakes (trailing commas, smart quotes, unquoted
        // keys, bare newlines in strings, truncated closers) so a delta that
        // would have burned a 5-8s LLM repair pass instead parses first try.
        // Keeps the locked §5 contract intact — this is syntactic only; semantic
        // repair (wrong keys/types) still goes through the 3-pass loop.
        let repaired = crate::json_repair::repair(cleaned);
        serde_json::from_str(&repaired)
    }

    /// True if the delta carries ANY actual change (summary, events, or entity
    /// mutations). False for an empty `{}` delta: which the model may emit when
    /// the player's request didn't translate to anything. Used by the
    /// game-manager routing (Phase E, 2026-07-18) to decide whether to apply +
    /// confirm vs. ask the player to rephrase.
    pub fn has_changes(&self) -> bool {
        self.summary.is_some()
            || self
                .recent_events
                .as_ref()
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            || self
                .entities
                .as_ref()
                .map(|m| !m.is_empty())
                .unwrap_or(false)
    }
}

/// Extract the reply channel from Gemma4 protocol output. The model emits
/// `<|channel>thought\n...<channel|>reply`: the thought channel (internal
/// reasoning) comes first, closed by `<channel|>`, then the reply text
/// follows. The JSON delta is in the reply.
///
/// Uses `rsplit_once("<channel|>")` to take everything after the LAST closing
/// marker, then strips any `<audio|>` markers from the reply. `<audio|>` is
/// the Gemma4 audio-channel closer: the model emits it mid-prose when it
/// "speaks" audio. Wupi renders no audio, so the marker would otherwise leak
/// as literal text (e.g. "Mira's voice is a<audio|> whisper"). A reply can
/// contain multiple `<audio|>` markers, so this is a replace-all, not a split.
///
/// Returns an owned `String` (not `&str`) because the `<audio|>` strip may
/// mutate. Allocation cost is negligible: this runs once per turn, not per
/// token.
///
/// Handles:
/// - Protocol-wrapped output (the common case): thought discarded, reply kept.
/// - Thought-only output (no reply): returns empty string, parse fails
///   gracefully (the repair prompt or error path takes over).
/// - Raw output with no protocol wrapping: returns the input with `<audio|>`
///   stripped (the rare case where the model emits content directly).
pub fn extract_reply_channel(raw: &str) -> String {
    let reply = match raw.rsplit_once("<channel|>") {
        Some((_, reply)) => reply,
        None => raw,
    };
    reply.replace("<audio|>", "")
}

/// Strip ```json ... ``` markdown fences if present. The model is told to
/// emit raw JSON but may wrap it anyway; this is cheaper than fighting the
/// model and more robust than erroring.
fn strip_markdown_fences(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        if let Some(body) = rest.strip_suffix("```") {
            return body;
        }
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(body) = rest.strip_suffix("```") {
            return body;
        }
    }
    trimmed
}

/// Build a sibling temp-file path for an atomic save. Mirrors
/// `session::temp_path_for`: same directory/volume so `rename` is atomic.
fn temp_path_for(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from("wupi.tmp"));
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_delta_upserts_entities() {
        let mut schema = WorldSchema::default();
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(HashMap::from([
                ("iron_sword".to_string(), Some("acquired".to_string())),
                ("loc.current".to_string(), Some("tavern".to_string())),
            ])),
        };
        schema.apply_delta(delta);
        assert_eq!(schema.entities.get("iron_sword"), Some(&"acquired".to_string()));
        assert_eq!(schema.entities.get("loc.current"), Some(&"tavern".to_string()));
    }

    #[test]
    fn apply_delta_null_deletes_key() {
        let mut schema = WorldSchema {
            summary: String::new(),
            recent_events: vec![],
            entities: HashMap::from([
                ("iron_sword".to_string(), "acquired".to_string()),
                ("loc.current".to_string(), "tavern".to_string()),
            ]),
            ..Default::default()
        };
        // Drop the sword, move locations.
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(HashMap::from([
                ("iron_sword".to_string(), None), // delete
                ("loc.current".to_string(), Some("forest".to_string())),
            ])),
        };
        schema.apply_delta(delta);
        assert!(!schema.entities.contains_key("iron_sword"), "null should delete");
        assert_eq!(schema.entities.get("loc.current"), Some(&"forest".to_string()));
    }

    #[test]
    fn apply_delta_null_on_missing_key_is_noop() {
        let mut schema = WorldSchema::default();
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(HashMap::from([("ghost".to_string(), None)])),
        };
        schema.apply_delta(delta);
        assert!(schema.entities.is_empty());
    }

    #[test]
    fn apply_delta_appends_recent_events() {
        let mut schema = WorldSchema {
            summary: String::new(),
            recent_events: vec!["entered tavern".to_string()],
            entities: HashMap::new(),
            ..Default::default()
        };
        let delta = SchemaDelta {
            summary: None,
            recent_events: Some(vec!["ordered ale".to_string(), "heard rumor".to_string()]),
            entities: None,
        };
        schema.apply_delta(delta);
        assert_eq!(
            schema.recent_events,
            vec!["entered tavern", "ordered ale", "heard rumor"]
        );
    }

    #[test]
    fn apply_delta_overwrites_summary() {
        let mut schema = WorldSchema {
            summary: "old summary".to_string(),
            recent_events: vec![],
            entities: HashMap::new(),
            ..Default::default()
        };
        let delta = SchemaDelta {
            summary: Some("new summary".to_string()),
            recent_events: None,
            entities: None,
        };
        schema.apply_delta(delta);
        assert_eq!(schema.summary, "new summary");
    }

    #[test]
    fn apply_delta_empty_is_noop() {
        let mut schema = WorldSchema {
            summary: "kept".to_string(),
            recent_events: vec!["kept".to_string()],
            entities: HashMap::from([("k".to_string(), "v".to_string())]),
            ..Default::default()
        };
        schema.apply_delta(SchemaDelta::default());
        assert_eq!(schema.summary, "kept");
        assert_eq!(schema.recent_events, vec!["kept"]);
        assert_eq!(schema.entities.get("k"), Some(&"v".to_string()));
    }

    #[test]
    fn has_changes_detects_populated_delta() {
        // Summary populated → has_changes.
        assert!(SchemaDelta {
            summary: Some("hi".into()),
            recent_events: None,
            entities: None,
        }
        .has_changes());
        // Recent events populated → has_changes.
        assert!(SchemaDelta {
            summary: None,
            recent_events: Some(vec!["e".into()]),
            entities: None,
        }
        .has_changes());
        // Entity mutations (even a delete/null) → has_changes.
        let mut ents = HashMap::new();
        ents.insert("k".to_string(), None);
        assert!(SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
        }
        .has_changes());
    }

    #[test]
    fn has_changes_false_for_empty_delta() {
        // Default (all-None) → no changes. This is the `{}` case the model
        // emits when a player's request didn't translate to anything.
        assert!(!SchemaDelta::default().has_changes());
        // Empty Vec / empty Map also count as no-op.
        assert!(!SchemaDelta {
            summary: None,
            recent_events: Some(Vec::new()),
            entities: Some(HashMap::new()),
        }
        .has_changes());
    }

    #[test]
    fn from_model_output_parses_clean_json() {
        let raw = r#"{"summary":"new","entities":{"x":"1"}}"#;
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(delta.summary.as_deref(), Some("new"));
        assert_eq!(
            delta.entities.unwrap().get("x"),
            Some(&Some("1".to_string()))
        );
    }

    #[test]
    fn from_model_output_strips_markdown_fences() {
        let raw = "```json\n{\"summary\":\"x\"}\n```";
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(delta.summary.as_deref(), Some("x"));
    }

    #[test]
    fn from_model_output_null_entity_value_is_delete_signal() {
        // JSON null deserializes to Option::None: the delete signal.
        let raw = r#"{"entities":{"drop_me":null}}"#;
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(delta.entities.unwrap().get("drop_me"), Some(&None));
    }

    #[test]
    fn from_model_output_strips_gemma4_channel_protocol() {
        // Regression for the 2026-07-13 runtime failure: the delta pass
        // emitted `<|channel>thought\n<channel|>{}`: a valid empty delta
        // wrapped in the Gemma4 channel protocol. Without extracting the
        // reply channel serde saw `<|channel>...` and bailed at column 1.
        let raw = "<|channel>thought\n<channel|>{}";
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert!(delta.summary.is_none());
        assert!(delta.recent_events.is_none());
        assert!(delta.entities.is_none());
    }

    #[test]
    fn from_model_output_extracts_json_after_thought_channel() {
        // The realistic case: model thinks briefly, then emits the JSON delta
        // in the reply channel.
        let raw = "<|channel>thought\nI should record the sword pickup.\n<channel|>{\"entities\":{\"item.iron_sword\":\"acquired\"}}";
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(
            delta.entities.unwrap().get("item.iron_sword"),
            Some(&Some("acquired".to_string()))
        );
    }

    #[test]
    fn from_model_output_channel_protocol_with_markdown_fence() {
        // Double wrapping: channel protocol + markdown fence. The reply
        // channel is extracted first, then the fence is stripped.
        let raw = "<|channel>thought\n<channel|>```json\n{\"summary\":\"updated\"}\n```";
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(delta.summary.as_deref(), Some("updated"));
    }

    #[test]
    fn from_model_output_thought_only_no_reply_is_error() {
        // The model emitted only a thought channel (no reply). Extraction
        // returns empty → parse fails gracefully. The repair prompt or error
        // path takes over; the schema is left unchanged for that turn.
        let raw = "<|channel>thought\nthinking...\n<channel|>";
        assert!(SchemaDelta::from_model_output(raw).is_err());
    }

    #[test]
    fn from_model_output_raw_json_without_protocol_passes_through() {
        // No channel markers at all: the model emitted JSON directly (rare
        // but possible). rsplit_once finds no `<channel|>` and returns the
        // whole string unchanged.
        let raw = r#"{"recent_events":["saw a fox"]}"#;
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(delta.recent_events.unwrap(), vec!["saw a fox".to_string()]);
    }

    #[test]
    fn render_for_prompt_empty_schema_is_empty_string() {
        assert_eq!(WorldSchema::default().render_for_prompt(), "");
    }

    #[test]
    fn render_for_prompt_caps_recent_events_at_five() {
        let schema = WorldSchema {
            summary: String::new(),
            recent_events: (0..10).map(|i| format!("event{i}")).collect(),
            entities: HashMap::new(),
            ..Default::default()
        };
        let rendered = schema.render_for_prompt();
        // Only the last 5 events should appear.
        assert!(rendered.contains("event5"));
        assert!(rendered.contains("event9"));
        assert!(!rendered.contains("event4"));
    }

    /// Player state (Seam #7) renders as a `player_state:` block at the
    /// tail of the world-state render — the loudest fact cluster. Default
    /// player state adds zero tokens (no block emitted).
    #[test]
    fn render_for_prompt_includes_player_state_when_injured() {
        let mut schema = WorldSchema::default();
        schema
            .player_state
            .body
            .insert(crate::player_state::BodyPart::LeftBicep, crate::player_state::BodyPartState::Orange);
        schema.player_state.stamina = crate::player_state::Stamina::Winded;
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("player_state:"), "player_state block must appear");
        assert!(rendered.contains("stamina: Winded"));
        assert!(rendered.contains("Left Bicep (Medium Injury)"));
    }

    /// A schema with ONLY a default player state + nothing else renders to
    /// the empty string (so a brand-new game emits no `<world_state>` block).
    #[test]
    fn render_for_prompt_default_player_state_emits_no_block() {
        let schema = WorldSchema::default();
        let rendered = schema.render_for_prompt();
        assert_eq!(rendered, "");
        assert!(!rendered.contains("player_state:"));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join("wupi_schema_test.json");
        let _ = std::fs::remove_file(&path);
        let schema = WorldSchema {
            summary: "test summary".to_string(),
            recent_events: vec!["e1".to_string()],
            entities: HashMap::from([("k".to_string(), "v".to_string())]),
            ..Default::default()
        };
        schema.save(&path).unwrap();
        let loaded = WorldSchema::load(&path).unwrap();
        assert_eq!(loaded.summary, "test summary");
        assert_eq!(loaded.recent_events, vec!["e1"]);
        assert_eq!(loaded.entities.get("k"), Some(&"v".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_default() {
        let path = std::env::temp_dir().join("wupi_schema_does_not_exist_xyz.json");
        let _ = std::fs::remove_file(&path);
        let loaded = WorldSchema::load(&path).unwrap();
        assert!(loaded.summary.is_empty());
        assert!(loaded.recent_events.is_empty());
        assert!(loaded.entities.is_empty());
    }

    #[test]
    fn extract_reply_channel_strips_audio_marker_mid_prose() {
        // Regression for the §2Z known issue: the model emits `<audio|>` (the
        // Gemma4 audio-channel closer) mid-prose. Without stripping it, the
        // marker leaks as literal text into narrator output, e.g.
        // "Mira's voice is a<audio|> whisper".
        let raw = "<|channel>thought\nsetting the scene.\n<channel|>Mira's voice is a<audio|> whisper.";
        assert_eq!(
            extract_reply_channel(raw),
            "Mira's voice is a whisper."
        );
    }

    #[test]
    fn extract_reply_channel_strips_multiple_audio_markers() {
        // A reply can contain more than one `<audio|>`: replace-all, not split.
        let raw = "<channel|>He says hi<audio|> and she replies<audio|>.";
        assert_eq!(extract_reply_channel(raw), "He says hi and she replies.");
    }

    #[test]
    fn extract_reply_channel_preserves_reply_without_markers() {
        // No `<audio|>` and no channel wrapping: input passes through unchanged.
        let raw = "The tavern door creaks open.";
        assert_eq!(extract_reply_channel(raw), "The tavern door creaks open.");
    }

    #[test]
    fn extract_reply_channel_audio_without_channel_marker() {
        // `<audio|>` present but no `<channel|>` at all (model emitted raw
        // prose with an audio marker, no thought channel). The audio marker
        // is still stripped.
        let raw = "A bell tolls<audio|> in the distance.";
        assert_eq!(extract_reply_channel(raw), "A bell tolls in the distance.");
    }

    // ---------- WorldClock (Seam #4) ----------

    #[test]
    fn world_clock_default_is_unset() {
        let clock = WorldClock::default();
        assert!(!clock.is_set());
        assert_eq!(clock.minutes_since_last_tick(), 0);
        assert_eq!(clock.render_clock_line(), None);
    }

    #[test]
    fn world_clock_is_set_when_current_positive() {
        let clock = WorldClock { current_minutes: 1440, last_tick_minutes: 0 };
        assert!(clock.is_set());
        // 1 day elapsed since the baseline.
        assert_eq!(clock.minutes_since_last_tick(), 1440);
    }

    #[test]
    fn world_clock_render_line_format() {
        // Day 3, 14:30 → (3-1)*1440 + 14*60 + 30 = 3720 + 30 = 3750 minutes.
        let clock = WorldClock { current_minutes: 3750, last_tick_minutes: 0 };
        assert_eq!(clock.render_clock_line().as_deref(), Some("Day 3, 14:30"));
    }

    #[test]
    fn world_clock_render_line_day_one_midnight() {
        // Day 1, 00:00 → 0 minutes. But is_set() returns false for 0! The
        // clock is only "set" once the narrator emits a nonzero time. Day 1
        // midnight is the dormant state — no render, no tick.
        let clock = WorldClock { current_minutes: 0, last_tick_minutes: 0 };
        assert!(!clock.is_set());
        assert_eq!(clock.render_clock_line(), None);
    }

    #[test]
    fn render_for_prompt_includes_clock_when_set() {
        let mut schema = WorldSchema::default();
        // 2880 minutes = Day 3, 00:00 (Day 1 starts at minute 0).
        schema.world_clock = WorldClock { current_minutes: 2880, last_tick_minutes: 0 };
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("clock: Day 3, 00:00"));
    }

    #[test]
    fn render_for_prompt_omits_clock_when_unset() {
        // A fresh game (no [TIME] emitted yet) → no clock block, zero tokens.
        let schema = WorldSchema::default();
        assert_eq!(schema.render_for_prompt(), "");
        assert!(!schema.render_for_prompt().contains("clock:"));
    }

    #[test]
    fn apply_delta_does_not_touch_world_clock() {
        // Architectural invariant: world_clock is outside the LLM delta path.
        // A delta carrying "world_clock" in its entities map must NOT mutate
        // the typed field — it would just become a regular entity key (which
        // the validator would later reject if flagged immutable, but at the
        // apply layer it's a no-op on the typed struct).
        let mut schema = WorldSchema::default();
        schema.world_clock = WorldClock { current_minutes: 1000, last_tick_minutes: 500 };
        let mut ents = HashMap::new();
        // A naive/malicious delta trying to set "world_clock" as an entity.
        ents.insert("world_clock".to_string(), Some("9999".to_string()));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
        };
        schema.apply_delta(delta);
        // The typed world_clock is unchanged.
        assert_eq!(schema.world_clock.current_minutes, 1000);
        assert_eq!(schema.world_clock.last_tick_minutes, 500);
        // The "world_clock" string landed in the entities map (it's just a
        // regular key from apply_delta's perspective). Whether it stays there
        // is the validator's call, not apply_delta's.
        assert_eq!(schema.entities.get("world_clock").map(|s| s.as_str()), Some("9999"));
    }

    // ---------- weather (Fable Phase 4 Component 2, 2026-07-28) ----------

    #[test]
    fn weather_default_is_unset() {
        // Fresh schema: weather dormant (empty condition). Mirrors world_clock.
        let schema = WorldSchema::default();
        assert!(!schema.weather.is_set());
        assert_eq!(schema.weather.render_line(), None);
    }

    #[test]
    fn weather_is_set_when_condition_non_empty() {
        let mut w = Weather::default();
        w.condition = "heavy rain".to_string();
        assert!(w.is_set());
        assert_eq!(w.render_line().as_deref(), Some("heavy rain"));
    }

    #[test]
    fn render_for_prompt_includes_weather_when_set() {
        let mut schema = WorldSchema::default();
        schema.weather = Weather {
            condition: "thick fog".to_string(),
            started_at_minutes: 1000,
        };
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("weather: thick fog"));
    }

    #[test]
    fn render_for_prompt_omits_weather_when_unset() {
        // A fresh game (no [WEATHER] yet) → no weather line, zero tokens.
        let schema = WorldSchema::default();
        assert!(!schema.render_for_prompt().contains("weather:"));
    }

    #[test]
    fn render_for_prompt_emits_weather_only_when_set() {
        // The empty predicate must include weather: a schema with ONLY weather
        // set (nothing else) should still emit the block.
        let mut schema = WorldSchema::default();
        schema.weather.condition = "clear".to_string();
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("weather: clear"));
        // And it shouldn't drag in empty blocks for unset fields.
        assert!(!rendered.contains("clock:"));
        assert!(!rendered.contains("summary:"));
    }

    #[test]
    fn apply_delta_does_not_touch_weather() {
        // Architectural invariant: weather is outside the LLM delta path
        // (mirrors apply_delta_does_not_touch_world_clock). A delta carrying
        // "weather" in its entities map must NOT mutate the typed field — it
        // just becomes a regular entity key (the playtest "weather" entity
        // convention; the typed field is authoritative going forward).
        let mut schema = WorldSchema::default();
        schema.weather = Weather {
            condition: "heavy rain".to_string(),
            started_at_minutes: 1000,
        };
        let mut ents = HashMap::new();
        // A naive/malicious delta trying to overwrite weather via entities.
        ents.insert("weather".to_string(), Some("sunny".to_string()));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
        };
        schema.apply_delta(delta);
        // The typed weather is unchanged.
        assert_eq!(schema.weather.condition, "heavy rain");
        assert_eq!(schema.weather.started_at_minutes, 1000);
        // The "weather" string landed in the entities map (legacy convention).
        assert_eq!(schema.entities.get("weather").map(|s| s.as_str()), Some("sunny"));
    }

    #[test]
    fn weather_backwards_compat_pre_phase4_save_loads_as_unset() {
        // A pre-Phase-4 save JSON (no "weather" field) must deserialize to
        // Weather::default() (unset). The #[serde(default)] attribute on the
        // field enforces this; this test pins it.
        let pre_phase4_json = r#"{
            "summary": "",
            "recent_events": [],
            "entities": {},
            "player_state": {},
            "world_clock": {"current_minutes": 0, "last_tick_minutes": 0},
            "immutable_keys": [],
            "scene_pacing": {"mode": "Exploration", "spatial": 0, "emotional": 0, "kinetic": 0},
            "status_tags": [],
            "relationships": {},
            "offscreen_tasks": []
        }"#;
        let parsed: WorldSchema = serde_json::from_str(pre_phase4_json)
            .expect("pre-Phase-4 JSON must deserialize");
        assert!(!parsed.weather.is_set());
        assert_eq!(parsed.weather.condition, "");
        assert_eq!(parsed.weather.started_at_minutes, 0);
    }

    #[test]
    fn weather_serialize_roundtrip() {
        // Save/load must preserve the weather field.
        let mut schema = WorldSchema::default();
        schema.weather = Weather {
            condition: "heavy rain".to_string(),
            started_at_minutes: 4321,
        };
        let dir = std::env::temp_dir();
        let path = dir.join("wupi_schema_weather_test.json");
        let _ = std::fs::remove_file(&path);
        schema.save(&path).unwrap();
        let loaded = WorldSchema::load(&path).unwrap();
        assert_eq!(loaded.weather.condition, "heavy rain");
        assert_eq!(loaded.weather.started_at_minutes, 4321);
        let _ = std::fs::remove_file(&path);
    }

    // ---------- travel_graph (Fable Phase 4 Component 3, 2026-07-28) ----------

    /// Helper: a small 3-node graph (tavern ↔ cellar ↔ market_square) used by
    /// several tests. Tavern is indoor; the others outdoor. Tavern↔cellar +
    /// tavern↔market_square are the edges (cellar↔market_square is NOT adjacent).
    fn sample_travel_graph() -> TravelGraph {
        TravelGraph {
            nodes: vec![
                Node {
                    id: "tavern".to_string(),
                    name: "The Rusty Anchor".to_string(),
                    neighbors: vec!["cellar".to_string(), "market_square".to_string()],
                    setting: "indoor".to_string(),
                },
                Node {
                    id: "cellar".to_string(),
                    name: "The Cellar".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: "outdoor".to_string(),
                },
                Node {
                    id: "market_square".to_string(),
                    name: "Market Square".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: "".to_string(),
                },
            ],
            current_node: Some("tavern".to_string()),
        }
    }

    #[test]
    fn travel_graph_default_is_dormant() {
        // Fresh schema: no nodes → dormant (mirrors world_clock / weather).
        let schema = WorldSchema::default();
        assert!(!schema.travel_graph.is_set());
        assert_eq!(schema.travel_graph.render_line(), None);
        assert_eq!(schema.travel_graph.current(), None);
        assert!(!schema.travel_graph.current_is_indoor());
    }

    #[test]
    fn travel_graph_is_set_when_nodes_exist() {
        let mut g = TravelGraph::default();
        g.nodes.push(Node {
            id: "lonely".to_string(),
            name: "A Lonely Place".to_string(),
            neighbors: vec![],
            setting: "".to_string(),
        });
        // is_set is about the GRAPH existing, not the current_node pointer.
        assert!(g.is_set());
        // But render_line is None when there's no current_node.
        assert_eq!(g.render_line(), None);
    }

    #[test]
    fn travel_graph_find_node_returns_node_for_known_id() {
        let g = sample_travel_graph();
        assert_eq!(g.find_node("cellar").map(|n| n.name.as_str()), Some("The Cellar"));
    }

    #[test]
    fn travel_graph_find_node_returns_none_for_unknown() {
        let g = sample_travel_graph();
        assert!(g.find_node("nonexistent").is_none());
    }

    #[test]
    fn travel_graph_current_returns_node_when_set() {
        let g = sample_travel_graph();
        assert_eq!(g.current().map(|n| n.id.as_str()), Some("tavern"));
    }

    #[test]
    fn travel_graph_current_returns_none_when_unset() {
        let mut g = sample_travel_graph();
        g.current_node = None;
        assert_eq!(g.current(), None);
        assert_eq!(g.render_line(), None);
    }

    #[test]
    fn travel_graph_current_returns_none_for_dangling_pointer() {
        // Defensive: current_node points at a missing node (should never happen
        // given the collocation invariant, but the helper must not panic).
        let mut g = sample_travel_graph();
        g.current_node = Some("deleted_node".to_string());
        assert_eq!(g.current(), None);
        assert_eq!(g.render_line(), None);
        assert!(!g.current_is_indoor());
    }

    #[test]
    fn travel_graph_is_adjacent_to_current_true_for_neighbor() {
        let g = sample_travel_graph();
        // tavern's neighbors: cellar, market_square.
        assert!(g.is_adjacent_to_current("cellar"));
        assert!(g.is_adjacent_to_current("market_square"));
    }

    #[test]
    fn travel_graph_is_adjacent_to_current_false_for_non_neighbor() {
        let g = sample_travel_graph();
        // cellar is in the graph but NOT adjacent to tavern (one-way edge).
        // Wait — sample graph has cellar→tavern; tavern's neighbors are
        // cellar + market_square, so cellar IS adjacent to tavern. Use a node
        // that exists but isn't in tavern's neighbor list.
        // Actually all nodes ARE adjacent from tavern in this sample. Build a
        // case where the destination exists but isn't a neighbor:
        let mut g = sample_travel_graph();
        g.nodes.push(Node {
            id: "distant".to_string(),
            name: "Far Away".to_string(),
            neighbors: vec!["market_square".to_string()],
            setting: "".to_string(),
        });
        // "distant" exists in graph but is not in tavern's neighbor list.
        assert!(!g.is_adjacent_to_current("distant"));
    }

    #[test]
    fn travel_graph_is_adjacent_to_current_false_for_unknown() {
        let g = sample_travel_graph();
        assert!(!g.is_adjacent_to_current("nonexistent"));
    }

    #[test]
    fn travel_graph_is_adjacent_to_current_false_when_no_current() {
        let mut g = sample_travel_graph();
        g.current_node = None;
        assert!(!g.is_adjacent_to_current("cellar"));
    }

    #[test]
    fn travel_graph_render_line_shows_current_and_exits() {
        let g = sample_travel_graph();
        let rendered = g.render_line().expect("current is set");
        // Format: "<name> [<id>] (exits: <comma-joined neighbor names>)".
        assert!(rendered.contains("The Rusty Anchor"));
        assert!(rendered.contains("[tavern]"));
        // Exits resolve to neighbor names ("The Cellar", "Market Square").
        assert!(rendered.contains("The Cellar"));
        assert!(rendered.contains("Market Square"));
        assert!(rendered.starts_with("The Rusty Anchor [tavern] (exits: "));
    }

    #[test]
    fn travel_graph_render_line_shows_none_for_no_exits() {
        let g = TravelGraph {
            nodes: vec![Node {
                id: "island".to_string(),
                name: "Deserted Island".to_string(),
                neighbors: vec![],
                setting: "".to_string(),
            }],
            current_node: Some("island".to_string()),
        };
        let rendered = g.render_line().expect("current is set");
        assert!(rendered.contains("(exits: none)"));
    }

    #[test]
    fn travel_graph_render_line_falls_back_to_id_for_unknown_neighbor() {
        // A node lists a neighbor id that doesn't exist in the graph
        // (defensive — should never happen, but render must not panic).
        let g = TravelGraph {
            nodes: vec![Node {
                id: "tavern".to_string(),
                name: "Tavern".to_string(),
                neighbors: vec!["ghost_node".to_string()],
                setting: "".to_string(),
            }],
            current_node: Some("tavern".to_string()),
        };
        let rendered = g.render_line().expect("current is set");
        // The unknown neighbor id falls back to the bare id.
        assert!(rendered.contains("ghost_node"));
    }

    #[test]
    fn travel_graph_current_is_indoor_true_for_indoor_setting() {
        let g = sample_travel_graph();
        // tavern.setting = "indoor".
        assert!(g.current_is_indoor());
    }

    #[test]
    fn travel_graph_current_is_indoor_false_for_outdoor() {
        let mut g = sample_travel_graph();
        g.current_node = Some("cellar".to_string()); // cellar.setting = "outdoor"
        assert!(!g.current_is_indoor());
    }

    #[test]
    fn travel_graph_current_is_indoor_false_for_empty_setting() {
        let mut g = sample_travel_graph();
        g.current_node = Some("market_square".to_string()); // setting = ""
        assert!(!g.current_is_indoor());
    }

    #[test]
    fn travel_graph_current_is_indoor_case_insensitive() {
        let g = TravelGraph {
            nodes: vec![Node {
                id: "hall".to_string(),
                name: "Great Hall".to_string(),
                neighbors: vec![],
                setting: "INDOOR".to_string(),
            }],
            current_node: Some("hall".to_string()),
        };
        assert!(g.current_is_indoor());
    }

    #[test]
    fn render_for_prompt_omits_location_when_dormant() {
        // Fresh game (no nodes) → no location line, zero tokens.
        let schema = WorldSchema::default();
        assert!(!schema.render_for_prompt().contains("location:"));
    }

    #[test]
    fn render_for_prompt_omits_location_when_no_current_node() {
        // Graph exists but current_node is None → no location line.
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph();
        schema.travel_graph.current_node = None;
        assert!(!schema.render_for_prompt().contains("location:"));
    }

    #[test]
    fn render_for_prompt_includes_location_when_set() {
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph();
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("location: The Rusty Anchor [tavern]"));
        assert!(rendered.contains("(exits:"));
    }

    #[test]
    fn render_for_prompt_suppresses_weather_for_indoor_node() {
        // Component 3 coupling: indoor current node → no weather line.
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph(); // current = tavern (indoor)
        schema.weather = Weather {
            condition: "heavy rain".to_string(),
            started_at_minutes: 1000,
        };
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("location:"));
        // Weather suppressed because the player is indoors.
        assert!(!rendered.contains("weather:"));
    }

    #[test]
    fn render_for_prompt_keeps_weather_for_outdoor_node() {
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph();
        schema.travel_graph.current_node = Some("cellar".to_string()); // outdoor
        schema.weather = Weather {
            condition: "heavy rain".to_string(),
            started_at_minutes: 1000,
        };
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("weather: heavy rain"));
        assert!(rendered.contains("location: The Cellar"));
    }

    #[test]
    fn render_for_prompt_keeps_weather_for_empty_setting_node() {
        // Back-compat: a node with empty setting (no indoor/outdoor flag) →
        // weather renders as before (the default behavior).
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph();
        schema.travel_graph.current_node = Some("market_square".to_string()); // setting = ""
        schema.weather = Weather {
            condition: "clear".to_string(),
            started_at_minutes: 0,
        };
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("weather: clear"));
    }

    #[test]
    fn render_for_prompt_keeps_weather_when_graph_unset() {
        // No graph at all → weather renders (no indoor gate possible).
        let mut schema = WorldSchema::default();
        schema.weather = Weather {
            condition: "fog".to_string(),
            started_at_minutes: 0,
        };
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("weather: fog"));
        assert!(!rendered.contains("location:"));
    }

    #[test]
    fn apply_delta_does_not_touch_travel_graph() {
        // Architectural invariant: travel_graph is outside the LLM delta path
        // (mirrors apply_delta_does_not_touch_world_clock / _weather). A delta
        // carrying "travel_graph" in its entities map must NOT mutate the typed
        // field — it just becomes a regular entity key.
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph();
        let original = schema.travel_graph.clone();
        let mut ents = HashMap::new();
        ents.insert("travel_graph".to_string(), Some("injected".to_string()));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
        };
        schema.apply_delta(delta);
        // The typed travel_graph is unchanged.
        assert_eq!(schema.travel_graph, original);
        // The "travel_graph" string landed in the entities map (legacy).
        assert_eq!(
            schema.entities.get("travel_graph").map(|s| s.as_str()),
            Some("injected")
        );
    }

    #[test]
    fn travel_graph_backwards_compat_pre_component3_save_loads_as_empty() {
        // A pre-Component-3 save JSON (no "travel_graph" field) must
        // deserialize to TravelGraph::default() (empty graph, dormant).
        let pre_comp3_json = r#"{
            "summary": "",
            "recent_events": [],
            "entities": {},
            "player_state": {},
            "world_clock": {"current_minutes": 0, "last_tick_minutes": 0},
            "weather": {"condition": "", "started_at_minutes": 0},
            "immutable_keys": [],
            "scene_pacing": {"mode": "Exploration", "spatial": 0, "emotional": 0, "kinetic": 0},
            "status_tags": [],
            "relationships": {},
            "offscreen_tasks": []
        }"#;
        let parsed: WorldSchema = serde_json::from_str(pre_comp3_json)
            .expect("pre-Component-3 JSON must deserialize");
        assert!(!parsed.travel_graph.is_set());
        assert!(parsed.travel_graph.nodes.is_empty());
        assert_eq!(parsed.travel_graph.current_node, None);
    }

    #[test]
    fn travel_graph_serialize_roundtrip() {
        let mut schema = WorldSchema::default();
        schema.travel_graph = sample_travel_graph();
        let dir = std::env::temp_dir();
        let path = dir.join("wupi_schema_travel_graph_test.json");
        let _ = std::fs::remove_file(&path);
        schema.save(&path).unwrap();
        let loaded = WorldSchema::load(&path).unwrap();
        // The graph survives the roundtrip intact (nodes + current_node).
        assert_eq!(loaded.travel_graph.nodes.len(), 3);
        assert_eq!(loaded.travel_graph.current_node.as_deref(), Some("tavern"));
        assert_eq!(
            loaded.travel_graph.find_node("tavern").map(|n| n.name.as_str()),
            Some("The Rusty Anchor")
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---------- immutable_keys (the [CORE]-style lock) ----------

    #[test]
    fn immutable_keys_default_empty() {
        // Fresh schema has no locked keys — backwards-compatible with
        // existing saves (which have no immutable_keys field).
        let schema = WorldSchema::default();
        assert!(schema.immutable_keys.is_empty());
    }

    #[test]
    fn immutable_keys_serialize_roundtrip() {
        // Save/load must preserve the set.
        let mut schema = WorldSchema::default();
        schema.immutable_keys.insert("npc.marcus.core".to_string());
        schema.immutable_keys.insert("loc.tavern.canon".to_string());
        let dir = std::env::temp_dir();
        let path = dir.join("wupi_schema_immutable_test.json");
        let _ = std::fs::remove_file(&path);
        schema.save(&path).unwrap();
        let loaded = WorldSchema::load(&path).unwrap();
        assert_eq!(loaded.immutable_keys, schema.immutable_keys);
        let _ = std::fs::remove_file(&path);
    }
}
