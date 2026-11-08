//! Fixture driver only: expose Chromium's lazy tree so diagnostics can find a named field.
//! Recording uses GetFocusedElement directly and never calls this helper.
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM},
        UI::{
            Accessibility::{AccessibleObjectFromWindow, IAccessible},
            WindowsAndMessaging::{EnumChildWindows, GetClassNameW, IsWindowVisible, OBJID_CLIENT},
        },
    },
    core::{BOOL, Interface},
};

pub(super) fn activate(hwnd: HWND) -> Option<IAccessible> {
    // SAFETY: EnumChildWindows calls the callback synchronously; `renderer` remains
    // valid throughout. The COM API returns an owned IAccessible reference.
    unsafe {
        let mut renderer = HWND::default();
        let _ = EnumChildWindows(
            Some(hwnd),
            Some(find_renderer),
            LPARAM((&mut renderer as *mut HWND) as isize),
        );
        if renderer.is_invalid() {
            return None;
        }
        let mut object = std::ptr::null_mut();
        AccessibleObjectFromWindow(
            renderer,
            OBJID_CLIENT.0 as u32,
            &IAccessible::IID,
            &mut object,
        )
        .ok()?;
        Some(IAccessible::from_raw(object))
    }
}

unsafe extern "system" fn find_renderer(hwnd: HWND, target: LPARAM) -> BOOL {
    let mut class = [0u16; 128];
    // SAFETY: Win32 writes within the provided buffer; target is the live HWND
    // pointer passed by activate to this synchronous enumeration.
    unsafe {
        let len = GetClassNameW(hwnd, &mut class) as usize;
        if IsWindowVisible(hwnd).as_bool()
            && String::from_utf16_lossy(&class[..len]) == "Chrome_RenderWidgetHostHWND"
        {
            *(target.0 as *mut HWND) = hwnd;
            return BOOL(false as i32);
        }
    }
    BOOL(true as i32)
}
