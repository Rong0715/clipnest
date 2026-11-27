//! Looks at copied text before Clipnest keeps it.
//!
//! * **Secrets**: API keys and tokens with a recognizable shape, private keys,
//!   passwords in links and in `KEY=value` lines, card and ID numbers. Items with
//!   secrets are kept in memory only and removed after a while (see `History`).
//! * **Hidden characters**: zero-width and text-direction characters, invisible
//!   Unicode tag text, data hidden in variation selectors, terminal escape
//!   sequences and other control characters, which make copied text (for example
//!   a command from a web page) differ from what it looks like.
//!
//! Everything runs locally and is heuristic: it lowers the risk, it is not a guarantee.

use std::sync::LazyLock;

use regex::Regex;

/// Longest text that is scanned; anything after this is not looked at.
const SCAN_LIMIT: usize = 4 * 1024 * 1024;

const DOTS: &str = "••••";

fn scan_window(text: &str) -> &str {
    if text.len() <= SCAN_LIMIT {
        return text;
    }
    let mut end = SCAN_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn shorten(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

// ---- secrets ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mask {
    /// Keep a known prefix (`sk-proj-`, `ghp_`…) and, for long values, the last four characters.
    Token,
    /// Keep the `-----BEGIN … PRIVATE KEY-----` line only.
    PrivateKey,
    /// Hide the whole value.
    Full,
    /// Keep the last four characters (card and ID numbers).
    LastFour,
}

struct Rule {
    label: &'static str,
    re: Regex,
    /// Capture group holding the secret (0: the whole match).
    group: usize,
    mask: Mask,
    check: Option<fn(&str) -> bool>,
}

/// A secret found in a text (byte range).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub label: &'static str,
    pub start: usize,
    pub end: usize,
    mask: Mask,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |label: &'static str, pattern: &str, group: usize, mask: Mask, check: Option<fn(&str) -> bool>| Rule {
        label,
        re: Regex::new(pattern).expect("valid secret pattern"),
        group,
        mask,
        check,
    };
    vec![
        rule(
            "Private key",
            r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----(?s:.*?)(?:-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----|\z)",
            0,
            Mask::PrivateKey,
            None,
        ),
        rule("Anthropic API key", r"\bsk-ant-[A-Za-z0-9_-]{20,}", 0, Mask::Token, None),
        // OpenAI, DeepSeek, Moonshot, DashScope and others use sk-…
        rule("API key", r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{20,}", 0, Mask::Token, Some(looks_like_sk_key)),
        rule("AWS access key", r"\b(?:AKIA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b", 0, Mask::Token, None),
        rule("GitHub token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,})", 0, Mask::Token, None),
        rule("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}", 0, Mask::Token, None),
        rule("Slack token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}", 0, Mask::Token, None),
        rule("Google API key", r"\bAIza[0-9A-Za-z_-]{35}", 0, Mask::Token, None),
        rule("Stripe key", r"\b[rs]k_(?:live|test)_[0-9A-Za-z]{20,}", 0, Mask::Token, None),
        rule("Hugging Face token", r"\bhf_[A-Za-z0-9]{30,}", 0, Mask::Token, Some(mixed_case)),
        rule("npm token", r"\bnpm_[A-Za-z0-9]{36}\b", 0, Mask::Token, None),
        rule("PyPI token", r"\bpypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{50,}", 0, Mask::Token, None),
        rule("JSON Web Token", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", 0, Mask::Token, None),
        rule(
            "Link with a password",
            r"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:([^\s/@]+)@",
            1,
            Mask::Full,
            Some(not_placeholder),
        ),
        // KEY=value, KEY: value, "key": "value" lines (.env, YAML, JSON, shell exports).
        rule(
            "Password or secret",
            r#"(?mi)^[ \t]*(?:export[ \t]+)?["']?[A-Za-z0-9_.-]*(?:secret|password|passwd|passphrase|token|api[_-]?key|access[_-]?key|private[_-]?key|credentials?)[A-Za-z0-9_.-]*["']?[ \t]*[=:][ \t]*["']?([^\s"'$<{}(),;]{8,})["']?[ \t]*,?[ \t]*\r?$"#,
            1,
            Mask::Full,
            Some(looks_like_secret_value),
        ),
    ]
});

