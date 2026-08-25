// Terminal UI over the same SQLite file the CLI writes. It can file papers
// into collections and create new collections; it cannot edit or delete an
// entry, delete a collection, rename anything, or tag -- those stay
// CLI-only, so a mis-keypress here can misfile a paper but can't destroy
// data.
//
// Data is fetched only when state changes (on load, on a collection
// selection change, on a sort/filter edit, or on manual reload) and cached
// in `App`; the render function (`draw`) never touches the `Connection`.
// Sorting and filtering happen in memory over the already-loaded entries --
// `view` holds the indices into `entries` after filter then sort, so
// neither needs a round trip to SQLite.

use std::collections::HashMap;
use std::collections::HashSet;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::tty::IsTty;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
    Wrap,
};
use ratatui::Frame;
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
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
                {
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
    // An error is shown for exactly one frame: whatever key dismisses it
    // also clears it, so it can't linger over unrelated activity.
    app.error = None;

    match app.mode {
        Mode::Normal => handle_normal_key(app, conn, code, modifiers),
        Mode::Input(..) => handle_input_key(app, conn, code),
        Mode::Picker { .. } => handle_picker_key(app, conn, code),
        Mode::Command { .. } => handle_command_key(app, conn, terminal, code),
        Mode::FieldPicker { .. } => handle_field_picker_key(app, code),
        Mode::EntryPicker { .. } => handle_entry_picker_key(app, code),
        Mode::Confirm { .. } => handle_confirm_key(app, conn, code),
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
            }
        }
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('r') => {
            // A failed reload leaves the previous state in place rather
            // than crashing the session over a transient DB error.
            if let Err(e) = app.reload(conn) {
                app.error = Some(e);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => match app.focus {
            Focus::Collections => app.move_tree(conn, -1),
            Focus::Entries => app.move_table(-1),
            Focus::Details => {}
        },
        KeyCode::Down | KeyCode::Char('j') => match app.focus {
            Focus::Collections => app.move_tree(conn, 1),
            Focus::Entries => app.move_table(1),
            Focus::Details => {}
        },
        KeyCode::Char('d')
            if modifiers.contains(KeyModifiers::CONTROL) && app.focus == Focus::Entries =>
        {
            app.move_table(10);
        }
        KeyCode::Char('u')
            if modifiers.contains(KeyModifiers::CONTROL) && app.focus == Focus::Entries =>
        {
            app.move_table(-10);
        }
        KeyCode::Char('g') => match app.focus {
            Focus::Collections => app.tree_top(conn),
            Focus::Entries => app.table_home(),
            Focus::Details => {}
        },
        KeyCode::Char('G') => match app.focus {
            Focus::Collections => app.tree_bottom(conn),
            Focus::Entries => app.table_end(),
            Focus::Details => {}
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
        }
        KeyCode::Char('S') => {
            app.sort_desc = !app.sort_desc;
            app.rebuild_view();
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
        KeyCode::Char('c') if app.focus == Focus::Entries => app.open_picker(conn),
        KeyCode::Char('o') if matches!(app.focus, Focus::Entries | Focus::Details) => {
            app.open_selected();
        }
        // Toggles the current row into the merge marks. Insertion order
        // matters (first marked survives a merge, second is folded in and
        // deleted) -- see App::toggle_mark.
        KeyCode::Char(' ') if app.focus == Focus::Entries => app.toggle_mark(),
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

// Owns the buffer for the duration of the key: mem::replace pulls it out of
// `app.mode` so the match arms below can freely call back into `app`
// (reload, rebuild_view) without fighting the borrow checker over a field
// that's simultaneously borrowed and being written back to.
fn handle_input_key(app: &mut App, conn: &Connection, code: KeyCode) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let (kind, mut buffer) = match mode {
        Mode::Input(kind, buffer) => (kind, buffer),
        other => {
            app.mode = other;
            return;
        }
    };

    match code {
        KeyCode::Char(c) => {
            buffer.push(c);
            if matches!(kind, InputKind::Search { .. }) {
                app.filter = buffer.clone();
                app.rebuild_view();
            }
            app.mode = Mode::Input(kind, buffer);
        }
        KeyCode::Backspace => {
            buffer.pop();
            if matches!(kind, InputKind::Search { .. }) {
                app.filter = buffer.clone();
                app.rebuild_view();
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
            }
        }
        KeyCode::Esc => {
            match kind {
                InputKind::Search { previous } => {
                    app.filter = previous;
                    app.rebuild_view();
                    app.mode = Mode::Normal;
                }
                InputKind::NewCollection => {
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

// Same ownership move as handle_input_key, and for the same reason: a
// toggle needs to call back into `app` (reload_tree_counts) while updating
// the picker's own state.
fn handle_picker_key(app: &mut App, conn: &Connection, code: KeyCode) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::Picker {
        rows,
        mut selected,
        mut member,
        entry_id,
    } = mode
    else {
        app.mode = mode;
        return;
    };

    match code {
        KeyCode::Char('q') | KeyCode::Esc => return, // app.mode is already Normal
        KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            if selected + 1 < rows.len() {
                selected += 1;
            }
        }
        KeyCode::Enter => {
            let (_, collection_id, _) = rows[selected];
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
                        app.error = Some(e);
                    }
                }
                Err(e) => app.error = Some(e.to_string()),
            }
        }
        _ => {}
    }

    app.mode = Mode::Picker {
        rows,
        selected,
        member,
        entry_id,
    };
}

// The ":" palette: Edit / Fetch / Merge / Delete, scoped to whichever entry
// was selected when it opened.
fn handle_command_key(
    app: &mut App,
    conn: &Connection,
    terminal: &mut ratatui::DefaultTerminal,
    code: KeyCode,
) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::Command { entry_id } = mode else {
        app.mode = mode;
        return;
    };

    match code {
        KeyCode::Esc => {} // app.mode is already Normal
        KeyCode::Char('e') => {
            app.mode = Mode::FieldPicker {
                entry_id,
                selected: 0,
            };
        }
        KeyCode::Char('f') => app.fetch_selected(conn, terminal, entry_id),
        KeyCode::Char('m') => app.begin_merge(entry_id),
        KeyCode::Char('d') => app.begin_delete(entry_id),
        _ => app.mode = Mode::Command { entry_id },
    }
}

// Edit's field-name picker. Enter opens Mode::Input pre-filled with the
// field's current value; Esc backs out to Normal.
fn handle_field_picker_key(app: &mut App, code: KeyCode) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::FieldPicker {
        entry_id,
        mut selected,
    } = mode
    else {
        app.mode = mode;
        return;
    };

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
            let Some(entry) = app.entries.iter().find(|e| e.id == Some(entry_id)) else {
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
fn handle_entry_picker_key(app: &mut App, code: KeyCode) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::EntryPicker {
        keep_id,
        mut filter,
        mut rows,
        mut selected,
    } = mode
    else {
        app.mode = mode;
        return;
    };

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
            rows = app.entry_picker_rows(keep_id, &filter);
            selected = 0;
        }
        KeyCode::Char(c) => {
            filter.push(c);
            rows = app.entry_picker_rows(keep_id, &filter);
            selected = 0;
        }
        KeyCode::Enter => {
            if let Some(drop_id) = rows.get(selected).and_then(|&i| app.entries[i].id) {
                app.confirm_merge(keep_id, drop_id);
                return;
            }
        }
        _ => {}
    }

    app.mode = Mode::EntryPicker {
        keep_id,
        filter,
        rows,
        selected,
    };
}

