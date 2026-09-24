# Development

```sh
cargo test    # never touches the network
cargo build
```

Tests for the Crossref, Unpaywall, and bioRxiv code run against saved
responses, so the suite works offline.

## How work is organized

`DESIGN.md`, at the root of the repository, is the working record of the
project. It lists the design principles and every phase of work so far: what
was built, why, and what review turned up. New work starts with a section
there. {doc}`design` summarizes the parts that matter for using and extending
ferref.

## Building these docs

```sh
pip install -r docs/requirements.txt
sphinx-build -b html docs docs/_build/html
```
