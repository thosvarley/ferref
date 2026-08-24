# ferref

A command-line reference manager that keeps your library in a plain SQLite file
and gets out of the way.

The premise: the main consumer of a reference library increasingly isn't a
person reading it — it's a script, a model, an embedding pipeline. So ferref
stores everything in a file anyone can open with `sqlite3`, prints `--json` from
every command that emits data, and pulls full text out of PDFs into a queryable
column. It does not do the AI work itself. It hands over clean structured data
and stops.

See `DESIGN.md` for the reasoning, the phase plan, and a frank list of known
limitations.

## Requirements

- Rust (2024 edition) — `cargo build --release`
- `pdftotext` from **poppler-utils**, for full-text extraction
  (`apt install poppler-utils`)
- `xdg-open` (Linux) or `open` (macOS), for `ferref open`

SQLite is bundled via `rusqlite`; you don't need to install it.

```sh
cargo build --release
./target/release/ferref --help
```

The database is `./ferref.db` in whatever directory you run from. That's
deliberate — a library is a folder you `cd` into, like a git repo. It also means
running ferref somewhere else gives you a different, empty library.

## Tutorial

Everything below is a real session. Make a directory and work in it.

```sh
mkdir ~/papers && cd ~/papers
```

### 1. Add a paper by DOI

The fastest way in. Crossref fills in the metadata, and the cite key is derived
from the first author and year.

```console
$ ferref add --doi 10.1103/PhysRev.106.620
Information Theory and Statistical Mechanics [jaynes1957]
  Type: article
  Authors: Jaynes, E. T.
  Year: 1957
  Journal: Physical Review
  Volume: 106
  Pages: 620-630
  DOI: 10.1103/PhysRev.106.620
```

Or add one by hand when there's no DOI. `--author` repeats, each as
`"Last, First"`:

```sh
ferref add --type article --key shannon1948 \
  --title "A Mathematical Theory of Communication" \
  --author "Shannon, Claude E." \
  --year 1948 --journal "Bell System Technical Journal"
```

### 2. Look at your library

```console
$ ferref list
jaynes1957      1957   Information Theory and Statistical Mechanics       Jaynes, E. T.
shannon1948     1948   A Mathematical Theory of Communication             Shannon, Claude E.
```

`show` gives one entry in full, and every data command takes `--json`:

```sh
ferref show jaynes1957
ferref show jaynes1957 --json
```

### 3. Tag things

Tags are lowercased and trimmed, so `ML`, `ml`, and `  Ml  ` are one tag.
Tagging twice is a no-op, not an error.

```console
$ ferref tag jaynes1957 "  Entropy  "
Tagged 'jaynes1957' with 'entropy'
$ ferref tag jaynes1957 entropy
'jaynes1957' already tagged 'entropy'
```

### 4. Organise into collections

Tags and collections do different jobs and both exist. A **tag** describes a
paper and doesn't nest; a **collection** is *where a paper lives*, and it does.

Paths are `mkdir -p`-style — creating a nested path creates every level:

```console
$ ferref collection new "Information Theory/Foundations"
Created collection 'Information Theory/Foundations' (id 2)

$ ferref collection add "Information Theory/Foundations" shannon1948
Added 'shannon1948' to 'Information Theory/Foundations'
$ ferref collection add "Information Theory" jaynes1957
Added 'jaynes1957' to 'Information Theory'

$ ferref collection ls
Information Theory (1)
  Foundations (1)
```

Filtering is direct by default; `--recursive` includes descendants:

```console
$ ferref list --collection "Information Theory"
jaynes1957      1957   Information Theory and Statistical Mechanics       Jaynes, E. T.

$ ferref list --collection "Information Theory" --recursive
jaynes1957      1957   Information Theory and Statistical Mechanics       Jaynes, E. T.
shannon1948     1948   A Mathematical Theory of Communication             Shannon, Claude E.
```

`collection mv` reparents a subtree and refuses to create a loop:

```console
$ ferref collection mv "Information Theory" --parent "Information Theory/Foundations"
Error: cannot move a collection under its own descendant
```

