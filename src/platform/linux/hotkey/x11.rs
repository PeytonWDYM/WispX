//! X11 shortcuts: XInput2 raw key events feed the same state machine as the
//! Windows hook. X11 can't drop an event after the fact, so the keys Windows
//! swallows are claimed ahead of time with grabs that follow the machine's state:
//! Space while it would lock hands-free, Esc while dictating, the main key of a
//! chord shortcut, and the whole keyboard while Options captures a shortcut.
use super::{HookMessage, Sink, keymap};
use crate::{
    hotkey::{ESCAPE, Hotkey, LALT, LCTRL, LSHIFT, LWIN, RALT, RCTRL, RSHIFT, RWIN, SPACE},
    hotkey_machine::{HotkeyMachine, KeyEvent},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xinput::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _, GrabMode, ModMask},
    },
    rust_connection::RustConnection,
};

/// Grabs ignore Caps Lock and Num Lock (Mod2), like the Windows hook does.
fn locks() -> [ModMask; 4] {
    [
        ModMask::from(0_u16),
        ModMask::LOCK,
        ModMask::M2,
        ModMask::LOCK | ModMask::M2,
    ]
}
fn keycode(vk: u8) -> Option<u8> {
    keymap::evdev(vk).and_then(|code| u8::try_from(code + 8).ok())
}

/// What the state machine wants claimed right now.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Grabs {
    keyboard: bool,
    main: Option<(u8, ModMask)>,
    space: bool,
    escape: bool,
}
fn desired(machine: &HotkeyMachine) -> Grabs {
    Grabs {
        keyboard: machine.capturing(),
        main: main_grab(machine.hotkey()),
        space: machine.lock_armed(),
        escape: machine.dictating(),
    }
}
fn modifiers(hotkey: &Hotkey) -> ModMask {
    hotkey.keys().iter().fold(ModMask::from(0_u16), |mask, &k| {
        mask | match k {
            LCTRL | RCTRL => ModMask::CONTROL,
            LSHIFT | RSHIFT => ModMask::SHIFT,
            LALT | RALT => ModMask::M1,
            LWIN | RWIN => ModMask::M4,
            _ => ModMask::from(0_u16),
        }
    })
}
fn main_grab(hotkey: &Hotkey) -> Option<(u8, ModMask)> {
    Some((keycode(hotkey.main_key()?)?, modifiers(hotkey)))
}

