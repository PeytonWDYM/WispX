//! Microphone capture through a native tool that streams raw PCM on its stdout.
//!
//! Samples stay in this process's memory until a backend needs a file, and the
//! tool only ever sees a request for 16 kHz mono s16, so PipeWire, PulseAudio
//! or ALSA do any resampling.

pub mod level;
pub mod wav;

use crate::config::{AudioConfig, RecorderChoice};
use crate::process::{self, Capture, Drain, Spec, Toolbox};
use crate::sys;
use level::LevelMeter;
use std::env;
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How long a recorder may take to flush and exit after SIGTERM.
const STOP_GRACE: Duration = Duration::from_millis(800);
/// How long to wait for the last samples once a stop was requested.
const FINISH_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecorderPlan {
    pub name: &'static str,
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl RecorderPlan {
    fn new(choice: RecorderChoice, program: PathBuf, device: Option<&str>) -> Self {
        RecorderPlan {
            name: choice.as_str(),
            program,
            args: recorder_args(choice, device),
        }
    }
}

/// Command lines that write headerless 16 kHz mono s16le to stdout.
pub fn recorder_args(choice: RecorderChoice, device: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<String> = match choice {
        RecorderChoice::PwRecord => ["--rate=16000", "--channels=1", "--format=s16", "--raw"]
            .map(String::from)
            .to_vec(),
        RecorderChoice::Parec => [
            "--raw",
            "--format=s16le",
            "--rate=16000",
            "--channels=1",
            "--client-name=WispX",
            "--stream-name=Dictation",
            "--latency-msec=30",
        ]
        .map(String::from)
        .to_vec(),
        RecorderChoice::Arecord => ["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"]
            .map(String::from)
            .to_vec(),
        RecorderChoice::Auto => unreachable!("auto is resolved before building arguments"),
    };
    if let Some(device) = device {
        // The `=` form keeps a device name from being read as an option.
        args.push(match choice {
            RecorderChoice::PwRecord => format!("--target={device}"),
            _ => format!("--device={device}"),
        });
    }
    if choice == RecorderChoice::PwRecord {
        args.push("-".to_string());
    }
    args.into_iter().map(OsString::from).collect()
}

/// Finds the recorder to use without touching the microphone.
pub fn resolve(config: &AudioConfig, tools: &Toolbox) -> Result<RecorderPlan, String> {
    let device = config.device.as_deref();
    if config.recorder != RecorderChoice::Auto {
        let name = config.recorder.as_str();
        let program = tools.find(name).ok_or_else(|| {
            format!(
                "{name} is not installed, but audio.recorder asks for it. Install it \
                 ({}) or set audio.recorder to \"auto\"",
                package_hint(config.recorder)
            )
        })?;
        return Ok(RecorderPlan::new(config.recorder, program, device));
    }

    let available = Available {
        pw_record: tools.find("pw-record").filter(|p| supports_raw(p)),
        parec: tools.find("parec"),
        arecord: tools.find("arecord"),
    };
    let servers = audio_servers();
    choose(&available, servers.pipewire, servers.pulse)
        .map(|(choice, program)| RecorderPlan::new(choice, program, device))
        .ok_or_else(|| {
            "No audio capture tool found. Install one of: pw-record (pipewire), \
             parec (pulseaudio-utils), arecord (alsa-utils)"
                .to_string()
        })
}

fn package_hint(choice: RecorderChoice) -> &'static str {
    match choice {
        RecorderChoice::PwRecord => "package pipewire or pipewire-bin",
        RecorderChoice::Parec => "package pulseaudio-utils or libpulse",
        RecorderChoice::Arecord | RecorderChoice::Auto => "package alsa-utils",
    }
}

/// `--raw` is what lets pw-record write to a pipe; older versions lack it.
fn supports_raw(pw_record: &Path) -> bool {
    let args = [OsString::from("--help")];
    process::run(Spec::new(pw_record, &args, Duration::from_secs(2)))
        .map(|output| {
            let has_raw = |bytes: &[u8]| String::from_utf8_lossy(bytes).contains("--raw");
            has_raw(&output.stdout) || has_raw(&output.stderr)
        })
        .unwrap_or(false)
}

struct Servers {
    pipewire: bool,
    pulse: bool,
}

fn audio_servers() -> Servers {
    let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let exists = |relative: &str| {
        runtime
            .as_ref()
            .is_some_and(|dir| dir.join(relative).exists())
    };
    Servers {
        pipewire: exists("pipewire-0") || env::var_os("PIPEWIRE_REMOTE").is_some(),
        pulse: exists("pulse/native") || env::var_os("PULSE_SERVER").is_some(),
    }
}

