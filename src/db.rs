// This imports the rusqlite library's Connection type
// Connection represents a connection to a SQLite database file
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OptionalExtension, Result, Row};

// This imports the Path type from Rust's standard library
// Path is used for working with file system paths
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::models::{now, Attachment, Author, Entry};

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
    full_text TEXT,
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

-- No UNIQUE(parent_id, name): SQLite treats NULLs as distinct, so that
-- constraint would let two root-level (parent_id IS NULL) collections share
-- a name. Sibling-uniqueness is instead enforced in code (create_collection),
-- via a case-insensitive lookup before insert.
CREATE TABLE IF NOT EXISTS collections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    parent_id INTEGER REFERENCES collections(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS collection_entries (
    collection_id INTEGER NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    PRIMARY KEY (collection_id, entry_id)
);
CREATE INDEX IF NOT EXISTS idx_collections_parent ON collections(parent_id);

-- External-content FTS5 index over attachments.full_text, trigram-tokenized
-- so substring (not just whole-word) queries can use the index instead of a
-- full scan. 'content='attachments'' means the text itself isn't duplicated
-- here -- only the trigram index structures live in this table. Kept in sync
-- by the triggers below, the standard FTS5 external-content pattern.
CREATE VIRTUAL TABLE IF NOT EXISTS attachments_fts USING fts5(
    full_text, content='attachments', content_rowid='id', tokenize='trigram'
);

-- These three triggers must fire unconditionally, NULL full_text included --
-- no `WHEN new.full_text IS NOT NULL` guard. FTS5 treats NULL as empty
-- content, which is fine. What is NOT fine is the AFTER UPDATE trigger's
-- 'delete' command not matching exactly what AFTER INSERT actually indexed;
-- guarding one but not the other breaks that invariant silently.
CREATE TRIGGER IF NOT EXISTS attachments_fts_ai AFTER INSERT ON attachments BEGIN
  INSERT INTO attachments_fts(rowid, full_text) VALUES (new.id, new.full_text);
END;

CREATE TRIGGER IF NOT EXISTS attachments_fts_ad AFTER DELETE ON attachments BEGIN
  INSERT INTO attachments_fts(attachments_fts, rowid, full_text) VALUES('delete', old.id, old.full_text);
END;

CREATE TRIGGER IF NOT EXISTS attachments_fts_au AFTER UPDATE ON attachments BEGIN
  INSERT INTO attachments_fts(attachments_fts, rowid, full_text) VALUES('delete', old.id, old.full_text);
  INSERT INTO attachments_fts(rowid, full_text) VALUES (new.id, new.full_text);
END;
"#;

// CREATE TABLE IF NOT EXISTS does not add a column to a table that already
// exists. Phase 6 shipped `attachments` without `full_text`, so any DB
// created before this phase needs the column added explicitly -- this is
// the project's first migration.
fn create_schema(conn: &Connection) -> Result<()> {
    // attachments_fts is new in this phase; a library that already extracted
    // text has rows in attachments.full_text that predate the sync triggers
    // and need a one-time backfill, done only the first time this table is
    // created (not on every process start -- re-running the backfill on an
    // already-populated table would try to re-insert existing rowids).
    let fts_existed = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'attachments_fts'")?
        .exists([])?;

    conn.execute_batch(SCHEMA_SQL)?;

    let has_full_text = conn
        .prepare("SELECT 1 FROM pragma_table_info('attachments') WHERE name = 'full_text'")?
        .exists([])?;
    if !has_full_text {
        conn.execute("ALTER TABLE attachments ADD COLUMN full_text TEXT", [])?;
    }

    if !fts_existed {
        // NOT a manual `INSERT ... SELECT ... WHERE full_text IS NOT NULL`.
        // The AFTER INSERT trigger indexes every row unconditionally, NULL
        // full_text included (see the trigger's own comment on why that
        // invariant matters) -- a backfill that skips NULL rows leaves them
        // absent from the index while every later trigger assumes they're
        // present. On a pre-existing library (any attachment predating this
        // migration, e.g. one never `--extract`ed), the first UPDATE or
        // DELETE trigger firing on one of those rows issued a 'delete'
        // command for a rowid the index never actually held -- reproduced
        // against the real bundled SQLite (3.46.0) as an outright
        // "database disk image is malformed" error, not a graceful no-op.
        // FTS5's own 'rebuild' command is the documented, correct way to
        // populate/repair an external-content table: it re-scans every row
        // of the content table itself, NULLs included, so it can't disagree
        // with what the triggers already assume.
        conn.execute("INSERT INTO attachments_fts(attachments_fts) VALUES('rebuild')", [])?;
    }

    Ok(())
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
// Returns the cite_key of the entry already carrying `doi`, if there is one.
//
// A DOI names one specific paper, so two entries holding the same DOI are the
// same paper stored twice. That is easy to do by accident: `add --doi` derives
// a cite_key, and the collision handling politely finds a *free* name for what
// is really a re-add, so you end up with zhou2020b and zhou2020c pointing at
// one article.
//
// An absent or empty DOI is exempt -- plenty of entries legitimately have none,
// and those are not duplicates of each other. Matching is case-insensitive
// because DOIs are (the ISO standard says so; Crossref lowercases, humans
// don't always).
//
// `exclude_id` is the row being written, so an update doesn't collide with
// itself. `id IS NOT ?2` is null-safe: passing NULL matches every row, since
// no row has a NULL id.
fn doi_holder(conn: &Connection, doi: &str, exclude_id: Option<i64>) -> Result<Option<String>> {
    let doi = doi.trim();
    if doi.is_empty() {
        return Ok(None);
    }
    conn.query_row(
        "SELECT cite_key FROM entries WHERE lower(doi) = lower(?1) AND id IS NOT ?2",
        rusqlite::params![doi, exclude_id],
        |row| row.get(0),
    )
    .optional()
}

// The guard every write goes through. Deliberately here rather than as a UNIQUE
// index: this database is hand-editable by design, and an index would refuse to
// build on an existing library that already holds duplicates -- turning one
// stale row into a library where every command fails at open.
//
// Ceiling: the check and the insert are in the same deferred transaction, so
// two ferref processes adding one DOI simultaneously can still both pass. That
// costs a duplicate row, not data, and BEGIN IMMEDIATE would close it if it
// ever matters.
fn reject_duplicate_doi(conn: &Connection, entry: &Entry, exclude_id: Option<i64>) -> Result<()> {
    let Some(doi) = entry.doi.as_deref() else {
        return Ok(());
    };
    match doi_holder(conn, doi, exclude_id)? {
        Some(holder) => Err(rusqlite::Error::InvalidParameterName(format!(
            "DOI {} is already on entry '{holder}'",
            doi.trim()
        ))),
        None => Ok(()),
    }
}

pub fn insert_entry(conn: &Connection, entry: &Entry) -> Result<i64> {
    let tx = conn.unchecked_transaction()?;
    reject_duplicate_doi(&tx, entry, None)?;

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

    // Tags travel with the entry, so a BibTeX `keywords` field survives an
    // import. A tag the normalizer rejects (empty, whitespace) is skipped
    // rather than failing the whole insert -- one junk keyword shouldn't cost
    // you the paper.
    for tag in &entry.tags {
        if let Ok(name) = normalize_tag(tag) {
            attach_tag(&tx, entry_id, &name)?;
        }
    }

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
    entry.attachments = attachments_for_entry(conn, entry.id.unwrap(), true)?;
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

// with_full_text controls whether extracted text is pulled along with the
// path. list/search project it off by default -- otherwise printing a table
// of entries would pull every byte of every extracted PDF in the library
// into memory just to render four columns.
pub fn attachments_for_entry(
    conn: &Connection,
    entry_id: i64,
    with_full_text: bool,
) -> Result<Vec<Attachment>> {
    if with_full_text {
        let mut stmt = conn
            .prepare("SELECT path, full_text FROM attachments WHERE entry_id = ?1 ORDER BY id")?;
        stmt.query_map([entry_id], |row| {
            Ok(Attachment {
                path: row.get(0)?,
                full_text: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<Attachment>>>()
    } else {
        let mut stmt =
            conn.prepare("SELECT path FROM attachments WHERE entry_id = ?1 ORDER BY id")?;
        stmt.query_map([entry_id], |row| {
            Ok(Attachment {
                path: row.get(0)?,
                full_text: None,
            })
        })?
        .collect::<Result<Vec<Attachment>>>()
    }
}

// Records a path. This function never copies anything -- `main::copy_into_library`
// is what puts the file in `./pdfs/` first and hands the copy's path down here,
// so an attachment row points into the library directory, not at wherever the
// file was downloaded. Idempotent per the UNIQUE(entry_id, path) constraint; the
// bool reports whether a row was added.
// An unknown cite_key is an error (QueryReturnedNoRows), same as add_tag.
pub fn attach(conn: &Connection, cite_key: &str, path: &str) -> Result<(i64, bool)> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;

    conn.execute(
        "INSERT OR IGNORE INTO attachments (entry_id, path, date_added) VALUES (?1, ?2, ?3)",
        rusqlite::params![entry_id, path, now()],
    )?;
    let changed = conn.changes() > 0;

    // Looked up rather than taken from last_insert_rowid(), which is stale
    // when the INSERT was ignored. The id is what set_full_text needs.
    let attachment_id: i64 = conn.query_row(
        "SELECT id FROM attachments WHERE entry_id = ?1 AND path = ?2",
        rusqlite::params![entry_id, path],
        |row| row.get(0),
    )?;
    Ok((attachment_id, changed))
}

// Overwrites any previous extraction for this path -- re-extraction is meant
// to replace, not accumulate.
// Keyed by attachment id, not by path: the same file can be attached to two
// entries, and matching on path would let `extract <one_key>` silently write
// into a row belonging to an entry the command was never pointed at.
// Ok(0) means the row is gone, which the caller must not report as success.
pub fn set_full_text(conn: &Connection, attachment_id: i64, text: &str) -> Result<usize> {
    conn.execute(
        "UPDATE attachments SET full_text = ?1 WHERE id = ?2",
        rusqlite::params![text, attachment_id],
    )
}

// The attachment paths for one entry, unknown cite_key is an error
// (QueryReturnedNoRows), same pattern as attach/add_tag.
// (id, path) pairs -- the id is what set_full_text keys on.
pub fn attachments_for_cite_key(conn: &Connection, cite_key: &str) -> Result<Vec<(i64, String)>> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;
    let mut stmt =
        conn.prepare("SELECT id, path FROM attachments WHERE entry_id = ?1 ORDER BY id")?;
    stmt.query_map([entry_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<(i64, String)>>>()
}

// Character length of each attachment's extracted text for one entry,
// without loading the text itself. SQLite's length() on a TEXT column counts
// characters, so this is what the TUI's details pane uses to show "text:
// 41013 chars" without ever pulling full_text into memory (see
// attachments_for_entry's with_full_text comment -- this is the same
// concern one level more targeted).
// full_text is never loaded here, only its character length, via SQLite's
// length() -- every entry's in one query rather than one query per entry.
// The TUI's load_entries used to call a per-entry version of this in a
// loop, measured the same N+1 shape list_entries's own bulk queries exist
// to avoid (see list_entries's comment on the 9.4x measurement). Ordered
// the same way attachments_for_entry orders Entry.attachments (by id), so
// callers can zip the two positionally without storing the path twice.
pub fn all_attachment_text_lengths(conn: &Connection) -> Result<HashMap<i64, Vec<Option<i64>>>> {
    let mut stmt = conn
        .prepare("SELECT entry_id, length(full_text) FROM attachments ORDER BY entry_id, id")?;
    let mut out: HashMap<i64, Vec<Option<i64>>> = HashMap::new();
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
    })?;
    for row in rows {
        let (entry_id, len) = row?;
        out.entry(entry_id).or_default().push(len);
    }
    Ok(out)
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
// Tagging by entry id, shared by `add_tag` (which resolves a cite_key first)
// and `insert_entry` (which already holds the id it just wrote). Takes an
// already-normalized name.
fn attach_tag(conn: &Connection, entry_id: i64, name: &str) -> Result<bool> {
    // last_insert_rowid() is unsafe to rely on after INSERT OR IGNORE -- it's
    // stale when the insert was ignored -- so the id is looked up explicitly.
    conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [name])?;
    let tag_id: i64 =
        conn.query_row("SELECT id FROM tags WHERE name = ?1", [name], |row| row.get(0))?;

    conn.execute(
        "INSERT OR IGNORE INTO entry_tags (entry_id, tag_id) VALUES (?1, ?2)",
        [entry_id, tag_id],
    )?;
    Ok(conn.changes() > 0)
}

pub fn add_tag(conn: &Connection, cite_key: &str, tag: &str) -> Result<bool> {
    let name = normalize_tag(tag).map_err(rusqlite::Error::InvalidParameterName)?;
    let tx = conn.unchecked_transaction()?;

    let entry_id: i64 = tx.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;

    let changed = attach_tag(&tx, entry_id, &name)?;

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

// A collection with its direct (non-recursive) entry count. Not `Serialize`
// -- main.rs's `ls --json` output is flat with `path`/`depth` fields that
// don't exist on this struct, so it builds the JSON by hand from
// `collection_tree` instead of deriving off this type.
#[derive(Debug, Clone)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub parent_id: Option<i64>,
    pub entry_count: i64,
}

// Validates one path segment: trimmed, non-empty, and free of '/' (the path
// separator). Unlike normalize_tag, NOT lowercased -- a collection name is a
// label a human reads ("Machine Learning" should display as typed).
// Sibling-uniqueness is nonetheless checked case-insensitively; see
// create_collection.
fn validate_collection_name(raw: &str) -> std::result::Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("collection name cannot be empty".to_string());
    }
    if name.contains('/') {
        return Err(format!("collection name {name:?} cannot contain '/'"));
    }
    Ok(name.to_string())
}

// Splits a slash-separated path into validated segment names. A leading,
// trailing, or doubled '/' produces an empty segment, which
// validate_collection_name rejects the same as any other empty name.
fn split_path(path: &str) -> std::result::Result<Vec<String>, String> {
    path.split('/').map(validate_collection_name).collect()
}

// Resolves a slash-separated path to a collection id, walking one segment at
// a time. An invalid path (e.g. an empty segment) simply can't match
// anything real -- creating one is rejected by create_collection -- so this
// treats it as "not found" rather than a separate error.
pub fn collection_by_path(conn: &Connection, path: &str) -> Result<Option<i64>> {
    let Ok(segments) = split_path(path) else {
        return Ok(None);
    };

    let mut current: Option<i64> = None;
    for seg in segments {
        // `IS` rather than `=` so this is null-safe: with current = NULL,
        // "parent_id IS ?" correctly matches root-level collections.
        let found: Option<i64> = conn
            .query_row(
                "SELECT id FROM collections WHERE lower(name) = lower(?1) AND parent_id IS ?2",
                rusqlite::params![seg, current],
                |row| row.get(0),
            )
            .optional()?;
        match found {
            Some(id) => current = Some(id),
            None => return Ok(None),
        }
    }
    Ok(current)
}

// One segment of create_collection's mkdir -p loop, and the core the TUI
// calls directly with a parent id (never a rebuilt path -- see the module
// comment on why a slash-named collection has none). Looks the name up
// case-insensitively among siblings of `parent` before inserting, which is
// what makes sibling names unique without relying on a DB constraint (see
// the schema comment on `collections`).
pub fn create_collection_under(conn: &Connection, parent: Option<i64>, name: &str) -> Result<i64> {
    let seg = validate_collection_name(name).map_err(rusqlite::Error::InvalidParameterName)?;

    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM collections WHERE lower(name) = lower(?1) AND parent_id IS ?2",
            rusqlite::params![seg, parent],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => Ok(id),
        None => {
            conn.execute(
                "INSERT INTO collections (name, parent_id) VALUES (?1, ?2)",
                rusqlite::params![seg, parent],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

// Creates intermediate collections as needed (mkdir -p semantics) and
// returns the leaf id. Re-running with the same path is a no-op that returns
// the existing leaf rather than duplicating it -- each segment goes through
// create_collection_under, which handles the case-insensitive sibling lookup.
pub fn create_collection(conn: &Connection, path: &str) -> Result<i64> {
    let segments = split_path(path).map_err(rusqlite::Error::InvalidParameterName)?;

    let mut id = 0i64;
    let mut parent: Option<i64> = None;
    for seg in &segments {
        id = create_collection_under(conn, parent, seg)?;
        parent = Some(id);
    }
    Ok(id)
}

// Just the number, for callers that only want the number. The TUI's tree pane
// was loading every entry, author, tag and attachment in the library and then
// calling .len() on it to render "All Papers (n)" -- and did so again after
// every collection-picker toggle.
pub fn count_entries(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
}

// Entry count per collection *including its descendants*, in one recursive CTE.
//
// `all_collections` counts direct membership, which is what `collection ls`
// shows and what `list --collection` returns without `--recursive`. The TUI
// needs the other number: it always filters recursively, so a direct count
// beside a recursively-filtered table makes the pane contradict itself.
//
// COUNT(DISTINCT) because a paper filed in both a parent and its child is one
// paper, not two -- summing direct counts up the tree would double it. UNION
// rather than UNION ALL so a cyclic parent_id graph (this database is
// hand-editable) terminates instead of spinning.
pub fn recursive_entry_counts(conn: &Connection) -> Result<HashMap<i64, i64>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE subtree(root, id) AS ( \
             SELECT id, id FROM collections \
             UNION \
             SELECT s.root, c.id FROM collections c JOIN subtree s ON c.parent_id = s.id \
         ) \
         SELECT s.root, COUNT(DISTINCT ce.entry_id) \
         FROM subtree s LEFT JOIN collection_entries ce ON ce.collection_id = s.id \
         GROUP BY s.root",
    )?;
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<HashMap<i64, i64>>>()
}

// Every collection with its direct entry count, in one query (LEFT JOIN +
// GROUP BY, not N+1). Ordered by parent then name so a caller can build the
// tree deterministically without a second sort.
pub fn all_collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, c.parent_id, COUNT(ce.entry_id) AS entry_count \
         FROM collections c \
         LEFT JOIN collection_entries ce ON ce.collection_id = c.id \
         GROUP BY c.id ORDER BY c.parent_id, c.name",
    )?;
    stmt.query_map([], |row| {
        Ok(Collection {
            id: row.get(0)?,
            name: row.get(1)?,
            parent_id: row.get(2)?,
            entry_count: row.get(3)?,
        })
    })?
    .collect::<Result<Vec<Collection>>>()
}

// Idempotent: returns whether membership actually changed. The id-based
// core the TUI calls directly (it already holds the collection id from the
// tree, never a path).
pub fn add_entry_to_collection(conn: &Connection, collection_id: i64, entry_id: i64) -> Result<bool> {
    conn.execute(
        "INSERT OR IGNORE INTO collection_entries (collection_id, entry_id) VALUES (?1, ?2)",
        rusqlite::params![collection_id, entry_id],
    )?;
    Ok(conn.changes() > 0)
}

// Idempotent: Ok(false) if the entry wasn't in the collection.
pub fn remove_entry_from_collection(conn: &Connection, collection_id: i64, entry_id: i64) -> Result<bool> {
    conn.execute(
        "DELETE FROM collection_entries WHERE collection_id = ?1 AND entry_id = ?2",
        rusqlite::params![collection_id, entry_id],
    )?;
    Ok(conn.changes() > 0)
}

// Same shape as add_tag: returns whether membership actually changed.
// Unknown cite_key -> Err(QueryReturnedNoRows), same as add_tag. Unknown
// collection path -> Err(InvalidParameterName) with a message naming the
// path, so it isn't confused with the cite_key error upstream.
pub fn add_to_collection(conn: &Connection, path: &str, cite_key: &str) -> Result<bool> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;
    let collection_id = collection_by_path(conn, path)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("no collection found at path '{path}'"))
    })?;
    add_entry_to_collection(conn, collection_id, entry_id)
}

