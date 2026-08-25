# ferref

A command-line/TUI reference manager.

The premise: a reference manager that can be useful for an AI agent as well
as a human being — for a local RAG system on air-gapped machines, fine-tuning
on pre-selected scientific literature, or compiling bibliographies. There is
no GUI; everything is done on the command line for easy script-based
interfacing. There is a TUI for human users that aims to mimic the experience
of Zotero, but using keystrokes inspired by Vim motions.

The database is a plain SQLite file — no proprietary blob format. Anyone (or
anything) can open it directly. See [Design & architecture](design) for the
reasoning behind that and every other choice in the project, and a frank list
of known limitations.

```{toctree}
:maxdepth: 2
:hidden:

installation
tutorial
tui
cli-reference
scripting
database
limitations
development
design
```

## Where to start

- New to ferref? Start with {doc}`installation`, then work through {doc}`tutorial`.
- Driving ferref from a script or a model pipeline? {doc}`scripting` covers the
  `--json` contract and the output shape.
- Using the terminal browser? See {doc}`tui`.
- Looking for a specific flag? {doc}`cli-reference` is the full command table.
