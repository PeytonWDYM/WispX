//! Shared plumbing for XDG desktop portals: the sanctioned way for a Wayland
//! program to ask the desktop for things it may not simply take.

use crate::dbus::{Connection, Error, Message, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const BUS_NAME: &str = "org.freedesktop.portal.Desktop";
pub const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
/// Must match the desktop entry `wispx setup` installs, since portals use that
/// file to identify a program that is not sandboxed.
pub const APP_ID: &str = "io.github.PeytonWDYM.WispX";
pub const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";

const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const REGISTRY_INTERFACE: &str = "org.freedesktop.host.portal.Registry";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTER_ATTEMPTS: u32 = 8;
const REGISTER_RETRY: Duration = Duration::from_millis(250);

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(0);

/// A unique token, valid as an object path element.
pub fn token() -> String {
    format!(
        "wispx_{}_{}",
        std::process::id(),
        NEXT_TOKEN.fetch_add(1, Ordering::SeqCst)
    )
}

/// The version of a portal interface, or `None` where the desktop lacks it. Any
/// other failure is an error, since a portal still starting up may be slow.
pub fn interface_version(bus: &mut Connection, interface: &str) -> Result<Option<u32>, Error> {
    let reply = bus.call(
        BUS_NAME,
        OBJECT_PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        vec![Value::str(interface), Value::str("version")],
        Duration::from_secs(25),
    );
    match reply {
        Ok(values) => Ok(values.first().and_then(Value::as_u32)),
        Err(Error::Remote { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Tells the portal which application this connection belongs to. It must come
/// before any other portal call, and fails while the desktop entry is missing, in
/// which case the portal still works but shows an anonymous program. A running
/// portal takes a moment to notice an entry `wispx setup` just wrote, so a refusal
/// is worth a few more tries.
pub fn register_app(bus: &mut Connection) -> Result<(), Error> {
    let mut attempts = 0;
    loop {
        let reply = bus.call(
            BUS_NAME,
            OBJECT_PATH,
            REGISTRY_INTERFACE,
            "Register",
            vec![Value::str(APP_ID), Value::vardict(Vec::new())],
            CALL_TIMEOUT,
        );
        attempts += 1;
        match reply {
            Err(Error::Remote { .. }) if attempts < REGISTER_ATTEMPTS => {
                thread::sleep(REGISTER_RETRY);
            }
            other => return other.map(|_| ()),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Response {
    Success(Value),
    /// The user dismissed the dialog.
    Cancelled,
    Failed(u32),
}

/// The object path a request will use, known in advance so we can subscribe
/// before making the call and cannot miss a fast answer.
pub fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{OBJECT_PATH}/request/{sender}/{token}")
}

/// Makes a portal call that answers through a `Request::Response` signal. Every
/// such method takes its options dictionary as the last argument.
pub fn request(
    bus: &mut Connection,
    interface: &str,
    member: &str,
    arguments: Vec<Value>,
    mut options: Vec<(String, Value)>,
    wait: Duration,
) -> Result<Response, Error> {
    let handle_token = token();
    let expected = request_path(bus.unique_name(), &handle_token);
    let rule = format!("type='signal',interface='{REQUEST_INTERFACE}',member='Response'");
    bus.add_match(&rule, CALL_TIMEOUT)?;

    options.push(("handle_token".to_string(), Value::str(handle_token)));
    let mut args = arguments;
    args.push(Value::vardict(options));
    let reply = bus.call(BUS_NAME, OBJECT_PATH, interface, member, args, CALL_TIMEOUT)?;
    // Very old portals pick their own handle and return it.
    let handle = reply
        .first()
        .and_then(Value::as_str)
        .map_or(expected, str::to_string);

    let deadline = Instant::now() + wait;
    let mut unrelated: Vec<Message> = Vec::new();
    let result = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match bus.next_message(remaining) {
            Ok(Some(message)) => {
                if message.is_signal(REQUEST_INTERFACE, "Response")
                    && message.path.as_deref() == Some(handle.as_str())
                {
                    break decode_response(&message);
                }
                unrelated.push(message);
            }
            Ok(None) => break Err(Error::Timeout),
            Err(error) => break Err(error),
        }
    };
    bus.requeue(unrelated);
    let _ = bus.remove_match(&rule, CALL_TIMEOUT);
    result
}

fn decode_response(message: &Message) -> Result<Response, Error> {
    let code = message.body.first().and_then(Value::as_u32);
    let results = message.body.get(1).cloned();
    match (code, results) {
        (Some(0), Some(results)) => Ok(Response::Success(results)),
        (Some(1), _) => Ok(Response::Cancelled),
        (Some(other), _) => Ok(Response::Failed(other)),
        _ => Err(Error::Protocol("malformed portal response".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_follow_the_portal_convention() {
        assert_eq!(
            request_path(":1.42", "tok"),
            "/org/freedesktop/portal/desktop/request/1_42/tok"
        );
    }

    #[test]
    fn tokens_are_unique_and_valid_path_elements() {
        let first = token();
        let second = token();
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
    }
}
