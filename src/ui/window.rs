//! The history window: search, browse, copy, pin and delete clipboard items.
//!
//! Keyboard-first like Maccy: the search field keeps focus while ↑/↓ move the
//! selection, Enter copies, Shift+Enter copies without formatting, Ctrl+1…9 copy
//! the first nine items and Ctrl+<key> copies a pinned item.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::app::app;
use crate::keys;
use crate::model::Item;
use crate::search;
use crate::settings::{HighlightMatch, SearchMode, Settings};
use crate::ui::row::{ItemObject, RowWidget};

const TITLE_DISPLAY_CHARS: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Pin(i64),
    Item(i64),
}

impl Slot {
    fn id(self) -> i64 {
        match self {
            Slot::Pin(id) | Slot::Item(id) => id,
        }
    }
}

#[derive(Default)]
struct State {
    slots: Vec<Slot>,
    selected: Option<usize>,
    query: String,
    shortcut_chars: HashMap<i64, String>,
    objects: HashMap<i64, ItemObject>,
    /// Decoded thumbnails survive refreshes (row objects are recreated each time
    /// so GTK re-binds rows whose label or shortcut changed).
    thumbs: HashMap<i64, Option<gdk::Texture>>,
    search_timer: Option<glib::SourceId>,
    preedit: bool,
}

struct Preview {
    root: gtk::Box,
    stack: gtk::Stack,
    text: gtk::TextView,
    picture: gtk::Picture,
    meta: gtk::Label,
    pin: gtk::Button,
    /// Text recognized in an image.
    ocr: gtk::Label,
}

pub struct MainWindow {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    search: gtk::SearchEntry,
    banner: adw::Banner,
    pins: gtk::ListBox,
    pins_scroller: gtk::ScrolledWindow,
    separator: gtk::Separator,
    scroller: gtk::ScrolledWindow,
    list_view: gtk::ListView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    preview: Preview,
    st: RefCell<State>,
}

fn settings() -> Settings {
    app().settings.borrow().clone()
}

impl MainWindow {
    pub fn new(gtk_app: &adw::Application) -> Rc<MainWindow> {
        let s = settings();
        let window = adw::ApplicationWindow::builder()
            .application(gtk_app)
            .title(crate::config::APP_NAME)
            .default_width(s.window_width)
            .default_height(s.window_height)
            .hide_on_close(true)
            .build();

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Type to search…")
            .hexpand(true)
            .search_delay(0)
            .build();
        let header = adw::HeaderBar::new();
        let search_clamp = adw::Clamp::builder().maximum_size(520).child(&search).build();
        header.set_title_widget(Some(&search_clamp));

        let menu = gio::Menu::new();
        let s1 = gio::Menu::new();
        s1.append(Some("Pause Saving New Copies"), Some("app.pause"));
        s1.append(Some("Ignore Next Copy"), Some("app.ignore-next"));
        menu.append_section(None, &s1);
        let s2 = gio::Menu::new();
        s2.append(Some("Clear History"), Some("win.clear"));
        s2.append(Some("Clear All, Including Pinned"), Some("win.clear-all"));
        menu.append_section(None, &s2);
        let s3 = gio::Menu::new();
        s3.append(Some("Preferences"), Some("app.preferences"));
        s3.append(Some("Keyboard Shortcuts"), Some("win.show-help"));
        s3.append(Some("About Clipnest"), Some("app.about"));
        s3.append(Some("Quit"), Some("app.quit"));
        menu.append_section(None, &s3);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main Menu")
            .primary(true)
            .build();
        header.pack_end(&menu_button);
        let preview_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
            .tooltip_text("Show Details (Ctrl+Space)")
            .action_name("win.toggle-preview")
            .build();
        header.pack_end(&preview_toggle);

        let banner = adw::Banner::new("");
        banner.set_revealed(false);

        // Pinned items (not scrolling with the rest, like Maccy), then the history.
        let pins = gtk::ListBox::new();
        pins.add_css_class("clipnest-list");
        pins.set_selection_mode(gtk::SelectionMode::Single);
        pins.set_focusable(false);
        pins.set_activate_on_single_click(false);
        let pins_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(230)
            .child(&pins)
            .build();
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        separator.add_css_class("clipnest-separator");

        let store = gio::ListStore::new::<ItemObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        list_view.set_focusable(false);
        list_view.add_css_class("clipnest-listview");
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_view)
            .build();

        let list_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_box.add_css_class("clipnest-list-area");
        list_box.append(&pins_scroller);
        list_box.append(&separator);
        list_box.append(&scroller);

