// Element resolution: find a DOM element using a [`Selector`]'s strategies.

use std::time::Duration;

use crate::cdp::Browser;
use crate::workflow::{Resolution, Selector};

use super::PlaybackError;

/// How long to keep retrying before giving up.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait between retry attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Generate a self-invoking JS expression for one resolution strategy.
///
/// On success the expression evaluates to `true` and stores the element in
/// `window.__roteTarget`. On failure it evaluates to `false`.
///
/// `tag` is the expected HTML tag name (e.g. `"INPUT"`, `"BUTTON"`) and is
/// used only for the [`Resolution::TextContent`] strategy.
#[must_use]
pub(crate) fn resolution_js(resolution: &Resolution, tag: &str) -> String {
    match resolution {
        Resolution::Id { id } => format!(
            "(function(){{var e=document.getElementById({id});if(e){{window.__roteTarget=e;return true;}}return false;}})()",
            id = serde_json::to_string(id).unwrap_or_default(),
        ),

        Resolution::Css { selector } => format!(
            "(function(){{var e=document.querySelector({sel});if(e){{window.__roteTarget=e;return true;}}return false;}})()",
            sel = serde_json::to_string(selector).unwrap_or_default(),
        ),

        Resolution::XPath { path } => format!(
            "(function(){{var e=document.evaluate({path},document,null,XPathResult.FIRST_ORDERED_NODE_TYPE,null).singleNodeValue;if(e){{window.__roteTarget=e;return true;}}return false;}})()",
            path = serde_json::to_string(path).unwrap_or_default(),
        ),

        Resolution::TextContent { text } => format!(
            "(function(){{var es=document.getElementsByTagName({tag});for(var i=0;i<es.length;i++){{if(es[i].textContent.trim()==={text}){{window.__roteTarget=es[i];return true;}}}}return false;}})()",
            tag  = serde_json::to_string(tag).unwrap_or_default(),
            text = serde_json::to_string(text).unwrap_or_default(),
        ),
    }
}

/// Try each resolution strategy in `selector` until one locates the element.
///
/// On success, `window.__roteTarget` holds the element in the browser page.
/// Retries every [`POLL_INTERVAL`] until [`DEFAULT_TIMEOUT`] is exceeded.
///
/// # Errors
///
/// - [`PlaybackError::ElementNotFound`] — no strategy succeeded within the timeout.
/// - [`PlaybackError::Cdp`] — a CDP command failed.
pub(crate) async fn resolve_element(
    browser: &Browser,
    selector: &Selector,
) -> Result<(), PlaybackError> {
    let deadline = tokio::time::Instant::now() + DEFAULT_TIMEOUT;

    loop {
        for resolution in &selector.strategies {
            let js = resolution_js(resolution, &selector.tag);
            let response = browser.evaluate(&js).await?;

            // Runtime.evaluate returns {"result": {"type": ..., "value": ...}}.
            let found = response
                .get("result")
                .and_then(|r| r.get("value"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);

            if found {
                return Ok(());
            }
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    Err(PlaybackError::ElementNotFound(format!(
        "selector with {} strateg{} (tag: {})",
        selector.strategies.len(),
        if selector.strategies.len() == 1 { "y" } else { "ies" },
        selector.tag,
    )))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::Resolution;

    /// Extract the JS body from the IIFE so we can inspect it more easily.
    fn has_snippet(js: &str, snippet: &str) -> bool {
        js.contains(snippet)
    }

    #[test]
    fn id_resolution_uses_get_element_by_id() {
        let r = Resolution::Id {
            id: "name-field".to_owned(),
        };
        let js = resolution_js(&r, "INPUT");
        assert!(has_snippet(&js, "getElementById"));
        assert!(has_snippet(&js, r#""name-field""#));
        assert!(has_snippet(&js, "__roteTarget"));
    }

    #[test]
    fn css_resolution_uses_query_selector() {
        let r = Resolution::Css {
            selector: "#age-field".to_owned(),
        };
        let js = resolution_js(&r, "INPUT");
        assert!(has_snippet(&js, "querySelector"));
        // serde_json serialises the string as "\"#age-field\""
        assert!(has_snippet(&js, "age-field"));
    }

    #[test]
    fn xpath_resolution_uses_evaluate() {
        let r = Resolution::XPath {
            path: "//input[@id='x']".to_owned(),
        };
        let js = resolution_js(&r, "INPUT");
        assert!(has_snippet(&js, "document.evaluate"));
        assert!(has_snippet(&js, "FIRST_ORDERED_NODE_TYPE"));
        assert!(has_snippet(&js, r#""//input[@id='x']""#));
    }

    #[test]
    fn text_content_resolution_uses_get_elements_by_tag_name() {
        let r = Resolution::TextContent {
            text: "Submit".to_owned(),
        };
        let js = resolution_js(&r, "BUTTON");
        assert!(has_snippet(&js, "getElementsByTagName"));
        assert!(has_snippet(&js, r#""BUTTON""#));
        assert!(has_snippet(&js, r#""Submit""#));
        assert!(has_snippet(&js, "textContent.trim()"));
    }

    #[test]
    fn js_returns_true_and_false_branches() {
        for resolution in &[
            Resolution::Id {
                id: "x".to_owned(),
            },
            Resolution::Css {
                selector: ".x".to_owned(),
            },
            Resolution::XPath {
                path: "//x".to_owned(),
            },
            Resolution::TextContent {
                text: "X".to_owned(),
            },
        ] {
            let js = resolution_js(resolution, "INPUT");
            assert!(has_snippet(&js, "return true;"), "missing true branch: {js}");
            assert!(has_snippet(&js, "return false;"), "missing false branch: {js}");
        }
    }

    #[test]
    fn special_characters_are_escaped() {
        let r = Resolution::Id {
            id: r#"it's a "test""#.to_owned(),
        };
        let js = resolution_js(&r, "INPUT");
        // serde_json escapes the string — no raw quotes or apostrophes in the value.
        assert!(!js.contains(r#"getElementById(it's"#));
    }
}
