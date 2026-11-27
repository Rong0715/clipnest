//! History items and their clipboard contents (Maccy's `HistoryItem` / `HistoryItemContent`).

use std::rc::Rc;

pub const MIME_TEXT: &str = "text/plain;charset=utf-8";
pub const MIME_HTML: &str = "text/html";
pub const MIME_RTF: &str = "text/rtf";
pub const MIME_URI_LIST: &str = "text/uri-list";
pub const MIME_GNOME_FILES: &str = "x-special/gnome-copied-files";

/// Image formats we keep, in order of preference.
pub const IMAGE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/avif",
    "image/bmp",
    "image/tiff",
];

pub const FILE_MIMES: &[&str] = &[MIME_URI_LIST, MIME_GNOME_FILES];
pub const RICH_TEXT_MIMES: &[&str] = &[MIME_HTML, MIME_RTF];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Files,
}

pub fn kind_of(mime: &str) -> Option<Kind> {
    if mime == MIME_TEXT || RICH_TEXT_MIMES.contains(&mime) {
        Some(Kind::Text)
    } else if IMAGE_MIMES.contains(&mime) {
        Some(Kind::Image)
    } else if FILE_MIMES.contains(&mime) {
        Some(Kind::Files)
    } else {
        None
    }
}

/// Stable 64-bit FNV-1a hash, stored in the database for duplicate detection.
pub fn content_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Clone, Debug)]
pub struct Content {
    /// Row id in the `contents` table, 0 while not stored.
    pub id: i64,
    pub mime: String,
    pub size: usize,
    pub hash: u64,
    /// `None` when the (large) blob is still on disk; see `Storage::load_content`.
    pub data: Option<Rc<[u8]>>,
}

impl Content {
    pub fn new(mime: impl Into<String>, data: Vec<u8>) -> Content {
        Content {
            id: 0,
            mime: mime.into(),
            size: data.len(),
            hash: content_hash(&data),
            data: Some(Rc::from(data)),
        }
    }

