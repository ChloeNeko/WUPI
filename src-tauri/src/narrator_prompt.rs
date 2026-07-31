use crate::schema::ScenePacing;
use crate::sim_card::SimCard;

/// Which narrator role a prompt is being built for.
///
/// §11.41 (2026-07-28, DM / Voice-Actor split). In LOCAL mode one model does
/// the full job — narration + bracket tracking — and uses the [`Tracker`]
/// prompt (the legacy [`build_narrator_system_prompt`]). In API mode the turn
/// is split into two stages: the local 12B runs FIRST as the **tracker**
/// (Tracker prompt, prose discarded — only its brackets are applied), then the
/// API runs SECOND as the **narrator** ([`Narrator`] prompt, prose-only, no
/// `BRACKET_PROTOCOL` at all). The narrator is a blindfolded storyteller: it
/// receives authoritative post-tracker state as `<world_state>` and must honor
/// it — no God Mode, no inventing outcomes the engine didn't track.
///
/// [`Tracker`]: NarratorMode::Tracker
/// [`Narrator`]: NarratorMode::Narrator
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarratorMode {
    /// Full job: prose + bracket tracking. Used by LOCAL mode AND the API-mode
    /// tracker stage (whose prose is discarded — only its brackets are applied).
    Tracker,
    /// Prose-only. The API narrator in API mode. No `BRACKET_PROTOCOL`; sees
    /// authoritative post-tracker state as `<world_state>`.
    Narrator,
}

pub fn build_narrator_system_prompt(
    card: &SimCard,
    world_state: Option<&str>,
    scene_pacing: ScenePacing,
    directive: Option<&str>,
    memory_block: Option<&str>,
) -> String {
    let player_display = player_display_name(card);
    let player_address = player_address(card);

    let mut out = String::with_capacity(3072);

    out.push_str("<narrator_role>\n");
    out.push_str(&narrator_core(&player_display));
    out.push_str("\n</narrator_role>\n\n");

    out.push_str("<scenario>\n");
    if let Some(setting) = card.setting.as_deref() {
        out.push_str("setting: ");
        out.push_str(setting.trim());
        out.push_str("\n\n");
    }
    if let Some(tone) = card.tone.as_deref() {
        out.push_str("tone: ");
        out.push_str(tone.trim());
        out.push_str("\n\n");
    }
    out.push_str("player: ");
    out.push_str(&player_display);
    out.push_str("\n\n");
    if !card.start_npc_ids.is_empty() {
        out.push_str("present_npcs: ");
        out.push_str(&card.start_npc_ids.join(", "));
        out.push_str("\n");
        out.push_str(
            "  (Each NPC above may speak. Wrap their dialogue with \
             [CHARACTER_TURN:<npc_id>] ... [CHARACTER_TURN:end]. \
             The id must match one of these exactly.)\n",
        );
    }
    if !card.declared_activities.is_empty() {
        out.push_str("\nactivities_in_play: ");
        out.push_str(&card.declared_activities.join(", "));
        out.push_str("\n");
    }
    out.push_str("</scenario>\n\n");

    out.push_str("<player>\n");
    out.push_str(&player_contract(&player_address));
    out.push_str("\n</player>\n\n");

    out.push_str("<bracket_commands>\n");
    out.push_str(BRACKET_PROTOCOL);
    out.push_str("\n</bracket_commands>\n\n");

    if let Some(state) = world_state {
        if !state.trim().is_empty() {
            out.push_str("<world_state>\n");
            out.push_str(state.trim());
            out.push_str("\n</world_state>\n\n");
        }
    }

    // Retrieved fable codex knowledge (2026-07-29): the deep playbook slice
    // the embedder judged relevant to THIS turn (bracket-command detail,
    // narrative discipline, common errors, plus the active card's own lore).
    // Surfaced under the codex frame ("reference knowledge you possess;
    // internalize it, weave it naturally"). Zero baseline cost — omitted when
    // no entries clear the cosine floor. This is the offload target for the
    // prompt-distillation: hyper-specific rules live in the codex and arrive
    // on semantic match, keeping the inline prompt lean.
    if let Some(block) = memory_block {
        let trimmed = block.trim();
        if !trimmed.is_empty() {
            out.push_str("<retrieved_knowledge>\n");
            out.push_str(trimmed);
            out.push_str("\n</retrieved_knowledge>\n\n");
        }
    }

    // Ghostwriter Director directive (§11.24, 2026-07-27): a one-shot player-
    // armed steer the narrator MUST obey for THIS turn only. Placed AFTER
    // <world_state> (it's a constraint about the world just described) and
    // BEFORE <scene_pacing> (so the ordering chain is: facts → constraints →
    // pacing rhythm → identity anchor). <active_reality> stays the LAST block
    // — its cross-card-KV-contamination job depends on being the literal tail,
    // so the directive sits earlier in the chain. Omitted entirely when empty
    // (mirrors the world_state-omitted-when-empty skip path). The text is the
    // player's free-form steer, rewritten by Ghostwriter into a single
    // imperative sentence; we emit it verbatim inside the tag.
    if let Some(d) = directive {
        let trimmed = d.trim();
        if !trimmed.is_empty() {
            out.push_str("<director_directive>\n");
            out.push_str(trimmed);
            out.push_str("\n</director_directive>\n\n");
        }
    }

    // Scene pacing tag (Fable Seam #4 expansion, 2026-07-27): Rust-computed
    // per-turn mode that the narrator MUST obey for prose rhythm. Placed
    // AFTER <world_state> (it's a directive about the facts just listed) and
    // BEFORE <active_reality> (the identity-anchor stays the tail — its
    // cross-card-KV-contamination job depends on being the literal last
    // block; scene_pacing is a directive, not an anchor, so it sits earlier).
    // The mode is the operationally consumed field; the prose_guidance line
    // is bespoke per mode (Combat → terse present-tense, Downtime → lush slow).
    out.push_str("<scene_pacing mode=\"");
    out.push_str(scene_pacing.mode.tag());
    out.push_str("\">\n");
    out.push_str(scene_pacing.mode.prose_guidance());
    out.push_str("\n</scene_pacing>\n\n");

    out.push_str("<active_reality>\n");
    out.push_str(&format!(
        "You are narrating {}, NOT any other scenario. ",
        card.name.trim(),
    ));
    if let Some(name) = card.player_name.as_deref() {
        let n = name.trim();
        if !n.is_empty() {
            out.push_str(&format!(
                "The player's name is {n}: use this name exclusively when an \
                 NPC addresses them directly; never invent or import a \
                 different player name. "
            ));
        } else {
            out.push_str(
                "The player has no set name. Refer to the player as \"you\" \
                 (second person); never invent a name for them. ",
            );
        }
    } else {
        out.push_str(
            "The player has no set name. Refer to the player as \"you\" \
             (second person); never invent a name for them. ",
        );
    }
    if let Some(setting) = card.setting.as_deref() {
        let brief: String = setting.trim().chars().take(160).collect();
        out.push_str(&format!("Setting recap: {brief}… "));
    }
    out.push_str(
        "Do NOT reference characters, locations, items, or elements from \
         any other scenario: only what belongs to this one.\n",
    );
    out.push_str("</active_reality>\n\n");

    out
}

