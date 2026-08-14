// In release builds, run on the Windows GUI subsystem so no console window is
// allocated. In debug builds we keep the console (the default) so log output is
// still visible during development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Windows boot preflight (DLL search-dir hook + WebView2 cache bust).
    // MUST run before wupi_lib::run() initializes anything that touches a
    // CUDA/VC++ DLL or starts WebView2 (which locks its cache files). Shared
    // with fable.exe via wupi_lib::windows_preflight (see boot_preflight.rs).
    wupi_lib::windows_preflight();
    wupi_lib::run();
}
