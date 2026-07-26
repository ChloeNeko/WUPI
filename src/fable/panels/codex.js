// =============================================================
// PANEL: CODEX — the read-only world reference.
// Renders WorldSchema.summary + recent_events. The "ground truth"
// the narrator + Wupi reason over. This is the default panel when
// no focus keyword matches — a recap of where the story stands.
// =============================================================

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
function prose(s) { return esc(s).replace(/\n/g, '<br>'); }

export function renderCodex(entities, schema) {
  const summary = (schema && schema.summary) || '';
  const events = (schema && Array.isArray(schema.recent_events)) ? schema.recent_events : [];
  const entityCount = Object.keys(entities || {}).length;

  const head = `<div class="panel-head">
      <h2>World Codex</h2>
      <p class="panel-hint">The state of the story.</p>
    </div>`;

  let body = '';
  if (summary) {
    body += `<section class="codex-section">
      <h3>Summary</h3>
      <p class="codex-prose">${prose(summary)}</p>
    </section>`;
  }
  if (events.length) {
    const list = events.map((e) => `<li>${esc(e)}</li>`).join('');
    body += `<section class="codex-section">
      <h3>Recent Events</h3>
      <ul class="codex-events">${list}</ul>
    </section>`;
  }
  if (entityCount) {
    body += `<section class="codex-section">
      <h3>Tracked Details <span class="codex-count">${entityCount}</span></h3>
      <ul class="codex-entities">${
        Object.entries(entities).map(([k, v]) =>
          `<li><span class="codex-key">${esc(k)}</span><span class="codex-val">${esc(v || '—')}</span></li>`
        ).join('')
      }</ul>
    </section>`;
  }
  if (!body) {
    body = `<div class="panel-empty">
      <p>The story hasn't been recorded yet.</p>
      <p class="panel-empty-hint">Play a few turns and ask Wupi for a recap.</p>
    </div>`;
  }
  return head + `<div class="codex-body">${body}</div>`;
}
