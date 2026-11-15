//! Best-effort caret context over AT-SPI, read only from the focused editable text
//! of the window that will receive the paste.
//!
//! The paste target names a process. The accessibility bus reports each
//! application's process from the bus daemon's own credentials, so text is read
//! only from that application, inside its one active window. Every node on the way
//! to the field must report a role and none may be a password field or terminal;
//! that, and the field's states and Text interface, are checked before any text is
//! requested.
//! Only the characters around the caret are read; selection, caret and clipboard
//! stay untouched. Anything uncertain yields no context, never a guess.
mod atspi;

use crate::{
    settings::Settings,
    transcribe::{auth::AuthMode, cleanup::MAX_CONTEXT_CHARS},
};
use futures_lite::future;
use std::{
    future::Future,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

const BEFORE_CHARS: i32 = 1_500;
const AFTER_CHARS: i32 = 500;
const CURSOR: &str = "[cursor]";
/// Covers connecting, finding the field and reading it. Dictation ends with an
/// upload that usually outlasts this, so waiting for it rarely delays the paste.
const DEADLINE: Duration = Duration::from_millis(750);
/// Bounds on the search for the focused field.
const MAX_APPLICATIONS: usize = 256;
const MAX_NODES: usize = 600;

const TEXT: &str = "org.a11y.atspi.Text";
const ROLE_PASSWORD_TEXT: u32 = 40;
const ROLE_TERMINAL: u32 = 60;
const STATE_ACTIVE: u32 = 1;
const STATE_DEFUNCT: u32 = 6;
const STATE_EDITABLE: u32 = 7;
const STATE_FOCUSED: u32 = 12;
const STATE_SHOWING: u32 = 25;
const STATE_STALE: u32 = 27;
const STATE_VISIBLE: u32 = 30;
const STATE_MANAGES_DESCENDANTS: u32 = 31;
const STATE_READ_ONLY: u32 = 43;

// A hung bus could outlive a dictation. Never accumulate more blocked threads.
static BUSY: AtomicBool = AtomicBool::new(false);
struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

pub struct Capture {
    result: Receiver<Option<String>>,
    deadline: Instant,
}
impl Capture {
    /// Waits no later than the capture's own deadline, which has usually passed
    /// while the recording uploaded.
    pub fn finish(self) -> Option<String> {
        self.result
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .ok()
            .flatten()
    }
}

/// Nearby text is read only when cleanup will send it.
pub fn wanted(settings: &Settings, mode: Option<AuthMode>) -> bool {
    settings.cleanup && settings.cleanup_context && mode == Some(AuthMode::ChatGpt)
}

/// Starts reading the text around the caret in process `pid`'s focused field.
pub fn begin(pid: u32) -> Option<Capture> {
    if pid == std::process::id() {
        return None;
    }
    let deadline = Instant::now() + DEADLINE;
    spawn(deadline, move || {
        future::block_on(within(deadline, async {
            read(&atspi::AtSpi::connect().await?, pid).await
        }))
    })
}

fn spawn(
    deadline: Instant,
    capture: impl FnOnce() -> Option<String> + Send + 'static,
) -> Option<Capture> {
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    let busy = Busy;
    let (tx, result) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("caret-context".into())
        .spawn(move || {
            let _busy = busy;
            let _ = tx.send(capture());
        })
        .ok()?;
    Some(Capture { result, deadline })
}

/// Drops `work`, and any D-Bus call it's waiting on, at `deadline`.
async fn within<T>(deadline: Instant, work: impl Future<Output = Option<T>>) -> Option<T> {
    future::or(work, async {
        async_io::Timer::at(deadline).await;
        None
    })
    .await
}

/// An accessible object: its application's connection on the bus and its path.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Node {
    bus: String,
    path: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct States([u32; 2]);
impl States {
    fn has(self, state: u32) -> bool {
        self.0
            .get(state as usize / 32)
            .is_some_and(|word| word & (1 << (state % 32)) != 0)
    }
    /// A focused, visible field that accepts typing.
    fn editable_field(self) -> bool {
        [STATE_FOCUSED, STATE_EDITABLE, STATE_SHOWING, STATE_VISIBLE]
            .iter()
            .all(|&s| self.has(s))
            && ![STATE_DEFUNCT, STATE_STALE, STATE_READ_ONLY]
                .iter()
                .any(|&s| self.has(s))
    }
}

/// The AT-SPI calls this module makes; `None` is any failure. Offsets are in
/// characters as the application counts them.
trait Tree {
    /// Applications registered on the accessibility bus.
    async fn applications(&self) -> Option<Vec<Node>>;
    /// The process behind an application's connection, as the bus daemon saw it.
    async fn pid(&self, application: &Node) -> Option<u32>;
    async fn children(&self, node: &Node) -> Option<Vec<Node>>;
    async fn states(&self, node: &Node) -> Option<States>;
    async fn role(&self, node: &Node) -> Option<u32>;
    async fn interfaces(&self, node: &Node) -> Option<Vec<String>>;
    async fn caret(&self, node: &Node) -> Option<i32>;
    async fn character_count(&self, node: &Node) -> Option<i32>;
    /// The first selection, or `None` inside `Some` when nothing is selected.
    async fn selection(&self, node: &Node) -> Option<Option<(i32, i32)>>;
    async fn text(&self, node: &Node, start: i32, end: i32) -> Option<String>;
}

async fn read(tree: &impl Tree, pid: u32) -> Option<String> {
    let mut matches = Vec::new();
    for application in tree
        .applications()
        .await?
        .into_iter()
        .take(MAX_APPLICATIONS)
    {
        if tree.pid(&application).await == Some(pid) {
            matches.push(application);
        }
    }
    let [application] = matches.as_slice() else {
        return None;
    };
    let mut active = Vec::new();
    for window in tree.children(application).await? {
        if tree
            .states(&window)
            .await
            .is_some_and(|s| s.has(STATE_ACTIVE))
        {
            active.push(window);
        }
    }
    let [window] = active.as_slice() else {
        return None;
    };
    let path = focused_path(tree, window).await?;
    let field = path.last()?;
    for node in &path {
        if matches!(tree.role(node).await?, ROLE_PASSWORD_TEXT | ROLE_TERMINAL) {
            return None;
        }
    }
    if !tree.states(field).await?.editable_field()
        || !tree.interfaces(field).await?.iter().any(|i| i == TEXT)
    {
        return None;
    }
    let caret = tree.caret(field).await?;
    let count = tree.character_count(field).await?;
    if !(0..=count).contains(&caret) {
        return None;
    }
    // Dictation replaces a selection that ends at the caret, so skip it.
    let (start, end) = match tree.selection(field).await? {
        Some((start, end))
            if 0 <= start && start < end && end <= count && (start == caret || end == caret) =>
        {
            (start, end)
        }
        _ => (caret, caret),
    };
    let before = tree
        .text(field, (start - BEFORE_CHARS).max(0), start)
        .await?;
    let after = tree
        .text(field, end, end.saturating_add(AFTER_CHARS).min(count))
        .await?;
    // Focus may have moved while reading.
    if !tree.states(field).await?.has(STATE_FOCUSED)
        || tree.caret(field).await? != caret
        || !tree.states(window).await?.has(STATE_ACTIVE)
    {
        return None;
    }
    around_caret(&before, &after)
}

/// Depth-first from the active window to its focused object, skipping hidden
/// subtrees and widgets that manage their own (often huge) descendants.
async fn focused_path(tree: &impl Tree, window: &Node) -> Option<Vec<Node>> {
    let mut nodes: Vec<(Node, Option<usize>)> = vec![(window.clone(), None)];
    let mut pending = vec![0];
    while let Some(index) = pending.pop() {
        let node = nodes[index].0.clone();
        let Some(states) = tree.states(&node).await else {
            continue;
        };
        if states.has(STATE_FOCUSED) {
            let mut path = Vec::new();
            let mut at = Some(index);
            while let Some(i) = at {
                path.push(nodes[i].0.clone());
                at = nodes[i].1;
            }
            path.reverse();
            return Some(path);
        }
        if !states.has(STATE_SHOWING) || states.has(STATE_MANAGES_DESCENDANTS) {
            continue;
        }
        let Some(children) = tree.children(&node).await else {
            continue;
        };
        for child in children.into_iter().rev() {
            if nodes.len() == MAX_NODES {
                return None;
            }
            pending.push(nodes.len());
            nodes.push((child, Some(index)));
        }
    }
    None
}

/// Up to `MAX_CONTEXT_CHARS` with the marker, whitespace kept as it is.
fn around_caret(before: &str, after: &str) -> Option<String> {
    if before.trim().is_empty() && after.trim().is_empty() {
        return None;
    }
    let skip = before.chars().count().saturating_sub(BEFORE_CHARS as usize);
    let before: String = before.chars().skip(skip).collect();
    let room = MAX_CONTEXT_CHARS - CURSOR.len() - before.chars().count();
    let after: String = after.chars().take(room.min(AFTER_CHARS as usize)).collect();
    Some(format!("{before}{CURSOR}{after}"))
}

#[cfg(test)]
mod tests {
    // Failure modes: text read from a password field, a field inside one, or a field
    // whose role or states can't be confirmed; any request before those checks;
    // context from another process, an ambiguous match or an inactive window;
    // focus or caret moving mid-read; byte offsets splitting characters; trimmed
    // whitespace; a selection included in the context; more than 2,000 characters;
    // an unbounded walk of a huge tree; a hung bus holding the worker; overlapping
    // workers piling up; reads when the user opted out.
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    const PID: u32 = 4242;
    const ROLE_ENTRY: u32 = 79;
    const ROLE_PANEL: u32 = 39;
    const FIELD: [u32; 4] = [STATE_FOCUSED, STATE_EDITABLE, STATE_SHOWING, STATE_VISIBLE];

    #[derive(Clone)]
    struct FakeNode {
        role: Option<u32>,
        states: Vec<u32>,
        children: Vec<String>,
        interfaces: Vec<&'static str>,
        text: Vec<char>,
        caret: i32,
        selection: Option<(i32, i32)>,
    }
    fn node(role: u32, states: &[u32], children: &[&str]) -> FakeNode {
        FakeNode {
            role: Some(role),
            states: states.to_vec(),
            children: children.iter().map(|c| c.to_string()).collect(),
            interfaces: Vec::new(),
            text: Vec::new(),
            caret: -1,
            selection: None,
        }
    }
    fn field(text: &str, caret: i32) -> FakeNode {
        FakeNode {
            interfaces: vec![TEXT],
            text: text.chars().collect(),
            caret,
            ..node(ROLE_ENTRY, &FIELD, &[])
        }
    }

    /// One application, `:1.7`, whose active window holds `panel` → `field`.
    struct Fake {
        apps: Vec<(&'static str, u32)>,
        nodes: RefCell<HashMap<String, FakeNode>>,
        calls: RefCell<Vec<String>>,
        hang: Option<&'static str>,
        /// Runs after each text read, standing in for the user moving on.
        after_text: Option<fn(&mut HashMap<String, FakeNode>)>,
    }
    impl Fake {
        fn new(field: FakeNode) -> Self {
            let nodes = [
                ("root", node(75, &[], &["window"])),
                (
                    "window",
                    node(23, &[STATE_ACTIVE, STATE_SHOWING], &["panel"]),
                ),
                ("panel", node(ROLE_PANEL, &[STATE_SHOWING], &["field"])),
                ("field", field),
            ];
            Self {
                apps: vec![(":1.7", PID)],
                nodes: RefCell::new(nodes.into_iter().map(|(k, v)| (k.into(), v)).collect()),
                calls: RefCell::new(Vec::new()),
                hang: None,
                after_text: None,
            }
        }
        fn with(self, path: &str, edit: impl FnOnce(&mut FakeNode)) -> Self {
            edit(self.nodes.borrow_mut().get_mut(path).expect("node"));
            self
        }
        async fn enter(&self, method: &str, node: &Node) -> Option<FakeNode> {
            self.calls
                .borrow_mut()
                .push(format!("{method} {}", node.path));
            if self.hang == Some(method) {
                future::pending::<()>().await;
            }
            self.nodes.borrow().get(&node.path).cloned()
        }
        fn called(&self, method: &str) -> bool {
            self.calls.borrow().iter().any(|c| c.starts_with(method))
        }
    }
    impl Tree for Fake {
        async fn applications(&self) -> Option<Vec<Node>> {
            Some(
                self.apps
                    .iter()
                    .map(|(bus, _)| Node {
                        bus: bus.to_string(),
                        path: "root".into(),
                    })
                    .collect(),
            )
        }
        async fn pid(&self, application: &Node) -> Option<u32> {
            self.calls.borrow_mut().push("pid".into());
            self.apps
                .iter()
                .find(|(bus, _)| *bus == application.bus)
                .map(|(_, pid)| *pid)
        }
        async fn children(&self, node: &Node) -> Option<Vec<Node>> {
            let fake = self.enter("children", node).await?;
            Some(
                fake.children
                    .into_iter()
                    .map(|path| Node {
                        bus: node.bus.clone(),
                        path,
                    })
                    .collect(),
            )
        }
        async fn states(&self, node: &Node) -> Option<States> {
            let mut states = States::default();
            for state in self.enter("states", node).await?.states {
                states.0[state as usize / 32] |= 1 << (state % 32);
            }
            Some(states)
        }
        async fn role(&self, node: &Node) -> Option<u32> {
            self.enter("role", node).await?.role
        }
        async fn interfaces(&self, node: &Node) -> Option<Vec<String>> {
            let fake = self.enter("interfaces", node).await?;
            Some(fake.interfaces.iter().map(|i| i.to_string()).collect())
        }
        async fn caret(&self, node: &Node) -> Option<i32> {
            Some(self.enter("caret", node).await?.caret)
        }
        async fn character_count(&self, node: &Node) -> Option<i32> {
            Some(self.enter("count", node).await?.text.len() as i32)
        }
        async fn selection(&self, node: &Node) -> Option<Option<(i32, i32)>> {
            Some(self.enter("selection", node).await?.selection)
        }
        async fn text(&self, node: &Node, start: i32, end: i32) -> Option<String> {
            let fake = self.enter("text", node).await?;
            if let Some(hook) = self.after_text {
                hook(&mut self.nodes.borrow_mut());
            }
            let clamp = |offset: i32| (offset.max(0) as usize).min(fake.text.len());
            Some(
                fake.text[clamp(start)..clamp(end).max(clamp(start))]
                    .iter()
                    .collect(),
            )
        }
    }
    fn run(fake: &Fake) -> Option<String> {
        future::block_on(read(fake, PID))
    }

    #[test]
    fn reads_whitespace_and_characters_around_the_caret() {
        let fake = Fake::new(field("  Hello  world\n", 7));
        assert_eq!(run(&fake).as_deref(), Some("  Hello[cursor]  world\n"));
        let fake = Fake::new(field("naïve 🦀 cafe\u{301}", 7));
        assert_eq!(run(&fake).as_deref(), Some("naïve 🦀[cursor] cafe\u{301}"));
        for caret in [0, 11] {
            let fake = Fake::new(field("Hello world", caret));
            let expected = if caret == 0 {
                "[cursor]Hello world"
            } else {
                "Hello world[cursor]"
            };
            assert_eq!(run(&fake).as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_selection_ending_at_the_caret_is_left_out() {
        let selected = |selection, caret| {
            Fake::new(field("Hello big world", caret))
                .with("field", |f| f.selection = Some(selection))
        };
        assert_eq!(
            run(&selected((6, 10), 10)).as_deref(),
            Some("Hello [cursor]world")
        );
        assert_eq!(
            run(&selected((6, 10), 6)).as_deref(),
            Some("Hello [cursor]world")
        );
        // Not at the caret: an unrelated or stale selection is ignored.
        assert_eq!(
            run(&selected((0, 5), 10)).as_deref(),
            Some("Hello big [cursor]world")
        );
        assert_eq!(
            run(&selected((6, 99), 6)).as_deref(),
            Some("Hello [cursor]big world")
        );
    }

    #[test]
    fn context_is_bounded_in_characters_with_the_marker() {
        let text = format!("{}{}", "é🦀".repeat(1_000), "ü".repeat(1_000));
        let fake = Fake::new(field(&text, 2_000));
        let context = run(&fake).expect("context");
        let (before, after) = context.split_once(CURSOR).expect("marker");
        assert_eq!(before, "é🦀".repeat(750));
        assert_eq!(after, "ü".repeat(MAX_CONTEXT_CHARS - CURSOR.len() - 1_500));
        assert_eq!(context.chars().count(), MAX_CONTEXT_CHARS);
        let short = around_caret("ab", &"c".repeat(900)).expect("context");
        assert_eq!(
            short.chars().count(),
            2 + CURSOR.len() + AFTER_CHARS as usize
        );
    }

    #[test]
    fn protected_or_unconfirmed_fields_are_rejected_before_any_text_request() {
        let cases: Vec<(&str, Fake)> = vec![
            (
                "password field",
                Fake::new(field("secret", 3)).with("field", |f| f.role = Some(ROLE_PASSWORD_TEXT)),
            ),
            (
                "inside a password field",
                Fake::new(field("secret", 3)).with("panel", |f| f.role = Some(ROLE_PASSWORD_TEXT)),
            ),
            (
                "unknown field role",
                Fake::new(field("secret", 3)).with("field", |f| f.role = None),
            ),
            (
                "unknown ancestor role",
                Fake::new(field("secret", 3)).with("panel", |f| f.role = None),
            ),
            (
                "terminal",
                Fake::new(field("secret", 3)).with("field", |f| f.role = Some(ROLE_TERMINAL)),
            ),
            (
                "inside a terminal",
                Fake::new(field("secret", 3)).with("panel", |f| f.role = Some(ROLE_TERMINAL)),
            ),
            (
                "not editable",
                Fake::new(field("secret", 3))
                    .with("field", |f| f.states.retain(|&s| s != STATE_EDITABLE)),
            ),
            (
                "read only",
                Fake::new(field("secret", 3)).with("field", |f| f.states.push(STATE_READ_ONLY)),
            ),
            (
                "invisible",
                Fake::new(field("secret", 3))
                    .with("field", |f| f.states.retain(|&s| s != STATE_VISIBLE)),
            ),
            (
                "defunct",
                Fake::new(field("secret", 3)).with("field", |f| f.states.push(STATE_DEFUNCT)),
            ),
            (
                "no Text interface",
                Fake::new(field("secret", 3)).with("field", |f| f.interfaces.clear()),
            ),
            ("unknown caret", Fake::new(field("secret", -1))),
            ("caret past the end", Fake::new(field("secret", 7))),
        ];
        for (case, fake) in cases {
            assert_eq!(run(&fake), None, "{case}");
            assert!(!fake.called("text"), "{case}: text was requested");
        }
    }

    #[test]
    fn only_the_target_process_and_its_one_active_window_are_read() {
        let mut other = Fake::new(field("Hello", 5));
        other.apps = vec![(":1.7", PID + 1)];
        assert_eq!(run(&other), None);
        // Only the bus daemon was asked; the other application never was.
        assert!(other.calls.borrow().iter().all(|c| c == "pid"));

        let mut ambiguous = Fake::new(field("Hello", 5));
        ambiguous.apps.push((":1.8", PID));
        assert_eq!(run(&ambiguous), None);
        assert!(!ambiguous.called("children"));

        let inactive =
            Fake::new(field("Hello", 5)).with("window", |w| w.states = vec![STATE_SHOWING]);
        assert_eq!(run(&inactive), None);
        assert!(!inactive.called("role"));

        let two = Fake::new(field("Hello", 5)).with("root", |r| r.children.push("window".into()));
        assert_eq!(run(&two), None);
        assert!(!two.called("role"));

        assert!(begin(std::process::id()).is_none(), "WispX itself");
    }

    #[test]
    fn focus_or_caret_moving_during_the_read_discards_it() {
        let mut moved = Fake::new(field("Hello world", 5));
        moved.after_text = Some(|nodes| {
            nodes
                .get_mut("field")
                .unwrap()
                .states
                .retain(|&s| s != STATE_FOCUSED)
        });
        assert_eq!(run(&moved), None);
        let mut typed = Fake::new(field("Hello world", 5));
        typed.after_text = Some(|nodes| nodes.get_mut("field").unwrap().caret = 6);
        assert_eq!(run(&typed), None);
        let mut switched = Fake::new(field("Hello world", 5));
        switched.after_text = Some(|nodes| nodes.get_mut("window").unwrap().states.clear());
        assert_eq!(run(&switched), None);
    }

    #[test]
    fn blank_fields_give_no_context() {
        assert_eq!(run(&Fake::new(field("", 0))), None);
        assert_eq!(run(&Fake::new(field(" \n\t", 1))), None);
    }

    #[test]
    fn the_search_is_bounded_and_skips_hidden_or_managed_subtrees() {
        let wide = Fake::new(field("Hello", 5)).with("panel", |p| {
            p.children = (0..5_000).map(|i| format!("leaf{i}")).collect();
            p.children.push("field".into());
        });
        assert_eq!(run(&wide), None);
        assert!(wide.calls.borrow().len() <= 2 * MAX_NODES + 8);
        assert!(!wide.called("text"));

        for hidden in [vec![], vec![STATE_SHOWING, STATE_MANAGES_DESCENDANTS]] {
            let fake = Fake::new(field("Hello", 5)).with("panel", |p| p.states = hidden);
            assert_eq!(run(&fake), None);
            assert!(!fake.called("states field"));
        }
    }

    #[test]
    fn a_hung_bus_ends_at_the_deadline() {
        for method in ["children", "states", "role", "text"] {
            let mut fake = Fake::new(field("Hello", 5));
            fake.hang = Some(method);
            let started = Instant::now();
            let deadline = started + Duration::from_millis(50);
            assert_eq!(future::block_on(within(deadline, read(&fake, PID))), None);
            assert!(started.elapsed() < Duration::from_secs(1), "{method}");
        }
    }

    #[test]
    fn one_worker_at_a_time_and_finish_waits_only_until_the_deadline() {
        let (release, hold) = mpsc::channel::<()>();
        let deadline = Instant::now() + Duration::from_millis(30);
        let hung = spawn(deadline, move || {
            let _ = hold.recv();
            Some("late".into())
        })
        .expect("first worker");
        assert!(spawn(Instant::now(), || None).is_none(), "second worker");
        let started = Instant::now();
        assert_eq!(hung.finish(), None);
        assert!(started.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        assert!(
            (0..200).any(|_| {
                std::thread::sleep(Duration::from_millis(5));
                !BUSY.load(Ordering::Acquire)
            }),
            "the worker released its slot"
        );
        let ready = spawn(Instant::now() + Duration::from_secs(5), || {
            Some("text".into())
        })
        .expect("next worker");
        assert_eq!(ready.finish().as_deref(), Some("text"));
    }

    #[test]
    fn nothing_is_read_unless_cleanup_will_send_it() {
        let on = Settings::default();
        assert!(wanted(&on, Some(AuthMode::ChatGpt)));
        for mode in [None, Some(AuthMode::ApiKey), Some(AuthMode::Unknown)] {
            assert!(!wanted(&on, mode));
        }
        let off = |edit: fn(&mut Settings)| {
            let mut settings = Settings::default();
            edit(&mut settings);
            wanted(&settings, Some(AuthMode::ChatGpt))
        };
        assert!(!off(|s| s.cleanup_context = false));
        assert!(!off(|s| s.cleanup = false));
    }

    #[test]
    fn context_joins_the_transcript_at_the_caret() {
        use crate::transcribe::cleanup::join_context;
        let fake = Fake::new(field("Testing the", 11));
        let context = run(&fake).expect("context");
        assert_eq!(join_context("system.".into(), &context), " system.");
        let fake = Fake::new(field("Testing the ", 12));
        assert_eq!(
            join_context("system.".into(), &run(&fake).unwrap()),
            "system."
        );
    }
}
