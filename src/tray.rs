//! Panel icon via StatusNotifierItem + com.canonical.dbusmenu, implemented on gio.
//!
//! Works with KDE, most desktops, and GNOME with the AppIndicator extension
//! (enabled by default on Ubuntu). That extension hands us an activation token
//! before `Activate`/`SecondaryActivate`, so opening the popup from the icon
//! gets keyboard focus.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gdk_pixbuf::prelude::*;
use gtk::gio;
use gtk::glib;
use gtk::glib::variant::ObjectPath;
use gtk::glib::Variant;
use gtk::prelude::ToVariant;

const SNI_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_IFACE: &str = "com.canonical.dbusmenu";

const SNI_XML: &str = r#"<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="WindowId" type="i" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="IconPixmap" type="a(iiay)" access="read"/>
    <property name="OverlayIconName" type="s" access="read"/>
    <property name="AttentionIconName" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <property name="XAyatanaLabel" type="s" access="read"/>
    <property name="XAyatanaLabelGuide" type="s" access="read"/>
    <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
    <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
    <method name="ProvideXdgActivationToken"><arg name="token" type="s" direction="in"/></method>
    <signal name="NewTitle"/>
    <signal name="NewIcon"/>
    <signal name="NewAttentionIcon"/>
    <signal name="NewOverlayIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus"><arg name="status" type="s"/></signal>
    <signal name="XAyatanaNewLabel"><arg name="label" type="s"/><arg name="guide" type="s"/></signal>
  </interface>
</node>"#;

const MENU_XML: &str = r#"<node>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg type="i" name="parentId" direction="in"/>
      <arg type="i" name="recursionDepth" direction="in"/>
      <arg type="as" name="propertyNames" direction="in"/>
      <arg type="u" name="revision" direction="out"/>
      <arg type="(ia{sv}av)" name="layout" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg type="ai" name="ids" direction="in"/>
      <arg type="as" name="propertyNames" direction="in"/>
      <arg type="a(ia{sv})" name="properties" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg type="i" name="id" direction="in"/>
      <arg type="s" name="name" direction="in"/>
      <arg type="v" name="value" direction="out"/>
    </method>
    <method name="Event">
      <arg type="i" name="id" direction="in"/>
      <arg type="s" name="eventId" direction="in"/>
      <arg type="v" name="data" direction="in"/>
      <arg type="u" name="timestamp" direction="in"/>
    </method>
    <method name="EventGroup">
      <arg type="a(isvu)" name="events" direction="in"/>
      <arg type="ai" name="idErrors" direction="out"/>
    </method>
    <method name="AboutToShow">
      <arg type="i" name="id" direction="in"/>
      <arg type="b" name="needUpdate" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg type="ai" name="ids" direction="in"/>
      <arg type="ai" name="updatesNeeded" direction="out"/>
      <arg type="ai" name="idErrors" direction="out"/>
    </method>
    <signal name="ItemsPropertiesUpdated">
      <arg type="a(ia{sv})" name="updatedProps"/>
      <arg type="a(ias)" name="removedProps"/>
    </signal>
    <signal name="LayoutUpdated">
      <arg type="u" name="revision"/>
      <arg type="i" name="parent"/>
    </signal>
    <signal name="ItemActivationRequested">
      <arg type="i" name="id"/>
      <arg type="u" name="timestamp"/>
    </signal>
  </interface>
</node>"#;

pub enum MenuEntry {
    Item {
        label: String,
        enabled: bool,
        checked: Option<bool>,
        action: Rc<dyn Fn()>,
        /// Stable id source (a history item id) so hosts never mix up cached rows.
        key: Option<i64>,
        /// PNG shown next to the label.
        icon: Option<Rc<Vec<u8>>>,
    },
    Separator,
}

impl MenuEntry {
    fn new(label: impl Into<String>, enabled: bool, checked: Option<bool>, action: Rc<dyn Fn()>) -> MenuEntry {
        MenuEntry::Item { label: label.into(), enabled, checked, action, key: None, icon: None }
    }
    pub fn item(label: impl Into<String>, action: impl Fn() + 'static) -> MenuEntry {
        Self::new(label, true, None, Rc::new(action))
    }
    pub fn check(label: impl Into<String>, checked: bool, action: impl Fn() + 'static) -> MenuEntry {
        Self::new(label, true, Some(checked), Rc::new(action))
    }
    pub fn disabled(label: impl Into<String>) -> MenuEntry {
        Self::new(label, false, None, Rc::new(|| {}))
    }
    pub fn with_key(mut self, k: i64) -> MenuEntry {
        if let MenuEntry::Item { key, .. } = &mut self {
            *key = Some(k);
        }
        self
    }
    pub fn with_icon(mut self, png: Option<Rc<Vec<u8>>>) -> MenuEntry {
        if let MenuEntry::Item { icon, .. } = &mut self {
            *icon = png;
        }
        self
    }
    /// dbusmenu id: history items get ids derived from their key, the rest their position.
    fn id(&self, index: usize) -> i32 {
        match self {
            MenuEntry::Item { key: Some(k), .. } => 100_000 + k.rem_euclid(1_000_000_000) as i32,
            _ => index as i32 + 1,
        }
    }
}

