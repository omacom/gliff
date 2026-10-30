use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::mpsc;

use calloop::channel::{self, Sender};
use nix::sys::memfd::{memfd_create, MFdFlags};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1;
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
    self, ZwpLockedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
    self, ZwpRelativePointerV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    Layer, ZwlrLayerShellV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

use crate::{
    release_point, screen_on, Axis, EdgeCmd, EdgeConfig, EdgeEvent, Error, EventSink, Rect, Result,
    Side,
};
use hypr_wl::{LoopState, Outputs, Seat};

const NAMESPACE: &str = "gliff-edge";

/// One side's edge: the screen it runs along and, while armed, its surface.
struct Edge {
    side: Side,
    output: String,
    rect: Rect,
    /// Whether the owner wants it to take the pointer. It keeps a surface
    /// while armed, given again when its screen changes or the compositor
    /// closes it.
    armed: bool,
    mapped: Option<Mapped>,
}

struct Mapped {
    surface: WlSurface,
    layer: ZwlrLayerSurfaceV1,
    /// The configured size, in logical pixels.
    size: (u32, u32),
    buffer: Option<WlBuffer>,
}

struct Capture {
    edge: usize,
    lock: ZwpLockedPointerV1,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

struct State {
    sink: EventSink,
    conn: Connection,
    qh: QueueHandle<State>,
    instance: hypr_ipc::Instance,
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    constraints: ZwpPointerConstraintsV1,
    relative_mgr: ZwpRelativePointerManagerV1,
    inhibit_mgr: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    outputs: Outputs,
    seat: Seat,
    pointer: Option<WlPointer>,
    relative: Option<ZwpRelativePointerV1>,
    keyboard: Option<WlKeyboard>,
    edges: Vec<Edge>,
    capture: Option<Capture>,
    /// `axis_discrete` comes before the `axis` event it belongs to.
    discrete: [Option<i32>; 2],
    quit: bool,
}

pub fn spawn(
    cfg: EdgeConfig,
    sink: EventSink,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<(Sender<EdgeCmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = channel::channel::<EdgeCmd>();
    let join = std::thread::Builder::new()
        .name("hypr-edge".into())
        .spawn(move || {
            if let Err(e) = run(cfg, sink, rx, ready_tx.clone()) {
                let _ = ready_tx.send(Err(e));
            }
        })?;
    Ok((tx, join))
}

fn run(
    cfg: EdgeConfig,
    sink: EventSink,
    rx: channel::Channel<EdgeCmd>,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<()> {
    let instance = cfg.target.instance()?;
    let monitors = instance.monitors()?;
    let mut edges = Vec::new();
    for &side in &cfg.sides {
        let (output, rect) = screen_on(&monitors, side).ok_or(Error::NoScreen(side))?;
        edges.push(Edge {
            side,
            output,
            rect,
            armed: false,
            mapped: None,
        });
    }
    let (conn, globals, mut queue) = hypr_wl::init::<State>(&cfg.target)?;
    let qh = queue.handle();
    let missing = hypr_wl::Error::MissingGlobal;
    let mut state = State {
        sink,
        conn: conn.clone(),
        qh: qh.clone(),
        instance,
        compositor: globals
            .bind(&qh, 4..=6, ())
            .map_err(|_| missing("wl_compositor"))?,
        shm: globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| missing("wl_shm"))?,
        layer_shell: globals
            .bind(&qh, 3..=4, ())
            .map_err(|_| missing("zwlr_layer_shell_v1"))?,
        constraints: globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| missing("zwp_pointer_constraints_v1"))?,
        relative_mgr: globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| missing("zwp_relative_pointer_manager_v1"))?,
        inhibit_mgr: globals.bind(&qh, 1..=1, ()).ok(),
        outputs: Outputs::bind(&globals, &qh)?,
        seat: Seat::bind(&globals, &qh)?,
        pointer: None,
        relative: None,
        keyboard: None,
        edges,
        capture: None,
        discrete: [None; 2],
        quit: false,
    };
    queue.roundtrip(&mut state).map_err(hypr_wl::Error::from)?;
    queue.roundtrip(&mut state).map_err(hypr_wl::Error::from)?;
    for edge in &state.edges {
        if state.outputs.get(&edge.output).is_none() {
            return Err(hypr_wl::Error::NoOutput(edge.output.clone()).into());
        }
        tracing::info!(side = edge.side.name(), output = %edge.output, rect = ?edge.rect, "edge");
    }
    let _ = ready_tx.send(Ok(()));
    let result = hypr_wl::run_loop(conn, queue, rx, &mut state);
    state.end_capture();
    for i in 0..state.edges.len() {
        state.unmap(i);
    }
    let _ = state.conn.flush();
    Ok(result?)
}

