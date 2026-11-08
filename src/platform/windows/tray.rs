//! Native notification icon, with Explorer restart recovery and a default Options item.
use super::files::wide;
use crate::{settings::Settings, transcribe::auth};
use std::{cell::RefCell, sync::Arc};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};
const CALLBACK: u32 = WM_APP + 1;
#[derive(Clone, Copy)]
pub enum TrayAction {
    Options,
    CopyLast,
    Help,
    Uninstall,
    Quit,
}
struct State {
    data: NOTIFYICONDATAW,
    settings: Settings,
    has_transcript: bool,
    taskbar_created: u32,
    sink: Arc<dyn Fn(TrayAction) + Send + Sync>,
}
pub struct Tray {
    hwnd: HWND,
    state: Box<RefCell<State>>,
}
fn copy_text<const N: usize>(target: &mut [u16; N], text: &str) {
    target.fill(0);
    for (slot, c) in target.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *slot = c;
    }
}
impl Tray {
    pub fn new(
        settings: Settings,
        sink: Arc<dyn Fn(TrayAction) + Send + Sync>,
    ) -> windows::core::Result<Self> {
        // SAFETY: class, callback and icon resource remain valid for this process.
        unsafe {
            let module = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: module.into(),
                lpszClassName: w!("WispX.Tray"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let icon = LoadIconW(Some(module.into()), PCWSTR(1_u16 as usize as *const u16))?;
            let mut state = Box::new(RefCell::new(State {
                data: NOTIFYICONDATAW {
                    cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                    uID: 1,
                    uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                    uCallbackMessage: CALLBACK,
                    hIcon: icon,
                    ..Default::default()
                },
                settings,
                has_transcript: false,
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                sink,
            }));
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("WispX.Tray"),
                w!("WispX"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(module.into()),
                Some((&mut *state as *mut RefCell<State>).cast()),
            )?;
            state.borrow_mut().data.hWnd = hwnd;
            let tray = Self { hwnd, state };
            tray.update_icon(NIM_ADD);
            Ok(tray)
        }
    }
    fn update_icon(&self, operation: NOTIFY_ICON_MESSAGE) {
        let mut state = self.state.borrow_mut();
        let text = format!("WispX · hold {}", state.settings.hotkey);
        copy_text(&mut state.data.szTip, &text);
        // SAFETY: cbSize and all icon fields are initialized for Shell_NotifyIconW.
        unsafe {
            let _ = Shell_NotifyIconW(operation, &state.data);
        }
    }
    pub fn update(&self, settings: &Settings, has_transcript: bool) {
        {
            let mut state = self.state.borrow_mut();
            state.settings = settings.clone();
            state.has_transcript = has_transcript;
        }
        self.update_icon(NIM_MODIFY);
    }
    pub fn balloon(&self, title: &str, message: &str, milliseconds: u32) {
        let mut state = self.state.borrow_mut();
        let mut data = state.data;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_NONE;
        data.Anonymous.uTimeout = milliseconds;
        copy_text(&mut data.szInfoTitle, title);
        copy_text(&mut data.szInfo, message);
        // SAFETY: this copy refers to our live notification icon and contains bounded UTF-16 strings.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
        }
        state.data.szInfo.fill(0);
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: our window and icon are still alive until this call.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.state.borrow().data);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: WM_NCCREATE supplies CREATESTRUCTW; user data points to the stable boxed RefCell owned by Tray.
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(l.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if !pointer.is_null() {
            let state = &*pointer;
            if msg == state.borrow().taskbar_created {
                let _ = Shell_NotifyIconW(NIM_ADD, &state.borrow().data);
            }
            if msg == CALLBACK {
                match l.0 as u32 {
                    WM_LBUTTONUP => (state.borrow().sink)(TrayAction::Options),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        let _ = menu(hwnd, &state.borrow());
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, msg, w, l)
    }
}
fn menu(hwnd: HWND, state: &State) -> windows::core::Result<()> {
    // SAFETY: the popup owns copied menu strings and is destroyed after TrackPopupMenu returns.
    unsafe {
        let menu = CreatePopupMenu()?;
        let title = wide(format!("WispX · hold {}", state.settings.hotkey));
        AppendMenuW(menu, MF_STRING | MF_DISABLED, 1, PCWSTR(title.as_ptr()))?;
        AppendMenuW(
            menu,
            MF_STRING | MF_DISABLED,
            2,
            if auth::login().is_some() {
                w!("✓ Using your ChatGPT login from Codex")
            } else {
                w!("⚠ Not signed in. Open Codex and sign in")
            },
        )?;
        AppendMenuW(menu, MF_SEPARATOR, 0, None)?;
        AppendMenuW(
            menu,
            if state.has_transcript {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            },
            3,
            w!("Copy last transcript"),
        )?;
        AppendMenuW(menu, MF_STRING, 4, w!("Options…"))?;
        SetMenuDefaultItem(menu, 4, 0)?;
        AppendMenuW(menu, MF_SEPARATOR, 0, None)?;
        AppendMenuW(menu, MF_STRING, 5, w!("How to use"))?;
        AppendMenuW(menu, MF_STRING, 6, w!("Uninstall WispX…"))?;
        AppendMenuW(menu, MF_STRING, 7, w!("Quit WispX"))?;
        let mut point = Default::default();
        GetCursorPos(&mut point)?;
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd,
            None,
        )
        .0;
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        DestroyMenu(menu)?;
        if let Some(action) = match command {
            3 => Some(TrayAction::CopyLast),
            4 => Some(TrayAction::Options),
            5 => Some(TrayAction::Help),
            6 => Some(TrayAction::Uninstall),
            7 => Some(TrayAction::Quit),
            _ => None,
        } {
            (state.sink)(action);
        }
        Ok(())
    }
}
