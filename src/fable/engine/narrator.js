// =============================================================
// GAMES NARRATOR — the fable_send streaming loop.
//
// Owns the per-call Channel lifecycle for fable_send. Routes the 4
// channel event types (chunk / scene_event / done / error) to:
//   - beats.js       (dialogue feed rendering)
//   - fx/effects.js  (FX rendering — explicit [FX] brackets only)
//
// Channel event shapes (the contract, verbatim from fable_send):
//   { type: 'chunk',       text }
//   { type: 'scene_event', command: { kind, ...} }
//   { type: 'done',        final_text, reasoning?, cancelled? }
//   { type: 'error',       message }
//
// command.kind values (snake_case via serde rename):
//   'character_turn' → { npc_id, line }
//   'object'         → { id, state }    → rendered as a system beat
//   'fx'             → { effect }       → playFX(effect)
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import * as beats from './beats.js';
import { playFX, clearFX } from '../fx/effects.js';

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
let onApiLost = null;      // hook: (message) => void — fires on the `api_lost`
                           // event (2026-08-07 override): the API narrator died
                           // mid-session and there's no local fallback. stage.js
                           // wires this to lock the composer with the red "API
                           // LOST CONNECTION" state + surface a retry affordance.
let rerolling = false;     // true when the current turn is a swipeable-variant
                           // reroll (the last beat is being streamed over in
                           // place). Set in sendFableTurn({reroll:true}), read
                           // in onDone to update the beat's variant stamp +
                           // refresh the swipe controls.
// Stage 3 (2026-08-11): armed by interruptAndReroll when the user re-presses ›
// mid-reroll. The backend's abort path emits a `cancelled` event once the
// in-flight roll is discarded + the schema reverted to base; onCancelled
// consumes this flag to fire the fresh reroll. Cleared on any turn end.
let deferredReroll = false;

// The unlisten handle for the `fable-session-changed` Tauri event (set up in
// initNarrator, torn down in resetNarrator). Captured at module scope so re-
// entry into the Fable stage cleans up the prior listener before registering
// a new one (otherwise multiple listeners accumulate across stage entries).
let sessionChangedUnlisten = null;
// Optional hook fired when a chat-side `fable_schema_patch` tool mutates the
// live schema (the HUD may refresh its Soul Gem panel immediately). Receives
// the list of merged top-level field names.
let onSchemaPatch = null;

// Identity for the message headers. cardName → narrator beats; playerName →
// user beats. Forwarded to beats.setIdentity so the builders pick them up.
let cardName = '';
let playerName = '';
// Portrait identity for the VN chat (Phase 2 bridge). Each is an asset:// URL
// (or '' when absent). cardPortrait is the narrator portrait AND the NPC
// fallback (per-NPC portraits are deferred). playerPortrait is the user side.
let cardPortrait = '';
let playerPortrait = '';