        let empty = adw::StatusPage::builder()
            .icon_name(crate::config::SYMBOLIC_ICON_NAME)
            .title("Nothing copied yet")
            .description("Copy text, images or files and they will show up here.")
            .vexpand(true)
            .build();
        empty.add_css_class("compact");
        let stack = gtk::Stack::new();
        stack.add_named(&list_box, Some("list"));
        stack.add_named(&empty, Some("empty"));
        stack.set_hexpand(true);

        let preview = build_preview();
        let paned = gtk::Paned::new(gtk::Orientation::Horizontal);
        paned.set_start_child(Some(&stack));
        paned.set_end_child(Some(&preview.root));
        paned.set_resize_start_child(true);
        paned.set_resize_end_child(false);
        paned.set_shrink_start_child(false);
        paned.set_shrink_end_child(false);
        paned.set_position((s.window_width as f64 * 0.52) as i32);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&paned));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&toasts));
        window.set_content(Some(&toolbar));

        let win = Rc::new(MainWindow {
            window,
            toasts,
            search,
            banner,
            pins,
            pins_scroller,
            separator,
            scroller,
            list_view,
            store,
            selection,
            stack,
            empty,
            preview,
            st: RefCell::new(State::default()),
        });
        win.setup_factory(&factory);
        win.setup_actions();
        win.connect_signals();
        win.set_preview_visible(s.show_preview);
        win
    }

    // ---- setup ------------------------------------------------------------

    fn setup_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            item.set_focusable(false);
            item.set_activatable(false);
            let row = RowWidget::new();
            if let Some(w) = weak.upgrade() {
                w.attach_row_controllers(&row, true);
            }
            item.set_child(Some(&row));
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let (Some(row), Some(obj)) = (item.child().and_downcast::<RowWidget>(), item.item().and_downcast::<ItemObject>())
            else {
                return;
            };
            if let Some(w) = weak.upgrade() {
                w.bind_row(&row, &obj, item.position());
            }
        });
    }

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: Box<dyn Fn(&Rc<MainWindow>)>| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(w) = weak.upgrade() {
                    f(&w);
                }
            });
            self.window.add_action(&action);
        };
        add("copy", Box::new(|w| w.copy_selected(false)));
        add("copy-plain", Box::new(|w| w.copy_selected(true)));
        add("pin", Box::new(|w| w.toggle_pin_selected()));
        add("delete", Box::new(|w| w.delete_selected()));
        add("pin-up", Box::new(|w| w.move_pin_selected(-1)));
        add("pin-down", Box::new(|w| w.move_pin_selected(1)));
        add("clear", Box::new(|w| app().request_clear(false, Some(w.window.upcast_ref()))));
        add("clear-all", Box::new(|w| app().request_clear(true, Some(w.window.upcast_ref()))));
        add("focus-search", Box::new(|w| {
            w.search.grab_focus();
        }));
        add("show-help", Box::new(|w| w.show_shortcuts_help()));

        let toggle = gio::SimpleAction::new_stateful("toggle-preview", None, &settings().show_preview.to_variant());
        let weak = Rc::downgrade(self);
        toggle.connect_activate(move |action, _| {
            let Some(w) = weak.upgrade() else { return };
            let visible = !action.state().and_then(|v| v.get::<bool>()).unwrap_or(true);
            action.set_state(&visible.to_variant());
            app().update_settings(|s| s.show_preview = visible);
            w.set_preview_visible(visible);
        });
        self.window.add_action(&toggle);
    }

    fn connect_signals(self: &Rc<Self>) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |ctrl, key, keycode, mods| {
            let Some(w) = weak.upgrade() else { return glib::Propagation::Proceed };
            let layout = ctrl
                .current_event()
                .and_then(|e| e.downcast::<gdk::KeyEvent>().ok())
                .map(|e| e.layout())
                .unwrap_or(0);
            w.on_key_pressed(key, keycode, layout, mods)
        });
        self.window.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.search.connect_search_changed(move |entry| {
            if let Some(w) = weak.upgrade() {
                w.on_query_changed(entry.text().to_string());
            }
        });
        if let Some(text) = self.search.delegate().and_downcast::<gtk::Text>() {
            let weak = Rc::downgrade(self);
            text.connect_preedit_changed(move |_, preedit| {
                if let Some(w) = weak.upgrade() {
                    w.st.borrow_mut().preedit = !preedit.is_empty();
                }
            });
        }

        // Remember the window size.
        self.window.connect_close_request(|win| {
            let (w, h) = (win.width(), win.height());
            if w > 0 && h > 0 && !win.is_maximized() {
                app().update_settings(|s| {
                    s.window_width = w;
                    s.window_height = h;
                });
            }
            glib::Propagation::Proceed
        });

        let weak = Rc::downgrade(self);
        self.banner.connect_button_clicked(move |_| {
            if weak.upgrade().is_some() {
                app().set_paused(false);
            }
        });
    }

    fn attach_row_controllers(self: &Rc<Self>, row: &RowWidget, in_list: bool) {
        let click = gtk::GestureClick::new();
        click.set_button(0);
        let weak = Rc::downgrade(self);
        let r = row.downgrade();
        click.connect_pressed(move |gesture, n_press, x, y| {
            let (Some(w), Some(row)) = (weak.upgrade(), r.upgrade()) else { return };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let id = row.key();
            let slot = if in_list { Slot::Item(id) } else { Slot::Pin(id) };
            w.select_slot(slot);
            match gesture.current_button() {
                gdk::BUTTON_SECONDARY => w.show_context_menu(&row, x, y),
                gdk::BUTTON_PRIMARY if n_press == 2 => {
                    let plain = gesture.current_event_state().contains(gdk::ModifierType::SHIFT_MASK);
                    w.copy_selected(plain);
                }
                _ => {}
            }
        });
        row.add_controller(click);
    }

    fn bind_row(&self, row: &RowWidget, obj: &ItemObject, position: u32) {
        let height = app().settings.borrow().image_max_height as i32;
        row.bind_item(obj, position, height);
        if obj.is_image() && obj.thumb().is_none() && !obj.thumb_failed() {
            let obj = obj.clone();
            let row = row.downgrade();
            glib::idle_add_local_once(move || {
                if obj.thumb().is_some() || obj.thumb_failed() {
                    return;
                }
                let texture = load_thumbnail(obj.id(), height);
                obj.set_thumb(texture.clone());
                if let Some(w) = app().window() {
                    w.st.borrow_mut().thumbs.insert(obj.id(), texture.clone());
                }
                if let (Some(row), Some(texture)) = (row.upgrade(), texture) {
                    if row.key() == obj.id() {
                        row.set_thumb(&texture, height);
                    }
                }
            });
        }
    }

    // ---- showing ------------------------------------------------------------

    pub fn present(self: &Rc<Self>, token: Option<String>) {
        if !self.window.is_visible() {
            self.search.set_text("");
            self.st.borrow_mut().query.clear();
            self.refresh_with_selection(None);
        }
        if let (Some(token), Some(display)) = (token, gdk::Display::default()) {
            if let Some(wayland) = display.downcast_ref::<gdk_wayland::WaylandDisplay>() {
                wayland.set_startup_notification_id(&token);
            }
        }
        self.window.present();
        self.search.grab_focus();
    }

    pub fn toggle(self: &Rc<Self>, token: Option<String>) {
        if self.window.is_visible() && self.window.is_active() {
            self.window.close();
        } else {
            self.present(token);
        }
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn close(&self) {
        self.window.close();
    }

    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.window
    }

    pub fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_timeout(2);
        self.toasts.add_toast(toast);
    }

    fn set_preview_visible(&self, visible: bool) {
        self.preview.root.set_visible(visible);
        if visible {
            self.update_preview();
        }
    }

    /// Updates the "paused" banner.
    pub fn update_banner(&self) {
        let s = settings();
        if s.ignore_events && s.ignore_only_next_event {
            self.banner.set_title("The next copy will not be saved");
            self.banner.set_button_label(Some("Save It"));
            self.banner.set_revealed(true);
        } else if s.ignore_events {
            self.banner.set_title("Paused — new copies are not saved");
            self.banner.set_button_label(Some("Resume"));
            self.banner.set_revealed(true);
        } else {
            self.banner.set_revealed(false);
        }
    }

    // ---- data -------------------------------------------------------------

    /// Re-reads the history (after a copy, pin, delete…) keeping the selection.
    pub fn refresh(self: &Rc<Self>) {
        let selected = self.selected_slot();
        self.refresh_with_selection(selected);
    }

    fn refresh_with_selection(&self, keep: Option<Slot>) {
        self.update_banner();
        let s = settings();
        let query = self.st.borrow().query.clone();
        struct Row {
            id: i64,
            title: String,
            ranges: Vec<(usize, usize)>,
            is_image: bool,
            color: Option<[f64; 4]>,
            pin: Option<String>,
        }
        let (pinned_rows, unpinned_rows, total) = {
            let a = app();
            let history = a.history.borrow();
            let (pinned, unpinned) = history.ordered(&s);
            let collect = |ids: &[i64]| -> Vec<Row> {
                let items: Vec<&Item> = ids.iter().filter_map(|id| history.get(*id)).collect();
                let texts: Vec<String> = items.iter().map(|i| i.search_text()).collect();
                let titles: Vec<&str> = texts.iter().map(String::as_str).collect();
                search::search(&query, s.search_mode, &titles)
                    .into_iter()
                    .map(|m| {
                        let item = items[m.index];
                        let is_image = item.is_image_item();
                        // Pictures show a snippet of the text found in them, around the match.
                        let (title, ranges) = if is_image {
                            snippet(&texts[m.index], &m.ranges, 70)
                        } else {
                            (item.title.clone(), m.ranges)
                        };
                        Row {
                            id: item.id,
                            title,
                            ranges,
                            is_image,
                            color: if s.show_hex_color_swatch { crate::model::parse_hex_color(&item.title) } else { None },
                            pin: item.pin.clone(),
                        }
                    })
                    .collect()
            };
            (collect(&pinned), collect(&unpinned), history.items().len())
        };

        // Quick-copy keys: pins keep theirs, the first nine others get 1–9.
        let mut chars: HashMap<i64, String> = HashMap::new();
        for r in &pinned_rows {
            if let Some(p) = &r.pin {
                chars.insert(r.id, p.clone());
            }
        }
        for (n, r) in unpinned_rows.iter().take(9).enumerate() {
            chars.insert(r.id, (n + 1).to_string());
        }
        let make_obj = |r: &Row| -> ItemObject {
            let obj = ItemObject::new(r.id);
            if let Some(thumb) = self.st.borrow().thumbs.get(&r.id) {
                obj.set_thumb(thumb.clone());
            }
            let markup = title_markup(&r.title, &r.ranges, s.highlight_match);
            let shortcut = chars.get(&r.id).map(|c| keys::char_label(c)).unwrap_or_default();
            obj.set_display(markup, shortcut, r.color, r.is_image);
            obj
        };
        let pinned_objs: Vec<ItemObject> = pinned_rows.iter().map(make_obj).collect();
        let unpinned_objs: Vec<ItemObject> = unpinned_rows.iter().map(make_obj).collect();
        {
            let mut st = self.st.borrow_mut();
            st.objects = pinned_objs.iter().chain(unpinned_objs.iter()).map(|o| (o.id(), o.clone())).collect();
            st.shortcut_chars = chars;
        }

        self.store.splice(0, self.store.n_items(), &unpinned_objs);
        self.pins.remove_all();
        for obj in &pinned_objs {
            let row_widget = RowWidget::new();
            self.bind_row(&row_widget, obj, 0);
            let row = gtk::ListBoxRow::new();
            row.set_child(Some(&row_widget));
            row.set_activatable(false);
            row.set_focusable(false);
            self.pins.append(&row);
            if let Some(w) = app().window() {
                w.attach_row_controllers(&row_widget, false);
            }
        }
        self.pins_scroller.set_visible(!pinned_objs.is_empty());
        // A scrollbar reserves height even when idle: only scroll when there are many pins.
        self.pins_scroller.set_vscrollbar_policy(if pinned_objs.len() > 8 {
            gtk::PolicyType::Automatic
        } else {
            gtk::PolicyType::Never
        });
        self.separator.set_visible(!pinned_objs.is_empty() && !unpinned_objs.is_empty());
        self.scroller.set_visible(!unpinned_objs.is_empty());

        if pinned_objs.is_empty() && unpinned_objs.is_empty() {
            if total == 0 {
                self.empty.set_title("Nothing copied yet");
                self.empty.set_description(Some("Copy text, images or files and they will show up here."));
            } else {
                self.empty.set_title("No matches");
                self.empty.set_description(Some("Press Enter to copy the search text."));
            }
            self.stack.set_visible_child_name("empty");
        } else {
            self.stack.set_visible_child_name("list");
        }

        let mut slots: Vec<Slot> = pinned_objs.iter().map(|o| Slot::Pin(o.id())).collect();
        slots.extend(unpinned_objs.iter().map(|o| Slot::Item(o.id())));
        let selected = keep.and_then(|k| slots.iter().position(|s| *s == k));
        let default = if query.is_empty() {
            // Like Maccy: the most recent copy, not a pin.
            slots.iter().position(|s| matches!(s, Slot::Item(_))).or(if slots.is_empty() { None } else { Some(0) })
        } else if slots.is_empty() {
            None
        } else {
            Some(0)
        };
        {
            let mut st = self.st.borrow_mut();
            st.slots = slots;
            st.selected = selected.or(default);
        }
        self.apply_selection(true);
    }

    fn selected_slot(&self) -> Option<Slot> {
        let st = self.st.borrow();
        st.selected.and_then(|i| st.slots.get(i).copied())
    }

    fn selected_id(&self) -> Option<i64> {
        self.selected_slot().map(Slot::id)
    }

    fn select_slot(&self, slot: Slot) {
        let idx = self.st.borrow().slots.iter().position(|s| *s == slot);
        if idx.is_some() {
            self.st.borrow_mut().selected = idx;
            self.apply_selection(false);
        }
    }

    fn select_index(&self, idx: Option<usize>) {
        self.st.borrow_mut().selected = idx;
        self.apply_selection(true);
    }

    fn apply_selection(&self, scroll: bool) {
        match self.selected_slot() {
            Some(Slot::Item(id)) => {
                self.pins.unselect_all();
                let pos = (0..self.store.n_items())
                    .find(|i| self.store.item(*i).and_downcast::<ItemObject>().map(|o| o.id() == id).unwrap_or(false));
                if let Some(pos) = pos {
                    self.selection.set_selected(pos);
                    if scroll {
                        self.list_view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
                    }
                }
            }
            Some(Slot::Pin(id)) => {
                self.selection.set_selected(gtk::INVALID_LIST_POSITION);
                let mut i = 0;
                while let Some(row) = self.pins.row_at_index(i) {
                    if row.child().and_downcast::<RowWidget>().map(|r| r.key() == id).unwrap_or(false) {
                        self.pins.select_row(Some(&row));
                        if scroll {
                            row.grab_focus();
                            self.search.grab_focus();
                        }
                        break;
                    }
                    i += 1;
                }
            }
            None => {
                self.selection.set_selected(gtk::INVALID_LIST_POSITION);
                self.pins.unselect_all();
            }
        }
        if self.preview.root.get_visible() {
            self.update_preview();
        }
    }

    fn on_query_changed(self: &Rc<Self>, query: String) {
        if self.st.borrow().query == query {
            return;
        }
        self.st.borrow_mut().query = query;
        if let Some(id) = self.st.borrow_mut().search_timer.take() {
            id.remove();
        }
        let delay = match settings().search_mode {
            SearchMode::Fuzzy | SearchMode::Mixed => 80,
            _ => 0,
        };
        let weak = Rc::downgrade(self);
        let run = move || {
            if let Some(w) = weak.upgrade() {
                w.st.borrow_mut().search_timer = None;
                w.refresh_with_selection(None);
            }
        };
        if delay == 0 {
            run();
        } else {
            let id = glib::timeout_add_local_once(Duration::from_millis(delay), run);
            self.st.borrow_mut().search_timer = Some(id);
        }
    }

    // ---- keyboard -----------------------------------------------------------

    fn on_key_pressed(self: &Rc<Self>, key: gdk::Key, keycode: u32, layout: u32, mods: gdk::ModifierType) -> glib::Propagation {
        use gdk::Key;
        use gdk::ModifierType as M;
        use glib::Propagation::{Proceed, Stop};

        if self.window.visible_dialog().is_some() || self.st.borrow().preedit {
            return Proceed;
        }
        // Only steer the list while the search field (or nothing) has focus.
        let focus_in_search = gtk::prelude::RootExt::focus(&self.window)
            .map(|f| f.is_ancestor(&self.search) || f == self.search.clone().upcast::<gtk::Widget>())
            .unwrap_or(true);
        if !focus_in_search {
            return Proceed;
        }
        let m = keys::relevant(mods);
        let ctrl = M::CONTROL_MASK;
        let lower = key.to_lower();
        let down = matches!(key, Key::Down | Key::KP_Down);
        let up = matches!(key, Key::Up | Key::KP_Up);

        if (down && m.is_empty()) || (matches!(lower, Key::n | Key::j) && m == ctrl) {
            self.move_selection(1);
            return Stop;
        }
        if (up && m.is_empty()) || (matches!(lower, Key::p | Key::k) && m == ctrl) {
            self.move_selection(-1);
            return Stop;
        }
        if key == Key::Page_Down || (down && m.intersects(ctrl | M::ALT_MASK)) {
            let last = self.st.borrow().slots.len().checked_sub(1);
            self.select_index(last);
            return Stop;
        }
        if key == Key::Page_Up || (up && m.intersects(ctrl | M::ALT_MASK)) {
            let first = if self.st.borrow().slots.is_empty() { None } else { Some(0) };
            self.select_index(first);
            return Stop;
        }
        if matches!(key, Key::Return | Key::KP_Enter | Key::ISO_Enter) && (m.is_empty() || m == M::SHIFT_MASK) {
            let plain = m == M::SHIFT_MASK;
            if self.selected_id().is_some() {
                self.copy_selected(plain);
            } else {
                let text = self.search.text().to_string();
                if !text.is_empty() {
                    app().copy_text(&text);
                    self.after_copy();
                }
            }
            return Stop;
        }
        if key == Key::Escape && m.is_empty() {
            if self.search.text().is_empty() {
                self.window.close();
            } else {
                self.search.set_text("");
            }
            return Stop;
        }
        if lower == Key::u && m == ctrl {
            self.search.set_text("");
            return Stop;
        }
        if matches!(key, Key::Delete | Key::KP_Delete) && m.is_empty() && self.search.text().is_empty() {
            self.delete_selected();
            return Stop;
        }
        // Quick copy: Ctrl+1…9 for the first items, Ctrl+<key> for pins.
        if m == ctrl || m == ctrl | M::SHIFT_MASK {
            if let Some(c) = keys::event_char(&WidgetExt::display(&self.window), key, keycode, layout) {
                let id = self.st.borrow().shortcut_chars.iter().find(|(_, ch)| **ch == c).map(|(id, _)| *id);
                if let Some(id) = id {
                    let slot = self.st.borrow().slots.iter().copied().find(|s| s.id() == id);
                    if let Some(slot) = slot {
                        self.select_slot(slot);
                        self.copy_selected(m.contains(M::SHIFT_MASK));
                        return Stop;
                    }
                }
            }
        }
        Proceed
    }

    fn move_selection(&self, delta: i32) {
        let (len, cur) = {
            let st = self.st.borrow();
            (st.slots.len(), st.selected)
        };
        if len == 0 {
            return;
        }
        let next = match cur {
            None => 0,
            Some(i) => (i as i32 + delta).clamp(0, len as i32 - 1) as usize,
        };
        self.select_index(Some(next));
    }

    // ---- actions ----------------------------------------------------------

    fn copy_selected(self: &Rc<Self>, plain: bool) {
        let Some(id) = self.selected_id() else { return };
        app().copy_item(id, plain);
        self.after_copy();
    }

    fn after_copy(self: &Rc<Self>) {
        if settings().close_after_copy {
            self.window.close();
            app().notify_copied_from_history();
        } else {
            self.toast("Copied to clipboard");
            self.refresh_with_selection(None);
        }
    }

    fn delete_selected(self: &Rc<Self>) {
        let Some(id) = self.selected_id() else { return };
        let idx = self.st.borrow().selected.unwrap_or(0);
        app().delete_item(id);
        let len = self.st.borrow().slots.len();
        if len > 0 {
            self.select_index(Some(idx.min(len - 1)));
        }
    }

    fn toggle_pin_selected(self: &Rc<Self>) {
        let Some(id) = self.selected_id() else { return };
        app().toggle_pin(id);
        let idx = self.st.borrow().slots.iter().position(|s| s.id() == id);
        self.select_index(idx);
    }

    fn move_pin_selected(self: &Rc<Self>, delta: i32) {
        let Some(Slot::Pin(id)) = self.selected_slot() else { return };
        app().move_pin(id, delta);
        self.select_slot(Slot::Pin(id));
    }

    fn show_context_menu(self: &Rc<Self>, row: &RowWidget, x: f64, y: f64) {
        let pinned = self.selected_slot().map(|s| matches!(s, Slot::Pin(_))).unwrap_or(false);
        let menu = gio::Menu::new();
        menu.append(Some("Copy"), Some("win.copy"));
        menu.append(Some("Copy Without Formatting"), Some("win.copy-plain"));
        menu.append(Some(if pinned { "Unpin" } else { "Pin" }), Some("win.pin"));
        if pinned {
            menu.append(Some("Move Up"), Some("win.pin-up"));
            menu.append(Some("Move Down"), Some("win.pin-down"));
        }
        menu.append(Some("Delete"), Some("win.delete"));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(row);
        popover.set_has_arrow(false);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn show_shortcuts_help(&self) {
        let dialog = adw::AlertDialog::new(Some("Keyboard Shortcuts"), None);
        dialog.set_body(
            "↑ / ↓  or  Ctrl+N / Ctrl+P — move\n\
             Enter or double-click — copy\n\
             Shift+Enter — copy without formatting\n\
             Ctrl+1 … Ctrl+9 — copy one of the first nine items\n\
             Ctrl+<key> — copy a pinned item\n\
             Alt+P — pin or unpin\n\
             Alt+Backspace, or Delete with an empty search — delete\n\
             Ctrl+Space — show or hide details\n\
             Ctrl+F — search · Ctrl+U — clear search\n\
             Ctrl+Shift+Backspace — clear history\n\
             Escape — clear search, then close · Ctrl+, — preferences",
        );
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.window));
    }

    // ---- details pane ---------------------------------------------------------

    fn update_preview(&self) {
        let p = &self.preview;
        let Some(id) = self.selected_id() else {
            p.stack.set_visible_child_name("none");
            p.meta.set_text("");
            return;
        };
        let app = app();
        let history = app.history.borrow();
        let Some(item) = history.get(id) else { return };
        let mut meta: Vec<String> = Vec::new();
        let mut showed_image = false;
        if item.has_image() && item.title.is_empty() {
            if let Some(content) = item.image_content() {
                if let Some(data) = history.content_data(content) {
                    if let Ok(texture) = gdk::Texture::from_bytes(&glib::Bytes::from(&data[..])) {
                        meta.push(format!("Dimensions: {} × {}", texture.width(), texture.height()));
                        meta.push(format!("Size: {}", glib::format_size(data.len() as u64)));
                        p.picture.set_paintable(Some(&texture));
                        p.stack.set_visible_child_name("image");
                        showed_image = true;
                        match &item.ocr_text {
                            Some(t) if !t.is_empty() => {
                                p.ocr.set_text(t);
                                p.ocr.set_visible(true);
                                meta.push("Copy Plain copies the text in the image".to_owned());
                            }
                            Some(_) => p.ocr.set_visible(false),
                            None => {
                                let pending = app.ocr_langs.borrow().is_some();
                                p.ocr.set_text(if pending { "Reading text…" } else { "" });
                                p.ocr.set_visible(pending);
                            }
                        }
                    }
                }
            }
        }
        if !showed_image {
            let text: String = item.previewable_text().chars().take(20_000).collect();
            p.text.buffer().set_text(&text);
            p.stack.set_visible_child_name("text");
            p.picture.set_paintable(gdk::Paintable::NONE);
            meta.push(format!("{} characters", item.previewable_text().chars().count()));
        }
        if let Some(application) = &item.application {
            meta.push(format!("Application: {application}"));
        }
        meta.push(format!("First copied: {}", format_time(item.first_copied_at)));
        meta.push(format!("Last copied: {}", format_time(item.last_copied_at)));
        meta.push(format!("Copied {} time{}", item.number_of_copies, if item.number_of_copies == 1 { "" } else { "s" }));
        if let Some(pin) = &item.pin {
            meta.push(format!("Pinned — Ctrl+{} copies it", pin.to_uppercase()));
        }
        p.meta.set_text(&meta.join("\n"));
        p.pin.set_label(if item.pin.is_some() { "Unpin" } else { "Pin" });
    }

    /// Drops cached thumbnails (after the image height setting changed).
    pub fn clear_thumbnails(&self) {
        self.st.borrow_mut().thumbs.clear();
        for obj in self.st.borrow().objects.values() {
            obj.clear_thumb();
        }
    }
}

