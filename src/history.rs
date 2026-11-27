//! The clipboard history: adding copies, de-duplication, pins, ordering, limits
//! (Maccy's `History`, `Sorter` and `PinManager`).

use std::rc::Rc;

use crate::model::{now_ms, Content, Item};
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
                History { storage: Some(storage), items }
            }
            Err(e) => {
                eprintln!("clipnest: could not open {}: {e}; history will not be saved", path.display());
                History { storage: None, items: Vec::new() }
            }
        }
    }

    /// No storage; replaced by `open()` once the app instance becomes primary.
    pub fn empty() -> History {
        History { storage: None, items: Vec::new() }
    }

    #[cfg(test)]
    pub fn in_memory() -> History {
        History { storage: Some(Storage::open_in_memory()), items: Vec::new() }
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

    pub fn storage_size(&self) -> u64 {
        self.storage.as_ref().map(|s| s.size_bytes()).unwrap_or(0)
    }

    fn save(&self, item: &Item) {
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

        // An existing item that already contains everything we just copied.
        if let Some(pos) = self.items.iter().position(|e| e.supersedes(&item)) {
            let existing = &mut self.items[pos];
            existing.last_copied_at = item.last_copied_at;
            existing.number_of_copies += 1;
            if existing.application.is_none() {
                existing.application = item.application.take();
            }
            let existing = existing.clone();
            self.save(&existing);
            return Added::Existing(existing.id);
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
            if let Some(storage) = &self.storage {
                let _ = storage.delete_item(old.id);
            }
        }

        if let Some(storage) = &self.storage {
            if let Err(e) = storage.insert_item(&mut item) {
                eprintln!("clipnest: could not save copy: {e}");
            }
        }
        if item.id == 0 {
            // Not persisted (no database): still keep it for this session.
            item.id = -(self.items.len() as i64) - 1 - now_ms().rem_euclid(1_000_000);
        }
        let id = item.id;
        self.items.push(item);
        self.limit_size(settings);
        Added::New(id)
    }

    /// Image items whose text has not been recognized yet, newest first.
    pub fn pending_ocr(&self) -> Vec<i64> {
        let mut items: Vec<&Item> = self.items.iter().filter(|i| i.is_image_item() && i.ocr_text.is_none()).collect();
        items.sort_by(|a, b| b.last_copied_at.cmp(&a.last_copied_at));
        items.iter().map(|i| i.id).collect()
    }

    pub fn set_ocr_text(&mut self, id: i64, text: String) {
        if let Some(pos) = self.position(id) {
            self.items[pos].ocr_text = Some(text);
            let item = self.items[pos].clone();
            self.save(&item);
        }
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
            self.items.remove(pos);
            if let Some(storage) = &self.storage {
                let _ = storage.delete_item(id);
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

    /// Pins with a random free key, or unpins. Returns the new pin.
    pub fn toggle_pin(&mut self, id: i64, settings: &mut Settings) -> Option<String> {
        let pos = self.position(id)?;
        if let Some(pin) = self.items[pos].pin.take() {
            settings.pin_order.retain(|p| *p != pin);
            let item = self.items[pos].clone();
            self.save(&item);
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
}
