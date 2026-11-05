//! The command line: argument parsing and the short-lived commands.

use crate::app::{install, launch};
use crate::audio::wav;
use crate::config::{self, Backend, Config, HotkeyMode, Overrides, PasteMode};
use crate::daemon;
use crate::doctor;
use crate::ipc::{self, ClientError, Request};
use crate::paths;
use crate::process::Toolbox;
use crate::sys;
use crate::transcribe::{self, TranscribeError};
use serde_json::Value;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_RUNNING: i32 = 3;
const EXIT_INTERRUPTED: i32 = 130;

const HELP: &str = "\
WispX: hold a key, speak, and your words appear. Runs on your own machine by default.

USAGE
    wispx [--config PATH] [COMMAND]

    With no command, opens the WispX app: a small pill on screen, a tray icon, and
    a settings window that walks through anything still needing setup.

RUNNING
    app [--background]
                      Open the app, or bring the running one to the front. It
                      keeps running after the terminal closes; --background
                      opens no window unless something needs attention
    daemon            Run without any window, in the foreground, and listen for
                      shortcuts
        --quiet               No greeting popup
        --hotkey MODE         auto, portal or none
        --paste MODE          auto, copy or none
        plus any of the transcription options below
    setup [--autostart]
                      Install the applications-menu entry; --autostart also
                      opens WispX when you log in
    paste-setup       Ask the desktop for permission to paste automatically
    doctor [--backend NAME]
                      Check tools, models and desktop support without
                      recording or changing anything

CONTROLLING THE DAEMON
    press, release    Begin and end a hold, for desktops without portal shortcuts
    toggle            Start hands-free, keep a hold going, or finish
    start, stop       Explicit forms of the above
    cancel            Throw away the recording or running transcription
    status [--json]   Show what WispX is doing
    last              Print the last transcript
    copy-last         Copy the last transcript again
    backend [NAME]    Show or change the backend: local or chatgpt. A running
                      daemon's backend and config file are the ones that count
    reload            Re-read the config file
    quit              Stop the daemon

ONE-OFF
    transcribe FILE   Print the text of a 16 kHz mono 16-bit WAV file
        --backend NAME        local or chatgpt
        --language CODE       ISO code such as en, or auto
        --model PATH          whisper.cpp model
        --whisper-bin PATH    whisper-cli to run
        --threads N           whisper.cpp threads, 0 to choose automatically

CONFIG
    config path       Print where the config file is
    config show       Print the settings from the config file
    config init       Write a config file with the defaults

OTHER
    help, --help      Show this help
    --version         Show the version

EXIT CODES
    0 success, 1 failure, 2 bad usage, 3 daemon not running, 4 already running,
    130 interrupted

The first start asks you to confirm these KDE Plasma shortcuts:
    Ctrl+Meta+D           Hold to dictate
    Ctrl+Meta+Space       Hands-free: start, keep going, or finish
    Ctrl+Meta+Backspace   Cancel
";

/// The same help, in terms of what happens on Windows.
#[cfg(windows)]
fn help() -> String {
    const LINUX_KEYS: &str = "The first start asks you to confirm these KDE Plasma shortcuts:
    Ctrl+Meta+D           Hold to dictate
    Ctrl+Meta+Space       Hands-free: start, keep going, or finish
    Ctrl+Meta+Backspace   Cancel
";
    const WINDOWS_KEYS: &str = "The shortcuts are the same as they have always been on Windows:
    Ctrl+Win              Hold to dictate
    Ctrl+Win, then Space  Hands-free, with Ctrl+Win to finish
    Esc                   Cancel
";
    HELP.replace(LINUX_KEYS, WINDOWS_KEYS)
        .replace(
            "Install the applications-menu entry",
            "Add WispX to the Start menu",
        )
        .replace(
            "paste-setup       Ask the desktop for permission to paste automatically\n",
            "paste-setup       Not needed on Windows, where pasting is always allowed\n",
        )
        .replace(
            "for desktops without portal shortcuts",
            "for your own key bindings",
        )
}

#[cfg(unix)]
fn help() -> &'static str {
    HELP
}

