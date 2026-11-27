//! XDG desktop portal client for the optional global shortcut (GlobalShortcuts).
//! Its `Activated` signal carries an activation token, which lets the window take
//! keyboard focus on Wayland. GNOME asks the user to confirm the shortcut once.

use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ObjectPath;
use gtk::glib::Variant;
use gtk::prelude::ToVariant;

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const GLOBAL_SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";

type Options = HashMap<String, Variant>;

/// Tells the portal which app we are. Must run before GTK talks to any portal.
pub fn register_host_app(app_id: &str) {
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return };
    let args = (app_id, Options::new()).to_variant();
    let result = conn.call_sync(
        Some(DEST),
        PATH,
        "org.freedesktop.host.portal.Registry",
        "Register",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    );
    if let Err(e) = result {
        crate::debug!("portal app id registration: {e}");
    }
}

fn token() -> String {
    format!("clipnest{}", glib::random_int())
}

fn opt<T: ToVariant>(value: T) -> Variant {
    value.to_variant()
}

async fn interface_version(conn: &gio::DBusConnection, iface: &str) -> Option<u32> {
    let reply = conn
        .call_future(
            Some(DEST),
            PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(iface, "version").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            3000,
        )
        .await
        .ok()?;
    let (value,): (Variant,) = reply.get()?;
    if value.type_().is_variant() {
        value.as_variant().and_then(|v| v.get::<u32>())
    } else {
        value.get::<u32>()
    }
}

/// Calls a portal method that answers through a `Request` object and waits for its `Response`.
async fn request(
    conn: &gio::DBusConnection,
    iface: &str,
    method: &str,
    args: impl FnOnce(&str) -> Variant,
) -> Result<Options, String> {
    let handle_token = token();
    let sender = conn
        .unique_name()
        .ok_or("no unique bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let request_path = format!("{PATH}/request/{sender}/{handle_token}");
    let (tx, rx) = async_channel::bounded::<Variant>(1);
    // Subscribe before calling so the response cannot be missed.
    #[allow(deprecated)]
    let sub = conn.signal_subscribe(
        Some(DEST),
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&request_path),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            let _ = tx.try_send(params.clone());
        },
    );
    let result = async {
        conn.call_future(Some(DEST), PATH, iface, method, Some(&args(&handle_token)), None, gio::DBusCallFlags::NONE, 30_000)
            .await
            .map_err(|e| e.to_string())?;
        let response = rx.recv().await.map_err(|e| e.to_string())?;
        let (code, results): (u32, Options) = response.get().ok_or("malformed portal response")?;
        match code {
            0 => Ok(results),
            1 => Err("cancelled by the user".to_owned()),
            _ => Err(format!("{method} failed")),
        }
    }
    .await;
    #[allow(deprecated)]
    conn.signal_unsubscribe(sub);
    result
}

fn object_path(s: &str) -> Result<ObjectPath, String> {
    ObjectPath::try_from(s.to_owned()).map_err(|_| format!("invalid object path {s}"))
}

/// `<Shift><Super>c` → `SHIFT+LOGO+c` (the portal's shortcut syntax).
pub fn accel_to_portal_trigger(accel: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut rest = accel;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else { break };
        let modifier = rest[start + 1..start + end].to_ascii_lowercase();
        parts.push(
            match modifier.as_str() {
                "control" | "ctrl" | "primary" => "CTRL",
                "alt" | "mod1" => "ALT",
                "shift" => "SHIFT",
                "super" | "meta" | "mod4" | "hyper" => "LOGO",
                other => return other.to_owned(),
            }
            .to_owned(),
        );
        rest = &rest[start + end + 1..];
    }
    parts.push(rest.to_owned());
    parts.join("+")
}

pub struct GlobalShortcuts {
    conn: gio::DBusConnection,
    session: String,
    #[allow(dead_code)]
    subscription: gio::SignalSubscriptionId,
}

pub struct BoundShortcut {
    pub shortcuts: GlobalShortcuts,
    /// Human-readable trigger, e.g. "Press <Shift><Super>c".
    pub trigger: Option<String>,
}

