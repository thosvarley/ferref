# Installation

## Requirements

- **Rust**, 2024 edition, to build ferref.
- **`pdftotext`**, from poppler-utils, to extract text from PDFs
  (`apt install poppler-utils`).
- **`xdg-open`** (Linux) or **`open`** (macOS), for `ferref open`.

SQLite is built into ferref, so you don't need to install it.

## With the install script

```sh
./install.sh
```

The script builds ferref, copies the binary to `~/.local/bin`, and asks where
your library should live (the default is `~/.ferref`). It adds `~/.local/bin`
to your `PATH` if needed. Run it again whenever you want to install a newer
build.

## With Nix

The repository includes a flake that provides both Rust and `pdftotext` for
you:

```sh
nix run github:thosvarley/ferref             # try it without installing
nix profile install github:thosvarley/ferref # install it
```

You can also add it as an input to your own flake and use
`ferref.packages.${system}.default`.

## Where your library lives

ferref keeps one library, and you can use it from any directory. It contains:

- `ferref.db`, the database
- `pdfs/`, every attached PDF

The location is `~/.ferref` unless the `FERREF_HOME` environment variable says
otherwise.

## Optional: an email address for `fetch`

`ferref fetch` asks the Unpaywall service for open-access PDFs, and Unpaywall
asks callers to identify themselves with an email address. Set it once in
`~/.config/ferref/config.toml`:

```toml
email = "you@example.com"
```

You can also use the `FERREF_EMAIL` environment variable, or pass `--email`
to a single `fetch`. The address is sent to Unpaywall and nowhere else.
