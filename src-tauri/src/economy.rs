//! The Fable economy & management system (2026-08-20).
//!
//! Money is THREE pools, deliberately never merged into one global stat:
//!
//! 1. `wealth` — pocket coin on `PlayerState`, fully liquid. Moved by the
//!    `[LEDGER wealth ±N]` verb (2026-08-22 — payments, tips, loot, rewards;
//!    the tavern-scale flows) + wages/lifestyle at settlement.
//! 2. Property treasuries — per-property tills on `WorldSchema.properties`,
//!    node-liquid and owner-gated (deposit/withdraw/invest only work while
//!    the player stands at the property's node).
//! 3. Pack items — fiction, never priced (shops/item pricing are out of
//!    scope for v1).
//!
//! Net worth is DERIVED at render time only (pocket + owned tills); nothing
//! stores it. All mutation flows through the `[LEDGER]` bracket verb
//! (bracket_parser) + the pure-Rust daily settlement below — the schema
//! delta path has no arm for any of this (the `site_maps`/`promises`
//! structural-immunity pattern).
//!
//! # Settlement
//!
//! [`settle_daily_economy`] runs at the `[TIME]` apply, keyed on per-entity
//! day-boundary crossings, INDEPENDENT of the world-progression tick gate
//! (cheap arithmetic must not wait behind the 4h/1h LLM gate — a Downtime
//! sleep still settles its day). Each entity tracks its own
//! `last_settled_minutes` stamp; `days = floor((now − stamp)/1440)` drives
//! everything. Directives land in the narrator's `<directives>` block via
//! `pending_tick_directives`.
//!
//! # Starving
//!
//! An unpayable lifestyle is a pure-Debuff permanent `StatusTag`
//! (label "Starving") + ONE stamina drain — the existing illness/health-tier/
//! DC cascade (`consequence::SICK_STEMS` carries `"starv"`) picks it up from
//! there. Cleared on solvency via the disguise-revoke removal pattern
//! (position-find + `Vec::remove`).

use crate::consequence::{Polarity, StatusTag};
use crate::schema::WorldSchema;

// ── Constants (module-local, the MAX_PROMISES discipline) ─────────────────

/// Daily lifestyle prices (g/day). Squatter is free — the default tier.
pub const LIFESTYLE_SQUATTER: u32 = 0;
pub const LIFESTYLE_MODEST: u32 = 2;
pub const LIFESTYLE_COMFORTABLE: u32 = 10;
pub const LIFESTYLE_ARISTOCRATIC: u32 = 50;

/// The prosperity band. 100 = normal; a bare `#[serde(default)]` on
/// `Node.prosperity` would zero old saves, so the field carries
/// `default_prosperity` (schema.rs) instead.
pub const PROSPERITY_MIN: u8 = 25;
pub const PROSPERITY_MAX: u8 = 200;
pub const PROSPERITY_DEFAULT: u8 = 100;

/// Lifestyle cost curve cap: `ceil(base × 100 / pct)` clamped to
/// `[base, base × 4]` — a broken town never charges infinity.
pub const COST_MULTIPLIER_CAP: u32 = 4;

/// Stored-property cap (true FIFO by `schema::WorldSchema::property_order`
/// in `enforce_typed_caps`, refuse in the `[LEDGER found]` applier — an
/// authored hub beats a silent drop).
pub const MAX_PROPERTIES: usize = 8;
/// Concurrent-job cap (refuse in the `[LEDGER job]` applier).
pub const MAX_JOBS: usize = 2;
/// Consecutive away-days before a job contract lapses at settlement.
pub const JOB_LAPSE_ABSENT_DAYS: u32 = 3;
/// Consecutive deficit days before creditors seize a property.
pub const DEFICIT_COLLAPSE_DAYS: u32 = 7;
/// The `[LEDGER]` amount clamp (overflow guard, the `PROMISE_DEADLINE_MAX`
/// pattern — an hallucinated astronomically-large number can't overflow the
/// u32 arithmetic).
pub const LEDGER_AMOUNT_MAX: u32 = 100_000;
/// Investment rate: `revenue += floor(amount / 20)`…
pub const INVEST_DIVISOR: u64 = 20;
/// …capped at this daily revenue (per property).
pub const REVENUE_CAP: u32 = 200;
/// Derived buy price when no authored `price:` exists:
/// `25 × max(net_yield, 1)`.
pub const BUY_PRICE_PER_YIELD: u32 = 25;

// ── Types ─────────────────────────────────────────────────────────────────

/// What kind of property this is — flavor + buy-price intuition only;
/// every kind settles identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PropertyKind {
    #[default]
    Business,
    Estate,
    Settlement,
}

impl PropertyKind {
    pub fn parse(word: &str) -> Option<PropertyKind> {
        match word.trim().to_lowercase().as_str() {
            "" | "business" => Some(PropertyKind::Business),
            "estate" => Some(PropertyKind::Estate),
            "settlement" => Some(PropertyKind::Settlement),
            _ => None,
        }
    }

    pub fn word(&self) -> &'static str {
        match self {
            PropertyKind::Business => "business",
            PropertyKind::Estate => "estate",
            PropertyKind::Settlement => "settlement",
        }
    }
}

/// The player's upkeep tier. Default (and the serde default via
/// `Lifestyle::default`) is Squatter — free, dormant, zero prompt bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Lifestyle {
    #[default]
    Squatter,
    Modest,
    Comfortable,
    Aristocratic,
}

impl Lifestyle {
    pub fn parse(word: &str) -> Option<Lifestyle> {
        match word.trim().to_lowercase().as_str() {
            "squatter" => Some(Lifestyle::Squatter),
            "modest" => Some(Lifestyle::Modest),
            "comfortable" => Some(Lifestyle::Comfortable),
            "aristocratic" => Some(Lifestyle::Aristocratic),
            _ => None,
        }
    }

    pub fn word(&self) -> &'static str {
        match self {
            Lifestyle::Squatter => "squatter",
            Lifestyle::Modest => "modest",
            Lifestyle::Comfortable => "comfortable",
            Lifestyle::Aristocratic => "aristocratic",
        }
    }

    /// The g/day base price at prosperity 100.
    pub fn daily_base(&self) -> u32 {
        match self {
            Lifestyle::Squatter => LIFESTYLE_SQUATTER,
            Lifestyle::Modest => LIFESTYLE_MODEST,
            Lifestyle::Comfortable => LIFESTYLE_COMFORTABLE,
            Lifestyle::Aristocratic => LIFESTYLE_ARISTOCRATIC,
        }
    }
}

/// Who holds a property. NPC wealth IS their properties' treasuries (the
/// settlement loop is owner-agnostic); a "town treasury" is a
/// `Settlement`-kind property owned by the mayor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Owner {
    Player,
    Npc(String),
    Unowned,
}

impl Default for Owner {
    fn default() -> Self {
        Owner::Unowned
    }
}

