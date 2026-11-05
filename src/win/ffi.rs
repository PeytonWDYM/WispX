//! The parts of the Windows API this program uses, declared by hand so the only
//! crate it depends on stays `serde_json`. Layouts are checked below on 64-bit.

#![allow(non_snake_case, dead_code)]

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::ptr;

pub type Handle = *mut c_void;
pub type Hwnd = *mut c_void;
pub type Hdc = *mut c_void;
pub type Hgdi = *mut c_void;
pub type Hmenu = *mut c_void;
pub type Hicon = *mut c_void;
pub type Psid = *mut c_void;
pub type WndProc = unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize;
pub type HookProc = unsafe extern "system" fn(i32, usize, isize) -> isize;
pub type SubclassProc = unsafe extern "system" fn(Hwnd, u32, usize, isize, usize, usize) -> isize;
pub type ConsoleHandler = unsafe extern "system" fn(u32) -> i32;

// Errors.
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_HANDLE_EOF: u32 = 38;
pub const ERROR_BROKEN_PIPE: u32 = 109;
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_PIPE_BUSY: u32 = 231;
pub const ERROR_NO_DATA: u32 = 232;
pub const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
pub const ERROR_OPERATION_ABORTED: u32 = 995;
pub const ERROR_IO_PENDING: u32 = 997;
pub const ERROR_PIPE_CONNECTED: u32 = 535;

// Waiting.
pub const INFINITE: u32 = u32::MAX;
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;

// Files and pipes.
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
pub const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
pub const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
pub const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
pub const PIPE_ACCESS_DUPLEX: u32 = 3;
pub const PIPE_REJECT_REMOTE_CLIENTS: u32 = 8;
pub const PIPE_UNLIMITED_INSTANCES: u32 = 255;

// Processes and tokens.
pub const PROCESS_TERMINATE: u32 = 1;
pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
pub const TOKEN_QUERY: u32 = 8;
pub const TOKEN_USER_CLASS: u32 = 1;
pub const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
pub const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
pub const DETACHED_PROCESS: u32 = 8;
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const ASFW_ANY: u32 = u32::MAX;
pub const SDDL_REVISION_1: u32 = 1;

// Console events.
pub const CTRL_CLOSE_EVENT: u32 = 2;
pub const CTRL_LOGOFF_EVENT: u32 = 5;
pub const CTRL_SHUTDOWN_EVENT: u32 = 6;

// Registry.
/// Sign-extended on 64-bit Windows, as `winreg.h` has it.
pub const HKEY_CURRENT_USER: *mut c_void = 0x8000_0001_u32 as i32 as isize as *mut c_void;
pub const REG_SZ: u32 = 1;
pub const RRF_RT_REG_SZ: u32 = 2;

// Memory.
pub const GMEM_MOVEABLE: u32 = 2;

// The clipboard.
pub const CF_UNICODETEXT: u32 = 13;

// Sound and the microphone.
pub const SND_SYNC: u32 = 0;
pub const SND_NODEFAULT: u32 = 2;
pub const SND_MEMORY: u32 = 4;
pub const WAVE_MAPPER: u32 = u32::MAX;
pub const WAVE_FORMAT_PCM: u16 = 1;
pub const CALLBACK_EVENT: u32 = 0x0005_0000;
pub const WAVE_MAPPED: u32 = 4;
pub const WHDR_DONE: u32 = 1;
pub const MMSYSERR_ALLOCATED: u32 = 4;
pub const MMSYSERR_BADDEVICEID: u32 = 2;
pub const MMSYSERR_NODRIVER: u32 = 6;

// Keyboard.
pub const WH_KEYBOARD_LL: i32 = 13;
pub const LLKHF_INJECTED: u32 = 0x10;
pub const INPUT_KEYBOARD: u32 = 1;
pub const KEYEVENTF_EXTENDEDKEY: u32 = 1;
pub const KEYEVENTF_KEYUP: u32 = 2;

