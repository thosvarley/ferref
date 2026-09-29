# The terminal browser

`ferref tui` opens a three-pane browser:

```
┌ COLLECTIONS ─┬ ENTRIES [year ↓] ───────────────┬ DETAILS ──────┐
│ ▾ All (12)   │ Title        Authors  Year Jrnl │ Jaynes, E. T. │
│   ▾ Physics  │ Information… Jaynes   1957 Phys │ 1957          │
│     Entropy  │ A Mathemati… Shannon  1948 Bell │ #entropy      │
└──────────────┴─────────────────────────────────┴───────────────┘
 ?  help · q quit
```

- **Collections** (left): pick a collection to see its papers, including those
  in its subcollections.
- **Entries** (middle): the papers, which you can sort, search, and mark.
- **Details** (right): everything about the highlighted paper, including its
  abstract.

Press `?` at any time for a list of keys.

## Moving around

Vim keys and arrow keys both work.

| Key | Action |
| --- | --- |
| `Tab` / `Shift-Tab` | Move focus to the next / previous pane |
| `j` `k` or `↓` `↑` | Move down / up (in Details, scroll) |
| `g` / `G` | Jump to the top / bottom |
| `Ctrl-d` / `Ctrl-u` | 10 rows down / up |
| `h` / `l` | In Collections: fold / unfold. Elsewhere: move to the pane on the left / right |
| `r` | Reload from the database |
| `q` | Quit |
| `Esc` | Clear the search and all marks. If there's nothing to clear, quit |

## Finding papers

| Key | Action |
| --- | --- |
| `/` | Search. The list filters as you type, matching titles, authors, journal, year, cite key, and tags. `Enter` keeps the filter; `Esc` cancels |
| `s` | Change the sort column: title, author, year, journal |
| `S` | Reverse the sort |

Searching and sorting only rearrange what's on screen, so they're instant.

## Working with papers

These keys work on the highlighted paper in the Entries pane (most also work
from Details).

| Key | Action |
| --- | --- |
| `o` | Open the paper's PDFs |
| `y` | Copy the paper's URL (or its DOI link) to the clipboard |
| `c` | File the paper into a collection. `Enter` toggles, so it can also unfile |
| `x` | Export as BibTeX to a file you name |
| `:` | Open the command menu (below) |

The command menu (`:`):

| Key | Action |
| --- | --- |
| `e` | Edit a field. Clearing a field empties it (the title can't be empty). Authors are written `Last, First; Last, First` |
| `f` | Fetch an open-access PDF (see {doc}`tutorial`) |
| `a` | Attach a PDF from your disk, using a file browser |
| `t` / `u` | Add / remove a tag |
| `m` | Merge two entries |
| `d` | Delete the paper, after a y/n confirmation |

In the file browser, `j`/`k` move, `l` or `Enter` opens a folder or attaches a
file, `h` or `Backspace` goes up a folder, and `Esc` cancels. It starts in your
home directory. Attached PDFs are converted to text straight away.

## Working with many papers at once

Mark papers, then act on all of them.

| Key | Action |
| --- | --- |
| `Space` | Mark or unmark the highlighted paper |
| `A` | Mark every paper currently shown |
| `U` | Clear all marks |

With papers marked, `c` files all of them into a collection, `x` exports all of
them, and `:` then `t` or `u` tags or untags all of them. Marks stay in place
when you switch collections, so you can gather papers from several.

**Merging.** Press `:` then `m`. Which paper survives depends on your marks:

- **Two marked:** the first one you marked is kept, and the second is folded
  into it and deleted.
- **None or one marked:** the highlighted paper is kept, and you pick the other
  from a searchable list.
- **Three or more marked:** the first one you marked is kept, and you pick the
  other from the rest.

Tags, collections, and PDFs move to the paper you keep. Its own fields are left
unchanged.

## Collections

With the Collections pane focused:

| Key | Action |
| --- | --- |
| `n` | Create a collection inside the highlighted one |
| `R` | Rename the highlighted collection |
| `D` | Delete the highlighted collection and its subcollections, after a y/n confirmation. Papers stay in the library |
| `x` | Export every paper in the highlighted collection (and its subcollections) as BibTeX. The file is named after the collection |

Neither `R` nor `D` does anything on "All Papers" -- it isn't a real collection.

## Good to know

- The TUI doesn't watch the database. If you change something from another
  terminal, press `r` to see it.
- `x` writes plain BibTeX. For biblatex output, use
  `ferref export --biblatex` from the command line.
- Copying to the clipboard works over SSH too, as long as your terminal
  supports OSC 52 (most modern ones do).
