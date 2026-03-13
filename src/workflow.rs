// Workflow serialization and types.
//
// These types are shared between the training recorder and future playback.
// They serialize to JSON workflow files.

use std::collections::BTreeMap;
use std::path::Path;
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::data::DataSourceConfig;

/// How an element is found in the DOM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Resolution {
    /// Match by HTML `id` attribute.
    Id { id: String },
    /// Match by CSS selector.
    Css { selector: String },
    /// Match by `XPath` expression.
    XPath { path: String },
    /// Match by visible text content.
    TextContent { text: String },
}

/// Multi-strategy selector for a DOM element.
///
/// Contains one or more resolution strategies. During playback, strategies
/// are tried in order until one succeeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Selector {
    /// Resolution strategies, tried in order.
    pub strategies: Vec<Resolution>,
    /// The HTML tag name of the target element (e.g. "INPUT", "BUTTON").
    pub tag: String,
}

/// Where a type step gets its value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ValueSource {
    /// Value comes from a data column.
    Column { index: usize },
    /// Static literal value, same for every row.
    Literal { value: String },
}

/// A single recorded action in the workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Step {
    /// Click on an element.
    Click { selector: Selector },
    /// Type a value into an element.
    Type {
        selector: Selector,
        source: ValueSource,
    },
    /// Wait for a browser navigation to complete.
    ///
    /// This step does not trigger navigation itself; it waits for a navigation
    /// that was triggered by the previous step (typically a form submit or
    /// click). The engine pre-subscribes to CDP events before executing the
    /// previous step to avoid a race condition.
    WaitForNavigation,
}

/// What to do when a cell is blank for a bound column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum EmptyCellRule {
    /// Skip the step entirely.
    Skip,
    /// Clear the field.
    Clear,
    /// Use a default value.
    Default { value: String },
}

/// Playback speed levels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackSpeed {
    /// User manually triggers each step.
    #[default]
    Manual,
    /// Auto-advance within a cell, pause between cells.
    Cell,
    /// Auto-advance within a row, pause between rows.
    Row,
    /// Fully automatic playback.
    Auto,
}

/// Current workflow format version.
const FORMAT_VERSION: u32 = 1;

/// A complete workflow: the artifact that bridges sessions.
///
/// Contains everything needed to replay a recorded data entry sequence.
/// Serialized to JSON for storage, sharing, and version control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    /// Format version for forward compatibility.
    pub version: u32,
    /// Number of data columns this workflow expects.
    pub column_count: usize,
    /// The recorded sequence of actions.
    pub steps: Vec<Step>,
    /// Per-column binding: `column_bindings[col]` is `Some(step_index)` when
    /// that column is bound to a [`Step::Type`] step. Always has exactly
    /// `column_count` entries; `None` means the column is not yet bound.
    pub column_bindings: Vec<Option<usize>>,
    /// Per-column rules for handling empty cells.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub empty_cell_rules: BTreeMap<usize, EmptyCellRule>,
    /// How the data was originally loaded (so playback can reload it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source: Option<DataSourceConfig>,
}

/// Errors from workflow serialization and validation.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported workflow version: {found} (max supported: {max})")]
    UnsupportedVersion { found: u32, max: u32 },

    #[error("column binding references step {step_index}, but only {step_count} steps exist")]
    InvalidBinding {
        step_index: usize,
        step_count: usize,
    },

    #[error("empty cell rule references column {column}, but only {column_count} columns exist")]
    InvalidEmptyCellRule { column: usize, column_count: usize },

    #[error("column_bindings has {found} entries but column_count is {expected}")]
    ColumnBindingLengthMismatch { expected: usize, found: usize },

    #[error("column {column} is bound to step {step_index}, which is not a Type step")]
    BindingNotTypeStep { column: usize, step_index: usize },
}

impl Workflow {
    /// Create a new workflow with the current format version.
    #[must_use]
    pub fn new(
        column_count: usize,
        steps: Vec<Step>,
        column_bindings: Vec<Option<usize>>,
        empty_cell_rules: BTreeMap<usize, EmptyCellRule>,
        data_source: Option<DataSourceConfig>,
    ) -> Self {
        Self {
            version: FORMAT_VERSION,
            column_count,
            steps,
            column_bindings,
            empty_cell_rules,
            data_source,
        }
    }

