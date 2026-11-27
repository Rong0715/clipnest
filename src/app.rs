//! Application glue: settings, history, clipboard watcher, window, tray, notifications.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::clipboard::x11::{self, Command, Event, WatchConfig};
use crate::config::{self, APP_ID};
use crate::history::{Added, History};
use crate::model::{self, Content, Kind};
use crate::notify::Notifier;
use crate::portal::GlobalShortcuts;
use crate::settings::Settings;
use crate::tray::{MenuEntry, Tray};
use crate::ui::window::MainWindow;

pub struct App {
    pub gtk_app: adw::Application,
    pub settings: RefCell<Settings>,
    pub history: RefCell<History>,
    clipboard: RefCell<Option<x11::Handle>>,
    clipboard_retries: Cell<u32>,
    window: RefCell<Option<Rc<MainWindow>>>,
    tray: RefCell<Option<Tray>>,
    shortcuts: RefCell<Option<GlobalShortcuts>>,
    pub shortcut_trigger: RefCell<Option<String>>,
    notifier: Notifier,
    preferences: RefCell<Option<glib::WeakRef<adw::PreferencesDialog>>>,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    regexes: RefCell<Vec<regex::Regex>>,
    last_copied: Cell<Option<i64>>,
    /// PNG thumbnail + size of image items, for the tray menu.
    tray_thumbs: RefCell<std::collections::HashMap<i64, Option<(Rc<Vec<u8>>, i32, i32)>>>,
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// The running application (main thread only).
pub fn app() -> Rc<App> {
    APP.with(|a| a.borrow().clone()).expect("application not initialized")
}

pub fn run() -> glib::ExitCode {
    let gtk_app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let instance = Rc::new(App {
        gtk_app: gtk_app.clone(),
        settings: RefCell::new(Settings::default()),
        history: RefCell::new(History::empty()),
        clipboard: RefCell::new(None),
        clipboard_retries: Cell::new(0),
        window: RefCell::new(None),
        tray: RefCell::new(None),
        shortcuts: RefCell::new(None),
        shortcut_trigger: RefCell::new(None),
        notifier: Notifier::default(),
        preferences: RefCell::new(None),
        hold: RefCell::new(None),
        regexes: RefCell::new(Vec::new()),
        last_copied: Cell::new(None),
        tray_thumbs: RefCell::new(std::collections::HashMap::new()),
    });
    APP.with(|a| *a.borrow_mut() = Some(instance));
    gtk_app.connect_startup(|_| app().startup());
    gtk_app.connect_command_line(|_, cmd| app().command_line(cmd));
    gtk_app.connect_activate(|_| app().show_window(None));
    gtk_app.connect_shutdown(|_| app().shutdown());
    let code = gtk_app.run();
    APP.with(|a| a.borrow_mut().take());
    code
}

impl App {
    // ---- lifecycle ----------------------------------------------------------

    fn startup(self: &Rc<Self>) {
        *self.settings.borrow_mut() = Settings::load();
        *self.history.borrow_mut() = History::open();
        {
            let s = self.settings.borrow().clone();
            self.history.borrow_mut().limit_size(&s);
        }
        self.compile_regexes();

        if let Some(display) = gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(include_str!("ui/style.css"));
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
            if let Some(dir) = dev_icon_dir() {
                gtk::IconTheme::for_display(&display).add_search_path(dir);
            }
        }
        gtk::Window::set_default_icon_name(config::ICON_NAME);
        self.setup_actions();

        *self.window.borrow_mut() = Some(MainWindow::new(&self.gtk_app));
        // Keep running in the background to watch the clipboard.
        *self.hold.borrow_mut() = Some(self.gtk_app.hold());
        self.start_clipboard();
        self.update_tray();
        if self.settings.borrow().global_shortcut {
            self.bind_global_shortcut(false);
        }
    }

    fn shutdown(&self) {
        if self.settings.borrow().clear_on_quit {
            self.history.borrow_mut().clear();
        }
        self.tray.borrow_mut().take();
    }

