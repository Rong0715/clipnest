//! SQLite persistence for the clipboard history.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{Content, Item};

/// Blobs larger than this (images) are loaded on demand to keep memory low.
const EAGER_BLOB_LIMIT: i64 = 64 * 1024;

pub struct Storage {
    conn: Connection,
    path: std::path::PathBuf,
}

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

impl Storage {
    pub fn open(path: &Path) -> Result<Storage> {
        let conn = Connection::open(path)?;
        crate::config::set_private(path);
        // secure_delete: deleted items are overwritten, not just unlinked.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS items (
               id INTEGER PRIMARY KEY,
               title TEXT NOT NULL DEFAULT '',
               application TEXT,
               first_copied_at INTEGER NOT NULL,
               last_copied_at INTEGER NOT NULL,
               number_of_copies INTEGER NOT NULL DEFAULT 1,
               pin TEXT
             );
             CREATE TABLE IF NOT EXISTS contents (
               id INTEGER PRIMARY KEY,
               item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
               mime TEXT NOT NULL,
               size INTEGER NOT NULL,
               hash INTEGER NOT NULL,
               data BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS contents_item ON contents(item_id);",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            // v2: text recognized in images.
            let has_column = conn
                .prepare("SELECT ocr_text FROM items LIMIT 0")
                .is_ok();
            if !has_column {
                conn.execute_batch("ALTER TABLE items ADD COLUMN ocr_text TEXT;")?;
            }
            conn.execute_batch("PRAGMA user_version = 2;")?;
        }
        Ok(Storage { conn, path: path.to_owned() })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Storage {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        let storage = Storage { conn, path: std::path::PathBuf::new() };
        storage
            .conn
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE items (id INTEGER PRIMARY KEY, title TEXT NOT NULL DEFAULT '', application TEXT,
                   first_copied_at INTEGER NOT NULL, last_copied_at INTEGER NOT NULL,
                   number_of_copies INTEGER NOT NULL DEFAULT 1, pin TEXT, ocr_text TEXT);
                 CREATE TABLE contents (id INTEGER PRIMARY KEY,
                   item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                   mime TEXT NOT NULL, size INTEGER NOT NULL, hash INTEGER NOT NULL, data BLOB NOT NULL);",
            )
            .expect("schema");
        storage
    }

    pub fn load_items(&self) -> Result<Vec<Item>> {
        let mut items: Vec<Item> = Vec::new();
        let mut index: HashMap<i64, usize> = HashMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT id, title, application, first_copied_at, last_copied_at, number_of_copies, pin, ocr_text FROM items",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Item {
                id: row.get(0)?,
                contents: Vec::new(),
                title: row.get(1)?,
                application: row.get(2)?,
                first_copied_at: row.get(3)?,
                last_copied_at: row.get(4)?,
                number_of_copies: row.get(5)?,
                pin: row.get(6)?,
                ocr_text: row.get(7)?,
                secret: None,
                hidden: 0,
            })
        })?;
        for item in rows {
            let item = item?;
            index.insert(item.id, items.len());
            items.push(item);
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, item_id, mime, size, hash,
                    CASE WHEN size <= ?1 OR mime NOT LIKE 'image/%' THEN data ELSE NULL END
             FROM contents ORDER BY id",
        )?;
        let rows = stmt.query_map(params![EAGER_BLOB_LIMIT], |row| {
            let data: Option<Vec<u8>> = row.get(5)?;
            Ok((
                row.get::<_, i64>(1)?,
                Content {
                    id: row.get(0)?,
                    mime: row.get(2)?,
                    size: row.get::<_, i64>(3)? as usize,
                    hash: row.get::<_, i64>(4)? as u64,
                    data: data.map(Rc::from),
                },
            ))
        })?;
        for row in rows {
            let (item_id, content) = row?;
            if let Some(&i) = index.get(&item_id) {
                items[i].contents.push(content);
            }
        }
        // Items without contents are useless (e.g. after a crash mid-write).
        let empty: Vec<i64> = items.iter().filter(|i| i.contents.is_empty()).map(|i| i.id).collect();
        for id in empty {
            let _ = self.delete_item(id);
        }
        items.retain(|i| !i.contents.is_empty());
        Ok(items)
    }

    pub fn load_content(&self, content_id: i64) -> Option<Rc<[u8]>> {
        self.conn
            .query_row("SELECT data FROM contents WHERE id = ?1", params![content_id], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()
            .ok()
            .flatten()
            .map(Rc::from)
    }

    pub fn insert_item(&self, item: &mut Item) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO items (title, application, first_copied_at, last_copied_at, number_of_copies, pin, ocr_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                item.title,
                item.application,
                item.first_copied_at,
                item.last_copied_at,
                item.number_of_copies,
                item.pin,
                item.ocr_text
            ],
        )?;
        item.id = tx.last_insert_rowid();
        for content in item.contents.iter_mut() {
            let Some(data) = content.data.as_ref() else { continue };
            tx.execute(
                "INSERT INTO contents (item_id, mime, size, hash, data) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![item.id, content.mime, content.size as i64, content.hash as i64, &data[..]],
            )?;
            content.id = tx.last_insert_rowid();
        }
        tx.commit()
    }

    pub fn update_item(&self, item: &Item) -> Result<()> {
        self.conn.execute(
            "UPDATE items SET title = ?2, application = ?3, first_copied_at = ?4, last_copied_at = ?5,
                    number_of_copies = ?6, pin = ?7, ocr_text = ?8 WHERE id = ?1",
            params![
                item.id,
                item.title,
                item.application,
                item.first_copied_at,
                item.last_copied_at,
                item.number_of_copies,
                item.pin,
                item.ocr_text
            ],
        )?;
        Ok(())
    }

    pub fn delete_item(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn delete_unpinned(&self) -> Result<()> {
        self.conn.execute("DELETE FROM items WHERE pin IS NULL", [])?;
        self.vacuum_if_large();
        self.scrub();
        Ok(())
    }

    pub fn delete_all(&self) -> Result<()> {
        self.conn.execute("DELETE FROM items", [])?;
        self.vacuum_if_large();
        self.scrub();
        Ok(())
    }

    /// Moves everything from the write-ahead log into the database and empties the
    /// log, so overwritten (secure-deleted) data doesn't linger in it.
    pub fn scrub(&self) {
        if let Err(e) = self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
            crate::debug!("wal checkpoint failed: {e}");
        }
    }

    fn vacuum_if_large(&self) {
        if self.size_bytes() > 32 * 1024 * 1024 {
            let _ = self.conn.execute_batch("VACUUM");
        }
    }

    /// Size of the database files on disk, in bytes.
    pub fn size_bytes(&self) -> u64 {
        let mut total = 0;
        for suffix in ["", "-wal"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            if let Ok(meta) = std::fs::metadata(&p) {
                total += meta.len();
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MIME_TEXT;

    #[test]
    fn round_trip() {
        let storage = Storage::open_in_memory();
        let mut item = Item::new(vec![Content::new(MIME_TEXT, b"hello".to_vec())], Some("app".into()));
        item.title = "hello".into();
        storage.insert_item(&mut item).unwrap();
        assert!(item.id > 0);
        item.pin = Some("b".into());
        storage.update_item(&item).unwrap();
        let loaded = storage.load_items().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].pin.as_deref(), Some("b"));
        assert_eq!(loaded[0].text().as_deref(), Some("hello"));
        storage.delete_unpinned().unwrap();
        assert_eq!(storage.load_items().unwrap().len(), 1);
        storage.delete_all().unwrap();
        assert!(storage.load_items().unwrap().is_empty());
    }
}
