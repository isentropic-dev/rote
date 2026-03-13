// Terminal user interface — data preview screen.

use std::io;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use crate::data::DataSet;

const MAX_PREVIEW_ROWS: usize = 20;
const MIN_COL_WIDTH: u16 = 4;
const MAX_COL_WIDTH: u16 = 30;

/// The outcome of running the TUI data screen.
#[must_use]
pub enum Outcome {
    /// User confirmed; the dataset is configured (headers set if chosen).
    Continue(DataSet),
    /// User quit the application.
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// Waiting for the user to press `h` or `d`.
    WaitingForHeaderChoice,
    /// User chose; showing the configured table summary.
    ShowingDataSummary { with_headers: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopOutcome {
    Continue { with_headers: bool },
    Quit,
}

/// Run the TUI data preview screen.
///
/// Expects a [`DataSet`] loaded without header detection (`has_headers = false`).
/// Asks the user whether the first row is headers or data, then returns the
/// appropriately configured [`DataSet`].
///
/// # Errors
///
/// Returns an error if drawing or event reading fails.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dataset: DataSet,
) -> io::Result<Outcome> {
    match event_loop(terminal, &dataset).await? {
        LoopOutcome::Continue { with_headers: true } => {
            Ok(Outcome::Continue(dataset.with_first_row_as_headers()))
        }
        LoopOutcome::Continue {
            with_headers: false,
        } => Ok(Outcome::Continue(dataset)),
        LoopOutcome::Quit => Ok(Outcome::Quit),
    }
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dataset: &DataSet,
) -> io::Result<LoopOutcome> {
    let col_widths = compute_col_widths(dataset);
    let mut screen = Screen::WaitingForHeaderChoice;
    let mut events = EventStream::new();

    loop {
        terminal.draw(|frame| draw(frame, dataset, screen, &col_widths))?;

        let Some(event_result) = events.next().await else {
            return Ok(LoopOutcome::Quit);
        };
        let event = event_result?;

        if let Some(outcome) = handle_event(&event, &mut screen) {
            return Ok(outcome);
        }
    }
}

fn handle_event(event: &Event, screen: &mut Screen) -> Option<LoopOutcome> {
    let Event::Key(key) = event else {
        // Resize and other events just trigger a redraw on the next iteration.
        return None;
    };

    // Quit from anywhere.
    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Some(LoopOutcome::Quit);
    }

    match screen {
        Screen::WaitingForHeaderChoice => {
            if key.code == KeyCode::Char('h') {
                *screen = Screen::ShowingDataSummary { with_headers: true };
            } else if key.code == KeyCode::Char('d') {
                *screen = Screen::ShowingDataSummary {
                    with_headers: false,
                };
            }
        }
        Screen::ShowingDataSummary { with_headers } => {
            if key.code == KeyCode::Enter {
                return Some(LoopOutcome::Continue {
                    with_headers: *with_headers,
                });
            }
        }
    }

    None
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, dataset: &DataSet, screen: Screen, col_widths: &[u16]) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(frame.area());

    draw_table(frame, chunks[0], dataset, screen, col_widths);
    draw_prompt(frame, chunks[1], dataset, screen);
}

fn draw_table(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    dataset: &DataSet,
    screen: Screen,
    col_widths: &[u16],
) {
    let title = table_title(dataset, screen);
    let (header_row, data_start) = build_header_row(dataset, screen);
    let data_rows = build_data_rows(dataset, screen, data_start);
    let constraints = build_constraints(dataset.row_count(), col_widths);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let table = Table::new(data_rows, constraints)
        .header(header_row)
        .block(block);

    frame.render_widget(table, area);
}

fn table_title(dataset: &DataSet, screen: Screen) -> String {
    let cols = dataset.column_count();
    match screen {
        Screen::WaitingForHeaderChoice => " Data Preview ".to_string(),
        Screen::ShowingDataSummary { with_headers: true } => {
            let rows = dataset.row_count().saturating_sub(1);
            format!(" Data Preview — {rows} rows × {cols} columns ")
        }
        Screen::ShowingDataSummary {
            with_headers: false,
        } => {
            format!(
                " Data Preview — {} rows × {cols} columns ",
                dataset.row_count()
            )
        }
    }
}

fn build_constraints(row_count: usize, col_widths: &[u16]) -> Vec<Constraint> {
    // Row-number column: wide enough for the largest row number + 1 padding.
    let digits = u16::try_from(row_count.to_string().len()).unwrap_or(4);
    let num_width = digits.max(2);

    let mut out = vec![Constraint::Length(num_width)];
    out.extend(col_widths.iter().map(|&w| Constraint::Length(w)));
    out
}

