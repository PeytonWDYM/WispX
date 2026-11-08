//! Read and clamp caret ranges without changing the application's selection.
use super::{AFTER_CHARS, BEFORE_CHARS, CURSOR, MAX_CONTEXT_CHARS};
use windows::{
    Win32::UI::Accessibility::{
        IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTextPattern2,
        TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start, TextUnit_Character,
        UIA_TextPattern2Id, UIA_TextPatternId,
    },
    core::BOOL,
};

pub(super) fn read(element: &IUIAutomationElement) -> Option<String> {
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
        // Chromium exposes the document's TextPattern even on individual edit fields.
        // RangeFromChild locates the field; movement by character can still escape it,
        // so clamp both endpoints before reading. Native document editors may expose
        // only DocumentRange.
        let document = match pattern.RangeFromChild(element) {
            Ok(field) => field,
            Err(_) if element.CurrentFrameworkId().ok()? == "Chrome" => return None,
            Err(_) => pattern.DocumentRange().ok()?,
        };
        if selected
            .CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &document,
                TextPatternRangeEndpoint_Start,
            )
            .ok()?
            < 0
            || selected
                .CompareEndpoints(
                    TextPatternRangeEndpoint_End,
                    &document,
                    TextPatternRangeEndpoint_End,
                )
                .ok()?
                > 0
        {
            return None;
        }
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
        if before
            .CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &document,
                TextPatternRangeEndpoint_Start,
            )
            .ok()?
            < 0
        {
            before
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_Start,
                    &document,
                    TextPatternRangeEndpoint_Start,
                )
                .ok()?;
        }
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
        if after
            .CompareEndpoints(
                TextPatternRangeEndpoint_End,
                &document,
                TextPatternRangeEndpoint_End,
            )
            .ok()?
            > 0
        {
            after
                .MoveEndpointByRange(
                    TextPatternRangeEndpoint_End,
                    &document,
                    TextPatternRangeEndpoint_End,
                )
                .ok()?;
        }
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
