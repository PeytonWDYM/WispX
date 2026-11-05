//! The settings window, in the system's own controls. It reads top to bottom like
//! `ui/Settings.qml`, from the same JSON, and every control is kept by a name and reused
//! when the state changes, so focus and open dropdowns survive.

use super::register_class;
use crate::win::ffi::*;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr;

const CLASS: &str = "WispXSettings";
const WIDTH: i32 = 560;
const HEIGHT: i32 = 720;
const MIN_WIDTH: i32 = 440;
const MIN_HEIGHT: i32 = 360;
const MARGIN: i32 = 24;
const SECTION_GAP: i32 = 16;
const BUTTON_HEIGHT: i32 = 28;
const COMBO_HEIGHT: i32 = 26;
/// A dropdown's window includes its list.
const COMBO_LIST_HEIGHT: i32 = 260;
const LABEL_COLUMN: i32 = 110;
const FIRST_CONTROL_ID: usize = 100;
const LINE_SCROLL: i32 = 24;
const WHEEL_SCROLL: i32 = 72;
const WHEEL_NOTCH: i32 = 120;
const FILE_BUFFER: usize = 32 * 1024;

const COLOR_OK: u32 = rgb(0x2E, 0xA0, 0x43);
const COLOR_WARN: u32 = rgb(0xD2, 0x99, 0x22);
const COLOR_FAIL: u32 = rgb(0xE5, 0x53, 0x4B);
const COLOR_BUSY: u32 = rgb(0x58, 0xA6, 0xFF);
const COLOR_WAIT: u32 = rgb(0x8B, 0x94, 0x9E);
/// Secondary text, which the Qt window draws at 80%.
const COLOR_DIM: u32 = rgb(0x57, 0x60, 0x6A);
const DOT: &str = "●";

const BACKENDS: [&str; 2] = [
    "On this computer (whisper.cpp)",
    "ChatGPT (sends audio online)",
];

const CONSENT_TITLE: &str = "Send your dictation to ChatGPT?";
const CONSENT_TEXT: &str = "Your recorded audio will go to chatgpt.com using your Codex login, \
     through a private endpoint that can change without notice. On this computer, nothing \
     leaves your machine.\n\nChoose Yes to use ChatGPT, or No to stay on this computer.";

const fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    red as u32 | (green as u32) << 8 | (blue as u32) << 16
}

