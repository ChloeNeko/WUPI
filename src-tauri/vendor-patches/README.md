# vendor-patches — the tracked source of the vendored crates

`src-tauri/vendor/` is **gitignored** (`.gitignore:20`): the vendored crates
bundle their full C++ trees (`stable-diffusion.cpp`, `llama.cpp`) and are
GB-scale on disk. But WUPI **patches** two of them — and those patches are
load-bearing build configuration, not disposable local state. This directory
is the git-tracked record of every modification, so a fresh clone can
reconstruct the vendor trees exactly:

```bash
bash scripts/vendor-restore.sh          # restores both crates (needs a populated ~/.cargo registry)
```

Each `.patch` is `git apply`-format, generated with
`git diff --no-index <registry copy> <vendor copy>` and **validated to
reverse-apply** against the current vendor tree (`git apply --check -R`), so
patch ↔ disk drift is detectable: if `--check -R` ever fails after hand
editing a vendor file, regenerate the patch (diff again) and commit it.

## diffusion-rs-sys 0.1.20 (SD backend, `--features diffusion-rs`)

1. **`build.rs-shared-libs.patch`** — the dual-static-ggml symbol-dedup fix
   (2026-08-15). wupi.exe also links llama-cpp-sys-2's statically-linked
   ggml; two static ggml copies export identical symbols and MSVC resolves
   sd.cpp's calls into LLAMA's ggml (the first SD model load died in another
   copy's file I/O). The patch builds the SD stack as DLLs on MSVC
   (`SD_BUILD_SHARED_LIBS` + `SD_BUILD_SHARED_GGML_LIB`, linked `dylib=`,
   DLLs copied to `target/<profile>/`) so each ggml resolves its own symbols.
   After ANY shared/static flip, wipe `target/debug/build/diffusion-rs-sys-*`
   (Chloe's build-safety procedure — stale static `.lib`s shadow the import
   libs on the recursive link-search path).
2. **`model_loader.cpp-gguf-arch.patch`** — GGUF architecture detection +
   ComfyUI orig-shape recovery (§11.59 Bug B). A ComfyUI-convention Q8 GGUF
   UNet stores only `input_blocks.*` tensors; sd.cpp's tensor-name-based
   version detection is circular for that layout. The patch presets
   `version_` from the GGUF's `general.architecture` metadata (="sdxl")
   before the tensor scan, and consumes `comfy.gguf.orig_shape.*` metadata
   for ComfyUI-exported tensors.

## llama-cpp-sys-2 0.1.151 (always linked)

1. **`build.rs-no-msvcrtd.patch`** — the dual-CRT fix (§11.59, 2026-08-16).
   Upstream emits `cargo:rustc-link-lib=dylib=msvcrtd` under
   `cfg!(debug_assertions)`; cargo compiles build scripts with
   debug-assertions ON in dev, so every dev exe linked BOTH CRTs and UCRT
   debug asserts (`0x80000003`) killed SD generation post-render. WUPI
   compiles llama.cpp's C++ in Release config even in dev, so nothing needs
   the debug CRT — the emission block is removed outright. Re-fingerprints
   this crate's build script → one full llama CUDA rebuild on the next
   feature build. Release builds were never affected.

## Rules

- **Never edit a vendored crate without regenerating its patch** — an
  untracked edit is a change only one machine has (the exact failure mode
  this directory exists to prevent).
- **Never re-enable `[profile.dev.build-override] debug-assertions = false`**
  (Cargo.toml tombstone): it breaks tauri-build's ACL manifest (E0063).
  The llama-cpp-sys-2 patch is the CRT fix.
- The restore script pins crate versions — bump them in lockstep with any
  future `Cargo.toml` dependency bump.
