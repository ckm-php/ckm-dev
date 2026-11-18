//! Windows security, named-pipe and Job Object calls.

use std::ffi::{OsStr, c_void};
use std::io;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use tokio::process::Child;
use windows_sys::Win32::Foundation::{
    ERROR_SUCCESS, FALSE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree,
    SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::core::PWSTR;

/// Process creation flags for `Command::creation_flags`, so that `ck-core`
/// needs no `windows-sys` of its own. The detached supervisor uses all three
/// (retrying without breakaway when the caller's Job forbids it).
pub use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
};

/// Every instance of a pipe is in use; a client retries. A `u32`, while
/// `io::Error::raw_os_error` gives an `i32`.
pub use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

/// The current user's SID in string form (`S-1-5-21-…`), from the process token.
///
/// Safety contract: opens this process's token with `TOKEN_QUERY`, sizes and
/// reads `TokenUser` into a buffer owned by this function, converts the SID
/// with `ConvertSidToStringSidW`, and frees that string with `LocalFree`. The
/// token handle is owned and closed on every path.
pub fn current_user_sid() -> io::Result<String> {
    ProcessUser::current()?.sid_string()
}

/// Creates an instance of the pipe `name` (`\\.\pipe\…`) that only the current
/// user can open.
///
/// - The DACL is `D:P(A;;GA;;;<current SID>)`: protected, with one allow entry.
/// - Remote clients are rejected, and the handle is not inheritable.
/// - `first_instance` makes creation fail when the name already exists, so
///   another process cannot have squatted on it.
///
/// It must run inside a tokio runtime with I/O enabled. Outside one, tokio
/// 1.54 panics: its docs say it returns an error, but registering the pipe
/// calls `Handle::current()`, which panics.
///
/// Safety contract: builds the security descriptor with
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`, keeps it alive for
/// tokio's `create_with_security_attributes_raw` call (which requires a valid
/// `SECURITY_ATTRIBUTES` pointer), then frees it with `LocalFree`.
#[allow(
    unsafe_code,
    reason = "tokio takes a pipe's SECURITY_ATTRIBUTES only through an unsafe fn"
)]
pub fn private_pipe_server(name: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
    let sid = current_user_sid()?;
    let descriptor = security_descriptor(&format!("D:P(A;;GA;;;{sid})"))?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_u32::<SECURITY_ATTRIBUTES>()?,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: FALSE,
    };
    let mut options = ServerOptions::new();
    options
        .reject_remote_clients(true)
        .first_pipe_instance(first_instance);
    let attributes_ptr = (&raw mut attributes).cast::<c_void>();
    // SAFETY: `attributes_ptr` points at a valid SECURITY_ATTRIBUTES whose
    // descriptor is owned by `descriptor`, which is dropped only after the call.
    let server = unsafe { options.create_with_security_attributes_raw(name, attributes_ptr) };
    drop(descriptor);
    server
}

/// Whether the client connected to `pipe` runs as the current user (token SID).
///
/// Safety contract: reads the client's process id with
/// `GetNamedPipeClientProcessId`, opens that process with
/// `PROCESS_QUERY_LIMITED_INFORMATION` and its token with `TOKEN_QUERY`,
/// compares the `TokenUser` SID with `EqualSid`, and closes both owned handles
/// on every path. The pipe handle is only borrowed.
#[allow(
    unsafe_code,
    reason = "GetNamedPipeClientProcessId has no safe wrapper"
)]
pub fn pipe_client_is_current_user(pipe: &NamedPipeServer) -> io::Result<bool> {
    let mut pid = 0_u32;
    // SAFETY: the pipe handle is borrowed from `pipe`, which outlives the call,
    // and `pid` is a local out-parameter.
    let found = unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &raw mut pid) };
    if found == FALSE {
        return Err(io::Error::last_os_error());
    }
    runs_as_current_user(pid)
}

/// Whether the server end of `pipe` runs as the current user (token SID).
///
/// Safety contract: as [`pipe_client_is_current_user`], starting from
/// `GetNamedPipeServerProcessId`.
#[allow(
    unsafe_code,
    reason = "GetNamedPipeServerProcessId has no safe wrapper"
)]
pub fn pipe_server_is_current_user(pipe: &NamedPipeClient) -> io::Result<bool> {
    let mut pid = 0_u32;
    // SAFETY: the pipe handle is borrowed from `pipe`, which outlives the call,
    // and `pid` is a local out-parameter.
    let found = unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &raw mut pid) };
    if found == FALSE {
        return Err(io::Error::last_os_error());
    }
    runs_as_current_user(pid)
}

/// Compares the token user of process `pid` with this process's.
///
/// Opening the peer process works the same from both ends (a client cannot
/// impersonate its server) and never changes the calling thread's token, which
/// on a tokio thread would leak into other tasks. A peer that cannot be opened
/// is an error, which callers treat as a refusal.
fn runs_as_current_user(pid: u32) -> io::Result<bool> {
    let peer = ProcessUser::of_pid(pid)?;
    Ok(ProcessUser::current()?.same_user(&peer))
}

/// Makes the folder `path` private, creating it first when it is missing.
///
/// It sets a protected DACL (no entries inherited from the parent) that gives
/// full control to the current user, SYSTEM and Administrators only, and is
/// inherited by every file and folder inside.
///
/// Safety contract: builds the descriptor from SDDL with
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`, applies its DACL
/// with `SetNamedSecurityInfoW` on a NUL-terminated wide copy of `path` owned
/// by this function, and frees the descriptor with `LocalFree`.
#[allow(
    unsafe_code,
    reason = "folder DACLs are set through Win32 calls with no safe wrapper"
)]
pub fn make_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let sid = current_user_sid()?;
    let descriptor = security_descriptor(&format!(
        "D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))?;

    let mut present = FALSE;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut defaulted = FALSE;
    // SAFETY: the descriptor is owned by `descriptor` and alive; the three
    // out-parameters are locals.
    let found = unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    };
    if found == FALSE {
        return Err(io::Error::last_os_error());
    }
    // A missing or NULL DACL would grant everyone access.
    if present == FALSE || dacl.is_null() {
        return Err(io::Error::other(
            "the private folder's security descriptor has no DACL",
        ));
    }

    let wide_path = wide_nul(path.as_os_str())?;
    // SAFETY: `wide_path` is NUL-terminated and `dacl` points into
    // `descriptor`; both outlive the call. Owner, group and SACL are null, so
    // they are left unchanged.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    Ok(())
}

