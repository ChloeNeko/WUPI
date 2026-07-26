// =============================================================
// SOUNDTRACK — mood-based in-game music for the Fable stage.
//
// Restored from v0.6.5 (the 4-track mood library was lost in the v0.7.0
// source revert). The 4 MP3s ship in public/ and are picked by matching
// keywords in the active card's tone/setting. This is DISTINCT from the
// Fable title-screen music (fable_theme.mp3, handled in screens/reveal.js):
//   - reveal.js  → title screen music (one fixed track, plays on the title)
//   - soundtrack → in-game narration music (mood-picked, plays on the stage)
//
// STATUS (2026-07-26): data + keyword matcher restored from v0.6.5. The
// playback integration with the stage lifecycle (play on stage-enter,
// pause on Wupi-drawer open, stop on stage-exit) is a FOLLOW-UP — wiring
// it needs to coordinate with reveal.js's title-music lifecycle so the
// two don't overlap. The module is exported + ready; stage.js can import
// + call startSoundtrack(host, cardTone) / stopSoundtrack(host) once the
// coordination is designed.
// =============================================================

// The 4-track library. Each entry binds a src URL (in public/) to a mood
// keyword string. The picker (soundtrackForTone) returns the entry whose
// mood keywords best match the card's tone/setting.
const SOUNDTRACKS = {
  acrossRidge: {
    src: './Across_the_Verdant_Ridge.mp3',
    mood: 'calm, pastoral, verdant, hopeful, green',
  },
  ironAndSilk: {
    src: './Iron_and_Silk.mp3',
    mood: 'tense, martial, combat, steel, duty, war',
  },
  promises: {
    src: './Promises_in_the_Pavilion.mp3',
    mood: 'cozy, tender, intimate, romance, tavern, fire, warm',
  },
  thunderSalt: {
    src: './Thunder_and_Salt.mp3',
    mood: 'storm, gothic, dark, sea, ominous, frontier, rain',
  },
};

// Default fallback when no keyword matches. The starter rusty_tavern card
// (tone: "atmospheric, slow-burn, morally grey, frontier gothic") matches
// thunderSalt via the "gothic/frontier/rain" keywords.
const DEFAULT_SOUNDTRACK = SOUNDTRACKS.thunderSalt;

// Pick a soundtrack entry by matching the card's tone/setting keywords
// against each entry's mood string. First match wins (object insertion
// order). Returns the entry object ({src, mood}).
export function soundtrackForTone(tone = '') {
  const t = (tone || '').toLowerCase();
  for (const entry of Object.values(SOUNDTRACKS)) {
    const keywords = entry.mood.split(',').map((k) => k.trim());
    if (keywords.some((k) => t.includes(k))) return entry;
  }
  return DEFAULT_SOUNDTRACK;
}

// ── Playback lifecycle (FOLLOW-UP: not yet wired into stage.js) ───────────
// The below functions mirror reveal.js's title-music lifecycle pattern so
// the eventual stage.js integration is a drop-in. They are exported but not
// yet called from anywhere; stage.js needs to import + invoke them at the
// stage-enter / stage-exit seams once the title-music coordination is done.

const SOUNDTRACK_ID = 'fable-stage-soundtrack';
const SOUNDTRACK_VOLUME = 0.22; // lower than title music (0.3) — ambient bed

export function startSoundtrack(host, tone = '') {
  if (!host) return;
  if (host.querySelector('#' + SOUNDTRACK_ID)) return; // never stack
  const track = soundtrackForTone(tone);
  const audio = document.createElement('audio');
  audio.id = SOUNDTRACK_ID;
  audio.src = track.src;
  audio.loop = true;
  audio.volume = SOUNDTRACK_VOLUME;
  audio.setAttribute('aria-hidden', 'true');
  host.appendChild(audio);
  const p = audio.play();
  if (p && typeof p.catch === 'function') p.catch(() => {}); // autoplay-blocked: silent
}

export function stopSoundtrack(host) {
  if (!host) return;
  const audio = host.querySelector('#' + SOUNDTRACK_ID);
  if (audio) {
    try { audio.pause(); } catch (_) {}
    audio.remove();
  }
}
