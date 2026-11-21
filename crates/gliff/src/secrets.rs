//! Passwords for RDP and RustDesk machines, kept in the desktop's keyring
//! through the Secret Service (gnome-keyring, KeePassXC, KWallet). Each is
//! an item labelled for gliff, found by the machine's name, with the user
//! name beside it.
//!
//! The calls block, and unlocking the keyring may show its own prompt: run
//! them off the GTK thread.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dbus::arg::{PropMap, RefArg, Variant};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties as _;
use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus::Path;

const BUS_NAME: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
const PROMPT: &str = "org.freedesktop.Secret.Prompt";
/// A call; the keyring answers at once unless something is wrong.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the user may take over the keyring's own unlock prompt.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// The attribute every gliff item carries.
const APPLICATION: (&str, &str) = ("application", "gliff");

/// A saved login.
#[derive(Clone, PartialEq, Eq)]
pub struct Saved {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Saved").field("user", &self.user).finish()
    }
}

/// What `machine` was last saved with, if anything.
pub fn load(machine: &str) -> anyhow::Result<Option<Saved>> {
    let k = Keyring::open()?;
    let Some(item) = k.find(machine)? else {
        return Ok(None);
    };
    let proxy = k.conn.with_proxy(BUS_NAME, item, CALL_TIMEOUT);
    let ((_session, _params, value, _type),): ((Path, Vec<u8>, Vec<u8>, String),) =
        proxy.method_call(ITEM, "GetSecret", (k.session.clone(),))?;
    let attributes: HashMap<String, String> = proxy.get(ITEM, "Attributes")?;
    Ok(Some(Saved {
        user: attributes.get("user").cloned().unwrap_or_default(),
        password: String::from_utf8(value)?,
    }))
}

/// Save `saved` for `machine`, replacing what was there.
pub fn store(machine: &str, saved: &Saved) -> anyhow::Result<()> {
    let k = Keyring::open()?;
    let service = k.conn.with_proxy(BUS_NAME, SERVICE_PATH, CALL_TIMEOUT);
    let (mut collection,): (Path,) = service.method_call(SERVICE, "ReadAlias", ("default",))?;
    if &*collection == "/" {
        collection = Path::from("/org/freedesktop/secrets/aliases/default");
    }
    k.unlock(vec![collection.clone()])?;
    let mut attributes = attributes(machine);
    attributes.insert("user".into(), saved.user.clone());
    let mut props: PropMap = HashMap::new();
    props.insert(
        "org.freedesktop.Secret.Item.Label".into(),
        Variant(Box::new(format!("gliff: {machine}")) as Box<dyn RefArg>),
    );
    props.insert(
        "org.freedesktop.Secret.Item.Attributes".into(),
        Variant(Box::new(attributes) as Box<dyn RefArg>),
    );
    let secret = (
        k.session.clone(),
        Vec::<u8>::new(),
        saved.password.as_bytes().to_vec(),
        "text/plain".to_string(),
    );
    let proxy = k.conn.with_proxy(BUS_NAME, collection, CALL_TIMEOUT);
    let (_item, prompt): (Path, Path) =
        proxy.method_call(COLLECTION, "CreateItem", (props, secret, true))?;
    k.prompt(prompt)?;
    Ok(())
}

