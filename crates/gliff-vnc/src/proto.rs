//! RFB messages and the handshake (RFC 6143), for protocol versions 3.3,
//! 3.7 and 3.8, which is what old clients still speak.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::auth;
use crate::encode::Rect;
use crate::pixel::PixelFormat;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Protocol(String),
    #[error("wrong password")]
    WrongPassword,
}

pub type Result<T> = std::result::Result<T, Error>;

/// The protocol version agreed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V3_3,
    V3_7,
    V3_8,
}

impl Version {
    /// The version to speak with a client that sent `line`: the highest the
    /// server knows that is not above the client's.
    fn from_client(line: &[u8; 12]) -> Result<Self> {
        // Read as bytes: a client's line is not trusted to be ASCII, and
        // slicing text could split a character.
        let bad = || {
            Error::Protocol(format!(
                "not a VNC client: {:?}",
                String::from_utf8_lossy(line).trim_end()
            ))
        };
        if &line[..4] != b"RFB " || line[7] != b'.' || line[11] != b'\n' {
            return Err(bad());
        }
        let number = |digits: &[u8]| -> Result<u32> {
            if !digits.iter().all(u8::is_ascii_digit) {
                return Err(bad());
            }
            std::str::from_utf8(digits)
                .ok()
                .and_then(|d| d.parse().ok())
                .ok_or_else(bad)
        };
        let major = number(&line[4..7])?;
        let minor = number(&line[8..11])?;
        Ok(match (major, minor) {
            (3, 0..=6) => Self::V3_3,
            (3, 7) => Self::V3_7,
            (3, _) | (4.., _) => Self::V3_8,
            _ => return Err(bad()),
        })
    }
}

/// How the client proves it may connect.
#[derive(Clone)]
pub enum Security {
    /// No authentication (RFB type 1).
    None,
    /// VNC Authentication (RFB type 2) with this password.
    Password(Vec<u8>),
}

impl Security {
    fn number(&self) -> u8 {
        match self {
            Self::None => 1,
            Self::Password(_) => 2,
        }
    }
}

/// What the client said during the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    pub version: Version,
    /// The client lets other clients stay connected.
    pub shared: bool,
}

/// The handshake's parameters.
pub struct Login<'a> {
    pub security: &'a Security,
    /// Fresh random bytes for VNC Authentication.
    pub challenge: [u8; 16],
    /// How long a wrong password waits before the client hears of it, so
    /// that passwords cannot be tried quickly.
    pub wrong_password_delay: std::time::Duration,
    pub size: (u16, u16),
    pub name: &'a str,
}

/// Run the handshake up to and including ServerInit.
pub async fn handshake<S>(stream: &mut S, login: Login<'_>) -> Result<Hello>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Login {
        security,
        challenge,
        wrong_password_delay,
        size: (width, height),
        name,
    } = login;
    stream.write_all(b"RFB 003.008\n").await?;
    let mut line = [0u8; 12];
    stream.read_exact(&mut line).await?;
    let version = Version::from_client(&line)?;

    let kind = security.number();
    if version == Version::V3_3 {
        stream.write_all(&u32::from(kind).to_be_bytes()).await?;
    } else {
        stream.write_all(&[1, kind]).await?;
        let chosen = stream.read_u8().await?;
        if chosen != kind {
            fail(stream, version, "unsupported security type").await?;
            return Err(Error::Protocol(format!(
                "the client chose security type {chosen}"
            )));
        }
    }
    match security {
        Security::None => {
            // 3.8 confirms even when there was nothing to check.
            if version == Version::V3_8 {
                stream.write_all(&0u32.to_be_bytes()).await?;
            }
        }
        Security::Password(password) => {
            stream.write_all(&challenge).await?;
            let mut answer = [0u8; 16];
            stream.read_exact(&mut answer).await?;
            if !auth::verify(password, &challenge, &answer) {
                tokio::time::sleep(wrong_password_delay).await;
                fail(stream, version, "wrong password").await?;
                return Err(Error::WrongPassword);
            }
            stream.write_all(&0u32.to_be_bytes()).await?;
        }
    }

    let shared = stream.read_u8().await? != 0;
    let mut init = Vec::with_capacity(24 + name.len());
    init.extend_from_slice(&width.to_be_bytes());
    init.extend_from_slice(&height.to_be_bytes());
    init.extend_from_slice(&PixelFormat::BGRX.to_bytes());
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name.as_bytes());
    stream.write_all(&init).await?;
    stream.flush().await?;
    Ok(Hello { version, shared })
}