    fn setup_actions(self: &Rc<Self>) {
        let simple = |name: &str, f: fn(&Rc<App>)| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(move |_, _| f(&app()));
            self.gtk_app.add_action(&action);
        };
        simple("preferences", |a| a.open_preferences());
        simple("about", |a| a.open_about());
        simple("quit", |a| a.quit());
        simple("ignore-next", |a| a.ignore_next());
        simple("show", |a| a.show_window(None));
        let pause = gio::SimpleAction::new_stateful("pause", None, &self.settings.borrow().ignore_events.to_variant());
        pause.connect_activate(|_, _| {
            let a = app();
            let paused = a.settings.borrow().ignore_events && !a.settings.borrow().ignore_only_next_event;
            a.set_paused(!paused);
        });
        self.gtk_app.add_action(&pause);

        let accels: &[(&str, &[&str])] = &[
            ("app.quit", &["<Control>q"]),
            ("app.preferences", &["<Control>comma"]),
            ("window.close", &["<Control>w"]),
            ("win.focus-search", &["<Control>f"]),
            ("win.toggle-preview", &["<Control>space"]),
            ("win.pin", &["<Alt>p"]),
            ("win.delete", &["<Alt>BackSpace"]),
            ("win.clear", &["<Control><Shift>BackSpace"]),
            ("win.show-help", &["<Control>question", "F1"]),
        ];
        for (action, keys) in accels {
            self.gtk_app.set_accels_for_action(action, keys);
        }
    }

    fn command_line(self: &Rc<Self>, cmd: &gio::ApplicationCommandLine) -> glib::ExitCode {
        let args: Vec<String> = cmd.arguments().iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
        let token = cmd
            .platform_data()
            .and_then(|d| {
                let dict = glib::VariantDict::new(Some(&d));
                dict.lookup::<String>("activation-token").ok().flatten()
                    .or_else(|| dict.lookup::<String>("desktop-startup-id").ok().flatten())
            });
        let print = |text: String| cmd.print_literal(&format!("{text}\n"));
        let nth = |n: Option<&String>| -> Option<i64> {
            let n: usize = n?.parse().ok()?;
            let s = self.settings.borrow().clone();
            let (pinned, unpinned) = self.history.borrow().ordered(&s);
            pinned.into_iter().chain(unpinned).nth(n.checked_sub(1)?)
        };
        match args.first().map(String::as_str) {
            None | Some("show") => self.show_window(token),
            Some("--background") => {}
            Some("toggle") => self.toggle_window(token),
            Some("hide") => {
                if let Some(w) = self.window() {
                    w.close();
                }
            }
            Some("preferences") => self.open_preferences(),
            Some("clear") => self.clear(false),
            Some("clear-all") => self.clear(true),
            Some("pause") => self.set_paused(true),
            Some("resume") => self.set_paused(false),
            Some("ignore-next") => self.ignore_next(),
            Some("list") => {
                let limit: usize = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(20);
                let s = self.settings.borrow().clone();
                let history = self.history.borrow();
                let (pinned, unpinned) = history.ordered(&s);
                for (i, id) in pinned.iter().chain(unpinned.iter()).take(limit).enumerate() {
                    if let Some(item) = history.get(*id) {
                        let pin = item.pin.as_deref().map(|p| format!("[{p}] ")).unwrap_or_default();
                        print(format!("{:>3}  {pin}{}", i + 1, item.summary(100)));
                    }
                }
            }
            Some("get") => match nth(args.get(1)) {
                Some(id) => {
                    let text = self.history.borrow().get(id).map(|i| i.previewable_text()).unwrap_or_default();
                    print(text);
                }
                None => {
                    cmd.printerr_literal("usage: clipnest get N (see `clipnest list`)\n");
                    return glib::ExitCode::FAILURE;
                }
            },
            Some("copy") => match nth(args.get(1)) {
                Some(id) => self.copy_item(id, args.iter().any(|a| a == "--plain")),
                None => {
                    cmd.printerr_literal("usage: clipnest copy N [--plain]\n");
                    return glib::ExitCode::FAILURE;
                }
            },
            Some("delete") => match nth(args.get(1)) {
                Some(id) => self.delete_item(id),
                None => {
                    cmd.printerr_literal("usage: clipnest delete N\n");
                    return glib::ExitCode::FAILURE;
                }
            },
            Some("quit") => self.quit(),
            Some(other) => {
                cmd.printerr_literal(&format!("clipnest: unknown command “{other}”. Try `clipnest --help`.\n"));
                return glib::ExitCode::FAILURE;
            }
        }
        glib::ExitCode::SUCCESS
    }

    pub fn quit(self: &Rc<Self>) {
        if let Some(w) = self.window() {
            w.close();
        }
        self.hold.borrow_mut().take();
        self.gtk_app.quit();
    }

    // ---- window -------------------------------------------------------------

    pub fn window(&self) -> Option<Rc<MainWindow>> {
        self.window.borrow().clone()
    }

    pub fn show_window(self: &Rc<Self>, token: Option<String>) {
        if let Some(w) = self.window() {
            w.present(token);
        }
    }

    fn toggle_window(self: &Rc<Self>, token: Option<String>) {
        if let Some(w) = self.window() {
            w.toggle(token);
        }
    }

    fn history_changed(self: &Rc<Self>) {
        if let Some(w) = self.window() {
            w.refresh();
        }
        self.update_tray_menu();
    }

    // ---- clipboard ------------------------------------------------------------

    fn watch_config(&self) -> WatchConfig {
        let s = self.settings.borrow();
        WatchConfig {
            save_text: s.save_text,
            save_images: s.save_images,
            save_files: s.save_files,
            ignored_types: s.ignored_types.clone(),
            enabled: !s.ignore_events,
        }
    }

    fn send(&self, cmd: Command) {
        if let Some(handle) = self.clipboard.borrow().as_ref() {
            handle.send(cmd);
        }
    }

    fn start_clipboard(self: &Rc<Self>) {
        match x11::start(self.watch_config()) {
            Ok((handle, events)) => {
                self.clipboard_retries.set(0);
                *self.clipboard.borrow_mut() = Some(handle);
                let weak = Rc::downgrade(self);
                glib::spawn_future_local(async move {
                    while let Ok(event) = events.recv().await {
                        let Some(app) = weak.upgrade() else { break };
                        app.on_clipboard_event(event);
                    }
                });
            }
            Err(e) => {
                let retries = self.clipboard_retries.get();
                if retries == 0 {
                    eprintln!("clipnest: cannot watch the clipboard yet: {e}");
                }
                self.clipboard_retries.set(retries + 1);
                let delay = (5 * (retries + 1)).min(60);
                let weak = Rc::downgrade(self);
                glib::timeout_add_seconds_local_once(delay, move || {
                    if let Some(app) = weak.upgrade() {
                        app.start_clipboard();
                    }
                });
            }
        }
    }

    fn on_clipboard_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Ignored => {
                self.consume_ignore_next();
            }
            Event::Copied { contents, application } => self.on_copied(contents, application),
            Event::Failed(e) => {
                eprintln!("clipnest: clipboard watcher stopped: {e}; restarting");
                self.clipboard.borrow_mut().take();
                let weak = Rc::downgrade(self);
                glib::timeout_add_seconds_local_once(3, move || {
                    if let Some(app) = weak.upgrade() {
                        app.start_clipboard();
                    }
                });
            }
        }
    }

    /// Returns true when this copy must be skipped (paused or "ignore next copy").
    fn consume_ignore_next(self: &Rc<Self>) -> bool {
        let (ignore, only_next) = {
            let s = self.settings.borrow();
            (s.ignore_events, s.ignore_only_next_event)
        };
        if ignore && only_next {
            self.update_settings(|s| {
                s.ignore_events = false;
                s.ignore_only_next_event = false;
            });
        }
        ignore
    }

    fn on_copied(self: &Rc<Self>, contents: Vec<(String, Vec<u8>)>, application: Option<String>) {
        if self.consume_ignore_next() {
            return;
        }
        let s = self.settings.borrow().clone();
        if let Some(app_name) = &application {
            let listed = s.ignored_apps.iter().any(|a| a.eq_ignore_ascii_case(app_name));
            if listed != s.ignore_all_apps_except_listed {
                return;
            }
        } else if s.ignore_all_apps_except_listed && !s.ignored_apps.is_empty() {
            // Unknown source app (e.g. Wayland apps on GNOME): keep the copy.
        }
        let text = contents
            .iter()
            .find(|(m, _)| m == model::MIME_TEXT)
            .map(|(_, d)| String::from_utf8_lossy(d).into_owned());
        if let Some(text) = &text {
            if self.regexes.borrow().iter().any(|re| re.is_match(text)) {
                return;
            }
        }
        // Like Maccy: skip copies of nothing but whitespace (unless there is rich text).
        let has_rich = contents.iter().any(|(m, d)| model::RICH_TEXT_MIMES.contains(&m.as_str()) && !d.is_empty());
        let has_other = contents
            .iter()
            .any(|(m, d)| matches!(model::kind_of(m), Some(Kind::Image | Kind::Files)) && !d.is_empty());
        if text.as_ref().map(|t| t.trim().is_empty()).unwrap_or(true) && !has_rich && !has_other {
            return;
        }
        let contents: Vec<Content> =
            contents.into_iter().filter(|(_, d)| !d.is_empty()).map(|(m, d)| Content::new(m, d)).collect();
        if contents.is_empty() {
            return;
        }
        let added = {
            let mut settings = self.settings.borrow_mut();
            self.history.borrow_mut().add(contents, application, &mut settings)
        };
        let id = match added {
            Added::New(id) | Added::Existing(id) => id,
        };
        self.history_changed();
        if s.notify_on_copy {
            self.notify_item("Copied", id);
        }
    }

    fn notify_item(&self, summary: &str, id: i64) {
        let s = self.settings.borrow().clone();
        let body = {
            let history = self.history.borrow();
            let Some(item) = history.get(id) else { return };
            if !s.notification_shows_content {
                if item.has_image() && item.title.is_empty() {
                    "An image".to_owned()
                } else if !item.file_uris().is_empty() {
                    "Files".to_owned()
                } else {
                    "Text".to_owned()
                }
            } else if item.has_image() && item.title.is_empty() {
                let size = item.image_content().map(|c| glib::format_size(c.size as u64).to_string()).unwrap_or_default();
                format!("Image ({size})")
            } else {
                item.summary(140)
            }
        };
        self.notifier.notify(summary, &body);
    }

    /// Called when the window closes right after copying from it (Maccy notifies too).
    pub fn notify_copied_from_history(&self) {
        if let (true, Some(id)) = (self.settings.borrow().notify_on_copy, self.last_copied.get()) {
            self.notify_item("Copied from history", id);
        }
    }

    /// Puts a history item back on the clipboard.
    pub fn copy_item(self: &Rc<Self>, id: i64, plain: bool) {
        let contents = self.history.borrow().clipboard_contents(id, plain);
        if contents.is_empty() {
            return;
        }
        let mut out: Vec<(String, Arc<Vec<u8>>)> =
            contents.into_iter().map(|(m, d)| (m, Arc::new(d.to_vec()))).collect();
        // Most apps paste PNG; offer it next to other image formats.
        if !out.iter().any(|(m, _)| m == "image/png") {
            if let Some((_, data)) = out.iter().find(|(m, _)| m.starts_with("image/")).cloned() {
                if let Some(png) = to_png(&data) {
                    out.push(("image/png".to_owned(), Arc::new(png)));
                }
            }
        }
        self.send(Command::SetClipboard(out));
        self.history.borrow_mut().bump(id);
        self.last_copied.set(Some(id));
        self.history_changed();
    }

    /// Copies arbitrary text (Enter on a search without matches) and records it.
    pub fn copy_text(self: &Rc<Self>, text: &str) {
        let data = text.as_bytes().to_vec();
        self.send(Command::SetClipboard(vec![(model::MIME_TEXT.to_owned(), Arc::new(data.clone()))]));
        let added = {
            let mut settings = self.settings.borrow_mut();
            self.history.borrow_mut().add(vec![Content::new(model::MIME_TEXT, data)], None, &mut settings)
        };
        let (Added::New(id) | Added::Existing(id)) = added;
        self.last_copied.set(Some(id));
        self.history_changed();
    }

    pub fn toggle_pin(self: &Rc<Self>, id: i64) {
        {
            let mut settings = self.settings.borrow_mut();
            self.history.borrow_mut().toggle_pin(id, &mut settings);
            settings.save();
        }
        self.history_changed();
    }

    pub fn move_pin(self: &Rc<Self>, id: i64, delta: i32) {
        {
            let mut settings = self.settings.borrow_mut();
            self.history.borrow_mut().move_pin(id, delta, &mut settings);
            settings.save();
        }
        self.history_changed();
    }

    pub fn delete_item(self: &Rc<Self>, id: i64) {
        {
            let mut settings = self.settings.borrow_mut();
            self.history.borrow_mut().delete(id, &mut settings);
            settings.save();
        }
        self.history_changed();
    }

    pub fn clear(self: &Rc<Self>, all: bool) {
        {
            let mut settings = self.settings.borrow_mut();
            if all {
                self.history.borrow_mut().clear_all(&mut settings);
            } else {
                self.history.borrow_mut().clear();
            }
            settings.save();
        }
        if self.settings.borrow().clear_system_clipboard {
            self.send(Command::Clear);
        }
        self.history_changed();
    }

    pub fn request_clear(self: &Rc<Self>, all: bool, parent: Option<&gtk::Widget>) {
        if self.settings.borrow().suppress_clear_alert {
            self.clear(all);
            return;
        }
        let heading = if all { "Clear the whole history?" } else { "Clear the history?" };
        let body = if all {
            "This removes every item, including pinned ones. You can't undo this."
        } else {
            "This removes every item except pinned ones. You can't undo this."
        };
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("clear", "Clear")]);
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_close_response("cancel");
        let check = gtk::CheckButton::with_label("Don't ask again");
        dialog.set_extra_child(Some(&check));
        let app = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "clear" {
                if check.is_active() {
                    app.update_settings(|s| s.suppress_clear_alert = true);
                }
                app.clear(all);
            }
        });
        dialog.present(parent);
    }

    pub fn set_paused(self: &Rc<Self>, paused: bool) {
        self.update_settings(|s| {
            s.ignore_events = paused;
            s.ignore_only_next_event = false;
        });
    }

    pub fn ignore_next(self: &Rc<Self>) {
        self.update_settings(|s| {
            s.ignore_events = true;
            s.ignore_only_next_event = true;
        });
    }

    // ---- settings -------------------------------------------------------------

    pub fn update_settings(self: &Rc<Self>, f: impl FnOnce(&mut Settings)) {
        let old = self.settings.borrow().clone();
        f(&mut self.settings.borrow_mut());
        let new = self.settings.borrow().clone();
        new.save();
        self.send(Command::Config(self.watch_config()));
        if old.ignore_regexp != new.ignore_regexp {
            self.compile_regexes();
        }
        if old.show_special_symbols != new.show_special_symbols {
            self.history.borrow_mut().refresh_titles(new.show_special_symbols);
        }
        if old.history_size != new.history_size || old.sort_by != new.sort_by {
            self.history.borrow_mut().limit_size(&new);
        }
        if old.image_max_height != new.image_max_height {
            if let Some(w) = self.window() {
                w.clear_thumbnails();
            }
        }
        if old.show_in_status_bar != new.show_in_status_bar {
            self.update_tray();
        }
        if old.ignore_events != new.ignore_events || old.ignore_only_next_event != new.ignore_only_next_event {
            if let Some(action) = self.gtk_app.lookup_action("pause").and_downcast::<gio::SimpleAction>() {
                action.set_state(&(new.ignore_events && !new.ignore_only_next_event).to_variant());
            }
        }
        if old.global_shortcut != new.global_shortcut && !new.global_shortcut {
            self.shortcuts.borrow_mut().take();
            self.shortcut_trigger.borrow_mut().take();
        }
        if old != new {
            self.history_changed();
        }
    }

    fn compile_regexes(&self) {
        let compiled = self
            .settings
            .borrow()
            .ignore_regexp
            .iter()
            .filter_map(|r| regex::Regex::new(r).ok())
            .collect();
        *self.regexes.borrow_mut() = compiled;
    }

    pub fn open_preferences(self: &Rc<Self>) {
        let parent = self.window().filter(|w| w.is_visible()).map(|w| w.window().clone());
        if let Some(existing) = self.preferences.borrow().as_ref().and_then(|w| w.upgrade()) {
            existing.present(parent.as_ref());
            return;
        }
        let dialog = crate::ui::preferences::build();
        *self.preferences.borrow_mut() = Some(dialog.downgrade());
        dialog.present(parent.as_ref());
    }

    pub fn open_about(self: &Rc<Self>) {
        let about = adw::AboutDialog::builder()
            .application_name(config::APP_NAME)
            .application_icon(config::ICON_NAME)
            .version(config::VERSION)
            .developer_name("Rong0715")
            .comments(
                "Lightweight clipboard manager: keeps what you copy and lets you search it, \
                 pin it and copy it again.\n\nModeled on Maccy for macOS by Alex Rodionov \
                 (maccy.app). Not affiliated with Maccy.\n\nGenerated entirely by AI (Claude Opus 5.5, max \
                 effort). Use with caution.",
            )
            .website("https://github.com/Rong0715/clipnest")
            .issue_url("https://github.com/Rong0715/clipnest/issues")
            .license_type(gtk::License::MitX11)
            .copyright("© 2026 Rong0715 · design based on Maccy © 2025 Alex Rodionov")
            .build();
        let parent = self.window().filter(|w| w.is_visible()).map(|w| w.window().clone());
        about.present(parent.as_ref());
    }

    // ---- global shortcut (opt-in) -----------------------------------------------

    pub fn shortcuts_bound(&self) -> bool {
        self.shortcuts.borrow().is_some()
    }

    /// Binds the global shortcut through the portal. GNOME asks the user once.
    pub fn bind_global_shortcut(self: &Rc<Self>, report: bool) {
        let preferred = self.settings.borrow().popup_shortcut.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (w1, w2) = (weak.clone(), weak.clone());
            let result = GlobalShortcuts::bind(
                &preferred,
                move |token| {
                    if let Some(app) = w1.upgrade() {
                        app.toggle_window(token);
                    }
                },
                move |trigger| {
                    if let Some(app) = w2.upgrade() {
                        *app.shortcut_trigger.borrow_mut() = trigger;
                    }
                },
            )
            .await;
            let Some(app) = weak.upgrade() else { return };
            match result {
                Ok(bound) => {
                    *app.shortcut_trigger.borrow_mut() = bound.trigger;
                    *app.shortcuts.borrow_mut() = Some(bound.shortcuts);
                    if !app.settings.borrow().global_shortcut {
                        app.update_settings(|s| s.global_shortcut = true);
                    }
                    if report {
                        app.notifier.notify("Shortcut ready", "Press it anywhere to open Clipnest.");
                    }
                }
                Err(e) => {
                    eprintln!("clipnest: global shortcut unavailable: {e}");
                    if report {
                        app.notifier.notify("No global shortcut", &format!("{e}. You can bind “clipnest toggle” in your keyboard settings."));
                    }
                }
            }
        });
    }

    pub async fn configure_global_shortcut(self: &Rc<Self>) -> Result<(), String> {
        let handle = self.shortcuts.borrow().as_ref().map(|s| s.handle());
        match handle {
            Some((conn, session)) => crate::portal::configure_shortcuts(&conn, &session).await,
            None => Err("no shortcut session".into()),
        }
    }

    // ---- tray ---------------------------------------------------------------

    fn update_tray(self: &Rc<Self>) {
        if !self.settings.borrow().show_in_status_bar {
            self.tray.borrow_mut().take();
            return;
        }
        if self.tray.borrow().is_none() {
            let weak = Rc::downgrade(self);
            // Tray hosts (e.g. the AppIndicator extension) only find icons installed in
            // the standard places; otherwise send the glyph as a pixmap.
            let icon_name = if icon_installed(config::SYMBOLIC_ICON_NAME) { config::SYMBOLIC_ICON_NAME } else { "" };
            match Tray::start(icon_name, Some(config::SYMBOLIC_ICON_SVG), String::new(), move |token| {
                if let Some(app) = weak.upgrade() {
                    app.toggle_window(token);
                }
            }) {
                Ok(tray) => *self.tray.borrow_mut() = Some(tray),
                Err(e) => eprintln!("clipnest: no panel icon: {e}"),
            }
        }
        self.update_tray_menu();
    }

    fn update_tray_menu(self: &Rc<Self>) {
        let tray = self.tray.borrow();
        let Some(tray) = tray.as_ref() else { return };
        let s = self.settings.borrow().clone();
        let mut entries = vec![MenuEntry::item("Open Clipnest", || app().show_window(None)), MenuEntry::Separator];
        {
            let history = self.history.borrow();
            let (pinned, unpinned) = history.ordered(&s);
            let recent: Vec<i64> = pinned.into_iter().chain(unpinned.into_iter().take(10)).collect();
            if recent.is_empty() {
                entries.push(MenuEntry::disabled("Nothing copied yet"));
            }
            for id in recent {
                if let Some(item) = history.get(id) {
                    let thumb = if item.has_image() && item.title.is_empty() {
                        self.tray_thumbs
                            .borrow_mut()
                            .entry(id)
                            .or_insert_with(|| {
                                let data = history.content_data(item.image_content()?)?;
                                tray_thumbnail(&data)
                            })
                            .clone()
                    } else {
                        None
                    };
                    let text = match &thumb {
                        Some((_, w, h)) => format!("Image ({w}×{h})"),
                        None => item.summary(48),
                    };
                    let label = match &item.pin {
                        Some(p) => format!("{text}   ⟨{}⟩", p.to_uppercase()),
                        None => text,
                    };
                    let icon = thumb.map(|(png, _, _)| png);
                    entries.push(MenuEntry::item(label, move || {
                        let app = app();
                        app.copy_item(id, false);
                        if app.settings.borrow().notify_on_copy {
                            app.notify_item("Copied from history", id);
                        }
                    })
                    .with_key(id)
                    .with_icon(icon));
                }
            }
        }
        let paused = s.ignore_events && !s.ignore_only_next_event;
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::check("Pause Saving", paused, move || app().set_paused(!paused)));
        entries.push(MenuEntry::item("Ignore Next Copy", || app().ignore_next()));
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::item("Preferences…", || app().open_preferences()));
        entries.push(MenuEntry::item("Quit Clipnest", || app().quit()));
        tray.set_menu(entries);
        tray.set_tooltip(if paused { "Clipboard history (paused)" } else { "Clipboard history" });
    }
}