export function initNarrator(hooks = {}) {
  onTurnStart = hooks.onTurnStart || null;
  onTurnEnd = hooks.onTurnEnd || null;
  npcPretty = hooks.npcPretty || null;
  onSchemaPop = hooks.onSchemaPop || null;
  onApiLost = hooks.onApiLost || null;
  onSchemaPatch = hooks.onSchemaPatch || null;
  if (typeof hooks.cardName === 'string') cardName = hooks.cardName;
  if (typeof hooks.playerName === 'string') playerName = hooks.playerName;
  if (typeof hooks.cardPortrait === 'string') cardPortrait = hooks.cardPortrait;
  if (typeof hooks.playerPortrait === 'string') playerPortrait = hooks.playerPortrait;
  // Mirror into beats so its builders (addUserBeat/startNarratorBeat) read the
  // same identity. beats.setIdentity only overwrites fields it's handed.
  beats.setIdentity({
    cardName, playerName,
    cardPortrait, playerPortrait,
    npcNames: hooks.npcNames instanceof Map ? hooks.npcNames : new Map(),
  });
  // Wire the fable-session-changed listener. The backend emits this when a
  // chat-side stateful tool (fable_message_edit/delete/fable_schema_patch,
  // dispatched from run_agent_loop) mutates the active Fable session's live
  // state. Two payload kinds:
  //   - { kind: 'messages', messages: [...] } → rebuild the dialogue feed
  //     (the operator asked WUPI to edit/delete a roleplay message via chat).
  //   - { kind: 'schema', merged_keys: [...] } → refresh the Soul Gem panel /
  //     HUD (the operator asked WUPI to patch world/player/npc state).
  // Re-entry safe: any prior listener is torn down before the new one is set
  // up (initNarrator runs on every Fable stage entry).
  if (sessionChangedUnlisten) {
    try { sessionChangedUnlisten(); } catch (_) { /* already torn down */ }
    sessionChangedUnlisten = null;
  }
  listen('fable-session-changed', (e) => {
    const payload = e?.payload || {};
    if (payload.kind === 'messages' && Array.isArray(payload.messages)) {
      beats.rebuildFromMessages(payload.messages);
    } else if (payload.kind === 'schema') {
      // The HUD (left-drawer) reads schema on its own refresh cadence; the
      // turn-end hook already calls refreshAll. We hook here so a future
      // immediate-refresh path can wire in without touching narrator again.
      if (typeof onSchemaPatch === 'function') {
        try { onSchemaPatch(payload.merged_keys || []); } catch (_) {}
      }
    }
  }).then((un) => { sessionChangedUnlisten = un; })
    .catch((err) => { /* listener setup failed; non-fatal */ });
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
  // Tear down the session-changed listener — the stage is exiting, no more
  // chat-side mutations should rebuild the (now-hidden) feed.
  if (sessionChangedUnlisten) {
    try { sessionChangedUnlisten(); } catch (_) {}
    sessionChangedUnlisten = null;
  }
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
    case 'api_lost':
      // 2026-08-07 override: the API narrator died mid-session and there's no
      // local fallback. The backend already autosaved + cleared the cancel
      // slot; the turn aborts without a narrator beat. Lock the composer via
      // the onApiLost hook so the player reconnects via Settings + retries.
      onApiLost(msg.message || 'The API connection was lost.');
      finishTurn();
      break;
    case 'cancelled':
      onCancelled();
      break;
    case 'done':
      onDone(msg.final_text, msg.reasoning, msg.cancelled);
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
    // NPC portrait resolution: per-NPC portraits are deferred, so every NPC
    // falls back to the card portrait (the narrator sprite). reclassToCharacter
    // accepts an optional portrait URL (3rd arg) for this.
    beats.reclassToCharacter(activeBeat, label, cardPortrait);
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

function onDone(finalText, reasoning, cancelled) {
  // `reasoning` is unused post-2026-08-07 override (the API narrator never
  // emits a thought channel + the player-facing reasoning UI was removed).
  void reasoning;
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
      // Stamp the new count + active index (the fresh variant is the new tail).
      // variantCount(variants) == variants.length post-fix, so the synthetic
      // array's length must equal newCount (was newCount-1 under the old +1).
      beats.stampVariants(activeBeat, new Array(newCount).fill(''), newCount - 1);
    }
    activeBeat = null;
  } else if (finalText) {
    // No chunks arrived (edge case) but we have final prose — render it.
    const b = beats.startNarratorBeat({ name: cardName });
    beats.finalizeBeat(b, finalText);
  }
  finishTurn();
}

function finishTurn() {
  generating = false;
  activeBeat = null;
  rerolling = false;
  if (onTurnEnd) onTurnEnd();
}

export function isGenerating() { return generating; }
export function isRerolling() { return rerolling; }

export async function stopFableTurn() {
  try { await invoke('fable_stop'); } catch (_) {}
}

// Stage 3 (2026-08-11): the drawer's › interrupt — the user re-pressed ›
// mid-reroll to abandon the in-flight roll + start a fresh one. Arms the
// deferred reroll, then signals the backend to abort (cancel decode + set the
// discard-and-revert flag). The backend's fable_send abort path discards the
// partial, reverts the schema to the pre-turn base, emits `cancelled`; this
// module's onCancelled consumes the flag + fires the fresh reroll. No-op if
// no reroll is in flight (a normal turn keeps its existing stop gesture).
export async function interruptAndReroll() {
  if (!generating || !rerolling) return;
  deferredReroll = true;
  try {
    await invoke('fable_interrupt_reroll');
  } catch (_) {
    deferredReroll = false;
  }
}

// The `cancelled` event handler (Stage 3): the in-flight roll was discarded
// by the backend's abort path. Clear the partial from the streaming beat +
// start the deferred reroll (re-streams over the trailing beat, replacing the
// aborted partial). If no reroll was armed (defensive — shouldn't happen since
// `cancelled` is only emitted by the abort path), just finalize the turn.
function onCancelled() {
  if (activeBeat) {
    // beginReroll clears the aborted partial + preps the beat for the fresh
    // stream (so nothing lingers during the reroll's IPC round-trip).
    beats.beginReroll(activeBeat);
  }
  finishTurn();
  if (deferredReroll) {
    deferredReroll = false;
    // generating was just cleared by finishTurn, so rerollLastTurn's guard passes.
    rerollLastTurn();
  }
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

// Delete a single message by index. Permanently removes the message + shifts
// the tail down — the same primitive the model-facing fable_message_delete
// tool uses (Conversation::remove_at). No inference, no schema change
// (schema_pop_count is 0). Rebuilds the feed from the returned messages[].
// Destructive (no conversation-undo), so the drawer gates it behind a
// two-step inline confirm.
export async function deleteMessage(index) {
  if (generating) return false;
  generating = true;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('delete_message', { index });
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
        const beat = feed && feed.querySelector(`.fable-mes[data-index="${index}"]`);
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
