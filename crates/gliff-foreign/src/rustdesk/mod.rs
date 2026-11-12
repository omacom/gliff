//! RustDesk by direct IP: the other machine must have "Enable direct IP
//! access" on, and a permanent or one-time password.
//!
//! RustDesk does not encrypt direct IP connections: its direct server skips
//! the key exchange and opens with the login challenge, so the screen and
//! the keys cross the network as they are. The password itself never
//! travels, only a hash salted by the other machine's challenge. The login
//! dialog says so before the password is typed. A server that does send
//! its identity with a key (as RustDesk does through its rendezvous
//! servers) gets a fresh secret sealed to that key, and both ends encrypt
//! with it from then on.

use std::time::Duration;

use crypto_box::aead::Aead as _;
use crypto_box::{PublicKey as BoxPublicKey, SalsaBox, SecretKey};
use gliff_proto::{Axis, ClientMsg};
use prost::Message as _;
use rand_core::{OsRng, RngCore as _};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{keys, Credentials, Error, Event, Purpose, Scroll, Target};

pub mod proto;
pub mod stream;
pub mod vpx;

use proto::message::Union;
use proto::Message;
use stream::{Cipher, Stream};

/// How long the login may take: the other machine's user may have to
/// accept the connection, or pick the screen to share on Wayland.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(120);
/// The client version gliff presents: one that sends keys by position.
const CLIENT_VERSION: &str = "1.4.0";
/// Frames a second asked for while only driving from the edge, where no
/// picture is shown.
const CONTROL_FPS: i32 = 1;

/// Whether `stream` opens like a RustDesk server: its identity, or the
/// login challenge of one that skips encryption.
pub(crate) async fn answers(stream: TcpStream) -> bool {
    let mut s = Stream::new(stream);
    matches!(
        s.recv().await,
        Ok(Message {
            union: Some(Union::SignedId(_) | Union::Hash(_))
        })
    )
}

fn msg(union: Union) -> Message {
    Message { union: Some(union) }
}

/// The key the other machine offers in its signed identity, past the
/// 64-byte signature.
fn offered_key(signed: &[u8]) -> Option<[u8; 32]> {
    let idpk = proto::IdPk::decode(signed.get(64..)?).ok()?;
    idpk.pk.as_slice().try_into().ok()
}

/// Seal a fresh secret to `their_key`: our public key, the sealed secret,
/// and the secret.
fn seal_secret(their_key: &[u8; 32]) -> ([u8; 32], Vec<u8>, [u8; 32]) {
    let ours = SecretKey::generate(&mut OsRng);
    let mut secret = [0u8; 32];
    OsRng.fill_bytes(&mut secret);
    // Under an all-zero nonce, as RustDesk seals it: the key is used once.
    let sealed = SalsaBox::new(&BoxPublicKey::from(*their_key), &ours)
        .encrypt(&Default::default(), secret.as_slice())
        .expect("sealing into a vector cannot fail");
    (*ours.public_key().as_bytes(), sealed, secret)
}

/// What the password proves: SHA-256 of (SHA-256 of password and salt) and
/// the challenge.
fn password_proof(password: &str, salt: &str, challenge: &str) -> Vec<u8> {
    let first = Sha256::new()
        .chain_update(password.as_bytes())
        .chain_update(salt.as_bytes())
        .finalize();
    Sha256::new()
        .chain_update(first)
        .chain_update(challenge.as_bytes())
        .finalize()
        .to_vec()
}

fn login_error(target: &Target, error: &str) -> Error {
    match error {
        "Wrong Password" | "Empty Password" => {
            Error::Auth(format!("{} refused the password", target.host))
        }
        "2FA Required" => Error::other(format!(
            "{} asks for a two-factor code, which gliff cannot give yet",
            target.host
        )),
        "No Password Access" => Error::other(format!(
            "{} takes no password logins: set a permanent password in its RustDesk",
            target.host
        )),
        other => Error::other(format!("{} refused the login: {other}", target.host)),
    }
}

/// The session this machine knows gliff's logins to `target` by. It is the
/// same for every connection, so a one-time password, which RustDesk
/// replaces as soon as it lets a peer in, still lets in the view's and the
/// control's next connections while the first is live: RustDesk takes them
/// on the password it remembers for the session.
fn session_id(target: &Target) -> u64 {
    let digest = Sha256::new()
        .chain_update(b"gliff session\0")
        .chain_update(my_name().as_bytes())
        .chain_update(b"\0")
        .chain_update(target.address().as_bytes())
        .finalize();
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
}

