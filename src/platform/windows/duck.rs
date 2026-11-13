//! Per-application session volume through Core Audio, on every active output.
//! Only the lowering worker thread creates and uses these COM objects.
use crate::audio::duck::{Mixer, MixerError, Stream};
use std::collections::HashMap;
use windows::{
    Win32::{
        Media::Audio::{
            AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl,
            IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
            ISimpleAudioVolume, MMDeviceEnumerator, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        },
    },
    core::{GUID, Interface, PWSTR},
};

/// Tags WispX's own volume changes for other apps' session listeners.
const CONTEXT: GUID = GUID::from_u128(0x5f1b_6c47_2a8e_4d1b_9c3e_7a2d_4e6f_8b10);

pub struct CoreAudio {
    enumerator: IMMDeviceEnumerator,
    /// The sessions from the latest listing, by key.
    volumes: HashMap<String, ISimpleAudioVolume>,
    own_pid: u32,
}

impl CoreAudio {
    /// Joins the multithreaded apartment for the rest of the calling worker's life.
    pub fn connect() -> Result<Self, MixerError> {
        // SAFETY: initializes COM for this thread, which then owns every interface below;
        // the worker lives as long as the process, so the apartment is never left.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| MixerError::Unavailable(format!("COM: {e}")))?;
            let enumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| MixerError::Unavailable(format!("Windows Audio: {e}")))?;
            Ok(Self {
                enumerator,
                volumes: HashMap::new(),
                own_pid: std::process::id(),
            })
        }
    }

    /// One output's sessions; a session that fails a query is skipped.
    fn device(
        &mut self,
        device: &IMMDevice,
        streams: &mut Vec<Stream>,
    ) -> windows::core::Result<()> {
        // SAFETY: COM calls on interfaces owned by this thread; returned strings are
        // copied and then freed with CoTaskMemFree as the API requires.
        unsafe {
            let device_id = take(device.GetId()?);
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
            let sessions = manager.GetSessionEnumerator()?;
            for index in 0..sessions.GetCount()? {
                let Ok(control) = sessions.GetSession(index) else {
                    continue;
                };
                if let Some((key, stream_volume, stream)) = self.session(&device_id, &control) {
                    self.volumes.insert(key, stream_volume);
                    streams.push(stream);
                }
            }
        }
        Ok(())
    }

    fn session(
        &self,
        device_id: &str,
        control: &IAudioSessionControl,
    ) -> Option<(String, ISimpleAudioVolume, Stream)> {
        // SAFETY: as in `device`.
        unsafe {
            if control.GetState().ok()? == AudioSessionStateExpired {
                return None;
            }
            let control2: IAudioSessionControl2 = control.cast().ok()?;
            // Sessions shared by several processes report no single owner.
            if control2.GetProcessId().ok() == Some(self.own_pid) {
                return None;
            }
            let instance = take(control2.GetSessionInstanceIdentifier().ok()?);
            let group = control2.GetSessionIdentifier().ok().map(|id| take(id));
            let volume: ISimpleAudioVolume = control.cast().ok()?;
            let level = volume.GetMasterVolume().ok()?;
            let key = format!("{device_id}|{instance}");
            Some((
                key.clone(),
                volume,
                Stream {
                    key,
                    group,
                    volume: vec![f64::from(level)],
                },
            ))
        }
    }
}

/// Copies and frees a COM-allocated string.
///
/// # Safety
/// `text` must be a valid, CoTaskMemAlloc'd, NUL-terminated string or null.
unsafe fn take(text: PWSTR) -> String {
    if text.is_null() {
        return String::new();
    }
    // SAFETY: the caller guarantees a valid string, freed exactly once here.
    unsafe {
        let copy = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0 as *const _));
        copy
    }
}

impl Mixer for CoreAudio {
    fn streams(&mut self) -> Result<Vec<Stream>, MixerError> {
        self.volumes.clear();
        let mut streams = Vec::new();
        // SAFETY: COM calls on this thread's enumerator.
        let devices = unsafe {
            self.enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .and_then(|devices| {
                    (0..devices.GetCount()?)
                        .map(|i| devices.Item(i))
                        .collect::<windows::core::Result<Vec<_>>>()
                })
                .map_err(|e| MixerError::Failed(format!("listing outputs: {e}")))?
        };
        for device in devices {
            // An output that disappears mid-listing shouldn't hide the others.
            let _ = self.device(&device, &mut streams);
        }
        Ok(streams)
    }

    fn set_volume(&mut self, key: &str, volume: &[f64]) -> Result<(), MixerError> {
        let session = self
            .volumes
            .get(key)
            .ok_or_else(|| MixerError::Ended("an audio session ended".into()))?;
        let level = volume.first().copied().unwrap_or(0.0).clamp(0.0, 1.0) as f32;
        // SAFETY: a COM call on this thread's session interface with a static GUID.
        unsafe { session.SetMasterVolume(level, &CONTEXT) }
            .map_err(|e| MixerError::Failed(format!("setting session volume: {e}")))
    }
}
