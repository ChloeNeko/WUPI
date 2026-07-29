//! Fable Phase 4 Component 2 — weather drift mechanics (2026-07-28).
//!
//! Pure Rust. The World Progression tick calls [`drift_weather`] each fire; on
//! a failed persistence check, the current condition shifts to a new one
//! drawn from [`WEATHER_POOL`]. Deterministic via a seeded xorshift RNG
//! (mirrors [`crate::offscreen_task::resolve_task`] — same FNV-1a seed +
//! [`crate::player_state::Roller`] / [`crate::player_state::roll_d20`]
//! primitives). Combat ticks are suspended upstream by the
//! [`crate::schema::SceneMode::progression_interval_hours`] `== 0` gate, so
//! weather is stable mid-fight unless the tracker explicitly emits
//! `[WEATHER]`.
//!
//! Architecture line: schema owns the *data* ([`crate::schema::Weather`]);
//! this module owns the *mechanics* (pool + persistence curve + drift fn).
//! Same separation as [`crate::offscreen_task`] (queue lives on schema,
//! resolution lives here-adjacent). The d20 + pool pick are NEVER shown to
//! the narrator — only the new condition as a hard fact in `<world_state>`
//! plus a directive seed prompting the narrator to narrate the shift. Same
//! anti-sycophancy contract as the combat + skill-check Referees.
//!
//! v1: one generic pool; conservative DC constants that likely need a live
//! tuning pass (mirrors the §11.41 DRY-multiplier "conservative starting
//! values" pattern). Climate-specific pools (desert / arctic / coastal) are
//! a Component 4+ refinement (attach to a spatial node's climate flag — the
//! nodes themselves ship in Component 3, 2026-07-28, but per-node climate
//! pools are not modeled at v1).

use crate::player_state::{roll_d20, Roller};
use crate::schema::Weather;

/// The generic drift pool — the ONLY valid drift targets. Rust owns the dice;
/// the narrator owns the prose. Small + generic by design (anti-bloat: no
/// per-climate pools at v1). Climate-specific pools are a Component 4+
/// refinement (would attach to a node's climate flag; nodes ship in Component
/// 3, 2026-07-28). v1; tunable.
pub const WEATHER_POOL: &[&str] = &[
    "clear",
    "partly cloudy",
    "overcast",
    "light rain",
    "heavy rain",
    "thunderstorm",
    "fog",
    "light snow",
    "blizzard",
];

/// Base persistence DC. The persistence check: d20 (no modifier) vs
/// [`persistence_dc`]. `roll >= dc` → persists; `roll < dc` → shifts.
/// Low DC = easy to persist (short-elapsed weather holds); high DC = overdue
/// for a shift. v1 default — likely needs a live tuning pass.
const PERSISTENCE_BASE_DC: i32 = 8;

/// Persistence DC bonus per 4 in-world hours the current condition has held.
/// Long-running weather is more likely to shift (a 24h storm has earned its
/// exit). v1 default.
const PERSISTENCE_BONUS_PER_4H: i32 = 1;

/// Cap on the elapsed-time bonus. Max DC = base 8 + cap 6 = 14 (~30% persist).
/// v1 default.
const PERSISTENCE_BONUS_CAP: i32 = 6;

/// Persistence DC for the current condition, scaled by how long it has held.
/// `elapsed_minutes` is `now - started_at_minutes`.
///
/// - 0h elapsed → DC 8 (~65% persist on an unmodified d20).
/// - 4h elapsed → DC 9.
/// - 24h elapsed → DC 14 (capped; ~30% persist).
/// - Negative elapsed (defensive — clock moved backward somehow) → DC 8.
///
/// Pure.
pub fn persistence_dc(elapsed_minutes: i64) -> i32 {
    let elapsed_4h = (elapsed_minutes.max(0) / 240) as i32;
    PERSISTENCE_BASE_DC + (elapsed_4h * PERSISTENCE_BONUS_PER_4H).min(PERSISTENCE_BONUS_CAP)
}

