//! The clipboard history: adding copies, de-duplication, pins, ordering, limits
//! (Maccy's `History`, `Sorter` and `PinManager`), and sensitive items that are
//! kept in memory only.

use std::rc::Rc;

use crate::inspect;
use crate::model::{now_ms, Content, Item, Secret};
use crate::settings::{Settings, SortBy};
use crate::storage::Storage;

/// Keys a pinned item can be bound to (used as Ctrl+key in the window). Letters the
/// window already uses with Ctrl are excluded: a/c/v/x/z (editing), f (find),
/// h/u/w (search editing, close), j/k/n/p (navigation) and q (quit).
const PIN_KEYS: &[&str] = &[
    "b", "d", "e", "g", "i", "l", "m", "o", "r", "s", "t", "y", "[", "]", ";", "'", ".", "/", "-", "=", "\\",
];

pub fn supported_pins() -> Vec<String> {
    PIN_KEYS.iter().map(|s| s.to_string()).collect()
}

pub enum Added {
    New(i64),
    Existing(i64),
}

pub struct History {
    storage: Option<Storage>,
    items: Vec<Item>,
    /// Ids for items that are not in the database count down from -1.
    next_temp_id: i64,
}

/// Looks for secrets and hidden characters in an item. `fresh` copies with a
/// secret are kept in memory only, until `settings.secret_lifetime` is over.
fn inspect_item(item: &mut Item, settings: &Settings, fresh: bool) {
    let text = item.text();
    item.hidden = text.as_deref().map(|t| inspect::find_hidden(t).len()).unwrap_or(0);
    item.danger = text.as_deref().and_then(inspect::dangerous_command);
    let label = if !settings.protect_secrets {
        None
    } else if item.is_image_item() {
        item.ocr_text.as_deref().and_then(|t| inspect::secret_label(t, false))
    } else {
        item.inspected_text().and_then(|t| inspect::secret_label(&t, true))
    };
    let expires_at = (fresh && !item.is_pinned()).then(|| now_ms() + settings.secret_lifetime_ms());
    item.secret = label.map(|label| Secret { label, expires_at });
}

impl History {
    pub fn open() -> History {
        let path = crate::config::database_path();
        match Storage::open(&path) {
            Ok(storage) => {
                let items = storage.load_items().unwrap_or_else(|e| {
                    eprintln!("clipnest: could not read history: {e}");
                    Vec::new()
                });
                History { storage: Some(storage), items, next_temp_id: 0 }
            }
            Err(e) => {
                eprintln!("clipnest: could not open {}: {e}; history will not be saved", path.display());
                History { storage: None, items: Vec::new(), next_temp_id: 0 }
            }
        }
    }

    /// No storage; replaced by `open()` once the app instance becomes primary.
    pub fn empty() -> History {
        History { storage: None, items: Vec::new(), next_temp_id: 0 }
    }

