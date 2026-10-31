//! `--control SIDE:HOST`: this machine's pointer and keyboard drive another
//! machine placed on SIDE of this one's screens, with no window. Pushing the
//! pointer past that edge carries it onto the other machine's screen; moving
//! it back across the facing edge, or the release hotkey, returns it.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytes::Bytes;
use gliff_bridge::Bridge;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{
    ClientCaps, ClientMsg, ClipboardMsg, ServerMsg, FEATURE_KEYBOARD_LOCKS, PROTOCOL_VERSION,
};
use gliff_transport::clipboard::progress::Jobs;
use gliff_transport::clipboard::{self, outbound_channel, Transfers};
use gliff_transport::Framed;
use gliff_transport::{spawn_ssh, SshStream, SshTarget};
use hypr_edge::{EdgeCmd, EdgeConfig, EdgeEvent, Edges, Side};
use hypr_input::{Clipboard, ClipboardEvent};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use xkbcommon::xkb;

/// How long a connection waits for the local keymap before saying hello
/// without one.
const KEYMAP_WAIT: Duration = Duration::from_secs(2);
/// Reconnection backoff after a link drops.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// A link that stayed up this long retries from the first delay again.
const STABLE: Duration = Duration::from_secs(10);
/// How often a link asks the other machine whether it is there.
const KEEPALIVE: Duration = Duration::from_secs(2);
/// A link that hears nothing for this long, or cannot write for this long,
/// is dropped, so its edge stops taking the pointer to a machine that is
/// gone without closing the connection.
const SILENCE: Duration = Duration::from_secs(6);
/// How long after the pointer comes back from one side another side leaves
/// it here, so a fast return does not carry it straight across this screen.
const SETTLE: Duration = Duration::from_millis(350);

/// When the pointer last came back, and from which side. The window runs
/// each machine's control on a thread of its own, so this is shared by them.
static RETURNED: std::sync::Mutex<Option<(Instant, Side)>> = std::sync::Mutex::new(None);

fn note_return(side: Side) {
    if let Ok(mut returned) = RETURNED.lock() {
        *returned = Some((Instant::now(), side));
    }
}

/// Whether a pointer reaching `side` now is one still moving from a return.
fn still_returning(returned: Option<(Instant, Side)>, side: Side, now: Instant) -> bool {
    returned.is_some_and(|(at, from)| from != side && now.duration_since(at) < SETTLE)
}

/// One `--control` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub side: Side,
    pub host: String,
}

/// `SIDE:HOST`, as `--control` takes it.
pub fn parse(text: &str) -> Result<Control, String> {
    let (side, host) = text
        .split_once(':')
        .ok_or_else(|| format!("{text:?} is not SIDE:HOST, such as left:user@host"))?;
    let side =
        Side::parse(side).ok_or_else(|| format!("{side:?} is not left, right, above or below"))?;
    if host.is_empty() {
        return Err(format!("{text:?} names no host"));
    }
    Ok(Control {
        side,
        host: host.to_string(),
    })
}

/// A chord that hands the pointer and keyboard back, matched by keysym in
/// the local keymap, so it means the same keys whatever the layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    /// XKB modifier names.
    mods: Vec<&'static str>,
    keysym: u32,
}

impl Hotkey {
    /// Forms like `shift+escape` or `ctrl+alt+q`; `None` for `none`.
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        let text = text.trim();
        if text.is_empty() || text.eq_ignore_ascii_case("none") {
            return Ok(None);
        }
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let key = parts.pop().unwrap_or_default();
        let mods = parts
            .into_iter()
            .map(|m| match m.to_ascii_lowercase().as_str() {
                "shift" => Ok(xkb::MOD_NAME_SHIFT),
                "ctrl" | "control" => Ok(xkb::MOD_NAME_CTRL),
                "alt" => Ok(xkb::MOD_NAME_ALT),
                "super" | "logo" => Ok(xkb::MOD_NAME_LOGO),
                other => Err(format!("{other:?} is not shift, ctrl, alt or super")),
            })
            .collect::<Result<_, _>>()?;
        let name = if key.eq_ignore_ascii_case("esc") {
            "Escape"
        } else {
            key
        };
        let keysym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE).raw();
        if key.is_empty() || keysym == 0 {
            return Err(format!("{key:?} is not a key name"));
        }
        Ok(Some(Self { mods, keysym }))
    }

    /// Whether pressing `code` (evdev) with the modifiers of `keys` is this
    /// chord. The key may name the symbol without modifiers (`shift+escape`)
    /// or the one they produce (`shift+exclam`).
    fn matches(&self, keys: &xkb::State, code: u32) -> bool {
        let key = xkb::Keycode::new(code + 8);
        let layout = keys.key_get_layout(key);
        let unmodified = keys
            .get_keymap()
            .key_get_syms_by_level(key, layout, 0)
            .iter()
            .any(|s| s.raw() == self.keysym);
        let produced = keys
            .key_get_syms(key)
            .iter()
            .any(|s| s.raw() == self.keysym);
        (unmodified || produced)
            && self
                .mods
                .iter()
                .all(|m| keys.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE))
    }
}