struct Inner {
    conn: gio::DBusConnection,
    icon_name: RefCell<String>,
    icon_pixmaps: RefCell<Vec<(i32, i32, Vec<u8>)>>,
    icon_theme_path: String,
    label: RefCell<String>,
    tooltip: RefCell<String>,
    menu: RefCell<Vec<MenuEntry>>,
    revision: Cell<u32>,
    activation_token: RefCell<Option<String>>,
    on_activate: Box<dyn Fn(Option<String>)>,
    registrations: RefCell<Vec<gio::RegistrationId>>,
    owner_id: RefCell<Option<gio::OwnerId>>,
    watcher_id: RefCell<Option<gio::WatcherId>>,
    bus_name: String,
}

pub struct Tray {
    inner: Rc<Inner>,
}

fn escape_label(label: &str) -> String {
    // dbusmenu uses '_' for mnemonics.
    label.replace('_', "__")
}

impl Tray {
    pub fn start(
        icon_name: &str,
        icon_svg: Option<&str>,
        icon_theme_path: String,
        on_activate: impl Fn(Option<String>) + 'static,
    ) -> Result<Tray, String> {
        let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(|e| e.to_string())?;
        let bus_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let inner = Rc::new(Inner {
            conn: conn.clone(),
            icon_name: RefCell::new(icon_name.to_owned()),
            icon_pixmaps: RefCell::new(icon_svg.map(render_pixmaps).unwrap_or_default()),
            icon_theme_path,
            label: RefCell::new(String::new()),
            tooltip: RefCell::new("Clipboard history".to_owned()),
            menu: RefCell::new(Vec::new()),
            revision: Cell::new(1),
            activation_token: RefCell::new(None),
            on_activate: Box::new(on_activate),
            registrations: RefCell::new(Vec::new()),
            owner_id: RefCell::new(None),
            watcher_id: RefCell::new(None),
            bus_name: bus_name.clone(),
        });

        let sni_info = gio::DBusNodeInfo::for_xml(SNI_XML).map_err(|e| e.to_string())?;
        let sni_iface = sni_info.lookup_interface(SNI_IFACE).ok_or("bad SNI xml")?;
        let weak = Rc::downgrade(&inner);
        let weak2 = Rc::downgrade(&inner);
        let sni_id = conn
            .register_object(SNI_PATH, &sni_iface)
            .method_call(move |_conn, _sender, _path, _iface, method, params, invocation| {
                let Some(inner) = weak.upgrade() else { return };
                match method {
                    "Activate" | "SecondaryActivate" => {
                        let token = inner.activation_token.borrow_mut().take();
                        invocation.return_value(None);
                        let inner = inner.clone();
                        glib::idle_add_local_once(move || (inner.on_activate)(token));
                    }
                    "ProvideXdgActivationToken" => {
                        if let Some((token,)) = params.get::<(String,)>() {
                            *inner.activation_token.borrow_mut() = Some(token);
                        }
                        invocation.return_value(None);
                    }
                    _ => invocation.return_value(None),
                }
            })
            .property(move |_conn, _sender, _path, _iface, prop| {
                let Some(inner) = weak2.upgrade() else { return "".to_variant() };
                inner.sni_property(prop)
            })
            .build()
            .map_err(|e| e.to_string())?;

        let menu_info = gio::DBusNodeInfo::for_xml(MENU_XML).map_err(|e| e.to_string())?;
        let menu_iface = menu_info.lookup_interface(MENU_IFACE).ok_or("bad menu xml")?;
        let weak = Rc::downgrade(&inner);
        let weak2 = Rc::downgrade(&inner);
        let menu_id = conn
            .register_object(MENU_PATH, &menu_iface)
            .method_call(move |_conn, _sender, _path, _iface, method, params, invocation| {
                let Some(inner) = weak.upgrade() else { return };
                inner.menu_method(method, &params, invocation);
            })
            .property(move |_conn, _sender, _path, _iface, prop| {
                let Some(_inner) = weak2.upgrade() else { return "".to_variant() };
                match prop {
                    "Version" => 3u32.to_variant(),
                    "TextDirection" => "ltr".to_variant(),
                    "Status" => "normal".to_variant(),
                    "IconThemePath" => Vec::<String>::new().to_variant(),
                    _ => "".to_variant(),
                }
            })
            .build()
            .map_err(|e| e.to_string())?;
        inner.registrations.borrow_mut().extend([sni_id, menu_id]);

        let owner = gio::bus_own_name_on_connection(
            &conn,
            &bus_name,
            gio::BusNameOwnerFlags::NONE,
            |_, _| {},
            |_, name| crate::debug!("lost tray bus name {name}"),
        );
        *inner.owner_id.borrow_mut() = Some(owner);

        // (Re-)register whenever a StatusNotifierWatcher appears, e.g. when the
        // AppIndicator extension gets enabled after we started.
        let weak = Rc::downgrade(&inner);
        let watcher = gio::bus_watch_name_on_connection(
            &conn,
            "org.kde.StatusNotifierWatcher",
            gio::BusNameWatcherFlags::NONE,
            move |conn, _, _| {
                let Some(inner) = weak.upgrade() else { return };
                conn.call(
                    Some("org.kde.StatusNotifierWatcher"),
                    "/StatusNotifierWatcher",
                    "org.kde.StatusNotifierWatcher",
                    "RegisterStatusNotifierItem",
                    Some(&(inner.bus_name.as_str(),).to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    5000,
                    gio::Cancellable::NONE,
                    |res| {
                        if let Err(e) = res {
                            crate::debug!("tray registration failed: {e}");
                        }
                    },
                );
            },
            |_, _| crate::debug!("no StatusNotifierWatcher; the panel icon is hidden"),
        );
        *inner.watcher_id.borrow_mut() = Some(watcher);
        Ok(Tray { inner })
    }

    pub fn set_tooltip(&self, tooltip: &str) {
        *self.inner.tooltip.borrow_mut() = tooltip.to_owned();
        self.inner.emit(SNI_PATH, SNI_IFACE, "NewToolTip", None);
    }

    pub fn set_menu(&self, entries: Vec<MenuEntry>) {
        *self.inner.menu.borrow_mut() = entries;
        let revision = self.inner.revision.get() + 1;
        self.inner.revision.set(revision);
        self.inner
            .emit(MENU_PATH, MENU_IFACE, "LayoutUpdated", Some(&(revision, 0i32).to_variant()));
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Some(id) = self.inner.watcher_id.borrow_mut().take() {
            gio::bus_unwatch_name(id);
        }
        if let Some(id) = self.inner.owner_id.borrow_mut().take() {
            gio::bus_unown_name(id);
        }
        for id in self.inner.registrations.borrow_mut().drain(..) {
            let _ = self.inner.conn.unregister_object(id);
        }
    }
}

impl Inner {
    fn emit(&self, path: &str, iface: &str, signal: &str, params: Option<&Variant>) {
        if let Err(e) = self.conn.emit_signal(None, path, iface, signal, params) {
            crate::debug!("tray signal {signal}: {e}");
        }
    }

