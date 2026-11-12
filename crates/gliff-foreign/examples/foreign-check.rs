//! Try a foreign machine from the command line, with no window:
//!
//! ```text
//! FOREIGN_PASSWORD=… cargo run -p gliff-foreign --example foreign-check -- \
//!     rdp://user@host [view|control] [seconds] [frame.ppm]
//! ```
//!
//! It says what the host's ports answer, logs in, reports the screen and
//! the frames, moves the pointer across the screen, clicks, types "gliff",
//! and writes the last frame to a PPM file when asked.

use std::time::{Duration, Instant};

use gliff_foreign::{Credentials, Event, Purpose, Target};
use gliff_proto::ClientMsg;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();
    let mut args = std::env::args().skip(1);
    let text = args.next().unwrap_or_default();
    let target = Target::parse(&text)
        .ok_or_else(|| anyhow::anyhow!("{text:?} is not rdp://, rustdesk:// or vnc://"))?
        .map_err(anyhow::Error::msg)?;
    let purpose = match args.next().as_deref() {
        Some("control") => Purpose::Control,
        _ => Purpose::View {
            width: 1280,
            height: 800,
        },
    };
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let out = args.next();
    println!(
        "probe {}: {:?}",
        target.host,
        gliff_foreign::probe(&target.host).await
    );
    let credentials = Credentials {
        user: target.user.clone().unwrap_or_default(),
        password: std::env::var("FOREIGN_PASSWORD").unwrap_or_default(),
    };
    let known = std::env::var("FOREIGN_CERTIFICATE").ok();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let started = Instant::now();
    let mut frames = 0u32;
    let mut last: Option<(u32, u32, Vec<u8>)> = None;
    let mut size = None;
    let mut on_event = |e| match e {
        Event::Connected {
            width,
            height,
            video,
        } => {
            println!(
                "connected after {:?}: {width}x{height} {video}",
                started.elapsed()
            );
            size = Some((width, height));
            drive(&tx, width, height);
        }
        Event::Resized { width, height } => println!("resized: {width}x{height}"),
        Event::Frame {
            width,
            height,
            bgrx,
        } => {
            frames += 1;
            last = Some((width, height, bgrx));
        }
        Event::Cursor(c) => println!(
            "cursor {}x{} hot {},{}",
            c.width, c.height, c.hot_x, c.hot_y
        ),
        Event::CursorHidden => println!("cursor hidden"),
        Event::NewCertificate(fp) => println!("new certificate {fp}"),
    };
    let session = gliff_foreign::run(
        &target,
        &credentials,
        purpose,
        known.as_deref(),
        &mut rx,
        &mut on_event,
    );
    let result = tokio::time::timeout(Duration::from_secs(seconds), session).await;
    println!("frames: {frames}; screen {size:?}");
    match result {
        Err(_) => println!("ran {seconds} s"),
        Ok(Ok(())) => println!("the session ended"),
        Ok(Err(e)) => println!("error: {e} ({e:?})"),
    }
    if let (Some(path), Some((w, h, bgrx))) = (out, last) {
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in bgrx.as_chunks::<4>().0 {
            ppm.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        std::fs::write(&path, ppm)?;
        println!("wrote {path}");
    }
    Ok(())
}

/// Queue a short walk of the pointer, a click and some typing.
fn drive(tx: &tokio::sync::mpsc::UnboundedSender<ClientMsg>, w: u32, h: u32) {
    for i in 0..=10 {
        let _ = tx.send(ClientMsg::PointerMotion {
            x: f64::from(w) * f64::from(i) / 10.0,
            y: f64::from(h) / 2.0,
        });
    }
    let _ = tx.send(ClientMsg::PointerMotion {
        x: f64::from(w) / 2.0,
        y: f64::from(h) / 2.0,
    });
    for pressed in [true, false] {
        let _ = tx.send(ClientMsg::PointerButton {
            button: 0x110,
            pressed,
        });
    }
    // g l i f f
    for code in [34u32, 38, 23, 33, 33] {
        for pressed in [true, false] {
            let _ = tx.send(ClientMsg::Key {
                keycode: code,
                pressed,
            });
        }
    }
}

/// Logs to stderr, filtered by `RUST_LOG`.
fn tracing_subscriber_init() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
}