/// A Job Object that ends every process in it when its last handle closes.
///
/// Dropping the `Job` closes the handle, so the services in it die with the
/// supervisor. A service never runs outside a Job.
#[derive(Debug)]
pub struct Job {
    handle: OwnedHandle,
}

impl Job {
    /// Creates an anonymous Job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    ///
    /// Safety contract: `CreateJobObjectW` with null attributes and name; the
    /// returned handle is checked and moved into an `OwnedHandle` at once. The
    /// limit struct passed to `SetInformationJobObject` is owned by this
    /// function and passed with its exact size.
    #[allow(unsafe_code, reason = "Job Objects have no safe wrapper in std")]
    pub fn kill_on_close() -> io::Result<Self> {
        // SAFETY: null attributes and a null name are allowed; a null result is
        // checked below.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a new Job handle that nothing else owns.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };

        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        let length = size_u32::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()?;
        let limits_ptr = (&raw const limits).cast::<c_void>();
        // SAFETY: the Job handle is owned by `handle`; `limits_ptr` points at
        // `limits`, which this function owns and which is `length` bytes long.
        let set = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                limits_ptr,
                length,
            )
        };
        if set == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// Puts the running `child` into this Job.
    ///
    /// On failure the caller must terminate the child: a service never runs
    /// outside a Job. Fails when the child has already exited.
    ///
    /// Safety contract: borrows the child's process handle from tokio for the
    /// duration of `AssignProcessToJobObject` only; both handles stay owned by
    /// their owners.
    #[allow(unsafe_code, reason = "Job Objects have no safe wrapper in std")]
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        let Some(process) = child.raw_handle() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the child has already exited",
            ));
        };
        // SAFETY: the Job handle is owned by `self` and the process handle by
        // `child`; both are open for the duration of the call.
        let assigned = unsafe { AssignProcessToJobObject(self.handle.as_raw_handle(), process) };
        if assigned == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Ends every process in the Job with `exit_code`.
    ///
    /// Safety contract: `TerminateJobObject` on the owned handle; no memory is
    /// passed.
    #[allow(unsafe_code, reason = "Job Objects have no safe wrapper in std")]
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the Job handle is owned by `self` and open; no memory is passed.
        let terminated = unsafe { TerminateJobObject(self.handle.as_raw_handle(), exit_code) };
        if terminated == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Marks this process's standard input, output and error handles as not
