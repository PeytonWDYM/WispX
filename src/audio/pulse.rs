//! Linux Listen capture through the sound server's PulseAudio interface (PulseAudio
//! itself or PipeWire's pipewire-pulse). CPAL reaches only ALSA and JACK here, which
//! can't record another app's output, so `pactl` lists devices and `parec` records a
//! sink's monitor, already converted to 16 kHz mono by the server.
//!
//! A recording must never continue on a device nobody chose. PipeWire is told not to
//! move the stream. PulseAudio has no such option in `parec`, and its rescue module
//! moves a stream whose device goes away, so `parec --verbose` reports where the stream
//! connects and every move, and a move ends the capture.
use super::{
    capture::{CaptureError, Device, Devices, Input, Kind},
    recorder::RATE,
};
use std::{
    io::Read,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MISSING: &str = "Listen needs PulseAudio's parec and pactl (the pulseaudio-utils package, or libpulse on Arch), with PulseAudio or PipeWire's pipewire-pulse running.";
const UNRESPONSIVE: &str =
    "The sound server isn't responding. Restart PulseAudio or PipeWire's pipewire-pulse.";
const MOVED: &str =
    "the device went away or was switched, so WispX stopped instead of recording another one";
/// How long `pactl` and a new recording may take before the server counts as hung.
const SERVER_TIMEOUT: Duration = Duration::from_secs(3);

fn command(tool: &str) -> Command {
    let mut command = Command::new(tool);
    // Parsed output must not be translated.
    command.env("LC_ALL", "C").stdin(Stdio::null());
    command
}
/// Runs `pactl`, killing it if the server doesn't answer within `SERVER_TIMEOUT`.
fn pactl(args: &[&str]) -> Result<String, CaptureError> {
    let mut child = command("pactl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => CaptureError(MISSING.into()),
            _ => CaptureError(format!("Couldn't run pactl: {error}")),
        })?;
    // Read beside the wait, so a long listing can't fill the pipe and block pactl.
    let read = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let stdout = read(Box::new(child.stdout.take().expect("piped stdout")));
    let stderr = read(Box::new(child.stderr.take().expect("piped stderr")));
    let deadline = Instant::now() + SERVER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CaptureError(UNRESPONSIVE.into()));
            }
        }
    };
    let (stdout, stderr) = (
        stdout.join().unwrap_or_default(),
        stderr.join().unwrap_or_default(),
    );
    if !status.success() {
        return Err(server_error(&stderr));
    }
    Ok(stdout)
}
fn server_error(stderr: &str) -> CaptureError {
    let line = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    CaptureError(if line.contains("Connection") || line.contains("connect") {
        "Couldn't reach the sound server. Start PulseAudio or PipeWire's pipewire-pulse.".into()
    } else if line.contains("No such") || line.contains("Entity killed") {
        "That audio device isn't available anymore. Choose another.".into()
    } else if line.is_empty() {
        "The sound server stopped the recording.".into()
    } else {
        format!("The sound server stopped the recording ({line}).")
    })
}

