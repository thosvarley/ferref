// This imports the rusqlite library's Connection type
// Connection represents a connection to a SQLite database file
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, Result, Row};

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

CREATE TABLE IF NOT EXISTS attachments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    date_added INTEGER NOT NULL,
    UNIQUE (entry_id, path)
);

CREATE INDEX IF NOT EXISTS idx_authors_entry ON authors(entry_id);
CREATE INDEX IF NOT EXISTS idx_entries_cite_key ON entries(cite_key);
CREATE INDEX IF NOT EXISTS idx_entries_year ON entries(year);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS entry_tags (
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, tag_id)
);
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
        tags: Vec::new(),
        attachments: Vec::new(),
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
    entry.tags = tags_for_entry(conn, entry.id.unwrap())?;
    entry.attachments = attachments_for_entry(conn, entry.id.unwrap())?;
    Ok(Some(entry))
}

fn tags_for_entry(conn: &Connection, entry_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT t.name FROM tags t JOIN entry_tags et ON et.tag_id = t.id \
         WHERE et.entry_id = ?1 ORDER BY t.name",
    )?;
    stmt.query_map([entry_id], |row| row.get(0))?
        .collect::<Result<Vec<String>>>()
}

fn attachments_for_entry(conn: &Connection, entry_id: i64) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT path FROM attachments WHERE entry_id = ?1 ORDER BY id")?;
    let paths = stmt
        .query_map([entry_id], |row| row.get(0))?
        .collect::<Result<Vec<String>>>()?;
    Ok(paths)
}

// Records a path, never a copy of the file — see DESIGN.md. Idempotent per the
// UNIQUE(entry_id, path) constraint; the bool reports whether a row was added.
// An unknown cite_key is an error (QueryReturnedNoRows), same as add_tag.
pub fn attach(conn: &Connection, cite_key: &str, path: &str) -> Result<bool> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;

    conn.execute(
        "INSERT OR IGNORE INTO attachments (entry_id, path, date_added) VALUES (?1, ?2, ?3)",
        rusqlite::params![entry_id, path, now()],
    )?;
    Ok(conn.changes() > 0)
}

// Single place tag names are normalized, so writes (add_tag/remove_tag) and
// reads (the `tag` filter in list_entries) can't disagree about what counts
// as the same tag. Rejects empty-after-trim rather than writing it, for the
// same reason parse_author rejects an empty last name: NOT NULL still accepts "".
// Returns Result<_, String> rather than a rusqlite error: this is input
// validation, not a DB failure, and it matches cli::parse_author.
pub fn normalize_tag(raw: &str) -> std::result::Result<String, String> {
    let name = raw.trim().to_lowercase();
    if name.is_empty() {
        return Err("tag name cannot be empty".to_string());
    }
    Ok(name)
}

// Idempotent: returns Ok(false) (not an error) if the entry already had the
// tag. An unknown cite_key is a real error (QueryReturnedNoRows), same as
// update_entry's id lookup.
pub fn add_tag(conn: &Connection, cite_key: &str, tag: &str) -> Result<bool> {
    let name = normalize_tag(tag).map_err(rusqlite::Error::InvalidParameterName)?;
    let tx = conn.unchecked_transaction()?;

    let entry_id: i64 = tx.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;

    // last_insert_rowid() is unsafe to rely on after INSERT OR IGNORE -- it's
    // stale when the insert was ignored -- so the id is looked up explicitly.
    tx.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [&name])?;
    let tag_id: i64 =
        tx.query_row("SELECT id FROM tags WHERE name = ?1", [&name], |row| row.get(0))?;

    tx.execute(
        "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
        [entry_id, tag_id],
    )?;
    let changed = tx.changes() > 0;

    tx.commit()?;
    Ok(changed)
}

// Idempotent: returns Ok(false) if the entry wasn't tagged with it.
//
// ponytail: a tag row orphaned by its last untag is left in `tags` rather
// than garbage-collected; harmless because nothing lists all tags yet.
pub fn remove_tag(conn: &Connection, cite_key: &str, tag: &str) -> Result<bool> {
    let name = normalize_tag(tag).map_err(rusqlite::Error::InvalidParameterName)?;
    let tx = conn.unchecked_transaction()?;

    let entry_id: i64 = tx.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;

    tx.execute(
        "DELETE FROM entry_tags WHERE entry_id = ?1 \
         AND tag_id = (SELECT id FROM tags WHERE name = ?2)",
        rusqlite::params![entry_id, name],
    )?;
    let changed = tx.changes() > 0;

    tx.commit()?;
    Ok(changed)
}

