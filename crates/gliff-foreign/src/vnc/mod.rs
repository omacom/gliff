//! VNC (RFB 3.3 to 3.8): wayvnc, TigerVNC, x11vnc, macOS screen sharing
//! with its VNC password on, and the rest.
//!
//! Logins gliff can make: none, the VNC password (DES over the server's
//! challenge, as weak as the protocol has it), and VeNCrypt under TLS with
//! a user and password, the VNC password, or nothing. A server's
//! certificate is kept on first use, as RDP's. VeNCrypt's anonymous TLS and
//! its cleartext user and password are not taken.
//!
//! Keys go as the key's position (QEMU's extended key events) when the
//! server takes them, as gliff's own do; otherwise as the keysym a US
//! keyboard types.

use std::time::Duration;

use des::cipher::{generic_array::GenericArray, BlockEncrypt as _, KeyInit as _};
use flate2::Decompress;
use gliff_proto::{Axis, ClientMsg};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{keys, Credentials, Cursor, Error, Event, Purpose, Scroll, Target};

mod keysym;
mod picture;

use picture::Screen;

/// How long the server has to let gliff in, including a person at the
/// other machine accepting it, as some servers ask.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(60);

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC: u8 = 2;
const SECURITY_VENCRYPT: u8 = 19;

const VENCRYPT_NONE: u32 = 1;
const VENCRYPT_VNC: u32 = 2;
const VENCRYPT_X509_NONE: u32 = 260;
const VENCRYPT_X509_VNC: u32 = 261;
const VENCRYPT_X509_PLAIN: u32 = 262;

const ENCODING_RAW: i32 = 0;
const ENCODING_COPY_RECT: i32 = 1;
const ENCODING_ZRLE: i32 = 16;
const ENCODING_CURSOR: i32 = -239;
const ENCODING_DESKTOP_SIZE: i32 = -223;
const ENCODING_LAST_RECT: i32 = -224;
const ENCODING_QEMU_KEYS: i32 = -258;
const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Boxed = Box<dyn Io>;

/// Whether `stream` speaks VNC: the server names its protocol version
/// first.
pub(crate) async fn answers(mut stream: TcpStream) -> bool {
    let mut v = [0u8; 12];
    stream.read_exact(&mut v).await.is_ok() && version(&v).is_some()
}

/// `RFB xxx.yyy\n`: the major and minor version.
fn version(v: &[u8; 12]) -> Option<(u32, u32)> {
    if &v[..4] != b"RFB " || v[7] != b'.' || v[11] != b'\n' {
        return None;
    }
    let n = |b: &[u8]| std::str::from_utf8(b).ok()?.parse::<u32>().ok();
    Some((n(&v[4..7])?, n(&v[8..11])?))
}

/// The minor version of 3 to speak with a server of `major`.`minor`: 3.3,
/// 3.7 or 3.8; Apple's 3.889 and anything newer get 3.8.
fn speak(major: u32, minor: u32) -> u32 {
    match (major, minor) {
        (3, 7) => 7,
        (3, m) if m < 7 => 3,
        _ => 8,
    }
}

fn io_error(target: &Target) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => {
            Error::other(format!("{} closed the connection", target.host))
        }
        _ => Error::other(format!("{}: {e}", target.host)),
    }
}

async fn read_reason(s: &mut Boxed, target: &Target) -> String {
    let Ok(len) = s.read_u32().await else {
        return String::new();
    };
    let mut why = vec![0; len.min(4096) as usize];
    if s.read_exact(&mut why).await.is_err() {
        let _ = target;
        return String::new();
    }
    String::from_utf8_lossy(&why).trim().to_string()
}

/// The VNC password's answer to `challenge`: the challenge encrypted with
/// DES, keyed by the password's first eight bytes, each bit-reversed.
fn vnc_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, b) in key.iter_mut().zip(password.bytes()) {
        *k = b.reverse_bits();
    }
    let cipher = des::Des::new(GenericArray::from_slice(&key));
    let mut out = *challenge;
    for block in out.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    out
}

