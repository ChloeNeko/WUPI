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
// Every other kind ('weather', 'time', 'npc_item', 'mood', 'intent', …)
// is world-sim machinery with no live UI — silently ignored below.
// =============================================================

import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import * as beats from './beats.js';
import { playFX, clearFX } from '../fx/effects.js';
// (2026-08-22) Turn notices — the referee's injuries + the tracker's
// automatic inventory moves render as top-left bubbles the moment they land.
import { showTurnNotice } from './turn-notices.js';
// (2026-08-16 yellow J2) hidePencil — every feed rebuild dissolves the
// DOM selection the pencil anchors to; without this the pencil floats at a
// stale viewport position until the next selection event.
import { hidePencil } from './slice-regen.js';

let activeBeat = null;     // the streaming narrator/character beat for the current turn
let generating = false;
let onTurnStart = null;    // hook: UI disables input
let onTurnEnd = null;      // hook: UI re-enables input
// Hook: () => void — fires on the first REAL narrator output of a turn
// (stream chunk / character_turn line). The typing indicator must key off
// this, NOT onTurnStart: Stage 1 (the hidden local tracker) runs silently
// before any chunk, and showing "X is typing..." over it made the tracker
// read as a second narrator turn (2026-08-22 Chloe ruling).
let onNarratorActive = null;
let npcPretty = null;      // optional fn: npcId → display name
let onSchemaPop = null;    // hook: (count) => void — the schema-ring-buffer
                           // consumer (the parallel fable_rollback work) wires
                           // here so the mutation commands below can hand off
                           // the pop count without narrator.js knowing about
                           // the schema layer.
let onApiLost = null;      // hook: (message) => void — fires on the `api_lost`
                           // event (2026-08-07 override): the API narrator died
                           // mid-session and there is no local fallback. stage.js
                           // wires this to lock the composer with the red "API
                           // LOST CONNECTION" state + surface a retry affordance.
let onTrackerSkipped = null; // hook: () => void — fires on the `tracker_skipped`
                           // event (2026-08-21): the local tracker pass was
                           // skipped for being over budget even after the
                           // tail-drop + core-tier degrade — world-state
                           // tracking is OFF this turn. De-duped to once per
                           // session (the backend emits every occurrence; one
                           // warning chip is enough).
let trackerSkipWarned = false;
let rerolling = false;     // true when the current turn is a swipeable-variant
                           // reroll (the last beat is being streamed over in
                           // place). Set in sendFableTurn({reroll:true}), read
                           // in onDone to update the beat's variant stamp +
                           // refresh the swipe controls.
// (2026-08-16 audit fix #8) The claimed beat's pre-reroll prose, captured
// BEFORE beginReroll wipes the body. A cancelled/aborted roll must restore
// THIS text — the backend already re-installed the prior variant server-side;
// the old wipe left the beat blank with a live streaming caret until some
// later feed rebuild.
let rerollPrevText = null;
// (2026-08-16 audit fix #5) The turn-start user bubble. Every backend revert
// path (soft cancel, api_lost, dev-narrator error) pops the turn-start user
// message server-side — the DOM must match or the Enter-retry renders the
// action twice + shifts every feed index vs the session.
let uncommittedUserBeat = null;
// The typed action of the in-flight NORMAL turn ('' on reroll/regenerate).
// Handed back via onTurnEnd({revertedText}) when the turn reverts so the
// composer restore can resurrect it (removing the bubble alone would lose
// the player's text entirely).
let turnInputText = '';
// Set by revertUncommittedTurn, consumed (and cleared) by finishTurn.
let revertRestoreText = null;
// Stage 3 (2026-08-11): armed by interruptAndReroll when the user re-presses ›
// mid-reroll. The backend's abort path emits a `cancelled` event once the
// in-flight roll is discarded + the schema reverted to base; onCancelled
// consumes this flag to fire the fresh reroll. Cleared on any turn end.
let deferredReroll = false;
// Slice regenerate (golden pencil, 2026-08-11): the beat + streaming span
// for the in-flight partial regen. Null when no slice is running. Tracked
// separately from `activeBeat` (a full narrator turn) so the two flows can't
// collide + so isSliceRegenerating() can gate the stop button to the right
// cancel slot (fable_slice_stop vs fable_stop).
let sliceBeat = null;
let sliceSpan = null;
// (2026-08-16 yellow J2) The in-flight slice's split context — kept so a
// chat-side feed rebuild (the M3b `fable-session-changed` path) can RE-BIND
// the streaming span onto the fresh node instead of streaming into a
// detached one (the live beat showed stale text + a follow-up pencil spliced
// from it).
let sliceIndex = -1;
let slicePre = '';
let slicePost = '';
let sliceEscapeFn = null;   // transient Escape listener, removed on turn end
// (2026-08-16 audit M3) Turn EPOCH — bumped by every entry that starts (or
// tears down) a streaming lifecycle. Each in-flight turn captures the value
// at entry; its channel events + the post-invoke backstop self-ignore once
// the epoch moved on. This kills the three identity races:
//   (a) an interrupt-reroll's OLD invoke resolving after the replacement
//       turn started used to run the backstop's finishTurn() on the NEW
//       turn (composer unlocks mid-stream, armed restart disarmed);
//   (c) a channel event from a wedged/stale stream (or a pre-teardown turn)
//       landing after resetNarrator used to stamp ghost beats into the
//       NEXT session's feed.
let turnEpoch = 0;

