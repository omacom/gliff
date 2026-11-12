//! Remote Desktop (RDP) through IronRDP: TLS, with Network Level
//! Authentication (CredSSP, NTLM) when the server asks for it.
//!
//! Most servers show a certificate of their own making. The first one seen
//! is kept by the caller and any other refused after, as ssh keeps host
//! keys; one an authority vouches for is checked as usual.

use std::time::Duration;

use gliff_proto::{Axis, ClientMsg};
use ironrdp_connector::connection_activation::{
    ConnectionActivationFactory, ConnectionActivationState,
};
use ironrdp_connector::sspi::generator::NetworkRequest;
use ironrdp_connector::{
    ClientConnector, ConnectorError, ConnectorErrorExt as _, ConnectorErrorKind, ConnectorResult,
    Credentials as RdpCredentials, DesktopSize, Sequence as _, ServerName,
};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::{
    synchronize_event, Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations,
};
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::input::fast_path::FastPathInputEvent;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, TokioFramed};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{keys, Credentials, Cursor, Error, Event, Purpose, Scroll, Target};

/// How often a changed screen is sent on, at most.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// How long a server that answered has to finish TLS and the login.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// The screen asked for when driving from the edge: the server decides.
const CONTROL_SIZE: DesktopSize = DesktopSize {
    width: 1920,
    height: 1080,
};
/// RDP wheel units in a notch, against Wayland's.
const WHEEL_PER_NOTCH: i32 = 120;

/// Whether `stream` answers an RDP connection request: an X.224
/// connection confirm.
pub(crate) async fn answers(mut stream: TcpStream) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    // TPKT header, X.224 connection request, RDP negotiation request for
    // TLS and CredSSP.
    const REQUEST: [u8; 19] = [
        0x03, 0x00, 0x00, 0x13, 0x0E, 0xE0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x00,
        0x03, 0x00, 0x00, 0x00,
    ];
    if stream.write_all(&REQUEST).await.is_err() {
        return false;
    }
    let mut reply = [0u8; 7];
    stream.read_exact(&mut reply).await.is_ok() && is_connection_confirm(&reply)
}

fn is_connection_confirm(reply: &[u8; 7]) -> bool {
    reply[0] == 0x03 && reply[1] == 0x00 && reply[5] & 0xF0 == 0xD0
}

/// `DOMAIN\user` as a domain and a user; `user@domain` stays whole, as
/// Windows takes it so.
fn split_domain(user: &str) -> (Option<String>, String) {
    match user.split_once('\\') {
        Some((domain, user)) if !domain.is_empty() => (Some(domain.into()), user.into()),
        _ => (None, user.into()),
    }
}

fn config(credentials: &Credentials, size: DesktopSize) -> ironrdp_connector::Config {
    let (domain, username) = split_domain(&credentials.user);
    ironrdp_connector::Config {
        credentials: RdpCredentials::UsernamePassword {
            username,
            password: credentials.password.clone(),
        },
        domain,
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        // The server's own layout: gliff sends key positions.
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: ironrdp_pdu::gcc::ConnectionType::Lan,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: size,
        monitor_layout: None,
        // RemoteFX through surface commands, which some servers require
        // (weston), besides the plain bitmaps every server sends.
        bitmap: Some(ironrdp_connector::BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs: ironrdp_pdu::rdp::capability_sets::client_codecs_capabilities(&[])
                .unwrap_or_default(),
        }),
        client_build: 0,
        client_name: "gliff".into(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".into(),
        platform: MajorPlatformType::UNIX,
        // The cursor comes as images for the window to show, not drawn into
        // the screen.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        compression_type: None,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        desktop_physical_size: None,
        desktop_orientation: None,
        device_scale_factor: None,
        monitor_extended_layout: None,
        relative_mouse_input: false,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp_pdu::rdp::capability_sets::RailSupportLevel::empty(),
    }
}