/// The local keyboard's state, for the release hotkey.
fn key_state(keymap: &str) -> Option<xkb::State> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_string(
        &ctx,
        keymap.to_string(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    Some(xkb::State::new(&keymap))
}

/// The layout in effect and the locked modifiers, by name, of `keys`.
fn locks_of(keys: &xkb::State) -> (u32, Vec<String>) {
    let keymap = keys.get_keymap();
    let locked = (0..keymap.num_mods())
        .filter(|&i| keys.mod_index_is_active(i, xkb::STATE_MODS_LOCKED))
        .map(|i| keymap.mod_get_name(i).to_string())
        .collect();
    (keys.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE), locked)
}

/// What a link reports to the controller.
struct Up {
    side: Side,
    /// The other machine's screen in logical pixels, or `None` when the link
    /// is down.
    extent: Option<(f64, f64)>,
    /// Why the link went down, when it failed.
    error: Option<String>,
}

/// What a running control tells whoever started it.
#[derive(Debug, Clone, PartialEq)]
pub enum Report {
    /// The machine answers: its edge takes the pointer.
    Connected { host: String },
    /// The machine cannot be reached now; gliff keeps trying.
    Disconnected { host: String, error: Option<String> },
    /// The control ended, with why when it failed.
    Ended { error: Option<String> },
}

/// A control running on its own thread, as the window starts one. Dropping
/// it stops the control and gives the pointer back.
pub struct Handle {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// Run `controls` until stopped, telling `report` how they are doing.
    pub fn spawn(
        controls: Vec<Control>,
        server_bin: String,
        hotkey: String,
        share_clipboard: bool,
        report: std::sync::mpsc::Sender<Report>,
    ) -> std::io::Result<Self> {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let join = std::thread::Builder::new()
            .name("gliff-control".into())
            .spawn(move || {
                let tell = report.clone();
                let result = drive(
                    &controls,
                    &server_bin,
                    &hotkey,
                    share_clipboard,
                    async {
                        let _ = stopped.await;
                    },
                    Box::new(move |r| {
                        let _ = tell.send(r);
                    }),
                );
                let _ = report.send(Report::Ended {
                    error: result.err().map(|e| format!("{e:#}")),
                });
            })?;
        Ok(Self {
            stop: Some(stop),
            join: Some(join),
        })
    }

    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct Link {
    host: String,
    to: UnboundedSender<ClientMsg>,
    extent: Option<(f64, f64)>,
}

/// The input currently going to another machine.
struct Active {
    side: Side,
    x: f64,
    y: f64,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    /// The layout and locks last sent there.
    locks: Option<(u32, Vec<String>)>,
}

/// This machine's clipboard, shared with every machine it drives: what is
/// copied here is offered to all of them, and what is copied on one of them
/// becomes the selection here, pasted from that machine.
#[derive(Default)]
struct Shared {
    compositor: Option<Rc<Clipboard>>,
    bridges: RefCell<BTreeMap<Side, Rc<Bridge>>>,
    /// The selection made here, for a machine that connects later.
    selection: RefCell<Option<Vec<String>>>,
    /// The machine whose copy is the selection here.
    holder: Cell<Option<Side>>,
}

impl Shared {
    fn on_compositor(&self, ev: ClipboardEvent) {
        match ev {
            ClipboardEvent::Selection { mime_types } => {
                // Offering nothing withdraws what the machines were offered.
                let mime_types = if crate::clipboard_ui::sensitive_selection(&mime_types) {
                    Vec::new()
                } else {
                    mime_types
                };
                self.holder.set(None);
                *self.selection.borrow_mut() = Some(mime_types.clone());
                let bridges: Vec<_> = self.bridges.borrow().values().cloned().collect();
                for bridge in bridges {
                    bridge.on_compositor_event(ClipboardEvent::Selection {
                        mime_types: mime_types.clone(),
                    });
                }
            }
            paste @ ClipboardEvent::Paste { .. } => {
                let bridge = self
                    .holder
                    .get()
                    .and_then(|side| self.bridges.borrow().get(&side).cloned());
                if let Some(bridge) = bridge {
                    bridge.on_compositor_event(paste);
                }
            }
        }
    }

