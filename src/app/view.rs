//! Turns what the app knows into what its windows show. Pure functions, so every
//! state a person can run into can be checked without a display.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const LANGUAGES: [(&str, &str); 14] = [
    ("", "Detect automatically"),
    ("en", "English"),
    ("es", "Spanish"),
    ("fr", "French"),
    ("de", "German"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("zh", "Chinese"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
];

#[derive(Clone, Debug, PartialEq)]
pub enum Service {
    Connecting,
    /// `owned` means this app started it, so it also stops when the app quits.
    Running {
        owned: bool,
    },
    Stopped,
    Failed(String),
}

pub struct DownloadView {
    pub label: String,
    pub bytes: u64,
    pub done: u64,
}

pub struct Model {
    pub service: Service,
    /// The latest status from the daemon, which is the authority while it runs.
    pub status: Option<Value>,
    pub checks: Option<Vec<Value>>,
    /// The file named with `--config`, if any.
    pub requested_config: Option<PathBuf>,
    pub autostart: bool,
    pub tray_available: bool,
    pub overlay_available: bool,
    pub paste_busy: bool,
    /// What went wrong with the last thing asked for in this window.
    pub problem: Option<String>,
    pub download: Option<DownloadView>,
    pub models: Vec<PathBuf>,
    pub devices: Vec<(String, String)>,
}

impl Model {
    pub fn new(requested_config: Option<PathBuf>) -> Model {
        Model {
            service: Service::Connecting,
            status: None,
            checks: None,
            requested_config,
            autostart: false,
            tray_available: true,
            overlay_available: true,
            paste_busy: false,
            problem: None,
            download: None,
            models: Vec::new(),
            devices: Vec::new(),
        }
    }

    fn text(&self, key: &str) -> &str {
        self.status
            .as_ref()
            .and_then(|status| status[key].as_str())
            .unwrap_or_default()
    }

    fn setting(&self, key: &str) -> &Value {
        self.status
            .as_ref()
            .map_or(&Value::Null, |status| &status["settings"][key])
    }

    fn check(&self, name: &str) -> Option<&Value> {
        self.checks
            .as_ref()?
            .iter()
            .find(|check| check["name"] == name)
    }

    fn trigger(&self, id: &str) -> Option<&str> {
        self.status.as_ref()?["triggers"][id]
            .as_str()
            .filter(|keys| *keys != "not assigned")
    }

    /// The key that really starts a dictation, if one is bound.
    pub fn hold_keys(&self) -> Option<&str> {
        self.trigger("dictate")
    }

    fn running(&self) -> bool {
        matches!(self.service, Service::Running { .. }) && self.status.is_some()
    }

    fn is_plasma(&self) -> bool {
        self.text("session").contains("KDE Plasma")
    }

    fn model_path(&self) -> Option<&Path> {
        self.setting("model").as_str().map(Path::new)
    }

    fn model_missing(&self) -> bool {
        self.model_path()
            .is_some_and(|path| !path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0))
    }

    /// The daemon is waiting on the desktop, which may be showing a dialog.
    pub fn asking_about_shortcuts(&self) -> bool {
        self.running() && self.text("hotkey_state") == "starting"
    }

    /// The daemon in charge has a different file than the one asked for.
    pub fn config_mismatch(&self) -> Option<String> {
        let requested = self.requested_config.as_ref()?;
        let in_use = self.text("config");
        if in_use.is_empty() || crate::paths::same_file(Path::new(in_use), requested) {
            return None;
        }
        Some(format!(
            "The background service already running uses {in_use}, not {}. It stays in charge. \
             Stop it below to use the other file.",
            requested.display()
        ))
    }
}

fn row(
    id: &str,
    title: &str,
    level: &str,
    detail: impl Into<String>,
    buttons: Vec<Value>,
) -> Value {
    json!({
        "id": id,
        "title": title,
        "level": level,
        "detail": detail.into(),
        "buttons": buttons,
    })
}

fn button(action: &str, label: &str, primary: bool) -> Value {
    json!({ "action": action, "label": label, "primary": primary, "enabled": true, "args": {} })
}