/// The clipboard channel, present and silent: gliff shares no clipboard
/// with these machines, but some servers end a session that lacks the
/// channel (weston).
#[derive(Debug)]
struct NoClipboard;

ironrdp_svc::impl_as_any!(NoClipboard);

impl ironrdp_svc::SvcProcessor for NoClipboard {
    fn channel_name(&self) -> ironrdp_pdu::gcc::ChannelName {
        ironrdp_pdu::gcc::ChannelName::from_static(b"cliprdr\0")
    }

    fn process(&mut self, _payload: &[u8]) -> ironrdp_pdu::PduResult<Vec<ironrdp_svc::SvcMessage>> {
        Ok(Vec::new())
    }
}

impl ironrdp_svc::SvcClientProcessor for NoClipboard {}

/// CredSSP asks for this only to reach a Kerberos KDC; gliff logs in with
/// NTLM.
struct NoKdc;

impl ironrdp_tokio::NetworkClient for NoKdc {
    async fn send(&mut self, _request: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        Err(ConnectorError::general(
            "Kerberos is not supported; use NTLM",
        ))
    }
}

/// Why a connection failed, telling a refused login from the rest.
fn connect_error(target: &Target, e: ConnectorError) -> Error {
    tracing::debug!(error = ?e, "RDP connection failed");
    match e.kind() {
        ConnectorErrorKind::Credssp(_) | ConnectorErrorKind::AccessDenied => {
            Error::Auth(format!("{} refused the user name or password", target.host))
        }
        _ => Error::other(format!("{e}")),
    }
}

type Connected = (
    TokioFramed<crate::tls::TlsStream<TcpStream>>,
    ironrdp_connector::ConnectionResult,
    Option<String>,
);

/// Connect and log in with `config`: the session's stream, what the server
/// agreed to, and the fingerprint of a certificate no authority vouches for.
async fn connect(
    target: &Target,
    config: &ironrdp_connector::Config,
    known_certificate: Option<&str>,
) -> Result<Connected, Error> {
    let stream = crate::reach(target).await?;
    let _ = stream.set_nodelay(true);
    let client_addr = stream
        .local_addr()
        .map_err(|e| Error::other(format!("local address: {e}")))?;
    let mut framed = TokioFramed::new(stream);
    let mut connector =
        ClientConnector::new(config.clone(), client_addr).with_static_channel(NoClipboard);
    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .map_err(|e| connect_error(target, e))?;

    let initial = framed.into_inner_no_leftover();
    let (tls, certificate, seen) = crate::tls::upgrade(initial, target, known_certificate).await?;
    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&certificate)
        .ok_or_else(|| Error::other("the server's certificate carries no public key"))?
        .to_owned();
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new(tls);
    let result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoKdc,
        ServerName::new(target.host.clone()),
        server_public_key,
        None,
    )
    .await
    .and_then(ironrdp_connector::ConnectionOutcome::into_connected)
    .map_err(|e| connect_error(target, e))?;
    Ok((framed, result, seen))
}

