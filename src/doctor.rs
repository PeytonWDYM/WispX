//! `wispx doctor`: read-only checks of everything WispX depends on. It never
//! opens the microphone, binds a shortcut, starts a portal session or loads a
//! KWin script, and only looks at the login when ChatGPT is the selected backend.

use crate::audio;
use crate::config::{Backend, Config, NotifyLevel, Overrides, PasteMode};
use crate::dbus::Connection;
use crate::desktop::clipboard::ToolClipboard;
use crate::desktop::{SessionKind, kwin, notify, remote};
use crate::ipc::{self, ClientError, Request};
use crate::paths;
use crate::portal;
use crate::process::Toolbox;
use crate::transcribe::{chatgpt, whisper};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const BUS_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

/// Prints each line as it is found, or collects them for the desktop interface.
#[derive(Default)]
struct Report {
    failed: bool,
    collected: Option<Vec<Value>>,
}

impl Report {
    fn line(&mut self, level: Level, name: &str, detail: impl AsRef<str>) {
        let tag = match level {
            Level::Ok => "[ok]  ",
            Level::Warn => "[warn]",
            Level::Fail => {
                self.failed = true;
                "[FAIL]"
            }
        };
        match &mut self.collected {
            Some(checks) => checks.push(json!({
                "name": name,
                "level": level.as_str(),
                "detail": detail.as_ref(),
            })),
            None => println!("{tag} {name}: {}", detail.as_ref()),
        }
    }

    fn result(&mut self, name: &str, result: Result<String, String>) {
        match result {
            Ok(detail) => self.line(Level::Ok, name, detail),
            Err(detail) => self.line(Level::Fail, name, detail),
        }
    }
}

pub fn run(config_path: &Path, overrides: &Overrides) -> i32 {
    let mut report = Report::default();
    let tools = Toolbox::from_env();

    let config = Config::load(config_path).and_then(|mut config| {
        overrides.apply(&mut config)?;
        Ok(config)
    });
    let config = match config {
        Ok(config) => {
            let origin = if config_path.exists() {
                config_path.display().to_string()
            } else {
                format!("defaults ({} does not exist)", config_path.display())
            };
            report.line(Level::Ok, "config", origin);
            config
        }
        Err(error) => {
            report.line(Level::Fail, "config", error);
            return 1;
        }
    };

    let desktop = SessionKind::detect();
    check_environment(&mut report, &config, &tools, &desktop);

    match ipc::send(&paths::client_socket_path(), Request::Status) {
        Ok(reply) => {
            let text = |key: &str| reply[key].as_str().unwrap_or("unknown").to_string();
            report.line(
                Level::Ok,
                "daemon",
                format!(
                    "running, {}, using the {} backend and {}",
                    text("state"),
                    text("backend"),
                    text("config")
                ),
            );
            report.line(Level::Ok, "shortcuts", text("hotkey"));
            report.line(Level::Ok, "delivery", text("paste"));
        }
        Err(ClientError::NotRunning) => {
            report.line(
                Level::Warn,
                "daemon",
                "not running; open WispX, or start it with `wispx daemon`",
            );
        }
        Err(ClientError::Failed(error)) => report.line(Level::Warn, "daemon", error),
    }

    i32::from(report.failed)
}

/// The same checks for the desktop interface, using the settings actually in use.
pub fn collect(config: &Config, tools: &Toolbox, desktop: &SessionKind) -> Vec<Value> {
    let mut report = Report {
        failed: false,
        collected: Some(Vec::new()),
    };
    check_environment(&mut report, config, tools, desktop);
    report.collected.unwrap_or_default()
}

fn check_environment(report: &mut Report, config: &Config, tools: &Toolbox, desktop: &SessionKind) {
    check_session(report, desktop);

    report.result(
        "recorder",
        audio::resolve(&config.audio, tools)
            .map(|plan| format!("{} (the microphone was not opened)", plan.program.display())),
    );

    check_backend(report, config, tools);

    match (config.paste, ToolClipboard::detect(desktop, tools)) {
        (PasteMode::None, _) => report.line(Level::Ok, "clipboard", "not used (paste is none)"),
        (_, Ok(clipboard)) => report.line(Level::Ok, "clipboard", clipboard.tool_name()),
        (_, Err(error)) => report.line(Level::Fail, "clipboard", error),
    }

    if config.sounds {
        match notify::player_name(tools) {
            Some(name) => report.line(Level::Ok, "sounds", name),
            None => report.line(
                Level::Warn,
                "sounds",
                "no player found (pw-play, paplay or aplay), so cues stay silent",
            ),
        }
    }

    check_bus_services(report, config, desktop);
    check_desktop_entry(report);
}

fn check_session(report: &mut Report, desktop: &SessionKind) {
    let name = "session";
    match desktop {
        SessionKind::Wayland { plasma: true, .. } => {
            report.line(Level::Ok, name, desktop.describe());
        }
        SessionKind::Wayland { .. } => report.line(
            Level::Warn,
            name,
            format!(
                "{}. Only KDE Plasma is fully supported: expect copy-only delivery",
                desktop.describe()
            ),
        ),
        SessionKind::X11 => report.line(
            Level::Warn,
            name,
            "X11 is secondary: bind `wispx press`/`wispx release` in your window manager; \
             delivery is copy-only",
        ),
        SessionKind::Unknown => report.line(Level::Warn, name, desktop.describe()),
    }
}