/// One income-bearing property with its own till. Money accounting at the
/// applier: `wealth + Σ player-owned treasuries` moves only through
/// jobs/lifestyle/invest (and deposit/withdraw, which just shift pools);
/// `found` mints at 0; `buy` is a SINK — the price is paid to the seller
/// and leaves the books entirely (2026-08-20 audit fix; crediting the
/// property's own till made every purchase self-refundable).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct Property {
    /// The travel-graph node the property stands at (gate for
    /// deposit/withdraw/invest — the player must be THERE to touch the
    /// till).
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub kind: PropertyKind,
    /// The till. Signed: a deficit drive it negative (collapse tracking);
    /// settlement drains never push it further below its current value.
    #[serde(default)]
    pub treasury_balance: i64,
    #[serde(default)]
    pub daily_revenue: u32,
    #[serde(default)]
    pub daily_upkeep: u32,
    /// Authored asking price; `None` → derived (`25 × max(net_yield, 1)`)
    /// at buy time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<u32>,
    #[serde(default)]
    pub owner: Owner,
    /// Epoch-minutes stamp of the last settled day boundary.
    #[serde(default)]
    pub last_settled_minutes: i64,
    /// Consecutive settled days in deficit (reset to 0 on any net-positive
    /// day; collapse at `DEFICIT_COLLAPSE_DAYS`).
    #[serde(default)]
    pub deficit_days: u32,
}

/// The player's paid work: a title at a node for a daily wage. Jobs pay at
/// settlement (presence-free — the wage arrives whether or not the player
/// stands at the node that day); `JOB_LAPSE_ABSENT_DAYS` consecutive away
/// days end the contract.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, Default)]
pub struct Job {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub daily_wage: u32,
    #[serde(default)]
    pub last_settled_minutes: i64,
    #[serde(default)]
    pub absent_days: u32,
}

/// An authored property seed — one line of a card/player `<properties>`
/// sibling (`id: forge | node: iron-forge | kind: business | revenue: 8 |
/// upkeep: 3`). Parsed + rendered by this module (shared by `sim_card.rs`
/// + `player.rs`); converted to a live [`Property`] at session entry
/// (`seed_properties_into`, lib.rs).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AuthoredProperty {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub node: String,
    /// `business|estate|settlement` (unknown → Business at seed).
    #[serde(default)]
    pub kind: String,
    /// Authored npc id (slug) for scenario/world cards; npc cards override
    /// to the card's own id, player cards to `Player`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub revenue: u32,
    #[serde(default)]
    pub upkeep: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<u32>,
}

// ── Pure curves ───────────────────────────────────────────────────────────

/// The lifestyle cost at a given node prosperity — the inverse curve
/// `ceil(base × 100 / pct)`: hard times cost more (prosperity 25 → the
/// `base × COST_MULTIPLIER_CAP` ceiling), boom discounts (200 → half
/// price). The prosperity clamp already bounds `raw` to
/// `[ceil(base/2), 4×base]`; the explicit floor of 1 only guards the
/// zero case (a nonzero base never costs nothing).
pub fn lifestyle_cost(base: u32, prosperity: u8) -> u32 {
    if base == 0 {
        return 0;
    }
    let pct = prosperity.clamp(PROSPERITY_MIN, PROSPERITY_MAX) as u64;
    let raw = (base as u64 * 100 + pct - 1) / pct;
    let cap = base as u64 * COST_MULTIPLIER_CAP as u64;
    raw.clamp(1, cap) as u32
}

/// Prosperity-scaled daily revenue: `floor(revenue × pct / 100)`.
pub fn scaled_revenue(revenue: u32, prosperity: u8) -> i64 {
    (revenue as u64 * prosperity as u64 / 100).min(i64::MAX as u64) as i64
}

/// Net daily yield at a prosperity: scaled revenue − upkeep (signed).
pub fn net_yield(p: &Property, prosperity: u8) -> i64 {
    scaled_revenue(p.daily_revenue, prosperity) - p.daily_upkeep as i64
}

/// The derived asking price when no authored `price` exists:
/// `25 × max(net_yield, 1)` — a broken property still costs something.
pub fn derived_buy_price(p: &Property, prosperity: u8) -> u32 {
    (BUY_PRICE_PER_YIELD as i64 * net_yield(p, prosperity).max(1)) as u32
}

/// A node's prosperity, defaulting to 100 when the node isn't in the graph
/// yet (an authored property may reference a place the tracker hasn't
/// minted — the ledger still renders).
pub fn node_prosperity(s: &WorldSchema, node_id: &str) -> u8 {
    s.travel_graph
        .nodes
        .iter()
        .find(|n| n.id == node_id)
        .map(|n| n.prosperity)
        .unwrap_or(PROSPERITY_DEFAULT)
}

// ── Currency (2026-08-21 economy addendum — zero hardcoded units) ─────────

/// Max total length (chars) of the currency label — the `[LEDGER currency]`
/// gate. Short by construction: a label is a name ("dollars", "beli") or a
/// slash-tier spec ("gold/silver/copper").
pub const CURRENCY_LABEL_MAX: usize = 40;

/// Max tiers in a `/`-separated tiered label (highest first, lowest = the
/// base unit wealth stores). 2 ("dollars/cents") or 3
/// ("gold/silver/copper"); a single word is a flat label.
pub const CURRENCY_TIERS_MAX: usize = 3;

/// Validate + normalize a tracker-emitted currency label. Accepts a flat
/// name or a 2-3 tier `/`-separated spec (highest tier FIRST, base unit
/// LAST). Rejects (→ `None`, the applier's reject-directive channel):
/// empty, oversize, a tier with no alphanumeric char (the tier render
/// abbreviates to its first letter), and >3 tiers. Never invents a label.
pub fn normalize_currency_label(raw: &str) -> Option<String> {
    let label = raw.trim();
    if label.is_empty() || label.chars().count() > CURRENCY_LABEL_MAX {
        return None;
    }
    let tiers: Vec<&str> = label.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if tiers.len() > CURRENCY_TIERS_MAX {
        return None;
    }
    // A flat single label passes as-is (trimmed); every tier of a spec must
    // carry an alphanumeric for its abbreviation.
    if tiers.len() > 1 && tiers.iter().any(|t| !t.chars().any(|c| c.is_alphanumeric())) {
        return None;
    }
    // Collapse "gold / silver" spacing onto canonical "gold/silver".
    Some(tiers.join("/"))
}

/// The model-facing money render — `{n} {label}` or the naked base-unit
/// integer when the label is empty (`0`, `150`). MODEL SURFACES USE THIS,
/// NOT [`format_money`]: the tracker must read the BASE unit to do
/// `[LEDGER]` arithmetic (tier-splitting a number in `<world_state>` would
/// make deposit/withdraw math unparseable).
pub fn money_plain(amount: i64, label: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        amount.to_string()
    } else {
        format!("{amount} {label}")
    }
}

/// The tier abbreviation for a tiered render: the tier name's first
/// alphanumeric char, lowercased ("gold" → `g`, "Silver" → `s`).
/// `normalize_currency_label` guarantees the char exists.
fn tier_abbrev(name: &str) -> char {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or('?')
}

