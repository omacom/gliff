use std::io::Write as _;

use flate2::write::ZlibEncoder;
use tokio::io::DuplexStream;

use super::*;

fn target() -> Target {
    Target {
        kind: crate::Kind::Vnc,
        user: None,
        host: "pc".into(),
        port: 5900,
    }
}

fn credentials(password: &str) -> Credentials {
    Credentials {
        user: String::new(),
        password: password.into(),
    }
}

#[test]
fn the_vnc_password_answers_like_other_clients() {
    // openssl enc -des-ecb -nopad -K cea6c64ea62e0000 of "0123456789abcdef":
    // "secret" padded and bit-reversed.
    let got = vnc_response("secret", b"0123456789abcdef");
    let want = [
        0x75, 0x24, 0x40, 0xee, 0x2b, 0xfc, 0xc2, 0xa0, 0xd9, 0x01, 0x3f, 0xd2, 0x03, 0x71, 0xe2,
        0x3b,
    ];
    assert_eq!(got, want);
}

#[test]
fn versions_are_read_and_agreed() {
    assert_eq!(version(b"RFB 003.008\n"), Some((3, 8)));
    assert_eq!(version(b"RFB 003.889\n"), Some((3, 889)));
    assert_eq!(version(b"SSH-2.0-Open"), None);
    assert_eq!(speak(3, 3), 3);
    assert_eq!(speak(3, 5), 3);
    assert_eq!(speak(3, 7), 7);
    assert_eq!(speak(3, 8), 8);
    assert_eq!(speak(3, 889), 8);
}

#[test]
fn vencrypt_takes_a_certificate_first_and_never_cleartext() {
    assert_eq!(
        pick_vencrypt(&[257, 258, 262, 2], false),
        Some(VENCRYPT_X509_PLAIN)
    );
    assert_eq!(pick_vencrypt(&[257, 2], false), Some(VENCRYPT_VNC));
    assert_eq!(pick_vencrypt(&[2, 1, 262], true), Some(VENCRYPT_X509_PLAIN));
    assert_eq!(pick_vencrypt(&[2, 1], true), None);
    assert_eq!(pick_vencrypt(&[256, 257, 259], false), None);
}

/// The server's side of a 3.8 login with the VNC password `password`,
/// ending with a `w`×`h` ServerInit; whether the client's answer was right.
async fn serve_login(s: &mut DuplexStream, password: &str, w: u16, h: u16) -> bool {
    s.write_all(b"RFB 003.008\n").await.unwrap();
    let mut v = [0u8; 12];
    s.read_exact(&mut v).await.unwrap();
    assert_eq!(&v, b"RFB 003.008\n");
    s.write_all(&[1, SECURITY_VNC]).await.unwrap();
    assert_eq!(s.read_u8().await.unwrap(), SECURITY_VNC);
    let challenge = *b"fedcba9876543210";
    s.write_all(&challenge).await.unwrap();
    let mut answer = [0u8; 16];
    s.read_exact(&mut answer).await.unwrap();
    if answer != vnc_response(password, &challenge) {
        s.write_all(&1u32.to_be_bytes()).await.unwrap();
        let why = b"Authentication failed";
        s.write_all(&(why.len() as u32).to_be_bytes())
            .await
            .unwrap();
        s.write_all(why).await.unwrap();
        return false;
    }
    s.write_all(&0u32.to_be_bytes()).await.unwrap();
    assert_eq!(s.read_u8().await.unwrap(), 1, "shared");
    s.write_all(&w.to_be_bytes()).await.unwrap();
    s.write_all(&h.to_be_bytes()).await.unwrap();
    s.write_all(&[0; 16]).await.unwrap();
    s.write_all(&4u32.to_be_bytes()).await.unwrap();
    s.write_all(b"desk").await.unwrap();
    true
}