// Same error shape as add_to_collection.
pub fn remove_from_collection(conn: &Connection, path: &str, cite_key: &str) -> Result<bool> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;
    let collection_id = collection_by_path(conn, path)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("no collection found at path '{path}'"))
    })?;
    remove_entry_from_collection(conn, collection_id, entry_id)
}

// Ids of every collection an entry is directly filed into (not recursive --
// membership is a direct row, not inherited from a parent collection).
pub fn collections_for_entry(conn: &Connection, entry_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT collection_id FROM collection_entries WHERE entry_id = ?1")?;
    stmt.query_map([entry_id], |row| row.get(0))?
        .collect::<Result<Vec<i64>>>()
}

// Reparents a collection. Refuses to create a cycle: walks up from the
// proposed new parent, and rejects if the collection being moved appears in
// that chain (or is the new parent itself). That walk is itself over the
// graph it's validating, so it's bounded (32 hops) rather than trusting the
// graph is acyclic going in.
pub fn move_collection(conn: &Connection, path: &str, new_parent: Option<&str>) -> Result<()> {
    let id = collection_by_path(conn, path)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("no collection found at path '{path}'"))
    })?;

    let new_parent_id: Option<i64> = match new_parent {
        None => None,
        Some(p) => Some(collection_by_path(conn, p)?.ok_or_else(|| {
            rusqlite::Error::InvalidParameterName(format!("no collection found at path '{p}'"))
        })?),
    };

    if let Some(np) = new_parent_id {
        if np == id {
            return Err(rusqlite::Error::InvalidParameterName(
                "a collection cannot be its own parent".to_string(),
            ));
        }
        let mut cursor = Some(np);
        let mut hops = 0;
        while let Some(cur) = cursor {
            if cur == id {
                return Err(rusqlite::Error::InvalidParameterName(
                    "cannot move a collection under its own descendant".to_string(),
                ));
            }
            hops += 1;
            if hops > 32 {
                break;
            }
            cursor = conn.query_row(
                "SELECT parent_id FROM collections WHERE id = ?1",
                [cur],
                |row| row.get(0),
            )?;
        }
    }

    conn.execute(
        "UPDATE collections SET parent_id = ?1 WHERE id = ?2",
        rusqlite::params![new_parent_id, id],
    )?;
    Ok(())
}

