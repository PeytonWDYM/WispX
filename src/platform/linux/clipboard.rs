//! Clipboard access without focus: Wayland's data-control protocol where the
//! compositor offers it (Plasma does), otherwise the X11 selection (also XWayland).
use super::session::{Protocol, Session};
use std::{
    io::Read,
    sync::{Mutex, OnceLock, mpsc},
    time::Duration,
};
use wl_clipboard_rs::{copy, paste};

/// Larger clipboards are left alone rather than half restored.
const MAX_BACKUP: usize = 16 * 1024 * 1024;
const BACKUP_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    /// `ext`/`wlr` data control: every MIME type can be read and restored.
    DataControl,
    /// The X11 CLIPBOARD selection: plain text only.
    X11,
    Unavailable,
}
pub fn backend() -> Backend {
    static BACKEND: OnceLock<Backend> = OnceLock::new();
    *BACKEND.get_or_init(|| {
        let session = Session::detect();
        if session.protocol == Protocol::Wayland && data_control() {
            Backend::DataControl
        } else if std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty()) {
            Backend::X11
        } else {
            Backend::Unavailable
        }
    })
}
fn data_control() -> bool {
    !matches!(
        paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified),
        Err(paste::Error::MissingProtocol { .. }
            | paste::Error::WaylandConnection(_)
            | paste::Error::NoSeats)
    )
}

/// arboard serves X11 selections from a background thread while a handle lives.
fn x11() -> Result<std::sync::MutexGuard<'static, arboard::Clipboard>, String> {
    static CLIPBOARD: OnceLock<Result<Mutex<arboard::Clipboard>, String>> = OnceLock::new();
    CLIPBOARD
        .get_or_init(|| {
            arboard::Clipboard::new()
                .map(Mutex::new)
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
        .map(|m| m.lock().expect("clipboard lock"))
}

/// `transient` text exists only to be pasted, so history managers are asked to skip it.
pub fn set_text(text: &str, transient: bool) -> Result<(), String> {
    match backend() {
        Backend::DataControl => {
            let mut options = copy::Options::new();
            options.sensitive(transient);
            options
                .copy(
                    copy::Source::Bytes(text.as_bytes().into()),
                    copy::MimeType::Text,
                )
                .map_err(|e| e.to_string())
        }
        Backend::X11 => {
            use arboard::SetExtLinux;
            let mut clipboard = x11()?;
            let set = clipboard.set();
            let set = if transient {
                set.exclude_from_history()
            } else {
                set
            };
            set.text(text).map_err(|e| e.to_string())
        }
        Backend::Unavailable => Err("No clipboard is available".into()),
    }
}
pub fn text() -> Option<String> {
    match backend() {
        Backend::DataControl => {
            let (mut pipe, _) = paste::get_contents(
                paste::ClipboardType::Regular,
                paste::Seat::Unspecified,
                paste::MimeType::Text,
            )
            .ok()?;
            let mut text = String::new();
            pipe.read_to_string(&mut text).ok()?;
            Some(text)
        }
        Backend::X11 => x11().ok()?.get_text().ok(),
        Backend::Unavailable => None,
    }
}

/// The clipboard as it was before WispX borrowed it for a paste.
pub enum Backup {
    Mime(Vec<(String, Vec<u8>)>),
    Text(String),
}
/// X11 selection plumbing, never data, so it's not restored.
fn restorable(mime: &str) -> bool {
    !matches!(
        mime,
        "TARGETS" | "MULTIPLE" | "SAVE_TARGETS" | "TIMESTAMP" | "DELETE" | "INSERT_SELECTION"
    )
}
/// Snapshots every offered type, or nothing if any part is missing, huge or slow.
pub fn backup() -> Option<Backup> {
    match backend() {
        Backend::DataControl => {
            let (tx, rx) = mpsc::sync_channel(1);
            // A stalled source app must not hold up the paste, so read on a worker.
            std::thread::spawn(move || {
                let _ = tx.send(read_all());
            });
            rx.recv_timeout(BACKUP_TIMEOUT)
                .ok()
                .flatten()
                .map(Backup::Mime)
        }
        Backend::X11 => text().map(Backup::Text),
        Backend::Unavailable => None,
    }
}
fn read_all() -> Option<Vec<(String, Vec<u8>)>> {
    let types =
        paste::get_mime_types_ordered(paste::ClipboardType::Regular, paste::Seat::Unspecified)
            .ok()?;
    let mut total = 0;
    let mut entries = Vec::new();
    for mime in types.into_iter().filter(|m| restorable(m)) {
        let (pipe, _) = paste::get_contents(
            paste::ClipboardType::Regular,
            paste::Seat::Unspecified,
            paste::MimeType::Specific(&mime),
        )
        .ok()?;
        let mut bytes = Vec::new();
        pipe.take((MAX_BACKUP - total + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        total += bytes.len();
        if total > MAX_BACKUP {
            return None;
        }
        entries.push((mime, bytes));
    }
    (!entries.is_empty()).then_some(entries)
}
/// Puts the backup back. The caller checks first that the clipboard still holds
/// its transcript, so a newer clipboard is never overwritten.
pub fn restore(backup: Backup) -> Result<(), String> {
    match backup {
        Backup::Mime(entries) => {
            let sources = entries
                .into_iter()
                .map(|(mime, bytes)| copy::MimeSource {
                    source: copy::Source::Bytes(bytes.into()),
                    mime_type: copy::MimeType::Specific(mime),
                })
                .collect();
            let mut options = copy::Options::new();
            // Offer exactly the original types, not extra text aliases.
            options.omit_additional_text_mime_types(true);
            options.copy_multi(sources).map_err(|e| e.to_string())
        }
        Backup::Text(text) => set_text(&text, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_plumbing_is_not_restored() {
        assert!(!restorable("TARGETS"));
        assert!(!restorable("SAVE_TARGETS"));
        assert!(restorable("text/plain;charset=utf-8"));
        assert!(restorable("image/png"));
        assert!(restorable("x-kde-passwordManagerHint"));
    }
}