/// One `pactl list sinks|sources` entry: its top-level `Key: value` fields.
#[derive(Debug, Default)]
struct Entry {
    name: String,
    description: String,
    monitor_source: String,
    monitor_of: String,
}
fn entries(text: &str) -> Vec<Entry> {
    let mut list: Vec<Entry> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Sink #") || line.starts_with("Source #") {
            list.push(Entry::default());
            continue;
        }
        // Fields sit one tab deep; properties and ports sit deeper.
        let (Some(entry), Some(field)) = (list.last_mut(), line.strip_prefix('\t')) else {
            continue;
        };
        if field.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = field.split_once(": ") else {
            continue;
        };
        let value = value.trim().to_owned();
        match key {
            "Name" => entry.name = value,
            "Description" => entry.description = value,
            "Monitor Source" => entry.monitor_source = value,
            "Monitor of Sink" => entry.monitor_of = value,
            _ => {}
        }
    }
    list
}
fn label(entry: &Entry) -> String {
    if entry.description.is_empty() {
        entry.name.clone()
    } else {
        entry.description.clone()
    }
}
/// Outputs are recorded through their monitor sources; microphones are real sources.
fn parse(sinks: &str, sources: &str) -> Devices {
    Devices {
        outputs: entries(sinks)
            .iter()
            .filter(|e| !e.name.is_empty())
            .map(|e| Device {
                id: if e.monitor_source.is_empty() {
                    format!("{}.monitor", e.name)
                } else {
                    e.monitor_source.clone()
                },
                name: label(e),
            })
            .collect(),
        microphones: entries(sources)
            .iter()
            .filter(|e| !e.name.is_empty() && matches!(e.monitor_of.as_str(), "" | "n/a"))
            .map(|e| Device {
                id: e.name.clone(),
                name: label(e),
            })
            .collect(),
    }
}
pub(super) fn devices() -> Result<Devices, CaptureError> {
    Ok(parse(
        &pactl(&["list", "sinks"])?,
        &pactl(&["list", "sources"])?,
    ))
}

/// The default device when none is chosen; the server resolves it at start.
fn device(kind: Kind, id: &str) -> String {
    match (kind, id) {
        (Kind::Output, "") => "@DEFAULT_MONITOR@".into(),
        (Kind::Microphone, "") => "@DEFAULT_SOURCE@".into(),
        (_, id) => id.into(),
    }
}
fn arguments(kind: Kind, id: &str) -> Vec<String> {
    vec![
        format!("--device={}", device(kind, id)),
        // Reports the device the stream connects to and any move (`watch`).
        "--verbose".into(),
        "--raw".into(),
        "--format=s16le".into(),
        format!("--rate={RATE}"),
        "--channels=1".into(),
        "--latency-msec=100".into(),
        "--client-name=WispX".into(),
        // PipeWire otherwise moves the stream to another device when this one goes away,
        // which would record something else without saying so.
        "--property=node.dont-reconnect=true".into(),
        "--property=node.dont-fallback=true".into(),
        format!(
            "--stream-name=WispX Listen ({})",
            match kind {
                Kind::Output => "computer audio",
                Kind::Microphone => "microphone",
            }
        ),
    ]
}
/// Little-endian samples, keeping an odd trailing byte for the next read.
fn decode(bytes: &[u8], carry: &mut Option<u8>, pcm: &mut Vec<i16>) {
    pcm.clear();
    let mut rest = bytes;
    if let Some(low) = carry.take()
        && let Some((&high, tail)) = rest.split_first()
    {
        pcm.push(i16::from_le_bytes([low, high]));
        rest = tail;
    }
    let (pairs, tail) = rest.as_chunks::<2>();
    pcm.extend(pairs.iter().map(|&pair| i16::from_le_bytes(pair)));
    *carry = tail.first().copied();
}