/// The HUMAN-FACING money render. A tiered label (`gold/silver/copper`)
/// splits the base-unit amount by modulo AT THE RENDER STAGE ONLY — the
/// stored value never changes: 3 tiers step 1:10:100 (1254 → `12g 5s 4c`),
/// 2 tiers step 1:100 (1254 → `12d 54c`). Leading zero tiers are
/// suppressed; the base tier always renders (0 → `0c`). A flat or empty
/// label renders [`money_plain`] ("150 dollars" / "150"). Negative amounts
/// (a till can run below zero) prefix `-` on the whole figure.
pub fn format_money(amount: i64, label: &str) -> String {
    let tiers: Vec<&str> = label.trim().split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if !(2..=CURRENCY_TIERS_MAX).contains(&tiers.len()) {
        return money_plain(amount, label);
    }
    let sign = if amount < 0 { "-" } else { "" };
    let n = amount.unsigned_abs();
    let values: Vec<u64> = match tiers.len() {
        3 => vec![n / 100, (n / 10) % 10, n % 10],
        _ => vec![n / 100, n % 100],
    };
    // Suppress leading zero tiers; keep everything from the first nonzero
    // tier down (the base tier always shows).
    let start = values.iter().position(|v| *v > 0).unwrap_or(values.len() - 1);
    let body: Vec<String> = values[start..]
        .iter()
        .zip(tiers[start..].iter())
        .map(|(v, t)| format!("{v}{}", tier_abbrev(t)))
        .collect();
    format!("{sign}{}", body.join(" "))
}

/// (2026-08-22 second playtest pass — the fabricated wealth gain) Does the
/// narrative window carry any money-MOVEMENT signal? The 0.29.1 playtest
/// tracker emitted `[LEDGER wealth +12]` off the player *mentioning* coin
/// ("counting the coins in my coinpurse" — an inspection, not an exchange)
/// and the schema minted coin that never existed. A GAIN is grounded only
/// when the window carries a transfer verb; coin NOUNS ("coinpurse",
/// "silver") are deliberately NOT signals — they describe, they don't move.
/// Word-boundary, case-insensitive; an EMPTY corpus fails OPEN (the
/// `narrative_grounded` convention — a degenerate window never rejects).
/// SPENDS stay ungated (the insufficient-funds check owns them). A false
/// negative (an obliquely-narrated reward) costs one coached turn; a false
/// positive mints permanent coin — the list skews unambiguous on purpose.
pub fn wealth_gain_grounded(corpus: &[&str]) -> bool {
    if corpus.is_empty() {
        return true;
    }
    const TRANSFER_STEMS: &[&str] = &[
        "paid", "pays", "payment", "repay", "repaid", "repayment",
        "reward", "rewards", "rewarded",
        "earns", "earned", "earnings",
        "loot", "loots", "looted", "looting",
        "pillage", "pillaged", "plunder", "plunders", "plundered",
        "steals", "stole", "stolen",
        "receives", "received",
        "sell", "sells", "sold", "selling",
        "wage", "wages", "salary", "bounty", "bounties", "payout",
        "handout", "handouts",
        // (2026-08-23 audit) The high-frequency phrasings the playtest class
        // kept hitting: direct giving + collection verbs. PAST-TENSE verb
        // forms only — the shared noun forms ("hands", "tips", "pockets")
        // describe, they don't move coin (the "his pockets jingled" class
        // would ground the exact fabrication this gate exists to stop).
        "gave", "given", "gives",
        "granted", "granting",
        "handed",
        "tipped",
        "gifted",
        "collect", "collects", "collected", "collecting",
        "pocketed",
    ];
    corpus.iter().any(|text| {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .any(|w| TRANSFER_STEMS.contains(&w.to_lowercase().as_str()))
    })
}

/// The `<economy_anchor>` block (2026-08-21 Chloe addendum): the Rust-owned
/// relative price ladder rendered inside `<world_state>` — the
/// anti-price-hallucination scaffolding. Deterministic values ONLY (the
/// lifestyle curve at the current node's prosperity); no invented line
/// items, no LLM-written prices. The narrator (and the tracker, which sees
/// the same world_state) prices everyday items against this ladder instead
/// of inventing "500 gold for a beer".
///
/// DORMANT while the economy is dormant (`None` — zero tokens, the
/// fresh-game empty-render invariant): it wakes the moment ANY economy fact
/// exists (a currency label learned, a property, a job, a lifestyle tier,
/// pocket coin in the purse — the wealth verb's flows; 2026-08-22) —
/// exactly the moment money can first appear in prose.
pub fn render_economy_anchor(s: &WorldSchema) -> Option<String> {
    let economy_awake = !s.currency_label.trim().is_empty()
        || !s.properties.is_empty()
        || !s.player_state.jobs.is_empty()
        || s.player_state.lifestyle != Lifestyle::Squatter
        || s.player_state.wealth != 0;
    if !economy_awake {
        return None;
    }
    let prosperity = s
        .travel_graph
        .current_node
        .as_deref()
        .map(|node| node_prosperity(s, node))
        .unwrap_or(PROSPERITY_DEFAULT);
    let unit = if s.currency_label.trim().is_empty() {
        "base money units".to_string()
    } else {
        format!("base units of {}", s.currency_label.trim())
    };
    Some(format!(
        "<economy_anchor>\nPrices in {unit}. Relative ladder at this location: modest living {}/day, comfortable {}/day, aristocratic {}/day.\nA meal, drink, or small purchase is a fraction of the modest line; never invent sums far outside this ladder.\n</economy_anchor>",
        lifestyle_cost(LIFESTYLE_MODEST, prosperity),
        lifestyle_cost(LIFESTYLE_COMFORTABLE, prosperity),
        lifestyle_cost(LIFESTYLE_ARISTOCRATIC, prosperity),
    ))
}

// ── Settlement ────────────────────────────────────────────────────────────