/// What the X server actually granted, as exact (key, modifiers) pairs, so only
/// grabs WispX holds are released and nothing is claimed that another app owns.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Held {
    keyboard: bool,
    main: Vec<(u8, ModMask)>,
    space: Vec<(u8, ModMask)>,
    escape: Vec<(u8, ModMask)>,
}
/// A grab another client already holds. X11 grabs are exclusive: the other
/// app keeps receiving those keys, so the user has to be told.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Refusal {
    Keyboard(String),
    Main,
    Space,
    Escape,
}
impl Refusal {
    fn message(&self, hotkey: &Hotkey) -> String {
        match self {
            Self::Keyboard(reason) => {
                format!(
                    "Another app has the keyboard, so the shortcut can't be recorded ({reason})"
                )
            }
            Self::Main => format!("Another app also uses {hotkey}, and will still receive it"),
            Self::Space => {
                "Another app uses Space with modifiers; hands-free may also reach it".into()
            }
            Self::Escape => {
                "Another app uses Esc with modifiers; cancelling may also reach it".into()
            }
        }
    }
}
/// The X server's grab requests, checked; a seam for failure tests.
trait GrabServer {
    fn grab_key(&self, key: u8, mask: ModMask) -> Result<(), String>;
    fn ungrab_key(&self, key: u8, mask: ModMask);
    fn grab_keyboard(&self) -> Result<(), String>;
    fn ungrab_keyboard(&self);
}
/// Grabs `key` under each mask, keeping whichever the server grants.
fn grab_each(server: &impl GrabServer, key: u8, masks: &[ModMask]) -> (Vec<(u8, ModMask)>, bool) {
    let mut held = Vec::new();
    let mut complete = true;
    for &mask in masks {
        match server.grab_key(key, mask) {
            Ok(()) => held.push((key, mask)),
            Err(_) => complete = false,
        }
    }
    (held, complete)
}
fn with_locks(mask: ModMask) -> Vec<ModMask> {
    locks().into_iter().map(|lock| mask | lock).collect()
}
/// Space and Esc are wanted under any modifiers. If another app grabbed one of
/// them with some modifier, AnyModifier fails as a whole, so fall back to the
/// combinations dictation actually uses: none, and the shortcut's modifiers.
fn grab_any(server: &impl GrabServer, key: u8, shortcut: ModMask) -> (Vec<(u8, ModMask)>, bool) {
    if server.grab_key(key, ModMask::ANY).is_ok() {
        return (vec![(key, ModMask::ANY)], true);
    }
    let mut masks = with_locks(ModMask::from(0_u16));
    if shortcut != ModMask::from(0_u16) {
        masks.extend(with_locks(shortcut));
    }
    grab_each(server, key, &masks)
}
fn release(server: &impl GrabServer, held: &mut Vec<(u8, ModMask)>) {
    for (key, mask) in held.drain(..) {
        server.ungrab_key(key, mask);
    }
}
/// Moves the server's grabs from `before` to `want`, acting only on what
/// changed, and records in `held` exactly what the server granted.
fn reconcile(
    server: &impl GrabServer,
    held: &mut Held,
    before: &Grabs,
    want: &Grabs,
    shortcut: ModMask,
) -> Vec<Refusal> {
    let mut refused = Vec::new();
    if want.keyboard != before.keyboard {
        if want.keyboard {
            match server.grab_keyboard() {
                Ok(()) => held.keyboard = true,
                Err(reason) => refused.push(Refusal::Keyboard(reason)),
            }
        } else if std::mem::take(&mut held.keyboard) {
            server.ungrab_keyboard();
        }
    }
    if want.main != before.main {
        release(server, &mut held.main);
        if let Some((key, mask)) = want.main {
            let (granted, complete) = grab_each(server, key, &with_locks(mask));
            held.main = granted;
            if !complete {
                refused.push(Refusal::Main);
            }
        }
    }
    for (key, was, now, slot, refusal) in [
        (
            SPACE,
            before.space,
            want.space,
            &mut held.space,
            Refusal::Space,
        ),
        (
            ESCAPE,
            before.escape,
            want.escape,
            &mut held.escape,
            Refusal::Escape,
        ),
    ] {
        if was == now {
            continue;
        }
        release(server, slot);
        if now && let Some(code) = keycode(key) {
            let (granted, complete) = grab_any(server, code, shortcut);
            *slot = granted;
            if !complete {
                refused.push(refusal);
            }
        }
    }
    refused
}

