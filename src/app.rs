//! Application glue: settings, history, clipboard watcher, window, tray, notifications.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::clipboard::x11::{self, Command, Event, WatchConfig};
use crate::config::{self, APP_ID};
use crate::history::{Added, History};
use crate::inspect;
use crate::model::{self, Content, Kind};
use crate::notify::{Notice, Notifier, Urgency};
use crate::ocr::Ocr;
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
    /// What the clipboard holds as far as we know (`Item::fingerprint`), so a
    /// sensitive copy is only cleared while it is still there.
    clipboard_now: Cell<Option<u64>>,
    /// Paste in order: the items (oldest first) and which one is on the clipboard.
    sequence: RefCell<Option<(Vec<i64>, usize)>>,
    ocr: RefCell<Option<Ocr>>,
    /// Tesseract languages in use, when text recognition is running.
    pub ocr_langs: RefCell<Option<String>>,
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
        clipboard_now: Cell::new(None),
        sequence: RefCell::new(None),
        ocr: RefCell::new(None),
        ocr_langs: RefCell::new(None),
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
            let mut history = self.history.borrow_mut();
            history.limit_size(&s);
            history.refresh_inspection(&s);
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
        if self.settings.borrow().recognize_text {
            self.start_ocr();
        }
        self.update_tray();
        if self.settings.borrow().global_shortcut {
            self.bind_global_shortcut(false);
        }
    }

    fn shutdown(&self) {
        // Don't leave a sensitive copy behind on the clipboard.
        if self.clipboard_holds_secret() {
            self.send(Command::Clear);
            std::thread::sleep(Duration::from_millis(200));
        }
        if self.settings.borrow().clear_on_quit {
            self.history.borrow_mut().clear();
        }
        self.tray.borrow_mut().take();
    }

    fn clipboard_holds_secret(&self) -> bool {
        let Some(now) = self.clipboard_now.get() else { return false };
        self.history.borrow().items().iter().any(|i| i.is_ephemeral() && i.fingerprint() == Some(now))
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
            ("win.sequence", &["<Alt>o"]),
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
                        let mut summary = item.summary(100);
                        if item.hidden > 0 {
                            summary = inspect::reveal(&summary);
                        }
                        let note = match (item.expires_at(), item.danger) {
                            (Some(at), _) => format!("   (sensitive, removed in {} s)", seconds_left(at)),
                            (None, Some(why)) => format!("   (dangerous: {why})"),
                            (None, None) => String::new(),
                        };
                        print(format!("{:>3}  {pin}{summary}{note}", i + 1));
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
            Some("sequence") if args.get(1).map(String::as_str) == Some("stop") => self.stop_sequence(),
            Some("sequence") => match nth(args.get(1)) {
                Some(id) => {
                    let ids = self.sequence_from(id);
                    self.start_sequence(ids);
                }
                None => {
                    cmd.printerr_literal("usage: clipnest sequence N | stop\n");
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
                self.clipboard_now.set(None);
                self.consume_ignore_next();
            }
            Event::SequenceNext { next, total } => self.on_sequence_next(next, total),
            Event::SequenceDone { pasted, total } => self.on_sequence_done(pasted, total),
            Event::Copied { contents, application } => {
                self.clipboard_now.set(model::fingerprint_of(&contents));
                self.on_copied(contents, application);
            }
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
        self.queue_ocr(id);
        self.schedule_expiry(id);
        self.history_changed();
        self.announce_copy(id);
    }

    /// The notification for a new copy: a warning for hidden characters, a note for
    /// sensitive items, or the usual "Copied".
    fn announce_copy(self: &Rc<Self>, id: i64) {
        let s = self.settings.borrow().clone();
        let (hidden, danger, sensitive) = {
            let history = self.history.borrow();
            let Some(item) = history.get(id) else { return };
            (item.hidden > 0, item.danger.is_some(), item.is_ephemeral())
        };
        if (hidden && s.warn_hidden_characters) || (danger && s.warn_dangerous_commands) {
            self.warn_suspicious(id);
        } else if s.notify_on_copy {
            if sensitive {
                self.notify_secret(id);
            } else {
                self.notify_item("Copied", id);
            }
        }
    }

    /// "API key copied — not saved to disk, removed in 30 s".
    fn notify_secret(&self, id: i64) {
        let shows_content = self.settings.borrow().notification_shows_content;
        let (summary, body) = {
            let history = self.history.borrow();
            let Some(item) = history.get(id) else { return };
            let Some(secret) = &item.secret else { return };
            let when = secret
                .expires_at
                .map(|at| format!("Not saved to disk, removed in {} s", seconds_left(at)))
                .unwrap_or_default();
            let body = match (shows_content, item.is_image_item()) {
                (true, false) => format!("{}\n{when}", item.summary(140)),
                _ => when,
            };
            let summary = if item.is_image_item() {
                format!("{} found in a copied image", secret.label)
            } else {
                format!("{} copied", secret.label)
            };
            (summary, body)
        };
        self.notifier.notify(&summary, &body);
    }

    /// What "copy cleaned" puts on the clipboard for an item, and the button label
    /// for it; `None` when there is nothing to clean.
    fn cleaned_text(&self, id: i64) -> Option<(String, &'static str)> {
        let s = self.settings.borrow().clone();
        let history = self.history.borrow();
        let item = history.get(id)?;
        let text = item.text()?;
        let hidden = item.hidden > 0 && s.warn_hidden_characters;
        let danger = item.danger.is_some() && s.warn_dangerous_commands;
        let without_hidden = if hidden { inspect::clean(&text) } else { text.clone() };
        let cleaned = if danger { without_hidden.trim_end_matches(['\n', '\r']).to_owned() } else { without_hidden.clone() };
        let label = match (without_hidden != text, cleaned != without_hidden) {
            (false, false) => return None,
            (true, false) => "Copy Without Them",
            (false, true) => "Copy Without Line Break",
            (true, true) => "Copy Cleaned",
        };
        Some((cleaned, label))
    }

    /// A warning that stays until dismissed (hidden characters, dangerous command),
    /// with actions to copy a cleaned version or to look at it. With "clean
    /// suspicious copies" on, the clipboard is cleaned right away instead.
    fn warn_suspicious(self: &Rc<Self>, id: i64) {
        let s = self.settings.borrow().clone();
        let cleaned = self.cleaned_text(id);
        let auto = s.clean_suspicious_copies && cleaned.is_some();
        let (summary, body) = {
            let history = self.history.borrow();
            let Some(item) = history.get(id) else { return };
            let mut lines = vec![inspect::reveal(&item.summary(120))];
            let danger = item.danger.filter(|_| s.warn_dangerous_commands);
            if let Some(why) = danger {
                lines.push(format!("{why}."));
            }
            if item.hidden > 0 && s.warn_hidden_characters {
                let mut what = inspect::describe(&inspect::find_hidden(&item.text().unwrap_or_default()));
                if let Some(first) = what.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                lines.push(format!("Hidden: {what}."));
            }
            if auto {
                lines.push("Cleaned on the clipboard.".to_owned());
            }
            let summary = if danger.is_some() { "Dangerous command copied" } else { "Hidden characters in copied text" };
            (summary, lines.join("\n"))
        };
        if auto {
            if let Some((text, _)) = &cleaned {
                self.copy_text(text);
            }
        }
        let clean_label = cleaned.as_ref().filter(|_| !auto).map(|(_, label)| *label);
        let mut actions: Vec<(&str, &str)> = Vec::new();
        if let Some(label) = clean_label {
            actions.push(("clean", label));
        }
        actions.push(("default", "Show"));
        let weak = Rc::downgrade(self);
        self.notifier.show(
            Notice { summary, body: &body, urgency: Urgency::Critical, transient: false, actions: &actions },
            Some(Rc::new(move |action: &str, token: Option<String>| {
                let Some(app) = weak.upgrade() else { return };
                match action {
                    "clean" => app.copy_cleaned(id),
                    _ => app.show_item(id, token),
                }
            })),
        );
    }

    /// Opens the window with an item selected.
    pub fn show_item(self: &Rc<Self>, id: i64, token: Option<String>) {
        if let Some(w) = self.window() {
            w.present(token);
            w.select_item(id);
        }
    }

    // ---- paste in order -----------------------------------------------------------

    /// Item `id` and everything copied after it, oldest first (for "paste in order").
    pub fn sequence_from(&self, id: i64) -> Vec<i64> {
        let history = self.history.borrow();
        let Some(start) = history.get(id) else { return Vec::new() };
        if start.is_pinned() {
            return vec![id];
        }
        let mut items: Vec<&model::Item> = history
            .items()
            .iter()
            .filter(|i| !i.is_pinned() && i.last_copied_at >= start.last_copied_at)
            .collect();
        items.sort_by_key(|i| i.last_copied_at);
        items.iter().map(|i| i.id).collect()
    }

    /// Puts the first item on the clipboard; each paste (Ctrl+V in any app) moves
    /// on to the next one.
    pub fn start_sequence(self: &Rc<Self>, ids: Vec<i64>) {
        let items: Vec<(i64, x11::Contents)> = {
            let history = self.history.borrow();
            ids.iter()
                .map(|id| {
                    let contents = history.clipboard_contents(*id, false);
                    (*id, contents.into_iter().map(|(m, d)| (m, Arc::new(d.to_vec()))).collect::<x11::Contents>())
                })
                .filter(|(_, c)| !c.is_empty())
                .collect()
        };
        if items.is_empty() {
            return;
        }
        *self.sequence.borrow_mut() = Some((items.iter().map(|(id, _)| *id).collect(), 0));
        self.send(Command::Sequence(items.into_iter().map(|(_, c)| c).collect()));
    }

    pub fn stop_sequence(&self) {
        self.send(Command::StopSequence);
    }

    /// (position of the item on the clipboard, 1-based; number of items).
    pub fn sequence_status(&self) -> Option<(usize, usize)> {
        self.sequence.borrow().as_ref().map(|(ids, next)| (next + 1, ids.len()))
    }

    fn on_sequence_next(self: &Rc<Self>, next: usize, total: usize) {
        let id = {
            let mut seq = self.sequence.borrow_mut();
            let Some((ids, pos)) = seq.as_mut() else { return };
            *pos = next;
            ids.get(next).copied()
        };
        let summary = id.and_then(|id| self.history.borrow().get(id).map(|i| (i.fingerprint(), i.summary(100))));
        if let Some((fingerprint, text)) = summary {
            self.clipboard_now.set(fingerprint);
            let title = if next == 0 {
                format!("Paste in order: {total} items")
            } else {
                format!("Next: {} of {total}", next + 1)
            };
            let body = if next == 0 { format!("Paste as usual (Ctrl+V); each paste moves to the next item.\n1: {text}") } else { text };
            self.notifier.notify(&title, &body);
        }
        if let Some(id) = id {
            self.renew_secret(id);
        }
        self.sequence_changed();
    }

    fn on_sequence_done(self: &Rc<Self>, pasted: usize, total: usize) {
        if self.sequence.borrow_mut().take().is_none() {
            return;
        }
        if pasted >= total {
            self.notifier.notify("Pasted all items in order", &format!("{total} items. The last one stays on the clipboard."));
        } else {
            self.notifier.notify("Stopped pasting in order", &format!("{pasted} of {total} items were pasted."));
        }
        self.sequence_changed();
    }

    fn sequence_changed(self: &Rc<Self>) {
        if let Some(w) = self.window() {
            w.update_banner();
        }
        self.update_tray_menu();
    }

    // ---- sensitive items --------------------------------------------------------

    /// Arranges for `expire_secrets` to run when this item's time is up.
    fn schedule_expiry(self: &Rc<Self>, id: i64) {
        let Some(at) = self.history.borrow().get(id).and_then(|i| i.expires_at()) else { return };
        let delay = (at - model::now_ms()).max(0) as u64 + 50;
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            if let Some(app) = weak.upgrade() {
                app.expire_secrets();
            }
        });
    }

    /// Removes sensitive items whose time is up, and empties the clipboard if it
    /// still holds one of them.
    fn expire_secrets(self: &Rc<Self>) {
        let gone = self.history.borrow_mut().expire(model::now_ms() + 20);
        if gone.is_empty() {
            return;
        }
        if let Some(now) = self.clipboard_now.get() {
            if gone.iter().any(|i| i.fingerprint() == Some(now)) {
                self.send(Command::Clear);
                self.clipboard_now.set(None);
            }
        }
        {
            let mut thumbs = self.tray_thumbs.borrow_mut();
            for item in &gone {
                thumbs.remove(&item.id);
            }
        }
        if let Some(w) = self.window() {
            w.forget(&gone.iter().map(|i| i.id).collect::<Vec<_>>());
        }
        crate::debug!("removed {} sensitive item(s)", gone.len());
        self.history_changed();
    }

    /// Deletes saved items that look sensitive. Returns how many.
    pub fn delete_saved_secrets(self: &Rc<Self>) -> usize {
        let count = {
            let mut settings = self.settings.borrow_mut();
            let count = self.history.borrow_mut().delete_saved_secrets(&mut settings);
            settings.save();
            count
        };
        self.history_changed();
        count
    }

    /// Copies an item's text without its hidden characters (and records that copy).
    pub fn copy_cleaned(self: &Rc<Self>, id: i64) {
        let Some((cleaned, _)) = self.cleaned_text(id) else { return };
        self.copy_text(&cleaned);
        if self.settings.borrow().notify_on_copy {
            let preview = self.history.borrow().get(self.last_copied.get().unwrap_or(0)).map(|i| i.summary(140));
            self.notifier.notify("Copied cleaned text", &preview.unwrap_or_default());
        }
    }

    /// Button label for `copy_cleaned`, if the item has anything to clean.
    pub fn clean_label(&self, id: i64) -> Option<&'static str> {
        self.cleaned_text(id).map(|(_, label)| label)
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
        // "Copy plain" on a picture copies the text recognized in it.
        let image_text = {
            let history = self.history.borrow();
            history.get(id).filter(|i| plain && i.is_image_item()).and_then(|i| i.ocr_text.clone())
        };
        if let Some(text) = image_text.filter(|t| !t.is_empty()) {
            self.clipboard_now.set(Some(model::content_hash(text.as_bytes())));
            self.send(Command::SetClipboard(vec![(model::MIME_TEXT.to_owned(), Arc::new(text.into_bytes()))]));
            self.last_copied.set(Some(id));
            self.renew_secret(id);
            return;
        }
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
        self.clipboard_now.set(self.history.borrow().get(id).and_then(|i| i.fingerprint()));
        self.history.borrow_mut().bump(id);
        self.last_copied.set(Some(id));
        self.renew_secret(id);
        self.history_changed();
    }

    /// A sensitive item copied again stays (and is cleared from the clipboard) for
    /// another full period.
    fn renew_secret(self: &Rc<Self>, id: i64) {
        let s = self.settings.borrow().clone();
        if self.history.borrow_mut().renew_secret(id, &s).is_some() {
            self.schedule_expiry(id);
        }
    }

    /// Copies arbitrary text (Enter on a search without matches) and records it.
    pub fn copy_text(self: &Rc<Self>, text: &str) {
        let data = text.as_bytes().to_vec();
        self.clipboard_now.set(Some(model::content_hash(&data)));
        self.send(Command::SetClipboard(vec![(model::MIME_TEXT.to_owned(), Arc::new(data.clone()))]));
        let added = {
            let mut settings = self.settings.borrow_mut();
            self.history.borrow_mut().add(vec![Content::new(model::MIME_TEXT, data)], None, &mut settings)
        };
        let (Added::New(id) | Added::Existing(id)) = added;
        self.last_copied.set(Some(id));
        self.schedule_expiry(id);
        self.history_changed();
    }

    // ---- text recognition -------------------------------------------------

    fn start_ocr(self: &Rc<Self>) {
        if self.ocr.borrow().is_some() {
            return;
        }
        let Some((ocr, results, langs)) = Ocr::start() else {
            crate::debug!("tesseract not found; images won't be searchable");
            return;
        };
        crate::debug!("recognizing text in images with tesseract ({langs})");
        *self.ocr.borrow_mut() = Some(ocr);
        *self.ocr_langs.borrow_mut() = Some(langs);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((id, text)) = results.recv().await {
                let Some(app) = weak.upgrade() else { break };
                let s = app.settings.borrow().clone();
                let became_sensitive = app.history.borrow_mut().set_ocr_text(id, text, &s).is_some();
                if became_sensitive {
                    // A screenshot of a key, say: off the disk, and out of the tray menu.
                    app.tray_thumbs.borrow_mut().remove(&id);
                    if let Some(w) = app.window() {
                        w.forget(&[id]);
                    }
                    app.schedule_expiry(id);
                    app.history_changed();
                    if s.notify_on_copy {
                        app.notify_secret(id);
                    }
                } else if let Some(w) = app.window() {
                    w.refresh();
                }
            }
        });
        let pending = self.history.borrow().pending_ocr();
        for id in pending {
            self.queue_ocr(id);
        }
    }

    fn stop_ocr(&self) {
        self.ocr.borrow_mut().take();
        self.ocr_langs.borrow_mut().take();
    }

    fn queue_ocr(&self, id: i64) {
        let ocr = self.ocr.borrow();
        let Some(ocr) = ocr.as_ref() else { return };
        let image = {
            let history = self.history.borrow();
            let Some(item) = history.get(id).filter(|i| i.is_image_item() && i.ocr_text.is_none()) else { return };
            item.image_content().and_then(|c| history.content_data(c)).map(|d| d.to_vec())
        };
        if let Some(image) = image {
            ocr.queue(id, image);
        }
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
        if old.protect_secrets != new.protect_secrets {
            self.history.borrow_mut().refresh_inspection(&new);
        }
        if old.history_size != new.history_size || old.sort_by != new.sort_by {
            self.history.borrow_mut().limit_size(&new);
        }
        if old.image_max_height != new.image_max_height {
            if let Some(w) = self.window() {
                w.clear_thumbnails();
            }
        }
        if old.recognize_text != new.recognize_text {
            if new.recognize_text {
                self.start_ocr();
            } else {
                self.stop_ocr();
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
        let mut entries = vec![MenuEntry::item("Open Clipnest", || app().show_window(None))];
        if let Some((next, total)) = self.sequence_status() {
            entries.push(MenuEntry::item(format!("Stop Pasting in Order ({next} of {total})"), || app().stop_sequence()));
        }
        entries.push(MenuEntry::Separator);
        {
            let history = self.history.borrow();
            let (pinned, unpinned) = history.ordered(&s);
            let recent: Vec<i64> = pinned.into_iter().chain(unpinned.into_iter().take(10)).collect();
            if recent.is_empty() {
                entries.push(MenuEntry::disabled("Nothing copied yet"));
            }
            for id in recent {
                if let Some(item) = history.get(id) {
                    // Sensitive pictures get no thumbnail.
                    let thumb = if item.is_image_item() && item.secret.is_none() {
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
                        None if item.is_image_item() && item.secret.is_some() => "Sensitive image".to_owned(),
                        None if (item.hidden > 0 && s.warn_hidden_characters)
                            || (item.danger.is_some() && s.warn_dangerous_commands) =>
                        {
                            format!("⚠ {}", inspect::reveal(&item.summary(48)))
                        }
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

/// Whole seconds until `at` (ms since the epoch), rounded up.
pub fn seconds_left(at: i64) -> i64 {
    ((at - model::now_ms()).max(0) + 999) / 1000
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
