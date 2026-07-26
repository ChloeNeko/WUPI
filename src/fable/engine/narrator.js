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

export function initNarrator(hooks = {}) {
  onTurnStart = hooks.onTurnStart || null;
  onTurnEnd = hooks.onTurnEnd || null;
  npcPretty = hooks.npcPretty || null;
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

// Send a narrator turn. `silent` skips the user bubble (for system-driven
// turns; not currently used but reserved).
export async function sendFableTurn(text, opts = {}) {
  if (generating) return;
  generating = true;
  if (onTurnStart) onTurnStart();
  if (!opts.silent) beats.addUserBeat(text);

  const channel = new Channel();
  channel.onmessage = (msg) => handleEvent(msg);

  try {
    await invoke('fable_send', { text, onEvent: channel });
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
