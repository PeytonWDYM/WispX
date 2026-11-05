//! The control socket: one text line in, one JSON line out, per connection.
//!
//! The socket lives in a 0700 directory, so only this user can reach it.

use crate::config::Backend;
use serde_json::{Value, json};
use std::fs::{self, Permissions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

const MAX_REQUEST: u64 = 4 * 1024;
/// A reply can carry the whole transcript.
const MAX_REPLY: u64 = 8 * 1024 * 1024;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_WAIT: Duration = Duration::from_secs(30);
/// Long enough for someone to answer the desktop's permission dialog.
const SETUP_REPLY_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Press,
    Release,
    Start,
    Stop,
    Toggle,
    Cancel,
    Status,
    CopyLast,
    Last,
    Reload,
    Quit,
    SetBackend(Backend),
    PasteSetup,
}

impl Request {
    pub fn encode(self) -> String {
        match self {
            Request::Press => "press".to_string(),
            Request::Release => "release".to_string(),
            Request::Start => "start".to_string(),
            Request::Stop => "stop".to_string(),
            Request::Toggle => "toggle".to_string(),
            Request::Cancel => "cancel".to_string(),
            Request::Status => "status".to_string(),
            Request::CopyLast => "copy-last".to_string(),
            Request::Last => "last".to_string(),
            Request::Reload => "reload".to_string(),
            Request::Quit => "quit".to_string(),
            Request::SetBackend(backend) => format!("backend {}", backend.as_str()),
            Request::PasteSetup => "paste-setup".to_string(),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("backend ") {
            return Backend::parse(name.trim()).map(Request::SetBackend);
        }
        Some(match line {
            "press" => Request::Press,
            "release" => Request::Release,
            "start" => Request::Start,
            "stop" => Request::Stop,
            "toggle" => Request::Toggle,
            "cancel" => Request::Cancel,
            "status" => Request::Status,
            "copy-last" => Request::CopyLast,
            "last" => Request::Last,
            "reload" => Request::Reload,
            "quit" => Request::Quit,
            "paste-setup" => Request::PasteSetup,
            _ => return None,
        })
    }

    fn reply_wait(self) -> Duration {
        match self {
            Request::PasteSetup => SETUP_REPLY_WAIT,
            _ => REPLY_WAIT,
        }
    }
}

pub struct Incoming {
    pub request: Request,
    pub reply: Sender<Value>,
}

pub fn failure(message: impl Into<String>) -> Value {
    json!({ "ok": false, "message": message.into() })
}

/// Binds the socket. Call only while holding the instance lock, since this
/// removes whatever socket a previous daemon left behind.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Counts connections in progress so shutdown can finish replying first.
#[derive(Clone, Default)]
pub struct Connections(Arc<AtomicUsize>);

impl Connections {
    pub fn drain(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        while self.0.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Active(Arc<AtomicUsize>);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Accepts connections on a background thread and hands each request to `send`.
pub fn serve(
    listener: UnixListener,
    send: impl Fn(Incoming) + Send + Sync + 'static,
) -> Connections {
    let connections = Connections::default();
    let counter = connections.0.clone();
    let send = Arc::new(send);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                thread::sleep(Duration::from_millis(50));
                continue;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let active = Active(counter.clone());
            let send = Arc::clone(&send);
            thread::spawn(move || {
                let _active = active;
                handle(stream, send.as_ref());
            });
        }
    });
    connections
}

fn handle(stream: UnixStream, send: &(impl Fn(Incoming) + ?Sized)) {
    let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line);
    let reply = match read.ok().and_then(|_| Request::decode(&line)) {
        Some(request) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            send(Incoming {
                request,
                reply: reply_tx,
            });
            reply_rx
                .recv_timeout(request.reply_wait())
                .unwrap_or_else(|_| failure("the daemon did not answer in time"))
        }
        None => failure("unknown request"),
    };
    let mut text = reply.to_string();
    text.push('\n');
    let mut writer = &stream;
    let _ = writer.write_all(text.as_bytes());
}

#[derive(Debug)]
pub enum ClientError {
    NotRunning,
    Failed(String),
}

