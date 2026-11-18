//! The CK Dev desktop app: a Tauri shell around the React UI.

use ck_core::branding::DISPLAY_NAME;
use serde::Serialize;
use tauri::Manager;

/// What the UI shows about the running app.
#[derive(Debug, Serialize)]
pub struct AppInfo {
    name: &'static str,
    version: &'static str,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: DISPLAY_NAME,
        version: env!("CARGO_PKG_VERSION"),
    }
}

/// Starts the desktop app and blocks until it exits.
pub fn run() -> tauri::Result<()> {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_info])
        .setup(|app| {
            let window = app
                .get_webview_window("main")
                .ok_or("window `main` is missing from tauri.conf.json")?;
            window.set_title(DISPLAY_NAME)?;
            Ok(())
        })
        .run(tauri::generate_context!())
}
