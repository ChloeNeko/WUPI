//! Episodic memory consolidation (2026-08-23 WS6) — the LLM-curated memory
//! compressor, greenlit after WS4 laid its foundation (the turn journal,
//! pinning, rollback). Runs OFF-TURN as a deferred background job (the
//! world-tick discipline: post-lock-drop, session-identity-guarded,
//! watermark-resumable — a failed batch commits NOTHING, so the next
//! trigger retries from the surviving un-consolidated set).
//!
//! # The pipeline (cheapest-first, the fail-proof contract shape)
//!
//! 1. **Trigger** (lib.rs): after a Fable turn fully finalizes, if the
//!    card's partition carries > [`TRIGGER_UNCONSOLIDATED_TURNS`] live
//!    un-pinned turns, the worker silently boots.
//! 2. **Pre-scoring** (pure Rust, zero tokens): fetch the oldest
//!    un-consolidated turns, group them into consecutive batches
//!    (≤ [`BATCH_MAX_TURNS`] / ≤ [`BATCH_CHAR_BUDGET`] chars), and GATE each
//!    batch on a redundancy signal: any adjacent-turn word-bigram Jaccard
//!    ≥ [`JACCARD_GATE`] OR ≥ [`DUPE_LINE_MIN_COUNT`] duplicate normalized
//!    lines. A batch with NO redundancy is a run of distinct events — the
//!    never-merge-distinct-events law, expressed mechanically — and stays
//!    raw for the FIFO prune. Duplicate lines are also dropped from the
//!    render (token savings).
//! 3. **Extraction** (the local E4B, `FableTurnMode::Consolidator` on the
//!    fable engine's 8192 context — factual extraction is the local
//!    model's job class, per the 2026-08-23 ruling): ONE low-token decode
//!    per batch + at most ONE repair pass (the architect loop). Summaries
//!    are factual — preserve names/places/numbers/outcomes/time markers
//!    in source order; never invent.
//! 4. **Commit** (`memory::consolidate_apply`): ONE transaction inserts
//!    the `Role::Summary` row(s) under a `consol_*` batch key and
//!    supersedes the source turns under the Immutable Source Law guard.
//!    Retrieval never sees superseded rows (the Golden Retrieval Rule).
//!
//! # Role isolation
//!
//! Local-only by design. The optional API heavy-lifter fallback (a failed
//! batch retried against the narrator API) was DEFERRED at greenlight —
//! factual extraction is tracking-class work; the retry hook point is the
//! repair-pass failure below. Revisit only with a Chloe ruling.

use crate::memory::UnconsolidatedTurn;

// ---------------------------------------------------------------------------
// Caps (module-scope, per the settings.rs single-module rule)
// ---------------------------------------------------------------------------

/// The worker's trigger threshold: more live un-consolidated turns than
/// this after a Fable turn finalizes → consolidate (the greenlit spec's
/// ">30 un-consolidated turns").
pub const TRIGGER_UNCONSOLIDATED_TURNS: usize = 30;
/// Max turns per extraction batch (the greenlit micro-batch size — small
/// enough for the 8192 tracker context, large enough to compress a scene).
pub const BATCH_MAX_TURNS: usize = 15;
/// A batch smaller than this consolidates nothing (a lone turn is not a
/// pattern; leave it raw until neighbors accumulate).
pub const BATCH_MIN_TURNS: usize = 2;
/// Whole-batch source-char budget (≈ the derived tracker prompt headroom
/// class; 15 fat turns at [`ROW_CHAR_CAP`] would overflow 8192 tokens, so
/// the budget — not the turn cap — is the binding limit on fat turns).
pub const BATCH_CHAR_BUDGET: usize = 18_000;
/// Per-part (one user action / one assistant chunk) char cap in the render.
pub const ROW_CHAR_CAP: usize = 1_200;
/// Word-bigram Jaccard gate — adjacent turns must share vocabulary at this
/// level (a redundant run: same scene, same actors, same routine) before
/// any LLM spend on the batch.
pub const JACCARD_GATE: f64 = 0.6;
/// A duplicate normalized line must be at least this many CHARS
/// (`chars().count()`, the anti-pattern #6 discipline) to count.
pub const DUPE_LINE_MIN_CHARS: usize = 15;
/// ≥ this many duplicate lines across the batch is an independent
/// redundancy signal (passes the gate without the Jaccard threshold).
pub const DUPE_LINE_MIN_COUNT: usize = 3;
/// Extracted summary cap (chars) — Rust-side truncate after the model.
pub const SUMMARY_CHAR_MAX: usize = 600;
/// Max extracted events per record.
pub const EVENTS_MAX: usize = 8;
/// Per-event char cap — Rust-side truncate after the model.
pub const EVENT_CHAR_MAX: usize = 200;
/// Max batches per worker run — bounds lock churn; the next trigger eats
/// the rest of the backlog.
pub const MAX_BATCHES_PER_RUN: usize = 4;
/// After this many consecutive all-fail runs the worker stands down until
/// a success resets the streak (the bounded-burn guard).
pub const FAIL_STREAK_LIMIT: u32 = 3;

