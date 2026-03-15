// Playback screen: renders a live data table driven by `PlaybackEngine` events.
//
// Layout: main area (table) + 5-line bottom status bar.
// Starts at `Cell` speed — the engine pauses after each `Type` step and waits
// for the user to press Enter before continuing.

use std::io;
use std::pin::pin;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use tokio::sync::mpsc;

use crate::{
    cdp::Browser,
    data::DataSet,
    playback::{ErrorAction, PlaybackControl, PlaybackEngine, PlaybackEvent},
    workflow::{PlaybackSpeed, Workflow},
};

use super::table::{self, CellState, RowState, TableState};

// ── Public types ──────────────────────────────────────────────────────────────

/// Result of the playback screen.
#[allow(dead_code)] // Variant fields available to callers; return value currently discarded.
pub enum PlaybackOutcome {
    /// Playback completed (possibly with some rows skipped).
    Done {
        rows_completed: usize,
        rows_skipped: usize,
    },
    /// Playback stopped due to an error.
    Error(String),
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Run the playback screen.
///
/// Playback starts at `Cell` speed: the engine pauses after each `Type` step
/// and waits for the user to press Enter before continuing.
///
/// Rows `0..start_row` are shown as already `Done` (trained).
/// The browser is borrowed, not consumed — the caller keeps it alive
/// so the browser window doesn't close when playback finishes.
///
/// # Errors
///
/// Returns an error if drawing or event reading fails.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    workflow: Workflow,
    dataset: DataSet,
    browser: &Browser,
    start_row: usize,
) -> io::Result<PlaybackOutcome> {
    let total_rows = dataset.row_count();
    let col_count = dataset.column_count();

    // Build reverse lookup: step_index → column (if any).
    // `column_bindings[col] = Some(step_idx)` means that step fills that column.
    // Invert it so we can flip a cell Done when StepCompleted fires.
    let step_to_column: Vec<Option<usize>> = (0..workflow.steps.len())
        .map(|step_idx| {
            workflow
                .column_bindings
                .iter()
                .position(|b| *b == Some(step_idx))
        })
        .collect();

    let (mut engine, control_tx, mut event_rx) =
        PlaybackEngine::new(workflow, dataset.clone(), PlaybackSpeed::Cell, start_row);

    // Drive the engine as a pinned future alongside the TUI event loop.
    // We do NOT tokio::spawn — that would move the browser into a task and
    // drop it (killing the browser process) when the task completes.
    let mut engine_future = pin!(engine.run(browser));
    let mut engine_done = false;

    let mut terminal_events = EventStream::new();

    // Initialize table state: rows before start_row are already Done.
    let mut table_state = TableState::new(total_rows, col_count, &dataset);
    for row in 0..start_row {
        for col in 0..col_count {
            table_state.set_cell_state(row, col, CellState::Done);
        }
        table_state.set_row_state(row, RowState::Done);
    }

    let mut screen = ScreenState::new(total_rows, start_row, PlaybackSpeed::Cell);
    let mut user_quit = false;

    loop {
        terminal.draw(|frame| {
            let table_area_height = frame.area().height.saturating_sub(5);
            table_state.update_viewport(table_area_height);
            draw(frame, &table_state, &dataset, &screen);
        })?;

        if screen.finished {
            break;
        }

        tokio::select! {
            maybe_terminal_event = terminal_events.next() => {
                let Some(event_result) = maybe_terminal_event else {
                    user_quit = true;
                    break;
                };

                if handle_key_event(&event_result?, &mut screen, &control_tx) {
                    user_quit = true;
                    break;
                }
            }
            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    // Engine dropped the sender without sending Finished.
                    engine_done = true;
                    if !screen.finished {
                        screen.finished = true;
                        screen.status = format!(
                            "Stopped. {} completed, {} skipped. Press [q] to quit.",
                            screen.rows_completed, screen.rows_skipped,
                        );
                    }
                    continue;
                };

                handle_playback_event(event, &mut screen, &mut table_state, &step_to_column);
                if screen.finished {
                    engine_done = true;
                }
            }
            _engine_result = &mut engine_future, if !engine_done => {
                engine_done = true;
            }
        }
    }

    // Render final state and wait for 'q' (unless the user already quit).
    if !user_quit {
        terminal.draw(|frame| {
            let table_area_height = frame.area().height.saturating_sub(5);
            table_state.update_viewport(table_area_height);
            draw(frame, &table_state, &dataset, &screen);
        })?;
        wait_for_quit(terminal, &mut table_state, &dataset, &screen, &mut terminal_events)
            .await?;
    }

    Ok(if let Some(ref error) = screen.error {
        PlaybackOutcome::Error(error.clone())
    } else {
        PlaybackOutcome::Done {
            rows_completed: screen.rows_completed,
            rows_skipped: screen.rows_skipped,
        }
    })
}

