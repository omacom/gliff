//! `--control SIDE`: no picture, only this machine's keyboard and pointer,
//! driven by a client whose screens lie on SIDE of this machine's. The
//! pointer is bound to the screen on that side, which the client's pointer
//! crosses into.

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};

use gliff_proto::{
    ClientMsg, OutputInfo as ProtoOutput, ServerMsg, SessionInfo, FEATURE_KEYBOARD_LOCKS,
    PROTOCOL_VERSION,
};
use gliff_transport::Framed;
use hypr_edge::{screen_on, Side};
use hypr_input::{Axis as InAxis, Input, InputCmd, InputConfig, InputEvent};
use hypr_wl::Target;

use crate::session::{handshake, now_ms, report_setup};

pub async fn run<R, W>(rd: R, wr: W, target: Target, side: Side) -> Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);
    let (keymap, _) = handshake(&mut reader, &mut writer).await?;
    let instance = report_setup(
        &mut writer,
        target.instance().context("find Hyprland instance"),
    )
    .await?;
    let monitors = report_setup(&mut writer, instance.monitors().context("list monitors")).await?;
    let screen = screen_on(&monitors, side)
        .and_then(|(name, _)| monitors.iter().find(|m| m.name == name))
        .with_context(|| format!("this machine has no screen on the {} side", side.name()));
    let monitor = report_setup(&mut writer, screen).await?.clone();
    let (_, rect) = screen_on(std::slice::from_ref(&monitor), side).expect("the screen just found");
    let (width, height) = (rect.width as u32, rect.height as u32);
    let mut config = InputConfig::new(monitor.name.clone());
    config.target = target.clone();
    config.keymap = (!keymap.is_empty()).then_some(keymap);
    let sink = Box::new(|event| {
        if let InputEvent::Error(e) = event {
            tracing::warn!(error = %e, "input");
        }
    });
    let input = report_setup(
        &mut writer,
        Input::start(config, sink).context("start the virtual keyboard and pointer"),
    )
    .await?;
    input.send(InputCmd::SetExtent { width, height })?;
    tracing::info!(output = %monitor.name, width, height, side = side.name(), "controlled from that side");
    // The client works out the extent from these, so a rotated screen
    // names its size as it stands, the way `SetExtent` has it.
    let (mode_width, mode_height) = if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    };
    writer
        .write_msg(&ServerMsg::HelloAck {
            version: PROTOCOL_VERSION,
            session: SessionInfo {
                headless: false,
                output: monitor.name.clone(),
            },
            outputs: vec![ProtoOutput {
                name: monitor.name.clone(),
                width: mode_width,
                height: mode_height,
                scale_milli: (monitor.scale * 1000.0).round() as u32,
            }],
            features: vec![FEATURE_KEYBOARD_LOCKS.into()],
        })
        .await?;
    loop {
        let msg = match reader.read_msg::<ClientMsg>().await {
            Ok(msg) => msg,
            Err(gliff_transport::Error::Closed) => break,
            Err(e) => return Err(e.into()),
        };
        let cmd = match msg {
            ClientMsg::Key { keycode, pressed } => InputCmd::Key {
                code: keycode,
                pressed,
            },
            ClientMsg::PointerMotion { x, y } => InputCmd::Motion { x, y },
            ClientMsg::PointerButton { button, pressed } => InputCmd::Button { button, pressed },
            ClientMsg::PointerAxis {
                axis,
                value,
                discrete,
                stop,
            } => InputCmd::Axis {
                axis: match axis {
                    gliff_proto::Axis::Vertical => InAxis::Vertical,
                    gliff_proto::Axis::Horizontal => InAxis::Horizontal,
                },
                value,
                discrete,
                stop,
            },
            ClientMsg::Keymap { keymap } if !keymap.is_empty() => InputCmd::SetKeymap(keymap),
            ClientMsg::KeyboardLocks { layout, locked } => InputCmd::SetLocks { layout, locked },
            ClientMsg::Ping { t } => {
                writer
                    .write_msg(&ServerMsg::Pong {
                        t,
                        server_now_ms: now_ms(),
                    })
                    .await?;
                continue;
            }
            ClientMsg::Bye => break,
            _ => continue,
        };
        input
            .send(cmd)
            .context("the virtual keyboard and pointer stopped")?;
    }
    // Dropping the input releases every key and button still held.
    Ok(())
}
