//! Fable Phase 4 Component 4 — rumor propagation mechanics (2026-07-28).
//!
//! The LAST Phase 4 component. A rumor is a free-form diegetic phrase ("the
//! stranger paid in gold", "the captain is looking for someone") that spreads
//! between connected nodes on the World Progression tick. Each rumor owns its
//! `known_nodes` — the nodes that have heard it. The narrator sees only the
//! rumors the CURRENT node knows (the node-based knowledge model — "the
//! tavern has heard X", not "Marcus specifically has heard X").
//!
//! Pure Rust. The World Progression tick calls [`propagate_rumors`] each fire;
//! each rumor attempts to spread from its `known_nodes` to their adjacent
//! unknown neighbors via a per-edge d20 roll against an age-decayed DC.
//! Deterministic via a seeded xorshift RNG (mirrors
//! [`crate::weather::drift_weather`] + [`crate::offscreen_task::resolve_task`]
//! — same FNV-1a seed + [`crate::player_state::Roller`] /
//! [`crate::player_state::roll_d20`] primitives). Combat ticks are suspended
//! upstream by [`crate::schema::SceneMode::progression_interval_hours`]
//! `== 0`, so rumors are stable mid-fight unless the tracker explicitly emits
//! `[RUMOR]`.
//!
//! Architecture line: schema owns the *data* ([`crate::schema::WorldSchema`]
//! `.rumors`: `Vec<Rumor>`); this module owns the *mechanics* (DC curve +
//! spread fn). Same separation as [`crate::weather`] (data lives on schema,
//! drift lives in the module) and [`crate::offscreen_task`] (queue on schema,
//! resolution in the module). The d20 + per-edge rolls are NEVER shown to the
//! narrator — only the new `known_nodes` set as a hard fact in `<world_state>`
//! (`rumors:` line) plus a directive seed prompting the narrator to weave
//! ambient gossip into the prose. Same anti-sycophancy contract as the combat
//! + skill-check Referees + the weather drift directive.
//!
//! ## Anti-bloat audit (what v1 deliberately does NOT do)
//!
//! This component is propagation-only by design (per Chloe's locked verdict,
//! 2026-07-28). It is NOT a reputation economy. Specifically:
//!
//! - **No polarity / truth field on rumors.** The originating event IS the
//!   truth; the rumor is just its propagation. Reputation is narratively
//!   derived from which rumor texts circulate (the narrator reads the
//!   `rumors:` line + frames NPC reactions in prose). A polarity enum or a
//!   stored reputation score would push toward the faction-standing-matrix
//!   trap (the bloat Gemini's original Phase 4 draft fell into). Forward-
//!   compat: a `polarity: Option<...>` field can be added later if playtesting
//!   shows the narrator needs a mechanical signal.
//! - **No per-NPC knowledge graphs.** Knowledge is node-based. A node "knows"
//!   the rumors that have propagated to it; NPCs at that node inherit. The
//!   player learns "the tavern has heard X", not "Marcus specifically has
//!   heard X". Per-NPC belief tracking is the anti-bloat trap.
//! - **No `Apocalyptic` / `Global` magnitude tier.** Propagation is bounded by
//!   graph topology + [`NEW_NODES_PER_TICK_CAP`] — a rumor structurally
//!   cannot reach every node in one tick. There is no magnitude enum here at
//!   all (unlike [`crate::offscreen_task::FocusMagnitude`]); the cap is the
//!   sole bound. The "no apocalyptic shift" rule from the architect directive
//!   is honored by the absence of any unbounded path.
//! - **No weighted decay curves / Bayesian confidence per edge.** A single
//!   d20 vs age-decayed DC; binary spread. Modeling per-edge belief
//!   confidence is over-engineering for v1.
//! - **No milestone-derived auto-seeding.** v1 is bracket-only (`[RUMOR ...]`);
//!   the Tracker consciously authors rumors. A one-line hook in the
//!   `[MILESTONE]` apply block can auto-seed notable events in v2 IF
//!   playtesting shows the Tracker under-emits (forward-compat, zero v1 cost).
//!
//! v1: conservative DC constants that likely need a live tuning pass (mirrors
//! the §11.41 DRY-multiplier + §11.45 weather-DC "conservative starting
//! values" pattern).

use crate::player_state::{roll_d20, Roller};
use crate::schema::TravelGraph;