    pub fn same_value(&self, other: &Content) -> bool {
        self.mime == other.mime && self.size == other.size && self.hash == other.hash
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: i64,
    pub contents: Vec<Content>,
    pub title: String,
    pub application: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub first_copied_at: i64,
    pub last_copied_at: i64,
    pub number_of_copies: i64,
    pub pin: Option<String>,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Item {
    pub fn new(contents: Vec<Content>, application: Option<String>) -> Item {
        let now = now_ms();
        Item {
            id: 0,
            contents,
            title: String::new(),
            application,
            first_copied_at: now,
            last_copied_at: now,
            number_of_copies: 1,
            pin: None,
        }
    }

    pub fn is_pinned(&self) -> bool {
        self.pin.is_some()
    }

    pub fn content(&self, mime: &str) -> Option<&Content> {
        self.contents.iter().find(|c| c.mime == mime)
    }

    fn loaded_str(&self, mime: &str) -> Option<String> {
        let data = self.content(mime)?.data.as_ref()?;
        Some(String::from_utf8_lossy(data).into_owned())
    }

    pub fn text(&self) -> Option<String> {
        self.loaded_str(MIME_TEXT)
    }

    pub fn image_content(&self) -> Option<&Content> {
        IMAGE_MIMES.iter().find_map(|m| self.content(m))
    }

    pub fn has_image(&self) -> bool {
        self.image_content().is_some()
    }

    #[cfg(test)]
    pub fn has_rich_text(&self) -> bool {
        RICH_TEXT_MIMES.iter().any(|m| self.content(m).is_some())
    }

    /// `file://` URIs of copied files, if any.
    pub fn file_uris(&self) -> Vec<String> {
        if let Some(list) = self.loaded_str(MIME_URI_LIST) {
            let uris: Vec<String> = list
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect();
            if !uris.is_empty() {
                return uris;
            }
        }
        if let Some(list) = self.loaded_str(MIME_GNOME_FILES) {
            return list
                .lines()
                .skip(1) // "copy" or "cut"
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
        }
        Vec::new()
    }

    /// Text shown in the list and searched over (Maccy's `previewableText`).
    pub fn previewable_text(&self) -> String {
        let files = self.file_uris();
        if !files.is_empty() {
            return files
                .iter()
                .map(|uri| uri_to_display(uri))
                .collect::<Vec<_>>()
                .join("\n");
        }
        if let Some(text) = self.text().filter(|t| !t.is_empty()) {
            return text;
        }
        if let Some(rtf) = self.loaded_str(MIME_RTF) {
            let text = rtf_to_text(&rtf);
            if !text.trim().is_empty() {
                return text;
            }
        }
        if let Some(html) = self.loaded_str(MIME_HTML) {
            let text = html_to_text(&html);
            if !text.trim().is_empty() {
                return text;
            }
        }
        self.title.clone()
    }

    pub fn generate_title(&self, show_special_symbols: bool) -> String {
        if self.has_image() && self.file_uris().is_empty() && self.text().is_none() {
            return String::new();
        }
        let text: String = self
            .previewable_text()
            .chars()
            .filter(|c| *c != '\u{FFFC}')
            .take(1_000)
            .collect();
        if show_special_symbols {
            special_symbols(&text)
        } else {
            text.trim().to_owned()
        }
    }

    /// True if every content of `other` is also present here (Maccy's `supersedes`).
    pub fn supersedes(&self, other: &Item) -> bool {
        !other.contents.is_empty()
            && other
                .contents
                .iter()
                .all(|c| self.contents.iter().any(|mine| mine.same_value(c)))
    }

    /// Single-line summary used by the tray menu and the CLI.
    pub fn summary(&self, max_chars: usize) -> String {
        let source = if self.title.is_empty() {
            if self.has_image() {
                "Image".to_owned()
            } else {
                self.previewable_text()
            }
        } else {
            self.title.clone()
        };
        let one_line: String = source
            .replace(['\n', '\r', '\t'], " ")
            .replace('⏎', " ")
            .replace('⇥', " ");
        let one_line = one_line.trim();
        if one_line.chars().count() > max_chars {
            let mut s: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
            s.push('…');
            s
        } else {
            one_line.to_owned()
        }
    }
}

fn special_symbols(text: &str) -> String {
    let leading = text.len() - text.trim_start_matches(' ').len();
    let trailing = text.len() - text.trim_end_matches(' ').len();
    let mut out = String::with_capacity(text.len() + 8);
    if leading == text.len() {
        // Only spaces.
        return "·".repeat(text.chars().count());
    }
    out.push_str(&"·".repeat(leading));
    out.push_str(&text[leading..text.len() - trailing]);
    out.push_str(&"·".repeat(trailing));
    out.replace("\r\n", "⏎").replace(['\n', '\r'], "⏎").replace('\t', "⇥")
}

/// `file:///home/me/a%20b.txt` -> `/home/me/a b.txt`
pub fn uri_to_display(uri: &str) -> String {
    let decoded = percent_decode(uri);
    decoded
        .strip_prefix("file://")
        .map(|p| p.to_owned())
        .unwrap_or(decoded)
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Very small HTML-to-text conversion, used only when no plain text was copied.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    'outer: while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let lower: String = after.chars().take(10).collect::<String>().to_ascii_lowercase();
        let end = after.find('>').map(|e| e + 1).unwrap_or(after.len());
        for block in ["<script", "<style"] {
            if lower.starts_with(block) {
                let close = format!("</{}", &block[1..]);
                if let Some(pos) = after.to_ascii_lowercase().find(&close) {
                    let skip = after[pos..].find('>').map(|e| pos + e + 1).unwrap_or(after.len());
                    rest = &after[skip..];
                    continue 'outer;
                }
            }
        }
        let tag = after[..end].to_ascii_lowercase();
        if tag.starts_with("<br")
            || tag.starts_with("</p")
            || tag.starts_with("</div")
            || tag.starts_with("</li")
            || tag.starts_with("</tr")
            || tag.starts_with("</h")
        {
            out.push('\n');
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after[..after.len().min(12)].find(';') {
            let entity = &after[1..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                e if e.starts_with("#x") || e.starts_with("#X") => {
                    u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32)
                }
                e if e.starts_with('#') => e[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

/// Minimal RTF-to-text conversion (control words, groups, \'hh and \uN escapes).
pub fn rtf_to_text(rtf: &str) -> String {
    const SKIP_DESTINATIONS: &[&str] = &[
        "fonttbl", "colortbl", "stylesheet", "info", "pict", "header", "footer", "themedata",
        "colorschememapping", "latentstyles", "datastore", "xmlnstbl", "listtable",
        "listoverridetable", "rsidtbl", "generator", "mmathPr",
    ];
    let chars: Vec<char> = rtf.chars().collect();
    let mut out = String::new();
    // Stack of "skip this group" flags.
    let mut skip_stack: Vec<bool> = vec![false];
    let mut uc_skip = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let skipping = *skip_stack.last().unwrap_or(&false);
        match c {
            '{' => {
                skip_stack.push(skipping);
                i += 1;
            }
            '}' => {
                if skip_stack.len() > 1 {
                    skip_stack.pop();
                }
                i += 1;
            }
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    break;
                }
                let n = chars[i];
                if n == '\'' && i + 2 < chars.len() {
                    let hex: String = chars[i + 1..i + 3].iter().collect();
                    if let Ok(b) = u8::from_str_radix(&hex, 16) {
                        if !skipping && uc_skip == 0 {
                            out.push(cp1252(b));
                        }
                        uc_skip = uc_skip.saturating_sub(1);
                    }
                    i += 3;
                } else if n == '*' {
                    if let Some(flag) = skip_stack.last_mut() {
                        *flag = true;
                    }
                    i += 1;
                } else if n.is_ascii_alphabetic() {
                    let start = i;
                    while i < chars.len() && chars[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    let num_start = i;
                    if i < chars.len() && (chars[i] == '-' || chars[i].is_ascii_digit()) {
                        i += 1;
                        while i < chars.len() && chars[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                    let param: Option<i32> = chars[num_start..i].iter().collect::<String>().parse().ok();
                    if i < chars.len() && chars[i] == ' ' {
                        i += 1;
                    }
                    if SKIP_DESTINATIONS.contains(&word.as_str()) {
                        if let Some(flag) = skip_stack.last_mut() {
                            *flag = true;
                        }
                        continue;
                    }
                    if skipping {
                        continue;
                    }
                    match word.as_str() {
                        "par" | "line" | "row" => out.push('\n'),
                        "tab" | "cell" => out.push('\t'),
                        "u" => {
                            if let Some(p) = param {
                                let code = if p < 0 { p + 65536 } else { p } as u32;
                                if let Some(ch) = char::from_u32(code) {
                                    out.push(ch);
                                }
                                uc_skip = 1;
                            }
                        }
                        "emdash" => out.push('—'),
                        "endash" => out.push('–'),
                        "bullet" => out.push('•'),
                        "lquote" => out.push('‘'),
                        "rquote" => out.push('’'),
                        "ldblquote" => out.push('“'),
                        "rdblquote" => out.push('”'),
                        _ => {}
                    }
                } else {
                    // Escaped literal: \\, \{, \}, or other control symbols.
                    if !skipping && matches!(n, '\\' | '{' | '}') {
                        out.push(n);
                    } else if !skipping && n == '~' {
                        out.push('\u{a0}');
                    }
                    i += 1;
                }
            }
            '\r' | '\n' => i += 1,
            _ => {
                if !skipping {
                    if uc_skip > 0 {
                        uc_skip -= 1;
                    } else {
                        out.push(c);
                    }
                }
                i += 1;
            }
        }
    }
    out
}

fn cp1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    if (0x80..0xa0).contains(&b) {
        HIGH[(b - 0x80) as usize]
    } else {
        b as char
    }
}

/// Parses `#RGB`, `#RGBA`, `#RRGGBB` and `#RRGGBBAA` into RGBA floats.
pub fn parse_hex_color(text: &str) -> Option<[f64; 4]> {
    let t = text.trim();
    let hex = t.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let expand = |s: &str| -> Option<f64> {
        let v = u8::from_str_radix(s, 16).ok()?;
        Some(v as f64 / 255.0)
    };
    let digits: Vec<String> = match hex.len() {
        3 | 4 => hex.chars().map(|c| format!("{c}{c}")).collect(),
        6 | 8 => hex
            .as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect(),
        _ => return None,
    };
    let r = expand(&digits[0])?;
    let g = expand(&digits[1])?;
    let b = expand(&digits[2])?;
    let a = digits.get(3).map(|d| expand(d)).unwrap_or(Some(1.0))?;
    Some([r, g, b, a])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_item(s: &str) -> Item {
        Item::new(vec![Content::new(MIME_TEXT, s.as_bytes().to_vec())], None)
    }

    #[test]
    fn titles_show_special_symbols() {
        let item = text_item("  hello\tworld\nbye  ");
        assert_eq!(item.generate_title(true), "··hello⇥world⏎bye··");
        assert_eq!(item.generate_title(false), "hello\tworld\nbye");
    }

    #[test]
    fn supersedes_checks_all_contents() {
        let mut rich = text_item("x");
        rich.contents.push(Content::new(MIME_HTML, b"<b>x</b>".to_vec()));
        assert!(rich.supersedes(&text_item("x")));
        assert!(!text_item("x").supersedes(&rich));
        assert!(!rich.supersedes(&text_item("y")));
    }

    #[test]
    fn files_are_listed_as_paths() {
        let item = Item::new(
            vec![Content::new(MIME_URI_LIST, b"file:///tmp/a%20b.txt\r\nfile:///tmp/c\r\n".to_vec())],
            None,
        );
        assert_eq!(item.generate_title(false), "/tmp/a b.txt\n/tmp/c");
    }

    #[test]
    fn html_and_rtf_fallbacks() {
        assert_eq!(html_to_text("<p>Hi &amp; <b>bye</b></p><style>x{}</style>"), "Hi & bye\n");
        assert_eq!(rtf_to_text(r"{\rtf1\ansi{\fonttbl\f0 Helvetica;}\f0 Hello\par W\'f6rld}"), "Hello\nWörld");
    }

    #[test]
    fn hex_colors() {
        assert_eq!(parse_hex_color("#fff"), Some([1.0, 1.0, 1.0, 1.0]));
        assert!(parse_hex_color("#12345").is_none());
        assert!(parse_hex_color("cafe").is_none());
    }
}