#[derive(Debug, PartialEq)]
enum Command {
    App {
        background: bool,
    },
    Daemon {
        overrides: Overrides,
        quiet: bool,
        app_child: bool,
    },
    Control(Request),
    Status {
        json: bool,
    },
    Last,
    Backend(Option<Backend>),
    Transcribe {
        file: PathBuf,
        overrides: Overrides,
    },
    Doctor {
        overrides: Overrides,
    },
    ConfigPath,
    ConfigShow,
    ConfigInit,
    Setup {
        autostart: bool,
    },
    Help,
    Version,
}

#[derive(Debug, PartialEq)]
struct Parsed {
    config: Option<PathBuf>,
    command: Command,
}

/// Walks the arguments, keeping non-UTF-8 values usable as paths.
struct Args {
    rest: std::vec::IntoIter<OsString>,
}

impl Args {
    fn next_os(&mut self) -> Option<OsString> {
        self.rest.next()
    }

    fn next_text(&mut self) -> Result<Option<String>, String> {
        self.rest
            .next()
            .map(|arg| {
                arg.into_string()
                    .map_err(|arg| format!("`{}` is not valid UTF-8", arg.to_string_lossy()))
            })
            .transpose()
    }

    fn value_of(&mut self, flag: &str) -> Result<String, String> {
        self.next_text()?
            .ok_or_else(|| format!("{flag} needs a value"))
    }

    fn finish(&mut self) -> Result<(), String> {
        match self.rest.next() {
            Some(extra) => Err(format!("unexpected argument `{}`", extra.to_string_lossy())),
            None => Ok(()),
        }
    }
}

fn parse_choice<T>(
    flag: &str,
    value: &str,
    parse: impl Fn(&str) -> Option<T>,
    choices: &str,
) -> Result<T, String> {
    parse(value).ok_or_else(|| format!("{flag} must be one of: {}", choices.trim()))
}

fn parse_backend(flag: &str, value: &str) -> Result<Backend, String> {
    parse_choice(flag, value, Backend::parse, Backend::CHOICES)
}

