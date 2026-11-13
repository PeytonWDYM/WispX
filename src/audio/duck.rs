//! Lowers other applications' playback while dictating, then puts it back.
//!
//! Each playing stream drops to a fifth of its own volume, channel by channel, so
//! balance, mute and silent streams are left as they were. When the recording ends,
//! only streams still at the level WispX set return to their earlier volume; any the
//! user changed meanwhile keep the user's choice.
//!
//! The portable part decides what to change and what to put back. Each platform
//! supplies a [`Mixer`] that lists and sets stream volumes. One worker thread owns the
//! mixer, so a slow sound server never blocks the UI or the hotkey, and each
//! recording's [`Guard`] releases only the changes made for that recording. A stream
//! is only ever set right after a listing shows it is the same stream at the level
//! WispX left it; anything that can't be confirmed that way is kept and retried.
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

/// Lowered playback keeps this share of each channel's volume.
pub const FACTOR: f64 = 0.2;
/// Sound servers round the volumes they store; a level this close is still WispX's.
const TOLERANCE: f64 = 0.005;
/// How often a recording looks for playback that started after it.
const POLL: Duration = Duration::from_millis(250);
/// Playback that couldn't be restored is retried after this long, doubling up to
/// [`RETRY_MAX`] while it keeps failing.
const RETRY_MIN: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 1000 });
const RETRY_MAX: Duration = Duration::from_millis(if cfg!(test) { 40 } else { 60_000 });

/// One playback stream (Linux) or audio session (Windows) of another application.
#[derive(Clone, Debug, PartialEq)]
pub struct Stream {
    /// Identifies this stream instance, on this run of the sound server, and is never
    /// reused for a different one.
    pub key: String,
    /// Streams whose volume the sound server remembers together, such as one app's.
    pub group: Option<String>,
    /// The stream's own channel volumes, or one master level, where 1.0 is 100%.
    pub volume: Vec<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum MixerError {
    /// No usable sound server or volume control; playback stays as it is.
    #[error("{0}")]
    Unavailable(String),
    /// One request failed or timed out; it may be retried.
    #[error("{0}")]
    Failed(String),
    /// The stream no longer exists, so there is nothing left to change.
    #[error("{0}")]
    Ended(String),
}

/// Other applications' playback on every output. Implementations leave out WispX's
/// own streams, so its cues stay audible, and never touch capture.
pub trait Mixer {
    fn streams(&mut self) -> Result<Vec<Stream>, MixerError>;
    /// Sets a stream from the latest listing to `volume`.
    fn set_volume(&mut self, key: &str, volume: &[f64]) -> Result<(), MixerError>;
    /// Whether [`Mixer::set_volume`] sets a temporary level instead of the stream's own
    /// volume. Such a level isn't remembered by the sound server, isn't reported in
    /// [`Stream::volume`], and lasts only until the stream ends or its own volume changes.
    fn temporary(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Lowered {
    key: String,
    original: Vec<f64>,
    /// The level WispX set; anything else means the user has taken over.
    expected: Vec<f64>,
}

impl Lowered {
    /// Whether `stream` is this one, still at the level WispX left it.
    fn holds(&self, stream: &Stream, temporary: bool) -> bool {
        // A temporary level leaves the stream's own volume as it was, and goes away by
        // itself as soon as anyone changes that volume.
        let level = if temporary {
            &self.original
        } else {
            &self.expected
        };
        stream.key == self.key && same(&stream.volume, level)
    }
}

fn same(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= TOLERANCE)
}

/// The changes made for one recording.
#[derive(Default)]
pub(crate) struct Ducker {
    lowered: Vec<Lowered>,
    /// Each group's level before and after lowering. A sound server that remembers
    /// per-app volume starts that app's next stream at the lowered level.
    groups: HashMap<String, (Vec<f64>, Vec<f64>)>,
    /// Streams already considered. Ones left alone, such as those the user changed
    /// or silent ones, are never lowered later in the recording.
    seen: HashSet<String>,
    /// The sound server was unreachable, so this recording stops asking.
    unavailable: bool,
    /// Streams in the first listing were all playing before the recording.
    listed: bool,
}

impl Ducker {
    /// Takes over streams an earlier recording couldn't restore yet, so they are
    /// restored with this one's instead of being lowered a second time.
    fn adopting(lowered: Vec<Lowered>) -> Self {
        Self {
            lowered,
            ..Self::default()
        }
    }

