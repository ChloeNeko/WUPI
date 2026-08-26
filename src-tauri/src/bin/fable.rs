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
// DIRECT LAUNCH: optional CLI args `--card <slug> [--session <id>] [--save
// <save_id>]` boot straight into a specific card+session/save, skipping the
// title entirely. Parsed here (std::env::args) by the SHARED
// `wupi_lib::parse_fable_cli` (also the single-instance forwarder's parser,
// 2026-08-20 — one rule set, no drift) + stashed via set_launch_context()
// before run(); setup() appends `?direct=1` to the URL + the frontend drives
// the rest. `--save` REQUIRES `--session` (v0.30.0: a bare save_id with no
// session context is refused by enter_fable_session rather than guessed —
// only ancient .lnk payloads ever carried one; the shipped .lnk bakes
// `--card` alone). `--save` defaults to None = Continue (the
// most-recently-played session's live state). This is also the arg shape a
// generated desktop shortcut (shortcut.rs) bakes into the .lnk.
//
// Single-instance: both exes share the identifier `com.wupi.desktop`, so they
// are mutually exclusive (can't run wupi.exe + fable.exe at once). Launching
// one while the other runs focuses the existing window — safe (prevents a
// duplicate E4B VRAM load + SQLite write collisions).
//
// In release builds, run on the Windows GUI subsystem so no console window is
// allocated. In debug builds we keep the console (the default) so log output is
// still visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Same Windows boot preflight as wupi.exe (DLL hook + WebView2 cache bust).
    // fable.exe loads the same CUDA DLLs + shares the WebView2 identifier.
    wupi_lib::windows_preflight();
    // Flag this process as the fable launcher BEFORE run() so setup() picks the
    // `wupi.html#fable` window URL.
    wupi_lib::set_fable_entry();
    // DIRECT LAUNCH: parse `--card`/`--session`/`--save` + stash for
    // setup()/get_launch_context. The parser lives in the lib
    // (wupi_lib::parse_fable_cli) so the single-instance forwarder reads
    // argv with the SAME rules.
    if let Some(cli) = wupi_lib::parse_fable_cli(std::env::args().skip(1)) {
        wupi_lib::set_launch_context(cli.card, cli.session, cli.save);
    }
    wupi_lib::run();
}