/// A small PNG preview for the tray menu, plus the original size.
fn tray_thumbnail(data: &[u8]) -> Option<(Rc<Vec<u8>>, i32, i32)> {
    use gtk::gdk_pixbuf::prelude::*;
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    let original = Rc::new(Cell::new((0, 0)));
    let o = original.clone();
    loader.connect_size_prepared(move |loader, w, h| {
        o.set((w, h));
        let scale = (64.0 / w.max(h).max(1) as f64).min(1.0);
        loader.set_size(((w as f64 * scale) as i32).max(1), ((h as f64 * scale) as i32).max(1));
    });
    loader.write(data).ok()?;
    loader.close().ok()?;
    let png = loader.pixbuf()?.save_to_bufferv("png", &[]).ok()?;
    let (w, h) = original.get();
    Some((Rc::new(png), w, h))
}

/// Re-encodes an image as PNG.
fn to_png(data: &[u8]) -> Option<Vec<u8>> {
    let texture = gdk::Texture::from_bytes(&glib::Bytes::from(data)).ok()?;
    Some(texture.save_to_png_bytes().to_vec())
}

/// When running from the source tree, use the bundled icons.
fn dev_icon_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/icons");
    let exe = std::env::current_exe().ok()?;
    (exe.starts_with(env!("CARGO_MANIFEST_DIR")) && dir.exists()).then_some(dir)
}