    /// Lowers playback this recording hasn't considered yet.
    pub(crate) fn lower(&mut self, mixer: &mut dyn Mixer) -> Result<(), MixerError> {
        let streams = mixer.streams()?;
        let temporary = mixer.temporary();
        // Ended streams have nothing to restore; changed ones now belong to the user.
        self.lowered
            .retain(|l| streams.iter().any(|s| l.holds(s, temporary)));
        // Only a stream started since the last listing can have inherited a lowered level.
        let started_since = std::mem::replace(&mut self.listed, true);
        let mut failure = None;
        for stream in streams {
            if !self.seen.insert(stream.key.clone()) || stream.volume.iter().all(|&v| v <= 0.0) {
                continue;
            }
            // Still lowered for an earlier recording. Its own volume is already at
            // WispX's level, while a temporary level is simply applied again.
            let adopted = self.lowered.iter().any(|l| l.key == stream.key);
            if adopted && !temporary {
                continue;
            }
            if !temporary
                && started_since
                && let Some((original, expected)) =
                    stream.group.as_ref().and_then(|g| self.groups.get(g))
                && same(&stream.volume, expected)
            {
                self.lowered.push(Lowered {
                    key: stream.key,
                    original: original.clone(),
                    expected: stream.volume,
                });
                continue;
            }
            let lowered: Vec<f64> = stream.volume.iter().map(|v| v * FACTOR).collect();
            match mixer.set_volume(&stream.key, &lowered) {
                Ok(()) => {}
                Err(MixerError::Ended(_)) => continue,
                // Kept even after an error: a request that timed out may still have
                // applied, and restoring checks the level first.
                Err(error) => failure = Some(error),
            }
            if adopted {
                continue;
            }
            if !temporary && let Some(group) = stream.group {
                self.groups
                    .insert(group, (stream.volume.clone(), lowered.clone()));
            }
            self.lowered.push(Lowered {
                key: stream.key,
                original: stream.volume,
                expected: lowered,
            });
        }
        failure.map_or(Ok(()), Err)
    }

