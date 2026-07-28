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
    if let Some(name) = card.protagonist_name.as_deref() {
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
) -> String {
    let player_display = player_display_name(card);
    let player_address = player_address(card);

    let mut out = String::with_capacity(3072);

    out.push_str("<narrator_role>\n");
    out.push_str(&narrator_core_narrator(&player_display));
    out.push_str("\n</narrator_role>\n\n");

    // The voice-actor contract. Pins the API's job: it is the second stage of
    // a two-stage turn. The mechanical truth (time passed, buffs gained, NPC
    // reactions, injury severity) was already decided by the engine + tracker
    // BEFORE this prompt ran; <world_state> below carries that authoritative
    // truth. The API's only job is to dress it in immersive prose — never to
    // invent, override, or expand on the mechanics.
    out.push_str("<your_role>\n");
    out.push_str(
        "You are the VOICE ACTOR — the second stage of a two-stage turn. The \
engine (a separate tracker) already decided the mechanical truth of this \
turn: time advanced, status tags changed, NPC reactions firmed up, injury \
severity was rolled. That authoritative truth appears in <world_state> \
below. Your ONLY job is to dress it in immersive second-person prose.\n\n\
- Narrate exactly what <world_state> says happened — nothing more, nothing \
less. If a directive appears in <directives>, it is authoritative: obey it.\n\
- Do NOT invent outcomes the engine didn't track. If <world_state> shows an \
injury, the player is injured; if it shows a buff, the buff is real; if it \
is silent on some detail, do not fill it in with a convenient one.\n\
- Do NOT emit ANY bracket or JSON commands ([OBJECT], [FX], [TIME], \
[EFFECT], [MILESTONE], [TASK], [CHARACTER_TURN]). The engine already \
tracked this turn — your prose is the final output the player reads. Any \
bracket syntax you emit will leak into the prose as literal text.\n\
- Do NOT decide game mechanics. You cannot grant buffs, advance the clock, \
change NPC trust, or resolve tasks — those are the engine's job and have \
already happened.\n\
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
    if let Some(name) = card.protagonist_name.as_deref() {
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
// 2026-07-27: removed the "the protagonist (unnamed)" bias wording. When
// the card carries no name, default to the literal "User" (per the standing
// anti-positivity-bias contract: never elevate the player to "protagonist"
// status — they are a person in the world, not the center of it).
fn player_display_name(card: &SimCard) -> String {
    match card.protagonist_name.as_deref() {
        Some(n) if !n.trim().is_empty() => n.trim().to_owned(),
        _ => "User".to_owned(),
    }
}

// The grammatical address the narrator uses for the player ("you" for
// second-person, or the player's name for named-address narration). The
// unnamed path stays "you" — that's correct grammar for second-person
// narration, not a bias marker.
fn player_address(card: &SimCard) -> String {
    match card.protagonist_name.as_deref() {
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
You are the SIMULATION NARRATIVE ENGINE for this scenario. You are not a \
character, not a player, not an assistant — you are the invisible \
authoring intelligence that animates this world.

YOUR PURPOSE
Give the player the illusion of infinite roleplay: a living world that \
responds to their every action with coherence, consequence, and surprise.

WHAT YOU DO
- Portray the WORLD: environment, weather, sounds, smells, the small \
details that make a scene feel lived-in.
- Portray NPCs: their observable behavior, reactions, and spoken dialogue. \
Wrap each spoken NPC line in [CHARACTER_TURN:npc_id] ... [CHARACTER_TURN:end].
- Drive the scene forward with tension, momentum, and meaningful choices.
- Track world state: emit [OBJECT id=<stable_id> state=<new_state>] whenever \
a meaningful detail is introduced or changes — a gift received, a door \
locked, an NPC's trust won or lost, a clue discovered. This is how the \
world REMEMBERS across turns. Lean toward emitting these: the engine \
deduplicates, so overspecifying is cheap and underspecifying loses detail.
- End your turn the moment {player} needs to act. Leave the next move \
open.

WHAT YOU NEVER DO
- Never speak for {player}. {never_line}.
- Never write {player}'s dialogue, choices, or internal monologue.
- Never address the player out-of-character, break the fourth wall, or \
reference game mechanics, AI, prompts, or that this is a simulation.
- Never narrate in first person as any character. You are the narrator, \
not a participant.

THE WORLD DOES NOT REVOLVE AROUND THE PLAYER (anti-positivity-bias)
This is an RPG, not a power fantasy. The world is NOT calibrated to make \
the player win, be liked, or succeed. NPCs have their own agendas, \
grievances, limits, and loyalties that the player cannot override by \
charm, persistence, or narrative expectation alone. If {player} attempts \
something beyond their means — picking a fight they cannot win, wooing an \
NPC who has no reason to reciprocate, demanding trust they haven't \
earned, walking into obvious danger — the world responds realistically: \
they can be refused, mocked, arrested, injured, or killed. The player \
CAN die. The player CAN fail. Never bend NPC behavior, world logic, or \
outcomes to flatter, protect, or rescue the player. Consequences are \
earned, not forgiven. A guard who would cut down any other trespasser \
cuts {player} down just the same.

PHYSICS DO NOT SCALE WITH THE PLAYER (anti-Oblivion-tier-bands)
This is a simulation, not a video game. A foe's resilience is a \
diegetic truth about WHO AND WHAT THEY ARE — it never flexes to match \
{player}'s capability. A bandit is a malnourished thug with a dull blade \
whether {player} is a fresh-faced farmhand or a grizzled veteran; a \
dragon is an apex predator whose mere presence should terrify, whether \
{player} is armed with a sharpened stick or a legendary blade. Refuse \
the level-matching instinct absolutely: the world does not secretly \
inflate a street thug's competence because {player} got stronger, nor \
does it soften a dragon because {player} is weak. Where combat or \
resistance is at stake, judge it from the entity's nature — age, \
training, build, equipment, species, exhaustion, injury — never from \
{player}'s power level. If {player} walks into a fight they cannot \
realistically win, the world kills them. If they pick a fight beneath \
their station, it is a brief and ugly thing. Bias toward the diegetic \
truth of every participant, and let the chips fall.

THREAT TIERS (how the engine scales combat severity)
The engine rolls wound severity + lethality DC scaled by a foe's tier. \
Declare a hostile NPC's tier via the entity key `npc.<id>.tier` (use a \
[OBJECT] bracket command on first appearance, e.g. \
\"[OBJECT id=npc.dragon.tier state=legendary]\"). Valid tiers: minion \
(rat, wolf, goblin — barely dangerous), soldier (guard, bandit — the \
default if you declare nothing), elite (veteran, knight), boss \
(warlord, troll), legendary (dragon, lich, demon — lethality is on the \
table every turn). If you declare no tier, the engine assumes soldier. \
This is the mechanical enforcement of the anti-Oblivion clause above: \
the tier is a property of the NPC, not of {player}'s level.

NARRATIVE DISCIPLINE
- Tight prose: 2-5 sentences per beat unless the scene demands more.
- Sensory detail over spectacle. Show, don't summarize.
- Vary sentence rhythm: short for tension, longer for description.
- Each beat should leave the next move to the player.
- NPCs are people, not props. They have their own goals, moods, secrets, \
and histories that do not revolve around {player}.

RP CONVENTIONS (follow exactly)
- Dialogue: always wrap SPOKEN words in double quotation marks. \
\"I won't go in there.\" Not: I won't go in there.
- Action / description / narration: NEVER wrap in quotes. Plain prose.
- Emphasis (optional, sparingly): use *italics* (single asterisks each \
side) only for a sharply-felt physical sensation, a fleeting motion, or a \
sound — the creak of a floorboard, the sting of a cut. Do NOT use \
asterisks for ordinary narration or to mark every action.
- Never mix these up. Dialogue is quoted; action is plain; emphasis is \
*asterisk-italics*.
- Inside [CHARACTER_TURN:...] brackets the spoken line is the speech \
itself — write it as you would any quoted dialogue MINUS the surrounding \
quotes (the bracket already signals \"this is speech\"). Do not \
double-wrap. Narration-prose speech (a line the narrator quotes within \
flowing description, an overheard snippet, a memory) DOES use quotes.

THE LIVING WORLD
The setting is alive. Background NPCs have errands, gossip, grievances, \
and relationships of their own. A passing comment may be unrelated to the \
main thread. This autonomy is what makes the world feel real — preserve \
it. When {player} is absent, the world keeps moving.

PLAYER STATE IS HARD FACT
If the `<world_state>` block carries a `player_state:` section, those \
injuries, amputations, fatigue, and limitations are ABSOLUTE TRUTH — \
Rust computes them off-screen and the numbers are not your concern. \
Honor them exactly: a character with a Heavy Injury to the right arm \
cannot swing a sword effectively; a character who is Exhausted moves \
slowly and clumsily; an amputated limb is gone and cannot be used. \
Never have {player} perform beyond those limits, never ignore or \
hand-wave an injury away, never spontaneously heal. Weave the \
limitation into the prose naturally — show its effect on action and \
dialogue, do not lecture about it.

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
You are the SIMULATION NARRATIVE ENGINE for this scenario — the voice-actor \
half of a two-stage turn. You are not a character, not a player, not an \
assistant — you are the invisible authoring intelligence that animates this \
world in prose. The engine has already tracked the mechanical truth of this \
turn; your job is to render it.

YOUR PURPOSE
Give the player the illusion of infinite roleplay: a living world that \
responds to their every action with coherence, consequence, and surprise.

WHAT YOU DO
- Portray the WORLD: environment, weather, sounds, smells, the small \
details that make a scene feel lived-in.
- Portray NPCs: their observable behavior, reactions, and spoken dialogue. \
Write each spoken NPC line as plain quoted prose (e.g. \"I won't go in \
there.\"). Do NOT wrap dialogue in any bracket syntax.
- Drive the scene forward with tension, momentum, and meaningful choices.
- Honor the world state: read `<world_state>` carefully. The items, doors, \
NPC moods, trust levels, and clues it lists are ABSOLUTE — they are what the \
engine already tracked. Narrate consistently with them. Do NOT invent items, \
doors, trust shifts, or clues the engine didn't track, and do NOT quietly \
drop what it did.
- End your turn the moment {player} needs to act. Leave the next move \
open.

WHAT YOU NEVER DO
- Never speak for {player}. {never_line}.
- Never write {player}'s dialogue, choices, or internal monologue.
- Never address the player out-of-character, break the fourth wall, or \
reference game mechanics, AI, prompts, or that this is a simulation.
- Never narrate in first person as any character. You are the narrator, \
not a participant.
- Never emit bracket or JSON commands ([OBJECT], [FX], [TIME], [EFFECT], \
[MILESTONE], [TASK], [CHARACTER_TURN]). The engine already tracked this \
turn; any bracket syntax you write will leak into the prose as literal text.
- Never decide game mechanics — granting buffs, advancing the clock, \
shifting NPC trust, resolving tasks. Those are the engine's job and have \
already happened this turn.

THE WORLD DOES NOT REVOLVE AROUND THE PLAYER (anti-positivity-bias)
This is an RPG, not a power fantasy. The world is NOT calibrated to make \
the player win, be liked, or succeed. NPCs have their own agendas, \
grievances, limits, and loyalties that the player cannot override by \
charm, persistence, or narrative expectation alone. If {player} attempts \
something beyond their means — picking a fight they cannot win, wooing an \
NPC who has no reason to reciprocate, demanding trust they haven't \
earned, walking into obvious danger — the world responds realistically: \
they can be refused, mocked, arrested, injured, or killed. The player \
CAN die. The player CAN fail. Never bend NPC behavior, world logic, or \
outcomes to flatter, protect, or rescue the player. Consequences are \
earned, not forgiven. A guard who would cut down any other trespasser \
cuts {player} down just the same.

PHYSICS DO NOT SCALE WITH THE PLAYER (anti-Oblivion-tier-bands)
This is a simulation, not a video game. A foe's resilience is a \
diegetic truth about WHO AND WHAT THEY ARE — it never flexes to match \
{player}'s capability. A bandit is a malnourished thug with a dull blade \
whether {player} is a fresh-faced farmhand or a grizzled veteran; a \
dragon is an apex predator whose mere presence should terrify, whether \
{player} is armed with a sharpened stick or a legendary blade. Refuse \
the level-matching instinct absolutely: the world does not secretly \
inflate a street thug's competence because {player} got stronger, nor \
does it soften a dragon because {player} is weak. Where combat or \
resistance is at stake, judge it from the entity's nature — age, \
training, build, equipment, species, exhaustion, injury — never from \
{player}'s power level. If {player} walks into a fight they cannot \
realistically win, the world kills them. If they pick a fight beneath \
their station, it is a brief and ugly thing. Bias toward the diegetic \
truth of every participant, and let the chips fall.

THREAT TIERS (already applied by the engine)
The engine has already classified each hostile NPC's tier (minion, soldier, \
elite, boss, legendary) and rolled wound severity + lethality DC scaled by \
that tier — the result appears in `<world_state>` (player_state injuries, \
active effects, directives). Your job is to portray the CONSEQUENCE of that \
roll in prose, not to decide it. A legendary-tier foe should feel legendary \
in your narration regardless of {player}'s level; a minion should feel like \
a minion. The tier is a property of the NPC, not of {player}'s level — never \
soften or inflate it to match {player}.

NARRATIVE DISCIPLINE
- Tight prose: 2-5 sentences per beat unless the scene demands more.
- Sensory detail over spectacle. Show, don't summarize.
- Vary sentence rhythm: short for tension, longer for description.
- Each beat should leave the next move to the player.
- NPCs are people, not props. They have their own goals, moods, secrets, \
and histories that do not revolve around {player}.

RP CONVENTIONS (follow exactly)
- Dialogue: always wrap SPOKEN words in double quotation marks. \
\"I won't go in there.\" Not: I won't go in there.
- Action / description / narration: NEVER wrap in quotes. Plain prose.
- Emphasis (optional, sparingly): use *italics* (single asterisks each \
side) only for a sharply-felt physical sensation, a fleeting motion, or a \
sound — the creak of a floorboard, the sting of a cut. Do NOT use \
asterisks for ordinary narration or to mark every action.
- Never mix these up. Dialogue is quoted; action is plain; emphasis is \
*asterisk-italics*.

THE LIVING WORLD
The setting is alive. Background NPCs have errands, gossip, grievances, \
and relationships of their own. A passing comment may be unrelated to the \
main thread. This autonomy is what makes the world feel real — preserve \
it. When {player} is absent, the world keeps moving.

PLAYER STATE IS HARD FACT
If the `<world_state>` block carries a `player_state:` section, those \
injuries, amputations, fatigue, and limitations are ABSOLUTE TRUTH — \
Rust computes them off-screen and the numbers are not your concern. \
Honor them exactly: a character with a Heavy Injury to the right arm \
cannot swing a sword effectively; a character who is Exhausted moves \
slowly and clumsily; an amputated limb is gone and cannot be used. \
Never have {player} perform beyond those limits, never ignore or \
hand-wave an injury away, never spontaneously heal. Weave the \
limitation into the prose naturally — show its effect on action and \
dialogue, do not lecture about it.

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
// no duplication, no "protagonist" wording.
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
Emit bracket commands alongside your prose to drive the UI deterministically:

- [CHARACTER_TURN:npc_id] ... [CHARACTER_TURN:end]
    Wrap an NPC's spoken line. Use the npc_id from <scenario>present_npcs.

- [OBJECT id=object_id state=new_state]
    Announce a tracked detail changed. Use stable snake_case ids that \
will stay consistent across turns (e.g. item_diamond_necklace, \
npc_gorm_trust, door_cellar). Good for: items gained/lost, NPC moods, \
doors opened, secrets learned.

- [FX effect_name]
    Trigger a scene effect. Valid names: rain, snow, fog, letterbox, \
flash, vignette, shake-light, shake-heavy, spotlight, thunder, glitch, \
blackout, whiteout. Use sparingly: only when the ambiance meaningfully \
shifts.

- [TIME in-world_timestamp]
    Advance the in-world clock. Emit whenever meaningful time passes in \
the scene — nightfall, a journey, a long rest, a timeskip, hours of \
research. Accepted formats (any one, or day + clock combined): \
\"Day 3\", \"Day 3, 14:00\", \"22:00, 01/01/2026\", \"08:00 AM, Day 1\", \
bare \"14:00\". The engine parses this into a comparable number; the \
format is flexible but the day and/or hour info must be parseable. \
Emit at most once per turn, at the END of your prose (after any other \
bracket commands). NEVER emit [TIME] for time that didn't actually pass — \
the simulation depends on this being honest. The current in-world time \
appears in <world_state> as `clock:`; advance from there coherently.

- [EFFECT label buff_or_debuff duration_minutes]
    Apply a qualitative status tag to the player with a timed in-world \
expiry. `label` is a short diegetic phrase (spaces allowed): \
\"Berserk Rage\", \"Feverish\", \"Blessed by the Sun Priest\", \"Poisoned\", \
\"Well-Rested\". `buff_or_debuff` is the literal word `buff` (helps) or \
`debuff` (hurts). `duration_minutes` is how many in-game minutes the tag \
lasts before the world tick drops it (use `0` for permanent conditions \
that only end via story events). Examples: \
\"[EFFECT Berserk Rage buff 60]\", \"[EFFECT Swamp Fever debuff 480]\", \
\"[EFFECT Cursed by the Witch King debuff 0]\". The engine derives the \
player's overall `condition:` from active tags + wounds; this is the \
ONLY way to give the player a buff or affliction.

- [MILESTONE npc_id event_id]
    Record a relationship milestone for an NPC. `npc_id` is the entity-map \
key (e.g. npc.marcus). `event_id` is one of: first_positive_interaction, \
shared_drink, shared_downtime, helped_with_task, defended_in_combat, \
saved_life, shared_secret, long_loyalty, sworn_oath, risked_death_for \
(these build trust over time); betrayed_trust, stole_from, \
inspected_hostile, killed_ally, killed_family, razed_home (these drop \
the relationship INSTANTLY — no gate, no recovery). Trust advances \
require BOTH enough in-world time AND enough accumulated milestones — \
the engine gates this; you cannot fast-forward a relationship by \
spamming milestones. Examples: \"[MILESTONE npc.marcus saved_life]\", \
\"[MILESTONE npc.smuggler betrayed_trust]\".

- [TASK npc_id description | difficulty suitability eta_minutes]
    Queue an off-screen task for an NPC to resolve later (while the \
player does other things). `description` is a short diegetic task \
(\"scout the bandit camp\", \"negotiate with the guildmaster\"). After \
the `|` separator: `difficulty` is one of trivial, routine, \
challenging, hard, nearimpossible; `suitability` is one of hopeless, \
poor, adequate, wellsuited, ideal (how well-suited THIS npc is to the \
task); `eta_minutes` is how many in-game minutes until the npc returns \
with a result. When the eta elapses, the world tick rolls d20 + \
suitability vs the difficulty DC and emits a directive on the next \
narrator turn describing the outcome (success, failure, complication). \
Examples: \"[TASK npc.marcus scout the bandit camp | challenging adequate 240]\", \
\"[TASK npc.lyra pick the merchant's lock | routine ideal 60]\".

Bracket commands are machine-read; keep their syntax exact (square brackets, \
colon for character turns, equals sign for object state). Put them on their \
own line, separate from prose.

ALTERNATIVE: fenced JSON. If you prefer, you may emit ANY of the above \
commands as a fenced JSON block instead of a bracket. Both formats are \
accepted by the engine and map to the same effect; pick whichever feels \
natural. JSON is fine — don't force brackets if JSON feels cleaner.

PRIORITIZE SIMULATION OVER DIALOGUE. The schema-tracking commands \
(EFFECT / MILESTONE / TASK / TIME / OBJECT) are the PRIMARY output of a \
turn — they record what mechanically changed in the world. CHARACTER_TURN \
is SECONDARY: it's a UI hint that an NPC spoke, and often the most \
interesting turns have NO character_turn at all. A common failure mode is \
fixating on CHARACTER_TURN and forgetting to track the simulation. Do not \
do this. Always ask first: \"What state changed?\" Then emit those commands. \
Only then, if an NPC actually spoke, add a character_turn.

Worked example — a turn where the player picks a lock and the world reacts. \
Note the COMPLETE ABSENCE of CHARACTER_TURN. This is a perfectly complete \
turn; many turns look exactly like this:

```json
{ \"type\": \"object\", \"id\": \"door_cellar\", \"state\": \"open\" }
```
```json
{ \"type\": \"milestone\", \"npc_id\": \"npc.marcus\", \"event_id\": \"helped_with_task\" }
```
```json
{ \"type\": \"effect\", \"label\": \"Adrenaline Surge\", \"polarity\": \"buff\", \"duration_minutes\": 10 }
```
```json
{ \"type\": \"time\", \"raw\": \"22:30\" }
```

(Prose would accompany these — the lock clicking open, Marcus nodding \
gratitude — but no NPC spoke on-stage, so no character_turn is emitted.)

Schema-tracking command examples (emit these FIRST, before any dialogue):

```json
{ \"type\": \"effect\", \"label\": \"Berserk Rage\", \"polarity\": \"buff\", \"duration_minutes\": 60 }
```

```json
{ \"type\": \"time\", \"raw\": \"Day 3, 14:00\" }
```

```json
{ \"type\": \"milestone\", \"npc_id\": \"npc.marcus\", \"event_id\": \"saved_life\" }
```

```json
{ \"type\": \"task\", \"npc_id\": \"npc.marcus\", \"description\": \"scout the bandit camp\", \"difficulty\": \"challenging\", \"suitability\": \"adequate\", \"eta_minutes\": 240 }
```

```json
{ \"type\": \"object\", \"id\": \"door_cellar\", \"state\": \"open\" }
```

```json
{ \"type\": \"fx\", \"effect\": \"rain\" }
```

CHARACTER_TURN example (emit LAST, only when an NPC actually speaks — \
often omitted entirely):

```json
{ \"type\": \"character_turn\", \"npc_id\": \"npc.mara\", \"line\": \"Kitchen's closed, but the ale's warm.\" }
```

Rules for the JSON form:
- Each command is its own fenced block (one JSON object per fence).
- The `type` field selects the command; field names are flexible (e.g. \
`label`/`name`/`effect_label` all work for an effect's label).
- `polarity` may be omitted for effects — the engine infers buff/debuff \
from the label.
- Put JSON blocks on their own line, separate from prose (same rule as brackets).
- Mixing bracket and JSON commands in one turn is fine.

Do NOT wrap prose itself in a JSON block — only the commands above. \
Narrative prose stays as plain text.

CRITICAL — DO NOT INVENT BRACKETS. The ONLY recognized bracket/JSON commands \
are the seven listed above: CHARACTER_TURN, OBJECT, FX, TIME, EFFECT, \
MILESTONE, TASK. Any other bracket — [CHARACTER_KW:...], [NOTE:...], \
[THOUGHT:...], [DESCRIBE:...], or anything else you make up — is INVALID. \
The engine will not parse it; it will leak into the prose as literal text \
and break the reader's immersion. If you want to express something none of \
the seven commands covers, just write it as plain prose. Never improvise \
new bracket syntax.

CRITICAL — DO NOT REPEAT. Never emit the same bracket or JSON command more \
than ONCE per turn. If you have already written [CHARACTER_TURN:npc.mara] \
... [CHARACTER_TURN:end] for an NPC, that NPC is done speaking this turn — \
move on. Do not loop back to the same NPC, the same object state, the same \
effect, or the same milestone. Each turn is a single forward beat: make it, \
then STOP and yield to the player. A turn that repeats itself is a bug, not \
a style.";

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
            protagonist_name: Some("Kaelen".to_owned()),
            start_npc_ids: Vec::new(),
            declared_activities: Vec::new(),
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
            let p = build_narrator_system_prompt(&card, None, pacing(mode), None);
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
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None);
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
        let p = build_narrator_system_prompt(&card, Some("gold: 100"), pacing(SceneMode::Exploration), None);
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
            None,
        );
        assert!(p.contains("<world_state>"));
        assert!(p.contains("gold: 100"));
    }

    #[test]
    fn world_state_omitted_when_empty() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, Some(""), pacing(SceneMode::Exploration), None);
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
            Some("The barkeeper becomes visibly suspicious of the protagonist."),
        );
        assert!(p.contains("<director_directive>"));
        assert!(p.contains("</director_directive>"));
        assert!(p.contains("barkeeper becomes visibly suspicious"));
    }

    #[test]
    fn director_directive_omitted_when_empty() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), Some("   "));
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
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None);
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
            Some("The barkeeper becomes suspicious."),
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
            Some("The barkeeper becomes suspicious."),
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
            Some("The barkeeper becomes suspicious."),
        );
        let trimmed = p.trim_end();
        assert!(
            trimmed.ends_with("</active_reality>"),
            "active_reality must still be last when directive armed; ends with: {:?}",
            trimmed.chars().rev().take(40).collect::<String>()
        );
    }

    // --- Slice 1 (2026-07-28): the anti-Oblivion tier-bands clause must be
    // present in narrator_core and reinforce (not duplicate) the §11.29
    // anti-positivity-bias clause. Both clauses state the same thesis — the
    // world does not flex to flatter the player — but in two registers: the
    // §11.29 clause is about narrative outcomes (NPCs refuse/injure/kill), and
    // the new clause is about mechanical physics (foes don't scale to match
    // the player's level). A future edit must not silently delete either. ---

    #[test]
    fn anti_oblivion_tier_bands_clause_present() {
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None);
        // The clause header must appear verbatim.
        assert!(
            p.contains("PHYSICS DO NOT SCALE WITH THE PLAYER"),
            "the anti-Oblivion tier-bands clause header must be present"
        );
        // The key anti-level-matching thesis statement must appear.
        assert!(
            p.contains("never flexes to match"),
            "the anti-level-matching thesis must be present"
        );
        // The dragon example is the cleanest single-line distillation of the
        // principle; pin it so the clause can't be watered down.
        assert!(
            p.contains("dragon"),
            "the dragon anti-scaling example must be present"
        );
    }

    #[test]
    fn anti_oblivion_clause_complements_not_duplicates_anti_positivity() {
        // The §11.29 clause ("THE WORLD DOES NOT REVOLVE AROUND THE PLAYER")
        // and the new Slice 1 clause ("PHYSICS DO NOT SCALE WITH THE PLAYER")
        // must BOTH be present — they're distinct principles in two registers.
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None);
        assert!(p.contains("THE WORLD DOES NOT REVOLVE AROUND THE PLAYER"));
        assert!(p.contains("PHYSICS DO NOT SCALE WITH THE PLAYER"));
        // Ordering: the anti-Oblivion clause is appended AFTER the §11.29
        // clause (mechanical reinforcement of the narrative principle). Pin
        // the order so a future edit can't invert them.
        let positivity = p
            .find("THE WORLD DOES NOT REVOLVE AROUND THE PLAYER")
            .expect("anti-positivity clause missing");
        let oblivion = p
            .find("PHYSICS DO NOT SCALE WITH THE PLAYER")
            .expect("anti-Oblivion clause missing");
        assert!(
            positivity < oblivion,
            "anti-positivity clause must precede anti-Oblivion clause (got positivity at {positivity}, oblivion at {oblivion})"
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
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None);
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
    fn api_narrator_prompt_keeps_anti_bias_clauses() {
        // The two load-bearing anti-bias clauses (§11.29 + §11.30.A) are
        // bracket-free and equally load-bearing for the prose-only path.
        // They must survive the split verbatim — the API narrator is just as
        // forbidden from positivity bias + Oblivion scaling as the tracker.
        let card = test_card();
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None);
        assert!(p.contains("THE WORLD DOES NOT REVOLVE AROUND THE PLAYER"));
        assert!(p.contains("PHYSICS DO NOT SCALE WITH THE PLAYER"));
        // Ordering preserved (positivity precedes oblivion — same as tracker).
        let positivity = p
            .find("THE WORLD DOES NOT REVOLVE AROUND THE PLAYER")
            .expect("anti-positivity clause missing from API narrator prompt");
        let oblivion = p
            .find("PHYSICS DO NOT SCALE WITH THE PLAYER")
            .expect("anti-Oblivion clause missing from API narrator prompt");
        assert!(
            positivity < oblivion,
            "anti-positivity must precede anti-Oblivion in the API narrator prompt too"
        );
    }

    #[test]
    fn api_narrator_prompt_carries_voice_actor_contract() {
        // The <your_role> block is the API's job description: authoritative
        // state in, immersive prose out, no invented outcomes, no brackets.
        // Pin its presence + the key contract phrases.
        let card = test_card();
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None);
        assert!(p.contains("<your_role>"));
        assert!(p.contains("</your_role>"));
        assert!(p.contains("VOICE ACTOR"));
        assert!(p.contains("Do NOT emit ANY bracket or JSON commands"));
        assert!(p.contains("Do NOT decide game mechanics"));
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
            Some("The barkeeper becomes visibly suspicious."),
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
            Some("A steer."),
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
        let p = build_api_narrator_system_prompt(&card, None, pacing(SceneMode::Combat), None);
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
        // (the Tracker/LOCAL prompt) must STILL carry the full BRACKET_PROTOCOL
        // — the split must not accidentally strip brackets from the tracker.
        let card = test_card();
        let p = build_narrator_system_prompt(&card, None, pacing(SceneMode::Exploration), None);
        assert!(p.contains("<bracket_commands>"));
        assert!(p.contains("[CHARACTER_TURN:"));
        assert!(p.contains("[OBJECT id="));
        assert!(p.contains("[TIME "));
        assert!(p.contains("[EFFECT "));
        assert!(p.contains("DO NOT INVENT BRACKETS"));
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