    fn connected(&self, side: Side, bridge: Rc<Bridge>) {
        let selection = self.selection.borrow().clone();
        if let Some(mime_types) = selection.filter(|_| self.holder.get().is_none()) {
            bridge.on_compositor_event(ClipboardEvent::Selection { mime_types });
        }
        self.bridges.borrow_mut().insert(side, bridge);
    }

    fn disconnected(&self, side: Side) {
        self.bridges.borrow_mut().remove(&side);
        if self.holder.get() == Some(side) {
            self.holder.set(None);
            if let Some(compositor) = &self.compositor {
                compositor.offer(Vec::new());
            }
        }
    }

    fn on_peer(&self, side: Side, msg: ClipboardMsg, payload: Bytes) {
        if let ClipboardMsg::Offer {
            mime_types, files, ..
        } = &msg
        {
            if !mime_types.is_empty() || !files.is_empty() {
                self.holder.set(Some(side));
                *self.selection.borrow_mut() = None;
            } else if self.holder.get() != Some(side) {
                // That machine's clipboard emptied; ours is not its copy.
                return;
            }
        }
        let bridge = self.bridges.borrow().get(&side).cloned();
        if let Some(bridge) = bridge {
            bridge.on_peer_msg(msg, payload);
        }
    }
}

/// Run `controls` until Ctrl+C or SIGTERM, for `gliff --control`.
pub fn run(
    controls: &[Control],
    server_bin: &str,
    hotkey: &str,
    share_clipboard: bool,
) -> anyhow::Result<()> {
    let ended = async {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    };
    drive(
        controls,
        server_bin,
        hotkey,
        share_clipboard,
        ended,
        Box::new(|_| {}),
    )
}

fn drive(
    controls: &[Control],
    server_bin: &str,
    hotkey: &str,
    share_clipboard: bool,
    stop: impl std::future::Future<Output = ()>,
    report: Box<dyn FnMut(Report)>,
) -> anyhow::Result<()> {
    let hotkey = Hotkey::parse(hotkey).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "unusable release hotkey; using shift+escape");
        Hotkey::parse("shift+escape").expect("the default hotkey parses")
    });
    let mut sides = BTreeSet::new();
    for c in controls {
        anyhow::ensure!(
            sides.insert(c.side),
            "two machines are on the {} side",
            c.side.name()
        );
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let (ev_tx, ev_rx) = unbounded_channel();
        let edges = Edges::start(
            EdgeConfig {
                target: Default::default(),
                sides: sides.iter().copied().collect(),
            },
            Box::new(move |e| {
                let _ = ev_tx.send(e);
            }),
        )
        .context("set up the screen edges")?;
        let (keymap_tx, keymap_rx) = watch::channel(String::new());
        let (up_tx, up_rx) = unbounded_channel();
        let (clip_tx, mut clip_rx) = unbounded_channel();
        let compositor = share_clipboard
            .then(|| {
                Clipboard::start(
                    Default::default(),
                    Box::new(move |ev| {
                        let _ = clip_tx.send(ev);
                    }),
                )
                .map_err(|e| tracing::warn!(error = %e, "clipboard bridge unavailable"))
                .ok()
                .map(Rc::new)
            })
            .flatten();
        let shared = Rc::new(Shared {
            compositor,
            ..Shared::default()
        });
        tokio::task::spawn_local({
            let shared = shared.clone();
            async move {
                while let Some(ev) = clip_rx.recv().await {
                    shared.on_compositor(ev);
                }
            }
        });
        let mut links = BTreeMap::new();
        for c in controls {
            let mut target = SshTarget::new(c.host.clone());
            target.server_bin = server_bin.to_string();
            target.server_args = vec!["--control".into(), c.side.opposite().name().into()];
            let (to, from) = unbounded_channel();
            tokio::task::spawn_local(link(
                c.side,
                target,
                keymap_rx.clone(),
                up_tx.clone(),
                from,
                shared.clone(),
            ));
            links.insert(
                c.side,
                Link {
                    host: c.host.clone(),
                    to,
                    extent: None,
                },
            );
        }
        let mut controller = Controller {
            edges,
            links,
            active: None,
            hotkey,
            keymap: keymap_tx,
            keys: None,
            report,
        };
        controller.run(ev_rx, up_rx, stop).await
    })
}