    /// Restores streams still at the level WispX set. Streams it can't settle, because
    /// the sound server didn't answer or a request failed, stay for a later attempt.
    pub(crate) fn restore(&mut self, mixer: &mut dyn Mixer) -> Result<(), MixerError> {
        if self.lowered.is_empty() {
            return Ok(());
        }
        // Without a fresh listing, a stream's identity and level are unknown, so
        // nothing is set.
        let streams = mixer.streams()?;
        let temporary = mixer.temporary();
        let mut failure = None;
        let mut unconfirmed = false;
        let mut pending = Vec::new();
        for lowered in std::mem::take(&mut self.lowered) {
            // Ended, or changed by the user: nothing of WispX's is left to undo.
            let Some(stream) = streams.iter().find(|s| lowered.holds(s, temporary)) else {
                continue;
            };
            // A temporary level gives way to the stream's own volume as it is now.
            let volume = if temporary {
                &stream.volume
            } else {
                &lowered.original
            };
            match mixer.set_volume(&lowered.key, volume) {
                Ok(()) if temporary => {}
                Ok(()) => {
                    unconfirmed = true;
                    pending.push(lowered);
                }
                Err(MixerError::Ended(_)) => {}
                Err(error) => {
                    failure = Some(error);
                    pending.push(lowered);
                }
            }
        }
        self.lowered = pending;
        // A changed volume shows in the next listing; one still at WispX's level is
        // retried later. A temporary level isn't listed, so its request must suffice.
        if unconfirmed {
            match mixer.streams() {
                Ok(streams) => self
                    .lowered
                    .retain(|l| streams.iter().any(|s| l.holds(s, false))),
                Err(error) => failure = Some(error),
            }
            if failure.is_none() && !self.lowered.is_empty() {
                failure = Some(MixerError::Failed(
                    "some playback didn't return to its earlier volume".into(),
                ));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Records lowered streams so a WispX that crashed or was killed mid-recording
/// restores them when it next starts.
struct Journal(Option<PathBuf>);

impl Journal {
    fn load(&self) -> Vec<Lowered> {
        self.0
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    fn save(&self, lowered: &[Lowered]) -> io::Result<()> {
        let Some(path) = &self.0 else { return Ok(()) };
        if lowered.is_empty() {
            return match fs::remove_file(path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                other => other,
            };
        }
        serde_json::to_vec(&lowered)
            .map_err(io::Error::from)
            .and_then(|bytes| crate::settings::atomic_write(path, &bytes))
    }
}

enum Command {
    Lower(u64),
    Restore(u64),
    Flush(Sender<()>),
}

type Connect = Box<dyn FnMut() -> Result<Box<dyn Mixer>, MixerError>>;

struct Worker {
    connect: Connect,
    mixer: Option<Box<dyn Mixer>>,
    journal: Journal,
    active: Option<(u64, Ducker)>,
    /// Lowered by an earlier recording or WispX process and not restored yet.
    orphaned: Ducker,
    /// When to try restoring `orphaned` again while no recording runs.
    retry: Duration,
    /// What the journal holds, so it's rewritten only after a change, and again
    /// after a write that failed.
    saved: Vec<Lowered>,
    logged: HashSet<String>,
    poll: Duration,
}

impl Worker {
    fn new(connect: Connect, journal: Journal, poll: Duration) -> Self {
        let orphaned = Ducker {
            lowered: journal.load(),
            ..Ducker::default()
        };
        Self {
            connect,
            mixer: None,
            journal,
            active: None,
            saved: orphaned.lowered.clone(),
            orphaned,
            retry: RETRY_MIN,
            logged: HashSet::new(),
            poll,
        }
    }

    fn serve(mut self, commands: Receiver<Command>) {
        self.recover();
        loop {
            let wait = if self.active.is_some() {
                Some(self.poll)
            } else if !self.orphaned.lowered.is_empty() {
                Some(self.retry)
            } else {
                None
            };
            let command = match wait {
                Some(wait) => match commands.recv_timeout(wait) {
                    Ok(command) => command,
                    Err(RecvTimeoutError::Timeout) => {
                        match self.active {
                            Some(_) => self.lower(),
                            None => self.recover(),
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match commands.recv() {
                    Ok(command) => command,
                    Err(_) => break,
                },
            };
            match command {
                Command::Lower(id) => {
                    self.release();
                    let adopted = std::mem::take(&mut self.orphaned.lowered);
                    self.active = Some((id, Ducker::adopting(adopted)));
                    self.lower();
                }
                // An earlier recording's late release must not undo a newer one.
                Command::Restore(id) if self.active.as_ref().is_some_and(|(a, _)| *a == id) => {
                    self.release()
                }
                Command::Restore(_) => {}
                Command::Flush(done) => {
                    // One more attempt before exit; the journal keeps what's left.
                    if self.active.is_none() {
                        self.recover();
                    }
                    self.persist();
                    let _ = done.send(());
                }
            }
        }
        self.release();
        self.recover();
    }

    fn mixer(&mut self) -> Option<&mut (dyn Mixer + 'static)> {
        if self.mixer.is_none() {
            match (self.connect)() {
                Ok(mixer) => self.mixer = Some(mixer),
                Err(error) => self.log(&error),
            }
        }
        self.mixer.as_deref_mut()
    }

    /// Each distinct problem is reported once per process.
    fn log(&mut self, error: &MixerError) {
        let message = error.to_string();
        if self.logged.insert(message.clone()) {
            eprintln!("Lower other audio: {message}");
        }
    }

    fn lower(&mut self) {
        let Some((id, mut ducker)) = self.active.take() else {
            return;
        };
        let result = match self.mixer() {
            Some(_) if ducker.unavailable => Ok(()),
            Some(mixer) => ducker.lower(mixer),
            None => Err(MixerError::Unavailable(String::new())),
        };
        ducker.unavailable |= matches!(result, Err(MixerError::Unavailable(_)));
        self.active = Some((id, ducker));
        self.finish(result);
    }

    fn release(&mut self) {
        let Some((_, mut ducker)) = self.active.take() else {
            return;
        };
        let result = match self.mixer() {
            Some(mixer) => ducker.restore(mixer),
            None => Err(MixerError::Unavailable(String::new())),
        };
        if !ducker.lowered.is_empty() {
            // Retried shortly, before the next recording, and after a crash via the journal.
            self.orphaned.lowered.append(&mut ducker.lowered);
            self.retry = RETRY_MIN;
        }
        self.finish(result);
    }

    fn recover(&mut self) {
        if self.orphaned.lowered.is_empty() {
            return;
        }
        let mut orphaned = std::mem::take(&mut self.orphaned);
        let result = match self.mixer() {
            Some(mixer) => orphaned.restore(mixer),
            None => Err(MixerError::Unavailable(String::new())),
        };
        self.retry = match orphaned.lowered.is_empty() {
            true => RETRY_MIN,
            false => (self.retry * 2).min(RETRY_MAX),
        };
        self.orphaned = orphaned;
        self.finish(result);
    }

    fn finish(&mut self, result: Result<(), MixerError>) {
        match &result {
            // Connection failures were already logged by `mixer()`.
            Err(MixerError::Unavailable(message)) if message.is_empty() => {}
            Err(error) => self.log(error),
            Ok(()) => {}
        }
        self.persist();
    }

    fn persist(&mut self) {
        let lowered: Vec<Lowered> = self
            .active
            .iter()
            .flat_map(|(_, d)| &d.lowered)
            .chain(&self.orphaned.lowered)
            .cloned()
            .collect();
        if lowered == self.saved {
            return;
        }
        match self.journal.save(&lowered) {
            Ok(()) => self.saved = lowered,
            Err(error) => self.log(&MixerError::Failed(format!(
                "couldn't record lowered playback for after a crash: {error}"
            ))),
        }
    }
}

/// Releases one recording's changes when dropped.
#[must_use]
pub struct Guard(u64);

impl Drop for Guard {
    fn drop(&mut self) {
        send(Command::Restore(self.0));
    }
}

fn worker() -> Option<&'static Sender<Command>> {
    static WORKER: OnceLock<Option<Sender<Command>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (sender, commands) = mpsc::channel();
            let journal = Journal(
                crate::settings::config_dir()
                    .ok()
                    .map(|dir| dir.join("lowered-audio.json")),
            );
            thread::Builder::new()
                .name("lower-audio".into())
                .spawn(move || Worker::new(Box::new(connect), journal, POLL).serve(commands))
                .map_err(|error| eprintln!("Lower other audio: {error}"))
                .ok()?;
            Some(sender)
        })
        .as_ref()
}

fn send(command: Command) {
    if let Some(sender) = worker() {
        let _ = sender.send(command);
    }
}

/// Starts the worker, which restores anything a crashed WispX left lowered.
pub fn start() {
    worker();
}

/// Lowers other playback until the returned guard is dropped.
pub fn begin() -> Guard {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    send(Command::Lower(id));
    Guard(id)
}

/// Waits, at most `timeout`, for released playback to be restored. Used on exit.
pub fn flush(timeout: Duration) {
    let (done, finished) = mpsc::channel();
    send(Command::Flush(done));
    let _ = finished.recv_timeout(timeout);
}

#[cfg(target_os = "linux")]
fn connect() -> Result<Box<dyn Mixer>, MixerError> {
    Ok(Box::new(crate::platform::linux::duck::Pactl::new()))
}

#[cfg(windows)]
fn connect() -> Result<Box<dyn Mixer>, MixerError> {
    Ok(Box::new(
        crate::platform::windows::duck::CoreAudio::connect()?,
    ))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn connect() -> Result<Box<dyn Mixer>, MixerError> {
    Err(MixerError::Unavailable(
        "not supported on this system".into(),
    ))
}

#[cfg(test)]
mod tests {
    // Failure modes: balance or a silent stream changes; a stream the user turned
    // up or down while lowered is reset on release, or lowered again; playback that
    // starts mid-recording stays loud; a stream that inherits the remembered lowered
    // level is lowered twice and never comes back; an ended stream's level is applied
    // to another; a late release from one recording restores the next; a timed-out
    // or failed request, or a listing that fails, leaves playback stuck or sets a
    // stream unchecked; a recording that starts while a restore is pending lowers
    // playback twice; a crash, or a journal that couldn't be written, leaves playback
    // lowered for good; an unreachable sound server spams the log or blocks recording.
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy)]
    enum Outcome {
        /// Reports failure, though the change applied, like a request that timed out.
        Applied,
        /// Reports failure and changes nothing.
        Refused,
        /// Reports success but changes nothing.
        Ignored,
    }

    #[derive(Default)]
    struct Server {
        streams: Vec<Stream>,
        sets: Vec<(String, Vec<f64>)>,
        /// Rounds stored levels, like PipeWire's float volumes.
        rounding: f64,
        list_error: Option<fn() -> MixerError>,
        set_errors: HashMap<String, Outcome>,
        /// Sets a temporary level, like PipeWire's software volume, instead of the
        /// stream's own volume.
        temporary: bool,
        /// Temporary levels in effect.
        gains: HashMap<String, Vec<f64>>,
        /// Each group's remembered volume, which its next stream starts at.
        remembered: HashMap<String, Vec<f64>>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Server>>);

    impl Fake {
        fn with(streams: &[(&str, Option<&str>, &[f64])]) -> Self {
            let fake = Self::default();
            for &(key, group, volume) in streams {
                fake.add(key, group, volume);
            }
            fake
        }
        fn temporary(streams: &[(&str, Option<&str>, &[f64])]) -> Self {
            let fake = Self::with(streams);
            fake.0.lock().unwrap().temporary = true;
            fake
        }
        fn add(&self, key: &str, group: Option<&str>, volume: &[f64]) {
            self.0.lock().unwrap().streams.push(Stream {
                key: key.into(),
                group: group.map(Into::into),
                volume: volume.to_vec(),
            });
        }
        /// Starts a stream at its group's remembered volume.
        fn launch(&self, key: &str, group: &str) {
            let volume = self.0.lock().unwrap().remembered[group].clone();
            self.add(key, Some(group), &volume);
        }
        fn remove(&self, key: &str) {
            self.0.lock().unwrap().streams.retain(|s| s.key != key);
        }
        fn user_sets(&self, key: &str, volume: &[f64]) {
            let mut server = self.0.lock().unwrap();
            // A stream's own volume changing replaces any temporary level.
            server.gains.remove(key);
            let stream = server.streams.iter_mut().find(|s| s.key == key).unwrap();
            stream.volume = volume.to_vec();
        }
        fn volume(&self, key: &str) -> Vec<f64> {
            let server = self.0.lock().unwrap();
            server
                .streams
                .iter()
                .find(|s| s.key == key)
                .unwrap()
                .volume
                .clone()
        }
        /// What the stream plays at.
        fn audible(&self, key: &str) -> Vec<f64> {
            let gain = self.0.lock().unwrap().gains.get(key).cloned();
            gain.unwrap_or_else(|| self.volume(key))
        }
        fn sets(&self) -> Vec<(String, Vec<f64>)> {
            self.0.lock().unwrap().sets.clone()
        }
        fn fail(&self, key: &str, outcome: Outcome) {
            self.0
                .lock()
                .unwrap()
                .set_errors
                .insert(key.into(), outcome);
        }
        fn heal(&self) {
            let mut server = self.0.lock().unwrap();
            server.set_errors.clear();
            server.list_error = None;
        }
    }

    impl Mixer for Fake {
        fn streams(&mut self) -> Result<Vec<Stream>, MixerError> {
            let server = self.0.lock().unwrap();
            match server.list_error {
                Some(error) => Err(error()),
                None => Ok(server.streams.clone()),
            }
        }
        fn set_volume(&mut self, key: &str, volume: &[f64]) -> Result<(), MixerError> {
            let mut server = self.0.lock().unwrap();
            server.sets.push((key.into(), volume.to_vec()));
            let outcome = server.set_errors.get(key).copied();
            let rounding = server.rounding;
            let Some(stream) = server.streams.iter_mut().find(|s| s.key == key) else {
                return Err(MixerError::Ended("No such entity".into()));
            };
            let stream = stream.clone();
            if !matches!(outcome, Some(Outcome::Refused | Outcome::Ignored)) {
                let volume: Vec<f64> = volume.iter().map(|v| v + rounding).collect();
                if server.temporary {
                    server.gains.insert(key.into(), volume);
                } else {
                    if let Some(group) = &stream.group {
                        server.remembered.insert(group.clone(), volume.clone());
                    }
                    let stream = server.streams.iter_mut().find(|s| s.key == key).unwrap();
                    stream.volume = volume;
                }
            }
            match outcome {
                Some(Outcome::Applied | Outcome::Refused) => {
                    Err(MixerError::Failed("timed out".into()))
                }
                _ => Ok(()),
            }
        }
        fn temporary(&self) -> bool {
            self.0.lock().unwrap().temporary
        }
    }

    fn close(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn lowers_each_channel_and_restores_exactly() {
        let mut fake = Fake::with(&[
            ("music", None, &[1.14, 0.5]),
            ("silent", None, &[0.0, 0.0]),
            ("video", None, &[0.65]),
        ]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        assert!(close(&fake.volume("music"), &[0.228, 0.1]));
        assert!(close(&fake.volume("video"), &[0.13]));
        assert_eq!(fake.volume("silent"), [0.0, 0.0]);
        // Polling again changes nothing that is already lowered.
        ducker.lower(&mut fake).unwrap();
        assert_eq!(fake.sets().len(), 2);
        ducker.restore(&mut fake).unwrap();
        assert_eq!(fake.volume("music"), [1.14, 0.5]);
        assert_eq!(fake.volume("video"), [0.65]);
        assert_eq!(fake.volume("silent"), [0.0, 0.0]);
        assert!(fake.sets().iter().all(|(key, _)| key != "silent"));
    }

    #[test]
    fn the_users_own_changes_win() {
        let mut fake = Fake::with(&[("up", None, &[0.8]), ("down", None, &[0.8])]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        fake.user_sets("up", &[0.5]);
        // A change noticed while still recording is never lowered again either.
        ducker.lower(&mut fake).unwrap();
        assert_eq!(fake.volume("up"), [0.5]);
        // Once the user has taken a stream over, it stays theirs even at WispX's level.
        fake.user_sets("up", &[0.8 * FACTOR]);
        fake.user_sets("down", &[0.05]);
        // A stream the user raised from silence mid-recording isn't touched.
        fake.add("unmuted", None, &[0.0]);
        ducker.lower(&mut fake).unwrap();
        fake.user_sets("unmuted", &[0.6]);
        ducker.lower(&mut fake).unwrap();
        ducker.restore(&mut fake).unwrap();
        assert_eq!(fake.volume("up"), [0.8 * FACTOR]);
        assert_eq!(fake.volume("down"), [0.05]);
        assert_eq!(fake.volume("unmuted"), [0.6]);
    }

    #[test]
    fn rounding_by_the_server_still_counts_as_ours() {
        let mut fake = Fake::with(&[("a", None, &[1.0, 0.6])]);
        fake.0.lock().unwrap().rounding = -0.00002;
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        ducker.lower(&mut fake).unwrap();
        fake.0.lock().unwrap().rounding = 0.0;
        ducker.restore(&mut fake).unwrap();
        assert_eq!(fake.volume("a"), [1.0, 0.6]);
    }

    #[test]
    fn streams_that_come_and_go_are_tracked_by_identity() {
        let mut fake = Fake::with(&[("7#100", Some("player"), &[0.9])]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        // The player's stream ends; a new stream from another app starts.
        fake.remove("7#100");
        fake.add("7#101", Some("game"), &[0.18]);
        ducker.lower(&mut fake).unwrap();
        assert!(close(&fake.volume("7#101"), &[0.036]));
        ducker.restore(&mut fake).unwrap();
        // The game returns to its own level, not the ended player's.
        assert!(close(&fake.volume("7#101"), &[0.18]));
        assert!(
            fake.sets()
                .iter()
                .all(|(key, v)| key != "7#100" || v == &[0.9 * FACTOR])
        );
    }

    #[test]
    fn a_stream_inheriting_the_lowered_level_returns_to_the_apps_level() {
        let mut fake = Fake::with(&[("tab1", Some("browser"), &[0.8, 0.4])]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        // The sound server remembered the lowered level and starts a new tab there.
        fake.add("tab2", Some("browser"), &fake.volume("tab1"));
        ducker.lower(&mut fake).unwrap();
        assert!(close(&fake.volume("tab2"), &[0.16, 0.08]), "lowered twice");
        ducker.restore(&mut fake).unwrap();
        assert_eq!(fake.volume("tab1"), [0.8, 0.4]);
        assert_eq!(fake.volume("tab2"), [0.8, 0.4]);

        // A tab already playing at that level before recording is its own.
        let mut fake = Fake::with(&[
            ("tab1", Some("browser"), &[1.0]),
            ("tab2", Some("browser"), &[0.2]),
        ]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        assert!(close(&fake.volume("tab2"), &[0.04]));
        ducker.restore(&mut fake).unwrap();
        assert!(close(&fake.volume("tab2"), &[0.2]));
    }

    #[test]
    fn failed_requests_never_strand_playback() {
        let mut fake = Fake::with(&[("applied", None, &[1.0]), ("refused", None, &[1.0])]);
        fake.fail("applied", Outcome::Applied);
        fake.fail("refused", Outcome::Refused);
        let mut ducker = Ducker::default();
        assert!(ducker.lower(&mut fake).is_err());
        assert!(close(&fake.volume("applied"), &[0.2]));
        assert_eq!(fake.volume("refused"), [1.0]);
        // Restoring fails too; nothing is forgotten.
        fake.fail("applied", Outcome::Refused);
        assert!(ducker.restore(&mut fake).is_err());
        assert!(close(&fake.volume("applied"), &[0.2]));
        assert_eq!(ducker.lowered.len(), 1);
        fake.heal();
        ducker.restore(&mut fake).unwrap();
        assert!(ducker.lowered.is_empty());
        assert_eq!(fake.volume("applied"), [1.0]);
        assert_eq!(fake.volume("refused"), [1.0]);
        assert!(
            !fake
                .sets()
                .iter()
                .any(|(k, v)| k == "refused" && v == &[1.0])
        );

        // A restore that timed out but applied is recognised as done on the next try.
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        fake.fail("applied", Outcome::Applied);
        assert!(ducker.restore(&mut fake).is_err());
        assert_eq!(fake.volume("applied"), [1.0]);
        let sets = fake.sets().len();
        fake.heal();
        ducker.restore(&mut fake).unwrap();
        assert!(ducker.lowered.is_empty());
        assert_eq!(fake.sets().len(), sets);

        // One reported as done but not applied is caught by listing again.
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        fake.fail("refused", Outcome::Ignored);
        assert!(ducker.restore(&mut fake).is_err());
        assert!(close(&fake.volume("refused"), &[0.2]));
        fake.heal();
        ducker.restore(&mut fake).unwrap();
        assert_eq!(fake.volume("refused"), [1.0]);
    }

    #[test]
    fn a_failed_listing_never_sets_anything_unchecked() {
        let mut fake = Fake::with(&[("music", None, &[0.9]), ("7#100", None, &[0.5])]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        let sets = fake.sets().len();
        fake.0.lock().unwrap().list_error = Some(|| MixerError::Failed("timed out".into()));
        assert!(ducker.restore(&mut fake).is_err());
        assert_eq!(fake.sets().len(), sets);
        assert_eq!(ducker.lowered.len(), 2);
        // Meanwhile the user turns the music down, and the other stream ends while a
        // new one reuses its index at the very level WispX had set.
        fake.user_sets("music", &[0.05]);
        fake.remove("7#100");
        fake.add("7#101", None, &[0.1]);
        fake.heal();
        ducker.restore(&mut fake).unwrap();
        assert!(ducker.lowered.is_empty());
        assert_eq!(fake.sets().len(), sets);
        assert_eq!(fake.volume("music"), [0.05]);
        assert_eq!(fake.volume("7#101"), [0.1]);
    }

    #[test]
    fn a_temporary_level_leaves_the_apps_remembered_volume_alone() {
        let mut fake = Fake::temporary(&[
            ("player", Some("player"), &[0.9, 0.45]),
            ("game", Some("game"), &[0.6]),
            ("voice", Some("voice"), &[0.8]),
        ]);
        fake.0.lock().unwrap().remembered = HashMap::from([
            ("player".into(), vec![0.9, 0.45]),
            ("game".into(), vec![0.6]),
        ]);
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        assert!(close(&fake.audible("player"), &[0.18, 0.09]));
        assert_eq!(fake.volume("player"), [0.9, 0.45]);
        // Polling never lowers it again, though its own volume is unchanged.
        ducker.lower(&mut fake).unwrap();
        assert_eq!(fake.sets().len(), 3);
        // The player closes before the recording ends; the user turns the game up.
        fake.remove("player");
        fake.user_sets("game", &[0.7]);
        ducker.lower(&mut fake).unwrap();
        ducker.restore(&mut fake).unwrap();
        assert_eq!(fake.audible("game"), [0.7]);
        assert_eq!(fake.audible("voice"), [0.8]);
        assert!(
            !fake
                .sets()
                .iter()
                .any(|(k, v)| k == "game" && v != &[0.6 * FACTOR])
        );
        // Started again afterwards, the player plays at its own level.
        fake.launch("player2", "player");
        assert_eq!(fake.audible("player2"), [0.9, 0.45]);

        // A recording that starts before the last one's restore went through applies
        // the same level again rather than a fifth of a fifth.
        let mut ducker = Ducker::default();
        ducker.lower(&mut fake).unwrap();
        fake.fail("voice", Outcome::Refused);
        assert!(ducker.restore(&mut fake).is_err());
        fake.heal();
        let mut next = Ducker::adopting(std::mem::take(&mut ducker.lowered));
        next.lower(&mut fake).unwrap();
        assert!(close(&fake.audible("voice"), &[0.16]));
        assert_eq!(next.lowered.len(), 3);
        next.restore(&mut fake).unwrap();
        assert_eq!(fake.audible("voice"), [0.8]);
        assert!(next.lowered.is_empty());
    }

    struct Harness {
        fake: Fake,
        commands: Sender<Command>,
        worker: thread::JoinHandle<()>,
    }

    impl Harness {
        fn start(fake: Fake, journal: Option<PathBuf>, connect_fails: bool) -> Self {
            let mixer = fake.clone();
            let connect = move || -> Result<Box<dyn Mixer>, MixerError> {
                match connect_fails {
                    true => Err(MixerError::Unavailable("no sound server".into())),
                    false => Ok(Box::new(mixer.clone())),
                }
            };
            let (commands, receiver) = mpsc::channel();
            // A long poll keeps the test in control of when passes happen.
            let worker = thread::spawn(move || {
                Worker::new(Box::new(connect), Journal(journal), Duration::from_secs(60))
                    .serve(receiver)
            });
            Self {
                fake,
                commands,
                worker,
            }
        }
        fn send(&self, command: Command) {
            self.commands.send(command).unwrap();
        }
        fn settle(&self) {
            let (done, finished) = mpsc::channel();
            self.send(Command::Flush(done));
            finished.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        fn stop(self) -> Fake {
            drop(self.commands);
            self.worker.join().unwrap();
            self.fake
        }
    }

    #[test]
    fn a_late_release_never_restores_a_newer_recording() {
        let harness = Harness::start(Fake::with(&[("music", None, &[0.5])]), None, false);
        harness.send(Command::Lower(1));
        harness.send(Command::Restore(1));
        harness.send(Command::Lower(2));
        harness.send(Command::Restore(1));
        harness.settle();
        assert!(close(&harness.fake.volume("music"), &[0.1]));
        harness.send(Command::Restore(2));
        harness.settle();
        assert_eq!(harness.fake.volume("music"), [0.5]);
        // A new recording started before the last one released restores it first.
        harness.send(Command::Lower(3));
        harness.send(Command::Lower(4));
        harness.settle();
        assert!(close(&harness.fake.volume("music"), &[0.1]));
        // Shutting the worker down releases whatever is still lowered.
        let fake = harness.stop();
        assert_eq!(fake.volume("music"), [0.5]);
    }

    #[test]
    fn polling_lowers_playback_that_starts_mid_recording() {
        let fake = Fake::with(&[("a", None, &[1.0])]);
        let mixer = fake.clone();
        let (commands, receiver) = mpsc::channel();
        let connect = move || -> Result<Box<dyn Mixer>, MixerError> { Ok(Box::new(mixer.clone())) };
        let worker = thread::spawn(move || {
            Worker::new(Box::new(connect), Journal(None), Duration::from_millis(10)).serve(receiver)
        });
        commands.send(Command::Lower(1)).unwrap();
        fake.add("late", None, &[0.7]);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !close(&fake.volume("late"), &[0.14]) {
            assert!(std::time::Instant::now() < deadline, "never lowered");
            thread::sleep(Duration::from_millis(5));
        }
        commands.send(Command::Restore(1)).unwrap();
        drop(commands);
        worker.join().unwrap();
        assert_eq!(fake.volume("late"), [0.7]);
        assert_eq!(fake.volume("a"), [1.0]);
    }

    #[test]
    fn a_crashed_recording_is_restored_at_the_next_start() {
        let journal = std::env::temp_dir().join(format!(
            "wispx-lowered-{}-{:?}.json",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_file(&journal);
        let fake = Fake::with(&[("music", None, &[0.9]), ("other", None, &[0.4])]);
        let harness = Harness::start(fake.clone(), Some(journal.clone()), false);
        harness.send(Command::Lower(1));
        harness.settle();
        assert!(journal.exists());
        // The process dies: the worker never releases and the journal stays.
        std::mem::forget(harness);
        fake.user_sets("other", &[0.3]);

        // The next start finds no sound server yet, then one appears.
        let unavailable = Harness::start(fake.clone(), Some(journal.clone()), true);
        unavailable.send(Command::Lower(1));
        unavailable.send(Command::Restore(1));
        unavailable.settle();
        drop(unavailable.commands);
        assert!(journal.exists(), "lost before it could be restored");
        let harness = Harness::start(fake.clone(), Some(journal.clone()), false);
        harness.settle();
        assert_eq!(fake.volume("music"), [0.9]);
        assert_eq!(fake.volume("other"), [0.3]);
        assert!(!journal.exists());
        harness.stop();
    }

    fn journal_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "wispx-lowered-{name}-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&path);
        path
    }

    fn eventually(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_failed_restore_is_kept_retried_and_never_lowered_twice() {
        let journal = journal_path("retry").join("lowered.json");
        let fake = Fake::with(&[("music", None, &[0.5])]);
        let harness = Harness::start(fake.clone(), Some(journal.clone()), false);
        harness.send(Command::Lower(1));
        harness.settle();
        fake.fail("music", Outcome::Refused);
        harness.send(Command::Restore(1));
        harness.settle();
        assert!(close(&fake.volume("music"), &[0.1]));
        assert!(journal.exists(), "forgotten while still lowered");
        // A new recording starts before the restore went through.
        harness.send(Command::Lower(2));
        harness.settle();
        assert!(close(&fake.volume("music"), &[0.1]));
        assert!(
            fake.sets()
                .iter()
                .all(|(_, v)| close(v, &[0.1]) || v == &[0.5])
        );
        // An old recording's late release doesn't restore the new one.
        fake.heal();
        harness.send(Command::Restore(1));
        harness.settle();
        assert!(close(&fake.volume("music"), &[0.1]));
        harness.send(Command::Restore(2));
        harness.settle();
        assert_eq!(fake.volume("music"), [0.5]);
        assert!(!journal.exists());

        // Between recordings, a failed restore is retried until it goes through.
        harness.send(Command::Lower(3));
        harness.settle();
        fake.fail("music", Outcome::Refused);
        harness.send(Command::Restore(3));
        harness.settle();
        thread::sleep(RETRY_MAX * 2);
        assert!(close(&fake.volume("music"), &[0.1]));
        fake.heal();
        eventually("never retried", || fake.volume("music") == [0.5]);
        eventually("journal kept", || !journal.exists());
        // Retries back off: a few attempts, not one per millisecond.
        let attempts = fake.sets().iter().filter(|(_, v)| v == &[0.5]).count();
        assert!(attempts < 15, "{attempts} attempts");
        harness.stop();
        let _ = fs::remove_dir_all(journal.parent().unwrap());
    }

    #[test]
    fn a_journal_that_couldnt_be_written_is_written_later() {
        let blocked = journal_path("blocked");
        // A file where the journal's folder should be makes every write fail.
        fs::write(&blocked, b"").unwrap();
        let journal = blocked.join("lowered.json");
        let fake = Fake::with(&[("music", None, &[0.5])]);
        let harness = Harness::start(fake.clone(), Some(journal.clone()), false);
        harness.send(Command::Lower(1));
        harness.settle();
        assert!(!journal.exists());
        fs::remove_file(&blocked).unwrap();
        harness.settle();
        assert!(journal.exists(), "not retried");
        harness.send(Command::Restore(1));
        harness.settle();
        assert!(!journal.exists());
        harness.stop();
        let _ = fs::remove_dir_all(&blocked);
    }

    #[test]
    fn an_unreachable_server_stops_asking_until_the_next_recording() {
        let fake = Fake::with(&[("a", None, &[1.0])]);
        fake.0.lock().unwrap().list_error = Some(|| MixerError::Unavailable("down".into()));
        let mixer = fake.clone();
        let lists = Arc::new(Mutex::new(0));
        let counted = lists.clone();
        struct Counting(Fake, Arc<Mutex<usize>>);
        impl Mixer for Counting {
            fn streams(&mut self) -> Result<Vec<Stream>, MixerError> {
                *self.1.lock().unwrap() += 1;
                self.0.streams()
            }
            fn set_volume(&mut self, key: &str, volume: &[f64]) -> Result<(), MixerError> {
                self.0.set_volume(key, volume)
            }
        }
        let connect = move || -> Result<Box<dyn Mixer>, MixerError> {
            Ok(Box::new(Counting(mixer.clone(), counted.clone())))
        };
        let (commands, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            Worker::new(Box::new(connect), Journal(None), Duration::from_millis(5)).serve(receiver)
        });
        commands.send(Command::Lower(1)).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(*lists.lock().unwrap(), 1);
        commands.send(Command::Restore(1)).unwrap();
        drop(commands);
        worker.join().unwrap();
        assert_eq!(fake.volume("a"), [1.0]);
    }
}
