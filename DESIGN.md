# ferref — Design Doc

## Vision

A reference manager for a world where the main consumer of your library isn't
just you reading it — it's a model. Not a tool that *does* embeddings,
bibliometrics, or LLM analysis, but one that gets out of the way of tools that
do. The contrast with Zotero isn't CLI-vs-GUI, it's "opaque app-owned
datastore" vs. "plain SQLite file + scriptable CLI that any script or model
pipeline can hit directly."

Design principles that follow from that, and constrain every phase below:

- **Open data.** The DB is a plain SQLite file. No proprietary blob formats.
  Anyone can `sqlite3 ferref.db` and get something sane.
- **Scriptable by default.** Every CLI command that prints data supports
  `--json`, from the first subcommand that exists. This is the thing that
  makes ferref pipeable into a Python embedding script or an LLM-feeding
  pipeline — retrofitting it later means touching every command twice.
- **Text-first.** Abstracts, notes, and extracted full text are first-class
  fields, not an afterthought bolted on after cosmetic features. This is what
  actually enables embeddings/bibliometrics later, so it's pulled ahead of
  things like citation formatting.
- **Stable IDs.** `cite_key` (and DB `id`) are what external tools key
  embeddings, graphs, or analysis results against ferref entries. Don't
  redesign these casually once other tools depend on them.
- UI is CLI now, TUI later, never a GUI — out of scope, not deferred.
- The code should be written in a **modular** and largely **functional**
  form. Keeping things modular should aid in maintenance and debuggging. 

Essential features that should be present at the end:

- The ability to make sub-collections and assign papers from the main library
  to them. 
- The ability to export collections or subcollections as Bibtex files. 
- A "fetch" function that, given a DOI, tries to find the full text online. 
- Some kind of intuitive directory structure where PDFs are stored. 
- A field to link authors to ORCID IDs if available. 

## Current state (2026-08-24)

All fifteen phases are complete.

- `src/models.rs` — `Entry`/`Author` + `now()`. `Serialize` derived; `abstract_text` serializes as `"abstract"` (Rust reserves `abstract`), locked by a test.
- `src/db.rs` — schema + `insert_entry`/`get_entry`/`list_entries`/`update_entry`/`delete_entry`, all free functions over `&Connection`. `update_entry` stamps `date_modified` itself so callers can't forget. `list_entries` takes a `Filter`; an all-`None` filter matches everything, so `list` and `search` are one query. `add_tag`/`remove_tag` are idempotent and report whether anything changed; `normalize_tag` (trim + lowercase) is the single point both writes and the `tag` filter go through. `attach` copies the file into `./pdfs/` under the same `<cite_key>.<ext>` scheme `fetch` uses, and stores the copy's path, so a library is one directory of papers. The name is claimed with `O_EXCL`, not by checking whether it exists first — two concurrent attaches to one cite_key otherwise lose a file 23% of the time, each reporting success.
- `src/bibtex.rs` — `import`/`export` over the `biblatex` crate, plus the `biblatex::Entry` ↔ `models::Entry` mapping.
- `src/text.rs` — `extract_text` over `pdftotext`. The project's trust boundary: bounded memory (the drain keeps the first 10MB and discards the rest rather than buffering it all), a 30s deadline covering *both* the child wait and the pipe drain, and a process-group kill so a wrapper script's descendants can't outlive us.
- `src/doi.rs` — Crossref metadata + Unpaywall OA lookup over `ureq`. The network trust boundary: every request goes through `fetch_guarded`, which follows redirects by hand so each hop's scheme and resolved IP are revalidated, with capped reads and a `%PDF` magic-byte check before anything is written.
- `src/config.rs` — reads one key (`email`) from `~/.config/ferref/config.toml`. Deliberately a line reader, not TOML, and not a settings system.
- Collections (Phase 10) nest; tags (Phase 5) don't. A tag describes a paper, a collection is where it lives. `collection_tree` is the single traversal, and it terminates on a cyclic `parent_id` graph because the DB is hand-editable by design.
- `src/tui.rs` — three-pane TUI over ratatui. Blocking `event::read()`, DB queried only on state change, and `Filter` addressed by collection **id** rather than path. Sorts, filters, creates collections and files papers into them (Phase 12); everything else is still CLI-only.
- `src/cli.rs` — clap derive types for `add`/`list`/`show`/`edit`/`rm`/`search`/`import`/`export`, plus `parse_author`.
- `src/main.rs` — thin dispatcher; the four biggest commands (`attach`, `extract`, `import`, `fetch`) live in their own `cmd_*` functions rather than inline match arms, the same shape `dispatch_collection` already had. All JSON goes through `emit_json`, which streams to stdout instead of building the document as a `String` first — that halved peak memory on `list --full-text --json`. All stdout goes through `emit()`, which exits 0 on a closed pipe instead of panicking. Failures exit non-zero with the message on stderr.

### Known limitations

- `edit` cannot clear a field back to `null`, or empty an author list — flags are
  only applied when passed, and there's no "unset" sentinel. Workaround is `rm` +
  `add`. Worth revisiting when something actually needs to retract a field.
  This is CLI-specific: the TUI's Edit (`:` -> `e`, Phase 16) has no such gap
  for any optional field or the author list — an empty input box clears it
  (`None`, or an empty `Vec`), since a text box naturally represents
  "nothing typed" where a CLI flag can't distinguish "not passed" from
  "passed as empty." Title is the one exception, in both places: it's
  `NOT NULL` in the schema, so an empty edit is rejected rather than
  clearing it.
- `edit` has no `--type`, so `entry_type` is fixed at creation.
- `cite_key` is not renameable; `update_entry` looks entries up by it. This is
  also why a second Zhou 2020 becomes `zhou2020b` and never `zhou2020a` — the
  unsuffixed key is already taken by the first, and it can't be rewritten. APA's
  own `2020a`/`2020b` convention needs *both* suffixed, so real disambiguation
  belongs at cite time, computed from the colliding set, not in the key.
- Tags export to BibTeX's `keywords` field and are read back from it, so they
  survive a round trip. `insert_entry` writes them alongside authors, which is
  what makes `import` pick them up.
- Two entries can't hold the same DOI (`reject_duplicate_doi`, checked on insert
  and update). Enforced in code rather than as a UNIQUE index on purpose: the
  database is hand-editable by design, and an index would refuse to build on an
  existing library that already contains duplicates, turning one stale row into
  a library where every command fails at open. Nothing detects the same paper
  entered twice under two different DOIs, or under none.
- Tag names are lowercased on the way in, so a tag can't carry display casing
  (`NLP` is stored and shown as `nlp`).
- Nothing lists all known tags, and a tag row orphaned by its last `untag` is
  left behind rather than garbage-collected. Both are worth fixing together, if
  a `ferref tags` command ever exists.
- There is no `detach`, and no way to edit an attachment's path. Since `attach`
  rejects a path that doesn't resolve, the usual cause (a typo) is caught up
  front; a moved file still needs `rm` + re-add.
- Attachment paths are absolute and stored at attach time. `attach` and `fetch`
  both copy into `./pdfs/`, so the files travel with the library — but the
  stored paths don't, and moving the library directory breaks every one of them
  silently. Nothing revalidates them.
- BibTeX export writes legacy BibTeX unless `--biblatex` is passed. That's a
  real fork, not a quality setting: BibLaTeX keeps `@online`/`@dataset` and
  writes `date`/`journaltitle`, which legacy BibTeX styles don't read. Tags
  round-trip through `keywords`; collections don't round-trip at all.
- `fetch` only reads `best_oa_location.url_for_pdf`. Plenty of genuinely open
  papers are linked only as landing pages, so "open access" and "fetchable PDF"
  are reported as separate facts. Scanning the other `oa_locations` was tried
  and reverted: on live DOIs the extra candidates were landing pages too, so it
  turned a clean "no PDF available" into a download failure.
