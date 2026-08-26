// =============================================================
// PROGRESS CLOCKS (2026-08-24 Part II C4) — the World tab's
// Quests section helpers: deadline-bearing quests + promises
// render a conic-gradient ring chip (elapsed/total fraction,
// brass; overdue flips red). Pure fractions + models only — the
// DOM/CSS live in tab-rail.js / fable.css (`.fable-clock-*`, the
// creator-ring conic pattern).
// =============================================================

/// PURE: the elapsed fraction of a deadline window, clamped to [0, 1].
/// `startedAt`/`deadline`/`now` are world-clock minutes; a non-positive
/// window (missing start or an already-passed deadline at arm time)
/// returns 1 (fully elapsed — the overdue flag does the rest).
export function clockFraction(startedAt, deadline, now) {
  const s = Number(startedAt) || 0;
  const d = Number(deadline) || 0;
  const n = Number(now) || 0;
  if (d <= 0 || s <= 0 || d <= s) return d > 0 ? 1 : 0;
  const frac = (n - s) / (d - s);
  return Math.min(1, Math.max(0, frac));
}

/// PURE: the chip's CSS custom property value (a 0..1 string with three
/// decimals — the conic gradient multiplies it by 360deg in CSS).
export function clockFracVar(frac) {
  return (Math.min(1, Math.max(0, Number(frac) || 0))).toFixed(3);
}

/// PURE: the Quests-section model over one `fable_schema_get` snapshot.
/// Quests render title + objective progress (cur/total summed) + giver;
/// every deadline-bearing entry (quest or promise) carries a clock chip.
/// Missing keys (both serialize skip-when-empty) degrade to empty lists.
export function buildQuestClocksModel(schema) {
  const s = schema && typeof schema === 'object' ? schema : {};
  const now = Number((s.world_clock && s.world_clock.current_minutes) || 0);

  const quests = (Array.isArray(s.quests) ? s.quests : []).map((q) => q || {});
  const promises = (Array.isArray(s.promises) ? s.promises : []).map((p) => p || {});

  const questRows = quests.map((q) => {
    const objectives = Array.isArray(q.objectives) ? q.objectives : [];
    const done = objectives.filter((o) => o && o.done).length;
    const counting = objectives.filter((o) => o && Number(o.total) > 0);
    const cur = counting.reduce((a, o) => a + (Number(o.cur) || 0), 0);
    const total = counting.reduce((a, o) => a + Number(o.total), 0);
    const deadline = Number(q.deadline_minutes) || 0;
    return {
      kind: 'quest',
      id: q.id || '',
      title: q.title || '',
      giver: q.giver || '',
      reward: q.reward || '',
      done,
      objectiveCount: objectives.length,
      counter: total > 0 ? `${cur}/${total}` : '',
      deadline,
      overdue: deadline > 0 && now >= deadline,
      frac: clockFraction(q.accepted_at_minutes, deadline, now),
    };
  });

  const promiseRows = promises.map((p) => {
    const deadline = Number(p.deadline_minutes) || 0;
    return {
      kind: 'promise',
      id: p.npc_id || '',
      title: p.description || '',
      giver: p.npc_id || '',
      reward: '',
      done: 0,
      objectiveCount: 0,
      counter: '',
      deadline,
      overdue: deadline > 0 && now >= deadline,
      frac: clockFraction(p.accepted_at_minutes, deadline, now),
    };
  });

  return { now, rows: [...questRows, ...promiseRows] };
}
