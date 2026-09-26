# Known limitations

**Library**

- There is one library per install. `FERREF_HOME` can point it elsewhere, but
  ferref can't switch between several.
- Attachment paths are stored in full. If you move the library folder, every
  path breaks, and `ferref doctor --fix` would remove them all. Move it back,
  or update the paths with SQL, before running `--fix`.
- Deleting or merging a paper leaves its PDF files in `pdfs/`. Nothing cleans
  them up yet.

**Entries**

- Two papers can't share a DOI, but ferref can't tell that a preprint and its
  published version are the same paper. Use `ferref merge` when you spot one.
- A cite key can't be changed after the paper is added.
- `ferref edit` can't clear a field. The TUI's editor can.
- There is no command to detach a PDF that still exists on disk.

**Fetching**

- `add --doi` only works for DOIs that Crossref knows about. DataCite DOIs
  (arXiv's `10.48550/arXiv.*`, Zenodo, Figshare, ...) are not among them, so
  add those with `add --url <landing page>` instead -- when the DOI comes
  from the page itself rather than `--doi`, a Crossref 404 falls back to the
  page's own meta tags automatically, with a note that Crossref had no
  record.
- `fetch` only downloads free, legal copies. It will not get around a paywall.
  It tries every copy Unpaywall lists plus PubMed Central's, moving on after a
  failed download, but some publisher sites answer scripts with a bot check
  (Cloudflare, Akamai, reCAPTCHA); `fetch` reports those clearly rather than
  trying to solve them -- download the PDF in a browser and attach it with
  `ferref attach` instead. The whole search is capped at 60 seconds, so a DOI
  with many candidates (some journals list a dozen-plus) doesn't hang
  indefinitely; whatever wasn't tried in time is reported as such rather than
  silently skipped.

**Text**

- Only PDFs can be converted to text, and only if `pdftotext` is installed.
- At most 10 MB of text is kept per PDF.

**BibTeX**

- Export writes plain BibTeX unless you pass `--biblatex`. Plain BibTeX has no
  `@online` or `@dataset`, so those become `@misc`.
- Tags survive an export and re-import (in the `keywords` field). Collections
  don't.
- Line breaks inside a field, such as a multi-paragraph abstract, become spaces.

**Collections**

- The command line addresses collections by path, so a collection whose name
  contains `/` can't be reached from it. ferref won't create one, and the TUI
  handles them fine.

**TUI**

- It doesn't notice changes made from another terminal until you press `r`.
- Its export (`x`) writes plain BibTeX only.
- Its author editor round-trips the whole author list through one
  "Last, First; Last, First" text box, so an author whose name contains `;`,
  or a single-name/organization author (e.g. "LIGO Scientific Collaboration")
  whose name contains `,`, gets split at the wrong place. Not redesigned;
  use `ferref edit --author` on the CLI, once per author, for those.