// Ordered (depth, collection) pairs for rendering a tree. Terminates on a
// cyclic parent_id graph rather than recursing forever: built iteratively
// from the flat `all_collections` list with an explicit stack (DFS
// pre-order) and a visited set, so every node is walked at most once no
// matter how the parent_id graph is shaped. A node never reached from a root
// -- orphaned in a cycle, since the DB is a plain file anyone can edit by
// hand -- is appended at depth 0 rather than silently dropped, so a
// corrupted tree stays visible instead of losing rows. Depth is additionally
// capped at 32 as a backstop.
pub fn collection_tree(conn: &Connection) -> Result<Vec<(usize, Collection)>> {
    let all = all_collections(conn)?;
    let mut by_id: HashMap<i64, Collection> = all.into_iter().map(|c| (c.id, c)).collect();

    let mut children: HashMap<Option<i64>, Vec<i64>> = HashMap::new();
    for c in by_id.values() {
        children.entry(c.parent_id).or_default().push(c.id);
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|id| by_id.get(id).map(|c| c.name.to_lowercase()));
    }

    let mut roots = children.get(&None).cloned().unwrap_or_default();
    roots.sort_by_key(|id| by_id.get(id).map(|c| c.name.to_lowercase()));

    let mut stack: Vec<(i64, usize)> = roots.into_iter().rev().map(|id| (id, 0)).collect();
    let mut visited: HashSet<i64> = HashSet::new();
    let mut out: Vec<(usize, Collection)> = Vec::new();

    while let Some((id, depth)) = stack.pop() {
        if !visited.insert(id) {
            continue; // already rendered -- part of a cycle
        }
        if depth <= 32 {
            if let Some(kids) = children.get(&Some(id)) {
                for &k in kids.iter().rev() {
                    stack.push((k, depth + 1));
                }
            }
        }
        if let Some(c) = by_id.remove(&id) {
            out.push((depth, c));
        }
    }

    // Anything left wasn't reachable from a root: orphaned in a cycle.
    let mut orphans: Vec<Collection> = by_id.into_values().collect();
    orphans.sort_by_key(|c| c.id);
    out.extend(orphans.into_iter().map(|c| (0, c)));

    Ok(out)
}

// Slash-joined path for every row of a collection_tree() walk, same order.
// Same stack-truncation algorithm `collection ls --json` builds inline in
// main.rs -- factored out here so the TUI doesn't hand-roll a third copy.
pub fn collection_tree_paths(tree: &[(usize, Collection)]) -> Vec<String> {
    let mut stack: Vec<&str> = Vec::new();
    tree.iter()
        .map(|(depth, c)| {
            stack.truncate(*depth);
            stack.push(&c.name);
            stack.join("/")
        })
        .collect()
}

// Descendant ids of `root` (inclusive), derived from collection_tree's
// pre-order walk rather than a second traversal -- one place cycles are
// handled. Valid because collection_tree emits a node's whole subtree
// contiguously right after it, at strictly greater depth.
fn subtree_ids(conn: &Connection, root: i64) -> Result<Vec<i64>> {
    let tree = collection_tree(conn)?;
    let Some(root_pos) = tree.iter().position(|(_, c)| c.id == root) else {
        return Ok(Vec::new());
    };
    let root_depth = tree[root_pos].0;

    let mut ids = vec![root];
    for (depth, c) in &tree[root_pos + 1..] {
        if *depth <= root_depth {
            break;
        }
        ids.push(c.id);
    }
    Ok(ids)
}

// Deletes the subtree (ON DELETE CASCADE on parent_id takes it) and returns
// how many collections went. Entries are never touched -- only
// collection_entries membership rows, which cascade off collection_id.
pub fn delete_collection(conn: &Connection, path: &str) -> Result<usize> {
    let id = collection_by_path(conn, path)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("no collection found at path '{path}'"))
    })?;
    let count = subtree_ids(conn, id)?.len();
    conn.execute("DELETE FROM collections WHERE id = ?1", [id])?;
    Ok(count)
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
    pub collection_id: Option<i64>,
    pub recursive: bool,
    pub text: Option<String>,
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

