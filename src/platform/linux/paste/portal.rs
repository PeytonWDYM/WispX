//! RemoteDesktop portal keyboard with KWin focus checks.
use super::super::kwin;
use super::{Keyboard, Refused, Target, lock};
use ashpd::desktop::{
    PersistMode,
    remote_desktop::{DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions, StartOptions},
};
use futures_lite::{StreamExt, future};
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// How long a paste request may wait for the portal worker before it's dropped.
const PORTAL_WAIT: Duration = Duration::from_secs(3);
/// Upper bound on one key chord through the portal, releases included.
const CHORD_LIMIT: Duration = Duration::from_millis(1000);
const RELEASE_LIMIT: Duration = Duration::from_millis(250);
const KEY_GAP: Duration = Duration::from_millis(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Queued,
    Typing,
    /// The caller gave up or the session ended; must never be typed.
    Dropped,
}
struct Request {
    keysyms: Vec<u32>,
    target: Target,
    deadline: Instant,
    state: Mutex<State>,
    reply: mpsc::SyncSender<Result<(), Refused>>,
}
impl Request {
    fn drop_with(&self, refused: Refused) {
        *lock(&self.state) = State::Dropped;
        let _ = self.reply.try_send(Err(refused));
    }
}
enum Command {
    Paste(Arc<Request>),
    /// The portal closed session number `.0`.
    Closed(u64),
}
/// Queues a paste and waits for it. A request that times out while still
/// queued is dropped, and the worker checks that before typing, so a caller
/// that has fallen back to copying never sees a late paste. A request already
/// typing is waited out: its chord is time-bounded.
fn submit(
    commands: &mpsc::Sender<Command>,
    keysyms: &[u32],
    target: &Target,
    wait: Duration,
) -> Result<(), Refused> {
    let (reply, result) = mpsc::sync_channel(1);
    let request = Arc::new(Request {
        keysyms: keysyms.to_vec(),
        target: target.clone(),
        deadline: Instant::now() + wait,
        state: Mutex::new(State::Queued),
        reply,
    });
    commands
        .send(Command::Paste(request.clone()))
        .map_err(|_| Refused::Failed("portal keyboard stopped".into()))?;
    await_reply(&request, &result, wait)
}
fn await_reply(
    request: &Request,
    result: &mpsc::Receiver<Result<(), Refused>>,
    wait: Duration,
) -> Result<(), Refused> {
    match result.recv_timeout(wait) {
        Ok(result) => result,
        Err(_) => {
            {
                let mut state = lock(&request.state);
                if *state != State::Typing {
                    *state = State::Dropped;
                    return Err(Refused::Failed("portal keyboard timed out".into()));
                }
            }
            result
                .recv()
                .unwrap_or_else(|_| Err(Refused::Failed("portal keyboard stopped".into())))
        }
    }
}
/// One request on the worker: verify focus, claim it, type. The claim happens
/// under the request's lock after the focus answer, so a caller that timed out
/// during the focus query wins and nothing is typed.
fn serve(
    request: &Request,
    focused: impl FnOnce() -> Option<Target>,
    chord: impl FnOnce(&[u32]) -> Result<(), String>,
) {
    if *lock(&request.state) != State::Queued || Instant::now() >= request.deadline {
        request.drop_with(Refused::Failed("paste request expired".into()));
        return;
    }
    let focus = focused();
    {
        let mut state = lock(&request.state);
        if *state != State::Queued || Instant::now() >= request.deadline {
            drop(state);
            request.drop_with(Refused::Failed("paste request expired".into()));
            return;
        }
        if focus.as_ref() != Some(&request.target) {
            drop(state);
            request.drop_with(Refused::FocusChanged);
            return;
        }
        *state = State::Typing;
    }
    let result = chord(&request.keysyms).map_err(Refused::Failed);
    let _ = request.reply.try_send(result);
}
/// Drops every queued request; used when a session ends.
fn reject_queued(commands: &mpsc::Receiver<Command>, reason: &str) {
    while let Ok(command) = commands.try_recv() {
        if let Command::Paste(request) = command {
            request.drop_with(Refused::Failed(reason.into()));
        }
    }
}
#[derive(Debug, Eq, PartialEq)]
enum SessionEnd {
    /// The desktop closed the session: remote control was revoked or stopped.
    Closed,
    /// Sending a key failed; the session may be broken.
    Failed,
}
/// Serves requests for session `generation` until it ends.
fn run_session(
    commands: &mpsc::Receiver<Command>,
    generation: u64,
    mut focused: impl FnMut() -> Option<Target>,
    mut chord: impl FnMut(&[u32]) -> Result<(), String>,
) -> SessionEnd {
    while let Ok(command) = commands.recv() {
        match command {
            Command::Paste(request) => {
                let mut failed = false;
                serve(&request, &mut focused, |keys| {
                    let result = chord(keys);
                    failed = result.is_err();
                    result
                });
                if failed {
                    return SessionEnd::Failed;
                }
            }
            Command::Closed(closed) if closed == generation => return SessionEnd::Closed,
            Command::Closed(_) => {}
        }
    }
    SessionEnd::Failed
}