/// Sends one request and waits for the daemon's answer.
pub fn send(socket: &Path, request: Request) -> Result<Value, ClientError> {
    let stream = UnixStream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::NotRunning,
        _ => ClientError::Failed(format!("cannot reach the daemon: {error}")),
    })?;
    let failed = |error: io::Error| ClientError::Failed(format!("daemon connection: {error}"));
    stream
        .set_write_timeout(Some(SOCKET_IO_TIMEOUT))
        .map_err(failed)?;
    stream
        .set_read_timeout(Some(request.reply_wait() + SOCKET_IO_TIMEOUT))
        .map_err(failed)?;
    let mut writer = &stream;
    writer
        .write_all(format!("{}\n", request.encode()).as_bytes())
        .map_err(failed)?;

    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REPLY))
        .read_line(&mut line)
        .map_err(failed)?;
    if line.trim().is_empty() {
        return Err(ClientError::Failed(
            "the daemon closed the connection without answering".to_string(),
        ));
    }
    serde_json::from_str(&line)
        .map_err(|_| ClientError::Failed("the daemon sent an invalid reply".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::FileTypeExt;

    const ALL: [Request; 14] = [
        Request::Press,
        Request::Release,
        Request::Start,
        Request::Stop,
        Request::Toggle,
        Request::Cancel,
        Request::Status,
        Request::CopyLast,
        Request::Last,
        Request::Reload,
        Request::Quit,
        Request::SetBackend(Backend::Local),
        Request::SetBackend(Backend::ChatGpt),
        Request::PasteSetup,
    ];

    #[test]
    fn requests_round_trip_and_junk_is_rejected() {
        for request in ALL {
            assert_eq!(Request::decode(&request.encode()), Some(request));
            assert_eq!(
                Request::decode(&format!("{}\n", request.encode())),
                Some(request)
            );
        }
        for junk in ["", "nope", "backend", "backend aws", "press now", "PRESS"] {
            assert_eq!(Request::decode(junk), None, "{junk}");
        }
    }

    fn echo_server(temp: &TempDir) -> (std::path::PathBuf, Connections) {
        let path = temp.path().join("test.sock");
        let listener = bind(&path).unwrap();
        let connections = serve(listener, |incoming: Incoming| {
            let _ = incoming.reply.send(json!({
                "ok": true,
                "message": incoming.request.encode(),
            }));
        });
        (path, connections)
    }

    #[test]
    fn a_request_gets_its_reply_over_a_private_socket() {
        let temp = TempDir::new();
        let (path, connections) = echo_server(&temp);
        let meta = fs::metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);

        let reply = send(&path, Request::SetBackend(Backend::ChatGpt)).unwrap();
        assert_eq!(reply["message"], "backend chatgpt");
        connections.drain(Duration::from_secs(2));
        assert_eq!(connections.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let temp = TempDir::new();
        let path = temp.path().join("test.sock");
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        // Until it execs, a child that another test is forking right now keeps
        // the listener alive, so the socket can take a moment to go stale.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(send(&path, Request::Status), Err(ClientError::NotRunning)) {
            assert!(Instant::now() < deadline, "the socket never went stale");
            thread::sleep(Duration::from_millis(5));
        }
        bind(&path).unwrap();
    }

    #[test]
    fn a_missing_daemon_is_recognized() {
        let temp = TempDir::new();
        let result = send(&temp.path().join("none.sock"), Request::Status);
        assert!(matches!(result, Err(ClientError::NotRunning)));
    }

    #[test]
    fn malformed_and_oversized_requests_get_an_error_without_reaching_the_daemon() {
        let temp = TempDir::new();
        let path = temp.path().join("test.sock");
        let (seen_tx, seen_rx) = mpsc::channel();
        serve(bind(&path).unwrap(), move |incoming: Incoming| {
            let _ = seen_tx.send(incoming.request);
            let _ = incoming.reply.send(json!({"ok": true}));
        });

        let exchange = |payload: &[u8]| {
            let mut stream = UnixStream::connect(&path).unwrap();
            // The server stops reading at its limit, so the rest of a huge
            // request may fail to send and the reply may be cut short.
            let _ = stream.write_all(payload);
            let _ = stream.shutdown(std::net::Shutdown::Write);
            let mut text = String::new();
            let _ = stream.read_to_string(&mut text);
            text
        };
        assert!(exchange(b"launch-missiles\n").contains("unknown request"));
        assert!(exchange(b"\xff\xfe\n").contains("unknown request"));
        exchange(&vec![b'a'; 64 * 1024]);
        // A valid command hidden past the limit must not run either.
        let mut hidden = vec![b' '; MAX_REQUEST as usize];
        hidden.extend_from_slice(b"quit\n");
        exchange(&hidden);
        assert!(seen_rx.try_recv().is_err());
    }

    #[test]
    fn a_client_that_never_finishes_its_line_cannot_block_the_server() {
        let temp = TempDir::new();
        let (path, _connections) = echo_server(&temp);
        let _idle = UnixStream::connect(&path).unwrap();
        let reply = send(&path, Request::Status).unwrap();
        assert_eq!(reply["ok"], true);
    }

    #[test]
    fn a_daemon_that_drops_a_request_produces_a_failure_reply() {
        let temp = TempDir::new();
        let path = temp.path().join("test.sock");
        serve(bind(&path).unwrap(), drop);
        let reply = send(&path, Request::Status).unwrap();
        assert_eq!(reply["ok"], false);
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .contains("did not answer")
        );
    }
}
