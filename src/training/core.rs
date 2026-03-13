// Training core state machine implementation.

use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::data::DataSet;
use crate::workflow::{EmptyCellRule, PlaybackSpeed, Step, ValueSource};

use super::{Command, SelectorInfo, TrainingEvent};

/// The training session state machine.
///
/// Processes browser events and user commands to build a workflow template.
/// Emits `TrainingEvent`s for the TUI layer to render.
pub struct TrainingCore {
    data: DataSet,
    current_row: usize,
    steps: Vec<Step>,
    /// Tracks the `SelectorInfo` that produced each step, for same-element matching.
    step_selectors: Vec<Option<SelectorInfo>>,
    /// `column_bindings[col]` = the step index that column is bound to.
    column_bindings: Vec<Option<usize>>,
    empty_cell_rules: HashMap<usize, EmptyCellRule>,
    speed: PlaybackSpeed,
    event_tx: mpsc::UnboundedSender<TrainingEvent>,
}

impl TrainingCore {
    /// Create a new training session with the given data set.
    pub fn new(data: DataSet, event_tx: mpsc::UnboundedSender<TrainingEvent>) -> Self {
        let col_count = data.column_count();
        Self {
            data,
            current_row: 0,
            steps: Vec::new(),
            step_selectors: Vec::new(),
            column_bindings: vec![None; col_count],
            empty_cell_rules: HashMap::new(),
            speed: PlaybackSpeed::default(),
            event_tx,
        }
    }

    /// Process a command and emit any resulting events.
    pub fn process(&mut self, command: Command) {
        match command {
            Command::BrowserClick { selector_info, tag } => {
                self.handle_click(selector_info, tag);
            }
            Command::BrowserInput {
                selector_info,
                tag,
                value,
            } => {
                self.handle_input(selector_info, tag, &value);
            }
            Command::BrowserNavigation { url } => {
                self.handle_navigation(url);
            }
            Command::AdvanceRow => {
                self.handle_advance_row();
            }
            Command::SetSpeed(speed) => {
                self.speed = speed;
                self.emit(TrainingEvent::SpeedChanged(speed));
            }
            Command::HandleEmptyCell { column, rule } => {
                self.empty_cell_rules.insert(column, rule);
            }
            Command::HandleNewField { column } => {
                self.handle_new_field(column);
            }
        }
    }

    // -- Accessors --

    /// Index of the current data row.
    pub fn current_row_index(&self) -> usize {
        self.current_row
    }

    /// Data values for the current row.
    pub fn current_row_data(&self) -> Option<&[String]> {
        self.data.row(self.current_row)
    }

    /// The recorded steps so far.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Current playback speed.
    pub fn speed(&self) -> PlaybackSpeed {
        self.speed
    }

    /// Column binding state: `bound_columns()[col]` is `Some(step_index)` if bound.
    pub fn bound_columns(&self) -> &[Option<usize>] {
        &self.column_bindings
    }

    /// Whether every non-empty column in the current row is bound to a step.
    pub fn is_row_complete(&self) -> bool {
        let Some(row) = self.data.row(self.current_row) else {
            return false;
        };
        for (col, cell) in row.iter().enumerate() {
            if !cell.is_empty() && self.column_bindings.get(col).copied().flatten().is_none() {
                return false;
            }
        }
        true
    }

    // -- Private handlers --

    fn handle_click(&mut self, selector_info: SelectorInfo, tag: String) {
        let selector = selector_info.clone().into_selector(tag);
        let step = Step::Click { selector };
        let index = self.steps.len();
        self.steps.push(step.clone());
        self.step_selectors.push(Some(selector_info));
        self.emit(TrainingEvent::StepRecorded { index, step });
    }

