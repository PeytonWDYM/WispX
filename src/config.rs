//! `config.json`: strict parsing, validation and atomic updates.
//!
//! Unknown keys are errors, so a typo cannot silently change which backend
//! receives someone's voice.

use crate::paths;
use serde_json::{Map, Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const CHOICES: &'static str = concat!($($text, " "),+);

            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }
    };
}

string_enum! {
    /// Which service turns audio into text. There is deliberately no automatic
    /// fallback between them: audio only leaves the machine when asked to.
    Backend { Local => "local", ChatGpt => "chatgpt" }
}

string_enum! {
    /// `auto` and `portal` both use the GlobalShortcuts portal, except that
    /// `auto` stays quiet outside a Wayland session.
    HotkeyMode { Auto => "auto", Portal => "portal", None => "none" }
}

string_enum! {
    PasteMode { Auto => "auto", Copy => "copy", None => "none" }
}

string_enum! {
    /// `auto` means Ctrl+Shift+V in terminals and Ctrl+V everywhere else.
    PasteShortcut {
        Auto => "auto",
        CtrlV => "ctrl+v",
        CtrlShiftV => "ctrl+shift+v",
        ShiftInsert => "shift+insert",
    }
}

string_enum! {
    NotifyLevel { All => "all", Errors => "errors", None => "none" }
}

string_enum! {
    RecorderChoice {
        Auto => "auto",
        PwRecord => "pw-record",
        Parec => "parec",
        Arecord => "arecord",
        WaveIn => "wavein",
    }
}