// Delete/Merge confirm: any key but 'y' cancels. Marks are cleared either
// way, per DESIGN.md's Phase 16 merge rule.
fn handle_confirm_key(app: &mut App, conn: &Connection, code: KeyCode) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let Mode::Confirm { action, .. } = mode else {
        app.mode = mode;
        return;
    };

    if code != KeyCode::Char('y') {
        app.marked.clear();
        return; // app.mode is already Normal
    }

    let result = match action {
        PendingAction::Delete { entry_id } => app
            .entries
            .iter()
            .find(|e| e.id == Some(entry_id))
            .map(|e| e.cite_key.clone())
            .ok_or_else(|| "entry no longer exists".to_string())
            .and_then(|cite_key| db::delete_entry(conn, &cite_key).map_err(|e| e.to_string())),
        PendingAction::Merge { keep_id, drop_id } => db::merge_entries(conn, keep_id, drop_id)
            .map_err(|e| crate::db_error("merge entries", e)),
    };
    if let Err(e) = result {
        app.error = Some(e);
    }

    app.marked.clear();
    // The entry list's shape changed (a row is gone) either way -- reload
    // rather than patch app.entries in place.
    if let Err(e) = app.reload(conn) {
        app.error = Some(e);
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
    },
    // The ":" palette (Edit/Fetch/Merge/Delete), scoped to one entry.
    Command { entry_id: i64 },
    // Edit's field-name list, opened by ":" -> "e".
    FieldPicker { entry_id: i64, selected: usize },
    // Merge's fold-in-entry picker, opened by ":" -> "m" when fewer than two
    // entries are marked. `keep_id` is fixed for the picker's lifetime;
    // `rows` are indices into `App::entries` matching `filter`.
    EntryPicker {
        keep_id: i64,
        filter: String,
        rows: Vec<usize>,
        selected: usize,
    },
    // Delete/merge confirmation. An enum of pending actions (rather than a
    // boxed closure) since there are exactly two call sites.
    Confirm { message: String, action: PendingAction },
}