/// Build the **prose-only** narrator system prompt for the API stage of an
/// API-mode turn (§11.41, the "voice actor" half of the DM / voice-actor
/// split).
///
/// Same block order + identity anchors as [`build_narrator_system_prompt`]
/// (the Tracker prompt), but:
/// - Drops the entire `<bracket_commands>` block (`BRACKET_PROTOCOL`) — the
///   API narrator NEVER emits brackets. The tracker already tracked the turn.
/// - Swaps `narrator_core` for [`narrator_core_narrator`] (prose-only: the
///   four bracket references in `narrator_core` are reworded to instruct the
///   narrator to HONOR tracked state rather than EMIT brackets).
/// - Adds a `<your_role>` block right after `<narrator_role>` declaring the
///   voice-actor contract: authoritative state in, immersive prose out, no
///   invented outcomes, no bracket commands.
///
/// Everything else — `<scenario>`, `<player>`, `<world_state>` (now the
/// authoritative POST-tracker state), `<director_directive>`, `<scene_pacing>`,
/// `<active_reality>` (tail) — is identical to the Tracker prompt. The two
/// load-bearing anti-bias clauses (§11.29 anti-positivity-bias + §11.30.A
/// anti-Oblivion) are preserved verbatim inside `narrator_core_narrator`.
pub fn build_api_narrator_system_prompt(
    card: &SimCard,
    world_state: Option<&str>,
    scene_pacing: ScenePacing,
    directive: Option<&str>,
    memory_block: Option<&str>,
) -> String {
    let player_display = player_display_name(card);
    let player_address = player_address(card);

    let mut out = String::with_capacity(3072);

    out.push_str("<narrator_role>\n");
    out.push_str(&narrator_core_narrator(&player_display));
    out.push_str("\n</narrator_role>\n\n");

    // The voice-actor contract. Pins the API's job: it is the second stage of
    // a two-stage turn. The mechanical truth was already decided by the engine
    // + tracker BEFORE this prompt ran; <world_state> carries it. The API's
    // only job is to dress it in immersive prose — never to invent, override,
    // or expand on the mechanics.
    out.push_str("<your_role>\n");
    out.push_str(
        "You are the VOICE ACTOR — the second stage of a two-stage turn. The \
engine (a separate tracker) already decided the mechanical truth of this \
turn; <world_state> below carries it. Your ONLY job is to dress it in \
immersive second-person prose.\n\n\
- Narrate exactly what <world_state> + <directives> say happened — obey \
both. Nothing more, nothing less.\n\
- Do NOT invent outcomes the engine didn't track, and do NOT emit ANY \
bracket or JSON commands — your prose is the final output the player reads; \
any bracket syntax leaks as literal text.\n\
</your_role>\n\n");

    out.push_str("<scenario>\n");
    if let Some(setting) = card.setting.as_deref() {
        out.push_str("setting: ");
        out.push_str(setting.trim());
        out.push_str("\n\n");
    }
    if let Some(tone) = card.tone.as_deref() {
        out.push_str("tone: ");
        out.push_str(tone.trim());
        out.push_str("\n\n");
    }
    out.push_str("player: ");
    out.push_str(&player_display);
    out.push_str("\n\n");
    if !card.start_npc_ids.is_empty() {
        out.push_str("present_npcs: ");
        out.push_str(&card.start_npc_ids.join(", "));
        out.push_str("\n");
        // Prose-only: the API does not wrap dialogue in [CHARACTER_TURN] (the
        // tracker can't wrap speech that doesn't exist yet — it runs BEFORE
        // this narration). NPC dialogue flows as plain quoted prose per the
        // RP CONVENTIONS clause in narrator_core_narrator.
        out.push_str(
            "  (Each NPC above may speak. Write their dialogue inline as \
             quoted prose — do not wrap it in any bracket syntax.)\n",
        );
    }
    if !card.declared_activities.is_empty() {
        out.push_str("\nactivities_in_play: ");
        out.push_str(&card.declared_activities.join(", "));
        out.push_str("\n");
    }
    out.push_str("</scenario>\n\n");

    out.push_str("<player>\n");
    out.push_str(&player_contract(&player_address));
    out.push_str("\n</player>\n\n");

    // NOTE: no <bracket_commands> block. The API narrator never emits brackets.

    if let Some(state) = world_state {
        if !state.trim().is_empty() {
            out.push_str("<world_state>\n");
            out.push_str(state.trim());
            out.push_str("\n</world_state>\n\n");
        }
    }

    // Retrieved fable codex knowledge (2026-07-29): the deep playbook slice
    // the embedder judged relevant to THIS turn. Same injection as the tracker
    // prompt — one retrieval query serves both stages of the two-stage turn.
    // Surfaced under the codex frame; zero baseline cost when empty.
    if let Some(block) = memory_block {
        let trimmed = block.trim();
        if !trimmed.is_empty() {
            out.push_str("<retrieved_knowledge>\n");
            out.push_str(trimmed);
            out.push_str("\n</retrieved_knowledge>\n\n");
        }
    }

    if let Some(d) = directive {
        let trimmed = d.trim();
        if !trimmed.is_empty() {
            out.push_str("<director_directive>\n");
            out.push_str(trimmed);
            out.push_str("\n</director_directive>\n\n");
        }
    }

    out.push_str("<scene_pacing mode=\"");
    out.push_str(scene_pacing.mode.tag());
    out.push_str("\">\n");
    out.push_str(scene_pacing.mode.prose_guidance());
    out.push_str("\n</scene_pacing>\n\n");

    out.push_str("<active_reality>\n");
    out.push_str(&format!(
        "You are narrating {}, NOT any other scenario. ",
        card.name.trim(),
    ));
    if let Some(name) = card.player_name.as_deref() {
        let n = name.trim();
        if !n.is_empty() {
            out.push_str(&format!(
                "The player's name is {n}: use this name exclusively when an \
                 NPC addresses them directly; never invent or import a \
                 different player name. "
            ));
        } else {
            out.push_str(
                "The player has no set name. Refer to the player as \"you\" \
                 (second person); never invent a name for them. ",
            );
        }
    } else {
        out.push_str(
            "The player has no set name. Refer to the player as \"you\" \
             (second person); never invent a name for them. ",
        );
    }
    if let Some(setting) = card.setting.as_deref() {
        let brief: String = setting.trim().chars().take(160).collect();
        out.push_str(&format!("Setting recap: {brief}… "));
    }
    out.push_str(
        "Do NOT reference characters, locations, items, or elements from \
         any other scenario: only what belongs to this one.\n",
    );
    out.push_str("</active_reality>\n\n");

    out
}