/// The triggers suggested to the desktop, in XDG shortcut syntax. They only
/// seed the first request: afterwards the user's choice in the desktop's own
/// settings wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutsConfig {
    pub hold: String,
    pub hands_free: String,
    pub cancel: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioConfig {
    pub recorder: RecorderChoice,
    pub device: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WhisperConfig {
    pub binary: String,
    pub model: PathBuf,
    /// 0 picks a sensible count from the available CPUs.
    pub threads: u32,
    pub timeout_secs: u64,
    pub extra_args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatGptConfig {
    pub auth_file: PathBuf,
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub backend: Backend,
    /// ISO language hint such as "en". Empty or "auto" means auto-detect.
    pub language: String,
    pub sounds: bool,
    /// Whether the small pill stays on screen between dictations.
    pub idle_pill: bool,
    pub notifications: NotifyLevel,
    pub hotkey: HotkeyMode,
    pub shortcuts: ShortcutsConfig,
    pub paste: PasteMode,
    pub paste_shortcut: PasteShortcut,
    pub restore_clipboard: bool,
    pub min_hold_ms: u64,
    pub max_recording_secs: u64,
    pub silence_threshold: f32,
    pub audio: AudioConfig,
    pub whisper: WhisperConfig,
    pub chatgpt: ChatGptConfig,
}

impl Config {
    pub fn defaults() -> Result<Self, String> {
        Ok(Config {
            backend: Backend::Local,
            language: String::new(),
            sounds: true,
            idle_pill: true,
            notifications: NotifyLevel::All,
            hotkey: HotkeyMode::Auto,
            shortcuts: ShortcutsConfig {
                hold: "CTRL+LOGO+d".to_string(),
                hands_free: "CTRL+LOGO+space".to_string(),
                // Not Escape: Ctrl+Meta+Esc is KWin's stock Kill Window shortcut.
                cancel: "CTRL+LOGO+BackSpace".to_string(),
            },
            paste: PasteMode::Auto,
            paste_shortcut: PasteShortcut::Auto,
            restore_clipboard: true,
            min_hold_ms: 300,
            max_recording_secs: 600,
            silence_threshold: 0.12,
            audio: AudioConfig {
                recorder: RecorderChoice::Auto,
                device: None,
            },
            whisper: WhisperConfig {
                binary: "whisper-cli".to_string(),
                model: paths::data_dir()?.join("models").join("ggml-base.bin"),
                threads: 0,
                timeout_secs: 900,
                extra_args: Vec::new(),
            },
            chatgpt: ChatGptConfig {
                auth_file: paths::default_codex_auth_file()?,
                timeout_secs: 120,
            },
        })
    }

    /// Loads `path`, or the defaults when it does not exist. A file that
    /// exists but is invalid is an error rather than something to ignore.
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::defaults(),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        let mut root = Fields::new(value, "")?;
        let mut config = Self::defaults()?;

        if let Some(v) = root.choice("backend", Backend::parse, Backend::CHOICES)? {
            config.backend = v;
        }
        if let Some(v) = root.string("language")? {
            config.language = validate_language(&v)?;
        }
        if let Some(v) = root.boolean("sounds")? {
            config.sounds = v;
        }
        if let Some(v) = root.boolean("idle_pill")? {
            config.idle_pill = v;
        }
        if let Some(v) = root.choice("notifications", NotifyLevel::parse, NotifyLevel::CHOICES)? {
            config.notifications = v;
        }
        if let Some(v) = root.choice("hotkey", HotkeyMode::parse, HotkeyMode::CHOICES)? {
            config.hotkey = v;
        }
        if let Some(v) = root.choice("paste", PasteMode::parse, PasteMode::CHOICES)? {
            config.paste = v;
        }
        if let Some(v) = root.choice(
            "paste_shortcut",
            PasteShortcut::parse,
            PasteShortcut::CHOICES,
        )? {
            config.paste_shortcut = v;
        }
        if let Some(v) = root.boolean("restore_clipboard")? {
            config.restore_clipboard = v;
        }
        if let Some(v) = root.integer("min_hold_ms", 0, 5_000)? {
            config.min_hold_ms = v;
        }
        if let Some(v) = root.integer("max_recording_secs", 1, 3_600)? {
            config.max_recording_secs = v;
        }
        if let Some(v) = root.number("silence_threshold", 0.0, 1.0)? {
            config.silence_threshold = v as f32;
        }

        if let Some(mut shortcuts) = root.section("shortcuts")? {
            if let Some(v) = shortcuts.string("hold")? {
                config.shortcuts.hold = validate_trigger("shortcuts.hold", v)?;
            }
            if let Some(v) = shortcuts.string("hands_free")? {
                config.shortcuts.hands_free = validate_trigger("shortcuts.hands_free", v)?;
            }
            if let Some(v) = shortcuts.string("cancel")? {
                config.shortcuts.cancel = validate_trigger("shortcuts.cancel", v)?;
            }
            shortcuts.finish()?;
        }

        if let Some(mut audio) = root.section("audio")? {
            if let Some(v) =
                audio.choice("recorder", RecorderChoice::parse, RecorderChoice::CHOICES)?
            {
                config.audio.recorder = v;
            }
            if let Some(v) = audio.nullable_string("device")? {
                config.audio.device = v.map(|d| validate_arg("audio.device", d)).transpose()?;
            }
            audio.finish()?;
        }

        if let Some(mut whisper) = root.section("whisper")? {
            if let Some(v) = whisper.string("binary")? {
                config.whisper.binary = validate_binary(&v)?;
            }
            if let Some(v) = whisper.string("model")? {
                config.whisper.model =
                    paths::expand_absolute(&v).map_err(|e| format!("whisper.model: {e}"))?;
            }
            if let Some(v) = whisper.integer("threads", 0, 1_024)? {
                config.whisper.threads = v as u32;
            }
            if let Some(v) = whisper.integer("timeout_secs", 1, 86_400)? {
                config.whisper.timeout_secs = v;
            }
            if let Some(v) = whisper.string_list("extra_args")? {
                config.whisper.extra_args = v
                    .into_iter()
                    .map(|arg| validate_arg("whisper.extra_args", arg))
                    .collect::<Result<_, _>>()?;
            }
            whisper.finish()?;
        }

        if let Some(mut chatgpt) = root.section("chatgpt")? {
            if let Some(v) = chatgpt.string("auth_file")? {
                config.chatgpt.auth_file =
                    paths::expand_absolute(&v).map_err(|e| format!("chatgpt.auth_file: {e}"))?;
            }
            if let Some(v) = chatgpt.integer("timeout_secs", 1, 3_600)? {
                config.chatgpt.timeout_secs = v;
            }
            chatgpt.finish()?;
        }

        root.finish()?;
        Ok(config)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "backend": self.backend.as_str(),
            "language": self.language,
            "sounds": self.sounds,
            "idle_pill": self.idle_pill,
            "notifications": self.notifications.as_str(),
            "hotkey": self.hotkey.as_str(),
            "shortcuts": {
                "hold": self.shortcuts.hold,
                "hands_free": self.shortcuts.hands_free,
                "cancel": self.shortcuts.cancel,
            },
            "paste": self.paste.as_str(),
            "paste_shortcut": self.paste_shortcut.as_str(),
            "restore_clipboard": self.restore_clipboard,
            "min_hold_ms": self.min_hold_ms,
            "max_recording_secs": self.max_recording_secs,
            "silence_threshold": self.silence_threshold,
            "audio": {
                "recorder": self.audio.recorder.as_str(),
                "device": self.audio.device,
            },
            "whisper": {
                "binary": self.whisper.binary,
                "model": self.whisper.model.to_string_lossy(),
                "threads": self.whisper.threads,
                "timeout_secs": self.whisper.timeout_secs,
                "extra_args": self.whisper.extra_args,
            },
            "chatgpt": {
                "auth_file": self.chatgpt.auth_file.to_string_lossy(),
                "timeout_secs": self.chatgpt.timeout_secs,
            },
        })
    }
}

