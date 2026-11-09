//! Pastes a transcript only into the window that was focused when dictation
//! ended, like the Windows host. X11 checks `_NET_ACTIVE_WINDOW` and types with
//! XTEST. Wayland asks KWin for focus and types through the RemoteDesktop portal;
//! where focus can't be verified, the text is only copied.
//!
//! A paste borrows the clipboard: back it up, put the transcript there, press
//! Ctrl + V, put the backup back. Transcripts finish on independent threads, so
//! the whole exchange is one transaction under a lock, and focus and modifiers
//! are checked again at the last moment before any key is sent.
use super::{
    clipboard::{self, Backup},
    kwin,
    session::{Protocol, Session},
};
use crate::app::Delivery;
use ashpd::desktop::{
    PersistMode,
    remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions, StartOptions},
};
use futures_lite::{StreamExt, future};
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};

const CONTROL_L: u32 = 0xffe3;
const SHIFT_L: u32 = 0xffe1;
const KEY_V: u32 = 0x76;
/// Classes that paste with Ctrl + Shift + V, matched against whole name parts.
const TERMINALS: [&str; 18] = [
    "alacritty",
    "blackbox",
    "contour",
    "foot",
    "footclient",
    "ghostty",
    "kitty",
    "konsole",
    "ptyxis",
    "rio",
    "st",
    "terminal",
    "terminator",
    "tilix",
    "urxvt",
    "wezterm",
    "xterm",
    "yakuake",
];
pub fn is_terminal(class: &str) -> bool {
    class
        .to_ascii_lowercase()
        .split(['.', '-', '_', ' '])
        .any(|part| TERMINALS.contains(&part))
}
fn paste_keys(class: &str) -> &'static [u32] {
    if is_terminal(class) {
        &[CONTROL_L, SHIFT_L, KEY_V]
    } else {
        &[CONTROL_L, KEY_V]
    }
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The focused window when dictation ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    pub id: String,
    pub class: String,
}

/// Why no key reached the target. Every variant means nothing was typed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refused {
    FocusChanged,
    ModifiersHeld,
    Failed(String),
}

/// The desktop's clipboard, behind a seam so delivery ordering can be tested.
pub trait Clip: Send + Sync {
    fn set_text(&self, text: &str, transient: bool) -> Result<(), String>;
    fn text(&self) -> Option<String>;
    /// `None` when the clipboard is empty, too large, too slow or unreadable.
    fn backup(&self) -> Option<Backup>;
    fn restore(&self, backup: Backup) -> Result<(), String>;
}
struct SystemClip;
impl Clip for SystemClip {
    fn set_text(&self, text: &str, transient: bool) -> Result<(), String> {
        clipboard::set_text(text, transient)
    }
    fn text(&self) -> Option<String> {
        clipboard::text()
    }
    fn backup(&self) -> Option<Backup> {
        clipboard::backup()
    }
    fn restore(&self, backup: Backup) -> Result<(), String> {
        clipboard::restore(backup)
    }
}

/// Focus queries and key injection for one display protocol.
pub trait Keyboard: Send + Sync {
    /// Whether keys can be sent at all right now.
    fn ready(&self) -> bool;
    fn describe(&self) -> String;
    fn focused(&self) -> Option<Target>;
    /// `Some(held)` where key state is visible (X11); `None` on Wayland.
    fn modifier_held(&self) -> Option<bool>;
    /// Sends `keysyms` only if, immediately before the first key, `target` still
    /// has focus and no modifier is held. `Err` guarantees nothing was sent.
    fn paste(&self, keysyms: &[u32], target: &Target) -> Result<(), Refused>;
}
struct NoKeyboard;
impl Keyboard for NoKeyboard {
    fn ready(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        "copy only".into()
    }
    fn focused(&self) -> Option<Target> {
        None
    }
    fn modifier_held(&self) -> Option<bool> {
        None
    }
    fn paste(&self, _: &[u32], _: &Target) -> Result<(), Refused> {
        Err(Refused::Failed("no keyboard injection".into()))
    }
}

#[derive(Clone, Copy, Debug)]
struct Timing {
    /// How long a held modifier from the finishing shortcut may take to come up.
    modifier_wait: Duration,
    poll: Duration,
    /// Wayland can't report key state; the hold shortcut released before this ran.
    blind_release: Duration,
    /// Lets the clipboard owner announce the new offer before Ctrl + V.
    settle: Duration,
    /// Lets the target read the clipboard before the backup goes back.
    restore_after: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            modifier_wait: Duration::from_millis(2250),
            poll: Duration::from_millis(15),
            blind_release: Duration::from_millis(150),
            settle: Duration::from_millis(40),
            restore_after: Duration::from_millis(500),
        }
    }
}

