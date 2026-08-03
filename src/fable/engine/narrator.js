// =============================================================
// GAMES NARRATOR — the fable_send streaming loop.
//
// Owns the per-call Channel lifecycle for fable_send. Routes the 4
// channel event types (chunk / scene_event / done / error) to:
//   - beats.js       (dialogue feed rendering)
//   - fx/effects.js  (FX rendering)
//   - fx/atmosphere  (time/weather scan after the turn)
//
// Channel event shapes (the contract, verbatim from fable_send):
//   { type: 'chunk',       text }
//   { type: 'scene_event', command: { kind, ...} }
//   { type: 'done',        final_text, cancelled? }
//   { type: 'error',       message }
//
// command.kind values (snake_case via serde rename):
//   'character_turn' → { npc_id, line }
//   'object'         → { id, state }    → rendered as a system beat
//   'fx'             → { effect }       → playFX(effect)
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import * as beats from './beats.js';
import { playFX, clearFX } from '../fx/effects.js';
import { scanAndApply } from '../fx/atmosphere.js';

let activeBeat = null;     // the streaming narrator/character beat for the current turn
let generating = false;
let onTurnStart = null;    // hook: UI disables input
let onTurnEnd = null;      // hook: UI re-enables input
let npcPretty = null;      // optional fn: npcId → display name
let onSchemaPop = null;    // hook: (count) => void — the schema-ring-buffer
                           // consumer (the parallel fable_rollback work) wires
                           // here so the mutation commands below can hand off
                           // the pop count without narrator.js knowing about
                           // the schema layer.
let rerolling = false;     // true when the current turn is a swipeable-variant
                           // reroll (the last beat is being streamed over in
                           // place). Set in sendFableTurn({reroll:true}), read
                           // in onDone to update the beat's variant stamp +
                           // refresh the swipe controls.

// Identity for the message headers. cardName → narrator beats; playerName →
// user beats. Forwarded to beats.setIdentity so the builders pick them up.
let cardName = '';
let playerName = '';

export function initNarrator(hooks = {}) {
  onTurnStart = hooks.onTurnStart || null;
  onTurnEnd = hooks.onTurnEnd || null;
  npcPretty = hooks.npcPretty || null;
  onSchemaPop = hooks.onSchemaPop || null;
  if (typeof hooks.cardName === 'string') cardName = hooks.cardName;
  if (typeof hooks.playerName === 'string') playerName = hooks.playerName;
  // Mirror into beats so its builders (addUserBeat/startNarratorBeat) read the
  // same names. beats.setIdentity only overwrites fields it's handed.
  beats.setIdentity({ cardName, playerName });
}

// Hard reset of all module state (Chloe 2026-07-23: the resource-isolation
// audit). Called from teardownStage on stage exit so a close mid-turn can't
// leave `generating` stuck true (which would no-op the next session's first
// send via the `if (generating) return` guard) or leave `activeBeat`
// dangling at a beat the teardown already wiped from the feed. After this,
// the next initNarrator/sendFableTurn starts from a pristine module.
export function resetNarrator() {
  activeBeat = null;
  generating = false;
  rerolling = false;
}

// Send a narrator turn. Options:
//   opts.silent      — skip the user bubble (for system-driven turns; reserved).
//   opts.regenerate  — re-generation after rewind-and-edit: the backend skips
//                      pushing a fresh user message and generates from the
//                      existing last user message. We skip the local
//                      addUserBeat too (the feed was already rebuilt).
//   opts.reroll      — swipeable-variant reroll (2026-07-29): the last beat is
//                      an assistant turn whose old prose we KEEP as a swipeable
//                      sibling. We reuse that beat as `activeBeat` (clearing its
//                      body so the new variant streams in fresh) and tell the
//                      backend `reroll: true` so it stashes the old content into
//                      variants + installs the new prose as the active variant.
//                      No feed wipe, no flash — the new text streams in place.
export async function sendFableTurn(text, opts = {}) {
  if (generating) return;
  generating = true;
  if (onTurnStart) onTurnStart();
  const regenerate = !!opts.regenerate;
  const reroll = !!opts.reroll;
  if (!opts.silent && !regenerate && !reroll) beats.addUserBeat(text);

  // Reroll: claim the last assistant beat as the streaming target so the new
  // variant renders in place (the user watches the reroll happen over the old
  // text, no full feed rebuild). Clear its body; the new text streams in.
  if (reroll) {
    rerolling = true;
    activeBeat = beats.lastNarratorBeat();
    if (activeBeat) beats.beginReroll(activeBeat);
    else activeBeat = beats.startNarratorBeat();
  }

  const channel = new Channel();
  channel.onmessage = (msg) => handleEvent(msg);

  try {
    await invoke('fable_send', { text, onEvent: channel, regenerate, reroll });
  } catch (err) {
    beats.addErrorBeat(String(err));
    finishTurn();
  }
}

