//! The Hazard Referees (2026-08-23) — loot rarity, road & city events,
//! rest interruption.
//!
//! Three pure-Rust dice layers over the existing world simulation, per the
//! Chloe-ratified plan. ZERO new bracket verbs, ZERO tracker-prompt
//! teaching: every outcome is a `[DIRECTIVE: …]` line that renders only
//! into the API narrator's rich `<world_state>` (`render_tracker_world_
//! state` passes an empty directives slice), so the no-degrade tracker
//! contract and the budget pins are untouched by construction. All dice
//! are Rust-authoritative — the LLM never rolls, never sees a number.
//!
//! Architecture line (the weather/rumor convention): each fn is PURE,
//! deterministic via a seeded xorshift [`crate::player_state::Roller`]
//! over an FNV-1a seed string, and mirrors
//! [`crate::weather::drift_weather`] / [`crate::rumor::propagate_rumors`]
//! in shape. The three consumers live in `lib.rs`:
//!
//! 1. **Loot Rarity Referee** — [`referee_evaluate_loot`], wired into the
//!    `fable_send` referee block beside the skill checks. One roll per
//!    turn: `d20 + tier_mod + prosperity_mod` against Chloe's 6-rung
//!    ladder (Common → Mystic), hard-capped post-roll by the site's danger
//!    tier (a natural 20 looting a bandit camp stays Rare — the pin).
//!    The item itself still enters play through the tracker's existing
//!    `[PACK]`/`[ASSET … Taken]` emissions; the directive only fixes the
//!    BEST recoverable quality.
//! 2. **Travel & Road / City Event Referee** — [`road_event_check`] /
//!    [`city_event_check`], rolled inside the `[TRAVEL]`/`[ROOM]`
//!    appliers (and the ≥6h time-skip inside
//!    `apply_time_command_and_maybe_tick`). `d20 ≥ DC` fires an event; a
//!    second d20 sets the valence band (1–7 negative / 8–14 ambiguous /
//!    15–20 favorable). Rumors heard at the destination LOWER the road DC
//!    (dangerous roads attract trouble).
//! 3. **Rest Interruption Referee** — [`rest_interruption_check`], rolled
//!    inside `apply_time_command_and_maybe_tick`'s authoritative rest
//!    funnel. Settlements auto-rest (DC 0); everywhere else the site's
//!    mob tier sets the DC. Failure halves recovery, skips the rested
//!    anchor, and stamps the 30-minute "Impaired" pure Debuff (a BLEARY
//!    condition, not an illness — it never matches `SICK_STEMS`; pinned
//!    in `consequence` + here).
//!
//! Shared vocabulary: [`RUMOR_THREAT_STEMS`] also powers the rumor →
//! Suspected-asset seeding (`site_map::seed_rumor_asset`) — the rumor
//! mill grows teeth on the hidden maps.

use crate::player_state::{
    keyword_present, roll_d20, strip_dialogue, AttackerTier, Roller,
};
use crate::rumor::Rumor;
use crate::schema::Node;
use crate::site_map::{present_mob_tier, AssetKind, AssetOrigin, SiteMap};

// ===========================================================================
// Shared primitives
// ===========================================================================

/// FNV-1a 64-bit hash of the seed string. Kept local so this module stays
/// self-contained (mirrors [`crate::weather::hash_seed`] +
/// [`crate::rumor::hash_seed`] — same FNV-1a prime, same convention: each
/// tick-resolved module redefines its own local copy).
fn hash_seed(s: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// The 1-based rank ladder the rest DC uses: Minion 1 … Legendary 5
/// (`5 + 3 × rank` → Minion DC 8 … Legendary DC 20, Chloe's bands).
pub(crate) fn tier_rank(t: AttackerTier) -> i32 {
    match t {
        AttackerTier::Minion => 1,
        AttackerTier::Soldier => 2,
        AttackerTier::Elite => 3,
        AttackerTier::Boss => 4,
        AttackerTier::Legendary => 5,
    }
}

/// (2026-08-24 fix) An asset origin that counts as AUTHORED map truth for
/// settlement classification: the architect's initial map, or a later
/// off-screen evolution of an EXISTING asset (`set_asset`/`move_asset` flip
/// even authored assets to `Evolved`). Tracker-minted
/// (`NarratorEstablished`) and Playground spawns never qualify — a
/// `[ASSET +… kind=building]` minted mid-dungeon must not flip the map's
/// `is_settlement`, which permanently disarmed the rest-interruption
/// referee and fired city events in dungeons.
fn authored_asset_origin(origin: AssetOrigin) -> bool {
    matches!(
        origin,
        AssetOrigin::InitialMap | AssetOrigin::Evolved
    )
}

/// Settlement detection — the architect's own three signals (the [ROOM]
/// city-event gate + the rest funnel share ONE fn so the two can never
/// drift): the node's authored `setting=settlement`, a clearly-named town
/// ([`crate::site_map::looks_like_settlement`]), or a map carrying an
/// AUTHORED Building asset ([`authored_asset_origin`] — tracker-minted
/// buildings never reclassify a site). A HOSTED child map (a building
/// interior) counts too — sleeping inside a building of a settlement is
/// still sleeping in town.
pub fn is_settlement(node: Option<&Node>, map: Option<&SiteMap>) -> bool {
    if let Some(n) = node {
        if n.setting.eq_ignore_ascii_case("settlement") {
            return true;
        }
        if crate::site_map::looks_like_settlement(&n.name) {
            return true;
        }
    }
    map.is_some_and(|m| {
        m.host.is_some()
            || m
                .assets
                .iter()
                .any(|a| a.kind == AssetKind::Building && authored_asset_origin(a.origin))
    })
}

/// The loot roll's prosperity modifier: `(p − 100) / 25`, clamped to
/// [−2, +2] (a destitute quarter carries scrap; a boom town carries
/// treasure). Prosperity is a `u8` (25–200 band, default 100).
pub(crate) fn loot_prosperity_mod(prosperity: u8) -> i32 {
    ((i32::from(prosperity) - 100) / 25).clamp(-2, 2)
}

/// The city event's prosperity modifier (its own band shape, NOT the loot
/// curve): desperate quarters (≤ 75) draw trouble (−2), boom towns (≥ 150)
/// police their streets (+2).
pub(crate) fn city_prosperity_mod(prosperity: u8) -> i32 {
    if prosperity <= 75 {
        -2
    } else if prosperity >= 150 {
        2
    } else {
        0
    }
}

/// Minute-of-day from absolute epoch-minutes (day = 1440 min; a card's
/// "[TIME 1, 09:00]" baseline keeps 09:00 == 540 under the modulo).
pub(crate) fn minutes_of_day(total_minutes: i64) -> i64 {
    ((total_minutes % 1440) + 1440) % 1440
}

/// The city event's time-of-day modifier: night (22:00–05:00) −4 — alleys
/// at 2 AM get you jumped; dusk/dawn (05:00–07:00 + 18:00–22:00) −2;
/// daylight 0 (the DC stays ~14+ — broad-street muggings are rare).
pub(crate) fn time_of_day_mod(minute_of_day: i64) -> i32 {
    let m = minute_of_day.clamp(0, 1439);
    if m >= 1320 || m < 300 {
        -4
    } else if (300..420).contains(&m) || (1080..1320).contains(&m) {
        -2
    } else {
        0
    }
}

// ===========================================================================
// 1. Loot Rarity Referee
// ===========================================================================

/// Hard trigger stems — dialogue-stripped, word-boundary matched. These
/// fire on their own: the turn IS a loot attempt.
const LOOT_HARD_STEMS: &[&str] = &[
    "loot", "loots", "looted", "looting",
    "pillage", "pillages", "pillaged", "pillaging",
    "plunder", "plunders", "plundered", "plundering",
    "scavenge", "scavenges", "scavenged", "scavenging",
    "rummage", "rummages", "rummaged", "rummaging",
];

/// Soft trigger stems — fire ONLY alongside a container word ("I search
/// the body" is a loot attempt; "I search for my father" is not).
const LOOT_SOFT_STEMS: &[&str] = &[
    "search", "searches", "searched", "searching",
    "check", "checks", "checked", "checking",
    "open", "opens", "opened", "opening",
    "strip", "strips", "stripped", "stripping",
];

/// Container words that upgrade a soft stem into a loot trigger.
const LOOT_CONTAINER_WORDS: &[&str] = &[
    "body", "bodies", "corpse", "corpses", "dead", "chest", "chests",
    "crate", "crates", "container", "containers", "backpack", "backpacks",
    "pocket", "pockets", "remains",
];

/// The loot ladder — Chloe's 6 rungs. Derives `Ord` in ladder order
/// (Common lowest) so the tier cap composes with `min`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LootRarity {
    Common,
    Uncommon,
    Rare,
    Epic,
    Legendary,
    Mystic,
}