/// Settle every economy entity whose day boundary crossed. Pure Rust (the
/// LLM is never involved); called from `apply_time_command_and_maybe_tick`
/// between the clock apply and the tick gate. Returns
/// `(directives, mutated)` — directives ride the narrator's `<directives>`
/// block, `mutated` tells the caller to push an undo snapshot.
///
/// Emission discipline: TRANSITION-ONLY directives (a state entered/left —
/// deficit, seizure, lapse, starving, recovery, till-dipped). Routine
/// accrual and wages are silent (the ledger line shows them).
///
/// (2026-08-23 Playground) `freeze_survival` — the FREEZE CLAMPS god flag
/// threaded from its caller: when true the INSOLVENCY arm skips the stamina
/// drain + the "Starving" stamp (+ its directive). The money math still
/// runs (pocket drained, tills dipped) — only the survival punishment is
/// suppressed. No prompt-text changes anywhere (Prime Mandate untouched).
pub fn settle_daily_economy(
    s: &mut WorldSchema,
    now_minutes: i64,
    freeze_survival: bool,
) -> (Vec<String>, bool) {
    let mut directives: Vec<String> = Vec::new();
    let mut mutated = false;

    // 1. Properties: accrue `net × days`, track deficit, collapse at 7.
    //    (Immutable reads collect first — the mutation pass then borrows
    //    each entry alone.)
    let property_ids: Vec<String> = s.properties.keys().cloned().collect();
    let mut seized: Vec<(String, String)> = Vec::new();
    for pid in &property_ids {
        let (days, prosperity, node_id) = {
            let Some(p) = s.properties.get(pid) else { continue };
            let days = ((now_minutes - p.last_settled_minutes) / 1440).max(0) as u32;
            if days == 0 {
                continue;
            }
            (
                days,
                node_prosperity(s, &p.node_id),
                p.node_id.clone(),
            )
        };
        let net = {
            let p = s.properties.get(pid).expect("key just collected");
            net_yield(p, prosperity)
        };
        let Some(p) = s.properties.get_mut(pid) else { continue };
        p.treasury_balance = p
            .treasury_balance
            .saturating_add(net.saturating_mul(days as i64));
        p.last_settled_minutes += days as i64 * 1440;
        if net < 0 {
            let was = p.deficit_days;
            p.deficit_days = p.deficit_days.saturating_add(days);
            if was == 0 {
                directives.push(format!(
                    "The \"{}\" at {} has slipped into deficit — upkeep now outpaces its revenue. Let the strain show in the scene.",
                    pid, node_id
                ));
            }
        } else {
            p.deficit_days = 0;
        }
        mutated = true;
        if p.deficit_days >= DEFICIT_COLLAPSE_DAYS {
            // Owner-aware loss wording (2026-08-20 audit): an NPC-owned
            // business collapsing is not "gone from the player's holdings".
            let whose = match &p.owner {
                Owner::Player => "the player's holdings".to_string(),
                Owner::Npc(npc) => format!("{}'s holdings", npc),
                Owner::Unowned => "the ledger".to_string(),
            };
            seized.push((pid.clone(), whose));
        }
    }
    for (pid, whose) in &seized {
        s.properties.remove(pid);
        s.property_order.retain(|o| o != pid);
        directives.push(format!(
            "Creditors seized the \"{}\" after {} days of unchecked deficit — it is gone from {}. Narrate the loss.",
            pid, DEFICIT_COLLAPSE_DAYS, whose
        ));
    }

    // 2. Jobs: pay `wage × days` into pocket wealth (presence-free), count
    //    away-days against the contract.
    let current_node = s.travel_graph.current_node.clone();
    let mut lapsed: Vec<usize> = Vec::new();
    for i in 0..s.player_state.jobs.len() {
        let days = {
            let j = &s.player_state.jobs[i];
            let days = ((now_minutes - j.last_settled_minutes) / 1440).max(0) as u32;
            if days == 0 {
                continue;
            }
            days
        };
        let (title, away) = {
            let present = current_node.as_deref() == Some(s.player_state.jobs[i].node_id.as_str());
            let j = &mut s.player_state.jobs[i];
            s.player_state.wealth = s
                .player_state
                .wealth
                .saturating_add(j.daily_wage.saturating_mul(days));
            j.last_settled_minutes += days as i64 * 1440;
            j.absent_days = if present { 0 } else { j.absent_days.saturating_add(days) };
            mutated = true;
            (j.title.clone(), j.absent_days)
        };
        if away >= JOB_LAPSE_ABSENT_DAYS {
            lapsed.push(i);
            directives.push(format!(
                "The job \"{}\" has lapsed — {} days of absence ended the contract. It no longer pays.",
                title, away
            ));
        }
    }
    for i in lapsed.into_iter().rev() {
        s.player_state.jobs.remove(i);
    }

    // 3. Lifestyle (skip Squatter): the inverse-curve cost at the CURRENT
    //    node, fallback chain pocket → player-owned tills here → Starving.
    if s.player_state.lifestyle != Lifestyle::Squatter {
        let days = ((now_minutes - s.player_state.lifestyle_settled_minutes) / 1440).max(0)
            as u32;
        if days > 0 {
            let prosperity = current_node
                .as_deref()
                .map(|n| node_prosperity(s, n))
                .unwrap_or(PROSPERITY_DEFAULT);
            let per_day = lifestyle_cost(s.player_state.lifestyle.daily_base(), prosperity);
            let total = (per_day as u64 * days as u64).min(LEDGER_AMOUNT_MAX as u64) as u32;
            let mut remaining = total as i64;
            let pocket = (s.player_state.wealth as i64).min(remaining).max(0);
            s.player_state.wealth = (s.player_state.wealth as i64 - pocket) as u32;
            remaining -= pocket;
            // Till-dip: player-owned treasuries at the CURRENT node only
            // (the same node gate as a manual withdraw).
            let mut dipped: Vec<String> = Vec::new();
            if remaining > 0 {
                if let Some(cur) = current_node.as_deref() {
                    let ids: Vec<String> = s.properties.keys().cloned().collect();
                    for tid in ids {
                        if remaining <= 0 {
                            break;
                        }
                        let take = {
                            let Some(p) = s.properties.get_mut(&tid) else { continue };
                            if p.owner != Owner::Player || p.node_id != cur {
                                continue;
                            }
                            let take = p.treasury_balance.clamp(0, remaining);
                            p.treasury_balance -= take;
                            take
                        };
                        if take > 0 {
                            remaining -= take;
                            dipped.push(tid);
                        }
                    }
                }
            }
            s.player_state.lifestyle_settled_minutes += days as i64 * 1440;
            mutated = true;
            let starving_label = |t: &StatusTag| {
                t.kind.is_empty() && t.label.trim().eq_ignore_ascii_case("Starving")
            };
            if remaining > 0 {
                // Could not pay → Starving (once; the tag is permanent, the
                // illness cascade + health-tier derive the rest).
                // (2026-08-23 Playground) FREEZE CLAMPS skips this arm —
                // no drain, no stamp, no directive — WITHOUT falling into
                // the solvent else-branch (a frozen insolvent day must not
                // CLEAR Starving or emit recovery lines; the money math
                // above already ran).
                if !s.status_tags.iter().any(starving_label) && !freeze_survival {
                    crate::consequence::upsert_tag(
                        &mut s.status_tags,
                        StatusTag {
                            label: "Starving".into(),
                            polarity: Polarity::Debuff,
                            expires_at: 0,
                            source: "economy".into(),
                            kind: String::new(),
                        },
                        crate::settings::FABLE_STATUS_TAG_CAP,
                    );
                    s.player_state.stamina.drain();
                    directives.push(format!(
                        "The player could not keep up their {} lifestyle — they went hungry. Weave the deprivation into the scene.",
                        s.player_state.lifestyle.word()
                    ));
                }
            } else {
                if !dipped.is_empty() {
                    directives.push(format!(
                        "Living costs drained the till{} for {} day{} of upkeep — the coffers are thinner.",
                        dipped
                            .iter()
                            .take(2)
                            .map(|id| format!(" \"{}\"", id))
                            .collect::<Vec<_>>()
                            .join(","),
                        days,
                        if days == 1 { "" } else { "s" }
                    ));
                }
                // Solvent → clear Starving if present (the disguise-revoke
                // removal pattern: position-find + Vec::remove).
                if let Some(pos) = s.status_tags.iter().position(starving_label) {
                    s.status_tags.remove(pos);
                    directives.push(
                        "The player is eating properly again — the hunger has broken. Reflect the recovery."
                            .into(),
                    );
                }
            }
        }
    }

    (directives, mutated)
}