fn speech_row(model: &Model) -> Value {
    let (id, title) = ("speech", "Speech recognition");
    if !model.running() {
        return row(
            id,
            title,
            "wait",
            "Waiting for the background service",
            vec![],
        );
    }
    let chatgpt = model.text("backend") == "chatgpt";
    if !chatgpt && model.model_missing() {
        if model.download.is_some() {
            return row(id, title, "busy", "Downloading a speech model.", vec![]);
        }
        return row(
            id,
            title,
            "fail",
            "There is no speech model yet. Download one, or choose one you already have.",
            vec![
                json!({
                    "action": "download-model",
                    "label": "Download the base model (148 MB)",
                    "primary": true,
                    "enabled": true,
                    "args": { "id": "base" },
                }),
                button("choose-model", "Choose a model file…", false),
            ],
        );
    }
    let Some(check) = model.check("backend") else {
        return row(id, title, "wait", "Checking…", vec![]);
    };
    let mut level = match check["level"].as_str() {
        Some("fail") => "fail",
        Some("warn") => "warn",
        _ => "ok",
    };
    let mut detail = check["detail"].as_str().unwrap_or_default().to_string();
    if level == "ok" && !chatgpt {
        let name = model
            .model_path()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        detail =
            format!("whisper.cpp on this computer, using {name}. Nothing leaves your machine.");
    }
    if chatgpt {
        detail.push_str(". Your audio is sent to chatgpt.com, through a private endpoint that can change without notice.");
    }
    if let Some(language) = model.check("language") {
        if level == "ok" {
            level = "warn";
        }
        detail.push(' ');
        detail.push_str(language["detail"].as_str().unwrap_or_default());
    }
    row(id, title, level, detail, vec![])
}

fn microphone_row(model: &Model) -> Value {
    let title = "Microphone";
    match model.check("recorder") {
        None => row("microphone", title, "wait", "Checking…", vec![]),
        Some(check) if check["level"] == "ok" => row(
            "microphone",
            title,
            "ok",
            "The microphone is only open while you dictate.",
            vec![],
        ),
        Some(check) => row(
            "microphone",
            title,
            "fail",
            check["detail"].as_str().unwrap_or_default(),
            vec![],
        ),
    }
}

fn shortcuts_row(model: &Model) -> Value {
    let title = "Shortcuts";
    if !model.running() {
        return row(
            "shortcuts",
            title,
            "wait",
            "Waiting for the background service",
            vec![],
        );
    }
    let settings_button = if model.is_plasma() {
        vec![button("open-shortcuts", "Change in System Settings", false)]
    } else {
        Vec::new()
    };
    match model.text("hotkey_state") {
        "bound" => {
            let keys = |id: &str| model.trigger(id).unwrap_or("not assigned");
            let detail = format!(
                "Hold {} to dictate. {} for hands-free. {} to cancel.",
                keys("dictate"),
                keys("hands-free"),
                keys("cancel")
            );
            let complete = model.trigger("dictate").is_some();
            let level = if complete { "ok" } else { "warn" };
            row("shortcuts", title, level, detail, settings_button)
        }
        "starting" => row(
            "shortcuts",
            title,
            "busy",
            "Waiting for you to confirm them in the desktop's dialog.",
            vec![],
        ),
        "declined" => row(
            "shortcuts",
            title,
            "warn",
            "They were declined, so no key starts a dictation. Assign them in System Settings under Keyboard, Shortcuts, WispX.",
            settings_button,
        ),
        "disconnected" => row(
            "shortcuts",
            title,
            "warn",
            format!("{}. Trying again…", model.text("hotkey")),
            settings_button,
        ),
        _ => row("shortcuts", title, "warn", model.text("hotkey"), vec![]),
    }
}

