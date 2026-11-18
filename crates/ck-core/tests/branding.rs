use std::fs;
use std::path::Path;

use ck_core::branding::{APP_ID, DISPLAY_NAME};

#[test]
fn tauri_config_matches_branding() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("apps")
        .join("desktop")
        .join("src-tauri")
        .join("tauri.conf.json");
    let text = fs::read_to_string(&path).expect("read tauri.conf.json");
    let config: serde_json::Value = serde_json::from_str(&text).expect("tauri.conf.json is JSON");

    assert_eq!(config["productName"], DISPLAY_NAME);
    assert_eq!(config["identifier"], APP_ID);
}