// ── Renders ───────────────────────────────────────────────────────────────

/// The `ledger:` prompt line — the narrator's economy read. Cap 8 entries +
/// `(+N more)` (2026-08-21 evening follow-up to the 8192 ruling: 4 → 8 —
/// every property of a real portfolio renders); `BANKRUPT <n>d` marker while
/// in deficit; an owner marker for NPC-owned tills (player-owned is the
/// unmarked default reading). `None` when no properties exist (dormant —
/// zero prompt bytes).
pub fn render_ledger_line(s: &WorldSchema) -> Option<String> {
    if s.properties.is_empty() {
        return None;
    }
    const LEDGER_PROMPT_CAP: usize = 8;
    let mut parts: Vec<String> = Vec::new();
    for (id, p) in s.properties.iter().take(LEDGER_PROMPT_CAP) {
        let prosperity = node_prosperity(s, &p.node_id);
        let net = net_yield(p, prosperity);
        let mut entry = format!(
            "{}@{} {:+}/day till {}",
            id, p.node_id, net, p.treasury_balance
        );
        if p.deficit_days > 0 {
            entry.push_str(&format!(" BANKRUPT {}d", p.deficit_days));
        }
        if let Owner::Npc(npc) = &p.owner {
            entry.push_str(&format!(" (owner {})", npc));
        }
        parts.push(entry);
    }
    if s.properties.len() > LEDGER_PROMPT_CAP {
        parts.push(format!("(+{} more)", s.properties.len() - LEDGER_PROMPT_CAP));
    }
    Some(parts.join("; "))
}

/// The player's derived wealth across pools (pocket + player-owned tills) —
/// the net-worth read for UI surfaces. Pocket alone is `wealth`.
pub fn player_net_worth(s: &WorldSchema) -> i64 {
    s.player_state.wealth as i64
        + s.properties
            .values()
            .filter(|p| p.owner == Owner::Player)
            .map(|p| p.treasury_balance)
            .sum::<i64>()
}

// ── Authored <properties> sibling lines ───────────────────────────────────

/// Parse the pipe-kv line format (`id: forge | node: iron-forge | kind:
/// business | revenue: 8 | upkeep: 3`). One property per line; lines
/// missing an id or node are skipped (author noise, never fatal). Shared by
/// the card + player parsers.
pub fn parse_property_lines(text: &str) -> Vec<AuthoredProperty> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut ap = AuthoredProperty::default();
        for part in line.split('|') {
            let Some((k, v)) = part.split_once(':') else { continue };
            let k = k.trim().to_lowercase();
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k.as_str() {
                "id" => ap.id = v.to_string(),
                "node" => ap.node = v.to_string(),
                "kind" => ap.kind = v.to_string(),
                "owner" => ap.owner = Some(v.to_string()),
                "revenue" => ap.revenue = v.parse().unwrap_or(0),
                "upkeep" => ap.upkeep = v.parse().unwrap_or(0),
                "price" => ap.price = v.parse().ok(),
                _ => {}
            }
        }
        if ap.id.trim().is_empty() || ap.node.trim().is_empty() {
            continue;
        }
        out.push(ap);
    }
    out
}