// Quotes a literal substring as an FTS5 MATCH phrase: wrapped in double
// quotes, with any literal double quote in the text doubled to escape it --
// FTS5's phrase-quoting rule, not the LIKE-escaping `like_pattern` does.
// Against a trigram-tokenized column, phrase (adjacency) matching on the
// query's own trigrams is exactly substring matching: nothing about `%`,
// `_`, or `\` is special to MATCH, so none of like_pattern's escaping
// applies here.
fn fts5_phrase(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

// with_full_text is a projection, not a filter, so it lives as a parameter
// here rather than on Filter -- see attachments_for_entry's comment.
pub fn list_entries(conn: &Connection, filter: &Filter, with_full_text: bool) -> Result<Vec<Entry>> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if let Some(title) = &filter.title {
        clauses.push("title LIKE ? ESCAPE '\\'".to_string());
        params.push(Value::Text(like_pattern(title)));
    }

    if let Some(author) = &filter.author {
        // EXISTS rather than a JOIN: a match on any one author shouldn't
        // duplicate the entry row.
        clauses.push(
            "EXISTS (SELECT 1 FROM authors a WHERE a.entry_id = entries.id \
             AND (a.last_name LIKE ? ESCAPE '\\' OR a.first_name LIKE ? ESCAPE '\\'))"
                .to_string(),
        );
        let pattern = like_pattern(author);
        params.push(Value::Text(pattern.clone()));
        params.push(Value::Text(pattern));
    }

    if let Some(min) = filter.year_min {
        clauses.push("year >= ?".to_string());
        params.push(Value::Integer(min.into()));
    }
    if let Some(max) = filter.year_max {
        clauses.push("year <= ?".to_string());
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
             WHERE et.entry_id = entries.id AND t.name = ?)"
                .to_string(),
        );
        params.push(Value::Text(normalize_tag(tag).unwrap_or_default()));
    }

    if let Some(root_id) = filter.collection_id {
        // An id, not a path: callers resolve the path themselves. The TUI
        // already holds the id it clicked on, and round-tripping it through a
        // slash-joined path silently filtered the wrong (empty) set for any
        // collection whose name contains a literal "/" -- reachable by hand-
        // editing the DB, which this project explicitly invites.
        //
        // Resolved to concrete ids in Rust -- via collection_tree's
        // cycle-safe walk when `recursive` is set -- rather than a recursive
        // CTE, so there's one traversal implementation and one place cycles
        // are handled. EXISTS, not a JOIN, same reason as tag/author: an
        // entry in more than one matched collection must not duplicate the
        // entry row.
        let ids = if filter.recursive {
            subtree_ids(conn, root_id)?
        } else {
            vec![root_id]
        };
        let placeholders = vec!["?"; ids.len()].join(",");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM collection_entries ce WHERE ce.entry_id = entries.id \
             AND ce.collection_id IN ({placeholders}))"
        ));
        params.extend(ids.into_iter().map(Value::Integer));
    }

    if let Some(text) = &filter.text {
        // Two real traps here, both found by measuring the actual query
        // shape rather than trusting the tokenizer's documented behavior in
        // isolation (see DESIGN.md Phase 15's "false start" note):
        //
        // 1. A LIKE constraint only gets served from the trigram index when
        //    SQLite scans attachments_fts as the *driving* table of the
        //    query -- i.e. non-correlated. Written as a per-row correlated
        //    EXISTS (the shape every other clause here uses), the LIKE
        //    constraint isn't pushed at all: SQLite re-scans the whole FTS5
        //    table from scratch for every row of `entries`, measured 5x
        //    *slower* than not having the index. `entries.id IN (subquery)`
        //    with no correlation to the outer query lets SQLite evaluate the
        //    inner scan exactly once as a genuine indexed lookup.
        // 2. Also measured: adding `ESCAPE '\'` to that LIKE -- needed for
        //    correctness on every OTHER substring filter here -- silently
        //    disables the trigram index entirely (confirmed via EXPLAIN
        //    QUERY PLAN: the constraint stops being recognized at all). So
        //    this can't reuse like_pattern/LIKE the way every other clause
        //    does; it has to use MATCH with FTS5's own phrase-quoting
        //    (fts5_phrase), which needs no `%`/`_`/`\` escaping in the
        //    first place, and does get served from the index.
        //
        // MATCH has its own gap LIKE didn't: a phrase shorter than one
        // trigram (under 3 characters) matches nothing at all, even when
        // the substring is genuinely present -- confirmed directly, not
        // assumed. Below that length there's nothing to index against
        // regardless, so it falls back to a plain, correlated LIKE scan
        // against attachments.full_text directly (skipping attachments_fts
        // entirely -- it can't help here either way).
        if text.chars().count() >= 3 {
            clauses.push(
                "entries.id IN (SELECT att.entry_id FROM attachments att, attachments_fts fts \
                 WHERE fts.rowid = att.id AND fts.full_text MATCH ?)"
                    .to_string(),
            );
            params.push(Value::Text(fts5_phrase(text)));
        } else {
            clauses.push(
                "EXISTS (SELECT 1 FROM attachments att \
                 WHERE att.entry_id = entries.id AND att.full_text LIKE ? ESCAPE '\\')"
                    .to_string(),
            );
            params.push(Value::Text(like_pattern(text)));
        }
    }

    let where_clause = clauses.join(" AND ");
    let sql = if where_clause.is_empty() {
        "SELECT * FROM entries ORDER BY id".to_string()
    } else {
        format!("SELECT * FROM entries WHERE {where_clause} ORDER BY id")
    };

    let mut stmt = conn.prepare(&sql)?;
    let mut entries = stmt
        .query_map(params_from_iter(params.clone()), entry_from_row)?
        .collect::<Result<Vec<Entry>>>()?;

    // Three bulk queries for the children, not three per entry. The per-entry
    // form cost 15,000 queries (and 15,000 SQL compilations) on a 5,000-entry
    // library, and was measured 9.4x slower than this on the same data -- most
    // of `ferref list`'s runtime, and `list` is the hottest path in the tool.
    //
    // The scope is the entry query's own WHERE clause reapplied as a subquery
    // rather than an `IN (?,?,...)` list of the ids just fetched: the id list
    // runs into SQLite's bound-variable ceiling on a large result, and this way
    // the filter is written once. Each child query rebinds the same params.
    let index: HashMap<i64, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.id.map(|id| (id, i)))
        .collect();

    let scope = |column: &str| {
        if where_clause.is_empty() {
            String::new()
        } else {
            format!(" WHERE {column} IN (SELECT id FROM entries WHERE {where_clause})")
        }
    };

    // Ordering within each entry is the ORDER BY's second key, so authors keep
    // their author_order and attachments their id order, exactly as the
    // per-entry queries produced.
    let mut stmt = conn.prepare(&format!(
        "SELECT entry_id, first_name, last_name FROM authors{} \
         ORDER BY entry_id, author_order",
        scope("entry_id")
    ))?;
    let mut rows = stmt.query(params_from_iter(params.clone()))?;
    while let Some(row) = rows.next()? {
        if let Some(&i) = index.get(&row.get::<_, i64>(0)?) {
            entries[i].authors.push(Author {
                first_name: row.get(1)?,
                last_name: row.get(2)?,
            });
        }
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT et.entry_id, t.name FROM tags t \
         JOIN entry_tags et ON et.tag_id = t.id \
         {} ORDER BY et.entry_id, t.name",
        scope("et.entry_id")
    ))?;
    let mut rows = stmt.query(params_from_iter(params.clone()))?;
    while let Some(row) = rows.next()? {
        if let Some(&i) = index.get(&row.get::<_, i64>(0)?) {
            entries[i].tags.push(row.get(1)?);
        }
    }

    // full_text is selected only when asked for: it is the whole reason the
    // projection exists (see the comment on attachments_for_entry).
    let columns = if with_full_text {
        "entry_id, path, full_text"
    } else {
        "entry_id, path, NULL"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM attachments{} ORDER BY entry_id, id",
        columns,
        scope("entry_id")
    ))?;
    let mut rows = stmt.query(params_from_iter(params))?;
    while let Some(row) = rows.next()? {
        if let Some(&i) = index.get(&row.get::<_, i64>(0)?) {
            entries[i].attachments.push(Attachment {
                path: row.get(1)?,
                full_text: row.get(2)?,
            });
        }
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

    reject_duplicate_doi(&tx, entry, Some(entry_id))?;

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

// Folds drop_id's tags, collections, and attachments into keep_id, then
// deletes drop_id. keep_id's own scalar fields (title, doi, ...) are
// untouched -- merge only moves relationships; Edit is what field-by-field
// union would go through.
//
// entry_tags and collection_entries are both PRIMARY KEY (entry_id, X), so a
// drop_id row that keep_id already has (same tag, same collection) can't be
// re-parented without hitting that key. `UPDATE OR IGNORE` (confirmed
// against real SQLite -- see the sqlite3 CLI check in this phase's notes,
// and the #[test] below -- to skip just the conflicting row, not the whole
// statement) leaves it parked on drop_id, where `DELETE FROM entries`'s
// ON DELETE CASCADE sweeps it up for free.
//
// Attachments physically live at ./pdfs/<cite_key>.<ext> (Phase 12), so a
// drop-side file's name stops matching the naming convention once its row
// points at keep_id. Each one is renamed onto a name built from keep's
// cite_key, claimed with the same O_EXCL discipline main.rs's
// copy_into_library/land_downloaded_pdf use for the identical race: never
// overwrite a file that's already there; if the natural name is taken by an
// unrelated attachment keep already has, fall back to "-2", "-3", ...
pub fn merge_entries(conn: &Connection, keep_id: i64, drop_id: i64) -> Result<()> {
    let tx = conn.unchecked_transaction()?;

    let keep_cite_key: String = tx.query_row(
        "SELECT cite_key FROM entries WHERE id = ?1",
        [keep_id],
        |row| row.get(0),
    )?;

    tx.execute(
        "UPDATE OR IGNORE entry_tags SET entry_id = ?1 WHERE entry_id = ?2",
        rusqlite::params![keep_id, drop_id],
    )?;
    tx.execute(
        "UPDATE OR IGNORE collection_entries SET entry_id = ?1 WHERE entry_id = ?2",
        rusqlite::params![keep_id, drop_id],
    )?;

    let drop_attachments: Vec<(i64, String)> = {
        let mut stmt = tx.prepare("SELECT id, path FROM attachments WHERE entry_id = ?1")?;
        stmt.query_map([drop_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>>>()?
    };

    let base =
        crate::doi::sanitize_filename(&keep_cite_key).map_err(rusqlite::Error::InvalidParameterName)?;

    // Renames happen on the filesystem, outside SQLite's transaction, so
    // committing/rolling back the DB rows can't undo them. The DB is
    // hand-editable by design, so a row can point at a file that's already
    // gone -- checked up front, before anything is touched, so a doomed
    // merge fails loud with zero side effects rather than after moving some
    // attachments but not others.
    for (_, old_path_str) in &drop_attachments {
        if !Path::new(old_path_str).is_file() {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "attachment file '{old_path_str}' does not exist on disk; refusing to merge"
            )));
        }
    }

    // Every source file existing up front doesn't rule out a rename failing
    // partway through the loop (permissions, disk full, ...), so each
    // successful (old, new) pair is tracked and rewound -- moved back to
    // where it started -- if a later one fails, keeping the filesystem in
    // step with the DB rows the aborted transaction is about to roll back.
    let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();

    for (attachment_id, old_path_str) in drop_attachments {
        match move_attachment(&tx, keep_id, &base, attachment_id, &old_path_str) {
            Ok(pair) => moved.push(pair),
            Err(e) => {
                for (old, new) in moved.iter().rev() {
                    let _ = std::fs::rename(new, old);
                }
                return Err(e);
            }
        }
    }

    tx.execute("DELETE FROM entries WHERE id = ?1", [drop_id])?;

    tx.commit()
}

