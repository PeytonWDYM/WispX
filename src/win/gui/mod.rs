//! The Windows desktop app: the pill, the tray icon and the settings window, in plain
//! Win32. As with the Qt one on Linux, this only draws. All of what it shows comes as
//! JSON from `crate::app`, and what people do goes straight back there.
//!
//! Windows may only be touched from the thread that made them, so the controller's
//! calls leave what they carry in a mailbox and post a message. Everything else runs in
//! window procedures, which can be re-entered whenever a system call pumps messages, so
//! no borrow of state is ever held across one of those.

mod pill;
mod settings;
mod tray;

use super::ffi::*;
use super::install;
use crate::app::controller::{self, Controller, Msg, Options, Ui};
use crate::app::instance;
use crate::cli;
use crate::paths;
use crate::sys;
use pill::PillWindow;
use serde_json::{Value, json};
use settings::Settings;
use std::cell::Cell;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;
use tray::Tray;

const MAIN_CLASS: &str = "WispXMain";
const WM_WAKE: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
/// Windows ends the process when the session-ending message has been answered.
const END_SESSION_GRACE: Duration = Duration::from_secs(3);
/// The icon's ID in the resources that `build.rs` links in.
const ICON_ID: usize = 1;

#[derive(Default)]
struct Mailbox {
    overlay: Option<String>,
    state: Option<String>,
    tray: Option<String>,
    show: bool,
    quit: bool,
}

/// What the controller has of the interface. Only the latest of each thing matters.
pub struct WinUi {
    main: SendHwnd,
    pill: SendHwnd,
    settings: SendHwnd,
    mailbox: Mutex<Mailbox>,
    level: AtomicU32,
}

impl WinUi {
    fn post(&self, update: impl FnOnce(&mut Mailbox)) {
        update(&mut self.mailbox.lock().unwrap_or_else(PoisonError::into_inner));
        // SAFETY: posting is allowed from any thread, to a window that may have gone.
        unsafe {
            PostMessageW(self.main.0, WM_WAKE, 0, 0);
        }
    }

    fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }
}

impl Ui for WinUi {
    fn set_overlay(&self, json: &str) {
        self.post(|mailbox| mailbox.overlay = Some(json.to_string()));
    }

    fn set_state(&self, json: &str) {
        self.post(|mailbox| mailbox.state = Some(json.to_string()));
    }

    fn set_tray(&self, json: &str) {
        self.post(|mailbox| mailbox.tray = Some(json.to_string()));
    }

    fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }

    fn show_settings(&self, _activation_token: Option<&str>) {
        self.post(|mailbox| mailbox.show = true);
    }

    fn inspect(&self) -> String {
        let describe = |window: SendHwnd| {
            let mut rect = Rect::default();
            // SAFETY: both only read, which is allowed from any thread.
            let visible = unsafe {
                GetWindowRect(window.0, &raw mut rect);
                IsWindowVisible(window.0) != 0
            };
            json!({
                "visible": visible,
                "x": rect.left,
                "y": rect.top,
                "width": rect.right - rect.left,
                "height": rect.bottom - rect.top,
            })
        };
        json!({
            "platform": "windows",
            "overlay_available": true,
            "overlay": describe(self.pill),
            "settings": describe(self.settings),
            "level": self.level(),
        })
        .to_string()
    }

    fn quit(&self) {
        self.post(|mailbox| mailbox.quit = true);
    }
}

/// Everything the window procedures reach, set once for the life of the main thread.
struct App {
    ui: Arc<WinUi>,
    tx: Sender<Msg>,
    pill: PillWindow,
    tray: Tray,
    settings: Settings,
    taskbar_created: u32,
}

thread_local! {
    static APP: Cell<Option<&'static App>> = const { Cell::new(None) };
}

fn app() -> Option<&'static App> {
    APP.with(Cell::get)
}

impl App {
    fn act(&self, name: &str, args: Value) {
        let _ = self.tx.send(Msg::Action {
            name: name.to_string(),
            args,
        });
    }

