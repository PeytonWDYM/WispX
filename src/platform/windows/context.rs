//! Best-effort caret context. UI Automation and its COM objects stay on one worker.
mod text;
use crate::transcribe::cleanup::MAX_CONTEXT_CHARS;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::{
        Controls::EM_GETSEL,
        WindowsAndMessaging::{
            GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, SMTO_ABORTIFHUNG,
            SendMessageTimeoutW,
        },
    },
};
use windows::Win32::{
    System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    },
    UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern,
        UIA_DocumentControlTypeId, UIA_GroupControlTypeId, UIA_PaneControlTypeId,
        UIA_ValuePatternId,
    },
};

const BEFORE_CHARS: i32 = 1_500;
const AFTER_CHARS: i32 = 500;
const CURSOR: &str = "[cursor]";
// A hung provider may outlive a recording. Never accumulate more blocked threads.
static BUSY: AtomicBool = AtomicBool::new(false);
struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}
struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: created only after successful CoInitializeEx, on the same worker.
        unsafe {
            CoUninitialize();
        }
    }
}

pub struct Capture {
    result: Receiver<Option<String>>,
}
pub(super) enum Target {
    Foreground(isize),
    SelftestEdit(isize),
}
impl Capture {
    /// Take an already-completed capture after transcription. Never wait for UIA:
    /// slow providers get the recording/upload time, with zero added paste latency.
    pub fn finish(self) -> Option<String> {
        self.result.try_recv().ok().flatten()
    }
}

pub fn begin() -> Option<Capture> {
    // SAFETY: snapshot only the foreground handle; COM work stays off the hotkey thread.
    let target = unsafe { GetForegroundWindow() }.0 as isize;
    spawn(move || capture(Target::Foreground(target), None))
}

/// Only the fixed EDIT control created by selftest may bypass the own-process exclusion.
pub(super) fn begin_selftest(edit: isize) -> Option<Capture> {
    spawn(move || capture(Target::SelftestEdit(edit), None))
}

/// Simulate a provider that only completes after the old hotkey-relative deadline.
pub(super) fn begin_delayed_selftest(edit: isize) -> Option<Capture> {
    spawn(move || {
        std::thread::sleep(Duration::from_millis(350));
        capture(Target::SelftestEdit(edit), None)
    })
}

fn spawn(capture: impl FnOnce() -> Option<String> + Send + 'static) -> Option<Capture> {
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let busy = Busy;
    let (tx, result) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("caret-context".into())
        .spawn(move || {
            let _busy = busy;
            let text = capture();
            let _ = tx.send(text);
        })
        .ok()?;
    Some(Capture { result })
}