// An all-None Filter matches everything, so `list` and `search` are the same
// query with different arguments rather than two near-identical functions.
#[derive(Default)]
pub struct Filter {
    pub author: Option<String>,
    pub title: Option<String>,
    pub year_min: Option<i32>,
    pub year_max: Option<i32>,
    pub tag: Option<String>,
}

// Wraps a user substring for LIKE, escaping the wildcards so searching for
// "100%" or "a_b" means those literal characters. Paired with ESCAPE '\' in
// the SQL below.
fn like_pattern(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

pub fn list_entries(conn: &Connection, filter: &Filter) -> Result<Vec<Entry>> {
    let mut clauses: Vec<&str> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(title) = &filter.title {
        clauses.push("title LIKE ? ESCAPE '\\'");
        params.push(Value::Text(like_pattern(title)));
    }

    if let Some(author) = &filter.author {
        // EXISTS rather than a JOIN: a match on any one author shouldn't
        // duplicate the entry row.
        clauses.push(
            "EXISTS (SELECT 1 FROM authors a WHERE a.entry_id = entries.id \
             AND (a.last_name LIKE ? ESCAPE '\\' OR a.first_name LIKE ? ESCAPE '\\'))",
        );
        let pattern = like_pattern(author);
        params.push(Value::Text(pattern.clone()));
        params.push(Value::Text(pattern));
    }

    if let Some(min) = filter.year_min {
        clauses.push("year >= ?");
        params.push(Value::Integer(min.into()));
    }
    if let Some(max) = filter.year_max {
        clauses.push("year <= ?");
        params.push(Value::Integer(max.into()));
    }

    if let Some(tag) = &filter.tag {
        // EXISTS, not a JOIN, for the same reason the author clause is: a
        // join can duplicate the entry row. Exact match, not LIKE -- tags
        // are identifiers, not prose. Normalized the same way add_tag/
        // remove_tag write it, so "ML" finds what was stored as "ml"; an
        // empty-after-trim filter just matches nothing (no tag row is ever
        // empty), so the fallible normalize_tag error is not surfaced here.
        clauses.push(
            "EXISTS (SELECT 1 FROM entry_tags et JOIN tags t ON t.id = et.tag_id \
             WHERE et.entry_id = entries.id AND t.name = ?)",
        );
        params.push(Value::Text(normalize_tag(tag).unwrap_or_default()));
    }

    let sql = if clauses.is_empty() {
        "SELECT * FROM entries ORDER BY id".to_string()
    } else {
        format!(
            "SELECT * FROM entries WHERE {} ORDER BY id",
            clauses.join(" AND ")
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let mut entries = stmt
        .query_map(params_from_iter(params), entry_from_row)?
        .collect::<Result<Vec<Entry>>>()?;

    for entry in &mut entries {
        entry.authors = authors_for_entry(conn, entry.id.unwrap())?;
        entry.tags = tags_for_entry(conn, entry.id.unwrap())?;
        entry.attachments = attachments_for_entry(conn, entry.id.unwrap())?;
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

        assert_eq!(list_entries(&conn, &Filter::default()).unwrap().len(), 1);

        delete_entry(&conn, "smith2024").unwrap();
        assert!(get_entry(&conn, "smith2024").unwrap().is_none());

        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM authors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "delete_entry must cascade to authors");
    }

    fn seed(conn: &Connection, key: &str, title: &str, year: i32, last: &str, first: &str) {
        let mut entry = Entry::new("article".into(), key.into(), title.into());
        entry.add_author(Author::new(last.into(), Some(first.into())));
        entry.year = Some(year);
        insert_entry(conn, &entry).unwrap();
    }

    fn keys(conn: &Connection, filter: &Filter) -> Vec<String> {
        list_entries(conn, filter)
            .unwrap()
            .into_iter()
            .map(|e| e.cite_key)
            .collect()
    }

    #[test]
    fn filters_by_author_title_and_year_range() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        seed(&conn, "a", "Neural Networks", 2020, "Smith", "John");
        seed(&conn, "b", "Graph Theory", 2024, "Smithson", "Jane");
        seed(&conn, "c", "Neural Coding", 2022, "Jones", "Ada");

        // Empty filter matches everything, so `list` keeps working.
        assert_eq!(keys(&conn, &Filter::default()).len(), 3);

        // Substring, case-insensitive, and matches Smithson too.
        let by_author = Filter {
            author: Some("smith".into()),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &by_author), ["a", "b"]);

        // First names are searched as well as last.
        let by_first = Filter {
            author: Some("ada".into()),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &by_first), ["c"]);

        let by_title = Filter {
            title: Some("neural".into()),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &by_title), ["a", "c"]);

        let by_range = Filter {
            year_min: Some(2021),
            year_max: Some(2023),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &by_range), ["c"]);

        // Filters combine with AND.
        let combined = Filter {
            title: Some("neural".into()),
            year_min: Some(2021),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &combined), ["c"]);
    }

    // Without ESCAPE, a literal % or _ in the query would act as a wildcard
    // and match everything.
    #[test]
    fn like_wildcards_in_a_query_are_literal() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        seed(&conn, "pct", "Growth of 100% Yield", 2020, "Smith", "John");
        seed(&conn, "other", "Unrelated Work", 2020, "Jones", "Ada");

        let literal = Filter {
            title: Some("100%".into()),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &literal), ["pct"]);

        // A bare wildcard finds nothing, because no title contains "%_".
        let wildcard = Filter {
            title: Some("%_".into()),
            ..Default::default()
        };
        assert!(keys(&conn, &wildcard).is_empty());
    }

    #[test]
    fn tagging_add_remove_normalize_filter_and_cascade() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        seed(&conn, "a", "Neural Networks", 2020, "Smith", "John");
        seed(&conn, "b", "Graph Theory", 2024, "Smithson", "Jane");

        // tag -> get_entry sees it; tagging again is a no-op, no duplicate.
        assert_eq!(add_tag(&conn, "a", "ml").unwrap(), true);
        assert_eq!(add_tag(&conn, "a", "ml").unwrap(), false);
        assert_eq!(get_entry(&conn, "a").unwrap().unwrap().tags, vec!["ml".to_string()]);

        // untag: true, then false the second time.
        assert_eq!(remove_tag(&conn, "a", "ml").unwrap(), true);
        assert_eq!(remove_tag(&conn, "a", "ml").unwrap(), false);
        assert!(get_entry(&conn, "a").unwrap().unwrap().tags.is_empty());

        // Unknown cite_key is a real error, not a silent no-op.
        assert!(add_tag(&conn, "nonexistent", "ml").is_err());

        // Empty-after-trim tag name is rejected before it reaches the DB.
        assert!(normalize_tag("   ").is_err());

        // Normalization: "  ML  " and "ml" collapse to one tag row.
        add_tag(&conn, "a", "  ML  ").unwrap();
        add_tag(&conn, "b", "ml").unwrap();
        let tag_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(tag_count, 1);

        // Filtering by "ML" finds both, matched against the normalized name.
        let by_tag = Filter {
            tag: Some("ML".into()),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &by_tag), ["a", "b"]);

    // delete_entry cascades to entry_tags.
        delete_entry(&conn, "a").unwrap();
        delete_entry(&conn, "b").unwrap();
        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM entry_tags", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "delete_entry must cascade to entry_tags");
    }

    // Paths are stored, never copied (DESIGN.md), so these need not exist.
    #[test]
    fn attachments_are_idempotent_and_cascade() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2024, "Smith", "John");

        assert!(attach(&conn, "a", "/tmp/paper.pdf").unwrap());
        // UNIQUE(entry_id, path) makes a repeat attach a no-op, not a duplicate.
        assert!(!attach(&conn, "a", "/tmp/paper.pdf").unwrap());
        assert!(attach(&conn, "a", "/tmp/supplement.pdf").unwrap());

        assert_eq!(
            get_entry(&conn, "a").unwrap().unwrap().attachments,
            ["/tmp/paper.pdf", "/tmp/supplement.pdf"]
        );

        assert!(attach(&conn, "nonexistent", "/tmp/paper.pdf").is_err());

        delete_entry(&conn, "a").unwrap();
        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "delete_entry must cascade to attachments");
    }
}
