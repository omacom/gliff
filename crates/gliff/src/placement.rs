//! The output the window is on, so a server can tell when a connection from
//! this window would show its own screen inside itself.

use std::time::Duration;

use gtk::gdk;
use gtk::prelude::*;
use gtk4 as gtk;
use tokio::sync::watch;

/// The connector name of the window's output (`DP-1`, `gliff-4242`), `None`
/// until the window has been shown.
pub type Placement = watch::Receiver<Option<String>>;

/// How long a new session waits for the window to be placed (a window that is
/// still opening) before it connects without saying.
pub const FIRST_PLACEMENT_WAIT: Duration = Duration::from_secs(1);

/// Follow `window` from the moment it is shown.
pub fn follow(window: &impl IsA<gtk::Native>) -> Placement {
    let (tx, rx) = watch::channel(None);
    let native = window.as_ref();
    let watch_surface = move |native: &gtk::Native| {
        let Some(surface) = native.surface() else {
            return;
        };
        let report = {
            let tx = tx.clone();
            move |monitor: &gdk::Monitor| {
                tracing::debug!(connector = ?monitor.connector(), "window output changed");
                tx.send_replace(monitor.connector().map(Into::into));
            }
        };
        let settle = {
            let report = report.clone();
            let surface = surface.clone();
            move || {
                if let Some(monitor) =
                    gdk::Display::default().and_then(|d| d.monitor_at_surface(&surface))
                {
                    report(&monitor);
                }
            }
        };
        settle();
        surface.connect_enter_monitor(move |_, monitor| report(monitor));
        // Leaving a monitor the window overlapped puts it back on the one it
        // still touches, which an enter alone would not report.
        surface.connect_leave_monitor(move |_, _| settle());
    };
    match native.surface() {
        Some(_) => watch_surface(native),
        None => {
            let widget = native.upcast_ref::<gtk::Widget>();
            let once = std::cell::Cell::new(Some(watch_surface));
            widget.connect_realize(move |w| {
                if let (Some(f), Some(native)) = (once.take(), w.dynamic_cast_ref::<gtk::Native>())
                {
                    f(native);
                }
            });
        }
    }
    rx
}
