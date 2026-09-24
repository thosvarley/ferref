# The database

Your library is an ordinary SQLite file at `~/.ferref/ferref.db`. You can query
it directly:

```sh
sqlite3 ~/.ferref/ferref.db '.schema'
sqlite3 ~/.ferref/ferref.db "SELECT cite_key, title FROM entries WHERE year > 2015;"
```

## Tables

| Table | Holds |
| --- | --- |
| `entries` | One row per paper: title, year, journal, DOI, abstract, and so on |
| `authors` | Each paper's authors, in order |
| `tags`, `entry_tags` | Tag names, and which papers have which tags |
| `collections` | The collection tree. Each row points to its parent |
| `collection_entries` | Which papers are filed in which collections |
| `attachments` | Each attached file's path, and its extracted text |
| `attachments_fts` | The full-text search index. ferref keeps it up to date; don't write to it |

Deleting a paper also deletes its authors, tags, memberships, and attachment
records. Deleting a collection deletes its subcollections, but never the papers
in them.

## Keying your own data

`entries.id` and `entries.cite_key` never change. Use either one to link your
own data (embeddings, notes, citation graphs) to ferref's papers.

## Editing by hand

Reading the database is always safe. Writing to it works too, but ferref's own
commands do a few checks that raw SQL skips (for example, that two papers don't
share a DOI). If you do edit it by hand, `ferref doctor` will tell you about
attachment rows that point to missing files.