fn level_color(level: &str) -> u32 {
    match level {
        "ok" => COLOR_OK,
        "warn" => COLOR_WARN,
        "fail" => COLOR_FAIL,
        "busy" => COLOR_BUSY,
        _ => COLOR_WAIT,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Label,
    Button,
    Check,
    Combo,
    Edit,
    Progress,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Font {
    Normal,
    Bold,
    Title,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Backend,
    Model,
    Language,
    Device,
    /// Only remembered, until the Download button goes with it.
    Download,
}

/// What using a control does.
#[derive(Clone, Debug, PartialEq)]
enum Use {
    Nothing,
    Act(String, Value),
    ChooseModel,
    StartDownload,
    Toggle(&'static str),
    Choose(Choice),
}

struct Control {
    hwnd: Hwnd,
    id: usize,
    kind: Kind,
    text: String,
    rect: Rect,
    enabled: bool,
    font: Font,
    what: Use,
    items: Vec<String>,
    used: bool,
}

struct Fonts {
    normal: Hgdi,
    bold: Hgdi,
    title: Hgdi,
}

impl Fonts {
    fn new(dpi: u32) -> Fonts {
        let face = wide("Segoe UI");
        let make = |pixels: i32, weight: i32| {
            // SAFETY: the name is NUL-terminated, and the rest are plain values.
            unsafe {
                CreateFontW(
                    -MulDiv(pixels, dpi as i32, 96),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    0,
                    0,
                    0,
                    0,
                    face.as_ptr(),
                )
            }
        };
        Fonts {
            normal: make(13, FW_NORMAL),
            bold: make(13, FW_SEMIBOLD),
            title: make(18, FW_SEMIBOLD),
        }
    }

    fn get(&self, font: Font) -> Hgdi {
        match font {
            Font::Normal => self.normal,
            Font::Bold => self.bold,
            Font::Title => self.title,
        }
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        // SAFETY: nothing is using these by the time they are replaced.
        unsafe {
            DeleteObject(self.normal);
            DeleteObject(self.bold);
            DeleteObject(self.title);
        }
    }
}

struct Inner {
    hwnd: Hwnd,
    state: Value,
    controls: HashMap<String, Control>,
    order: Vec<String>,
    last_order: Vec<String>,
    dpi: u32,
    fonts: Fonts,
    scroll: i32,
    content: i32,
    next_id: usize,
    placed: bool,
}

pub struct Settings {
    hwnd: Hwnd,
    inner: RefCell<Inner>,
    /// Text colors, kept apart since they are asked for while controls are being moved.
    colors: RefCell<HashMap<usize, u32>>,
}

/// Lets Tab move on from the multi-line box, and Esc close the window, as they do from
/// everything else. Enter still starts a new line.
unsafe extern "system" fn tab_moves_on(
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _data: usize,
) -> isize {
    // SAFETY: passes on what it was given, to the next handler for this window.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if message != WM_GETDLGCODE {
        return result;
    }
    let key = lparam as *const super::super::ffi::Msg;
    // SAFETY: for this message, `lparam` is either null or points to the message.
    let enter =
        !key.is_null() && unsafe { (*key).message == WM_KEYDOWN && (*key).wparam == VK_RETURN };
    if enter {
        result
    } else {
        result & !(DLGC_WANTTAB | DLGC_WANTALLKEYS)
    }
}

impl Inner {
    fn px(&self, logical: i32) -> i32 {
        // SAFETY: plain values.
        unsafe { MulDiv(logical, self.dpi as i32, 96) }
    }

    /// How much room text takes, wrapped to `width` if that is more than zero.
    fn measure(&self, text: &str, font: Font, width: i32) -> (i32, i32) {
        let units = wide(text);
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: width,
            bottom: 0,
        };
        let format = if width > 0 {
            DT_CALCRECT | DT_NOPREFIX | DT_WORDBREAK
        } else {
            DT_CALCRECT | DT_NOPREFIX | DT_SINGLELINE
        };
        // SAFETY: the context is released here, with its font put back first, and the
        // string and rectangle are local.
        unsafe {
            let dc = GetDC(self.hwnd);
            let previous = SelectObject(dc, self.fonts.get(font));
            DrawTextW(dc, units.as_ptr(), -1, &raw mut rect, format);
            SelectObject(dc, previous);
            ReleaseDC(self.hwnd, dc);
        }
        (rect.right, rect.bottom)
    }

    fn create_control(&mut self, kind: Kind, text: &str, rect: Rect) -> (Hwnd, usize) {
        let (class, style, ex_style) = match kind {
            Kind::Label => ("STATIC", SS_NOPREFIX, 0),
            Kind::Button => ("BUTTON", WS_TABSTOP | BS_PUSHBUTTON, 0),
            Kind::Check => ("BUTTON", WS_TABSTOP | BS_AUTOCHECKBOX, 0),
            Kind::Combo => (
                "COMBOBOX",
                WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST | CBS_HASSTRINGS,
                0,
            ),
            Kind::Edit => (
                "EDIT",
                WS_TABSTOP | WS_VSCROLL | ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN,
                WS_EX_CLIENTEDGE,
            ),
            Kind::Progress => ("msctls_progress32", PBS_SMOOTH, 0),
        };
        let id = self.next_id;
        self.next_id += 1;
        let class = wide(class);
        let text = wide(text);
        // SAFETY: the strings are NUL-terminated, and for a child window the menu argument
        // carries its number. The parent is this thread's own.
        let hwnd = unsafe {
            let hwnd = CreateWindowExW(
                ex_style,
                class.as_ptr(),
                text.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                self.hwnd,
                id as Hmenu,
                super::instance_handle(),
                ptr::null(),
            );
            if kind == Kind::Progress {
                SendMessageW(hwnd, PBM_SETRANGE32, 0, 100);
            }
            if kind == Kind::Edit {
                SetWindowSubclass(hwnd, tab_moves_on, 1, 0);
            }
            hwnd
        };
        (hwnd, id)
    }

    /// Finds or makes a control, and brings it up to date without touching what already
    /// is, so nothing repaints or loses its place for no reason.
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        key: &str,
        kind: Kind,
        rect: Rect,
        text: &str,
        enabled: bool,
        font: Font,
        what: Use,
    ) -> Hwnd {
        self.order.push(key.to_string());
        if !self.controls.contains_key(key) {
            let (hwnd, id) = self.create_control(kind, text, rect);
            // SAFETY: the window was just made, and the font lives as long as `fonts`.
            unsafe {
                SendMessageW(hwnd, WM_SETFONT, self.fonts.get(font) as usize, 1);
                if !enabled {
                    EnableWindow(hwnd, 0);
                }
            }
            self.controls.insert(
                key.to_string(),
                Control {
                    hwnd,
                    id,
                    kind,
                    text: text.to_string(),
                    rect,
                    enabled,
                    font,
                    what: what.clone(),
                    items: Vec::new(),
                    used: true,
                },
            );
        }
        let Some(control) = self.controls.get_mut(key) else {
            return ptr::null_mut();
        };
        control.used = true;
        control.what = what;
        // SAFETY: the window belongs to this thread, and the text outlives the call.
        unsafe {
            if control.rect != rect {
                control.rect = rect;
                SetWindowPos(
                    control.hwnd,
                    ptr::null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            if control.text != text && kind != Kind::Edit && kind != Kind::Combo {
                control.text = text.to_string();
                let units = wide(text);
                SetWindowTextW(control.hwnd, units.as_ptr());
            }
            if control.enabled != enabled {
                control.enabled = enabled;
                EnableWindow(control.hwnd, i32::from(enabled));
            }
        }
        control.hwnd
    }

    fn set_items(&mut self, key: &str, items: Vec<String>, selected: Option<i32>) {
        let Some(control) = self.controls.get_mut(key) else {
            return;
        };
        // SAFETY: the window belongs to this thread, and the strings are copied by it.
        unsafe {
            // Changing a list that is open under someone's pointer would be rude.
            if SendMessageW(control.hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0 {
                return;
            }
            if control.items != items {
                SendMessageW(control.hwnd, CB_RESETCONTENT, 0, 0);
                for item in &items {
                    let units = wide(item);
                    SendMessageW(control.hwnd, CB_ADDSTRING, 0, units.as_ptr() as isize);
                }
                control.items = items;
                if selected.is_none() {
                    SendMessageW(control.hwnd, CB_SETCURSEL, 0, 0);
                }
            }
            if let Some(index) = selected {
                SendMessageW(control.hwnd, CB_SETCURSEL, index as usize, 0);
            }
        }
    }

    fn drop_unused(&mut self) {
        let keys: Vec<String> = self
            .controls
            .iter()
            .filter(|(_, control)| !control.used)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            let Some(control) = self.controls.remove(&key) else {
                continue;
            };
            // SAFETY: the window belongs to this thread. Keys go to the window if the
            // control that had them is about to go.
            unsafe {
                if GetFocus() == control.hwnd {
                    SetFocus(self.hwnd);
                }
                DestroyWindow(control.hwnd);
            }
        }
    }

    /// Keeps Tab going down the window in the order things are read.
    fn fix_tab_order(&mut self) {
        if self.order == self.last_order {
            return;
        }
        let mut previous: Hwnd = ptr::null_mut();
        for key in &self.order {
            if let Some(control) = self.controls.get(key) {
                // SAFETY: the window belongs to this thread, and only its order changes.
                unsafe {
                    SetWindowPos(
                        control.hwnd,
                        previous,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
                previous = control.hwnd;
            }
        }
        self.last_order.clone_from(&self.order);
    }
}

fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
    Rect {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    }
}

fn find_index(list: &Value, key: &str, value: &Value) -> Option<i32> {
    list.as_array()?
        .iter()
        .position(|item| item[key] == *value)
        .map(|index| index as i32)
}

fn labels(list: &Value) -> Vec<String> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| item["label"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

impl Settings {
    pub fn create(icon: Hicon, small_icon: Hicon) -> Result<Settings, String> {
        register_class(CLASS, window_proc, icon, small_icon, true);
        let class = wide(CLASS);
        let title = wide("WispX");
        // SAFETY: the strings are NUL-terminated and the class was registered above.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_CONTROLPARENT,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VSCROLL,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                WIDTH,
                HEIGHT,
                ptr::null_mut(),
                ptr::null_mut(),
                super::instance_handle(),
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(format!(
                "cannot create the settings window: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: the window was just made.
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        Ok(Settings {
            hwnd,
            inner: RefCell::new(Inner {
                hwnd,
                state: Value::Null,
                controls: HashMap::new(),
                order: Vec::new(),
                last_order: Vec::new(),
                dpi,
                fonts: Fonts::new(dpi),
                scroll: 0,
                content: 0,
                next_id: FIRST_CONTROL_ID,
                placed: false,
            }),
            colors: RefCell::new(HashMap::new()),
        })
    }

    pub fn hwnd(&self) -> Hwnd {
        self.hwnd
    }

    fn visible(&self) -> bool {
        // SAFETY: only reads.
        unsafe { IsWindowVisible(self.hwnd) != 0 }
    }

    pub fn update(&self, json: &str) {
        let Ok(state) = serde_json::from_str(json) else {
            return;
        };
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.state = state;
        }
        if self.visible() {
            self.render();
        }
    }

    pub fn show(&self) {
        self.place_first_time();
        self.render();
        // SAFETY: the window belongs to this thread. Whether it may come forward is up to
        // the system, which allows it after a tray click or a launch that asked for it.
        unsafe {
            let command = if IsIconic(self.hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            ShowWindow(self.hwnd, command);
            SetForegroundWindow(self.hwnd);
        }
    }

    /// In the middle of the screen, at a size that fits it.
    fn place_first_time(&self) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        if std::mem::replace(&mut inner.placed, true) {
            return;
        }
        let (width, height) = (inner.px(WIDTH), inner.px(HEIGHT));
        drop(inner);
        // SAFETY: the structures are local and the window belongs to this thread.
        unsafe {
            let monitor = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            let mut info = MonitorInfo {
                size: size_of::<MonitorInfo>() as u32,
                ..MonitorInfo::default()
            };
            if GetMonitorInfoW(monitor, &raw mut info) == 0 {
                return;
            }
            let work = info.work;
            let width = width.min(work.right - work.left);
            let height = height.min(work.bottom - work.top);
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                work.left + (work.right - work.left - width) / 2,
                work.top + (work.bottom - work.top - height) / 2,
                width,
                height,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Everything on the window, from what the state says now.
    fn render(&self) {
        let Ok(mut guard) = self.inner.try_borrow_mut() else {
            return;
        };
        let inner = &mut *guard;
        let again = self.lay_out(inner);
        drop(guard);
        // Scrolled past the end of what is now shorter content.
        if again {
            self.render();
        }
    }

    /// Sets a label's text color. A label repaints only when told to, and this is all
    /// that changes on a dot going from one color to another.
    fn colored(&self, hwnd: Hwnd, color: u32) {
        if self.colors.borrow_mut().insert(hwnd as usize, color) != Some(color) {
            // SAFETY: the label belongs to this thread.
            unsafe {
                InvalidateRect(hwnd, ptr::null(), 1);
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn lay_out(&self, inner: &mut Inner) -> bool {
        for control in inner.controls.values_mut() {
            control.used = false;
        }
        inner.order.clear();
        let s = inner.state.clone();
        let ready = s["ready"].as_bool() == Some(true);
        let mut client = Rect::default();
        // SAFETY: the rectangle is local and the window belongs to this thread.
        unsafe {
            GetClientRect(self.hwnd, &raw mut client);
        }
        let left = inner.px(MARGIN);
        let width = (client.right - 2 * left).max(inner.px(200));
        let right = left + width;
        let gap = inner.px(SECTION_GAP);
        let small_gap = inner.px(4);
        let mut y = inner.px(20) - inner.scroll;

        // Header.
        let summary = s["summary"]["text"].as_str().unwrap_or("Getting ready…");
        let level = s["summary"]["level"].as_str().unwrap_or("wait");
        let dot_width = inner.px(22);
        let (_, height) = inner.measure(summary, Font::Title, width - dot_width);
        let dot = inner.put(
            "summary-dot",
            Kind::Label,
            rect(left, y, dot_width, height),
            DOT,
            true,
            Font::Title,
            Use::Nothing,
        );
        self.colored(dot, level_color(level));
        inner.put(
            "summary",
            Kind::Label,
            rect(left + dot_width, y, width - dot_width, height),
            summary,
            true,
            Font::Title,
            Use::Nothing,
        );
        y += height + gap;

        // Errors worth reading stay here until the next thing goes right.
        let problem = s["problem"]
            .as_str()
            .or_else(|| s["last_error"].as_str())
            .unwrap_or_default();
        if !problem.is_empty() {
            let (_, height) = inner.measure(problem, Font::Normal, width);
            let label = inner.put(
                "problem",
                Kind::Label,
                rect(left, y, width, height),
                problem,
                true,
                Font::Normal,
                Use::Nothing,
            );
            self.colored(label, COLOR_FAIL);
            y += height + gap;
        }

        // How to use, with the keys that are really bound.
        y = self.heading(inner, "how-to-use", "How to use", left, y, width);
        let line = inner.measure("Ag", Font::Normal, 0).1;
        let keys_width = (width * 2 / 5).max(inner.px(120));
        if let Some(shortcuts) = s["shortcuts"].as_array() {
            for (index, shortcut) in shortcuts.iter().enumerate() {
                let label = shortcut["label"].as_str().unwrap_or_default();
                let keys = shortcut["keys"].as_str().unwrap_or_default();
                let label_width = width - keys_width - inner.px(12);
                let (_, label_height) = inner.measure(label, Font::Normal, label_width);
                let (_, keys_height) = inner.measure(keys, Font::Bold, keys_width);
                inner.put(
                    &format!("shortcut-{index}"),
                    Kind::Label,
                    rect(left, y, label_width, label_height),
                    label,
                    true,
                    Font::Normal,
                    Use::Nothing,
                );
                inner.put(
                    &format!("keys-{index}"),
                    Kind::Label,
                    rect(right - keys_width, y, keys_width, keys_height),
                    keys,
                    true,
                    Font::Bold,
                    Use::Nothing,
                );
                y += label_height.max(keys_height).max(line) + inner.px(6);
            }
        }
        y += gap;

        // Readiness.
        if let Some(rows) = s["rows"].as_array() {
            let indent = inner.px(20);
            for row in rows {
                let id = row["id"].as_str().unwrap_or_default();
                let title = row["title"].as_str().unwrap_or_default();
                let detail = row["detail"].as_str().unwrap_or_default();
                let level = row["level"].as_str().unwrap_or("wait");
                let dot = inner.put(
                    &format!("row-{id}-dot"),
                    Kind::Label,
                    rect(left, y, indent, line),
                    DOT,
                    true,
                    Font::Normal,
                    Use::Nothing,
                );
                self.colored(dot, level_color(level));
                inner.put(
                    &format!("row-{id}-title"),
                    Kind::Label,
                    rect(left + indent, y, width - indent, line),
                    title,
                    true,
                    Font::Bold,
                    Use::Nothing,
                );
                y += line + small_gap;
                if !detail.is_empty() {
                    let (_, height) = inner.measure(detail, Font::Normal, width - indent);
                    let label = inner.put(
                        &format!("row-{id}-detail"),
                        Kind::Label,
                        rect(left + indent, y, width - indent, height),
                        detail,
                        true,
                        Font::Normal,
                        Use::Nothing,
                    );
                    self.colored(label, COLOR_DIM);
                    y += height + small_gap;
                }
                if let Some(buttons) = row["buttons"].as_array().filter(|list| !list.is_empty()) {
                    let mut x = left + indent;
                    let height = inner.px(BUTTON_HEIGHT);
                    for button in buttons {
                        let action = button["action"].as_str().unwrap_or_default();
                        let label = button["label"].as_str().unwrap_or_default();
                        let enabled = button["enabled"].as_bool().unwrap_or(true);
                        let wide_enough = inner.measure(label, Font::Normal, 0).0 + inner.px(24);
                        if x > left + indent && x + wide_enough > right {
                            x = left + indent;
                            y += height + small_gap;
                        }
                        let what = if action == "choose-model" {
                            Use::ChooseModel
                        } else {
                            Use::Act(action.to_string(), button["args"].clone())
                        };
                        inner.put(
                            &format!("row-{id}-{action}"),
                            Kind::Button,
                            rect(x, y, wide_enough, height),
                            label,
                            enabled,
                            Font::Normal,
                            what,
                        );
                        x += wide_enough + inner.px(8);
                    }
                    y += height + small_gap;
                }
                y += inner.px(8);
            }
        }

        // Download under way.
        if s["download"].is_object() {
            let cancel_width = inner.px(72);
            let bar_width = width - cancel_width - inner.px(10);
            let text = format!(
                "Downloading {} ({}%)",
                s["download"]["label"].as_str().unwrap_or_default(),
                s["download"]["percent"].as_i64().unwrap_or(0)
            );
            let (_, height) = inner.measure(&text, Font::Normal, bar_width);
            inner.put(
                "download-label",
                Kind::Label,
                rect(left, y, bar_width, height),
                &text,
                true,
                Font::Normal,
                Use::Nothing,
            );
            let bar_top = y + height + small_gap;
            let bar = inner.put(
                "download-bar",
                Kind::Progress,
                rect(left, bar_top, bar_width, inner.px(10)),
                "",
                true,
                Font::Normal,
                Use::Nothing,
            );
            // SAFETY: the bar belongs to this thread.
            unsafe {
                SendMessageW(
                    bar,
                    PBM_SETPOS,
                    s["download"]["percent"].as_u64().unwrap_or(0) as usize,
                    0,
                );
            }
            inner.put(
                "download-cancel",
                Kind::Button,
                rect(
                    right - cancel_width,
                    y,
                    cancel_width,
                    inner.px(BUTTON_HEIGHT),
                ),
                "Cancel",
                true,
                Font::Normal,
                Use::Act("cancel-download".to_string(), json!({})),
            );
            y = (bar_top + inner.px(10)).max(y + inner.px(BUTTON_HEIGHT)) + gap;
        }

        // Settings.
        y = self.heading(inner, "settings-heading", "Settings", left, y, width);
        let column = inner.px(LABEL_COLUMN);
        let field_x = left + column + inner.px(12);
        let field_width = right - field_x;
        let row_step = inner.px(COMBO_HEIGHT) + inner.px(8);
        let label_y = inner.px(4);
        let list_height = inner.px(COMBO_LIST_HEIGHT);
        let local = s["backend"].as_str() != Some("chatgpt");

        let row_label = |inner: &mut Inner, key: &str, text: &str, y: i32| {
            inner.put(
                key,
                Kind::Label,
                rect(left, y + label_y, column, line + inner.px(2)),
                text,
                true,
                Font::Normal,
                Use::Nothing,
            );
        };

        row_label(inner, "backend-label", "Speech", y);
        inner.put(
            "backend",
            Kind::Combo,
            rect(field_x, y, field_width, list_height),
            "",
            ready,
            Font::Normal,
            Use::Choose(Choice::Backend),
        );
        inner.set_items(
            "backend",
            BACKENDS.iter().map(|label| (*label).to_string()).collect(),
            Some(i32::from(!local)),
        );
        y += row_step;

        if local {
            let button_width = inner.px(84);
            let combo_width = field_width - button_width - inner.px(8);
            row_label(inner, "model-label", "Model", y);
            inner.put(
                "model",
                Kind::Combo,
                rect(field_x, y, combo_width, list_height),
                "",
                ready,
                Font::Normal,
                Use::Choose(Choice::Model),
            );
            inner.set_items(
                "model",
                labels(&s["models"]),
                Some(find_index(&s["models"], "path", &s["model"]).unwrap_or(-1)),
            );
            inner.put(
                "choose-model",
                Kind::Button,
                rect(
                    right - button_width,
                    y - inner.px(1),
                    button_width,
                    inner.px(BUTTON_HEIGHT),
                ),
                "Choose…",
                ready,
                Font::Normal,
                Use::ChooseModel,
            );
            y += row_step;

            let can_download = !s["download"].is_object()
                && s["downloads"]
                    .as_array()
                    .is_some_and(|list| !list.is_empty());
            if can_download {
                row_label(inner, "download-choice-label", "Get a model", y);
                inner.put(
                    "download-choice",
                    Kind::Combo,
                    rect(field_x, y, combo_width, list_height),
                    "",
                    ready,
                    Font::Normal,
                    Use::Choose(Choice::Download),
                );
                inner.set_items("download-choice", labels(&s["downloads"]), None);
                inner.put(
                    "download-start",
                    Kind::Button,
                    rect(
                        right - button_width,
                        y - inner.px(1),
                        button_width,
                        inner.px(BUTTON_HEIGHT),
                    ),
                    "Download",
                    ready,
                    Font::Normal,
                    Use::StartDownload,
                );
                y += row_step;
            }
        }

        row_label(inner, "language-label", "Language", y);
        inner.put(
            "language",
            Kind::Combo,
            rect(field_x, y, field_width, list_height),
            "",
            ready,
            Font::Normal,
            Use::Choose(Choice::Language),
        );
        inner.set_items(
            "language",
            labels(&s["languages"]),
            Some(find_index(&s["languages"], "code", &s["language"]).unwrap_or(-1)),
        );
        y += row_step;

        row_label(inner, "device-label", "Microphone", y);
        inner.put(
            "device",
            Kind::Combo,
            rect(field_x, y, field_width, list_height),
            "",
            ready,
            Font::Normal,
            Use::Choose(Choice::Device),
        );
        inner.set_items(
            "device",
            labels(&s["devices"]),
            Some(find_index(&s["devices"], "id", &s["device"]).unwrap_or(0)),
        );
        y += row_step;

        let check_height = inner.px(22);
        for (key, label, action, checked, enabled) in [
            ("sounds", "Sounds", "set-sounds", &s["sounds"], ready),
            (
                "idle-pill",
                "Show idle pill",
                "set-idle-pill",
                &s["idle_pill"],
                ready,
            ),
            (
                "autostart",
                "Start at login",
                "set-autostart",
                &s["autostart"],
                true,
            ),
        ] {
            let check = inner.put(
                key,
                Kind::Check,
                rect(left, y, width, check_height),
                label,
                enabled,
                Font::Normal,
                Use::Toggle(action),
            );
            // SAFETY: the box belongs to this thread.
            unsafe {
                SendMessageW(
                    check,
                    BM_SETCHECK,
                    usize::from(checked.as_bool() == Some(true)),
                    0,
                );
            }
            y += check_height + small_gap;
        }
        y += gap - small_gap;

        // Somewhere safe for a first try.
        y = self.heading(inner, "try-heading", "Try it", left, y, width);
        let hint = "Click in the box, then dictate into it.";
        let (_, height) = inner.measure(hint, Font::Normal, width);
        let label = inner.put(
            "try-hint",
            Kind::Label,
            rect(left, y, width, height),
            hint,
            true,
            Font::Normal,
            Use::Nothing,
        );
        self.colored(label, COLOR_DIM);
        y += height + small_gap;
        let box_height = inner.px(72);
        inner.put(
            "try-it",
            Kind::Edit,
            rect(left, y, width, box_height),
            "",
            true,
            Font::Normal,
            Use::Nothing,
        );
        y += box_height + gap;

        let note = if s["tray_available"].as_bool() == Some(false) {
            "No notification area was found, so start WispX again to come back to this window."
        } else {
            ""
        };
        if !note.is_empty() {
            let (_, height) = inner.measure(note, Font::Normal, width);
            let label = inner.put(
                "tray-note",
                Kind::Label,
                rect(left, y, width, height),
                note,
                true,
                Font::Normal,
                Use::Nothing,
            );
            self.colored(label, COLOR_DIM);
            y += height + gap;
        }

        // The bottom row.
        let height = inner.px(BUTTON_HEIGHT);
        let mut x = left;
        for (key, label, action, enabled) in [
            (
                "copy-last",
                "Copy last transcript",
                "copy-last",
                s["has_transcript"].as_bool() == Some(true),
            ),
            ("recheck", "Check again", "recheck", true),
        ] {
            let button_width = inner.measure(label, Font::Normal, 0).0 + inner.px(24);
            inner.put(
                key,
                Kind::Button,
                rect(x, y, button_width, height),
                label,
                enabled,
                Font::Normal,
                Use::Act(action.to_string(), json!({})),
            );
            x += button_width + inner.px(8);
        }
        let quit_width = inner.measure("Quit WispX", Font::Normal, 0).0 + inner.px(24);
        inner.put(
            "quit",
            Kind::Button,
            rect(right - quit_width, y, quit_width, height),
            "Quit WispX",
            true,
            Font::Normal,
            Use::Act("quit".to_string(), json!({})),
        );
        y += height + inner.px(20);

        inner.drop_unused();
        inner.fix_tab_order();

        let content = y + inner.scroll;
        inner.content = content;
        let page = client.bottom.max(1);
        let limit = (content - page).max(0);
        let info = ScrollInfo {
            size: size_of::<ScrollInfo>() as u32,
            mask: SIF_RANGE | SIF_PAGE | SIF_POS | SIF_DISABLENOSCROLL,
            min: 0,
            max: content - 1,
            page: page as u32,
            pos: inner.scroll.min(limit),
            track_pos: 0,
        };
        // SAFETY: the structure is complete, and the bar is always there, so setting it
        // never resizes the window under this code.
        unsafe {
            SetScrollInfo(self.hwnd, SB_VERT, &raw const info, 1);
        }
        if inner.scroll > limit {
            inner.scroll = limit;
            return true;
        }
        false
    }

    fn heading(
        &self,
        inner: &mut Inner,
        key: &str,
        text: &str,
        left: i32,
        y: i32,
        width: i32,
    ) -> i32 {
        let (_, height) = inner.measure(text, Font::Bold, width);
        inner.put(
            key,
            Kind::Label,
            rect(left, y, width, height),
            text,
            true,
            Font::Bold,
            Use::Nothing,
        );
        y + height + inner.px(6)
    }

    fn scroll_to(&self, target: i32) {
        let Ok(mut inner) = self.inner.try_borrow_mut() else {
            return;
        };
        let mut client = Rect::default();
        // SAFETY: the rectangle is local and the window belongs to this thread.
        unsafe {
            GetClientRect(self.hwnd, &raw mut client);
        }
        let limit = (inner.content - client.bottom).max(0);
        let target = target.clamp(0, limit);
        if target == inner.scroll {
            return;
        }
        inner.scroll = target;
        drop(inner);
        self.render();
    }

    fn on_scroll_bar(&self, code: u16) {
        let Ok(inner) = self.inner.try_borrow() else {
            return;
        };
        let (scroll, line, content) = (inner.scroll, inner.px(LINE_SCROLL), inner.content);
        drop(inner);
        let mut client = Rect::default();
        let mut info = ScrollInfo {
            size: size_of::<ScrollInfo>() as u32,
            mask: SIF_TRACKPOS,
            ..ScrollInfo::default()
        };
        // SAFETY: the structures are local and the window belongs to this thread.
        unsafe {
            GetClientRect(self.hwnd, &raw mut client);
            GetScrollInfo(self.hwnd, SB_VERT, &raw mut info);
        }
        let target = match code {
            SB_LINEUP => scroll - line,
            SB_LINEDOWN => scroll + line,
            SB_PAGEUP => scroll - client.bottom,
            SB_PAGEDOWN => scroll + client.bottom,
            SB_THUMBTRACK => info.track_pos,
            SB_TOP => 0,
            SB_BOTTOM => content,
            _ => return,
        };
        self.scroll_to(target);
    }

    fn on_wheel(&self, delta: i32) {
        let Ok(inner) = self.inner.try_borrow() else {
            return;
        };
        let target = inner.scroll - delta * inner.px(WHEEL_SCROLL) / WHEEL_NOTCH;
        drop(inner);
        self.scroll_to(target);
    }

    fn on_dpi_changed(&self, dpi: u32, suggested: Rect) {
        {
            let Ok(mut inner) = self.inner.try_borrow_mut() else {
                return;
            };
            inner.dpi = dpi.max(96);
            let fonts = Fonts::new(inner.dpi);
            for control in inner.controls.values() {
                // SAFETY: the window belongs to this thread and the font outlives it.
                unsafe {
                    SendMessageW(
                        control.hwnd,
                        WM_SETFONT,
                        fonts.get(control.font) as usize,
                        1,
                    );
                }
            }
            inner.fonts = fonts;
        }
        // SAFETY: the window belongs to this thread. This is the rectangle the system asks
        // windows to take, and sends the size message that lays everything out again.
        unsafe {
            SetWindowPos(
                self.hwnd,
                ptr::null_mut(),
                suggested.left,
                suggested.top,
                suggested.right - suggested.left,
                suggested.bottom - suggested.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    fn choose_model(&self, app: &super::App) {
        let mut file = vec![0u16; FILE_BUFFER];
        let filter: Vec<u16> = "Whisper models (*.bin)\0*.bin\0All files (*.*)\0*.*\0\0"
            .encode_utf16()
            .collect();
        let title = wide("Choose a whisper.cpp model");
        let mut info = OpenFileName {
            size: size_of::<OpenFileName>() as u32,
            owner: self.hwnd,
            instance: ptr::null_mut(),
            filter: filter.as_ptr(),
            custom_filter: ptr::null_mut(),
            max_custom_filter: 0,
            filter_index: 1,
            file: file.as_mut_ptr(),
            max_file: FILE_BUFFER as u32,
            file_title: ptr::null_mut(),
            max_file_title: 0,
            initial_dir: ptr::null(),
            title: title.as_ptr(),
            flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR,
            file_offset: 0,
            file_extension: 0,
            default_extension: ptr::null(),
            custom_data: 0,
            hook: ptr::null_mut(),
            template_name: ptr::null(),
            reserved_pointer: ptr::null_mut(),
            reserved: 0,
            flags_ex: 0,
        };
        // SAFETY: everything pointed to is local and outlives the dialog.
        if unsafe { GetOpenFileNameW(&raw mut info) } != 0 {
            app.act("set-model", json!({ "path": from_wide(&file) }));
        }
    }

    fn selected(hwnd: Hwnd) -> Option<usize> {
        // SAFETY: the box belongs to this thread.
        let index = unsafe { SendMessageW(hwnd, CB_GETCURSEL, 0, 0) };
        usize::try_from(index).ok()
    }

    fn on_choice(&self, app: &super::App, choice: Choice, hwnd: Hwnd) {
        let Some(index) = Self::selected(hwnd) else {
            return;
        };
        let Ok(inner) = self.inner.try_borrow() else {
            return;
        };
        let s = inner.state.clone();
        drop(inner);
        match choice {
            Choice::Backend => self.on_backend(app, index, &s),
            Choice::Model => {
                if let Some(path) = s["models"][index]["path"].as_str() {
                    app.act("set-model", json!({ "path": path }));
                }
            }
            Choice::Language => {
                if let Some(code) = s["languages"][index]["code"].as_str() {
                    app.act("set-language", json!({ "code": code }));
                }
            }
            Choice::Device => {
                if let Some(id) = s["devices"][index]["id"].as_str() {
                    app.act("set-device", json!({ "id": id }));
                }
            }
            Choice::Download => {}
        }
    }

    fn on_backend(&self, app: &super::App, index: usize, s: &Value) {
        let current = s["backend"].as_str().unwrap_or_default();
        if index == 1 && current != "chatgpt" {
            let answer = super::message_box(
                self.hwnd,
                CONSENT_TEXT,
                CONSENT_TITLE,
                MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
            );
            if answer == IDYES {
                app.act("set-backend", json!({ "backend": "chatgpt" }));
            }
            // However it closed, the box goes back to showing what the daemon says.
            self.render();
        } else if index == 0 && current != "local" {
            app.act("set-backend", json!({ "backend": "local" }));
        }
    }

    fn on_command(&self, app: &super::App, id: usize, code: u16) {
        let Ok(inner) = self.inner.try_borrow() else {
            return;
        };
        let found = inner
            .controls
            .values()
            .find(|control| control.id == id)
            .map(|control| (control.kind, control.hwnd, control.what.clone()));
        let chosen_download = inner
            .controls
            .get("download-choice")
            .and_then(|control| Self::selected(control.hwnd))
            .and_then(|index| inner.state["downloads"][index]["id"].as_str())
            .map(str::to_string);
        drop(inner);
        let Some((kind, hwnd, what)) = found else {
            return;
        };
        match (kind, code, what) {
            (Kind::Button, BN_CLICKED, Use::Act(name, args)) => app.act(&name, args),
            (Kind::Button, BN_CLICKED, Use::ChooseModel) => self.choose_model(app),
            (Kind::Button, BN_CLICKED, Use::StartDownload) => {
                if let Some(id) = chosen_download {
                    app.act("download-model", json!({ "id": id }));
                }
            }
            (Kind::Check, BN_CLICKED, Use::Toggle(name)) => {
                // SAFETY: the box belongs to this thread.
                let checked = unsafe { SendMessageW(hwnd, BM_GETCHECK, 0, 0) } as usize;
                app.act(name, json!({ "checked": checked == BST_CHECKED }));
            }
            (Kind::Combo, CBN_SELCHANGE, Use::Choose(choice)) => {
                self.on_choice(app, choice, hwnd);
            }
            _ => {}
        }
    }

    fn hide(&self) {
        // SAFETY: the window belongs to this thread.
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    let low_word = (wparam & 0xFFFF) as u16;
    let high_word = ((wparam >> 16) & 0xFFFF) as u16;
    if let Some(app) = super::app() {
        let settings = &app.settings;
        match message {
            WM_CLOSE => {
                settings.hide();
                return 0;
            }
            WM_COMMAND => {
                match low_word {
                    IDCANCEL => settings.hide(),
                    IDOK => {}
                    id => settings.on_command(app, usize::from(id), high_word),
                }
                return 0;
            }
            WM_SIZE => {
                if settings.visible() {
                    settings.render();
                }
                return 0;
            }
            WM_VSCROLL => {
                settings.on_scroll_bar(low_word);
                return 0;
            }
            WM_MOUSEWHEEL => {
                settings.on_wheel(i32::from(high_word as i16));
                return 0;
            }
            WM_CTLCOLORSTATIC => {
                let color = settings.colors.borrow().get(&(lparam as usize)).copied();
                let dc = wparam as Hdc;
                // SAFETY: for this message, `wparam` is the control's device context.
                unsafe {
                    SetTextColor(dc, color.unwrap_or_else(|| GetSysColor(COLOR_WINDOWTEXT)));
                    SetBkColor(dc, GetSysColor(COLOR_WINDOW));
                    return GetSysColorBrush(COLOR_WINDOW) as isize;
                }
            }
            WM_DPICHANGED => {
                // SAFETY: for this message, `lparam` points to a rectangle.
                let suggested = unsafe { *(lparam as *const Rect) };
                settings.on_dpi_changed(u32::from(low_word), suggested);
                return 0;
            }
            WM_GETMINMAXINFO => {
                if let Ok(inner) = settings.inner.try_borrow() {
                    let (width, height) = (inner.px(MIN_WIDTH), inner.px(MIN_HEIGHT));
                    // SAFETY: for this message, `lparam` points to this structure.
                    unsafe {
                        let info = lparam as *mut MinMaxInfo;
                        (*info).min_track = Point {
                            x: width,
                            y: height,
                        };
                    }
                    return 0;
                }
            }
            _ => {}
        }
    }
    // SAFETY: passes on what it was given, for a window of this thread.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