- The Unpaywall contact email is never compiled in. It comes from `--email`,
  `FERREF_EMAIL`, or the config file, and is sent to Unpaywall and nowhere else.
- SSRF protection resolves the host and then lets `ureq` resolve it again to
  connect, so a DNS record that changes between the two (rebinding) can still
  get through. Closing that needs a resolver we control, i.e. a dependency.
- Fetched PDFs land in `./pdfs/<cite_key>.pdf`. `sanitize_filename` is
  many-to-one, so colliding keys get `-2`, `-3` suffixes rather than sharing a
  file; an existing file is only reused when the same entry already has it.
- Extraction is PDF-only and requires `pdftotext` (poppler-utils) on `PATH`.
- Extracted text is capped at 10MB per attachment, with a truncation marker.
- `full_text` is omitted from `list`/`search` unless `--full-text` is passed, so
  the common case doesn't read the whole library's text into memory. `--full-text`
  requires `--json`, because the plain-text listing has no column for it: without
  that guard the flag read 270MB and printed none of it.
- `list_entries` loads authors, tags and attachments in three bulk queries, not
  three per entry. The per-entry form was measured 9.4x slower on a 5,000-entry
  library — most of `ferref list`'s runtime.
- The TUI's tree counts are **recursive**; `collection ls` counts **directly**.
  They differ on purpose: selecting a tree row filters recursively, so a direct
  count beside it made the pane disagree with itself.
- **Superseded (Phase 14):** the DB and `pdfs/` used to live at `./ferref.db` and
  `./pdfs/`, relative to the current directory — a library was a folder you `cd`
  into, like a git repo. That meant `ferref` behaved differently depending on
  where it was invoked from, and every project got its own accidental library.
  `config::library_root()` now resolves one fixed location (`FERREF_HOME` env
  var, else `~/.ferref`), so `ferref` is a single library reachable from any
  directory — unlike a git repo, there's nothing to `cd` into.
- `delete_entry` (and merge's fold-then-delete of the dropped entry) removes
  the DB rows but never the attachment files in `./pdfs/` — they're orphaned
  on disk, not cleaned up. True since `rm` shipped in Phase 1; Phase 16 makes
  it easier to hit casually, since `:d` in the TUI is one confirm away rather
  than a deliberate CLI invocation. `ferref doctor` (Phase 17) checks the
  opposite direction — a DB row pointing at a file that's gone — not this
  one; it's the natural place to eventually report these too, once it also
  scans `./pdfs/` for files with no matching row.

BibTeX round trips (Phase 3) lose two things, both inherent to the target format
rather than fixable in our mapping:

- **Non-legacy `entry_type`s collapse to `misc` on export** — *unless you pass
  `--biblatex`.* Legacy BibTeX has no `@online`/`@dataset`/`@software`, nor any
  custom type like `@preprint`, so `to_bibtex_string()` downgrades them.
  `to_biblatex_string()` preserves them but emits `journaltitle`/`date` instead
  of `journal`/`year`, which plain BibTeX styles can't read — a worse loss for
  the default audience. This section originally called for "an `export --format
  bibtex|biblatex` flag, not a change of default"; that is what shipped, as a
  boolean `--biblatex`.
- **Newlines inside a field collapse to single spaces** on re-import. This is
  standard BibTeX field-content normalization, not specific to our parser. A
  multi-paragraph abstract survives as one paragraph.

## Target v1

- CLI subcommands (`clap`), every data-printing command with `--json`
- Real CRUD against SQLite
- BibTeX import/export
- Search/filter (author, year, title)
- Tags/collections
- File attachments + extracted full text (the AI-native payoff)
- DOI lookup: metadata autofill + open-access full-text fetch
- Citation formatting (APA, MLA) — last, purely cosmetic *(later removed; BibTeX export supersedes it)*

Each phase below is independently shippable and testable before starting the next.

---

## Phase 1 — Real CRUD

**Goal:** `db.rs` becomes actually usable; `main.rs` persists instead of printing.

`src/db.rs`, new functions:
- `insert_entry(conn: &Connection, entry: &Entry) -> Result<i64>` — inserts the entry row + its authors in one `Connection::transaction()`, returns the new `id`.
- `get_entry(conn: &Connection, cite_key: &str) -> Result<Option<Entry>>` — join entries+authors, ordered by `author_order`.
- `list_entries(conn: &Connection) -> Result<Vec<Entry>>`
- `update_entry(conn: &Connection, entry: &Entry) -> Result<()>` — update entry row, delete+reinsert authors (simplest correct approach — author lists are small).
- `delete_entry(conn: &Connection, cite_key: &str) -> Result<()>`

`src/models.rs`: add `impl TryFrom<&rusqlite::Row<'_>> for Entry` (or a private `from_row` fn in `db.rs` — no need for a trait if it's only used in one place).

Wire `main.rs`'s demo entry through `insert_entry` + `get_entry` to prove round-trip.

**Test:** one `#[test]` that opens an in-memory DB (`Connection::open_in_memory`), inserts an entry with 2 authors, reads it back, asserts equality.

---

## Phase 2 — CLI (clap), JSON output baked in

Add `clap = { version = "4", features = ["derive"] }` and `serde`/`serde_json` — `#[derive(Serialize)]` on `Entry`/`Author` from the start.

New `src/cli.rs`: a `#[derive(Parser)]` enum of subcommands. `main.rs` becomes a thin dispatcher.

Subcommands:
```
ferref add --type article --key smith2024 --title "..." --author "Smith, John" [--year 2024] [--journal Nature] ...
ferref list [--json]
ferref show <cite_key> [--json]
ferref edit <cite_key> --field value ...
ferref rm <cite_key>
```

`--author` repeatable (`Vec<String>`), parsed as `"Last, First"`.

Every command that prints entries takes `--json` and dumps `serde_json::to_string_pretty`
instead of the human-readable table — this is the hook external scripts/pipelines use
for the rest of ferref's life, so it's a Phase 2 concern, not a later retrofit.

---

## Phase 3 — BibTeX import/export

