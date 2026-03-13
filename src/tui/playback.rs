use std::io;
use std::pin::pin;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use tokio::sync::mpsc;

use crate::{
    cdp::Browser,
    data::DataSet,
    playback::{
        ErrorAction, PlaybackControl, PlaybackEngine, PlaybackEvent,
    },
    workflow::{PlaybackSpeed, Step, Workflow},
};

/// Result of the playback screen.
#[allow(dead_code)] // Fields read by callers in future milestones.
pub enum PlaybackOutcome {
    /// Playback completed (possibly with some rows skipped).
    Done {
        rows_completed: usize,
        rows_skipped: usize,
    },
    /// Playback stopped due to an error.
    Error(String),
}

/// Run the playback screen.
///
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
    let total_steps = workflow.steps.len();
    let step_summaries: Vec<String> = workflow.steps.iter().map(step_summary).collect();

    let (mut engine, control_tx, mut event_rx) =
        PlaybackEngine::new(workflow, dataset, PlaybackSpeed::Auto, start_row);

    // Drive the engine as a pinned future alongside the TUI event loop.
    // We do NOT tokio::spawn — that would move the browser into the task
    // and drop it (killing the browser process) when the task completes.
    let mut engine_future = pin!(engine.run(browser));
    let mut engine_done = false;

    let mut terminal_events = EventStream::new();
    let mut state = PlaybackScreenState::new(total_rows, total_steps, step_summaries, start_row, PlaybackSpeed::Auto);
    let mut user_quit = false;

    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        if state.finished || engine_done {
            break;
        }

        tokio::select! {
            maybe_terminal_event = terminal_events.next() => {
                let Some(event_result) = maybe_terminal_event else {
                    user_quit = true;
                    break;
                };

                if handle_key_event(&event_result?, &mut state, &control_tx) {
                    user_quit = true;
                    break;
                }
            }
            maybe_event = event_rx.recv(), if !engine_done => {
                let Some(event) = maybe_event else {
                    // Engine dropped the sender without sending Finished.
                    engine_done = true;
                    if !state.finished {
                        state.finished = true;
                        state.status = if state.error.is_some() {
                            format!(
                                "Stopped. {} completed, {} skipped. Press [q] to quit.",
                                state.rows_completed, state.rows_skipped,
                            )
                        } else {
                            format!(
                                "Done! {} rows completed, {} skipped. Press [q] to quit.",
                                state.rows_completed, state.rows_skipped,
                            )
                        };
                    }
                    continue;
                };
                handle_playback_event(event, &mut state, &control_tx);
                if state.finished {
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
        terminal.draw(|frame| draw(frame, &state))?;
        wait_for_quit(terminal, &state, &mut terminal_events).await?;
    }

    Ok(if let Some(ref error) = state.error {
        PlaybackOutcome::Error(error.clone())
    } else {
        PlaybackOutcome::Done {
            rows_completed: state.rows_completed,
            rows_skipped: state.rows_skipped,
        }
    })
}

struct PlaybackScreenState {
    total_rows: usize,
    total_steps: usize,
    step_summaries: Vec<String>,
    current_row: usize,
    current_step: usize,
    rows_completed: usize,
    rows_skipped: usize,
    row_log: Vec<String>,
    speed: PlaybackSpeed,
    status: String,
    finished: bool,
    error: Option<String>,
}

impl PlaybackScreenState {
    fn new(
        total_rows: usize,
        total_steps: usize,
        step_summaries: Vec<String>,
        start_row: usize,
        speed: PlaybackSpeed,
    ) -> Self {
        let playback_rows = total_rows.saturating_sub(start_row);
        Self {
            total_rows,
            total_steps,
            step_summaries,
            current_row: start_row,
            current_step: 0,
            rows_completed: 0,
            rows_skipped: 0,
            row_log: Vec::new(),
            speed,
            status: format!("Playing {playback_rows} rows..."),
            finished: false,
            error: None,
        }
    }
}

fn handle_key_event(
    event: &Event,
    state: &mut PlaybackScreenState,
    control_tx: &mpsc::UnboundedSender<PlaybackControl>,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };

    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        if state.finished {
            return true;
        }
        let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
        return true;
    }

    // Pause / resume: space bar toggles between Auto and Manual.
    if key.code == KeyCode::Char(' ') && !state.finished {
        if state.speed == PlaybackSpeed::Manual {
            state.speed = PlaybackSpeed::Auto;
            let _ = control_tx.send(PlaybackControl::SetSpeed(PlaybackSpeed::Auto));
            let _ = control_tx.send(PlaybackControl::Proceed);
        } else {
            let _ = control_tx.send(PlaybackControl::Pause);
            state.speed = PlaybackSpeed::Manual;
        }
    }

    // Step forward when paused.
    if key.code == KeyCode::Enter && state.speed == PlaybackSpeed::Manual && !state.finished {
        let _ = control_tx.send(PlaybackControl::Proceed);
    }

    false
}

