//! X11 / XWayland clipboard engine running on its own thread.
//!
//! * Watches CLIPBOARD ownership with XFixes and reads the new contents
//!   (TARGETS, then each wanted target, including INCR transfers).
//! * Owns CLIPBOARD to put history items back, serving TARGETS/TIMESTAMP and
//!   every stored type, with INCR for large data.
//!
//! On GNOME/KDE Wayland the compositor mirrors the Wayland clipboard into
//! XWayland, so this works there too without any special permission.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Property, SelectionNotifyEvent, Window, WindowClass,
};
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use crate::model::{self, IMAGE_MIMES, MIME_GNOME_FILES, MIME_HTML, MIME_RTF, MIME_TEXT, MIME_URI_LIST};

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        TIMESTAMP,
        MULTIPLE,
        INCR,
        UTF8_STRING,
        TEXT,
        SAVE_TARGETS,
        MACCY_SEL: b"_CLIPNEST_SELECTION",
        _NET_ACTIVE_WINDOW,
    }
}

/// What to collect from the clipboard. Updated from the main thread.
#[derive(Clone, Debug)]
pub struct WatchConfig {
    pub save_text: bool,
    pub save_images: bool,
    pub save_files: bool,
    pub ignored_types: Vec<String>,
    /// When false, ownership changes are not read at all (paused).
    pub enabled: bool,
}

pub enum Command {
    SetClipboard(Vec<(String, Arc<Vec<u8>>)>),
    Clear,
    Config(WatchConfig),
}

pub enum Event {
    Copied {
        contents: Vec<(String, Vec<u8>)>,
        application: Option<String>,
    },
    /// A copy that was not read (paused, or an ignored type).
    Ignored,
    Failed(String),
}

pub struct Handle {
    tx: mpsc::Sender<Command>,
    wake: UnixStream,
}

impl Handle {
    pub fn send(&self, cmd: Command) {
        if self.tx.send(cmd).is_ok() {
            let _ = (&self.wake).write(&[1]);
        }
    }
}

/// Connects to the X server in `$DISPLAY` and starts the clipboard thread.
pub fn start(config: WatchConfig) -> Result<(Handle, async_channel::Receiver<Event>), String> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| format!("cannot connect to X11/XWayland: {e}"))?;
    let (tx, rx) = mpsc::channel();
    let (wake_read, wake_write) = UnixStream::pair().map_err(|e| e.to_string())?;
    wake_read.set_nonblocking(true).map_err(|e| e.to_string())?;
    wake_write.set_nonblocking(true).map_err(|e| e.to_string())?;
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let engine = Engine::new(conn, screen, config, ev_tx).map_err(|e| format!("X11 setup failed: {e}"))?;
    std::thread::Builder::new()
        .name("clipnest-x11".into())
        .spawn(move || engine.run(rx, wake_read))
        .map_err(|e| e.to_string())?;
    Ok((Handle { tx, wake: wake_write }, ev_rx))
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A target being read from the current owner.
struct Transfer {
    target: Atom,
    mime: String,
    buf: Vec<u8>,
    incr: bool,
}

struct ReadTx {
    timestamp: u32,
    targets: Vec<String>,
    queue: VecDeque<(Atom, String, String)>, // (target atom, target name, stored mime)
    current: Option<Transfer>,
    results: Vec<(String, Vec<u8>)>,
    deadline: Instant,
    application: Option<String>,
}

/// Outgoing INCR transfer to a requestor.
struct Outgoing {
    data: Arc<Vec<u8>>,
    offset: usize,
    type_: Atom,
    done: bool,
    deadline: Instant,
}

struct Owned {
    entries: Vec<(Atom, Atom, Arc<Vec<u8>>)>, // (target, type, data)
    timestamp: u32,
}

const READ_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_ITEM_BYTES: usize = 64 * 1024 * 1024;

struct Engine {
    conn: RustConnection,
    root: Window,
    win: Window,
    atoms: Atoms,
    config: WatchConfig,
    events: async_channel::Sender<Event>,
    atom_names: HashMap<Atom, String>,
    atom_ids: HashMap<String, Atom>,
    read: Option<ReadTx>,
    owned: Option<Owned>,
    outgoing: HashMap<(Window, Atom), Outgoing>,
    chunk_size: usize,
    wayland_session: bool,
}