/// Is the icon installed where every icon theme lookup (including the shell's) finds it?
fn icon_installed(name: &str) -> bool {
    std::iter::once(glib::user_data_dir())
        .chain(glib::system_data_dirs())
        .flat_map(|dir| {
            ["symbolic", "scalable"].map(|size| dir.join(format!("icons/hicolor/{size}/apps/{name}.svg")))
        })
        .any(|path| path.exists())
}

// ---- launch at login --------------------------------------------------------

fn desktop_exec_path() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "clipnest".to_owned());
    // Desktop-entry quoting rules.
    if exe.chars().any(|c| c.is_whitespace() || "\"'\\$`".contains(c)) {
        format!("\"{}\"", exe.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace('`', "\\`"))
    } else {
        exe
    }
}

pub fn autostart_enabled() -> bool {
    config::autostart_path().exists()
}

pub fn set_autostart(enabled: bool) {
    let path = config::autostart_path();
    if enabled {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName={name}\nComment=Clipboard history\nExec={exec} --background\n\
             Icon={icon}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
            name = config::APP_NAME,
            exec = desktop_exec_path(),
            icon = config::ICON_NAME
        );
        if let Err(e) = std::fs::write(&path, entry) {
            eprintln!("clipnest: could not enable launch at login: {e}");
        }
    } else {
        let _ = std::fs::remove_file(&path);
    }
}
