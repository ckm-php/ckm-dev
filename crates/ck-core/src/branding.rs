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

/// Folder name used inside each OS's normal app-data location.
pub const DATA_DIR_NAME: &str = "ck-dev";

/// Default HTTP port for the web server.
pub const WEB_PORT: u16 = 8900;

/// Default HTTPS port for the web server.
pub const HTTPS_PORT: u16 = 8443;

/// Default MySQL port, kept off 3306 so it never collides with a system server.
pub const MYSQL_PORT: u16 = 13306;