function handleEvent(msg) {
  if (!msg || typeof msg !== 'object') return;
  switch (msg.type) {
    case 'chunk':
      onChunk(msg.text);
      break;
    case 'scene_event':
      onSceneEvent(msg.command);
      break;
    case 'error':
      beats.addErrorBeat(msg.message || 'Generation failed.');
      finishTurn();
      break;
    case 'done':
      onDone(msg.final_text, msg.cancelled);
      break;
  }
}

function onChunk(text) {
  if (!text) return;
  if (!activeBeat) activeBeat = beats.startNarratorBeat({ name: cardName });
  beats.appendChunk(activeBeat, text);
}

function onSceneEvent(cmd) {
  if (!cmd || !cmd.kind) return;
  if (cmd.kind === 'fx') {
    playFX(cmd.effect);
    return;
  }
  if (cmd.kind === 'character_turn') {
    // Re-class the live narrator beat as a character beat.
    if (!activeBeat) activeBeat = beats.startNarratorBeat();
    const label = prettySpeaker(cmd.npc_id);
    beats.reclassToCharacter(activeBeat, label);
    if (cmd.line) beats.appendChunk(activeBeat, cmd.line);
    return;
  }
  if (cmd.kind === 'object') {
    // State change → a small system beat beneath the narration.
    const label = prettyObject(cmd.id, cmd.state);
    beats.addSystemBeat(label);
    return;
  }
}

function onDone(finalText, cancelled) {
  if (activeBeat) {
    beats.finalizeBeat(activeBeat, finalText);
    // After a reroll, this beat now has one more variant (the freshly-
    // generated one is active; the prior content is a swipeable sibling).
    // Update the stamp so refreshControls renders the ‹ N/N › bar. The prior
    // count is on the dataset (stamped at rebuild); the new active is the
    // new tail = old count.
    if (rerolling) {
      const prior = Number.parseInt(activeBeat.dataset.variantCount || '1', 10);
      const newCount = prior + 1;
      beats.stampVariants(activeBeat, new Array(newCount - 1).fill(''), newCount - 1);
    }
    activeBeat = null;
  } else if (finalText) {
    // No chunks arrived (edge case) but we have final prose — render it.
    const b = beats.startNarratorBeat({ name: cardName });
    beats.finalizeBeat(b, finalText);
  }
  // Scan the finalized prose for atmosphere cues (time/weather keywords).
  // Cheap: one pass over the text, two applies.
  scanAndApply(finalText || '', playFX, clearFX);
  finishTurn();
}

function finishTurn() {
  generating = false;
  activeBeat = null;
  rerolling = false;
  if (onTurnEnd) onTurnEnd();
}

export function isGenerating() { return generating; }

export async function stopFableTurn() {
  try { await invoke('fable_stop'); } catch (_) {}
}

// =============================================================
// UX CHAT CONTROLS — edit / reroll / rewind-and-edit.
//
// Each wrapper invokes the corresponding Tauri command, rebuilds the feed
// from the returned `messages[]` (the backend is the source of truth —
// the DOM is regenerated, not patched), and hands the `schema_pop_count`
// to the `onSchemaPop` hook so the parallel schema-ring-buffer work can
// keep world state aligned with the new timeline.
//
// Flow summary:
//   editMessage(idx, text)         → edit_message         → rebuild, NO regen
//   rerollLastTurn()               → reroll_last_turn     → rebuild + regen
//   rewindAndEditUser(idx, text)   → rewind_and_edit_user → rebuild + regen
//
// All three guard on `isGenerating()` — mutating mid-stream would collide
// with the live `activeBeat` (the streaming turn appends to it). edit
// blocks too because the round-trip shouldn't race a concurrent send.
// =============================================================

// In-place typo/content fix for either a user or assistant message. Does
// NOT re-trigger inference (per spec §1) and does NOT touch the schema
// (schema_pop_count is always 0, but we still hand it to onSchemaPop for
// contract symmetry — it's a no-op there).
export async function editMessage(index, newText) {
  if (generating) return false;
  generating = true;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('edit_message', { index, newText });
    if (res && Array.isArray(res.messages)) {
      beats.rebuildFromMessages(res.messages);
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
    return true;
  } catch (err) {
    beats.addErrorBeat(String(err));
    return false;
  } finally {
    finishTurn();
  }
}

