//! Notifications through org.freedesktop.Notifications.
//!
//! "Copied" notifications are transient (they don't pile up in the notification
//! list). Each notification replaces the previous one, so rapid copies don't flood
//! the screen. Warnings stay until dismissed and can offer actions.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::ToVariant;

use crate::config;

/// Called with the action key ("default" when the notification itself was
/// clicked) and the activation token the shell sent, if any.
pub type ActionHandler = Rc<dyn Fn(&str, Option<String>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Urgency {
    Normal = 1,
    Critical = 2,
}

pub struct Notice<'a> {
    pub summary: &'a str,
    pub body: &'a str,
    pub urgency: Urgency,
    pub transient: bool,
    /// (key, label) pairs.
    pub actions: &'a [(&'a str, &'a str)],
}

#[derive(Default)]
struct State {
    last_id: Cell<u32>,
    handlers: RefCell<HashMap<u32, ActionHandler>>,
    tokens: RefCell<HashMap<u32, String>>,
}

#[derive(Default)]
pub struct Notifier {
    conn: RefCell<Option<gio::DBusConnection>>,
    state: Rc<State>,
    subscription: RefCell<Option<gio::SignalSubscriptionId>>,
}

impl Notifier {
    fn connection(&self) -> Option<gio::DBusConnection> {
        if self.conn.borrow().is_none() {
            *self.conn.borrow_mut() = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok();
        }
        let conn = self.conn.borrow().clone()?;
        if self.subscription.borrow().is_none() {
            let state = Rc::downgrade(&self.state);
            #[allow(deprecated)]
            let id = conn.signal_subscribe(
                None,
                Some("org.freedesktop.Notifications"),
                None,
                Some("/org/freedesktop/Notifications"),
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, _, _, signal, params| {
                    let Some(state) = state.upgrade() else { return };
                    match signal {
                        // Sent just before ActionInvoked, so the window can take focus.
                        "ActivationToken" => {
                            if let Some((id, token)) = params.get::<(u32, String)>() {
                                state.tokens.borrow_mut().insert(id, token);
                            }
                        }
                        "ActionInvoked" => {
                            if let Some((id, action)) = params.get::<(u32, String)>() {
                                crate::debug!("notification {id}: action {action}");
                                let handler = state.handlers.borrow().get(&id).cloned();
                                let token = state.tokens.borrow_mut().remove(&id);
                                if let Some(handler) = handler {
                                    handler(&action, token);
                                }
                            }
                        }
                        "NotificationClosed" => {
                            if let Some((id, _reason)) = params.get::<(u32, u32)>() {
                                state.handlers.borrow_mut().remove(&id);
                                state.tokens.borrow_mut().remove(&id);
                            }
                        }
                        _ => {}
                    }
                },
            );
            *self.subscription.borrow_mut() = Some(id);
        }
        Some(conn)
    }

    /// A short "copied" notification.
    pub fn notify(&self, summary: &str, body: &str) {
        self.show(Notice { summary, body, urgency: Urgency::Normal, transient: true, actions: &[] }, None);
    }

    pub fn show(&self, notice: Notice, on_action: Option<ActionHandler>) {
        let Some(conn) = self.connection() else { return };
        let mut hints: HashMap<String, Variant> = HashMap::new();
        if notice.transient {
            hints.insert("transient".into(), true.to_variant());
            hints.insert("category".into(), "transfer.complete".to_variant());
        }
        hints.insert("desktop-entry".into(), config::APP_ID.to_variant());
        hints.insert("urgency".into(), (notice.urgency as u8).to_variant());
        let actions: Vec<String> =
            notice.actions.iter().flat_map(|(key, label)| [key.to_string(), label.to_string()]).collect();
        let timeout = if notice.urgency == Urgency::Critical { 0i32 } else { 3000i32 };
        let args = (
            config::APP_NAME,
            self.state.last_id.get(),
            config::ICON_NAME,
            notice.summary,
            glib::markup_escape_text(notice.body).as_str(),
            actions,
            hints,
            timeout,
        )
            .to_variant();
        let state = self.state.clone();
        conn.call(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            Some(&args),
            Some(glib::VariantTy::new("(u)").expect("valid type")),
            gio::DBusCallFlags::NONE,
            3000,
            gio::Cancellable::NONE,
            move |result| match result {
                Ok(reply) => {
                    if let Some((id,)) = reply.get::<(u32,)>() {
                        crate::debug!("notification {id} shown");
                        state.last_id.set(id);
                        // The id may be reused for a replacement: drop the old handler.
                        match on_action {
                            Some(handler) => state.handlers.borrow_mut().insert(id, handler),
                            None => state.handlers.borrow_mut().remove(&id),
                        };
                    }
                }
                Err(e) => crate::debug!("notification failed: {e}"),
            },
        );
    }
}