fn check_backend(report: &mut Report, config: &Config, tools: &Toolbox) {
    match config.backend {
        Backend::Local => {
            report.result(
                "backend",
                whisper::precheck(&config.whisper, tools).map(|()| {
                    format!(
                        "local whisper.cpp ({}, model {})",
                        config.whisper.binary,
                        config.whisper.model.display()
                    )
                }),
            );
            if let Some(warning) = language_warning(&config.whisper.model, &config.language) {
                report.line(Level::Warn, "language", warning);
            }
        }
        Backend::ChatGpt => {
            report.line(
                Level::Warn,
                "privacy",
                "the ChatGPT backend sends your audio to chatgpt.com, through a private, \
                 undocumented endpoint that can change without notice",
            );
            report.result(
                "backend",
                chatgpt::precheck(&config.chatgpt, tools)
                    .and_then(|()| chatgpt::login_summary(&config.chatgpt))
                    .map(|login| format!("ChatGPT. {login}")),
            );
        }
    }
}

/// `.en` models understand only English, and would mistranslate anything else.
fn language_warning(model: &Path, language: &str) -> Option<String> {
    let name = model.file_name()?.to_string_lossy();
    let english_only = name.ends_with(".en.bin") || name.contains(".en-");
    let other = !language.is_empty() && !language.eq_ignore_ascii_case("en");
    if english_only && (other || language.is_empty()) {
        let wanted = if language.is_empty() {
            "auto-detect"
        } else {
            language
        };
        return Some(format!(
            "{name} only understands English, but language is {wanted}. Set language to \"en\" \
             or use a multilingual model"
        ));
    }
    None
}

fn check_bus_services(report: &mut Report, config: &Config, desktop: &SessionKind) {
    let mut bus = match Connection::session() {
        Ok(bus) => bus,
        Err(error) => {
            report.line(
                Level::Warn,
                "session bus",
                format!("{error}. Portal shortcuts, focus checks and popups are unavailable"),
            );
            return;
        }
    };
    report.line(Level::Ok, "session bus", "connected");

    let shortcuts = portal_version(&mut bus, "org.freedesktop.portal.GlobalShortcuts");
    match shortcuts {
        Some(version) => report.line(
            Level::Ok,
            "shortcut portal",
            format!("GlobalShortcuts version {version}"),
        ),
        None => report.line(
            if desktop.is_wayland() {
                Level::Warn
            } else {
                Level::Ok
            },
            "shortcut portal",
            "not available. Bind `wispx press`, `release` and `toggle` in your desktop instead",
        ),
    }

    let notifications = bus.name_has_owner("org.freedesktop.Notifications", BUS_TIMEOUT);
    if config.notifications != NotifyLevel::None {
        match notifications {
            Ok(true) => report.line(Level::Ok, "notifications", "server running"),
            _ => report.line(
                Level::Warn,
                "notifications",
                "no server is running, so there will be no popups",
            ),
        }
    }

    if !desktop.is_plasma() {
        report.line(
            Level::Warn,
            "automatic paste",
            "needs KDE Plasma, so transcripts go to the clipboard",
        );
        return;
    }
    if config.paste != PasteMode::Auto {
        report.line(
            Level::Ok,
            "automatic paste",
            format!("off (paste is {})", config.paste.as_str()),
        );
        return;
    }

    let kwin_up = bus
        .name_has_owner(kwin::BUS_NAME, BUS_TIMEOUT)
        .unwrap_or(false);
    let remote_version = portal_version(&mut bus, "org.freedesktop.portal.RemoteDesktop");
    let granted = remote::token_path().is_ok_and(|path| path.exists());
    let (level, detail) = match (kwin_up, remote_version, granted) {
        (false, _, _) => (Level::Warn, "KWin is not on the session bus".to_string()),
        (_, None, _) => (
            Level::Warn,
            "the RemoteDesktop portal is missing".to_string(),
        ),
        (true, Some(_), false) => (
            Level::Warn,
            "not allowed yet. Use Enable automatic paste in WispX, or run `wispx paste-setup`, \
             or keep copy-only"
                .to_string(),
        ),
        (true, Some(version), true) => (
            Level::Ok,
            format!("allowed (RemoteDesktop version {version})"),
        ),
    };
    report.line(level, "automatic paste", detail);
}

fn portal_version(bus: &mut Connection, interface: &str) -> Option<u32> {
    portal::interface_version(bus, interface).ok().flatten()
}

fn check_desktop_entry(report: &mut Report) {
    let path = crate::app::install::menu_entry_path();
    match path {
        Ok(path) if path.exists() => {
            report.line(Level::Ok, "desktop entry", path.display().to_string())
        }
        Ok(path) => report.line(
            Level::Warn,
            "desktop entry",
            format!(
                "{} is missing. Open WispX or run `wispx setup` so the desktop can name WispX in \
                 its dialogs",
                path.display()
            ),
        ),
        Err(error) => report.line(Level::Warn, "desktop entry", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_only_models_warn_unless_english_is_set() {
        let model = Path::new("/m/ggml-tiny.en.bin");
        assert!(language_warning(model, "en").is_none());
        assert!(language_warning(model, "EN").is_none());
        assert!(
            language_warning(model, "de")
                .unwrap()
                .contains("only understands English")
        );
        assert!(language_warning(model, "").unwrap().contains("auto-detect"));
        assert!(language_warning(Path::new("/m/ggml-base.bin"), "de").is_none());
        assert!(language_warning(Path::new("/m/ggml-small.en-q5_1.bin"), "fr").is_some());
    }
}
