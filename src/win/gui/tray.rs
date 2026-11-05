//! The notification area icon and its menu. What the menu says comes from
//! `crate::app::view::tray`.

use super::App;
use crate::win::ffi::*;
use crate::win::install;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::ptr;

const ICON_NUMBER: u32 = 1;
/// Menu entries are numbered from their place in the list, far below this one.
const UNINSTALL: i32 = 1000;

#[derive(Clone, Debug, Default)]
struct Item {
    id: String,
    label: String,
    enabled: bool,
    checkable: bool,
    checked: bool,
    separator: bool,
}

fn parse_items(state: &Value) -> Vec<Item> {
    let text = |item: &Value, key: &str| item[key].as_str().unwrap_or_default().to_string();
    let flag = |item: &Value, key: &str| item[key].as_bool().unwrap_or(false);
    state["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| Item {
                    id: text(item, "id"),
                    label: text(item, "label"),
                    enabled: flag(item, "enabled"),
                    checkable: flag(item, "checkable"),
                    checked: flag(item, "checked"),
                    separator: flag(item, "separator"),
                })
                .collect()
        })
        .unwrap_or_default()
}

struct Inner {
    tooltip: String,
    items: Vec<Item>,
    added: bool,
}

pub struct Tray {
    hwnd: Hwnd,
    callback: u32,
    icon: Hicon,
    inner: RefCell<Inner>,
}

impl Tray {
    pub fn new(hwnd: Hwnd, callback: u32) -> Tray {
        // SAFETY: only reads the window's screen, for how large the icon should be.
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as i32;
        Tray {
            hwnd,
            callback,
            // SAFETY: plain values.
            icon: super::load_icon(unsafe { MulDiv(16, dpi, 96) }),
            inner: RefCell::new(Inner {
                tooltip: "WispX".to_string(),
                items: Vec::new(),
                added: false,
            }),
        }
    }

    fn data(&self, flags: u32, tooltip: &str) -> NotifyIconData {
        let mut tip = [0u16; 128];
        for (place, unit) in tip[..127].iter_mut().zip(tooltip.encode_utf16()) {
            *place = unit;
        }
        // SAFETY: the structure holds only numbers, arrays and pointers that may be null.
        let mut data: NotifyIconData = unsafe { std::mem::zeroed() };
        data.size = size_of::<NotifyIconData>() as u32;
        data.hwnd = self.hwnd;
        data.id = ICON_NUMBER;
        data.flags = flags;
        data.callback = self.callback;
        data.icon = self.icon;
        data.tip = tip;
        data.version = NOTIFYICON_VERSION_4;
        data
    }

    /// Puts the icon up, whether for the first time or because the tray was restarted.
    pub fn add(&self) -> bool {
        let tooltip = self.inner.borrow().tooltip.clone();
        let data = self.data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP, &tooltip);
        // SAFETY: the structure is complete and outlives the calls.
        let added = unsafe {
            let added = Shell_NotifyIconW(NIM_ADD, &raw const data) != 0;
            if added {
                Shell_NotifyIconW(NIM_SETVERSION, &raw const data);
            }
            added
        };
        self.inner.borrow_mut().added = added;
        added
    }

    pub fn remove(&self) {
        let added = std::mem::take(&mut self.inner.borrow_mut().added);
        if added {
            let data = self.data(0, "");
            // SAFETY: the structure is complete and outlives the call.
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &raw const data);
            }
        }
    }

    pub fn update(&self, json: &str) {
        let Ok(state) = serde_json::from_str::<Value>(json) else {
            return;
        };
        let tooltip = state["tooltip"].as_str().unwrap_or("WispX").to_string();
        let changed = {
            let mut inner = self.inner.borrow_mut();
            inner.items = parse_items(&state);
            let changed = inner.added && inner.tooltip != tooltip;
            inner.tooltip.clone_from(&tooltip);
            changed
        };
        if changed {
            let data = self.data(NIF_TIP | NIF_SHOWTIP, &tooltip);
            // SAFETY: the structure is complete and outlives the call.
            unsafe {
                Shell_NotifyIconW(NIM_MODIFY, &raw const data);
            }
        }
    }

    pub fn on_event(&self, app: &App, lparam: isize) {
        // With version 4, the event is in the low word.
        match (lparam & 0xFFFF) as u32 {
            NIN_SELECT | NIN_KEYSELECT => app.act("show-settings", json!({})),
            WM_CONTEXTMENU => self.show_menu(app),
            _ => {}
        }
    }

    fn show_menu(&self, app: &App) {
        let items = self.inner.borrow().items.clone();
        let offer_uninstall = install::is_installed_copy();
        // SAFETY: the menu is made and destroyed here, and its strings outlive the calls
        // that copy them. Going foreground first, and posting a message after, are what
        // the system asks for so that clicking elsewhere closes the menu.
        let chosen = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            for (index, item) in items.iter().enumerate() {
                if item.id == "quit" && offer_uninstall {
                    append(menu, UNINSTALL as usize, "Uninstall WispX…", MF_STRING);
                }
                if item.separator {
                    AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
                    continue;
                }
                let mut flags = MF_STRING;
                if !item.enabled {
                    flags |= MF_GRAYED;
                }
                if item.checkable && item.checked {
                    flags |= MF_CHECKED;
                }
                append(menu, index + 1, &item.label, flags);
            }
            let mut point = Point::default();
            GetCursorPos(&raw mut point);
            SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                point.x,
                point.y,
                self.hwnd,
                ptr::null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            chosen
        };
        if chosen == UNINSTALL {
            uninstall(app);
        } else if chosen > 0
            && let Some(item) = items.get(chosen as usize - 1)
        {
            let args = if item.checkable {
                json!({ "checked": !item.checked })
            } else {
                json!({})
            };
            app.act(&item.id, args);
        }
    }
}

/// `&` would otherwise underline the next letter.
unsafe fn append(menu: Hmenu, id: usize, label: &str, flags: u32) {
    let label = wide(&label.replace('&', "&&"));
    // SAFETY: the caller's menu is valid, and the system copies the string.
    unsafe {
        AppendMenuW(menu, flags, id, label.as_ptr());
    }
}

fn uninstall(app: &App) {
    let answer = super::message_box(
        ptr::null_mut(),
        "Remove WispX from this PC, along with its settings and downloaded models?\n\nYour \
         Codex/ChatGPT login isn't touched.",
        "Uninstall WispX",
        MB_OKCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2,
    );
    if answer != 1 {
        return;
    }
    match install::uninstall() {
        Ok(()) => app.act("quit", json!({})),
        Err(error) => {
            super::message_box(ptr::null_mut(), &error, "WispX", MB_OK | MB_ICONERROR);
        }
    }
}
