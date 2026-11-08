//! WPF's preferred fonts with the same Windows 10 fallbacks.
use super::files::wide;
use windows::{Win32::Graphics::DirectWrite::*, core::PCWSTR};

pub fn family(
    factory: &IDWriteFactory,
    preferred: &'static str,
    fallback: &'static str,
) -> windows::core::Result<&'static str> {
    let name = wide(preferred);
    // SAFETY: the collection and output values belong to this call; name is NUL-terminated.
    unsafe {
        let mut fonts = None;
        factory.GetSystemFontCollection(&mut fonts, false)?;
        let fonts = fonts.expect("successful GetSystemFontCollection returns a collection");
        let mut index = 0;
        let mut exists = Default::default();
        fonts.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists)?;
        Ok(if exists.as_bool() {
            preferred
        } else {
            fallback
        })
    }
}

pub fn options_families() -> windows::core::Result<(&'static str, &'static str)> {
    // SAFETY: the factory is created for use only on this UI thread.
    let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }?;
    Ok((
        family(&factory, "Segoe UI Variable", "Segoe UI")?,
        family(&factory, "Segoe Fluent Icons", "Segoe MDL2 Assets")?,
    ))
}