/// A circulating rumor. Free-form diegetic phrase + propagation state.
///
/// The rumor itself carries no polarity / truth value — the originating event
/// IS the truth (per the locked v1 verdict). Propagation state is the SOLE
/// mutable concern: `known_nodes` grows as the rumor spreads.
///
/// Rust is the SOLE authority — `apply_delta` does NOT touch
/// [`crate::schema::WorldSchema::rumors`] (mirrors `world_clock` / `weather` /
/// `travel_graph`). The only writers are (1) the `[RUMOR ...]` bracket command
/// (creates a rumor rooted at the current node — `known_nodes` initialized to
/// `[origin_node]`), (2) the World Progression tick propagation pass
/// ([`propagate_rumors`] — appends newly-reached node ids to `known_nodes`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rumor {
    /// The free-form diegetic phrase the narrator weaves into ambient gossip
    /// ("the stranger paid in gold coins", "a bandit scout was seen at the
    /// ridge"). Spaces allowed. Set once at creation; never mutated.
    pub label: String,
    /// The node id where the rumor originated (where the `[RUMOR]` was
    /// emitted). Always a member of `known_nodes`. Set once at creation.
    pub origin_node: String,
    /// The node ids that have heard the rumor (includes `origin_node`). Grows
    /// monotonically as [`propagate_rumors`] spreads the rumor to adjacent
    /// unknown nodes. The `rumors:` render line filters this by the current
    /// node — "what does THIS node know?".
    pub known_nodes: Vec<String>,
    /// The in-world minute the rumor was born (epoch-minutes, same units as
    /// [`crate::schema::WorldClock::current_minutes`]). Set once at creation;
    /// drives the age-decayed [`propagation_dc`] (fresh rumors spread fast,
    /// stale news slow). Never mutated.
    pub born_minutes: i64,
}

/// Base spread DC. The spread check: d20 (no modifier) vs [`propagation_dc`].
/// `roll >= dc` → spreads to that edge's destination node; `roll < dc` → the
/// edge doesn't carry the rumor this tick. Low DC = easy to spread (fresh
/// gossip, people talk); high DC = hard (stale news). v1 default — likely
/// needs a live tuning pass.
const SPREAD_BASE_DC: i32 = 6;

/// Spread DC bonus per 8 in-world hours since the rumor was born. Long-running
/// rumors spread slower (yesterday's news doesn't travel). 8h (not 4h like
/// weather) because news aging is slower than weather persistence. v1 default.
const SPREAD_AGE_BONUS_PER_8H: i32 = 1;

/// Cap on the age bonus. Max DC = base 6 + cap 8 = 14 (~25% spread per edge).
/// v1 default.
const SPREAD_AGE_BONUS_CAP: i32 = 8;

/// The HARD anti-saturation cap: the maximum number of NEW nodes a single
/// rumor can reach per tick. Prevents runaway saturation (a hub-connected
/// rumor reaching the whole graph in 2-3 ticks) and gives organic asymmetric
/// spread the player can race against or exploit ("the rumor hasn't reached
/// the guardhouse yet — I can still get ahead of it").
///
/// This is the load-bearing anti-bloat guard, mirroring
/// [`crate::offscreen_task::FocusMagnitude::per_tick_cap`] in spirit: a
/// bounded per-tick mutation count that structurally prevents an apocalyptic
/// shift (here, "every node knows everything instantly"). Pinned by the
/// `new_nodes_per_tick_cap_is_two` test. v1 default.
const NEW_NODES_PER_TICK_CAP: usize = 2;

/// Spread DC for a rumor, scaled by how long ago it was born.
/// `age_minutes` is `now - born_minutes`.
///
/// - 0h old → DC 6 (~75% spread per edge on an unmodified d20).
/// - 8h old → DC 7.
/// - 64h+ old → DC 14 (capped; ~25% spread per edge).
/// - Negative age (defensive — clock moved backward somehow) → DC 6.
///
/// Mirror of [`crate::weather::persistence_dc`] (same shape; different
/// constants — news ages on an 8h cadence, weather on 4h). Pure.
pub fn propagation_dc(age_minutes: i64) -> i32 {
    let age_8h = (age_minutes.max(0) / 480) as i32;
    SPREAD_BASE_DC + (age_8h * SPREAD_AGE_BONUS_PER_8H).min(SPREAD_AGE_BONUS_CAP)
}

