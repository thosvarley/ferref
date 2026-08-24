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

All thirteen phases are complete.

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
- The DB is always `./ferref.db`, relative to the current directory. A library is
  a folder you `cd` into, like a git repo, and that has held up — but it means two
  libraries can't be worked with from one shell. `config.rs` exists and reads one
  key, so a `database` key is the obvious home if that ever bites.

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

No new dependencies. Meta-tag scanning is ~60 lines over the raw HTML, in the
same spirit as `strip_jats_tags`: crude, documented as crude, and tested. An
HTML parser would be a dependency bought to read six attributes.

---

## Explicit non-goals for v1

- Doing the AI work ourselves — no embeddings, no bibliometric analysis, no LLM calls inside ferref. ferref's job stops at handing clean, structured, scriptable data to whatever does that work.
- Circumventing paywalls — full-text fetch is strictly limited to what Unpaywall reports as legally open access. No scraping, no Sci-Hub-style fallbacks.
- Full CSL styling engine / arbitrary citation styles
- Sync or multi-user access
- GUI (TUI is a real future goal, GUI is not)

## Order of work

Phase 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13. Phase 1 unblocks everything else — nothing downstream is useful until entries actually persist. Phases 7 and 8 (full text, DOI fetch) are pulled ahead of citation formatting because they're what actually serves the AI-native vision; APA/MLA formatting is cosmetic and can slip without cost.

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