pub struct Paster {
    keys: Box<dyn Keyboard>,
    clip: Box<dyn Clip>,
    /// Held for a whole backup → paste → restore exchange, and for plain copies,
    /// so overlapping transcripts never interleave on the clipboard.
    transaction: Mutex<()>,
    timing: Timing,
}
impl Paster {
    pub fn start(session: &Session) -> Arc<Self> {
        let keys: Box<dyn Keyboard> = match session.protocol {
            Protocol::X11 => X11::connect().map_or_else(
                |error| {
                    eprintln!("Paste: {error}");
                    Box::new(NoKeyboard) as Box<dyn Keyboard>
                },
                |x| Box::new(x),
            ),
            // Without a focus query a paste could land in another window.
            Protocol::Wayland if session.kde() => Box::new(PortalKeys::start()),
            _ => Box::new(NoKeyboard),
        };
        Arc::new(Self::with(keys, Box::new(SystemClip), Timing::default()))
    }
    fn with(keys: Box<dyn Keyboard>, clip: Box<dyn Clip>, timing: Timing) -> Self {
        Self {
            keys,
            clip,
            transaction: Mutex::new(()),
            timing,
        }
    }
    /// What can be verified about focus right now; `None` means copy only.
    /// Blocking (KWin can take up to two seconds): never call on the UI thread.
    pub fn focused(&self) -> Option<Target> {
        self.keys.ready().then(|| self.keys.focused()).flatten()
    }
    pub fn describe(&self) -> String {
        self.keys.describe()
    }
    /// Copies `text` as an ordinary clipboard entry, after any paste in progress.
    pub fn copy(&self, text: &str) -> Result<(), String> {
        let _transaction = lock(&self.transaction);
        self.clip.set_text(text, false)
    }
    /// Blocking: runs on the transcription worker, never the UI thread.
    pub fn deliver(&self, text: &str, target: Option<&Target>) -> Delivery {
        let _transaction = lock(&self.transaction);
        let copy = |delivery| match self.clip.set_text(text, false) {
            Ok(()) => delivery,
            Err(error) => {
                eprintln!("Clipboard: {error}");
                Delivery::Failed
            }
        };
        let Some(target) = target.filter(|_| self.keys.ready()) else {
            return copy(Delivery::Copied);
        };
        // Modifiers from the finishing shortcut would turn Ctrl + V into another chord.
        if !self.modifiers_released() {
            eprintln!("Paste: a modifier key stayed down; copying instead");
            return copy(Delivery::Copied);
        }
        // Cheap early check, so a lost target never disturbs the clipboard.
        if self.keys.focused().as_ref() != Some(target) {
            return copy(Delivery::FocusChanged);
        }
        let backup = self.clip.backup();
        if let Err(error) = self.clip.set_text(text, true) {
            eprintln!("Clipboard: {error}");
            return copy(Delivery::Copied);
        }
        thread::sleep(self.timing.settle);
        // Focus and modifiers are checked again inside, right before the keys.
        if let Err(refused) = self.keys.paste(paste_keys(&target.class), target) {
            if let Refused::Failed(error) = &refused {
                eprintln!("Paste: {error}");
            }
            // Re-offered as a normal entry: a transcript the user must paste by
            // hand shouldn't be hidden from clipboard history.
            return copy(match refused {
                Refused::FocusChanged => Delivery::FocusChanged,
                Refused::ModifiersHeld | Refused::Failed(_) => Delivery::Copied,
            });
        }
        thread::sleep(self.timing.restore_after);
        // Only while the clipboard still holds the transcript: anything else is
        // newer and belongs to the user.
        if let Some(backup) = backup
            && self.clip.text().as_deref() == Some(text)
            && let Err(error) = self.clip.restore(backup)
        {
            eprintln!("Clipboard restore: {error}");
        }
        Delivery::Pasted
    }
    fn modifiers_released(&self) -> bool {
        let deadline = Instant::now() + self.timing.modifier_wait;
        loop {
            match self.keys.modifier_held() {
                None => {
                    thread::sleep(self.timing.blind_release);
                    return true;
                }
                Some(false) => return true,
                Some(true) if Instant::now() >= deadline => return false,
                Some(true) => thread::sleep(self.timing.poll),
            }
        }
    }
}