async fn vnc_auth(s: &mut Boxed, target: &Target, credentials: &Credentials) -> Result<(), Error> {
    if credentials.password.is_empty() {
        return Err(Error::Auth(format!(
            "{} asks for its VNC password",
            target.host
        )));
    }
    let mut challenge = [0u8; 16];
    s.read_exact(&mut challenge)
        .await
        .map_err(io_error(target))?;
    s.write_all(&vnc_response(&credentials.password, &challenge))
        .await
        .map_err(io_error(target))
}

/// The VeNCrypt kind to use of those `offered`, best first: under TLS with
/// a certificate, then plain VNC password or none. Anonymous TLS and
/// cleartext passwords are left out.
fn pick_vencrypt(offered: &[u32], tls_only: bool) -> Option<u32> {
    [
        VENCRYPT_X509_PLAIN,
        VENCRYPT_X509_VNC,
        VENCRYPT_X509_NONE,
        VENCRYPT_VNC,
        VENCRYPT_NONE,
    ]
    .into_iter()
    .filter(|k| !tls_only || *k >= VENCRYPT_X509_NONE)
    .find(|k| offered.contains(k))
}

/// VeNCrypt: the stream, under TLS when it went that way, and a
/// certificate seen for the first time.
async fn vencrypt(
    mut s: Boxed,
    target: &Target,
    credentials: &Credentials,
    known_certificate: Option<&str>,
) -> Result<(Boxed, Option<String>), Error> {
    let io = io_error(target);
    let mut v = [0u8; 2];
    s.read_exact(&mut v).await.map_err(&io)?;
    if v < [0, 2] {
        return Err(Error::other(format!(
            "{} speaks VeNCrypt {}.{}, older than gliff takes",
            target.host, v[0], v[1]
        )));
    }
    s.write_all(&[0, 2]).await.map_err(&io)?;
    if s.read_u8().await.map_err(&io)? != 0 {
        return Err(Error::other(format!(
            "{} refused VeNCrypt 0.2",
            target.host
        )));
    }
    let n = s.read_u8().await.map_err(&io)?;
    let mut offered = Vec::with_capacity(n.into());
    for _ in 0..n {
        offered.push(s.read_u32().await.map_err(&io)?);
    }
    tracing::debug!(host = %target.host, ?offered, "VeNCrypt kinds");
    // With a saved certificate, only TLS that shows it will do.
    let Some(kind) = pick_vencrypt(&offered, known_certificate.is_some()) else {
        return Err(Error::other(format!(
            "{} offers only VeNCrypt logins gliff does not make ({offered:?}): anonymous \
             TLS or a cleartext password. Give its VNC server a certificate.",
            target.host
        )));
    };
    s.write_all(&kind.to_be_bytes()).await.map_err(&io)?;
    let (mut s, new_certificate): (Boxed, _) = if kind >= VENCRYPT_X509_NONE {
        // The server says whether it can start TLS; a kind without TLS
        // goes straight on to its login.
        if s.read_u8().await.map_err(&io)? != 1 {
            return Err(Error::other(format!("{} cannot start TLS", target.host)));
        }
        let (tls, _, new) = crate::tls::upgrade(s, target, known_certificate).await?;
        (Box::new(tls), new)
    } else {
        (s, None)
    };
    match kind {
        VENCRYPT_X509_PLAIN => {
            if credentials.user.is_empty() {
                return Err(Error::Auth(format!(
                    "{} asks for a user name as well as the password",
                    target.host
                )));
            }
            let mut m = Vec::new();
            m.extend_from_slice(&(credentials.user.len() as u32).to_be_bytes());
            m.extend_from_slice(&(credentials.password.len() as u32).to_be_bytes());
            m.extend_from_slice(credentials.user.as_bytes());
            m.extend_from_slice(credentials.password.as_bytes());
            s.write_all(&m).await.map_err(&io)?;
        }
        VENCRYPT_X509_VNC | VENCRYPT_VNC => vnc_auth(&mut s, target, credentials).await?,
        _ => {}
    }
    Ok((s, new_certificate))
}

/// What the server says of itself once it lets gliff in.
#[derive(Debug)]
struct ServerInit {
    width: u32,
    height: u32,
    name: String,
}