/// Settings given on the command line, layered over the config file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    pub backend: Option<Backend>,
    pub language: Option<String>,
    pub model: Option<String>,
    pub whisper_bin: Option<String>,
    pub threads: Option<u32>,
    pub hotkey: Option<HotkeyMode>,
    pub paste: Option<PasteMode>,
}

impl Overrides {
    /// Forgets what the command line said about anything `patch` sets. Once the
    /// file has been changed on purpose it has the final say, so a later reload
    /// must not bring the startup value back.
    pub fn forget(&mut self, patch: &Value) {
        let has = |key: &str| patch.get(key).is_some();
        let has_whisper = |key: &str| patch.get("whisper").and_then(|w| w.get(key)).is_some();
        if has("backend") {
            self.backend = None;
        }
        if has("language") {
            self.language = None;
        }
        if has_whisper("model") {
            self.model = None;
        }
        if has_whisper("binary") {
            self.whisper_bin = None;
        }
        if has_whisper("threads") {
            self.threads = None;
        }
        if has("hotkey") {
            self.hotkey = None;
        }
        if has("paste") {
            self.paste = None;
        }
    }

    pub fn apply(&self, config: &mut Config) -> Result<(), String> {
        if let Some(v) = self.backend {
            config.backend = v;
        }
        if let Some(v) = &self.language {
            config.language = validate_language(v)?;
        }
        if let Some(v) = &self.model {
            config.whisper.model =
                paths::expand_absolute(v).map_err(|e| format!("--model: {e}"))?;
        }
        if let Some(v) = &self.whisper_bin {
            config.whisper.binary = validate_binary(v)?;
        }
        if let Some(v) = self.threads {
            config.whisper.threads = v;
        }
        if let Some(v) = self.hotkey {
            config.hotkey = v;
        }
        if let Some(v) = self.paste {
            config.paste = v;
        }
        Ok(())
    }
}

/// Changes only `backend` in the file, keeping every other key the user set.
pub fn persist_backend(path: &Path, backend: Backend) -> Result<(), String> {
    persist_patch(path, &json!({ "backend": backend.as_str() }))
}

/// Merges `patch` into the file, keeping every other key the user set. Nothing is
/// written unless the existing file is valid and the result would load too.
pub fn persist_patch(path: &Path, patch: &Value) -> Result<(), String> {
    let mut root = match fs::read_to_string(path) {
        Ok(text) => {
            Config::parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
            serde_json::from_str::<Value>(&text).map_err(|e| e.to_string())?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    merge(&mut root, patch);
    Config::parse(&root.to_string())?;
    write_json(path, &root)
}

fn merge(target: &mut Value, patch: &Value) {
    match (target.as_object_mut(), patch.as_object()) {
        (Some(target), Some(patch)) => {
            for (key, value) in patch {
                merge(target.entry(key.as_str()).or_insert(Value::Null), value);
            }
        }
        _ => *target = patch.clone(),
    }
}

/// Writes the defaults, never replacing an existing file.
pub fn init_file(path: &Path) -> Result<(), String> {
    if path.exists() {
        return Err(format!("{} already exists; left unchanged", path.display()));
    }
    write_json(path, &Config::defaults()?.to_json())
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    paths::atomic_write(path, text.as_bytes())
}

fn validate_language(value: &str) -> Result<String, String> {
    let value = value.trim();
    let shape_ok = value.len() <= 16
        && value
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '-' || c == '_');
    if shape_ok {
        Ok(value.to_string())
    } else {
        Err(format!(
            "language `{value}` must be an ISO code such as `en`, or `auto`"
        ))
    }
}

fn validate_binary(value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err("whisper binary must not be empty".to_string());
    }
    if value.contains('\0') {
        return Err("whisper binary must not contain NUL".to_string());
    }
    if value.contains('/') || (cfg!(windows) && value.contains('\\')) {
        let path = paths::expand_absolute(value).map_err(|e| format!("whisper binary: {e}"))?;
        return Ok(path.to_string_lossy().into_owned());
    }
    Ok(value.to_string())
}

