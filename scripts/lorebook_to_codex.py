#!/usr/bin/env python3
"""
Convert a SillyTavern lorebook JSON into a WUPI compound .codex file.

Output format matches `parse_compound_file` in src-tauri/src/codex.rs:214:
each entry is a YAML-ish front-matter block (`---` opener + `title:`/`tags:`
lines) followed by a blank line and the body. Entries are separated by a
blank line + `---` opener. The body is preserved VERBATIM from the source
lorebook (fidelity-first: the user authored this lorebook deliberately).

Transformations are the minimum needed for a clean codex:
  1. Title  = lorebook `name`/`comment` with decorative SillyTavern
     markers stripped (emoji icons, [☰]/[W]/[❗] tags, ═ dividers, leading
     [›]/[▸] bullets). Falls back to a sensible id-derived title.
  2. Tags   = the entry's `keys` (SillyTavern triggers) + the top-level
     lorebook name, lowercased + sanitized. Tags drive WUPI's keyword
     retrieval, so folding the triggers in keeps the same recall behavior.
  3. Body   = `content` verbatim. Empty-content entries are skipped
     (mirrors parse_compound_file's empty-body skip at codex.rs:244).

The compound file is NOT auto-loaded by WUPI (only wupi.codex/fable.codex
are engine-seeded). It's a portable artifact for review/backup/archival;
wiring a generic .codex loader is a separate follow-up.
"""
import json
import re
import sys
from pathlib import Path


# ---------------------------------------------------------------------------
# Title cleaning
# ---------------------------------------------------------------------------

# Leading bullet/diamond markers SillyTavern authors use to organize entries.
_LEADING_MARKERS = re.compile(r"""^[▸›•·▪◦○\-\u200b\u00a0\s]*""")

# Decorative bracketed tags authors prefix/suffix names with: [☰], [W], [❗],
# [🌊🇬🇱-🇳], [📍], etc. Match at EITHER end so symmetric names like
# "[☰] World Setting [☰]" clean to "World Setting". Strip the whole bracket
# (the inner text is decorative, e.g. an icon or section code).
_DECORATIVE_BRACKETS_LEAD = re.compile(r"^\[[^\]]*\]\s*")
_DECORATIVE_BRACKETS_TRAIL = re.compile(r"\s*\[[^\]]*\]$")

# Fancy divider lines: ═══════[Setting]═══════ → "Setting".
# Match a run of box-drawing / equals / dash chars, optional bracketed label,
# then another run. Capture the label.
_DIVIDER = re.compile(
    r"^[\s=─━═\-]*\[?([^\[\]=─━═\-]+?)\]?[\s=─━═\-]*$"
)

# Collapse repeated whitespace inside a cleaned title.
_WS = re.compile(r"\s+")


def clean_title(raw: str, fallback: str) -> str:
    """Reduce a SillyTavern entry name/comment to a clean codex title."""
    if not raw:
        return fallback
    t = raw.strip()
    if not t:
        return fallback
    # Strip decorative bracket markers ([☰], [W], [🌊🇬🇱-🇳], ...) from BOTH
    # ends FIRST — symmetric names like "[☰] World Setting [☰]" need both
    # stripped, and a leading section code like "[▸] ═══[Races]═══" must lose
    # the "[▸] " prefix before the divider pattern below can match the core.
    while True:
        new = _DECORATIVE_BRACKETS_LEAD.sub("", t)
        new = _DECORATIVE_BRACKETS_TRAIL.sub("", new)
        if new == t:
            break
        t = new
    # Divider line like "═══════[Setting]═══════" → "Setting". Run AFTER the
    # bracket strip so a bracketed section code doesn't block the match. When
    # the captured label is itself bracket-wrapped ("═══[Races]═══"), the
    # regex's `\[?`...`\]?` already consumes those brackets, so group(1) is
    # the clean label.
    m = _DIVIDER.match(t)
    if m and ("═" in t or "─" in t or "━" in t) and m.group(1).strip():
        t = m.group(1).strip()
    # A residual isolated bracket pair around the whole name (e.g. "[Races]"
    # with no box-drawing core) — strip brackets one more time.
    t = _DECORATIVE_BRACKETS_LEAD.sub("", t)
    t = _DECORATIVE_BRACKETS_TRAIL.sub("", t)
    # Strip leading bullet/diamond markers.
    t = _LEADING_MARKERS.sub("", t)
    # Strip trailing markers / whitespace.
    t = t.strip(" \t\u200b\u00a0▪▸›•·◦○-─━═")
    t = _WS.sub(" ", t).strip()
    # Drop leftover stray emoji that survived as the whole title.
    if not re.search(r"[A-Za-z0-9]", t):
        return fallback
    # Titles shouldn't contain newlines or colons-newline (front-matter safe).
    t = t.replace("\n", " ").replace("\r", " ")
    t = _WS.sub(" ", t).strip()
    return t or fallback


