//! The badge at the start of the address bar: the protocol the typed
//! machine is reached by. It says SSH, gliff's own, until another is picked
//! from its menu, and follows a scheme typed by hand, so it always shows what
//! Connect will use.

use std::cell::Cell;
use std::rc::Rc;

use gliff_foreign::Kind;
use gtk4::prelude::*;
use gtk4::{self as gtk, gio, glib};

/// How a machine in the address bar is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Ssh,
    Foreign(Kind),
}

const ALL: [Protocol; 3] = [
    Protocol::Ssh,
    Protocol::Foreign(Kind::Rdp),
    Protocol::Foreign(Kind::RustDesk),
];

impl Protocol {
    fn id(self) -> &'static str {
        match self {
            Protocol::Ssh => "ssh",
            Protocol::Foreign(k) => k.scheme(),
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        ALL.into_iter().find(|p| p.id() == id)
    }

    /// The badge's text.
    fn short(self) -> &'static str {
        match self {
            Protocol::Ssh => "SSH",
            Protocol::Foreign(Kind::Rdp) => "RDP",
            Protocol::Foreign(Kind::RustDesk) => "RustDesk",
        }
    }

    /// The menu's text.
    fn long(self) -> &'static str {
        match self {
            Protocol::Ssh => "SSH (gliff)",
            Protocol::Foreign(k) => k.label(),
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Protocol::Foreign(Kind::RustDesk) => "host",
            _ => "user@host",
        }
    }
}

/// The protocol named by `text`'s scheme, and the rest of it; `None` when
/// it has none gliff knows.
fn split_scheme(text: &str) -> Option<(Protocol, &str)> {
    ALL.into_iter().find_map(|p| {
        let Protocol::Foreign(k) = p else {
            return None;
        };
        let rest = text.strip_prefix(k.scheme())?.strip_prefix("://")?;
        Some((p, rest))
    })
}

/// The machine to open for `text` typed with `protocol` picked: as typed
/// when it names its own scheme, otherwise with the picked one's.
pub fn address(protocol: Protocol, text: &str) -> String {
    let text = text.trim();
    match protocol {
        _ if split_scheme(text).is_some() => text.to_string(),
        Protocol::Ssh => text.to_string(),
        Protocol::Foreign(k) => with_scheme(k, text),
    }
}

/// `rest` under `kind`'s scheme; RustDesk takes no user, so one typed for
/// ssh or RDP is dropped.
fn with_scheme(kind: Kind, rest: &str) -> String {
    let rest = match kind.takes_user() {
        true => rest,
        false => rest.rsplit_once('@').map_or(rest, |(_, host)| host),
    };
    format!("{}://{rest}", kind.scheme())
}

/// `text` rewritten for `protocol`: its scheme, if any, swapped for the
/// new one's, or dropped for ssh.
fn rewrite(protocol: Protocol, text: &str) -> Option<String> {
    let (_, rest) = split_scheme(text)?;
    Some(match protocol {
        Protocol::Ssh => rest.to_string(),
        Protocol::Foreign(k) => with_scheme(k, rest),
    })
}

pub struct ProtocolBadge {
    button: gtk::MenuButton,
    action: gio::SimpleAction,
    entry: gtk::Entry,
    chosen: Cell<Protocol>,
}

impl ProtocolBadge {
    /// A badge for `entry`; its menu's action lives in `actions_on`, which
    /// must hold the badge.
    pub fn new(entry: &gtk::Entry, actions_on: &impl IsA<gtk::Widget>) -> Rc<Self> {
        let menu = gio::Menu::new();
        for p in ALL {
            menu.append(
                Some(p.long()),
                Some(&format!("address.protocol::{}", p.id())),
            );
        }
        let button = gtk::MenuButton::builder()
            .label(Protocol::Ssh.short())
            .tooltip_text("Protocol")
            .menu_model(&menu)
            .build();
        let action = gio::SimpleAction::new_stateful(
            "protocol",
            Some(glib::VariantTy::STRING),
            &Protocol::Ssh.id().to_variant(),
        );
        let group = gio::SimpleActionGroup::new();
        group.add_action(&action);
        actions_on.insert_action_group("address", Some(&group));
        let badge = Rc::new(Self {
            button,
            action: action.clone(),
            entry: entry.clone(),
            chosen: Cell::new(Protocol::Ssh),
        });
        {
            let badge = Rc::downgrade(&badge);
            action.connect_change_state(move |_, value| {
                let (Some(badge), Some(p)) = (
                    badge.upgrade(),
                    value
                        .and_then(|v| v.str().map(str::to_string))
                        .and_then(|id| Protocol::from_id(&id)),
                ) else {
                    return;
                };
                if let Some(text) = rewrite(p, &badge.entry.text()) {
                    badge.entry.set_text(&text);
                    badge.entry.set_position(-1);
                }
                badge.show(p);
                badge.entry.grab_focus();
            });
        }
        {
            let badge = Rc::downgrade(&badge);
            entry.connect_changed(move |e| {
                if let (Some(badge), Some((p, _))) = (badge.upgrade(), split_scheme(&e.text())) {
                    badge.show(p);
                }
            });
        }
        badge
    }

    pub fn widget(&self) -> &gtk::MenuButton {
        &self.button
    }

    /// The machine to open for what the address bar holds.
    pub fn address(&self) -> String {
        address(self.chosen.get(), &self.entry.text())
    }

    /// Back to SSH, for the next machine.
    pub fn reset(&self) {
        self.show(Protocol::Ssh);
    }

    fn show(&self, p: Protocol) {
        self.chosen.set(p);
        self.action.set_state(&p.id().to_variant());
        self.button.set_label(p.short());
        self.entry.set_placeholder_text(Some(p.placeholder()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RDP: Protocol = Protocol::Foreign(Kind::Rdp);
    const RUSTDESK: Protocol = Protocol::Foreign(Kind::RustDesk);

    #[test]
    fn the_picked_protocol_names_the_machine() {
        assert_eq!(address(Protocol::Ssh, " ann@pc "), "ann@pc");
        assert_eq!(address(RDP, "ann@pc"), "rdp://ann@pc");
        assert_eq!(address(RUSTDESK, "10.0.0.5"), "rustdesk://10.0.0.5");
        assert_eq!(address(RUSTDESK, "ann@10.0.0.5"), "rustdesk://10.0.0.5");
    }

    #[test]
    fn a_typed_scheme_wins() {
        assert_eq!(address(Protocol::Ssh, "rdp://ann@pc"), "rdp://ann@pc");
        assert_eq!(address(RDP, "rustdesk://pc"), "rustdesk://pc");
    }

    #[test]
    fn picking_swaps_a_typed_scheme() {
        assert_eq!(
            rewrite(RUSTDESK, "rdp://pc:3390").as_deref(),
            Some("rustdesk://pc:3390")
        );
        assert_eq!(
            rewrite(Protocol::Ssh, "rdp://ann@pc").as_deref(),
            Some("ann@pc")
        );
        assert_eq!(
            rewrite(RUSTDESK, "rdp://ann@pc").as_deref(),
            Some("rustdesk://pc")
        );
        assert_eq!(rewrite(RDP, "ann@pc"), None);
    }

    #[test]
    fn ids_round_trip() {
        for p in ALL {
            assert_eq!(Protocol::from_id(p.id()), Some(p));
        }
        assert_eq!(split_scheme("rdpx://pc"), None);
    }
}
