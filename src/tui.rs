// Terminal UI over the same SQLite file the CLI writes. Sorts, searches,
// files papers into collections, edits fields, fetches PDFs, tags/untags,
// deletes and merges entries -- singly or, with entries marked, in bulk --
// and copies an entry's link to the system clipboard. Mutating and
// destructive actions live behind the ":" command palette and (for
// delete/merge) a confirm prompt, so they're deliberate rather than a
// stray keypress. Collections can also be renamed ("R") and deleted ("D"),
// each behind its own confirm/input; creating a new entry stays CLI-only.
//
// Data is fetched only when state changes (on load, on a collection
// selection change, on a sort/filter edit, or on manual reload) and cached
// in `App`; the render function (`draw`) never touches the `Connection`.
// Sorting and filtering happen in memory over the already-loaded entries --
// `view` holds the indices into `entries` after filter then sort, so
// neither needs a round trip to SQLite.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::tty::IsTty;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use rusqlite::Connection;

use crate::db::{self, Filter};
use crate::models::Entry;

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

// ---------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------

pub fn run(conn: &Connection) -> Result<(), String> {
    if !std::io::stdout().is_tty() {
        return Err("ferref tui requires an interactive terminal (stdout is not a tty)".into());
    }

    let mut app = App::load(conn).map_err(|e| format!("failed to load library: {e}"))?;

    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, conn);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    conn: &Connection,
) -> Result<(), String> {
    loop {
        terminal
            .draw(|frame| draw(frame, app))
            .map_err(|e| e.to_string())?;

        match event::read().map_err(|e| e.to_string())? {
            Event::Key(key) => {
                // On Windows every key produces both press and release;
                // acting on both would make every keystroke fire twice.
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(());
                }
                handle_key(app, conn, terminal, key.code, key.modifiers);
                if app.should_quit {
                    return Ok(());
                }
            }
            // Resize just needs a redraw, which happens at the top of the loop.
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

// Dispatches on mode FIRST, before any key is interpreted as a command --
// the one rule that keeps a search query like "query" from also quitting at
// 'q' or reloading at 'r' along the way.
fn handle_key(
    app: &mut App,
    conn: &Connection,
    terminal: &mut ratatui::DefaultTerminal,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    // A status message is shown for exactly one frame: whatever key
    // dismisses it also clears it, so it can't linger over unrelated
    // activity.
    app.status = None;

    // Every mode past Normal carries data a handler needs to own (a text
    // buffer, a picker's rows, ...), so it has to come out of `app.mode`
    // before a handler can touch it -- one mem::replace here, rather than
    // one per handler each re-deriving "and put it back if this wasn't
    // really my variant" (which can't actually happen: this match already
    // established which variant it is).
    match std::mem::replace(&mut app.mode, Mode::Normal) {
        Mode::Normal => handle_normal_key(app, conn, code, modifiers),
        Mode::Input(kind, buffer) => handle_input_key(app, conn, code, kind, buffer),
        Mode::Picker {
            rows,
            selected,
            member,
            entry_id,
            bulk,
        } => handle_picker_key(app, conn, code, rows, selected, member, entry_id, bulk),
        Mode::Command { entry_id } => handle_command_key(app, conn, terminal, code, entry_id),
        Mode::FieldPicker { entry_id, selected } => {
            handle_field_picker_key(app, code, entry_id, selected)
        }
        Mode::EntryPicker {
            keep_id,
            candidates,
            within_marked,
            filter,
            rows,
            selected,
        } => handle_entry_picker_key(
            app,
            conn,
            code,
            keep_id,
            candidates,
            within_marked,
            filter,
            rows,
            selected,
        ),
        Mode::Confirm { action, .. } => handle_confirm_key(app, conn, code, action),
        // any key closes it; app.mode is already Normal from the replace above
        Mode::Help => {}
        Mode::FileBrowser {
            entry_id,
            cwd,
            entries,
            selected,
        } => handle_file_browser_key(app, conn, code, entry_id, cwd, entries, selected),
    }
}

fn handle_normal_key(app: &mut App, conn: &Connection, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Char('q') => app.should_quit = true,
        // Esc clears an active filter and any merge marks rather than
        // quitting, so backing out of a search or a mark-in-progress
        // doesn't also close the app.
        KeyCode::Esc => {
            if app.filter.is_empty() && app.marked.is_empty() {
                app.should_quit = true;
            } else {
                app.filter.clear();
                app.marked.clear();
                app.rebuild_view();
                app.details_scroll = 0;
            }
        }
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('r') => {
            // A failed reload leaves the previous state in place rather
            // than crashing the session over a transient DB error.
            if let Err(e) = app.reload(conn) {
                app.status = Some(e);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Focus::Collections => app.move_tree(conn, -1),
            Focus::Entries => app.move_table(-1),
            Focus::Details => app.scroll_details(-1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Focus::Collections => app.move_tree(conn, 1),
            Focus::Entries => app.move_table(1),
            Focus::Details => app.scroll_details(1),
        },
        KeyCode::Char('d')
            if modifiers.contains(KeyModifiers::CONTROL)
                && matches!(app.focus, Focus::Entries | Focus::Details) =>
        {
            match app.focus {
                Focus::Entries => app.move_table(10),
                Focus::Details => app.scroll_details(10),
                Focus::Collections => {}
            }
        }
        KeyCode::Char('u')
            if modifiers.contains(KeyModifiers::CONTROL)
                && matches!(app.focus, Focus::Entries | Focus::Details) =>
        {
            match app.focus {
                Focus::Entries => app.move_table(-10),
                Focus::Details => app.scroll_details(-10),
                Focus::Collections => {}
            }
        }
        KeyCode::Char('g') => match app.focus {
            Focus::Collections => app.tree_top(conn),
            Focus::Entries => app.table_home(),
            Focus::Details => app.details_scroll = 0,
        },
        KeyCode::Char('G') => match app.focus {
            Focus::Collections => app.tree_bottom(conn),
            Focus::Entries => app.table_end(),
            // The true bottom depends on the pane's actual rendered width
            // (line-wrapping), which this key handler doesn't have -- draw_details
            // clamps whatever's stored here down to the real max at render time,
            // so a large sentinel always lands exactly at the bottom.
            Focus::Details => app.details_scroll = u16::MAX,
        },
        KeyCode::Left | KeyCode::Char('h') if app.focus == Focus::Collections => {
            app.collapse_or_to_parent(conn)
        }
        KeyCode::Right | KeyCode::Char('l') if app.focus == Focus::Collections => app.expand(),
        KeyCode::Char('h') if matches!(app.focus, Focus::Entries | Focus::Details) => {
            app.focus = app.focus.left();
        }
        KeyCode::Char('l') if matches!(app.focus, Focus::Entries | Focus::Details) => {
            app.focus = app.focus.right();
        }
        KeyCode::PageUp if app.focus == Focus::Entries => app.move_table(-10),
        KeyCode::PageDown if app.focus == Focus::Entries => app.move_table(10),
        KeyCode::Home if app.focus == Focus::Entries => app.table_home(),
        KeyCode::End if app.focus == Focus::Entries => app.table_end(),
        KeyCode::Char('s') => {
            app.sort_key = app.sort_key.next();
            app.rebuild_view();
            // Re-sorting can put a different entry at the same table
            // position `table_selected` still points at -- it's an index
            // into `view`, not an entry id -- so a Details-pane scroll
            // position from the entry that *used* to be there must not
            // silently carry over onto whatever landed there instead.
            app.details_scroll = 0;
        }
        KeyCode::Char('S') => {
            app.sort_desc = !app.sort_desc;
            app.rebuild_view();
            app.details_scroll = 0; // see the "s" arm above
        }
        KeyCode::Char('/') => {
            app.mode = Mode::Input(
                InputKind::Search {
                    previous: app.filter.clone(),
                },
                app.filter.clone(),
            );
        }
        KeyCode::Char('n') if app.focus == Focus::Collections => {
            app.mode = Mode::Input(InputKind::NewCollection, String::new());
        }
        // "R"/"D": rename/delete the highlighted collection. Capitals, and
        // deliberately not "r"/"d" -- "r" is already reload, and a delete
        // key shouldn't be one slip away from a common one. Both are
        // no-ops on the "All Papers" row (see App::begin_rename_collection
        // and App::begin_delete_collection).
        KeyCode::Char('R') if app.focus == Focus::Collections => app.begin_rename_collection(),
        KeyCode::Char('D') if app.focus == Focus::Collections => app.begin_delete_collection(),
        KeyCode::Char('c') if app.focus == Focus::Entries => app.open_picker(conn),
        KeyCode::Char('o') if matches!(app.focus, Focus::Entries | Focus::Details) => {
            app.open_selected(conn);
        }
        KeyCode::Char('y') if matches!(app.focus, Focus::Entries | Focus::Details) => {
            app.copy_url();
        }
        // Export the marked set (or just the selected entry, with nothing
        // marked) as BibTeX. Doesn't touch `app.marked` -- export doesn't
        // consume the set the way merge/delete's confirm does, so the same
        // marks can still be filed into a collection afterward.
        KeyCode::Char('x')
            if matches!(app.focus, Focus::Entries | Focus::Details) && !app.view.is_empty() =>
        {
            if let Some(entry_id) = app.selected_entry().and_then(|e| e.id) {
                let ids = app.bulk_targets(entry_id);
                app.mode = Mode::Input(InputKind::ExportPath { ids }, "export.bib".to_string());
            }
        }
        // "x" in the tree pane exports the *highlighted collection* --
        // every entry already loaded for it (self.entries is always that
        // row's recursive set, per load_entries/select_row), not the
        // marked/selected-entry set the Entries-pane "x" above uses. No
        // marking required first, which is the whole point: marking every
        // paper in a collection by hand was the friction this was added to
        // remove. The filename defaults to the collection's own name
        // (sanitized the same way attachment filenames are); "All Papers"
        // (the synthetic root, `id: None`) has no name worth using as a
        // filename, so it falls back to the same "export.bib" default the
        // Entries-pane export uses.
        KeyCode::Char('x') if app.focus == Focus::Collections && !app.entries.is_empty() => {
            let ids: Vec<i64> = app.entries.iter().filter_map(|e| e.id).collect();
            let filename = export_filename_for_row(&app.rows[app.selected_row]);
            app.mode = Mode::Input(InputKind::ExportPath { ids }, filename);
        }
        KeyCode::Char('?') => app.mode = Mode::Help,
        // Toggles the current row into the merge marks. Insertion order
        // matters (first marked survives a merge, second is folded in and
        // deleted) -- see App::toggle_mark.
        KeyCode::Char(' ') if app.focus == Focus::Entries => app.toggle_mark(),
        // "A": marks every row currently visible (respecting the active "/"
        // filter), in addition to whatever's already marked -- doesn't
        // clear or replace existing marks, since a mark is deliberately
        // meant to survive a collection change (see select_row's own
        // comment on App::marked) and a bulk "mark everything I can see"
        // shouldn't undo a cross-collection selection already in progress.
        KeyCode::Char('A') if app.focus == Focus::Entries => app.mark_all_visible(),
        // "U": clears every mark, regardless of focus -- the direct undo
        // for "A"/Space, without also touching the search filter or
        // quitting the way Esc does when both happen to already be empty.
        // Not gated on `Focus::Entries`: marks (unlike making one) are
        // useful to back out of from wherever you ended up, e.g. after
        // tabbing to Collections to file a marked set and changing your
        // mind before pressing "c".
        KeyCode::Char('U') => app.marked.clear(),
        // The ":" command palette (Edit/Fetch/Merge/Delete), scoped to
        // whichever entry is currently selected.
        KeyCode::Char(':')
            if matches!(app.focus, Focus::Entries | Focus::Details) && !app.view.is_empty() =>
        {
            if let Some(entry_id) = app.selected_entry().and_then(|e| e.id) {
                app.mode = Mode::Command { entry_id };
            }
        }
        _ => {}
    }
}

// `kind`/`buffer` come in owned (handle_key already took them out of
// app.mode) so the match arms below can freely call back into `app`
// (reload, rebuild_view) without fighting the borrow checker over a field
// that's simultaneously borrowed and being written back to.
fn handle_input_key(
    app: &mut App,
    conn: &Connection,
    code: KeyCode,
    kind: InputKind,
    mut buffer: String,
) {
    match code {
        KeyCode::Char(c) => {
            buffer.push(c);
            if matches!(kind, InputKind::Search { .. }) {
                app.filter = buffer.clone();
                app.rebuild_view();
                app.details_scroll = 0;
            }
            app.mode = Mode::Input(kind, buffer);
        }
        KeyCode::Backspace => {
            buffer.pop();
            if matches!(kind, InputKind::Search { .. }) {
                app.filter = buffer.clone();
                app.rebuild_view();
                app.details_scroll = 0;
            }
            app.mode = Mode::Input(kind, buffer);
        }
        KeyCode::Enter => {
            match kind {
                InputKind::NewCollection => {
                    let name = buffer.trim().to_string();
                    if !name.is_empty() {
                        let parent = app.rows[app.selected_row].id;
                        app.create_collection(conn, parent, &name);
                    }
                    // Search: the filter was already applied live as it was typed.
                    app.mode = Mode::Normal;
                }
                InputKind::RenameCollection { id } => {
                    let name = buffer.trim().to_string();
                    if !name.is_empty() {
                        app.rename_collection(conn, id, &name);
                    }
                    app.mode = Mode::Normal;
                }
                InputKind::Search { .. } => {
                    app.mode = Mode::Normal;
                }
                InputKind::EditField {
                    entry_id,
                    field,
                    return_selected,
                } => {
                    app.apply_field_edit(conn, entry_id, field, &buffer);
                    app.mode = Mode::FieldPicker {
                        entry_id,
                        selected: return_selected,
                    };
                }
                InputKind::ExportPath { ids } => {
                    app.export_bibtex(conn, &ids, buffer.trim());
                    app.mode = Mode::Normal;
                }
                InputKind::Tag { ids, add } => {
                    app.apply_tag(conn, &ids, buffer.trim(), add);
                    app.mode = Mode::Normal;
                }
            }
        }
        KeyCode::Esc => {
            match kind {
                InputKind::Search { previous } => {
                    app.filter = previous;
                    app.rebuild_view();
                    app.details_scroll = 0;
                    app.mode = Mode::Normal;
                }
                InputKind::NewCollection
                | InputKind::RenameCollection { .. }
                | InputKind::ExportPath { .. }
                | InputKind::Tag { .. } => {
                    app.mode = Mode::Normal;
                }
                // Esc on a field edit backs out to the field picker without
                // saving, not all the way to Normal -- Enter is the only
                // way this input box writes anything.
                InputKind::EditField {
                    entry_id,
                    return_selected,
                    ..
                } => {
                    app.mode = Mode::FieldPicker {
                        entry_id,
                        selected: return_selected,
                    };
                }
            }
        }
        _ => {
            app.mode = Mode::Input(kind, buffer);
        }
    }
}

// One parameter per Mode::Picker field (handle_key already took ownership
// of them out of app.mode before calling this) plus the entry-relevant
// context Enter needs.
#[allow(clippy::too_many_arguments)]
fn handle_picker_key(
    app: &mut App,
    conn: &Connection,
    code: KeyCode,
    rows: Vec<(usize, i64, String)>,
    mut selected: usize,
    mut member: HashSet<i64>,
    entry_id: i64,
    bulk: Option<Vec<i64>>,
) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.marked.clear();
            return; // app.mode is already Normal
        }
        KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if selected + 1 < rows.len() {
                selected += 1;
            }
        }
        KeyCode::Enter => {
            let (_, collection_id, collection_name) = rows[selected].clone();
            if let Some(ids) = &bulk {
                let mut filed = 0usize;
                let mut err = None;
                for &id in ids {
                    match db::add_entry_to_collection(conn, collection_id, id) {
                        Ok(changed) => filed += changed as usize,
                        Err(e) => err = Some(e.to_string()),
                    }
                }
                match err {
                    Some(e) => app.status = Some(e),
                    None => {
                        member.insert(collection_id); // flips to [x]: confirms this row was filed into
                        app.status = Some(format!(
                            "Filed {filed} of {} marked entr{} into '{collection_name}'",
                            ids.len(),
                            entries_plural(ids.len())
                        ));
                    }
                }
                if let Err(e) = app.reload_tree_counts(conn) {
                    app.status = Some(e);
                }
            } else {
                let is_member = member.contains(&collection_id);
                let result = if is_member {
                    db::remove_entry_from_collection(conn, collection_id, entry_id)
                } else {
                    db::add_entry_to_collection(conn, collection_id, entry_id)
                };
                match result {
                    Ok(_) => {
                        if is_member {
                            member.remove(&collection_id);
                        } else {
                            member.insert(collection_id);
                        }
                        // Membership changed a collection's entry_count; the
                        // tree pane's counts need to catch up.
                        if let Err(e) = app.reload_tree_counts(conn) {
                            app.status = Some(e);
                        }
                    }
                    Err(e) => app.status = Some(e.to_string()),
                }
            }
        }
        _ => {}
    }

    app.mode = Mode::Picker {
        rows,
        selected,
        member,
        entry_id,
        bulk,
    };
}

