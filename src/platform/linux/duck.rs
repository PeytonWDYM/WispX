//! Playback volume through `pactl`, which talks to PulseAudio or PipeWire's
//! pipewire-pulse. Its JSON output is stable across locales and versions, and a
//! missing server or tool only disables lowering.
//!
//! Under PipeWire, streams are lowered through their software volume with `pw-cli`
//! rather than their own volume. WirePlumber remembers each app's own volume and starts
//! its next stream there, so an app that ended while lowered would otherwise stay
//! quiet for good. The software volume isn't remembered, and any later change to the
//! stream's own volume, by the user or the app, replaces it. Plain PulseAudio has no
//! such level, so there the stream's own volume is lowered.
use crate::audio::duck::{Mixer, MixerError, Stream};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// PulseAudio's 100% (`PA_VOLUME_NORM`).
const NORM: f64 = 65536.0;
/// Every request is bounded so a stuck sound server can't hold the worker.
const TIMEOUT: Duration = Duration::from_secs(2);

type Run = fn(&str, &[&str]) -> Result<Vec<u8>, MixerError>;

pub struct Pactl {
    own_pid: u32,
    /// The sound server run that keys and caches belong to.
    server: Option<Server>,
    /// Each client's process, cached because client indexes are never reused while
    /// the server runs.
    clients: HashMap<u32, Option<u32>>,
    /// The streams from the latest listing: their pactl index and PipeWire node.
    listed: HashMap<String, (u32, Option<u32>)>,
    /// Limits listing to one output; only the live test sets it.
    sink: Option<u32>,
    /// Uses PipeWire's software volume where available; the live test turns it off
    /// to exercise the PulseAudio way.
    software: bool,
    /// Runs pactl or pw-cli; tests substitute a fake sound server.
    run: Run,
}

struct Server {
    /// Chosen at random each time the server starts.
    cookie: String,
    software: bool,
}

impl Pactl {
    pub fn new() -> Self {
        Self {
            own_pid: std::process::id(),
            server: None,
            clients: HashMap::new(),
            listed: HashMap::new(),
            sink: None,
            software: true,
            run: |program, args| tool(program, args),
        }
    }

    /// The running server's cookie. A different one means the server restarted and
    /// numbers its streams and clients afresh.
    fn identify(&mut self) -> Result<String, MixerError> {
        #[derive(Deserialize)]
        struct Info {
            cookie: String,
            #[serde(default)]
            server_name: String,
        }
        let info: Info = serde_json::from_slice(&(self.run)("pactl", &["-f", "json", "info"])?)
            .map_err(|error| MixerError::Failed(format!("unexpected pactl output: {error}")))?;
        if self.server.as_ref().is_none_or(|s| s.cookie != info.cookie) {
            self.clients.clear();
            let software = self.software
                && info.server_name.contains("PipeWire")
                && (self.run)("pw-cli", &["--version"]).is_ok();
            self.server = Some(Server {
                cookie: info.cookie.clone(),
                software,
            });
        }
        Ok(info.cookie)
    }
}

impl Default for Pactl {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer for Pactl {
    fn streams(&mut self) -> Result<Vec<Stream>, MixerError> {
        self.listed.clear();
        let cookie = self.identify()?;
        let inputs = parse_inputs(&(self.run)(
            "pactl",
            &["-f", "json", "list", "sink-inputs"],
        )?)?;
        // Streams from PipeWire's ALSA plugin, like WispX's own cues, name their
        // process only on the client, so new clients are looked up.
        let unknown = |clients: &HashMap<u32, Option<u32>>| {
            inputs
                .iter()
                .filter_map(|input| input.client)
                .filter(|client| !clients.contains_key(client))
                .collect::<Vec<_>>()
        };
        if !unknown(&self.clients).is_empty() {
            self.clients =
                parse_clients(&(self.run)("pactl", &["-f", "json", "list", "clients"])?)?;
            // A client that ended meanwhile isn't asked about again.
            for client in unknown(&self.clients) {
                self.clients.insert(client, None);
            }
        }
        // A restart mid-listing would mix two runs' numbering.
        if self.identify()? != cookie {
            self.clients.clear();
            return Err(MixerError::Failed("the sound server restarted".into()));
        }
        let own = Some(self.own_pid);
        let mut streams = Vec::new();
        for input in inputs {
            let other = input.pid != own
                && input
                    .client
                    .and_then(|c| self.clients.get(&c).copied().flatten())
                    != own;
            if !other || self.sink.is_some_and(|sink| input.sink != Some(sink)) {
                continue;
            }
            // Passthrough streams have no volume to lower.
            let Some(volume) = input.volume else { continue };
            // pactl indexes are never reused while the server runs, nor is PipeWire's serial.
            let key = format!("{cookie}/{}#{}", input.index, input.serial);
            self.listed.insert(key.clone(), (input.index, input.node));
            streams.push(Stream {
                key,
                group: input.group,
                volume,
            });
        }
        Ok(streams)
    }

