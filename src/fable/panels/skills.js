// =============================================================
// PANEL: SKILLS — read view over skill_ entities.
// Each skill_ entity's state is its level/rank (e.g. "3", "master",
// "apprentice"). Rendered as a list of labelled bars where the bar
// fill is inferred from a numeric or keyword state.
// =============================================================

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function renderSkills(entities, schema) {
  const skills = Object.entries(entities || {})
    .filter(([id]) => id.startsWith('skill_'));
  const head = `<div class="panel-head">
      <h2>Skills</h2>
      <p class="panel-hint">What you're capable of.</p>
    </div>`;
  if (!skills.length) {
    return head + `<div class="panel-empty">
      <p>No skills tracked yet.</p>
      <p class="panel-empty-hint">Ask Wupi to note one.</p>
    </div>`;
  }
  const rows = skills.map(([id, state]) => skillRow(id, state)).join('');
  return head + `<div class="skill-list">${rows}</div>`;
}

function skillRow(id, state) {
  const name = prettify(id);
  const { label, pct } = toLevel(state);
  return `<div class="skill-row">
    <div class="skill-row-head">
      <span class="skill-row-name">${esc(name)}</span>
      <span class="skill-row-rank">${esc(label)}</span>
    </div>
    <div class="skill-bar"><div class="skill-bar-fill" style="width:${pct}%"></div></div>
  </div>`;
}

// Map a state to a 0..100 fill + label.
function toLevel(state) {
  const s = String(state || '').trim();
  const num = parseFloat(s);
  if (!isNaN(num) && /^[\d.]+$/.test(s)) {
    // Raw number: if >1, treat as a 0-10 or 0-100 scale.
    const pct = num <= 1 ? num * 100 : num <= 10 ? num * 10 : Math.min(100, num);
    return { label: s, pct: Math.round(pct) };
  }
  const lower = s.toLowerCase();
  const ranks = [
    [/untrained|none|0/, 0],
    [/novice|beginner|apprentice/, 20],
    [/adept|competent|junior/, 40],
    [/skilled|proficient/, 60],
    [/expert|veteran|senior/, 80],
    [/master|mastery|legendary/, 100],
  ];
  for (const [re, pct] of ranks) if (re.test(lower)) return { label: s, pct };
  return { label: s || '—', pct: s ? 50 : 10 };
}

function prettify(id) {
  return id.replace(/^skill_/, '').replace(/_/g, ' ')
    .replace(/\b\w/g, (c) => c.toUpperCase());
}