// The ":" palette: Edit / Fetch / Merge / Delete, scoped to whichever entry
// was selected when it opened.
fn handle_command_key(
    app: &mut App,
    conn: &Connection,
    terminal: &mut ratatui::DefaultTerminal,
    code: KeyCode,
    entry_id: i64,
) {
    match code {
        KeyCode::Esc => {} // app.mode is already Normal
        KeyCode::Char('e') => {
            app.mode = Mode::FieldPicker {
                entry_id,
                selected: 0,
            };
        }
        KeyCode::Char('f') => app.fetch_selected(conn, terminal, entry_id),
        KeyCode::Char('m') => app.begin_merge(conn, entry_id),
        KeyCode::Char('d') => app.begin_delete(entry_id),
        KeyCode::Char('t') => {
            let ids = app.bulk_targets(entry_id);
            app.mode = Mode::Input(InputKind::Tag { ids, add: true }, String::new());
        }
        KeyCode::Char('u') => {
            let ids = app.bulk_targets(entry_id);
            app.mode = Mode::Input(InputKind::Tag { ids, add: false }, String::new());
        }
        KeyCode::Char('a') => app.begin_attach(entry_id),
        _ => app.mode = Mode::Command { entry_id },
    }
}

// Edit's field-name picker. Enter opens Mode::Input pre-filled with the
// field's current value; Esc backs out to Normal.
fn handle_field_picker_key(app: &mut App, code: KeyCode, entry_id: i64, mut selected: usize) {
    match code {
        KeyCode::Esc => {} // app.mode is already Normal
        KeyCode::Up | KeyCode::Char('k') => {
            selected = selected.saturating_sub(1);
            app.mode = Mode::FieldPicker { entry_id, selected };
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if selected + 1 < EditField::ALL.len() {
                selected += 1;
            }
            app.mode = Mode::FieldPicker { entry_id, selected };
        }
        KeyCode::Enter => {
            let Some(entry) = app.entry_by_id(entry_id) else {
                return;
            };
            let field = EditField::ALL[selected];
            let initial = field.current_value(entry);
            app.mode = Mode::Input(
                InputKind::EditField {
                    entry_id,
                    field,
                    return_selected: selected,
                },
                initial,
            );
        }
        _ => app.mode = Mode::FieldPicker { entry_id, selected },
    }
}

// Merge's fold-in-entry picker: types narrow `filter`, Up/Down move the
// selection (not j/k -- both are ordinary letters someone might filter by,
// and this picker has a live text box the collection picker doesn't).
#[allow(clippy::too_many_arguments)] // one per Mode::EntryPicker field, plus conn
fn handle_entry_picker_key(
    app: &mut App,
    conn: &Connection,
    code: KeyCode,
    keep_id: i64,
    candidates: Vec<Entry>,
    within_marked: bool,
    mut filter: String,
    mut rows: Vec<usize>,
    mut selected: usize,
) {
    match code {
        KeyCode::Esc => {
            app.marked.clear();
            return; // app.mode is already Normal
        }
        KeyCode::Up => selected = selected.saturating_sub(1),
        KeyCode::Down => {
            if selected + 1 < rows.len() {
                selected += 1;
            }
        }
        KeyCode::Backspace => {
            filter.pop();
            rows = entry_picker_rows(&candidates, keep_id, &filter);
            selected = 0;
        }
        KeyCode::Char(c) => {
            filter.push(c);
            rows = entry_picker_rows(&candidates, keep_id, &filter);
            selected = 0;
        }
        KeyCode::Enter => {
            if let Some(drop_id) = rows.get(selected).and_then(|&i| candidates[i].id) {
                app.confirm_merge(conn, keep_id, drop_id);
                return;
            }
        }
        _ => {}
    }

    app.mode = Mode::EntryPicker {
        keep_id,
        candidates,
        within_marked,
        filter,
        rows,
        selected,
    };
}

// Attach's directory browser: j/k/g/G move, l/Enter descends a directory or
// attaches a file (extraction always on -- matches fetch's TUI behavior),
// h/Backspace goes to the parent (a no-op at filesystem root), Esc cancels.
// A directory that fails to list is a footer error, not a crash or a lost
// listing -- cwd/entries/selected stay exactly as they were before the
// failed attempt.
fn handle_file_browser_key(
    app: &mut App,
    conn: &Connection,
    code: KeyCode,
    entry_id: i64,
    mut cwd: PathBuf,
    mut entries: Vec<BrowserEntry>,
    mut selected: usize,
) {
    match code {
        KeyCode::Esc => return, // app.mode is already Normal
        KeyCode::Char('j') | KeyCode::Down => {
            selected = (selected + 1).min(entries.len().saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => selected = selected.saturating_sub(1),
        KeyCode::Char('g') => selected = 0,
        KeyCode::Char('G') => selected = entries.len().saturating_sub(1),
        KeyCode::Char('l') | KeyCode::Enter => {
            if let Some(entry) = entries.get(selected) {
                if entry.is_dir {
                    match list_dir(&entry.path) {
                        Ok(new_entries) => {
                            cwd = entry.path.clone();
                            entries = new_entries;
                            selected = 0;
                        }
                        Err(e) => app.status = Some(e),
                    }
                } else {
                    let Some(cite_key) = app.entry_by_id(entry_id).map(|e| e.cite_key.clone())
                    else {
                        return;
                    };
                    match crate::attach_path_for_entry(conn, &cite_key, &entry.path, true) {
                        Ok(outcome) => {
                            let mut msg = if outcome.changed {
                                format!("Attached '{}' to '{}'", outcome.path, cite_key)
                            } else {
                                format!("'{}' already has '{}'", cite_key, outcome.path)
                            };
                            if let Some(extraction) = &outcome.extraction {
                                msg.push_str(&match extraction {
                                    Ok(chars) => format!(" ({chars} chars extracted)"),
                                    Err(e) => format!(", but extraction failed: {e}"),
                                });
                            }
                            app.status = Some(msg);
                            if let Err(e) = app.refresh_entry(conn, entry_id) {
                                app.status = Some(e);
                            }
                        }
                        Err(e) => app.status = Some(e),
                    }
                    return; // back to Mode::Normal
                }
            }
        }
        KeyCode::Char('h') | KeyCode::Backspace => {
            if let Some(parent) = cwd.parent() {
                match list_dir(parent) {
                    Ok(new_entries) => {
                        cwd = parent.to_path_buf();
                        entries = new_entries;
                        selected = 0;
                    }
                    Err(e) => app.status = Some(e),
                }
            }
        }
        _ => {}
    }

    app.mode = Mode::FileBrowser {
        entry_id,
        cwd,
        entries,
        selected,
    };
}

// Delete/Merge confirm: any key but 'y' cancels. Marks are cleared either
// way, per DESIGN.md's Phase 16 merge rule.
fn handle_confirm_key(app: &mut App, conn: &Connection, code: KeyCode, action: PendingAction) {
    if code != KeyCode::Char('y') {
        app.marked.clear();
        return; // app.mode is already Normal
    }

    // DeleteCollection is handled separately: it manages its own post-
    // delete selection (parent, or the nearest remaining row -- see
    // App::finish_delete_collection), unlike Delete/Merge below, which
    // both always land back on the same selected collection via the
    // generic app.reload(). Marks are cleared here too, same as the other
    // two -- harmless, since marks hold entry ids and deleting a
    // collection never deletes an entry, but there's no reason for this
    // one action to behave differently from a cancelled confirm above.
    if let PendingAction::DeleteCollection { id, parent_id } = action {
        app.marked.clear();
        app.finish_delete_collection(conn, id, parent_id);
        return;
    }

    let result = match action {
        PendingAction::Delete { entry_id } => app
            .entry_by_id(entry_id)
            .map(|e| e.cite_key.clone())
            .ok_or_else(|| "entry no longer exists".to_string())
            .and_then(|cite_key| db::delete_entry(conn, &cite_key).map_err(|e| e.to_string())),
        PendingAction::Merge { keep_id, drop_id } => db::merge_entries(conn, keep_id, drop_id)
            .map_err(|e| crate::friendly(None, "merge entries", e)),
        PendingAction::DeleteCollection { .. } => unreachable!("handled above"),
    };
    if let Err(e) = result {
        app.status = Some(e);
    }

    app.marked.clear();
    // The entry list's shape changed (a row is gone) either way -- reload
    // rather than patch app.entries in place.
    if let Err(e) = app.reload(conn) {
        app.status = Some(e);
    }
}

// ---------------------------------------------------------------------
// State
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Collections,
    Entries,
    Details,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Focus::Collections => Focus::Entries,
            Focus::Entries => Focus::Details,
            Focus::Details => Focus::Collections,
        }
    }
    fn prev(self) -> Self {
        match self {
            Focus::Collections => Focus::Details,
            Focus::Entries => Focus::Collections,
            Focus::Details => Focus::Entries,
        }
    }
    // Bounded, not cyclic: h/l in Entries/Details reads as "move to the
    // pane in that physical direction", and there's no pane to the right
    // of Details or to the left of Collections to wrap to.
    fn left(self) -> Self {
        match self {
            Focus::Collections => Focus::Collections,
            Focus::Entries => Focus::Collections,
            Focus::Details => Focus::Entries,
        }
    }
    fn right(self) -> Self {
        match self {
            Focus::Collections => Focus::Entries,
            Focus::Entries => Focus::Details,
            Focus::Details => Focus::Details,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Title,
    Author,
    Year,
    Journal,
}

impl SortKey {
    fn next(self) -> Self {
        match self {
            SortKey::Title => SortKey::Author,
            SortKey::Author => SortKey::Year,
            SortKey::Year => SortKey::Journal,
            SortKey::Journal => SortKey::Title,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SortKey::Title => "title",
            SortKey::Author => "author",
            SortKey::Year => "year",
            SortKey::Journal => "journal",
        }
    }
}

// Normal mode is the only one where a keystroke is a command; the other two
// modes eat every printable character into a buffer (see the module-level
// "TRAP" note in DESIGN.md's Phase 12 section -- typing "query" must not
// also quit at 'q' or reload at 'r').
enum Mode {
    Normal,
    Input(InputKind, String),
    Picker {
        // (depth, collection id, name), same shape draw_tree renders from.
        rows: Vec<(usize, i64, String)>,
        selected: usize,
        member: HashSet<i64>,
        entry_id: i64,
        // Set when opened with marked entries (see App::open_picker):
        // Enter files every id here into the chosen collection, add-only,
        // instead of toggling `entry_id`'s own membership. `member` stays
        // empty in this mode -- there's no single well-defined checked
        // state for a mixed set, so a row only flips to `[x]` once this
        // session has actually filed the set into it.
        bulk: Option<Vec<i64>>,
    },
    // The ":" palette (Edit/Fetch/Merge/Delete), scoped to one entry.
    Command {
        entry_id: i64,
    },
    // Edit's field-name list, opened by ":" -> "e".
    FieldPicker {
        entry_id: i64,
        selected: usize,
    },
    // Merge's fold-in-entry picker, opened by ":" -> "m". `keep_id` is fixed
    // for the picker's lifetime; `candidates` is resolved once, at open time
    // (App::begin_merge), from either `App::entries` or the DB -- owned here
    // rather than indices into `App::entries`, so a marked entry from
    // another collection never has to be (and, per T1, never is) appended
    // there just to make it visible to the picker. `rows` are indices into
    // `candidates` matching `filter`. `within_marked` is display-only (3+
    // marked -- see MergePlan): whether candidates were narrowed to the
    // marked subset instead of the whole library, for the popup's title.
    EntryPicker {
        keep_id: i64,
        candidates: Vec<Entry>,
        within_marked: bool,
        filter: String,
        rows: Vec<usize>,
        selected: usize,
    },
    // Delete/merge confirmation. An enum of pending actions (rather than a
    // boxed closure) since there are exactly two call sites.
    Confirm {
        message: String,
        action: PendingAction,
    },
    // "?": the full keymap reference. Static content, so no fields -- any
    // key closes it.
    Help,
    // Attach's directory browser, opened by ":" -> "a". `cwd` starts at
    // $HOME (see App::begin_attach); `entries` is `list_dir(&cwd)`'s
    // listing, refreshed on every descend/ascend.
    FileBrowser {
        entry_id: i64,
        cwd: PathBuf,
        entries: Vec<BrowserEntry>,
        selected: usize,
    },
}

struct BrowserEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

// A sorted, dotfile-filtered directory listing for Mode::FileBrowser:
// directories first, then files, both alphabetical case-insensitively.
// `file_type()` (not `path.is_dir()`) decides dir-vs-file so a broken
// symlink or a permission-denied stat can't panic; an entry that errors on
// `file_type()` is skipped rather than failing the whole listing, since one
// bad entry in a directory shouldn't hide every other entry.
fn list_dir(dir: &Path) -> Result<Vec<BrowserEntry>, String> {
    let read = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to read '{}': {e}", dir.display()))?;

    let mut entries: Vec<BrowserEntry> = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        // DirEntry::file_type() uses lstat semantics: a symlink is never
        // "a directory" by its own report, even one pointing at a real
        // directory -- common under $HOME (a papers folder symlinked from
        // another mount), and without this the browser could never descend
        // into one. metadata() follows the link to classify it correctly;
        // a broken link or a symlink loop makes that error, which is
        // treated as "not a directory" rather than propagated -- the
        // file-attach path already produces a clean error for an
        // unreadable/vanished path, so this just defers to that instead of
        // failing the whole listing over one bad entry.
        let is_dir = if file_type.is_symlink() {
            std::fs::metadata(item.path())
                .map(|m| m.is_dir())
                .unwrap_or(false)
        } else {
            file_type.is_dir()
        };
        entries.push(BrowserEntry {
            name,
            path: item.path(),
            is_dir,
        });
    }

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(entries)
}

enum InputKind {
    // Carries the filter that was active before '/' was pressed, so Esc can
    // restore it rather than just clearing it.
    Search {
        previous: String,
    },
    NewCollection,
    // "R": pre-filled with the collection's current name.
    RenameCollection {
        id: i64,
    },
    // Edit's value box. `return_selected` is the field picker's row to
    // return to on Enter/Esc, so fixing several fields in one visit doesn't
    // reset the list to the top each time.
    EditField {
        entry_id: i64,
        field: EditField,
        return_selected: usize,
    },
    // "x": the output path for a BibTeX export of `ids` (the marked set,
    // or just the selected entry when nothing's marked).
    ExportPath {
        ids: Vec<i64>,
    },
    // ":" -> "t"/"u": the tag name to add to (add: true) or remove from
    // (add: false) every id in `ids` -- same marked-or-selected target rule
    // as export.
    Tag {
        ids: Vec<i64>,
        add: bool,
    },
}

#[derive(Clone, Copy)]
enum PendingAction {
    Delete { entry_id: i64 },
    Merge { keep_id: i64, drop_id: i64 },
    // "D" in the Collections pane. `parent_id` is captured at confirm time
    // (see App::begin_delete_collection), not re-derived afterward -- once
    // the collection is deleted there's nothing left to ask its parent id.
    DeleteCollection {
        id: i64,
        parent_id: Option<i64>,
    },
}

// Field names Edit (":" -> "e") can change: Entry's own scalar columns plus
// authors (whole-list replace, the same semantics `ferref edit --author`
// already has). Tags aren't here -- they're not an `entries` column, and
// DESIGN.md's Phase 16 section lists tagging from the TUI as out of scope.
#[derive(Clone, Copy, PartialEq)]
enum EditField {
    Title,
    Year,
    Journal,
    Volume,
    Pages,
    Doi,
    Url,
    Abstract,
    Authors,
}

