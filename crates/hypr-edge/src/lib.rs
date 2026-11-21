//! Hand this machine's pointer and keyboard to another one at a screen edge.
//!
//! Each armed side of the layout gets a one-pixel layer surface along its
//! outer edge. When the pointer touches it, the pointer is locked there and
//! hidden, shortcuts are inhibited and the keyboard focused on it: from then
//! on relative motion, buttons, the wheel and keys arrive as [`EdgeEvent`]s
//! instead of reaching this desktop, until [`EdgeCmd::Release`] puts the
//! pointer back just inside that edge.

#![forbid(unsafe_code)]

mod thread;

use std::sync::mpsc;

use calloop::channel::Sender;
use hypr_wl::Target;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Wl(#[from] hypr_wl::Error),
    #[error(transparent)]
    Ipc(#[from] hypr_ipc::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("no screen is on the {0:?} side")]
    NoScreen(Side),
    #[error("the edge thread is gone")]
    ThreadGone,
}

pub type Result<T> = std::result::Result<T, Error>;

/// A side of this machine's screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
}

impl Side {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "above" => Some(Self::Above),
            "below" => Some(Self::Below),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Above => "above",
            Self::Below => "below",
        }
    }

    /// The side this one faces from the other machine.
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Above => Self::Below,
            Self::Below => Self::Above,
        }
    }

    /// Whether the edge runs top to bottom.
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone)]
pub struct EdgeConfig {
    pub target: Target,
    /// The sides that may be armed.
    pub sides: Vec<Side>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EdgeEvent {
    /// The local keymap, as XKB text; sent at start and on every change.
    Keymap(String),
    /// The pointer reached an armed edge and is captured. `along` is where,
    /// from 0 (top or left end of that edge) to 1.
    Entered {
        side: Side,
        along: f64,
    },
    /// Relative pointer motion in logical pixels, with acceleration.
    Motion {
        dx: f64,
        dy: f64,
    },
    /// An evdev key code.
    Key {
        code: u32,
        pressed: bool,
    },
    /// The keyboard's modifier and layout state, as `wl_keyboard` sends it.
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
    /// An evdev `BTN_*` code.
    Button {
        button: u32,
        pressed: bool,
    },
    Axis {
        axis: Axis,
        value: f64,
        discrete: Option<i32>,
        stop: bool,
    },
    /// The compositor ended the capture itself (another client took the
    /// pointer or keyboard, the screen locked). The pointer is free again.
    Lost,
    Error(String),
}

#[derive(Debug)]
pub enum EdgeCmd {
    /// Show or remove the edge on a side, so it captures only while there
    /// is somewhere to send the input.
    Arm {
        side: Side,
        armed: bool,
    },
    /// End the capture and put the pointer just inside the captured edge,
    /// `along` it as in [`EdgeEvent::Entered`].
    Release {
        along: f64,
    },
    Stop,
}

pub type EventSink = Box<dyn FnMut(EdgeEvent) + Send>;

pub struct Edges {
    cmd: Sender<EdgeCmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Edges {
    pub fn start(config: EdgeConfig, sink: EventSink) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (cmd, join) = thread::spawn(config, sink, ready_tx)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cmd,
                join: Some(join),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::ThreadGone),
        }
    }

    pub fn send(&self, cmd: EdgeCmd) -> Result<()> {
        self.cmd.send(cmd).map_err(|_| Error::ThreadGone)
    }
}

impl Drop for Edges {
    fn drop(&mut self) {
        let _ = self.cmd.send(EdgeCmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// A screen's rectangle in layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The screen at `side` of the layout, by name and rectangle: the one whose
/// edge on that side lies furthest out, the first listed among equals.
pub fn screen_on(monitors: &[hypr_ipc::Monitor], side: Side) -> Option<(String, Rect)> {
    let rects = monitors.iter().filter(|m| !m.disabled).map(|m| {
        let s = m.scale.max(0.5);
        let (w, h) = if m.transform % 2 == 1 {
            (m.height, m.width)
        } else {
            (m.width, m.height)
        };
        let rect = Rect {
            x: m.x,
            y: m.y,
            width: (w as f32 / s).round().max(1.0) as i32,
            height: (h as f32 / s).round().max(1.0) as i32,
        };
        (m.name.clone(), rect)
    });
    let edge = |r: &Rect| match side {
        Side::Left => -r.x,
        Side::Right => r.x + r.width,
        Side::Above => -r.y,
        Side::Below => r.y + r.height,
    };
    let best = rects.clone().map(|(_, r)| edge(&r)).max()?;
    rects.into_iter().find(|(_, r)| edge(r) == best)
}

/// How far inside the edge a released pointer lands, so it does not touch
/// the edge again at once.
pub const RELEASE_INSET: i32 = 8;

/// Where a pointer released at `along` an edge of `rect` goes.
pub fn release_point(rect: Rect, side: Side, along: f64) -> (i32, i32) {
    let along = along.clamp(0.0, 1.0);
    let at = |start: i32, len: i32| start + (along * (len - 1).max(0) as f64).round() as i32;
    let inset = RELEASE_INSET.min(rect.width / 2).min(rect.height / 2);
    match side {
        Side::Left => (rect.x + inset, at(rect.y, rect.height)),
        Side::Right => (rect.x + rect.width - 1 - inset, at(rect.y, rect.height)),
        Side::Above => (at(rect.x, rect.width), rect.y + inset),
        Side::Below => (at(rect.x, rect.width), rect.y + rect.height - 1 - inset),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: i32, y: i32, w: u32, h: u32, scale: f32) -> hypr_ipc::Monitor {
        hypr_ipc::Monitor {
            id: 0,
            name: name.into(),
            description: String::new(),
            width: w,
            height: h,
            refresh_rate: 60.0,
            x,
            y,
            scale,
            transform: 0,
            focused: false,
            disabled: false,
            dpms_status: true,
        }
    }

    #[test]
    fn finds_the_outer_screen_of_each_side() {
        let monitors = [
            monitor("eDP-1", 3648, 4, 3456, 2234, 2.0),
            monitor("DP-1", 1728, 0, 1920, 1080, 1.0),
        ];
        let (name, rect) = screen_on(&monitors, Side::Left).unwrap();
        assert_eq!(name, "DP-1");
        assert_eq!(rect.x, 1728);
        let (name, rect) = screen_on(&monitors, Side::Right).unwrap();
        assert_eq!(name, "eDP-1");
        assert_eq!((rect.width, rect.height), (1728, 1117));
        assert_eq!(screen_on(&monitors, Side::Below).unwrap().0, "eDP-1");
    }

    #[test]
    fn releases_just_inside_the_edge() {
        let rect = Rect {
            x: 3648,
            y: 4,
            width: 1728,
            height: 1117,
        };
        assert_eq!(release_point(rect, Side::Left, 0.0), (3656, 4));
        assert_eq!(release_point(rect, Side::Right, 1.0), (5367, 1120));
        assert_eq!(release_point(rect, Side::Above, 0.5), (4512, 12));
        assert_eq!(release_point(rect, Side::Below, 2.0), (5375, 1112));
    }

    #[test]
    fn sides_face_each_other() {
        for side in [Side::Left, Side::Right, Side::Above, Side::Below] {
            assert_eq!(side.opposite().opposite(), side);
            assert_eq!(Side::parse(side.name()), Some(side));
        }
        assert!(Side::Left.is_vertical() && !Side::Below.is_vertical());
    }
}
