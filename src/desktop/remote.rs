//! Pressing the paste shortcut through the XDG `RemoteDesktop` portal: the only
//! sanctioned way for a Wayland program to send a key to another window.
//!
//! The session asks for the keyboard alone (no pointer, no screen). The desktop
//! asks the user once, during `wispx paste-setup`, and hands back a restore token
//! so later daemon starts skip the dialog. Plasma announces a remote-control
//! session when one starts, so a single session stays open for the daemon's
//! lifetime instead of one per paste, and dictation itself never opens a dialog.

use super::kwin::{self, FocusInfo};
use super::paste::{Injector, Keys};
use crate::dbus::{self, Connection, Value};
use crate::paths;
use crate::portal::{self, Response};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const DEVICE_KEYBOARD: u32 = 1;
/// Keep the permission until the user revokes it.
const PERSIST_UNTIL_REVOKED: u32 = 2;
const KEY_PRESSED: u32 = 1;
const KEY_RELEASED: u32 = 0;
const CALL_WAIT: Duration = Duration::from_secs(30);
/// A daemon start with a good token shows no dialog, but an expired token does.
const RESTORE_WAIT: Duration = Duration::from_secs(120);
const SETUP_WAIT: Duration = Duration::from_secs(280);
const KEY_GAP: Duration = Duration::from_millis(8);
const KEY_TIMEOUT: Duration = Duration::from_secs(2);

static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

pub fn token_path() -> Result<PathBuf, String> {
    Ok(paths::data_dir()?.join("remote-desktop-token"))
}