impl EditField {
    const ALL: [EditField; 9] = [
        EditField::Title,
        EditField::Year,
        EditField::Journal,
        EditField::Volume,
        EditField::Pages,
        EditField::Doi,
        EditField::Url,
        EditField::Abstract,
        EditField::Authors,
    ];

    fn label(self) -> &'static str {
        match self {
            EditField::Title => "title",
            EditField::Year => "year",
            EditField::Journal => "journal",
            EditField::Volume => "volume",
            EditField::Pages => "pages",
            EditField::Doi => "doi",
            EditField::Url => "url",
            EditField::Abstract => "abstract",
            EditField::Authors => "authors",
        }
    }

    // Pre-fills the Input box with the field's string form as it stands now,
    // so an unchanged Enter is a no-op rather than blanking the field.
    fn current_value(self, e: &Entry) -> String {
        match self {
            EditField::Title => e.title.clone(),
            EditField::Year => e.year.map(|y| y.to_string()).unwrap_or_default(),
            EditField::Journal => e.journal.clone().unwrap_or_default(),
            EditField::Volume => e.volume.clone().unwrap_or_default(),
            EditField::Pages => e.pages.clone().unwrap_or_default(),
            EditField::Doi => e.doi.clone().unwrap_or_default(),
            EditField::Url => e.url.clone().unwrap_or_default(),
            EditField::Abstract => e.abstract_text.clone().unwrap_or_default(),
            EditField::Authors => crate::format_authors(&e.authors),
        }
    }

    // Applies the edited text onto a clone of the current entry --
    // db::update_entry replaces every scalar column at once, so every field
    // this isn't editing has to already be sitting on `entry` untouched.
    fn apply(self, entry: &mut Entry, raw: &str) -> Result<(), String> {
        let trimmed = raw.trim();
        match self {
            EditField::Title => {
                if trimmed.is_empty() {
                    return Err("title cannot be empty".to_string());
                }
                entry.title = trimmed.to_string();
            }
            EditField::Year => {
                entry.year = if trimmed.is_empty() {
                    None
                } else {
                    Some(
                        trimmed
                            .parse::<i32>()
                            .map_err(|_| "year must be a whole number".to_string())?,
                    )
                };
            }
            EditField::Journal => {
                entry.journal = (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            EditField::Volume => entry.volume = (!trimmed.is_empty()).then(|| trimmed.to_string()),
            EditField::Pages => entry.pages = (!trimmed.is_empty()).then(|| trimmed.to_string()),
            EditField::Doi => entry.doi = (!trimmed.is_empty()).then(|| trimmed.to_string()),
            EditField::Url => entry.url = (!trimmed.is_empty()).then(|| trimmed.to_string()),
            EditField::Abstract => {
                entry.abstract_text = (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            EditField::Authors => {
                entry.authors = if trimmed.is_empty() {
                    Vec::new()
                } else {
                    trimmed
                        .split(';')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(crate::cli::parse_author)
                        .collect::<Result<Vec<_>, String>>()?
                };
            }
        }
        Ok(())
    }
}

// The ":" -> "m" branching rule from DESIGN.md's Phase 16 section, factored
// out as pure logic over `&[i64]` so it's testable without a live App.
#[derive(Debug, PartialEq)]
enum MergePlan {
    // 0 or 1 marked: the selected entry (carried here) is the keeper: open
    // a picker, over the whole library, to choose what folds into it.
    PickDrop(i64),
    // Exactly 2 marked: order already decides keep/drop.
    Pair(i64, i64),
    // 3+ marked: the first-marked entry is the keeper, and the picker's
    // candidates are narrowed to the rest of the marked set (not the whole
    // library) -- marking is how you say "these are the ones I actually
    // mean," so a merge started from a marked subset shouldn't fall back to
    // searching everything.
    PickDropWithin(i64, Vec<i64>),
}

fn plan_merge(marked: &[i64], selected_id: Option<i64>) -> Option<MergePlan> {
    match marked.len() {
        0 | 1 => selected_id.map(MergePlan::PickDrop),
        2 => Some(MergePlan::Pair(marked[0], marked[1])),
        _ => Some(MergePlan::PickDropWithin(marked[0], marked[1..].to_vec())),
    }
}

// Space (Entries, Normal): toggles `id` into `marked`. A `Vec`, not a
// `HashSet` -- insertion order is the whole point, since the first entry
// marked is the merge survivor and the second is folded in and deleted.
// Marking the same id twice unmarks it.
fn toggle_marked(marked: &mut Vec<i64>, id: i64) {
    if let Some(pos) = marked.iter().position(|&m| m == id) {
        marked.remove(pos);
    } else {
        marked.push(id);
    }
}

// One row of the rendered tree, including the synthetic "All Papers" root
// (id = None) that db::collection_tree never produces -- it isn't a DB row,
// it means "no collection filter".
struct TreeRow {
    // The Filter::collection_id to fetch this row's entries with; None means
    // "All Papers", i.e. no collection filter at all.
    id: Option<i64>,
    depth: usize,
    name: String,
    // Recursive: this collection plus its descendants. See load_tree.
    entry_count: i64,
}

// The default filename the tree pane's "x" (export the highlighted
// collection) pre-fills. A named collection gets its own name, sanitized
// the same way attachment filenames are (rejects nothing here -- a
// collection's `name` is never empty by construction, so sanitize_filename
// can't return the reject case, only remap unsafe characters). The
// synthetic "All Papers" root (`id: None`) has no name worth using as a
// filename, so it falls back to the same "export.bib" default the
// Entries-pane export already uses.
fn export_filename_for_row(row: &TreeRow) -> String {
    row.id
        .and_then(|_| crate::doi::sanitize_filename(&row.name).ok())
        .map(|n| format!("{n}.bib"))
        .unwrap_or_else(|| "export.bib".to_string())
}

// Positionally aligned with entry.attachments (both ORDER BY id): index i
// here is the length for e.attachments[i]. No path stored -- that's already
// on the Attachment itself, and nothing here ever read a second copy of it.
type AttachmentLengths = HashMap<i64, Vec<Option<i64>>>;

struct App {
    rows: Vec<TreeRow>,
    collapsed: HashSet<Option<i64>>,
    selected_row: usize, // index into `rows` (not the visible subset)

    entries: Vec<Entry>,
    // Indices into `entries`, after filter then sort. The table's row index
    // is an index into THIS, never into `entries` directly.
    view: Vec<usize>,
    table_selected: usize, // index into `view`
    // Lines scrolled down in the DETAILS pane, for a long abstract that
    // doesn't fit. Reset to 0 wherever the *selected entry* changes
    // (move_table/table_home/table_end/select_row/rebuild_view) so a fresh
    // paper always opens at its top rather than wherever the last one left
    // off. Clamped to the real bottom at render time (see draw_details) --
    // not here, since the true max depends on the pane's rendered width
    // (line-wrapping), which this field's own writers don't have.
    details_scroll: u16,
    // entry id -> [(attachment path, extracted-text char length)], loaded
    // alongside `entries` so the details pane never queries during render.
    attachment_lengths: AttachmentLengths,

    filter: String,
    sort_key: SortKey,
    sort_desc: bool,

    // Entries marked for merge (Space, Entries pane). Insertion-ordered:
    // see toggle_marked.
    marked: Vec<i64>,

    focus: Focus,
    mode: Mode,
    // The footer's one-line message slot: an error, a confirmation, or an
    // in-progress notice ("Fetching…", "Copied ...", "Filed N of M..."),
    // shown for exactly one keypress, then cleared by handle_key.
    status: Option<String>,
    should_quit: bool,

    // "y"'s clipboard handle, created once and held for the app's whole
    // lifetime -- not per-copy. arboard's X11 backend serves the clipboard
    // from a background thread owned by this handle; a Clipboard created
    // and dropped inside one keypress hands the data off to a system
    // clipboard manager on drop, which silently loses it if no manager is
    // running (verified directly: exactly this shape of bug, caught before
    // shipping -- see DESIGN.md's Phase 18). None if creation failed (no
    // display, unsupported platform, ...); "y" reports that as an error
    // rather than panicking or retrying every keypress.
    clipboard: Option<arboard::Clipboard>,
}

impl App {
    fn load(conn: &Connection) -> Result<Self, String> {
        let rows = load_tree(conn)?;
        let (entries, attachment_lengths) = load_entries(conn, None)?;
        let mut app = Self {
            rows,
            collapsed: HashSet::new(),
            selected_row: 0,
            entries,
            view: Vec::new(),
            table_selected: 0,
            details_scroll: 0,
            attachment_lengths,
            filter: String::new(),
            sort_key: SortKey::Title,
            sort_desc: false,
            marked: Vec::new(),
            focus: Focus::Collections,
            mode: Mode::Normal,
            status: None,
            should_quit: false,
            clipboard: arboard::Clipboard::new().ok(),
        };
        app.rebuild_view();
        Ok(app)
    }

    // Re-reads the tree and the currently selected collection's entries --
    // the CLI may have changed either underneath the TUI. If the selected
    // collection no longer exists, falls back to "All Papers" rather than
    // erroring.
    fn reload(&mut self, conn: &Connection) -> Result<(), String> {
        let selected_id = self.rows[self.selected_row].id;
        self.rows = load_tree(conn)?;
        self.selected_row = self
            .rows
            .iter()
            .position(|r| r.id == selected_id)
            .unwrap_or(0);

        // A reload can reparent the selected collection under a node that is
        // currently collapsed. Snap to a visible ancestor before fetching, so
        // the entry table matches the row actually highlighted.
        self.ensure_selected_visible();

        let collection_id = self.rows[self.selected_row].id;
        let (entries, lengths) = load_entries(conn, collection_id)?;
        self.entries = entries;
        self.attachment_lengths = lengths;
        self.rebuild_view();
        // `table_selected` is a position, not an entry id -- a reload can
        // change what actually occupies that position (an edit made from
        // another session, an entry deleted elsewhere), so a stale Details
        // scroll must not carry over onto whatever's there now. Same
        // reasoning as the "s"/"S" sort handlers.
        self.details_scroll = 0;
        Ok(())
    }

    // Re-reads just the tree (rows + entry_count), keeping the current
    // selection by id. Used after a picker toggle, which changes a count
    // but not which entries are loaded into the table.
    fn reload_tree_counts(&mut self, conn: &Connection) -> Result<(), String> {
        let selected_id = self.rows[self.selected_row].id;
        self.rows = load_tree(conn)?;
        self.selected_row = self
            .rows
            .iter()
            .position(|r| r.id == selected_id)
            .unwrap_or(0);
        self.ensure_selected_visible();
        Ok(())
    }

    // Recomputes `view` from `filter` + sort, and clamps `table_selected`
    // into it. The one place either changes, so every caller that touches
    // `entries`, `filter`, `sort_key`, or `sort_desc` ends with this.
    fn rebuild_view(&mut self) {
        let needle = self.filter.to_lowercase();
        self.view = (0..self.entries.len())
            .filter(|&i| matches_filter(&self.entries[i], &needle))
            .collect();
        sort_view(&self.entries, &mut self.view, self.sort_key, self.sort_desc);
        self.table_selected = clamp_selection(self.table_selected, self.view.len());
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.view
            .get(self.table_selected)
            .map(|&i| &self.entries[i])
    }

    // Looks up an entry by id in the already-loaded set -- the id half of
    // what selected_entry does by table position. What "not found" means
    // (a no-op, an error, an empty default) differs by call site, same as
    // it always did; this only shares the lookup itself.
    fn entry_by_id(&self, id: i64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == Some(id))
    }

    // The target set for a bulk action (export, tag/untag): the marked
    // entries, or just `entry_id` alone with nothing marked. Same rule
    // `open_picker`'s bulk-file mode uses.
    fn bulk_targets(&self, entry_id: i64) -> Vec<i64> {
        if self.marked.is_empty() {
            vec![entry_id]
        } else {
            self.marked.clone()
        }
    }

    // Creates a collection under `parent` (None = root, same as "All
    // Papers" selected) and reloads so the tree's rows/counts pick it up.
    // A DB error is shown on the footer rather than propagated -- a bad
    // name shouldn't end the session.
    fn create_collection(&mut self, conn: &Connection, parent: Option<i64>, name: &str) {
        match db::create_collection_under(conn, parent, name) {
            Ok(_) => {
                if let Err(e) = self.reload(conn) {
                    self.status = Some(e);
                }
            }
            // db_error, not e.to_string(): a rejected name arrives as
            // InvalidParameterName wrapping a message already written for a
            // human, and to_string() prefixes it with "Invalid parameter
            // name:" -- rusqlite's vocabulary leaking onto the footer.
            Err(e) => self.status = Some(crate::friendly(None, "create collection", e)),
        }
    }

    // The highlighted row's parent, found by walking back to the nearest
    // preceding row one depth shallower -- valid because the tree pane's
    // rows are a pre-order walk (see load_tree/db::collection_tree), which
    // always lists a parent immediately before its subtree. None both for
    // a top-level collection (its "parent" row is the synthetic "All
    // Papers" root, whose id is itself None -- matching a real top-level
    // collection's parent_id, which is NULL in the DB) and for "All
    // Papers" itself.
    fn parent_row_id(&self, row_idx: usize) -> Option<i64> {
        let depth = self.rows[row_idx].depth;
        self.rows[..row_idx]
            .iter()
            .rev()
            .find(|r| r.depth + 1 == depth)
            .and_then(|r| r.id)
    }

    // How many rows below `row_idx` belong to its subtree: the contiguous
    // run of following rows at strictly greater depth, same shape
    // db::subtree_ids walks (over collection_tree's output rather than
    // this pane's rows, but the same pre-order guarantee).
    fn subcollection_count(&self, row_idx: usize) -> usize {
        let depth = self.rows[row_idx].depth;
        self.rows[row_idx + 1..]
            .iter()
            .take_while(|r| r.depth > depth)
            .count()
    }

    // "R": opens the input line pre-filled with the collection's current
    // name. A no-op on "All Papers" (id: None) -- that row isn't a real
    // collection.
    fn begin_rename_collection(&mut self) {
        let row = &self.rows[self.selected_row];
        let Some(id) = row.id else {
            self.status = Some("'All Papers' isn't a collection -- nothing to rename".to_string());
            return;
        };
        self.mode = Mode::Input(InputKind::RenameCollection { id }, row.name.clone());
    }

    // RenameCollection's Enter. Reuses `reload`, which re-selects by id
    // (see its own comment) -- since `id` here is the same collection
    // being renamed, the selection lands right back on it.
    fn rename_collection(&mut self, conn: &Connection, id: i64, new_name: &str) {
        match db::rename_collection(conn, id, new_name) {
            Ok(()) => {
                if let Err(e) = self.reload(conn) {
                    self.status = Some(e);
                }
            }
            Err(e) => self.status = Some(crate::friendly(None, "rename collection", e)),
        }
    }

    // "D": confirms before deleting the highlighted collection and its
    // subtree. Names the collection, says how many subcollections go with
    // it, and says papers aren't touched -- delete_collection_by_id only
    // ever removes collection_entries membership rows, never an entry
    // itself. A no-op on "All Papers", same as begin_rename_collection.
    fn begin_delete_collection(&mut self) {
        let row_idx = self.selected_row;
        let row = &self.rows[row_idx];
        let Some(id) = row.id else {
            self.status = Some("'All Papers' isn't a collection -- nothing to delete".to_string());
            return;
        };
        let name = row.name.clone();
        let subcollections = self.subcollection_count(row_idx);
        let parent_id = self.parent_row_id(row_idx);
        let message = if subcollections == 0 {
            format!("Delete '{name}'? Papers stay in the library. y/n")
        } else {
            format!(
                "Delete '{name}' and its {subcollections} subcollection(s)? Papers stay in the library. y/n"
            )
        };
        self.mode = Mode::Confirm {
            message,
            action: PendingAction::DeleteCollection { id, parent_id },
        };
    }

    // PendingAction::DeleteCollection, confirmed. Selection goes to the
    // parent if there was one; a top-level collection has none
    // (parent_id: None), so it falls to whatever slid into the deleted
    // row's old index once the subtree is gone -- the "nearest remaining
    // row" a plain list deletion produces. Reloads the tree via load_tree
    // and the entries pane via load_entries, the same helpers `reload`
    // itself calls, rather than hand-rolling either.
    fn finish_delete_collection(&mut self, conn: &Connection, id: i64, parent_id: Option<i64>) {
        if let Err(e) = db::delete_collection_by_id(conn, id) {
            self.status = Some(crate::friendly(None, "delete collection", e));
            return;
        }

        self.rows = match load_tree(conn) {
            Ok(r) => r,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };
        self.selected_row = match parent_id {
            Some(pid) => self.rows.iter().position(|r| r.id == Some(pid)).unwrap_or(0),
            None => self.selected_row.min(self.rows.len() - 1),
        };
        self.ensure_selected_visible();

        let collection_id = self.rows[self.selected_row].id;
        match load_entries(conn, collection_id) {
            Ok((entries, lengths)) => {
                self.entries = entries;
                self.attachment_lengths = lengths;
                self.rebuild_view();
                self.details_scroll = 0;
            }
            Err(e) => self.status = Some(e),
        }
    }

    // Opens the collection picker. Uses collection_tree directly (not the
    // tree pane's rows) since there's nothing to file a paper into "All
    // Papers" -- that's not a collection.
    //
    // With entries marked, this becomes a bulk-file operation over the
    // whole marked set (add-only, no per-entry toggle -- see Mode::Picker's
    // `bulk` field); otherwise it's the original single-entry toggle over
    // whichever entry is selected.
    fn open_picker(&mut self, conn: &Connection) {
        let bulk = (!self.marked.is_empty()).then(|| self.marked.clone());
        let entry_id = match &bulk {
            Some(ids) => ids[0],
            None => match self.selected_entry().and_then(|e| e.id) {
                Some(id) => id,
                None => return,
            },
        };

        let tree = match db::collection_tree(conn) {
            Ok(t) => t,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        if tree.is_empty() {
            self.status = Some("no collections exist yet -- create one with 'n' first".to_string());
            return;
        }

        // Bulk mode starts with nothing checked: there's no single
        // well-defined membership state for a mixed set of entries, so a
        // row only flips to [x] once this session actually files into it.
        let member: HashSet<i64> = if bulk.is_some() {
            HashSet::new()
        } else {
            match db::collections_for_entry(conn, entry_id) {
                Ok(v) => v.into_iter().collect(),
                Err(e) => {
                    self.status = Some(e.to_string());
                    return;
                }
            }
        };

        let rows = tree
            .into_iter()
            .map(|(depth, c)| (depth, c.id, c.name))
            .collect();

        self.mode = Mode::Picker {
            rows,
            selected: 0,
            member,
            entry_id,
            bulk,
        };
    }

    // Opens every attachment of the selected entry through the system
    // opener. A failure (missing opener, no attachments) is shown on the
    // footer rather than propagated -- a broken path shouldn't end the
    // session. A dangling attachment (file gone from disk) is self-healed
    // via the same crate::open_or_detach_stale CLI `open` uses, rather than
    // handed to the opener to fail on -- see that function's doc comment.
    fn open_selected(&mut self, conn: &Connection) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let entry_id = entry.id;
        let cite_key = entry.cite_key.clone();
        let attachments = match db::attachments_for_cite_key(conn, &cite_key) {
            Ok(a) => a,
            Err(e) => {
                self.status = Some(crate::friendly(Some(&cite_key), "list attachments", e));
                return;
            }
        };
        if attachments.is_empty() {
            self.status = Some(format!("'{cite_key}' has no attachments"));
            return;
        }

        let mut opened = 0usize;
        let mut cleaned = 0usize;
        let mut early_error = None;
        for (id, path) in &attachments {
            match crate::open_or_detach_stale(conn, *id, path) {
                Ok(crate::OpenOutcome::Opened) => opened += 1,
                Ok(crate::OpenOutcome::DetachedStale) => cleaned += 1,
                Err(e) => {
                    early_error = Some(e);
                    break;
                }
            }
        }

        self.status = Some(match early_error {
            Some(e) => e,
            None if opened == 0 => format!(
                "removed {cleaned} dangling attachment(s) for '{cite_key}'; nothing left to open"
            ),
            None if cleaned > 0 => format!(
                "Opened {opened} attachment(s); removed {cleaned} dangling one(s) for '{cite_key}'"
            ),
            None => format!("Opened {opened} attachment(s) for '{cite_key}'"),
        });

        if cleaned > 0
            && let Some(id) = entry_id
        {
            let _ = self.refresh_entry(conn, id);
        }
    }

    // "y": copies the selected entry's link to the system clipboard.
    // Single-entry only -- a clipboard holds one string, and there's no
    // obviously correct joined form for a marked set nobody asked for.
    //
    // Reuses `self.clipboard` rather than creating a fresh `Clipboard` here:
    // a `Clipboard` created and dropped within one call hands the data off
    // to a system clipboard manager on drop (arboard's X11 backend), which
    // silently loses it when no manager is running -- confirmed directly,
    // this was the first shape of this method and it copied nothing.
    //
    // `self.clipboard` is `None` when `arboard` found no local
    // display-server clipboard to talk to at all -- a plain SSH session to
    // a headless machine, the case that actually matters for this project
    // (see DESIGN.md's Phase 18 addendum). There, `copy_via_osc52` asks the
    // *local* terminal emulator on the other end of the connection to grab
    // the text directly, which needs no display server on this end.
    fn copy_url(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let Some(url) = url_for_entry(entry) else {
            self.status = Some(format!("'{}' has no URL or DOI to copy", entry.cite_key));
            return;
        };
        self.status = Some(match &mut self.clipboard {
            Some(cb) => match cb.set_text(&url) {
                Ok(()) => format!("Copied {url}"),
                Err(e) => format!("failed to copy to clipboard: {e}"),
            },
            None => match copy_via_osc52(&url) {
                Ok(()) => format!("Sent {url} to your terminal's clipboard (OSC 52)"),
                Err(e) => format!("no system clipboard, and {e}"),
            },
        });
    }

    fn toggle_mark(&mut self) {
        if let Some(id) = self.selected_entry().and_then(|e| e.id) {
            toggle_marked(&mut self.marked, id);
        }
    }

    // "A": adds every row in the current view to `marked`, in view order,
    // skipping any already marked -- a union, not a replace, so marks made
    // in a previously-viewed collection (see select_row) survive this too.
    fn mark_all_visible(&mut self) {
        for &idx in &self.view {
            if let Some(id) = self.entries[idx].id
                && !self.marked.contains(&id)
            {
                self.marked.push(id);
            }
        }
    }

    // Refetches one entry by id and swaps it into `entries` in place, then
    // recomputes `view` -- used after an edit or fetch, where the entry
    // list's shape (which rows exist) hasn't changed, only one row's data.
    // Merge and delete DO change the shape, and use `reload` instead.
    fn refresh_entry(&mut self, conn: &Connection, entry_id: i64) -> Result<(), String> {
        let Some(idx) = self.entries.iter().position(|e| e.id == Some(entry_id)) else {
            return Ok(());
        };
        let cite_key = self.entries[idx].cite_key.clone();
        if let Some(fresh) = db::get_entry(conn, &cite_key).map_err(|e| e.to_string())? {
            self.entries[idx] = fresh;
        }
        self.rebuild_view();
        // T2: rebuild_view's clamp_selection only keeps table_selected in
        // bounds -- it has no notion of "follow the entry that was just
        // edited", so a sort key that reorders the list (title change under
        // a title sort) or a filter that drops the entry out (an untag
        // under an active `/` tag filter) left the highlight and Details
        // pane on whatever row index the edited entry used to occupy,
        // showing a different entry. Find its new row by id and select it;
        // if it's no longer in the view at all, the clamp above already
        // picked a valid row, but that row is a different entry, so the
        // stale scroll position from the old entry's Details pane no longer
        // means anything.
        match self.view.iter().position(|&i| self.entries[i].id == Some(entry_id)) {
            Some(row) => self.table_selected = row,
            None => self.details_scroll = 0,
        }
        Ok(())
    }

    // Edit's Input box, on Enter: applies the field to a clone of the
    // current entry and writes it with db::update_entry -- the same
    // function `ferref edit` uses, so a TUI edit and a CLI edit go through
    // one write path.
    fn apply_field_edit(&mut self, conn: &Connection, entry_id: i64, field: EditField, raw: &str) {
        let Some(current) = self.entry_by_id(entry_id) else {
            return;
        };
        let mut updated = current.clone();
        if let Err(e) = field.apply(&mut updated, raw) {
            self.status = Some(e);
            return;
        }
        match db::update_entry(conn, &updated) {
            Ok(()) => {
                if let Err(e) = self.refresh_entry(conn, entry_id) {
                    self.status = Some(e);
                }
            }
            Err(e) => {
                self.status = Some(crate::friendly(Some(&updated.cite_key), "update entry", e))
            }
        }
    }

    // "x": writes a BibTeX file for exactly `ids` (the marked set, or the
    // single selected entry) -- a subset of the library, unlike `ferref
    // export` which always writes everything. Legacy BibTeX only, matching
    // that command's own default; --biblatex has no TUI equivalent, same as
    // Fetch has no --email flag here.
    fn export_bibtex(&mut self, conn: &Connection, ids: &[i64], path: &str) {
        if path.is_empty() {
            self.status = Some("export path cannot be empty".to_string());
            return;
        }
        // B7: `marked` persists across a collection switch on purpose (mark
        // some papers, browse elsewhere, mark more, then bulk-export
        // together), but self.entries only holds the currently loaded
        // collection -- a marked id from a previous collection falls back to
        // a direct DB lookup instead of being silently dropped.
        let selected: Vec<Entry> = ids
            .iter()
            .filter_map(|&id| match self.entry_by_id(id).cloned() {
                Some(e) => Some(e),
                None => db::get_entry_by_id(conn, id).ok().flatten(),
            })
            .collect();
        let bibtex_str = crate::bibtex::export(&selected, false);
        match std::fs::write(path, &bibtex_str) {
            Ok(()) => {
                self.status = Some(format!(
                    "Exported {} entr{} to '{path}'",
                    selected.len(),
                    entries_plural(selected.len())
                ));
            }
            Err(e) => self.status = Some(format!("failed to write '{path}': {e}")),
        }
    }

    // ":" -> "t"/"u": adds (add: true) or removes (add: false) `tag` for
    // every id in `ids`, via the same db::add_tag/remove_tag `ferref
    // tag`/`untag` use -- both already idempotent, so a mixed set (some
    // already tagged, some not) is not an error.
    fn apply_tag(&mut self, conn: &Connection, ids: &[i64], tag: &str, add: bool) {
        if tag.is_empty() {
            self.status = Some("tag name cannot be empty".to_string());
            return;
        }
        // B7: same fallback as export_bibtex -- a marked id from a
        // collection that isn't currently loaded must not be silently
        // dropped from a bulk tag/untag.
        let cite_keys: Vec<String> = ids
            .iter()
            .filter_map(|&id| match self.entry_by_id(id) {
                Some(e) => Some(e.cite_key.clone()),
                None => db::cite_key_for_id(conn, id).ok().flatten(),
            })
            .collect();

        let mut changed = 0usize;
        for cite_key in &cite_keys {
            let result = if add {
                db::add_tag(conn, cite_key, tag)
            } else {
                db::remove_tag(conn, cite_key, tag)
            };
            match result {
                Ok(did_change) => changed += did_change as usize,
                Err(e) => {
                    // crate::entry_error, not e.to_string(): add_tag/remove_tag
                    // report an unknown cite_key as QueryReturnedNoRows, which
                    // would otherwise reach the footer as the bare SQLite
                    // message "Query returned no rows" instead of naming the
                    // entry that vanished (e.g. deleted from another session
                    // between load and this tag action).
                    let action = if add { "tag entry" } else { "untag entry" };
                    self.status = Some(crate::friendly(Some(cite_key), action, e));
                    return;
                }
            }
        }

        // Entry.tags is loaded once at reload time -- refresh every touched
        // entry so DETAILS (and "/" search, which matches on tags) don't
        // show stale tags until the next full reload.
        for &id in ids {
            if let Err(e) = self.refresh_entry(conn, id) {
                self.status = Some(e);
                return;
            }
        }

        let verb = if add { "Tagged" } else { "Untagged" };
        self.status = Some(format!(
            "{verb} {changed} of {} entr{} with '{tag}'",
            cite_keys.len(),
            entries_plural(cite_keys.len())
        ));
    }

    // Fetch (":" -> "f"): the Unpaywall round-trip and PDF download block
    // the event loop, so a "Fetching…" footer is drawn (reusing `error`'s
    // slot, the one line already rendered every frame) *before* the
    // blocking call, or the freeze reads as a hang rather than progress.
    // No --email equivalent in the TUI: falls back to FERREF_EMAIL / the
    // config file, same as the CLI does when --email is omitted.
    fn fetch_selected(
        &mut self,
        conn: &Connection,
        terminal: &mut ratatui::DefaultTerminal,
        entry_id: i64,
    ) {
        let Some(cite_key) = self.entry_by_id(entry_id).map(|e| e.cite_key.clone()) else {
            return;
        };

        self.status = Some(format!("Fetching PDF for '{cite_key}'\u{2026}"));
        let _ = terminal.draw(|frame| draw(frame, self));

        match crate::fetch_pdf_for_entry(conn, &cite_key, None) {
            Ok(crate::FetchOutcome::NoPdfFound { is_oa, .. }) => {
                self.status = Some(if is_oa {
                    format!("'{cite_key}' is open access, but no direct PDF link was found")
                } else {
                    format!("No open-access copy found for '{cite_key}'")
                });
            }
            Ok(crate::FetchOutcome::Downloaded {
                path,
                extraction,
                source,
                ..
            }) => {
                let landed = match source {
                    Some(source) => format!("Downloaded '{path}' from {source}"),
                    None => format!("'{cite_key}' already has its PDF at '{path}'"),
                };
                self.status = Some(match extraction {
                    Ok(chars) => format!("{landed} ({chars} chars extracted)"),
                    Err(e) => format!("{landed}, but extraction failed: {e}"),
                });
                if let Err(e) = self.refresh_entry(conn, entry_id) {
                    self.status = Some(e);
                }
            }
            Err(e) => self.status = Some(crate::summarize_fetch_error(&cite_key, &e)),
        }
    }

    // T1: `marked` persists across a collection switch (same as export/tag,
    // B7), but `entries` only holds the currently loaded collection -- a
    // merge started from marks spanning two collections used to show a
    // blank title in the confirm prompt and, at 3+ marks, an empty picker.
    // Resolves one id to a full Entry, `entries` first, `db::get_entry_by_id`
    // as a fallback -- never writing the result back into `self.entries`, so
    // a cancelled merge (Esc out of the picker, 'n' at the confirm prompt)
    // can't leave a foreign-collection entry sitting there for the next
    // `rebuild_view` to show in the wrong collection's list.
    fn resolve_merge_entry(&self, conn: &Connection, id: i64) -> Option<Entry> {
        self.entry_by_id(id)
            .cloned()
            .or_else(|| db::get_entry_by_id(conn, id).ok().flatten())
    }

    // ":" -> "m": see MergePlan / plan_merge for the branching rule itself.
    fn begin_merge(&mut self, conn: &Connection, entry_id: i64) {
        match plan_merge(&self.marked, Some(entry_id)) {
            Some(MergePlan::PickDrop(keep_id)) => {
                let candidates = self.entries.clone();
                let rows = entry_picker_rows(&candidates, keep_id, "");
                self.mode = Mode::EntryPicker {
                    keep_id,
                    candidates,
                    within_marked: false,
                    filter: String::new(),
                    rows,
                    selected: 0,
                };
            }
            Some(MergePlan::Pair(keep_id, drop_id)) => self.confirm_merge(conn, keep_id, drop_id),
            Some(MergePlan::PickDropWithin(keep_id, marked_ids)) => {
                // Candidates are resolved once, here, into owned Entry
                // values the picker carries itself -- never appended to
                // `self.entries` (see resolve_merge_entry).
                let candidates: Vec<Entry> = marked_ids
                    .iter()
                    .filter_map(|&id| self.resolve_merge_entry(conn, id))
                    .collect();
                let rows = entry_picker_rows(&candidates, keep_id, "");
                self.mode = Mode::EntryPicker {
                    keep_id,
                    candidates,
                    within_marked: true,
                    filter: String::new(),
                    rows,
                    selected: 0,
                };
            }
            None => {}
        }
    }

    fn confirm_merge(&mut self, conn: &Connection, keep_id: i64, drop_id: i64) {
        // Titles truncated (not the cite_key, which is short by convention)
        // so a long title plus the trailing "y/n" can't wrap the confirm
        // box past one line and push the actual prompt off screen. Resolved
        // once, here, into the message string itself -- nothing is kept
        // around afterward for a cancelled confirm to leak.
        let title_of = |id: i64| {
            self.resolve_merge_entry(conn, id)
                .map(|e| truncate_display(&e.title, 20))
                .unwrap_or_default()
        };
        self.mode = Mode::Confirm {
            message: format!(
                "Merge '{}' into '{}'? y/n",
                title_of(drop_id),
                title_of(keep_id)
            ),
            action: PendingAction::Merge { keep_id, drop_id },
        };
    }

    // ":" -> "d".
    fn begin_delete(&mut self, entry_id: i64) {
        let Some(entry) = self.entry_by_id(entry_id) else {
            return;
        };
        self.mode = Mode::Confirm {
            message: format!(
                "Delete '{}' [{}]? y/n",
                truncate_display(&entry.title, 20),
                entry.cite_key
            ),
            action: PendingAction::Delete { entry_id },
        };
    }

    // ":" -> "a": opens the file browser at $HOME (falling back to the
    // process's current directory if $HOME is unset), matching
    // config::library_root's own $HOME-first convention.
    fn begin_attach(&mut self, entry_id: i64) {
        let start = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        let Some(start) = start else {
            self.status = Some("cannot determine a starting directory: $HOME is not set".to_string());
            return;
        };
        match list_dir(&start) {
            Ok(entries) => {
                self.mode = Mode::FileBrowser {
                    entry_id,
                    cwd: start,
                    entries,
                    selected: 0,
                };
            }
            Err(e) => self.status = Some(e),
        }
    }

    // Moves the selection to the nearest visible ancestor when the selected
    // row has been hidden inside a collapsed subtree -- which a reload can do,
    // since it restores the selection by id without consulting `collapsed`.
    // Left alone, the pane draws no highlight at all and Up/Down do nothing,
    // which reads as a frozen UI rather than a lost selection.
    fn ensure_selected_visible(&mut self) {
        let visible = self.visible();
        if visible.contains(&self.selected_row) {
            return;
        }
        let mut idx = self.selected_row;
        while self.rows[idx].depth > 0 {
            let target_depth = self.rows[idx].depth - 1;
            match (0..idx).rev().find(|&i| self.rows[i].depth == target_depth) {
                Some(parent) => {
                    if visible.contains(&parent) {
                        self.selected_row = parent;
                        return;
                    }
                    idx = parent;
                }
                None => break,
            }
        }
        self.selected_row = 0;
    }

    fn seq(&self) -> Vec<(usize, Option<i64>)> {
        self.rows.iter().map(|r| (r.depth, r.id)).collect()
    }

    fn visible(&self) -> Vec<usize> {
        visible_rows(&self.seq(), &self.collapsed)
    }

    fn has_children(&self, row_idx: usize) -> bool {
        self.rows
            .get(row_idx + 1)
            .is_some_and(|next| next.depth > self.rows[row_idx].depth)
    }

    // Re-filters the entry table for the collection at `row_idx`. On a
    // fetch failure the previous entries stay put rather than blanking the
    // pane over a transient error -- but the failure is reported, since the
    // highlight has already moved and the table would otherwise be showing a
    // different collection's entries with nothing to say so.
    fn select_row(&mut self, conn: &Connection, row_idx: usize) {
        self.selected_row = row_idx;
        let collection_id = self.rows[row_idx].id;
        match load_entries(conn, collection_id) {
            Ok((entries, lengths)) => {
                self.entries = entries;
                self.attachment_lengths = lengths;
                self.table_selected = 0;
                self.details_scroll = 0;
                self.rebuild_view();
            }
            Err(e) => self.status = Some(e),
        }
    }

    fn move_tree(&mut self, conn: &Connection, delta: i32) {
        // Recover rather than bail: if the selection somehow isn't visible,
        // returning here leaves the arrow keys permanently inert.
        self.ensure_selected_visible();
        let visible = self.visible();
        let Some(pos) = visible.iter().position(|&i| i == self.selected_row) else {
            return;
        };
        let new_pos = (pos as i32 + delta).clamp(0, visible.len() as i32 - 1) as usize;
        let new_row = visible[new_pos];
        if new_row != self.selected_row {
            self.select_row(conn, new_row);
        }
    }

    fn tree_top(&mut self, conn: &Connection) {
        if let Some(&first) = self.visible().first() {
            self.select_row(conn, first);
        }
    }

    fn tree_bottom(&mut self, conn: &Connection) {
        if let Some(&last) = self.visible().last() {
            self.select_row(conn, last);
        }
    }

    // Collapses the current node if it has children and isn't already
    // collapsed; otherwise moves selection to its parent (which re-filters
    // the entry table, same as any other selection change).
    fn collapse_or_to_parent(&mut self, conn: &Connection) {
        let row = self.selected_row;
        let id = self.rows[row].id;
        if self.has_children(row) && !self.collapsed.contains(&id) {
            self.collapsed.insert(id);
            return;
        }
        if self.rows[row].depth > 0 {
            let target_depth = self.rows[row].depth - 1;
            if let Some(parent) = (0..row).rev().find(|&i| self.rows[i].depth == target_depth) {
                self.select_row(conn, parent);
            }
        }
    }

    fn expand(&mut self) {
        let row = self.selected_row;
        if self.has_children(row) {
            self.collapsed.remove(&self.rows[row].id);
        }
    }

    fn move_table(&mut self, delta: i32) {
        if self.view.is_empty() {
            return;
        }
        let len = self.view.len() as i32;
        let new = (self.table_selected as i32 + delta).clamp(0, len - 1);
        self.table_selected = new as usize;
        self.details_scroll = 0;
    }

    fn table_home(&mut self) {
        self.table_selected = 0;
        self.details_scroll = 0;
    }

    fn table_end(&mut self) {
        if !self.view.is_empty() {
            self.table_selected = self.view.len() - 1;
        }
        self.details_scroll = 0;
    }

    // "j"/"k"/Ctrl-d/Ctrl-u in the DETAILS pane. Only ever moves down from
    // (or up to) 0 -- the real bottom depends on the pane's rendered width
    // (line-wrapping), which this method doesn't have, so it's clamped at
    // render time instead (see draw_details); scrolling past the true end
    // here is harmless; the render just doesn't move any further.
    fn scroll_details(&mut self, delta: i32) {
        self.details_scroll = if delta < 0 {
            self.details_scroll.saturating_sub((-delta) as u16)
        } else {
            self.details_scroll.saturating_add(delta as u16)
        };
    }
}

// Counts here are RECURSIVE, unlike `collection ls`. Selecting a row filters
// the table recursively (see load_entries), so a direct count beside it made
// the pane disagree with itself: a parent whose papers all live in its children
// read "(0)" and then filled the table when you clicked it.
fn load_tree(conn: &Connection) -> Result<Vec<TreeRow>, String> {
    let tree = db::collection_tree(conn).map_err(|e| e.to_string())?;
    let total = db::count_entries(conn).map_err(|e| e.to_string())?;
    let counts = db::recursive_entry_counts(conn).map_err(|e| e.to_string())?;

    let mut rows = Vec::with_capacity(tree.len() + 1);
    rows.push(TreeRow {
        id: None,
        depth: 0,
        name: "All Papers".to_string(),
        entry_count: total,
    });
    for (depth, c) in tree.iter() {
        rows.push(TreeRow {
            id: Some(c.id),
            depth: depth + 1,
            name: c.name.clone(),
            // A collection missing from the map can't happen -- the CTE seeds
            // from every row of `collections` -- but falling back to the direct
            // count beats panicking in a render path.
            entry_count: *counts.get(&c.id).unwrap_or(&c.entry_count),
        });
    }
    Ok(rows)
}

// Fetches entries with with_full_text = false: the middle pane only shows
// four columns, so pulling every extracted PDF's text into memory here
// would be wasted work (see db::attachments_for_entry's comment). Recursive
// is always on for a real collection -- clicking a parent should show what's
// beneath it, same as Zotero.
fn load_entries(
    conn: &Connection,
    collection_id: Option<i64>,
) -> Result<(Vec<Entry>, AttachmentLengths), String> {
    // The id we already hold, not a path rebuilt from names: a collection
    // named with a literal "/" (hand-written into SQLite) produces a path that
    // can't be parsed back, and filtering by it silently returned nothing
    // while the tree pane went on showing a non-zero count beside it.
    let filter = match collection_id {
        None => Filter::default(),
        Some(id) => Filter {
            collection_id: Some(id),
            recursive: true,
            ..Default::default()
        },
    };
    let entries = db::list_entries(conn, &filter, false).map_err(|e| e.to_string())?;
    let lengths = db::all_attachment_text_lengths(conn).map_err(|e| e.to_string())?;
    Ok((entries, lengths))
}

// ---------------------------------------------------------------------
// Pure logic (tested below without a terminal)
// ---------------------------------------------------------------------

// Given a pre-order (depth, id) sequence -- collection_tree's output shape,
// with the synthetic "All Papers" root prepended at depth 0 -- and a set of
// collapsed ids, returns the indices that should be rendered. A node's
// subtree is the contiguous run of following rows at strictly greater
// depth, which collection_tree guarantees; collapsing it hides that whole
// run, collapsing a leaf (no such run) hides nothing.
fn visible_rows(seq: &[(usize, Option<i64>)], collapsed: &HashSet<Option<i64>>) -> Vec<usize> {
    let mut visible = Vec::new();
    let mut hide_below: Option<usize> = None;
    for (i, (depth, id)) in seq.iter().enumerate() {
        if let Some(d) = hide_below {
            if *depth > d {
                continue;
            }
            hide_below = None;
        }
        visible.push(i);
        if collapsed.contains(id) {
            hide_below = Some(*depth);
        }
    }
    visible
}

// Clamps a selection index into a list that may have shrunk (e.g. a reload
// landed on fewer rows than were selected before). An empty list clamps to 0.
fn clamp_selection(selected: usize, len: usize) -> usize {
    if len == 0 { 0 } else { selected.min(len - 1) }
}

// "y"'s link resolution: the entry's own url if it has one, else its DOI
// turned into a real, resolvable link (not just the bare identifier), else
// nothing to copy. Pure and DB-free so it's testable without a live
// clipboard -- see App::copy_url for the actual arboard call.
// "entry" or "entries" depending on n, for the several footer messages that
// report how many of a marked/bulk set something happened to.
fn entries_plural(n: usize) -> &'static str {
    if n == 1 { "y" } else { "ies" }
}

fn url_for_entry(entry: &Entry) -> Option<String> {
    if let Some(url) = &entry.url {
        return Some(url.clone());
    }
    entry
        .doi
        .as_ref()
        .map(|doi| format!("https://doi.org/{doi}"))
}

// OSC 52 clipboard-set escape sequence: `ESC ] 52 ; c ; <base64> BEL`.
// Understood by most terminal emulators still in wide use for SSH work
// (iTerm2, kitty, foot, WezTerm, Windows Terminal; tmux passes it through
// when `set-clipboard` is enabled) -- and, critically, works with *no*
// display server on this end at all, unlike `arboard`, because it asks the
// terminal emulator running on the user's own machine to grab the text,
// not this machine's (possibly nonexistent) clipboard. A terminal that
// doesn't understand OSC 52 simply discards it, so this can't corrupt the
// screen -- but nothing acks a successful paste either, so unlike a real
// clipboard API's `Result`, "the bytes went out" is the only thing this
// function can actually promise.
fn copy_via_osc52(text: &str) -> Result<(), String> {
    use base64::Engine;
    use std::io::Write;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("failed to write the OSC 52 escape sequence: {e}"))
}