/// Applies a transcription option, or returns false if `flag` is not one.
fn transcription_flag(
    flag: &str,
    args: &mut Args,
    overrides: &mut Overrides,
) -> Result<bool, String> {
    match flag {
        "--backend" => overrides.backend = Some(parse_backend(flag, &args.value_of(flag)?)?),
        "--language" => overrides.language = Some(args.value_of(flag)?),
        "--model" => overrides.model = Some(args.value_of(flag)?),
        "--whisper-bin" => overrides.whisper_bin = Some(args.value_of(flag)?),
        "--threads" => {
            let value = args.value_of(flag)?;
            let threads = value
                .parse::<u32>()
                .ok()
                .filter(|threads| *threads <= 1024)
                .ok_or_else(|| format!("--threads must be a number up to 1024, not `{value}`"))?;
            overrides.threads = Some(threads);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn parse_arguments(arguments: Vec<OsString>) -> Result<Parsed, String> {
    let mut args = Args {
        rest: arguments.into_iter(),
    };
    let mut config = None;
    let command = loop {
        match args.next_text_or_path()? {
            Some(Word::Text(word)) if word == "--config" => {
                let path = args
                    .next_os()
                    .ok_or_else(|| "--config needs a value".to_string())?;
                config = Some(absolute_path(PathBuf::from(path))?);
            }
            Some(Word::Text(word)) => break word,
            Some(Word::Other(word)) => {
                return Err(format!("unknown command `{}`", word.to_string_lossy()));
            }
            None => {
                return Ok(Parsed {
                    config,
                    command: Command::App { background: false },
                });
            }
        }
    };
    let command = parse_command(&command, &mut args)?;
    Ok(Parsed { config, command })
}

enum Word {
    Text(String),
    Other(OsString),
}

impl Args {
    fn next_text_or_path(&mut self) -> Result<Option<Word>, String> {
        Ok(self.rest.next().map(|arg| match arg.into_string() {
            Ok(text) => Word::Text(text),
            Err(other) => Word::Other(other),
        }))
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path);
    }
    // The daemon may reload this file much later, from wherever it was started.
    env::current_dir()
        .map(|dir| dir.join(path))
        .map_err(|error| format!("cannot find the current directory: {error}"))
}

fn parse_command(command: &str, args: &mut Args) -> Result<Command, String> {
    let control = |request: Request, args: &mut Args| {
        args.finish()?;
        Ok(Command::Control(request))
    };
    match command {
        "app" => match args.next_text()?.as_deref() {
            None => Ok(Command::App { background: false }),
            Some("--background") => {
                args.finish()?;
                Ok(Command::App { background: true })
            }
            Some(other) => Err(format!("unexpected argument `{other}`")),
        },
        "daemon" => parse_daemon(args),
        "press" => control(Request::Press, args),
        "release" => control(Request::Release, args),
        "start" => control(Request::Start, args),
        "stop" => control(Request::Stop, args),
        "toggle" => control(Request::Toggle, args),
        "cancel" => control(Request::Cancel, args),
        "copy-last" => control(Request::CopyLast, args),
        "reload" => control(Request::Reload, args),
        "quit" => control(Request::Quit, args),
        "paste-setup" => control(Request::PasteSetup, args),
        "last" => {
            args.finish()?;
            Ok(Command::Last)
        }
        "status" => match args.next_text()?.as_deref() {
            None => Ok(Command::Status { json: false }),
            Some("--json") => {
                args.finish()?;
                Ok(Command::Status { json: true })
            }
            Some(other) => Err(format!("unexpected argument `{other}`")),
        },
        "backend" => {
            let backend = args
                .next_text()?
                .map(|name| parse_backend("backend", &name))
                .transpose()?;
            args.finish()?;
            Ok(Command::Backend(backend))
        }
        "transcribe" => parse_transcribe(args),
        "doctor" => {
            let mut overrides = Overrides::default();
            while let Some(flag) = args.next_text()? {
                if flag != "--backend" || !transcription_flag(&flag, args, &mut overrides)? {
                    return Err(format!("unexpected argument `{flag}`"));
                }
            }
            Ok(Command::Doctor { overrides })
        }
        "config" => {
            let what = args.next_text()?;
            args.finish()?;
            match what.as_deref() {
                Some("path") => Ok(Command::ConfigPath),
                Some("show") => Ok(Command::ConfigShow),
                Some("init") => Ok(Command::ConfigInit),
                _ => Err("use `wispx config path`, `show` or `init`".to_string()),
            }
        }
        "setup" => match args.next_text()?.as_deref() {
            None => Ok(Command::Setup { autostart: false }),
            Some("--autostart") => {
                args.finish()?;
                Ok(Command::Setup { autostart: true })
            }
            Some(other) => Err(format!("unexpected argument `{other}`")),
        },
        "help" | "--help" | "-h" => Ok(Command::Help),
        "version" | "--version" | "-V" => Ok(Command::Version),
        other => Err(format!("unknown command `{other}`")),
    }
}

fn parse_daemon(args: &mut Args) -> Result<Command, String> {
    let mut overrides = Overrides::default();
    let mut quiet = false;
    let mut app_child = false;
    while let Some(flag) = args.next_text()? {
        match flag.as_str() {
            "--quiet" => quiet = true,
            // Only for the app starting its own daemon, so it is not in the help.
            "--app-child" => app_child = true,
            "--hotkey" => {
                let value = args.value_of(&flag)?;
                overrides.hotkey = Some(parse_choice(
                    &flag,
                    &value,
                    HotkeyMode::parse,
                    HotkeyMode::CHOICES,
                )?);
            }
            "--paste" => {
                let value = args.value_of(&flag)?;
                overrides.paste = Some(parse_choice(
                    &flag,
                    &value,
                    PasteMode::parse,
                    PasteMode::CHOICES,
                )?);
            }
            other => {
                if !transcription_flag(other, args, &mut overrides)? {
                    return Err(format!("unexpected argument `{other}`"));
                }
            }
        }
    }
    Ok(Command::Daemon {
        overrides,
        quiet,
        app_child,
    })
}

fn parse_transcribe(args: &mut Args) -> Result<Command, String> {
    let mut overrides = Overrides::default();
    let mut file = None;
    while let Some(arg) = args.next_os() {
        match arg.to_str() {
            Some(flag) if flag.starts_with("--") => {
                if !transcription_flag(flag, args, &mut overrides)? {
                    return Err(format!("unexpected argument `{flag}`"));
                }
            }
            _ if file.is_some() => {
                return Err(format!("unexpected argument `{}`", arg.to_string_lossy()));
            }
            _ => file = Some(PathBuf::from(arg)),
        }
    }
    let file = file.ok_or("transcribe needs a WAV file")?;
    Ok(Command::Transcribe { file, overrides })
}

pub fn main() -> i32 {
    let parsed = match parse_arguments(env::args_os().skip(1).collect()) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("wispx: {message}. Run `wispx help` for usage");
            return EXIT_USAGE;
        }
    };
    let explicit_config = parsed.config.is_some();
    if let Command::App { background } = parsed.command {
        return launch::open(parsed.config.as_deref(), background);
    }
    let config_path = match parsed.config.map_or_else(paths::default_config_file, Ok) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("wispx: {error}");
            return EXIT_FAILED;
        }
    };
    execute(parsed.command, config_path, explicit_config)
}

