// Training core state machine.
//
// Processes browser events and user commands during workflow recording.
// Maintains session state and emits events for the TUI to render.

mod core;
pub mod recorder;

pub use self::core::TrainingCore;

use crate::workflow::{EmptyCellRule, PlaybackSpeed, Resolution, Selector, Step};

/// Raw selector data from the recorder JS.
///
/// Contains all available identification strategies for a DOM element.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectorInfo {
    pub id: Option<String>,
    pub css: Option<String>,
    pub xpath: Option<String>,
    pub text_content: Option<String>,
}

impl SelectorInfo {
    /// Build a multi-strategy `Selector` from the raw info.
    fn into_selector(self, tag: String) -> Selector {
        let mut strategies = Vec::new();
        if let Some(id) = self.id
            && !id.is_empty()
        {
            strategies.push(Resolution::Id { id });
        }
        if let Some(selector) = self.css
            && !selector.is_empty()
        {
            strategies.push(Resolution::Css { selector });
        }
        if let Some(path) = self.xpath
            && !path.is_empty()
        {
            strategies.push(Resolution::XPath { path });
        }
        if let Some(text) = self.text_content
            && !text.is_empty()
        {
            strategies.push(Resolution::TextContent { text });
        }
        Selector { strategies, tag }
    }

    /// Check if two `SelectorInfo`s refer to the same DOM element.
    ///
    /// Matches if they share a non-empty id, CSS selector, or `XPath` expression.
    fn same_element(&self, other: &Self) -> bool {
        if let (Some(a), Some(b)) = (&self.id, &other.id)
            && !a.is_empty()
            && !b.is_empty()
            && a == b
        {
            return true;
        }
        if let (Some(a), Some(b)) = (&self.css, &other.css)
            && !a.is_empty()
            && !b.is_empty()
            && a == b
        {
            return true;
        }
        if let (Some(a), Some(b)) = (&self.xpath, &other.xpath)
            && !a.is_empty()
            && !b.is_empty()
            && a == b
        {
            return true;
        }
        false
    }
}

/// Commands processed by the training state machine.
#[derive(Debug, Clone)]
pub enum Command {
    // Browser events from the recorder JS.
    /// User clicked an element.
    BrowserClick {
        selector_info: SelectorInfo,
        tag: String,
    },
    /// User typed into an element.
    BrowserInput {
        selector_info: SelectorInfo,
        tag: String,
        value: String,
    },
    /// Browser navigated to a new URL.
    BrowserNavigation { url: String },

    // User commands from the TUI.
    /// Advance to the next data row.
    AdvanceRow,
    /// Change the playback speed.
    #[allow(dead_code)] // Wired in a future milestone.
    SetSpeed(PlaybackSpeed),
    /// Set the rule for handling empty cells in a column.
    #[allow(dead_code)] // Wired in a future milestone.
    HandleEmptyCell { column: usize, rule: EmptyCellRule },
    /// Assign an unbound column to the most recent input step.
    #[allow(dead_code)] // Wired in a future milestone.
    HandleNewField { column: usize },
}

/// Events emitted by the training core for the TUI to render.
#[derive(Debug, Clone)]
pub enum TrainingEvent {
    /// A new step was recorded.
    StepRecorded { index: usize, step: Step },
    /// An existing step was updated (e.g. from incremental typing).
    StepUpdated { index: usize, step: Step },
    /// A data column was bound to a step.
    ColumnBound { column: usize, step_index: usize },
    /// All required columns in the current row are bound.
    RowComplete { row_index: usize },
    /// Playback speed was changed.
    SpeedChanged(#[allow(dead_code)] PlaybackSpeed),
    /// A bound column has an empty cell in the current row.
    EmptyCellEncountered { column: usize, row_index: usize },
    /// An unbound column has a non-empty value in the current row.
    NewFieldEncountered { column: usize, value: String },
    /// An error occurred.
    Error(String),
}