/// `sk-…` is also used for ordinary words ("sk-learn-tutorial-2024"): keys have
/// digits and either capitals or a long run of hex.
fn looks_like_sk_key(s: &str) -> bool {
    let body = s.trim_start_matches("sk-");
    let body = ["proj-", "svcacct-", "admin-"].iter().find_map(|p| body.strip_prefix(p)).unwrap_or(body);
    let digit = body.bytes().any(|b| b.is_ascii_digit());
    let upper = body.bytes().any(|b| b.is_ascii_uppercase());
    let hex = body.len() >= 32 && body.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    digit && (upper || hex)
}

fn mixed_case(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_uppercase()) && s.bytes().any(|b| b.is_ascii_lowercase())
}

/// Documentation links like `postgres://user:password@localhost` are not secrets.
fn not_placeholder(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    let first = s.chars().next().unwrap_or(' ');
    !(["password", "pass", "passwd", "pwd", "secret", "changeme", "example"].contains(&lower.as_str())
        || s.starts_with(['<', '$', '{', '[', '%'])
        || s.chars().all(|c| c == first)
        || lower.contains("your"))
}

/// Values in `KEY=value` lines: real secrets mix letters and digits; references
/// (`OTHER_VAR`, `settings.KEY`), links, paths and examples don't count.
fn looks_like_secret_value(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_alphabetic())
        && !lower.contains("://")
        && !s.starts_with(['/', '~', '.'])
        && !["example", "your", "xxxx", "placeholder", "changeme", "redacted"].iter().any(|p| lower.contains(p))
}

fn luhn(digits: &[u8]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            let d = d as u32;
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum % 10 == 0
}

/// A whole copied text that is a payment card number (issuer prefix + Luhn check).
fn is_card_number(s: &str) -> bool {
    if s.len() > 40 || !s.bytes().all(|b| b.is_ascii_digit() || b == b' ' || b == b'-') {
        return false;
    }
    if s.contains("  ") || s.contains("--") || s.contains(" -") || s.contains("- ") {
        return false;
    }
    let digits: Vec<u8> = s.bytes().filter(u8::is_ascii_digit).map(|b| b - b'0').collect();
    if !(13..=19).contains(&digits.len()) {
        return false;
    }
    let two = digits[0] as u32 * 10 + digits[1] as u32;
    let four = two * 100 + digits[2] as u32 * 10 + digits[3] as u32;
    let issuer = match digits[0] {
        4 => true,
        5 => (51..=55).contains(&two),
        2 => (2221..=2720).contains(&four),
        3 => matches!(two, 30 | 34 | 35 | 36 | 37 | 38),
        6 => matches!(two, 60 | 62 | 64 | 65),
        _ => false,
    };
    issuer && luhn(&digits)
}

