// Reusable data table component for training and playback screens.
//
// Owns the cell/row state grid, viewport position, and cached column widths.
// Rendering is a pure function over `&DataSet` and `&TableState`.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style, Stylize},
    widgets::{Cell, Row, Table},
};

use crate::data::DataSet;

// ── State types ───────────────────────────────────────────────────────────────

/// The state of a single data cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellState {
    /// Not yet filled in.
    Pending,
    /// Value has been entered.
    Done,
}

/// The lifecycle state of a data row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowState {
    /// Not yet started.
    Upcoming,
    /// Currently being processed.
    InProgress,
    /// All steps complete.
    Done,
}

// ── TableState ────────────────────────────────────────────────────────────────

const MIN_COL_WIDTH: u16 = 4;
const MAX_COL_WIDTH: u16 = 30;

/// Holds cell/row states, viewport position, and cached column widths.
pub(crate) struct TableState {
    /// `[row][col]` cell states.
    cell_states: Vec<Vec<CellState>>,
    row_states: Vec<RowState>,
    /// Index of the first visible row.
    pub(crate) viewport_row: usize,
    /// Index of the first visible column (horizontal scroll).
    pub(crate) viewport_col: usize,
    /// Cached column widths, computed from the full dataset on construction.
    column_widths: Vec<u16>,
}

impl TableState {
    /// Create a new `TableState` for a dataset with the given shape.
    ///
    /// All cells start as `Pending`, all rows as `Upcoming`.
    /// Column widths are computed from the full dataset and cached.
    pub(crate) fn new(row_count: usize, col_count: usize, dataset: &DataSet) -> Self {
        let cell_states = vec![vec![CellState::Pending; col_count]; row_count];
        let row_states = vec![RowState::Upcoming; row_count];
        let column_widths = compute_col_widths(dataset);

        Self {
            cell_states,
            row_states,
            viewport_row: 0,
            viewport_col: 0,
            column_widths,
        }
    }

    /// Set the state of a single cell. Panics if indices are out of bounds.
    pub(crate) fn set_cell_state(&mut self, row: usize, col: usize, state: CellState) {
        self.cell_states[row][col] = state;
    }

    /// Set the state of a row. Panics if `row` is out of bounds.
    pub(crate) fn set_row_state(&mut self, row: usize, state: RowState) {
        self.row_states[row] = state;
    }

    /// Get the state of a single cell. Panics if indices are out of bounds.
    pub(crate) fn cell_state(&self, row: usize, col: usize) -> CellState {
        self.cell_states[row][col]
    }

    /// Get the state of a row. Panics if `row` is out of bounds.
    pub(crate) fn row_state(&self, row: usize) -> RowState {
        self.row_states[row]
    }

    /// Recalculate `viewport_row` so the active row stays visible,
    /// roughly in the top third of the visible area.
    ///
    /// The "active row" is the first `InProgress` row, or the last `Done` row
    /// if none is in progress.
    pub(crate) fn update_viewport(&mut self, visible_rows: u16) {
        let visible = usize::from(visible_rows);
        if visible == 0 {
            return;
        }

        let active = self.active_row();

        // Target position: top third of the visible area.
        let target_offset = (visible / 3).max(1).min(visible.saturating_sub(1));

        // Desired viewport_row so the active row sits at target_offset.
        let desired = active.saturating_sub(target_offset);

        // Clamp: don't scroll past the last row.
        let max_start = self.row_states.len().saturating_sub(visible);
        self.viewport_row = desired.min(max_start);
    }

    /// The index of the first `InProgress` row, or the last `Done` row,
    /// or 0 if none qualify.
    fn active_row(&self) -> usize {
        // First InProgress.
        if let Some(idx) = self
            .row_states
            .iter()
            .position(|&s| s == RowState::InProgress)
        {
            return idx;
        }
        // Last Done.
        if let Some(idx) = self
            .row_states
            .iter()
            .rposition(|&s| s == RowState::Done)
        {
            return idx;
        }
        0
    }

    /// Cached column widths (one per column in the dataset).
    #[cfg(test)]
    fn column_widths(&self) -> &[u16] {
        &self.column_widths
    }
}

