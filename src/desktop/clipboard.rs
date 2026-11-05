//! The system clipboard through its native command line tools. Text always
//! travels over stdin, so it never shows up in a process listing.

use super::SessionKind;
use crate::process::{self, RunError, Spec, Toolbox};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);
/// Larger clipboards are left alone rather than half-restored.
const MAX_BACKUP: usize = 4 * 1024 * 1024;
const TEXT_TYPE: &str = "text/plain;charset=utf-8";

/// The clipboard that dictation uses here.
#[cfg(unix)]
pub type SystemClipboard = ToolClipboard;

#[cfg(windows)]
pub use crate::win::clipboard::WinClipboard as SystemClipboard;

pub trait Clipboard: Send + Sync {
    /// The current plain text, or `None` if the clipboard holds anything else.
    fn read_text(&self) -> Result<Option<String>, String>;
    /// `transient` text is only on the clipboard to be pasted right away, so
    /// clipboard managers are asked to keep it out of their history.
    fn write_text(&self, text: &str, transient: bool) -> Result<(), String>;
}

#[derive(Debug)]
enum Tools {
    Wayland {
        copy: PathBuf,
        paste: PathBuf,
        sensitive: bool,
    },
    Xclip(PathBuf),
    Xsel(PathBuf),
}

#[derive(Debug)]
pub struct ToolClipboard {
    tools: Tools,
}

impl ToolClipboard {
    pub fn detect(session: &SessionKind, tools: &Toolbox) -> Result<Self, String> {
        if *session == SessionKind::X11 {
            return Self::x11(tools);
        }
        let (Some(copy), Some(paste)) = (tools.find("wl-copy"), tools.find("wl-paste")) else {
            return Err(
                "wl-copy and wl-paste are needed for the clipboard. Install \
                        wl-clipboard (package wl-clipboard)"
                    .to_string(),
            );
        };
        let sensitive = advertises_sensitive(&copy);
        Ok(ToolClipboard {
            tools: Tools::Wayland {
                copy,
                paste,
                sensitive,
            },
        })
    }

    fn x11(tools: &Toolbox) -> Result<Self, String> {
        if let Some(path) = tools.find("xclip") {
            return Ok(ToolClipboard {
                tools: Tools::Xclip(path),
            });
        }
        if let Some(path) = tools.find("xsel") {
            return Ok(ToolClipboard {
                tools: Tools::Xsel(path),
            });
        }
        Err("xclip or xsel is needed for the clipboard on X11".to_string())
    }

    pub fn tool_name(&self) -> &'static str {
        match self.tools {
            Tools::Wayland { .. } => "wl-copy/wl-paste",
            Tools::Xclip(_) => "xclip",
            Tools::Xsel(_) => "xsel",
        }
    }

    fn read_command(&self) -> (&PathBuf, Vec<OsString>) {
        match &self.tools {
            Tools::Wayland { paste, .. } => {
                (paste, strings(&["--no-newline", "--type", "text/plain"]))
            }
            Tools::Xclip(path) => (
                path,
                strings(&["-selection", "clipboard", "-out", "-target", "UTF8_STRING"]),
            ),
            Tools::Xsel(path) => (path, strings(&["--clipboard", "--output"])),
        }
    }

    fn write_command(&self, transient: bool) -> (&PathBuf, Vec<OsString>) {
        match &self.tools {
            Tools::Wayland {
                copy, sensitive, ..
            } => {
                let mut args = strings(&["--type", TEXT_TYPE]);
                if transient && *sensitive {
                    args.push("--sensitive".into());
                }
                (copy, args)
            }
            Tools::Xclip(path) => (
                path,
                strings(&["-selection", "clipboard", "-in", "-target", "UTF8_STRING"]),
            ),
            Tools::Xsel(path) => (path, strings(&["--clipboard", "--input"])),
        }
    }
}