fn paste_row(model: &Model) -> Value {
    let title = "Automatic paste";
    if !model.running() {
        return row(
            "paste",
            title,
            "wait",
            "Waiting for the background service",
            vec![],
        );
    }
    match model.text("paste_state") {
        "automatic" => row(
            "paste",
            title,
            "ok",
            "Text goes straight into the window you were using. If you switch windows first, it waits on the clipboard.",
            vec![],
        ),
        "needs-permission" if model.paste_busy => row(
            "paste",
            title,
            "busy",
            "Waiting for your answer in the desktop's dialog…",
            vec![],
        ),
        "needs-permission" => {
            let blocked = model
                .check("automatic paste")
                .and_then(|check| check["detail"].as_str())
                .filter(|detail| !detail.starts_with("not allowed yet"));
            match blocked {
                Some(detail) => row(
                    "paste",
                    title,
                    "warn",
                    format!("{detail}. Text is copied for you to paste with Ctrl+V."),
                    vec![],
                ),
                None => row(
                    "paste",
                    title,
                    "warn",
                    "Not allowed yet, so text is copied for you to paste with Ctrl+V. The desktop asks once, and then remembers.",
                    vec![button("enable-paste", "Enable automatic paste", true)],
                ),
            }
        }
        "needs-plasma" => row(
            "paste",
            title,
            "warn",
            "This needs KDE Plasma, where WispX can check which window has focus first. Text is copied for you to paste with Ctrl+V.",
            vec![],
        ),
        "copy" => row(
            "paste",
            title,
            "ok",
            "Off in the settings file: text is copied for you to paste.",
            vec![],
        ),
        _ => row(
            "paste",
            title,
            "ok",
            "Off in the settings file: text is only kept for Copy last transcript.",
            vec![],
        ),
    }
}

fn clipboard_row(model: &Model) -> Option<Value> {
    let check = model.check("clipboard")?;
    (check["level"] != "ok").then(|| {
        row(
            "clipboard",
            "Clipboard",
            "fail",
            check["detail"].as_str().unwrap_or_default(),
            vec![],
        )
    })
}

fn service_row(model: &Model) -> Value {
    let title = "Background service";
    if let Some(mismatch) = model.config_mismatch() {
        return row(
            "service",
            title,
            "warn",
            mismatch,
            vec![button("stop-service", "Stop it", false)],
        );
    }
    match &model.service {
        Service::Connecting => row("service", title, "busy", "Starting…", vec![]),
        Service::Running { owned: true } => row(
            "service",
            title,
            "ok",
            "Running. It stops when you quit WispX.",
            vec![],
        ),
        Service::Running { owned: false } => row(
            "service",
            title,
            "ok",
            "Running separately, started with `wispx daemon`, so it keeps running when you quit WispX.",
            vec![button("stop-service", "Stop it", false)],
        ),
        Service::Stopped => row(
            "service",
            title,
            "fail",
            "Not running, so shortcuts do nothing.",
            vec![button("start-service", "Start it", true)],
        ),
        Service::Failed(error) => row(
            "service",
            title,
            "fail",
            error.clone(),
            vec![
                button("start-service", "Try again", true),
                button("open-config", "Open settings file", false),
            ],
        ),
    }
}

pub fn rows(model: &Model) -> Vec<Value> {
    let mut rows = vec![
        speech_row(model),
        microphone_row(model),
        shortcuts_row(model),
        paste_row(model),
    ];
    rows.extend(clipboard_row(model));
    rows.push(service_row(model));
    rows
}

/// Whether anything needs a person's attention, once the checks are in.
pub fn needs_attention(model: &Model) -> bool {
    rows(model)
        .iter()
        .any(|row| matches!(row["level"].as_str(), Some("fail" | "warn" | "busy")))
}

/// Whether something stops dictation working at all.
pub fn is_broken(model: &Model) -> bool {
    rows(model).iter().any(|row| row["level"] == "fail")
}

fn summary(model: &Model, rows: &[Value]) -> Value {
    let has = |level: &str| rows.iter().any(|row| row["level"] == level);
    let (level, text) = if has("fail") {
        (
            "fail",
            "WispX needs something before it can dictate.".to_string(),
        )
    } else if has("wait") || has("busy") {
        ("busy", "Getting ready…".to_string())
    } else if let Some(keys) = model.hold_keys() {
        if has("warn") {
            (
                "warn",
                format!("Ready. Hold {keys} to dictate. One thing could be better below."),
            )
        } else {
            ("ok", format!("Ready. Hold {keys} to dictate."))
        }
    } else {
        ("warn", "No key starts a dictation yet.".to_string())
    };
    json!({ "level": level, "text": text })
}

