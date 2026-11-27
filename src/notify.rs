//! "Copied" notifications through org.freedesktop.Notifications.
//!
//! Notifications are transient (they don't pile up in the notification list) and
//! replace the previous one, so rapid copies don't flood the screen.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use gtk::gio;
use gtk::glib::{self, Variant};
use gtk::prelude::ToVariant;

use crate::config;

#[derive(Default)]
pub struct Notifier {
    conn: RefCell<Option<gio::DBusConnection>>,
    last_id: std::rc::Rc<Cell<u32>>,
}

impl Notifier {
    fn connection(&self) -> Option<gio::DBusConnection> {
        if self.conn.borrow().is_none() {
            *self.conn.borrow_mut() = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok();
        }
        self.conn.borrow().clone()
    }

    pub fn notify(&self, summary: &str, body: &str) {
        let Some(conn) = self.connection() else { return };
        let mut hints: HashMap<String, Variant> = HashMap::new();
        hints.insert("transient".into(), true.to_variant());
        hints.insert("desktop-entry".into(), config::APP_ID.to_variant());
        hints.insert("urgency".into(), 1u8.to_variant());
        hints.insert("category".into(), "transfer.complete".to_variant());
        let args = (
            config::APP_NAME,
            self.last_id.get(),
            config::ICON_NAME,
            summary,
            glib::markup_escape_text(body).as_str(),
            Vec::<String>::new(),
            hints,
            3000i32,
        )
            .to_variant();
        let last_id = self.last_id.clone();
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
                        last_id.set(id);
                    }
                }
                Err(e) => crate::debug!("notification failed: {e}"),
            },
        );
    }
}
