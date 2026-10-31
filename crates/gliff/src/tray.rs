//! An icon in the desktop's tray (a StatusNotifierItem, which Omarchy's bar
//! and Waybar show) for while the window is hidden: a click on it brings the
//! window back.
//!
//! libdbus is blocking, so one thread owns the connection and answers the
//! tray's calls until the icon is dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use dbus::arg::{RefArg, Variant};
use dbus::blocking::Connection;
use dbus::channel::{MatchingReceiver, Sender as _};
use dbus::message::MatchRule;
use dbus::Message;

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const PATH: &str = "/StatusNotifierItem";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(200);

/// The icon, shown while this lives.
pub struct Tray {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Tray {
    /// Show the icon with `tooltip`; each click on it sends on `clicked`.
    /// Fails when nothing on the desktop shows tray icons.
    pub fn show(tooltip: String, clicked: Sender<()>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let stopped = stop.clone();
        let join = std::thread::Builder::new()
            .name("gliff-tray".into())
            .spawn(move || {
                let conn = match register(tooltip, clicked) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                while !stopped.load(Ordering::Relaxed) {
                    if let Err(e) = conn.process(POLL) {
                        tracing::warn!(error = %e, "tray icon");
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let tray = Self {
            stop,
            join: Some(join),
        };
        match ready_rx.recv_timeout(CALL_TIMEOUT * 2) {
            Ok(Ok(())) => Ok(tray),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("the session bus did not answer".into()),
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread wakes within one poll; closing its connection takes the
        // icon away.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Own a name on the session bus, answer the item's calls on it, and tell
/// the tray's watcher about it, again whenever a new watcher starts.
fn register(tooltip: String, clicked: Sender<()>) -> dbus::Result<Connection> {
    let conn = Connection::new_session()?;
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    conn.request_name(name.as_str(), false, true, false)?;
    conn.start_receive(
        MatchRule::new_method_call(),
        Box::new(move |msg, conn| {
            if let Some(reply) = answer(&msg, &tooltip, &clicked) {
                let _ = conn.send(reply);
            }
            true
        }),
    );
    let again = name.clone();
    conn.add_match(
        MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged"),
        move |(bus, _old, new): (String, String, String), conn, _| {
            if bus == WATCHER && !new.is_empty() {
                let _ = watch(conn, &again);
            }
            true
        },
    )?;
    watch(&conn, &name)?;
    Ok(conn)
}

fn watch(conn: &Connection, name: &str) -> dbus::Result<()> {
    conn.with_proxy(WATCHER, WATCHER_PATH, CALL_TIMEOUT)
        .method_call(WATCHER, "RegisterStatusNotifierItem", (name,))
}

/// The reply to a call on the item, or `None` for calls meant elsewhere.
fn answer(msg: &Message, tooltip: &str, clicked: &Sender<()>) -> Option<Message> {
    if msg.path().as_deref() != Some(PATH) {
        return None;
    }
    let interface = msg.interface();
    let member = msg.member();
    let (interface, member) = (interface.as_deref()?, member.as_deref()?);
    Some(match (interface, member) {
        (ITEM, "Activate" | "SecondaryActivate" | "ContextMenu") => {
            let _ = clicked.send(());
            msg.method_return()
        }
        (ITEM, "Scroll") => msg.method_return(),
        (PROPERTIES, "GetAll") => msg.method_return().append1(properties(tooltip)),
        (PROPERTIES, "Get") => {
            let (_, property): (&str, &str) = msg.read2().ok()?;
            match properties(tooltip).remove(property) {
                Some(value) => msg.method_return().append1(value),
                None => unknown(msg, "no such property"),
            }
        }
        ("org.freedesktop.DBus.Introspectable", "Introspect") => {
            msg.method_return().append1(INTROSPECTION)
        }
        ("org.freedesktop.DBus.Peer", "Ping") => msg.method_return(),
        _ => unknown(msg, "no such method"),
    })
}

fn unknown(msg: &Message, why: &str) -> Message {
    let name = dbus::strings::ErrorName::from("org.freedesktop.DBus.Error.UnknownMethod");
    let text = std::ffi::CString::new(why).expect("no NUL in the message");
    msg.error(&name, &text)
}

type Properties = std::collections::HashMap<&'static str, Variant<Box<dyn RefArg>>>;

fn properties(tooltip: &str) -> Properties {
    let text = |s: &str| Variant(Box::new(s.to_string()) as Box<dyn RefArg>);
    let no_pixmaps: Vec<(i32, i32, Vec<u8>)> = Vec::new();
    Properties::from([
        ("Category", text("ApplicationStatus")),
        ("Id", text("gliff")),
        ("Title", text("Gliff")),
        ("Status", text("Active")),
        ("IconName", text("gliff")),
        ("IconThemePath", text("")),
        ("ItemIsMenu", Variant(Box::new(false) as Box<dyn RefArg>)),
        ("WindowId", Variant(Box::new(0i32) as Box<dyn RefArg>)),
        (
            "ToolTip",
            Variant(Box::new((
                String::new(),
                no_pixmaps,
                "Gliff".to_string(),
                tooltip.to_string(),
            )) as Box<dyn RefArg>),
        ),
    ])
}

const INTROSPECTION: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="WindowId" type="i" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
  </interface>
</node>"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A call as it comes off the bus, which numbers every message: a reply
    /// needs that number.
    fn on(path: &str, interface: &str, member: &str) -> Message {
        let mut msg = Message::new_method_call("org.example", path, interface, member).unwrap();
        msg.set_serial(7);
        msg
    }

    fn call(member: &str) -> Message {
        on(PATH, ITEM, member)
    }

    #[test]
    fn a_click_on_the_icon_reaches_the_window() {
        let (tx, rx) = std::sync::mpsc::channel();
        let reply = answer(&call("Activate").append2(3i32, 4i32), "tip", &tx).unwrap();
        assert_eq!(reply.msg_type(), dbus::MessageType::MethodReturn);
        assert!(rx.try_recv().is_ok());
        answer(&call("Scroll").append2(1i32, "vertical"), "tip", &tx).unwrap();
        assert!(rx.try_recv().is_err(), "scrolling does not open the window");
    }

    #[test]
    fn answers_its_properties_and_nothing_else() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let get = on(PATH, PROPERTIES, "Get").append2(ITEM, "IconName");
        let reply = answer(&get, "tip", &tx).unwrap();
        let value: Variant<String> = reply.read1().unwrap();
        assert_eq!(value.0, "gliff");
        let all = on(PATH, PROPERTIES, "GetAll").append1(ITEM);
        assert_eq!(
            answer(&all, "tip", &tx).unwrap().msg_type(),
            dbus::MessageType::MethodReturn
        );
        let missing = on(PATH, PROPERTIES, "Get").append2(ITEM, "Nope");
        assert_eq!(
            answer(&missing, "tip", &tx).unwrap().msg_type(),
            dbus::MessageType::Error
        );
        let elsewhere = on("/other", ITEM, "Activate");
        assert!(answer(&elsewhere, "tip", &tx).is_none());
    }
}