struct Controller {
    edges: Edges,
    links: BTreeMap<Side, Link>,
    active: Option<Active>,
    hotkey: Option<Hotkey>,
    keymap: watch::Sender<String>,
    /// The local keyboard's state, while the keymap is known.
    keys: Option<xkb::State>,
    report: Box<dyn FnMut(Report)>,
}

impl Controller {
    async fn run(
        &mut self,
        mut events: UnboundedReceiver<EdgeEvent>,
        mut ups: UnboundedReceiver<Up>,
        stop: impl std::future::Future<Output = ()>,
    ) -> anyhow::Result<()> {
        tokio::pin!(stop);
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.on_edge(event),
                    None => anyhow::bail!("the screen edges stopped"),
                },
                Some(up) = ups.recv() => self.on_link(up),
                _ = &mut stop => break,
            }
        }
        self.release();
        Ok(())
    }

    fn send(&self, side: Side, msg: ClientMsg) {
        if let Some(link) = self.links.get(&side) {
            let _ = link.to.send(msg);
        }
    }

    fn on_link(&mut self, up: Up) {
        // Return the pointer from a machine that is gone while its size
        // still says where along the edge it was.
        if up.extent.is_none() && self.active.as_ref().is_some_and(|a| a.side == up.side) {
            self.release();
        }
        let Some(link) = self.links.get_mut(&up.side) else {
            return;
        };
        link.extent = up.extent;
        let host = link.host.clone();
        match up.extent {
            Some((w, h)) => {
                tracing::info!(
                    %host,
                    width = w,
                    height = h,
                    "move the pointer past the {} edge to control it",
                    up.side.name()
                );
                (self.report)(Report::Connected { host });
            }
            None => {
                tracing::info!(%host, "not connected");
                (self.report)(Report::Disconnected {
                    host,
                    error: up.error,
                });
            }
        }
        let _ = self.edges.send(EdgeCmd::Arm {
            side: up.side,
            armed: up.extent.is_some(),
        });
    }

    fn on_edge(&mut self, event: EdgeEvent) {
        match event {
            EdgeEvent::Keymap(text) => {
                self.keys = key_state(&text);
                if self.keys.is_none() {
                    tracing::warn!("the local keymap does not compile; no release hotkey");
                }
                self.keymap.send_replace(text);
            }
            EdgeEvent::Entered { side, along } => self.enter(side, along),
            EdgeEvent::Motion { dx, dy } => self.motion(dx, dy),
            EdgeEvent::Key { code, pressed } => {
                let hotkey = pressed
                    && self
                        .hotkey
                        .as_ref()
                        .zip(self.keys.as_ref())
                        .is_some_and(|(h, keys)| h.matches(keys, code));
                if hotkey && self.active.is_some() {
                    self.release();
                    return;
                }
                let Some(a) = self.active.as_mut() else {
                    return;
                };
                if pressed {
                    a.keys.insert(code);
                } else if !a.keys.remove(&code) {
                    // Pressed before the capture, so the other machine never
                    // saw it go down.
                    return;
                }
                let side = a.side;
                self.send(
                    side,
                    ClientMsg::Key {
                        keycode: code,
                        pressed,
                    },
                );
            }
            EdgeEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => {
                if let Some(keys) = self.keys.as_mut() {
                    keys.update_mask(depressed, latched, locked, 0, 0, group);
                }
                self.send_locks();
            }
            EdgeEvent::Button { button, pressed } => {
                let Some(a) = self.active.as_mut() else {
                    return;
                };
                if pressed {
                    a.buttons.insert(button);
                } else if !a.buttons.remove(&button) {
                    return;
                }
                let side = a.side;
                self.send(side, ClientMsg::PointerButton { button, pressed });
            }
            EdgeEvent::Axis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let Some(side) = self.active.as_ref().map(|a| a.side) else {
                    return;
                };
                let axis = match axis {
                    hypr_edge::Axis::Vertical => gliff_proto::Axis::Vertical,
                    hypr_edge::Axis::Horizontal => gliff_proto::Axis::Horizontal,
                };
                self.send(
                    side,
                    ClientMsg::PointerAxis {
                        axis,
                        value,
                        discrete,
                        stop,
                    },
                );
            }
            EdgeEvent::Lost => {
                tracing::info!("the compositor took the pointer back");
                self.release_remote();
            }
            EdgeEvent::Error(e) => tracing::warn!(error = %e, "screen edge"),
        }
    }

    fn enter(&mut self, side: Side, along: f64) {
        let returned = RETURNED.lock().ok().and_then(|r| *r);
        if still_returning(returned, side, Instant::now()) {
            tracing::info!(side = side.name(), "pointer still moving from a return");
            let _ = self.edges.send(EdgeCmd::Release { along });
            return;
        }
        let Some((w, h)) = self.links.get(&side).and_then(|l| l.extent) else {
            let _ = self.edges.send(EdgeCmd::Release { along });
            return;
        };
        let (x, y) = entry_point(side, along, (w, h));
        self.active = Some(Active {
            side,
            x,
            y,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            locks: None,
        });
        self.send(side, ClientMsg::PointerMotion { x, y });
        self.send_locks();
    }

    /// Give the other machine this keyboard's layout and locks (Caps Lock,
    /// Num Lock) when they differ from what it last got, so keys type there
    /// as they would here.
    fn send_locks(&mut self) {
        let (Some(keys), Some(a)) = (self.keys.as_ref(), self.active.as_mut()) else {
            return;
        };
        let locks = locks_of(keys);
        if a.locks.as_ref() == Some(&locks) {
            return;
        }
        a.locks = Some(locks.clone());
        let side = a.side;
        let (layout, locked) = locks;
        self.send(side, ClientMsg::KeyboardLocks { layout, locked });
    }

    fn motion(&mut self, dx: f64, dy: f64) {
        let Some(a) = self.active.as_mut() else {
            return;
        };
        let Some(extent) = self.links.get(&a.side).and_then(|l| l.extent) else {
            return;
        };
        a.x += dx;
        a.y += dy;
        // A held button keeps the pointer there: a drag cannot cross.
        if a.buttons.is_empty() {
            if let Some(along) = crossed_back(a.side, (a.x, a.y), extent) {
                self.release_at(along);
                return;
            }
        }
        let (w, h) = extent;
        a.x = a.x.clamp(0.0, w - 1.0);
        a.y = a.y.clamp(0.0, h - 1.0);
        let (side, x, y) = (a.side, a.x, a.y);
        self.send(side, ClientMsg::PointerMotion { x, y });
    }

    /// Let go of every key and button the other machine holds for us.
    fn release_remote(&mut self) -> Option<Active> {
        let a = self.active.take()?;
        note_return(a.side);
        for &keycode in &a.keys {
            self.send(
                a.side,
                ClientMsg::Key {
                    keycode,
                    pressed: false,
                },
            );
        }
        for &button in &a.buttons {
            self.send(
                a.side,
                ClientMsg::PointerButton {
                    button,
                    pressed: false,
                },
            );
        }
        Some(a)
    }

    fn release_at(&mut self, along: f64) {
        if self.release_remote().is_some() {
            let _ = self.edges.send(EdgeCmd::Release { along });
        }
    }

    /// Return the pointer at the point along the edge it is at now.
    fn release(&mut self) {
        let Some(a) = self.active.as_ref() else {
            return;
        };
        let along = self
            .links
            .get(&a.side)
            .and_then(|l| l.extent)
            .map_or(0.5, |(w, h)| along_edge(a.side, (a.x, a.y), (w, h)));
        self.release_at(along);
    }
}

