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
"#;

// CREATE TABLE IF NOT EXISTS does not add a column to a table that already
// exists. Phase 6 shipped `attachments` without `full_text`, so any DB
// created before this phase needs the column added explicitly -- this is
// the project's first migration.
fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;

    let has_full_text = conn
        .prepare("SELECT 1 FROM pragma_table_info('attachments') WHERE name = 'full_text'")?
        .exists([])?;
    if !has_full_text {
        conn.execute("ALTER TABLE attachments ADD COLUMN full_text TEXT", [])?;
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
fn attachments_for_entry(
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

// Records a path, never a copy of the file — see DESIGN.md. Idempotent per the
// UNIQUE(entry_id, path) constraint; the bool reports whether a row was added.
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
pub fn attachment_text_lengths(conn: &Connection, entry_id: i64) -> Result<Vec<(String, Option<i64>)>> {
    let mut stmt = conn
        .prepare("SELECT path, length(full_text) FROM attachments WHERE entry_id = ?1 ORDER BY id")?;
    stmt.query_map([entry_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<(String, Option<i64>)>>>()
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

// Creates intermediate collections as needed (mkdir -p semantics) and
// returns the leaf id. Re-running with the same path is a no-op that returns
// the existing leaf rather than duplicating it -- each segment is looked up
// case-insensitively among its siblings before an insert is attempted, which
// is also what makes sibling names unique without relying on a DB
// constraint (see the schema comment on `collections`).
pub fn create_collection(conn: &Connection, path: &str) -> Result<i64> {
    let segments = split_path(path).map_err(rusqlite::Error::InvalidParameterName)?;

    let mut id = 0i64;
    let mut parent: Option<i64> = None;
    for seg in &segments {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM collections WHERE lower(name) = lower(?1) AND parent_id IS ?2",
                rusqlite::params![seg, parent],
                |row| row.get(0),
            )
            .optional()?;
        id = match existing {
            Some(existing_id) => existing_id,
            None => {
                conn.execute(
                    "INSERT INTO collections (name, parent_id) VALUES (?1, ?2)",
                    rusqlite::params![seg, parent],
                )?;
                conn.last_insert_rowid()
            }
        };
        parent = Some(id);
    }
    Ok(id)
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

// Idempotent, same shape as add_tag: returns whether membership actually
// changed. Unknown cite_key -> Err(QueryReturnedNoRows), same as add_tag.
// Unknown collection path -> Err(InvalidParameterName) with a message
// naming the path, so it isn't confused with the cite_key error upstream.
pub fn add_to_collection(conn: &Connection, path: &str, cite_key: &str) -> Result<bool> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;
    let collection_id = collection_by_path(conn, path)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("no collection found at path '{path}'"))
    })?;

    conn.execute(
        "INSERT OR IGNORE INTO collection_entries (collection_id, entry_id) VALUES (?1, ?2)",
        rusqlite::params![collection_id, entry_id],
    )?;
    Ok(conn.changes() > 0)
}

// Idempotent: Ok(false) if the entry wasn't in the collection. Same error
// shape as add_to_collection.
pub fn remove_from_collection(conn: &Connection, path: &str, cite_key: &str) -> Result<bool> {
    let entry_id: i64 = conn.query_row(
        "SELECT id FROM entries WHERE cite_key = ?1",
        [cite_key],
        |row| row.get(0),
    )?;
    let collection_id = collection_by_path(conn, path)?.ok_or_else(|| {
        rusqlite::Error::InvalidParameterName(format!("no collection found at path '{path}'"))
    })?;

    conn.execute(
        "DELETE FROM collection_entries WHERE collection_id = ?1 AND entry_id = ?2",
        rusqlite::params![collection_id, entry_id],
    )?;
    Ok(conn.changes() > 0)
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
        entry.attachments = attachments_for_entry(conn, entry.id.unwrap(), with_full_text)?;
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
    // true)); only its character length is, via SQLite's length().
    #[test]
    fn attachment_text_lengths_reports_length_without_loading_text() {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        let mut entry = Entry::new("article".into(), "k".into(), "T".into());
        let id = insert_entry(&conn, &entry).unwrap();
        entry.id = Some(id);

        let (att_id, _) = attach(&conn, "k", "/tmp/a.pdf").unwrap();
        attach(&conn, "k", "/tmp/b.pdf").unwrap();
        set_full_text(&conn, att_id, "hello").unwrap();

        let lens = attachment_text_lengths(&conn, id).unwrap();
        assert_eq!(lens.len(), 2);
        assert_eq!(lens[0], ("/tmp/a.pdf".to_string(), Some(5)));
        assert_eq!(lens[1], ("/tmp/b.pdf".to_string(), None));
    }
}
