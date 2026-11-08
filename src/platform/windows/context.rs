//! Best-effort caret context. UI Automation and its COM objects stay on one worker.
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
        WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW},
    },
};
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
            IUIAutomationTextPattern2, IUIAutomationValuePattern, TextPatternRangeEndpoint_End,
            TextPatternRangeEndpoint_Start, TextUnit_Character, UIA_TextPattern2Id,
            UIA_TextPatternId, UIA_ValuePatternId,
        },
    },
    core::BOOL,
};

pub const CAPTURE_TIMEOUT: Duration = Duration::from_millis(250);
const BEFORE_CHARS: i32 = 1_500;
const AFTER_CHARS: i32 = 500;
const CURSOR: &str = "[cursor]";
// A hung provider may outlive the deadline. Never accumulate more blocked threads.
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
    deadline: Instant,
}
impl Capture {
    /// Called by the finish worker, never the UI thread. A stale/hung capture is omitted.
    pub fn finish(self) -> Option<String> {
        self.result
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .ok()
            .flatten()
    }
}

pub fn begin() -> Option<Capture> {
    spawn(None)
}

/// Only the fixed EDIT control created by selftest may bypass the own-process exclusion.
pub(super) fn begin_selftest(edit: isize) -> Option<Capture> {
    spawn(Some(edit))
}

fn spawn(test_edit: Option<isize>) -> Option<Capture> {
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let busy = Busy;
    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    let (tx, result) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("caret-context".into())
        .spawn(move || {
            let _busy = busy;
            let text = capture(test_edit);
            let _ = tx.send(if Instant::now() <= deadline {
                text
            } else {
                None
            });
        })
        .ok()?;
    Some(Capture { result, deadline })
}

fn capture(test_edit: Option<isize>) -> Option<String> {
    // SAFETY: this dedicated worker owns the COM apartment and releases all interfaces
    // before CoUninitialize. UIA methods return owned RAII interfaces/BSTRs.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok()?;
        let _com = Com;
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        if let Some(edit) = test_edit {
            // The selftest owns this handle until capture finishes. UIA focus also
            // activates the owned window when Windows denies background SetForegroundWindow.
            let element = automation
                .ElementFromHandle(windows::Win32::Foundation::HWND(edit as *mut _))
                .ok()?;
            element.SetFocus().ok()?;
        }
        let element = automation.GetFocusedElement().ok()?;
        if element.CurrentIsPassword().ok()?.as_bool() {
            return None;
        }
        let own = element.CurrentProcessId().ok()? as u32 == std::process::id();
        match test_edit {
            Some(edit) if !own || element.CurrentNativeWindowHandle().ok()?.0 as isize != edit => {
                return None;
            }
            None if own => return None,
            _ => {}
        }
        if let Some(text) = text_context(&element) {
            return Some(text);
        }
        let value: IUIAutomationValuePattern =
            element.GetCurrentPatternAs(UIA_ValuePatternId).ok()?;
        let value: Vec<u16> = value.CurrentValue().ok()?.to_vec();
        if String::from_utf16_lossy(&value).trim().is_empty() {
            return None;
        }
        Some(match edit_caret(&element) {
            Some(caret) => {
                let (before, after) = value.split_at(caret.min(value.len()));
                let before = last_chars(&String::from_utf16_lossy(before), BEFORE_CHARS as usize);
                let after: String = String::from_utf16_lossy(after)
                    .chars()
                    .take(AFTER_CHARS as usize)
                    .collect();
                format!("{before}{CURSOR}{after}")
            }
            // The caret is unknown, so send plain context (as Codex does) rather than guess.
            None => last_chars(&String::from_utf16_lossy(&value), MAX_CONTEXT_CHARS),
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

fn text_context(element: &IUIAutomationElement) -> Option<String> {
    // SAFETY: every range belongs to this element's text provider on the initialized
    // worker. Only clones are moved; the application's selection is never modified.
    unsafe {
        let pattern: IUIAutomationTextPattern =
            element.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
        let selected = pattern
            .GetSelection()
            .ok()
            .and_then(|ranges| ranges.GetElement(0).ok())
            .or_else(|| {
                let pattern: IUIAutomationTextPattern2 =
                    element.GetCurrentPatternAs(UIA_TextPattern2Id).ok()?;
                pattern.GetCaretRange(&mut BOOL::default()).ok()
            })?;
        let document = pattern.DocumentRange().ok()?;
        let before = document.Clone().ok()?;
        before
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                &selected,
                TextPatternRangeEndpoint_Start,
            )
            .ok()?;
        before
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                &selected,
                TextPatternRangeEndpoint_Start,
            )
            .ok()?;
        before
            .MoveEndpointByUnit(
                TextPatternRangeEndpoint_Start,
                TextUnit_Character,
                -BEFORE_CHARS,
            )
            .ok()?;
        let after = document.Clone().ok()?;
        after
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_Start,
                &selected,
                TextPatternRangeEndpoint_End,
            )
            .ok()?;
        after
            .MoveEndpointByRange(
                TextPatternRangeEndpoint_End,
                &selected,
                TextPatternRangeEndpoint_End,
            )
            .ok()?;
        after
            .MoveEndpointByUnit(
                TextPatternRangeEndpoint_End,
                TextUnit_Character,
                AFTER_CHARS,
            )
            .ok()?;
        let before = before.GetText(BEFORE_CHARS).ok()?.to_string();
        let after = after.GetText(AFTER_CHARS).ok()?.to_string();
        if before.trim().is_empty() && after.trim().is_empty() {
            return None;
        }
        let after: String = after
            .chars()
            .take(MAX_CONTEXT_CHARS - before.chars().count() - CURSOR.len())
            .collect();
        Some(format!("{before}{CURSOR}{after}"))
    }
}