fn execute(command: Command, config_path: PathBuf, explicit_config: bool) -> i32 {
    match command {
        Command::Help => {
            print!("{}", help());
            EXIT_OK
        }
        Command::Version => {
            println!("wispx {}", env!("CARGO_PKG_VERSION"));
            EXIT_OK
        }
        // Handled before a config path is worked out, by the app itself.
        Command::App { .. } => EXIT_USAGE,
        Command::Daemon {
            overrides,
            quiet,
            app_child,
        } => daemon::run(daemon::Options {
            config_path,
            overrides,
            quiet,
            app_child,
        }),
        Command::Control(request) => control(request),
        Command::Status { json } => status(json),
        Command::Last => last(),
        Command::Backend(backend) => report(match backend {
            Some(backend) => set_backend(&config_path, explicit_config, backend),
            None => show_backend(&config_path, explicit_config),
        }),
        Command::Transcribe { file, overrides } => transcribe_file(&config_path, &file, &overrides),
        Command::Doctor { overrides } => doctor::run(&config_path, &overrides),
        Command::ConfigPath => {
            println!("{}", config_path.display());
            EXIT_OK
        }
        Command::ConfigShow => config_show(&config_path),
        Command::ConfigInit => report(
            config::init_file(&config_path).map(|()| format!("Wrote {}", config_path.display())),
        ),
        Command::Setup { autostart } => report(setup(autostart)),
    }
}

fn report(result: Result<String, String>) -> i32 {
    match result {
        Ok(message) => {
            println!("{message}");
            EXIT_OK
        }
        Err(message) => {
            eprintln!("wispx: {message}");
            EXIT_FAILED
        }
    }
}

fn request(request: Request) -> Result<Value, i32> {
    let socket = paths::client_socket_path();
    match ipc::send(&socket, request) {
        Ok(reply) => Ok(reply),
        Err(ClientError::NotRunning) => {
            eprintln!("wispx: not running. Open WispX, or start it with `wispx daemon`");
            Err(EXIT_NOT_RUNNING)
        }
        Err(ClientError::Failed(message)) => {
            eprintln!("wispx: {message}");
            Err(EXIT_FAILED)
        }
    }
}

fn message_of(reply: &Value) -> &str {
    reply["message"].as_str().unwrap_or_default()
}

fn control(what: Request) -> i32 {
    let reply = match request(what) {
        Ok(reply) => reply,
        Err(code) => return code,
    };
    if reply["ok"] == true {
        if !message_of(&reply).is_empty() {
            println!("{}", message_of(&reply));
        }
        EXIT_OK
    } else {
        eprintln!("wispx: {}", message_of(&reply));
        EXIT_FAILED
    }
}

fn status(json: bool) -> i32 {
    let reply = match request(Request::Status) {
        Ok(reply) => reply,
        Err(code) => return code,
    };
    if json {
        println!("{reply}");
        return EXIT_OK;
    }
    let text = |key: &str| reply[key].as_str().unwrap_or("unknown");
    println!("state:     {}", text("state"));
    println!("backend:   {}", text("backend"));
    println!("config:    {}", text("config"));
    println!("session:   {}", text("session"));
    println!("shortcuts: {}", text("hotkey"));
    println!("delivery:  {}", text("paste"));
    if let Some(error) = reply["last_error"].as_str() {
        println!("problem:   {error}");
    }
    EXIT_OK
}

