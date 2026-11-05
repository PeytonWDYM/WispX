//! The control socket: one text line in, one JSON line out, per connection. The
//! exception is `watch`, whose connection stays open and carries a JSON line for
//! everything the desktop interface has to show.
//!
//! On Unix the socket lives in a 0700 directory, so only this user can reach it.
//! On Windows it is a named pipe that only this user may open.

use crate::config::Backend;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(windows)]
pub use crate::win::pipe::{PipeListener as Listener, PipeStream as Stream};

const MAX_REQUEST: u64 = 4 * 1024;
/// A reply can carry the whole transcript.
const MAX_REPLY: u64 = 8 * 1024 * 1024;
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_WAIT: Duration = Duration::from_secs(30);
/// Long enough for someone to answer the desktop's permission dialog.
const SETUP_REPLY_WAIT: Duration = Duration::from_secs(300);
/// How quickly a watcher that went away is noticed.
const WATCH_POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Saves a JSON object of settings over the config file and applies them.
    Configure(String),
    /// Runs the `wispx doctor` checks and answers with the results.
    Readiness,
    /// Streams status changes, the mic level and messages. With
    /// `presents_feedback`, the caller shows them on screen in place of popups.
    Watch {
        presents_feedback: bool,
    },
}

impl Request {
    pub fn encode(&self) -> String {
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
            Request::Configure(patch) => format!("configure {patch}"),
            Request::Readiness => "readiness".to_string(),
            Request::Watch {
                presents_feedback: false,
            } => "watch".to_string(),
            Request::Watch {
                presents_feedback: true,
            } => "watch feedback".to_string(),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("backend ") {
            return Backend::parse(name.trim()).map(Request::SetBackend);
        }
        if let Some(patch) = line.strip_prefix("configure ") {
            let patch = patch.trim();
            return matches!(serde_json::from_str(patch), Ok(Value::Object(_)))
                .then(|| Request::Configure(patch.to_string()));
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
            "readiness" => Request::Readiness,
            "watch" => Request::Watch {
                presents_feedback: false,
            },
            "watch feedback" => Request::Watch {
                presents_feedback: true,
            },
            _ => return None,
        })
    }

    fn reply_wait(&self) -> Duration {
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
#[cfg(unix)]
pub fn bind(path: &Path) -> io::Result<Listener> {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::PermissionsExt;

    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = Listener::bind(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// A pipe goes when its last handle does, so nothing is ever left behind.
#[cfg(windows)]
pub fn bind(path: &Path) -> io::Result<Listener> {
    Listener::bind(path)
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
pub fn serve(listener: Listener, send: impl Fn(Incoming) + Send + Sync + 'static) -> Connections {
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

fn handle(stream: Stream, send: &(impl Fn(Incoming) + ?Sized)) {
    let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line);
    let reply = match read.ok().and_then(|_| Request::decode(&line)) {
        Some(request @ Request::Watch { .. }) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            send(Incoming {
                request,
                reply: reply_tx,
            });
            stream_replies(&stream, &reply_rx);
            return;
        }
        Some(request) => {
            let wait = request.reply_wait();
            let (reply_tx, reply_rx) = mpsc::channel();
            send(Incoming {
                request,
                reply: reply_tx,
            });
            reply_rx
                .recv_timeout(wait)
                .unwrap_or_else(|_| failure("the daemon did not answer in time"))
        }
        None => failure("unknown request"),
    };
    let mut text = reply.to_string();
    text.push('\n');
    let mut writer = &stream;
    let _ = writer.write_all(text.as_bytes());
}

/// Writes each event as it comes, until the daemon drops the sender or the client
/// hangs up. Returning drops the receiver, and that is how the daemon finds out.
fn stream_replies(stream: &Stream, events: &mpsc::Receiver<Value>) {
    let _ = stream.set_nonblocking(true);
    loop {
        match events.recv_timeout(WATCH_POLL) {
            Ok(event) => {
                let _ = stream.set_nonblocking(false);
                let mut text = event.to_string();
                text.push('\n');
                let mut writer = stream;
                if writer.write_all(text.as_bytes()).is_err() {
                    return;
                }
                let _ = stream.set_nonblocking(true);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let mut probe = [0u8; 64];
        let mut reader = stream;
        match reader.read(&mut probe) {
            // The client has nothing to say here, so this can only be it leaving.
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return,
        }
    }
}

/// An open `watch` connection, read a line at a time.
pub struct Watch {
    reader: BufReader<Stream>,
}

impl Watch {
    pub fn open(socket: &Path, presents_feedback: bool) -> Result<Watch, ClientError> {
        let stream = connect(socket)?;
        let failed = |error: io::Error| ClientError::Failed(format!("daemon connection: {error}"));
        stream
            .set_write_timeout(Some(SOCKET_IO_TIMEOUT))
            .map_err(failed)?;
        let mut writer = &stream;
        writer
            .write_all(format!("{}\n", Request::Watch { presents_feedback }.encode()).as_bytes())
            .map_err(failed)?;
        Ok(Watch {
            reader: BufReader::new(stream),
        })
    }

    /// The next event, or `None` once the daemon has gone.
    pub fn next_event(&mut self) -> Option<Value> {
        loop {
            let mut line = String::new();
            let read = (&mut self.reader).take(MAX_REPLY).read_line(&mut line);
            match read {
                Ok(0) | Err(_) => return None,
                Ok(_) => {
                    if let Ok(event) = serde_json::from_str(&line) {
                        return Some(event);
                    }
                }
            }
        }
    }

    /// Ends the connection from another thread, which unblocks `next_event`.
    pub fn closer(&self) -> Option<Stream> {
        self.reader.get_ref().try_clone().ok()
    }
}

fn connect(socket: &Path) -> Result<Stream, ClientError> {
    Stream::connect(socket).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ClientError::NotRunning,
        _ => ClientError::Failed(format!("cannot reach the daemon: {error}")),
    })
}

#[derive(Debug)]
pub enum ClientError {
    NotRunning,
    Failed(String),
}

/// Sends one request and waits for the daemon's answer.
pub fn send(socket: &Path, request: Request) -> Result<Value, ClientError> {
    let stream = connect(socket)?;
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};

    fn all() -> Vec<Request> {
        vec![
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
            Request::Configure(r#"{"sounds":false}"#.to_string()),
            Request::Readiness,
            Request::Watch {
                presents_feedback: false,
            },
            Request::Watch {
                presents_feedback: true,
            },
        ]
    }

    #[test]
    fn requests_round_trip_and_junk_is_rejected() {
        for request in all() {
            assert_eq!(Request::decode(&request.encode()).as_ref(), Some(&request));
            assert_eq!(
                Request::decode(&format!("{}\n", request.encode())).as_ref(),
                Some(&request)
            );
        }
        for junk in [
            "",
            "nope",
            "backend",
            "backend aws",
            "press now",
            "PRESS",
            "configure",
            "configure [1]",
            "configure {broken",
            "watch everything",
        ] {
            assert_eq!(Request::decode(junk), None, "{junk}");
        }
    }

    #[test]
    fn a_watch_streams_until_the_daemon_stops_and_notices_a_client_leaving() {
        let temp = TempDir::new();
        let path = temp.path().join("test.sock");
        let (senders_tx, senders_rx) = mpsc::channel();
        let connections = serve(bind(&path).unwrap(), move |incoming: Incoming| {
            let _ = senders_tx.send((incoming.request, incoming.reply));
        });

        // Events arrive in order for as long as the daemon keeps the sender.
        let mut watch = Watch::open(&path, true).unwrap();
        let (request, events) = senders_rx.recv().unwrap();
        assert_eq!(
            request,
            Request::Watch {
                presents_feedback: true
            }
        );
        for number in 0..3 {
            events.send(json!({ "number": number })).unwrap();
        }
        for number in 0..3 {
            assert_eq!(watch.next_event().unwrap()["number"], number);
        }

        // The daemon going away ends the stream, and so frees the connection.
        drop(events);
        assert!(watch.next_event().is_none());
        connections.drain(Duration::from_secs(2));
        assert_eq!(connections.0.load(Ordering::SeqCst), 0);

        // A client that hangs up is noticed without any event to write.
        let watch = Watch::open(&path, false).unwrap();
        let (_, events) = senders_rx.recv().unwrap();
        drop(watch);
        let deadline = Instant::now() + Duration::from_secs(5);
        while events.send(json!({})).is_ok() {
            assert!(Instant::now() < deadline, "the watcher was never dropped");
            thread::sleep(Duration::from_millis(10));
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