enum InputKind {
    // Carries the filter that was active before '/' was pressed, so Esc can
    // restore it rather than just clearing it.
    Search { previous: String },
    NewCollection,
    // Edit's value box. `return_selected` is the field picker's row to
    // return to on Enter/Esc, so fixing several fields in one visit doesn't
    // reset the list to the top each time.
    EditField {
        entry_id: i64,
        field: EditField,
        return_selected: usize,
    },
}

#[derive(Clone, Copy)]
enum PendingAction {
    Delete { entry_id: i64 },
    Merge { keep_id: i64, drop_id: i64 },
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
            EditField::Authors => e
                .authors
                .iter()
                .map(|a| match &a.first_name {
                    Some(f) => format!("{}, {}", a.last_name, f),
                    None => a.last_name.clone(),
                })
                .collect::<Vec<_>>()
                .join("; "),
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
            EditField::Journal => entry.journal = (!trimmed.is_empty()).then(|| trimmed.to_string()),
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
    // a picker to choose what folds into it.
    PickDrop(i64),
    // Exactly 2 marked: order already decides keep/drop.
    Pair(i64, i64),
    // 3+ marked: out of scope for one merge.
    TooMany,
}

fn plan_merge(marked: &[i64], selected_id: Option<i64>) -> Option<MergePlan> {
    match marked.len() {
        0 | 1 => selected_id.map(MergePlan::PickDrop),
        2 => Some(MergePlan::Pair(marked[0], marked[1])),
        _ => Some(MergePlan::TooMany),
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
    // Set by a failed write or reload, shown on the footer for one
    // keypress, then cleared by handle_key.
    error: Option<String>,
    should_quit: bool,
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
            attachment_lengths,
            filter: String::new(),
            sort_key: SortKey::Title,
            sort_desc: false,
            marked: Vec::new(),
            focus: Focus::Collections,
            mode: Mode::Normal,
            error: None,
            should_quit: false,
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
        self.view.get(self.table_selected).map(|&i| &self.entries[i])
    }

    // Creates a collection under `parent` (None = root, same as "All
    // Papers" selected) and reloads so the tree's rows/counts pick it up.
    // A DB error is shown on the footer rather than propagated -- a bad
    // name shouldn't end the session.
    fn create_collection(&mut self, conn: &Connection, parent: Option<i64>, name: &str) {
        match db::create_collection_under(conn, parent, name) {
            Ok(_) => {
                if let Err(e) = self.reload(conn) {
                    self.error = Some(e);
                }
            }
            // db_error, not e.to_string(): a rejected name arrives as
            // InvalidParameterName wrapping a message already written for a
            // human, and to_string() prefixes it with "Invalid parameter
            // name:" -- rusqlite's vocabulary leaking onto the footer.
            Err(e) => self.error = Some(crate::db_error("create collection", e)),
        }
    }

    // Opens the collection picker for the currently selected entry. Uses
    // collection_tree directly (not the tree pane's rows) since there's
    // nothing to file a paper into "All Papers" -- that's not a collection.
    fn open_picker(&mut self, conn: &Connection) {
        let Some(entry_id) = self.selected_entry().and_then(|e| e.id) else {
            return;
        };

        let tree = match db::collection_tree(conn) {
            Ok(t) => t,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        if tree.is_empty() {
            self.error = Some("no collections exist yet -- create one with 'n' first".to_string());
            return;
        }

        let member: HashSet<i64> = match db::collections_for_entry(conn, entry_id) {
            Ok(v) => v.into_iter().collect(),
            Err(e) => {
                self.error = Some(e.to_string());
                return;
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
        };
    }

    // Opens every attachment of the selected entry through the system
    // opener. A failure (missing opener, no attachments) is shown on the
    // footer rather than propagated -- a broken path shouldn't end the
    // session.
    fn open_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if entry.attachments.is_empty() {
            self.error = Some(format!("'{}' has no attachments", entry.cite_key));
            return;
        }
        for a in &entry.attachments {
            if let Err(e) = crate::open_path(&a.path) {
                self.error = Some(e);
                return;
            }
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(id) = self.selected_entry().and_then(|e| e.id) {
            toggle_marked(&mut self.marked, id);
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
        Ok(())
    }

    // Edit's Input box, on Enter: applies the field to a clone of the
    // current entry and writes it with db::update_entry -- the same
    // function `ferref edit` uses, so a TUI edit and a CLI edit go through
    // one write path.
    fn apply_field_edit(&mut self, conn: &Connection, entry_id: i64, field: EditField, raw: &str) {
        let Some(current) = self.entries.iter().find(|e| e.id == Some(entry_id)) else {
            return;
        };
        let mut updated = current.clone();
        if let Err(e) = field.apply(&mut updated, raw) {
            self.error = Some(e);
            return;
        }
        match db::update_entry(conn, &updated) {
            Ok(()) => {
                if let Err(e) = self.refresh_entry(conn, entry_id) {
                    self.error = Some(e);
                }
            }
            Err(e) => self.error = Some(crate::db_error("update entry", e)),
        }
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
        let Some(cite_key) = self
            .entries
            .iter()
            .find(|e| e.id == Some(entry_id))
            .map(|e| e.cite_key.clone())
        else {
            return;
        };

        self.error = Some(format!("Fetching PDF for '{cite_key}'\u{2026}"));
        let _ = terminal.draw(|frame| draw(frame, self));

        match crate::fetch_pdf_for_entry(conn, &cite_key, None) {
            Ok(crate::FetchOutcome::NoPdfFound { is_oa, .. }) => {
                self.error = Some(if is_oa {
                    format!("'{cite_key}' is open access, but Unpaywall has no direct PDF link")
                } else {
                    format!("No open-access copy found for '{cite_key}'")
                });
            }
            Ok(crate::FetchOutcome::Downloaded { path, extraction, .. }) => {
                self.error = Some(match extraction {
                    Ok(chars) => format!("Downloaded '{path}' ({chars} chars extracted)"),
                    Err(e) => format!("Downloaded '{path}', but extraction failed: {e}"),
                });
                if let Err(e) = self.refresh_entry(conn, entry_id) {
                    self.error = Some(e);
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    // Builds the rows for the merge entry-picker: every entry except
    // `keep_id` itself, narrowed by `matches_filter` (the same
    // case-insensitive substring match "/" search uses).
    fn entry_picker_rows(&self, keep_id: i64, filter: &str) -> Vec<usize> {
        let needle = filter.to_lowercase();
        (0..self.entries.len())
            .filter(|&i| {
                self.entries[i].id != Some(keep_id) && matches_filter(&self.entries[i], &needle)
            })
            .collect()
    }

    // ":" -> "m": see MergePlan / plan_merge for the branching rule itself.
    fn begin_merge(&mut self, entry_id: i64) {
        match plan_merge(&self.marked, Some(entry_id)) {
            Some(MergePlan::PickDrop(keep_id)) => {
                self.mode = Mode::EntryPicker {
                    keep_id,
                    filter: String::new(),
                    rows: self.entry_picker_rows(keep_id, ""),
                    selected: 0,
                };
            }
            Some(MergePlan::Pair(keep_id, drop_id)) => self.confirm_merge(keep_id, drop_id),
            Some(MergePlan::TooMany) => {
                self.error = Some("merge only supports two entries at a time".to_string());
            }
            None => {}
        }
    }

    fn confirm_merge(&mut self, keep_id: i64, drop_id: i64) {
        let title_of = |id: i64| {
            self.entries
                .iter()
                .find(|e| e.id == Some(id))
                .map(|e| e.title.clone())
                .unwrap_or_default()
        };
        self.mode = Mode::Confirm {
            message: format!("Merge '{}' into '{}'? y/n", title_of(drop_id), title_of(keep_id)),
            action: PendingAction::Merge { keep_id, drop_id },
        };
    }

    // ":" -> "d".
    fn begin_delete(&mut self, entry_id: i64) {
        let Some(entry) = self.entries.iter().find(|e| e.id == Some(entry_id)) else {
            return;
        };
        self.mode = Mode::Confirm {
            message: format!("Delete '{}' [{}]? y/n", entry.title, entry.cite_key),
            action: PendingAction::Delete { entry_id },
        };
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
                self.rebuild_view();
            }
            Err(e) => self.error = Some(e),
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
    }

    fn table_home(&mut self) {
        self.table_selected = 0;
    }

    fn table_end(&mut self) {
        if !self.view.is_empty() {
            self.table_selected = self.view.len() - 1;
        }
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
    if len == 0 {
        0
    } else {
        selected.min(len - 1)
    }
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
            if desc {
                ord.reverse()
            } else {
                ord
            }
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

    let outer = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
    let cols = Layout::horizontal([
        Constraint::Length(26),
        Constraint::Min(20),
        Constraint::Length(34),
    ])
    .split(outer[0]);

    draw_tree(frame, cols[0], app);
    draw_table(frame, cols[1], app);
    draw_details(frame, cols[2], app);
    draw_footer(frame, outer[1], app);

    match &app.mode {
        Mode::Picker {
            rows,
            selected,
            member,
            ..
        } => draw_picker(frame, area, rows, *selected, member),
        Mode::Command { entry_id } => draw_command(frame, area, app, *entry_id),
        Mode::FieldPicker { entry_id, selected } => {
            draw_field_picker(frame, area, app, *entry_id, *selected)
        }
        Mode::EntryPicker {
            filter,
            rows,
            selected,
            ..
        } => draw_entry_picker(frame, area, app, filter, rows, *selected),
        Mode::Confirm { message, .. } => draw_confirm(frame, area, message),
        Mode::Normal | Mode::Input(..) => {}
    }
}

fn pane_block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(style)
        .title_style(style)
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
        .block(pane_block("COLLECTIONS".to_string(), app.focus == Focus::Collections))
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
        .map(|e| {
            // A colored marker in a leading column, not just the selection
            // highlight -- a mark must stay visible after the cursor moves
            // off the row, which REVERSED alone wouldn't show.
            let marked = e.id.is_some_and(|id| app.marked.contains(&id));
            let mark_cell = if marked {
                Cell::from("\u{25cf}").style(Style::default().fg(Color::Yellow))
            } else {
                Cell::from(" ")
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
        })
        .collect();

    let mut state = TableState::default();
    if !app.view.is_empty() {
        state.select(Some(app.table_selected));
    }

    let arrow = if app.sort_desc { '\u{2193}' } else { '\u{2191}' };
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
        let names: Vec<String> = e
            .authors
            .iter()
            .map(|a| match &a.first_name {
                Some(f) => format!("{}, {}", a.last_name, f),
                None => a.last_name.clone(),
            })
            .collect();
        lines.push(Line::raw(names.join("; ")));
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
    let text = match app.selected_entry() {
        None => Text::from("No entries."),
        Some(e) => {
            let lengths = e.id.and_then(|id| app.attachment_lengths.get(&id));
            Text::from(details_lines(e, lengths))
        }
    };

    let para = Paragraph::new(text)
        .block(pane_block("DETAILS".to_string(), app.focus == Focus::Details))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

// Priority order: a pending error beats everything (it's transient, shown
// once); then the input prompt, so the user can see what they're typing;
// then the active filter, so it doesn't silently vanish from view; then the
// keymap.
fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = if let Some(err) = &app.error {
        format!(" {err}")
    } else {
        match &app.mode {
            Mode::Input(InputKind::Search { .. }, buffer) => format!(" /{buffer}"),
            Mode::Input(InputKind::NewCollection, buffer) => {
                format!(" New collection: {buffer}")
            }
            Mode::Input(InputKind::EditField { field, .. }, buffer) => {
                format!(" {}: {buffer}", field.label())
            }
            Mode::Picker { .. } => " Enter toggle \u{b7} jk move \u{b7} Esc/q close".to_string(),
            Mode::Command { .. } => {
                " e edit \u{b7} f fetch \u{b7} m merge \u{b7} d delete \u{b7} Esc close".to_string()
            }
            Mode::FieldPicker { .. } => " Enter edit \u{b7} jk move \u{b7} Esc back".to_string(),
            Mode::EntryPicker { .. } => {
                " type to filter \u{b7} \u{2191}\u{2193} move \u{b7} Enter pick \u{b7} Esc cancel"
                    .to_string()
            }
            Mode::Confirm { message, .. } => format!(" {message}"),
            Mode::Normal if !app.filter.is_empty() => format!(" filter: {}", app.filter),
            Mode::Normal => {
                " Tab pane \u{b7} jk move \u{b7} / search \u{b7} s sort \u{b7} n new \u{b7} \
                 c file \u{b7} o open \u{b7} space mark \u{b7} : cmd \u{b7} r reload \u{b7} q quit"
                    .to_string()
            }
        }
    };
    let footer = Paragraph::new(text);
    frame.render_widget(footer, area);
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
) {
    let width = frame_area.width.saturating_sub(6).clamp(10, 60);
    let height = ((rows.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    frame.render_widget(Clear, popup);

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
        .block(Block::default().title("File into…").borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// The ":" palette -- a small fixed list, titled with the scoped entry's
// cite_key so it's clear which paper the four actions apply to.
fn draw_command(frame: &mut Frame, frame_area: Rect, app: &App, entry_id: i64) {
    let cite_key = app
        .entries
        .iter()
        .find(|e| e.id == Some(entry_id))
        .map(|e| e.cite_key.as_str())
        .unwrap_or("");

    let width = 26u16.min(frame_area.width.saturating_sub(4)).max(12);
    let height = 6u16.min(frame_area.height.saturating_sub(4)).max(3);
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    frame.render_widget(Clear, popup);
    let items = vec![
        ListItem::new("e  Edit field"),
        ListItem::new("f  Fetch PDF"),
        ListItem::new("m  Merge"),
        ListItem::new("d  Delete"),
    ];
    let list = List::new(items).block(Block::default().title(cite_key.to_string()).borders(Borders::ALL));
    frame.render_widget(list, popup);
}

// Edit's field-name list: label plus each field's current value, so picking
// one is informed rather than a guess at what's already there.
fn draw_field_picker(frame: &mut Frame, frame_area: Rect, app: &App, entry_id: i64, selected: usize) {
    let Some(entry) = app.entries.iter().find(|e| e.id == Some(entry_id)) else {
        return;
    };

    let width = frame_area.width.saturating_sub(6).clamp(20, 70);
    let height = ((EditField::ALL.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    frame.render_widget(Clear, popup);

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
        .block(Block::default().title("Edit field").borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, popup, &mut state);
}

// Merge's fold-in-entry picker: a filter box folded into the title (there's
// no separate input line in the popup) plus the matching entries, title and
// cite_key both shown since either might be what the user remembers.
fn draw_entry_picker(
    frame: &mut Frame,
    frame_area: Rect,
    app: &App,
    filter: &str,
    rows: &[usize],
    selected: usize,
) {
    let width = frame_area.width.saturating_sub(6).clamp(20, 70);
    let height = ((rows.len() as u16) + 2)
        .min(frame_area.height.saturating_sub(4))
        .max(3);
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    frame.render_widget(Clear, popup);

    let text_width = (width as usize).saturating_sub(2);
    let items: Vec<ListItem> = rows
        .iter()
        .map(|&i| {
            let e = &app.entries[i];
            let label = format!("{} ({})", e.title, e.cite_key);
            ListItem::new(truncate_display(&label, text_width))
        })
        .collect();

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(selected));
    }

    let title = if filter.is_empty() {
        "Merge into…".to_string()
    } else {
        format!("Merge into… /{filter}")
    };
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
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
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let popup = Rect { x, y, width, height };

    frame.render_widget(Clear, popup);
    let para = Paragraph::new(message.to_string())
        .block(Block::default().borders(Borders::ALL))
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
        let seq: Vec<(usize, Option<i64>)> = vec![
            (0, None),
            (1, Some(1)),
            (2, Some(2)),
            (1, Some(3)),
        ];

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
        assert_eq!(truncate_display("\u{738b}\u{738b}\u{738b}", 3), "\u{738b}\u{2026}");
        assert_eq!(truncate_display("hello", 1), "\u{2026}");
        assert_eq!(truncate_display("hello", 0), "");
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
        assert_eq!(view, vec![2, 0, 1], "descending: 2021, 2019, then still-missing-last");
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
            attachment_lengths: HashMap::new(),
            filter: String::new(),
            sort_key: SortKey::Title,
            sort_desc: false,
            marked: Vec::new(),
            focus: Focus::Entries,
            mode: Mode::Normal,
            error: None,
            should_quit: false,
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
        // 3+: out of scope for one merge.
        assert_eq!(plan_merge(&[1, 2, 3], Some(9)), Some(MergePlan::TooMany));
    }
}