#[tokio::test]
async fn a_wrong_password_is_refused_as_a_login() {
    let (client, mut server) = tokio::io::duplex(1 << 16);
    let serve = tokio::spawn(async move { serve_login(&mut server, "right", 4, 2).await });
    let result = handshake(Box::new(client), &target(), &credentials("wrong"), None).await;
    assert!(!serve.await.unwrap());
    match result {
        Err(Error::Auth(why)) => assert!(why.contains("Authentication failed"), "{why}"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("logged in"),
    }
}

fn rect(x: u16, y: u16, w: u16, h: u16, encoding: i32) -> Vec<u8> {
    let mut m = Vec::new();
    for v in [x, y, w, h] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    m.extend_from_slice(&encoding.to_be_bytes());
    m
}

/// The next part of one zlib stream, as ZRLE runs it across rectangles.
fn zlib(z: &mut ZlibEncoder<Vec<u8>>, data: &[u8]) -> Vec<u8> {
    z.write_all(data).unwrap();
    z.flush().unwrap();
    std::mem::take(z.get_mut())
}

#[tokio::test]
async fn logs_in_and_draws_raw_zrle_and_copies() {
    let (client, mut server) = tokio::io::duplex(1 << 16);
    let serve = tokio::spawn(async move {
        assert!(serve_login(&mut server, "secret", 4, 2).await);
        let mut format = [0u8; 20];
        server.read_exact(&mut format).await.unwrap();
        assert_eq!(format, set_pixel_format());
        let mut head = [0u8; 4];
        server.read_exact(&mut head).await.unwrap();
        let n = u16::from_be_bytes([head[2], head[3]]);
        let mut encodings = vec![0u8; usize::from(n) * 4];
        server.read_exact(&mut encodings).await.unwrap();
        let mut request = [0u8; 10];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(request, update_request(false, 4, 2));

        let mut stream = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        let mut m = vec![0, 0, 0, 4];
        // Two raw pixels on the top row.
        m.extend(rect(0, 0, 2, 1, ENCODING_RAW));
        m.extend([1, 2, 3, 0, 4, 5, 6, 0]);
        // A solid ZRLE tile on the bottom row's left half...
        m.extend(rect(0, 1, 2, 1, ENCODING_ZRLE));
        let z = zlib(&mut stream, &[1, 9, 8, 7]);
        m.extend((z.len() as u32).to_be_bytes());
        m.extend(z);
        // ...and a two-colour palette tile, the stream running on.
        m.extend(rect(2, 1, 2, 1, ENCODING_ZRLE));
        let z = zlib(&mut stream, &[2, 10, 11, 12, 20, 21, 22, 0b0100_0000]);
        m.extend((z.len() as u32).to_be_bytes());
        m.extend(z);
        // The top row's two pixels copied to its right half.
        m.extend(rect(2, 0, 2, 1, ENCODING_COPY_RECT));
        m.extend([0, 0, 0, 0]);
        server.write_all(&m).await.unwrap();

        let mut request = [0u8; 10];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(request, update_request(true, 4, 2));
        // A click at 3,1.
        let mut pointer = [0u8; 12];
        server.read_exact(&mut pointer).await.unwrap();
        assert_eq!(pointer, [5, 0, 0, 3, 0, 1, 5, 1, 0, 3, 0, 1]);
    });

    let (s, init, _) = handshake(Box::new(client), &target(), &credentials("secret"), None)
        .await
        .unwrap();
    assert_eq!(
        (init.width, init.height, init.name.as_str()),
        (4, 2, "desk")
    );
    let (tx, mut input) = unbounded_channel();
    let mut frames = Vec::new();
    // Kept open, so it is the server closing that ends the session.
    let _open = tx.clone();
    let mut tx = Some(tx);
    let ended = session(
        s,
        &target(),
        init,
        Purpose::View {
            width: 4,
            height: 2,
        },
        &mut input,
        &mut |e| {
            if let Event::Frame { bgrx, .. } = e {
                frames.push(bgrx);
                if let Some(tx) = tx.take() {
                    tx.send(ClientMsg::PointerMotion { x: 3.0, y: 1.0 })
                        .unwrap();
                    tx.send(ClientMsg::PointerButton {
                        button: 0x110,
                        pressed: true,
                    })
                    .unwrap();
                }
            }
        },
    )
    .await;
    serve.await.unwrap();
    assert!(ended.is_err(), "the server closing ends the session");
    assert_eq!(
        frames[0],
        [
            1, 2, 3, 0, 4, 5, 6, 0, 1, 2, 3, 0, 4, 5, 6, 0, //
            9, 8, 7, 0, 9, 8, 7, 0, 10, 11, 12, 0, 20, 21, 22, 0,
        ]
    );
}

#[test]
fn zrle_runs_fill_the_tile() {
    let mut screen = Screen::new(3, 2, true);
    // Plain RLE: one pixel for 4, another for 2.
    let data = [128, 1, 1, 1, 3, 2, 2, 2, 1];
    picture::zrle(&mut screen, 0, 0, 3, 2, &data).unwrap();
    let px: Vec<u8> = screen.bgrx.chunks(4).map(|p| p[0]).collect();
    assert_eq!(px, [1, 1, 1, 1, 2, 2]);
    // Palette RLE: index 1 alone, then index 0 run for 5.
    let data = [130, 5, 5, 5, 6, 6, 6, 1, 0x80, 4];
    picture::zrle(&mut screen, 0, 0, 3, 2, &data).unwrap();
    let px: Vec<u8> = screen.bgrx.chunks(4).map(|p| p[0]).collect();
    assert_eq!(px, [6, 5, 5, 5, 5, 5]);
    // A run past the tile is refused.
    assert!(picture::zrle(&mut screen, 0, 0, 3, 2, &[128, 1, 1, 1, 9]).is_err());
}

#[test]
fn cursors_take_their_mask_as_alpha() {
    let pixels = [1, 2, 3, 0, 4, 5, 6, 0];
    assert_eq!(
        cursor_image(2, 1, &pixels, &[0b1000_0000]),
        [1, 2, 3, 255, 4, 5, 6, 0]
    );
}

#[test]
fn input_becomes_rfb_events() {
    let mut i = Input::default();
    let size = (100, 50);
    assert_eq!(
        i.translate(ClientMsg::PointerMotion { x: 500.0, y: -3.0 }, size),
        [vec![5, 0, 0, 99, 0, 0]]
    );
    let scroll = i.translate(
        ClientMsg::PointerAxis {
            axis: Axis::Vertical,
            value: 30.0,
            discrete: None,
            stop: false,
        },
        size,
    );
    assert_eq!(
        scroll,
        [
            vec![5, 16, 0, 99, 0, 0],
            vec![5, 0, 0, 99, 0, 0],
            vec![5, 16, 0, 99, 0, 0],
            vec![5, 0, 0, 99, 0, 0]
        ]
    );
    // Without QEMU keys: the keysym, shifted.
    i.translate(
        ClientMsg::Key {
            keycode: 42,
            pressed: true,
        },
        size,
    );
    assert_eq!(
        i.translate(
            ClientMsg::Key {
                keycode: 30,
                pressed: true
            },
            size
        ),
        [vec![4, 1, 0, 0, 0, 0, 0, b'A']]
    );
    // With them: the position too, an extended key's prefix folded in.
    i.qemu_keys = true;
    assert_eq!(
        i.translate(
            ClientMsg::Key {
                keycode: 103,
                pressed: false
            },
            size
        ),
        [vec![255, 0, 0, 0, 0, 0, 0xff, 0x52, 0, 0, 0, 0xc8]]
    );
}

#[test]
fn a_key_goes_up_as_the_keysym_it_went_down_as() {
    let mut i = Input::default();
    let size = (100, 50);
    let key =
        |i: &mut Input, keycode, pressed| i.translate(ClientMsg::Key { keycode, pressed }, size);
    key(&mut i, 42, true);
    assert_eq!(key(&mut i, 30, true), [vec![4, 1, 0, 0, 0, 0, 0, b'A']]);
    key(&mut i, 42, false);
    // Shift is up now, but A goes up as it went down.
    assert_eq!(key(&mut i, 30, false), [vec![4, 0, 0, 0, 0, 0, 0, b'A']]);
}

#[tokio::test]
async fn a_saved_certificate_is_not_given_up_for_a_login_without_it() {
    let (client, mut server) = tokio::io::duplex(1 << 16);
    let serve = tokio::spawn(async move {
        server.write_all(b"RFB 003.008\n").await.unwrap();
        let mut v = [0u8; 12];
        server.read_exact(&mut v).await.unwrap();
        // Only a password, no TLS, from a machine whose certificate is saved.
        server.write_all(&[1, SECURITY_VNC]).await.unwrap();
        let mut rest = Vec::new();
        let _ = server.read_to_end(&mut rest).await;
        rest
    });
    let result = handshake(
        Box::new(client),
        &target(),
        &credentials("secret"),
        Some("AA:BB"),
    )
    .await;
    let sent = serve.await.unwrap();
    assert!(sent.is_empty(), "the client chose a login: {sent:?}");
    let why = result
        .err()
        .expect("logged in without the certificate")
        .to_string();
    assert!(why.contains("posing"), "{why}");
}
