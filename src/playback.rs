// Playback engine.
//
// Executes a recorded workflow against data rows, driving a browser via CDP.

mod engine;
mod execute;
mod resolve;

pub use engine::{PlaybackEngine, PlaybackResult};

use crate::cdp::CdpError;

/// Errors that can occur during playback.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::module_name_repetitions)]
pub enum PlaybackError {
    /// Element matching the selector was not found within the timeout.
    #[error("element not found: {0}")]
    ElementNotFound(String),

    /// A CDP command failed.
    #[error("CDP error: {0}")]
    Cdp(#[from] CdpError),

    /// Navigation did not complete within the timeout.
    #[error("navigation timeout")]
    NavigationTimeout,

    /// User explicitly stopped playback.
    #[error("playback stopped by user")]
    Stopped,

    /// Other playback error.
    #[error("playback error: {0}")]
    Other(String),
}

/// Control signals sent from the TUI or CLI into the playback engine.
#[derive(Debug, Clone)]
#[allow(clippy::module_name_repetitions)]
pub enum PlaybackControl {
    /// Proceed past a confirmation gate.
    Proceed,
    /// Change playback speed.
    SetSpeed(crate::workflow::PlaybackSpeed),
    /// Pause playback (drops to Manual speed).
    Pause,
    /// Respond to a step error.
    ErrorResponse(ErrorAction),
}

/// What to do when a step fails during playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    /// Skip the current row and continue with the next.
    SkipRow,
    /// Retry the current row from its first step.
    RetryRow,
    /// Stop playback entirely.
    Stop,
}

/// Progress events emitted by the playback engine to the TUI or CLI.
#[derive(Debug, Clone)]
#[allow(clippy::module_name_repetitions)]
pub enum PlaybackEvent {
    /// Starting a new row.
    RowStarted {
        /// Zero-based index of the row being started.
        row_index: usize,
    },
    /// A step is about to execute.
    StepStarted {
        /// Zero-based row index.
        row_index: usize,
        /// Zero-based step index within the workflow.
        step_index: usize,
    },
    /// A step completed successfully (or was skipped by an empty-cell rule).
    StepCompleted {
        /// Zero-based row index.
        row_index: usize,
        /// Zero-based step index within the workflow.
        step_index: usize,
    },
    /// A row completed successfully.
    RowCompleted {
        /// Zero-based index of the completed row.
        row_index: usize,
    },
    /// Playback speed changed.
    SpeedChanged(crate::workflow::PlaybackSpeed),
    /// Engine is paused at a confirmation gate, waiting for [`PlaybackControl::Proceed`].
    WaitingForConfirmation,
    /// A step failed.
    StepFailed {
        /// Zero-based row index.
        row_index: usize,
        /// Zero-based step index within the workflow.
        step_index: usize,
        /// Human-readable error description.
        error: String,
    },
    /// All rows processed; playback is finished.
    Finished {
        /// Number of rows that completed all steps successfully.
        rows_completed: usize,
        /// Number of rows that were skipped due to errors or empty-cell rules.
        rows_skipped: usize,
    },
}