struct Control {
    keys: HotkeyMachine,
    generation: u64,
    /// Last state requested of the server, and what it granted.
    wanted: Grabs,
    held: Held,
    /// Conflicts already reported, so each is announced once, not per press.
    reported: Vec<Refusal>,
}
/// Brings the server's grabs in line with what the state machine owns now.
/// A refused keyboard grab ends the capture, since keys would leak to the
/// app holding the keyboard. Returns the conflicts to tell the user about.
fn settle(server: &impl GrabServer, control: &mut Control) -> Settled {
    let mut settled = Settled::default();
    loop {
        let want = desired(&control.keys);
        let before = std::mem::replace(&mut control.wanted, want.clone());
        if want == before {
            break;
        }
        let shortcut = modifiers(control.keys.hotkey());
        let refused = reconcile(server, &mut control.held, &before, &want, shortcut);
        let mut again = false;
        for refusal in refused {
            if matches!(refusal, Refusal::Keyboard(_)) {
                control.keys.cancel_capture();
                settled.capture_failed = true;
                again = true;
            }
            if matches!(refusal, Refusal::Keyboard(_)) || !control.reported.contains(&refusal) {
                settled
                    .messages
                    .push(refusal.message(control.keys.hotkey()));
                control.reported.push(refusal);
            }
        }
        if !again {
            break;
        }
    }
    settled
}
#[derive(Debug, Default)]
struct Settled {
    messages: Vec<String>,
    capture_failed: bool,
    /// The capture generation a failed capture belongs to.
    generation: u64,
}
struct Server<'a> {
    conn: &'a RustConnection,
    root: xproto::Window,
}
impl GrabServer for Server<'_> {
    fn grab_key(&self, key: u8, mask: ModMask) -> Result<(), String> {
        self.conn
            .grab_key(
                false,
                self.root,
                mask,
                key,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| e.to_string())
    }
    fn ungrab_key(&self, key: u8, mask: ModMask) {
        let _ = self.conn.ungrab_key(key, self.root, mask);
        let _ = self.conn.flush();
    }
    fn grab_keyboard(&self) -> Result<(), String> {
        let status = self
            .conn
            .grab_keyboard(
                false,
                self.root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .status;
        if status == xproto::GrabStatus::SUCCESS {
            Ok(())
        } else {
            Err(format!("{status:?}"))
        }
    }
    fn ungrab_keyboard(&self) {
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }
}
struct Shared {
    conn: RustConnection,
    root: xproto::Window,
    wake: xproto::Window,
    xtest: Option<u16>,
    control: Mutex<Control>,
    stopping: AtomicBool,
}
pub struct Hook {
    shared: Arc<Shared>,
    sink: Sink,
    thread: Option<JoinHandle<()>>,
}
impl Hook {
    pub fn start(hotkey: Hotkey, sink: Sink) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let version = conn.xinput_xi_query_version(2, 2)?.reply()?;
        if (version.major_version, version.minor_version) < (2, 1) {
            return Err("XInput 2.1 is required for global shortcuts".into());
        }
        // Raw events reach every root listener, regardless of grabs (XI 2.1).
        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask {
                deviceid: 1, // XIAllMasterDevices
                mask: vec![
                    xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_KEY_RELEASE,
                ],
            }],
        )?
        .check()?;
        // Our own pastes arrive through the XTEST device; skip them like injected input.
        let xtest = conn
            .xinput_xi_query_device(0_u16)?
            .reply()?
            .infos
            .iter()
            .find(|d| {
                d.type_ == xinput::DeviceType::SLAVE_KEYBOARD
                    && String::from_utf8_lossy(&d.name).contains("XTEST")
            })
            .map(|d| d.deviceid);
        let wake = conn.generate_id()?;
        conn.create_window(
            0,
            wake,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new(),
        )?;
        conn.flush()?;
        let shared = Arc::new(Shared {
            conn,
            root,
            wake,
            xtest,
            control: Mutex::new(Control {
                keys: HotkeyMachine::new(hotkey),
                generation: 0,
                wanted: Grabs::default(),
                held: Held::default(),
                reported: Vec::new(),
            }),
            stopping: AtomicBool::new(false),
        });
        let settled = shared.sync(&mut shared.control.lock().expect("hotkey lock"));
        report(&sink, settled);
        let worker = shared.clone();
        let worker_sink = sink.clone();
        let thread = thread::Builder::new()
            .name("x11-hotkeys".into())
            .spawn(move || worker.run(&worker_sink))?;
        Ok(Self {
            shared,
            sink,
            thread: Some(thread),
        })
    }
    fn with<R>(&self, f: impl FnOnce(&mut Control) -> R) -> R {
        let (result, settled) = {
            let mut control = self.shared.control.lock().expect("hotkey lock");
            let result = f(&mut control);
            (result, self.shared.sync(&mut control))
        };
        report(&self.sink, settled);
        result
    }
    pub fn set_hotkey(&self, key: Hotkey) {
        self.with(|c| {
            // A new shortcut may conflict with a different app; say so again.
            c.reported.clear();
            c.keys.set_hotkey(key);
        });
    }
    pub fn force_idle(&self) {
        self.with(|c| c.keys.force_idle());
    }
    pub fn begin_capture(&self) -> u64 {
        let (event, generation) = self.with(|c| {
            c.generation += 1;
            (c.keys.begin_capture(), c.generation)
        });
        if let Some(event) = event {
            (self.sink)(HookMessage::Dictation(event));
        }
        generation
    }
    pub fn cancel_capture(&self) {
        self.with(|c| c.keys.cancel_capture());
    }
}
impl Shared {
    fn run(&self, sink: &Sink) {
        loop {
            let event = match self.conn.wait_for_event() {
                Ok(event) => event,
                Err(error) => {
                    eprintln!("X11 shortcuts: {error}");
                    return;
                }
            };
            let (detail, sourceid, down) = match event {
                Event::XinputRawKeyPress(e) => (e.detail, e.sourceid, true),
                Event::XinputRawKeyRelease(e) => (e.detail, e.sourceid, false),
                Event::ClientMessage(e) if e.window == self.wake => {
                    if self.stopping.load(Ordering::Acquire) {
                        return;
                    }
                    continue;
                }
                _ => continue,
            };
            let Some(key) = u16::try_from(detail)
                .ok()
                .and_then(|d| d.checked_sub(8))
                .and_then(keymap::vk)
            else {
                continue;
            };
            let (out, generation, settled) = {
                let mut control = self.control.lock().expect("hotkey lock");
                let out = control.keys.process(KeyEvent {
                    key,
                    down,
                    injected: Some(sourceid) == self.xtest,
                });
                let settled = self.sync(&mut control);
                (out, control.generation, settled)
            };
            report(sink, settled);
            if let Some(event) = out.event {
                sink(HookMessage::Dictation(event));
            }
            if let Some(key) = out.capture {
                sink(HookMessage::Captured { generation, key });
            }
        }
    }
    fn sync(&self, control: &mut Control) -> Settled {
        let server = Server {
            conn: &self.conn,
            root: self.root,
        };
        let mut settled = settle(&server, control);
        settled.generation = control.generation;
        settled
    }
}
/// Tells the user about grabs another app holds, after the hotkey lock is released.
fn report(sink: &Sink, settled: Settled) {
    for message in settled.messages {
        eprintln!("X11 shortcuts: {message}");
        sink(HookMessage::Conflict(message));
    }
    if settled.capture_failed {
        sink(HookMessage::Captured {
            generation: settled.generation,
            key: None,
        });
    }
}
impl Drop for Hook {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        let wake = xproto::ClientMessageEvent::new(
            32,
            self.shared.wake,
            xproto::AtomEnum::STRING,
            [0_u32; 5],
        );
        let _ =
            self.shared
                .conn
                .send_event(false, self.shared.wake, xproto::EventMask::NO_EVENT, wake);
        let _ = self.shared.conn.flush();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = self.shared.conn.destroy_window(self.shared.wake);
        let _ = self.shared.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: Space or Esc stay claimed after dictation, blocking typing;
    // the main key is grabbed without its modifiers; capture leaves the
    // keyboard grabbed; a grab another app holds is counted as ours, or
    // reported on every press; capture waits forever for keys another app has.
    use super::*;
    use crate::hotkey_machine::KeyEvent;
    use std::cell::RefCell;