/// Log in: the stream ready for the session, the server's screen, and the
/// fingerprint of a certificate seen for the first time.
async fn handshake(
    mut s: Boxed,
    target: &Target,
    credentials: &Credentials,
    known_certificate: Option<&str>,
) -> Result<(Boxed, ServerInit, Option<String>), Error> {
    let io = io_error(target);
    let mut v = [0u8; 12];
    s.read_exact(&mut v).await.map_err(&io)?;
    let (major, minor) =
        version(&v).ok_or_else(|| Error::other(format!("{} does not speak VNC", target.host)))?;
    let minor = speak(major, minor);
    s.write_all(format!("RFB 003.{minor:03}\n").as_bytes())
        .await
        .map_err(&io)?;
    let offered = if minor == 3 {
        match s.read_u32().await.map_err(&io)? {
            0 => {
                let why = read_reason(&mut s, target).await;
                return Err(Error::other(format!("{} refused: {why}", target.host)));
            }
            t => vec![u8::try_from(t).unwrap_or(0)],
        }
    } else {
        let n = s.read_u8().await.map_err(&io)?;
        if n == 0 {
            let why = read_reason(&mut s, target).await;
            return Err(Error::other(format!("{} refused: {why}", target.host)));
        }
        let mut offered = vec![0; n.into()];
        s.read_exact(&mut offered).await.map_err(&io)?;
        offered
    };
    // A machine whose certificate is saved logs in under that certificate
    // or not at all: one that offers a login without it may be another
    // machine posing as it. Otherwise TLS comes first where offered.
    let allowed: &[u8] = if known_certificate.is_some() {
        &[SECURITY_VENCRYPT]
    } else {
        &[SECURITY_VENCRYPT, SECURITY_NONE, SECURITY_VNC]
    };
    let chosen = allowed
        .iter()
        .copied()
        .find(|t| offered.contains(t))
        .ok_or_else(|| match known_certificate {
            Some(_) => Error::other(format!(
                "{} showed a certificate before, but now offers only logins without \
                 it ({offered:?}); someone may be posing as it. If its VNC server was \
                 set up again, forget it in gliff and connect again.",
                target.host
            )),
            None => Error::other(format!(
                "{} offers only VNC logins gliff does not make ({offered:?}); a VNC \
                 password or VeNCrypt with a certificate works",
                target.host
            )),
        })?;
    tracing::debug!(host = %target.host, ?offered, chosen, minor, "VNC security");
    if minor != 3 {
        s.write_all(&[chosen]).await.map_err(&io)?;
    }
    let mut new_certificate = None;
    let result_follows = match chosen {
        SECURITY_NONE => minor == 8,
        SECURITY_VNC => {
            vnc_auth(&mut s, target, credentials).await?;
            true
        }
        _ => {
            let (upgraded, new) = vencrypt(s, target, credentials, known_certificate).await?;
            s = upgraded;
            new_certificate = new;
            true
        }
    };
    if result_follows && s.read_u32().await.map_err(&io)? != 0 {
        let why = if minor == 8 {
            read_reason(&mut s, target).await
        } else {
            String::new()
        };
        let why = if why.is_empty() {
            String::new()
        } else {
            format!(": {why}")
        };
        return Err(Error::Auth(format!(
            "{} refused the login{why}",
            target.host
        )));
    }
    // Shared: others looking at the same screen stay on.
    s.write_all(&[1]).await.map_err(&io)?;
    let width = u32::from(s.read_u16().await.map_err(&io)?);
    let height = u32::from(s.read_u16().await.map_err(&io)?);
    let mut format = [0u8; 16];
    s.read_exact(&mut format).await.map_err(&io)?;
    let len = s.read_u32().await.map_err(&io)?;
    let mut name = vec![0; len.min(4096) as usize];
    s.read_exact(&mut name).await.map_err(&io)?;
    if len > 4096 {
        tokio::io::copy(
            &mut (&mut s).take(u64::from(len - 4096)),
            &mut tokio::io::sink(),
        )
        .await
        .map_err(&io)?;
    }
    if width == 0 || height == 0 || width > picture::MAX_SIDE || height > picture::MAX_SIDE {
        return Err(Error::other(format!(
            "{} names a {width}x{height} screen",
            target.host
        )));
    }
    Ok((
        s,
        ServerInit {
            width,
            height,
            name: String::from_utf8_lossy(&name).into_owned(),
        },
        new_certificate,
    ))
}