/// Where the pointer lands on the other machine's screen, `extent` in
/// logical pixels, when it leaves this one's `side` edge `along` it.
fn entry_point(side: Side, along: f64, (w, h): (f64, f64)) -> (f64, f64) {
    let along = along.clamp(0.0, 1.0);
    match side {
        Side::Left => (w - 1.0, along * (h - 1.0)),
        Side::Right => (0.0, along * (h - 1.0)),
        Side::Above => (along * (w - 1.0), h - 1.0),
        Side::Below => (along * (w - 1.0), 0.0),
    }
}

/// How far along the shared edge a point of the other screen is.
fn along_edge(side: Side, (x, y): (f64, f64), (w, h): (f64, f64)) -> f64 {
    let ratio = |v: f64, len: f64| (v / (len - 1.0).max(1.0)).clamp(0.0, 1.0);
    if side.is_vertical() {
        ratio(y, h)
    } else {
        ratio(x, w)
    }
}

/// Whether the pointer, at `pos` on the other screen, has crossed back over
/// the edge shared with this machine; if so, where along it.
fn crossed_back(side: Side, pos: (f64, f64), (w, h): (f64, f64)) -> Option<f64> {
    let (x, y) = pos;
    let crossed = match side {
        Side::Left => x > w - 1.0,
        Side::Right => x < 0.0,
        Side::Above => y > h - 1.0,
        Side::Below => y < 0.0,
    };
    crossed.then(|| along_edge(side, pos, (w, h)))
}

