//! The accessibility bus, reached through its launcher on the session bus.
use super::{Node, States, Tree};
use serde::{Serialize, de::DeserializeOwned};
use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Type};

const LAUNCHER: &str = "org.a11y.Bus";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const TEXT: &str = "org.a11y.atspi.Text";
/// What toolkits return for a missing object.
const NULL_PATH: &str = "/org/a11y/atspi/null";

pub(super) struct AtSpi {
    bus: zbus::Connection,
}
impl AtSpi {
    pub(super) async fn connect() -> Option<Self> {
        let session = zbus::Connection::session().await.ok()?;
        // Calling the launcher would start it, but applications only join the
        // accessibility bus at startup, so a fresh one would be empty anyway.
        let running: bool = call(
            &session,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            &(LAUNCHER,),
        )
        .await?;
        if !running {
            return None;
        }
        let address: String = call(
            &session,
            LAUNCHER,
            "/org/a11y/bus",
            LAUNCHER,
            "GetAddress",
            &(),
        )
        .await?;
        let bus = zbus::connection::Builder::address(address.as_str())
            .ok()?
            .build()
            .await
            .ok()?;
        Some(Self { bus })
    }
    async fn call<R>(
        &self,
        node: &Node,
        interface: &str,
        method: &str,
        body: &(impl Serialize + DynamicType),
    ) -> Option<R>
    where
        R: DeserializeOwned + Type,
    {
        call(&self.bus, &node.bus, &node.path, interface, method, body).await
    }
    async fn property(&self, node: &Node, interface: &str, name: &str) -> Option<OwnedValue> {
        self.call(
            node,
            "org.freedesktop.DBus.Properties",
            "Get",
            &(interface, name),
        )
        .await
    }
}

async fn call<R>(
    bus: &zbus::Connection,
    destination: &str,
    path: &str,
    interface: &str,
    method: &str,
    body: &(impl Serialize + DynamicType),
) -> Option<R>
where
    R: DeserializeOwned + Type,
{
    bus.call_method(Some(destination), path, Some(interface), method, body)
        .await
        .ok()?
        .body()
        .deserialize()
        .ok()
}

fn nodes(references: Vec<(String, OwnedObjectPath)>) -> Vec<Node> {
    references
        .into_iter()
        .filter(|(bus, path)| !bus.is_empty() && path.as_str() != NULL_PATH)
        .map(|(bus, path)| Node {
            bus,
            path: path.to_string(),
        })
        .collect()
}

impl Tree for AtSpi {
    async fn applications(&self) -> Option<Vec<Node>> {
        let registry = Node {
            bus: "org.a11y.atspi.Registry".into(),
            path: "/org/a11y/atspi/accessible/root".into(),
        };
        Some(nodes(
            self.call(&registry, ACCESSIBLE, "GetChildren", &()).await?,
        ))
    }
    async fn pid(&self, application: &Node) -> Option<u32> {
        // Only unique names are applications' own connections.
        if !application.bus.starts_with(':') {
            return None;
        }
        call(
            &self.bus,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            &(application.bus.as_str(),),
        )
        .await
    }
    async fn children(&self, node: &Node) -> Option<Vec<Node>> {
        // A child in another application (an embedded plug) would escape the
        // matched process.
        let children = nodes(self.call(node, ACCESSIBLE, "GetChildren", &()).await?);
        Some(children.into_iter().filter(|c| c.bus == node.bus).collect())
    }
    async fn states(&self, node: &Node) -> Option<States> {
        let words: Vec<u32> = self.call(node, ACCESSIBLE, "GetState", &()).await?;
        Some(States([
            words.first().copied()?,
            words.get(1).copied().unwrap_or(0),
        ]))
    }
    async fn role(&self, node: &Node) -> Option<u32> {
        self.call(node, ACCESSIBLE, "GetRole", &()).await
    }
    async fn interfaces(&self, node: &Node) -> Option<Vec<String>> {
        self.call(node, ACCESSIBLE, "GetInterfaces", &()).await
    }
    async fn caret(&self, node: &Node) -> Option<i32> {
        i32::try_from(self.property(node, TEXT, "CaretOffset").await?).ok()
    }
    async fn character_count(&self, node: &Node) -> Option<i32> {
        i32::try_from(self.property(node, TEXT, "CharacterCount").await?).ok()
    }
    async fn selection(&self, node: &Node) -> Option<Option<(i32, i32)>> {
        let count: i32 = self.call(node, TEXT, "GetNSelections", &()).await?;
        if count <= 0 {
            return Some(None);
        }
        Some(Some(
            self.call(node, TEXT, "GetSelection", &(0_i32,)).await?,
        ))
    }
    async fn text(&self, node: &Node, start: i32, end: i32) -> Option<String> {
        self.call(node, TEXT, "GetText", &(start, end)).await
    }
}