/// A whole copied text that is a Chinese resident ID number (GB 11643 check digit).
fn is_cn_id(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 18 || b[0] == b'0' || !b[..17].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let last = b[17].to_ascii_uppercase();
    if !(last.is_ascii_digit() || last == b'X') {
        return false;
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<u32>().unwrap_or(0);
    let (year, month, day) = (num(6..10), num(10..12), num(12..14));
    if !(1900..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    const WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
    const CHECK: &[u8; 11] = b"10X98765432";
    let sum: u32 = b[..17].iter().zip(WEIGHTS).map(|(d, w)| (d - b'0') as u32 * w).sum();
    CHECK[(sum % 11) as usize] == last
}

fn whole_item(text: &str) -> Option<Found> {
    let trimmed = text.trim();
    let start = text.len() - text.trim_start().len();
    let found = |label| Some(Found { label, start, end: start + trimmed.len(), mask: Mask::LastFour });
    if is_card_number(trimmed) {
        return found("Card number");
    }
    if is_cn_id(trimmed) {
        return found("ID card number");
    }
    None
}

/// Secrets in `text`, in order and without overlaps. With `whole`, the text as a
/// whole is also checked for card and ID numbers (not done for text read from images).
pub fn find_secrets(text: &str, whole: bool) -> Vec<Found> {
    let text = scan_window(text);
    let mut found = Vec::new();
    for rule in RULES.iter() {
        for caps in rule.re.captures_iter(text) {
            let Some(m) = caps.get(rule.group) else { continue };
            if rule.check.is_some_and(|check| !check(m.as_str())) {
                continue;
            }
            found.push(Found { label: rule.label, start: m.start(), end: m.end(), mask: rule.mask });
        }
    }
    if whole {
        found.extend(whole_item(text));
    }
    // Stable sort: for matches starting at the same place the earlier (more specific) rule wins.
    found.sort_by_key(|f| f.start);
    let mut kept: Vec<Found> = Vec::new();
    for f in found {
        if kept.last().is_none_or(|k| f.start >= k.end) {
            kept.push(f);
        }
    }
    kept
}

/// What kind of secret `text` contains, if any.
pub fn secret_label(text: &str, whole: bool) -> Option<&'static str> {
    find_secrets(text, whole).first().map(|f| f.label)
}

/// Prefixes that are safe and useful to keep visible.
const PREFIXES: &[&str] = &[
    "sk-ant-api03-", "sk-ant-", "sk-proj-", "sk-svcacct-", "sk-admin-", "sk-", "github_pat_", "ghp_", "gho_", "ghu_",
    "ghs_", "ghr_", "glpat-", "xoxa-", "xoxb-", "xoxo-", "xoxp-", "xoxr-", "xoxs-", "AKIA", "ASIA", "ABIA", "ACCA",
    "AIza", "sk_live_", "sk_test_", "rk_live_", "rk_test_", "hf_", "npm_", "pypi-", "eyJ",
];

fn masked(secret: &str, mask: Mask) -> String {
    match mask {
        Mask::Full => DOTS.to_owned(),
        Mask::PrivateKey => {
            let header_end = secret.get(5..).and_then(|s| s.find("-----")).map(|i| i + 10).unwrap_or(secret.len());
            format!("{} {DOTS}", &secret[..header_end.min(secret.len())])
        }
        Mask::LastFour => {
            let chars: Vec<char> = secret.chars().filter(char::is_ascii_alphanumeric).collect();
            let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
            format!("{DOTS} {tail}")
        }
        Mask::Token => {
            let prefix = PREFIXES.iter().filter(|p| secret.starts_with(**p)).map(|p| p.len()).max().unwrap_or(0);
            let count = secret.chars().count();
            let tail: String = if count >= prefix + 16 { secret.chars().skip(count - 4).collect() } else { String::new() };
            format!("{}{DOTS}{tail}", &secret[..prefix])
        }
    }
}

/// `text` with every secret replaced by a masked form such as `sk-proj-••••3fA9`.
pub fn mask(text: &str, whole: bool) -> String {
    let found = find_secrets(text, whole);
    if found.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for f in &found {
        out.push_str(&text[pos..f.start]);
        out.push_str(&masked(&text[f.start..f.end], f.mask));
        pos = f.end;
    }
    out.push_str(&text[pos..]);
    out
}

// ---- hidden characters ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HiddenKind {
    ZeroWidth,
    Direction,
    TagText,
    HiddenData,
    Escape,
    Control,
    Filler,
}

impl HiddenKind {
    fn describe(self, n: usize) -> String {
        let (one, many) = match self {
            HiddenKind::ZeroWidth => ("zero-width character", "zero-width characters"),
            HiddenKind::Direction => ("text-direction control", "text-direction controls"),
            HiddenKind::TagText => return if n == 1 { "invisible text".into() } else { format!("{n} pieces of invisible text") },
            HiddenKind::HiddenData => return if n == 1 { "hidden data".into() } else { format!("{n} blocks of hidden data") },
            HiddenKind::Escape => ("terminal escape sequence", "terminal escape sequences"),
            HiddenKind::Control => ("control character", "control characters"),
            HiddenKind::Filler => ("blank filler character", "blank filler characters"),
        };
        format!("{n} {}", if n == 1 { one } else { many })
    }
}

/// A run of hidden characters (byte range) and a short label to show instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hidden {
    pub start: usize,
    pub end: usize,
    pub kind: HiddenKind,
    pub label: String,
}

fn is_zero_width(c: char, has_mongolian: bool) -> bool {
    matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' | '\u{034F}' | '\u{2061}'..='\u{2064}')
        || (c == '\u{180E}' && !has_mongolian)
}

