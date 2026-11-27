//! User preferences (a trimmed version of Maccy's `Defaults.Keys`) and small persisted state.

use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchMode {
    Exact,
    Fuzzy,
    Regexp,
    Mixed,
}

impl SearchMode {
    pub const ALL: [SearchMode; 4] = [Self::Exact, Self::Fuzzy, Self::Regexp, Self::Mixed];
    pub fn label(self) -> &'static str {
        match self {
            Self::Exact => "Exact",
            Self::Fuzzy => "Fuzzy",
            Self::Regexp => "Regex",
            Self::Mixed => "Mixed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SortBy {
    LastCopiedAt,
    FirstCopiedAt,
    NumberOfCopies,
}

impl SortBy {
    pub const ALL: [SortBy; 3] = [Self::LastCopiedAt, Self::FirstCopiedAt, Self::NumberOfCopies];
    pub fn label(self) -> &'static str {
        match self {
            Self::LastCopiedAt => "Last copied",
            Self::FirstCopiedAt => "First copied",
            Self::NumberOfCopies => "Number of copies",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HighlightMatch {
    Bold,
    Color,
    Italic,
    Underline,
}

impl HighlightMatch {
    pub const ALL: [HighlightMatch; 4] = [Self::Bold, Self::Color, Self::Italic, Self::Underline];
    pub fn label(self) -> &'static str {
        match self {
            Self::Bold => "Bold",
            Self::Color => "Color",
            Self::Italic => "Italic",
            Self::Underline => "Underline",
        }
    }
}

pub fn default_ignored_types() -> Vec<String> {
    // Hints that password managers and similar apps put on the clipboard.
    vec![
        "x-kde-passwordManagerHint".into(),
        "application/x-nspasteboard-concealed-type".into(),
        "application/x-nspasteboard-transient-type".into(),
    ]
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    // General
    pub notify_on_copy: bool,
    pub notification_shows_content: bool,
    pub close_after_copy: bool,
    pub search_mode: SearchMode,
    /// Recognize text in copied images so they can be searched (needs Tesseract).
    pub recognize_text: bool,
    /// Register a global shortcut through the desktop portal (opt-in).
    pub global_shortcut: bool,
    /// Preferred trigger for that shortcut, in GTK accelerator syntax.
    pub popup_shortcut: String,
    // Storage
    pub save_text: bool,
    pub save_images: bool,
    pub save_files: bool,
    pub history_size: u32,
    pub sort_by: SortBy,
    // Appearance
    pub show_special_symbols: bool,
    pub highlight_match: HighlightMatch,
    pub image_max_height: u32,
    pub show_hex_color_swatch: bool,
    pub show_in_status_bar: bool,
    pub show_preview: bool,
    pub window_width: i32,
    pub window_height: i32,
    // Ignore
    pub ignore_events: bool,
    pub ignore_only_next_event: bool,
    pub ignored_apps: Vec<String>,
    pub ignore_all_apps_except_listed: bool,
    pub ignored_types: Vec<String>,
    pub ignore_regexp: Vec<String>,
    // Privacy
    /// Keep copies that look like passwords, keys or tokens in memory only and
    /// remove them (from the clipboard too) after `secret_lifetime` seconds.
    pub protect_secrets: bool,
    pub secret_lifetime: u32,
    /// Mark and warn about invisible characters and terminal escapes in copied text.
    pub warn_hidden_characters: bool,
    /// Mark and warn about a few clearly dangerous commands (`rm -rf ~`, `mkfs`…).
    pub warn_dangerous_commands: bool,
    /// Replace suspicious copies on the clipboard right away: hidden characters
    /// removed and, for dangerous commands, the final line break (opt-in).
    pub clean_suspicious_copies: bool,
    // Advanced
    pub clear_on_quit: bool,
    pub clear_system_clipboard: bool,
    pub suppress_clear_alert: bool,
    pub pin_order: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            notify_on_copy: true,
            notification_shows_content: true,
            close_after_copy: true,
            search_mode: SearchMode::Exact,
            recognize_text: true,
            global_shortcut: false,
            popup_shortcut: "<Shift><Super>c".into(),
            save_text: true,
            save_images: true,
            save_files: true,
            history_size: 200,
            sort_by: SortBy::LastCopiedAt,
            show_special_symbols: true,
            highlight_match: HighlightMatch::Bold,
            image_max_height: 40,
            show_hex_color_swatch: true,
            show_in_status_bar: true,
            show_preview: true,
            window_width: 820,
            window_height: 560,
            ignore_events: false,
            ignore_only_next_event: false,
            ignored_apps: Vec::new(),
            ignore_all_apps_except_listed: false,
            ignored_types: default_ignored_types(),
            ignore_regexp: Vec::new(),
            protect_secrets: true,
            secret_lifetime: 30,
            warn_hidden_characters: true,
            warn_dangerous_commands: true,
            clean_suspicious_copies: false,
            clear_on_quit: false,
            clear_system_clipboard: false,
            suppress_clear_alert: false,
            pin_order: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        match std::fs::read_to_string(config::settings_path()) {
            Ok(text) => serde_json::from_str::<Settings>(&text)
                .map(Settings::sanitized)
                .unwrap_or_else(|e| {
                    eprintln!("clipnest: ignoring unreadable settings file: {e}");
                    Settings::default()
                }),
            Err(_) => Settings::default(),
        }
    }

    fn sanitized(mut self) -> Settings {
        self.history_size = self.history_size.clamp(1, 999);
        self.image_max_height = self.image_max_height.clamp(16, 200);
        self.window_width = self.window_width.clamp(360, 4000);
        self.window_height = self.window_height.clamp(300, 4000);
        self.secret_lifetime = self.secret_lifetime.clamp(5, 3600);
        self
    }

    /// How long a sensitive copy is kept, in milliseconds.
    pub fn secret_lifetime_ms(&self) -> i64 {
        self.secret_lifetime.clamp(5, 3600) as i64 * 1000
    }

    pub fn save(&self) {
        let path = config::settings_path();
        let tmp = path.with_extension("json.tmp");
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if std::fs::write(&tmp, text).is_ok() {
                    config::set_private(&tmp);
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
            Err(e) => eprintln!("clipnest: could not save settings: {e}"),
        }
    }
}