/// FNV-1a 64-bit hash of the seed string. Kept local so this module stays
/// self-contained (mirrors [`crate::offscreen_task::hash_task`] +
/// [`crate::player_state::hash_text`] — same FNV-1a prime, same pattern).
fn hash_seed(s: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// Drift check. Called once per World Progression tick fire.
///
/// - Returns `None` if `weather` is unset (mirrors [`crate::schema::WorldClock`]
///   dormant behavior — no `[WEATHER]` yet, nothing to drift).
/// - Returns `None` if the persistence check passes (the current condition
///   holds — roll >= DC).
/// - Returns `Some(new_weather)` if the check fails: pick a new condition
///   from [`WEATHER_POOL`] via the seeded RNG, **excluding the current
///   condition** (a shift always produces a visible change). The new weather
///   carries a fresh `started_at_minutes = now_minutes` so the persistence
///   curve resets.
///
/// Deterministic per (current condition, now_minutes): same seed → same
/// outcome (testable + replayable, mirrors [`crate::offscreen_task::resolve_task`]).
///
/// Pure.
pub fn drift_weather(weather: &Weather, now_minutes: i64) -> Option<Weather> {
    if !weather.is_set() {
        return None;
    }
    let elapsed = now_minutes.saturating_sub(weather.started_at_minutes);
    let dc = persistence_dc(elapsed);

    // Seed by (now_minutes, current condition) — deterministic per tick. The
    // now_minutes term ensures consecutive ticks with the same condition
    // don't all resolve identically (each tick is a fresh roll); the
    // condition term ensures two different conditions at the same minute
    // diverge.
    let seed = hash_seed(&format!("{}|{}", now_minutes, weather.condition));
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);

    if (roll as i32) >= dc {
        return None; // persists
    }

    // Shift: pick from pool, excluding the current condition (never re-pick
    // the same one — a detected shift must produce a visible change).
    let current = weather.condition.as_str();
    let candidates: Vec<&str> = WEATHER_POOL
        .iter()
        .copied()
        .filter(|c| *c != current)
        .collect();
    if candidates.is_empty() {
        return None; // defensive: pool of size 1 — nothing to shift to.
    }
    let pick_idx = (roller.range(candidates.len())) as usize;
    let new_condition = candidates[pick_idx].to_string();

    Some(Weather {
        condition: new_condition,
        started_at_minutes: now_minutes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_dc_at_zero_elapsed_is_base() {
        assert_eq!(persistence_dc(0), 8);
    }

    #[test]
    fn persistence_dc_scales_per_4h() {
        assert_eq!(persistence_dc(240), 9); // 4h
        assert_eq!(persistence_dc(480), 10); // 8h
        assert_eq!(persistence_dc(720), 11); // 12h
    }

    #[test]
    fn persistence_dc_caps_at_24h() {
        // 24h = 1440 min = 6 × 4h → bonus 6 (the cap). DC = 8 + 6 = 14.
        assert_eq!(persistence_dc(1440), 14);
        // Well past 24h: still capped.
        assert_eq!(persistence_dc(10_000), 14);
        assert_eq!(persistence_dc(1_000_000), 14);
    }

    #[test]
    fn persistence_dc_negative_elapsed_is_clamped_to_base() {
        // Defensive: a clock regression (shouldn't happen — the [TIME]
        // applier guards against it) must not produce a sub-base DC.
        assert_eq!(persistence_dc(-1), 8);
        assert_eq!(persistence_dc(-10_000), 8);
    }

    #[test]
    fn drift_weather_unset_returns_none() {
        let unset = Weather::default();
        assert_eq!(drift_weather(&unset, 10_000), None);
    }

    #[test]
    fn drift_weather_is_deterministic_for_same_args() {
        let w = Weather {
            condition: "clear".to_string(),
            started_at_minutes: 0,
        };
        // Same (condition, now_minutes) → same outcome (whatever it is).
        let a = drift_weather(&w, 10_000);
        let b = drift_weather(&w, 10_000);
        assert_eq!(a, b);
    }

    #[test]
    fn drift_weather_different_minutes_can_differ() {
        // Sanity: at least one of a sweep over many minutes must produce a
        // different outcome than minute 0 (otherwise the seeding is broken).
        let w = Weather {
            condition: "clear".to_string(),
            started_at_minutes: 0,
        };
        let baseline = drift_weather(&w, 0);
        let any_differ = (1..200).any(|m| drift_weather(&w, m) != baseline);
        assert!(any_differ, "drift should vary across minutes");
    }

    #[test]
    fn drift_weather_shift_never_returns_the_current_condition() {
        // Sweep every pool member as the current condition; any drift
        // produced must NOT equal the current.
        for &current in WEATHER_POOL {
            let w = Weather {
                condition: current.to_string(),
                started_at_minutes: 0,
            };
            // Try many minutes so a shift (if any) is likely to surface.
            for m in 0..500i64 {
                if let Some(new) = drift_weather(&w, m) {
                    assert_ne!(
                        new.condition, current,
                        "drift returned the current condition"
                    );
                    assert!(
                        WEATHER_POOL.contains(&new.condition.as_str()),
                        "drift returned a non-pool condition: {}",
                        new.condition
                    );
                    assert_eq!(
                        new.started_at_minutes, m,
                        "drift must stamp started_at_minutes = now"
                    );
                }
            }
        }
    }

    #[test]
    fn drift_weather_shift_resets_started_at() {
        // Any drift outcome must carry started_at_minutes == now (fresh
        // persistence baseline).
        let w = Weather {
            condition: "clear".to_string(),
            started_at_minutes: 0,
        };
        for m in 0..500i64 {
            if let Some(new) = drift_weather(&w, m) {
                assert_eq!(new.started_at_minutes, m);
            }
        }
    }

    #[test]
    fn weather_pool_is_non_empty_and_has_no_dupes() {
        assert!(!WEATHER_POOL.is_empty());
        let mut sorted = WEATHER_POOL.to_vec();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "WEATHER_POOL has duplicate entries");
    }

    #[test]
    fn weather_pool_has_at_least_two_entries() {
        // A pool of size 1 would make drift impossible (can't exclude the
        // only member). Lock the minimum size.
        assert!(WEATHER_POOL.len() >= 2);
    }

    #[test]
    fn drift_weather_unset_at_negative_minute_is_still_none() {
        // Defensive: unset weather drifts to None regardless of clock value.
        let unset = Weather::default();
        assert_eq!(drift_weather(&unset, -1), None);
        assert_eq!(drift_weather(&unset, i64::MAX), None);
    }
}