/// Forget what was saved for `machine`.
pub fn delete(machine: &str) -> anyhow::Result<()> {
    let k = Keyring::open()?;
    while let Some(item) = k.find(machine)? {
        let proxy = k.conn.with_proxy(BUS_NAME, item, CALL_TIMEOUT);
        let (prompt,): (Path<'static>,) = proxy.method_call(ITEM, "Delete", ())?;
        if !k.prompt(prompt)? {
            break;
        }
    }
    Ok(())
}

fn attributes(machine: &str) -> HashMap<String, String> {
    HashMap::from([
        (APPLICATION.0.into(), APPLICATION.1.into()),
        ("machine".into(), machine.into()),
    ])
}

struct Keyring {
    conn: Connection,
    session: Path<'static>,
}

impl Keyring {
    fn open() -> anyhow::Result<Self> {
        let conn = Connection::new_session()?;
        let service = conn.with_proxy(BUS_NAME, SERVICE_PATH, CALL_TIMEOUT);
        // "plain": the secret crosses the session bus, which only this user
        // reaches, as libsecret's default does too on most desktops.
        let (_output, session): (Variant<Box<dyn RefArg>>, Path) = service
            .method_call(SERVICE, "OpenSession", ("plain", Variant("")))
            .map_err(|e| anyhow::anyhow!("no keyring answers ({e})"))?;
        Ok(Self { conn, session })
    }

    /// The first item for `machine`, unlocked.
    fn find(&self, machine: &str) -> anyhow::Result<Option<Path<'static>>> {
        let service = self.conn.with_proxy(BUS_NAME, SERVICE_PATH, CALL_TIMEOUT);
        let (unlocked, locked): (Vec<Path<'static>>, Vec<Path<'static>>) =
            service.method_call(SERVICE, "SearchItems", (attributes(machine),))?;
        if let Some(item) = unlocked.into_iter().next() {
            return Ok(Some(item));
        }
        let Some(item) = locked.into_iter().next() else {
            return Ok(None);
        };
        Ok(self.unlock(vec![item.clone()])?.then_some(item))
    }

    /// Unlock `objects`, through the keyring's prompt if it asks; false when
    /// the user dismissed it.
    fn unlock(&self, objects: Vec<Path<'static>>) -> anyhow::Result<bool> {
        let service = self.conn.with_proxy(BUS_NAME, SERVICE_PATH, CALL_TIMEOUT);
        let (_done, prompt): (Vec<Path<'static>>, Path<'static>) =
            service.method_call(SERVICE, "Unlock", (objects,))?;
        self.prompt(prompt)
    }

    /// Show `prompt`, unless it is "/" (none needed), and wait for the user;
    /// false when they dismissed it.
    fn prompt(&self, prompt: Path<'static>) -> anyhow::Result<bool> {
        if &*prompt == "/" {
            return Ok(true);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let rule = MatchRule::new_signal(PROMPT, "Completed").with_path(prompt.clone());
        let token = self.conn.add_match(
            rule,
            move |(dismissed, _): (bool, Variant<Box<dyn RefArg>>), _, _| {
                let _ = tx.send(dismissed);
                false
            },
        )?;
        let proxy = self.conn.with_proxy(BUS_NAME, prompt, CALL_TIMEOUT);
        proxy.method_call::<(), _, _, _>(PROMPT, "Prompt", ("",))?;
        let deadline = Instant::now() + PROMPT_TIMEOUT;
        let dismissed = loop {
            if let Ok(dismissed) = rx.try_recv() {
                break dismissed;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the keyring prompt was left open"
            );
            self.conn.process(Duration::from_millis(200))?;
        };
        let _ = self.conn.remove_match(token);
        Ok(!dismissed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_logins_hide_the_password_when_printed() {
        let s = Saved {
            user: "ann".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn items_are_found_by_application_and_machine() {
        let a = attributes("rdp://pc");
        assert_eq!(a.get("application").map(String::as_str), Some("gliff"));
        assert_eq!(a.get("machine").map(String::as_str), Some("rdp://pc"));
    }

    /// Against the real keyring, when one runs: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn round_trips_through_the_keyring() {
        let machine = format!("rdp://gliff-test-{}", std::process::id());
        let saved = Saved {
            user: "ann".into(),
            password: "pässword".into(),
        };
        store(&machine, &saved).unwrap();
        assert_eq!(load(&machine).unwrap(), Some(saved));
        delete(&machine).unwrap();
        assert_eq!(load(&machine).unwrap(), None);
    }
}
