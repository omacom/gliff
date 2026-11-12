//! Machines that run a Remote Desktop (RDP) server, RustDesk or a VNC
//! server instead of gliff: tell them apart ([`probe`]), then show their screen and drive
//! them with the same input gliff sends its own servers ([`run`]).
//!
//! Input arrives as gliff's own [`ClientMsg`]: evdev key codes, pointer
//! positions in the other machine's pixels, evdev buttons and Wayland scroll
//! units. Each protocol module turns those into its own events.

use std::fmt;
use std::time::Duration;

use gliff_proto::ClientMsg;
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

pub mod keys;
pub mod rdp;
pub mod rustdesk;
mod tls;
pub mod vnc;

/// The port a Remote Desktop server listens on unless told otherwise.
pub const RDP_PORT: u16 = 3389;
/// The port RustDesk takes direct connections on, once its "Enable direct IP
/// access" option is on.
pub const RUSTDESK_PORT: u16 = 21118;
/// The port of a VNC server's first display.
pub const VNC_PORT: u16 = 5900;

/// How long [`probe`] waits for each port.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long a machine has to answer before a session gives up on it, so a
/// firewall that drops packets fails the tab instead of leaving it
/// connecting forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// What a foreign machine runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Rdp,
    RustDesk,
    Vnc,
}

impl Kind {
    pub fn scheme(self) -> &'static str {
        match self {
            Kind::Rdp => "rdp",
            Kind::RustDesk => "rustdesk",
            Kind::Vnc => "vnc",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Kind::Rdp => RDP_PORT,
            Kind::RustDesk => RUSTDESK_PORT,
            Kind::Vnc => VNC_PORT,
        }
    }

    /// The name people know it by.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Rdp => "Remote Desktop (RDP)",
            Kind::RustDesk => "RustDesk",
            Kind::Vnc => "VNC",
        }
    }

    /// Whether it can ask for a user name as well as a password: RDP
    /// always, VNC under VeNCrypt.
    pub fn takes_user(self) -> bool {
        self != Kind::RustDesk
    }
}

/// A foreign machine, written `rdp://[user@]host[:port]`,
/// `rustdesk://host[:port]` or `vnc://[user@]host[:port]`; a VNC port below
/// 100 is a display number, `:1` being port 5901.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub kind: Kind,
    /// The user to log in as, for RDP; it can also come with the password.
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
}

impl Target {
    /// `None` when `text` names no foreign machine (a plain ssh host).
    pub fn parse(text: &str) -> Option<Result<Self, String>> {
        let (scheme, rest) = text.split_once("://")?;
        let kind = match scheme.to_ascii_lowercase().as_str() {
            "rdp" => Kind::Rdp,
            "rustdesk" => Kind::RustDesk,
            "vnc" => Kind::Vnc,
            _ => {
                return Some(Err(format!(
                    "{scheme}:// is not rdp://, rustdesk:// or vnc://"
                )))
            }
        };
        Some(Self::parse_rest(kind, rest))
    }

    fn parse_rest(kind: Kind, rest: &str) -> Result<Self, String> {
        let rest = rest.trim_end_matches('/');
        let (user, hostport) = match rest.rsplit_once('@') {
            Some((user, hostport)) => (Some(user.to_string()), hostport),
            None => (None, rest),
        };
        if user.as_deref() == Some("") {
            return Err(format!("{rest:?} names an empty user"));
        }
        let (host, port) = split_port(hostport)?;
        let host = host.unwrap_or_default();
        if host.is_empty() {
            return Err(format!("{rest:?} names no host"));
        }
        if host.chars().any(|c| c.is_whitespace() || c == '/') {
            return Err(format!("{host:?} is not a host name"));
        }
        Ok(Self {
            kind,
            user,
            host,
            port: match port {
                Some(display) if kind == Kind::Vnc && display < 100 => VNC_PORT + display,
                Some(0) => return Err(format!("{hostport:?} names port 0")),
                Some(port) => port,
                None => kind.default_port(),
            },
        })
    }