fn build_preview() -> Preview {
    let text = gtk::TextView::new();
    text.set_editable(false);
    text.set_cursor_visible(false);
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.set_top_margin(6);
    text.set_left_margin(6);
    text.set_right_margin(6);
    let text_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&text)
        .build();
    let picture = gtk::Picture::new();
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_vexpand(true);
    let none = gtk::Label::new(Some("Select an item to see it here"));
    none.add_css_class("dim-label");
    let stack = gtk::Stack::new();
    stack.add_named(&text_scroll, Some("text"));
    let ocr = gtk::Label::new(None);
    ocr.set_wrap(true);
    ocr.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    ocr.set_xalign(0.0);
    ocr.set_selectable(true);
    ocr.set_max_width_chars(40);
    ocr.add_css_class("clipnest-ocr");
    let ocr_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(160)
        .child(&ocr)
        .build();
    let image_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    image_page.append(&picture);
    image_page.append(&ocr_scroll);
    stack.add_named(&image_page, Some("image"));
    stack.add_named(&none, Some("none"));
    stack.set_vexpand(true);

    let meta = gtk::Label::new(None);
    meta.add_css_class("dim-label");
    meta.add_css_class("caption");
    meta.set_xalign(0.0);
    meta.set_wrap(true);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let copy = gtk::Button::builder().label("Copy").action_name("win.copy").build();
    copy.add_css_class("suggested-action");
    let plain = gtk::Button::builder().label("Copy Plain").action_name("win.copy-plain").build();
    plain.set_tooltip_text(Some("Copy without formatting"));
    let pin = gtk::Button::builder().label("Pin").action_name("win.pin").build();
    let delete = gtk::Button::builder().icon_name("user-trash-symbolic").action_name("win.delete").build();
    delete.set_tooltip_text(Some("Delete"));
    delete.add_css_class("destructive-action");
    delete.set_hexpand(true);
    delete.set_halign(gtk::Align::End);
    buttons.append(&copy);
    buttons.append(&plain);
    buttons.append(&pin);
    buttons.append(&delete);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("clipnest-preview");
    root.set_size_request(260, -1);
    root.append(&stack);
    root.append(&meta);
    root.append(&buttons);
    Preview { root, stack, text, picture, meta, pin, ocr }
}

