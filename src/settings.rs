//! Existing PascalCase settings and atomic persistence; the UI notifies its settings host.
use crate::hotkey::Hotkey;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    pub sounds: bool,
    pub idle_pill: bool,
    pub language: String,
    pub hotkey: Hotkey,
    pub microphone: String,
    pub first_run_done: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            sounds: true,
            idle_pill: true,
            language: String::new(),
            hotkey: Hotkey::default(),
            microphone: String::new(),
            first_run_done: false,
        }
    }
}
/// The UI owns the value and calls OptionsHost::settings_changed after a successful save.
pub struct SettingsStore {
    pub value: Settings,
    path: PathBuf,
}
impl SettingsStore {
    pub fn load() -> io::Result<Self> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Application data directory is unavailable",
                )
            })?;
        Ok(Self::at(base.join("WispX/settings.json")))
    }
    pub fn at(path: PathBuf) -> Self {
        let value = fs::read(&path)
            .ok()
            .and_then(|s| serde_json::from_slice(&s).ok())
            .unwrap_or_default();
        Self { value, path }
    }
    pub fn save(&mut self) -> io::Result<()> {
        atomic_write(
            &self.path,
            &self.path.with_extension("json.tmp"),
            &serde_json::to_vec_pretty(&self.value)?,
        )
    }
}

/// Syncs a uniquely created sibling before atomically replacing the destination.
/// The legacy temp-path argument is ignored to keep existing auth callers compatible.
pub(crate) fn atomic_write(path: &Path, _tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Destination needs a filename")
    })?;
    let (tmp, mut file) = loop {
        let mut name = name.to_os_string();
        name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let tmp = path.with_file_name(name);
        match OpenOptions::new().create_new(true).write(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        // Close even on a write/flush failure so Windows permits cleanup.
        drop(file);
        written?;
        #[cfg(windows)]
        {
            crate::platform::windows::files::replace(&tmp, path)
        }
        #[cfg(not(windows))]
        {
            fs::rename(&tmp, path)
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}
