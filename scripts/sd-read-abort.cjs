// Reads the captured stderr from `npm run sd:dev` (logs/sd-stderr.log) and
// extracts the GGML_ABORT / GGML_ASSERT / debug-assertion line that fired
// inside diffusion-rs / stable-diffusion.cpp. The fatal message goes to
// STDERR (not the tracing log) because diffusion-rs ships its own ggml +
// never registers an abort callback — GGML_ABORT falls through to
// fprintf(stderr)+abort(). WUPI's tracing subscriber can't see stderr, so
// this capture is the only way to read the assertion text without scrolling
// the tauri-dev terminal.
//
// Usage:  npm run sd:abort
//   (after a crash — prints the relevant lines + the full tail for context)
const fs = require('fs');
const path = require('path');

const logPath = path.resolve(__dirname, '..', 'logs', 'sd-out.log');

if (!fs.existsSync(logPath)) {
  console.error('No output capture found at ' + logPath);
  console.error('Run `npm run sd:dev` (which redirects stdout+stderr to that file) and reproduce the crash, then re-run this.');
  process.exit(1);
}

const raw = fs.readFileSync(logPath, 'utf8');
const lines = raw.split(/\r?\n/);

// The abort signature: ggml prints a few recognizable patterns. Grab any
// line containing them + a few lines of context after (the assertion often
// prints the file:line + expression on the lines immediately following).
const PATTERNS = [
  /GGML_ABORT/i,
  /GGML_ASSERT/i,
  /debug assertion failed/i,
  /Assertion failed/i,
  /abort\(\)/i,
  /fatal runtime error/i,
  /panicked at/i,
];

const hits = [];
for (let i = 0; i < lines.length; i++) {
  if (PATTERNS.some((p) => p.test(lines[i]))) {
    hits.push({ idx: i, line: lines[i] });
  }
}

if (hits.length === 0) {
  console.log('No abort/assertion pattern found in ' + logPath);
  console.log('Showing the last 40 lines for context:');
  console.log('---');
  console.log(lines.slice(-40).join('\n'));
  process.exit(0);
}

// Print each hit + 3 lines of trailing context (the message usually follows).
console.log('Found ' + hits.length + ' abort/assertion site(s) in ' + logPath + ':\n');
for (const h of hits.slice(-5)) { // last 5 (a noisy compile may have many)
  console.log('=== line ' + (h.idx + 1) + ' ===');
  const ctx = lines.slice(h.idx, h.idx + 4);
  console.log(ctx.join('\n'));
  console.log('');
}
console.log('--- full last 25 lines ---');
console.log(lines.slice(-25).join('\n'));