fn handle_playback_event(
    event: PlaybackEvent,
    state: &mut PlaybackScreenState,
    control_tx: &mpsc::UnboundedSender<PlaybackControl>,
) {
    match event {
        PlaybackEvent::RowStarted { row_index } => {
            state.current_row = row_index;
            state.current_step = 0;
            let display_row = row_index + 1;
            state.status = format!(
                "Playing row {display_row} of {}...",
                state.total_rows,
            );
        }
        PlaybackEvent::StepStarted { step_index, .. } => {
            state.current_step = step_index;
        }
        PlaybackEvent::StepCompleted { .. }
        | PlaybackEvent::WaitingForConfirmation => {}
        PlaybackEvent::SpeedChanged(speed) => {
            state.speed = speed;
        }
        PlaybackEvent::RowCompleted { row_index } => {
            state.rows_completed += 1;
            state.row_log.push(format!("✓ Row {} completed", row_index + 1));
        }
        PlaybackEvent::StepFailed {
            row_index,
            step_index,
            error,
        } => {
            let msg = format!(
                "✗ Row {} failed at step {}: {error}",
                row_index + 1,
                step_index + 1,
            );
            state.row_log.push(msg);
            state.error = Some(error);
            state.status = format!("Error on row {}. Stopping.", row_index + 1);
            // Stop playback on error.
            let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
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

async fn wait_for_quit(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &PlaybackScreenState,
    terminal_events: &mut EventStream,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, state))?;

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

// ─── Drawing ──────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, state: &PlaybackScreenState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(6),
            Constraint::Length(4),
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], state);
    draw_progress(frame, chunks[1], state);
    draw_status(frame, chunks[2], state);
}

fn draw_header(frame: &mut Frame, area: ratatui::layout::Rect, state: &PlaybackScreenState) {
    let display_row = state.current_row + 1;
    let title = format!(
        " Playback — Row {display_row} of {} ",
        state.total_rows,
    );

    let step_line = if state.finished {
        "Playback complete.".to_owned()
    } else if state.total_steps == 0 {
        "No steps in workflow.".to_owned()
    } else {
        let step_num = state.current_step + 1;
        let summary = state
            .step_summaries
            .get(state.current_step)
            .cloned()
            .unwrap_or_default();
        format!("Step {step_num}/{}: {summary}", state.total_steps)
    };

    let paragraph = Paragraph::new(vec![
        Line::from(format!("Playing row {display_row} of {}", state.total_rows)),
        Line::from(step_line),
    ])
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    )
    .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn draw_progress(frame: &mut Frame, area: ratatui::layout::Rect, state: &PlaybackScreenState) {
    let items: Vec<ListItem<'_>> = if state.row_log.is_empty() {
        vec![ListItem::new(Line::from("Waiting...").dark_gray())]
    } else {
        state
            .row_log
            .iter()
            .map(|line| {
                let style = if line.starts_with('✓') {
                    Style::default().fg(Color::Green)
                } else if line.starts_with('✗') {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(line.as_str()).style(style))
            })
            .collect()
    };

    // Show in-progress row if not finished.
    let mut items = items;
    if !state.finished && !state.row_log.is_empty() {
        items.push(ListItem::new(
            Line::from(format!("▶ Row {} in progress...", state.current_row + 1))
                .style(Style::default().fg(Color::Yellow)),
        ));
    }

    let list = List::new(items).block(
        Block::default()
            .title(" Progress ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );

    frame.render_widget(list, area);
}

fn speed_label(speed: PlaybackSpeed) -> &'static str {
    match speed {
        PlaybackSpeed::Manual => "Paused",
        PlaybackSpeed::Cell => "Cell",
        PlaybackSpeed::Row => "Row",
        PlaybackSpeed::Auto => "Auto",
    }
}

fn draw_status(frame: &mut Frame, area: ratatui::layout::Rect, state: &PlaybackScreenState) {
    let hint: Vec<Span<'_>> = if state.finished {
        vec![
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Quit"),
        ]
    } else if state.speed == PlaybackSpeed::Manual {
        vec![
            Span::raw(format!("Speed: {} ", speed_label(state.speed))),
            Span::styled("[Space]", Style::default().fg(Color::Green)),
            Span::raw(" Resume   "),
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" Step   "),
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]
    } else {
        vec![
            Span::raw(format!("Speed: {} ", speed_label(state.speed))),
            Span::styled("[Space]", Style::default().fg(Color::Green)),
            Span::raw(" Pause   "),
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]
    };

    let lines = vec![
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

// ─── Helpers ──────────────────────────────────────────────────────────────

fn step_summary(step: &Step) -> String {
    match step {
        Step::Click { selector } => {
            format!("Click {}", selector.tag)
        }
        Step::Type { selector, source } => {
            let source_desc = match source {
                crate::workflow::ValueSource::Column { index } => {
                    format!("column {}", index + 1)
                }
                crate::workflow::ValueSource::Literal { value } => {
                    format!("literal \"{value}\"")
                }
            };
            format!("Type {source_desc} → {}", selector.tag)
        }
        Step::WaitForNavigation => "Wait for navigation".to_owned(),
    }
}