// Claims a destination name under keep's cite_key, renames the attachment's
// file onto it, and updates its DB row -- returns the (old, new) path pair
// so a caller merging several attachments can unwind earlier successes if a
// later one fails.
fn move_attachment(
    tx: &Connection,
    keep_id: i64,
    base: &str,
    attachment_id: i64,
    old_path_str: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let old_path = Path::new(old_path_str);
    let dir = old_path.parent().ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!(
            "attachment path '{old_path_str}' has no parent directory"
        ))
    })?;
    let ext = old_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("pdf");

    let new_path = claim_attachment_name(dir, base, ext)?;
    if let Err(e) = std::fs::rename(old_path, &new_path) {
        // The claim above created an empty placeholder at new_path -- don't
        // leave it squatting on the name if the rename that was meant to
        // fill it fails.
        let _ = std::fs::remove_file(&new_path);
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "failed to move attachment '{old_path_str}' to '{}': {e}",
            new_path.display()
        )));
    }
    let new_path_str = new_path.to_str().ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!(
            "path {} is not valid UTF-8",
            new_path.display()
        ))
    })?;
    if let Err(e) = tx.execute(
        "UPDATE attachments SET entry_id = ?1, path = ?2 WHERE id = ?3",
        rusqlite::params![keep_id, new_path_str, attachment_id],
    ) {
        let _ = std::fs::rename(&new_path, old_path);
        return Err(e);
    }

    Ok((old_path.to_path_buf(), new_path))
}

