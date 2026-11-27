//! Text recognition in copied images with Tesseract (runs locally, offline).
//!
//! Like Maccy's Vision-based OCR: recognized text makes image items searchable.
//! The `tesseract` command is optional; without it images simply aren't searchable.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Languages Tesseract has data for, joined for `-l` (e.g. `eng+chi_sim`).
pub fn languages() -> Option<String> {
    let output = Command::new("tesseract").arg("--list-langs").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    let mut langs: Vec<String> = text
        .lines()
        .skip_while(|l| !l.starts_with("List of"))
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "osd" && *l != "equ" && !l.contains(' '))
        .map(str::to_owned)
        .collect();
    // English first: it is the most common text in screenshots and helps mixed text.
    langs.sort_by_key(|l| if l == "eng" { 0 } else { 1 });
    (!langs.is_empty()).then(|| langs.join("+"))
}

pub struct Ocr {
    jobs: mpsc::Sender<(i64, Vec<u8>)>,
}

impl Ocr {
    /// Starts the recognition thread. Returns `None` when Tesseract isn't installed.
    pub fn start() -> Option<(Ocr, async_channel::Receiver<(i64, String)>, String)> {
        let langs = languages()?;
        let (tx, rx) = mpsc::channel::<(i64, Vec<u8>)>();
        let (done_tx, done_rx) = async_channel::unbounded();
        let thread_langs = langs.clone();
        std::thread::Builder::new()
            .name("clipnest-ocr".into())
            .spawn(move || {
                while let Ok((id, image)) = rx.recv() {
                    let text = recognize(&image, &thread_langs).unwrap_or_default();
                    if done_tx.send_blocking((id, text)).is_err() {
                        break;
                    }
                }
            })
            .ok()?;
        Some((Ocr { jobs: tx }, done_rx, langs))
    }

    pub fn queue(&self, id: i64, image: Vec<u8>) {
        let _ = self.jobs.send((id, image));
    }
}

/// Recognizes text twice — with automatic layout analysis (best for screenshots
/// with columns) and as one block (catches lines the layout pass misses) — and
/// keeps whichever found more text.
fn recognize(image: &[u8], langs: &str) -> Option<String> {
    let auto = run(image, langs, "3");
    let block = run(image, langs, "6");
    let score = |t: &Option<String>| t.as_deref().map(|t| t.chars().filter(|c| c.is_alphanumeric()).count()).unwrap_or(0);
    if score(&block) > score(&auto) { block } else { auto.or(block) }
}

/// Runs `tesseract stdin stdout` with a time limit. `None` means it failed.
fn run(image: &[u8], langs: &str, psm: &str) -> Option<String> {
    let mut child = Command::new("nice")
        .args(["-n", "10", "tesseract", "stdin", "stdout", "-l", langs, "--psm", psm])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .or_else(|_| {
            Command::new("tesseract")
                .args(["stdin", "stdout", "-l", langs, "--psm", psm])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
        })
        .ok()?;
    // Write on another thread so a large image can't deadlock against stdout.
    let mut stdin = child.stdin.take()?;
    let data = image.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let _ = writer.join();
    let out = reader.join().ok()?;
    Some(clean(&String::from_utf8_lossy(&out)))
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0xac00..=0xd7af | 0xff00..=0xffef | 0x3000..=0x303f)
}

/// Tidies Tesseract output: trims lines, drops empty ones and removes the spaces
/// it puts between CJK characters.
pub fn clean(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            let chars: Vec<char> = line.trim().chars().collect();
            let mut out = String::new();
            for (i, c) in chars.iter().enumerate() {
                if *c == ' ' && i > 0 && i + 1 < chars.len() && is_cjk(chars[i - 1]) && is_cjk(chars[i + 1]) {
                    continue;
                }
                out.push(*c);
            }
            out
        })
        .filter(|l| !l.is_empty() && l.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_output() {
        assert_eq!(clean("  Hello world \n\n 你 好 世 界 \n ~ \n"), "Hello world\n你好世界");
    }
}
