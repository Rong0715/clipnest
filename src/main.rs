//! Clipnest — a lightweight clipboard manager for Linux, modeled on Maccy for macOS.

mod app;
mod clipboard;
mod config;
mod history;
mod keys;
mod model;
mod notify;
mod ocr;
mod portal;
mod search;
mod settings;
mod storage;
mod tray;
mod ui;

use gtk::gio;
use gtk::glib;
use gtk::prelude::ToVariant;

const HELP: &str = "\
Clipnest — clipboard history for Linux

Usage: clipnest [COMMAND]

Without a command, starts Clipnest if needed and opens the history window.
Clipnest keeps running in the background to remember what you copy.

Commands:
  --background         start without opening the window (used at login)
  show | toggle | hide open, toggle or close the window
  list [N]             print the N most recent items (default 20)
  get N                print item N (numbers as shown by `list`)
  copy N [--plain]     put item N back on the clipboard
  delete N             delete item N
  clear | clear-all    clear the history (clear-all also removes pinned items)
  pause | resume       stop or resume saving new copies
  ignore-next          don't save the next copy
  preferences          open the preferences
  quit                 quit Clipnest
  --version            print the version

Set CLIPNEST_DEBUG=1 for diagnostic output.
";

/// Is another instance already running on this session bus?
fn is_running() -> bool {
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return false };
    conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "NameHasOwner",
        Some(&(config::APP_ID,).to_variant()),
        Some(glib::VariantTy::new("(b)").expect("valid type")),
        gio::DBusCallFlags::NONE,
        2000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|v| v.get::<(bool,)>())
    .map(|(b,)| b)
    .unwrap_or(false)
}

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str);
    match command {
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            return glib::ExitCode::SUCCESS;
        }
        Some("-V" | "--version" | "version") => {
            println!("clipnest {}", config::VERSION);
            return glib::ExitCode::SUCCESS;
        }
        _ => {}
    }
    let running = is_running();
    let starts_app = matches!(command, None | Some("--background" | "show" | "toggle" | "preferences"));
    if !running && !starts_app {
        eprintln!("clipnest: not running. Start it with `clipnest` or `clipnest --background`.");
        return glib::ExitCode::FAILURE;
    }
    if !running {
        // Tell the desktop portal who we are before GTK talks to it.
        portal::register_host_app(config::APP_ID);
    }
    app::run()
}