    fn take_mail(&self) {
        let mail = std::mem::take(
            &mut *self
                .ui
                .mailbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if let Some(json) = mail.overlay {
            self.pill.set_overlay(&json, self.ui.level());
        }
        if let Some(json) = mail.tray {
            self.tray.update(&json);
        }
        if let Some(json) = mail.state {
            self.settings.update(&json);
        }
        if mail.show {
            self.settings.show();
            self.act("settings-opened", json!({}));
        }
        if mail.quit {
            self.tray.remove();
            // SAFETY: takes a plain value, on the thread running the message loop.
            unsafe {
                PostQuitMessage(0);
            }
        }
    }
}

unsafe extern "system" fn main_proc(
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if let Some(app) = app() {
        match message {
            WM_WAKE => {
                app.take_mail();
                return 0;
            }
            WM_TRAY => {
                app.tray.on_event(app, lparam);
                return 0;
            }
            WM_QUERYENDSESSION => return 1,
            WM_ENDSESSION => {
                if wparam != 0 {
                    sys::request_shutdown();
                    thread::sleep(END_SESSION_GRACE);
                }
                return 0;
            }
            _ if message == app.taskbar_created => {
                // Explorer restarted, and the tray it had is gone.
                let available = app.tray.add();
                app.act("tray-status", json!({ "available": available }));
                return 0;
            }
            _ => {}
        }
    }
    // SAFETY: passes on what it was given, for a window of this thread.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// Registers a window class. A failure shows up when the window can't be made.
fn register_class(name: &str, proc: WndProc, icon: Hicon, small_icon: Hicon, background: bool) {
    let name = wide(name);
    let class = WndClass {
        size: size_of::<WndClass>() as u32,
        style: if background {
            CS_HREDRAW | CS_VREDRAW
        } else {
            0
        },
        proc: Some(proc),
        class_extra: 0,
        window_extra: 0,
        instance: instance_handle(),
        icon,
        // SAFETY: the name is one of the system's own numbered cursors.
        cursor: unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW as *const u16) },
        background: if background {
            (COLOR_WINDOW as usize + 1) as Hgdi
        } else {
            ptr::null_mut()
        },
        menu_name: ptr::null(),
        class_name: name.as_ptr(),
        small_icon,
    };
    // SAFETY: the structure is complete, and the system copies the name.
    unsafe {
        RegisterClassExW(&raw const class);
    }
}

fn instance_handle() -> Handle {
    // SAFETY: a null name asks for this program, whose handle needs no closing.
    unsafe { GetModuleHandleW(ptr::null()) }
}

/// The program's icon at this size, from its resources.
fn load_icon(size: i32) -> Hicon {
    // SAFETY: a small number stands for a resource's name here. The icon stays loaded for
    // the life of the program.
    unsafe {
        LoadImageW(
            instance_handle(),
            ICON_ID as *const u16,
            IMAGE_ICON,
            size,
            size,
            0,
        )
    }
}

fn message_box(owner: Hwnd, text: &str, title: &str, kind: u32) -> i32 {
    let text = wide(text);
    let title = wide(title);
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe { MessageBoxW(owner, text.as_ptr(), title.as_ptr(), kind) }
}

/// With no console to print to, a failure at the start would otherwise look like nothing
/// at all. It goes to stderr too, for the launcher to show.
fn fatal(message: &str, background: bool) -> i32 {
    eprintln!("wispx-gui: {message}");
    if !background {
        message_box(ptr::null_mut(), message, "WispX", MB_OK | MB_ICONERROR);
    }
    1
}

struct Arguments {
    options: Options,
    portable: bool,
}

fn parse_args(args: Vec<OsString>) -> Result<Arguments, String> {
    let mut parsed = Arguments {
        options: Options {
            config: None,
            background: false,
        },
        portable: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--background") => parsed.options.background = true,
            Some("--portable") => parsed.portable = true,
            Some("--config") => {
                let path = args.next().ok_or("--config needs a file")?;
                parsed.options.config = Some(PathBuf::from(path));
            }
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        }
    }
    Ok(parsed)
}

