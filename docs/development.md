# Development

```sh
cargo test        # no network access required
cargo build
```

No test touches the network; the Crossref and Unpaywall parsers are tested
against captured fixture strings.

See {doc}`design` for the design principles and the reasoning behind ferref's
choices.

## Building these docs

```sh
pip install -r docs/requirements.txt
sphinx-build -b html docs docs/_build/html
```
