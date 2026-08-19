// Portrait namesake-contract tests (2026-08-19): portraits live as
// `<Name>.png` / `<Name>.jpg` beside the `<Name>.sim` / `<Name>.player` —
// NEVER a fixed `portrait.<ext>` (that's the legacy name, folded onto the
// stem by the boot migration + tolerated at discovery).
//
// Plain Node ESM — no test runner. Run: `node tests/portrait-files.test.mjs`.
// Exits non-zero on any failure so it can gate CI.
//
// What this pins (the filesystem contract the frontend's portrait loading
// stands on — the Rust side of each rule is pinned by the lib.rs unit
// tests for `find_portrait_sibling` / `rename_legacy_portraits` /
// `reap_stale_portraits`):
//   1. Discovery order: namesake png > jpg > jpeg, THEN legacy
//      `portrait.<ext>` (same ext order) as the read-side fallback.
//   2. Every discovered portrait actually LOADS: readable bytes, magic
//      signature matches the ext (a truncated/swapped file fails), and the
//      absolute path survives the URL-encode/decode lane `convertFileSrc`
//      puts it through (spaces, capitals, accents).
//   3. The boot-migration fold semantics: `portrait.<ext>` → `<Name>.<ext>`,
//      namesake wins over a legacy twin, idempotent, drops the derived
//      legacy `portrait.ico`.
//   4. A foreign-stem image (`Wrong.png` inside `Liam/`) is NOT discovered —
//      and the live-tree scan below FAILS on any it finds, because that's
//      exactly the "portrait silently missing" bug shape.
//   5. Live scan: when `apps/fable/cards` / `apps/fable/players` exist,
//      every image in an entity folder must be namesake or legacy-pending,
//      and every discovered portrait must pass the load checks from (2).
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
  for (const ext of EXTS) {
    const name = `portrait.${ext}`;
    if (fs.existsSync(path.join(dir, name))) return name;
  }
  return null;
}

// Mirror of the Rust `rename_legacy_portraits` fold (boot migration).
function renameLegacyPortraits(dir) {
  const stem = path.basename(dir);
  if (stem.toLowerCase() === 'portrait') return false; // namesake == legacy on Windows
  let moved = false;
  for (const ext of EXTS) {
    const legacy = path.join(dir, `portrait.${ext}`);
    if (!fs.existsSync(legacy)) continue;
    const namesake = path.join(dir, `${stem}.${ext}`);
    if (fs.existsSync(namesake)) {
      fs.rmSync(legacy); // the namesake is canonical; the legacy copy is a stale twin
    } else {
      fs.renameSync(legacy, namesake);
      moved = true;
    }
  }
  if (moved) {
    const ico = path.join(dir, 'portrait.ico');
    if (fs.existsSync(ico)) fs.rmSync(ico);
  }
  return moved;
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

test('discovery: any namesake beats the legacy name; legacy keeps working alone', () => {
  const root = freshDir('legacy');
  const dir = path.join(root, 'Mara');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'portrait.png'), 'x');
  assert.equal(findPortraitSibling(dir), 'portrait.png', 'legacy-only folder still resolves');
  fs.writeFileSync(path.join(dir, 'Mara.jpg'), 'x');
  assert.equal(findPortraitSibling(dir), 'Mara.jpg', 'namesake jpg beats legacy png');
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
    assert.ok(!found.startsWith('portrait.'), 'tree is fully namesake — no legacy lane needed');
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

// ── 3. the boot-migration fold ────────────────────────────────────────────

test('fold: portrait.<ext> → <Name>.<ext>, ico dropped, idempotent, loads after', () => {
  const root = freshDir('fold');
  const dir = path.join(root, 'Kael Brightwood');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'Kael Brightwood.player'), '<player/>');
  fs.writeFileSync(path.join(dir, 'portrait.png'), PNG_BYTES);
  fs.writeFileSync(path.join(dir, 'portrait.jpg'), JPG_BYTES);
  fs.writeFileSync(path.join(dir, 'portrait.ico'), 'ico');

  assert.equal(renameLegacyPortraits(dir), true);
  assert.ok(fs.existsSync(path.join(dir, 'Kael Brightwood.png')));
  assert.ok(fs.existsSync(path.join(dir, 'Kael Brightwood.jpg')));
  assert.ok(!fs.existsSync(path.join(dir, 'portrait.png')));
  assert.ok(!fs.existsSync(path.join(dir, 'portrait.ico')), 'derived legacy icon goes with the fold');
  assert.equal(renameLegacyPortraits(dir), false, 'second run is a no-op');
  // Post-fold discovery + load still resolve the same images.
  assert.equal(findPortraitSibling(dir), 'Kael Brightwood.png');
  assertLoads(path.join(dir, 'Kael Brightwood.png'));
  assertLoads(path.join(dir, 'Kael Brightwood.jpg'));
});

test('fold: namesake of the same ext WINS over the legacy twin', () => {
  const root = freshDir('twin');
  const dir = path.join(root, 'Mara');
  fs.mkdirSync(dir);
  fs.writeFileSync(path.join(dir, 'Mara.png'), PNG_BYTES);
  fs.writeFileSync(path.join(dir, 'portrait.png'), 'stale');
  // A same-ext twin is REAP-only (no move); a different-ext legacy DOES
  // move — the fold reports action for the latter.
  fs.writeFileSync(path.join(dir, 'portrait.jpg'), JPG_BYTES);
  assert.equal(renameLegacyPortraits(dir), true, 'the portrait.jpg fold reports action');
  assert.equal(fs.readFileSync(path.join(dir, 'Mara.png')).length, PNG_BYTES.length,
    'the canonical namesake content survives');
  assert.ok(!fs.existsSync(path.join(dir, 'portrait.png')), 'the stale same-ext twin is reaped');
  assert.ok(fs.existsSync(path.join(dir, 'Mara.jpg')), 'the other-ext legacy folded onto the stem');
});

// ── 4. foreign-stem images are invisible to discovery ─────────────────────

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
        if (lower === 'portrait.ico') { notes.push(`${label}/${stem}: legacy portrait.ico (regenerated on next shortcut build)`); continue; }
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
          // Same-ext legacy twin = invisible cruft (namesake wins) — warn.
          if (EXTS.some((ext) => img === `${stem}.${ext}` && fs.existsSync(path.join(dir, `portrait.${ext}`)))) {
            notes.push(`${label}/${stem}: legacy portrait.${img.split('.').pop()} twin of the namesake (boot migration reaps it)`);
          }
        } else if (isLegacy) {
          notes.push(`${label}/${stem}/${img}: legacy name, loads via the fallback; boot migration folds it onto ${stem}.${img.split('.').pop()}`);
          try {
            assertLoads(path.join(dir, img));
          } catch (e) {
            problems.push(`${label}/${stem}/${img}: ${e.message}`);
          }
        } else {
          problems.push(`${label}/${stem}/${img}: FOREIGN image name — discovery will never find it (expected <stem>.(png|jpg) or portrait.(png|jpg))`);
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