pub(super) fn capture(target: Target, mut diagnostics: Option<&mut Vec<String>>) -> Option<String> {
    // SAFETY: this dedicated worker owns the COM apartment and releases all interfaces
    // before CoUninitialize. UIA methods return owned RAII interfaces/BSTRs.
    unsafe {
        use windows::Win32::Foundation::HWND;
        if let Target::Foreground(target) = target {
            let mut pid = 0;
            GetWindowThreadProcessId(HWND(target as *mut _), Some(&mut pid));
            if pid == std::process::id() {
                return None;
            }
        }
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok()?;
        let _com = Com;
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let chromium = match target {
            Target::Foreground(hwnd) => {
                let mut class = [0u16; 128];
                let len = GetClassNameW(HWND(hwnd as *mut _), &mut class) as usize;
                String::from_utf16_lossy(&class[..len]).starts_with("Chrome_")
            }
            Target::SelftestEdit(_) => false,
        };
        let started = Instant::now();
        loop {
            let element = match target {
                // A fixed owned Edit tests the channel and native caret path even if
                // another application takes focus during a simulated provider delay.
                Target::SelftestEdit(edit) => {
                    automation.ElementFromHandle(HWND(edit as *mut _)).ok()?
                }
                Target::Foreground(hwnd) => {
                    if GetForegroundWindow().0 as isize != hwnd {
                        return None;
                    }
                    let element = automation.GetFocusedElement().ok()?;
                    if GetForegroundWindow().0 as isize != hwnd {
                        return None;
                    }
                    element
                }
            };
            if let Some(log) = diagnostics.as_mut() {
                log.push(format!(
                    "focused: name={:?}, class={:?}, control={:?}, pid={:?}",
                    element.CurrentName(),
                    element.CurrentClassName(),
                    element.CurrentControlType(),
                    element.CurrentProcessId()
                ));
            }
            if element.CurrentIsPassword().ok()?.as_bool() {
                return None;
            }
            let own = element.CurrentProcessId().ok()? as u32 == std::process::id();
            match target {
                Target::SelftestEdit(edit)
                    if !own || element.CurrentNativeWindowHandle().ok()?.0 as isize != edit =>
                {
                    return None;
                }
                Target::Foreground(_) if own => return None,
                _ => {}
            }
            if let Some(text) = text::read(&element) {
                if let Some(log) = diagnostics.as_mut() {
                    log.push("path: TextPattern".into());
                }
                return Some(text);
            }
            if let Some(text) = value_context(&element, &mut diagnostics) {
                return Some(text);
            }
            // A cold Chromium tree can initially expose only a container. Give its
            // asynchronous tree two seconds on this worker, with focus pinned to the
            // original window. Capture::finish never waits for these retries, and BUSY
            // still permits only one worker if a provider hangs inside a COM call.
            let container = [
                UIA_PaneControlTypeId,
                UIA_DocumentControlTypeId,
                UIA_GroupControlTypeId,
            ]
            .contains(&element.CurrentControlType().ok()?);
            if !chromium || !container || started.elapsed() >= Duration::from_secs(2) {
                return None;
            }
            if let Some(log) = diagnostics.as_mut() {
                log.push("pending Chromium container: retry".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn value_context(
    element: &IUIAutomationElement,
    diagnostics: &mut Option<&mut Vec<String>>,
) -> Option<String> {
    // SAFETY: element belongs to the caller's COM apartment; ValuePattern returns an owned BSTR.
    unsafe {
        let value: IUIAutomationValuePattern =
            element.GetCurrentPatternAs(UIA_ValuePatternId).ok()?;
        // Read-only ValuePatterns on document containers can hold a URL rather
        // than editable text. Classic Edit fixtures may themselves be read-only.
        if element.CurrentControlType().ok()?
            != windows::Win32::UI::Accessibility::UIA_EditControlTypeId
            && value.CurrentIsReadOnly().ok()?.as_bool()
        {
            return None;
        }
        let value: Vec<u16> = value.CurrentValue().ok()?.to_vec();
        if String::from_utf16_lossy(&value).trim().is_empty() {
            return None;
        }
        Some(match edit_caret(element) {
            Some(caret) => {
                if let Some(log) = diagnostics.as_mut() {
                    log.push("path: Value + EM_GETSEL".into());
                }
                let (before, after) = value.split_at(caret.min(value.len()));
                let before = last_chars(&String::from_utf16_lossy(before), BEFORE_CHARS as usize);
                let after: String = String::from_utf16_lossy(after)
                    .chars()
                    .take(AFTER_CHARS as usize)
                    .collect();
                format!("{before}{CURSOR}{after}")
            }
            // The caret is unknown, so send plain context (as Codex does) rather than guess.
            None => {
                if let Some(log) = diagnostics.as_mut() {
                    log.push("path: Value (unknown caret)".into());
                }
                last_chars(&String::from_utf16_lossy(&value), MAX_CONTEXT_CHARS)
            }
        })
    }
}

fn last_chars(text: &str, count: usize) -> String {
    let skip = text.chars().count().saturating_sub(count);
    text.chars().skip(skip).collect()
}

/// Classic Win32 Edit controls expose only ValuePattern, but EM_GETSEL still reports
/// their caret (a UTF-16 index into the same text). Other controls return `None`.
fn edit_caret(element: &IUIAutomationElement) -> Option<usize> {
    // SAFETY: EM_GETSEL with null pointers only returns positions; the timeout and
    // SMTO_ABORTIFHUNG keep a hung target from blocking this worker.
    unsafe {
        if !element
            .CurrentClassName()
            .ok()?
            .to_string()
            .eq_ignore_ascii_case("Edit")
        {
            return None;
        }
        let hwnd = element.CurrentNativeWindowHandle().ok()?;
        let mut result = 0_usize;
        let sent = SendMessageTimeoutW(
            hwnd,
            EM_GETSEL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            100,
            Some(&mut result),
        );
        // LOWORD is the selection start; 0xFFFF means it's past what the return value can hold.
        let start = result & 0xFFFF;
        (sent.0 != 0 && start != 0xFFFF).then_some(start)
    }
}