    /// The same machine reached with `kind` on its default port, for a host
    /// written for ssh: `user@host` keeps the user for RDP.
    pub fn from_ssh_host(kind: Kind, ssh_host: &str) -> Result<Self, String> {
        let (user, host) = match ssh_host.rsplit_once('@') {
            Some((user, host)) => (Some(user.to_string()), host),
            None => (None, ssh_host),
        };
        let mut target = Self::parse_rest(kind, host)?;
        target.port = kind.default_port();
        if kind.takes_user() {
            target.user = user.filter(|u| !u.is_empty());
        }
        Ok(target)
    }

    /// `host:port`, bracketed for IPv6, as a socket address is written.
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Where its saved password is kept: the user, host and port, so two
    /// accounts on one machine keep two passwords.
    pub fn secret_key(&self) -> String {
        match &self.user {
            Some(user) if self.kind.takes_user() => {
                format!("{}://{user}@{}", self.kind.scheme(), self.address())
            }
            _ => format!("{}://{}", self.kind.scheme(), self.address()),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.kind.scheme())?;
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            write!(f, "{}", self.host)?;
        }
        if self.port != self.kind.default_port() {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`.
fn split_port(text: &str) -> Result<(Option<String>, Option<u16>), String> {
    let port = |p: &str| {
        // Zero is checked by the caller: VNC's display 0 is port 5900.
        p.parse::<u16>()
            .ok()
            .ok_or_else(|| format!("{p:?} is not a port"))
    };
    if let Some(rest) = text.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("{text:?} has no closing ]"))?;
        let p = match after.strip_prefix(':') {
            Some(p) => Some(port(p)?),
            None if after.is_empty() => None,
            None => return Err(format!("{text:?} is not [address]:port")),
        };
        return Ok((Some(host.to_string()), p));
    }
    match text.matches(':').count() {
        0 => Ok((Some(text.to_string()), None)),
        1 => {
            let (host, p) = text.split_once(':').unwrap_or((text, ""));
            Ok((Some(host.to_string()), Some(port(p)?)))
        }
        // A bare IPv6 address.
        _ => Ok((Some(text.to_string()), None)),
    }
}

/// What to log in with.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The RDP user, maybe `DOMAIN\user` or `user@domain`; RustDesk ignores
    /// it.
    pub user: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"…")
            .finish()
    }
}

/// What a session is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// Show the screen in a window, asking for this size where the
    /// protocol lets the client choose (RDP).
    View { width: u16, height: u16 },
    /// Drive it from this computer's edge: no picture is needed.
    Control,
}

/// A cursor image: BGRA bytes, not premultiplied, as gliff's own cursor
/// messages carry them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub argb: Vec<u8>,
}

/// What a running session reports.
#[derive(Debug)]
pub enum Event {
    /// Logged in: the screen is this many pixels.
    Connected {
        width: u32,
        height: u32,
        /// How it is shown, such as "RDP" or "RustDesk VP9".
        video: String,
    },
    /// The screen changed size.
    Resized {
        width: u32,
        height: u32,
    },
    /// The whole screen, BGRX, `width * 4` bytes a row.
    Frame {
        width: u32,
        height: u32,
        bgrx: Vec<u8>,
    },
    Cursor(Cursor),
    /// The other machine stopped drawing a cursor.
    CursorHidden,
    /// The RDP server showed a certificate no authority vouches for, seen
    /// for the first time: its SHA-256 fingerprint, to keep and hand back
    /// as `known_certificate` next time, as ssh keeps host keys.
    NewCertificate(String),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The user name or password was refused: asking again can help.
    #[error("{0}")]
    Auth(String),
    /// The RDP server's certificate is not the one it showed before.
    #[error(
        "{host} shows another certificate than before (SHA-256 {seen}). If it was \
         reinstalled, forget it in gliff and connect again; otherwise someone may be \
         in the way."
    )]
    CertificateChanged { host: String, seen: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn other(message: impl fmt::Display) -> Self {
        Self::Other(anyhow::anyhow!("{message}"))
    }
}

/// Run a session with `target` until `input` closes or the other machine
/// ends it, reporting through `events`.
///
/// `known_certificate` is the RDP or VNC server certificate's fingerprint from an
/// earlier [`Event::NewCertificate`], so a different one is refused.
pub async fn run(
    target: &Target,
    credentials: &Credentials,
    purpose: Purpose,
    known_certificate: Option<&str>,
    input: &mut UnboundedReceiver<ClientMsg>,
    events: &mut dyn FnMut(Event),
) -> Result<(), Error> {
    match target.kind {
        Kind::Rdp => {
            rdp::run(
                target,
                credentials,
                purpose,
                known_certificate,
                input,
                events,
            )
            .await
        }
        Kind::RustDesk => rustdesk::run(target, credentials, purpose, input, events).await,
        Kind::Vnc => {
            vnc::run(
                target,
                credentials,
                purpose,
                known_certificate,
                input,
                events,
            )
            .await
        }
    }
}

/// What answers on `host`'s RDP, RustDesk and VNC ports, in that order. Each answer
/// is checked to speak the protocol, so another service on the port does not
/// count.
pub async fn probe(host: &str) -> Option<Kind> {
    let (rdp, rustdesk, vnc) = tokio::join!(
        probe_one(host, Kind::Rdp),
        probe_one(host, Kind::RustDesk),
        probe_one(host, Kind::Vnc)
    );
    rdp.or(rustdesk).or(vnc)
}

/// Wayland scroll units, 15 to a wheel notch, gathered into notches: a
/// touchpad scrolls in small steps the protocols here cannot carry.
#[derive(Default)]
pub(crate) struct Scroll {
    vertical: f64,
    horizontal: f64,
}

/// Wayland scroll units in a wheel notch.
pub(crate) const NOTCH: f64 = 15.0;

impl Scroll {
    /// Whole notches to scroll now, positive down or right as Wayland has
    /// it; `stop` drops what is left over.
    pub(crate) fn notches(
        &mut self,
        axis: gliff_proto::Axis,
        value: f64,
        discrete: Option<i32>,
        stop: bool,
    ) -> i32 {
        let left = match axis {
            gliff_proto::Axis::Vertical => &mut self.vertical,
            gliff_proto::Axis::Horizontal => &mut self.horizontal,
        };
        if stop {
            *left = 0.0;
            return 0;
        }
        if let Some(d) = discrete {
            *left = 0.0;
            return d;
        }
        *left += value;
        let whole = (*left / NOTCH).trunc();
        *left -= whole * NOTCH;
        whole as i32
    }
}

/// A connection to `target`, or why there is none within
/// [`CONNECT_TIMEOUT`].
pub(crate) async fn reach(target: &Target) -> Result<TcpStream, Error> {
    let address = target.address();
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(Error::other(format!("connect to {address}: {e}"))),
        Err(_) => Err(Error::other(format!(
            "{address} did not answer within {}s; is the {} server running and the port open?",
            CONNECT_TIMEOUT.as_secs(),
            target.kind.label()
        ))),
    }
}

/// `setup`, a connection's login, given up after `limit`, or dropped when
/// `input` closes because the tab or the control stopped: `Ok(None)` then.
/// Input sent meanwhile stays queued for the session.
pub(crate) async fn set_up<T>(
    target: &Target,
    limit: Duration,
    input: &UnboundedReceiver<ClientMsg>,
    setup: impl std::future::Future<Output = Result<T, Error>>,
) -> Result<Option<T>, Error> {
    tokio::pin!(setup);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut check = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            done = &mut setup => return done.map(Some),
            () = &mut deadline => {
                return Err(Error::other(format!(
                    "{} did not finish the login within {} s",
                    target.host,
                    limit.as_secs()
                )));
            }
            _ = check.tick() => {
                if input.is_closed() {
                    return Ok(None);
                }
            }
        }
    }
}

async fn probe_one(host: &str, kind: Kind) -> Option<Kind> {
    let target = Target {
        kind,
        user: None,
        host: host.to_string(),
        port: kind.default_port(),
    };
    let answered = tokio::time::timeout(PROBE_TIMEOUT, async {
        let stream = TcpStream::connect(target.address()).await.ok()?;
        match kind {
            Kind::Rdp => rdp::answers(stream).await,
            Kind::RustDesk => rustdesk::answers(stream).await,
            Kind::Vnc => vnc::answers(stream).await,
        }
        .then_some(kind)
    })
    .await;
    answered.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_login_stops_with_its_tab_or_its_deadline() {
        let target = Target::parse("rdp://pc").unwrap().unwrap();
        let stalled = std::future::pending::<Result<(), Error>>;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(ClientMsg::PointerMotion { x: 1.0, y: 1.0 })
            .unwrap();
        drop(tx);
        let r = set_up(&target, Duration::from_secs(60), &rx, stalled()).await;
        assert!(matches!(r, Ok(None)));

        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let r = set_up(&target, Duration::from_millis(50), &rx, stalled()).await;
        assert!(r
            .unwrap_err()
            .to_string()
            .contains("did not finish the login"));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(ClientMsg::PointerMotion { x: 1.0, y: 1.0 })
            .unwrap();
        let r = set_up(&target, Duration::from_secs(60), &rx, async { Ok(7) }).await;
        assert_eq!(r.unwrap(), Some(7));
        assert!(rx.try_recv().is_ok(), "input sent meanwhile stays queued");
    }

    #[test]
    fn parses_foreign_addresses() {
        let t = Target::parse("rdp://alice@win.local").unwrap().unwrap();
        assert_eq!(t.kind, Kind::Rdp);
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "win.local");
        assert_eq!(t.port, RDP_PORT);
        assert_eq!(t.to_string(), "rdp://alice@win.local");

        let t = Target::parse("rustdesk://192.168.1.9:21200")
            .unwrap()
            .unwrap();
        assert_eq!((t.kind, t.port), (Kind::RustDesk, 21200));
        assert_eq!(t.address(), "192.168.1.9:21200");
        assert_eq!(t.to_string(), "rustdesk://192.168.1.9:21200");

        let t = Target::parse("RDP://[fe80::1]:3390/").unwrap().unwrap();
        assert_eq!((t.host.as_str(), t.port), ("fe80::1", 3390));
        assert_eq!(t.address(), "[fe80::1]:3390");

        assert!(Target::parse("user@host").is_none());
        assert!(Target::parse("ftp://host").unwrap().is_err());

        let t = Target::parse("vnc://ann@mac.local:1").unwrap().unwrap();
        assert_eq!((t.kind, t.port), (Kind::Vnc, 5901));
        assert_eq!(t.user.as_deref(), Some("ann"));
        let t = Target::parse("vnc://pc:0").unwrap().unwrap();
        assert_eq!(t.port, 5900);
        let t = Target::parse("vnc://pc:5902").unwrap().unwrap();
        assert_eq!(t.port, 5902);
        assert_eq!(
            Target::parse("vnc://pc").unwrap().unwrap().to_string(),
            "vnc://pc"
        );
        assert!(Target::parse("rdp://").unwrap().is_err());
        assert!(Target::parse("rdp://@host").unwrap().is_err());
        assert!(Target::parse("rdp://host:0").unwrap().is_err());
        assert!(Target::parse("rdp://host:x").unwrap().is_err());
        assert!(Target::parse("rdp://a b").unwrap().is_err());
    }

    #[test]
    fn an_ssh_host_becomes_a_foreign_target() {
        let t = Target::from_ssh_host(Kind::Rdp, "bob@pc").unwrap();
        assert_eq!(t.to_string(), "rdp://bob@pc");
        // RustDesk logs in with a password only.
        let t = Target::from_ssh_host(Kind::RustDesk, "bob@pc").unwrap();
        assert_eq!(t.to_string(), "rustdesk://pc");
        assert_eq!(t.secret_key(), "rustdesk://pc:21118");
    }

    #[test]
    fn passwords_are_kept_per_account() {
        let a = Target::parse("rdp://a@pc").unwrap().unwrap();
        let b = Target::parse("rdp://b@pc").unwrap().unwrap();
        assert_ne!(a.secret_key(), b.secret_key());
        assert_eq!(a.secret_key(), "rdp://a@pc:3389");
    }

    #[test]
    fn touchpad_scrolling_gathers_into_notches() {
        use gliff_proto::Axis;
        let mut s = Scroll::default();
        assert_eq!(s.notches(Axis::Vertical, 10.0, None, false), 0);
        assert_eq!(s.notches(Axis::Vertical, 10.0, None, false), 1);
        assert_eq!(s.notches(Axis::Vertical, -40.0, None, false), -2);
        assert_eq!(s.notches(Axis::Horizontal, 15.0, Some(1), false), 1);
        assert_eq!(s.notches(Axis::Vertical, 0.0, None, true), 0);
        assert_eq!(s.notches(Axis::Vertical, 14.0, None, false), 0);
    }

    #[test]
    fn credentials_hide_the_password_when_printed() {
        let c = Credentials {
            user: "u".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
