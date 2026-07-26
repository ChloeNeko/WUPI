// =============================================================
// GAMES ATMOSPHERE — port of UIE's atmosphere.js.
// Three concerns, all driven by narrator prose + mouse position:
//   1. TIME-OF-DAY filter (night/sunset/dawn/day) applied over the bg.
//   2. WEATHER overlay (delegates to effects.js so we never double-render).
//   3. PARALLAX: mouse-driven translate on the bg (cheap, immersion-heavy),
//      plus a slow 24s "breathing" drift.
//
// Detection is keyword-based over incoming prose (same heuristic as UIE).
// Stateless detector + stateful applier: scan() is pure, apply() mutates.
// =============================================================

// CSS filter presets per time-of-day. Applied to #fable-atmo-layer which
// sits above the bg image. (UIE uses backdrop-filter; we layer it directly
// because our bg is an <img>, not a CSS-bg on a big element.)
const TIME_FILTERS = {
  night:   'brightness(0.55) saturate(0.85) hue-rotate(200deg)',
  sunset:  'brightness(0.85) sepia(0.4) hue-rotate(-25deg) saturate(1.2)',
  dawn:    'brightness(0.9) sepia(0.2) hue-rotate(-10deg) saturate(1.1)',
  day:     'none',
};

const TIME_KEYWORDS = {
  night:  ['night', 'midnight', 'moonlit', 'moon', 'dark', 'stars', '3 a.m', '2 a.m', 'small hours'],
  sunset: ['sunset', 'dusk', 'evening', 'twilight', 'gloaming'],
  dawn:   ['dawn', 'sunrise', 'first light', 'morning light'],
  day:    ['noon', 'midday', 'afternoon', 'daylight', 'broad daylight'],
};

const WEATHER_KEYWORDS = {
  rain:  ['rain', 'drizzle', 'downpour', 'storm', 'deluge', 'shower', 'pouring'],
  snow:  ['snow', 'snowfall', 'blizzard', 'flurries', 'snowing'],
  fog:   ['fog', 'mist', 'misty', 'haze', 'hazy', 'low clouds'],
};

let atmoLayer = null;   // #fable-atmo-layer (filter overlay)
let bgEl = null;        // the bg image element (parallax target)
let parallaxRAF = 0;    // throttle handle
let currentWeather = null;  // active weather effect name (avoids double-toggle)
let hooks = {};

// Pure detector: returns { time, weather } or null per category.
export function detect(text) {
  const lower = (text || '').toLowerCase();
  let time = null, weather = null;
  for (const [key, words] of Object.entries(TIME_KEYWORDS)) {
    if (words.some((w) => lower.includes(w))) { time = key; break; }
  }
  for (const [key, words] of Object.entries(WEATHER_KEYWORDS)) {
    if (words.some((w) => lower.includes(w))) { weather = key; break; }
  }
  return { time, weather };
}

export function initAtmosphere(atmoElement, bgElement, fxHooks = {}) {
  atmoLayer = atmoElement;
  bgEl = bgElement;
  hooks = fxHooks; // { onWeatherStart, onWeatherStop } → effects.js toggles
}

// Apply a time-of-day filter directly (from detect() or explicit call).
export function applyTime(timeKey) {
  if (!atmoLayer || !TIME_FILTERS[timeKey]) return;
  atmoLayer.style.filter = TIME_FILTERS[timeKey];
}

// Apply a weather effect. Idempotent: switching rain→snow clears rain first.
// Delegates the visual to effects.js via the onWeatherStart/Stop hooks so
// we never maintain a parallel particle system.
export function applyWeather(weatherKey, playFX, clearFX) {
  if (currentWeather === weatherKey) return;
  if (currentWeather) {
    if (hooks.onWeatherStop) hooks.onWeatherStop(currentWeather);
    else if (clearFX) clearFX(currentWeather);
  }
  currentWeather = weatherKey;
  if (!weatherKey) return;
  if (hooks.onWeatherStart) hooks.onWeatherStart(weatherKey);
  else if (playFX) playFX(weatherKey);
}

// Convenience: scan prose and apply both. Called by narrator.js after a
// narrator turn completes (cheap: one pass, two applies).
export function scanAndApply(text, playFX, clearFX) {
  const { time, weather } = detect(text);
  if (time) applyTime(time);
  if (weather) applyWeather(weather, playFX, clearFX);
}

// Parallax: translate the bg slightly toward the cursor. Throttled via
// rAF so a flurry of mousemoves coalesces to one transform per frame.
export function attachParallax() {
  if (!bgEl) return;
  window.addEventListener('mousemove', onParallaxMove, { passive: true });
}
export function detachParallax() {
  window.removeEventListener('mousemove', onParallaxMove, { passive: true });
  if (parallaxRAF) cancelAnimationFrame(parallaxRAF);
  parallaxRAF = 0;
}
function onParallaxMove(e) {
  if (parallaxRAF) return;
  parallaxRAF = requestAnimationFrame(() => {
    parallaxRAF = 0;
    if (!bgEl) return;
    // Max 12px shift — subtle, not seasick.
    const dx = (e.clientX / window.innerWidth - 0.5) * 24;
    const dy = (e.clientY / window.innerHeight - 0.5) * 16;
    // The breathing keyframe (fable.css) already animates the bg; we add
    // the parallax on top via CSS vars so both compose in one transform.
    bgEl.style.setProperty('--parallax-x', dx.toFixed(1) + 'px');
    bgEl.style.setProperty('--parallax-y', dy.toFixed(1) + 'px');
  });
}

// Reset on scene change / game exit.
export function resetAtmosphere() {
  if (atmoLayer) atmoLayer.style.filter = '';
  currentWeather = null;
  if (bgEl) {
    bgEl.style.removeProperty('--parallax-x');
    bgEl.style.removeProperty('--parallax-y');
  }
  detachParallax();
}