/// What one line of `parec --verbose` says about the stream's device.
#[derive(Debug, Eq, PartialEq)]
enum Report<'a> {
    Connected(&'a str),
    Moved(&'a str),
    /// Anything else worth keeping, such as a stream error.
    Other(&'a str),
}
/// Reads a verbose line, as printed with `LC_ALL=C`; `None` for status noise.
fn report(line: &str) -> Option<Report<'_>> {
    // pacat clears the rest of some lines with an ANSI erase sequence.
    let line = line.trim().trim_end_matches("\x1b[K").trim();
    let device = |rest: &'static str| {
        line.strip_prefix(rest)
            .and_then(|tail| tail.rsplit_once(" ("))
            .map(|(device, _)| device)
    };
    if let Some(device) = device("Connected to device ") {
        Some(Report::Connected(device))
    } else if let Some(device) = device("Stream moved to device ") {
        Some(Report::Moved(device))
    } else if line.is_empty() || line.starts_with("Time: ") {
        None
    } else {
        Some(Report::Other(line))
    }
}
/// Follows `parec`'s reports: sends the connected device once, and fails the source
/// when the stream lands on any other device. Returns the last other line, which
/// explains an exit.
fn watch(
    stderr: impl Read,
    chosen: Option<&str>,
    input: &Input,
    connected: &mpsc::SyncSender<String>,
) -> String {
    let mut last = String::new();
    let mut handle = |line: &[u8]| match report(&String::from_utf8_lossy(line)) {
        Some(Report::Connected(device)) => {
            if chosen.is_some_and(|chosen| chosen != device) {
                input.fail(MOVED);
            }
            let _ = connected.try_send(device.to_owned());
        }
        Some(Report::Moved(_)) => input.fail(MOVED),
        Some(Report::Other(text)) => last = text.chars().take(300).collect(),
        None => {}
    };
    // pacat ends its periodic status lines with a carriage return, not a newline, so
    // split on both; a line is capped so an endless one can't grow without bound.
    let mut line = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut stderr = stderr;
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &byte in &buffer[..read] {
            if byte == b'\n' || byte == b'\r' {
                handle(&line);
                line.clear();
            } else if line.len() < 1024 {
                line.push(byte);
            }
        }
    }
    handle(&line);
    last
}

/// Kills `parec` and joins its readers when the capture ends, however it ends.
struct Recorder {
    child: Child,
    stopping: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}