    /// Serialize to pretty-printed JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, WorkflowError> {
        serde_json::to_string_pretty(self).map_err(WorkflowError::from)
    }

    /// Deserialize from JSON, then validate.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is invalid, the version is unsupported,
    /// or the internal references are inconsistent.
    pub fn from_json(json: &str) -> Result<Self, WorkflowError> {
        let workflow: Self = serde_json::from_str(json)?;
        workflow.validate()?;
        Ok(workflow)
    }

    /// Save to a file.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save(&self, path: &Path) -> Result<(), WorkflowError> {
        let json = self.to_json()?;
        fs::write(path, json)?;
        Ok(())
    }

    /// Load from a file, then validate.
    ///
    /// # Errors
    ///
    /// Returns an error if reading, parsing, or validation fails.
    pub fn load(path: &Path) -> Result<Self, WorkflowError> {
        let json = fs::read_to_string(path)?;
        Self::from_json(&json)
    }

    /// Validate internal consistency.
    ///
    /// # Errors
    ///
    /// Returns an error if the version is unsupported or references are invalid.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version > FORMAT_VERSION {
            return Err(WorkflowError::UnsupportedVersion {
                found: self.version,
                max: FORMAT_VERSION,
            });
        }

        if self.column_bindings.len() != self.column_count {
            return Err(WorkflowError::ColumnBindingLengthMismatch {
                expected: self.column_count,
                found: self.column_bindings.len(),
            });
        }

        for (col, binding) in self.column_bindings.iter().enumerate() {
            if let Some(step_index) = binding {
                if *step_index >= self.steps.len() {
                    return Err(WorkflowError::InvalidBinding {
                        step_index: *step_index,
                        step_count: self.steps.len(),
                    });
                }
                if !matches!(self.steps[*step_index], Step::Type { .. }) {
                    return Err(WorkflowError::BindingNotTypeStep {
                        column: col,
                        step_index: *step_index,
                    });
                }
            }
        }

        for &column in self.empty_cell_rules.keys() {
            if column >= self.column_count {
                return Err(WorkflowError::InvalidEmptyCellRule {
                    column,
                    column_count: self.column_count,
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_serialization() {
        let r = Resolution::Id {
            id: "foo".to_owned(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""type":"id""#));
        let back: Resolution = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn step_click_serialization() {
        let step = Step::Click {
            selector: Selector {
                strategies: vec![Resolution::Id {
                    id: "btn".to_owned(),
                }],
                tag: "BUTTON".to_owned(),
            },
        };
        let json = serde_json::to_string_pretty(&step).unwrap();
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn step_type_column_serialization() {
        let step = Step::Type {
            selector: Selector {
                strategies: vec![],
                tag: "INPUT".to_owned(),
            },
            source: ValueSource::Column { index: 2 },
        };
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains(r#""action":"type""#));
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn step_wait_for_navigation_serialization() {
        let step = Step::WaitForNavigation;
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains(r#""action":"waitForNavigation""#));
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn empty_cell_rule_serialization() {
        let rules = vec![
            EmptyCellRule::Skip,
            EmptyCellRule::Clear,
            EmptyCellRule::Default {
                value: "N/A".to_owned(),
            },
        ];
        for rule in rules {
            let json = serde_json::to_string(&rule).unwrap();
            let back: EmptyCellRule = serde_json::from_str(&json).unwrap();
            assert_eq!(back, rule);
        }
    }

    #[test]
    fn playback_speed_default() {
        assert_eq!(PlaybackSpeed::default(), PlaybackSpeed::Manual);
    }

    #[test]
    fn playback_speed_serialization() {
        let speed = PlaybackSpeed::Auto;
        let json = serde_json::to_string(&speed).unwrap();
        assert_eq!(json, r#""auto""#);
        let back: PlaybackSpeed = serde_json::from_str(&json).unwrap();
        assert_eq!(back, speed);
    }

    fn sample_workflow() -> Workflow {
        let mut rules = BTreeMap::new();
        rules.insert(1, EmptyCellRule::Skip);

        Workflow::new(
            3,
            vec![
                Step::WaitForNavigation,
                Step::Click {
                    selector: Selector {
                        strategies: vec![Resolution::Id {
                            id: "name-field".to_owned(),
                        }],
                        tag: "INPUT".to_owned(),
                    },
                },
                Step::Type {
                    selector: Selector {
                        strategies: vec![
                            Resolution::Id {
                                id: "name-field".to_owned(),
                            },
                            Resolution::Css {
                                selector: "#name-field".to_owned(),
                            },
                        ],
                        tag: "INPUT".to_owned(),
                    },
                    source: ValueSource::Column { index: 0 },
                },
                Step::Type {
                    selector: Selector {
                        strategies: vec![Resolution::Css {
                            selector: "#age-field".to_owned(),
                        }],
                        tag: "INPUT".to_owned(),
                    },
                    source: ValueSource::Column { index: 1 },
                },
                Step::Click {
                    selector: Selector {
                        strategies: vec![Resolution::TextContent {
                            text: "Submit".to_owned(),
                        }],
                        tag: "BUTTON".to_owned(),
                    },
                },
            ],
            vec![Some(2), Some(3), None],
            rules,
            Some(DataSourceConfig::file(
                "data.tsv",
                crate::data::Delimiter::Tab,
                true,
            )),
        )
    }

    #[test]
    fn workflow_round_trip() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        let back = Workflow::from_json(&json).unwrap();
        assert_eq!(back, workflow);
    }

    #[test]
    fn workflow_has_version() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        assert!(json.contains(r#""version": 1"#));
    }

    #[test]
    fn workflow_omits_empty_optional_fields() {
        let workflow = Workflow::new(1, vec![], vec![None], BTreeMap::new(), None);
        let json = workflow.to_json().unwrap();
        assert!(!json.contains("emptyCellRules"));
        assert!(!json.contains("dataSource"));
    }

    #[test]
    fn workflow_unsupported_version() {
        let mut workflow = sample_workflow();
        workflow.version = 999;
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("unsupported workflow version"));
    }

    #[test]
    fn workflow_invalid_binding() {
        let workflow = Workflow {
            version: 1,
            column_count: 1,
            steps: vec![],
            column_bindings: vec![Some(5)],
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("column binding references step 5"));
    }

    #[test]
    fn workflow_invalid_empty_cell_rule() {
        let mut rules = BTreeMap::new();
        rules.insert(10, EmptyCellRule::Clear);
        let workflow = Workflow {
            version: 1,
            column_count: 2,
            steps: vec![],
            column_bindings: vec![None, None],
            empty_cell_rules: rules,
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string()
                .contains("empty cell rule references column 10")
        );
    }

    #[test]
    fn workflow_file_round_trip() {
        let workflow = sample_workflow();
        let dir = std::env::temp_dir().join("rote-test-workflow");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.json");

        workflow.save(&path).unwrap();
        let loaded = Workflow::load(&path).unwrap();
        assert_eq!(loaded, workflow);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workflow_json_is_human_readable() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        // Pretty-printed: should have newlines and indentation.
        assert!(json.contains('\n'));
        assert!(json.contains("  "));
        // Key fields should be camelCase.
        assert!(json.contains("columnCount"));
        assert!(json.contains("columnBindings"));
        assert!(json.contains("emptyCellRules"));
        assert!(json.contains("dataSource"));
    }

    #[test]
    fn workflow_column_binding_length_mismatch() {
        let workflow = Workflow {
            version: 1,
            column_count: 3,
            steps: vec![],
            column_bindings: vec![None, None], // length 2, but column_count is 3
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string().contains("column_bindings has 2 entries"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn workflow_binding_not_type_step() {
        let workflow = Workflow {
            version: 1,
            column_count: 1,
            steps: vec![Step::Click {
                selector: Selector {
                    strategies: vec![],
                    tag: "BUTTON".to_owned(),
                },
            }],
            column_bindings: vec![Some(0)], // bound to a Click step, not Type
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string().contains("not a Type step"),
            "unexpected error: {err}",
        );
    }
}