impl State {
    fn emit(&mut self, event: EdgeEvent) {
        (self.sink)(event);
    }

    fn map(&mut self, i: usize) {
        if self.edges[i].mapped.is_some() {
            return;
        }
        let Some((output, _)) = self.outputs.get(&self.edges[i].output).cloned() else {
            tracing::warn!(output = %self.edges[i].output, "the edge's screen is gone");
            return;
        };
        let side = self.edges[i].side;
        let surface = self.compositor.create_surface(&self.qh, ());
        let layer = self.layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            Layer::Overlay,
            NAMESPACE.into(),
            &self.qh,
            i,
        );
        let (anchor, size) = match side {
            Side::Left => (Anchor::Top | Anchor::Bottom | Anchor::Left, (1, 0)),
            Side::Right => (Anchor::Top | Anchor::Bottom | Anchor::Right, (1, 0)),
            Side::Above => (Anchor::Left | Anchor::Right | Anchor::Top, (0, 1)),
            Side::Below => (Anchor::Left | Anchor::Right | Anchor::Bottom, (0, 1)),
        };
        layer.set_anchor(anchor);
        layer.set_size(size.0, size.1);
        // Cover bars too: the edge is the whole side of the screen.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        surface.commit();
        self.edges[i].mapped = Some(Mapped {
            surface,
            layer,
            size: (0, 0),
            buffer: None,
        });
    }

    fn unmap(&mut self, i: usize) {
        if self.capture.as_ref().is_some_and(|c| c.edge == i) {
            self.end_capture();
            self.emit(EdgeEvent::Lost);
        }
        if let Some(m) = self.edges[i].mapped.take() {
            m.layer.destroy();
            m.surface.destroy();
            if let Some(b) = m.buffer {
                b.destroy();
            }
        }
    }

    /// Give a configured edge a transparent buffer of its size.
    fn configure(&mut self, i: usize, width: u32, height: u32) -> Result<()> {
        let Some(m) = self.edges[i].mapped.as_mut() else {
            return Ok(());
        };
        let (w, h) = (width.max(1), height.max(1));
        if m.size != (w, h) || m.buffer.is_none() {
            let len = (w * h * 4) as usize;
            let fd = memfd_create(c"gliff-edge", MFdFlags::MFD_CLOEXEC)
                .map_err(|e| Error::Io(e.into()))?;
            let file = File::from(fd);
            file.set_len(len as u64)?;
            let pool = self.shm.create_pool(file.as_fd(), len as i32, &self.qh, ());
            let buffer = pool.create_buffer(
                0,
                w as i32,
                h as i32,
                (w * 4) as i32,
                wl_shm::Format::Argb8888,
                &self.qh,
                (),
            );
            pool.destroy();
            if let Some(old) = m.buffer.replace(buffer) {
                old.destroy();
            }
            m.size = (w, h);
        }
        m.surface.attach(m.buffer.as_ref(), 0, 0);
        m.surface.damage_buffer(0, 0, w as i32, h as i32);
        m.surface.commit();
        Ok(())
    }

    fn edge_of(&self, surface: &WlSurface) -> Option<usize> {
        self.edges
            .iter()
            .position(|e| e.mapped.as_ref().is_some_and(|m| &m.surface == surface))
    }

    fn start_capture(&mut self, i: usize, serial: u32, x: f64, y: f64) {
        let (Some(pointer), Some(m)) = (self.pointer.clone(), self.edges[i].mapped.as_ref()) else {
            return;
        };
        pointer.set_cursor(serial, None, 0, 0);
        let lock = self.constraints.lock_pointer(
            &m.surface,
            &pointer,
            None,
            Lifetime::Oneshot,
            &self.qh,
            (),
        );
        m.layer
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        m.surface.commit();
        let inhibitor = match (&self.inhibit_mgr, &self.seat.seat) {
            (Some(mgr), Some(seat)) => Some(mgr.inhibit_shortcuts(&m.surface, seat, &self.qh, ())),
            _ => None,
        };
        let side = self.edges[i].side;
        let (w, h) = m.size;
        let along = if side.is_vertical() {
            y / f64::from(h.max(1))
        } else {
            x / f64::from(w.max(1))
        };
        self.capture = Some(Capture {
            edge: i,
            lock,
            inhibitor,
        });
        tracing::debug!(side = side.name(), along, "captured");
        self.emit(EdgeEvent::Entered {
            side,
            along: along.clamp(0.0, 1.0),
        });
    }

    /// Undo a capture's lock, shortcut inhibitor and keyboard focus.
    /// Returns the edge it was on.
    fn end_capture(&mut self) -> Option<usize> {
        let c = self.capture.take()?;
        c.lock.destroy();
        if let Some(inhibitor) = c.inhibitor {
            inhibitor.destroy();
        }
        if let Some(m) = self.edges[c.edge].mapped.as_ref() {
            m.layer
                .set_keyboard_interactivity(KeyboardInteractivity::None);
            m.surface.commit();
        }
        let _ = self.conn.flush();
        Some(c.edge)
    }

    /// The compositor took the pointer back. An edge kept on its screen
    /// while it held the pointer moves now, as after a release.
    fn lose_capture(&mut self) {
        if self.end_capture().is_some() {
            self.emit(EdgeEvent::Lost);
            self.refresh_edges();
        }
    }

    fn release(&mut self, along: f64) {
        let Some(i) = self.end_capture() else {
            return;
        };
        // Screens may have moved or changed scale while the pointer was
        // away, and an edge kept while it was captured moves now.
        self.refresh_edges();
        let side = self.edges[i].side;
        let (x, y) = release_point(self.edges[i].rect, side, along);
        if let Err(e) = self.instance.move_cursor(x, y) {
            tracing::warn!(error = %e, "could not put the pointer back");
        }
    }
}

