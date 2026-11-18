//! The listening and connected ends of the IPC, the same API on every OS.
//!
//! - **Unix:** a tokio Unix socket, 0600 inside a 0700 folder. Both ends
//!   compare `peer_cred().uid()` with `ck_sys::unix::effective_uid()`.
//! - **Windows:** a tokio named pipe made by `ck_sys::windows::private_pipe_server`.
//!   The server checks the client's token SID and the client checks the
//!   server's.
//!
//! Clients never delete a socket. "Not found" or "connection refused" means no
//! supervisor is running.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::branding;
use crate::error::{Error, Result};
use crate::ipc::endpoint::Endpoint;
use crate::ipc::protocol::{HelloReply, PROTOCOL, Request, Response};
use crate::ipc::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES};
use crate::supervisor::lock::InstanceLock;

/// The supervisor's end: accepts connections from the current user only.
#[derive(Debug)]
pub struct Listener {
    inner: sys::Listener,
}

impl Listener {
    /// Starts listening on `endpoint`. Call inside a tokio runtime with I/O.
    ///
    /// Holding `lock` proves this process is the one supervisor, which is the
    /// only process allowed to remove a stale socket before binding. On Unix
    /// it creates the `/tmp` fallback folder when the endpoint uses one.
    pub fn bind(endpoint: &Endpoint, lock: &InstanceLock) -> Result<Self> {
        // The borrow is the proof; nothing is read from the lock.
        let _ = lock;
        Self::bind_without_lock(endpoint)
    }

    /// [`Listener::bind`] without the proof, so unit tests can listen without
    /// a supervisor. It may remove a stale socket, so outside tests only
    /// `bind` calls it.
    fn bind_without_lock(endpoint: &Endpoint) -> Result<Self> {
        sys::Listener::bind(endpoint).map(|inner| Self { inner })
    }

    /// Waits for the next connection and checks its peer.
    ///
    /// A peer that is not the current user gives
    /// [`Error::PeerRejected`](crate::Error::PeerRejected) after its connection
    /// is closed; the caller logs it and keeps accepting.
    pub async fn accept(&mut self) -> Result<Connection> {
        self.inner
            .accept()
            .await
            .map(|stream| Connection { stream })
    }

    /// Stops listening and removes the socket (Unix) and its fallback folder.
    pub fn close(self) -> Result<()> {
        self.inner.close()
    }
}

/// One connection, on either end, carrying frames.
#[derive(Debug)]
pub struct Connection {
    stream: sys::Stream,
}

impl Connection {
    /// Client end: connects to `endpoint` and checks that the server runs as
    /// the current user.
    ///
    /// No listener gives
    /// [`Error::SupervisorUnavailable`](crate::Error::SupervisorUnavailable).
    pub async fn open(endpoint: &Endpoint) -> Result<Self> {
        sys::connect(endpoint).await.map(|stream| Self { stream })
    }

    /// Sends one frame of at most `limit` bytes.
    pub async fn send<T: Serialize>(&mut self, message: &T, limit: u32) -> Result<()> {
        sys::send(&mut self.stream, message, limit).await
    }

    /// Receives one frame of at most `limit` bytes; `Ok(None)` when the peer
    /// closed cleanly. Errors as in `frame::read_frame`.
    pub async fn recv<T: DeserializeOwned>(&mut self, limit: u32) -> Result<Option<T>> {
        sys::recv(&mut self.stream, limit).await
    }
}

/// The client handshake: sends `hello` and reads the reply.
///
/// A different `protocol` gives
/// [`Error::VersionMismatch`](crate::Error::VersionMismatch), and a `data_dir`
/// other than `endpoint.data` gives
/// [`Error::WrongSupervisor`](crate::Error::WrongSupervisor).
pub async fn handshake(connection: &mut Connection, endpoint: &Endpoint) -> Result<HelloReply> {
    let hello = Request::Hello {
        protocol: PROTOCOL,
        client: format!("{} {}", branding::COMMAND, env!("CARGO_PKG_VERSION")),
    };
    connection.send(&hello, MAX_REQUEST_BYTES).await?;
    let reply = connection
        .recv::<Response<HelloReply>>(MAX_RESPONSE_BYTES)
        .await?
        .ok_or_else(|| Error::SupervisorUnavailable {
            detail: "it closed the connection without answering".to_owned(),
        })?
        .into_result()?;
    check_hello(&reply, endpoint)?;
    Ok(reply)
}