Add `biblatex` (parses/writes `.bib`, handles entry types and fields already — don't hand-roll a BibTeX parser).

New `src/bibtex.rs`:
- `import(path: &Path) -> Result<Vec<Entry>>`
- `export(entries: &[Entry]) -> String`

CLI:
```
ferref import refs.bib
ferref export --out refs.bib
```

---

## Phase 4 — Search/filter

Extend `list_entries` with an optional filter struct (author substring, year range, title substring), built as SQL `WHERE` + `LIKE`.

> ponytail: plain `LIKE` queries, not FTS5 — add an FTS5 virtual table only if search actually feels slow or `LIKE` proves too dumb (no substring-in-multiple-fields ranking, etc.).

CLI:
```
ferref search --author smith --year 2024
```

---

## Phase 5 — Tags / collections

Schema additions in `init_db`:
```sql
CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS entry_tags (
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (entry_id, tag_id)
);
```

CLI:
```
ferref tag <cite_key> <tag_name>
ferref untag <cite_key> <tag_name>
ferref list --tag <tag_name>
```

---

## Phase 6 — File attachments

Schema:
```sql
CREATE TABLE IF NOT EXISTS attachments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    date_added INTEGER NOT NULL
);
```

Store the path, not the file — copying/managing a file store is a separate feature, out of scope for v1. **Superseded:** `attach` now copies into `./pdfs/` under the same `<cite_key>.<ext>` scheme `fetch` uses, so a library is one directory of papers. The DB still stores only a path; it's the path of the copy.

CLI:
```
ferref attach <cite_key> <path>
ferref open <cite_key>   # spawns `xdg-open`/`open` via std::process::Command — no new dep
```

---

## Phase 7 — Full-text extraction (the AI-native payoff)

This is the phase the whole vision hinges on: get raw text out of attachments and
into the DB so external tools (embeddings, bibliometrics, LLM feeds) have something
to consume without reimplementing PDF parsing themselves.

Schema: add `full_text TEXT` to `attachments` (nullable — extraction may fail or be skipped).

`src/text.rs`: `extract_text(path: &Path) -> Result<String>`.

> ponytail: shell out to `pdftotext` (poppler-utils, `std::process::Command`) rather than
> pulling in a Rust PDF-parsing crate — one system dependency vs. a much larger crate
> and its transitive deps, for a solved problem. Swap to a Rust crate later only if
> shelling out proves unreliable or poppler can't be assumed present.

CLI:
```
ferref attach <cite_key> <path> --extract   # extracts immediately
ferref extract <cite_key>                    # (re)extract for an existing attachment
```

`ferref show --json` includes `full_text` — this is the field an embedding script
or LLM pipeline actually wants.

---

## Phase 8 — DOI lookup: metadata + open-access full text

Two separate API calls, worth keeping conceptually distinct:

1. **Metadata autofill** — Crossref (`https://api.crossref.org/works/{doi}`, no key needed) resolves any registered DOI to title/authors/year/journal/volume/pages. Works for essentially every DOI, paywalled or not.
2. **Open-access full text** — Unpaywall (`https://api.unpaywall.org/v2/{doi}?email=...`, free, requires a contact email per their polite-pool policy) returns a PDF URL *only if* a legal OA copy exists. When it doesn't, ferref reports that and stops — no scraping, no paywall bypassing.

Add `ureq` (sync HTTP client — no async runtime needed for a handful of blocking requests in a CLI tool; skip `reqwest`/`tokio`).

New `src/doi.rs`:
- `fetch_metadata(doi: &str) -> Result<Entry>` — parses Crossref JSON into an `Entry`.
- `fetch_oa_pdf_url(doi: &str, email: &str) -> Result<Option<String>>` — parses Unpaywall's `best_oa_location.url_for_pdf`.
- `fetch_and_attach(conn, cite_key, doi, email) -> Result<()>` — downloads the PDF if one was found, saves it, inserts an `attachments` row, runs it through Phase 7's `extract_text`.

CLI:
```
ferref add --doi 10.1234/xyz.5678         # fetches metadata, creates the entry
ferref fetch <cite_key>                    # looks up OA full text for an existing entry's DOI, attaches + extracts if found
```

Email for the Unpaywall polite pool: a `--email` flag or a one-time config value (e.g. `~/.config/ferref/config.toml`) — a config file is the first genuinely new piece of state ferref needs; don't build a general settings system for it, just the one key.

> ponytail: Unpaywall is the one well-known free/legal OA-discovery API — don't build a fallback chain across multiple providers unless Unpaywall's coverage proves inadequate in practice.

---

## Phase 9 — Citation formatting *(removed)*

**Removed after Phase 12.** `cite` was always the most cosmetic thing here, and
the BibTeX export makes it redundant: `ferref export` → biblatex → LaTeX is the
natural pipeline, and biblatex does the job better, because it disambiguates
`2020a`/`2020b` across the whole bibliography while `cite` only ever saw one
entry at a time. Deleting it removed 260 lines and one subcommand, and closed a
direction — arbitrary citation styles — that was already an explicit non-goal.
What it looked like:


New `src/cite.rs`: `format_apa(&Entry) -> String`, `format_mla(&Entry) -> String` — plain string templates over the fields already on `Entry`. Purely cosmetic, last on purpose — doesn't feed the AI-native use case at all.

> ponytail: skip a full CSL engine (e.g. `hayagriva`) for two fixed styles — add it later only if more styles or edge-case correctness (et al. rules, ordinals, etc.) are actually needed.

CLI:
```
ferref cite <cite_key> --style apa
```

---

## Phase 10 — Collections

Phase 5 delivered flat tags, not the nested collections listed as an essential
feature. This is the gap. Tags stay as they are — a paper can carry many, they
describe it. A collection is *where a paper lives*, it nests, and it's what the
TUI's tree pane reads.

```sql
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
```

Collections are addressed by slash-separated path (`Physics/Entropy`), since
names are unique among siblings. A `UNIQUE (parent_id, name)` constraint will
*not* enforce that at the root: SQLite treats NULLs as distinct, so two
top-level collections could share a name. Uniqueness is checked in code.

CLI:
```
ferref collection new <path> ferref collection ls [--json]
ferref collection add <path> <cite_key>   ferref collection rm <path> <cite_key>
ferref collection mv <path> --parent <path>|--root
ferref collection delete <path>
ferref list --collection <path> [--recursive]
```

> The tree reader must terminate on a cyclic `parent_id` graph. The database is
> a plain SQLite file anyone can edit by hand — that is the project's core
> premise — so a cycle is reachable no matter how careful `mv` is, and a naive
> recursive walk would hang the TUI's render loop.

---

## Phase 11 — TUI

Three panes, Zotero's arrangement: collections tree on the left, entry table in
the middle, details for the selected entry on the right. Read-only over the same
SQLite file the CLI writes.

Add `ratatui` + `crossterm`. DESIGN.md has said "TUI later" from the start; a
terminal UI is not something to hand-roll over raw ANSI escapes, and ratatui is
the standard choice with crossterm as its backend. No async runtime.

```
┌─────────────┬────────────────────────────────┬──────────────┐
│ COLLECTIONS │ Title        Authors  Year Jrnl │ DETAILS      │
│ ▾ All (12)  │ Array progr… Harris  2020 Natu │ Jaynes, E.T. │
│   ▾ Physics │ Information… Jaynes  1957 Phys │ Phys Rev 106 │
│     Entropy │ A Mathemati… Shannon 1948 Bell │ #entropy     │
└─────────────┴────────────────────────────────┴──────────────┘
```

Read-only in this phase: no editing, no deletion. The CLI remains the way to
change anything, so the TUI can't corrupt a library through a mis-keypress.
(Phase 12 opens exactly two of those doors — filing papers and creating
collections — and no others.)

---

## Phase 12 — TUI: sorting, search, and collection editing

Phase 11's TUI reads. This one lets it write, in the three places a reference
manager is actually used: finding a paper, ordering a list, and filing papers
into collections. Everything else stays CLI-only.

- **Sorting** — `s` cycles the sort column (title / first author / year /
  journal), `S` reverses. In memory over the already-loaded entries; the sort
  is not persisted, and no SQL changes.
- **Search box** — `/` opens a one-line input; typing filters the entry table
  live on a case-insensitive substring across title, authors, journal, year,
  cite_key, and tags. `Esc` clears it. In memory again — the collection's
  entries are already loaded, and an instant filter beats a round trip.
- **New collections** — `n` in the collections pane prompts for a name and
  creates a child of the selected collection; with "All Papers" selected that
  means a new root collection. One key covers "New Collection" and "New
  Subcollection" because the selection already says which is meant.
- **Assigning papers** — `c` in the entry table opens a collection picker over
  the same tree; `Enter` toggles the selected paper's membership, showing
  `[x]`/`[ ]` per collection. Toggle, not add-only: unfiling is the same
  gesture as filing.
- **Vim keys** — `j`/`k` move, `g`/`G` jump to top/bottom, `Ctrl-d`/`Ctrl-u`
  half-page, `h`/`l` fold and unfold in the tree and move between panes
  elsewhere. Arrows and `Tab` keep working.
- **`o`** opens the selected paper's attachments through the same system opener
  `ferref open` uses, with the child's output discarded so it can't scribble
  over the screen.

Sorting and filtering mean the table's row index is no longer an index into
`entries`. A `view: Vec<usize>` of indices into `entries` is the whole change:
filter, then sort, then index through it.

Writes go through the existing `db` functions, and by **id** rather than path —
the tree already holds ids, and a collection whose name contains `/` has no
addressable path (a known limitation from Phase 10). The path-based
`add_to_collection`/`create_collection` grow id-based cores that the existing
path-based functions then call, so there's one implementation of each write.

Still not in the TUI: editing entry fields, deleting entries or collections,
tagging, renaming. Those stay CLI-only. The line is that the TUI can file and
find papers, but cannot destroy data.

**Fixed by the Phase 15-era audit pass**: `load_entries` was calling
`attachment_text_lengths` (one query per entry) in a loop — the exact N+1
shape `list_entries`'s own bulk queries exist to avoid, reintroduced here
and run on every pane load *and* every `j`/`k` in the collections pane.
Replaced with `all_attachment_text_lengths`, one query for every entry's
attachment lengths at once, grouped in Rust. While there: `AttachmentLengths`
was storing a path string per attachment that nothing ever read (`details_text`
already gets the path from `Entry.attachments`, indexing the lengths list
positionally) — simplified from `HashMap<i64, Vec<(String, Option<i64>)>>`
to `HashMap<i64, Vec<Option<i64>>>`. Verified with a real `tmux` TUI session,
not just the type-checker.

---

## Phase 13 — `add --from-url`

`fetch` asks Unpaywall, which answers a question about a paper's *licence*.
That is the right question for open-access work and the wrong one for a paper
your institution subscribes to: Unpaywall says "not open" and it is correct,
while your browser on the campus VPN downloads it without complaint. The gap
isn't legal, it's architectural — ferref asks a third party about the paper,
where Zotero's browser connector is already inside an authenticated session
looking at the page.

`ferref add --from-url <landing page URL>` closes it, as a peer of `--doi`:

- Fetches the page through the existing `fetch_guarded`, so it inherits the
  redirect revalidation and internal-address refusal, and (via `ureq`) honours
  `HTTPS_PROXY`/`ALL_PROXY` — which is what makes an institutional proxy work.
- Reads the **Highwire Press `citation_*` meta tags**. This is the one piece of
  publisher-agnostic structure worth relying on: Google Scholar indexing depends
  on them, so essentially every journal emits them. It is emphatically *not* the
  translator-per-publisher approach — there is one parser, and a site that
  doesn't emit the tags simply isn't supported.
- If the page advertises `citation_doi`, metadata comes from **Crossref**, reusing
  the whole `--doi` path. Publisher pages lie and abbreviate; Crossref is
  authoritative. The page tags are the fallback, not the preference.
- If the page advertises `citation_pdf_url`, the PDF is downloaded **from the same
  network position** and attached and extracted. That is the entire point: the
  bytes arrive because the requesting IP is entitled to them, exactly as they do
  in a browser.

This is not paywall circumvention and the non-goal below is unchanged. ferref
still never bypasses an access control — it makes an ordinary request and takes
what the server chooses to return. Off the VPN the same command yields metadata
and, usually, a login page, which `download_pdf`'s `%PDF` magic-byte check
already rejects rather than saving.

Landing the PDF (claim a name under `./pdfs/`, write, attach, clean up on
failure) is shared with `fetch` rather than written twice — `fetch`'s copy also
had the exists-then-write race that `attach` was fixed for.

No new dependencies. Meta-tag scanning is a small hand-rolled tokenizer over the
raw HTML, in the same spirit as `strip_jats_tags`: crude, documented as crude,
and tested. An HTML parser would be a dependency bought to read six attributes.

It does have to **track quotes**, which the obvious version (find `<meta`, slice
to the next `>`, substring-search for `content=`) does not. All three of that
version's failures were real and are locked down by tests: a decoy attribute
whose *value* contained ` content=` was read as the content attribute, letting
the page choose which PDF got downloaded; a `>` inside a quoted value truncated
the tag and silently dropped it; and one unclosed `<meta` swallowed every
following tag. A `<` where an attribute name or quoted value should be means the
tag was never closed, so it is abandoned rather than merged with the next one.

Relative `citation_pdf_url`s resolve against the URL the page **actually came
from**, not the one that was typed — `fetch_guarded` returns its final URL for
exactly this. A DOI resolver, a `www` redirect, or an SSO proxy all land
somewhere else, and that is the case this phase exists to serve.

**Fixed by the Phase 15-era audit pass**: `resolve_location`'s relative-path
branch resolved against the host root, not the base URL's actual directory —
a `citation_pdf_url` of `12345.pdf` on a landing page at
`.../articles/9` became `.../12345.pdf` instead of the RFC 3986 §5.3-correct
`.../articles/12345.pdf`. Silent failure mode: a publisher emitting a
genuinely relative PDF URL (rather than absolute, which is more common but
not universal) would 404 and the entry would land with no attachment, no
error. The two existing tests baked the wrong behavior in as the expected
result, so nothing caught it. Fixed to merge against the base path's
directory instead of the authority root; both tests updated, plus a new
case for a base URL with no path at all.

---

## Phase 14 — Fixed library location + `install.sh`

Before this phase, `ferref` opened `./ferref.db` and wrote `./pdfs/` relative to
the current directory (see the superseded note in **Current state** above). That
made ferref behave like a git repo — one library per folder you `cd` into — which
was fine for development but wrong for daily use: the point of a reference
manager is one library reachable from every project, not a fresh accidental
database wherever the command happens to run.

`config::library_root()` resolves a single fixed directory:

- `FERREF_HOME` env var if set and non-empty, else `~/.ferref`.
- `main()` creates it (`create_dir_all`) before opening the DB; `ferref.db` and
  `pdfs/` both live directly under it.
- No new dependency, no config-file key — this is the same env-var-then-`$HOME`
  precedence `config.rs` already uses for the Unpaywall email, one function over.

`install.sh` (repo root, not part of the crate) does the one-time setup:

- `cargo build --release`.
- Asks where the library should live (default `~/.ferref`); if the user picks
  somewhere else, writes `export FERREF_HOME=<path>` into their shell rc.
- Copies the built binary into `~/.local/bin` (creating it if needed) and adds
  that directory to `PATH` in the shell rc if it isn't already there — the
  standard place for a user-local binary on a Linux desktop, so `ferref` works
  without `sudo` or touching `/usr/local`. A copy, not a symlink, so the
  installed binary keeps working if the repo checkout moves or is deleted;
  the tradeoff is re-running `install.sh` after every `cargo build --release`
  you want to pick up.
- Re-running it is safe: it overwrites the copy and only appends a `PATH`/
  `FERREF_HOME` line if grep doesn't already find one.

Not delegated — see the delegation table. Not reviewed — the only trust boundary
touched is "where does this process read `$HOME` from," which was already true
of `config.rs`.

**Fixed by the Phase 15-era audit pass**, since `install.sh` doesn't get its
own `cargo test`: it was cwd-dependent (`cargo build`/`cp` are relative
paths, so `bash ~/Code/ferref/install.sh` from another directory built
whatever crate happened to be under `$PWD`), and didn't expand `~` in a
custom library path (`read` doesn't do that — answering the prompt with
`~/refs` wrote a literal, unexpanded `export FERREF_HOME="~/refs"` into the
shell rc, so every future `ferref` invocation would treat `~/refs` as a
path relative to wherever it happened to run — precisely the per-directory
accidental library this phase exists to abolish). Both fixed and verified
in a fake-`$HOME` sandbox: running from an unrelated directory, and
answering the prompt with a literal `~/...` path and confirming the rc file
holds the real expanded path.

---

## Phase 15 — `search --text`: FTS5-trigram content search with snippets

**Built and shipped**, after a genuine false start that's worth recording
because the mistake is easy to repeat: the trigram index existing and being
*correct* was verified early and thoroughly (schema, triggers, mid-word
substring matching, sub-trigram fallback — all confirmed empirically before
shipping). What wasn't verified until an adversarial review forced it was
whether the *actual query shape* — the clause as it's embedded in
`list_entries`'s per-row filter architecture, not the mechanism in isolation
— ever reached the index at all. It didn't. See "What actually shipped"
below for the real design; the rest of this section is preserved as the
original (wrong) plan, because the gap between them is the lesson.

Today `--full-text` (on both `list` and `search`) is a projection flag, not a
filter — it decides whether extracted PDF text rides along in the `--json`
output, but there is no way to ask "which entries' attachments *contain*
this string." `Filter` (`db.rs:844`) has no field for it, and `list_entries`
never touches `attachments.full_text` except to project it. This phase adds
that missing filter, plus enough of a result shape to make it useful without
dumping a whole PDF's text at you.

A plain `LIKE '%…%'` clause was the first draft, and works, but doesn't
scale: a leading wildcard can't use a B-tree index, so it's a full scan of
every extracted PDF's text, every query, cost growing with total corpus size
rather than match count. Verified against the actual vendored SQLite
(`libsqlite3-sys` 0.30.1 bundles SQLite 3.46.0): the `bundled` feature we
already depend on compiles with `-DSQLITE_ENABLE_FTS5` unconditionally
(`build.rs`), and that amalgamation includes the **trigram tokenizer**
(`fts5TriCreate`/`fts5TriTokenize` in `sqlite3.c`), which indexes every
3-character sequence — an inverted index that still matches arbitrary
mid-word substrings, the same thing `LIKE '%…%'` gives you, just indexed. No
new dependency, no new Cargo feature: it's already compiled into the binary
we already ship.

**Schema.** An FTS5 external-content table mirroring `attachments.full_text`,
so the text itself isn't stored twice — only the trigram index structures
are new, and they still live inside `ferref.db`, so the "one file" property
(`DESIGN.md`'s database principles) holds:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS attachments_fts USING fts5(
    full_text, content='attachments', content_rowid='id', tokenize='trigram'
);
```

Kept in sync with triggers on `attachments` (the standard external-content
pattern — insert/delete/update all go through `attachments_fts`'s special
`('delete', rowid, content)` protocol on the old row before any insert of the
new one). Migration follows the existing `full_text` column migration's shape
(`db.rs:85-95`): guarded by checking whether `attachments_fts` already exists
before creating it, plus a one-time backfill —
`INSERT INTO attachments_fts(rowid, full_text) SELECT id, full_text FROM
attachments WHERE full_text IS NOT NULL` — for libraries that extracted text
before this phase shipped.

**Querying.** Verified in `sqlite3.c`: FTS5's `xBestIndex` recognizes a
`LIKE`/`GLOB` constraint applied directly to an FTS5 table's column, and
when the tokenizer reports `ePattern == FTS5_PATTERN_LIKE` (trigram's
default, case-insensitive), it serves that constraint from the trigram
index — so the query is still a `LIKE` on a table, not `MATCH` query syntax
to learn or escape. The `Filter.text` clause ends up close to the shape
every other clause already has, just querying the FTS5 table instead of
`attachments` directly:

```sql
EXISTS (SELECT 1 FROM attachments att, attachments_fts fts
        WHERE fts.rowid = att.id AND att.entry_id = entries.id
        AND fts.full_text LIKE ? ESCAPE '\')
```

Same `like_pattern` escaping already used everywhere else; ANDs with
`--author`, `--title`, `--tag`, `--collection`, etc. like every other
`search` filter. Exposed as `--text <QUERY>` on `search` only. One thing the
implementation has to nail down empirically, not assumed from reading the
source: whether a pattern whose fixed substring is under 3 characters (too
short to form a trigram) still returns correct — if unindexed — results, or
errors. Either is fine; which one it is needs a test, not a guess.

**Snippets.** SQL only tells you an entry matched, not where. Once
`list_entries` returns the (now much smaller) matching set, a second pass
loads full text for just those entries' attachments and locates the matches
in Rust: a pure function,

```rust
fn find_snippets(text: &str, query: &str, context_chars: usize, max_matches: usize) -> Vec<String>
```

ASCII-case-insensitive substring search (matching the SQL side), a fixed
character window on each side of a hit, runs of whitespace collapsed to one
space (PDF extraction leaves ugly line-wrapping), `…` at a window edge that
was truncated mid-string, and capped at `max_matches` hits with a "+N more"
count rather than printing every occurrence of a common word. Pure and
DB-free, so it gets a real `#[test]`: overlapping matches, a hit at the very
start/end of the text, more matches than the cap, no match (shouldn't be
reachable given SQL already filtered, but the function must not panic on it).

**Output.** Plain text: cite_key/title header per matching entry, then the
capped snippets indented underneath, one per line. `--json`: a dedicated
small struct (`cite_key`, `title`, and a `matches: [{path, snippet, count}]`
per attachment) — not the shared `Entry` type, so `list`/`show`/export are
untouched and this doesn't force a full unbounded-text dump the way reusing
`Entry` with `with_full_text` would.

**Why not the flat-files-plus-`grep`/`ripgrep` alternative.** Considered and
rejected. `pdftotext`-extracted text today lives in exactly one place — the
`attachments.full_text` column, never written to disk as `.txt` — and
DESIGN.md's whole premise for the database is "it's just one file, `sqlite3
ferref.db` gets you everything." Shelling out to `ripgrep` over flat files
would genuinely be fast, but means text exists in two places, and loses the
ability to combine a text query with SQL filters (`--tag`, `--collection`,
`--year`) in one query. FTS5-trigram gets `grep`-speed search without either
cost — it's still one table inside `ferref.db`.

**Stress test, before calling this done.** Generate a synthetic library (N
entries, realistic-sized full_text each — zhou2020's real 35,163 chars is the
reference point) at a few sizes (500 / 5,000 / 50,000 "papers") and time
`search --text` before vs. after the index exists, to confirm the scaling
claim rather than assume it from reading tokenizer source.

Delegation: **yes / yes**. Bigger than Phase 4's shape now — a real schema
migration (virtual table, triggers, backfill) with a genuine silent-failure
trap (a trigger that doesn't fire, or a backfill that misses rows, means
`search --text` quietly stops finding things that are actually there, not a
loud error) — same class of risk the delegation table already flags trigger-
sync and migration correctness for elsewhere. Review earns its keep here.

**What actually shipped, after review caught the plan above being wrong.**

Everything above this point — schema, triggers, backfill guard, snippet
extraction, output shape, delegation call — was built by the `coder`
subagent as scoped and is unchanged; all of it independently re-verified
(89 tests, clean build, real CLI smoke tests including trigger sync via raw
`sqlite3` UPDATEs and via entry deletion cascades). What was wrong was the
`Filter.text` clause's query shape, and it was wrong in a way that plain
correctness testing — including a genuine mid-word-substring test and a
sub-3-character test, both passing — could not have caught, because those
tests only check *what* rows come back, never *how fast* or *by what plan*.

The adversarial review's key finding, independently reproduced against the
real schema via `EXPLAIN QUERY PLAN`: the clause above is a **correlated**
subquery (`... AND att.entry_id = entries.id ...`, evaluated once per row of
the outer `entries` scan — the same shape every other `Filter` clause uses,
by design, so they all AND together correctly). FTS5's `xBestIndex` only
recognizes a `LIKE` constraint when it can scan `attachments_fts` as the
*driving* table of a query. Under correlation, it never gets that chance —
SQLite instead re-scans the whole FTS5 structure, unindexed, once per outer
row. Measured: **5x slower than the original plain-`LIKE`-on-`attachments`
draft this phase set out to replace.** The index wasn't just unused, it was
actively worse than not having it.

A second, independent finding, also verified directly: adding
`ESCAPE '\'` to that `LIKE` — required for correctness, since every other
substring filter in this codebase needs it to treat a user's literal `%`/`_`
as literal — **silently disables the trigram fast path entirely**, confirmed
via `EXPLAIN QUERY PLAN` losing its index annotation the moment `ESCAPE` is
added, even outside the correlation problem. So `LIKE` was never going to
work here, correlated or not, once real user input needed real escaping.

The fix, verified via `EXPLAIN QUERY PLAN` against the real schema and via
direct SQL benchmarking before being written into `db.rs`:

```sql
entries.id IN (
    SELECT att.entry_id FROM attachments att, attachments_fts fts
    WHERE fts.rowid = att.id AND fts.full_text MATCH ?
)
```

Non-correlated (`IN` against a subquery with no reference to the outer
`entries` row), so SQLite evaluates it exactly once and — verified — plans
it as `LIST SUBQUERY` with `SCAN fts VIRTUAL TABLE INDEX 0:M0`, the genuine
trigram-accelerated match path. `MATCH` instead of `LIKE`, with the query
string quoted as an FTS5 phrase (`fts5_phrase`: wrap in `"`, double any
literal `"`) rather than `like_pattern`-escaped — `MATCH` has no `%`/`_`/`\`
special characters to escape in the first place, and phrase (adjacency)
matching against a trigram-tokenized column is exactly substring matching.

`MATCH` has its own gap `LIKE` didn't: verified directly that a phrase under
3 characters (too short to form one trigram) matches **nothing**, even when
the substring is genuinely present — not an error, not a graceful unindexed
fallback, just silently wrong. So the sub-3-character case (still a real
case — "AI", "ML" are plausible searches) falls back to a plain, correlated
`LIKE ... ESCAPE '\'` against `attachments.full_text` directly, skipping
`attachments_fts` entirely since it can't help there regardless of query
shape. That path was already correct in the original draft; it just needed
to stop being the *only* path.

Real numbers, 20,000 synthetic attachments (~700MB total text, 35KB each —
zhou2020's real extraction as the sizing reference), realistic vocabulary
(the system dictionary, ~75,000 words, not a small repeated word list —
which matters, see the process note below): a genuinely rare search term
went from ~160ms (plain scan) to ~0.1ms (fixed query) — **~1,500x**. A term
built from common English fragments still saw **~18x**. The corrected
query's plan was independently confirmed against the actual compiled
binary's schema, not just a scratch reproduction.

**Second-order finding, worth keeping for next time this kind of question
comes up:** the first attempt to measure this (before the correlation bug
was even known) used a synthetic corpus built from a 31-word repeated
vocabulary and measured **zero speedup** — which looked like it disproved
the whole premise. It didn't; that corpus had artificially low trigram
diversity (a 31-word vocabulary repeats the same few hundred trigrams
constantly, so posting lists for any of them are enormous and unselective).
Switching to a genuine ~75,000-word dictionary reproduced the expected
speedup immediately. Two false readings in one phase, in opposite
directions — one test too unrealistic to show a real effect, one query
shape too naive to get a real effect — is the actual reason this section is
being kept this detailed: "we benchmarked it and it was fast" is not a
claim that survives contact with either a bad synthetic corpus or an
untested query shape. Only a benchmark against the literal shipped query,
on realistic data, counts.

Two regression tests added beyond the original ten: a literal `"` in the
search text (proves `fts5_phrase`'s escaping, and exercises the `MATCH`
path specifically), and a three-entry scoping test (proves the
non-correlated rewrite still scopes each match to its own entry, not just
"some entry"). Also fixed in the same pass: `search --text ""` previously
returned nothing with no error (SQL's `LIKE '%%'` matches everything, but
`find_snippets` on an empty query always returns zero matches, so every
entry was silently dropped by the "0 Rust-side matches" defensive skip) —
now rejected up front with a clear error instead of a silent empty result.

**A second audit pass, after all of the above shipped, found one more real
bug and a genuine efficiency regression, both fixed here too.**

The backfill's `INSERT ... SELECT ... WHERE full_text IS NOT NULL` — meant
to index attachments that predate `attachments_fts` — silently excluded
every attachment that had never been `--extract`ed (the default state for
a newly attached PDF), while the sync triggers assume every row is indexed,
NULL included. On a pre-existing library, the first `extract` or `rm`
touching one of those un-backfilled rows issued an FTS5 `'delete'` command
for a rowid the index never actually held. Reproduced directly against the
project's real bundled SQLite (3.46.0): `rm` returned `database disk image
is malformed`, not a hypothetical. Worse, the two existing tests that
should have caught this couldn't have: both used `COUNT(*) FROM
attachments_fts` as a proxy for "is it indexed," and `COUNT(*)` on an
external-content FTS5 table reads the *content* table's row count
regardless of whether the index was ever populated — confirmed directly, a
completely empty index still reports the same count as a fully populated
one. Fixed by replacing the manual backfill with FTS5's own `'rebuild'`
command, which re-scans every content row unconditionally rather than a
hand-picked subset, and rewriting both tests to use a real `MATCH` query
and `PRAGMA integrity_check` instead of `COUNT(*)`. One more regression
test added specifically for the migration path (a pre-existing library,
built by hand rather than through `create_schema`, upgrading through it) —
none of the other fts tests could exercise this scenario, since they all
start from a fresh schema where every row goes through the AFTER INSERT
trigger from birth.

Separately: `search --text` was bulk-loading every matching entry's full
text into one `Vec<Entry>` up front (`list_entries(..., with_full_text:
true)`) before cutting a single snippet — on a large library, hundreds of
MB resident before any output, the same unbounded-memory shape
`--full-text` already needed a `--json` guard for, reintroduced here
without one. Fixed by loading one entry's attachments at a time
(`db::attachments_for_entry`, already existed, just needed to be made
`pub`) inside `text_search_results` instead.

---

## Phase 16 — TUI: editing, fetch, delete, and merge

Phase 12 drew a line — "the TUI can file and find papers, but cannot destroy
data" — as an implementation-time scoping call, not something the user ever
asked for. In practice it's pure friction: fixing a typo'd journal name or
adding a missing DOI by hand means leaving the TUI for another pane to run
`ferref edit`. This phase moves that line: the TUI gains editing, `fetch`,
delete, and merge, gated behind a `:`-command palette (vim-style) rather than
bare keystrokes, so the mutating/destructive actions are deliberate rather
than a stray keypress away. Sorting, search, and collection filing (`c`, `n`)
keep their existing bare-key bindings — only the four new operations below go
through `:`.

**The `:` palette.** Pressing `:` with focus on `Entries` or `Details` and a
row selected opens a small popup, scoped to the selected entry:

```
┌ shannon1948 ───────────┐
│ e  Edit field          │
│ f  Fetch PDF           │
│ m  Merge               │
│ d  Delete              │
└─────────────────────────┘
```

`Esc` closes it with no effect. A new `Mode::Command` variant, rendered the
same way `Mode::Picker`'s popup already is (`draw_picker`, `Clear` first so
panes underneath don't bleed through).

**Edit** (`e`) opens a picker of fields — title, year, journal, volume,
pages, doi, url, abstract, authors — each showing its current value.
Selecting one opens `Mode::Input` (reusing the existing single-line input the
search box and "new collection" prompt already use) pre-filled with that
field's current value; `Enter` saves via `db::update_entry` (`db.rs:1165`,
already exists, already used by `ferref edit`) and returns to the field
picker rather than to `Normal`, so several fields can be fixed in one visit.
`Esc` at the field picker returns to `Normal`. Authors edit as one
semicolon-separated "Last, First; Last, First" line replacing the whole
list — the same whole-list-replace semantics `ferref edit --author` already
has, not a new per-author sub-editor.

**Fetch** (`f`) reuses `cmd_fetch`'s logic (`main.rs:674`) — Unpaywall lookup
by the entry's DOI, download and land the PDF, extract text — which today
calls `die()` (`process::exit`) on any failure. That has to be pulled out
into a function returning `Result<FetchOutcome, String>` that both the CLI
handler and the TUI call, so a fetch failure in the TUI becomes a footer
error (`app.error`) instead of killing the whole session. No async runtime
(consistent with Phase 11's call): the network request blocks the event
loop for its duration. The UI must render a "Fetching…" footer state
*before* making the blocking call, not after, or the freeze looks like a
hang rather than progress.

**Delete** (`d`) opens a y/n confirm popup ("Delete '<title>' [<cite_key>]?
y/n") before calling `db::delete_entry` (`db.rs:1206`). Any other key than
`y` cancels. A new `Mode::Confirm { message, on_yes }`-shaped variant (exact
shape is an implementation detail — an enum of pending actions is simplest,
a boxed closure is more general and probably unneeded for four call sites).

**Merge** (`m`) is new at every layer — `db::merge_entries` doesn't exist yet
(it was a "roadmap, not yet scoped" bullet; this phase scopes and builds it),
and so does a CLI `ferref merge <keep> <drop>` that calls the same function,
matching the existing rule that a write has exactly one implementation
regardless of which front end triggers it. `merge_entries(conn, keep_id,
drop_id)`:

- Re-parents `drop_id`'s rows in `entry_tags` and `collection_entries` onto
  `keep_id` (`INSERT OR IGNORE`, since `drop` and `keep` may already share a
  tag or collection — both tables have the entry_id in their primary key).
- Re-parents `attachments` rows onto `keep_id`, handling the real trap the
  roadmap note flagged: attachments physically live at `./pdfs/<cite_key>.<ext>`
  (Phase 12), so a `drop`-side attachment's path collides with a `keep`-side
  file of the same extension. Needs the same claim-with-`O_EXCL` discipline
  Phase 12's `attach` race fix already established, not a bare
  `UPDATE ... SET entry_id`.
- Deletes the `drop` entry (`db::delete_entry`), cascading its now-empty
  `authors` row via the existing `ON DELETE CASCADE`.
- Whichever entry's own fields (title, year, doi, ...) `keep_id` had going in
  are untouched — merge folds relationships (tags, collections, attachments)
  into the survivor, it does not attempt a field-by-field union. If `drop`
  had a field `keep` is missing, that's what Edit is for afterward.

**Choosing the pair to merge, in the TUI.** `Space` (Entries pane, `Normal`
mode) toggles the current row into a new `marked: Vec<i64>` on `App` —
insertion-ordered, not a `HashSet`, because order is the whole UX: the first
entry marked is the one that survives, the second is the one that gets
folded in and deleted. Marked rows get a visible marker (a distinct cell
style, not just the existing selection highlight, since a mark must stay
visible after the cursor moves off the row) and the `ENTRIES` pane title
gains a `(N marked)` suffix whenever the set is non-empty, so the state is
never invisible. `Esc` (which already clears the search filter) clears
marks too.

`:` → `m` behavior depends on how many are marked:
- **Exactly 2** — those two are the pair; skip straight to the confirm popup
  ("Merge '<drop title>' into '<keep title>'? y/n"), naming both explicitly
  since order isn't visually obvious from the mark alone.
- **0 or 1** (the default — this is "default to 1" from the mark being
  optional) — the *selected* row is the keeper, and a new picker opens to
  choose the entry to fold in and delete: a bordered popup list, rendered
  the way `draw_picker` already renders the collection picker, but over
  `app.entries` instead of the collection tree, *and* with a live text
  filter — the collection picker is just `j`/`k` over a short tree with no
  filter box, which doesn't scale to a library with hundreds of entries.
  Reuse `matches_filter` (`tui.rs`, already does case-insensitive substring
  matching across title/authors/journal/year/cite_key/tags for `/`) to drive
  it, typing narrows the list, `j`/`k` moves, `Enter` picks. Then the same
  confirm popup.
- **3 or more** — footer error ("merge only supports two entries at a time"),
  no action. Chained/N-way merges are out of scope; do them one pair at a
  time.

Marks are cleared after a merge completes either way.

**Still out of scope.** Renaming a cite_key (Phase 10's own noted
limitation — a `/` in a collection name has no addressable path, and a
cite_key rename has the same shape of problem for attachment filenames that
merge's collision handling exists to solve; worth its own phase). Creating a
brand-new entry from inside the TUI (`add` stays CLI-only; the TUI's job is
managing what's already there). Tagging/untagging from the TUI (not asked
for here, and `Edit`'s field list doesn't cover tags since they're not an
`entries` column).

Delegation: **yes / yes**. Bigger than Phase 12's grind (five new modes worth
of key handling, a field-by-field editor, a confirm flow) plus two genuine
correctness traps in the same class Phase 12 and 15 were both burned by
before review caught them: the attachment-filename collision on merge
(silent overwrite is exactly the Phase 12 `attach` race bug, in a new
location) and a blocking network call inside a render loop that must not
leave the terminal in a broken state if it errors or panics mid-request.

**What review caught.** The attachment-collision handling itself (claim with
`O_EXCL`, fall back to `-2`/`-3`/...) was correct on the first pass — an
adversarial review confirmed it directly by forcing a real collision. What
wasn't correct: `merge_entries`'s attachment loop renamed each drop-side file
on the filesystem *and then* recorded the move in the (uncommitted) SQL
transaction, one attachment at a time. Renames aren't transactional — if a
later attachment in the same merge failed (its DB row pointing at a file
that had already gone missing, since the database is hand-editable by
design), the transaction rolled back cleanly, but the earlier attachment's
file had already been physically moved and stayed moved. Result, reproduced
directly: a `drop`-side attachment row left pointing at a path that no
longer existed, and an orphaned file sitting at the destination name with no
DB row pointing at it — `PRAGMA integrity_check` still reported `ok`, since
SQLite's own consistency was never in question, only DB-vs-filesystem
agreement. Fixed two ways: every drop-side attachment's source file is
checked to actually exist *before* any renames start, so the reported case
fails loud with zero side effects; and each successful rename is tracked and
unwound (renamed back) if a later one in the same merge fails, so a
same-scale problem the pre-flight check can't rule out (permissions, disk
full, mid-loop) can't leave a half-migrated merge either. A regression test
reproduces the exact missing-file case.

**Follow-on: two more bulk actions over the marked set.** After the phase
shipped, marking turned out to be useful for more than merge — the same
"these are the ones I mean" primitive now backs several more actions, all
reusing marks without introducing a new selection mechanism:

- `x` exports the marked entries (or, with nothing marked, just the
  selected one) as BibTeX to a path typed into an input box, via
  `App::export_bibtex` calling the same `bibtex::export` the CLI's own
  `ferref export` uses — legacy syntax only, no TUI equivalent of
  `--biblatex`. Unlike merge/delete, this doesn't clear `marked` afterward:
  export doesn't consume or change the entries, so the same set can still
  be filed into a collection right after.
- `c` (the existing single-entry "file into collection" key) becomes a bulk
  operation when entries are marked: every marked id gets added
  (`add_entry_to_collection`, idempotent) to whichever collection is
  picked, rather than toggling the one selected entry's membership.
  Deliberately add-only, not a toggle, since there's no single well-defined
  membership state for a mixed set — `Mode::Picker`'s `member` set starts
  empty in this mode and a row only flips to `[x]` once the set has
  actually been filed into it this session. With nothing marked, `c` is
  exactly what it always was.
- `:` gains `t`/`u` — tag/untag every id in the same marked-or-selected
  target set (`App::bulk_targets`, the small helper `x` and `t`/`u` both
  call), via the same `db::add_tag`/`remove_tag` the CLI's `tag`/`untag`
  already use. Both are idempotent server-side, so a mixed set (some
  already tagged, some not) is never an error. This is the one place the
  Phase 16 write-up's "tagging stays out of scope" line gets revisited —
  worth it once bulk actions over marks already existed for export and
  filing; single-entry tag management is still CLI-only.
- `?` replaces the Normal-mode footer's keymap line, which had grown a
  clause every time a feature landed here, with a full-screen reference
  (`draw_help`) grouped by Navigate / Find & sort / Entries / Collections /
  Other. Any key closes it. One real bug caught building it, worth keeping
  as a lesson: the popup's height was computed by counting *logical* rows
  (one per keybinding), not accounting for a long description wrapping
  inside the popup's fixed width — the `:` row's original text (listing
  every palette hotkey) wrapped to two lines on a real 120-column terminal,
  and the fixed-height box silently clipped the last entry off the bottom.
  Fixed by shortening that one line (it was redundant anyway — `:` shows
  its own hotkeys when opened) rather than making the height calculation
  wrap-aware; a `ponytail:`-style comment in `draw_help` names the
  remaining ceiling (a long line could still wrap at the popup's narrowest
  clamp, ~30 columns, reachable only on a terminal already at `MIN_WIDTH`
  where every pane is already cramped).

---

## Phase 17 — `ferref doctor`

Scoped and built directly off the Roadmap bullet this section replaces:
attachment paths are absolute and stored at attach time (`db.rs`'s known
limitations), so moving the library directory, or hand-editing a row to
point somewhere that never existed, leaves a dangling reference nothing
previously reported. `doctor` is a read-only scan: `db::all_attachment_paths`
(one query, `entries JOIN attachments`, not one query per entry — same
shape as `all_attachment_text_lengths`) pairs every attachment's cite_key
with its stored path; the CLI checks each with `Path::is_file` and reports
the ones that fail. `--json` gets `{"checked": N, "broken": [{"cite_key",
"path"}, ...]}`; plain text lists `cite_key: path` per broken row, or "All N
attachments resolve." if none. Exits 1 if anything's broken, 0 otherwise, so
it's usable as a health-check script (`ferref doctor || alert-someone`).

No `--fix` yet, per the original Roadmap note — re-pointing a path needs a
human to say what it should point to instead (there's no way to infer a
moved file's new location), and dropping the row outright is a data-loss
decision that shouldn't be a flag's default behavior. Report first, decide
later whether a fix mode earns its keep.

Note the direction: `doctor` catches a DB row pointing at a file that
isn't there. It does *not* catch the opposite — a file sitting in `./pdfs/`
that no DB row references any more, which is what `delete_entry` and
merge's dropped entry currently leave behind (see the known-limitations
entry on orphaned attachment files). Symmetric coverage (scanning `./pdfs/`
for files with no matching row) is a natural follow-on once this direction
has been useful for a while, not built here since it's a different query
shape (filesystem-driven instead of DB-driven) and a different set of
false-positive risks (a file mid-copy, a library sharing `./pdfs/` with
something else).

Delegation: **no / no**. One new DB query (a straightforward join, not a
migration) and a CLI command with no CLI framework precedent to get wrong —
smaller and lower-risk than Phase 10/5's shape, closer to Phase 4's "extends
one function, small enough to just write."

---

## Roadmap (not yet scoped)

Ideas worth doing sometime, deliberately not designed in detail yet — see
DESIGN.md's "read the phase's section before starting" rule; these don't have
one yet.

- **Semantic search via embeddings** — "papers related to multivariate
  information decomposition" instead of a literal substring. Embed each
  entry (title/abstract, or full text) and rank by vector similarity instead
  of matching characters. Flagged here specifically so it doesn't get lost,
  *and* because it directly contradicts the current explicit non-goal below
  ("no embeddings... ferref's job stops at handing clean, structured data to
  whatever does that work") — picking this up later means revisiting that
  line deliberately, not just building around it. Real open questions before
  it's even scoped: embed locally (which model, how big a dependency) vs.
  call out to something external (which reintroduces the "ferref talks to a
  third party" question Phase 8/13 were careful about); whether it's a new
  command (`ferref similar <cite_key>`) or a `search` mode; and whether
  storing vectors still fits "it's just one SQLite file" or needs its own
  store. Big lift, correctly not for now.

## Explicit non-goals for v1

- Doing the AI work ourselves — no embeddings, no bibliometric analysis, no LLM calls inside ferref. ferref's job stops at handing clean, structured, scriptable data to whatever does that work.
- Circumventing paywalls — full-text fetch is strictly limited to what Unpaywall reports as legally open access. No scraping, no Sci-Hub-style fallbacks.
- Full CSL styling engine / arbitrary citation styles
- Sync or multi-user access
- GUI (TUI is a real future goal, GUI is not)

## Order of work

Phase 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14 → 15 → 16 → 17. Phase 1 unblocks everything else — nothing downstream is useful until entries actually persist. Phases 7 and 8 (full text, DOI fetch) are pulled ahead of citation formatting because they're what actually serves the AI-native vision; APA/MLA formatting is cosmetic and can slip without cost.

---

## Delegation policy

Which phases get farmed out to a `coder` subagent, and which get an
`adversarial-reviewer` pass afterward. Consult this at the start of each phase.

| Phase | Coder | Review | Why |
|---|---|---|---|
| 1 — CRUD | yes | yes | *(done)* Schema mapping traps, transaction semantics. |
| 2 — CLI + `--json` | yes | yes | Big and mechanical. `--json` is a permanent API surface — get it wrong and every downstream script breaks. |
| 3 — BibTeX | yes | yes | New dep, parser edge cases, malformed `.bib` input. |
| 4 — Search/filter | no | no | Extends one function. Small enough to just write. |
| 5 — Tags | yes | no | Schema + 3 subcommands. Mechanical, failures are loud. |
| 6 — Attachments | no | no | Store a path, spawn `xdg-open`. Tiny. |
| 7 — Full text | yes | **yes** | Shells out to `pdftotext` with untrusted PDFs and arbitrary paths. Trust boundary. |
| 8 — DOI fetch | yes | **yes** | Network, remote JSON we don't control, a config file, partial-failure paths. Riskiest phase in the plan. |
| 9 — Citations | no | no | *(removed after Phase 12 — see the phase section.)* |
| 10 — Collections | yes | no | Schema + subcommands, like tags. One real trap (cycles), specified in the brief and tested. |
| 11 — TUI | yes | **yes** | New dep, terminal state that must be restored on panic, and a render loop that must not hang on malformed data. |
| 12 — TUI writes | yes | **yes** | Big mechanical grind (modes, key table, picker, sort/filter view), and the first phase where a keypress mutates the database. |
| 13 — `add --from-url` | no | **yes** | Specifying it *was* the work — the design question (why meta tags and not translators) is the whole phase. Review earns its keep: it's a new network path taking untrusted HTML. |
| 14 — Fixed library location + `install.sh` | no | no | Small, mechanical, same env-var-then-`$HOME` pattern `config.rs` already has. `install.sh` is an install script, not a trust boundary in the running program. |
| 15 — `search --text` (FTS5-trigram) | **yes** | **yes** | Real schema migration (virtual table, sync triggers, backfill), not a one-clause extension. A trigger that silently fails to fire is exactly the "review earns its keep on silent failures" case. |
| 16 — TUI editing, fetch, delete, merge | **yes** | **yes** | Biggest TUI grind yet (five new modes), plus two silent-failure traps: attachment-filename collision on merge (same class as Phase 12's `attach` race) and a blocking network call inside the render loop that must not corrupt terminal state on failure. |
| 17 — `ferref doctor` | no | no | One join query plus a CLI command, no migration, no new trust boundary. Same shape as Phase 4. |

The table is a default, not a rule. The reasoning behind it, which outlives the
table if the phases change:

- **If writing the brief is most of the work, don't delegate.** A subagent
  starts cold; everything already known about the schema and design has to be
  re-derived or written into the brief. Delegation wins when the implementation
  grind is bigger than the specification.
- **The win is context isolation, not cost.** A subagent's tool output — cargo
  runs, file reads, compile-error iteration — never enters the coordinating
  session. Judge a delegation by how much noise it keeps out.
- **Review earns its keep on silent failures.** Panics, type errors, and failed
  assertions are already caught by the toolchain. Fresh eyes pay off where the
  bug is a wrong *assumption* rather than a wrong line — both defects found in
  Phase 1 (`date_modified` silently freezing; `main.rs` printing a stale row
  that read like success) were invisible to both compiler and tests.
- **Don't delegate exploratory work.** If it isn't yet clear what correct looks
  like, a subagent can't find out for you.
- **Check the subagent's self-report against a real `cargo test`.** Nearly free,
  and the failure it guards against — a confident summary of work that didn't
  happen — is the expensive one.
