//! A loopback HTTP/1.1 server for ck-net's tests, on plain std TCP.
//!
//! It listens on a free port of `127.0.0.1`, answers each request target
//! with a scripted [`Reply`] (404 when none is set), and records every
//! request it reads. Each connection gets one reply with `Connection: close`,
//! so the client never reuses a connection and every request is seen. Bytes
//! that are not an HTTP request, such as a TLS ClientHello, close the
//! connection at once.
//!
//! Shared by `#[path]`: ck-net's unit tests (`src/tests.rs`) and its
//! integration tests, all of them debug-only.
#![allow(dead_code, reason = "each test crate uses part of this server")]

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// How long a connection may sit idle before the server drops it, so no
/// thread waits for ever on a client that went away.
const IDLE: Duration = Duration::from_secs(10);

/// The most bytes of request head the server reads.
const MAX_HEAD: usize = 64 * 1024;

/// What the server answers for one request target.
#[derive(Debug, Clone)]
pub struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Sends only this many bytes of `body`, then waits for the client to
    /// hang up.
    stall_after: Option<usize>,
}

impl Reply {
    /// 200 with `body` and its exact `Content-Length`.
    pub fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Length".to_owned(), body.len().to_string())],
            body,
            stall_after: None,
        }
    }

    /// A redirect `status` to `location`, with an empty body.
    pub fn redirect(status: u16, location: &str) -> Self {
        Self::status(status).header("Location", location)
    }

    /// `status` with an empty body.
    pub fn status(status: u16) -> Self {
        Self {
            status,
            ..Self::ok(Vec::new())
        }
    }

    /// Adds a header line, after the others.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Drops every header named `name`, ignoring case.
    pub fn without(mut self, name: &str) -> Self {
        self.headers
            .retain(|(header, _)| !header.eq_ignore_ascii_case(name));
        self
    }

    /// Sends the headers and the first `bytes` of the body, then stalls.
    pub fn stalling_after(mut self, bytes: usize) -> Self {
        self.stall_after = Some(bytes);
        self
    }
}

/// One request as the server read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    /// For example `GET /catalog-v1.json HTTP/1.1`.
    pub line: String,
    /// In the order sent: names in lower case, values trimmed.
    pub headers: Vec<(String, String)>,
}

impl Recorded {
    /// The first value of the header `name`, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// The server. It stops when dropped.
pub struct TestServer {
    port: u16,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    replies: Mutex<HashMap<String, Reply>>,
    requests: Mutex<Vec<Recorded>>,
    stopping: AtomicBool,
}

impl TestServer {
    /// Listens on a free port of 127.0.0.1, one thread per connection.
    pub fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("the bound address").port();
        let shared = Arc::new(Shared::default());
        let accept = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if shared.stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else {
                        continue;
                    };
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || serve(&shared, stream));
                }
            })
        };
        Self {
            port,
            shared,
            accept: Some(accept),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `http://127.0.0.1:<port>` followed by `target`.
    pub fn url(&self, target: &str) -> String {
        format!("http://127.0.0.1:{}{target}", self.port)
    }

    /// Answers `target` (the path and query, as sent) with `reply` from now
    /// on.
    pub fn reply(&self, target: &str, reply: Reply) {
        lock(&self.shared.replies).insert(target.to_owned(), reply);
    }

    /// The requests so far, oldest first.
    pub fn requests(&self) -> Vec<Recorded> {
        lock(&self.shared.requests).clone()
    }

    /// The request line of each request so far, oldest first.
    pub fn request_lines(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.line)
            .collect()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        // Wakes the accept loop, which then sees `stopping` and ends. When
        // the connection fails the thread is left behind, holding only the
        // listener, rather than joined for ever.
        if TcpStream::connect((Ipv4Addr::LOCALHOST, self.port)).is_ok()
            && let Some(accept) = self.accept.take()
        {
            let _ = accept.join();
        }
    }
}

/// Reads one request head, records it and sends its reply. Any error ends
/// the connection, which the client sees as such.
fn serve(shared: &Shared, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(IDLE));
    let _ = stream.set_write_timeout(Some(IDLE));
    let Some(request) = read_head(&mut stream) else {
        return;
    };
    let target = request
        .line
        .split(' ')
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    lock(&shared.requests).push(request);
    let reply = lock(&shared.replies)
        .get(&target)
        .cloned()
        .unwrap_or_else(|| Reply::status(404));
    let _ = send(&mut stream, &reply);
}

/// The request line and headers, or `None` when the bytes are not an HTTP
/// request or the client goes quiet first.
fn read_head(stream: &mut TcpStream) -> Option<Recorded> {
    let mut head = Vec::new();
    let mut chunk = [0; 4096];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut chunk).ok().filter(|&read| read > 0)?;
        head.extend_from_slice(&chunk[..read]);
        // A request line starts with an upper-case method. A TLS record
        // starts with 0x16, so a ClientHello closes the connection here.
        if !head[0].is_ascii_uppercase() || head.len() > MAX_HEAD {
            return None;
        }
    }
    let text = String::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let line = lines.next()?.to_owned();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    Some(Recorded { line, headers })
}

fn send(stream: &mut TcpStream, reply: &Reply) -> io::Result<()> {
    let headers: String = reply
        .headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let head = format!(
        "HTTP/1.1 {} {}\r\n{headers}Connection: close\r\n\r\n",
        reply.status,
        status_text(reply.status)
    );
    stream.write_all(head.as_bytes())?;
    match reply.stall_after {
        None => stream.write_all(&reply.body)?,
        Some(sent) => {
            stream.write_all(&reply.body[..sent.min(reply.body.len())])?;
            stream.flush()?;
            // Returns when the client hangs up, or after IDLE.
            let _ = io::copy(stream, &mut io::sink());
        }
    }
    stream.flush()
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 | 302 | 303 | 307 | 308 => "Redirect",
        404 => "Not Found",
        _ => "Scripted",
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