impl Engine {
    fn new(
        conn: RustConnection,
        screen: usize,
        config: WatchConfig,
        events: async_channel::Sender<Event>,
    ) -> Result<Engine, BoxError> {
        let root = conn.setup().roots[screen].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        let win = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, b"Clipnest clipboard")?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.xfixes_select_selection_input(
            win,
            atoms.CLIPBOARD,
            SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?;
        conn.flush()?;
        // Stay well below the maximum request size for property data.
        let chunk_size = (conn.maximum_request_bytes().saturating_sub(1024)).clamp(4096, 256 * 1024);
        let wayland_session = std::env::var("XDG_SESSION_TYPE").map(|v| v == "wayland").unwrap_or(false)
            || std::env::var_os("WAYLAND_DISPLAY").is_some();
        Ok(Engine {
            conn,
            root,
            win,
            atoms,
            config,
            events,
            atom_names: HashMap::new(),
            atom_ids: HashMap::new(),
            read: None,
            owned: None,
            outgoing: HashMap::new(),
            chunk_size,
            wayland_session,
        })
    }

    fn run(mut self, commands: mpsc::Receiver<Command>, mut wake: UnixStream) {
        if let Err(e) = self.event_loop(&commands, &mut wake) {
            let _ = self.events.send_blocking(Event::Failed(e.to_string()));
        }
    }

