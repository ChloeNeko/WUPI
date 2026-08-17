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

    /// Render the time-of-day ONLY ("14:00"), suppressing the "Day N" prefix.
    /// Used when a rich calendar label (`WorldSchema.calendar`) is set: the
    /// day/date is carried by the `date:` line, so the `clock:` line shows just
    /// the time-of-day to avoid a redundant day counter. Returns `None` when
    /// the clock is unset.
    pub fn render_time_of_day(&self) -> Option<String> {
        if !self.is_set() {
            return None;
        }
        let rem = self.current_minutes % 1440;
        let h24 = rem / 60;
        let m = rem % 60;
        Some(format!("{h24:02}:{m:02}"))
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
fn slugify(s: &str) -> String {
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
        // carry-back caps).
        const EXITS_RENDER_CAP: usize = 8;
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
        let slug = slugify(raw_trimmed);
        if slug.is_empty() {
            return None;
        }
        // Typo guard: best similarity ≥0.75 across (raw|slug) × (id|name).
        let mut best: Option<(f32, String)> = None;
        for n in &self.nodes {
            let slug_name = slugify(&n.name);
            for (a, b) in [
                (raw_trimmed, n.id.as_str()),
                (raw_trimmed, n.name.as_str()),
                (slug.as_str(), n.id.as_str()),
                (slug.as_str(), slug_name.as_str()),
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
        if let Some((score, id)) = best.filter(|(s, _)| *s >= 0.75) {
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
        let name_src = phrase.unwrap_or_else(|| raw_trimmed.to_string());
        let id = slugify(&name_src);
        if id.is_empty() {
            return None;
        }
        let name = if name_src.chars().count() > 80 {
            name_src.chars().take(80).collect()
        } else {
            name_src
        };
        let node = Node {
            id: id.clone(),
            name,
            neighbors: Vec::new(),
            setting: String::new(),
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

/// Normalized Levenshtein similarity in [0,1] (1 = identical). Chars-based
/// (anti-pattern #6: byte-index math on multi-byte input panics); the
/// classic O(m·n) DP is fine for short location labels on a small graph.
fn similarity(a: &str, b: &str) -> f32 {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
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
    let dist = prev[b.len()] as f32;
    1.0 - dist / a.len().max(b.len()) as f32
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
    let clean = |t: &str| t.trim_matches(|c: char| !c.is_alphanumeric());
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
        tracing::info!(
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
    /// (#48) HARD-CAPPED at the first 16 entries + a `(+N more)` marker:
    /// `npc_registry` grows via `[NPC_REGISTER]` with no ceiling, and this
    /// line rides EVERY tracker + narrator prompt — uncapped it re-grew the
    /// overflow the bounded carry-back was built to prevent.
    pub fn render_line(&self) -> Option<String> {
        const CAST_PROMPT_CAP: usize = 16;
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

/// (2026-08-16 audit LOW) Shared growth caps for the typed referee-owned
/// collections — the bracket appliers (lib.rs) + `merge_patch`'s
/// full-replace defense (`enforce_typed_caps`) agree on one set of numbers.
pub const MAX_TRACKED_RELATIONSHIPS: usize = 48;
pub const MAX_STORED_TASKS: usize = 20;
pub const MAX_STORED_RUMORS: usize = 20;
pub const MAX_TRAVEL_NODES: usize = 96;
/// (2026-08-16 yellow W3) The dynamic-cast registry cap — module scope so
/// `NpcRegistry::upsert_entry` AND `merge_patch`'s full-replace arm share one
/// number (the raw-editor JSON tab installs whole registries; the applier's
/// refuse-at-cap discipline now backstops it).
pub const MAX_NPC_REGISTRY: usize = 96;

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

    /// Free-form calendar label (2026-08-13): a verbatim date string
    /// ("3rd of Harvest, Year 1247, Market Day") — month/year/type-of-day, NOT
    /// just "Day N". Seeded from the card's `<start><date>` + advanced in play
    /// by the `[DATE]` bracket (the tracker rewrites the label — no Rust
    /// calendar arithmetic). When set, `render_for_prompt` emits `date:` +
    /// renders the clock as time-of-day only (suppressing "Day N"). When unset
    /// (None — legacy cards / pre-2026-08-13 saves), the legacy
    /// `clock: Day N, HH:MM` render is preserved. `#[serde(default)]` keeps old
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
                // refused here, not just swept at load: the load-time
                // `migrate_legacy_items` converts any `item_*`/`inv_*`
                // entity into a typed pack item, so a model delta re-creating
                // one alongside a real [PACK] bracket silently duplicated the
                // item on the next boot. Strip + warn (same discipline as the
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
                        self.entities.remove(&key);
                    }
                }
            }
            if grew {
                self.enforce_entity_cap();
            }
        }
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
                        // apply_delta — the typed inventory owns items; a
                        // model patch re-creating item_*/inv_* keys would be
                        // converted into duplicate pack items by the next
                        // boot's migrate_legacy_items sweep.
                        if ek.starts_with("item_") || ek.starts_with("inv_") {
                            tracing::warn!(%ek, "merge_patch: legacy inventory entity key refused (typed inventory owns items)");
                            continue;
                        }
                        if ev.is_null() {
                            self.entities.remove(ek);
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
        let empty = self.summary.trim().is_empty()
            && self.recent_events.is_empty()
            && self.player_state.is_default()
            && !self.world_clock.is_set()
            && !self.weather.is_set()
            && !self.travel_graph.is_set()
            && self.rumors.is_empty()
            && !self.npc_registry.is_set()
            && self.presences.is_empty()
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
            out.push_str(cal);
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
            // (#48) HARD-CAPPED at the first 12 + a `(+N more)` marker —
            // presence is bounded by the [PRESENCE]-per-turn rebuild + the
            // 4-turn age-out in practice, but a burst turn (a crowded tavern)
            // must not blow the always-on prompt line.
            const PRESENCE_PROMPT_CAP: usize = 12;
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
            out.push_str(&parts.join(", "));
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
            // (#48) HARD-CAPPED at the first 6 heard rumors + a `(+N more)`
            // marker — rumor texts are full prose phrases (heavy), the list
            // grows monotonically via propagation, and this line rides every
            // prompt.
            const RUMORS_PROMPT_CAP: usize = 6;
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
                out.push_str(&line);
                out.push('\n');
            }
        }
        if !self.summary.trim().is_empty() {
            out.push_str("summary: ");
            // (yellow S7) flattened — see flatten_inline.
            out.push_str(&Self::flatten_inline(self.summary.trim()));
            out.push('\n');
        }
        // Cap recent events shown in chat at the last 5: older events live
        // in the persisted schema + memory retrieval, not the chat prompt.
        let show_events = self.recent_events.len().saturating_sub(5);
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
        // This block is the LEAN carry-back: only the three pieces the tracker
        // cannot infer from the 1-turn window + Rust anchors, each BOUNDED so
        // it cannot re-grow the overflow:
        //   - cast: the roster line (id list, no prose) — the [PRESENCE]
        //     whitelist source. Empty when no NPCs are registered.
        //   - belt: the 4-slot quick rack, names only (the [BELT] state).
        //     Empty when nothing's on the belt.
        //   - pack: the unbounded deep store, names + qty ONLY, HARD-CAPPED at
        //     the first 12 entries. Pack can grow large in long sessions; the
        //     cap keeps the line bounded. (Older entries live in the persisted
        //     schema + the inventory panel UI — not the prompt.)
        // Item tags/stats are deliberately NOT rendered here (they're authoring
        // noise for the tracker; the apply path keeps them on the items). The
        // narrator sees these too — it's legitimate observer knowledge (what
        // you carry + who's in the cast).
        if let Some(cast_line) = self.npc_registry.render_line() {
            out.push_str("cast: ");
            out.push_str(&cast_line);
            out.push('\n');
        }
        if !self.player_state.belt.is_empty() {
            let names: Vec<String> = self
                .player_state
                .belt
                .iter()
                .map(|i| {
                    if i.qty > 1 {
                        format!("{} ×{}", i.name, i.qty)
                    } else {
                        i.name.clone()
                    }
                })
                .collect();
            out.push_str("belt: ");
            out.push_str(&names.join(", "));
            out.push('\n');
        }
        if !self.player_state.pack.is_empty() {
            const PACK_PROMPT_CAP: usize = 12;
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
            out.push_str(&shown.join(", "));
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
            const CUSTOM_PROMPT_CAP: usize = 12;
            let shown: Vec<String> = self
                .custom_tags
                .iter()
                .take(CUSTOM_PROMPT_CAP)
                .map(|(k, v)| format!("{k}: {v}"))
                .collect();
            let overflow = self.custom_tags.len().saturating_sub(CUSTOM_PROMPT_CAP);
            out.push_str("custom: ");
            out.push_str(&shown.join("; "));
            if overflow > 0 {
                out.push_str(&format!(" (+{} more)", overflow));
            }
            out.push('\n');
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
    pub fn to_json_prompt(&self) -> String {
        const EVENTS_PROMPT_CAP: usize = 5;
        const ENTITY_VALUE_PROMPT_CHARS: usize = 400;
        // (2026-08-16 yellow S4) The per-field legal maxima (500 entities ×
        // 400-char values + summary + events) compose to ~25× CTX_SCHEMA —
        // an at-the-caps schema overflowed the 2048-token prompt and the
        // middle-drop spliced a contiguous band out of the sorted JSON the
        // model must diff against (re-minted keys → growth spiral). The
        // renderer now enforces a TOTAL char budget: entities are included
        // in priority order until the budget is spent, the rest counted in a
        // visible `(+N trimmed)` marker. Priority = `player.*` identity keys
        // first (the diff anchor, never many), then `entity_order` (FIFO =
        // oldest first, the same recency assumption eviction uses), then any
        // keys the order list never knew (sorted — deterministic fallback).
        let budget = crate::settings::SCHEMA_JSON_PROMPT_BUDGET_CHARS;
        let events: Vec<String> = if self.recent_events.len() > EVENTS_PROMPT_CAP {
            self.recent_events[self.recent_events.len() - EVENTS_PROMPT_CAP..]
                .iter()
                .map(|e| Self::flatten_inline(e))
                .collect()
        } else {
            self.recent_events
                .iter()
                .map(|e| Self::flatten_inline(e))
                .collect()
        };
        // Deterministic inclusion order (see the budget note above).
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
            if used + entry_cost > budget {
                trimmed = self.entities.len() - entities.len();
                break;
            }
            used += entry_cost;
            entities.insert(k.clone(), value);
        }
        let mut obj = serde_json::json!({
            "summary": Self::flatten_inline(&self.summary),
            "recent_events": events,
            "entities": entities,
        });
        if trimmed > 0 {
            tracing::warn!(
                total = self.entities.len(),
                shown = self.entities.len() - trimmed,
                budget_chars = budget,
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
    /// the relationships map keeps the first 48 keys in sorted order
    /// (deterministic); a travel graph over the node cap is a hard error
    /// (refusing an authored hub beats silently dropping it — checked at
    /// the install arm, not here).
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
            let keep: std::collections::HashSet<String> =
                std::collections::BTreeSet::from_iter(self.relationships.keys().cloned())
                    .into_iter()
                    .take(MAX_TRACKED_RELATIONSHIPS)
                    .collect();
            self.relationships.retain(|k, _| keep.contains(k));
        }
        self.cap_recent_events();
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
        for key in ["npc_registry", "relationships", "presences", "offscreen_tasks"] {
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
                continue; // a non-object file is ignored (defensive; shouldn't happen)
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

        // Clean-delete safety net (2026-08-07): the 22-part `BodyPart` set
        // REPLACED the old 16-part set outright — `Torso`, `LeftBicep`,
        // `LeftThigh`, `LeftAnkle` + mirrors no longer name a real variant.
        // A pre-reorg `player.json` carrying those dead keys would ERROR on
        // the deserialize below (serde rejects unknown enum variants). Filter
        // the `player_state.body` object to ONLY the 22 known PascalCase wire
        // keys before deserializing — dead-part injury data simply vanishes
        // (the part no longer exists), no remap, no crash. Best-effort: if the
        // shape isn't the expected nested object, leave it untouched + let the
        // deserialize surface the real error.
        if let Some(ps) = merged.get_mut("player_state").and_then(|v| v.as_object_mut()) {
            if let Some(body) = ps.get_mut("body").and_then(|v| v.as_object_mut()) {
                let known = crate::player_state::BodyPart::wire_keys();
                body.retain(|key, _| known.contains(key.as_str()));
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

        // Legacy item migration (2026-08-07): absorb freeform `item_*`/`inv_*`
        // entity keys (the deleted `panels/inventory.js` convention) into the
        // typed equipment/belt/pack model. Idempotent — a second load finds no
        // legacy keys → no-op. Runs AFTER deserialize so the typed migration
        // helper (`equipment::migrate_legacy_items`) works against real Rust
        // types rather than JSON values. The entities map shrinks as items
        // leave it; the typed model grows by the same amount.
        if !schema.entities.is_empty() {
            crate::equipment::migrate_legacy_items(
                &mut schema.entities,
                &mut schema.player_state.equipment,
                &mut schema.player_state.pack,
            );
        }

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

/// The starting world-state anchors derived from a card's `.intro` by the
/// launch-time bootstrap pass (2026-08-10). A sibling of `CardStart`
/// (sim_card.rs) — both seed the dormant clock/weather/location at
/// `enter_fable_session`, but `CardStart` is *authored* (the card's `<start>`
/// block) while `BootstrapAnchors` is *derived* (one schema-engine pass reads
/// the intro + extracts the implied time/weather/location). The bootstrap runs
/// only when the `<start>` block left an anchor dormant (no `<start>` block, or
/// it seeded only one of clock/weather). Mirrors the cold-start seed discipline:
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
        let location = match (parsed.location_id, parsed.location_name) {
            (Some(id), Some(name)) => {
                match (
                    clean_anchor(id, ANCHOR_TEXT_MAX),
                    clean_anchor(name, ANCHOR_TEXT_MAX),
                ) {
                    (Some(id), Some(name)) => Some((id, name)),
                    _ => None,
                }
            }
            _ => None,
        };
        Ok(Self { time_minutes, weather, location })
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
        // No calendar yet → legacy render.
        let legacy = schema.render_for_prompt();
        assert!(legacy.contains("clock: Day 2, 14:00"));
        assert!(!legacy.contains("date:"));
        // With a calendar → date: + time-of-day clock:.
        schema.calendar = Some("3rd of Harvest, Year 1247".into());
        let labeled = schema.render_for_prompt();
        assert!(labeled.contains("date: 3rd of Harvest, Year 1247"));
        assert!(labeled.contains("clock: 14:00"));
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
    fn render_for_prompt_caps_recent_events_at_five() {
        let schema = WorldSchema {
            summary: String::new(),
            recent_events: (0..10).map(|i| format!("event{i}")).collect(),
            entities: BTreeMap::new(),
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

    #[test]
    fn load_split_drops_legacy_body_part_keys() {
        // 2026-08-07 clean-delete safety net. A pre-reorg player.json carries
        // the deleted 16-part keys (Torso, LeftBicep, LeftThigh, LeftAnkle, …)
        // alongside — in principle — the new 22-part keys. The load seam MUST
        // drop the dead keys before deserializing, else serde errors on the
        // unknown variant. After load: the known key survived at its severity,
        // the dead keys vanished (no panic, no remap), and the body has exactly
        // the entries the save carried for live parts.
        use crate::player_state::{BodyPart, BodyPartState};
        let dir = std::env::temp_dir();
        let world = dir.join("wupi_loadsplit_drop_world.json");
        let player = dir.join("wupi_loadsplit_drop_player.json");
        let npc = dir.join("wupi_loadsplit_drop_npc.json");
        for p in [&world, &player, &npc] {
            let _ = std::fs::remove_file(p);
        }
        // world.json + npc.json empty; player.json carries a mix of live +
        // dead body keys.
        let legacy_player = r#"{
            "player_state": {
                "body": {
                    "LeftUpperArm": "Orange",
                    "Torso": "Red",
                    "LeftBicep": "Red",
                    "LeftThigh": "Purple",
                    "LeftAnkle": "Yellow"
                },
                "stamina": "Winded"
            }
        }"#;
        std::fs::write(&player, legacy_player).unwrap();
        std::fs::write(&world, "{}").unwrap();
        std::fs::write(&npc, "{}").unwrap();

        let schema = WorldSchema::load_split(&world, &player, &npc).unwrap();
        // The one live key survived at its severity.
        assert_eq!(
            schema.player_state.body.get(&BodyPart::LeftUpperArm).copied(),
            Some(BodyPartState::Orange),
        );
        // Stamina came through too (the filter only touched `body`).
        assert_eq!(
            schema.player_state.stamina,
            crate::player_state::Stamina::Winded,
        );
        // The body map has EXACTLY one entry — the four dead keys were
        // dropped, not remapped onto new parts.
        assert_eq!(schema.player_state.body.len(), 1);
        for p in [&world, &player, &npc] {
            let _ = std::fs::remove_file(p);
        }
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
        ents.insert("world_clock".to_string(), Some(serde_json::Value::String("9999".into())));
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
                setting: String::new(),
            });
        }
        assert_eq!(g.nodes.len(), MAX_TRAVEL_NODES);
        assert_eq!(g.resolve_or_mint_node("brand new place", &[]), None, "cap refuses new nodes");
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
            setting: String::new(),
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
            setting: String::new(),
        });
        let id = g.resolve_or_mint_node("market", &[]).expect("ambiguous fragment mints fresh");
        assert_eq!(id, "market", "minted as its own node — no silent guess between the two");
        // Noise fragments ("the") must never subset-match a long compound id.
        let mut g2 = sample_travel_graph();
        g2.upsert_node(Node {
            id: "the-crooked-lantern-tavern".to_string(),
            name: "The Crooked Lantern Tavern".to_string(),
            neighbors: vec![],
            setting: String::new(),
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
            setting: String::new(),
        });
        assert_eq!(
            g2.resolve_fragment_alias("market"),
            None,
            "ambiguous containment declines — the guard only blocks high-confidence dupes"
        );
    }

    #[test]
    fn similarity_orders_exact_partial_and_distant() {
        assert!((similarity("market_square", "market_square") - 1.0).abs() < 1e-6);
        assert!(similarity("mrket_square", "market_square") >= 0.75, "single dropped char resolves");
        assert!(similarity("king_s_road", "market_square") < 0.75, "different place mints");
        assert_eq!(similarity("", "anything"), 0.0);
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

    // --- upsert_node (dynamic world-seeding, [DISCOVER] applier) ---

    #[test]
    fn upsert_node_inserts_new_node() {
        let mut g = TravelGraph::default();
        let inserted = g.upsert_node(Node {
            id: "shell_town".into(),
            name: "Shell Town".into(),
            neighbors: vec![],
            setting: "outdoor".into(),
        });
        assert!(inserted, "first insert returns true");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.find_node("shell_town").unwrap().name, "Shell Town");
    }

    #[test]
    fn upsert_node_is_idempotent_on_duplicate_id() {
        // Re-discovering an existing id is a no-op (the tracker may re-emit it).
        let mut g = TravelGraph::default();
        g.upsert_node(Node { id: "shell_town".into(), name: "Shell Town".into(), neighbors: vec![], setting: String::new() });
        let inserted = g.upsert_node(Node { id: "shell_town".into(), name: "DIFFERENT NAME".into(), neighbors: vec![], setting: String::new() });
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
            nodes: vec![Node { id: "loguetown".into(), name: "Loguetown".into(), neighbors: vec![], setting: String::new() }],
            current_node: None,
        };
        g.upsert_node(Node {
            id: "shell_town".into(),
            name: "Shell Town".into(),
            neighbors: vec!["loguetown".into()],
            setting: String::new(),
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
            setting: String::new(),
        });
        assert!(g.find_node("shell_town").unwrap().neighbors.contains(&"foosha".to_string()),
            "forward edge to unknown neighbor kept");
        // Now discover foosha naming shell_town — the back-link resolves.
        g.upsert_node(Node {
            id: "foosha".into(),
            name: "Foosha Village".into(),
            neighbors: vec!["shell_town".into()],
            setting: String::new(),
        });
        assert!(g.find_node("shell_town").unwrap().neighbors.contains(&"foosha".to_string()));
        assert!(g.find_node("foosha").unwrap().neighbors.contains(&"shell_town".to_string()));
    }

    #[test]
    fn upsert_node_empty_id_returns_false() {
        let mut g = TravelGraph::default();
        let inserted = g.upsert_node(Node { id: String::new(), name: "X".into(), neighbors: vec![], setting: String::new() });
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
                Node { id: "tavern".into(), name: "Tavern".into(), neighbors: vec!["cellar".into()], setting: "indoor".into() },
                Node { id: "cellar".into(), name: "Cellar".into(), neighbors: vec!["tavern".into()], setting: "".into() },
                Node { id: "docks".into(), name: "Docks".into(), neighbors: vec![], setting: "outdoor".into() },
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
        };
        schema.apply_delta(delta);
        assert_eq!(schema.npc_registry, original, "registry must be LLM-immutable");
        assert_eq!(
            schema.entities.get("npc_registry").and_then(|s| s.as_str()),
            Some("injected"),
            "the injected key lands in entities (legacy), NOT the typed field"
        );
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
        };
        schema.apply_delta(delta);
        assert_eq!(schema.presences, original, "presences must be LLM-immutable");
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
                    id: "mara_the_innkeep".into(),
                    name: "Mara".into(),
                    role: String::new(),
                    tier: None,
                    aliases: vec![],
                },
                NpcEntry {
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
        reg.upsert_entry(NpcEntry { id: "coby".into(), name: "Coby".into(), role: String::new(), tier: None, aliases: vec![] });
        let inserted = reg.upsert_entry(NpcEntry { id: "coby".into(), name: "DIFFERENT".into(), role: String::new(), tier: None, aliases: vec!["newalias".into()] });
        assert!(!inserted, "duplicate id returns false");
        assert_eq!(reg.entries.len(), 1);
        // Original entry preserved; new alias NOT merged (stable registry).
        assert_eq!(reg.find("coby").unwrap().name, "Coby");
        assert!(!reg.resolve("newalias").is_some(), "re-registration does not merge new aliases");
    }

    #[test]
    fn upsert_entry_empty_id_returns_false() {
        let mut reg = NpcRegistry::default();
        let inserted = reg.upsert_entry(NpcEntry { id: String::new(), name: "X".into(), role: String::new(), tier: None, aliases: vec![] });
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
}