    fn sni_property(&self, prop: &str) -> Variant {
        match prop {
            "Category" => "ApplicationStatus".to_variant(),
            "Id" => "clipnest".to_variant(),
            "Title" => crate::config::APP_NAME.to_variant(),
            "Status" => "Active".to_variant(),
            "WindowId" => 0i32.to_variant(),
            "IconName" => self.icon_name.borrow().to_variant(),
            "IconThemePath" => self.icon_theme_path.to_variant(),
            "IconPixmap" => self.icon_pixmaps.borrow().to_variant(),
            "OverlayIconName" | "AttentionIconName" | "XAyatanaLabelGuide" => "".to_variant(),
            "ToolTip" => (
                self.icon_name.borrow().as_str(),
                Vec::<(i32, i32, Vec<u8>)>::new(),
                crate::config::APP_NAME,
                self.tooltip.borrow().as_str(),
            )
                .to_variant(),
            "ItemIsMenu" => false.to_variant(),
            "Menu" => ObjectPath::try_from(MENU_PATH.to_owned()).expect("valid path").to_variant(),
            "XAyatanaLabel" => self.label.borrow().to_variant(),
            _ => "".to_variant(),
        }
    }

    /// Every property is always sent: hosts cache them by id, and ids get reused
    /// when the menu is rebuilt (an old separator can become a normal item).
    fn index_of(&self, id: i32) -> Option<usize> {
        self.menu.borrow().iter().enumerate().position(|(i, e)| e.id(i) == id)
    }