// The player's display name as it should appear in prompt labels. Chloe
// 2026-07-27: removed the old "the player (unnamed)" bias wording. When
// the card carries no name, default to the literal "User" (per the standing
// anti-positivity-bias contract: never elevate the player with a titled
// default — they are a person in the world, not the center of it).
fn player_display_name(card: &SimCard) -> String {
    match card.player_name.as_deref() {
        Some(n) if !n.trim().is_empty() => n.trim().to_owned(),
        _ => "User".to_owned(),
    }
}

// The grammatical address the narrator uses for the player ("you" for
// second-person, or the player's name for named-address narration). The
// unnamed path stays "you" — that's correct grammar for second-person
// narration, not a bias marker.
fn player_address(card: &SimCard) -> String {
    match card.player_name.as_deref() {
        Some(n) if !n.trim().is_empty() => n.trim().to_owned(),
        _ => "you".to_owned(),
    }
}

fn narrator_core(player: &str) -> String {
    // The "never speak for" + narration-person lines adapt to whether we
    // have a real name (third-person, named) or are addressing the player
    // as "you" (second-person, unnamed). `player` is already resolved by
    // the caller to either the card name or the literal "User" — but for
    // GRAMMATICAL addressing we need to know if we're in the "you" mode,
    // which only happens when no name was set. We detect that by checking
    // against the sentinel "User" default (the unnamed case).
    let is_you = player == "User" || player == "you";
    let never_line = if is_you {
        "Never decide what you do, say, think, or feel".to_owned()
    } else {
        format!("Never decide what {player} does, says, thinks, or feels")
    };
    let person = if is_you { "second" } else { "third" };
    format!(
        "\
You are the SIMULATION NARRATIVE ENGINE — the invisible authoring intelligence \
that animates this world in prose. You are not a character, not a player, not \
an assistant.

WHAT YOU DO
- Portray the world: environment, weather, sounds, smells, the small details \
that make a scene feel lived-in.
- Portray NPCs: their observable behavior, reactions, and spoken dialogue. \
Wrap each spoken NPC line in [CHARACTER_TURN:npc_id] ... [CHARACTER_TURN:end].
- Drive the scene forward with tension, momentum, and meaningful choices.
- Track world state by emitting bracket commands (see <bracket_commands> + the \
retrieved playbook for the full reference). The engine deduplicates — \
overspecifying is cheap, underspecifying loses detail.
- End your turn the moment {player} needs to act.

LAWS (cold + mechanical — Rust enforces the dice; you narrate the result)
- The world is indifferent. NPCs act from their own nature; consequences are \
earned, not forgiven. The player can be refused, mocked, arrested, injured, \
or killed.
- A foe's resilience is a property of who they are, never flexed to match \
{player}. Declare a hostile NPC's tier via `npc.<id>.tier` (minion / soldier / \
elite / boss / legendary); Rust rolls wound severity + lethality from it. No \
tier declared = soldier.
- Player state in <world_state> is absolute truth (Rust computes it). A Heavy \
Injury to the right arm means no effective sword swing; Exhausted means slow \
and clumsy. Never have {player} perform beyond those limits, never heal.
- The `present:` line in <world_state> is the on-camera cast. Only NPCs listed \
there may speak, act, or be addressed in the scene this turn. An NPC who left \
is gone until you re-assert them with [PRESENCE]; do not summon them back.
- The <directives> Rust injects (lethality, disguise gate, skill checks, \
travel rejections, tick resolutions) are the mechanical truth of the turn. \
Obey them exactly in your prose.

WHAT YOU NEVER DO
- Never speak for {player}. {never_line}.
- Never write {player}'s dialogue, choices, or internal monologue.
- Never break the fourth wall or reference game mechanics, AI, prompts, or \
that this is a simulation.
- Never narrate in first person as any character.

PROSE
Tight beats, 2-5 sentences. Sensory detail over spectacle. Vary rhythm. Each \
beat leaves the next move to {player}. The deeper craft (pacing, the living \
world, RP formatting conventions) is in the retrieved playbook — consult it \
when the scene calls for it.

NARRATION PERSON
Narrate the world {person}-person. The narrator's camera follows {player}; \
NPCs address {player} by name when speaking to them directly."
    )
}