    /// An X server where another client already holds some grabs.
    #[derive(Default)]
    struct FakeServer {
        taken: Vec<(u8, ModMask)>,
        keyboard_taken: bool,
        grabbed: RefCell<Vec<(u8, ModMask)>>,
        keyboard: RefCell<bool>,
        attempts: RefCell<usize>,
    }
    impl GrabServer for FakeServer {
        fn grab_key(&self, key: u8, mask: ModMask) -> Result<(), String> {
            *self.attempts.borrow_mut() += 1;
            // AnyModifier conflicts with any existing grab of the key.
            let clash = self
                .taken
                .iter()
                .any(|&(k, m)| k == key && (m == mask || mask == ModMask::ANY));
            if clash {
                return Err("BadAccess".into());
            }
            self.grabbed.borrow_mut().push((key, mask));
            Ok(())
        }
        fn ungrab_key(&self, key: u8, mask: ModMask) {
            let mut grabbed = self.grabbed.borrow_mut();
            let before = grabbed.len();
            grabbed.retain(|&g| g != (key, mask));
            assert_eq!(grabbed.len() + 1, before, "released a grab it didn't hold");
        }
        fn grab_keyboard(&self) -> Result<(), String> {
            if self.keyboard_taken {
                return Err("AlreadyGrabbed".into());
            }
            *self.keyboard.borrow_mut() = true;
            Ok(())
        }
        fn ungrab_keyboard(&self) {
            assert!(
                self.keyboard.replace(false),
                "released a keyboard grab it didn't hold"
            );
        }
    }
    fn control(hotkey: &str) -> Control {
        Control {
            keys: HotkeyMachine::new(hotkey.parse().expect("hotkey")),
            generation: 0,
            wanted: Grabs::default(),
            held: Held::default(),
            reported: Vec::new(),
        }
    }
    fn code(vk: u8) -> u8 {
        keycode(vk).expect("keycode")
    }

    fn press(m: &mut HotkeyMachine, key: u8, down: bool) {
        m.process(KeyEvent {
            key,
            down,
            injected: false,
        });
    }

    #[test]
    fn grabs_follow_dictation_state() {
        let mut m = HotkeyMachine::default();
        assert!(desired(&m) == Grabs::default());
        press(&mut m, LCTRL, true);
        press(&mut m, LWIN, true);
        let holding = desired(&m);
        assert!(holding.space && holding.escape && !holding.keyboard);
        press(&mut m, SPACE, true);
        let locked = desired(&m);
        assert!(!locked.space && locked.escape);
        press(&mut m, ESCAPE, true);
        assert!(desired(&m) == Grabs::default());
        m.begin_capture();
        assert!(desired(&m).keyboard);
        m.cancel_capture();
        assert!(!desired(&m).keyboard);
    }