pub fn run() -> i32 {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    // One file is enough to run everything: it can be its own daemon.
    if args.iter().any(|arg| arg == "daemon") {
        return cli::main();
    }
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return fatal(&error, false),
    };
    let background = parsed.options.background;

    // SAFETY: the manifest already asks for this, and failing leaves things as they were.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // Before looking for a running copy, because replacing that is how updates work.
    let keep_here = parsed.portable || parsed.options.config.is_some();
    if install::ensure_installed(keep_here, background) {
        return 0;
    }
    let runtime = match paths::runtime_dir() {
        Ok(runtime) => runtime,
        Err(error) => return fatal(&error, background),
    };
    let lock = match instance::acquire(&runtime) {
        Ok(Some(lock)) => lock,
        // Already open: nobody else is going to bring it forward.
        Ok(None) => {
            if !background {
                let _ = instance::send("show");
            }
            return 0;
        }
        Err(error) => return fatal(&error, background),
    };
    install::repair();
    sys::install_shutdown_handlers();

    let (tx, inbox) = mpsc::channel();
    if let Err(error) = controller::serve_instance(&runtime, tx.clone()) {
        return fatal(&error, background);
    }

    let controls = InitCommonControls {
        size: size_of::<InitCommonControls>() as u32,
        classes: ICC_STANDARD_CLASSES | ICC_PROGRESS_CLASS,
    };
    // SAFETY: the structure is complete. COM is for the file dialog, which is fine
    // without it failing to set up here.
    unsafe {
        InitCommonControlsEx(&raw const controls);
        CoInitializeEx(
            ptr::null_mut(),
            COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
        );
    }

    match start_interface(tx, inbox, parsed.options) {
        Ok(()) => {}
        Err(error) => return fatal(&error, background),
    }
    let code = message_loop();
    // The lock goes with the process, and with it an owned daemon's stdin.
    drop(lock);
    code
}

fn start_interface(
    tx: Sender<Msg>,
    inbox: mpsc::Receiver<Msg>,
    options: Options,
) -> Result<(), String> {
    let icon = load_icon(32);
    let small_icon = load_icon(16);
    register_class(MAIN_CLASS, main_proc, icon, small_icon, false);
    let class = wide(MAIN_CLASS);
    let title = wide("WispX");
    // Not a message-only window: those don't hear of Explorer restarting or the session
    // ending. It has no taskbar entry, and is never shown.
    // SAFETY: the strings are NUL-terminated and the class was registered above.
    let main = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance_handle(),
            ptr::null(),
        )
    };
    if main.is_null() {
        return Err(format!(
            "cannot create a window: {}",
            std::io::Error::last_os_error()
        ));
    }
    let pill = PillWindow::create()?;
    let settings = Settings::create(icon, small_icon)?;
    let ui = Arc::new(WinUi {
        main: SendHwnd(main),
        pill: SendHwnd(pill.hwnd()),
        settings: SendHwnd(settings.hwnd()),
        mailbox: Mutex::default(),
        level: AtomicU32::new(0),
    });
    let taskbar_created = wide("TaskbarCreated");
    let tray = Tray::new(main, WM_TRAY);
    let tray_available = tray.add();
    let app: &'static App = Box::leak(Box::new(App {
        ui: Arc::clone(&ui),
        tx: tx.clone(),
        pill,
        tray,
        settings,
        // SAFETY: the name is NUL-terminated.
        taskbar_created: unsafe { RegisterWindowMessageW(taskbar_created.as_ptr()) },
    }));
    APP.with(|cell| cell.set(Some(app)));

    let controller = Controller::new(ui, tx.clone(), options);
    thread::spawn(move || controller.run(&inbox));
    let _ = tx.send(Msg::UiReady {
        overlay_available: true,
        tray_available,
    });
    Ok(())
}

fn message_loop() -> i32 {
    let Some(app) = app() else {
        return 1;
    };
    let settings = app.settings.hwnd();
    // SAFETY: a zeroed message is only a place for the system to write to, and everything
    // here is for this thread's windows.
    unsafe {
        let mut message: super::ffi::Msg = std::mem::zeroed();
        while GetMessageW(&raw mut message, ptr::null_mut(), 0, 0) > 0 {
            // Tab, Esc and the arrow keys, for the settings window's controls.
            if IsDialogMessageW(settings, &raw mut message) != 0 {
                continue;
            }
            TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
    0
}
