//! VNC Authentication (RFB security type 2): DES challenge-response.
//!
//! It keeps the password off the wire, but DES and an 8-character password
//! are weak by today's standards, and nothing after the handshake is
//! encrypted. Serve it only on a network you trust.

use des::cipher::{BlockEncrypt, KeyInit};
use des::Des;

/// The longest password VNC Authentication uses; longer ones are cut.
pub const MAX_PASSWORD: usize = 8;

/// The response a client holding `password` gives to `challenge`.
pub fn response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    // The DES key is the password, padded with zeros, with each byte's bits
    // reversed: VNC's historical quirk.
    let mut key = [0u8; 8];
    for (k, p) in key.iter_mut().zip(password.iter().take(MAX_PASSWORD)) {
        *k = p.reverse_bits();
    }
    let cipher = Des::new(&key.into());
    let mut out = *challenge;
    for block in out.chunks_exact_mut(8) {
        cipher.encrypt_block(block.into());
    }
    out
}

/// Whether `answer` proves knowledge of `password`, compared in constant
/// time.
pub fn verify(password: &[u8], challenge: &[u8; 16], answer: &[u8; 16]) -> bool {
    let expected = response(password, challenge);
    expected
        .iter()
        .zip(answer)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_known_vnc_response() {
        // Computed independently with OpenSSL's DES-ECB and the bit-reversed
        // key for "password" (0e86ceceeef64e26).
        let challenge: [u8; 16] = *b"0123456789abcdef";
        let expected = EXPECTED;
        assert_eq!(response(b"password", &challenge), expected);
        assert!(verify(b"password", &challenge, &expected));
        assert!(!verify(b"passwore", &challenge, &expected));
    }

    #[test]
    fn only_the_first_eight_characters_count() {
        let challenge = [7u8; 16];
        assert_eq!(
            response(b"password", &challenge),
            response(b"password-and-more", &challenge)
        );
        assert_ne!(
            response(b"pass", &challenge),
            response(b"pass\0x", &challenge)
        );
    }

    const EXPECTED: [u8; 16] = [
        0x56, 0x45, 0xab, 0xeb, 0x5f, 0x1e, 0x64, 0x75, 0xe8, 0xfe, 0xb1, 0x1b, 0xeb, 0x66, 0xea,
        0x19,
    ];
}
