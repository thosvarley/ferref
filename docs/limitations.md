# Limitations worth knowing

- The library is one fixed location (`FERREF_HOME`, else `~/.ferref`) — there's
  no support for multiple libraries from one install.
- Two entries can't share a DOI, but nothing stops the same paper being added
  twice under two DOIs (a preprint and its published version, say), or with no
  DOI at all.
- `edit` can't clear a field back to null, and there's no `detach`.
- Attachment paths are absolute and stored once. The files live in
  `~/.ferref/pdfs/`, but the paths don't move with the library — relocating the
  directory breaks every link silently.
- BibTeX export writes legacy BibTeX by default; `@online` and `@dataset` need
  `--biblatex`, which legacy BibTeX styles can't read. Collections don't survive
  a round trip (tags do, via `keywords`).
- Extraction is PDF-only, capped at 10 MB of text per attachment.
- A collection whose *name* contains `/` can't be addressed by the CLI's path
  syntax. The CLI won't create one; only hand-editing the database can. The TUI
  reaches collections by id and handles them fine.
- The TUI files papers and creates collections, but can't edit or delete
  anything, rename a collection, or tag — use the CLI, then press `r`.
- The TUI doesn't watch the database. Changes from another shell appear on `r`,
  not on their own.

{doc}`design` has the full list, with the reasoning behind each.
