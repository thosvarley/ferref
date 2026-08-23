// Read-only terminal UI over the same SQLite file the CLI writes. No
// editing, deletion, or tagging happens here -- the CLI stays the only way
// to change a library, so a mis-keypress in the TUI can't corrupt one.
//
// Data is fetched only when state changes (on load, on a collection
// selection change, or on manual reload) and cached in `App`; the render
// function (`draw`) never touches the `Connection`.

use std::collections::HashMap;
use std::collections::HashSet;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::tty::IsTty;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap};
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
                handle_key(app, conn, key.code);
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

fn handle_key(app: &mut App, conn: &Connection, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab => app.focus = app.focus.next(),
        KeyCode::BackTab => app.focus = app.focus.prev(),
        KeyCode::Char('r') => {
            // A failed reload leaves the previous state in place rather
            // than crashing the session over a transient DB error.
            let _ = app.reload(conn);
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
        KeyCode::Left | KeyCode::Char('h') if app.focus == Focus::Collections => {
            app.collapse_or_to_parent(conn)
        }
        KeyCode::Right | KeyCode::Char('l') if app.focus == Focus::Collections => app.expand(),
        KeyCode::PageUp if app.focus == Focus::Entries => app.move_table(-10),
        KeyCode::PageDown if app.focus == Focus::Entries => app.move_table(10),
        KeyCode::Home if app.focus == Focus::Entries => app.table_home(),
        KeyCode::End if app.focus == Focus::Entries => app.table_end(),
        _ => {}
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
}

// One row of the rendered tree, including the synthetic "All Papers" root
// (id = None) that db::collection_tree never produces -- it isn't a DB row,
// it means "no collection filter".
struct TreeRow {
    id: Option<i64>,
    depth: usize,
    name: String,
    entry_count: i64,
    // Filter::collection value; None for "All Papers".
}

type AttachmentLengths = HashMap<i64, Vec<(String, Option<i64>)>>;

struct App {
    rows: Vec<TreeRow>,
    collapsed: HashSet<Option<i64>>,
    selected_row: usize, // index into `rows` (not the visible subset)

    entries: Vec<Entry>,
    table_selected: usize,
    // entry id -> [(attachment path, extracted-text char length)], loaded
    // alongside `entries` so the details pane never queries during render.
    attachment_lengths: AttachmentLengths,

    focus: Focus,
    should_quit: bool,
}

impl App {
    fn load(conn: &Connection) -> Result<Self, String> {
        let rows = load_tree(conn)?;
        let (entries, attachment_lengths) = load_entries(conn, None)?;
        Ok(Self {
            rows,
            collapsed: HashSet::new(),
            selected_row: 0,
            entries,
            table_selected: 0,
            attachment_lengths,
            focus: Focus::Collections,
            should_quit: false,
        })
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
        self.table_selected = clamp_selection(self.table_selected, self.entries.len());
        Ok(())
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
    // pane over a transient error.
    fn select_row(&mut self, conn: &Connection, row_idx: usize) {
        self.selected_row = row_idx;
        let collection_id = self.rows[row_idx].id;
        if let Ok((entries, lengths)) = load_entries(conn, collection_id) {
            self.entries = entries;
            self.attachment_lengths = lengths;
            self.table_selected = 0;
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
        if self.entries.is_empty() {
            return;
        }
        let len = self.entries.len() as i32;
        let new = (self.table_selected as i32 + delta).clamp(0, len - 1);
        self.table_selected = new as usize;
    }

    fn table_home(&mut self) {
        self.table_selected = 0;
    }

    fn table_end(&mut self) {
        if !self.entries.is_empty() {
            self.table_selected = self.entries.len() - 1;
        }
    }
}

fn load_tree(conn: &Connection) -> Result<Vec<TreeRow>, String> {
    let tree = db::collection_tree(conn).map_err(|e| e.to_string())?;
    let total = db::list_entries(conn, &Filter::default(), false)
        .map_err(|e| e.to_string())?
        .len() as i64;

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
            entry_count: c.entry_count,
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

    let mut lengths = HashMap::new();
    for e in &entries {
        if let Some(id) = e.id {
            let v = db::attachment_text_lengths(conn, id).map_err(|e| e.to_string())?;
            lengths.insert(id, v);
        }
    }
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

// Truncates `s` to at most `max_width` display columns, appending "…" if it
// doesn't fit. Character-safe by construction (built one whole char at a
// time, never sliced by byte index) and width-safe: width is measured with
// ratatui's own text width (which is unicode-width under the hood), not
// `.len()` or `.chars().count()`, so wide (CJK) characters count as 2
// columns and combining marks don't inflate the count.
fn truncate_display(s: &str, max_width: usize) -> String {
    use ratatui::text::Span;

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
    draw_footer(frame, outer[1]);
}

fn pane_block(title: &str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .title(title.to_string())
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
        .block(pane_block("COLLECTIONS", app.focus == Focus::Collections))
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

fn draw_table(frame: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec!["Title", "Authors", "Year", "Journal"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .entries
        .iter()
        .map(|e| {
            Row::new(vec![
                truncate_display(&e.title, 60),
                truncate_display(&author_summary(&e.authors), 14),
                e.year.map(|y| y.to_string()).unwrap_or_default(),
                truncate_display(e.journal.as_deref().unwrap_or(""), 14),
            ])
        })
        .collect();

    let mut state = TableState::default();
    if !app.entries.is_empty() {
        state.select(Some(app.table_selected));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Min(10),
            Constraint::Length(14),
            Constraint::Length(4),
            Constraint::Length(14),
        ],
    )
    .header(header)
    .block(pane_block("ENTRIES", app.focus == Focus::Entries))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(table, area, &mut state);
}

fn details_text(e: &Entry, lengths: Option<&Vec<(String, Option<i64>)>>) -> String {
    let mut lines = vec![e.title.clone()];

    if !e.authors.is_empty() {
        let names: Vec<String> = e
            .authors
            .iter()
            .map(|a| match &a.first_name {
                Some(f) => format!("{}, {}", a.last_name, f),
                None => a.last_name.clone(),
            })
            .collect();
        lines.push(names.join("; "));
    }

    let mut meta = Vec::new();
    if let Some(y) = e.year {
        meta.push(y.to_string());
    }
    if let Some(j) = &e.journal {
        meta.push(j.clone());
    }
    if !meta.is_empty() {
        lines.push(meta.join(" \u{b7} "));
    }
    if let Some(v) = &e.volume {
        lines.push(format!("vol. {v}"));
    }
    if let Some(p) = &e.pages {
        lines.push(format!("pp. {p}"));
    }
    if let Some(d) = &e.doi {
        lines.push(format!("doi: {d}"));
    }
    if let Some(u) = &e.url {
        lines.push(format!("url: {u}"));
    }
    if !e.tags.is_empty() {
        lines.push(
            e.tags
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }

    for (i, a) in e.attachments.iter().enumerate() {
        let status = match lengths.and_then(|l| l.get(i)) {
            Some((_, Some(n))) => format!("text: {n} chars"),
            Some((_, None)) => "text: not extracted".to_string(),
            None => "text: unknown".to_string(),
        };
        lines.push(format!("{} ({status})", a.path));
    }

    if let Some(abs) = &e.abstract_text {
        lines.push(String::new());
        lines.push(abs.clone());
    }

    lines.join("\n")
}

fn draw_details(frame: &mut Frame, area: Rect, app: &App) {
    let text = match app.entries.get(app.table_selected) {
        None => "No entries.".to_string(),
        Some(e) => {
            let lengths = e.id.and_then(|id| app.attachment_lengths.get(&id));
            details_text(e, lengths)
        }
    };

    let para = Paragraph::new(text)
        .block(pane_block("DETAILS", app.focus == Focus::Details))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let footer = Paragraph::new(" Tab pane \u{b7} \u{2191}\u{2193} move \u{b7} \u{2190}\u{2192} fold \u{b7} r reload \u{b7} q quit");
    frame.render_widget(footer, area);
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