struct X11 {
    conn: RustConnection,
    root: xproto::Window,
    active: xproto::Atom,
}
impl X11 {
    fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let active = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        Ok(Self { conn, root, active })
    }
    fn keycode(&self, keysym: u32) -> Option<u8> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = usize::from(mapping.keysyms_per_keycode).max(1);
        mapping
            .keysyms
            .chunks(per)
            .position(|syms| syms.contains(&keysym))
            .and_then(|i| u8::try_from(usize::from(min) + i).ok())
    }
    fn send(&self, kind: u8, code: u8) -> Result<(), String> {
        self.conn
            .xtest_fake_input(kind, code, x11rb::CURRENT_TIME, self.root, 0, 0, 0)
            .map(drop)
            .map_err(|e| e.to_string())
    }
}
impl Keyboard for X11 {
    fn ready(&self) -> bool {
        true
    }
    fn describe(&self) -> String {
        "XTEST with _NET_ACTIVE_WINDOW focus checks".into()
    }
    fn focused(&self) -> Option<Target> {
        let window = self
            .conn
            .get_property(
                false,
                self.root,
                self.active,
                xproto::AtomEnum::WINDOW,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?
            .value32()
            .and_then(|mut v| v.next())
            .filter(|&w| w != 0)
            .or_else(|| Some(self.conn.get_input_focus().ok()?.reply().ok()?.focus))?;
        let class = self
            .conn
            .get_property(
                false,
                window,
                xproto::AtomEnum::WM_CLASS,
                xproto::AtomEnum::STRING,
                0,
                256,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|reply| {
                // WM_CLASS is "instance\0class\0"; the class names the application.
                let parts: Vec<_> = reply.value.split(|&b| b == 0).collect();
                String::from_utf8_lossy(parts.get(1).copied().unwrap_or_default()).into_owned()
            })
            .unwrap_or_default();
        Some(Target {
            id: window.to_string(),
            class,
        })
    }
    fn modifier_held(&self) -> Option<bool> {
        // Unknown key state counts as held: refusing beats typing a wrong chord.
        let Some(keys) = self
            .conn
            .query_keymap()
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.keys)
        else {
            return Some(true);
        };
        // Ctrl, Shift, Alt and Super on both sides, as evdev codes plus 8.
        Some(
            [29_usize, 97, 42, 54, 56, 100, 125, 126]
                .iter()
                .map(|code| code + 8)
                .any(|code| keys[code / 8] & (1 << (code % 8)) != 0),
        )
    }
    fn paste(&self, keysyms: &[u32], target: &Target) -> Result<(), Refused> {
        let codes = keysyms
            .iter()
            .map(|&k| self.keycode(k).ok_or(format!("no key for keysym {k:#x}")))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Refused::Failed)?;
        if self.modifier_held() != Some(false) {
            return Err(Refused::ModifiersHeld);
        }
        if self.focused().as_ref() != Some(target) {
            return Err(Refused::FocusChanged);
        }
        let mut pressed = Vec::new();
        let mut result = Ok(());
        for &code in &codes {
            if let Err(error) = self.send(xproto::KEY_PRESS_EVENT, code) {
                result = Err(Refused::Failed(error));
                break;
            }
            pressed.push(code);
        }
        // Released even after a failure, so no modifier stays stuck down.
        for &code in pressed.iter().rev() {
            let _ = self.send(xproto::KEY_RELEASE_EVENT, code);
        }
        let flushed = self.conn.flush().map_err(|e| e.to_string());
        result?;
        flushed.map_err(Refused::Failed)
    }
}

/// How long a paste request may wait for the portal worker before it's dropped.
const PORTAL_WAIT: Duration = Duration::from_secs(3);
/// Upper bound on one key chord through the portal, releases included.
const CHORD_LIMIT: Duration = Duration::from_millis(1000);
const RELEASE_LIMIT: Duration = Duration::from_millis(250);
const KEY_GAP: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Queued,
    Typing,
    /// The caller gave up or the session ended; must never be typed.
    Dropped,
}
struct Request {
    keysyms: Vec<u32>,
    target: Target,
    deadline: Instant,
    state: Mutex<State>,
    reply: mpsc::SyncSender<Result<(), Refused>>,
}
impl Request {
    fn drop_with(&self, refused: Refused) {
        *lock(&self.state) = State::Dropped;
        let _ = self.reply.try_send(Err(refused));
    }
}
enum Command {
    Paste(Arc<Request>),
    /// The portal closed session number `.0`.
    Closed(u64),
}
/// Queues a paste and waits for it. A request that times out while still
/// queued is dropped, and the worker checks that before typing, so a caller
/// that has fallen back to copying never sees a late paste. A request already
/// typing is waited out: its chord is time-bounded.
fn submit(
    commands: &mpsc::Sender<Command>,
    keysyms: &[u32],
    target: &Target,
    wait: Duration,
) -> Result<(), Refused> {
    let (reply, result) = mpsc::sync_channel(1);
    let request = Arc::new(Request {
        keysyms: keysyms.to_vec(),
        target: target.clone(),
        deadline: Instant::now() + wait,
        state: Mutex::new(State::Queued),
        reply,
    });
    commands
        .send(Command::Paste(request.clone()))
        .map_err(|_| Refused::Failed("portal keyboard stopped".into()))?;
    await_reply(&request, &result, wait)
}
fn await_reply(
    request: &Request,
    result: &mpsc::Receiver<Result<(), Refused>>,
    wait: Duration,
) -> Result<(), Refused> {
    match result.recv_timeout(wait) {
        Ok(result) => result,
        Err(_) => {
            {
                let mut state = lock(&request.state);
                if *state != State::Typing {
                    *state = State::Dropped;
                    return Err(Refused::Failed("portal keyboard timed out".into()));
                }
            }
            result
                .recv()
                .unwrap_or_else(|_| Err(Refused::Failed("portal keyboard stopped".into())))
        }
    }
}
/// One request on the worker: verify focus, claim it, type. The claim happens
/// under the request's lock after the focus answer, so a caller that timed out
/// during the focus query wins and nothing is typed.
fn serve(
    request: &Request,
    focused: impl FnOnce() -> Option<Target>,
    chord: impl FnOnce(&[u32]) -> Result<(), String>,
) {
    if *lock(&request.state) != State::Queued || Instant::now() >= request.deadline {
        request.drop_with(Refused::Failed("paste request expired".into()));
        return;
    }
    let focus = focused();
    {
        let mut state = lock(&request.state);
        if *state != State::Queued || Instant::now() >= request.deadline {
            drop(state);
            request.drop_with(Refused::Failed("paste request expired".into()));
            return;
        }
        if focus.as_ref() != Some(&request.target) {
            drop(state);
            request.drop_with(Refused::FocusChanged);
            return;
        }
        *state = State::Typing;
    }
    let result = chord(&request.keysyms).map_err(Refused::Failed);
    let _ = request.reply.try_send(result);
}
/// Drops every queued request; used when a session ends.
fn reject_queued(commands: &mpsc::Receiver<Command>, reason: &str) {
    while let Ok(command) = commands.try_recv() {
        if let Command::Paste(request) = command {
            request.drop_with(Refused::Failed(reason.into()));
        }
    }
}
#[derive(Debug, Eq, PartialEq)]
enum SessionEnd {
    /// The desktop closed the session: remote control was revoked or stopped.
    Closed,
    /// Sending a key failed; the session may be broken.
    Failed,
}
/// Serves requests for session `generation` until it ends.
fn run_session(
    commands: &mpsc::Receiver<Command>,
    generation: u64,
    mut focused: impl FnMut() -> Option<Target>,
    mut chord: impl FnMut(&[u32]) -> Result<(), String>,
) -> SessionEnd {
    while let Ok(command) = commands.recv() {
        match command {
            Command::Paste(request) => {
                let mut failed = false;
                serve(&request, &mut focused, |keys| {
                    let result = chord(keys);
                    failed = result.is_err();
                    result
                });
                if failed {
                    return SessionEnd::Failed;
                }
            }
            Command::Closed(closed) if closed == generation => return SessionEnd::Closed,
            Command::Closed(_) => {}
        }
    }
    SessionEnd::Failed
}