// ── Style resolver ────────────────────────────────────────────────────────────

/// Map semantic `(CellState, RowState)` to a ratatui `Style`.
///
/// This is the single place that controls visual treatment.
/// Changing how "Done" looks means changing only this function.
fn cell_style(cell: CellState, row: RowState) -> Style {
    match (cell, row) {
        (CellState::Done, _) => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        (CellState::Pending, RowState::InProgress) => Style::default(),
        (CellState::Pending, RowState::Upcoming | RowState::Done) => {
            Style::default().fg(Color::DarkGray)
        }
    }
}

/// Style for the row-number cell on the left.
fn row_num_style(row: RowState) -> Style {
    match row {
        RowState::InProgress => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        RowState::Done => Style::default().fg(Color::Green),
        RowState::Upcoming => Style::default().fg(Color::DarkGray),
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// Render the data table into `area`.
///
/// - Header row uses dataset headers or "Column N" fallback.
/// - Row number column on the left, styled by row state.
/// - Data cells styled by `cell_style`.
/// - Horizontal viewport: columns starting from `state.viewport_col` that fit.
/// - Vertical viewport: rows starting from `state.viewport_row` that fit.
/// - Cell values are sanitized (non-printable chars stripped) and truncated.
pub(crate) fn draw_table(frame: &mut Frame, area: Rect, dataset: &DataSet, state: &TableState) {
    let col_count = dataset.column_count();
    let row_count = dataset.row_count();

    // Row-number column width: wide enough for the largest row number.
    let row_num_width = u16::try_from(row_count.to_string().len()).unwrap_or(4).max(2);

    // Determine which columns fit starting from viewport_col.
    // Available width after row-number column and its separator.
    let available_data_width = area.width.saturating_sub(row_num_width + 1);
    let visible_cols = visible_column_range(state, col_count, available_data_width);

    // Build ratatui column constraints: row-num + selected columns.
    let mut constraints = Vec::with_capacity(1 + visible_cols.len());
    constraints.push(ratatui::layout::Constraint::Length(row_num_width));
    for &col in &visible_cols {
        constraints.push(ratatui::layout::Constraint::Length(
            state.column_widths[col],
        ));
    }

    // Header row.
    let header = build_header(dataset, &visible_cols, row_num_width);

    // Data rows: only those in viewport.
    let available_rows = area.height.saturating_sub(1); // minus header
    let data_rows: Vec<Row<'_>> = (0..row_count)
        .skip(state.viewport_row)
        .take(usize::from(available_rows))
        .map(|row_idx| build_row(dataset, state, row_idx, &visible_cols, row_num_width))
        .collect();

    let table = Table::new(data_rows, constraints).header(header);
    frame.render_widget(table, area);
}

/// Returns the list of column indices visible from `viewport_col` that fit
/// within `available_width` terminal columns.
fn visible_column_range(state: &TableState, col_count: usize, available_width: u16) -> Vec<usize> {
    let mut cols = Vec::new();
    let mut used: u16 = 0;
    for col in state.viewport_col..col_count {
        let w = state.column_widths[col];
        // +1 for the inter-column gap ratatui adds between cells.
        let needed = if cols.is_empty() { w } else { w + 1 };
        if used + needed > available_width {
            break;
        }
        used += needed;
        cols.push(col);
    }
    cols
}

fn build_header(
    dataset: &DataSet,
    visible_cols: &[usize],
    row_num_width: u16,
) -> Row<'static> {
    let num_cell = Cell::new(" ".repeat(usize::from(row_num_width))).dark_gray();

    let col_cells: Vec<Cell<'static>> = visible_cols
        .iter()
        .map(|&col| {
            let label: String = dataset
                .headers()
                .and_then(|h| h.get(col))
                .filter(|s| !s.is_empty())
                .map_or_else(|| format!("Column {}", col + 1), |s| truncate(s));
            Cell::new(label).dark_gray().bold()
        })
        .collect();

    let mut cells: Vec<Cell<'static>> = vec![num_cell];
    cells.extend(col_cells);
    Row::new(cells).height(1)
}

fn build_row<'a>(
    dataset: &'a DataSet,
    state: &TableState,
    row_idx: usize,
    visible_cols: &[usize],
    row_num_width: u16,
) -> Row<'a> {
    let rs = state.row_state(row_idx);

    // Row number cell — 1-indexed display.
    let display_num = row_idx + 1;
    let num_str = format!("{display_num:>width$}", width = usize::from(row_num_width));
    let num_cell = Cell::new(num_str).style(row_num_style(rs));

    let data_cells = visible_cols.iter().map(|&col| {
        let raw: &str = dataset
            .row(row_idx)
            .and_then(|r| r.get(col))
            .map_or("", String::as_str);
        let sanitized = sanitize(raw);
        let truncated = truncate(&sanitized);
        let cs = state.cell_state(row_idx, col);
        Cell::new(truncated).style(cell_style(cs, rs))
    });

    let mut cells: Vec<Cell<'_>> = vec![num_cell];
    cells.extend(data_cells);
    Row::new(cells).height(1)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Compute column widths from the full dataset.
