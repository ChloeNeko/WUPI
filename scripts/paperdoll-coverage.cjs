#!/usr/bin/env node
// paperdoll-coverage.cjs — verify paperdoll hitbox polygons against the PNGs.
//
// For each polygon, computes (from the PNG alpha channel as ground truth):
//   fillEff  — fraction of the polygon's area that is inside the silhouette
//              (low = polygon floats off the body)
//   coverage — fraction of the silhouette in the polygon's bbox that the
//              polygon covers (low = body part under-covered)
//   crossings — non-adjacent edge crossings (garbled/self-intersecting shape)
//
// Flags: SELF-INTERSECT (crossings>0), MISALIGNED (fillEff<0.6 OR coverage<0.5).
// Note: some "MISALIGNED" flags are benign overlap-with-neighbor artifacts
// (e.g. the chest polygon already covers the shoulder area, so the shoulder
// polygon's coverage reads low). Read flags alongside the bbox coordinates.
//
// Usage: node scripts/paperdoll-coverage.cjs
// (pngjs required: npm i pngjs --no-save in a temp dir + NODE_PATH, or global)
const fs = require('fs');
const path = require('path');
let PNG;
try { PNG = require('pngjs').PNG; }
catch (e) {
  console.error('pngjs not found. Install: npm i pngjs --no-save');
  process.exit(1);
}

const ROOT = path.resolve(__dirname, '..');
const LEFT_DRAWER = path.join(ROOT, 'src/fable/engine/left-drawer.js');
const FEM_PNG = path.join(ROOT, 'src/fable/assets/paperdoll_female.png');
const MALE_PNG = path.join(ROOT, 'src/fable/assets/paperdoll_male.png');

// Extract the FEMALE_POLYS / MALE_POLYS arrays from left-drawer.js by brace matching.
function extractArray(src, name) {
  const start = src.indexOf('const ' + name + ' = [');
  if (start < 0) throw new Error(name + ' not found in left-drawer.js');
  let i = src.indexOf('[', start), depth = 0, end = -1;
  for (let j = i; j < src.length; j++) {
    if (src[j] === '[') depth++;
    else if (src[j] === ']') { depth--; if (depth === 0) { end = j; break; } }
  }
  // eval the array literal in a sandbox that provides the `reads`-bearing objects
  const body = src.slice(i, end + 1);
  return eval(body);
}

function loadSil(p) {
  const png = PNG.sync.read(fs.readFileSync(p));
  const { width: W, height: H, data } = png;
  return { W, H, isOpaque: (x, y) => x >= 0 && x < W && y >= 0 && y < H && data[(y * W + x) * 4 + 3] > 32 };
}
function inPoly(pt, pts) {
  const [x, y] = pt; let inside = false;
  for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) {
    const [xi, yi] = pts[i], [xj, yj] = pts[j];
    if (((yi > y) !== (yj > y)) && (x < (xj - xi) * (y - yi) / ((yj - yi) || 1e-9) + xi)) inside = !inside;
  }
  return inside;
}
function crossings(pts) {
  const n = pts.length; let c = 0;
  const si = (a, b, cc, d) => {
    const det = (b[0]-a[0])*(d[1]-cc[1]) - (b[1]-a[1])*(d[0]-cc[0]);
    if (Math.abs(det) < 1e-9) return false;
    const t = ((cc[0]-a[0])*(d[1]-cc[1]) - (cc[1]-a[1])*(d[0]-cc[0])) / det;
    const u = ((cc[0]-a[0])*(b[1]-a[1]) - (cc[1]-a[1])*(b[0]-a[0])) / det;
    return t > 0.001 && t < 0.999 && u > 0.001 && u < 0.999;
  };
  for (let i = 0; i < n; i++) for (let j = i + 2; j < n; j++) {
    if (j === n - 1 && i === 0) continue;
    if (si(pts[i], pts[(i+1)%n], pts[j], pts[(j+1)%n])) c++;
  }
  return c;
}
function bbox(pts) {
  const xs = pts.map(p => p[0]), ys = pts.map(p => p[1]);
  return { x0: Math.min(...xs), x1: Math.max(...xs), y0: Math.min(...ys), y1: Math.max(...ys) };
}
function analyze(name, silPath, polys) {
  const sil = loadSil(silPath);
  console.log(`\n===== ${name} =====`);
  for (const p of polys) {
    const pts = p.points.trim().split(/\s+/).map(c => c.split(',').map(Number));
    const bb = bbox(pts);
    const cr = crossings(pts);
    let polyArea = 0, polyInSil = 0, silInPoly = 0, silInBBox = 0;
    for (let y = bb.y0; y <= bb.y1; y++) for (let x = bb.x0; x <= bb.x1; x++) {
      const inP = inPoly([x, y], pts), inS = sil.isOpaque(x, y);
      if (inP) { polyArea++; if (inS) polyInSil++; }
      if (inS) { silInBBox++; if (inP) silInPoly++; }
    }
    const fillEff = polyArea > 0 ? (polyInSil / polyArea) : 0;
    const coverage = silInBBox > 0 ? (silInPoly / silInBBox) : 0;
    const flag = cr > 0 ? 'SELF-INTERSECT' : (fillEff < 0.6 || coverage < 0.5 ? 'MISALIGNED' : 'ok');
    const mark = (cr > 0 || fillEff < 0.6 || coverage < 0.5) ? ` <<<<< ${flag}` : '';
    console.log(`  ${p.id.padEnd(16)} bb=[${bb.x0},${bb.y0}-${bb.x1},${bb.y1}] crossings=${cr} fillEff=${fillEff.toFixed(2)} coverage=${coverage.toFixed(2)}${mark}`);
  }
}

const src = fs.readFileSync(LEFT_DRAWER, 'utf8');
analyze('FEMALE', FEM_PNG, extractArray(src, 'FEMALE_POLYS'));
analyze('MALE', MALE_PNG, extractArray(src, 'MALE_POLYS'));
