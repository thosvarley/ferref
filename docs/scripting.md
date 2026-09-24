# Scripting

Every command that prints data accepts `--json`. That makes ferref easy to call
from shell pipelines, Python, or a model pipeline.

The examples use [`jq`](https://jqlang.github.io/jq/), but any JSON tool works:

```sh
ferref list --json | python3 -c 'import json, sys; [print(e["cite_key"]) for e in json.load(sys.stdin)]'
```

## Recipes

```sh
# Every cite key, one per line
ferref list --json | jq -r '.[].cite_key'

# Extract text for every paper
ferref list --json | jq -r '.[].cite_key' | xargs -n1 ferref extract

# Try to fetch a PDF for every paper that has a DOI
ferref list --json | jq -r '.[] | select(.doi) | .cite_key' | xargs -n1 ferref fetch

# Export one collection, including its subcollections
ferref export --collection "Information Theory" --recursive --out it.bib

# cite key and full text, tab-separated, for an embedding pipeline
ferref list --json --full-text \
  | jq -r '.[] | select(.attachments[0].full_text) | [.cite_key, .attachments[0].full_text] | @tsv'

# Fail a scheduled job if any PDF has gone missing
ferref doctor || echo "ferref: missing PDFs" | mail -s ferref you@example.com
```

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. This includes "no results" and "no open-access copy found" |
| `1` | Something went wrong. The message is on stderr |
| `2` | Bad arguments |

Output is safe to pipe into `head`: ferref exits quietly when the pipe closes.

## An entry in JSON

`show`, `list`, and `search` return entries in this shape (`list` and `search`
return an array of them):

```json
{
  "id": 2,
  "entry_type": "article",
  "cite_key": "harris2020",
  "title": "Array programming with NumPy",
  "authors": [{ "first_name": "Charles R.", "last_name": "Harris" }],
  "tags": ["numerics"],
  "attachments": [{ "path": "/home/you/.ferref/pdfs/harris2020.pdf", "full_text": "Review\n\nArray..." }],
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

- `full_text` is `null` if the PDF hasn't been converted to text. In `list` and
  `search` it is always `null` unless you pass `--full-text`.
- Dates are Unix timestamps, in seconds.
- `id` and `cite_key` never change, so they're safe to use as keys in your own
  data.

## Full-text search

`search --text <query>` finds papers whose text contains the query, ignoring
case. It returns snippets rather than whole texts, so it works without
`--json` too:

```json
[
  {
    "cite_key": "vaswani2017",
    "title": "Attention Is All You Need",
    "year": 2017,
    "matches": [
      {
        "path": "/home/you/.ferref/pdfs/vaswani2017.pdf",
        "snippets": ["…Noam proposed scaled dot-product attention, multi-head attention and the parameter…"],
        "total_matches": 6
      }
    ]
  }
]
```

The search uses an index, so it stays fast with tens of thousands of papers.
Queries shorter than three characters can't use the index and scan the text
instead, which is slower but still correct.

## Other JSON outputs

`fetch --json` when a PDF was downloaded:

```json
{ "cite_key": "harris2020", "doi": "10.1038/s41586-020-2649-2", "oa_found": true,
  "source": "Unpaywall", "path": "/home/you/.ferref/pdfs/harris2020.pdf",
  "already_present": false, "extracted": true, "chars": 41013 }
```

…and when none was found. `is_oa` is Unpaywall's verdict, or `null` if
Unpaywall couldn't be reached:

```json
{ "cite_key": "piwowar2018", "doi": "10.7717/peerj.4375", "oa_found": false,
  "is_oa": true, "attempted": ["Unpaywall", "arXiv", "bioRxiv", "OSF", "preprints.org"] }
```

`doctor --json`:

```json
{ "checked": 3, "broken": [{ "cite_key": "shannon1948", "path": "/home/you/.ferref/pdfs/shannon1948.pdf" }] }
```

With `--fix`, a `"fixed"` count is added, and `"broken"` still lists what was
broken before the fix.
