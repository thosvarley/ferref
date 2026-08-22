// This imports the rusqlite library's Connection type
// Connection represents a connection to a SQLite database file
use rusqlite::{Connection, Result, Row};

// This imports the Path type from Rust's standard library
// Path is used for working with file system paths
use std::path::Path;

use crate::models::{now, Author, Entry};

// Shared DDL for both init_db and the in-memory test connection.
// PRAGMA foreign_keys is per-connection in SQLite, so it's set here too —
// without it, `ON DELETE CASCADE` on authors.entry_id is a no-op.
const SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_type TEXT NOT NULL,
    cite_key TEXT UNIQUE NOT NULL,
    title TEXT NOT NULL,
    year INTEGER,
    journal TEXT,
    volume TEXT,
    pages TEXT,
    doi TEXT,
    url TEXT,
    abstract TEXT,
    date_added INTEGER NOT NULL,
    date_modified INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS authors (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id INTEGER NOT NULL,
    first_name TEXT,
    last_name TEXT NOT NULL,
    author_order INTEGER NOT NULL,
    FOREIGN KEY (entry_id) REFERENCES entries(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_authors_entry ON authors(entry_id);
CREATE INDEX IF NOT EXISTS idx_entries_cite_key ON entries(cite_key);
CREATE INDEX IF NOT EXISTS idx_entries_year ON entries(year);
"#;

fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
}

// 'pub' means "public" - other files can use this function
// 'fn' declares a function
// The function takes a 'path' parameter of type &Path
// &Path means "a reference to a Path" - references let us use data without taking ownership
// -> Result<Connection> means this function returns a Result type
// Result is Rust's way of handling errors - it's either Ok(Connection) or Err(error)
pub fn init_db(path: &Path) -> Result<Connection> {
    // Connection::open creates or opens a database file at the given path
    // The ? operator says "if this fails, return the error immediately"
    // If it succeeds, unwrap the Ok value and continue
    let conn = Connection::open(path)?;

    create_schema(&conn)?;

    // If everything succeeded, return Ok(conn)
    // The connection is now ready to use
    Ok(conn)
}

// Builds an Entry from an `entries` row. Authors aren't part of this row —
// callers attach them separately (they come from a different table/query).
fn entry_from_row(row: &Row) -> Result<Entry> {
    Ok(Entry {
        id: row.get("id")?,
        entry_type: row.get("entry_type")?,
        cite_key: row.get("cite_key")?,
        title: row.get("title")?,
        authors: Vec::new(),
        year: row.get("year")?,
        journal: row.get("journal")?,
        volume: row.get("volume")?,
        pages: row.get("pages")?,
        doi: row.get("doi")?,
        url: row.get("url")?,
        abstract_text: row.get("abstract")?,
        date_added: row.get("date_added")?,
        date_modified: row.get("date_modified")?,
    })
}

fn authors_for_entry(conn: &Connection, entry_id: i64) -> Result<Vec<Author>> {
    let mut stmt = conn.prepare(
        "SELECT first_name, last_name FROM authors WHERE entry_id = ?1 ORDER BY author_order",
    )?;
    let authors = stmt
        .query_map([entry_id], |row| {
            Ok(Author {
                first_name: row.get(0)?,
                last_name: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<Author>>>()?;
    Ok(authors)
}

fn insert_authors(conn: &Connection, entry_id: i64, authors: &[Author]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO authors (entry_id, first_name, last_name, author_order) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (order, author) in authors.iter().enumerate() {
        stmt.execute(rusqlite::params![
            entry_id,
            author.first_name,
            author.last_name,
            order as i64
        ])?;
    }
    Ok(())
}

// Takes &Connection (not &mut) per the spec, so the transaction is opened
// with `unchecked_transaction` rather than `Connection::transaction`, which
// requires &mut self. Still a real transaction — entry + authors commit or
// roll back together.
pub fn insert_entry(conn: &Connection, entry: &Entry) -> Result<i64> {
    let tx = conn.unchecked_transaction()?;

    tx.execute(
        r#"
        INSERT INTO entries
            (entry_type, cite_key, title, year, journal, volume, pages, doi, url, abstract, date_added, date_modified)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
        rusqlite::params![
            entry.entry_type,
            entry.cite_key,
            entry.title,
            entry.year,
            entry.journal,
            entry.volume,
            entry.pages,
            entry.doi,
            entry.url,
            entry.abstract_text,
            entry.date_added,
            entry.date_modified,
        ],
    )?;

    let entry_id = tx.last_insert_rowid();
    insert_authors(&tx, entry_id, &entry.authors)?;

    tx.commit()?;
    Ok(entry_id)
}

pub fn get_entry(conn: &Connection, cite_key: &str) -> Result<Option<Entry>> {
    let mut stmt = conn.prepare("SELECT * FROM entries WHERE cite_key = ?1")?;
    let mut rows = stmt.query([cite_key])?;

    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    let mut entry = entry_from_row(row)?;
    entry.authors = authors_for_entry(conn, entry.id.unwrap())?;
    Ok(Some(entry))
}

pub fn list_entries(conn: &Connection) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare("SELECT * FROM entries ORDER BY id")?;
    let mut entries = stmt
        .query_map([], entry_from_row)?
        .collect::<Result<Vec<Entry>>>()?;

    for entry in &mut entries {
        entry.authors = authors_for_entry(conn, entry.id.unwrap())?;
    }
    Ok(entries)
}

// Updates the entry row, then replaces its authors wholesale (delete +
// reinsert) rather than diffing — author lists are small, so this is the
// simplest correct approach.
//
// date_modified is stamped here rather than read off `entry`, so callers can't
// forget to bump it. cite_key is the lookup key and is deliberately not
// updatable — renaming one needs its own operation.
pub fn update_entry(conn: &Connection, entry: &Entry) -> Result<()> {
    let tx = conn.unchecked_transaction()?;

    let entry_id: i64 = tx.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [&entry.cite_key],
        |row| row.get(0),
    )?;

    tx.execute(
        r#"
        UPDATE entries SET
            entry_type = ?1, title = ?2, year = ?3, journal = ?4, volume = ?5,
            pages = ?6, doi = ?7, url = ?8, abstract = ?9, date_modified = ?10
        WHERE id = ?11
        "#,
        rusqlite::params![
            entry.entry_type,
            entry.title,
            entry.year,
            entry.journal,
            entry.volume,
            entry.pages,
            entry.doi,
            entry.url,
            entry.abstract_text,
            now(),
            entry_id,
        ],
    )?;

    tx.execute("DELETE FROM authors WHERE entry_id = ?1", [entry_id])?;
    insert_authors(&tx, entry_id, &entry.authors)?;

    tx.commit()
}

// Relies on `PRAGMA foreign_keys = ON` (set in create_schema) for the
// ON DELETE CASCADE on authors.entry_id to actually fire.
pub fn delete_entry(conn: &Connection, cite_key: &str) -> Result<()> {
    conn.execute("DELETE FROM entries WHERE cite_key = ?1", [cite_key])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let mut entry = Entry::new(
            "article".to_string(),
            "smith2024".to_string(),
            "A Very Important Paper".to_string(),
        );
        entry.add_author(Author::new("Smith".to_string(), Some("John".to_string())));
        entry.add_author(Author::new("Doe".to_string(), Some("Jane".to_string())));
        entry.year = Some(2024);
        entry.journal = Some("Nature".to_string());

        let id = insert_entry(&conn, &entry).unwrap();
        assert!(id > 0);

        let fetched = get_entry(&conn, "smith2024").unwrap().expect("entry should exist");

        assert_eq!(fetched.id, Some(id));
        assert_eq!(fetched.entry_type, entry.entry_type);
        assert_eq!(fetched.cite_key, entry.cite_key);
        assert_eq!(fetched.title, entry.title);
        assert_eq!(fetched.year, entry.year);
        assert_eq!(fetched.journal, entry.journal);
        assert_eq!(fetched.date_added, entry.date_added);

        assert_eq!(fetched.authors.len(), 2);
        assert_eq!(fetched.authors[0].last_name, "Smith");
        assert_eq!(fetched.authors[0].first_name, Some("John".to_string()));
        assert_eq!(fetched.authors[1].last_name, "Doe");
        assert_eq!(fetched.authors[1].first_name, Some("Jane".to_string()));
    }

    // Covers the three functions main.rs doesn't call, plus the two behaviors
    // the code comments assert but nothing verified: that delete cascades to
    // authors, and that update stamps date_modified itself.
    #[test]
    fn update_and_delete() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let mut entry = Entry::new("article".into(), "smith2024".into(), "Original".into());
        entry.add_author(Author::new("Smith".into(), Some("John".into())));
        entry.abstract_text = Some("An abstract.".into());
        insert_entry(&conn, &entry).unwrap();

        // Backdate so a re-stamped date_modified is detectable without sleeping.
        let stale = entry.date_modified - 1000;
        conn.execute("UPDATE entries SET date_modified = ?1", [stale]).unwrap();

        let mut edited = get_entry(&conn, "smith2024").unwrap().unwrap();
        assert_eq!(edited.abstract_text.as_deref(), Some("An abstract."));
        assert_eq!(edited.date_modified, stale);

        edited.title = "Revised".into();
        edited.authors = vec![Author::new("Doe".into(), None)];
        update_entry(&conn, &edited).unwrap();

        let after = get_entry(&conn, "smith2024").unwrap().unwrap();
        assert_eq!(after.title, "Revised");
        assert_eq!(after.authors.len(), 1);
        assert_eq!(after.authors[0].last_name, "Doe");
        assert_eq!(after.authors[0].first_name, None);
        assert!(after.date_modified > stale, "update_entry must stamp date_modified");

        assert_eq!(list_entries(&conn).unwrap().len(), 1);

        delete_entry(&conn, "smith2024").unwrap();
        assert!(get_entry(&conn, "smith2024").unwrap().is_none());

        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM authors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "delete_entry must cascade to authors");
    }
}
