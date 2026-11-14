//! Asks where to save Listen notes: the desktop's file chooser portal, or KDE's or
//! GNOME's dialog tool when no portal answers. Blocking; call it off the UI thread.
use ashpd::desktop::{
    ResponseError,
    file_chooser::{FileFilter, SelectedFiles},
};
use std::{path::PathBuf, process::Command};

/// The chosen path, or `None` when the person cancelled.
pub fn choose(name: &str) -> Result<Option<PathBuf>, String> {
    super::register_app();
    let portal = futures_lite::future::block_on(async {
        let request = SelectedFiles::save_file()
            .title("Save notes")
            .accept_label("Save")
            .modal(true)
            .current_name(name)
            .filter(FileFilter::new("Markdown").glob("*.md"))
            .send()
            .await?;
        request.response()
    });
    let error = match portal {
        Ok(files) => {
            return files
                .uris()
                .first()
                .and_then(|uri| file_path(uri.as_str()))
                .map(Some)
                .ok_or_else(|| "The file chooser picked a location WispX can't write to".into());
        }
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => return Ok(None),
        Err(error) => error.to_string(),
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let tools: [(&str, Vec<String>); 2] = [
        (
            "kdialog",
            vec![
                "--getsavefilename".into(),
                home.join(name).display().to_string(),
                "*.md|Markdown".into(),
            ],
        ),
        (
            "zenity",
            vec![
                "--file-selection".into(),
                "--save".into(),
                "--confirm-overwrite".into(),
                format!("--filename={}", home.join(name).display()),
            ],
        ),
    ];
    for (tool, args) in tools {
        let Ok(output) = Command::new(tool).args(&args).output() else {
            continue;
        };
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        return Ok((output.status.success() && !path.is_empty()).then(|| PathBuf::from(path)));
    }
    Err(format!(
        "No file chooser is available ({error}). Install xdg-desktop-portal, or copy the notes instead."
    ))
}

/// A local path from a `file://` URI, decoding percent escapes.
fn file_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let encoded = encoded.strip_prefix("localhost").unwrap_or(encoded);
    if !encoded.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%'
            && let Some(hex) = tail.get(..2)
            && let Ok(value) = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16)
        {
            bytes.push(value);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    // Failure modes: escaped spaces or UTF-8 stay encoded; non-file URIs are written to.
    use super::*;

    #[test]
    fn decodes_file_uris_only() {
        assert_eq!(
            file_path("file:///home/me/My%20Notes/caf%C3%A9.md"),
            Some(PathBuf::from("/home/me/My Notes/café.md"))
        );
        assert_eq!(
            file_path("file://localhost/tmp/a.md"),
            Some(PathBuf::from("/tmp/a.md"))
        );
        assert_eq!(file_path("https://example.com/a.md"), None);
        assert_eq!(file_path("file://server/tmp/a.md"), None);
        assert_eq!(file_path("file://localhost-other/tmp/a.md"), None);
        assert_eq!(file_path("file://relative.md"), None);
    }
}
