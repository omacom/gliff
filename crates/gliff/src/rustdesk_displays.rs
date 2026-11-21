//! The remote display picker follows the RustDesk tab, not the local outputs.

use std::rc::Rc;

use gliff_foreign::RemoteDisplay;
use gtk4::prelude::*;

use crate::{clipboard::ToWorker, gtk, App, Session};

#[derive(Default)]
pub struct State {
    displays: Vec<RemoteDisplay>,
    active: Option<i32>,
    requested: Option<i32>,
    shown: Option<i32>,
}

impl State {
    pub fn update(&mut self, displays: Vec<RemoteDisplay>, active: i32, failed: bool) {
        self.displays = displays;
        self.active = Some(active);
        if failed {
            self.requested = None;
        }
    }

    pub fn accepts(&self, display: Option<i32>) -> bool {
        self.requested.is_none_or(|index| display == Some(index))
    }

    pub fn painted(&mut self, display: Option<i32>) -> bool {
        let changed = self.shown != display;
        self.shown = display;
        if self.requested.is_some() && self.requested == display {
            self.requested = None;
            return true;
        }
        changed
    }

    pub fn expect_display(&mut self, index: i32) {
        self.requested = Some(index);
    }

    pub fn name(&self, index: i32) -> Option<&str> {
        self.displays
            .iter()
            .find(|display| display.index == index)
            .map(|display| display.name.as_str())
    }

    pub fn switching(&self) -> bool {
        self.requested.is_some()
    }

    pub fn has_display(&self) -> bool {
        self.shown.is_some()
    }

    pub fn blocks_input(&self) -> bool {
        self.requested.is_some() || self.active.is_some_and(|index| self.shown != Some(index))
    }
}

pub fn refresh(ui: &Rc<App>) {
    let button = &ui.rustdesk_display_button;
    if ui.detached_display.is_some() {
        return;
    }
    let Some(session) = crate::active(ui) else {
        button.set_visible(false);
        return;
    };
    let state = session.rustdesk_displays.borrow();
    if state.displays.is_empty()
        || (state.displays.len() == 1 && state.active == Some(state.displays[0].index))
    {
        button.set_visible(false);
        return;
    }

    // Use the same numbering as the menu, based on the last painted frame.
    let number = state.shown.map(|index| i64::from(index) + 1);
    ui.rustdesk_display_number.set_visible(number.is_some());
    if let Some(number) = number {
        ui.rustdesk_display_number.set_text(&number.to_string());
        button.set_tooltip_text(Some(&format!(
            "Choose RustDesk monitor (showing monitor {number})"
        )));
    } else {
        button.set_tooltip_text(Some("Choose RustDesk monitor"));
    }

    let popover = gtk::Popover::new();
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    for display in &state.displays {
        let selected = state.shown == Some(display.index);
        let clean_name: String = display
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(48)
            .collect();
        let name = if clean_name.is_empty() {
            String::new()
        } else {
            format!(" · {clean_name}")
        };
        let label = format!(
            "Monitor {}{name} · {} × {}{}",
            i64::from(display.index) + 1,
            display.width,
            display.height,
            if selected { " (current)" } else { "" }
        );
        let row = gtk::Button::with_label(&label);
        row.add_css_class("flat");
        row.set_sensitive(!selected);
        let index = display.index;
        let weak_ui = Rc::downgrade(ui);
        let weak_session = Rc::downgrade(&session);
        row.connect_clicked(move |_| {
            if let (Some(ui), Some(session)) = (weak_ui.upgrade(), weak_session.upgrade()) {
                select(&ui, &session, index);
            }
        });
        if !selected {
            row.set_tooltip_text(Some("Right-click to open in a new window"));
            let click = gtk::GestureClick::builder().button(3).build();
            let weak_ui = Rc::downgrade(ui);
            let weak_session = Rc::downgrade(&session);
            click.connect_released(move |gesture, _, _, _| {
                if let (Some(ui), Some(session)) = (weak_ui.upgrade(), weak_session.upgrade()) {
                    if crate::is_active(&ui, &session) {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        ui.rustdesk_display_button.popdown();
                        crate::open_rustdesk_display_window(&ui, &session, index);
                    }
                }
            });
            row.add_controller(click);
        }
        list.append(&row);
    }
    popover.set_child(Some(&list));
    button.set_popover(Some(&popover));
    button.set_sensitive(state.requested.is_none() && state.shown.is_some());
    button.set_visible(true);
}

fn select(ui: &Rc<App>, session: &Rc<Session>, index: i32) {
    if !crate::is_active(ui, session) {
        return;
    }
    {
        let state = session.rustdesk_displays.borrow();
        if state.blocks_input()
            || state.shown == Some(index)
            || !state.displays.iter().any(|display| display.index == index)
        {
            return;
        }
    }
    crate::release_pressed_keys(ui);
    let sent = session
        .input_tx
        .borrow()
        .as_ref()
        .is_some_and(|tx| tx.send(ToWorker::SelectRustDeskDisplay(index)).is_ok());
    if sent {
        session.rustdesk_displays.borrow_mut().requested = Some(index);
        crate::set_status(ui, session, "Switching RustDesk monitor…");
    }
    ui.rustdesk_display_button.popdown();
    refresh(ui);
}

#[cfg(test)]
mod tests {
    use super::State;

    #[test]
    fn old_pictures_cannot_reenable_input_after_a_monitor_choice() {
        let mut state = State {
            active: Some(0),
            shown: Some(0),
            requested: Some(1),
            ..Default::default()
        };
        assert!(!state.accepts(Some(0)));
        assert!(state.blocks_input());

        state.update(Vec::new(), 1, false);
        assert!(state.blocks_input());
        assert!(state.accepts(Some(1)));
        assert!(state.painted(Some(1)));
        assert!(!state.blocks_input());
    }
}