// Messages.
pub const WM_NULL: u32 = 0;
pub const WM_SIZE: u32 = 5;
pub const WM_CLOSE: u32 = 0x10;
pub const WM_QUERYENDSESSION: u32 = 0x11;
pub const WM_ENDSESSION: u32 = 0x16;
pub const WM_MOUSEACTIVATE: u32 = 0x21;
pub const WM_GETMINMAXINFO: u32 = 0x24;
pub const WM_SETFONT: u32 = 0x30;
pub const WM_CONTEXTMENU: u32 = 0x7B;
pub const WM_NCHITTEST: u32 = 0x84;
pub const WM_GETDLGCODE: u32 = 0x87;
pub const WM_KEYDOWN: u32 = 0x100;
pub const VK_RETURN: usize = 0x0D;
pub const WM_KEYUP: u32 = 0x101;
pub const WM_SYSKEYDOWN: u32 = 0x104;
pub const WM_SYSKEYUP: u32 = 0x105;
pub const WM_COMMAND: u32 = 0x111;
pub const WM_TIMER: u32 = 0x113;
pub const WM_VSCROLL: u32 = 0x115;
pub const WM_CTLCOLORSTATIC: u32 = 0x138;
pub const WM_MOUSEWHEEL: u32 = 0x20A;
pub const WM_DPICHANGED: u32 = 0x2E0;
pub const WM_USER: u32 = 0x400;
pub const WM_APP: u32 = 0x8000;
pub const MA_NOACTIVATE: isize = 3;
pub const HTTRANSPARENT: isize = -1;
pub const IDOK: u16 = 1;
pub const IDCANCEL: u16 = 2;
pub const DLGC_WANTTAB: isize = 2;
pub const DLGC_WANTALLKEYS: isize = 4;

// Window styles.
pub const WS_POPUP: u32 = 0x8000_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
pub const WS_VSCROLL: u32 = 0x0020_0000;
pub const WS_TABSTOP: u32 = 0x0001_0000;
pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
pub const WS_EX_TOPMOST: u32 = 8;
pub const WS_EX_TRANSPARENT: u32 = 0x20;
pub const WS_EX_TOOLWINDOW: u32 = 0x80;
pub const WS_EX_CLIENTEDGE: u32 = 0x200;
pub const WS_EX_CONTROLPARENT: u32 = 0x0001_0000;
pub const WS_EX_LAYERED: u32 = 0x0008_0000;
pub const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
pub const CW_USEDEFAULT: i32 = i32::MIN;
pub const HWND_MESSAGE: Hwnd = -3_isize as Hwnd;
pub const HWND_TOPMOST: Hwnd = -1_isize as Hwnd;
pub const CS_HREDRAW: u32 = 2;
pub const CS_VREDRAW: u32 = 1;
pub const COLOR_WINDOW: i32 = 5;
pub const COLOR_WINDOWTEXT: i32 = 8;
pub const IDC_ARROW: usize = 32512;

// Controls.
pub const BS_PUSHBUTTON: u32 = 0;
pub const BS_AUTOCHECKBOX: u32 = 3;
pub const BN_CLICKED: u16 = 0;
pub const BM_GETCHECK: u32 = 0xF0;
pub const BM_SETCHECK: u32 = 0xF1;
pub const BST_CHECKED: usize = 1;
pub const SS_NOPREFIX: u32 = 0x80;
pub const CBS_DROPDOWNLIST: u32 = 3;
pub const CBS_HASSTRINGS: u32 = 0x200;
pub const CBN_SELCHANGE: u16 = 1;
pub const CB_ADDSTRING: u32 = 0x143;
pub const CB_GETCURSEL: u32 = 0x147;
pub const CB_RESETCONTENT: u32 = 0x14B;
pub const CB_SETCURSEL: u32 = 0x14E;
pub const CB_GETDROPPEDSTATE: u32 = 0x157;
pub const ES_MULTILINE: u32 = 4;
pub const ES_AUTOVSCROLL: u32 = 0x40;
pub const ES_WANTRETURN: u32 = 0x1000;
pub const PBS_SMOOTH: u32 = 1;
pub const PBM_SETPOS: u32 = 0x402;
pub const PBM_SETRANGE32: u32 = 0x406;
pub const ICC_PROGRESS_CLASS: u32 = 0x20;
pub const ICC_STANDARD_CLASSES: u32 = 0x4000;