// The unlisten handle for the `fable-session-changed` Tauri event (set up in
// initNarrator, torn down in resetNarrator). Captured at module scope so re-
// entry into the Fable stage cleans up the prior listener before registering
// a new one (otherwise multiple listeners accumulate across stage entries).
let sessionChangedUnlisten = null;
// Generation counter for the async unlisten-handle race (see initNarrator).
let sessionChangedGen = 0;
// (2026-08-24) The DEFERRED WORLD TICK in-flight signal. The backend's
// world-progression tick runs ~1-3s AFTER `done` (turn lock + lease must
// drop first) — `generating` is already false in that window, so the Soul
// Gem panel / raw editor's isGenerating() gate was green while the tick
// still mutated the schema, and a schema install there overwrote the
// tick's mutations with a pre-tick snapshot. The backend emits
// `world-tick-begin` / `world-tick-end` around the whole tick; the flag
// rides isGenerating() so every existing gate picks it up.
let worldTickInFlight = false;
let worldTickBeginUnlisten = null;
let worldTickEndUnlisten = null;
// (2026-08-25 failsafe) The backend's WorldTickFlightGuard is RAII on every
// exit path, so a missing `world-tick-end` is near-impossible — but the cost
// asymmetry is brutal: a stuck-true flag is a session-long Soul-Gem/raw-
// editor lockout with no indicator. The watchdog converts that into a ≤120s
// lockout: begin→end legitimately spans at most a turn-lock wait behind a
// live tracker decode plus the tick's own decode (~tens of seconds), so 120s
// clears it with several × headroom and can never cut a real tick short.
const WORLD_TICK_WATCHDOG_MS = 120_000;
let worldTickWatchdog = 0;
// Optional hook fired when a chat-side `fable_schema_patch` tool mutates the
// live schema (the HUD may refresh its Soul Gem panel immediately). Receives
// the list of merged top-level field names.
let onSchemaPatch = null;
// (D5 2026-08-16) Hook: ({ index, text, role }) => void — fired after a
// chat-side `fable-session-changed` messages rebuild that landed while a
// narrator turn streamed AND an inline beat editor was open. The editor's
// save would be refused by the wrappers' `if (generating) return false`
// guard AFTER its close() already swapped the body to the typed text (the
// rebuild then replaced it with the backend's unedited messages — the edit
// silently vaporized), so the handler CANCELS the editor, captures the
// in-progress edit, rebuilds, and hands it here to re-open seeded with the
// typed text. stage.js owns the role-shaped onSave closure (user →
// rewindAndEditUser, assistant → editMessage).
let onRestoreEditor = null;

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
  onNarratorActive = hooks.onNarratorActive || null;
  npcPretty = hooks.npcPretty || null;
  onSchemaPop = hooks.onSchemaPop || null;
  onApiLost = hooks.onApiLost || null;
  onTrackerSkipped = hooks.onTrackerSkipped || null;
  onSchemaPatch = hooks.onSchemaPatch || null;
  onRestoreEditor = hooks.onRestoreEditor || null;
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
  // up (initNarrator runs on every Fable stage entry). (2026-08-15 audit fix)
  // GENERATION guard: the unlisten handle arrives ASYNC — two wireStages
  // within IPC latency both saw `sessionChangedUnlisten == null`, and the
  // FIRST handle to resolve was clobbered by the second (a leaked listener
  // that no teardown ever removed). A superseded handle now unlistens ITSELF
  // the moment it resolves.
  const mySessionGen = ++sessionChangedGen;
  if (sessionChangedUnlisten) {
    try { sessionChangedUnlisten(); } catch (_) { /* already torn down */ }
    sessionChangedUnlisten = null;
  }
  if (worldTickBeginUnlisten) {
    try { worldTickBeginUnlisten(); } catch (_) { /* already torn down */ }
    worldTickBeginUnlisten = null;
  }
  if (worldTickEndUnlisten) {
    try { worldTickEndUnlisten(); } catch (_) { /* already torn down */ }
    worldTickEndUnlisten = null;
  }
  // (2026-08-24) The deferred-tick busy signal — same async-unlisten race
  // discipline as the session-changed listener above (superseded handles
  // self-release against the generation).
  listen('world-tick-begin', () => {
    worldTickInFlight = true;
    clearTimeout(worldTickWatchdog);
    worldTickWatchdog = setTimeout(() => {
      if (worldTickInFlight) {
        console.warn('[narrator] world-tick-end was never received — watchdog releasing the busy flag');
        worldTickInFlight = false;
      }
    }, WORLD_TICK_WATCHDOG_MS);
  }).then((un) => {
    if (mySessionGen !== sessionChangedGen) {
      try { un(); } catch (_) {}
      return;
    }
    worldTickBeginUnlisten = un;
  }).catch(() => { /* listener setup failed; non-fatal */ });
  listen('world-tick-end', () => {
    worldTickInFlight = false;
    clearTimeout(worldTickWatchdog);
    worldTickWatchdog = 0;
  }).then((un) => {
    if (mySessionGen !== sessionChangedGen) {
      try { un(); } catch (_) {}
      return;
    }
    worldTickEndUnlisten = un;
  }).catch(() => { /* listener setup failed; non-fatal */ });
  listen('fable-session-changed', async (e) => {
    const payload = e?.payload || {};
    if (payload.kind === 'messages' && Array.isArray(payload.messages)) {
      // (2026-08-15 audit fix) Settle the open inline editor BEFORE the
      // rebuild — rebuildFromMessages replaces every feed node, so an open
      // editor's typed text would be silently vaporized. Same
      // single-editor discipline as the composer submit / delete paths;
      // commitOpenEditor is a cheap no-op when no editor is open. A failure
      // in the save is swallowed (the backend-driven rebuild still lands).
      //
      // (D5 2026-08-16) EXCEPT while a narrator turn streams: the commit's
      // save is refused by the wrappers' `if (generating) return false`
      // guard AFTER the editor's close() already swapped the beat body to
      // the typed text — the rebuild below then replaced it with the
      // backend's unedited messages, so the commit path itself vaporized
      // the edit it was supposed to settle. In that state: capture the
      // in-progress edit (index + typed text + role off the textarea),
      // CANCEL the editor (no save attempt — restores the committed prose),
      // and re-open it seeded post-rebuild via the onRestoreEditor hook.
      let restoreEdit = null;
      const editingBeat = beats.openEditingBeat();
      if (editingBeat && generating) {
        const ta = editingBeat.querySelector('.fable-mes-editor');
        const editIdx = Number.parseInt(editingBeat.dataset.index || '-1', 10);
        if (ta && editIdx >= 0) {
          restoreEdit = {
            index: editIdx,
            text: ta.value,
            role: editingBeat.dataset.role === 'user' ? 'user' : 'assistant',
          };
        }
        beats.exitEditMode(editingBeat, false); // cancel — no save attempt
      } else {
        const pendingSave = beats.commitOpenEditor();
        if (pendingSave) {
          try { await pendingSave; } catch (_) { /* rebuild regardless */ }
        }
      }
      // (2026-08-16 audit M3b) A chat-side edit/delete can land while an API
      // narrator turn streams (the documented Stage-2 concurrency window —
      // the local chat's agent tools run beside the turn-lock-free HTTP
      // stage). Rebuilding under a live activeBeat DETACHES it: every later
      // chunk would append to an invisible node until the next rebuild.
      // Capture the streaming context, rebuild, then RE-BIND: a reroll
      // re-claims the trailing assistant beat (wiped fresh — the partial
      // was DOM-only); a normal turn re-claims the trailing user bubble
      // (the backend pushed it at turn start, so it IS in the payload) and
      // opens a fresh narrator beat. The partial text is lost either way;
      // done/finalize writes the backend's authoritative full text.
      const wasGenerating = generating;
      const wasRerolling = rerolling;
      const wasSlice = sliceBeat !== null;
      // (yellow J2) The slice's streamed partial, captured BEFORE the rebuild
      // destroys the old span — re-bound onto the fresh node below.
      const slicePartial = wasSlice && sliceSpan ? sliceSpan.textContent : '';
      // (yellow J2) The rebuild dissolves the pencil's selection — hide it.
      hidePencil();
      beats.rebuildFromMessages(payload.messages);
      if (wasGenerating) {
        if (wasRerolling) {
          activeBeat = beats.lastNarratorBeat();
          if (activeBeat) beats.beginReroll(activeBeat);
        } else if (wasSlice) {
          // (2026-08-16 yellow J2) RE-BIND the in-flight slice regen: the old
          // code deliberately skipped the slice case, so every later chunk
          // appended to a DETACHED span — the live beat showed stale text,
          // and a follow-up pencil spliced from that stale text. Re-claim
          // the beat by index + re-split it with the ORIGINAL pre/post (the
          // backend's slice_done writes the authoritative full text
          // regardless). Index shifted or beat gone → the regen is orphaned;
          // cancel the local state (the backend's own cancel/restore path
          // still fires its terminal event, which no-ops on a null sliceBeat).
          const rebound = sliceIndex >= 0 ? beats.beatByIndex(sliceIndex) : null;
          const stillAssistant = rebound && rebound.classList.contains('assistant');
          if (rebound && stillAssistant) {
            sliceBeat = rebound;
            sliceSpan = beats.beginSliceRegen(rebound, { pre: slicePre, post: slicePost });
            if (sliceSpan && slicePartial) {
              sliceSpan.textContent = slicePartial;
              sliceSpan.removeAttribute('data-empty');
            }
          } else {
            clearSliceState();
          }
        } else {
          const lastIdx = payload.messages.length - 1;
          const lastMsg = payload.messages[lastIdx];
          if (lastMsg && lastMsg.role === 'user') {
            uncommittedUserBeat = beats.beatByIndex(lastIdx);
          }
          activeBeat = beats.startNarratorBeat();
        }
      }
      // (D5) Re-open the captured editor onto the rebuilt feed — ONLY if a
      // beat with that index survived the chat-side mutation (a delete can
      // have removed it; then there is nothing to re-open onto and the
      // captured text is regrettably lost with the beat it belonged to).
      // Fired after the re-bind block so the editor opens on the final DOM.
      if (restoreEdit && typeof onRestoreEditor === 'function'
          && beats.beatByIndex(restoreEdit.index)) {
        try { onRestoreEditor(restoreEdit); } catch (_) { /* best-effort */ }
      }
    } else if (payload.kind === 'schema') {
      // The HUD (left-drawer) reads schema on its own refresh cadence; the
      // turn-end hook already calls refreshAll. We hook here so a future
      // immediate-refresh path can wire in without touching narrator again.
      if (typeof onSchemaPatch === 'function') {
        try { onSchemaPatch(payload.merged_keys || []); } catch (_) {}
      }
    }
  }).then((un) => {
    if (mySessionGen !== sessionChangedGen) {
      // Superseded while the handle was in flight — release it immediately
      // (it would otherwise leak: nothing references it anymore).
      try { un(); } catch (_) {}
      return;
    }
    sessionChangedUnlisten = un;
  })
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
  worldTickInFlight = false;
  clearTimeout(worldTickWatchdog);
  worldTickWatchdog = 0;
  rerolling = false;
  rerollPrevText = null;
  uncommittedUserBeat = null;
  turnInputText = '';
  revertRestoreText = null;
  trackerSkipWarned = false;
  clearSliceState();
  // (2026-08-16 audit M3c) Invalidate every in-flight turn/slice: their
  // channel closures + backstops compare against the epoch and self-ignore
  // from here on — a late event from the old session can never stamp a
  // ghost beat into the next one.
  turnEpoch++;
  // Tear down the session-changed listener — the stage is exiting, no more
  // chat-side mutations should rebuild the (now-hidden) feed. Bump the
  // generation so an in-flight handle from THIS session self-releases.
  sessionChangedGen++;
  if (sessionChangedUnlisten) {
    try { sessionChangedUnlisten(); } catch (_) {}
    sessionChangedUnlisten = null;
  }
}