/// Keep a connection to one machine, reconnecting with backoff.
async fn link(
    side: Side,
    target: SshTarget,
    mut keymap: watch::Receiver<String>,
    up: UnboundedSender<Up>,
    mut out: UnboundedReceiver<ClientMsg>,
    shared: Rc<Shared>,
) {
    let mut delay = RETRY_FIRST;
    loop {
        let started = Instant::now();
        let error = match session(side, &target, &mut keymap, &up, &mut out, &shared).await {
            Ok(()) => {
                tracing::info!(host = %target.host, "connection closed");
                None
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!(host = %target.host, %error, "connection failed");
                Some(error)
            }
        };
        shared.disconnected(side);
        let _ = up.send(Up {
            side,
            extent: None,
            error,
        });
        while out.try_recv().is_ok() {}
        if started.elapsed() > STABLE {
            delay = RETRY_FIRST;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

async fn session(
    side: Side,
    target: &SshTarget,
    keymap: &mut watch::Receiver<String>,
    up: &UnboundedSender<Up>,
    out: &mut UnboundedReceiver<ClientMsg>,
    shared: &Shared,
) -> anyhow::Result<()> {
    let SshStream {
        child: _child,
        stdin,
        stdout,
    } = spawn_ssh(target).context("start ssh")?;
    let mut reader = Framed::new(stdout);
    let mut writer = Framed::new(stdin);
    let _ = tokio::time::timeout(KEYMAP_WAIT, keymap.wait_for(|k| !k.is_empty())).await;
    let first_keymap = keymap.borrow_and_update().clone();
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: first_keymap,
            caps: ClientCaps {
                codecs: Vec::new(),
                max_width: 0,
                max_height: 0,
                chroma: Vec::new(),
                features: Vec::new(),
            },
        })
        .await?;
    // Whether that server takes `KeyboardLocks`; one that does not is not
    // sent them.
    let (extent, takes_locks) = loop {
        match reader.read_msg::<ServerMsg>().await? {
            ServerMsg::HelloAck {
                version,
                outputs,
                features,
                ..
            } => {
                anyhow::ensure!(
                    version == PROTOCOL_VERSION,
                    "the other machine speaks protocol {version}, this one {PROTOCOL_VERSION}"
                );
                let o = outputs
                    .first()
                    .context("the other machine named no screen")?;
                let s = f64::from(o.scale_milli.max(500)) / 1000.0;
                tracing::debug!(output = %o.name, "controlling");
                break (
                    (
                        (f64::from(o.width) / s).round().max(1.0),
                        (f64::from(o.height) / s).round().max(1.0),
                    ),
                    features.iter().any(|f| f == FEATURE_KEYBOARD_LOCKS),
                );
            }
            ServerMsg::Error { message, .. } => anyhow::bail!("{message}"),
            ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
            _ => {}
        }
    };
    let _ = up.send(Up {
        side,
        extent: Some(extent),
        error: None,
    });
    let (clip_out_tx, mut clip_out) = outbound_channel();
    shared.connected(
        side,
        Rc::new(Bridge::new(
            Transfers::new(clip_out_tx, clipboard::Side::Client),
            Jobs::new(|_, _| {}),
            shared.compositor.clone(),
        )),
    );
    let mut keymap_alive = true;
    // The last layout and locks written. Keymaps and locks reach this loop by
    // separate ways, so the locks are written again after each keymap: a
    // layout the old keymap lacked would otherwise have been dropped.
    let mut locks: Option<ClientMsg> = None;
    let mut heard = Instant::now();
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = out.recv() => match msg {
                Some(ClientMsg::KeyboardLocks { .. }) if !takes_locks => {}
                Some(msg @ ClientMsg::KeyboardLocks { .. }) => {
                    write(&mut writer, &msg).await?;
                    locks = Some(msg);
                }
                Some(msg) => write(&mut writer, &msg).await?,
                None => return Ok(()),
            },
            Some((msg, payload)) = clip_out.recv() => {
                write_with(&mut writer, &ClientMsg::from(msg), &payload).await?;
            }
            changed = keymap.changed(), if keymap_alive => match changed {
                Ok(()) => {
                    let text = keymap.borrow_and_update().clone();
                    if !text.is_empty() {
                        write(&mut writer, &ClientMsg::Keymap { keymap: text }).await?;
                        if let Some(locks) = &locks {
                            write(&mut writer, locks).await?;
                        }
                    }
                }
                Err(_) => keymap_alive = false,
            },
            read = reader.read_msg::<ServerMsg>() => {
                heard = Instant::now();
                match read {
                    Ok(ServerMsg::Ping { t }) => write(&mut writer, &ClientMsg::Pong { t }).await?,
                    Ok(msg) => {
                        if let Ok(clip) = msg.into_clipboard() {
                            let payload = match &clip {
                                ClipboardMsg::Data { data_len, .. } => {
                                    anyhow::ensure!(
                                        *data_len as usize <= CHUNK,
                                        "clipboard chunk of {data_len} bytes exceeds the limit"
                                    );
                                    reader.read_payload(*data_len).await?
                                }
                                _ => Bytes::new(),
                            };
                            shared.on_peer(side, clip, payload);
                        }
                    }
                    Err(gliff_transport::Error::Closed) => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
            _ = keepalive.tick() => {
                anyhow::ensure!(
                    heard.elapsed() < SILENCE,
                    "the other machine stopped answering"
                );
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                write(&mut writer, &ClientMsg::Ping { t }).await?;
            }
        }
    }
}

/// Write `msg`, giving up on a link that takes longer than [`SILENCE`].
async fn write<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut Framed<W>,
    msg: &ClientMsg,
) -> anyhow::Result<()> {
    write_with(writer, msg, &[]).await
}