impl State {
    /// Follow the layout: each edge runs along the screen now outermost on
    /// its side, with that screen's rectangle. An armed edge whose screen
    /// changed is put on the new one, unless it holds the pointer; it moves
    /// once the pointer is given back. When the screens cannot be read, the
    /// edges stay as they were.
    fn refresh_edges(&mut self) {
        let monitors = match self.instance.monitors() {
            Ok(monitors) => monitors,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the screens again");
                return;
            }
        };
        for i in 0..self.edges.len() {
            let side = self.edges[i].side;
            let Some((output, rect)) = screen_on(&monitors, side) else {
                continue;
            };
            tracing::debug!(side = side.name(), %output, ?rect, "outermost screen");
            self.edges[i].rect = rect;
            if self.capture.as_ref().is_some_and(|c| c.edge == i) {
                continue;
            }
            // A screen Wayland has not finished announcing is taken once it
            // has: its own burst ends with another refresh.
            if output != self.edges[i].output
                && self.outputs.get(&output).is_some_and(|(_, info)| info.done)
            {
                tracing::info!(side = side.name(), %output, "the edge moves to another screen");
                self.edges[i].output = output;
                self.unmap(i);
            }
            if self.edges[i].armed {
                self.map(i);
            }
        }
    }
}

impl LoopState for State {
    type Cmd = EdgeCmd;

    fn on_cmd(&mut self, cmd: EdgeCmd) {
        match cmd {
            EdgeCmd::Arm { side, armed } => {
                for i in 0..self.edges.len() {
                    if self.edges[i].side == side {
                        self.edges[i].armed = armed;
                        if armed {
                            self.map(i);
                        } else {
                            self.unmap(i);
                        }
                    }
                }
            }
            EdgeCmd::Release { along } => self.release(along),
            EdgeCmd::Stop => self.quit = true,
        }
        let _ = self.conn.flush();
    }

    fn stop(&mut self) {
        self.quit = true;
    }