fn is_direction(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn is_tag(c: char) -> bool {
    ('\u{E0000}'..='\u{E007F}').contains(&c)
}

fn is_selector(c: char) -> bool {
    ('\u{FE00}'..='\u{FE0F}').contains(&c) || ('\u{E0100}'..='\u{E01EF}').contains(&c)
}

fn is_filler(c: char) -> bool {
    matches!(c, '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}')
}

fn is_rtl(c: char) -> bool {
    matches!(c, '\u{0590}'..='\u{08FF}' | '\u{FB1D}'..='\u{FDFF}' | '\u{FE70}'..='\u{FEFC}' | '\u{10800}'..='\u{10FFF}' | '\u{1E800}'..='\u{1EFFF}')
        && !is_direction(c)
}

fn is_jamo(c: char) -> bool {
    matches!(c, '\u{1100}'..='\u{115E}' | '\u{1161}'..='\u{11FF}' | '\u{3131}'..='\u{3163}' | '\u{3165}'..='\u{318E}' | '\u{A960}'..='\u{A97F}' | '\u{D7B0}'..='\u{D7FF}')
}

fn is_ideograph(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{20000}'..='\u{3134F}')
}

fn zero_width_name(c: char) -> String {
    match c {
        '\u{200B}' => "ZWSP".into(),
        '\u{200C}' => "ZWNJ".into(),
        '\u{200D}' => "ZWJ".into(),
        '\u{2060}' => "WJ".into(),
        '\u{FEFF}' => "BOM".into(),
        '\u{180E}' => "MVS".into(),
        '\u{034F}' => "CGJ".into(),
        c => format!("U+{:04X}", c as u32),
    }
}

fn direction_name(c: char) -> &'static str {
    match c {
        '\u{200E}' => "LRM",
        '\u{200F}' => "RLM",
        '\u{061C}' => "ALM",
        '\u{202A}' => "LRE",
        '\u{202B}' => "RLE",
        '\u{202C}' => "PDF",
        '\u{202D}' => "LRO",
        '\u{202E}' => "RLO",
        '\u{2066}' => "LRI",
        '\u{2067}' => "RLI",
        '\u{2068}' => "FSI",
        _ => "PDI",
    }
}

fn control_name(c: char) -> String {
    match c {
        '\0' => "NUL".into(),
        '\u{7}' => "BEL".into(),
        '\u{8}' => "BS".into(),
        '\u{B}' => "VT".into(),
        '\u{C}' => "FF".into(),
        '\r' => "CR".into(),
        '\u{7F}' => "DEL".into(),
        '\u{9B}' => "CSI".into(),
        c => format!("U+{:04X}", c as u32),
    }
}

/// Variation selectors 1–256 as bytes 0–255 (how data is hidden behind an emoji).
fn selector_byte(c: char) -> u8 {
    let v = c as u32;
    if v <= 0xFE0F {
        (v - 0xFE00) as u8
    } else {
        (v - 0xE0100 + 16) as u8
    }
}

/// End (char index) of the escape sequence starting with ESC at `i`.
fn escape_end(chars: &[(usize, char)], i: usize) -> usize {
    let n = chars.len();
    let code = |j: usize| chars.get(j).map(|&(_, c)| c as u32);
    let mut j = i + 1;
    match chars.get(j).map(|&(_, c)| c) {
        // CSI: parameters and intermediates, then one final byte.
        Some('[') => {
            j += 1;
            while code(j).is_some_and(|c| (0x20..=0x3F).contains(&c)) {
                j += 1;
            }
            if code(j).is_some_and(|c| (0x40..=0x7E).contains(&c)) {
                j += 1;
            }
            j
        }
        // OSC, DCS, SOS, PM, APC: up to BEL or ESC \.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            j += 1;
            let limit = (j + 512).min(n);
            while j < limit {
                match chars[j].1 {
                    '\u{7}' => return j + 1,
                    '\u{1B}' if chars.get(j + 1).map(|&(_, c)| c) == Some('\\') => return j + 2,
                    _ => j += 1,
                }
            }
            j
        }
        Some(c) if (' '..='~').contains(&c) => j + 1,
        _ => j,
    }
}

