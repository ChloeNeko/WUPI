//! Windows `.lnk` shortcut generation for `fable.exe` direct card launches.
//!
//! Self-contained Win32 COM (`IShellLinkW` + `IPersistFile`) + a tiny PNG→ICO
//! wrapper. Windows Vista+ reads PNG-compressed icon entries directly, so a
//! card's portrait PNG can become a shortcut icon without decode/re-encode.
//! This module is a leaf utility: `lib.rs` owns all path resolution (card
//! folders, portrait detection, install root) and calls [`write_lnk`] with
//! concrete absolute paths; the COM + ICO mechanics live here.

use std::path::{Path, PathBuf};
use windows::core::{HSTRING, Interface};
use windows::Win32::Foundation::{BOOL, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, IPersistFile,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

/// Build an `.ico` from a PNG by wrapping the raw PNG bytes in an ICONDIR +
/// a single ICONDIRENTRY. Vista+ shell reads PNG-compressed icon entries
/// natively, so no decode/re-encode is needed — the portrait ships verbatim.
/// Returns `None` if `png` is not a PNG or its IHDR dimensions are unreadable.
/// Width/height ≥256 are stored as `0` (the ICO "256" sentinel).
pub fn png_to_ico(png: &[u8]) -> Option<Vec<u8>> {
    const PNG_SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if png.len() < 24 || &png[..8] != PNG_SIG {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    if w == 0 || h == 0 {
        return None;
    }
    let bwidth = if w >= 256 { 0u8 } else { w as u8 };
    let bheight = if h >= 256 { 0u8 } else { h as u8 };
    let mut out = Vec::with_capacity(22 + png.len());
    // ICONDIR: idReserved(0), idType=1 (icon), idCount=1
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    // ICONDIRENTRY (16 bytes)
    out.push(bwidth); // bWidth (0 = 256)
    out.push(bheight); // bHeight (0 = 256)
    out.push(0); // bColorCount (0 = ≥256 colors)
    out.push(0); // bReserved
    out.extend_from_slice(&1u16.to_le_bytes()); // wPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // wBitCount
    out.extend_from_slice(&(png.len() as u32).to_le_bytes()); // dwBytesInRes
    out.extend_from_slice(&22u32.to_le_bytes()); // dwImageOffset = 6 + 16
    out.extend_from_slice(png);
    Some(out)
}

/// Resolve the user's Desktop directory via `%USERPROFILE%\Desktop` (the
/// conventional single-user location). Returns `None` if the env var is unset
/// or the path isn't a directory. A relocated Desktop is a known v1
/// limitation — acceptable for the common case.
pub fn desktop_dir() -> Option<PathBuf> {
    let profile = std::env::var("USERPROFILE").ok()?;
    let p = PathBuf::from(profile).join("Desktop");
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// Create a Windows shortcut at `out_path` (an `.lnk`) pointing to `target`
/// with `args`, an optional `icon`, and a working directory. When `icon` is
/// `None` the target's own embedded icon is used (e.g. `fable.exe`'s F icon —
/// the fallback for a card with no PNG portrait).
///
/// The COM work runs on a fresh `std::thread` (MTA init) so it never clashes
/// with the async runtime's apartment state — mirrors
/// `hardware/audio.rs::with_com`. All paths are absolute + sanitized by the
/// caller (`lib.rs` owns path resolution).
pub fn write_lnk(
    target: &Path,
    args: &str,
    icon: Option<&Path>,
    work_dir: &Path,
    out_path: &Path,
) -> Result<(), String> {
    let target = target.to_string_lossy().into_owned();
    let args = args.to_string();
    let icon = icon.map(|p| p.to_string_lossy().into_owned());
    let work_dir = work_dir.to_string_lossy().into_owned();
    let out_path = out_path.to_string_lossy().into_owned();

    let result = std::thread::spawn(move || -> windows::core::Result<()> {
        // CoInitializeEx on a clean thread: COM apartments don't mix with the
        // async runtime. RPC_E_CHANGED_MODE (already-inited in a different
        // apartment) is tolerated — rare for a fresh thread, but still run.
        let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let init_ok = init.is_ok();
        let run = if init_ok || init == RPC_E_CHANGED_MODE {
            build_lnk(&target, &args, icon.as_deref(), &work_dir, &out_path)
        } else {
            Err(windows::core::Error::from(init))
        };
        if init_ok {
            unsafe { CoUninitialize() };
        }
        run
    })
    .join()
    .map_err(|e| format!("shortcut COM thread panicked: {e:?}"))?;

    result.map_err(|e| format!("shortcut create failed: HRESULT 0x{:08X}", e.code().0 as u32))
}

/// The actual `IShellLinkW` → `IPersistFile::Save` sequence. Safe fn wrapping
/// the `unsafe` COM calls (avoids `unsafe_op_in_unsafe_fn` lint surface).
#[allow(clippy::missing_safety_doc)]
fn build_lnk(
    target: &str,
    args: &str,
    icon: Option<&str>,
    work_dir: &str,
    out_path: &str,
) -> windows::core::Result<()> {
    unsafe {
        let shelllink: IShellLinkW = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_ALL)?;
        let target_h = HSTRING::from(target);
        shelllink.SetPath(&target_h)?;
        let args_h = HSTRING::from(args);
        shelllink.SetArguments(&args_h)?;
        // When an icon path is given it's a per-card portrait.ico (index 0).
        // When absent, the target fable.exe's embedded F icon is used.
        if let Some(icon) = icon {
            let icon_h = HSTRING::from(icon);
            shelllink.SetIconLocation(&icon_h, 0)?;
        }
        let work_h = HSTRING::from(work_dir);
        shelllink.SetWorkingDirectory(&work_h)?;
        let persist: IPersistFile = shelllink.cast::<IPersistFile>()?;
        let out_h = HSTRING::from(out_path);
        // fremember = FALSE: a one-shot save, don't add to the MRU / remember
        // it as the object's persistent state.
        persist.Save(&out_h, BOOL(0))?;
        Ok(())
    }
}
