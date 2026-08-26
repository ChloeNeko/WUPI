// Portrait namesake-contract tests (2026-08-19; v0.30.0): portraits live
// as `<Name>.png` / `<Name>.jpg` beside the `<Name>.sim` / `<Name>.player`
// — the namesake is the ONLY recognized name (the legacy fixed
// `portrait.<ext>` lane was removed with the v0.30.0 clean break).
//
// Plain Node ESM — no test runner. Run: `node tests/portrait-files.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
//
// What this pins (the filesystem contract the frontend's portrait loading
// stands on — the Rust side of each rule is pinned by the lib.rs unit
// tests for `find_portrait_sibling` / `reap_stale_portraits`):
//   1. Discovery order: namesake png > jpg > jpeg.
//   2. Every discovered portrait actually LOADS: readable bytes, magic
//      signature matches the ext (a truncated/swapped file fails), and the
//      absolute path survives the URL-encode/decode lane `convertFileSrc`
//      puts it through (spaces, capitals, accents).
//   3. A foreign-stem image (`Wrong.png` inside `Liam/`) is NOT discovered —
//      and the live-tree scan below FAILS on any it finds, because that's
//      exactly the "portrait silently missing" bug shape.
//   5. Live scan: when `apps/fable/cards` / `apps/fable/players` exist,
//      every image in an entity folder must be namesake, and every
//      discovered portrait must pass the load checks from (2).
import { strict as assert } from 'node:assert';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const EXTS = ['png', 'jpg', 'jpeg'];

// ── shared fixtures: REAL decodable images from the repo's own assets ────
const PNG_FIXTURE = path.join(ROOT, 'src', 'fable', 'assets', 'placeholder_ai.png');
const JPG_FIXTURE = path.join(ROOT, 'src', 'fable', 'assets', 'fable_background.jpg');
assert.ok(fs.existsSync(PNG_FIXTURE), 'PNG fixture (repo asset) must exist');
assert.ok(fs.existsSync(JPG_FIXTURE), 'JPG fixture (repo asset) must exist');
const PNG_BYTES = fs.readFileSync(PNG_FIXTURE);
const JPG_BYTES = fs.readFileSync(JPG_FIXTURE);

function isPng(buf) {
  return buf.length > 8 && buf[0] === 0x89 && buf[1] === 0x50 && buf[2] === 0x4e
    && buf[3] === 0x47 && buf[4] === 0x0d && buf[5] === 0x0a && buf[6] === 0x1a && buf[7] === 0x0a;
}
function isJpg(buf) {
  return buf.length > 3 && buf[0] === 0xff && buf[1] === 0xd8 && buf[2] === 0xff;
}
function magicOk(file) {
  const buf = fs.readFileSync(file);
  if (file.toLowerCase().endsWith('.png')) return isPng(buf);
  return isJpg(buf);
}

// Mirror of the Rust `find_portrait_sibling` contract: the stem is the
// FOLDER's own name (the same derivation as `<Name>.sim`), never caller
// input.
function findPortraitSibling(dir) {
  const stem = path.basename(dir);
  if (stem) {
    for (const ext of EXTS) {
      const name = `${stem}.${ext}`;
      if (fs.existsSync(path.join(dir, name))) return name;
    }
  }
  return null;
}

// The frontend load chain proxy: the backend hands `convertFileSrc` an
// ABSOLUTE path; the asset URL encodes it; the <img> decodes + reads the
// bytes. Node-side: full read (not locked/truncated) + magic-signature
// validation + URL round-trip of the path.
function assertLoads(absPath) {
  const buf = fs.readFileSync(absPath); // throws on locked/missing
  assert.ok(buf.length > 8, `${absPath}: not enough bytes to be a real image`);
  assert.ok(magicOk(absPath), `${absPath}: magic signature does not match its ext`);
  const round = decodeURI(encodeURI(absPath));
  assert.equal(round, absPath, `${absPath}: path must survive the asset-URL encode/decode lane`);
}

let passed = 0;
let failed = 0;
function test(name, fn) {
  try {
    fn();
    console.log('  ok   %s', name);
    passed++;
  } catch (e) {
    console.error('  FAIL %s\n       %s', name, e.message);
    failed++;
  }
}
const tmpdirs = [];
function freshDir(name) {
  const d = fs.mkdtempSync(path.join(os.tmpdir(), `wupi-portrait-${name}-`));
  tmpdirs.push(d);
  return d;
}

// ── 1. discovery order ────────────────────────────────────────────────────

test('discovery: no portrait → null', () => {
  const root = freshDir('empty');
  const dir = path.join(root, 'Liam');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'Liam.sim'), '<sim_card/>');
  assert.equal(findPortraitSibling(dir), null);
});

test('discovery: namesake png > jpg > jpeg', () => {
  const root = freshDir('extorder');
  const dir = path.join(root, 'Kael');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'Kael.jpeg'), 'x');
  assert.equal(findPortraitSibling(dir), 'Kael.jpeg');
  fs.writeFileSync(path.join(dir, 'Kael.jpg'), 'x');
  assert.equal(findPortraitSibling(dir), 'Kael.jpg');
  fs.writeFileSync(path.join(dir, 'Kael.png'), 'x');
  assert.equal(findPortraitSibling(dir), 'Kael.png');
});

test('discovery: the legacy portrait.<ext> name is invisible', () => {
  const root = freshDir('legacy');
  const dir = path.join(root, 'Mara');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'portrait.png'), 'x');
  assert.equal(findPortraitSibling(dir), null, 'the legacy name no longer resolves');
  fs.writeFileSync(path.join(dir, 'Mara.jpg'), 'x');
  assert.equal(findPortraitSibling(dir), 'Mara.jpg', 'the namesake resolves');
});