    fn handle_input(&mut self, selector_info: SelectorInfo, tag: String, value: &str) {
        // Check if this element already has a step (incremental typing).
        let existing_index = self.find_step_for_element(&selector_info);

        // Try to match value to an unbound column.
        let (source, matched_column) = self.match_column(value, existing_index);

        let selector = selector_info.clone().into_selector(tag);
        let step = Step::Type {
            selector,
            source: source.clone(),
        };

        if let Some(idx) = existing_index {
            // Update existing step.
            // If the old step was column-bound, unbind it first.
            self.unbind_step(idx);

            self.steps[idx] = step.clone();
            self.step_selectors[idx] = Some(selector_info);

            if let Some(col) = matched_column {
                self.column_bindings[col] = Some(idx);
                self.emit(TrainingEvent::ColumnBound {
                    column: col,
                    step_index: idx,
                });
            }

            self.emit(TrainingEvent::StepUpdated { index: idx, step });
        } else {
            // New step.
            let index = self.steps.len();
            self.steps.push(step.clone());
            self.step_selectors.push(Some(selector_info));

            if let Some(col) = matched_column {
                self.column_bindings[col] = Some(index);
                self.emit(TrainingEvent::ColumnBound {
                    column: col,
                    step_index: index,
                });
            }

            self.emit(TrainingEvent::StepRecorded { index, step });
        }

        // Check if row is now complete.
        if self.is_row_complete() {
            self.emit(TrainingEvent::RowComplete {
                row_index: self.current_row,
            });
        }
    }

    fn handle_navigation(&mut self, url: String) {
        let step = Step::Navigate { url };
        let index = self.steps.len();
        self.steps.push(step.clone());
        self.step_selectors.push(None);
        self.emit(TrainingEvent::StepRecorded { index, step });
    }

    fn handle_advance_row(&mut self) {
        if self.current_row + 1 >= self.data.row_count() {
            self.emit(TrainingEvent::Error(
                "Already on the last row".to_owned(),
            ));
            return;
        }

        self.current_row += 1;

        // Check for empty cells and new fields in the new row.
        if let Some(row) = self.data.row(self.current_row) {
            let row = row.to_vec(); // Clone to avoid borrow issues.
            for (col, cell) in row.iter().enumerate() {
                if let Some(Some(_step_idx)) = self.column_bindings.get(col) {
                    // Column is bound — check if the new cell is empty.
                    if cell.is_empty() && !self.empty_cell_rules.contains_key(&col) {
                        self.emit(TrainingEvent::EmptyCellEncountered {
                            column: col,
                            row_index: self.current_row,
                        });
                    }
                } else if !cell.is_empty() {
                    // Column is unbound but has a value — new field encountered.
                    self.emit(TrainingEvent::NewFieldEncountered {
                        column: col,
                        value: cell.clone(),
                    });
                }
            }
        }
    }

    fn handle_new_field(&mut self, column: usize) {
        // Bind the column to the most recent Type step that isn't already bound.
        let last_type_step = self
            .steps
            .iter()
            .enumerate()
            .rev()
            .find(|(_, step)| matches!(step, Step::Type { .. }));

        if let Some((idx, _)) = last_type_step {
            self.column_bindings[column] = Some(idx);
            self.emit(TrainingEvent::ColumnBound {
                column,
                step_index: idx,
            });
        }
    }

