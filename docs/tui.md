# Browsing in the terminal

`ferref tui` opens a three-pane browser: collections on the left, entries in the
middle, details for the highlighted entry on the right.

```
┌─────────────┬─ENTRIES [year ↓]───────────────┬──────────────┐
│ COLLECTIONS │ Title        Authors  Year Jrnl │ DETAILS      │
│ ▾ All (12)  │ Array progr… Harris  2020 Natu │ Jaynes, E.T. │
│   ▾ Physics │ Information… Jaynes  1957 Phys │ Phys Rev 106 │
│     Entropy │ A Mathemati… Shannon 1948 Bell │ #entropy     │
└─────────────┴────────────────────────────────┴──────────────┘
 Tab pane · jk move · / search · s sort · n new · c file · o open · q quit
```

It navigates like vim, and like arrows — both work everywhere.

| Key | Does |
| --- | --- |
| `Tab` / `Shift-Tab` | Cycle focus between panes |
| `j` `k` or `↑` `↓` | Move the selection |
| `g` / `G` | Jump to top / bottom |
| `Ctrl-d` / `Ctrl-u` | Half a page down / up |
| `h` `l` or `←` `→` | Collapse / expand a collection; elsewhere, move a pane left or right |
| `s` / `S` | Cycle the sort column (title → author → year → journal) / reverse it |
| `/` | Search — filters the table live as you type, over titles, authors, journal, year, cite_key and tags |
| `n` | New collection, as a child of the highlighted one (or at the root, under "All Papers") |
| `c` | File the highlighted paper into a collection — `Enter` toggles, so it unfiles too |
| `o` | Open the highlighted paper's attachments in your viewer |
| `r` | Reload from the database |
| `q`, `Esc`, `Ctrl-C` | Quit — `Esc` clears an active search first |

Selecting a collection filters the middle pane recursively, so a parent shows
everything beneath it. Sorting and searching happen in memory over what's
already loaded, so they're instant and they don't touch the database.

**What the TUI can change:** it files papers into collections and creates
collections. That's it. Editing entries, deleting anything, and tagging stay
CLI-only, so a mis-keypress can misfile a paper but can't destroy data. Press
`r` to pick up changes made from another shell.

The TUI doesn't watch the database — changes from another shell appear on
`r`, not on their own.