// ── Screen state ──────────────────────────────────────────────────────────────

struct ScreenState {
    total_rows: usize,
    current_row: usize,
    rows_completed: usize,
    rows_skipped: usize,
    speed: PlaybackSpeed,
    status: String,
    finished: bool,
    error: Option<String>,
    /// Engine is paused at a confirmation gate (Cell or Row speed).
    waiting_for_confirmation: bool,
    /// Engine hit an error and is waiting for the user to choose an action.
    error_prompt: Option<String>,
}

impl ScreenState {
    fn new(total_rows: usize, start_row: usize, speed: PlaybackSpeed) -> Self {
        let playback_rows = total_rows.saturating_sub(start_row);
        Self {
            total_rows,
            current_row: start_row,
            rows_completed: 0,
            rows_skipped: 0,
            speed,
            status: format!("Playing {playback_rows} rows..."),
            finished: false,
            error: None,
            waiting_for_confirmation: false,
            error_prompt: None,
        }
    }
}

// ── Event handling ────────────────────────────────────────────────────────────

/// Returns `true` if the caller should quit.
fn handle_key_event(
    event: &Event,
    state: &mut ScreenState,
    control_tx: &mpsc::UnboundedSender<PlaybackControl>,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };

    // Ctrl-C always quits.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if !state.finished {
            let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
        }
        return true;
    }

    // When finished, only 'q' exits.
    if state.finished {
        return key.code == KeyCode::Char('q');
    }

    // ── Error prompt mode ─────────────────────────────────────────────
    if state.error_prompt.is_some() {
        match key.code {
            KeyCode::Char('s') => {
                state.error_prompt = None;
                let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::SkipRow));
            }
            KeyCode::Char('r') => {
                state.error_prompt = None;
                let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::RetryRow));
            }
            KeyCode::Char('q') => {
                if let Some(error) = state.error_prompt.take() {
                    state.error = Some(error);
                }
                let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
                return true;
            }
            _ => {}
        }
        return false;
    }

    // ── Normal playback mode ──────────────────────────────────────────

    // Quit.
    if key.code == KeyCode::Char('q') {
        let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
        return true;
    }

    // Space: pause ↔ resume.
    // Pause drops to Manual; resume goes back to Cell (default for this session).
    if key.code == KeyCode::Char(' ') {
        if state.speed == PlaybackSpeed::Manual {
            state.speed = PlaybackSpeed::Cell;
            let _ = control_tx.send(PlaybackControl::SetSpeed(PlaybackSpeed::Cell));
            let _ = control_tx.send(PlaybackControl::Proceed);
            state.waiting_for_confirmation = false;
        } else {
            let _ = control_tx.send(PlaybackControl::Pause);
            state.speed = PlaybackSpeed::Manual;
        }
        return false;
    }

    // Enter: proceed past a confirmation gate.
    if key.code == KeyCode::Enter && state.waiting_for_confirmation {
        let _ = control_tx.send(PlaybackControl::Proceed);
        state.waiting_for_confirmation = false;
        return false;
    }

    // Direct speed selection: 1/2/3/4 → Manual/Cell/Row/Auto.
    let new_speed = match key.code {
        KeyCode::Char('1') => Some(PlaybackSpeed::Manual),
        KeyCode::Char('2') => Some(PlaybackSpeed::Cell),
        KeyCode::Char('3') => Some(PlaybackSpeed::Row),
        KeyCode::Char('4') => Some(PlaybackSpeed::Auto),
        _ => None,
    };
    if let Some(speed) = new_speed {
        state.speed = speed;
        let _ = control_tx.send(PlaybackControl::SetSpeed(speed));
        // If switching away from Manual while at a gate, auto-proceed.
        if speed != PlaybackSpeed::Manual && state.waiting_for_confirmation {
            let _ = control_tx.send(PlaybackControl::Proceed);
            state.waiting_for_confirmation = false;
        }
    }

    false
}