    /// Find the index of an existing step that targets the same DOM element.
    fn find_step_for_element(&self, info: &SelectorInfo) -> Option<usize> {
        self.step_selectors
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, stored)| {
                stored
                    .as_ref()
                    .filter(|s| s.same_element(info))
                    .map(|_| i)
            })
    }

    /// Try to match a value to an unbound column in the current row.
    ///
    /// If `updating_step` is `Some`, columns currently bound to that step are
    /// considered "available" (since we're about to replace the step).
    fn match_column(
        &self,
        value: &str,
        updating_step: Option<usize>,
    ) -> (ValueSource, Option<usize>) {
        let Some(row) = self.data.row(self.current_row) else {
            return (ValueSource::Literal { value: value.to_owned() }, None);
        };

        for (col, cell) in row.iter().enumerate() {
            if cell == value {
                let binding = self.column_bindings.get(col).copied().flatten();
                let available = binding.is_none()
                    || binding == updating_step;
                if available {
                    return (ValueSource::Column { index: col }, Some(col));
                }
            }
        }

        (ValueSource::Literal { value: value.to_owned() }, None)
    }

    /// Remove any column binding that points to the given step index.
    fn unbind_step(&mut self, step_index: usize) {
        for binding in &mut self.column_bindings {
            if *binding == Some(step_index) {
                *binding = None;
            }
        }
    }

    fn emit(&self, event: TrainingEvent) {
        // Ignore send errors — the receiver may have been dropped.
        let _ = self.event_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data;

    /// Create a test data set from TSV text.
    fn test_data(tsv: &str) -> DataSet {
        data::from_delimited_str(tsv, data::Delimiter::Tab, true).unwrap()
    }

    /// Create a `SelectorInfo` with just an id and css.
    fn sel(id: &str, css: &str) -> SelectorInfo {
        SelectorInfo {
            id: Some(id.to_owned()),
            css: Some(css.to_owned()),
            xpath: Some(format!("//*[@id=\"{id}\"]")),
            text_content: None,
        }
    }

    /// Drain all available events from the receiver.
    fn drain_events(rx: &mut mpsc::UnboundedReceiver<TrainingEvent>) -> Vec<TrainingEvent> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    fn has_event<F>(events: &[TrainingEvent], pred: F) -> bool
    where
        F: Fn(&TrainingEvent) -> bool,
    {
        events.iter().any(pred)
    }

    // -- Tests --

    #[test]
    fn click_records_a_step() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserClick {
            selector_info: sel("submit-btn", "#submit-btn"),
            tag: "BUTTON".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            TrainingEvent::StepRecorded { index: 0, step: Step::Click { .. } }
        ));
        assert_eq!(core.steps().len(), 1);
    }

    #[test]
    fn input_binds_a_column() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound { column: 0, step_index: 0 }
        )));
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepRecorded { index: 0, .. }
        )));

        // Verify the step source is Column.
        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(*source, ValueSource::Column { index: 0 });
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn input_with_no_match_is_literal() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("other", "#other"),
            tag: "INPUT".to_owned(),
            value: "unmatched-value".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(!has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound { .. }
        )));

        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(
                    *source,
                    ValueSource::Literal {
                        value: "unmatched-value".to_owned()
                    }
                );
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn incremental_typing_updates_step() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type "Al" — no column match.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Al".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepRecorded { index: 0, .. }
        )));

        // Type "Ali" — still no match, but same element → update.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Ali".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepUpdated { index: 0, .. }
        )));

        // Only one step should exist.
        assert_eq!(core.steps().len(), 1);
    }

    #[test]
    fn incremental_typing_rebinds_column() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type "Al" — literal.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Al".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert!(core.bound_columns()[0].is_none());

        // Type "Alice" — matches column 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound { column: 0, step_index: 0 }
        )));

        assert_eq!(core.bound_columns()[0], Some(0));
        assert_eq!(core.steps().len(), 1);

        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(*source, ValueSource::Column { index: 0 });
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn row_completion() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert!(!core.is_row_complete());

        // Bind column 1.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::RowComplete { row_index: 0 }
        )));
        assert!(core.is_row_complete());
    }

    #[test]
    fn advance_row_increments() {
        let ds = test_data("name\nAlice\nBob\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        assert_eq!(core.current_row_index(), 0);
        core.process(Command::AdvanceRow);
        assert_eq!(core.current_row_index(), 1);

        let _ = drain_events(&mut rx);
    }

    #[test]
    fn advance_row_detects_empty_cell() {
        // Row 0: Alice, 30 — Row 1: Bob, (empty)
        let ds = test_data("name\tage\nAlice\t30\nBob\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind both columns on row 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Advance to row 1.
        core.process(Command::AdvanceRow);

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::EmptyCellEncountered { column: 1, row_index: 1 }
        )));
    }

    #[test]
    fn advance_row_detects_new_field() {
        // Row 0: Alice, (empty) — Row 1: Bob, 25
        // Only bind column 0 on row 0. Column 1 is unbound.
        let ds = test_data("name\tage\nAlice\t\nBob\t25\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0 only.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Advance to row 1.
        core.process(Command::AdvanceRow);

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::NewFieldEncountered { column: 1, .. }
        )));
    }

    #[test]
    fn speed_change() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::SetSpeed(PlaybackSpeed::Auto));

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::SpeedChanged(PlaybackSpeed::Auto)
        )));
        assert_eq!(core.speed(), PlaybackSpeed::Auto);
    }

    #[test]
    fn navigation_records_a_step() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserNavigation {
            url: "https://example.com/form".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepRecorded {
                index: 0,
                step: Step::Navigate { .. }
            }
        )));
    }

    #[test]
    fn row_complete_with_empty_cells_not_required() {
        // Row has an empty cell — that column doesn't need binding for completion.
        let ds = test_data("name\tage\nAlice\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind only column 0 (column 1 is empty).
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::RowComplete { row_index: 0 }
        )));
        assert!(core.is_row_complete());
    }

    #[test]
    fn advance_past_last_row_emits_error() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::AdvanceRow);

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::Error(_)
        )));
        // Row should not have changed.
        assert_eq!(core.current_row_index(), 0);
    }
}