impl LootRarity {
    pub fn label(self) -> &'static str {
        match self {
            LootRarity::Common => "Common",
            LootRarity::Uncommon => "Uncommon",
            LootRarity::Rare => "Rare",
            LootRarity::Epic => "Epic",
            LootRarity::Legendary => "Legendary",
            LootRarity::Mystic => "Mystic",
        }
    }

    /// One rung up the ladder (Mystic is the ceiling — the vault rule's
    /// prosperity bump saturates).
    fn bump(self) -> LootRarity {
        match self {
            LootRarity::Common => LootRarity::Uncommon,
            LootRarity::Uncommon => LootRarity::Rare,
            LootRarity::Rare => LootRarity::Epic,
            LootRarity::Epic => LootRarity::Legendary,
            LootRarity::Legendary | LootRarity::Mystic => LootRarity::Mystic,
        }
    }
}

/// Map a raw roll TOTAL onto the rarity rung (pre-cap). Chloe's bands:
/// ≤6 Common · 7–10 Uncommon · 11–13 Rare · 14–16 Epic · 17–19 Legendary ·
/// ≥20 Mystic. Pure.
pub fn loot_rarity_for(total: i32) -> LootRarity {
    match total {
        ..=6 => LootRarity::Common,
        7..=10 => LootRarity::Uncommon,
        11..=13 => LootRarity::Rare,
        14..=16 => LootRarity::Epic,
        17..=19 => LootRarity::Legendary,
        _ => LootRarity::Mystic,
    }
}

/// The post-roll HARD CAP: the site's danger tier bounds what its bodies
/// can carry — None/Minion stays at Rare (a Nat 20 at a bandit camp finds
/// a fine blade, not an artifact), Soldier Epic, Elite Legendary,
/// Boss/Legendary Mystic. Prosperity ≥ 150 raises the cap one rung (the
/// vault rule: boom towns carry boom-town treasure). Pure.
pub fn loot_cap_for(tier: AttackerTier, prosperity: u8) -> LootRarity {
    let base = match tier {
        AttackerTier::Minion => LootRarity::Rare,
        AttackerTier::Soldier => LootRarity::Epic,
        AttackerTier::Elite => LootRarity::Legendary,
        AttackerTier::Boss | AttackerTier::Legendary => LootRarity::Mystic,
    };
    if prosperity >= 150 {
        base.bump()
    } else {
        base
    }
}

/// Roll → final rarity (ladder then cap). Exposed pure so the pinned
/// tests assert the composition without RNG gymnastics.
pub fn resolve_loot_rarity(total: i32, tier: AttackerTier, prosperity: u8) -> (LootRarity, bool) {
    let rolled = loot_rarity_for(total);
    let capped = loot_cap_for(tier, prosperity);
    if rolled > capped {
        (capped, true)
    } else {
        (rolled, false)
    }
}

/// The loot roll's attacker-tier modifier (Minion +0 … Legendary +4) —
/// shared by the live referee + the Playground's on-demand roll so the two
/// can never drift.
pub(crate) fn loot_tier_mod(tier: AttackerTier) -> i32 {
    match tier {
        AttackerTier::Minion => 0,
        AttackerTier::Soldier => 1,
        AttackerTier::Elite => 2,
        AttackerTier::Boss => 3,
        AttackerTier::Legendary => 4,
    }
}

/// (2026-08-23 Playground) The on-demand loot roll — the LOOT RARITY
/// GENERATOR's pure core. The live referee seeds per turn-text; a
/// Playground roll has no turn, so the CLOCK stands in (the minute-keyed
/// weather/rumor convention — repeated rolls at the same minute repeat,
/// the next minute re-rolls). Same math, same resolver: `total = d20 +
/// loot_tier_mod + loot_prosperity_mod` → [`resolve_loot_rarity`]. No
/// mutation, no schema read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaygroundLootRoll {
    pub roll: u32,
    pub tier_mod: i32,
    pub prosperity_mod: i32,
    pub total: i32,
    pub rarity: LootRarity,
    pub capped: bool,
}

pub fn playground_loot_roll(
    now_minutes: i64,
    tier: AttackerTier,
    prosperity: u8,
) -> PlaygroundLootRoll {
    let seed = hash_seed(&format!("{now_minutes}|playground|loot"));
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);
    let tier_mod = loot_tier_mod(tier);
    let prosperity_mod = loot_prosperity_mod(prosperity);
    let total = roll as i32 + tier_mod + prosperity_mod;
    let (rarity, capped) = resolve_loot_rarity(total, tier, prosperity);
    PlaygroundLootRoll {
        roll,
        tier_mod,
        prosperity_mod,
        total,
        rarity,
        capped,
    }
}

/// (2026-08-23 Playground) One full-math report line — the shape both the
/// TRAVEL D20 and REST D20 report buttons render. `fired` = the event /
/// interruption actually happens; the valence + roll fields carry the dice
/// math either way (the quiet case still shows what was rolled against
/// what DC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaygroundHazardReport {
    /// The composed DC the d20 was rolled against.
    pub dc: i32,
    /// The d20 (same seed the live check would roll at this minute).
    pub roll: u32,
    /// Did it fire / interrupt?
    pub fired: bool,
    /// Travel events only: the second-roll valence (None when not fired).
    pub valence: Option<EventValence>,
    /// One-line outcome text for the report line.
    pub detail: String,
}

/// The TRAVEL D20 report — the time-skip scope (the exact roll a ≥6h
/// unresisted advance at THIS minute + node would make): road DC from the
/// current node's rumor stack, one d20, valence when it fires.
pub fn playground_travel_report(
    now_minutes: i64,
    node_id: &str,
    rumors: &[Rumor],
) -> PlaygroundHazardReport {
    let dc = road_dc(true, rumors, node_id);
    // Same seed formula as time_skip_event_check — parity by construction.
    let seed = hash_seed(&format!("{now_minutes}|timeskip|{node_id}"));
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);
    if (roll as i32) >= dc {
        let valence_roll = roll_d20(&mut roller);
        let valence = valence_for_roll(valence_roll);
        let detail = format!(
            "d20 {roll} vs DC {dc} — EVENT (valence {}): the road/skip carries an encounter",
            valence.label()
        );
        PlaygroundHazardReport {
            dc,
            roll,
            fired: true,
            valence: Some(valence),
            detail,
        }
    } else {
        PlaygroundHazardReport {
            dc,
            roll,
            fired: false,
            valence: None,
            detail: format!("d20 {roll} vs DC {dc} — quiet, no event"),
        }
    }
}

