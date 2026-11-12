//! A RustDesk server made for the test, speaking the protocol as RustDesk
//! does, against the client here.

use super::*;
use tokio::net::TcpListener;
use tokio::sync::mpsc::unbounded_channel;

const PASSWORD: &str = "correct horse";

/// Serve one client: identity, key exchange, challenge, login; then hand
/// the stream to `then`.
async fn serve<F, Fut>(listener: TcpListener, then: F) -> anyhow::Result<()>
where
    F: FnOnce(Stream<TcpStream>, proto::LoginRequest) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let (io, _) = listener.accept().await?;
    let mut s = Stream::new(io);
    let ours = SecretKey::generate(&mut OsRng);
    let idpk = proto::IdPk {
        id: "123456789".into(),
        pk: ours.public_key().as_bytes().to_vec(),
        kx_version: 1,
    };
    let mut signed = vec![0u8; 64];
    signed.extend(idpk.encode_to_vec());
    s.send(&msg(Union::SignedId(proto::SignedId { id: signed })))
        .await?;
    let Some(Union::PublicKey(pk)) = s.recv().await?.union else {
        anyhow::bail!("expected a public key");
    };
    assert_eq!(pk.kx_version, 0);
    let theirs: [u8; 32] = pk.asymmetric_value.as_slice().try_into()?;
    let secret = SalsaBox::new(&BoxPublicKey::from(theirs), &ours)
        .decrypt(&Default::default(), pk.symmetric_value.as_slice())
        .map_err(|_| anyhow::anyhow!("the sealed secret does not open"))?;
    s.cipher = Some(Cipher::new(secret.as_slice().try_into()?));
    s.send(&msg(Union::Hash(proto::Hash {
        salt: "salt".into(),
        challenge: "challenge".into(),
    })))
    .await?;
    let Some(Union::LoginRequest(lr)) = s.recv().await?.union else {
        anyhow::bail!("expected a login");
    };
    then(s, lr).await
}

fn target(port: u16) -> Target {
    Target {
        kind: crate::Kind::RustDesk,
        user: None,
        host: "127.0.0.1".into(),
        port,
    }
}

fn credentials(password: &str) -> Credentials {
    Credentials {
        user: String::new(),
        password: password.into(),
    }
}

fn peer_info() -> proto::PeerInfo {
    proto::PeerInfo {
        platform: "Windows".into(),
        hostname: "pc".into(),
        displays: vec![
            proto::DisplayInfo {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                name: "one".into(),
            },
            proto::DisplayInfo {
                x: 1920,
                y: 0,
                width: 1280,
                height: 1024,
                name: "two".into(),
            },
        ],
        current_display: 1,
        ..Default::default()
    }
}