    #[test]
    fn chord_main_key_is_grabbed_with_its_modifiers() {
        let hotkey: Hotkey = "Ctrl+Shift+D".parse().expect("chord");
        assert_eq!(
            main_grab(&hotkey),
            Some((32 + 8, ModMask::CONTROL | ModMask::SHIFT))
        );
        assert_eq!(main_grab(&Hotkey::default()), None);
        let win: Hotkey = "Win+Alt+F9".parse().expect("chord");
        assert_eq!(main_grab(&win), Some((67 + 8, ModMask::M4 | ModMask::M1)));
    }

    #[test]
    fn a_main_key_another_app_holds_is_reported_once_and_not_claimed() {
        let d = code(b'D');
        let ctrl_shift = ModMask::CONTROL | ModMask::SHIFT;
        let server = FakeServer {
            taken: vec![(d, ctrl_shift)],
            ..FakeServer::default()
        };
        let mut c = control("Ctrl+Shift+D");
        let settled = settle(&server, &mut c);
        assert_eq!(settled.messages.len(), 1, "{settled:?}");
        assert!(
            settled.messages[0].contains("Ctrl"),
            "{:?}",
            settled.messages
        );
        // Only the variants the server granted are held.
        assert_eq!(c.held.main.len(), 3);
        assert!(!c.held.main.contains(&(d, ctrl_shift)));
        assert_eq!(*server.grabbed.borrow(), c.held.main);
        // A key press re-settles without retrying or repeating the warning.
        let attempts = *server.attempts.borrow();
        press(&mut c.keys, LCTRL, true);
        assert!(settle(&server, &mut c).messages.is_empty());
        press(&mut c.keys, LCTRL, false);
        assert!(settle(&server, &mut c).messages.is_empty());
        assert_eq!(*server.attempts.borrow(), attempts);
    }

    #[test]
    fn space_falls_back_to_the_combinations_dictation_uses() {
        let space = code(SPACE);
        // Another app owns Shift + Space, so AnyModifier is refused.
        let server = FakeServer {
            taken: vec![(space, ModMask::SHIFT)],
            ..FakeServer::default()
        };
        let mut c = control("Ctrl+Win");
        assert!(settle(&server, &mut c).messages.is_empty());
        press(&mut c.keys, LCTRL, true);
        press(&mut c.keys, LWIN, true);
        let settled = settle(&server, &mut c);
        // Space with no modifiers and with Ctrl + Win was granted, so hands-free
        // works; nothing that app owns was claimed, and nothing to report.
        assert!(settled.messages.is_empty(), "{settled:?}");
        let held = &c.held.space;
        assert!(held.contains(&(space, ModMask::CONTROL | ModMask::M4)));
        assert!(held.contains(&(space, ModMask::from(0_u16))));
        assert!(!held.contains(&(space, ModMask::ANY)));
        // Releasing frees exactly the grabs held (the fake asserts on extras).
        press(&mut c.keys, ESCAPE, true);
        settle(&server, &mut c);
        assert!(c.held.space.is_empty() && c.held.escape.is_empty());
        assert!(server.grabbed.borrow().is_empty());
    }

    #[test]
    fn a_space_conflict_on_the_shortcut_modifiers_is_reported() {
        let space = code(SPACE);
        let server = FakeServer {
            taken: vec![(space, ModMask::CONTROL | ModMask::M4)],
            ..FakeServer::default()
        };
        let mut c = control("Ctrl+Win");
        press(&mut c.keys, LCTRL, true);
        press(&mut c.keys, LWIN, true);
        let settled = settle(&server, &mut c);
        assert_eq!(settled.messages.len(), 1, "{settled:?}");
        assert!(settled.messages[0].contains("Space"));
    }

    #[test]
    fn a_refused_keyboard_grab_ends_the_capture() {
        let server = FakeServer {
            keyboard_taken: true,
            ..FakeServer::default()
        };
        let mut c = control("Ctrl+Win");
        settle(&server, &mut c);
        c.keys.begin_capture();
        let settled = settle(&server, &mut c);
        assert!(settled.capture_failed);
        assert_eq!(settled.messages.len(), 1);
        assert!(!c.keys.capturing(), "keys would leak to the grabbing app");
        assert!(!c.held.keyboard);
        // Granted on the next try, and released when capture ends.
        let server = FakeServer::default();
        c.keys.begin_capture();
        assert!(!settle(&server, &mut c).capture_failed);
        assert!(c.held.keyboard && *server.keyboard.borrow());
        c.keys.cancel_capture();
        settle(&server, &mut c);
        assert!(!c.held.keyboard && !*server.keyboard.borrow());
    }
}