/// Checks a `hello` reply against the endpoint this client derived.
///
/// The data folder is checked first, so `VersionMismatch` always means the
/// supervisor serves this data folder: a caller may still send it `shutdown`,
/// which never changes between versions.
fn check_hello(reply: &HelloReply, endpoint: &Endpoint) -> Result<()> {
    if reply.data_dir != endpoint.data {
        return Err(Error::WrongSupervisor {
            expected: endpoint.data.clone(),
            actual: reply.data_dir.clone(),
        });
    }
    if reply.protocol != PROTOCOL {
        return Err(Error::VersionMismatch {
            ours: PROTOCOL,
            theirs: reply.protocol,
        });
    }
    Ok(())
}

/// The error for "nothing listens at the endpoint".
fn not_running() -> Error {
    Error::SupervisorUnavailable {
        detail: "it is not running".to_owned(),
    }
}

#[cfg(unix)]
use self::unix as sys;
#[cfg(windows)]
use self::windows as sys;

#[cfg(unix)]
mod unix {
    use std::fs::{self, Permissions};
    use std::io;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::net::{UnixListener, UnixStream};

    use crate::error::{Error, Result};
    use crate::ipc::endpoint::Endpoint;
    use crate::ipc::frame;
    use crate::layout;

    const SOCKET_MODE: u32 = 0o600;

    pub(super) type Stream = UnixStream;

    #[derive(Debug)]
    pub(super) struct Listener {
        listener: UnixListener,
        socket: PathBuf,
        fallback_dir: Option<PathBuf>,
    }

    impl Listener {
        pub(super) fn bind(endpoint: &Endpoint) -> Result<Self> {
            let socket = endpoint.address.clone();
            match &endpoint.fallback_dir {
                Some(dir) => prepare_fallback_dir(dir)?,
                None => {
                    let folder = socket.parent().ok_or_else(|| {
                        Error::Internal(format!("{} has no folder", socket.display()))
                    })?;
                    layout::check_private_dir(folder)?;
                }
            }
            remove_stale_socket(&socket)?;

            // The socket is not created under a tightened umask, because the
            // umask is process-wide and other threads may be creating files.
            // Its folder was checked above to be a real 0700 folder of this
            // user, so no other user can reach the socket between `bind` and
            // `set_permissions`; 0600 is a second barrier behind the folder.
            let listener =
                UnixListener::bind(&socket).map_err(|e| Error::io("listen on", &socket, e))?;
            if let Err(e) = fs::set_permissions(&socket, Permissions::from_mode(SOCKET_MODE)) {
                drop(listener);
                // Best effort: the permissions error is the one worth reporting.
                let _ = fs::remove_file(&socket);
                return Err(Error::io("set permissions on", &socket, e));
            }

            Ok(Self {
                listener,
                socket,
                fallback_dir: endpoint.fallback_dir.clone(),
            })
        }

        /// Cancel safe: the only `.await` comes first, and the peer check
        /// after it does not wait.
        pub(super) async fn accept(&mut self) -> Result<Stream> {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| Error::os("accept a connection", e))?;
            // Fails closed: on any error below, `stream` is dropped here,
            // which closes the connection before the caller sees the error.
            let peer = stream
                .peer_cred()
                .map_err(|e| Error::os("read the user of a connection", e))?;
            check_peer_uid(peer.uid(), ck_sys::unix::effective_uid())?;
            Ok(stream)
        }

