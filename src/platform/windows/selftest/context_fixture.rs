//! Owned, read-only native fixtures for context privacy and channel lifetime checks.
use super::super::context;
use super::{Step, step};
use std::{
    io, thread,
    time::{Duration, Instant},
};
const CONTEXT_BEFORE: &str = "Before the cursor.\r\nWispX ";
const CONTEXT_AFTER: &str = "fits here.\r\nAfter the cursor.";
#[derive(Clone, Copy)]
pub(super) enum ContextField {
    Multiline,
    Password,
}
pub(super) struct ContextWindow(windows::Win32::Foundation::HWND);
impl Drop for ContextWindow {
    fn drop(&mut self) {
        // SAFETY: selftest owns this window and destroys it on its creating UI thread.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}
impl ContextWindow {
    pub(super) fn open(field: ContextField) -> windows::core::Result<(Self, isize)> {
        use windows::{
            Win32::{
                Foundation::{LPARAM, WPARAM},
                UI::{
                    Controls::EM_SETSEL,
                    Input::KeyboardAndMouse::SetFocus,
                    WindowsAndMessaging::{
                        CreateWindowExW, ES_MULTILINE, ES_PASSWORD, ES_READONLY, SendMessageW,
                        WINDOW_STYLE, WS_CHILD, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
                    },
                },
            },
            core::{PCWSTR, w},
        };
        let (text, style, caret) = match field {
            ContextField::Multiline => (
                format!("{CONTEXT_BEFORE}{CONTEXT_AFTER}"),
                ES_MULTILINE,
                CONTEXT_BEFORE.encode_utf16().count(),
            ),
            ContextField::Password => ("fixed selftest password".into(), ES_PASSWORD, 0),
        };
        let text = super::super::files::wide(text);
        // SAFETY: registered system classes, owned parent/child handles, and a NUL-terminated
        // text buffer valid for creation. Messages target only our own EDIT control.
        unsafe {
            let window = Self(CreateWindowExW(
                WS_EX_TOPMOST,
                w!("STATIC"),
                w!("WispX context selftest"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                80,
                80,
                520,
                200,
                None,
                None,
                None,
                None,
            )?);
            let edit = CreateWindowExW(
                Default::default(),
                w!("EDIT"),
                PCWSTR(text.as_ptr()),
                WS_CHILD | WS_VISIBLE | WINDOW_STYLE((style | ES_READONLY) as u32),
                12,
                12,
                470,
                130,
                Some(window.0),
                None,
                None,
                None,
            )?;
            SendMessageW(
                edit,
                EM_SETSEL,
                Some(WPARAM(caret)),
                Some(LPARAM(caret as isize)),
            );
            SetFocus(Some(edit))?;
            Ok((window, edit.0 as isize))
        }
    }
}

pub(super) fn verify(
    mark: &impl Fn(&str) -> io::Result<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    step(Step::ContextOpen(ContextField::Multiline, tx))?;
    let edit = rx.recv()?;
    let context_start = Instant::now();
    let capture = context::begin_selftest(edit).ok_or("Context worker busy")?;
    thread::sleep(Duration::from_millis(500));
    let captured = capture.finish();
    step(Step::ContextClose)?;
    mark(&format!(
        "CONTEXT ({} ms): {captured:?}",
        context_start.elapsed().as_millis()
    ))?;
    // Native EDIT exposes only ValuePattern; its caret comes from EM_GETSEL.
    let expected = format!("{CONTEXT_BEFORE}[cursor]{CONTEXT_AFTER}");
    if captured.as_deref() != Some(expected.as_str()) {
        return Err("Context did not mark the caret in the fixed selftest EDIT".into());
    }
    mark("CONTEXT: caret position matched")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    step(Step::ContextOpen(ContextField::Multiline, tx))?;
    let late = context::begin_delayed_selftest(rx.recv()?).ok_or("Context worker busy")?;
    thread::sleep(Duration::from_millis(750));
    let captured = late.finish();
    step(Step::ContextClose)?;
    mark(&format!("CONTEXT LATE: {captured:?}"))?;
    if captured.as_deref() != Some(expected.as_str()) {
        return Err("Completed capture was discarded while the user was dictating".into());
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    step(Step::ContextOpen(ContextField::Multiline, tx))?;
    let edit = rx.recv()?;
    let pending = context::begin_delayed_selftest(edit).ok_or("Context worker busy")?;
    let start = Instant::now();
    if pending.finish().is_some() {
        return Err("Pending capture unexpectedly completed".into());
    }
    mark(&format!(
        "CONTEXT PENDING: omitted without waiting ({} us)",
        start.elapsed().as_micros()
    ))?;
    // Keep the owned fixture alive through the simulated slow provider call.
    thread::sleep(Duration::from_millis(750));
    if context::capture(context::Target::Foreground(edit), None).is_some() {
        return Err("WispX's own window was not excluded".into());
    }
    step(Step::ContextClose)?;
    mark("CONTEXT: own window excluded")?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    step(Step::ContextOpen(ContextField::Password, tx))?;
    let capture = context::begin_selftest(rx.recv()?).ok_or("Context worker busy")?;
    thread::sleep(Duration::from_millis(500));
    let password = capture.finish();
    step(Step::ContextClose)?;
    if password.is_some() {
        return Err("Password control was not excluded from context".into());
    }
    mark("CONTEXT: password field excluded")?;

    Ok(())
}