test('discovery: spaces + capitals ride the stem verbatim', () => {
  const root = freshDir('spaces');
  const dir = path.join(root, 'One Piece');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'One Piece.png'), 'x');
  assert.equal(findPortraitSibling(dir), 'One Piece.png');
});

// ── 2. the load chain over a realistic tree ───────────────────────────────

test('load chain: card + player portraits with REAL image bytes load fine', () => {
  const root = freshDir('load');
  const cards = path.join(root, 'cards', 'Liam');
  const cards2 = path.join(root, 'cards', 'One Piece');
  const player = path.join(root, 'players', 'Alex');
  fs.mkdirSync(cards, { recursive: true });
  fs.mkdirSync(cards2, { recursive: true });
  fs.mkdirSync(player, { recursive: true });
  fs.writeFileSync(path.join(cards, 'Liam.sim'), '<sim_card/>');
  fs.writeFileSync(path.join(cards2, 'One Piece.sim'), '<sim_card/>');
  fs.writeFileSync(path.join(player, 'Alex.player'), '<player/>');
  fs.writeFileSync(path.join(cards, 'Liam.png'), PNG_BYTES);
  fs.writeFileSync(path.join(cards2, 'One Piece.jpg'), JPG_BYTES);
  fs.writeFileSync(path.join(player, 'Alex.png'), PNG_BYTES);

  for (const dir of [cards, cards2, player]) {
    const found = findPortraitSibling(dir);
    assert.ok(found, `${dir}: portrait must be discovered`);
    assertLoads(path.join(dir, found));
    assert.ok(!found.startsWith('portrait.'), 'discovery only ever returns namesake names');
  }
});

test('load chain: a corrupted portrait FAILS the magic gate (the gate works)', () => {
  const root = freshDir('truncated');
  const dir = path.join(root, 'Nyx');
  fs.mkdirSync(dir);
  // Enough bytes to be a file, but the 8-byte PNG signature is gone.
  fs.writeFileSync(path.join(dir, 'Nyx.png'), PNG_BYTES.subarray(8, 40));
  const found = findPortraitSibling(dir);
  assert.equal(found, 'Nyx.png');
  assert.throws(() => assertLoads(path.join(dir, found)), /magic/);
});

// ── 3. foreign-stem images are invisible to discovery ─────────────────────

test('foreign stem: Wrong.png inside Liam/ is NOT discovered', () => {
  const root = freshDir('foreign');
  const dir = path.join(root, 'Liam');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'Liam.sim'), '<sim_card/>');
  fs.writeFileSync(path.join(dir, 'Wrong.png'), PNG_BYTES);
  assert.equal(findPortraitSibling(dir), null,
    'a foreign-stem image must never satisfy discovery — the live scan flags these');
});

// ── 5. live-tree scan (runs when the dev checkout carries real data) ──────

function scanLiveTree() {
  const roots = [
    ['cards', path.join(ROOT, 'apps', 'fable', 'cards'), '.sim'],
    ['players', path.join(ROOT, 'apps', 'fable', 'players'), '.player'],
  ];
  const problems = [];
  const notes = [];
  let folders = 0;
  let portraits = 0;
  for (const [label, root, identityExt] of roots) {
    if (!fs.existsSync(root)) continue;
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const dir = path.join(root, entry.name);
      const stem = entry.name;
      if (!fs.existsSync(path.join(dir, `${stem}${identityExt}`))) continue; // not an entity folder
      folders++;
      const images = fs.readdirSync(dir).filter(
        (f) => /\.(png|jpe?g|ico)$/i.test(f) && !f.endsWith('.lnk'),
      );
      for (const img of images) {
        const lower = img.toLowerCase();
        if (lower === 'portrait.ico') { notes.push(`${label}/${stem}: leftover portrait.ico (inert — the derived icon is namesake-stemmed now)`); continue; }
        if (img === `${stem}.ico` || /\.(png|jpe?g)$/i.test(img) === false) continue;
        const isNamesake = EXTS.some((ext) => img === `${stem}.${ext}`);
        const isLegacy = EXTS.some((ext) => lower === `portrait.${ext}`);
        if (isNamesake) {
          portraits++;
          try {
            assertLoads(path.join(dir, img));
          } catch (e) {
            problems.push(`${label}/${stem}/${img}: ${e.message}`);
          }
        } else if (isLegacy) {
          notes.push(`${label}/${stem}/${img}: leftover legacy portrait name (invisible to discovery since v0.30.0 — safe to delete)`);
        } else {
          problems.push(`${label}/${stem}/${img}: FOREIGN image name — discovery will never find it (expected <stem>.(png|jpg))`);
        }
      }
    }
  }
  return { folders, portraits, problems, notes };
}

const live = scanLiveTree();
test(`live tree: every portrait in apps/fable/{{cards,players}} is discoverable + loads (${live.portraits} portraits / ${live.folders} folders scanned)`, () => {
  assert.deepEqual(live.problems, []);
});
for (const note of live.notes) console.log('  note %s', note);

// ── teardown + report ─────────────────────────────────────────────────────
for (const d of tmpdirs) fs.rmSync(d, { recursive: true, force: true });

console.log(
  '\n%s — %d passed, %d failed',
  failed === 0 ? 'PORTRAITS OK' : 'PORTRAITS FAILED',
  passed,
  failed,
);
process.exit(failed === 0 ? 0 : 1);