/// Returns the header `Row` and the index of the first data row in `dataset.rows()`.
fn build_header_row(dataset: &DataSet, screen: Screen) -> (Row<'static>, usize) {
    let col_count = dataset.column_count();

    match screen {
        Screen::ShowingDataSummary { with_headers: true } => {
            // Use actual row-0 values as column names; data starts at row 1.
            let name_cells: Vec<Cell<'static>> = dataset
                .row(0)
                .unwrap_or_default()
                .iter()
                .map(|s| Cell::new(s.clone()).bold().yellow())
                .collect();

            let mut cells = vec![Cell::new("").dark_gray()];
            cells.extend(name_cells);
            (Row::new(cells).height(1), 1)
        }
        Screen::WaitingForHeaderChoice
        | Screen::ShowingDataSummary {
            with_headers: false,
        } => {
            // Auto-generated "Column N" names; all rows are data.
            let name_cells: Vec<Cell<'static>> = (1..=col_count)
                .map(|i| Cell::new(format!("Column {i}")).dark_gray())
                .collect();

            let mut cells = vec![Cell::new("#").dark_gray()];
            cells.extend(name_cells);
            (Row::new(cells).height(1), 0)
        }
    }
}

fn build_data_rows(dataset: &DataSet, screen: Screen, data_start: usize) -> Vec<Row<'static>> {
    dataset
        .rows()
        .iter()
        .enumerate()
        .skip(data_start)
        .take(MAX_PREVIEW_ROWS)
        .map(|(idx, row_data)| {
            let display_num = idx + 1 - data_start;
            let num_cell = Cell::new(display_num.to_string()).dark_gray();

            let data_cells: Vec<Cell<'static>> = row_data
                .iter()
                .map(|val| Cell::new(truncate(val)))
                .collect();

            let mut cells = vec![num_cell];
            cells.extend(data_cells);

            // Highlight row 1 when asking the headers question.
            let row = Row::new(cells).height(1);
            if idx == 0 && screen == Screen::WaitingForHeaderChoice {
                row.cyan().bold()
            } else {
                row
            }
        })
        .collect()
}

fn draw_prompt(frame: &mut Frame, area: ratatui::layout::Rect, dataset: &DataSet, screen: Screen) {
    let lines = prompt_lines(dataset, screen);
    frame.render_widget(Paragraph::new(lines), area);
}

fn prompt_lines<'a>(dataset: &DataSet, screen: Screen) -> Vec<Line<'a>> {
    match screen {
        Screen::WaitingForHeaderChoice => vec![
            Line::from("Is row 1 headers or data?"),
            Line::from(vec![
                Span::styled("[h]", Style::default().fg(Color::Green)),
                Span::raw(" Headers   "),
                Span::styled("[d]", Style::default().fg(Color::Green)),
                Span::raw(" Data   "),
                Span::styled("[q]", Style::default().fg(Color::Red)),
                Span::raw(" Quit"),
            ]),
        ],
        Screen::ShowingDataSummary { with_headers: true } => {
            let col_names = dataset.row(0).unwrap_or_default().to_vec().join(", ");
            vec![
                Line::from(format!("Headers: {col_names}")),
                Line::from(vec![
                    Span::styled("[Enter]", Style::default().fg(Color::Green)),
                    Span::raw(" Continue   "),
                    Span::styled("[q]", Style::default().fg(Color::Red)),
                    Span::raw(" Quit"),
                ]),
            ]
        }
        Screen::ShowingDataSummary {
            with_headers: false,
        } => vec![
            Line::from("All rows are data, no headers."),
            Line::from(vec![
                Span::styled("[Enter]", Style::default().fg(Color::Green)),
                Span::raw(" Continue   "),
                Span::styled("[q]", Style::default().fg(Color::Red)),
                Span::raw(" Quit"),
            ]),
        ],
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn compute_col_widths(dataset: &DataSet) -> Vec<u16> {
    let col_count = dataset.column_count();

    // Start with the width of auto-generated "Column N" labels so they
    // are never truncated in the initial header-choice screen.
    let mut widths: Vec<u16> = (1..=col_count)
        .map(|i| {
            let label_len = "Column ".len() + i.to_string().len();
            let capped = label_len.min(usize::from(MAX_COL_WIDTH));
            u16::try_from(capped).unwrap_or(MAX_COL_WIDTH).max(MIN_COL_WIDTH)
        })
        .collect();

    for row in dataset.rows().iter().take(MAX_PREVIEW_ROWS) {
        for (i, cell) in row.iter().enumerate().take(col_count) {
            let capped = cell.len().min(usize::from(MAX_COL_WIDTH));
            let w = u16::try_from(capped).unwrap_or(MAX_COL_WIDTH);
            if w > widths[i] {
                widths[i] = w;
            }
        }
    }

    widths
}

fn truncate(s: &str) -> String {
    let max = usize::from(MAX_COL_WIDTH);
    if s.len() > max {
        let mut t = s[..max.saturating_sub(1)].to_string();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}
