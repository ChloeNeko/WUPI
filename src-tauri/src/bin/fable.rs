// fable.exe — the Fable-only launcher binary (a sibling of wupi.exe).
//
// (Name note, 2026-08-14: Discord's game detection matches this exe name to
// the 2005 game "Fable: The Lost Chapters" (that game's exe is literally
// Fable.exe). Chloe's call: KEEP the name — do not rename to dodge Discord.)
//
// This is the SECOND launcher binary of the same `wupi` crate. It runs the
// IDENTICAL boot code as wupi.exe (wupi_lib::run) — same model load, same
// memory DB, same tray, same IPCs — with ONE difference: set_fable_entry()
// flips a flag that makes setup() build the main window with the URL
// `wupi.html#fable` instead of `wupi.html`. The frontend detects `#fable`
// (script.js + fable.js) and skips the OS boot ceremony + Fable's fog-gate/
// ripple, so double-clicking fable.exe lands straight on the Fable title
// screen with no bootup / loading screen / ripple.
//
// DIRECT LAUNCH: optional CLI args `--card <slug> [--save <save_id>]` boot
// straight into a specific card+save, skipping the title entirely. Parsed
// here (std::env::args) + stashed via set_launch_context() before run();
// setup() appends `?direct=1` to the URL + the frontend drives the rest.
// `--save` defaults to None = Continue (live session.json). This is also the
// arg shape a generated desktop shortcut (shortcut.rs) bakes into the .lnk.
//
// Single-instance: both exes share the identifier `com.wupi.desktop`, so they
// are mutually exclusive (can't run wupi.exe + fable.exe at once). Launching
// one while the other runs focuses the existing window — safe (prevents a
// duplicate 12B VRAM load + SQLite write collisions).
//
// In release builds, run on the Windows GUI subsystem so no console window is
// allocated. In debug builds we keep the console (the default) so log output is
// still visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Parsed fable.exe CLI target. `--card` is required for a direct launch;
/// `--save` is optional (None → Continue).
struct FableCli {
    card: String,
    save: Option<String>,
}

/// Walk argv looking for `--card <slug>` and an optional `--save <save_id>`.
/// Unknown flags are ignored (forward-compat). Returns `None` when `--card` is
/// absent (a plain fable.exe launch → title screen, as before). An empty /
/// separator-containing value is rejected (a card slug is a bare token), in
/// which case the offending flag is treated as absent so the launch still
/// succeeds — just without that override — rather than stranding the window.
fn parse_fable_cli(args: impl Iterator<Item = String>) -> Option<FableCli> {
    let mut tokens = args.peekable();
    let mut card: Option<String> = None;
    let mut save: Option<String> = None;
    while let Some(flag) = tokens.next() {
        let take_value = |raw: String, next: Option<String>, slot: &mut Option<String>| {
            // A bare flag with no following token, or a value that looks like a
            // path separator / another flag, is rejected — the slot stays None.
            if let Some(v) = next {
                if !v.starts_with("--") && !v.contains('/') && !v.contains('\\') && !v.is_empty() {
                    *slot = Some(v);
                } else {
                    tracing::warn!(flag = %raw, "fable.exe: {} value missing or invalid — ignored", raw);
                }
            }
        };
        match flag.as_str() {
            "--card" => take_value(flag, tokens.next(), &mut card),
            "--save" => take_value(flag, tokens.next(), &mut save),
            _ => { /* ignore unknown flags (forward-compat) */ }
        }
    }
    card.map(|c| FableCli { card: c, save })
}

fn main() {
    // Same Windows boot preflight as wupi.exe (DLL hook + WebView2 cache bust).
    // fable.exe loads the same CUDA DLLs + shares the WebView2 identifier.
    wupi_lib::windows_preflight();
    // Flag this process as the fable launcher BEFORE run() so setup() picks the
    // `wupi.html#fable` window URL.
    wupi_lib::set_fable_entry();
    // DIRECT LAUNCH: parse `--card`/`--save` + stash for setup()/get_launch_context.
    if let Some(cli) = parse_fable_cli(std::env::args().skip(1)) {
        wupi_lib::set_launch_context(cli.card, cli.save);
    }
    wupi_lib::run();
}
