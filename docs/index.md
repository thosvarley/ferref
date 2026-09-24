# ferref

ferref is a reference manager for the terminal. It is built to be used by a
person at a keyboard and by scripts or AI agents, with neither treated as an
afterthought.

- **Your library is a plain SQLite file** plus a folder of PDFs. Any tool that
  reads SQLite can read it, and backing it up means copying one directory.
- **Every command that prints data accepts `--json`**, so ferref slots into
  shell pipelines, Python scripts, and model pipelines.
- **Full text is first-class.** PDFs are converted to text and indexed, so you
  can search inside papers or hand their text to an embedding or LLM pipeline.
- **`ferref tui`** opens a three-pane browser in the style of Zotero, driven with
  Vim keys.

It is useful for keeping a bibliography, building a local retrieval (RAG) corpus
on a machine without internet access, or assembling a set of papers for
fine-tuning.

## Where to start

- **New here?** {doc}`installation`, then the {doc}`tutorial`.
- **Browsing in the terminal?** {doc}`tui`.
- **Looking up a command or flag?** {doc}`cli-reference`.
- **Writing a script against ferref?** {doc}`scripting`, then {doc}`database`.

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