// Scroll bars.
pub const SB_VERT: i32 = 1;
pub const SB_LINEUP: u16 = 0;
pub const SB_LINEDOWN: u16 = 1;
pub const SB_PAGEUP: u16 = 2;
pub const SB_PAGEDOWN: u16 = 3;
pub const SB_THUMBTRACK: u16 = 5;
pub const SB_TOP: u16 = 6;
pub const SB_BOTTOM: u16 = 7;
pub const SIF_RANGE: u32 = 1;
pub const SIF_PAGE: u32 = 2;
pub const SIF_POS: u32 = 4;
pub const SIF_DISABLENOSCROLL: u32 = 8;
pub const SIF_TRACKPOS: u32 = 0x10;

// Moving and showing windows.
pub const SWP_NOSIZE: u32 = 1;
pub const SWP_NOMOVE: u32 = 2;
pub const SWP_NOZORDER: u32 = 4;
pub const SWP_NOACTIVATE: u32 = 0x10;
pub const SW_HIDE: i32 = 0;
pub const SW_SHOWNORMAL: i32 = 1;
pub const SW_SHOWNOACTIVATE: i32 = 4;
pub const SW_SHOW: i32 = 5;
pub const SW_RESTORE: i32 = 9;
pub const MONITOR_DEFAULTTONEAREST: u32 = 2;
pub const MDT_EFFECTIVE_DPI: i32 = 0;
pub const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: *mut c_void = -4_isize as *mut c_void;

// Layered windows and drawing.
pub const ULW_ALPHA: u32 = 2;
pub const AC_SRC_ALPHA: u8 = 1;
pub const DIB_RGB_COLORS: u32 = 0;
pub const TRANSPARENT: i32 = 1;
pub const FW_NORMAL: i32 = 400;
pub const FW_SEMIBOLD: i32 = 600;
pub const DEFAULT_CHARSET: u32 = 1;
pub const ANTIALIASED_QUALITY: u32 = 4;
pub const CLEARTYPE_QUALITY: u32 = 5;
pub const DT_CENTER: u32 = 1;
pub const DT_VCENTER: u32 = 4;
pub const DT_WORDBREAK: u32 = 0x10;
pub const DT_SINGLELINE: u32 = 0x20;
pub const DT_CALCRECT: u32 = 0x400;
pub const DT_NOPREFIX: u32 = 0x800;
pub const DT_END_ELLIPSIS: u32 = 0x8000;

// The tray.
pub const NIM_ADD: u32 = 0;
pub const NIM_MODIFY: u32 = 1;
pub const NIM_DELETE: u32 = 2;
pub const NIM_SETVERSION: u32 = 4;
pub const NIF_MESSAGE: u32 = 1;
pub const NIF_ICON: u32 = 2;
pub const NIF_TIP: u32 = 4;
pub const NIF_SHOWTIP: u32 = 0x80;
pub const NOTIFYICON_VERSION_4: u32 = 4;
pub const NIN_SELECT: u32 = WM_USER;
pub const NIN_KEYSELECT: u32 = WM_USER + 1;
pub const MF_STRING: u32 = 0;
pub const MF_GRAYED: u32 = 1;
pub const MF_CHECKED: u32 = 8;
pub const MF_SEPARATOR: u32 = 0x800;
pub const TPM_RIGHTBUTTON: u32 = 2;
pub const TPM_BOTTOMALIGN: u32 = 0x20;
pub const TPM_RETURNCMD: u32 = 0x100;
pub const IMAGE_ICON: u32 = 1;

// Dialogs.
pub const MB_OK: u32 = 0;
pub const MB_OKCANCEL: u32 = 1;
pub const MB_YESNO: u32 = 4;
pub const MB_ICONERROR: u32 = 0x10;
pub const MB_ICONQUESTION: u32 = 0x20;
pub const MB_DEFBUTTON2: u32 = 0x100;
pub const IDYES: i32 = 6;
pub const OFN_HIDEREADONLY: u32 = 4;
pub const OFN_NOCHANGEDIR: u32 = 8;
pub const OFN_PATHMUSTEXIST: u32 = 0x800;
pub const OFN_FILEMUSTEXIST: u32 = 0x1000;

// Fullscreen and "do not disturb".
pub const QUNS_BUSY: i32 = 2;
pub const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
pub const QUNS_PRESENTATION_MODE: i32 = 4;

