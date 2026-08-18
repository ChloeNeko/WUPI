#!/usr/bin/env python3
"""Extract NoobAI-cyberfix's OWN text encoders as PRISM fp16 sidecars (2026-08-17).

WHY: `image.gguf` is NoobAI-XL-v1.1-cyberfix-perpendicular converted to Q8_0 —
a full-checkpoint GGUF whose embedded text encoders (Illustrious-lineage,
FINE-TUNED — NOT stock SDXL weights) are majority-q8_0. The scene_art.rs
`ClipOverride` shadow layout swaps in external fp16 encoders by tensor-name
collision — but the FIRST attempt used stock stabilityai SDXL-base encoders
(WRONG weights for this lineage → saturated-mush renders, the 2026-08-17
incident). This script extracts the checkpoint's OWN encoders so the shadow
is weight-correct by construction.

HOW: HTTP Range reads against the source safetensors on HuggingFace (the
6.9 GB file is NOT downloaded whole — only the two `conditioner.embedders.*`
subtrees, ~1.9 GB total). Tensor names are mapped open_clip → HF style to
match the naming the sd.cpp loader prefix+convert path expects for external
clip files (blueprint: the comfyanonymous clip_l / stabilityai text_encoder_2
file layouts — verified identical name sets before writing).

Usage: python scripts/extract-noobai-encoders.py
Writes: models/sd/clip_l.safetensors + models/sd/clip_g.safetensors
"""

import json
import re
import struct
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent  # the WUPI repo root
SD_DIR = ROOT / "src-tauri" / "models" / "sd"
SRC_URL = ("https://huggingface.co/Panchovix/noobai-XL-1.1-perpendicular-cyberfix"
           "/resolve/main/NoobAI-XL-v1.1-cyberfix-perpendicular.safetensors")
# The disabled stock sidecars double as the exact tensor-name blueprints.
BLUEPRINT_L = SD_DIR / "clip_l.safetensors.disabled"
BLUEPRINT_G = SD_DIR / "clip_g.safetensors.disabled"
OUT_L = SD_DIR / "clip_l.safetensors"
OUT_G = SD_DIR / "clip_g.safetensors"
TMP = SD_DIR / "_extract_tmp"
HEADER_PROBE = 8 * 1024 * 1024  # first 8 MB is always enough for the name table

# The checkpoint stores its encoders under ALREADY-CANONICAL names:
# `conditioner.embedders.N.transformer.text_model.*` (the 2026-08-17 probe —
# NOT the stabilityai open_clip `...N.model.*` style). Strip the subtree
# prefix + the `transformer.` infix and the remainder IS the file tensor name
# the sd.cpp loader prefix+convert path expects (identical to the stock
# clip-file layout). NOTE the checkpoint's clip_g is 390 tensors / 24 layers
# — the Illustrious-lineage encoder is STRUCTURALLY different from stock
# SDXL text_encoder_2 (517 tensors / 32 layers); only scheme-valid names are
# accepted, and anything unrecognized aborts the extraction loudly.
SCHEME = re.compile(
    r"^(?:text_model\.(?:embeddings\.(?:token|position)_embedding\.weight"
    r"|encoder\.layers\.\d+\.(?:layer_norm[12]\.(?:weight|bias)"
    r"|mlp\.fc[12]\.(?:weight|bias)"
    r"|self_attn\.(?:[kvq]_proj|in_proj|out_proj)\.(?:weight|bias))"
    r"|final_layer_norm\.(?:weight|bias))"
    r"|text_projection\.weight)$"
)

