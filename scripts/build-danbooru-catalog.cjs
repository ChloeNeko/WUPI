#!/usr/bin/env node
// =============================================================
// build-danbooru-catalog.cjs — regenerate PRISM's tag-search catalog.
//
// Converts a full Danbooru tag CSV into the compact JSON the Tag
// Composer's search engine consumes (src/prism/data/danbooru-tags.json).
//
// SOURCE CSV (default, fetched once by the maintainer — the app NEVER
// downloads anything at runtime, portable-offline by design):
//   https://github.com/DominikDoom/a1111-sd-webui-tagcomplete/blob/main/tags/danbooru.csv
//   (community-maintained full danbooru dump: ~140K tags with categories,
//   post counts + aliases; Danbooru shut down their public tag API, so this
//   dump is the standard SD-community source. For a fresher dump see
//   DraconicDragon/danbooru-e621-tag-list-processor.)
//
// CSV shape (no header, count-sorted descending):
//   name,category,post_count,"alias,alias,..."
//   category: 0=general 1=artist 3=copyright 4=character 5=meta
//
// Usage:
//   node scripts/build-danbooru-catalog.cjs <path-to-danbooru.csv>
//
// Output format (compact rows, NOT objects — half the bytes at 140K rows):
//   ["tag", category, postCount, "alias;alias"]
// The quality-meta family (the locked recipe's invisible machinery) is
// EXCLUDED at generation time; composer.js re-filters at load as the
// belt-and-braces guard. Keep the two lists in sync.
// =============================================================

const fs = require('fs');
const path = require('path');

// The engine-injected families under the locked recipe — never searchable,
// never visible (Chloe ruling 2026-08-17; extended same day with the
// subject-gate + rating-steering tags). Space-form names (the CSV source
// form); composer.js's META_EXCLUDED re-filters at load as the
// belt-and-braces guard — keep the two in sync.
const EXCLUDED = new Set([
  // quality ladder + negative block (NoobAI v1.1 recipe)
  'masterpiece', 'best quality', 'amazing quality', 'very aesthetic',
  'absurdres', 'worst quality', 'low quality', 'worst aesthetic',
  'low aesthetic', 'normal quality', 'lowres', 'highres', 'newest',
  'old', 'early', 'signature', 'username', 'logo', 'bad hands', 'mutated hands',
  // crowd-logic subject gate (prism.rs)
  'solo', 'no humans',
  // SFW rating steering (prism.rs)
  'safe', 'nsfw',
]);

// Minimal quoted-CSV field parser (fields may be quoted + contain commas).
function parseCsvLine(line) {
  const fields = [];
  let cur = '';
  let inQuotes = false;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (inQuotes) {
      if (ch === '"') {
        if (line[i + 1] === '"') { cur += '"'; i++; }
        else inQuotes = false;
      } else cur += ch;
    } else if (ch === '"') {
      inQuotes = true;
    } else if (ch === ',') {
      fields.push(cur); cur = '';
    } else cur += ch;
  }
  fields.push(cur);
  return fields;
}

function main() {
  const csvPath = process.argv[2];
  if (!csvPath || !fs.existsSync(csvPath)) {
    console.error('usage: node scripts/build-danbooru-catalog.cjs <path-to-danbooru.csv>');
    process.exit(1);
  }
  const text = fs.readFileSync(csvPath, 'utf8');
  const rows = [];
  let skipped = 0;
  for (const line of text.split(/\r?\n/)) {
    if (!line.trim()) continue;
    const [name, cat, count, aliases] = parseCsvLine(line);
    if (!name || count === undefined || !/^\d+$/.test(count.trim())) { skipped++; continue; }
    if (EXCLUDED.has(name.trim().toLowerCase())) { skipped++; continue; }
    const aliasStr = (aliases || '').split(',').map((a) => a.trim()).filter(Boolean).join(';');
    rows.push([name.trim(), Number(cat.trim()) || 0, Number(count.trim()), aliasStr]);
  }
  const out = {
    version: 2,
    source: 'danbooru tag dump via DominikDoom/a1111-sd-webui-tagcomplete tags/danbooru.csv',
    note: 'PRISM tag-search catalog — the full Danbooru vocabulary for the Tag Composer search engine (NoobAI-XL 1.1 is a danbooru-tag model). Row shape: [tag, category, postCount, "alias;alias"]. Categories: 0 general, 1 artist, 3 copyright, 4 character, 5 meta. The naiXL quality-meta family is EXCLUDED (engine-injected, invisible — Chloe ruling 2026-08-17). Regenerate via scripts/build-danbooru-catalog.cjs.',
    tags: rows,
  };
  const dest = path.join(__dirname, '..', 'src', 'prism', 'data', 'danbooru-tags.json');
  fs.writeFileSync(dest, JSON.stringify(out));
  console.log(`wrote ${rows.length} tags (${skipped} skipped) -> ${dest} (${(fs.statSync(dest).size / 1048576).toFixed(1)} MB)`);
}

main();
