//! Build-time product identity (D15). Changing a value here needs a rebuild.
//!
//! `apps/desktop/src-tauri/tauri.conf.json` repeats `DISPLAY_NAME` and `APP_ID`,
//! because Tauri reads them from its config file; a test fails if they drift.

/// Name shown to the user: window title, installer and macOS app name.
pub const DISPLAY_NAME: &str = "CK Dev";

/// Name of the command-line binary.
pub const COMMAND: &str = "ck";

/// Reverse-DNS application identifier.
pub const APP_ID: &str = "dev.ck.desktop";

/// Data folder name inside the OS's local app-data folder (D15). It also names
/// the IPC endpoint.
pub const DATA_DIR_NAME: &str = "ck-dev";

/// Data folder name for a debug build that runs without a test profile (D21),
/// so development builds never touch a release install's data.
pub const DEBUG_DATA_DIR_NAME: &str = "ck-dev-debug";