///
/// For each column: max of header width and all cell widths, capped at
/// `MAX_COL_WIDTH`, floored at `MIN_COL_WIDTH`.
fn compute_col_widths(dataset: &DataSet) -> Vec<u16> {
    let col_count = dataset.column_count();

    // Seed with header widths (or fallback label widths).
    let mut widths: Vec<u16> = (0..col_count)
        .map(|col| {
            let header_len = dataset
                .headers()
                .and_then(|h| h.get(col))
                .map_or(0, String::len);
            // "Column N" fallback width.
            let fallback_len = "Column ".len() + (col + 1).to_string().len();
            let w = header_len.max(fallback_len).min(usize::from(MAX_COL_WIDTH));
            u16::try_from(w).unwrap_or(MAX_COL_WIDTH).max(MIN_COL_WIDTH)
        })
        .collect();

    // Expand with data cell widths.
    for row in dataset.rows() {
        for (col, cell) in row.iter().enumerate().take(col_count) {
            let capped = cell.len().min(usize::from(MAX_COL_WIDTH));
            let w = u16::try_from(capped).unwrap_or(MAX_COL_WIDTH);
            if w > widths[col] {
                widths[col] = w;
            }
        }
    }

    widths
}

/// Strip non-printable characters (0x00–0x1f and 0x7f) from a string.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| c >= '\x20' && c != '\x7f')
        .collect()
}