// ---------------------------------------------------------------------------
// Pre-scoring (pure)
// ---------------------------------------------------------------------------

/// Lowercase, split to word tokens (alphanumeric runs — Unicode-aware via
/// `char::is_alphanumeric`), for bigram scoring.
fn normalize_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            cur.push(c.to_ascii_lowercase());
        } else if !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// The set of word bigrams ("the watch" → "the watch"). A lone word has no
/// bigrams (empty set — Jaccard against anything = 0).
fn word_bigrams(text: &str) -> std::collections::BTreeSet<String> {
    let words = normalize_words(text);
    words
        .windows(2)
        .map(|w| format!("{} {}", w[0], w[1]))
        .collect()
}

/// Jaccard similarity of two bigram sets: |∩| / |∪| ∈ [0, 1]. Both empty →
/// 0.0 (nothing in common to measure).
fn jaccard(a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// One normalized line for the dedup check: lowercase, whitespace
/// collapsed, trimmed — compared by CHARS (`chars().count()`), never bytes.
fn normalize_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut last_ws = true;
    for c in line.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(lc);
            last_ws = false;
        }
    }
    out.trim().to_string()
}

/// The full text of one turn (its parts concatenated) — the Jaccard unit.
fn turn_text(turn: &UnconsolidatedTurn) -> String {
    turn.parts
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Group ordered turns into consecutive batches: ≤ [`BATCH_MAX_TURNS`],
/// cumulative rendered chars ≤ [`BATCH_CHAR_BUDGET`]; only batches of
/// ≥ [`BATCH_MIN_TURNS`] emit (a lone tail turn stays raw until neighbors
/// accumulate). A turn that can NEVER share a legal batch (alone over the
/// char budget) is dropped BEFORE it can touch the window — dropped turns
/// stay live + retrievable and are re-fetched by the next worker trigger;
/// crucially, a batch carries EXACTLY the turns it renders, so the commit's
/// supersede set (the batch's turn keys) is exactly the extraction prompt's
/// turn set. Pure; consumes the input.
pub fn build_batches(turns: Vec<UnconsolidatedTurn>) -> Vec<Vec<UnconsolidatedTurn>> {
    let mut batches = Vec::new();
    let mut cur: Vec<UnconsolidatedTurn> = Vec::new();
    let mut cur_chars = 0usize;
    for turn in turns {
        let t_chars: usize = turn
            .parts
            .iter()
            .map(|(_, t)| t.chars().count().min(ROW_CHAR_CAP) + 16) // per-part render chrome
            .sum();
        // (2026-08-24 bug-2 fix) A turn TOO FAT FOR ANY LEGAL BATCH is
        // dropped BEFORE it can touch the window: alone it busts the char
        // budget, so every ≥2-turn batch holding it busts too, and
        // accumulating it built the over-budget batches the Consolidator
        // hard-refuses (the 2026-08-23 audit wedge). Checking FIRST means a
        // fat ARRIVAL can never evict a healthy accumulated window — the
        // old code cleared `cur` at the overflow, so a small lone turn
        // sitting in front of a fat arrival was silently dropped from
        // batching too, and the window restarted ON the fat turn only to
        // overflow again on the next arrival.
        if t_chars > BATCH_CHAR_BUDGET {
            tracing::debug!(
                chars = t_chars,
                "consolidation batching: dropping an over-fat turn (stays raw)"
            );
            continue;
        }
        if !cur.is_empty()
            && (cur.len() >= BATCH_MAX_TURNS || cur_chars + t_chars > BATCH_CHAR_BUDGET)
        {
            if cur.len() >= BATCH_MIN_TURNS {
                batches.push(std::mem::take(&mut cur));
                cur_chars = 0;
            } else if cur_chars >= t_chars {
                // (2026-08-24 bug-2 fix) Lone-window overflow (accumulated +
                // incoming don't fit; neither alone busts the budget):
                // drop the FATTER of the two and keep the thinner as the
                // window head. Oldest-first grouping must lose turns from
                // the CORRECT end — the old unconditional `cur.clear()`
                // always sacrificed the OLDER accumulated turn even when
                // the ARRIVAL was the fat one, and a fat head that survived
                // here would skip every later turn this run (the audit
                // wedge re-entering through the side door). The dropped
                // turn stays raw + retrievable; the next trigger retries it.
                tracing::debug!(
                    chars = cur_chars,
                    "consolidation batching: dropping an over-fat lone turn (stays raw)"
                );
                cur.clear();
                cur_chars = 0;
            } else {
                // The ARRIVAL is the fatter side — skip it; the window (and
                // its char tally) is untouched, so the older turn keeps its
                // place and later turns still batch with it.
                tracing::debug!(
                    chars = t_chars,
                    "consolidation batching: skipping a fat arrival at the window head (stays raw)"
                );
                continue;
            }
        }
        cur_chars += t_chars;
        cur.push(turn);
    }
    if cur.len() >= BATCH_MIN_TURNS {
        batches.push(cur);
    }
    batches
}

/// Count duplicate normalized lines (≥ [`DUPE_LINE_MIN_CHARS`] chars) that
/// appear in ≥2 DIFFERENT parts of the batch — the line-level redundancy
/// signal.
fn count_duplicate_lines(batch: &[UnconsolidatedTurn]) -> usize {
    use std::collections::BTreeMap;
    let mut seen_in_parts: BTreeMap<String, usize> = BTreeMap::new();
    let mut part_idx = 0usize;
    for turn in batch {
        for (_, text) in &turn.parts {
            part_idx += 1;
            for line in text.split('\n') {
                let norm = normalize_line(line);
                if norm.chars().count() < DUPE_LINE_MIN_CHARS {
                    continue;
                }
                let entry = seen_in_parts.entry(norm).or_insert(part_idx);
                if *entry != part_idx {
                    // Already seen in a different part — count once by
                    // marking it with a sentinel (usize::MAX).
                    *entry = usize::MAX;
                }
            }
        }
    }
    seen_in_parts.values().filter(|v| **v == usize::MAX).count()
}

/// The redundancy gate: a batch is worth LLM spend iff any ADJACENT pair of
/// turns shares word-bigram Jaccard ≥ [`JACCARD_GATE`] (a redundant run)
/// OR ≥ [`DUPE_LINE_MIN_COUNT`] lines repeat across parts. Distinct-event
/// batches fail and stay raw (re-checked on later triggers — cheap).
pub fn batch_redundant(batch: &[UnconsolidatedTurn]) -> bool {
    if batch.len() < BATCH_MIN_TURNS {
        return false;
    }
    let texts: Vec<std::collections::BTreeSet<String>> =
        batch.iter().map(|t| word_bigrams(&turn_text(t))).collect();
    let linked = texts.windows(2).any(|w| jaccard(&w[0], &w[1]) >= JACCARD_GATE);
    linked || count_duplicate_lines(batch) >= DUPE_LINE_MIN_COUNT
}

// ---------------------------------------------------------------------------
// Prompt render (pure; the Gemma `<|turn>` protocol shape, the architect
// precedent)
// ---------------------------------------------------------------------------

// (2026-08-24 Part II A7) The craft laws: temporal anchors stay, the
// detailed facts of ALL source turns are compiled + retained (generic
// filler like "Merged scene data." is a severe failure), and the target
// length is 30-60% of the source — compression, not erasure.
const CONSOLIDATION_SYSTEM_INSTRUCTION: &str = "You are a memory archivist. You compress past \
roleplay turns into ONE compact factual record. Preserve every name, place, number, outcome, \
and time reference, in source order. Drop prose style, repeated dialogue, and routine \
actions. Never invent facts. Preserve every time reference — never strip all temporal \
anchors from a record. Compile and retain the detailed facts of all source turns; replacing \
detail with generic summary text (e.g. \"Merged scene data.\") is a severe failure. Target \
30-60% of the source length. Output ONLY one fenced ```json object.";

/// Render the extraction prompt for one batch: per-part entries
/// `[turn|role] text` (duplicate normalized lines dropped from the render,
/// each part capped at [`ROW_CHAR_CAP`] chars). Pure.
pub fn render_consolidation_prompt(batch: &[UnconsolidatedTurn]) -> String {
    let mut out = String::with_capacity(2_048);
    out.push_str("<|turn>system\n");
    out.push_str(CONSOLIDATION_SYSTEM_INSTRUCTION);
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str(&format!(
        "Consolidate these {} consecutive past turns of one campaign into one \
         compact factual record.\n",
        batch.len()
    ));
    // Line-dedup across the whole batch render (token savings — the same
    // action line repeated across turns carries once).
    let mut seen_lines: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (ti, turn) in batch.iter().enumerate() {
        for (role, text) in &turn.parts {
            let mut kept: Vec<&str> = Vec::new();
            for line in text.split('\n') {
                let norm = normalize_line(line);
                if norm.chars().count() >= DUPE_LINE_MIN_CHARS {
                    if !seen_lines.insert(norm) {
                        continue; // duplicate line — drop from the render
                    }
                }
                kept.push(line);
            }
            let joined = kept.join("\n");
            let bounded: String = joined.chars().take(ROW_CHAR_CAP).collect();
            out.push_str(&format!("[{}|{}] {}\n", ti + 1, role.as_str(), bounded));
        }
    }
    out.push_str(
        "Output ONLY one fenced ```json object of this shape:\n\
         {\"summary\": \"<=600 chars, the arc in source order\", \"events\": \
         [\"<=200 chars each, <=8 discrete events in order\"]}\n",
    );
    out.push_str("<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

/// The single repair pass's prompt (the `generate_with_repair` shape: prior
/// raw output + every error). Pure.
pub fn render_consolidation_repair(prior_raw: &str, errors: &[String]) -> String {
    let mut out = String::with_capacity(2_048);
    out.push_str("<|turn>system\n");
    out.push_str("Your previous consolidation output was invalid. Emit ONLY one corrected \
                  fenced ```json object. Fix EVERY error:\n");
    for e in errors {
        out.push_str(&format!("- {}\n", e));
    }
    if crate::settings::THINKING_ENABLED {
        out.push_str("<|think|>");
    }
    out.push_str("<turn|>\n");
    out.push_str("<|turn>user\n");
    out.push_str("Your previous output was:\n");
    out.push_str(&prior_raw.chars().take(2_000).collect::<String>());
    out.push_str("\n---\nEmit the corrected fenced ```json object now.\n<turn|>\n");
    out.push_str("<|turn>model\n");
    out
}

// ---------------------------------------------------------------------------
// Output parse + validate (pure)
// ---------------------------------------------------------------------------

/// One validated consolidation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidatedRecord {
    pub summary: String,
    pub events: Vec<String>,
}

/// Strip a ``` / ```json fence wrapper (the architect's tolerance class);
/// bare JSON passes through.
fn strip_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
    else {
        return trimmed;
    };
    let Some(end) = after_open.rfind("```") else {
        return after_open.trim();
    };
    after_open[..end].trim()
}

/// Clean one extracted string: control chars out, capped at `cap` CHARS.
fn clean_capped(raw: &str, cap: usize) -> String {
    raw.chars()
        .filter(|c| {
            let code = *c as u32;
            !(code == 0x7F || code <= 0x08 || code == 0x0B || code == 0x0C || (0x0E..=0x1F).contains(&code))
        })
        .collect::<String>()
        .chars()
        .take(cap)
        .collect()
}

/// Parse + validate a consolidator reply (the `SchemaDelta::from_model_output`
/// pipeline shape: reply channel → fence strip → `json_repair` → parse →
/// Rust-side clamps). Errors are human-readable coaching lines for the
/// repair pass. Pure.
pub fn parse_consolidation_output(raw: &str) -> Result<ConsolidatedRecord, Vec<String>> {
    let reply = crate::schema::extract_reply_channel(raw);
    let cleaned = strip_fences(&reply).trim();
    let repaired = crate::json_repair::repair(cleaned);
    let value: serde_json::Value = match serde_json::from_str(&repaired) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("output is not valid JSON ({e}) — emit ONLY the fenced ```json object")]),
    };
    let summary_raw = value
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let summary = clean_capped(summary_raw, SUMMARY_CHAR_MAX);
    let mut errors = Vec::new();
    if summary.trim().is_empty() {
        errors.push("\"summary\" is empty — summarize the turns in one factual paragraph".to_string());
    }
    let mut events: Vec<String> = Vec::new();
    if let Some(arr) = value.get("events").and_then(|v| v.as_array()) {
        for ev in arr.iter().take(EVENTS_MAX) {
            let text = clean_capped(ev.as_str().unwrap_or(""), EVENT_CHAR_MAX);
            if !text.trim().is_empty() {
                events.push(text);
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(ConsolidatedRecord { summary, events })
}

/// Mint the consolidation batch key — `consol_` + a fresh turn uuid. The
/// prefix is load-bearing: the un-consolidated count/fetch filters it out
/// so batches never feed themselves back in.
pub fn new_batch_turn_uuid() -> String {
    format!("consol_{}", crate::memory::new_turn_uuid())
}

impl ConsolidatedRecord {
    /// The consolidated row's text: a self-describing header + the summary
    /// + discrete events. `add_memory` chunks it under [`crate::memory::CHUNK_CHAR_BUDGET`]
    /// (the bge gate) with parent grouping.
    pub fn to_row_text(&self, turn_count: usize) -> String {
        let mut out = format!("Consolidated record of {turn_count} turns: {}", self.summary);
        for e in &self.events {
            out.push_str("\n- ");
            out.push_str(e);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Role;

    fn turn(id: &str, user: &str, asst: &str) -> UnconsolidatedTurn {
        UnconsolidatedTurn {
            turn_uuid: id.to_string(),
            parts: vec![
                (Role::User, user.to_string()),
                (Role::Assistant, asst.to_string()),
            ],
        }
    }

    #[test]
    fn jaccard_math() {
        let a = word_bigrams("the watch patrols the gate");
        let b = word_bigrams("the watch patrols the hall");
        // Shared: "the watch", "watch patrols", "patrols the". Union adds
        // "the gate" + "the hall" → 3/5.
        assert!((jaccard(&a, &b) - 0.6).abs() < 1e-9, "{:?}", jaccard(&a, &b));
        // Identical → 1.0; disjoint → 0.0; empty → 0.0.
        assert!((jaccard(&a, &a) - 1.0).abs() < 1e-9);
        let c = word_bigrams("completely unrelated words here");
        assert!(jaccard(&a, &c) == 0.0);
        assert!(jaccard(&a, &Default::default()) == 0.0);
        // A lone word has no bigrams — no false similarity.
        assert!(word_bigrams("gate").is_empty());
    }

    /// (2026-08-24 Part II A7) The craft laws ride the extraction prompt:
    /// temporal anchors preserved, generic filler named as severe failure,
    /// and the 30-60% target length.
    #[test]
    fn system_instruction_carries_craft_laws() {
        let prompt = render_consolidation_prompt(&[turn(
            "t1",
            "I search the vault.",
            "You find sealed lead caskets.",
        )]);
        assert!(
            prompt.contains("never strip all temporal anchors"),
            "temporal-anchor law missing"
        );
        assert!(
            prompt.contains("severe failure"),
            "generic-filler law missing"
        );
        assert!(
            prompt.contains("30-60% of the source length"),
            "target-length law missing"
        );
    }

    /// (2026-08-23 audit fix) A turn too fat to share a legal batch is
    /// DROPPED from batching (stays raw) — never accumulated past the char
    /// budget. Before the fix, a fat lone turn kept accumulating until a
    /// flush emitted an over-budget batch the Consolidator hard-refuses,
    /// deterministically wedging the card's consolidation.
    #[test]
    fn build_batches_drops_over_fat_lone_turns() {
        // Fat = 16 parts at the per-part cap (a pasted ~19k-char turn) —
        // alone it already exceeds the 18k budget.
        let fat_part: String = "x ".repeat(ROW_CHAR_CAP / 2);
        let fat_turn = UnconsolidatedTurn {
            turn_uuid: "fat".to_string(),
            parts: (0..16).map(|_| (Role::User, fat_part.clone())).collect(),
        };
        let small = |id: &str| turn(id, "I look around.", "The room is quiet.");
        // [fat, small, small]: the fat turn pairs with nothing (any batch
        // holding it is over budget) — it must stay raw, the smalls batch.
        let batches = build_batches(vec![fat_turn, small("s1"), small("s2")]);
        assert!(
            batches
                .iter()
                .all(|b| b.iter().all(|t| t.turn_uuid != "fat")),
            "the over-fat turn must stay raw"
        );
        for b in &batches {
            let chars: usize = b
                .iter()
                .flat_map(|t| &t.parts)
                .map(|(_, t)| t.chars().count().min(ROW_CHAR_CAP) + 16)
                .sum();
            assert!(
                chars <= BATCH_CHAR_BUDGET,
                "batch over budget: {chars} > {BATCH_CHAR_BUDGET}"
            );
            assert!(b.len() >= BATCH_MIN_TURNS, "a lone turn never batches");
        }
        // Two fat turns back-to-back: NEITHER may batch with the other.
        let fat_turn2 = UnconsolidatedTurn {
            turn_uuid: "fat2".to_string(),
            parts: (0..16).map(|_| (Role::Assistant, fat_part.clone())).collect(),
        };
        let batches = build_batches(vec![fat_turn, fat_turn2, small("s1"), small("s2")]);
        for b in &batches {
            assert!(
                b.iter().all(|t| t.turn_uuid != "fat" && t.turn_uuid != "fat2"),
                "fat turns must never ride a batch"
            );
            let chars: usize = b
                .iter()
                .flat_map(|t| &t.parts)
                .map(|(_, t)| t.chars().count().min(ROW_CHAR_CAP) + 16)
                .sum();
            assert!(chars <= BATCH_CHAR_BUDGET, "pair batch over budget: {chars}");
        }
    }

    #[test]
    fn gate_accepts_redundant_runs_and_rejects_distinct_events() {
        // Same scene, same actors, near-identical beats → linked.
        let redundant = vec![
            turn(
                "t1",
                "I train with Wren in the courtyard again",
                "Wren drills you through the same sword forms in the courtyard; your arms burn.",
            ),
            turn(
                "t2",
                "I keep training with Wren in the courtyard",
                "Wren drills you through the same sword forms in the courtyard; your arms burn.",
            ),
        ];
        assert!(batch_redundant(&redundant), "a redundant run must pass");
        // Distinct scenes share almost no vocabulary → rejected.
        let distinct = vec![
            turn(
                "t1",
                "I ask the ferryman about the far shore",
                "The ferryman names a price; the river runs high with spring melt.",
            ),
            turn(
                "t2",
                "I break into the counting house at night",
                "The ledgers reveal the magistrate's debts; a guard's lantern swings near.",
            ),
        ];
        assert!(!batch_redundant(&distinct), "distinct events stay raw");
    }

    #[test]
    fn gate_accepts_on_duplicate_lines_alone() {
        // Three DISTINCT short repeated lines (few bigrams each) inside
        // long DIFFERENT prose: adjacent Jaccard stays far below the bar,
        // the line signal alone passes the gate.
        let beat1 = "the evening bell tolls twice";
        let beat2 = "wren counts seven coppers";
        let beat3 = "the guard waves us through";
        let mut runs = Vec::new();
        for i in 0..3 {
            runs.push(turn(
                &format!("t{i}"),
                &format!("day {i}: I chart a wholly different errand route {i}"),
                &format!(
                    "{beat1}\n{beat2}\n{beat3}\nThen a long scene unique to \
                     this day unfolds: crooked alley number {i}, a stranger \
                     with a lantern, an argument about tide tables, and a \
                     dog that follows you exactly halfway home."
                ),
            ));
        }
        // Sanity: the adjacent Jaccard really is below the gate here (the
        // test must exercise the LINE signal, not the bigram one).
        let texts: Vec<_> = runs.iter().map(|t| word_bigrams(&turn_text(t))).collect();
        assert!(
            texts.windows(2).all(|w| jaccard(&w[0], &w[1]) < JACCARD_GATE),
            "fixture must not pass via Jaccard"
        );
        assert!(
            batch_redundant(&runs),
            ">=3 distinct repeating lines must pass independently"
        );
    }

    #[test]
    fn normalize_line_is_char_aware() {
        assert_eq!(normalize_line("  The   Watch  "), "the watch");
        assert_eq!(normalize_line("").chars().count(), 0);
        // chars().count() (never bytes) — the CJK discipline.
        assert!(normalize_line("追赶在市场广场上的人").chars().count() >= DUPE_LINE_MIN_CHARS);
    }

    /// (2026-08-24 bug-2 fix) A fat ARRIVAL must never evict a healthy
    /// accumulated window: the old code cleared `cur` at the overflow, so
    /// [small, fat, small, small] dropped the FIRST small (stayed raw) and
    /// restarted the window on the fat turn. Now the fat turn alone is
    /// dropped and every small turn batches.
    #[test]
    fn fat_arrival_never_evicts_a_healthy_window() {
        let fat_part: String = "x ".repeat(ROW_CHAR_CAP / 2);
        let fat = UnconsolidatedTurn {
            turn_uuid: "fat".to_string(),
            parts: (0..16).map(|_| (Role::User, fat_part.clone())).collect(),
        };
        let small = |id: &str| turn(id, "I look around.", "The room is quiet.");
        let batches = build_batches(vec![small("s1"), fat, small("s2"), small("s3")]);
        assert!(
            batches
                .iter()
                .all(|b| b.iter().all(|t| t.turn_uuid != "fat")),
            "the over-fat turn must stay raw"
        );
        let batched: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.iter().map(|t| t.turn_uuid.as_str()))
            .collect();
        assert!(
            batched.contains(&"s1"),
            "the healthy window head batches despite the fat arrival: {batched:?}"
        );
        for b in &batches {
            assert!(b.len() >= BATCH_MIN_TURNS, "a lone turn never batches");
        }
    }

    /// (2026-08-24 bug-2 fix) A lone-window overflow drops the FATTER side:
    /// a fat head is sacrificed so later turns still batch (no wedge), and
    /// a fat arrival is skipped so the older thinner head keeps its place.
    #[test]
    fn lone_window_overflow_drops_the_fatter_side() {
        let mk = |id: &str, parts: usize| UnconsolidatedTurn {
            turn_uuid: id.to_string(),
            parts: (0..parts)
                .map(|_| (Role::User, "x".repeat(ROW_CHAR_CAP)))
                .collect(),
        };
        // Each part contributes ROW_CHAR_CAP + 16 = 1216 chars.
        let big_head = mk("big-head", 9); // 10944 chars
        let thinner = mk("thinner", 7); // 8512 chars
        let small = |id: &str| turn(id, "I look around.", "The room is quiet.");

        // Fat HEAD: 10944 + 8512 > budget, head is fatter → head drops,
        // the thinner arrival batches with the smalls after it.
        let batches = build_batches(vec![big_head.clone(), thinner.clone(), small("s1"), small("s2")]);
        assert!(
            batches
                .iter()
                .all(|b| b.iter().all(|t| t.turn_uuid != "big-head")),
            "the fatter HEAD drops at a lone-window overflow"
        );
        let batched: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.iter().map(|t| t.turn_uuid.as_str()))
            .collect();
        assert!(batched.contains(&"thinner"), "the thinner side batches: {batched:?}");

        // Fat ARRIVAL: the same pair in the other order — the thin window
        // (the older turn) keeps its place, the fat arrival is skipped.
        let batches = build_batches(vec![thinner, big_head, small("s1"), small("s2")]);
        assert!(
            batches
                .iter()
                .all(|b| b.iter().all(|t| t.turn_uuid != "big-head")),
            "the fatter ARRIVAL is skipped, not the healthy window"
        );
        let batched: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.iter().map(|t| t.turn_uuid.as_str()))
            .collect();
        assert!(batched.contains(&"thinner"), "the older thin head batches: {batched:?}");
        for b in &batches {
            let chars: usize = b
                .iter()
                .flat_map(|t| &t.parts)
                .map(|(_, t)| t.chars().count().min(ROW_CHAR_CAP) + 16)
                .sum();
            assert!(chars <= BATCH_CHAR_BUDGET, "batch over budget: {chars}");
        }
    }

    #[test]
    fn batches_respect_turn_and_char_caps() {
        let mut turns = Vec::new();
        for i in 0..40 {
            turns.push(turn(&format!("t{i}"), "short", "short"));
        }
        let batches = build_batches(turns);
        assert!(
            batches.iter().all(|b| b.len() <= BATCH_MAX_TURNS),
            "turn cap"
        );
        assert!(
            batches.iter().all(|b| b.len() >= BATCH_MIN_TURNS),
            "min turns"
        );
        assert!(batches.len() >= 3, "40 short turns split into >=3 batches");
        // Order preserved: first batch starts at the oldest turn.
        assert_eq!(batches[0][0].turn_uuid, "t0");

        // Fat turns close the batch on the CHAR budget before the turn cap.
        let fat: Vec<_> = (0..20)
            .map(|i| turn(&format!("f{i}"), &"x".repeat(1_500), &"y".repeat(1_500)))
            .collect();
        let fat_batches = build_batches(fat);
        assert!(
            fat_batches.iter().all(|b| b.len() < BATCH_MAX_TURNS),
            "char budget binds before the turn cap on fat turns"
        );
        // A lone tail turn stays raw.
        let single = build_batches(vec![turn("only", "a", "b")]);
        assert!(single.is_empty(), "a lone turn never batches");
    }

    #[test]
    fn prompt_renders_protocol_and_dedupes_lines() {
        let dup = "the evening bell tolls across the market square";
        let batch = vec![
            turn("t1", &format!("I wait\n{dup}"), &format!("{dup}\nfirst beat")),
            turn("t2", "I wait again", &format!("{dup}\nsecond beat")),
        ];
        let prompt = render_consolidation_prompt(&batch);
        assert!(prompt.starts_with("<|turn>system\n"));
        assert!(prompt.contains("<|turn>user\n"));
        assert!(prompt.ends_with("<|turn>model\n"));
        assert!(prompt.contains("[1|user]"), "entries carry index + role: {prompt}");
        assert!(prompt.contains("[2|assistant]"));
        assert!(prompt.contains("```json"), "output contract teaches the fence");
        // The duplicate line renders exactly once across the whole prompt.
        assert_eq!(
            prompt.matches(dup).count(),
            1,
            "duplicate lines dedupe from the render"
        );
    }

    #[test]
    fn parse_accepts_fenced_and_bare_json() {
        let fenced = "```json\n{\"summary\": \"The party cleared the warren over three days.\", \"events\": [\"Wren joined\", \"The chief fell\"]}\n```";
        let rec = parse_consolidation_output(fenced).expect("fenced parses");
        assert!(rec.summary.starts_with("The party"));
        assert_eq!(rec.events, vec!["Wren joined".to_string(), "The chief fell".to_string()]);
        let bare = parse_consolidation_output("{\"summary\": \"Bare ok.\", \"events\": []}").unwrap();
        assert!(bare.events.is_empty());
        // The Gemma channel wrapper tolerates (reply channel extracted).
        let wrapped = parse_consolidation_output(
            "<|channel>thought\nshould compress\n<channel|>{\"summary\": \"Ok.\", \"events\": []}",
        )
        .unwrap();
        assert_eq!(wrapped.summary, "Ok.");
    }

    #[test]
    fn parse_rejects_and_coaches() {
        let errs = parse_consolidation_output("not json at all").unwrap_err();
        assert!(errs.iter().any(|e| e.contains("not valid JSON")));
        let errs =
            parse_consolidation_output("{\"summary\": \"   \", \"events\": []}").unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("summary")),
            "empty summary coached: {errs:?}"
        );
    }

    #[test]
    fn clamps_apply_after_parse() {
        // Oversize summary + events: Rust-side clamps hold the caps.
        let raw = format!(
            "{{\"summary\": \"{}\", \"events\": [\"{}\"]}}",
            "s".repeat(2_000),
            "e".repeat(900)
        );
        let rec = parse_consolidation_output(&raw).unwrap();
        assert!(rec.summary.chars().count() <= SUMMARY_CHAR_MAX);
        assert!(rec.events[0].chars().count() <= EVENT_CHAR_MAX);
        // Control chars never survive into the row text.
        let dirty = parse_consolidation_output(
            "{\"summary\": \"a\u{0001}b\", \"events\": []}",
        )
        .unwrap();
        assert_eq!(dirty.summary, "ab");
    }

    #[test]
    fn row_text_carries_header_and_events() {
        let rec = ConsolidatedRecord {
            summary: "The arc.".into(),
            events: vec!["one".into(), "two".into()],
        };
        let text = rec.to_row_text(12);
        assert!(text.starts_with("Consolidated record of 12 turns: The arc."));
        assert!(text.contains("\n- one"));
    }

    #[test]
    fn batch_uuid_is_consol_prefixed() {
        let id = new_batch_turn_uuid();
        assert!(id.starts_with("consol_"), "{id}");
        assert!(id.len() > "consol_".len());
    }
}
