//! Processes the tests learn of from `ck`, so that assertions can check them
//! and cleanup can end them by pid, but only after checking that the pid is
//! still the same process.

#[cfg(unix)]
pub use self::unix::Proc;
#[cfg(windows)]
pub use self::windows::Proc;

#[cfg(unix)]
mod unix {
    use ck_core::service::identity::{self, ProcessIdentity};
    use ck_core::service::record::ServiceRecord;
    use ck_sys::unix::{Pid, Signal, signal_process};

    use crate::support::Program;

    /// A process known by its start time and executable (and boot id on
    /// Linux), the identity `ck` itself checks before it signals.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Proc {
        pid: Pid,
        identity: ProcessIdentity,
    }

    impl Proc {
        /// The live process `pid`. The program is not compared: on macOS a
        /// hard-linked executable, which cargo makes, can report another of
        /// its names.
        pub fn observe(pid: u32, _program: Program) -> Option<Self> {
            let pid = Pid::new(pid).ok()?;
            let identity = identity::read(pid).ok()??;
            Some(Self { pid, identity })
        }

        /// The service process a record describes, by the identity recorded
        /// at spawn.
        pub fn from_record(record: &ServiceRecord) -> Option<Self> {
            Some(Self {
                pid: Pid::new(record.pid).ok()?,
                identity: record.identity.clone()?,
            })
        }

        pub fn pid(&self) -> u32 {
            self.pid.get()
        }

        pub fn is_alive(&self) -> bool {
            identity::verify(self.pid, &self.identity).is_ok()
        }

        /// `SIGKILL`, only while the pid still shows the same identity.
        pub fn kill(&self) {
            if self.is_alive() {
                let _ = signal_process(self.pid, Signal::Kill);
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use crate::support::Program;

    /// A process known only by its pid and image name: without FFI, which the
    /// tests may not use, Windows offers no start time. Windows reuses pids
    /// quickly, so [`Proc::kill`] is only for a pid a supervisor reported a
    /// moment ago.
    #[derive(Debug, Clone)]
    pub struct Proc {
        pid: u32,
        image: String,
    }

    impl Proc {
        /// The live process `pid`, when it runs `program`.
        pub fn observe(pid: u32, program: Program) -> Option<Self> {
            let image = program.path().file_name()?.to_str()?.to_owned();
            let process = Self { pid, image };
            process.is_alive().then_some(process)
        }

        pub fn pid(&self) -> u32 {
            self.pid
        }

        pub fn is_alive(&self) -> bool {
            image_of(self.pid).is_some_and(|image| image.eq_ignore_ascii_case(&self.image))
        }

        /// `taskkill /F`, only while the pid still runs the same image.
        pub fn kill(&self) {
            if self.is_alive() {
                let _ = Command::new(system_tool("taskkill.exe"))
                    .args(["/PID", &self.pid.to_string(), "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    /// The image name of process `pid` from `tasklist`; `None` when no such
    /// process runs.
    fn image_of(pid: u32) -> Option<String> {
        let output = Command::new(system_tool("tasklist.exe"))
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        // A match is one CSV row, `"image","pid",…`. No match is a notice in
        // the system's language, which never starts with a quote.
        let text = String::from_utf8_lossy(&output.stdout);
        let row = text.lines().find(|line| line.starts_with('"'))?;
        let mut fields = row.split(',').map(|field| field.trim_matches('"'));
        let image = fields.next()?;
        let row_pid: u32 = fields.next()?.parse().ok()?;
        (row_pid == pid).then(|| image.to_owned())
    }

    /// `%SystemRoot%\System32\<tool>`, never a `PATH` lookup.
    fn system_tool(tool: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        root.join("System32").join(tool)
    }
}