    #[cfg(test)]
    pub fn in_memory() -> History {
        History { storage: Some(Storage::open_in_memory()), items: Vec::new(), next_temp_id: 0 }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn get(&self, id: i64) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    fn position(&self, id: i64) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    fn temp_id(&mut self) -> i64 {
        self.next_temp_id -= 1;
        self.next_temp_id
    }

    pub fn storage_size(&self) -> u64 {
        self.storage.as_ref().map(|s| s.size_bytes()).unwrap_or(0)
    }

    fn save(&self, item: &Item) {
        if item.is_ephemeral() {
            return; // never written to disk
        }
        if let Some(storage) = &self.storage {
            if let Err(e) = storage.update_item(item) {
                eprintln!("clipnest: could not save item: {e}");
            }
        }
    }

    /// Ids of pinned and unpinned items, each in display order.
    pub fn ordered(&self, settings: &Settings) -> (Vec<i64>, Vec<i64>) {
        let mut unpinned: Vec<&Item> = self.items.iter().filter(|i| !i.is_pinned()).collect();
        match settings.sort_by {
            SortBy::LastCopiedAt => unpinned.sort_by(|a, b| b.last_copied_at.cmp(&a.last_copied_at)),
            SortBy::FirstCopiedAt => unpinned.sort_by(|a, b| b.first_copied_at.cmp(&a.first_copied_at)),
            SortBy::NumberOfCopies => unpinned.sort_by(|a, b| {
                b.number_of_copies
                    .cmp(&a.number_of_copies)
                    .then(b.last_copied_at.cmp(&a.last_copied_at))
            }),
        }
        let mut pinned: Vec<&Item> = self.items.iter().filter(|i| i.is_pinned()).collect();
        let order = &settings.pin_order;
        pinned.sort_by_key(|i| {
            i.pin
                .as_ref()
                .and_then(|p| order.iter().position(|o| o == p))
                .unwrap_or(usize::MAX)
        });
        (pinned.iter().map(|i| i.id).collect(), unpinned.iter().map(|i| i.id).collect())
    }

    /// Records a copy. Returns the id of the new or matching existing item.
    pub fn add(&mut self, contents: Vec<Content>, application: Option<String>, settings: &mut Settings) -> Added {
        let mut item = Item::new(contents, application);
        item.title = item.generate_title(settings.show_special_symbols);
        inspect_item(&mut item, settings, true);
        if item.secret.is_some() {
            item.title = item.generate_title(settings.show_special_symbols); // masked
        }

        // An existing item that already contains everything we just copied.
        if let Some(pos) = self.items.iter().position(|e| e.supersedes(&item)) {
            let existing = &mut self.items[pos];
            existing.last_copied_at = item.last_copied_at;
            existing.number_of_copies += 1;
            if existing.application.is_none() {
                existing.application = item.application.take();
            }
            let id = existing.id;
            match item.secret {
                // A secret copied again: (re)start its countdown and take it off the disk.
                Some(Secret { label, expires_at: Some(at) }) if !self.items[pos].is_pinned() => {
                    self.make_ephemeral(pos, label, at, settings);
                }
                _ => {
                    let existing = self.items[pos].clone();
                    self.save(&existing);
                }
            }
            return Added::Existing(id);
        }

        // The new copy is a richer version of an existing item: replace it.
        if let Some(pos) = self.items.iter().position(|e| item.supersedes(e)) {
            let old = self.items.remove(pos);
            item.first_copied_at = old.first_copied_at;
            item.number_of_copies = old.number_of_copies + 1;
            item.pin = old.pin.clone();
            if item.application.is_none() {
                item.application = old.application.clone();
            }
            if item.is_pinned() {
                // Pins stay on disk (shown masked if sensitive).
                if let Some(secret) = item.secret.as_mut() {
                    secret.expires_at = None;
                }
            }
            if let Some(storage) = &self.storage {
                let _ = storage.delete_item(old.id);
                if old.secret.is_some() || item.secret.is_some() {
                    storage.scrub();
                }
            }
        }

        if !item.is_ephemeral() {
            if let Some(storage) = &self.storage {
                if let Err(e) = storage.insert_item(&mut item) {
                    eprintln!("clipnest: could not save copy: {e}");
                }
            }
        }
        if item.id == 0 {
            // Sensitive, or no database: keep it for this session only.
            item.id = self.temp_id();
        }
        let id = item.id;
        self.items.push(item);
        self.limit_size(settings);
        Added::New(id)
    }

    /// Turns an item into a sensitive one kept in memory only, removing it from disk.
    fn make_ephemeral(&mut self, pos: usize, label: &'static str, expires_at: i64, settings: &Settings) {
        if !self.items[pos].is_ephemeral() {
            if let Some(storage) = &self.storage {
                // Large images are still on disk: read them before deleting.
                let item = &mut self.items[pos];
                for content in item.contents.iter_mut().filter(|c| c.data.is_none()) {
                    content.data = storage.load_content(content.id);
                }
                let _ = storage.delete_item(item.id);
                storage.scrub();
            }
        }
        let item = &mut self.items[pos];
        item.secret = Some(Secret { label, expires_at: Some(expires_at) });
        item.title = item.generate_title(settings.show_special_symbols);
    }

    /// Removes sensitive items whose time is up and returns them.
    pub fn expire(&mut self, now: i64) -> Vec<Item> {
        let (gone, kept): (Vec<Item>, Vec<Item>) =
            std::mem::take(&mut self.items).into_iter().partition(|i| i.expires_at().is_some_and(|t| t <= now));
        self.items = kept;
        gone
    }

    /// Restarts the countdown of a sensitive item (it was just copied again).
    /// Returns the new expiry time.
    pub fn renew_secret(&mut self, id: i64, settings: &Settings) -> Option<i64> {
        let pos = self.position(id)?;
        let secret = self.items[pos].secret.as_mut().filter(|s| s.expires_at.is_some())?;
        let at = now_ms() + settings.secret_lifetime_ms();
        secret.expires_at = Some(at);
        Some(at)
    }

    /// Image items whose text has not been recognized yet, newest first.
    pub fn pending_ocr(&self) -> Vec<i64> {
        let mut items: Vec<&Item> = self.items.iter().filter(|i| i.is_image_item() && i.ocr_text.is_none()).collect();
        items.sort_by(|a, b| b.last_copied_at.cmp(&a.last_copied_at));
        items.iter().map(|i| i.id).collect()
    }

    /// Stores text recognized in an image. If it shows a secret (a screenshot of
    /// a key, say), the image becomes sensitive; returns its expiry time then.
    pub fn set_ocr_text(&mut self, id: i64, text: String, settings: &Settings) -> Option<i64> {
        let pos = self.position(id)?;
        let label = if settings.protect_secrets { inspect::secret_label(&text, false) } else { None };
        self.items[pos].ocr_text = Some(text);
        if let Some(label) = label {
            if !self.items[pos].is_pinned() {
                let at = self.items[pos].expires_at().unwrap_or_else(|| now_ms() + settings.secret_lifetime_ms());
                self.make_ephemeral(pos, label, at, settings);
                return Some(at);
            }
            self.items[pos].secret = Some(Secret { label, expires_at: None });
        }
        let item = self.items[pos].clone();
        self.save(&item);
        None
    }

    /// Re-checks every saved item (at startup, or when protection was switched).
    pub fn refresh_inspection(&mut self, settings: &Settings) {
        for pos in 0..self.items.len() {
            if self.items[pos].is_ephemeral() {
                continue; // keeps its countdown
            }
            inspect_item(&mut self.items[pos], settings, false);
            let title = self.items[pos].generate_title(settings.show_special_symbols);
            if title != self.items[pos].title {
                self.items[pos].title = title;
                let item = self.items[pos].clone();
                self.save(&item);
            }
        }
    }

    /// Saved items that look sensitive (copied before protection was on, or pinned).
    pub fn saved_secret_count(&self) -> usize {
        self.items.iter().filter(|i| i.secret.is_some() && !i.is_ephemeral()).count()
    }

    /// Deletes the items counted by `saved_secret_count`. Returns how many.
    pub fn delete_saved_secrets(&mut self, settings: &mut Settings) -> usize {
        let ids: Vec<i64> =
            self.items.iter().filter(|i| i.secret.is_some() && !i.is_ephemeral()).map(|i| i.id).collect();
        for id in &ids {
            self.delete(*id, settings);
        }
        ids.len()
    }

    /// Selecting an item from the popup counts as copying it again.
    pub fn bump(&mut self, id: i64) {
        if let Some(pos) = self.position(id) {
            let item = &mut self.items[pos];
            item.last_copied_at = now_ms();
            item.number_of_copies += 1;
            let item = item.clone();
            self.save(&item);
        }
    }

    pub fn limit_size(&mut self, settings: &Settings) {
        let max = settings.history_size.max(1) as usize;
        let (_, unpinned) = self.ordered(settings);
        if unpinned.len() > max {
            for id in &unpinned[max..] {
                self.remove(*id);
            }
        }
    }

    fn remove(&mut self, id: i64) {
        if let Some(pos) = self.position(id) {
            let item = self.items.remove(pos);
            if item.is_ephemeral() {
                return; // was never on disk
            }
            if let Some(storage) = &self.storage {
                let _ = storage.delete_item(id);
                if item.secret.is_some() {
                    storage.scrub();
                }
            }
        }
    }

    pub fn delete(&mut self, id: i64, settings: &mut Settings) {
        if let Some(pin) = self.get(id).and_then(|i| i.pin.clone()) {
            settings.pin_order.retain(|p| *p != pin);
        }
        self.remove(id);
    }

    pub fn clear(&mut self) {
        self.items.retain(|i| i.is_pinned());
        if let Some(storage) = &self.storage {
            let _ = storage.delete_unpinned();
        }
    }

    pub fn clear_all(&mut self, settings: &mut Settings) {
        self.items.clear();
        settings.pin_order.clear();
        if let Some(storage) = &self.storage {
            let _ = storage.delete_all();
        }
    }

    pub fn available_pins(&self) -> Vec<String> {
        let used: Vec<&String> = self.items.iter().filter_map(|i| i.pin.as_ref()).collect();
        supported_pins().into_iter().filter(|p| !used.contains(&p)).collect()
    }

    /// Sensitive items in memory can't be pinned: pins are saved to disk.
    pub fn can_pin(&self, id: i64) -> bool {
        self.get(id).is_some_and(|i| !i.is_ephemeral())
    }

    /// Pins with a random free key, or unpins. Returns the new pin.
    pub fn toggle_pin(&mut self, id: i64, settings: &mut Settings) -> Option<String> {
        let pos = self.position(id)?;
        if let Some(pin) = self.items[pos].pin.take() {
            settings.pin_order.retain(|p| *p != pin);
            let item = self.items[pos].clone();
            self.save(&item);
            return None;
        }
        if self.items[pos].is_ephemeral() {
            return None;
        }
        let available = self.available_pins();
        if available.is_empty() {
            return None;
        }
        let pick = gtk::glib::random_int_range(0, available.len() as i32) as usize;
        let pin = available[pick].clone();
        self.items[pos].pin = Some(pin.clone());
        settings.pin_order.retain(|p| *p != pin);
        settings.pin_order.push(pin.clone());
        let item = self.items[pos].clone();
        self.save(&item);
        Some(pin)
    }

    /// Moves a pinned item up or down among the pins.
    pub fn move_pin(&mut self, id: i64, delta: i32, settings: &mut Settings) {
        let (pinned, _) = self.ordered(settings);
        let Some(from) = pinned.iter().position(|p| *p == id) else { return };
        let to = (from as i32 + delta).clamp(0, pinned.len() as i32 - 1) as usize;
        let mut ids = pinned;
        let moved = ids.remove(from);
        ids.insert(to, moved);
        settings.pin_order = ids.iter().filter_map(|id| self.get(*id).and_then(|i| i.pin.clone())).collect();
    }

    /// Regenerates titles, e.g. after toggling "show special symbols".
    pub fn refresh_titles(&mut self, show_special_symbols: bool) {
        for pos in 0..self.items.len() {
            let title = self.items[pos].generate_title(show_special_symbols);
            if title != self.items[pos].title {
                self.items[pos].title = title;
                let item = self.items[pos].clone();
                self.save(&item);
            }
        }
    }

    pub fn content_data(&self, content: &Content) -> Option<Rc<[u8]>> {
        content
            .data
            .clone()
            .or_else(|| self.storage.as_ref().and_then(|s| s.load_content(content.id)))
    }

    /// All contents of an item, loading lazy blobs. With `plain_only`, rich text
    /// and images are dropped (Maccy's "paste without formatting").
    pub fn clipboard_contents(&self, id: i64, plain_only: bool) -> Vec<(String, Rc<[u8]>)> {
        let Some(item) = self.get(id) else { return Vec::new() };
        let has_text = item.content(crate::model::MIME_TEXT).is_some();
        item.contents
            .iter()
            .filter(|c| {
                if !plain_only || !has_text {
                    return true;
                }
                matches!(
                    crate::model::kind_of(&c.mime),
                    Some(crate::model::Kind::Files)
                ) || c.mime == crate::model::MIME_TEXT
            })
            .filter_map(|c| self.content_data(c).map(|d| (c.mime.clone(), d)))
            .collect()
    }

    #[cfg(test)]
    fn stored_count(&self) -> usize {
        self.storage.as_ref().map(|s| s.load_items().unwrap().len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MIME_HTML, MIME_TEXT};

    fn text(s: &str) -> Vec<Content> {
        vec![Content::new(MIME_TEXT, s.as_bytes().to_vec())]
    }

    fn titles(h: &History, ids: &[i64]) -> Vec<String> {
        ids.iter().map(|id| h.get(*id).unwrap().title.clone()).collect()
    }

    fn fake_key() -> String {
        format!("sk-proj-{}", "Ab3dEf9h".repeat(6))
    }

    #[test]
    fn duplicates_are_merged_and_bumped() {
        let mut s = Settings::default();
        let mut h = History::in_memory();
        h.add(text("a"), None, &mut s);
        std::thread::sleep(std::time::Duration::from_millis(2));
        h.add(text("b"), None, &mut s);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let again = h.add(text("a"), None, &mut s);
        assert!(matches!(again, Added::Existing(_)));
        let (_, unpinned) = h.ordered(&s);
        assert_eq!(titles(&h, &unpinned), vec!["a", "b"]);
        assert_eq!(h.items().iter().find(|i| i.title == "a").unwrap().number_of_copies, 2);
    }

    #[test]
    fn richer_copy_replaces_plain_one() {
        let mut s = Settings::default();
        let mut h = History::in_memory();
        h.add(text("x"), None, &mut s);
        let mut rich = text("x");
        rich.push(Content::new(MIME_HTML, b"<b>x</b>".to_vec()));
        h.add(rich, None, &mut s);
        assert_eq!(h.items().len(), 1);
        assert!(h.items()[0].has_rich_text());
        assert_eq!(h.items()[0].number_of_copies, 2);
    }

    #[test]
    fn size_limit_keeps_pins() {
        let mut s = Settings { history_size: 2, ..Settings::default() };
        let mut h = History::in_memory();
        let Added::New(first) = h.add(text("1"), None, &mut s) else { panic!() };
        h.toggle_pin(first, &mut s).unwrap();
        for t in ["2", "3", "4"] {
            std::thread::sleep(std::time::Duration::from_millis(2));
            h.add(text(t), None, &mut s);
        }
        let (pinned, unpinned) = h.ordered(&s);
        assert_eq!(titles(&h, &pinned), vec!["1"]);
        assert_eq!(titles(&h, &unpinned), vec!["4", "3"]);
    }

    #[test]
    fn pins_are_unique_and_ordered() {
        let mut s = Settings::default();
        let mut h = History::in_memory();
        let mut ids = Vec::new();
        for t in ["a", "b", "c"] {
            if let Added::New(id) = h.add(text(t), None, &mut s) {
                ids.push(id);
            }
        }
        let p1 = h.toggle_pin(ids[0], &mut s).unwrap();
        let p2 = h.toggle_pin(ids[2], &mut s).unwrap();
        assert_ne!(p1, p2);
        let (pinned, _) = h.ordered(&s);
        assert_eq!(pinned, vec![ids[0], ids[2]]);
        h.move_pin(ids[2], -1, &mut s);
        assert_eq!(h.ordered(&s).0, vec![ids[2], ids[0]]);
        assert_eq!(h.toggle_pin(ids[0], &mut s), None);
        assert_eq!(s.pin_order, vec![p2]);
        h.clear();
        assert_eq!(h.items().len(), 1);
    }

    #[test]
    fn secrets_stay_in_memory_masked_and_expire() {
        let mut s = Settings::default();
        let mut h = History::in_memory();
        h.add(text("plain"), None, &mut s);
        let key = fake_key();
        let Added::New(id) = h.add(text(&format!("OPENAI={key}")), None, &mut s) else { panic!() };
        assert!(id < 0, "not stored in the database");
        assert_eq!(h.stored_count(), 1);
        let item = h.get(id).unwrap();
        assert!(item.is_ephemeral());
        assert!(!item.title.contains(&key) && item.title.starts_with("OPENAI=sk-proj-••••"));
        assert_eq!(item.text().as_deref(), Some(format!("OPENAI={key}").as_str()));
        assert_eq!(h.toggle_pin(id, &mut s), None);
        assert!(h.expire(now_ms()).is_empty());
        let gone = h.expire(now_ms() + s.secret_lifetime_ms() + 1);
        assert_eq!(gone.len(), 1);
        assert!(h.get(id).is_none());
        assert_eq!(h.items().len(), 1);
    }

    #[test]
    fn saved_secret_copied_again_leaves_the_disk() {
        let mut s = Settings { protect_secrets: false, ..Settings::default() };
        let mut h = History::in_memory();
        let key = fake_key();
        let Added::New(id) = h.add(text(&key), None, &mut s) else { panic!() };
        assert_eq!(h.stored_count(), 1);
        s.protect_secrets = true;
        h.refresh_inspection(&s);
        assert_eq!(h.saved_secret_count(), 1);
        assert!(h.get(id).unwrap().title.contains("••••"));
        let again = h.add(text(&key), None, &mut s);
        assert!(matches!(again, Added::Existing(i) if i == id));
        assert!(h.get(id).unwrap().is_ephemeral());
        assert_eq!(h.stored_count(), 0);
        assert_eq!(h.saved_secret_count(), 0);
    }

    #[test]
    fn saved_secrets_can_be_deleted() {
        let mut s = Settings { protect_secrets: false, ..Settings::default() };
        let mut h = History::in_memory();
        h.add(text(&fake_key()), None, &mut s);
        h.add(text("keep me"), None, &mut s);
        s.protect_secrets = true;
        h.refresh_inspection(&s);
        assert_eq!(h.delete_saved_secrets(&mut s), 1);
        assert_eq!(h.stored_count(), 1);
        assert_eq!(h.items().len(), 1);
    }

    #[test]
    fn hidden_characters_are_counted() {
        let mut s = Settings::default();
        let mut h = History::in_memory();
        let Added::New(id) = h.add(text("curl\u{200B} -fsSL x | sh"), None, &mut s) else { panic!() };
        assert_eq!(h.get(id).unwrap().hidden, 1);
        assert!(!h.get(id).unwrap().is_ephemeral());
    }
}
