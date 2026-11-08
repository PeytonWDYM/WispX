//! Explicit fixture-only diagnostic. Never runs in the installed app's recording path.
use super::context;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, time::Instant};
use windows::{
    Win32::System::Variant::VARIANT,
    Win32::{
        Foundation::HWND,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationTextPattern, TextPatternRangeEndpoint_End,
            TextPatternRangeEndpoint_Start, TextUnit_Character, TreeScope_Descendants,
            UIA_ControlTypePropertyId, UIA_NamePropertyId, UIA_TextPatternId,
        },
    },
};

#[derive(Deserialize)]
struct Request {
    hwnd: isize,
    name: Option<String>,
    control_type: Option<i32>,
    caret: Option<i32>,
    out: std::path::PathBuf,
    #[serde(default = "repeats")]
    repeats: usize,
}
fn repeats() -> usize {
    3
}

#[derive(Serialize)]
struct Sample {
    focus_ms: u128,
    capture_ms: u128,
    old_deadline_would_discard: bool,
    diagnostics: Vec<String>,
    text: Option<String>,
}

/// The caller must supply only a window it created, containing synthetic fixture text.
pub fn run(request: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let request: Request = serde_json::from_slice(&fs::read(request)?)?;
    let mut samples = Vec::new();
    // SAFETY: this dev-only process owns its COM apartment; interfaces drop before uninitialization.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            use windows::Win32::UI::WindowsAndMessaging::{
                GetForegroundWindow, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
                SetForegroundWindow, SetWindowPos,
            };
            let hwnd = HWND(request.hwnd as *mut _);
            // Keep only the owned fixture visible: Chromium reparents an occluded
            // renderer away from its browser HWND, hiding it from accessibility.
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            )?;
            let _ = SetForegroundWindow(hwnd);
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            automation.ElementFromHandle(hwnd)?.SetFocus()?;
            // Investigation: request the window's MSAA object, which Chromium documents
            // as an assistive-client signal, before the UIA field lookup.
            let mut accessible = None;
            for _ in 0..request.repeats {
                let start = Instant::now();
                if request.name.is_some() || request.control_type.is_some() {
                    let root = automation
                        .ElementFromHandle(hwnd)
                        .map_err(|e| format!("root: {e}"))?;
                    let condition = match &request.name {
                        Some(name) => automation.CreatePropertyCondition(
                            UIA_NamePropertyId,
                            &VARIANT::from(name.as_str()),
                        )?,
                        None => automation.CreatePropertyCondition(
                            UIA_ControlTypePropertyId,
                            &VARIANT::from(request.control_type.ok_or("Missing fixture selector")?),
                        )?,
                    };
                    // A first query starts Chromium's lazy accessibility initialization.
                    // Poll only in the fixture driver, before measuring production capture.
                    let target = loop {
                        if accessible.is_none() {
                            accessible = super::chromium::activate(hwnd);
                        }
                        if let Ok(target) = automation
                            .ElementFromHandle(hwnd)?
                            .FindFirst(TreeScope_Descendants, &condition)
                        {
                            break target;
                        }
                        if start.elapsed() > std::time::Duration::from_secs(5) {
                            let all = root.FindAll(
                                TreeScope_Descendants,
                                &automation.CreateTrueCondition()?,
                            )?;
                            let mut names = Vec::new();
                            for i in 0..all.Length()? {
                                let c = all.GetElement(i)?;
                                let n = c.CurrentName()?.to_string();
                                if n.starts_with("WispX") {
                                    names.push(n);
                                }
                            }
                            return Err(format!(
                                "Fixture field not available; root {:?}; fields {names:?}; activated {}",
                                root.CurrentName(), accessible.is_some()
                            )
                            .into());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    };
                    target.SetFocus().map_err(|e| format!("focus: {e}"))?;
                    if let Some(caret) = request.caret {
                        let pattern: IUIAutomationTextPattern =
                            target.GetCurrentPatternAs(UIA_TextPatternId)?;
                        let range = pattern.DocumentRange()?;
                        range.MoveEndpointByRange(
                            TextPatternRangeEndpoint_End,
                            &range,
                            TextPatternRangeEndpoint_Start,
                        )?;
                        range.Move(TextUnit_Character, caret)?;
                        range.Select()?;
                        // Electron applies UIA selection asynchronously. Wait only in
                        // this fixture driver until it acknowledges our requested caret.
                        let placed = Instant::now();
                        loop {
                            let selected = pattern.GetSelection()?.GetElement(0)?;
                            if selected.CompareEndpoints(
                                TextPatternRangeEndpoint_Start,
                                &range,
                                TextPatternRangeEndpoint_Start,
                            )? == 0
                            {
                                break;
                            }
                            if placed.elapsed() > std::time::Duration::from_secs(1) {
                                return Err(
                                    "Fixture editor did not acknowledge the requested caret".into(),
                                );
                            }
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                    }
                }
                if GetForegroundWindow() != hwnd {
                    return Err(
                        "Fixture window did not receive focus; refusing to read another window"
                            .into(),
                    );
                }
                let focus_ms = start.elapsed().as_millis();
                let start = Instant::now();
                let mut diagnostics = Vec::new();
                let text = context::capture(
                    context::Target::Foreground(request.hwnd),
                    Some(&mut diagnostics),
                );
                if text.is_none() {
                    diagnostics.push("path: none".into());
                }
                let capture_ms = start.elapsed().as_millis();
                samples.push(Sample {
                    focus_ms,
                    capture_ms,
                    old_deadline_would_discard: capture_ms > 250,
                    diagnostics,
                    text,
                });
            }
            Ok(())
        })();
        CoUninitialize();
        result?;
    }
    fs::write(request.out, serde_json::to_vec_pretty(&samples)?)?;
    Ok(())
}

/// Compare fixed synthetic prompts against the live cleanup endpoint.
pub fn cleanup(out: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let client = crate::transcribe::ChatGpt::default();
    let mut samples = Vec::new();
    for surrounding in [
        "",
        "Basically, we're testing the[cursor]",
        "Basically, we're testing the [cursor]",
    ] {
        for _ in 0..3 {
            let start = Instant::now();
            let cleaned = client.cleanup(
                "System of the desires of the world.".into(),
                true,
                surrounding,
                &[],
            );
            samples.push(serde_json::json!({"surrounding": surrounding, "cleaned": cleaned, "ms": start.elapsed().as_millis()}));
        }
    }
    fs::write(out, serde_json::to_vec_pretty(&samples)?)?;
    Ok(())
}
