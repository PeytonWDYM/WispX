//! A small D-Bus client over the session bus socket.
//!
//! Plasma's integration points (portals, KWin, notifications) are all D-Bus, and
//! a portal session dies with the connection that created it, so one-shot tools
//! such as `gdbus call` cannot hold a global shortcut. Implementing the part of
//! the wire format WispX needs takes a few hundred lines; a client crate would
//! add dozens of dependencies. Only `std` is used, so integration tests can
//! include this file directly.

use std::collections::VecDeque;
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// Far beyond anything WispX exchanges, and well below the protocol's limit.
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// The specification allows 32 nested arrays plus 32 structs.
const MAX_DEPTH: usize = 64;
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const FLAG_NO_REPLY: u8 = 1;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Closed,
    Timeout,
    Protocol(String),
    /// The peer answered with a D-Bus error.
    Remote {
        name: String,
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(f, "{error}"),
            Error::Closed => write!(f, "the D-Bus connection closed"),
            Error::Timeout => write!(f, "D-Bus request timed out"),
            Error::Protocol(message) => write!(f, "D-Bus protocol error: {message}"),
            Error::Remote { name, message } => write!(f, "{name}: {message}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

fn protocol<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error::Protocol(message.into()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    Int16(i16),
    Uint16(u16),
    Int32(i32),
    Uint32(u32),
    Int64(i64),
    Uint64(u64),
    Double(f64),
    Str(String),
    ObjectPath(String),
    Signature(String),
    Array { element: String, items: Vec<Value> },
    Struct(Vec<Value>),
    DictEntry(Box<Value>, Box<Value>),
    Variant(Box<Value>),
}

impl Value {
    pub fn str(text: impl Into<String>) -> Value {
        Value::Str(text.into())
    }

    pub fn path(text: impl Into<String>) -> Value {
        Value::ObjectPath(text.into())
    }

    /// An `a{sv}` dictionary, the options argument of nearly every portal call.
    pub fn vardict(entries: Vec<(String, Value)>) -> Value {
        Value::Array {
            element: "{sv}".to_string(),
            items: entries
                .into_iter()
                .map(|(key, value)| {
                    Value::DictEntry(
                        Box::new(Value::Str(key)),
                        Box::new(Value::Variant(Box::new(value))),
                    )
                })
                .collect(),
        }
    }

    pub fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".to_string(),
            Value::Bool(_) => "b".to_string(),
            Value::Int16(_) => "n".to_string(),
            Value::Uint16(_) => "q".to_string(),
            Value::Int32(_) => "i".to_string(),
            Value::Uint32(_) => "u".to_string(),
            Value::Int64(_) => "x".to_string(),
            Value::Uint64(_) => "t".to_string(),
            Value::Double(_) => "d".to_string(),
            Value::Str(_) => "s".to_string(),
            Value::ObjectPath(_) => "o".to_string(),
            Value::Signature(_) => "g".to_string(),
            Value::Array { element, .. } => format!("a{element}"),
            Value::Struct(fields) => {
                let inner: String = fields.iter().map(Value::signature).collect();
                format!("({inner})")
            }
            Value::DictEntry(key, value) => {
                format!("{{{}{}}}", key.signature(), value.signature())
            }
            Value::Variant(_) => "v".to_string(),
        }
    }

    /// The text of a string or object path, looking through variants.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) | Value::ObjectPath(text) => Some(text),
            Value::Variant(inner) => inner.as_str(),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        match self {
            Value::Uint32(number) => Some(*number),
            Value::Variant(inner) => inner.as_u32(),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        match self {
            Value::Int32(number) => Some(*number),
            Value::Variant(inner) => inner.as_i32(),
            _ => None,
        }
    }

    /// Looks up a key in an `a{sv}` dictionary, without the variant wrapper.
    pub fn lookup(&self, key: &str) -> Option<&Value> {
        let Value::Array { items, .. } = self else {
            return None;
        };
        items.iter().find_map(|item| match item {
            Value::DictEntry(k, value) if k.as_str() == Some(key) => match value.as_ref() {
                Value::Variant(inner) => Some(inner.as_ref()),
                other => Some(other),
            },
            _ => None,
        })
    }
}