fn last() -> i32 {
    let reply = match request(Request::Last) {
        Ok(reply) => reply,
        Err(code) => return code,
    };
    match reply["transcript"].as_str() {
        Some(text) => {
            println!("{text}");
            EXIT_OK
        }
        None => {
            eprintln!("wispx: {}", message_of(&reply));
            EXIT_FAILED
        }
    }
}

/// The status of the running daemon, if there is one. While it runs, the backend
/// it is using and the file it was started with are the ones that count, whatever
/// the default file says. Naming some other file is refused rather than guessed
/// at: it would change nothing the daemon does.
fn running_daemon(config_path: &Path, explicit_config: bool) -> Result<Option<Value>, String> {
    let status = match ipc::send(&paths::client_socket_path(), Request::Status) {
        Ok(status) => status,
        Err(ClientError::NotRunning) => return Ok(None),
        Err(ClientError::Failed(error)) => {
            return Err(format!("cannot ask the daemon what it is using: {error}"));
        }
    };
    if status["ok"] != true {
        return Err(message_of(&status).to_string());
    }
    if explicit_config {
        let in_use = status["config"].as_str().unwrap_or("an unknown file");
        if !paths::same_file(Path::new(in_use), config_path) {
            return Err(format!(
                "the running daemon uses {in_use}, not {}, so nothing changed. Leave out \
                 --config to work with the daemon, or stop it with `wispx quit` to work with \
                 that file",
                config_path.display()
            ));
        }
    }
    Ok(Some(status))
}

fn show_backend(config_path: &Path, explicit_config: bool) -> Result<String, String> {
    match running_daemon(config_path, explicit_config)? {
        Some(status) => status["backend"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "the daemon did not say which backend it is using".to_string()),
        None => Ok(Config::load(config_path)?.backend.as_str().to_string()),
    }
}

fn set_backend(
    config_path: &Path,
    explicit_config: bool,
    backend: Backend,
) -> Result<String, String> {
    let mut message = if running_daemon(config_path, explicit_config)?.is_some() {
        set_daemon_backend(backend)?
    } else {
        config::persist_backend(config_path, backend)?;
        // A daemon starting right now may have read the file just before that.
        if running_daemon(config_path, explicit_config)?.is_some() {
            set_daemon_backend(backend)?
        } else {
            format!(
                "Backend: {}\nSaved in {}",
                backend.as_str(),
                config_path.display()
            )
        }
    };
    if backend == Backend::ChatGpt {
        message.push_str("\nYour audio will now be sent to chatgpt.com when you dictate.");
    }
    Ok(message)
}

/// The daemon saves and applies the change together, so success here means both.
fn set_daemon_backend(backend: Backend) -> Result<String, String> {
    match ipc::send(&paths::client_socket_path(), Request::SetBackend(backend)) {
        Ok(reply) if reply["ok"] == true => Ok(message_of(&reply).to_string()),
        Ok(reply) => Err(message_of(&reply).to_string()),
        Err(ClientError::NotRunning) => {
            Err("the daemon stopped before it could make the change. Try again".to_string())
        }
        Err(ClientError::Failed(error)) => Err(format!(
            "the daemon did not confirm the change: {error}. Check with `wispx status`"
        )),
    }
}

fn transcribe_file(config_path: &Path, file: &Path, overrides: &Overrides) -> i32 {
    let run = || -> Result<String, TranscribeError> {
        let failed = TranscribeError::Failed;
        let mut config = Config::load(config_path).map_err(failed)?;
        overrides.apply(&mut config).map_err(failed)?;
        let bytes = fs::read(file)
            .map_err(|error| failed(format!("cannot read {}: {error}", file.display())))?;
        let info =
            wav::parse(&bytes).map_err(|error| failed(format!("{}: {error}", file.display())))?;
        if !info.is_dictation_format() {
            return Err(failed(format!(
                "{} must be 16 kHz mono 16-bit PCM. Convert it with: ffmpeg -i in -ar 16000 -ac \
                 1 -c:a pcm_s16le out.wav",
                file.display()
            )));
        }
        drop(bytes);
        let tools = Toolbox::from_env();
        transcribe::precheck(&config, &tools).map_err(failed)?;
        let cancel = AtomicBool::new(false);
        transcribe::transcribe(&config, &tools, file, info.duration_ms(), &cancel)
    };
    sys::install_shutdown_handlers();
    match run() {
        Ok(text) => {
            println!("{text}");
            EXIT_OK
        }
        Err(TranscribeError::Cancelled) => EXIT_INTERRUPTED,
        Err(TranscribeError::Failed(message)) => {
            eprintln!("wispx: {message}");
            EXIT_FAILED
        }
    }
}

