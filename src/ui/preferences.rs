//! Preferences window (General / Storage / Appearance / Ignore / Advanced, like Maccy).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::app::app;
use crate::keys;
use crate::settings::{HighlightMatch, SearchMode, Settings, SortBy};

fn update(f: impl FnOnce(&mut Settings)) {
    app().update_settings(f);
}

fn current() -> Settings {
    app().settings.borrow().clone()
}

fn switch(title: &str, subtitle: Option<&str>, get: fn(&Settings) -> bool, set: fn(&mut Settings, bool)) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(title).active(get(&current())).build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    row.connect_active_notify(move |r| {
        let v = r.is_active();
        update(|s| set(s, v));
    });
    row
}

fn combo<T: Copy + PartialEq + 'static>(
    title: &str,
    options: &'static [T],
    label: fn(T) -> &'static str,
    get: fn(&Settings) -> T,
    set: fn(&mut Settings, T),
) -> adw::ComboRow {
    let labels: Vec<&str> = options.iter().map(|o| label(*o)).collect();
    let model = gtk::StringList::new(&labels);
    let selected = options.iter().position(|o| *o == get(&current())).unwrap_or(0) as u32;
    let row = adw::ComboRow::builder().title(title).model(&model).selected(selected).build();
    row.connect_selected_notify(move |r| {
        if let Some(v) = options.get(r.selected() as usize).copied() {
            update(|s| set(s, v));
        }
    });
    row
}

fn spin(title: &str, min: f64, max: f64, get: fn(&Settings) -> u32, set: fn(&mut Settings, u32)) -> adw::SpinRow {
    let row = adw::SpinRow::with_range(min, max, 1.0);
    row.set_title(title);
    row.set_value(get(&current()) as f64);
    row.connect_value_notify(move |r| {
        let v = r.value().round() as u32;
        update(|s| set(s, v));
    });
    row
}

/// An editable list of strings (ignored apps, types, regular expressions).
fn list_group(
    title: &str,
    description: &str,
    placeholder: &str,
    get: fn(&Settings) -> Vec<String>,
    set: fn(&mut Settings, Vec<String>),
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).description(description).build();
    let entry = adw::EntryRow::builder().title(placeholder).show_apply_button(true).build();
    group.add(&entry);
    let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    fn rebuild(
        group: &adw::PreferencesGroup,
        rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
        get: fn(&Settings) -> Vec<String>,
        set: fn(&mut Settings, Vec<String>),
    ) {
        for row in rows.borrow_mut().drain(..) {
            group.remove(&row);
        }
        for value in get(&current()) {
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&value).as_str()).build();
            let remove = gtk::Button::from_icon_name("user-trash-symbolic");
            remove.add_css_class("flat");
            remove.set_valign(gtk::Align::Center);
            remove.set_tooltip_text(Some("Remove"));
            let (g, r) = (group.downgrade(), rows.clone());
            remove.connect_clicked(move |_| {
                let v = value.clone();
                update(|s| {
                    let list: Vec<String> = get(s).into_iter().filter(|x| *x != v).collect();
                    set(s, list);
                });
                if let Some(g) = g.upgrade() {
                    rebuild(&g, &r, get, set);
                }
            });
            row.add_suffix(&remove);
            group.add(&row);
            rows.borrow_mut().push(row);
        }
    }

    rebuild(&group, &rows, get, set);
    let (g, r) = (group.downgrade(), rows.clone());
    entry.connect_apply(move |e| {
        let value = e.text().trim().to_owned();
        if value.is_empty() {
            return;
        }
        update(|s| {
            let mut list = get(s);
            if !list.contains(&value) {
                list.push(value.clone());
            }
            set(s, list);
        });
        e.set_text("");
        if let Some(g) = g.upgrade() {
            rebuild(&g, &r, get, set);
        }
    });
    group
}

fn page(title: &str, icon: &str) -> adw::PreferencesPage {
    adw::PreferencesPage::builder().title(title).icon_name(icon).build()
}

fn group(title: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).build()
}

pub fn build() -> adw::PreferencesDialog {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    dialog.add(&general_page(&dialog));
    dialog.add(&storage_page());
    dialog.add(&appearance_page());
    dialog.add(&ignore_page());
    dialog.add(&advanced_page());
    dialog
}