// Regenerate the AI's last response as a SWIPEABLE VARIANT (2026-07-29).
// The old prose is kept as a sibling you can swipe back to; the new prose
// streams into the SAME beat in place (no feed wipe, no flash). The backend
// (`fable_send` with `reroll: true`) stashes the old content into the
// message's `variants` + installs the new prose as the active variant.
// schema_pop_count is 0 — a prose re-roll no longer undoes the turn's
// deterministic world-state mutation (the mechanics stand; only the prose
// varies). We do NOT rebuild the feed: `reroll_last_turn` is now a pure
// validation gate, and the variant bookkeeping happens entirely in the
// `sendFableTurn({ reroll: true })` call that follows.
export async function rerollLastTurn() {
  if (generating) return false;
  generating = true;
  if (onTurnStart) onTurnStart();
  // Validate via the gate command (checks the last message is an assistant
  // turn). No feed rebuild — the last beat stays on screen + gets streamed
  // over by the reroll.
  try {
    const res = await invoke('reroll_last_turn');
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
  } catch (err) {
    beats.addErrorBeat(String(err));
    finishTurn();
    return false;
  }
  // Hand off to the streaming path with reroll=true. Reset generating first
  // so sendFableTurn's `if (generating) return` guard doesn't bail.
  generating = false;
  await sendFableTurn('', { reroll: true });
  return true;
}

// Swipe to a different variant of an assistant message (the ‹ 1/N › UX).
// Backend swaps the active `content`/`raw_output` with the sibling at
// `variantIdx` + persists; we splice the new content into the beat body in
// place (mirror the §11.29 selective-regenerate splice — no feed rebuild).
// schema_pop_count is 0 (a swipe is a display change, not a world change).
export async function swipeVariant(index, variantIdx) {
  if (generating) return false;
  generating = true;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('swipe_variant', { index, variantIdx });
    if (res && Array.isArray(res.messages)) {
      // Splice the newly-active content into the target beat in place + update
      // the variant stamp so refreshControls (fired by finishTurn's onTurnEnd)
      // renders the bar at the new position.
      const msg = res.messages[index];
      if (msg) {
        beats.swapVariantBody(index, msg.content);
        // Re-stamp: the backend's select_variant updated active_idx; mirror it
        // onto the DOM. variants length unchanged.
        const feed = document.querySelector('[data-feed]');
        const beat = feed && feed.querySelector(`.fable-beat[data-index="${index}"]`);
        if (beat) beats.stampVariants(beat, msg.variants || [], msg.active_idx || 0);
      }
      if (onSchemaPop && typeof res.schema_pop_count === 'number') {
        onSchemaPop(res.schema_pop_count);
      }
    }
    return true;
  } catch (err) {
    beats.addErrorBeat(String(err));
    return false;
  } finally {
    finishTurn();
  }
}

// Branch the timeline: edit a user message from N turns ago. Truncates the
// conversation right after the target index, overwrites the target, rebuilds
// the feed, then re-streams a fresh turn for the newly edited timeline.
// schema_pop_count is N (count of assistant turns the truncation removed).
export async function rewindAndEditUser(index, newText) {
  if (generating) return false;
  generating = true;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('rewind_and_edit_user', { index, newText });
    if (res && Array.isArray(res.messages)) {
      beats.rebuildFromMessages(res.messages);
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
  } catch (err) {
    beats.addErrorBeat(String(err));
    finishTurn();
    return false;
  }
  // Regenerate from the edited user message (now the last message). Same
  // hand-off pattern as rerollLastTurn: reset generating + delegate to the
  // streaming path with regenerate=true.
  generating = false;
  await sendFableTurn(newText, { regenerate: true });
  return true;
}

// npc_id → display name. Cards declare start_npcs as ids; the model
// emits the same ids in [CHARACTER_TURN:id]. We prettify: "the_stranger"
// → "The Stranger". A passed-in npcPretty hook overrides (e.g. to map
// card-declared display names).
function prettySpeaker(npcId) {
  if (npcPretty) {
    const mapped = npcPretty(npcId);
    if (mapped) return mapped;
  }
  if (!npcId) return 'Someone';
  return npcId
    .split(/[_\s]+/)
    .filter(Boolean)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(' ');
}

// object id + state → a system beat line.
function prettyObject(id, state) {
  const niceId = (id || 'something').replace(/_/g, ' ');
  const niceState = (state || '?').replace(/_/g, ' ');
  return `— ${niceId}: ${niceState}`;
}