/// Presses `keysyms` in order and releases them in reverse, within `limit`.
/// Every key that went down is released again even if a step fails or the
/// limit passes, so no modifier stays stuck. `Ok` means every key went down;
/// a press cut off by the limit counts as not typed, so the caller copies.
async fn chord<F, Fut>(keysyms: &[u32], mut send: F, limit: Duration) -> Result<(), String>
where
    F: FnMut(u32, KeyState) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut down: Vec<u32> = Vec::new();
    let mut all_down = false;
    let outcome = {
        let sequence = async {
            for &k in keysyms {
                send(k, KeyState::Pressed).await?;
                down.push(k);
                async_io::Timer::after(KEY_GAP).await;
            }
            all_down = true;
            while let Some(&k) = down.last() {
                send(k, KeyState::Released).await?;
                down.pop();
                async_io::Timer::after(KEY_GAP).await;
            }
            Ok(())
        };
        future::or(sequence, async {
            async_io::Timer::after(limit).await;
            Err(format!("keys took longer than {limit:?}"))
        })
        .await
    };
    while let Some(k) = down.pop() {
        let release = future::or(send(k, KeyState::Released), async {
            async_io::Timer::after(RELEASE_LIMIT).await;
            Err("release timed out".into())
        })
        .await;
        if let Err(error) = release {
            eprintln!("Paste: couldn't release key {k:#x}: {error}");
        }
    }
    match outcome {
        Ok(()) => Ok(()),
        // Every key went down, so the paste happened; only a release failed.
        Err(error) if all_down => {
            eprintln!("Paste: {error}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// One long-lived keyboard-only RemoteDesktop session. Plasma asks once, then a
/// restore token skips the dialog on later starts; dictation never opens it.
struct PortalKeys {
    commands: Mutex<mpsc::Sender<Command>>,
    ready: Arc<AtomicBool>,
    revoked: Arc<AtomicBool>,
}
fn token_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|dir| dir.join("WispX/remote-desktop.token"))
}
fn kwin_focus() -> Option<Target> {
    kwin::active_window().ok().flatten().map(|w| Target {
        id: w.id,
        class: w.class,
    })
}
impl PortalKeys {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let ready = Arc::new(AtomicBool::new(false));
        let revoked = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let (flag, gone, closer) = (ready.clone(), revoked.clone(), tx.clone());
        thread::Builder::new()
            .name("portal-keyboard".into())
            .spawn(move || {
                super::register_app();
                loop {
                    match future::block_on(open()) {
                        Ok((portal, session)) => {
                            let current = generation.fetch_add(1, Ordering::AcqRel) + 1;
                            // Requests queued while no session was open are stale.
                            reject_queued(&rx, "remote control was not ready");
                            let session = Arc::new(session);
                            watch_closed(
                                session.clone(),
                                current,
                                generation.clone(),
                                flag.clone(),
                                closer.clone(),
                            );
                            flag.store(true, Ordering::Release);
                            let end = run_session(&rx, current, kwin_focus, |keys| {
                                future::block_on(chord(
                                    keys,
                                    |keysym, state| {
                                        let (portal, session) = (&portal, &*session);
                                        async move {
                                            portal
                                                .notify_keyboard_keysym(
                                                    session,
                                                    keysym as i32,
                                                    state,
                                                    Default::default(),
                                                )
                                                .await
                                                .map_err(|e| e.to_string())
                                        }
                                    },
                                    CHORD_LIMIT,
                                ))
                            });
                            flag.store(false, Ordering::Release);
                            generation.fetch_add(1, Ordering::AcqRel);
                            reject_queued(&rx, "remote control ended");
                            let _ = future::block_on(session.close());
                            if end == SessionEnd::Closed {
                                // Reopening would undo the user's choice to stop
                                // remote control; it's requested again at next start.
                                eprintln!(
                                    "Paste: the desktop ended remote control; transcripts \
                                     will be copied until WispX restarts"
                                );
                                gone.store(true, Ordering::Release);
                                return;
                            }
                        }
                        Err(Opened::Declined) => {
                            eprintln!(
                                "Paste: remote control was declined; transcripts will be copied"
                            );
                            gone.store(true, Ordering::Release);
                            return;
                        }
                        Err(Opened::Failed(error)) => eprintln!("Paste: {error}"),
                    }
                    thread::sleep(Duration::from_secs(5));
                }
            })
            .expect("spawn portal keyboard");
        Self {
            commands: Mutex::new(tx),
            ready,
            revoked,
        }
    }
}
/// Tells the worker when the desktop closes session `current`, for example when
/// the user stops remote control. Returns once it's listening, so no close is
/// missed; it stops watching once a newer session replaces this one.
fn watch_closed(
    session: Arc<ashpd::desktop::Session<RemoteDesktop>>,
    current: u64,
    generation: Arc<AtomicU64>,
    ready: Arc<AtomicBool>,
    commands: mpsc::Sender<Command>,
) {
    let (listening, subscribed) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("portal-keyboard-closed".into())
        .spawn(move || {
            let closed = match future::block_on(session.receive_closed()) {
                Ok(closed) => closed,
                Err(error) => {
                    eprintln!("Paste: can't watch the remote control session: {error}");
                    let _ = listening.send(());
                    return;
                }
            };
            let _ = listening.send(());
            let mut closed = std::pin::pin!(closed);
            loop {
                // A stream that ends means the portal connection went away.
                let signal = future::block_on(future::or(
                    async {
                        closed.next().await;
                        Some(())
                    },
                    async {
                        async_io::Timer::after(Duration::from_secs(1)).await;
                        None
                    },
                ));
                if generation.load(Ordering::Acquire) != current {
                    return;
                }
                if signal.is_some() {
                    // Before the worker sees the message, so no new paste is queued.
                    ready.store(false, Ordering::Release);
                    let _ = commands.send(Command::Closed(current));
                    return;
                }
            }
        });
    match spawned {
        Ok(_) => {
            let _ = subscribed.recv_timeout(Duration::from_secs(2));
        }
        Err(error) => eprintln!("Paste: {error}"),
    }
}
impl Keyboard for PortalKeys {
    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    fn describe(&self) -> String {
        if self.ready() {
            "RemoteDesktop portal with KWin focus checks".into()
        } else if self.revoked.load(Ordering::Acquire) {
            "copy only (remote control was declined or stopped; restart WispX to ask again)".into()
        } else {
            "RemoteDesktop portal (not granted yet)".into()
        }
    }
    fn focused(&self) -> Option<Target> {
        kwin_focus()
    }
    fn modifier_held(&self) -> Option<bool> {
        None
    }
    fn paste(&self, keysyms: &[u32], target: &Target) -> Result<(), Refused> {
        if !self.ready() {
            return Err(Refused::Failed("remote control isn't available".into()));
        }
        let commands = lock(&self.commands).clone();
        // The worker checks focus again immediately before typing.
        submit(&commands, keysyms, target, PORTAL_WAIT)
    }
}
enum Opened {
    Declined,
    Failed(String),
}
impl From<ashpd::Error> for Opened {
    fn from(error: ashpd::Error) -> Self {
        match error {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => Self::Declined,
            other => Self::Failed(other.to_string()),
        }
    }
}
async fn open() -> Result<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>), Opened> {
    let portal = RemoteDesktop::new().await?;
    let session = portal.create_session(Default::default()).await?;
    let path = token_path();
    let token = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());
    portal
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(ashpd::enumflags2::BitFlags::from_flag(DeviceType::Keyboard))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?
        .response()?;
    let selected = portal
        .start(&session, None, StartOptions::default())
        .await?
        .response()?;
    if !selected.devices().contains(DeviceType::Keyboard) {
        return Err(Opened::Declined);
    }
    if let (Some(path), Some(token)) = (path, selected.restore_token()) {
        let _ = crate::settings::atomic_write(&path, token.as_bytes());
    }
    Ok((portal, session))
}