/// FNV-1a 64-bit hash of the seed string. Kept local so this module stays
/// self-contained (mirrors [`crate::weather::hash_seed`] +
/// [`crate::offscreen_task::hash_task`] +
/// [`crate::player_state::hash_text`] — same FNV-1a prime, same pattern).
/// `crate::player_state::hash_text` is module-private, so each tick-resolved
/// module redefines its own local copy — this is the established convention.
fn hash_seed(s: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// Propagation pass. Called once per World Progression tick fire.
///
/// For each rumor, iterate its `known_nodes`; for each known node's neighbors
/// NOT already in `known_nodes`, roll d20 vs [`propagation_dc`] (seeded per
/// (tick, rumor, from, to)) with a per-rumor `new_nodes_this_tick` counter
/// enforcing [`NEW_NODES_PER_TICK_CAP`]. On `roll >= dc`, the destination is
/// appended to the rumor's `known_nodes`.
///
/// Returns `(new_rumors, directives)`:
/// - `new_rumors`: a fresh `Vec<Rumor>` with updated `known_nodes`. Identical
///   to the input when no spread occurred (so the caller's
///   `if new_rumors != old_rumors` gate is correct — no false mutations).
/// - `directives`: one string per rumor that spread AT ALL, formatted for the
///   narrator's `<directives>` block (mirrors
///   [`crate::weather::drift_weather`]'s directive shape). Lists the newly-
///   reached nodes + an imperative to weave ambient gossip into the prose.
///   Empty when nothing spread.
///
/// Deterministic per (rumor label, known set, graph, now_minutes): same seed
/// → same outcome (testable + replayable, mirrors
/// [`crate::offscreen_task::resolve_task`] + [`crate::weather::drift_weather`]).
///
/// Pure. Does NOT mutate the input.
///
/// # Combat suspension
///
/// The caller ([`crate::lib::apply_time_command_and_maybe_tick`]) gates the
/// tick on `progression_interval_hours() == 0` upstream — so this fn is never
/// called mid-combat. Rumors are stable during a fight unless the Tracker
/// explicitly forces `[RUMOR]`. No combat-awareness needed here.
pub fn propagate_rumors(
    rumors: &[Rumor],
    graph: &TravelGraph,
    now_minutes: i64,
) -> (Vec<Rumor>, Vec<String>) {
    if rumors.is_empty() || !graph.is_set() {
        return (rumors.to_vec(), Vec::new());
    }

    let mut new_rumors: Vec<Rumor> = Vec::with_capacity(rumors.len());
    let mut directives: Vec<String> = Vec::new();
    let mut any_spread = false;

    for rumor in rumors {
        let age = now_minutes.saturating_sub(rumor.born_minutes);
        let dc = propagation_dc(age);

        // Collect candidate edges: (from_node, to_node) where `from` is a
        // known node, `to` is one of its neighbors, and `to` is NOT already
        // known. Dedup `to` across `from`s (a rumor doesn't spread "twice as
        // fast" to a node reachable from two known nodes — one roll decides
        // its fate this tick).
        let mut candidates: Vec<(String, String)> = Vec::new();
        let mut seen_dest: std::collections::HashSet<String> = std::collections::HashSet::new();
        for from in &rumor.known_nodes {
            let Some(from_node) = graph.find_node(from) else {
                continue; // known node drifted off the graph (shouldn't happen)
            };
            for to in &from_node.neighbors {
                if rumor.known_nodes.iter().any(|n| n == to) {
                    continue; // already known
                }
                if !seen_dest.insert(to.clone()) {
                    continue; // already a candidate via another known node
                }
                candidates.push((from.clone(), to.clone()));
            }
        }

        let mut updated = rumor.clone();
        let mut newly_reached: Vec<String> = Vec::new();

        for (from, to) in candidates {
            if newly_reached.len() >= NEW_NODES_PER_TICK_CAP {
                break; // anti-saturation cap (the load-bearing guard)
            }
            // Seed per (tick, rumor label, from, to) — deterministic per edge.
            // Each (tick, rumor, edge) tuple rolls independently; the same
            // tuple always produces the same roll (testable + replayable).
            let seed = hash_seed(&format!("{}|{}|{}|{}", now_minutes, rumor.label, from, to));
            let mut roller = Roller::new(seed);
            let roll = roll_d20(&mut roller);
            if (roll as i32) >= dc {
                updated.known_nodes.push(to.clone());
                newly_reached.push(to);
            }
        }

        if !newly_reached.is_empty() {
            any_spread = true;
            // Resolve node ids to diegetic names where possible (mirrors
            // TravelGraph::render_line's id→name resolution).
            let names: Vec<String> = newly_reached
                .iter()
                .map(|id| {
                    graph
                        .find_node(id)
                        .map(|n| n.name.clone())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            directives.push(format!(
                "Rumor spread — '{}' has reached {}. Weave ambient gossip \
                 into the prose where NPCs at those locations would plausibly \
                 have heard it (overheard fragments, knowing glances, averted \
                 eyes). This is a hard fact; do not contradict it.",
                rumor.label,
                names.join(", ")
            ));
        }

        new_rumors.push(updated);
    }

    // If nothing spread at all, return the input unchanged so the caller's
    // `new_rumors != old_rumors` gate stays exact (no spurious snapshot).
    if !any_spread {
        return (rumors.to_vec(), Vec::new());
    }

    (new_rumors, directives)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Node, TravelGraph};

    /// Build a small linear graph: tavern — cellar — cellar_tunnel.
    /// `current_node` is the tavern (irrelevant to propagation, but kept for
    /// realism).
    fn linear_graph() -> TravelGraph {
        TravelGraph {
            nodes: vec![
                Node {
                    id: "tavern".to_string(),
                    name: "The Rusty Tavern".to_string(),
                    neighbors: vec!["cellar".to_string()],
                    setting: String::new(), ..Default::default()
                },
                Node {
                    id: "cellar".to_string(),
                    name: "The Cellar".to_string(),
                    neighbors: vec!["tavern".to_string(), "cellar_tunnel".to_string()],
                    setting: "indoor".to_string(), ..Default::default()
                },
                Node {
                    id: "cellar_tunnel".to_string(),
                    name: "Smuggler's Tunnel".to_string(),
                    neighbors: vec!["cellar".to_string()],
                    setting: "indoor".to_string(), ..Default::default()
                },
            ],
            current_node: Some("tavern".to_string()),
        }
    }

    /// Build a star graph: tavern connects to 3 leaves (cellar, market,
    /// guardhouse). Useful for the per-tick cap test (one known node → 3
    /// candidate edges).
    fn star_graph() -> TravelGraph {
        TravelGraph {
            nodes: vec![
                Node {
                    id: "tavern".to_string(),
                    name: "The Rusty Tavern".to_string(),
                    neighbors: vec![
                        "cellar".to_string(),
                        "market".to_string(),
                        "guardhouse".to_string(),
                    ],
                    setting: String::new(), ..Default::default()
                },
                Node {
                    id: "cellar".to_string(),
                    name: "The Cellar".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: "indoor".to_string(), ..Default::default()
                },
                Node {
                    id: "market".to_string(),
                    name: "Market Square".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: String::new(), ..Default::default()
                },
                Node {
                    id: "guardhouse".to_string(),
                    name: "Guardhouse".to_string(),
                    neighbors: vec!["tavern".to_string()],
                    setting: "indoor".to_string(), ..Default::default()
                },
            ],
            current_node: Some("tavern".to_string()),
        }
    }

    #[test]
    fn propagation_dc_at_zero_age_is_base() {
        assert_eq!(propagation_dc(0), 6);
    }

    #[test]
    fn propagation_dc_scales_per_8h() {
        assert_eq!(propagation_dc(480), 7); // 8h
        assert_eq!(propagation_dc(960), 8); // 16h
        assert_eq!(propagation_dc(1440), 9); // 24h
    }

    #[test]
    fn propagation_dc_caps_at_64h() {
        // 64h = 3840 min = 8 × 8h → bonus 8 (the cap). DC = 6 + 8 = 14.
        assert_eq!(propagation_dc(3840), 14);
        // Well past 64h: still capped.
        assert_eq!(propagation_dc(10_000), 14);
        assert_eq!(propagation_dc(1_000_000), 14);
    }

    #[test]
    fn propagation_dc_negative_age_is_clamped_to_base() {
        // Defensive: a clock regression (shouldn't happen — the [TIME]
        // applier guards against it) must not produce a sub-base DC.
        assert_eq!(propagation_dc(-1), 6);
        assert_eq!(propagation_dc(-10_000), 6);
    }

    #[test]
    fn new_nodes_per_tick_cap_is_two() {
        // The load-bearing anti-bloat guard. Pinned — mirrors the
        // `focus_magnitude_caps_enforce_no_apocalyptic` test in
        // offscreen_task.rs. Changing this const is a deliberate design
        // decision, not a silent edit.
        assert_eq!(NEW_NODES_PER_TICK_CAP, 2);
    }

    #[test]
    fn hash_seed_is_deterministic() {
        assert_eq!(hash_seed("1|rumor|tavern|cellar"), hash_seed("1|rumor|tavern|cellar"));
    }

    #[test]
    fn hash_seed_diverges_on_different_inputs() {
        // Sanity: at least one of these must differ from the baseline.
        let baseline = hash_seed("1|rumor|tavern|cellar");
        let others = [
            hash_seed("2|rumor|tavern|cellar"), // different tick
            hash_seed("1|other|tavern|cellar"), // different label
            hash_seed("1|rumor|market|cellar"), // different from
            hash_seed("1|rumor|tavern|market"), // different to
        ];
        assert!(
            others.iter().any(|&h| h != baseline),
            "hash_seed should diverge across (tick, label, from, to) tuples"
        );
    }

    #[test]
    fn propagate_empty_rumors_is_noop() {
        let g = linear_graph();
        let (out, dirs) = propagate_rumors(&[], &g, 10_000);
        assert!(out.is_empty());
        assert!(dirs.is_empty());
    }

    #[test]
    fn propagate_unset_graph_is_noop() {
        // An unset graph (no nodes) can't carry propagation.
        let unset = TravelGraph::default();
        let rumor = Rumor {
            label: "test".to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: 0,
        };
        let (out, dirs) = propagate_rumors(&[rumor.clone()], &unset, 10_000);
        assert_eq!(out, vec![rumor]);
        assert!(dirs.is_empty());
    }

    #[test]
    fn propagate_is_deterministic_for_same_args() {
        let g = linear_graph();
        let rumor = Rumor {
            label: "the stranger paid in gold".to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: 0,
        };
        // Same (rumor, graph, now_minutes) → same outcome.
        let (a_out, a_dirs) = propagate_rumors(&[rumor.clone()], &g, 10_000);
        let (b_out, b_dirs) = propagate_rumors(&[rumor], &g, 10_000);
        assert_eq!(a_out, b_out);
        assert_eq!(a_dirs, b_dirs);
    }

    #[test]
    fn propagate_eventually_spreads_at_least_once_over_a_sweep() {
        // At age 0 (DC 6, ~75% per edge), a single-edge rumor should spread
        // on at least one tick in a sweep over many minutes. (If the RNG
        // never produced a spread across 500 ticks, the seeding or roll
        // comparison is broken.)
        let g = linear_graph();
        let rumor = Rumor {
            label: "the stranger paid in gold".to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: 0,
        };
        let any_spread = (0..500i64).any(|m| {
            let (out, dirs) = propagate_rumors(&[rumor.clone()], &g, m);
            !dirs.is_empty() || out[0].known_nodes.len() > 1
        });
        assert!(any_spread, "rumor should spread on at least one tick in a sweep");
    }

    #[test]
    fn propagate_respects_new_nodes_per_tick_cap() {
        // Star graph: tavern knows the rumor, with 3 candidate edges
        // (tavern→cellar, tavern→market, tavern→guardhouse). Even if all 3
        // rolls would pass, at most NEW_NODES_PER_TICK_CAP (=2) can be added.
        let g = star_graph();
        let rumor = Rumor {
            label: "the captain is looking for someone".to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: 0,
        };
        for m in 0..500i64 {
            let (out, _dirs) = propagate_rumors(&[rumor.clone()], &g, m);
            let new_count = out[0]
                .known_nodes
                .iter()
                .filter(|n| **n != "tavern")
                .count();
            assert!(
                new_count <= NEW_NODES_PER_TICK_CAP,
                "rumor added {} new nodes at minute {} (cap {})",
                new_count,
                m,
                NEW_NODES_PER_TICK_CAP
            );
        }
    }

    #[test]
    fn propagate_never_duplicates_known_nodes() {
        // A node already in known_nodes must never be re-added (the candidate
        // filter excludes it, but double-check the output invariant).
        let g = linear_graph();
        // Start with cellar already known — propagation from tavern + cellar
        // should never re-add cellar.
        let rumor = Rumor {
            label: "test rumor".to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string(), "cellar".to_string()],
            born_minutes: 0,
        };
        for m in 0..500i64 {
            let (out, _dirs) = propagate_rumors(&[rumor.clone()], &g, m);
            let known = &out[0].known_nodes;
            let mut sorted = known.clone();
            sorted.sort();
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "known_nodes has duplicates at minute {}", m);
        }
    }

    #[test]
    fn propagate_stale_rumor_spreads_slower_than_fresh() {
        // Aggregate spread across many ticks for fresh (age 0, DC 6) vs stale
        // (age 64h+, DC 14). The fresh one must reach more nodes in aggregate.
        // (DC 6 ≈ 75% per edge; DC 14 ≈ 25% per edge.)
        let g = star_graph();
        let fresh_label = "fresh";
        let stale_label = "stale";

        // Fresh: born now, sweep 0..500.
        let fresh_rumor = Rumor {
            label: fresh_label.to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: 0,
        };
        let fresh_total: usize = (0..500i64)
            .map(|m| {
                let (out, _) = propagate_rumors(&[fresh_rumor.clone()], &g, m);
                out[0].known_nodes.len() - 1 // minus the origin
            })
            .sum();

        // Stale: born 64h ago (DC 14), sweep over the same minute range. We
        // shift the sweep so `now - born_minutes` stays ≥ 64h throughout.
        let stale_born = -3840; // 64h before minute 0
        let stale_rumor = Rumor {
            label: stale_label.to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: stale_born,
        };
        let stale_total: usize = (0..500i64)
            .map(|m| {
                let (out, _) = propagate_rumors(&[stale_rumor.clone()], &g, m);
                out[0].known_nodes.len() - 1
            })
            .sum();

        assert!(
            fresh_total > stale_total,
            "fresh rumor (DC 6) should spread more than stale (DC 14): {} vs {}",
            fresh_total,
            stale_total
        );
    }

    #[test]
    fn propagate_returns_input_unchanged_when_no_spread() {
        // When no spread occurs, the output Vec must equal the input Vec
        // exactly (so the caller's `new_rumors != old_rumors` gate is exact).
        // Construct a scenario where spread is impossible: a rumor known only
        // at a leaf node with no unknown neighbors.
        let g = linear_graph();
        let rumor = Rumor {
            label: "test".to_string(),
            origin_node: "cellar_tunnel".to_string(),
            known_nodes: vec![
                "cellar_tunnel".to_string(),
                "cellar".to_string(),
                "tavern".to_string(),
            ], // every node already known
            born_minutes: 0,
        };
        let (out, dirs) = propagate_rumors(&[rumor.clone()], &g, 10_000);
        assert_eq!(out, vec![rumor]);
        assert!(dirs.is_empty());
    }

    #[test]
    fn propagate_directive_resolves_node_ids_to_names() {
        // When spread occurs, the directive should reference diegetic names
        // where possible (mirrors TravelGraph::render_line). Find a tick
        // where the linear rumor spreads to the cellar + check the directive
        // names it "The Cellar".
        let g = linear_graph();
        let rumor = Rumor {
            label: "the stranger paid in gold".to_string(),
            origin_node: "tavern".to_string(),
            known_nodes: vec!["tavern".to_string()],
            born_minutes: 0,
        };
        for m in 0..500i64 {
            let (out, dirs) = propagate_rumors(&[rumor.clone()], &g, m);
            if !dirs.is_empty() {
                // The first directive references the spread; it should name
                // the destination by its diegetic name (or fall back to the
                // bare id). Either way it should contain the destination id
                // OR name as a substring.
                let directive = &dirs[0];
                let spread_reached = out[0]
                    .known_nodes
                    .iter()
                    .any(|n| n == "cellar");
                if spread_reached {
                    assert!(
                        directive.contains("The Cellar") || directive.contains("cellar"),
                        "directive should name the destination: {}",
                        directive
                    );
                }
                return; // one spread is enough to verify the format
            }
        }
        // If no spread ever occurred in 500 ticks, the seeding is likely
        // broken (covered by propagate_eventually_spreads_*). Don't fail here
        // — that test owns the "must spread" assertion.
    }
}
