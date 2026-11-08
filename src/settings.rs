//! Existing PascalCase settings and atomic persistence with live change notifications.
use crate::hotkey::Hotkey;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
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
/// The UI owns the value; subscribers receive a snapshot only after a successful save.
pub struct SettingsStore {
    pub value: Settings,
    path: PathBuf,
    listeners: Vec<Sender<Settings>>,
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
        Self {
            value,
            path,
            listeners: Vec::new(),
        }
    }
    pub fn subscribe(&mut self) -> Receiver<Settings> {
        let (tx, rx) = mpsc::channel();
        self.listeners.push(tx);
        rx
    }
    pub fn save(&mut self) -> io::Result<()> {
        atomic_write(
            &self.path,
            &self.path.with_extension("json.tmp"),
            &serde_json::to_vec_pretty(&self.value)?,
        )?;
        self.listeners
            .retain(|tx| tx.send(self.value.clone()).is_ok());
        Ok(())
    }
}

/// Syncs the new contents before atomically replacing the destination.
pub(crate) fn atomic_write(path: &Path, tmp: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(windows)]
    {
        crate::platform::windows::files::replace(tmp, path)
    }
    #[cfg(not(windows))]
    {
        fs::rename(tmp, path)
    }
}