/// XDG shortcut syntax such as `CTRL+LOGO+d`. The desktop does the real parsing;
/// this catches typos early and keeps the value printable.
fn validate_trigger(name: &str, value: String) -> Result<String, String> {
    let shape_ok = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'_')
        && !value.starts_with('+')
        && !value.ends_with('+');
    if shape_ok {
        Ok(value)
    } else {
        Err(format!(
            "`{name}` must look like CTRL+LOGO+d; got `{value}`"
        ))
    }
}

fn validate_arg(name: &str, value: String) -> Result<String, String> {
    if value.contains('\0') {
        Err(format!("{name} must not contain NUL"))
    } else if value.is_empty() {
        Err(format!("{name} must not be empty"))
    } else {
        Ok(value)
    }
}

/// Takes typed values out of a JSON object, reporting anything left over.
struct Fields {
    map: Map<String, Value>,
    prefix: String,
}

impl Fields {
    fn new(value: Value, prefix: &str) -> Result<Self, String> {
        match value {
            Value::Object(map) => Ok(Fields {
                map,
                prefix: prefix.to_string(),
            }),
            _ if prefix.is_empty() => Err("the top level must be a JSON object".to_string()),
            _ => Err(format!(
                "`{}` must be a JSON object",
                prefix.trim_end_matches('.')
            )),
        }
    }

