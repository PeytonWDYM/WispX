//! Microphone capture through the waveIn API, which the system has no matter what else
//! is installed. As on Linux, samples stay in this process's memory until a backend
//! needs a file, and only 16 kHz mono s16 is ever asked for, so Windows does any
//! resampling.

use super::ffi::*;
use crate::audio::level::LevelMeter;
use crate::audio::wav;
use crate::config::{AudioConfig, RecorderChoice};
use crate::process::Toolbox;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SAMPLE_RATE: u32 = 16_000;
/// Four buffers of 30 ms, like the original: enough that a busy moment loses nothing,
/// and few enough that the level meter keeps up with speech.
const BUFFERS: usize = 4;
const BUFFER_BYTES: usize = 960;
const OPEN_LIMIT: Duration = Duration::from_secs(2);
/// How long to wait for the last samples once a stop was requested.
const FINISH_TIMEOUT: Duration = Duration::from_millis(1500);
/// A microphone that has delivered nothing for this long has gone away.
const STALL_LIMIT: Duration = Duration::from_secs(3);
const WAIT_STEP: u32 = 100;

/// Microphones still open. See [`wait_for_stops`].
static OPEN: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecorderPlan {
    pub name: &'static str,
    device: Option<String>,
}

impl RecorderPlan {
    /// What `wispx doctor` says is recording.
    pub fn describe(&self) -> String {
        match &self.device {
            Some(device) => format!("waveIn, {device}"),
            None => "waveIn, the default microphone".to_string(),
        }
    }
}

pub fn resolve(config: &AudioConfig, _tools: &Toolbox) -> Result<RecorderPlan, String> {
    match config.recorder {
        RecorderChoice::Auto | RecorderChoice::WaveIn => Ok(RecorderPlan {
            name: RecorderChoice::WaveIn.as_str(),
            device: config.device.clone().filter(|device| !device.is_empty()),
        }),
        other => Err(format!(
            "audio.recorder is \"{}\", which only exists on Linux. Set it to \"auto\" or \
             \"wavein\"",
            other.as_str()
        )),
    }
}

/// Every microphone, with the name for both its ID and its label: numbers change when
/// a device is plugged in, and a name goes on meaning the same microphone.
pub fn devices() -> Vec<(String, String)> {
    device_names()
        .into_iter()
        .map(|name| (name.clone(), name))
        .collect()
}

fn device_names() -> Vec<String> {
    // SAFETY: takes nothing.
    let count = unsafe { waveInGetNumDevs() };
    let mut names = Vec::new();
    for id in 0..count {
        // SAFETY: an all-zero `WaveInCaps` is valid, and its size is passed along.
        unsafe {
            let mut caps: WaveInCaps = mem::zeroed();
            if waveInGetDevCapsW(id as usize, &raw mut caps, size_of::<WaveInCaps>() as u32) == 0 {
                let name = caps.name;
                names.push(from_wide(&name));
            }
        }
    }
    names
}

struct Shared {
    pcm: Vec<u8>,
    meter: LevelMeter,
}

pub struct Captured {
    pub pcm: Vec<u8>,
    pub duration_ms: u64,
}

/// A running capture. Dropping it turns the microphone off and discards the audio.
pub struct Recording {
    name: &'static str,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    wake: Arc<Owned>,
    input_closed: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
    done: Receiver<()>,
}