#[cfg(test)]
mod tests {
    // Failure modes: "footnotes" or "testing" count as terminals; terminals get
    // Ctrl + V, which they treat as a literal control character; two transcripts
    // finishing together paste each other's text or restore the wrong clipboard;
    // focus moving during the clipboard backup still pastes; a timed-out portal
    // request types later; a failed chord leaves Ctrl held; a failed copy
    // reports success.
    use super::*;
    use std::sync::{Barrier, atomic::AtomicUsize};

    #[test]
    fn terminals_paste_with_shift() {
        for class in [
            "org.kde.konsole",
            "kitty",
            "gnome-terminal-server",
            "XTerm",
            "foot",
        ] {
            assert!(is_terminal(class), "{class}");
            assert_eq!(paste_keys(class), &[CONTROL_L, SHIFT_L, KEY_V]);
        }
        for class in ["firefox", "footnotes", "org.kde.kate", "testing", ""] {
            assert!(!is_terminal(class), "{class}");
            assert_eq!(paste_keys(class), &[CONTROL_L, KEY_V]);
        }
    }

    fn quick() -> Timing {
        Timing {
            modifier_wait: Duration::from_millis(30),
            poll: Duration::from_millis(1),
            blind_release: Duration::ZERO,
            settle: Duration::ZERO,
            restore_after: Duration::from_millis(5),
        }
    }
    fn target(id: &str) -> Target {
        Target {
            id: id.into(),
            class: "editor".into(),
        }
    }