/// What the settings window shows.
pub fn settings(model: &Model) -> Value {
    let rows = rows(model);
    let summary = summary(model, &rows);
    let current_model = model.model_path().map(Path::to_path_buf);
    let mut models: Vec<PathBuf> = model.models.clone();
    if let Some(current) = &current_model
        && !models.contains(current)
    {
        models.insert(0, current.clone());
    }
    let label = |path: &PathBuf| {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        if path.is_file() {
            name
        } else {
            format!("{name} (missing)")
        }
    };
    let models: Vec<Value> = models
        .iter()
        .map(|path| json!({ "label": label(path), "path": path.to_string_lossy() }))
        .collect();
    let downloads: Vec<Value> = super::models::CATALOG
        .iter()
        .filter(|choice| {
            !model
                .models
                .iter()
                .any(|path| path.file_name().is_some_and(|name| name == choice.file))
        })
        .map(|choice| json!({ "id": choice.id, "label": choice.label }))
        .collect();
    let download = model.download.as_ref().map(|download| {
        let percent = download
            .done
            .saturating_mul(100)
            .checked_div(download.bytes)
            .unwrap_or(0)
            .min(100);
        json!({ "label": download.label, "percent": percent })
    });
    let languages: Vec<Value> = LANGUAGES
        .iter()
        .map(|(code, label)| json!({ "code": code, "label": label }))
        .collect();
    let mut devices = vec![json!({ "id": "", "label": "System default" })];
    let device = model.setting("device").as_str().unwrap_or_default();
    let mut device_listed = device.is_empty();
    for (id, label) in &model.devices {
        device_listed |= id == device;
        devices.push(json!({ "id": id, "label": label }));
    }
    if !device_listed {
        devices.push(json!({ "id": device, "label": device }));
    }
    let keys = |id: &str| model.trigger(id).unwrap_or("not assigned").to_string();
    json!({
        "summary": summary,
        "rows": rows,
        "ready": model.running(),
        "backend": model.text("backend"),
        "language": model.setting("language").as_str().unwrap_or_default(),
        "model": current_model.map(|path| path.to_string_lossy().into_owned()).unwrap_or_default(),
        "device": device,
        "sounds": model.setting("sounds").as_bool().unwrap_or(true),
        "idle_pill": model.setting("idle_pill").as_bool().unwrap_or(true),
        "autostart": model.autostart,
        "has_transcript": model.status.as_ref().is_some_and(|s| s["has_transcript"] == true),
        "models": models,
        "downloads": downloads,
        "download": download,
        "languages": languages,
        "devices": devices,
        "shortcuts": [
            { "label": "Hold to dictate", "keys": keys("dictate") },
            { "label": "Hands-free: start, keep going, or finish", "keys": keys("hands-free") },
            { "label": "Cancel", "keys": keys("cancel") },
        ],
        "problem": model.problem,
        "last_error": model.status.as_ref().and_then(|s| s["last_error"].as_str()),
        "tray_available": model.tray_available,
        "overlay_available": model.overlay_available,
        "session": model.text("session"),
    })
}

fn status_line(model: &Model) -> String {
    match &model.service {
        Service::Connecting => "Starting…".to_string(),
        Service::Stopped => "Background service stopped".to_string(),
        Service::Failed(_) => "Background service could not start".to_string(),
        Service::Running { .. } => match model.text("state") {
            "recording" => "Recording".to_string(),
            "recording-hands-free" => "Recording hands-free".to_string(),
            "transcribing" => "Transcribing".to_string(),
            _ if is_broken(model) => "Needs setup. Open Settings".to_string(),
            _ if model.hold_keys().is_none() => "No shortcut assigned".to_string(),
            _ => "Ready".to_string(),
        },
    }
}