    fn stop_requested(&self) -> bool {
        self.quit
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // A screen plugged in is followed once its first burst is done; one
        // unplugged, at once.
        match event {
            wl_registry::Event::Global {
                name, interface, ..
            } if interface == WlOutput::interface().name => {
                tracing::debug!(name, "a screen was plugged in");
                state.outputs.add(registry, name, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                let was_output = state.outputs.remove(name);
                tracing::debug!(name, was_output, "a global went away");
                if was_output {
                    state.refresh_edges();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if state.outputs.handle(output, event) {
            state.refresh_edges();
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        state.seat.handle(event);
        if state.seat.has_pointer && state.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            state.relative = Some(state.relative_mgr.get_relative_pointer(&pointer, qh, ()));
            state.pointer = Some(pointer);
        }
        if state.seat.has_keyboard && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let captured = state.capture.is_some();
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                if let (false, Some(i)) = (captured, state.edge_of(&surface)) {
                    state.start_capture(i, serial, surface_x, surface_y);
                }
            }
            wl_pointer::Event::Leave { .. } if captured => state.lose_capture(),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(pressed),
                ..
            } if captured => state.emit(EdgeEvent::Button {
                button,
                pressed: pressed == wl_pointer::ButtonState::Pressed,
            }),
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(axis),
                discrete,
            } if captured => state.discrete[axis_index(axis)] = Some(discrete),
            wl_pointer::Event::Axis {
                axis: WEnum::Value(axis),
                value,
                ..
            } if captured => {
                let discrete = state.discrete[axis_index(axis)].take();
                state.emit(EdgeEvent::Axis {
                    axis: our_axis(axis),
                    value,
                    discrete,
                    stop: false,
                });
            }
            wl_pointer::Event::AxisStop {
                axis: WEnum::Value(axis),
                ..
            } if captured => state.emit(EdgeEvent::Axis {
                axis: our_axis(axis),
                value: 0.0,
                discrete: None,
                stop: true,
            }),
            _ => {}
        }
    }
}

fn axis_index(axis: wl_pointer::Axis) -> usize {
    match axis {
        wl_pointer::Axis::HorizontalScroll => 1,
        _ => 0,
    }
}

fn our_axis(axis: wl_pointer::Axis) -> Axis {
    match axis {
        wl_pointer::Axis::HorizontalScroll => Axis::Horizontal,
        _ => Axis::Vertical,
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                // The fd may be shared with other clients, so read at an
                // offset rather than move its position.
                let mut text = vec![0; size as usize];
                match File::from(fd).read_exact_at(&mut text, 0) {
                    Ok(()) => {
                        while text.last() == Some(&0) {
                            text.pop();
                        }
                        match String::from_utf8(text) {
                            Ok(text) => state.emit(EdgeEvent::Keymap(text)),
                            Err(_) => tracing::warn!("the keymap is not UTF-8"),
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "could not read the keymap"),
                }
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => state.emit(EdgeEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            }),
            wl_keyboard::Event::Enter { .. } => tracing::debug!("keyboard focus here"),
            wl_keyboard::Event::Leave { .. } => tracing::debug!("keyboard focus gone"),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(pressed),
                ..
            } if state.capture.is_some() => state.emit(EdgeEvent::Key {
                code: key,
                pressed: pressed == wl_keyboard::KeyState::Pressed,
            }),
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            if state.capture.is_some() {
                state.emit(EdgeEvent::Motion { dx, dy });
            }
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        lock: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let ours = state.capture.as_ref().is_some_and(|c| &c.lock == lock);
        match (event, ours) {
            (zwp_locked_pointer_v1::Event::Locked, true) => tracing::debug!("pointer locked"),
            (zwp_locked_pointer_v1::Event::Unlocked, true) => {
                tracing::debug!("the compositor ended the pointer lock");
                state.lose_capture();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // An edge given a new surface may still hear about its old one.
        let current = state.edges[i]
            .mapped
            .as_ref()
            .is_some_and(|m| m.layer == *layer);
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } if !current => {
                layer.ack_configure(serial);
            }
            zwlr_layer_surface_v1::Event::Closed if !current => {}
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if let Err(e) = state.configure(i, width, height) {
                    state.emit(EdgeEvent::Error(e.to_string()));
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.unmap(i),
            _ => {}
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwpPointerConstraintsV1);
delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
