# Command reference

| Command | What it does |
| --- | --- |
| `add` | Create an entry. `--type/--key/--title`, or `--doi` to autofill from Crossref |
| `list` | List everything. `--tag`, `--collection`, `--recursive`, `--full-text` |
| `show <key>` | One entry in full |
| `edit <key>` | Change fields; only the flags you pass are touched |
| `rm <key>` | Delete an entry (cascades to authors, tags, attachments) |
| `search` | `--author --title --year --from --to --tag --collection --recursive --text --full-text` |
| `tag` / `untag <key> <tag>` | Add/remove a tag (flat, describes a paper). Idempotent |
| `attach <key> <path>` | Copy a file into `pdfs/` and attach it. `--extract` to pull text immediately |
| `extract <key>` | (Re)extract text for all of an entry's attachments |
| `open <key>` | Open attachments in the system viewer |
| `fetch <key>` | Find and download an open-access PDF for the entry's DOI |
| `add --from-url <url>` | Add from a publisher's landing page, downloading the PDF it advertises using this machine's network access |
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

`search --text` is a substring search over extracted PDF text — see
{doc}`scripting` for details.
