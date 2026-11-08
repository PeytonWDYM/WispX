//! Best-effort caret context. UI Automation and its COM objects stay on one worker.
use crate::transcribe::cleanup::MAX_CONTEXT_CHARS;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
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
        let value = value.CurrentValue().ok()?.to_string();
        let tail: String = value
            .chars()
            .rev()
            .take(MAX_CONTEXT_CHARS - CURSOR.len())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        (!tail.trim().is_empty()).then(|| format!("{tail}{CURSOR}"))
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