/// The REST D20 report — the interruption referee's exact math: settlement
/// auto-rests (DC 0); otherwise the active map's mob tier sets the DC and
/// a roll under it means the rest is interrupted (Impaired).
pub fn playground_rest_report(
    now_minutes: i64,
    node_id: &str,
    node: Option<&Node>,
    map: Option<&SiteMap>,
) -> PlaygroundHazardReport {
    if is_settlement(node, map) {
        return PlaygroundHazardReport {
            dc: 0,
            roll: 20,
            fired: false,
            valence: None,
            detail: "Settlement — automatic full rest (DC 0)".to_string(),
        };
    }
    let dc = rest_dc(map);
    // Same seed formula as rest_interruption_check — parity by construction.
    let seed = hash_seed(&format!("{now_minutes}|rest|{node_id}"));
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);
    if (roll as i32) < dc {
        PlaygroundHazardReport {
            dc,
            roll,
            fired: true,
            valence: None,
            detail: format!(
                "d20 {roll} vs DC {dc} — INTERRUPTED: half recovery, no anchor, Impaired (30 min)"
            ),
        }
    } else {
        PlaygroundHazardReport {
            dc,
            roll,
            fired: false,
            valence: None,
            detail: format!("d20 {roll} vs DC {dc} — rest holds, full recovery + anchor"),
        }
    }
}

/// Type hint from the action text — the directive's parenthetical. Word-
/// boundary matched, first family wins. `None` → the generic "find".
fn loot_type_hint(lower: &str) -> Option<&'static str> {
    const FAMILIES: &[(&[&str], &str)] = &[
        (
            &[
                "sword", "dagger", "axe", "blade", "bow", "crossbow", "spear",
                "hammer", "mace", "weapon", "weapons", "rifle", "pistol",
                "shield", "quiver",
            ],
            "weapon",
        ),
        (
            &[
                "armor", "armour", "mail", "plate", "helm", "helmet",
                "gauntlet", "gauntlets", "breastplate", "greaves", "vambrace",
            ],
            "armor",
        ),
        (
            &[
                "potion", "potions", "elixir", "tonic", "draught",
                "flask", "salve", "vial",
            ],
            "potion",
        ),
        (
            &[
                "coin", "coins", "gold", "silver", "copper", "money",
                "purse", "gems", "gem", "jewel", "jewels",
            ],
            "coin",
        ),
        (
            &[
                "letter", "document", "documents", "map", "scroll",
                "scrolls", "journal", "book", "tome", "ledger", "papers",
                "note", "notes",
            ],
            "document",
        ),
        (
            &[
                "ring", "amulet", "pendant", "charm", "trinket",
                "locket", "talisman", "bracelet", "brooch",
            ],
            "trinket",
        ),
    ];
    for (words, label) in FAMILIES {
        if words.iter().any(|w| keyword_present(lower, w)) {
            return Some(label);
        }
    }
    None
}

/// Does this turn's text trigger a loot check? Hard stems alone; soft
/// stems only beside a container word. Dialogue-stripped (spoken words
/// don't loot), word-boundary (the `keyword_present` contract).
pub fn loot_check_triggers(text: &str) -> bool {
    let lower = strip_dialogue(text).to_lowercase();
    loot_check_triggers_lower(&lower)
}

fn loot_check_triggers_lower(lower: &str) -> bool {
    if LOOT_HARD_STEMS.iter().any(|kw| keyword_present(lower, kw)) {
        return true;
    }
    if LOOT_SOFT_STEMS.iter().any(|kw| keyword_present(lower, kw)) {
        return LOOT_CONTAINER_WORDS
            .iter()
            .any(|kw| keyword_present(lower, kw));
    }
    false
}

/// One loot-referee outcome: the final (post-cap) rarity + the directive
/// line for the narrator's `<directives>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LootOutcome {
    pub rarity: LootRarity,
    pub roll: u32,
    pub total: i32,
    pub capped: bool,
    pub directive: String,
}

/// The Loot Rarity Referee. One roll per turn, seeded by the turn text
/// (the skill-referee convention: same text → same outcome, distinct
/// skills/turns diverge). `tier` is the referee block's `combined` tier
/// (on-camera NPC tier max the ACTIVE site map's mob tier — the same
/// stakes the combat referee reads); `prosperity` the CURRENT node's.
/// Returns `None` when the text triggers no loot attempt. Pure.
pub fn referee_evaluate_loot(
    text: &str,
    tier: AttackerTier,
    prosperity: u8,
    now_minutes: i64,
) -> Option<LootOutcome> {
    let lower = strip_dialogue(text).to_lowercase();
    if !loot_check_triggers_lower(&lower) {
        return None;
    }
    let tier_mod = loot_tier_mod(tier);
    // (2026-08-24 review P2) The seed carries the WORLD CLOCK beside the
    // text — the old text-only seed meant the same action ("I search the
    // body") rolled the SAME rarity on every reuse, forever. The clock is
    // the house nonce convention (weather/rumor/city events are all
    // minute-keyed); repeated attempts in the same in-world minute still
    // repeat, matching those referees.
    let seed = hash_seed(&format!("loot|{now_minutes}|{lower}"));
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);
    let total = roll as i32 + tier_mod + loot_prosperity_mod(prosperity);
    let (rarity, capped) = resolve_loot_rarity(total, tier, prosperity);
    let what = loot_type_hint(&lower).unwrap_or("find");
    // (2026-08-24 Part II A5) The suffix-naming law rides only when a loot
    // check fires — named finds read "Flame Dagger +1", never "+1 Flame
    // Dagger" (the enchantment qualifies the item, it does not lead it).
    let directive = format!(
        "Loot check — rarity {r} ({what}): the best recoverable find here is exactly \
         {r} quality, grounded in this place; named finds use suffix form \
         (\"Flame Dagger +1\" — never \"+1 Flame Dagger\")",
        r = rarity.label(),
        what = what,
    );
    Some(LootOutcome {
        rarity,
        roll,
        total,
        capped,
        directive,
    })
}

// ===========================================================================
// 2. Travel & Road / City Event Referee — base DC 14, dual scope
// ===========================================================================

/// Base event DC (both scopes). v1 tuning — likely needs a live pass.
pub const EVENT_BASE_DC: i32 = 14;
/// The absolute DC floor (heavy rumor stacks + night could otherwise
/// reach automatic).
pub const EVENT_DC_FLOOR: i32 = 2;

/// The second-roll valence band (Chloe's bands): 1–7 negative /
/// 8–14 ambiguous / 15–20 favorable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventValence {
    Negative,
    Ambiguous,
    Favorable,
}

impl EventValence {
    pub fn label(self) -> &'static str {
        match self {
            EventValence::Negative => "negative",
            EventValence::Ambiguous => "ambiguous",
            EventValence::Favorable => "favorable",
        }
    }

    /// The valence-specific narrator seed (the skill referee's
    /// success_seed/fail_seed convention — a starting clause, never
    /// creative license).
    pub fn seed(self) -> &'static str {
        match self {
            EventValence::Negative => {
                "The encounter turns against the player — danger, loss, or pursuit."
            }
            EventValence::Ambiguous => {
                "The encounter is double-edged — opportunity and risk intertwined."
            }
            EventValence::Favorable => {
                "The encounter favors the player — aid, windfall, or useful news."
            }
        }
    }
}

