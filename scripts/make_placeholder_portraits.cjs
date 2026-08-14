// Generates two placeholder portrait PNGs for the Fable dev-preview:
//   src/fable/assets/placeholder_ai.png     (slate-blue, labeled AI)
//   src/fable/assets/placeholder_player.png (ash-gray, labeled P)
// Pure Node (zlib only) — builds a valid PNG with raw filter-byte scanlines.
// Run: node scripts/make_placeholder_portraits.cjs
const fs = require('fs');
const path = require('path');
const zlib = require('zlib');

const W = 240, H = 320; // portrait aspect (3:4)

function crc32(buf) {
  let c = ~0;
  for (let i = 0; i < buf.length; i++) {
    c ^= buf[i];
    for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xEDB88320 & -(c & 1));
  }
  return (~c) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const t = Buffer.from(type, 'ascii');
  const body = Buffer.concat([t, data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

function makePng(rgb) {
  // PNG signature
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  // IHDR
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(W, 0);
  ihdr.writeUInt32BE(H, 4);
  ihdr[8] = 8;   // bit depth
  ihdr[9] = 2;   // color type (RGB)
  ihdr[10] = 0;  // compression
  ihdr[11] = 0;  // filter
  ihdr[12] = 0;  // interlace
  // Build a vertical gradient: darker top → lighter bottom, all within the
  // base hue. Pre-fills the portrait so it's clearly distinct from empty.
  const rows = [];
  for (let y = 0; y < H; y++) {
    const line = Buffer.alloc(1 + W * 3);
    line[0] = 0; // filter: none
    const t = y / (H - 1);             // 0 top → 1 bottom
    const shade = 0.55 + t * 0.45;     // 0.55 .. 1.0
    for (let x = 0; x < W; x++) {
      // subtle horizontal vignette (darker at far edges)
      const cx = (x / (W - 1)) - 0.5;  // -0.5 .. 0.5
      const vig = 1 - Math.abs(cx) * 0.35;
      const m = shade * vig;
      line[1 + x * 3 + 0] = Math.min(255, Math.round(rgb[0] * m));
      line[1 + x * 3 + 1] = Math.min(255, Math.round(rgb[1] * m));
      line[1 + x * 3 + 2] = Math.min(255, Math.round(rgb[2] * m));
    }
    rows.push(line);
  }
  const raw = Buffer.concat(rows);
  const idat = zlib.deflateSync(raw);
  return Buffer.concat([
    sig,
    chunk('IHDR', ihdr),
    chunk('IDAT', idat),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

const outDir = path.join(__dirname, '..', 'src', 'fable', 'assets');
fs.mkdirSync(outDir, { recursive: true });

// Slate-blue for AI (#5A6B82 ish), ash-gray for Player (#8A93A0 ish)
fs.writeFileSync(path.join(outDir, 'placeholder_ai.png'), makePng([90, 107, 130]));
fs.writeFileSync(path.join(outDir, 'placeholder_player.png'), makePng([138, 147, 160]));

console.log('wrote placeholder_ai.png + placeholder_player.png into', path.relative(process.cwd(), outDir));
