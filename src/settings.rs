//! Existing PascalCase settings and atomic persistence; the UI notifies its settings host.
use crate::hotkey::{self, Hotkey};
use serde::{Deserialize, Deserializer, Serialize};
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
    /// Lower other apps' playback while recording.
    pub lower_audio: bool,
    pub language: String,
    pub hotkey: Hotkey,
    pub microphone: String,
    pub first_run_done: bool,
    pub cleanup: bool,
    pub cleanup_context: bool,
    pub dictionary: Vec<String>,
    /// Toggles Listen. An unreadable value falls back alone, keeping other settings.
    #[serde(deserialize_with = "listen_hotkey")]
    pub listen_hotkey: Hotkey,
    /// Platform output-capture ID; empty follows the system default output.
    pub listen_output: String,
    pub listen_include_microphone: bool,
    /// Platform microphone ID for Listen; empty follows the system default input.
    pub listen_microphone: String,
}
fn listen_hotkey<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Hotkey, D::Error> {
    Ok(serde_json::Value::deserialize(deserializer)?
        .as_str()
        .and_then(|value| value.parse::<Hotkey>().ok())
        .filter(|key| key.main_key().is_some())
        .unwrap_or_else(Hotkey::listen_default))
}
impl Settings {
    /// The Listen shortcut WispX watches for, unless pressing it would collide with dictation.
    pub fn listen_shortcut(&self) -> Option<Hotkey> {
        hotkey::listen_conflict(&self.hotkey, &self.listen_hotkey)
            .is_none()
            .then(|| self.listen_hotkey.clone())
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            sounds: true,
            idle_pill: true,
            lower_audio: true,
            language: String::new(),
            hotkey: Hotkey::default(),
            microphone: String::new(),
            first_run_done: false,
            cleanup: true,
            cleanup_context: true,
            dictionary: Vec::new(),
            listen_hotkey: Hotkey::listen_default(),
            listen_output: String::new(),
            listen_include_microphone: false,
            listen_microphone: String::new(),
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
        Ok(Self::at(config_dir()?.join("settings.json")))
    }
    pub fn at(path: PathBuf) -> Self {
        let value = fs::read(&path)
            .ok()
            .and_then(|s| serde_json::from_slice(&s).ok())
            .unwrap_or_default();
        Self { value, path }
    }
    pub fn save(&mut self) -> io::Result<()> {
        self.value.dictionary =
            crate::transcribe::cleanup::normalize_dictionary(&self.value.dictionary);
        atomic_write(&self.path, &serde_json::to_vec_pretty(&self.value)?)
    }
}

/// Shared WispX directory for settings and aggregate statistics.
pub(crate) fn config_dir() -> io::Result<PathBuf> {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("WispX"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Application data directory is unavailable",
            )
        })
}

/// Syncs a uniquely created sibling before atomically replacing the destination.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    // Failure modes: settings written before Listen lose every value or fail to load;
    // a broken Listen shortcut resets the dictation shortcut; a saved file drops the new
    // fields; a conflicting Listen shortcut is still watched.
    use super::*;

    #[test]
    fn older_settings_load_with_listen_defaults() {
        let old =
            r#"{"Sounds":false,"Hotkey":"Ctrl+Shift+D","Microphone":"USB","Dictionary":["WispX"]}"#;
        let settings: Settings = serde_json::from_str(old).expect("old settings");
        assert!(!settings.sounds);
        assert_eq!(settings.hotkey.to_string(), "Ctrl + Shift + D");
        assert_eq!(settings.microphone, "USB");
        assert_eq!(settings.listen_hotkey, Hotkey::listen_default());
        assert!(!settings.listen_include_microphone);
        assert!(settings.listen_output.is_empty());
    }

    #[test]
    fn a_bad_listen_shortcut_falls_back_alone() {
        for value in [r#""Nonsense""#, r#""Ctrl+Shift""#, "42", "null"] {
            let json = format!(r#"{{"Hotkey":"F9","ListenHotkey":{value}}}"#);
            let settings: Settings = serde_json::from_str(&json).expect(&json);
            assert_eq!(settings.hotkey.to_string(), "F9", "{value}");
            assert_eq!(settings.listen_hotkey, Hotkey::listen_default(), "{value}");
        }
    }

    #[test]
    fn listen_settings_round_trip_and_conflicts_disable_the_shortcut() {
        let mut settings = Settings {
            listen_hotkey: "Ctrl+Alt+L".parse().expect("chord"),
            listen_output: "monitor".into(),
            listen_include_microphone: true,
            listen_microphone: "mic".into(),
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).expect("serialize");
        assert!(json.contains(r#""ListenHotkey":"Ctrl+Alt+L""#), "{json}");
        let back: Settings = serde_json::from_str(&json).expect("parse");
        assert_eq!(back.listen_hotkey, settings.listen_hotkey);
        assert_eq!(back.listen_output, "monitor");
        assert!(back.listen_include_microphone);
        assert_eq!(back.listen_microphone, "mic");
        assert!(settings.listen_shortcut().is_some());
        settings.hotkey = "Ctrl+Alt".parse().expect("chord");
        assert!(settings.listen_shortcut().is_none());
    }
}