#[tokio::test]
async fn logs_in_encrypted_and_drives_the_screen() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(serve(listener, |mut s, lr| async move {
        assert_eq!(lr.username, "127.0.0.1");
        assert_eq!(lr.password, password_proof(PASSWORD, "salt", "challenge"));
        assert_eq!(lr.my_platform, "Linux");
        let option = lr.option.expect("options");
        assert_eq!(option.custom_fps, CONTROL_FPS);
        let decoding = option.supported_decoding.expect("codecs");
        assert_eq!((decoding.ability_vp9, decoding.ability_h264), (1, 0));
        s.send(&msg(Union::LoginResponse(proto::LoginResponse {
            union: Some(proto::login_response::Union::PeerInfo(peer_info())),
        })))
        .await?;
        let delay = proto::TestDelay {
            time: 42,
            ..Default::default()
        };
        s.send(&msg(Union::TestDelay(delay.clone()))).await?;
        let mut got = Vec::new();
        while got.len() < 6 {
            got.push(s.recv().await?.union.expect("a message"));
        }
        // The echo races the input already queued.
        let echo = got
            .iter()
            .position(|m| matches!(m, Union::TestDelay(_)))
            .expect("the delay test echoed");
        assert_eq!(got.remove(echo), Union::TestDelay(delay));
        got.insert(0, Union::TestDelay(proto::TestDelay::default()));
        // The second screen starts at x 1920, so the pointer lands there.
        assert_eq!(
            got[1],
            Union::MouseEvent(proto::MouseEvent {
                mask: proto::MOUSE_TYPE_MOVE,
                x: 1920 + 10,
                y: 20,
                modifiers: vec![],
            })
        );
        assert_eq!(
            got[2],
            Union::MouseEvent(proto::MouseEvent {
                mask: proto::MOUSE_BUTTON_LEFT << 3 | proto::MOUSE_TYPE_DOWN,
                x: 1930,
                y: 20,
                modifiers: vec![],
            })
        );
        // Right Ctrl, by its Windows scan code, with Caps Lock locked.
        assert_eq!(
            got[3],
            Union::KeyEvent(proto::KeyEvent {
                down: true,
                press: false,
                union: Some(proto::key_event::Union::Chr(0xE01D)),
                modifiers: vec![proto::CONTROL_KEY_CAPS_LOCK],
                mode: proto::KEYBOARD_MODE_MAP,
            })
        );
        // One notch down is one step the other way.
        assert_eq!(
            got[4],
            Union::MouseEvent(proto::MouseEvent {
                mask: proto::MOUSE_TYPE_WHEEL,
                x: 0,
                y: -1,
                modifiers: vec![],
            })
        );
        assert!(matches!(&got[5], Union::KeyEvent(k) if !k.down));
        s.send(&msg(Union::Misc(proto::Misc {
            union: Some(proto::misc::Union::CloseReason("bye".into())),
        })))
        .await?;
        Ok(())
    }));
    let (tx, mut rx) = unbounded_channel();
    for m in [
        ClientMsg::PointerMotion { x: 10.0, y: 20.0 },
        ClientMsg::PointerButton {
            button: 0x110,
            pressed: true,
        },
        ClientMsg::KeyboardLocks {
            layout: 0,
            locked: vec!["Lock".into()],
        },
        ClientMsg::Key {
            keycode: 97,
            pressed: true,
        },
        ClientMsg::PointerAxis {
            axis: Axis::Vertical,
            value: 15.0,
            discrete: Some(1),
            stop: false,
        },
        ClientMsg::Key {
            keycode: 97,
            pressed: false,
        },
    ] {
        tx.send(m)?;
    }
    let mut seen = Vec::new();
    let result = run(
        &target(port),
        &credentials(PASSWORD),
        Purpose::Control,
        &mut rx,
        &mut |e| seen.push(e),
    )
    .await;
    server.await??;
    let err = result.expect_err("the server closed the session");
    assert!(err.to_string().contains("bye"), "{err}");
    assert!(matches!(
        seen.first(),
        Some(Event::Connected {
            width: 1280,
            height: 1024,
            ..
        })
    ));
    Ok(())
}

#[tokio::test]
async fn a_wrong_password_asks_again() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(serve(listener, |mut s, lr| async move {
        assert_ne!(lr.password, password_proof(PASSWORD, "salt", "challenge"));
        s.send(&msg(Union::LoginResponse(proto::LoginResponse {
            union: Some(proto::login_response::Union::Error("Wrong Password".into())),
        })))
        .await?;
        Ok(())
    }));
    let (_tx, mut rx) = unbounded_channel();
    let result = run(
        &target(port),
        &credentials("nope"),
        Purpose::Control,
        &mut rx,
        &mut |_| {},
    )
    .await;
    server.await??;
    assert!(matches!(result, Err(Error::Auth(_))), "{result:?}");
    Ok(())
}