// Builds the rows for the merge entry-picker: every candidate except
// `keep_id` itself, narrowed by `matches_filter` (the same case-insensitive
// substring match "/" search uses). A free function over an explicit
// `candidates` slice, not an `App` method, since T1's fix is exactly that
// the picker's candidates are its own owned list, never `App::entries`.
fn entry_picker_rows(candidates: &[Entry], keep_id: i64, filter: &str) -> Vec<usize> {
    let needle = filter.to_lowercase();
    (0..candidates.len())
        .filter(|&i| candidates[i].id != Some(keep_id) && matches_filter(&candidates[i], &needle))
        .collect()
}

// Case-insensitive substring match across every field a user would plausibly
// search by. An empty needle matches everything, so clearing the search box
// (or never opening it) is the same code path as "no filter".
fn matches_filter(entry: &Entry, needle_lowercase: &str) -> bool {
    if needle_lowercase.is_empty() {
        return true;
    }
    if entry.title.to_lowercase().contains(needle_lowercase) {
        return true;
    }
    for a in &entry.authors {
        if a.last_name.to_lowercase().contains(needle_lowercase) {
            return true;
        }
        if let Some(f) = &a.first_name
            && f.to_lowercase().contains(needle_lowercase)
        {
            return true;
        }
    }
    if let Some(j) = &entry.journal
        && j.to_lowercase().contains(needle_lowercase)
    {
        return true;
    }
    if let Some(y) = entry.year
        && y.to_string().contains(needle_lowercase)
    {
        return true;
    }
    if entry.cite_key.to_lowercase().contains(needle_lowercase) {
        return true;
    }
    entry
        .tags
        .iter()
        .any(|t| t.to_lowercase().contains(needle_lowercase))
}