`collection delete` removes a collection and its subtree. It never deletes
entries — only their membership.

### 5. Search

Filters combine with AND. `--author` and `--title` are case-insensitive
substring matches; `--tag` is an exact match, because tags are identifiers.

```sh
ferref search --author jaynes
ferref search --title "information theory" --from 1950 --to 1960
ferref search --tag ENTROPY --year 1957
```

No matches prints nothing and exits 0 — it's a query, not a test.

### 6. Attach a PDF you already have

ferref copies the file into `./pdfs/`, named after the cite_key — the same
scheme `fetch` uses — and stores that copy's absolute path. Your original is
left where it is. The point is that every paper in a library sits in one
directory, whether it arrived by hand or over the network, so backing the whole
thing up is `pdfs/` plus one `.db` file.

Say you've already downloaded a paper and added its entry (`ferref add --doi
10.1186/s12859-020-3494-x`, which the next step covers):

```console
$ ferref attach zhou2020 ~/Downloads/zhou2020.pdf --extract
Attached '/home/you/papers/pdfs/zhou2020.pdf' to 'zhou2020'
Extracted 35163 characters from '/home/you/papers/pdfs/zhou2020.pdf'
```

(The character count is whatever's in your PDF.)

Attaching the same file twice is a no-op. Attaching a *second*, different file
to the same entry — a paper and its supplement — gets `zhou2020-2.pdf`
rather than overwriting the first.

`--extract` runs `pdftotext` and stores the result. Without it, use
`ferref extract zhou2020` later. `ferref open zhou2020` opens the
attachments in your default viewer.

### 7. Fetch an open-access PDF automatically

`fetch` asks Unpaywall whether a legal open-access copy exists, and if one does,
downloads it to `./pdfs/`, attaches it, and extracts the text.

This needs a contact email — Unpaywall's polite-pool policy. Set it once:

```sh
mkdir -p ~/.config/ferref
echo 'email = you@example.com' > ~/.config/ferref/config.toml
```

(Or pass `--email`, or set `FERREF_EMAIL`. The email is sent to Unpaywall and
nowhere else — Crossref never sees it.)

Add an open-access paper, then fetch it:

```console
$ ferref add --doi 10.1038/s41586-020-2649-2
Array programming with NumPy [harris2020]
  ...

$ ferref fetch harris2020
Downloaded open-access PDF for 'harris2020' to '/home/you/papers/pdfs/harris2020.pdf'
Extracted 41013 characters from '/home/you/papers/pdfs/harris2020.pdf'
```

Plenty of genuinely open papers are only linked as landing pages, so you'll also
see this — it's an answer, not a failure, and exits 0:

```console
$ ferref add --doi 10.7717/peerj.4375   # then:
$ ferref fetch piwowar2018
'piwowar2018' (DOI 10.7717/peerj.4375) is open access, but Unpaywall has no
direct PDF link for it -- only landing pages
```

ferref will not work around a paywall. If Unpaywall says there's no legal open
copy, that's the end of it.

### 8. Get the text back out

This is the point of the whole thing. `show --json` carries the extracted text:

```console
$ ferref show harris2020 --json | jq -r '.attachments[].full_text' | head -3
Review

Array programming with NumPy
```

`list` and `search` omit full text unless you ask, because otherwise every
listing would pull every PDF's text into memory:

```sh
ferref list --json --full-text | jq -r '.[] | "\(.cite_key)\t\(.attachments[0].full_text // "" | length)"'
```

### 9. Cite and export

```console
$ ferref cite jaynes1957
Jaynes, E. T. (1957). Information Theory and Statistical Mechanics. Physical Review, 106, 620–630. https://doi.org/10.1103/PhysRev.106.620

