# Design

This page summarizes how ferref is built and why. The full record, including
every phase of work and what review found, is in
[`DESIGN.md`](https://github.com/thosvarley/ferref/blob/main/DESIGN.md).

## Principles

- **Open data.** The library is a plain SQLite file. Nothing is stored in a
  format only ferref can read.
- **Scriptable by default.** Every command that prints data supports `--json`
  from the day it's added, so scripts never have to parse human-readable
  output.
- **Text first.** Abstracts and extracted PDF text are core data, because they
  are what search, embeddings, and language models need.
- **Stable identifiers.** A paper's `id` and `cite_key` never change, so other
  tools can safely key their own data to them.
- **Two equal front ends.** The command line serves scripts and agents; the TUI
  serves people. Friction in either one counts as a bug.

## What ferref deliberately doesn't do

- **AI work.** ferref doesn't compute embeddings or call language models. It
  hands clean text and metadata to the tools that do.
- **Paywall workarounds.** `fetch` only downloads copies that are legally free.
- **Citation formatting.** BibTeX and biblatex already do this well.
- **Sync or multiple users.**
- **A graphical interface.**

## How the code is organized

| Module | Responsibility |
| --- | --- |
| `models.rs` | The `Entry` and `Author` types |
| `db.rs` | The schema and every SQL query, as plain functions |
| `bibtex.rs` | Converting between entries and BibTeX |
| `text.rs` | Running `pdftotext` safely |
| `doi.rs` | Crossref, Unpaywall, the preprint servers, and every network request |
| `config.rs` | The library location and the Unpaywall email |
| `cli.rs` | Command-line argument definitions |
| `main.rs` | Running commands, plus the logic the CLI and TUI share |
| `tui.rs` | The terminal browser |

Any change to the library, such as attaching or fetching a PDF, has one
implementation that both the CLI and the TUI call.

## Safety

ferref handles input it doesn't control, so three places are deliberately
defensive:

- **Network (`doi.rs`).** Every request goes through one function. It only
  allows `http` and `https`, refuses private and local addresses, re-checks
  every redirect, caps response sizes, and rejects a "PDF" that doesn't start
  with `%PDF`.
- **PDF extraction (`text.rs`).** `pdftotext` runs with a time limit and a cap
  on how much output is kept, and is fully stopped if it overruns.
- **Files.** New files in `pdfs/` are claimed atomically, so two commands
  running at once can't overwrite each other's downloads.
