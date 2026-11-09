//! Asks KWin which window has focus. Wayland deliberately gives clients no such
//! query; KWin's scripting API does, and a one-line script reports it back over
//! D-Bus. The script is unloaded and deleted immediately.
use super::instance::runtime_dir;
use futures_lite::{StreamExt, future};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use zbus::message::Type as MessageType;

const TIMEOUT: Duration = Duration::from_secs(2);
const REPORT_PATH: &str = "/io/github/PeytonWDYM/WispX/Focus";
const REPORT_INTERFACE: &str = "io.github.PeytonWDYM.WispX.Focus";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    /// KWin's per-window UUID, unlike titles or classes unique to each window.
    pub id: String,
    pub class: String,
}

fn script(unique_name: &str) -> String {
    format!(
        "const w = workspace.activeWindow !== undefined ? workspace.activeWindow : workspace.activeClient;\n\
         callDBus(\"{unique_name}\", \"{REPORT_PATH}\", \"{REPORT_INTERFACE}\", \"Report\", \
         w ? String(w.internalId) : \"\", w ? String(w.resourceClass) : \"\");\n"
    )
}

/// The focused window, `Ok(None)` when nothing has focus, or an error off Plasma.
pub fn active_window() -> Result<Option<Window>, String> {
    future::block_on(query()).map_err(|e| e.to_string())
}

async fn query() -> Result<Option<Window>, Box<dyn std::error::Error>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let bus = zbus::Connection::session().await?;
    let unique = bus.unique_name().ok_or("no D-Bus name")?.to_string();
    // The bus assigns this name, but it's pasted into script source, so check it.
    if !unique
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b":._-".contains(&b))
    {
        return Err("unexpected D-Bus name".into());
    }
    let mut messages = zbus::MessageStream::from(&bus);
    let name = format!(
        "wispx-focus-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = runtime_dir()?.join(format!("{name}.js"));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?
        .write_all(script(&unique).as_bytes())?;
    let exchange = async {
        let id: i32 = bus
            .call_method(
                Some("org.kde.KWin"),
                "/Scripting",
                Some("org.kde.kwin.Scripting"),
                "loadScript",
                &(path.to_string_lossy().as_ref(), name.as_str()),
            )
            .await?
            .body()
            .deserialize()?;
        if id < 0 {
            return Err("KWin refused the focus script".into());
        }
        // Plasma 6 names script objects /Scripting/ScriptN; Plasma 5 used /N.
        let mut ran = false;
        for object in [format!("/Scripting/Script{id}"), format!("/{id}")] {
            if bus
                .call_method(
                    Some("org.kde.KWin"),
                    object.as_str(),
                    Some("org.kde.kwin.Script"),
                    "run",
                    &(),
                )
                .await
                .is_ok()
            {
                ran = true;
                break;
            }
        }
        if !ran {
            return Err("KWin did not run the focus script".into());
        }
        while let Some(message) = messages.next().await {
            let message = message?;
            let header = message.header();
            if header.message_type() == MessageType::MethodCall
                && header.member().is_some_and(|m| m.as_str() == "Report")
                && header.path().is_some_and(|p| p.as_str() == REPORT_PATH)
            {
                let (id, class): (String, String) = message.body().deserialize()?;
                return Ok((!id.is_empty()).then_some(Window { id, class }));
            }
        }
        Err::<_, Box<dyn std::error::Error>>("D-Bus connection closed".into())
    };
    // The timeout covers only the exchange, so the script is always cleaned up.
    let result = future::or(exchange, async {
        async_io::Timer::after(TIMEOUT).await;
        Err("KWin did not answer".into())
    })
    .await;
    let _ = bus
        .call_method(
            Some("org.kde.KWin"),
            "/Scripting",
            Some("org.kde.kwin.Scripting"),
            "unloadScript",
            &(name.as_str(),),
        )
        .await;
    let _ = fs::remove_file(&path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_reports_to_our_name_and_tolerates_no_focus() {
        let source = script(":1.42");
        assert!(source.contains(&format!(
            "callDBus(\":1.42\", \"{REPORT_PATH}\", \"{REPORT_INTERFACE}\", \"Report\","
        )));
        assert!(source.contains("w ? String(w.internalId) : \"\""));
        assert!(source.contains("workspace.activeClient"));
    }
}