impl Drop for Recorder {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
pub(super) fn open(kind: Kind, id: &str, input: Input) -> Result<Box<dyn Send>, CaptureError> {
    record(&arguments(kind, id), (!id.is_empty()).then_some(id), input)
}
fn record(
    arguments: &[String],
    chosen: Option<&str>,
    input: Input,
) -> Result<Box<dyn Send>, CaptureError> {
    let mut child = command("parec")
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => CaptureError(MISSING.into()),
            _ => CaptureError(format!("Couldn't start parec: {error}")),
        })?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stopping = Arc::new(AtomicBool::new(false));
    let (connected, ready) = mpsc::sync_channel(1);
    let reader = {
        let (input, stopping) = (input.clone(), stopping.clone());
        let chosen = chosen.map(str::to_owned);
        thread::Builder::new()
            .name("listen-parec".into())
            .spawn(move || {
                // Read beside stdout so a chatty server can't block the recording.
                let errors = {
                    let input = input.clone();
                    thread::spawn(move || watch(&mut stderr, chosen.as_deref(), &input, &connected))
                };
                let mut buffer = [0_u8; 3200];
                let (mut carry, mut pcm) = (None, Vec::new());
                loop {
                    match stdout.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            decode(&buffer[..read], &mut carry, &mut pcm);
                            input.push(&pcm);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let stderr = errors.join().unwrap_or_default();
                if !stopping.load(Ordering::Acquire) {
                    input.fail(server_error(&stderr).0);
                }
            })
            .map_err(|e| CaptureError(e.to_string()))?
    };
    let recorder = Recorder {
        child,
        stopping,
        reader: Some(reader),
    };
    // A bad device or unreachable server exits at once; otherwise wait until the stream
    // is connected, and only to the device chosen.
    let deadline = Instant::now() + SERVER_TIMEOUT;
    loop {
        if let Some(failure) = input.failure() {
            return Err(CaptureError(if failure == MOVED {
                "That audio device isn't available anymore. Choose another.".into()
            } else {
                failure
            }));
        }
        if Instant::now() >= deadline {
            return Err(CaptureError(UNRESPONSIVE.into()));
        }
        match ready.recv_timeout(Duration::from_millis(20)) {
            // A wrong device fails the source before it is reported, so check again.
            Ok(_) if input.failure().is_none() => return Ok(Box::new(recorder)),
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            // parec is exiting; its reason follows.
            Err(mpsc::RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    // Failure modes: monitors listed as microphones; sinks without a monitor field
    // become unusable; nested properties overwrite names; an empty choice records a
    // fixed device instead of the server's default; odd reads split samples.
    use super::*;

    const SINKS: &str = "Sink #66\n\tState: RUNNING\n\tName: alsa_output.usb\n\tDescription: USB Audio Analog Stereo\n\tMonitor Source: alsa_output.usb.monitor\n\tProperties:\n\t\tdevice.description = \"Ignored\"\n\t\tName: nested\n\nSink #67\n\tName: hdmi\n\tDescription: \n";
    const SOURCES: &str = "Source #65\n\tName: alsa_output.usb.monitor\n\tDescription: Monitor of USB Audio\n\tMonitor of Sink: alsa_output.usb\nSource #68\n\tName: alsa_input.usb\n\tDescription: USB Audio Mic\n\tMonitor of Sink: n/a\n";

    #[test]
    fn lists_outputs_by_monitor_and_only_real_microphones() {
        let devices = parse(SINKS, SOURCES);
        assert_eq!(
            devices.outputs,
            [
                Device {
                    id: "alsa_output.usb.monitor".into(),
                    name: "USB Audio Analog Stereo".into()
                },
                Device {
                    id: "hdmi.monitor".into(),
                    name: "hdmi".into()
                },
            ]
        );
        assert_eq!(
            devices.microphones,
            [Device {
                id: "alsa_input.usb".into(),
                name: "USB Audio Mic".into()
            }]
        );
    }

    #[test]
    fn records_the_chosen_device_or_the_servers_default() {
        let output = arguments(Kind::Output, "");
        assert_eq!(output[0], "--device=@DEFAULT_MONITOR@");
        assert!(output.contains(&"--rate=16000".to_owned()));
        assert!(output.contains(&"--channels=1".to_owned()));
        assert_eq!(
            arguments(Kind::Microphone, "")[0],
            "--device=@DEFAULT_SOURCE@"
        );
        assert_eq!(arguments(Kind::Microphone, "mic")[0], "--device=mic");
    }

    #[test]
    fn decodes_samples_across_odd_reads() {
        let bytes: Vec<u8> = [1_i16, -2, 300]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (mut carry, mut pcm, mut all) = (None, Vec::new(), Vec::new());
        for part in [&bytes[..1], &bytes[1..4], &bytes[4..]] {
            decode(part, &mut carry, &mut pcm);
            all.extend_from_slice(&pcm);
        }
        assert_eq!(all, [1, -2, 300]);
        assert_eq!(carry, None);
    }

    #[test]
    fn explains_server_failures() {
        assert!(
            server_error("Connection failure: Connection refused")
                .0
                .contains("sound server")
        );
        assert!(
            server_error("Stream error: No such entity\n")
                .0
                .contains("isn't available")
        );
        assert!(
            server_error("Stream error: Entity killed")
                .0
                .contains("isn't available")
        );
        assert!(server_error("").0.contains("stopped"));
    }

    /// Real `parec --verbose` output (PipeWire 1.6, `LC_ALL=C`) while its stream was
    /// moved to another device and then to the fallback, as PulseAudio's rescue does.
    const MOVED_LOG: &[u8] = b"Opening a recording stream with sample specification 's16le 1ch 16000Hz' and channel map 'mono'.\nConnection established.\x1b[K\nStream successfully created.\nBuffer metrics: maxlength=4194304, fragsize=3200\nUsing sample spec 's16le 1ch 16000Hz', channel map 'mono'.\nConnected to device WispXProbeA.monitor (index: 20847, suspended: no).\nTime: 0.043 sec; Latency: 42713 usec.        \rTime: 1.024 sec; Latency: 24079 usec.        \rStream moved to device WispXProbeB.monitor (20854, not suspended).\x1b[K\nTime: 1.536 sec; Latency: 36073 usec.        \rStream moved to device alsa_output.usb.analog-stereo.monitor (66, not suspended).\x1b[K\nTime: 2.971 sec; Latency: 70702 usec.        \rGot signal, exiting.\n";

    fn watched(log: &[u8], chosen: Option<&str>) -> (Option<String>, Option<String>, String) {
        let (_capture, feeder) = crate::audio::capture::Capture::fed();
        let input = feeder.input();
        let (connected, ready) = mpsc::sync_channel(1);
        let last = watch(log, chosen, &input, &connected);
        (ready.try_recv().ok(), input.failure(), last)
    }

    #[test]
    fn a_moved_stream_fails_the_source_instead_of_recording_elsewhere() {
        let (connected, failure, last) = watched(MOVED_LOG, Some("WispXProbeA.monitor"));
        assert_eq!(connected.as_deref(), Some("WispXProbeA.monitor"));
        assert_eq!(failure.as_deref(), Some(MOVED));
        assert_eq!(last, "Got signal, exiting.");
        // The default device is resolved by the server, but a move still ends it.
        assert_eq!(watched(MOVED_LOG, None).1.as_deref(), Some(MOVED));
        // Without a move the recording carries on.
        let steady = MOVED_LOG
            .split(|&b| b == b'\n')
            .take(6)
            .collect::<Vec<_>>()
            .join(&b'\n');
        let (connected, failure, _) = watched(&steady, Some("WispXProbeA.monitor"));
        assert!(connected.is_some() && failure.is_none());
        // Connecting anywhere but the chosen device fails at once.
        assert_eq!(
            watched(&steady, Some("other.monitor")).1.as_deref(),
            Some(MOVED)
        );
        // Endless status lines without a newline are read in bounded pieces.
        let noisy = b"Time: 1.0 sec; Latency: 1 usec.        \r".repeat(10_000);
        assert_eq!(watched(&noisy, None), (None, None, String::new()));
        assert_eq!(
            report("Stream error: Entity killed"),
            Some(Report::Other("Stream error: Entity killed"))
        );
    }

    fn require_private_server() {
        let server = std::env::var("WISPX_TEST_PULSE_SERVER")
            .expect("Set WISPX_TEST_PULSE_SERVER and PULSE_SERVER to a private test sound server");
        assert!(
            !server.trim().is_empty(),
            "The private server must be explicit"
        );
        assert_eq!(
            std::env::var("PULSE_SERVER").ok().as_deref(),
            Some(server.as_str()),
            "PULSE_SERVER must match WISPX_TEST_PULSE_SERVER; never use the desktop server"
        );
    }

    /// Moves a live recording to another device, as PulseAudio's rescue module does when
    /// the chosen one goes away; nothing reaches a speaker:
    ///
    ///     cargo test --lib audio::pulse -- --ignored --nocapture
    #[test]
    #[ignore = "requires an explicit private sound server; see WISPX_TEST_PULSE_SERVER"]
    fn a_live_move_ends_the_capture() {
        require_private_server();
        use crate::audio::capture::Capture;
        let load = |name: &str| {
            pactl(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=device.description={name}"),
            ])
            .expect("null sink")
            .trim()
            .to_owned()
        };
        let modules = [load("WispXMoveA"), load("WispXMoveB")];
        let tag = format!("wispx.test={}", std::process::id());
        let index = || {
            let list = pactl(&["list", "source-outputs"]).expect("list");
            let mut index = None;
            for line in list.lines() {
                if let Some(id) = line.strip_prefix("Source Output #") {
                    index = Some(id.trim().to_owned());
                }
                if line.contains(&tag.replace('=', " = \"")) {
                    return index;
                }
            }
            None
        };
        // Plain PulseAudio can always move a stream; PipeWire only without WispX's
        // properties, so leave them out to exercise the move itself.
        let mut args: Vec<String> = arguments(Kind::Output, "WispXMoveA.monitor")
            .into_iter()
            .filter(|a| !a.starts_with("--property=node."))
            .collect();
        args.push(format!("--property={tag}"));
        let (mut capture, feeder) = Capture::fed();
        let backend = record(&args, Some("WispXMoveA.monitor"), feeder.input()).expect("record");
        let stream = index().expect("our stream is listed");
        pactl(&["move-source-output", &stream, "WispXMoveB.monitor"]).expect("move");
        let deadline = Instant::now() + Duration::from_secs(5);
        let failure = loop {
            match capture.pull(Instant::now(), false) {
                Err(error) => break error.0,
                Ok(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(_) => panic!("a moved recording must end"),
            }
        };
        println!("after the move: {failure}");
        assert!(failure.contains(MOVED));
        drop(backend);
        // With WispX's real arguments, the server refuses the move or the capture ends.
        let (mut capture, feeder) = Capture::fed();
        let mut args = arguments(Kind::Output, "WispXMoveA.monitor");
        args.push(format!("--property={tag}"));
        let backend = record(&args, Some("WispXMoveA.monitor"), feeder.input()).expect("record");
        let stream = index().expect("our stream is listed");
        let moved = pactl(&["move-source-output", &stream, "WispXMoveB.monitor"]).is_ok();
        thread::sleep(Duration::from_millis(500));
        let result = capture
            .pull(Instant::now(), false)
            .and_then(|_| capture.pull(Instant::now(), false));
        println!(
            "real arguments: move accepted {moved}, capture {:?}",
            result.as_ref().map(Vec::len)
        );
        assert!(!moved || result.is_err());
        drop(backend);
        for module in modules {
            let _ = pactl(&["unload-module", &module]);
        }
    }

    /// Records a tone played into a throwaway null sink, then unloads the sink
    /// mid-recording. Nothing reaches a speaker:
    ///
    ///     cargo test --lib audio::pulse -- --ignored --nocapture
    #[test]
    #[ignore = "requires an explicit private sound server; see WISPX_TEST_PULSE_SERVER"]
    fn records_a_real_monitor_and_reports_its_removal() {
        require_private_server();
        use crate::audio::capture::Capture;
        use std::io::Write;
        let module = pactl(&[
            "load-module",
            "module-null-sink",
            "sink_name=WispXListenTest",
            "sink_properties=device.description=WispXListenTest",
        ])
        .expect("null sink");
        let module = module.trim().to_owned();
        let unload = || drop(pactl(&["unload-module", &module]));
        let devices = devices().expect("devices");
        let output = devices
            .outputs
            .iter()
            .find(|d| d.name == "WispXListenTest")
            .expect("the null sink is listed")
            .id
            .clone();
        assert!(!devices.microphones.iter().any(|d| d.id == output));
        let mut player = Command::new("pacat")
            .args([
                "--playback",
                "--device=WispXListenTest",
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
            ])
            .stdin(Stdio::piped())
            .spawn()
            .expect("pacat");
        let mut stdin = player.stdin.take().unwrap();
        let tone: Vec<u8> = (0..16_000 * 4)
            .flat_map(|i| (((i as f32 * 0.2).sin() * 12_000.0) as i16).to_le_bytes())
            .collect();
        let feeder = thread::spawn(move || drop(stdin.write_all(&tone)));
        let mut capture = Capture::start(&output, None).expect("capture");
        thread::sleep(Duration::from_millis(1500));
        let pcm = capture.pull(Instant::now(), false).expect("pull");
        let loud = pcm.iter().filter(|s| s.unsigned_abs() > 4_000).count();
        println!("captured {} samples, {loud} loud", pcm.len());
        assert!(pcm.len() > 12_000, "{}", pcm.len());
        assert!(loud > pcm.len() / 10, "the tone came through");
        let _ = player.kill();
        let _ = player.wait();
        let _ = feeder.join();
        unload();
        let deadline = Instant::now() + Duration::from_secs(5);
        let failure = loop {
            match capture.pull(Instant::now(), false) {
                Err(error) => break error.0,
                Ok(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
                Ok(_) => panic!("removing the device must end the capture"),
            }
        };
        println!("after removal: {failure}");
        assert!(failure.starts_with("Computer audio stopped"));
        assert!(Capture::start("WispXListenTest.monitor", None).is_err());
    }
}