impl GlobalShortcuts {
    /// Creates a session and binds the "open" shortcut. GNOME shows a confirmation
    /// dialog the first time; afterwards the binding is restored silently.
    pub async fn bind(
        preferred_trigger: &str,
        on_activated: impl Fn(Option<String>) + 'static,
        on_changed: impl Fn(Option<String>) + 'static,
    ) -> Result<BoundShortcut, String> {
        let conn = gio::bus_get_future(gio::BusType::Session).await.map_err(|e| e.to_string())?;
        if interface_version(&conn, GLOBAL_SHORTCUTS).await.is_none() {
            return Err("the GlobalShortcuts portal is not available".into());
        }
        let session_token = token();
        let results = request(&conn, GLOBAL_SHORTCUTS, "CreateSession", |handle| {
            let mut o = Options::new();
            o.insert("handle_token".into(), opt(handle));
            o.insert("session_handle_token".into(), opt(session_token.as_str()));
            (o,).to_variant()
        })
        .await?;
        let session: String = results
            .get("session_handle")
            .and_then(|v| v.get::<String>().or_else(|| v.get::<ObjectPath>().map(|p| p.as_str().to_owned())))
            .ok_or("no session handle")?;

        let on_activated = Rc::new(on_activated);
        let on_changed = Rc::new(on_changed);
        let session_for_signal = session.clone();
        #[allow(deprecated)]
        let subscription = conn.signal_subscribe(
            Some(DEST),
            Some(GLOBAL_SHORTCUTS),
            None,
            Some(PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, signal, params| match signal {
                "Activated" => {
                    if let Some((path, id, _ts, options)) = params.get::<(ObjectPath, String, u64, Options)>() {
                        if path.as_str() == session_for_signal && id == "toggle" {
                            let token = options.get("activation_token").and_then(|v| v.get::<String>());
                            on_activated(token);
                        }
                    }
                }
                "ShortcutsChanged" => {
                    if let Some((path, shortcuts)) = params.get::<(ObjectPath, Vec<(String, Options)>)>() {
                        if path.as_str() == session_for_signal {
                            on_changed(trigger_of(&shortcuts));
                        }
                    }
                }
                _ => {}
            },
        );

        let session_path = object_path(&session)?;
        let trigger = accel_to_portal_trigger(preferred_trigger);
        let results = request(&conn, GLOBAL_SHORTCUTS, "BindShortcuts", |handle| {
            let mut props = Options::new();
            props.insert("description".into(), opt("Open clipboard history"));
            props.insert("preferred_trigger".into(), opt(trigger.as_str()));
            let shortcuts = vec![("toggle".to_owned(), props)];
            let mut o = Options::new();
            o.insert("handle_token".into(), opt(handle));
            (session_path.clone(), shortcuts, "", o).to_variant()
        })
        .await?;
        let trigger = results
            .get("shortcuts")
            .and_then(|v| v.get::<Vec<(String, Options)>>())
            .and_then(|s| trigger_of(&s));
        Ok(BoundShortcut { shortcuts: GlobalShortcuts { conn, session, subscription }, trigger })
    }

    /// Connection and session handle, for calls that must not hold a borrow across `.await`.
    pub fn handle(&self) -> (gio::DBusConnection, String) {
        (self.conn.clone(), self.session.clone())
    }
}

/// Opens the desktop's UI for changing the shortcut (portal version 2+).
pub async fn configure_shortcuts(conn: &gio::DBusConnection, session: &str) -> Result<(), String> {
    {
        let session_path = object_path(session)?;
        conn
            .call_future(
                Some(DEST),
                PATH,
                GLOBAL_SHORTCUTS,
                "ConfigureShortcuts",
                Some(&(session_path, "", Options::new()).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                10_000,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn trigger_of(shortcuts: &[(String, Options)]) -> Option<String> {
    shortcuts
        .iter()
        .find(|(id, _)| id == "toggle")
        .and_then(|(_, props)| props.get("trigger_description"))
        .and_then(|v| v.get::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_accelerators() {
        assert_eq!(accel_to_portal_trigger("<Shift><Super>c"), "SHIFT+LOGO+c");
        assert_eq!(accel_to_portal_trigger("<Control><Alt>v"), "CTRL+ALT+v");
        assert_eq!(accel_to_portal_trigger("F12"), "F12");
    }
}