        pub(super) fn close(self) -> Result<()> {
            let Self {
                listener,
                socket,
                fallback_dir,
            } = self;
            drop(listener);
            remove_if_present(&socket, fs::remove_file(&socket))?;
            if let Some(dir) = fallback_dir {
                remove_if_present(&dir, fs::remove_dir(&dir))?;
            }
            Ok(())
        }
    }

    pub(super) async fn connect(endpoint: &Endpoint) -> Result<Stream> {
        let stream = match UnixStream::connect(&endpoint.address).await {
            Ok(stream) => stream,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(super::not_running());
            }
            Err(e) => return Err(Error::io("connect to", &endpoint.address, e)),
        };
        let peer = stream
            .peer_cred()
            .map_err(|e| Error::os("read the user of the supervisor", e))?;
        check_peer_uid(peer.uid(), ck_sys::unix::effective_uid())?;
        Ok(stream)
    }

    pub(super) async fn send<T: Serialize>(
        stream: &mut Stream,
        message: &T,
        limit: u32,
    ) -> Result<()> {
        frame::write_frame(stream, message, limit).await
    }

    pub(super) async fn recv<T: DeserializeOwned>(
        stream: &mut Stream,
        limit: u32,
    ) -> Result<Option<T>> {
        frame::read_frame(stream, limit).await
    }

    /// The peer rule on both ends: the same uid as this process, and no
    /// exception for root.
    pub(super) fn check_peer_uid(peer: u32, own: u32) -> Result<()> {
        if peer == own {
            Ok(())
        } else {
            Err(Error::PeerRejected)
        }
    }

    /// Creates the `/tmp` folder for a long data path (0700), or reuses it,
    /// then checks with `lstat` that it is a real folder of this user with
    /// mode 0700. An existing folder that breaks a rule is refused, never
    /// repaired: another user may have planted it.
    pub(super) fn prepare_fallback_dir(dir: &Path) -> Result<()> {
        layout::create_private_dir(dir)?;
        layout::check_private_dir(dir)
    }

    /// Removes a socket left by a supervisor that died. Only the lock holder
    /// gets here (through `Listener::bind`), so a live supervisor's socket is
    /// never removed. Anything that is not a socket is left alone.
    fn remove_stale_socket(socket: &Path) -> Result<()> {
        match fs::symlink_metadata(socket) {
            Ok(meta) if meta.file_type().is_socket() => {
                fs::remove_file(socket).map_err(|e| Error::io("remove the stale socket", socket, e))
            }
            Ok(_) => Err(Error::io(
                "listen on",
                socket,
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "something other than a socket is in its place",
                ),
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io("inspect", socket, e)),
        }
    }

    fn remove_if_present(path: &Path, result: io::Result<()>) -> Result<()> {
        match result {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Error::io("remove", path, e)),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::time::Duration;

    use ck_sys::windows::{
        ERROR_PIPE_BUSY, pipe_client_is_current_user, pipe_server_is_current_user,
        private_pipe_server,
    };
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
    use tokio::time::{Instant, sleep};

    use crate::error::{Error, Result};
    use crate::ipc::endpoint::Endpoint;
    use crate::ipc::frame;

    /// How long a client retries a busy pipe. The supervisor always keeps one
    /// instance waiting, so "busy" lasts only until it makes the next one.
    const BUSY_WAIT: Duration = Duration::from_secs(1);
    const BUSY_RETRY: Duration = Duration::from_millis(20);

    #[derive(Debug)]
    pub(super) enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    #[derive(Debug)]
    pub(super) struct Listener {
        name: String,
        /// The instance waiting for the next client. There is always one, so
        /// the pipe name never lapses and no other process can take it.
        next: NamedPipeServer,
    }

    impl Listener {
        pub(super) fn bind(endpoint: &Endpoint) -> Result<Self> {
            let name = endpoint
                .address
                .to_str()
                .ok_or_else(|| {
                    Error::Internal(format!(
                        "the pipe name {} is not valid Unicode",
                        endpoint.address.display()
                    ))
                })?
                .to_owned();
            // `first_instance` fails when any process already owns the name,
            // so nobody can have squatted on it.
            let next = private_pipe_server(&name, true)
                .map_err(|e| Error::io("create the pipe", &endpoint.address, e))?;
            Ok(Self { name, next })
        }

        /// Cancel safe: the only `.await` comes first (tokio's `connect` is
        /// cancel safe), and nothing after it waits.
        pub(super) async fn accept(&mut self) -> Result<Stream> {
            self.next
                .connect()
                .await
                .map_err(|e| Error::os("wait for a connection", e))?;

            // The next instance exists before this one is handed off.
            let fresh = match private_pipe_server(&self.name, false) {
                Ok(fresh) => fresh,
                Err(e) => {
                    // Keep this instance, and so the name, but drop its client.
                    // Best effort: the creation error is the one worth reporting.
                    let _ = self.next.disconnect();
                    return Err(Error::os("create the next pipe instance", e));
                }
            };
            let pipe = std::mem::replace(&mut self.next, fresh);

            // A client that cannot be checked (for example, one that has
            // already exited) is refused like one of another user, so the
            // caller logs it and keeps accepting. `pipe` is dropped here,
            // which closes the client's connection.
            match pipe_client_is_current_user(&pipe) {
                Ok(true) => Ok(Stream::Server(pipe)),
                Ok(false) | Err(_) => Err(Error::PeerRejected),
            }
        }

        /// Dropping the last instance removes the pipe name; there is no file.
        pub(super) fn close(self) -> Result<()> {
            drop(self);
            Ok(())
        }
    }

    pub(super) async fn connect(endpoint: &Endpoint) -> Result<Stream> {
        let deadline = Instant::now() + BUSY_WAIT;
        let pipe = loop {
            match ClientOptions::new().open(&endpoint.address) {
                Ok(pipe) => break pipe,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(super::not_running());
                }
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY.cast_signed())
                        && Instant::now() < deadline =>
                {
                    sleep(BUSY_RETRY).await;
                }
                Err(e) => return Err(Error::io("connect to", &endpoint.address, e)),
            }
        };
        // A server that cannot be checked is refused like one of another user.
        match pipe_server_is_current_user(&pipe) {
            Ok(true) => Ok(Stream::Client(pipe)),
            Ok(false) | Err(_) => Err(Error::PeerRejected),
        }
    }

    pub(super) async fn send<T: Serialize>(
        stream: &mut Stream,
        message: &T,
        limit: u32,
    ) -> Result<()> {
        match stream {
            Stream::Server(pipe) => frame::write_frame(pipe, message, limit).await,
            Stream::Client(pipe) => frame::write_frame(pipe, message, limit).await,
        }
    }

    pub(super) async fn recv<T: DeserializeOwned>(
        stream: &mut Stream,
        limit: u32,
    ) -> Result<Option<T>> {
        match stream {
            Stream::Server(pipe) => frame::read_frame(pipe, limit).await,
            Stream::Client(pipe) => frame::read_frame(pipe, limit).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::paths::BuildKind;

    #[test]
    fn a_hello_reply_must_name_this_data_folder_and_protocol() {
        let data = std::env::temp_dir().join("ck-dev");
        let endpoint = Endpoint {
            data: data.clone(),
            address: data.join("run").join("ck.sock"),
            fallback_dir: None,
        };
        let reply = |protocol: u32, data_dir: PathBuf| HelloReply {
            protocol,
            version: "0.1.0".to_owned(),
            pid: 42,
            build: BuildKind::Debug,
            data_dir,
        };

        assert!(check_hello(&reply(PROTOCOL, data.clone()), &endpoint).is_ok());
        assert!(matches!(
            check_hello(&reply(PROTOCOL + 1, data), &endpoint),
            Err(Error::VersionMismatch { ours: PROTOCOL, theirs }) if theirs == PROTOCOL + 1
        ));
        // Another data folder wins over another version.
        let other = std::env::temp_dir().join("ck-dev-other");
        assert!(matches!(
            check_hello(&reply(PROTOCOL + 1, other), &endpoint),
            Err(Error::WrongSupervisor { .. })
        ));
    }

    #[cfg(unix)]
    mod unix {
        use std::fs;
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        use serde_json::{Value, json};

        use super::super::sys::{check_peer_uid, prepare_fallback_dir};
        use super::super::*;
        use crate::layout;
        use crate::paths::{Paths, Profile};

        /// A folder under the system temp folder. On drop it is removed with
        /// everything inside, along with any `/tmp` socket folder a test
        /// endpoint used.
        struct Scratch {
            root: PathBuf,
            fallback_dirs: Vec<PathBuf>,
        }

        impl Scratch {
            fn new(tag: &str) -> Self {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let root =
                    std::env::temp_dir().join(format!("ck-{tag}-{}-{n}", std::process::id()));
                let _ = fs::remove_dir_all(&root);
                fs::create_dir(&root).unwrap();
                Self {
                    root,
                    fallback_dirs: Vec::new(),
                }
            }

            /// Lays out a profile whose data folder is `<root>/<name>` and
            /// returns its endpoint.
            fn endpoint(&mut self, name: &str) -> Endpoint {
                let paths = Paths {
                    home: self.root.clone(),
                    data: self.root.join(name),
                    profile: Profile::Isolated,
                };
                layout::ensure(&paths).unwrap();
                assert_eq!(mode(&paths.data), 0o700);
                let endpoint = Endpoint::locate(&paths.data).unwrap().unwrap();
                self.fallback_dirs.extend(endpoint.fallback_dir.clone());
                endpoint
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                for dir in self.fallback_dirs.iter().chain([&self.root]) {
                    let _ = fs::remove_dir_all(dir);
                }
            }
        }

        fn mode(path: &Path) -> u32 {
            fs::symlink_metadata(path).unwrap().mode() & 0o7777
        }

        #[tokio::test]
        async fn the_socket_is_private_and_serves_the_same_user() {
            let mut scratch = Scratch::new("sock");
            // The socket sits in `run/` unless the temp folder itself is long
            // enough to push it to the `/tmp` fallback; both must be private.
            let endpoint = scratch.endpoint("d");
            let mut listener = Listener::bind_without_lock(&endpoint).unwrap();

            let socket = fs::symlink_metadata(&endpoint.address).unwrap();
            assert!(socket.file_type().is_socket());
            assert_eq!(socket.permissions().mode() & 0o7777, 0o600);
            let folder = endpoint.address.parent().unwrap();
            assert_eq!(mode(folder), 0o700);

            // Same uid: accepted at both ends, and frames flow.
            let (server, client) = tokio::join!(listener.accept(), Connection::open(&endpoint));
            let (mut server, mut client) = (server.unwrap(), client.unwrap());
            let request = json!({"op": "status"});
            client.send(&request, MAX_REQUEST_BYTES).await.unwrap();
            let received: Option<Value> = server.recv(MAX_REQUEST_BYTES).await.unwrap();
            assert_eq!(received, Some(request));
            drop(client);
            let after_close: Option<Value> = server.recv(MAX_REQUEST_BYTES).await.unwrap();
            assert!(after_close.is_none());

            listener.close().unwrap();
            assert!(fs::symlink_metadata(&endpoint.address).is_err());
        }

        #[tokio::test]
        async fn no_listener_means_not_running_and_bind_replaces_a_stale_socket() {
            let mut scratch = Scratch::new("stale");
            let endpoint = scratch.endpoint("d");

            let nothing = Connection::open(&endpoint).await;
            assert!(matches!(nothing, Err(Error::SupervisorUnavailable { .. })));

            // A supervisor that died leaves its socket behind.
            drop(Listener::bind_without_lock(&endpoint).unwrap());
            let left = fs::symlink_metadata(&endpoint.address).unwrap();
            assert!(left.file_type().is_socket());
            let refused = Connection::open(&endpoint).await;
            assert!(matches!(refused, Err(Error::SupervisorUnavailable { .. })));

            let listener = Listener::bind_without_lock(&endpoint).unwrap();
            listener.close().unwrap();
        }

        #[test]
        fn only_the_same_uid_passes_the_peer_check() {
            assert!(check_peer_uid(1000, 1000).is_ok());
            assert!(matches!(
                check_peer_uid(1001, 1000),
                Err(Error::PeerRejected)
            ));
            assert!(matches!(check_peer_uid(0, 1000), Err(Error::PeerRejected)));
        }

        #[test]
        fn a_fallback_folder_must_be_a_real_private_folder() {
            let scratch = Scratch::new("fallback");

            let fresh = scratch.root.join("fresh");
            prepare_fallback_dir(&fresh).unwrap();
            assert_eq!(mode(&fresh), 0o700);
            prepare_fallback_dir(&fresh).unwrap();

            // A link is refused even when it points at a private folder.
            let link = scratch.root.join("link");
            symlink(&fresh, &link).unwrap();
            assert!(matches!(
                prepare_fallback_dir(&link),
                Err(Error::NotPrivate { .. })
            ));

            // A folder others can enter is refused, and left unchanged.
            let open = scratch.root.join("open");
            fs::create_dir(&open).unwrap();
            fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(
                prepare_fallback_dir(&open),
                Err(Error::NotPrivate { .. })
            ));
            assert_eq!(mode(&open), 0o755);
        }
    }
}