# ---------------------------------------------------------------------------
# Tag cleaning
# ---------------------------------------------------------------------------

def sanitize_tag(tag: str) -> str:
    """Lowercase, collapse internal whitespace, strip awkward chars. Keep
    CJK + accented Latin (the lorebook has Japanese trigger terms)."""
    if tag is None:
        return ""
    t = str(tag).strip().lower()
    if not t:
        return ""
    # Drop surrounding quotes / brackets an author may have left in.
    t = t.strip("\"'`[](){}")
    t = _WS.sub(" ", t).strip()
    # Tag list in front-matter is comma-separated; a tag containing a comma
    # would split wrongly. Replace commas with spaces.
    t = t.replace(",", " ")
    t = _WS.sub(" ", t).strip()
    return t


def collect_tags(entry: dict, lorebook_name: str, seen: set, include_tags: bool = False) -> list:
    """Build the codex tag list.

    WUPI codex retrieval is SEMANTIC (bge-small cosine over the body text),
    NOT keyword-triggered — see src-tauri/src/memory.rs (the `tags` live in
    `metadata_json`, which is opaque to retrieval and never embedded). So by
    default this returns an EMPTY list: the SillyTavern `keys` and the
    lorebook name are dead weight in a WUPI codex, not retrieval triggers.
    A global "one piece" tag on every entry does nothing (it's never matched
    against) and only clutters the file.

    Pass `include_tags=True` to opt back into emitting the lorebook name +
    per-entry `keys`/`key`/`secondary_keys` as tags (useful only if the
    output is later consumed by a keyword-based system, not WUPI). Dedupes
    case-insensitively, preserves first-seen order."""
    if not include_tags:
        return []
    tags = []
    # Global "this is One Piece lore" tag first.
    g = sanitize_tag(lorebook_name)
    src_keys = []
    # `keys` is the modern ST field; `key` is the legacy field.
    for field in ("keys", "key", "secondary_keys"):
        v = entry.get(field)
        if isinstance(v, list):
            src_keys.extend(v)
        elif isinstance(v, str):
            src_keys.append(v)
    # Stable order: global, then keys as authored.
    if g:
        src_keys.insert(0, g)
    for k in src_keys:
        s = sanitize_tag(k)
        if not s:
            continue
        key = s.casefold()
        if key in seen:
            continue
        seen.add(key)
        tags.append(s)
    return tags


# ---------------------------------------------------------------------------
# Front-matter escaping
# ---------------------------------------------------------------------------

def fm_escape(value: str) -> str:
    """Make a value safe for a single YAML-ish front-matter line.
    parse_front_matter (codex.rs:669) splits on the first ':'; wrap any value
    containing a colon or leading special char in double quotes so the split
    lands on the field colon, not an interior one."""
    v = value.strip()
    if v == "":
        return '""'
    if ":" in v or v[0] in "\"'{}[]#&*!|>%@`":
        # Escape interior double quotes + wrap.
        inner = v.replace('"', '\\"')
        return f'"{inner}"'
    return v


# ---------------------------------------------------------------------------
# Body splitting (bge-small embedding budget)
# ---------------------------------------------------------------------------
#
# bge-small truncates silently at 512 tokens (~1400 chars of prose). A long
# body gets a garbage embedding and scores near the retrieval floor even on a
# perfect match, so entries over the budget MUST be split into <1400-char
# parts. Each part becomes its own codex entry (same tags + a "(Part N)" title
# suffix) so every part is independently retrievable.
#
# Boundary priority (break at the cleanest available seam, never mid-word):
#   1. BLANK-LINE PARAGRAPHS — preferred. Entries are XML-tagged prose with
#      heading-led paragraphs ("Definition", "Role", "Status"...) that read as
#      self-contained units. Greedily pack whole paragraphs under the target.
#   2. SENTENCES — for a single paragraph over the budget (rare; one entry in
#      this lorebook). Split on sentence terminators, keep the punctuation.
#   3. HARD WRAP — last resort, never observed in practice here.
#
# The target leaves headroom under 1400 so the part is never borderline.

SPLIT_TARGET = 1380  # chars; comfortably under bge-small's ~1400-char budget


def _split_keep(pattern, text):
    """Split `text` on `pattern`, returning units that each INCLUDE their
    trailing delimiter (the matched separator stays attached to the unit
    before it). Concatenating all returned units reconstructs `text`
    exactly — byte-perfect — so split parts never diverge from the source
    on whitespace. Mirrors how paragraph splitting preserves the `\\n\\n`."""
    # re.split with one capturing group returns [pre, delim, post, delim, ...].
    # Walk it pairing each non-delim segment with the delimiter that follows.
    pieces = _re_split_capture(pattern, text)
    return pieces


