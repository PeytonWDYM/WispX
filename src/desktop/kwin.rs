//! Asks KWin which window has focus.
//!
//! Wayland gives programs no way to ask this, by design. KWin's scripting API
//! can answer it: a tiny script reads `workspace.activeWindow` and reports back
//! over D-Bus. Nothing about the script persists, and it reads a single property.

use crate::dbus::{Connection, Error, Kind, Value};
use crate::paths;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const BUS_NAME: &str = "org.kde.KWin";
const SCRIPTING_PATH: &str = "/Scripting";
const SCRIPTING_INTERFACE: &str = "org.kde.kwin.Scripting";
const SCRIPT_INTERFACE: &str = "org.kde.kwin.Script";
const REPORT_METHOD: &str = "result";
const TIMEOUT: Duration = Duration::from_secs(2);

static NEXT_SCRIPT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FocusInfo {
    /// Unique for each window, unlike its title or class.
    pub id: String,
    pub class: String,
}

fn script_source(unique_name: &str) -> String {
    format!(
        "const w = workspace.activeWindow;\n\
         callDBus(\"{unique_name}\", \"/\", \"\", \"{REPORT_METHOD}\", \
         w ? String(w.internalId) : \"\", w ? String(w.resourceClass) : \"\");\n"
    )
}

/// The window that has focus right now, or `None` when no window does.
pub fn active_window(runtime: &Path) -> Result<Option<FocusInfo>, String> {
    let mut bus = Connection::session().map_err(|error| error.to_string())?;
    // Bus-assigned, but this gets pasted into source code, so make sure of it.
    let unique_name = bus.unique_name().to_string();
    if !unique_name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b":._-".contains(&byte))
    {
        return Err("unexpected D-Bus name".to_string());
    }

    let serial = NEXT_SCRIPT.fetch_add(1, Ordering::SeqCst);
    let name = format!("wispx-focus-{}-{serial}", std::process::id());
    let path = runtime.join(format!("{name}.js"));
    write_script(&path, &script_source(&unique_name))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    let result = run_script(&mut bus, &path, &name);
    // KWin has read the script by now; it is removed whatever the outcome.
    let _ = bus.call(
        BUS_NAME,
        SCRIPTING_PATH,
        SCRIPTING_INTERFACE,
        "unloadScript",
        vec![Value::str(name)],
        TIMEOUT,
    );
    let _ = fs::remove_file(&path);
    result.map_err(|error| error.to_string())
}

fn write_script(path: &Path, source: &str) -> std::io::Result<()> {
    paths::create_private_file(path)?.write_all(source.as_bytes())
}

fn run_script(bus: &mut Connection, path: &Path, name: &str) -> Result<Option<FocusInfo>, Error> {
    let reply = bus.call(
        BUS_NAME,
        SCRIPTING_PATH,
        SCRIPTING_INTERFACE,
        "loadScript",
        vec![Value::str(path.to_string_lossy()), Value::str(name)],
        TIMEOUT,
    )?;
    let Some(id) = reply.first().and_then(Value::as_i32).filter(|id| *id >= 0) else {
        return Err(Error::Protocol("KWin did not load the script".to_string()));
    };
    bus.call(
        BUS_NAME,
        &format!("{SCRIPTING_PATH}/Script{id}"),
        SCRIPT_INTERFACE,
        "run",
        Vec::new(),
        TIMEOUT,
    )?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(message) = bus.next_message(remaining)? else {
            return Err(Error::Timeout);
        };
        if message.kind == Kind::MethodCall && message.member.as_deref() == Some(REPORT_METHOD) {
            let text = |index: usize| {
                message
                    .body
                    .get(index)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let (id, class) = (text(0), text(1));
            let _ = bus.reply(&message, Vec::new());
            return Ok((!id.is_empty()).then_some(FocusInfo { id, class }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_reports_to_our_bus_name_and_handles_no_focused_window() {
        let source = script_source(":1.42");
        assert!(
            source.contains(r#"callDBus(":1.42", "/", "", "result","#),
            "{source}"
        );
        assert!(source.contains("workspace.activeWindow"), "{source}");
        assert!(
            source.contains(r#"w ? String(w.internalId) : """#),
            "{source}"
        );
    }
}