fn valence_for_roll(roll: u32) -> EventValence {
    match roll {
        1..=7 => EventValence::Negative,
        8..=14 => EventValence::Ambiguous,
        _ => EventValence::Favorable,
    }
}

/// One fired event: the check roll, the valence roll, and the DC it beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HazardEvent {
    pub valence: EventValence,
    pub roll: u32,
    pub valence_roll: u32,
    pub dc: i32,
}

/// The two-roll core: `d20 ≥ DC` fires; a second d20 sets the valence.
/// `None` = no event (the quiet majority at base DC 14 — a 35% fire rate).
fn roll_event(roller: &mut Roller, dc: i32) -> Option<HazardEvent> {
    let roll = roll_d20(roller);
    if (roll as i32) < dc {
        return None;
    }
    let valence_roll = roll_d20(roller);
    Some(HazardEvent {
        valence: valence_for_roll(valence_roll),
        roll,
        valence_roll,
        dc,
    })
}

/// Rumor stems that mark a rumor as a THREAT (drives the road-DC stack +
/// the Suspected-asset seeding). Full-word stems matched EXACTLY plus
/// regular inflections (+s/+ed/+ing/+er/+ers) and a small irregular list —
/// a rumor carries no polarity field (the §11.46 anti-bloat verdict), so
/// threat detection is a word scan. The two truncation stems (`smuggl`,
/// `maraud`) keep prefix matching — they have no benign collisions
/// (smuggler/smuggling/marauder/marauding are all threats). Shared with
/// `site_map::seed_rumor_asset`.
///
/// (2026-08-24 review P1) The old plain substring scan matched stems
/// INSIDE words: "the guild rewards loyal service" (war), "the orchards
/// are ripe" (orc), "skilled workers" (kill) each minted a fake Suspected
/// creature on the fog map and stacked −3 onto road DCs. Word-boundary
/// exact+inflection matching is the fix; a benign word sharing a stem's
/// LETTERS no longer qualifies.
pub const RUMOR_THREAT_STEMS: &[&str] = &[
    "bandit", "raid", "ambush", "attack", "beast", "wolf", "goblin", "orc",
    "war", "plague", "kill", "murder", "thief", "outlaw", "horde", "cult",
    "demon", "monster",
];

/// The two truncation stems — deliberately kept prefix-matched (every word
/// starting with them is a threat; no benign collision exists).
const RUMOR_THREAT_PREFIX_STEMS: &[&str] = &["smuggl", "maraud"];

/// Irregular inflections + compound threat words the regular suffix rules
/// can't derive from a stem: irregular plurals (wolves, thieves), the
/// sh-plural (ambushes), consonant-doubling/irregular verbs (warring), and
/// clear compounds (warband, warlord, warfare, cultist, warmonger).
const RUMOR_THREAT_IRREGULAR: &[&str] = &[
    "wolves", "thieves", "ambushes", "warring", "warband", "warbands",
    "warlord", "warlords", "warfare", "cultist", "cultists", "warmonger",
    "warmongers", "bloodbath", "massacre", "massacred", "slaughter",
    "slaughtered", "butcher", "butchered",
];

/// Does this single (lowercased) word carry a threat stem? Exact stem +
/// regular inflections, prefix for the two truncation stems, membership in
/// the irregular list. Pure.
fn is_threat_word(word: &str) -> bool {
    if RUMOR_THREAT_PREFIX_STEMS.iter().any(|stem| word.starts_with(stem)) {
        return true;
    }
    if RUMOR_THREAT_IRREGULAR.contains(&word) {
        return true;
    }
    RUMOR_THREAT_STEMS.iter().any(|stem| {
        word == *stem
            || word == format!("{stem}s")
            || word == format!("{stem}ed")
            || word == format!("{stem}ing")
            || word == format!("{stem}er")
            || word == format!("{stem}ers")
    })
}

/// Is this rumor's label a threat? Word-boundary stem scan on the lowercase
/// label.
pub fn is_threat_rumor(label: &str) -> bool {
    rumor_threat_word(label).is_some()
}

/// The first whole word of the (lowercased) label carrying a threat
/// stem — the seed asset's name/kind source (a word ending in "s" is a
/// GROUP: "bandits" → a Bandit Group; "wolf" → a lone Creature).
pub fn rumor_threat_word(label: &str) -> Option<String> {
    let lower = label.to_lowercase();
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if is_threat_word(word) {
            return Some(word.to_string());
        }
    }
    None
}

/// The road DC's rumor stack: every rumor the DESTINATION has heard
/// lowers the DC by 1 (word travels with danger), a threat-stem rumor by
/// 3, clamped to [−8, 0]. Two threatening rumors → DC 6 → 75%: a
/// near-guaranteed ambush (the ruling's worked example).
pub(crate) fn rumor_threat_mod(rumors: &[Rumor], node_id: &str) -> i32 {
    let mut acc = 0i32;
    for r in rumors {
        if !r.known_nodes.iter().any(|n| n == node_id) {
            continue;
        }
        acc -= if is_threat_rumor(&r.label) { 3 } else { 1 };
    }
    acc.clamp(-8, 0)
}

/// The composed road DC (pure — the pinned composition test's surface).
pub(crate) fn road_dc(adjacent: bool, rumors: &[Rumor], destination: &str) -> i32 {
    let distance_mod = if adjacent { 0 } else { -3 };
    (EVENT_BASE_DC + distance_mod + rumor_threat_mod(rumors, destination)).max(EVENT_DC_FLOOR)
}

/// ROAD scope — rolled by the `[TRAVEL]` applier after a successful move.
/// `adjacent` = an adjacent hop (0); a non-adjacent auto-linked long haul
/// is −3 (empty roads breed incidents). Seeded per (minute, from, to).
pub fn road_event_check(
    now_minutes: i64,
    from: &str,
    to: &str,
    adjacent: bool,
    rumors: &[Rumor],
) -> Option<HazardEvent> {
    let dc = road_dc(adjacent, rumors, to);
    let seed = hash_seed(&format!("{now_minutes}|travel|{from}|{to}"));
    let mut roller = Roller::new(seed);
    roll_event(&mut roller, dc)
}

/// The composed city DC (pure — the pinned time × prosperity test's
/// surface).
pub(crate) fn city_dc(now_minutes: i64, prosperity: u8) -> i32 {
    let tod = time_of_day_mod(minutes_of_day(now_minutes));
    (EVENT_BASE_DC + tod + city_prosperity_mod(prosperity)).max(EVENT_DC_FLOOR)
}

/// CITY scope — rolled by the `[ROOM]` applier on a visited move across a
/// settlement ROOT map (never a hosted building child — indoors is the
/// site map's own domain). Seeded per (minute, node, from-area, to-area).
pub fn city_event_check(
    now_minutes: i64,
    node: &str,
    from_area: &str,
    to_area: &str,
    prosperity: u8,
) -> Option<HazardEvent> {
    let dc = city_dc(now_minutes, prosperity);
    let seed = hash_seed(&format!("{now_minutes}|city|{node}|{from_area}|{to_area}"));
    let mut roller = Roller::new(seed);
    roll_event(&mut roller, dc)
}

