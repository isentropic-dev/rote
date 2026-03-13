// Workflow serialization and types.
//
// These types are shared between the training recorder and future playback.
// They serialize to JSON workflow files.

use serde::{Deserialize, Serialize};

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
    /// Navigate to a URL.
    Navigate { url: String },
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
    fn step_navigate_serialization() {
        let step = Step::Navigate {
            url: "https://example.com".to_owned(),
        };
        let json = serde_json::to_string(&step).unwrap();
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
}