/// [`write`], with the chunk that follows a clipboard message.
async fn write_with<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut Framed<W>,
    msg: &ClientMsg,
    payload: &[u8],
) -> anyhow::Result<()> {
    let written = async {
        if payload.is_empty() {
            writer.write_msg(msg).await
        } else {
            writer.write_msg_with_payloads(msg, &[payload]).await
        }
    };
    tokio::time::timeout(SILENCE, written)
        .await
        .map_err(|_| anyhow::anyhow!("the other machine stopped taking input"))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(mime: &[&str]) -> ClipboardMsg {
        ClipboardMsg::Offer {
            serial: 1,
            mime_types: mime.iter().map(|m| m.to_string()).collect(),
            files: Vec::new(),
        }
    }

    #[test]
    fn the_last_copy_holds_the_clipboard() {
        let shared = Shared::default();
        shared.on_compositor(ClipboardEvent::Selection {
            mime_types: vec!["text/plain".into()],
        });
        assert_eq!(shared.holder.get(), None);

        shared.on_peer(Side::Left, offer(&["text/plain"]), Bytes::new());
        assert_eq!(shared.holder.get(), Some(Side::Left));
        assert!(shared.selection.borrow().is_none());

        // Another machine's clipboard emptying takes nothing from the holder.
        shared.on_peer(Side::Right, offer(&[]), Bytes::new());
        assert_eq!(shared.holder.get(), Some(Side::Left));

        shared.on_peer(Side::Right, offer(&["text/plain"]), Bytes::new());
        assert_eq!(shared.holder.get(), Some(Side::Right));

        // A machine that was not holding it going away changes nothing.
        shared.disconnected(Side::Left);
        assert_eq!(shared.holder.get(), Some(Side::Right));
        shared.disconnected(Side::Right);
        assert_eq!(shared.holder.get(), None);

        shared.on_peer(Side::Left, offer(&["text/plain"]), Bytes::new());
        shared.on_compositor(ClipboardEvent::Selection {
            mime_types: vec!["text/html".into()],
        });
        assert_eq!(shared.holder.get(), None);
        assert_eq!(
            shared.selection.borrow().as_deref(),
            Some(&["text/html".to_string()][..])
        );

        shared.on_compositor(ClipboardEvent::Selection {
            mime_types: vec!["text/plain".into(), "x-kde-passwordManagerHint".into()],
        });
        assert_eq!(shared.selection.borrow().as_deref(), Some(&[][..]));
    }

    #[test]
    fn a_return_keeps_the_pointer_from_the_other_side_for_a_moment() {
        let at = Instant::now();
        let returned = Some((at, Side::Left));
        assert!(still_returning(returned, Side::Right, at));
        assert!(still_returning(
            returned,
            Side::Right,
            at + SETTLE - Duration::from_millis(1)
        ));
        assert!(!still_returning(returned, Side::Right, at + SETTLE));
        // Going straight back where it came from is not a mistake.
        assert!(!still_returning(returned, Side::Left, at));
        assert!(!still_returning(None, Side::Right, at));
    }

    #[test]
    fn parses_side_and_host() {
        assert_eq!(
            parse("left:node@NODE.local").unwrap(),
            Control {
                side: Side::Left,
                host: "node@NODE.local".into()
            }
        );
        assert!(parse("node@NODE.local").is_err());
        assert!(parse("middle:host").is_err());
        assert!(parse("right:").is_err());
    }

    #[test]
    fn names_the_layout_and_locks_in_effect() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            "us,es",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut keys = xkb::State::new(&keymap);
        assert_eq!(locks_of(&keys), (0, Vec::new()));
        let caps = 1 << keymap.mod_get_index(xkb::MOD_NAME_CAPS);
        keys.update_mask(0, 0, caps, 0, 0, 1);
        assert_eq!(locks_of(&keys), (1, vec![xkb::MOD_NAME_CAPS.to_string()]));
    }

    #[test]
    fn matches_hotkeys_by_keysym_in_the_layout() {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let state = |layout: &str, variant: &str| {
            let keymap = xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                layout,
                variant,
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .unwrap();
            xkb::State::new(&keymap)
        };
        let press = |s: &mut xkb::State, code: u32| {
            s.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Down);
        };
        const ESC: u32 = 1;
        const LEFT_SHIFT: u32 = 42;
        const RIGHT_SHIFT: u32 = 54;
        const CTRL: u32 = 29;
        const ALT: u32 = 56;
        const Q: u32 = 16;
        const X: u32 = 45;

        let hk = Hotkey::parse("shift+escape").unwrap().unwrap();
        let mut us = state("us", "");
        assert!(!hk.matches(&us, ESC), "Escape alone");
        press(&mut us, RIGHT_SHIFT);
        assert!(hk.matches(&us, ESC));
        assert!(!hk.matches(&us, Q));

        // A shifted symbol: on US, shift+1 types !.
        const ONE: u32 = 2;
        let hk = Hotkey::parse("shift+exclam").unwrap().unwrap();
        let mut us = state("us", "");
        assert!(!hk.matches(&us, ONE), "1 alone");
        press(&mut us, LEFT_SHIFT);
        assert!(hk.matches(&us, ONE));

        // On Dvorak the key that types q is where QWERTY has x.
        let hk = Hotkey::parse("Ctrl+Alt+q").unwrap().unwrap();
        let mut dvorak = state("us", "dvorak");
        press(&mut dvorak, CTRL);
        press(&mut dvorak, ALT);
        assert!(hk.matches(&dvorak, X));
        assert!(!hk.matches(&dvorak, Q));
        let mut es = state("es", "");
        press(&mut es, LEFT_SHIFT);
        assert!(!hk.matches(&es, Q), "shift is not ctrl+alt");

        assert_eq!(Hotkey::parse("none").unwrap(), None);
        assert!(Hotkey::parse("esc").unwrap().is_some());
        assert!(Hotkey::parse("super+F12").unwrap().is_some());
        assert!(Hotkey::parse("hyper+q").is_err());
        assert!(Hotkey::parse("ctrl+").is_err());
        assert!(Hotkey::parse("ctrl+nokey").is_err());
    }

    #[test]
    fn enters_at_the_facing_edge_and_crosses_back_over_it() {
        let extent = (1920.0, 1080.0);
        assert_eq!(entry_point(Side::Left, 0.5, extent), (1919.0, 539.5));
        assert_eq!(entry_point(Side::Right, 0.0, extent), (0.0, 0.0));
        assert_eq!(entry_point(Side::Above, 1.0, extent), (1919.0, 1079.0));
        assert_eq!(entry_point(Side::Below, 0.5, extent), (959.5, 0.0));

        assert_eq!(crossed_back(Side::Left, (1919.0, 539.5), extent), None);
        assert_eq!(crossed_back(Side::Left, (1919.5, 539.5), extent), Some(0.5));
        assert_eq!(crossed_back(Side::Right, (-0.1, 1079.0), extent), Some(1.0));
        assert_eq!(crossed_back(Side::Right, (5.0, 10.0), extent), None);
        assert_eq!(crossed_back(Side::Above, (0.0, 1080.0), extent), Some(0.0));
        assert_eq!(crossed_back(Side::Below, (959.5, -1.0), extent), Some(0.5));
    }
}