/// Characters that make the text differ from how it looks. Context matters: a
/// zero-width joiner inside an emoji or a direction mark in Arabic text is normal.
pub fn find_hidden(text: &str) -> Vec<Hidden> {
    let text = scan_window(text);
    if text.bytes().all(|b| (0x20..0x7F).contains(&b) || b == b'\t' || b == b'\n') {
        return Vec::new(); // plain printable ASCII
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let has_rtl = chars.iter().any(|&(_, c)| is_rtl(c));
    let has_jamo = chars.iter().any(|&(_, c)| is_jamo(c));
    let has_mongolian = chars.iter().any(|&(_, c)| ('\u{1800}'..='\u{18AF}').contains(&c) && c != '\u{180E}');
    let at = |i: usize| chars.get(i).map(|&(_, c)| c);
    let byte = |i: usize| chars.get(i).map(|&(b, _)| b).unwrap_or(text.len());
    let mut out = Vec::new();
    let mut push = |from: usize, to: usize, kind: HiddenKind, label: String| {
        out.push(Hidden { start: byte(from), end: byte(to), kind, label });
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let prev = i.checked_sub(1).and_then(at);
        if is_zero_width(c, has_mongolian) {
            let mut j = i;
            while at(j).is_some_and(|c| is_zero_width(c, has_mongolian)) {
                j += 1;
            }
            let next = at(j);
            let bom = c == '\u{FEFF}' && i == 0 && j == 1;
            // One between two non-ASCII characters is normal (emoji sequences, Indic,
            // Persian or Thai text); a run, or one in or next to plain ASCII, is not.
            let normal = j - i == 1 && prev.is_some_and(|p| !p.is_ascii()) && next.is_some_and(|n| !n.is_ascii());
            if !bom && !normal {
                for k in i..j {
                    push(k, k + 1, HiddenKind::ZeroWidth, zero_width_name(chars[k].1));
                }
            }
            i = j;
            continue;
        }
        if is_direction(c) {
            // Overrides reorder what you see; marks and isolates are normal in right-to-left text.
            if matches!(c, '\u{202D}' | '\u{202E}') || !has_rtl {
                push(i, i + 1, HiddenKind::Direction, direction_name(c).into());
            }
            i += 1;
            continue;
        }
        if is_tag(c) {
            let mut j = i;
            while at(j).is_some_and(is_tag) {
                j += 1;
            }
            // 🏴 + tags + cancel tag is a flag emoji (England, Scotland, Wales).
            let flag = prev == Some('\u{1F3F4}') && at(j - 1) == Some('\u{E007F}') && j - i <= 8;
            if !flag {
                let hidden: String = chars[i..j]
                    .iter()
                    .filter_map(|&(_, t)| {
                        let v = t as u32;
                        if (0xE0020..=0xE007E).contains(&v) { char::from_u32(v - 0xE0000) } else { None }
                    })
                    .collect();
                push(i, j, HiddenKind::TagText, format!("hidden text: {}", shorten(&hidden, 60)));
            }
            i = j;
            continue;
        }
        if is_selector(c) {
            let mut j = i;
            while at(j).is_some_and(is_selector) {
                j += 1;
            }
            // One selector after a character is normal: emoji style (incl. keycaps
            // like 1️⃣) and ideographic variants. Anything more carries hidden data.
            let first_ok = match prev {
                None => false,
                Some(p) if matches!(c, '\u{FE0E}' | '\u{FE0F}') => !p.is_ascii() || at(i + 1) == Some('\u{20E3}'),
                Some(p) if ('\u{E0100}'..='\u{E01EF}').contains(&c) => is_ideograph(p),
                Some(p) => !p.is_ascii(),
            };
            let from = if first_ok { i + 1 } else { i };
            if from < j {
                let bytes: Vec<u8> = chars[from..j].iter().map(|&(_, s)| selector_byte(s)).collect();
                let label = match std::str::from_utf8(&bytes) {
                    Ok(s) if s.chars().all(|c| !c.is_control() || c == '\n') => format!("hidden text: {}", shorten(s, 60)),
                    _ => format!("hidden data: {} byte{}", bytes.len(), if bytes.len() == 1 { "" } else { "s" }),
                };
                push(from, j, HiddenKind::HiddenData, label);
            }
            i = j;
            continue;
        }
        if is_filler(c) {
            if !has_jamo {
                push(i, i + 1, HiddenKind::Filler, format!("U+{:04X}", c as u32));
            }
            i += 1;
            continue;
        }
        match c {
            '\t' | '\n' => {}
            '\r' => {
                if at(i + 1) != Some('\n') {
                    push(i, i + 1, HiddenKind::Control, "CR".into());
                }
            }
            '\u{1B}' => {
                let j = escape_end(&chars, i);
                let seq: String = chars[i + 1..j].iter().map(|&(_, c)| c).filter(|c| !c.is_control()).collect();
                push(i, j, HiddenKind::Escape, format!("ESC{}", shorten(&seq, 16)));
                i = j;
                continue;
            }
            c if c.is_control() => push(i, i + 1, HiddenKind::Control, control_name(c)),
            _ => {}
        }
        i += 1;
    }
    out
}

/// E.g. "2 zero-width characters, 1 terminal escape sequence".
pub fn describe(found: &[Hidden]) -> String {
    let mut counts: Vec<(HiddenKind, usize)> = Vec::new();
    for h in found {
        match counts.iter_mut().find(|(k, _)| *k == h.kind) {
            Some((_, n)) => *n += 1,
            None => counts.push((h.kind, 1)),
        }
    }
    counts.iter().map(|(k, n)| k.describe(*n)).collect::<Vec<_>>().join(", ")
}

/// `text` without the hidden characters.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for h in find_hidden(text) {
        out.push_str(&text[pos..h.start]);
        pos = h.end;
    }
    out.push_str(&text[pos..]);
    out
}