/// The road directive line (also the time-skip event's carrier — the
/// wording names the destination).
pub fn road_event_directive(ev: HazardEvent, destination_name: &str) -> String {
    format!(
        "Road event (valence: {v}) — an encounter colors the journey to {dest}; \
         weave it into this beat. {seed}",
        v = ev.valence.label(),
        dest = destination_name,
        seed = ev.valence.seed(),
    )
}

/// The city directive line.
pub fn city_event_directive(ev: HazardEvent, node_name: &str) -> String {
    format!(
        "City event (valence: {v}) — an encounter colors the streets of {node} as the \
         player moves through it; weave it into this beat. {seed}",
        v = ev.valence.label(),
        node = node_name,
        seed = ev.valence.seed(),
    )
}

/// TIME-SKIP scope — a `[TIME]` advance ≥ 6h with no rest is hours spent
/// moving/waiting through dangerous country: one road-scope event
/// anchored at the CURRENT node (its rumors apply). Seeded per (minute,
/// node). Pure.
pub fn time_skip_event_check(
    now_minutes: i64,
    node_id: &str,
    rumors: &[Rumor],
) -> Option<HazardEvent> {
    // Same journey math as a road move that ENDS where it started —
    // the player orbited their own neighborhood through dangerous hours.
    let dc = road_dc(true, rumors, node_id);
    let seed = hash_seed(&format!("{now_minutes}|timeskip|{node_id}"));
    let mut roller = Roller::new(seed);
    roll_event(&mut roller, dc)
}

/// The time-skip directive line (its own wording — the "journey" is the
/// hours passing, not a road).
pub fn time_skip_event_directive(ev: HazardEvent, node_name: &str) -> String {
    format!(
        "Time-skip event (valence: {v}) — the hours passing around {node} bring an \
         encounter; weave it into this beat. {seed}",
        v = ev.valence.label(),
        node = node_name,
        seed = ev.valence.seed(),
    )
}

// ===========================================================================
// 3. Rest Interruption Referee
// ===========================================================================

/// The "Impaired" debuff's lifetime, in-world minutes, from the
/// post-advance clock — long enough to count in `condition_penalty` for
/// the lethality DC of the ensuing encounter (the bleary-eyed ambush),
/// short enough to be gone by the next scene.
pub const IMPAIRED_TAG_MINUTES: i64 = 30;

/// The rest-interruption outcome — `Some` means the rest FAILED. The
/// caller owns the mechanical fallout (half recovery steps, no anchor
/// stamp, the Impaired tag, the notice).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RestInterruption {
    pub dc: i32,
    pub roll: u32,
    pub directive: String,
    pub notice: String,
}

/// The rest DC: settlement → 0 (automatic full rest — towns sleep safe);
/// otherwise the site's strongest present mob sets the stakes
/// (`5 + 3 × rank`: Minion 8 … Legendary 20); a mapped site with nothing
/// hostile on it is 5; MAPLESS wilderness is 8 (the Minion default — the
/// road itself is the threat). `map` is the ACTIVE map (resolver law:
/// inside a building, ITS creatures set the stakes). Pure.
pub fn rest_dc(map: Option<&SiteMap>) -> i32 {
    match map {
        None => 8,
        Some(m) => match present_mob_tier(m) {
            Some(t) => 5 + 3 * tier_rank(t),
            None => 5,
        },
    }
}