fn config_show(config_path: &Path) -> i32 {
    match Config::load(config_path) {
        Ok(config) => {
            match serde_json::to_string_pretty(&config.to_json()) {
                Ok(text) => println!("{text}"),
                Err(error) => {
                    eprintln!("wispx: {error}");
                    return EXIT_FAILED;
                }
            }
            EXIT_OK
        }
        Err(error) => {
            eprintln!("wispx: {error}");
            EXIT_FAILED
        }
    }
}

/// Writes user-level desktop entries. Only runs when explicitly asked to.
fn setup(autostart: bool) -> Result<String, String> {
    let path = install::install_menu_entry()?;
    install::install_icons();
    let mut message = format!("Installed {}", path.display());
    message.push_str(if launch::app_available() {
        "\nNext: open WispX from your applications menu, or run `wispx`."
    } else {
        "\nThis build has no desktop app, so the entry runs `wispx daemon --quiet`."
    });
    if autostart {
        let path = install::set_autostart(true)?;
        message.push_str(&format!(
            "\nInstalled {}\nWispX will start when you log in.",
            path.display()
        ));
    }
    Ok(message)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn parse(words: &[&str]) -> Result<Parsed, String> {
        parse_arguments(words.iter().map(OsString::from).collect())
    }

    fn command(words: &[&str]) -> Command {
        parse(words).unwrap().command
    }

    #[test]
    fn simple_commands_map_to_requests() {
        for (word, expected) in [
            ("press", Request::Press),
            ("release", Request::Release),
            ("start", Request::Start),
            ("stop", Request::Stop),
            ("toggle", Request::Toggle),
            ("cancel", Request::Cancel),
            ("copy-last", Request::CopyLast),
            ("reload", Request::Reload),
            ("quit", Request::Quit),
            ("paste-setup", Request::PasteSetup),
        ] {
            assert_eq!(command(&[word]), Command::Control(expected));
            assert!(parse(&[word, "extra"]).is_err(), "{word}");
        }
    }

    #[test]
    fn daemon_flags_override_settings() {
        let Command::Daemon {
            overrides, quiet, ..
        } = command(&[
            "daemon",
            "--quiet",
            "--backend",
            "chatgpt",
            "--hotkey",
            "none",
            "--paste",
            "copy",
            "--language",
            "de",
            "--model",
            "/m.bin",
            "--whisper-bin",
            "/w",
            "--threads",
            "4",
        ])
        else {
            panic!("not a daemon command");
        };
        assert!(quiet);
        assert_eq!(overrides.backend, Some(Backend::ChatGpt));
        assert_eq!(overrides.hotkey, Some(HotkeyMode::None));
        assert_eq!(overrides.paste, Some(PasteMode::Copy));
        assert_eq!(overrides.language.as_deref(), Some("de"));
        assert_eq!(overrides.model.as_deref(), Some("/m.bin"));
        assert_eq!(overrides.whisper_bin.as_deref(), Some("/w"));
        assert_eq!(overrides.threads, Some(4));
    }

    #[test]
    fn bad_usage_is_rejected_with_a_helpful_message() {
        for (words, expected) in [
            (&["launch"][..], "unknown command"),
            (&["app", "--everywhere"], "unexpected argument"),
            (&["daemon", "--bogus"], "unexpected argument"),
            (&["daemon", "--backend"], "needs a value"),
            (
                &["daemon", "--backend", "aws"],
                "must be one of: local chatgpt",
            ),
            (&["daemon", "--hotkey", "evdev"], "must be one of"),
            (&["daemon", "--threads", "lots"], "--threads"),
            (&["daemon", "--threads", "5000"], "--threads"),
            (&["transcribe"], "needs a WAV file"),
            (&["transcribe", "a.wav", "b.wav"], "unexpected argument"),
            (
                &["transcribe", "a.wav", "--paste", "copy"],
                "unexpected argument",
            ),
            (&["doctor", "--model", "m"], "unexpected argument"),
            (&["status", "--xml"], "unexpected argument"),
            (&["backend", "aws"], "must be one of"),
            (&["backend", "local", "x"], "unexpected argument"),
            (&["config"], "wispx config"),
            (&["config", "delete"], "wispx config"),
            (&["setup", "--everywhere"], "unexpected argument"),
            (&["--config"], "needs a value"),
        ] {
            let error = parse(words).unwrap_err();
            assert!(error.contains(expected), "{words:?}: {error}");
        }
    }

    #[test]
    fn transcribe_takes_a_file_and_options_in_any_order() {
        let Command::Transcribe { file, overrides } = command(&[
            "transcribe",
            "--language",
            "en",
            "my audio.wav",
            "--threads",
            "2",
        ]) else {
            panic!("not a transcribe command");
        };
        assert_eq!(file, PathBuf::from("my audio.wav"));
        assert_eq!(overrides.language.as_deref(), Some("en"));
        assert_eq!(overrides.threads, Some(2));
    }

    #[test]
    fn non_utf8_file_names_work_for_transcribe() {
        use std::os::unix::ffi::OsStringExt;
        let name = OsString::from_vec(b"caf\xe9.wav".to_vec());
        let parsed = parse_arguments(vec!["transcribe".into(), name.clone()]).unwrap();
        assert_eq!(
            parsed.command,
            Command::Transcribe {
                file: PathBuf::from(name),
                overrides: Overrides::default(),
            }
        );
    }

    #[test]
    fn global_config_path_is_made_absolute() {
        let parsed = parse(&["--config", "/etc/w.json", "status"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("/etc/w.json")));
        assert_eq!(parsed.command, Command::Status { json: false });

        let parsed = parse(&["--config", "w.json", "last"]).unwrap();
        assert!(parsed.config.unwrap().is_absolute());
    }

    #[test]
    fn status_backend_config_and_setup_variants() {
        assert_eq!(
            command(&["status", "--json"]),
            Command::Status { json: true }
        );
        assert_eq!(command(&["backend"]), Command::Backend(None));
        assert_eq!(
            command(&["backend", "chatgpt"]),
            Command::Backend(Some(Backend::ChatGpt))
        );
        assert_eq!(command(&["config", "path"]), Command::ConfigPath);
        assert_eq!(command(&["config", "show"]), Command::ConfigShow);
        assert_eq!(command(&["config", "init"]), Command::ConfigInit);
        assert_eq!(command(&["setup"]), Command::Setup { autostart: false });
        assert_eq!(
            command(&["setup", "--autostart"]),
            Command::Setup { autostart: true }
        );
        assert_eq!(command(&["--help"]), Command::Help);
        assert_eq!(command(&["--version"]), Command::Version);
    }

    #[test]
    fn doctor_accepts_only_a_backend_choice() {
        let Command::Doctor { overrides } = command(&["doctor", "--backend", "chatgpt"]) else {
            panic!("not a doctor command");
        };
        assert_eq!(overrides.backend, Some(Backend::ChatGpt));
    }

    #[test]
    fn no_command_opens_the_app() {
        assert_eq!(command(&[]), Command::App { background: false });
        assert_eq!(command(&["app"]), Command::App { background: false });
        assert_eq!(
            command(&["app", "--background"]),
            Command::App { background: true }
        );
        let parsed = parse(&["--config", "/etc/w.json"]).unwrap();
        assert_eq!(parsed.config, Some(PathBuf::from("/etc/w.json")));
        assert_eq!(parsed.command, Command::App { background: false });
    }

    #[test]
    fn the_app_marks_its_own_daemon() {
        let Command::Daemon { app_child, .. } = command(&["daemon", "--quiet", "--app-child"])
        else {
            panic!("not a daemon command");
        };
        assert!(app_child);
    }
}