/// SetPixelFormat: 32-bit little-endian true colour, red at bit 16, green
/// at 8, blue at 0, so pixels arrive as BGRX bytes.
fn set_pixel_format() -> [u8; 20] {
    [
        0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0,
    ]
}

fn set_encodings(view: bool) -> Vec<u8> {
    let encodings: &[i32] = if view {
        &[
            ENCODING_ZRLE,
            ENCODING_COPY_RECT,
            ENCODING_RAW,
            ENCODING_CURSOR,
            ENCODING_DESKTOP_SIZE,
            ENCODING_EXTENDED_DESKTOP_SIZE,
            ENCODING_LAST_RECT,
            ENCODING_QEMU_KEYS,
        ]
    } else {
        &[
            ENCODING_RAW,
            ENCODING_DESKTOP_SIZE,
            ENCODING_LAST_RECT,
            ENCODING_QEMU_KEYS,
        ]
    };
    let mut m = vec![2, 0];
    m.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
    for e in encodings {
        m.extend_from_slice(&e.to_be_bytes());
    }
    m
}

/// FramebufferUpdateRequest for the screen's top-left `w`×`h`.
fn update_request(incremental: bool, w: u32, h: u32) -> [u8; 10] {
    let [w0, w1] = (w.min(0xffff) as u16).to_be_bytes();
    let [h0, h1] = (h.min(0xffff) as u16).to_be_bytes();
    [3, u8::from(incremental), 0, 0, 0, 0, w0, w1, h0, h1]
}

/// What the reading side tells the session.
#[derive(Debug)]
enum Update {
    Resized(u32, u32),
    Cursor(Cursor),
    CursorHidden,
    /// The server takes key positions.
    QemuKeys,
    /// An update ended; the whole screen when it is shown.
    Done(Option<(u32, u32, Vec<u8>)>),
}

/// Cursor pseudo-rectangle: BGRX pixels and a bit mask, a row padded to a
/// byte, to gliff's BGRA.
fn cursor_image(w: u32, h: u32, pixels: &[u8], mask: &[u8]) -> Vec<u8> {
    let row = w.div_ceil(8) as usize;
    let mut argb = Vec::with_capacity(pixels.len());
    for (i, p) in pixels.as_chunks::<4>().0.iter().enumerate() {
        let (x, y) = (i % w as usize, i / w as usize);
        let on = mask
            .get(y * row + x / 8)
            .is_some_and(|b| b & (0x80 >> (x % 8)) != 0);
        argb.extend_from_slice(&[p[0], p[1], p[2], if on { 255 } else { 0 }]);
    }
    let _ = h;
    argb
}

async fn skip<R: AsyncRead + Unpin>(r: &mut R, n: u64) -> std::io::Result<()> {
    tokio::io::copy(&mut r.take(n), &mut tokio::io::sink()).await?;
    Ok(())
}

