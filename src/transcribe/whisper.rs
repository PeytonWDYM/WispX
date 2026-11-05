//! Local transcription through whisper.cpp's `whisper-cli`.

use super::TranscribeError;
use crate::config::WhisperConfig;
use crate::process::{self, RunError, Spec, Toolbox};
use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
const INSTALL_HINT: &str = "Install whisper.cpp (https://github.com/ggml-org/whisper.cpp) and \
                            put whisper-cli on PATH, or set whisper.binary in the config";
#[cfg(windows)]
const INSTALL_HINT: &str = "Get it from the WispX settings window, or put whisper-cli.exe on \
                            PATH, or set whisper.binary in the config";
/// What Windows exits a program with when a DLL it needs isn't there, without a word.
#[cfg(windows)]
const STATUS_DLL_NOT_FOUND: i32 = 0xC000_0135_u32 as i32;
#[cfg(windows)]
const MISSING_RUNTIME: &str = "whisper.cpp needs the Microsoft Visual C++ Redistributable, which \
                               isn't on this PC. Install it from \
                               https://aka.ms/vs/17/release/vc_redist.x64.exe and try again";
/// More threads than this barely helps whisper.cpp and starves the desktop.
const MAX_AUTO_THREADS: usize = 8;

pub fn precheck(config: &WhisperConfig, tools: &Toolbox) -> Result<(), String> {
    if !tools.has(&config.binary) {
        return Err(format!("{} not found. {INSTALL_HINT}", config.binary));
    }
    let model = &config.model;
    match model.metadata() {
        Ok(meta) if meta.is_file() && meta.len() > 0 => Ok(()),
        _ => Err(format!(
            "whisper.cpp model not found at {path}. Download one with: curl -L --create-dirs \
             -o {path} https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
            path = model.display()
        )),
    }
}

/// `whisper-cli` arguments that print only the transcript to stdout. An empty
/// language means auto-detect; whisper-cli itself would otherwise assume "en".
fn command_args(config: &WhisperConfig, language: &str, wav: &Path) -> Vec<OsString> {
    let language = match language {
        "" => "auto",
        other => other,
    };
    let threads = match config.threads {
        0 => auto_threads(),
        fixed => fixed as usize,
    };
    let mut args: Vec<OsString> = vec![
        "-m".into(),
        config.model.clone().into(),
        "-f".into(),
        wav.into(),
        "-t".into(),
        threads.to_string().into(),
        "-l".into(),
        language.into(),
        "-nt".into(),
        "-np".into(),
    ];
    args.extend(config.extra_args.iter().map(OsString::from));
    args
}

fn auto_threads() -> usize {
    thread::available_parallelism()
        .map_or(4, |count| count.get())
        .clamp(1, MAX_AUTO_THREADS)
}

/// Joins whisper's per-segment lines and drops its non-speech annotations such
/// as `[BLANK_AUDIO]` and `(wind blowing)`.
pub fn parse_transcript(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    let words: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_annotation(line))
        .collect();
    words
        .join(" ")
        .replace("[BLANK_AUDIO]", "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_annotation(line: &str) -> bool {
    let wrapped = |open: char, close: char| {
        // "(one) and (two)" is speech that happens to start and end so.
        line.strip_prefix(open)
            .and_then(|rest| rest.strip_suffix(close))
            .is_some_and(|inner| !inner.contains(close))
    };
    wrapped('[', ']') || wrapped('(', ')') || wrapped('♪', '♪')
}

