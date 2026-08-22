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

## Current state (2026-08-21)

Phases 1–7 are complete. Phase 8 (DOI lookup) is next.

- `src/models.rs` — `Entry`/`Author` + `now()`. `Serialize` derived; `abstract_text` serializes as `"abstract"` (Rust reserves `abstract`), locked by a test.
- `src/db.rs` — schema + `insert_entry`/`get_entry`/`list_entries`/`update_entry`/`delete_entry`, all free functions over `&Connection`. `update_entry` stamps `date_modified` itself so callers can't forget. `list_entries` takes a `Filter`; an all-`None` filter matches everything, so `list` and `search` are one query. `add_tag`/`remove_tag` are idempotent and report whether anything changed; `normalize_tag` (trim + lowercase) is the single point both writes and the `tag` filter go through. `attach` stores a path, never a copy of the file.
- `src/bibtex.rs` — `import`/`export` over the `biblatex` crate, plus the `biblatex::Entry` ↔ `models::Entry` mapping.
- `src/text.rs` — `extract_text` over `pdftotext`. The project's trust boundary: bounded memory (the drain keeps the first 10MB and discards the rest rather than buffering it all), a 30s deadline covering *both* the child wait and the pipe drain, and a process-group kill so a wrapper script's descendants can't outlive us.
- `src/cli.rs` — clap derive types for `add`/`list`/`show`/`edit`/`rm`/`search`/`import`/`export`, plus `parse_author`.
- `src/main.rs` — thin dispatcher. All stdout goes through `emit()`, which exits 0 on a closed pipe instead of panicking. Failures exit non-zero with the message on stderr.

### Known limitations

- `edit` cannot clear a field back to `null`, or empty an author list — flags are
  only applied when passed, and there's no "unset" sentinel. Workaround is `rm` +
  `add`. Worth revisiting when something actually needs to retract a field.
- `edit` has no `--type`, so `entry_type` is fixed at creation.
- `cite_key` is not renameable; `update_entry` looks entries up by it.
- Tag names are lowercased on the way in, so a tag can't carry display casing
  (`NLP` is stored and shown as `nlp`).
- Nothing lists all known tags, and a tag row orphaned by its last `untag` is
  left behind rather than garbage-collected. Both are worth fixing together, if
  a `ferref tags` command ever exists.
- There is no `detach`, and no way to edit an attachment's path. Since `attach`
  rejects a path that doesn't resolve, the usual cause (a typo) is caught up
  front; a moved file still needs `rm` + re-add.
- Attachment paths are absolute and stored at attach time. Moving the file, or
  the library, breaks them silently — nothing revalidates them.
- Extraction is PDF-only and requires `pdftotext` (poppler-utils) on `PATH`.
- Extracted text is capped at 10MB per attachment, with a truncation marker.
- `full_text` is omitted from `list`/`search` unless `--full-text` is passed, so
  the common case doesn't read the whole library's text into memory.
- Tags don't survive a BibTeX round trip. BibLaTeX has a `keywords` field that
  would carry them; mapping it wasn't in Phase 5's scope.
- The DB is always `./ferref.db`, relative to the current directory. Phase 8 adds
  a config file and is the natural point to fix this.

BibTeX round trips (Phase 3) lose two things, both inherent to the target format
rather than fixable in our mapping:

- **Non-legacy `entry_type`s collapse to `misc` on export.** We serialize with
  `to_bibtex_string()`, and legacy BibTeX has no `@online`/`@dataset`/`@software`,
  nor any custom type like `@preprint`. Switching to `to_biblatex_string()` would
  preserve them but emit `journaltitle`/`date` instead of `journal`/`year`, which
  plain BibTeX and LaTeX can't read — a worse loss for the main use case. If both
  audiences ever need serving, the fix is an `export --format bibtex|biblatex`
  flag, not a change of default.
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
- Citation formatting (APA, MLA) — last, purely cosmetic

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

Store the path, not the file — copying/managing a file store is a separate feature, out of scope for v1.

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

## Phase 9 — Citation formatting

New `src/cite.rs`: `format_apa(&Entry) -> String`, `format_mla(&Entry) -> String` — plain string templates over the fields already on `Entry`. Purely cosmetic, last on purpose — doesn't feed the AI-native use case at all.

> ponytail: skip a full CSL engine (e.g. `hayagriva`) for two fixed styles — add it later only if more styles or edge-case correctness (et al. rules, ordinals, etc.) are actually needed.

CLI:
```
ferref cite <cite_key> --style apa
```

---

## Explicit non-goals for v1

- Doing the AI work ourselves — no embeddings, no bibliometric analysis, no LLM calls inside ferref. ferref's job stops at handing clean, structured, scriptable data to whatever does that work.
- Circumventing paywalls — full-text fetch is strictly limited to what Unpaywall reports as legally open access. No scraping, no Sci-Hub-style fallbacks.
- Full CSL styling engine / arbitrary citation styles
- Sync or multi-user access
- GUI (TUI is a real future goal, GUI is not)

## Order of work

Phase 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9. Phase 1 unblocks everything else — nothing downstream is useful until entries actually persist. Phases 7 and 8 (full text, DOI fetch) are pulled ahead of citation formatting because they're what actually serves the AI-native vision; APA/MLA formatting is cosmetic and can slip without cost.

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
| 9 — Citations | no | no | String templates over fields that already exist. Trivially testable. |

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