/// Read the server's messages until it closes, drawing the screen.
async fn read_loop<R: AsyncRead + Unpin>(
    mut r: R,
    target: &Target,
    mut screen: Screen,
    view: bool,
    tx: UnboundedSender<Update>,
) -> Result<(), Error> {
    let io = io_error(target);
    let bad = |what: String| Error::other(format!("{}: {what}", target.host));
    let mut zlib = Decompress::new(true);
    let mut buf = Vec::new();
    loop {
        match r.read_u8().await.map_err(&io)? {
            // FramebufferUpdate
            0 => {
                r.read_u8().await.map_err(&io)?;
                let rects = r.read_u16().await.map_err(&io)?;
                for _ in 0..rects {
                    let x = u32::from(r.read_u16().await.map_err(&io)?);
                    let y = u32::from(r.read_u16().await.map_err(&io)?);
                    let w = u32::from(r.read_u16().await.map_err(&io)?);
                    let h = u32::from(r.read_u16().await.map_err(&io)?);
                    let encoding = r.read_i32().await.map_err(&io)?;
                    let on_screen = || {
                        screen
                            .holds(x, y, w, h)
                            .then_some(())
                            .ok_or_else(|| bad(format!("a {w}x{h} rectangle off the screen")))
                    };
                    match encoding {
                        ENCODING_RAW => {
                            on_screen()?;
                            buf.resize(w as usize * h as usize * 4, 0);
                            r.read_exact(&mut buf).await.map_err(&io)?;
                            screen.put(x, y, w, h, &buf);
                        }
                        ENCODING_COPY_RECT => {
                            let sx = u32::from(r.read_u16().await.map_err(&io)?);
                            let sy = u32::from(r.read_u16().await.map_err(&io)?);
                            on_screen()?;
                            if !screen.holds(sx, sy, w, h) {
                                return Err(bad("a copy from off the screen".into()));
                            }
                            screen.copy(sx, sy, x, y, w, h);
                        }
                        ENCODING_ZRLE => {
                            on_screen()?;
                            let len = r.read_u32().await.map_err(&io)? as usize;
                            let limit = w as usize * h as usize * 4 + 4096;
                            if len > limit {
                                return Err(bad("a ZRLE rectangle too long".into()));
                            }
                            buf.resize(len, 0);
                            r.read_exact(&mut buf).await.map_err(&io)?;
                            let data = picture::inflate(&mut zlib, &buf, limit).map_err(bad)?;
                            picture::zrle(&mut screen, x, y, w, h, &data).map_err(bad)?;
                        }
                        ENCODING_CURSOR => {
                            if w > 256 || h > 256 {
                                return Err(bad(format!("a {w}x{h} cursor")));
                            }
                            let mut pixels = vec![0; w as usize * h as usize * 4];
                            r.read_exact(&mut pixels).await.map_err(&io)?;
                            let mut mask = vec![0; w.div_ceil(8) as usize * h as usize];
                            r.read_exact(&mut mask).await.map_err(&io)?;
                            let _ = tx.send(if w == 0 || h == 0 {
                                Update::CursorHidden
                            } else {
                                Update::Cursor(Cursor {
                                    width: w,
                                    height: h,
                                    hot_x: x.min(w - 1),
                                    hot_y: y.min(h - 1),
                                    argb: cursor_image(w, h, &pixels, &mask),
                                })
                            });
                        }
                        ENCODING_DESKTOP_SIZE | ENCODING_EXTENDED_DESKTOP_SIZE => {
                            let ok = if encoding == ENCODING_EXTENDED_DESKTOP_SIZE {
                                let screens = r.read_u8().await.map_err(&io)?;
                                skip(&mut r, 3 + u64::from(screens) * 16)
                                    .await
                                    .map_err(&io)?;
                                // y is the status of a change gliff asked
                                // for; it never asks.
                                y == 0
                            } else {
                                true
                            };
                            let fits = (1..=picture::MAX_SIDE).contains(&w)
                                && (1..=picture::MAX_SIDE).contains(&h);
                            if ok && fits && (w, h) != (screen.width, screen.height) {
                                screen.resize(w, h);
                                let _ = tx.send(Update::Resized(w, h));
                            }
                        }
                        ENCODING_LAST_RECT => break,
                        ENCODING_QEMU_KEYS => {
                            let _ = tx.send(Update::QemuKeys);
                        }
                        other => return Err(bad(format!("encoding {other}, not asked for"))),
                    }
                }
                let frame = view.then(|| (screen.width, screen.height, screen.bgrx.clone()));
                let _ = tx.send(Update::Done(frame));
            }
            // SetColourMapEntries: not used with true colour.
            1 => {
                r.read_u8().await.map_err(&io)?;
                r.read_u16().await.map_err(&io)?;
                let n = r.read_u16().await.map_err(&io)?;
                skip(&mut r, u64::from(n) * 6).await.map_err(&io)?;
            }
            // Bell
            2 => {}
            // ServerCutText: the clipboard, not shared.
            3 => {
                skip(&mut r, 3).await.map_err(&io)?;
                let len = r.read_i32().await.map_err(&io)?;
                skip(&mut r, u64::from(len.unsigned_abs()))
                    .await
                    .map_err(&io)?;
            }
            other => return Err(bad(format!("message {other}, not one gliff knows"))),
        }
    }
}

