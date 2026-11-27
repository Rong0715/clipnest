//! App-wide constants and XDG paths.

use std::path::PathBuf;

pub const APP_ID: &str = "io.github.Rong0715.Clipnest";
pub const APP_NAME: &str = "Clipnest";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ICON_NAME: &str = "io.github.Rong0715.Clipnest";
pub const SYMBOLIC_ICON_NAME: &str = "io.github.Rong0715.Clipnest-symbolic";

/// Embedded copy of the tray glyph, used when the icon is not installed in the theme.
pub const SYMBOLIC_ICON_SVG: &str =
    include_str!("../data/icons/hicolor/symbolic/apps/io.github.Rong0715.Clipnest-symbolic.svg");

fn dir(base: PathBuf) -> PathBuf {
    let d = base.join("clipnest");
    if !d.exists() {
        let _ = std::fs::create_dir_all(&d);
        set_private(&d);
    }
    d
}

/// Clipboard history is private: keep our directories and files owner-only.
pub fn set_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

pub fn data_dir() -> PathBuf {
    dir(gtk::glib::user_data_dir())
}

pub fn config_dir() -> PathBuf {
    dir(gtk::glib::user_config_dir())
}

pub fn database_path() -> PathBuf {
    data_dir().join("history.sqlite3")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn autostart_path() -> PathBuf {
    gtk::glib::user_config_dir()
        .join("autostart")
        .join(format!("{APP_ID}.desktop"))
}

pub fn debug_enabled() -> bool {
    std::env::var_os("CLIPNEST_DEBUG").is_some()
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::config::debug_enabled() {
            eprintln!("[clipnest] {}", format!($($arg)*));
        }
    };
}