fn read_token(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

struct Session {
    bus: Connection,
    handle: String,
}

impl Session {
    fn press(&mut self, keysym: i32, state: u32) -> Result<(), dbus::Error> {
        self.bus
            .call(
                portal::BUS_NAME,
                portal::OBJECT_PATH,
                INTERFACE,
                "NotifyKeyboardKeysym",
                vec![
                    Value::path(self.handle.as_str()),
                    Value::vardict(Vec::new()),
                    Value::Int32(keysym),
                    Value::Uint32(state),
                ],
                KEY_TIMEOUT,
            )
            .map(|_| ())
    }

    fn closed(&mut self) -> Result<bool, dbus::Error> {
        while let Some(message) = self.bus.next_message(Duration::ZERO)? {
            if message.is_signal(portal::SESSION_INTERFACE, "Closed") {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Opens a keyboard session. `Ok(None)` means the user or the portal said no.
fn open(
    token_file: &Path,
    restore: Option<String>,
    wait: Duration,
) -> Result<Option<Session>, String> {
    let describe = |error: dbus::Error| error.to_string();
    let mut bus = Connection::session().map_err(describe)?;
    if portal::interface_version(&mut bus, INTERFACE)
        .map_err(describe)?
        .is_none()
    {
        return Err("this desktop's portal has no RemoteDesktop interface".to_string());
    }
    // Without the desktop entry the portal still works, under a generic name.
    let _ = portal::register_app(&mut bus);

    let session_token = format!(
        "wispx_remote_{}_{}",
        std::process::id(),
        NEXT_SESSION.fetch_add(1, Ordering::SeqCst)
    );
    let created = portal::request(
        &mut bus,
        INTERFACE,
        "CreateSession",
        Vec::new(),
        vec![(
            "session_handle_token".to_string(),
            Value::str(session_token),
        )],
        CALL_WAIT,
    )
    .map_err(describe)?;
    let Response::Success(results) = created else {
        return Ok(None);
    };
    let Some(handle) = results
        .lookup("session_handle")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err("the portal returned no session".to_string());
    };

    let mut options = vec![
        ("types".to_string(), Value::Uint32(DEVICE_KEYBOARD)),
        (
            "persist_mode".to_string(),
            Value::Uint32(PERSIST_UNTIL_REVOKED),
        ),
    ];
    if let Some(token) = restore {
        options.push(("restore_token".to_string(), Value::str(token)));
    }
    let selected = portal::request(
        &mut bus,
        INTERFACE,
        "SelectDevices",
        vec![Value::path(handle.as_str())],
        options,
        CALL_WAIT,
    )
    .map_err(describe)?;
    if !matches!(selected, Response::Success(_)) {
        return Ok(None);
    }

    bus.add_match(
        &format!(
            "type='signal',sender='{}',interface='{}',path='{handle}'",
            portal::BUS_NAME,
            portal::SESSION_INTERFACE
        ),
        CALL_WAIT,
    )
    .map_err(describe)?;

    let started = portal::request(
        &mut bus,
        INTERFACE,
        "Start",
        vec![Value::path(handle.as_str()), Value::str("")],
        Vec::new(),
        wait,
    )
    .map_err(describe)?;
    let Response::Success(results) = started else {
        return Ok(None);
    };
    let devices = results
        .lookup("devices")
        .and_then(Value::as_u32)
        .unwrap_or(0);
    if devices & DEVICE_KEYBOARD == 0 {
        return Ok(None);
    }
    // The portal may issue a new token each start, and the old one then stops
    // working. KDE currently keeps the same one, but that is not promised.
    if let Some(token) = results.lookup("restore_token").and_then(Value::as_str) {
        paths::atomic_write(token_file, token.as_bytes())?;
    }
    Ok(Some(Session { bus, handle }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupOutcome {
    Granted,
    Declined,
}

/// The paste path on Plasma: KWin for focus, the portal for keys.
pub struct PlasmaInjector {
    runtime: PathBuf,
    token_file: PathBuf,
    session: Mutex<Option<Session>>,
    reconnecting: AtomicBool,
}

impl PlasmaInjector {
    pub fn new(runtime: PathBuf, token_file: PathBuf) -> Arc<Self> {
        Arc::new(PlasmaInjector {
            runtime,
            token_file,
            session: Mutex::new(None),
            reconnecting: AtomicBool::new(false),
        })
    }

    pub fn has_permission(&self) -> bool {
        read_token(&self.token_file).is_some()
    }

    /// Reopens an earlier granted session in the background. Does nothing when
    /// the user never ran `paste-setup`, so a daemon start cannot show a dialog
    /// out of the blue.
    pub fn restore_in_background(self: &Arc<Self>) {
        if !self.has_permission() || self.reconnecting.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = Arc::clone(self);
        thread::spawn(move || {
            let token = read_token(&this.token_file);
            match open(&this.token_file, token, RESTORE_WAIT) {
                Ok(Some(session)) => this.install(session),
                Ok(None) => {
                    eprintln!(
                        "wispx: the desktop no longer allows pasting; run `wispx paste-setup` \
                         to allow it again"
                    );
                    let _ = fs::remove_file(&this.token_file);
                }
                Err(error) => eprintln!("wispx: cannot restore the paste session: {error}"),
            }
            this.reconnecting.store(false, Ordering::SeqCst);
        });
    }

    /// Runs the desktop's consent dialog. Blocks until the user answers.
    pub fn setup(&self) -> Result<SetupOutcome, String> {
        // Ask afresh, so this also repairs a stale token.
        match open(&self.token_file, None, SETUP_WAIT)? {
            Some(session) => {
                self.install(session);
                Ok(SetupOutcome::Granted)
            }
            None => Ok(SetupOutcome::Declined),
        }
    }

    fn install(&self, session: Session) {
        if let Ok(mut slot) = self.session.lock() {
            *slot = Some(session);
        }
    }

    fn clear(&self) {
        if let Ok(mut slot) = self.session.lock() {
            *slot = None;
        }
    }
}

impl Injector for Arc<PlasmaInjector> {
    fn ready(&self) -> bool {
        self.session.lock().is_ok_and(|slot| slot.is_some())
    }

    fn current_focus(&self) -> Result<Option<FocusInfo>, String> {
        kwin::active_window(&self.runtime)
    }

    fn send_paste(&self, keys: Keys) -> Result<(), String> {
        let result = {
            let mut slot = self
                .session
                .lock()
                .map_err(|_| "the paste session is unavailable".to_string())?;
            let session = slot
                .as_mut()
                .ok_or_else(|| "the paste session is not open".to_string())?;
            press_chord(session, keys.keysyms())
        };
        if let Err(error) = &result {
            // The next dictation must not wait on this, or show a dialog mid-way.
            eprintln!("wispx: paste session lost: {error}");
            self.clear();
            self.restore_in_background();
        }
        result
    }
}

fn press_chord(session: &mut Session, keysyms: &[i32]) -> Result<(), String> {
    match session.closed() {
        Ok(false) => {}
        Ok(true) => return Err("the desktop closed the session".to_string()),
        Err(error) => return Err(error.to_string()),
    }
    let mut pressed = 0;
    let mut result = Ok(());
    for keysym in keysyms {
        match session.press(*keysym, KEY_PRESSED) {
            Ok(()) => pressed += 1,
            Err(error) => {
                result = Err(error.to_string());
                break;
            }
        }
        thread::sleep(KEY_GAP);
    }
    // Whatever happened, never leave a modifier stuck down.
    for keysym in keysyms[..pressed].iter().rev() {
        if let Err(error) = session.press(*keysym, KEY_RELEASED)
            && result.is_ok()
        {
            result = Err(error.to_string());
        }
        thread::sleep(KEY_GAP);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn tokens_are_read_trimmed_and_missing_or_blank_ones_mean_no_permission() {
        let temp = TempDir::new();
        let file = temp.path().join("token");
        let injector = PlasmaInjector::new(temp.path().to_path_buf(), file.clone());
        assert!(!injector.has_permission());
        fs::write(&file, "  \n").unwrap();
        assert!(!injector.has_permission());
        fs::write(&file, "abc\n").unwrap();
        assert!(injector.has_permission());
        assert_eq!(read_token(&file).as_deref(), Some("abc"));
    }

    #[test]
    fn without_a_session_paste_fails_cleanly_and_is_not_ready() {
        let temp = TempDir::new();
        let injector = PlasmaInjector::new(temp.path().to_path_buf(), temp.path().join("token"));
        assert!(!injector.ready());
        assert!(injector.send_paste(Keys::CtrlV).is_err());
    }
}
