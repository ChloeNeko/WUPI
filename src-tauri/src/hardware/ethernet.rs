//! Wired (Ethernet) link detection via the NDIS interface table.
//!
//! `ethernet_get_state` reports whether any physical Ethernet adapter is up and
//! its media (cable) is connected. The status bar uses this to swap the Wi-Fi
//! indicator for a wired one when the machine is on Ethernet (AGENTS §0: Fable
//! + PRISM + Wupi shell; the wifi module is WLAN-only and errors out on a
//! cabled desktop with no Wi-Fi card).
//!
//! NDIS is plain Win32: `GetIfTable2` allocates a table the caller frees with
//! `FreeMibTable`. The call is sub-millisecond, so it runs inline in the
//! command — no worker thread, unlike the blocking WLAN FFI in `wifi.rs`.
//!
//! Uses the `Win32_NetworkManagement_Ndis` cargo feature (already enabled in
//! Cargo.toml; previously unused).

use serde::{Deserialize, Serialize};
// The IP Helper API (GetIfTable2 family) enumerates adapters; the per-row
// oper-status / media-state enums live in Ndis. Both Win32 features are
// enabled in Cargo.toml. ifType 6 = Ethernet CSMA/CD (IANA ifType); the
// crate doesn't expose it as a named constant in 0.58.
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::NetworkManagement::Ndis::{IfOperStatusUp, MediaConnectStateConnected};

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;

#[derive(Serialize, Deserialize)]
pub struct EthernetState {
    pub connected: bool,
    /// Friendly adapter name for display (e.g. "Ethernet"), if any.
    pub name: Option<String>,
}

/// Whether any physical Ethernet adapter is up, cabled, and connected.
///
/// A wired link takes precedence over Wi-Fi in the status bar: when this
/// returns `connected: true`, the frontend renders the wired indicator instead
/// of the Wi-Fi panel.
#[tauri::command]
pub fn ethernet_get_state() -> Result<EthernetState, String> {
    let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: GetIfTable2 heap-allocates the table into table_ptr; we free it
    // with FreeMibTable on every exit path below.
    let err = unsafe { GetIfTable2(&mut table_ptr) };
    if err != WIN32_ERROR(0) {
        return Err(format!("GetIfTable2 failed: error {:?}", err.0));
    }
    let state = unsafe {
        let table = &*table_ptr;
        // The Table field is a flexible-array member ([MIB_IF_ROW2; 1]); read
        // NumEntries contiguous rows from its base pointer.
        let rows = std::slice::from_raw_parts(table.Table.as_ptr(), table.NumEntries as usize);
        // First physical Ethernet adapter that is up + media connected. The
        // name guard excludes Hyper-V/Docker/VM "virtual" adapters, which are
        // also IF_TYPE_ETHERNET_CSMACD and can read as Up+Connected — so a real
        // cabled NIC is what surfaces.
        let hit = rows.iter().find(|r| {
            r.Type == IF_TYPE_ETHERNET_CSMACD
                && r.OperStatus == IfOperStatusUp
                && r.MediaConnectState == MediaConnectStateConnected
                && !looks_virtual(&r.Alias, &r.Description)
        });
        hit.map(|r| EthernetState {
            connected: true,
            name: wide_to_string(&r.Alias).or_else(|| wide_to_string(&r.Description)),
        })
        .unwrap_or(EthernetState {
            connected: false,
            name: None,
        })
    };
    unsafe { FreeMibTable(table_ptr as *mut _ as *const std::ffi::c_void) };
    Ok(state)
}

/// Heuristic guard: Hyper-V/Docker/VMware/VirtualBox virtual Ethernet adapters
/// share IF_TYPE_ETHERNET_CSMACD and can be Up+Connected, so exclude them by
/// name to detect a genuine cabled NIC. Case-insensitive substring match.
fn looks_virtual(alias: &[u16], desc: &[u16]) -> bool {
    let blob = format!(
        "{} {}",
        wide_to_string(alias).unwrap_or_default(),
        wide_to_string(desc).unwrap_or_default()
    )
    .to_ascii_uppercase();
    blob.contains("VIRTUAL")
        || blob.contains("VETHERNET")
        || blob.contains("HYPER-V")
        || blob.contains("VMWARE")
        || blob.contains("VIRTUALBOX")
}

/// Decode a NUL-terminated UTF-16 wide-char buffer into a String. Returns None
/// for empty buffers or invalid UTF-16 (shouldn't happen for adapter names).
fn wide_to_string(buf: &[u16]) -> Option<String> {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    String::from_utf16(&buf[..len]).ok()
}
