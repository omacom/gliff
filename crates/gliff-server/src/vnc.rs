//! `gliff-server --vnc`: serve an output to VNC clients, so that any device
//! with a VNC viewer, however old, can be one more screen of this machine.
//!
//! A headless output (or a named one) is set up once and lives as long as
//! the server: a client that sleeps or loses the network comes back to the
//! same windows. One client is served at a time; a new one that passes the
//! password takes over from the previous one.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use gliff_vnc::encode::{number, Encoder, Image, Method, Rect};
use gliff_vnc::keys::Keys;
use gliff_vnc::pixel::{Converter, PixelFormat};
use gliff_vnc::proto::{self, ClientMsg, Hello, Security};
use gliff_vnc::screen::Screen;
use hypr_capture::{CaptureConfig, CaptureEvent, CapturedFrame, Capturer};
use hypr_input::{Axis, Input, InputCmd, InputConfig};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{channel, unbounded_channel, Sender, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::session::{self, ServedOutput, SessionOutput};

pub struct Config {
    /// Address to listen on, e.g. `0.0.0.0:5900`.
    pub listen: String,
    /// `None` serves without authentication.
    pub password: Option<Vec<u8>>,
    /// The headless output's mode: width, height and scale.
    pub mode: (u32, u32, f32),
    /// The desktop name clients show.
    pub name: String,
}

/// The fastest the screen is captured for a client.
const MAX_FPS: u64 = 30;
/// How long a new client's first update waits for a frame captured after
/// it connected, rather than one from before.
const FIRST_FRAME_WAIT: Duration = Duration::from_millis(250);
/// How often the output's mode is checked against the one it was given.
const MODE_CHECK: Duration = Duration::from_secs(2);
/// How long a client may take to log in.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// The wait before a client hears its password was wrong. With at most
/// MAX_HANDSHAKES logins at once, that is two guesses a second.
const WRONG_PASSWORD_DELAY: Duration = Duration::from_secs(2);
/// Logins in progress at once.
const MAX_HANDSHAKES: usize = 4;
/// Updates waiting for a client to read them before it counts as stalled.
const MAX_QUEUED_UPDATES: usize = 4;
/// How long one write to a client may take before it counts as stalled.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Client messages read ahead of the server; past that, reading waits.
const MAX_QUEUED_MESSAGES: usize = 256;

/// A client's message, or why its connection ended, under its id.
type Message = (u64, Result<ClientMsg, String>);

/// RFB pointer button bits and the evdev buttons they press.
const BUTTONS: [(u8, u32); 3] = [
    (1, 0x110), // left: BTN_LEFT
    (2, 0x112), // middle: BTN_MIDDLE
    (4, 0x111), // right: BTN_RIGHT
];
/// RFB wheel bits: up, down, left, right.
const WHEEL: [(u8, Axis, f64); 4] = [
    (8, Axis::Vertical, -1.0),
    (16, Axis::Vertical, 1.0),
    (32, Axis::Horizontal, -1.0),
    (64, Axis::Horizontal, 1.0),
];

enum Event {
    Frame(CapturedFrame),
    Stopped,
    Error(String),
}

pub async fn run(cfg: session::Config, vnc: Config) -> Result<()> {
    let instance = cfg.target.instance().context("find Hyprland instance")?;
    let (width, height, scale) = vnc.mode;
    let (width, height) = session::headless_target(width, height, scale);
    let output = session::setup_output(&instance, &cfg, (width, height, scale))?;
    tracing::info!(output = %output.name, output.width, output.height, output.scale, "VNC output ready");

    let addresses: Vec<SocketAddr> = tokio::net::lookup_host(&vnc.listen)
        .await
        .with_context(|| format!("resolve {}", vnc.listen))?
        .collect();
    let listener = TcpListener::bind(addresses.as_slice())
        .await
        .with_context(|| format!("listen on {}", vnc.listen))?;

    let (events, mut event_rx) = unbounded_channel();
    let capturer = start_capture(&cfg, &output, events)?;
    let keys = Keys::new();
    let mut ic = InputConfig::new(output.name.clone());
    ic.target = cfg.target.clone();
    ic.keymap = Some(keys.keymap());
    let input = Input::start(ic, Box::new(|_| {}))?;
    input.send(output.logical_extent())?;
    let served = ServedOutput {
        output,
        capturer: Some(capturer),
        input: Some(input),
    };
    tracing::info!(listen = %vnc.listen, password = vnc.password.is_some(), "serving VNC");

    let security = match &vnc.password {
        Some(p) => Security::Password(p.clone()),
        None => Security::None,
    };
    let mut server = Server {
        served,
        keys,
        screen: None,
        client: None,
        next_id: 0,
        frame_in_flight: false,
        next_frame_at: Instant::now(),
    };
    let (logins, mut login_rx) = unbounded_channel();
    let (messages, mut message_rx) = channel::<Message>(MAX_QUEUED_MESSAGES);
    let handshakes = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let mut terminate = signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut mode_check = tokio::time::interval(MODE_CHECK);
    mode_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let wants_frame = server.wants_frame();
        let stale_until = server.client.as_ref().and_then(Client::stale_until);
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                if handshakes.get() >= MAX_HANDSHAKES {
                    tracing::warn!(%peer, "too many logins at once; refusing");
                    continue;
                }
                let slot = LoginSlot::take(&handshakes);
                let (security, logins) = (security.clone(), logins.clone());
                let size = server.size();
                let name = vnc.name.clone();
                tokio::task::spawn_local(async move {
                    // Given back however the login ends, a panic included.
                    let _slot = slot;
                    login(stream, peer, security, size, name, logins).await;
                });
            }
            Some((stream, hello, peer)) = login_rx.recv() => {
                server.take_client(stream, hello, peer, &messages);
            }
            Some((id, msg)) = message_rx.recv() => {
                if server.client.as_ref().is_some_and(|c| c.id == id) {
                    match msg {
                        Ok(msg) => {
                            if let Err(e) = server.on_message(msg) {
                                server.drop_client(&format!("{e:#}"));
                            }
                        }
                        Err(e) => server.drop_client(&e),
                    }
                }
            }
            Some(event) = event_rx.recv() => match event {
                Event::Frame(frame) => {
                    server.frame_in_flight = false;
                    server.on_frame(frame);
                    if let Some(client) = &mut server.client {
                        client.fresh = true;
                    }
                    if let Err(e) = server.send_update() {
                        server.drop_client(&format!("{e:#}"));
                    }
                }
                Event::Stopped => anyhow::bail!("the capture stopped"),
                Event::Error(e) => anyhow::bail!("capture: {e}"),
            },
            _ = tokio::time::sleep_until(server.next_frame_at), if wants_frame => {
                server.request_frame()?;
            }
            // No new frame came: the screen has not changed since the last.
            _ = tokio::time::sleep_until(stale_until.unwrap_or_else(Instant::now)), if stale_until.is_some() => {
                if let Some(client) = &mut server.client {
                    client.fresh = true;
                }
                if let Err(e) = server.send_update() {
                    server.drop_client(&format!("{e:#}"));
                }
            }
            // The screen outlives config reloads, which drop its mode.
            _ = mode_check.tick() => {
                server.served.output.restore_mode(&instance);
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
            _ = hangup.recv() => break,
        }
    }
    tracing::info!("stopping; giving the screen back");
    server.drop_client("the server is stopping");
    Ok(())
}