// COM.
pub const COINIT_APARTMENTTHREADED: u32 = 2;
pub const COINIT_DISABLE_OLE1DDE: u32 = 4;
pub const CLSCTX_INPROC_SERVER: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub cx: i32,
    pub cy: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

#[repr(C)]
pub struct Overlapped {
    pub internal: usize,
    pub internal_high: usize,
    pub offset: u32,
    pub offset_high: u32,
    pub event: Handle,
}

#[repr(C)]
pub struct SecurityAttributes {
    pub length: u32,
    pub descriptor: *mut c_void,
    pub inherit: i32,
}

#[repr(C)]
pub struct TokenUser {
    pub sid: Psid,
    pub attributes: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct IoCounters {
    pub counts: [u64; 6],
}

#[repr(C)]
#[derive(Default)]
pub struct JobBasicLimits {
    pub per_process_time: i64,
    pub per_job_time: i64,
    pub limit_flags: u32,
    pub min_working_set: usize,
    pub max_working_set: usize,
    pub active_processes: u32,
    pub affinity: usize,
    pub priority_class: u32,
    pub scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct JobExtendedLimits {
    pub basic: JobBasicLimits,
    pub io: IoCounters,
    pub process_memory: usize,
    pub job_memory: usize,
    pub peak_process_memory: usize,
    pub peak_job_memory: usize,
}

#[repr(C, packed(1))]
pub struct WaveFormat {
    pub tag: u16,
    pub channels: u16,
    pub samples_per_sec: u32,
    pub bytes_per_sec: u32,
    pub block_align: u16,
    pub bits_per_sample: u16,
    pub extra: u16,
}

#[repr(C)]
pub struct WaveHdr {
    pub data: *mut u8,
    pub buffer_length: u32,
    pub bytes_recorded: u32,
    pub user: usize,
    pub flags: u32,
    pub loops: u32,
    pub next: *mut WaveHdr,
    pub reserved: usize,
}

#[repr(C, packed(1))]
pub struct WaveInCaps {
    pub manufacturer: u16,
    pub product: u16,
    pub driver_version: u32,
    pub name: [u16; 32],
    pub formats: u32,
    pub channels: u16,
    pub reserved: u16,
}

#[repr(C)]
pub struct Msg {
    pub hwnd: Hwnd,
    pub message: u32,
    pub wparam: usize,
    pub lparam: isize,
    pub time: u32,
    pub point: Point,
}

#[repr(C)]
pub struct KbdLlHook {
    pub vk_code: u32,
    pub scan_code: u32,
    pub flags: u32,
    pub time: u32,
    pub extra: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KeybdInput {
    pub vk: u16,
    pub scan: u16,
    pub flags: u32,
    pub time: u32,
    pub extra: usize,
}

/// A keyboard `INPUT`, padded to the size of the union's largest member.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Input {
    pub kind: u32,
    pub key: KeybdInput,
    pub padding: [u8; 8],
}

#[repr(C)]
pub struct WndClass {
    pub size: u32,
    pub style: u32,
    pub proc: Option<WndProc>,
    pub class_extra: i32,
    pub window_extra: i32,
    pub instance: Handle,
    pub icon: Hicon,
    pub cursor: Handle,
    pub background: Hgdi,
    pub menu_name: *const u16,
    pub class_name: *const u16,
    pub small_icon: Hicon,
}

#[repr(C)]
pub struct BlendFunction {
    pub op: u8,
    pub flags: u8,
    pub constant_alpha: u8,
    pub alpha_format: u8,
}

#[repr(C)]
pub struct BitmapInfo {
    pub size: u32,
    pub width: i32,
    pub height: i32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: u32,
    pub image_size: u32,
    pub x_per_meter: i32,
    pub y_per_meter: i32,
    pub colors_used: u32,
    pub colors_important: u32,
    pub colors: [u32; 1],
}

#[repr(C)]
pub struct NotifyIconData {
    pub size: u32,
    pub hwnd: Hwnd,
    pub id: u32,
    pub flags: u32,
    pub callback: u32,
    pub icon: Hicon,
    pub tip: [u16; 128],
    pub state: u32,
    pub state_mask: u32,
    pub info: [u16; 256],
    pub version: u32,
    pub info_title: [u16; 64],
    pub info_flags: u32,
    pub guid: Guid,
    pub balloon_icon: Hicon,
}

#[repr(C)]
pub struct OpenFileName {
    pub size: u32,
    pub owner: Hwnd,
    pub instance: Handle,
    pub filter: *const u16,
    pub custom_filter: *mut u16,
    pub max_custom_filter: u32,
    pub filter_index: u32,
    pub file: *mut u16,
    pub max_file: u32,
    pub file_title: *mut u16,
    pub max_file_title: u32,
    pub initial_dir: *const u16,
    pub title: *const u16,
    pub flags: u32,
    pub file_offset: u16,
    pub file_extension: u16,
    pub default_extension: *const u16,
    pub custom_data: isize,
    pub hook: *mut c_void,
    pub template_name: *const u16,
    pub reserved_pointer: *mut c_void,
    pub reserved: u32,
    pub flags_ex: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct MonitorInfo {
    pub size: u32,
    pub monitor: Rect,
    pub work: Rect,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct ScrollInfo {
    pub size: u32,
    pub mask: u32,
    pub min: i32,
    pub max: i32,
    pub page: u32,
    pub pos: i32,
    pub track_pos: i32,
}

#[repr(C)]
pub struct MinMaxInfo {
    pub reserved: Point,
    pub max_size: Point,
    pub max_position: Point,
    pub min_track: Point,
    pub max_track: Point,
}

#[repr(C)]
pub struct InitCommonControls {
    pub size: u32,
    pub classes: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    pub fn CancelIoEx(file: Handle, overlapped: *mut Overlapped) -> i32;
    pub fn CloseHandle(handle: Handle) -> i32;
    pub fn ConnectNamedPipe(pipe: Handle, overlapped: *mut Overlapped) -> i32;
    pub fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
    pub fn CreateEventW(
        attributes: *const SecurityAttributes,
        manual_reset: i32,
        initial: i32,
        name: *const u16,
    ) -> Handle;
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        attributes: *const SecurityAttributes,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    pub fn CreateJobObjectW(attributes: *const SecurityAttributes, name: *const u16) -> Handle;
    pub fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_size: u32,
        in_size: u32,
        default_timeout: u32,
        attributes: *const SecurityAttributes,
    ) -> Handle;
    pub fn GetCurrentProcess() -> Handle;
    pub fn GetCurrentProcessId() -> u32;
    pub fn GetLastError() -> u32;
    pub fn GetModuleHandleW(name: *const u16) -> Handle;
    pub fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
    pub fn GetOverlappedResult(
        file: Handle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: i32,
    ) -> i32;
    pub fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    pub fn GlobalFree(memory: Handle) -> Handle;
    pub fn GlobalLock(memory: Handle) -> *mut c_void;
    pub fn GlobalSize(memory: Handle) -> usize;
    pub fn GlobalUnlock(memory: Handle) -> i32;
    pub fn LocalFree(memory: *mut c_void) -> *mut c_void;
    pub fn MulDiv(number: i32, numerator: i32, denominator: i32) -> i32;
    pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    pub fn PeekNamedPipe(
        pipe: Handle,
        buffer: *mut c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
    pub fn ProcessIdToSessionId(pid: u32, session: *mut u32) -> i32;
    pub fn ReadFile(
        file: Handle,
        buffer: *mut u8,
        size: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    pub fn ResetEvent(event: Handle) -> i32;
    pub fn SetConsoleCtrlHandler(handler: Option<ConsoleHandler>, add: i32) -> i32;
    pub fn SetEvent(event: Handle) -> i32;
    pub fn SetInformationJobObject(
        job: Handle,
        class: u32,
        information: *const c_void,
        size: u32,
    ) -> i32;
    pub fn TerminateProcess(process: Handle, code: u32) -> i32;
    pub fn WaitForMultipleObjects(
        count: u32,
        handles: *const Handle,
        wait_all: i32,
        milliseconds: u32,
    ) -> u32;
    pub fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
    pub fn WaitNamedPipeW(name: *const u16, milliseconds: u32) -> i32;
    pub fn WriteFile(
        file: Handle,
        buffer: *const u8,
        size: u32,
        written: *mut u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    pub fn ConvertSidToStringSidW(sid: Psid, text: *mut *mut u16) -> i32;
    pub fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
    pub fn EqualSid(first: Psid, second: Psid) -> i32;
    pub fn GetTokenInformation(
        token: Handle,
        class: u32,
        information: *mut c_void,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    pub fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    pub fn RegDeleteKeyValueW(key: *mut c_void, subkey: *const u16, value: *const u16) -> i32;
    pub fn RegGetValueW(
        key: *mut c_void,
        subkey: *const u16,
        value: *const u16,
        flags: u32,
        kind: *mut u32,
        data: *mut c_void,
        size: *mut u32,
    ) -> i32;
    pub fn RegSetKeyValueW(
        key: *mut c_void,
        subkey: *const u16,
        value: *const u16,
        kind: u32,
        data: *const c_void,
        size: u32,
    ) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    pub fn AllowSetForegroundWindow(pid: u32) -> i32;
    pub fn AppendMenuW(menu: Hmenu, flags: u32, id: usize, text: *const u16) -> i32;
    pub fn CallNextHookEx(hook: Handle, code: i32, wparam: usize, lparam: isize) -> isize;
    pub fn CloseClipboard() -> i32;
    pub fn CreatePopupMenu() -> Hmenu;
    pub fn CreateWindowExW(
        ex_style: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: Hmenu,
        instance: Handle,
        param: *const c_void,
    ) -> Hwnd;
    pub fn DefWindowProcW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    pub fn DestroyMenu(menu: Hmenu) -> i32;
    pub fn DestroyWindow(hwnd: Hwnd) -> i32;
    pub fn DispatchMessageW(message: *const Msg) -> isize;
    pub fn DrawTextW(dc: Hdc, text: *const u16, length: i32, rect: *mut Rect, format: u32) -> i32;
    pub fn EmptyClipboard() -> i32;
    pub fn EnableWindow(hwnd: Hwnd, enable: i32) -> i32;
    pub fn GetAsyncKeyState(key: i32) -> i16;
    pub fn GetClassNameW(hwnd: Hwnd, name: *mut u16, length: i32) -> i32;
    pub fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    pub fn GetClipboardData(format: u32) -> Handle;
    pub fn GetCursorPos(point: *mut Point) -> i32;
    pub fn GetDC(hwnd: Hwnd) -> Hdc;
    pub fn GetDpiForWindow(hwnd: Hwnd) -> u32;
    pub fn GetFocus() -> Hwnd;
    pub fn GetForegroundWindow() -> Hwnd;
    pub fn GetMessageW(message: *mut Msg, hwnd: Hwnd, min: u32, max: u32) -> i32;
    pub fn GetMonitorInfoW(monitor: Handle, info: *mut MonitorInfo) -> i32;
    pub fn GetScrollInfo(hwnd: Hwnd, bar: i32, info: *mut ScrollInfo) -> i32;
    pub fn GetSysColor(index: i32) -> u32;
    pub fn GetSysColorBrush(index: i32) -> Hgdi;
    pub fn GetWindowRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    pub fn IsClipboardFormatAvailable(format: u32) -> i32;
    pub fn IsDialogMessageW(hwnd: Hwnd, message: *mut Msg) -> i32;
    pub fn IsIconic(hwnd: Hwnd) -> i32;
    pub fn IsWindowVisible(hwnd: Hwnd) -> i32;
    pub fn KillTimer(hwnd: Hwnd, id: usize) -> i32;
    pub fn LoadCursorW(instance: Handle, name: *const u16) -> Handle;
    pub fn LoadImageW(
        instance: Handle,
        name: *const u16,
        kind: u32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> Handle;
    pub fn MessageBoxW(hwnd: Hwnd, text: *const u16, title: *const u16, kind: u32) -> i32;
    pub fn MonitorFromPoint(point: Point, flags: u32) -> Handle;
    pub fn MonitorFromWindow(hwnd: Hwnd, flags: u32) -> Handle;
    pub fn OpenClipboard(owner: Hwnd) -> i32;
    pub fn PostMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> i32;
    pub fn PostQuitMessage(code: i32);
    pub fn RegisterClassExW(class: *const WndClass) -> u16;
    pub fn RegisterClipboardFormatW(name: *const u16) -> u32;
    pub fn RegisterWindowMessageW(name: *const u16) -> u32;
    pub fn ReleaseDC(hwnd: Hwnd, dc: Hdc) -> i32;
    pub fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    pub fn SendMessageW(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    pub fn SetClipboardData(format: u32, data: Handle) -> Handle;
    pub fn SetFocus(hwnd: Hwnd) -> Hwnd;
    pub fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    pub fn SetProcessDpiAwarenessContext(context: *mut c_void) -> i32;
    pub fn SetScrollInfo(hwnd: Hwnd, bar: i32, info: *const ScrollInfo, redraw: i32) -> i32;
    pub fn SetTimer(hwnd: Hwnd, id: usize, milliseconds: u32, callback: *const c_void) -> usize;
    pub fn SetWindowPos(
        hwnd: Hwnd,
        after: Hwnd,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    pub fn SetWindowTextW(hwnd: Hwnd, text: *const u16) -> i32;
    pub fn SetWindowsHookExW(kind: i32, proc: HookProc, module: Handle, thread: u32) -> Handle;
    pub fn InvalidateRect(window: Hwnd, rect: *const Rect, erase: i32) -> i32;
    pub fn ShowWindow(hwnd: Hwnd, command: i32) -> i32;
    pub fn TrackPopupMenuEx(
        menu: Hmenu,
        flags: u32,
        x: i32,
        y: i32,
        hwnd: Hwnd,
        params: *const c_void,
    ) -> i32;
    pub fn TranslateMessage(message: *const Msg) -> i32;
    pub fn UpdateLayeredWindow(
        hwnd: Hwnd,
        destination: Hdc,
        position: *const Point,
        size: *const Size,
        source: Hdc,
        source_position: *const Point,
        key: u32,
        blend: *const BlendFunction,
        flags: u32,
    ) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateCompatibleDC(dc: Hdc) -> Hdc;
    pub fn CreateDIBSection(
        dc: Hdc,
        info: *const BitmapInfo,
        usage: u32,
        bits: *mut *mut c_void,
        section: Handle,
        offset: u32,
    ) -> Hgdi;
    pub fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike_out: u32,
        charset: u32,
        out_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch_and_family: u32,
        face: *const u16,
    ) -> Hgdi;
    pub fn DeleteDC(dc: Hdc) -> i32;
    pub fn DeleteObject(object: Hgdi) -> i32;
    pub fn SelectObject(dc: Hdc, object: Hgdi) -> Hgdi;
    pub fn SetBkColor(dc: Hdc, color: u32) -> u32;
    pub fn SetBkMode(dc: Hdc, mode: i32) -> i32;
    pub fn SetTextColor(dc: Hdc, color: u32) -> u32;
}

#[link(name = "winmm")]
unsafe extern "system" {
    pub fn PlaySoundW(sound: *const u16, module: Handle, flags: u32) -> i32;
    pub fn waveInAddBuffer(device: Handle, header: *mut WaveHdr, size: u32) -> u32;
    pub fn waveInClose(device: Handle) -> u32;
    pub fn waveInGetDevCapsW(id: usize, caps: *mut WaveInCaps, size: u32) -> u32;
    pub fn waveInGetNumDevs() -> u32;
    pub fn waveInOpen(
        device: *mut Handle,
        id: u32,
        format: *const WaveFormat,
        callback: usize,
        instance: usize,
        flags: u32,
    ) -> u32;
    pub fn waveInPrepareHeader(device: Handle, header: *mut WaveHdr, size: u32) -> u32;
    pub fn waveInReset(device: Handle) -> u32;
    pub fn waveInStart(device: Handle) -> u32;
    pub fn waveInUnprepareHeader(device: Handle, header: *mut WaveHdr, size: u32) -> u32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    pub fn SHGetKnownFolderPath(
        id: *const Guid,
        flags: u32,
        token: Handle,
        path: *mut *mut u16,
    ) -> i32;
    pub fn SHQueryUserNotificationState(state: *mut i32) -> i32;
    pub fn ShellExecuteW(
        hwnd: Hwnd,
        operation: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        show: i32,
    ) -> Handle;
    pub fn Shell_NotifyIconW(message: u32, data: *const NotifyIconData) -> i32;
}

#[link(name = "shcore")]
unsafe extern "system" {
    pub fn GetDpiForMonitor(monitor: Handle, kind: i32, x: *mut u32, y: *mut u32) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    pub fn CoCreateInstance(
        class: *const Guid,
        outer: *mut c_void,
        context: u32,
        interface: *const Guid,
        object: *mut *mut c_void,
    ) -> i32;
    pub fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    pub fn CoTaskMemFree(memory: *mut c_void);
}

#[link(name = "comdlg32")]
unsafe extern "system" {
    pub fn GetOpenFileNameW(info: *mut OpenFileName) -> i32;
}

#[link(name = "comctl32")]
unsafe extern "system" {
    pub fn DefSubclassProc(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize;
    pub fn InitCommonControlsEx(info: *const InitCommonControls) -> i32;
    pub fn SetWindowSubclass(hwnd: Hwnd, proc: SubclassProc, id: usize, data: usize) -> i32;
}

/// A NUL-terminated copy for a `W` function.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

pub fn wide_os(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain([0]).collect()
}

/// Everything before the first NUL.
pub fn from_wide(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

pub fn from_wide_ptr(text: *const u16) -> String {
    if text.is_null() {
        return String::new();
    }
    let mut length = 0;
    // SAFETY: callers pass a NUL-terminated string from the system.
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, length))
    }
}

/// `INVALID_HANDLE_VALUE`, which some calls return in place of null.
pub fn invalid_handle() -> Handle {
    -1_isize as Handle
}

pub fn last_error() -> u32 {
    // SAFETY: takes nothing and reads a per-thread value.
    unsafe { GetLastError() }
}

/// A handle closed when this goes. The handles here are safe to use and to close from
/// any thread, unlike window handles, which do not go in one of these.
#[derive(Debug)]
pub struct Owned(pub Handle);

// SAFETY: kernel handles are not tied to the thread that opened them.
unsafe impl Send for Owned {}
// SAFETY: as above, and every call made on these handles is safe to make concurrently.
unsafe impl Sync for Owned {}

impl Owned {
    /// An event that needs resetting by hand and starts off.
    pub fn event() -> std::io::Result<Owned> {
        // SAFETY: all the pointers are allowed to be null.
        let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Owned(handle))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: this owns the handle, and nothing can use it after this.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A window handle that may be sent to other threads. Only calls which Windows
/// allows from any thread are made with it there.
#[derive(Clone, Copy, Debug)]
pub struct SendHwnd(pub Hwnd);

// SAFETY: it is only a number until used, and see above.
unsafe impl Send for SendHwnd {}
// SAFETY: as above.
unsafe impl Sync for SendHwnd {}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<Overlapped>() == 32);
    assert!(size_of::<SecurityAttributes>() == 24);
    assert!(size_of::<JobBasicLimits>() == 64);
    assert!(size_of::<JobExtendedLimits>() == 144);
    assert!(size_of::<WaveFormat>() == 18);
    assert!(size_of::<WaveHdr>() == 48);
    assert!(size_of::<WaveInCaps>() == 80);
    assert!(size_of::<Msg>() == 48);
    assert!(size_of::<KbdLlHook>() == 24);
    assert!(size_of::<Input>() == 40);
    assert!(offset_of!(Input, key) == 8);
    assert!(size_of::<WndClass>() == 80);
    assert!(size_of::<BitmapInfo>() == 44);
    assert!(size_of::<NotifyIconData>() == 976);
    assert!(offset_of!(NotifyIconData, tip) == 40);
    assert!(offset_of!(NotifyIconData, version) == 816);
    assert!(offset_of!(NotifyIconData, guid) == 952);
    assert!(size_of::<OpenFileName>() == 152);
    assert!(offset_of!(OpenFileName, flags) == 96);
    assert!(size_of::<MonitorInfo>() == 40);
    assert!(size_of::<ScrollInfo>() == 28);
    assert!(size_of::<MinMaxInfo>() == 40);
};