    fn event_loop(&mut self, commands: &mpsc::Receiver<Command>, wake: &mut UnixStream) -> Result<(), BoxError> {
        let x_fd = self.conn.stream().as_raw_fd();
        let wake_fd = wake.as_raw_fd();
        let mut scratch = [0u8; 64];
        loop {
            while let Some(event) = self.conn.poll_for_event()? {
                if let Err(e) = self.handle_event(event) {
                    crate::debug!("x11 event error: {e}");
                    self.read = None;
                }
            }
            loop {
                match commands.try_recv() {
                    Ok(cmd) => {
                        if let Err(e) = self.handle_command(cmd) {
                            crate::debug!("x11 command error: {e}");
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            while wake.read(&mut scratch).map(|n| n > 0).unwrap_or(false) {}
            self.check_timeouts()?;
            self.conn.flush()?;
            // Events may have been queued while waiting for replies above.
            if let Some(event) = self.conn.poll_for_event()? {
                if let Err(e) = self.handle_event(event) {
                    crate::debug!("x11 event error: {e}");
                    self.read = None;
                }
                continue;
            }
            let timeout = self.next_deadline().map(|d| {
                d.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32 + 1
            });
            let mut fds = [
                libc::pollfd { fd: x_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout.unwrap_or(-1)) };
            if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return Err("poll failed".into());
            }
            if fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return Err("X11 connection closed".into());
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let read = self.read.as_ref().map(|r| r.deadline);
        let out = self.outgoing.values().map(|o| o.deadline).min();
        match (read, out) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn check_timeouts(&mut self) -> Result<(), BoxError> {
        let now = Instant::now();
        if self.read.as_ref().map(|r| r.deadline <= now).unwrap_or(false) {
            crate::debug!("clipboard read timed out");
            // Keep whatever arrived; the slow target is skipped.
            if let Some(tx) = self.read.as_mut() {
                tx.current = None;
            }
            self.request_next()?;
        }
        self.outgoing.retain(|_, o| o.deadline > now);
        Ok(())
    }

    fn atom(&mut self, name: &str) -> Result<Atom, BoxError> {
        if let Some(a) = self.atom_ids.get(name) {
            return Ok(*a);
        }
        let a = self.conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
        self.atom_ids.insert(name.to_owned(), a);
        self.atom_names.insert(a, name.to_owned());
        Ok(a)
    }

    fn atom_names(&mut self, atoms: &[Atom]) -> Result<Vec<String>, BoxError> {
        let unknown: Vec<Atom> = atoms.iter().copied().filter(|a| !self.atom_names.contains_key(a)).collect();
        let cookies: Vec<_> = unknown
            .iter()
            .map(|a| self.conn.get_atom_name(*a))
            .collect::<Result<_, _>>()?;
        for (atom, cookie) in unknown.into_iter().zip(cookies) {
            if let Ok(reply) = cookie.reply() {
                let name = String::from_utf8_lossy(&reply.name).into_owned();
                self.atom_ids.insert(name.clone(), atom);
                self.atom_names.insert(atom, name);
            }
        }
        Ok(atoms.iter().filter_map(|a| self.atom_names.get(a).cloned()).collect())
    }

    fn handle_event(&mut self, event: XEvent) -> Result<(), BoxError> {
        match event {
            XEvent::XfixesSelectionNotify(ev) if ev.selection == self.atoms.CLIPBOARD => {
                if ev.owner == self.win {
                    if let Some(owned) = self.owned.as_mut() {
                        owned.timestamp = ev.selection_timestamp;
                    }
                } else {
                    // Someone else took the clipboard.
                    if ev.subtype == xfixes::SelectionEvent::SET_SELECTION_OWNER && ev.owner != NONE {
                        self.start_read(ev.owner, ev.selection_timestamp)?;
                    }
                }
            }
            XEvent::SelectionNotify(ev) if ev.requestor == self.win => self.on_selection_notify(ev)?,
            XEvent::PropertyNotify(ev) => self.on_property_notify(ev)?,
            XEvent::SelectionRequest(ev) => self.on_selection_request(ev)?,
            XEvent::SelectionClear(ev) if ev.selection == self.atoms.CLIPBOARD => {
                self.owned = None;
            }
            _ => {}
        }
        Ok(())
    }

    // ---- reading ---------------------------------------------------------

    fn start_read(&mut self, owner: Window, timestamp: u32) -> Result<(), BoxError> {
        if !self.config.enabled {
            // Still report the change so "ignore next copy" bookkeeping can run.
            let _ = self.events.send_blocking(Event::Ignored);
            return Ok(());
        }
        let application = self.source_application(owner);
        self.read = Some(ReadTx {
            timestamp,
            targets: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            results: Vec::new(),
            deadline: Instant::now() + READ_TIMEOUT,
            application,
        });
        self.conn
            .convert_selection(self.win, self.atoms.CLIPBOARD, self.atoms.TARGETS, self.atoms.MACCY_SEL, timestamp)?;
        Ok(())
    }

    fn on_selection_notify(&mut self, ev: SelectionNotifyEvent) -> Result<(), BoxError> {
        let Some(tx) = self.read.as_ref() else { return Ok(()) };
        if ev.selection != self.atoms.CLIPBOARD {
            return Ok(());
        }
        let waiting_targets = tx.current.is_none() && tx.targets.is_empty() && tx.queue.is_empty();
        if waiting_targets {
            if ev.target != self.atoms.TARGETS {
                return Ok(());
            }
            if ev.property == NONE {
                // Owner without TARGETS support: just ask for text.
                let utf8 = self.atoms.UTF8_STRING;
                if let Some(tx) = self.read.as_mut() {
                    tx.targets = vec!["UTF8_STRING".into()];
                    tx.queue.push_back((utf8, "UTF8_STRING".into(), MIME_TEXT.into()));
                }
                return self.request_next();
            }
            let reply = self.conn.get_property(true, self.win, self.atoms.MACCY_SEL, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
            let atoms: Vec<Atom> = reply.value32().map(|it| it.collect()).unwrap_or_default();
            let names = self.atom_names(&atoms)?;
            crate::debug!("clipboard targets: {:?}", names);
            if self.is_ignored(&names) {
                let _ = self.events.send_blocking(Event::Ignored);
                self.read = None;
                return Ok(());
            }
            let wanted = choose_targets(&names, &self.config);
            let mut queue = VecDeque::new();
            for (name, mime) in wanted {
                if let Some(atom) = self.atom_ids.get(&name).copied() {
                    queue.push_back((atom, name, mime));
                }
            }
            if let Some(tx) = self.read.as_mut() {
                tx.targets = names;
                tx.queue = queue;
            }
            return self.request_next();
        }

        let expected = tx.current.as_ref().map(|c| c.target);
        if expected != Some(ev.target) {
            return Ok(()); // stale reply
        }
        if ev.property == NONE {
            if let Some(tx) = self.read.as_mut() {
                tx.current = None;
            }
            return self.request_next();
        }
        let reply = self.conn.get_property(true, self.win, self.atoms.MACCY_SEL, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
        if reply.type_ == self.atoms.INCR {
            if let Some(cur) = self.read.as_mut().and_then(|t| t.current.as_mut()) {
                cur.incr = true;
            }
            if let Some(tx) = self.read.as_mut() {
                tx.deadline = Instant::now() + READ_TIMEOUT;
            }
            return Ok(()); // chunks arrive as PropertyNotify(NewValue)
        }
        if let Some(tx) = self.read.as_mut() {
            if let Some(cur) = tx.current.take() {
                tx.results.push((cur.mime, reply.value));
            }
        }
        self.request_next()
    }

    fn on_property_notify(&mut self, ev: xproto::PropertyNotifyEvent) -> Result<(), BoxError> {
        if ev.window == self.win && ev.atom == self.atoms.MACCY_SEL && ev.state == Property::NEW_VALUE {
            let incr = self
                .read
                .as_ref()
                .and_then(|t| t.current.as_ref())
                .map(|c| c.incr)
                .unwrap_or(false);
            if !incr {
                return Ok(());
            }
            let reply = self.conn.get_property(true, self.win, self.atoms.MACCY_SEL, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
            let tx = self.read.as_mut().expect("read in progress");
            tx.deadline = Instant::now() + READ_TIMEOUT;
            if reply.value.is_empty() {
                if let Some(cur) = tx.current.take() {
                    tx.results.push((cur.mime, cur.buf));
                }
                return self.request_next();
            }
            let cur = tx.current.as_mut().expect("transfer in progress");
            cur.buf.extend_from_slice(&reply.value);
            if cur.buf.len() > MAX_ITEM_BYTES {
                crate::debug!("skipping {}: larger than {} bytes", cur.mime, MAX_ITEM_BYTES);
                tx.current = None;
                return self.request_next();
            }
            return Ok(());
        }
        if ev.state == Property::DELETE {
            self.continue_outgoing(ev.window, ev.atom)?;
        }
        Ok(())
    }

    fn request_next(&mut self) -> Result<(), BoxError> {
        let Some(tx) = self.read.as_mut() else { return Ok(()) };
        if tx.current.is_some() {
            return Ok(());
        }
        match tx.queue.pop_front() {
            Some((atom, _name, mime)) => {
                tx.current = Some(Transfer { target: atom, mime, buf: Vec::new(), incr: false });
                tx.deadline = Instant::now() + READ_TIMEOUT;
                let ts = tx.timestamp;
                self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, atom, self.atoms.MACCY_SEL, ts)?;
            }
            None => {
                let tx = self.read.take().expect("read in progress");
                let contents = normalize(tx.results);
                let _ = self.events.send_blocking(Event::Copied { contents, application: tx.application });
            }
        }
        Ok(())
    }

    fn is_ignored(&self, targets: &[String]) -> bool {
        targets.iter().any(|t| self.config.ignored_types.iter().any(|i| i.eq_ignore_ascii_case(t)))
    }

    fn window_class(&self, w: Window) -> Option<String> {
        let reply = self.conn.get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256).ok()?.reply().ok()?;
        let parts: Vec<String> = reply
            .value
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        parts.last().cloned()
    }

    /// Best-effort guess of the copying app (only possible for X11 apps).
    fn source_application(&self, owner: Window) -> Option<String> {
        if let Some(class) = self.window_class(owner) {
            return Some(class);
        }
        if self.wayland_session {
            return None;
        }
        let reply = self
            .conn
            .get_property(false, self.root, self.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let active = reply.value32()?.next()?;
        if active == NONE {
            return None;
        }
        self.window_class(active)
    }

    // ---- owning ----------------------------------------------------------

    fn handle_command(&mut self, cmd: Command) -> Result<(), BoxError> {
        match cmd {
            Command::Config(config) => self.config = config,
            Command::SetClipboard(contents) => self.take_ownership(contents)?,
            // Owning the clipboard with empty text replaces its contents everywhere.
            // Setting no owner is not passed on to Wayland apps by GNOME, so the old
            // contents would stay pasteable there.
            Command::Clear => self.take_ownership(vec![(MIME_TEXT.to_owned(), Arc::new(Vec::new()))])?,
        }
        Ok(())
    }

    fn take_ownership(&mut self, contents: Vec<(String, Arc<Vec<u8>>)>) -> Result<(), BoxError> {
        let mut entries: Vec<(Atom, Atom, Arc<Vec<u8>>)> = Vec::new();
        let mut push = |engine: &mut Engine, target: &str, type_: Option<&str>, data: Arc<Vec<u8>>| -> Result<(), BoxError> {
            let t = engine.atom(target)?;
            let ty = match type_ {
                Some(name) => engine.atom(name)?,
                None => t,
            };
            if !entries.iter().any(|(e, _, _)| *e == t) {
                entries.push((t, ty, data));
            }
            Ok(())
        };
        let has_text = contents.iter().any(|(m, _)| m == MIME_TEXT);
        for (mime, data) in &contents {
            match mime.as_str() {
                MIME_TEXT => {
                    push(self, "UTF8_STRING", None, data.clone())?;
                    push(self, MIME_TEXT, None, data.clone())?;
                    push(self, "text/plain", None, data.clone())?;
                    push(self, "TEXT", Some("UTF8_STRING"), data.clone())?;
                    push(self, "STRING", None, Arc::new(to_latin1(data)))?;
                }
                MIME_RTF => {
                    push(self, MIME_RTF, None, data.clone())?;
                    push(self, "application/rtf", None, data.clone())?;
                }
                MIME_URI_LIST => {
                    push(self, MIME_URI_LIST, None, data.clone())?;
                    if !contents.iter().any(|(m, _)| m == MIME_GNOME_FILES) {
                        let uris = String::from_utf8_lossy(data);
                        let lines: Vec<&str> =
                            uris.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
                        let gnome = format!("copy\n{}", lines.join("\n"));
                        push(self, MIME_GNOME_FILES, None, Arc::new(gnome.into_bytes()))?;
                    }
                    if !has_text {
                        let paths: Vec<String> = String::from_utf8_lossy(data)
                            .lines()
                            .map(str::trim)
                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                            .map(model::uri_to_display)
                            .collect();
                        let text = Arc::new(paths.join("\n").into_bytes());
                        push(self, "UTF8_STRING", None, text.clone())?;
                        push(self, MIME_TEXT, None, text)?;
                    }
                }
                other => push(self, other, None, data.clone())?,
            }
        }
        self.owned = Some(Owned { entries, timestamp: CURRENT_TIME });
        self.conn.set_selection_owner(self.win, self.atoms.CLIPBOARD, CURRENT_TIME)?;
        let owner = self.conn.get_selection_owner(self.atoms.CLIPBOARD)?.reply()?.owner;
        if owner != self.win {
            self.owned = None;
            return Err("could not take clipboard ownership".into());
        }
        Ok(())
    }

    fn on_selection_request(&mut self, ev: xproto::SelectionRequestEvent) -> Result<(), BoxError> {
        let property = if ev.property == NONE { ev.target } else { ev.property };
        let mut reply_property = NONE;
        if ev.selection == self.atoms.CLIPBOARD && ev.owner == self.win {
            if let Some(owned) = self.owned.as_ref() {
                if ev.target == self.atoms.TARGETS {
                    let mut targets = vec![self.atoms.TARGETS, self.atoms.TIMESTAMP];
                    targets.extend(owned.entries.iter().map(|(t, _, _)| *t));
                    self.conn.change_property32(PropMode::REPLACE, ev.requestor, property, AtomEnum::ATOM, &targets)?;
                    reply_property = property;
                } else if ev.target == self.atoms.TIMESTAMP {
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        ev.requestor,
                        property,
                        AtomEnum::INTEGER,
                        &[owned.timestamp],
                    )?;
                    reply_property = property;
                } else if let Some((_, type_, data)) = owned.entries.iter().find(|(t, _, _)| *t == ev.target) {
                    let (type_, data) = (*type_, data.clone());
                    if data.len() > self.chunk_size {
                        // Large: announce INCR and stream chunks on PropertyNotify(Delete).
                        self.conn.change_window_attributes(
                            ev.requestor,
                            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                        )?;
                        self.conn.change_property32(
                            PropMode::REPLACE,
                            ev.requestor,
                            property,
                            self.atoms.INCR,
                            &[data.len() as u32],
                        )?;
                        self.outgoing.insert(
                            (ev.requestor, property),
                            Outgoing { data, offset: 0, type_, done: false, deadline: Instant::now() + Duration::from_secs(10) },
                        );
                    } else {
                        self.conn.change_property8(PropMode::REPLACE, ev.requestor, property, type_, &data)?;
                    }
                    reply_property = property;
                }
            }
        }
        let notify = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: ev.time,
            requestor: ev.requestor,
            selection: ev.selection,
            target: ev.target,
            property: reply_property,
        };
        self.conn.send_event(false, ev.requestor, EventMask::NO_EVENT, notify)?;
        Ok(())
    }

    fn continue_outgoing(&mut self, window: Window, property: Atom) -> Result<(), BoxError> {
        let Some(out) = self.outgoing.get_mut(&(window, property)) else { return Ok(()) };
        if out.done {
            self.outgoing.remove(&(window, property));
            return Ok(());
        }
        let end = (out.offset + self.chunk_size).min(out.data.len());
        let chunk = out.data[out.offset..end].to_vec();
        let type_ = out.type_;
        out.offset = end;
        out.deadline = Instant::now() + Duration::from_secs(10);
        if chunk.is_empty() {
            out.done = true; // zero-length property ends the transfer
        }
        self.conn.change_property8(PropMode::REPLACE, window, property, type_, &chunk)?;
        if chunk.is_empty() {
            self.outgoing.remove(&(window, property));
            self.conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT))?;
        }
        Ok(())
    }
}

/// Picks which advertised targets to read, mapped to the mime we store them as.
pub fn choose_targets(targets: &[String], config: &WatchConfig) -> Vec<(String, String)> {
    let has = |m: &str| targets.iter().any(|t| t == m);
    let mut wanted: Vec<(String, String)> = Vec::new();
    if config.save_text {
        if let Some(t) = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"]
            .into_iter()
            .find(|t| has(t))
        {
            wanted.push((t.to_owned(), MIME_TEXT.to_owned()));
        }
        if has(MIME_HTML) {
            wanted.push((MIME_HTML.to_owned(), MIME_HTML.to_owned()));
        }
        if let Some(t) = [MIME_RTF, "application/rtf", "text/richtext"].into_iter().find(|t| has(t)) {
            wanted.push((t.to_owned(), MIME_RTF.to_owned()));
        }
    }
    if config.save_images {
        if let Some(t) = IMAGE_MIMES.iter().find(|t| has(t)) {
            wanted.push((t.to_string(), t.to_string()));
        }
    }
    if config.save_files {
        if has(MIME_URI_LIST) {
            wanted.push((MIME_URI_LIST.to_owned(), MIME_URI_LIST.to_owned()));
        }
        if has(MIME_GNOME_FILES) {
            wanted.push((MIME_GNOME_FILES.to_owned(), MIME_GNOME_FILES.to_owned()));
        }
    }
    wanted
}

/// Text targets can come as Latin-1 (`STRING`) or UTF-16 HTML; store UTF-8.
fn normalize(results: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    for (mime, mut data) in results {
        if mime == MIME_TEXT || mime == MIME_HTML || mime == MIME_URI_LIST || mime == MIME_GNOME_FILES {
            if data.len() >= 2 && ((data[0] == 0xff && data[1] == 0xfe) || (data[0] == 0xfe && data[1] == 0xff)) {
                let le = data[0] == 0xff;
                let units: Vec<u16> = data[2..]
                    .chunks_exact(2)
                    .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
                    .collect();
                data = String::from_utf16_lossy(&units).into_bytes();
            } else if std::str::from_utf8(&data).is_err() {
                // Latin-1 (STRING) or broken UTF-8.
                data = data.iter().map(|b| *b as char).collect::<String>().into_bytes();
            }
            while data.last() == Some(&0) {
                data.pop();
            }
        }
        if !out.iter().any(|(m, _)| *m == mime) {
            out.push((mime, data));
        }
    }
    out
}

fn to_latin1(utf8: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(utf8)
        .chars()
        .map(|c| if (c as u32) < 256 { c as u32 as u8 } else { b'?' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WatchConfig {
        WatchConfig { save_text: true, save_images: true, save_files: true, ignored_types: vec![], enabled: true }
    }

    #[test]
    fn picks_best_targets() {
        let targets: Vec<String> = ["TARGETS", "UTF8_STRING", "text/plain;charset=utf-8", "text/html", "image/jpeg", "image/png"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let wanted = choose_targets(&targets, &cfg());
        assert_eq!(
            wanted,
            vec![
                ("text/plain;charset=utf-8".to_string(), MIME_TEXT.to_string()),
                ("text/html".to_string(), MIME_HTML.to_string()),
                ("image/png".to_string(), "image/png".to_string()),
            ]
        );
        let no_images = WatchConfig { save_images: false, ..cfg() };
        assert_eq!(choose_targets(&targets, &no_images).len(), 2);
    }

    #[test]
    fn normalizes_text_encodings() {
        let utf16 = vec![0xff, 0xfe, b'h', 0, b'i', 0];
        let latin1 = vec![b'c', b'a', b'f', 0xe9, 0];
        let out = normalize(vec![(MIME_HTML.into(), utf16), (MIME_TEXT.into(), latin1)]);
        assert_eq!(out[0].1, b"hi");
        assert_eq!(String::from_utf8(out[1].1.clone()).unwrap(), "café");
    }
}