fn signal(kind: tokio::signal::unix::SignalKind) -> Result<tokio::signal::unix::Signal> {
    Ok(tokio::signal::unix::signal(kind)?)
}

type Login = (TcpStream, Hello, SocketAddr);

/// One of the MAX_HANDSHAKES logins in progress, given back when dropped.
struct LoginSlot(std::rc::Rc<std::cell::Cell<usize>>);

impl LoginSlot {
    fn take(count: &std::rc::Rc<std::cell::Cell<usize>>) -> Self {
        count.set(count.get() + 1);
        Self(count.clone())
    }
}

impl Drop for LoginSlot {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// Log a client in, and hand it to the server once its password checks out.
async fn login(
    mut stream: TcpStream,
    peer: SocketAddr,
    security: Security,
    (width, height): (u16, u16),
    name: String,
    logins: UnboundedSender<Login>,
) {
    let _ = stream.set_nodelay(true);
    let challenge = match challenge() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "no randomness for the login challenge");
            return;
        }
    };
    let handshake = proto::handshake(
        &mut stream,
        proto::Login {
            security: &security,
            challenge,
            wrong_password_delay: WRONG_PASSWORD_DELAY,
            size: (width, height),
            name: &name,
        },
    );
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(hello)) => {
            tracing::info!(%peer, version = ?hello.version, "VNC client logged in");
            let _ = logins.send((stream, hello, peer));
        }
        Ok(Err(proto::Error::WrongPassword)) => tracing::warn!(%peer, "wrong VNC password"),
        Ok(Err(e)) => tracing::warn!(%peer, error = %e, "VNC login failed"),
        Err(_) => tracing::warn!(%peer, "VNC login timed out"),
    }
}