fn my_name() -> String {
    std::env::var("USER").unwrap_or_else(|_| "gliff".into())
}

pub(crate) async fn run(
    target: &Target,
    credentials: &Credentials,
    purpose: Purpose,
    input: &mut UnboundedReceiver<ClientMsg>,
    events: &mut dyn FnMut(Event),
) -> Result<(), Error> {
    let io = crate::reach(target).await?;
    let _ = io.set_nodelay(true);
    // RustDesk takes a direct login only under the address it was reached
    // at.
    let username = io
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| target.host.clone());
    let mut s = Stream::new(io);
    let login = login(&mut s, target, credentials, purpose, username);
    let Some(peer) = crate::set_up(target, LOGIN_TIMEOUT, input, login).await? else {
        return Ok(());
    };
    Session::new(s, peer, purpose)?.run(input, events).await
}

async fn login<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut Stream<S>,
    target: &Target,
    credentials: &Credentials,
    purpose: Purpose,
    username: String,
) -> Result<proto::PeerInfo, Error> {
    let first = s.recv().await?;
    let hash = match first.union {
        Some(Union::SignedId(id)) => {
            match offered_key(&id.id) {
                Some(theirs) => {
                    let (ours, sealed, secret) = seal_secret(&theirs);
                    s.send(&msg(Union::PublicKey(proto::PublicKey {
                        asymmetric_value: ours.to_vec(),
                        symmetric_value: sealed,
                        kx_version: 0,
                    })))
                    .await?;
                    s.cipher = Some(Cipher::new(&secret));
                }
                None => {
                    tracing::warn!(host = %target.host, "RustDesk offered no key; unencrypted");
                    s.send(&Message::default()).await?;
                }
            }
            loop {
                match s.recv().await?.union {
                    Some(Union::Hash(h)) => break h,
                    Some(Union::LoginResponse(r)) => {
                        if let Some(proto::login_response::Union::Error(e)) = r.union {
                            return Err(login_error(target, &e));
                        }
                    }
                    _ => {}
                }
            }
        }
        // RustDesk's direct IP access: no key exchange, no encryption.
        Some(Union::Hash(h)) => {
            tracing::debug!(host = %target.host, "RustDesk direct IP access: unencrypted");
            h
        }
        _ => {
            return Err(Error::other(format!(
                "{} does not speak RustDesk",
                target.host
            )))
        }
    };
    let password = if credentials.password.is_empty() {
        Vec::new()
    } else {
        password_proof(&credentials.password, &hash.salt, &hash.challenge)
    };
    let control = purpose == Purpose::Control;
    s.send(&msg(Union::LoginRequest(proto::LoginRequest {
        username,
        password,
        my_id: "gliff".into(),
        my_name: my_name(),
        option: Some(proto::OptionMessage {
            image_quality: if control { proto::IMAGE_QUALITY_LOW } else { 0 },
            disable_audio: proto::YES,
            disable_clipboard: proto::YES,
            supported_decoding: Some(proto::SupportedDecoding {
                ability_vp9: 1,
                ability_vp8: 1,
                prefer: proto::PREFER_VP9,
                ..Default::default()
            }),
            custom_fps: if control { CONTROL_FPS } else { 0 },
        }),
        video_ack_required: false,
        session_id: session_id(target),
        version: CLIENT_VERSION.into(),
        my_platform: "Linux".into(),
    })))
    .await?;
    loop {
        let m = s.recv().await?;
        tracing::debug!(message = ?m.union.as_ref().map(std::mem::discriminant), "while logging in");
        match m.union {
            Some(Union::LoginResponse(r)) => match r.union {
                Some(proto::login_response::Union::PeerInfo(p)) => return Ok(p),
                Some(proto::login_response::Union::Error(e)) => {
                    return Err(login_error(target, &e))
                }
                None => {}
            },
            Some(Union::Auth2Fa(_)) => return Err(login_error(target, "2FA Required")),
            Some(Union::TestDelay(t)) if !t.from_client => {
                s.send(&msg(Union::TestDelay(t))).await?;
            }
            Some(Union::Misc(proto::Misc {
                union: Some(proto::misc::Union::CloseReason(why)),
            })) => return Err(Error::other(format!("{} closed: {why}", target.host))),
            _ => {}
        }
    }
}

/// The other machine's platform, for key codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Platform {
    Windows,
    Mac,
    Linux,
    Other,
}