$ ferref cite jaynes1957 --style mla
Jaynes, E. T. "Information Theory and Statistical Mechanics." Physical Review, vol. 106, 1957, pp. 620-630. https://doi.org/10.1103/PhysRev.106.620
```

APA lists every author (no 21-author truncation); MLA collapses three or more
to `et al.`

```sh
ferref export > library.bib          # all entries as BibTeX
ferref import someone-elses.bib      # duplicate cite keys are skipped, not fatal
```

## Browsing in the terminal

`ferref tui` opens a three-pane browser: collections on the left, entries in the
middle, details for the highlighted entry on the right.

```
┌─────────────┬─ENTRIES [year ↓]───────────────┬──────────────┐
│ COLLECTIONS │ Title        Authors  Year Jrnl │ DETAILS      │
│ ▾ All (12)  │ Array progr… Harris  2020 Natu │ Jaynes, E.T. │
│   ▾ Physics │ Information… Jaynes  1957 Phys │ Phys Rev 106 │
│     Entropy │ A Mathemati… Shannon 1948 Bell │ #entropy     │
└─────────────┴────────────────────────────────┴──────────────┘
 Tab pane · jk move · / search · s sort · n new · c file · o open · q quit
```

It navigates like vim, and like arrows — both work everywhere.

| Key | Does |
| --- | --- |
| `Tab` / `Shift-Tab` | Cycle focus between panes |
| `j` `k` or `↑` `↓` | Move the selection |
| `g` / `G` | Jump to top / bottom |
| `Ctrl-d` / `Ctrl-u` | Half a page down / up |
| `h` `l` or `←` `→` | Collapse / expand a collection; elsewhere, move a pane left or right |
| `s` / `S` | Cycle the sort column (title → author → year → journal) / reverse it |
| `/` | Search — filters the table live as you type, over titles, authors, journal, year, cite_key and tags |
| `n` | New collection, as a child of the highlighted one (or at the root, under "All Papers") |
| `c` | File the highlighted paper into a collection — `Enter` toggles, so it unfiles too |
| `o` | Open the highlighted paper's attachments in your viewer |
| `r` | Reload from the database |
| `q`, `Esc`, `Ctrl-C` | Quit — `Esc` clears an active search first |

Selecting a collection filters the middle pane recursively, so a parent shows
everything beneath it. Sorting and searching happen in memory over what's
already loaded, so they're instant and they don't touch the database.

**What the TUI can change:** it files papers into collections and creates
collections. That's it. Editing entries, deleting anything, and tagging stay
CLI-only, so a mis-keypress can misfile a paper but can't destroy data. Press
`r` to pick up changes made from another shell.

## Command reference

| Command | What it does |
| --- | --- |
| `add` | Create an entry. `--type/--key/--title`, or `--doi` to autofill from Crossref |
| `list` | List everything. `--tag`, `--collection`, `--recursive`, `--full-text` |
| `show <key>` | One entry in full |
| `edit <key>` | Change fields; only the flags you pass are touched |
| `rm <key>` | Delete an entry (cascades to authors, tags, attachments) |
| `search` | `--author --title --year --from --to --tag --collection --recursive --full-text` |
| `tag` / `untag <key> <tag>` | Add/remove a tag (flat, describes a paper). Idempotent |
| `attach <key> <path>` | Copy a file into `pdfs/` and attach it. `--extract` to pull text immediately |
| `extract <key>` | (Re)extract text for all of an entry's attachments |
| `open <key>` | Open attachments in the system viewer |
| `fetch <key>` | Find and download an open-access PDF for the entry's DOI |
| `cite <key>` | `--style apa` (default) or `--style mla` |
| `collection new <path>` | Create a collection, `mkdir -p` style |
| `collection ls` | The whole tree, with entry counts |
| `collection add` / `rm` | Add or remove an entry's membership. Idempotent |
| `collection mv <path>` | Reparent a subtree: `--parent <path>` or `--root` |
| `collection delete <path>` | Delete a subtree; entries are never deleted |
| `tui` | Three-pane terminal browser: sort, search, file papers into collections |
| `import <path>` | Read a `.bib` file |
| `export` | Write BibTeX to stdout, or `--out file.bib` |

Every command above takes `--json` except `export`, whose output format is
BibTeX by definition, and `tui`, which is an interactive screen rather than a
data-printing command.

## Scripting

`--json` on everything is the feature, not a convenience. The examples below
use [`jq`](https://jqlang.github.io/jq/), which ferref does not require — any
JSON tool works:

```sh
ferref list --json | python3 -c 'import json,sys; [print(e["cite_key"]) for e in json.load(sys.stdin)]'
```

Some patterns:

```sh
# Every cite key, one per line
ferref list --json | jq -r '.[].cite_key'

