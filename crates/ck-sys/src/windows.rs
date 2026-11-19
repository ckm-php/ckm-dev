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
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SE_KERNEL_OBJECT,
    SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetTokenInformation, INHERITED_ACE, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
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

/// `ACCESS_ALLOWED_ACE_TYPE`; windows-sys keeps it behind a feature this crate
/// does not enable.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// LocalSystem and BUILTIN\Administrators, the two accounts besides the
/// current user that a private folder admits.
const SYSTEM_SID: &str = "S-1-5-18";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";

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
/// - The descriptor is `O:<current SID>D:P(A;;GA;;;<current SID>)`: the
///   current user owns it, and its protected DACL has one allow entry. The
///   explicit owner is what a client checks; without it an elevated process
///   would give the pipe to Administrators.
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
    let descriptor = security_descriptor(&format!("O:{sid}D:P(A;;GA;;;{sid})"))?;
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

/// Whether the server end of `pipe` runs as the current user (token SID), and
/// the pipe itself is owned by the current user.
///
/// The owner check does not depend on the server's process id, so a server
/// that exits and has its id reused between the two reads still fails it.
///
/// Safety contract: as [`pipe_client_is_current_user`], starting from
/// `GetNamedPipeServerProcessId`; the owner is read from the borrowed pipe
/// handle with `GetSecurityInfo`.
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
    if !runs_as_current_user(pid)? {
        return Ok(false);
    }
    let pipe_security = SecurityInfo::read(Object::Handle(pipe.as_raw_handle()))?;
    Ok(pipe_security.owner_is(&ProcessUser::current()?))
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
/// It sets the current user as owner and a protected DACL (no entries
/// inherited from the parent) that gives full control to the current user,
/// SYSTEM and Administrators only, and is inherited by every file and folder
/// inside. The explicit owner matters on an elevated token, whose new objects
/// may otherwise be owned by Administrators and fail [`is_private_dir`].
///
/// Safety contract: builds the descriptor from SDDL with
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`, applies its DACL
/// and then its owner with two `SetNamedSecurityInfoW` calls on a
/// NUL-terminated wide copy of `path` owned by this function, and frees the
/// descriptor with `LocalFree`.
#[allow(
    unsafe_code,
    reason = "folder DACLs are set through Win32 calls with no safe wrapper"
)]
pub fn make_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    let sid = current_user_sid()?;
    let descriptor = security_descriptor(&format!(
        "O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"
    ))?;

    let mut owner: PSID = ptr::null_mut();
    let mut owner_defaulted = FALSE;
    // SAFETY: the descriptor is owned by `descriptor` and alive; both
    // out-parameters are locals.
    let found = unsafe {
        GetSecurityDescriptorOwner(descriptor.0, &raw mut owner, &raw mut owner_defaulted)
    };
    if found == FALSE {
        return Err(io::Error::last_os_error());
    }
    if owner.is_null() {
        return Err(io::Error::other(
            "the private folder's security descriptor has no owner",
        ));
    }

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
    // The DACL goes first: setting the owner needs WRITE_OWNER, which the
    // user may only hold once this DACL grants full control.
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
    // SAFETY: `wide_path` is NUL-terminated and `owner` points into
    // `descriptor`; both outlive the call. Group, DACL and SACL are null, so
    // they are left unchanged.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            owner,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    Ok(())
}

/// Whether the folder `path` is private as [`make_private_dir`] leaves it: the
/// current user owns it, its DACL is protected, and every entry is an explicit
/// allow entry for the current user, SYSTEM or Administrators, with one for
/// the current user.
///
/// Safety contract: reads the owner and DACL through [`SecurityInfo`], which
/// owns the descriptor they point into.
pub fn is_private_dir(path: &Path) -> io::Result<bool> {
    let security = SecurityInfo::read(Object::Path(path))?;
    let user = ProcessUser::current()?;
    if !security.owner_is(&user) || !security.dacl_protected()? {
        return Ok(false);
    }
    let Some(aces) = security.aces()? else {
        return Ok(false);
    };
    let own = user.sid_string()?;
    let mut admits_user = false;
    for ace in aces {
        let Some(sid) = ace.allowed_sid.filter(|_| !ace.inherited) else {
            return Ok(false);
        };
        if sid == own {
            admits_user = true;
        } else if sid != SYSTEM_SID && sid != ADMINISTRATORS_SID {
            return Ok(false);
        }
    }
    Ok(admits_user)
}

/// Whether the current user owns the file or folder `path`.
///
/// Safety contract: as [`is_private_dir`].
pub fn is_owned_by_current_user(path: &Path) -> io::Result<bool> {
    let security = SecurityInfo::read(Object::Path(path))?;
    Ok(security.owner_is(&ProcessUser::current()?))
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

/// What [`SecurityInfo::read`] reads from.
enum Object<'a> {
    /// A kernel object, such as a pipe, through a handle the caller keeps open.
    Handle(HANDLE),
    /// A file or folder, by path.
    Path(&'a Path),
}

/// The owner and DACL of an object, read back from the system.
struct SecurityInfo {
    /// Owns the self-relative descriptor that `owner` and `dacl` point into.
    descriptor: LocalMemory,
    /// Null when the descriptor has no owner.
    owner: PSID,
    /// Null for a NULL DACL, which admits everyone.
    dacl: *mut ACL,
}

/// One entry of a DACL.
struct Ace {
    /// The SID in string form for an allow entry; `None` for any other kind.
    allowed_sid: Option<String>,
    /// Inherited from a parent rather than set on the object.
    inherited: bool,
}

impl SecurityInfo {
    /// Reads the owner and DACL of `object`.
    #[allow(unsafe_code, reason = "security descriptors are read through Win32")]
    fn read(object: Object<'_>) -> io::Result<Self> {
        let which = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut owner: PSID = ptr::null_mut();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let status = match object {
            Object::Handle(handle) => {
                // SAFETY: the caller keeps `handle` open for the call; `owner`,
                // `dacl` and `descriptor` are local out-parameters, and group
                // and SACL are null because they are not requested.
                unsafe {
                    GetSecurityInfo(
                        handle,
                        SE_KERNEL_OBJECT,
                        which,
                        &raw mut owner,
                        ptr::null_mut(),
                        &raw mut dacl,
                        ptr::null_mut(),
                        &raw mut descriptor,
                    )
                }
            }
            Object::Path(path) => {
                let wide = wide_nul(path.as_os_str())?;
                // SAFETY: `wide` is NUL-terminated and outlives the call; the
                // out-parameters are as above.
                unsafe {
                    GetNamedSecurityInfoW(
                        wide.as_ptr(),
                        SE_FILE_OBJECT,
                        which,
                        &raw mut owner,
                        ptr::null_mut(),
                        &raw mut dacl,
                        ptr::null_mut(),
                        &raw mut descriptor,
                    )
                }
            }
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status.cast_signed()));
        }
        Ok(Self {
            descriptor: LocalMemory(descriptor),
            owner,
            dacl,
        })
    }

    /// Whether `user` owns the object.
    #[allow(unsafe_code, reason = "EqualSid has no safe wrapper")]
    fn owner_is(&self, user: &ProcessUser) -> bool {
        if self.owner.is_null() {
            return false;
        }
        // SAFETY: `self.owner` points into the descriptor that `self` owns, and
        // `user` keeps its own SID alive for the call.
        let equal = unsafe { EqualSid(self.owner, user.sid()) };
        equal != FALSE
    }

    /// Whether the DACL is protected from inheriting its parent's entries.
    #[allow(
        unsafe_code,
        reason = "GetSecurityDescriptorControl has no safe wrapper"
    )]
    fn dacl_protected(&self) -> io::Result<bool> {
        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: the descriptor is owned by `self` and alive; the
        // out-parameters are locals.
        let read = unsafe {
            GetSecurityDescriptorControl(self.descriptor.0, &raw mut control, &raw mut revision)
        };
        if read == FALSE {
            return Err(io::Error::last_os_error());
        }
        Ok(control & SE_DACL_PROTECTED != 0)
    }

    /// The DACL's entries, or `None` for a NULL DACL.
    #[allow(unsafe_code, reason = "ACL entries are read through Win32")]
    fn aces(&self) -> io::Result<Option<Vec<Ace>>> {
        if self.dacl.is_null() {
            return Ok(None);
        }
        // SAFETY: `self.dacl` points at the ACL header inside the descriptor
        // that `self` owns.
        let header = unsafe { self.dacl.read() };
        let mut aces = Vec::with_capacity(usize::from(header.AceCount));
        for index in 0..u32::from(header.AceCount) {
            let mut ace: *mut c_void = ptr::null_mut();
            // SAFETY: `index` is below the ACL's entry count, the ACL lives in
            // `self`, and `ace` is a local out-parameter.
            let found = unsafe { GetAce(self.dacl, index, &raw mut ace) };
            if found == FALSE {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `ace` points at an entry inside the live ACL, and every
            // entry starts with an ACE_HEADER.
            let ace_header = unsafe { ace.cast::<ACE_HEADER>().read() };
            let allowed_sid = if ace_header.AceType == ACCESS_ALLOWED_ACE_TYPE {
                // An allow entry stores its SID at `SidStart`, inside the ACL.
                let sid = ace
                    .cast::<u8>()
                    .wrapping_add(mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
                    .cast::<c_void>();
                Some(sid_to_string(sid)?)
            } else {
                None
            };
            aces.push(Ace {
                allowed_sid,
                inherited: u32::from(ace_header.AceFlags) & INHERITED_ACE != 0,
            });
        }
        Ok(Some(aces))
    }
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

    fn sid_string(&self) -> io::Result<String> {
        sid_to_string(self.sid())
    }
}

/// `sid` in string form (`S-1-5-…`). The caller keeps the SID alive and valid
/// for the call: it points into a buffer or descriptor the system filled.
#[allow(unsafe_code, reason = "ConvertSidToStringSidW has no safe wrapper")]
fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut wide: PWSTR = ptr::null_mut();
    // SAFETY: the caller keeps `sid` valid for the call, and `wide` is a local
    // out-parameter.
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

    use super::*;

    fn unique(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{}-{nanos}", std::process::id())
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

            let user = ProcessUser::current().unwrap();
            let pipe = SecurityInfo::read(Object::Handle(server.as_raw_handle())).unwrap();
            assert!(pipe.owner_is(&user));
            let aces = pipe.aces().unwrap().unwrap();
            assert_eq!(aces.len(), 1);
            assert_eq!(aces[0].allowed_sid, Some(user.sid_string().unwrap()));

            let client = ClientOptions::new().open(&name).unwrap();
            server.connect().await.unwrap();
            assert!(pipe_client_is_current_user(&server).unwrap());
            assert!(pipe_server_is_current_user(&client).unwrap());
        });
    }

    #[test]
    fn private_dir_gets_a_protected_dacl_with_explicit_entries_only() {
        let path = std::env::temp_dir().join(unique("ck-sys-test"));
        std::fs::create_dir(&path).unwrap();
        let plain = is_private_dir(&path);
        make_private_dir(&path).unwrap();
        let private = is_private_dir(&path);
        let folder = SecurityInfo::read(Object::Path(&path)).unwrap();
        let protected = folder.dacl_protected().unwrap();
        let aces = folder.aces().unwrap().unwrap();
        let own = ProcessUser::current().unwrap().sid_string().unwrap();
        drop(folder);
        std::fs::remove_dir(&path).unwrap();

        assert!(!plain.unwrap(), "a folder that inherits its parent's DACL");
        assert!(private.unwrap());
        assert!(protected);
        assert_eq!(aces.len(), 3);
        assert!(
            aces.iter()
                .all(|ace| ace.allowed_sid.is_some() && !ace.inherited)
        );
        assert!(
            aces.iter()
                .any(|ace| ace.allowed_sid.as_deref() == Some(own.as_str()))
        );
    }
}