def _re_split_capture(pattern, text):
    import re as _re
    if isinstance(pattern, str):
        pattern = _re.compile(pattern)
    # Find all split points (match spans).
    result = []
    last = 0
    for m in pattern.finditer(text):
        start, end = m.span()
        unit = text[last:end]  # text up to + INCLUDING the delimiter
        if unit:
            result.append(unit)
        last = end
    if last < len(text):
        result.append(text[last:])
    return result


def _pack(units, target, joiner):
    """Greedily pack `units` (strs) into chunks each <= `target` chars.
    `joiner` is used ONLY between units that didn't already carry their own
    trailing delimiter (the paragraph-boundary path); the sentence path
    passes units that already include their delimiter and uses joiner="".
    A unit larger than `target` passes through whole (caller handles the
    cascade)."""
    chunks = []
    cur = ""
    for u in units:
        if not cur:
            cur = u
            continue
        candidate = cur + joiner + u
        if len(candidate) <= target:
            cur = candidate
        else:
            chunks.append(cur)
            cur = u
    if cur:
        chunks.append(cur)
    return chunks


def split_body(body: str, target: int = SPLIT_TARGET):
    """Split `body` into <=target-char parts at the cleanest available seam.
    Returns the list of part strings. A body already <= target returns as a
    single-element list (caller treats len==1 as 'no split needed').

    Reconstruction is BYTE-PERFECT: concatenating all returned parts (no
    joiner — each part carries its own trailing separator) reproduces the
    original `body` exactly, so splitting never alters the source prose, not
    even whitespace."""
    if len(body) <= target:
        return [body]

    # Split into paragraph units that each INCLUDE their trailing separator,
    # so concatenation reconstructs `body` byte-for-byte. The separator regex
    # matches the blank line between paragraphs (two+ newlines), keeping it
    # attached to the preceding paragraph. This preserves trailing spaces /
    # single-newline idiosyncrasies an author may have inside a paragraph.
    para_units = _split_keep(r"\n\n+", body)

    # Tier 1: if every paragraph (separator included) fits the target, pack
    # at paragraph boundaries with NO joiner (separators are already attached).
    if all(len(u) <= target for u in para_units):
        return _pack(para_units, target, "")

    # Tier 2: a paragraph itself exceeds the target. Sub-split ONLY the
    # oversized units at sentence boundaries (preserving their separators),
    # leave small ones whole, then pack the mixed stream with no joiner.
    # Sentence splits keep the matched whitespace attached to the preceding
    # unit, so `:\n` stays `:\n` (never `:\n\n`).
    sent_pattern = r"(?<=[.!?])\s+(?=[A-Z\"\u201c(])|(?<=:)\n"
    units = []
    for u in para_units:
        if len(u) <= target:
            units.append(u)
        else:
            units.extend(_split_keep(sent_pattern, u))
    packed = _pack(units, target, "")

    # Tier 3: any residual unit over target (a sentence longer than target —
    # doesn't happen in this lorebook, but guard against silent corruption)
    # gets a hard wrap on word boundaries as a last resort. NOTE: a hard wrap
    # inserts a space, so reconstruction of such a unit is no longer exact;
    # acceptable as an unreachable last resort (no unit here is that long).
    final = []
    for chunk in packed:
        if len(chunk) <= target:
            final.append(chunk)
        else:
            final.extend(_hard_wrap(chunk, target))
    return final


def _hard_wrap(text: str, target: int):
    """Word-boundary wrap. Absolute last resort."""
    words = text.split()
    chunks = []
    cur = ""
    for w in words:
        if cur and len(cur) + 1 + len(w) > target:
            chunks.append(cur)
            cur = w
        else:
            cur = w if not cur else cur + " " + w
    if cur:
        chunks.append(cur)
    return chunks


# ---------------------------------------------------------------------------
# Conversion
# ---------------------------------------------------------------------------

