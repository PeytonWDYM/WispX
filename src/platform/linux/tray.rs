//! StatusNotifierItem tray (Plasma natively; GNOME with the AppIndicator extension).
//! Left click opens Options, like the Windows notification icon.
use super::{APP_ID, notify};
use crate::transcribe::auth;
use ksni::{
    MenuItem, ToolTip,
    blocking::{Handle, TrayMethods},
    menu::StandardItem,
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    Options,
    CopyLast,
    Help,
    Quit,
}
type Sink = Arc<dyn Fn(TrayAction) + Send + Sync>;
struct Item {
    hotkey: String,
    has_transcript: bool,
    sink: Sink,
}
pub struct Tray {
    handle: Option<Handle<Item>>,
}
/// The bundled PNG as SNI's ARGB32 pixmap, for hosts without the installed icon.
fn pixmap() -> Vec<ksni::Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(
        &include_bytes!("../../../assets/wispx.png")[..],
    ));
    let Ok(mut reader) = decoder.read_info() else {
        return Vec::new();
    };
    let Some(size) = reader.output_buffer_size() else {
        return Vec::new();
    };
    let mut buffer = vec![0; size];
    let Ok(info) = reader.next_frame(&mut buffer) else {
        return Vec::new();
    };
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Vec::new();
    }
    let data = buffer[..info.buffer_size()]
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect();
    vec![ksni::Icon {
        width: info.width as i32,
        height: info.height as i32,
        data,
    }]
}
impl ksni::Tray for Item {
    fn id(&self) -> String {
        "wispx".into()
    }
    fn title(&self) -> String {
        "WispX".into()
    }
    fn icon_name(&self) -> String {
        APP_ID.into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        pixmap()
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: format!("WispX · hold {}", self.hotkey),
            ..ToolTip::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        (self.sink)(TrayAction::Options);
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let action = |label: &str, enabled: bool, action: TrayAction| {
            StandardItem {
                label: label.into(),
                enabled,
                activate: Box::new(move |item: &mut Self| (item.sink)(action)),
                ..StandardItem::default()
            }
            .into()
        };
        let note = |label: String| {
            StandardItem {
                label,
                enabled: false,
                ..StandardItem::default()
            }
            .into()
        };
        vec![
            note(format!("WispX · hold {}", self.hotkey)),
            note(
                if auth::login().is_some() {
                    "✓ Using your ChatGPT login from Codex"
                } else {
                    "⚠ Not signed in. Open Codex and sign in"
                }
                .into(),
            ),
            MenuItem::Separator,
            action(
                "Copy last transcript",
                self.has_transcript,
                TrayAction::CopyLast,
            ),
            action("Options…", true, TrayAction::Options),
            MenuItem::Separator,
            action("How to use", true, TrayAction::Help),
            action("Quit WispX", true, TrayAction::Quit),
        ]
    }
}
impl Tray {
    /// `hotkey` is the text the desktop shows for the hold shortcut.
    pub fn new(hotkey: String, sink: Sink) -> Self {
        let item = Item {
            hotkey,
            has_transcript: false,
            sink,
        };
        let handle = match item.spawn() {
            Ok(handle) => Some(handle),
            Err(error) => {
                eprintln!("Tray: {error}. Launch WispX again to open Options.");
                None
            }
        };
        Self { handle }
    }
    pub fn available(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_closed())
    }
    pub fn update(&self, hotkey: String, has_transcript: bool) {
        if let Some(handle) = &self.handle {
            handle.update(|item| {
                item.hotkey = hotkey;
                item.has_transcript = has_transcript;
            });
        }
    }
    pub fn balloon(&self, title: &str, message: &str, milliseconds: i32) {
        notify::show(title, message, milliseconds);
    }
}
impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().wait();
        }
    }
}