// Clear all slice-regen state (called from finishTurn / resetNarrator).
function clearSliceState() {
  if (sliceEscapeFn) {
    // (2026-08-16 bug 3) The listener is ADDED with capture=true — the
    // removal must pass the SAME flag or it never matches (per spec), which
    // leaked one document-capture handler per pencil use + per stage reset.
    // Each leaked handler unconditionally ate every Escape app-wide (see
    // the early-return guard added alongside this in regenerateSlice).
    document.removeEventListener('keydown', sliceEscapeFn, true);
    sliceEscapeFn = null;
  }
  sliceBeat = null;
  sliceSpan = null;
  sliceIndex = -1;
  slicePre = '';
  slicePost = '';
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
  const myEpoch = ++turnEpoch;
  if (onTurnStart) onTurnStart();
  const regenerate = !!opts.regenerate;
  const reroll = !!opts.reroll;
  // (2026-08-22 Ghost Writer) The guided-swipe steer: a non-empty trimmed
  // string rides fable_send's `guidance` param, rendered as a <direction>
  // block LAST in the narrator turn tail. Only the ghost Swipe sets it; a
  // plain reroll passes nothing (null maps to backend None).
  const guidance = typeof opts.guidance === 'string' && opts.guidance.trim()
    ? opts.guidance.trim()
    : null;
  // (audit #5) Track the turn-start user bubble so every revert path can
  // remove it in lockstep with the backend's server-side pop. Regenerate
  // turns never push one (the rewind mutation owns the user tail); rerolls
  // reuse the trailing assistant beat.
  if (!opts.silent && !regenerate && !reroll) {
    turnInputText = text;
    uncommittedUserBeat = beats.addUserBeat(text);
  } else {
    turnInputText = '';
    uncommittedUserBeat = null;
  }

  // Reroll: claim the last assistant beat as the streaming target so the new
  // variant renders in place (the user watches the reroll happen over the old
  // text, no full feed rebuild). Clear its body; the new text streams in.
  if (reroll) {
    rerolling = true;
    activeBeat = beats.lastNarratorBeat();
    if (activeBeat) {
      // (audit #8) Capture the pre-reroll prose BEFORE the wipe — the cancel
      // paths restore it (finalizeBeat), they don't re-wipe.
      const body = activeBeat.querySelector('.fable-mes-text');
      rerollPrevText = activeBeat.dataset.raw != null
        ? activeBeat.dataset.raw
        : (body ? body.textContent : '');
      beats.beginReroll(activeBeat);
    } else {
      activeBeat = beats.startNarratorBeat();
      rerollPrevText = null;
    }
  }

  const channel = new Channel();
  // (2026-08-16 audit M3c) Events from THIS turn only — once the epoch moves
  // on (interrupt-reroll replacement, stage teardown), a stale channel event
  // self-ignores instead of stamping a ghost beat into the new turn/session.
  channel.onmessage = (msg) => {
    if (myEpoch !== turnEpoch) return;
    handleEvent(msg);
  };

  try {
    await invoke('fable_send', { text, onEvent: channel, regenerate, reroll, guidance });
    // (.finally backstop, 2026-08-15 audit fix) A backend resolve WITHOUT a
    // terminal event (done / error / api_lost / cancelled) would leave
    // `generating` latched → the composer wedges until app restart. Every
    // terminal path runs finishTurn, so reaching here STILL generating means
    // no terminal arrived: finish defensively (mirrors the chat window's
    // .finally backstop). (2026-08-16 audit M3a) EPOCH-guarded: after an
    // interrupt-reroll the OLD invoke resolves while the replacement turn is
    // mid-stream — an unguarded finishTurn here killed the new turn's
    // `generating` flag (composer unlocks mid-restart) or disarmed the armed
    // restart.
    if (generating && myEpoch === turnEpoch) finishTurn();
  } catch (err) {
    // (2026-08-16 audit M3c) Epoch-guarded like the backstop: an invoke that
    // REJECTS after a teardown/replacement must not stamp a ghost error beat
    // into the new session's feed or kill its turn state.
    if (myEpoch !== turnEpoch) return;
    // (2026-08-16 audit H4) A REJECTED invoke — preflight refusal (API
    // disconnected via the OS panel, session-guard while a turn finalizes)
    // — means the backend never saw the turn. The optimistic DOM mutations
    // (the uncommitted user bubble, the wiped reroll beat) must revert
    // exactly like the channel 'error' path, or the orphan beat shifts
    // every later data-index by one and index-based mutations (edit/
    // delete/swipe/slice) target the wrong message until a rebuild.
    revertUncommittedTurn();
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
      // (audit #5) The dev-narrator error path reverts server-side (schema
      // restore + user-message pop) — mirror it in the DOM before surfacing
      // the error beat.
      revertUncommittedTurn();
      beats.addErrorBeat(msg.message || 'Generation failed.');
      finishTurn();
      break;
    case 'api_lost':
      // 2026-08-07 override: the API narrator died mid-session and there's no
      // local fallback. The backend already autosaved + cleared the cancel
      // slot; the turn aborts without a narrator beat. (audit #5) It ALSO
      // popped the turn-start user message + never committed the narrator
      // beat — discard both from the DOM (the half-streamed beat kept its
      // live caret, and the orphan user bubble made the Enter-retry render
      // the action twice). Lock the composer via the onApiLost hook so the
      // player reconnects via Settings + retries (the retry stash is taken
      // from lastSentTurnText, still set at this point).
      revertUncommittedTurn();
      onApiLost(msg.message || 'The API connection was lost.');
      finishTurn();
      break;
    case 'cancelled':
      onCancelled();
      break;
    case 'tracker_skipped':
      // (2026-08-21) The backend's tracker pass died on the prompt budget —
      // the turn still narrates, but tracking is frozen this turn. Surface
      // once per session; the log carries the per-turn detail.
      if (!trackerSkipWarned) {
        trackerSkipWarned = true;
        if (onTrackerSkipped) onTrackerSkipped();
      }
      break;
    case 'turn_notice':
      // (2026-08-22) Silent state changes made player-visible: combat
      // Referee injuries + automatic inventory moves (auto-wear, equip
      // swaps, belt spills). Injuries fire pre-tracker, inventory post-
      // apply — both stream before/around the narrator's prose.
      showTurnNotice(msg.kind, msg.text);
      break;
    case 'done':
      onDone(msg.final_text, msg.reasoning, msg.cancelled);
      break;
  }
}