    fn set_volume(&mut self, key: &str, volume: &[f64]) -> Result<(), MixerError> {
        let &(index, node) = self
            .listed
            .get(key)
            .ok_or_else(|| MixerError::Ended("the stream ended".into()))?;
        if self.temporary() {
            let node =
                node.ok_or_else(|| MixerError::Failed("a stream has no PipeWire node".into()))?;
            (self.run)(
                "pw-cli",
                &["set-param", &node.to_string(), "Props", &software(volume)],
            )?;
        } else {
            let index = index.to_string();
            let volume: Vec<String> = raw(volume).collect();
            let mut args = vec!["set-sink-input-volume", &index];
            args.extend(volume.iter().map(String::as_str));
            (self.run)("pactl", &args)?;
        }
        Ok(())
    }

    fn temporary(&self) -> bool {
        self.server.as_ref().is_some_and(|s| s.software)
    }
}

#[derive(Deserialize)]
struct Entry {
    index: u32,
    #[serde(default)]
    sink: Option<u32>,
    #[serde(default)]
    client: Value,
    #[serde(default)]
    volume: Map<String, Value>,
    #[serde(default)]
    properties: HashMap<String, Value>,
}

impl Entry {
    fn property(&self, name: &str) -> Option<&str> {
        self.properties.get(name).and_then(Value::as_str)
    }
    /// The owning process. PipeWire's socket credentials are verified by the
    /// kernel; a sandboxed app's self-reported ID is from its own namespace.
    fn pid(&self) -> Option<u32> {
        let sandboxed = self.property("pipewire.access.portal.app_id").is_some()
            || self.property("pipewire.client.access") == Some("flatpak");
        self.property("pipewire.sec.pid")
            .or_else(|| {
                self.property("application.process.id")
                    .filter(|_| !sandboxed)
            })
            .and_then(|pid| pid.parse().ok())
    }
}

struct Input {
    index: u32,
    serial: String,
    /// The PipeWire node, which `pw-cli` addresses.
    node: Option<u32>,
    sink: Option<u32>,
    client: Option<u32>,
    pid: Option<u32>,
    group: Option<String>,
    /// Channel volumes in channel-map order; `None` for passthrough streams.
    volume: Option<Vec<f64>>,
}

fn entries(json: &[u8]) -> Result<Vec<Entry>, MixerError> {
    serde_json::from_slice(json)
        .map_err(|error| MixerError::Failed(format!("unexpected pactl output: {error}")))
}

fn parse_inputs(json: &[u8]) -> Result<Vec<Input>, MixerError> {
    Ok(entries(json)?
        .into_iter()
        .map(|entry| Input {
            index: entry.index,
            serial: entry.property("object.serial").unwrap_or("").to_owned(),
            node: entry.property("object.id").and_then(|id| id.parse().ok()),
            sink: entry.sink,
            client: match &entry.client {
                Value::String(client) => client.parse().ok(),
                other => other.as_u64().and_then(|c| u32::try_from(c).ok()),
            },
            pid: entry.pid(),
            group: entry
                .property("module-stream-restore.id")
                .or_else(|| entry.property("application.name"))
                .map(str::to_owned),
            volume: entry
                .volume
                .values()
                .map(|channel| Some(channel.get("value")?.as_u64()? as f64 / NORM))
                .collect::<Option<Vec<_>>>()
                .filter(|volume| !volume.is_empty()),
        })
        .collect())
}

fn parse_clients(json: &[u8]) -> Result<HashMap<u32, Option<u32>>, MixerError> {
    Ok(entries(json)?
        .into_iter()
        .map(|entry| (entry.index, entry.pid()))
        .collect())
}

/// Raw per-channel volumes, which pactl reads as exact `pa_volume_t` values.
fn raw(volume: &[f64]) -> impl Iterator<Item = String> + '_ {
    volume
        .iter()
        .map(|v| ((v * NORM).round().clamp(0.0, f64::from(u32::MAX)) as u32).to_string())
}