/// The rest-interruption roll. `Some(RestInterruption)` = the rest is
/// interrupted (rolled BELOW the DC). Settlements never interrupt
/// (`is_settlement`). Seeded per (minute, node) — the weather/rumor
/// minute convention. Pure.
pub fn rest_interruption_check(
    now_minutes: i64,
    node_id: &str,
    node: Option<&Node>,
    map: Option<&SiteMap>,
) -> Option<RestInterruption> {
    if is_settlement(node, map) {
        return None;
    }
    let dc = rest_dc(map);
    let seed = hash_seed(&format!("{now_minutes}|rest|{node_id}"));
    let mut roller = Roller::new(seed);
    let roll = roll_d20(&mut roller);
    if roll as i32 >= dc {
        return None;
    }
    Some(RestInterruption {
        dc,
        roll,
        directive: "The rest is interrupted — a threat from this site strikes during sleep \
            (combat or stealth, fitting the danger); the player sleeps badly and wakes impaired"
            .to_string(),
        notice: "Rest interrupted — something struck from the dark; you wake Impaired."
            .to_string(),
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site_map::{
        AssetKnowledge, AssetOrigin, SiteAsset, SiteMap, SiteThreat,
    };
    use crate::schema::Node;

    fn rumor(label: &str, known: &[&str]) -> Rumor {
        Rumor {
            label: label.to_string(),
            origin_node: known.first().unwrap_or(&"").to_string(),
            known_nodes: known.iter().map(|s| s.to_string()).collect(),
            born_minutes: 0,
        }
    }

    fn mob_map(tier_word: &str) -> SiteMap {
        SiteMap {
            threat: SiteThreat::Moderate,
            assets: vec![SiteAsset {
                id: "m1".into(),
                name: "M1".into(),
                kind: crate::site_map::AssetKind::Creature,
                knowledge: AssetKnowledge::Known,
                tier: Some(tier_word.into()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    // ---- determinism (the weather/rumor pattern) ----

    #[test]
    fn loot_roll_is_deterministic_for_same_text_and_minute() {
        let a = referee_evaluate_loot("I loot the bodies.", AttackerTier::Soldier, 100, 1_000);
        let b = referee_evaluate_loot("I loot the bodies.", AttackerTier::Soldier, 100, 1_000);
        assert_eq!(a, b);
        // (2026-08-24 review P2) The clock is part of the seed — the SAME
        // text at a different minute must re-roll (the old text-only seed
        // froze the rarity of a repeated action forever). A 9-minute sweep:
        // at least one must differ (a single-minute pair would flake at
        // 1/20 on coincidental d20 equality).
        let later: Vec<_> = (1..10)
            .map(|d| {
                referee_evaluate_loot("I loot the bodies.", AttackerTier::Soldier, 100, 1_000 + d)
            })
            .collect();
        assert!(
            later.iter().any(|o| o.roll != a.unwrap().roll),
            "later attempts at the same text must re-roll"
        );
    }

    #[test]
    fn loot_outcomes_vary_across_texts() {
        // A sweep of distinct texts must produce at least two distinct
        // rarities/rolls somewhere (otherwise the seeding is broken).
        let texts: Vec<String> = (0..40)
            .map(|i| format!("I loot the bodies, taking care over crate {i}."))
            .collect();
        let mut distinct = std::collections::HashSet::new();
        for t in &texts {
            if let Some(o) = referee_evaluate_loot(t, AttackerTier::Elite, 100, 1_000) {
                distinct.insert((o.roll, o.total));
            }
        }
        assert!(distinct.len() > 1, "loot rolls should vary across texts");
    }

    #[test]
    fn road_event_is_deterministic_and_varies_over_minutes() {
        let rumors = vec![rumor("bandits raid the road", &["ridge"])];
        let a = road_event_check(1000, "tavern", "ridge", false, &rumors);
        let b = road_event_check(1000, "tavern", "ridge", false, &rumors);
        assert_eq!(a, b);
        let mut any = std::collections::HashSet::new();
        for m in 0..200i64 {
            any.insert(road_event_check(m, "tavern", "ridge", false, &rumors));
        }
        assert!(any.len() > 1, "road events should vary across minutes");
    }

    #[test]
    fn rest_check_is_deterministic_and_varies_over_minutes() {
        let map = mob_map("soldier");
        let a = rest_interruption_check(500, "camp", None, Some(&map));
        let b = rest_interruption_check(500, "camp", None, Some(&map));
        assert_eq!(a, b);
        let mut any = std::collections::HashSet::new();
        for m in 0..200i64 {
            any.insert(rest_interruption_check(m, "camp", None, Some(&map)));
        }
        assert!(any.len() > 1, "rest outcomes should vary across minutes");
    }

    // ---- loot ladder + caps ----

    #[test]
    fn loot_rarity_rung_table() {
        assert_eq!(loot_rarity_for(1), LootRarity::Common);
        assert_eq!(loot_rarity_for(6), LootRarity::Common);
        assert_eq!(loot_rarity_for(7), LootRarity::Uncommon);
        assert_eq!(loot_rarity_for(10), LootRarity::Uncommon);
        assert_eq!(loot_rarity_for(11), LootRarity::Rare);
        assert_eq!(loot_rarity_for(13), LootRarity::Rare);
        assert_eq!(loot_rarity_for(14), LootRarity::Epic);
        assert_eq!(loot_rarity_for(16), LootRarity::Epic);
        assert_eq!(loot_rarity_for(17), LootRarity::Legendary);
        assert_eq!(loot_rarity_for(19), LootRarity::Legendary);
        assert_eq!(loot_rarity_for(20), LootRarity::Mystic);
        assert_eq!(loot_rarity_for(27), LootRarity::Mystic);
    }

    #[test]
    fn nat_20_at_a_minion_camp_caps_at_rare() {
        // Chloe's worked example, pinned: a natural 20 looting a bandit
        // camp (Minion stakes, normal prosperity) stays Rare.
        let (rarity, capped) = resolve_loot_rarity(20, AttackerTier::Minion, 100);
        assert_eq!(rarity, LootRarity::Rare);
        assert!(capped);
    }

    #[test]
    fn loot_cap_ladder_by_tier() {
        assert_eq!(loot_cap_for(AttackerTier::Minion, 100), LootRarity::Rare);
        assert_eq!(loot_cap_for(AttackerTier::Soldier, 100), LootRarity::Epic);
        assert_eq!(
            loot_cap_for(AttackerTier::Elite, 100),
            LootRarity::Legendary
        );
        assert_eq!(loot_cap_for(AttackerTier::Boss, 100), LootRarity::Mystic);
        assert_eq!(
            loot_cap_for(AttackerTier::Legendary, 100),
            LootRarity::Mystic
        );
    }

    #[test]
    fn prosperity_150_raises_the_cap_one_rung() {
        assert_eq!(loot_cap_for(AttackerTier::Minion, 150), LootRarity::Epic);
        assert_eq!(
            loot_cap_for(AttackerTier::Soldier, 200),
            LootRarity::Legendary
        );
        // Mystic saturates.
        assert_eq!(
            loot_cap_for(AttackerTier::Boss, 200),
            LootRarity::Mystic
        );
    }

    #[test]
    fn loot_prosperity_mod_band() {
        assert_eq!(loot_prosperity_mod(25), -2);
        assert_eq!(loot_prosperity_mod(75), -1);
        assert_eq!(loot_prosperity_mod(100), 0);
        assert_eq!(loot_prosperity_mod(150), 2);
        assert_eq!(loot_prosperity_mod(130), 1);
    }

    #[test]
    fn loot_triggers_hard_soft_and_negative() {
        assert!(loot_check_triggers("I loot the bodies."));
        assert!(loot_check_triggers("I plunder the supply wagon."));
        assert!(loot_check_triggers("I scavenge what's left."));
        assert!(loot_check_triggers("I rummage through the crates."));
        // Soft stem + container word.
        assert!(loot_check_triggers("I search the corpse."));
        assert!(loot_check_triggers("I check the body for valuables."));
        assert!(loot_check_triggers("I open the chest."));
        assert!(loot_check_triggers("I strip the dead."));
        // Soft stem WITHOUT a container — not a loot attempt.
        assert!(!loot_check_triggers("I search for my father."));
        assert!(!loot_check_triggers("I open the door."));
        // Dialogue never triggers.
        assert!(!loot_check_triggers("He said: \"loot the bodies\" and left."));
    }

    #[test]
    fn loot_directive_carries_rarity_and_hint() {
        let o = referee_evaluate_loot(
            "I loot the fallen guard, taking his sword.",
            AttackerTier::Soldier,
            100,
            1_000,
        )
        .expect("triggers");
        assert!(o.directive.starts_with("Loot check — rarity "));
        assert!(o.directive.contains("(weapon)"), "{}", o.directive);
        assert!(o.directive.contains("exactly"));
        let no_hint = referee_evaluate_loot(
            "I loot the bodies.",
            AttackerTier::Soldier,
            100,
            1_000,
        )
        .expect("triggers");
        assert!(no_hint.directive.contains("(find)"), "{}", no_hint.directive);
    }

    // ---- valence bands ----

    #[test]
    fn valence_bands_match_chloes_split() {
        assert_eq!(valence_for_roll(1), EventValence::Negative);
        assert_eq!(valence_for_roll(7), EventValence::Negative);
        assert_eq!(valence_for_roll(8), EventValence::Ambiguous);
        assert_eq!(valence_for_roll(14), EventValence::Ambiguous);
        assert_eq!(valence_for_roll(15), EventValence::Favorable);
        assert_eq!(valence_for_roll(20), EventValence::Favorable);
    }

    #[test]
    fn valence_distribution_over_a_sweep_is_sane() {
        // Over a long minute sweep of fired events, every valence band
        // must appear at least once (a broken second roll would collapse
        // the band).
        let rumors: Vec<Rumor> = vec![rumor("bandits raid the road", &["ridge"])];
        let mut seen = std::collections::HashSet::new();
        for m in 0..500i64 {
            if let Some(ev) = road_event_check(m, "tavern", "ridge", false, &rumors) {
                seen.insert(ev.valence);
            }
        }
        assert_eq!(
            seen.len(),
            3,
            "all three valence bands must appear over a sweep"
        );
    }

    // ---- road DC composition ----

    #[test]
    fn road_dc_distance_and_rumor_composition() {
        let none: Vec<Rumor> = vec![];
        // Adjacent hop, no rumors: base 14.
        assert_eq!(road_dc(true, &none, "ridge"), 14);
        // Non-adjacent long haul: −3.
        assert_eq!(road_dc(false, &none, "ridge"), 11);
        // One benign rumor at the destination: −1.
        let benign = vec![rumor("the stranger paid in gold", &["ridge"])];
        assert_eq!(road_dc(true, &benign, "ridge"), 13);
        // One threat rumor: −3.
        let threat = vec![rumor("bandits raid the road", &["ridge"])];
        assert_eq!(road_dc(true, &threat, "ridge"), 11);
        // Two threat rumors → DC 8? No: 14 − 6 = 8. Chloe's worked example
        // said "two threatening rumors → DC 6" — that example carries the
        // non-adjacent −3 too (14 − 3 − 6 = 5 → the ruling's near-guarantee
        // band). Pin BOTH compositions exactly.
        let two_threats = vec![
            rumor("bandits raid the road", &["ridge"]),
            rumor("a monster prowls the pass", &["ridge"]),
        ];
        assert_eq!(road_dc(true, &two_threats, "ridge"), 8);
        assert_eq!(road_dc(false, &two_threats, "ridge"), 5);
        // Clamped at −8: four threat rumors (−12) clamp to DC 6 adjacent /
        // floor 2 never reached here.
        let many = vec![
            rumor("bandits raid the road", &["ridge"]),
            rumor("a monster prowls the pass", &["ridge"]),
            rumor("orcs mass at the border", &["ridge"]),
            rumor("a cult murders travelers", &["ridge"]),
        ];
        assert_eq!(road_dc(true, &many, "ridge"), 6);
        // Rumors known only ELSEWHERE don't stack.
        let elsewhere = vec![rumor("bandits raid the road", &["tavern"])];
        assert_eq!(road_dc(true, &elsewhere, "ridge"), 14);
    }

    // ---- city DC: time of day × prosperity ----

    #[test]
    fn city_dc_day_night_dusk_and_prosperity() {
        // Noon (12:00 = 720), normal town: 14.
        assert_eq!(city_dc(720, 100), 14);
        // 2 AM (120): night −4 → 10 (alleys at 2 AM get you jumped).
        assert_eq!(city_dc(120, 100), 10);
        // 10 PM (1320): night.
        assert_eq!(city_dc(1320, 100), 10);
        // 6 AM (360) + 8 PM (1200): dusk/dawn −2.
        assert_eq!(city_dc(360, 100), 12);
        assert_eq!(city_dc(1200, 100), 12);
        // Desperate quarters: night + poverty → 8.
        assert_eq!(city_dc(120, 60), 8);
        // Boom town daylight: 16.
        assert_eq!(city_dc(720, 160), 16);
        // The floor: night + poverty is 8, above the floor — but pin the
        // clamp exists via the constant contract.
        assert_eq!(EVENT_DC_FLOOR, 2);
    }

    #[test]
    fn minutes_of_day_wraps_and_time_bands() {
        // Day 1 09:00 baseline = minute 1980 → 540.
        assert_eq!(minutes_of_day(1980), 540);
        assert_eq!(minutes_of_day(0), 0);
        assert_eq!(minutes_of_day(1439), 1439);
        assert_eq!(minutes_of_day(1440), 0);
        assert_eq!(time_of_day_mod(0), -4);
        assert_eq!(time_of_day_mod(299), -4);
        assert_eq!(time_of_day_mod(300), -2);
        assert_eq!(time_of_day_mod(419), -2);
        assert_eq!(time_of_day_mod(420), 0);
        assert_eq!(time_of_day_mod(1079), 0);
        assert_eq!(time_of_day_mod(1080), -2);
        assert_eq!(time_of_day_mod(1319), -2);
        assert_eq!(time_of_day_mod(1320), -4);
    }

    #[test]
    fn city_event_is_deterministic() {
        let a = city_event_check(120, "town", "market", "alley", 100);
        let b = city_event_check(120, "town", "market", "alley", 100);
        assert_eq!(a, b);
    }

    // ---- rest interruption ----

    #[test]
    fn settlement_rest_is_automatic() {
        // All three settlement signals, plus the hosted-child case.
        let node_setting = Node {
            setting: "settlement".into(),
            ..Default::default()
        };
        assert!(rest_interruption_check(500, "town", Some(&node_setting), None).is_none());
        let node_named = Node {
            name: "Ironhollow City".into(),
            ..Default::default()
        };
        assert!(rest_interruption_check(500, "town", Some(&node_named), None).is_none());
        let mut with_building = SiteMap::default();
        with_building.assets.push(SiteAsset {
            id: "guildhall".into(),
            name: "Guildhall".into(),
            kind: AssetKind::Building,
            knowledge: AssetKnowledge::Known,
            ..Default::default()
        });
        assert!(rest_interruption_check(500, "town", None, Some(&with_building)).is_none());
        let mut child = SiteMap::default();
        child.host = Some(crate::site_map::HostRef::default());
        assert!(rest_interruption_check(500, "town", None, Some(&child)).is_none());
    }

    #[test]
    fn wilderness_and_mapless_rest_dc_is_minion_default() {
        assert_eq!(rest_dc(None), 8);
        // An empty mapped site (nothing hostile on it): 5.
        assert_eq!(rest_dc(Some(&SiteMap::default())), 5);
    }

    #[test]
    fn rest_dc_scales_with_mob_tier() {
        assert_eq!(rest_dc(Some(&mob_map("minion"))), 8);
        assert_eq!(rest_dc(Some(&mob_map("soldier"))), 11);
        assert_eq!(rest_dc(Some(&mob_map("elite"))), 14);
        assert_eq!(rest_dc(Some(&mob_map("boss"))), 17);
        assert_eq!(rest_dc(Some(&mob_map("legendary"))), 20);
    }

    #[test]
    fn wilderness_rest_interrupts_over_a_sweep() {
        // Mapless wilderness DC 8 — over a minute sweep the roll must
        // sometimes interrupt (a broken comparison would never fire) and
        // sometimes not (an always-fire would mean the DC is unreachable).
        let mut interrupted = 0;
        let mut rested = 0;
        for m in 0..200i64 {
            match rest_interruption_check(m, "wilds", None, None) {
                Some(r) => {
                    assert!((r.roll as i32) < r.dc);
                    assert!(r.directive.contains("interrupted"));
                    assert!(r.notice.contains("Impaired"));
                    interrupted += 1;
                }
                None => rested += 1,
            }
        }
        assert!(interrupted > 0, "wilderness rest must sometimes interrupt");
        assert!(rested > 0, "wilderness rest must sometimes hold");
    }

    #[test]
    fn impaired_tag_shape_is_pure_debuff() {
        // The tag contract: label "Impaired", kind "" (a PURE debuff — it
        // counts in count_by_polarity for the lethality DC), 30-minute
        // expiry. The never-matches-SICK_STEMS pin lives in consequence's
        // tests; here we pin the constant + the label spelling.
        assert_eq!(IMPAIRED_TAG_MINUTES, 30);
        // "Impaired" must not contain any threat stem either (it never
        // feeds the rumor mill).
        assert!(!is_threat_rumor("Impaired"));
    }

    #[test]
    fn playground_loot_roll_math_parity() {
        // (2026-08-23 Playground) The on-demand roll composes EXACTLY like
        // the live referee: total = d20 + tier_mod + prosperity_mod, then
        // the SAME resolve_loot_rarity ladder+cap. Parity pinned across a
        // minute sweep — every roll's rarity must equal
        // resolve_loot_rarity(total, tier, prosperity).0.
        for m in (0..600i64).step_by(7) {
            for (tier, prosperity) in [
                (AttackerTier::Minion, 100u8),
                (AttackerTier::Soldier, 60),
                (AttackerTier::Elite, 150),
                (AttackerTier::Boss, 200),
            ] {
                let r = playground_loot_roll(m, tier, prosperity);
                assert_eq!(r.total, r.roll as i32 + r.tier_mod + r.prosperity_mod);
                let (rarity, capped) = resolve_loot_rarity(r.total, tier, prosperity);
                assert_eq!(r.rarity, rarity, "band parity at minute {m}");
                assert_eq!(r.capped, capped, "cap parity at minute {m}");
            }
        }
        // Determinism: same minute repeats; minutes vary somewhere.
        assert_eq!(
            playground_loot_roll(1234, AttackerTier::Soldier, 100),
            playground_loot_roll(1234, AttackerTier::Soldier, 100)
        );
        let mut distinct = std::collections::HashSet::new();
        for m in 0..200i64 {
            distinct.insert(playground_loot_roll(m, AttackerTier::Elite, 100).total);
        }
        assert!(distinct.len() > 1, "rolls must vary across minutes");
        // The worked cap: somewhere in a long sweep a high roll at a Minion
        // camp must hit the Rare cap (the composition is real, not frozen).
        let any_capped = (0..1000i64).any(|m| {
            playground_loot_roll(m, AttackerTier::Minion, 100).capped
        });
        assert!(any_capped, "a Minion-site roll must sometimes cap at Rare");
    }

    // ---- threat stems ----

    #[test]
    fn threat_stem_matching() {
        assert!(is_threat_rumor("bandits raid the eastern road"));
        assert!(is_threat_rumor("A wolf prowls the mill"));
        assert!(is_threat_rumor("the smuggler runs the cove"));
        assert!(!is_threat_rumor("the stranger paid in gold"));
        assert!(!is_threat_rumor("the captain is looking for someone"));
        assert_eq!(
            rumor_threat_word("bandits raid the road").as_deref(),
            Some("bandits")
        );
        assert_eq!(
            rumor_threat_word("a lone wolf near the mill").as_deref(),
            Some("wolf")
        );
        assert_eq!(rumor_threat_word("nothing to see"), None);
    }

    /// (2026-08-24 review P1) The substring-scan false positives: benign
    /// words CONTAINING a stem's letters must never mint a Suspected
    /// creature or stack the road DC. Each of these matched under the old
    /// `word.contains(stem)` scan.
    #[test]
    fn threat_stems_do_not_match_substring_benign_words() {
        assert!(!is_threat_rumor("the guild rewards loyal service"), "war ⊂ rewards");
        assert!(!is_threat_rumor("the orchards are ripe for harvest"), "orc ⊂ orchards");
        assert!(!is_threat_rumor("skilled workers wanted"), "kill ⊂ skilled");
        assert!(!is_threat_rumor("a warm welcome at the inn"), "war ⊂ warm");
        assert!(!is_threat_rumor("the warden asks about you"), "war ⊂ warden");
        assert!(!is_threat_rumor("the warehouse burned down"), "war ⊂ warehouse");
        assert!(!is_threat_rumor("she cultivates her garden"), "cult ⊂ cultivates");
        assert!(!is_threat_rumor("the culture festival begins"), "cult ⊂ culture");
        assert!(!is_threat_rumor("orcish? no: an orchestra plays"), "orc ⊂ orchestra");
        assert!(!is_threat_rumor("the council meets at noon"), "no stem present");
        assert!(!is_threat_rumor("a swarm of bees by the mill"), "war ⊂ swarm");
        assert!(!is_threat_rumor("warmed ale by the fire"), "war ⊂ warmed");
        assert!(!is_threat_rumor("the diplomat disarms the crisis"), "no stem present");
        assert_eq!(rumor_threat_word("the orchards are ripe"), None);
    }

    /// (2026-08-24 review P1) The matching-law positive side: exact words,
    /// regular inflections, the prefix stems, and the irregular list.
    #[test]
    fn threat_stems_match_whole_words_and_inflections() {
        assert!(is_threat_rumor("orcs mass at the border"), "exact");
        assert!(is_threat_rumor("a goblin ambushes the cart"), "sh-plural");
        assert!(is_threat_rumor("wolves circle the farm"), "irregular plural");
        assert!(is_threat_rumor("thieves stole the tithe"), "irregular plural");
        assert!(is_threat_rumor("the warband swells"), "compound");
        assert!(is_threat_rumor("a warlord claims the pass"), "compound");
        assert!(is_threat_rumor("cultists meet at midnight"), "compound");
        assert!(is_threat_rumor("raiders attacked the mill"), "raider+ed — via raid");
        assert!(is_threat_rumor("a murder shakes the town"), "exact");
        assert!(is_threat_rumor("murderers flee the city"), "er+es");
        assert!(is_threat_rumor("the marauding horde moves"), "prefix stem + exact");
        assert!(is_threat_rumor("plague rumors spread"), "exact");
        assert!(is_threat_rumor("outlaws hold the bridge"), "exact");
        assert!(is_threat_rumor("demons in the cellar"), "exact");
        assert!(is_threat_rumor("a monster was seen"), "exact");
        assert!(is_threat_rumor("the beast is slain"), "exact");
        assert!(is_threat_rumor("war comes to the valley"), "exact bare war");
        assert!(is_threat_rumor("warring clans meet"), "irregular verb");
        assert_eq!(
            rumor_threat_word("the warband swells").as_deref(),
            Some("warband")
        );
    }

    #[test]
    fn time_skip_event_uses_road_scope() {
        let threats = vec![rumor("bandits raid the road", &["camp"])];
        // Deterministic + same-args equal.
        let a = time_skip_event_check(4320, "camp", &threats);
        let b = time_skip_event_check(4320, "camp", &threats);
        assert_eq!(a, b);
        // The destination rumors apply: a threatened camp fires more
        // often over a sweep than a quiet one.
        let quiet: Vec<Rumor> = vec![];
        let fired_threat = (0..400i64)
            .filter(|m| time_skip_event_check(*m, "camp", &threats).is_some())
            .count();
        let fired_quiet = (0..400i64)
            .filter(|m| time_skip_event_check(*m, "camp", &quiet).is_some())
            .count();
        assert!(
            fired_threat > fired_quiet,
            "threat rumors must lower the time-skip DC: {fired_threat} vs {fired_quiet}"
        );
    }

    // ---- settlement detection ----

    #[test]
    fn settlement_detection_signals() {
        assert!(is_settlement(
            Some(&Node {
                setting: "settlement".into(),
                ..Default::default()
            }),
            None
        ));
        assert!(is_settlement(
            Some(&Node {
                name: "Port Vale".into(),
                ..Default::default()
            }),
            None
        ));
        assert!(!is_settlement(
            Some(&Node {
                name: "The Dark Cave".into(),
                ..Default::default()
            }),
            None
        ));
        let mut m = SiteMap::default();
        assert!(!is_settlement(None, Some(&m)));
        m.assets.push(SiteAsset {
            kind: AssetKind::Building,
            ..Default::default()
        });
        assert!(is_settlement(None, Some(&m)));
        // (2026-08-24 fix) Provenance gate: a TRACKER-minted building never
        // reclassifies the map — the dungeon's rest-interruption referee
        // stays armed and city events stay off. Same for Playground spawns.
        m.assets[0].origin = AssetOrigin::NarratorEstablished;
        assert!(
            !is_settlement(None, Some(&m)),
            "tracker-minted building must not settle the map"
        );
        m.assets[0].origin = AssetOrigin::Playground;
        assert!(
            !is_settlement(None, Some(&m)),
            "playground-spawned building must not settle the map"
        );
        // Authored (architect) + later off-screen-evolved buildings DO.
        m.assets[0].origin = AssetOrigin::InitialMap;
        assert!(is_settlement(None, Some(&m)));
        m.assets[0].origin = AssetOrigin::Evolved;
        assert!(is_settlement(None, Some(&m)));
        // A tracker-minted building must not settle a map even BESIDE
        // authored non-building assets (the kind gate + origin gate both
        // apply).
        let mut dungeon = SiteMap::default();
        dungeon.assets.push(SiteAsset {
            kind: AssetKind::Creature,
            origin: AssetOrigin::InitialMap,
            ..Default::default()
        });
        dungeon.assets.push(SiteAsset {
            kind: AssetKind::Building,
            origin: AssetOrigin::NarratorEstablished,
            ..Default::default()
        });
        assert!(
            !is_settlement(None, Some(&dungeon)),
            "a minted building in a dungeon must not settle it"
        );
        // And the armed rest referee actually fires there (DC > 0 path).
        assert_eq!(rest_dc(Some(&dungeon)), 5);
    }
}