    /// A clipboard holding one text entry; `transient` marks hidden entries.
    #[derive(Default)]
    struct FakeClip {
        content: Mutex<Option<(String, bool)>>,
        broken: bool,
        /// Runs during `backup`, standing in for a slow clipboard owner.
        during_backup: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl FakeClip {
        fn holding(text: &str) -> Arc<Self> {
            Arc::new(Self {
                content: Mutex::new(Some((text.into(), false))),
                ..Self::default()
            })
        }
        fn now(&self) -> Option<(String, bool)> {
            lock(&self.content).clone()
        }
    }
    impl Clip for Arc<FakeClip> {
        fn set_text(&self, text: &str, transient: bool) -> Result<(), String> {
            if self.broken {
                return Err("No clipboard is available".into());
            }
            *lock(&self.content) = Some((text.into(), transient));
            Ok(())
        }
        fn text(&self) -> Option<String> {
            self.now().map(|(text, _)| text)
        }
        fn backup(&self) -> Option<Backup> {
            if let Some(hook) = lock(&self.during_backup).take() {
                hook();
            }
            self.text().map(Backup::Text)
        }
        fn restore(&self, backup: Backup) -> Result<(), String> {
            let Backup::Text(text) = backup else {
                unreachable!()
            };
            *lock(&self.content) = Some((text, false));
            Ok(())
        }
    }

    /// A keyboard whose focused window and modifier state the test controls.
    /// Each paste records what the target would have received.
    struct FakeKeys {
        clip: Arc<FakeClip>,
        focus: Mutex<String>,
        modifier: Mutex<Option<bool>>,
        pasted: Mutex<Vec<(String, String)>>,
        /// When set, a paste waits here until the test lets it finish.
        gate: Option<Arc<Barrier>>,
        /// How long each paste takes, so concurrent deliveries would overlap.
        hold: Duration,
        active: AtomicUsize,
        most_active: AtomicUsize,
    }
    impl FakeKeys {
        fn new(clip: &Arc<FakeClip>, focus: &str) -> Arc<Self> {
            Arc::new(Self {
                clip: clip.clone(),
                focus: Mutex::new(focus.into()),
                modifier: Mutex::new(Some(false)),
                pasted: Mutex::new(Vec::new()),
                gate: None,
                hold: Duration::ZERO,
                active: AtomicUsize::new(0),
                most_active: AtomicUsize::new(0),
            })
        }
    }
    impl Keyboard for Arc<FakeKeys> {
        fn ready(&self) -> bool {
            true
        }
        fn describe(&self) -> String {
            "fake".into()
        }
        fn focused(&self) -> Option<Target> {
            Some(target(&lock(&self.focus)))
        }
        fn modifier_held(&self) -> Option<bool> {
            *lock(&self.modifier)
        }
        fn paste(&self, _: &[u32], target: &Target) -> Result<(), Refused> {
            if self.modifier_held() == Some(true) {
                return Err(Refused::ModifiersHeld);
            }
            if self.focused().as_ref() != Some(target) {
                return Err(Refused::FocusChanged);
            }
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_active.fetch_max(now, Ordering::SeqCst);
            let received = self.clip.text().unwrap_or_default();
            lock(&self.pasted).push((target.id.clone(), received));
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            thread::sleep(self.hold);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }
    fn paster(keys: &Arc<FakeKeys>, clip: &Arc<FakeClip>) -> Arc<Paster> {
        Arc::new(Paster::with(
            Box::new(keys.clone()),
            Box::new(clip.clone()),
            quick(),
        ))
    }

    #[test]
    fn paste_restores_the_previous_clipboard() {
        let clip = FakeClip::holding("user's text");
        let keys = FakeKeys::new(&clip, "a");
        let delivery = paster(&keys, &clip).deliver("transcript", Some(&target("a")));
        assert_eq!(delivery, Delivery::Pasted);
        assert_eq!(
            *lock(&keys.pasted),
            vec![("a".to_owned(), "transcript".to_owned())]
        );
        assert_eq!(clip.now(), Some(("user's text".into(), false)));
    }

