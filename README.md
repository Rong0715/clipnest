<p align="center"><img src="data/icons/hicolor/scalable/apps/io.github.Rong0715.Clipnest.svg" width="128" alt="Clipnest icon"></p>

<h1 align="center">Clipnest</h1>

> [!WARNING]
> **This project was generated entirely by AI (Claude Opus 5.5, max effort).** It has been
> tested, but no human has reviewed the code line by line. **Use with caution** — it reads
> your clipboard, which can contain sensitive data. Issues and reviews are welcome.

A lightweight, keyboard-first clipboard manager for Linux (GNOME, GTK 4 / libadwaita),
written in Rust. Its design is based on [Maccy](https://maccy.app), the macOS clipboard
manager by Alex Rodionov. Clipnest is **not affiliated with Maccy**.

It runs quietly in the background, remembers what you copy, shows a notification each time
something is copied, and gives you a window to search, copy back, pin and delete items.

## Features

- **Copy notifications** — a short notification for every copy (can show the copied text,
  or just that something was copied).
- **History window** — search (exact, fuzzy, regex or mixed), a details pane with the full
  text or image, pinning, deleting and clearing.
- **Keyboard first** — the search field keeps focus; ↑/↓ move, Enter copies, Shift+Enter
  copies without formatting, Ctrl+1…9 copy the first nine items and Ctrl+&lt;key&gt; copies a
  pinned item.
- Text, rich text (HTML/RTF), images and copied files.
- **Search text in images** — text in copied screenshots and pictures is recognized on your
  computer (Tesseract, any installed language) so you can search for it; *Copy Plain* on an
  image copies its text.
- **Sensitive copies stay off the disk** — API keys and tokens (OpenAI, Anthropic, GitHub,
  AWS, Slack, Stripe…), private keys, JSON Web Tokens, passwords in links and in `KEY=value`
  lines, card and ID numbers, also in screenshots, are shown masked (`sk-proj-••••3fA9`), kept
  in memory only and removed after 30 seconds — from the clipboard too, if they are still on it.
- **Hidden-character warnings** — zero-width characters, text-direction controls, invisible
  tag text and terminal escape codes in copied text (a common trick in commands on web
  pages) are shown as red labels, with a notification offering to copy the text without them.
- **Dangerous-command warnings** — a short list of commands that wipe or damage the system
  (`rm -rf ~`, `mkfs`, `dd of=/dev/sda`, a fork bomb…) or run code in disguise (base64 piped
  to a shell, scripts from bare IP addresses or plain HTTP). Ordinary `curl … | sh`
  installers are not flagged. Optionally, suspicious copies are cleaned on the clipboard
  right away, including the line break at the end that would run a command when pasted.
- **Paste in order** — pick an item (Alt+O or right-click → *Paste in Order From Here*) and
  it plus everything copied after it are pasted one by one, oldest first: each normal paste
  (Ctrl+V, in any app) moves on to the next item. Handy for filling in forms.
- **Quick conversions** in the details pane — Unix timestamps shown as dates, JSON formatted
  or minified, URL-encoded and Base64 text decoded, JSON Web Tokens decoded, and colors
  (`#ff5733`, `rgb()`, `hsl()`) shown as swatches and copied as HEX, RGB or HSL.
- Ignore lists: password-manager hints (KeePassXC etc.), custom clipboard types, regular
  expressions, and X11 applications.
- Pause, "ignore next copy", history size limit, sort order.
- Panel icon with recent items, including image thumbnails (on desktops with a tray, e.g.
  Ubuntu, KDE).
- Optional global shortcut (through the desktop's GlobalShortcuts portal) and optional launch
  at login.

## Install

Build requirements on Ubuntu/Debian:

```sh
sudo apt install cargo build-essential pkg-config libgtk-4-dev libadwaita-1-dev
```

Optional, to search text inside images (add more `tesseract-ocr-*` language packs as you
like, e.g. `tesseract-ocr-chi-sim`):

```sh
sudo apt install tesseract-ocr
```

Then build and install for your user only (into `~/.local`, no root needed):

```sh
./install.sh
```

Open **Clipnest** from the app grid. Close the window and Clipnest keeps watching the
clipboard; open it again from the app grid, the panel icon or `clipnest`.

Turn on *Launch at login* in Preferences → General to start it automatically.

## Uninstall

```sh
./uninstall.sh          # keeps your history
./uninstall.sh --purge  # also deletes history and settings
```

## Keyboard shortcuts (in the window)

| Keys | Action |
|---|---|
| ↑ ↓, Ctrl+N / Ctrl+P | Move |
| Enter, double-click | Copy |
| Shift+Enter | Copy without formatting |
| Ctrl+1 … Ctrl+9 | Copy one of the first nine items |
| Ctrl+&lt;key&gt; | Copy a pinned item |
| Alt+P | Pin / unpin |
| Alt+Backspace, Delete (empty search) | Delete |
| Alt+O | Paste this item and the newer ones in order |
| Ctrl+Space | Show / hide details |
| Ctrl+F, Ctrl+U | Search, clear search |
| Ctrl+Shift+Backspace | Clear history |
| Escape | Clear search, then close |
| Ctrl+, / Ctrl+Q | Preferences / Quit |

## Command line

`clipnest --help` lists commands such as `clipnest list`, `clipnest copy 3`,
`clipnest pause`, `clipnest sequence 3` (paste the last three copies in order) and
`clipnest toggle` (handy for a custom keyboard shortcut).

## How it works on Wayland

Wayland compositors don't let ordinary apps read the clipboard in the background. Clipnest
reads (and sets) it through XWayland, which GNOME and KDE keep in sync with the Wayland
clipboard. This needs no special permission and changes nothing in your system.

## Privacy

History stays on your computer in `~/.local/share/clipnest/history.sqlite3` (readable only
by you). Settings are in `~/.config/clipnest/settings.json`. Copies marked by password
managers are skipped before they are read. Copies that look sensitive are never written to
the database, and deleted items are overwritten on disk rather than just unlinked. Set the
time and turn these checks on or off in Preferences → Privacy.

## Status and limitations

- Tested on Ubuntu 26.04 with GNOME 50 (Wayland). Other desktops should work through
  XWayland but are untested.
- A regular window, not a popup at the cursor (Wayland doesn't let apps position windows).
- No "paste automatically": apps can't type into other apps on Wayland without a
  remote-control permission.
- "Ignore applications" only works for X11 apps, because Wayland apps don't report who
  copied.
- Sensitive-data and hidden-character detection is pattern-based: it can miss things or flag
  harmless text. Clipnest sees copies, not pastes, so it can't block a paste, and
  middle-click pastes (the primary selection) are not checked.

## License

MIT (see [LICENSE](LICENSE)). The design and behavior are based on Maccy, © Alex Rodionov,
also MIT licensed.
