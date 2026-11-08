//! Compatible named objects let the Rust exe wake or replace the old C# install.
use std::{
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
    thread::{self, JoinHandle},
};
use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0},
        System::Threading::*,
    },
    core::w,
};
pub const QUIT_NAME: windows::core::PCWSTR = w!("Local\\WispX.Quit.v1");
pub enum InstanceEvent {
    Wake,
    Quit,
}
pub struct Instance {
    _mutex: OwnedHandle,
    shutdown: Arc<OwnedHandle>,
    threads: Vec<JoinHandle<()>>,
}
impl Instance {
    pub fn acquire(
        sink: Arc<dyn Fn(InstanceEvent) + Send + Sync>,
    ) -> windows::core::Result<Option<Self>> {
        // SAFETY: named objects use constant names; each created handle has exactly one owner.
        unsafe {
            let mutex = CreateMutexW(None, false, w!("Local\\WispX.SingleInstance.v1"))?;
            let existed = GetLastError() == ERROR_ALREADY_EXISTS;
            let mutex = OwnedHandle::from_raw_handle(mutex.0);
            if existed {
                if let Ok(wake) = OpenEventW(EVENT_MODIFY_STATE, false, w!("Local\\WispX.Wake.v1"))
                {
                    let wake = OwnedHandle::from_raw_handle(wake.0);
                    let _ = SetEvent(HANDLE(wake.as_raw_handle()));
                }
                return Ok(None);
            }
            let shutdown = Arc::new(OwnedHandle::from_raw_handle(
                CreateEventW(None, true, false, None)?.0,
            ));
            let mut threads = Vec::new();
            for (name, event) in [
                (w!("Local\\WispX.Wake.v1"), InstanceEvent::Wake),
                (QUIT_NAME, InstanceEvent::Quit),
            ] {
                let handle =
                    OwnedHandle::from_raw_handle(CreateEventW(None, false, false, name)?.0);
                let shutdown = shutdown.clone();
                let sink = sink.clone();
                threads.push(thread::spawn(move || {
                    let handles = [
                        HANDLE(shutdown.as_raw_handle()),
                        HANDLE(handle.as_raw_handle()),
                    ];
                    loop {
                        if WaitForMultipleObjects(&handles, false, INFINITE).0
                            != WAIT_OBJECT_0.0 + 1
                        {
                            break;
                        }
                        match event {
                            InstanceEvent::Wake => sink(InstanceEvent::Wake),
                            InstanceEvent::Quit => {
                                sink(InstanceEvent::Quit);
                                break;
                            }
                        }
                    }
                }));
            }
            Ok(Some(Self {
                _mutex: mutex,
                shutdown,
                threads,
            }))
        }
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        // SAFETY: shutdown remains alive until all waiter threads have joined.
        unsafe {
            let _ = SetEvent(HANDLE(self.shutdown.as_raw_handle()));
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
