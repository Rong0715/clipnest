//! Quick conversions offered in the details pane, depending on what was copied:
//! Unix timestamps shown as dates, JSON formatted or minified, URL-encoded and
//! Base64 text decoded, JSON Web Tokens decoded. All local; no web tools needed.

/// A converted version of the text, offered as "label" → copy result.
#[derive(Debug, PartialEq, Eq)]
pub struct Conversion {
    pub label: &'static str,
    pub result: String,
}

/// Seconds since the epoch if `text` is a plausible Unix time (2000–2100), in
/// seconds (10 digits) or milliseconds (13 digits).
pub fn unix_time(text: &str) -> Option<i64> {
    let t = text.trim();
    if !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs = match t.len() {
        10 => t.parse::<i64>().ok()?,
        13 => t.parse::<i64>().ok()? / 1000,
        _ => return None,
    };
    (946_684_800..4_102_444_800).contains(&secs).then_some(secs)
}

/// Re-indents (or, with `indent` None, minifies) valid JSON, keeping key order.
fn reformat_json(text: &str, indent: Option<&str>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    let newline = |out: &mut String, depth: usize| {
        if let Some(ind) = indent {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(ind);
            }
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '{' | '[' => {
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && matches!(chars[j], '}' | ']') {
                    out.push(c);
                    out.push(chars[j]); // empty: {} or []
                    i = j + 1;
                    continue;
                }
                out.push(c);
                depth += 1;
                newline(&mut out, depth);
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => {
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
            }
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn is_json(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('{') || t.starts_with('[')) && serde_json::from_str::<serde::de::IgnoredAny>(t).is_ok()
}

/// Decodes standard or URL-safe Base64, with or without padding.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let t = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(t.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in t.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (t.len() % 4 != 1).then_some(out)
}

/// Readable text: no control characters except line breaks and tabs.
fn readable(bytes: Vec<u8>) -> Option<String> {
    let s = String::from_utf8(bytes).ok()?;
    let ok = !s.trim().is_empty() && s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t' || c == '\r');
    ok.then_some(s)
}

/// Base64 that decodes to readable text (plain words that happen to be valid
/// Base64 decode to garbage and are skipped).
fn base64_text(text: &str) -> Option<String> {
    let t = text.trim();
    if t.len() < 8 || t.contains(char::is_whitespace) || t.len() % 4 == 1 {
        return None;
    }
    let decoded = readable(base64_decode(t)?)?;
    (decoded.chars().count() >= 4).then_some(decoded)
}

/// The header and payload of a JSON Web Token, formatted.
fn jwt(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.trim().split('.').collect();
    if parts.len() != 3 || !parts[0].starts_with("eyJ") {
        return None;
    }
    let header = readable(base64_decode(parts[0])?).filter(|h| is_json(h))?;
    let payload = readable(base64_decode(parts[1])?).filter(|p| is_json(p))?;
    Some(format!("{}\n{}", reformat_json(&header, Some("  ")), reformat_json(&payload, Some("  "))))
}

fn url_decoded(text: &str) -> Option<String> {
    let t = text.trim();
    let has_escape = t.as_bytes().windows(3).any(|w| w[0] == b'%' && w[1].is_ascii_hexdigit() && w[2].is_ascii_hexdigit());
    if !has_escape {
        return None;
    }
    let decoded = crate::model::percent_decode(t);
    (decoded != t && !decoded.contains('\u{FFFD}')).then_some(decoded)
}

// ---- colors -----------------------------------------------------------------------

/// Parses a CSS color: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`/`rgba()`
/// and `hsl()`/`hsla()` (comma or space separated, optional `/ alpha`).
pub fn parse_color(text: &str) -> Option<[f64; 4]> {
    let t = text.trim();
    if t.starts_with('#') {
        return crate::model::parse_hex_color(t);
    }
    let lower = t.to_ascii_lowercase();
    let (func, rest) = lower.split_once('(')?;
    let inner = rest.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let num = |p: &str| -> Option<f64> { p.trim_end_matches(['%', 'g', 'd', 'e']).parse::<f64>().ok() };
    let alpha = match parts.get(3) {
        Some(a) if a.ends_with('%') => num(a)? / 100.0,
        Some(a) => num(a)?,
        None => 1.0,
    };
    let rgb = match func {
        "rgb" | "rgba" => {
            let channel = |p: &str| -> Option<f64> {
                Some(if p.ends_with('%') { num(p)? / 100.0 } else { num(p)? / 255.0 })
            };
            [channel(parts[0])?, channel(parts[1])?, channel(parts[2])?]
        }
        "hsl" | "hsla" => hsl_to_rgb(num(parts[0])?, num(parts[1])? / 100.0, num(parts[2])? / 100.0),
        _ => return None,
    };
    let ok = |v: f64| (0.0..=1.0).contains(&v);
    (rgb.iter().all(|v| ok(*v)) && ok(alpha)).then_some([rgb[0], rgb[1], rgb[2], alpha])
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [f64; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r + m, g + m, b + m]
}

fn rgb_to_hsl([r, g, b]: [f64; 3]) -> (f64, f64, f64) {
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, s, l)
}