# Extract text for the whole library
ferref list --json | jq -r '.[].cite_key' | xargs -n1 ferref extract

# Try to fetch an OA PDF for everything that has a DOI
ferref list --json | jq -r '.[] | select(.doi) | .cite_key' | xargs -n1 ferref fetch

# Bibliography for one tag
ferref search --tag to-read --json | jq -r '.[].cite_key' | xargs -n1 ferref cite

# Export one collection (and everything under it) as BibTeX
ferref list --collection "Information Theory" --recursive --json \
  | jq -r '.[].cite_key' > keys.txt

# Dump full text for an embedding pipeline
ferref list --json --full-text \
  | jq -r '.[] | select(.attachments[0].full_text) | [.cite_key, .attachments[0].full_text] | @tsv'
```

Exit codes: `0` on success, `1` on failure, `2` for a usage error from the
argument parser. "No results" and "no open-access copy available" are successes.

Output is safe to pipe into `head` — ferref exits cleanly on a closed pipe
rather than panicking.

### The JSON shape

```json
{
  "id": 2,
  "entry_type": "article",
  "cite_key": "harris2020",
  "title": "Array programming with NumPy",
  "authors": [{ "first_name": "Charles R.", "last_name": "Harris" }],
  "tags": [],
  "attachments": [{ "path": "/home/you/papers/pdfs/harris2020.pdf", "full_text": "Review\n\nArray..." }],
  "year": 2020,
  "journal": "Nature",
  "volume": "585",
  "pages": "357-362",
  "doi": "10.1038/s41586-020-2649-2",
  "url": null,
  "abstract": null,
  "date_added": 1787430871,
  "date_modified": 1787430871
}
```

`full_text` is `null` when nothing has been extracted, and also in `list`/
`search` output unless you pass `--full-text`. `abstract` is spelled `abstract`,
not `abstract_text`, and there's a test pinning that.

## The database

It's just SQLite. Go read it:

```sh
sqlite3 ferref.db '.schema'
sqlite3 ferref.db "SELECT cite_key, title FROM entries WHERE year > 2015;"
```

Tables: `entries`, `authors`, `tags`, `entry_tags`, `attachments`, `collections`,
`collection_entries`. Foreign keys cascade from `entries`, so deleting an entry
cleans up after itself, and from `collections`, so deleting a collection takes
its subtree without touching the papers. `cite_key` and `id` are the stable
identifiers — key your own tools against those.

## Limitations worth knowing

- The database is always `./ferref.db`, relative to the current directory.
- `edit` can't clear a field back to null, and there's no `detach`.
- Attachment paths are absolute and stored once. The files live in `pdfs/`, but
  the paths don't move with the library — relocating the directory breaks every
  link silently.
- BibTeX export collapses non-legacy entry types (`@online`, `@dataset`) to
  `@misc`, and tags don't survive a round trip.
- `cite` covers APA and MLA only, and doesn't recase titles or handle APA's
  21-author truncation.
- Extraction is PDF-only, capped at 10 MB of text per attachment.
- A collection whose *name* contains `/` can't be addressed by the CLI's path
  syntax. The CLI won't create one; only hand-editing the database can. The TUI
  reaches collections by id and handles them fine.
- The TUI files papers and creates collections, but can't edit or delete
  anything, rename a collection, or tag — use the CLI, then press `r`.
- The TUI doesn't watch the database. Changes from another shell appear on `r`,
  not on their own.

`DESIGN.md` has the full list with the reasoning behind each.

## Development

```sh
cargo test        # 72 tests, no network access required
cargo build
```

No test touches the network; the Crossref and Unpaywall parsers are tested
against captured fixture strings.