pub fn transcribe(
    config: &WhisperConfig,
    language: &str,
    tools: &Toolbox,
    wav: &Path,
    cancel: &AtomicBool,
) -> Result<String, TranscribeError> {
    precheck(config, tools).map_err(TranscribeError::Failed)?;
    let program = tools
        .find(&config.binary)
        .ok_or_else(|| TranscribeError::Failed(format!("{} not found", config.binary)))?;
    let args = command_args(config, language, wav);
    let mut spec = Spec::new(&program, &args, Duration::from_secs(config.timeout_secs));
    spec.cancel = Some(cancel);

    let output = process::run(spec).map_err(|error| match error {
        RunError::Cancelled => TranscribeError::Cancelled,
        RunError::TimedOut(..) => TranscribeError::Failed(format!(
            "whisper.cpp took longer than {} s. Try a smaller model or raise \
             whisper.timeout_secs",
            config.timeout_secs
        )),
        other => TranscribeError::Failed(other.to_string()),
    })?;

    if !output.status.success() {
        #[cfg(windows)]
        if output.status.code() == Some(STATUS_DLL_NOT_FOUND) {
            return Err(TranscribeError::Failed(MISSING_RUNTIME.to_string()));
        }
        let detail = process::last_error_line(&output.stderr)
            .unwrap_or_else(|| format!("exit status {}", output.status));
        return Err(TranscribeError::Failed(format!(
            "whisper.cpp failed: {detail}"
        )));
    }
    if output.stdout_truncated {
        return Err(TranscribeError::Failed(
            "whisper.cpp printed more text than expected".to_string(),
        ));
    }
    Ok(parse_transcript(&output.stdout))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn config(binary: &str, model: PathBuf) -> WhisperConfig {
        WhisperConfig {
            binary: binary.to_string(),
            model,
            threads: 2,
            timeout_secs: 30,
            extra_args: Vec::new(),
        }
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    /// Everything a run needs, plus a script that records its arguments.
    struct Fixture {
        temp: TempDir,
        model: PathBuf,
        wav: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new();
            let model = temp.path().join("ggml-tiny.bin");
            fs::write(&model, b"model").unwrap();
            let wav = temp.path().join("audio file.wav");
            fs::write(&wav, b"wav").unwrap();
            Fixture { temp, model, wav }
        }

        fn run(&self, script_body: &str, language: &str) -> Result<String, TranscribeError> {
            self.temp.script("whisper-cli", script_body);
            let tools = Toolbox::with_path(self.temp.path());
            let cancel = AtomicBool::new(false);
            transcribe(
                &config("whisper-cli", self.model.clone()),
                language,
                &tools,
                &self.wav,
                &cancel,
            )
        }
    }

    #[test]
    fn arguments_match_the_documented_whisper_cli_flags() {
        let mut cfg = config("whisper-cli", "/m/ggml.bin".into());
        cfg.threads = 6;
        cfg.extra_args = vec!["-ng".into(), "--vad".into()];
        assert_eq!(
            strings(command_args(&cfg, "de", Path::new("/r/a b.wav"))),
            [
                "-m",
                "/m/ggml.bin",
                "-f",
                "/r/a b.wav",
                "-t",
                "6",
                "-l",
                "de",
                "-nt",
                "-np",
                "-ng",
                "--vad"
            ]
        );
    }

    #[test]
    fn empty_language_means_auto_detect_not_english() {
        let args = strings(command_args(
            &config("w", "/m".into()),
            "",
            Path::new("/a.wav"),
        ));
        let at = args.iter().position(|a| a == "-l").unwrap();
        assert_eq!(args[at + 1], "auto");
    }

    #[test]
    fn automatic_thread_count_is_bounded() {
        let mut cfg = config("w", "/m".into());
        cfg.threads = 0;
        let args = strings(command_args(&cfg, "en", Path::new("/a.wav")));
        let at = args.iter().position(|a| a == "-t").unwrap();
        let threads: usize = args[at + 1].parse().unwrap();
        assert!((1..=MAX_AUTO_THREADS).contains(&threads));
    }

    #[test]
    fn transcripts_are_joined_and_annotations_removed() {
        for (raw, expected) in [
            (
                " And so my fellow Americans,\n ask not.\n",
                "And so my fellow Americans, ask not.",
            ),
            ("\n\n", ""),
            (" [BLANK_AUDIO]\n", ""),
            (" (wind blowing)\n hello\n", "hello"),
            (" [MUSIC]\n", ""),
            (" ♪ la la ♪\n", ""),
            (" hello [BLANK_AUDIO] there\n", "hello there"),
            (" (one) and (two)\n", "(one) and (two)"),
            (" [a] then [b]\n", "[a] then [b]"),
            (" naïve café 你好\n", "naïve café 你好"),
        ] {
            assert_eq!(parse_transcript(raw.as_bytes()), expected, "{raw:?}");
        }
    }

    #[test]
    fn invalid_utf8_from_the_tool_does_not_panic() {
        assert_eq!(
            parse_transcript(b"ok \xff\xfe bytes\n"),
            "ok \u{fffd}\u{fffd} bytes"
        );
    }

    #[test]
    fn runs_the_tool_and_passes_paths_with_spaces_intact() {
        let fixture = Fixture::new();
        let log = fixture.temp.path().join("args");
        let text = fixture
            .run(
                &format!(
                    r#"printf '%s\n' "$@" > {}
echo ' Hello from whisper.'"#,
                    log.display()
                ),
                "en",
            )
            .unwrap();
        assert_eq!(text, "Hello from whisper.");
        let args = fs::read_to_string(&log).unwrap();
        assert!(args.contains(fixture.wav.to_str().unwrap()), "{args}");
        assert!(args.contains("-nt\n-np"), "{args}");
    }

    #[test]
    fn failures_surface_the_last_stderr_line() {
        let fixture = Fixture::new();
        let error = fixture
            .run(
                "echo 'error: failed to initialize whisper context' >&2\nexit 2",
                "en",
            )
            .unwrap_err();
        assert_eq!(
            error,
            TranscribeError::Failed(
                "whisper.cpp failed: error: failed to initialize whisper context".to_string()
            )
        );

        let error = fixture.run("exit 5", "en").unwrap_err();
        assert!(error.to_string().contains("exit status"), "{error}");
    }

    #[test]
    fn cancellation_kills_the_tool() {
        let fixture = Fixture::new();
        fixture.temp.script("whisper-cli", "exec sleep 60");
        let tools = Toolbox::with_path(fixture.temp.path());
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let result = transcribe(
            &config("whisper-cli", fixture.model.clone()),
            "en",
            &tools,
            &fixture.wav,
            &cancel,
        );
        assert_eq!(result, Err(TranscribeError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn timeout_is_explained() {
        let fixture = Fixture::new();
        fixture.temp.script("whisper-cli", "exec sleep 60");
        let tools = Toolbox::with_path(fixture.temp.path());
        let mut cfg = config("whisper-cli", fixture.model.clone());
        cfg.timeout_secs = 1;
        let cancel = AtomicBool::new(false);
        let error = transcribe(&cfg, "en", &tools, &fixture.wav, &cancel).unwrap_err();
        assert!(error.to_string().contains("timeout_secs"), "{error}");
    }

    #[test]
    fn precheck_gives_actionable_errors() {
        let temp = TempDir::new();
        let tools = Toolbox::with_path(temp.path());
        let model = temp.path().join("ggml.bin");

        let error = precheck(&config("whisper-cli", model.clone()), &tools).unwrap_err();
        assert!(error.contains("ggml-org/whisper.cpp"), "{error}");

        temp.script("whisper-cli", "exit 0");
        let error = precheck(&config("whisper-cli", model.clone()), &tools).unwrap_err();
        assert!(error.contains("huggingface.co"), "{error}");

        fs::write(&model, b"").unwrap();
        assert!(precheck(&config("whisper-cli", model.clone()), &tools).is_err());

        fs::write(&model, b"model").unwrap();
        precheck(&config("whisper-cli", model), &tools).unwrap();
    }
}
