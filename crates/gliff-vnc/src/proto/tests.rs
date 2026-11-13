use super::*;
use tokio::io::duplex;

const CHALLENGE: [u8; 16] = *b"fedcba9876543210";

/// Run the server side of a handshake against a scripted client.
async fn run<F, Fut>(security: Security, client: F) -> Result<Hello>
where
    F: FnOnce(tokio::io::DuplexStream) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (mut server, client_end) = duplex(4096);
    let task = tokio::spawn(client(client_end));
    let login = Login {
        security: &security,
        challenge: CHALLENGE,
        wrong_password_delay: std::time::Duration::from_millis(50),
        size: (1024, 768),
        name: "gliff",
    };
    let result = handshake(&mut server, login).await;
    drop(server);
    task.await.unwrap();
    result
}

async fn expect(c: &mut tokio::io::DuplexStream, want: &[u8]) {
    let mut got = vec![0u8; want.len()];
    c.read_exact(&mut got).await.unwrap();
    assert_eq!(got, want);
}

async fn server_init(c: &mut tokio::io::DuplexStream) {
    let mut init = [0u8; 24];
    c.read_exact(&mut init).await.unwrap();
    assert_eq!(&init[0..4], &[4, 0, 3, 0]);
    assert_eq!(
        PixelFormat::from_bytes(init[4..20].try_into().unwrap()),
        PixelFormat::BGRX
    );
    assert_eq!(&init[20..24], &5u32.to_be_bytes());
    expect(c, b"gliff").await;
}

#[tokio::test]
async fn a_3_3_client_with_the_password() {
    let hello = run(Security::Password(b"secret".to_vec()), |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        c.write_all(b"RFB 003.003\n").await.unwrap();
        // 3.3: the server names the one security type as a u32.
        expect(&mut c, &2u32.to_be_bytes()).await;
        expect(&mut c, &CHALLENGE).await;
        c.write_all(&auth::response(b"secret", &CHALLENGE))
            .await
            .unwrap();
        expect(&mut c, &0u32.to_be_bytes()).await;
        c.write_all(&[1]).await.unwrap();
        server_init(&mut c).await;
    })
    .await
    .unwrap();
    assert_eq!(
        hello,
        Hello {
            version: Version::V3_3,
            shared: true
        }
    );
}

#[tokio::test]
async fn a_3_8_client_with_a_wrong_password_hears_why_only_after_the_delay() {
    let started = std::time::Instant::now();
    let err = run(Security::Password(b"secret".to_vec()), |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        c.write_all(b"RFB 003.008\n").await.unwrap();
        expect(&mut c, &[1, 2]).await;
        c.write_all(&[2]).await.unwrap();
        expect(&mut c, &CHALLENGE).await;
        c.write_all(&auth::response(b"guess", &CHALLENGE))
            .await
            .unwrap();
        expect(&mut c, &1u32.to_be_bytes()).await;
        expect(&mut c, &14u32.to_be_bytes()).await;
        expect(&mut c, b"wrong password").await;
    })
    .await
    .unwrap_err();
    assert!(matches!(err, Error::WrongPassword));
    assert!(started.elapsed() >= std::time::Duration::from_millis(50));
}

#[tokio::test]
async fn a_3_7_client_without_authentication_gets_no_security_result() {
    let hello = run(Security::None, |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        c.write_all(b"RFB 003.007\n").await.unwrap();
        expect(&mut c, &[1, 1]).await;
        c.write_all(&[1]).await.unwrap();
        c.write_all(&[0]).await.unwrap();
        server_init(&mut c).await;
    })
    .await
    .unwrap();
    assert_eq!(
        hello,
        Hello {
            version: Version::V3_7,
            shared: false
        }
    );
}

#[tokio::test]
async fn a_3_8_client_without_authentication_gets_a_security_result() {
    run(Security::None, |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        // Apple's screen sharing announces 3.889.
        c.write_all(b"RFB 003.889\n").await.unwrap();
        expect(&mut c, &[1, 1]).await;
        c.write_all(&[1]).await.unwrap();
        expect(&mut c, &0u32.to_be_bytes()).await;
        c.write_all(&[1]).await.unwrap();
        server_init(&mut c).await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_client_choosing_another_security_type_is_refused() {
    let err = run(Security::Password(b"x".to_vec()), |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        c.write_all(b"RFB 003.008\n").await.unwrap();
        expect(&mut c, &[1, 2]).await;
        c.write_all(&[1]).await.unwrap();
        expect(&mut c, &1u32.to_be_bytes()).await;
    })
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Protocol(_)));
}

#[tokio::test]
async fn garbage_is_not_a_client() {
    let err = run(Security::None, |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        c.write_all(b"GET / HTTP/1").await.unwrap();
    })
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Protocol(_)));
}

#[tokio::test]
async fn a_line_that_is_not_ascii_is_refused_not_a_panic() {
    // "é" is two bytes: slicing the line as text would split it.
    let err = run(Security::None, |mut c| async move {
        expect(&mut c, b"RFB 003.008\n").await;
        c.write_all("RFB 00é000\n".as_bytes()).await.unwrap();
    })
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Protocol(_)));
}

#[tokio::test]
async fn client_messages_parse() {
    let (mut a, mut b) = duplex(4096);
    let mut bytes = vec![0u8, 0, 0, 0];
    bytes.extend_from_slice(&PixelFormat::PALETTE_332.to_bytes());
    bytes.extend_from_slice(&[2, 0, 0, 2]);
    bytes.extend_from_slice(&5i32.to_be_bytes());
    bytes.extend_from_slice(&(-223i32).to_be_bytes());
    bytes.extend_from_slice(&[3, 1, 0, 1, 0, 2, 0, 3, 0, 4]);
    bytes.extend_from_slice(&[4, 1, 0, 0, 0, 0, 0, 0x61]);
    bytes.extend_from_slice(&[5, 0b101, 1, 0, 0, 20]);
    bytes.extend_from_slice(&[6, 0, 0, 0, 0, 0, 0, 2, b'h', b'i']);
    b.write_all(&bytes).await.unwrap();
    assert_eq!(
        read_msg(&mut a).await.unwrap(),
        ClientMsg::SetPixelFormat(PixelFormat::PALETTE_332)
    );
    assert_eq!(
        read_msg(&mut a).await.unwrap(),
        ClientMsg::SetEncodings(vec![5, -223])
    );
    assert_eq!(
        read_msg(&mut a).await.unwrap(),
        ClientMsg::UpdateRequest {
            incremental: true,
            area: Rect {
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }
        }
    );
    assert_eq!(
        read_msg(&mut a).await.unwrap(),
        ClientMsg::Key {
            down: true,
            keysym: 0x61
        }
    );
    assert_eq!(
        read_msg(&mut a).await.unwrap(),
        ClientMsg::Pointer {
            buttons: 5,
            x: 256,
            y: 20
        }
    );
    assert_eq!(
        read_msg(&mut a).await.unwrap(),
        ClientMsg::CutText(b"hi".to_vec())
    );
    b.write_all(&[99]).await.unwrap();
    assert!(matches!(read_msg(&mut a).await, Err(Error::Protocol(_))));
}