    #[test]
    fn overlapping_deliveries_paste_their_own_text_and_restore_the_original() {
        let clip = FakeClip::holding("original");
        let mut keys = FakeKeys::new(&clip, "a");
        Arc::get_mut(&mut keys).expect("unshared").hold = Duration::from_millis(40);
        let paster = paster(&keys, &clip);
        let start = Arc::new(Barrier::new(3));
        let workers: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(|text| {
                let (paster, start) = (paster.clone(), start.clone());
                thread::spawn(move || {
                    start.wait();
                    paster.deliver(text, Some(&target("a")))
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().expect("delivery"), Delivery::Pasted);
        }
        assert_eq!(
            keys.most_active.load(Ordering::SeqCst),
            1,
            "pastes overlapped"
        );
        let mut received: Vec<_> = lock(&keys.pasted)
            .iter()
            .map(|(_, text)| text.clone())
            .collect();
        received.sort();
        assert_eq!(received, ["first", "second", "third"]);
        // Each backup saw the original, not another transcript.
        assert_eq!(clip.now(), Some(("original".into(), false)));
    }

    #[test]
    fn copy_last_waits_for_a_paste_in_progress() {
        let clip = FakeClip::holding("original");
        let mut keys = FakeKeys::new(&clip, "a");
        let gate = Arc::new(Barrier::new(2));
        Arc::get_mut(&mut keys).expect("unshared").gate = Some(gate.clone());
        let paster = paster(&keys, &clip);
        let delivering = {
            let paster = paster.clone();
            thread::spawn(move || paster.deliver("new", Some(&target("a"))))
        };
        // Wait until the paste is underway, then ask for the copy.
        while lock(&keys.pasted).is_empty() {
            thread::yield_now();
        }
        let copying = {
            let paster = paster.clone();
            thread::spawn(move || paster.copy("older transcript"))
        };
        thread::sleep(Duration::from_millis(20));
        // The copy hasn't run yet: the paste still owns the clipboard.
        assert_eq!(clip.text().as_deref(), Some("new"));
        gate.wait();
        assert_eq!(delivering.join().expect("deliver"), Delivery::Pasted);
        copying.join().expect("copy").expect("copied");
        // The copy came last and the restore didn't overwrite it.
        assert_eq!(clip.now(), Some(("older transcript".into(), false)));
    }

    #[test]
    fn focus_change_during_the_backup_copies_instead_of_pasting() {
        let clip = FakeClip::holding("original");
        let keys = FakeKeys::new(&clip, "a");
        let moved = keys.clone();
        *lock(&clip.during_backup) = Some(Box::new(move || *lock(&moved.focus) = "b".into()));
        let delivery = paster(&keys, &clip).deliver("transcript", Some(&target("a")));
        assert_eq!(delivery, Delivery::FocusChanged);
        assert!(lock(&keys.pasted).is_empty(), "nothing may be typed");
        // Left copied as a normal entry, not a hidden transient one.
        assert_eq!(clip.now(), Some(("transcript".into(), false)));
    }

    #[test]
    fn a_modifier_that_stays_down_copies_instead_of_pasting() {
        let clip = FakeClip::holding("original");
        let keys = FakeKeys::new(&clip, "a");
        *lock(&keys.modifier) = Some(true);
        let delivery = paster(&keys, &clip).deliver("transcript", Some(&target("a")));
        assert_eq!(delivery, Delivery::Copied);
        assert!(lock(&keys.pasted).is_empty());
        assert_eq!(clip.now(), Some(("transcript".into(), false)));
    }

    #[test]
    fn a_newer_clipboard_is_not_overwritten_by_the_restore() {
        let clip = FakeClip::holding("original");
        let keys = FakeKeys::new(&clip, "a");
        let paster = Paster::with(
            Box::new(keys.clone()),
            Box::new(clip.clone()),
            Timing {
                restore_after: Duration::from_millis(80),
                ..quick()
            },
        );
        let copier = {
            let clip = clip.clone();
            let keys = keys.clone();
            thread::spawn(move || {
                while lock(&keys.pasted).is_empty() {
                    thread::yield_now();
                }
                *lock(&clip.content) = Some(("user copied this".into(), false));
            })
        };
        assert_eq!(
            paster.deliver("transcript", Some(&target("a"))),
            Delivery::Pasted
        );
        copier.join().expect("copier");
        assert_eq!(clip.now(), Some(("user copied this".into(), false)));
    }

    #[test]
    fn no_clipboard_reports_failure_instead_of_a_copy() {
        let clip = Arc::new(FakeClip {
            broken: true,
            ..FakeClip::default()
        });
        let keys = FakeKeys::new(&clip, "a");
        let paster = paster(&keys, &clip);
        assert_eq!(paster.deliver("transcript", None), Delivery::Failed);
        assert_eq!(
            paster.deliver("transcript", Some(&target("a"))),
            Delivery::Failed
        );
        assert!(lock(&keys.pasted).is_empty());
        assert!(paster.copy("transcript").is_err());
    }

    fn request(wait: Duration) -> (Arc<Request>, mpsc::Receiver<Result<(), Refused>>) {
        let (reply, result) = mpsc::sync_channel(1);
        let request = Arc::new(Request {
            keysyms: vec![CONTROL_L, KEY_V],
            target: target("a"),
            deadline: Instant::now() + wait,
            state: Mutex::new(State::Queued),
            reply,
        });
        (request, result)
    }

    #[test]
    fn a_timed_out_portal_request_is_never_typed_later() {
        let (tx, rx) = mpsc::channel();
        // No worker yet: the caller gives up and falls back to copying.
        let started = Instant::now();
        let result = submit(
            &tx,
            &[CONTROL_L, KEY_V],
            &target("a"),
            Duration::from_millis(20),
        );
        assert!(matches!(result, Err(Refused::Failed(_))), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        // The worker catches up, sees the dropped request and types nothing.
        drop(tx);
        let typed = Mutex::new(0);
        run_session(
            &rx,
            1,
            || Some(target("a")),
            |_| {
                *lock(&typed) += 1;
                Ok(())
            },
        );
        assert_eq!(*lock(&typed), 0);
    }

    #[test]
    fn a_request_that_expires_during_the_focus_query_is_not_typed() {
        let (request, result) = request(Duration::from_millis(20));
        let mut typed = false;
        serve(
            &request,
            || {
                // KWin answering slowly; meanwhile the caller gives up.
                *lock(&request.state) = State::Dropped;
                Some(target("a"))
            },
            |_| {
                typed = true;
                Ok(())
            },
        );
        assert!(!typed);
        assert!(matches!(result.try_recv(), Ok(Err(Refused::Failed(_)))));
    }

    #[test]
    fn the_worker_checks_focus_right_before_typing() {
        let (request, result) = request(Duration::from_secs(5));
        let mut typed = false;
        serve(
            &request,
            || Some(target("b")),
            |_| {
                typed = true;
                Ok(())
            },
        );
        assert!(!typed);
        assert_eq!(result.try_recv(), Ok(Err(Refused::FocusChanged)));
    }

    #[test]
    fn a_closed_session_rejects_queued_pastes() {
        let (tx, rx) = mpsc::channel();
        let (late, late_result) = request(Duration::from_secs(5));
        tx.send(Command::Closed(7)).expect("send");
        tx.send(Command::Paste(late.clone())).expect("send");
        let typed = Mutex::new(0);
        let end = run_session(
            &rx,
            7,
            || Some(target("a")),
            |_| {
                *lock(&typed) += 1;
                Ok(())
            },
        );
        assert_eq!(end, SessionEnd::Closed);
        reject_queued(&rx, "remote control ended");
        assert_eq!(*lock(&typed), 0);
        assert!(matches!(
            late_result.try_recv(),
            Ok(Err(Refused::Failed(_)))
        ));
        assert_eq!(*lock(&late.state), State::Dropped);
        // A close notice for an older session doesn't end the current one.
        tx.send(Command::Closed(6)).expect("send");
        drop(tx);
        assert_eq!(run_session(&rx, 7, || None, |_| Ok(())), SessionEnd::Failed);
    }

    #[test]
    fn a_caller_waits_out_a_paste_already_typing() {
        let (typing, result) = request(Duration::from_secs(5));
        *lock(&typing.state) = State::Typing;
        let worker = {
            let typing = typing.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(60));
                let _ = typing.reply.try_send(Ok(()));
            })
        };
        // The wait runs out mid-chord: reporting a copy now would be a lie.
        assert_eq!(
            await_reply(&typing, &result, Duration::from_millis(5)),
            Ok(())
        );
        worker.join().expect("typing");
        // Still queued when the wait runs out: dropped, and never typed after.
        let (queued, result) = request(Duration::from_secs(5));
        assert!(await_reply(&queued, &result, Duration::from_millis(5)).is_err());
        assert_eq!(*lock(&queued.state), State::Dropped);
    }