/// A PipeWire software volume per channel. It takes the place of the stream's own
/// channel volume while set, and is linear where pactl's volume is cubic.
fn software(volume: &[f64]) -> String {
    let channels: Vec<String> = volume
        .iter()
        .map(|v| v.powi(3).clamp(0.0, 10.0).to_string())
        .collect();
    format!("{{ softVolumes: [ {} ] }}", channels.join(", "))
}

/// Runs pactl or pw-cli without a shell, killing it if it outlives [`TIMEOUT`].
fn tool(program: &str, args: &[&str]) -> Result<Vec<u8>, MixerError> {
    let mut child = Command::new(program)
        .args(args)
        // Some locales print JSON numbers with a decimal comma; C also keeps errors in English.
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            MixerError::Unavailable(match error.kind() {
                std::io::ErrorKind::NotFound if program == "pactl" => {
                    "pactl isn't installed, so other audio can't be lowered. Install \
                     pulseaudio-utils (Debian, Ubuntu, Fedora) or libpulse (Arch)"
                        .into()
                }
                _ => format!("couldn't run {program}: {error}"),
            })
        })?;
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    // Reading while waiting keeps a long listing from filling the pipe and stalling it.
    let output = thread::spawn(move || {
        let (mut out, mut err) = (Vec::new(), String::new());
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_end(&mut out);
        }
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_string(&mut err);
        }
        (out, err)
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                break Err(MixerError::Failed(format!(
                    "{program} didn't answer in time"
                )));
            }
            Err(error) => break Err(MixerError::Failed(format!("{program}: {error}"))),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let (out, err) = output.join().unwrap_or_default();
    let status = status?;
    // pw-cli reports a failed command on stderr but still exits successfully.
    let error = err
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Error:"));
    if status.success() && error.is_none() {
        return Ok(out);
    }
    let message = error.or_else(|| err.lines().next()).unwrap_or("").trim();
    Err(if message.starts_with("Connection failure") {
        MixerError::Unavailable(format!(
            "no PulseAudio or pipewire-pulse server answered ({message})"
        ))
    } else if message.contains("No such entity") || message.contains("unknown global") {
        MixerError::Ended(format!("{program}: {message}"))
    } else {
        MixerError::Failed(format!("{program}: {message}"))
    })
}

#[cfg(test)]
mod tests {
    // Failure modes: WispX's own cue stream is lowered, or a sandboxed app that
    // happens to share its PID is skipped; channels are reordered or rescaled,
    // shifting balance; a reused index, or one from before the sound server
    // restarted, is mistaken for a lowered stream; a passthrough stream with no volume
    // fails the whole listing; PipeWire's remembered per-app volume is changed, so an
    // app that ends while lowered comes back quiet; a failed pw-cli request passes as
    // done.
    use super::*;
    use crate::audio::duck::{Ducker, FACTOR};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Fake {
        cookie: &'static str,
        pipewire: bool,
        pw_cli: bool,
        calls: Vec<String>,
    }

    thread_local! {
        static FAKE: RefCell<Fake> = const { RefCell::new(Fake {
            cookie: "a:1",
            pipewire: true,
            pw_cli: true,
            calls: Vec::new(),
        }) };
    }

    fn fake<T>(f: impl FnOnce(&mut Fake) -> T) -> T {
        FAKE.with(|fake| f(&mut fake.borrow_mut()))
    }