// None always sorts last, in both directions -- reversing flips the order
// among present values, not whether an absent one counts as smallest. An
// entry missing a year belongs at the bottom of a year sort either way, not
// at the top just because the direction flipped.
fn cmp_optional<T: Ord>(a: Option<T>, b: Option<T>, desc: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let ord = x.cmp(&y);
            if desc { ord.reverse() } else { ord }
        }
    }
}

// Sorts a `Vec` of indices into `entries` (never the entries themselves --
// see `App::view`). String keys are lowercased for a case-insensitive order;
// `sort_by` is stable, so entries that tie on the key keep their prior
// relative order.
fn sort_view(entries: &[Entry], view: &mut [usize], key: SortKey, desc: bool) {
    view.sort_by(|&a, &b| {
        let (a, b) = (&entries[a], &entries[b]);
        match key {
            SortKey::Title => cmp_optional(
                Some(a.title.to_lowercase()),
                Some(b.title.to_lowercase()),
                desc,
            ),
            SortKey::Author => cmp_optional(
                a.authors.first().map(|x| x.last_name.to_lowercase()),
                b.authors.first().map(|x| x.last_name.to_lowercase()),
                desc,
            ),
            SortKey::Year => cmp_optional(a.year, b.year, desc),
            SortKey::Journal => cmp_optional(
                a.journal.as_ref().map(|j| j.to_lowercase()),
                b.journal.as_ref().map(|j| j.to_lowercase()),
                desc,
            ),
        }
    });
}

// Truncates `s` to at most `max_width` display columns, appending "…" if it
// doesn't fit. Character-safe by construction (built one whole char at a
// time, never sliced by byte index) and width-safe: width is measured with
// ratatui's own text width (which is unicode-width under the hood), not
// `.len()` or `.chars().count()`, so wide (CJK) characters count as 2
// columns and combining marks don't inflate the count.
fn truncate_display(s: &str, max_width: usize) -> String {
    if Span::raw(s).width() <= max_width {
        return s.to_string();
    }
    if max_width == 0 {
        return String::new();
    }

    let budget = max_width - 1; // reserve one column for the ellipsis
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = Span::raw(ch.to_string()).width();
        if width + w > budget {
            break;
        }
        out.push(ch);
        width += w;
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = Paragraph::new(format!(
            "Terminal too small (need at least {MIN_WIDTH}x{MIN_HEIGHT})"
        ))
        .wrap(Wrap { trim: true });
        frame.render_widget(msg, area);
        return;
    }

    let [main, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let [tree, table, details] = Layout::horizontal([
        Constraint::Length(26),
        Constraint::Min(20),
        Constraint::Length(34),
    ])
    .areas(main);

    draw_tree(frame, tree, app);
    draw_table(frame, table, app);
    draw_details(frame, details, app);
    draw_footer(frame, footer, app);

    match &app.mode {
        Mode::Picker {
            rows,
            selected,
            member,
            bulk,
            ..
        } => draw_picker(
            frame,
            area,
            rows,
            *selected,
            member,
            bulk.as_deref().map(<[_]>::len),
        ),
        Mode::Command { entry_id } => draw_command(frame, area, app, *entry_id),
        Mode::FieldPicker { entry_id, selected } => {
            draw_field_picker(frame, area, app, *entry_id, *selected)
        }
        Mode::EntryPicker {
            candidates,
            filter,
            rows,
            selected,
            within_marked,
            ..
        } => draw_entry_picker(frame, area, candidates, filter, rows, *selected, *within_marked),
        Mode::Confirm { message, .. } => draw_confirm(frame, area, message),
        Mode::Help => draw_help(frame, area),
        Mode::FileBrowser {
            entry_id,
            cwd,
            entries,
            selected,
        } => draw_file_browser(frame, area, app, *entry_id, cwd, entries, *selected),
        Mode::Normal | Mode::Input(..) => {}
    }
}