/// Presses `keysyms` in order and releases them in reverse, within `limit`.
/// Every key that went down is released again even if a step fails or the
/// limit passes, so no modifier stays stuck. `Ok` means every key went down;
/// a press cut off by the limit counts as not typed, so the caller copies.
async fn chord<F, Fut>(keysyms: &[u32], mut send: F, limit: Duration) -> Result<(), String>
where
    F: FnMut(u32, KeyState) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut down: Vec<u32> = Vec::new();
    let mut all_down = false;
    let outcome = {
        let sequence = async {
            for &k in keysyms {
                send(k, KeyState::Pressed).await?;
                down.push(k);
                async_io::Timer::after(KEY_GAP).await;
            }
            all_down = true;
            while let Some(&k) = down.last() {
                send(k, KeyState::Released).await?;
                down.pop();
                async_io::Timer::after(KEY_GAP).await;
            }
            Ok(())
        };
        future::or(sequence, async {
            async_io::Timer::after(limit).await;
            Err(format!("keys took longer than {limit:?}"))
        })
        .await
    };
    while let Some(k) = down.pop() {
        let release = future::or(send(k, KeyState::Released), async {
            async_io::Timer::after(RELEASE_LIMIT).await;
            Err("release timed out".into())
        })
        .await;
        if let Err(error) = release {
            eprintln!("Paste: couldn't release key {k:#x}: {error}");
        }
    }
    match outcome {
        Ok(()) => Ok(()),
        // Every key went down, so the paste happened; only a release failed.
        Err(error) if all_down => {
            eprintln!("Paste: {error}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// One long-lived keyboard-only RemoteDesktop session. Plasma asks once, then a
/// restore token skips the dialog on later starts; dictation never opens it.
pub(super) struct PortalKeys {
    commands: Mutex<mpsc::Sender<Command>>,
    ready: Arc<AtomicBool>,
    revoked: Arc<AtomicBool>,
}
fn token_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|dir| dir.join("WispX/remote-desktop.token"))
}
fn kwin_focus() -> Option<Target> {
    kwin::active_window().ok().flatten().map(|w| Target {
        id: w.id,
        class: w.class,
    })
}
impl PortalKeys {
    pub(super) fn start() -> Self {
        let (tx, rx) = mpsc::channel::<Command>();
        let ready = Arc::new(AtomicBool::new(false));
        let revoked = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let (flag, gone, closer) = (ready.clone(), revoked.clone(), tx.clone());
        thread::Builder::new()
            .name("portal-keyboard".into())
            .spawn(move || {
                super::super::register_app();
                loop {
                    match future::block_on(open()) {
                        Ok((portal, session)) => {
                            let current = generation.fetch_add(1, Ordering::AcqRel) + 1;
                            // Requests queued while no session was open are stale.
                            reject_queued(&rx, "remote control was not ready");
                            let session = Arc::new(session);
                            watch_closed(
                                session.clone(),
                                current,
                                generation.clone(),
                                flag.clone(),
                                closer.clone(),
                            );
                            flag.store(true, Ordering::Release);
                            let end = run_session(&rx, current, kwin_focus, |keys| {
                                future::block_on(chord(
                                    keys,
                                    |keysym, state| {
                                        let (portal, session) = (&portal, &*session);
                                        async move {
                                            portal
                                                .notify_keyboard_keysym(
                                                    session,
                                                    keysym as i32,
                                                    state,
                                                    Default::default(),
                                                )
                                                .await
                                                .map_err(|e| e.to_string())
                                        }
                                    },
                                    CHORD_LIMIT,
                                ))
                            });
                            flag.store(false, Ordering::Release);
                            generation.fetch_add(1, Ordering::AcqRel);
                            reject_queued(&rx, "remote control ended");
                            let _ = future::block_on(session.close());
                            if end == SessionEnd::Closed {
                                // Reopening would undo the user's choice to stop
                                // remote control; it's requested again at next start.
                                eprintln!(
                                    "Paste: the desktop ended remote control; transcripts \
                                     will be copied until WispX restarts"
                                );
                                gone.store(true, Ordering::Release);
                                return;
                            }
                        }
                        Err(Opened::Declined) => {
                            eprintln!(
                                "Paste: remote control was declined; transcripts will be copied"
                            );
                            gone.store(true, Ordering::Release);
                            return;
                        }
                        Err(Opened::Failed(error)) => eprintln!("Paste: {error}"),
                    }
                    thread::sleep(Duration::from_secs(5));
                }
            })
            .expect("spawn portal keyboard");
        Self {
            commands: Mutex::new(tx),
            ready,
            revoked,
        }
    }
}
/// Tells the worker when the desktop closes session `current`, for example when
/// the user stops remote control. Returns once it's listening, so no close is
/// missed; it stops watching once a newer session replaces this one.
fn watch_closed(
    session: Arc<ashpd::desktop::Session<RemoteDesktop>>,
    current: u64,
    generation: Arc<AtomicU64>,
    ready: Arc<AtomicBool>,
    commands: mpsc::Sender<Command>,
) {
    let (listening, subscribed) = mpsc::sync_channel(1);
    let spawned = thread::Builder::new()
        .name("portal-keyboard-closed".into())
        .spawn(move || {
            let closed = match future::block_on(session.receive_closed()) {
                Ok(closed) => closed,
                Err(error) => {
                    eprintln!("Paste: can't watch the remote control session: {error}");
                    let _ = listening.send(());
                    return;
                }
            };
            let _ = listening.send(());
            let mut closed = std::pin::pin!(closed);
            loop {
                // A stream that ends means the portal connection went away.
                let signal = future::block_on(future::or(
                    async {
                        closed.next().await;
                        Some(())
                    },
                    async {
                        async_io::Timer::after(Duration::from_secs(1)).await;
                        None
                    },
                ));
                if generation.load(Ordering::Acquire) != current {
                    return;
                }
                if signal.is_some() {
                    // Before the worker sees the message, so no new paste is queued.
                    ready.store(false, Ordering::Release);
                    let _ = commands.send(Command::Closed(current));
                    return;
                }
            }
        });
    match spawned {
        Ok(_) => {
            let _ = subscribed.recv_timeout(Duration::from_secs(2));
        }
        Err(error) => eprintln!("Paste: {error}"),
    }
}
impl Keyboard for PortalKeys {
    fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    fn describe(&self) -> String {
        if self.ready() {
            "RemoteDesktop portal with KWin focus checks".into()
        } else if self.revoked.load(Ordering::Acquire) {
            "copy only (remote control was declined or stopped; restart WispX to ask again)".into()
        } else {
            "RemoteDesktop portal (not granted yet)".into()
        }
    }
    fn focused(&self) -> Option<Target> {
        kwin_focus()
    }
    fn modifier_held(&self) -> Option<bool> {
        None
    }
    fn paste(&self, keysyms: &[u32], target: &Target) -> Result<(), Refused> {
        if !self.ready() {
            return Err(Refused::Failed("remote control isn't available".into()));
        }
        let commands = lock(&self.commands).clone();
        // The worker checks focus again immediately before typing.
        submit(&commands, keysyms, target, PORTAL_WAIT)
    }
}
enum Opened {
    Declined,
    Failed(String),
}
impl From<ashpd::Error> for Opened {
    fn from(error: ashpd::Error) -> Self {
        match error {
            ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => Self::Declined,
            other => Self::Failed(other.to_string()),
        }
    }
}
async fn open() -> Result<(RemoteDesktop, ashpd::desktop::Session<RemoteDesktop>), Opened> {
    let portal = RemoteDesktop::new().await?;
    let session = portal.create_session(Default::default()).await?;
    let path = token_path();
    let token = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());
    portal
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(ashpd::enumflags2::BitFlags::from_flag(DeviceType::Keyboard))
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(token.as_deref()),
        )
        .await?
        .response()?;
    let selected = portal
        .start(&session, None, StartOptions::default())
        .await?
        .response()?;
    if !selected.devices().contains(DeviceType::Keyboard) {
        return Err(Opened::Declined);
    }
    if let (Some(path), Some(token)) = (path, selected.restore_token()) {
        let _ = crate::settings::atomic_write(&path, token.as_bytes());
    }
    Ok((portal, session))
}