/// The color written as HEX, RGB and HSL.
pub fn color_formats(c: [f64; 4]) -> [(&'static str, String); 3] {
    let byte = |v: f64| (v * 255.0).round() as u8;
    let (r, g, b) = (byte(c[0]), byte(c[1]), byte(c[2]));
    let alpha = c[3] < 1.0;
    let a = (c[3] * 100.0).round() / 100.0;
    let hex = if alpha { format!("#{r:02X}{g:02X}{b:02X}{:02X}", byte(c[3])) } else { format!("#{r:02X}{g:02X}{b:02X}") };
    let rgb = if alpha { format!("rgba({r}, {g}, {b}, {a})") } else { format!("rgb({r}, {g}, {b})") };
    let (h, s, l) = rgb_to_hsl([c[0], c[1], c[2]]);
    let (h, s, l) = (h.round(), (s * 100.0).round(), (l * 100.0).round());
    let hsl = if alpha { format!("hsla({h}, {s}%, {l}%, {a})") } else { format!("hsl({h}, {s}%, {l}%)") };
    [("Copy as HEX", hex), ("Copy as RGB", rgb), ("Copy as HSL", hsl)]
}

/// What the copied text can be turned into.
pub fn conversions(text: &str) -> Vec<Conversion> {
    let mut out = Vec::new();
    if text.len() > 2 * 1024 * 1024 {
        return out;
    }
    let mut add = |label, result: Option<String>| {
        if let Some(result) = result.filter(|r| r != text) {
            out.push(Conversion { label, result });
        }
    };
    if let Some(color) = parse_color(text) {
        let written = text.trim().to_ascii_lowercase();
        for (label, result) in color_formats(color) {
            // Skip the format the color was copied in.
            let same = result.to_ascii_lowercase() == written
                || written.starts_with(&result.to_ascii_lowercase()[..3]);
            if !same {
                out.push(Conversion { label, result });
            }
        }
        return out;
    }
    if let Some(decoded) = jwt(text) {
        add("Decode JWT", Some(decoded));
        return out;
    }
    if is_json(text) {
        add("Format JSON", Some(reformat_json(text.trim(), Some("  "))));
        add("Minify JSON", Some(reformat_json(text.trim(), None)));
        return out;
    }
    add("Decode URL", url_decoded(text));
    add("Decode Base64", base64_text(text));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(text: &str) -> Vec<&'static str> {
        conversions(text).into_iter().map(|c| c.label).collect()
    }

    #[test]
    fn colors() {
        assert_eq!(labels("#FF5733"), vec!["Copy as RGB", "Copy as HSL"]);
        let all = conversions("#ff5733");
        assert_eq!(all[0].result, "rgb(255, 87, 51)");
        assert_eq!(all[1].result, "hsl(11, 100%, 60%)");
        assert_eq!(conversions("rgb(255, 87, 51)")[0].result, "#FF5733");
        assert_eq!(conversions("hsl(11 100% 60%)")[0].result, "#FF5833"); // HSL is rounded
        assert_eq!(conversions("rgba(0,0,0,0.5)")[0].result, "#00000080");
        assert!(parse_color("rgb(300, 0, 0)").is_none());
        assert!(parse_color("hello(1,2,3)").is_none());
    }

    #[test]
    fn timestamps() {
        assert_eq!(unix_time("1732670000"), Some(1732670000));
        assert_eq!(unix_time(" 1732670000123 "), Some(1732670000));
        assert_eq!(unix_time("123456789"), None);
        assert_eq!(unix_time("9999999999"), None); // year 2286
        assert_eq!(unix_time("17326x0000"), None);
    }

    #[test]
    fn json_keeps_key_order() {
        let text = r#"{"b":1, "a":[1,2,{"c":"x, y: {z}"}],"e":{}, "f":[ ]}"#;
        assert_eq!(labels(text), vec!["Format JSON", "Minify JSON"]);
        let pretty = &conversions(text)[0].result;
        assert_eq!(
            pretty,
            "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2,\n    {\n      \"c\": \"x, y: {z}\"\n    }\n  ],\n  \"e\": {},\n  \"f\": []\n}"
        );
        assert_eq!(labels(pretty), vec!["Minify JSON"]); // already formatted
        assert_eq!(conversions(pretty)[0].result,r#"{"b":1,"a":[1,2,{"c":"x, y: {z}"}],"e":{},"f":[]}"#);
        assert!(labels("{not json").is_empty());
    }

    #[test]
    fn decodes_url_base64_and_jwt() {
        assert_eq!(conversions("q=hello%20world%21")[0].result, "q=hello world!");
        assert_eq!(conversions("aGVsbG8gd29ybGQ=")[0].result, "hello world");
        assert_eq!(conversions("5L2g5aW95LiW55WM")[0].result, "你好世界");
        assert!(labels("Password").is_empty());
        assert!(labels("HelloWorld").is_empty());
        assert!(labels("just some words").is_empty());
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMiLCJuYW1lIjoiQW5uYSJ9.c2lnbmF0dXJl";
        assert_eq!(labels(token), vec!["Decode JWT"]);
        assert_eq!(conversions(token)[0].result, "{\n  \"alg\": \"HS256\"\n}\n{\n  \"sub\": \"123\",\n  \"name\": \"Anna\"\n}");
    }
}