fn strings(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// `--sensitive` is recent. It asks Klipper not to record the text.
fn advertises_sensitive(copy: &std::path::Path) -> bool {
    let args = strings(&["--help"]);
    process::run(Spec::new(copy, &args, TIMEOUT))
        .map(|output| {
            let has = |bytes: &[u8]| String::from_utf8_lossy(bytes).contains("--sensitive");
            has(&output.stdout) || has(&output.stderr)
        })
        .unwrap_or(false)
}

impl Clipboard for ToolClipboard {
    fn read_text(&self) -> Result<Option<String>, String> {
        let (program, args) = self.read_command();
        let mut spec = Spec::new(program, &args, TIMEOUT);
        spec.stdout_limit = MAX_BACKUP;
        let output = process::run(spec).map_err(|error| error.to_string())?;
        // These tools exit non-zero for an empty clipboard or one without text.
        if !output.status.success() || output.stdout_truncated {
            return Ok(None);
        }
        Ok(String::from_utf8(output.stdout).ok())
    }

    fn write_text(&self, text: &str, transient: bool) -> Result<(), String> {
        let (program, args) = self.write_command(transient);
        let name = process::short_name(program);
        let mut spec = Spec::new(program, &args, TIMEOUT);
        spec.stdin = Some(text.as_bytes());
        // wl-copy and xclip leave a server behind to hold the selection. It must
        // not inherit pipes we would wait on.
        spec.capture = false;
        let output = process::run(spec).map_err(|error| match error {
            RunError::TimedOut(..) => format!("{name} did not respond"),
            other => other.to_string(),
        })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{name} could not set the clipboard ({})",
                output.status
            ))
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    const WAYLAND: SessionKind = SessionKind::Unknown;

    /// Fake wl-copy/wl-paste sharing a file, recording wl-copy's arguments.
    fn wayland_tools(temp: &TempDir, help: &str) -> ToolClipboard {
        let store = temp.path().join("store");
        let args = temp.path().join("args");
        temp.script(
            "wl-copy",
            &format!(
                r#"if [ "$1" = --help ]; then echo '{help}'; exit 0; fi
printf '%s\n' "$@" > {args}
cat > {store}"#,
                args = args.display(),
                store = store.display()
            ),
        );
        temp.script(
            "wl-paste",
            &format!(
                r#"[ -f {store} ] || {{ echo 'Nothing is copied' >&2; exit 1; }}
cat {store}"#,
                store = store.display()
            ),
        );
        ToolClipboard::detect(&WAYLAND, &Toolbox::with_path(temp.path())).unwrap()
    }

    #[test]
    fn text_round_trips_including_newlines_and_unicode() {
        let temp = TempDir::new();
        let clipboard = wayland_tools(&temp, "");
        assert_eq!(clipboard.read_text().unwrap(), None);
        let text = "first line\nsecond \"quoted\" $(line) 你好\n";
        clipboard.write_text(text, false).unwrap();
        assert_eq!(clipboard.read_text().unwrap().as_deref(), Some(text));
    }

    #[test]
    fn transient_text_is_marked_sensitive_only_when_supported() {
        let temp = TempDir::new();
        let args = temp.path().join("args");

        let clipboard = wayland_tools(&temp, "  -s, --sensitive  hint");
        clipboard.write_text("x", true).unwrap();
        assert_eq!(
            fs::read_to_string(&args).unwrap(),
            format!("--type\n{TEXT_TYPE}\n--sensitive\n")
        );
        clipboard.write_text("x", false).unwrap();
        assert!(!fs::read_to_string(&args).unwrap().contains("--sensitive"));

        let clipboard = wayland_tools(&temp, "  -o, --paste-once");
        clipboard.write_text("x", true).unwrap();
        assert!(!fs::read_to_string(&args).unwrap().contains("--sensitive"));
    }

    #[test]
    fn a_tool_leaving_a_background_server_does_not_block_the_write() {
        let temp = TempDir::new();
        // A server that outlives the command, like the real wl-copy.
        temp.script("wl-copy", "cat > /dev/null\nsleep 30 &\nexit 0");
        temp.script("wl-paste", "exit 1");
        let clipboard = ToolClipboard::detect(&WAYLAND, &Toolbox::with_path(temp.path())).unwrap();
        let started = std::time::Instant::now();
        clipboard.write_text("text", false).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn non_text_and_oversized_contents_are_not_treated_as_backups() {
        let temp = TempDir::new();
        temp.script("wl-copy", "cat > /dev/null");
        temp.script("wl-paste", r"printf '\377\376\000binary'");
        let clipboard = ToolClipboard::detect(&WAYLAND, &Toolbox::with_path(temp.path())).unwrap();
        assert_eq!(clipboard.read_text().unwrap(), None);
    }

    #[test]
    fn write_failures_are_reported() {
        let temp = TempDir::new();
        temp.script("wl-copy", "cat > /dev/null\nexit 3");
        temp.script("wl-paste", "exit 1");
        let clipboard = ToolClipboard::detect(&WAYLAND, &Toolbox::with_path(temp.path())).unwrap();
        let error = clipboard.write_text("x", false).unwrap_err();
        assert!(error.contains("wl-copy"), "{error}");
    }

    #[test]
    fn missing_tools_say_what_to_install() {
        let temp = TempDir::new();
        let tools = Toolbox::with_path(temp.path());
        let error = ToolClipboard::detect(&WAYLAND, &tools).unwrap_err();
        assert!(error.contains("wl-clipboard"), "{error}");
        let error = ToolClipboard::detect(&SessionKind::X11, &tools).unwrap_err();
        assert!(error.contains("xclip"), "{error}");
    }

    #[test]
    fn x11_tools_are_only_used_in_x11_sessions() {
        let temp = TempDir::new();
        temp.script("xclip", "exit 0");
        let tools = Toolbox::with_path(temp.path());
        let clipboard = ToolClipboard::detect(&SessionKind::X11, &tools).unwrap();
        assert_eq!(clipboard.tool_name(), "xclip");
        // A Wayland session never falls back to XWayland's clipboard.
        assert!(ToolClipboard::detect(&WAYLAND, &tools).is_err());
    }
}