pub(crate) async fn run(
    target: &Target,
    credentials: &Credentials,
    purpose: Purpose,
    known_certificate: Option<&str>,
    input: &mut UnboundedReceiver<ClientMsg>,
    events: &mut dyn FnMut(Event),
) -> Result<(), Error> {
    let size = match purpose {
        Purpose::View { width, height } => DesktopSize {
            width: width.clamp(200, 8192),
            height: height.clamp(200, 8192),
        },
        Purpose::Control => CONTROL_SIZE,
    };
    let mut config = config(credentials, size);
    let setup = async {
        match connect(target, &config, known_certificate).await {
            // Some servers offer NLA without a way to log in through it,
            // and take the login over TLS alone (weston); one that requires
            // NLA, as Windows does by default, refuses this second try, and
            // the first refusal stands.
            Err(Error::Auth(why)) => {
                tracing::info!(host = %target.host, "NLA refused the login; trying TLS alone");
                config.enable_credssp = false;
                match connect(target, &config, known_certificate).await {
                    Ok(connected) => Ok(connected),
                    Err(e) => {
                        tracing::debug!(error = %e, "TLS alone failed too");
                        Err(Error::Auth(why))
                    }
                }
            }
            connected => connected,
        }
    };
    let Some((framed, result, seen)) = crate::set_up(target, SETUP_TIMEOUT, input, setup).await?
    else {
        return Ok(());
    };
    if let Some(seen) = seen {
        events(Event::NewCertificate(seen));
    }
    let factory =
        ConnectionActivationFactory::new(config, result.io_channel_id, result.user_channel_id);
    let stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let size = result.desktop_size;
    events(Event::Connected {
        width: size.width.into(),
        height: size.height.into(),
        video: "RDP".into(),
    });
    Session {
        framed,
        stage,
        image: DecodedImage::new(PixelFormat::BgrA32, size.width, size.height),
        input_db: Database::new(),
        scroll: Scroll::default(),
        view: matches!(purpose, Purpose::View { .. }),
        factory,
        redraw: (result.refresh_rect_support, result.suppress_output_support),
    }
    .run(input, events)
    .await
}

struct Session<S> {
    framed: TokioFramed<S>,
    stage: ActiveStage,
    image: DecodedImage,
    input_db: Database,
    scroll: Scroll,
    view: bool,
    factory: ConnectionActivationFactory,
    /// How the server lets the client ask for the whole screen again:
    /// Refresh Rect, Suppress Output.
    redraw: (bool, bool),
}