impl Platform {
    fn of(name: &str) -> Self {
        let name = name.to_ascii_lowercase();
        if name.contains("windows") {
            Platform::Windows
        } else if name.contains("mac") {
            Platform::Mac
        } else if name.contains("linux") {
            Platform::Linux
        } else {
            Platform::Other
        }
    }
}

/// The key event for evdev `code` on `platform`: its code there, sent by
/// position.
fn key_event(platform: Platform, code: u32, down: bool, locks: &[i32]) -> Option<proto::KeyEvent> {
    // Windows has no scan code for Pause; RustDesk sends it by name.
    if platform == Platform::Windows && code == 119 {
        return Some(proto::KeyEvent {
            down,
            press: false,
            union: Some(proto::key_event::Union::ControlKey(
                proto::CONTROL_KEY_PAUSE,
            )),
            modifiers: locks.to_vec(),
            mode: proto::KEYBOARD_MODE_LEGACY,
        });
    }
    let chr = match platform {
        Platform::Windows => keys::scancode(code).map(u32::from),
        Platform::Mac => keys::mac_keycode(code).map(u32::from),
        Platform::Linux => keys::x_keycode(code),
        Platform::Other => None,
    }?;
    Some(proto::KeyEvent {
        down,
        press: false,
        union: Some(proto::key_event::Union::Chr(chr)),
        modifiers: locks.to_vec(),
        mode: proto::KEYBOARD_MODE_MAP,
    })
}

fn mouse_button(button: u32) -> Option<i32> {
    match button {
        0x110 => Some(proto::MOUSE_BUTTON_LEFT),
        0x111 => Some(proto::MOUSE_BUTTON_RIGHT),
        0x112 => Some(proto::MOUSE_BUTTON_WHEEL),
        0x113 => Some(proto::MOUSE_BUTTON_BACK),
        0x114 => Some(proto::MOUSE_BUTTON_FORWARD),
        _ => None,
    }
}