    fn item_props(&self, index: usize) -> HashMap<String, Variant> {
        let menu = self.menu.borrow();
        let (kind, label, enabled, toggle_type, toggle_state) = match menu.get(index) {
            Some(MenuEntry::Separator) => ("separator", String::new(), true, "", 0i32),
            Some(MenuEntry::Item { label, enabled, checked, .. }) => (
                "standard",
                escape_label(label),
                *enabled,
                if checked.is_some() { "checkmark" } else { "" },
                if checked == &Some(true) { 1 } else { 0 },
            ),
            None => return HashMap::new(),
        };
        let mut props = HashMap::new();
        props.insert("type".to_owned(), kind.to_variant());
        props.insert("label".to_owned(), label.to_variant());
        props.insert("enabled".to_owned(), enabled.to_variant());
        props.insert("visible".to_owned(), true.to_variant());
        props.insert("toggle-type".to_owned(), toggle_type.to_variant());
        props.insert("toggle-state".to_owned(), toggle_state.to_variant());
        if let Some(MenuEntry::Item { icon: Some(png), .. }) = menu.get(index) {
            props.insert("icon-data".to_owned(), Variant::array_from_fixed_array(&png[..]));
        }
        props
    }

    fn layout(&self) -> Variant {
        let count = self.menu.borrow().len();
        let children: Vec<Variant> = (0..count)
            .map(|i| (self.menu.borrow()[i].id(i), self.item_props(i), Vec::<Variant>::new()).to_variant())
            .collect();
        let mut root = HashMap::new();
        root.insert("children-display".to_owned(), "submenu".to_variant());
        (0i32, root, children).to_variant()
    }

    fn activate_item(self: &Rc<Self>, id: i32) {
        let Some(index) = self.index_of(id) else { return };
        let action = match self.menu.borrow().get(index) {
            Some(MenuEntry::Item { action, enabled: true, .. }) => Some(action.clone()),
            _ => None,
        };
        if let Some(action) = action {
            glib::idle_add_local_once(move || action());
        }
    }

    fn menu_method(self: &Rc<Self>, method: &str, params: &Variant, invocation: gio::DBusMethodInvocation) {
        match method {
            "GetLayout" => {
                let reply = Variant::tuple_from_iter([self.revision.get().to_variant(), self.layout()]);
                invocation.return_value(Some(&reply));
            }
            "GetGroupProperties" => {
                let ids: Vec<i32> = params.child_value(0).get().unwrap_or_default();
                let props: Vec<(i32, HashMap<String, Variant>)> = ids
                    .into_iter()
                    .filter_map(|id| self.index_of(id).map(|i| (id, self.item_props(i))))
                    .collect();
                invocation.return_value(Some(&(props,).to_variant()));
            }
            "GetProperty" => {
                let (id, name): (i32, String) = params.get().unwrap_or((0, String::new()));
                let props = self.index_of(id).map(|i| self.item_props(i)).unwrap_or_default();
                let value = props.get(&name).cloned().unwrap_or_else(|| "".to_variant());
                invocation.return_value(Some(&Variant::tuple_from_iter([Variant::from_variant(&value)])));
            }
            "Event" => {
                let id: i32 = params.child_value(0).get().unwrap_or(0);
                let event: String = params.child_value(1).get().unwrap_or_default();
                invocation.return_value(None);
                if event == "clicked" {
                    self.activate_item(id);
                }
            }
            "EventGroup" => {
                let events = params.child_value(0);
                for i in 0..events.n_children() {
                    let e = events.child_value(i);
                    let id: i32 = e.child_value(0).get().unwrap_or(0);
                    let kind: String = e.child_value(1).get().unwrap_or_default();
                    if kind == "clicked" {
                        self.activate_item(id);
                    }
                }
                invocation.return_value(Some(&(Vec::<i32>::new(),).to_variant()));
            }
            "AboutToShow" => invocation.return_value(Some(&(false,).to_variant())),
            "AboutToShowGroup" => {
                invocation.return_value(Some(&(Vec::<i32>::new(), Vec::<i32>::new()).to_variant()))
            }
            _ => invocation.return_value(None),
        }
    }
}

/// Renders the tray glyph (white, for dark panels) into SNI ARGB32 pixmaps.
fn render_pixmaps(svg: &str) -> Vec<(i32, i32, Vec<u8>)> {
    let white = svg.replace("#2e3436", "#ffffff");
    let mut out = Vec::new();
    for size in [22, 44] {
        let loader = gtk::gdk_pixbuf::PixbufLoader::new();
        loader.set_size(size, size);
        if loader.write(white.as_bytes()).is_err() || loader.close().is_err() {
            continue;
        }
        let Some(pixbuf) = loader.pixbuf() else { continue };
        let pixbuf = if pixbuf.has_alpha() {
            pixbuf
        } else {
            match pixbuf.add_alpha(false, 0, 0, 0) {
                Ok(p) => p,
                Err(_) => continue,
            }
        };
        let (w, h, stride, n) = (pixbuf.width(), pixbuf.height(), pixbuf.rowstride() as usize, pixbuf.n_channels() as usize);
        let bytes = pixbuf.read_pixel_bytes();
        let mut argb = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = &bytes[y * stride + x * n..y * stride + x * n + 4];
                argb.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
            }
        }
        out.push((w, h, argb));
    }
    out
}