// The one cyan-bold accent this whole UI uses for "this needs your
// attention": a focused pane's border, and -- unconditionally, see
// floating_window below -- every floating window's border and title.
const FLOAT_ACCENT: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        FLOAT_ACCENT
    } else {
        Style::default()
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
        .title_style(style)
}

// Rect-centering, Clear, and the accented bordered Block every floating
// window (a popup, a picker, a menu, a confirmation, an overlay) shares --
// see DESIGN.md's Phase 16 addendum: every floating window uses
// FLOAT_ACCENT, unconditionally, no exceptions. Sharing this setup is what
// makes that true by construction: three popups already shipped without it
// once, when each of the six draw_* functions below hand-rolled its own
// copy of exactly this preamble.
//
// Returns the popup's Rect and an already-accented Block for the caller to
// attach to whatever widget it renders into that Rect (`.block(block)`) --
// the Block isn't rendered here on its own, since a widget needs to own its
// Block to compute where its content goes *inside* the border; rendering
// the border separately first would leave the content widget drawing over
// it. A caller that also needs the accent for content inside the window (a
// hotkey letter, a heading) uses FLOAT_ACCENT directly.
fn floating_window(
    frame: &mut Frame,
    frame_area: Rect,
    width: u16,
    height: u16,
    title: impl Into<String>,
) -> (Rect, Block<'static>) {
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(title.into())
        .borders(Borders::ALL)
        .border_style(FLOAT_ACCENT)
        .title_style(FLOAT_ACCENT);
    (popup, block)
}

fn draw_tree(frame: &mut Frame, area: Rect, app: &App) {
    let visible = app.visible();
    let text_width = area.width.saturating_sub(2) as usize; // borders

    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let row = &app.rows[i];
            let marker = if app.has_children(i) {
                if app.collapsed.contains(&row.id) {
                    "\u{25b8} "
                } else {
                    "\u{25be} "
                }
            } else {
                "  "
            };
            let indent = "  ".repeat(row.depth);
            let label = format!("{indent}{marker}{} ({})", row.name, row.entry_count);
            ListItem::new(truncate_display(&label, text_width))
        })
        .collect();

    let mut state = ListState::default();
    state.select(visible.iter().position(|&i| i == app.selected_row));

    let list = List::new(items)
        .block(pane_block(
            "COLLECTIONS".to_string(),
            app.focus == Focus::Collections,
        ))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

fn author_summary(authors: &[crate::models::Author]) -> String {
    match authors.split_first() {
        None => String::new(),
        Some((first, rest)) => {
            if rest.is_empty() {
                first.last_name.clone()
            } else {
                format!("{} et al.", first.last_name)
            }
        }
    }
}

// A dim "│" cell dropped between real columns so the splits in ENTRIES read
// clearly without a full bordered-table widget.
fn sep_cell() -> Cell<'static> {
    Cell::from("\u{2502}").style(Style::default().fg(Color::DarkGray))
}

fn draw_table(frame: &mut Frame, area: Rect, app: &App) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let header = Row::new(vec![
        Cell::from(" "),
        Cell::from("Title").style(bold),
        sep_cell(),
        Cell::from("Authors").style(bold),
        sep_cell(),
        Cell::from("Year").style(bold),
        sep_cell(),
        Cell::from("Journal").style(bold),
    ]);

    let rows: Vec<Row> = app
        .view
        .iter()
        .map(|&i| &app.entries[i])
        .enumerate()
        .map(|(i, e)| {
            // A colored marker in a leading column, not just the selection
            // highlight -- a mark must stay visible after the cursor moves
            // off the row, which REVERSED alone wouldn't show.
            let marked = e.id.is_some_and(|id| app.marked.contains(&id));
            let mark_cell = if marked {
                Cell::from("\u{25cf}").style(Style::default().fg(Color::Yellow))
            } else {
                Cell::from(" ")
            };
            // Zebra striping: without it every row is one undifferentiated
            // wall of text and it's not obvious where one entry ends and
            // the next begins. DIM rather than a fixed bg color, so the
            // stripe is relative to whatever the terminal's own palette is
            // and doesn't need a guess at what the background actually is.
            let row_style = if i % 2 == 1 {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            Row::new(vec![
                mark_cell,
                Cell::from(truncate_display(&e.title, 60)),
                sep_cell(),
                Cell::from(truncate_display(&author_summary(&e.authors), 14)),
                sep_cell(),
                Cell::from(e.year.map(|y| y.to_string()).unwrap_or_default()),
                sep_cell(),
                Cell::from(truncate_display(e.journal.as_deref().unwrap_or(""), 14)),
            ])
            .style(row_style)
        })
        .collect();

    let mut state = TableState::default();
    if !app.view.is_empty() {
        state.select(Some(app.table_selected));
    }

    let arrow = if app.sort_desc {
        '\u{2193}'
    } else {
        '\u{2191}'
    };
    let mut title = format!("ENTRIES [{} {arrow}]", app.sort_key.label());
    if !app.marked.is_empty() {
        title.push_str(&format!(" ({} marked)", app.marked.len()));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
            Constraint::Length(14),
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(14),
        ],
    )
    .header(header)
    .block(pane_block(title, app.focus == Focus::Entries))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(table, area, &mut state);
}

// A bold, all-caps "LABEL: value" line, so scanning the field names in
// DETAILS (doi, url, volume, ...) doesn't require reading the whole value
// first to tell where one field ends and the next begins.
fn field_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{}: ", label.to_uppercase()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value),
    ])
}

fn details_lines(e: &Entry, lengths: Option<&Vec<Option<i64>>>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        e.title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];

    if !e.authors.is_empty() {
        lines.push(Line::raw(crate::format_authors(&e.authors)));
    }

    let mut meta = Vec::new();
    if let Some(y) = e.year {
        meta.push(y.to_string());
    }
    if let Some(j) = &e.journal {
        meta.push(j.clone());
    }
    if !meta.is_empty() {
        lines.push(Line::raw(meta.join(" \u{b7} ")));
    }

    let mut fields = Vec::new();
    if let Some(v) = &e.volume {
        fields.push(field_line("volume", v.clone()));
    }
    if let Some(p) = &e.pages {
        fields.push(field_line("pages", p.clone()));
    }
    if let Some(d) = &e.doi {
        fields.push(field_line("doi", d.clone()));
    }
    if let Some(u) = &e.url {
        fields.push(field_line("url", u.clone()));
    }
    if !fields.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(fields);
    }

    if !e.tags.is_empty() {
        lines.push(Line::raw(""));
        lines.push(field_line(
            "tags",
            e.tags
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));
    }

    if !e.attachments.is_empty() {
        lines.push(Line::raw(""));
        for (i, a) in e.attachments.iter().enumerate() {
            let status = match lengths.and_then(|l| l.get(i)) {
                Some(Some(n)) => format!("text: {n} chars"),
                Some(None) => "text: not extracted".to_string(),
                None => "text: unknown".to_string(),
            };
            lines.push(Line::raw(format!("{} ({status})", a.path)));
        }
    }

    if let Some(abs) = &e.abstract_text {
        lines.push(Line::raw(""));
        lines.push(Line::raw(abs.clone()));
    }

    lines
}

fn draw_details(frame: &mut Frame, area: Rect, app: &App) {
    let lines = match app.selected_entry() {
        None => vec![Line::raw("No entries.")],
        Some(e) => {
            let lengths = e.id.and_then(|id| app.attachment_lengths.get(&id));
            details_lines(e, lengths)
        }
    };

    let para = Paragraph::new(Text::from(lines))
        .block(pane_block(
            "DETAILS".to_string(),
            app.focus == Focus::Details,
        ))
        .wrap(Wrap { trim: false });

    // The pane's two-cell border eats into the width text actually wraps
    // at; `area.width` itself is the *outer* rect draw_details was given.
    // `line_count` is ratatui's own word-wrapper, not a hand-rolled
    // estimate -- a first attempt here divided each Line's raw character
    // width by the available width, which undercounts real word-wrapped
    // rows (word-wrap leaves ragged-right space at each break, so it needs
    // *more* rows than a plain division assumes) and was caught silently
    // clamping "G" short of a real abstract's actual last words -- exactly
    // the failure this feature exists to prevent. `line_count` needs the
    // `unstable-rendered-line-info` cargo feature (see Cargo.toml); that
    // API surface not being under semver is an accepted, documented
    // tradeoff against being wrong about where the text actually ends.
    let text_width = area.width.saturating_sub(2);
    let total_lines = para.line_count(text_width);
    let visible = area.height.saturating_sub(2) as usize;
    let max_scroll = total_lines.saturating_sub(visible) as u16;
    let scroll = app.details_scroll.min(max_scroll);

    frame.render_widget(para.scroll((scroll, 0)), area);
}

// Priority order: a pending status message beats everything (it's
// transient, shown once); then the input prompt, so the user can see what
// they're typing; then the active filter, so it doesn't silently vanish
// from view; then the keymap.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text =
        if let Some(status) = &app.status {
            format!(" {status}")
        } else {
            match &app.mode {
            Mode::Input(InputKind::Search { .. }, buffer) => format!(" /{buffer}"),
            Mode::Input(InputKind::NewCollection, buffer) => {
                format!(" New collection: {buffer}")
            }
            Mode::Input(InputKind::RenameCollection { .. }, buffer) => {
                format!(" Rename collection: {buffer}")
            }
            Mode::Input(InputKind::EditField { field, .. }, buffer) => {
                format!(" {}: {buffer}", field.label())
            }
            Mode::Input(InputKind::ExportPath { ids }, buffer) => {
                format!(" Export {} entr{} to: {buffer}", ids.len(), entries_plural(ids.len()))
            }
            Mode::Input(InputKind::Tag { ids, add }, buffer) => {
                format!(
                    " {} {} entr{}: {buffer}",
                    if *add { "Tag" } else { "Untag" },
                    ids.len(),
                    entries_plural(ids.len())
                )
            }
            Mode::Picker { bulk: None, .. } => {
                " Enter toggle \u{b7} jk move \u{b7} Esc/q close".to_string()
            }
            Mode::Picker { bulk: Some(_), .. } => {
                " Enter file marked \u{b7} jk move \u{b7} Esc/q close".to_string()
            }
            Mode::Command { .. } => {
                " e edit \u{b7} f fetch \u{b7} a attach \u{b7} m merge \u{b7} d delete \u{b7} \
                 t tag \u{b7} u untag \u{b7} Esc close"
                    .to_string()
            }
            Mode::FieldPicker { .. } => " Enter edit \u{b7} jk move \u{b7} Esc back".to_string(),
            Mode::EntryPicker { .. } => {
                " type to filter \u{b7} \u{2191}\u{2193} move \u{b7} Enter pick \u{b7} Esc cancel"
                    .to_string()
            }
            Mode::Confirm { message, .. } => format!(" {message}"),
            Mode::Help => " Esc/q/any key: close".to_string(),
            Mode::FileBrowser { .. } => {
                " jk move \u{b7} l/Enter open/attach \u{b7} h back \u{b7} Esc cancel".to_string()
            }
            Mode::Normal if !app.filter.is_empty() => format!(" filter: {}", app.filter),
            // The full keymap lived here as one long line that grew with
            // every new feature; it's behind "?" now instead (draw_help).
            Mode::Normal => " ?  help \u{b7} q quit".to_string(),
        }
        };
    let footer = Paragraph::new(text.clone());
    frame.render_widget(footer, area);

    // A visible, terminal-native blinking cursor is the signal that a field
    // is actively accepting input -- otherwise the only difference between
    // "typing into an empty search box" and "not searching" was the footer
    // text itself, easy to miss at a glance. Every InputKind's footer text
    // above ends with the live buffer verbatim (nothing rendered after it),
    // so its rendered char count is exactly where the next keystroke lands.
    // Only shown while an input mode's own text is actually on screen: a
    // transient status message (app.status, cleared on the very next key)
    // pre-empts it above, and must pre-empt the cursor too, or the cursor
    // would sit at the end of someone else's message.
    if app.status.is_none() && matches!(app.mode, Mode::Input(..)) {
        let col = area.x + (text.chars().count() as u16).min(area.width.saturating_sub(1));
        frame.set_cursor_position((col, area.y));
    }
}