/// A failed SecurityResult, with the reason where the version has one.
async fn fail<W: AsyncWrite + Unpin>(w: &mut W, version: Version, reason: &str) -> Result<()> {
    w.write_all(&1u32.to_be_bytes()).await?;
    if version == Version::V3_8 {
        w.write_all(&(reason.len() as u32).to_be_bytes()).await?;
        w.write_all(reason.as_bytes()).await?;
    }
    w.flush().await?;
    Ok(())
}

/// A message from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    SetPixelFormat(PixelFormat),
    SetEncodings(Vec<i32>),
    UpdateRequest { incremental: bool, area: Rect },
    Key { down: bool, keysym: u32 },
    Pointer { buttons: u8, x: u16, y: u16 },
    CutText(Vec<u8>),
}

/// Longest clipboard text accepted from a client.
const MAX_CUT_TEXT: u32 = 1 << 20;

pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> Result<ClientMsg> {
    let kind = r.read_u8().await?;
    Ok(match kind {
        0 => {
            let mut b = [0u8; 19];
            r.read_exact(&mut b).await?;
            let format: [u8; 16] = b[3..].try_into().expect("16 bytes");
            ClientMsg::SetPixelFormat(PixelFormat::from_bytes(&format))
        }
        2 => {
            let _pad = r.read_u8().await?;
            let n = r.read_u16().await?;
            let mut list = Vec::with_capacity(usize::from(n));
            for _ in 0..n {
                list.push(r.read_i32().await?);
            }
            ClientMsg::SetEncodings(list)
        }
        3 => {
            let incremental = r.read_u8().await? != 0;
            let area = Rect {
                x: r.read_u16().await?,
                y: r.read_u16().await?,
                width: r.read_u16().await?,
                height: r.read_u16().await?,
            };
            ClientMsg::UpdateRequest { incremental, area }
        }
        4 => {
            let down = r.read_u8().await? != 0;
            let _pad = r.read_u16().await?;
            ClientMsg::Key {
                down,
                keysym: r.read_u32().await?,
            }
        }
        5 => ClientMsg::Pointer {
            buttons: r.read_u8().await?,
            x: r.read_u16().await?,
            y: r.read_u16().await?,
        },
        6 => {
            let mut pad = [0u8; 3];
            r.read_exact(&mut pad).await?;
            let len = r.read_u32().await?;
            if len > MAX_CUT_TEXT {
                return Err(Error::Protocol(format!("{len} bytes of clipboard text")));
            }
            let mut text = vec![0u8; len as usize];
            r.read_exact(&mut text).await?;
            ClientMsg::CutText(text)
        }
        other => {
            // Without its length the stream cannot be followed past it.
            return Err(Error::Protocol(format!("unknown client message {other}")));
        }
    })
}

/// The start of a FramebufferUpdate with `rects` rectangles.
pub fn update_header(rects: u16, out: &mut Vec<u8>) {
    out.push(0);
    out.push(0);
    out.extend_from_slice(&rects.to_be_bytes());
}

/// SetColourMapEntries from index 0.
pub fn colour_map(colours: &[[u16; 3]], out: &mut Vec<u8>) {
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&(colours.len() as u16).to_be_bytes());
    for c in colours {
        for v in c {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

/// A DesktopSize pseudo-rectangle: the framebuffer is now `width`x`height`.
pub fn desktop_size(width: u16, height: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&crate::encode::number::DESKTOP_SIZE.to_be_bytes());
}

#[cfg(test)]
mod tests;
