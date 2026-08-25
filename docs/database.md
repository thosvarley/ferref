# The database

It's just SQLite. Go read it:

```sh
sqlite3 ~/.ferref/ferref.db '.schema'
sqlite3 ~/.ferref/ferref.db "SELECT cite_key, title FROM entries WHERE year > 2015;"
```

Tables: `entries`, `authors`, `tags`, `entry_tags`, `attachments`, `collections`,
`collection_entries`. Foreign keys cascade from `entries`, so deleting an entry
cleans up after itself, and from `collections`, so deleting a collection takes
its subtree without touching the papers. `cite_key` and `id` are the stable
identifiers — key your own tools against those.

Full text extracted from attachments is indexed for substring search via an
FTS5 trigram virtual table (`attachments_fts`), kept in sync with `attachments`
by triggers — don't write to it directly. See {doc}`scripting` for how to use
it from the CLI.