/// `text` with each hidden run shown as `⟨LABEL⟩`, plus where those markers are
/// (character offsets, for highlighting).
pub fn reveal_marked(text: &str) -> (String, Vec<(usize, usize)>) {
    let found = find_hidden(text);
    let mut out = String::with_capacity(text.len() + found.len() * 8);
    let mut marks = Vec::with_capacity(found.len());
    let (mut pos, mut offset) = (0, 0);
    for h in &found {
        let before = &text[pos..h.start];
        out.push_str(before);
        offset += before.chars().count();
        let marker = format!("⟨{}⟩", h.label);
        let len = marker.chars().count();
        out.push_str(&marker);
        marks.push((offset, offset + len));
        offset += len;
        pos = h.end;
    }
    out.push_str(&text[pos..]);
    (out, marks)
}

/// `text` with each hidden run shown as `⟨LABEL⟩`.
pub fn reveal(text: &str) -> String {
    reveal_marked(text).0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake secrets are assembled at run time so the source never contains a
    // complete key (and doesn't trip secret scanners).
    fn fake(prefix: &str, body: &str, times: usize) -> String {
        format!("{prefix}{}", body.repeat(times))
    }

    #[test]
    fn recognizes_keys_and_tokens() {
        let cases = [
            (fake("sk-proj-", "Ab3dEf9h", 6), "API key"),
            (fake("sk-", "0123456789abcdef", 2), "API key"),
            (fake("sk-ant-api03-", "Xy7_", 10), "Anthropic API key"),
            (fake("AKIA", "ABCD2345", 2), "AWS access key"),
            (fake("ghp_", "aB3d", 9), "GitHub token"),
            (fake("glpat-", "xY9_", 6), "GitLab token"),
            (fake("xoxb-", "1234-abcd", 2), "Slack token"),
            (fake("AIza", "Sy1-", 9) + "x9z", "Google API key"),
            (fake("sk_live_", "4eC3", 7), "Stripe key"),
            (fake("hf_", "AbCdEfGh", 5), "Hugging Face token"),
            (fake("eyJhbGciOiJIUzI1NiJ9.eyJ", "zdWIiOiIx", 2) + ".SflKxwRJSMeKKF2QT4fwp", "JSON Web Token"),
        ];
        for (secret, label) in cases {
            let text = format!("key: {secret} (do not share)");
            assert_eq!(secret_label(&text, true), Some(label), "{secret}");
        }
    }

    #[test]
    fn ignores_look_alikes() {
        for text in [
            "sk-learn-tutorial-for-beginners-2024",
            "AKIA is not a key",
            "the ghp_ prefix marks GitHub tokens",
            "postgres://user:password@localhost:5432/db",
            "https://user:<password>@example.com",
            "API_KEY_VAR=OPENAI_API_KEY",
            "token = get_token()",
            "TOKEN_URL=https://auth.example.com/oauth2/token",
            "SECRET_KEY = settings.SECRET_KEY",
            "1732670000123",
            "4111111111111112",
        ] {
            assert_eq!(secret_label(text, true), None, "{text}");
        }
    }

    #[test]
    fn recognizes_private_keys_links_and_assignments() {
        let key = format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----", "b3BlbnNzaA".repeat(8));
        assert_eq!(secret_label(&key, true), Some("Private key"));
        assert_eq!(mask(&key, true), "-----BEGIN OPENSSH PRIVATE KEY----- ••••");
        let link = format!("DATABASE_URL=postgres://app:{}@db.internal/app", "s3cr3tPa55");
        assert_eq!(mask(&link, true), "DATABASE_URL=postgres://app:••••@db.internal/app");
        let env = format!("export DB_PASSWORD=\"{}\"\nDEBUG=1", "hunter2hunter2");
        assert_eq!(secret_label(&env, true), Some("Password or secret"));
        assert_eq!(mask(&env, true), "export DB_PASSWORD=\"••••\"\nDEBUG=1");
        let json = format!("{{\n  \"client_secret\": \"{}\",\n}}", "9f8e7d6c5b4a");
        assert_eq!(secret_label(&json, true), Some("Password or secret"));
    }

    #[test]
    fn recognizes_card_and_id_numbers_only_as_whole_items() {
        assert_eq!(secret_label("4111 1111 1111 1111", true), Some("Card number"));
        assert_eq!(mask("4111-1111-1111-1111", true), "•••• 1111");
        assert_eq!(secret_label("Card 4111 1111 1111 1111 on file", true), None);
        assert_eq!(secret_label("4111 1111 1111 1111", false), None);
        assert_eq!(secret_label("11010519491231002X", true), Some("ID card number"));
        assert_eq!(secret_label("110105194912310021", true), None);
    }

    #[test]
    fn masks_keep_a_recognizable_prefix() {
        let key = fake("sk-proj-", "Ab3dEf9h", 6);
        assert_eq!(mask(&key, true), format!("sk-proj-••••{}", &key[key.len() - 4..]));
        let text = format!("token {} end", fake("ghp_", "aB3d", 9));
        assert_eq!(mask(&text, true), "token ghp_••••aB3d end");
    }

    fn labels(text: &str) -> Vec<String> {
        find_hidden(text).into_iter().map(|h| h.label).collect()
    }

    #[test]
    fn finds_zero_width_characters_in_plain_text() {
        assert_eq!(labels("curl\u{200B} -fsSL example.sh"), vec!["ZWSP"]);
        assert_eq!(labels("a\u{200C}\u{200D}b"), vec!["ZWNJ", "ZWJ"]);
        assert_eq!(clean("rm\u{2060} -rf\u{200B} build"), "rm -rf build");
    }

    #[test]
    fn leaves_normal_uses_alone() {
        assert!(find_hidden("family 👨\u{200D}👩\u{200D}👧 and ❤\u{FE0F}\u{200D}🔥").is_empty());
        assert!(find_hidden("می\u{200C}خواهم").is_empty()); // Persian
        assert!(find_hidden("ภาษา\u{200B}ไทย").is_empty()); // Thai word break
        assert!(find_hidden("\u{FEFF}hello").is_empty()); // byte order mark
        assert!(find_hidden("שלום \u{200F}abc").is_empty()); // RLM in Hebrew text
        assert!(find_hidden("keycap 1\u{FE0F}\u{20E3} and ✌\u{FE0F}").is_empty());
        assert!(find_hidden("🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}").is_empty()); // England flag
        assert!(find_hidden("line one\r\nline two\ttab").is_empty());
    }

    #[test]
    fn finds_direction_overrides_tags_and_selector_data() {
        assert_eq!(labels("invoice\u{202E}fdp.exe"), vec!["RLO"]);
        assert_eq!(labels("abc\u{200F}def"), vec!["RLM"]);
        let tags: String = "ignore this".chars().map(|c| char::from_u32(c as u32 + 0xE0000).unwrap()).collect();
        assert_eq!(labels(&format!("Summarize this{tags}")), vec!["hidden text: ignore this"]);
        let data: String = "hi".bytes().map(|b| char::from_u32(0xE0100 + b as u32 - 16).unwrap()).collect();
        assert_eq!(labels(&format!("😀{data}")), vec!["hidden text: hi"]);
    }

    #[test]
    fn finds_terminal_escapes_and_controls() {
        let text = "echo safe\u{1B}[2K\u{1B}]0;title\u{7}\rrm -rf ~";
        let found = find_hidden(text);
        let got: Vec<&str> = found.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(got, vec!["ESC[2K", "ESC]0;title", "CR"]);
        assert_eq!(clean(text), "echo saferm -rf ~");
        assert_eq!(describe(&found), "2 terminal escape sequences, 1 control character");
        assert_eq!(reveal("a\u{0}b"), "a⟨NUL⟩b");
    }

    #[test]
    fn marks_are_character_offsets() {
        let (text, marks) = reveal_marked("é\u{200B}x");
        assert_eq!(text, "é⟨ZWSP⟩x");
        assert_eq!(marks, vec![(1, 7)]);
    }
}