fn general_page(dialog: &adw::PreferencesDialog) -> adw::PreferencesPage {
    let p = page("General", "preferences-system-symbolic");

    let startup = group("");
    let launch = adw::SwitchRow::builder()
        .title("Launch at login")
        .subtitle("Start in the background when you log in")
        .active(crate::app::autostart_enabled())
        .build();
    launch.connect_active_notify(|r| crate::app::set_autostart(r.is_active()));
    startup.add(&launch);
    p.add(&startup);

    let notifications = group("Notifications");
    notifications.add(&switch(
        "Notify when something is copied",
        None,
        |s| s.notify_on_copy,
        |s, v| s.notify_on_copy = v,
    ));
    notifications.add(&switch(
        "Show what was copied",
        Some("Turn off to keep copied text out of notifications"),
        |s| s.notification_shows_content,
        |s, v| s.notification_shows_content = v,
    ));
    p.add(&notifications);

    let window = group("Window");
    window.add(&switch(
        "Close after copying",
        Some("Hide the window once an item is copied"),
        |s| s.close_after_copy,
        |s, v| s.close_after_copy = v,
    ));
    window.add(&combo("Search", &SearchMode::ALL, SearchMode::label, |s| s.search_mode, |s, v| s.search_mode = v));
    let langs = app().ocr_langs.borrow().clone().or_else(crate::ocr::languages);
    let ocr = switch(
        "Search text in images",
        None,
        |s| s.recognize_text,
        |s, v| s.recognize_text = v,
    );
    match langs {
        Some(l) => ocr.set_subtitle(&format!("Recognized on this computer with Tesseract ({l})")),
        None => {
            ocr.set_subtitle("Install the tesseract-ocr package to enable this");
            ocr.set_sensitive(false);
        }
    }
    window.add(&ocr);
    p.add(&window);

    let shortcut = adw::PreferencesGroup::builder()
        .title("Global shortcut")
        .description("Open Clipnest from anywhere. GNOME asks you to confirm the shortcut once.")
        .build();
    let row = adw::ActionRow::builder().title("Open Clipnest").build();
    let a = app();
    if a.shortcuts_bound() {
        let trigger = a.shortcut_trigger.borrow().clone();
        row.set_subtitle(&trigger.map(|t| t.trim_start_matches("Press ").to_owned()).unwrap_or_else(|| "Set".to_owned()));
        let change = gtk::Button::with_label("Change…");
        change.set_valign(gtk::Align::Center);
        let d = dialog.downgrade();
        change.connect_clicked(move |_| {
            let d = d.clone();
            glib::spawn_future_local(async move {
                if app().configure_global_shortcut().await.is_err() {
                    if let Some(d) = d.upgrade() {
                        d.add_toast(adw::Toast::new("Change it in Settings → Apps → Clipnest"));
                    }
                }
            });
        });
        row.add_suffix(&change);
        let off = gtk::Button::with_label("Turn Off");
        off.set_valign(gtk::Align::Center);
        let r = row.downgrade();
        off.connect_clicked(move |b| {
            update(|s| s.global_shortcut = false);
            b.set_sensitive(false);
            if let Some(r) = r.upgrade() {
                r.set_subtitle("Off (takes effect now)");
            }
        });
        row.add_suffix(&off);
    } else {
        row.set_subtitle(&format!(
            "Off — suggested: {}",
            keys::accelerator_label(&current().popup_shortcut)
        ));
        let setup = gtk::Button::with_label("Set Up…");
        setup.set_valign(gtk::Align::Center);
        let r = row.downgrade();
        setup.connect_clicked(move |b| {
            b.set_sensitive(false);
            if let Some(r) = r.upgrade() {
                r.set_subtitle("Waiting for confirmation…");
            }
            app().bind_global_shortcut(true);
        });
        row.add_suffix(&setup);
    }
    shortcut.add(&row);
    p.add(&shortcut);
    p
}

fn storage_page() -> adw::PreferencesPage {
    let p = page("Storage", "drive-harddisk-symbolic");
    let save = adw::PreferencesGroup::builder()
        .title("Save")
        .description("Clipnest only keeps the kinds of data you choose.")
        .build();
    save.add(&switch("Text", None, |s| s.save_text, |s, v| s.save_text = v));
    save.add(&switch("Images", None, |s| s.save_images, |s, v| s.save_images = v));
    save.add(&switch("Files", None, |s| s.save_files, |s, v| s.save_files = v));
    p.add(&save);

    let history = group("History");
    let size = spin("Size", 1.0, 999.0, |s| s.history_size, |s, v| s.history_size = v);
    size.set_subtitle(&format!(
        "Items to keep, not counting pinned ones. Using {} on disk.",
        glib::format_size(app().history.borrow().storage_size())
    ));
    history.add(&size);
    history.add(&combo("Sort by", &SortBy::ALL, SortBy::label, |s| s.sort_by, |s, v| s.sort_by = v));
    p.add(&history);
    p
}

