fn main() {
    tauri_build::build();
    // Only the FABLE launcher binary gets the F icon, and only when building for
    // Windows on a Windows host (cc::windows_registry is host-windows-only; the
    // fable.exe target is Windows anyway).
    #[cfg(windows)]
    if std::env::var("TARGET").map(|t| t.contains("windows")).unwrap_or(false) {
        embed_fable_icon();
    }
}

/// Embed the F-icon resource into `fable.exe` ONLY (not `wupi.exe`).
///
/// tauri-build (via tauri-winres) embeds the wupi paw icon at RT_GROUP_ICON id
/// 32512 crate-wide (`resource.lib`, linked into EVERY bin), so both wupi.exe and
/// fable.exe carry it. To make fable.exe show the F instead, we give it a SECOND
/// icon whose RT_GROUP_ICON sits at id 1 — Windows displays the icon with the
/// LOWEST group id, so the F wins on fable.exe while wupi.exe keeps the paw (its
/// sole icon). The resource is linked only into the `fable` binary via
/// `cargo:rustc-link-arg-bin=fable=…`.
///
/// THE COLLISION GOTCHA (load-bearing — this is why the build used to fail with
/// `CVT1100: duplicate resource. type:ICON, name:1`): an ICON resource compiled
/// by rc.exe becomes ONE `RT_GROUP_ICON` (at the id the .rc declares) PLUS one
/// `RT_ICON` *image* entry per size in the .ico — and rc.exe numbers those image
/// entries starting at 1 on EVERY compilation. So both resource.lib's paw
/// (icon.ico, 6 sizes → RT_ICON 1..6) and our F (fable.ico, 7 sizes → RT_ICON
/// 1..7) own `RT_ICON@1`. The GROUP ids (32512 vs 1) DON'T collide, but the
/// image ids do → fatal duplicate. (Note `HeaderSize` in a .res record INCLUDES
/// the 8-byte DataSize/HeaderSize prefix, so a record's data starts at
/// `off + HeaderSize`, not `off + 8 + HeaderSize`.)
///
/// FIX: after rc.exe compiles fable.rc, [`offset_icon_image_ids`] walks the .res
/// and bumps the F's RT_ICON image ids (and the matching nID refs inside the
/// RT_GROUP_ICON directory) by `FABLE_ICON_ID_BASE` (4096) — landing them at
/// 4097..4103, clear of the paw's 1..6. The group stays at id 1 (lowest → the F
/// is still what Windows displays).
///
/// rc.exe is the same compiler tauri-winres already uses for the wupi icon, so
/// it's guaranteed present on an MSVC toolchain; if it's missing we warn + skip
/// (the build never fails here).
///
/// MSVC-only: the GNU target uses windres, not rc.exe (skipped — the shipped
/// build is MSVC).
#[cfg(windows)]
fn embed_fable_icon() {
    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let icons_dir = manifest_dir.join("icons");
    let ico = icons_dir.join("fable.ico");
    let rc_name = "fable.rc";
    let rc = icons_dir.join(rc_name);

    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed={}", rc.display());

    if !ico.is_file() || !rc.is_file() {
        println!(
            "cargo:warning=fable icon: icons/fable.ico or icons/fable.rc missing; skipping"
        );
        return;
    }

    let target = std::env::var("TARGET").unwrap_or_default();
    let rc_tool = cc::windows_registry::find_tool(&target, "rc.exe")
        .or_else(|| cc::windows_registry::find_tool(&target, "rc"));
    let rc_tool = match rc_tool {
        Some(t) => t,
        None => {
            println!(
                "cargo:warning=fable icon: rc.exe not found for {target}; skipping (fable.exe keeps the default icon)"
            );
            return;
        }
    };

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let res = out_dir.join("fable_icon.res");

    // rc.exe /fo<out.res> fable.rc, CWD = icons/ so the "fable.ico" reference in
    // fable.rc resolves. rc_tool.env() is the MSVC SDK env (PATH/INCLUDE) as a
    // &[(OsString, OsString)] slice — .iter().cloned() yields the owned
    // (OsString, OsString) pairs Command::envs expects.
    let ran = std::process::Command::new(rc_tool.path())
        .envs(rc_tool.env().iter().cloned())
        .current_dir(&icons_dir)
        .args([format!("/fo{}", res.display()), rc_name.to_string()])
        .status();
    match ran {
        Ok(s) if s.success() => {}
        Ok(s) => {
            println!("cargo:warning=fable icon: rc.exe failed (exit {s}); skipping");
            return;
        }
        Err(e) => {
            println!("cargo:warning=fable icon: could not run rc.exe ({e}); skipping");
            return;
        }
    }

    // Push the F's RT_ICON *image* ids clear of resource.lib's paw (see fn doc +
    // `offset_icon_image_ids`). Best-effort: a failure here warns + skips rather
    // than failing the build (fable.exe would then fall back to the paw icon).
    if let Err(e) = offset_icon_image_ids(&res, FABLE_ICON_ID_BASE) {
        println!("cargo:warning=fable icon: could not offset image ids ({e:?}); skipping");
        return;
    }

    // Link the compiled resource ONLY into the fable binary.
    println!("cargo:rustc-link-arg-bin=fable={}", res.display());
}

