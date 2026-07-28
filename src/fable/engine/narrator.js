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

export function initNarrator(hooks = {}) {
  onTurnStart = hooks.onTurnStart || null;
  onTurnEnd = hooks.onTurnEnd || null;
  npcPretty = hooks.npcPretty || null;
  onSchemaPop = hooks.onSchemaPop || null;
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
}

// Send a narrator turn. `opts.silent` skips the user bubble (for system-
// driven turns; not currently used but reserved). `opts.regenerate` is the
// re-generation flag: when true, the backend SKIPS pushing a fresh user
// message and generates from the existing last user message in
// `fable_session` (the mutation commands — rerollLastTurn /
// rewindAndEditUser — left it there). We also skip the local `addUserBeat`
// on regenerate since the feed was already rebuilt by the mutation wrapper.
export async function sendFableTurn(text, opts = {}) {
  if (generating) return;
  generating = true;
  if (onTurnStart) onTurnStart();
  const regenerate = !!opts.regenerate;
  if (!opts.silent && !regenerate) beats.addUserBeat(text);

  const channel = new Channel();
  channel.onmessage = (msg) => handleEvent(msg);

  try {
    await invoke('fable_send', { text, onEvent: channel, regenerate });
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
  if (!activeBeat) activeBeat = beats.startNarratorBeat();
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
    activeBeat = null;
  } else if (finalText) {
    // No chunks arrived (edge case) but we have final prose — render it.
    const b = beats.startNarratorBeat();
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

// Regenerate the AI's last response. Pops the last assistant message,
// rebuilds the feed, then re-streams a fresh turn via `fable_send` with
// `regenerate: true` (the backend generates from the now-last user
// message without pushing a duplicate). schema_pop_count is 1 — the
// bad turn's world-state mutation is undone by the onSchemaPop hook.
export async function rerollLastTurn() {
  if (generating) return false;
  generating = true;
  if (onTurnStart) onTurnStart();
  let seedText = '';
  try {
    const res = await invoke('reroll_last_turn');
    if (res && Array.isArray(res.messages)) {
      beats.rebuildFromMessages(res.messages);
      // The seed for regeneration is the now-last user message (the reroll
      // popped the assistant reply that followed it). Grab its text so the
      // `fable_send` invoke below has a non-empty `text` arg (the backend
      // ignores it under regenerate=true, but it's the natural payload and
      // useful for any future logging).
      const lastUser = [...(res.messages || [])].reverse().find((m) => m.role === 'user');
      seedText = lastUser ? lastUser.content : '';
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
  } catch (err) {
    beats.addErrorBeat(String(err));
    finishTurn();
    return false;
  }
  // Hand off to the normal streaming path with regenerate=true. We DO NOT
  // call finishTurn() here — sendFableTurn owns the turn lifecycle from
  // this point (it set generating=true itself, fires onTurnStart, and
  // finishTurn runs in its done/error handler). Reset generating first so
  // sendFableTurn's `if (generating) return` guard doesn't bail.
  generating = false;
  await sendFableTurn(seedText, { regenerate: true });
  return true;
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