impl<S> Session<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    async fn run(
        mut self,
        input: &mut UnboundedReceiver<ClientMsg>,
        events: &mut dyn FnMut(Event),
    ) -> Result<(), Error> {
        let mut tick = tokio::time::interval(FRAME_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut dirty = false;
        // Some servers draw only what changes (weston): ask for the whole
        // screen, so the window does not stay black until something moves.
        self.redraw().await?;
        loop {
            tokio::select! {
                pdu = self.framed.read_pdu() => {
                    let (action, payload) =
                        pdu.map_err(|e| Error::other(format!("read from the server: {e}")))?;
                    let received_at = self.framed.last_read_at();
                    let outputs = self
                        .stage
                        .process(&mut self.image, action, &payload, received_at)
                        .map_err(|e| Error::other(format!("{e}")))?;
                    if !self.act(outputs, &mut dirty, events).await? {
                        return Ok(());
                    }
                }
                msg = input.recv() => {
                    let Some(msg) = msg else {
                        self.shut_down().await;
                        return Ok(());
                    };
                    let events_out = self.translate(msg);
                    if !events_out.is_empty() {
                        let outputs = self
                            .stage
                            .process_fastpath_input(&mut self.image, &events_out)
                            .map_err(|e| Error::other(format!("{e}")))?;
                        if !self.act(outputs, &mut dirty, events).await? {
                            return Ok(());
                        }
                    }
                }
                _ = tick.tick(), if dirty && self.view => {
                    dirty = false;
                    events(self.frame());
                }
            }
        }
    }

    /// Carry out what the session asks; false once the server ended it.
    async fn act(
        &mut self,
        outputs: Vec<ActiveStageOutput>,
        dirty: &mut bool,
        events: &mut dyn FnMut(Event),
    ) -> Result<bool, Error> {
        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => self.write(&frame).await?,
                ActiveStageOutput::GraphicsUpdate(_) => *dirty = true,
                ActiveStageOutput::PointerBitmap(p) if self.view => {
                    let mut bgra = p.bitmap_data.clone();
                    for px in bgra.as_chunks_mut::<4>().0 {
                        px.swap(0, 2);
                    }
                    events(Event::Cursor(Cursor {
                        width: p.width.into(),
                        height: p.height.into(),
                        hot_x: p.hotspot_x.into(),
                        hot_y: p.hotspot_y.into(),
                        argb: bgra,
                    }));
                }
                ActiveStageOutput::PointerHidden if self.view => events(Event::CursorHidden),
                ActiveStageOutput::Terminate(reason) => {
                    tracing::info!(%reason, "the RDP server ended the session");
                    return Ok(false);
                }
                ActiveStageOutput::DeactivateAll => {
                    self.reactivate(events).await?;
                    *dirty = true;
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// Ask for the whole screen, where the server lets the client.
    async fn redraw(&mut self) -> Result<(), Error> {
        if !self.view {
            return Ok(());
        }
        let (refresh, suppress) = self.redraw;
        let frames = self
            .stage
            .request_full_redraw(self.image.width(), self.image.height(), refresh, suppress)
            .map_err(|e| Error::other(format!("{e}")))?;
        for frame in frames {
            self.write(&frame).await?;
        }
        Ok(())
    }

    async fn write(&mut self, frame: &[u8]) -> Result<(), Error> {
        self.framed
            .write_all(frame)
            .await
            .map_err(|e| Error::other(format!("write to the server: {e}")))
    }

    /// The server deactivated the session, as it does when the screen
    /// changes size: run the capability exchange again.
    async fn reactivate(&mut self, events: &mut dyn FnMut(Event)) -> Result<(), Error> {
        let mut sequence = self.factory.create();
        let mut buf = ironrdp_pdu::WriteBuf::new();
        loop {
            if let ConnectionActivationState::Finalized {
                desktop_size,
                share_id,
                enable_server_pointer,
                pointer_software_rendering,
                static_channel_chunk_size,
                refresh_rect_support,
                suppress_output_support,
                ..
            } = sequence.connection_activation_state()
            {
                if !self.stage.reactivate(
                    self.factory.io_channel_id(),
                    self.factory.user_channel_id(),
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    static_channel_chunk_size,
                ) {
                    return Err(Error::other(
                        "the server reactivated with a bad channel size",
                    ));
                }
                if (desktop_size.width, desktop_size.height)
                    != (self.image.width(), self.image.height())
                {
                    self.image = DecodedImage::new(
                        PixelFormat::BgrA32,
                        desktop_size.width,
                        desktop_size.height,
                    );
                    events(Event::Resized {
                        width: desktop_size.width.into(),
                        height: desktop_size.height.into(),
                    });
                }
                self.redraw = (refresh_rect_support, suppress_output_support);
                return self.redraw().await;
            }
            if sequence.state().is_terminal() {
                return Err(Error::other(
                    "the server ended the session while reactivating",
                ));
            }
            buf.clear();
            ironrdp_tokio::single_sequence_step(&mut self.framed, &mut sequence, &mut buf)
                .await
                .map_err(|e| Error::other(format!("reactivate: {e}")))?;
        }
    }

    async fn shut_down(&mut self) {
        if let Ok(outputs) = self.stage.graceful_shutdown() {
            for out in outputs {
                if let ActiveStageOutput::ResponseFrame(frame) = out {
                    let _ = self.write(&frame).await;
                }
            }
        }
    }

    fn frame(&self) -> Event {
        let (w, h) = (
            usize::from(self.image.width()),
            usize::from(self.image.height()),
        );
        let stride = self.image.stride();
        let data = self.image.data();
        let mut bgrx = Vec::with_capacity(w * h * 4);
        for row in data.chunks(stride).take(h) {
            bgrx.extend_from_slice(&row[..w * 4]);
        }
        Event::Frame {
            width: w as u32,
            height: h as u32,
            bgrx,
        }
    }

    fn translate(&mut self, msg: ClientMsg) -> Vec<FastPathInputEvent> {
        tracing::trace!(?msg, "input for the RDP server");
        let ops: Vec<Operation> = match msg {
            ClientMsg::Key { keycode, pressed } => match keys::scancode(keycode) {
                Some(code) => {
                    let code = Scancode::from_u16(code);
                    vec![if pressed {
                        Operation::KeyPressed(code)
                    } else {
                        Operation::KeyReleased(code)
                    }]
                }
                None => {
                    tracing::debug!(keycode, "no RDP scan code for this key");
                    Vec::new()
                }
            },
            ClientMsg::PointerMotion { x, y } => vec![Operation::MouseMove(MousePosition {
                x: to_u16(x, self.image.width()),
                y: to_u16(y, self.image.height()),
            })],
            ClientMsg::PointerButton { button, pressed } => match mouse_button(button) {
                Some(b) if pressed => vec![Operation::MouseButtonPressed(b)],
                Some(b) => vec![Operation::MouseButtonReleased(b)],
                None => Vec::new(),
            },
            ClientMsg::PointerAxis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let notches = self.scroll.notches(axis, value, discrete, stop);
                if notches == 0 {
                    Vec::new()
                } else {
                    // RDP scrolls up, and right, with positive units.
                    let units = (notches * WHEEL_PER_NOTCH).clamp(-255 * 4, 255 * 4);
                    let (is_vertical, units) = match axis {
                        Axis::Vertical => (true, -units),
                        Axis::Horizontal => (false, units),
                    };
                    vec![Operation::WheelRotations(WheelRotations {
                        is_vertical,
                        rotation_units: units as i16,
                    })]
                }
            }
            ClientMsg::KeyboardLocks { locked, .. } => {
                let has = |name: &str| locked.iter().any(|l| l == name);
                return vec![synchronize_event(false, has("Mod2"), has("Lock"), false)];
            }
            _ => Vec::new(),
        };
        if ops.is_empty() {
            return Vec::new();
        }
        self.input_db.apply(ops).into_vec()
    }
}

fn to_u16(v: f64, len: u16) -> u16 {
    v.round().clamp(0.0, f64::from(len.saturating_sub(1))) as u16
}

/// The RDP button for an evdev button code.
fn mouse_button(button: u32) -> Option<MouseButton> {
    match button {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        0x113 => Some(MouseButton::X1),
        0x114 => Some(MouseButton::X2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_confirm_is_rdp() {
        assert!(is_connection_confirm(&[3, 0, 0, 19, 14, 0xD0, 0]));
        assert!(!is_connection_confirm(b"SSH-2.0"));
        assert!(!is_connection_confirm(&[3, 0, 0, 19, 14, 0xE0, 0]));
    }

    #[test]
    fn a_domain_comes_before_a_backslash() {
        assert_eq!(
            split_domain("CORP\\ann"),
            (Some("CORP".into()), "ann".into())
        );
        assert_eq!(
            split_domain("ann@corp.example"),
            (None, "ann@corp.example".into())
        );
        assert_eq!(split_domain("ann"), (None, "ann".into()));
        assert_eq!(split_domain("\\ann"), (None, "\\ann".into()));
    }

    #[test]
    fn fingerprints_read_like_openssl() {
        let fp = crate::tls::fingerprint(b"abc");
        assert!(fp.starts_with("BA:78:16:BF"));
        assert_eq!(fp.len(), 32 * 3 - 1);
    }

    #[test]
    fn evdev_buttons_map_to_rdp_buttons() {
        assert_eq!(mouse_button(0x110), Some(MouseButton::Left));
        assert_eq!(mouse_button(0x111), Some(MouseButton::Right));
        assert_eq!(mouse_button(0x112), Some(MouseButton::Middle));
        assert_eq!(mouse_button(0x200), None);
    }

    #[test]
    fn positions_stay_on_the_screen() {
        assert_eq!(to_u16(-5.0, 100), 0);
        assert_eq!(to_u16(150.0, 100), 99);
        assert_eq!(to_u16(10.4, 100), 10);
    }
}
