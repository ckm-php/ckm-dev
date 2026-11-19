//! The supervisor's log: one plain UTC line per event on stderr, such as
//! `2026-11-19T01:02:03Z info started`.
//!
//! `client::ensure` points the supervisor's stderr at `logs/supervisor.log`
//! (rotated when it is opened), so this module only writes lines.

use std::fmt::Display;
use std::io::{self, Write};

use time::OffsetDateTime;

/// Longest message kept, in characters. Messages can carry names a client
/// sent, so one request cannot fill the log.
const MAX_MESSAGE_CHARS: usize = 2000;

pub(super) fn info(message: impl Display) {
    write("info", &message);
}

pub(super) fn warn(message: impl Display) {
    write("warn", &message);
}

pub(super) fn error(message: impl Display) {
    write("error", &message);
}

/// Best effort: a log that cannot be written must not stop the supervisor,
/// so errors are ignored (`eprintln!` would panic).
fn write(level: &str, message: &dyn Display) {
    let line = line(OffsetDateTime::now_utc(), level, &message.to_string());
    let _ = io::stderr().lock().write_all(line.as_bytes());
}

/// `<UTC time> <level> <message>\n`. Control characters in the message become
/// spaces, so every entry stays on one line.
fn line(at: OffsetDateTime, level: &str, message: &str) -> String {
    let mut line = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z {level} ",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    );
    line.extend(
        message
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .map(|c| if c.is_control() { ' ' } else { c }),
    );
    line.push('\n');
    line
}
