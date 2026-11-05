//! The desktop app: a Qt window system on the main thread, and [`Controller`]
//! everywhere else. This file is only the join between them, and the one place in
//! the Rust code that talks to C++.

use serde_json::Value;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, PoisonError};
use std::thread;
use wispx::app::controller::{self, Controller, Msg, Options, Ui};
use wispx::app::{install, instance};
use wispx::paths;
use wispx::portal::APP_ID;
use wispx::sys;

const OVERLAY_QML: &str = include_str!("../ui/Overlay.qml");
const SETTINGS_QML: &str = include_str!("../ui/Settings.qml");

type ActionFn = unsafe extern "C" fn(*const c_char, *const c_char, *mut c_void);
type ReadyFn = unsafe extern "C" fn(c_int, c_int, *mut c_void);

#[repr(C)]
struct Params {
    overlay_qml: *const c_char,
    overlay_qml_len: usize,
    settings_qml: *const c_char,
    settings_qml_len: usize,
    icon_png: *const u8,
    icon_png_len: usize,
    app_id: *const c_char,
    on_action: ActionFn,
    on_ready: ReadyFn,
    user: *mut c_void,
}

unsafe extern "C" {
    fn wispx_ui_run(params: *const Params) -> c_int;
    fn wispx_ui_set_overlay(json: *const c_char);
    fn wispx_ui_set_state(json: *const c_char);
    fn wispx_ui_set_tray(json: *const c_char);
    fn wispx_ui_set_level(level: f32);
    fn wispx_ui_show_settings(activation_token: *const c_char);
    fn wispx_ui_quit();
    fn wispx_ui_inspect() -> *mut c_char;
    fn wispx_ui_free(text: *mut c_char);
}

/// Text with an interior NUL can't cross, and nothing here produces any.
fn c_text(text: &str) -> CString {
    CString::new(text.replace('\0', "")).unwrap_or_default()
}

struct QtUi;

impl Ui for QtUi {
    fn set_overlay(&self, json: &str) {
        let text = c_text(json);
        // SAFETY: the pointer is valid for the call, and the function copies it.
        unsafe { wispx_ui_set_overlay(text.as_ptr()) }
    }

    fn set_state(&self, json: &str) {
        let text = c_text(json);
        // SAFETY: as above.
        unsafe { wispx_ui_set_state(text.as_ptr()) }
    }

    fn set_tray(&self, json: &str) {
        let text = c_text(json);
        // SAFETY: as above.
        unsafe { wispx_ui_set_tray(text.as_ptr()) }
    }

    fn set_level(&self, level: f32) {
        // SAFETY: takes a plain value, from any thread.
        unsafe { wispx_ui_set_level(level) }
    }

    fn show_settings(&self, activation_token: Option<&str>) {
        let token = activation_token.map(c_text);
        let pointer = token
            .as_ref()
            .map_or(std::ptr::null(), |token| token.as_ptr());
        // SAFETY: null is allowed; otherwise valid for the call, and it copies it.
        unsafe { wispx_ui_show_settings(pointer) }
    }

    fn inspect(&self) -> String {
        // SAFETY: returns a NUL-terminated string this side frees below, once.
        unsafe {
            let text = wispx_ui_inspect();
            if text.is_null() {
                return String::new();
            }
            let copy = CStr::from_ptr(text).to_string_lossy().into_owned();
            wispx_ui_free(text);
            copy
        }
    }

    fn quit(&self) {
        // SAFETY: only posts to the Qt thread, if it is still running.
        unsafe { wispx_ui_quit() }
    }
}

struct Shared {
    tx: Sender<Msg>,
    /// Handed to the controller once, when the windows exist.
    start: Mutex<Option<(Receiver<Msg>, Options)>>,
}

unsafe extern "C" fn on_action(name: *const c_char, args: *const c_char, user: *mut c_void) {
    // SAFETY: Qt passes NUL-terminated strings valid for the call, and `user` is
    // the `Shared` that outlives the event loop that calls this.
    let (name, args, shared) = unsafe {
        (
            CStr::from_ptr(name).to_string_lossy().into_owned(),
            CStr::from_ptr(args).to_string_lossy(),
            &*user.cast::<Shared>(),
        )
    };
    let args = serde_json::from_str(&args).unwrap_or(Value::Null);
    let _ = shared.tx.send(Msg::Action { name, args });
}

unsafe extern "C" fn on_ready(overlay: c_int, tray: c_int, user: *mut c_void) {
    // SAFETY: as in `on_action`.
    let shared = unsafe { &*user.cast::<Shared>() };
    let taken = shared
        .start
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some((inbox, options)) = taken {
        let controller = Controller::new(Arc::new(QtUi), shared.tx.clone(), options);
        thread::spawn(move || controller.run(&inbox));
    }
    let _ = shared.tx.send(Msg::UiReady {
        overlay_available: overlay != 0,
        tray_available: tray != 0,
    });
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        config: None,
        background: false,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--background") => options.background = true,
            Some("--config") => {
                let path = args.next().ok_or("--config needs a file")?;
                options.config = Some(PathBuf::from(path));
            }
            _ => return Err(format!("unknown argument {}", arg.to_string_lossy())),
        }
    }
    Ok(options)
}

pub fn run() -> i32 {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("wispx-gui: {error}");
            return 2;
        }
    };
    let runtime = match paths::runtime_dir() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("wispx-gui: {error}");
            return 1;
        }
    };
    let lock = match instance::acquire(&runtime) {
        Ok(Some(lock)) => lock,
        // Already open: the launcher's `show` will reach that one.
        Ok(None) => return 0,
        Err(error) => {
            eprintln!("wispx-gui: {error}");
            return 1;
        }
    };
    install::repair();
    sys::install_shutdown_handlers();

    let (tx, inbox) = mpsc::channel();
    if let Err(error) = controller::serve_instance(&runtime, tx.clone()) {
        eprintln!("wispx-gui: {error}");
        return 1;
    }
    let shared = Box::new(Shared {
        tx,
        start: Mutex::new(Some((inbox, options))),
    });

    let app_id = c_text(APP_ID);
    let params = Params {
        overlay_qml: OVERLAY_QML.as_ptr().cast(),
        overlay_qml_len: OVERLAY_QML.len(),
        settings_qml: SETTINGS_QML.as_ptr().cast(),
        settings_qml_len: SETTINGS_QML.len(),
        icon_png: install::ICON_PNG.as_ptr(),
        icon_png_len: install::ICON_PNG.len(),
        app_id: app_id.as_ptr(),
        on_action,
        on_ready,
        user: std::ptr::from_ref(shared.as_ref()).cast_mut().cast(),
    };
    // SAFETY: everything `params` points to outlives this call, which runs the Qt
    // event loop on the main thread and returns after it ends.
    let code = unsafe { wispx_ui_run(&raw const params) };
    // The lock goes with the process, and its end is what ends an owned daemon if
    // the event loop stopped on its own, such as when the compositor went away.
    drop(lock);
    code
}