/// gliff's input, as RFB messages.
#[derive(Default)]
struct Input {
    qemu_keys: bool,
    buttons: u8,
    pointer: (u16, u16),
    shift: [bool; 2],
    caps: bool,
    scroll: Scroll,
    /// The keysym each held key went down as, for its release: Shift may
    /// have changed in between.
    held: std::collections::HashMap<u32, u32>,
}

fn pointer_event(buttons: u8, (x, y): (u16, u16)) -> Vec<u8> {
    let [x0, x1] = x.to_be_bytes();
    let [y0, y1] = y.to_be_bytes();
    vec![5, buttons, x0, x1, y0, y1]
}

/// The RFB button mask bit for evdev `button`.
fn button_bit(button: u32) -> Option<u8> {
    match button {
        0x110 => Some(1),
        0x112 => Some(2),
        0x111 => Some(4),
        _ => None,
    }
}

/// QEMU's key code for evdev `code`: the PC scan code, with an extended
/// key's 0xE0 prefix folded into the high bit.
fn qemu_keycode(code: u32) -> Option<u32> {
    let sc = keys::scancode(code)?;
    Some(if sc & 0xff00 == 0xe000 {
        0x80 | u32::from(sc & 0x7f)
    } else {
        u32::from(sc)
    })
}

impl Input {
    fn translate(&mut self, m: ClientMsg, (width, height): (u32, u32)) -> Vec<Vec<u8>> {
        match m {
            ClientMsg::PointerMotion { x, y } => {
                let at = |v: f64, len: u32| {
                    v.round().clamp(0.0, f64::from(len.saturating_sub(1))) as u16
                };
                self.pointer = (at(x, width), at(y, height));
                vec![pointer_event(self.buttons, self.pointer)]
            }
            ClientMsg::PointerButton { button, pressed } => {
                let Some(bit) = button_bit(button) else {
                    return Vec::new();
                };
                if pressed {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                vec![pointer_event(self.buttons, self.pointer)]
            }
            ClientMsg::PointerAxis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let n = self.scroll.notches(axis, value, discrete, stop);
                // Buttons 4 and 5 scroll up and down, 6 and 7 left and
                // right.
                let bit = match (axis, n > 0) {
                    (Axis::Vertical, false) => 8,
                    (Axis::Vertical, true) => 16,
                    (Axis::Horizontal, false) => 32,
                    (Axis::Horizontal, true) => 64,
                };
                (0..n.unsigned_abs())
                    .flat_map(|_| {
                        [
                            pointer_event(self.buttons | bit, self.pointer),
                            pointer_event(self.buttons, self.pointer),
                        ]
                    })
                    .collect()
            }
            ClientMsg::Key { keycode, pressed } => {
                match keycode {
                    42 => self.shift[0] = pressed,
                    54 => self.shift[1] = pressed,
                    _ => {}
                }
                let sym = if pressed {
                    let sym = keysym::keysym(keycode, self.shift.contains(&true), self.caps);
                    if let Some(sym) = sym {
                        self.held.insert(keycode, sym);
                    }
                    sym
                } else {
                    self.held
                        .remove(&keycode)
                        .or_else(|| keysym::keysym(keycode, self.shift.contains(&true), self.caps))
                };
                let qemu = self.qemu_keys.then(|| qemu_keycode(keycode)).flatten();
                match (qemu, sym) {
                    (Some(code), sym) => {
                        let mut m = vec![255, 0, 0, u8::from(pressed)];
                        m.extend_from_slice(&sym.unwrap_or(0).to_be_bytes());
                        m.extend_from_slice(&code.to_be_bytes());
                        vec![m]
                    }
                    (None, Some(sym)) => {
                        let mut m = vec![4, u8::from(pressed), 0, 0];
                        m.extend_from_slice(&sym.to_be_bytes());
                        vec![m]
                    }
                    (None, None) => {
                        tracing::debug!(keycode, "no VNC key for it");
                        Vec::new()
                    }
                }
            }
            ClientMsg::KeyboardLocks { locked, .. } => {
                self.caps = locked.iter().any(|l| l == "Lock");
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

pub(crate) async fn run(
    target: &Target,
    credentials: &Credentials,
    purpose: Purpose,
    known_certificate: Option<&str>,
    input: &mut UnboundedReceiver<ClientMsg>,
    events: &mut dyn FnMut(Event),
) -> Result<(), Error> {
    let stream = crate::reach(target).await?;
    let _ = stream.set_nodelay(true);
    let login = handshake(Box::new(stream), target, credentials, known_certificate);
    let Some((s, init, new_certificate)) =
        crate::set_up(target, LOGIN_TIMEOUT, input, login).await?
    else {
        return Ok(());
    };
    if let Some(fp) = new_certificate {
        events(Event::NewCertificate(fp));
    }
    tracing::info!(
        host = %target.host,
        name = %init.name,
        width = init.width,
        height = init.height,
        "logged in to VNC"
    );
    session(s, target, init, purpose, input, events).await
}

async fn session(
    s: Boxed,
    target: &Target,
    init: ServerInit,
    purpose: Purpose,
    input: &mut UnboundedReceiver<ClientMsg>,
    events: &mut dyn FnMut(Event),
) -> Result<(), Error> {
    let io = io_error(target);
    let view = matches!(purpose, Purpose::View { .. });
    let (r, mut w) = tokio::io::split(s);
    let mut size = (init.width, init.height);
    w.write_all(&set_pixel_format()).await.map_err(&io)?;
    w.write_all(&set_encodings(view)).await.map_err(&io)?;
    // Driving from the edge needs no picture: one pixel, once, tells
    // whether the server takes key positions.
    let (rw, rh) = if view { size } else { (1, 1) };
    w.write_all(&update_request(false, rw, rh))
        .await
        .map_err(&io)?;
    events(Event::Connected {
        width: size.0,
        height: size.1,
        video: "VNC".into(),
    });
    let (tx, mut rx) = unbounded_channel();
    let reader = read_loop(
        BufReader::with_capacity(1 << 16, r),
        target,
        Screen::new(size.0, size.1, view),
        view,
        tx,
    );
    tokio::pin!(reader);
    let mut keys = Input::default();
    let mut resized = false;
    // Input waits for the first update, which tells whether the server
    // takes key positions, so every key goes the same way.
    let mut waiting: Option<Vec<ClientMsg>> = Some(Vec::new());
    loop {
        tokio::select! {
            ended = &mut reader => {
                ended?;
                return Err(Error::other(format!("{} ended the session", target.host)));
            }
            Some(u) = rx.recv() => match u {
                Update::Resized(width, height) => {
                    size = (width, height);
                    resized = true;
                    events(Event::Resized { width, height });
                }
                Update::Cursor(c) => events(Event::Cursor(c)),
                Update::CursorHidden => events(Event::CursorHidden),
                Update::QemuKeys => {
                    tracing::debug!(host = %target.host, "VNC server takes key positions");
                    keys.qemu_keys = true;
                }
                Update::Done(frame) => {
                    for m in waiting.take().unwrap_or_default() {
                        for bytes in keys.translate(m, size) {
                            w.write_all(&bytes).await.map_err(&io)?;
                        }
                    }
                    if let Some((width, height, bgrx)) = frame {
                        events(Event::Frame { width, height, bgrx });
                    }
                    if view {
                        w.write_all(&update_request(!resized, size.0, size.1))
                            .await
                            .map_err(&io)?;
                        resized = false;
                    }
                }
            },
            m = input.recv() => match m {
                Some(m) if waiting.is_some() => {
                    if let Some(w) = waiting.as_mut() {
                        w.push(m);
                    }
                }
                Some(m) => {
                    for bytes in keys.translate(m, size) {
                        w.write_all(&bytes).await.map_err(&io)?;
                    }
                }
                None => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests;
