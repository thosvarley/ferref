# Scripting

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

# Cite keys for one collection, recursively (there's no per-collection
# BibTeX export yet — `ferref export` always writes the whole library)
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

## The JSON shape

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
`search` output unless you pass `--full-text`. `abstract` is spelled
`abstract`, not `abstract_text`.

## Full-text search

`search --text <query>` does a case-insensitive substring search over every
attachment's extracted PDF text, backed by a SQLite FTS5 virtual table using
the trigram tokenizer rather than a linear scan — this stays fast as the
library grows into the tens of thousands of attachments. Queries under 3
characters fall back to a plain scan, since a trigram index can't represent a
match shorter than one trigram.

See {doc}`design` for how that index is built and why trigram was chosen over
the alternatives.