    fn input(index: u32, client: u32, volume: &[(&str, u64)], extra: &str) -> String {
        let volume = volume
            .iter()
            .map(|(channel, value)| {
                format!(r#""{channel}":{{"value":{value},"value_percent":"x","db":"x"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"index":{index},"sink":7,"client":"{client}","mute":false,"volume":{{{volume}}},
               "balance":0.00,"properties":{{"application.name":"App{index}",
               "object.serial":"{}","object.id":"{}"{extra}}}}}"#,
            index + 1000,
            index + 200
        )
    }

    fn server(program: &str, args: &[&str]) -> Result<Vec<u8>, MixerError> {
        let own = std::process::id();
        fake(|fake| fake.calls.push(format!("{program} {}", args.join(" "))));
        Ok(match (program, args) {
            ("pactl", ["-f", "json", "info"]) => {
                let (cookie, pipewire) = fake(|fake| (fake.cookie, fake.pipewire));
                let name = match pipewire {
                    true => "PulseAudio (on PipeWire 1.6.11)",
                    false => "pulseaudio",
                };
                format!(r#"{{"cookie":"{cookie}","server_name":"{name}"}}"#)
            }
            ("pactl", ["-f", "json", "list", "sink-inputs"]) => format!(
                "[{},{},{},{},{},{}]",
                // WispX's cue through PipeWire's ALSA plugin: only its client names it.
                input(1, 50, &[("front-left", 65536)], ""),
                // A Pulse client that reports its own process on the stream.
                input(2, 51, &[("mono", 65536)], &format!(r#","application.process.id":"{own}""#)),
                // A Flatpak app whose namespace happens to reuse WispX's PID.
                input(
                    3,
                    52,
                    &[("mono", 32768)],
                    &format!(
                        r#","application.process.id":"{own}","pipewire.access.portal.app_id":"org.example.App""#
                    )
                ),
                input(
                    4,
                    53,
                    &[("front-right", 6554), ("front-left", 65536)],
                    r#","module-stream-restore.id":"sink-input-by-application-name:Music""#
                ),
                r#"{"index":5,"sink":7,"client":"53","volume":{},"properties":{}}"#,
                // Its client ended between the two listings.
                input(6, 99, &[("mono", 65536)], ""),
            ),
            ("pactl", ["-f", "json", "list", "clients"]) => format!(
                r#"[{{"index":50,"properties":{{"pipewire.sec.pid":"{own}"}}}},
                    {{"index":51,"properties":{{}}}},
                    {{"index":52,"properties":{{"pipewire.sec.pid":"1","application.process.id":"{own}",
                      "pipewire.client.access":"flatpak"}}}},
                    {{"index":53,"properties":{{"application.process.id":"77"}}}}]"#
            ),
            ("pactl", ["set-sink-input-volume", ..]) => String::new(),
            ("pw-cli", ["--version"]) if fake(|fake| fake.pw_cli) => String::new(),
            ("pw-cli", ["--version"]) => {
                return Err(MixerError::Unavailable("couldn't run pw-cli".into()));
            }
            ("pw-cli", ["set-param", ..]) => String::new(),
            other => panic!("unexpected {other:?}"),
        }
        .into_bytes())
    }

    fn mixer() -> Pactl {
        Pactl {
            run: server,
            ..Pactl::new()
        }
    }

    fn calls() -> Vec<String> {
        fake(|fake| std::mem::take(&mut fake.calls))
    }

    #[test]
    fn lists_other_processes_streams_in_channel_order() {
        let mut mixer = mixer();
        let streams = mixer.streams().unwrap();
        let keys: Vec<_> = streams.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["a:1/3#1003", "a:1/4#1004", "a:1/6#1006"]);
        assert_eq!(streams[0].volume, [0.5]);
        assert_eq!(streams[0].group.as_deref(), Some("App3"));
        assert_eq!(streams[1].volume, [6554.0 / NORM, 1.0]);
        assert_eq!(
            streams[1].group.as_deref(),
            Some("sink-input-by-application-name:Music")
        );
        // Known clients, including the one that ended, aren't looked up again.
        calls();
        assert_eq!(mixer.streams().unwrap().len(), 3);
        assert!(!calls().iter().any(|call| call.contains("clients")));
        mixer.sink = Some(8);
        assert!(mixer.streams().unwrap().is_empty());
        mixer.run = |_, _| Ok(b"Failure".to_vec());
        assert!(matches!(mixer.streams(), Err(MixerError::Failed(_))));
    }

    #[test]
    fn pipewire_streams_get_a_software_volume_it_doesnt_remember() {
        let mut mixer = mixer();
        let streams = mixer.streams().unwrap();
        assert!(mixer.temporary());
        calls();
        mixer.set_volume(&streams[1].key, &[0.2, 0.1]).unwrap();
        // Node 204 is index 4's; the level is linear and each channel keeps its share.
        assert_eq!(
            calls(),
            [
                "pw-cli set-param 204 Props { softVolumes: [ 0.008000000000000002, 0.0010000000000000002 ] }"
            ]
        );
        // Only streams from the latest listing can be set.
        assert!(matches!(
            mixer.set_volume("a:1/9#1009", &[0.2]),
            Err(MixerError::Ended(_))
        ));
        assert!(calls().is_empty());
    }

    #[test]
    fn plain_pulseaudio_or_no_pw_cli_sets_the_streams_own_volume() {
        for (pipewire, pw_cli) in [(false, true), (true, false)] {
            fake(|fake| (fake.pipewire, fake.pw_cli) = (pipewire, pw_cli));
            let mut mixer = mixer();
            let streams = mixer.streams().unwrap();
            assert!(!mixer.temporary());
            calls();
            mixer.set_volume(&streams[1].key, &[0.2, 0.1]).unwrap();
            assert_eq!(calls(), ["pactl set-sink-input-volume 4 13107 6554"]);
        }
    }

    #[test]
    fn a_restarted_sound_server_shares_no_streams_or_clients_with_the_last() {
        let mut mixer = mixer();
        let before = mixer.streams().unwrap();
        fake(|fake| fake.cookie = "b:2");
        calls();
        let after = mixer.streams().unwrap();
        assert!(
            before
                .iter()
                .all(|old| after.iter().all(|new| new.key != old.key))
        );
        // The new run's clients are looked up afresh.
        assert!(calls().iter().any(|call| call.contains("clients")));
        assert!(matches!(
            mixer.set_volume(&before[0].key, &[0.1]),
            Err(MixerError::Ended(_))
        ));

        // Nothing is listed from a server that restarted mid-listing.
        mixer.run = |program, args| {
            let result = server(program, args);
            if args.contains(&"sink-inputs") {
                fake(|fake| fake.cookie = "c:3");
            }
            result
        };
        assert!(matches!(mixer.streams(), Err(MixerError::Failed(_))));
        assert!(matches!(
            mixer.set_volume(&after[0].key, &[0.1]),
            Err(MixerError::Ended(_))
        ));
    }

    #[test]
    fn ducks_through_the_fake_without_compounding() {
        let mut mixer = mixer();
        let mut ducker = Ducker::default();
        ducker.lower(&mut mixer).unwrap();
        // The listed volume stays the stream's own, so polling never lowers it twice.
        ducker.lower(&mut mixer).unwrap();
        ducker.restore(&mut mixer).unwrap();
        let sets: Vec<_> = calls()
            .into_iter()
            .filter(|call| call.contains("set-param"))
            .collect();
        assert_eq!(sets.len(), 6, "{sets:?}");
        assert!(sets[..3].iter().all(|set| !set.contains("0.125 ")));
        // Restoring hands each stream back its own volume, cubed: 0.5 for node 203.
        assert!(sets[3].starts_with("pw-cli set-param 203 Props { softVolumes: [ 0.125 ]"));
    }

    #[test]
    fn tool_errors_are_classified() {
        let ended = tool(
            "sh",
            &["-c", r#"echo 'Error: "set-param: unknown global 9"' >&2"#],
        );
        assert!(matches!(ended, Err(MixerError::Ended(_))), "{ended:?}");
        let failed = tool("sh", &["-c", "echo ok; echo 'Error: busy' >&2"]);
        assert!(matches!(failed, Err(MixerError::Failed(_))), "{failed:?}");
        let ended = tool("sh", &["-c", "echo 'Failure: No such entity' >&2; exit 1"]);
        assert!(matches!(ended, Err(MixerError::Ended(_))), "{ended:?}");
        assert_eq!(tool("sh", &["-c", "echo ok"]).unwrap(), b"ok\n");
    }

    #[test]
    fn raw_volumes_round_trip_exactly() {
        let volume = [74711.0 / NORM, 0.0, 1.5];
        assert_eq!(raw(&volume).collect::<Vec<_>>(), ["74711", "0", "98304"]);
        let lowered: Vec<f64> = volume.iter().map(|v| v * FACTOR).collect();
        assert_eq!(raw(&lowered).collect::<Vec<_>>(), ["14942", "0", "19661"]);
    }

    /// Exercises a real sound server, touching only its own streams on a private null
    /// sink that it removes afterwards. Nothing reaches a speaker:
    ///
    ///     cargo test --lib platform::linux::duck -- --ignored --nocapture --test-threads=1
    ///
    /// `WISPX_TEST_PULSE=1` exercises the PulseAudio way on PipeWire too.
    #[test]
    #[ignore = "needs a running PulseAudio or pipewire-pulse server"]
    fn lowers_and_restores_real_streams() {
        let sink = Sandbox::new();
        let mut mixer = Pactl {
            sink: Some(sink.index),
            software: std::env::var_os("WISPX_TEST_PULSE").is_none(),
            ..Pactl::new()
        };
        mixer.streams().unwrap();
        let software = mixer.temporary();
        println!("temporary software volume: {software}");

        // Uneven balance survives, and A's level is the one its app is remembered at.
        let a = sink.play("WispXLowerTestA", true);
        let key_a = sink.wait(&mut mixer, 1)[0].key.clone();
        pactl(&["set-sink-input-volume", &index(&key_a), "58982", "29491"]);
        let full = sink.level();
        if software {
            // WirePlumber saves the user's volume shortly after it changes.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !sink
                .remembered("WispXLowerTestA")
                .is_some_and(|line| line.contains("0.728985"))
            {
                assert!(
                    Instant::now() < deadline,
                    "WirePlumber never saved A's volume"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
        let remembered = sink.remembered("WispXLowerTestA");
        let mut ducker = Ducker::default();
        ducker.lower(&mut mixer).unwrap();
        let lowered = sink.level();
        println!("A at its level {full:?}, lowered {lowered:?}");
        for (low, full) in lowered.iter().zip(&full) {
            assert!(
                (low / full - FACTOR.powi(3)).abs() < 0.002,
                "{lowered:?} {full:?}"
            );
        }
        if software {
            // The volume the user sees and the one the sound server remembers stay put.
            assert_eq!(volume(&mut mixer, &key_a), Some(vec![0.9, 0.45]));
            // Long enough for WirePlumber to have saved any change.
            thread::sleep(Duration::from_secs(2));
            assert_eq!(sink.remembered("WispXLowerTestA"), remembered);
        }

        if software {
            // Lowering again, as a recording that starts while another's restore is
            // still pending does, never compounds: the stream's own volume is the base.
            Ducker::default().lower(&mut mixer).unwrap();
            assert!(close(&sink.level(), &lowered, 0.002));
        }

        // A's app closes while lowered, then starts again after the recording.
        sink.stop(a);
        ducker.restore(&mut mixer).unwrap();
        let a = sink.play("WispXLowerTestA", true);
        let key_a = sink.wait(&mut mixer, 1)[0].key.clone();
        let relaunched = sink.level();
        println!("A relaunched after the recording: {relaunched:?}");
        if software {
            assert_eq!(volume(&mut mixer, &key_a), Some(vec![0.9, 0.45]));
            for (now, full) in relaunched.iter().zip(&full) {
                assert!((now - full).abs() < 0.01, "{relaunched:?} vs {full:?}");
            }
        } else {
            // The known limit of lowering a stream's own volume: the server remembered
            // the lowered level for the app. Put it back by hand to carry on.
            println!("A's remembered volume: {:?}", volume(&mut mixer, &key_a));
            pactl(&["set-sink-input-volume", &index(&key_a), "58982", "29491"]);
        }

        // Rapid recordings: lowered, restored, lowered again, never compounded.
        let mut ducker = Ducker::default();
        ducker.lower(&mut mixer).unwrap();
        ducker.restore(&mut mixer).unwrap();
        assert!(close(&sink.level(), &full, 0.01));
        let mut ducker = Ducker::default();
        ducker.lower(&mut mixer).unwrap();
        ducker.lower(&mut mixer).unwrap();
        assert!(close(&sink.level(), &lowered, 0.002));
        // A silent stream starting mid-recording is lowered on the next pass.
        let b = sink.play("WispXLowerTestB", false);
        let key_b = sink
            .wait(&mut mixer, 2)
            .into_iter()
            .find(|s| s.key != key_a)
            .unwrap()
            .key;
        ducker.lower(&mut mixer).unwrap();
        let node_b = mixer.listed[&key_b].1.unwrap();
        if software {
            assert!(close(&soft_volume(node_b), &[0.008, 0.008], 0.0001));
        } else {
            assert_eq!(volume(&mut mixer, &key_b), Some(vec![0.2, 0.2]));
        }
        // The user turns A up while it's lowered, as a mixer applet would; that
        // choice takes over at once.
        pactl(&["set-sink-input-volume", &index(&key_a), "45875", "45875"]);
        let user = sink.level();
        assert!(
            user.iter().all(|v| (v - 0.343 * TONE).abs() < 0.01),
            "{user:?}"
        );
        ducker.restore(&mut mixer).unwrap();
        assert!(close(&sink.level(), &user, 0.01));
        assert_eq!(volume(&mut mixer, &key_a), Some(vec![0.7, 0.7]));
        assert_eq!(volume(&mut mixer, &key_b), Some(vec![1.0, 1.0]));
        if software {
            assert!(close(&soft_volume(node_b), &[1.0, 1.0], 0.0001));
        }

        // Leave the remembered per-app levels as they were before the test.
        pactl(&["set-sink-input-volume", &index(&key_a), "65536", "65536"]);
        pactl(&["set-sink-input-volume", &index(&key_b), "65536", "65536"]);
        sink.stop(a);
        sink.stop(b);
    }

    /// The tone's RMS level per channel at 100%.
    const TONE: f64 = 0.5 / std::f64::consts::SQRT_2;

    fn close(a: &[f64], b: &[f64], tolerance: f64) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < tolerance)
    }

    fn pactl(args: &[&str]) -> Vec<u8> {
        tool("pactl", args).unwrap()
    }

    fn index(key: &str) -> String {
        key.split_once('/')
            .unwrap()
            .1
            .split_once('#')
            .unwrap()
            .0
            .to_owned()
    }

    /// A PipeWire node's software volume.
    fn soft_volume(node: u32) -> Vec<f64> {
        let dump: Value =
            serde_json::from_slice(&tool("pw-dump", &[&node.to_string()]).unwrap()).unwrap();
        dump[0]["info"]["params"]["Props"][0]["softVolumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect()
    }

    fn volume(mixer: &mut Pactl, key: &str) -> Option<Vec<f64>> {
        let streams = mixer.streams().unwrap();
        let volume = streams.into_iter().find(|s| s.key == key)?.volume;
        Some(volume.iter().map(|v| (v * 100.0).round() / 100.0).collect())
    }

    struct Sandbox {
        module: String,
        name: String,
        index: u32,
        tone: std::path::PathBuf,
        players: RefCell<Vec<std::process::Child>>,
    }

    impl Sandbox {
        fn new() -> Self {
            let name = format!("wispx_lower_test_{}", std::process::id());
            let module = String::from_utf8(pactl(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={name}"),
                "sink_properties=device.description=WispXLowerTest",
            ]))
            .unwrap()
            .trim()
            .to_owned();
            let sinks: Vec<Value> =
                serde_json::from_slice(&pactl(&["-f", "json", "list", "sinks"])).unwrap();
            let index = sinks
                .iter()
                .find(|s| s["name"] == name.as_str())
                .and_then(|s| s["index"].as_u64())
                .unwrap() as u32;
            // A minute of a 440 Hz tone at half scale on both channels.
            let tone = std::env::temp_dir().join(format!("{name}.raw"));
            let samples: Vec<u8> = (0..48_000 * 60)
                .flat_map(|i| {
                    let phase = 2.0 * std::f64::consts::PI * 440.0 * f64::from(i) / 48_000.0;
                    let sample = ((0.5 * phase.sin()) * 32767.0) as i16;
                    [sample, sample]
                })
                .flat_map(i16::to_le_bytes)
                .collect();
            std::fs::write(&tone, samples).unwrap();
            Self {
                module,
                name,
                index,
                tone,
                players: RefCell::default(),
            }
        }

        /// Starts a stream on the private sink, playing the tone or silence.
        fn play(&self, client: &str, tone: bool) -> u32 {
            let source = match tone {
                true => self.tone.clone(),
                false => "/dev/zero".into(),
            };
            let player = Command::new("pacat")
                .args([
                    "--playback",
                    "--channels=2",
                    "--format=s16le",
                    "--rate=48000",
                ])
                .arg(format!("--device={}", self.name))
                .arg(format!("--client-name={client}"))
                .arg(source)
                .spawn()
                .unwrap();
            let id = player.id();
            self.players.borrow_mut().push(player);
            id
        }

        fn stop(&self, id: u32) {
            let mut players = self.players.borrow_mut();
            let position = players.iter().position(|p| p.id() == id).unwrap();
            let mut player = players.remove(position);
            let _ = player.kill();
            let _ = player.wait();
            thread::sleep(Duration::from_millis(300));
        }

        fn wait(&self, mixer: &mut Pactl, count: usize) -> Vec<Stream> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let streams = mixer.streams().unwrap();
                if streams.len() >= count || Instant::now() > deadline {
                    assert_eq!(streams.len(), count, "{streams:?}");
                    // Let the sound server apply the app's remembered volume first.
                    thread::sleep(Duration::from_millis(300));
                    return mixer.streams().unwrap();
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        /// The RMS level of each channel reaching the private sink.
        fn level(&self) -> Vec<f64> {
            thread::sleep(Duration::from_millis(200));
            let mut recorder = Command::new("parec")
                .args([
                    "--format=float32le",
                    "--channels=2",
                    "--rate=48000",
                    "--raw",
                ])
                .arg("--latency-msec=20")
                .arg(format!("--device={}.monitor", self.name))
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            thread::sleep(Duration::from_millis(600));
            let _ = recorder.kill();
            let mut bytes = Vec::new();
            recorder
                .stdout
                .take()
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            let _ = recorder.wait();
            let samples: Vec<f64> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f64::from(f32::from_le_bytes(*b)))
                .collect();
            // The latest part, after any change has settled.
            let samples = &samples[samples.len() / 2 / 2 * 2..];
            (0..2)
                .map(|channel| {
                    let channel: Vec<f64> =
                        samples.iter().skip(channel).step_by(2).copied().collect();
                    (channel.iter().map(|v| v * v).sum::<f64>() / channel.len() as f64).sqrt()
                })
                .collect()
        }

        /// What WirePlumber remembers for an app, if it runs.
        fn remembered(&self, app: &str) -> Option<String> {
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::Path::new(&home).join(".local/state"))
                })?
                .join("wireplumber/stream-properties");
            let key = format!("Output/Audio:application.name:{app}=");
            std::fs::read_to_string(state)
                .ok()?
                .lines()
                .find(|line| line.starts_with(&key))
                .map(str::to_owned)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            for player in self.players.get_mut() {
                let _ = player.kill();
                let _ = player.wait();
            }
            let _ = tool("pactl", &["unload-module", &self.module]);
            let _ = std::fs::remove_file(&self.tone);
        }
    }
}
