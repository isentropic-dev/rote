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
    playback::{ErrorAction, PlaybackControl, PlaybackEngine, PlaybackEvent},
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
    let mut state = PlaybackScreenState::new(
        total_rows,
        total_steps,
        step_summaries,
        start_row,
        PlaybackSpeed::Auto,
    );
    let mut user_quit = false;

    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        if state.finished {
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
            maybe_event = event_rx.recv() => {
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
                handle_playback_event(event, &mut state);
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
    /// Engine is paused at a confirmation gate (Cell or Row speed).
    waiting_for_confirmation: bool,
    /// Engine hit an error and is waiting for the user to choose an action.
    error_prompt: Option<ErrorPrompt>,
}

/// State for the interactive error prompt.
struct ErrorPrompt {
    row_index: usize,
    step_index: usize,
    error: String,
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
            waiting_for_confirmation: false,
            error_prompt: None,
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

    // Ctrl-C always quits.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        if !state.finished {
            let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
        }
        return true;
    }

    // If finished, only 'q' exits.
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
                if let Some(prompt) = state.error_prompt.take() {
                    state.error = Some(prompt.error);
                }
                let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
                return true;
            }
            _ => {}
        }
        return false;
    }

    // ── Normal playback mode ──────────────────────────────────────────

    // Quit / stop.
    if key.code == KeyCode::Char('q') {
        let _ = control_tx.send(PlaybackControl::ErrorResponse(ErrorAction::Stop));
        return true;
    }

    // Space: pause ↔ resume.
    if key.code == KeyCode::Char(' ') {
        if state.speed == PlaybackSpeed::Manual {
            state.speed = PlaybackSpeed::Auto;
            let _ = control_tx.send(PlaybackControl::SetSpeed(PlaybackSpeed::Auto));
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
        // If we switched away from Manual while at a gate, auto-proceed.
        if speed != PlaybackSpeed::Manual && state.waiting_for_confirmation {
            let _ = control_tx.send(PlaybackControl::Proceed);
            state.waiting_for_confirmation = false;
        }
    }

    false
}

fn handle_playback_event(event: PlaybackEvent, state: &mut PlaybackScreenState) {
    match event {
        PlaybackEvent::RowStarted { row_index } => {
            state.current_row = row_index;
            state.current_step = 0;
            let display_row = row_index + 1;
            state.status = format!("Playing row {display_row} of {}...", state.total_rows,);
        }
        PlaybackEvent::StepStarted { step_index, .. } => {
            state.current_step = step_index;
        }
        PlaybackEvent::StepCompleted { .. } => {}
        PlaybackEvent::WaitingForConfirmation => {
            state.waiting_for_confirmation = true;
            state.status = match state.speed {
                PlaybackSpeed::Manual => "Paused. [Enter] step forward, [Space] resume.".to_owned(),
                PlaybackSpeed::Cell => "Waiting for confirmation. [Enter] next field.".to_owned(),
                PlaybackSpeed::Row => "Row complete. [Enter] next row.".to_owned(),
                PlaybackSpeed::Auto => "Waiting...".to_owned(),
            };
        }
        PlaybackEvent::SpeedChanged(speed) => {
            state.speed = speed;
        }
        PlaybackEvent::RowCompleted { row_index } => {
            state.rows_completed += 1;
            state
                .row_log
                .push(format!("✓ Row {} completed", row_index + 1));
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
            state.status = format!("Error on row {}, step {}.", row_index + 1, step_index + 1,);
            state.error_prompt = Some(ErrorPrompt {
                row_index,
                step_index,
                error,
            });
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
    let has_error_prompt = state.error_prompt.is_some();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if has_error_prompt {
            vec![
                Constraint::Length(4),
                Constraint::Min(4),
                Constraint::Length(5),
                Constraint::Length(4),
            ]
        } else {
            vec![
                Constraint::Length(4),
                Constraint::Min(6),
                Constraint::Length(0),
                Constraint::Length(4),
            ]
        })
        .split(frame.area());

    draw_header(frame, chunks[0], state);
    draw_progress(frame, chunks[1], state);
    if has_error_prompt {
        draw_error_prompt(frame, chunks[2], state);
    }
    draw_status(frame, chunks[3], state);
}

fn draw_header(frame: &mut Frame, area: ratatui::layout::Rect, state: &PlaybackScreenState) {
    let display_row = state.current_row + 1;
    let title = format!(" Playback — Row {display_row} of {} ", state.total_rows,);

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

fn draw_error_prompt(frame: &mut Frame, area: ratatui::layout::Rect, state: &PlaybackScreenState) {
    let Some(prompt) = &state.error_prompt else {
        return;
    };

    let lines = vec![
        Line::from(format!(
            "Row {}, step {}: {}",
            prompt.row_index + 1,
            prompt.step_index + 1,
            prompt.error,
        ))
        .style(Style::default().fg(Color::Red)),
        Line::from(""),
        Line::from(vec![
            Span::styled("[s]", Style::default().fg(Color::Yellow)),
            Span::raw(" Skip this row   "),
            Span::styled("[r]", Style::default().fg(Color::Yellow)),
            Span::raw(" Retry from step 1   "),
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop playback"),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" Error ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn draw_status(frame: &mut Frame, area: ratatui::layout::Rect, state: &PlaybackScreenState) {
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
            Span::raw(format!("Speed: {} ", speed_label(state.speed))),
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
    } else if state.speed == PlaybackSpeed::Manual {
        let mut spans = vec![
            Span::raw(format!("Speed: {} ", speed_label(state.speed))),
            Span::styled("[Space]", Style::default().fg(Color::Green)),
            Span::raw(" Resume   "),
        ];
        spans.extend(speed_key_hints());
        spans.extend([
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]);
        spans
    } else {
        let mut spans = vec![
            Span::raw(format!("Speed: {} ", speed_label(state.speed))),
            Span::styled("[Space]", Style::default().fg(Color::Green)),
            Span::raw(" Pause   "),
        ];
        spans.extend(speed_key_hints());
        spans.extend([
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Stop"),
        ]);
        spans
    };

    let lines = vec![Line::from(state.status.as_str()), Line::from(hint)];

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(paragraph, area);
}

/// Hint spans for the 1/2/3/4 speed keys.
fn speed_key_hints() -> Vec<Span<'static>> {
    vec![
        Span::styled("[1-4]", Style::default().fg(Color::Cyan)),
        Span::raw(" Speed   "),
    ]
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