/// The F's RT_GROUP_ICON stays at id 1 (lowest → Windows displays it). Its
/// underlying RT_ICON *image* ids are bumped by this base so they sit above
/// resource.lib's paw-image range (rc.exe assigns those 1..N; ~6 for icon.ico).
/// 4096 is far above any plausible multi-size .ico image count.
#[cfg(windows)]
const FABLE_ICON_ID_BASE: u16 = 0x1000;

/// Rewrite a compiled `.res` in place: bump every `RT_ICON` (type 3) resource
/// NAME by `base`, and bump the matching `nID` refs inside the `RT_GROUP_ICON`
/// (type 14) directory by the same amount — so the group still points at its own
/// (now-renumbered) images. Pure byte surgery over the record stream; no parsing
/// of image data, no re-encode.
///
/// `.res` record layout: `u32 DataSize`, `u32 HeaderSize` (HeaderSize INCLUDES
/// those 8 bytes), then `HeaderSize - 8` header bytes, then `DataSize` data
/// bytes, DWORD-aligned. TYPE/NAME are ordinals (`0xFFFF` + `u16 id`). The
/// `RT_GROUP_ICON` data is a `GRPICONDIR`: 6-byte NEWHEADER (Reserved, Type=1,
/// Count) then `Count` × 14-byte `GRPICONDENTRY`, each with `nID` (the RT_ICON
/// id it references) at byte offset 12.
#[cfg(windows)]
fn offset_icon_image_ids(res: &std::path::Path, base: u16) -> std::io::Result<()> {
    let mut buf = std::fs::read(res)?;
    let len = buf.len();
    let mut off = 0usize;
    while off + 8 <= len {
        let dsize = u32_le(&buf, off) as usize;
        let hsize = u32_le(&buf, off + 4) as usize;
        if hsize < 8 {
            break; // malformed record or end-of-file padding
        }
        let data_off = off + hsize; // hsize already includes the 8-byte prefix
        if data_off > len {
            break;
        }
        // TYPE ordinal at off+8, NAME ordinal at off+12 (both 0xFFFF + u16 id).
        if off + 16 <= len && u16_le(&buf, off + 8) == 0xFFFF && u16_le(&buf, off + 12) == 0xFFFF {
            let type_id = u16_le(&buf, off + 10);
            match type_id {
                // RT_ICON: offset the image's resource NAME.
                3 => {
                    let nf = off + 14;
                    // Read-then-write: a single `set_u16_le(&mut buf, nf, .. + u16_le(&buf, nf))`
                    // would borrow `buf` mutably + immutably at once. `buf` is an owned
                    // Vec here, so `&mut buf`/`&buf` are fresh borrows (not reborrows) →
                    // NOT two-phase-eligible → E0502. Splitting the statements ends the
                    // shared read before the mutable write.
                    let new_id = base.wrapping_add(u16_le(&buf, nf));
                    set_u16_le(&mut buf, nf, new_id);
                }
                // RT_GROUP_ICON: offset each directory entry's nID ref.
                14 if data_off + 6 <= len => {
                    let count = u16_le(&buf, data_off + 4) as usize; // GRPICONDIR.idCount
                    for i in 0..count {
                        let nid = data_off + 6 + 14 * i + 12; // GRPICONDENTRY.nID
                        if nid + 2 > len {
                            break;
                        }
                        let new_id = base.wrapping_add(u16_le(&buf, nid));
                        set_u16_le(&mut buf, nid, new_id);
                    }
                }
                _ => {}
            }
        }
        off = (data_off + dsize + 3) & !3;
    }
    std::fs::write(res, buf)?;
    Ok(())
}

#[cfg(windows)]
fn u32_le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[cfg(windows)]
fn u16_le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

#[cfg(windows)]
fn set_u16_le(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
