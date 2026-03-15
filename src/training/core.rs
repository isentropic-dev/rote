// Training core state machine implementation.

use std::collections::{BTreeMap, HashSet};

use tokio::sync::mpsc;

use crate::data::DataSet;
use crate::data::DataSourceConfig;
use crate::workflow::{EmptyCellRule, PlaybackSpeed, Step, ValueSource, Workflow};

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
    empty_cell_rules: BTreeMap<usize, EmptyCellRule>,
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
            empty_cell_rules: BTreeMap::new(),
            speed: PlaybackSpeed::default(),
            event_tx,
        }
    }

    /// Process a command and emit any resulting events.
    ///
    /// This method is synchronous because training sessions are driven by a
    /// single-threaded event loop in the TUI. All browser events and user
    /// commands are serialised through this entry point, so there is no risk
    /// of concurrent mutation.
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
    #[cfg(test)]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Current playback speed.
    #[cfg(test)]
    pub fn speed(&self) -> PlaybackSpeed {
        self.speed
    }

    /// Column binding state: `bound_columns()[col]` is `Some(step_index)` if bound.
    pub fn bound_columns(&self) -> &[Option<usize>] {
        &self.column_bindings
    }

    /// The current per-column empty-cell rules.
    #[cfg(test)]
    pub fn empty_cell_rules(&self) -> &BTreeMap<usize, EmptyCellRule> {
        &self.empty_cell_rules
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

    /// Build a [`Workflow`] from the current training state.
    ///
    /// The optional `data_source` is embedded in the workflow so that playback
    /// can reload the data automatically.
    pub fn build_workflow(&self, data_source: Option<DataSourceConfig>) -> Workflow {
        Workflow::new(
            self.data.column_count(),
            self.steps.clone(),
            self.column_bindings.clone(),
            self.empty_cell_rules.clone(),
            data_source,
        )
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

    fn handle_navigation(&mut self, _url: String) {
        let step = Step::WaitForNavigation;
        let index = self.steps.len();
        self.steps.push(step.clone());
        self.step_selectors.push(None);
        self.emit(TrainingEvent::StepRecorded { index, step });
    }

    fn handle_advance_row(&mut self) {
        if self.current_row + 1 >= self.data.row_count() {
            self.emit(TrainingEvent::Error("Already on the last row".to_owned()));
            return;
        }

        self.current_row += 1;
        self.emit(TrainingEvent::RowAdvanced {
            row_index: self.current_row,
        });

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
        // Bounds-check first; panic would be wrong for user-driven input.
        if column >= self.column_bindings.len() {
            self.emit(TrainingEvent::Error(format!(
                "column index {column} out of range (have {})",
                self.column_bindings.len(),
            )));
            return;
        }

        // Collect already-bound step indices so we can skip them.
        let already_bound: HashSet<usize> =
            self.column_bindings.iter().filter_map(|b| *b).collect();

        // Find the most recent Type step that is not already bound to a column.
        // Clone the selector out so we can mutate self.steps afterwards.
        let found = self.steps.iter().enumerate().rev().find_map(|(i, step)| {
            if already_bound.contains(&i) {
                return None;
            }
            if let Step::Type { selector, .. } = step {
                Some((i, selector.clone()))
            } else {
                None
            }
        });

        if let Some((idx, selector)) = found {
            // Update the step's source to the new column.
            let updated = Step::Type {
                selector,
                source: ValueSource::Column { index: column },
            };
            self.steps[idx] = updated.clone();

            self.column_bindings[column] = Some(idx);
            self.emit(TrainingEvent::ColumnBound {
                column,
                step_index: idx,
            });
            self.emit(TrainingEvent::StepUpdated {
                index: idx,
                step: updated,
            });
        }
    }

    /// Find the index of an existing step that targets the same DOM element.
    fn find_step_for_element(&self, info: &SelectorInfo) -> Option<usize> {
        self.step_selectors
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, stored)| stored.as_ref().filter(|s| s.same_element(info)).map(|_| i))
    }

    /// Try to match a value to the next unbound column in the current row.
    ///
    /// Column-order training: only the leftmost unbound column is considered.
    /// If the value matches that column's cell, it becomes a column binding.
    /// Otherwise the value is treated as a literal.
    ///
    /// If `updating_step` is `Some`, columns currently bound to that step are
    /// considered "available" (since we're about to replace the step).
    fn match_column(
        &self,
        value: &str,
        updating_step: Option<usize>,
    ) -> (ValueSource, Option<usize>) {
        let Some(row) = self.data.row(self.current_row) else {
            return (
                ValueSource::Literal {
                    value: value.to_owned(),
                },
                None,
            );
        };

        // Find the leftmost unbound column (considering updating_step as available).
        let next_unbound = row.iter().enumerate().find(|(col, _cell)| {
            let binding = self.column_bindings.get(*col).copied().flatten();
            binding.is_none() || binding == updating_step
        });

        if let Some((col, cell)) = next_unbound
            && cell == value
        {
            return (ValueSource::Column { index: col }, Some(col));
        }

        (
            ValueSource::Literal {
                value: value.to_owned(),
            },
            None,
        )
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
            TrainingEvent::StepRecorded {
                index: 0,
                step: Step::Click { .. }
            }
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
            TrainingEvent::ColumnBound {
                column: 0,
                step_index: 0
            }
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
            TrainingEvent::ColumnBound {
                column: 0,
                step_index: 0
            }
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
            TrainingEvent::EmptyCellEncountered {
                column: 1,
                row_index: 1
            }
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
                step: Step::WaitForNavigation
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
        assert!(has_event(&events, |e| matches!(e, TrainingEvent::Error(_))));
        // Row should not have changed.
        assert_eq!(core.current_row_index(), 0);
    }

    // -- HandleNewField tests --

    #[test]
    fn handle_new_field_binds_column_and_updates_source() {
        // Column 1 is unbound; a Type step exists with a literal value.
        let ds = test_data("name\tage\nAlice\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Record a Type step whose value doesn't match any column.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "something-else".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Issue HandleNewField to bind column 1 to that step.
        core.process(Command::HandleNewField { column: 1 });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(
                e,
                TrainingEvent::ColumnBound {
                    column: 1,
                    step_index: 0
                }
            )),
            "expected ColumnBound event",
        );

        // The step's source should now be Column { index: 1 }.
        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(*source, ValueSource::Column { index: 1 });
            }
            _ => panic!("expected Type step"),
        }

        // The binding should be recorded.
        assert_eq!(core.bound_columns()[1], Some(0));
    }

    #[test]
    fn handle_new_field_skips_already_bound_steps() {
        // Two Type steps; first is bound to column 0, second is unbound.
        let ds = test_data("first\tsecond\nAlice\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0 to step 0.
        core.process(Command::BrowserInput {
            selector_info: sel("first-field", "#first-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        // Record a second Type step (not bound).
        core.process(Command::BrowserInput {
            selector_info: sel("second-field", "#second-field"),
            tag: "INPUT".to_owned(),
            value: "unmatched".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert_eq!(core.bound_columns()[0], Some(0)); // step 0 bound to col 0
        assert_eq!(core.bound_columns()[1], None); // col 1 unbound

        // HandleNewField for column 1 should bind to step 1 (not step 0).
        core.process(Command::HandleNewField { column: 1 });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(
                e,
                TrainingEvent::ColumnBound {
                    column: 1,
                    step_index: 1
                }
            )),
            "expected binding to step 1, not step 0",
        );
        assert_eq!(core.bound_columns()[1], Some(1));
    }

    #[test]
    fn handle_new_field_out_of_range_emits_error() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Record a Type step.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "unmatched".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Column 99 is out of range (only 1 column exists).
        core.process(Command::HandleNewField { column: 99 });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(e, TrainingEvent::Error(_))),
            "expected an Error event",
        );
        // Bindings should be unmodified.
        assert_eq!(core.bound_columns()[0], None);
    }

    // -- HandleEmptyCell tests --

    #[test]
    fn handle_empty_cell_stores_rule() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::HandleEmptyCell {
            column: 0,
            rule: EmptyCellRule::Skip,
        });
        core.process(Command::HandleEmptyCell {
            column: 1,
            rule: EmptyCellRule::Default {
                value: "N/A".to_owned(),
            },
        });

        assert_eq!(core.empty_cell_rules().get(&0), Some(&EmptyCellRule::Skip));
        assert_eq!(
            core.empty_cell_rules().get(&1),
            Some(&EmptyCellRule::Default {
                value: "N/A".to_owned()
            }),
        );
    }

    // -- build_workflow tests --

    // -- Column-order training tests --

    #[test]
    fn column_order_skips_non_next_column_match() {
        // Data: name=Alice, age=30. If user types "30" first,
        // it should NOT bind to column 1 — column 0 is the next unbound.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        // Should NOT emit a ColumnBound event — "30" matches column 1
        // but column 0 is the next unbound column.
        assert!(
            !has_event(&events, |e| matches!(e, TrainingEvent::ColumnBound { .. })),
            "should not bind a non-next column",
        );

        // Step should be a literal.
        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(
                    *source,
                    ValueSource::Literal {
                        value: "30".to_owned()
                    }
                );
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn column_order_binds_leftmost_unbound() {
        // Bind column 0 first, then column 1 becomes the next unbound.
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
        assert_eq!(core.bound_columns()[0], Some(0));

        // Now "30" should bind to column 1 (the new leftmost unbound).
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound {
                column: 1,
                step_index: 1
            }
        )));
        assert_eq!(core.bound_columns()[1], Some(1));
    }

    #[test]
    fn column_order_nudge_rebinds_after_out_of_order_fill() {
        // User fills age (col 1) first — literal. Then fills name (col 0) — binds.
        // Then nudges age field — now col 1 is leftmost unbound, so it binds.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type "30" into age field — col 0 is next unbound, no match → literal.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert!(core.bound_columns()[1].is_none());

        // Type "Alice" into name field — col 0 is next unbound, matches → binds.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert_eq!(core.bound_columns()[0], Some(1));

        // Nudge age field (delete and retype) — col 1 is now leftmost unbound.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(
                e,
                TrainingEvent::ColumnBound {
                    column: 1,
                    step_index: 0
                }
            )),
            "nudge should bind col 1 to the existing age step",
        );
        assert!(core.is_row_complete());
    }

    // -- build_workflow tests --

    #[test]
    fn build_workflow_captures_state() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        assert_eq!(wf.column_count, 2);
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.column_bindings[0], Some(0));
        assert_eq!(wf.column_bindings[1], None);
    }
}