#[tokio::test]
async fn direct_ip_access_logs_in_without_a_key_exchange() {
    // RustDesk's direct server opens with the login challenge and no key.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (io, _) = listener.accept().await?;
        let mut s = Stream::new(io);
        s.send(&msg(Union::Hash(proto::Hash {
            salt: "salt".into(),
            challenge: "challenge".into(),
        })))
        .await?;
        let Some(Union::LoginRequest(lr)) = s.recv().await?.union else {
            anyhow::bail!("expected a login");
        };
        anyhow::ensure!(lr.password == password_proof(PASSWORD, "salt", "challenge"));
        s.send(&msg(Union::LoginResponse(proto::LoginResponse {
            union: Some(proto::login_response::Union::Error("Wrong Password".into())),
        })))
        .await?;
        anyhow::Ok(())
    });
    let (_tx, mut rx) = unbounded_channel();
    let result = run(
        &target(port),
        &credentials(PASSWORD),
        Purpose::Control,
        &mut rx,
        &mut |_| {},
    )
    .await;
    server.await.unwrap().unwrap();
    // The proof went out and the server's answer came back.
    assert!(matches!(result, Err(Error::Auth(_))), "{result:?}");
}

#[tokio::test]
async fn a_rustdesk_server_answers_the_probe() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (io, _) = listener.accept().await?;
        let mut s = Stream::new(io);
        s.send(&msg(Union::SignedId(proto::SignedId { id: vec![0; 70] })))
            .await?;
        anyhow::Ok(())
    });
    assert!(answers(TcpStream::connect(addr).await?).await);
    server.await??;

    // Something else on the port does not count.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt as _;
        let (mut io, _) = listener.accept().await?;
        io.write_all(b"SSH-2.0-OpenSSH_9.9\r\n").await?;
        anyhow::Ok(())
    });
    assert!(!answers(TcpStream::connect(addr).await?).await);
    server.await??;
    Ok(())
}

#[test]
fn keys_go_by_the_other_platform() {
    let chr = |p, code| match key_event(p, code, true, &[]).and_then(|k| k.union) {
        Some(proto::key_event::Union::Chr(c)) => Some(c),
        _ => None,
    };
    assert_eq!(chr(Platform::Windows, 30), Some(0x1E));
    assert_eq!(chr(Platform::Mac, 30), Some(0x00));
    assert_eq!(chr(Platform::Linux, 30), Some(38));
    assert_eq!(chr(Platform::Other, 30), None);
    let pause = key_event(Platform::Windows, 119, true, &[]).unwrap();
    assert_eq!(
        pause.union,
        Some(proto::key_event::Union::ControlKey(
            proto::CONTROL_KEY_PAUSE
        ))
    );
    assert_eq!(Platform::of("Mac OS"), Platform::Mac);
    assert_eq!(Platform::of("Windows"), Platform::Windows);
}

#[test]
fn the_offered_key_follows_the_signature() {
    let idpk = proto::IdPk {
        id: "1".into(),
        pk: vec![5; 32],
        kx_version: 0,
    };
    let mut signed = vec![0u8; 64];
    signed.extend(idpk.encode_to_vec());
    assert_eq!(offered_key(&signed), Some([5; 32]));
    assert_eq!(offered_key(&signed[..60]), None);
}

#[test]
fn the_box_seals_as_libsodium_does() {
    // crypto_box_easy(100..132, zero nonce, to the public key of
    // secret 33..65, from secret 1..33), computed with libsodium.
    let from = SecretKey::from(<[u8; 32]>::try_from((1..=32).collect::<Vec<u8>>()).unwrap());
    let to = SecretKey::from(<[u8; 32]>::try_from((33..=64).collect::<Vec<u8>>()).unwrap());
    assert_eq!(
        hex(to.public_key().as_bytes()),
        "5869aff450549732cbaaed5e5df9b30a6da31cb0e5742bad5ad4a1a768f1a67b"
    );
    let secret: Vec<u8> = (100..132).collect();
    let sealed = SalsaBox::new(&to.public_key(), &from)
        .encrypt(&Default::default(), secret.as_slice())
        .unwrap();
    assert_eq!(
        hex(&sealed),
        "baa82b812a819d31671acdd55380716a87c701ea335501c2ae31591732735875\
         886c60f7bb0f3ab02583329f1d7595d5"
    );
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn every_login_to_a_machine_is_one_session() {
    assert_eq!(session_id(&target(21118)), session_id(&target(21118)));
    assert_ne!(session_id(&target(21118)), session_id(&target(21119)));
}
