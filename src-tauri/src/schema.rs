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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

// Pull the player-state types in at the top of schema.rs so the new
// `player_state` field + render can reference them unqualified. Sibling
// module (declared in lib.rs); the structs themselves are pure data.
use crate::consequence::StatusTag;
use crate::equipment; // (2026-08-18) NpcInterior.items: the shared typed item rack.
use crate::offscreen_task::OffScreenTask;
use crate::player_state::PlayerState;
use crate::relationship::{RelationshipState, RelationshipTier};
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
    /// human-readable ("Day 3, 2:30 PM") so the narrator can emit coherent
    /// `[TIME ...]` progressions: it sees the current time, advances it by
    /// the scene's elapsed time, emits the new value.
    ///
    /// (2026-08-21 AM/PM fix) The time-of-day renders as 12-HOUR + meridiem,
    /// never bare 24-hour: the Liam playtest had GLM read `clock: 10:00` as
    /// 10 PM in an evening-coded scene and run "4 more hours to 2 AM"
    /// arithmetic off the wrong half of the day. The meridiem makes the
    /// half-of-day a visible fact. The `[TIME]` bracket grammar stays
    /// 24-hour (its teaching line says so) and `parse_in_world_time` accepts
    /// BOTH forms, so emissions parse regardless of which the model mirrors.
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
        let (h12, meridiem) = to_12h(rem / 60);
        let m = rem % 60;
        Some(format!("Day {day}, {h12:02}:{m:02} {meridiem}"))
    }

    /// Render the time-of-day ONLY ("2:30 PM"), suppressing the "Day N"
    /// prefix. Used when a rich calendar label (`WorldSchema.calendar`) is
    /// set: the day/date is carried by the `date:` line, so the `clock:` line
    /// shows just the time-of-day to avoid a redundant day counter. Returns
    /// `None` when the clock is unset. 12-hour + meridiem for the same
    /// 2026-08-21 half-of-day-ambiguity reason as `render_clock_line`.
    pub fn render_time_of_day(&self) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let rem = self.current_minutes % 1440;
        let (h12, meridiem) = to_12h(rem / 60);
        let m = rem % 60;
        Some(format!("{h12:02}:{m:02} {meridiem}"))
    }
}

/// 24-hour → (12-hour, "AM"|"PM"). Hour 0 → 12 AM, hour 12 → 12 PM.
fn to_12h(h24: i64) -> (i64, &'static str) {
    match h24 {
        0 => (12, "AM"),
        1..=11 => (h24, "AM"),
        12 => (12, "PM"),
        _ => (h24 - 12, "PM"),
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

    /// (2026-08-19 Hidden site maps) Un-germinated site hooks for this node —
    /// short one-line premises the World Progression tick's Stale Roulette
    /// pass emits (`site_seeds` on the delta) and the JIT Architect folds
    /// into the map it generates on arrival. Capped at
    /// `site_map::NODE_SEEDS_MAX` (FIFO). Rust-owned: only the tick apply +
    /// the architect's seed-consume write here.
    #[serde(default)]
    pub seeds: Vec<String>,

    /// (2026-08-19 Stale Roulette) The clock minute this node was last
    /// designated to the world-progression pass (stamped on EVERY designated
    /// site, seeds or not — stamping all is what guarantees rotation).
    /// 0 = never designated → sorts FIRST in `select_stale_sites`.
    #[serde(default)]
    pub last_evolved_minutes: i64,

    /// (2026-08-23 starvation fix) The clock minute something MATERIAL last
    /// happened here — a seed actually planted by the tick. The designation
    /// watermark above rotates on every offer; this one only moves on change,
    /// so the designated-site line can show BOTH ("last touched 1d ago, last
    /// material change 30d ago") — the model no longer reads a fresh
    /// designation stamp as "nothing has had time to happen." 0 = never
    /// materialized. Rust-owned: only the tick's seed-plant stamp writes it.
    #[serde(default)]
    pub last_material_minutes: i64,

    /// (2026-08-20 Economy) The node's prosperity percent (25–200, 100 =
    /// normal). Single-source on the Node: property revenue scales ∝ it,
    /// the lifestyle cost curve is its inverse. Rust-owned — only the
    /// `[LEDGER prosperity]` applier writes it (clamped there) + the
    /// `load_split` post-migration clamp. A bare `#[serde(default)]` would
    /// zero old saves, hence the `default_prosperity` fn.
    #[serde(default = "default_prosperity")]
    pub prosperity: u8,

    /// (2026-08-22 multihog WS3) Pending site pressure — accumulated
    /// directional intent for this site's next consuming pass. The
    /// world-progression tick EMITS it (`site_pressure` on the delta); the
    /// architect's germination, an applied evolution op, or a planted seed
    /// CONSUMES it; a no-op pass RETAINS it (the anti-starvation rule:
    /// frequent short ticks accumulate intent instead of resetting it).
    /// Cap 3 FIFO, ≤140 chars/line (the seed-hook discipline). Dormant
    /// when empty (byte-identical pre-WS3 saves).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_pressure: Vec<String>,

    /// (2026-08-24 stall fix) Full architect ROUND failures at this node
    /// (initial decode + both repair passes exhausted). The v0.30.0 live-test
    /// shape: a map the model can't fit makes the write-once idempotence
    /// gate re-run the whole 3-decode cycle EVERY turn. At
    /// `site_map::ARCHITECT_FAIL_STANDDOWN` both architects stand down — the
    /// node stays deliberately map-less. ONE counter per node guards BOTH
    /// architects for that settlement (the hosted interior shares its parent
    /// node's counter; documented tradeoff). Never reset in play — a
    /// stood-down node is a stable state, not a retry loop. Rust-owned: only
    /// the two architects' decode-failure paths write here. 0 = never failed.
    #[serde(default)]
    pub architect_fail_rounds: u8,
}

/// The serde default for [`Node::prosperity`] — see that field (a plain
/// `#[serde(default)]` would deserialize missing prosperity as 0, wrecking
/// the revenue/lifestyle curves for every pre-economy save).
fn default_prosperity() -> u8 {
    crate::economy::PROSPERITY_DEFAULT
}

/// Manual `Default` so `Node { ..Default::default() }` construction sites
/// (node minting, seeds, tests) get the 100-percent default instead of a
/// raw `u8` zero — 0 is outside the legal [25, 200] band.
impl Default for Node {
    fn default() -> Self {
        Node {
            id: String::default(),
            name: String::default(),
            neighbors: Vec::default(),
            setting: String::default(),
            seeds: Vec::default(),
            last_evolved_minutes: 0,
            last_material_minutes: 0,
            prosperity: crate::economy::PROSPERITY_DEFAULT,
            pending_pressure: Vec::default(),
            architect_fail_rounds: 0,
        }
    }
}

/// The spatial travel graph (Rust-authoritative — same line as `world_clock` /
/// `weather`). Nodes + a single `current_node` pointer. v1 is structural
/// geography: no NPC positions, no tick-resolved traversal mechanics, no
/// per-node weather. Writers to `current_node`: the `[TRAVEL]` bracket (which
/// also auto-links edges between known non-adjacent nodes, 2026-08-10) + the
/// first-`[DISCOVER]`-on-empty-graph seed. Writers to `nodes`: the card's
/// `<locations>` block (seeded once at `fable_start`), `[DISCOVER]` (dynamic
/// growth), `[TRAVEL]` auto-link (bidirectional edge formation), + the
/// derive-from-intro bootstrap (`enter_fable_session`).
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

/// Normalize a raw diegetic location name to a node-id slug: lowercase +
/// whitespace runs → single `_`, other non-alphanumerics dropped. Used by
/// `TravelGraph::resolve_node_id` to fuzzy-match `[TRAVEL Market Square]` →
/// `market_square`. Keeps `_` and `-` (valid id chars); everything else that
/// isn't alphanumeric collapses into the underscore stream.
// (2026-08-20 audit) pub(crate): the authored-property seed normalizes its
// node ids through this SAME slug so an authored "Iron Forge" lands on
// `iron_forge` — the id a later [TRAVEL]/[DISCOVER] mint of that name
// produces — instead of a verbatim string that never matches the graph
// (till gate + prosperity reads broke forever on the mismatch).
pub(crate) fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_under = false;
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_under = false;
        } else if c == '_' || c == '-' {
            if !prev_under && !out.is_empty() {
                out.push('_');
                prev_under = true;
            }
        } else if !prev_under && !out.is_empty() {
            // Any other char (space, apostrophe, punctuation) → underscore.
            out.push('_');
            prev_under = true;
        }
    }
    out.trim_end_matches('_').to_string()
}

/// (2026-08-24 fix) Reject garbage identifiers at the id chokepoints —
/// tokens that must NEVER become node/NPC ids. The v0.30.0 live test
/// caught JS-leakage sentinels (`undefined`) + bare numerals (`"1"`)
/// minting as real world entities. Garbage is:
/// - trimmed-empty;
/// - containing NO alphabetic characters at all (subsumes pure digits
///   like `"1"`/`"42"` and punctuation soup like `"!!!"` — ids are names);
/// - the JS/JSON null-family sentinels, case-insensitive:
///   `undefined` / `null` / `none` / `nan` / `true` / `false`.
/// Alphabetic uses `char::is_alphabetic` (Unicode-aware — CJK + accented
/// names are real names, not garbage). Pure fn; unit-tested. Wired at the
/// five id chokepoints (`resolve_or_mint_node`, `[DISCOVER]` node id +
/// neighbors, `[NPC_REGISTER]` id, `auto_register_presence_stub`).
pub fn is_garbage_identifier(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return true;
    }
    if !t.chars().any(|c| c.is_alphabetic()) {
        return true;
    }
    matches!(
        t.to_ascii_lowercase().as_str(),
        "undefined" | "null" | "none" | "nan" | "true" | "false"
    )
}

/// (2026-08-26 Chloe ruling) Location display names NEVER carry parentheses
/// or authored meta-qualifiers — "Earth (variable by scene)" is draft-prompt
/// prose a wizard/ST-import dragged into the `<location>` sibling, not a
/// place. Pure fn: strips every parenthetical run (balanced pairs, nested
/// runs, and a dangling opener/closer a hand-edit could leave), collapses
/// whitespace runs, trims. A name that cleans to nothing returns "" and the
/// CALLER keeps the original (a label must never blank out).
///
/// Applied at every site a location NAME is born — the `<location>` seed at
/// game start, the `[TRAVEL]` mint, the intro-bootstrap anchor — plus a
/// one-time normalize over stored node names in `load_split` so legacy
/// saves heal on their next load. Node IDS are never touched (the graph's
/// keys + edges stay stable; only the diegetic label cleans), and the
/// stored-name normalize re-runs harmlessly (already-clean names are
/// byte-identical after the pass).
pub fn clean_location_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth: usize = 0;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // A closed run (or a stray closer) reads as a word break
                    // so "Earth (x) Village" → "Earth Village", not
                    // "EarthVillage".
                    out.push(' ');
                }
            }
            _ if depth > 0 => {}
            _ => {
                if ch.is_whitespace() {
                    out.push(' ');
                } else {
                    out.push(ch);
                }
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
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

    /// (2026-08-17 rec-2 follow-up) Unique word-containment fragment alias,
    /// shared by the TRAVEL resolve chain AND the `[DISCOVER]` ghost guard:
    /// the emitted fragment's words must be a STRICT word-subset of exactly
    /// ONE node's id/name words ("market" → "market-square" — Levenshtein
    /// ≈0.46, under the typo guard, so without this arm it minted a ghost
    /// twin). Longest fragment word must be ≥4 chars (stops "the" noise);
    /// ambiguity (2+ hits) declines — no silent teleport guess.
    pub fn resolve_fragment_alias(&self, fragment: &str) -> Option<String> {
        let frag_words = word_list(fragment);
        if !fragment_is_specific(&frag_words) {
            return None;
        }
        let mut hits: Vec<&Node> = self
            .nodes
            .iter()
            .filter(|n| {
                let mut node_words = word_list(&n.id);
                node_words.extend(word_list(&n.name));
                node_words.len() > frag_words.len()
                    && frag_words.iter().all(|w| node_words.contains(w))
            })
            .collect();
        hits.dedup_by(|a, b| a.id == b.id);
        if hits.len() == 1 {
            Some(hits[0].id.clone())
        } else {
            None
        }
    }

    /// Resolve a raw `[TRAVEL]` destination to a canonical node id, tolerating
    /// the model's tendency to emit diegetic names instead of bare slugs
    /// (2026-08-10, T52 Open Issue #1: the model wrote `[TRAVEL Market Square]`
    /// but the node id is `market_square` → the strict `find_node` rejected it
    /// → location never advanced across all 52 turns). Tries, in order:
    ///   1. Exact id match (`find_node`) — the common case, zero cost.
    ///   2. Normalized slug match: lowercase + spaces→`_` + trim → compare
    ///      against every node id ("Market Square" → "market_square").
    ///   3. Diegetic name match: case-insensitive compare against `node.name`
    ///      ("The Rusty Anchor" → the `rusty_anchor` node).
    /// Returns the matched node's canonical id, or `None` if nothing fits (the
    /// caller then emits the reject directive listing known nodes). Cheap: O(n)
    /// over a small graph, only reached when the exact match misses.
    pub fn resolve_node_id(&self, raw: &str) -> Option<String> {
        // 1. Exact id match (the fast path).
        if self.find_node(raw).is_some() {
            return Some(raw.to_string());
        }
        // 2. Normalized slug match: "Market Square" → "market_square".
        let normalized = slugify(raw);
        if normalized != raw && self.find_node(&normalized).is_some() {
            return Some(normalized);
        }
        // 3. Diegetic name match (case-insensitive): compare the raw input
        // against each node's `name` field. Also try the normalized form.
        let raw_lower = raw.to_lowercase();
        for n in &self.nodes {
            if !n.name.is_empty() {
                let name_lower = n.name.to_lowercase();
                if name_lower == raw_lower || name_lower == normalized {
                    return Some(n.id.clone());
                }
            }
        }
        None
    }

    /// Best normalized-Levenshtein match of the raw + slug forms against
    /// every node's id + slugified name — the shared engine of the
    /// [`resolve_or_mint_node`] typo guard and the [`resolve_existing_node`]
    /// twin guard (2026-08-20 audit P2-1 extracted it so both arms compare
    /// identically).
    fn best_similarity_match(&self, raw: &str, slug: &str) -> Option<(f32, String)> {
        let mut best: Option<(f32, String)> = None;
        for n in &self.nodes {
            let slug_name = slugify(&n.name);
            for (a, b) in [
                (raw, n.id.as_str()),
                (raw, n.name.as_str()),
                (slug, n.id.as_str()),
                (slug, slug_name.as_str()),
            ] {
                if b.is_empty() {
                    continue;
                }
                let s = similarity(a, b);
                if best.as_ref().map(|(bs, _)| s > *bs).unwrap_or(true) {
                    best = Some((s, n.id.clone()));
                }
            }
        }
        best
    }

    /// (2026-08-20 audit P2-1) The FULL non-minting resolution chain, shared
    /// by [TRAVEL] (pre-mint) and now [DISCOVER] (pre-upsert):
    /// [`resolve_node_id`] (exact / normalized slug / diegetic name), then
    /// the ≥0.75 typo guard, then the strict fragment alias. A discovery
    /// whose emitted surface matches an existing node by ANY travel-
    /// resolution arm ("market square" / "market_square" against the
    /// authored kebab id `market-square` — equal word counts sit UNDER the
    /// fragment alias's strict-subset test) is the documented re-discovery
    /// no-op, never a ghost twin.
    pub fn resolve_existing_node(&self, raw: &str) -> Option<String> {
        let raw_trimmed = raw.trim();
        if raw_trimmed.is_empty() {
            return None;
        }
        if let Some(id) = self.resolve_node_id(raw_trimmed) {
            return Some(id);
        }
        let slug = slugify(raw_trimmed);
        if slug.is_empty() {
            return None;
        }
        if let Some((score, id)) = self
            .best_similarity_match(raw_trimmed, &slug)
            .filter(|(s, _)| *s >= 0.75)
        {
            tracing::info!(
                emitted = %raw_trimmed,
                resolved = %id,
                similarity = score,
                "twin guard: near-match resolved to the existing node"
            );
            return Some(id);
        }
        self.resolve_fragment_alias(&slug)
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
        // (2026-08-16 audit LOW) Bounded exits line — the one unbounded
        // `<world_state>` render: hub nodes accumulate neighbors across a
        // whole campaign, inflating every turn's tracker prompt. Cap the
        // listed exits with a `(+N more)` marker (same shape as the
        // carry-back caps). (2026-08-21 evening follow-up to the 8192
        // ruling: 8 → 12.)
        const EXITS_RENDER_CAP: usize = 12;
        let exits_str = if exits.is_empty() {
            "none".to_string()
        } else if exits.len() > EXITS_RENDER_CAP {
            let extra = exits.len() - EXITS_RENDER_CAP;
            format!(
                "{} (+{extra} more)",
                exits[..EXITS_RENDER_CAP].join(", ")
            )
        } else {
            exits.join(", ")
        };
        Some(format!("{} [{}] (exits: {})", cur.name, cur.id, exits_str))
    }

    /// Dynamic world-seeding: insert a new node if its id isn't already
    /// present, else no-op (idempotent — re-discovering an existing location
    /// is not an error; the tracker may re-emit it). Back-links each named
    /// neighbor: if a neighbor node already exists, add THIS node's id to its
    /// `neighbors` (if not already there) so the graph stays undirected —
    /// matching the rusty_tavern convention where each side lists the other.
    /// A named neighbor that doesn't exist yet keeps its forward edge here;
    /// the reverse edge lands when that node is itself discovered (eventually-
    /// consistent). Returns `true` if a new node was inserted (the caller uses
    /// this to decide whether to take an undo snapshot + set `mutated`).
    pub fn upsert_node(&mut self, node: Node) -> bool {
        if node.id.is_empty() {
            return false;
        }
        if self.find_node(&node.id).is_some() {
            return false;
        }
        // (2026-08-16 audit fix #14) Stored-node cap: a tracker that mints
        // [DISCOVER] nodes every turn grew the graph (rendered into the
        // tracker prompt + folded WHOLE into every schema-engine pass)
        // unboundedly across a long campaign. Refuse NEW nodes at the cap —
        // returning false reads as "duplicate" to the applier (no snapshot,
        // no mutation). Authored cards seed well under it; evicting an
        // authored hub to make room for one more discovered clearing would
        // be worse than refusing it.
        if self.nodes.len() >= MAX_TRAVEL_NODES {
            tracing::warn!(
                len = self.nodes.len(),
                node_id = %node.id,
                "travel-graph node cap reached; refusing new node"
            );
            return false;
        }
        let new_id = node.id.clone();
        let new_neighbors: Vec<String> = node.neighbors.clone();
        self.nodes.push(node);
        // Back-link: for each named neighbor that exists, add the new node to
        // its neighbor list (idempotent — skip if already present). A neighbor
        // that doesn't exist yet is left as a dangling forward edge; it
        // resolves when that node is discovered.
        for n_id in new_neighbors {
            if let Some(n) = self.nodes.iter_mut().find(|n| n.id == n_id) {
                if !n.neighbors.iter().any(|x| x == &new_id) {
                    n.neighbors.push(new_id.clone());
                }
            }
        }
        true
    }

    /// Bidirectionally link two EXISTING nodes (idempotent). Used by the
    /// `[TRAVEL]` auto-link (2026-08-10): when the player travels from A to B
    /// and both are known but not adjacent, the movement itself is evidence the
    /// two locations are connected — form the edge rather than rejecting the
    /// move. Adds each id to the other's `neighbors` if missing. No-op (returns
    /// false) if either id is unknown, identical (`link_nodes(a, a)`), or
    /// already linked in both directions. Does NOT touch `current_node` (the
    /// caller advances it). Mirrors the `upsert_node` back-link body.
    pub fn link_nodes(&mut self, a: &str, b: &str) -> bool {
        if a.is_empty() || b.is_empty() || a == b {
            return false;
        }
        // Both must exist (link_nodes never creates nodes — use upsert_node +
        // then link, or DISCOVER + then TRAVEL).
        if self.find_node(a).is_none() || self.find_node(b).is_none() {
            return false;
        }
        let mut changed = false;
        // a → b
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == a) {
            if !n.neighbors.iter().any(|x| x == b) {
                n.neighbors.push(b.to_string());
                changed = true;
            }
        }
        // b → a (the reverse edge)
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == b) {
            if !n.neighbors.iter().any(|x| x == a) {
                n.neighbors.push(a.to_string());
                changed = true;
            }
        }
        changed
    }

    /// (2026-08-17 E4B shakedown P1c) Resolve a raw `[TRAVEL]` destination OR
    /// mint it as a new node. The E4B obeys the old unknown-destination
    /// reject directive so well it stops traveling — 0 `[DISCOVER]`
    /// emissions in the 51-turn playtest, and the T49–50 departure via the
    /// King's Road never moved `loc` off `market-square`. Walking somewhere
    /// unmapped IS the evidence the place exists (the same principle as the
    /// known-but-non-adjacent auto-link), so an unresolvable destination
    /// now MINTS a node: slugified id from the emitted name, diegetic name
    /// from the raw text, no neighbors (the caller's auto-link arm wires it
    /// to `current_node` bidirectionally), subject to the same
    /// `MAX_TRAVEL_NODES` cap as `[DISCOVER]`.
    ///
    /// **Typo guard:** before minting, similarity-match the raw + slug forms
    /// against every node's id + name. A ≥0.75 best match RESOLVES to that
    /// node instead of minting (`[TRAVEL mrket square]` → `market-square`,
    /// not a phantom twin). Returns the canonical node id; `None` only when
    /// the raw is empty or the graph is at its cap (the caller then rejects
    /// as before).
    ///
    /// (2026-08-17 recommendation 2) Two further arms before minting:
    /// **Fragment alias** — a shorthand destination whose words are a strict
    /// word-subset of exactly ONE known node ("market" → "market-square";
    /// the Levenshtein similarity for that pair is ≈0.46, under the typo
    /// guard, so it used to MINT a ghost twin) resolves to that node.
    /// **Proper-noun naming** — when a genuinely new node is minted,
    /// `narrative` (the tracker's own window) is scanned for the
    /// capitalized place-phrase the story uses ("greywater" → "Greywater
    /// Village") so the node carries a real diegetic name + a matching id.
    /// Pure graph mechanics — no undo snapshot here (the caller
    /// owns snapshotting discipline).
pub fn resolve_or_mint_node(&mut self, raw: &str, narrative: &[&str]) -> Option<String> {
    let raw_trimmed = raw.trim();
    if raw_trimmed.is_empty() {
        return None;
    }
    // (2026-08-24 fix) Garbage destinations never mint — the caller's
    // reject arm teaches ("not possible" + known locations). Catches
    // `undefined`/`null` sentinels + bare numerals before any resolution.
    if is_garbage_identifier(raw_trimmed) {
        tracing::warn!(
            raw = %raw_trimmed,
            "[TRAVEL] destination rejected as a garbage identifier (sentinel/numeric/no-letter token)"
        );
        return None;
    }
        let slug = slugify(raw_trimmed);
        if slug.is_empty() {
            return None;
        }
        // Typo guard: best similarity ≥0.75 across (raw|slug) × (id|name).
        if let Some((score, id)) = self
            .best_similarity_match(raw_trimmed, &slug)
            .filter(|(s, _)| *s >= 0.75)
        {
            tracing::info!(
                raw = %raw_trimmed,
                resolved = %id,
                similarity = score,
                "[TRAVEL] typo-guard: near-mint resolved to the existing node instead"
            );
            return Some(id);
        }
        // Fragment alias: a strict word-subset of exactly ONE known node.
        if let Some(id) = self.resolve_fragment_alias(&slug) {
            tracing::info!(
                raw = %raw_trimmed,
                resolved = %id,
                "[TRAVEL] fragment alias: shorthand resolved to the existing node"
            );
            return Some(id);
        }
        // Mint. Prefer the narrative's capitalized place-phrase for both the
        // diegetic name AND the id ("greywater" + "Greywater Village" in the
        // window → id "greywater_village"); the model's bare shorthand still
        // re-finds the node later via the fragment-alias arm above.
        let phrase = proper_noun_phrase_for(&slug, narrative);
        let mut name_src = phrase.unwrap_or_else(|| raw_trimmed.to_string());
        // (2026-08-26) Location names never carry parenthetical qualifiers —
        // clean BEFORE the id derivation so a narrative phrase like "the
        // warehouse (docks)" mints id `warehouse`, not `warehouse_docks`.
        let cleaned_src = clean_location_label(&name_src);
        if !cleaned_src.is_empty() {
            name_src = cleaned_src;
        }
        // (2026-08-27 playtest H3) Relational tails never belong to a minted
        // name: the playtest minted node `greymist_through` from the emitted
        // fragment "Greymist through the taproom window" (phrase mining
        // failed → the raw rode through verbatim). Strip trailing
        // function/preposition words BEFORE the id derivation.
        let name_src = trim_trailing_relational_words(&name_src);
        if name_src.trim().is_empty() {
            return None;
        }
        // (2026-08-27 playtest H3) Generic connector-places never mint: the
        // playtest minted node `path` from a capitalized sentence-initial
        // "Path". A name whose EVERY word is a generic way/corridor word is
        // movement, not a destination — reject so the caller's teach-back
        // points at the real places instead.
        if is_generic_place_name(&name_src) {
            tracing::warn!(
                raw = %raw_trimmed,
                "[TRAVEL] destination rejected — a generic way/corridor word, not a place"
            );
            return None;
        }
        let id = slugify(&name_src);
        if id.is_empty() {
            return None;
        }
        // (2026-08-27 playtest H3) The relational trim may have collapsed the
        // name onto an EXISTING node ("greymist through" → "greymist" while
        // greymist exists): resolve, never mint a twin (upsert would also
        // wipe the existing node's neighbors).
        if let Some(existing) = self.resolve_existing_node(&id) {
            tracing::info!(
                raw = %raw_trimmed,
                resolved = %existing,
                "[TRAVEL] post-trim id resolved to an existing node instead of minting"
            );
            return Some(existing);
        }
        let name = if name_src.chars().count() > 80 {
            name_src.chars().take(80).collect()
        } else {
            name_src
        };
        // (2026-08-27 playtest H2) Infer the setting from the diegetic
        // name — the JIT architect requires indoor|settlement|seeds|
        // pressure, and a minted setting:"" node can never generate a
        // site map (the playtest's whole site-map subsystem stayed dead
        // for 30 turns this way).
        let setting = infer_node_setting(&name).to_string();
        let node = Node {
            id: id.clone(),
            name,
            neighbors: Vec::new(),
            setting,
            ..Default::default()
        };
        if self.upsert_node(node) {
            tracing::info!(node_id = %id, "[TRAVEL] minted unknown destination as a new node");
            Some(id)
        } else {
            // Cap reached (upsert_node refuses at MAX_TRAVEL_NODES) or a
            // lost race — either way the caller treats None as reject.
            None
        }
    }
}

/// (2026-08-27 playtest H2) Infer a node's `setting` from its diegetic
/// name. The JIT site architect fires only on `setting=indoor`,
/// `setting=settlement`, seeds, or pending pressure; the playtest world
/// seeded its `<location>` town with NO setting and minted every node
/// with `""` — no interiors, no assets, no hidden truth for an entire
/// campaign. Word-boundary token match (split on non-alphanumerics).
/// Settlement-scale names (district maps) outrank indoor ones ("Harbor
/// Inn" is a building in a port — but a node named for the building is
/// the building; the settlement word wins only when the name IS
/// place-scale). Returns "" (outdoor/unknown) as the no-signal default.
pub fn infer_node_setting(name: &str) -> &'static str {
    const SETTLEMENT_WORDS: &[&str] = &[
        "town", "village", "city", "port", "harbor", "harbour", "docks",
        "dockside", "hamlet", "borough", "quarter", "ward", "settlement",
        "outpost", "market",
    ];
    const INDOOR_WORDS: &[&str] = &[
        "tavern", "inn", "pub", "house", "home", "hall", "shop", "store",
        "temple", "church", "chapel", "lighthouse", "warehouse", "forge",
        "smithy", "bakery", "brewery", "mill", "prison", "jail", "keep",
        "castle", "manor", "tower", "library", "guild", "bank", "stable",
        "barn", "kitchen", "cellar", "cabin", "cottage", "hut", "brothel",
        "bathhouse", "theater", "theatre", "office", "study", "workshop",
        "archive", "observatory", "greenhouse", "boathouse", "mansion",
        "palace", "hospital", "clinic", "school", "academy",
        // (2026-08-29 module E2) café-scale places — the friend-log worlds
        // ran mapless because their named places ("the diner", "a cramped
        // apartment") never matched an indoor word and the architect never
        // fired. Settlement words keep priority.
        "corridor", "hallway", "stairwell", "lobby", "cafe", "café",
        "diner", "bar", "apartment", "dorm", "barracks", "lab", "garage",
    ];
    let lower = name.to_lowercase();
    let tokens: std::collections::HashSet<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.iter().any(|t| SETTLEMENT_WORDS.contains(t)) {
        return "settlement";
    }
    if tokens.iter().any(|t| INDOOR_WORDS.contains(t)) {
        return "indoor";
    }
    ""
}

/// (2026-08-27 playtest H3) Relational/function words stripped from the
/// TAIL of a minted location name — "greymist through" → "greymist".
/// Word-wise (split/trim on non-alphanumerics), case-insensitive, repeats
/// until a non-relational word anchors the tail. Pure.
pub fn trim_trailing_relational_words(name: &str) -> String {
    const RELATIONAL: &[&str] = &[
        "through", "across", "along", "past", "beyond", "via", "toward",
        "towards", "into", "onto", "from", "to", "at", "in", "on", "of",
        "by", "near", "under", "over", "and", "or", "the", "a", "an",
        "then", "than", "as", "up", "down", "off", "out", "inside",
        "outside", "around", "behind", "beside", "between", "against",
    ];
    let mut words: Vec<String> = name
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect();
    while let Some(last) = words.last() {
        if RELATIONAL.contains(&last.to_lowercase().as_str()) {
            words.pop();
        } else {
            break;
        }
    }
    words.join(" ")
}

/// (2026-08-27 playtest H3) Is EVERY word of this name a generic
/// way/corridor word ("path", "road", "the old lane")? Such mints are
/// movement phrasing, not destinations — the playtest minted node `path`
/// from a capitalized sentence-initial "Path". A name with even one
/// distinctive word ("North Road", "Iron Alley") mints normally. Pure.
pub fn is_generic_place_name(name: &str) -> bool {
    const GENERIC: &[&str] = &[
        "the", "a", "an", "path", "road", "street", "way", "trail", "lane",
        "alley", "route", "track", "passage", "hallway", "corridor",
        "stairway", "stairs", "stair", "door", "doorway", "gate",
        "entrance", "exit", "corner", "fork", "junction", "walkway",
        "thoroughfare", "somewhere", "there", "here", "place", "spot",
        "area",
    ];
    let words: Vec<String> = name
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|w| !w.is_empty())
        .collect();
    !words.is_empty()
        && words
            .iter()
            .all(|w| GENERIC.contains(&w.to_lowercase().as_str()))
}

/// Normalized Levenshtein similarity in [0,1] (1 = identical). Chars-based
/// (anti-pattern #6: byte-index math on multi-byte input panics); the
/// classic O(m·n) DP is fine for short location labels on a small graph.
fn similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let dist = levenshtein_chars(&a, &b) as f32;
    1.0 - dist / a.len().max(b.len()) as f32
}

/// Raw char-based Levenshtein distance (the DP core `similarity` normalizes
/// over). Chars, never bytes (anti-pattern #6). Shared by the location
/// similarity pass + the 2026-08-23 near-name resolver.
fn levenshtein_chars(a: &[char], b: &[char]) -> u32 {
    if a.is_empty() || b.is_empty() {
        return (a.len() + b.len()) as u32;
    }
    let mut prev: Vec<u32> = (0..=b.len() as u32).collect();
    let mut cur: Vec<u32> = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i as u32 + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

// ===========================================================================
// (2026-08-23 Playground + [NPC_REGISTER] guard) Near-name resolution — the
// Kira/Kyra/Kiera ghost-twin protection. One shared resolver, used twice:
// the Playground's Registry Management tool (ranked candidates for manual
// merges) AND the live `[NPC_REGISTER]` applier's refusal guard (a new
// registration whose name is a near-miss of a registered NPC is refused
// with a one-line directive instead of minting a duplicate cast member).
// ===========================================================================

/// Collision threshold for names where at least one side is ≥5 normalized
/// chars (Kira ↔ Kiera distance 2 must collide).
pub const NEAR_NAME_DISTANCE_LONG: u32 = 2;
/// Collision threshold when BOTH sides are short (<5 chars) — a single edit
/// on a short name is already suspicious (Kira ↔ Kyra distance 1), two is
/// a different word (Jo ↔ Bran).
pub const NEAR_NAME_DISTANCE_SHORT: u32 = 1;
/// The normalized-char count at which the long threshold applies.
pub const NEAR_NAME_LONG_MIN_CHARS: usize = 5;

/// Normalize a surface for near-name comparison: lowercase, keep only
/// alphanumeric CHARS (punctuation/accents on the same base letter stay —
/// they're part of the name's identity, not noise).
fn normalize_near_name(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// Rank every registered NPC whose name / id / alias is a near-name of
/// `query` — normalized Levenshtein distance ≤
/// [`NEAR_NAME_DISTANCE_LONG`] when either side is ≥
/// [`NEAR_NAME_LONG_MIN_CHARS`] normalized chars, ≤
/// [`NEAR_NAME_DISTANCE_SHORT`] when both are shorter; an exact-normalized
/// match (distance 0) is ALWAYS a candidate. Sorted by distance, then id.
/// Each entry appears once, at its best (smallest) distance. Pure.
pub fn near_name_candidates(query: &str, registry: &NpcRegistry) -> Vec<(String, String, u32)> {
    let q = normalize_near_name(query);
    let qc: Vec<char> = q.chars().collect();
    if qc.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(String, String, u32)> = Vec::new();
    for e in &registry.entries {
        let mut best: Option<u32> = None;
        let surfaces = std::iter::once(e.name.as_str())
            .chain(std::iter::once(e.id.as_str()))
            .chain(e.aliases.iter().map(String::as_str));
        for surface in surfaces {
            let n = normalize_near_name(surface);
            let nc: Vec<char> = n.chars().collect();
            if nc.is_empty() {
                continue;
            }
            let d = levenshtein_chars(&qc, &nc);
            let long = qc.len() >= NEAR_NAME_LONG_MIN_CHARS || nc.len() >= NEAR_NAME_LONG_MIN_CHARS;
            let allowed = if long {
                NEAR_NAME_DISTANCE_LONG
            } else {
                NEAR_NAME_DISTANCE_SHORT
            };
            // Exact-normalized always collides; otherwise the threshold.
            if d == 0 || d <= allowed {
                best = Some(best.map_or(d, |b: u32| b.min(d)));
            }
        }
        if let Some(d) = best {
            let label = if e.name.is_empty() {
                e.id.clone()
            } else {
                e.name.clone()
            };
            out.push((e.id.clone(), label, d));
        }
    }
    out.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    out
}

/// The live `[NPC_REGISTER]` guard's decision core: the single closest
/// near-name collision for a NEW registration, EXCLUDING the incoming entry
/// itself (a re-registration of the same id never self-collides; the
/// applier's dup path already handles that no-op). `None` = a genuinely
/// new name — registration proceeds untouched.
pub fn near_name_collision(
    query: &str,
    registry: &NpcRegistry,
    incoming_id: &str,
) -> Option<(String, String, u32)> {
    near_name_candidates(query, registry)
        .into_iter()
        .find(|(id, _, _)| id != incoming_id)
}

/// (2026-08-17 recommendation 2) Scan the narrative window for a capitalized
/// proper-noun phrase containing one of the emitted fragment's words
/// ("greywater" → the story's "Greywater" / "Greywater Village"). Absorbs
/// following capitalized words (with `of/the/and`-style connectors only when
/// another capitalized word follows), capped at 5 words. A lowercase
/// mid-sentence mention never anchors — the story must itself treat the word
/// as a name. Returns `None` when the narrative never capitalizes the
/// fragment; the mint then falls back to the raw emitted text.
fn proper_noun_phrase(fragment_words: &[String], narrative: &[&str]) -> Option<String> {
    const CONNECTORS: &[&str] = &["of", "the", "and", "de", "du", "al"];
    const MAX_WORDS: usize = 5;
    if fragment_words.is_empty() {
        return None;
    }
    // Nested fn (not a closure): a closure returning a borrow of its own
    // argument can't tie the two lifetimes together (E0373-class inference
    // failure — the `move` hint doesn't fix it); elision on a fn does.
    fn clean(t: &str) -> &str {
        t.trim_matches(|c: char| !c.is_alphanumeric())
    }
    let is_cap = |t: &str| -> bool {
        t.chars()
            .next()
            .is_some_and(|c| c.is_uppercase() && !c.is_numeric())
    };
    for text in narrative {
        let toks: Vec<&str> = text.split_whitespace().collect();
        for (i, tok) in toks.iter().enumerate() {
            let bare = clean(tok);
            if !is_cap(bare) {
                continue;
            }
            let lower = bare.to_lowercase();
            if !fragment_words.iter().any(|w| *w == lower) {
                continue;
            }
            let mut words: Vec<String> = vec![bare.to_string()];
            let mut j = i + 1;
            while words.len() < MAX_WORDS && j < toks.len() {
                let nb = clean(toks[j]);
                // Length ≥2 keeps the pronoun "I"/"A" out of place-names
                // ("Greywater Village. I want…" → "Greywater Village").
                if is_cap(nb) && nb.chars().count() >= 2 {
                    words.push(nb.to_string());
                    j += 1;
                } else if CONNECTORS.contains(&nb.to_lowercase().as_str())
                    && words.len() + 2 <= MAX_WORDS
                {
                    // A connector joins only when a capitalized word follows
                    // ("Hall of the Grey King"), else it ends the phrase.
                    let nb2 = toks.get(j + 1).map(|t| clean(t)).unwrap_or("");
                    if is_cap(nb2) && nb2.chars().count() >= 2 {
                        words.push(nb.to_string());
                        words.push(nb2.to_string());
                        j += 2;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            return Some(words.join(" "));
        }
    }
    None
}

/// (2026-08-17 rec-2 follow-up) Applier-facing wrapper: derive the ≥4-char
/// anchor words from a raw emitted fragment/id, then scan the narrative.
/// Shared by the TRAVEL mint path and the `[DISCOVER]` label fallback.
pub fn proper_noun_phrase_for(fragment: &str, narrative: &[&str]) -> Option<String> {
    let words: Vec<String> = word_list(fragment)
        .into_iter()
        .filter(|w| w.chars().count() >= 4)
        .collect();
    proper_noun_phrase(&words, narrative)
}

/// (2026-08-18 Dedicated-NPC interior state) The NPC prominence axis —
/// Chloe's 3-tier ruling, expressed as two tracked states plus one untracked
/// one. `Core` = authored `<cast>` NPCs (companions, villains, faction
/// leaders): full interior, reaper-immune, registry-pinned forever. `Named` =
/// `[NPC_REGISTER]` discoveries (shopkeepers, recurring quest givers): full
/// interior, archived by the reaper after
/// `settings::NPC_REAP_NAMED_AFTER_DAYS` in-world days without contact —
/// UNLESS `npc_is_reaper_protected` derives importance from live world state
/// at reap time (relationship extremes, pending tasks, held items — the
/// left-behind-family guard). Ambient throwaways (guards, pedestrians,
/// thugs) are the third tier BY ABSENCE — they never register, carry zero
/// state, and the API narrator's prose interiority is their whole existence
/// (the per-ambient local decode was rejected as the wall-clock death the
/// 12B thought-channel already taught us).
///
/// Name discipline: NEVER call this concept "tier" — `npc.<id>.tier` is the
/// COMBAT axis (`select_attacker_tier_from_entities`) and a collision would
/// entangle two unrelated validators. The STORED axis is only the
/// authored-vs-discovered distinction; everything else about "importance" is
/// derived, never stamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NpcProminence {
    Core,
    #[default]
    Named,
}

/// One named NPC in the Rust-authoritative registry (Fable Phase 5A,
/// 2026-07-29). Seeded once from the scenario card's `<cast>` block by
/// `enter_fable_session`; grows only via `[NPC_REGISTER]` thereafter.
/// The registry is the `[PRESENCE]` whitelist the Tracker validates against
/// (unknown id → reject; the anti-hallucination gate that closes the
/// "teleporting NPC" bug — the narrator cannot summon an NPC that isn't on
/// the whitelist because it isn't in the `present:` line).
///
/// `WorldSchema::apply_delta` deliberately does NOT touch the registry — see
/// `apply_delta_does_not_touch_npc_registry`. The ONLY writer is the scenario
/// card seed at game start.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct NpcEntry {
    /// Stable identifier ("mara_the_innkeep"). Bare slug, matches the `<cast>`
    /// `id` attribute. This is the load-bearing field the `[PRESENCE]`
    /// bracket's first token is validated against.
    #[serde(default)]
    pub id: String,

    /// (2026-08-18 Dedicated-NPC interior state) Cognitive prominence — the
    /// reaper axis, deliberately a separate WORD and separate HOME from the
    /// combat `tier` above (`core`/`named` never collide with
    /// minion/soldier/elite/boss/legendary in any parser or entity key).
    /// `core` = authored `<cast>` (the card author put them in the world
    /// deliberately — full interior, reaper-immune, registry-pinned). `named`
    /// = `[NPC_REGISTER]` discoveries (full interior, archived by the reaper
    /// after `settings::NPC_REAP_NAMED_AFTER_DAYS` of no contact, evictable
    /// from the registry once archived). Ambient throwaways never enter the
    /// registry at all — the narrator's prose is their whole existence.
    #[serde(default)]
    pub prominence: NpcProminence,

    /// Diegetic prose label shown to the narrator + the image-gen prompt
    /// composer ("Mara"). The prose label only; personality/appearance prose
    /// lives in the card's own CDATA blocks (authored by the user).
    #[serde(default)]
    pub name: String,

    /// One-line vocation/role hint ("The innkeeper behind the bar"). Optional
    /// flavor; helps the narrator + the image-gen prompt composer.
    #[serde(default)]
    pub role: String,

    /// Optional combat tier label ("soldier" / "elite" / "boss" / ...).
    /// Forward-compat for the §11.30 `select_attacker_tier_from_entities`
    /// heuristic; `None` for non-combat NPCs (civilians, vendors, atmosphere).
    /// Left optional at Phase 5A — the registry's job is the ID whitelist +
    /// name; tier threads in later when the heuristic reads the registry
    /// directly instead of scanning `entities`.
    #[serde(default)]
    pub tier: Option<String>,

    /// Alternate surface forms the narrator may emit ("mara", "innkeep").
    /// The `[PRESENCE]` applier normalizes any alias back to `id` so the
    /// `[PRESENCE mara "..."]` and `[PRESENCE mara_the_innkeep "..."]` forms
    /// both resolve to the same registry entry. Populated from the `<alias>`
    /// children in `<cast>`.
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl NpcEntry {
    /// True if `candidate` matches this entry's `id` or any `alias`
    /// (case-insensitive). The normalization the `[PRESENCE]` applier uses so
    /// the narrator's surface form ("Mara") resolves to the canonical id.
    pub fn matches(&self, candidate: &str) -> bool {
        let c = candidate.trim();
        if c.eq_ignore_ascii_case(&self.id) {
            return true;
        }
        self.aliases.iter().any(|a| c.eq_ignore_ascii_case(a))
    }
}

/// Lowercase word list of an id/name/alias, split on every non-alphanumeric
/// char ("captain-harsk" → ["captain", "harsk"]). Shared by the travel-node
/// + NPC fragment-alias arms (Chloe's recommendation 2, 2026-08-17: the E4B
/// emits shorthand fragments — "market", "harsk" — that must resolve to the
/// full stored id instead of minting ghost twins or tripping the
/// anti-hallucination reject).
fn word_list(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// A fragment is alias-worthy only if it carries real signal: its longest
/// word is ≥4 chars. Blocks "the"-style noise from subset-matching a
/// long compound id ("the" ⊆ "the-crooked-lantern-tavern").
fn fragment_is_specific(words: &[String]) -> bool {
    words
        .iter()
        .map(|w| w.chars().count())
        .max()
        .unwrap_or(0)
        >= 4
}

/// Resolve a `[PRESENCE]` surface form against a registry slice: exact
/// id/alias first, then a UNIQUE whole-word-containment alias pass
/// ("harsk" → "captain-harsk" when exactly one entry's id/name/alias words
/// contain every fragment word). Ambiguous (2+ hits) or unspecific
/// fragments return `None` — the caller's anti-hallucination reject stands
/// (a wrong assert is worse than a rejected one). Free fn so the lib.rs
/// applier can use it over its cloned registry slice without the full
/// `NpcRegistry` borrow.
pub fn resolve_npc_surface<'a>(entries: &'a [NpcEntry], surface: &str) -> Option<&'a NpcEntry> {
    let c = surface.trim();
    if let Some(e) = entries.iter().find(|e| e.matches(c)) {
        return Some(e);
    }
    let words = word_list(c);
    if !fragment_is_specific(&words) {
        return None;
    }
    let mut hits: Vec<&NpcEntry> = entries
        .iter()
        .filter(|e| {
            let mut entry_words = word_list(&e.id);
            entry_words.extend(word_list(&e.name));
            for a in &e.aliases {
                entry_words.extend(word_list(a));
            }
            // Strict subset: every fragment word present AND the entry
            // strictly longer (an equal match was the exact pass above).
            entry_words.len() > words.len() && words.iter().all(|w| entry_words.contains(w))
        })
        .collect();
    hits.dedup_by(|a, b| a.id == b.id);
    if hits.len() == 1 {
        // (2026-08-29 module C3) DEBUG — the combined-bracket split (C2)
        // collapsed the ~11 resolutions/turn to ~1/NPC; what remains is
        // routine resolution, not signal.
        tracing::debug!(
            surface = %c,
            resolved = %hits[0].id,
            "[PRESENCE] fragment alias: shorthand resolved to the existing entry"
        );
        Some(hits[0])
    } else {
        None
    }
}

/// The Rust-authoritative named-NPC registry (Phase 5A, 2026-07-29). Seeded
/// from the scenario card's `<cast>` block; the source of truth for which NPC
/// ids exist (the `[PRESENCE]` whitelist). `Vec` (not a map) — matches the
/// `TravelGraph::nodes` precedent; small casts (dozens of NPCs), O(n) lookup
/// is correct + cheap. Rust is the SOLE authority — `apply_delta` does NOT
/// touch this field (mirrors `travel_graph` / `weather` / `world_clock`).
///
/// Deliberately NOT nested in `TravelGraph`: the registry is the cast
/// (characters), the graph is the geography (places) — collocating muddies
/// both (the §11.47 "topology vs propagation state are conceptually distinct"
/// lesson, applied to cast vs geography).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct NpcRegistry {
    /// The registered NPCs. Seeded once; read-only thereafter.
    #[serde(default)]
    pub entries: Vec<NpcEntry>,
}

impl NpcRegistry {
    /// True once at least one NPC is registered (the dormant contract — a
    /// fresh game with no seeded cast suppresses the registry entirely; the
    /// `[PRESENCE]` applier reject-gates on an empty registry by returning
    /// reject-directives for every bracket, same as `[TRAVEL]` on an empty
    /// graph).
    pub fn is_set(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Find a registry entry by id (exact match). O(n); small casts.
    pub fn find(&self, id: &str) -> Option<&NpcEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Mutable `find` — the registry-editing callers (the Playground god
    /// tools) already resolved the canonical id via `resolve_npc_surface`
    /// on the shared borrow and re-find it under `&mut`.
    pub fn find_mut(&mut self, id: &str) -> Option<&mut NpcEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    /// Resolve a surface form (id OR alias) to the canonical `NpcEntry`.
    /// Case-insensitive. Returns `None` for unknown forms — the caller (the
    /// `[PRESENCE]` applier) treats that as a reject-directive (the
    /// anti-hallucination gate). This is the load-bearing normalization fn.
    /// (2026-08-17 recommendation 2) Falls through to the unique
    /// fragment-alias pass (`resolve_npc_surface`) so shorthand ids
    /// ("harsk" → "captain-harsk") resolve instead of rejecting.
    pub fn resolve(&self, surface: &str) -> Option<&NpcEntry> {
        resolve_npc_surface(&self.entries, surface)
    }

    /// Compact prompt render for the `cast:` line (the registry's roster —
    /// distinct from `present:` which shows only on-camera NPCs).
    /// Returns `None` when dormant (no registered NPCs). Mirrors
    /// `TravelGraph::render_line`. The narrator sees the roster so it
    /// knows which ids are valid `[PRESENCE]` targets; the `present:` line
    /// (rendered from `WorldSchema::presences`) narrows to who's on-camera.
    /// Format: `Mara [mara_the_innkeep], Corin [bard_corin]`.
    ///
    /// (#48) HARD-CAPPED at the first 20 entries + a `(+N more)` marker:
    /// `npc_registry` grows via `[NPC_REGISTER]` with no ceiling, and this
    /// line rides EVERY tracker + narrator prompt — uncapped it re-grew the
    /// overflow the bounded carry-back was built to prevent. (2026-08-21
    /// evening follow-up to the 8192 ruling: 16 → 20.)
    pub fn render_line(&self) -> Option<String> {
        const CAST_PROMPT_CAP: usize = 20;
        if self.entries.is_empty() {
            return None;
        }
        let shown = self.entries.len().min(CAST_PROMPT_CAP);
        let mut parts: Vec<String> = self
            .entries[..shown]
            .iter()
            .map(|e| {
                if e.name.is_empty() {
                    format!("[{}]", e.id)
                } else {
                    format!("{} [{}]", e.name, e.id)
                }
            })
            .collect();
        let hidden = self.entries.len() - shown;
        if hidden > 0 {
            parts.push(format!("(+{hidden} more)"));
        }
        Some(parts.join(", "))
    }

    /// Dynamic world-seeding: insert a new entry if its id isn't already
    /// present, else no-op (idempotent — re-registering an existing NPC is
    /// not an error; the tracker may re-emit it). Returns `true` if a new
    /// entry was inserted (the caller uses this to decide whether to take an
    /// undo snapshot + set `mutated`). The entry's aliases are NOT merged on
    /// a no-op — a re-registration with new aliases is a deliberate no-op to
    /// keep the registry stable (the first registration wins, mirroring the
    /// card-seed's "first writer" semantics).
    pub fn upsert_entry(&mut self, entry: NpcEntry) -> bool {
        if entry.id.is_empty() {
            return false;
        }
        if self.find(&entry.id).is_some() {
            return false;
        }
        // (2026-08-17 recommendation 2) Ghost guard: a shorthand id that
        // fragment-aliases to an EXISTING entry ("harsk" when
        // "captain-harsk" is registered) is a re-registration of that NPC,
        // not a new one — treat as the duplicate no-op instead of minting a
        // ghost twin the cast: roster + presence whitelist then carry
        // forever.
        if let Some(existing) = resolve_npc_surface(&self.entries, &entry.id) {
            tracing::info!(
                emitted = %entry.id,
                resolved = %existing.id,
                "[NPC_REGISTER] shorthand resolves to an existing entry — no ghost twin"
            );
            return false;
        }
        // (2026-08-16 audit fix #14) Registry cap, same growth guard as the
        // travel-graph nodes: a registry bloated by [NPC_REGISTER] spam rides
        // every save + every schema-engine pass. Refuse at the cap (false =
        // the applier's duplicate semantics).
        // (2026-08-16 yellow W3) The const moved to module scope — merge_patch's
        // full-replace arm shares it.
        if self.entries.len() >= MAX_NPC_REGISTRY {
            tracing::warn!(
                len = self.entries.len(),
                npc_id = %entry.id,
                "npc registry cap reached; refusing new entry"
            );
            return false;
        }
        self.entries.push(entry);
        true
    }
}

/// One NPC currently on-camera (Fable Phase 5A, 2026-07-29). The Tracker
/// emits `[PRESENCE npc_id "stance and micro-location"]` per on-camera NPC
/// each turn; the applier resolves the surface form to a canonical id via
/// `NpcRegistry::resolve`, then upserts a `Presence` here. The `present:`
/// render line in `render_for_prompt` is the whitelist the narrator obeys —
/// only NPCs in `presences` may speak, act, or be addressed in the scene.
///
/// The `ttl` field implements the grace window: an NPC not re-asserted by a
/// `[PRESENCE]` this turn has its `ttl` decremented; it drops when `ttl`
/// reaches 0. `PRESENCE_GRACE_RESET` (4 as of 2026-08-10) is the value
/// fresh/re-asserted presences start at, so an NPC survives a multi-turn
/// tracker under-emission streak (the §11.51 failure mode) without vaporizing
/// mid-conversation.
///
/// NO `Default` (mirrors `rumor::Rumor`): a presence is always authored by
/// the Tracker, never default-constructed. NO `node_id` (Option B —
/// presence-implies-location; off-screen NPC positions are the rumor/task
/// engine's job, not this struct's). Clone but NOT Copy (owns Strings).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Presence {
    /// Canonical NPC id (resolved from the bracket's surface form via
    /// `NpcRegistry::resolve`). The key the registry + relationship engine
    /// + off-screen task queue all index by.
    pub npc_id: String,

    /// Diegetic name (copied from the registry entry at upsert time, so the
    /// `present:` line renders a readable label even if the registry were
    /// later cleared — defensive, mirrors how `Rumor::label` is self-contained).
    pub name: String,

    /// Free-text stance + micro-location ("standing by the wooden table, arms
    /// crossed"). The Tracker extracts this from the narrator's prose; the
    /// image-gen prompt composer aggregates it into the "micro (subjects)"
    /// layer of the scene prompt. Run through `truncate_repetition` +
    /// `normalize_whitespace` before storing (the §11.41 / §11.29 prose-cleanup
    /// contract — free-text Tracker output carries the repetition +
    /// verbatim-copy risks).
    pub stance: String,

    /// Turns of grace remaining before this presence drops (see struct doc).
    /// The applier sets this to `GRACE_RESET` on every re-assertion +
    /// decrements on every turn the NPC is NOT re-asserted.
    pub ttl: u32,
}

/// The grace-TTL reset value (Phase 5A, 2026-07-29). Fresh + re-asserted
/// presences start at 4; an NPC survives THREE missed `[PRESENCE]` extractions
/// (4 → 3 → 2 → 1 → drop). Raised from 2 on 2026-08-10 (Issue #3): a 2-value
/// grace (one miss tolerated) was too aggressive for real roleplay — a short
/// conversation easily spans 3-4 turns where the tracker under-emits, and the
/// NPC vaporized mid-scene (the cinderfen playtest saw Mara drop at T3 despite
/// staying on-camera through T10). 4 tolerates a multi-turn tracker under-
/// emission streak without letting a genuinely-departed NPC linger long. The
/// prompt-side fix (the `<per_turn_presence>` maintainer discipline) is the
/// real cure; this TTL is the defensive net beneath it.
pub const PRESENCE_GRACE_RESET: u32 = 4;

/// (2026-08-18 Dedicated-NPC interior state; cap raised 6→12→16 on the
/// 2026-08-21 Chloe 8192-context rulings) Render + growth caps for the
/// per-NPC interior. The `minds:` line is PRESENT-only and HARD-CAPPED like
/// `present:` — a crowded tavern full of schemers must not blow the
/// always-on prompt budget (Prime Mandate: state is unbounded, prompt
/// lines never are). The cap matches the presence cap (16) so every
/// on-camera NPC's interior renders; the selection past 16 stays
/// importance-ranked (core → reaper-protected → ambient) — see the render
/// block in `render_for_prompt`.
pub const NPC_MINDS_PROMPT_CAP: usize = 16;
/// Total char cap for one `minds:` entry (name + body) — the render-side
/// backstop for hand-edited saves that bypass the parse-time caps.
pub const NPC_MINDS_ENTRY_CHAR_CAP: usize = 200;
/// Per-NPC held-item cap (FIFO drain — a hoarder NPC's oldest acquisition
/// drops when the 17th lands; same belt-style discipline).
pub const NPC_INTERIOR_ITEMS_MAX: usize = 16;

/// Cap on an NPC's WORN outfit rack (seeded from the card): an authored
/// outfit is a handful of garments; beyond this the seed clips FIFO (the
/// registry + prose carry the rest).
pub const NPC_WORN_MAX: usize = 10;
/// Char cap for the reaper's compressed archive stub.
pub const NPC_ARCHIVE_STUB_CHAR_CAP: usize = 160;

/// (2026-08-18 Dedicated-NPC interior state) Per-NPC tracked interior: what
/// an NPC HOLDS, FEELS, and is currently ABOUT — the machinery behind NPCs
/// that steal from the player, lie to their face, and nurse grudges. Keyed
/// by canonical registry id (same key space as `relationships`).
///
/// Rust is the SOLE authority — `apply_delta` does NOT touch this field and
/// `merge_patch` has NO arm for it (same structural immunity as
/// `player_state.equipment`, pinned by test). The only writers: the
/// `[NPC_ITEM]` / `[MOOD]` / `[INTENT]` bracket appliers, the `[PRESENCE]`
/// interaction stamp, and the world-tick reaper.
///
/// The intent field is DISTILLED state, never verbatim model thought (§3.4
/// invariant: the model never re-reads its own reasoning) — the tracker
/// deliberately emitted it as a one-line declarative, and the narrator reads
/// it as world state to play the lie/scheme accordingly. Scene-scoped
/// injection: `render_for_prompt` emits a `minds:` line for PRESENT NPCs
/// only; off-screen scheming never leaks and never costs prompt tokens.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct NpcInterior {
    /// Current emotional read ("suspicious", "warming") — a flattened,
    /// parse-time-capped label from `[MOOD]`. `None` = unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,

    /// Distilled one-line plan/suspicion from `[INTENT]` ("get her out
    /// before she checks the display case"). `None` = unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,

    /// Items the NPC holds — stolen goods, received gifts, spent stock.
    /// Typed + merge-on-add via the same `equipment::stack_*` helpers as the
    /// player pack, capped FIFO at `NPC_INTERIOR_ITEMS_MAX`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<equipment::StackItem>,

    /// Items the NPC WEARS — the outfit (clothing + specific jewelry), seeded
    /// from an npc card's `<inventory>` Clothing/Equipped/Accessories lines
    /// through the shared garment router (2026-08-19 zone sweep, Chloe
    /// ruling: npc-card clothing is auto-EQUIPPED and tracked from turn one).
    /// Distinguished from `items` so the `wearing:` render reads as an
    /// outfit, not a shopping bag. Same typed stack discipline; capped at
    /// `NPC_WORN_MAX` at seed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worn: Vec<equipment::StackItem>,

    /// Interaction counter — incremented on every `[PRESENCE]` assert and
    /// interior mutation. Diagnostics/reaper signal.
    #[serde(default)]
    pub interactions: u32,

    /// In-world epoch-minutes of last contact (the reaper key; 0 = unknown —
    /// dormant clock or pre-interior save, which the reaper treats as "do
    /// not measure", never "instantly stale").
    #[serde(default)]
    pub last_seen_minutes: i64,

    /// Compressed archive stub set by the reaper when a `named` NPC passes
    /// the no-contact TTL — mood/intent/items are cleared, this one-line
    /// summary survives so a return visit still recalls the shape of the
    /// relationship. `None` = live. The next `[MOOD]`/`[INTENT]`/`[NPC_ITEM]`
    /// emission overwrites it with fresh live state (the applier treats an
    /// archived interior as fresh ground — no mechanical un-archive needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<String>,
}

/// Char-boundary-safe truncation for prompt-facing free text (counts
/// CHARACTERS, not bytes — the player-field cap ruling). Render-side
/// backstop: parse-time caps already gate the bracket path; this catches
/// hand-edited saves.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

impl NpcInterior {
    /// One `minds:` prompt entry for a PRESENT NPC:
    /// `Mara [suspicious; intends "get her out"; carries Worn Ring, Ale]`.
    /// An archived interior renders its recall stub instead
    /// (`Mara [recalls: suspicious; …]`). Empty string = nothing to show
    /// (caller skips). All free text flattened + capped — a hand-edited save
    /// can't inject a forged prompt line (audit M2 discipline).
    pub fn render_minds_entry(&self, name: &str) -> String {
        let body = if let Some(stub) = self.archived.as_deref().filter(|s| !s.trim().is_empty()) {
            format!("recalls: {}", stub.trim())
        } else {
            let mut segs: Vec<String> = Vec::new();
            if let Some(m) = self.mood.as_deref().filter(|m| !m.trim().is_empty()) {
                segs.push(m.trim().to_string());
            }
            if let Some(i) = self.intent.as_deref().filter(|i| !i.trim().is_empty()) {
                segs.push(format!("intends \"{}\"", i.trim()));
            }
            if !self.items.is_empty() {
                const NAMES_SHOWN: usize = 4;
                let mut names: Vec<String> = self
                    .items
                    .iter()
                    .take(NAMES_SHOWN)
                    .map(|it| it.name.clone())
                    .collect();
                let hidden = self.items.len().saturating_sub(NAMES_SHOWN);
                if hidden > 0 {
                    names.push(format!("+{hidden}"));
                }
                segs.push(format!("carries {}", names.join(", ")));
            }
            segs.join("; ")
        };
        if body.is_empty() {
            return String::new();
        }
        cap_chars(
            &format!("{} [{}]", name.trim(), body),
            NPC_MINDS_ENTRY_CHAR_CAP,
        )
    }

    /// Compose the reaper's compressed stub from the live fields (pre-clear).
    /// Deterministic order: mood; intent; item count. Capped at
    /// `NPC_ARCHIVE_STUB_CHAR_CAP`.
    fn compose_archive_stub(&self) -> String {
        let mut segs: Vec<String> = Vec::new();
        if let Some(m) = self.mood.as_deref().filter(|m| !m.trim().is_empty()) {
            segs.push(m.trim().to_string());
        }
        if let Some(i) = self.intent.as_deref().filter(|i| !i.trim().is_empty()) {
            segs.push(i.trim().to_string());
        }
        if !self.items.is_empty() {
            segs.push(format!("held {} item(s)", self.items.len()));
        }
        if segs.is_empty() {
            segs.push("no recorded state".to_string());
        }
        cap_chars(&segs.join("; "), NPC_ARCHIVE_STUB_CHAR_CAP)
    }
}

/// (2026-08-16 audit LOW) Shared growth caps for the typed referee-owned
/// collections — the bracket appliers (lib.rs) + `merge_patch`'s
/// full-replace defense (`enforce_typed_caps`) agree on one set of numbers.
/// (2026-08-21 evening follow-up to the 8192 ruling: raised across the
/// board — these are STORAGE bounds, and neither prompt path renders them
/// wholesale: the tracker lean render caps its lines, `to_json_prompt`
/// trims to its own char budget, so the raised ceilings only mean longer
/// campaigns stop hitting the walls.)
pub const MAX_TRACKED_RELATIONSHIPS: usize = 64;
pub const MAX_STORED_TASKS: usize = 24;
pub const MAX_STORED_RUMORS: usize = 32;
pub const MAX_TRAVEL_NODES: usize = 128;
/// (2026-08-16 yellow W3) The dynamic-cast registry cap — module scope so
/// `NpcRegistry::upsert_entry` AND `merge_patch`'s full-replace arm share one
/// number (the raw-editor JSON tab installs whole registries; the applier's
/// refuse-at-cap discipline now backstops it). (2026-08-21 evening: 96 →
/// 128 — the reaper still bounds `named` growth; the cast line renders 20.)
pub const MAX_NPC_REGISTRY: usize = 128;
/// (2026-08-19 Referee QoL) Open `[PROMISE]` cap — FIFO like the tasks; a
/// long campaign can't accumulate an unbounded obligations list on the
/// npc.json slice + the `owed:` render.
pub const MAX_PROMISES: usize = 8;
/// (2026-08-22 living-world) Open `[QUEST]` cap — FIFO like the promises;
/// `done`/`fail` remove immediately so 8 concurrent threads is a busy
/// campaign. The `quests:` render caps at 5 shown + `(+N more)` (the
/// worst-case tracker budget is the binding constraint, not storage).
pub const MAX_QUESTS: usize = 8;
/// (2026-08-22 living-world) Objectives per quest — the bracket applier
/// refuses beyond this (a quest with more parts is two quests).
pub const MAX_QUEST_OBJECTIVES: usize = 6;

/// The serde skip-helper for [`WorldSchema::last_rest_minutes`] (0 = the
/// dormant anchor — keeps pre-living-world saves byte-identical).
fn is_zero(v: &i64) -> bool {
    *v == 0
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
            SceneMode::Exploration => "Pace your prose for exploration: balanced past-tense beats, a mix of action and atmosphere. Each turn covers roughly a minute of in-world time.",
            SceneMode::Downtime => "Pace your prose for downtime: linger in past tense on sensory detail, ambient sound, the texture of the place. Each turn can cover an hour or more — let time breathe.",
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

    /// Additive DC modifier for the COMBAT REFEREE'S LETHALITY SAVE, by
    /// mode. LOWER = a killing blow lands easier. Deliberately the OPPOSITE
    /// sign of [`Self::dc_modifier`] (2026-08-15, Chloe's call): mid-combat
    /// is when blows are thrown with intent, so tension slightly raises the
    /// stakes — the old code threaded `dc_modifier()` straight in, which
    /// made Combat the SAFEST place to take a hit (a fresh body literally
    /// could not die to a Soldier in a fight it couldn't lose outside one —
    /// inverted). Magnitude is half the skill mod and the common-case
    /// invariant is preserved: a Soldier still cannot one-shot an Unscathed
    /// player even in Combat (DC 18 + 4 − 1 = 21 > max d20); only real
    /// threats (Elite+) gain menace. Downtime is the safest mode (+2).
    pub fn lethality_dc_mod(self) -> i32 {
        match self {
            SceneMode::Combat => -1,
            SceneMode::Exploration => 0,
            SceneMode::Downtime => 2,
        }
    }

    /// (2026-08-17 E4B shakedown P1d) Max clock advance a single `[TIME]`
    /// bracket may apply, by mode, in MINUTES. The E4B once jumped the clock
    /// +20175 minutes (~14 days) in one turn — the pacing-aware clamp kills
    /// day-scale jumps without judging prose: Downtime 24h (a long rest or
    /// travel leg), Exploration 6h, Combat 1h (a fight is seconds-scale; an
    /// hour is already generous). Overshoot clamps to the cap; the applier
    /// warns the tracker via a next-turn directive.
    pub fn time_clamp_minutes(self) -> i64 {
        match self {
            SceneMode::Combat => 60,
            SceneMode::Exploration => 6 * 60,
            SceneMode::Downtime => 24 * 60,
        }
    }
}

/// (2026-08-17 E4B shakedown P1d) Pure core of the `[TIME]` apply: clamp the
/// requested advance to the pacing-aware cap + derive the next-turn
/// directives (clamp warning; stale-calendar nudge on a midnight crossing
/// with no `[DATE]` the same turn). Returns `(effective_minutes, directives)`.
/// The caller handles the first-set baseline (no clamp on a cold clock) and
/// the regression guard — this fn assumes `requested >= prev`.
pub fn clamp_time_advance(
    prev: i64,
    requested: i64,
    mode: SceneMode,
    calendar_label: Option<&str>,
    date_rode_this_turn: bool,
) -> (i64, Vec<String>) {
    let mut directives = Vec::new();
    let clamp = mode.time_clamp_minutes();
    let effective = if requested > prev.saturating_add(clamp) {
        directives.push(format!(
            "Time advance clamped: this scene's pacing allows at most {clamp} minutes of \
             in-world advance per turn. The clock now reads the clamped time — continue from \
             it in smaller steps; do not repeat the large jump."
        ));
        prev.saturating_add(clamp)
    } else {
        requested
    };
    if calendar_label.is_some() && !date_rode_this_turn && effective / 1440 > prev / 1440 {
        directives.push(
            "The clock crossed a midnight boundary but the `date:` label is now stale — emit \
             [DATE <new full label>] this turn to advance the calendar."
                .to_string(),
        );
    }
    (effective, directives)
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

/// (2026-08-19 Referee QoL) One open player promise: what the player owes
/// WHOM, and by when. Pure data — the frustration math lives in
/// `offscreen_task::promise_frustration` (the volatility-scaled curve).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Promise {
    /// The giver's canonical registry id (resolved by the applier through
    /// `resolve_npc_surface`, same gate as PRESENCE).
    #[serde(default)]
    pub npc_id: String,
    /// The diegetic obligation ("return the horse", "meet me at the well by
    /// dusk"). The remove-form match key (npc_id + description).
    #[serde(default)]
    pub description: String,
    /// The in-world minute the promise falls due (accepted + the emitted
    /// window).
    #[serde(default)]
    pub deadline_minutes: i64,
    /// The in-world minute the promise was accepted (the curve's zero
    /// point; 0 = an un-stamped legacy row).
    #[serde(default)]
    pub accepted_at_minutes: i64,
    /// (2026-08-25 quest anchors) OPTIONAL site-map AREA id the obligation
    /// targets ("meet me at the well" → the courtyard area) — emits as
    /// `area=<area_id>` on the bracket. Drives the location-card map's
    /// scroll marker over the anchored area (knowledge-gated at slice
    /// time). Empty = unanchored; an anchor is a hard tracker-vouched
    /// fact, NEVER inferred from the description text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub area_anchor: String,
}

/// (2026-08-22 living-world) One countable objective inside a quest. The
/// `[cur/total]` counter pair is OPTIONAL (0/0 = a plain done-flag
/// objective); when set, `cur` ≤ `total` and reaching them is completion
/// the tracker can SEE — the Anti-Difficulty Mandate lives here: objectives
/// are concrete and countable, difficulty itself is world context, never a
/// stored number (no difficulty/rank/level field exists on any quest
/// shape).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct QuestObjective {
    /// The objective text, ≤160 chars (the `clean_free_text` discipline at
    /// the parse). The upsert match key under normalization.
    #[serde(default)]
    pub text: String,
    /// Completed flag (`[QUEST update <id> done <text>]`).
    #[serde(default)]
    pub done: bool,
    /// Counter numerator (`3` of "3/6 wolves culled"). 0 = no counter.
    #[serde(default)]
    pub cur: u32,
    /// Counter denominator. 0 = no counter.
    #[serde(default)]
    pub total: u32,
}

/// (2026-08-22 living-world) One open quest. The structural twin of
/// [`Promise`] — what NPCs want FROM you — filling the void for what the
/// PLAYER is trying to accomplish. Giver `"player"` marks a self-imposed
/// emergent goal (exempt from the deadline patience curve — the system
/// never penalizes the player for delaying a personal side goal).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Quest {
    /// Kebab id, unique among open quests (the bracket's match key).
    #[serde(default)]
    pub id: String,
    /// The giver's canonical registry id, or the literal `"player"` for a
    /// self-imposed goal (resolved at the bracket apply through
    /// `resolve_npc_surface`).
    #[serde(default)]
    pub giver: String,
    /// Diegetic title, ≤120 chars.
    #[serde(default)]
    pub title: String,
    /// Countable objectives, ≤[`MAX_QUEST_OBJECTIVES`].
    #[serde(default)]
    pub objectives: Vec<QuestObjective>,
    /// Optional promised reward (free text, ≤160).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reward: String,
    /// The in-world minute the quest falls due (0 = no deadline). Drives
    /// the auto-fail patience curve on the tick for non-player givers.
    #[serde(default)]
    pub deadline_minutes: i64,
    /// The in-world minute the quest was accepted (the curve's zero point).
    #[serde(default)]
    pub accepted_at_minutes: i64,
    /// (2026-08-25 quest anchors) OPTIONAL site-map AREA id the objective
    /// targets ("investigate the cutpurse" → the market-ward area) — emits
    /// as `area=<area_id>` on the new/update brackets. Drives the
    /// location-card map's scroll marker over the anchored area
    /// (knowledge-gated at slice time; `done`/`fail` remove the quest and
    /// the anchor with it). Empty = unanchored; an anchor is a hard
    /// tracker-vouched fact, NEVER inferred from the title text.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub area_anchor: String,
}

/// (2026-08-22 living-world) The giver-patience score for an open quest's
/// deadline — the SAME volatility-scaled curve promises use
/// (`offscreen_task::promise_frustration`), so deadlines are enforced by
/// deterministic Rust clock math, never fuzzy LLM guessing. Returns
/// `f64::NEG_INFINITY` for exempt quests (no deadline, or giver =
/// `"player"` — self-imposed goals are never penalized), so any positive
/// threshold comparison is naturally false for them.
pub fn quest_deadline_frustration(
    quest: &Quest,
    volatility: Option<f64>,
    now_minutes: i64,
) -> f64 {
    if quest.deadline_minutes <= 0 || quest.giver == "player" {
        return f64::NEG_INFINITY;
    }
    crate::offscreen_task::promise_frustration(
        quest.accepted_at_minutes,
        quest.deadline_minutes,
        now_minutes,
        volatility,
    )
}

/// (2026-08-22 living-world) Stamina/mana recovery steps granted by a rest
/// of `hours` — the deterministic curve that makes rest LENGTH matter:
/// under an hour recovers nothing (a breather is the Recovery Referee's
/// one-step domain), 1-4h one step, 4-8h two, a full night's 8h+ everything
/// (4 steps = `Fresh`/`Surging` from the floor).
pub fn rest_recovery_steps(hours: i64) -> usize {
    if hours >= 8 {
        4
    } else if hours >= 4 {
        2
    } else if hours >= 1 {
        1
    } else {
        0
    }
}

/// (2026-08-22 living-world) The fatigue band for a since-last-rest delta.
/// `None` = healthy (no clamp, no band on the `rested:` line); `"weary"`
/// past 16h on your feet; `"exhausted"` past 24h. Pure — the caller owns
/// the clock reads + the mechanical clamp.
pub fn rested_band(delta_minutes: i64) -> Option<&'static str> {
    if delta_minutes <= 16 * 60 {
        None
    } else if delta_minutes <= 24 * 60 {
        Some("weary")
    } else {
        Some("exhausted")
    }
}

/// Normalize objective text into the upsert match key: lowercase +
/// whitespace collapsed (a re-emission with different spacing or casing is
/// the SAME objective, never a twin).
pub(crate) fn normalize_quest_objective_key(text: &str) -> String {
    text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
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
    /// `"char.mira.trust"`, `"loc.current"`). Values are **arbitrary JSON**
    /// (widened 2026-08-11 from `String` to `serde_json::Value`): a bare
    /// string (`"rusty knife"`) for simple facts, or a structured object /
    /// array / number for rich tracking (`{"quest.dragon": {"progress":3,
    /// "target":5}}`). The model decides per-key how deep to go.
    ///
    /// In a delta, a `None` value (JSON `null`) means "delete this key";
    /// `Some(v)` means "set/overwrite." A `Value::Null` payload is treated
    /// as a delete by `apply_delta` (matches the `Option<String>` semantics
    /// the delta has always used).
    #[serde(default)]
    pub entities: BTreeMap<String, serde_json::Value>,

    /// (2026-08-16 audit fix #14) First-insert order of `entities` — the
    /// FIFO eviction key for [`WorldSchema::enforce_entity_cap`]. Entities
    /// accumulated unboundedly in long campaigns (deletion is model-
    /// discretionary), and the whole-schema JSON folded into every delta/
    /// translation/progression prompt blew the 2048-token budget around
    /// turn ~100-200 — the middle-drop then permanently spliced the schema
    /// the model must diff against. Re-upserting an existing key does NOT
    /// refresh its slot (the original insert order stands). Empty for legacy
    /// saves → backfilled deterministically (sorted) on first enforcement.
    #[serde(default, skip_serializing_if = "std::collections::VecDeque::is_empty")]
    pub entity_order: std::collections::VecDeque<String>,

    /// (2026-08-16 deferred-3, Chloe-approved) The 3-file split's GENERATION
    /// stamp — a cross-file commit hash. `save_split` stamps the same
    /// `split_gen` (old + 1) into all three sibling files; `load_split`
    /// refuses a trio whose stamps disagree (a crash between the back-to-back
    /// renames used to leave a mixed-generation trio where every file
    /// INDIVIDUALLY parsed, so the corrupt-file guard couldn't catch the
    /// Frankenstein combination). Referee-owned bookkeeping: `merge_patch`
    /// refuses it like every other typed field, prompts never render it.
    /// 0 for legacy saves (unstamped files load as-is — can't retrofit).
    #[serde(default)]
    pub split_gen: u64,

    /// (2026-08-29 module F1) The SAVE-HEAL generation this schema last
    /// healed through (`heal_schema_state`, gated on `HEAL_VERSION`). Rides
    /// world.json's split root + every save slot (the field lives on the
    /// struct both serialize); 0 = a pre-heal save (every existing save on
    /// first load after this ships). Referee-owned bookkeeping — `merge_patch`
    /// refuses it, prompts never render it.
    #[serde(default)]
    pub heal_version: u32,

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

    /// (2026-08-22 living-world) The rested anchor — the in-world minute
    /// of the player's last genuine sleep/recuperation (`[REST]` bracket
    /// or the Recovery Referee's backstop flag; stamped POST-`[TIME]` so a
    /// night's sleep anchors on the morning after). 0 = dormant: legacy
    /// saves + fresh games render no `rested:` line and clamp nothing
    /// until the first rest establishes the anchor (the first-`[TIME]`
    /// baseline stamps it — fresh campaigns start rested, the economy-
    /// stamp precedent). Rust-owned: no `apply_delta` field, no
    /// `merge_patch` arm. Rides world.json (the save split is
    /// remove-based).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_rest_minutes: i64,

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

    /// Free-form calendar label (2026-08-13): a verbatim date string
    /// ("3rd of Harvest, Year 1247, Market Day") — month/year/type-of-day, NOT
    /// just "Day N". Seeded from the card's `<start><date>` + advanced in play
    /// by the `[DATE]` bracket (the tracker rewrites the label — no Rust
    /// calendar arithmetic). When set, `render_for_prompt` emits `date:` +
    /// renders the clock as time-of-day only (suppressing "Day N"). When
    /// unset (None — pre-2026-08-13 saves), the `clock: Day N, HH:MM`
    /// render is preserved. `#[serde(default)]` keeps old
    /// saves loadable as dormant (None → no `date:` line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<String>,

    /// (2026-08-17 E4B shakedown P1d) The clock minute the `calendar` label
    /// was last synchronized (`[DATE]` apply stamps it). The playtest let the
    /// label sit at "17th of Peatfall…" forever while the clock reached
    /// Day ~17 — no `[DATE]` in 51 turns. Day-crossing `[TIME]` advances push
    /// a next-turn directive asking the tracker to re-emit `[DATE]`; if the
    /// label stays >48h stale anyway, `render_for_prompt` appends the true
    /// `— day N` suffix so the prompt never asserts a date the clock has
    /// passed. `None` = not yet stamped: a legacy save or a card-seeded label
    /// is assumed synced as of the first `[TIME]` apply (the apply path
    /// bootstraps the stamp) — no suffix until staleness is actually observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar_synced_minutes: Option<i64>,

    /// The simulation's tone (2026-08-19 Chloe ruling): seeded from the
    /// card's `<world>` sibling, rendered per-turn as the `tone:` line right
    /// after `weather:` — tone is LIVE world state owned by the tracker,
    /// never static prompt text (the card cache block carries identity
    /// only). `None` = dormant (no line; saves + cards without a tone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,

    /// (2026-08-21 economy addendum) The world's money-unit label — context
    /// text learned in play, NEVER hardcoded (no default "gold": a sci-fi
    /// card mixing "credits" with an assumed "g" is the exact bug this
    /// field kills). Empty = unknown: every money render is the NAKED
    /// base-unit integer (`wealth: 0`, `+8/day`). The tracker sets it via
    /// `[LEDGER currency <label>]` the first time narration names the
    /// world's currency ("dollars", "beli"), or as a 2-3 tier slash spec
    /// (`gold/silver/copper` — highest first, wealth stays the LOWEST unit
    /// and only `economy::format_money` splits it at the render stage:
    /// 1254 → `12g 5s 4c`). Rust-owned like `weather`/`tone`: the
    /// schema-delta path has no arm for it. `String` (not `Option`) per
    /// the addendum — `""` is the dormant default; skip-serializing keeps
    /// pre-addendum saves byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub currency_label: String,

    /// Custom extensions (2026-08-13): a flat key→value string map seeded from
    /// the card's `<custom_tags>` (sim) + the SavedPlayer's `custom_tags`
    /// (player attach) — any extra stat / faction standing / curse / currency /
    /// attribute that doesn't fit a standard field. Rendered as a bounded
    /// `custom:` line so it reaches the narrator (entities themselves are
    /// persisted but NOT prompted). `#[serde(default)]` keeps old saves
    /// loadable as empty (dormant — no `custom:` line).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom_tags: BTreeMap<String, String>,

    /// The spatial travel graph (Fable Phase 4 Component 3, 2026-07-28):
    /// discrete locations (nodes) + adjacency edges + a single current
    /// location pointer. Rust is the SOLE authority — `apply_delta` does NOT
    /// touch this field (mirrors `world_clock` / `weather` / `player_state`).
    /// Writers to `current_node`: `[TRAVEL]` (the Tracker owns "the player
    /// moved"; Rust auto-links + allows known moves, rejects unknown ones) +
    /// the first-`[DISCOVER]`-on-empty-graph seed. `nodes` grows via
    /// `[DISCOVER]`, the `[TRAVEL]` auto-link, + the card seed/bootstrap. v1
    /// is geography only: no NPC positions, no tick traversal mechanics, no
    /// weighted edges.
    /// `#[serde(default)]` keeps pre-Component-3 saves loadable as an empty
    /// graph (dormant — no `location:` line, no adjacency to validate).
    #[serde(default)]
    pub travel_graph: TravelGraph,

    /// (2026-08-19 Hidden site maps) The pre-generated interiors, keyed by
    /// travel-node id. Rust is the SOLE authority — `apply_delta` has no
    /// field for it and `merge_patch` has no arm (the unknown-field refusal
    /// is the immunity, the `npc_interior` pattern). Writers: the JIT
    /// Architect insert (`maybe_run_site_architect`), the `[ROOM]`/
    /// `[ASSET]` bracket appliers, and LRU eviction at
    /// `site_map::MAX_SITE_MAPS`. Maps are write-once (a mapped node never
    /// re-architects + is excluded from the Stale Roulette). HashMap, never
    /// a diff target — same precedent as `npc_interior`. Rides world.json
    /// automatically (the save split is remove-based).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub site_maps: HashMap<String, crate::site_map::SiteMap>,

    /// (2026-08-20 Economy) Owned income-bearing properties with their own
    /// treasuries, keyed by property id (BTreeMap — deterministic key order
    /// in saves, the `ledger:` render, and the caps trim). Rust is the SOLE
    /// authority — `apply_delta` has no field for it and `merge_patch` has
    /// no arm (the unknown-field refusal is the immunity, the `site_maps`/
    /// `promises` pattern). Writers: the `[LEDGER]` bracket applier, the
    /// daily settlement (`economy::settle_daily_economy`), the card/player
    /// authored seeds at `enter_fable_session`, and the FIFO trim in
    /// `enforce_typed_caps` (`economy::MAX_PROPERTIES`). Rides world.json
    /// automatically (the save split is remove-based). Dormant when empty
    /// (no `ledger:` line, zero tokens).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, crate::economy::Property>,

    /// (2026-08-20 audit) First-insert order of `properties` — the TRUE FIFO
    /// eviction key for the `MAX_PROPERTIES` cap trim (the `entity_order`
    /// pattern). The pre-audit trim dropped `properties.keys().take(overflow)`
    /// — ALPHABETICALLY-first ids on the BTreeMap, not oldest — so a
    /// hand-edited over-cap install could silently delete the player's
    /// flagship "Annex" while keeping newer holdings. Re-inserting an
    /// existing id does not refresh its slot. Empty for legacy saves →
    /// backfilled deterministically (BTreeMap key order) on first
    /// enforcement; pruned of dead ids at the same reconcile.
    #[serde(default, skip_serializing_if = "std::collections::VecDeque::is_empty")]
    pub property_order: std::collections::VecDeque<String>,

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

    /// (2026-08-19 Referee QoL) Open player promises (`[PROMISE <npc_id>
    /// <description> | <minutes>]`). The tracker emits them; Rust tracks
    /// acceptance + deadline and renders the frustration band on the
    /// `owed:` line for PRESENT givers. Fulfilled promises are REMOVED
    /// (`[PROMISE <npc_id> -<description>`) — v1 keeps no history.
    /// Rust-owned: no `apply_delta` field, no `merge_patch` arm; capped at
    /// `MAX_PROMISES` (FIFO) in `enforce_typed_caps`. Rides the npc.json
    /// split (giver-keyed, like relationships).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub promises: Vec<Promise>,

    /// (2026-08-22 living-world) Open quests (`[QUEST new/update/done/
    /// fail]`) — what the PLAYER is trying to accomplish, the structural
    /// complement to `promises`. Dormant when empty (zero tokens for fresh
    /// games — the economy precedent). `done`/`fail` REMOVE (v1 keeps no
    /// history, the PROMISE precedent); overdue non-player-giver quests
    /// auto-fail on the tick via the promise frustration curve
    /// (`quest_deadline_frustration`). Rust-owned: no `apply_delta` field,
    /// no `merge_patch` arm; capped at `MAX_QUESTS` (FIFO) in
    /// `enforce_typed_caps`. Rides world.json (the save split is
    /// remove-based — quests are player+world facing, not giver-keyed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quests: Vec<Quest>,

    /// (2026-08-22 multihog WS1) Timed entity expiry — entity key → the
    /// in-world minute its truth lapses. Armed by the `[EXPIRY]` bracket;
    /// swept deterministically on every clock advance by
    /// [`Self::sweep_entity_expiry`] (pre-tick-gate, the economy-settle
    /// precedent: cheap clock math never waits for the LLM gate). Immutable
    /// + `player.*` identity keys refuse deletion at the sweep (the lock
    /// outranks time); a lapsed slot is removed either way so the
    /// observation never re-fires. Rust-owned: no `apply_delta` field, no
    /// `merge_patch` arm. Dormant when empty (zero tokens, byte-identical
    /// pre-WS1 saves).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entity_expiry: BTreeMap<String, i64>,

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

    /// The Rust-authoritative named-NPC registry (Fable Phase 5A, 2026-07-29):
    /// seeded once from the scenario card's `<cast>` block by
    /// `enter_fable_session`; the source of truth for which NPC ids exist
    /// (the `[PRESENCE]` whitelist). Rust is the SOLE authority —
    /// `apply_delta` does NOT touch this field (mirrors `travel_graph` /
    /// `weather` / `world_clock`). The only writer is the scenario card seed.
    /// Dormant when empty (a card with no `<cast>` block → pre-Phase-5
    /// behavior: no `cast:` line, no `present:` line, `[PRESENCE]` brackets
    /// reject every npc_id as unknown). `#[serde(default)]` keeps pre-Phase-5
    /// saves loadable as an empty registry.
    #[serde(default)]
    pub npc_registry: NpcRegistry,

    /// NPCs currently on-camera (Fable Phase 5A, 2026-07-29): one entry per
    /// NPC the Tracker asserted via `[PRESENCE ...]` this turn or within the
    /// grace window (`PRESENCE_GRACE_RESET`). The `present:` render line in
    /// `render_for_prompt` is the anti-teleport whitelist the narrator obeys
    /// — only NPCs in this list may speak, act, or be addressed in the scene.
    /// Rust is the SOLE authority — `apply_delta` does NOT touch this field;
    /// the only writer is the `[PRESENCE]` applier (with the grace-decay
    /// pass). `#[serde(default)]` keeps pre-Phase-5 saves loadable as empty.
    #[serde(default)]
    pub presences: Vec<Presence>,

    /// (2026-08-18 Dedicated-NPC interior state): per-NPC held items, mood,
    /// and distilled intent — keyed by canonical registry id (the same key
    /// space as `relationships`). The machinery behind NPCs that steal, lie,
    /// and hold grudges: the `[NPC_ITEM]`/`[MOOD]`/`[INTENT]` appliers write
    /// it, the reaper (world tick) archives it, `render_for_prompt` surfaces
    /// it as the PRESENT-only `minds:` line. Rust is the SOLE authority —
    /// `apply_delta` does NOT touch it and `merge_patch` has no arm for it
    /// (same structural immunity as `player_state.equipment`). Rides the
    /// npc.json split + every save/undo snapshot for free as part of
    /// `WorldSchema`. `#[serde(default)]` keeps pre-interior saves loadable
    /// as an empty map (dormant — no `minds:` line, zero prompt cost).
    #[serde(default)]
    pub npc_interior: HashMap<String, NpcInterior>,

    /// Active stage background for this world (2026-08-11 Background Library):
    /// the FILENAME of the selected image in the shared library at
    /// `apps/fable/images/backgrounds/`. `None` = no background (the default
    /// pure-black void). PER-CARD + SAVE-PERSISTENT: this field rides inside
    /// EVERY save — `fable_save_now` + the per-turn autosave both snapshot
    /// the full schema, and `fable_load_save` restores
    /// it — so leaving and continuing a game brings its background back with
    /// the save. Rust is the SOLE authority: `apply_delta` does NOT touch it
    /// (the AI tracker can neither read nor write the selection) and
    /// `render_for_prompt` does NOT emit it (it never reaches the narrator
    /// prompt). The only writers are the `fable_background_active_set` IPC
    /// (user selection) and the save/load restore path. The library itself is
    /// GLOBAL (shared across cards); only THIS selection field is per-card.
    /// `#[serde(default)]` keeps pre-Background-Library saves loadable as None.
    #[serde(default)]
    pub background: Option<String>,
}

impl WorldSchema {
    /// (2026-08-22 multihog WS1) Deterministic timestamp expiry sweep: every
    /// entity whose armed deadline has passed is DELETED, and one
    /// narrator-facing directive is returned per expiry (positive form —
    /// the lapse is settled world fact, never something to re-litigate).
    /// Immutable + `player.*` identity keys refuse deletion (the expiry
    /// slot still drops, so the refusal is observed once, never re-fired
    /// every clock advance). Pure: `now_minutes` passed in. Returns
    /// `(directives, mutation_count)` so the caller's pre-mutation
    /// snapshot discipline only fires when something actually moved.
    pub fn sweep_entity_expiry(&mut self, now_minutes: i64) -> (Vec<String>, usize) {
        if now_minutes <= 0 || self.entity_expiry.is_empty() {
            return (Vec::new(), 0);
        }
        let day = now_minutes / 1440 + 1;
        let rem = now_minutes % 1440;
        let (h12, meridiem) = to_12h(rem / 60);
        let clock = format!("Day {day}, {h12:02}:{:02} {meridiem}", rem % 60);
        let due: Vec<String> = self
            .entity_expiry
            .iter()
            .filter(|(_, at)| **at <= now_minutes)
            .map(|(k, _)| k.clone())
            .collect();
        let mut directives = Vec::new();
        let mut mutated = 0usize;
        for key in due {
            self.entity_expiry.remove(&key);
            mutated += 1;
            if self.immutable_keys.contains(&key) || key.starts_with("player.") {
                tracing::warn!(%key, "entity expiry refused: locked identity key survives the sweep");
                continue;
            }
            if self.remove_entity_with_slot(&key) {
                directives.push(format!(
                    "Expired: {key} — the state lapsed as of {clock}. Narrate the world \
                     having moved on; treat the lapse as settled fact."
                ));
            }
        }
        (directives, mutated)
    }

    /// (2026-08-22 re-track hardening) Keep the Rust-owned ANCHORS — the
    /// world clock + the calendar label family + the rest anchor — when a
    /// re-track path reverts the live schema to a stored base. `world_clock`
    /// and `calendar` are Rust-authoritative state the `[TIME]`/`[DATE]`
    /// brackets advanced for a turn that STILL HAPPENED (an edit/reroll
    /// changes the prose, not the fact that time flowed); the 2026-08-22
    /// playtest caught the base revert rolling the clock back whenever the
    /// re-track's re-emission hit its token wall and dropped the `[TIME]`
    /// bracket — turn 2 then re-applied `09:05` against `prev=540` a
    /// second time. `last_rest_minutes` is the same class of fact (the
    /// Recovery Referee's `[REST]` stamp): reverting it beside a preserved
    /// post-sleep clock would re-render a phantom weary/exhausted band and
    /// clamp stamina/mana back down on the next `[TIME]`. All brackets are
    /// ABSOLUTE-set (idempotent), so a faithful re-emission over preserved
    /// anchors is a no-op — only the dropped-bracket rollback is killed.
    /// Called on the EDIT re-track + the successful-REROLL revert ONLY; the
    /// cancel/api_lost full-revert paths restore the whole pre-turn world
    /// by design (there the turn never happened).
    pub fn retain_revert_safe_anchors(&mut self, live: &WorldSchema) {
        self.world_clock = live.world_clock.clone();
        self.calendar = live.calendar.clone();
        self.calendar_synced_minutes = live.calendar_synced_minutes;
        self.last_rest_minutes = live.last_rest_minutes;
    }

    /// (2026-08-15 audit fix) The ONE capped entry point for direct
    /// `recent_events` pushes (the belt-spill note, the Soul-Gem UI
    /// `event_note`). The delta path caps via `cap_recent_events`; the
    /// direct `Vec::push` sites bypassed the 50-entry cap until the next
    /// delta pass — every push now goes through here so the stored vec
    /// can never outgrow the cap between passes. Caller pre-trims the text
    /// (EVENT_NOTE_MAX discipline lives at the call sites).
    pub fn push_event(&mut self, event: String) {
        self.recent_events.push(event);
        self.cap_recent_events();
    }

    /// (P3 fix) Bound the stored history: the render reads only the last 5,
    /// but the stored vec grew unbounded over a long campaign (save bloat +
    /// prompt-path drift). Keep the most recent 50.
    fn cap_recent_events(&mut self) {
        let overflow = self.recent_events.len().saturating_sub(50);
        if overflow > 0 {
            self.recent_events.drain(..overflow);
        }
    }

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
            self.cap_recent_events();
        }
        if let Some(ents) = delta.entities {
            let mut grew = false;
            for (key, value) in ents {
                // (2026-08-15 audit fix) Legacy freeform inventory keys are
                // refused here: the typed inventory owns items, and a model
                // delta re-creating an `item_*`/`inv_*` key alongside a real
                // [PACK] bracket would duplicate the item. Strip + warn (the
                // referee-owned field strip in fable_schema_patch).
                if key.starts_with("item_") || key.starts_with("inv_") {
                    tracing::warn!(%key, "apply_delta: legacy inventory entity key refused (typed inventory owns items)");
                    continue;
                }
                match value {
                    Some(v) => {
                        if !self.entities.contains_key(&key) {
                            self.entity_order.push_back(key.clone());
                            grew = true;
                        }
                        self.entities.insert(key, v);
                    }
                    None => {
                        self.remove_entity_with_slot(&key);
                    }
                }
            }
            if grew {
                self.enforce_entity_cap();
            }
        }
    }

    /// (2026-08-24 review fix) Delete an entity AND its `entity_order` slot
    /// together. A deleted key used to leave its stale slot behind: the
    /// deque grew unboundedly with insert/delete churn (the cap sweep only
    /// pruned slots once OVER the cap), and re-inserting the same key later
    /// minted a SECOND slot while the stale one made the key look older than
    /// it was — premature FIFO eviction against the true first-insert order.
    fn remove_entity_with_slot(&mut self, key: &str) -> bool {
        let removed = self.entities.remove(key).is_some();
        if removed {
            self.entity_order.retain(|o| o.as_str() != key);
        }
        removed
    }

    /// (2026-08-16 audit fix #14) Total-entity cap with FIFO eviction — see
    /// `entity_order`. `player.*` keys are NEVER evicted: they are the
    /// identity ground truth seeded at attach (§6C; the narrator + retrieval
    /// read them as who the player IS). If protected keys alone exceed the
    /// cap they all stay — the cap is a growth guard, not a hard wall.
    pub fn enforce_entity_cap(&mut self) {
        const ENTITY_TOTAL_CAP: usize = 500;
        if self.entities.len() <= ENTITY_TOTAL_CAP {
            return;
        }
        // Deterministic backfill for legacy saves (no order list): sorted
        // keys stand in for insert order — arbitrary but stable.
        if self.entity_order.is_empty() && !self.entities.is_empty() {
            let mut keys: Vec<String> = self.entities.keys().cloned().collect();
            keys.sort();
            self.entity_order = keys.into();
        }
        let mut idx = 0;
        while self.entities.len() > ENTITY_TOTAL_CAP && idx < self.entity_order.len() {
            let key = self.entity_order[idx].clone();
            if !self.entities.contains_key(&key) {
                // Deleted since ordering — drop the stale slot.
                self.entity_order.remove(idx);
                continue;
            }
            // (2026-08-16 yellow S5) Immutable keys are EXEMPT from FIFO
            // eviction: they occupy the oldest order slots by construction
            // (canon keys seed first), so the sweep used to evict them before
            // the immutability lock's overwrite check could ever matter —
            // a first-set retcon path would then re-mint the "locked" key.
            // The set ships empty today (latent); this makes the lock real.
            if key.starts_with("player.") || self.immutable_keys.contains(&key) {
                idx += 1;
                continue;
            }
            self.entities.remove(&key);
            self.entity_order.remove(idx);
            tracing::info!(%key, "entity cap (500) reached; oldest entity FIFO-evicted");
        }
        // Re-sync: any map keys the order list never knew (seeded outside
        // the model paths) append at the tail so future sweeps see them.
        if self.entity_order.len() < self.entities.len() {
            let missing: Vec<String> = self
                .entities
                .keys()
                .filter(|k| !self.entity_order.iter().any(|o| o.as_str() == k.as_str()))
                .cloned()
                .collect();
            for k in missing {
                self.entity_order.push_back(k);
            }
        }
    }

    /// Merge a partial JSON patch into this schema. Used by the model-facing
    /// `fable_schema_patch` tool (chat-side stateful tool, dispatched inline
    /// from `run_agent_loop`). Mirrors the user-facing `fable_schema_set` IPC's
    /// authority but with field-level granularity: only the top-level keys
    /// PRESENT in `patch` are replaced; absent keys keep their current value.
    ///
    /// # Merge semantics (load-bearing)
    ///
    /// - `entities`: shallow-merge by key. For each `(k, v)` in patch.entities:
    ///   `Value::Null` deletes the key from `self.entities`; any other value
    ///   upserts. (Lets the model add/remove individual entity keys without
    ///   resending the whole map.)
    /// - All other allowed top-level fields: **full-replace** via typed
    ///   deserialization. The patch value is fed through
    ///   `serde_json::from_value::<FieldType>`; a type error becomes a
    ///   model-facing error string (so the repair loop can fold it back).
    ///
    /// # Excluded field
    ///
    /// `immutable_keys` is REFUSED if present in the patch. The immutability
    /// lock is the meta-layer protecting other fields from LLM retcons; letting
    /// the model edit it would let it unlock its own canon. (The user-facing
    /// `fable_schema_set` IPC bypasses this — user trust = full control; the
    /// model path doesn't get that trust.)
    ///
    /// Returns the list of top-level field names that were merged (for the
    /// `tool_result` payload so the model can see what it changed).
    pub fn merge_patch(&mut self, patch: serde_json::Value) -> Result<Vec<String>, String> {
        let obj = patch
            .as_object()
            .ok_or_else(|| "patch must be a JSON object".to_string())?
            .clone();
        if obj.contains_key("immutable_keys") {
            return Err(
                "immutable_keys is the meta-lock + may not be patched (it would \
                 let the model unlock its own canon)"
                    .to_string(),
            );
        }
        // (#47 2026-08-15) Defense-in-depth: run the patch's delta-shaped
        // fields through the SAME structural validator the schema engine
        // uses (caps, control chars, immutability lock). The caller
        // (`dispatch_fable_state_tool`) pre-filters to these three fields,
        // but the mutation fn itself must not silently accept what the
        // validator would reject — the defense belonged here, not only in
        // the caller.
        {
            let mut delta = SchemaDelta::default();
            if let Some(v) = obj.get("summary") {
                delta.summary = serde_json::from_value(v.clone())
                    .map_err(|e| format!("summary: {e}"))?;
            }
            if let Some(v) = obj.get("recent_events") {
                delta.recent_events = serde_json::from_value(v.clone())
                    .map_err(|e| format!("recent_events: {e}"))?;
            }
            if let Some(v) = obj.get("entities").and_then(|v| v.as_object()) {
                delta.entities = Some(
                    v.iter()
                        .map(|(k, ev)| {
                            (k.clone(), if ev.is_null() { None } else { Some(ev.clone()) })
                        })
                        .collect(),
                );
            }
            let existing_keys: std::collections::HashSet<String> =
                self.entities.keys().cloned().collect();
            let ctx = crate::schema_validator::ValidationContext {
                known_nodes: None,
                immutable_keys: Some(&self.immutable_keys),
                existing_keys: Some(&existing_keys),
            };
            crate::schema_validator::validate(&delta, &ctx).map_err(|f| f.to_string())?;
        }
        let mut merged: Vec<String> = Vec::new();
        for (key, value) in obj {
            match key.as_str() {
                "summary" => {
                    self.summary =
                        serde_json::from_value(value).map_err(|e| format!("summary: {e}"))?;
                    merged.push("summary".into());
                }
                "recent_events" => {
                    self.recent_events = serde_json::from_value(value)
                        .map_err(|e| format!("recent_events: {e}"))?;
                    merged.push("recent_events".into());
                }
                "entities" => {
                    // Shallow-merge: Null deletes, any other value upserts.
                    let map = value
                        .as_object()
                        .ok_or_else(|| "entities must be an object".to_string())?;
                    let mut grew = false;
                    for (ek, ev) in map {
                        // (2026-08-15 audit fix) Same legacy-key refusal as
                        // apply_delta — the typed inventory owns items.
                        if ek.starts_with("item_") || ek.starts_with("inv_") {
                            tracing::warn!(%ek, "merge_patch: legacy inventory entity key refused (typed inventory owns items)");
                            continue;
                        }
                        if ev.is_null() {
                            self.remove_entity_with_slot(ek);
                        } else {
                            if !self.entities.contains_key(ek) {
                                self.entity_order.push_back(ek.clone());
                                grew = true;
                            }
                            self.entities.insert(ek.clone(), ev.clone());
                        }
                    }
                    // (2026-08-16 audit fix #14) Same total-entity cap as
                    // apply_delta — the patch path grows entities too.
                    if grew {
                        self.enforce_entity_cap();
                    }
                    merged.push("entities".into());
                }
                "player_state" => {
                    self.player_state = serde_json::from_value(value)
                        .map_err(|e| format!("player_state: {e}"))?;
                    merged.push("player_state".into());
                }
                "world_clock" => {
                    self.world_clock = serde_json::from_value(value)
                        .map_err(|e| format!("world_clock: {e}"))?;
                    merged.push("world_clock".into());
                }
                "weather" => {
                    self.weather =
                        serde_json::from_value(value).map_err(|e| format!("weather: {e}"))?;
                    merged.push("weather".into());
                }
                "travel_graph" => {
                    let graph: TravelGraph = serde_json::from_value(value)
                        .map_err(|e| format!("travel_graph: {e}"))?;
                    // (2026-08-16 audit LOW) Refuse-at-cap, the applier's
                    // discipline — a full-replace graph over the node cap is
                    // a corrupt/hostile patch, and truncating could drop an
                    // authored hub mid-list.
                    if graph.nodes.len() > MAX_TRAVEL_NODES {
                        return Err(format!(
                            "travel_graph: {} nodes exceeds the {} cap",
                            graph.nodes.len(),
                            MAX_TRAVEL_NODES
                        ));
                    }
                    self.travel_graph = graph;
                    merged.push("travel_graph".into());
                }
                "scene_pacing" => {
                    self.scene_pacing = serde_json::from_value(value)
                        .map_err(|e| format!("scene_pacing: {e}"))?;
                    merged.push("scene_pacing".into());
                }
                "status_tags" => {
                    self.status_tags = serde_json::from_value(value)
                        .map_err(|e| format!("status_tags: {e}"))?;
                    merged.push("status_tags".into());
                }
                "relationships" => {
                    self.relationships = serde_json::from_value(value)
                        .map_err(|e| format!("relationships: {e}"))?;
                    merged.push("relationships".into());
                }
                "offscreen_tasks" => {
                    self.offscreen_tasks = serde_json::from_value(value)
                        .map_err(|e| format!("offscreen_tasks: {e}"))?;
                    merged.push("offscreen_tasks".into());
                }
                "rumors" => {
                    self.rumors =
                        serde_json::from_value(value).map_err(|e| format!("rumors: {e}"))?;
                    merged.push("rumors".into());
                }
                "npc_registry" => {
                    let registry: NpcRegistry =
                        serde_json::from_value(value).map_err(|e| format!("npc_registry: {e}"))?;
                    // (2026-08-16 yellow W3) Refuse-at-cap, the applier's +
                    // travel-graph's discipline — a full-replace registry over
                    // the cap is a corrupt/hand-edited patch (the bracket
                    // applier refuses one-by-one at the same ceiling), and
                    // truncating could drop an authored cast entry mid-list.
                    if registry.entries.len() > MAX_NPC_REGISTRY {
                        return Err(format!(
                            "npc_registry: {} entries exceeds the {} cap",
                            registry.entries.len(),
                            MAX_NPC_REGISTRY
                        ));
                    }
                    self.npc_registry = registry;
                    merged.push("npc_registry".into());
                }
                "presences" => {
                    self.presences = serde_json::from_value(value)
                        .map_err(|e| format!("presences: {e}"))?;
                    merged.push("presences".into());
                }
                // Note: `immutable_keys` is refused up top (the meta-lock).
                unknown => {
                    return Err(format!(
                        "unknown top-level field {unknown:?}; allowed: summary, \
                         recent_events, entities, player_state, world_clock, weather, \
                         travel_graph, scene_pacing, status_tags, relationships, \
                         offscreen_tasks, rumors, npc_registry, presences"
                    ));
                }
            }
        }
        // (2026-08-16 audit LOW) Typed full-replace defense-in-depth: clamp
        // the referee-owned collections to their growth caps (the dispatch
        // pre-filter is the primary gate; this is the backstop for the raw
        // JSON tab + any future caller).
        if !merged.is_empty() {
            self.enforce_typed_caps();
        }
        Ok(merged)
    }

    /// Render the schema into a compact, prompt-friendly string for injection
    /// into the chat turn's `<world_state>` block. Compactness matters: this
    /// goes into the inter-turn region alongside the memory block, and every
    /// token is prefill cost.
    ///
    /// Emits ONLY bounded deterministic anchors: clock (or `date:` + time-of-day
    /// when a calendar label is set), weather, location + exits, present NPCs,
    /// current-node rumors, summary, the last 5 recent events, a bounded
    /// `custom:` line, + player_state (stamina/injuries/equipped/appearance).
    /// The `entities` map is deliberately NOT rendered (2026-08-10): it was the
    /// sole unbounded growth source. Entity state stays in the Rust schema
    /// (God-Tier authority) and reaches the model via the 1-turn bracket
    /// window — never via this prompt block, never via probabilistic RRF.
    ///
    /// Returns an empty string for an empty schema so the caller can skip
    /// emitting the `<world_state>` block entirely (matches the memory block's
    /// empty-skip behavior in `chat_format.rs`).
    /// (2026-08-16 yellow S7) Render-time inline flattening for prose fields
    /// that ride `<world_state>` / the schema-engine prompt as single lines.
    /// The write-time gate stops new mutations, but a pre-fix or hand-edited
    /// SAVE can still carry an embedded newline that would forge a fake
    /// `present:`/`clock:`/`exits:` render line into every later prompt. Chars,
    /// not bytes (anti-pattern #6).
    fn flatten_inline(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                '\n' | '\r' | '\t' => ' ',
                _ => c,
            })
            .collect()
    }

    pub fn render_for_prompt(&self) -> String {
        self.render_for_prompt_opts(false, true)
    }

    /// The exposure-gated variant (2026-08-19): passes the reveal flag down
    /// to the player_state block (a `beneath:` line naming concealed wear on
    /// turns whose narrative window tripped the exposure gate — see
    /// `equipment::narrative_trips_exposure`). Used ONLY by the API narrator
    /// tail render; the tracker skeleton + every other consumer stay on the
    /// ungated `render_for_prompt`.
    pub fn render_for_prompt_with_beneath(&self, reveal_beneath: bool) -> String {
        self.render_for_prompt_opts(reveal_beneath, true)
    }

    /// (2026-08-27 evening Chloe ruling) The LOCAL-tracker render: identical
    /// to the standard render except the `tone:` line NEVER appears. Tone is
    /// card/story flavor — it belongs to the API narrator's prose and to
    /// nothing local. The tracker (and its architect sibling) is a state
    /// ledger that parses the user's message + the AI's beat and emits a
    /// state delta; feeding it the tone is how comedy-flavored map names
    /// ("The Awkward Greeting Zone") got minted. This is the hard gate —
    /// every local Fable prompt surface renders through here, never through
    /// the tone-inclusive variants.
    pub fn render_for_prompt_local(&self) -> String {
        self.render_for_prompt_opts(false, false)
    }

    fn render_for_prompt_opts(&self, reveal_beneath: bool, include_tone: bool) -> String {
        let empty = self.summary.trim().is_empty()
            && self.recent_events.is_empty()
            && self.player_state.is_default()
            && !self.world_clock.is_set()
            && !self.weather.is_set()
            // (2026-08-23 audit fix) `tone` renders + `currency_label` wakes
            // the economy anchor — a schema whose ONLY live field is either
            // used to early-return empty here, suppressing both.
            // (2026-08-27 evening) A tone-suppressed render doesn't count the
            // tone toward non-emptiness — the tracker variant of a
            // tone-only schema is legitimately empty.
            && (!include_tone || !self.tone.as_deref().is_some_and(|s| !s.trim().is_empty()))
            && self.currency_label.trim().is_empty()
            && !self.travel_graph.is_set()
            && self.rumors.is_empty()
            && !self.npc_registry.is_set()
            && self.presences.is_empty()
            && self.properties.is_empty()
            && self.quests.is_empty()
            && self.calendar.as_deref().filter(|s| !s.is_empty()).is_none()
            && self.custom_tags.is_empty();
        if empty {
            return String::new();
        }

        let mut out = String::with_capacity(512);
        // Clock renders FIRST (before summary): the narrator needs the current
        // time as its top-of-mind anchor so its [TIME ...] emissions advance
        // it coherently. Without seeing the current time, the narrator would
        // emit inconsistent timestamps. ~30 tokens; zero when unset.
        //
        // Calendar coupling (2026-08-13): when a rich `calendar` label is set
        // (month/year/type-of-day, advanced via `[DATE]`), emit `date:` + render
        // the clock as time-of-day ONLY ("14:00") — the day/date is carried by
        // the label, so a "Day N" counter would be redundant/conflicting. When
        // unset (legacy cards), the "clock: Day N, HH:MM" render stands.
        if let Some(cal) = self.calendar.as_deref().filter(|s| !s.is_empty()) {
            out.push_str("date: ");
            // (2026-08-24 review P2) flatten_inline — the calendar label is
            // free text from `[DATE]`/hand-edited saves; a raw render could
            // forge prompt lines.
            out.push_str(&Self::flatten_inline(cal));
            // (P1d, 2026-08-17 E4B shakedown) Staleness fallback: if the label
            // wasn't refreshed via [DATE] in >48h of clock time, append the
            // true day counter so the prompt never asserts a date the clock
            // has long passed (the playtest label said "17th of Peatfall"
            // forever while the clock reached Day ~17 — 0 [DATE] in 51 turns).
            // The day-crossing directive nudges the tracker first; this is
            // the mechanical backstop when it doesn't comply.
            if let Some(synced) = self.calendar_synced_minutes {
                if self.world_clock.is_set()
                    && self.world_clock.current_minutes.saturating_sub(synced) > 48 * 60
                {
                    let day = self.world_clock.current_minutes / 1440 + 1;
                    out.push_str(&format!(" — day {day}"));
                }
            }
            out.push('\n');
            if let Some(tod) = self.world_clock.render_time_of_day() {
                out.push_str("clock: ");
                out.push_str(&tod);
                out.push('\n');
            }
        } else if let Some(clock_line) = self.world_clock.render_clock_line() {
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
                // (2026-08-24 review P2) flatten_inline — condition is
                // free text; same newline-injection gate as date/tone.
                out.push_str(&Self::flatten_inline(&weather_line));
                out.push('\n');
            }
        }
        // Tone (2026-08-19): the simulation's tone rides WITH the time +
        // weather — live world state the tracker owns, seeded from the card's
        // `<world>` sibling, not static prompt text (the card cache block
        // carries identity only). None → no line (dormant).
        // (2026-08-27 evening Chloe ruling) GATED on `include_tone`: the
        // local-tracker render (`render_for_prompt_local`) NEVER emits it —
        // tone is narrator-only flavor, and it was the comedy-map-name engine.
        if include_tone {
            if let Some(tone) = self.tone.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                out.push_str("tone: ");
                // (2026-08-24 review P2) flatten_inline — same gate as the other
                // anchor lines (hand-edited saves).
                out.push_str(&Self::flatten_inline(&tone.chars().take(120).collect::<String>()));
                out.push('\n');
            }
        }
        // (2026-08-22 living-world) The rested anchor's read — how long the
        // player has gone without genuine rest, + the fatigue band once it
        // bites. Clock-family placement (with weather/tone, before location):
        // it's a when-anchor the narrator colors prose with and Rust clamps
        // stamina/mana against. Dormant while the anchor is unset (legacy
        // saves / fresh games — zero tokens).
        if self.world_clock.is_set() && self.last_rest_minutes > 0 {
            let delta = self
                .world_clock
                .current_minutes
                .saturating_sub(self.last_rest_minutes);
            if delta > 0 {
                let hours = delta / 60;
                match rested_band(delta) {
                    Some(band) => {
                        out.push_str(&format!("rested: {hours}h since last rest — {band}\n"))
                    }
                    None => out.push_str(&format!("rested: {hours}h since last rest\n")),
                }
            }
        }
        // Location renders alongside clock + weather (Component 3, 2026-07-28):
        // the third top-of-mind anchor. The narrator needs the current location
        // + its exits to write coherent movement prose + emit valid `[TRAVEL]`
        // commands (without seeing the exits, the Tracker would guess at node
        // ids). `None` when no current node is set (dormant — zero tokens,
        // mirroring `WorldClock` / `Weather` before their first command).
        if let Some(mut travel_line) = self.travel_graph.render_line() {
            // (2026-08-20 Economy) Location-line prosperity marker — the
            // narrator reads the town's fortunes WHERE it stands (≤50 hard
            // times / ≥150 booming), so prose colors itself without a
            // separate prompt line.
            if let Some(cur) = self.travel_graph.current_node.as_deref() {
                if let Some(prosperity) = self
                    .travel_graph
                    .nodes
                    .iter()
                    .find(|n| n.id == cur)
                    .map(|n| n.prosperity)
                {
                    if prosperity <= 50 {
                        travel_line.push_str(" — hard times");
                    } else if prosperity >= 150 {
                        travel_line.push_str(" — booming");
                    }
                }
            }
            out.push_str("location: ");
            out.push_str(&travel_line);
            out.push('\n');
        }
        // (2026-08-19 Hidden site maps) The `site:` block — the knowledge-
        // filtered interior of the CURRENT node, straight after `location:`
        // (the "what's inside here" pairing with "where am I"). Dormant when
        // the current node has no map (the common case outdoors — zero
        // tokens). Multi-line indented block; every line flattened (a
        // hand-edited save can't forge a render line) — the tracker's lean
        // render flattens + caps it further.
        // (2026-08-23 hosted interiors) THE RESOLVER LAW: the block renders
        // the ACTIVE map — the building's interior while the player stands
        // in one, led by a single parent breadcrumb line. The district map
        // itself stays out (bounded prompting; the exit transition is the
        // way back).
        if let Some(cur_id) = self.travel_graph.current_node.as_deref() {
            let active_key =
                crate::site_map::active_site_map_key(&self.site_maps, Some(cur_id));
            let (map_key, breadcrumb): (Option<String>, Option<String>) = match &active_key {
                Some(k) if k.as_str() != cur_id => (
                    Some(k.clone()),
                    crate::site_map::hosted_breadcrumb(
                        &self.site_maps,
                        &self.travel_graph,
                        k,
                    ),
                ),
                other => (other.clone(), None),
            };
            if let Some(site_block) = map_key
                .as_deref()
                .and_then(|k| self.site_maps.get(k))
                // (2026-08-22 living-world) `now` rides in — the re-entry
                // digest + the remnants collapse are clock-driven.
                .and_then(|m| crate::site_map::render_narrator_slice(m, self.world_clock.current_minutes))
            {
                out.push_str("site:\n");
                if let Some(bc) = breadcrumb {
                    out.push_str("  in ");
                    out.push_str(&Self::flatten_inline(&bc));
                    out.push('\n');
                }
                for line in site_block.lines() {
                    out.push_str("  ");
                    out.push_str(&Self::flatten_inline(line));
                    out.push('\n');
                }
            }
        }
        // Present NPCs (Phase 5A, 2026-07-29): the on-camera whitelist. The
        // narrator sees ONLY the NPCs the Tracker asserted via `[PRESENCE]`
        // this turn (or within the grace window). This is the anti-teleport
        // enforcement vector: an NPC not in this list may not speak, act, or
        // be addressed in the scene (bound by the narrator_core clause).
        // Renders immediately after `location:` — the natural "who's here"
        // pairing with "where am I". Dormant when `presences` is empty (a
        // fresh game with no `[PRESENCE]` yet, or a card with no `<cast>`
        // registry — zero tokens). Format:
        //   `present: Mara (standing by the bar, arms crossed), Corin (tuning a lute)`
        if !self.presences.is_empty() {
        // (#48) HARD-CAPPED at the first 16 + a `(+N more)` marker —
        // presence is bounded by the [PRESENCE]-per-turn rebuild + the
        // 4-turn age-out in practice, but a burst turn (a crowded tavern)
        // must not blow the always-on prompt line. (2026-08-21 evening
        // follow-up to the 8192 ruling: 12 → 16.)
            const PRESENCE_PROMPT_CAP: usize = 16;
            let shown = self.presences.len().min(PRESENCE_PROMPT_CAP);
            let mut parts: Vec<String> = self.presences[..shown]
                .iter()
                .map(|p| {
                    if p.stance.trim().is_empty() {
                        p.name.clone()
                    } else {
                        format!("{} ({})", p.name, p.stance.trim())
                    }
                })
                .collect();
            let hidden = self.presences.len() - shown;
            if hidden > 0 {
                parts.push(format!("(+{hidden} more)"));
            }
            out.push_str("present: ");
            // (2026-08-24 review P2) flatten_inline — names + stances are
            // free text; same newline-injection gate as minds:/wearing:.
            out.push_str(&Self::flatten_inline(&parts.join(", ")));
            out.push('\n');
        }
        // (2026-08-18 Dedicated-NPC interior state) The `minds:` line —
        // per-NPC interior for PRESENT NPCs only (scene-scoped injection:
        // the FIELD is unbounded, the prompt line never is). Same
        // anti-teleport whitelist as `present:` — an NPC not on-camera this
        // turn (or within grace) gets no interior line, so off-screen
        // scheming never leaks into the scene. This is what lets the API
        // narrator play the lie: it reads that Mara is suspicious, intends
        // to get the player out, and carries the stolen ring, and writes
        // prose that conceals exactly that. HARD-CAPPED at the
        // `NPC_MINDS_PROMPT_CAP` highest-priority interior-bearing present
        // NPCs + `(+N more)` (crowded-tavern guard, mirrors `present:`'s
        // #48 cap — the caps move together); flattened
        // at the join so a hand-edited save can't forge extra prompt
        // lines. Dormant when no present NPC has interior state (zero
        // tokens, always).
        //
        // (2026-08-18 audit) The capped selection is IMPORTANCE-RANKED,
        // never positional: `core` cast first, then reaper-protected
        // interiors (the Bonded/Nemesis relationship bands, open tasks,
        // item-holders — the same derived signals `npc_is_reaper_protected`
        // reads), then ambient discovery order. A crowded scene clips
        // throwaways, never the scene's principals — the identical
        // tier-aware-truncation discipline as the relationships cap. The
        // ranking also orders the joined line core-first, so the LEAN
        // render's trailing `, `-boundary cut (lib.rs
        // `lean_truncate_line`) drops ambient entries before it can touch
        // a principal's. Same cap, same flatten — only the selection
        // order, so the token cost is unchanged.
        if !self.presences.is_empty() && !self.npc_interior.is_empty() {
            let mut ranked: Vec<(u8, String)> = Vec::new();
            for p in &self.presences {
                let Some(interior) = self.npc_interior.get(&p.npc_id) else {
                    continue;
                };
                let entry = interior.render_minds_entry(&p.name);
                if entry.is_empty() {
                    continue;
                }
                let is_core = self
                    .npc_registry
                    .entries
                    .iter()
                    .any(|e| e.id == p.npc_id && e.prominence == NpcProminence::Core);
                let rank: u8 = if is_core {
                    0
                } else if self.reaper_protection_reason(&p.npc_id).is_some() {
                    1
                } else {
                    2
                };
                ranked.push((rank, entry));
            }
            // Stable sort: presences order survives within a rank.
            ranked.sort_by_key(|(rank, _)| *rank);
            let mut parts: Vec<String> = Vec::new();
            let mut hidden = 0usize;
            for (_, entry) in ranked {
                if parts.len() < NPC_MINDS_PROMPT_CAP {
                    parts.push(entry);
                } else {
                    hidden += 1;
                }
            }
            if !parts.is_empty() {
                if hidden > 0 {
                    parts.push(format!("(+{hidden} more)"));
                }
                out.push_str("minds: ");
                out.push_str(&Self::flatten_inline(&parts.join(", ")));
                out.push('\n');
            }
        }
        // (2026-08-19 zone sweep) The `wearing:` line — the OUTFITS of
        // PRESENT NPCs (seeded from an npc card's `<inventory>` Clothing/
        // Equipped-garments/Accessories through the shared garment router:
        // npc-card clothing is auto-EQUIPPED from turn one, never mixed into
        // the held rack). Same present-scoped whitelist + cap discipline as
        // `holding:`; per-NPC names capped at 5 + `(+N)`. Dormant when no
        // present NPC wears anything (zero tokens, always).
        if !self.presences.is_empty() && !self.npc_interior.is_empty() {
            const WEARING_PROMPT_CAP: usize = 6;
            const PER_NPC_WORN_CAP: usize = 5;
            let mut parts: Vec<String> = Vec::new();
            let mut hidden = 0usize;
            for p in &self.presences {
                let Some(interior) = self.npc_interior.get(&p.npc_id) else {
                    continue;
                };
                if interior.worn.is_empty() {
                    continue;
                }
                let shown: Vec<String> = interior
                    .worn
                    .iter()
                    .take(PER_NPC_WORN_CAP)
                    .map(|it| {
                        if it.qty > 1 {
                            format!("{} ×{}", it.name, it.qty)
                        } else {
                            it.name.clone()
                        }
                    })
                    .collect();
                let extra = interior.worn.len().saturating_sub(PER_NPC_WORN_CAP);
                let list = if extra > 0 {
                    format!("{}, (+{extra} more)", shown.join(", "))
                } else {
                    shown.join(", ")
                };
                let entry = format!("{}({})", p.name, list);
                if parts.len() < WEARING_PROMPT_CAP {
                    parts.push(entry);
                } else {
                    hidden += 1;
                }
            }
            if !parts.is_empty() {
                if hidden > 0 {
                    parts.push(format!("(+{hidden} more)"));
                }
                out.push_str("wearing: ");
                out.push_str(&Self::flatten_inline(&parts.join(", ")));
                out.push('\n');
            }
        }
        // (2026-08-19 v2 cards) The `holding:` line — the item racks of
        // PRESENT NPCs (seeded from an npc card's `<inventory>` sibling +
        // mutated by `[NPC_ITEM]` in play). Same scene-scoped whitelist as
        // `present:`/`minds:`: an off-camera NPC's items never render. Capped
        // at 6 present rack-holders + `(+N more)`; per-NPC names capped at 6
        // + `(+N more)` (the `wearing:` PER_NPC discipline — a hoarder's
        // 16-slot rack renders as a summary, never a shopping list; the
        // state keeps every item). Flattened at the join.
        // Dormant when no present NPC carries items (zero tokens, always) —
        // the narrator sees clothing/held items turn 1 without re-reading the
        // card.
        if !self.presences.is_empty() && !self.npc_interior.is_empty() {
            const HOLDING_PROMPT_CAP: usize = 6;
            const PER_NPC_HELD_CAP: usize = 6;
            let mut parts: Vec<String> = Vec::new();
            let mut hidden = 0usize;
            for p in &self.presences {
                let Some(interior) = self.npc_interior.get(&p.npc_id) else {
                    continue;
                };
                if interior.items.is_empty() {
                    continue;
                }
                let shown: Vec<String> = interior
                    .items
                    .iter()
                    .take(PER_NPC_HELD_CAP)
                    .map(|it| {
                        if it.qty > 1 {
                            format!("{} ×{}", it.name, it.qty)
                        } else {
                            it.name.clone()
                        }
                    })
                    .collect();
                let extra = interior.items.len().saturating_sub(PER_NPC_HELD_CAP);
                let list = if extra > 0 {
                    format!("{}, (+{extra} more)", shown.join(", "))
                } else {
                    shown.join(", ")
                };
                let entry = format!("{}({})", p.name, list);
                if parts.len() < HOLDING_PROMPT_CAP {
                    parts.push(entry);
                } else {
                    hidden += 1;
                }
            }
            if !parts.is_empty() {
                if hidden > 0 {
                    parts.push(format!("(+{hidden} more)"));
                }
                out.push_str("holding: ");
                out.push_str(&Self::flatten_inline(&parts.join(", ")));
                out.push('\n');
            }
        }
        // (2026-08-19 Referee QoL) The `owed:` line — open promises held by
        // PRESENT givers only (scene-scoped like minds:/holding:; an
        // off-screen creditor is not narrator news until they return). The
        // label comes from the volatility-scaled frustration curve; v1
        // renders the band only — NO automatic relationship mutation.
        // Dormant when empty (zero tokens, always). Capped at 8 rendered
        // promises (== MAX_PROMISES; see the const's note).
        if !self.promises.is_empty() && !self.presences.is_empty() {
            // (2026-08-21 evening follow-up to the 8192 ruling: 4 → 8 ==
            // MAX_PROMISES — every open promise held by a present giver
            // renders; the cap is the FIFO storage bound, nothing lower.)
            const OWED_PROMPT_CAP: usize = 8;
            let mut parts: Vec<String> = Vec::new();
            // (2026-08-20 audit P2-2) ALL open promises per giver, not just
            // the first: the [PROMISE] applier dedupes on (npc_id,
            // description), so one giver can legitimately hold several
            // distinct obligations — the old `.find()` silently hid every
            // one past the first. The 4-part cap now bounds rendered
            // promises (was givers), same ceiling either way.
            'giver: for p in &self.presences {
                for promise in self.promises.iter().filter(|pr| pr.npc_id == p.npc_id) {
                    if parts.len() >= OWED_PROMPT_CAP {
                        break 'giver;
                    }
                    let vol = self.relationships.get(&p.npc_id).map(|r| r.volatility);
                    let band = crate::offscreen_task::frustration_band(
                        crate::offscreen_task::promise_frustration(
                            promise.accepted_at_minutes,
                            promise.deadline_minutes,
                            self.world_clock.current_minutes,
                            vol,
                        ),
                    );
                    parts.push(format!("{} — \"{}\" — {}", p.name, promise.description, band));
                }
            }
            if !parts.is_empty() {
                out.push_str("owed: ");
                out.push_str(&Self::flatten_inline(&parts.join("; ")));
                out.push('\n');
            }
        }
        // (2026-08-22 living-world) The `quests:` line — the player's open
        // threads, ALL givers (the owed: complement). Single flattened line
        // (the lean surgery caps it); render cap 5 + `(+N more)` — the
        // worst-case tracker budget is the binding constraint, storage is
        // MAX_QUESTS. Each entry: `<title> (<giver>, <patience band when
        // overdue>) — <objectives: cur/total text, ✓ text>`; the reward
        // rides when authored. Dormant when empty (zero-token invariant for
        // fresh games — the economy precedent).
        if !self.quests.is_empty() {
            const QUESTS_PROMPT_CAP: usize = 5;
            const QUEST_OBJECTIVES_SHOWN: usize = 3;
            const QUEST_OBJ_TEXT_CHARS: usize = 40;
            // (2026-08-24 review P2) The TITLE rides the same char cap as
            // objective text — storage allows 120-char titles, and 5 × 120
            // unbounded chars is 600 the STAGE0 budget pin never priced in
            // (its fixture titles are ~43 chars).
            const QUEST_TITLE_CHARS: usize = 40;
            let mut parts: Vec<String> = Vec::new();
            for q in self.quests.iter().take(QUESTS_PROMPT_CAP) {
                let giver = if q.giver == "player" {
                    "self".to_string()
                } else {
                    self.npc_registry
                        .resolve(&q.giver)
                        .map(|e| e.name.clone())
                        .unwrap_or_else(|| q.giver.clone())
                };
                let band = crate::offscreen_task::frustration_band(
                    quest_deadline_frustration(
                        q,
                        self.relationships.get(&q.giver).map(|r| r.volatility),
                        self.world_clock.current_minutes,
                    ),
                );
                // NEG_INFINITY bands to "Very Pleased" — hide the band for
                // exempt quests (no deadline / self-given) + comfortable
                // ones (Neutral or better reads as noise).
                let giver_note = if q.deadline_minutes > 0
                    && q.giver != "player"
                    && self.world_clock.current_minutes > q.deadline_minutes
                {
                    format!("{giver}, {band}")
                } else {
                    giver
                };
                let mut objectives: Vec<String> = Vec::new();
                for o in q.objectives.iter().take(QUEST_OBJECTIVES_SHOWN) {
                    let text: String =
                        o.text.chars().take(QUEST_OBJ_TEXT_CHARS).collect::<String>();
                    if o.total > 0 {
                        objectives.push(format!("{}/{} {}", o.cur, o.total, text));
                    } else if o.done {
                        objectives.push(format!("✓ {text}"));
                    } else {
                        objectives.push(text);
                    }
                }
                if q.objectives.len() > QUEST_OBJECTIVES_SHOWN {
                    objectives
                        .push(format!("(+{} more)", q.objectives.len() - QUEST_OBJECTIVES_SHOWN));
                }
                let title: String = q.title.chars().take(QUEST_TITLE_CHARS).collect();
                let mut entry = format!("{} ({})", title, giver_note);
                if !objectives.is_empty() {
                    entry.push_str(&format!(" — {}", objectives.join(", ")));
                }
                if !q.reward.is_empty() {
                    let reward: String = q.reward.chars().take(40).collect();
                    entry.push_str(&format!(", reward: {reward}"));
                }
                parts.push(entry);
            }
            let hidden = self.quests.len().saturating_sub(QUESTS_PROMPT_CAP);
            out.push_str("quests: ");
            out.push_str(&Self::flatten_inline(&parts.join("; ")));
            if hidden > 0 {
                out.push_str(&format!(" (+{hidden} more)"));
            }
            out.push('\n');
        }
        // (2026-08-19 Referee QoL) The `bonds:` line — PRESENT NPCs whose
        // relationship tier is LOUD (≥ Friendly on the affinity track or
        // ≤ Rival on the grudge track); the quiet middle
        // (Stranger/Acquaintance) stays silent. Cap 10 (2026-08-21 evening
        // follow-up to the 8192 ruling: 6 → 10), existing tier
        // glosses. Dormant when empty (zero tokens, always).
        if !self.relationships.is_empty() && !self.presences.is_empty() {
            const BONDS_PROMPT_CAP: usize = 10;
            let mut parts: Vec<String> = Vec::new();
            for p in &self.presences {
                if parts.len() >= BONDS_PROMPT_CAP {
                    break;
                }
                let Some(rel) = self.relationships.get(&p.npc_id) else {
                    continue;
                };
                if rel.tier < RelationshipTier::Friendly && rel.tier > RelationshipTier::Rival {
                    continue;
                }
                parts.push(format!("{} [{}] {}", p.name, rel.tier.tag(), rel.tier.label()));
            }
            if !parts.is_empty() {
                out.push_str("bonds: ");
                out.push_str(&Self::flatten_inline(&parts.join("; ")));
                out.push('\n');
            }
        }
        // (2026-08-20 Economy) The `ledger:` line — the owned-property
        // read (till, net/day, deficit marker, NPC-owner marker), capped +
        // flattened by `economy::render_ledger_line`. Dormant when no
        // properties exist (zero tokens, always — most cards carry none).
        // Jobs + lifestyle ride the `player_state:` block instead.
        if !self.properties.is_empty() {
            if let Some(ledger_line) = crate::economy::render_ledger_line(self) {
                out.push_str("ledger: ");
                out.push_str(&Self::flatten_inline(&ledger_line));
                out.push('\n');
            }
        }
        // (2026-08-21 economy addendum) The Rust-owned price ladder — the
        // anti-price-hallucination anchor. Deterministic values only
        // (lifestyle curve × current node prosperity); dormant while the
        // economy is (a fresh game still renders empty — the zero-token
        // invariant). Both the tracker (this skeleton) and the API narrator
        // (the turn tail carries this same world_state) price everyday
        // items against it instead of inventing sums. See
        // `economy::render_economy_anchor`.
        if let Some(anchor) = crate::economy::render_economy_anchor(self) {
            out.push_str(&anchor);
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
            // (#48) HARD-CAPPED at the first 10 heard rumors + a `(+N more)`
            // marker — rumor texts are full prose phrases (heavy), the list
            // grows monotonically via propagation, and this line rides every
            // prompt. (2026-08-21 evening follow-up to the 8192 ruling:
            // 6 → 10.)
            const RUMORS_PROMPT_CAP: usize = 10;
            let all_heard: Vec<&str> = self
                .rumors
                .iter()
                .filter(|r| r.known_nodes.iter().any(|n| n == cur_id))
                .map(|r| r.label.as_str())
                .collect();
            if !all_heard.is_empty() {
                let shown = &all_heard[..all_heard.len().min(RUMORS_PROMPT_CAP)];
                let mut line = shown.join("; ");
                let hidden = all_heard.len() - shown.len();
                if hidden > 0 {
                    line.push_str(&format!("; (+{hidden} more)"));
                }
                out.push_str("rumors: ");
                // (2026-08-24 review P2) flatten_inline — rumor labels are
                // free text; same gate as the other roster lines.
                out.push_str(&Self::flatten_inline(&line));
                out.push('\n');
            }
        }
        if !self.summary.trim().is_empty() {
            out.push_str("summary: ");
            // (yellow S7) flattened — see flatten_inline.
            out.push_str(&Self::flatten_inline(self.summary.trim()));
            out.push('\n');
        }
        // Cap recent events shown in chat at the last 6: older events live
        // in the persisted schema + memory retrieval, not the chat prompt.
        // (2026-08-21 evening follow-up to the 8192 ruling: 5 → 6.)
        let show_events = self.recent_events.len().saturating_sub(6);
        if !self.recent_events[show_events..].is_empty() {
            out.push_str("recent_events:\n");
            for ev in &self.recent_events[show_events..] {
                out.push_str("  - ");
                // (yellow S7) flattened — an event ending in a forged
                // `present:`-style continuation must not become a line.
                out.push_str(&Self::flatten_inline(ev));
                out.push('\n');
            }
        }
        // BOUNDED CARRY-BACK (2026-08-10): the prior "ENTITIES BLOCK REMOVED"
        // fix correctly killed the uncapped entity dump (every NPC tier, world
        // fact, and item detail wholesale) — that was the genuine overflow
        // driver. But going to ZERO carry-back left the tracker blind to three
        // things it must see to do its job: the NPC roster (so [PRESENCE] has
        // valid targets), the belt (so [BELT -x] is meaningful), and the pack
        // (so [PACK x] isn't a blind guess at what's already there). The
        // tracker lands on "emit nothing" when it can't see current state —
        // the 2026-08-10 playtest showed 6 brackets across 52 turns + a frozen
        // world.
        //
        // This block is the LEAN carry-back: only what the tracker cannot
        // infer from the 1-turn window + Rust anchors, each BOUNDED so it
        // cannot re-grow the overflow:
        //   - cast: the roster line (id list, no prose) — the [PRESENCE]
        //     whitelist source. Empty when no NPCs are registered.
        //   - belt: the 4-slot quick rack, names only (the [BELT] state).
        //     Empty when nothing's on the belt.
        //   - pouch: the wallet (2026-08-23 pouch ruling) — currency, coins,
        //     keys, ID, small valuables, names + qty, capped like the pack so
        //     a hoarded gem collection can't re-grow the prompt.
        //   - pack: the unbounded deep store, names + qty ONLY, HARD-CAPPED at
        //     the first 16 entries. Pack can grow large in long sessions; the
        //     cap keeps the line bounded. (Older entries live in the persisted
        //     schema + the inventory panel UI — not the prompt.)
        // Item tags/stats are deliberately NOT rendered here (they're authoring
        // noise for the tracker; the apply path keeps them on the items). The
        // narrator sees these too — it's legitimate observer knowledge (what
        // you carry + who's in the cast).
        if let Some(cast_line) = self.npc_registry.render_line() {
            out.push_str("cast: ");
            // (2026-08-24 review P2) flatten_inline — registry names are
            // free text; same newline-injection gate as present:/rumors:.
            out.push_str(&Self::flatten_inline(&cast_line));
            out.push('\n');
        }
        if !self.player_state.belt.is_empty() {
            // (2026-08-24 review fix) The belt renders under the SAME cap
            // discipline as pouch/pack — the quick rack is small in honest
            // play, but hand-edited saves + a drifted apply path can grow
            // it, and the `belt:` line was the one unbounded carry-back.
            const BELT_PROMPT_CAP: usize = 16;
            let shown: Vec<String> = self
                .player_state
                .belt
                .iter()
                .take(BELT_PROMPT_CAP)
                .map(|i| {
                    if i.qty > 1 {
                        format!("{} ×{}", i.name, i.qty)
                    } else {
                        i.name.clone()
                    }
                })
                .collect();
            let overflow = self.player_state.belt.len().saturating_sub(BELT_PROMPT_CAP);
            out.push_str("belt: ");
            // (2026-08-24 review P2) flatten_inline: item names from
            // hand-edited saves can carry newlines — a raw render could
            // inject fake `present:`/`clock:` lines into <world_state>.
            out.push_str(&Self::flatten_inline(&shown.join(", ")));
            if overflow > 0 {
                out.push_str(&format!(" (+{overflow} more)"));
            }
            out.push('\n');
        }
        // (2026-08-23 pouch ruling) The wallet line — the same read-back
        // discipline as the pack (the tracker must see current coin/keys/ID to
        // restate or remove them), bounded by the same cap.
        if !self.player_state.pouch.is_empty() {
            const POUCH_PROMPT_CAP: usize = 16;
            let shown: Vec<String> = self
                .player_state
                .pouch
                .iter()
                .take(POUCH_PROMPT_CAP)
                .map(|i| {
                    if i.qty > 1 {
                        format!("{} ×{}", i.name, i.qty)
                    } else {
                        i.name.clone()
                    }
                })
                .collect();
            let overflow = self.player_state.pouch.len().saturating_sub(POUCH_PROMPT_CAP);
            out.push_str("pouch: ");
            // (2026-08-24 review P2) flatten_inline — same newline-injection
            // gate as belt/pack/quests (hand-edited saves).
            out.push_str(&Self::flatten_inline(&shown.join(", ")));
            if overflow > 0 {
                out.push_str(&format!(" (+{} more)", overflow));
            }
            out.push('\n');
        }
        if !self.player_state.pack.is_empty() {
            // (2026-08-21 evening follow-up to the 8192 ruling: 12 → 16.)
            const PACK_PROMPT_CAP: usize = 16;
            let shown: Vec<String> = self
                .player_state
                .pack
                .iter()
                .take(PACK_PROMPT_CAP)
                .map(|i| {
                    if i.qty > 1 {
                        format!("{} ×{}", i.name, i.qty)
                    } else {
                        i.name.clone()
                    }
                })
                .collect();
            let overflow = self.player_state.pack.len().saturating_sub(PACK_PROMPT_CAP);
            out.push_str("pack: ");
            // (2026-08-24 review P2) flatten_inline — the belt/pouch twin.
            out.push_str(&Self::flatten_inline(&shown.join(", ")));
            if overflow > 0 {
                out.push_str(&format!(" (+{} more)", overflow));
            }
            out.push('\n');
        }
        // Custom extensions (2026-08-13): authored key→value pairs from the
        // card's `<custom_tags>` + the attached player's `custom_tags` (stats,
        // faction standings, curses, currencies — anything that doesn't fit a
        // standard field). BOUNDED like the pack line so a large map can't
        // re-grow the prompt; entries beyond the cap are summarized as
        // `(+N more)`. Empty map → no line (zero tokens for a fresh game).
        if !self.custom_tags.is_empty() {
            // (2026-08-21 evening follow-up to the 8192 ruling: 12 → 16.)
            const CUSTOM_PROMPT_CAP: usize = 16;
            let shown: Vec<String> = self
                .custom_tags
                .iter()
                .take(CUSTOM_PROMPT_CAP)
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            let overflow = self.custom_tags.len().saturating_sub(CUSTOM_PROMPT_CAP);
            out.push_str("custom: ");
            // (2026-08-24 review P2) flatten_inline — authored tag values are
            // free text; same gate as the pack/pouch lines.
            out.push_str(&Self::flatten_inline(&shown.join("; ")));
            if overflow > 0 {
                out.push_str(&format!(" (+{} more)", overflow));
            }
            out.push('\n');
        }
        // Player state (the Rust Referee's canonical fact block). Rendered
        // LAST in the world-state block so it's the loudest signal — the
        // player's injuries + fatigue are the most turn-relevant facts.
        // Returns None when fully default, so a fresh game adds zero tokens.
        if let Some(player_block) = self.player_state.render_for_prompt_with_beneath(reveal_beneath, &self.currency_label) {
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

    /// Serialize for the delta pass's "current schema" prompt input —
    /// PROMPT-SHAPED, not save-shaped (2026-08-16 audit H2). The old
    /// `to_json_pretty` fold serialized the ENTIRE struct; at the
    /// long-campaign growth caps (entities ≤ 500 with `entity_order`
    /// re-serializing every key a second time as pure bookkeeping, all 50
    /// stored `recent_events` instead of the rendered window, the
    /// deliberately-unbounded pack riding inside `player_state`) that is
    /// 5-10× the 1792-token prompt budget — the middle-drop then goes
    /// permanently active and the delta model diffs against a flickering
    /// head+tail subset of its own schema, re-minting duplicate entities
    /// and missing mid-list mutations. This was precisely the failure the
    /// growth caps were built to kill, one layer down.
    ///
    /// Carries ONLY what the schema-engine passes read or write:
    /// `summary`, the last `EVENTS_PROMPT_CAP` `recent_events` (the same
    /// window `render_for_prompt` shows the narrator), and `entities`
    /// (BTreeMap → sorted keys, the diff target — deterministic across
    /// turns so the middle-drop subset can't flicker). Rust-owned referee
    /// fields (`player_state`, `world_clock`, `weather`, `travel_graph`,
    /// `status_tags`, …) are omitted: no schema-engine pass may write
    /// them, so they are dead prompt weight (Prime Mandate). Compact JSON
    /// — no pretty-print indentation. A single entity value whose
    /// serialized form exceeds `ENTITY_VALUE_PROMPT_CHARS` collapses to a
    /// marker string so one giant blob can't eat the budget (the model may
    /// still overwrite or null-delete the key).
    ///
    /// (2026-08-24 bug fix) The char budget bounds the WHOLE document —
    /// summary + events + entities TOGETHER. The old accounting spent the
    /// entire budget on entities and let the envelope ride unaccounted: a
    /// legal-max summary (4,000 chars) + 6 legal-max events (1,000 each)
    /// added ~10k invisible chars on top, and the composed schema-engine
    /// prompt re-blew the CTX_SCHEMA prompt ceiling (the middle-drop
    /// failure the budget exists to kill). The envelope now renders FIRST
    /// (flattened + prompt-capped, the `ENTITY_VALUE_PROMPT_CHARS`
    /// discipline — the write-time gates stay authoritative), is measured
    /// in its serialized form, and entities spend the REMAINDER — with a
    /// floor so entities never fully starve on a fat envelope.
    pub fn to_json_prompt(&self) -> String {
        const EVENTS_PROMPT_CAP: usize = 6;
        const ENTITY_VALUE_PROMPT_CHARS: usize = 400;
        // Render-time caps for the envelope pieces (see the doc note above).
        // Summary 1,200 + events 6×250 + framing ≈ 2.8k worst — the
        // remaining ~1.2k goes to entities in the fattest legal envelope.
        const SUMMARY_PROMPT_CHARS: usize = 1_200;
        const EVENT_PROMPT_CHARS: usize = 250;
        // Entities floor: however fat the (capped) envelope, entities keep
        // at least this much so the diff target never empties. With the
        // caps above the envelope maxes ~2.8k, so envelope_max + floor
        // stays under the total budget — the floor is headroom for future
        // cap changes, not a path past the budget.
        const ENTITIES_FLOOR_CHARS: usize = 1_000;
        let budget = crate::settings::SCHEMA_JSON_PROMPT_BUDGET_CHARS;
        // Envelope first — flattened (the yellow S7 forgery gate), then
        // prompt-capped with the visible `[…]` marker when a legal-max or
        // hand-edited value overflows its share.
        let summary = Self::flatten_inline(&self.summary);
        let summary = if summary.chars().count() > SUMMARY_PROMPT_CHARS {
            let mut cut: String = summary.chars().take(SUMMARY_PROMPT_CHARS).collect();
            cut.push_str(" […]");
            cut
        } else {
            summary
        };
        let event_window: &[String] = if self.recent_events.len() > EVENTS_PROMPT_CAP {
            &self.recent_events[self.recent_events.len() - EVENTS_PROMPT_CAP..]
        } else {
            &self.recent_events
        };
        let events: Vec<String> = event_window
            .iter()
            .map(|e| {
                let flat = Self::flatten_inline(e);
                if flat.chars().count() > EVENT_PROMPT_CHARS {
                    let mut cut: String = flat.chars().take(EVENT_PROMPT_CHARS).collect();
                    cut.push_str(" […]");
                    cut
                } else {
                    flat
                }
            })
            .collect();
        // The envelope's serialized cost, measured the way serde will
        // actually emit it (quotes + escapes included), plus the fixed
        // `{"summary":…,"recent_events":[…],"entities":…}` framing.
        let envelope_cost = serde_json::to_string(&summary)
            .map(|s| s.chars().count())
            .unwrap_or(2)
            + events
                .iter()
                .map(|e| serde_json::to_string(e).map(|s| s.chars().count()).unwrap_or(2))
                .sum::<usize>()
            + events.len()
            + 64;
        let entities_budget = budget
            .saturating_sub(envelope_cost)
            // The floor can never push past the total budget: an escape-
            // dense envelope (every quote doubled in the serde cost) can
            // starve the remainder below ENTITIES_FLOOR_CHARS, and a bare
            // .max(FLOOR) then OVERSHOT the budget it exists to protect.
            .max(ENTITIES_FLOOR_CHARS.min(budget));
        // Deterministic inclusion order (2026-08-16 yellow S4): priority =
        // `player.*` identity keys first (the diff anchor, never many), then
        // `entity_order` (FIFO = oldest first, the same recency assumption
        // eviction uses), then any keys the order list never knew (sorted —
        // deterministic fallback).
        let mut order: Vec<&String> = Vec::with_capacity(self.entities.len());
        let mut seen: std::collections::HashSet<&String> = std::collections::HashSet::new();
        for k in self.entities.keys() {
            if k.starts_with("player.") {
                order.push(k);
                seen.insert(k);
            }
        }
        for k in &self.entity_order {
            if self.entities.contains_key(k) && seen.insert(k) {
                order.push(k);
            }
        }
        for k in self.entities.keys() {
            if seen.insert(k) {
                order.push(k);
            }
        }
        let mut entities: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        let mut used = 0usize;
        let mut trimmed = 0usize;
        for k in order {
            let v = &self.entities[k];
            let compact = serde_json::to_string(v).unwrap_or_default();
            let value = if compact.chars().count() <= ENTITY_VALUE_PROMPT_CHARS {
                v.clone()
            } else {
                serde_json::Value::String("<long value omitted>".to_string())
            };
            let rendered = serde_json::to_string(&value).unwrap_or_default();
            // +2 covers the `"key":` + `,` framing serde adds per entry.
            let entry_cost = k.len() + rendered.chars().count() + 8;
            if used + entry_cost > entities_budget {
                trimmed = self.entities.len() - entities.len();
                break;
            }
            used += entry_cost;
            entities.insert(k.clone(), value);
        }
        let mut obj = serde_json::json!({
            "summary": summary,
            "recent_events": events,
            "entities": entities,
        });
        if trimmed > 0 {
            tracing::warn!(
                total = self.entities.len(),
                shown = self.entities.len() - trimmed,
                entities_budget_chars = entities_budget,
                "schema prompt JSON over budget; oldest entities trimmed (player.* identity keys kept)"
            );
            obj.as_object_mut()
                .expect("json! object")
                .insert("entities_trimmed".into(), serde_json::json!(trimmed));
        }
        serde_json::to_string(&obj).unwrap_or_else(|_| "{}".to_string())
    }

    /// Full-struct pretty serialization. NOT a prompt input — the whole
    /// struct easily outgrows the schema-engine prompt budget (see
    /// [`WorldSchema::to_json_prompt`]); use that for any prompt-shaped
    /// consumer. This is the save/debug/manager-query surface.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// (2026-08-16 audit LOW) Clamp the typed referee-owned collections to
    /// their growth caps after a `merge_patch` full-replace. The dispatch
    /// pre-filter is the primary gate (model patches never reach these
    /// fields), but the mutation fn itself must not install what the bracket
    /// appliers would refuse — the raw-editor JSON tab + any future caller
    /// route through here too. Vec fields truncate FIFO (oldest dropped);
    /// the relationships map pins the tier extremes (Trusted/Bonded/
    /// Nemesis/Hostile — the same bands `npc_is_reaper_protected` holds)
    /// and only drops mid-band entries, in sorted-key order
    /// (deterministic); a travel graph over the node cap is a hard error
    /// (refusing an authored hub beats silently dropping it — checked at
    /// the install arm, not here).
    /// (2026-08-20 audit) Bring `property_order` back in step with
    /// `properties`: prune ids whose property died (seizure, cap trim,
    /// hand-edited removal), append properties the order vec has never seen
    /// (legacy saves backfill in deterministic BTreeMap key order; live
    /// inserts push in arrival order at the applier/seed sites). Idempotent;
    /// pure bookkeeping — no mutation of `properties` itself.
    pub fn reconcile_property_order(&mut self) {
        if !self.property_order.is_empty() || !self.properties.is_empty() {
            // Owned copy — the retain closure then borrows nothing of self.
            let live: Vec<String> = self.properties.keys().cloned().collect();
            self.property_order.retain(|id| live.iter().any(|l| l == id));
        }
        if self.property_order.len() < self.properties.len() {
            for key in self.properties.keys() {
                if !self.property_order.iter().any(|o| o.as_str() == key.as_str()) {
                    self.property_order.push_back(key.clone());
                }
            }
        }
    }

    pub fn enforce_typed_caps(&mut self) {
        let tag_cap = crate::settings::FABLE_STATUS_TAG_CAP;
        if self.status_tags.len() > tag_cap {
            let overflow = self.status_tags.len() - tag_cap;
            self.status_tags.drain(..overflow);
        }
        if self.offscreen_tasks.len() > MAX_STORED_TASKS {
            let overflow = self.offscreen_tasks.len() - MAX_STORED_TASKS;
            self.offscreen_tasks.drain(..overflow);
        }
        if self.rumors.len() > MAX_STORED_RUMORS {
            let overflow = self.rumors.len() - MAX_STORED_RUMORS;
            self.rumors.drain(..overflow);
        }
        if self.relationships.len() > MAX_TRACKED_RELATIONSHIPS {
            // (2026-08-18 relation-to-player ruling) Cap truncation is
            // TIER-AWARE, never arbitrary: the extremes (Trusted/Bonded on
            // the affinity track, Nemesis/Hostile on the grudge track — the
            // same bands the reaper protection pins) are NEVER dropped; only
            // mid-band entries fall, sorted-key first for determinism. A
            // pathological save whose extremes alone exceed the cap keeps
            // them all — the cap is a bloat guard, not a family-separation
            // law. Dropped ids surface via a tracing warn (wupi.log + the
            // logs/ mirror) so the truncation stays auditable.
            let pinned: std::collections::HashSet<String> = self
                .relationships
                .iter()
                .filter(|(_, rel)| {
                    rel.tier >= RelationshipTier::Trusted
                        || rel.tier <= RelationshipTier::Hostile
                })
                .map(|(k, _)| k.clone())
                .collect();
            let pinned_count = pinned.len();
            let mut keep = pinned.clone();
            for k in std::collections::BTreeSet::from_iter(self.relationships.keys().cloned()) {
                if keep.len() >= MAX_TRACKED_RELATIONSHIPS {
                    break;
                }
                if !pinned.contains(&k) {
                    keep.insert(k);
                }
            }
            let dropped: Vec<String> = self
                .relationships
                .keys()
                .filter(|k| !keep.contains(*k))
                .cloned()
                .collect();
            if !dropped.is_empty() {
                const DROPPED_SHOWN: usize = 8;
                let mut list = dropped[..dropped.len().min(DROPPED_SHOWN)].join(", ");
                if dropped.len() > DROPPED_SHOWN {
                    list.push_str(&format!(" (+{} more)", dropped.len() - DROPPED_SHOWN));
                }
                tracing::warn!(
                    cap = MAX_TRACKED_RELATIONSHIPS,
                    pinned = pinned_count,
                    dropped = %list,
                    "relationship cap truncation: mid-band entries fell"
                );
            }
            self.relationships.retain(|k, _| keep.contains(k));
        }
        // (2026-08-18 Dedicated-NPC interior state) Orphan sweep + item cap:
        // interiors for ids no longer in the registry (hand-edited registry
        // installs, evicted entries) are grime — drop them; each surviving
        // interior's item rack caps FIFO like the belt. The map itself needs
        // no separate cap — it is bounded by the registry's own 96.
        let registry_ids: std::collections::HashSet<String> = self
            .npc_registry
            .entries
            .iter()
            .map(|e| e.id.clone())
            .collect();
        self.npc_interior.retain(|id, _| registry_ids.contains(id));
        for interior in self.npc_interior.values_mut() {
            if interior.items.len() > NPC_INTERIOR_ITEMS_MAX {
                let overflow = interior.items.len() - NPC_INTERIOR_ITEMS_MAX;
                interior.items.drain(..overflow);
            }
        }
        // (2026-08-19 Referee QoL) Open-promise cap — FIFO like the tasks.
        if self.promises.len() > MAX_PROMISES {
            let overflow = self.promises.len() - MAX_PROMISES;
            self.promises.drain(..overflow);
        }
        // (2026-08-22 living-world) Open-quest cap — FIFO like the promises
        // (the bracket applier refuses at the same ceiling; this is the
        // hand-edited-save backstop).
        if self.quests.len() > MAX_QUESTS {
            let overflow = self.quests.len() - MAX_QUESTS;
            self.quests.drain(..overflow);
        }
        // (2026-08-20 Economy; 2026-08-20 audit rework) Property cap — TRUE
        // FIFO by `property_order` (first-insert), not BTreeMap key order
        // (the old `keys().take(overflow)` dropped alphabetically-first ids,
        // which only matched "oldest" when insertion order was alphabetical
        // — the original test passed by accident). Reaching here means a
        // hand-edited install (the bracket applier refuses at the same
        // ceiling), so the order vec is reconciled first: legacy saves
        // backfill deterministically (key order), dead ids prune, unseen
        // ids append.
        if self.properties.len() > crate::economy::MAX_PROPERTIES {
            self.reconcile_property_order();
            let overflow = self.properties.len() - crate::economy::MAX_PROPERTIES;
            let dropped: Vec<String> = self
                .property_order
                .iter()
                .take(overflow)
                .cloned()
                .collect();
            for id in &dropped {
                self.properties.remove(id);
                self.property_order.retain(|o| o != id);
            }
            tracing::warn!(
                cap = crate::economy::MAX_PROPERTIES,
                dropped = ?dropped,
                "property cap truncation: first-inserted entries fell (FIFO)"
            );
        }
        // (2026-08-19 Hidden site maps) LRU eviction at the map cap — never
        // the current node's map. A WHILE, not one call: a hand-edited
        // install can sit over the cap by 2+ maps and must converge in a
        // single pass (the None break guards the degenerate all-current case).
        while self.site_maps.len() > crate::site_map::MAX_SITE_MAPS {
            let current = self
                .travel_graph
                .current_node
                .clone()
                .unwrap_or_default();
            if crate::site_map::evict_lru_site_map(&mut self.site_maps, &current).is_none() {
                break;
            }
        }
        self.cap_recent_events();
    }

    /// (2026-08-18 reaper follow-up — Chloe's left-behind-family catch)
    /// DERIVED reaper protection: is this NPC load-bearing in the live
    /// world state RIGHT NOW? Evaluated fresh at every reap/evict — never
    /// stored, so it can never go stale the way a static tag would (the
    /// discovered shopkeeper's daughter you MARRIED mid-campaign stays
    /// full-state while you're away, because the marriage milestone put her
    /// at `Bonded`, not because anyone remembered to flip a flag). Signals:
    ///
    /// 1. Relationship tier EXTREMES — `Trusted`/`Bonded` are the
    ///    mentor/lover/companion bands (unreachable without recorded
    ///    `[MILESTONE]` events — the promotion IS the milestone ladder),
    ///    `Nemesis`/`Hostile` are the grudge band (a villain whose revenge
    ///    archives after 30 days is the feature eating itself). The mid
    ///    bands (Stranger→Friendly, Rival) stay archivable — a merchant you
    ///    liked is allowed to become a memory.
    /// 2. An unresolved off-screen `[TASK]` — the world is waiting on them.
    /// 3. A non-empty item rack — quest-item holders, and the NPC who
    ///    STOLE something and left town (you return weeks later to reclaim
    ///    it; the goods must still be there).
    ///
    /// Deliberately NOT a signal: home-node ownership (no per-NPC residence
    /// structure exists in the schema; the tier ladder already covers the
    /// family case). Bounded: relationships cap at 48 + tasks at 20, so the
    /// protected set can't outgrow those caps plus item-holders.
    pub fn npc_is_reaper_protected(&self, id: &str) -> bool {
        self.reaper_protection_reason(id).is_some()
    }

    /// The WHY behind [`npc_is_reaper_protected`], surfaced for the
    /// diagnostics log (BRK): which live-world signal held the NPC out of
    /// the reaper's hands — `rel:<tier>` (the relationship-to-player
    /// extremes), `open-task`, or `holds-items`. One source of truth with
    /// the bool; the reap/evict paths log it so a playtest log can verify
    /// the relation-to-player band actually fired.
    pub fn reaper_protection_reason(&self, id: &str) -> Option<String> {
        if let Some(rel) = self.relationships.get(id) {
            // Ord runs worst→best: Nemesis < Hostile < ... < Trusted < Bonded.
            // Protect both extremes, archive the mid-band.
            if rel.tier >= RelationshipTier::Trusted || rel.tier <= RelationshipTier::Hostile {
                return Some(format!("rel:{}", rel.tier.tag()));
            }
        }
        if self
            .offscreen_tasks
            .iter()
            .any(|t| t.npc_id == id && !t.resolved)
        {
            return Some("open-task".into());
        }
        if self
            .npc_interior
            .get(id)
            .map(|i| !i.items.is_empty())
            .unwrap_or(false)
        {
            return Some("holds-items".into());
        }
        None
    }

    /// (2026-08-18 Dedicated-NPC reaper — Chloe's Garbage-Collector ruling)
    /// Archive stale `named` NPCs' interior state on the world tick. `core`
    /// (authored cast) is NEVER reaped, and neither is any NPC passing
    /// `npc_is_reaper_protected` (the derived left-behind-family guard —
    /// relationship extremes, pending tasks, held items). A `named` NPC
    /// with no contact for `settings::NPC_REAP_NAMED_AFTER_DAYS` in-world
    /// days has its rich fields (mood/intent/items) compressed into a
    /// one-line `archived` stub — the registry entry survives
    /// (`[PRESENCE]` still resolves, `cast:` still lists them), the interior
    /// weight drops to the stub. A returning NPC's next
    /// `[MOOD]`/`[INTENT]`/`[NPC_ITEM]` emission simply overwrites the stub
    /// with live state. Present NPCs are skipped (on-camera = contacted —
    /// never reap the scene you're in), and a `last_seen_minutes` of 0
    /// (dormant clock / pre-interior save) is "do not measure", never
    /// "instantly stale". Logs each archive + the past-TTL held-back set
    /// under BRK (id + protection reason — a `rel:` hold is the visible
    /// proof the relation-to-player band fired). Returns the reap count;
    /// otherwise pure, runs under the schema lock inside the tick's
    /// one-snapshot discipline.
    pub fn reap_stale_npc_interiors(&mut self, now_minutes: i64) -> usize {
        if now_minutes <= 0 {
            return 0;
        }
        let ttl_minutes: i64 = crate::settings::NPC_REAP_NAMED_AFTER_DAYS as i64 * 1440;
        let present: std::collections::HashSet<String> =
            self.presences.iter().map(|p| p.npc_id.clone()).collect();
        let named_ids: Vec<String> = self
            .npc_registry
            .entries
            .iter()
            .filter(|e| e.prominence == NpcProminence::Named)
            .map(|e| e.id.clone())
            .collect();
        let mut reaped = 0usize;
        let mut held: Vec<String> = Vec::new();
        for id in named_ids {
            if present.contains(&id) {
                continue;
            }
            // TTL-first eligibility: only an interior the reaper could
            // actually act on this tick (past the no-contact TTL, still
            // live) reaches the protection check below — so the held-back
            // log fires only when truncation was genuinely on the table,
            // never as per-tick noise from healthy NPCs.
            let eligible = {
                let Some(interior) = self.npc_interior.get(&id) else {
                    continue;
                };
                interior.archived.is_none()
                    && interior.last_seen_minutes > 0
                    && now_minutes - interior.last_seen_minutes >= ttl_minutes
            };
            if !eligible {
                continue;
            }
            // Derived protection (the mutable world-state prominence): a
            // Bonded spouse, a Nemesis, a pending-task holder, or an NPC
            // still holding items is never truncated. Checked fresh at reap
            // time — before the mutable borrow, since the predicate reads
            // across schema fields.
            if let Some(reason) = self.reaper_protection_reason(&id) {
                held.push(format!("{id}: {reason}"));
                continue;
            }
            let Some(interior) = self.npc_interior.get_mut(&id) else {
                continue;
            };
            let stub = interior.compose_archive_stub();
            interior.mood = None;
            interior.intent = None;
            interior.items.clear();
            interior.worn.clear();
            interior.archived = Some(stub.clone());
            reaped += 1;
        }
        if reaped > 0 {
            tracing::info!(
                reaped,
                "reaper: stale named-NPC interiors archived (rich fields compressed to stubs)"
            );
        }
        if !held.is_empty() {
            const HELD_SHOWN: usize = 8;
            let mut list = held[..held.len().min(HELD_SHOWN)].join(", ");
            if held.len() > HELD_SHOWN {
                list.push_str(&format!(" (+{} more)", held.len() - HELD_SHOWN));
            }
            // (2026-08-24 review fix) The held-back set was computed and then
            // dropped — the dead diagnostic this block replaces. This line is
            // the playtest evidence that the protection bands (rel extremes /
            // open tasks / held items) actually fire against past-TTL NPCs.
            tracing::debug!(
                held = %list,
                "reaper: past-TTL named NPCs held back by live-world protection"
            );
        }
        reaped
    }

    /// (2026-08-18 registry-cap amendment — plan-approved) Relieve
    /// `MAX_NPC_REGISTRY` pressure by evicting the single
    /// least-recently-seen ARCHIVED, DISCOVERED (`named`), non-present
    /// registry entry — plus its interior row and any lingering presence.
    /// Authored `core` entries are pinned forever (the §5 ruling's fear —
    /// losing an authored hub — is structurally preserved); live `named`
    /// NPCs are safe too (only reaper-archived stales qualify). This is the
    /// fix for the 96-cap brick: a long campaign that met too many people
    /// used to have discovery refuse permanently once 96 ids accumulated.
    /// Relationships are deliberately KEPT for the evicted id (capped
    /// independently at 48) — a re-registered returning NPC picks its old
    /// tier back up. Returns the evicted id, if any; the caller
    /// (`[NPC_REGISTER]` applier at cap) retries the upsert once on Some.
    pub fn evict_archived_registry_entry(&mut self) -> Option<String> {
        let present: std::collections::HashSet<String> =
            self.presences.iter().map(|p| p.npc_id.clone()).collect();
        // (2026-08-18 reaper follow-up) Protected ids are collected FIRST —
        // `npc_is_reaper_protected` needs &self, the eviction pass below
        // mutates. Belt-and-braces: the reaper never archives a protected
        // NPC, so this only matters for hand-edited saves that pre-set an
        // `archived` stub on someone who is now load-bearing.
        let protected: std::collections::HashSet<String> = self
            .npc_registry
            .entries
            .iter()
            .map(|e| e.id.clone())
            .filter(|id| self.npc_is_reaper_protected(id))
            .collect();
        let mut candidate: Option<(String, i64)> = None;
        for e in &self.npc_registry.entries {
            if e.prominence != NpcProminence::Named {
                continue;
            }
            if present.contains(&e.id) || protected.contains(&e.id) {
                continue;
            }
            let Some(interior) = self.npc_interior.get(&e.id) else {
                continue;
            };
            if interior.archived.is_none() {
                continue; // live NPC — not evictable
            }
            let last = interior.last_seen_minutes;
            if candidate.as_ref().map_or(true, |(_, l)| last < *l) {
                candidate = Some((e.id.clone(), last));
            }
        }
        let (id, _) = candidate?;
        self.npc_registry.entries.retain(|e| e.id != id);
        self.npc_interior.remove(&id);
        self.presences.retain(|p| p.npc_id != id);
        Some(id)
    }

    /// Atomic save to `world_schema.json` (temp + fsync + rename, same pattern
    /// as `session::Conversation::save`). A crash mid-write can never truncate
    /// the existing file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        atomic_write_text(path, &json)
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

    /// The per-card split persistence (2026-08-01 Fable folder reorg). The
    /// in-memory `WorldSchema` stays one struct (the engine + Referees read it
    /// as such); the disk form splits into three sibling files inside the
    /// card's folder so the Player / World / NPC tabs each own one file:
    ///
    ///   • `world.json`  — world fields + non-npc entities + clock/weather/
    ///     travel/rumors/scene_pacing/status_tags/immutable_keys.
    ///   • `player.json` — the `player_state` subtree (body, stamina, wealth,
    ///     reputation).
    ///   • `npc.json`    — `npc.*` entities + `npc_registry` + `relationships`
    ///     + `presences` + `offscreen_tasks`.
    ///
    /// Implemented at the `serde_json::Value` level (partition a serialized
    /// object by key; `entities` is split by the `npc.` prefix). `load_split`
    /// is the inverse: read three files, deep-merge their `entities`, deserialize
    /// the merged object back into one `WorldSchema`. A missing file = that
    /// slice's keys deserialize to their `#[serde(default)]` (so a card with no
    /// `player.json` yet loads a fully-healthy default body — same contract as
    /// the old single-file load).
    pub fn save_split(
        &self,
        world_path: &Path,
        player_path: &Path,
        npc_path: &Path,
    ) -> std::io::Result<()> {
        // Ensure the card folder exists (sibling files share a parent).
        if let Some(parent) = world_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let full = serde_json::to_value(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let Some(obj) = full.as_object().cloned() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "world schema serialized to non-object",
            ));
        };

        let mut world = obj;
        let mut player = serde_json::Map::new();
        let mut npc = serde_json::Map::new();

        // player_state → player.json
        if let Some(ps) = world.remove("player_state") {
            player.insert("player_state".to_string(), ps);
        }

        // NPC-grouped fields → npc.json
        for key in [
            "npc_registry",
            "relationships",
            "presences",
            "offscreen_tasks",
            "npc_interior",
            "promises",
        ] {
            if let Some(v) = world.remove(key) {
                npc.insert(key.to_string(), v);
            }
        }

        // Partition entities by the `npc.` prefix: npc.* → npc.json, else world.json.
        if let Some(entities_val) = world.remove("entities") {
            if let Some(entities) = entities_val.as_object().cloned() {
                let mut world_ent = serde_json::Map::new();
                let mut npc_ent = serde_json::Map::new();
                for (k, v) in entities {
                    if k.starts_with("npc.") {
                        npc_ent.insert(k, v);
                    } else {
                        world_ent.insert(k, v);
                    }
                }
                // Only re-insert a (possibly empty) entities object when it's
                // non-empty, so an absent slice deserializes to the default
                // empty map on load (no `entities: {}` noise on disk).
                if !world_ent.is_empty() {
                    world.insert("entities".to_string(), world_ent.into());
                }
                if !npc_ent.is_empty() {
                    npc.insert("entities".to_string(), npc_ent.into());
                }
            } else {
                // entities wasn't an object (shouldn't happen) — preserve as-is.
                world.insert("entities".to_string(), entities_val);
            }
        }

        // (2026-08-16 deferred-3) Stamp the SAME generation into all three
        // files — `self.split_gen + 1` (the struct field round-trips through
        // world.json, so the counter is monotonic across saves without
        // needing &mut self here). Overwrites the stale struct-field copy in
        // `world` so the next load picks up the advanced value.
        let split_gen = self.split_gen + 1;
        for map in [&mut world, &mut player, &mut npc] {
            map.insert(
                "split_gen".to_string(),
                serde_json::Value::Number(split_gen.into()),
            );
        }

        // (2026-08-15 audit fix) STAGED trio write: serialize + write all
        // three temp files FIRST, then rename them back-to-back. The old
        // sequential per-file atomic writes left a seconds-wide window where
        // a crash produced mixed-generation siblings (a new world.json next
        // to a stale player.json — both individually valid JSON, so the
        // corrupt-file guard couldn't catch the combination). Staging shrinks
        // the cross-file window to the rename sequence; a crash mid-write
        // (2026-08-16 audit fix #13) Each of the three temps is UNIQUE per
        // write — racing `save_schema` callers each stage their own trio;
        // a crashed writer's stale temps are inert grime (never loaded, and
        // nothing renames over them).
        let world_json = serde_json::to_string_pretty(&world)?;
        let player_json = serde_json::to_string_pretty(&player)?;
        let npc_json = serde_json::to_string_pretty(&npc)?;
        let world_tmp = temp_path_for(world_path);
        let player_tmp = temp_path_for(player_path);
        let npc_tmp = temp_path_for(npc_path);
        for (tmp, body) in [
            (&world_tmp, &world_json),
            (&player_tmp, &player_json),
            (&npc_tmp, &npc_json),
        ] {
            let mut file = std::fs::File::create(tmp)?;
            std::io::Write::write_all(&mut file, body.as_bytes())?;
            std::io::Write::flush(&mut file)?;
            let _ = file.sync_all();
        }
        std::fs::rename(&world_tmp, world_path)?;
        std::fs::rename(&player_tmp, player_path)?;
        let r = std::fs::rename(&npc_tmp, npc_path);
        if r.is_err() {
            // Never leave THIS write's temps behind as grime on failure.
            let _ = std::fs::remove_file(&world_tmp);
            let _ = std::fs::remove_file(&player_tmp);
            let _ = std::fs::remove_file(&npc_tmp);
        }
        r
    }

    /// Human-readable JSON type name for error messages ("array", "string", …).
fn type_name_of_value(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// (2026-08-29 module F2) The current save-heal generation. Bump when a new
/// heal action is added to `heal_schema_state`; schemas below it heal on
/// their next load. v1 shipped with the 2026-08-29 hardening pass.
pub const HEAL_VERSION: u32 = 1;

/// (2026-08-29 modules E1+F2) The one-shot SAVE HEAL — repairs pre-fix
/// saves/sessions IN PLACE (no new game required). Idempotent +
/// conservative: every action only ever REMOVES garbage or backfills EMPTY
/// fields, never touches authored state, and each action INFO-logs only
/// when it actually changed something. Runs at `load_split` completion and
/// save-slot load, gated on `heal_version < HEAL_VERSION`; the version
/// persists with the next normal autosave/mutation save — no dedicated
/// write on load.
///
/// v1 actions:
/// - NODE SETTING BACKFILL (E1): every travel node with an empty `setting`
///   gets `infer_node_setting(name)` — pre-H2 saves minted `""` everywhere
///   and the JIT site architect never fired (whole campaigns ran mapless).
/// - GARBAGE-ITEM SWEEP (B4): the player racks (belt/pack/pouch) + NPC
///   interior racks lose entries whose names fail the B4 gates —
///   verb-tailed prose fragments ("tobacco smell came", "pipe turned"),
///   embedded-`+` merges ("Adopolous +Pipe"), person/rank tails.
///
/// The AUTHORED-KIT CLAMP is NOT here — it needs the bound SavedPlayer and
/// runs at session entry (`clamp_player_kit_to_authored`, lib.rs).
pub fn heal_schema_state(schema: &mut WorldSchema) {
    if schema.heal_version >= Self::HEAL_VERSION {
        return;
    }
    // E1: node setting backfill.
    let mut backfilled = 0usize;
    for node in &mut schema.travel_graph.nodes {
        if node.setting.is_empty() {
            let inferred = infer_node_setting(&node.name);
            if !inferred.is_empty() {
                node.setting = inferred.to_string();
                backfilled += 1;
            }
        }
    }
    if backfilled > 0 {
        tracing::info!(
            count = backfilled,
            "save heal v1: backfilled empty node settings (JIT architect re-armed)"
        );
    }
    // B4: garbage-item sweep (player racks + NPC interior racks).
    let is_garbage = |name: &str| {
        crate::bracket_parser::item_name_is_verb_garbage(name)
            || crate::bracket_parser::split_embedded_item_names(name).len() > 1
            || crate::item_name_looks_like_person(name)
    };
    let mut swept = 0usize;
    let ps = &mut schema.player_state;
    for rack in [&mut ps.belt, &mut ps.pack, &mut ps.pouch] {
        let before = rack.len();
        rack.retain(|i| !is_garbage(&i.name));
        swept += before - rack.len();
    }
    for interior in schema.npc_interior.values_mut() {
        for rack in [&mut interior.items, &mut interior.worn] {
            let before = rack.len();
            rack.retain(|i| !is_garbage(&i.name));
            swept += before - rack.len();
        }
    }
    if swept > 0 {
        tracing::info!(
            count = swept,
            "save heal v1: swept garbage item names from racks"
        );
    }
    schema.heal_version = Self::HEAL_VERSION;
}

/// The inverse of [`save_split`]: read the three sibling files, deep-merge
    /// their `entities` maps, and deserialize the merged object into one
    /// `WorldSchema`. Any missing file contributes nothing (its keys fall back
    /// to `#[serde(default)]`), so a pre-split save (only `world.json` exists)
    /// loads cleanly — the player + npc slices default in.
    pub fn load_split(
        world_path: &Path,
        player_path: &Path,
        npc_path: &Path,
    ) -> std::io::Result<Self> {
        let mut merged: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        // (2026-08-16 deferred-3) Generation stamps seen across the trio:
        // None = legacy file (unstamped). A trio whose PRESENT stamps disagree
        // is a mixed-generation Frankenstein (crash between save_split's
        // back-to-back renames) — every file parses individually, so this
        // cross-check is the only detector. Refuse loudly, same doctrine as
        // the other corrupt-state guards here.
        let mut seen_gens: Vec<(std::path::PathBuf, u64)> = Vec::new();

        // Read each file + shallow-merge its keys. `entities` is special-cased:
        // world.json + npc.json each may carry an `entities` object, and the two
        // must UNION (deep-merge) rather than overwrite.
        for path in [world_path, player_path, npc_path] {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let val: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let Some(obj) = val.as_object() else {
                // Same refuse-don't-reset contract as the `entities` guard
                // below: a parseable-but-non-object slice (hand-edit, corrupt
                // write — e.g. `[]`) must NOT load as an all-defaults schema,
                // because the next autosave would permanently overwrite the
                // file with those defaults. Fail the load instead.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{}: slice file is not an object (got {})",
                        path.display(),
                        Self::type_name_of_value(&val)
                    ),
                ));
            };
            let mut obj = obj.clone();
            if let Some(gen) = obj.remove("split_gen").and_then(|v| v.as_u64()) {
                seen_gens.push((path.to_path_buf(), gen));
            }
            for (k, v) in obj {
                if k == "entities" {
                    // Deep-merge: both sides' entities objects union together.
                    // (2026-08-16 audit LOW) A non-object `entities` value
                    // (hand-edit, corrupt write) used to be swallowed
                    // SILENTLY — the partition reset to empty, violating the
                    // refuse-don't-reset contract every other corrupt-state
                    // path here upholds. Fail the load instead.
                    let Some(src_map) = v.as_object() else {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "{}: `entities` is not an object (got {})",
                                path.display(),
                                Self::type_name_of_value(&v)
                            ),
                        ));
                    };
                    let target = merged
                        .entry("entities".to_string())
                        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
                    // Defensive: the accumulated target should always be an
                    // object (only this branch writes it) — if a prior file
                    // somehow poisoned it, replace rather than silently drop.
                    match target.as_object_mut() {
                        Some(target_map) => {
                            for (ek, ev) in src_map {
                                target_map.insert(ek.clone(), ev.clone());
                            }
                        }
                        None => {
                            *target = v.clone();
                        }
                    }
                } else {
                    merged.insert(k.clone(), v.clone());
                }
            }
        }

        // (deferred-3) Cross-file generation check. All PRESENT stamps must
        // agree (absent = legacy, accepted — a mix of stamped + legacy files
        // can only mean the very first stamped write crashed mid-rename,
        // which the disagreement check below still catches when two stamps
        // exist; a single stamped file among two legacy ones is the benign
        // post-upgrade first write).
        if let Some((_, first)) = seen_gens.first() {
            if let Some((bad_path, _)) = seen_gens.iter().find(|(_, g)| g != first) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "mixed-generation card state: {} disagrees with its siblings \
                         (a write was interrupted — restore from a save)",
                        bad_path.display()
                    ),
                ));
            }
        }

        let mut schema: WorldSchema = serde_json::from_value(serde_json::Value::Object(merged))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Carry the agreed stamp into the struct so the NEXT save stamps
        // strictly higher (monotonic even across loads).
        if let Some((_, gen)) = seen_gens.first() {
            schema.split_gen = *gen;
        }

        // (2026-08-20 Economy) Clamp every node's prosperity into the legal
        // [25, 200] band — old saves deserialize missing values at the 100
        // serde default, but a hand-edited 0/255 must not reach the
        // revenue/lifestyle curves (the bracket apply clamps the same way).
        for node in &mut schema.travel_graph.nodes {
            node.prosperity =
                node.prosperity.clamp(crate::economy::PROSPERITY_MIN, crate::economy::PROSPERITY_MAX);
        }

        // (2026-08-26 location-hygiene ruling) Normalize stored node NAMES:
        // parentheses never appear in a location. A card drafted before the
        // seed-side purifier seeded labels like "Earth (variable by scene)";
        // this pass heals them on the save's next load (ids untouched — the
        // graph's keys/edges stay stable; the cleaned name persists with the
        // session's next autosave).
        for node in &mut schema.travel_graph.nodes {
            let cleaned = clean_location_label(&node.name);
            if !cleaned.is_empty() && cleaned != node.name {
                tracing::info!(
                    node_id = %node.id,
                    before = %node.name,
                    after = %cleaned,
                    "load_split: stripped parenthetical qualifier from a node name"
                );
                node.name = cleaned;
            }
        }

        // (2026-08-29 modules E1+F2) The one-shot save heal — node setting
        // backfill + garbage-item sweep, version-gated on `heal_version`.
        // No-op (single comparison) for already-healed saves.
        Self::heal_schema_state(&mut schema);

        Ok(schema)
    }

}

/// Atomic write of arbitrary text (temp + fsync + rename). Shared by the
/// single-file `WorldSchema::save` and the three-file `save_split`. A crash
/// mid-write leaves the destination either at its prior complete state or the
/// new complete state — never a truncated middle (same guarantee as
/// `session::Conversation::save`). The temp file is a sibling (same dir/volume)
/// so `rename` is atomic on Windows (`MOVEFILE_REPLACE_EXISTING`).
fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp_path = temp_path_for(path);
    let _ = std::fs::remove_file(&tmp_path); // clear stale temp
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        std::io::Write::write_all(&mut file, text.as_bytes())?;
        std::io::Write::flush(&mut file)?;
        let _ = file.sync_all();
    }
    std::fs::rename(&tmp_path, path)
}

/// A micro-delta against [`WorldSchema`]. All fields optional: the model
/// emits ONLY the keys that changed this turn. Omitted fields = unchanged.
///
/// Deserialized from the JSON object the schema-delta model pass emits. The
/// `entities` field's inner `Option<String>` is load-bearing: outer `Option`
/// = "did any entity change?", inner `Option` = "is this a delete (`null`)
/// or a set (`Some`)?". `serde` deserializes JSON `null` to `None` and any
/// other JSON value to `Some(value)`, giving us the unambiguous delete-vs-set
/// signal for free. Values are `serde_json::Value` (widened 2026-08-11 from
/// `String`) so a single entity key can carry structured data — e.g. a quest
/// counter `{"progress":3,"target":5}` — without baking a parallel typed
/// system. Bare-string values (`"rusty knife"`) round-trip as
/// `Value::String` for full back-compat with pre-widening saves.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SchemaDelta {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub recent_events: Option<Vec<String>>,
    #[serde(default)]
    pub entities: Option<HashMap<String, Option<serde_json::Value>>>,
    /// (2026-08-19 Stale Roulette) Site-seed hooks the WORLD-PROGRESSION pass
    /// may emit for its designated nodes (`{node_id: "one-line hook"}`).
    /// Consumed + stripped by `fire_world_progression_tick` BEFORE
    /// `apply_delta` (validated against the graph + `clean_free_text`-capped,
    /// then pushed into `Node.seeds`) — `apply_delta` itself NEVER touches
    /// node seeds (site maps + the roulette are Rust-authoritative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_seeds: Option<HashMap<String, String>>,
    /// (2026-08-22 living-world) Site-evolution ops the WORLD-PROGRESSION
    /// pass may emit for its designated DEPARTED mapped sites
    /// (`{node_id: [SiteEvolutionOp…]}` — the constrained set_asset/
    /// move_asset/remove_asset mutation pass). Consumed + stripped by
    /// `fire_world_progression_tick` step 7c BEFORE `apply_delta` (the
    /// play-canon-locked applier owns validation) — `apply_delta` itself
    /// NEVER touches site maps (Rust-authoritative, the `site_seeds`
    /// pattern). Deliberately NOT counted in `has_changes()`: an
    /// ops-only delta still evolves (mirrors seeds-only deltas planting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_ops: Option<HashMap<String, Vec<crate::site_map::SiteEvolutionOp>>>,
    /// (2026-08-22 multihog WS3) Site pressure the WORLD-PROGRESSION pass
    /// emits for any KNOWN site it mentions (`{node_id: "one ≤140-char
    /// directional line"}` — not just the designated). Consumed +
    /// stripped by `fire_world_progression_tick` BEFORE `apply_delta`
    /// (validated against the graph + `clean_free_text`-capped, pushed
    /// into `Node.pending_pressure`) — `apply_delta` itself NEVER touches
    /// node pressure (Rust-authoritative, the `site_seeds` pattern).
    /// Deliberately NOT counted in `has_changes()`: a pressure-only delta
    /// still accumulates intent (mirrors seeds-only deltas planting).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_pressure: Option<HashMap<String, String>>,
    /// (2026-08-24 Part II A6) ONE regional pattern beyond per-entity deltas
    /// the WORLD-PROGRESSION pass may emit (`"<=160 chars, one regional
    /// pattern"` — a war's front shifts, a trade route collapses). Consumed +
    /// stripped by `fire_world_progression_tick` BEFORE `apply_delta`
    /// (`clean_free_text`-capped, then pushed as ONE bounded
    /// `pending_tick_directives` line the next narrator turn renders).
    /// Deliberately NOT counted in `has_changes()`: a currents-only delta
    /// still lands its line (mirrors seeds-only deltas planting).
    /// Rumor-seeding from the line is deferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wider_currents: Option<String>,
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
        let result = serde_json::from_str(&repaired);
        result
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

/// The starting world-state anchors derived from a card's `.intro` by the
/// launch-time bootstrap pass (2026-08-10). Where the card's authored
/// `<world>` sibling is the *authored* anchor set, `BootstrapAnchors` is
/// *derived*: one schema-engine pass reads the intro + extracts the implied
/// time/weather/location. The bootstrap runs only when the `<world>` seed
/// left an anchor dormant. Mirrors the cold-start seed discipline:
/// writes `world_clock` + `weather` + an opening travel-graph node directly
/// (NOT through `apply_delta`, which is test-pinned to never touch them).
///
/// Every field is `Option` — the model may legitimately omit any it can't
/// derive from the intro (a card with no weather mention → `weather: None` →
/// the caller's sensible-defaults fallback seeds `"clear"`). A fully-empty
/// result (all `None`) is valid; the caller falls through to defaults for each
/// dormant field independently.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootstrapAnchors {
    /// Parsed in-world minutes since 0001-01-01 (the same epoch `WorldClock`
    /// uses). `None` when the intro gives no usable time signal.
    pub time_minutes: Option<i64>,
    /// Diegetic weather condition phrase ("thick fog off the marsh"). `None`
    /// when the intro gives no usable weather signal.
    pub weather: Option<String>,
    /// The opening location as `(node_id, diegetic name)`. `None` when the
    /// intro gives no usable location signal (the `[DISCOVER]` path then
    /// handles it organically on turn 1). When `Some`, the caller upserts the
    /// node + sets `current_node` (only when the graph is still empty — a
    /// resumed save with an existing graph is preserved).
    pub location: Option<(String, String)>,
    /// (2026-08-22 living-world, the Auto-Harvest Dormancy ruling) The
    /// arcane resource the opening names ("mana", "biotics", "rage"),
    /// ≤24 chars. `None` when the fiction has none — the pool stays
    /// dormant (zero tokens, zero mechanics forever).
    pub arcana_label: Option<String>,
}

impl BootstrapAnchors {
    /// Parse the bootstrap pass's model output. Mirrors `SchemaDelta::from_
    /// model_output`'s tolerant pipeline (extract reply channel → strip
    /// markdown fences → syntactic repair → serde_json::from_str) but parses
    /// the bootstrap JSON shape `{time, weather, location_id, location_name}`
    /// (all fields optional). Returns `Ok(Self)` on a clean parse (fields left
    /// `None` when absent), `Err` on unparseable JSON so the caller can fall
    /// through to sensible defaults.
    ///
    /// `time` is a free-form string ("Day 1, 21:00") parsed to minutes via
    /// `bracket_parser::parse_in_world_time` (the same parser `[TIME]` uses).
    /// An unparseable `time` → `time_minutes: None` (NOT an `Err` — the other
    /// fields may still be valid).
    pub fn from_model_output(raw: &str) -> Result<Self, serde_json::Error> {
        let reply = extract_reply_channel(raw);
        let cleaned = strip_markdown_fences(&reply).trim();
        let repaired = crate::json_repair::repair(cleaned);
        #[derive(serde::Deserialize)]
        struct Raw {
            #[serde(default)]
            time: Option<String>,
            #[serde(default)]
            weather: Option<String>,
            #[serde(default, rename = "location_id")]
            location_id: Option<String>,
            #[serde(default, rename = "location_name")]
            location_name: Option<String>,
            /// (2026-08-22 living-world) The arcane resource the opening
            /// names ("mana", "biotics", "rage") — the Auto-Harvest
            /// Dormancy activation seed. Absent when the fiction has none.
            #[serde(default)]
            arcana: Option<String>,
        }
        let parsed: Raw = serde_json::from_str(&repaired)?;
        // (2026-08-16 audit M2) Free-text anchor strings render verbatim into
        // `<world_state>` (`weather:`/`location:` lines) — the same control-
        // char/newline gate every other render-facing field got. A
        // newline-laden model string would forge fake state lines into every
        // subsequent turn + save.
        fn clean_anchor(raw: String, cap: usize) -> Option<String> {
            let flattened: String = raw
                .chars()
                .map(|c| match c {
                    '\n' | '\r' | '\t' => ' ',
                    _ => c,
                })
                .collect();
            let cleaned: String = flattened
                .chars()
                .filter(|c| {
                    let code = *c as u32;
                    !((code <= 0x08)
                        || code == 0x0B
                        || code == 0x0C
                        || (0x0E..=0x1F).contains(&code))
                })
                .collect();
            let cleaned = cleaned.trim().chars().take(cap).collect::<String>();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        }
        // time → minutes (a parse failure is a soft None, not a hard Err).
        let time_minutes = parsed
            .time
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| crate::bracket_parser::parse_in_world_time(s));
        const ANCHOR_TEXT_MAX: usize = 80;
        let weather = parsed.weather.and_then(|w| clean_anchor(w, WEATHER_ANCHOR_MAX));
        // location: require BOTH id + name (a partial pair is useless).
        // (2026-08-26) The name runs through clean_location_label — a
        // model-emitted "Somewhere (near the docks)" never seeds a
        // parenthesized node label. A name that cleans to nothing keeps the
        // anchor-cleaned original.
        let location = match (parsed.location_id, parsed.location_name) {
            (Some(id), Some(name)) => {
                match (
                    clean_anchor(id, ANCHOR_TEXT_MAX),
                    clean_anchor(name, ANCHOR_TEXT_MAX),
                ) {
                    (Some(id), Some(name)) => {
                        let cleaned = clean_location_label(&name);
                        Some((id, if cleaned.is_empty() { name } else { cleaned }))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        // (2026-08-22 living-world) The arcane label — the [ARCANA] bracket's
        // 24-char cap discipline (a resource NAME, not a sentence).
        let arcana_label = parsed.arcana.and_then(|a| clean_anchor(a, 24));
        Ok(Self { time_minutes, weather, location, arcana_label })
    }
}

/// Cap for the bootstrap-derived weather condition (audit M2; the
/// `[WEATHER]` bracket path uses the tighter `WEATHER_CONDITION_MAX`
/// discipline — this matches it).
const WEATHER_ANCHOR_MAX: usize = 60;

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
/// `session::temp_path_for`: same directory/volume so `rename` is atomic,
/// UNIQUE per write (2026-08-16 audit fix #13 — `save_schema` has 5 call
/// sites and no serialization lock; the old fixed `.tmp` let racing writers
/// interleave on one file).
fn temp_path_for(path: &Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from("wupi.tmp"));
    name.push(".");
    name.push(crate::fable_save::unique_tmp_suffix());
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (2026-08-22 re-track hardening) A base-schema revert KEEPS the
    /// Rust-owned anchors (clock + calendar family + the rest anchor) from
    /// the live world — the moment still happened; only its prose is
    /// re-derived. Everything else on the base wins.
    #[test]
    fn retain_revert_safe_anchors_keeps_clock_and_calendar() {
        let mut base = WorldSchema::default();
        base.world_clock.current_minutes = 540; // 09:00, pre-turn
        base.world_clock.last_tick_minutes = 540;
        base.calendar = Some("1st of Harvest, Year 1247".into());
        base.calendar_synced_minutes = Some(540);
        base.last_rest_minutes = 300; // stale pre-rest anchor on the base
        base.summary = "the base summary".into();

        let mut live = base.clone();
        live.world_clock.current_minutes = 545; // the turn's [TIME] applied
        live.calendar = Some("2nd of Harvest, Year 1247".into());
        live.calendar_synced_minutes = Some(545);
        live.last_rest_minutes = 545; // the turn's [REST] stamped post-sleep
        live.summary = "the live summary".into();

        let mut restored = base.clone();
        restored.retain_revert_safe_anchors(&live);
        assert_eq!(restored.world_clock.current_minutes, 545, "clock survives the revert");
        assert_eq!(restored.calendar.as_deref(), Some("2nd of Harvest, Year 1247"));
        assert_eq!(restored.calendar_synced_minutes, Some(545));
        assert_eq!(restored.last_rest_minutes, 545, "rest anchor survives (no phantom weary band)");
        assert_eq!(restored.summary, "the base summary", "everything else reverts");
    }

    #[test]
    fn apply_delta_upserts_entities() {
        let mut schema = WorldSchema::default();
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(HashMap::from([
                ("iron_sword".to_string(), Some(serde_json::Value::String("acquired".into()))),
                ("loc.current".to_string(), Some(serde_json::Value::String("tavern".into()))),
            ])),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert_eq!(
            schema.entities.get("iron_sword").and_then(|v| v.as_str()),
            Some("acquired")
        );
        assert_eq!(
            schema.entities.get("loc.current").and_then(|v| v.as_str()),
            Some("tavern")
        );
    }

    #[test]
    fn apply_delta_null_deletes_key() {
        let mut schema = WorldSchema {
            summary: String::new(),
            recent_events: vec![],
            entities: BTreeMap::from([
                ("iron_sword".to_string(), serde_json::Value::String("acquired".into())),
                ("loc.current".to_string(), serde_json::Value::String("tavern".into())),
            ]),
            ..Default::default()
        };
        // Drop the sword, move locations.
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(HashMap::from([
                ("iron_sword".to_string(), None), // delete
                ("loc.current".to_string(), Some(serde_json::Value::String("forest".into()))),
            ])),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert!(!schema.entities.contains_key("iron_sword"), "null should delete");
        assert_eq!(
            schema.entities.get("loc.current").and_then(|v| v.as_str()),
            Some("forest")
        );
    }

    #[test]
    fn apply_delta_null_on_missing_key_is_noop() {
        let mut schema = WorldSchema::default();
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(HashMap::from([("ghost".to_string(), None)])),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert!(schema.entities.is_empty());
    }

    #[test]
    fn apply_delta_appends_recent_events() {
        let mut schema = WorldSchema {
            summary: String::new(),
            recent_events: vec!["entered tavern".to_string()],
            entities: BTreeMap::new(),
            ..Default::default()
        };
        let delta = SchemaDelta {
            summary: None,
            recent_events: Some(vec!["ordered ale".to_string(), "heard rumor".to_string()]),
            entities: None,
            ..Default::default()
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
            entities: BTreeMap::new(),
            ..Default::default()
        };
        let delta = SchemaDelta {
            summary: Some("new summary".to_string()),
            recent_events: None,
            entities: None,
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert_eq!(schema.summary, "new summary");
    }

    #[test]
    fn apply_delta_empty_is_noop() {
        let mut schema = WorldSchema {
            summary: "kept".to_string(),
            recent_events: vec!["kept".to_string()],
            entities: BTreeMap::from([("k".to_string(), serde_json::Value::String("v".into()))]),
            ..Default::default()
        };
        schema.apply_delta(SchemaDelta::default());
        assert_eq!(schema.summary, "kept");
        assert_eq!(schema.recent_events, vec!["kept"]);
        assert_eq!(schema.entities.get("k").and_then(|v| v.as_str()), Some("v"));
    }

    // --- merge_patch (the model-facing fable_schema_patch path, 2026-08-11) ---

    #[test]
    fn merge_patch_full_replaces_scalar_fields() {
        let mut schema = WorldSchema {
            summary: "old".to_string(),
            ..Default::default()
        };
        let patch = serde_json::json!({ "summary": "new summary" });
        let merged = schema.merge_patch(patch).expect("scalar replace");
        assert_eq!(merged, vec!["summary".to_string()]);
        assert_eq!(schema.summary, "new summary");
    }

    #[test]
    fn merge_patch_entities_shallow_merges_and_deletes() {
        let mut schema = WorldSchema::default();
        schema.entities.insert("keep".into(), serde_json::Value::String("v".into()));
        schema.entities.insert("drop".into(), serde_json::Value::String("x".into()));
        let patch = serde_json::json!({
            "entities": {
                "add": "added",
                "drop": null,
                "structured": { "progress": 3, "target": 5 }
            }
        });
        let merged = schema.merge_patch(patch).expect("entities merge");
        assert_eq!(merged, vec!["entities".to_string()]);
        assert_eq!(schema.entities.get("keep").and_then(|v| v.as_str()), Some("v"));
        assert_eq!(schema.entities.get("add").and_then(|v| v.as_str()), Some("added"));
        assert!(!schema.entities.contains_key("drop"), "null should delete");
        // Structured value survives as a JSON object (the widening's point).
        let mut expected_obj = serde_json::Map::new();
        expected_obj.insert("progress".to_string(), serde_json::Value::from(3));
        expected_obj.insert("target".to_string(), serde_json::Value::from(5));
        assert_eq!(
            schema.entities.get("structured").and_then(|v| v.as_object()),
            Some(&expected_obj)
        );
    }

    #[test]
    fn merge_patch_refuses_immutable_keys() {
        let mut schema = WorldSchema::default();
        let patch = serde_json::json!({ "immutable_keys": ["npc.marcus.core"] });
        let err = schema.merge_patch(patch).expect_err("must refuse");
        assert!(err.contains("immutable_keys"), "error should explain: {err}");
    }

    #[test]
    fn merge_patch_unknown_field_errors() {
        let mut schema = WorldSchema::default();
        let patch = serde_json::json!({ "totally_made_up": "field" });
        let err = schema.merge_patch(patch).expect_err("must refuse");
        assert!(err.contains("unknown top-level field"), "error should explain: {err}");
    }

    #[test]
    fn merge_patch_partial_leaves_absent_fields_alone() {
        // Only `summary` is in the patch; recent_events + entities stay put.
        let mut schema = WorldSchema {
            summary: "old".to_string(),
            recent_events: vec!["kept".to_string()],
            entities: BTreeMap::from([("k".into(), serde_json::Value::String("v".into()))]),
            ..Default::default()
        };
        let patch = serde_json::json!({ "summary": "new" });
        schema.merge_patch(patch).expect("partial patch");
        assert_eq!(schema.summary, "new");
        assert_eq!(schema.recent_events, vec!["kept"]);
        assert_eq!(schema.entities.get("k").and_then(|v| v.as_str()), Some("v"));
    }

    #[test]
    fn merge_patch_typed_field_replace_via_deserialize() {
        // weather is a typed struct; the patch must deserialize cleanly.
        let mut schema = WorldSchema::default();
        let patch = serde_json::json!({
            "weather": { "condition": "rain", "started_at_minutes": 540 }
        });
        let merged = schema.merge_patch(patch).expect("typed replace");
        assert_eq!(merged, vec!["weather".to_string()]);
        assert_eq!(schema.weather.condition, "rain");
    }

    #[test]
    fn merge_patch_malformed_typed_field_errors() {
        let mut schema = WorldSchema::default();
        // weather is an object, not an array → type error must surface.
        let patch = serde_json::json!({ "weather": ["not", "an", "object"] });
        let err = schema.merge_patch(patch).expect_err("type mismatch");
        assert!(err.contains("weather"), "error should name the field: {err}");
    }

    #[test]
    fn merge_patch_empty_object_is_noop() {
        let mut schema = WorldSchema {
            summary: "kept".to_string(),
            ..Default::default()
        };
        let merged = schema.merge_patch(serde_json::json!({})).expect("empty patch");
        assert!(merged.is_empty(), "no fields merged");
        assert_eq!(schema.summary, "kept");
    }

    #[test]
    fn merge_patch_non_object_errors() {
        let mut schema = WorldSchema::default();
        let err = schema
            .merge_patch(serde_json::json!(["not", "an", "object"]))
            .expect_err("non-object patch");
        assert!(err.contains("must be a JSON object"), "error: {err}");
    }

    /// (2026-08-16 yellow W3) A full-replace registry over the cap is refused
    /// (the raw-editor JSON tab is the reachable caller) — same discipline as
    /// the travel-graph arm.
    #[test]
    fn merge_patch_refuses_over_cap_npc_registry() {
        let mut schema = WorldSchema::default();
        let entries: Vec<serde_json::Value> = (0..MAX_NPC_REGISTRY + 1)
            .map(|i| serde_json::json!({ "id": format!("npc{i}"), "name": format!("Npc {i}") }))
            .collect();
        let patch = serde_json::json!({ "npc_registry": { "entries": entries } });
        let err = schema.merge_patch(patch).expect_err("over-cap registry");
        assert!(err.contains("npc_registry"), "error names the field: {err}");
        assert!(err.contains("exceeds"), "error states the cap: {err}");
        // At exactly the cap it installs.
        let entries: Vec<serde_json::Value> = (0..MAX_NPC_REGISTRY)
            .map(|i| serde_json::json!({ "id": format!("npc{i}"), "name": format!("Npc {i}") }))
            .collect();
        let mut schema = WorldSchema::default();
        schema
            .merge_patch(serde_json::json!({ "npc_registry": { "entries": entries } }))
            .expect("at-cap registry installs");
    }

    /// (2026-08-22 multihog WS1) The deterministic entity-expiry sweep:
    /// past deadlines delete + direct; future deadlines stand; immutable +
    /// player identity keys refuse deletion (their slots still drop so the
    /// observation fires once); a slot for a missing entity is dropped
    /// silently.
    #[test]
    fn sweep_entity_expiry_past_future_immutable() {
        let mut schema = WorldSchema::default();
        schema.entities.insert(
            "bridge-out".to_string(),
            serde_json::Value::String("the crossing is torn".into()),
        );
        schema.entities.insert(
            "ward-vault".to_string(),
            serde_json::Value::String("sealed".into()),
        );
        schema.entities.insert(
            "npc.marcus.core".to_string(),
            serde_json::Value::String("canon".into()),
        );
        schema.entities.insert(
            "player.name".to_string(),
            serde_json::Value::String("hero".into()),
        );
        schema
            .entity_expiry
            .insert("bridge-out".to_string(), 1_000);
        schema.entity_expiry.insert("ward-vault".to_string(), 5_000);
        schema
            .entity_expiry
            .insert("npc.marcus.core".to_string(), 1_000);
        schema
            .entity_expiry
            .insert("player.name".to_string(), 1_000);
        schema.entity_expiry.insert("ghost-key".to_string(), 900);

        let (directives, mutated) = schema.sweep_entity_expiry(2_000);
        assert_eq!(mutated, 4, "three due slots + the ghost slot all drop");
        assert!(!schema.entities.contains_key("bridge-out"), "past deadline deletes");
        assert!(
            schema.entities.contains_key("ward-vault"),
            "future deadline stands"
        );
        assert!(
            schema.entities.contains_key("npc.marcus.core"),
            "immutable key survives the sweep"
        );
        assert!(
            schema.entities.contains_key("player.name"),
            "player identity key survives the sweep"
        );
        assert!(!schema.entity_expiry.contains_key("npc.marcus.core"));
        assert!(!schema.entity_expiry.contains_key("ghost-key"), "dead slot drops");
        assert_eq!(directives.len(), 1, "only the real expiry directs");
        assert!(
            directives[0].starts_with("Expired: bridge-out"),
            "directive names the key: {}",
            directives[0]
        );
        assert!(directives[0].contains("Day 2"), "directive carries the clock: {}", directives[0]);

        // A second sweep at the same now is a no-op (slots already dropped).
        let (dirs2, n2) = schema.sweep_entity_expiry(2_000);
        assert_eq!((dirs2.len(), n2), (0, 0));
        // Dormant clock → never fires.
        let mut fresh = WorldSchema::default();
        fresh.entity_expiry.insert("k".to_string(), 1);
        assert_eq!(fresh.sweep_entity_expiry(0).0.len(), 0);
    }

    /// (2026-08-22 multihog WS1) Serde dormancy: a pre-WS1 save (no
    /// `entity_expiry` key) loads with the field empty; an empty field
    /// serializes back without the key (byte-identical saves).
    #[test]
    fn entity_expiry_serde_dormant_roundtrip() {
        let legacy = r#"{"summary":"old","recent_events":[],"entities":{}}"#;
        let loaded: WorldSchema = serde_json::from_str(legacy).expect("legacy loads");
        assert!(loaded.entity_expiry.is_empty());
        let back = serde_json::to_value(&loaded).expect("serialize");
        assert!(
            back.get("entity_expiry").is_none(),
            "empty field must not serialize: {back}"
        );
        let armed = r#"{"entity_expiry":{"bridge-out":1440}}"#;
        let loaded: WorldSchema = serde_json::from_str(armed).expect("armed loads");
        assert_eq!(loaded.entity_expiry.get("bridge-out"), Some(&1440));
    }

    /// (2026-08-23 starvation fix) `last_material_minutes` serde dormancy: a
    /// pre-fix node JSON (no field) loads with 0 (= never materialized), and
    /// a set value round-trips — the designation watermark's exact pattern.
    #[test]
    fn node_last_material_minutes_serde_dormant() {
        let legacy = r#"{"id":"warren","name":"Warren","neighbors":[],"setting":""}"#;
        let node: Node = serde_json::from_str(legacy).expect("legacy node loads");
        assert_eq!(node.last_material_minutes, 0, "absent field = never materialized");
        let mut stamped = Node::default();
        stamped.id = "warren".into();
        stamped.last_material_minutes = 43_200;
        let back: Node =
            serde_json::from_str(&serde_json::to_string(&stamped).unwrap()).unwrap();
        assert_eq!(back.last_material_minutes, 43_200, "set value round-trips");
    }

    /// (2026-08-22 multihog WS3) `site_pressure` is a Rust-consumed field:
    /// a pressure-only delta has NO has_changes (the seeds-only precedent)
    /// and `apply_delta` never touches node state — the apply lives in
    /// `fire_world_progression_tick`'s take-and-strip step.
    #[test]
    fn site_pressure_delta_is_rust_consumed_not_applied() {
        let mut delta = SchemaDelta::default();
        delta.site_pressure = Some(HashMap::from([(
            "warren".to_string(),
            "the debt comes due".to_string(),
        )]));
        assert!(!delta.has_changes(), "pressure-only deltas are not changes");
        let mut schema = WorldSchema::default();
        schema.travel_graph.nodes = vec![Node::default()];
        schema.travel_graph.nodes[0].id = "warren".into();
        schema.apply_delta(delta);
        assert!(
            schema.travel_graph.nodes[0].pending_pressure.is_empty(),
            "apply_delta never plants pressure"
        );
        // Dormant serde: absent field loads empty; empty serializes absent.
        let loaded: WorldSchema =
            serde_json::from_str(r#"{"summary":"x"}"#).expect("loads");
        assert!(loaded.travel_graph.nodes.is_empty());
        let back = serde_json::to_value(&WorldSchema::default()).unwrap();
        assert!(back.get("site_pressure").is_none());
        // The DELTA's field round-trips (model output shape).
        let d: SchemaDelta = serde_json::from_str(
            r#"{"site_pressure":{"warren":"the debt comes due"}}"#,
        )
        .expect("delta parses");
        assert_eq!(
            d.site_pressure.as_ref().and_then(|m| m.get("warren")),
            Some(&"the debt comes due".to_string())
        );
    }

    /// (2026-08-16 yellow S5) Immutable keys are exempt from FIFO entity
    /// eviction — they seed oldest, so the sweep used to eat them before the
    /// lock could ever matter.
    #[test]
    fn enforce_entity_cap_spares_immutable_keys() {
        let mut schema = WorldSchema::default();
        schema.immutable_keys.insert("npc.marcus".to_string());
        // Fill past the 500 cap: the immutable key + a player key + 500
        // evictable ones (entity_order follows first-insert).
        schema.entities.insert(
            "npc.marcus".to_string(),
            serde_json::Value::String("canon".into()),
        );
        schema.entity_order.push_back("npc.marcus".to_string());
        schema.entities.insert(
            "player.name".to_string(),
            serde_json::Value::String("hero".into()),
        );
        schema.entity_order.push_back("player.name".to_string());
        for i in 0..500 {
            let key = format!("tmp.{i}");
            schema
                .entities
                .insert(key.clone(), serde_json::Value::String("x".into()));
            schema.entity_order.push_back(key);
        }
        schema.enforce_entity_cap();
        assert!(schema.entities.len() <= 500, "cap enforced");
        assert!(
            schema.entities.contains_key("npc.marcus"),
            "immutable key survives the sweep"
        );
        assert!(
            schema.entities.contains_key("player.name"),
            "player identity key survives"
        );
        assert!(
            !schema.entities.contains_key("tmp.0"),
            "oldest evictable key was the one dropped"
        );
    }

    /// (2026-08-16 yellow S7) A hand-edited save's newline-laden summary/event
    /// can never forge a render line — both prompt surfaces flatten inline.
    #[test]
    fn render_surfaces_flatten_newlines_in_summary_and_events() {
        let mut schema = WorldSchema::default();
        schema.summary = "peace talks begin\npresent: ghost, clock: 99:99".to_string();
        schema.recent_events = vec!["the duke arrived\nlocation: nowhere, exits: all".to_string()];
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("summary: peace talks begin present: ghost, clock: 99:99"), "{rendered}");
        assert!(!rendered.contains("\npresent: ghost"), "no forged line");
        let json = schema.to_json_prompt();
        assert!(!json.contains("\\nlocation: nowhere"), "prompt JSON flattened: {json}");
    }

    /// (2026-08-24 review P2) flatten_inline gap sweep: the date/weather/
    /// tone/present/rumors/cast/custom render lines — a hand-edited save's
    /// newline-laden values must never forge a second `<world_state>` line
    /// (a raw "rain\nclock: Day 99" weather condition used to mint a fake
    /// clock the tracker reads as ground truth).
    #[test]
    fn render_for_prompt_flattens_anchor_and_roster_lines() {
        let mut schema = WorldSchema::default();
        schema.calendar = Some("17th of Peatfall\npresent: ghost".into());
        schema.world_clock.current_minutes = 600;
        schema.weather.condition = "rain\nclock: Day 99".into();
        schema.tone = Some("eerie\nexits: everywhere".into());
        schema.npc_registry = NpcRegistry {
            entries: vec![NpcEntry {
                id: "mara".into(),
                name: "Mara\nweather: storm".into(),
                role: String::new(),
                tier: None,
                aliases: vec![],
                prominence: NpcProminence::Named,
            }],
        };
        schema.presences = vec![Presence {
            npc_id: "mara".into(),
            name: "Mara".into(),
            stance: "at the bar\nclock: Day 42".into(),
            ttl: PRESENCE_GRACE_RESET,
        }];
        schema.travel_graph.nodes.push(Node {
            id: "tavern".into(),
            name: "Tavern".into(),
            ..Node::default()
        });
        schema.travel_graph.current_node = Some("tavern".into());
        schema.rumors.push(crate::rumor::Rumor {
            label: "the duke lied\nsummary: fake".into(),
            origin_node: "tavern".into(),
            known_nodes: vec!["tavern".into()],
            born_minutes: 0,
        });
        schema.custom_tags.insert("curse".into(), "withering\ntone: chipper".into());
        let rendered = schema.render_for_prompt();
        for forged in [
            "\npresent: ghost",
            "\nclock: Day 99",
            "\nexits: everywhere",
            "\nweather: storm",
            "\nclock: Day 42",
            "\nsummary: fake",
            "\ntone: chipper",
        ] {
            assert!(
                !rendered.contains(forged),
                "forged line {forged:?} must not render: {rendered}"
            );
        }
        // The values survive, flattened onto their own lines.
        assert!(rendered.contains("date: 17th of Peatfall present: ghost"), "{rendered}");
        assert!(rendered.contains("weather: rain clock: Day 99"), "{rendered}");
        assert!(rendered.contains("tone: eerie exits: everywhere"), "{rendered}");
        assert!(rendered.contains("cast: Mara weather: storm [mara]"), "{rendered}");
        assert!(rendered.contains("present: Mara (at the bar clock: Day 42)"), "{rendered}");
        assert!(rendered.contains("rumors: the duke lied summary: fake"), "{rendered}");
        assert!(rendered.contains("custom: curse: withering tone: chipper"), "{rendered}");
    }

    /// (2026-08-16 yellow S4) The prompt JSON enforces its total char budget:
    /// a schema at the growth cap trims OLDEST entities with a visible
    /// marker, keeping player identity keys.
    #[test]
    fn to_json_prompt_trims_to_budget_keeping_player_keys() {
        let mut schema = WorldSchema::default();
        schema.summary = "s".to_string();
        schema.entities.insert(
            "player.name".to_string(),
            serde_json::Value::String("hero".into()),
        );
        schema.entity_order.push_back("player.name".to_string());
        // 300 entities × ~120 chars each ≈ 36k chars — far past the 4000
        // budget. Oldest-first insertion: e000 is oldest.
        for i in 0..300 {
            let key = format!("e{i:03}");
            let val = serde_json::Value::String("v".repeat(100));
            schema.entities.insert(key.clone(), val);
            schema.entity_order.push_back(key);
        }
        let json = schema.to_json_prompt();
        assert!(
            json.chars().count() < crate::settings::SCHEMA_JSON_PROMPT_BUDGET_CHARS + 512,
            "trimmed near the budget: {}",
            json.chars().count()
        );
        assert!(json.contains("player.name"), "identity key kept: {json}");
        assert!(json.contains("e000"), "oldest entities included first: {json}");
        assert!(!json.contains("e299"), "newest trimmed: {json}");
        assert!(json.contains("entities_trimmed"), "trim is visible: {json}");
    }

    /// (2026-08-24 bug fix) The JSON budget accounts summary + events +
    /// entities TOGETHER: a legal-max envelope (4,000-char summary + 6 ×
    /// ~1,600-char events) used to ride ~10k chars on top of the entities
    /// budget — the composed schema-engine prompt re-blew the CTX_SCHEMA
    /// prompt ceiling (the middle-drop the budget exists to kill). The
    /// envelope now renders first (prompt-capped with the `[…]` marker),
    /// entities spend the remainder, and the WHOLE document stays at the
    /// budget with the entities floor keeping the diff target alive.
    #[test]
    fn to_json_prompt_accounts_envelope_in_total_budget() {
        let mut schema = WorldSchema::default();
        schema.summary = "s".repeat(4_000);
        schema.recent_events = (0..6).map(|i| format!("event {i} ").repeat(200)).collect();
        schema.entities.insert(
            "player.name".to_string(),
            serde_json::Value::String("hero".into()),
        );
        schema.entity_order.push_back("player.name".to_string());
        // Same overload as the trim test: entities far past the remainder.
        for i in 0..300 {
            let key = format!("e{i:03}");
            let val = serde_json::Value::String("v".repeat(100));
            schema.entities.insert(key.clone(), val);
            schema.entity_order.push_back(key);
        }
        let json = schema.to_json_prompt();
        assert!(
            json.chars().count() < crate::settings::SCHEMA_JSON_PROMPT_BUDGET_CHARS + 64,
            "whole document (summary + events + entities) stays at the budget: {}",
            json.chars().count()
        );
        assert!(json.contains(" […]"), "oversize summary/events render capped: {json}");
        assert!(json.contains("event 0"), "events still render: {json}");
        assert!(json.contains("player.name"), "identity key kept: {json}");
        assert!(json.contains("e000"), "entities never fully starve (floor): {json}");
        assert!(!json.contains("e299"), "newest trimmed: {json}");
        assert!(json.contains("entities_trimmed"), "trim is visible: {json}");
    }

    #[test]
    fn entities_legacy_string_value_round_trips_through_save_load() {
        // The 2026-08-11 widening from HashMap<String, String> to <String, Value>
        // must NOT break old saves: a bare-string value deserializes cleanly to
        // Value::String and re-serializes back to the same bytes.
        let dir = std::env::temp_dir();
        let path = dir.join("wupi_schema_widening_test.json");
        let _ = std::fs::remove_file(&path);
        let schema = WorldSchema {
            summary: "widening test".to_string(),
            recent_events: vec![],
            entities: BTreeMap::from([
                ("item.sword".to_string(), serde_json::Value::String("rusty".into())),
                ("quest.dragon".to_string(), serde_json::json!({"progress": 3, "target": 5})),
            ]),
            ..Default::default()
        };
        schema.save(&path).unwrap();
        let loaded = WorldSchema::load(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // Bare string value survives as Value::String.
        assert_eq!(
            loaded.entities.get("item.sword").and_then(|v| v.as_str()),
            Some("rusty")
        );
        // Structured value survives as a JSON object.
        assert_eq!(
            loaded.entities.get("quest.dragon").and_then(|v| v.get("progress")).and_then(|v| v.as_u64()),
            Some(3)
        );
    }

    #[test]
    fn has_changes_detects_populated_delta() {
        // Summary populated → has_changes.
        assert!(SchemaDelta {
            summary: Some("hi".into()),
            recent_events: None,
            entities: None,
            ..Default::default()
        }
        .has_changes());
        // Recent events populated → has_changes.
        assert!(SchemaDelta {
            summary: None,
            recent_events: Some(vec!["e".into()]),
            entities: None,
            ..Default::default()
        }
        .has_changes());
        // Entity mutations (even a delete/null) → has_changes.
        let mut ents = HashMap::new();
        ents.insert("k".to_string(), None);
        assert!(SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
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
            ..Default::default()
        }
        .has_changes());
    }

    #[test]
    fn from_model_output_parses_clean_json() {
        let raw = r#"{"summary":"new","entities":{"x":"1"}}"#;
        let delta = SchemaDelta::from_model_output(raw).unwrap();
        assert_eq!(delta.summary.as_deref(), Some("new"));
        // The "1" deserializes as Value::String("1") (bare-string JSON values
        // are the simple-value case the widening keeps backwards-compatible).
        assert_eq!(
            delta.entities.unwrap().get("x").and_then(|opt| opt.as_ref().and_then(|v| v.as_str())),
            Some("1")
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
            Some(&Some(serde_json::Value::String("acquired".into())))
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
    fn render_calendar_label_suppresses_day_counter() {
        // 2026-08-13: when a calendar label is set, `date:` renders + the clock
        // shows time-of-day ONLY (no redundant "Day N"). Without a label, the
        // legacy "Day N, HH:MM" render stands.
        let mut schema = WorldSchema::default();
        // Day 2, 14:00 = 1440 + 14*60 = 2280 minutes.
        schema.world_clock.current_minutes = 2280;
        schema.world_clock.last_tick_minutes = 2280;
        // No calendar yet → legacy render. (2026-08-21 AM/PM: 14:00 → 2:00 PM.)
        let legacy = schema.render_for_prompt();
        assert!(legacy.contains("clock: Day 2, 2:00 PM"));
        assert!(!legacy.contains("date:"));
        // With a calendar → date: + time-of-day clock:.
        schema.calendar = Some("3rd of Harvest, Year 1247".into());
        let labeled = schema.render_for_prompt();
        assert!(labeled.contains("date: 3rd of Harvest, Year 1247"));
        assert!(labeled.contains("clock: 2:00 PM"));
        assert!(!labeled.contains("Day 2"), "Day N suppressed when calendar is set");
    }

    #[test]
    fn render_custom_tags_as_bounded_line() {
        // custom_tags render as a bounded `custom:` line so they reach the
        // narrator (entities themselves are NOT rendered).
        let mut schema = WorldSchema::default();
        schema.summary = "scene set".into(); // keep it non-empty
        schema.custom_tags = BTreeMap::from([
            ("starting_currency".into(), "200 gold".into()),
            ("guard_reputation".into(), "-20".into()),
        ]);
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("custom:"));
        assert!(rendered.contains("starting_currency: 200 gold"));
        assert!(rendered.contains("guard_reputation: -20"));
    }

    #[test]
    fn render_for_prompt_does_not_render_entities_dump() {
        // 2026-08-10: the uncapped entities dump is stripped from the prompt.
        // Entity state stays in the Rust schema (God-Tier authority) + reaches
        // the model via the 1-turn bracket window — NEVER via this prompt
        // block. This test pins the contract so a future "helpful" re-add of a
        // capped entity render is caught immediately (a cap re-grows; the
        // bracket window is the lightweight carry).
        let schema = WorldSchema {
            entities: BTreeMap::from([
                ("npc.mara.tier".to_string(), serde_json::Value::String("acquaintance".into())),
                ("npc.harsk.tier".to_string(), serde_json::Value::String("foe".into())),
                ("world.fact".to_string(), serde_json::Value::String("the mire is poisonous".into())),
            ]),
            // Set one anchor so the function doesn't early-return as empty.
            summary: "the scene is set".to_string(),
            ..Default::default()
        };
        let rendered = schema.render_for_prompt();
        // Anchors still render.
        assert!(rendered.contains("summary: the scene is set"));
        // NO entity content leaks into the prompt.
        assert!(!rendered.contains("entities:"), "entities dump must NOT render");
        assert!(!rendered.contains("npc.mara.tier"), "no entity key in prompt");
        assert!(!rendered.contains("acquaintance"), "no entity value in prompt");
        assert!(!rendered.contains("the mire is poisonous"), "no world.fact in prompt");
    }

    #[test]
    fn render_for_prompt_caps_recent_events_at_six() {
        let schema = WorldSchema {
            summary: String::new(),
            recent_events: (0..10).map(|i| format!("event{i}")).collect(),
            entities: BTreeMap::new(),
            ..Default::default()
        };
        let rendered = schema.render_for_prompt();
        // Only the last 6 events appear — the cap was raised 5 → 6 by the
        // 2026-08-21 evening follow-up to the 8192 ruling.
        assert!(rendered.contains("event4"));
        assert!(rendered.contains("event9"));
        assert!(!rendered.contains("event3"));
    }

    #[test]
    fn render_for_prompt_caps_belt_at_sixteen() {
        // (2026-08-24 review fix) The belt renders under the same cap
        // discipline as pouch/pack — hand-edited saves + a drifted apply
        // path can grow it past the 4-slot intent.
        let mut schema = WorldSchema::default();
        for i in 0..20 {
            schema.player_state.belt.push(crate::equipment::StackItem {
                name: format!("belt-item-{i:02}"),
                qty: 1,
                ..Default::default()
            });
        }
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("belt:"), "belt line missing");
        assert!(rendered.contains("belt-item-00"), "first (oldest) item missing");
        assert!(rendered.contains("belt-item-15"), "16th item missing");
        assert!(!rendered.contains("belt-item-16"), "17th item must not render");
        assert!(rendered.contains("(+4 more)"), "overflow marker missing");
    }

    #[test]
    fn entity_delete_prunes_its_order_slot() {
        // (2026-08-24 review fix) A deleted entity must take its
        // entity_order slot with it — the stale slot both bloated the
        // deque under churn and aged a re-inserted key forward of its true
        // first-insert position.
        let mut s = WorldSchema::default();
        let delta = |pairs: &[(&str, Option<&str>)]| crate::schema::SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(
                pairs
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string(),
                            v.map(|val| serde_json::Value::String(val.to_string())),
                        )
                    })
                    .collect(),
            ),
        };
        s.apply_delta(delta(&[
            ("npc.marcus", Some("the blacksmith")),
            ("npc.rival", Some("the rival")),
            ("weather", Some("clear")),
        ]));
        assert_eq!(s.entity_order.len(), 3);
        // Delete through the delta's null arm.
        s.apply_delta(delta(&[("npc.rival", None)]));
        assert!(!s.entities.contains_key("npc.rival"));
        assert_eq!(
            s.entity_order.len(),
            2,
            "the stale slot must die with the entity: {:?}",
            s.entity_order
        );
        // Re-insert: the key gets exactly ONE slot, at the TAIL (true
        // first-insert order for the new life of the key).
        s.apply_delta(delta(&[("npc.rival", Some("returned"))]));
        assert_eq!(
            s.entity_order.iter().filter(|k| k.as_str() == "npc.rival").count(),
            1,
            "re-insertion must not double-slot the key"
        );
        assert_eq!(s.entity_order.back().map(String::as_str), Some("npc.rival"));
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
            .insert(crate::player_state::BodyPart::LeftUpperArm, crate::player_state::BodyPartState::Orange);
        schema.player_state.stamina = crate::player_state::Stamina::Winded;
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("player_state:"), "player_state block must appear");
        assert!(rendered.contains("stamina: Winded"));
        assert!(rendered.contains("Left Upper Arm (Medium Injury)"));
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
            entities: BTreeMap::from([("k".to_string(), serde_json::Value::String("v".into()))]),
            ..Default::default()
        };
        schema.save(&path).unwrap();
        let loaded = WorldSchema::load(&path).unwrap();
        assert_eq!(loaded.summary, "test summary");
        assert_eq!(loaded.recent_events, vec!["e1"]);
        assert_eq!(loaded.entities.get("k").and_then(|v| v.as_str()), Some("v"));
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

    /// (2026-08-16 deferred-3) The split trio's generation stamps: all three
    /// files carry the SAME stamp, the counter advances monotonically across
    /// saves, a mixed-generation trio is REFUSED (the crash-between-renames
    /// Frankenstein every file individually survives), and a legacy unstamped
    /// trio still loads.
    #[test]
    fn split_gen_stamps_roundtrip_and_refuse_mixed_trios() {
        let dir = std::env::temp_dir();
        let world = dir.join("wupi_splitgen_world.json");
        let player = dir.join("wupi_splitgen_player.json");
        let npc = dir.join("wupi_splitgen_npc.json");
        for p in [&world, &player, &npc] {
            let _ = std::fs::remove_file(p);
        }

        // Save twice — stamps advance 1, then 2, and agree across the trio.
        let mut schema = WorldSchema::default();
        schema.summary = "gen test".into();
        schema.save_split(&world, &player, &npc).unwrap();
        for p in [&world, &player, &npc] {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
            assert_eq!(v["split_gen"].as_u64(), Some(1), "{} stamped", p.display());
        }
        let loaded = WorldSchema::load_split(&world, &player, &npc).unwrap();
        assert_eq!(loaded.split_gen, 1);
        loaded.save_split(&world, &player, &npc).unwrap();
        let loaded = WorldSchema::load_split(&world, &player, &npc).unwrap();
        assert_eq!(loaded.split_gen, 2, "the counter is monotonic across loads");

        // Mixed generation: rewind ONE file's stamp → refuse loudly.
        let text = std::fs::read_to_string(&player).unwrap();
        std::fs::write(&player, text.replace("\"split_gen\": 2", "\"split_gen\": 1")).unwrap();
        let err = WorldSchema::load_split(&world, &player, &npc).unwrap_err();
        assert!(
            err.to_string().contains("mixed-generation"),
            "error explains the refusal: {err}"
        );

        // Legacy unstamped trio loads (split_gen defaults 0, no refusal).
        std::fs::write(&world, "{}").unwrap();
        std::fs::write(&player, "{}").unwrap();
        std::fs::write(&npc, "{}").unwrap();
        let legacy = WorldSchema::load_split(&world, &player, &npc).unwrap();
        assert_eq!(legacy.split_gen, 0);

        for p in [&world, &player, &npc] {
            let _ = std::fs::remove_file(p);
        }
    }

    /// A parseable-but-NON-object slice (e.g. a hand-edited `[]`) refuses
    /// the load — the same refuse-don't-reset contract as the `entities`
    /// guard. The old silent `continue` loaded an all-defaults schema and
    /// the next autosave permanently overwrote the file.
    #[test]
    fn load_split_refuses_non_object_slice() {
        let dir = std::env::temp_dir();
        let world = dir.join("wupi_splitnonobj_world.json");
        let player = dir.join("wupi_splitnonobj_player.json");
        let npc = dir.join("wupi_splitnonobj_npc.json");
        let mut schema = WorldSchema::default();
        schema.summary = "kept".into();
        schema.save_split(&world, &player, &npc).unwrap();
        // Clobber ONE slice with a valid-JSON array.
        std::fs::write(&player, "[]").unwrap();
        let err = WorldSchema::load_split(&world, &player, &npc).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("not an object"),
            "error explains the refusal: {err}"
        );
        for p in [&world, &player, &npc] {
            let _ = std::fs::remove_file(p);
        }
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
        // (2026-08-21 AM/PM: 14:30 renders as 2:30 PM.)
        let clock = WorldClock { current_minutes: 3750, last_tick_minutes: 0 };
        assert_eq!(clock.render_clock_line().as_deref(), Some("Day 3, 2:30 PM"));
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
        assert!(rendered.contains("clock: Day 3, 12:00 AM"));
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
        ents.insert("world_clock".to_string(), Some(serde_json::Value::String("9999".into())));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        // The typed world_clock is unchanged.
        assert_eq!(schema.world_clock.current_minutes, 1000);
        assert_eq!(schema.world_clock.last_tick_minutes, 500);
        // The "world_clock" string landed in the entities map (it's just a
        // regular key from apply_delta's perspective). Whether it stays there
        // is the validator's call, not apply_delta's.
        assert_eq!(schema.entities.get("world_clock").and_then(|s| s.as_str()), Some("9999"));
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
        ents.insert("weather".to_string(), Some(serde_json::Value::String("sunny".into())));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        // The typed weather is unchanged.
        assert_eq!(schema.weather.condition, "heavy rain");
        assert_eq!(schema.weather.started_at_minutes, 1000);
        // The "weather" string landed in the entities map (legacy convention).
        assert_eq!(schema.entities.get("weather").and_then(|s| s.as_str()), Some("sunny"));
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
                    setting: "indoor".to_string(), ..Default::default()
                },
                Node {
                    id: "cellar".to_string(),
                    name: "The Cellar".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: "outdoor".to_string(), ..Default::default()
                },
                Node {
                    id: "market_square".to_string(),
                    name: "Market Square".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: "".to_string(), ..Default::default()
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
            setting: "".to_string(), ..Default::default()
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
            setting: "".to_string(), ..Default::default()
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

    // ---- resolve_node_id fuzzy matcher (2026-08-10, T52 Open Issue #1) ----
    // The tracker emits diegetic names ("Market Square") instead of bare slugs
    // ("market_square"). resolve_node_id normalizes + fuzzy-matches so legal
    // moves aren't rejected on a casing/spelling technicality.

    #[test]
    fn resolve_node_id_exact_match_is_fast_path() {
        let g = sample_travel_graph();
        assert_eq!(g.resolve_node_id("market_square"), Some("market_square".to_string()));
        assert_eq!(g.resolve_node_id("tavern"), Some("tavern".to_string()));
    }

    #[test]
    fn resolve_node_id_normalizes_diegetic_name_with_spaces() {
        // T52 case: "Market Square" → "market_square"
        let g = sample_travel_graph();
        assert_eq!(g.resolve_node_id("Market Square"), Some("market_square".to_string()));
        // Case-insensitive.
        assert_eq!(g.resolve_node_id("MARKET SQUARE"), Some("market_square".to_string()));
    }

    #[test]
    fn resolve_node_id_matches_diegetic_name_field() {
        // "The Rusty Anchor" is the node.name for id "tavern".
        let g = sample_travel_graph();
        assert_eq!(g.resolve_node_id("The Rusty Anchor"), Some("tavern".to_string()));
        assert_eq!(g.resolve_node_id("the rusty anchor"), Some("tavern".to_string()));
    }

    #[test]
    fn resolve_node_id_returns_none_for_genuinely_unknown() {
        let g = sample_travel_graph();
        assert_eq!(g.resolve_node_id("Mordor"), None);
        assert_eq!(g.resolve_node_id("nonexistent_node"), None);
    }

    // ---- P1c (2026-08-17 E4B shakedown): TRAVEL node minting + typo guard ----

    #[test]
    fn resolve_or_mint_creates_unknown_destination_then_autolinks() {
        // The playtest case: T49-50 `[TRAVEL king_s_road]` from
        // market-square never moved the player (unknown → reject). Now the
        // mint creates the node + the caller's auto-link arm wires the edge.
        let mut g = sample_travel_graph();
        g.current_node = Some("market_square".to_string());
        let id = g.resolve_or_mint_node("King's Road", &[]).expect("unknown destination mints");
        assert_eq!(id, "king_s_road", "slug derived from the emitted name");
        let node = g.find_node("king_s_road").expect("node exists");
        assert_eq!(node.name, "King's Road", "diegetic name preserved");
        assert!(node.neighbors.is_empty(), "mint carries no neighbors — the caller links");
        // The applier's auto-link arm: current_node ↔ minted node.
        assert!(g.link_nodes("market_square", "king_s_road"));
        assert!(g.is_adjacent_to_current("king_s_road"));
        let minted = g.find_node("king_s_road").unwrap();
        assert!(minted.neighbors.contains(&"market_square".to_string()), "bidirectional edge");
        g.current_node = Some("king_s_road".to_string());
        assert_eq!(g.current().unwrap().id, "king_s_road", "current_node advances");
    }

    #[test]
    fn resolve_or_mint_typo_guard_resolves_near_misses() {
        // "mrket square" must NOT mint a phantom twin of market_square.
        let mut g = sample_travel_graph();
        g.current_node = Some("tavern".to_string());
        let id = g.resolve_or_mint_node("mrket square", &[]).expect("near-miss resolves");
        assert_eq!(id, "market_square");
        assert_eq!(g.nodes.len(), 3, "no phantom node minted");
        // Diegetic-name typos resolve too ("The Rusty Ancor").
        let id2 = g.resolve_or_mint_node("The Rusty Ancor", &[]).expect("name typo resolves");
        assert_eq!(id2, "tavern");
        assert_eq!(g.nodes.len(), 3);
    }

    #[test]
    fn resolve_or_mint_genuinely_new_places_mint() {
        let mut g = sample_travel_graph();
        // Low similarity against every existing node → mint (the organic
        // world-growth path the E4B needs to leave town).
        let id = g.resolve_or_mint_node("ferry landing", &[]).expect("new place mints");
        assert_eq!(id, "ferry_landing");
        assert_eq!(g.nodes.len(), 4);
    }

    #[test]
    fn resolve_or_mint_refuses_at_cap_and_empty() {
        let mut g = sample_travel_graph();
        assert_eq!(g.resolve_or_mint_node("   ", &[]), None, "blank raw never mints");
        assert_eq!(g.resolve_or_mint_node("!!!", &[]), None, "slug-empty raw never mints");
        // Cap: fill to MAX_TRAVEL_NODES, then mint must refuse.
        for i in g.nodes.len()..MAX_TRAVEL_NODES {
            g.upsert_node(Node {
                id: format!("n{i}"),
                name: format!("Node {i}"),
                neighbors: vec![],
                setting: String::new(), ..Default::default()
            });
        }
        assert_eq!(g.nodes.len(), MAX_TRAVEL_NODES);
        assert_eq!(g.resolve_or_mint_node("brand new place", &[]), None, "cap refuses new nodes");
    }

    // ---- (2026-08-24 fix) garbage-identifier guard ----

    #[test]
    fn is_garbage_identifier_rejects_sentinels_and_numerals() {
        // JS/JSON leakage sentinels, any case.
        for bad in ["undefined", "UNDEFINED", "null", "None", "NaN", "true", "false"] {
            assert!(is_garbage_identifier(bad), "{bad:?} is garbage");
        }
        // No alphabetic chars at all: bare numerals + punctuation soup.
        for bad in ["1", "42", "007", "12345", "!!!", " - ", "?"] {
            assert!(is_garbage_identifier(bad), "{bad:?} is garbage");
        }
        // Trimmed-empty.
        assert!(is_garbage_identifier(""), "empty is garbage");
        assert!(is_garbage_identifier("   "), "whitespace-only is garbage");
    }

    #[test]
    fn is_garbage_identifier_passes_real_names() {
        for good in [
            "liam", "iron-forge", "Portsedge", "market_square", "greywater",
            "Marra", "港口", "Chloé",
        ] {
            assert!(!is_garbage_identifier(good), "{good:?} is a real id");
        }
        // Mixed but letter-bearing tokens are real (numbers + letters).
        assert!(!is_garbage_identifier("district9"), "letters present → real");
        assert!(!is_garbage_identifier("area_2b"), "letters present → real");
    }

    /// (2026-08-26 location-hygiene ruling) Parentheses never appear in a
    /// location — the live repro was a wizard-drafted `<location>` of
    /// "Earth (variable by scene)" seeding exactly that as the node label.
    #[test]
    fn clean_location_label_strips_parenthetical_qualifiers() {
        assert_eq!(clean_location_label("Earth (variable by scene)"), "Earth");
        assert_eq!(clean_location_label("Earth"), "Earth", "clean input is idempotent");
        assert_eq!(
            clean_location_label("Warehouse (docks) District"),
            "Warehouse District",
            "a closed run reads as a word break"
        );
        assert_eq!(
            clean_location_label("A ((nested) run) tail"),
            "A tail",
            "nested runs strip whole"
        );
        assert_eq!(clean_location_label("Earth (old"), "Earth", "dangling opener strips to end");
        assert_eq!(clean_location_label("Earth) split"), "Earth split", "stray closer becomes a space");
        assert_eq!(
            clean_location_label("  spaced   out  "),
            "spaced out",
            "whitespace runs collapse"
        );
        assert_eq!(clean_location_label("(everything)"), "", "caller keeps the original on empty");
    }

    /// The mint path derives BOTH the diegetic name and the slug id from the
    /// CLEANED label — "the warehouse (docks)" mints id `warehouse`.
    #[test]
    fn resolve_or_mint_cleans_parenthesized_mint_names() {
        let mut g = sample_travel_graph();
        let id = g
            .resolve_or_mint_node("the warehouse (docks)", &[])
            .expect("parenthesized destination mints");
        let node = g.find_node(&id).expect("minted node exists");
        assert_eq!(node.name, "the warehouse");
        assert!(!id.contains("dock"), "the id derives from the cleaned name: {id}");
    }

    /// The v0.30.0 live-test repro shape: garbage TRAVEL destinations never
    /// mint — the reject arm teaches instead of growing a phantom node.
    #[test]
    fn resolve_or_mint_never_mints_garbage_destinations() {
        let mut g = sample_travel_graph();
        let before = g.nodes.len();
        for bad in ["undefined", "null", "1", "42", "!!!", "NaN"] {
            assert_eq!(
                g.resolve_or_mint_node(bad, &[]),
                None,
                "garbage destination {bad:?} must never mint or resolve"
            );
        }
        assert_eq!(g.nodes.len(), before, "no phantom nodes were minted");
    }

    /// The happy twin: a fresh graph + a real destination mints + the caller
    /// (the [TRAVEL] applier's fall-through arm) assigns current_node —
    /// the never-None regression the live test flagged.
    #[test]
    fn fresh_graph_travel_mints_and_assigns_current_node() {
        let mut g = TravelGraph::default();
        assert!(!g.is_set());
        let id = g
            .resolve_or_mint_node("Portsedge", &[])
            .expect("real destination mints on a fresh graph");
        assert_eq!(id, "portsedge");
        assert!(g.find_node(&id).is_some(), "the minted node exists");
        // The applier's fall-through arm (first-move-from-None):
        g.current_node = Some(id.clone());
        assert_eq!(g.current_node.as_deref(), Some("portsedge"));
    }

    // ---- Recommendation 2 (2026-08-17): travel fragment alias + proper-noun mint naming ----

    #[test]
    fn travel_fragment_alias_resolves_shorthand_destination() {
        // "market" vs "market-square" scores ≈0.46 under the typo guard —
        // without the alias arm this MINTED a ghost twin of the real node.
        let mut g = sample_travel_graph();
        let id = g
            .resolve_or_mint_node("market", &[])
            .expect("shorthand resolves instead of minting");
        assert_eq!(id, "market_square");
        assert_eq!(g.nodes.len(), 3, "no phantom twin minted");
        // Through the diegetic NAME as well ("anchor" → The Rusty Anchor).
        let id2 = g.resolve_or_mint_node("anchor", &[]).expect("name fragment resolves");
        assert_eq!(id2, "tavern");
        assert_eq!(g.nodes.len(), 3);
        // A re-emitted bare shorthand finds a phrase-named mint ("greywater"
        // → the earlier "greywater-village" mint).
        g.upsert_node(Node {
            id: "greywater-village".to_string(),
            name: "Greywater Village".to_string(),
            neighbors: vec![],
            setting: String::new(), ..Default::default()
        });
        let id3 = g.resolve_or_mint_node("greywater", &[]).expect("re-finds the mint");
        assert_eq!(id3, "greywater-village");
        assert_eq!(g.nodes.len(), 4);
    }

    #[test]
    fn travel_fragment_alias_ambiguous_mints_and_noise_never_resolves() {
        // Two nodes contain "market" → ambiguous → the alias arm declines
        // (a wrong teleport is worse than a mint).
        let mut g = sample_travel_graph();
        g.upsert_node(Node {
            id: "market-stalls".to_string(),
            name: "Market Stalls".to_string(),
            neighbors: vec![],
            setting: String::new(), ..Default::default()
        });
        let id = g.resolve_or_mint_node("market", &[]).expect("ambiguous fragment mints fresh");
        assert_eq!(id, "market", "minted as its own node — no silent guess between the two");
        // Noise fragments ("the") must never subset-match a long compound id.
        let mut g2 = sample_travel_graph();
        g2.upsert_node(Node {
            id: "the-crooked-lantern-tavern".to_string(),
            name: "The Crooked Lantern Tavern".to_string(),
            neighbors: vec![],
            setting: String::new(), ..Default::default()
        });
        let id2 = g2
            .resolve_or_mint_node("the", &[])
            .expect("noise input is GIGO-minted, not aliased");
        assert_ne!(id2, "the-crooked-lantern-tavern", "stopword fragment never aliases");
    }

    #[test]
    fn travel_mint_names_the_node_from_the_narrative_proper_noun() {
        // The story says "Greywater Village" → the mint carries the real
        // place-name + a matching id (slugify's underscore style); the bare
        // shorthand re-finds it via the fragment alias.
        let mut g = sample_travel_graph();
        let narrative = [
            "I take the left branch toward a village in the next valley — the goatherd called it Greywater Village. I want to reach it before dark.",
        ];
        let id = g
            .resolve_or_mint_node("greywater", &narrative)
            .expect("new place mints");
        assert_eq!(id, "greywater_village", "id slugged from the narrative phrase");
        let node = g.find_node("greywater_village").unwrap();
        assert_eq!(node.name, "Greywater Village", "diegetic name from the story");
        assert_eq!(
            g.resolve_or_mint_node("greywater", &[]),
            Some("greywater_village".to_string()),
            "the bare shorthand re-finds the phrase-named mint"
        );
        // Lowercase-only mentions never anchor — the raw text is the name.
        let mut g2 = sample_travel_graph();
        let id2 = g2
            .resolve_or_mint_node("fog hollow", &["we sleep in a fog hollow off the track"])
            .expect("mint still works without a capitalized mention");
        assert_eq!(id2, "fog_hollow");
        assert_eq!(g2.find_node("fog_hollow").unwrap().name, "fog hollow");
    }

    // ---- Recommendation 2 (2026-08-17): NPC fragment alias + ghost guard ----

    fn harsk_registry() -> NpcRegistry {
        NpcRegistry {
            entries: vec![NpcEntry {
                prominence: NpcProminence::Named,
                id: "captain-harsk".to_string(),
                name: "Captain Harsk".to_string(),
                role: "watch captain".to_string(),
                tier: None,
                aliases: vec!["harsk of the watch".to_string()],
            }],
        }
    }

    #[test]
    fn npc_fragment_alias_resolves_shorthand_surface() {
        let reg = harsk_registry();
        // Exact id still first.
        assert_eq!(reg.resolve("captain-harsk").unwrap().id, "captain-harsk");
        // Shorthand via id words, via NAME words, and via ALIAS words.
        assert_eq!(reg.resolve("harsk").unwrap().id, "captain-harsk");
        assert_eq!(reg.resolve("captain").unwrap().id, "captain-harsk");
        assert_eq!(reg.resolve("watch").unwrap().id, "captain-harsk");
        // Unspecific (all words <4 chars) + genuinely unknown stay rejected.
        assert!(reg.resolve("mar").is_none());
        assert!(reg.resolve("vera").is_none());
        // Ambiguity declines: two entries contain "captain".
        let mut amb = harsk_registry();
        amb.entries.push(NpcEntry {
            prominence: NpcProminence::Named,
            id: "captain-brann".to_string(),
            name: "Captain Brann".to_string(),
            role: String::new(),
            tier: None,
            aliases: vec![],
        });
        assert!(amb.resolve("captain").is_none(), "no silent guess between captains");
        assert_eq!(amb.resolve("harsk").unwrap().id, "captain-harsk", "still unique through the name");
    }

    #[test]
    fn npc_register_shorthand_never_mints_a_ghost_twin() {
        let mut reg = harsk_registry();
        let inserted = reg.upsert_entry(NpcEntry {
            prominence: NpcProminence::Named,
            id: "harsk".to_string(),
            name: "Harsk".to_string(),
            role: String::new(),
            tier: None,
            aliases: vec![],
        });
        assert!(!inserted, "shorthand re-registration is the duplicate no-op");
        assert_eq!(reg.entries.len(), 1, "no ghost twin in the cast roster");
        assert_eq!(reg.entries[0].id, "captain-harsk", "the canonical entry stands");
        // A genuinely new NPC still registers.
        let inserted2 = reg.upsert_entry(NpcEntry {
            prominence: NpcProminence::Named,
            id: "mara".to_string(),
            name: "Mara".to_string(),
            role: String::new(),
            tier: None,
            aliases: vec![],
        });
        assert!(inserted2);
        assert_eq!(reg.entries.len(), 2);
    }

    #[test]
    fn discover_ghost_guard_resolves_shorthand_before_upserting() {
        // The [DISCOVER] applier's guard rides resolve_fragment_alias: a
        // shorthand discovery of a known node is a re-discovery no-op,
        // never a ghost twin (the same corruption class as the TRAVEL
        // mint twins and the NPC register ghosts).
        let g = sample_travel_graph();
        assert_eq!(
            g.resolve_fragment_alias("market"),
            Some("market_square".to_string())
        );
        assert_eq!(
            g.resolve_fragment_alias("greywater"),
            None,
            "unknown place is a genuine discovery — the guard lets it through"
        );
        // Two near-identical nodes already on the graph → ambiguity declines
        // (no silent guess); the discovery proceeds as-emitted.
        let mut g2 = sample_travel_graph();
        g2.upsert_node(Node {
            id: "market-square".to_string(),
            name: "Market Square".to_string(),
            neighbors: vec![],
            setting: String::new(), ..Default::default()
        });
        assert_eq!(
            g2.resolve_fragment_alias("market"),
            None,
            "ambiguous containment declines — the guard only blocks high-confidence dupes"
        );
    }

    /// (2026-08-20 audit P2-1) The DISCOVER twin guard runs the FULL chain
    /// (`resolve_existing_node`), not just the fragment alias: a separator
    /// variant of an authored id ("market square" / "market_square" vs the
    /// kebab `market-square`) has EQUAL word counts, so the strict-subset
    /// alias test passes it through — only the exact/slug/name + ≥0.75 typo
    /// arms catch it. Old behavior: a ghost twin; new: re-discovery no-op.
    #[test]
    fn resolve_existing_node_catches_separator_variant_twins() {
        let g = TravelGraph {
            nodes: vec![
                // (a) The name arm: the diegetic name matches the emitted text.
                Node {
                    id: "greywater-village".to_string(),
                    name: "Greywater Village".to_string(),
                    neighbors: vec![],
                    setting: String::new(), ..Default::default()
                },
                // (b) The P2-1 seam: kebab id, NO name — "market square" vs
                // "market-square" is 1 substitution (similarity ≈ 0.92).
                Node {
                    id: "market-square".to_string(),
                    name: String::new(),
                    neighbors: vec![],
                    setting: String::new(), ..Default::default()
                },
            ],
            current_node: None,
        };
        assert_eq!(
            g.resolve_existing_node("Greywater Village"),
            Some("greywater-village".to_string())
        );
        assert_eq!(
            g.resolve_existing_node("market square"),
            Some("market-square".to_string()),
            "the space/kebab/underscore variants are ONE place — no twin"
        );
        assert_eq!(
            g.resolve_existing_node("market_square"),
            Some("market-square".to_string())
        );
        assert_eq!(
            g.resolve_existing_node("market-square"),
            Some("market-square".to_string())
        );
        // A genuinely new place resolves to nothing — the discovery proceeds.
        assert_eq!(
            g.resolve_existing_node("abandoned windmill"),
            None,
            "unknown place stays discoverable"
        );
    }

    #[test]
    fn similarity_orders_exact_partial_and_distant() {
        assert!((similarity("market_square", "market_square") - 1.0).abs() < 1e-6);
        assert!(similarity("mrket_square", "market_square") >= 0.75, "single dropped char resolves");
        assert!(similarity("king_s_road", "market_square") < 0.75, "different place mints");
        assert_eq!(similarity("", "anything"), 0.0);
    }

    // ---- (2026-08-23 Playground) near-name resolution ----

    fn near_registry() -> NpcRegistry {
        let mut reg = NpcRegistry::default();
        reg.entries.push(NpcEntry {
            id: "kira".into(),
            name: "Kira".into(),
            role: "the herbalist".into(),
            aliases: vec!["kira".into(), "the herbalist".into()],
            prominence: NpcProminence::Named,
            tier: None,
        });
        reg.entries.push(NpcEntry {
            id: "brannoc".into(),
            name: "Brannoc".into(),
            role: String::new(),
            aliases: vec!["brannoc".into()],
            prominence: NpcProminence::Named,
            tier: None,
        });
        reg
    }

    #[test]
    fn near_name_kira_kyra_kiera_family() {
        let reg = near_registry();
        // Kyra vs Kira: both 4 chars (short threshold 1), one substitution
        // → distance 1 → collides.
        let hits = near_name_candidates("Kyra", &reg);
        assert!(hits.iter().any(|(id, _, d)| id == "kira" && *d == 1), "{hits:?}");
        // Kiera vs Kira: long threshold (Kiera is 5 chars, ≤2 allowed), one
        // deletion → distance 1 → collides.
        let hits = near_name_candidates("Kiera", &reg);
        assert!(hits.iter().any(|(id, _, d)| id == "kira" && *d == 1), "{hits:?}");
        // The LONG threshold in action: "Kiroc" vs "Kira" is exactly 2
        // edits (delete o, sub c→a) — allowed because Kiroc is ≥5 chars;
        // the short threshold (≤1) would refuse it. The same query stays 3
        // from "Kiara" → never a candidate.
        let mut with_kiara = near_registry();
        with_kiara.entries.push(NpcEntry {
            id: "kiara".into(),
            name: "Kiara".into(),
            ..Default::default()
        });
        let hits = near_name_candidates("Kiroc", &with_kiara);
        assert!(
            hits.iter().any(|(id, _, d)| id == "kira" && *d == 2),
            "2-away on a ≥5-char name collides: {hits:?}"
        );
        assert!(
            !hits.iter().any(|(id, _, _)| id == "kiara"),
            "3-away never collides: {hits:?}"
        );
        // The alias surface counts too ("the herbalist" vs "the herbilist").
        let hits = near_name_candidates("the herbilist", &reg);
        assert!(hits.iter().any(|(id, _, _)| id == "kira"), "{hits:?}");
        // Exact-normalized always: case + punctuation fold to distance 0.
        let hits = near_name_candidates("KIRA!", &reg);
        assert!(hits.iter().any(|(id, _, d)| id == "kira" && *d == 0), "{hits:?}");
    }

    #[test]
    fn near_name_benign_pairs_stay_clean() {
        let reg = near_registry();
        // A genuinely different name finds nothing.
        assert!(near_name_candidates("Marcus", &reg).is_empty());
        assert!(near_name_candidates("Jo", &reg).is_empty());
        // Two edits on a short pair is a different word (threshold 1):
        // "Ko" vs "Jo" is one substitution (collides); "Ba" vs "Jo" is two
        // (never collides).
        let mut reg2 = NpcRegistry::default();
        reg2.entries.push(NpcEntry {
            id: "jo".into(),
            name: "Jo".into(),
            ..Default::default()
        });
        assert!(near_name_candidates("Ko", &reg2)
            .iter()
            .any(|(id, _, d)| id == "jo" && *d == 1));
        assert!(near_name_candidates("Ba", &reg2).is_empty(), "distance 2 on a short pair is a different name");
        // Empty / punctuation-only queries never match.
        assert!(near_name_candidates("", &reg).is_empty());
        assert!(near_name_candidates("!!!", &reg).is_empty());
    }

    #[test]
    fn near_name_collision_guard_decision() {
        // The live [NPC_REGISTER] guard's core: the closest OTHER entry.
        let reg = near_registry();
        let hit = near_name_collision("Kyra", &reg, "kyra_new");
        assert_eq!(hit.as_ref().map(|(id, _, d)| (id.as_str(), *d)), Some(("kira", 1)));
        // Re-registering the same id never self-collides.
        assert!(near_name_collision("Kira", &reg, "kira").is_none());
        // A genuinely new name is clean — registration proceeds untouched.
        assert!(near_name_collision("Marcus", &reg, "marcus").is_none());
    }

    // ---- P1d (2026-08-17 E4B shakedown): TIME clamp + calendar coupling ----

    #[test]
    fn time_clamp_table_per_scene_mode() {
        assert_eq!(SceneMode::Combat.time_clamp_minutes(), 60);
        assert_eq!(SceneMode::Exploration.time_clamp_minutes(), 360);
        assert_eq!(SceneMode::Downtime.time_clamp_minutes(), 1440);
    }

    #[test]
    fn clamp_time_advance_kills_the_14_day_jump() {
        // The playtest regression: prev = Day 1 09:00 (780), the tracker
        // emitted +20175 min in ONE bracket. Downtime (the sleep case) clamps
        // to 24h; a clamp directive fires.
        let prev = 780i64;
        let requested = prev + 20_175;
        let (effective, dirs) =
            clamp_time_advance(prev, requested, SceneMode::Downtime, None, false);
        assert_eq!(effective, prev + 1440, "clamped to the 24h Downtime cap");
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].contains("clamped"), "the clamp warns the tracker");
        // Combat clamps to 1h, Exploration to 6h.
        let (e_combat, _) = clamp_time_advance(prev, requested, SceneMode::Combat, None, false);
        assert_eq!(e_combat, prev + 60);
        let (e_expl, _) = clamp_time_advance(prev, requested, SceneMode::Exploration, None, false);
        assert_eq!(e_expl, prev + 360);
    }

    #[test]
    fn clamp_time_advance_passes_legit_advances() {
        // An 8h sleep in Downtime is legal (T42's +360 was NOT the bug — the
        // 14-day jump was). No directive, no clamp.
        let (effective, dirs) = clamp_time_advance(780, 780 + 480, SceneMode::Downtime, None, false);
        assert_eq!(effective, 780 + 480);
        assert!(dirs.is_empty());
        // Same advance in Exploration (6h cap) clamps.
        let (effective, dirs) =
            clamp_time_advance(780, 780 + 480, SceneMode::Exploration, None, false);
        assert_eq!(effective, 780 + 360);
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn day_crossing_directive_fires_once_per_crossing() {
        // With a calendar label set + no [DATE] this turn, a midnight
        // crossing nudges the tracker to refresh the label — exactly once.
        let prev = 23 * 60; // 23:00, Day 1
        let (effective, dirs) = clamp_time_advance(
            prev,
            prev + 120, // crosses midnight
            SceneMode::Downtime,
            Some("17th of Peatfall, Year 214"),
            false,
        );
        assert_eq!(effective, prev + 120);
        assert_eq!(dirs.len(), 1, "one stale-calendar directive");
        assert!(dirs[0].contains("[DATE"));
        // A [DATE] on the same turn suppresses it.
        let (_, dirs) = clamp_time_advance(
            prev,
            prev + 120,
            SceneMode::Downtime,
            Some("17th of Peatfall, Year 214"),
            true,
        );
        assert!(dirs.is_empty(), "[DATE] the same turn = not stale");
        // The NEXT turn (prev now on Day 2) crossing again within the day
        // boundary fires again only when ANOTHER midnight is crossed.
        let (e2, dirs2) =
            clamp_time_advance(effective, effective + 60, SceneMode::Downtime, Some("label"), false);
        assert_eq!(e2, effective + 60);
        assert!(dirs2.is_empty(), "no new midnight — no repeat directive");
        // No calendar label → no directive ever.
        let (_, dirs3) = clamp_time_advance(prev, prev + 120, SceneMode::Downtime, None, false);
        assert!(dirs3.is_empty());
    }

    #[test]
    fn calendar_render_appends_day_suffix_only_when_48h_stale() {
        let mut schema = WorldSchema::default();
        schema.calendar = Some("17th of Peatfall, Year 214".into());
        schema.world_clock.current_minutes = 9 * 60; // Day 1, 09:00
        schema.calendar_synced_minutes = Some(9 * 60);
        let fresh = schema.render_for_prompt();
        assert!(fresh.contains("date: 17th of Peatfall"));
        assert!(!fresh.contains("day "), "fresh label renders clean");
        // 48h+ of drift → the true day counter rides along.
        schema.world_clock.current_minutes = 9 * 60 + 49 * 60; // +49h → Day 3, 10:00
        let stale = schema.render_for_prompt();
        assert!(stale.contains("day 3"), "the true day counters the stale label: {stale}");
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
                setting: "".to_string(), ..Default::default()
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
                setting: "".to_string(), ..Default::default()
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
                setting: "INDOOR".to_string(), ..Default::default()
            }],
            current_node: Some("hall".to_string()),
        };
        assert!(g.current_is_indoor());
    }

    // --- upsert_node (dynamic world-seeding, [DISCOVER] applier) ---

    #[test]
    fn upsert_node_inserts_new_node() {
        let mut g = TravelGraph::default();
        let inserted = g.upsert_node(Node {
            id: "shell_town".into(),
            name: "Shell Town".into(),
            neighbors: vec![],
            setting: "outdoor".into(), ..Default::default()
        });
        assert!(inserted, "first insert returns true");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.find_node("shell_town").unwrap().name, "Shell Town");
    }

    #[test]
    fn upsert_node_is_idempotent_on_duplicate_id() {
        // Re-discovering an existing id is a no-op (the tracker may re-emit it).
        let mut g = TravelGraph::default();
        g.upsert_node(Node { id: "shell_town".into(), name: "Shell Town".into(), neighbors: vec![], setting: String::new(), ..Default::default() });
        let inserted = g.upsert_node(Node { id: "shell_town".into(), name: "DIFFERENT NAME".into(), neighbors: vec![], setting: String::new(), ..Default::default() });
        assert!(!inserted, "duplicate id returns false (no-op)");
        assert_eq!(g.nodes.len(), 1, "no duplicate node added");
        // The original entry wins (first writer semantics).
        assert_eq!(g.find_node("shell_town").unwrap().name, "Shell Town");
    }

    #[test]
    fn upsert_node_back_links_existing_neighbors() {
        // Discover a new node that names an EXISTING neighbor: the existing
        // node gains a back-edge so the graph stays undirected.
        let mut g = TravelGraph {
            nodes: vec![Node { id: "loguetown".into(), name: "Loguetown".into(), neighbors: vec![], setting: String::new(), ..Default::default() }],
            current_node: None,
        };
        g.upsert_node(Node {
            id: "shell_town".into(),
            name: "Shell Town".into(),
            neighbors: vec!["loguetown".into()],
            setting: String::new(), ..Default::default()
        });
        // loguetown should now list shell_town as a neighbor (back-link).
        assert!(g.find_node("loguetown").unwrap().neighbors.contains(&"shell_town".to_string()),
            "back-link added to existing neighbor");
        // And shell_town lists loguetown (the forward edge, as authored).
        assert!(g.find_node("shell_town").unwrap().neighbors.contains(&"loguetown".to_string()));
    }

    #[test]
    fn upsert_node_dangling_forward_edge_kept_for_unknown_neighbor() {
        // A named neighbor that doesn't exist yet keeps its forward edge; the
        // reverse lands when that node is itself discovered (eventually-consistent).
        let mut g = TravelGraph::default();
        g.upsert_node(Node {
            id: "shell_town".into(),
            name: "Shell Town".into(),
            neighbors: vec!["foosha".into()], // foosha doesn't exist yet
            setting: String::new(), ..Default::default()
        });
        assert!(g.find_node("shell_town").unwrap().neighbors.contains(&"foosha".to_string()),
            "forward edge to unknown neighbor kept");
        // Now discover foosha naming shell_town — the back-link resolves.
        g.upsert_node(Node {
            id: "foosha".into(),
            name: "Foosha Village".into(),
            neighbors: vec!["shell_town".into()],
            setting: String::new(), ..Default::default()
        });
        assert!(g.find_node("shell_town").unwrap().neighbors.contains(&"foosha".to_string()));
        assert!(g.find_node("foosha").unwrap().neighbors.contains(&"shell_town".to_string()));
    }

    #[test]
    fn upsert_node_empty_id_returns_false() {
        let mut g = TravelGraph::default();
        let inserted = g.upsert_node(Node { id: String::new(), name: "X".into(), neighbors: vec![], setting: String::new(), ..Default::default() });
        assert!(!inserted);
        assert!(g.nodes.is_empty());
    }

    // --- link_nodes (the [TRAVEL] auto-link, 2026-08-10) ---
    // When the player travels between two KNOWN but non-adjacent nodes, the
    // movement is evidence the locations are connected. link_nodes forms the
    // bidirectional edge idempotently.

    #[test]
    fn link_nodes_forms_bidirectional_edge() {
        // Two known nodes (tavern, market_square) that are already adjacent —
        // use two NON-adjacent nodes instead. Build a graph where cellar + a
        // new "docks" node are disconnected.
        let mut g = TravelGraph {
            nodes: vec![
                Node { id: "tavern".into(), name: "Tavern".into(), neighbors: vec!["cellar".into()], setting: "indoor".into(), ..Default::default() },
                Node { id: "cellar".into(), name: "Cellar".into(), neighbors: vec!["tavern".into()], setting: "".into(), ..Default::default() },
                Node { id: "docks".into(), name: "Docks".into(), neighbors: vec![], setting: "outdoor".into(), ..Default::default() },
            ],
            current_node: Some("tavern".into()),
        };
        // docks is known but NOT adjacent to tavern. Link them.
        let changed = g.link_nodes("tavern", "docks");
        assert!(changed, "linking two unlinked known nodes must report a change");
        // Bidirectional: tavern→docks + docks→tavern.
        assert!(g.find_node("tavern").unwrap().neighbors.contains(&"docks".to_string()));
        assert!(g.find_node("docks").unwrap().neighbors.contains(&"tavern".to_string()));
    }

    #[test]
    fn link_nodes_is_idempotent_when_already_linked() {
        // tavern ↔ cellar already linked in sample_travel_graph.
        let mut g = sample_travel_graph();
        let changed = g.link_nodes("tavern", "cellar");
        assert!(!changed, "linking two already-linked nodes must report no change");
        // No duplicate edges.
        let count = g.find_node("tavern").unwrap().neighbors.iter().filter(|n| *n == "cellar").count();
        assert_eq!(count, 1, "no duplicate edge after idempotent re-link");
    }

    #[test]
    fn link_nodes_noop_for_unknown_id() {
        let mut g = sample_travel_graph();
        // One side unknown.
        assert!(!g.link_nodes("tavern", "nonexistent"));
        // Both unknown.
        assert!(!g.link_nodes("ghost_a", "ghost_b"));
        // tavern's neighbors unchanged.
        assert_eq!(g.find_node("tavern").unwrap().neighbors.len(), 2);
    }

    #[test]
    fn link_nodes_noop_for_identical_ids() {
        let mut g = sample_travel_graph();
        assert!(!g.link_nodes("tavern", "tavern"), "self-link is a no-op");
    }

    // --- BootstrapAnchors::from_model_output (the intro-derived seed parser,
    // 2026-08-10) ---

    #[test]
    fn bootstrap_parses_full_extraction() {
        let raw = r#"{"time":"Day 1, 21:00","weather":"thick fog","location_id":"crooked_lantern","location_name":"The Crooked Lantern"}"#;
        let a = BootstrapAnchors::from_model_output(raw).expect("full extraction parses");
        // Day 1 21:00 = 0*1440 + 21*60 = 1260 minutes.
        assert_eq!(a.time_minutes, Some(1260));
        assert_eq!(a.weather.as_deref(), Some("thick fog"));
        assert_eq!(a.location, Some(("crooked_lantern".into(), "The Crooked Lantern".into())));
    }

    #[test]
    fn bootstrap_parses_partial_extraction() {
        // Only time + weather, no location — a valid partial (the intro
        // mentioned time/weather but no specific place).
        let raw = r#"{"time":"Day 2, 08:30","weather":"clear morning"}"#;
        let a = BootstrapAnchors::from_model_output(raw).expect("partial parses");
        assert_eq!(a.time_minutes, Some(1440 + 8 * 60 + 30));
        assert_eq!(a.weather.as_deref(), Some("clear morning"));
        assert!(a.location.is_none(), "absent location → None");
    }

    #[test]
    fn bootstrap_empty_object_is_all_none() {
        // The model may emit `{}` when the intro gives nothing. Not an error —
        // the caller's sensible-defaults fallback seeds each field.
        let a = BootstrapAnchors::from_model_output("{}").expect("empty object parses");
        assert!(a.time_minutes.is_none());
        assert!(a.weather.is_none());
        assert!(a.location.is_none());
    }

    #[test]
    fn bootstrap_unparseable_time_becomes_none_not_error() {
        // A bare word like "night" doesn't match the "Day N, HH:MM" parser.
        // time_minutes → None (soft), but the rest still parses.
        let raw = r#"{"time":"night","weather":"storm"}"#;
        let a = BootstrapAnchors::from_model_output(raw).expect("parses despite bad time");
        assert!(a.time_minutes.is_none(), "unparseable time → None, not Err");
        assert_eq!(a.weather.as_deref(), Some("storm"));
    }

    #[test]
    fn bootstrap_partial_location_pair_is_dropped() {
        // location_id without location_name (or vice versa) is useless → None.
        let raw = r#"{"location_id":"tavern"}"#;
        let a = BootstrapAnchors::from_model_output(raw).expect("parses");
        assert!(a.location.is_none(), "half a location pair → None");
    }

    #[test]
    fn bootstrap_strips_markdown_fences_and_channel_protocol() {
        // The model may wrap JSON in ```json fences + the Gemma4 channel
        // protocol. The parser must strip both (mirrors SchemaDelta's pipeline).
        let raw = "<|channel>thought\nthe scene is at night\n<channel|>```json\n{\"time\":\"Day 1, 22:00\"}\n```";
        let a = BootstrapAnchors::from_model_output(raw).expect("wrapped JSON parses");
        assert_eq!(a.time_minutes, Some(22 * 60));
    }

    #[test]
    fn bootstrap_unparseable_json_returns_err() {
        // Genuinely broken JSON → Err (the caller falls through to defaults).
        let result = BootstrapAnchors::from_model_output("not json at all");
        assert!(result.is_err());
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
        ents.insert("travel_graph".to_string(), Some(serde_json::Value::String("injected".into())));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        // The typed travel_graph is unchanged.
        assert_eq!(schema.travel_graph, original);
        // The "travel_graph" string landed in the entities map (legacy).
        assert_eq!(
            schema.entities.get("travel_graph").and_then(|s| s.as_str()),
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

    // ---------- Phase 5A: NpcRegistry + Presence (2026-07-29) ----------

    /// Architectural invariant: the npc_registry is outside the LLM delta path
    /// (mirrors apply_delta_does_not_touch_travel_graph). A delta carrying
    /// "npc_registry" in its entities map must NOT mutate the typed field —
    /// the registry is seeded once from the card and read-only thereafter.
    #[test]
    fn apply_delta_does_not_touch_npc_registry() {
        let mut schema = WorldSchema::default();
        schema.npc_registry = NpcRegistry {
            entries: vec![NpcEntry {
                prominence: NpcProminence::Named,
                id: "mara".into(),
                name: "Mara".into(),
                role: "innkeep".into(),
                tier: Some("soldier".into()),
                aliases: vec!["innkeep".into()],
            }],
        };
        let original = schema.npc_registry.clone();
        let mut ents = HashMap::new();
        ents.insert("npc_registry".to_string(), Some(serde_json::Value::String("injected".into())));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert_eq!(schema.npc_registry, original, "registry must be LLM-immutable");
        assert_eq!(
            schema.entities.get("npc_registry").and_then(|s| s.as_str()),
            Some("injected"),
            "the injected key lands in entities (legacy), NOT the typed field"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // (2026-08-18 Dedicated-NPC interior state) immunity, render, reaper,
    // derived protection, eviction, and typed-cap pins.
    // ─────────────────────────────────────────────────────────────────────

    /// Architectural invariant: npc_interior is outside the LLM delta path
    /// (mirrors the registry/presences pins). The only writers are the
    /// bracket appliers, the PRESENCE stamp, and the tick reaper.
    #[test]
    fn apply_delta_does_not_touch_npc_interior() {
        let mut schema = WorldSchema::default();
        schema.npc_interior.insert(
            "mara".into(),
            NpcInterior {
                mood: Some("suspicious".into()),
                intent: Some("hide the ring".into()),
                ..NpcInterior::default()
            },
        );
        let original = schema.npc_interior.clone();
        let mut ents = HashMap::new();
        ents.insert(
            "npc_interior".to_string(),
            Some(serde_json::Value::String("injected".into())),
        );
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert_eq!(schema.npc_interior, original, "interior must be LLM-immutable");
    }

    /// merge_patch has NO npc_interior arm — a full-replace attempt hits the
    /// unknown-top-level-field error (the structural immunity).
    #[test]
    fn merge_patch_refuses_npc_interior() {
        let mut schema = WorldSchema::default();
        let patch = serde_json::json!({ "npc_interior": { "mara": { "mood": "forged" } } });
        let err = schema.merge_patch(patch).expect_err("must refuse");
        assert!(err.contains("unknown top-level field"), "error should explain: {err}");
    }

    #[test]
    fn minds_render_present_only_capped_and_flattened() {
        let mut s = WorldSchema::default();
        s.npc_registry = NpcRegistry {
            entries: vec![NpcEntry {
                id: "mara".into(),
                name: "Mara".into(),
                role: "innkeep".into(),
                tier: None,
                aliases: vec![],
                prominence: NpcProminence::Named,
            }],
        };
        // On-camera Mara with a full interior.
        s.presences = vec![Presence {
            npc_id: "mara".into(),
            name: "Mara".into(),
            stance: "behind the bar".into(),
            ttl: PRESENCE_GRACE_RESET,
        }];
        s.npc_interior.insert(
            "mara".into(),
            NpcInterior {
                mood: Some("suspicious".into()),
                intent: Some("get her out".into()),
                items: vec![equipment::StackItem {
                    name: "Worn Ring".into(),
                    ..equipment::StackItem::default()
                }],
                ..NpcInterior::default()
            },
        );
        // Off-camera Corin with a full interior — must NOT render.
        s.npc_interior.insert(
            "corin".into(),
            NpcInterior {
                mood: Some("cheerful".into()),
                ..NpcInterior::default()
            },
        );
        let rendered = s.render_for_prompt();
        let minds_line = rendered
            .lines()
            .find(|l| l.starts_with("minds: "))
            .expect("minds line renders for the present NPC");
        assert!(minds_line.contains("Mara [suspicious"), "mood renders: {minds_line}");
        assert!(minds_line.contains("intends \"get her out\""), "intent renders: {minds_line}");
        assert!(minds_line.contains("carries Worn Ring"), "items render: {minds_line}");
        assert!(!rendered.contains("cheerful"), "off-camera interior never renders");

        // (2026-08-19 zone sweep) The `wearing:` line — present-NPC outfits
        // (seeded from npc-card clothing) render SEPARATELY from the held
        // rack; an outfit is never a shopping bag.
        s.npc_interior.insert(
            "mara".into(),
            NpcInterior {
                mood: Some("suspicious".into()),
                intent: Some("get her out".into()),
                worn: vec![
                    equipment::StackItem { name: "Linen Shirt".into(), ..Default::default() },
                    equipment::StackItem { name: "Wool Trousers".into(), ..Default::default() },
                ],
                items: vec![equipment::StackItem {
                    name: "Worn Ring".into(),
                    ..Default::default()
                }],
                ..NpcInterior::default()
            },
        );
        let rendered = s.render_for_prompt();
        let wearing_line = rendered
            .lines()
            .find(|l| l.starts_with("wearing: "))
            .expect("wearing line renders for the present NPC");
        assert!(
            wearing_line.contains("Mara(Linen Shirt, Wool Trousers)"),
            "outfit renders by name: {wearing_line}"
        );
        let holding_line = rendered
            .lines()
            .find(|l| l.starts_with("holding: "))
            .expect("holding line renders for the present NPC");
        assert!(
            holding_line.contains("Mara(Worn Ring"),
            "held items stay on the rack line: {holding_line}"
        );
        assert!(!wearing_line.contains("Worn Ring"), "held items never bleed into the outfit");
        // Archived interiors render the recall stub instead.
        s.npc_interior.insert(
            "mara".into(),
            NpcInterior {
                archived: Some("suspicious; get her out; held 1 item(s)".into()),
                ..NpcInterior::default()
            },
        );
        let rendered = s.render_for_prompt();
        assert!(
            rendered.lines().any(|l| l.starts_with("minds: ") && l.contains("recalls:")),
            "archived interior renders the stub"
        );
        // Prompt cap: cap+2 present interior-bearing NPCs → the cap shown +
        // marker (was 8→6 in the cap-6 era; the 2026-08-21 raises kept
        // missing this fixture — now derived from the const so it can't
        // rot again).
        let crowded = NPC_MINDS_PROMPT_CAP + 2;
        s.presences = (0..crowded)
            .map(|i| Presence {
                npc_id: format!("npc_{i}"),
                name: format!("N{i}"),
                stance: String::new(),
                ttl: PRESENCE_GRACE_RESET,
            })
            .collect();
        s.npc_interior.clear();
        for i in 0..crowded {
            s.npc_interior.insert(
                format!("npc_{i}"),
                NpcInterior {
                    mood: Some("m".to_string()),
                    ..NpcInterior::default()
                },
            );
        }
        let rendered = s.render_for_prompt();
        let minds_line = rendered
            .lines()
            .find(|l| l.starts_with("minds: "))
            .expect("minds line renders");
        assert!(minds_line.contains("(+2 more)"), "overflow marker: {minds_line}");
        assert_eq!(
            minds_line.matches('[').count(),
            NPC_MINDS_PROMPT_CAP,
            "exactly the cap entries shown"
        );
    }

    /// (2026-08-18 audit) The cap-6 `minds:` selection is IMPORTANCE-RANKED,
    /// never positional: a `core` cast member and a reaper-protected NPC
    /// (here: a Bonded relationship) positioned LAST in a crowded scene
    /// still render their interior — ambient discovered NPCs take the
    /// `(+N more)` cut instead. Pins the render's rank order too: core
    /// leads, protected next, ambient after (the order the lean render's
    /// trailing cut relies on).
    #[test]
    fn minds_cap_ranks_core_and_protected_first() {
        use crate::relationship::{RelationshipState, RelationshipTier};

        let mut s = WorldSchema::default();
        // cap+1 ambient named + 1 core + 1 bonded (derived from the const,
        // same anti-rot discipline as the sibling cap test — the scene is
        // deliberately over-cap so the marker fires) — the two
        // principals LAST in presences order (the adversarial arrangement: a
        // positional first-N selection would drop exactly them).
        let ambient_count = NPC_MINDS_PROMPT_CAP + 1;
        let mut entries: Vec<NpcEntry> = (0..ambient_count)
            .map(|i| NpcEntry {
                id: format!("amb{i}"),
                name: format!("Amb{i}"),
                role: String::new(),
                tier: None,
                aliases: vec![],
                prominence: NpcProminence::Named,
            })
            .collect();
        for (id, name, prominence) in [
            ("villain_mara", "Mara", NpcProminence::Core),
            ("spouse", "Spouse", NpcProminence::Named),
        ] {
            entries.push(NpcEntry {
                id: id.into(),
                name: name.into(),
                role: String::new(),
                tier: None,
                aliases: vec![],
                prominence,
            });
        }
        s.npc_registry = NpcRegistry { entries };
        s.relationships.insert(
            "spouse".into(),
            RelationshipState {
                tier: RelationshipTier::Bonded,
                tier_entered_at_minutes: 0,
                events: Vec::new(),
                volatility: 1.0,
            },
        );
        let mut presences: Vec<Presence> = (0..ambient_count)
            .map(|i| Presence {
                npc_id: format!("amb{i}"),
                name: format!("Amb{i}"),
                stance: String::new(),
                ttl: PRESENCE_GRACE_RESET,
            })
            .collect();
        for (id, name) in [("villain_mara", "Mara"), ("spouse", "Spouse")] {
            presences.push(Presence {
                npc_id: id.into(),
                name: name.into(),
                stance: String::new(),
                ttl: PRESENCE_GRACE_RESET,
            });
        }
        s.presences = presences;
        for i in 0..ambient_count {
            s.npc_interior.insert(
                format!("amb{i}"),
                NpcInterior {
                    mood: Some("wary".into()),
                    ..NpcInterior::default()
                },
            );
        }
        for id in ["villain_mara", "spouse"] {
            s.npc_interior.insert(
                id.into(),
                NpcInterior {
                    mood: Some("wary".into()),
                    ..NpcInterior::default()
                },
            );
        }
        let rendered = s.render_for_prompt();
        let minds_line = rendered
            .lines()
            .find(|l| l.starts_with("minds: "))
            .expect("minds renders for a present-interior scene");
        assert!(minds_line.contains("Mara ["), "core principal survives the cap: {minds_line}");
        assert!(
            minds_line.contains("Spouse ["),
            "Bonded principal survives the cap: {minds_line}"
        );
        assert_eq!(
            minds_line.matches('[').count(),
            NPC_MINDS_PROMPT_CAP,
            "exactly the cap entries shown: {minds_line}"
        );
        assert!(
            minds_line.contains("(+3 more)"),
            "the 3 overflow slots are ambient: {minds_line}"
        );
        let tail = format!("Amb{}", ambient_count - 1);
        assert!(
            !minds_line.contains(&tail),
            "the tail-most ambient entries take the cut: {minds_line}"
        );
        assert!(!minds_line.contains("Amb4"), "an ambient NPC took the cut: {minds_line}");
        let core_pos = minds_line.find("Mara [").expect("core entry present");
        let prot_pos = minds_line.find("Spouse [").expect("protected entry present");
        let amb_pos = minds_line.find("Amb0").expect("ambient entry present");
        assert!(
            core_pos < prot_pos && prot_pos < amb_pos,
            "rank order: core leads, protected next, ambient after: {minds_line}"
        );
    }

    /// Prime-Mandate pin: the schema-engine prompt serializer carries ONLY
    /// the diff-relevant fields — `npc_interior` (like every referee-owned
    /// collection) must never ride the delta/translation/
    /// progression prompts. A future "serialize the whole struct" regression
    /// reintroduces the 5-10× prompt bloat the 2026-08-16 audit H2 killed.
    #[test]
    fn to_json_prompt_excludes_npc_interior() {
        let mut s = WorldSchema::default();
        s.npc_interior.insert(
            "mara".into(),
            NpcInterior {
                mood: Some("suspicious".into()),
                intent: Some("hide the ring before she looks".into()),
                items: vec![equipment::StackItem {
                    name: "Worn Ring".into(),
                    ..equipment::StackItem::default()
                }],
                ..NpcInterior::default()
            },
        );
        let text = s.to_json_prompt();
        assert!(!text.contains("npc_interior"), "field key never serializes: {text}");
        assert!(
            !text.contains("hide the ring"),
            "interior free text never serializes: {text}"
        );
        let v: serde_json::Value = serde_json::from_str(&text).expect("valid prompt json");
        for k in v.as_object().expect("object").keys() {
            assert!(
                matches!(k.as_str(), "summary" | "recent_events" | "entities" | "entities_trimmed"),
                "unexpected prompt field {k}: {text}"
            );
        }
    }

    /// Absolute ceiling for the `minds:` line: cap × entry-cap entries + the
    /// joiner + overflow marker — even a hand-edited save full of maxed-out
    /// interiors can't push the always-on prompt line past it.
    #[test]
    fn minds_line_absolute_char_bound() {
        let mut s = WorldSchema::default();
        // 8 present NPCs, each interior rendering at the full 200-char
        // entry cap (40-char name + 60-char mood + 160-char intent).
        s.presences = (0..8)
            .map(|i| Presence {
                npc_id: format!("npc{i}"),
                name: "N".repeat(40),
                stance: String::new(),
                ttl: PRESENCE_GRACE_RESET,
            })
            .collect();
        for i in 0..8 {
            s.npc_interior.insert(
                format!("npc{i}"),
                NpcInterior {
                    mood: Some("m".repeat(60)),
                    intent: Some("i".repeat(160)),
                    ..NpcInterior::default()
                },
            );
        }
        let rendered = s.render_for_prompt();
        let minds_len = rendered
            .lines()
            .find(|l| l.starts_with("minds: "))
            .map(|l| l.chars().count())
            .expect("minds renders");
        assert!(
            minds_len <= NPC_MINDS_PROMPT_CAP * NPC_MINDS_ENTRY_CHAR_CAP + 40,
            "minds line {minds_len} chars exceeds the crowded-tavern ceiling"
        );
    }

    #[test]
    fn reaper_archives_stale_named_only() {
        let mut s = WorldSchema::default();
        s.npc_registry = NpcRegistry {
            entries: vec![
                NpcEntry {
                    id: "core_elder".into(),
                    name: "Elder".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Core,
                },
                NpcEntry {
                    id: "stale_merchant".into(),
                    name: "Merchant".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Named,
                },
            ],
        };
        let ttl_minutes = crate::settings::NPC_REAP_NAMED_AFTER_DAYS * 1440;
        let now = 100_000;
        for id in ["core_elder", "stale_merchant"] {
            s.npc_interior.insert(
                id.into(),
                NpcInterior {
                    mood: Some("m".into()),
                    intent: Some("i".into()),
                    items: vec![], // empty rack: no derived protection
                    last_seen_minutes: now - ttl_minutes - 1,
                    ..NpcInterior::default()
                },
            );
        }
        let reaped = s.reap_stale_npc_interiors(now);
        assert_eq!(reaped, 1, "only the stale named NPC archives");
        let core = s.npc_interior.get("core_elder").unwrap();
        assert!(core.archived.is_none() && core.mood.is_some(), "core is reaper-immune");
        let stale = s.npc_interior.get("stale_merchant").unwrap();
        assert!(stale.archived.is_some(), "stale named archives");
        assert!(stale.mood.is_none() && stale.intent.is_none() && stale.items.is_empty());
        // Just-inside-TTL does NOT reap.
        s.npc_interior.get_mut("stale_merchant").unwrap().archived = None;
        s.npc_interior.get_mut("stale_merchant").unwrap().mood = Some("m".into());
        s.npc_interior.get_mut("stale_merchant").unwrap().last_seen_minutes = now - ttl_minutes + 10;
        assert_eq!(s.reap_stale_npc_interiors(now), 0, "inside the TTL: no reap");
        // Dormant clock (0) never measures.
        assert_eq!(s.reap_stale_npc_interiors(0), 0);
        // last_seen 0 = unknown, never instantly stale.
        s.npc_interior.get_mut("stale_merchant").unwrap().last_seen_minutes = 0;
        assert_eq!(s.reap_stale_npc_interiors(now), 0);
    }

    /// (2026-08-18 reaper follow-up — the left-behind-family guard) Derived
    /// protection: relationship extremes, pending tasks, and held items each
    /// independently block the archive — the discovered spouse stays
    /// full-state while you're away.
    #[test]
    fn reaper_derived_protection_blocks_archive() {
        let mut s = WorldSchema::default();
        let ttl_minutes = crate::settings::NPC_REAP_NAMED_AFTER_DAYS * 1440;
        let now = 100_000;
        let stale = now - ttl_minutes - 1;
        let ids = ["spouse", "nemesis", "task_holder", "item_holder", "stranger"];
        s.npc_registry = NpcRegistry {
            entries: ids
                .iter()
                .map(|id| NpcEntry {
                    id: id.to_string(),
                    name: id.to_string(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Named,
                })
                .collect(),
        };
        for id in ids {
            s.npc_interior.insert(
                id.into(),
                NpcInterior {
                    mood: Some("m".into()),
                    last_seen_minutes: stale,
                    ..NpcInterior::default()
                },
            );
        }
        use crate::relationship::{RelationshipState, RelationshipTier};
        s.relationships.insert(
            "spouse".into(),
            RelationshipState {
                tier: RelationshipTier::Bonded,
                tier_entered_at_minutes: 0,
                events: Vec::new(),
                volatility: 1.0,
            },
        );
        s.relationships.insert(
            "nemesis".into(),
            RelationshipState {
                tier: RelationshipTier::Nemesis,
                tier_entered_at_minutes: 0,
                events: Vec::new(),
                volatility: 1.0,
            },
        );
        s.offscreen_tasks.push(crate::offscreen_task::OffScreenTask {
            npc_id: "task_holder".into(),
            description: "scout the ridge".into(),
            difficulty: crate::offscreen_task::TaskDifficulty::Routine,
            suitability: crate::offscreen_task::Suitability::Adequate,
            resolves_at_minutes: now + 5_000,
            resolved: false,
        });
        s.npc_interior.get_mut("item_holder").unwrap().items = vec![equipment::StackItem {
            name: "Player's Stolen Ring".into(),
            ..equipment::StackItem::default()
        }];
        // A resolved task does NOT protect.
        s.offscreen_tasks.push(crate::offscreen_task::OffScreenTask {
            npc_id: "stranger".into(),
            description: "fetch water".into(),
            difficulty: crate::offscreen_task::TaskDifficulty::Trivial,
            suitability: crate::offscreen_task::Suitability::Adequate,
            resolves_at_minutes: now - 1,
            resolved: true,
        });

        let reaped = s.reap_stale_npc_interiors(now);
        assert_eq!(reaped, 1, "only the unprotected stranger archives");
        for id in ["spouse", "nemesis", "task_holder", "item_holder"] {
            assert!(
                s.npc_interior.get(id).unwrap().archived.is_none(),
                "{id} is derived-protected"
            );
        }
        assert!(s.npc_interior.get("stranger").unwrap().archived.is_some());
    }

    #[test]
    fn evict_archived_registry_entry_lru_and_pins() {
        let mut s = WorldSchema::default();
        s.npc_registry = NpcRegistry {
            entries: vec![
                NpcEntry {
                    id: "pinned_core".into(),
                    name: "Core".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Core,
                },
                NpcEntry {
                    id: "live_named".into(),
                    name: "Live".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Named,
                },
                NpcEntry {
                    id: "arch_fresh".into(),
                    name: "Fresh".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Named,
                },
                NpcEntry {
                    id: "arch_stale".into(),
                    name: "Stale".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                    prominence: NpcProminence::Named,
                },
            ],
        };
        // pinned_core: archived but CORE → pinned.
        s.npc_interior.insert(
            "pinned_core".into(),
            NpcInterior {
                archived: Some("stub".into()),
                last_seen_minutes: 1,
                ..NpcInterior::default()
            },
        );
        // live_named: no archive → not evictable.
        s.npc_interior.insert(
            "live_named".into(),
            NpcInterior {
                last_seen_minutes: 1,
                ..NpcInterior::default()
            },
        );
        // arch_fresh: archived recently.
        s.npc_interior.insert(
            "arch_fresh".into(),
            NpcInterior {
                archived: Some("stub".into()),
                last_seen_minutes: 9_000,
                ..NpcInterior::default()
            },
        );
        // arch_stale: archived, oldest last_seen → the LRU victim.
        s.npc_interior.insert(
            "arch_stale".into(),
            NpcInterior {
                archived: Some("stub".into()),
                last_seen_minutes: 2_000,
                ..NpcInterior::default()
            },
        );
        // A presence for arch_stale would pin it — ensure it's absent.
        assert_eq!(
            s.evict_archived_registry_entry().as_deref(),
            Some("arch_stale"),
            "the least-recently-seen archived discovered entry evicts"
        );
        assert!(s.npc_registry.find("arch_stale").is_none());
        assert!(s.npc_interior.get("arch_stale").is_none());
        assert!(s.npc_registry.find("pinned_core").is_some(), "core stays");
        assert!(s.npc_registry.find("live_named").is_some(), "live stays");
        assert!(s.npc_registry.find("arch_fresh").is_some(), "fresher archive stays");
        // Present NPCs are pinned from eviction too.
        s.presences = vec![Presence {
            npc_id: "arch_fresh".into(),
            name: "Fresh".into(),
            stance: String::new(),
            ttl: PRESENCE_GRACE_RESET,
        }];
        assert_eq!(
            s.evict_archived_registry_entry(),
            None,
            "the only remaining archived entry is on-camera — nothing evicts"
        );
    }

    #[test]
    fn enforce_typed_caps_sweeps_orphan_interiors_and_caps_items() {
        let mut s = WorldSchema::default();
        s.npc_registry = NpcRegistry {
            entries: vec![NpcEntry {
                id: "mara".into(),
                name: "Mara".into(),
                role: String::new(),
                tier: None,
                aliases: vec![],
                prominence: NpcProminence::Named,
            }],
        };
        s.npc_interior.insert(
            "ghost_npc".into(),
            NpcInterior {
                mood: Some("orphan".into()),
                ..NpcInterior::default()
            },
        );
        let mut hoarder = NpcInterior::default();
        hoarder.items = (0..NPC_INTERIOR_ITEMS_MAX + 5)
            .map(|i| equipment::StackItem {
                name: format!("Item {i}"),
                ..equipment::StackItem::default()
            })
            .collect();
        s.npc_interior.insert("mara".into(), hoarder);
        s.enforce_typed_caps();
        assert!(!s.npc_interior.contains_key("ghost_npc"), "orphan interior swept");
        assert_eq!(
            s.npc_interior.get("mara").unwrap().items.len(),
            NPC_INTERIOR_ITEMS_MAX,
            "item rack caps FIFO"
        );
    }

    /// The 3-file split routes npc_interior to the NPC slice (round-trips
    /// through save_split + load_split via a temp dir).
    #[test]
    fn save_split_routes_npc_interior_to_npc_slice() {
        let dir = std::env::temp_dir().join(format!("wupi-split-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let world_p = dir.join("world.json");
        let player_p = dir.join("player.json");
        let npc_p = dir.join("npc.json");
        let mut s = WorldSchema::default();
        s.npc_interior.insert(
            "mara".into(),
            NpcInterior {
                mood: Some("suspicious".into()),
                ..NpcInterior::default()
            },
        );
        s.save_split(&world_p, &player_p, &npc_p).expect("save_split");
        let npc_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&npc_p).expect("npc file"))
                .expect("npc json");
        assert!(
            npc_json.get("npc_interior").is_some(),
            "interior rides the npc slice: {npc_json}"
        );
        let world_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&world_p).expect("world file"))
                .expect("world json");
        assert!(world_json.get("npc_interior").is_none(), "not in the world slice");
        let loaded = WorldSchema::load_split(&world_p, &player_p, &npc_p).expect("load_split");
        assert_eq!(
            loaded.npc_interior.get("mara").and_then(|i| i.mood.as_deref()),
            Some("suspicious"),
            "round-trips through the split"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Architectural invariant: presences is outside the LLM delta path
    /// (mirrors apply_delta_does_not_touch_travel_graph). A delta carrying
    /// "presences" must NOT mutate the typed Vec — the only writer is the
    /// `[PRESENCE]` applier (with grace decay).
    #[test]
    fn apply_delta_does_not_touch_presences() {
        let mut schema = WorldSchema::default();
        schema.presences = vec![Presence {
            npc_id: "mara".into(),
            name: "Mara".into(),
            stance: "behind the bar".into(),
            ttl: PRESENCE_GRACE_RESET,
        }];
        let original = schema.presences.clone();
        let mut ents = HashMap::new();
        ents.insert("presences".to_string(), Some(serde_json::Value::String("injected".into())));
        let delta = SchemaDelta {
            summary: None,
            recent_events: None,
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert_eq!(schema.presences, original, "presences must be LLM-immutable");
    }

    /// (2026-08-19 Hidden site maps) THE immunity pin: a delta (even one
    /// smuggling an entities key named "site_maps") NEVER touches the typed
    /// site-map store. Mirrors `apply_delta_does_not_touch_presences`.
    #[test]
    fn apply_delta_does_not_touch_site_maps() {
        let mut schema = WorldSchema::default();
        let mut map = crate::site_map::SiteMap::default();
        map.node_id = "warren".into();
        map.entrance = "gatehouse".into();
        map.areas = vec![
            crate::site_map::SiteArea { id: "gatehouse".into(), name: "Gatehouse".into(), knowledge: crate::site_map::AreaKnowledge::Visited, ..Default::default() },
            crate::site_map::SiteArea { id: "hall".into(), name: "Hall".into(), ..Default::default() },
            crate::site_map::SiteArea { id: "vault".into(), name: "Vault".into(), ..Default::default() },
        ];
        schema.site_maps.insert("warren".into(), map);
        let original = schema.site_maps.clone();
        let mut ents = HashMap::new();
        ents.insert(
            "site_maps".to_string(),
            Some(serde_json::Value::String("injected".into())),
        );
        let delta = SchemaDelta {
            entities: Some(ents),
            ..Default::default()
        };
        schema.apply_delta(delta);
        assert_eq!(schema.site_maps, original, "site_maps must be LLM-immutable");
    }

    /// (2026-08-19) merge_patch has no arm for site_maps — the unknown-field
    /// refusal is the immunity. Same for promises. (2026-08-20) Same for
    /// the economy's properties.
    #[test]
    fn merge_patch_refuses_site_maps_and_promises() {
        let mut schema = WorldSchema::default();
        let err = schema
            .merge_patch(serde_json::json!({ "site_maps": {} }))
            .expect_err("site_maps patch must be refused");
        assert!(err.contains("unknown top-level field"), "error should explain: {err}");
        let err = schema
            .merge_patch(serde_json::json!({ "promises": [] }))
            .expect_err("promises patch must be refused");
        assert!(err.contains("unknown top-level field"), "error should explain: {err}");
        let err = schema
            .merge_patch(serde_json::json!({ "properties": {} }))
            .expect_err("properties patch must be refused");
        assert!(err.contains("unknown top-level field"), "error should explain: {err}");
    }

    /// (2026-08-19) The 3-file split: site_maps ride world.json; promises
    /// (giver-keyed) ride npc.json. Round-trips through save/load_split.
    #[test]
    fn save_split_routes_site_maps_to_world_slice() {
        let dir = std::env::temp_dir().join(format!("wupi-site-split-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let world_p = dir.join("world.json");
        let player_p = dir.join("player.json");
        let npc_p = dir.join("npc.json");
        let mut s = WorldSchema::default();
        let mut map = crate::site_map::SiteMap::default();
        map.node_id = "warren".into();
        map.entrance = "gatehouse".into();
        map.areas = vec![
            crate::site_map::SiteArea { id: "gatehouse".into(), name: "Gatehouse".into(), knowledge: crate::site_map::AreaKnowledge::Visited, ..Default::default() },
            crate::site_map::SiteArea { id: "hall".into(), name: "Hall".into(), ..Default::default() },
            crate::site_map::SiteArea { id: "vault".into(), name: "Vault".into(), ..Default::default() },
        ];
        s.site_maps.insert("warren".into(), map);
        s.promises = vec![Promise {
            npc_id: "mara".into(),
            description: "return the horse".into(),
            deadline_minutes: 2_000,
            accepted_at_minutes: 1_000,
            ..Default::default()
        }];
        // (2026-08-22 living-world) Quests + the rested anchor ride the
        // WORLD slice by omission (player+world facing, not giver-keyed).
        s.quests = vec![Quest {
            id: "slay-warband".into(),
            giver: "mara".into(),
            title: "Slay the Warband".into(),
            ..Default::default()
        }];
        s.last_rest_minutes = 1_200;
        // (2026-08-20 Economy) Properties ride the WORLD slice by omission
        // (never removed into the player/npc partitions).
        s.properties.insert(
            "forge".into(),
            crate::economy::Property {
                node_id: "warren".into(),
                treasury_balance: 40,
                daily_revenue: 6,
                daily_upkeep: 2,
                ..Default::default()
            },
        );
        s.save_split(&world_p, &player_p, &npc_p).expect("save_split");
        let world_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&world_p).expect("world file"))
                .expect("world json");
        assert!(world_json.get("site_maps").is_some(), "site maps ride the world slice");
        assert!(world_json.get("promises").is_none(), "promises never ride the world slice");
        assert!(world_json.get("properties").is_some(), "properties ride the world slice");
        assert!(world_json.get("quests").is_some(), "quests ride the world slice");
        assert!(
            world_json.get("last_rest_minutes").is_some(),
            "the rested anchor rides the world slice"
        );
        let player_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&player_p).expect("player file"))
                .expect("player json");
        assert!(player_json.get("properties").is_none(), "properties never ride the player slice");
        let npc_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&npc_p).expect("npc file"))
                .expect("npc json");
        assert!(npc_json.get("promises").is_some(), "promises ride the npc slice");
        assert!(npc_json.get("properties").is_none(), "properties never ride the npc slice");
        let loaded = WorldSchema::load_split(&world_p, &player_p, &npc_p).expect("load_split");
        assert!(loaded.site_maps.contains_key("warren"), "site maps round-trip");
        assert_eq!(loaded.promises.len(), 1, "promises round-trip");
        assert_eq!(loaded.quests.len(), 1, "quests round-trip");
        assert_eq!(loaded.last_rest_minutes, 1_200, "rested anchor round-trips");
        assert_eq!(
            loaded.properties.get("forge").map(|p| p.treasury_balance),
            Some(40),
            "properties round-trip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (2026-08-22 living-world) The `quests:` + `rested:` render lines:
    /// dormant when empty (zero-token invariant), bounded + banded when
    /// live, and the overdue band surfaces only past the deadline.
    #[test]
    fn render_carries_quests_and_rested_lines() {
        let mut s = WorldSchema::default();
        // Dormant: no quests, no anchor → neither line renders.
        let rendered = s.render_for_prompt();
        assert!(!rendered.contains("quests:"), "quests dormant when empty");
        assert!(!rendered.contains("rested:"), "rested dormant without an anchor");

        // Clock + anchor set: the rested line renders with hours; a healthy
        // delta carries no band, a weary one does.
        s.world_clock.current_minutes = 30 * 60; // Day 2 06:00
        s.last_rest_minutes = 16 * 60; // 14h since rest → no band
        let rendered = s.render_for_prompt();
        assert!(rendered.contains("rested: 14h since last rest\n"), "{rendered}");
        assert!(!rendered.contains("weary"));
        s.last_rest_minutes = 5 * 60; // 25h since rest → exhausted band
        let rendered = s.render_for_prompt();
        assert!(
            rendered.contains("rested: 25h since last rest — exhausted\n"),
            "{rendered}"
        );
        // 20h delta → weary.
        s.last_rest_minutes = 10 * 60;
        let rendered = s.render_for_prompt();
        assert!(rendered.contains("rested: 20h since last rest — weary\n"), "{rendered}");

        // Quests: counter + done-flag objectives, the giver's registry name,
        // the overdue band, the render cap + (+N more).
        s.quests = (0..MAX_QUESTS + 2)
            .map(|i| Quest {
                id: format!("quest-{i}"),
                giver: if i == 0 { "player".into() } else { "mara".into() },
                title: format!("Thread {i} of the conspiracy"),
                objectives: vec![
                    QuestObjective { text: "cull the wolves".into(), cur: 3, total: 6, ..Default::default() },
                    QuestObjective { text: "burn the shrine".into(), done: true, ..Default::default() },
                ],
                reward: "30 silver".into(),
                deadline_minutes: if i == 1 { 20 * 60 } else { 0 },
                accepted_at_minutes: 10 * 60,
                ..Default::default()
            })
            .collect();
        s.npc_registry = NpcRegistry {
            entries: vec![NpcEntry {
                id: "mara".into(),
                name: "Mara".into(),
                ..Default::default()
            }],
        };
        let rendered = s.render_for_prompt();
        assert!(rendered.contains("quests: "), "{rendered}");
        assert!(rendered.contains("3/6 cull the wolves"), "{rendered}");
        assert!(rendered.contains("✓ burn the shrine"), "{rendered}");
        assert!(rendered.contains("reward: 30 silver"), "{rendered}");
        assert!(rendered.contains("(self)"), "player-giver renders as self");
        assert!(rendered.contains("(Mara, "), "overdue quest carries the giver band");
        assert!(rendered.contains("(+5 more)"), "render cap 5 + marker");
        // Cap discipline (hand-edited backstop).
        let mut capped = s.clone();
        capped.enforce_typed_caps();
        assert_eq!(capped.quests.len(), MAX_QUESTS);
        assert_eq!(capped.quests[0].id, "quest-2", "FIFO drops the oldest");
    }

    /// (2026-08-22 living-world) The quest deadline curve: the promise
    /// frustration math applied to quests, with the player-giver + no-
    /// deadline exemptions.
    #[test]
    fn quest_deadline_frustration_exemptions_and_curve() {
        let player_quest = Quest {
            giver: "player".into(),
            deadline_minutes: 100,
            accepted_at_minutes: 0,
            ..Default::default()
        };
        assert_eq!(
            quest_deadline_frustration(&player_quest, None, 500),
            f64::NEG_INFINITY,
            "self-imposed goals are never penalized"
        );
        let no_deadline = Quest { giver: "mara".into(), ..Default::default() };
        assert_eq!(quest_deadline_frustration(&no_deadline, None, 500), f64::NEG_INFINITY);
        // 50% overrun at coeff 1.0 → +0.5 (the pinned promise vector).
        let overdue = Quest {
            giver: "mara".into(),
            deadline_minutes: 100,
            accepted_at_minutes: 0,
            ..Default::default()
        };
        let score = quest_deadline_frustration(&overdue, None, 150);
        assert!((score - 0.5).abs() < 1e-9, "{score}");
        // A volatile giver (3.0) at the same overrun → +1.5 (auto-fails at
        // the ≥1.0 threshold; a patient one (0.4) does not yet).
        assert!(quest_deadline_frustration(&overdue, Some(3.0), 150) >= 1.0);
        assert!(quest_deadline_frustration(&overdue, Some(0.4), 150) < 1.0);
    }

    /// (2026-08-22 living-world) The rest curves: recovery steps by rest
    /// length + the fatigue band boundaries.
    #[test]
    fn rest_curves_pin_boundaries() {
        assert_eq!(rest_recovery_steps(0), 0, "a breather recovers nothing");
        assert_eq!(rest_recovery_steps(1), 1);
        assert_eq!(rest_recovery_steps(3), 1);
        assert_eq!(rest_recovery_steps(4), 2);
        assert_eq!(rest_recovery_steps(7), 2);
        assert_eq!(rest_recovery_steps(8), 4, "a full night recovers everything");
        assert_eq!(rest_recovery_steps(12), 4);
        assert_eq!(rested_band(16 * 60), None);
        assert_eq!(rested_band(16 * 60 + 1), Some("weary"));
        assert_eq!(rested_band(24 * 60), Some("weary"));
        assert_eq!(rested_band(24 * 60 + 1), Some("exhausted"));
        // The objective upsert key: spacing/case insensitive.
        assert_eq!(
            normalize_quest_objective_key("  Cull   the Wolves "),
            normalize_quest_objective_key("cull the wolves")
        );
    }

    /// (2026-08-19) The three new render lines: `site:` only for the CURRENT
    /// node's map (knowledge-filtered), `owed:` for present givers with the
    /// frustration band, `bonds:` for present loud-tier NPCs only.
    #[test]
    fn render_carries_site_owed_bonds_lines() {
        let mut s = WorldSchema::default();
        s.world_clock.current_minutes = 1_500;
        s.presences = vec![
            Presence { npc_id: "mara".into(), name: "Mara".into(), stance: String::new(), ttl: PRESENCE_GRACE_RESET },
            Presence { npc_id: "harsk".into(), name: "Harsk".into(), stance: String::new(), ttl: PRESENCE_GRACE_RESET },
            Presence { npc_id: "randopasserby".into(), name: "Passerby".into(), stance: String::new(), ttl: PRESENCE_GRACE_RESET },
        ];
        s.relationships.insert(
            "mara".into(),
            RelationshipState { tier: RelationshipTier::Friendly, ..Default::default() },
        );
        s.relationships.insert(
            "harsk".into(),
            RelationshipState { tier: RelationshipTier::Nemesis, ..Default::default() },
        );
        s.relationships.insert(
            "randopasserby".into(),
            RelationshipState { tier: RelationshipTier::Acquaintance, ..Default::default() },
        );
        s.promises = vec![Promise {
            npc_id: "mara".into(),
            description: "return the horse".into(),
            // Halfway at coeff 1.0 → −0.5 → "Very Pleased"
            deadline_minutes: 2_000,
            accepted_at_minutes: 1_000,
            ..Default::default()
        }];
        let mut map = crate::site_map::SiteMap::default();
        map.node_id = "warren".into();
        map.threat = crate::site_map::SiteThreat::High;
        map.entrance = "gatehouse".into();
        map.areas = vec![
            crate::site_map::SiteArea {
                id: "gatehouse".into(),
                name: "Gatehouse".into(),
                knowledge: crate::site_map::AreaKnowledge::Visited,
                geometry: vec!["cold draft".into()],
                connections: vec![crate::site_map::SiteConnection {
                    to: "hall".into(),
                    state: crate::site_map::ConnState::Open,
                    detail: String::new(),
                }],
            },
            crate::site_map::SiteArea { id: "hall".into(), name: "Great Hall".into(), ..Default::default() },
            crate::site_map::SiteArea { id: "vault".into(), name: "Vault".into(), ..Default::default() },
        ];
        s.site_maps.insert("warren".into(), map);
        s.travel_graph.current_node = Some("warren".into());

        let rendered = s.render_for_prompt_with_beneath(false);
        assert!(rendered.contains("site:\n"), "site block renders for the current node: {rendered}");
        assert!(rendered.contains("threat: high"), "site block carries the threat line");
        // Unrevealed truth stays hidden.
        assert!(!rendered.contains("Great Hall"), "unrevealed area must not render");
        assert!(
            rendered.contains("owed: Mara — \"return the horse\" — Very Pleased"),
            "owed line with the frustration band: {rendered}"
        );
        assert!(
            rendered.contains("bonds: Mara [friendly]") && rendered.contains("Harsk [nemesis]"),
            "bonds line carries the loud tiers: {rendered}"
        );
        assert!(
            !rendered.contains("Passerby [acquaintance]"),
            "the quiet middle stays silent: {rendered}"
        );

        // A DIFFERENT current node → no site block (dormant).
        s.travel_graph.current_node = Some("town".into());
        let rendered = s.render_for_prompt_with_beneath(false);
        assert!(!rendered.contains("site:\n"), "site block is current-node-scoped");
        // owed/bonds are presence-scoped, not node-scoped — still render.
        assert!(rendered.contains("owed:"));
    }

    /// (2026-08-19) enforce_typed_caps: promise FIFO cap.
    #[test]
    fn enforce_typed_caps_drops_oldest_promises() {
        let mut s = WorldSchema::default();
        for i in 0..(MAX_PROMISES + 3) {
            s.promises.push(Promise {
                npc_id: format!("npc{i}"),
                description: format!("obligation {i}"),
                deadline_minutes: 100 + i as i64,
                accepted_at_minutes: 0,
                ..Default::default()
            });
        }
        s.enforce_typed_caps();
        assert_eq!(s.promises.len(), MAX_PROMISES, "capped at MAX_PROMISES");
        assert_eq!(s.promises[0].description, "obligation 3", "FIFO — oldest dropped");
    }

    /// (2026-08-20 Economy; audit rework same day) enforce_typed_caps:
    /// property cap drops FIRST-INSERTED entries (true FIFO via
    /// `property_order`), NOT alphabetically-first BTreeMap keys. The ids
    /// are inserted in REVERSE alphabetical order so the two orderings
    /// disagree — the old key-order trim passed only because the original
    /// test's ids happened to be alphabetical.
    #[test]
    fn enforce_typed_caps_drops_oldest_properties() {
        let mut s = WorldSchema::default();
        let n = crate::economy::MAX_PROPERTIES + 3;
        for i in 0..n {
            // "z00" inserted FIRST … "a00" inserted LAST: key order is the
            // exact reverse of insertion order.
            let id = format!("{:c}{:02}", (b'z' - (i as u8)), i);
            s.properties.insert(
                id.clone(),
                crate::economy::Property { node_id: "town".into(), ..Default::default() },
            );
            s.property_order.push_back(id);
        }
        s.enforce_typed_caps();
        assert_eq!(s.properties.len(), crate::economy::MAX_PROPERTIES, "capped");
        assert!(!s.properties.contains_key("z00"), "FIRST-INSERTED dropped");
        assert!(!s.properties.contains_key("y01"), "second-inserted dropped");
        assert!(!s.properties.contains_key("x02"), "third-inserted dropped");
        // "p10" is the LAST inserted AND the alphabetically-first key — the
        // old key-order trim dropped exactly this one.
        assert!(s.properties.contains_key("p10"), "last-inserted survives");
        // The order vec carries no dead ids after the trim.
        assert_eq!(s.property_order.len(), s.properties.len());
        for id in &s.property_order {
            assert!(s.properties.contains_key(id), "no dead ids in property_order");
        }
    }

    /// (2026-08-20 audit) A legacy save (empty `property_order`) backfills
    /// deterministically in BTreeMap key order, and dead order ids prune.
    #[test]
    fn property_order_backfills_and_prunes() {
        let mut s = WorldSchema::default();
        for id in ["b", "a", "c"] {
            s.properties
                .insert(id.into(), crate::economy::Property::default());
        }
        s.property_order.push_back("ghost".into());
        s.reconcile_property_order();
        assert_eq!(
            s.property_order.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["b", "a", "c"],
            "dead id pruned, unseen ids appended in key order"
        );
    }

    /// (2026-08-20 Economy) The `ledger:` render — dormant when no properties
    /// (the fresh-game zero-token contract), live + capped when they exist,
    /// and the location-line prosperity marker.
    #[test]
    fn render_carries_ledger_line_and_prosperity_marker() {
        // Dormant: a fresh schema renders nothing (empty check includes
        // properties).
        assert_eq!(WorldSchema::default().render_for_prompt(), "");
        let mut s = WorldSchema::default();
        s.world_clock.current_minutes = 1_500;
        s.travel_graph.nodes.push(Node {
            id: "town".into(),
            name: "Town".into(),
            prosperity: 40,
            ..Default::default()
        });
        s.travel_graph.current_node = Some("town".into());
        s.properties.insert(
            "forge".into(),
            crate::economy::Property {
                node_id: "town".into(),
                daily_revenue: 6,
                daily_upkeep: 2,
                treasury_balance: 40,
                owner: crate::economy::Owner::Npc("liam".into()),
                ..Default::default()
            },
        );
        let rendered = s.render_for_prompt();
        assert!(rendered.contains("ledger: forge@town +0/day till 40 (owner liam)"),
            "ledger line renders (revenue scales at 40%% prosperity: floor(6×40/100)−2 = 0): {rendered}");
        assert!(rendered.contains("location: "), "location line renders");
        assert!(rendered.contains("— hard times"), "≤50 prosperity marker: {rendered}");
        // Boom marker at ≥150; no marker mid-band.
        s.travel_graph.nodes[0].prosperity = 175;
        let rendered = s.render_for_prompt();
        assert!(rendered.contains("— booming"), "≥150 prosperity marker");
        s.travel_graph.nodes[0].prosperity = 100;
        let rendered = s.render_for_prompt();
        assert!(!rendered.contains("— hard times") && !rendered.contains("— booming"),
            "mid-band carries no marker");
    }

    /// (2026-08-20 Economy) load_split clamps hand-edited prosperity into
    /// the legal band; missing prosperity deserializes at the 100 default.
    #[test]
    fn load_split_clamps_and_defaults_prosperity() {
        let dir = std::env::temp_dir().join(format!("wupi-prosperity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let world_p = dir.join("world.json");
        std::fs::write(
            &world_p,
            serde_json::json!({
                "travel_graph": {
                    "nodes": [
                        { "id": "low", "name": "Low", "prosperity": 0 },
                        { "id": "high", "name": "High", "prosperity": 255 },
                        { "id": "legacy", "name": "Legacy" }
                    ],
                    "current_node": "low"
                }
            })
            .to_string(),
        )
        .expect("write world");
        let loaded = WorldSchema::load_split(&world_p, &dir.join("player.json"), &dir.join("npc.json"))
            .expect("load_split");
        let get = |id: &str| {
            loaded
                .travel_graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .prosperity
        };
        assert_eq!(get("low"), crate::economy::PROSPERITY_MIN, "0 clamps up to 25");
        assert_eq!(get("high"), crate::economy::PROSPERITY_MAX, "255 clamps down to 200");
        assert_eq!(get("legacy"), crate::economy::PROSPERITY_DEFAULT, "missing defaults to 100");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (2026-08-21 economy addendum) The currency label persists through the
    /// split (world slice), defaults to "" for pre-addendum saves, and the
    /// `<economy_anchor>` price ladder renders inside `<world_state>`.
    #[test]
    fn currency_label_persists_and_anchor_renders() {
        let dir = std::env::temp_dir().join(format!("wupi-currency-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut s = WorldSchema::default();
        assert_eq!(s.currency_label, "", "fresh world: no currency assumed");
        s.currency_label = "gold/silver/copper".into();
        s.save_split(
            &dir.join("world.json"),
            &dir.join("player.json"),
            &dir.join("npc.json"),
        )
        .expect("save_split");
        let loaded = WorldSchema::load_split(
            &dir.join("world.json"),
            &dir.join("player.json"),
            &dir.join("npc.json"),
        )
        .expect("load_split");
        assert_eq!(loaded.currency_label, "gold/silver/copper");
        // A world file WITHOUT the key (every pre-addendum save) loads "".
        let bare = dir.join("bare");
        std::fs::create_dir_all(&bare).expect("temp dir");
        std::fs::write(
            &bare.join("world.json"),
            serde_json::json!({}).to_string(),
        )
        .expect("write world");
        let legacy = WorldSchema::load_split(
            &bare.join("world.json"),
            &bare.join("player.json"),
            &bare.join("npc.json"),
        )
        .expect("load_split");
        assert_eq!(legacy.currency_label, "", "legacy save stays unit-free");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
        // The anchor rides the world_state render (both tracker skeleton +
        // narrator tail consume it).
        let rendered = loaded.render_for_prompt();
        assert!(rendered.contains("<economy_anchor>"), "{rendered}");
        assert!(rendered.contains("base units of gold/silver/copper"), "{rendered}");
    }

    /// NpcRegistry dormant contract: empty registry is_set()==false, renders
    /// no line (zero tokens for a fresh game).
    #[test]
    fn npc_registry_dormant_when_empty() {
        let reg = NpcRegistry::default();
        assert!(!reg.is_set());
        assert!(reg.render_line().is_none());
        assert!(reg.resolve("anyone").is_none(), "empty registry resolves nothing");
    }

    /// NpcRegistry.resolve is the load-bearing normalization fn — id OR alias
    /// matches, case-insensitive. Unknown forms return None (the reject gate).
    #[test]
    fn npc_registry_resolve_matches_id_or_alias_case_insensitive() {
        let reg = NpcRegistry {
            entries: vec![NpcEntry {
                prominence: NpcProminence::Named,
                id: "mara_the_innkeep".into(),
                name: "Mara".into(),
                role: String::new(),
                tier: None,
                aliases: vec!["mara".into(), "Innkeep".into()],
            }],
        };
        assert_eq!(reg.resolve("mara_the_innkeep").map(|e| e.id.as_str()), Some("mara_the_innkeep"));
        assert_eq!(reg.resolve("MARA").map(|e| e.id.as_str()), Some("mara_the_innkeep"), "alias + case-insensitive");
        assert_eq!(reg.resolve("innkeep").map(|e| e.id.as_str()), Some("mara_the_innkeep"), "alias lowercased vs stored mixed-case");
        assert!(reg.resolve("stranger").is_none(), "unknown form → None (reject gate)");
    }

    /// NpcRegistry.render_line emits `Name [id]` per entry, joined by commas.
    /// Id-only (no name) renders as `[id]`.
    #[test]
    fn npc_registry_render_line_format() {
        let reg = NpcRegistry {
            entries: vec![
                NpcEntry {
                    prominence: NpcProminence::Named,
                    id: "mara_the_innkeep".into(),
                    name: "Mara".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                },
                NpcEntry {
                    prominence: NpcProminence::Named,
                    id: "anon".into(),
                    name: String::new(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                },
            ],
        };
        assert_eq!(reg.render_line().as_deref(), Some("Mara [mara_the_innkeep], [anon]"));
    }

    // --- upsert_entry (dynamic world-seeding, [NPC_REGISTER] applier) ---

    #[test]
    fn upsert_entry_inserts_new_npc() {
        let mut reg = NpcRegistry::default();
        let inserted = reg.upsert_entry(NpcEntry {
            prominence: NpcProminence::Named,
            id: "coby".into(),
            name: "Coby".into(),
            role: "timid Marine recruit".into(),
            tier: Some("soldier".into()),
            aliases: vec!["coby".into()],
        });
        assert!(inserted);
        assert_eq!(reg.entries.len(), 1);
        assert!(reg.resolve("coby").is_some(), "id is its own alias → resolves");
    }

    #[test]
    fn upsert_entry_is_idempotent_on_duplicate_id() {
        // Re-registering an existing id is a no-op (first writer wins).
        let mut reg = NpcRegistry::default();
        reg.upsert_entry(NpcEntry { id: "coby".into(), name: "Coby".into(), role: String::new(), tier: None, aliases: vec![], prominence: NpcProminence::Named });
        let inserted = reg.upsert_entry(NpcEntry { id: "coby".into(), name: "DIFFERENT".into(), role: String::new(), tier: None, aliases: vec!["newalias".into()], prominence: NpcProminence::Named });
        assert!(!inserted, "duplicate id returns false");
        assert_eq!(reg.entries.len(), 1);
        // Original entry preserved; new alias NOT merged (stable registry).
        assert_eq!(reg.find("coby").unwrap().name, "Coby");
        assert!(!reg.resolve("newalias").is_some(), "re-registration does not merge new aliases");
    }

    #[test]
    fn upsert_entry_empty_id_returns_false() {
        let mut reg = NpcRegistry::default();
        let inserted = reg.upsert_entry(NpcEntry { id: String::new(), name: "X".into(), role: String::new(), tier: None, aliases: vec![], prominence: NpcProminence::Named });
        assert!(!inserted);
        assert!(reg.entries.is_empty());
    }

    /// A pre-Phase-5 save JSON (no "npc_registry"/"presences" fields) must
    /// deserialize to empty defaults (backward-compat — mirrors the pre-
    /// Component-3 travel_graph test).
    #[test]
    fn phase5a_backwards_compat_pre_phase5_save_loads_empty() {
        let pre_phase5_json = r#"{
            "summary": "",
            "recent_events": [],
            "entities": {},
            "player_state": {},
            "world_clock": {"current_minutes": 0, "last_tick_minutes": 0},
            "weather": {"condition": "", "started_at_minutes": 0},
            "travel_graph": {"nodes": [], "current_node": null},
            "immutable_keys": [],
            "scene_pacing": {"mode": "Exploration", "spatial": 0, "emotional": 0, "kinetic": 0},
            "status_tags": [],
            "relationships": {},
            "offscreen_tasks": [],
            "rumors": []
        }"#;
        let parsed: WorldSchema = serde_json::from_str(pre_phase5_json)
            .expect("pre-Phase-5 JSON must deserialize");
        assert!(!parsed.npc_registry.is_set());
        assert!(parsed.npc_registry.entries.is_empty());
        assert!(parsed.presences.is_empty());
    }

    /// The `present:` render line emits only when presences is non-empty, in
    /// `Name (stance)` form (or bare `Name` when stance is empty). Dormant
    /// (zero tokens) when empty — a fresh game suppresses the line entirely.
    #[test]
    fn present_line_renders_on_camera_whitelist() {
        let mut schema = WorldSchema::default();
        // Empty → no present: line.
        let rendered = schema.render_for_prompt();
        assert!(!rendered.contains("present:"), "empty presences must not render");

        // Set one presence (force clock set so render_for_prompt emits at all).
        schema.world_clock = WorldClock { current_minutes: 60, last_tick_minutes: 0 };
        schema.presences = vec![
            Presence {
                npc_id: "mara".into(),
                name: "Mara".into(),
                stance: "behind the bar, polishing a tankard".into(),
                ttl: PRESENCE_GRACE_RESET,
            },
            Presence {
                npc_id: "corin".into(),
                name: "Corin".into(),
                stance: String::new(),
                ttl: PRESENCE_GRACE_RESET,
            },
        ];
        let rendered = schema.render_for_prompt();
        assert!(rendered.contains("present: Mara (behind the bar, polishing a tankard), Corin"),
            "present line must list on-camera NPCs with stance; bare name when stance empty.\n---\n{rendered}");
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

    /// (2026-08-27 playtest H2) The setting classifier — the architect gate
    /// (indoor|settlement|seeds|pressure) must see a setting on minted +
    /// seeded nodes.
    #[test]
    fn infer_node_setting_classifies_playtest_places() {
        assert_eq!(infer_node_setting("Ashfall Reach"), "");
        assert_eq!(infer_node_setting("Ashfall Reach Harbor"), "settlement");
        assert_eq!(infer_node_setting("the Rusty Anchor Tavern"), "indoor");
        assert_eq!(infer_node_setting("Sable's Lighthouse"), "indoor");
        assert_eq!(infer_node_setting("Greymist"), "");
        assert_eq!(infer_node_setting("Greymist Village"), "settlement");
        // Word-boundary: "customs" never trips "docks"-class substrings.
        assert_eq!(infer_node_setting("Customs Ledger"), "");
    }

    /// (2026-08-27 playtest H3) Mint-name hygiene: relational tails strip,
    /// generic way-words reject, distinctive names pass.
    #[test]
    fn mint_name_hygiene_trims_and_gates() {
        assert_eq!(trim_trailing_relational_words("greymist through"), "greymist");
        assert_eq!(
            trim_trailing_relational_words("Warehouse on the"),
            "Warehouse"
        );
        assert_eq!(trim_trailing_relational_words("Old Gate Road"), "Old Gate Road");
        assert!(is_generic_place_name("path"));
        assert!(is_generic_place_name("The old lane"));
        assert!(!is_generic_place_name("North Road"));
        assert!(!is_generic_place_name("Iron Alley"));
        assert!(!is_generic_place_name("Greymist"));
    }

    // ─────────────────────────────────────────────────────────────────────
    // (2026-08-29 modules E1+E2+F2) The one-shot save heal + the E2 lexicon.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn infer_node_setting_extends_to_cafe_scale_places() {
        // (E2) The friend-log worlds' named places — none matched before.
        assert_eq!(infer_node_setting("The Corner Café"), "indoor");
        assert_eq!(infer_node_setting("Riverside Diner"), "indoor");
        assert_eq!(infer_node_setting("cramped apartment"), "indoor");
        assert_eq!(infer_node_setting("the guard barracks"), "indoor");
        assert_eq!(infer_node_setting("east corridor"), "indoor");
        assert_eq!(infer_node_setting("subway lab"), "indoor");
        // Settlement words keep priority ("Harbor Diner" is a building in a
        // port — the node IS the settlement's docks).
        assert_eq!(infer_node_setting("Harbor Diner"), "settlement");
        // A whole-level node stays unset (outdoor/unknown).
        assert_eq!(infer_node_setting("the-backrooms"), "");
    }

    /// (F2) A synthetic PRE-FIX save heals in place: settings backfill,
    /// garbage sweep, version stamp — and the second run is a no-op.
    #[test]
    fn heal_schema_state_fixes_prefab_save_idempotently() {
        let mut s = WorldSchema::default();
        assert_eq!(s.heal_version, 0, "a pre-heal save deserializes at v0");
        // Nodes: one inferable indoor, one settlement, one that stays empty.
        for (id, name) in [
            ("diner", "Riverside Diner"),
            ("market", "Harbor Market"),
            ("field", "Open Field"),
        ] {
            s.travel_graph.nodes.push(Node {
                id: id.into(),
                name: name.into(),
                setting: String::new(), // the pre-H2 mint shape
                ..Default::default()
            });
        }
        // Player pack: two garbage names + one legit stack.
        for (name, qty) in [
            ("tobacco smell came", 2), // B4b verb tail
            ("Adopolous +Pipe", 1),    // B4a embedded ` +`
            ("Field Points", 12),      // legit loot — untouched
        ] {
            s.player_state.pack.push(crate::equipment::StackItem {
                name: name.into(),
                qty,
                ..Default::default()
            });
        }
        // NPC worn rack: one verb-tail garbage garment.
        s.npc_interior.entry("abba".into()).or_default().worn.push(
            crate::equipment::StackItem {
                name: "pipe turned".into(),
                ..Default::default()
            },
        );

        WorldSchema::heal_schema_state(&mut s);

        // E1 backfill.
        let by_id = |id: &str| {
            s.travel_graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .unwrap()
                .clone()
        };
        assert_eq!(by_id("diner").setting, "indoor");
        assert_eq!(by_id("market").setting, "settlement");
        assert_eq!(by_id("field").setting, "", "no signal stays empty");
        // Sweep.
        let names: Vec<&str> = s.player_state.pack.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["Field Points"], "garbage swept, legit kept");
        assert!(
            s.npc_interior["abba"].worn.is_empty(),
            "NPC racks swept too"
        );
        // Version stamped.
        assert_eq!(s.heal_version, WorldSchema::HEAL_VERSION);

        // Idempotent: a second run changes nothing (also proves the guard).
        // WorldSchema carries no PartialEq — compare through the JSON
        // projection (the same projection saves travel through).
        let mut again = s.clone();
        WorldSchema::heal_schema_state(&mut again);
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&s).unwrap(),
            "second heal is a no-op"
        );

        // Already-healed schemas skip entirely (the single-comparison gate).
        let before = serde_json::to_value(&again).unwrap();
        WorldSchema::heal_schema_state(&mut again);
        assert_eq!(serde_json::to_value(&again).unwrap(), before);
    }
}