function onChunk(text) {
  if (!text) return;
  if (onNarratorActive) onNarratorActive();
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
    if (onNarratorActive) onNarratorActive();
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
  // (2026-08-15 audit fix) A soft-cancelled turn (fable_stop) is FULLY
  // reverted by the backend, which emits done with `cancelled: true` and an
  // EMPTY final_text. (2026-08-16 audit fixes #5/#8) The frontend revert now
  // mirrors the backend exactly: the partial narrator beat is DISCARDED (on
  // a reroll the pre-reroll prose is RESTORED — the backend re-installed the
  // prior variant server-side; the old wipe left a blank beat with a stuck
  // caret), and the turn-start user bubble is REMOVED (the backend popped
  // it; keeping it duplicated the action on re-send + shifted feed indexes).
  // The typed text is handed back to the composer via onTurnEnd so the
  // player doesn't lose it. finishTurn runs the same cleanup as a normal
  // done (composer re-enable, slice state, deferred-reroll disarm).
  if (cancelled) {
    revertUncommittedTurn();
    finishTurn();
    return;
  }
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

// (2026-08-16 audit fixes #5 + #8) Shared DOM revert for every path where the
// backend REVERTED the turn (soft cancel, api_lost, dev-narrator error): the
// backend popped the turn-start user message + never committed the narrator
// beat, so the DOM must match. Discards the partial narrator beat — on a
// reroll it RESTORES the pre-reroll prose instead (the backend re-installed
// the prior variant server-side; the beat's variant stamp is unchanged since
// the aborted roll added no variant) — removes the uncommitted user bubble,
// and stashes the typed text for the composer restore via finishTurn's
// onTurnEnd handoff.
function revertUncommittedTurn() {
  if (activeBeat) {
    if (rerolling && rerollPrevText != null) {
      beats.finalizeBeat(activeBeat, rerollPrevText);
      beats.refreshDrawer(activeBeat);
    } else if (rerolling) {
      beats.beginReroll(activeBeat); // defensive: no prior text was captured
    } else {
      activeBeat.remove();
      // (2026-08-16 bug 18) The removed streamed beat held the trailing
      // stamp — the assistant below it becomes the new tail still carrying
      // its stale DISABLED ›. stage.js's `if (btn.disabled) return` makes
      // the stamp a hard gate (a disabled button never fires the click-time
      // re-derive), so after a soft-stop / api_lost the player couldn't
      // reroll the last AI beat until the next append/rebuild. Re-sync the
      // new tail's drawer here, mirroring the reroll branch above.
      const tail = beats.lastNarratorBeat();
      if (tail) beats.refreshDrawer(tail);
    }
  }
  if (uncommittedUserBeat && uncommittedUserBeat.isConnected) {
    uncommittedUserBeat.remove();
  }
  uncommittedUserBeat = null;
  revertRestoreText = turnInputText || null;
  rerollPrevText = null;
}

function finishTurn() {
  generating = false;
  activeBeat = null;
  rerolling = false;
  // (P3 fix) Disarm the deferred reroll: if interruptAndReroll's invoke
  // resolved after the roll had already finalized, the flag stayed armed and
  // a LATER `cancelled` event fired a spurious reroll of the wrong beat.
  deferredReroll = false;
  clearSliceState();
  // (2026-08-27 playtest M1) Turn-end drawer self-heal: whatever path
  // produced the turn, the trailing assistant beats' chevron stamps now
  // match the feed (a stale is-locked variantbar used to survive turn
  // completion with no later append/rebuild to fix it).
  beats.refreshTrailingDrawers();
  // (audit #5) Hand the reverted turn's typed text to the stage so it can
  // restore it into the composer (the bubble is gone; without this the
  // player's action would be lost entirely). Undefined on non-revert paths.
  if (onTurnEnd) {
    onTurnEnd(revertRestoreText != null ? { revertedText: revertRestoreText } : undefined);
  }
  revertRestoreText = null;
  turnInputText = '';
  uncommittedUserBeat = null;
}

// (2026-08-24) Includes the deferred world-tick window: the backend tick
// runs 1-3s AFTER `done` cleared `generating`, and the schema-install gates
// (Soul Gem panel, raw editor) must stay shut through it — see
// worldTickInFlight above.
export function isGenerating() { return generating || worldTickInFlight; }
export function isRerolling() { return rerolling; }
// True while a golden-pencil slice regen is streaming. The composer's stop
// button reads this to route the cancel to `fable_slice_stop` (the slice
// cancel slot) instead of `fable_stop` (the full-turn slot) — Bug #7 cross-
// wire lesson.
export function isSliceRegenerating() { return sliceBeat !== null; }

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
  // Capture BEFORE finishTurn() — its unconditional disarm (the P3 guard
  // against late-resolving interrupt invokes) erases the flag, so reading
  // it after would always see false and the armed reroll would never fire.
  const wasDeferred = deferredReroll;
  // (audit #8) The backend discarded the partial + reverted to the prior
  // variant server-side — restore the beat's pre-reroll prose (not the old
  // wipe). If a deferred reroll was armed it immediately re-streams over
  // the restored text, so nothing lingers during the IPC round-trip.
  revertUncommittedTurn();
  finishTurn();
  if (wasDeferred) {
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
//
// (2026-08-16 yellow J1) EPOCH GUARDS — the exact M3c class fixed for turns:
// a resolve landing after a stage exit/teardown used to rebuild the feed
// with the OLD session's messages inside the NEXT stage's fresh one (+ fire
// a spurious finishTurn on it). Every wrapper captures the epoch at entry
// and self-ignores once it moved; the two delegating wrappers (reroll/
// rewind) also refuse to hand off to sendFableTurn after a teardown.
// =============================================================

// In-place edit for either a user or assistant message. The backend
// persists the prose; for ASSISTANT messages it also RE-TRACKS (2026-08-14):
// reverts the live schema to the message's base_schema (undoing the turn's
// last track) + re-runs the local tracker over the edited beat, storing the
// fresh snapshot into the variant↔schema binding. That tracker pass is a
// local decode (a few seconds) — `generating` stays true across it so the
// composer + drawer wait it out. schema_pop_count is 0 (the ring-buffer
// push happens inside the backend, the swipe precedent).
export async function editMessage(index, newText) {
  if (generating) return false;
  generating = true;
  const myEpoch = turnEpoch;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('edit_message', { index, newText });
    if (myEpoch !== turnEpoch) return false; // stage exited mid-save (J1)
    if (res && Array.isArray(res.messages)) {
      hidePencil(); // (yellow J2) the rebuild dissolves the pencil's selection
      beats.rebuildFromMessages(res.messages);
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
    return true;
  } catch (err) {
    if (myEpoch !== turnEpoch) return false;
    beats.addErrorBeat(String(err));
    return false;
  } finally {
    if (myEpoch === turnEpoch) finishTurn();
  }
}

// Delete a message by index. CASCADE (2026-08-19, Chloe): `opts.cascade`
// removes the target AND every message below it (the only shape the feed's
// delete button offers for 2+ doomed messages — a transcript gap between
// surviving beats is meaningless). The backend truncates the session tail and
// rewinds the world-schema ring to the surviving era (the rewind_and_edit_
// user discipline), so world state matches the surviving timeline. The
// single-message path (the trailing beat, no confirm modal) keeps the old
// `Conversation::remove_at` primitive: prose-only, no schema change
// (schema_pop_count is 0 either way). Rebuilds the feed from the returned
// messages[]. Destructive (no conversation-undo), so the drawer gates it
// behind a two-step inline confirm (+ the cascade confirm modal).
export async function deleteMessage(index, opts = {}) {
  if (generating) return false;
  generating = true;
  const myEpoch = turnEpoch;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('delete_message', {
      index,
      cascade: opts.cascade === true,
    });
    if (myEpoch !== turnEpoch) return false; // stage exited mid-delete (J1)
    if (res && Array.isArray(res.messages)) {
      hidePencil(); // (yellow J2) the rebuild dissolves the pencil's selection
      beats.rebuildFromMessages(res.messages);
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
    return true;
  } catch (err) {
    if (myEpoch !== turnEpoch) return false;
    beats.addErrorBeat(String(err));
    return false;
  } finally {
    if (myEpoch === turnEpoch) finishTurn();
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
// (2026-08-22 Ghost Writer) The optional `nudge` is the composer's typed
// steer: it rides `guidance` into the narrator turn tail as the fresh
// variant's <direction> block.
export async function rerollLastTurn(nudge = '') {
  if (generating) return false;
  generating = true;
  const myEpoch = turnEpoch;
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
    if (myEpoch !== turnEpoch) return false; // stage exited mid-gate (J1)
    beats.addErrorBeat(String(err));
    finishTurn();
    return false;
  }
  // (J1) The gate round-trip is an await — a stage exit in that window must
  // NOT delegate to sendFableTurn (it would mint a ghost user beat + stream
  // into the torn-down feed of the next entry).
  if (myEpoch !== turnEpoch) return false;
  // Hand off to the streaming path with reroll=true. Reset generating first
  // so sendFableTurn's `if (generating) return` guard doesn't bail.
  generating = false;
  const guidance = typeof nudge === 'string' && nudge.trim() ? nudge.trim() : null;
  await sendFableTurn('', { reroll: true, guidance });
  return true;
}

// (2026-08-22 Ghost Writer) CONTINUE: extend the trailing beat from where
// it ends, steered by the player's typed direction. The backend one-shots
// the continuation over the narrator cache, then lands through the edit
// path (apply_edit + assistant-edit re-track) — so this wrapper mirrors
// editMessage's shape exactly: generating lock across the whole round-trip
// (the API call AND the local re-track), feed rebuild from the returned
// messages[], epoch guards on every resume point.
export async function ghostContinue(nudge) {
  if (generating) return false;
  generating = true;
  const myEpoch = turnEpoch;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('ghostwriter_continue', { nudge });
    if (myEpoch !== turnEpoch) return false; // stage exited mid-continue (J1)
    if (res && Array.isArray(res.messages)) {
      hidePencil(); // the rebuild dissolves the pencil's selection (J2)
      beats.rebuildFromMessages(res.messages);
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
    return true;
  } catch (err) {
    if (myEpoch !== turnEpoch) return false;
    beats.addErrorBeat(String(err));
    return false;
  } finally {
    if (myEpoch === turnEpoch) finishTurn();
  }
}

// Swipe to a different variant of an assistant message (the ‹ 1/N › UX).
// Backend swaps the active `content`/`raw_output` with the sibling at
// `variantIdx` + persists; we splice the new content into the beat body in
// place (mirror the §11.29 selective-regenerate splice — no feed rebuild).
// schema_pop_count is 0 (a swipe is a display change, not a world change).
export async function swipeVariant(index, variantIdx) {
  if (generating) return false;
  generating = true;
  const myEpoch = turnEpoch;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('swipe_variant', { index, variantIdx });
    if (myEpoch !== turnEpoch) return false; // stage exited mid-swipe (J1)
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
    if (myEpoch !== turnEpoch) return false;
    beats.addErrorBeat(String(err));
    return false;
  } finally {
    if (myEpoch === turnEpoch) finishTurn();
  }
}

// Branch the timeline: edit a user message from N turns ago. Truncates the
// conversation right after the target index, overwrites the target, rebuilds
// the feed, then re-streams a fresh turn for the newly edited timeline.
// schema_pop_count is N (count of assistant turns the truncation removed).
export async function rewindAndEditUser(index, newText) {
  if (generating) return false;
  generating = true;
  const myEpoch = turnEpoch;
  if (onTurnStart) onTurnStart();
  try {
    const res = await invoke('rewind_and_edit_user', { index, newText });
    if (myEpoch !== turnEpoch) return false; // stage exited mid-rewind (J1)
    if (res && Array.isArray(res.messages)) {
      hidePencil(); // (yellow J2) the rebuild dissolves the pencil's selection
      beats.rebuildFromMessages(res.messages);
    }
    if (onSchemaPop && typeof res.schema_pop_count === 'number') {
      onSchemaPop(res.schema_pop_count);
    }
  } catch (err) {
    if (myEpoch !== turnEpoch) return false;
    beats.addErrorBeat(String(err));
    finishTurn();
    return false;
  }
  // (J1) Same delegation guard as rerollLastTurn: a stage exit during the
  // rewind round-trip must not stream a turn into the torn-down feed.
  if (myEpoch !== turnEpoch) return false;
  // Regenerate from the edited user message (now the last message). Same
  // hand-off pattern as rerollLastTurn: reset generating + delegate to the
  // streaming path with regenerate=true.
  generating = false;
  await sendFableTurn(newText, { regenerate: true });
  return true;
}

// =============================================================
// SLICE REGENERATE — the golden pencil (2026-08-11).
//
// Partial in-place regen of a highlighted span inside an assistant message.
// The frontend's slice-regen.js computed the authoritative 3-way split
// (pre/selection/post) from the DOM Selection; the backend (fable_regenerate
// _slice) asks the API to rewrite ONLY the selection, splicing cleanly, then
// streams the replacement into a streaming span between `pre` and `post`.
// On slice_done the beat is re-rendered from the final full text.
//
// API-only, no tracker, no schema mutation, in-place, no undo. Guard on
// isGenerating() like the other mutators. Cancellable via Escape (or the
// composer stop button when isSliceRegenerating()) → fable_slice_stop.
// =============================================================
export async function regenerateSlice({ index, pre, selection, post }) {
  if (generating) return false;
  generating = true;
  const myEpoch = ++turnEpoch;
  if (onTurnStart) onTurnStart();

  const beat = beats.beatByIndex(index);
  if (!beat) {
    beats.addErrorBeat('Cannot regenerate: message not found.');
    finishTurn();
    return false;
  }

  // Split the beat body into pre + streaming span + post.
  sliceBeat = beat;
  sliceSpan = beats.beginSliceRegen(beat, { pre, post });
  // (yellow J2) Remember the split context so a chat-side feed rebuild can
  // re-bind the span (see the session-changed handler).
  sliceIndex = index;
  slicePre = pre;
  slicePost = post;

  // Transient Escape → cancel. Removed on finishTurn via clearSliceState.
  // (2026-08-16 bug 3) Inert when no slice is live: a handler that outlived
  // its turn (or resolved before removal) must not swallow Escape for every
  // other consumer (inline beat editors, popups) at document-capture.
  sliceEscapeFn = (e) => {
    if (e.key !== 'Escape') return;
    if (!sliceBeat) return;
    e.preventDefault();
    e.stopPropagation();
    stopSliceRegen();
  };
  document.addEventListener('keydown', sliceEscapeFn, true);

  const channel = new Channel();
  channel.onmessage = (msg) => {
    if (myEpoch !== turnEpoch) return;
    handleSliceEvent(msg);
  };

  try {
    await invoke('fable_regenerate_slice', { index, pre, selection, post, onEvent: channel });
    // (.finally backstop, 2026-08-15 audit fix — same as sendFableTurn) a
    // backend resolve WITHOUT a terminal event must not latch `generating`.
    // (2026-08-16 audit M3a) Epoch-guarded like the full-turn backstop.
    if (generating && myEpoch === turnEpoch) finishTurn();
  } catch (err) {
    // Restore the beat to its pre-regen prose + surface the error.
    if (sliceBeat) beats.cancelSliceRegen(sliceBeat);
    beats.addErrorBeat(String(err));
    finishTurn();
    return false;
  }
  return true;
}

// Slice channel event router. Distinct from handleEvent (the fable_send path)
// because the finalization event is `slice_done` (not `done`) + the streaming
// target is `sliceSpan` (not `activeBeat`).
function handleSliceEvent(msg) {
  if (!msg || typeof msg !== 'object') return;
  switch (msg.type) {
    case 'chunk':
      if (onNarratorActive) onNarratorActive();
      if (sliceSpan) beats.streamSliceChunk(sliceSpan, msg.text);
      break;
    case 'slice_done':
      if (sliceBeat) beats.finalizeSliceRegen(sliceBeat, msg.final_text);
      finishTurn();
      break;
    case 'error':
      if (sliceBeat) beats.cancelSliceRegen(sliceBeat);
      beats.addErrorBeat(msg.message || 'Slice regeneration failed.');
      finishTurn();
      break;
    case 'cancelled':
      if (sliceBeat) beats.cancelSliceRegen(sliceBeat);
      finishTurn();
      break;
    case 'api_lost':
      // Same lockout contract as a full-turn api_lost: restore the beat, lock
      // the composer via the onApiLost hook so the player reconnects + retries.
      if (sliceBeat) beats.cancelSliceRegen(sliceBeat);
      if (onApiLost) onApiLost(msg.message || 'The API connection was lost.');
      finishTurn();
      break;
  }
}

// Cancel the in-flight slice regen (Escape or the composer stop button).
// Signals the reserved fable_slice_stop slot. The backend breaks the HTTP
// stream + emits `cancelled`, which handleSliceEvent routes to a beat restore.
export async function stopSliceRegen() {
  if (!sliceBeat) return;
  try { await invoke('fable_slice_stop'); } catch (_) {}
}

// npc_id → display name. The registry + tracker carry ids; the model emits
// the same ids in [CHARACTER_TURN:id]. We prettify: "the_stranger"
// → "The Stranger". (The optional npcPretty override hook currently has no
// supplier — the default prettifier below is the single live path.)
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
