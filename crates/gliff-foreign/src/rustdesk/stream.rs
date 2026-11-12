//! RustDesk's framing, and the encryption both ends agree on first.
//!
//! A frame is a little-endian length shifted left two bits, its low two
//! bits telling how many bytes the length takes (one to four), then the
//! bytes. Once keyed, each frame longer than one byte is sealed with NaCl's
//! secretbox, under a nonce that counts the frames each way from one.

use bytes::{Buf as _, BytesMut};
use crypto_secretbox::aead::{Aead as _, KeyInit as _};
use crypto_secretbox::{Key, Nonce, XSalsa20Poly1305};
use prost::Message as _;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use super::proto::Message;

/// The largest frame taken, against a peer that claims a huge one.
pub const MAX_FRAME: usize = 64 << 20;

/// Seals and opens frames once a key is agreed.
pub struct Cipher {
    cipher: XSalsa20Poly1305,
    sent: u64,
    received: u64,
}

impl Cipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: XSalsa20Poly1305::new(Key::from_slice(key)),
            sent: 0,
            received: 0,
        }
    }

    fn nonce(n: u64) -> Nonce {
        let mut nonce = Nonce::default();
        nonce[..8].copy_from_slice(&n.to_le_bytes());
        nonce
    }

    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        self.sent += 1;
        self.cipher
            .encrypt(&Self::nonce(self.sent), plain)
            .expect("sealing into a vector cannot fail")
    }

    pub fn open(&mut self, sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
        // The other end sends a lone byte, or nothing, unsealed.
        if sealed.len() <= 1 {
            return Ok(sealed.to_vec());
        }
        self.received += 1;
        self.cipher
            .decrypt(&Self::nonce(self.received), sealed)
            .map_err(|_| anyhow::anyhow!("a message from the other machine failed to decrypt"))
    }
}

/// The header for a frame of `len` bytes.
pub fn header(len: usize) -> anyhow::Result<Vec<u8>> {
    let h = (len as u64) << 2;
    Ok(if len <= 0x3F {
        vec![h as u8]
    } else if len <= 0x3FFF {
        (h as u16 | 1).to_le_bytes().to_vec()
    } else if len <= 0x3F_FFFF {
        (h as u32 | 2).to_le_bytes()[..3].to_vec()
    } else if len <= 0x3FFF_FFFF {
        (h as u32 | 3).to_le_bytes().to_vec()
    } else {
        anyhow::bail!("a {len}-byte frame is too large to send");
    })
}

/// A whole frame at the front of `buf`, taken off it.
pub fn take_frame(buf: &mut BytesMut) -> anyhow::Result<Option<BytesMut>> {
    let Some(&first) = buf.first() else {
        return Ok(None);
    };
    let head = usize::from(first & 3) + 1;
    if buf.len() < head {
        return Ok(None);
    }
    let mut n = 0usize;
    for (i, b) in buf[..head].iter().enumerate() {
        n |= usize::from(*b) << (8 * i);
    }
    let len = n >> 2;
    anyhow::ensure!(
        len <= MAX_FRAME,
        "the other machine sent a {len}-byte frame"
    );
    if buf.len() < head + len {
        buf.reserve(head + len - buf.len());
        return Ok(None);
    }
    buf.advance(head);
    Ok(Some(buf.split_to(len)))
}

/// A connection carrying [`Message`]s.
pub struct Stream<S> {
    io: S,
    buf: BytesMut,
    pub cipher: Option<Cipher>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S> {
    pub fn new(io: S) -> Self {
        Self {
            io,
            buf: BytesMut::with_capacity(64 << 10),
            cipher: None,
        }
    }

    /// The next message. Cancel safe: what was read stays in the buffer.
    pub async fn recv(&mut self) -> anyhow::Result<Message> {
        loop {
            if let Some(frame) = take_frame(&mut self.buf)? {
                let plain = match self.cipher.as_mut() {
                    Some(c) => c.open(&frame)?,
                    None => frame.to_vec(),
                };
                return Ok(Message::decode(plain.as_slice())?);
            }
            if self.io.read_buf(&mut self.buf).await? == 0 {
                anyhow::bail!("the other machine closed the connection");
            }
        }
    }

    pub async fn send(&mut self, msg: &Message) -> anyhow::Result<()> {
        let plain = msg.encode_to_vec();
        let body = match self.cipher.as_mut() {
            Some(c) => c.seal(&plain),
            None => plain,
        };
        let mut out = header(body.len())?;
        out.extend_from_slice(&body);
        self.io.write_all(&out).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_grow_with_the_frame() {
        assert_eq!(header(0).unwrap(), [0]);
        assert_eq!(header(0x3F).unwrap(), [0xFC]);
        assert_eq!(header(0x40).unwrap(), [0x01, 0x01]);
        assert_eq!(header(0x3FFF).unwrap().len(), 2);
        assert_eq!(header(0x4000).unwrap().len(), 3);
        assert_eq!(header(0x40_0000).unwrap().len(), 4);
    }

    #[test]
    fn frames_come_back_whole_however_they_arrive() {
        for len in [0usize, 1, 63, 64, 300, 0x4000 + 5] {
            let body: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut wire = header(len).unwrap();
            wire.extend_from_slice(&body);
            let mut buf = BytesMut::new();
            let mut got = None;
            for b in &wire {
                assert!(got.is_none());
                buf.extend_from_slice(&[*b]);
                got = take_frame(&mut buf).unwrap();
            }
            assert_eq!(got.unwrap().as_ref(), body.as_slice());
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn a_huge_frame_is_refused() {
        let mut buf = BytesMut::from(&header(MAX_FRAME + 1).unwrap()[..]);
        assert!(take_frame(&mut buf).is_err());
    }

    #[test]
    fn sealed_frames_open_in_order_on_the_other_end() {
        let key = [7u8; 32];
        let (mut a, mut b) = (Cipher::new(&key), Cipher::new(&key));
        let one = a.seal(b"first");
        let two = a.seal(b"second");
        assert_eq!(b.open(&one).unwrap(), b"first");
        assert_eq!(b.open(&two).unwrap(), b"second");
        // Out of order, the nonce no longer matches.
        let three = a.seal(b"third");
        let _four = a.seal(b"fourth");
        let mut c = Cipher::new(&key);
        c.received = 3;
        assert!(c.open(&three).is_err());
        // A lone byte passes unsealed and counts for nothing.
        assert_eq!(b.open(&[9]).unwrap(), [9]);
        assert_eq!(b.open(&three).unwrap(), b"third");
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn seals_as_libsodium_does() {
        // crypto_secretbox_easy("x", nonce 1 little endian, key 0).
        let mut c = Cipher::new(&[0u8; 32]);
        assert_eq!(c.seal(b"x"), hex("04ae5b07251c6d8613a931b4c313213526"));
    }

    #[test]
    fn a_sealed_frame_carries_a_sixteen_byte_tag() {
        let mut c = Cipher::new(&[0u8; 32]);
        let sealed = c.seal(b"x");
        assert_eq!(sealed.len(), 16 + 1);
        let mut d = Cipher::new(&[0u8; 32]);
        assert_eq!(d.open(&sealed).unwrap(), b"x");
    }
}