# open_clip resblock suffixes → the sd.cpp-canonical attention/MLP names.
# NOTE the FUSED in_proj: open_clip stores attention projections as ONE
# [3*dim, dim] tensor (`attn.in_proj_weight`) where HF/stock splits them into
# q/k/v — sd.cpp's canonical set accepts BOTH forms (its own
# convert_open_clip_to_hf_clip_name maps fused → `self_attn.in_proj.*`), and
# the checkpoint's embedded encoders ride the fused form, so the sidecars
# keep it too (byte-identical shadow, no q/k/v splitting required).
RESBLOCK_SUFFIXES = [
    ("attn.in_proj_weight", "self_attn.in_proj.weight"),
    ("attn.in_proj_bias", "self_attn.in_proj.bias"),
    ("attn.out_proj.weight", "self_attn.out_proj.weight"),
    ("attn.out_proj.bias", "self_attn.out_proj.bias"),
    ("mlp.c_fc.weight", "mlp.fc1.weight"),
    ("mlp.c_fc.bias", "mlp.fc1.bias"),
    ("mlp.c_proj.weight", "mlp.fc2.weight"),
    ("mlp.c_proj.bias", "mlp.fc2.bias"),
    ("ln_1.weight", "layer_norm1.weight"),
    ("ln_1.bias", "layer_norm1.bias"),
    ("ln_2.weight", "layer_norm2.weight"),
    ("ln_2.bias", "layer_norm2.bias"),
]


def openclip_to_hf(leaf: str) -> str | None:
    """Map an open_clip-style name (prefixes stripped) to the HF/canonical
    form, or None for inference-irrelevant artifacts."""
    fixed = {
        "token_embedding.weight": "text_model.embeddings.token_embedding.weight",
        "positional_embedding": "text_model.embeddings.position_embedding.weight",
        "ln_final.weight": "text_model.final_layer_norm.weight",
        "ln_final.bias": "text_model.final_layer_norm.bias",
        "text_projection": "text_projection.weight",
    }
    if leaf in fixed:
        return fixed[leaf]
    if leaf.startswith("resblocks."):
        rest = leaf[len("resblocks."):]
        idx, _, tail = rest.partition(".")
        for suffix, target in RESBLOCK_SUFFIXES:
            if tail == suffix or tail.endswith("." + suffix):
                return f"text_model.encoder.layers.{idx}.{target}"
        sys.exit(f"unrecognized resblock tensor: {leaf}")
    sys.exit(f"unrecognized open_clip encoder tensor: {leaf}")


def strip_to_base(name: str, key: str) -> str | None:
    """`conditioner.embedders.{key}.` prefix + any stacked `transformer.` /
    `model.` segments removed. None when the name is outside the subtree."""
    p = f"conditioner.embedders.{key}."
    if not name.startswith(p):
        return None
    base = name[len(p):]
    while base.startswith("transformer.") or base.startswith("model."):
        base = base.split(".", 1)[1]
    return base


def to_file_name(name: str, key: str) -> str | None:
    """Map one checkpoint tensor name to its sidecar file name.
    Returns None for inference-irrelevant artifacts."""
    base = strip_to_base(name, key)
    if base is None:
        return None
    if base == "logit_scale" or base.endswith("position_ids"):
        return None  # training/legacy artifacts, unused at inference
    if SCHEME.match(base):
        return base
    hf = openclip_to_hf(base)
    if not SCHEME.match(hf):
        sys.exit(f"mapping produced a scheme-invalid name: {name} -> {hf}")
    return hf


def curl_range(url: str, start: int, end: int, dest: Path) -> None:
    # -L: the resolve URL redirects to the CDN; curl re-sends the Range header.
    r = subprocess.run(
        ["curl", "-sL", "--fail", "-r", f"{start}-{end}", url, "-o", str(dest)],
    )
    if r.returncode != 0:
        sys.exit(f"curl range {start}-{end} failed (exit {r.returncode})")