/// Render the pipe-kv lines back out (round-trip half of
/// [`parse_property_lines`]). Owner + price ride only when present.
pub fn render_property_lines(props: &[AuthoredProperty]) -> String {
    let mut out = String::new();
    for ap in props {
        let mut parts = vec![
            format!("id: {}", ap.id.trim()),
            format!("node: {}", ap.node.trim()),
            format!("kind: {}", if ap.kind.trim().is_empty() { "business".into() } else { ap.kind.trim().to_string() }),
        ];
        if let Some(owner) = ap.owner.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            parts.push(format!("owner: {}", owner));
        }
        parts.push(format!("revenue: {}", ap.revenue));
        parts.push(format!("upkeep: {}", ap.upkeep));
        if let Some(price) = ap.price {
            parts.push(format!("price: {}", price));
        }
        out.push_str(&parts.join(" | "));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Node;

    fn schema_with_property(p: Property) -> WorldSchema {
        let mut s = WorldSchema::default();
        s.travel_graph.nodes.push(Node {
            id: p.node_id.clone(),
            prosperity: PROSPERITY_DEFAULT,
            ..Default::default()
        });
        s.properties.insert("forge".into(), p);
        s
    }

    fn property(node: &str, revenue: u32, upkeep: u32) -> Property {
        Property {
            node_id: node.into(),
            kind: PropertyKind::Business,
            treasury_balance: 0,
            daily_revenue: revenue,
            daily_upkeep: upkeep,
            price: None,
            owner: Owner::Player,
            last_settled_minutes: 0,
            deficit_days: 0,
        }
    }

    // ── Wealth-gain grounding (2026-08-22 second playtest pass) ──────────

    #[test]
    fn wealth_gain_grounded_gates_on_transfer_verbs() {
        // Empty corpus fails OPEN (the narrative_grounded convention).
        assert!(wealth_gain_grounded(&[]));
        // The playtest's exact fabrication — coin NOUNS never ground a gain.
        assert!(!wealth_gain_grounded(&[
            "*I whistle, counting the coins in my coinpurse now with this*",
        ]));
        assert!(!wealth_gain_grounded(&["\"Three silver to find Rhet,\" she quotes"]));
        // Real flows ground.
        assert!(wealth_gain_grounded(&["the guildmaster paid me twelve silver"]));
        assert!(wealth_gain_grounded(&["I loot the corpse's purse"]));
        assert!(wealth_gain_grounded(&["she sells the ears to the guild"]));
        assert!(wealth_gain_grounded(&["a reward of 5 silver, earned in full"]));
        // Word-boundary + case-insensitive: "Repayment" (no substring
        // accident), and "coinpurse" must NOT match any stem.
        assert!(wealth_gain_grounded(&["Repayment arrives by courier"]));
        assert!(!wealth_gain_grounded(&["coinpurse coinpurse coinpurse"]));
    }

    // ── Curves ────────────────────────────────────────────────────────────

    #[test]
    fn lifestyle_curve_scales_and_clamps() {
        // At 100: the base price, unchanged.
        assert_eq!(lifestyle_cost(LIFESTYLE_COMFORTABLE, 100), 10);
        // Hard times (25): 10 × 100/25 = 40 — exactly the 4× cap.
        assert_eq!(lifestyle_cost(LIFESTYLE_COMFORTABLE, 25), 40);
        // Below the floor clamps up to the floor first (a prosperity of 1
        // would ask 1000): still the cap.
        assert_eq!(lifestyle_cost(LIFESTYLE_COMFORTABLE, 1), 40);
        // Boom (200): ceil(10 × 100/200) = 5 — half price.
        assert_eq!(lifestyle_cost(LIFESTYLE_COMFORTABLE, 200), 5);
        // Ceil division: modest (2) at 150 → ceil(200/150) = 2.
        assert_eq!(lifestyle_cost(LIFESTYLE_MODEST, 150), 2);
        // Squatter is always free.
        assert_eq!(lifestyle_cost(LIFESTYLE_SQUATTER, 25), 0);
    }

    #[test]
    fn scaled_revenue_floors() {
        assert_eq!(scaled_revenue(8, 100), 8);
        assert_eq!(scaled_revenue(8, 50), 4);
        assert_eq!(scaled_revenue(7, 50), 3, "floor(3.5) = 3");
    }

    #[test]
    fn derived_buy_price_floors_at_one_yield() {
        let mut p = property("town", 10, 20); // net −10 → max(−10, 1) = 1
        assert_eq!(derived_buy_price(&p, 100), 25);
        p.daily_upkeep = 0; // net 10 → 250
        assert_eq!(derived_buy_price(&p, 100), 250);
    }

    // ── Currency (2026-08-21 addendum) ───────────────────────────────────

    #[test]
    fn currency_label_normalizes_and_rejects() {
        assert_eq!(normalize_currency_label("  dollars "), Some("dollars".into()));
        assert_eq!(
            normalize_currency_label("gold / silver / copper"),
            Some("gold/silver/copper".into()),
            "loose tier spacing canonicalizes"
        );
        assert_eq!(normalize_currency_label("dollars/cents"), Some("dollars/cents".into()));
        assert_eq!(normalize_currency_label(""), None);
        assert_eq!(normalize_currency_label("   "), None);
        assert_eq!(normalize_currency_label("a/b/c/d"), None, ">3 tiers reject");
        assert_eq!(
            normalize_currency_label(&"x".repeat(CURRENCY_LABEL_MAX + 1)),
            None,
            "oversize rejects"
        );
    }

    #[test]
    fn money_plain_never_invents_a_unit() {
        assert_eq!(money_plain(0, ""), "0");
        assert_eq!(money_plain(150, ""), "150");
        assert_eq!(money_plain(150, "dollars"), "150 dollars");
        // The label renders flat even when tiered — model surfaces must
        // show the BASE unit (tracker arithmetic).
        assert_eq!(money_plain(1254, "gold/silver/copper"), "1254 gold/silver/copper");
        assert_eq!(money_plain(-7, "credits"), "-7 credits");
    }

    #[test]
    fn format_money_tiers_at_render_stage_only() {
        // Flat + empty passthrough.
        assert_eq!(format_money(0, ""), "0");
        assert_eq!(format_money(150, "dollars"), "150 dollars");
        // 3 tiers step 1:10:100 — Chloe's pinned example.
        assert_eq!(format_money(1254, "gold/silver/copper"), "12g 5s 4c");
        // Leading zero tiers suppress; the base tier always renders.
        assert_eq!(format_money(54, "gold/silver/copper"), "5s 4c");
        assert_eq!(format_money(4, "gold/silver/copper"), "4c");
        assert_eq!(format_money(0, "gold/silver/copper"), "0c");
        assert_eq!(format_money(1200, "gold/silver/copper"), "12g");
        // 2 tiers step 1:100.
        assert_eq!(format_money(1254, "dollars/cents"), "12d 54c");
        // Negative (a till below zero) keeps the sign on the whole figure.
        assert_eq!(format_money(-1254, "gold/silver/copper"), "-12g 5s 4c");
        // Tier abbreviation = first alphanumeric, case-insensitive.
        assert_eq!(format_money(1254, "Gold/Silver/Copper"), "12g 5s 4c");
    }

    #[test]
    fn economy_anchor_renders_rust_owned_ladder() {
        // Dormant on a fresh world — the empty-render invariant (zero
        // tokens until money can appear).
        let s = crate::schema::WorldSchema::default();
        assert!(render_economy_anchor(&s).is_none(), "dormant while the economy is");
        // (2026-08-22 wealth verb) Pocket coin wakes it too — money in the
        // purse is the moment prices can first appear in prose.
        let mut s = s;
        s.player_state.wealth = 7;
        assert!(
            render_economy_anchor(&s).is_some(),
            "pocket wealth wakes the anchor"
        );
        let mut s = crate::schema::WorldSchema::default();
        // Awake via a property (no label yet): unit-free framing + the
        // lifestyle ladder at prosperity 100 (no node → default).
        s.properties.insert("forge".into(), Property::default());
        let anchor = render_economy_anchor(&s).expect("property wakes the anchor");
        assert!(anchor.starts_with("<economy_anchor>"), "{anchor}");
        assert!(anchor.contains("modest living 2/day"), "{anchor}");
        assert!(anchor.contains("comfortable 10/day"), "{anchor}");
        assert!(anchor.contains("aristocratic 50/day"), "{anchor}");
        assert!(anchor.contains("base money units"), "no label → unit-free framing");
        // A learned label names the unit; prosperity shifts the ladder.
        s.currency_label = "gold/silver/copper".into();
        s.travel_graph.nodes.push(crate::schema::Node {
            id: "town".into(),
            name: "Town".into(),
            prosperity: 200,
            ..Default::default()
        });
        s.travel_graph.current_node = Some("town".into());
        let anchor = render_economy_anchor(&s).expect("label keeps it awake");
        assert!(anchor.contains("base units of gold/silver/copper"), "{anchor}");
        assert!(anchor.contains("comfortable 5/day"), "boom halves the curve: {anchor}");
    }

    // ── Settlement ────────────────────────────────────────────────────────

    #[test]
    fn settlement_accrues_per_day_and_stamps() {
        let mut s = schema_with_property(property("town", 10, 3)); // net +7
        let (dirs, mutated) = settle_daily_economy(&mut s, 2 * 1440, false);
        assert!(mutated);
        assert!(dirs.is_empty(), "routine accrual is silent");
        assert_eq!(s.properties["forge"].treasury_balance, 14);
        assert_eq!(s.properties["forge"].last_settled_minutes, 2 * 1440);
        // Idempotence within the same day.
        let (_, mutated2) = settle_daily_economy(&mut s, 2 * 1440 + 300, false);
        assert!(!mutated2);
        assert_eq!(s.properties["forge"].treasury_balance, 14);
    }

    #[test]
    fn settlement_tracks_deficit_transitions_and_collapse() {
        let mut s = schema_with_property(property("town", 0, 5)); // net −5
        let (dirs, _) = settle_daily_economy(&mut s, 1440, false);
        assert_eq!(dirs.len(), 1, "deficit ENTRY is a transition directive");
        assert!(dirs[0].contains("deficit"));
        assert_eq!(s.properties["forge"].treasury_balance, -5);
        assert_eq!(s.properties["forge"].deficit_days, 1);
        // Day 2..6: still in deficit, no new directive.
        let (dirs, _) = settle_daily_economy(&mut s, 6 * 1440, false);
        assert!(dirs.is_empty(), "ongoing deficit is silent");
        assert_eq!(s.properties["forge"].deficit_days, 6);
        assert_eq!(s.properties["forge"].treasury_balance, -30);
        // Day 7: seizure.
        let (dirs, _) = settle_daily_economy(&mut s, 7 * 1440, false);
        assert!(!s.properties.contains_key("forge"), "collapsed at 7 deficit days");
        assert!(dirs.iter().any(|d| d.contains("seized")));
        // (2026-08-20 audit) The order vec carries no dead id after a
        // seizure, and an NPC-held collapse names the NPC, not the player.
        assert!(s.property_order.is_empty(), "seizure sweeps property_order");
        s.properties.insert(
            "guild".into(),
            Property {
                owner: Owner::Npc("mara".into()),
                ..property("town", 0, 5)
            },
        );
        let (dirs, _) = settle_daily_economy(&mut s, 8 * 1440, false);
        assert!(
            dirs.iter()
                .any(|d| d.contains("seized") && d.contains("mara's holdings")),
            "owner-aware seizure wording: {dirs:?}"
        );
        // Conservation: the treasury vanished with the property (no ghost
        // money anywhere).
        assert_eq!(player_net_worth(&s), 0);
    }

    #[test]
    fn settlement_recovery_resets_deficit() {
        let mut s = schema_with_property(property("town", 0, 5));
        settle_daily_economy(&mut s, 2 * 1440, false);
        assert_eq!(s.properties["forge"].deficit_days, 2);
        // The town booms (node prosperity 200 doubles revenue) — but upkeep
        // still wins at revenue 0. Instead: revenue now exceeds upkeep.
        s.properties.get_mut("forge").unwrap().daily_revenue = 10;
        settle_daily_economy(&mut s, 3 * 1440, false);
        assert_eq!(s.properties["forge"].deficit_days, 0, "net-positive day resets the streak");
        // −10 accrued + net +5/day × 1 day… (−10 at day 2, then +5 at day 3).
        assert_eq!(s.properties["forge"].treasury_balance, -10 + 5);
    }

    #[test]
    fn jobs_pay_and_lapse() {
        let mut s = WorldSchema::default();
        s.travel_graph.nodes.push(Node {
            id: "mill".into(),
            ..Default::default()
        });
        s.travel_graph.current_node = Some("away".into());
        s.player_state.jobs.push(Job {
            title: "Miller".into(),
            node_id: "mill".into(),
            daily_wage: 4,
            last_settled_minutes: 0,
            absent_days: 0,
        });
        // Day 1-2: paid while away (presence-free), absence counted.
        let (dirs, mutated) = settle_daily_economy(&mut s, 2 * 1440, false);
        assert!(mutated);
        assert_eq!(s.player_state.wealth, 8);
        assert_eq!(s.player_state.jobs[0].absent_days, 2);
        assert!(dirs.is_empty(), "wages are silent");
        // Day 3: contract lapses.
        let (dirs, _) = settle_daily_economy(&mut s, 3 * 1440, false);
        assert_eq!(s.player_state.wealth, 12, "the lapsing day still pays");
        assert!(s.player_state.jobs.is_empty());
        assert!(dirs.iter().any(|d| d.contains("lapsed")));
    }

    #[test]
    fn jobs_reset_absence_when_present() {
        let mut s = WorldSchema::default();
        s.travel_graph.current_node = Some("mill".into());
        s.player_state.jobs.push(Job {
            title: "Miller".into(),
            node_id: "mill".into(),
            daily_wage: 4,
            last_settled_minutes: 0,
            absent_days: 2,
        });
        settle_daily_economy(&mut s, 1440, false);
        assert_eq!(s.player_state.jobs[0].absent_days, 0, "presence resets the counter");
        assert_eq!(s.player_state.jobs.len(), 1);
    }

    #[test]
    fn lifestyle_fallback_chain_pocket_till_starving() {
        let mut s = WorldSchema::default();
        s.travel_graph.current_node = Some("town".into());
        s.travel_graph.nodes.push(Node {
            id: "town".into(),
            ..Default::default()
        });
        s.player_state.wealth = 5;
        s.player_state.lifestyle = Lifestyle::Comfortable; // 10/day at 100
        s.properties.insert(
            "inn".into(),
            Property {
                node_id: "town".into(),
                treasury_balance: 3,
                owner: Owner::Player,
                ..property("town", 0, 0)
            },
        );
        // A player-owned till at ANOTHER node must NOT be dipped.
        s.properties.insert(
            "farm".into(),
            Property {
                node_id: "elsewhere".into(),
                treasury_balance: 100,
                owner: Owner::Player,
                ..property("elsewhere", 0, 0)
            },
        );
        // An NPC-owned till at the current node must NOT be dipped either.
        s.properties.insert(
            "guild".into(),
            Property {
                node_id: "town".into(),
                treasury_balance: 100,
                owner: Owner::Npc("mara".into()),
                ..property("town", 0, 0)
            },
        );
        let (dirs, mutated) = settle_daily_economy(&mut s, 1440, false);
        assert!(mutated);
        // 10 due: 5 from pocket + 3 from the inn till, 2 short → Starving.
        assert_eq!(s.player_state.wealth, 0);
        assert_eq!(s.properties["inn"].treasury_balance, 0);
        assert_eq!(s.properties["farm"].treasury_balance, 100);
        assert_eq!(s.properties["guild"].treasury_balance, 100);
        assert!(s
            .status_tags
            .iter()
            .any(|t| t.label == "Starving" && t.polarity == Polarity::Debuff));
        assert_eq!(s.player_state.stamina.semantic(), "Active", "one drain from Fresh");
        assert!(dirs.iter().any(|d| d.contains("hungry")));
        assert!(!dirs.iter().any(|d| d.contains("drained")),
            "the till-dip note rides only the SOLVENT path: {dirs:?}");
        // The unfed remainder is hardship, not debt: nothing went negative.
        assert!(!s.properties.values().any(|p| p.treasury_balance < 0));
    }

    #[test]
    fn lifestyle_solvent_till_dip_directive() {
        let mut s = WorldSchema::default();
        s.travel_graph.current_node = Some("town".into());
        s.travel_graph.nodes.push(Node {
            id: "town".into(),
            ..Default::default()
        });
        s.player_state.wealth = 5;
        s.player_state.lifestyle = Lifestyle::Comfortable; // 10/day
        s.properties.insert(
            "inn".into(),
            Property {
                node_id: "town".into(),
                treasury_balance: 8,
                owner: Owner::Player,
                ..property("town", 0, 0)
            },
        );
        let (dirs, mutated) = settle_daily_economy(&mut s, 1440, false);
        assert!(mutated);
        // Fully paid: pocket 5 + till 5 of 8.
        assert_eq!(s.player_state.wealth, 0);
        assert_eq!(s.properties["inn"].treasury_balance, 3);
        assert!(s.status_tags.is_empty(), "solvent day never starves");
        assert!(dirs.iter().any(|d| d.contains("drained")), "the dip is notable");
        // Conservation: 5 pocket + 8 till in, 10 paid out, 3 remain.
        assert_eq!(player_net_worth(&s), 3);
    }

    #[test]
    fn starving_clears_on_solvency() {
        let mut s = WorldSchema::default();
        s.travel_graph.current_node = Some("town".into());
        s.player_state.lifestyle = Lifestyle::Modest; // 2/day
        s.player_state.lifestyle_settled_minutes = 0;
        s.status_tags.push(StatusTag {
            label: "Starving".into(),
            polarity: Polarity::Debuff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(),
        });
        s.player_state.wealth = 10;
        let (dirs, _) = settle_daily_economy(&mut s, 2 * 1440, false);
        assert!(s.status_tags.is_empty(), "solvent day clears Starving");
        assert!(dirs.iter().any(|d| d.contains("eating properly")));
        assert_eq!(s.player_state.wealth, 6);
    }

    #[test]
    fn starving_not_reapplied_or_redrained() {
        let mut s = WorldSchema::default();
        s.travel_graph.current_node = Some("town".into());
        s.player_state.lifestyle = Lifestyle::Comfortable;
        s.status_tags.push(StatusTag {
            label: "Starving".into(),
            polarity: Polarity::Debuff,
            expires_at: 0,
            source: String::new(),
            kind: String::new(),
        });
        s.player_state.stamina = crate::player_state::Stamina::Winded;
        settle_daily_economy(&mut s, 3 * 1440, false);
        assert_eq!(
            s.status_tags.iter().filter(|t| t.label == "Starving").count(),
            1,
            "no stacking"
        );
        assert_eq!(
            s.player_state.stamina.semantic(),
            "Winded",
            "no second drain while already Starving"
        );
    }

    #[test]
    fn squatter_and_empty_economy_settle_nothing() {
        let mut s = WorldSchema::default();
        let (dirs, mutated) = settle_daily_economy(&mut s, 30 * 1440, false);
        assert!(!mutated);
        assert!(dirs.is_empty());
    }

    #[test]
    fn freeze_survival_skips_starving_stamp_and_drain() {
        // (2026-08-23 Playground) FREEZE CLAMPS: an insolvent lifestyle day
        // still drains the pocket (the money math runs), but the "Starving"
        // stamp + the stamina drain + the hunger directive never land.
        let mut make = || {
            let mut s = WorldSchema::default();
            s.travel_graph.current_node = Some("town".into());
            s.player_state.lifestyle = Lifestyle::Comfortable;
            s.player_state.wealth = 0;
            s
        };
        let mut frozen = make();
        let (dirs, mutated) = settle_daily_economy(&mut frozen, 2 * 1440, true);
        assert!(mutated, "the settle still ran (stamps advanced)");
        assert!(
            !frozen.status_tags.iter().any(|t| t.label == "Starving"),
            "no Starving stamp while frozen"
        );
        assert!(dirs.is_empty(), "no hunger directive while frozen: {dirs:?}");
        // The unfrozen twin DOES starve (the pin that freeze suppresses a
        // real arm, not a dead one).
        let mut live = make();
        let (dirs, _) = settle_daily_economy(&mut live, 2 * 1440, false);
        assert!(live.status_tags.iter().any(|t| t.label == "Starving"));
        assert!(!dirs.is_empty());
    }

    // ── Conservation invariant ────────────────────────────────────────────

    #[test]
    fn conservation_across_a_mixed_week() {
        // wealth + Σ treasuries changes ONLY by property net + wages −
        // lifestyle across settlement — every flow is visible in the books.
        let mut s = WorldSchema::default();
        s.travel_graph.current_node = Some("town".into());
        s.travel_graph.nodes.push(Node {
            id: "town".into(),
            ..Default::default()
        });
        s.player_state.wealth = 20;
        s.properties.insert(
            "forge".into(),
            property("town", 6, 2), // +4/day
        );
        s.player_state.jobs.push(Job {
            title: "Apprentice".into(),
            node_id: "town".into(),
            daily_wage: 3,
            ..Default::default()
        });
        s.player_state.lifestyle = Lifestyle::Modest; // −2/day
        let before = player_net_worth(&s);
        settle_daily_economy(&mut s, 7 * 1440, false);
        let after = player_net_worth(&s);
        // 7 × (4 + 3 − 2) = +35 exactly.
        assert_eq!(after - before, 35);
        assert_eq!(s.player_state.wealth, 20 + 7 * 3 - 7 * 2);
        assert_eq!(s.properties["forge"].treasury_balance, 7 * 4);
    }

    // ── Renders ───────────────────────────────────────────────────────────

    #[test]
    fn ledger_line_caps_and_marks() {
        let mut s = WorldSchema::default();
        // 10 properties against the live LEDGER_PROMPT_CAP of 8 → the
        // first 8 render (sorted BTreeMap order) + a (+2 more) marker.
        for i in 0..10 {
            s.properties.insert(
                format!("p{i}"),
                Property {
                    node_id: "town".into(),
                    treasury_balance: i as i64 * 10,
                    daily_revenue: 4,
                    daily_upkeep: 2,
                    owner: if i == 0 {
                        Owner::Npc("mara".into())
                    } else {
                        Owner::Player
                    },
                    ..property("town", 4, 2)
                },
            );
        }
        s.properties.get_mut("p3").unwrap().deficit_days = 2;
        let line = render_ledger_line(&s).expect("properties exist");
        assert!(line.starts_with("p0@town +2/day till 0 (owner mara)"));
        assert!(line.contains("BANKRUPT 2d"), "deficit marker rides: {line}");
        assert!(line.contains("(+2 more)"), "cap 8 + overflow marker");
        assert!(render_ledger_line(&WorldSchema::default()).is_none());
    }

    // ── Authored sibling lines ────────────────────────────────────────────

    #[test]
    fn property_lines_round_trip() {
        let text = "id: forge | node: iron-forge | kind: business | owner: liam | revenue: 8 | upkeep: 3 | price: 250\n\
                    id: manor | node: hill | kind: estate | revenue: 2 | upkeep: 9\n";
        let props = parse_property_lines(text);
        assert_eq!(props.len(), 2);
        assert_eq!(props[0].owner.as_deref(), Some("liam"));
        assert_eq!(props[0].price, Some(250));
        assert!(props[1].owner.is_none());
        let rendered = render_property_lines(&props);
        let reparsed = parse_property_lines(&rendered);
        assert_eq!(props, reparsed, "round-trip is lossless");
        // Noise lines are skipped, not fatal.
        assert!(parse_property_lines("garbage without pipes\n\n").is_empty());
        assert!(parse_property_lines("id: x | revenue: notanumber\n").is_empty(),
            "missing node skips the line");
    }
}
