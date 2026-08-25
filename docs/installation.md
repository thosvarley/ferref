# Installation

## Requirements

- Rust (2024 edition) — `cargo build --release`
- `pdftotext` from **poppler-utils**, for full-text extraction
  (`apt install poppler-utils`)
- `xdg-open` (Linux) or `open` (macOS), for `ferref open`

SQLite is bundled via `rusqlite`; you don't need to install it.

## Install script

```sh
./install.sh
```

Builds the release binary, installs it to `~/.local/bin`, and creates the
library directory. Re-run it any time to pick up a new build.

The library lives in one fixed place — `~/.ferref` by default, or wherever you
told `install.sh` to put it.
Override it per-invocation with the `FERREF_HOME` environment variable.

(For local hacking without installing: `cargo build --release &&
./target/release/ferref --help` works the same way, still reading `~/.ferref`.)