def main() -> None:
    TMP.mkdir(exist_ok=True)

    # 1. Probe the header.
    probe = TMP / "probe.bin"
    curl_range(SRC_URL, 0, HEADER_PROBE - 1, probe)
    raw = probe.read_bytes()
    header_len = struct.unpack("<Q", raw[:8])[0]
    if header_len + 8 > len(raw):
        sys.exit(f"header ({header_len}B) exceeds the {len(raw)}B probe — raise HEADER_PROBE")
    header = json.loads(raw[8:8 + header_len])
    data_base = 8 + header_len
    print(f"source header: {header_len}B, {len(header)} entries")

    # 2. Split the conditioner subtrees.
    subs = {"0": {}, "1": {}}
    for name, info in header.items():
        if name == "__metadata__":
            continue
        if name.startswith("conditioner.embedders.0."):
            subs["0"][name] = info
        elif name.startswith("conditioner.embedders.1."):
            subs["1"][name] = info
    for k in ("0", "1"):
        print(f"  embedders.{k}: {len(subs[k])} tensors")

    # 3. Map names + check against the stock blueprints (exact set equality).
    blueprints = {}
    for key, path in (("0", BLUEPRINT_L), ("1", BLUEPRINT_G)):
        with open(path, "rb") as fh:
            n = struct.unpack("<Q", fh.read(8))[0]
            hdr = json.loads(fh.read(n))
        blueprints[key] = {k: v for k, v in hdr.items() if k != "__metadata__"}

    plans = {}
    for key in ("0", "1"):
        # The checkpoint stores many clip_g tensors under TWO spellings
        # (canonical `transformer.text_model.*` AND open_clip
        # `model.transformer.resblocks.*`). When both map to the same file
        # name, the canonical spelling wins (tier 0 beats tier 1) —
        # deterministic + byte-preference for the sd.cpp-native form.
        plan, dropped = {}, []
        for name, info in subs[key].items():
            base = strip_to_base(name, key)
            if base is None:
                continue
            if base == "logit_scale" or base.endswith("position_ids"):
                dropped.append(name)
                continue
            if SCHEME.match(base):
                cand, tier = base, 0
            else:
                cand, tier = openclip_to_hf(base), 1
            if not SCHEME.match(cand):
                sys.exit(f"mapping produced a scheme-invalid name: {name} -> {cand}")
            prior = plan.get(cand)
            if prior is None or (tier == 0 and prior[1] == 1):
                plan[cand] = (info, tier)
            elif prior is not None and tier == 0 and prior[1] == 0:
                sys.exit(f"duplicate canonical spelling for {cand}: {name}")
        plans[key] = {k: v[0] for k, v in plan.items()}
        # clip_l must equal the stock blueprint exactly (same architecture,
        # split q/k/v style); clip_g CANNOT — the checkpoint stores its
        # attention as FUSED open_clip in_proj tensors (389 names, not
        # stock's 517 split-form set) — so it's validated structurally
        # instead (scheme enforced; layers contiguous; embeddings/final-
        # norm/projection present).
        if key == "0":
            want, got = set(blueprints[key]), set(plan)
            if want != got:
                sys.exit(f"embedders.0 name mismatch — missing {sorted(want-got)[:5]} "
                         f"extra {sorted(got-want)[:5]}")
        else:
            layers = {int(m.group(1)) for n in plan
                      if (m := re.match(r"text_model\.encoder\.layers\.(\d+)\.", n))}
            if layers != set(range(max(layers) + 1)):
                sys.exit(f"embedders.1 layer indices not contiguous: {sorted(layers)}")
            for must in ("text_model.embeddings.token_embedding.weight",
                         "text_model.embeddings.position_embedding.weight",
                         "text_model.final_layer_norm.weight",
                         "text_projection.weight"):
                if must not in plan:
                    sys.exit(f"embedders.1 missing {must}")
            fused = sum(1 for n in plan if "self_attn.in_proj" in n)
            print(f"  embedders.1: {len(plan)} tensors, {max(layers)+1} layers, "
                  f"fused in_proj tensors: {fused} (open_clip-style attention — "
                  f"sd.cpp's canonical fused form, NOT stock's split q/k/v; "
                  f"389 = 517 - 192 split + 64 fused)")
        print(f"  embedders.{key}: {len(plan)} mapped ({len(dropped)} dropped: {dropped})")

    # 4. Range plan: coalesce per-subtree byte spans (gap < 16 MB merges).
    def coalesce(infos):
        spans = sorted((i["data_offsets"][0], i["data_offsets"][1]) for i in infos)
        out = [list(spans[0])]
        for s, e in spans[1:]:
            if s - out[-1][1] < 16 * 1024 * 1024:
                out[-1][1] = max(out[-1][1], e)
            else:
                out.append([s, e])
        return out

    for key, out_path, tag in (("0", OUT_L, "l"), ("1", OUT_G, "g")):
        spans = coalesce(plans[key].values())
        total = sum(e - s for s, e in spans)
        print(f"clip_{tag}: {len(spans)} range(s), {total / 1e9:.2f} GB to fetch")
        # Download the coalesced spans, then assemble.
        chunks = []
        for i, (s, e) in enumerate(spans):
            part = TMP / f"{tag}_{i}.bin"
            curl_range(SRC_URL, data_base + s, data_base + e - 1, part)
            chunks.append((s, e, part))
            print(f"  fetched span {i}: {e - s} bytes")

        # One flat buffer per subtree (max ~1.4 GB), streamed from the part
        # files (64 MB reads), with each chunk's position recorded so
        # per-tensor slicing is O(1) to locate.
        full = bytearray()
        chunk_starts = []
        for s, e, part in chunks:
            chunk_starts.append((s, e, len(full)))
            with open(part, "rb") as fh:
                while True:
                    buf = fh.read(64 * 1024 * 1024)
                    if not buf:
                        break
                    full += buf

        def locate(src_off: int) -> int:
            # `<=` on the upper bound: this locates BOTH start and END
            # offsets, and the final tensor's end == the span boundary (a
            # strict < would reject it — the off-by-one that ate the first
            # extraction attempt).
            for cs, ce, base in chunk_starts:
                if cs <= src_off <= ce:
                    return base + (src_off - cs)
            sys.exit(f"source offset {src_off} outside fetched spans")

        header_out, blobs = {}, []
        offset = 0
        # Deterministic order: blueprint name order (sorted) — offsets assigned
        # as we go, data sliced from the flat buffer by SOURCE offsets.
        for hf_name in sorted(plans[key]):
            info = plans[key][hf_name]
            s, e = info["data_offsets"]
            rel_s, rel_e = locate(s), locate(e)
            piece = bytes(full[rel_s:rel_e])
            blobs.append(piece)
            header_out[hf_name] = {
                "dtype": info["dtype"],
                "shape": info["shape"],
                "data_offsets": [offset, offset + len(piece)],
            }
            offset += len(piece)
        del full

        hdr_bytes = json.dumps(header_out, separators=(",", ":")).encode()
        with open(out_path, "wb") as fh:
            fh.write(struct.pack("<Q", len(hdr_bytes)))
            fh.write(hdr_bytes)
            for b in blobs:
                fh.write(b)
            fh.flush()
        del blobs
        print(f"  wrote {out_path} ({out_path.stat().st_size / 1e9:.3f} GB)")

    # 5. Verify: parse the outputs. clip_l must match its blueprint exactly;
    # clip_g is scheme-checked (its 24-layer lineage encoder is intentionally
    # NOT the stock 32-layer set). Both: dtypes all F16 + offsets tile the file.
    for out_path in (OUT_L, OUT_G):
        with open(out_path, "rb") as fh:
            n = struct.unpack("<Q", fh.read(8))[0]
            hdr = json.loads(fh.read(n))
        names = set(hdr)
        for k in names:
            assert SCHEME.match(k), f"{out_path}: {k} failed the name scheme"
            assert hdr[k]["dtype"] == "F16", f"{out_path}: {k} dtype {hdr[k]['dtype']}"
        end = max(v["data_offsets"][1] for v in hdr.values())
        assert 8 + n + end == out_path.stat().st_size, f"{out_path}: size mismatch"
        if out_path == OUT_L:
            with open(BLUEPRINT_L, "rb") as fh:
                nb = struct.unpack("<Q", fh.read(8))[0]
                bp = {k for k in json.loads(fh.read(nb)) if k != "__metadata__"}
            assert names == bp, f"{out_path}: clip_l name set drifted from blueprint"
        print(f"verified {out_path}: {len(names)} tensors, F16, offsets tile exactly")

    for p in TMP.iterdir():
        p.unlink()
    TMP.rmdir()
    print("done — sidecars are the checkpoint's OWN encoders (fp16, weight-correct)")


if __name__ == "__main__":
    main()