fn challenge() -> std::io::Result<[u8; 16]> {
    use std::io::Read;
    let mut c = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut c)?;
    Ok(c)
}

fn start_capture(
    cfg: &session::Config,
    output: &SessionOutput,
    events: UnboundedSender<Event>,
) -> Result<Capturer> {
    let mut cc = CaptureConfig::new(output.name.clone());
    cc.target = cfg.target.clone();
    cc.render_node = cfg.render_node.clone();
    cc.mode = cfg.capture;
    // Frames are read on the CPU, and the client has no cursor of its own.
    cc.prefer_linear = true;
    cc.shm_fallback = true;
    cc.cursor = false;
    cc.paint_cursors = true;
    if output.is_headless() {
        cc.max_size = Some(session::HEADLESS_LIMIT);
    }
    let sink = Box::new(move |ev: CaptureEvent| {
        let event = match ev {
            CaptureEvent::Frame(frame) => Event::Frame(frame),
            CaptureEvent::Stopped => Event::Stopped,
            CaptureEvent::Error(e) => Event::Error(e),
            _ => return,
        };
        let _ = events.send(event);
    });
    Ok(Capturer::start(cc, sink)?)
}

struct Client {
    id: u64,
    peer: SocketAddr,
    /// Bytes for the writer task, which sends them so a client that stops
    /// reading never holds up the server.
    out: Sender<Vec<u8>>,
    /// The tasks reading and writing its connection, stopped with it.
    tasks: [JoinHandle<()>; 2],
    encoder: Encoder,
    desktop_size: bool,
    /// The screen changed size since the client was last told.
    resized: bool,
    /// The update the client is waiting for.
    pending: Option<Rect>,
    buttons: u8,
    pointer: (u16, u16),
    /// A frame was captured since the client connected.
    fresh: bool,
    connected_at: Instant,
}

impl Drop for Client {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Client {
    /// Queue `bytes` for the client; an error when it has stopped reading.
    fn send(&self, bytes: Vec<u8>) -> Result<()> {
        self.out.try_send(bytes).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => {
                anyhow::anyhow!("the client stopped reading its updates")
            }
            tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                anyhow::anyhow!("the connection to the client ended")
            }
        })
    }

    /// Until when the first update waits for a fresh frame.
    fn stale_until(&self) -> Option<Instant> {
        (!self.fresh).then(|| self.connected_at + FIRST_FRAME_WAIT)
    }
}

struct Server {
    served: ServedOutput<SessionOutput, Capturer, Input>,
    keys: Keys,
    screen: Option<Screen>,
    client: Option<Client>,
    next_id: u64,
    frame_in_flight: bool,
    next_frame_at: Instant,
}

impl Server {
    fn input(&self) -> &Input {
        self.served
            .input
            .as_ref()
            .expect("input lives until the drop")
    }

    fn size(&self) -> (u16, u16) {
        match &self.screen {
            Some(s) => {
                let (w, h) = s.size();
                (w as u16, h as u16)
            }
            None => (self.served.width as u16, self.served.height as u16),
        }
    }

    /// Capture while a client is connected. The compositor completes a
    /// capture only once the screen changes, so a still screen costs
    /// nothing, and the client's next update is ready when it asks.
    fn wants_frame(&self) -> bool {
        !self.frame_in_flight && self.client.is_some()
    }

    fn request_frame(&mut self) -> Result<()> {
        self.served
            .capturer
            .as_ref()
            .expect("the capturer lives until the drop")
            .request_frame()?;
        self.frame_in_flight = true;
        self.next_frame_at = Instant::now() + Duration::from_millis(1000 / MAX_FPS);
        Ok(())
    }