    type Log = Arc<Mutex<Vec<(u32, bool)>>>;
    fn fake_send(
        log: &Log,
        fail_on: Option<(u32, bool)>,
        hang_on: Option<(u32, bool)>,
    ) -> impl FnMut(u32, KeyState) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>>>>
    {
        let log = log.clone();
        move |key, state| {
            let down = state == KeyState::Pressed;
            let log = log.clone();
            Box::pin(async move {
                if hang_on == Some((key, down)) {
                    future::pending::<()>().await;
                }
                if fail_on == Some((key, down)) {
                    return Err("portal refused the key".into());
                }
                lock(&log).push((key, down));
                Ok(())
            })
        }
    }

    #[test]
    fn a_failed_press_releases_the_keys_already_down() {
        let log: Log = Arc::default();
        let keys = [CONTROL_L, SHIFT_L, KEY_V];
        let result = future::block_on(chord(
            &keys,
            fake_send(&log, Some((KEY_V, true)), None),
            Duration::from_secs(1),
        ));
        assert!(result.is_err());
        assert_eq!(
            *lock(&log),
            [
                (CONTROL_L, true),
                (SHIFT_L, true),
                (SHIFT_L, false),
                (CONTROL_L, false)
            ]
        );
    }

    #[test]
    fn a_stalled_chord_is_bounded_and_cleaned_up() {
        let log: Log = Arc::default();
        let started = Instant::now();
        let result = future::block_on(chord(
            &[CONTROL_L, KEY_V],
            fake_send(&log, None, Some((KEY_V, true))),
            Duration::from_millis(50),
        ));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(*lock(&log), [(CONTROL_L, true), (CONTROL_L, false)]);
    }

    #[test]
    fn a_failed_release_still_counts_as_pasted() {
        let log: Log = Arc::default();
        let result = future::block_on(chord(
            &[CONTROL_L, KEY_V],
            fake_send(&log, Some((KEY_V, false)), None),
            Duration::from_secs(1),
        ));
        assert_eq!(result, Ok(()));
        // Ctrl is still released even though releasing V failed.
        assert!(lock(&log).contains(&(CONTROL_L, false)));
    }
}