fn format_time(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|d| d.format("%x %X"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A window of `text` around the first match (with ranges shifted to it).
fn snippet(text: &str, ranges: &[(usize, usize)], max_chars: usize) -> (String, Vec<(usize, usize)>) {
    let start = ranges.first().map(|r| r.0).unwrap_or(0);
    // Start a little before the match, on a char boundary.
    let mut from = text[..start].char_indices().rev().nth(15).map(|(i, _)| i).unwrap_or(0);
    if from > 0 && !ranges.is_empty() {
        from = text[from..].find(' ').map(|p| from + p + 1).filter(|p| *p <= start).unwrap_or(from);
    }
    let to = text[from..].char_indices().nth(max_chars).map(|(i, _)| from + i).unwrap_or(text.len());
    let mut out = String::new();
    let prefix = if from > 0 { "…" } else { "" };
    out.push_str(prefix);
    out.push_str(&text[from..to]);
    if to < text.len() {
        out.push('…');
    }
    let shift = prefix.len() as isize - from as isize;
    let shifted = ranges
        .iter()
        .filter(|(s, e)| *s >= from && *e <= to)
        .map(|(s, e)| ((*s as isize + shift) as usize, (*e as isize + shift) as usize))
        .collect();
    (out, shifted)
}

/// Escapes the title for Pango and highlights the matched byte ranges.
pub fn title_markup(title: &str, ranges: &[(usize, usize)], style: HighlightMatch) -> String {
    let limit = title.char_indices().nth(TITLE_DISPLAY_CHARS).map(|(i, _)| i).unwrap_or(title.len());
    let text = &title[..limit];
    let clean = |s: &str| -> String {
        let s: String = s.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
        glib::markup_escape_text(&s).to_string()
    };
    let (open, close) = match style {
        HighlightMatch::Bold => ("<b>", "</b>"),
        HighlightMatch::Italic => ("<i>", "</i>"),
        HighlightMatch::Underline => ("<u>", "</u>"),
        HighlightMatch::Color => ("<span background=\"#ffe066\" foreground=\"#000000\">", "</span>"),
    };
    let mut out = String::new();
    let mut pos = 0;
    for &(start, end) in ranges {
        if start < pos || start >= text.len() || !text.is_char_boundary(start) {
            continue;
        }
        let end = end.min(text.len());
        if !text.is_char_boundary(end) {
            continue;
        }
        out.push_str(&clean(&text[pos..start]));
        out.push_str(open);
        out.push_str(&clean(&text[start..end]));
        out.push_str(close);
        pos = end;
    }
    out.push_str(&clean(&text[pos..]));
    out
}

/// Decodes an item's image scaled to the thumbnail height.
pub fn load_thumbnail(id: i64, max_height: i32) -> Option<gdk::Texture> {
    let app = app();
    let data = {
        let history = app.history.borrow();
        let item = history.get(id)?;
        let content = item.image_content()?;
        history.content_data(content)?
    };
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    let max_h = max_height.max(1);
    loader.connect_size_prepared(move |loader, w, h| {
        let scale = (max_h as f64 / h.max(1) as f64).min(340.0 / w.max(1) as f64).min(1.0);
        loader.set_size(((w as f64 * scale).round() as i32).max(1), ((h as f64 * scale).round() as i32).max(1));
    });
    loader.write(&data[..]).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    #[allow(deprecated)]
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_escapes_and_highlights() {
        let m = title_markup("a<b> & c", &[(1, 4)], HighlightMatch::Bold);
        assert_eq!(m, "a<b>&lt;b&gt;</b> &amp; c");
        let m = title_markup("x\ny", &[], HighlightMatch::Bold);
        assert_eq!(m, "x y");
    }
}
