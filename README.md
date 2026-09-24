# ferref

A reference manager for the terminal, built for people and for scripts.

- **Plain SQLite storage.** Your library is one `.db` file plus a folder of PDFs.
  Any tool that reads SQLite can read it.
- **Scriptable.** Every command that prints data accepts `--json`.
- **Full text.** PDFs are converted to text and indexed, so you can search inside
  papers or feed them to an embedding or LLM pipeline.
- **A terminal browser.** `ferref tui` gives a three-pane, Zotero-style view with
  Vim keys.

Full documentation: **https://ferref.readthedocs.io**

## Install

You need Rust (2024 edition) and `pdftotext` (`apt install poppler-utils`).

```sh
./install.sh
```

This builds ferref, copies it to `~/.local/bin`, and creates your library at
`~/.ferref` (set `FERREF_HOME` to put it elsewhere). Run it again after pulling
changes.

With Nix, no toolchain is needed:

```sh
nix run github:thosvarley/ferref             # try it
nix profile install github:thosvarley/ferref # install it
```

## A quick look

```console
$ ferref add --doi 10.1103/PhysRev.106.620
Information Theory and Statistical Mechanics [jaynes1957]
  ...
$ ferref tag jaynes1957 entropy
$ ferref collection new "Information Theory"
$ ferref collection add "Information Theory" jaynes1957
$ ferref search --tag entropy --json | jq -r '.[].cite_key'
jaynes1957
$ ferref tui
```

## Development

```sh
cargo test    # never touches the network
cargo build
```

`DESIGN.md` records the design principles and the history of every change.