fn handle_playback_event(
    event: PlaybackEvent,
    state: &mut ScreenState,
    table_state: &mut TableState,
    step_to_column: &[Option<usize>],
) {
    match event {
        PlaybackEvent::RowStarted { row_index } => {
            state.current_row = row_index;
            table_state.set_row_state(row_index, RowState::InProgress);
            let display = row_index + 1;
            state.status = format!("Playing row {display} of {}...", state.total_rows);
        }
        PlaybackEvent::StepStarted { .. } => {}
        PlaybackEvent::StepCompleted {
            row_index,
            step_index,
        } => {
            // Flip the cell for the column this step fills (if any).
            // Clicks and navigations don't have column bindings; only Type steps do.
            if let Some(col) = step_to_column.get(step_index).copied().flatten() {
                table_state.set_cell_state(row_index, col, CellState::Done);
            }
        }
        PlaybackEvent::WaitingForConfirmation => {
            state.waiting_for_confirmation = true;
            state.status = match state.speed {
                PlaybackSpeed::Manual => "Paused. [Enter] next step, [Space] resume.".to_owned(),
                PlaybackSpeed::Cell => "Field complete. [Enter] next field.".to_owned(),
                PlaybackSpeed::Row => "Row complete. [Enter] next row.".to_owned(),
                PlaybackSpeed::Auto => "Waiting...".to_owned(),
            };
        }
        PlaybackEvent::SpeedChanged(speed) => {
            state.speed = speed;
        }
        PlaybackEvent::RowCompleted { row_index } => {
            state.rows_completed += 1;
            table_state.set_row_state(row_index, RowState::Done);
            state.waiting_for_confirmation = false;
        }
        PlaybackEvent::StepFailed {
            row_index,
            step_index,
            error,
        } => {
            state.status = format!(
                "Row {}, step {}: {error}",
                row_index + 1,
                step_index + 1,
            );
            state.error_prompt = Some(error);
        }
        PlaybackEvent::Finished {
            rows_completed,
            rows_skipped,
        } => {
            state.rows_completed = rows_completed;
            state.rows_skipped = rows_skipped;
            state.finished = true;
            if state.error.is_some() {
                state.status = format!(
                    "Stopped. {rows_completed} completed, {rows_skipped} skipped. Press [q] to quit.",
                );
            } else {
                state.status = format!(
                    "Done! {rows_completed} rows completed, {rows_skipped} skipped. Press [q] to quit.",
                );
            }
        }
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, table_state: &TableState, dataset: &DataSet, state: &ScreenState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(5)])
        .split(frame.area());

    table::draw_table(frame, chunks[0], dataset, table_state);
    draw_status_bar(frame, chunks[1], state);
}

fn draw_status_bar(frame: &mut Frame, area: Rect, state: &ScreenState) {
    let display_row = state.current_row + 1;
    let header = format!(
        "Playback — Row {} of {}   Speed: {}",
        display_row,
        state.total_rows,
        speed_label(state.speed),
    );

    let hint: Vec<Span<'_>> = if state.finished {
        vec![
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Quit"),
        ]
    } else if state.error_prompt.is_some() {
        vec![
            Span::styled("[s]", Style::default().fg(Color::Yellow)),
            Span::raw(" Skip row   "),
            Span::styled("[r]", Style::default().fg(Color::Yellow)),
            Span::raw(" Retry row   "),
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]
    } else if state.waiting_for_confirmation {
        let mut spans = vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" Proceed   "),
        ];
        if state.speed == PlaybackSpeed::Manual {
            spans.extend([
                Span::styled("[Space]", Style::default().fg(Color::Green)),
                Span::raw(" Resume   "),
            ]);
        }
        spans.extend(speed_key_hints());
        spans.extend([
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]);
        spans
    } else {
        let pause_resume = if state.speed == PlaybackSpeed::Manual {
            " Resume   "
        } else {
            " Pause   "
        };
        let mut spans = vec![
            Span::styled("[Space]", Style::default().fg(Color::Green)),
            Span::raw(pause_resume),
        ];
        spans.extend(speed_key_hints());
        spans.extend([
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]);
        spans
    };

    let lines = vec![
        Line::from(header.as_str()),
        Line::from(state.status.as_str()),
        Line::from(hint),
    ];

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(paragraph, area);
}

async fn wait_for_quit(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    table_state: &mut TableState,
    dataset: &DataSet,
    state: &ScreenState,
    terminal_events: &mut EventStream,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let table_area_height = frame.area().height.saturating_sub(5);
            table_state.update_viewport(table_area_height);
            draw(frame, table_state, dataset, state);
        })?;

        let Some(event_result) = terminal_events.next().await else {
            return Ok(());
        };
        let event = event_result?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.code == KeyCode::Char('q')
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Ok(());
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Human-readable label for a playback speed.
fn speed_label(speed: PlaybackSpeed) -> &'static str {
    match speed {
        PlaybackSpeed::Manual => "Manual",
        PlaybackSpeed::Cell => "Cell",
        PlaybackSpeed::Row => "Row",
        PlaybackSpeed::Auto => "Auto",
    }
}

/// Key-hint spans for the 1/2/3/4 speed keys.
fn speed_key_hints() -> Vec<Span<'static>> {
    vec![
        Span::styled("[1-4]", Style::default().fg(Color::Cyan)),
        Span::raw(" Speed   "),
    ]
}