fn appearance_page() -> adw::PreferencesPage {
    let p = page("Appearance", "applications-graphics-symbolic");
    let list = group("List");
    list.add(&switch(
        "Show special symbols",
        Some("Show leading/trailing spaces, tabs and new lines as ·, ⇥ and ⏎"),
        |s| s.show_special_symbols,
        |s, v| s.show_special_symbols = v,
    ));
    list.add(&combo(
        "Highlight matches",
        &HighlightMatch::ALL,
        HighlightMatch::label,
        |s| s.highlight_match,
        |s, v| s.highlight_match = v,
    ));
    list.add(&spin("Image height", 16.0, 200.0, |s| s.image_max_height, |s, v| s.image_max_height = v));
    list.add(&switch(
        "Show color swatches",
        Some("For copied colors such as #ff6600"),
        |s| s.show_hex_color_swatch,
        |s, v| s.show_hex_color_swatch = v,
    ));
    p.add(&list);

    let panel = adw::PreferencesGroup::builder()
        .title("Panel icon")
        .description("Shown by desktops with a tray, e.g. Ubuntu (AppIndicator extension) and KDE.")
        .build();
    panel.add(&switch("Show icon in the top bar", None, |s| s.show_in_status_bar, |s, v| s.show_in_status_bar = v));
    p.add(&panel);
    p
}

fn ignore_page() -> adw::PreferencesPage {
    let p = page("Ignore", "action-unavailable-symbolic");
    let apps = list_group(
        "Applications",
        "Copies from these applications (X11 window class, e.g. “KeePassXC”) are not saved. \
         Wayland apps on GNOME don't say who copied, so this only works for X11 apps.",
        "Add application…",
        |s| s.ignored_apps.clone(),
        |s, v| s.ignored_apps = v,
    );
    apps.add(&switch(
        "Ignore all applications except listed",
        None,
        |s| s.ignore_all_apps_except_listed,
        |s, v| s.ignore_all_apps_except_listed = v,
    ));
    p.add(&apps);

    let types = list_group(
        "Clipboard types",
        "Copies offering any of these types are never read. Password managers such as \
         KeePassXC mark passwords with “x-kde-passwordManagerHint”.",
        "Add type…",
        |s| s.ignored_types.clone(),
        |s, v| s.ignored_types = v,
    );
    let reset = gtk::Button::with_label("Reset");
    reset.add_css_class("flat");
    reset.set_tooltip_text(Some("Reset to the default list"));
    reset.connect_clicked(|_| update(|s| s.ignored_types = crate::settings::default_ignored_types()));
    types.set_header_suffix(Some(&reset));
    p.add(&types);

    p.add(&list_group(
        "Regular expressions",
        "Text matching any of these is not saved, e.g. ^\\d{6}$ for one-time codes.",
        "Add regular expression…",
        |s| s.ignore_regexp.clone(),
        |s, v| s.ignore_regexp = v,
    ));
    p
}

fn advanced_page() -> adw::PreferencesPage {
    let p = page("Advanced", "applications-engineering-symbolic");
    let g = group("");
    g.add(&switch(
        "Pause",
        Some("Stop saving new copies. Clipnest keeps running."),
        |s| s.ignore_events,
        |s, v| {
            s.ignore_events = v;
            s.ignore_only_next_event = false;
        },
    ));
    g.add(&switch("Clear history on quit", None, |s| s.clear_on_quit, |s, v| s.clear_on_quit = v));
    g.add(&switch(
        "Clear the current clipboard too",
        Some("When clearing the history, also empty the clipboard"),
        |s| s.clear_system_clipboard,
        |s, v| s.clear_system_clipboard = v,
    ));
    g.add(&switch(
        "Ask before clearing",
        None,
        |s| !s.suppress_clear_alert,
        |s, v| s.suppress_clear_alert = !v,
    ));
    p.add(&g);
    p
}