/// Truncate a string to `MAX_COL_WIDTH` characters, appending `…` if truncated.
fn truncate(s: &str) -> String {
    let max = usize::from(MAX_COL_WIDTH);
    let mut chars = s.char_indices().skip(max.saturating_sub(1));
    match chars.next() {
        None => s.to_owned(),                                      // < max chars
        Some((_, _)) if chars.next().is_none() => s.to_owned(),    // exactly max chars
        Some((byte_pos, _)) => {
            let mut t = s[..byte_pos].to_owned();
            t.push('…');
            t
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::data::{self, Delimiter};

    fn small_ds() -> DataSet {
        data::from_file(Path::new("examples/data/small.tsv"), Delimiter::Tab, true).unwrap()
    }

    // ── Initialization ────────────────────────────────────────────────────────

    #[test]
    fn new_initializes_all_pending_upcoming() {
        let ds = small_ds();
        let rows = ds.row_count();
        let cols = ds.column_count();
        let state = TableState::new(rows, cols, &ds);

        for r in 0..rows {
            assert_eq!(state.row_state(r), RowState::Upcoming);
            for c in 0..cols {
                assert_eq!(state.cell_state(r, c), CellState::Pending);
            }
        }
    }

    // ── State transitions ─────────────────────────────────────────────────────

    #[test]
    fn set_cell_state_round_trips() {
        let ds = small_ds();
        let mut state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        state.set_cell_state(0, 0, CellState::Done);
        assert_eq!(state.cell_state(0, 0), CellState::Done);
        assert_eq!(state.cell_state(0, 1), CellState::Pending);
    }

    #[test]
    fn set_row_state_round_trips() {
        let ds = small_ds();
        let mut state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        state.set_row_state(1, RowState::InProgress);
        assert_eq!(state.row_state(1), RowState::InProgress);
        assert_eq!(state.row_state(0), RowState::Upcoming);
    }

    // ── Column widths ─────────────────────────────────────────────────────────

    fn check_widths(widths: &[u16]) {
        for &w in widths {
            assert!(w >= MIN_COL_WIDTH, "width {w} below minimum");
            assert!(w <= MAX_COL_WIDTH, "width {w} above maximum");
        }
    }

    #[test]
    fn column_widths_small() {
        let ds = small_ds();
        let state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        check_widths(state.column_widths());
        // "first_name" is 10 chars — expect width 10.
        assert_eq!(state.column_widths()[0], 10);
    }

    #[test]
    fn column_widths_wide() {
        let ds =
            data::from_file(Path::new("examples/data/wide.tsv"), Delimiter::Tab, true).unwrap();
        let state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        check_widths(state.column_widths());
    }

    #[test]
    fn column_widths_tall() {
        let ds =
            data::from_file(Path::new("examples/data/tall.tsv"), Delimiter::Tab, true).unwrap();
        let state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        check_widths(state.column_widths());
    }

    #[test]
    fn column_widths_large() {
        let ds =
            data::from_file(Path::new("examples/data/large.tsv"), Delimiter::Tab, true).unwrap();
        let state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        check_widths(state.column_widths());
    }

    #[test]
    fn column_widths_deep() {
        let ds =
            data::from_file(Path::new("examples/data/deep.tsv"), Delimiter::Tab, true).unwrap();
        let state = TableState::new(ds.row_count(), ds.column_count(), &ds);
        check_widths(state.column_widths());
    }

    // ── Viewport ──────────────────────────────────────────────────────────────

    #[test]
    fn viewport_row0_inprogress_stays_at_top() {
        let ds = small_ds();
        let rows = 50_usize.max(ds.row_count());
        let cols = ds.column_count();
        // Build a dataset-like view: we test viewport logic with synthetic state.
        let mut state = TableState::new(rows, cols, &ds);
        // Re-expand cell_states and row_states to 50 rows.
        state.cell_states = vec![vec![CellState::Pending; cols]; rows];
        state.row_states = vec![RowState::Upcoming; rows];

        state.set_row_state(0, RowState::InProgress);
        state.update_viewport(20);
        assert_eq!(state.viewport_row, 0);
    }

    #[test]
    fn viewport_mid_inprogress_scrolls() {
        let ds = small_ds();
        let rows = 50_usize;
        let cols = ds.column_count();
        let mut state = TableState::new(rows, cols, &ds);
        state.cell_states = vec![vec![CellState::Pending; cols]; rows];
        state.row_states = vec![RowState::Upcoming; rows];

        state.set_row_state(25, RowState::InProgress);
        state.update_viewport(20);
        // active row 25, target_offset = 20/3 = 6 (min 1)
        // desired = 25 - 6 = 19, max_start = 50 - 20 = 30
        assert_eq!(state.viewport_row, 19);
    }

    #[test]
    fn viewport_last_row_inprogress() {
        let ds = small_ds();
        let rows = 50_usize;
        let cols = ds.column_count();
        let mut state = TableState::new(rows, cols, &ds);
        state.cell_states = vec![vec![CellState::Pending; cols]; rows];
        state.row_states = vec![RowState::Upcoming; rows];

        state.set_row_state(49, RowState::InProgress);
        state.update_viewport(20);
        // desired = 49 - 6 = 43, max_start = 50 - 20 = 30
        assert_eq!(state.viewport_row, 30);
    }

    // ── Sanitization ──────────────────────────────────────────────────────────

    #[test]
    fn sanitize_strips_escape_sequences() {
        let raw = "\x1b[31mred\x1b[0m";
        let clean = sanitize(raw);
        // ESC (0x1b) is stripped; visible chars remain.
        assert!(!clean.contains('\x1b'));
        assert!(clean.contains('r'));
        assert!(clean.contains('e'));
        assert!(clean.contains('d'));
    }

    #[test]
    fn sanitize_strips_all_control_chars() {
        let raw = "hello\x00world\x1fend\x7fhere";
        let clean = sanitize(raw);
        assert_eq!(clean, "helloworldendhere");
    }
}
