// =============================================================
// PANEL: ACTION WHEEL — radial menu of declared activities.
// Unlike other panels, this reads schema.activities (the active card's
// declared list, injected by the stage's panel-manager summon), not from
// entity prefixes. Falls back to a default set.
// =============================================================

const DEFAULT_ACTIVITIES = ['explore', 'talk', 'rest'];

function esc(s) {
  return String(s || '')
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

export function renderActionWheel(entities, schema) {
  // schema.activities may be an array; if absent, use defaults.
  const activities = (schema && Array.isArray(schema.activities) && schema.activities.length)
    ? schema.activities
    : DEFAULT_ACTIVITIES;
  const head = `<div class="panel-head">
      <h2>Actions</h2>
      <p class="panel-hint">What you can do here.</p>
    </div>`;
  if (!activities.length) {
    return head + `<div class="panel-empty"><p>No actions available here.</p></div>`;
  }
  const slice = 360 / activities.length;
  const items = activities.map((act, i) => wheelItem(act, i, slice)).join('');
  return head + `<div class="action-wheel">${items}</div>`;
}

function wheelItem(activity, i, sliceDeg) {
  const rot = i * sliceDeg;
  const icon = glyph(activity);
  return `<div class="action-wheel-item" style="--rot:${rot}deg;--slice:${sliceDeg}deg">
    <div class="action-wheel-icon">${icon}</div>
    <div class="action-wheel-label">${esc(activity)}</div>
  </div>`;
}

function glyph(act) {
  const a = (act || '').toLowerCase();
  if (/talk|conversation|speech|persuad|chat/.test(a)) return '💬';
  if (/fight|combat|attack|battle/.test(a)) return '⚔';
  if (/sneak|stealth|hide/.test(a)) return '👁';
  if (/investigat|search|inspect|examine/.test(a)) return '🔍';
  if (/rest|sleep|camp/.test(a)) return '🏕';
  if (/travel|move|go|fast.?travel/.test(a)) return '🧭';
  if (/craft|forge|build|make/.test(a)) return '⚒';
  if (/magic|cast|spell/.test(a)) return '✨';
  if (/trade|buy|sell|shop/.test(a)) return '⚖';
  if (/explore|look|scout/.test(a)) return '🌿';
  return '•';
}
