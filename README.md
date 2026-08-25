# ferref

A command-line/TUI reference manager.

The premise: a reference manager that can be useful for an AI agent as well as
a human being — for a local RAG system on air-gapped machines, fine-tuning on
pre-selected scientific literature, or compiling bibliographies. There is no
GUI; everything is done on the command line for easy script-based
interfacing. There's also a TUI for human browsing, using Vim-inspired
keybindings.

The database is a plain SQLite file, not a proprietary blob format —
`sqlite3 ~/.ferref/ferref.db` gets you something sane.

**Full documentation: https://ferref.readthedocs.io** — tutorial, TUI guide,
command reference, JSON/scripting contract, and the design doc (reasoning and
known limitations).

## Install

Requires Rust (2024 edition) and `pdftotext` (`apt install poppler-utils`).

```sh
./install.sh
```

Builds the release binary, installs it to `~/.local/bin`, and creates the
library directory (`~/.ferref` by default, override with `FERREF_HOME`).
Re-run any time to pick up a new build.

## Quick look

```console
$ ferref add --doi 10.1103/PhysRev.106.620
Information Theory and Statistical Mechanics [jaynes1957]
  ...

$ ferref tag jaynes1957 entropy
$ ferref collection add "Information Theory" jaynes1957
$ ferref search --tag entropy --json | jq -r '.[].cite_key'
jaynes1957

$ ferref tui   # three-pane terminal browser
```

Every data-printing command supports `--json` — see the [scripting
docs](https://ferref.readthedocs.io/en/latest/scripting.html) for the full
contract and the JSON shape.

## Development

```sh
cargo test
cargo build
```

No test touches the network. See `DESIGN.md` for the design principles and
reasoning behind ferref's choices — or the rendered version at
[readthedocs](https://ferref.readthedocs.io/en/latest/design.html).

To build the docs locally: `pip install -r docs/requirements.txt && sphinx-build -b html docs docs/_build/html`.