#[cfg(test)]
mod tests {
    // Failure modes: a timed-out request types later; a failed chord leaves Ctrl
    // held; a closed session accepts queued pastes; focus changes before typing.
    use super::super::{CONTROL_L, KEY_V, SHIFT_L, test_helpers::target};
    use super::*;

    fn request(wait: Duration) -> (Arc<Request>, mpsc::Receiver<Result<(), Refused>>) {
        let (reply, result) = mpsc::sync_channel(1);
        let request = Arc::new(Request {
            keysyms: vec![CONTROL_L, KEY_V],
            target: target("a"),
            deadline: Instant::now() + wait,
            state: Mutex::new(State::Queued),
            reply,
        });
        (request, result)
    }

    #[test]
    fn a_timed_out_portal_request_is_never_typed_later() {
        let (tx, rx) = mpsc::channel();
        // No worker yet: the caller gives up and falls back to copying.
        let started = Instant::now();
        let result = submit(
            &tx,
            &[CONTROL_L, KEY_V],
            &target("a"),
            Duration::from_millis(20),
        );
        assert!(matches!(result, Err(Refused::Failed(_))), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
        // The worker catches up, sees the dropped request and types nothing.
        drop(tx);
        let typed = Mutex::new(0);
        run_session(
            &rx,
            1,
            || Some(target("a")),
            |_| {
                *lock(&typed) += 1;
                Ok(())
            },
        );
        assert_eq!(*lock(&typed), 0);
    }

    #[test]
    fn a_request_that_expires_during_the_focus_query_is_not_typed() {
        let (request, result) = request(Duration::from_millis(20));
        let mut typed = false;
        serve(
            &request,
            || {
                // KWin answering slowly; meanwhile the caller gives up.
                *lock(&request.state) = State::Dropped;
                Some(target("a"))
            },
            |_| {
                typed = true;
                Ok(())
            },
        );
        assert!(!typed);
        assert!(matches!(result.try_recv(), Ok(Err(Refused::Failed(_)))));
    }

    #[test]
    fn the_worker_checks_focus_right_before_typing() {
        let (request, result) = request(Duration::from_secs(5));
        let mut typed = false;
        serve(
            &request,
            || Some(target("b")),
            |_| {
                typed = true;
                Ok(())
            },
        );
        assert!(!typed);
        assert_eq!(result.try_recv(), Ok(Err(Refused::FocusChanged)));
    }

    #[test]
    fn a_closed_session_rejects_queued_pastes() {
        let (tx, rx) = mpsc::channel();
        let (late, late_result) = request(Duration::from_secs(5));
        tx.send(Command::Closed(7)).expect("send");
        tx.send(Command::Paste(late.clone())).expect("send");
        let typed = Mutex::new(0);
        let end = run_session(
            &rx,
            7,
            || Some(target("a")),
            |_| {
                *lock(&typed) += 1;
                Ok(())
            },
        );
        assert_eq!(end, SessionEnd::Closed);
        reject_queued(&rx, "remote control ended");
        assert_eq!(*lock(&typed), 0);
        assert!(matches!(
            late_result.try_recv(),
            Ok(Err(Refused::Failed(_)))
        ));
        assert_eq!(*lock(&late.state), State::Dropped);
        // A close notice for an older session doesn't end the current one.
        tx.send(Command::Closed(6)).expect("send");
        drop(tx);
        assert_eq!(run_session(&rx, 7, || None, |_| Ok(())), SessionEnd::Failed);
    }

    #[test]
    fn a_caller_waits_out_a_paste_already_typing() {
        let (typing, result) = request(Duration::from_secs(5));
        *lock(&typing.state) = State::Typing;
        let worker = {
            let typing = typing.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(60));
                let _ = typing.reply.try_send(Ok(()));
            })
        };
        // The wait runs out mid-chord: reporting a copy now would be a lie.
        assert_eq!(
            await_reply(&typing, &result, Duration::from_millis(5)),
            Ok(())
        );
        worker.join().expect("typing");
        // Still queued when the wait runs out: dropped, and never typed after.
        let (queued, result) = request(Duration::from_secs(5));
        assert!(await_reply(&queued, &result, Duration::from_millis(5)).is_err());
        assert_eq!(*lock(&queued.state), State::Dropped);
    }

    type Log = Arc<Mutex<Vec<(u32, bool)>>>;
    fn fake_send(
        log: &Log,
        fail_on: Option<(u32, bool)>,
        hang_on: Option<(u32, bool)>,
    ) -> impl FnMut(u32, KeyState) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>>>>
    {
        let log = log.clone();
        move |key, state| {
            let down = state == KeyState::Pressed;
            let log = log.clone();
            Box::pin(async move {
                if hang_on == Some((key, down)) {
                    future::pending::<()>().await;
                }
                if fail_on == Some((key, down)) {
                    return Err("portal refused the key".into());
                }
                lock(&log).push((key, down));
                Ok(())
            })
        }
    }

    #[test]
    fn a_failed_press_releases_the_keys_already_down() {
        let log: Log = Arc::default();
        let keys = [CONTROL_L, SHIFT_L, KEY_V];
        let result = future::block_on(chord(
            &keys,
            fake_send(&log, Some((KEY_V, true)), None),
            Duration::from_secs(1),
        ));
        assert!(result.is_err());
        assert_eq!(
            *lock(&log),
            [
                (CONTROL_L, true),
                (SHIFT_L, true),
                (SHIFT_L, false),
                (CONTROL_L, false)
            ]
        );
    }

    #[test]
    fn a_stalled_chord_is_bounded_and_cleaned_up() {
        let log: Log = Arc::default();
        let started = Instant::now();
        let result = future::block_on(chord(
            &[CONTROL_L, KEY_V],
            fake_send(&log, None, Some((KEY_V, true))),
            Duration::from_millis(50),
        ));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(*lock(&log), [(CONTROL_L, true), (CONTROL_L, false)]);
    }

    #[test]
    fn a_failed_release_still_counts_as_pasted() {
        let log: Log = Arc::default();
        let result = future::block_on(chord(
            &[CONTROL_L, KEY_V],
            fake_send(&log, Some((KEY_V, false)), None),
            Duration::from_secs(1),
        ));
        assert_eq!(result, Ok(()));
        // Ctrl is still released even though releasing V failed.
        assert!(lock(&log).contains(&(CONTROL_L, false)));
    }
}