    fn take_client(
        &mut self,
        stream: TcpStream,
        hello: Hello,
        peer: SocketAddr,
        messages: &Sender<Message>,
    ) {
        if self.client.is_some() {
            self.drop_client("another client logged in");
        }
        let (mut reader, mut writer) = stream.into_split();
        self.next_id += 1;
        let id = self.next_id;
        let read_messages = messages.clone();
        let read = tokio::task::spawn_local(async move {
            loop {
                let msg = proto::read_msg(&mut reader)
                    .await
                    .map_err(|e| e.to_string());
                let end = msg.is_err();
                if read_messages.send((id, msg)).await.is_err() || end {
                    return;
                }
            }
        });
        let (out, mut queued) = channel::<Vec<u8>>(MAX_QUEUED_UPDATES);
        let write_messages = messages.clone();
        let write = tokio::task::spawn_local(async move {
            while let Some(bytes) = queued.recv().await {
                let why = match tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(&bytes)).await
                {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) => format!("write: {e}"),
                    Err(_) => "the client stopped reading".to_string(),
                };
                let _ = write_messages.send((id, Err(why))).await;
                return;
            }
        });
        if let Some(screen) = &mut self.screen {
            let (w, h) = screen.size();
            screen.invalidate(Rect {
                x: 0,
                y: 0,
                width: w as u16,
                height: h as u16,
            });
        }
        tracing::info!(%peer, shared = hello.shared, "VNC client connected");
        self.client = Some(Client {
            id,
            peer,
            out,
            tasks: [read, write],
            encoder: Encoder::new(Converter::new(PixelFormat::BGRX), Method::Raw),
            desktop_size: false,
            resized: false,
            pending: None,
            buttons: 0,
            pointer: (0, 0),
            fresh: false,
            connected_at: Instant::now(),
        });
    }

    fn drop_client(&mut self, why: &str) {
        let Some(client) = self.client.take() else {
            return;
        };
        tracing::info!(peer = %client.peer, why, "VNC client disconnected");
        // Nothing the client held may stay pressed.
        self.keys.release_all();
        let _ = self.input().send(InputCmd::ReleaseAll);
    }

    fn on_message(&mut self, msg: ClientMsg) -> Result<()> {
        let Some(client) = &mut self.client else {
            return Ok(());
        };
        match msg {
            ClientMsg::SetPixelFormat(format) => {
                format
                    .check()
                    .map_err(|why| anyhow::anyhow!("the client asked for {why}"))?;
                tracing::info!(?format, "client pixel format");
                client.encoder.converter = Converter::new(format);
                if let Some(map) = client.encoder.converter.colour_map() {
                    let mut out = Vec::new();
                    proto::colour_map(&map, &mut out);
                    client.send(out)?;
                }
            }
            ClientMsg::SetEncodings(list) => {
                let method = Method::choose(&list);
                tracing::info!(?method, encodings = ?list, "client encodings");
                // ZRLE runs one zlib stream for the whole connection, so
                // the encoder, and its stream, stay.
                client.encoder.method = method;
                client.desktop_size = list.contains(&number::DESKTOP_SIZE);
            }
            ClientMsg::UpdateRequest { incremental, area } => {
                if !incremental {
                    if let Some(screen) = &mut self.screen {
                        screen.invalidate(area);
                    }
                }
                client.pending = Some(area);
                self.send_update()?;
            }
            ClientMsg::Key { down, keysym } => {
                if let Some(stroke) = self.keys.event(keysym, down) {
                    if let Some(keymap) = stroke.keymap {
                        self.input().send(InputCmd::SetKeymap(keymap))?;
                    }
                    self.input().key(stroke.code, stroke.pressed)?;
                }
            }
            ClientMsg::Pointer { buttons, x, y } => {
                let (prev, pos) = (client.buttons, client.pointer);
                client.buttons = buttons;
                client.pointer = (x, y);
                if pos != (x, y) {
                    let s = f64::from(self.served.scale.max(0.5));
                    self.input().motion(f64::from(x) / s, f64::from(y) / s)?;
                }
                for (bit, button) in BUTTONS {
                    if (prev ^ buttons) & bit != 0 {
                        self.input().button(button, buttons & bit != 0)?;
                    }
                }
                // A wheel step is a press of its button.
                for (bit, axis, sign) in WHEEL {
                    if buttons & bit != 0 && prev & bit == 0 {
                        self.input().send(InputCmd::Axis {
                            axis,
                            value: 15.0 * sign,
                            discrete: Some(sign as i32),
                            stop: false,
                        })?;
                    }
                }
            }
            ClientMsg::CutText(text) => {
                tracing::debug!(bytes = text.len(), "client clipboard text (not used)");
            }
        }
        Ok(())
    }

    fn on_frame(&mut self, frame: CapturedFrame) {
        let image = match frame.read_bgra() {
            Ok(image) => image,
            Err(e) => {
                // A dmabuf the CPU cannot map: capture into shared memory.
                tracing::warn!(error = %e, "cannot read the frame; capturing into shared memory");
                if let Some(c) = &self.served.capturer {
                    let _ = c.use_shm();
                }
                return;
            }
        };
        let damage: Option<Vec<Rect>> = (frame.transform == 0).then(|| {
            frame
                .damage
                .iter()
                .map(|r| Rect {
                    x: r.x.clamp(0, i32::from(u16::MAX)) as u16,
                    y: r.y.clamp(0, i32::from(u16::MAX)) as u16,
                    width: r.width.clamp(0, i32::from(u16::MAX)) as u16,
                    height: r.height.clamp(0, i32::from(u16::MAX)) as u16,
                })
                .collect()
        });
        let damage = damage.filter(|d| !d.is_empty());
        let resized = self
            .screen
            .as_ref()
            .is_none_or(|s| s.size() != (image.width, image.height));
        if resized {
            let first = self.screen.is_none();
            self.screen = Some(Screen::new(image.width, image.height));
            // A headless screen keeps the mode it was made with, for
            // restore_mode to put back after a reload changed it; only an
            // existing screen follows what it shows.
            if !self.served.is_headless() {
                self.served.width = image.width as u32;
                self.served.height = image.height as u32;
            }
            // The pointer covers what the viewer sees now, in the units
            // Pointer events are divided into, even while a reload has
            // the headless screen at another size for a moment.
            let s = self.served.scale.max(0.5);
            let _ = self.input().send(InputCmd::SetExtent {
                width: (image.width as f32 / s).round().max(1.0) as u32,
                height: (image.height as f32 / s).round().max(1.0) as u32,
            });
            if !first {
                if let Some(client) = &mut self.client {
                    if client.desktop_size {
                        // Told with the next update, which covers it all.
                        client.resized = true;
                        client.pending = Some(Rect {
                            x: 0,
                            y: 0,
                            width: image.width as u16,
                            height: image.height as u16,
                        });
                    } else {
                        self.drop_client("the screen changed size");
                    }
                }
            }
            self.screen
                .as_mut()
                .expect("just set")
                .update(&image.pixels, None);
            return;
        }
        self.screen
            .as_mut()
            .expect("checked above")
            .update(&image.pixels, damage.as_deref());
    }

    /// Answer the client's pending request if anything in it changed.
    fn send_update(&mut self) -> Result<()> {
        let (Some(client), Some(screen)) = (&mut self.client, &mut self.screen) else {
            return Ok(());
        };
        let Some(area) = client.pending else {
            return Ok(());
        };
        if !client.fresh {
            return Ok(());
        }
        let (w, h) = screen.size();
        let resized = std::mem::take(&mut client.resized);
        if !resized && !screen.has_changes_in(area) {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let rects = screen.take_changes(area);
        let image = Image {
            width: w,
            height: h,
            pixels: screen.latest(),
        };
        let mut out = Vec::with_capacity(1 << 16);
        let extra = u16::from(resized);
        proto::update_header(rects.len() as u16 + extra, &mut out);
        if resized {
            proto::desktop_size(w as u16, h as u16, &mut out);
        }
        for rect in rects {
            client.encoder.rect(&image, rect, &mut out);
        }
        client.pending = None;
        let (bytes, encoded) = (out.len(), started.elapsed());
        client.send(out)?;
        tracing::debug!(
            bytes,
            encode_ms = encoded.as_secs_f64() * 1000.0,
            "update queued"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_slot_is_given_back_however_the_login_ends() {
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let _a = LoginSlot::take(&count);
            let _b = LoginSlot::take(&count);
            assert_eq!(count.get(), 2);
        }
        assert_eq!(count.get(), 0);
        let c = count.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _slot = LoginSlot::take(&c);
            panic!("a login that panics");
        }));
        assert!(panicked.is_err());
        assert_eq!(count.get(), 0);
    }
}