def convert(lorebook_path: Path, out_path: Path, include_tags: bool = False) -> dict:
    with lorebook_path.open(encoding="utf-8") as f:
        data = json.load(f)

    lorebook_name = data.get("name") or data.get("description") or "Lorebook"
    entries = data.get("entries", {})
    if isinstance(entries, list):
        # Some exporters use a list; key by index.
        entries = {str(i): e for i, e in enumerate(entries)}

    # Stable order: by insertion order (dict preserves it in py3.7+), which
    # for ST lorebooks is roughly the author's uid order. We additionally sort
    # by the numeric uid string when present for a stable, reviewable layout.
    def uid_key(item):
        uid = item[1].get("uid", item[1].get("id", item[0]))
        try:
            return (0, int(uid))
        except (TypeError, ValueError):
            return (1, str(item[0]))

    ordered = sorted(entries.items(), key=uid_key)

    blocks = []
    skipped_empty = 0
    skipped_disabled = 0
    written = 0
    split_count = 0  # source entries that were split into >1 part
    long_entries = []  # entries still over budget after splitting (shouldn't happen)

    for _uid, e in ordered:
        if not e.get("enabled", True) or e.get("disable", False):
            skipped_disabled += 1
            continue
        content = (e.get("content") or "").strip()
        if not content:
            skipped_empty += 1
            continue

        raw_name = e.get("name") or e.get("comment") or ""
        uid_label = e.get("uid", e.get("id", _uid))
        fallback = f"One Piece Entry {uid_label}"
        title = clean_title(raw_name, fallback)

        seen = set()
        tags = collect_tags(e, lorebook_name, seen, include_tags=include_tags)

        # Split oversized bodies so every part fits the bge-small embedding
        # budget. A body under budget returns as a single part (no suffix).
        parts = split_body(content)
        if len(parts) > 1:
            split_count += 1

        for idx, body in enumerate(parts):
            # Each part may carry its trailing paragraph separator (kept for
            # byte-perfect reconstruction via "".join(parts)); strip it for
            # clean codex output so the block doesn't end in blank lines.
            body_out = body.rstrip()
            if len(body_out) > 1400:
                long_entries.append((title, len(body_out)))

            part_title = title
            if len(parts) > 1:
                part_title = f"{title} (Part {idx + 1} of {len(parts)})"

            fm = []
            fm.append("---")
            fm.append(f"title: {fm_escape(part_title)}")
            if tags:
                # Only emit a tags line when there are tags to emit. WUPI's
                # codex retrieval is semantic (bge-small cosine over the body),
                # so tags are cosmetic — emitted only when include_tags=True.
                fm.append(f"tags: {fm_escape(', '.join(tags))}")
            # When tags is empty, OMIT the line entirely (parse_front_matter
            # defaults missing tags to an empty Vec — a clean `tags: []` line
            # would be pure noise). The parser handles a title-only front-matter.
            fm.append("---")
            header = "\n".join(fm)

            blocks.append(f"{header}\n\n{body_out}")
            written += 1

    # Join with a blank line + the next block's `---` opener. The parser's
    # split_compound (codex.rs:265) treats `---` preceded by a blank line as
    # an entry-start fence; a single blank line between blocks satisfies that
    # and keeps the file human-readable.
    out_text = "\n\n".join(blocks) + "\n"

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(out_text, encoding="utf-8")

    return {
        "lorebook_name": lorebook_name,
        "total_source_entries": len(entries),
        "written": written,
        "split_count": split_count,
        "skipped_empty": skipped_empty,
        "skipped_disabled": skipped_disabled,
        "long_entries": long_entries,
        "out_path": str(out_path),
        "out_chars": len(out_text),
    }


def main(argv):
    # --tags: opt back into emitting the lorebook name + per-entry keys as tags.
    # Off by default — WUPI codex retrieval is semantic (bge-small cosine over
    # the body), NOT keyword-triggered, so tags are dead weight. See the doc on
    # collect_tags.
    include_tags = "--tags" in argv
    argv = [a for a in argv if a != "--tags"]

    if len(argv) >= 3:
        src = Path(argv[1])
        dst = Path(argv[2])
    else:
        src = Path(r"C:\Users\Chloe\Downloads\One Piece.json")
        dst = Path(r"C:\WUPI\data\One Piece.codex")

    if not src.exists():
        print(f"ERROR: source not found: {src}", file=sys.stderr)
        return 2

    stats = convert(src, dst, include_tags=include_tags)
    print("=== Lorebook → Codex conversion ===")
    print(f"  source          : {src}")
    print(f"  lorebook name   : {stats['lorebook_name']}")
    print(f"  source entries  : {stats['total_source_entries']}")
    print(f"  codex entries   : {stats['written']}")
    print(f"  tags            : {'emitted (lorebook name + per-entry keys)' if include_tags else 'omitted (WUPI retrieval is semantic, not keyword-triggered)'}")
    if stats["split_count"]:
        print(f"  entries split   : {stats['split_count']} (each emitted as multiple <=1400-char parts)")
    print(f"  skipped empty   : {stats['skipped_empty']}")
    print(f"  skipped disabled: {stats['skipped_disabled']}")
    print(f"  output          : {stats['out_path']}")
    print(f"  output size     : {stats['out_chars']:,} chars")
    if stats["long_entries"]:
        over = len(stats["long_entries"])
        print(f"  WARNING — bodies still >1400 chars after split (bge-small will truncate): {over}")
        for title, length in sorted(stats["long_entries"], key=lambda x: -x[1])[:5]:
            print(f"     - {length:5d} chars  {title}")
    else:
        print("  all bodies <=1400 chars (bge-small grabs 100%).")
    print("Done.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
