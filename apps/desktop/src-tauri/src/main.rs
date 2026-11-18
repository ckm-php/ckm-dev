// Release builds on Windows get no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> tauri::Result<()> {
    ck_desktop_lib::run()
}