    fn name(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    fn string(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("`{}` must be a string", self.name(key))),
        }
    }

    fn nullable_string(&mut self, key: &str) -> Result<Option<Option<String>>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(s)) => Ok(Some(Some(s))),
            Some(_) => Err(format!("`{}` must be a string or null", self.name(key))),
        }
    }

    fn string_list(&mut self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Array(items)) => items
                .into_iter()
                .map(|item| match item {
                    Value::String(s) => Ok(s),
                    _ => Err(format!("`{}` must contain only strings", self.name(key))),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(_) => Err(format!("`{}` must be an array of strings", self.name(key))),
        }
    }

    fn boolean(&mut self, key: &str) -> Result<Option<bool>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(b)),
            Some(_) => Err(format!("`{}` must be true or false", self.name(key))),
        }
    }

    fn integer(&mut self, key: &str, min: u64, max: u64) -> Result<Option<u64>, String> {
        let name = self.name(key);
        match self.map.remove(key) {
            None => Ok(None),
            Some(value) => match value.as_u64() {
                Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                _ => Err(format!("`{name}` must be an integer from {min} to {max}")),
            },
        }
    }

    fn number(&mut self, key: &str, min: f64, max: f64) -> Result<Option<f64>, String> {
        let name = self.name(key);
        match self.map.remove(key) {
            None => Ok(None),
            Some(value) => match value.as_f64() {
                Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                _ => Err(format!("`{name}` must be a number from {min} to {max}")),
            },
        }
    }

    fn choice<T>(
        &mut self,
        key: &str,
        parse: fn(&str) -> Option<T>,
        choices: &str,
    ) -> Result<Option<T>, String> {
        let Some(text) = self.string(key)? else {
            return Ok(None);
        };
        parse(&text).map(Some).ok_or_else(|| {
            format!(
                "`{}` must be one of: {}; got `{text}`",
                self.name(key),
                choices.trim()
            )
        })
    }

    fn section(&mut self, key: &str) -> Result<Option<Fields>, String> {
        match self.map.remove(key) {
            None => Ok(None),
            Some(value) => Fields::new(value, &format!("{}{key}.", self.prefix)).map(Some),
        }
    }

    fn finish(self) -> Result<(), String> {
        match self.map.keys().next() {
            None => Ok(()),
            Some(key) => Err(format!("unknown setting `{}{key}`", self.prefix)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn empty_object_gives_private_local_defaults() {
        let config = Config::parse("{}").unwrap();
        assert_eq!(config.backend, Backend::Local);
        assert_eq!(config.language, "");
        assert_eq!(config.min_hold_ms, 300);
        assert_eq!(config.max_recording_secs, 600);
        assert_eq!(config.silence_threshold, 0.12);
        assert_eq!(config.whisper.binary, "whisper-cli");
        assert!(config.whisper.model.is_absolute());
    }

    #[test]
    fn full_file_round_trips_through_to_json() {
        let text = r#"{
            "backend": "chatgpt",
            "language": "de",
            "sounds": false,
            "idle_pill": false,
            "notifications": "errors",
            "hotkey": "portal",
            "shortcuts": {
                "hold": "CTRL+ALT+d",
                "hands_free": "CTRL+ALT+space",
                "cancel": "CTRL+ALT+c"
            },
            "paste": "copy",
            "paste_shortcut": "ctrl+shift+v",
            "restore_clipboard": false,
            "min_hold_ms": 100,
            "max_recording_secs": 60,
            "silence_threshold": 0.25,
            "audio": { "recorder": "parec", "device": "mic-1" },
            "whisper": {
                "binary": "/opt/whisper/whisper-cli",
                "model": "/opt/whisper/ggml-small.bin",
                "threads": 6,
                "timeout_secs": 30,
                "extra_args": ["-ng"]
            },
            "chatgpt": { "auth_file": "/fixture/auth.json", "timeout_secs": 10 }
        }"#;
        let config = Config::parse(text).unwrap();
        assert_eq!(config.backend, Backend::ChatGpt);
        assert_eq!(config.audio.device.as_deref(), Some("mic-1"));
        assert_eq!(config.whisper.extra_args, ["-ng"]);
        assert_eq!(config.paste_shortcut, PasteShortcut::CtrlShiftV);
        assert_eq!(config.hotkey, HotkeyMode::Portal);
        assert_eq!(config.shortcuts.hold, "CTRL+ALT+d");

        let again = Config::parse(&config.to_json().to_string()).unwrap();
        assert_eq!(again, config);
    }

    #[test]
    fn invalid_values_are_rejected_with_the_setting_name() {
        for (text, expected) in [
            (r#"{"backnd":"local"}"#, "unknown setting `backnd`"),
            (r#"{"backend":"openai"}"#, "`backend` must be one of"),
            (r#"{"sounds":"yes"}"#, "`sounds` must be true or false"),
            (r#"{"max_recording_secs":0}"#, "`max_recording_secs`"),
            (r#"{"silence_threshold":2}"#, "`silence_threshold`"),
            (r#"{"language":"en;rm"}"#, "language"),
            (r#"{"hotkey":"evdev"}"#, "`hotkey` must be one of"),
            (r#"{"shortcuts":{"hold":""}}"#, "shortcuts.hold"),
            (r#"{"shortcuts":{"hold":"CTRL+"}}"#, "shortcuts.hold"),
            (r#"{"shortcuts":{"cancel":"a b"}}"#, "shortcuts.cancel"),
            (
                r#"{"shortcuts":{"lock":"a"}}"#,
                "unknown setting `shortcuts.lock`",
            ),
            (
                r#"{"whisper":{"threds":1}}"#,
                "unknown setting `whisper.threds`",
            ),
            (r#"{"whisper":{"threads":-1}}"#, "`whisper.threads`"),
            (r#"{"whisper":{"model":"relative.bin"}}"#, "whisper.model"),
            (
                r#"{"whisper":{"binary":"./bin/whisper"}}"#,
                "whisper binary",
            ),
            (r#"{"whisper":{"extra_args":[1]}}"#, "whisper.extra_args"),
            (r#"{"whisper":"x"}"#, "`whisper` must be a JSON object"),
            (r#"{"audio":{"device":""}}"#, "audio.device"),
            (r#"{"audio":{"recorder":"sox"}}"#, "audio.recorder"),
            (r#"{"chatgpt":{"auth_file":"a.json"}}"#, "chatgpt.auth_file"),
            (r#"[]"#, "top level"),
            (r#"not json"#, "invalid JSON"),
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

    #[test]
    fn missing_file_means_defaults_but_a_broken_file_is_an_error() {
        let temp = TempDir::new();
        let path = temp.path().join("config.json");
        assert_eq!(Config::load(&path).unwrap(), Config::defaults().unwrap());
        fs::write(&path, "{").unwrap();
        assert!(Config::load(&path).unwrap_err().contains("invalid JSON"));
    }

    #[test]
    fn overrides_win_and_are_validated() {
        let mut config = Config::parse("{}").unwrap();
        let overrides = Overrides {
            backend: Some(Backend::ChatGpt),
            language: Some("fr".into()),
            model: Some("/m/ggml.bin".into()),
            whisper_bin: Some("/w/whisper-cli".into()),
            threads: Some(3),
            hotkey: Some(HotkeyMode::None),
            paste: Some(PasteMode::Copy),
        };
        overrides.apply(&mut config).unwrap();
        assert_eq!(config.backend, Backend::ChatGpt);
        assert_eq!(config.language, "fr");
        assert_eq!(config.whisper.model, PathBuf::from("/m/ggml.bin"));
        assert_eq!(config.whisper.binary, "/w/whisper-cli");
        assert_eq!(config.whisper.threads, 3);

        let bad = Overrides {
            model: Some("relative.bin".into()),
            ..Overrides::default()
        };
        assert!(bad.apply(&mut config).is_err());
    }

    #[test]
    fn persisting_the_backend_keeps_other_settings() {
        let temp = TempDir::new();
        let path = temp.path().join("wispx").join("config.json");
        persist_backend(&path, Backend::ChatGpt).unwrap();
        assert_eq!(Config::load(&path).unwrap().backend, Backend::ChatGpt);

        fs::write(&path, r#"{"backend":"chatgpt","language":"de"}"#).unwrap();
        persist_backend(&path, Backend::Local).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.backend, Backend::Local);
        assert_eq!(config.language, "de");

        fs::write(&path, "{broken").unwrap();
        assert!(persist_backend(&path, Backend::Local).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn a_patch_merges_nested_sections_and_is_validated_before_writing() {
        let temp = TempDir::new();
        let path = temp.path().join("config.json");
        fs::write(
            &path,
            r#"{"language":"de","whisper":{"threads":2},"audio":{"device":"mic-1"}}"#,
        )
        .unwrap();
        persist_patch(
            &path,
            &json!({"whisper":{"model":"/m/ggml.bin"},"audio":{"device":null},"idle_pill":false}),
        )
        .unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.language, "de");
        assert_eq!(config.whisper.threads, 2);
        assert_eq!(config.whisper.model, PathBuf::from("/m/ggml.bin"));
        assert_eq!(config.audio.device, None);
        assert!(!config.idle_pill);

        let before = fs::read_to_string(&path).unwrap();
        for bad in [
            json!({"backnd":"local"}),
            json!({"whisper":{"model":"relative.bin"}}),
            json!({"whisper":"x"}),
        ] {
            assert!(persist_patch(&path, &bad).is_err(), "{bad}");
            assert_eq!(fs::read_to_string(&path).unwrap(), before);
        }
        fs::write(&path, "{broken").unwrap();
        assert!(persist_patch(&path, &json!({"sounds":false})).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn a_patch_forgets_only_the_command_line_values_it_sets() {
        let mut overrides = Overrides {
            backend: Some(Backend::ChatGpt),
            language: Some("fr".into()),
            model: Some("/m.bin".into()),
            paste: Some(PasteMode::Copy),
            ..Overrides::default()
        };
        overrides.forget(&json!({"whisper":{"model":"/n.bin"},"language":"en"}));
        assert_eq!(overrides.backend, Some(Backend::ChatGpt));
        assert_eq!(overrides.paste, Some(PasteMode::Copy));
        assert_eq!(overrides.language, None);
        assert_eq!(overrides.model, None);
    }

    #[test]
    fn init_never_overwrites() {
        let temp = TempDir::new();
        let path = temp.path().join("config.json");
        init_file(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), Config::defaults().unwrap());
        fs::write(&path, "{}").unwrap();
        assert!(init_file(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }
}