impl Recording {
    /// Starts capturing, keeping at most `max_bytes` of audio in memory.
    pub fn start(plan: &RecorderPlan, max_bytes: usize) -> Result<Self, String> {
        let device = match &plan.device {
            None => None,
            Some(name) => match device_names().iter().position(|have| have == name) {
                Some(index) => Some(index as u32),
                None => {
                    return Err(format!(
                        "the microphone \"{name}\" is not connected. Pick another in Settings"
                    ));
                }
            },
        };
        let wake = Arc::new(Owned::event().map_err(|error| format!("waveIn: {error}"))?);
        let shared = Arc::new(Mutex::new(Shared {
            pcm: Vec::with_capacity(max_bytes.min(wav::BYTES_PER_SECOND as usize * 30)),
            meter: LevelMeter::default(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let input_closed = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None));
        let (opened_tx, opened) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        {
            let capture = Capture {
                device,
                max_bytes,
                shared: Arc::clone(&shared),
                stop: Arc::clone(&stop),
                wake: Arc::clone(&wake),
                input_closed: Arc::clone(&input_closed),
                last_error: Arc::clone(&last_error),
            };
            OPEN.fetch_add(1, Ordering::SeqCst);
            thread::Builder::new()
                .name("microphone".to_string())
                .spawn(move || {
                    capture.run(&opened_tx);
                    OPEN.fetch_sub(1, Ordering::SeqCst);
                    let _ = done_tx.send(());
                })
                .map_err(|error| {
                    OPEN.fetch_sub(1, Ordering::SeqCst);
                    format!("waveIn: {error}")
                })?;
        }
        let mut recording = Recording {
            name: plan.name,
            shared,
            stop,
            wake,
            input_closed,
            last_error,
            done,
        };
        match opened.recv_timeout(OPEN_LIMIT) {
            Ok(Ok(())) => Ok(recording),
            Ok(Err(message)) => Err(message),
            Err(_) => {
                recording.request_stop();
                Err("the microphone did not open in time".to_string())
            }
        }
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

    /// True once the microphone stopped delivering samples by itself.
    pub fn input_closed(&self) -> bool {
        self.input_closed.load(Ordering::SeqCst)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// What went wrong with the microphone, for explaining a failure.
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok()?.clone()
    }

    /// Turns the microphone off now, without blocking. Samples stay available for
    /// `finish`.
    pub fn request_stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // SAFETY: `wake` is a valid event for as long as this holds it.
        unsafe {
            SetEvent(self.wake.0);
        }
    }

    /// Waits briefly for the last samples, then hands over all the audio.
    pub fn finish(mut self) -> Captured {
        self.request_stop();
        let _ = self.done.recv_timeout(FINISH_TIMEOUT);
        let mut pcm = self
            .shared
            .lock()
            .map(|mut s| mem::take(&mut s.pcm))
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
        self.request_stop();
    }
}

/// Waits for microphones still being closed, up to `limit`.
pub fn wait_for_stops(limit: Duration) {
    let deadline = Instant::now() + limit;
    while OPEN.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

fn open_error(code: u32) -> String {
    let reason = match code {
        MMSYSERR_ALLOCATED => "another program has it to itself",
        MMSYSERR_BADDEVICEID | MMSYSERR_NODRIVER => "it is not connected",
        _ => "Windows refused",
    };
    format!(
        "the microphone would not open: {reason} (waveIn error {code}). Check that one is \
         connected, and that Settings > Privacy & security > Microphone lets desktop apps \
         use it"
    )
}

struct Capture {
    device: Option<u32>,
    max_bytes: usize,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    wake: Arc<Owned>,
    input_closed: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<String>>>,
}

impl Capture {
    fn fail(&self, message: String) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(message);
        }
        self.input_closed.store(true, Ordering::SeqCst);
    }

    /// Owns the device from opening it to closing it, and ends with it closed.
    fn run(&self, opened: &Sender<Result<(), String>>) {
        let format = WaveFormat {
            tag: WAVE_FORMAT_PCM,
            channels: 1,
            samples_per_sec: SAMPLE_RATE,
            bytes_per_sec: SAMPLE_RATE * 2,
            block_align: 2,
            bits_per_sample: 16,
            extra: 0,
        };
        let (id, map_flag) = match self.device {
            Some(index) => (index, WAVE_MAPPED),
            None => (WAVE_MAPPER, 0),
        };
        let mut device = ptr::null_mut();
        // SAFETY: the event outlives the device, which is closed before this returns.
        let code = unsafe {
            waveInOpen(
                &raw mut device,
                id,
                &raw const format,
                self.wake.0 as usize,
                0,
                CALLBACK_EVENT | map_flag,
            )
        };
        if code != 0 {
            let _ = opened.send(Err(open_error(code)));
            return;
        }

        let mut storage = vec![[0u8; BUFFER_BYTES]; BUFFERS];
        // Boxed, so these stay put while the system holds pointers to them.
        let mut headers: Box<[WaveHdr]> = storage
            .iter_mut()
            .map(|buffer| WaveHdr {
                data: buffer.as_mut_ptr(),
                buffer_length: BUFFER_BYTES as u32,
                bytes_recorded: 0,
                user: 0,
                flags: 0,
                loops: 0,
                next: ptr::null_mut(),
                reserved: 0,
            })
            .collect();
        let size = size_of::<WaveHdr>() as u32;

        let mut started = true;
        // SAFETY: headers and buffers stay valid and in place until they are unprepared
        // below, and `device` until it is closed.
        unsafe {
            for header in &mut *headers {
                let prepare = waveInPrepareHeader(device, &raw mut *header, size);
                let add = waveInAddBuffer(device, &raw mut *header, size);
                started &= prepare == 0 && add == 0;
            }
            started = started && waveInStart(device) == 0;
        }
        if started {
            let _ = opened.send(Ok(()));
            self.capture(device, &mut headers);
        } else {
            let _ = opened.send(Err(open_error(1)));
        }

        // SAFETY: after the reset the system is done with every buffer.
        unsafe {
            waveInReset(device);
            for header in &mut *headers {
                waveInUnprepareHeader(device, &raw mut *header, size);
            }
            waveInClose(device);
        }
    }

    fn capture(&self, device: Handle, headers: &mut [WaveHdr]) {
        let size = size_of::<WaveHdr>() as u32;
        let mut next = 0;
        let mut last_data = Instant::now();
        loop {
            // SAFETY: the event is valid. This only reads the flags the system sets on
            // headers it was given, and gives each one back once it has been emptied.
            unsafe {
                WaitForSingleObject(self.wake.0, WAIT_STEP);
                // Before looking, so anything that happens from here sets it again.
                ResetEvent(self.wake.0);
                while ptr::read_volatile(&raw const headers[next].flags) & WHDR_DONE != 0 {
                    last_data = Instant::now();
                    self.take(&headers[next]);
                    let header = &raw mut headers[next];
                    (*header).flags &= !WHDR_DONE;
                    next = (next + 1) % headers.len();
                    if !self.stop.load(Ordering::SeqCst) {
                        let code = waveInAddBuffer(device, header, size);
                        if code != 0 {
                            self.fail(format!("the microphone went away (waveIn error {code})"));
                            return;
                        }
                    }
                }
            }
            if self.stop.load(Ordering::SeqCst) {
                self.drain(device, headers, next);
                return;
            }
            if last_data.elapsed() > STALL_LIMIT {
                self.fail("the microphone stopped sending sound".to_string());
                return;
            }
        }
    }

    /// Stopping hands back what is in the buffers, including the one being filled, so
    /// the last word is not cut off.
    fn drain(&self, device: Handle, headers: &mut [WaveHdr], mut next: usize) {
        // SAFETY: as in `capture`. After the reset, every buffer is marked done.
        unsafe {
            waveInReset(device);
            for _ in 0..headers.len() {
                if ptr::read_volatile(&raw const headers[next].flags) & WHDR_DONE != 0 {
                    self.take(&headers[next]);
                    let header = &raw mut headers[next];
                    (*header).flags &= !WHDR_DONE;
                }
                next = (next + 1) % headers.len();
            }
        }
    }

    fn take(&self, header: &WaveHdr) {
        // SAFETY: the system has finished with this buffer and filled this much of it.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                header.data,
                (ptr::read_volatile(&raw const header.bytes_recorded) as usize).min(BUFFER_BYTES),
            )
        };
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        shared.meter.feed(bytes);
        let room = self.max_bytes.saturating_sub(shared.pcm.len());
        shared
            .pcm
            .extend_from_slice(&bytes[..room.min(bytes.len())]);
    }
}