fn alignment(type_code: u8) -> usize {
    match type_code {
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'a' | b's' | b'o' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

/// The length of the first complete type in `signature`.
fn single_type_len(signature: &[u8], depth: usize) -> Result<usize, Error> {
    if depth > MAX_DEPTH {
        return protocol("type nesting is too deep");
    }
    let Some(first) = signature.first() else {
        return protocol("incomplete signature");
    };
    match first {
        b'y' | b'b' | b'n' | b'q' | b'i' | b'u' | b'x' | b't' | b'd' | b's' | b'o' | b'g'
        | b'v' => Ok(1),
        b'a' => Ok(1 + single_type_len(&signature[1..], depth + 1)?),
        b'(' | b'{' => {
            let close = if *first == b'(' { b')' } else { b'}' };
            let mut at = 1;
            let mut members = 0;
            while signature.get(at).is_some_and(|byte| *byte != close) {
                at += single_type_len(&signature[at..], depth + 1)?;
                members += 1;
            }
            if signature.get(at) != Some(&close) {
                return protocol("unterminated container in signature");
            }
            if *first == b'(' && members == 0 {
                return protocol("structure with no members in signature");
            }
            if *first == b'{' && members != 2 {
                return protocol("dictionary entry without exactly a key and a value");
            }
            Ok(at + 1)
        }
        other => protocol(format!("unsupported type code {:?}", *other as char)),
    }
}

#[derive(Default)]
struct Encoder {
    out: Vec<u8>,
}

impl Encoder {
    fn pad(&mut self, align: usize) {
        while !self.out.len().is_multiple_of(align) {
            self.out.push(0);
        }
    }

    fn u32(&mut self, value: u32) {
        self.pad(4);
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn string(&mut self, text: &str) {
        self.u32(text.len() as u32);
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(0);
    }

    fn signature(&mut self, text: &str) {
        self.out.push(text.len() as u8);
        self.out.extend_from_slice(text.as_bytes());
        self.out.push(0);
    }

    fn value(&mut self, value: &Value) {
        match value {
            Value::Byte(number) => self.out.push(*number),
            Value::Bool(flag) => self.u32(u32::from(*flag)),
            Value::Int16(number) => {
                self.pad(2);
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Uint16(number) => {
                self.pad(2);
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Int32(number) => {
                self.pad(4);
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Uint32(number) => self.u32(*number),
            Value::Int64(number) => {
                self.pad(8);
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Uint64(number) => {
                self.pad(8);
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Double(number) => {
                self.pad(8);
                self.out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Str(text) | Value::ObjectPath(text) => self.string(text),
            Value::Signature(text) => self.signature(text),
            Value::Array { element, items } => {
                self.u32(0);
                let length_at = self.out.len() - 4;
                // Padding before the first element is not part of the length.
                self.pad(element.bytes().next().map_or(1, alignment));
                let start = self.out.len();
                for item in items {
                    self.value(item);
                }
                let length = (self.out.len() - start) as u32;
                self.out[length_at..length_at + 4].copy_from_slice(&length.to_le_bytes());
            }
            Value::Struct(fields) => {
                self.pad(8);
                for field in fields {
                    self.value(field);
                }
            }
            Value::DictEntry(key, entry) => {
                self.pad(8);
                self.value(key);
                self.value(entry);
            }
            Value::Variant(inner) => {
                self.signature(&inner.signature());
                self.value(inner);
            }
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    big_endian: bool,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8], big_endian: bool) -> Self {
        Decoder {
            bytes,
            at: 0,
            big_endian,
        }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(count);
        match end.and_then(|end| self.bytes.get(self.at..end)) {
            Some(slice) => {
                self.at += count;
                Ok(slice)
            }
            None => protocol("message is truncated"),
        }
    }

    fn align(&mut self, align: usize) -> Result<(), Error> {
        let padding = (align - self.at % align) % align;
        self.take(padding).map(|_| ())
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.align(N)?;
        let mut bytes = [0u8; N];
        bytes.copy_from_slice(self.take(N)?);
        if self.big_endian {
            bytes.reverse();
        }
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.fixed::<4>().map(u32::from_le_bytes)
    }

    fn string(&mut self) -> Result<String, Error> {
        let length = self.u32()? as usize;
        let text = self.take(length)?;
        if self.take(1)? != [0] {
            return protocol("string is not terminated");
        }
        std::str::from_utf8(text)
            .map(str::to_string)
            .or_else(|_| protocol("string is not UTF-8"))
    }

    fn signature(&mut self) -> Result<String, Error> {
        let length = usize::from(self.take(1)?[0]);
        let text = self.take(length)?;
        if self.take(1)? != [0] {
            return protocol("signature is not terminated");
        }
        std::str::from_utf8(text)
            .map(str::to_string)
            .or_else(|_| protocol("signature is not UTF-8"))
    }

    fn value(&mut self, signature: &[u8], depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return protocol("value nesting is too deep");
        }
        let Some(code) = signature.first() else {
            return protocol("empty type");
        };
        Ok(match code {
            b'y' => Value::Byte(self.take(1)?[0]),
            b'b' => Value::Bool(self.u32()? != 0),
            b'n' => Value::Int16(self.fixed::<2>().map(i16::from_le_bytes)?),
            b'q' => Value::Uint16(self.fixed::<2>().map(u16::from_le_bytes)?),
            b'i' => Value::Int32(self.fixed::<4>().map(i32::from_le_bytes)?),
            b'u' => Value::Uint32(self.u32()?),
            b'x' => Value::Int64(self.fixed::<8>().map(i64::from_le_bytes)?),
            b't' => Value::Uint64(self.fixed::<8>().map(u64::from_le_bytes)?),
            b'd' => Value::Double(self.fixed::<8>().map(f64::from_le_bytes)?),
            b's' => Value::Str(self.string()?),
            b'o' => Value::ObjectPath(self.string()?),
            b'g' => Value::Signature(self.signature()?),
            b'a' => {
                let element_len = single_type_len(&signature[1..], depth)?;
                let element = &signature[1..1 + element_len];
                let length = self.u32()? as usize;
                self.align(alignment(element[0]))?;
                let end = self
                    .at
                    .checked_add(length)
                    .filter(|end| *end <= self.bytes.len());
                let Some(end) = end else {
                    return protocol("array overruns the message");
                };
                let mut items = Vec::new();
                while self.at < end {
                    let before = self.at;
                    items.push(self.value(element, depth + 1)?);
                    // Every pass must use up input, or this would never end.
                    if self.at == before {
                        return protocol("array element takes up no space");
                    }
                }
                if self.at != end {
                    return protocol("array length does not match its elements");
                }
                Value::Array {
                    element: String::from_utf8_lossy(element).into_owned(),
                    items,
                }
            }
            b'(' => {
                self.align(8)?;
                let total = single_type_len(signature, depth)?;
                let mut fields = Vec::new();
                let mut at = 1;
                while at < total - 1 {
                    let length = single_type_len(&signature[at..], depth)?;
                    fields.push(self.value(&signature[at..at + length], depth + 1)?);
                    at += length;
                }
                Value::Struct(fields)
            }
            b'{' => {
                self.align(8)?;
                let total = single_type_len(signature, depth)?;
                let key_len = single_type_len(&signature[1..], depth)?;
                let key = self.value(&signature[1..1 + key_len], depth + 1)?;
                let entry = self.value(&signature[1 + key_len..total - 1], depth + 1)?;
                Value::DictEntry(Box::new(key), Box::new(entry))
            }
            b'v' => {
                let inner = self.signature()?;
                let bytes = inner.as_bytes();
                if single_type_len(bytes, depth)? != bytes.len() {
                    return protocol("variant must hold exactly one value");
                }
                Value::Variant(Box::new(self.value(bytes, depth + 1)?))
            }
            other => return protocol(format!("unsupported type code {:?}", *other as char)),
        })
    }

    fn values(&mut self, signature: &str) -> Result<Vec<Value>, Error> {
        let bytes = signature.as_bytes();
        let mut at = 0;
        let mut out = Vec::new();
        while at < bytes.len() {
            let length = single_type_len(&bytes[at..], 0)?;
            out.push(self.value(&bytes[at..at + length], 0)?);
            at += length;
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub kind: Kind,
    pub serial: u32,
    pub no_reply: bool,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub body: Vec<Value>,
}

impl Message {
    fn new(kind: Kind) -> Self {
        Message {
            kind,
            serial: 0,
            no_reply: false,
            path: None,
            interface: None,
            member: None,
            error_name: None,
            reply_serial: None,
            destination: None,
            sender: None,
            body: Vec::new(),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn for_tests(interface: &str, member: &str, body: Vec<Value>) -> Message {
        let mut message = Message::new(Kind::Signal);
        message.interface = Some(interface.to_string());
        message.member = Some(member.to_string());
        message.body = body;
        message
    }

    pub fn is_signal(&self, interface: &str, member: &str) -> bool {
        self.kind == Kind::Signal
            && self.interface.as_deref() == Some(interface)
            && self.member.as_deref() == Some(member)
    }

    fn encode(&self) -> Vec<u8> {
        let mut body = Encoder::default();
        for value in &self.body {
            body.value(value);
        }
        let signature: String = self.body.iter().map(Value::signature).collect();

        let field = |code: u8, value: Value| {
            Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(value))])
        };
        let mut fields = Vec::new();
        if let Some(path) = &self.path {
            fields.push(field(1, Value::path(path.as_str())));
        }
        if let Some(interface) = &self.interface {
            fields.push(field(2, Value::str(interface.as_str())));
        }
        if let Some(member) = &self.member {
            fields.push(field(3, Value::str(member.as_str())));
        }
        if let Some(name) = &self.error_name {
            fields.push(field(4, Value::str(name.as_str())));
        }
        if let Some(serial) = self.reply_serial {
            fields.push(field(5, Value::Uint32(serial)));
        }
        if let Some(destination) = &self.destination {
            fields.push(field(6, Value::str(destination.as_str())));
        }
        if !signature.is_empty() {
            fields.push(field(8, Value::Signature(signature)));
        }

        let mut out = Encoder::default();
        out.out.push(b'l');
        out.out.push(match self.kind {
            Kind::MethodCall => 1,
            Kind::MethodReturn => 2,
            Kind::Error => 3,
            Kind::Signal => 4,
        });
        out.out.push(if self.no_reply { FLAG_NO_REPLY } else { 0 });
        out.out.push(1);
        out.u32(body.out.len() as u32);
        out.u32(self.serial);
        out.value(&Value::Array {
            element: "(yv)".to_string(),
            items: fields,
        });
        out.pad(8);
        out.out.extend_from_slice(&body.out);
        out.out
    }
}

/// The total size of the message at the front of `bytes`, once enough has
/// arrived to tell.
fn frame_len(bytes: &[u8]) -> Result<Option<usize>, Error> {
    if bytes.len() < 16 {
        return Ok(None);
    }
    let big_endian = match bytes[0] {
        b'l' => false,
        b'B' => true,
        _ => return protocol("invalid byte order"),
    };
    let word = |at: usize| {
        let raw = [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]];
        if big_endian {
            u32::from_be_bytes(raw)
        } else {
            u32::from_le_bytes(raw)
        }
    };
    let body = word(4) as usize;
    let fields = word(12) as usize;
    let header = 16usize.saturating_add(fields).saturating_add(7) & !7;
    let total = header.saturating_add(body);
    if total > MAX_MESSAGE {
        return protocol("message is too large");
    }
    Ok(Some(total))
}

fn decode_message(frame: &[u8]) -> Result<Message, Error> {
    let big_endian = frame[0] == b'B';
    let kind = match frame[1] {
        1 => Kind::MethodCall,
        2 => Kind::MethodReturn,
        3 => Kind::Error,
        4 => Kind::Signal,
        other => return protocol(format!("unknown message type {other}")),
    };
    if frame[3] != 1 {
        return protocol("unsupported protocol version");
    }
    let mut message = Message::new(kind);
    message.no_reply = frame[2] & FLAG_NO_REPLY != 0;

    let mut header = Decoder::new(frame, big_endian);
    header.at = 8;
    message.serial = header.u32()?;
    let fields = header.value(b"a(yv)", 0)?;
    let mut signature = String::new();
    if let Value::Array { items, .. } = fields {
        for item in items {
            let Value::Struct(parts) = item else { continue };
            let [Value::Byte(code), value] = parts.as_slice() else {
                continue;
            };
            let text = || value.as_str().map(str::to_string);
            match code {
                1 => message.path = text(),
                2 => message.interface = text(),
                3 => message.member = text(),
                4 => message.error_name = text(),
                5 => message.reply_serial = value.as_u32(),
                6 => message.destination = text(),
                7 => message.sender = text(),
                8 => {
                    if let Value::Variant(inner) = value
                        && let Value::Signature(text) = inner.as_ref()
                    {
                        signature.clone_from(text);
                    }
                }
                _ => {}
            }
        }
    }
    header.align(8)?;
    let body_start = header.at;
    let mut body = Decoder::new(&frame[body_start..], big_endian);
    message.body = body.values(&signature)?;
    Ok(message)
}

/// Where the session bus listens, from `DBUS_SESSION_BUS_ADDRESS` or the
/// standard per-user socket.
fn session_address() -> Result<String, Error> {
    if let Some(address) = env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        return Ok(address.to_string_lossy().into_owned());
    }
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        let socket = Path::new(&runtime).join("bus");
        if socket.exists() {
            return Ok(format!("unix:path={}", socket.display()));
        }
    }
    protocol("no session bus: DBUS_SESSION_BUS_ADDRESS is not set")
}

fn unescape_address(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some(hex) = value.get(at + 1..at + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, PartialEq, Eq)]
enum Endpoint {
    Path(String),
    Abstract(String),
}

/// Reads the first `unix:` transport in a bus address list.
fn parse_address(address: &str) -> Option<Endpoint> {
    address.split(';').find_map(|entry| {
        let options = entry.strip_prefix("unix:")?;
        options.split(',').find_map(|option| {
            let (key, value) = option.split_once('=')?;
            match key {
                "path" => Some(Endpoint::Path(unescape_address(value))),
                "abstract" => Some(Endpoint::Abstract(unescape_address(value))),
                _ => None,
            }
        })
    })
}

pub struct Connection {
    stream: UnixStream,
    unread: Vec<u8>,
    queue: VecDeque<Message>,
    next_serial: u32,
    unique_name: String,
}

impl Connection {
    pub fn session() -> Result<Connection, Error> {
        Self::connect(&session_address()?)
    }

    pub fn connect(address: &str) -> Result<Connection, Error> {
        let stream = match parse_address(address) {
            Some(Endpoint::Path(path)) => UnixStream::connect(path)?,
            Some(Endpoint::Abstract(name)) => {
                let socket = SocketAddr::from_abstract_name(name.as_bytes())?;
                UnixStream::connect_addr(&socket)?
            }
            None => return protocol("the bus address has no usable unix transport"),
        };
        Self::over_stream(stream)
    }

    fn over_stream(stream: UnixStream) -> Result<Connection, Error> {
        authenticate(&stream)?;
        let mut connection = Connection {
            stream,
            unread: Vec::new(),
            queue: VecDeque::new(),
            next_serial: 1,
            unique_name: String::new(),
        };
        let reply = connection.call(
            BUS_NAME,
            BUS_PATH,
            BUS_NAME,
            "Hello",
            Vec::new(),
            AUTH_TIMEOUT,
        )?;
        connection.unique_name = match reply.first().and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => return protocol("the bus did not assign a name"),
        };
        Ok(connection)
    }

    /// The name other peers use to reach this connection.
    pub fn unique_name(&self) -> &str {
        &self.unique_name
    }

    fn send(&mut self, mut message: Message) -> Result<u32, Error> {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.checked_add(1).unwrap_or(1);
        message.serial = serial;
        self.stream.set_write_timeout(Some(AUTH_TIMEOUT))?;
        self.stream.write_all(&message.encode())?;
        Ok(serial)
    }

    /// Calls a method and waits for its answer, keeping anything else that
    /// arrives meanwhile for `next_message`.
    pub fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> Result<Vec<Value>, Error> {
        let mut message = Message::new(Kind::MethodCall);
        message.destination = Some(destination.to_string());
        message.path = Some(path.to_string());
        // Optional on method calls, and KWin's callDBus sends none.
        message.interface = (!interface.is_empty()).then(|| interface.to_string());
        message.member = Some(member.to_string());
        message.body = args;
        let serial = self.send(message)?;

        let deadline = Instant::now() + timeout;
        loop {
            let Some(reply) = self.read_message(deadline)? else {
                return Err(Error::Timeout);
            };
            if reply.reply_serial != Some(serial) {
                self.queue.push_back(reply);
                continue;
            }
            return match reply.kind {
                Kind::MethodReturn => Ok(reply.body),
                Kind::Error => Err(Error::Remote {
                    name: reply.error_name.unwrap_or_default(),
                    message: reply
                        .body
                        .first()
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                _ => protocol("a call was answered by something other than a reply"),
            };
        }
    }

    /// Subscribes this connection to messages matching a bus match rule.
    pub fn add_match(&mut self, rule: &str, timeout: Duration) -> Result<(), Error> {
        self.call(
            BUS_NAME,
            BUS_PATH,
            BUS_NAME,
            "AddMatch",
            vec![Value::str(rule)],
            timeout,
        )
        .map(|_| ())
    }

    pub fn remove_match(&mut self, rule: &str, timeout: Duration) -> Result<(), Error> {
        self.call(
            BUS_NAME,
            BUS_PATH,
            BUS_NAME,
            "RemoveMatch",
            vec![Value::str(rule)],
            timeout,
        )
        .map(|_| ())
    }

    /// Whether anything currently owns a well-known bus name.
    pub fn name_has_owner(&mut self, name: &str, timeout: Duration) -> Result<bool, Error> {
        let reply = self.call(
            BUS_NAME,
            BUS_PATH,
            BUS_NAME,
            "NameHasOwner",
            vec![Value::str(name)],
            timeout,
        )?;
        Ok(matches!(reply.first(), Some(Value::Bool(true))))
    }

    /// The next message not already consumed by a call, or `None` on timeout.
    pub fn next_message(&mut self, timeout: Duration) -> Result<Option<Message>, Error> {
        if let Some(message) = self.queue.pop_front() {
            return Ok(Some(message));
        }
        self.read_message(Instant::now() + timeout)
    }

    /// Returns messages a helper looked at but does not handle.
    pub fn requeue(&mut self, messages: Vec<Message>) {
        for message in messages.into_iter().rev() {
            self.queue.push_front(message);
        }
    }

    /// Answers a method call a peer made to us, unless it asked for no reply.
    pub fn reply(&mut self, call: &Message, args: Vec<Value>) -> Result<(), Error> {
        if call.no_reply {
            return Ok(());
        }
        let mut message = Message::new(Kind::MethodReturn);
        message.reply_serial = Some(call.serial);
        message.destination = call.sender.clone();
        message.body = args;
        self.send(message).map(|_| ())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn reply_error(&mut self, call: &Message, name: &str, text: &str) -> Result<(), Error> {
        let mut message = Message::new(Kind::Error);
        message.reply_serial = Some(call.serial);
        message.destination = call.sender.clone();
        message.error_name = Some(name.to_string());
        message.body = vec![Value::str(text)];
        self.send(message).map(|_| ())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn emit_signal(
        &mut self,
        path: &str,
        interface: &str,
        member: &str,
        args: Vec<Value>,
    ) -> Result<(), Error> {
        let mut message = Message::new(Kind::Signal);
        message.path = Some(path.to_string());
        message.interface = Some(interface.to_string());
        message.member = Some(member.to_string());
        message.body = args;
        self.send(message).map(|_| ())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn request_name(&mut self, name: &str, timeout: Duration) -> Result<(), Error> {
        let reply = self.call(
            BUS_NAME,
            BUS_PATH,
            BUS_NAME,
            "RequestName",
            vec![Value::str(name), Value::Uint32(4)],
            timeout,
        )?;
        match reply.first().and_then(Value::as_u32) {
            Some(1) => Ok(()),
            other => protocol(format!("could not own {name}: {other:?}")),
        }
    }

    fn read_message(&mut self, deadline: Instant) -> Result<Option<Message>, Error> {
        loop {
            if let Some(total) = frame_len(&self.unread)?
                && self.unread.len() >= total
            {
                let message = decode_message(&self.unread[..total]);
                self.unread.drain(..total);
                return message.map(Some);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            // Out of time still means looking once at what has already arrived,
            // which is the whole point of polling with a zero timeout. A socket
            // read timeout of zero is an error, so that look is nonblocking.
            let polling = remaining.is_zero();
            if polling {
                self.stream.set_nonblocking(true)?;
            } else {
                self.stream.set_read_timeout(Some(remaining))?;
            }
            let mut buffer = [0u8; 4096];
            let read = self.stream.read(&mut buffer);
            if polling {
                self.stream.set_nonblocking(false)?;
            }
            match read {
                Ok(0) => return Err(Error::Closed),
                Ok(count) => self.unread.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(Error::Io(error)),
            }
        }
    }
}

/// SASL EXTERNAL: the bus already knows our uid from the socket credentials.
fn authenticate(stream: &UnixStream) -> Result<(), Error> {
    let uid = fs::metadata("/proc/self")?.uid();
    let hex: String = uid
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    stream.set_read_timeout(Some(AUTH_TIMEOUT))?;
    stream.set_write_timeout(Some(AUTH_TIMEOUT))?;
    let mut writer = stream;
    writer.write_all(b"\0")?;
    writer.write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())?;

    let mut line = Vec::new();
    let mut reader = stream;
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        if line.len() > 512 {
            return protocol("authentication reply is too long");
        }
        if reader.read(&mut byte)? == 0 {
            return Err(Error::Closed);
        }
        line.push(byte[0]);
    }
    if !line.starts_with(b"OK ") {
        return protocol("the bus rejected authentication");
    }
    writer.write_all(b"BEGIN\r\n")?;
    Ok(())
}

/// Starts a `dbus-daemon` only tests can reach, listening on `<dir>/bus`, or
/// `None` where it is not installed. The configuration names no service
/// directories, so calling an unowned name fails instead of starting a real
/// desktop service, and the cleared environment keeps the real session out of
/// reach of anything it might still run.
#[cfg(test)]
pub fn spawn_test_bus(dir: &std::path::Path) -> Option<(std::process::Child, String)> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let config = dir.join("bus.conf");
    let text = format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}/bus</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        dir.display()
    );
    std::fs::write(&config, text).unwrap();
    let spawned = Command::new("dbus-daemon")
        .env_clear()
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        eprintln!("skipped: dbus-daemon is not installed");
        return None;
    };
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    Some((child, line.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    const SECOND: Duration = Duration::from_secs(1);

    fn encode(value: &Value) -> Vec<u8> {
        let mut out = Encoder::default();
        out.value(value);
        out.out
    }

    fn round_trip(value: Value) {
        let bytes = encode(&value);
        let decoded = Decoder::new(&bytes, false)
            .values(&value.signature())
            .unwrap();
        assert_eq!(decoded, vec![value]);
    }

    #[test]
    fn values_round_trip_with_their_signatures() {
        for value in [
            Value::Byte(7),
            Value::Bool(true),
            Value::Int16(-3),
            Value::Uint16(9),
            Value::Int32(-70_000),
            Value::Uint32(70_000),
            Value::Int64(-5),
            Value::Uint64(u64::MAX),
            Value::Double(1.5),
            Value::str("héllo"),
            Value::path("/a/b"),
            Value::Signature("a{sv}".into()),
            Value::vardict(vec![
                ("handle_token".into(), Value::str("t1")),
                ("types".into(), Value::Uint32(1)),
                ("nested".into(), Value::vardict(Vec::new())),
            ]),
            Value::Struct(vec![Value::Byte(1), Value::Uint64(2), Value::str("x")]),
            Value::Array {
                element: "(sa{sv})".into(),
                items: vec![Value::Struct(vec![
                    Value::str("dictate"),
                    Value::vardict(vec![("description".into(), Value::str("Hold"))]),
                ])],
            },
            Value::Array {
                element: "(sa{sv})".into(),
                items: Vec::new(),
            },
        ] {
            round_trip(value);
        }
    }

    #[test]
    fn wire_layout_follows_the_specification() {
        // An empty a{sv}: the length excludes padding before the first element.
        assert_eq!(
            encode(&Value::vardict(Vec::new())),
            [0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(encode(&Value::str("ab")), [2, 0, 0, 0, b'a', b'b', 0]);
        assert_eq!(
            encode(&Value::Variant(Box::new(Value::Uint32(5)))),
            [1, b'u', 0, 0, 5, 0, 0, 0]
        );
        let array = Value::Array {
            element: "t".into(),
            items: vec![Value::Uint64(1)],
        };
        assert_eq!(
            encode(&array),
            [8, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn message_framing_round_trips_and_pads_the_header() {
        let mut message = Message::new(Kind::Signal);
        message.serial = 9;
        message.path = Some("/p".into());
        message.interface = Some("a.b".into());
        message.member = Some("M".into());
        message.body = vec![Value::str("hello"), Value::Uint32(3)];
        let bytes = message.encode();
        assert_eq!(frame_len(&bytes).unwrap(), Some(bytes.len()));

        let decoded = decode_message(&bytes).unwrap();
        assert!(decoded.is_signal("a.b", "M"));
        assert_eq!(decoded.serial, 9);
        assert_eq!(decoded.path.as_deref(), Some("/p"));
        assert_eq!(decoded.body, message.body);
    }

    #[test]
    fn big_endian_peers_are_understood() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"B\x04\0\x01");
        let body: [u8; 8] = [0, 0, 0, 2, b'h', b'i', 0, 0];
        bytes.extend_from_slice(&7u32.to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes());
        // One field: signature "s".
        let fields: [u8; 8] = [8, 1, b'g', 0, 1, b's', 0, 0];
        // The array's length leaves out the padding that ends the header.
        bytes.extend_from_slice(&7u32.to_be_bytes());
        bytes.extend_from_slice(&fields);
        bytes.extend_from_slice(&body[..7]);
        let message = decode_message(&bytes).unwrap();
        assert_eq!(message.serial, 5);
        assert_eq!(message.body, [Value::str("hi")]);
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        let mut good = Message::new(Kind::Signal);
        good.body = vec![Value::vardict(vec![("k".into(), Value::Uint32(1))])];
        let bytes = good.encode();
        for cut in 16..bytes.len() {
            assert!(decode_message(&bytes[..cut]).is_err(), "cut at {cut}");
        }

        let mut oversized = vec![b'l', 4, 0, 1];
        oversized.extend_from_slice(&u32::MAX.to_le_bytes());
        oversized.extend_from_slice(&[0; 8]);
        assert!(frame_len(&oversized).is_err());
        assert!(frame_len(b"Xbad-endian-marker").is_err());

        assert!(Decoder::new(&[0; 8], false).values("h").is_err());
        assert!(Decoder::new(&[0; 8], false).values("(u").is_err());
        assert!(Decoder::new(&[0; 8], false).values("{u}").is_err());

        // Arrays whose claimed length exceeds the message.
        assert!(
            Decoder::new(&[255, 255, 255, 255], false)
                .values("ay")
                .is_err()
        );

        // Nesting past the limit must not exhaust the stack.
        let deep = format!("{}y", "a".repeat(5000));
        assert!(Decoder::new(&[0; 8], false).values(&deep).is_err());
        let mut variants = Vec::new();
        for _ in 0..5000 {
            variants.extend_from_slice(&[1, b'v', 0]);
        }
        assert!(Decoder::new(&variants, false).values("v").is_err());
    }

    /// A signal whose body has the given signature and bytes.
    fn signal_frame(signature: &str, body: &[u8]) -> Vec<u8> {
        let mut fields = vec![8, 1, b'g', 0, signature.len() as u8];
        fields.extend_from_slice(signature.as_bytes());
        fields.push(0);

        let mut frame = vec![b'l', 4, 0, 1];
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        frame.extend_from_slice(&fields);
        frame.resize(frame.len().next_multiple_of(8), 0);
        frame.extend_from_slice(body);
        frame
    }

    #[test]
    fn empty_containers_on_the_wire_are_protocol_errors() {
        // Eight bytes of "elements" that would each take up no space, which used
        // to allocate empty structures forever.
        let body = [8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for signature in ["a()", "a(())", "aa()", "a(y())"] {
            let frame = signal_frame(signature, &body);
            assert_eq!(frame_len(&frame).unwrap(), Some(frame.len()));
            assert!(
                matches!(decode_message(&frame), Err(Error::Protocol(_))),
                "{signature}"
            );
        }

        // The same thing hidden inside a variant.
        let variant = [3, b'a', b'(', b')', 0, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0];
        let frame = signal_frame("v", &variant);
        assert!(matches!(decode_message(&frame), Err(Error::Protocol(_))));

        for signature in ["()", "a{}", "a{s}", "a{sss}"] {
            assert!(
                Decoder::new(&body, false).values(signature).is_err(),
                "{signature}"
            );
        }
    }

    #[test]
    fn dictionaries_are_searchable() {
        let dict = Value::vardict(vec![
            ("session_handle".into(), Value::str("/s/1")),
            ("code".into(), Value::Uint32(2)),
        ]);
        assert_eq!(
            dict.lookup("session_handle").and_then(Value::as_str),
            Some("/s/1")
        );
        assert_eq!(dict.lookup("code").and_then(Value::as_u32), Some(2));
        assert_eq!(dict.lookup("missing"), None);
        assert_eq!(Value::Byte(1).lookup("x"), None);
    }

    #[test]
    fn bus_addresses_parse() {
        assert_eq!(
            parse_address("unix:path=/run/user/1000/bus"),
            Some(Endpoint::Path("/run/user/1000/bus".into()))
        );
        assert_eq!(
            parse_address("unix:abstract=/tmp/dbus-x,guid=abc"),
            Some(Endpoint::Abstract("/tmp/dbus-x".into()))
        );
        assert_eq!(
            parse_address("tcp:host=a,port=1;unix:guid=g,path=/a%20b"),
            Some(Endpoint::Path("/a b".into()))
        );
        assert_eq!(parse_address("tcp:host=a,port=1"), None);
        assert_eq!(parse_address(""), None);
    }

    #[test]
    fn a_rejected_login_is_reported() {
        let (client, server) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut reader = BufReader::new(&server);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.starts_with("\0AUTH EXTERNAL "), "{line:?}");
            (&server).write_all(b"REJECTED EXTERNAL\r\n").unwrap();
        });
        let error = Connection::over_stream(client).err().unwrap();
        assert!(error.to_string().contains("rejected"), "{error}");
    }

    /// A private `dbus-daemon`, so these tests exercise real bus routing and
    /// real authentication without touching the desktop's session bus.
    struct PrivateBus {
        child: Child,
        address: String,
        dir: std::path::PathBuf,
    }

    static NEXT_BUS: AtomicU64 = AtomicU64::new(0);

    impl PrivateBus {
        fn start() -> Option<PrivateBus> {
            let dir = std::env::temp_dir().join(format!(
                "wispx-bus-{}-{}",
                std::process::id(),
                NEXT_BUS.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&dir).unwrap();
            let Some((child, address)) = super::spawn_test_bus(&dir) else {
                let _ = fs::remove_dir_all(&dir);
                return None;
            };
            Some(PrivateBus {
                child,
                address,
                dir,
            })
        }
    }

    #[test]
    fn polling_sees_what_already_arrived_and_leaves_the_socket_blocking() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let mut client = Connection::connect(&bus.address).unwrap();
        let mut service = Connection::connect(&bus.address).unwrap();
        client
            .add_match("type='signal',interface='test.wispx.Poll'", SECOND * 5)
            .unwrap();
        // The bus's greeting comes first; after that a poll finds nothing, at once.
        while client.next_message(Duration::ZERO).unwrap().is_some() {}
        let started = Instant::now();
        assert!(client.next_message(Duration::ZERO).unwrap().is_none());
        assert!(started.elapsed() < SECOND);

        service
            .emit_signal("/test", "test.wispx.Poll", "Closed", Vec::new())
            .unwrap();
        let deadline = Instant::now() + SECOND * 5;
        let signal = loop {
            match client.next_message(Duration::ZERO).unwrap() {
                Some(message) if message.is_signal("test.wispx.Poll", "Closed") => break message,
                Some(_) => {}
                None => {
                    assert!(Instant::now() < deadline, "the poll never saw the signal");
                    thread::sleep(Duration::from_millis(5));
                }
            }
        };
        assert_eq!(signal.member.as_deref(), Some("Closed"));

        // Ordinary calls, which wait for their reply, still work afterwards.
        assert!(
            client
                .name_has_owner("org.freedesktop.DBus", SECOND * 5)
                .unwrap()
        );
    }

    /// With the session configuration a call to an unowned name starts the real
    /// service, which is how a test once started the desktop's real portal.
    #[test]
    fn test_buses_cannot_start_any_service() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let mut client = Connection::connect(&bus.address).unwrap();
        let names = client
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ListActivatableNames",
                Vec::new(),
                SECOND * 5,
            )
            .unwrap();
        let Value::Array { items, .. } = &names[0] else {
            panic!("unexpected reply: {names:?}");
        };
        let names: Vec<_> = items.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, ["org.freedesktop.DBus"]);

        let error = client
            .call(
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.DBus.Properties",
                "Get",
                vec![
                    Value::str("org.freedesktop.portal.GlobalShortcuts"),
                    Value::str("version"),
                ],
                SECOND * 5,
            )
            .unwrap_err();
        assert!(error.to_string().contains("ServiceUnknown"), "{error}");
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn real_bus_authenticates_routes_calls_and_delivers_signals() {
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let mut service = Connection::connect(&bus.address).unwrap();
        assert!(service.unique_name().starts_with(':'));
        service.request_name("test.wispx.Echo", SECOND * 5).unwrap();

        let server = thread::spawn(move || {
            let mut answered = 0;
            while answered < 2 {
                let Some(call) = service.next_message(SECOND * 5).unwrap() else {
                    panic!("no call arrived");
                };
                if call.kind != Kind::MethodCall {
                    continue;
                }
                match call.member.as_deref() {
                    Some("Echo") => service.reply(&call, call.body.clone()).unwrap(),
                    _ => service
                        .reply_error(&call, "test.wispx.Nope", "not here")
                        .unwrap(),
                }
                answered += 1;
            }
            service
                .emit_signal("/test", "test.wispx.Echo", "Ping", vec![Value::Uint32(5)])
                .unwrap();
            // Keep the connection open until the client has seen the signal.
            let _ = service.next_message(SECOND * 2);
        });

        let mut client = Connection::connect(&bus.address).unwrap();
        client
            .add_match(
                "type='signal',interface='test.wispx.Echo',member='Ping'",
                SECOND * 5,
            )
            .unwrap();
        let args = vec![
            Value::str("hello"),
            Value::vardict(vec![("n".into(), Value::Uint32(1))]),
            Value::Array {
                element: "(sa{sv})".into(),
                items: Vec::new(),
            },
        ];
        let echoed = client
            .call(
                "test.wispx.Echo",
                "/test",
                "test.wispx.Echo",
                "Echo",
                args.clone(),
                SECOND * 5,
            )
            .unwrap();
        assert_eq!(echoed, args);

        let error = client
            .call(
                "test.wispx.Echo",
                "/test",
                "test.wispx.Echo",
                "Other",
                Vec::new(),
                SECOND * 5,
            )
            .unwrap_err();
        assert!(error.to_string().contains("test.wispx.Nope"), "{error}");

        let signal = loop {
            let message = client.next_message(SECOND * 5).unwrap().expect("signal");
            if message.is_signal("test.wispx.Echo", "Ping") {
                break message;
            }
        };
        assert_eq!(signal.body, [Value::Uint32(5)]);
        assert!(client.name_has_owner("test.wispx.Echo", SECOND).unwrap());
        assert!(!client.name_has_owner("test.wispx.None", SECOND).unwrap());
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn independent_clients_can_read_what_we_marshal() {
        // dbus-send is libdbus, a different implementation than this module.
        let Some(bus) = PrivateBus::start() else {
            return;
        };
        let mut service = Connection::connect(&bus.address).unwrap();
        service
            .request_name("test.wispx.Interop", SECOND * 5)
            .unwrap();
        let reply = vec![
            Value::Uint32(0),
            Value::vardict(vec![
                ("session_handle".into(), Value::str("/session/one")),
                ("n".into(), Value::Uint64(42)),
            ]),
        ];
        let server = thread::spawn(move || {
            loop {
                let call = service.next_message(SECOND * 5).unwrap().expect("a call");
                if call.kind == Kind::MethodCall && call.member.as_deref() == Some("Go") {
                    assert_eq!(call.body, [Value::str("from libdbus"), Value::Uint32(7)]);
                    service.reply(&call, reply).unwrap();
                    break;
                }
            }
            thread::sleep(Duration::from_millis(100));
        });
        let output = Command::new("dbus-send")
            .arg(format!("--bus={}", bus.address))
            .args([
                "--print-reply",
                "--dest=test.wispx.Interop",
                "/x",
                "test.wispx.Interop.Go",
                "string:from libdbus",
                "uint32:7",
            ])
            .output();
        let Ok(output) = output else {
            eprintln!("skipped: dbus-send is not installed");
            return;
        };
        server.join().unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{text}");
        for expected in ["uint32 0", "session_handle", "/session/one", "uint64 42"] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
    }
}