/// inheritable.
///
/// Call it before spawning the detached supervisor, so that the supervisor does
/// not hold a caller's pipe open (`$x = ck start …` would otherwise hang).
///
/// Safety contract: `GetStdHandle` for each of the three handles, skipping null
/// and invalid ones, then `SetHandleInformation` to clear
/// `HANDLE_FLAG_INHERIT`. The handles are borrowed, never closed.
#[allow(unsafe_code, reason = "std cannot change a handle's inherit flag")]
pub fn stop_std_handle_inheritance() -> io::Result<()> {
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes a constant and returns a borrowed handle,
        // or null or INVALID_HANDLE_VALUE, which are skipped below.
        let handle = unsafe { GetStdHandle(which) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `handle` is this process's open std handle; only its inherit
        // flag changes, and it is not closed.
        let set = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        if set == FALSE {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Memory that a Win32 call allocated and that `LocalFree` releases.
struct LocalMemory(*mut c_void);

impl Drop for LocalMemory {
    #[allow(unsafe_code, reason = "LocalFree has no safe wrapper")]
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: the pointer came from a Win32 call whose memory is released
        // with `LocalFree`, and this guard is its only owner, so it is freed once.
        unsafe { LocalFree(self.0) };
    }
}

/// Builds a self-relative security descriptor from SDDL.
#[allow(unsafe_code, reason = "SDDL conversion has no safe wrapper")]
fn security_descriptor(sddl: &str) -> io::Result<LocalMemory> {
    let wide = wide_nul(OsStr::new(sddl))?;
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated and outlives the call; `descriptor` is a
    // local out-parameter, and the optional size out-parameter is null.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == FALSE {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalMemory(descriptor))
}

/// The user a process runs as: the `TOKEN_USER` of its token.
struct ProcessUser {
    /// `u64` elements keep the buffer aligned for `TOKEN_USER`, which starts
    /// with a pointer. The SID it points at lies in the same buffer.
    buffer: Vec<u64>,
}

impl ProcessUser {
    /// The user this process runs as.
    #[allow(unsafe_code, reason = "GetCurrentProcess has no safe wrapper")]
    fn current() -> io::Result<Self> {
        // SAFETY: `GetCurrentProcess` takes no arguments and returns a
        // pseudo-handle that needs no closing.
        let process = unsafe { GetCurrentProcess() };
        Self::of_process(process)
    }

    /// The user that process `pid` runs as.
    #[allow(unsafe_code, reason = "OpenProcess has no safe wrapper")]
    fn of_pid(pid: u32) -> io::Result<Self> {
        // SAFETY: `OpenProcess` takes no pointers; a null result is checked below.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a new process handle that nothing else owns.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        Self::of_process(process.as_raw_handle())
    }

    /// Reads `TokenUser` from the token of `process`, which the caller keeps open.
    #[allow(unsafe_code, reason = "token calls have no safe wrapper")]
    fn of_process(process: HANDLE) -> io::Result<Self> {
        let mut raw_token: HANDLE = ptr::null_mut();
        // SAFETY: `process` is open for the duration of the call, and
        // `raw_token` is a local out-parameter.
        let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut raw_token) };
        if opened == FALSE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success `raw_token` is a new token handle that nothing else owns.
        let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };

        let mut needed = 0_u32;
        // SAFETY: a null buffer of length 0 only asks for the size, which is
        // written to the local `needed`.
        let sized = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                ptr::null_mut(),
                0,
                &raw mut needed,
            )
        };
        // The size query fails by design (ERROR_INSUFFICIENT_BUFFER); only a
        // missing size means a real error.
        if sized == FALSE && needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = usize::try_from(needed).map_err(io::Error::other)?;
        if length < mem::size_of::<TOKEN_USER>() {
            return Err(io::Error::other(
                "the token's user information is too short",
            ));
        }

        let mut buffer = vec![0_u64; length.div_ceil(mem::size_of::<u64>())];
        let buffer_ptr = buffer.as_mut_ptr().cast::<c_void>();
        let mut returned = 0_u32;
        // SAFETY: `buffer_ptr` points at `buffer`, which this function owns and
        // which is at least `needed` bytes long; `returned` is a local
        // out-parameter.
        let read = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer_ptr,
                needed,
                &raw mut returned,
            )
        };
        if read == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { buffer })
    }

    /// The user SID. It points into `self.buffer`, so it is valid while `self` lives.
    #[allow(
        unsafe_code,
        reason = "reads the TOKEN_USER that GetTokenInformation wrote"
    )]
    fn sid(&self) -> PSID {
        // SAFETY: `of_process` filled the buffer with a TOKEN_USER and checked
        // that it is large enough; `u64` elements keep it aligned for one.
        let user = unsafe { self.buffer.as_ptr().cast::<TOKEN_USER>().read() };
        user.User.Sid
    }

    #[allow(unsafe_code, reason = "EqualSid has no safe wrapper")]
    fn same_user(&self, other: &Self) -> bool {
        let (mine, theirs) = (self.sid(), other.sid());
        // SAFETY: both SIDs point into buffers that the system filled and that
        // `self` and `other` keep alive for the call.
        let equal = unsafe { EqualSid(mine, theirs) };
        equal != FALSE
    }

    #[allow(unsafe_code, reason = "ConvertSidToStringSidW has no safe wrapper")]
    fn sid_string(&self) -> io::Result<String> {
        let sid = self.sid();
        let mut wide: PWSTR = ptr::null_mut();
        // SAFETY: `sid` points into `self.buffer`, which outlives the call, and
        // `wide` is a local out-parameter.
        let converted = unsafe { ConvertSidToStringSidW(sid, &raw mut wide) };
        if converted == FALSE {
            return Err(io::Error::last_os_error());
        }
        let _allocation = LocalMemory(wide.cast());
        let mut units = Vec::new();
        loop {
            // SAFETY: the system wrote a NUL-terminated string at `wide`, and the
            // loop stops at the NUL, so every unit read lies inside it.
            let unit = unsafe { wide.wrapping_add(units.len()).read() };
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        String::from_utf16(&units).map_err(io::Error::other)
    }
}