// Claims a free `<base>.<ext>` / `<base>-2.<ext>` / ... filename in `dir` via
// O_EXCL (create_new) -- the same race-proof discipline
// main.rs::copy_into_library/land_downloaded_pdf use: checking a name is
// free and then writing to it are two steps, and a second mover can land in
// between them. The empty file this claims is what merge_entries's
// std::fs::rename then overwrites.
fn claim_attachment_name(dir: &Path, base: &str, ext: &str) -> Result<std::path::PathBuf> {
    for n in 1..=50 {
        let candidate = if n == 1 {
            dir.join(format!("{base}.{ext}"))
        } else {
            dir.join(format!("{base}-{n}.{ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "failed to claim '{}': {e}",
                    candidate.display()
                )))
            }
        }
    }
    Err(rusqlite::Error::InvalidParameterName(format!(
        "could not find a free filename for '{base}' in {}",
        dir.display()
    )))
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
    // A DOI names one paper, so a second entry carrying it is that paper
    // twice. The cases that matter are the exemptions: entries with no DOI
    // are not duplicates of each other, and an update must not collide with
    // the row it is updating.
    #[test]
    fn duplicate_dois_are_refused_but_absent_ones_are_not() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let mut first = Entry::new("article".into(), "zhou2020".into(), "First".into());
        first.doi = Some("10.1186/s12859-020-3494-x".into());
        insert_entry(&conn, &first).unwrap();

        // Same DOI under a different key is the same paper again.
        let mut again = Entry::new("article".into(), "zhou2020b".into(), "Second".into());
        again.doi = Some("10.1186/s12859-020-3494-x".into());
        let err = insert_entry(&conn, &again).unwrap_err().to_string();
        assert!(err.contains("zhou2020"), "message should name the holder: {err}");

        // DOIs are case-insensitive, so a shouted one is still the same paper.
        again.doi = Some("10.1186/S12859-020-3494-X".into());
        assert!(insert_entry(&conn, &again).is_err());

        // Absent and empty DOIs are exempt -- two undoi'd entries are fine.
        let none_a = Entry::new("misc".into(), "a".into(), "No DOI".into());
        let mut none_b = Entry::new("misc".into(), "b".into(), "Also no DOI".into());
        none_b.doi = Some("   ".into());
        insert_entry(&conn, &none_a).unwrap();
        insert_entry(&conn, &none_b).unwrap();

        // Updating an entry without changing its DOI must not trip on itself.
        let mut same = get_entry(&conn, "zhou2020").unwrap().unwrap();
        same.title = "First, retitled".into();
        update_entry(&conn, &same).unwrap();

        // Updating one onto a DOI another entry holds is still refused.
        let mut steal = get_entry(&conn, "a").unwrap().unwrap();
        steal.doi = Some("10.1186/s12859-020-3494-x".into());
        assert!(update_entry(&conn, &steal).is_err());
    }

    // The tree pane counts recursively while `collection ls` counts directly.
    // The trap is double-counting: a paper filed in both a parent and its child
    // is one paper.
    #[test]
    fn recursive_counts_include_descendants_without_double_counting() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        for key in ["p1", "p2"] {
            insert_entry(&conn, &Entry::new("article".into(), key.into(), "T".into())).unwrap();
        }
        create_collection(&conn, "Phys/Sub").unwrap();
        let phys = collection_by_path(&conn, "Phys").unwrap().unwrap();
        let sub = collection_by_path(&conn, "Phys/Sub").unwrap().unwrap();

        add_to_collection(&conn, "Phys/Sub", "p1").unwrap();
        add_to_collection(&conn, "Phys", "p2").unwrap();

        let counts = recursive_entry_counts(&conn).unwrap();
        assert_eq!(counts[&phys], 2, "parent should include its child's papers");
        assert_eq!(counts[&sub], 1);

        // p1 now sits in both. It is still one paper.
        add_to_collection(&conn, "Phys", "p1").unwrap();
        let counts = recursive_entry_counts(&conn).unwrap();
        assert_eq!(counts[&phys], 2, "a paper in both parent and child counts once");

        assert_eq!(count_entries(&conn).unwrap(), 2);
    }

    // list_entries loads authors/tags/attachments in three bulk queries and
    // folds them back by entry_id. A filtered query is where that fold can
    // silently attach one entry's children to another.
    #[test]
    fn filtered_list_still_folds_children_onto_the_right_entries() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let mut first = Entry::new("article".into(), "zhou2020".into(), "Alpha".into());
        first.add_author(Author::new("Zhou".into(), Some("Yi".into())));
        first.add_author(Author::new("Smith".into(), None));
        first.year = Some(2020);
        first.tags = vec!["entropy".into()];
        insert_entry(&conn, &first).unwrap();

        let mut second = Entry::new("article".into(), "jones1990".into(), "Beta".into());
        second.add_author(Author::new("Jones".into(), None));
        second.year = Some(1990);
        insert_entry(&conn, &second).unwrap();

        attach(&conn, "zhou2020", "/tmp/zhou.pdf").unwrap();

        let filter = Filter {
            author: Some("zhou".into()),
            ..Default::default()
        };
        let found = list_entries(&conn, &filter, false).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].cite_key, "zhou2020");
        // Author order is author_order, not insertion order of the bulk query.
        assert_eq!(found[0].authors[0].last_name, "Zhou");
        assert_eq!(found[0].authors[1].last_name, "Smith");
        assert_eq!(found[0].tags, vec!["entropy"]);
        assert_eq!(found[0].attachments.len(), 1);

        // The unfiltered case must not leak zhou2020's children onto jones1990.
        let all = list_entries(&conn, &Filter::default(), false).unwrap();
        let jones = all.iter().find(|e| e.cite_key == "jones1990").unwrap();
        assert!(jones.tags.is_empty());
        assert!(jones.attachments.is_empty());
        assert_eq!(jones.authors.len(), 1);
    }

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

        assert_eq!(list_entries(&conn, &Filter::default(), false).unwrap().len(), 1);

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
        list_entries(conn, filter, false)
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

        let (first_id, changed) = attach(&conn, "a", "/tmp/paper.pdf").unwrap();
        assert!(changed);
        // UNIQUE(entry_id, path) makes a repeat attach a no-op, not a
        // duplicate, and still reports the existing row's id.
        let (again_id, changed) = attach(&conn, "a", "/tmp/paper.pdf").unwrap();
        assert!(!changed);
        assert_eq!(first_id, again_id);
        assert!(attach(&conn, "a", "/tmp/supplement.pdf").unwrap().1);

        let paths: Vec<String> = get_entry(&conn, "a")
            .unwrap()
            .unwrap()
            .attachments
            .into_iter()
            .map(|a| a.path)
            .collect();
        assert_eq!(paths, ["/tmp/paper.pdf", "/tmp/supplement.pdf"]);

        assert!(attach(&conn, "nonexistent", "/tmp/paper.pdf").is_err());

        delete_entry(&conn, "a").unwrap();
        let orphans: i64 = conn
            .query_row("SELECT COUNT(*) FROM attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "delete_entry must cascade to attachments");
    }

    // Proves the Phase 7 migration: an old 4-column `attachments` table (no
    // `full_text`, as Phase 6 shipped it) gets the column added, and existing
    // rows survive.
    #[test]
    fn migration_adds_full_text_column_and_preserves_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_type TEXT NOT NULL,
                cite_key TEXT UNIQUE NOT NULL,
                title TEXT NOT NULL,
                year INTEGER,
                date_added INTEGER NOT NULL,
                date_modified INTEGER NOT NULL
            );
            CREATE TABLE attachments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                date_added INTEGER NOT NULL,
                UNIQUE (entry_id, path)
            );
            INSERT INTO entries (entry_type, cite_key, title, year, date_added, date_modified)
                VALUES ('article', 'old2020', 'Old Entry', 2020, 0, 0);
            INSERT INTO attachments (entry_id, path, date_added) VALUES (1, '/tmp/old.pdf', 0);
            "#,
        )
        .unwrap();

        create_schema(&conn).unwrap();

        let has_full_text = conn
            .prepare("SELECT 1 FROM pragma_table_info('attachments') WHERE name = 'full_text'")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(has_full_text, "migration must add full_text column");

        let path: String = conn
            .query_row("SELECT path FROM attachments WHERE entry_id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(path, "/tmp/old.pdf", "migration must preserve existing rows");
    }

    // Regression: set_full_text once matched on `path`, so extracting for one
    // entry silently wrote into every other entry sharing that file.
    #[test]
    fn extracting_one_entry_does_not_touch_another_sharing_the_file() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "e1", "One", 2024, "Smith", "John");
        seed(&conn, "e2", "Two", 2024, "Jones", "Ada");

        let (id1, _) = attach(&conn, "e1", "/tmp/shared.pdf").unwrap();
        attach(&conn, "e2", "/tmp/shared.pdf").unwrap();

        assert_eq!(set_full_text(&conn, id1, "text of e1").unwrap(), 1);

        let e1 = get_entry(&conn, "e1").unwrap().unwrap();
        let e2 = get_entry(&conn, "e2").unwrap().unwrap();
        assert_eq!(e1.attachments[0].full_text.as_deref(), Some("text of e1"));
        assert_eq!(e2.attachments[0].full_text, None, "e2 must be untouched");

        // A vanished row reports 0 rather than silently succeeding.
        assert_eq!(set_full_text(&conn, 9999, "orphan").unwrap(), 0);
    }

    // mkdir -p semantics: creates every missing intermediate collection, and
    // running it again returns the same leaf rather than duplicating.
    #[test]
    fn create_collection_is_mkdir_p_and_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let leaf = create_collection(&conn, "Physics/Quantum/Entropy").unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM collections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "all three levels must be created");

        let leaf_again = create_collection(&conn, "Physics/Quantum/Entropy").unwrap();
        assert_eq!(leaf, leaf_again);
        let count_again: i64 = conn
            .query_row("SELECT COUNT(*) FROM collections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_again, 3, "re-creating must not duplicate rows");

        assert_eq!(collection_by_path(&conn, "Physics/Quantum/Entropy").unwrap(), Some(leaf));
        assert_eq!(collection_by_path(&conn, "Physics/Nope").unwrap(), None);
    }

    // Sibling names are unique case-insensitively, including at the root --
    // the SQLite-NULL trap a UNIQUE(parent_id, name) constraint wouldn't
    // catch, since NULL parent_id values are never equal to each other.
    #[test]
    fn sibling_names_are_unique_case_insensitively_even_at_root() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let first = create_collection(&conn, "Physics").unwrap();
        let second = create_collection(&conn, "physics").unwrap();
        assert_eq!(first, second, "case-insensitive root sibling must reuse the row");

        let root_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM collections WHERE parent_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(root_count, 1, "two root collections must not both exist");

        // Same check one level down.
        let child = create_collection(&conn, "Physics/Entropy").unwrap();
        let child_again = create_collection(&conn, "physics/ENTROPY").unwrap();
        assert_eq!(child, child_again);
    }

    // The id-based cores the TUI uses directly: child creation under a
    // parent id is idempotent and case-insensitive (same as the path-based
    // wrapper), a slash in the name is rejected, and add/remove/lookup
    // round-trip through collection_entries.
    #[test]
    fn id_based_collection_cores_round_trip() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let parent = create_collection_under(&conn, None, "Physics").unwrap();
        let child = create_collection_under(&conn, Some(parent), "Entropy").unwrap();
        let child_again = create_collection_under(&conn, Some(parent), "entropy").unwrap();
        assert_eq!(child, child_again, "case-insensitive re-creation must reuse the row");

        assert!(create_collection_under(&conn, Some(parent), "Has/Slash").is_err());

        let entry = Entry::new(
            "article".to_string(),
            "round2024".to_string(),
            "Round Trip".to_string(),
        );
        let entry_id = insert_entry(&conn, &entry).unwrap();

        assert!(add_entry_to_collection(&conn, child, entry_id).unwrap());
        assert!(!add_entry_to_collection(&conn, child, entry_id).unwrap(), "already a member");
        assert_eq!(collections_for_entry(&conn, entry_id).unwrap(), vec![child]);

        assert!(remove_entry_from_collection(&conn, child, entry_id).unwrap());
        assert!(!remove_entry_from_collection(&conn, child, entry_id).unwrap(), "already removed");
        assert!(collections_for_entry(&conn, entry_id).unwrap().is_empty());
    }

    #[test]
    fn collection_name_validation() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        assert!(create_collection(&conn, "").is_err());
        assert!(create_collection(&conn, "   ").is_err());
        assert!(create_collection(&conn, "Has/Slash").is_ok()); // two valid segments
        assert!(create_collection(&conn, "Physics//Entropy").is_err()); // empty segment
    }

    #[test]
    fn collection_membership_is_idempotent_and_errors_on_unknowns() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2024, "Smith", "John");
        create_collection(&conn, "Physics").unwrap();

        assert_eq!(add_to_collection(&conn, "Physics", "a").unwrap(), true);
        assert_eq!(add_to_collection(&conn, "Physics", "a").unwrap(), false);
        assert_eq!(remove_from_collection(&conn, "Physics", "a").unwrap(), true);
        assert_eq!(remove_from_collection(&conn, "Physics", "a").unwrap(), false);

        assert!(add_to_collection(&conn, "Physics", "nonexistent").is_err());
        assert!(add_to_collection(&conn, "NoSuchCollection", "a").is_err());
    }

    // delete_collection removes the subtree and its membership rows, but
    // never the entries themselves.
    #[test]
    fn delete_collection_removes_subtree_but_not_entries() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2024, "Smith", "John");

        create_collection(&conn, "Physics/Quantum/Entropy").unwrap();
        add_to_collection(&conn, "Physics/Quantum", "a").unwrap();

        let removed = delete_collection(&conn, "Physics").unwrap();
        assert_eq!(removed, 3, "Physics + Quantum + Entropy");

        let collections_left: i64 = conn
            .query_row("SELECT COUNT(*) FROM collections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(collections_left, 0);

        let memberships_left: i64 = conn
            .query_row("SELECT COUNT(*) FROM collection_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(memberships_left, 0);

        // The entry itself must survive.
        assert!(get_entry(&conn, "a").unwrap().is_some());
    }

    // Direct membership vs. --recursive, which pulls in descendant
    // collections too.
    #[test]
    fn filter_by_collection_direct_vs_recursive() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Top Paper", 2024, "Smith", "John");
        seed(&conn, "b", "Deep Paper", 2024, "Jones", "Ada");

        create_collection(&conn, "Physics/Quantum").unwrap();
        add_to_collection(&conn, "Physics", "a").unwrap();
        add_to_collection(&conn, "Physics/Quantum", "b").unwrap();

        let physics = collection_by_path(&conn, "Physics").unwrap().unwrap();

        let direct = Filter {
            collection_id: Some(physics),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &direct), ["a"]);

        let recursive = Filter {
            collection_id: Some(physics),
            recursive: true,
            ..Default::default()
        };
        assert_eq!(keys(&conn, &recursive), ["a", "b"]);

        // An id that doesn't exist matches nothing rather than erroring --
        // this is what main.rs substitutes for an unresolvable --collection.
        let unknown = Filter {
            collection_id: Some(-1),
            ..Default::default()
        };
        assert!(keys(&conn, &unknown).is_empty());

        // Regression: a collection whose NAME contains the path separator is
        // unaddressable by path but must still filter correctly by id. The TUI
        // reaches collections this way, and hand-editing the DB can create one.
        conn.execute("INSERT INTO collections (name, parent_id) VALUES ('A/B', NULL)", [])
            .unwrap();
        let slash_id: i64 = conn
            .query_row("SELECT id FROM collections WHERE name = 'A/B'", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO collection_entries (collection_id, entry_id) \
             SELECT ?1, id FROM entries WHERE cite_key = 'a'",
            [slash_id],
        )
        .unwrap();
        let by_id = Filter {
            collection_id: Some(slash_id),
            ..Default::default()
        };
        assert_eq!(keys(&conn, &by_id), ["a"]);
        assert!(collection_by_path(&conn, "A/B").unwrap().is_none());
    }

    // A naive recursive walk over a cyclic parent_id graph would never
    // terminate. Built by direct SQL UPDATE, bypassing move_collection's own
    // guard, since that guard is the thing under test elsewhere.
    #[test]
    fn collection_tree_terminates_on_a_cycle_and_loses_no_rows() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let a = create_collection(&conn, "A").unwrap();
        let b = create_collection(&conn, "A/B").unwrap();
        // Make A a child of B -- A's ancestor chain now cycles A -> B -> A.
        conn.execute("UPDATE collections SET parent_id = ?1 WHERE id = ?2", [b, a])
            .unwrap();

        let tree = collection_tree(&conn).unwrap();
        assert_eq!(tree.len(), 2, "both rows must still be present, just not lost");

        let ids: HashSet<i64> = tree.iter().map(|(_, c)| c.id).collect();
        assert!(ids.contains(&a) && ids.contains(&b));
    }

    // move_collection must refuse to create the cycle in the first place.
    #[test]
    fn move_collection_refuses_to_create_a_cycle() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        create_collection(&conn, "A/B/C").unwrap();

        // A collection can't become its own parent...
        assert!(move_collection(&conn, "A", Some("A")).is_err());
        // ...nor be moved under its own descendant.
        assert!(move_collection(&conn, "A", Some("A/B/C")).is_err());

        // A legitimate move still works.
        assert!(move_collection(&conn, "A/B/C", None).is_ok());
        assert_eq!(collection_by_path(&conn, "C").unwrap().is_some(), true);
    }

    // The TUI reconstructs paths from collection_tree's pre-order walk the
    // same way `collection ls --json` does -- this is that shared algorithm.
    #[test]
    fn collection_tree_paths_matches_the_walk() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        create_collection(&conn, "Physics/Entropy").unwrap();
        create_collection(&conn, "Biology").unwrap();

        let tree = collection_tree(&conn).unwrap();
        let paths = collection_tree_paths(&tree);
        let by_name: HashMap<&str, &str> = tree
            .iter()
            .zip(paths.iter())
            .map(|((_, c), p)| (c.name.as_str(), p.as_str()))
            .collect();

        assert_eq!(by_name["Physics"], "Physics");
        assert_eq!(by_name["Entropy"], "Physics/Entropy");
        assert_eq!(by_name["Biology"], "Biology");
    }

    // full_text is never loaded here (unlike attachments_for_entry(...,
    // true)); only its character length is, via SQLite's length(). Also
    // covers two entries in one call, since the whole point of this
    // function over a per-entry version is doing that in one query.
    #[test]
    fn all_attachment_text_lengths_reports_length_without_loading_text() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "k", "T", 2020, "Doe", "Jane");
        seed(&conn, "other", "Other", 2021, "Roe", "Jan");

        let (att_id, _) = attach(&conn, "k", "/tmp/a.pdf").unwrap();
        attach(&conn, "k", "/tmp/b.pdf").unwrap();
        set_full_text(&conn, att_id, "hello").unwrap();
        attach(&conn, "other", "/tmp/c.pdf").unwrap();

        let id: i64 = conn
            .query_row("SELECT id FROM entries WHERE cite_key = 'k'", [], |r| r.get(0))
            .unwrap();
        let other_id: i64 = conn
            .query_row("SELECT id FROM entries WHERE cite_key = 'other'", [], |r| r.get(0))
            .unwrap();

        let all = all_attachment_text_lengths(&conn).unwrap();
        assert_eq!(all[&id], vec![Some(5), None]);
        assert_eq!(all[&other_id], vec![None]);
    }

    // The realistic regression for the backfill guard: nothing today stops a
    // future caller from calling create_schema twice on one connection (it
    // already runs once per process via init_db). The backfill INSERT must
    // not be the thing that breaks a second call, by trying to re-insert a
    // rowid attachments_fts already indexes.
    #[test]
    fn create_schema_is_idempotent_when_called_twice() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2020, "Smith", "John");
        let (att_id, _) = attach(&conn, "a", "/tmp/a.pdf").unwrap();
        set_full_text(&conn, att_id, "some extracted text").unwrap();

        create_schema(&conn).unwrap();

        // COUNT(*) on an external-content FTS5 table reads the *content*
        // table's row count regardless of whether the index itself was ever
        // populated -- confirmed empty-index still returns a nonzero count.
        // A real MATCH query is the only thing that actually exercises the
        // index, which is the whole point of this test.
        let matched: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM attachments_fts WHERE full_text MATCH '\"extracted\"'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            matched,
            Some(att_id),
            "a second create_schema call must not duplicate or break the fts index"
        );
    }

    // The test that actually catches a broken trigger: insert (with NULL
    // text, since a fresh attachment always starts that way) -> set_full_text
    // resyncs the fts row to the new text (AFTER UPDATE) -> delete the entry,
    // cascading to attachments (AFTER DELETE). Guarding any one of these
    // triggers on `full_text IS NOT NULL` would break this invariant.
    //
    // Deliberately does NOT use COUNT(*) FROM attachments_fts as a proxy for
    // "is it indexed": on an external-content table that reads the content
    // table's row count regardless of whether the index was ever actually
    // populated -- confirmed directly, an attachments_fts with zero index
    // rows still reports the same COUNT(*) as one fully populated. The only
    // things that actually exercise the index are a real MATCH query and a
    // full integrity_check, both used below.
    #[test]
    fn attachments_fts_stays_in_sync_with_inserts_updates_and_deletes() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "zhou2020", "Paper", 2020, "Zhou", "Yi");
        let (att_id, _) = attach(&conn, "zhou2020", "/tmp/zhou.pdf").unwrap();

        set_full_text(&conn, att_id, "quantum entanglement across noisy channels").unwrap();
        let matched: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM attachments_fts WHERE full_text MATCH '\"entanglement\"'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(matched, Some(att_id), "AFTER UPDATE must resync the row to the new text");

        delete_entry(&conn, "zhou2020").unwrap(); // cascades to attachments -> AFTER DELETE

        let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(
            integrity, "ok",
            "a stale or missing fts 'delete' command must not leave the index inconsistent"
        );
    }

    // The whole point of choosing the trigram tokenizer over the default:
    // "raph" is a strict mid-word substring of "paragraph" -- not a token
    // boundary match, which a unicode61-tokenized FTS5 table would require.
    #[test]
    fn text_filter_matches_a_mid_word_substring() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2020, "Smith", "John");
        let (att_id, _) = attach(&conn, "a", "/tmp/a.pdf").unwrap();
        set_full_text(&conn, att_id, "This is a paragraph about entropy.").unwrap();

        let filter = Filter { text: Some("raph".into()), ..Default::default() };
        assert_eq!(keys(&conn, &filter), ["a"], "trigram index must match a mid-word substring");
    }

    // Empirical, not assumed: a 2-character query is too short to form one
    // trigram. Observed behavior on the vendored SQLite (3.46.0): the LIKE
    // constraint still returns the correct row -- FTS5 falls back to
    // scanning full_text directly rather than erroring or returning nothing.
    // Either a correct-but-unindexed result or an error would have been an
    // acceptable outcome per the phase design; a silently wrong (empty)
    // result would not have been, so this pins down which one it actually is.
    #[test]
    fn text_filter_with_a_query_shorter_than_one_trigram_still_matches_correctly() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2020, "Smith", "John");
        let (att_id, _) = attach(&conn, "a", "/tmp/a.pdf").unwrap();
        set_full_text(&conn, att_id, "This is a paragraph about entropy.").unwrap();

        let filter = Filter { text: Some("ra".into()), ..Default::default() };
        assert_eq!(
            keys(&conn, &filter),
            ["a"],
            "a sub-trigram query must still return correct matches, not silently empty ones"
        );
    }

    // A literal `"` in the search text must survive FTS5's phrase-quoting
    // (doubled to escape, per fts5_phrase) rather than breaking the MATCH
    // syntax or being silently dropped. Also exercises the MATCH path (>=3
    // chars) specifically, since this is what replaced the first draft's
    // LIKE-on-attachments_fts approach.
    #[test]
    fn text_filter_handles_a_literal_double_quote_in_the_query() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper", 2020, "Smith", "John");
        let (att_id, _) = attach(&conn, "a", "/tmp/a.pdf").unwrap();
        set_full_text(&conn, att_id, "She said \"hello world\" to everyone.").unwrap();

        let filter = Filter { text: Some("said \"hello".into()), ..Default::default() };
        assert_eq!(keys(&conn, &filter), ["a"]);
    }

    // The bug the first draft shipped with: a per-row correlated EXISTS
    // against attachments_fts doesn't just fail to use the trigram index,
    // it can (in principle) evaluate incorrectly under join reordering.
    // Pins down that the non-correlated `entries.id IN (...)` rewrite keeps
    // each entry scoped to its own attachments only, across multiple
    // entries where only one actually matches.
    #[test]
    fn text_filter_scopes_matches_to_the_right_entry_among_several() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        seed(&conn, "a", "Paper A", 2020, "Smith", "John");
        seed(&conn, "b", "Paper B", 2021, "Doe", "Jane");
        seed(&conn, "c", "Paper C", 2022, "Lee", "Kim");
        let (att_a, _) = attach(&conn, "a", "/tmp/a.pdf").unwrap();
        let (att_b, _) = attach(&conn, "b", "/tmp/b.pdf").unwrap();
        let (att_c, _) = attach(&conn, "c", "/tmp/c.pdf").unwrap();
        set_full_text(&conn, att_a, "nothing relevant here").unwrap();
        set_full_text(&conn, att_b, "quantum entanglement across channels").unwrap();
        set_full_text(&conn, att_c, "also nothing relevant").unwrap();

        let filter = Filter { text: Some("entanglement".into()), ..Default::default() };
        assert_eq!(keys(&conn, &filter), ["b"], "only the entry whose own attachment matches");
    }

    // A pre-existing library (attachments predating attachments_fts, some
    // never `--extract`ed) upgrading through create_schema's migration path
    // -- not a fresh schema, where every row already goes through the AFTER
    // INSERT trigger from birth, so this is the one scenario none of the
    // other fts tests can exercise. Manually builds the pre-Phase-15 shape
    // by hand (not create_schema) so the later create_schema call is a real
    // migration, not schema creation from scratch.
    //
    // This used to fail: the original backfill was a manual
    // `INSERT ... SELECT ... WHERE full_text IS NOT NULL`, silently
    // excluding the NULL row from the index while the AFTER UPDATE/DELETE
    // triggers assumed every row was present -- the first trigger firing on
    // that row corrupted the database outright ("database disk image is
    // malformed", reproduced against the real bundled SQLite 3.46.0).
    // Fixed by using FTS5's own 'rebuild' command for the backfill, which
    // re-scans every content row unconditionally, matching what the
    // triggers already assume.
    #[test]
    fn create_schema_migration_backfills_pre_existing_attachments_including_unextracted_ones() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE entries (id INTEGER PRIMARY KEY AUTOINCREMENT, entry_type TEXT NOT NULL,
                 cite_key TEXT UNIQUE NOT NULL, title TEXT NOT NULL, year INTEGER, journal TEXT,
                 volume TEXT, pages TEXT, doi TEXT, url TEXT, abstract TEXT,
                 date_added INTEGER NOT NULL, date_modified INTEGER NOT NULL);
             CREATE TABLE attachments (id INTEGER PRIMARY KEY AUTOINCREMENT,
                 entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
                 path TEXT NOT NULL, date_added INTEGER NOT NULL, full_text TEXT,
                 UNIQUE (entry_id, path));",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO entries (entry_type, cite_key, title, date_added, date_modified) \
             VALUES ('article', 'a', 'Paper A', 0, 0)",
            [],
        )
        .unwrap();
        // One attachment never extracted (full_text NULL), one already extracted --
        // exactly the audit's "two attachments, one extracted one not" case.
        conn.execute(
            "INSERT INTO attachments (entry_id, path, date_added, full_text) \
             VALUES (1, '/tmp/never-extracted.pdf', 0, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments (entry_id, path, date_added, full_text) \
             VALUES (1, '/tmp/already-extracted.pdf', 0, 'some text')",
            [],
        )
        .unwrap();

        // This is the actual migration path a real upgrade goes through.
        create_schema(&conn).unwrap();

        // "ferref extract" on the never-extracted attachment.
        let never_extracted_id: i64 = conn
            .query_row(
                "SELECT id FROM attachments WHERE path = '/tmp/never-extracted.pdf'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let extract_result = set_full_text(&conn, never_extracted_id, "newly extracted text");
        assert!(extract_result.is_ok(), "extract after migration: {extract_result:?}");

        // "ferref rm" on the entry (cascades to both attachments).
        let rm_result = delete_entry(&conn, "a");
        assert!(rm_result.is_ok(), "rm after migration: {rm_result:?}");

        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok", "database must not be left malformed");
    }

    // Real files on disk (not just DB rows) so a silent rename-overwrite
    // would be observably wrong: keep and drop each get a distinct
    // attachment, plus a shared tag and a shared collection to exercise the
    // UPDATE OR IGNORE collision path this test would otherwise never touch.
    #[test]
    fn merge_entries_folds_tags_collections_and_attachments_without_collision() {
        let dir = std::env::temp_dir().join(format!(
            "ferref-merge-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let mut keep = Entry::new("article".to_string(), "keep2024".to_string(), "Keep".to_string());
        keep.doi = None;
        let keep_id = insert_entry(&conn, &keep).unwrap();

        let mut drop = Entry::new("article".to_string(), "drop2024".to_string(), "Drop".to_string());
        drop.doi = None;
        let drop_id = insert_entry(&conn, &drop).unwrap();

        // Shared tag: both entries get "physics" -- forces the entry_tags
        // collision path.
        add_tag(&conn, "keep2024", "physics").unwrap();
        add_tag(&conn, "drop2024", "physics").unwrap();
        // drop-only tag: should survive the move onto keep.
        add_tag(&conn, "drop2024", "solo").unwrap();

        // Shared collection: forces the collection_entries collision path.
        create_collection(&conn, "Shelf").unwrap();
        add_to_collection(&conn, "Shelf", "keep2024").unwrap();
        add_to_collection(&conn, "Shelf", "drop2024").unwrap();

        // Distinct attachment per entry, both real files on disk.
        let keep_pdf = dir.join("keep2024.pdf");
        std::fs::write(&keep_pdf, b"keep bytes").unwrap();
        attach(&conn, "keep2024", keep_pdf.to_str().unwrap()).unwrap();

        let drop_pdf = dir.join("drop2024.pdf");
        std::fs::write(&drop_pdf, b"drop bytes").unwrap();
        attach(&conn, "drop2024", drop_pdf.to_str().unwrap()).unwrap();

        merge_entries(&conn, keep_id, drop_id).unwrap();

        // drop's entry row is gone.
        assert!(get_entry(&conn, "drop2024").unwrap().is_none());

        let kept = get_entry(&conn, "keep2024").unwrap().unwrap();

        // Union of tags, no duplicate-row error, no lost drop-only tag.
        assert_eq!(kept.tags, vec!["physics".to_string(), "solo".to_string()]);

        // Still in the shared collection exactly once (PK collision handled).
        let in_shelf: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM collection_entries ce \
                 JOIN collections c ON c.id = ce.collection_id \
                 WHERE c.name = 'Shelf' AND ce.entry_id = ?1",
                [keep_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_shelf, 1);

        // Both attachments now belong to keep, at two distinct real files --
        // neither clobbered the other.
        assert_eq!(kept.attachments.len(), 2);
        let mut contents: Vec<Vec<u8>> = kept
            .attachments
            .iter()
            .map(|a| std::fs::read(&a.path).expect("attachment file must exist on disk"))
            .collect();
        contents.sort();
        assert_eq!(contents, vec![b"drop bytes".to_vec(), b"keep bytes".to_vec()]);
        let paths: std::collections::HashSet<&str> =
            kept.attachments.iter().map(|a| a.path.as_str()).collect();
        assert_eq!(paths.len(), 2, "attachment paths must be distinct, not one overwriting the other");

        std::fs::remove_dir_all(&dir).ok();
    }

    // A drop-side attachment row pointing at a file that no longer exists
    // (the DB is hand-editable by design, so this is reachable without any
    // bug elsewhere) must fail the whole merge before anything is touched --
    // not after quietly moving the *other* attachment first. Regression
    // test for a bug an adversarial review caught: the original
    // implementation moved files one at a time inside the loop, so a
    // failure on attachment 2 left attachment 1 already renamed on disk
    // while its DB row (rolled back with the rest of the transaction) still
    // pointed at the old, now-nonexistent path.
    #[test]
    fn merge_entries_leaves_everything_untouched_when_a_drop_attachment_file_is_missing() {
        let dir = std::env::temp_dir().join(format!(
            "ferref-merge-missing-file-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();

        let keep = Entry::new("article".to_string(), "keep2024".to_string(), "Keep".to_string());
        let keep_id = insert_entry(&conn, &keep).unwrap();
        let drop = Entry::new("article".to_string(), "drop2024".to_string(), "Drop".to_string());
        let drop_id = insert_entry(&conn, &drop).unwrap();

        // A real file that will actually get renamed if the pre-flight
        // check doesn't fire first.
        let real_pdf = dir.join("drop2024.pdf");
        std::fs::write(&real_pdf, b"real bytes").unwrap();
        attach(&conn, "drop2024", real_pdf.to_str().unwrap()).unwrap();

        // A second attachment row whose file was deleted out from under it
        // (or hand-inserted, same effect) -- this is what should make the
        // whole merge fail.
        let ghost_pdf = dir.join("ghost.pdf");
        conn.execute(
            "INSERT INTO attachments (entry_id, path, date_added) VALUES (?1, ?2, ?3)",
            rusqlite::params![drop_id, ghost_pdf.to_str().unwrap(), 0],
        )
        .unwrap();

        let err = merge_entries(&conn, keep_id, drop_id);
        assert!(err.is_err(), "merge must fail when a drop attachment file is missing");

        // Nothing moved: the real file is exactly where it started, keep
        // has no attachments, and drop still owns both rows.
        assert!(real_pdf.exists(), "the real file must not have been moved");
        let kept = get_entry(&conn, "keep2024").unwrap().unwrap();
        assert!(kept.attachments.is_empty(), "keep must gain nothing from a failed merge");
        let dropped = get_entry(&conn, "drop2024").unwrap().unwrap();
        assert_eq!(dropped.attachments.len(), 2, "drop must keep both attachment rows");

        std::fs::remove_dir_all(&dir).ok();
    }
}