/// The screen shown: where it starts on the other machine's desktop, and
/// its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Screen {
    index: i32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct Session<S> {
    s: Stream<S>,
    platform: Platform,
    screen: Screen,
    view: bool,
    decoder: Option<vpx::Decoder>,
    scroll: Scroll,
    /// Caps Lock and Num Lock as locked here, sent with each key so the
    /// other machine matches them.
    locks: Vec<i32>,
    pointer: (i32, i32),
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    fn new(s: Stream<S>, peer: proto::PeerInfo, purpose: Purpose) -> Result<Self, Error> {
        let index = peer.current_display;
        let d = usize::try_from(index)
            .ok()
            .and_then(|i| peer.displays.get(i))
            .or_else(|| peer.displays.first())
            .ok_or_else(|| Error::other("the other machine named no screen"))?;
        tracing::info!(
            platform = %peer.platform,
            version = %peer.version,
            host = %peer.hostname,
            "logged in to RustDesk"
        );
        Ok(Self {
            s,
            platform: Platform::of(&peer.platform),
            screen: Screen {
                index,
                x: d.x,
                y: d.y,
                width: d.width.max(1) as u32,
                height: d.height.max(1) as u32,
            },
            view: matches!(purpose, Purpose::View { .. }),
            decoder: None,
            scroll: Scroll::default(),
            locks: Vec::new(),
            pointer: (d.x, d.y),
        })
    }

    async fn run(
        mut self,
        input: &mut UnboundedReceiver<ClientMsg>,
        events: &mut dyn FnMut(Event),
    ) -> Result<(), Error> {
        events(Event::Connected {
            width: self.screen.width,
            height: self.screen.height,
            video: "RustDesk".into(),
        });
        loop {
            tokio::select! {
                m = self.s.recv() => self.on_message(m?, events).await?,
                m = input.recv() => match m {
                    Some(m) => self.on_input(m).await?,
                    None => return Ok(()),
                },
            }
        }
    }

    async fn on_message(&mut self, m: Message, events: &mut dyn FnMut(Event)) -> Result<(), Error> {
        match m.union {
            Some(Union::VideoFrame(f)) if self.view => self.on_video(f, events)?,
            Some(Union::TestDelay(t)) if !t.from_client => {
                self.s.send(&msg(Union::TestDelay(t))).await?;
            }
            Some(Union::Misc(proto::Misc { union: Some(u) })) => match u {
                proto::misc::Union::SwitchDisplay(d) => {
                    self.screen = Screen {
                        index: d.display,
                        x: d.x,
                        y: d.y,
                        width: d.width.max(1) as u32,
                        height: d.height.max(1) as u32,
                    };
                    events(Event::Resized {
                        width: self.screen.width,
                        height: self.screen.height,
                    });
                }
                proto::misc::Union::CloseReason(why) => {
                    return Err(Error::other(format!("the other machine closed: {why}")));
                }
            },
            _ => {}
        }
        Ok(())
    }

    fn on_video(
        &mut self,
        f: proto::VideoFrame,
        events: &mut dyn FnMut(Event),
    ) -> Result<(), Error> {
        use proto::video_frame::Union as V;
        if f.display != self.screen.index {
            return Ok(());
        }
        let (codec, frames) = match f.union {
            Some(V::Vp9s(f)) => (vpx::Codec::Vp9, f.frames),
            Some(V::Vp8s(f)) => (vpx::Codec::Vp8, f.frames),
            Some(V::H264s(_) | V::H265s(_) | V::Av1s(_)) => {
                return Err(Error::other(
                    "the other machine sends a codec gliff did not ask for",
                ));
            }
            None => return Ok(()),
        };
        if self.decoder.as_ref().map(|d| d.codec) != Some(codec) {
            self.decoder = Some(vpx::Decoder::new(codec)?);
        }
        let decoder = self.decoder.as_mut().expect("set above");
        let mut newest = None;
        for frame in frames {
            match decoder.decode(&frame.data) {
                Ok(Some(p)) => newest = Some(p),
                Ok(None) => {}
                // A lost frame spoils those after it until a key frame;
                // say so once rather than end the session.
                Err(e) => tracing::debug!(error = %e, "RustDesk frame"),
            }
        }
        if let Some(p) = newest {
            if (p.width, p.height) != (self.screen.width, self.screen.height) {
                self.screen.width = p.width;
                self.screen.height = p.height;
                events(Event::Resized {
                    width: p.width,
                    height: p.height,
                });
            }
            events(Event::Frame {
                width: p.width,
                height: p.height,
                bgrx: p.bgrx,
            });
        }
        Ok(())
    }

    async fn mouse(&mut self, mask: i32, x: i32, y: i32) -> Result<(), Error> {
        self.s
            .send(&msg(Union::MouseEvent(proto::MouseEvent {
                mask,
                x,
                y,
                modifiers: Vec::new(),
            })))
            .await?;
        Ok(())
    }

    async fn on_input(&mut self, m: ClientMsg) -> Result<(), Error> {
        match m {
            ClientMsg::PointerMotion { x, y } => {
                let max = |len: u32| f64::from(len.saturating_sub(1));
                let px = self.screen.x + x.round().clamp(0.0, max(self.screen.width)) as i32;
                let py = self.screen.y + y.round().clamp(0.0, max(self.screen.height)) as i32;
                self.pointer = (px, py);
                self.mouse(proto::MOUSE_TYPE_MOVE, px, py).await?;
            }
            ClientMsg::PointerButton { button, pressed } => {
                if let Some(b) = mouse_button(button) {
                    let kind = if pressed {
                        proto::MOUSE_TYPE_DOWN
                    } else {
                        proto::MOUSE_TYPE_UP
                    };
                    let (x, y) = self.pointer;
                    self.mouse(b << 3 | kind, x, y).await?;
                }
            }
            ClientMsg::PointerAxis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let n = self.scroll.notches(axis, value, discrete, stop);
                if n != 0 {
                    // RustDesk scrolls up, and left, with positive steps.
                    let (x, y) = match axis {
                        Axis::Vertical => (0, -n),
                        Axis::Horizontal => (-n, 0),
                    };
                    self.mouse(proto::MOUSE_TYPE_WHEEL, x, y).await?;
                }
            }
            ClientMsg::Key { keycode, pressed } => {
                match key_event(self.platform, keycode, pressed, &self.locks) {
                    Some(k) => self.s.send(&msg(Union::KeyEvent(k))).await?,
                    None => tracing::debug!(keycode, platform = ?self.platform, "no key code"),
                }
            }
            ClientMsg::KeyboardLocks { locked, .. } => {
                self.locks.clear();
                if locked.iter().any(|l| l == "Lock") {
                    self.locks.push(proto::CONTROL_KEY_CAPS_LOCK);
                }
                if locked.iter().any(|l| l == "Mod2") {
                    self.locks.push(proto::CONTROL_KEY_NUM_LOCK);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