/// `value` as a NUL-terminated UTF-16 string. An interior NUL is refused,
/// because Win32 would silently cut the string short there.
fn wide_nul(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path or descriptor contains a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// The size of `T` as the `u32` that Win32 length fields take.
fn size_u32<T>() -> io::Result<u32> {
    u32::try_from(mem::size_of::<T>()).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::net::windows::named_pipe::ClientOptions;
    use windows_sys::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, GetAce, GetSecurityDescriptorControl, INHERITED_ACE,
        SE_DACL_PROTECTED,
    };

    use super::*;

    /// `ACCESS_ALLOWED_ACE_TYPE`; windows-sys keeps it behind a feature this
    /// crate does not enable.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    /// What the tests check about a DACL, copied out of the system's descriptor.
    struct DaclFacts {
        protected: bool,
        aces: Vec<AceFacts>,
    }

    struct AceFacts {
        allow: bool,
        inherited: bool,
        current_user: bool,
    }

    enum Object<'a> {
        Handle(HANDLE),
        Path(&'a Path),
    }

    fn unique(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{}-{nanos}", std::process::id())
    }

    #[allow(unsafe_code, reason = "reads a security descriptor back through Win32")]
    fn dacl_facts(object: Object<'_>) -> DaclFacts {
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = match object {
            Object::Handle(handle) => {
                // SAFETY: `handle` is open for the call; `dacl` and `descriptor`
                // are local out-parameters and the others are null.
                unsafe {
                    GetSecurityInfo(
                        handle,
                        SE_KERNEL_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &raw mut dacl,
                        ptr::null_mut(),
                        &raw mut descriptor,
                    )
                }
            }
            Object::Path(path) => {
                let wide = wide_nul(path.as_os_str()).unwrap();
                // SAFETY: `wide` is NUL-terminated and outlives the call; `dacl`
                // and `descriptor` are local out-parameters and the others are null.
                unsafe {
                    GetNamedSecurityInfoW(
                        wide.as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &raw mut dacl,
                        ptr::null_mut(),
                        &raw mut descriptor,
                    )
                }
            }
        };
        assert_eq!(status, ERROR_SUCCESS);
        let _allocation = LocalMemory(descriptor);
        assert!(!dacl.is_null());

        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: `descriptor` stays alive in `_allocation`; the out-parameters
        // are locals.
        let read = unsafe {
            GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision)
        };
        assert_ne!(read, FALSE);

        // SAFETY: `dacl` points at the ACL header inside the live descriptor.
        let header = unsafe { dacl.read() };
        let current = ProcessUser::current().unwrap();
        let aces = (0..u32::from(header.AceCount))
            .map(|index| {
                let mut ace: *mut c_void = ptr::null_mut();
                // SAFETY: `index` is below the ACL's entry count, and `ace` is a
                // local out-parameter.
                let found = unsafe { GetAce(dacl, index, &raw mut ace) };
                assert_ne!(found, FALSE);
                // SAFETY: `ace` points at an entry inside the live ACL, and every
                // entry starts with an ACE_HEADER.
                let ace_header = unsafe { ace.cast::<ACE_HEADER>().read() };
                let allow = ace_header.AceType == ACCESS_ALLOWED_ACE_TYPE;
                let current_user = allow && {
                    let sid = ace
                        .cast::<u8>()
                        .wrapping_add(mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
                        .cast::<c_void>();
                    // SAFETY: an allow entry stores its SID at `SidStart`, inside
                    // the live ACL; `current` keeps its own SID alive.
                    let equal = unsafe { EqualSid(sid, current.sid()) };
                    equal != FALSE
                };
                AceFacts {
                    allow,
                    inherited: u32::from(ace_header.AceFlags) & INHERITED_ACE != 0,
                    current_user,
                }
            })
            .collect();
        DaclFacts {
            protected: control & SE_DACL_PROTECTED != 0,
            aces,
        }
    }

    #[test]
    fn pipe_dacl_allows_only_the_current_user_and_both_peers_pass() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        let name = format!(r"\\.\pipe\{}", unique("ck-sys-test"));
        runtime.block_on(async {
            let server = private_pipe_server(&name, true).unwrap();
            assert!(private_pipe_server(&name, true).is_err());

            let pipe = dacl_facts(Object::Handle(server.as_raw_handle()));
            assert_eq!(pipe.aces.len(), 1);
            assert!(pipe.aces[0].allow && pipe.aces[0].current_user);

            let client = ClientOptions::new().open(&name).unwrap();
            server.connect().await.unwrap();
            assert!(pipe_client_is_current_user(&server).unwrap());
            assert!(pipe_server_is_current_user(&client).unwrap());
        });
    }

    #[test]
    fn private_dir_gets_a_protected_dacl_with_explicit_entries_only() {
        let path = std::env::temp_dir().join(unique("ck-sys-test"));
        make_private_dir(&path).unwrap();
        let folder = dacl_facts(Object::Path(&path));
        std::fs::remove_dir(&path).unwrap();

        assert!(folder.protected);
        assert_eq!(folder.aces.len(), 3);
        assert!(folder.aces.iter().all(|ace| ace.allow && !ace.inherited));
        assert!(folder.aces.iter().any(|ace| ace.current_user));
    }
}