struct Available {
    pw_record: Option<PathBuf>,
    parec: Option<PathBuf>,
    arecord: Option<PathBuf>,
}

/// Prefers the tool that matches a running sound server. A server tool whose
/// server is missing is still better than nothing, since its server could be
/// reachable in ways we cannot see.
fn choose(available: &Available, pipewire: bool, pulse: bool) -> Option<(RecorderChoice, PathBuf)> {
    let pw = || {
        available
            .pw_record
            .clone()
            .map(|p| (RecorderChoice::PwRecord, p))
    };
    let parec = || available.parec.clone().map(|p| (RecorderChoice::Parec, p));
    let arecord = || {
        available
            .arecord
            .clone()
            .map(|p| (RecorderChoice::Arecord, p))
    };
    let running = pipewire
        .then(pw)
        .flatten()
        .or_else(|| pulse.then(parec).flatten());
    running.or_else(arecord).or_else(pw).or_else(parec)
}

struct Shared {
    pcm: Vec<u8>,
    meter: LevelMeter,
}

pub struct Captured {
    pub pcm: Vec<u8>,
    pub duration_ms: u64,
}

/// A running capture. Dropping it stops the tool and discards the audio.
pub struct Recording {
    child: Option<Child>,
    pid: u32,
    name: &'static str,
    shared: Arc<Mutex<Shared>>,
    input_closed: Arc<AtomicBool>,
    done: Receiver<()>,
    stderr: Option<Drain>,
}

impl Recording {
    /// Starts capturing, keeping at most `max_bytes` of audio in memory.
    pub fn start(plan: &RecorderPlan, max_bytes: usize) -> Result<Self, String> {
        let mut command = Command::new(&plan.program);
        command
            .args(&plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = process::spawn_with_retry(&mut command)
            .map_err(|e| format!("cannot start {}: {e}", plan.name))?;
        let pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{} has no stdout", plan.name))?;
        let stderr = child
            .stderr
            .take()
            .map(|pipe| Drain::spawn(pipe, Capture::tail(4096)));

        let shared = Arc::new(Mutex::new(Shared {
            pcm: Vec::with_capacity(max_bytes.min(wav::BYTES_PER_SECOND as usize * 30)),
            meter: LevelMeter::default(),
        }));
        let input_closed = Arc::new(AtomicBool::new(false));
        let (done_tx, done) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            let input_closed = Arc::clone(&input_closed);
            thread::spawn(move || {
                read_samples(stdout, &shared, max_bytes);
                input_closed.store(true, Ordering::SeqCst);
                let _ = done_tx.send(());
            });
        }
        Ok(Recording {
            child: Some(child),
            pid,
            name: plan.name,
            shared,
            input_closed,
            done,
            stderr,
        })
    }

    pub fn level(&self) -> f32 {
        self.shared
            .lock()
            .map(|s| s.meter.level())
            .unwrap_or_default()
    }

    pub fn peak(&self) -> f32 {
        self.shared
            .lock()
            .map(|s| s.meter.peak())
            .unwrap_or_default()
    }