/// The tray menu, in the original's order.
pub fn tray(model: &Model) -> Value {
    let title = match model.hold_keys() {
        Some(keys) => format!("WispX · hold {keys}"),
        None => "WispX".to_string(),
    };
    let status = status_line(model);
    let info = |id: &str, label: &str| json!({ "id": id, "label": label, "enabled": false });
    let action = |id: &str, label: &str, enabled: bool| json!({ "id": id, "label": label, "enabled": enabled });
    let check = |id: &str, label: &str, checked: bool| json!({ "id": id, "label": label, "enabled": true, "checkable": true, "checked": checked });
    let separator = json!({ "separator": true });
    let has_transcript = model
        .status
        .as_ref()
        .is_some_and(|s| s["has_transcript"] == true);
    json!({
        "tooltip": format!("{title}\n{status}"),
        "items": [
            info("title", &title),
            info("status", &status),
            separator,
            action("copy-last", "Copy last transcript", has_transcript),
            separator,
            check("set-sounds", "Sounds", model.setting("sounds").as_bool().unwrap_or(true)),
            check("set-idle-pill", "Show idle pill", model.setting("idle_pill").as_bool().unwrap_or(true)),
            check("set-autostart", "Start at login", model.autostart),
            separator,
            action("show-settings", "Settings…", true),
            action("how-to-use", "How to use", true),
            separator,
            action("quit", "Quit", true),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn status(paste_state: &str, hotkey_state: &str, model: &Path) -> Value {
        json!({
            "state": "idle",
            "backend": "local",
            "config": "/c/config.json",
            "session": "KDE Plasma on Wayland",
            "hotkey": "portal shortcuts: dictate: Meta+Ctrl+D",
            "hotkey_state": hotkey_state,
            "triggers": if hotkey_state == "bound" {
                json!({"dictate": "Meta+Ctrl+D", "hands-free": "Meta+Ctrl+Space", "cancel": "Meta+Ctrl+Backspace"})
            } else {
                json!({})
            },
            "paste_state": paste_state,
            "has_transcript": false,
            "last_error": null,
            "settings": {
                "language": "",
                "model": model.to_string_lossy(),
                "sounds": true,
                "idle_pill": true,
                "device": null,
            },
        })
    }

    fn checks(level: &str) -> Vec<Value> {
        vec![
            json!({"name": "backend", "level": level, "detail": "local whisper.cpp"}),
            json!({"name": "recorder", "level": "ok", "detail": "pw-record"}),
            json!({"name": "clipboard", "level": "ok", "detail": "wl-copy"}),
            json!({"name": "automatic paste", "level": "warn", "detail": "not allowed yet. Use Enable"}),
        ]
    }

    fn ready(temp: &TempDir, paste_state: &str, hotkey_state: &str) -> Model {
        let path = temp.path().join("ggml-base.bin");
        std::fs::write(&path, b"model").unwrap();
        let mut model = Model::new(None);
        model.service = Service::Running { owned: true };
        model.status = Some(status(paste_state, hotkey_state, &path));
        model.checks = Some(checks("ok"));
        model
    }

    fn find<'a>(rows: &'a [Value], id: &str) -> &'a Value {
        rows.iter().find(|row| row["id"] == id).unwrap()
    }

    fn actions(row: &Value) -> Vec<&str> {
        row["buttons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|button| button["action"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn everything_set_up_shows_the_actual_key_and_asks_for_nothing() {
        let temp = TempDir::new();
        let model = ready(&temp, "automatic", "bound");
        let state = settings(&model);
        assert_eq!(state["summary"]["level"], "ok");
        assert_eq!(
            state["summary"]["text"],
            "Ready. Hold Meta+Ctrl+D to dictate."
        );
        assert!(!needs_attention(&model));
        assert_eq!(state["shortcuts"][0]["keys"], "Meta+Ctrl+D");
    }

    #[test]
    fn missing_paste_permission_offers_the_button_without_blocking_dictation() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "needs-permission", "bound");
        let rows = rows(&model);
        assert_eq!(actions(find(&rows, "paste")), ["enable-paste"]);
        assert!(needs_attention(&model));
        assert!(!is_broken(&model));
        assert_eq!(settings(&model)["summary"]["level"], "warn");

        model.paste_busy = true;
        let busy = super::rows(&model);
        assert!(actions(find(&busy, "paste")).is_empty());
        assert_eq!(find(&busy, "paste")["level"], "busy");
    }

    #[test]
    fn a_blocked_portal_is_explained_rather_than_offered() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "needs-permission", "bound");
        model.checks = Some(vec![
            json!({"name": "automatic paste", "level": "warn", "detail": "the RemoteDesktop portal is missing"}),
        ]);
        let rows = rows(&model);
        let paste = find(&rows, "paste");
        assert!(actions(paste).is_empty());
        assert!(
            paste["detail"]
                .as_str()
                .unwrap()
                .contains("portal is missing")
        );
    }

    #[test]
    fn a_missing_model_offers_a_download_and_a_file_choice() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        model.status = Some(status("automatic", "bound", &temp.path().join("gone.bin")));
        model.checks = Some(checks("fail"));
        let rows = rows(&model);
        assert_eq!(find(&rows, "speech")["level"], "fail");
        assert_eq!(
            actions(find(&rows, "speech")),
            ["download-model", "choose-model"]
        );
        assert!(is_broken(&model));

        // Downloading already: nothing to start twice.
        model.download = Some(DownloadView {
            label: "x".into(),
            bytes: 100,
            done: 42,
        });
        assert!(actions(find(&super::rows(&model), "speech")).is_empty());
        assert_eq!(settings(&model)["download"]["percent"], 42);
    }

    #[test]
    fn an_existing_model_is_never_offered_for_download() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        model.models = vec![temp.path().join("ggml-base.bin")];
        let state = settings(&model);
        let offered: Vec<&str> = state["downloads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|choice| choice["id"].as_str().unwrap())
            .collect();
        assert!(!offered.contains(&"base"), "{offered:?}");
        assert!(offered.contains(&"small"));
    }

    #[test]
    fn declined_shortcuts_point_at_system_settings_and_show_no_invented_keys() {
        let temp = TempDir::new();
        let model = ready(&temp, "automatic", "declined");
        let rows = rows(&model);
        assert_eq!(find(&rows, "shortcuts")["level"], "warn");
        assert_eq!(actions(find(&rows, "shortcuts")), ["open-shortcuts"]);
        assert_eq!(settings(&model)["shortcuts"][0]["keys"], "not assigned");
        assert_eq!(
            settings(&model)["summary"]["text"],
            "No key starts a dictation yet."
        );
        assert_eq!(tray(&model)["items"][0]["label"], "WispX");
    }

    #[test]
    fn who_started_the_service_decides_what_is_offered() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        assert!(actions(find(&rows(&model), "service")).is_empty());

        model.service = Service::Running { owned: false };
        let attached = rows(&model);
        assert_eq!(actions(find(&attached, "service")), ["stop-service"]);
        assert!(
            find(&attached, "service")["detail"]
                .as_str()
                .unwrap()
                .contains("keeps running")
        );

        model.service = Service::Stopped;
        model.status = None;
        assert_eq!(actions(find(&rows(&model), "service")), ["start-service"]);

        model.service = Service::Failed("config.json: unknown setting `x`".into());
        let failed = rows(&model);
        assert_eq!(
            actions(find(&failed, "service")),
            ["start-service", "open-config"]
        );
        assert!(
            find(&failed, "service")["detail"]
                .as_str()
                .unwrap()
                .contains("unknown setting")
        );
        assert!(is_broken(&model));
    }

    #[test]
    fn a_daemon_using_another_config_stays_in_charge_and_says_so() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        model.requested_config = Some(PathBuf::from("/other/config.json"));
        let mismatch = model.config_mismatch().unwrap();
        assert!(mismatch.contains("/c/config.json"), "{mismatch}");
        assert_eq!(actions(find(&rows(&model), "service")), ["stop-service"]);

        model.requested_config = Some(PathBuf::from("/c/config.json"));
        assert!(model.config_mismatch().is_none());
    }

    #[test]
    fn chatgpt_says_where_the_audio_goes() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        model.status.as_mut().unwrap()["backend"] = json!("chatgpt");
        let rows = rows(&model);
        assert!(
            find(&rows, "speech")["detail"]
                .as_str()
                .unwrap()
                .contains("chatgpt.com")
        );
    }

    #[test]
    fn the_tray_menu_keeps_the_original_order_and_real_state() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        let menu = tray(&model);
        let ids: Vec<&str> = menu["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["id"].as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "title",
                "status",
                "copy-last",
                "set-sounds",
                "set-idle-pill",
                "set-autostart",
                "show-settings",
                "how-to-use",
                "quit",
            ]
        );
        assert_eq!(menu["items"][0]["label"], "WispX · hold Meta+Ctrl+D");
        assert_eq!(menu["items"][1]["label"], "Ready");
        assert_eq!(menu["items"][3]["enabled"], false, "no transcript yet");

        model.status.as_mut().unwrap()["state"] = json!("recording-hands-free");
        model.status.as_mut().unwrap()["has_transcript"] = json!(true);
        let menu = tray(&model);
        assert_eq!(menu["items"][1]["label"], "Recording hands-free");
        assert_eq!(menu["items"][3]["enabled"], true);
    }

    #[test]
    fn nothing_the_daemon_says_about_a_transcript_reaches_the_windows() {
        let temp = TempDir::new();
        let mut model = ready(&temp, "automatic", "bound");
        model.status.as_mut().unwrap()["transcript"] = json!("my private words");
        assert!(!settings(&model).to_string().contains("private words"));
        assert!(!tray(&model).to_string().contains("private words"));
    }
}
