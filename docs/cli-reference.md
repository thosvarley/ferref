# Command reference

Run `ferref <command> --help` for the full list of flags. Every command below
accepts `--json`, except `export` (its output is BibTeX) and `tui` (it's
interactive).

## Adding and editing

| Command | What it does |
| --- | --- |
| `add --doi <doi>` | Add a paper, with metadata from Crossref |
| `add --url <url>` | Add a paper from its web page, and download the PDF it links to. Use `--url` on its own for this |
| `add --type <t> --key <k> --title <t>` | Add a paper by hand. Also takes `--author` (repeatable, `"Last, First"`), `--year`, `--journal`, `--volume`, `--pages`, `--doi`, `--url`, `--abstract` |
| `edit <key>` | Change fields. Only the flags you pass are changed. `--author` replaces the whole author list |
| `rm <key>` | Delete a paper, with its tags, collection memberships, and attachment records |
| `merge <keep> <drop>` | Move `drop`'s tags, collections, and PDFs to `keep`, then delete `drop` |

With `--doi` or a lone `--url`, any other field you pass overrides the fetched
value.

## Viewing and searching

| Command | What it does |
| --- | --- |
| `list` | Every paper. Filter with `--tag` or `--collection` (add `--recursive` to include subcollections) |
| `show <key>` | One paper in full |
| `search` | Filter by `--author`, `--title`, `--year`, `--from`, `--to`, `--tag`, `--collection`, `--recursive`. All filters must match |
| `search --text <query>` | Search inside the papers' text, and show where each match occurs |

`list --full-text` and `search --full-text` include each PDF's text. They
require `--json`.

## Tags and collections

| Command | What it does |
| --- | --- |
| `tag <key> <tag>` / `untag <key> <tag>` | Add or remove a tag. Repeating either does nothing |
| `collection new <path>` | Create a collection, and any missing parents |
| `collection ls` | Show the collection tree, with paper counts |
| `collection add <path> <key>` / `collection rm <path> <key>` | File or unfile a paper |
| `collection mv <path> --parent <path>` | Move a collection under another. Use `--root` to move it to the top level |
| `collection delete <path>` | Delete a collection and its subcollections. The papers are not deleted |

## PDFs and text

| Command | What it does |
| --- | --- |
| `attach <key> <file>` | Copy a file into the library and attach it. `--extract` also converts it to text |
| `extract <key>` | Convert a paper's PDFs to text (again) |
| `open <key>` | Open a paper's PDFs in your default viewer |
| `fetch <key>` | Find and download a free, legal copy using the paper's DOI. Tries every copy Unpaywall lists (PubMed Central's copy first, if the paper has one, then Unpaywall's own PDF links), then arXiv, bioRxiv/medRxiv, OSF, and preprints.org, moving on after any failed download. `--email` sets the contact address Unpaywall requires |
| `doctor` | List attachments whose file is missing. Exits 1 if it finds any. `--fix` removes those records |

## BibTeX

| Command | What it does |
| --- | --- |
| `export` | Print BibTeX. `--out <file>` writes a file instead. `--collection <path>` (with optional `--recursive`) exports one collection. `--biblatex` writes biblatex instead of plain BibTeX |
| `import <file>` | Read a `.bib` file. Papers you already have (same cite key or DOI) are skipped |

## Browsing

| Command | What it does |
| --- | --- |
| `tui` | Open the terminal browser. See {doc}`tui` |