    /// True once the capture tool stopped delivering samples by itself.
    pub fn input_closed(&self) -> bool {
        self.input_closed.load(Ordering::SeqCst)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// What the tool last wrote to stderr, for explaining a failure.
    pub fn last_error(&self) -> Option<String> {
        process::last_error_line(&self.stderr.as_ref()?.peek())
    }

    /// Turns the microphone off now, without blocking. Samples stay available
    /// for `finish`, and a tool ignoring SIGTERM is killed in the background.
    pub fn request_stop(&mut self) {
        if let Some(child) = self.child.take() {
            stop_in_background(child, self.pid);
        }
    }

    /// Waits briefly for the last samples, then hands over all the audio.
    pub fn finish(mut self) -> Captured {
        self.request_stop();
        let _ = self.done.recv_timeout(FINISH_TIMEOUT);
        let mut pcm = self
            .shared
            .lock()
            .map(|mut s| std::mem::take(&mut s.pcm))
            .unwrap_or_default();
        pcm.truncate(pcm.len() & !1);
        Captured {
            duration_ms: wav::duration_ms(pcm.len() as u64),
            pcm,
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = sys::signal_group(self.pid, sys::SIGKILL);
            let _ = child.kill();
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn read_samples(mut pipe: impl Read, shared: &Mutex<Shared>, max_bytes: usize) {
    let mut buffer = [0u8; 4096];
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => {
                let Ok(mut shared) = shared.lock() else {
                    return;
                };
                shared.meter.feed(&buffer[..n]);
                let room = max_bytes.saturating_sub(shared.pcm.len());
                let taken = room.min(n);
                shared.pcm.extend_from_slice(&buffer[..taken]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn stop_in_background(mut child: Child, pid: u32) {
    let _ = sys::signal_process(pid, sys::SIGTERM);
    thread::spawn(move || {
        let deadline = Instant::now() + STOP_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() >= deadline => break,
                Ok(None) => thread::sleep(Duration::from_millis(10)),
            }
        }
        let _ = sys::signal_group(pid, sys::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;

    fn plan_for(script: PathBuf) -> RecorderPlan {
        RecorderPlan {
            name: "fake",
            program: script,
            args: Vec::new(),
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn strings(args: Vec<OsString>) -> Vec<String> {
        args.into_iter().map(|a| a.into_string().unwrap()).collect()
    }

    /// A script that writes a known PCM fixture, then idles like a live mic.
    fn fake_mic(temp: &TempDir, bytes: &[u8]) -> PathBuf {
        let fixture = temp.path().join("fixture.raw");
        fs::write(&fixture, bytes).unwrap();
        temp.script(
            "fake-mic",
            &format!("cat {}\nexec sleep 60", fixture.display()),
        )
    }

    #[test]
    fn command_lines_request_16k_mono_s16_on_stdout() {
        assert_eq!(
            strings(recorder_args(RecorderChoice::PwRecord, None)),
            ["--rate=16000", "--channels=1", "--format=s16", "--raw", "-"]
        );
        assert_eq!(
            strings(recorder_args(RecorderChoice::Parec, None)),
            [
                "--raw",
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
                "--client-name=WispX",
                "--stream-name=Dictation",
                "--latency-msec=30"
            ]
        );
        assert_eq!(
            strings(recorder_args(RecorderChoice::Arecord, None)),
            ["-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"]
        );
    }

    #[test]
    fn devices_use_the_equals_form_and_stdout_stays_last() {
        let pw = strings(recorder_args(RecorderChoice::PwRecord, Some("-mic")));
        assert_eq!(pw[pw.len() - 2], "--target=-mic");
        assert_eq!(pw[pw.len() - 1], "-");
        let parec = strings(recorder_args(RecorderChoice::Parec, Some("mic")));
        assert_eq!(parec.last().unwrap(), "--device=mic");
        let arecord = strings(recorder_args(RecorderChoice::Arecord, Some("hw:1")));
        assert_eq!(arecord.last().unwrap(), "--device=hw:1");
    }

    #[test]
    fn auto_prefers_the_tool_for_the_running_server() {
        let all = Available {
            pw_record: Some("/pw".into()),
            parec: Some("/parec".into()),
            arecord: Some("/arecord".into()),
        };
        let pick = |a: &Available, pw, pa| choose(a, pw, pa).map(|(c, _)| c);
        assert_eq!(pick(&all, true, true), Some(RecorderChoice::PwRecord));
        assert_eq!(pick(&all, false, true), Some(RecorderChoice::Parec));
        assert_eq!(pick(&all, false, false), Some(RecorderChoice::Arecord));

        let no_pw_raw = Available {
            pw_record: None,
            parec: Some("/parec".into()),
            arecord: Some("/arecord".into()),
        };
        // PipeWire running, but pw-record too old: parec via pipewire-pulse.
        assert_eq!(pick(&no_pw_raw, true, true), Some(RecorderChoice::Parec));

        let only_pw = Available {
            pw_record: Some("/pw".into()),
            parec: None,
            arecord: None,
        };
        assert_eq!(pick(&only_pw, false, false), Some(RecorderChoice::PwRecord));

        let none = Available {
            pw_record: None,
            parec: None,
            arecord: None,
        };
        assert_eq!(pick(&none, true, true), None);
    }

    #[test]
    fn auto_ignores_pw_record_without_raw_support() {
        let temp = TempDir::new();
        temp.script("pw-record", "echo 'usage: pw-record [options] file'");
        temp.script("parec", "exit 0");
        let tools = Toolbox::with_path(temp.path());
        assert!(tools.find("pw-record").is_some_and(|p| !supports_raw(&p)));

        temp.script("pw-record", "echo '  -a, --raw   RAW mode'");
        assert!(tools.find("pw-record").is_some_and(|p| supports_raw(&p)));
    }

    #[test]
    fn explicit_recorder_must_exist_and_says_how_to_get_it() {
        let temp = TempDir::new();
        let tools = Toolbox::with_path(temp.path());
        let config = AudioConfig {
            recorder: RecorderChoice::Parec,
            device: None,
        };
        let error = resolve(&config, &tools).unwrap_err();
        assert!(error.contains("pulseaudio-utils"), "{error}");
    }

    #[test]
    fn no_tools_at_all_lists_every_option() {
        let temp = TempDir::new();
        let tools = Toolbox::with_path(temp.path());
        let config = AudioConfig {
            recorder: RecorderChoice::Auto,
            device: None,
        };
        let error = resolve(&config, &tools).unwrap_err();
        for tool in ["pw-record", "parec", "arecord"] {
            assert!(error.contains(tool), "{error}");
        }
    }

    #[test]
    fn captures_samples_and_measures_them() {
        let temp = TempDir::new();
        let loud: Vec<u8> = (0..32_000u32)
            .flat_map(|n| {
                let value = if n % 2 == 0 { 8000i16 } else { -8000 };
                value.to_le_bytes()
            })
            .collect();
        let recording = Recording::start(&plan_for(fake_mic(&temp, &loud)), 1 << 20).unwrap();
        assert!(wait_for(|| recording.peak() > 0.5));
        let captured = recording.finish();
        assert_eq!(captured.pcm, loud);
        assert_eq!(captured.duration_ms, 2_000);
    }

    #[test]
    fn stop_lets_the_tool_flush_its_last_buffer_on_sigterm() {
        let temp = TempDir::new();
        // A recorder that holds back its final samples until told to stop.
        let script = temp.script(
            "flush-mic",
            "trap 'printf TAIL; exit 0' TERM\nprintf HEAD\nwhile :; do sleep 0.02; done",
        );
        let recording = Recording::start(&plan_for(script), 1024).unwrap();
        let shared = Arc::clone(&recording.shared);
        assert!(wait_for(|| shared.lock().unwrap().pcm.len() == 4));
        let captured = recording.finish();
        assert_eq!(captured.pcm, b"HEADTAIL");
    }

    #[test]
    fn a_recorder_ignoring_sigterm_is_killed() {
        let temp = TempDir::new();
        let pid_file = temp.path().join("pid");
        let script = temp.script(
            "stubborn",
            &format!(
                "echo $$ > {}\ntrap '' TERM\nwhile :; do sleep 0.05; done",
                pid_file.display()
            ),
        );
        let mut recording = Recording::start(&plan_for(script), 1024).unwrap();
        assert!(wait_for(
            || pid_file.exists() && !fs::read_to_string(&pid_file).unwrap().is_empty()
        ));
        let pid = fs::read_to_string(&pid_file).unwrap();
        recording.request_stop();
        let proc = format!("/proc/{}", pid.trim());
        assert!(wait_for(|| !Path::new(&proc).exists()), "recorder survived");
    }

    #[test]
    fn a_dead_recorder_is_noticed_and_explains_itself() {
        let temp = TempDir::new();
        let script = temp.script("broken", "echo 'Connection failure: refused' >&2\nexit 1");
        let recording = Recording::start(&plan_for(script), 1024).unwrap();
        assert!(wait_for(|| recording.input_closed()));
        assert!(wait_for(|| recording.last_error().is_some()));
        assert_eq!(
            recording.last_error().unwrap(),
            "Connection failure: refused"
        );
    }

    #[test]
    fn memory_is_bounded_even_if_a_recorder_keeps_writing() {
        let temp = TempDir::new();
        let script = temp.script("firehose", "head -c 2000000 /dev/zero\nexec sleep 60");
        let recording = Recording::start(&plan_for(script), 1000).unwrap();
        let shared = Arc::clone(&recording.shared);
        // All 2 MB drain, or the writer would block on a full pipe forever.
        assert!(wait_for(|| shared.lock().unwrap().pcm.len() == 1000));
        let captured = recording.finish();
        assert_eq!(captured.pcm.len(), 1000);
    }

    #[test]
    fn dropping_stops_the_recorder() {
        let temp = TempDir::new();
        let pid_file = temp.path().join("pid");
        let script = temp.script(
            "mic",
            &format!("echo $$ > {}\nexec sleep 60", pid_file.display()),
        );
        let recording = Recording::start(&plan_for(script), 1024).unwrap();
        assert!(wait_for(
            || pid_file.exists() && !fs::read_to_string(&pid_file).unwrap().is_empty()
        ));
        let pid = fs::read_to_string(&pid_file).unwrap();
        drop(recording);
        let proc = format!("/proc/{}", pid.trim());
        assert!(
            wait_for(|| !Path::new(&proc).exists()),
            "recorder survived drop"
        );
    }
}
