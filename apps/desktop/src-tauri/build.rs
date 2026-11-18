use tauri_build::{AppManifest, Attributes};

fn main() {
    // Declaring the app's commands makes Tauri generate an `allow-…` permission
    // for each, so a capability must grant them explicitly.
    let manifest = AppManifest::new().commands(&["app_info"]);
    tauri_build::try_build(Attributes::new().app_manifest(manifest)).expect("tauri-build failed");
}
