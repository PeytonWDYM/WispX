//! Desktop notifications stand in for the Windows tray balloon.
use super::APP_ID;
use std::collections::HashMap;
use zbus::zvariant::Value;

/// Sends a notification from a worker thread; a missing notification daemon is ignored.
pub fn show(title: &str, message: &str, milliseconds: i32) {
    let (title, message) = (title.to_owned(), message.to_owned());
    std::thread::spawn(move || {
        let result = (|| -> zbus::Result<()> {
            let bus = zbus::blocking::Connection::session()?;
            let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
            hints.insert("desktop-entry", Value::from(APP_ID));
            bus.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    "WispX",
                    0_u32,
                    APP_ID,
                    title.as_str(),
                    message.as_str(),
                    Vec::<&str>::new(),
                    hints,
                    milliseconds,
                ),
            )?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("Notification: {error}");
        }
    });
}