/// Prose-only variant of [`narrator_core`] for the API narrator stage of an
/// API-mode turn (§11.41). Identical body EXCEPT the four bracket references
/// in `narrator_core` are reworded: the API narrator is told to HONOR tracked
/// state, not EMIT brackets. The two load-bearing anti-bias clauses (§11.29
/// anti-positivity-bias + §11.30.A anti-Oblivion) are preserved VERBATIM —
/// they are bracket-free and equally load-bearing for the prose-only path.
///
/// The four bracket references in `narrator_core` and their prose-only
/// adaptations here:
/// - `WHAT YOU DO` CHARACTER_TURN bullet → dropped (the API writes dialogue as
///   plain quoted prose; it does not wrap speech in brackets).
/// - `WHAT YOU DO` OBJECT bullet → reworded to "honor the world-state you're
///   given" (the API does not emit `[OBJECT]`; it reads the tracked state).
/// - `THREAT TIERS` section → the `[OBJECT]` mechanism reference is removed;
///   the anti-Oblivion principle stays (tier is a property of the NPC, the
///   engine already applied it).
/// - `RP CONVENTIONS` CHARACTER_TURN bullet → dropped (no brackets to wrap in).
fn narrator_core_narrator(player: &str) -> String {
    let is_you = player == "User" || player == "you";
    let never_line = if is_you {
        "Never decide what you do, say, think, or feel".to_owned()
    } else {
        format!("Never decide what {player} does, says, thinks, or feels")
    };
    let person = if is_you { "second" } else { "third" };
    format!(
        "\
You are the SIMULATION NARRATIVE ENGINE — the voice-actor half of a two-stage \
turn. The engine (a separate tracker) already decided the mechanical truth of \
this turn; your job is to render it in prose. You are not a character, not a \
player, not an assistant.

WHAT YOU DO
- Portray the world: environment, weather, sounds, smells, the small details \
that make a scene feel lived-in.
- Portray NPCs: their observable behavior, reactions, and spoken dialogue. \
Write each spoken NPC line as plain quoted prose (\"I won't go in there.\"). \
Do NOT wrap dialogue in any bracket syntax.
- Drive the scene forward with tension, momentum, and meaningful choices.
- Honor <world_state>: the items, doors, NPC moods, trust levels, and clues \
it lists are ABSOLUTE — the engine already tracked them. Narrate consistently; \
do not invent, do not quietly drop.
- End your turn the moment {player} needs to act.

LAWS (cold + mechanical — Rust enforces the dice; you narrate the result)
- The world is indifferent. NPCs act from their own nature; consequences are \
earned, not forgiven. The player can be refused, mocked, arrested, injured, \
or killed.
- A foe's resilience is a property of who they are, never flexed to match \
{player}. The engine already classified each hostile's tier (minion / soldier \
/ elite / boss / legendary) and rolled severity from it — portray the \
consequence, never soften or inflate it to match {player}.
- Player state in <world_state> is absolute truth. A Heavy Injury to the \
right arm means no effective sword swing; Exhausted means slow and clumsy. \
Never have {player} perform beyond those limits, never heal.
- The `present:` line in <world_state> is the on-camera cast — the engine \
already filtered it. Only NPCs listed there may speak, act, or be addressed \
in the scene this turn. An NPC who left is gone; do not summon them back.
- The <directives> Rust injects (lethality, disguise gate, skill checks, \
travel rejections, tick resolutions) are the mechanical truth of the turn. \
Obey them exactly in your prose.

WHAT YOU NEVER DO
- Never speak for {player}. {never_line}.
- Never write {player}'s dialogue, choices, or internal monologue.
- Never break the fourth wall or reference game mechanics, AI, prompts, or \
that this is a simulation.
- Never narrate in first person as any character.
- Never emit bracket or JSON commands. The engine already tracked this turn; \
any bracket syntax you write will leak into the prose as literal text.
- Never decide game mechanics — granting buffs, advancing the clock, shifting \
NPC trust, resolving tasks. Those already happened this turn.

PROSE
Tight beats, 2-5 sentences. Sensory detail over spectacle. Vary rhythm. Each \
beat leaves the next move to {player}. The deeper craft (pacing, the living \
world, RP formatting conventions) is in the retrieved playbook — consult it \
when the scene calls for it.

NARRATION PERSON
Narrate the world {person}-person. The narrator's camera follows {player}; \
NPCs address {player} by name when speaking to them directly."
    )
}


// The `<player>` block: pins the player's authorship boundary + how the
// narrator should treat their input. Chloe 2026-07-27: the narration-person
// + never-speak-for grammar now lives in narrator_core's NARRATION PERSON +
// WHAT YOU NEVER DO sections (resolved there against the named/unnamed
// split), so this block is trimmed to the player-channel contract only —
// no duplication, no titled-default wording.
fn player_contract(player: &str) -> String {
    format!(
        "The player controls {player}. This is the player's one and only \
channel into the world.\n\n\
- When the player's input implies an action, narrate its diegetic \
consequences (how the world responds, what changes, who notices), not the \
action itself. Trust the player's stated intent; do not reinterpret it.\n\
- If the player attempts something impossible or rule-breaking, let the \
world push back naturally (an NPC refuses, a door holds, a guard frowns, \
a blade finds flesh) rather than refusing out-of-character. The world \
does not owe the player success."
    )
}

const BRACKET_PROTOCOL: &str = "\
Emit bracket commands alongside your prose to track world state. Thirteen \
recognized commands (full semantics + the JSON alternative form + common \
errors are in the retrieved playbook — consult it on mechanical turns). \
Each command on its own line, separate from prose. Any other bracket is \
invalid and leaks as literal text.

- [CHARACTER_TURN:npc_id] line [CHARACTER_TURN:end]  — an NPC spoke.
- [OBJECT id=<stable_id> state=<new_state>]  — a tracked detail changed.
- [FX <name>]  — scene effect (rain, snow, fog, flash, thunder, ...). Sparingly.
- [TIME <timestamp>]  — advance the clock when meaningful time passes. At most once/turn, at the END.
- [WEATHER <condition>]  — set the global atmosphere when it meaningfully changes.
- [TRAVEL <node_id>]  — move the player to an adjacent node.
- [RUMOR <diegetic phrase>]  — seed a rumor at the current location.
- [EFFECT <label> buff|debuff <minutes>]  — apply a status tag (optional kind=disguise).
- [MILESTONE <npc_id> <event_id>]  — record a relationship milestone.
- [TASK <npc_id> <desc> | <difficulty> <suitability> <eta_min>]  — queue an off-screen task.
- [PRESENCE <npc_id> <stance and micro-location>]  — assert who is on-camera now (one per present NPC).
- [DISCOVER <node_id> name=<diegetic name> setting=indoor|outdoor neighbors=<csv>]  — register a NEW location the story just established (a town, a ship, a dungeon room). Optional fields; `[DISCOVER shell_town name=\"Shell Town\"]` is enough.
- [NPC_REGISTER <npc_id> name=<diegetic name> role=<one-line hook> tier=<optional>]  — register a NEW named NPC who now matters (a companion, a recurring rival). Optional fields; `[NPC_REGISTER coby name=Coby role=\"timid Marine recruit\"]` is enough.

Schema-tracking commands (the eleven above except CHARACTER_TURN and FX) are \
the PRIMARY output — they record what mechanically changed. Ask first: \
\"What state changed this turn?\" Emit those. CHARACTER_TURN is secondary; \
many great turns have none. Ground every label in what actually happened — \
a diegetic phrase, not a snake_case id. One forward beat per turn; emit \
each command at most once, then yield to the player.

PRESENCE is special: emit one bracket per NPC physically in the scene this \
turn, every turn. Re-assert the full on-camera cast each turn (the whitelist \
refreshes). An NPC you do not re-assert drops after a grace turn — if they \
left the scene, that is correct; if they are still there, emit them again. \
Use the npc's id or alias as the first token; the stance is a short phrase \
of where they stand and what they are doing.

DISCOVER + NPC_REGISTER grow the world: emit them when the story establishes \
a NEW place or person the player can return to (a named town reached, a ship \
boarded, a notable NPC met who will recur). Emit DISCOVER once when a place \
first matters, then use [TRAVEL] to move there afterward. Emit NPC_REGISTER \
once when a character becomes a named, recurring presence, then [PRESENCE] \
asserts them each scene they're in. Re-discovering/re-registering is harmless \
(a no-op) — but emit each genuinely-new place and recurring person the FIRST \
turn they matter so [TRAVEL], [RUMOR], and [PRESENCE] can reach them.";


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{SceneMode, ScenePacing};

    /// Minimal roleplay card for prompt-construction tests. Most fields are
    /// empty — we only populate what `build_narrator_system_prompt` reads.
    fn test_card() -> SimCard {
        SimCard {
            id: "test".to_owned(),
            name: "Test Scenario".to_owned(),
            card_type: "roleplay".to_owned(),
            core_persona: String::new(),
            traits: String::new(),
            appearance: String::new(),
            role_instruction: String::new(),
            responsibilities: String::new(),
            conversational_rules: String::new(),
            technical_rules: String::new(),
            introductions: Vec::new(),
            setting: Some("A tavern at the edge of the world.".to_owned()),
            tone: Some("atmospheric".to_owned()),
            opening_scene: None,
            player_name: Some("Kaelen".to_owned()),
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
            locations: Vec::new(),
            cast: Vec::new(),
        }
    }

    fn pacing(mode: SceneMode) -> ScenePacing {
        ScenePacing {
            mode,
            spatial: 0,
            emotional: 0,
            kinetic: 0,
        }
    }

    // --- Scene pacing tag presence + correctness ---

    #[test]
    fn scene_pacing_tag_present_with_correct_mode() {
        let card = test_card();
        for mode in [SceneMode::Combat, SceneMode::Exploration, SceneMode::Downtime] {
            let p = build_narrator_system_prompt(&card, None, pacing(mode), None, None);
            let expected = format!("<scene_pacing mode=\"{}\">", mode.tag());
            assert!(
                p.contains(&expected),
                "prompt must contain scene_pacing tag for {:?}: missing {expected}",
                mode
            );
            // The prose guidance for this mode must also appear.
            assert!(
                p.contains(mode.prose_guidance()),
                "prompt must include the prose guidance for {:?}",
                mode
            );
        }
    }

    // --- Ordering invariant: scene_pacing BEFORE active_reality ---

    #[test]
    fn scene_pacing_precedes_active_reality() {
        // The recency-anchor design: <active_reality> stays the LAST block
        // (its cross-card-KV-contamination job depends on being the literal
        // tail). <scene_pacing> sits BEFORE it. Pin the ordering so a future
        // edit can't accidentally invert it.
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None, None);
        let sp = p.find("<scene_pacing").expect("scene_pacing tag missing");
        let ar = p.find("<active_reality>").expect("active_reality tag missing");
        assert!(
            sp < ar,
            "scene_pacing must come BEFORE active_reality (got scene_pacing at {sp}, active_reality at {ar})"
        );
    }

    #[test]
    fn active_reality_is_still_the_last_section() {
        // Pin that <active_reality> remains the final block in the prompt.
        // The closing tag </active_reality> should be the LAST </...> in the
        // rendered string (modulo trailing whitespace).
        let card = test_card();
        let p = build_narrator_system_prompt(&card, Some("gold: 100"), pacing(SceneMode::Exploration), None, None);
        let trimmed = p.trim_end();
        assert!(
            trimmed.ends_with("</active_reality>"),
            "active_reality must be the last block; prompt ends with: {:?}",
            trimmed.chars().rev().take(40).collect::<String>()
        );
    }

    // --- World_state still renders when provided ---

    #[test]
    fn world_state_renders_when_nonempty() {
        let card = test_card();
        let p = build_narrator_system_prompt(
            &card,
            Some("gold: 100\nstamina: Winded"),
            pacing(SceneMode::Exploration),
            None, None,
        );
        assert!(p.contains("<world_state>"));
        assert!(p.contains("gold: 100"));
    }

    #[test]
    fn world_state_omitted_when_empty() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, Some(""), pacing(SceneMode::Exploration), None, None);
        // The literal string "<world_state>" appears in narrator_core prose
        // (line 171) + BRACKET_PROTOCOL (line 236) as DOCUMENTATION, so we
        // can't just substring-check. Instead verify the actual block open/
        // close pair is NOT emitted (the empty-world_state skip path).
        assert!(
            !p.contains("<world_state>\n"),
            "empty world_state must not emit the <world_state> block"
        );
        assert!(
            !p.contains("</world_state>"),
            "empty world_state must not emit the closing </world_state> tag"
        );
        // scene_pacing is independent of world_state and must still render.
        assert!(p.contains("<scene_pacing"));
    }

    // --- Director directive (§11.24) ---

    #[test]
    fn director_directive_renders_when_nonempty() {
        let card = test_card();
        let p = build_narrator_system_prompt(
            &card,
            Some("gold: 100"),
            pacing(SceneMode::Exploration),
            Some("The barkeeper becomes visibly suspicious of the player."), None,
        );
        assert!(p.contains("<director_directive>"));
        assert!(p.contains("</director_directive>"));
        assert!(p.contains("barkeeper becomes visibly suspicious"));
    }

    #[test]
    fn director_directive_omitted_when_empty() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), Some("   "), None);
        assert!(
            !p.contains("<director_directive>"),
            "empty directive must not emit the block"
        );
        // scene_pacing must still render — the directive is independent.
        assert!(p.contains("<scene_pacing"));
    }

    #[test]
    fn director_directive_omitted_when_none() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None, None);
        assert!(
            !p.contains("<director_directive>"),
            "None directive must not emit the block"
        );
    }

    #[test]
    fn director_directive_precedes_scene_pacing() {
        // Ordering chain: <world_state> → <director_directive> → <scene_pacing>
        // → <active_reality>. Pin the directive BEFORE scene_pacing so a
        // future edit can't invert it.
        let card = test_card();
        let p = build_narrator_system_prompt(
            &card,
            Some("gold: 100"),
            pacing(SceneMode::Combat),
            Some("The barkeeper becomes suspicious."), None,
        );
        let dd = p.find("<director_directive>").expect("director_directive tag missing");
        let sp = p.find("<scene_pacing").expect("scene_pacing tag missing");
        assert!(
            dd < sp,
            "director_directive must come BEFORE scene_pacing (got directive at {dd}, scene_pacing at {sp})"
        );
    }

    #[test]
    fn director_directive_preceded_by_world_state() {
        let card = test_card();
        let p = build_narrator_system_prompt(
            &card,
            Some("gold: 100"),
            pacing(SceneMode::Combat),
            Some("The barkeeper becomes suspicious."), None,
        );
        let ws = p.find("<world_state>").expect("world_state tag missing");
        let dd = p.find("<director_directive>").expect("director_directive tag missing");
        assert!(
            ws < dd,
            "world_state must come BEFORE director_directive (got world_state at {ws}, directive at {dd})"
        );
    }

    #[test]
    fn director_directive_does_not_displace_active_reality_tail() {
        // The recency-anchor invariant: even with a directive armed,
        // <active_reality> must still be the literal last block.
        let card = test_card();
        let p = build_narrator_system_prompt(
            &card,
            Some("gold: 100"),
            pacing(SceneMode::Combat),
            Some("The barkeeper becomes suspicious."), None,
        );
        let trimmed = p.trim_end();
        assert!(
            trimmed.ends_with("</active_reality>"),
            "active_reality must still be last when directive armed; ends with: {:?}",
            trimmed.chars().rev().take(40).collect::<String>()
        );
    }

    // --- Slice 1 (2026-07-28, distilled 2026-07-29): the anti-bias thesis —
    // the world does not flex to flatter the player — is now a single cold
    // declarative LAW block, not two 200-word lectures. The distilled form
    // folds both registers (narrative outcomes + mechanical physics) into one
    // terse statement. Rust Referees (skill-check, combat+lethality,
    // relationship gates, disguise gate) MECHANICALLY enforce consequences via
    // dice regardless of model inclination — the prompt states the law once.
    // These tests pin the distilled law phrases so a future edit can't
    // silently drop the anti-bias thesis or re-bloat it into a lecture. ---

    #[test]
    fn anti_bias_distilled_laws_present() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None, None);
        // The distilled anti-indifference law: "The world is indifferent."
        // (replaces the §11.29 "THE WORLD DOES NOT REVOLVE" lecture). Consequences
        // are earned, not forgiven — the core anti-positivity thesis.
        assert!(
            p.contains("The world is indifferent"),
            "the distilled anti-indifference law must be present (was the §11.29 lecture)"
        );
        assert!(
            p.contains("consequences are earned, not forgiven"),
            "the consequences-earned-not-forgiven thesis must be present"
        );
        // The distilled anti-level-matching law: a foe's resilience never flexes
        // to match the player (replaces the §11.30 anti-Oblivion lecture).
        assert!(
            p.contains("never flexed to match"),
            "the anti-level-matching thesis must be present (was the §11.30 lecture)"
        );
        // The tier declaration mechanism must still be documented (the Rust
        // severity roll reads npc.<id>.tier entities).
        assert!(
            p.contains("npc.<id>.tier"),
            "the tier-declaration entity key must be documented"
        );
    }

    #[test]
    fn distilled_laws_reference_rust_enforcement() {
        // The distilled LAWS block explicitly hands the mechanical truth to
        // Rust: "Rust enforces the dice; you narrate the result" + "The
        // <directives> Rust injects ... are the mechanical truth." This is the
        // architectural discipline — the prompt does NOT lecture the model into
        // compliance; it states that Rust is the authority. Pin it.
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None, None);
        assert!(
            p.contains("Rust enforces the dice"),
            "the distilled laws must reference Rust's mechanical enforcement"
        );
        assert!(
            p.contains("mechanical truth"),
            "the distilled laws must frame <directives> as mechanical truth"
        );
    }


    // -----------------------------------------------------------------
    // §11.41 — DM / Voice-Actor split (NarratorMode). The API narrator
    // prompt is prose-only: no BRACKET_PROTOCOL, no bracket syntax docs at
    // all. These tests pin the split so a future edit can't accidentally
    // leak bracket instructions into the prose-only path (or drop them from
    // the tracker path).
    // -----------------------------------------------------------------

    #[test]
    fn api_narrator_prompt_has_no_bracket_protocol() {
        // The single most important invariant of the split: the API narrator
        // prompt must NOT contain the <bracket_commands> block or any of the
        // seven bracket commands' documentation. The API never emits brackets
        // — the tracker already tracked the turn. Any bracket syntax the API
        // emitted would leak into the prose as literal text.
        let card = test_card();
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None, None);
        assert!(
            !p.contains("<bracket_commands>"),
            "API narrator prompt must not contain the <bracket_commands> block"
        );
        assert!(
            !p.contains("</bracket_commands>"),
            "API narrator prompt must not contain the </bracket_commands> close tag"
        );
        // The seven bracket command docs must not appear.
        for needle in [
            "[CHARACTER_TURN:",
            "[OBJECT id=",
            "[FX ",
            "[TIME ",
            "[EFFECT ",
            "[MILESTONE ",
            "[TASK ",
        ] {
            assert!(
                !p.contains(needle),
                "API narrator prompt must not contain bracket doc {needle:?} (it never emits brackets)"
            );
        }
        // The JSON-alternative docs + the two CRITICAL bracket clauses must
        // also be absent (they're bracket-specific guidance).
        assert!(
            !p.contains("DO NOT INVENT BRACKETS"),
            "API narrator prompt must not carry the bracket 'DO NOT INVENT' clause"
        );
    }

    #[test]
    fn api_narrator_prompt_keeps_distilled_anti_bias_laws() {
        // The distilled LAWS block (replacing the §11.29 + §11.30.A lectures)
        // is bracket-free + equally load-bearing for the prose-only path.
        // The API narrator is just as bound by the anti-bias thesis as the
        // tracker — both fold into the same terse declarative laws. Pin the
        // distilled phrasing so it survives in the API prompt too.
        let card = test_card();
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None, None);
        assert!(
            p.contains("The world is indifferent"),
            "the distilled anti-indifference law must be present in the API narrator prompt"
        );
        assert!(
            p.contains("never flexed to match"),
            "the anti-level-matching law must be present in the API narrator prompt"
        );
        assert!(
            p.contains("Rust enforces the dice"),
            "the API narrator prompt must reference Rust's mechanical enforcement"
        );
    }


    #[test]
    fn api_narrator_prompt_carries_voice_actor_contract() {
        // The <your_role> block is the API's job description: authoritative
        // state in, immersive prose out, no invented outcomes, no brackets.
        // Pin its presence + the key contract phrases. (2026-07-29: the
        // 5-bullet lecture was distilled to 2 terse bullets — the assertions
        // pin the distilled phrasing: obey world_state+directives, no
        // invented outcomes + no bracket syntax in one clause.)
        let card = test_card();
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None, None);
        assert!(p.contains("<your_role>"));
        assert!(p.contains("</your_role>"));
        assert!(p.contains("VOICE ACTOR"));
        // The distilled <your_role> forbids bracket/JSON emission (one of two
        // terse bullets). The phrase spans a line-wrap in the source; assert
        // the stable contiguous fragment.
        assert!(p.contains("emit ANY"));
        assert!(p.contains("bracket or JSON commands"));
        assert!(p.contains("Do NOT invent outcomes"));
        assert!(p.contains("<world_state>"));
        assert!(p.contains("<directives>"));
    }

    #[test]
    fn api_narrator_prompt_keeps_world_state_and_director_directive() {
        // The API narrator consumes the authoritative POST-tracker state as
        // <world_state> and may receive a <director_directive>. Both blocks
        // must render in the prose-only prompt just as they do in the tracker
        // prompt.
        let card = test_card();
        let p = build_api_narrator_system_prompt(
            &card,
            Some("gold: 100\nstamina: Winded"),
            pacing(SceneMode::Exploration),
            Some("The barkeeper becomes visibly suspicious."), None,
        );
        assert!(p.contains("<world_state>"));
        assert!(p.contains("gold: 100"));
        assert!(p.contains("<director_directive>"));
        assert!(p.contains("barkeeper becomes visibly suspicious"));
    }

    #[test]
    fn api_narrator_active_reality_is_tail() {
        // The identity-anchor invariant from the tracker prompt carries over:
        // <active_reality> must remain the LAST block in the API narrator
        // prompt. Its cross-card-KV-contamination job depends on being the
        // literal tail.
        let card = test_card();
        let p = build_api_narrator_system_prompt(
            &card,
            Some("gold: 100"),
            pacing(SceneMode::Exploration),
            Some("A steer."), None,
        );
        let trimmed = p.trim_end();
        assert!(
            trimmed.ends_with("</active_reality>"),
            "API narrator prompt must end with </active_reality>; got: {:?}",
            trimmed.chars().rev().take(40).collect::<String>()
        );
    }

    #[test]
    fn api_narrator_scene_pacing_precedes_active_reality() {
        // Ordering invariant carries over: <scene_pacing> sits BEFORE
        // <active_reality> in the API narrator prompt.
        let card = test_card();
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None, None);
        let sp = p.find("<scene_pacing").expect("scene_pacing tag missing");
        let ar = p.find("<active_reality>").expect("active_reality tag missing");
        assert!(
            sp < ar,
            "scene_pacing must precede active_reality in the API narrator prompt"
        );
    }

    #[test]
    fn tracker_prompt_still_carries_bracket_protocol() {
        // Mirror of the above for the tracker path: build_narrator_system_prompt
        // (the Tracker/LOCAL prompt) must STILL carry the BRACKET_PROTOCOL —
        // the split must not accidentally strip brackets from the tracker.
        // (2026-07-29: the protocol was distilled to one line per command;
        // the full semantics + JSON form live in the fable.codex, retrieved
        // on-demand. The assertions pin the distilled form: the command list
        // header + the "any other bracket is invalid" guard.)
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None, None);
        assert!(p.contains("<bracket_commands>"));
        assert!(p.contains("[CHARACTER_TURN:"));
        assert!(p.contains("[OBJECT id="));
        assert!(p.contains("[TIME "));
        assert!(p.contains("[EFFECT "));
        assert!(p.contains("invalid and leaks as literal text"));
    }

    #[test]
    fn narrator_mode_enum_is_copy_and_comparable() {
        // NarratorMode is threaded by value through the prompt builders' call
        // sites; pin Copy + Eq so callers can compare/pass it cheaply.
        fn assert_copy_eq<T: Copy + std::cmp::Eq>() {}
        assert_copy_eq::<NarratorMode>();
        assert_eq!(NarratorMode::Tracker, NarratorMode::Tracker);
        assert_ne!(NarratorMode::Tracker, NarratorMode::Narrator);
    }
}