// Centered modal, blanked with Clear first so the panes underneath don't
// bleed through. Clamped to `frame_area` so it can't overflow a small
// terminal into a panic-worthy negative size.
fn draw_picker(
    frame: &mut Frame,
    frame_area: Rect,
    rows: &[(usize, i64, String)],
    selected: usize,
    member: &HashSet<i64>,
    bulk_count: Option<usize>,
) {
    let width = frame_area.width.saturating_sub(6).clamp(10, 60);
    let height = ((rows.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let title = match bulk_count {
        Some(n) => format!("File {n} marked into…"),
        None => "File into…".to_string(),
    };
    let (popup, block) = floating_window(frame, frame_area, width, height, title);

    let items: Vec<ListItem> = rows
        .iter()
        .map(|(depth, id, name)| {
            let mark = if member.contains(id) { "[x]" } else { "[ ]" };
            let indent = "  ".repeat(*depth);
            ListItem::new(format!("{mark} {indent}{name}"))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(selected));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// "?": the full keymap, grouped the way the footer hints used to be spread
// across modes -- one place to look instead of memorizing which mode shows
// which fragment. Static content (no App fields needed); Clone::clone would
// be free here anyway since it's all &'static str.
fn draw_help(frame: &mut Frame, frame_area: Rect) {
    const GROUPS: &[(&str, &[(&str, &str)])] = &[
        (
            "Navigate",
            &[
                ("Tab / BackTab", "switch pane"),
                ("j/k, \u{2191}\u{2193}", "move"),
                ("g / G", "top / bottom"),
                ("Ctrl-d / Ctrl-u", "10 rows"),
                ("h / l", "fold/unfold (tree) \u{b7} switch pane"),
            ],
        ),
        (
            "Find & sort",
            &[
                ("/", "search (Esc clears)"),
                ("s / S", "sort column / reverse"),
            ],
        ),
        (
            "Entries",
            &[
                ("Space", "mark for merge/bulk actions"),
                ("A", "mark every visible row"),
                ("U", "clear all marks"),
                (":", "command palette (opens its own menu)"),
                ("c", "file into collection (bulk if marked)"),
                ("x", "export marked as BibTeX"),
                ("o", "open attachment"),
                ("y", "copy url (or DOI link) to clipboard"),
            ],
        ),
        (
            "Collections",
            &[
                ("n", "new (sub)collection"),
                ("R", "rename this collection"),
                ("D", "delete this collection and its subtree"),
                ("x", "export this collection as BibTeX"),
            ],
        ),
        (
            "Other",
            &[("r", "reload"), ("q", "quit"), ("?", "this screen")],
        ),
    ];

    let width = frame_area.width.saturating_sub(6).clamp(30, 74);
    // ponytail: assumes every row fits on one line at this width, which is
    // true at the 74-col cap but not proven down at the 30-col floor (a
    // long description could wrap and get clipped by the fixed height
    // below) -- only matters on a terminal already at MIN_WIDTH, where
    // every pane is cramped anyway. Wrap-aware height math if that turns
    // out to matter in practice.
    let line_count: u16 = GROUPS
        .iter()
        .map(|(_, rows)| rows.len() as u16 + 1) // +1 for the group heading
        .sum();
    let height = (line_count + 2)
        .min(frame_area.height.saturating_sub(2))
        .max(3);
    let (popup, block) = floating_window(frame, frame_area, width, height, "Keymap");

    let key_width = GROUPS
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(k, _)| k.len()))
        .max()
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::new();
    for (heading, rows) in GROUPS {
        lines.push(Line::from(Span::styled(*heading, FLOAT_ACCENT)));
        for (key, desc) in *rows {
            lines.push(Line::from(vec![
                Span::raw(format!("  {key:<key_width$}  ")),
                Span::raw(*desc),
            ]));
        }
    }

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, popup);
}

// The ":" palette -- a small fixed list, titled with the scoped entry's
// cite_key so it's clear which paper the four actions apply to.
fn draw_command(frame: &mut Frame, frame_area: Rect, app: &App, entry_id: i64) {
    let cite_key = app
        .entry_by_id(entry_id)
        .map(|e| e.cite_key.as_str())
        .unwrap_or("");

    let width = 26u16.min(frame_area.width.saturating_sub(4)).max(12);
    let height = 9u16.min(frame_area.height.saturating_sub(4)).max(3);
    let (popup, block) = floating_window(frame, frame_area, width, height, cite_key.to_string());

    let hotkey = |key: &'static str, label: &'static str| {
        ListItem::new(Line::from(vec![
            Span::styled(key, FLOAT_ACCENT),
            Span::raw(format!("  {label}")),
        ]))
    };
    let items = vec![
        hotkey("e", "Edit field"),
        hotkey("f", "Fetch PDF"),
        hotkey("a", "Attach PDF"),
        hotkey("m", "Merge"),
        hotkey("d", "Delete"),
        hotkey("t", "Tag"),
        hotkey("u", "Untag"),
    ];
    frame.render_widget(List::new(items).block(block), popup);
}

// Edit's field-name list: label plus each field's current value, so picking
// one is informed rather than a guess at what's already there.
fn draw_field_picker(
    frame: &mut Frame,
    frame_area: Rect,
    app: &App,
    entry_id: i64,
    selected: usize,
) {
    let Some(entry) = app.entry_by_id(entry_id) else {
        return;
    };

    let width = frame_area.width.saturating_sub(6).clamp(20, 70);
    let height = ((EditField::ALL.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let (popup, block) = floating_window(frame, frame_area, width, height, "Edit field");

    let value_width = (width as usize).saturating_sub(14);
    let items: Vec<ListItem> = EditField::ALL
        .iter()
        .map(|&f| {
            let value = truncate_display(&f.current_value(entry), value_width);
            ListItem::new(format!("{:<10}{value}", f.label()))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(selected));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// Merge's fold-in-entry picker: a filter box folded into the title (there's
// no separate input line in the popup) plus the matching entries, title and
// cite_key both shown since either might be what the user remembers.
fn draw_entry_picker(
    frame: &mut Frame,
    frame_area: Rect,
    candidates: &[Entry],
    filter: &str,
    rows: &[usize],
    selected: usize,
    within_marked: bool,
) {
    let width = frame_area.width.saturating_sub(6).clamp(20, 70);
    let height = ((rows.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let base_title = if within_marked {
        "Merge into… (marked)"
    } else {
        "Merge into…"
    };
    let title = if filter.is_empty() {
        base_title.to_string()
    } else {
        format!("{base_title} /{filter}")
    };
    let (popup, block) = floating_window(frame, frame_area, width, height, title);

    let text_width = (width as usize).saturating_sub(2);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|&i| {
            let e = &candidates[i];
            let label = format!("{} ({})", e.title, e.cite_key);
            ListItem::new(truncate_display(&label, text_width))
        })
        .collect();

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(selected));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// Attach's directory browser: cwd as the title (truncated if too wide,
// same as draw_field_picker truncates field values), directories shown
// with a trailing '/' so they read as distinct from files at a glance.
fn draw_file_browser(
    frame: &mut Frame,
    frame_area: Rect,
    _app: &App,
    _entry_id: i64,
    cwd: &Path,
    entries: &[BrowserEntry],
    selected: usize,
) {
    let width = frame_area.width.saturating_sub(6).clamp(20, 70);
    let height = ((entries.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let title = truncate_display(&cwd.display().to_string(), (width as usize).saturating_sub(2));
    let (popup, block) = floating_window(frame, frame_area, width, height, title);

    let text_width = (width as usize).saturating_sub(2);
    let items: Vec<ListItem> = entries
        .iter()
        .map(|entry| {
            let label = if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            ListItem::new(truncate_display(&label, text_width))
        })
        .collect();

    let mut state = ListState::default();
    if !entries.is_empty() {
        state.select(Some(selected));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// Delete/merge confirmation: just the message, sized to fit it.
fn draw_confirm(frame: &mut Frame, frame_area: Rect, message: &str) {
    let width = (message.len() as u16 + 4)
        .min(frame_area.width.saturating_sub(2))
        .max(20);
    // A one-line message always fits a 3-row box (border, content, border);
    // draw()'s own MIN_HEIGHT check guarantees frame_area is tall enough.
    let height = 3;
    // No title (empty string): a destructive/mutating confirmation still
    // needs to pop, hence going through floating_window at all, but has
    // nothing worth naming in the border.
    let (popup, block) = floating_window(frame, frame_area, width, height, "");
    let para = Paragraph::new(message.to_string())
        .block(block)
        .wrap(Wrap { trim: true });
    frame.render_widget(para, popup);
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Author;

    // Collapsing a parent hides its whole (contiguous, deeper) subtree;
    // collapsing a leaf hides nothing.
    #[test]
    fn visible_rows_hides_collapsed_subtrees_only() {
        // All Papers -> A -> A/child -> B  (B is a sibling of A, not nested)
        let seq: Vec<(usize, Option<i64>)> =
            vec![(0, None), (1, Some(1)), (2, Some(2)), (1, Some(3))];

        let none: HashSet<Option<i64>> = HashSet::new();
        assert_eq!(visible_rows(&seq, &none), vec![0, 1, 2, 3]);

        // Collapsing A (row 1, has a child at depth 2) hides row 2 only.
        let mut collapsed_parent = HashSet::new();
        collapsed_parent.insert(Some(1));
        assert_eq!(visible_rows(&seq, &collapsed_parent), vec![0, 1, 3]);

        // Collapsing the leaf (row 2, no children) hides nothing.
        let mut collapsed_leaf = HashSet::new();
        collapsed_leaf.insert(Some(2));
        assert_eq!(visible_rows(&seq, &collapsed_leaf), vec![0, 1, 2, 3]);
    }

    #[test]
    fn truncate_display_is_width_safe_for_unicode() {
        assert_eq!(truncate_display("hello", 10), "hello");
        assert_eq!(truncate_display("hello world", 5), "hell\u{2026}");
        assert_eq!(truncate_display("M\u{fc}ller", 3), "M\u{fc}\u{2026}"); // accented char, 1 column wide
        // CJK characters are 2 columns wide: budget 3 fits exactly one plus ellipsis.
        assert_eq!(
            truncate_display("\u{738b}\u{738b}\u{738b}", 3),
            "\u{738b}\u{2026}"
        );
        assert_eq!(truncate_display("hello", 1), "\u{2026}");
        assert_eq!(truncate_display("hello", 0), "");
    }

    #[test]
    fn list_dir_filters_dotfiles_and_sorts_dirs_first() {
        let dir = std::env::temp_dir().join("ferref-list-dir-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("zeta_dir")).unwrap();
        std::fs::write(dir.join("alpha.txt"), b"x").unwrap();
        std::fs::write(dir.join(".hidden"), b"x").unwrap();

        let entries = list_dir(&dir).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["zeta_dir", "alpha.txt"]);
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);

        std::fs::remove_dir_all(&dir).ok();
    }

    // Regression: DirEntry::file_type() uses lstat semantics, so a symlink
    // pointing at a real directory (common under $HOME) used to be
    // classified as "not a directory" and the browser could never descend
    // into it. A broken symlink must still be classified as "not a
    // directory" (metadata() errors on it), not cause a panic.
    #[test]
    #[cfg(unix)]
    fn list_dir_follows_symlinks_to_classify_dir_vs_file() {
        let dir = std::env::temp_dir().join("ferref-list-dir-symlink-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("real_dir")).unwrap();
        std::os::unix::fs::symlink(dir.join("real_dir"), dir.join("dir_link")).unwrap();
        std::os::unix::fs::symlink(dir.join("does_not_exist"), dir.join("broken_link")).unwrap();

        let entries = list_dir(&dir).unwrap();
        let is_dir_of = |name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("missing entry {name}"))
                .is_dir
        };
        assert!(is_dir_of("dir_link"), "symlink to a directory must classify as a directory");
        assert!(!is_dir_of("broken_link"), "a broken symlink must not classify as a directory");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn path_reconstruction_matches_depth_stack() {
        let tree = vec![
            (0, mk_collection(1, "Physics")),
            (1, mk_collection(2, "Entropy")),
            (0, mk_collection(3, "Biology")),
        ];
        let paths = db::collection_tree_paths(&tree);
        assert_eq!(paths, vec!["Physics", "Physics/Entropy", "Biology"]);
    }

    fn mk_collection(id: i64, name: &str) -> db::Collection {
        db::Collection {
            id,
            name: name.to_string(),
            parent_id: None,
            entry_count: 0,
        }
    }

    #[test]
    fn clamp_selection_pulls_a_stale_index_back_into_range() {
        assert_eq!(clamp_selection(5, 2), 1);
        assert_eq!(clamp_selection(0, 0), 0);
        assert_eq!(clamp_selection(1, 5), 1);
    }

    fn mk_entry(title: &str, last_name: &str, year: Option<i32>, journal: &str) -> Entry {
        let mut e = Entry::new("article".to_string(), title.to_string(), title.to_string());
        if !last_name.is_empty() {
            e.add_author(Author::new(last_name.to_string(), None));
        }
        e.year = year;
        if !journal.is_empty() {
            e.journal = Some(journal.to_string());
        }
        e
    }

    // Minimal App fixture: the synthetic "All Papers" root as the only tree
    // row, `entries` as given, `view` = every index in order (tests that
    // care about filtering override it after construction).
    fn mk_app(entries: Vec<Entry>) -> App {
        let view = (0..entries.len()).collect();
        App {
            rows: vec![TreeRow {
                id: None,
                depth: 0,
                name: "All Papers".to_string(),
                entry_count: entries.len() as i64,
            }],
            collapsed: HashSet::new(),
            selected_row: 0,
            entries,
            view,
            table_selected: 0,
            details_scroll: 0,
            attachment_lengths: HashMap::new(),
            filter: String::new(),
            sort_key: SortKey::Title,
            sort_desc: false,
            marked: Vec::new(),
            focus: Focus::Entries,
            mode: Mode::Normal,
            status: None,
            should_quit: false,
            clipboard: None,
        }
    }

    // EditField::Authors::current_value formats an author list into the
    // "Last, First; Last, First" text box, and ::apply parses that same
    // text back into a Vec<Author> -- a serialize/deserialize pair that
    // nothing previously checked agree with each other. Re-applying a
    // value the field itself just produced must be a fixed point: it
    // shouldn't add, drop, or reorder an author, or start requiring commas
    // parse_author doesn't actually need (a lone surname has none).
    #[test]
    fn edit_field_authors_round_trips_through_its_own_current_value() {
        let mut e = Entry::new("article".to_string(), "k".to_string(), "T".to_string());
        e.add_author(Author::new("Shannon".to_string(), Some("C.E.".to_string())));
        e.add_author(Author::new("Jaynes".to_string(), None));

        let text = EditField::Authors.current_value(&e);
        assert_eq!(text, "Shannon, C.E.; Jaynes");

        let mut roundtripped = mk_entry("T", "", None, "");
        EditField::Authors.apply(&mut roundtripped, &text).unwrap();
        assert_eq!(roundtripped.authors, e.authors);
    }

    #[test]
    fn url_for_entry_prefers_url_then_falls_back_to_doi_then_nothing() {
        let mut e = mk_entry("Paper", "Smith", Some(2020), "");
        assert_eq!(url_for_entry(&e), None);

        e.doi = Some("10.1000/xyz".to_string());
        assert_eq!(
            url_for_entry(&e),
            Some("https://doi.org/10.1000/xyz".to_string())
        );

        e.url = Some("https://example.com/paper".to_string());
        assert_eq!(
            url_for_entry(&e),
            Some("https://example.com/paper".to_string())
        );
    }

    #[test]
    fn matches_filter_hits_every_searchable_field_case_insensitively() {
        let mut e = mk_entry("Deep Learning", "Smith", Some(2020), "Nature");
        e.cite_key = "smith2020".to_string();
        e.tags = vec!["ai".to_string()];

        assert!(matches_filter(&e, "deep")); // title
        assert!(matches_filter(&e, "smith")); // author
        assert!(matches_filter(&e, "nature")); // journal
        assert!(matches_filter(&e, "2020")); // year
        assert!(matches_filter(&e, "smith2020")); // cite_key
        assert!(matches_filter(&e, "ai")); // tag
        assert!(!matches_filter(&e, "quantum")); // matches nothing
        assert!(matches_filter(&e, "")); // empty matches everything
    }

    #[test]
    fn sort_view_by_year_puts_missing_last_in_both_directions() {
        let entries = vec![
            mk_entry("B", "", Some(2019), ""),
            mk_entry("A", "", None, ""),
            mk_entry("C", "", Some(2021), ""),
        ];
        let mut view: Vec<usize> = vec![0, 1, 2];
        sort_view(&entries, &mut view, SortKey::Year, false);
        assert_eq!(view, vec![0, 2, 1], "ascending: 2019, 2021, then missing");

        let mut view: Vec<usize> = vec![0, 1, 2];
        sort_view(&entries, &mut view, SortKey::Year, true);
        assert_eq!(
            view,
            vec![2, 0, 1],
            "descending: 2021, 2019, then still-missing-last"
        );
    }

    #[test]
    fn sort_view_by_title_is_case_insensitive() {
        let entries = vec![
            mk_entry("banana", "", None, ""),
            mk_entry("Apple", "", None, ""),
            mk_entry("Cherry", "", None, ""),
        ];
        let mut view: Vec<usize> = vec![0, 1, 2];
        sort_view(&entries, &mut view, SortKey::Title, false);
        assert_eq!(view, vec![1, 0, 2], "Apple, banana, Cherry");
    }

    #[test]
    fn rebuild_view_clamps_selection_when_the_filter_shrinks_the_list() {
        let mut app = App {
            rows: vec![TreeRow {
                id: None,
                depth: 0,
                name: "All Papers".to_string(),
                entry_count: 2,
            }],
            collapsed: HashSet::new(),
            selected_row: 0,
            entries: vec![
                mk_entry("Alpha", "Smith", None, ""),
                mk_entry("Beta", "Jones", None, ""),
            ],
            view: Vec::new(),
            table_selected: 1, // pointing at "Beta" before the filter narrows things
            details_scroll: 0,
            attachment_lengths: HashMap::new(),
            filter: String::new(),
            sort_key: SortKey::Title,
            sort_desc: false,
            marked: Vec::new(),
            focus: Focus::Entries,
            mode: Mode::Normal,
            status: None,
            should_quit: false,
            clipboard: None,
        };
        app.rebuild_view();
        assert_eq!(app.table_selected, 1);

        app.filter = "alpha".to_string();
        app.rebuild_view();
        assert_eq!(app.view.len(), 1);
        assert_eq!(app.table_selected, 0, "clamped back into the shrunk view");
    }

    // A field's label span is bold and upper-cased, distinct from its value
    // span, and a blank line separates the volume/pages/doi/url block from
    // the author/year line above it.
    #[test]
    fn details_lines_bolds_and_caps_field_labels() {
        let mut e = mk_entry("Deep Learning", "Smith", Some(2020), "Nature");
        e.doi = Some("10.1/xyz".to_string());
        e.url = Some("https://example.com".to_string());

        let lines = details_lines(&e, None);
        let doi_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("10.1/xyz")))
            .expect("doi line present");
        let label = &doi_line.spans[0];
        assert_eq!(label.content.as_ref(), "DOI: ");
        assert!(label.style.add_modifier.contains(Modifier::BOLD));

        // blank line immediately before the volume/pages/doi/url block
        let doi_idx = lines
            .iter()
            .position(|l| std::ptr::eq(l, doi_line))
            .unwrap();
        assert!(lines[..doi_idx].iter().any(|l| l.spans.is_empty()));
    }

    // draw_details's scroll clamp is `total_lines - visible`, where
    // `total_lines` comes from `Paragraph::line_count` (ratatui's own
    // word-wrapper, gated behind the `unstable-rendered-line-info` cargo
    // feature -- see Cargo.toml). An earlier version re-derived the
    // wrapped row count itself (each Line's raw character width divided
    // by the pane width) instead of asking ratatui, and that estimate
    // undercounted real word-wrapped text for some inputs -- word-wrap
    // can't split a word mid-token, so a row sometimes ends with unused
    // width a plain division doesn't account for -- which silently
    // clamped "G" short of a real abstract's actual last words, caught
    // live in a real terminal session, not by any unit test.
    //
    // This locks down two things a live-only check can't: that the naive
    // (wrong) estimate really does diverge from `line_count` for a
    // realistic multi-word case (a regression back to raw-division would
    // fail this), and that the clamp subtraction itself is correct at
    // both a fits-entirely and a needs-scrolling boundary. The divergent
    // case below is verified, not assumed -- a first attempt at this test
    // used a fixture (a single unbroken 100-char token) where word-wrap
    // has no breakpoints at all, so it coincidentally agreed with plain
    // division and would have passed even with the old, buggy code path
    // still in place; this one was checked to actually diverge before
    // being written down as a fixture.
    #[test]
    fn details_scroll_uses_real_word_wrap_not_naive_division() {
        let text =
            "alpha bee car delta elephant fig grape house ivy jelly kangaroo lemon mango";
        let width = 17u16;
        let naive_estimate = (text.chars().count() as u16).div_ceil(width);
        assert_eq!(naive_estimate, 5, "sanity check on the fixture itself");

        let para = Paragraph::new(Text::from(vec![Line::raw(text.to_string())]))
            .wrap(Wrap { trim: false });
        let total_lines = para.line_count(width) as u16;
        assert_eq!(
            total_lines, 6,
            "word-wrap needs a 6th row for this text at width 17 -- a \
             regression to character-width division would compute 5 here \
             and silently clamp scrolling one row short"
        );

        // A pane tall enough to show all 6 rows: nothing to scroll.
        assert_eq!(total_lines.saturating_sub(6), 0);
        // A pane that only fits 4 rows at a time: 2 rows of headroom.
        assert_eq!(total_lines.saturating_sub(4), 2);
    }

    // Insertion order matters (first marked survives a merge), and marking
    // the same id twice unmarks it rather than adding a duplicate.
    #[test]
    fn toggle_marked_is_insertion_ordered_and_toggles_off() {
        let mut marked = Vec::new();
        toggle_marked(&mut marked, 5);
        toggle_marked(&mut marked, 2);
        assert_eq!(marked, vec![5, 2], "insertion order preserved, not sorted");

        toggle_marked(&mut marked, 5);
        assert_eq!(marked, vec![2], "marking again unmarks");
    }

    // "A" unions the view into `marked` (existing marks first, then newly
    // added in view order), skips anything already marked (no duplicates),
    // and leaves an id outside the current view (id 5, simulating a mark
    // made in a previously-viewed collection) completely untouched -- the
    // whole point of "A" being additive, not a replace.
    #[test]
    fn mark_all_visible_unions_the_view_without_clearing_existing_marks() {
        let mut e1 = mk_entry("Alpha", "", None, "");
        e1.id = Some(1);
        let mut e2 = mk_entry("Beta", "", None, "");
        e2.id = Some(2);
        let mut e3 = mk_entry("Gamma", "", None, "");
        e3.id = Some(3);

        let mut app = mk_app(vec![e1, e2, e3]);
        app.view = vec![0, 2]; // Beta (id 2) filtered out
        app.marked = vec![5, 3]; // 5 is from elsewhere; 3 is already marked

        app.mark_all_visible();
        assert_eq!(
            app.marked,
            vec![5, 3, 1],
            "existing marks (including the out-of-view one) survive; only \
             the new, not-yet-marked visible id (1) is appended"
        );
    }

    #[test]
    fn export_filename_for_row_uses_the_collection_name_or_falls_back() {
        assert_eq!(
            export_filename_for_row(&TreeRow {
                id: None,
                depth: 0,
                name: "All Papers".to_string(),
                entry_count: 0,
            }),
            "export.bib",
            "the synthetic root has no name worth using as a filename"
        );
        assert_eq!(
            export_filename_for_row(&TreeRow {
                id: Some(1),
                depth: 1,
                name: "Physics".to_string(),
                entry_count: 0,
            }),
            "Physics.bib"
        );
        assert_eq!(
            export_filename_for_row(&TreeRow {
                id: Some(2),
                depth: 1,
                name: "Neuro/Science".to_string(),
                entry_count: 0,
            }),
            "Neuro_Science.bib",
            "a name containing a path separator must not escape ./ or collide with it"
        );
    }

    #[test]
    fn plan_merge_branches_on_how_many_are_marked() {
        // 0 marked: falls back to the selected row as keeper.
        assert_eq!(plan_merge(&[], Some(9)), Some(MergePlan::PickDrop(9)));
        // 1 marked: still falls back to the selected row, ignoring the mark.
        assert_eq!(plan_merge(&[1], Some(9)), Some(MergePlan::PickDrop(9)));
        // 0 or 1 marked with nothing selected: nothing to do.
        assert_eq!(plan_merge(&[], None), None);
        // Exactly 2: order decides keep/drop, no picker needed.
        assert_eq!(plan_merge(&[1, 2], Some(9)), Some(MergePlan::Pair(1, 2)));
        // 3+: first-marked keeps, picker is scoped to the rest of the marks.
        assert_eq!(
            plan_merge(&[1, 2, 3], Some(9)),
            Some(MergePlan::PickDropWithin(1, vec![2, 3]))
        );
    }

    // T1: mark an entry, switch collection (so `entries` no longer holds
    // it), mark another, then merge. Both the 2-marked confirm prompt (blank
    // title) and the 3+-marked picker (0 rows) used to drop marked entries
    // that weren't in the currently loaded collection.
    #[test]
    fn begin_merge_loads_marked_entries_outside_the_current_view() {
        let dir = std::env::temp_dir().join("ferref-begin-merge-test");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();

        let mut visible = mk_entry("Visible Paper", "Smith", Some(2020), "");
        visible.cite_key = "visible".to_string();
        let visible_id = db::insert_entry(&conn, &visible).unwrap();

        let mut elsewhere_a = mk_entry("Elsewhere A", "Jones", Some(2021), "");
        elsewhere_a.cite_key = "elsewherea".to_string();
        let elsewhere_a_id = db::insert_entry(&conn, &elsewhere_a).unwrap();

        let mut elsewhere_b = mk_entry("Elsewhere B", "Lee", Some(2022), "");
        elsewhere_b.cite_key = "elsewhereb".to_string();
        let elsewhere_b_id = db::insert_entry(&conn, &elsewhere_b).unwrap();

        // Only "Visible Paper" is loaded -- as if the other two were marked
        // in a different collection before switching to this one.
        let mut app = mk_app(vec![visible.clone()]);
        app.entries[0].id = Some(visible_id);

        // 2 marked (one loaded, one not): confirm prompt must show the real
        // title, not blank.
        app.marked = vec![elsewhere_a_id, visible_id];
        app.begin_merge(&conn, visible_id);
        match &app.mode {
            Mode::Confirm { message, .. } => {
                assert!(
                    message.contains("Elsewhere A") && message.contains("Visible Paper"),
                    "message was: {message}"
                );
            }
            _ => panic!("expected Confirm mode"),
        }

        // 3+ marked, all outside the loaded set but one: the picker must
        // list the other marked entries, not come back empty.
        app.marked = vec![visible_id, elsewhere_a_id, elsewhere_b_id];
        app.begin_merge(&conn, visible_id);
        match &app.mode {
            Mode::EntryPicker { rows, .. } => {
                assert_eq!(rows.len(), 2, "both other marked entries should be candidates");
            }
            _ => panic!("expected EntryPicker mode"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // T1 (review follow-up): a cancelled merge must not leave a
    // foreign-collection entry sitting in `entries` for the next
    // `rebuild_view` to show in the wrong collection's list. Esc out of the
    // picker (candidates were only ever owned by Mode::EntryPicker itself)
    // and confirm the entry never appears in `view`.
    #[test]
    fn cancelled_merge_does_not_leak_foreign_entries_into_view() {
        let dir = std::env::temp_dir().join("ferref-begin-merge-cancel-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();

        let mut here = mk_entry("Here Paper", "Smith", Some(2020), "");
        here.cite_key = "here".to_string();
        let here_id = db::insert_entry(&conn, &here).unwrap();

        let mut elsewhere = mk_entry("Elsewhere Paper", "Jones", Some(2021), "");
        elsewhere.cite_key = "elsewhere".to_string();
        let elsewhere_id = db::insert_entry(&conn, &elsewhere).unwrap();

        let mut other = mk_entry("Other Paper", "Lee", Some(2022), "");
        other.cite_key = "other".to_string();
        let other_id = db::insert_entry(&conn, &other).unwrap();

        let mut app = mk_app(vec![here.clone()]);
        app.entries[0].id = Some(here_id);

        // 3+ marked so plan_merge opens the picker (a 2-marked merge skips
        // straight to Mode::Confirm, which never touches `entries` either).
        app.marked = vec![here_id, elsewhere_id, other_id];
        app.begin_merge(&conn, here_id);
        let Mode::EntryPicker {
            keep_id,
            candidates,
            within_marked,
            filter,
            rows,
            selected,
        } = std::mem::replace(&mut app.mode, Mode::Normal)
        else {
            panic!("expected EntryPicker mode");
        };
        assert_eq!(app.entries.len(), 1, "candidates must not be appended to entries");

        handle_entry_picker_key(
            &mut app,
            &conn,
            KeyCode::Esc,
            keep_id,
            candidates,
            within_marked,
            filter,
            rows,
            selected,
        );

        app.rebuild_view();
        assert_eq!(app.entries.len(), 1, "entries must still be just the local collection");
        assert!(
            app.view.iter().all(|&i| app.entries[i].id != Some(elsewhere_id)),
            "the foreign entry must never appear in view"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // T2: sort by title, then edit "Gamma" to "Aardvark" -- it now sorts
    // first, so the highlight (and Details pane) must follow it to row 0
    // rather than staying at the old row index (which is now "Beta").
    #[test]
    fn refresh_entry_follows_the_edited_entry_after_a_resort() {
        let dir = std::env::temp_dir().join("ferref-refresh-entry-resort-test");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();

        let mut alpha = mk_entry("Alpha", "", None, "");
        alpha.cite_key = "alpha".to_string();
        let mut beta = mk_entry("Beta", "", None, "");
        beta.cite_key = "beta".to_string();
        let mut gamma = mk_entry("Gamma", "", None, "");
        gamma.cite_key = "gamma".to_string();
        let alpha_id = db::insert_entry(&conn, &alpha).unwrap();
        let beta_id = db::insert_entry(&conn, &beta).unwrap();
        let gamma_id = db::insert_entry(&conn, &gamma).unwrap();
        alpha.id = Some(alpha_id);
        beta.id = Some(beta_id);
        gamma.id = Some(gamma_id);

        let mut app = mk_app(vec![alpha, beta, gamma]);
        app.sort_key = SortKey::Title;
        app.rebuild_view(); // Alpha, Beta, Gamma
        app.table_selected = app.view.iter().position(|&i| app.entries[i].id == Some(gamma_id)).unwrap();
        app.details_scroll = 7; // an old, now-meaningless scroll offset

        app.apply_field_edit(&conn, gamma_id, EditField::Title, "Aardvark");

        let selected_id = app.view.get(app.table_selected).map(|&i| app.entries[i].id);
        assert_eq!(
            selected_id,
            Some(Some(gamma_id)),
            "highlight should follow the renamed entry to its new (first) row"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // The other half of T2: an untag that drops the edited entry out of an
    // active `/` filter. The entry vanishes from `view` entirely, so there's
    // no row to follow it to -- `details_scroll` must still be reset rather
    // than kept pointed at content that's no longer shown.
    #[test]
    fn refresh_entry_resets_details_scroll_when_the_entry_drops_out_of_the_filter() {
        let dir = std::env::temp_dir().join("ferref-refresh-entry-filter-test");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();

        let mut tagged = mk_entry("Tagged Paper", "", None, "");
        tagged.cite_key = "tagged".to_string();
        let tagged_id = db::insert_entry(&conn, &tagged).unwrap();
        db::add_tag(&conn, "tagged", "keep").unwrap();
        tagged.id = Some(tagged_id);
        tagged.tags = vec!["keep".to_string()];

        let mut other = mk_entry("Other Paper", "", None, "");
        other.cite_key = "other".to_string();
        let other_id = db::insert_entry(&conn, &other).unwrap();
        other.id = Some(other_id);

        let mut app = mk_app(vec![tagged, other]);
        app.filter = "keep".to_string();
        app.rebuild_view(); // only "Tagged Paper" matches
        app.table_selected = 0;
        app.details_scroll = 12;

        db::remove_tag(&conn, "tagged", "keep").unwrap();
        app.refresh_entry(&conn, tagged_id).unwrap();

        assert!(
            app.view.iter().all(|&i| app.entries[i].id != Some(tagged_id)),
            "the untagged entry should have dropped out of the active filter"
        );
        assert_eq!(app.details_scroll, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    // "R": renaming the selected collection must leave the selection on
    // the same collection (by id), not wherever the newly-named row's
    // sort position happens to land.
    #[test]
    fn rename_collection_keeps_the_selection_on_the_same_id() {
        let dir = std::env::temp_dir().join("ferref-rename-collection-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();

        let id = db::create_collection(&conn, "ml").unwrap();
        let mut app = App::load(&conn).unwrap();
        app.selected_row = app.rows.iter().position(|r| r.id == Some(id)).unwrap();

        app.rename_collection(&conn, id, "Machine Learning");

        assert_eq!(app.status, None, "rename should succeed");
        assert_eq!(app.rows[app.selected_row].id, Some(id));
        assert_eq!(app.rows[app.selected_row].name, "Machine Learning");

        std::fs::remove_dir_all(&dir).ok();
    }

    // "D": deleting a selected subcollection must move the selection to
    // its parent, not to "All Papers" or wherever it happened to land.
    #[test]
    fn delete_collection_selects_the_parent() {
        let dir = std::env::temp_dir().join("ferref-delete-collection-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();

        let parent_id = db::create_collection(&conn, "Physics").unwrap();
        let child_id = db::create_collection(&conn, "Physics/Entropy").unwrap();

        let mut app = App::load(&conn).unwrap();
        app.selected_row = app.rows.iter().position(|r| r.id == Some(child_id)).unwrap();

        app.begin_delete_collection();
        let (id, parent_id_captured) = match app.mode {
            Mode::Confirm {
                action: PendingAction::DeleteCollection { id, parent_id },
                ..
            } => (id, parent_id),
            _ => panic!("expected a Confirm(DeleteCollection) mode"),
        };
        assert_eq!(id, child_id);
        assert_eq!(parent_id_captured, Some(parent_id));

        app.finish_delete_collection(&conn, id, parent_id_captured);
        assert_eq!(app.status, None, "delete should succeed");
        assert_eq!(app.rows[app.selected_row].id, Some(parent_id));

        std::fs::remove_dir_all(&dir).ok();
    }

    // Neither key does anything on the synthetic "All Papers" row -- it
    // isn't a real collection, so there's nothing to rename or delete.
    #[test]
    fn rename_and_delete_do_nothing_on_the_all_entries_row() {
        let dir = std::env::temp_dir().join("ferref-collection-noop-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let conn = db::init_db(&dir.join("ferref.db")).unwrap();
        db::create_collection(&conn, "Physics").unwrap();

        let mut app = App::load(&conn).unwrap();
        app.selected_row = 0;
        assert_eq!(app.rows[0].id, None, "row 0 must be the 'All Papers' root");

        app.begin_rename_collection();
        assert!(matches!(app.mode, Mode::Normal), "R must not open an input on this row");
        assert!(app.status.is_some(), "R must leave an explanatory status");

        app.status = None;
        app.begin_delete_collection();
        assert!(matches!(app.mode, Mode::Normal), "D must not open a confirm on this row");
        assert!(app.status.is_some(), "D must leave an explanatory status");

        std::fs::remove_dir_all(&dir).ok();
    }
}
